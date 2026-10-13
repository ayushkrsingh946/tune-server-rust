//! #6167 (Tades, fil 2231, 4 emplacements) — « Le scan est à 54 % de
//! progression mais de quelle bibliothèque parmi les 4 ? » Chaque
//! `library.scan.progress` dit désormais sa portée (toute la bibliothèque ou un
//! dossier), le nombre d'emplacements, et l'emplacement en cours avec son rang.
use super::*;
use std::time::Duration;

#[test]
fn le_rang_suit_la_racine_la_plus_longue_et_l_ordre_des_reglages() {
    let p = PorteeDuScan::new(
        true,
        vec![
            "/srv/a".into(),
            "/srv/b".into(),
            "/srv/b/classique".into(),
            "D:\\Musique".into(),
        ],
    );
    assert_eq!(p.rang("/srv/a/Jazz/Album"), Some(0));
    assert_eq!(p.rang("/srv/b/Rock"), Some(1));
    // Deux racines emboîtées : la plus précise.
    assert_eq!(p.rang("/srv/b/classique/Bach"), Some(2));
    assert_eq!(p.rang("D:\\Musique\\Album"), Some(3));
    // Un préfixe de nom n'est pas un dossier.
    assert_eq!(p.rang("/srv/ab/x"), None);
}

#[test]
fn l_annonce_porte_la_portee_et_l_emplacement_en_cours() {
    let mut p = PorteeDuScan::new(true, vec!["/m/1".into(), "/m/2".into()]);
    p.compter(&[
        "/m/1/a.flac".into(),
        "/m/2/x/b.flac".into(),
        "/m/2/x/c.flac".into(),
    ]);
    let a = p.avec(json!({ "phase": "files" }), Some("/m/2/x"));
    assert_eq!(a["scope"], "library");
    assert_eq!(a["locations_total"], 2);
    assert_eq!(a["location_index"], 2);
    assert_eq!(a["location"], "/m/2");
    assert_eq!(a["location_files"], 2);
    assert_eq!(a["phase"], "files", "les champs existants restent");

    // Sans dossier connu : la portée seule, pas de rang inventé.
    let b = p.avec(json!({}), None);
    assert_eq!(b["locations_total"], 2);
    assert!(b.get("location_index").is_none());

    // Scan ciblé : « un dossier », pas « toute la bibliothèque ».
    let c = PorteeDuScan::new(false, vec!["/m/1/Jazz".into()]).avec(json!({}), Some("/m/1/Jazz/A"));
    assert_eq!(c["scope"], "folder");
    assert_eq!(c["locations_total"], 1);
    assert_eq!(c["location_index"], 1);
}

fn wav(i: u8) -> Vec<u8> {
    let data = vec![i; 2048];
    let mut wav = b"RIFF".to_vec();
    wav.extend((36u32 + data.len() as u32).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(1u16.to_le_bytes());
    wav.extend(44100u32.to_le_bytes());
    wav.extend(88200u32.to_le_bytes());
    wav.extend(2u16.to_le_bytes());
    wav.extend(16u16.to_le_bytes());
    wav.extend(b"data");
    wav.extend((data.len() as u32).to_le_bytes());
    wav.extend(data);
    wav
}

/// Le vrai `spawn_library_scan_avec_lecteur`, deux emplacements, une base de
/// fichier : les annonces nomment les DEUX emplacements, avec leur rang.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_scan_de_deux_emplacements_dit_sa_portee_et_l_emplacement_en_cours_6167() {
    let _seul = serialiser_les_scans_de_test_sans_bloquer().await;
    let dir =
        tune_core::test_scratch::scratch_dir_in(std::env::current_dir().unwrap(), "scan-6167");
    let un = dir.join("a-emplacement").join("Jazz");
    let deux = dir.join("b-emplacement").join("Rock");
    std::fs::create_dir_all(&un).unwrap();
    std::fs::create_dir_all(&deux).unwrap();
    std::fs::write(un.join("piste-0.wav"), wav(0)).unwrap();
    for i in 1..4u8 {
        std::fs::write(deux.join(format!("piste-{i}.wav")), wav(i)).unwrap();
    }
    let racine_un = dir.join("a-emplacement");
    let racine_deux = dir.join("b-emplacement");
    let state =
        AppState::new(dir.join("tune.db").to_str().unwrap(), 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            "music_dirs",
            &serde_json::to_string(&[racine_un.to_str().unwrap(), racine_deux.to_str().unwrap()])
                .unwrap(),
        )
        .unwrap();
    settings.set("enrich_on_scan", "false").unwrap();

    let mut rx = state.event_bus.subscribe();
    let lecteur: LecteurMetadonnees = std::sync::Arc::new(|_| Default::default());
    assert!(
        spawn_library_scan_avec_lecteur(
            state.clone(),
            false,
            None,
            None,
            lecteur,
            DELAI_LECTURE_CREDITS
        )
        .await
    );
    let mut progres: Vec<Value> = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(90), rx.recv())
            .await
            .expect("le scan #6167 doit rendre la main")
            .unwrap();
        let fini = event.event_type == "library.scan.completed";
        if event.event_type == "library.scan.progress" {
            progres.push(event.data.clone());
        }
        if fini {
            attendre_que_le_droit_de_scanner_soit_libre().await;
            break;
        }
    }

    // Les annonces `stage: extended_metadata` (lecture_bornee.rs, partagée avec
    // le scan automatique) ne portent pas la portée : le client garde la
    // dernière connue (fusion). Toutes les autres la portent.
    let avancement: Vec<&Value> = progres
        .iter()
        .filter(|d| d["phase"] == "indexing" || d["phase"] == "files")
        .filter(|d| d["stage"] != "extended_metadata")
        .collect();
    assert!(
        !avancement.is_empty(),
        "aucune annonce d'avancement : {progres:?}"
    );
    assert!(
        avancement
            .iter()
            .all(|d| d["scope"] == "library" && d["locations_total"] == 2),
        "#6167 : chaque annonce d'avancement doit dire « toute la bibliothèque, 2 emplacements » : {progres:?}"
    );
    let un_txt = racine_un.to_string_lossy().into_owned();
    let deux_txt = racine_deux.to_string_lossy().into_owned();
    assert!(
        avancement.iter().any(|d| d["location_index"] == 1
            && d["location"] == tune_core::scanner::walker::normalize_path(&un_txt).as_str()),
        "#6167 : le premier emplacement doit être nommé avec son rang : {progres:?}"
    );
    assert!(
        avancement.iter().any(|d| d["location_index"] == 2
            && d["location"] == tune_core::scanner::walker::normalize_path(&deux_txt).as_str()
            && d["location_files"] == 3),
        "#6167 : le second emplacement doit être nommé, rang et fichiers compris : {progres:?}"
    );
}
