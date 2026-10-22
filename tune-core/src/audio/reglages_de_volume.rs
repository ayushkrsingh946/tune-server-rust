//! #6774 — les réglages de volume d'une zone suivent l'APPAREIL, pas la ligne.
//!
//! `zone_{id}_*` est rangé sous le numéro de la ligne `zones`. Tout chemin qui
//! place un autre appareil sous une zone existante (choix de sortie, profil de
//! zones, rattachement automatique par nom ou par adresse), ou qui réécrit ces
//! clés sous d'autres numéros (restauration d'une sauvegarde), emmène donc le
//! trim, les limites et le verrou PURE avec la ligne. C'est ainsi qu'un trim de
//! +8,5 dB s'est retrouvé sur la zone d'un Marantz qui ne l'avait jamais reçu
//! (#6739).
//!
//! Ce module tient les deux règles communes :
//!
//! - [`reinitialiser_le_trim`] : la zone change d'appareil pour un AUTRE
//!   appareil — le trim repart de 0 dB, les limites restent (elles ne font
//!   que borner), et l'évènement est journalisé
//!   (`reglages_volume_reinitialises_changement_d_appareil`) ;
//! - [`neutraliser`] : la restauration n'a trouvé aucune correspondance sûre
//!   pour une zone — aucun réglage de volume n'y est posé, ceux qui y
//!   traînaient sont retirés.
//!
//! [`meme_appareil`] dit quand un changement d'identifiant n'est PAS un
//! changement d'appareil : identifiant inchangé, preuve d'identité locale, ou
//! même MAC normalisée des deux côtés. Rien d'autre — ni le nom, ni l'adresse.

use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Les réglages de VOLUME d'une zone, sous leur gabarit (`{id}` = numéro de
/// zone). Ce sont ceux qui peuvent faire monter le niveau d'un appareil :
///
/// - le trim de gain et sa marque « posé à la main » ;
/// - les limites de sécurité et de confort ;
/// - l'objet PURE (`enabled` + `lock_volume`) : verrouillé, il tient la
///   sortie à 100 % ;
/// - le volume mémorisé avant l'armement du volume fixe, reposé au
///   désarmement.
pub const GABARITS_DE_VOLUME: &[&str] = &[
    "zone_{id}_gain_trim_db",
    "zone_{id}_pose_gain_trim_db",
    "zone_{id}_volume_limit_safety",
    "zone_{id}_volume_limit_comfort",
    "zone_{id}_audiophile",
    "zone_{id}_volume_before_fixed",
];

/// Le trim et sa marque : ce que remet à zéro un changement d'appareil.
const GABARITS_DU_TRIM: &[&str] = &["zone_{id}_gain_trim_db", "zone_{id}_pose_gain_trim_db"];

/// Le gabarit est-il un réglage de volume ?
pub fn est_un_reglage_de_volume(gabarit: &str) -> bool {
    GABARITS_DE_VOLUME.contains(&gabarit)
}

fn cle(gabarit: &str, zone_id: i64) -> String {
    gabarit.replace("{id}", &zone_id.to_string())
}

/// Ce qui permet de dire que le nouvel identifiant désigne le MÊME appareil.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rattachement<'a> {
    /// Aucune preuve : choix de l'utilisateur, profil de zones, rattachement
    /// par nom ou par adresse. La MAC du nouvel appareil, si on la connaît,
    /// peut encore prouver qu'il s'agit du même.
    SansPreuve {
        origine: &'static str,
        mac: Option<&'a str>,
    },
    /// Même périphérique local prouvé par son identifiant d'endpoint
    /// (`outputs::identite_de_sortie`, décision `Reassocie`).
    MemePeripherique { origine: &'static str },
}

impl Rattachement<'_> {
    pub fn origine(&self) -> &'static str {
        match self {
            Rattachement::SansPreuve { origine, .. }
            | Rattachement::MemePeripherique { origine } => origine,
        }
    }
}

/// Le nouvel identifiant désigne-t-il le même appareil que l'ancien ?
///
/// Fonction pure. Vrai seulement si l'identifiant ne change pas, si
/// l'identité locale est prouvée, ou si les DEUX MAC sont connues, valides
/// et égales une fois normalisées. Une MAC absente d'un côté ne prouve rien.
pub fn meme_appareil(
    ancien_device_id: Option<&str>,
    ancienne_mac: Option<&str>,
    nouveau_device_id: &str,
    rattachement: &Rattachement<'_>,
) -> bool {
    if ancien_device_id == Some(nouveau_device_id) {
        return true;
    }
    match rattachement {
        Rattachement::MemePeripherique { .. } => true,
        Rattachement::SansPreuve { mac, .. } => {
            let normaliser = |m: Option<&str>| m.and_then(crate::discovery::mac::normalize_mac);
            match (normaliser(ancienne_mac), normaliser(*mac)) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            }
        }
    }
}

/// La zone vient de passer sur un AUTRE appareil : le trim repart de 0 dB.
///
/// Les limites et le reste restent en place. Rend le trim retiré (`None`
/// quand il n'y en avait aucun, ou qu'il valait 0). Journalise toujours
/// `reglages_volume_reinitialises_changement_d_appareil`.
pub fn reinitialiser_le_trim(
    db: &Arc<dyn DbBackend>,
    zone_id: i64,
    ancien_device_id: Option<&str>,
    nouveau_device_id: &str,
    origine: &'static str,
) -> Option<f64> {
    let settings = SettingsRepo::with_backend(db.clone());
    let trim_avant = settings
        .get(&cle(GABARITS_DU_TRIM[0], zone_id))
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|t| *t != 0.0);
    for gabarit in GABARITS_DU_TRIM {
        if let Err(e) = settings.delete(&cle(gabarit, zone_id)) {
            tracing::warn!(zone_id, cle = %cle(gabarit, zone_id), error = %e, "reglage_de_volume_non_retire");
        }
    }
    if let Some(trim) = trim_avant {
        tracing::warn!(
            zone_id,
            ancien = ?ancien_device_id,
            nouveau = nouveau_device_id,
            origine,
            trim_retire_db = trim,
            "reglages_volume_reinitialises_changement_d_appareil"
        );
    } else {
        tracing::info!(
            zone_id,
            ancien = ?ancien_device_id,
            nouveau = nouveau_device_id,
            origine,
            "reglages_volume_reinitialises_changement_d_appareil"
        );
    }
    trim_avant
}

/// Retire TOUS les réglages de volume d'une zone : elle revient au neutre
/// (trim 0 dB, aucune limite, PURE inactif). Rend les clés effectivement
/// retirées.
pub fn neutraliser(db: &Arc<dyn DbBackend>, zone_id: i64) -> Vec<String> {
    let settings = SettingsRepo::with_backend(db.clone());
    let mut retirees = Vec::new();
    for gabarit in GABARITS_DE_VOLUME {
        let k = cle(gabarit, zone_id);
        if settings.get(&k).ok().flatten().is_some() {
            match settings.delete(&k) {
                Ok(()) => retirees.push(k),
                Err(e) => {
                    tracing::warn!(zone_id, cle = %k, error = %e, "reglage_de_volume_non_retire")
                }
            }
        }
    }
    retirees
}

#[cfg(test)]
mod tests {
    use super::*;

    const SANS: Rattachement<'static> = Rattachement::SansPreuve {
        origine: "test",
        mac: None,
    };

    #[test]
    fn un_identifiant_inchange_est_le_meme_appareil() {
        assert!(meme_appareil(Some("uuid:a"), None, "uuid:a", &SANS));
    }

    #[test]
    fn un_autre_identifiant_sans_preuve_est_un_autre_appareil() {
        assert!(
            !meme_appareil(Some("uuid:a"), None, "uuid:b", &SANS),
            "sans MAC ni preuve locale, un autre identifiant est un AUTRE appareil (#6774)"
        );
        assert!(!meme_appareil(None, None, "uuid:b", &SANS));
    }

    #[test]
    fn la_meme_mac_normalisee_prouve_le_meme_appareil() {
        let r = Rattachement::SansPreuve {
            origine: "test",
            mac: Some("800A805C2689"),
        };
        assert!(meme_appareil(
            Some("uuid:a"),
            Some("80:0a:80:5c:26:89"),
            "uuid:b",
            &r
        ));
    }

    #[test]
    fn une_autre_mac_ou_une_mac_absente_ne_prouve_rien() {
        let autre = Rattachement::SansPreuve {
            origine: "test",
            mac: Some("80:0a:80:5c:26:90"),
        };
        assert!(!meme_appareil(
            Some("uuid:a"),
            Some("80:0a:80:5c:26:89"),
            "uuid:b",
            &autre
        ));
        let nouvelle_seule = Rattachement::SansPreuve {
            origine: "test",
            mac: Some("80:0a:80:5c:26:89"),
        };
        assert!(!meme_appareil(
            Some("uuid:a"),
            None,
            "uuid:b",
            &nouvelle_seule
        ));
        // Un identifiant opaque (TXT `id` d'un Chromecast) n'est pas une MAC.
        let opaque = Rattachement::SansPreuve {
            origine: "test",
            mac: Some("a8f3c1de9b7045"),
        };
        assert!(!meme_appareil(
            Some("uuid:a"),
            Some("a8f3c1de9b7045"),
            "uuid:b",
            &opaque
        ));
    }

    #[test]
    fn une_preuve_locale_d_endpoint_est_le_meme_appareil() {
        let r = Rattachement::MemePeripherique { origine: "test" };
        assert!(meme_appareil(Some("local:A"), None, "local:B", &r));
    }

    #[test]
    fn les_gabarits_de_volume_couvrent_trim_limites_et_pure() {
        for g in [
            "zone_{id}_gain_trim_db",
            "zone_{id}_volume_limit_safety",
            "zone_{id}_volume_limit_comfort",
            "zone_{id}_audiophile",
        ] {
            assert!(
                est_un_reglage_de_volume(g),
                "{g} doit être un réglage de volume"
            );
        }
        assert!(!est_un_reglage_de_volume("zone_{id}_crossfeed"));
        assert!(!est_un_reglage_de_volume("zone_{id}_eq_profile"));
    }
}
