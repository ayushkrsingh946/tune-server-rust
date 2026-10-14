//! L'aperçu d'un instantané cloud ne restaure rien (tune-web-client#902).
//!
//! L'assistant de première installation montre, AVANT le geste
//! « Restaurer », la date et le contenu résumé de la dernière sauvegarde du
//! compte. Le site ne connaît que la taille et la date de dépôt : le résumé
//! exige d'ouvrir l'instantané sur CE serveur, d'où
//! `POST /system/config-backup/cloud/preview`.
//!
//! Ce que ce fichier tient :
//! - avec le bon secret, la route rend le résumé (date écrite dans
//!   l'instantané, zones, préréglages d'égaliseur, jeton Discogs par son nom) ;
//! - elle n'écrit RIEN : ni réglage, ni zone, ni la clé de l'instantané
//!   (pas d'adoption, à la différence de `restore`) ;
//! - sans secret, ou avec un mauvais, elle refuse avec le code que l'écran
//!   sait lire ; sans Premium, elle refuse comme ses sœurs ;
//! - la valeur du jeton Discogs n'apparaît jamais dans la réponse ;
//! - la clé reste sur l'appareil : le site simulé enregistre chaque requête
//!   reçue (chemin, en-têtes, corps), et ni la phrase de passe ni la clé
//!   n'y figurent. L'aperçu ne fait qu'un GET de l'instantané opaque.
//!
//! Le site est simulé : aucun octet ne part vers mozaiklabs.fr.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::Path as AxumPath;
use axum::extract::State as AxumState;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::cloud::sauvegarde_config as sc;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const PHRASE: &str = "une phrase de passe longue";
const ID: i64 = 41;
const APERCU: &str = "/api/v1/system/config-backup/cloud/preview";

/// L'ANCIENNE machine : une zone, un préréglage d'égaliseur, un jeton
/// Discogs. Rend l'instantané chiffré et son `key_id`.
fn instantane_de_l_ancienne_machine() -> (String, String) {
    let source = AppState::new(":memory:", 0, Default::default()).unwrap();
    let s = SettingsRepo::with_backend(source.backend.clone());
    s.set("theme", "sombre").unwrap();
    s.set("eq_presets", r#"[{"name":"HE R10P","bands":31}]"#)
        .unwrap();
    s.set("discogs_token", "VOYAGE-discogs").unwrap();
    source
        .backend
        .execute(
            "INSERT INTO zones (name, output_type, output_device_id, volume) \
             VALUES ('Salon', 'local', 'local:DAC', 42)",
            &[],
        )
        .unwrap();
    sc::creer_la_cle(&s, PHRASE).unwrap();
    let cle = sc::cle_locale(&s).unwrap().unwrap();
    let mut contenu = sc::construire(&source.backend).unwrap();
    sc::horodater(
        &mut contenu,
        chrono::DateTime::parse_from_rfc3339("2026-10-12T08:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    );
    (sc::chiffrer(&cle, &contenu).unwrap(), cle.key_id.clone())
}

#[derive(Clone)]
struct Site {
    blob: String,
    key_id: String,
    /// Chaque requête reçue par le site : méthode, chemin, en-têtes, corps.
    vus: Arc<Mutex<Vec<String>>>,
}

impl Site {
    fn new(blob: String, key_id: String) -> Self {
        Self {
            blob,
            key_id,
            vus: Arc::default(),
        }
    }
}

/// Note toute requête qui arrive au site, quelle que soit la route.
async fn noter(
    AxumState(site): AxumState<Site>,
    req: Request<Body>,
    suite: axum::middleware::Next,
) -> Response {
    let (parts, corps) = req.into_parts();
    let octets = axum::body::to_bytes(corps, 16 * 1024 * 1024)
        .await
        .unwrap_or_default();
    site.vus.lock().unwrap().push(format!(
        "{} {} {:?} {}",
        parts.method,
        parts.uri,
        parts.headers,
        String::from_utf8_lossy(&octets)
    ));
    suite
        .run(Request::from_parts(parts, Body::from(octets)))
        .await
}

async fn un_instantane(AxumState(site): AxumState<Site>, AxumPath(id): AxumPath<i64>) -> Response {
    if id != ID {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error": "not_found"})),
        )
            .into_response();
    }
    axum::Json(json!({
        "backup": {
            "id": ID,
            "server_id": "ancienne-machine",
            "server_label": "Windows",
            "key_id": site.key_id,
            "format_version": 1,
            "size_bytes": site.blob.len(),
            "created_at": "2026-10-12T08:31:00Z",
            "payload": site.blob,
        }
    }))
    .into_response()
}

use axum::response::{IntoResponse, Response};

async fn site_simule(site: Site) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/api/v1/config-backups/{id}", get(un_instantane))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(site.clone(), noter))
        .with_state(site);
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let tache = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), tache)
}

/// La machine NEUVE, reliée au compte, Premium ou non.
async fn machine_neuve(base: &str, premium: bool) -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("mozaik_base_url", base).unwrap();
    s.set("mozaik_access_token", "jeton-de-test").unwrap();
    s.set("server_id", "machine-neuve").unwrap();
    if premium {
        state.license.set_account_premium(true, None).await;
    }
    state
}

async fn apercu(state: &AppState, corps: Value) -> (StatusCode, Value) {
    let app: Router = tune_server::routes::router(state.clone());
    let r = app
        .oneshot(
            Request::post(APERCU)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn nombre_de_zones(state: &AppState) -> i64 {
    state
        .backend
        .query_many("SELECT COUNT(*) FROM zones", &[])
        .unwrap()[0][0]
        .as_i64()
        .unwrap()
}

#[tokio::test]
async fn l_apercu_resume_l_instantane_sans_rien_ecrire() {
    let (blob, key_id) = instantane_de_l_ancienne_machine();
    let site = Site::new(blob, key_id);
    let (base, _tache) = site_simule(site.clone()).await;
    let state = machine_neuve(&base, true).await;
    let s = SettingsRepo::with_backend(state.backend.clone());
    let zones_avant = nombre_de_zones(&state);

    let (statut, corps) = apercu(&state, json!({"id": ID, "secret": PHRASE})).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let r = &corps["summary"];
    assert_eq!(r["created_at"], json!("2026-10-12T08:30:00Z"));
    assert_eq!(r["zones"], json!(1));
    assert_eq!(r["zone_names"], json!(["Salon"]));
    assert_eq!(r["eq_presets"], json!(1));
    assert_eq!(r["service_tokens"], json!(["discogs_token"]));
    assert!(r["settings"].as_u64().unwrap() >= 1);
    assert!(
        !corps.to_string().contains("VOYAGE-discogs"),
        "l'aperçu rend la valeur du jeton Discogs"
    );

    // RIEN n'est écrit : ni réglage, ni zone, ni la clé de l'instantané.
    assert_eq!(
        s.get("theme").unwrap(),
        None,
        "l'aperçu a restauré un réglage"
    );
    assert_eq!(
        s.get("discogs_token").unwrap(),
        None,
        "l'aperçu a restauré le jeton"
    );
    assert_eq!(
        nombre_de_zones(&state),
        zones_avant,
        "l'aperçu a créé une zone"
    );
    assert!(
        sc::cle_locale(&s).unwrap().is_none(),
        "l'aperçu a adopté la clé de l'instantané"
    );

    // La clé reste sur l'appareil : le site n'a reçu qu'un GET de
    // l'instantané, sans la phrase de passe ni rien qui en dérive.
    let vus = site.vus.lock().unwrap().clone();
    assert!(!vus.is_empty(), "le site simulé n'a rien reçu");
    for v in &vus {
        assert!(
            v.starts_with("GET /api/v1/config-backups/"),
            "requête inattendue au site : {v}"
        );
        assert!(
            !v.contains(PHRASE),
            "la phrase de passe est partie au site : {v}"
        );
    }
}

#[tokio::test]
async fn l_apercu_refuse_sans_le_bon_secret() {
    let (blob, key_id) = instantane_de_l_ancienne_machine();
    let (base, _tache) = site_simule(Site::new(blob, key_id)).await;
    let state = machine_neuve(&base, true).await;

    let (statut, corps) = apercu(&state, json!({"id": ID})).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert_eq!(corps["error"], json!("secret_required"));

    let (statut, corps) = apercu(&state, json!({"id": ID, "secret": "pas la bonne phrase"})).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert_eq!(corps["error"], json!("wrong_secret"));

    let (statut, corps) = apercu(&state, json!({"id": 999, "secret": PHRASE})).await;
    assert_eq!(statut, StatusCode::NOT_FOUND);
    assert_eq!(corps["error"], json!("backup_not_found"));
}

#[tokio::test]
async fn l_apercu_est_reserve_au_premium() {
    let (blob, key_id) = instantane_de_l_ancienne_machine();
    let (base, _tache) = site_simule(Site::new(blob, key_id)).await;
    let state = machine_neuve(&base, false).await;

    let (statut, _) = apercu(&state, json!({"id": ID, "secret": PHRASE})).await;
    assert!(
        statut.is_client_error() && statut != StatusCode::NOT_FOUND,
        "sans Premium, l'aperçu doit être refusé, pas {statut}"
    );
}
