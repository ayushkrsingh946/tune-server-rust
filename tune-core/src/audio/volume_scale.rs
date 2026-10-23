//! Échelle de volume : le SEUL endroit où l'on passe du volume de l'API aux
//! décibels, et l'inverse.
//!
//! ## Pourquoi ce module existe (#1274)
//!
//! L'API expose le volume sur `0.0..=1.0`, et l'interface le présente en
//! pourcentage. Un pourcentage ne dit rien à qui règle une chaîne : sur le
//! forum, `zaurux` (Roon + HQPlayer + Diretta) l'a résumé en cinq mots : « pas
//! de réglage au dB près ». Ce module donne à ce nombre sa lecture
//! d'audiophile, l'atténuation en dB par rapport à la pleine échelle.
//!
//! ## Deux échelles depuis #6787
//!
//! Jusqu'à #6787, le volume d'une sortie LOGICIELLE (sortie locale, zone
//! navigateur) multipliait directement les échantillons : 50 % ne valaient que
//! −6 dB, 25 % −12 dB, et il fallait descendre vers 5 % pour un niveau
//! d'écoute (fil ASR #49, aedagnino : « tout joue beaucoup trop fort »).
//!
//! [`EchelleDeVolume::Perceptive`] est désormais la loi de ces sorties : le
//! volume est une POSITION de curseur, et le gain rendu suit une loi en dB sur
//! [`PLAGE_PERCEPTIVE_DB`] (60 dB) — chaque point de curseur vaut 0,6 dB —
//! prolongée sous [`GENOU_PERCEPTIF`] (10 %, −54 dB) par une droite jusqu'au
//! silence, pour que 0 reste le silence exact. Voir [`gain_perceptif`].
//!
//! [`EchelleDeVolume::Lineaire`] reste celle des APPAREILS (DLNA, OpenHome,
//! AirPlay, Chromecast, BluOS, Sonos, Squeezebox, HQPlayer, greffons…) : le
//! nombre part tel quel sur l'échelle 0..100 de l'appareil, qui a sa propre
//! courbe. Leur appliquer la nôtre ferait une double courbe.
//!
//! ### La règle d'or : jamais de hausse
//!
//! Pour toute position `p`, `gain_perceptif(p) <= p` (témoin
//! `la_courbe_ne_depasse_jamais_l_ancienne_loi`). Une valeur enregistrée sous
//! l'ancienne loi (volume de zone, limites #6541, mémoire du volume fixe) est
//! donc gardée TELLE QUELLE : relue sur la nouvelle échelle, elle joue au même
//! niveau (0 et 100 %) ou plus bas, jamais plus haut. C'est volontairement la
//! conversion retenue, et non « le même dB » : un nombre remonté pour garder le
//! même dB rejouerait PLUS FORT après un retour à une version antérieure, sur
//! une zone passée de la sortie locale à un appareil réseau, ou dans un client
//! qui applique encore le volume en linéaire. L'identité est la plus haute
//! valeur sûre sous les deux lois à la fois.
//!
//! ## Trois règles qu'aucune évolution ne doit casser
//!
//! 1. **Le zéro est le silence, pas un plancher.** `linear_to_db(0.0)` rend
//!    `None`, qui se sérialise en `null`. Rendre `-60` ou `-120` inventerait
//!    une atténuation finie là où il n'y a plus de son, et un client qui
//!    renverrait cette valeur rallumerait la zone.
//! 2. **La conversion est exacte et réversible.** Aucun arrondi n'est fait ici.
//!    Arrondir à 0,1 dB ferait dériver le volume à chaque aller-retour
//!    lecture → écriture ; le formatage pour l'affichage appartient au client.
//! 3. **Le plafond est l'unité.** `db_to_linear` refuse de dépasser 0 dB, comme
//!    `effective_volume_units` borne déjà le produit à 1.0 : au-dessus, on ne
//!    monte pas le son, on écrête.
//!
//! Ce module ne connaît ni le ReplayGain, ni le trim de gain par renderer
//! (`zone_{id}_gain_trim_db`), ni le mode PURE. Il convertit le volume
//! **utilisateur**, celui du curseur, et rien d'autre — le champ `volume_db`
//! d'une charge utile est donc toujours la lecture en dB du champ `volume`
//! qui l'accompagne, sur l'échelle de la zone (`volume_curve`), jamais celle
//! du gain réellement appliqué.

/// #6787 — plage de la loi perceptive : 0 % → −60 dB (avant le genou), 100 %
/// → 0 dB. Chaque point de curseur vaut 0,6 dB.
pub const PLAGE_PERCEPTIVE_DB: f64 = 60.0;

/// #6787 — sous cette position, la loi en dB laisse place à une droite
/// jusqu'au silence : sans elle, 0,1 % vaudrait encore −59,9 dB, plus fort
/// que l'ancienne loi (−60 dB), et 0 ne serait plus le silence.
pub const GENOU_PERCEPTIF: f64 = 0.1;

/// #6787 — résolution de la POSITION tenue par une sortie logicielle
/// (`LocalOutput::user_volume`), en millionièmes. Le millième de l'ancienne
/// loi suffisait à un gain linéaire ; sur la loi en dB, un millième de
/// position vaut 0,06 dB et l'arrondi par défaut d'une consigne exprimée en
/// gain (trim, dB demandé) s'y voyait. Le gain RENDU reste en millièmes
/// (`outputs::local::effective_volume_units`) : seuls les écrivains et les
/// lecteurs de la position changent d'unité.
pub const UNITES_DE_POSITION: u32 = 1_000_000;

/// #6787 — le gain perceptif d'une position exprimée en
/// [`UNITES_DE_POSITION`].
pub fn gain_des_unites(unites: u32) -> f64 {
    gain_perceptif(f64::from(unites) / f64::from(UNITES_DE_POSITION))
}

/// #6787 — la position `volume` (0..1) en [`UNITES_DE_POSITION`], arrondie
/// PAR DÉFAUT : la position tenue ne rend jamais plus que celle demandée.
pub fn unites_de_position(volume: f64) -> u32 {
    let volume = if volume.is_finite() { volume } else { 0.0 };
    (volume.clamp(0.0, 1.0) * f64::from(UNITES_DE_POSITION)) as u32
}

/// Gain linéaire de la loi en dB au genou : 10^(−54/20) ≈ 0,001995.
fn gain_au_genou() -> f64 {
    10f64.powf(-PLAGE_PERCEPTIVE_DB * (1.0 - GENOU_PERCEPTIF) / 20.0)
}

/// #6787 — le gain rendu (linéaire, 0..1) pour une position de curseur sur
/// l'échelle perceptive.
///
/// * `p >= GENOU_PERCEPTIF` : `10^(−60·(1 − p)/20)`, une loi en dB ;
/// * `0 < p < GENOU_PERCEPTIF` : la droite `gain_au_genou · p / GENOU`, qui
///   rejoint le silence ;
/// * `p <= 0` (ou `NaN`) : 0, le silence ; `p >= 1` : 1, l'unité exacte.
///
/// Repères (témoin `les_db_a_10_25_50_100`) : 10 % → −54 dB, 25 % → −45 dB,
/// 50 % → −30 dB, 100 % → 0 dB. Strictement croissante, et jamais au-dessus
/// de `p` : c'est ce qui rend la bascule incapable de monter le son.
// `!(x > 0.0)` n'est pas `x <= 0.0` : seul le premier écarte `NaN`.
#[allow(clippy::neg_cmp_op_on_partial_ord)]
pub fn gain_perceptif(position: f64) -> f64 {
    if !(position > 0.0) {
        return 0.0;
    }
    if position >= 1.0 {
        return 1.0;
    }
    if position < GENOU_PERCEPTIF {
        return gain_au_genou() * position / GENOU_PERCEPTIF;
    }
    10f64.powf(-PLAGE_PERCEPTIVE_DB * (1.0 - position) / 20.0)
}

/// #6787 — l'inverse exact de [`gain_perceptif`] : la position de curseur qui
/// rend `gain`. Sert aux consignes en dB et au trim d'une sortie logicielle.
#[allow(clippy::neg_cmp_op_on_partial_ord)]
pub fn position_perceptive(gain: f64) -> f64 {
    if !(gain > 0.0) {
        return 0.0;
    }
    if gain >= 1.0 {
        return 1.0;
    }
    let genou = gain_au_genou();
    if gain < genou {
        return (gain / genou * GENOU_PERCEPTIF).clamp(0.0, GENOU_PERCEPTIF);
    }
    (1.0 + 20.0 * gain.log10() / PLAGE_PERCEPTIVE_DB).clamp(GENOU_PERCEPTIF, 1.0)
}

/// #6787 — l'échelle sur laquelle se lit le `volume` d'une zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EchelleDeVolume {
    /// Appareil réseau : le nombre part tel quel sur l'échelle de l'appareil.
    Lineaire,
    /// Sortie logicielle (sortie locale, zone navigateur) : position de
    /// curseur, gain par [`gain_perceptif`].
    Perceptive,
}

impl EchelleDeVolume {
    /// L'échelle d'une zone d'après son `output_type`. Seules les sorties dont
    /// le volume est un gain numérique appliqué au PCM sont perceptives :
    /// `local` (y compris ses bras exclusifs ASIO, WASAPI, CoreAudio), rendue
    /// par le serveur, et `browser`, rendue par le client. Tout le reste, et
    /// une zone sans sortie, reste linéaire — le côté sûr : aucun nombre n'y
    /// change de sens.
    pub fn de_la_sortie(output_type: Option<&str>) -> Self {
        match output_type {
            Some("local") | Some("browser") => Self::Perceptive,
            _ => Self::Lineaire,
        }
    }

    /// Code publié dans `volume_curve` : le client qui rend le son lui-même
    /// (zone navigateur) applique la même loi quand il lit `db60`.
    pub fn code(self) -> &'static str {
        match self {
            Self::Lineaire => "linear",
            Self::Perceptive => "db60",
        }
    }

    /// Le gain linéaire que représente `volume` sur cette échelle.
    pub fn gain(self, volume: f64) -> f64 {
        match self {
            Self::Lineaire => volume.clamp(0.0, 1.0),
            Self::Perceptive => gain_perceptif(volume),
        }
    }

    /// Le `volume` qui représente le gain linéaire `gain` sur cette échelle.
    pub fn volume_pour_le_gain(self, gain: f64) -> f64 {
        match self {
            Self::Lineaire => gain.clamp(0.0, 1.0),
            Self::Perceptive => position_perceptive(gain),
        }
    }

    /// Lecture en dB de `volume` (`None` = silence).
    pub fn vers_db(self, volume: f64) -> Option<f64> {
        match self {
            Self::Lineaire => linear_to_db(volume),
            Self::Perceptive => linear_to_db(gain_perceptif(volume)),
        }
    }

    /// Le `volume` pour une atténuation en dB (`None` pour `NaN`).
    pub fn depuis_db(self, db: f64) -> Option<f64> {
        db_to_linear(db).map(|g| self.volume_pour_le_gain(g))
    }

    /// #6787 — décale `volume` de `delta_db` décibels de GAIN (le trim d'une
    /// zone), borné à l'unité. Sur l'échelle perceptive, multiplier la
    /// position par le facteur du trim ferait un tout autre écart en dB
    /// (+6 dB de trim à 30 % en donneraient +18) : le décalage se fait sur le
    /// gain, puis revient en position.
    pub fn decaler_en_db(self, volume: f64, delta_db: f64) -> f64 {
        let volume = if volume.is_finite() { volume } else { 0.0 };
        if !delta_db.is_finite() || delta_db == 0.0 {
            return volume.clamp(0.0, 1.0);
        }
        let facteur = 10f64.powf(delta_db / 20.0);
        match self {
            Self::Lineaire => (volume * facteur).clamp(0.0, 1.0),
            Self::Perceptive => position_perceptive((gain_perceptif(volume) * facteur).min(1.0)),
        }
    }
}

/// L'échelle de la zone `zone_id`, lue en base (`zones.output_type`).
/// Zone inconnue : linéaire.
pub fn echelle_de_la_zone(
    db: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
    zone_id: i64,
) -> EchelleDeVolume {
    let output_type = crate::db::zone_repo::ZoneRepo::with_backend(db.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .and_then(|z| z.output_type);
    EchelleDeVolume::de_la_sortie(output_type.as_deref())
}

/// Plafond de l'échelle : 0 dB, l'unité. Au-delà, il n'y a pas de volume en
/// plus, seulement de l'écrêtage.
pub const MAX_DB: f64 = 0.0;

/// Atténuation en dB d'un facteur de volume linéaire.
///
/// `None` signifie **silence** (−∞ dB) : c'est le cas de `0.0`, et aussi celui
/// d'une valeur négative ou `NaN`, qui ne peut pas être un volume audible.
///
/// L'entrée est bornée à `1.0` avant le logarithme, comme partout ailleurs
/// dans la chaîne : un état interne qui déborderait afficherait sinon un gain
/// positif que personne n'entendra jamais.
// `!(x > 0.0)` n'est pas `x <= 0.0` : seul le premier écarte `NaN` (clippy 1.98).
#[allow(clippy::neg_cmp_op_on_partial_ord)]
pub fn linear_to_db(linear: f64) -> Option<f64> {
    if !(linear > 0.0) {
        // Couvre 0.0, les négatifs et NaN — la négation est volontaire.
        return None;
    }
    Some(20.0 * linear.min(1.0).log10())
}

/// Facteur de volume linéaire pour une atténuation en dB.
///
/// `None` pour un `NaN` : c'est un refus, pas un silence, et l'appelant doit
/// répondre 400 plutôt que couper le son. `-inf` en revanche est légitime et
/// vaut exactement `0.0`, le silence.
///
/// Une valeur au-dessus de `MAX_DB` est ramenée à l'unité. Aucun plancher :
/// −200 dB rend un nombre minuscule mais positif, et c'est correct — le
/// silence se demande avec `volume: 0` ou avec le mute, pas avec un dB assez
/// bas pour ressembler à zéro.
pub fn db_to_linear(db: f64) -> Option<f64> {
    if db.is_nan() {
        return None;
    }
    if db == f64::NEG_INFINITY {
        return Some(0.0);
    }
    Some(10f64.powf(db.min(MAX_DB) / 20.0))
}

/// Résout le volume demandé par une requête qui parle en linéaire **ou** en dB.
///
/// C'est le point d'entrée unique de l'écriture, symétrique de
/// [`inserer_volume`] côté lecture : les quatre routes de volume du serveur
/// (POST et PUT `/zones/{id}/volume`, PATCH `/zones/{id}`, et le groupe)
/// n'ont pas la même convention historique pour `volume` — 0..1 pour le web,
/// 0..100 pour le widget, entier 0..100 pour le PATCH. Chacune ramène donc
/// **elle-même** son `volume` sur 0..1 avant d'appeler ici ; ce qui est
/// partagé, et qui doit l'être, c'est l'arbitrage entre les deux champs et la
/// conversion des dB.
///
/// Les deux champs sont **exclusifs**. Les accepter ensemble obligerait à
/// inventer un gagnant, et le perdant serait silencieusement ignoré : sur un
/// réglage de volume, c'est la recette d'un niveau surprise.
pub fn demande_lineaire(
    echelle: EchelleDeVolume,
    lineaire: Option<f64>,
    db: Option<f64>,
) -> Result<f64, &'static str> {
    match (lineaire, db) {
        (Some(_), Some(_)) => Err("volume et volume_db sont exclusifs — n'en envoyer qu'un"),
        (None, None) => Err("volume ou volume_db est requis"),
        (Some(v), None) => Ok(v.clamp(0.0, 1.0)),
        // Un dB positif est REFUSÉ, pas ramené au plafond : le ramener ferait
        // croire au client qu'il a obtenu son +3 dB. Il n'y a pas de gain
        // au-dessus de la pleine échelle, il n'y a que de l'écrêtage.
        (None, Some(db)) if db > MAX_DB => Err("volume_db doit être négatif ou nul (0 dB = 100 %)"),
        // #6787 — le dB demandé est un dB de GAIN : sur une sortie
        // logicielle, il revient à la position qui le rend.
        (None, Some(db)) => echelle.depuis_db(db).ok_or("volume_db n'est pas un nombre"),
    }
}

/// Pose `volume` **et** `volume_db` dans une charge utile de zone.
///
/// Les deux champs sortent d'ici ensemble, à partir du même nombre. C'est la
/// seule garantie qu'ils ne puissent pas se contredire : les payloads de zone
/// n'émettent pas tous la même valeur (certaines lisent l'état de lecture,
/// d'autres la colonne `zones.volume` arrondie au pour-cent), et un `volume_db`
/// recalculé depuis une autre source afficherait un dB qui ne correspond pas
/// au curseur rendu dans la même réponse.
///
/// #6787 — `volume_db` se lit sur l'échelle de la zone, et `volume_curve` dit
/// laquelle (`linear` ou `db60`) : un client qui rend le son lui-même (zone
/// navigateur) en tire sa loi, et un serveur antérieur, qui ne publie pas le
/// champ, reste linéaire.
pub fn inserer_volume(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    volume: f64,
    echelle: EchelleDeVolume,
) {
    obj.insert("volume".into(), serde_json::json!(volume));
    obj.insert(
        "volume_db".into(),
        serde_json::json!(echelle.vers_db(volume)),
    );
    obj.insert("volume_curve".into(), serde_json::json!(echelle.code()));
}

/// Motif de refus d'une consigne en dB qu'une sortie ne sait pas tenir (#1274).
///
/// `None` = la consigne passe. C'est le cas de toute sortie au réglage continu
/// ou en dB, et de toute consigne au-dessus du premier pas d'une grille : ce
/// garde-fou ne mord QUE là où le niveau demandé n'existe pas sur le fil.
///
/// Le cas qu'il attrape n'est pas une imprécision, c'est une extinction. Une
/// sortie DLNA, OpenHome, BluOS, Squeezebox, HQPlayer ou OAAT ne reçoit qu'un
/// entier 0..100 : `−50 dB` vaut 0,00316 en linéaire, l'entier envoyé est
/// `round(0,316) = 0`, et le renderer se tait. Le serveur, lui, gardait la
/// valeur exacte, la persistait, et répondait `200` — la zone était annoncée
/// à −50 dB et ne faisait aucun bruit, indiscernable d'un mute volontaire.
/// C'est le même défaut que #2886 avait corrigé dans la colonne `zones.volume`,
/// resté entier sur le fil.
///
/// Le message NOMME la grille et le plancher : sans le chiffre, le client ne
/// peut ni corriger sa demande ni construire un champ de saisie qui l'évite.
pub fn refus_de_resolution(
    resolution: crate::outputs::VolumeResolution,
    db: f64,
) -> Option<String> {
    let linear = db_to_linear(db)?;
    if resolution.holds(linear) {
        return None;
    }
    // Seule une grille linéaire peut avaler un niveau audible ; `holds` l'a
    // déjà établi, ce `else` n'existe que pour lire `steps`.
    let crate::outputs::VolumeResolution::Linear { steps } = resolution else {
        return None;
    };
    let plancher = resolution.floor_db()?;
    Some(format!(
        "{db:.1} dB est plus bas que ce que cette sortie sait tenir : elle ne reçoit qu'un \
         entier sur {steps} pas, et son plus petit niveau audible vaut {plancher:.1} dB. \
         Au-dessous, la valeur envoyée s'arrondit à zéro — la zone ne baisserait pas, elle \
         se tairait."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tolérance de comparaison en dB. Volontairement mille fois plus fine que
    /// le « dB près » que réclame l'issue : ce qu'on vérifie ici, c'est que la
    /// conversion est exacte, pas qu'elle est suffisante.
    const EPS_DB: f64 = 1e-9;

    #[test]
    fn bornes_de_l_echelle() {
        // Pleine échelle = 0 dB, exactement.
        assert_eq!(linear_to_db(1.0), Some(0.0));
        assert_eq!(db_to_linear(0.0), Some(1.0));
        // Le zéro est le silence, PAS un plancher chiffré : c'est la règle
        // qu'un plancher à -60 dB casserait, et un client qui renverrait ce
        // -60 rallumerait la zone.
        assert_eq!(linear_to_db(0.0), None);
        assert_eq!(db_to_linear(f64::NEG_INFINITY), Some(0.0));
    }

    #[test]
    fn mi_echelle_vaut_moins_six_db() {
        // Le malentendu que l'issue dénonce : 50 % n'est pas « moitié moins
        // fort », c'est -6,02 dB.
        let db = linear_to_db(0.5).expect("0.5 est audible");
        assert!((db - (-6.020_599_913_279_624)).abs() < EPS_DB, "{db}");
        // Et 90 %, que l'utilisateur croit à -10 dB, vaut -0,915 dB.
        let db90 = linear_to_db(0.9).expect("0.9 est audible");
        assert!((db90 - (-0.915_149_811_213_501_5)).abs() < EPS_DB, "{db90}");
    }

    #[test]
    fn decades_exactes() {
        for (linear, attendu) in [(0.1, -20.0), (0.01, -40.0), (0.001, -60.0)] {
            let db = linear_to_db(linear).expect("audible");
            assert!((db - attendu).abs() < 1e-12, "{linear} → {db}");
        }
    }

    #[test]
    fn reversible_lineaire_puis_db() {
        // Aller-retour dans le sens que lisent les clients : la valeur rendue
        // par l'API, renvoyée telle quelle, ne doit pas déplacer le volume.
        for i in 1..=1000 {
            let linear = f64::from(i) / 1000.0;
            let db = linear_to_db(linear).expect("audible");
            let retour = db_to_linear(db).expect("fini");
            assert!(
                (retour - linear).abs() < 1e-12,
                "{linear} → {db} dB → {retour}"
            );
        }
    }

    #[test]
    fn reversible_db_puis_lineaire() {
        // Aller-retour dans le sens qu'utilise un réglage au dB près, du
        // silence utile jusqu'à la pleine échelle, par pas de 0,1 dB.
        let mut db = -80.0;
        while db <= 0.0 {
            let linear = db_to_linear(db).expect("fini");
            let retour = linear_to_db(linear).expect("audible");
            assert!(
                (retour - db).abs() < EPS_DB,
                "{db} dB → {linear} → {retour}"
            );
            db += 0.1;
        }
    }

    #[test]
    fn le_plafond_est_l_unite() {
        // Demander +6 dB ne monte pas le son, ça écrête : on rend l'unité.
        assert_eq!(db_to_linear(6.0), Some(1.0));
        assert_eq!(db_to_linear(0.5), Some(1.0));
        // Symétriquement, un état interne débordé ne s'affiche pas en positif.
        assert_eq!(linear_to_db(1.5), Some(0.0));
    }

    #[test]
    fn entrees_aberrantes_refusees_sans_couper_le_son() {
        // NaN est un refus explicite, que la route doit traduire en 400.
        assert_eq!(db_to_linear(f64::NAN), None);
        // Un +inf reste borné au plafond plutôt que de rendre un infini.
        assert_eq!(db_to_linear(f64::INFINITY), Some(1.0));
        // Côté linéaire, tout ce qui n'est pas audible est du silence.
        assert_eq!(linear_to_db(-0.5), None);
        assert_eq!(linear_to_db(f64::NAN), None);
    }

    #[test]
    fn la_paire_json_est_toujours_coherente() {
        // Le champ additif ne remplace rien : `volume` reste EXACTEMENT ce
        // qu'il était, et `volume_db` en est la lecture, pas une autre mesure.
        let mut obj = serde_json::Map::new();
        inserer_volume(&mut obj, 0.5, EchelleDeVolume::Lineaire);
        assert_eq!(obj["volume"], serde_json::json!(0.5));
        let db = obj["volume_db"].as_f64().expect("un nombre");
        assert!((db - (-6.020_599_913_279_624)).abs() < EPS_DB, "{db}");

        // Silence : `volume` vaut toujours 0, et `volume_db` est `null` —
        // present, donc lisible, mais sans valeur inventée.
        let mut muet = serde_json::Map::new();
        inserer_volume(&mut muet, 0.0, EchelleDeVolume::Lineaire);
        assert_eq!(muet["volume"], serde_json::json!(0.0));
        assert_eq!(muet["volume_db"], serde_json::Value::Null);
        assert!(muet.contains_key("volume_db"), "le champ doit être présent");
    }

    #[test]
    fn demande_en_db_ou_en_lineaire_mais_jamais_les_deux() {
        // Le chemin que l'issue réclame : régler au dB près.
        let v = demande_lineaire(EchelleDeVolume::Lineaire, None, Some(-20.0))
            .expect("−20 dB est légitime");
        assert!((v - 0.1).abs() < 1e-12, "{v}");
        // Le chemin historique reste intact — c'est la rétro-compatibilité.
        assert_eq!(
            demande_lineaire(EchelleDeVolume::Lineaire, Some(0.42), None),
            Ok(0.42)
        );
        // Les deux ensemble : refus explicite, aucun gagnant inventé.
        assert!(demande_lineaire(EchelleDeVolume::Lineaire, Some(0.5), Some(-6.0)).is_err());
        // Aucun des deux : refus, et surtout PAS un volume par défaut.
        assert!(demande_lineaire(EchelleDeVolume::Lineaire, None, None).is_err());
    }

    #[test]
    fn un_db_positif_est_refuse_pas_rabote() {
        // Rendre 1.0 en silence ferait croire au client qu'il a eu son +3 dB.
        assert!(demande_lineaire(EchelleDeVolume::Lineaire, None, Some(3.0)).is_err());
        // La pleine échelle, elle, est une demande valide.
        assert_eq!(
            demande_lineaire(EchelleDeVolume::Lineaire, None, Some(0.0)),
            Ok(1.0)
        );
    }

    #[test]
    fn demande_lineaire_hors_bornes_ramenee_sans_paniquer() {
        // Le champ historique reste tolérant : les routes le bornaient déjà.
        assert_eq!(
            demande_lineaire(EchelleDeVolume::Lineaire, Some(1.7), None),
            Ok(1.0)
        );
        assert_eq!(
            demande_lineaire(EchelleDeVolume::Lineaire, Some(-3.0), None),
            Ok(0.0)
        );
    }

    #[test]
    fn le_reglage_au_db_pres_atteint_sa_cible() {
        // La promesse de l'issue, vérifiée d'un bout à l'autre de l'échelle
        // utile : demander −N dB et relire donne −N dB, au millième près.
        for n in 0..=60 {
            let cible = -f64::from(n);
            let lineaire =
                demande_lineaire(EchelleDeVolume::Lineaire, None, Some(cible)).expect("légitime");
            let relu = linear_to_db(lineaire).expect("audible");
            assert!(
                (relu - cible).abs() < 1e-9,
                "{cible} dB → {lineaire} → {relu}"
            );
        }
    }

    #[test]
    fn monotone_strictement_croissante() {
        // Une échelle de volume qui n'est pas monotone est un piège : deux
        // curseurs différents rendraient le même dB, ou pire, s'inverseraient.
        let mut precedent = f64::NEG_INFINITY;
        for i in 1..=1000 {
            let db = linear_to_db(f64::from(i) / 1000.0).expect("audible");
            assert!(db > precedent, "{i} : {db} <= {precedent}");
            precedent = db;
        }
    }

    /// #1274 — le refus nomme la grille ET le plancher, sinon il n'apprend
    /// rien à qui doit corriger sa demande.
    #[test]
    fn le_refus_nomme_la_grille_et_le_plancher() {
        use crate::outputs::VolumeResolution;
        let pour_cent = VolumeResolution::Linear { steps: 100 };
        let motif = refus_de_resolution(pour_cent, -50.0).expect("−50 dB est hors grille");
        assert!(motif.contains("-50.0 dB"), "{motif}");
        assert!(motif.contains("100 pas"), "{motif}");
        assert!(motif.contains("-40.0 dB"), "{motif}");
    }

    /// Et il se TAIT partout ailleurs : le garde-fou ne doit pas rendre le
    /// réglage en dB inutilisable là où le matériel suit.
    #[test]
    fn le_refus_se_tait_quand_la_sortie_suit() {
        use crate::outputs::VolumeResolution;
        let pour_cent = VolumeResolution::Linear { steps: 100 };
        // La cible de l'issue, et le plancher lui-même.
        for db in [-18.0, -20.0, -40.0, -6.0, 0.0] {
            assert_eq!(refus_de_resolution(pour_cent, db), None, "{db} dB");
        }
        // Les grilles fines et le continu n'opposent jamais de refus.
        for resolution in [
            VolumeResolution::Continuous,
            VolumeResolution::Decibels { step_mdb: 100 },
            VolumeResolution::Linear { steps: 65536 },
        ] {
            assert_eq!(
                refus_de_resolution(resolution, -80.0),
                None,
                "{resolution:?}"
            );
        }
    }

    // ── #6787 — la loi perceptive des sorties logicielles ──────────────────

    /// Témoin demandé par l'issue : les dB obtenus à 10, 25, 50 et 100 %.
    #[test]
    fn les_db_a_10_25_50_100() {
        let p = EchelleDeVolume::Perceptive;
        for (position, attendu) in [(0.10, -54.0), (0.25, -45.0), (0.50, -30.0), (1.0, 0.0)] {
            let db = p.vers_db(position).expect("audible");
            assert!(
                (db - attendu).abs() < 1e-9,
                "{position} → {db} dB, attendu {attendu}"
            );
        }
        // L'ancienne loi, pour mémoire : c'est elle qui « jouait trop fort ».
        let l = EchelleDeVolume::Lineaire;
        for (position, attendu) in [(0.10, -20.0), (0.25, -12.041), (0.50, -6.0206), (1.0, 0.0)] {
            let db = l.vers_db(position).expect("audible");
            assert!((db - attendu).abs() < 1e-3, "{position} → {db} dB");
        }
        // Le niveau que le testeur cherchait à 5 % (−26 dB) tombe vers 57 %.
        let pos = p.depuis_db(-26.0).expect("fini");
        assert!((pos - 0.5666).abs() < 1e-3, "{pos}");
    }

    /// La règle d'or : sur toute l'échelle, la nouvelle loi ne rend JAMAIS
    /// plus de gain que l'ancienne pour le même nombre. C'est elle qui permet
    /// de garder les valeurs enregistrées telles quelles.
    #[test]
    fn la_courbe_ne_depasse_jamais_l_ancienne_loi() {
        // Au millionième sur toute l'échelle, et sur la grille de la sortie
        // locale (millièmes) en exact.
        for i in 0..=1_000_000u32 {
            let p = f64::from(i) / 1_000_000.0;
            let g = gain_perceptif(p);
            assert!(g <= p, "{p} → {g} : la courbe monterait le son");
        }
        assert_eq!(gain_perceptif(0.0), 0.0, "0 reste le silence");
        assert_eq!(gain_perceptif(1.0), 1.0, "100 % reste l'unité exacte");
        assert_eq!(gain_perceptif(f64::NAN), 0.0);
        assert_eq!(gain_perceptif(-0.3), 0.0);
        assert_eq!(gain_perceptif(1.7), 1.0);
    }

    #[test]
    fn la_courbe_est_continue_et_strictement_croissante() {
        let mut precedent = 0.0;
        for i in 1..=100_000u32 {
            let g = gain_perceptif(f64::from(i) / 100_000.0);
            assert!(g > precedent, "{i} : {g} <= {precedent}");
            precedent = g;
        }
        // Continue au genou : les deux branches s'y rejoignent.
        let juste_avant = gain_perceptif(GENOU_PERCEPTIF - 1e-12);
        let au_genou = gain_perceptif(GENOU_PERCEPTIF);
        assert!(
            (au_genou - juste_avant).abs() < 1e-12,
            "{juste_avant} / {au_genou}"
        );
    }

    #[test]
    fn la_position_inverse_exactement_le_gain() {
        for i in 0..=10_000u32 {
            let p = f64::from(i) / 10_000.0;
            let retour = position_perceptive(gain_perceptif(p));
            assert!((retour - p).abs() < 1e-9, "{p} → {retour}");
        }
        // Et dans l'autre sens, en dB, de −80 à 0 dB.
        let e = EchelleDeVolume::Perceptive;
        let mut db = -80.0;
        while db <= 0.0 {
            let relu = e.vers_db(e.depuis_db(db).expect("fini")).expect("audible");
            assert!((relu - db).abs() < 1e-9, "{db} → {relu}");
            db += 0.1;
        }
    }

    #[test]
    fn seules_les_sorties_logicielles_sont_perceptives() {
        assert_eq!(
            EchelleDeVolume::de_la_sortie(Some("local")),
            EchelleDeVolume::Perceptive
        );
        assert_eq!(
            EchelleDeVolume::de_la_sortie(Some("browser")),
            EchelleDeVolume::Perceptive
        );
        // Les appareils réseau gardent leur échelle : pas de double courbe.
        for t in [
            "dlna",
            "openhome",
            "airplay",
            "airplay2",
            "chromecast",
            "bluos",
            "sonos",
            "squeezebox",
            "slimproto",
            "hqplayer",
            "oaat",
            "oaat-multiroom",
            "bridge",
        ] {
            assert_eq!(
                EchelleDeVolume::de_la_sortie(Some(t)),
                EchelleDeVolume::Lineaire,
                "{t}"
            );
        }
        assert_eq!(
            EchelleDeVolume::de_la_sortie(None),
            EchelleDeVolume::Lineaire
        );
        // Sur l'échelle linéaire, rien ne change de sens.
        assert_eq!(EchelleDeVolume::Lineaire.gain(0.37), 0.37);
        assert_eq!(EchelleDeVolume::Lineaire.decaler_en_db(0.4, 0.0), 0.4);
    }

    #[test]
    fn le_trim_d_une_sortie_logicielle_est_un_ecart_de_gain() {
        let e = EchelleDeVolume::Perceptive;
        // +6 dB de trim à 30 % : +6 dB de gain, pas les +18 qu'aurait donnés
        // la multiplication de la position.
        let avant = e.vers_db(0.30).expect("audible");
        let apres = e.vers_db(e.decaler_en_db(0.30, 6.0)).expect("audible");
        assert!((apres - avant - 6.0).abs() < 1e-9, "{avant} → {apres}");
        let bas = e.vers_db(e.decaler_en_db(0.30, -6.0)).expect("audible");
        assert!((avant - bas - 6.0).abs() < 1e-9, "{avant} → {bas}");
        // Jamais au-dessus de l'unité, et le silence reste le silence.
        assert_eq!(e.decaler_en_db(0.95, 12.0), 1.0);
        assert_eq!(e.decaler_en_db(0.0, 12.0), 0.0);
        // Un trim illisible ne bouge rien.
        assert_eq!(e.decaler_en_db(0.4, f64::NAN), 0.4);
        // Et sur toute l'échelle, le gain trimé ne dépasse jamais celui de
        // l'ancienne loi au même trim.
        for trim in [-12.0, -6.0, -1.5, 1.5, 6.0, 12.0] {
            let f = 10f64.powf(trim / 20.0);
            for i in 0..=1000u32 {
                let p = f64::from(i) / 1000.0;
                let nouveau = gain_perceptif(e.decaler_en_db(p, trim));
                let ancien = (p * f).clamp(0.0, 1.0);
                assert!(
                    nouveau <= ancien + 1e-12,
                    "{p} @ {trim} dB : {nouveau} > {ancien}"
                );
            }
        }
    }

    #[test]
    fn la_paire_json_suit_l_echelle_de_la_zone() {
        let mut obj = serde_json::Map::new();
        inserer_volume(&mut obj, 0.5, EchelleDeVolume::Perceptive);
        assert_eq!(obj["volume"], serde_json::json!(0.5));
        assert_eq!(obj["volume_curve"], serde_json::json!("db60"));
        let db = obj["volume_db"].as_f64().expect("un nombre");
        assert!((db + 30.0).abs() < 1e-9, "{db}");
        let mut lin = serde_json::Map::new();
        inserer_volume(&mut lin, 0.5, EchelleDeVolume::Lineaire);
        assert_eq!(lin["volume_curve"], serde_json::json!("linear"));
        // Une consigne en dB revient à la position qui la rend.
        let v = demande_lineaire(EchelleDeVolume::Perceptive, None, Some(-30.0)).expect("légitime");
        assert!((v - 0.5).abs() < 1e-12, "{v}");
        // Le champ historique passe tel quel : c'est une position.
        assert_eq!(
            demande_lineaire(EchelleDeVolume::Perceptive, Some(0.42), None),
            Ok(0.42)
        );
    }
}
