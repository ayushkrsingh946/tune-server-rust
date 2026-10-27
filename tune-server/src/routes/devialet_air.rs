//! #6542 — réglages de la sortie Devialet AIR (expérimentale).
//!
//! `GET  /devialet-air/reglages` : l'option, l'indice de la source AIR, le
//! plafond de volume et la course du curseur en dB (#6918).
//! `POST /devialet-air/reglages` : `{ "enabled": bool, "source_index": 0..14,
//! "volume_min_db": -80..-10, "volume_max_db": -80..-10 }`, chaque champ
//! facultatif. Une borne absente garde sa valeur ; la course est refusée
//! (`400 volume_range_invalid`) si elle sort de `[−80 ; −10]` dB ou si ses
//! bornes sont à moins de 5 dB l'une de l'autre. Changer la course n'envoie
//! rien à l'ampli : elle vaut pour la consigne suivante.
//!
//! Le réglage est relu à chaque usage : désactiver coupe l'écoute au passage
//! suivant du fournisseur et refuse toute lecture ou commande sur-le-champ.
//! Activer fait apparaître les amplis au passage suivant (au plus 60 s).

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::outputs::devialet_air::reglages;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/reglages", get(lire).post(regler))
}

fn etat(state: &AppState) -> Value {
    let bornes = reglages::BornesDb::lues(&state.backend);
    json!({
        "enabled": reglages::activee(&state.backend),
        "experimental": true,
        "source_index": reglages::indice_source(&state.backend),
        "source_index_default": reglages::INDICE_SOURCE_PAR_DEFAUT,
        "source_index_max": reglages::INDICE_SOURCE_MAX,
        "volume_ceiling_db": reglages::PLAFOND_DB,
        "volume_floor_db": reglages::PLANCHER_DB,
        "volume_step_db": reglages::PAS_DB,
        "volume_min_db": bornes.min_db(),
        "volume_max_db": bornes.max_db(),
        "max_sample_rate": 96_000,
    })
}

async fn lire(State(state): State<AppState>) -> Json<Value> {
    Json(etat(&state))
}

#[derive(Deserialize)]
struct Corps {
    enabled: Option<bool>,
    source_index: Option<i64>,
    volume_min_db: Option<f64>,
    volume_max_db: Option<f64>,
}

async fn regler(State(state): State<AppState>, Json(corps): Json<Corps>) -> impl IntoResponse {
    let r = SettingsRepo::with_backend(state.backend.clone());
    // #6918 — la course est validée en entier AVANT toute écriture : une
    // requête refusée ne laisse aucun réglage à moitié posé.
    let avant = reglages::BornesDb::lues(&state.backend);
    let bornes = if corps.volume_min_db.is_some() || corps.volume_max_db.is_some() {
        match reglages::BornesDb::nouvelles(
            corps.volume_min_db.unwrap_or(avant.min_db()),
            corps.volume_max_db.unwrap_or(avant.max_db()),
        ) {
            Ok(b) => Some(b),
            Err(cause) => {
                info!(cause = %cause, "devialet_air_volume_range_refused");
                return crate::message_api::refus(
                    StatusCode::BAD_REQUEST,
                    "volume_range_invalid",
                    "api.devialet.volumeRangeInvalid",
                    &[
                        ("floor", reglages::PLANCHER_DB.to_string()),
                        ("ceiling", reglages::PLAFOND_DB.to_string()),
                        ("gap", reglages::ECART_MINIMAL_DB.to_string()),
                    ],
                );
            }
        }
    } else {
        None
    };
    if let Some(i) = corps.source_index {
        if !(0..=i64::from(reglages::INDICE_SOURCE_MAX)).contains(&i) {
            return crate::message_api::refus(
                StatusCode::BAD_REQUEST,
                "source_index_out_of_range",
                "api.devialet.sourceIndexRange",
                &[("max", reglages::INDICE_SOURCE_MAX.to_string())],
            );
        }
        if let Err(e) = r.set(reglages::CLE_INDICE_SOURCE, &i.to_string()) {
            return erreur(e);
        }
    }
    if let Some(on) = corps.enabled
        && let Err(e) = r.set(reglages::CLE_ACTIVEE, if on { "true" } else { "false" })
    {
        return erreur(e);
    }
    if let Some(b) = bornes {
        // #6918 — la course change, les niveaux en dB restent : volume et
        // limites des zones Devialet sont réécrits sur la nouvelle course
        // (refus si une limite devait remonter). Rien ne part vers l'ampli.
        let zones = match tune_core::audio::volume_appareil_db::reexprimer_les_zones(
            &state.backend,
            avant,
            b,
        ) {
            Ok(z) => z,
            Err(cause) => {
                info!(cause = %cause, "devialet_air_volume_range_refused");
                return crate::message_api::refus(
                    StatusCode::CONFLICT,
                    "volume_range_below_limit",
                    "api.devialet.volumeRangeBelowLimit",
                    &[],
                );
            }
        };
        if let Err(e) = b.enregistrer(&state.backend) {
            return erreur(e);
        }
        for id in zones {
            let actuel = state.playback.get_state(id).await.volume;
            state
                .playback
                .set_volume(
                    id,
                    tune_core::audio::volume_appareil_db::reexprimer(avant, b, actuel),
                )
                .await;
        }
    }
    let v = etat(&state);
    info!(
        enabled = %v["enabled"],
        source_index = %v["source_index"],
        volume_min_db = %v["volume_min_db"],
        volume_max_db = %v["volume_max_db"],
        "devialet_air_reglages"
    );
    Json(v).into_response()
}

fn erreur(e: String) -> axum::response::Response {
    crate::message_api::refus_avec_cause(
        StatusCode::INTERNAL_SERVER_ERROR,
        "setting_not_saved",
        "api.settings.notSaved",
        &[],
        e,
    )
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::state::AppState;

    async fn appel(
        app: &axum::Router,
        methode: &str,
        chemin: &str,
        corps: Option<Value>,
    ) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(methode)
            .uri(chemin)
            .header("content-type", "application/json")
            .body(
                corps
                    .map(|c| Body::from(c.to_string()))
                    .unwrap_or_else(Body::empty),
            )
            .unwrap();
        let rep = app.clone().oneshot(req).await.unwrap();
        let statut = rep.status();
        let octets = axum::body::to_bytes(rep.into_body(), 64 * 1024)
            .await
            .unwrap();
        (
            statut,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn air_6542_reglages_desactives_par_defaut_puis_persistes() {
        let state = AppState::new(":memory:", 0, Default::default()).expect("état");
        let db = state.backend.clone();
        let app = crate::routes::router_with_plugins(state, Vec::new());
        let (statut, corps) = appel(&app, "GET", "/api/v1/devialet-air/reglages", None).await;
        assert_eq!(statut, StatusCode::OK);
        assert_eq!(corps["enabled"], false, "désactivée par défaut : {corps}");
        assert_eq!(corps["source_index"], 9);
        assert_eq!(corps["volume_ceiling_db"], -10.0);

        let (statut, corps) = appel(
            &app,
            "POST",
            "/api/v1/devialet-air/reglages",
            Some(serde_json::json!({"enabled": true, "source_index": 13})),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        assert_eq!(corps["enabled"], true);
        assert_eq!(corps["source_index"], 13);
        assert!(tune_core::outputs::devialet_air::reglages::activee(&db));

        let (statut, _) = appel(
            &app,
            "POST",
            "/api/v1/devialet-air/reglages",
            Some(serde_json::json!({"source_index": 15})),
        )
        .await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "hors bornes refusé");

        let (_, corps) = appel(
            &app,
            "POST",
            "/api/v1/devialet-air/reglages",
            Some(serde_json::json!({"enabled": false})),
        )
        .await;
        assert_eq!(corps["enabled"], false);
        assert_eq!(corps["source_index"], 13, "l'indice n'a pas bougé");
        assert!(!tune_core::outputs::devialet_air::reglages::activee(&db));
    }

    /// #6918 — la course du curseur en dB se règle dans `[−80 ; −10]` ; une
    /// borne haute au-dessus du plafond est refusée et ne laisse rien écrit.
    #[tokio::test]
    async fn air_6918_course_du_curseur_reglable_sous_le_plafond() {
        let state = AppState::new(":memory:", 0, Default::default()).expect("état");
        let db = state.backend.clone();
        let app = crate::routes::router_with_plugins(state, Vec::new());
        let (_, corps) = appel(&app, "GET", "/api/v1/devialet-air/reglages", None).await;
        assert_eq!(corps["volume_min_db"], -80.0, "{corps}");
        assert_eq!(corps["volume_max_db"], -10.0, "{corps}");
        assert_eq!(corps["volume_step_db"], 0.5);

        let (statut, corps) = appel(
            &app,
            "POST",
            "/api/v1/devialet-air/reglages",
            Some(serde_json::json!({"volume_min_db": -70, "volume_max_db": -20})),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        assert_eq!(corps["volume_min_db"], -70.0);
        assert_eq!(corps["volume_max_db"], -20.0);
        assert_eq!(corps["volume_ceiling_db"], -10.0, "le plafond ne bouge pas");

        for refusee in [
            serde_json::json!({"volume_max_db": 0}),
            serde_json::json!({"volume_max_db": -9.5}),
            serde_json::json!({"volume_min_db": -90}),
            serde_json::json!({"volume_min_db": -22}),
            serde_json::json!({"volume_max_db": 0, "source_index": 3}),
        ] {
            let (statut, corps) = appel(
                &app,
                "POST",
                "/api/v1/devialet-air/reglages",
                Some(refusee.clone()),
            )
            .await;
            assert_eq!(statut, StatusCode::BAD_REQUEST, "{refusee} : {corps}");
            assert_eq!(corps["error"], "volume_range_invalid", "{corps}");
        }
        let lues = tune_core::outputs::devialet_air::reglages::BornesDb::lues(&db);
        assert_eq!((lues.min_db(), lues.max_db()), (-70.0, -20.0), "rien écrit");
        assert_eq!(
            tune_core::outputs::devialet_air::reglages::indice_source(&db),
            9,
            "une requête refusée n'écrit pas non plus l'indice"
        );
    }
}
