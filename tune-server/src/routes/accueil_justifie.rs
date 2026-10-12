//! L'accueil qui dit POURQUOI, et qui entend « Pas pour moi »
//! (renesenses/tune-web-client#916, FabienM, fil forum 1527, go de Bertrand
//! du 12/10/2026).
//!
//! # La raison structurée : `raison`
//!
//! Chaque élément d'une rangée qui SAIT pourquoi il le propose porte un champ
//! `raison` — un objet, jamais une phrase : la phrase est faite par le client,
//! dans la langue de l'interface. Les types, et seulement eux :
//!
//! | `type`            | champs                  | rangée — ce que la logique sait VRAIMENT |
//! |-------------------|-------------------------|------------------------------------------|
//! | `genre_frequent`  | `genre`                 | `/home/recommendations` : l'album est d'un des 5 genres les plus présents dans `listen_history`, et n'a jamais été écouté |
//! | `artiste_favori`  | `artiste`               | `/home/artist-releases` : l'artiste est en favori (local ou chez un service) |
//! | `artiste_possede` | `artiste`, `albums`     | `/home/artist-releases` : `albums` albums de l'artiste dans la bibliothèque |
//! | `autre_version`   | `titre`                 | `/home/other-versions` : une autre version de `titre`, écouté récemment |
//! | `ajout_recent`    | `ajoute_le` (s, option) | `/home/new-in-library`, `/home/recently-added` : la date d'AJOUT |
//!
//! Ce qu'on n'a PAS inventé, et pourquoi :
//!
//! - **« Parce que vous écoutez [artiste] »** (`artiste_ecoute`) : aucune
//!   rangée ne choisit aujourd'hui un élément à partir des artistes ÉCOUTÉS.
//!   Les nouveautés partent des favoris et de la bibliothèque, pas de
//!   l'historique ; le dire serait mentir.
//! - **Le repli au hasard** de `/home/recommendations` (`reason: "random"`) :
//!   le hasard n'est pas une raison. Pas de `raison` — la règle du chantier :
//!   une rangée qui ne peut pas justifier ses éléments n'en renvoie pas.
//!
//! Le champ historique `reason` (`"random"` / `"genre_match"`) reste tel quel
//! pour les clients déployés : il est lu comme une chaîne.
//!
//! # « Pas pour moi » : `/home/dismissed-suggestions`
//!
//! Par profil (`X-Profile-Id`), en base (`suggestions_ecartees`, migration
//! SQLite 125 / PG 089). Les rangées de RECOMMANDATION — recommandations,
//! nouveautés de vos artistes, autres versions — ne proposent plus ce que le
//! profil a écarté. Les rangées FACTUELLES (ajouts récents, reprendre
//! l'écoute) ne filtrent pas : un album ajouté hier reste ajouté hier.

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};
use tune_core::db::suggestions_ecartees::{Ecartees, SuggestionEcartee, SuggestionsEcarteesRepo};

use crate::error::AppError;
use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;

// ── Les raisons ────────────────────────────────────────────────────────────

/// L'album est d'un genre que l'historique compte parmi les plus écoutés.
pub(crate) fn raison_genre_frequent(genre: &str) -> Option<Value> {
    let g = genre.trim();
    (!g.is_empty()).then(|| json!({"type": "genre_frequent", "genre": g}))
}

/// L'artiste d'une nouveauté : favori d'abord, sinon possédé, sinon rien.
pub(crate) fn raison_artiste(artiste: &str, favori: bool, albums_possedes: i64) -> Option<Value> {
    let a = artiste.trim();
    if a.is_empty() {
        return None;
    }
    if favori {
        Some(json!({"type": "artiste_favori", "artiste": a}))
    } else if albums_possedes > 0 {
        Some(json!({"type": "artiste_possede", "artiste": a, "albums": albums_possedes}))
    } else {
        None
    }
}

/// Une autre version d'un titre écouté récemment.
pub(crate) fn raison_autre_version(titre: &str) -> Option<Value> {
    let t = titre.trim();
    (!t.is_empty()).then(|| json!({"type": "autre_version", "titre": t}))
}

/// Ajouté récemment ; `ajoute_le` en secondes depuis l'époque quand on l'a.
pub(crate) fn raison_ajout_recent(ajoute_le: Option<f64>) -> Value {
    match ajoute_le.filter(|s| s.is_finite() && *s > 0.0) {
        Some(s) => json!({"type": "ajout_recent", "ajoute_le": s.floor() as i64}),
        None => json!({"type": "ajout_recent"}),
    }
}

// ── Le filtre ──────────────────────────────────────────────────────────────

/// Ce que le profil qui regarde a écarté.
pub(crate) fn ecartees_du_profil(state: &AppState, profile: &ActiveProfile) -> Ecartees {
    SuggestionsEcarteesRepo::with_backend(state.backend.clone()).ensemble(profile.id())
}

/// Un album de recommandation (`id` local, `artist_name`) est-il écarté ?
pub(crate) fn album_local_ecarte(e: &Ecartees, album: &Value) -> bool {
    album["id"].as_i64().is_some_and(|id| e.album_local(id))
        || album["artist_name"].as_str().is_some_and(|a| e.artiste(a))
}

/// Épure les groupes d'« Autres versions » : versions locales et de service
/// écartées retirées, groupe d'un artiste écarté retiré, groupe vidé retiré.
/// Pose la `raison` de chaque groupe restant.
pub(crate) fn epurer_autres_versions(groupes: Vec<Value>, e: &Ecartees) -> Vec<Value> {
    groupes
        .into_iter()
        .filter_map(|mut g| {
            if g["artist_name"].as_str().is_some_and(|a| e.artiste(a)) {
                return None;
            }
            if !e.est_vide() {
                if let Some(v) = g["versions"].as_array_mut() {
                    v.retain(|x| !x["album_id"].as_i64().is_some_and(|id| e.album_local(id)));
                }
                if let Some(s) = g["streaming"].as_array_mut() {
                    s.retain(|x| {
                        let service = x["service"].as_str().unwrap_or_default();
                        let album = x["album_id"].as_str().unwrap_or_default();
                        let artiste = x["artist_name"].as_str().unwrap_or_default();
                        !(e.album_de_service(service, album) || e.artiste(artiste))
                    });
                }
                let vide = |k: &str| g[k].as_array().is_none_or(|a| a.is_empty());
                if vide("versions") && vide("streaming") {
                    return None;
                }
            }
            if let Some(r) = g["title"].as_str().and_then(raison_autre_version) {
                g["raison"] = r;
            }
            Some(g)
        })
        .collect()
}

// ── Les routes ─────────────────────────────────────────────────────────────

/// `GET /home/dismissed-suggestions` — ce que le profil qui agit a écarté,
/// le plus récent d'abord. La liste des Réglages (« Suggestions écartées »).
pub(crate) async fn lister(
    State(state): State<AppState>,
    profile: ActiveProfile,
) -> Result<Json<Value>, AppError> {
    let items = SuggestionsEcarteesRepo::with_backend(state.backend.clone())
        .lister(profile.id())
        .map_err(AppError::internal)?;
    Ok(Json(json!({
        "profile_id": profile.id(),
        "total": items.len(),
        "items": items,
    })))
}

/// `POST /home/dismissed-suggestions` — « Pas pour moi ». Corps :
/// `{item_type, source, item_id, title?, subtitle?, cover_url?}`. Rend la
/// désignation telle qu'elle est rangée (normalisée) : c'est elle que le
/// client renvoie pour annuler. 400 si elle ne désigne rien.
pub(crate) async fn ecarter(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<SuggestionEcartee>,
) -> Result<Json<Value>, AppError> {
    let repo = SuggestionsEcarteesRepo::with_backend(state.backend.clone());
    match repo
        .ecarter(profile.id(), &body)
        .map_err(AppError::internal)?
    {
        Some(rangee) => Ok(Json(json!({
            "profile_id": profile.id(),
            "dismissed": true,
            "item": rangee,
        }))),
        None => Err(AppError::bad_request(
            "item_type must be album, artist or playlist, with a usable item_id \
             (a library album id, a service source_id, or an artist name); \
             a playlist must come from a streaming service",
        )),
    }
}

/// `POST /home/dismissed-suggestions/restore` — « Annuler », ou le bouton
/// « Rétablir » des Réglages. Même corps que l'écart. Idempotent : rétablir
/// ce qui ne l'était pas rend `restored: false`.
pub(crate) async fn retablir(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<SuggestionEcartee>,
) -> Result<Json<Value>, AppError> {
    let restored = SuggestionsEcarteesRepo::with_backend(state.backend.clone())
        .retablir(profile.id(), &body)
        .map_err(AppError::internal)?;
    Ok(Json(json!({
        "profile_id": profile.id(),
        "restored": restored,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_hasard_et_le_vide_ne_sont_pas_des_raisons() {
        assert_eq!(raison_genre_frequent("  "), None);
        assert_eq!(raison_artiste("Miles Davis", false, 0), None);
        assert_eq!(raison_artiste("", true, 3), None);
        assert_eq!(raison_autre_version(""), None);
        assert_eq!(
            raison_ajout_recent(Some(f64::NAN)),
            json!({"type": "ajout_recent"})
        );
    }

    #[test]
    fn le_favori_passe_avant_la_possession() {
        assert_eq!(
            raison_artiste("Miles Davis", true, 5),
            Some(json!({"type": "artiste_favori", "artiste": "Miles Davis"}))
        );
        assert_eq!(
            raison_artiste("Miles Davis", false, 5),
            Some(json!({"type": "artiste_possede", "artiste": "Miles Davis", "albums": 5}))
        );
    }
}
