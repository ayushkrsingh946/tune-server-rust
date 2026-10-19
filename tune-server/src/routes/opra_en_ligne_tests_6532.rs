//! #6532 (fil 2334) — la base OPRA et la recherche unifiée, par le ROUTEUR
//! MONTÉ : `GET /api/v1/eq/headphones/search` et `POST /api/v1/eq/opra/import`,
//! sur des catalogues à faux réseau (ni GitHub ni le miroir OPRA pendant les
//! essais).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::audio::autoeq_base::{CatalogueAutoEq, TelechargeurAutoEq};
use tune_core::audio::opra_base::CatalogueOpra;

const INDEX: &str = "# Index\n\
- [Sennheiser HD 650](./oratory1990/over-ear/Sennheiser%20HD%20650) by oratory1990\n";

/// Lignes réelles d'OPRA (CC BY-SA 4.0, voir le fichier de licence voisin).
const OPRA: &str = include_str!("../../../tune-core/tests/fixtures/opra/database_v1_extrait.jsonl");

/// Une courbe fabriquée pour l'essai (pas une donnée OPRA) : un passe-bande,
/// que l'égaliseur de Tune ne sait pas construire.
const COURBE_PASSE_BANDE: &str = r#"{"type":"eq","id":"sennheiser:hd650::essai_passe_bande","data":{"author":"essai","type":"parametric_eq","parameters":{"gain_db":-1,"bands":[{"type":"peak_dip","frequency":100,"gain_db":1,"q":1},{"type":"band_pass","frequency":1000,"q":1}]},"product_id":"sennheiser::hd650"}}"#;

#[derive(Default)]
struct FauxReseau {
    coupe_autoeq: AtomicBool,
    coupe_opra: AtomicBool,
    appels: AtomicUsize,
}

#[async_trait]
impl TelechargeurAutoEq for FauxReseau {
    async fn telecharger(&self, url: &str, _taille_max: usize) -> Result<String, String> {
        self.appels.fetch_add(1, Ordering::SeqCst);
        if url.ends_with("/INDEX.md") {
            if self.coupe_autoeq.load(Ordering::SeqCst) {
                return Err("réseau injoignable".into());
            }
            return Ok(INDEX.into());
        }
        if self.coupe_opra.load(Ordering::SeqCst) {
            return Err("réseau injoignable".into());
        }
        Ok(format!("{OPRA}\n{COURBE_PASSE_BANDE}\n"))
    }
}

fn etat(reseau: &Arc<FauxReseau>) -> crate::state::AppState {
    let mut state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.autoeq = Arc::new(CatalogueAutoEq::new(
        "https://exemple.test/results",
        reseau.clone(),
        None,
    ));
    state.opra = Arc::new(CatalogueOpra::new(
        "https://exemple.test/database_v1.jsonl",
        reseau.clone(),
        None,
    ));
    state
}

fn app(reseau: &Arc<FauxReseau>) -> axum::Router {
    crate::routes::router(etat(reseau))
}

/// Premium et greffon égaliseur installé, comme `tests/autoeq_import_route.rs`.
async fn app_premium(reseau: &Arc<FauxReseau>) -> axum::Router {
    let state = etat(reseau);
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("plugin_equalizer_installed", "true")
        .unwrap();
    state.license.set_account_premium(true, None).await;
    crate::routes::router(state)
}

async fn appel(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn chercher(q: &str) -> Request<Body> {
    Request::get(format!(
        "/api/v1/eq/headphones/search?q={}",
        urlencoding::encode(q)
    ))
    .body(Body::empty())
    .unwrap()
}

fn importer(id: &str) -> Request<Body> {
    Request::post("/api/v1/eq/opra/import")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "id": id }).to_string()))
        .unwrap()
}

async fn nombre_de_presets(app: &axum::Router) -> usize {
    let (code, v) = appel(
        app,
        Request::get("/api/v1/eq/presets")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    v["presets"].as_array().map_or(0, Vec::len)
}

#[tokio::test]
async fn la_recherche_rend_les_deux_sources_dans_une_liste_avec_l_attribution() {
    let reseau = Arc::new(FauxReseau::default());
    let (code, v) = appel(&app(&reseau), chercher("hd650")).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    // 1 AutoEq + 3 OPRA réelles + la courbe d'essai.
    assert_eq!(v["total"], 5, "{v:#}");
    let resultats = v["results"].as_array().unwrap();
    let opra = resultats
        .iter()
        .find(|r| r["key"] == "sennheiser:hd650::oratory1990_harman_target")
        .expect("courbe OPRA listée");
    assert_eq!(
        *opra,
        json!({
            "source": "opra",
            "key": "sennheiser:hd650::oratory1990_harman_target",
            "name": "Sennheiser HD650",
            "vendor": "Sennheiser",
            "model": "HD650",
            "author": "oratory1990",
            "details": "Harman Target",
            "form": "over_the_ear",
            "link": "https://www.dropbox.com/s/qs4hs92tvfbwr04/Sennheiser%20HD650.pdf?dl=0",
        })
    );
    let autoeq = resultats.iter().find(|r| r["source"] == "autoeq").unwrap();
    assert_eq!(autoeq["key"], "oratory1990/over-ear/Sennheiser HD 650");
    assert_eq!(autoeq["author"], "oratory1990");
    assert_eq!(v["unavailable_sources"], json!([]));
    assert_eq!(
        v["attribution"]["opra"]["repository"],
        "https://github.com/opra-project/OPRA"
    );
    assert_eq!(v["attribution"]["opra"]["license"], "CC BY-SA 4.0");
}

#[tokio::test]
async fn une_base_injoignable_est_citee_et_l_autre_repond() {
    let reseau = Arc::new(FauxReseau::default());
    reseau.coupe_opra.store(true, Ordering::SeqCst);
    let (code, v) = appel(&app(&reseau), chercher("hd 650")).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 1);
    assert_eq!(v["unavailable_sources"], json!(["opra"]));
}

/// Sans réseau ni copie pour les DEUX bases : 503, jamais une liste vide.
#[tokio::test]
async fn sans_aucune_base_la_recherche_rend_503() {
    let reseau = Arc::new(FauxReseau::default());
    reseau.coupe_autoeq.store(true, Ordering::SeqCst);
    reseau.coupe_opra.store(true, Ordering::SeqCst);
    let (code, v) = appel(&app(&reseau), chercher("hd 650")).await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(v["error"], "headphone_databases_unreachable");
}

#[tokio::test]
async fn une_requete_vide_ne_touche_pas_au_reseau() {
    let reseau = Arc::new(FauxReseau::default());
    let (code, v) = appel(&app(&reseau), chercher("  ")).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(v["total"], 0);
    assert_eq!(v["attribution"]["opra"]["name"], "OPRA");
    assert_eq!(reseau.appels.load(Ordering::SeqCst), 0);
}

/// Mêmes portes que l'import d'un fichier (Premium, greffon égaliseur),
/// AVANT de lire la base.
#[tokio::test]
async fn l_import_opra_passe_les_portes_avant_tout_telechargement() {
    let reseau = Arc::new(FauxReseau::default());
    let (code, v) = appel(
        &app(&reseau),
        importer("sennheiser:hd650::oratory1990_harman_target"),
    )
    .await;
    assert!(
        code == StatusCode::PAYMENT_REQUIRED || code == StatusCode::CONFLICT,
        "{code} {v}"
    );
    assert_eq!(reseau.appels.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn l_import_opra_enregistre_la_courbe_avec_son_credit() {
    let reseau = Arc::new(FauxReseau::default());
    let app = app_premium(&reseau).await;
    let (code, v) = appel(
        &app,
        importer("sennheiser:hd650::oratory1990_harman_target"),
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{v:#}");
    assert_eq!(v["band_count"], 10);
    assert_eq!(v["preamp_db"], -9.3);
    assert_eq!(v["preamp_applied"], false);
    let preset = &v["preset"];
    assert_eq!(
        preset["name"],
        "Sennheiser HD650 (oratory1990 · Harman Target)"
    );
    assert_eq!(preset["source"], "opra");
    assert_eq!(
        preset["bands"][2],
        json!({"freq": 105.0, "gain": 5.5, "q": 0.71, "type": "low_shelf"})
    );
    let credit = json!({
        "author": "oratory1990",
        "details": "Harman Target",
        "link": "https://www.dropbox.com/s/qs4hs92tvfbwr04/Sennheiser%20HD650.pdf?dl=0",
        "database": "OPRA",
        "repository": "https://github.com/opra-project/OPRA",
        "license": "CC BY-SA 4.0",
        "license_url": "https://creativecommons.org/licenses/by-sa/4.0/",
    });
    assert_eq!(
        preset["credit"], credit,
        "le crédit voyage avec le préréglage"
    );
    assert_eq!(v["credit"], credit);
    assert_eq!(v["approximated_shelves"], json!([]));
    // Une seule lecture du réseau : la base ; rien d'autre n'est téléchargé.
    assert_eq!(reseau.appels.load(Ordering::SeqCst), 1);
    assert_eq!(nombre_de_presets(&app).await, 1);
}

#[tokio::test]
async fn les_plateaux_au_q_non_standard_sont_nommes_dans_la_reponse() {
    let reseau = Arc::new(FauxReseau::default());
    let app = app_premium(&reseau).await;
    let (code, v) = appel(&app, importer("fiio:ft1::oratory1990_harman_target")).await;
    assert_eq!(code, StatusCode::CREATED, "{v:#}");
    assert_eq!(
        v["approximated_shelves"],
        json!([
            {"band": 1, "type": "low_shelf", "requested_q": 0.48},
            {"band": 8, "type": "high_shelf", "requested_q": 0.4},
        ])
    );
}

/// Un filtre que l'égaliseur ne sait pas construire refuse la courbe
/// ENTIÈRE : 422, et rien n'est enregistré.
#[tokio::test]
async fn un_filtre_non_pris_en_charge_rend_422_et_n_enregistre_rien() {
    let reseau = Arc::new(FauxReseau::default());
    let app = app_premium(&reseau).await;
    let avant = nombre_de_presets(&app).await;
    let (code, v) = appel(&app, importer("sennheiser:hd650::essai_passe_bande")).await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"], "opra_filter_unsupported");
    assert!(v["message"].as_str().unwrap().contains("band_pass"), "{v}");
    assert_eq!(nombre_de_presets(&app).await, avant);
}

#[tokio::test]
async fn une_courbe_inconnue_rend_404_et_une_base_injoignable_503() {
    let reseau = Arc::new(FauxReseau::default());
    let app = app_premium(&reseau).await;
    let (code, v) = appel(&app, importer("../../etc/passwd")).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["error"], "opra_curve_unknown");

    let hors_ligne = Arc::new(FauxReseau::default());
    hors_ligne.coupe_opra.store(true, Ordering::SeqCst);
    let app = app_premium(&hors_ligne).await;
    let (code, v) = appel(
        &app,
        importer("sennheiser:hd650::oratory1990_harman_target"),
    )
    .await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(v["error"], "opra_unreachable");
}
