//! Témoins du calcul de fond (chantier « priorité lecture », étape 3).

use super::*;

/// Un pool armé, à lui : le pool du processus n'est pas armé dans les
/// binaires de test (voir le module).
static ARME: Pool = Pool::nouveau(true);
/// Un pool non armé, pour la contre-épreuve « ce sont bien les fils du pool
/// qui sont baissés, pas le processus ».
static NEUTRE: Pool = Pool::nouveau(false);

#[cfg(target_os = "linux")]
fn nice_du_fil_courant() -> i32 {
    // SAFETY: lecture de la priorité du seul fil courant.
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid);
        *libc::__errno_location() = 0;
        libc::getpriority(libc::PRIO_PROCESS as _, tid as libc::id_t)
    }
}

#[cfg(target_os = "linux")]
fn ioprio_du_fil_courant() -> (i64, i64) {
    // SAFETY: lecture de la priorité d'E/S du seul fil courant.
    let v = unsafe {
        let tid = libc::syscall(libc::SYS_gettid);
        libc::syscall(libc::SYS_ioprio_get, 1 as libc::c_long, tid)
    };
    ((v >> 13) as i64, (v & 0xff) as i64)
}

/// Le travail tourne sur un fil du pool, nommé, marqué « de fond » — jamais
/// sur le fil appelant.
#[tokio::test(flavor = "current_thread")]
async fn le_travail_part_sur_un_fil_du_calcul_de_fond() {
    let appelant = std::thread::current().id();
    let (nom, fil, de_fond, du_pool) = ARME
        .executer(|| {
            (
                std::thread::current().name().map(str::to_owned),
                std::thread::current().id(),
                crate::taches_de_fond::priorite::politique::fil_de_fond(),
                fil_du_calcul_de_fond(),
            )
        })
        .await
        .expect("le travail doit rendre son résultat");
    assert_ne!(fil, appelant, "le calcul a tourné sur le fil appelant");
    assert!(
        nom.as_deref().is_some_and(|n| n.starts_with(NOM_DES_FILS)),
        "fil inattendu : {nom:?}"
    );
    assert!(de_fond, "un fil du calcul de fond est « de fond »");
    assert!(du_pool);
    assert!(
        !fil_du_calcul_de_fond(),
        "le fil appelant ne doit pas se croire du pool"
    );
}

/// Le fil du pool est baissé (Linux : nice 10, E/S `IDLE` ou best-effort 7)
/// et le fil APPELANT — celui qui, en production, peut être un fil de la
/// lecture ou de l'exécuteur — garde exactement sa priorité. Un fil né à
/// côté (la sortie locale, un décodeur de lecture) aussi.
///
/// Contre-épreuve dans le test : le pool non armé laisse ses fils à la
/// priorité du processus.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
async fn seuls_les_fils_du_pool_sont_baisses() {
    let nice_appelant = nice_du_fil_courant();
    let io_appelant = ioprio_du_fil_courant();

    let (nice_pool, io_pool) = ARME
        .executer(|| (nice_du_fil_courant(), ioprio_du_fil_courant()))
        .await
        .unwrap();
    assert!(
        nice_appelant < 10,
        "le témoin suppose un processus de test sous nice 10 (vu {nice_appelant})"
    );
    assert_eq!(
        nice_pool, 10,
        "un fil du calcul de fond doit être à nice 10"
    );
    assert!(
        io_pool.0 == 3 || (io_pool.0 == 2 && io_pool.1 == 7),
        "E/S d'un fil du calcul de fond : classe IDLE ou best-effort 7, vu {io_pool:?}"
    );

    // Plusieurs travaux de suite : le fil appelant n'a pas bougé.
    for _ in 0..8 {
        ARME.executer(|| ()).await.unwrap();
    }
    assert_eq!(
        nice_du_fil_courant(),
        nice_appelant,
        "le fil appelant a perdu sa priorité processeur"
    );
    assert_eq!(
        ioprio_du_fil_courant(),
        io_appelant,
        "le fil appelant a perdu sa priorité d'E/S"
    );
    // Un fil « de lecture » né après : la priorité du processus, intacte.
    let lecture = std::thread::spawn(|| (nice_du_fil_courant(), ioprio_du_fil_courant()))
        .join()
        .unwrap();
    assert_eq!(
        lecture.0, nice_appelant,
        "un fil né hors du pool a hérité de la baisse"
    );

    // Contre-épreuve : un pool non armé ne baisse rien.
    let (nice_neutre, _) = NEUTRE
        .executer(|| (nice_du_fil_courant(), ioprio_du_fil_courant()))
        .await
        .unwrap();
    assert_eq!(
        nice_neutre, nice_appelant,
        "le pool non armé ne doit rien baisser — sinon le témoin ci-dessus ne distingue rien"
    );

    let releve = releve();
    assert_eq!(releve.cpu, Some("nice_10"));
    assert!(matches!(releve.io, Some("idle") | Some("best_effort_7")));
}

/// macOS : la classe QoS des fils du pool est BACKGROUND, celle du fil
/// appelant ne bouge pas. (Porte à jouer sur le Mac.)
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "current_thread")]
async fn seuls_les_fils_du_pool_sont_baisses() {
    fn qos() -> u32 {
        let mut classe = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
        let mut relative = 0;
        // SAFETY: lecture de la QoS du seul fil courant.
        unsafe {
            libc::pthread_get_qos_class_np(libc::pthread_self(), &mut classe, &mut relative);
        }
        classe as u32
    }
    const BACKGROUND: u32 = libc::qos_class_t::QOS_CLASS_BACKGROUND as u32;
    let appelant = qos();
    let pool = ARME.executer(qos).await.unwrap();
    assert_eq!(pool, BACKGROUND);
    for _ in 0..8 {
        ARME.executer(|| ()).await.unwrap();
    }
    assert_eq!(qos(), appelant, "le fil appelant a changé de classe QoS");
    let neutre = NEUTRE.executer(qos).await.unwrap();
    assert_ne!(neutre, BACKGROUND, "le pool non armé ne doit rien baisser");
}

/// `PrioriteBasse` sur un fil du pool : neutre. Sinon son `drop` RELÈVERAIT
/// un fil baissé pour toujours (macOS : `PRIO_DARWIN_BG` remis à 0).
///
/// Contre-épreuve dans le test : sur un fil ordinaire, la même baisse
/// s'applique (Linux, macOS ; ailleurs elle est neutre partout).
#[tokio::test(flavor = "current_thread")]
async fn la_baisse_reversible_est_neutre_sur_un_fil_du_pool() {
    let sur_le_pool = ARME
        .executer(|| super::super::politique::baisser().appliquee())
        .await
        .unwrap();
    assert!(!sur_le_pool, "la baisse réversible a touché un fil du pool");
    let ordinaire = std::thread::spawn(|| super::super::politique::baisser().appliquee())
        .join()
        .unwrap();
    assert_eq!(
        ordinaire,
        cfg!(any(target_os = "linux", target_os = "macos", windows)),
        "contre-épreuve : sur un fil ordinaire la baisse doit s'appliquer"
    );
}

/// Une panique dans le travail rend `Interrompu` et ne tue pas le pool.
#[tokio::test(flavor = "current_thread")]
async fn une_panique_rend_interrompu_et_le_pool_survit() {
    let r: Result<(), Interrompu> = NEUTRE.executer(|| panic!("panique voulue du témoin")).await;
    assert_eq!(r, Err(Interrompu));
    assert_eq!(NEUTRE.executer(|| 7).await, Ok(7));
}

/// Élastique : un travail bloqué ne bouche pas le pool, le suivant trouve un
/// fil (le cas du décodeur qui tourne en rond, que la passe abandonne).
#[tokio::test(flavor = "current_thread")]
async fn un_travail_bloque_ne_bouche_pas_le_pool() {
    static BLOQUE: Pool = Pool::nouveau(false);
    let (lacher, attendre) = std::sync::mpsc::channel::<()>();
    let bloque = BLOQUE.executer(move || {
        let _ = attendre.recv();
    });
    let bloque = tokio::spawn(bloque);
    let suivant = tokio::time::timeout(Duration::from_secs(5), BLOQUE.executer(|| 1)).await;
    assert_eq!(
        suivant.ok(),
        Some(Ok(1)),
        "le second travail a attendu le premier"
    );
    lacher.send(()).unwrap();
    bloque.await.unwrap().unwrap();
}

/// Le réglage coupé renvoie le calcul sur le pool bloquant de tokio : plus
/// aucun travail n'arrive aux fils du calcul de fond.
#[tokio::test(flavor = "current_thread")]
async fn le_reglage_coupe_rend_le_pool_bloquant_de_tokio() {
    let _verrou = crate::taches_de_fond::priorite::politique::verrou_des_essais();
    use crate::taches_de_fond::priorite::politique::{Politique, politique, regler};
    struct Remettre(bool);
    impl Drop for Remettre {
        fn drop(&mut self) {
            regler(Politique {
                background_compute_low_priority: self.0,
                ..politique()
            });
        }
    }
    let _r = Remettre(politique().background_compute_low_priority);
    regler(Politique {
        background_compute_low_priority: false,
        ..politique()
    });
    let du_pool = calcul_de_fond(fil_du_calcul_de_fond).await.unwrap();
    assert!(!du_pool, "réglage coupé : le calcul doit aller à tokio");
    regler(Politique {
        background_compute_low_priority: true,
        ..politique()
    });
    let du_pool = calcul_de_fond(fil_du_calcul_de_fond).await.unwrap();
    assert!(
        du_pool,
        "réglage armé : le calcul doit aller aux fils du pool"
    );
}

/// Aucun chemin de la LECTURE n'envoie de travail au calcul de fond : ni la
/// sortie locale, ni son fil de rendu temps réel, ni le flux HTTP, ni
/// l'orchestrateur. Garde de texte, comptée HORS des commentaires.
#[test]
fn aucun_chemin_de_lecture_n_y_passe() {
    let racine = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut fautifs = Vec::new();
    let mut vus = 0;
    for dossier in ["outputs", "streaming", "orchestrator", "audio"] {
        let mut pile = vec![racine.join(dossier)];
        let mut fichiers = Vec::new();
        if racine.join(format!("{dossier}.rs")).exists() {
            fichiers.push(racine.join(format!("{dossier}.rs")));
        }
        while let Some(d) = pile.pop() {
            let Ok(entrees) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in entrees.flatten() {
                let p = e.path();
                if p.is_dir() {
                    pile.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    fichiers.push(p);
                }
            }
        }
        for f in fichiers {
            let nom = f.to_string_lossy().replace('\\', "/");
            // Les seuls appelants admis : les passes d'analyse.
            let admis = [
                "audio/analyzer.rs",
                "audio/replaygain.rs",
                "audio/embedding.rs",
            ];
            if admis.iter().any(|a| nom.ends_with(a)) {
                continue;
            }
            vus += 1;
            let texte = std::fs::read_to_string(&f).unwrap_or_default();
            let code = texte
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .any(|l| l.contains("calcul_de_fond("));
            if code {
                fautifs.push(nom);
            }
        }
    }
    assert!(vus > 20, "le témoin n'a presque rien lu ({vus} fichiers)");
    assert!(
        fautifs.is_empty(),
        "un chemin de la lecture envoie du travail aux fils baissés : {fautifs:?}"
    );
}
