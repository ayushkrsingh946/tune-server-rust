//! #6132 étape 2b — la lecture groupée, côté moteur : ce que le meneur fait,
//! les zones qu'il entraîne le font.
//!
//! Abonné aux évènements de lecture (`PlaybackManager::subscribe`), ce fil
//! regarde chaque évènement d'une zone MENEUSE et le rejoue sur les zones
//! qu'elle entraîne (`tune_core::zones::groupe_de_lecture::zones_entrainees`) :
//!
//! | évènement du meneur        | sur chaque zone entraînée                   |
//! |----------------------------|---------------------------------------------|
//! | `started`, `track_changed` | la même piste, à la même position (`play`)  |
//! | `paused`                   | `pause`                                     |
//! | `resumed`                  | `resume`                                    |
//! | `stopped`                  | `stop`                                      |
//! | `seek`                     | `seek` à la position du meneur              |
//!
//! Écouter les évènements plutôt que modifier chaque chemin de lecture : un
//! meneur commence une piste par une dizaine d'entrées (route, file, sondeur
//! en fin de piste, enchaînement sans blanc, réveil, assistant…) et TOUTES
//! finissent par ces évènements. Une zone suiveuse n'entraîne personne : pas
//! de boucle.
//!
//! Les zones locales servies par une sortie double ne sont pas entraînées :
//! elles jouent déjà le flux de leur porteur, synchronisées. Les autres
//! (renderers réseau, porteur local d'un meneur réseau) suivent en renderers
//! indépendants : départs alignés au mieux, sans garantie d'horloge — la
//! charge utile de la zone le dit (`groupe_de_lecture.synchronisee`).

use tracing::{debug, info, warn};
use tune_core::orchestrator::PlayRequest;
use tune_core::playback::{PlaybackEvent, ZoneState};
use tune_core::zones::groupe_de_lecture::{
    groupes_enregistres, requete_de_suiveuse, zones_entrainees,
};

use crate::state::AppState;

/// Ce qu'une zone entraînée doit faire.
#[derive(Debug, Clone)]
pub(crate) enum Commande {
    Jouer(Box<PlayRequest>),
    Pause,
    Reprise,
    Arret,
    Saut(u64),
}

/// En deçà, une position de départ vaut zéro : une piste qui commence ne doit
/// pas faire sauter les suiveuses à la seconde où le meneur a été relevé.
const POSITION_DE_DEPART_MS: i64 = 2_000;

/// La décision, sans orchestrateur : pour un évènement du meneur et son état,
/// la commande de chaque zone entraînée.
pub(crate) fn commandes_pour(
    evenement: &str,
    meneur: &ZoneState,
    entrainees: &[(i64, String)],
) -> Vec<(i64, String, Commande)> {
    let position = u64::try_from(meneur.position_ms.max(0)).unwrap_or(0);
    let commande = |zone_id: i64, sortie: &str| -> Option<Commande> {
        match evenement {
            "started" | "track_changed" => {
                let np = meneur.now_playing.as_ref()?;
                let depart = if meneur.position_ms > POSITION_DE_DEPART_MS {
                    position
                } else {
                    0
                };
                Some(Commande::Jouer(Box::new(requete_de_suiveuse(
                    np, zone_id, sortie, depart,
                ))))
            }
            "paused" => Some(Commande::Pause),
            "resumed" => Some(Commande::Reprise),
            "stopped" => Some(Commande::Arret),
            "seek" => Some(Commande::Saut(position)),
            _ => None,
        }
    };
    entrainees
        .iter()
        .filter_map(|(id, sortie)| commande(*id, sortie).map(|c| (*id, sortie.clone(), c)))
        .collect()
}

/// Appliquer une commande à une zone entraînée.
async fn appliquer(state: &AppState, zone_id: i64, sortie: &str, commande: Commande) {
    let orchestrateur = &state.orchestrator;
    let resultat: Result<(), String> = match commande {
        Commande::Jouer(req) => orchestrateur.play(*req).await.map(|_| ()),
        Commande::Pause => orchestrateur
            .pause(zone_id, Some(sortie))
            .await
            .map_err(|e| e.to_string()),
        Commande::Reprise => orchestrateur
            .resume(zone_id, Some(sortie))
            .await
            .map_err(|e| e.to_string()),
        Commande::Arret => {
            orchestrateur.stop(zone_id, Some(sortie)).await;
            Ok(())
        }
        Commande::Saut(position) => orchestrateur
            .seek(zone_id, position, Some(sortie))
            .await
            .map_err(|e| e.to_string()),
    };
    if let Err(erreur) = resultat {
        warn!(zone_id, sortie, %erreur, "lecture_groupee_suiveuse_en_echec");
    }
}

/// Un évènement de lecture : s'il vient d'un meneur, le rejouer sur les zones
/// qu'il entraîne. Rend le nombre de commandes envoyées.
pub(crate) async fn traiter(state: &AppState, evenement: &PlaybackEvent) -> usize {
    if !matches!(
        evenement.event.as_str(),
        "started" | "track_changed" | "paused" | "resumed" | "stopped" | "seek"
    ) {
        return 0;
    }
    let groupes = groupes_enregistres(&state.backend);
    if groupes.is_empty() {
        return 0;
    }
    let zones = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .list()
        .unwrap_or_default();
    let entrainees = zones_entrainees(&groupes, evenement.zone_id, &zones);
    if entrainees.is_empty() {
        return 0;
    }
    let meneur = state.playback.get_state(evenement.zone_id).await;
    let commandes = commandes_pour(&evenement.event, &meneur, &entrainees);
    let n = commandes.len();
    debug!(
        meneur = evenement.zone_id,
        evenement = %evenement.event,
        suiveuses = n,
        "lecture_groupee_entraine"
    );
    // Toutes ensemble : les départs restent aussi proches que possible.
    let envois = commandes
        .into_iter()
        .map(|(zone_id, sortie, commande)| async move {
            appliquer(state, zone_id, &sortie, commande).await
        });
    futures_util::future::join_all(envois).await;
    n
}

/// Lancer le fil de la lecture groupée.
pub fn spawn_lecture_groupee(state: &AppState) {
    let state = state.clone();
    let mut evenements = state.playback.subscribe();
    tokio::spawn(async move {
        info!("lecture_groupee_demarree");
        loop {
            match evenements.recv().await {
                // Un évènement après l'autre : `started` puis `paused` doivent
                // arriver aux suiveuses dans cet ordre. Les suiveuses d'un
                // même évènement, elles, sont servies ensemble.
                Ok(evenement) => {
                    traiter(&state, &evenement).await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    warn!(perdus = n, "lecture_groupee_evenements_perdus");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
#[path = "lecture_groupee_tests.rs"]
mod tests;
