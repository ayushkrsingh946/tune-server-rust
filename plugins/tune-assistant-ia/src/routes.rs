//! Les routes HTTP du greffon. Voir le tableau de [`crate`].

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use crate::appel;
use crate::reglages::{self, Modification};
use crate::regles::{self, Cible};
use crate::suggestions::{self, EtatAlbum, LigneAAppliquer};
use crate::{Etat, cache, couts};

pub fn router_vers(etat: Etat) -> Router<()> {
    Router::new()
        .route("/reglages", get(lire_reglages).put(changer_reglages))
        .route("/tester", post(tester))
        .route("/playlist", post(playlist))
        .route("/albums/{id}/suggestions", post(suggerer))
        .route("/albums/{id}/appliquer", post(appliquer))
        .route("/albums/{id}/annuler", post(annuler))
        .route("/albums/{id}/historique", get(historique))
        .with_state(etat)
}

fn erreur(statut: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        statut,
        Json(json!({ "ok": false, "error": code, "message": message.into() })),
    )
        .into_response()
}

fn interne(e: impl std::fmt::Display) -> Response {
    warn!(erreur = %e, "assistant_ia_erreur_interne");
    erreur(StatusCode::INTERNAL_SERVER_ERROR, "interne", e.to_string())
}

/// La langue de la réponse : celle demandée, sinon celle de l'interface.
fn langue(demandee: Option<&str>, entetes: &HeaderMap) -> String {
    let brute = demandee
        .map(String::from)
        .or_else(|| {
            entetes
                .get(axum::http::header::ACCEPT_LANGUAGE)
                .and_then(|v| v.to_str().ok())
                .map(String::from)
        })
        .unwrap_or_default();
    let code: String = brute
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .take(3)
        .collect::<String>()
        .to_ascii_lowercase();
    if code.len() >= 2 { code } else { "fr".into() }
}

// ---------------------------------------------------------------------------
// Réglages
// ---------------------------------------------------------------------------

async fn lire_reglages(State(etat): State<Etat>) -> Response {
    Json(reglages::vue(&etat.backend, &etat.coffre)).into_response()
}

async fn changer_reglages(State(etat): State<Etat>, Json(m): Json<Modification>) -> Response {
    match reglages::appliquer(&etat.backend, &etat.coffre, &m) {
        Ok(()) => {
            info!(
                cle_changee = m.cle.is_some(),
                "assistant_ia_reglages_changes"
            );
            Json(reglages::vue(&etat.backend, &etat.coffre)).into_response()
        }
        Err(e) => erreur(StatusCode::BAD_REQUEST, "reglage_invalide", e),
    }
}

/// « Tester la clé » : un appel réel, minuscule, hors cache.
async fn tester(State(etat): State<Etat>) -> Response {
    let r = appel::appeler(
        &etat,
        "test",
        false,
        "Reply with the JSON object {\"ok\": true} and nothing else.",
        "ping",
        256,
        None,
    )
    .await;
    match r {
        Ok(res) => Json(json!({
            "ok": true,
            "cout_eur": couts::micro_en_euros(res.cout_micro_eur),
        }))
        .into_response(),
        Err(e) => appel::reponse_erreur(&e),
    }
}

// ---------------------------------------------------------------------------
// Playlist par phrase
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DemandePlaylist {
    phrase: String,
    cible: Option<String>,
    langue: Option<String>,
}

/// Les genres les plus fréquents de la bibliothèque : le modèle les reprend
/// tels qu'ils sont écrits, au lieu d'en inventer l'orthographe.
fn genres_de_la_bibliotheque(etat: &Etat) -> Vec<String> {
    etat.backend
        .query_many(
            "SELECT genre, COUNT(*) AS n FROM albums WHERE genre IS NOT NULL AND genre <> '' \
             GROUP BY genre ORDER BY n DESC LIMIT 60",
            &[],
        )
        .map(|lignes| {
            lignes
                .iter()
                .filter_map(|l| l.first().and_then(|v| v.as_string()))
                .collect()
        })
        .unwrap_or_default()
}

async fn playlist(
    State(etat): State<Etat>,
    entetes: HeaderMap,
    Json(d): Json<DemandePlaylist>,
) -> Response {
    let phrase = cache::normaliser_phrase(&d.phrase);
    if phrase.is_empty() || phrase.chars().count() > 500 {
        return erreur(
            StatusCode::BAD_REQUEST,
            "phrase_invalide",
            "Décrivez la playlist en une phrase (500 caractères au plus).",
        );
    }
    let cible = Cible::depuis(d.cible.as_deref());
    let langue = langue(d.langue.as_deref(), &entetes);
    let offerts = regles::criteres_offerts(&etat.criteres, cible);
    let genres = genres_de_la_bibliotheque(&etat);
    let systeme = regles::consigne(&offerts, cible, &genres, &langue);
    // La consigne entière fait partie de la clé : de nouveaux genres, un
    // critère ajouté au moteur, une autre langue — une autre question.
    let entree = format!("{}\n{}\n{}", cible.nom(), systeme, phrase);
    let res = match appel::appeler(
        &etat,
        "playlist",
        false,
        &systeme,
        &d.phrase,
        4096,
        Some(&entree),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return appel::reponse_erreur(&e),
    };
    let Some(brut) = appel::objet_json(&res.texte) else {
        return erreur(
            StatusCode::UNPROCESSABLE_ENTITY,
            "reponse_invalide",
            "Le modèle n'a pas rendu un objet JSON.",
        );
    };
    match regles::valider(&brut, &offerts, cible) {
        Ok(p) => Json(json!({
            "ok": true,
            "cible": cible.nom(),
            "proposition": p,
            "depuis_cache": res.depuis_cache,
            "cout_eur": couts::micro_en_euros(res.cout_micro_eur),
        }))
        .into_response(),
        Err(code) => erreur(
            StatusCode::UNPROCESSABLE_ENTITY,
            code,
            "La réponse du modèle ne suit pas le schéma demandé.",
        ),
    }
}

// ---------------------------------------------------------------------------
// Suggestions sur un album
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct DemandeSuggestions {
    langue: Option<String>,
}

async fn suggerer(
    State(etat): State<Etat>,
    Path(id): Path<i64>,
    entetes: HeaderMap,
    corps: Option<Json<DemandeSuggestions>>,
) -> Response {
    let d = corps.map(|Json(d)| d).unwrap_or_default();
    let etat_album = match EtatAlbum::charger(&etat.backend, id) {
        Ok(Some(e)) => e,
        Ok(None) => {
            return erreur(
                StatusCode::NOT_FOUND,
                "album_introuvable",
                "Album introuvable.",
            );
        }
        Err(e) => return interne(e),
    };
    let langue = langue(d.langue.as_deref(), &entetes);
    let systeme = suggestions::consigne(&langue);
    let contexte = etat_album.contexte().to_string();
    let entree = format!("{systeme}\n{contexte}");
    let classique = etat_album.semble_classique();
    let res = match appel::appeler(
        &etat,
        "album",
        classique,
        &systeme,
        &contexte,
        4096,
        Some(&entree),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return appel::reponse_erreur(&e),
    };
    let Some(brut) = appel::objet_json(&res.texte) else {
        return erreur(
            StatusCode::UNPROCESSABLE_ENTITY,
            "reponse_invalide",
            "Le modèle n'a pas rendu un objet JSON.",
        );
    };
    // L'état est RELU ici : une réponse du cache est jugée sur la base
    // d'aujourd'hui, pas sur celle du jour où elle a été payée.
    let etat_album = match EtatAlbum::charger(&etat.backend, id) {
        Ok(Some(e)) => e,
        Ok(None) => {
            return erreur(
                StatusCode::NOT_FOUND,
                "album_introuvable",
                "Album introuvable.",
            );
        }
        Err(e) => return interne(e),
    };
    match suggestions::evaluer(&brut, &etat_album) {
        Ok((retenues, ecartees)) => Json(json!({
            "ok": true,
            "album_id": id,
            "suggestions": retenues,
            "ecartees": ecartees,
            "depuis_cache": res.depuis_cache,
            "cout_eur": couts::micro_en_euros(res.cout_micro_eur),
        }))
        .into_response(),
        Err(code) => erreur(
            StatusCode::UNPROCESSABLE_ENTITY,
            code,
            "La réponse du modèle ne suit pas le schéma demandé.",
        ),
    }
}

#[derive(Deserialize)]
struct DemandeApplication {
    lignes: Vec<LigneAAppliquer>,
}

async fn appliquer(
    State(etat): State<Etat>,
    Path(id): Path<i64>,
    Json(d): Json<DemandeApplication>,
) -> Response {
    let _verrou = etat.verrou_albums.lock().await;
    match suggestions::appliquer(&etat.backend, id, &d.lignes) {
        Ok(Some(r)) => {
            info!(
                album_id = id,
                appliquees = r.appliquees.len(),
                refusees = r.refusees.len(),
                "assistant_ia_suggestions_appliquees"
            );
            Json(json!({ "ok": true, "rapport": r })).into_response()
        }
        Ok(None) => erreur(
            StatusCode::NOT_FOUND,
            "album_introuvable",
            "Album introuvable.",
        ),
        Err(e) => interne(e),
    }
}

#[derive(Deserialize, Default)]
struct DemandeAnnulation {
    operation_id: Option<String>,
}

async fn annuler(
    State(etat): State<Etat>,
    Path(id): Path<i64>,
    corps: Option<Json<DemandeAnnulation>>,
) -> Response {
    let d = corps.map(|Json(d)| d).unwrap_or_default();
    let _verrou = etat.verrou_albums.lock().await;
    match suggestions::annuler(&etat.backend, id, d.operation_id.as_deref()) {
        Ok(Some(r)) => Json(json!({ "ok": true, "rapport": r })).into_response(),
        Ok(None) => erreur(
            StatusCode::NOT_FOUND,
            "album_introuvable",
            "Album introuvable.",
        ),
        Err(e) => interne(e),
    }
}

async fn historique(State(etat): State<Etat>, Path(id): Path<i64>) -> Response {
    match suggestions::journal(&etat.backend, id) {
        Ok(ops) => Json(json!({ "ok": true, "operations": ops })).into_response(),
        Err(e) => interne(e),
    }
}
