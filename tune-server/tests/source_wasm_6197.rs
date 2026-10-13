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
//!
//! Étape 2 (`une_source_wasm_couvre_le_trait_et_se_lit_vraiment`) : la
//! recherche paginée, les artistes, les playlists, les favoris (lecture et
//! écriture), les nouveautés ; la LECTURE réelle — l'adresse de flux sur un
//! CDN qui exige la session, que l'hôte y ajoute, une session expirée qui se
//! renouvelle seule ; l'écran (`GET /plugins/sources`, journal réseau) et les
//! cookies persistés scellés.
#![cfg(feature = "plugins-wasm")]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

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

/// La session valide est `jeton-<génération>` : faire avancer la génération
/// EXPIRE la session en cours, comme un service qui renouvelle ses jetons.
#[derive(Clone)]
struct Faux {
    base: String,
    generation: Arc<AtomicU32>,
    favoris: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Faux {
    fn jeton(&self) -> String {
        format!("session=jeton-{}", self.generation.load(Ordering::SeqCst))
    }
    fn connecte(&self, entetes: &HeaderMap) -> bool {
        entetes
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.split("; ").any(|morceau| morceau == self.jeton()))
    }
}

fn album_du_catalogue(base: &str, id: &str) -> Option<Value> {
    catalogue(base)
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["ref"] == id)
        .cloned()
}

/// Le faux service : son propre dialecte, une session par cookie.
async fn faux_service() -> Faux {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    let faux = Faux {
        base: base.clone(),
        generation: Arc::new(AtomicU32::new(42)),
        favoris: Arc::new(std::sync::Mutex::new(Vec::new())),
    };

    let b = base.clone();
    let f = faux.clone();
    let app = Router::new()
        .route("/login", {
            let f = f.clone();
            post(move |Json(corps): Json<Value>| async move {
                if corps["email"] == ADRESSE && corps["password"] == MOT_DE_PASSE {
                    (
                        StatusCode::OK,
                        [(
                            header::SET_COOKIE,
                            format!("{}; Path=/; HttpOnly", f.jeton()),
                        )],
                        Json(json!({ "abonne": "Charly", "offre": "Premium" })),
                    )
                        .into_response()
                } else {
                    StatusCode::UNAUTHORIZED.into_response()
                }
            })
        })
        .route("/catalogue", {
            let (b, f) = (b.clone(), f.clone());
            get(
                move |entetes: HeaderMap,
                      Query(q): Query<std::collections::HashMap<String, String>>| async move {
                    if !f.connecte(&entetes) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    let tout = catalogue(&b).as_array().unwrap().clone();
                    let debut: usize = q.get("debut").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let nombre: usize = q
                        .get("nombre")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(tout.len());
                    let page: Vec<Value> = tout.iter().skip(debut).take(nombre).cloned().collect();
                    Json(json!({ "resultats": page, "total": tout.len() })).into_response()
                },
            )
        })
        .route("/artistes/{id}", {
            let (b, f) = (b.clone(), f.clone());
            get(
                move |entetes: HeaderMap, Chemin(id): Chemin<String>| async move {
                    if !f.connecte(&entetes) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    if id != "art-miles" {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    Json(json!({
                        "ref": "art-miles", "nom": "Miles Davis", "photo": null,
                        "albums": [album_du_catalogue(&b, "alb-1")],
                    }))
                    .into_response()
                },
            )
        })
        .route("/listes/{id}", {
            let (b, f) = (b.clone(), f.clone());
            get(
                move |entetes: HeaderMap, Chemin(id): Chemin<String>| async move {
                    if !f.connecte(&entetes) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    if id != "lst-1" {
                        return StatusCode::NOT_FOUND.into_response();
                    }
                    let a1 = album_du_catalogue(&b, "alb-1").unwrap();
                    let a2 = album_du_catalogue(&b, "alb-2").unwrap();
                    let mut p1 = a1["pistes"][0].clone();
                    p1["album"] = a1;
                    let mut p2 = a2["pistes"][1].clone();
                    p2["album"] = a2;
                    Json(json!({
                        "ref": "lst-1", "nom": "Nuit blanche", "texte": "Deux pour la route",
                        "auteur": "Charly", "pistes": [p1, p2],
                    }))
                    .into_response()
                },
            )
        })
        .route("/favoris", {
            let (b, f) = (b.clone(), f.clone());
            get(move |entetes: HeaderMap| async move {
                if !f.connecte(&entetes) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                let albums: Vec<Value> = f
                    .favoris
                    .lock()
                    .unwrap()
                    .iter()
                    .filter_map(|id| album_du_catalogue(&b, id))
                    .collect();
                Json(json!({ "albums": albums, "artistes": [], "listes": [], "pistes": [] }))
                    .into_response()
            })
        })
        .route("/favoris/albums/{id}", {
            let f = f.clone();
            axum::routing::put({
                let f = f.clone();
                move |entetes: HeaderMap, Chemin(id): Chemin<String>| async move {
                    if !f.connecte(&entetes) {
                        return StatusCode::UNAUTHORIZED;
                    }
                    f.favoris.lock().unwrap().push(id);
                    StatusCode::NO_CONTENT
                }
            })
            .delete(
                move |entetes: HeaderMap, Chemin(id): Chemin<String>| async move {
                    if !f.connecte(&entetes) {
                        return StatusCode::UNAUTHORIZED;
                    }
                    f.favoris.lock().unwrap().retain(|a| *a != id);
                    StatusCode::NO_CONTENT
                },
            )
        })
        .route("/nouveautes", {
            let (b, f) = (b.clone(), f.clone());
            get(move |entetes: HeaderMap| async move {
                if !f.connecte(&entetes) {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                Json(json!([album_du_catalogue(&b, "alb-2")])).into_response()
            })
        })
        .route("/cdn/{fichier}", {
            let f = f.clone();
            get(
                move |entetes: HeaderMap, Chemin(_fichier): Chemin<String>| async move {
                    // Le CDN exige la session : sans le cookie, 403.
                    if !f.connecte(&entetes) {
                        return StatusCode::FORBIDDEN.into_response();
                    }
                    ([(header::CONTENT_TYPE, "audio/mpeg")], "ID3-faux-flux").into_response()
                },
            )
        })
        .route("/albums/{id}", {
            let (b, f) = (b.clone(), f.clone());
            get(
                move |entetes: HeaderMap, Chemin(id): Chemin<String>| async move {
                    if !f.connecte(&entetes) {
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
            let (b, f) = (b.clone(), f.clone());
            get(
                move |entetes: HeaderMap, Chemin(id): Chemin<String>| async move {
                    if !f.connecte(&entetes) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    // L'adresse « hors liste » sert au refus de l'étape 2.
                    let adresse = if id == "trk-hors-liste" {
                        "https://cdn.evil.example/vol.mp3".to_string()
                    } else {
                        format!("{b}/cdn/{id}.mp3")
                    };
                    Json(json!({ "adresse": adresse, "expire_a": null })).into_response()
                },
            )
        });
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    faux
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
    let faux = faux_service().await;
    let base = faux.base.clone();
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
    assert_eq!(rep["url"], format!("{base}/cdn/trk-1.mp3"));
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
    assert_eq!(url.url, format!("{base}/cdn/trk-4.mp3"));

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

    // Étape 2, sur le même serveur : les variables d'environnement du
    // processus ne se partagent pas entre deux tests qui tournent ensemble.
    etape_2(&app, &state, &faux).await;
}

/// Étape 2 de #6197 : la couverture du trait, la lecture réelle, l'écran et
/// le durcissement — par les routes que sert tout service.
async fn etape_2(app: &Router, state: &AppState, faux: &Faux) {
    let base = &faux.base;
    let route = |fin: &str| format!("/api/v1/streaming/{SERVICE}/{fin}");

    // a. Recherche PAGINÉE : le greffon exporte `search_page`, le service
    //    annonce son total.
    let (statut, rep) = appeler(app, "GET", &route("search?q=jazz&limit=1&offset=0"), None).await;
    assert_eq!(statut, StatusCode::OK, "{rep}");
    assert_eq!(rep["albums"].as_array().map(Vec::len), Some(1), "{rep}");
    assert_eq!(rep["albums"][0]["title"], "Kind of Blue");
    assert_eq!(rep["has_more"], true, "{rep}");
    assert_eq!(rep["totals"]["albums"], 2, "{rep}");
    let (_, rep) = appeler(app, "GET", &route("search?q=jazz&limit=1&offset=1"), None).await;
    assert_eq!(rep["albums"][0]["title"], "Mingus Ah Um", "{rep}");
    assert_eq!(rep["offset"], 1);
    assert_eq!(rep["has_more"], false, "{rep}");

    // b. Artistes.
    let (statut, rep) = appeler(app, "GET", &route("artists/art-miles"), None).await;
    assert_eq!(statut, StatusCode::OK, "{rep}");
    assert_eq!(rep["name"], "Miles Davis");
    let (_, rep) = appeler(app, "GET", &route("artists/art-miles/albums"), None).await;
    assert_eq!(rep[0]["title"], "Kind of Blue", "{rep}");
    let (_, rep) = appeler(app, "GET", &route("artists/art-miles/top-tracks"), None).await;
    assert_eq!(rep.as_array().map(Vec::len), Some(2), "{rep}");

    // c. Playlists.
    let (statut, rep) = appeler(app, "GET", &route("playlists/lst-1"), None).await;
    assert_eq!(statut, StatusCode::OK, "{rep}");
    assert_eq!(rep["name"], "Nuit blanche");
    assert_eq!(rep["track_count"], 2);
    let (_, rep) = appeler(app, "GET", &route("playlists/lst-1/tracks"), None).await;
    assert_eq!(rep[1]["title"], "Goodbye Pork Pie Hat", "{rep}");
    assert_eq!(rep[1]["album_title"], "Mingus Ah Um", "{rep}");

    // d. Favoris, dans les deux sens.
    let (statut, rep) = appeler(app, "POST", &route("favorites/albums/alb-2"), None).await;
    assert!(statut.is_success(), "{statut} {rep}");
    assert_eq!(*faux.favoris.lock().unwrap(), vec!["alb-2".to_string()]);
    let (_, rep) = appeler(app, "GET", &route("favorites/albums"), None).await;
    assert_eq!(rep["albums"][0]["title"], "Mingus Ah Um", "{rep}");
    let (statut, _) = appeler(app, "DELETE", &route("favorites/albums/alb-2"), None).await;
    assert!(statut.is_success());
    assert!(faux.favoris.lock().unwrap().is_empty());

    // e. Découverte.
    let (_, rep) = appeler(app, "GET", &route("new-releases"), None).await;
    assert_eq!(rep[0]["title"], "Mingus Ah Um", "{rep}");

    // f. Lecture réelle : le CDN exige la session. L'adresse que voit le
    //    téléchargeur de l'hôte porte le cookie ; le greffon ne l'a jamais vu,
    //    et la route ne le rend pas au client.
    let svc = state.services.lock().await.get(SERVICE).unwrap();
    let flux = svc.read().await.get_track_url("trk-2", None).await.unwrap();
    let client = reqwest::Client::new();
    assert_eq!(
        client.get(&flux.url).send().await.unwrap().status(),
        reqwest::StatusCode::FORBIDDEN,
        "sans la session, le CDN refuse"
    );
    let mut requete = client.get(&flux.url);
    for (nom, valeur) in &flux.headers {
        requete = requete.header(nom, valeur);
    }
    let lu = requete.send().await.unwrap();
    assert_eq!(lu.status(), reqwest::StatusCode::OK, "{:?}", flux.headers);
    assert_eq!(lu.text().await.unwrap(), "ID3-faux-flux");
    let (_, rep) = appeler(app, "GET", &route("tracks/trk-2/url"), None).await;
    assert_eq!(rep["url"], flux.url);
    assert!(
        !rep.to_string().contains("jeton-"),
        "le cookie de session ne sort pas du serveur : {rep}"
    );

    // g. Une adresse de lecture hors de la liste de domaines est refusée.
    let refus = svc
        .read()
        .await
        .get_track_url("trk-hors-liste", None)
        .await
        .unwrap_err()
        .to_string();
    assert!(refus.contains("domain_not_allowed"), "{refus}");

    // h. La session expire chez le service : la route se reconnecte seule
    //    (identifiants scellés) et redemande, une fois.
    faux.generation.fetch_add(1, Ordering::SeqCst);
    let (statut, rep) = appeler(app, "GET", &route("tracks/trk-3/url"), None).await;
    assert_eq!(statut, StatusCode::OK, "session renouvelée : {rep}");
    assert_eq!(rep["url"], format!("{base}/cdn/trk-3.mp3"));

    // i. L'écran : les champs de connexion et le journal réseau.
    let (statut, rep) = appeler(app, "GET", "/api/v1/plugins/sources", None).await;
    assert_eq!(statut, StatusCode::OK, "{rep}");
    let source = &rep["sources"][0];
    assert_eq!(source["plugin_id"], "source-exemple", "{rep}");
    assert_eq!(source["service"], SERVICE);
    assert_eq!(source["display_name"], "Exemple (greffon)");
    assert_eq!(source["registered"], true);
    assert_eq!(source["login_fields"][1]["type"], "password");
    let journal = rep["network"]["source-exemple"]
        .as_array()
        .expect("journal réseau");
    let ligne = |hote: &str| {
        journal
            .iter()
            .find(|l| l["host"] == hote)
            .cloned()
            .unwrap_or_else(|| panic!("{hote} absent du journal : {journal:?}"))
    };
    assert!(ligne("127.0.0.1")["requests"].as_u64().unwrap() > 5);
    assert!(ligne("evil.example")["refused"].as_u64().unwrap() >= 1);
    assert_eq!(ligne("evil.example")["last_error"], "domain_not_allowed");
    assert_eq!(ligne("cdn.evil.example")["refused"], 1);
    for secret in [MOT_DE_PASSE, "jeton-"] {
        assert!(!rep.to_string().contains(secret), "{secret} : {rep}");
    }

    // j. Les cookies sont persistés SCELLÉS ; un réseau neuf (redémarrage) les
    //    relit et la session repart sans nouvelle connexion.
    let brut = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(&tune_server::sources_wasm::cle_des_cookies(
            "source-exemple",
        ))
        .unwrap()
        .expect("cookies persistés");
    assert!(!brut.contains("jeton-"), "cookie lisible en base : {brut}");
    let neuf = tune_server::sources_wasm::ReseauDesGreffons::new();
    neuf.brancher_la_persistance(
        state.backend.clone(),
        tune_server::sources_wasm::coffre_du_serveur(":memory:"),
    );
    neuf.restaurer_les_cookies("source-exemple");
    let r = neuf
        .executer(
            "source-exemple",
            json!({ "url": format!("{base}/favoris") }),
        )
        .await
        .unwrap();
    assert_eq!(r["status"], 200, "session relue après redémarrage : {r}");
}
