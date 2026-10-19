//! La base OPRA (#6532, fil 2334 ; #6388, fil 2282) : seconde source de
//! corrections de casques, à côté d'AutoEq ([`super::autoeq_base`]).
//!
//! ## La source
//!
//! [OPRA](https://github.com/opra-project/OPRA) est un annuaire ouvert de
//! fabricants, de casques et de courbes d'égalisation, lancé par Roon Labs.
//! Son code est sous licence MIT ; ses DONNÉES (fabricants, produits,
//! courbes) sont sous **CC BY-SA 4.0**, avec une attribution à deux niveaux
//! (README du dépôt) : un navigateur de la base montre le logo OPRA, une
//! courte description et un lien vers le dépôt ; une courbe importée crédite
//! son auteur, plus visiblement qu'OPRA, avec le lien vers le dépôt. C'est le
//! client qui l'affiche ; le serveur lui en donne la matière
//! ([`AttributionOpra`], et `author`/`details`/`link` de chaque courbe).
//!
//! `docs/CONSUMING.md` publie la base en UN fichier, `database_v1.jsonl`, une
//! entrée par ligne :
//!
//! ```text
//! {"type":"vendor","id":"sennheiser","data":{"name":"Sennheiser",…}}
//! {"type":"product","id":"sennheiser::hd650","data":{"name":"HD650","vendor_id":"sennheiser",…}}
//! {"type":"eq","id":"sennheiser:hd650::oratory1990_harman_target","data":{"author":"oratory1990",…}}
//! ```
//!
//! et demande de passer par le miroir de Roon Labs
//! (`opra.roonlabs.net`, servi par Cloudflare) plutôt que par GitHub. Le
//! fichier faisait 12 Mo et 19 538 lignes le 19/10/2026 ; une semaine de
//! copie locale suffit largement à ne pas le solliciter.
//!
//! ## Mêmes règles que la base AutoEq
//!
//! - C'est le SERVEUR qui télécharge, et il garde une copie dans
//!   `<données>/cache/opra` ; sans réseau, la copie sert, même périmée ; sans
//!   réseau NI copie, [`ErreurBaseOpra::HorsLigne`] le dit, jamais une liste
//!   vide.
//! - Une seule URL est jamais lue, fixe : le fichier de la base. L'import
//!   désigne une courbe par son `id` dans CE fichier ; il ne télécharge rien
//!   d'autre et le client ne fournit aucune URL.
//!
//! ## La conversion vers l'égaliseur de Tune
//!
//! [`convertir`] traduit une courbe OPRA (`parametric_eq`) en
//! [`ProfilAutoEq`], le modèle que produit déjà l'import d'un fichier
//! ParametricEQ : même enregistrement, même réserve de marge ensuite.
//!
//! | OPRA | Tune ([`EqBandSpec::band_type`]) |
//! |---|---|
//! | `peak_dip` (`frequency`, `gain_db`, `q`) | `peak` |
//! | `low_shelf` / `high_shelf` | `low_shelf` / `high_shelf` |
//! | `low_pass` / `high_pass`, `slope` 12 dB/oct | `low_pass` / `high_pass`, Q de Butterworth |
//! | `band_stop` (`q`) | `notch` |
//! | `band_pass`, pente ≠ 12 dB/oct, type inconnu | **refusé** |
//!
//! Le `gain_db` global de la courbe joue le rôle du `Preamp` d'AutoEq : il
//! est rendu, pas appliqué, parce que Tune réserve déjà sa propre marge (voir
//! l'en-tête de [`super::autoeq`]).
//!
//! Un refus porte sur la courbe ENTIÈRE : une correction amputée d'un filtre
//! est une autre correction, qui s'entendrait sans que rien l'ait dit.
//!
//! Les plateaux de Tune sont construits à pente S = 1 (Q ≈ 0,707) et leur
//! `q` n'entre pas dans les coefficients. Sur la base du 19/10/2026, 391
//! courbes sur 12 594 portent un plateau dont le Q s'en écarte de plus de
//! 0,1 (0,35, 0,48…). Elles sont importées, mais chaque plateau concerné est
//! NOMMÉ ([`PlateauApproche`]) : la réponse le dit au lieu de le taire.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::autoeq::{FREQ_MAX_HZ, GAIN_MAX_DB, ProfilAutoEq, Q_MAX, Q_MIN};
use super::autoeq_base::{
    FRAICHEUR_PAR_DEFAUT, TelechargeurAutoEq, TelechargeurHttp, ecrire_atomiquement,
};
use super::eq::EqBandSpec;

/// Le fichier de la base, sur le miroir que `docs/CONSUMING.md` désigne.
pub const URL_OPRA: &str = "https://opra.roonlabs.net/database_v1.jsonl";
/// Le dépôt, que toute attribution doit citer.
pub const DEPOT_OPRA: &str = "https://github.com/opra-project/OPRA";
/// La licence des DONNÉES d'OPRA (le code est sous MIT).
pub const LICENCE_DONNEES_OPRA: &str = "CC BY-SA 4.0";
pub const URL_LICENCE_DONNEES_OPRA: &str = "https://creativecommons.org/licenses/by-sa/4.0/";

/// `docs/CONSUMING.md` prévoit 5 000 à 50 000 entrées ; 12 Mo pour 19 538 en
/// 2026. Au-delà de 64 Mo, ce n'est plus la base OPRA.
pub const TAILLE_MAX_BASE: usize = 64 * 1024 * 1024;

const FICHIER_BASE: &str = "database_v1.jsonl";

/// L'écart de Q au-delà duquel un plateau est déclaré approché : Tune le
/// construit à Q = 1/√2, et OPRA écrit 0,7 ou 0,71 pour la pente standard.
const ECART_Q_PLATEAU: f64 = 0.05;

/// Une bande telle qu'OPRA l'écrit (`schemas/eq_info.json`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct BandeOpra {
    #[serde(rename = "type")]
    pub type_filtre: String,
    pub frequency: f64,
    #[serde(default)]
    pub gain_db: f64,
    #[serde(default)]
    pub q: Option<f64>,
    /// En dB/octave, pour `low_pass` et `high_pass` (12 par défaut).
    #[serde(default)]
    pub slope: Option<f64>,
}

/// Les paramètres d'une courbe `parametric_eq`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ParametresOpra {
    #[serde(default)]
    pub gain_db: f64,
    #[serde(default)]
    pub bands: Vec<BandeOpra>,
}

/// Une courbe de la base, avec son casque et son fabricant.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CourbeOpra {
    /// L'identifiant OPRA (« sennheiser:hd650::oratory1990_harman_target ») :
    /// ce que le client renvoie pour l'importer.
    pub id: String,
    /// Le fabricant (« Sennheiser »).
    #[serde(rename = "vendor")]
    pub fabricant: String,
    /// Le modèle (« HD650 »).
    #[serde(rename = "model")]
    pub modele: String,
    /// La forme (`over_the_ear`, `on_ear`, `in_ear`, `earbuds`).
    #[serde(rename = "form")]
    pub forme: Option<String>,
    /// L'auteur de la courbe (« oratory1990 », « AutoEQ »…) : c'est lui que
    /// l'attribution met en avant.
    #[serde(rename = "author")]
    pub auteur: String,
    /// La cible et les précisions (« Harman Target • 6 Band EQ »).
    pub details: Option<String>,
    /// Le lien vers la source de la courbe, quand OPRA en donne un.
    #[serde(rename = "link")]
    pub lien: Option<String>,
    /// Le type de courbe ; seul `parametric_eq` est retenu dans le catalogue.
    #[serde(skip)]
    pub type_courbe: String,
    #[serde(skip)]
    pub parametres: ParametresOpra,
}

impl CourbeOpra {
    /// « Sennheiser HD650 ».
    pub fn nom_affiche(&self) -> String {
        format!("{} {}", self.fabricant, self.modele)
    }
}

#[derive(Deserialize)]
struct LigneBase {
    #[serde(rename = "type")]
    genre: String,
    id: String,
    data: serde_json::Value,
}

#[derive(Deserialize)]
struct DonneesFabricant {
    name: String,
}

#[derive(Deserialize)]
struct DonneesProduit {
    name: String,
    #[serde(default)]
    vendor_id: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
}

#[derive(Deserialize)]
struct DonneesCourbe {
    #[serde(default)]
    product_id: Option<String>,
    author: String,
    #[serde(default)]
    details: Option<String>,
    #[serde(default)]
    link: Option<String>,
    #[serde(rename = "type", default = "type_par_defaut")]
    type_courbe: String,
    parameters: ParametresOpra,
}

fn type_par_defaut() -> String {
    "parametric_eq".into()
}

/// Lit `database_v1.jsonl`. Une ligne illisible, une courbe sans produit
/// connu ou d'un autre type que `parametric_eq` est écartée, pas fatale :
/// la base grossit sans prévenir. L'ordre des lignes n'est pas garanti (une
/// courbe peut précéder son produit), d'où les deux passes.
pub fn analyser_base(texte: &str) -> Vec<CourbeOpra> {
    let mut fabricants: HashMap<String, String> = HashMap::new();
    let mut produits: HashMap<String, DonneesProduit> = HashMap::new();
    let mut courbes: Vec<(String, DonneesCourbe)> = Vec::new();
    for ligne in texte.lines() {
        let Ok(l) = serde_json::from_str::<LigneBase>(ligne) else {
            continue;
        };
        match l.genre.as_str() {
            "vendor" => {
                if let Ok(f) = serde_json::from_value::<DonneesFabricant>(l.data) {
                    fabricants.insert(l.id, f.name);
                }
            }
            "product" => {
                if let Ok(p) = serde_json::from_value::<DonneesProduit>(l.data) {
                    produits.insert(l.id, p);
                }
            }
            "eq" => {
                if let Ok(c) = serde_json::from_value::<DonneesCourbe>(l.data) {
                    courbes.push((l.id, c));
                }
            }
            _ => {}
        }
    }
    courbes
        .into_iter()
        .filter(|(_, c)| c.type_courbe == "parametric_eq")
        .filter_map(|(id, c)| {
            // `product_id` est posé par la génération de `dist/` ; à défaut,
            // l'identifiant de la courbe le contient (`fab:prod::courbe`).
            let produit_id = c.product_id.clone().or_else(|| {
                let (avant, _) = id.split_once("::")?;
                let (fab, prod) = avant.split_once(':')?;
                Some(format!("{fab}::{prod}"))
            })?;
            let produit = produits.get(&produit_id)?;
            let fabricant_id = produit
                .vendor_id
                .clone()
                .or_else(|| produit_id.split_once("::").map(|(f, _)| f.to_string()))?;
            let fabricant = fabricants
                .get(&fabricant_id)
                .cloned()
                .unwrap_or(fabricant_id);
            Some(CourbeOpra {
                id,
                fabricant,
                modele: produit.name.clone(),
                forme: produit.subtype.clone(),
                auteur: c.author,
                details: c.details.filter(|d| !d.trim().is_empty()),
                lien: c.link.filter(|l| !l.trim().is_empty()),
                type_courbe: c.type_courbe,
                parametres: c.parameters,
            })
        })
        .collect()
}

/// Un plateau dont le Q n'est pas reproduit : Tune le joue à pente S = 1.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlateauApproche {
    /// Le rang de la bande dans la courbe, à partir de 1.
    #[serde(rename = "band")]
    pub bande: usize,
    /// `low_shelf` ou `high_shelf`.
    #[serde(rename = "type")]
    pub type_filtre: String,
    /// Le Q demandé par la courbe.
    #[serde(rename = "requested_q")]
    pub q_demande: f64,
}

/// Une courbe OPRA convertie pour l'égaliseur de Tune.
#[derive(Debug, Clone)]
pub struct ConversionOpra {
    /// Le même modèle que l'import d'un fichier ParametricEQ ; `preamp_db`
    /// porte le `gain_db` global de la courbe.
    pub profil: ProfilAutoEq,
    pub plateaux_approches: Vec<PlateauApproche>,
}

/// Ce qui empêche de convertir une courbe. Rien n'est importé.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ErreurConversionOpra {
    #[error("type de courbe « {0} » non pris en charge (attendu parametric_eq)")]
    TypeDeCourbe(String),
    #[error("la courbe ne contient aucune bande")]
    AucuneBande,
    #[error(
        "bande {bande} : filtre « {type_filtre} » non pris en charge par l'égaliseur de Tune ({raison}) ; rien n'a été importé"
    )]
    FiltreNonPrisEnCharge {
        bande: usize,
        type_filtre: String,
        raison: String,
    },
    #[error("bande {bande} : champ « {champ} » absent")]
    ChampManquant { bande: usize, champ: &'static str },
    #[error(
        "bande {bande} : « {champ} » vaut {valeur}, hors du domaine reproductible [{borne_basse} ; {borne_haute}]"
    )]
    ValeurHorsDomaine {
        /// 0 : le gain global de la courbe.
        bande: usize,
        champ: &'static str,
        valeur: f64,
        borne_basse: f64,
        borne_haute: f64,
    },
}

impl ErreurConversionOpra {
    /// La clé stable que porte la réponse HTTP.
    pub fn cle(&self) -> &'static str {
        match self {
            Self::FiltreNonPrisEnCharge { .. } | Self::TypeDeCourbe(_) => "opra_filter_unsupported",
            Self::AucuneBande | Self::ChampManquant { .. } | Self::ValeurHorsDomaine { .. } => {
                "opra_curve_invalid"
            }
        }
    }
}

fn domaine(
    valeur: f64,
    champ: &'static str,
    bande: usize,
    borne_basse: f64,
    borne_haute: f64,
) -> Result<f64, ErreurConversionOpra> {
    if !valeur.is_finite() || valeur < borne_basse || valeur > borne_haute {
        return Err(ErreurConversionOpra::ValeurHorsDomaine {
            bande,
            champ,
            valeur,
            borne_basse,
            borne_haute,
        });
    }
    Ok(valeur)
}

/// Convertit une courbe OPRA en profil de l'égaliseur de Tune, ou la refuse
/// entière (voir l'en-tête du module).
pub fn convertir(courbe: &CourbeOpra) -> Result<ConversionOpra, ErreurConversionOpra> {
    if courbe.type_courbe != "parametric_eq" {
        return Err(ErreurConversionOpra::TypeDeCourbe(
            courbe.type_courbe.clone(),
        ));
    }
    let p = &courbe.parametres;
    if p.bands.is_empty() {
        return Err(ErreurConversionOpra::AucuneBande);
    }
    let preamp_db = domaine(p.gain_db, "gain_db", 0, -GAIN_MAX_DB, GAIN_MAX_DB)?;
    let mut bandes = Vec::with_capacity(p.bands.len());
    let mut plateaux_approches = Vec::new();
    for (i, b) in p.bands.iter().enumerate() {
        let rang = i + 1;
        let freq = domaine(b.frequency, "frequency", rang, 1.0, FREQ_MAX_HZ)?;
        let q_obligatoire = || -> Result<f64, ErreurConversionOpra> {
            let q = b.q.ok_or(ErreurConversionOpra::ChampManquant {
                bande: rang,
                champ: "q",
            })?;
            domaine(q, "q", rang, Q_MIN, Q_MAX)
        };
        let gain = || domaine(b.gain_db, "gain_db", rang, -GAIN_MAX_DB, GAIN_MAX_DB);
        let (band_type, gain, q) = match b.type_filtre.as_str() {
            "peak_dip" => ("peak", gain()?, q_obligatoire()?),
            "low_shelf" | "high_shelf" => {
                let q = match b.q {
                    Some(q) => domaine(q, "q", rang, Q_MIN, Q_MAX)?,
                    None => std::f64::consts::FRAC_1_SQRT_2,
                };
                if (q - std::f64::consts::FRAC_1_SQRT_2).abs() > ECART_Q_PLATEAU {
                    plateaux_approches.push(PlateauApproche {
                        bande: rang,
                        type_filtre: b.type_filtre.clone(),
                        q_demande: q,
                    });
                }
                let t = if b.type_filtre == "low_shelf" {
                    "low_shelf"
                } else {
                    "high_shelf"
                };
                (t, gain()?, q)
            }
            "low_pass" | "high_pass" => {
                // Un biquad RBJ de Butterworth descend de 12 dB/octave : c'est
                // la seule pente que l'égaliseur sait construire.
                let pente = b.slope.unwrap_or(12.0);
                if pente != 12.0 {
                    return Err(ErreurConversionOpra::FiltreNonPrisEnCharge {
                        bande: rang,
                        type_filtre: b.type_filtre.clone(),
                        raison: format!(
                            "pente de {pente} dB/octave : l'égaliseur ne construit que 12 dB/octave"
                        ),
                    });
                }
                let q = match b.q {
                    Some(q) => domaine(q, "q", rang, Q_MIN, Q_MAX)?,
                    None => std::f64::consts::FRAC_1_SQRT_2,
                };
                let t = if b.type_filtre == "low_pass" {
                    "low_pass"
                } else {
                    "high_pass"
                };
                (t, 0.0, q)
            }
            "band_stop" => ("notch", 0.0, q_obligatoire()?),
            "band_pass" => {
                return Err(ErreurConversionOpra::FiltreNonPrisEnCharge {
                    bande: rang,
                    type_filtre: b.type_filtre.clone(),
                    raison: "l'égaliseur n'a pas de filtre passe-bande".into(),
                });
            }
            autre => {
                return Err(ErreurConversionOpra::FiltreNonPrisEnCharge {
                    bande: rang,
                    type_filtre: autre.to_string(),
                    raison: "type inconnu".into(),
                });
            }
        };
        bandes.push(EqBandSpec {
            freq,
            gain,
            q,
            band_type: band_type.to_string(),
            // Une correction de casque vaut pour les deux oreilles.
            channel: None,
        });
    }
    Ok(ConversionOpra {
        profil: ProfilAutoEq {
            preamp_db,
            bandes,
            ignores: Vec::new(),
        },
        plateaux_approches,
    })
}

/// Ce que le client doit afficher pour respecter la licence d'OPRA
/// (README du dépôt, « How is this repository licensed? »).
#[derive(Debug, Clone, Serialize)]
pub struct AttributionOpra {
    pub name: &'static str,
    pub repository: &'static str,
    pub license: &'static str,
    pub license_url: &'static str,
}

pub const ATTRIBUTION_OPRA: AttributionOpra = AttributionOpra {
    name: "OPRA",
    repository: DEPOT_OPRA,
    license: LICENCE_DONNEES_OPRA,
    license_url: URL_LICENCE_DONNEES_OPRA,
};

/// Ce qui empêche de chercher ou d'importer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ErreurBaseOpra {
    /// Ni réseau, ni copie locale.
    #[error("la base OPRA est injoignable et aucune copie locale n'existe : {0}")]
    HorsLigne(String),
    /// L'identifiant demandé n'est pas dans la base.
    #[error("courbe OPRA inconnue : {0}")]
    CourbeInconnue(String),
    /// Ce qui est arrivé n'est pas la base OPRA.
    #[error("la base OPRA reçue est illisible")]
    BaseIllisible,
}

struct BaseEnMemoire {
    courbes: Arc<Vec<CourbeOpra>>,
    releve: SystemTime,
}

/// La base OPRA vue du serveur, avec sa copie locale.
pub struct CatalogueOpra {
    url: String,
    telechargeur: Arc<dyn TelechargeurAutoEq>,
    /// `None` : pas de copie sur disque (base `:memory:`, témoins).
    dossier: Option<PathBuf>,
    fraicheur: Duration,
    base: tokio::sync::Mutex<Option<BaseEnMemoire>>,
}

impl CatalogueOpra {
    pub fn new(
        url: impl Into<String>,
        telechargeur: Arc<dyn TelechargeurAutoEq>,
        dossier: Option<PathBuf>,
    ) -> Self {
        Self {
            url: url.into(),
            telechargeur,
            dossier,
            fraicheur: FRAICHEUR_PAR_DEFAUT,
            base: tokio::sync::Mutex::new(None),
        }
    }

    /// Le catalogue de production : le miroir de Roon Labs, copie dans
    /// `dossier`.
    pub fn production(dossier: Option<PathBuf>) -> Self {
        Self::new(URL_OPRA, Arc::new(TelechargeurHttp::new()), dossier)
    }

    pub fn avec_fraicheur(mut self, fraicheur: Duration) -> Self {
        self.fraicheur = fraicheur;
        self
    }

    fn frais(&self, releve: SystemTime) -> bool {
        SystemTime::now()
            .duration_since(releve)
            .is_ok_and(|age| age < self.fraicheur)
    }

    /// Les courbes : la mémoire si elle est fraîche, sinon le réseau, et à
    /// défaut de réseau la copie la plus récente, même périmée.
    pub async fn courbes(&self) -> Result<Arc<Vec<CourbeOpra>>, ErreurBaseOpra> {
        let mut garde = self.base.lock().await;
        if let Some(b) = garde.as_ref()
            && self.frais(b.releve)
        {
            return Ok(b.courbes.clone());
        }
        if garde.is_none()
            && let Some((texte, releve)) = self.lire_copie()
        {
            let courbes = analyser_base(&texte);
            if !courbes.is_empty() {
                *garde = Some(BaseEnMemoire {
                    courbes: Arc::new(courbes),
                    releve,
                });
                if let Some(b) = garde.as_ref()
                    && self.frais(b.releve)
                {
                    return Ok(b.courbes.clone());
                }
            }
        }
        match self
            .telechargeur
            .telecharger(&self.url, TAILLE_MAX_BASE)
            .await
        {
            Ok(texte) => {
                let courbes = analyser_base(&texte);
                if courbes.is_empty() {
                    tracing::warn!(octets = texte.len(), "opra_base_illisible");
                    return garde
                        .as_ref()
                        .map(|b| b.courbes.clone())
                        .ok_or(ErreurBaseOpra::BaseIllisible);
                }
                tracing::info!(courbes = courbes.len(), "opra_base_telechargee");
                self.ecrire_copie(&texte);
                let courbes = Arc::new(courbes);
                *garde = Some(BaseEnMemoire {
                    courbes: courbes.clone(),
                    releve: SystemTime::now(),
                });
                Ok(courbes)
            }
            Err(erreur) => match garde.as_ref() {
                Some(b) => {
                    tracing::warn!(%erreur, "opra_hors_ligne_copie_servie");
                    Ok(b.courbes.clone())
                }
                None => {
                    tracing::warn!(%erreur, "opra_hors_ligne_sans_copie");
                    Err(ErreurBaseOpra::HorsLigne(erreur))
                }
            },
        }
    }

    /// La courbe `id`, prise dans la base déjà lue : aucun autre
    /// téléchargement.
    pub async fn courbe(&self, id: &str) -> Result<CourbeOpra, ErreurBaseOpra> {
        let courbes = self.courbes().await?;
        courbes
            .iter()
            .find(|c| c.id == id)
            .cloned()
            .ok_or_else(|| ErreurBaseOpra::CourbeInconnue(id.to_string()))
    }

    fn lire_copie(&self) -> Option<(String, SystemTime)> {
        let chemin = self.dossier.as_ref()?.join(FICHIER_BASE);
        let releve = std::fs::metadata(&chemin).and_then(|m| m.modified()).ok()?;
        let texte = std::fs::read_to_string(&chemin).ok()?;
        Some((texte, releve))
    }

    fn ecrire_copie(&self, texte: &str) {
        let Some(dossier) = self.dossier.as_ref() else {
            return;
        };
        if let Err(err) = ecrire_atomiquement(&dossier.join(FICHIER_BASE), texte) {
            tracing::warn!(%err, "opra_copie_locale_impossible");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Onze lignes RÉELLES de `database_v1.jsonl` (19/10/2026), CC BY-SA 4.0 :
    /// voir `tests/fixtures/opra/LICENCE-OPRA.txt`.
    const EXTRAIT: &str = include_str!("../../tests/fixtures/opra/database_v1_extrait.jsonl");

    fn courbe(id: &str) -> CourbeOpra {
        analyser_base(EXTRAIT)
            .into_iter()
            .find(|c| c.id == id)
            .unwrap_or_else(|| panic!("{id} absent de l'extrait"))
    }

    #[test]
    fn la_base_relie_chaque_courbe_a_son_casque_et_a_son_fabricant() {
        let courbes = analyser_base(EXTRAIT);
        assert_eq!(courbes.len(), 5, "{courbes:?}");
        let c = courbe("sennheiser:hd650::oratory1990_harman_target_6_band_eq");
        assert_eq!(c.fabricant, "Sennheiser");
        assert_eq!(c.modele, "HD650");
        assert_eq!(c.nom_affiche(), "Sennheiser HD650");
        assert_eq!(c.forme.as_deref(), Some("over_the_ear"));
        assert_eq!(c.auteur, "oratory1990");
        assert_eq!(c.details.as_deref(), Some("Harman Target • 6 Band EQ"));
        assert!(
            c.lien
                .as_deref()
                .unwrap()
                .starts_with("https://www.dropbox.com/")
        );
        // La courbe FiiO précède son produit dans le fichier : relue quand même.
        assert_eq!(courbe("fiio:ft1::oratory1990_harman_target").modele, "FT1");
    }

    #[test]
    fn une_ligne_cassee_ou_une_courbe_sans_produit_est_ecartee() {
        let texte = format!(
            "{EXTRAIT}\nligne cassée\n\
             {{\"type\":\"eq\",\"id\":\"inconnu:x::y\",\"data\":{{\"author\":\"a\",\"type\":\"parametric_eq\",\"parameters\":{{\"gain_db\":0,\"bands\":[]}},\"product_id\":\"inconnu::x\"}}}}\n\
             {{\"type\":\"eq\",\"id\":\"sennheiser:hd650::graphique\",\"data\":{{\"author\":\"a\",\"type\":\"graphic_eq\",\"parameters\":{{\"gain_db\":0,\"bands\":[]}},\"product_id\":\"sennheiser::hd650\"}}}}\n"
        );
        assert_eq!(analyser_base(&texte).len(), 5);
    }

    /// Le HD650 d'oratory1990 (Harman), bande par bande.
    #[test]
    fn la_courbe_hd650_devient_les_bandes_equivalentes() {
        let c = convertir(&courbe("sennheiser:hd650::oratory1990_harman_target")).unwrap();
        let p = &c.profil;
        assert_eq!(p.preamp_db, -9.3);
        assert_eq!(p.bandes.len(), 10);
        let b = &p.bandes[0];
        assert_eq!(
            (b.band_type.as_str(), b.freq, b.gain, b.q),
            ("peak", 20.0, 4.0, 1.1)
        );
        let b = &p.bandes[2];
        assert_eq!(
            (b.band_type.as_str(), b.freq, b.gain, b.q),
            ("low_shelf", 105.0, 5.5, 0.71)
        );
        let b = &p.bandes[9];
        assert_eq!(
            (b.band_type.as_str(), b.freq, b.gain),
            ("high_shelf", 11000.0, -2.5)
        );
        assert!(p.bandes.iter().all(|b| b.channel.is_none()));
        assert!(c.plateaux_approches.is_empty(), "Q 0,71 = pente standard");
        assert!(p.ignores.is_empty());
    }

    /// Le T-X0 de Fostex finit par un passe-bas 12 dB/octave (réel).
    #[test]
    fn un_passe_bas_12_db_par_octave_devient_un_butterworth() {
        let c = convertir(&courbe("fostex:t_x0::oratory1990_harman_target")).unwrap();
        let b = c.profil.bandes.last().unwrap();
        assert_eq!(b.band_type, "low_pass");
        assert_eq!(b.freq, 15000.0);
        assert_eq!(b.gain, 0.0);
        assert!((b.q - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-12);
    }

    /// Le FT1 de FiiO a deux plateaux à Q 0,48 et 0,4 (réel) : importé, mais
    /// les deux plateaux sont NOMMÉS.
    #[test]
    fn un_plateau_au_q_non_standard_est_nomme() {
        let c = convertir(&courbe("fiio:ft1::oratory1990_harman_target")).unwrap();
        assert_eq!(c.profil.bandes.len(), 10);
        assert_eq!(
            c.plateaux_approches,
            vec![
                PlateauApproche {
                    bande: 1,
                    type_filtre: "low_shelf".into(),
                    q_demande: 0.48
                },
                PlateauApproche {
                    bande: 8,
                    type_filtre: "high_shelf".into(),
                    q_demande: 0.4
                },
            ]
        );
    }

    fn avec_bande(bande: BandeOpra) -> CourbeOpra {
        let mut c = courbe("sennheiser:hd650::oratory1990_harman_target");
        c.parametres.bands.push(bande);
        c
    }

    #[test]
    fn un_filtre_non_pris_en_charge_refuse_la_courbe_entiere() {
        for (type_filtre, slope) in [
            ("band_pass", None),
            ("low_pass", Some(24.0)),
            ("high_pass", Some(6.0)),
            ("all_pass", None),
        ] {
            let c = avec_bande(BandeOpra {
                type_filtre: type_filtre.into(),
                frequency: 1000.0,
                gain_db: 0.0,
                q: Some(1.0),
                slope,
            });
            let e = convertir(&c).unwrap_err();
            assert!(
                matches!(&e, ErreurConversionOpra::FiltreNonPrisEnCharge { bande: 11, type_filtre: t, .. } if t == type_filtre),
                "{type_filtre} : {e:?}"
            );
            assert_eq!(e.cle(), "opra_filter_unsupported");
        }
    }

    #[test]
    fn un_rejet_de_bande_devient_un_notch() {
        let c = avec_bande(BandeOpra {
            type_filtre: "band_stop".into(),
            frequency: 6000.0,
            gain_db: 0.0,
            q: Some(4.0),
            slope: None,
        });
        let b = convertir(&c).unwrap().profil.bandes.pop().unwrap();
        assert_eq!((b.band_type.as_str(), b.freq, b.q), ("notch", 6000.0, 4.0));
    }

    #[test]
    fn une_valeur_hors_domaine_est_refusee_pas_rognee() {
        let mut c = avec_bande(BandeOpra {
            type_filtre: "peak_dip".into(),
            frequency: 1000.0,
            gain_db: 40.0,
            q: Some(1.0),
            slope: None,
        });
        assert!(matches!(
            convertir(&c),
            Err(ErreurConversionOpra::ValeurHorsDomaine {
                bande: 11,
                champ: "gain_db",
                ..
            })
        ));
        c.parametres.bands.pop();
        c.parametres.bands.push(BandeOpra {
            type_filtre: "peak_dip".into(),
            frequency: 1000.0,
            gain_db: 1.0,
            q: None,
            slope: None,
        });
        assert!(matches!(
            convertir(&c),
            Err(ErreurConversionOpra::ChampManquant {
                bande: 11,
                champ: "q"
            })
        ));
        c.parametres.bands.clear();
        assert_eq!(
            convertir(&c).unwrap_err(),
            ErreurConversionOpra::AucuneBande
        );
    }

    /// Un faux réseau : il rend l'extrait, compte ses appels, peut être coupé.
    #[derive(Default)]
    struct FauxReseau {
        coupe: AtomicBool,
        appels: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl TelechargeurAutoEq for FauxReseau {
        async fn telecharger(&self, url: &str, _taille_max: usize) -> Result<String, String> {
            self.appels.lock().unwrap().push(url.to_string());
            if self.coupe.load(Ordering::SeqCst) {
                return Err("réseau injoignable".into());
            }
            Ok(EXTRAIT.into())
        }
    }

    fn catalogue(reseau: &Arc<FauxReseau>, dossier: Option<PathBuf>) -> CatalogueOpra {
        CatalogueOpra::new(
            "https://exemple.test/database_v1.jsonl",
            reseau.clone(),
            dossier,
        )
    }

    #[tokio::test]
    async fn la_base_est_telechargee_une_fois_et_l_import_ne_telecharge_rien_d_autre() {
        let reseau = Arc::new(FauxReseau::default());
        let c = catalogue(&reseau, None);
        assert_eq!(c.courbes().await.unwrap().len(), 5);
        let courbe = c
            .courbe("fostex:t_x0::oratory1990_harman_target")
            .await
            .unwrap();
        assert_eq!(courbe.modele, "T-X0");
        assert_eq!(
            *reseau.appels.lock().unwrap(),
            ["https://exemple.test/database_v1.jsonl"]
        );
    }

    #[tokio::test]
    async fn sans_reseau_la_copie_locale_sert() {
        let dossier = tempfile::tempdir().unwrap();
        let reseau = Arc::new(FauxReseau::default());
        catalogue(&reseau, Some(dossier.path().to_path_buf()))
            .courbes()
            .await
            .unwrap();
        // Le serveur redémarre sans réseau : la copie fraîche suffit.
        let hors_ligne = Arc::new(FauxReseau::default());
        hors_ligne.coupe.store(true, Ordering::SeqCst);
        let c = catalogue(&hors_ligne, Some(dossier.path().to_path_buf()));
        assert_eq!(c.courbes().await.unwrap().len(), 5);
        assert!(hors_ligne.appels.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn une_copie_perimee_est_servie_quand_le_reseau_manque() {
        let dossier = tempfile::tempdir().unwrap();
        let reseau = Arc::new(FauxReseau::default());
        catalogue(&reseau, Some(dossier.path().to_path_buf()))
            .courbes()
            .await
            .unwrap();
        reseau.coupe.store(true, Ordering::SeqCst);
        let c =
            catalogue(&reseau, Some(dossier.path().to_path_buf())).avec_fraicheur(Duration::ZERO);
        assert_eq!(c.courbes().await.unwrap().len(), 5);
        assert_eq!(
            reseau.appels.lock().unwrap().len(),
            2,
            "le réseau a été tenté"
        );
    }

    #[tokio::test]
    async fn sans_reseau_ni_copie_l_erreur_le_dit() {
        let reseau = Arc::new(FauxReseau::default());
        reseau.coupe.store(true, Ordering::SeqCst);
        assert!(matches!(
            catalogue(&reseau, None).courbes().await,
            Err(ErreurBaseOpra::HorsLigne(_))
        ));
    }

    #[tokio::test]
    async fn un_identifiant_hors_de_la_base_est_inconnu() {
        let reseau = Arc::new(FauxReseau::default());
        let c = catalogue(&reseau, None);
        for id in ["../../etc/passwd", "sennheiser:hd650::inexistante", ""] {
            assert!(matches!(
                c.courbe(id).await,
                Err(ErreurBaseOpra::CourbeInconnue(_))
            ));
        }
        assert_eq!(reseau.appels.lock().unwrap().len(), 1);
    }
}
