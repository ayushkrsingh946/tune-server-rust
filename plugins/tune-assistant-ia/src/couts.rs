//! Ce que coûte un appel, et le plafond mensuel.
//!
//! # La table des prix
//!
//! Elle ne porte que des tarifs DOCUMENTÉS. Les prix sont ceux de l'API
//! Anthropic en accès direct (« first-party »), en dollars par million de
//! jetons, **relevés le 12/10/2026** dans la référence de l'API Claude (table
//! des modèles mise en cache le 06/10/2026). Pour Claude Haiku 5.5, c'est le
//! tarif des requêtes de moins de 100 000 jetons d'entrée — toutes les
//! nôtres : au-delà, Anthropic facture 0,50 $ / 2,50 $.
//!
//! Les prix Gemini viennent de la page officielle des tarifs de Google (voir
//! [`TARIFS_GEMINI`], source et date du relevé).
//!
//! Aucun prix n'est inventé pour les autres fournisseurs : pour un point
//! d'accès compatible OpenAI, l'utilisateur saisit lui-même les deux prix de
//! son modèle (`prix_entree_usd_mtok`, `prix_sortie_usd_mtok`). Sans eux,
//! l'appel est REFUSÉ (`tarif_inconnu`) plutôt que compté pour zéro : un
//! plafond qui ne compte rien ne plafonne rien. Ollama tourne chez
//! l'utilisateur : son coût est nul.
//!
//! # Dollars et euros
//!
//! Les fournisseurs facturent en dollars, le plafond se règle en euros. Le
//! taux de conversion est un réglage (`taux_usd_eur`) ; par défaut il vaut 1,
//! ce qui SURESTIME la dépense tant que le dollar vaut moins d'un euro : le
//! plafond est atteint un peu plus tôt, jamais plus tard. C'est un choix de
//! prudence, pas un taux relevé.

use serde::{Deserialize, Serialize};

/// Le modèle Anthropic par défaut.
pub const MODELE_ANTHROPIC_PAR_DEFAUT: &str = "claude-haiku-5-5";
/// Le modèle Anthropic proposé pour le classique difficile.
pub const MODELE_ANTHROPIC_CLASSIQUE: &str = "claude-sonnet-5-5";

/// Le modèle Gemini par défaut : celui que Google recommande pour un projet
/// nouveau, « fastest, most cost-effective » (page des modèles,
/// <https://ai.google.dev/gemini-api/docs/models>, relevée le 12/10/2026 :
/// statut Stable).
pub const MODELE_GEMINI_PAR_DEFAUT: &str = "gemini-3.5-flash-lite";

/// Un tarif documenté.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tarif {
    pub entree_usd_mtok: f64,
    pub sortie_usd_mtok: f64,
}

/// Les tarifs connus, par identifiant de modèle Anthropic (relevé du
/// 12/10/2026, voir l'en-tête du module).
pub const TARIFS_ANTHROPIC: &[(&str, Tarif)] = &[
    (
        "claude-haiku-5-5",
        Tarif {
            entree_usd_mtok: 0.10,
            sortie_usd_mtok: 0.50,
        },
    ),
    (
        "claude-sonnet-5-5",
        Tarif {
            entree_usd_mtok: 2.00,
            sortie_usd_mtok: 10.00,
        },
    ),
];

/// Les tarifs Gemini (palier payant, texte, ≤ 200 000 jetons d'entrée), en
/// dollars par million de jetons. Source : <https://ai.google.dev/gemini-api/docs/pricing>,
/// page mise à jour le 09/10/2026, **relevée le 12/10/2026**. La page annonce
/// des prix « à peu près doublés » à partir du 01/01/2027 : la table sera à
/// relever avant cette date.
pub const TARIFS_GEMINI: &[(&str, Tarif)] = &[
    (
        "gemini-3.5-flash-lite",
        Tarif {
            entree_usd_mtok: 0.30,
            sortie_usd_mtok: 2.50,
        },
    ),
    (
        "gemini-3.1-flash-lite",
        Tarif {
            entree_usd_mtok: 0.25,
            sortie_usd_mtok: 1.50,
        },
    ),
];

pub fn tarif_gemini(modele: &str) -> Option<Tarif> {
    TARIFS_GEMINI
        .iter()
        .find(|(m, _)| *m == modele.trim())
        .map(|(_, t)| *t)
}

/// Le tarif d'un modèle Anthropic, s'il est dans la table.
pub fn tarif_anthropic(modele: &str) -> Option<Tarif> {
    TARIFS_ANTHROPIC
        .iter()
        .find(|(m, _)| *m == modele.trim())
        .map(|(_, t)| *t)
}

/// Le coût d'un appel, en MICRO-euros, arrondi au micro-euro supérieur.
pub fn cout_micro_eur(tarif: Tarif, entree: u64, sortie: u64, taux_usd_eur: f64) -> u64 {
    let usd = (entree as f64) * tarif.entree_usd_mtok / 1_000_000.0
        + (sortie as f64) * tarif.sortie_usd_mtok / 1_000_000.0;
    let micro = usd * taux_usd_eur * 1_000_000.0;
    if micro <= 0.0 { 0 } else { micro.ceil() as u64 }
}

/// La consommation du mois en cours.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consommation {
    /// `AAAA-MM` : le mois que ces compteurs décrivent.
    pub mois: String,
    pub micro_eur: u64,
    pub appels: u64,
    pub jetons_entree: u64,
    pub jetons_sortie: u64,
    /// Réponses servies par le cache, sans appel ni coût.
    pub depuis_cache: u64,
}

impl Consommation {
    /// Les compteurs, remis à zéro si `mois` n'est plus le leur.
    pub fn pour_le_mois(self, mois: &str) -> Self {
        if self.mois == mois {
            self
        } else {
            Self {
                mois: mois.to_string(),
                ..Self::default()
            }
        }
    }
}

/// Le mois courant, à l'heure locale du serveur.
pub fn mois_courant() -> String {
    chrono::Local::now().format("%Y-%m").to_string()
}

/// Des euros saisis (« 2,50 » ou « 2.5 ») en micro-euros. `None` si illisible
/// ou négatif.
pub fn euros_en_micro(texte: &str) -> Option<u64> {
    let v: f64 = texte.trim().replace(',', ".").parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    Some((v * 1_000_000.0).round() as u64)
}

pub fn micro_en_euros(micro: u64) -> f64 {
    micro as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_modeles_par_defaut_ont_un_tarif() {
        assert!(tarif_anthropic(MODELE_ANTHROPIC_PAR_DEFAUT).is_some());
        assert!(tarif_anthropic(MODELE_ANTHROPIC_CLASSIQUE).is_some());
        assert!(tarif_anthropic("claude-inconnu").is_none());
        assert!(tarif_gemini(MODELE_GEMINI_PAR_DEFAUT).is_some());
    }

    #[test]
    fn un_million_de_jetons_haiku_coute_ce_que_dit_la_table() {
        let t = tarif_anthropic("claude-haiku-5-5").unwrap();
        // 1 M en entrée (0,10 $) + 1 M en sortie (0,50 $) = 0,60 € au taux 1.
        assert_eq!(cout_micro_eur(t, 1_000_000, 1_000_000, 1.0), 600_000);
        // Le moindre jeton coûte au moins un micro-euro : rien n'est gratuit
        // par arrondi.
        assert_eq!(cout_micro_eur(t, 1, 0, 1.0), 1);
    }

    #[test]
    fn la_consommation_repart_de_zero_chaque_mois() {
        let c = Consommation {
            mois: "2026-09".into(),
            micro_eur: 42,
            ..Default::default()
        };
        assert_eq!(c.clone().pour_le_mois("2026-09").micro_eur, 42);
        assert_eq!(c.pour_le_mois("2026-10").micro_eur, 0);
    }

    #[test]
    fn les_euros_se_lisent_avec_virgule_ou_point() {
        assert_eq!(euros_en_micro("2,50"), Some(2_500_000));
        assert_eq!(euros_en_micro("2.5"), Some(2_500_000));
        assert_eq!(euros_en_micro("-1"), None);
        assert_eq!(euros_en_micro("beaucoup"), None);
    }
}
