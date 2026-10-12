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
