//! L'ordre des étiquettes (#6257, web#2166) sur une VRAIE base PostgreSQL :
//! la migration 090 a posé `tags.position` en BIGINT, `PUT /tags/order`
//! l'écrit, et `GET /tags/` comme `GET /tags/for/…` le suivent.
//!
//! Même doctrine que `pg_etiquette_collection_5026.rs` : variable ABSENTE ⇒
//! saut annoncé ; variable POSÉE mais injoignable ⇒ le test TOMBE.

#![cfg(feature = "postgres")]

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_server::state::AppState;

/// Marque propre à cette épreuve : les tables sont partagées avec les autres
/// bancs PostgreSQL, on ne vide que nos lignes.
const MARQUE: &str = "ordre-6257";

fn etat_postgres(url: &str) -> AppState {
    let config = tune_server::config::TuneConfig {
        database_url: Some(url.to_string()),
        ..Default::default()
    };
    AppState::new("", 0, config).expect("AppState sur PostgreSQL")
}

fn menage(state: &AppState) {
    for sql in [
        format!(
            "DELETE FROM item_tags WHERE tag_id IN (SELECT id FROM tags WHERE name LIKE '{MARQUE}%')"
        ),
        format!("DELETE FROM tags WHERE name LIKE '{MARQUE}%'"),
    ] {
        state
            .backend
            .execute(&sql, &[])
            .unwrap_or_else(|e| panic!("ménage « {sql} » : {e}"));
    }
}

async fn appel(state: &AppState, methode: &str, route: &str, corps: Option<Value>) -> Value {
    let app: Router = tune_server::routes::router(state.clone());
    let requete = Request::builder()
        .method(methode)
        .uri(route)
        .header(header::CONTENT_TYPE, "application/json");
    let requete = match corps {
        Some(c) => requete.body(Body::from(c.to_string())).unwrap(),
        None => requete.body(Body::empty()).unwrap(),
    };
    let reponse = app.oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let corps = axum::body::to_bytes(reponse.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&corps).to_string();
    assert!(
        statut.is_success(),
        "{methode} {route} → {statut} sur PostgreSQL (#6257) : {texte}"
    );
    serde_json::from_str(&texte).unwrap_or(Value::Null)
}

fn les_notres(liste: &Value) -> Vec<String> {
    liste
        .as_array()
        .expect("une liste d'étiquettes")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .filter(|n| n.starts_with(MARQUE))
        .map(str::to_string)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_6257_l_ordre_des_etiquettes_est_enregistre_et_suivi() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let state = etat_postgres(&url);
    menage(&state);

    let type_pg = state
        .backend
        .query_one(
            "SELECT data_type FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name = 'tags' \
             AND column_name = 'position'",
            &[],
        )
        .expect("information_schema")
        .and_then(|l| l.first().and_then(|v| v.as_string()));
    assert_eq!(
        type_pg.as_deref(),
        Some("bigint"),
        "la 090 doit poser `tags.position` en BIGINT"
    );

    let mut ids = Vec::new();
    for suffixe in ["a", "b", "c"] {
        let nom = format!("{MARQUE} {suffixe}");
        let v = appel(&state, "POST", "/api/v1/tags", Some(json!({ "name": nom }))).await;
        ids.push(v["id"].as_i64().expect("id de l'étiquette créée"));
    }
    for id in &ids {
        appel(
            &state,
            "POST",
            &format!("/api/v1/tags/{id}/items"),
            Some(json!({ "item_type": "album", "item_id": 987_654 })),
        )
        .await;
    }

    let (a, b, c) = (ids[0], ids[1], ids[2]);
    appel(
        &state,
        "PUT",
        "/api/v1/tags/order",
        Some(json!({ "ids": [c, a, b] })),
    )
    .await;

    let attendu = vec![
        format!("{MARQUE} c"),
        format!("{MARQUE} a"),
        format!("{MARQUE} b"),
    ];
    for route in [
        "/api/v1/tags",
        "/api/v1/tags?item_type=album",
        "/api/v1/tags/for/album/987654",
    ] {
        let liste = appel(&state, "GET", route, None).await;
        assert_eq!(
            les_notres(&liste),
            attendu,
            "{route} ne suit pas l'ordre (#6257)"
        );
    }

    menage(&state);
}
