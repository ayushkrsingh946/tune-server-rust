use super::{
    CreateGroup, GroupRefusal, choisir_les_sorties_doubles, porteur_du_groupe,
    recalculer_les_sorties_doubles, refus_asio, validate_group,
};
use tune_core::db::zone_repo::Zone;

// #1702 (Bilou, fil 1392) — deux zones pointant sur la même sortie : le
// groupement répondait « 422 unprocessable entity », un code nu, sans
// phrase. Deux causes distinctes, testées séparément :
//   1. le client web n'envoie pas de `name`, et serde rejetait le corps
//      avant même d'atteindre le handler → 422 d'axum, sans texte ;
//   2. rien ne vérifiait la sortie partagée, donc aucun message ne
//      pouvait l'expliquer.

fn zone(id: i64, name: &str, device: Option<&str>) -> Zone {
    Zone {
        id: Some(id),
        name: name.to_string(),
        output_type: Some("local".into()),
        output_device_id: device.map(str::to_string),
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

#[test]
fn payload_without_name_is_accepted() {
    // Le corps exact qu'envoie le client web. Il échouait ici.
    let body: CreateGroup =
        serde_json::from_str(r#"{"leader_id": 1, "zone_ids": [1, 2]}"#).unwrap();
    assert_eq!(body.zone_ids, vec![1, 2]);
    assert_eq!(body.leader_id, Some(1));
    assert_eq!(body.name, None);
}

#[test]
fn two_zones_on_the_same_output_are_refused_by_name() {
    let zones = vec![
        zone(1, "PC", Some("hw:0,0")),
        zone(2, "Haut parleurs", Some("hw:0,0")),
    ];
    assert_eq!(
        validate_group(&[1, 2], &zones),
        Err(GroupRefusal::SameOutput(
            "PC".into(),
            "Haut parleurs".into()
        )),
        "le refus doit nommer les deux zones pour que le message soit lisible"
    );
}

#[test]
fn two_zones_on_distinct_outputs_are_accepted() {
    let zones = vec![
        zone(1, "Salon", Some("hw:0,0")),
        zone(2, "Cuisine", Some("hw:1,0")),
    ];
    assert_eq!(validate_group(&[1, 2], &zones), Ok(vec![1, 2]));
}

#[test]
fn zones_without_an_output_are_not_duplicates_of_each_other() {
    // Deux zones orphelines ne partagent pas « la même sortie » : elles
    // n'en ont aucune. Les refuser ici afficherait un message faux.
    let zones = vec![zone(1, "Salon", None), zone(2, "Cuisine", None)];
    assert_eq!(validate_group(&[1, 2], &zones), Ok(vec![1, 2]));
}

#[test]
fn the_same_zone_twice_is_not_a_group() {
    let zones = vec![zone(1, "Salon", Some("hw:0,0"))];
    assert_eq!(
        validate_group(&[1, 1], &zones),
        Err(GroupRefusal::NotEnoughZones)
    );
}

#[test]
fn a_single_zone_is_not_a_group() {
    let zones = vec![
        zone(1, "Salon", Some("hw:0,0")),
        zone(2, "Cuisine", Some("hw:1,0")),
    ];
    assert_eq!(
        validate_group(&[1], &zones),
        Err(GroupRefusal::NotEnoughZones)
    );
}

#[test]
fn a_vanished_zone_is_named_in_the_refusal() {
    let zones = vec![zone(1, "Salon", Some("hw:0,0"))];
    assert_eq!(
        validate_group(&[1, 7], &zones),
        Err(GroupRefusal::UnknownZone(7))
    );
}

#[test]
fn duplicate_ids_are_collapsed_not_flagged_as_same_output() {
    let zones = vec![
        zone(1, "Salon", Some("hw:0,0")),
        zone(2, "Cuisine", Some("hw:1,0")),
    ];
    assert_eq!(validate_group(&[1, 2, 1], &zones), Ok(vec![1, 2]));
}

#[test]
fn every_refusal_has_a_french_sentence() {
    for key in [
        "zonegroup.needsTwoZones",
        "zonegroup.unknownZone",
        "zonegroup.sameOutput",
    ] {
        let msg = crate::i18n::t("fr", key);
        assert_ne!(
            msg, key,
            "{key} n'a pas de traduction : le client afficherait la clé"
        );
        assert!(msg.len() > 20, "{key} doit expliquer, pas juste nommer");
    }
}

// #6132 étape 2 — grouper deux zones locales de la machine donne une sortie
// double au meneur ; le reste du groupe n'est pas concerné.

#[test]
fn chaque_zone_locale_du_groupe_devient_une_suiveuse() {
    let zones = vec![
        zone(1, "Salon", Some("local:SMSL SU-8")),
        zone(2, "Bureau", Some("local:TASCAM US-366")),
        zone(3, "Cuisine", Some("dlna:uuid-1")),
        zone(4, "Atelier", Some("local:Marantz AV7706")),
        // Doublon de sortie de la zone 2 : une seule recopie par périphérique.
        zone(5, "Bureau bis", Some("local:TASCAM US-366")),
    ];
    assert_eq!(
        choisir_les_sorties_doubles(1, &[1, 3, 2, 4, 5], &zones),
        vec![
            (2, "local:TASCAM US-366".to_string()),
            (4, "local:Marantz AV7706".to_string()),
        ]
    );
    // Meneur réseau : pas de sortie double (étape 2b), même avec des locales.
    assert!(choisir_les_sorties_doubles(3, &[3, 1, 2], &zones).is_empty());
    // Une seule zone locale : rien à recopier.
    assert!(choisir_les_sorties_doubles(1, &[1, 3], &zones).is_empty());
}

#[test]
fn asio_refuse_la_sortie_double_en_nommant_les_zones() {
    assert_eq!(
        refus_asio(Some("asio"), Some("wasapi"), "Salon", "Bureau"),
        Some(GroupRefusal::AsioOneZoneAtATime(
            "Salon".into(),
            "Bureau".into()
        ))
    );
    assert_eq!(
        refus_asio(Some("wasapi"), Some("ASIO"), "Salon", "Bureau"),
        Some(GroupRefusal::AsioOneZoneAtATime(
            "Salon".into(),
            "Bureau".into()
        ))
    );
    assert_eq!(refus_asio(Some("wasapi"), Some("wasapi"), "S", "B"), None);
    assert_eq!(refus_asio(None, None, "S", "B"), None);
}

#[test]
fn la_vue_du_groupe_dit_qui_est_synchronise() {
    let groupe = serde_json::json!({
        "id": 1, "leader_id": 1, "zone_ids": [1, 2, 3, 4],
        "sortie_double": { "suiveurs": [
            { "zone_id": 2, "sortie": "local:B" },
            { "zone_id": 4, "sortie": "local:D" },
        ] },
    });
    let vue = crate::routes::zone_manager::generic_group_view(groupe);
    let sync = &vue["synchronization"];
    assert_eq!(sync["supported"], true);
    assert_eq!(sync["transport"], "local_double_output");
    assert_eq!(sync["synchronized_zone_ids"], serde_json::json!([1, 2, 4]));
    assert_eq!(sync["independent_zone_ids"], serde_json::json!([3]));

    let sans = crate::routes::zone_manager::generic_group_view(
        serde_json::json!({ "id": 2, "leader_id": 1, "zone_ids": [1, 3] }),
    );
    assert_eq!(sans["synchronization"]["supported"], false);
}

#[test]
fn le_refus_asio_a_sa_phrase_dans_chaque_langue() {
    for lang in ["fr", "en", "de", "es", "it", "zh", "ja", "ko", "ro", "sv"] {
        let t = crate::i18n::t(lang, "zonegroup.asioOneZone");
        assert!(
            t.contains("{a}") && t.contains("{b}") && t.contains("ASIO"),
            "{lang} : {t}"
        );
    }
}

/// #6132 — un groupe enregistré avant la sortie double (aucun champ
/// `sortie_double`, aucun réglage) est recalculé au démarrage : le meneur
/// reçoit ses deux suiveuses locales, sans geste de l'utilisateur. Un groupe
/// au meneur réseau reste tel qu'il était.
#[tokio::test]
async fn les_groupes_d_avant_sont_recalcules_au_demarrage() {
    use tune_core::db::settings_repo::SettingsRepo;
    use tune_core::db::zone_repo::ZoneRepo;
    let state =
        crate::state::AppState::new(":memory:", 0, crate::config::TuneConfig::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let salon = repo
        .create("Salon", Some("local"), Some("local:DAC A"))
        .unwrap();
    let bureau = repo
        .create("Bureau", Some("local"), Some("local:DAC B"))
        .unwrap();
    let atelier = repo
        .create("Atelier", Some("local"), Some("local:DAC C"))
        .unwrap();
    let cuisine = repo
        .create("Cuisine", Some("dlna"), Some("dlna:uuid-1"))
        .unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let anciens = serde_json::json!([
        { "id": 1, "group_id": "1", "name": "Maison", "leader_id": salon,
          "zone_ids": [salon, bureau, cuisine, atelier] },
        { "id": 2, "group_id": "2", "name": "Réseau", "leader_id": cuisine,
          "zone_ids": [cuisine, salon] },
    ]);
    settings.set("zone_groups", &anciens.to_string()).unwrap();

    recalculer_les_sorties_doubles(&state).await;

    let cle = tune_core::outputs::sortie_double::cle_de_zone;
    assert_eq!(
        tune_core::outputs::sortie_double::secondes_sorties_de_zone(&state.backend, salon),
        vec!["DAC B".to_string(), "DAC C".to_string()],
        "le meneur local doit recevoir ses deux suiveuses"
    );
    assert_eq!(settings.get(&cle(cuisine)).unwrap(), None);
    let groupes: Vec<serde_json::Value> =
        serde_json::from_str(&settings.get("zone_groups").unwrap().unwrap()).unwrap();
    assert_eq!(
        groupes[0]["sortie_double"]["suiveurs"][1]["zone_id"],
        serde_json::json!(atelier)
    );
    assert!(groupes[1].get("sortie_double").is_none());
    assert_eq!(
        groupes[0]["name"], "Maison",
        "le groupe n'est pas refait, seulement complété"
    );
}

/// #6132 étape 2b — meneur réseau : le premier membre local porte la sortie
/// double et sert les autres zones locales, synchronisées entre elles.
#[test]
fn un_meneur_reseau_confie_la_sortie_double_au_premier_local() {
    let zones = vec![
        zone(1, "Salon", Some("local:DAC A")),
        zone(2, "Bureau", Some("local:DAC B")),
        zone(3, "Cuisine", Some("dlna:uuid-1")),
    ];
    assert_eq!(porteur_du_groupe(3, &[3, 2, 1], &zones), Some(2));
    assert_eq!(porteur_du_groupe(1, &[1, 3, 2], &zones), Some(1));
    assert_eq!(porteur_du_groupe(3, &[3], &zones), None);
    assert_eq!(
        choisir_les_sorties_doubles(2, &[3, 2, 1], &zones),
        vec![(1, "local:DAC A".to_string())]
    );
}

/// #6132 étape 2b — au démarrage, un groupe au meneur réseau et à deux zones
/// locales reçoit sa sortie double sur le porteur local.
#[tokio::test]
async fn un_groupe_au_meneur_reseau_est_recalcule_sur_son_porteur() {
    use tune_core::db::settings_repo::SettingsRepo;
    use tune_core::db::zone_repo::ZoneRepo;
    let state =
        crate::state::AppState::new(":memory:", 0, crate::config::TuneConfig::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let cuisine = repo
        .create("Cuisine", Some("dlna"), Some("dlna:uuid-1"))
        .unwrap();
    let salon = repo
        .create("Salon", Some("local"), Some("local:DAC A"))
        .unwrap();
    let bureau = repo
        .create("Bureau", Some("local"), Some("local:DAC B"))
        .unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            "zone_groups",
            &serde_json::json!([{ "id": 1, "group_id": "1", "name": "Réseau",
                "leader_id": cuisine, "zone_ids": [cuisine, salon, bureau] }])
            .to_string(),
        )
        .unwrap();
    recalculer_les_sorties_doubles(&state).await;
    assert_eq!(
        tune_core::outputs::sortie_double::secondes_sorties_de_zone(&state.backend, salon),
        vec!["DAC B".to_string()]
    );
    assert!(
        tune_core::outputs::sortie_double::secondes_sorties_de_zone(&state.backend, cuisine)
            .is_empty()
    );
    let groupes: Vec<serde_json::Value> =
        serde_json::from_str(&settings.get("zone_groups").unwrap().unwrap()).unwrap();
    assert_eq!(
        groupes[0]["sortie_double"]["porteur_id"],
        serde_json::json!(salon)
    );
    // Le meneur réseau entraîne le porteur ; le bureau joue par lui.
    let zones = repo.list().unwrap();
    assert_eq!(
        tune_core::zones::groupe_de_lecture::zones_entrainees(&groupes, cuisine, &zones),
        vec![(salon, "local:DAC A".to_string())]
    );
}
