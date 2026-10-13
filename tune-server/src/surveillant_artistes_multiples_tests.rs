//! Chantier artistes multiples — le surveillant de fichiers relie la piste à
//! tous les artistes de son crédit ARTIST, comme le scan par lots, et
//! apparie les MBID quand la balise en porte un par artiste (Picard).
use super::reimporter_fichier_surveillant;
use super::surveillant_retouche_tests_4896::coffret_indexe;
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::ogg::VorbisComments;
use std::sync::Arc;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::DbBackend;
use tune_core::db::track_artist_repo::artistes_de_la_piste;
use tune_core::db::track_repo::TrackRepo;
use tune_core::scanner::watcher::{ChangeType, FileChange};

const MBID_BEATLES: &str = "b10bbbfc-cf9e-42e0-be17-e2c3e1d2600d";
const MBID_GENESIS: &str = "8e3fcd7d-bda1-4ca0-b987-b8528d2ee74e";

/// Comme `baliser`, mais une clé peut se RÉPÉTER (`push`, pas `insert`).
fn baliser_en_repetant(piste: &std::path::Path, tags: &[(&str, &str)]) {
    let mut fh = std::fs::File::open(piste).expect("ouverture");
    let mut flac = FlacFile::read_from(&mut fh, ParseOptions::new()).expect("lecture FLAC");
    drop(fh);
    let mut vc = VorbisComments::default();
    for (k, v) in tags {
        vc.push(k.to_string(), v.to_string());
    }
    flac.set_vorbis_comments(vc);
    flac.save_to_path(piste, WriteOptions::default())
        .expect("écriture des tags");
    // Une date neuve : le surveillant relit le fichier.
    std::fs::File::options()
        .write(true)
        .open(piste)
        .and_then(|f| f.set_modified(std::time::SystemTime::now()))
        .expect("date de modification");
}

/// Contre-épreuve : retirer l'écriture `remplacer_par_chemin` de
/// `reimporter_fichier_surveillant` fait tomber la première assertion.
#[test]
fn le_surveillant_relie_la_piste_a_tous_les_artistes_du_credit() {
    let dossier = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "artistes-multiples-surveillant",
    );
    let db =
        tune_core::db::sqlite::SqliteDb::open(&dossier.join("tune.db").to_string_lossy()).unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let (_racine, pistes) = coffret_indexe(&db, "artistes-multiples");
    let piste = pistes[0].clone();
    let chemin = piste.to_string_lossy().into_owned();
    let tracks = TrackRepo::with_backend(db.clone());
    let change = FileChange {
        change_type: ChangeType::Modified,
        path: chemin.clone(),
    };

    baliser_en_repetant(
        &piste,
        &[
            ("TITLE", "Duo"),
            ("ARTIST", "The Beatles & Genesis"),
            ("MUSICBRAINZ_ARTISTID", MBID_BEATLES),
            ("MUSICBRAINZ_ARTISTID", MBID_GENESIS),
        ],
    );
    reimporter_fichier_surveillant(&db, &change, true);
    let id = tracks.get_by_path(&chemin).unwrap().unwrap().id.unwrap();
    let relies = artistes_de_la_piste(db.as_ref(), id);
    let noms: Vec<&str> = relies.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(
        noms,
        ["The Beatles", "Genesis"],
        "liaison posée par le surveillant"
    );
    // Chaque nom a reçu SON MBID, dans l'ordre du crédit.
    let genesis = ArtistRepo::with_backend(db.clone())
        .get(relies[1].0)
        .unwrap()
        .unwrap();
    assert_eq!(genesis.musicbrainz_id.as_deref(), Some(MBID_GENESIS));

    // Le fichier est re-étiqueté avec un seul artiste : la liaison part.
    baliser_en_repetant(&piste, &[("TITLE", "Duo"), ("ARTIST", "The Beatles")]);
    reimporter_fichier_surveillant(&db, &change, true);
    assert!(artistes_de_la_piste(db.as_ref(), id).is_empty());
}
