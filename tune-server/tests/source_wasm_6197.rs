//! #6197 — de bout en bout : un greffon WASM FOURNIT une source de streaming.
//!
//! Le greffon d'exemple (`tests/fixtures/plugins/source-exemple`, source Rust
//! et `main.wasm`) est un faux service qui lit son catalogue — deux albums —
//! et ses adresses de flux sur un serveur HTTP de test. Le test le charge
//! contre le VRAI hôte, puis passe par les routes `/streaming/{service}/…`
//! que servent Qobuz ou Deezer :
//!
//! 1. connexion : le greffon pose la session par `host_http` (le cookie reste
//!    chez l'hôte) ; un mauvais mot de passe est refusé ;
//! 2. les identifiants partent en base SCELLÉS : ni l'adresse ni le mot de
//!    passe ne s'y lisent ;
//! 3. recherche, album, adresse de lecture ;
//! 4. un domaine hors liste est refusé, À TRAVERS le vrai hôte ;
//! 5. redémarrage : un greffon neuf, sans session, se reconnecte seul depuis
//!    les identifiants scellés.
#![cfg(feature = "plugins-wasm")]

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path as Chemin, Query};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_server::state::AppState;

const MOT_DE_PASSE: &str = "s3cr3t-MusicMe!";
const ADRESSE: &str = "charly@exemple.fr";
const SERVICE: &str = "exemple-wasm";

fn catalogue(base: &str) -> Value {
    json!([
        {
            "ref": "alb-1", "titre": "Kind of Blue", "artiste": "Miles Davis", "annee": 1959,
            "pochette": format!("{base}/pochettes/alb-1.jpg"),
            "pistes": [
                { "ref": "trk-1", "titre": "So What", "numero": 1, "duree_s": 562 },
                { "ref": "trk-2", "titre": "Freddie Freeloader", "numero": 2, "duree_s": 586 }
            ]
        },
        {
            "ref": "alb-2", "titre": "Mingus Ah Um", "artiste": "Charles Mingus", "annee": 1959,
            "pochette": null,
            "pistes": [
                { "ref": "trk-3", "titre": "Better Git It in Your Soul", "numero": 1, "duree_s": 440 },
                { "ref": "trk-4", "titre": "Goodbye Pork Pie Hat", "numero": 2, "duree_s": 344 }
            ]
        }
    ])
}

fn connecte(entetes: &HeaderMap) -> bool {
    entetes
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.contains("session=jeton-42"))
}

/// Le faux service : son propre dialecte, une session par cookie.
async fn faux_service() -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());

    let b = base.clone();
    let app = Router::new()
        .route(
            "/login",
            post(|Json(corps): Json<Value>| async move {
                if corps["email"] == ADRESSE && corps["password"] == MOT_DE_PASSE {
                    (
                        StatusCode::OK,
                        [(header::SET_COOKIE, "session=jeton-42; Path=/; HttpOnly")],
                        Json(json!({ "abonne": "Charly", "offre": "Premium" })),
                    )
                        .into_response()
                } else {
                    StatusCode::UNAUTHORIZED.into_response()
                }
            }),
        )
        .route("/catalogue", {
            let b = b.clone();
            get(
                move |entetes: HeaderMap,
                      Query(_q): Query<std::collections::HashMap<String, String>>| async move {
                    if !connecte(&entetes) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    Json(json!({ "resultats": catalogue(&b) })).into_response()
                },
            )
        })
        .route("/albums/{id}", {
            let b = b.clone();
            get(
                move |entetes: HeaderMap, Chemin(id): Chemin<String>| async move {
                    if !connecte(&entetes) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    match catalogue(&b)
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|a| a["ref"] == id.as_str())
                    {
                        Some(a) => Json(a.clone()).into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                },
            )
        })
        .route("/flux/{id}", {
            let b = b.clone();
            get(
                move |entetes: HeaderMap, Chemin(id): Chemin<String>| async move {
                    if !connecte(&entetes) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    Json(json!({ "adresse": format!("{b}/audio/{id}.mp3"), "expire_a": null }))
                        .into_response()
                },
            )
        });
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    base
}

async fn appeler(
    app: &Router,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(uri)
        .header("content-type", "application/json")
        .body(corps.map_or_else(Body::empty, |c| Body::from(c.to_string())))
        .unwrap();
    let rep: Response = app.clone().oneshot(req).await.unwrap();
    let statut = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn copier_le_greffon(vers: &Path) {
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugins/source-exemple");
    let cible = vers.join("source-exemple");
    std::fs::create_dir_all(&cible).unwrap();
    for f in ["manifest.json", "main.wasm"] {
        std::fs::copy(source.join(f), cible.join(f)).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_greffon_wasm_fournit_une_source_de_streaming() {
    let base = faux_service().await;
    let dir = tempfile::tempdir().unwrap();
    copier_le_greffon(dir.path());
    let cle = dir.path().join("cles").join("plugin-credentials.key");
    unsafe {
        std::env::set_var("TUNE_PLUGINS_DIR", dir.path());
        std::env::set_var("TUNE_WASM_PROBE_SKIP", "1");
        std::env::set_var("TUNE_PLUGIN_CREDENTIALS_KEY", &cle);
    }

    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_server::plugins_host::load_wasm_plugins(&state).await;
    assert!(
        state
            .wasm_plugins
            .get()
            .unwrap()
            .get("source-exemple")
            .is_some(),
        "le greffon d'exemple doit être chargé"
    );
    assert!(
        state.services.lock().await.get(SERVICE).is_some(),
        "la source doit être inscrite dans le registre des services"
    );
    let app = tune_server::routes::router(state.clone());

    // 1. Connexion — d'abord refusée, puis acceptée.
    let (_, rep) = appeler(
        &app,
        "POST",
        &format!("/api/v1/streaming/{SERVICE}/auth"),
        Some(json!({ "email": ADRESSE, "password": "faux", "base_url": base })),
    )
    .await;
    assert_ne!(rep["authenticated"], true, "mauvais mot de passe : {rep}");
    let (statut, rep) = appeler(
        &app,
        "POST",
        &format!("/api/v1/streaming/{SERVICE}/auth"),
        Some(json!({ "email": ADRESSE, "password": MOT_DE_PASSE, "base_url": base })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{rep}");
    assert_eq!(rep["authenticated"], true, "{rep}");
    assert_eq!(rep["username"], "Charly");

    // 2. Les identifiants sont en base, SCELLÉS ; la clé est à part.
    let jetons = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(&format!("auth_tokens_{SERVICE}"))
        .unwrap()
        .expect("les identifiants doivent être gardés pour le redémarrage");
    assert!(jetons.contains("sealed_credentials"), "{jetons}");
    for clair in [MOT_DE_PASSE, ADRESSE, "s3cr3t", "charly", "password"] {
        assert!(
            !jetons.contains(clair),
            "« {clair} » se lit en base : {jetons}"
        );
    }
    assert_eq!(
        std::fs::read(&cle).unwrap().len(),
        32,
        "la clé vit dans son fichier"
    );

    // 3. Recherche, album, adresse de lecture — par les routes de tout service.
    let (statut, rep) = appeler(
        &app,
        "GET",
        &format!("/api/v1/streaming/{SERVICE}/search?q=jazz"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{rep}");
    let albums = rep["albums"].as_array().expect("albums");
    assert_eq!(albums.len(), 2, "{rep}");
    assert_eq!(albums[0]["title"], "Kind of Blue");
    assert_eq!(albums[1]["artist_name"], "Charles Mingus");
    assert_eq!(rep["tracks"].as_array().map(Vec::len), Some(4));

    let (statut, rep) = appeler(
        &app,
        "GET",
        &format!("/api/v1/streaming/{SERVICE}/albums/alb-2"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{rep}");
    assert_eq!(rep["title"], "Mingus Ah Um");
    assert_eq!(rep["track_count"], 2);

    let (statut, rep) = appeler(
        &app,
        "GET",
        &format!("/api/v1/streaming/{SERVICE}/tracks/trk-1/url"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{rep}");
    assert_eq!(rep["url"], format!("{base}/audio/trk-1.mp3"));
    assert_eq!(rep["mime_type"], "audio/mpeg");

    // 4. Un domaine hors liste est refusé, à travers le vrai hôte ; un domaine
    //    listé passe, avec la session que l'hôte garde.
    let (_, rep) = appeler(
        &app,
        "POST",
        "/api/v1/plugins/source-exemple/sonde",
        Some(json!({ "url": "https://evil.example/vol?c=1" })),
    )
    .await;
    assert_eq!(rep["error"], "domain_not_allowed", "{rep}");
    assert_eq!(rep["detail"], "evil.example");
    let (_, rep) = appeler(
        &app,
        "POST",
        "/api/v1/plugins/source-exemple/sonde",
        Some(json!({ "url": format!("{base}/catalogue?q=a") })),
    )
    .await;
    assert_eq!(rep["status"], 200, "{rep}");
    assert!(rep["headers"].get("set-cookie").is_none());

    // 5. Redémarrage : un greffon NEUF (aucune session en mémoire), inscrit
    //    dans un registre neuf, se reconnecte depuis les identifiants scellés.
    let hote = tune_server::plugins_host::AppStateHost::from_state(&state);
    let reseau = hote.reseau();
    let manifeste: tune_core::plugins::PluginManifest = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("source-exemple/manifest.json")).unwrap(),
    )
    .unwrap();
    let neuf = tune_plugin_runtime_wasm::WasmPlugin::load_with_host_http(
        &dir.path().join("source-exemple/main.wasm"),
        tune_plugin_runtime_wasm::Limits::default(),
        Arc::new(hote),
        manifeste.permissions.iter().cloned().collect(),
        &manifeste.id,
        &manifeste.http_allowed_domains,
    )
    .unwrap();
    let registre = Arc::new(tokio::sync::Mutex::new(
        tune_core::streaming::ServiceRegistry::new(),
    ));
    let coffre = tune_server::sources_wasm::coffre_du_serveur(":memory:");
    tune_server::sources_wasm::inscrire_la_source(
        &registre,
        &state.backend,
        &manifeste,
        Arc::new(tokio::sync::Mutex::new(neuf)),
        coffre,
        Some(reseau),
    )
    .await
    .unwrap();
    let svc = registre.lock().await.get(SERVICE).unwrap();
    let statut = svc.read().await.auth_status().await;
    assert!(statut.authenticated, "reconnecté au démarrage : {statut:?}");
    assert_eq!(statut.username.as_deref(), Some("Charly"));
    let url = svc.read().await.get_track_url("trk-4", None).await.unwrap();
    assert_eq!(url.url, format!("{base}/audio/trk-4.mp3"));

    // Un nom de service déjà pris est refusé.
    let refus = tune_server::sources_wasm::inscrire_la_source(
        &registre,
        &state.backend,
        &manifeste,
        state
            .wasm_plugins
            .get()
            .unwrap()
            .get("source-exemple")
            .unwrap()
            .plugin
            .clone(),
        tune_server::sources_wasm::coffre_du_serveur(":memory:"),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(
        refus,
        tune_server::sources_wasm::RefusDInscription::NomPris(SERVICE.to_string())
    );
}
