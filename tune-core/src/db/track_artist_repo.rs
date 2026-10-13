//! Chantier « artistes multiples » — la liaison piste ↔ artistes
//! (`track_artists`, migration SQLite 125 / PG 089).
//!
//! « The Beatles & Genesis » compte pour les deux artistes. `tracks.artist_id`
//! reste l'artiste PRINCIPAL de la piste, inchangé ; la table porte, à côté,
//! TOUS les artistes du crédit ARTIST quand le scan l'a découpé
//! (`artist_split::artistes_de_la_piste`). Une piste dont le crédit ne se
//! découpe pas n'y a aucune ligne : son seul artiste est `tracks.artist_id`.
//!
//! Voir `docs/chantier-artistes-multiples.md`.
use std::sync::Arc;

use crate::db::artist_repo::ArtistRepo;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use tracing::warn;

fn ph(db: &dyn DbBackend, n: usize) -> String {
    match db.engine() {
        Engine::Sqlite => SqliteDialect.placeholder(n),
        Engine::Postgres => PostgresDialect.placeholder(n),
    }
}

/// Remplace les artistes reliés à la piste de `file_path` par `artistes`
/// (`(artist_id, rang)`, dans l'ordre du crédit). Une liste de moins de deux
/// artistes ne pose aucune ligne : elle retire seulement les anciennes (un
/// crédit qui ne se découpe plus). Une piste absente de `tracks` (écartée
/// comme exemplaire, échec d'insertion) ne reçoit rien.
///
/// Rend le nombre de lignes posées.
pub fn remplacer_par_chemin(db: &dyn DbBackend, file_path: &str, artistes: &[(i64, u32)]) -> usize {
    let retrait = format!(
        "DELETE FROM track_artists WHERE track_id IN (SELECT id FROM tracks WHERE file_path = {})",
        ph(db, 1)
    );
    let params: [&dyn ToSqlValue; 1] = [&file_path];
    if let Err(e) = db.execute(&retrait, &params) {
        warn!(chemin = %file_path, error = %e, "track_artists_retrait_echec");
        return 0;
    }
    if artistes.len() < 2 {
        return 0;
    }
    let pose = format!(
        "INSERT INTO track_artists (track_id, artist_id, rang) \
         SELECT id, {}, {} FROM tracks WHERE file_path = {} \
         ON CONFLICT (track_id, artist_id) DO NOTHING",
        ph(db, 1),
        ph(db, 2),
        ph(db, 3)
    );
    artistes
        .iter()
        .map(|(artist_id, rang)| {
            let rang = i64::from(*rang);
            let params: [&dyn ToSqlValue; 3] = [artist_id, &rang, &file_path];
            db.execute(&pose, &params).unwrap_or_else(|e| {
                warn!(chemin = %file_path, artist_id, error = %e, "track_artists_pose_echec");
                0
            })
        })
        .sum()
}

/// La liste d'exceptions du découpage : le réglage `artist_split_allowlist`
/// (tableau JSON), vide s'il est absent ou illisible. À lire une fois par
/// passe.
pub fn exceptions_du_reglage(db: &Arc<dyn DbBackend>) -> Vec<String> {
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get("artist_split_allowlist")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

/// Les artistes d'un crédit ARTIST, résolus en `(artist_id, rang)` : le
/// découpage du scan (`artist_split::artistes_de_la_piste` : séparateurs
/// sûrs, liste d'exceptions, MBID). Vide quand le crédit ne se découpe pas.
///
/// Un seul chemin pour tous ceux qui écrivent une piste : le scan par lots,
/// le surveillant de fichiers, les feuilles CUE et les imports.
///
/// - `mbid_brut` : la balise `MUSICBRAINZ_ARTISTID`, toutes valeurs jointes.
/// - `principal` : l'artiste déjà résolu pour la piste, `(id, nom)` ; un nom
///   découpé qui lui est égal reprend son identifiant.
/// - `resoudre(nom, mbid)` : l'identifiant d'un artiste du crédit (le scan y
///   met son cache) ; [`resoudre_par_la_base`] sinon.
pub fn liens_du_credit(
    artist_repo: &ArtistRepo,
    exceptions: &[String],
    credit: &str,
    mbid_brut: Option<&str>,
    principal: Option<(i64, &str)>,
    resoudre: &mut dyn FnMut(&str, Option<&str>) -> Option<i64>,
) -> Vec<(i64, u32)> {
    use crate::metadata::artist_split::{artistes_de_la_piste, mbids_de_la_balise};
    let mbids = mbids_de_la_balise(mbid_brut);
    let nom_du_mbid = match mbids.as_slice() {
        [seul] => artist_repo
            .get_by_musicbrainz_id(seul)
            .ok()
            .flatten()
            .map(|a| a.name),
        _ => None,
    };
    let artistes = artistes_de_la_piste(credit, &mbids, nom_du_mbid.as_deref(), exceptions);
    let mut liens: Vec<(i64, u32)> = Vec::new();
    if artistes.len() < 2 {
        return liens;
    }
    for a in &artistes {
        let id = match principal {
            Some((id, nom)) if nom.eq_ignore_ascii_case(&a.nom) => Some(id),
            _ => resoudre(&a.nom, a.mbid.as_deref()),
        };
        if let Some(id) = id
            && !liens.iter().any(|(deja, _)| *deja == id)
        {
            liens.push((id, a.rang));
        }
    }
    liens
}

/// Le résolveur par défaut de [`liens_du_credit`] : `get_or_create` (MBID
/// d'abord, puis nom).
pub fn resoudre_par_la_base(
    artist_repo: &ArtistRepo,
) -> impl FnMut(&str, Option<&str>) -> Option<i64> + '_ {
    move |nom, mbid| artist_repo.get_or_create(nom, mbid, None).ok()?.id
}

/// Comme [`remplacer_par_chemin`], pour une piste désignée par son
/// identifiant : une tranche CUE n'a pas de chemin à elle, et un import
/// connaît l'identifiant qu'il vient de créer.
pub fn remplacer_par_id(db: &dyn DbBackend, track_id: i64, artistes: &[(i64, u32)]) -> usize {
    retirer_de_la_piste(db, track_id);
    if artistes.len() < 2 {
        return 0;
    }
    let pose = format!(
        "INSERT INTO track_artists (track_id, artist_id, rang) VALUES ({}, {}, {}) \
         ON CONFLICT (track_id, artist_id) DO NOTHING",
        ph(db, 1),
        ph(db, 2),
        ph(db, 3)
    );
    artistes
        .iter()
        .map(|(artist_id, rang)| {
            let rang = i64::from(*rang);
            let params: [&dyn ToSqlValue; 3] = [&track_id, artist_id, &rang];
            db.execute(&pose, &params).unwrap_or_else(|e| {
                warn!(track_id, artist_id, error = %e, "track_artists_pose_echec");
                0
            })
        })
        .sum()
}

/// Retire les artistes reliés à la piste `track_id` : l'utilisateur vient de
/// lui donner un artiste à la main. Elle garde l'artiste qu'on lui a donné,
/// le découpage des balises ne la relie plus à personne d'autre (le scan suit
/// la même règle, `Tenues::tient_l_artiste`).
///
/// Rend le nombre de lignes retirées ; une erreur est journalisée.
pub fn retirer_de_la_piste(db: &dyn DbBackend, track_id: i64) -> usize {
    let sql = format!("DELETE FROM track_artists WHERE track_id = {}", ph(db, 1));
    let params: [&dyn ToSqlValue; 1] = [&track_id];
    db.execute(&sql, &params).unwrap_or_else(|e| {
        warn!(track_id, error = %e, "track_artists_retrait_echec");
        0
    })
}

/// Les artistes reliés à une piste, dans l'ordre du crédit : `(artist_id,
/// nom)`. Vide pour une piste à un seul artiste.
pub fn artistes_de_la_piste(db: &dyn DbBackend, track_id: i64) -> Vec<(i64, String)> {
    let sql = format!(
        "SELECT a.id, a.name FROM track_artists ta JOIN artists a ON a.id = ta.artist_id \
         WHERE ta.track_id = {} ORDER BY ta.rang",
        ph(db, 1)
    );
    let params: [&dyn ToSqlValue; 1] = [&track_id];
    db.query_many(&sql, &params)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| Some((row.first()?.as_i64()?, row.get(1)?.as_str()?.to_string())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations::run_migrations;
    use crate::db::sqlite::SqliteDb;

    fn base() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        db
    }

    fn artiste(db: &SqliteDb, nom: &str) -> i64 {
        db.execute("INSERT INTO artists (name) VALUES (?1)", &[&nom])
            .unwrap();
        db.last_insert_rowid()
    }

    fn piste(db: &SqliteDb, chemin: &str, artist_id: i64) -> i64 {
        db.execute(
            "INSERT INTO tracks (title, artist_id, file_path) VALUES ('t', ?1, ?2)",
            &[&artist_id, &chemin],
        )
        .unwrap();
        db.last_insert_rowid()
    }

    #[test]
    fn la_liaison_se_pose_se_remplace_et_suit_la_piste() {
        let db = base();
        let beatles = artiste(&db, "The Beatles");
        let genesis = artiste(&db, "Genesis");
        let id = piste(&db, "/m/a.flac", beatles);

        assert_eq!(
            remplacer_par_chemin(&db, "/m/a.flac", &[(beatles, 0), (genesis, 1)]),
            2
        );
        assert_eq!(
            artistes_de_la_piste(&db, id),
            vec![(beatles, "The Beatles".into()), (genesis, "Genesis".into())]
        );
        // Relu : même résultat, pas de doublon.
        assert_eq!(
            remplacer_par_chemin(&db, "/m/a.flac", &[(beatles, 0), (genesis, 1)]),
            2
        );
        assert_eq!(artistes_de_la_piste(&db, id).len(), 2);
        // Le crédit ne se découpe plus : les lignes partent.
        assert_eq!(remplacer_par_chemin(&db, "/m/a.flac", &[(beatles, 0)]), 0);
        assert!(artistes_de_la_piste(&db, id).is_empty());
        // Un chemin inconnu ne pose rien et n'échoue pas.
        assert_eq!(
            remplacer_par_chemin(&db, "/m/absent.flac", &[(beatles, 0), (genesis, 1)]),
            0
        );
        // La suppression de la piste emporte sa liaison (ON DELETE CASCADE).
        remplacer_par_chemin(&db, "/m/a.flac", &[(beatles, 0), (genesis, 1)]);
        db.execute("DELETE FROM tracks WHERE id = ?1", &[&id])
            .unwrap();
        let reste = db
            .query_one("SELECT COUNT(*) FROM track_artists", &[])
            .unwrap()
            .unwrap()[0]
            .as_i64();
        assert_eq!(reste, Some(0));
    }

    #[test]
    fn l_artiste_donne_a_la_main_retire_la_liaison() {
        let db = base();
        let beatles = artiste(&db, "The Beatles");
        let genesis = artiste(&db, "Genesis");
        let id = piste(&db, "/m/a.flac", beatles);
        let autre = piste(&db, "/m/b.flac", beatles);
        remplacer_par_chemin(&db, "/m/a.flac", &[(beatles, 0), (genesis, 1)]);
        remplacer_par_chemin(&db, "/m/b.flac", &[(beatles, 0), (genesis, 1)]);
        assert_eq!(retirer_de_la_piste(&db, id), 2);
        assert!(artistes_de_la_piste(&db, id).is_empty());
        // L'autre piste garde la sienne.
        assert_eq!(artistes_de_la_piste(&db, autre).len(), 2);
        assert_eq!(retirer_de_la_piste(&db, id), 0);
    }

    #[test]
    fn le_credit_se_resout_et_se_pose_par_identifiant() {
        let sqlite = base();
        let beatles = artiste(&sqlite, "The Beatles");
        let id = piste(&sqlite, "/m/a.flac", beatles);
        let db: Arc<dyn DbBackend> = Arc::new(sqlite);
        let repo = ArtistRepo::with_backend(db.clone());
        let mut resoudre = resoudre_par_la_base(&repo);
        let liens = liens_du_credit(
            &repo,
            &[],
            "The Beatles & Genesis",
            None,
            Some((beatles, "The Beatles")),
            &mut resoudre,
        );
        assert_eq!(liens.len(), 2);
        assert_eq!(liens[0], (beatles, 0), "le principal garde son identifiant");
        assert_eq!(remplacer_par_id(db.as_ref(), id, &liens), 2);
        let noms: Vec<String> = artistes_de_la_piste(db.as_ref(), id)
            .into_iter()
            .map(|(_, n)| n)
            .collect();
        assert_eq!(noms, ["The Beatles", "Genesis"]);
        // Un crédit qui ne se découpe pas : rien, et les lignes partent.
        let seul = liens_du_credit(&repo, &[], "Simon & Garfunkel", None, None, &mut resoudre);
        assert!(seul.is_empty());
        assert_eq!(remplacer_par_id(db.as_ref(), id, &seul), 0);
        assert!(artistes_de_la_piste(db.as_ref(), id).is_empty());
        // Une exception du réglage n'est pas découpée.
        let exc = vec!["The Beatles & Genesis".to_string()];
        assert!(
            liens_du_credit(
                &repo,
                &exc,
                "The Beatles & Genesis",
                None,
                None,
                &mut resoudre
            )
            .is_empty()
        );
    }

    /// La page d'un artiste : ses pistes, plus celles où il est relié.
    #[test]
    fn la_page_d_un_artiste_liste_aussi_les_pistes_reliees() {
        let sqlite = base();
        let beatles = artiste(&sqlite, "The Beatles");
        let genesis = artiste(&sqlite, "Genesis");
        let duo = piste(&sqlite, "/m/a.flac", beatles);
        let seule = piste(&sqlite, "/m/b.flac", genesis);
        remplacer_par_chemin(&sqlite, "/m/a.flac", &[(beatles, 0), (genesis, 1)]);
        let db: Arc<dyn DbBackend> = Arc::new(sqlite);
        let repo = crate::db::track_repo::TrackRepo::with_backend(db);
        let ids = |a: i64| -> Vec<i64> {
            let mut v: Vec<i64> = repo
                .list_by_artist_et_relies(a)
                .unwrap()
                .into_iter()
                .filter_map(|t| t.id)
                .collect();
            v.sort();
            v
        };
        assert_eq!(ids(genesis), vec![duo, seule]);
        assert_eq!(ids(beatles), vec![duo], "pas de doublon pour le principal");
        // L'ancienne liste, elle, ne bouge pas.
        assert_eq!(repo.list_by_artist(genesis).unwrap().len(), 1);
    }

    /// Un artiste qui n'est relié QUE par la liaison n'est pas un orphelin :
    /// la purge (#5007) ne doit pas le supprimer.
    #[test]
    fn un_artiste_relie_seulement_par_la_liaison_n_est_pas_orphelin() {
        let db = base();
        let beatles = artiste(&db, "The Beatles");
        let genesis = artiste(&db, "Genesis");
        piste(&db, "/m/a.flac", beatles);
        remplacer_par_chemin(&db, "/m/a.flac", &[(beatles, 0), (genesis, 1)]);
        let orphelins = db
            .query_one(crate::db::artist_repo::sql::count_orphans(), &[])
            .unwrap()
            .unwrap()[0]
            .as_i64();
        assert_eq!(orphelins, Some(0), "Genesis est relié à une piste");
    }
}
