//! #6132 étape 2b — la lecture groupée : ce que le meneur joue, les autres
//! membres du groupe le jouent.
//!
//! Les groupes vivent dans le réglage JSON `zone_groups` (route de
//! groupement, `tune-server/src/routes/zones/groupes.rs`). Ce module ne fait
//! que LIRE ce réglage et décider :
//!
//! - quelles zones le meneur ENTRAÎNE : tous les autres membres, sauf les
//!   zones locales déjà servies par une sortie double (elles reçoivent le flux
//!   du porteur, synchronisées) ;
//! - ce qu'une zone suiveuse doit jouer quand le meneur commence une piste
//!   (une requête de lecture tirée de ce que joue le meneur) ;
//! - comment une zone se présente : meneuse, suiveuse, synchronisée ou non.
//!
//! Le moteur qui applique ces décisions — abonné aux évènements de lecture
//! du meneur — vit côté serveur (`tune-server/src/lecture_groupee.rs`).

use serde_json::{Value, json};

use crate::db::zone_repo::Zone;
use crate::orchestrator::PlayRequest;
use crate::playback::NowPlaying;

/// Les groupes enregistrés (`zone_groups`), vide si absent ou illisible.
pub fn groupes_enregistres(db: &std::sync::Arc<dyn crate::db::backend::DbBackend>) -> Vec<Value> {
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn membres(groupe: &Value) -> Vec<i64> {
    groupe["zone_ids"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default()
}

fn meneur(groupe: &Value) -> Option<i64> {
    groupe["leader_id"]
        .as_i64()
        .or_else(|| membres(groupe).first().copied())
}

/// Les zones locales servies par la sortie double du groupe : elles jouent
/// par le porteur, on ne les entraîne pas séparément.
pub fn servies_par_la_sortie_double(groupe: &Value) -> Vec<i64> {
    groupe["sortie_double"]["suiveurs"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| s["zone_id"].as_i64()).collect())
        .unwrap_or_default()
}

/// La zone qui porte la sortie double du groupe : le meneur s'il est local,
/// sinon le premier membre local (meneur réseau).
pub fn porteur_de_la_sortie_double(groupe: &Value) -> Option<i64> {
    groupe["sortie_double"]["porteur_id"]
        .as_i64()
        .or_else(|| groupe.get("sortie_double").and_then(|_| meneur(groupe)))
}

/// Les zones que `meneur_id` entraîne, avec leur sortie : tous les membres de
/// ses groupes, sauf lui-même, les zones locales servies par une sortie
/// double et les zones sans sortie.
pub fn zones_entrainees(groupes: &[Value], meneur_id: i64, zones: &[Zone]) -> Vec<(i64, String)> {
    let mut entrainees: Vec<(i64, String)> = Vec::new();
    for groupe in groupes.iter().filter(|g| meneur(g) == Some(meneur_id)) {
        let servies = servies_par_la_sortie_double(groupe);
        for id in membres(groupe) {
            if id == meneur_id || servies.contains(&id) || entrainees.iter().any(|(e, _)| *e == id)
            {
                continue;
            }
            if let Some(sortie) = zones
                .iter()
                .find(|z| z.id == Some(id))
                .and_then(|z| z.output_device_id.clone())
                .filter(|d| !d.is_empty())
            {
                entrainees.push((id, sortie));
            }
        }
    }
    entrainees
}

/// La zone meneuse qui entraîne `zone_id`, si `zone_id` est membre (non
/// meneur) d'un groupe.
pub fn meneur_de(groupes: &[Value], zone_id: i64) -> Option<i64> {
    groupes
        .iter()
        .filter(|g| membres(g).contains(&zone_id))
        .filter_map(meneur)
        .find(|m| *m != zone_id)
}

/// Ce qu'une zone suiveuse joue quand le meneur commence `np` : la même
/// piste, à la même position, sur SA sortie.
pub fn requete_de_suiveuse(
    np: &NowPlaying,
    zone_id: i64,
    sortie: &str,
    position_ms: u64,
) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some(sortie.to_string()),
        track_id: np.track_id,
        source: Some(np.source.clone()),
        source_id: np.source_id.clone(),
        title: Some(np.title.clone()),
        artist_name: np.artist_name.clone(),
        album_title: np.album_title.clone(),
        cover_url: np.cover_path.clone(),
        duration_ms: Some(np.duration_ms),
        seek_ms: (position_ms > 0).then_some(position_ms),
        ..Default::default()
    }
}

/// Ce que la charge utile d'une zone dit de sa place dans un groupe de
/// lecture : `None` hors de tout groupe.
///
/// `role` : `"meneur"` (ce que la zone joue, les autres le jouent) ou
/// `"suiveuse"` (elle joue ce que joue `meneur_id`). `synchronisee` : la zone
/// est servie par une sortie double (horloges asservies, latences
/// compensées), ou la porte ; sinon elle suit le meneur en renderer
/// indépendant, départs alignés au mieux.
pub fn place_de_la_zone(groupes: &[Value], zone_id: i64) -> Option<Value> {
    let groupe = groupes.iter().find(|g| membres(g).contains(&zone_id))?;
    let meneur_id = meneur(groupe)?;
    let synchronisee = servies_par_la_sortie_double(groupe).contains(&zone_id)
        || porteur_de_la_sortie_double(groupe) == Some(zone_id);
    Some(json!({
        "group_id": groupe["group_id"]
            .as_str()
            .map(str::to_string)
            .or_else(|| groupe["id"].as_i64().map(|i| i.to_string())),
        "nom": groupe["name"],
        "meneur_id": meneur_id,
        "role": if meneur_id == zone_id { "meneur" } else { "suiveuse" },
        "synchronisee": synchronisee,
    }))
}

#[cfg(test)]
#[path = "groupe_de_lecture_tests.rs"]
mod tests;
