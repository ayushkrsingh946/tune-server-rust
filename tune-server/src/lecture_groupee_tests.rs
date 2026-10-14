//! #6132 étape 2b — le moteur de la lecture groupée.

use super::*;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::playback::NowPlaying;

struct Banc {
    state: AppState,
    _dir: tempfile::TempDir,
}

/// Un meneur (zone 1) et une suiveuse réseau (zone 2), chacun sur une sortie
/// de banc, groupés ; une zone 3 hors groupe.
async fn banc() -> Banc {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("groupe.db");
    let config = crate::config::TuneConfig {
        db_path: db.to_str().unwrap().to_owned(),
        ..Default::default()
    };
    let state = AppState::new(db.to_str().unwrap(), 0, config).unwrap();
    for (id, nom, sortie) in [
        (1i64, "Salon", "mock-6132-meneur"),
        (2, "Cuisine", "mock-6132-suiveuse"),
        (3, "Seule", "mock-6132-seule"),
    ] {
        state
            .backend
            .execute(
                "INSERT INTO zones (id,name,output_type,output_device_id) VALUES (?1,?2,?3,?4)",
                &[&id, &nom, &"mock", &sortie],
            )
            .unwrap();
        state
            .outputs
            .lock()
            .await
            .register(Box::new(tune_core::outputs::mock::MockOutput::new(
                sortie, nom,
            )));
    }
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            "zone_groups",
            &serde_json::json!([
                { "id": 1, "group_id": "1", "name": "Maison", "leader_id": 1, "zone_ids": [1, 2] }
            ])
            .to_string(),
        )
        .unwrap();
    Banc { state, _dir: dir }
}

async fn arrets(state: &AppState, sortie: &str) -> u64 {
    let outputs = state.outputs.lock().await;
    let sortie = outputs.get(sortie).unwrap();
    let sortie = sortie.lock().await;
    sortie
        .as_any()
        .downcast_ref::<tune_core::outputs::mock::MockOutput>()
        .unwrap()
        .stop_call_count()
}

fn evenement(nom: &str, zone_id: i64) -> PlaybackEvent {
    PlaybackEvent {
        event: nom.into(),
        zone_id,
        data: serde_json::json!({}),
    }
}

#[tokio::test]
async fn chaque_evenement_du_meneur_a_sa_commande() {
    let b = banc().await;
    let mut meneur = b.state.playback.get_state(1).await;
    meneur.now_playing = Some(NowPlaying {
        track_id: Some(7),
        title: "So What".into(),
        source: "local".into(),
        duration_ms: 545_000,
        ..Default::default()
    });
    meneur.position_ms = 30_000;
    let entrainees = vec![(2, "mock-6132-suiveuse".to_string())];

    let jouer = commandes_pour("started", &meneur, &entrainees);
    match &jouer[..] {
        [(2, sortie, Commande::Jouer(req))] => {
            assert_eq!(sortie, "mock-6132-suiveuse");
            assert_eq!(req.zone_id, 2);
            assert_eq!(req.track_id, Some(7));
            assert_eq!(
                req.seek_ms,
                Some(30_000),
                "la suiveuse rejoint le meneur où il en est"
            );
        }
        autre => panic!("started : {autre:?}"),
    }
    meneur.position_ms = 500;
    match &commandes_pour("track_changed", &meneur, &entrainees)[..] {
        [(2, _, Commande::Jouer(req))] => assert_eq!(req.seek_ms, None, "début de piste"),
        autre => panic!("track_changed : {autre:?}"),
    }
    assert!(matches!(
        commandes_pour("paused", &meneur, &entrainees)[..],
        [(2, _, Commande::Pause)]
    ));
    assert!(matches!(
        commandes_pour("resumed", &meneur, &entrainees)[..],
        [(2, _, Commande::Reprise)]
    ));
    assert!(matches!(
        commandes_pour("stopped", &meneur, &entrainees)[..],
        [(2, _, Commande::Arret)]
    ));
    meneur.position_ms = 61_000;
    assert!(matches!(
        commandes_pour("seek", &meneur, &entrainees)[..],
        [(2, _, Commande::Saut(61_000))]
    ));
    assert!(
        commandes_pour("volume", &meneur, &entrainees).is_empty(),
        "le volume reste par zone"
    );
    meneur.now_playing = None;
    assert!(commandes_pour("started", &meneur, &entrainees).is_empty());
}

/// Le vrai chemin : un arrêt du meneur arrête la sortie de la suiveuse, et
/// rien d'autre ; un évènement d'une suiveuse ou d'une zone hors groupe ne
/// commande personne.
#[tokio::test]
async fn l_arret_du_meneur_arrete_la_suiveuse_et_elle_seule() {
    let b = banc().await;
    let (suiveuse_avant, seule_avant) = (
        arrets(&b.state, "mock-6132-suiveuse").await,
        arrets(&b.state, "mock-6132-seule").await,
    );
    assert_eq!(traiter(&b.state, &evenement("stopped", 1)).await, 1);
    assert_eq!(
        arrets(&b.state, "mock-6132-suiveuse").await,
        suiveuse_avant + 1
    );
    assert_eq!(arrets(&b.state, "mock-6132-seule").await, seule_avant);

    assert_eq!(
        traiter(&b.state, &evenement("stopped", 2)).await,
        0,
        "une suiveuse n'entraîne personne"
    );
    assert_eq!(traiter(&b.state, &evenement("stopped", 3)).await, 0);
    assert_eq!(traiter(&b.state, &evenement("position", 1)).await, 0);
}

/// La zone suiveuse se dit en groupe dans sa charge utile — par l'injecteur
/// que les quatre charges utiles de zone appellent.
#[tokio::test]
async fn la_charge_utile_de_la_suiveuse_dit_son_groupe() {
    let b = banc().await;
    let repo = tune_core::db::zone_repo::ZoneRepo::with_backend(b.state.backend.clone());
    let place = |zone_id: i64| {
        let mut obj = serde_json::Map::new();
        crate::routes::zones::injecter_reglages_renderer(&mut obj, &repo, zone_id);
        obj.remove("groupe_de_lecture").unwrap()
    };
    let suiveuse = place(2);
    assert_eq!(suiveuse["role"], "suiveuse");
    assert_eq!(suiveuse["meneur_id"], 1);
    assert_eq!(suiveuse["nom"], "Maison");
    assert_eq!(place(1)["role"], "meneur");
    assert!(place(3).is_null(), "hors groupe : null");
}
