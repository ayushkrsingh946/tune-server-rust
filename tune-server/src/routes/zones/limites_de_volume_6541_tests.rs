//! #6541 — les routes des boutons + / − et des limites, de bout en bout par
//! le routeur public, sur une sortie factice.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;

const APPAREIL: &str = "dlna-salon-6541";

async fn serveur() -> (axum::Router, crate::state::AppState, i64) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    let zone_id = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("dlna"), Some(APPAREIL))
        .unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Salon").with_type("dlna"),
    ));
    state.playback.set_volume(zone_id, 0.2).await;
    (routeur, state, zone_id)
}

async fn appeler(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Value,
) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("Content-Type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn recus(state: &crate::state::AppState) -> Vec<f64> {
    let outputs = state.outputs.lock().await;
    let out = outputs.get(APPAREIL).unwrap();
    let guard = out.lock().await;
    guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .unwrap()
        .volume_calls()
        .await
}

fn proche(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

/// Le scénario du fil, par l'API : sécurité 70 %, confort −10,457 dB (30 %) ;
/// le PUT du curseur à 100 % s'arrête au confort, les boutons montent jusqu'à
/// la sécurité et le disent quand elle les retient.
#[tokio::test]
async fn curseur_borne_au_confort_et_boutons_bornes_a_la_securite_6541() {
    let (app, state, id) = serveur().await;
    let (statut, corps) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume-limits"),
        json!({ "safety": 0.7, "comfort_db": 20.0 * 0.3f64.log10() }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(proche(corps["volume_limit_comfort"].as_f64().unwrap(), 0.3));

    let (statut, _) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume"),
        json!({ "volume": 1.0 }),
    )
    .await;
    assert_eq!(statut, StatusCode::NO_CONTENT);
    assert!(proche(*recus(&state).await.last().unwrap(), 0.3));

    let mut dernier = Value::Null;
    for _ in 0..30 {
        let (statut, corps) = appeler(
            &app,
            "POST",
            &format!("/api/v1/zones/{id}/volume/step"),
            json!({ "direction": "up", "step": 0.05 }),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        dernier = corps;
    }
    assert!(
        proche(dernier["volume"].as_f64().unwrap(), 0.7),
        "{dernier}"
    );
    assert_eq!(dernier["limited"], true);
    assert!(
        recus(&state).await.iter().all(|v| *v <= 0.7 + 1e-9),
        "aucune commande au-dessus de la sécurité : {:?}",
        recus(&state).await
    );
}

/// Poser une sécurité sous le volume courant ramène la zone sur-le-champ.
#[tokio::test]
async fn une_securite_sous_le_volume_courant_ramene_la_zone_6541() {
    let (app, state, id) = serveur().await;
    appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume"),
        json!({ "volume": 0.9 }),
    )
    .await;
    let (statut, _) = appeler(
        &app,
        "PUT",
        &format!("/api/v1/zones/{id}/volume-limits"),
        json!({ "safety": 0.5 }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(recus(&state).await, vec![0.9, 0.5]);
    // GET de la zone : les limites sont publiées.
    let (_, zone) = appeler(&app, "GET", &format!("/api/v1/zones/{id}"), Value::Null).await;
    assert!(
        proche(zone["volume_limit_safety"].as_f64().unwrap(), 0.5),
        "{zone}"
    );
    assert!(zone["volume_limit_comfort"].is_null());
}

#[tokio::test]
async fn les_demandes_invalides_sont_refusees_6541() {
    let (app, _state, id) = serveur().await;
    for corps in [
        json!({ "safety": 0.7, "safety_db": -3.0 }),
        json!({ "safety": 0.0 }),
        json!({ "comfort_db": 3.0 }),
    ] {
        let (statut, _) = appeler(
            &app,
            "PUT",
            &format!("/api/v1/zones/{id}/volume-limits"),
            corps.clone(),
        )
        .await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    }
    for corps in [
        json!({ "direction": "left" }),
        json!({ "direction": "up", "step": 0.5 }),
        json!({ "direction": "up", "step": 0.02, "step_db": 1.0 }),
    ] {
        let (statut, _) = appeler(
            &app,
            "POST",
            &format!("/api/v1/zones/{id}/volume/step"),
            corps.clone(),
        )
        .await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    }
}

/// Sans limite, le bouton fait un pas de 2 points (défaut) ; le PUT efface.
#[tokio::test]
async fn sans_limite_le_pas_par_defaut_est_de_deux_points_6541() {
    let (app, _state, id) = serveur().await;
    let (_, corps) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{id}/volume/step"),
        json!({ "direction": "down" }),
    )
    .await;
    assert!(proche(corps["volume"].as_f64().unwrap(), 0.18), "{corps}");
    assert_eq!(corps["limited"], false);
}
