//! Le calcul de fond sur des fils à lui, en basse priorité (chantier
//! « priorité lecture », étape 3, #4681).
//!
//! Les segments de décodage du ReplayGain, de la plage dynamique, des
//! empreintes et de l'analyse acoustique (décodage et inférence ONNX)
//! partaient sur le pool bloquant de tokio, PARTAGÉ avec le premier plan. Sous
//! Linux, on ne peut pas y baisser le `nice` d'un fil pour le temps d'un
//! travail : un compte ordinaire ne peut pas le relever ensuite (ni
//! `RLIMIT_NICE` ni `CAP_SYS_NICE` dans l'unité Tune OS). Seuls les fils du
//! scan (`scan-io`) étaient baissés, parce qu'ils sont à lui.
//!
//! Ce module fait pour le calcul de fond ce que `scan-io` fait pour le scan :
//! des fils À LUI, baissés une fois pour toutes à leur naissance.
//!
//! | Système | Processeur | Disque |
//! |---|---|---|
//! | Linux | `nice 10` | classe `IDLE` (`ioprio`), à défaut best-effort 7 |
//! | macOS | QoS `BACKGROUND` (`pthread_set_qos_class_self_np`) | idem, la QoS bride aussi les E/S |
//! | Windows | `THREAD_MODE_BACKGROUND_BEGIN` | idem, le mode baisse aussi les E/S et la mémoire |
//!
//! `IDLE` et non best-effort 7 comme le scan : le scan doit avancer pendant la
//! lecture, ces passes-ci, elles, cèdent TOUT à la lecture (elles s'arrêtent
//! dès qu'une zone joue, seul le segment déjà parti va à son terme). Hors
//! lecture, un disque occupé par le scan les fait attendre : c'est voulu.
//!
//! ## Ce qui n'y va JAMAIS
//!
//! * **Une écriture en base.** SQLite n'a qu'un écrivain, sous un verrou que
//!   la lecture attend aussi : un fil baissé qui le tient et se fait préempter
//!   fait attendre la lecture plus longtemps (voir le module parent). Les
//!   écritures des passes restent sur [`super::hors_du_fil_async`].
//! * **Un fil de lecture.** Rien dans ce module ne touche le fil appelant :
//!   seuls les fils nés ici sont baissés, à leur naissance. Le fil de rendu
//!   de la sortie locale (`audio::ordonnancement_rt`) et les décodeurs de la
//!   lecture n'y passent pas (témoin `aucun_chemin_de_lecture_n_y_passe`).
//!
//! ## Le pool
//!
//! Élastique, comme le pool bloquant de tokio : un fil naît quand aucun n'est
//! libre (jusqu'à [`FILS_MAX`]), et meurt après [`INACTIVITE`] sans travail.
//! Un nombre fixe de fils serait un piège : un décodeur qui tourne en rond sur
//! un fichier pathologique garde son fil pour toujours (le délai de la passe
//! abandonne l'attente, pas le fil), et un pool fixe finirait bouché.
//!
//! ## Éteignable
//!
//! `background_playback_policy.background_compute_low_priority` (vrai par
//! défaut) : `false` renvoie le calcul sur le pool bloquant de tokio, comme
//! avant. Les fils déjà nés restent baissés, ils ne reçoivent plus rien et
//! s'éteignent seuls.
//!
//! La baisse elle-même n'est ARMÉE que par le serveur ([`armer`], appelé au
//! lancement des tâches de fond) : les binaires de test de `tune-core`, qui
//! font tourner des milliers de tests en parallèle, ne doivent pas affamer
//! les analyses qu'ils mesurent avec un délai. Le témoin de la baisse prend
//! son propre pool armé ([`Pool::nouveau`]).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;

/// Au-delà, les travaux attendent un fil libre. Très au-dessus de ce que les
/// passes lancent (4 fichiers à la fois au plus, un segment chacun) : il ne
/// sert qu'à borner une avalanche de décodeurs bloqués.
pub const FILS_MAX: usize = 64;

/// Un fil sans travail pendant ce temps s'éteint.
pub const INACTIVITE: Duration = Duration::from_secs(60);

/// Le préfixe du nom des fils : c'est lui que `top -H`, `ps -L` et les
/// journaux montrent.
pub const NOM_DES_FILS: &str = "calcul-fond";

type Travail = Box<dyn FnOnce() + Send + 'static>;

struct File {
    travaux: VecDeque<Travail>,
    inactifs: usize,
    fils: usize,
}

/// Un pool de fils de calcul de fond.
pub struct Pool {
    file: Mutex<File>,
    signal: Condvar,
    /// Les fils nés de ce pool sont-ils baissés ?
    basse: bool,
    numero: AtomicU64,
    faits: AtomicU64,
}

/// Le travail est parti, mais il n'a pas rendu de résultat : il a paniqué.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrompu;

impl std::fmt::Display for Interrompu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("calcul de fond interrompu")
    }
}

impl std::error::Error for Interrompu {}

thread_local! {
    /// Le fil courant est-il un fil de ce module ? Lu par
    /// [`super::politique::baisser_pendant_la_lecture`] : un fil déjà baissé
    /// pour toujours n'a rien à baisser ni, surtout, à RELEVER au `drop`.
    static FIL_DU_POOL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Le fil courant est-il un fil du calcul de fond ?
pub fn fil_du_calcul_de_fond() -> bool {
    FIL_DU_POOL.with(|f| f.get())
}

impl Pool {
    /// Un pool vide : aucun fil ne naît avant le premier travail.
    pub const fn nouveau(basse: bool) -> Self {
        Pool {
            file: Mutex::new(File {
                travaux: VecDeque::new(),
                inactifs: 0,
                fils: 0,
            }),
            signal: Condvar::new(),
            basse,
            numero: AtomicU64::new(0),
            faits: AtomicU64::new(0),
        }
    }

    /// Confier un travail au pool. Un fil naît si les fils libres ne suffisent
    /// pas aux travaux qui attendent.
    pub fn confier(&'static self, travail: Travail) {
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        file.travaux.push_back(travail);
        if file.travaux.len() > file.inactifs && file.fils < FILS_MAX {
            file.fils += 1;
            drop(file);
            let n = self.numero.fetch_add(1, Ordering::Relaxed);
            let lance = std::thread::Builder::new()
                .name(format!("{NOM_DES_FILS}-{n}"))
                .spawn(move || self.ouvrier());
            if lance.is_err() {
                // Pas de fil : le travail attend un fil existant. S'il n'y en a
                // aucun, il ne partira jamais — on le rend à l'appelant en le
                // jetant (son `oneshot` se ferme : `Interrompu`).
                let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
                file.fils -= 1;
                if file.fils == 0 {
                    file.travaux.clear();
                }
                tracing::warn!("calcul_de_fond_fil_impossible");
            }
        } else {
            drop(file);
            self.signal.notify_one();
        }
    }

    fn ouvrier(&'static self) {
        FIL_DU_POOL.with(|f| f.set(true));
        // Ces fils ne servent que le calcul de fond : « de fond » pour toute
        // leur vie (réserve de lecture SQLite, cession de l'écrivain), comme
        // ceux du scan.
        std::mem::forget(super::politique::marquer_le_fil_de_fond());
        if self.basse {
            baisser_pour_toujours();
        }
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(travail) = file.travaux.pop_front() {
                drop(file);
                // Une panique ne tue pas le fil : le `oneshot` du travail se
                // ferme, l'appelant reçoit `Interrompu`.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(travail));
                self.faits.fetch_add(1, Ordering::Relaxed);
                file = self.file.lock().unwrap_or_else(|e| e.into_inner());
                continue;
            }
            file.inactifs += 1;
            let (garde, attente) = self
                .signal
                .wait_timeout(file, INACTIVITE)
                .unwrap_or_else(|e| e.into_inner());
            file = garde;
            file.inactifs -= 1;
            if attente.timed_out() && file.travaux.is_empty() {
                file.fils -= 1;
                return;
            }
        }
    }

    /// Faire tourner `travail` sur un fil de ce pool et en attendre le
    /// résultat sans tenir un fil de l'exécuteur.
    pub async fn executer<T, F>(&'static self, travail: F) -> Result<T, Interrompu>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (envoi, reception) = tokio::sync::oneshot::channel();
        self.confier(Box::new(move || {
            let _ = envoi.send(travail());
        }));
        reception.await.map_err(|_| Interrompu)
    }

    /// Combien de fils vivent, combien attendent, combien de travaux faits.
    pub fn compter(&self) -> (usize, usize, u64) {
        let file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        (file.fils, file.inactifs, self.faits.load(Ordering::Relaxed))
    }
}

/// La baisse est-elle armée pour le pool du processus ? Voir le module.
static ARMEE: AtomicBool = AtomicBool::new(false);

/// Armer la basse priorité des fils du calcul de fond. Appelé par le serveur,
/// une fois, avant de lancer les passes. Les fils nés AVANT restent à la
/// priorité normale ; il n'y en a pas : les passes dorment au démarrage.
pub fn armer() {
    ARMEE.store(true, Ordering::Relaxed);
}

/// Le pool du processus, né au premier travail, armé ou non selon [`armer`].
fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| Pool::nouveau(ARMEE.load(Ordering::Relaxed)))
}

/// Faire tourner un calcul de fond (décodage, mesure, inférence) hors de
/// l'exécuteur : sur un fil du calcul de fond si la politique le veut, sinon
/// sur le pool bloquant de tokio, comme avant.
///
/// ⚠️ Jamais une écriture en base (voir le module), jamais un travail de la
/// lecture.
pub async fn calcul_de_fond<T, F>(travail: F) -> Result<T, Interrompu>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    if !super::politique::politique().background_compute_low_priority {
        return tokio::task::spawn_blocking(travail)
            .await
            .map_err(|_| Interrompu);
    }
    pool().executer(travail).await
}

// ---------------------------------------------------------------------------
// La baisse, par système
// ---------------------------------------------------------------------------

/// Ce qu'un fil du calcul de fond a obtenu, pour le relevé. 0 : rien encore.
static PROCESSEUR: AtomicU8 = AtomicU8::new(0);
static DISQUE: AtomicU8 = AtomicU8::new(0);

const RIEN: u8 = 1;
const NICE_10: u8 = 2;
const IDLE: u8 = 3;
const BEST_EFFORT_7: u8 = 4;
const QOS_BACKGROUND: u8 = 5;
const MODE_BACKGROUND: u8 = 6;

fn nom(code: u8) -> Option<&'static str> {
    match code {
        RIEN => Some("none"),
        NICE_10 => Some("nice_10"),
        IDLE => Some("idle"),
        BEST_EFFORT_7 => Some("best_effort_7"),
        QOS_BACKGROUND => Some("qos_background"),
        MODE_BACKGROUND => Some("thread_mode_background"),
        _ => None,
    }
}

/// Baisser le fil COURANT pour toute sa vie. Seulement depuis
/// [`Pool::ouvrier`] : le fil est à ce module et n'a pas à être relevé.
fn baisser_pour_toujours() {
    let (processeur, disque) = baisser_le_fil_courant();
    PROCESSEUR.store(processeur, Ordering::Relaxed);
    DISQUE.store(disque, Ordering::Relaxed);
    tracing::debug!(
        processeur = nom(processeur),
        disque = nom(disque),
        "calcul_de_fond_fil_baisse"
    );
}

#[cfg(target_os = "linux")]
fn baisser_le_fil_courant() -> (u8, u8) {
    // SAFETY: gettid n'a ni argument ni effet de bord ; setpriority et
    // ioprio_set ne visent que ce fil (sémantique Linux : PRIO_PROCESS avec un
    // tid vise ce seul fil).
    unsafe {
        let tid = libc::syscall(libc::SYS_gettid);
        let processeur = if libc::setpriority(libc::PRIO_PROCESS as _, tid as libc::id_t, 10) == 0 {
            NICE_10
        } else {
            RIEN
        };
        const CLASSE_BE: libc::c_long = 2;
        const CLASSE_IDLE: libc::c_long = 3;
        const DECALAGE: libc::c_long = 13;
        const QUI_PROCESSUS: libc::c_long = 1;
        let idle = libc::syscall(
            libc::SYS_ioprio_set,
            QUI_PROCESSUS,
            tid,
            CLASSE_IDLE << DECALAGE,
        ) == 0;
        let disque = if idle {
            IDLE
        } else if libc::syscall(
            libc::SYS_ioprio_set,
            QUI_PROCESSUS,
            tid,
            (CLASSE_BE << DECALAGE) | 7,
        ) == 0
        {
            BEST_EFFORT_7
        } else {
            RIEN
        };
        (processeur, disque)
    }
}

#[cfg(target_os = "macos")]
fn baisser_le_fil_courant() -> (u8, u8) {
    // SAFETY: change la classe QoS du seul fil courant.
    let ok = unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0) == 0
    };
    if ok {
        (QOS_BACKGROUND, QOS_BACKGROUND)
    } else {
        (RIEN, RIEN)
    }
}

#[cfg(windows)]
fn baisser_le_fil_courant() -> (u8, u8) {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
    };
    // SAFETY: GetCurrentThread rend un pseudo-handle du fil courant, qui n'a
    // pas à être fermé ; le mode ne touche que ce fil.
    let ok = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN).is_ok() };
    if ok {
        (MODE_BACKGROUND, MODE_BACKGROUND)
    } else {
        (RIEN, RIEN)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn baisser_le_fil_courant() -> (u8, u8) {
    (RIEN, RIEN)
}

/// Le relevé du calcul de fond, sous `playback_priority.background_compute`
/// de `GET /system/background-tasks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleveDuCalcul {
    /// La politique envoie le calcul de fond sur ses fils à lui.
    pub low_priority_pool: bool,
    /// Le serveur a armé la baisse de ces fils.
    pub armed: bool,
    /// Ce que le dernier fil né a obtenu : `nice_10`, `qos_background`,
    /// `thread_mode_background`, `none` ; `null` tant qu'aucun fil n'est né.
    pub cpu: Option<&'static str>,
    /// Idem pour le disque : `idle`, `best_effort_7`, `qos_background`,
    /// `thread_mode_background`, `none`.
    pub io: Option<&'static str>,
    /// Fils vivants, dont libres.
    pub threads: usize,
    pub idle_threads: usize,
    /// Travaux faits depuis le démarrage.
    pub jobs_done: u64,
}

/// Le relevé, sans requête.
pub fn releve() -> ReleveDuCalcul {
    let (threads, idle_threads, jobs_done) = pool().compter();
    ReleveDuCalcul {
        low_priority_pool: super::politique::politique().background_compute_low_priority,
        armed: ARMEE.load(Ordering::Relaxed),
        cpu: nom(PROCESSEUR.load(Ordering::Relaxed)),
        io: nom(DISQUE.load(Ordering::Relaxed)),
        threads,
        idle_threads,
        jobs_done,
    }
}

#[cfg(test)]
#[path = "calcul_de_fond_tests.rs"]
mod tests;
