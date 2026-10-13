//! Témoins de #6197 côté hôte : la boîte à cookies cloisonnée et la requête
//! HTTP d'un greffon, contre un vrai serveur HTTP local.
use super::*;
use axum::Router;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;

#[test]
fn la_boite_a_cookies_est_cloisonnee_par_greffon_et_par_hote() {
    let mut b = BoiteACookies::default();
    b.absorber(
        "source-a",
        "api.exemple.com",
        ["session=jeton-42; Path=/; HttpOnly", "langue=fr"],
    );
    assert_eq!(
        b.en_tete("source-a", "API.exemple.com").as_deref(),
        Some("langue=fr; session=jeton-42")
    );
    assert_eq!(
        b.en_tete("source-b", "api.exemple.com"),
        None,
        "autre greffon"
    );
    assert_eq!(b.en_tete("source-a", "cdn.exemple.com"), None, "autre hôte");

    b.absorber(
        "source-a",
        "api.exemple.com",
        ["session=; Max-Age=0", "langue=en; Max-Age=0"],
    );
    assert_eq!(b.en_tete("source-a", "api.exemple.com"), None);

    b.absorber("source-a", "api.exemple.com", ["x=1", "sans-egal", "=vide"]);
    assert_eq!(
        b.en_tete("source-a", "api.exemple.com").as_deref(),
        Some("x=1")
    );
    b.oublier("source-a");
    assert_eq!(b.en_tete("source-a", "api.exemple.com"), None);
}

/// Un serveur local : `/login` pose un cookie, `/moi` le relit, `/ailleurs`
/// redirige.
async fn serveur() -> String {
    async fn login() -> impl IntoResponse {
        (
            [(header::SET_COOKIE, "session=jeton-42; Path=/; HttpOnly")],
            "{\"ok\":true}",
        )
    }
    async fn moi(entetes: HeaderMap) -> impl IntoResponse {
        let cookie = entetes
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if cookie.contains("session=jeton-42") {
            (StatusCode::OK, cookie)
        } else {
            (StatusCode::UNAUTHORIZED, cookie)
        }
    }
    async fn ailleurs() -> impl IntoResponse {
        (
            StatusCode::FOUND,
            [(header::LOCATION, "https://evil.com/")],
            "",
        )
    }
    let app = Router::new()
        .route("/login", get(login))
        .route("/moi", get(moi))
        .route("/ailleurs", get(ailleurs));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    format!("http://{adresse}")
}

/// Le cookie de session est gardé par l'HÔTE : le greffon ne voit pas le
/// `Set-Cookie`, la requête suivante porte le cookie, et un autre greffon ne
/// l'a pas. Une redirection n'est pas suivie.
#[tokio::test]
async fn la_session_reste_chez_l_hote_et_n_est_pas_partagee() {
    let base = serveur().await;
    let reseau = ReseauDesGreffons::new();

    let r = reseau
        .executer(
            "source-a",
            json!({ "method": "GET", "url": format!("{base}/login") }),
        )
        .await
        .unwrap();
    assert_eq!(r["status"], 200);
    assert!(
        r["headers"].get("set-cookie").is_none(),
        "le greffon ne voit pas le cookie : {r}"
    );
    assert!(!r.to_string().contains("jeton-42"), "{r}");

    let r = reseau
        .executer("source-a", json!({ "url": format!("{base}/moi") }))
        .await
        .unwrap();
    assert_eq!(r["status"], 200, "{r}");

    let r = reseau
        .executer("source-b", json!({ "url": format!("{base}/moi") }))
        .await
        .unwrap();
    assert_eq!(
        r["status"], 401,
        "un autre greffon n'a pas la session : {r}"
    );

    let r = reseau
        .executer("source-a", json!({ "url": format!("{base}/ailleurs") }))
        .await
        .unwrap();
    assert_eq!(r["status"], 302, "{r}");
    assert_eq!(r["headers"]["location"], "https://evil.com/");

    reseau.oublier("source-a");
    let r = reseau
        .executer("source-a", json!({ "url": format!("{base}/moi") }))
        .await
        .unwrap();
    assert_eq!(r["status"], 401, "déconnecté, la session est oubliée");

    assert!(
        reseau
            .executer("source-a", json!({ "method": "CONNECT", "url": base }))
            .await
            .is_err()
    );
}

/// Étape 2 — le budget d'UN appel (nombre de requêtes, délai) et le quota
/// par minute refusent AVANT tout réseau, avec un code que le greffon lit,
/// et chaque refus entre au journal.
#[tokio::test]
async fn le_budget_d_un_appel_et_le_quota_refusent_sans_reseau() {
    let base = serveur().await;
    let reseau = Arc::new(ReseauDesGreffons::avec_limites(Limites {
        delai_par_appel: Duration::from_millis(300),
        requetes_par_appel: 2,
        requetes_par_minute: 4,
    }));
    let moi = || json!({ "url": format!("{base}/moi") });

    {
        let _borne = reseau.borner("source-a");
        for _ in 0..2 {
            let r = reseau.executer("source-a", moi()).await.unwrap();
            assert_eq!(r["status"], 401, "{r}");
        }
        let r = reseau.executer("source-a", moi()).await.unwrap();
        assert_eq!(r["error"], "too_many_requests", "{r}");
    }
    // L'appel suivant a son propre budget ; le quota par minute, lui, court.
    let r = reseau.executer("source-a", moi()).await.unwrap();
    assert_eq!(r["status"], 401, "{r}");
    let r = reseau.executer("source-a", moi()).await.unwrap();
    assert_eq!(r["status"], 401, "{r}");
    let r = reseau.executer("source-a", moi()).await.unwrap();
    assert_eq!(r["error"], "rate_limited", "{r}");
    // Le quota est PAR greffon.
    let r = reseau.executer("source-b", moi()).await.unwrap();
    assert_eq!(r["status"], 401, "un autre greffon n'est pas puni : {r}");

    // Le délai de l'appel : passé l'échéance, plus rien ne part.
    {
        let _borne = reseau.borner("source-b");
        tokio::time::sleep(Duration::from_millis(350)).await;
        let r = reseau.executer("source-b", moi()).await.unwrap();
        assert_eq!(r["error"], "call_timeout", "{r}");
    }

    let journal = reseau.journal("source-a");
    assert_eq!(journal.len(), 1, "{journal:?}");
    assert_eq!(journal[0].host, "127.0.0.1");
    assert_eq!(journal[0].requests, 4);
    assert_eq!(journal[0].refused, 2);
    assert_eq!(journal[0].last_error.as_deref(), Some("rate_limited"));
    assert_eq!(journal[0].last_status, Some(401));
    let journal = reseau.journal("source-b");
    assert_eq!(journal[0].refused, 1);
    assert_eq!(journal[0].last_error.as_deref(), Some("call_timeout"));
}

/// Étape 2 — le journal ne grossit pas sans fin : au-delà de
/// [`HOTES_DANS_LE_JOURNAL`] hôtes, les suivants sont comptés ensemble.
#[test]
fn le_journal_borne_le_nombre_d_hotes() {
    let reseau = ReseauDesGreffons::new();
    for i in 0..(HOTES_DANS_LE_JOURNAL + 10) {
        reseau.noter_un_refus("g", &format!("h{i}.evil.example"), "domain_not_allowed");
    }
    let journal = reseau.journal("g");
    assert_eq!(journal.len(), HOTES_DANS_LE_JOURNAL + 1);
    let autres = journal.iter().find(|e| e.host == HOTE_AUTRES).unwrap();
    assert_eq!(autres.refused, 10);
}
