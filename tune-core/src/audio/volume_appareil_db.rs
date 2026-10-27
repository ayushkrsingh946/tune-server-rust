//! #6918 — le volume en dB d'une sortie qui règle son volume en décibels.
//!
//! Fil forum 2336 (Rémi, Devialet Expert D400) : le curseur doit parler en
//! dB, sur une course réglable. Arbitrage de Bertrand du 27/10 : course
//! réglable dans `[−80 ; −10]` dB, le plafond de −10 dB de la sortie
//! Devialet AIR ne bouge pas ; « confort » et « sécurité » sont les limites
//! de volume de zone de #6541.
//!
//! Ce module ne change NI le sens de `volume` (la fraction 0..1 que
//! l'orchestrateur, les limites #6541 et la garde de hausse #6721 lisent),
//! NI celui de `volume_db` (sa lecture en atténuation de la pleine échelle,
//! #1274). Il ajoute, pour la seule sortie dont la course en dB est connue
//! côté serveur, la lecture en dB de l'APPAREIL :
//!
//! - `device_volume_db` : les dB que l'appareil reçoit pour `volume` ;
//! - `device_volume_range` : la course (`min_db`, `max_db`, `step_db`,
//!   `floor_db`, `ceiling_db`) ;
//! - `volume_limit_safety_device_db` / `volume_limit_comfort_device_db` :
//!   les limites #6541 lues sur cette course.
//!
//! Les clés sont TOUJOURS présentes, `null` pour une sortie sans course en
//! dB : leur présence dit au client que ce serveur connaît le réglage.
//!
//! Dans l'autre sens, [`demande`] ramène des dB d'appareil à la fraction
//! 0..1, qui suit ensuite le chemin ordinaire de toute consigne (limites,
//! garde de hausse, trim). Une demande hors de la course est refusée, pas
//! ramenée : la sortie, elle, ne laisse de toute façon rien partir au-dessus
//! de son plafond.

use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::outputs::devialet_air::reglages::BornesDb;

/// La course en dB de la sortie `output_type`, s'il en a une côté serveur.
///
/// Seule la sortie Devialet AIR en a une : elle déclare un volume décibel
/// (`with_decibel_volume`) et c'est Tune qui fixe la correspondance curseur
/// ↔ dB. AirPlay déclare aussi un pas en dB, mais sa correspondance n'est pas
/// une course réglée ici : elle n'est pas publiée.
pub fn course_de_la_sortie(db: &Arc<dyn DbBackend>, output_type: Option<&str>) -> Option<BornesDb> {
    match output_type {
        Some(crate::outputs::devialet_air::TYPE_DE_SORTIE) => Some(BornesDb::lues(db)),
        _ => None,
    }
}

/// Pose `device_volume_db` et `device_volume_range` à côté de `volume`.
pub fn inserer(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    course: Option<BornesDb>,
    volume: f64,
) {
    obj.insert(
        "device_volume_db".into(),
        serde_json::json!(course.map(|c| c.volume_vers_db(volume))),
    );
    obj.insert(
        "device_volume_range".into(),
        course.map_or(serde_json::Value::Null, |c| c.json()),
    );
}

/// Pose les limites #6541 lues en dB d'appareil.
pub fn inserer_limites(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    course: Option<BornesDb>,
    limites: &super::volume_limites::LimitesDeVolume,
) {
    let en_db = |l: Option<f64>| course.and_then(|c| l.map(|v| c.volume_vers_db(v)));
    obj.insert(
        "volume_limit_safety_device_db".into(),
        serde_json::json!(en_db(limites.securite)),
    );
    obj.insert(
        "volume_limit_comfort_device_db".into(),
        serde_json::json!(en_db(limites.confort)),
    );
}

/// Tout ce que publie une charge utile de zone : la lecture en dB
/// d'appareil de `volume` et des limites #6541 enregistrées de `zone_id`.
pub fn publier(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    output_type: Option<&str>,
    volume: f64,
) {
    let course = course_de_la_sortie(db, output_type);
    inserer(obj, course, volume);
    inserer_limites(obj, course, &super::volume_limites::lire(db, zone_id));
}

/// dB d'appareil demandés → fraction 0..1. Refus si la sortie n'a pas de
/// course en dB, ou si la demande en sort.
pub fn demande(course: Option<BornesDb>, device_db: f64) -> Result<f64, String> {
    match course {
        None => {
            Err("cette sortie ne règle pas son volume sur une course en dB connue de Tune".into())
        }
        Some(c) => c.demande_vers_volume(device_db),
    }
}

/// La consigne d'une requête qui porte `device_volume_db` : ce champ exclut
/// `volume` et `volume_db`, comme ces deux-là s'excluent (#1274) — accepter
/// plusieurs champs obligerait à inventer un gagnant.
pub fn demande_de_la_requete(
    course: Option<BornesDb>,
    lineaire: Option<f64>,
    volume_db: Option<f64>,
    device_db: f64,
) -> Result<f64, String> {
    if lineaire.is_some() || volume_db.is_some() {
        return Err("device_volume_db exclut volume et volume_db — n'en envoyer qu'un".into());
    }
    demande(course, device_db)
}

/// #6918 — la position sur la course `apres` qui garde le niveau en dB de la
/// position `volume` sur la course `avant`.
#[must_use]
pub fn reexprimer(avant: BornesDb, apres: BornesDb, volume: f64) -> f64 {
    apres.db_vers_volume(avant.db_exacts(volume))
}

/// #6918 — une course changée ne déplace AUCUN niveau enregistré : sans
/// cela, le même pourcentage enverrait d'autres dB, et une course dont le
/// bas remonte ferait jouer le geste suivant jusqu'à des dizaines de dB plus
/// fort. Pour chaque zone Devialet AIR, le volume enregistré
/// (`zones.volume`) et les limites #6541 sont réécrits sur la nouvelle
/// course AU MÊME NIVEAU en dB.
///
/// Refus, sans rien écrire, si une limite tomberait sous le bas de la
/// nouvelle course : la garder exigerait de la REMONTER. Rend les zones
/// réécrites, pour que l'appelant recale leur état de lecture avec
/// [`reexprimer`]. Rien n'est envoyé à l'appareil.
pub fn reexprimer_les_zones(
    db: &Arc<dyn DbBackend>,
    avant: BornesDb,
    apres: BornesDb,
) -> Result<Vec<i64>, String> {
    use super::volume_limites::{LIMITE_MINIMALE, cle_confort, cle_securite, lire};
    if avant == apres {
        return Ok(Vec::new());
    }
    let repo = crate::db::zone_repo::ZoneRepo::with_backend(db.clone());
    let zones: Vec<_> = repo
        .list()?
        .into_iter()
        .filter(|z| z.output_type.as_deref() == Some(crate::outputs::devialet_air::TYPE_DE_SORTIE))
        .filter_map(|z| z.id.map(|id| (id, z.volume)))
        .collect();
    // Tout est vérifié avant la première écriture.
    let mut a_ecrire = Vec::new();
    for (id, volume) in &zones {
        let limites = lire(db, *id);
        let mut seuils = Vec::new();
        for (cle, seuil) in [
            (cle_securite(*id), limites.securite),
            (cle_confort(*id), limites.confort),
        ] {
            if let Some(f) = seuil {
                let db_seuil = avant.db_exacts(f);
                if db_seuil < apres.min_db() + LIMITE_MINIMALE * (apres.max_db() - apres.min_db()) {
                    return Err(format!(
                        "zone {id} : une limite de volume à {db_seuil:.1} dB tomberait sous le bas de la course ({} dB)",
                        apres.min_db()
                    ));
                }
                seuils.push((cle, reexprimer(avant, apres, f)));
            }
        }
        a_ecrire.push((
            *id,
            reexprimer(avant, apres, volume / 100.0) * 100.0,
            seuils,
        ));
    }
    let settings = crate::db::settings_repo::SettingsRepo::with_backend(db.clone());
    for (id, volume, seuils) in &a_ecrire {
        repo.update_volume(*id, *volume)?;
        for (cle, f) in seuils {
            settings.set(cle, &format!("{f}"))?;
        }
    }
    Ok(zones.into_iter().map(|(id, _)| id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::volume_limites::LimitesDeVolume;
    use crate::outputs::devialet_air::reglages::PLAFOND_DB;

    #[test]
    fn la_sortie_devialet_publie_ses_db_et_les_autres_null_6918() {
        let course = BornesDb::nouvelles(-70.0, -20.0).ok();
        let mut obj = serde_json::Map::new();
        inserer(&mut obj, course, 0.5);
        assert_eq!(obj["device_volume_db"], serde_json::json!(-45.0));
        assert_eq!(obj["device_volume_range"]["min_db"], -70.0);
        assert_eq!(obj["device_volume_range"]["max_db"], -20.0);
        assert_eq!(obj["device_volume_range"]["step_db"], 0.5);
        assert_eq!(obj["device_volume_range"]["ceiling_db"], PLAFOND_DB);

        let limites = LimitesDeVolume {
            securite: Some(1.0),
            confort: Some(0.5),
        };
        inserer_limites(&mut obj, course, &limites);
        assert_eq!(obj["volume_limit_safety_device_db"], -20.0);
        assert_eq!(obj["volume_limit_comfort_device_db"], -45.0);

        let mut autre = serde_json::Map::new();
        inserer(&mut autre, None, 0.5);
        inserer_limites(&mut autre, None, &limites);
        for cle in [
            "device_volume_db",
            "device_volume_range",
            "volume_limit_safety_device_db",
            "volume_limit_comfort_device_db",
        ] {
            assert_eq!(autre[cle], serde_json::Value::Null, "{cle}");
        }
    }

    #[test]
    fn une_demande_en_db_d_appareil_suit_la_course_ou_est_refusee_6918() {
        let course = BornesDb::nouvelles(-70.0, -20.0).ok();
        assert_eq!(demande(course, -45.0), Ok(0.5));
        assert_eq!(demande(course, -20.0), Ok(1.0));
        assert!(
            demande(course, -10.0).is_err(),
            "au-dessus de la borne haute"
        );
        assert!(demande(course, 0.0).is_err(), "au-dessus du plafond");
        assert!(demande(None, -45.0).is_err(), "sortie sans course");
        assert!(demande_de_la_requete(course, Some(0.5), None, -45.0).is_err());
        assert!(demande_de_la_requete(course, None, Some(-6.0), -45.0).is_err());
        assert_eq!(demande_de_la_requete(course, None, None, -45.0), Ok(0.5));
    }

    fn base_avec_une_zone_air(volume: f64) -> (Arc<dyn DbBackend>, i64) {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
        let repo = crate::db::zone_repo::ZoneRepo::with_backend(db.clone());
        let id = repo
            .create(
                "Expert",
                Some(crate::outputs::devialet_air::TYPE_DE_SORTIE),
                Some("devialet-air:02:00:5e:00:00:01"),
            )
            .unwrap();
        repo.update_volume(id, volume).unwrap();
        (db, id)
    }

    /// Changer la course garde le NIVEAU en dB du volume et des limites
    /// enregistrés : le même pourcentage ne joue pas plus fort.
    #[test]
    fn une_course_changee_garde_les_niveaux_en_db_6918() {
        let (db, id) = base_avec_une_zone_air(50.0);
        let settings = crate::db::settings_repo::SettingsRepo::with_backend(db.clone());
        // Sécurité à −24 dB, confort à −45 dB sur la course par défaut.
        settings
            .set(&super::super::volume_limites::cle_securite(id), "0.8")
            .unwrap();
        settings
            .set(&super::super::volume_limites::cle_confort(id), "0.5")
            .unwrap();
        let apres = BornesDb::nouvelles(-60.0, -20.0).unwrap();
        let zones = reexprimer_les_zones(&db, BornesDb::PAR_DEFAUT, apres).unwrap();
        assert_eq!(zones, vec![id]);
        let zone = crate::db::zone_repo::ZoneRepo::with_backend(db.clone())
            .get(id)
            .unwrap()
            .unwrap();
        // −45 dB avant, −45 dB après.
        assert!(
            (apres.db_exacts(zone.volume / 100.0) + 45.0).abs() < 1e-9,
            "le volume enregistré ne garde pas −45 dB sur la nouvelle course : {} % = {} dB",
            zone.volume,
            apres.db_exacts(zone.volume / 100.0)
        );
        let l = super::super::volume_limites::lire(&db, id);
        assert!((apres.db_exacts(l.securite.unwrap()) + 24.0).abs() < 1e-9);
        assert!((apres.db_exacts(l.confort.unwrap()) + 45.0).abs() < 1e-9);
    }

    /// Une course dont le bas passerait au-dessus d'une limite est refusée
    /// sans rien écrire : la garder obligerait à remonter la limite.
    #[test]
    fn une_course_au_dessus_d_une_limite_est_refusee_6918() {
        let (db, id) = base_avec_une_zone_air(10.0);
        let settings = crate::db::settings_repo::SettingsRepo::with_backend(db.clone());
        // Sécurité à −66 dB.
        settings
            .set(&super::super::volume_limites::cle_securite(id), "0.2")
            .unwrap();
        let apres = BornesDb::nouvelles(-50.0, -10.0).unwrap();
        assert!(reexprimer_les_zones(&db, BornesDb::PAR_DEFAUT, apres).is_err());
        let zone = crate::db::zone_repo::ZoneRepo::with_backend(db.clone())
            .get(id)
            .unwrap()
            .unwrap();
        assert_eq!(zone.volume, 10.0, "rien n'est écrit");
        assert_eq!(
            super::super::volume_limites::lire(&db, id).securite,
            Some(0.2)
        );
    }
}
