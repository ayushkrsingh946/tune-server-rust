//! Témoins de #5593 — le périmètre des analyses de fond.
//!
//! Deux étages. Les clauses d'abord, sur leur texte et sur une vraie base :
//! préfixe borné par le séparateur, les deux séparateurs (y compris à
//! l'intérieur de la racine), l'apostrophe d'un nom de dossier. Puis les PASSES elles-mêmes : une piste
//! d'une racine exclue, posée sur un vrai fichier décodable, n'est ni
//! sélectionnée — donc jamais décodée — ni comptée, et la même piste redevient
//! du travail dès qu'on lève l'exclusion (contre-épreuve dans le même test).
use std::sync::Arc;

use super::*;
use crate::audio::embedding_store;
use crate::audio::replaygain;
use crate::db::backend::DbBackend;
use crate::db::sqlite::SqliteDb;
use crate::db::track_metadata_repo::TrackMetadataRepo;

fn base() -> (SqliteDb, Arc<dyn DbBackend>) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db.execute("INSERT INTO artists (id, name) VALUES (1, 'Tades')", &[])
        .unwrap();
    db.execute(
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Disque', 1)",
        &[],
    )
    .unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
    (db, backend)
}

fn piste(db: &SqliteDb, id: i64, chemin: &str) {
    db.execute(
        "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, \
         sample_rate, channels) VALUES (?, 'T', 1, 1, ?, 1000, 44100, 2)",
        &[&id, &chemin],
    )
    .unwrap();
}

fn regler(backend: &Arc<dyn DbBackend>, cle: &str, valeur: serde_json::Value) {
    SettingsRepo::with_backend(backend.clone())
        .set(cle, &valeur.to_string())
        .unwrap();
}

fn ids(backend: &Arc<dyn DbBackend>, clause: &str) -> Vec<i64> {
    backend
        .query_many(
            &format!("SELECT t.id FROM tracks t WHERE 1 = 1{clause} ORDER BY t.id"),
            &[],
        )
        .unwrap()
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect()
}

// ── Les clauses ─────────────────────────────────────────────────────────

#[test]
fn listes_vides_aucune_clause() {
    assert_eq!(clause_hors_racines("t.file_path", &[]), "");
    let (_db, backend) = base();
    assert_eq!(
        clause_decodage(&backend, Passe::ReplayGain),
        "",
        "rien de réglé : rien d'ajouté"
    );
    assert_eq!(clause_clap(&backend), "");
}

/// Exclure `/music` n'exclut ni `/music2` ni `/musicale` : le préfixe porte le
/// séparateur. La racine elle-même, avec ou sans séparateur final, et les deux
/// séparateurs d'une base Windows sont exclus.
#[test]
fn la_racine_est_un_prefixe_borne_par_le_separateur() {
    let (db, backend) = base();
    piste(&db, 1, "/music/a.flac");
    piste(&db, 2, "/music2/b.flac");
    piste(&db, 3, "/musicale/c.flac");
    piste(&db, 4, "/music/sous/d.flac");
    piste(&db, 5, r"Z:\NAS\e.flac");
    piste(&db, 6, "Z:/NAS/f.flac");
    piste(&db, 7, r"Z:\NASbis\g.flac");
    let c = clause_hors_racines("t.file_path", &["/music/".into(), r"Z:\NAS".into()]);
    assert_eq!(ids(&backend, &c), vec![2, 3, 7], "{c}");
}

/// Le séparateur compte aussi À L'INTÉRIEUR de la racine : le partage réseau
/// de Tades, réglé `\\192.168.0.126\musique`, doit exclure ses pistes quelle
/// que soit la graphie qu'elles ont en base. Et la racine réglée avec des `/`
/// exclut les chemins écrits avec des `\`.
#[test]
fn les_separateurs_interieurs_de_la_racine_sont_normalises() {
    let (db, backend) = base();
    piste(&db, 1, r"\\192.168.0.126\musique\Jazz\a.flac");
    piste(&db, 2, "//192.168.0.126/musique/Jazz/b.flac");
    piste(&db, 3, r"\\192.168.0.126/musique\c.flac");
    piste(&db, 4, r"\\192.168.0.126\musiques\d.flac");
    piste(&db, 5, r"Y:\local\e.flac");
    piste(&db, 6, r"W:\Sous\Dossier\f.flac");
    let c = clause_hors_racines(
        "t.file_path",
        &[r"\\192.168.0.126\musique".into(), "W:/Sous/Dossier/".into()],
    );
    assert_eq!(ids(&backend, &c), vec![4, 5], "{c}");
}

/// `/` comme racine : tout chemin absolu.
#[test]
fn la_racine_slash_exclut_tout_chemin_absolu() {
    let (db, backend) = base();
    piste(&db, 1, "/a.flac");
    piste(&db, 2, "relatif/b.flac");
    let c = clause_hors_racines("t.file_path", &["/".into()]);
    assert_eq!(ids(&backend, &c), vec![2], "{c}");
}

/// Une apostrophe dans un nom de dossier ne casse pas la requête, et un `%` ou
/// un `_` n'y est pas un joker.
#[test]
fn apostrophe_et_jokers_sont_des_caracteres_ordinaires() {
    let (db, backend) = base();
    piste(&db, 1, "/mnt/l'été/a.flac");
    piste(&db, 2, "/mnt/100%_x/b.flac");
    piste(&db, 3, "/mnt/100ab_x/c.flac");
    let c = clause_hors_racines("t.file_path", &["/mnt/l'été".into(), "/mnt/100%_x".into()]);
    assert_eq!(ids(&backend, &c), vec![3], "{c}");
}

#[test]
fn normaliser_rogne_dedoublonne_et_refuse_le_reste() {
    use serde_json::json;
    assert_eq!(
        normaliser(&json!([" a ", "", "a", "b"])).unwrap(),
        vec!["a".to_string(), "b".to_string()]
    );
    assert_eq!(normaliser(&json!(null)).unwrap(), Vec::<String>::new());
    assert!(normaliser(&json!("a")).is_err());
    assert!(normaliser(&json!([1])).is_err());
    let trop: Vec<String> = (0..=ENTREES_MAX).map(|i| i.to_string()).collect();
    assert!(normaliser(&json!(trop)).is_err());
}

/// Une valeur illisible en base se relit « rien d'exclu », jamais une panne.
#[test]
fn une_valeur_illisible_en_base_vaut_rien_d_exclu() {
    let (_db, backend) = base();
    SettingsRepo::with_backend(backend.clone())
        .set(CLE_RACINES_EXCLUES, "pas du json")
        .unwrap();
    assert!(racines_exclues(&backend).is_empty());
    assert_eq!(clause_decodage(&backend, Passe::ReplayGain), "");
}

// ── Les compteurs ───────────────────────────────────────────────────────

/// Les compteurs des quatre passes ne comptent plus une piste exclue — et la
/// comptent de nouveau dès que l'exclusion est levée.
#[test]
fn les_compteurs_excluent_la_racine_reglee() {
    let (db, backend) = base();
    SettingsRepo::with_backend(backend.clone())
        .set(replaygain::MODE_KEY, "track")
        .unwrap();
    piste(&db, 1, "/local/a.flac");
    piste(&db, 2, "/nas/musique/b.flac");
    piste(&db, 3, "/nas/musique/c.flac");
    let meta = TrackMetadataRepo::new(db.clone());
    // Le témoin que le rattrapage des empreintes et de la plage dynamique
    // exigent quand le ReplayGain est armé : posé AVANT l'exclusion, il ne
    // doit pas suffire à garder la piste dans leur périmètre.
    for id in [1, 2, 3] {
        meta.set(id, "rg_analyzed", "1").unwrap();
    }
    piste(&db, 4, "/local/d.flac");
    piste(&db, 5, "/nas/musique/e.flac");

    let avant = (
        replaygain::compter_les_candidats_replaygain(&backend),
        replaygain::compter_les_candidats_dr(&backend),
        replaygain::compter_les_candidats_a_empreinter(&backend),
        embedding_store::eligible_count(&backend),
    );
    assert_eq!(avant, (2, 3, Some(3), 5), "sans périmètre");

    regler(
        &backend,
        CLE_RACINES_EXCLUES,
        serde_json::json!(["/nas/musique"]),
    );
    assert_eq!(replaygain::compter_les_candidats_replaygain(&backend), 1);
    assert_eq!(replaygain::compter_les_candidats_dr(&backend), 1);
    assert_eq!(
        replaygain::compter_les_candidats_a_empreinter(&backend),
        Some(1)
    );
    assert_eq!(embedding_store::eligible_count(&backend), 2);

    // Contre-épreuve : on lève l'exclusion, les chiffres d'avant reviennent.
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!([]));
    let apres = (
        replaygain::compter_les_candidats_replaygain(&backend),
        replaygain::compter_les_candidats_dr(&backend),
        replaygain::compter_les_candidats_a_empreinter(&backend),
        embedding_store::eligible_count(&backend),
    );
    assert_eq!(apres, avant);
}

/// Une piste d'une racine exclue dont le disque ne répond pas n'est plus
/// « en attente d'un disque » : elle n'est plus du travail du tout.
#[test]
fn une_reportee_d_une_racine_exclue_n_attend_plus_son_disque() {
    let (db, backend) = base();
    piste(&db, 1, "/local/a.flac");
    piste(&db, 2, "/nas/b.flac");
    let meta = TrackMetadataRepo::new(db.clone());
    // Un report très frais : au-dessus de tout seuil.
    meta.set(1, "rg_path_unresolved", "99999999999").unwrap();
    meta.set(2, "rg_path_unresolved", "99999999999").unwrap();
    assert_eq!(replaygain::compter_les_reportees_par_chemin(&backend), 2);
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!(["/nas"]));
    assert_eq!(replaygain::compter_les_reportees_par_chemin(&backend), 1);
}

/// Fil 2157 — une piste sans plage dynamique d'une racine exclue n'est
/// candidate d'aucune passe : elle doit être comptée À PART, sinon la carte
/// Santé l'attend pour toujours et sa jauge ne finit jamais.
///
/// Le test vérifie la PARTITION : toute piste sans DR est candidate, ou
/// nommée par un compteur — jamais les deux, jamais aucun.
#[test]
fn une_piste_sans_dr_d_une_racine_exclue_est_comptee_a_part() {
    let (db, backend) = base();
    piste(&db, 1, "/local/a.flac");
    piste(&db, 2, "/nas/musique/b.flac");
    piste(&db, 3, "/nas/musique/c.flac");
    piste(&db, 4, "/nas/musique/d.flac");
    piste(&db, 5, "/nas/musique/e.flac");
    // Même préfixe sans le séparateur : PAS dans la racine.
    piste(&db, 6, "/nas/musique2/f.flac");
    let meta = TrackMetadataRepo::new(db.clone());
    meta.set(3, "dr_track", "8").unwrap();
    meta.set(4, "dr_indisponible", "1").unwrap();
    meta.set(5, "rg_skipped_oversized", "1").unwrap();
    meta.set(5, "rg_analyzed", "1").unwrap();

    // Rien d'exclu : zéro, sans requête.
    assert_eq!(replaygain::compter_les_sans_dr_hors_perimetre(&backend), 0);

    regler(
        &backend,
        CLE_RACINES_EXCLUES,
        serde_json::json!(["/nas/musique"]),
    );
    // Seule la piste 2 : la 3 a son DR, la 4 est comptée « indisponible »,
    // la 5 « trop longue », la 6 est hors de la racine.
    assert_eq!(replaygain::compter_les_sans_dr_hors_perimetre(&backend), 1);
    // La partition : 1 et 6 candidates, 2 hors périmètre, 4 indisponible,
    // 5 trop longue — cinq pistes sans DR, cinq places, aucune en double.
    let candidates = replaygain::compter_les_candidats_dr(&backend);
    assert_eq!(candidates, 2);
    let sans_dr = backend
        .query_one(
            "SELECT COUNT(*) FROM tracks t WHERE NOT EXISTS (SELECT 1 FROM track_metadata m \
             WHERE m.track_id = t.id AND m.key = 'dr_track' AND TRIM(m.value) != '')",
            &[],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap();
    assert_eq!(
        sans_dr,
        candidates + replaygain::compter_les_sans_dr_hors_perimetre(&backend) + 1 + 1,
        "chaque piste sans DR doit avoir UNE place"
    );

    // Contre-épreuve : l'exclusion levée, la piste 2 redevient du travail.
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!([]));
    assert_eq!(replaygain::compter_les_sans_dr_hors_perimetre(&backend), 0);
    assert!(replaygain::compter_les_candidats_dr(&backend) >= 3);
}

/// Le numérateur de la jauge CLAP suit le dénominateur : une piste traitée
/// AVANT son exclusion sort des deux à la fois.
#[test]
fn le_numerateur_clap_suit_le_perimetre() {
    let (db, backend) = base();
    piste(&db, 1, "/local/a.flac");
    piste(&db, 2, "/nas/b.flac");
    let meta = TrackMetadataRepo::new(db.clone());
    meta.set(1, "audio_embed_analyzed", embedding_store::MODEL_ID)
        .unwrap();
    meta.set(2, "audio_embed_analyzed", embedding_store::MODEL_ID)
        .unwrap();
    assert_eq!(embedding_store::processed_count_in_scope(&backend), 2);
    assert_eq!(embedding_store::eligible_count(&backend), 2);
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!(["/nas"]));
    assert_eq!(embedding_store::eligible_count(&backend), 1);
    assert_eq!(
        embedding_store::processed_count_in_scope(&backend),
        1,
        "la piste du NAS sort du numérateur comme du dénominateur"
    );
    // Le compteur global, lui, ne bouge pas : la recherche par ambiance garde
    // les empreintes déjà calculées.
    assert_eq!(embedding_store::processed_count(&backend), 2);
}

// ── Les passes : jamais décodée ─────────────────────────────────────────

fn fixture() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ape/sine_16s_c3000.wav"
    )
}

fn racine_de_la_fixture() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures").to_string()
}

/// 🔴 Le cœur de la demande : une piste d'une racine exclue, sur un fichier
/// parfaitement décodable, n'est PAS décodée par la passe ReplayGain — elle
/// ne reçoit ni témoin, ni gain, ni empreinte. Contre-épreuve dans le même
/// test : l'exclusion levée, la même passe la décode.
#[tokio::test]
async fn la_passe_replaygain_ne_decode_jamais_une_racine_exclue() {
    let (db, backend) = base();
    SettingsRepo::with_backend(backend.clone())
        .set(replaygain::MODE_KEY, "track")
        .unwrap();
    piste(&db, 42, fixture());
    regler(
        &backend,
        CLE_RACINES_EXCLUES,
        serde_json::json!([racine_de_la_fixture()]),
    );

    assert_eq!(replaygain::analyze_track_batch(&backend).await, 0);
    let m = TrackMetadataRepo::new(db.clone()).get_all(42).unwrap();
    assert!(
        !m.contains_key("rg_analyzed") && !m.contains_key("rg_track_gain"),
        "exclue : jamais décodée, aucun témoin : {m:?}"
    );
    // Les rattrapages ne la prennent pas non plus (ReplayGain armé : ils
    // exigent son témoin, absent — on vérifie qu'ils restent muets).
    assert_eq!(replaygain::empreinter_un_lot(&backend).await, 0);
    assert_eq!(replaygain::rattraper_un_lot_de_dr(&backend).await, 0);

    // Contre-épreuve.
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!([]));
    assert_eq!(replaygain::analyze_track_batch(&backend).await, 1);
    assert!(
        TrackMetadataRepo::new(db.clone())
            .get_all(42)
            .unwrap()
            .contains_key("rg_analyzed")
    );
}

/// Même chose pour les deux rattrapages, ReplayGain COUPÉ (#5246 : ils
/// travaillent alors sans témoin) — c'est le cas où ils décoderaient seuls.
#[tokio::test]
async fn les_rattrapages_ne_decodent_jamais_une_racine_exclue() {
    let (db, backend) = base();
    piste(&db, 42, fixture());
    regler(
        &backend,
        CLE_RACINES_EXCLUES,
        serde_json::json!([racine_de_la_fixture()]),
    );
    assert!(!replaygain::analysis_enabled(&backend), "ReplayGain coupé");

    assert_eq!(replaygain::empreinter_un_lot(&backend).await, 0);
    assert_eq!(replaygain::rattraper_un_lot_de_dr(&backend).await, 0);
    let fp: Option<String> = db
        .query_one("SELECT audio_fingerprint FROM tracks WHERE id = 42", &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()));
    assert_eq!(fp, None, "exclue : aucune empreinte");
    let m = TrackMetadataRepo::new(db.clone()).get_all(42).unwrap();
    assert!(
        !m.contains_key("dr_track") && !m.contains_key("dr_indisponible"),
        "exclue : aucune mesure ni marque de plage : {m:?}"
    );

    // Contre-épreuve : levée, l'empreinte est posée et la plage tentée.
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!([]));
    assert_eq!(replaygain::empreinter_un_lot(&backend).await, 1);
    assert_eq!(replaygain::rattraper_un_lot_de_dr(&backend).await, 1);
}

/// La sélection du CLAP — la seule entrée de sa passe — ne rend pas une piste
/// d'une racine exclue.
#[test]
fn la_selection_clap_respecte_les_racines_exclues() {
    let (db, backend) = base();
    piste(&db, 1, "/local/a.flac");
    piste(&db, 2, "/local/b.flac");
    piste(&db, 3, "/nas/c.flac");
    let rendus = |backend: &Arc<dyn DbBackend>| -> Vec<i64> {
        let mut v: Vec<i64> = embedding_store::candidats_acoustiques(backend, "00000000000", 25)
            .unwrap()
            .into_iter()
            .filter_map(|c| match c {
                embedding_store::CandidatAcoustique::Pret { track_id, .. } => Some(track_id),
                _ => None,
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(rendus(&backend), vec![1, 2, 3]);
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!(["/nas"]));
    assert_eq!(rendus(&backend), vec![1, 2]);
}

// ── #6198 — les genres du CLAP ──────────────────────────────────────────

fn piste_de_genre(db: &SqliteDb, id: i64, chemin: &str, genre: Option<&str>, genres: Option<&str>) {
    db.execute(
        "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, \
         sample_rate, channels, genre, genres) VALUES (?, 'T', 1, 1, ?, 1000, 44100, 2, ?, ?)",
        &[&id, &chemin, &genre, &genres],
    )
    .unwrap();
}

fn candidats_clap(backend: &Arc<dyn DbBackend>) -> Vec<i64> {
    let mut v: Vec<i64> = embedding_store::candidats_acoustiques(backend, "00000000000", 25)
        .unwrap()
        .into_iter()
        .filter_map(|c| match c {
            embedding_store::CandidatAcoustique::Pret { track_id, .. } => Some(track_id),
            _ => None,
        })
        .collect();
    v.sort();
    v
}

/// Le genre se cherche dans la colonne `t.genre` OU dans le tableau
/// `t.genres`, sans la casse. Retenir : une piste sans genre n'est pas
/// retenue. Écarter : une piste sans genre reste.
#[test]
fn le_genre_du_clap_se_cherche_dans_la_colonne_et_dans_le_tableau() {
    let (db, backend) = base();
    piste_de_genre(&db, 1, "/m/1.flac", Some("Jazz"), None);
    piste_de_genre(&db, 2, "/m/2.flac", None, Some(r#"["Pop","Soul"]"#));
    piste_de_genre(
        &db,
        3,
        "/m/3.flac",
        Some("Classique"),
        Some(r#"["Classique"]"#),
    );
    piste_de_genre(&db, 4, "/m/4.flac", None, None);
    piste_de_genre(&db, 5, "/m/5.flac", Some("Rock'n'Roll"), None);

    assert_eq!(clause_genres_inclus(&[]), "");
    assert_eq!(clause_genres_exclus(&[" ".into()]), "");

    let retenus = clause_genres_inclus(&["jazz".into(), "SOUL".into(), "Rock'n'Roll".into()]);
    assert_eq!(ids(&backend, &retenus), vec![1, 2, 5]);
    let ecartes = clause_genres_exclus(&["jazz".into(), "SOUL".into(), "Rock'n'Roll".into()]);
    assert_eq!(
        ids(&backend, &ecartes),
        vec![3, 4],
        "la piste sans genre reste"
    );

    // Un joker dans un nom de genre n'élargit rien.
    assert_eq!(
        ids(&backend, &clause_genres_inclus(&["%".into()])),
        Vec::<i64>::new()
    );
    assert_eq!(
        ids(&backend, &clause_genres_inclus(&["P_p".into()])),
        Vec::<i64>::new()
    );
}

/// Thierry (#6198) : la passe CLAP — sa sélection ET ses compteurs — ne voit
/// que les genres retenus, moins les genres écartés, par-dessus les racines
/// exclues. Une piste écartée ne bloque pas la passe : une fois les retenues
/// traitées, plus aucun candidat, et la jauge est pleine. Contre-épreuve dans
/// le même test : la liste levée, la piste écartée redevient du travail.
#[test]
fn la_passe_clap_ne_voit_que_les_genres_retenus() {
    let (db, backend) = base();
    piste_de_genre(&db, 1, "/local/1.flac", Some("Jazz"), None);
    piste_de_genre(&db, 2, "/local/2.flac", Some("Metal"), None);
    piste_de_genre(&db, 3, "/local/3.flac", None, Some(r#"["Jazz","Blues"]"#));
    piste_de_genre(&db, 4, "/nas/4.flac", Some("Jazz"), None);
    piste_de_genre(&db, 5, "/local/5.flac", None, None);
    piste_de_genre(&db, 6, "/local/6.flac", Some("Blues"), None);

    assert_eq!(candidats_clap(&backend), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(embedding_store::eligible_count(&backend), 6);

    // Écarter le métal.
    regler(
        &backend,
        CLE_GENRES_CLAP_EXCLUS,
        serde_json::json!(["metal"]),
    );
    assert_eq!(candidats_clap(&backend), vec![1, 3, 4, 5, 6]);
    assert_eq!(embedding_store::eligible_count(&backend), 5);

    // Retenir le jazz seul, par-dessus la racine exclue.
    regler(&backend, CLE_GENRES_CLAP_EXCLUS, serde_json::json!([]));
    regler(
        &backend,
        CLE_GENRES_CLAP_INCLUS,
        serde_json::json!(["Jazz"]),
    );
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!(["/nas"]));
    assert_eq!(candidats_clap(&backend), vec![1, 3]);
    assert_eq!(embedding_store::eligible_count(&backend), 2);

    // Retenir et écarter à la fois : le blues retiré du jazz+blues.
    regler(
        &backend,
        CLE_GENRES_CLAP_INCLUS,
        serde_json::json!(["Jazz", "Blues"]),
    );
    regler(
        &backend,
        CLE_GENRES_CLAP_EXCLUS,
        serde_json::json!(["blues"]),
    );
    assert_eq!(candidats_clap(&backend), vec![1], "3 porte aussi le blues");
    regler(&backend, CLE_GENRES_CLAP_EXCLUS, serde_json::json!([]));
    regler(
        &backend,
        CLE_GENRES_CLAP_INCLUS,
        serde_json::json!(["Jazz"]),
    );

    // Les retenues traitées : plus aucun candidat, la jauge est pleine.
    let meta = TrackMetadataRepo::new(db.clone());
    for id in [1, 3] {
        meta.set(id, "audio_embed_analyzed", embedding_store::MODEL_ID)
            .unwrap();
    }
    assert_eq!(
        candidats_clap(&backend),
        Vec::<i64>::new(),
        "la passe est finie"
    );
    assert_eq!(
        embedding_store::processed_count_in_scope(&backend),
        embedding_store::eligible_count(&backend)
    );

    // Contre-épreuve : la liste levée, les autres redeviennent du travail.
    regler(&backend, CLE_GENRES_CLAP_INCLUS, serde_json::json!([]));
    assert_eq!(candidats_clap(&backend), vec![2, 5, 6]);
}

/// Les genres ne touchent QUE le CLAP : ReplayGain, plage dynamique et
/// empreintes servent à la lecture de chaque piste.
#[test]
fn les_genres_ne_touchent_que_le_clap() {
    let (_db, backend) = base();
    regler(
        &backend,
        CLE_GENRES_CLAP_INCLUS,
        serde_json::json!(["Jazz"]),
    );
    regler(
        &backend,
        CLE_GENRES_CLAP_EXCLUS,
        serde_json::json!(["Metal"]),
    );
    assert_eq!(clause_decodage(&backend, Passe::ReplayGain), "");
    assert_eq!(
        clause_hors_perimetre_decodage(&backend, Passe::ReplayGain),
        ""
    );
    assert!(!clause_clap(&backend).is_empty());
}

// ── Chantier périmètre — dossiers et genres de TOUTES les passes ─────────

/// Les quatre compteurs (ReplayGain, plage dynamique, empreintes, CLAP), dans
/// cet ordre. ReplayGain en mode piste, sans témoin posé : chaque piste
/// ouvrable est candidate de chacun tant qu'elle est dans le périmètre.
fn quatre_compteurs(backend: &Arc<dyn DbBackend>) -> (i64, i64, Option<i64>, i64) {
    (
        replaygain::compter_les_candidats_replaygain(backend),
        replaygain::compter_les_candidats_dr(backend),
        replaygain::compter_les_candidats_a_empreinter(backend),
        embedding_store::eligible_count(backend),
    )
}

/// Fil 2173 : « circonscrire l'analyse à un répertoire ». Un SOUS-DOSSIER
/// réglé dans `background_analysis_excluded_roots` exclut ce dossier et lui
/// seul, des quatre passes : ni son voisin de même préfixe, ni le reste de la
/// racine. Contre-épreuve : la liste levée, les chiffres d'avant reviennent.
#[test]
fn un_sous_dossier_exclu_sort_des_quatre_passes_et_lui_seul() {
    let (db, backend) = base();
    piste(&db, 1, "Z:\\NAS\\Jazz\\a.flac");
    piste(&db, 2, "Z:\\NAS\\Podcasts\\b.mp3");
    piste(&db, 3, "Z:/NAS/Podcasts/c.mp3");
    piste(&db, 4, "Z:\\NAS\\Podcasts2\\d.flac");
    let avant = quatre_compteurs(&backend);
    assert_eq!(avant, (4, 4, Some(4), 4), "sans périmètre");

    regler(
        &backend,
        CLE_RACINES_EXCLUES,
        serde_json::json!(["Z:\\NAS\\Podcasts\\"]),
    );
    assert_eq!(
        quatre_compteurs(&backend),
        (2, 2, Some(2), 2),
        "2 et 3 (les deux séparateurs) sortent ; 4, voisin de même préfixe, reste"
    );
    assert_eq!(
        ids(&backend, &clause_decodage(&backend, Passe::ReplayGain)),
        vec![1, 4]
    );
    assert_eq!(
        ids(
            &backend,
            &clause_hors_perimetre_decodage(&backend, Passe::ReplayGain)
        ),
        vec![2, 3]
    );

    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!([]));
    assert_eq!(quatre_compteurs(&backend), avant);
}

/// Un genre écarté de toutes les passes sort des QUATRE compteurs et des
/// sélections, qu'il soit dans la colonne ou dans le tableau ; une piste sans
/// genre reste. Contre-épreuve dans le même test.
#[test]
fn un_genre_exclu_sort_des_quatre_passes() {
    let (db, backend) = base();
    piste_de_genre(&db, 1, "/m/1.flac", Some("Jazz"), None);
    piste_de_genre(&db, 2, "/m/2.mp3", Some("podcast"), None);
    piste_de_genre(&db, 3, "/m/3.mp3", None, Some(r#"["Talk","Podcast"]"#));
    piste_de_genre(&db, 4, "/m/4.flac", None, None);
    let avant = quatre_compteurs(&backend);
    assert_eq!(avant, (4, 4, Some(4), 4));
    assert_eq!(candidats_clap(&backend), vec![1, 2, 3, 4]);

    regler(&backend, CLE_GENRES_EXCLUS, serde_json::json!(["Podcast"]));
    assert_eq!(
        quatre_compteurs(&backend),
        (2, 2, Some(2), 2),
        "2 (colonne, casse ignorée) et 3 (tableau) sortent ; 4, sans genre, reste"
    );
    assert_eq!(candidats_clap(&backend), vec![1, 4]);

    regler(&backend, CLE_GENRES_EXCLUS, serde_json::json!([]));
    assert_eq!(quatre_compteurs(&backend), avant);
    assert_eq!(candidats_clap(&backend), vec![1, 2, 3, 4]);
}

/// La PARTITION, dossiers et genres mêlés : chaque piste est dans le
/// périmètre (`clause_decodage`) ou hors de lui
/// (`clause_hors_perimetre_decodage`) — jamais les deux, jamais aucun. C'est
/// ce qui garde les jauges de la page Santé justes (fil 2157).
#[test]
fn dossiers_et_genres_partitionnent_la_bibliotheque() {
    let (db, backend) = base();
    piste_de_genre(&db, 1, "/local/1.flac", Some("Jazz"), None);
    piste_de_genre(&db, 2, "/local/2.mp3", Some("Podcast"), None);
    piste_de_genre(&db, 3, "/nas/3.flac", Some("Jazz"), None);
    piste_de_genre(&db, 4, "/nas/4.mp3", Some("Podcast"), None);
    piste_de_genre(&db, 5, "/local/5.flac", None, None);
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!(["/nas"]));
    regler(&backend, CLE_GENRES_EXCLUS, serde_json::json!(["Podcast"]));
    let dedans = ids(&backend, &clause_decodage(&backend, Passe::ReplayGain));
    let dehors = ids(
        &backend,
        &clause_hors_perimetre_decodage(&backend, Passe::ReplayGain),
    );
    assert_eq!(dedans, vec![1, 5]);
    assert_eq!(
        dehors,
        vec![2, 3, 4],
        "la 4 est exclue deux fois, comptée une"
    );

    // Seuls les genres : la clause hors périmètre ne dépend plus des racines.
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!([]));
    assert_eq!(
        ids(
            &backend,
            &clause_hors_perimetre_decodage(&backend, Passe::ReplayGain)
        ),
        vec![2, 4]
    );
    // Rien d'exclu : les deux clauses sont vides.
    regler(&backend, CLE_GENRES_EXCLUS, serde_json::json!([]));
    assert_eq!(clause_decodage(&backend, Passe::ReplayGain), "");
    assert_eq!(
        clause_hors_perimetre_decodage(&backend, Passe::ReplayGain),
        ""
    );
}

/// Une piste sans plage dynamique d'un genre écarté est comptée à part, comme
/// celle d'une racine exclue (fil 2157) : la jauge Santé ne l'attend pas.
#[test]
fn une_piste_sans_dr_d_un_genre_exclu_est_comptee_a_part() {
    let (db, backend) = base();
    piste_de_genre(&db, 1, "/m/1.flac", Some("Jazz"), None);
    piste_de_genre(&db, 2, "/m/2.mp3", Some("Podcast"), None);
    assert_eq!(replaygain::compter_les_sans_dr_hors_perimetre(&backend), 0);
    regler(&backend, CLE_GENRES_EXCLUS, serde_json::json!(["Podcast"]));
    assert_eq!(replaygain::compter_les_sans_dr_hors_perimetre(&backend), 1);
    assert_eq!(replaygain::compter_les_candidats_dr(&backend), 1);
}

/// 🔴 Le cœur, côté genre : une piste d'un genre écarté, sur un fichier
/// parfaitement décodable, n'est décodée par AUCUNE passe — ReplayGain,
/// empreintes, plage dynamique, CLAP (sa sélection). Contre-épreuve : la
/// liste levée, les mêmes passes la décodent.
#[tokio::test]
async fn aucune_passe_ne_decode_une_piste_d_un_genre_exclu() {
    let (db, backend) = base();
    piste_de_genre(&db, 42, fixture(), Some("Podcast"), None);
    regler(&backend, CLE_GENRES_EXCLUS, serde_json::json!(["Podcast"]));

    // ReplayGain coupé : les rattrapages travaillent sans témoin (#5246),
    // c'est le cas où ils décoderaient seuls.
    assert!(!replaygain::analysis_enabled(&backend));
    assert_eq!(replaygain::empreinter_un_lot(&backend).await, 0);
    assert_eq!(replaygain::rattraper_un_lot_de_dr(&backend).await, 0);
    assert!(candidats_clap(&backend).is_empty());
    SettingsRepo::with_backend(backend.clone())
        .set(replaygain::MODE_KEY, "track")
        .unwrap();
    assert_eq!(replaygain::analyze_track_batch(&backend).await, 0);
    let m = TrackMetadataRepo::new(db.clone()).get_all(42).unwrap();
    assert!(
        !m.contains_key("rg_analyzed") && !m.contains_key("dr_track"),
        "écartée : jamais décodée : {m:?}"
    );

    // Contre-épreuve.
    regler(&backend, CLE_GENRES_EXCLUS, serde_json::json!([]));
    assert_eq!(candidats_clap(&backend), vec![42]);
    assert_eq!(replaygain::analyze_track_batch(&backend).await, 1);
    assert!(
        TrackMetadataRepo::new(db.clone())
            .get_all(42)
            .unwrap()
            .contains_key("rg_analyzed")
    );
}

// ── Étapes 2 et 3 (décisions de Bertrand du 13/10) ──────────────────────

/// Un album de genre `genre` (colonne) et `genres` (tableau), d'id `id`.
fn album_de_genre(db: &SqliteDb, id: i64, genre: Option<&str>, genres: Option<&str>) {
    db.execute(
        "INSERT INTO albums (id, title, artist_id, genre, genres) VALUES (?, 'Compil', 1, ?, ?)",
        &[&id, &genre, &genres],
    )
    .unwrap();
}

/// Une piste de l'album `album`, de genre `genre`.
fn piste_d_album(db: &SqliteDb, id: i64, album: i64, chemin: &str, genre: Option<&str>) {
    db.execute(
        "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, \
         sample_rate, channels, genre) VALUES (?, 'T', ?, 1, ?, 1000, 44100, 2, ?)",
        &[&id, &album, &chemin, &genre],
    )
    .unwrap();
}

/// Le genre se juge À L'ALBUM. Une compilation « Jazz » dont une piste porte
/// « Podcast » est analysée EN ENTIER (son gain d'album reste calculable :
/// toutes ses pistes sont candidates) ; un album « Podcast » sort en entier,
/// même sa piste « Jazz » ; une piste d'un album SANS genre est jugée sur la
/// sienne. Contre-épreuve : la liste levée, tout revient.
#[test]
fn une_compilation_aux_genres_meles_est_jugee_a_l_album() {
    let (db, backend) = base();
    album_de_genre(&db, 2, Some("Jazz"), None);
    album_de_genre(&db, 3, None, Some(r#"["Talk","Podcast"]"#));
    piste_d_album(&db, 10, 2, "/m/c/1.flac", Some("Jazz"));
    piste_d_album(&db, 11, 2, "/m/c/2.flac", Some("Podcast"));
    piste_d_album(&db, 20, 3, "/m/p/1.mp3", Some("Jazz"));
    piste_d_album(&db, 21, 3, "/m/p/2.mp3", Some("Podcast"));
    // L'album 1 de `base()` n'a pas de genre : ses pistes se jugent seules.
    piste_d_album(&db, 30, 1, "/m/s/1.mp3", Some("Podcast"));
    piste_d_album(&db, 31, 1, "/m/s/2.flac", Some("Jazz"));
    assert_eq!(quatre_compteurs(&backend), (6, 6, Some(6), 6));

    regler(&backend, CLE_GENRES_EXCLUS, serde_json::json!(["podcast"]));
    assert_eq!(
        ids(&backend, &clause_decodage(&backend, Passe::ReplayGain)),
        vec![10, 11, 31],
        "la compilation Jazz reste entière ; l'album Podcast sort entier ; \
         l'album sans genre se juge piste par piste"
    );
    assert_eq!(
        ids(
            &backend,
            &clause_hors_perimetre_decodage(&backend, Passe::ReplayGain)
        ),
        vec![20, 21, 30],
        "le complément exact"
    );
    assert_eq!(quatre_compteurs(&backend), (3, 3, Some(3), 3));
    assert_eq!(candidats_clap(&backend), vec![10, 11, 31]);

    regler(&backend, CLE_GENRES_EXCLUS, serde_json::json!([]));
    assert_eq!(quatre_compteurs(&backend), (6, 6, Some(6), 6));
}

/// Étape 2 — un périmètre PAR PASSE. Le commun exclut `/nas` ; le ReplayGain
/// le reprend (`[]` remplace), les empreintes héritent du ReplayGain, la
/// plage dynamique garde le commun, le CLAP écarte en plus un genre à lui.
/// Contre-épreuve : le réglage par passe levé, toutes les passes reviennent
/// au commun.
#[test]
fn chaque_passe_a_son_perimetre_et_herite_du_commun() {
    let (db, backend) = base();
    piste_de_genre(&db, 1, "/local/1.flac", Some("Jazz"), None);
    piste_de_genre(&db, 2, "/nas/2.flac", Some("Jazz"), None);
    piste_de_genre(&db, 3, "/local/3.flac", Some("Rock"), None);
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!(["/nas"]));
    assert_eq!(
        quatre_compteurs(&backend),
        (2, 2, Some(2), 2),
        "le commun seul"
    );

    regler(
        &backend,
        CLE_PAR_PASSE,
        serde_json::json!({
            "replaygain": { "excluded_roots": [] },
            "audio_embedding": { "excluded_genres": ["Rock"] }
        }),
    );
    assert_eq!(
        quatre_compteurs(&backend),
        (3, 2, Some(3), 1),
        "RG reprend /nas, les empreintes suivent RG, la DR garde le commun, \
         le CLAP garde /nas ET écarte Rock"
    );
    assert_eq!(
        perimetre_de(&backend, Passe::Empreintes),
        perimetre_de(&backend, Passe::ReplayGain)
    );
    assert_eq!(candidats_clap(&backend), vec![1]);
    assert_eq!(
        ids(
            &backend,
            &clause_hors_perimetre_decodage(&backend, Passe::PlageDynamique)
        ),
        vec![2]
    );
    assert_eq!(
        clause_hors_perimetre_decodage(&backend, Passe::ReplayGain),
        ""
    );

    // Les empreintes peuvent avoir le leur, qui prime sur l'héritage.
    regler(
        &backend,
        CLE_PAR_PASSE,
        serde_json::json!({
            "replaygain": { "excluded_roots": [] },
            "fingerprints": { "excluded_roots": ["/local"] }
        }),
    );
    assert_eq!(quatre_compteurs(&backend), (3, 2, Some(1), 2));

    regler(&backend, CLE_PAR_PASSE, serde_json::json!({}));
    assert_eq!(quatre_compteurs(&backend), (2, 2, Some(2), 2));
}

/// La forme de `background_analysis_scope_by_pass` : passes et champs connus,
/// `null` hérite (retiré), le reste refusé en nommant la faute.
#[test]
fn normaliser_par_passe_garde_la_forme_unique() {
    assert_eq!(
        normaliser_par_passe(&serde_json::Value::Null).unwrap(),
        serde_json::json!({})
    );
    assert_eq!(
        normaliser_par_passe(&serde_json::json!({
            "replaygain": { "excluded_roots": [" /nas ", "/nas", ""], "excluded_genres": null },
            "dynamic_range": null,
            "fingerprints": {},
            "audio_embedding": { "excluded_genres": [] }
        }))
        .unwrap(),
        serde_json::json!({
            "replaygain": { "excluded_roots": ["/nas"] },
            "audio_embedding": { "excluded_genres": [] }
        })
    );
    for (mauvais, faute) in [
        (serde_json::json!([]), "objet"),
        (serde_json::json!({ "clap": {} }), "passe inconnue"),
        (serde_json::json!({ "replaygain": [] }), "replaygain"),
        (
            serde_json::json!({ "replaygain": { "roots": [] } }),
            "champ inconnu",
        ),
        (
            serde_json::json!({ "replaygain": { "excluded_roots": "/nas" } }),
            "replaygain.excluded_roots",
        ),
    ] {
        let e = normaliser_par_passe(&mauvais).unwrap_err();
        assert!(e.contains(faute), "{mauvais} : {e}");
    }
    // Une valeur illisible en base vaut « tout hérite ».
    let (db, backend) = base();
    piste(&db, 1, "/nas/1.flac");
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!(["/nas"]));
    regler(
        &backend,
        CLE_PAR_PASSE,
        serde_json::json!({ "replaygain": 3 }),
    );
    assert_eq!(
        perimetre_de(&backend, Passe::ReplayGain).dossiers,
        vec!["/nas"]
    );
}

/// Étape 3 — combien de pistes chaque entrée retire, commun et par passe ;
/// une entrée mal orthographiée compte 0. Les genres se comptent à l'album,
/// comme ils s'appliquent.
#[test]
fn chaque_entree_dit_combien_de_pistes_elle_retire() {
    let (db, backend) = base();
    album_de_genre(&db, 2, Some("Jazz"), None);
    piste_d_album(&db, 10, 2, "/nas/c/1.flac", Some("Jazz"));
    piste_d_album(&db, 11, 2, "/nas/c/2.flac", Some("Podcast"));
    piste_d_album(&db, 30, 1, "/local/1.mp3", Some("Podcast"));
    piste_d_album(&db, 31, 1, "/local/2.mp3", Some("Podcast"));
    regler(
        &backend,
        CLE_RACINES_EXCLUES,
        serde_json::json!(["/nas", "/nsa"]),
    );
    regler(&backend, CLE_GENRES_EXCLUS, serde_json::json!(["Podcast"]));
    regler(
        &backend,
        CLE_PAR_PASSE,
        serde_json::json!({ "dynamic_range": { "excluded_roots": ["/local"] } }),
    );
    let (commun, par_passe) = compter_les_retraits(&backend);
    let n = |r: &[Retrait]| {
        r.iter()
            .map(|x| (x.value.clone(), x.tracks))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        n(&commun.excluded_roots),
        vec![("/nas".to_string(), 2), ("/nsa".to_string(), 0)]
    );
    assert_eq!(
        n(&commun.excluded_genres),
        vec![("Podcast".to_string(), 2)],
        "la piste Podcast de la compilation Jazz n'est pas retirée"
    );
    assert_eq!(par_passe.len(), 1);
    assert_eq!(par_passe[0].0, Passe::PlageDynamique);
    assert_eq!(
        n(&par_passe[0].1.excluded_roots),
        vec![("/local".to_string(), 2)]
    );
    assert!(par_passe[0].1.excluded_genres.is_empty());
}
