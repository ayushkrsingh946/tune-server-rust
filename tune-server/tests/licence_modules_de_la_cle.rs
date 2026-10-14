//! Une clé de licence porte ses modules payants (chantier « mise à jour et
//! licence du binaire composite », étape 3 ; décisions du 14/10/2026).
//!
//! Le site renvoie `modules` dans le verdict de validation d'une clé. Un
//! appareil sans console ni compte lié s'active par
//! `POST /api/v1/cloud/license/activate {"license_key": …}` et doit :
//! - recevoir `status: "activated"` dès que la clé est confirmée, même quand
//!   elle n'est pas Premium (clé « module seul ») ;
//! - posséder le module, sans compte lié, et le garder quand le compte relu ne
//!   l'a pas (`set_modules` du compte remplace SA liste, pas celle de la clé) ;
//! - le garder hors ligne sans limite (D5), y compris après un redémarrage ;
//! - le perdre sur une expiration autoritaire ou une désactivation.
//!
//! Toutes les vérifications passent par les VRAIES routes montées, contre un
//! faux site local (`mozaik_base_url`), jamais mozaiklabs.fr.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const CLE: &str = "TUNE-MODU-LESE-ULCL-E001";
const AUTRE_CLE: &str = "TUNE-AUTR-ECLE-0000-0002";
const ACTIVATE: &str = "/api/v1/cloud/license/activate";
const VALIDATE: &str = "/api/v1/cloud/license/validate";
const DEACTIVATE: &str = "/api/v1/cloud/license/deactivate";
const STATUS: &str = "/api/v1/cloud/license/status";
const MODULE: &str = "module-a";

type Corps = Arc<Mutex<Value>>;

/// Le verdict d'une clé « module seul » : palier Free, un module, sans échéance.
fn cle_module_seul() -> Value {
    json!({
        "license_valid": true,
        "license_tier": "free",
        "license_expires_at": null,
        "plan": "module",
        "modules": [MODULE],
    })
}

/// Faux site : un vrai serveur HTTP (`axum::serve`), corps échangeable.
async fn faux_site(initial: Value) -> (String, Corps) {
    let corps: Corps = Arc::new(Mutex::new(initial));
    let partage = corps.clone();
    let app = axum::Router::new().route(
        "/api/v1/license/validate",
        axum::routing::post(move || {
            let partage = partage.clone();
            async move { axum::Json(partage.lock().unwrap().clone()) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), corps)
}

/// Une adresse où personne n'écoute : le site est injoignable.
async fn site_injoignable() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

struct Serveur {
    app: axum::Router,
    state: AppState,
    db_path: std::path::PathBuf,
}

fn demarrer(db_path: &std::path::Path, base_url: &str) -> Serveur {
    let state = AppState::new(db_path.to_str().unwrap(), 0, Default::default()).unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set("mozaik_base_url", base_url)
        .unwrap();
    Serveur {
        app: tune_server::routes::router(state.clone()),
        state,
        db_path: db_path.to_path_buf(),
    }
}

fn serveur_froid(base_url: &str) -> (Serveur, tune_core::test_scratch::ScratchDir) {
    let base = tune_core::test_scratch::scratch_dir("tune-modules-cle");
    let db_path = base.join("library.db");
    (demarrer(&db_path, base_url), base)
}

async fn appeler(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn poster(app: &axum::Router, route: &str, corps: Value) -> (StatusCode, Value) {
    let req = Request::post(route)
        .header("content-type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    appeler(app, req).await
}

async fn statut(app: &axum::Router) -> Value {
    let (code, body) = appeler(app, Request::get(STATUS).body(Body::empty()).unwrap()).await;
    assert_eq!(code, StatusCode::OK, "statut : {body}");
    body
}

/// Le cœur de la demande du partenaire : la commande d'activation scriptée
/// répond `activated` pour une clé confirmée qui n'est PAS Premium, et le
/// module est possédé tout de suite, sans compte lié.
#[tokio::test]
async fn une_cle_module_seul_s_active_et_porte_son_module() {
    let (site, _) = faux_site(cle_module_seul()).await;
    let (srv, _garde) = serveur_froid(&site);

    let (code, body) = poster(&srv.app, ACTIVATE, json!({ "license_key": CLE })).await;

    assert_eq!(code, StatusCode::OK, "activation : {body}");
    assert_eq!(
        body["status"], "activated",
        "une clé confirmée non Premium s'annonce « en attente » : {body}"
    );
    assert_eq!(body["tier"], "free", "activation : {body}");
    assert_eq!(body["modules"], json!([MODULE]), "activation : {body}");

    let s = statut(&srv.app).await;
    assert_eq!(s["modules"], json!([MODULE]), "statut : {s}");
    assert_eq!(s["module_sources"]["key"], json!([MODULE]), "statut : {s}");
    assert_eq!(s["module_sources"]["account"], json!([]), "statut : {s}");
    assert_eq!(
        s["tier"], "free",
        "une clé module ne donne pas Premium : {s}"
    );

    // Le chemin que lit la découverte des sorties (`license.modules()`).
    assert!(srv.state.license.has_module(MODULE).await);
    assert_eq!(srv.state.license.modules().await, vec![MODULE.to_string()]);
}

/// Une clé Premium qui porte aussi le module : les deux à la fois.
#[tokio::test]
async fn une_cle_premium_avec_module_porte_les_deux() {
    let (site, _) = faux_site(json!({
        "license_valid": true,
        "license_tier": "premium",
        "license_expires_at": "2099-01-01T00:00:00Z",
        "modules": [MODULE],
    }))
    .await;
    let (srv, _garde) = serveur_froid(&site);

    let (_, body) = poster(&srv.app, ACTIVATE, json!({ "license_key": CLE })).await;

    assert_eq!(body["status"], "activated", "activation : {body}");
    assert_eq!(body["tier"], "premium", "activation : {body}");
    assert_eq!(body["modules"], json!([MODULE]), "activation : {body}");
}

/// La relecture du compte (`set_modules`, battement d'une heure ou liaison)
/// remplace la liste DU COMPTE. Elle ne doit pas effacer le module de la clé.
#[tokio::test]
async fn la_relecture_d_un_compte_sans_module_garde_celui_de_la_cle() {
    let (site, _) = faux_site(cle_module_seul()).await;
    let (srv, _garde) = serveur_froid(&site);
    poster(&srv.app, ACTIVATE, json!({ "license_key": CLE })).await;

    // Un compte lié qui ne possède aucun module, puis son départ.
    srv.state.license.set_modules(Vec::new()).await;
    let s = statut(&srv.app).await;
    assert_eq!(
        s["modules"],
        json!([MODULE]),
        "la relecture du compte a effacé le module de la clé : {s}"
    );
    srv.state.license.clear_account_premium().await;
    assert!(
        srv.state.license.has_module(MODULE).await,
        "la déconnexion du compte a emporté le module de la clé"
    );

    // Et les deux sources s'unissent, sans doublon.
    srv.state
        .license
        .set_modules(vec![MODULE.to_string(), "autre".to_string()])
        .await;
    let s = statut(&srv.app).await;
    assert_eq!(s["modules"], json!(["autre", MODULE]), "statut : {s}");
    assert_eq!(s["module_sources"]["account"], json!([MODULE, "autre"]));
    assert_eq!(s["module_sources"]["key"], json!([MODULE]));
}

/// D5 : le module de la clé reste valable hors ligne SANS limite. Le serveur
/// redémarre, la dernière validation date de 400 jours, le site est
/// injoignable : le module est toujours là. Le témoin : une clé Premium
/// dans la même situation est retombée en Free (grâce de 14 jours).
#[tokio::test]
async fn le_module_de_la_cle_reste_hors_ligne_sans_limite() {
    let (site, _) = faux_site(json!({
        "license_valid": true,
        "license_tier": "premium",
        "license_expires_at": null,
        "modules": [MODULE],
    }))
    .await;
    let (srv, _garde) = serveur_froid(&site);
    let (_, body) = poster(&srv.app, ACTIVATE, json!({ "license_key": CLE })).await;
    assert_eq!(body["tier"], "premium", "activation : {body}");

    // 400 jours sans aucune validation réussie, puis redémarrage hors ligne.
    let il_y_a_400_jours = (chrono::Utc::now() - chrono::Duration::days(400))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    SettingsRepo::with_backend(srv.state.backend.clone())
        .set("license_last_validated", &il_y_a_400_jours)
        .unwrap();
    let db_path = srv.db_path.clone();
    drop(srv);
    let hors_ligne = site_injoignable().await;
    let srv = demarrer(&db_path, &hors_ligne);

    // La revalidation à la demande échoue (site injoignable) : rien ne bouge.
    let (_, v) = poster(&srv.app, VALIDATE, json!({})).await;
    assert_eq!(v["cached"], true, "validation hors ligne : {v}");

    let s = statut(&srv.app).await;
    assert_eq!(
        s["tier"], "free",
        "témoin : la grâce de 14 jours de la clé Premium n'a pas joué : {s}"
    );
    assert_eq!(
        s["modules"],
        json!([MODULE]),
        "le module de la clé a expiré hors ligne : {s}"
    );
    assert!(srv.state.license.has_module(MODULE).await);
}

/// Une expiration autoritaire (`license_valid:false` avec une échéance passée)
/// retire le module de la clé.
#[tokio::test]
async fn une_expiration_autoritaire_retire_le_module() {
    let (site, corps) = faux_site(cle_module_seul()).await;
    let (srv, _garde) = serveur_froid(&site);
    poster(&srv.app, ACTIVATE, json!({ "license_key": CLE })).await;
    assert!(srv.state.license.has_module(MODULE).await);

    *corps.lock().unwrap() = json!({
        "license_valid": false,
        "license_tier": "free",
        "license_expires_at": "2020-01-01T00:00:00Z",
    });
    poster(&srv.app, VALIDATE, json!({})).await;

    let s = statut(&srv.app).await;
    assert_eq!(
        s["modules"],
        json!([]),
        "module gardé après expiration : {s}"
    );
    assert!(!srv.state.license.has_module(MODULE).await);
}

/// Un refus nu (re-liaison, hoquet du site) n'est pas une révocation : le
/// module reste.
#[tokio::test]
async fn un_refus_transitoire_garde_le_module() {
    let (site, corps) = faux_site(cle_module_seul()).await;
    let (srv, _garde) = serveur_froid(&site);
    poster(&srv.app, ACTIVATE, json!({ "license_key": CLE })).await;

    *corps.lock().unwrap() = json!({ "license_valid": false, "license_tier": "free" });
    poster(&srv.app, VALIDATE, json!({})).await;

    assert_eq!(statut(&srv.app).await["modules"], json!([MODULE]));
}

/// Un site ancien ne renvoie pas le champ : son absence ne veut pas dire
/// « aucun module », on ne touche à rien. `[]` explicite, en revanche, vide.
#[tokio::test]
async fn un_site_sans_le_champ_ne_touche_pas_aux_modules_de_la_cle() {
    let (site, corps) = faux_site(cle_module_seul()).await;
    let (srv, _garde) = serveur_froid(&site);
    poster(&srv.app, ACTIVATE, json!({ "license_key": CLE })).await;

    *corps.lock().unwrap() = json!({
        "license_valid": true,
        "license_tier": "free",
        "license_expires_at": null,
    });
    let (_, v) = poster(&srv.app, VALIDATE, json!({})).await;
    assert_eq!(v["status"], "validated", "validation : {v}");
    assert_eq!(
        statut(&srv.app).await["modules"],
        json!([MODULE]),
        "un site sans le champ a effacé le module"
    );

    // Le site dit maintenant explicitement « aucun module » (retrait).
    *corps.lock().unwrap() = json!({
        "license_valid": true,
        "license_tier": "free",
        "license_expires_at": null,
        "modules": [],
    });
    poster(&srv.app, VALIDATE, json!({})).await;
    assert_eq!(statut(&srv.app).await["modules"], json!([]));
}

/// Une clé refusée reste en attente et ne porte rien.
#[tokio::test]
async fn une_cle_refusee_reste_en_attente_sans_module() {
    let (site, _) = faux_site(json!({ "license_valid": false, "license_tier": "free" })).await;
    let (srv, _garde) = serveur_froid(&site);

    let (_, body) = poster(&srv.app, ACTIVATE, json!({ "license_key": CLE })).await;

    assert_eq!(body["status"], "pending", "activation : {body}");
    assert_eq!(body["modules"], json!([]), "activation : {body}");
    assert!(!srv.state.license.has_module(MODULE).await);
}

/// Site injoignable au moment de l'activation : en attente, rien d'accordé.
#[tokio::test]
async fn une_activation_hors_ligne_reste_en_attente() {
    let hors_ligne = site_injoignable().await;
    let (srv, _garde) = serveur_froid(&hors_ligne);

    let (_, body) = poster(&srv.app, ACTIVATE, json!({ "license_key": CLE })).await;

    assert_eq!(body["status"], "pending", "activation : {body}");
    assert_eq!(body["modules"], json!([]), "activation : {body}");
}

/// La commande d'activation se rejoue sans risque : la même clé, rejouée
/// pendant une coupure, garde son module. Une AUTRE clé n'en hérite pas.
#[tokio::test]
async fn rejouer_la_meme_cle_garde_le_module_une_autre_cle_non() {
    let (site, corps) = faux_site(cle_module_seul()).await;
    let (srv, _garde) = serveur_froid(&site);
    poster(&srv.app, ACTIVATE, json!({ "license_key": CLE })).await;

    // Coupure : le site ne se prononce plus (corps sans verdict).
    *corps.lock().unwrap() = json!({ "ok": true });
    let (_, body) = poster(&srv.app, ACTIVATE, json!({ "license_key": CLE })).await;
    assert_eq!(body["status"], "pending", "activation rejouée : {body}");
    assert_eq!(
        body["modules"],
        json!([MODULE]),
        "rejouer la même clé a perdu son module : {body}"
    );

    let (_, body) = poster(&srv.app, ACTIVATE, json!({ "license_key": AUTRE_CLE })).await;
    assert_eq!(
        body["modules"],
        json!([]),
        "une autre clé hérite du module de la précédente : {body}"
    );
}

/// La désactivation emporte le module de la clé, pas celui du compte.
#[tokio::test]
async fn la_desactivation_retire_le_module_de_la_cle() {
    let (site, _) = faux_site(cle_module_seul()).await;
    let (srv, _garde) = serveur_froid(&site);
    poster(&srv.app, ACTIVATE, json!({ "license_key": CLE })).await;
    srv.state
        .license
        .set_modules(vec!["autre".to_string()])
        .await;

    let (code, _) = poster(&srv.app, DEACTIVATE, json!({})).await;
    assert_eq!(code, StatusCode::OK);

    let s = statut(&srv.app).await;
    assert_eq!(s["modules"], json!(["autre"]), "statut : {s}");
    assert_eq!(s["module_sources"]["key"], json!([]), "statut : {s}");

    // Et rien ne revient au redémarrage.
    let db_path = srv.db_path.clone();
    drop(srv);
    let srv = demarrer(&db_path, &site);
    assert!(!srv.state.license.has_module(MODULE).await);
}
