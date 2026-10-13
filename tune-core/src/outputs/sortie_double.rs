//! #6132 — la sortie double : la même écoute sur deux périphériques locaux,
//! gardés synchrones malgré leurs deux horloges.
//!
//! Deux cartes son ont deux quartz. À 100 ppm d'écart — l'ordre de grandeur
//! courant — deux flux ouverts séparément s'éloignent de 6 ms par minute :
//! un écho audible au bout de quelques minutes, un décrochage au bout d'une
//! heure. Rien dans la chaîne locale ne le traitait (seul OAAT horodate).
//!
//! Le principe est celui d'un pont d'horloges (`zita-ajbridge`, `alsa_out`) :
//!
//! - le périphérique **A** est le maître. Son rappel de rendu donne la
//!   cadence ; ce qu'il joue est recopié, période par période, vers **B** ;
//! - la recopie passe par un **rééchantillonneur fin** (sinc fenêtré,
//!   polyphasé) dont le rapport vaut `cadence B / cadence A` corrigé de
//!   quelques ppm ;
//! - la correction est **pilotée par l'écart mesuré** : le remplissage de
//!   l'anneau de B. Si B consomme plus vite que A ne produit, l'anneau se
//!   vide et le rapport monte ; s'il consomme moins vite, l'anneau se remplit
//!   et le rapport baisse. Un régulateur proportionnel-intégral ramène le
//!   remplissage à sa consigne, donc l'écart entre A et B à une constante ;
//! - A est retardé, par une ligne à retard, de la même consigne : ce que A
//!   joue à un instant est ce que B joue au même instant, au remplissage
//!   près de l'anneau de B (une demi-période de B en moyenne).
//!
//! Tout ce qui suit tourne dans le rappel temps réel de A : aucune allocation
//! après la construction, aucun verrou bloquant. Ce module ne dépend ni de
//! cpal ni de la feature `local-audio` : la règle se juge par `cargo test`
//! sous Linux, sur deux périphériques simulés (`sortie_double_tests`).

/// Nombre de coefficients du filtre d'interpolation (pair). 64 : bande
/// passante plate jusqu'à ~0,9 × Nyquist, réjection au-delà de 90 dB.
pub const COEFFICIENTS: usize = 64;
/// Nombre de phases tabulées entre deux échantillons d'entrée ; la phase
/// réelle est interpolée linéairement entre deux phases voisines.
const PHASES: usize = 256;
/// Bande passante du filtre, en fraction de la fréquence de Nyquist la plus
/// basse des deux cadences.
const BANDE_PASSANTE: f64 = 0.92;

/// La correction ne sort jamais de ±2 000 ppm : au-delà, ce n'est plus une
/// dérive de quartz, c'est un périphérique qui ne tient pas sa cadence (ou
/// une cadence nominale mal lue), et corriger plus fort ferait entendre la
/// correction.
pub const CORRECTION_MAX: f64 = 2.0e-3;
/// Pulsation propre de la boucle, en rad/s. 0,1 : la correction suit une
/// dérive en une vingtaine de secondes, sans moduler la hauteur sur le
/// grain d'une période.
const PULSATION_PROPRE: f64 = 0.1;
/// Constante de temps du filtre passe-bas de la mesure, en secondes. Le
/// remplissage de l'anneau de B est une dent de scie de la taille d'une
/// période de B ; c'est sa moyenne qu'on régule.
const LISSAGE_S: f64 = 1.0;

/// Consigne de remplissage par défaut de l'anneau de B, en secondes : de quoi
/// couvrir plusieurs périodes de B (10 ms en WASAPI partagé) et les à-coups
/// d'ordonnancement, sans retarder le départ de façon perceptible.
pub const CONSIGNE_PAR_DEFAUT_S: f64 = 0.040;

/// L'anneau qui alimente le périphérique B, vu du recopieur.
///
/// En production c'est le `RingBuf` SPSC de la sortie locale (producteur : le
/// rappel de A ; consommateur : le rappel de B). Dans les tests, un anneau
/// simulé.
pub trait AnneauSecondaire {
    /// Pousse des échantillons entrelacés ; rend le nombre accepté.
    fn pousser(&self, echantillons: &[f32]) -> usize;
    /// Échantillons entrelacés en attente de lecture par B.
    fn en_attente(&self) -> usize;
    /// Taille, en échantillons entrelacés, de la dernière période que le
    /// rappel de B a lue ; 0 tant qu'il n'a rien lu.
    ///
    /// B lit une période d'un coup : juste avant sa lecture, l'anneau tient
    /// en moyenne une demi-période DE PLUS que son remplissage moyen, et c'est
    /// cette tête-là que B joue. Le recopieur retranche cette demi-période de
    /// sa consigne pour que B ne joue pas en retard d'autant.
    fn periode_du_lecteur(&self) -> usize {
        0
    }
}

/// Table polyphasée d'un sinc fenêtré (Blackman-Harris), chaque phase
/// normalisée à un gain unitaire en continu.
struct TableSinc {
    /// `(PHASES + 1) × COEFFICIENTS` : la phase `PHASES` est la phase 0
    /// décalée d'un échantillon, pour interpoler sans cas particulier.
    coefficients: Box<[f32]>,
}

impl TableSinc {
    fn nouvelle(coupure: f64) -> Self {
        let mut coefficients = vec![0f32; (PHASES + 1) * COEFFICIENTS];
        let demi = (COEFFICIENTS / 2) as f64;
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let ligne = &mut coefficients[p * COEFFICIENTS..(p + 1) * COEFFICIENTS];
            let mut somme = 0.0f64;
            let mut valeurs = [0f64; COEFFICIENTS];
            for (k, v) in valeurs.iter_mut().enumerate() {
                // Coefficient k pondère l'entrée `i - (demi - 1) + k` pour une
                // sortie à la position `i + frac`.
                let t = k as f64 - (demi - 1.0) - frac;
                let sinc = if t.abs() < 1e-12 {
                    1.0
                } else {
                    let x = std::f64::consts::PI * coupure * t;
                    x.sin() / x
                };
                // Fenêtre de Blackman-Harris centrée, largeur `COEFFICIENTS`.
                let n = (t + demi) / (2.0 * demi);
                let w = if (0.0..=1.0).contains(&n) {
                    let a = 2.0 * std::f64::consts::PI * n;
                    0.35875 - 0.48829 * a.cos() + 0.14128 * (2.0 * a).cos()
                        - 0.01168 * (3.0 * a).cos()
                } else {
                    0.0
                };
                *v = coupure * sinc * w;
                somme += *v;
            }
            for (c, v) in ligne.iter_mut().zip(valeurs.iter()) {
                *c = (*v / somme) as f32;
            }
        }
        Self {
            coefficients: coefficients.into_boxed_slice(),
        }
    }
}

/// Régulateur proportionnel-intégral de l'écart, en secondes.
///
/// Le procédé est un intégrateur pur : `d(écart)/dt = dérive + correction`.
/// Avec `kp = 2ζω` et `ki = ω²` (ζ = 1), la boucle est amortie critique ;
/// l'intégrale absorbe la dérive constante, l'erreur statique est nulle.
///
/// Deux régimes. Au départ de B, l'écart initial vaut jusqu'à une période de
/// A (B ne sait pas où en est A dans sa période quand il démarre) : la boucle
/// tourne vite ([`PULSATION_ACCROCHAGE`]) pendant [`DUREE_ACCROCHAGE_S`],
/// puis lentement ([`PULSATION_PROPRE`]) pour ne plus suivre que la dérive
/// des quartz, sans moduler la hauteur au grain de la mesure. Le passage de
/// l'un à l'autre garde la correction intégrale telle quelle.
#[derive(Debug, Clone)]
struct Regulateur {
    integrale_ppm: f64,
    ecart_lisse: Option<f64>,
    correction: f64,
    temps: f64,
}

/// Pulsation de la boucle pendant l'accrochage, en rad/s.
const PULSATION_ACCROCHAGE: f64 = 0.5;
/// Durée de l'accrochage, en secondes après le départ de B.
const DUREE_ACCROCHAGE_S: f64 = 15.0;
/// Lissage de la mesure pendant l'accrochage, en secondes.
const LISSAGE_ACCROCHAGE_S: f64 = 0.3;

impl Regulateur {
    fn nouveau() -> Self {
        Self {
            integrale_ppm: 0.0,
            ecart_lisse: None,
            correction: 0.0,
            temps: 0.0,
        }
    }

    /// Une mesure d'écart `ecart_s` (remplissage − consigne, en secondes),
    /// prise `dt` secondes après la précédente. Rend la correction relative
    /// du rapport (+1e-4 = +100 ppm d'échantillons produits pour B).
    fn mesurer(&mut self, ecart_s: f64, dt: f64) -> f64 {
        let accrochage = self.temps < DUREE_ACCROCHAGE_S;
        self.temps += dt;
        let (w, lissage) = if accrochage {
            (PULSATION_ACCROCHAGE, LISSAGE_ACCROCHAGE_S)
        } else {
            (PULSATION_PROPRE, LISSAGE_S)
        };
        let lisse = match self.ecart_lisse {
            None => ecart_s,
            Some(prec) => prec + (ecart_s - prec) * (dt / lissage).min(1.0),
        };
        self.ecart_lisse = Some(lisse);
        // L'intégrale est tenue directement en correction (sans unité) :
        // changer de gain ne la fait pas sauter.
        // Un anneau trop plein demande MOINS d'échantillons : signe moins.
        let integrale = self.integrale_ppm - w * w * lisse * dt;
        let brute = integrale - 2.0 * w * lisse;
        // Anti-emballement : quand la sortie bute sur sa borne, l'intégrale
        // ne grandit plus dans le sens de la butée.
        if brute.abs() <= CORRECTION_MAX || (integrale - self.integrale_ppm) * brute < 0.0 {
            self.integrale_ppm = integrale.clamp(-CORRECTION_MAX, CORRECTION_MAX);
        }
        self.correction = brute.clamp(-CORRECTION_MAX, CORRECTION_MAX);
        self.correction
    }
}

/// Le remplissage visé de l'anneau de B.
///
/// La consigne nominale est le retard de A ; B y gagnerait deux retards que A
/// n'a pas, qu'on retranche :
///
/// - la demi-largeur du filtre : la trame de B centrée sur l'entrée `x` n'est
///   calculée qu'à l'arrivée de l'entrée `x + COEFFICIENTS / 2` ;
/// - la demi-période de B (voir [`AnneauSecondaire::periode_du_lecteur`]).
///
/// Sans descendre sous une période de B plus 5 ms : en dessous, B frôlerait
/// la famine. Ce plancher n'est atteint qu'avec des périodes de B de plus de
/// ~50 ms ; l'écart qui reste alors est constant, il ne dérive pas.
#[derive(Debug, Clone, Copy)]
pub struct Consigne {
    trames_b: f64,
    filtre_b: f64,
    canaux: usize,
    cadence_b: f64,
}

impl Consigne {
    /// Remplissage visé, en trames de B, pour une période de B de
    /// `periode_b` échantillons entrelacés (0 : inconnue).
    pub fn effective(&self, periode_b: usize) -> f64 {
        let periode_b = (periode_b / self.canaux) as f64;
        let voulue = self.trames_b - periode_b / 2.0 - self.filtre_b;
        let plancher = periode_b + 0.005 * self.cadence_b;
        voulue.max(plancher.min(self.trames_b))
    }

    /// Le seuil, en échantillons entrelacés, à partir duquel le rappel de B
    /// commence à lire, quand sa période est de `periode_b` échantillons.
    /// Partir à ce seuil-là plutôt qu'à la consigne nominale évite un
    /// accrochage de plusieurs millisecondes au départ.
    pub fn seuil_de_depart(&self, periode_b: usize) -> usize {
        // Une demi-période de plus : B part juste avant de lire, quand
        // l'anneau est au sommet de sa dent de scie.
        let trames = self.effective(periode_b) + (periode_b / self.canaux) as f64 / 2.0;
        (trames.round() as usize) * self.canaux
    }
}

/// Ce que le recopieur a vécu, pour le journal et le diagnostic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Releve {
    /// Écart lissé du remplissage de B à sa consigne, en millisecondes.
    pub ecart_ms: f64,
    /// Correction courante du rapport, en ppm (avec le bruit de mesure).
    pub correction_ppm: f64,
    /// Dérive d'horloge estimée de B par rapport à A, en ppm : la part
    /// intégrale de la correction, débarrassée du bruit de la mesure.
    pub derive_ppm: f64,
    /// Échantillons que l'anneau de B a refusés (B arrêté ou bloqué).
    pub echantillons_refuses: u64,
    /// Trames recopiées vers B depuis la construction.
    pub trames_produites: u64,
}

/// La recopie de A vers B : ligne à retard de A, rééchantillonneur et
/// régulateur de B.
pub struct Recopieur {
    canaux: usize,
    cadence_a: f64,
    cadence_b: f64,
    /// `cadence B / cadence A` : trames de B par trame de A, sans correction.
    rapport_nominal: f64,
    table: TableSinc,
    /// Entrées gardées, entrelacées. `entree[..longueur]` est valide ; la
    /// trame 0 du tampon est la trame absolue `base` (la trame 0 est la
    /// première trame réelle de A ; le silence initial est en négatif).
    entree: Box<[f32]>,
    longueur: usize,
    base: i64,
    /// Position absolue (en trames de A) de la prochaine trame de B.
    position: f64,
    sortie: Box<[f32]>,
    regulateur: Regulateur,
    consigne_trames_b: f64,
    /// Ligne à retard de A, `retard_trames × canaux`, initialisée à zéro.
    retard: Box<[f32]>,
    index_retard: usize,
    echantillons_refuses: u64,
    trames_produites: u64,
}

/// Plus grande période de A traitée d'un bloc ; une période plus longue est
/// découpée, sans allocation.
const BLOC_MAX: usize = 8192;

impl Recopieur {
    /// `consigne_s` : remplissage visé de l'anneau de B, donc retard ajouté
    /// aux deux sorties. L'anneau de B doit pouvoir en tenir au moins le
    /// double.
    pub fn nouveau(canaux: usize, cadence_a: u32, cadence_b: u32, consigne_s: f64) -> Self {
        let canaux = canaux.max(1);
        let cadence_a = f64::from(cadence_a.max(1));
        let cadence_b = f64::from(cadence_b.max(1));
        let rapport_nominal = cadence_b / cadence_a;
        let coupure = BANDE_PASSANTE * rapport_nominal.min(1.0);
        let retard_trames = (consigne_s * cadence_a).round().max(0.0) as usize;
        let sortie_max =
            ((BLOC_MAX as f64 * rapport_nominal * (1.0 + CORRECTION_MAX)).ceil() as usize + 2)
                * canaux;
        Self {
            canaux,
            cadence_a,
            cadence_b,
            rapport_nominal,
            table: TableSinc::nouvelle(coupure),
            entree: vec![0f32; (BLOC_MAX + 2 * COEFFICIENTS) * canaux].into_boxed_slice(),
            // Le tampon démarre sur `COEFFICIENTS / 2 - 1` trames de silence :
            // la première trame de B est centrée sur la première trame de A.
            longueur: (COEFFICIENTS / 2 - 1) * canaux,
            base: -((COEFFICIENTS / 2 - 1) as i64),
            position: (COEFFICIENTS / 2 - 1) as f64,
            sortie: vec![0f32; sortie_max].into_boxed_slice(),
            regulateur: Regulateur::nouveau(),
            consigne_trames_b: consigne_s * cadence_b,
            retard: vec![0f32; retard_trames * canaux].into_boxed_slice(),
            index_retard: 0,
            echantillons_refuses: 0,
            trames_produites: 0,
        }
    }

    /// La consigne de remplissage de B, à copier dans le rappel de B pour son
    /// seuil de départ.
    pub fn consigne(&self) -> Consigne {
        Consigne {
            trames_b: self.consigne_trames_b,
            filtre_b: (COEFFICIENTS / 2) as f64 * self.rapport_nominal,
            canaux: self.canaux,
            cadence_b: self.cadence_b,
        }
    }

    /// Rapport courant, correction comprise.
    pub fn rapport(&self) -> f64 {
        self.rapport_nominal * (1.0 + self.regulateur.correction)
    }

    /// Position absolue, en trames de A (avant retard), de la PROCHAINE
    /// trame produite pour B, et numéro de cette trame. Sert à mesurer
    /// l'écart réel dans les tests.
    pub fn repere(&self) -> (u64, f64) {
        (self.trames_produites, self.base as f64 + self.position)
    }

    pub fn releve(&self) -> Releve {
        Releve {
            ecart_ms: self.regulateur.ecart_lisse.unwrap_or(0.0) * 1000.0,
            correction_ppm: self.regulateur.correction * 1e6,
            derive_ppm: self.regulateur.integrale_ppm * 1e6,
            echantillons_refuses: self.echantillons_refuses,
            trames_produites: self.trames_produites,
        }
    }

    /// Une période de A : `periode` (entrelacée, déjà au volume) est recopiée
    /// vers B, puis remplacée en place par sa version retardée — ce que A
    /// doit réellement jouer.
    pub fn recopier(&mut self, periode: &mut [f32], anneau: &dyn AnneauSecondaire) {
        let ch = self.canaux;
        let trames = periode.len() / ch;
        if trames == 0 {
            return;
        }
        // 1. Mesure de l'écart : remplissage de B AVANT cette période, contre
        //    la consigne corrigée des deux retards propres à la recopie (la
        //    demi-largeur du filtre, la demi-période de B).
        //    Tant que B n'a pas commencé à lire, l'anneau se remplit jusqu'au
        //    seuil de départ par construction : il n'y a rien à corriger, et
        //    réguler là-dessus emballerait l'intégrale avant le départ.
        let periode_b = anneau.periode_du_lecteur();
        if periode_b > 0 {
            let rempli = anneau.en_attente() as f64 / ch as f64;
            let visee = self.consigne().effective(periode_b);
            let ecart_s = (rempli - visee) / self.cadence_b;
            let dt = trames as f64 / self.cadence_a;
            self.regulateur.mesurer(ecart_s, dt);
        }
        let pas = 1.0 / self.rapport();

        // 2. Rééchantillonnage par blocs de BLOC_MAX trames.
        for bloc in periode[..trames * ch].chunks(BLOC_MAX * ch) {
            self.reechantillonner(bloc, pas, anneau);
        }

        // 3. Ligne à retard de A.
        if !self.retard.is_empty() {
            for s in periode[..trames * ch].iter_mut() {
                std::mem::swap(&mut self.retard[self.index_retard], s);
                self.index_retard += 1;
                if self.index_retard == self.retard.len() {
                    self.index_retard = 0;
                }
            }
        }
    }

    fn reechantillonner(&mut self, bloc: &[f32], pas: f64, anneau: &dyn AnneauSecondaire) {
        let ch = self.canaux;
        let demi = COEFFICIENTS / 2;
        self.entree[self.longueur..self.longueur + bloc.len()].copy_from_slice(bloc);
        self.longueur += bloc.len();
        let disponibles = self.longueur / ch;

        let mut n = 0usize;
        // La sortie à `position` lit les entrées `i - (demi-1) ..= i + demi`.
        while (self.position.floor() as usize) + demi < disponibles {
            let i = self.position.floor() as usize;
            let frac = (self.position - i as f64) * PHASES as f64;
            let p = (frac.floor() as usize).min(PHASES - 1);
            let mix = (frac - p as f64) as f32;
            let l0 = &self.table.coefficients[p * COEFFICIENTS..(p + 1) * COEFFICIENTS];
            let l1 = &self.table.coefficients[(p + 1) * COEFFICIENTS..(p + 2) * COEFFICIENTS];
            let debut = i + 1 - demi;
            for c in 0..ch {
                let mut a0 = 0f32;
                let mut a1 = 0f32;
                for k in 0..COEFFICIENTS {
                    let x = self.entree[(debut + k) * ch + c];
                    a0 += l0[k] * x;
                    a1 += l1[k] * x;
                }
                self.sortie[n * ch + c] = a0 + (a1 - a0) * mix;
            }
            n += 1;
            self.position += pas;
        }

        if n > 0 {
            let voulu = n * ch;
            let pris = anneau.pousser(&self.sortie[..voulu]);
            self.echantillons_refuses += (voulu - pris) as u64;
            self.trames_produites += n as u64;
        }

        // Ne garder que ce que les sorties suivantes liront encore.
        let premiere_utile = (self.position.floor() as usize + 1).saturating_sub(demi);
        if premiere_utile > 0 {
            let debut = premiere_utile * ch;
            self.entree.copy_within(debut..self.longueur, 0);
            self.longueur -= debut;
            self.base += premiere_utile as i64;
            self.position -= premiere_utile as f64;
        }
    }

    pub fn canaux(&self) -> usize {
        self.canaux
    }

    pub fn cadences(&self) -> (f64, f64) {
        (self.cadence_a, self.cadence_b)
    }
}

/// La clé de réglage de la seconde sortie d'une zone. Valeur : l'identifiant
/// d'une sortie locale (`local:<nom>`, comme `output_device_id`) ou son nom ;
/// absente ou vide = pas de sortie double.
pub fn cle_de_zone(zone_id: i64) -> String {
    format!("zone_{zone_id}_sortie_double")
}

/// Le nom du périphérique B réglé sur la zone, préfixe `local:` retiré.
pub fn seconde_sortie_de_zone(
    db: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
    zone_id: i64,
) -> Option<String> {
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get(&cle_de_zone(zone_id))
        .ok()
        .flatten()
        .and_then(|v| nom_de_peripherique(&v))
}

/// `local:<nom>` ou `<nom>` vers `<nom>` ; vide vers `None`.
pub fn nom_de_peripherique(valeur: &str) -> Option<String> {
    let v = valeur.trim();
    let v = v.strip_prefix("local:").unwrap_or(v).trim();
    (!v.is_empty()).then(|| v.to_string())
}

#[cfg(test)]
#[path = "sortie_double_tests.rs"]
mod tests;
