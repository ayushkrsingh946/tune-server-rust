//! Mesure A de #4805 : `identify-all` rejoué, tel quel, sur une COPIE de
//! bibliothèque — jamais sur une base en service.
//!
//! C'est la boucle de la route ([`super::executer_le_lot`]) : même sélection,
//! même cadence MusicBrainz, même disjoncteur, mêmes écritures. Seules la
//! garde Premium et la réponse HTTP sont sautées : la mesure porte sur ce que
//! la passe identifie, pas sur le droit de la lancer.
//!
//! ```text
//! HOME=<dossier de mesure> TUNE_MESURE_A_DB=<copie>.db \
//!   TUNE_MESURE_A_SORTIE=<résultat>.json [TUNE_MESURE_A_LIMITE=N] \
//!   <binaire de test> --ignored --exact \
//!   routes::library::identification_lot::mesure_a::mesure_a_sur_une_copie
//! ```
//!
//! Refus d'ouvrir un chemin qui ressemble à une base en service (`/opt/`,
//! `/var/lib/`, `/srv/tune/`) : la copie se fait AVANT, à la main.

use std::time::Instant;

use serde_json::{Value, json};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

use super::{CLE_ETAT, executer_le_lot, sql_candidats_identification};
use crate::state::AppState;

fn compter(b: &std::sync::Arc<dyn DbBackend>, sql: &str) -> i64 {
    b.query_one(sql, &[])
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1)
}

/// Le relevé, avant et après : les chiffres du §1 de #4805.
fn releve(b: &std::sync::Arc<dyn DbBackend>) -> Value {
    const LOCAL_AL: &str = "COALESCE(source, 'local') = 'local'";
    json!({
        "albums_locaux": compter(b, &format!("SELECT COUNT(*) FROM albums WHERE {LOCAL_AL}")),
        "albums_locaux_avec_release": compter(b, &format!(
            "SELECT COUNT(*) FROM albums WHERE {LOCAL_AL} \
             AND musicbrainz_release_id IS NOT NULL AND musicbrainz_release_id <> ''")),
        "albums_locaux_avec_release_group": compter(b, &format!(
            "SELECT COUNT(*) FROM albums WHERE {LOCAL_AL} \
             AND musicbrainz_release_group_id IS NOT NULL AND musicbrainz_release_group_id <> ''")),
        "albums_marques_tentes": compter(b,
            "SELECT COUNT(*) FROM albums WHERE identification_tentee_le IS NOT NULL"),
        "pistes_locales": compter(b, &format!("SELECT COUNT(*) FROM tracks WHERE {LOCAL_AL}")),
        "pistes_locales_avec_recording": compter(b, &format!(
            "SELECT COUNT(*) FROM tracks WHERE {LOCAL_AL} \
             AND musicbrainz_recording_id IS NOT NULL AND musicbrainz_recording_id <> ''")),
        "artistes_avec_mbid": compter(b,
            "SELECT COUNT(*) FROM artists WHERE musicbrainz_id IS NOT NULL AND musicbrainz_id <> ''"),
        "artistes": compter(b, "SELECT COUNT(*) FROM artists"),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "mesure réseau de plusieurs heures, sur une copie : TUNE_MESURE_A_DB"]
async fn mesure_a_sur_une_copie() {
    let chemin = std::env::var("TUNE_MESURE_A_DB").expect("TUNE_MESURE_A_DB : la COPIE à mesurer");
    for interdit in ["/opt/", "/var/lib/", "/srv/tune/"] {
        assert!(
            !chemin.starts_with(interdit),
            "{chemin} ressemble à une base en service : copiez-la d'abord"
        );
    }
    let sortie = std::env::var("TUNE_MESURE_A_SORTIE").expect("TUNE_MESURE_A_SORTIE");
    let limite: Option<usize> = std::env::var("TUNE_MESURE_A_LIMITE")
        .ok()
        .and_then(|v| v.parse().ok());

    let state = AppState::new(&chemin, 0, Default::default()).expect("ouverture de la copie");
    let avant = releve(&state.backend);
    let mut albums: Vec<i64> = state
        .backend
        .query_many(sql_candidats_identification(), &[])
        .expect("sélection")
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect();
    let candidats = albums.len();
    if let Some(n) = limite {
        albums.truncate(n);
    }
    let lances = albums.len();
    eprintln!("mesure_a : {candidats} candidats, {lances} lancés");

    let debut = Instant::now();
    executer_le_lot(state.clone(), "mesure-a".into(), albums).await;
    let duree_s = debut.elapsed().as_secs_f64();

    let apres = releve(&state.backend);
    let etat: Value = SettingsRepo::with_backend(state.backend.clone())
        .get(CLE_ETAT)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let resultat = json!({
        "candidats": candidats,
        "lances": lances,
        "duree_s": duree_s.round(),
        "secondes_par_album": if lances > 0 { duree_s / lances as f64 } else { 0.0 },
        "avant": avant,
        "apres": apres,
        "etat_de_la_passe": etat,
    });
    std::fs::write(&sortie, serde_json::to_string_pretty(&resultat).unwrap()).unwrap();
    eprintln!("{resultat:#}");
}
