//! #6132 étape 2b — qui le meneur entraîne, ce que la suiveuse joue, comment
//! chaque zone se présente.

use super::*;

fn zone(id: i64, nom: &str, sortie: Option<&str>) -> Zone {
    Zone {
        id: Some(id),
        name: nom.into(),
        output_type: None,
        output_device_id: sortie.map(str::to_string),
        volume: 50.0,
        muted: false,
        online: true,
        gapless_enabled: false,
        group_id: None,
        sync_delay_ms: 0,
        last_position_ms: 0,
        last_track_id: None,
        last_track_source: None,
        last_track_source_id: None,
        max_sample_rate: None,
        fixed_volume: false,
        autoplay_enabled: false,
    }
}

fn zones() -> Vec<Zone> {
    vec![
        zone(1, "Salon", Some("local:DAC A")),
        zone(2, "Bureau", Some("local:DAC B")),
        zone(3, "Cuisine", Some("dlna:uuid-cuisine")),
        zone(4, "Terrasse", Some("airplay:terrasse")),
        zone(5, "Orpheline", None),
    ]
}

/// Meneur local : il entraîne les membres réseau ; la zone locale servie par
/// sa sortie double n'est pas entraînée en plus (elle joue déjà, synchronisée).
#[test]
fn le_meneur_entraine_les_membres_reseau_pas_les_locales_servies() {
    let groupes = vec![json!({
        "id": 1, "group_id": "1", "name": "Maison", "leader_id": 1,
        "zone_ids": [1, 2, 3, 4, 5],
        "sortie_double": { "porteur_id": 1, "suiveurs": [{ "zone_id": 2, "sortie": "local:DAC B" }] },
    })];
    assert_eq!(
        zones_entrainees(&groupes, 1, &zones()),
        vec![
            (3, "dlna:uuid-cuisine".to_string()),
            (4, "airplay:terrasse".to_string()),
        ]
    );
    // Une suiveuse n'entraîne personne : pas de boucle.
    assert!(zones_entrainees(&groupes, 3, &zones()).is_empty());
    assert_eq!(meneur_de(&groupes, 3), Some(1));
    assert_eq!(meneur_de(&groupes, 1), None);
}

/// Meneur réseau : il entraîne le porteur local, qui sert lui-même l'autre
/// zone locale par sa sortie double — les deux locales jouent synchronisées.
#[test]
fn un_meneur_reseau_entraine_le_porteur_local() {
    let groupes = vec![json!({
        "id": 2, "group_id": "2", "name": "Réseau", "leader_id": 3,
        "zone_ids": [3, 1, 2],
        "sortie_double": { "porteur_id": 1, "suiveurs": [{ "zone_id": 2, "sortie": "local:DAC B" }] },
    })];
    assert_eq!(
        zones_entrainees(&groupes, 3, &zones()),
        vec![(1, "local:DAC A".to_string())]
    );
    let porteur = place_de_la_zone(&groupes, 1).unwrap();
    assert_eq!(porteur["role"], "suiveuse");
    assert_eq!(porteur["synchronisee"], true);
    let meneur = place_de_la_zone(&groupes, 3).unwrap();
    assert_eq!(meneur["role"], "meneur");
    assert_eq!(meneur["synchronisee"], false);
}

#[test]
fn la_zone_suiveuse_se_dit_en_groupe() {
    let groupes = vec![json!({
        "id": 1, "group_id": "1", "name": "Maison", "leader_id": 1, "zone_ids": [1, 3],
    })];
    let place = place_de_la_zone(&groupes, 3).unwrap();
    assert_eq!(place["role"], "suiveuse");
    assert_eq!(place["meneur_id"], 1);
    assert_eq!(place["group_id"], "1");
    assert_eq!(place["nom"], "Maison");
    assert_eq!(place["synchronisee"], false);
    assert!(
        place_de_la_zone(&groupes, 4).is_none(),
        "hors groupe : rien"
    );
}

#[test]
fn la_suiveuse_joue_la_meme_piste_a_la_meme_position() {
    let np = NowPlaying {
        track_id: Some(42),
        title: "Kind of Blue".into(),
        artist_name: Some("Miles Davis".into()),
        source: "local".into(),
        duration_ms: 545_000,
        ..Default::default()
    };
    let req = requete_de_suiveuse(&np, 3, "dlna:uuid-cuisine", 12_000);
    assert_eq!(req.zone_id, 3);
    assert_eq!(req.output_device_id.as_deref(), Some("dlna:uuid-cuisine"));
    assert_eq!(req.track_id, Some(42));
    assert_eq!(req.source.as_deref(), Some("local"));
    assert_eq!(req.seek_ms, Some(12_000));
    assert_eq!(requete_de_suiveuse(&np, 3, "dlna:x", 0).seek_ms, None);
}
