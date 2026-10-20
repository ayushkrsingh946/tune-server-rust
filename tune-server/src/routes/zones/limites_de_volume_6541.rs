//! #6541 — boutons + / − et limites de volume d'une zone (fil forum 2335).
//!
//! - `POST /zones/{id}/volume/step` `{"direction": "up"|"down", "step"?: 0.02,
//!   "step_db"?: 1.0}` : un pas depuis le volume courant. `step` (fraction
//!   linéaire, défaut 0,02) et `step_db` sont exclusifs. La limite de CONFORT
//!   ne retient pas ce chemin ; la limite de sécurité, si. Rend le volume
//!   retenu.
//! - `GET` / `PUT /zones/{id}/volume-limits` `{"safety"?, "safety_db"?,
//!   "comfort"?, "comfort_db"?}` : les deux seuils, en fraction 0..1 OU en dB
//!   (exclusifs par seuil). Le PUT remplace les deux : un seuil absent ou
//!   `null` est effacé (pas de limite). Une zone au-dessus de la nouvelle
//!   sécurité est ramenée sur-le-champ.
//!
//! Les règles elles-mêmes vivent dans `tune_core::audio::volume_limites` et
//! s'appliquent dans `PlaybackOrchestrator::set_volume`, pour TOUTES les
//! routes de volume.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use tune_core::audio::volume_limites::{self, LIMITE_MINIMALE, LimitesDeVolume, PAS_PAR_DEFAUT};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;

use crate::state::AppState;

/// Plus grand pas accepté : un quart de l'échelle, ou 12 dB. Au-delà, ce
/// n'est plus un bouton, c'est un saut.
const PAS_MAXIMAL: f64 = 0.25;
const PAS_MAXIMAL_DB: f64 = 12.0;

#[derive(Debug, Deserialize)]
pub(super) struct PasDeVolume {
    direction: String,
    step: Option<f64>,
    step_db: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct ReglageDesLimites {
    safety: Option<f64>,
    safety_db: Option<f64>,
    comfort: Option<f64>,
    comfort_db: Option<f64>,
}

fn refus(code: &str, message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": code, "message": message.into() })),
    )
        .into_response()
}

/// Un seuil demandé en fraction OU en dB, ramené à une fraction. `Ok(None)` =
/// pas de limite.
fn un_seuil(nom: &str, lineaire: Option<f64>, db: Option<f64>) -> Result<Option<f64>, String> {
    let valeur = match (lineaire, db) {
        (None, None) => return Ok(None),
        (Some(_), Some(_)) => {
            return Err(format!(
                "{nom} et {nom}_db sont exclusifs — n'en envoyer qu'un"
            ));
        }
        (Some(v), None) => v,
        (None, Some(d)) if d > 0.0 => {
            return Err(format!("{nom}_db doit être négatif ou nul (0 dB = 100 %)"));
        }
        (None, Some(d)) => tune_core::audio::volume_scale::db_to_linear(d)
            .ok_or_else(|| format!("{nom}_db n'est pas un nombre"))?,
    };
    if !valeur.is_finite() || !(LIMITE_MINIMALE..=1.0).contains(&valeur) {
        return Err(format!(
            "{nom} doit être compris entre {LIMITE_MINIMALE} et 1 (fraction de la pleine échelle)"
        ));
    }
    Ok(Some(valeur))
}

fn corps_des_limites(zone_id: i64, limites: &LimitesDeVolume) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("zone_id".into(), json!(zone_id));
    volume_limites::inserer(&mut obj, limites);
    serde_json::Value::Object(obj)
}

pub(super) async fn lire_les_limites(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    if ZoneRepo::with_backend(state.backend.clone())
        .get(id)
        .ok()
        .flatten()
        .is_none()
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(corps_des_limites(
        id,
        &volume_limites::lire(&state.backend, id),
    ))
    .into_response()
}

pub(super) async fn regler_les_limites(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ReglageDesLimites>,
) -> Response {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let Some(zone) = repo.get(id).ok().flatten() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let securite = match un_seuil("safety", body.safety, body.safety_db) {
        Ok(v) => v,
        Err(m) => return refus("invalid_volume_limit", m),
    };
    let confort = match un_seuil("comfort", body.comfort, body.comfort_db) {
        Ok(v) => v,
        Err(m) => return refus("invalid_volume_limit", m),
    };
    let settings = SettingsRepo::with_backend(state.backend.clone());
    for (cle, valeur) in [
        (volume_limites::cle_securite(id), securite),
        (volume_limites::cle_confort(id), confort),
    ] {
        let r = match valeur {
            Some(v) => settings.set(&cle, &format!("{v}")),
            None => settings.delete(&cle),
        };
        if let Err(e) = r {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "volume_limit_write_failed", "message": e })),
            )
                .into_response();
        }
    }
    let limites = volume_limites::lire(&state.backend, id);
    info!(
        zone_id = id,
        securite = ?limites.securite,
        confort = ?limites.confort,
        "zone_volume_limits_set"
    );
    // Effet immédiat : une zone au-dessus de la nouvelle sécurité est
    // ramenée MAINTENANT, pas au prochain geste. Une zone à volume fixe ne
    // reçoit rien de `set_volume` : son appareil est réarmé, borné.
    if let Some(s) = limites.securite {
        let actuel = state.playback.get_state(id).await.volume;
        let device_id = zone.output_device_id.as_deref();
        let resultat = if zone.fixed_volume {
            state.orchestrator.arm_fixed_volume(id, device_id).await
        } else if actuel > s || zone.volume / 100.0 > s {
            state
                .orchestrator
                .set_volume(id, actuel.min(s), device_id)
                .await
        } else {
            Ok(())
        };
        if let Err(error) = resultat {
            warn!(zone_id = id, error = %error, "zone_volume_limit_refresh_failed");
        }
    }
    state.event_bus.emit_typed(
        tune_core::event_types::EventType::ZoneUpdated,
        json!({ "id": id }),
    );
    Json(corps_des_limites(id, &limites)).into_response()
}

pub(super) async fn pas_de_volume(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<PasDeVolume>,
) -> Response {
    let signe = match body.direction.as_str() {
        "up" => 1.0,
        "down" => -1.0,
        autre => {
            return refus(
                "invalid_volume_step",
                format!("direction « {autre} » inconnue : « up » ou « down »"),
            );
        }
    };
    let Some(zone) = ZoneRepo::with_backend(state.backend.clone())
        .get(id)
        .ok()
        .flatten()
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let actuel = state.playback.get_state(id).await.volume;
    let cible = match (body.step, body.step_db) {
        (Some(_), Some(_)) => {
            return refus(
                "invalid_volume_step",
                "step et step_db sont exclusifs — n'en envoyer qu'un",
            );
        }
        (None, Some(d)) if !(d.is_finite() && d > 0.0 && d <= PAS_MAXIMAL_DB) => {
            return refus(
                "invalid_volume_step",
                format!("step_db doit être compris entre 0 (exclu) et {PAS_MAXIMAL_DB}"),
            );
        }
        (None, Some(d)) => volume_limites::cible_du_pas_db(actuel, signe * d),
        (pas, None) => {
            let pas = pas.unwrap_or(PAS_PAR_DEFAUT);
            if !(pas.is_finite() && pas > 0.0 && pas <= PAS_MAXIMAL) {
                return refus(
                    "invalid_volume_step",
                    format!("step doit être compris entre 0 (exclu) et {PAS_MAXIMAL}"),
                );
            }
            volume_limites::cible_du_pas(actuel, signe * pas)
        }
    };
    if let Err(error) = state
        .orchestrator
        .set_volume_au_pas(id, cible, zone.output_device_id.as_deref())
        .await
    {
        return crate::routes::playback::output_command_error_response(error);
    }
    let retenu = state.playback.get_state(id).await.volume;
    let limites = volume_limites::lire(&state.backend, id);
    let mut obj = serde_json::Map::new();
    obj.insert("zone_id".into(), json!(id));
    tune_core::audio::volume_scale::inserer_volume(&mut obj, retenu);
    // `limited` : la sécurité a retenu le pas — le client peut le dire au
    // lieu de laisser croire à un bouton cassé.
    obj.insert("limited".into(), json!(retenu + 1e-9 < cible));
    volume_limites::inserer(&mut obj, &limites);
    Json(serde_json::Value::Object(obj)).into_response()
}

#[cfg(test)]
#[path = "limites_de_volume_6541_tests.rs"]
mod tests;
