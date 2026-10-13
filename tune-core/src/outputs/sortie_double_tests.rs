//! #6132 — la sortie double sur deux périphériques SIMULÉS, d'horloges
//! différentes, pendant plusieurs minutes.
//!
//! Le banc ne réplique pas le recopieur : il l'appelle tel que le rappel de A
//! l'appelle en production, et simule seulement ce que le matériel fait —
//! deux rappels périodiques, chacun cadencé par son propre quartz, l'anneau
//! SPSC entre les deux, et le rappel de B qui attend sa consigne avant de lire
//! puis comble de zéros quand l'anneau est vide.
//!
//! L'écart est mesuré par ce que chaque périphérique JOUE : à l'instant `t`,
//! A joue la trame source `cadence_A × t − retard`, B joue la trame source
//! portée par l'échantillon qu'il lit à cet instant (retrouvée par le repère
//! `(trame produite, position d'entrée)` que le recopieur publie).

use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;

struct AnneauSimule {
    file: RefCell<VecDeque<f32>>,
    capacite: usize,
    periode_lue: std::cell::Cell<usize>,
    latence_b: std::cell::Cell<Option<f64>>,
    avance_a: std::cell::Cell<f64>,
    maintenant: std::cell::Cell<f64>,
    derniere_lecture: std::cell::Cell<Option<f64>>,
}

impl AnneauSimule {
    fn nouveau(capacite: usize) -> Self {
        Self {
            file: RefCell::new(VecDeque::with_capacity(capacite)),
            capacite,
            periode_lue: std::cell::Cell::new(0),
            latence_b: std::cell::Cell::new(None),
            avance_a: std::cell::Cell::new(0.0),
            maintenant: std::cell::Cell::new(0.0),
            derniere_lecture: std::cell::Cell::new(None),
        }
    }
}

impl AnneauSecondaire for AnneauSimule {
    fn pousser(&self, echantillons: &[f32]) -> usize {
        let mut f = self.file.borrow_mut();
        let n = echantillons.len().min(self.capacite - f.len());
        f.extend(&echantillons[..n]);
        n
    }
    fn en_attente(&self) -> usize {
        self.file.borrow().len()
    }
    fn periode_du_lecteur(&self) -> usize {
        self.periode_lue.get()
    }
    fn latence_du_lecteur_s(&self) -> Option<f64> {
        self.latence_b.get()
    }
    fn depuis_la_derniere_lecture_s(&self) -> Option<f64> {
        self.derniere_lecture
            .get()
            .map(|t| self.maintenant.get() - t)
    }
    fn publier_l_avance_de_a(&self, avance_s: f64) {
        self.avance_a.set(avance_s);
    }
}

/// Un périphérique simulé : cadence nominale, écart de son quartz en ppm,
/// taille de période, latence matérielle (du rappel au convertisseur), et si
/// son pilote la donne dans les horodatages du rappel.
#[derive(Clone, Copy)]
struct Peripherique {
    cadence: u32,
    ppm: f64,
    periode: usize,
    latence_s: f64,
    horodate: bool,
}

impl Peripherique {
    fn latence_annoncee(&self) -> Option<f64> {
        self.horodate.then_some(self.latence_s)
    }
}

impl Peripherique {
    fn cadence_reelle(&self) -> f64 {
        f64::from(self.cadence) * (1.0 + self.ppm * 1e-6)
    }
}

struct Bilan {
    /// Écart A − B en ms, un point par période de B après le préchargement.
    ecarts_ms: Vec<(f64, f64)>,
    /// Trames de B comblées de zéros APRÈS le démarrage de B.
    famines_b: u64,
    releve: Releve,
}

/// Fait tourner A et B `duree_s` secondes de temps simulé.
fn simuler(a: Peripherique, b: Peripherique, canaux: usize, duree_s: f64) -> Bilan {
    simuler_avec_decalage(a, b, canaux, duree_s, 0.0)
}

/// Même banc, avec le décalage manuel `sync_delay_ms` (B − A) en secondes.
///
/// L'écart est mesuré à la SORTIE des convertisseurs : la trame qu'un rappel
/// livre à `t` s'entend à `t + latence` de son périphérique.
fn simuler_avec_decalage(
    a: Peripherique,
    b: Peripherique,
    canaux: usize,
    duree_s: f64,
    manuel_s: f64,
) -> Bilan {
    let consigne = CONSIGNE_PAR_DEFAUT_S;
    let mut rec = Recopieur::nouveau(canaux, a.cadence, b.cadence, consigne);
    rec.regler_le_decalage_manuel(manuel_s);
    let retard_a = (consigne * f64::from(a.cadence)).round();
    let consigne_b = rec.consigne();
    let anneau = AnneauSimule::nouveau((consigne * f64::from(b.cadence)) as usize * canaux * 4);

    // Repères (trame produite, position d'entrée, pas) au début de chaque
    // période de A, pour retrouver la position source d'une trame de B.
    let mut reperes: Vec<(u64, f64, f64)> = Vec::new();
    let mut periode_a = vec![0f32; a.periode * canaux];
    let mut lu_b = vec![0f32; b.periode * canaux];

    let (ra, rb) = (a.cadence_reelle(), b.cadence_reelle());
    let (mut na, mut nb) = (0u64, 0u64);
    let mut source = 0u64;
    let mut b_demarre = false;
    // Trames de B tirées de l'anneau (hors zéros), et zéros joués avant.
    let mut sortis_b = 0u64;
    let mut famines_b = 0u64;
    let mut ecarts_ms = Vec::new();

    loop {
        let ta = (na * a.periode as u64) as f64 / ra;
        let tb = (nb * b.periode as u64) as f64 / rb;
        if ta.min(tb) > duree_s {
            break;
        }
        if ta <= tb {
            for f in 0..a.periode {
                let x = ((source + f as u64) as f64 * 0.01).sin() as f32 * 0.5;
                for c in 0..canaux {
                    periode_a[f * canaux + c] = x;
                }
            }
            source += a.periode as u64;
            let (k, x) = rec.repere();
            anneau.maintenant.set(ta);
            rec.recopier(&mut periode_a, &anneau, a.latence_annoncee());
            reperes.push((k, x, 1.0 / rec.rapport()));
            na += 1;
        } else {
            anneau.latence_b.set(b.latence_annoncee());
            let compensation = anneau.avance_a.get() - b.latence_annoncee().unwrap_or(0.0);
            let seuil_b = consigne_b.seuil_de_depart(b.periode * canaux, compensation);
            if !b_demarre && anneau.en_attente() >= seuil_b {
                b_demarre = true;
            }
            if b_demarre {
                // La trame que B joue au début de cette période.
                let k = sortis_b;
                let i = reperes.partition_point(|r| r.0 <= k).saturating_sub(1);
                let (k0, x0, pas) = reperes[i];
                let pos_b = x0 + (k - k0) as f64 * pas;
                // Ce que A fait entendre à l'instant où cette trame de B sort
                // de son convertisseur.
                let pos_a = ra * (tb + b.latence_s - a.latence_s) - retard_a;
                if tb > 0.5 {
                    ecarts_ms.push((tb, (pos_a - pos_b) / f64::from(a.cadence) * 1000.0));
                }
                anneau.periode_lue.set(lu_b.len());
                anneau.derniere_lecture.set(Some(tb));
                let mut f = anneau.file.borrow_mut();
                let n = lu_b.len().min(f.len());
                for s in lu_b.iter_mut().take(n) {
                    *s = f.pop_front().unwrap();
                }
                sortis_b += (n / canaux) as u64;
                famines_b += ((lu_b.len() - n) / canaux) as u64;
            }
            nb += 1;
        }
    }
    Bilan {
        ecarts_ms,
        famines_b,
        releve: rec.releve(),
    }
}

fn pire_ecart(bilan: &Bilan, apres_s: f64) -> f64 {
    bilan
        .ecarts_ms
        .iter()
        .filter(|(t, _)| *t >= apres_s)
        .map(|(_, e)| e.abs())
        .fold(0.0, f64::max)
}

/// Le témoin principal : 48 kHz des deux côtés, A à +100 ppm, B à −80 ppm
/// (180 ppm d'écart relatif, soit 10,8 ms par minute sans correction), des
/// périodes différentes (10 ms contre 512 trames), dix minutes. Sans
/// correction, l'écart atteindrait 108 ms à la fin.
#[test]
fn deux_horloges_differentes_restent_synchrones_dix_minutes() {
    let a = Peripherique {
        cadence: 48_000,
        ppm: 100.0,
        periode: 480,
        latence_s: 0.0,
        horodate: true,
    };
    let b = Peripherique {
        cadence: 48_000,
        ppm: -80.0,
        periode: 512,
        latence_s: 0.0,
        horodate: true,
    };
    let bilan = simuler(a, b, 1, 600.0);
    let pire = pire_ecart(&bilan, 20.0);
    let depart = pire_ecart(&bilan, 0.0);
    let dernier = bilan.ecarts_ms.last().unwrap().1;
    eprintln!(
        "sortie_double 48k/48k +100/-80 ppm, 600 s : écart au départ ≤ {depart:.3} ms, \
         pire écart de 20 s à 600 s = {pire:.3} ms, écart final = {dernier:.3} ms, \
         dérive estimée = {:.1} ppm, famines B = {}",
        bilan.releve.derive_ppm, bilan.famines_b
    );
    assert!(
        pire < 0.5,
        "écart A/B non borné : {pire:.3} ms entre 20 s et 10 min (attendu < 0,5 ms)"
    );
    // Au départ, B ne sait pas où A en est dans sa période : l'écart initial
    // tient dans une période de A (10 ms) ; l'accrochage le résorbe.
    assert!(
        depart < 10.0 + 1.0,
        "écart A/B au départ : {depart:.3} ms (attendu < une période de A + 1 ms)"
    );
    assert_eq!(
        bilan.famines_b, 0,
        "B a manqué d'échantillons après son départ"
    );
    // La correction a retrouvé la dérive : B consomme 180 ppm moins vite.
    assert!(
        (bilan.releve.derive_ppm + 180.0).abs() < 5.0,
        "dérive estimée {:.1} ppm, attendu ≈ −180 ppm",
        bilan.releve.derive_ppm
    );
}

/// Cadences nominales différentes (44,1 kHz vers 48 kHz), dérive forte
/// (±250 ppm), stéréo : le même régulateur tient.
#[test]
fn cadences_differentes_et_derive_forte_en_stereo() {
    let a = Peripherique {
        cadence: 44_100,
        ppm: -250.0,
        periode: 441,
        latence_s: 0.0,
        horodate: true,
    };
    let b = Peripherique {
        cadence: 48_000,
        ppm: 250.0,
        periode: 1024,
        latence_s: 0.0,
        horodate: true,
    };
    let bilan = simuler(a, b, 2, 240.0);
    let pire = pire_ecart(&bilan, 20.0);
    eprintln!(
        "sortie_double 44,1k/48k -250/+250 ppm stéréo, 240 s : pire écart de 20 s à 240 s = {pire:.3} ms, \
         pire total = {:.3} ms, dérive estimée = {:.1} ppm, famines B = {}",
        pire_ecart(&bilan, 0.0),
        bilan.releve.derive_ppm,
        bilan.famines_b
    );
    assert!(
        pire < 0.5,
        "écart A/B non borné : {pire:.3} ms (attendu < 0,5 ms)"
    );
    assert!(
        (bilan.releve.derive_ppm - 500.0).abs() < 5.0,
        "dérive estimée {:.1} ppm, attendu ≈ +500 ppm",
        bilan.releve.derive_ppm
    );
    assert_eq!(
        bilan.famines_b, 0,
        "B a manqué d'échantillons après son départ"
    );
}

/// Le rééchantillonneur rend le signal : un sinus de 1 kHz passé de 48 kHz à
/// 48 048 Hz (+1 000 ppm), correction en mouvement, se retrouve à la position
/// d'entrée annoncée par le repère, à −80 dB près. C'est ce qui rend le repère du banc ci-dessus
/// digne de confiance : il dit où est vraiment le signal.
#[test]
fn le_signal_recopie_est_a_la_position_annoncee() {
    let canaux = 2;
    let mut rec = Recopieur::nouveau(canaux, 48_000, 48_048, 0.0);
    let anneau = AnneauSimule::nouveau(1 << 22);
    let w = 2.0 * std::f64::consts::PI * 1000.0 / 48_000.0;
    let mut periode = vec![0f32; 480 * canaux];
    let mut source = 0u64;
    let mut reperes = Vec::new();
    for _ in 0..200 {
        for f in 0..480 {
            let x = (w * (source + f as u64) as f64).sin() as f32;
            periode[f * canaux] = x;
            periode[f * canaux + 1] = -x;
        }
        source += 480;
        let (k, x) = rec.repere();
        rec.recopier(&mut periode, &anneau, None);
        reperes.push((k, x, 1.0 / rec.rapport()));
    }
    // Consigne nulle : le régulateur voit un anneau qui se remplit et pousse
    // la correction à sa borne — le pas change d'une période à l'autre, et
    // chaque trame est jugée au pas de SA période.
    let f = anneau.file.borrow();
    let produites = f.len() / canaux;
    let mut pire = 0f64;
    for k in 64..produites as u64 - 64 {
        let i = reperes.partition_point(|r: &(u64, f64, f64)| r.0 <= k) - 1;
        let (k0, x0, pas) = reperes[i];
        let x = x0 + (k - k0) as f64 * pas;
        let attendu = (w * x).sin();
        let g = f[(k as usize) * canaux] as f64;
        let d = f[(k as usize) * canaux + 1] as f64;
        pire = pire.max((g - attendu).abs()).max((d + attendu).abs());
    }
    eprintln!("sortie_double : erreur max du signal recopié = {pire:.2e}");
    assert!(
        pire < 1e-4,
        "signal recopié décalé ou déformé : erreur {pire:.2e}"
    );
}

/// Ce que A joue est la période d'entrée retardée de la consigne, échantillon
/// pour échantillon.
#[test]
fn la_ligne_a_retard_de_a_est_exacte() {
    let mut rec = Recopieur::nouveau(2, 48_000, 48_000, 0.001); // 48 trames
    let anneau = AnneauSimule::nouveau(1 << 16);
    let mut joue = Vec::new();
    for p in 0..4 {
        let mut periode: Vec<f32> = (0..64).map(|i| (p * 64 + i + 1) as f32).collect();
        rec.recopier(&mut periode, &anneau, None);
        joue.extend(periode);
    }
    assert!(joue[..96].iter().all(|s| *s == 0.0));
    let attendu: Vec<f32> = (1..=(256 - 96)).map(|i| i as f32).collect();
    assert_eq!(&joue[96..], &attendu[..]);
}

#[test]
fn la_valeur_du_reglage_donne_le_nom_du_peripherique() {
    assert_eq!(
        nom_de_peripherique("local:SMSL SU-8"),
        Some("SMSL SU-8".into())
    );
    assert_eq!(
        nom_de_peripherique(" TASCAM US-366 "),
        Some("TASCAM US-366".into())
    );
    assert_eq!(nom_de_peripherique("local:"), None);
    assert_eq!(nom_de_peripherique(""), None);
    assert_eq!(cle_de_zone(7), "zone_7_sortie_double");
}

/// #6132 étape 2 — deux cartes de latences matérielles différentes (A : 5 ms,
/// B : 25 ms), horodatées par leur pilote : la compensation automatique
/// nourrit B 20 ms plus tôt, et ce qui sort des deux convertisseurs reste
/// aligné. Sans elle, B s'entendrait 20 ms après A pendant toute l'écoute.
#[test]
fn la_latence_de_chaque_carte_est_compensee_par_les_horodatages() {
    let a = Peripherique {
        cadence: 48_000,
        ppm: 60.0,
        periode: 480,
        latence_s: 0.005,
        horodate: true,
    };
    let b = Peripherique {
        cadence: 48_000,
        ppm: -40.0,
        periode: 512,
        latence_s: 0.025,
        horodate: true,
    };
    let bilan = simuler(a, b, 2, 180.0);
    let pire = pire_ecart(&bilan, 20.0);
    eprintln!(
        "sortie_double latences 5/25 ms horodatées, 180 s : pire écart de 20 s à 180 s = {pire:.3} ms, \
         compensation = {:.3} ms, latences lues A = {:?} ms, B = {:?} ms, famines B = {}",
        bilan.releve.compensation_ms,
        bilan.releve.latence_a_ms,
        bilan.releve.latence_b_ms,
        bilan.famines_b
    );
    assert!(
        pire < 0.5,
        "latences matérielles non compensées : écart entendu {pire:.3} ms (attendu < 0,5 ms)"
    );
    assert!((bilan.releve.compensation_ms + 20.0).abs() < 0.1);
    assert_eq!(bilan.famines_b, 0);
}

/// Le réglage de secours : pilote sans horodatage, mêmes latences. Le
/// décalage manuel `sync_delay_ms` (B − A = −20 ms : A retardé de 20 ms) fait
/// le même travail.
#[test]
fn sans_horodatage_le_decalage_manuel_compense() {
    let a = Peripherique {
        cadence: 48_000,
        ppm: 0.0,
        periode: 480,
        latence_s: 0.005,
        horodate: false,
    };
    let b = Peripherique {
        cadence: 48_000,
        ppm: 100.0,
        periode: 480,
        latence_s: 0.025,
        horodate: false,
    };
    let sans = simuler_avec_decalage(a, b, 1, 60.0, 0.0);
    let avec = simuler_avec_decalage(a, b, 1, 60.0, -0.020);
    let (e_sans, e_avec) = (pire_ecart(&sans, 20.0), pire_ecart(&avec, 20.0));
    eprintln!(
        "sortie_double sans horodatage, 60 s : écart sans réglage = {e_sans:.3} ms, \
         avec sync_delay −20 ms = {e_avec:.3} ms"
    );
    assert!(
        e_sans > 19.0,
        "le banc doit voir l'écart matériel : {e_sans:.3} ms"
    );
    assert!(
        e_avec < 0.5,
        "le décalage manuel ne compense pas : {e_avec:.3} ms"
    );
    assert_eq!(avec.releve.latence_a_ms, None);
}

/// Deux cartes aux périodes ÉGALES (10 ms, le cas WASAPI partagé courant) et
/// aux quartz presque égaux (+10 ppm) : leurs rappels gardent la même phase
/// pendant des minutes. Mesuré à l'instant où A passe, le remplissage de B
/// dépendait de cette phase, et l'écart se calait jusqu'à 5 ms à côté.
#[test]
fn periodes_egales_et_phase_figee_restent_alignees() {
    let a = Peripherique {
        cadence: 48_000,
        ppm: 0.0,
        periode: 480,
        latence_s: 0.010,
        horodate: true,
    };
    let b = Peripherique {
        cadence: 48_000,
        ppm: 10.0,
        periode: 480,
        latence_s: 0.010,
        horodate: true,
    };
    let bilan = simuler(a, b, 2, 300.0);
    let pire = pire_ecart(&bilan, 20.0);
    eprintln!(
        "sortie_double périodes égales, +10 ppm, 300 s : pire écart de 20 s à 300 s = {pire:.3} ms"
    );
    assert!(
        pire < 0.5,
        "écart calé sur la phase des rappels : {pire:.3} ms"
    );
}
