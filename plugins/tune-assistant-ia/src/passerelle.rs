//! Étape 2 — ce que l'HÔTE emprunte à l'assistant : son coffre, son plafond,
//! son fournisseur.
//!
//! Deux usages anciens vivent hors du greffon et y restent :
//!
//! - l'ancien assistant conversationnel `POST /ai/query`
//!   (`tune-server/src/routes/ai.rs`), qui pilote la lecture par des OUTILS
//!   (tool use) — un dialogue propre à l'API Messages d'Anthropic ;
//! - la traduction des requêtes d'ambiance (`tune_core::ai::translate`),
//!   appelée par la recherche acoustique.
//!
//! Ils lisaient une clé EN CLAIR (`anthropic_api_key`…) et ne connaissaient
//! aucun plafond. Ils passent désormais par une [`Passerelle`] : la clé vient
//! du coffre (après [`crate::migration`]), chaque appel est compté au tarif de
//! la table et refusé plafond atteint, sous LE verrou de consommation du
//! processus ([`crate::appel::verrou_de_consommation`]) — celui que tient
//! aussi le greffon.
//!
//! La passerelle n'a pas de cache : la traduction a le sien
//! (`ambiance_query_translations`), et une conversation ne se rejoue pas.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use tracing::{info, warn};
use tune_core::db::backend::DbBackend;

use crate::appel::{self, ErreurAppel, Moyens};
use crate::coffre::Coffre;
use crate::fournisseur::{self, Fabrique, Famille};
use crate::migration;
use crate::reglages::{self, Reglages};

/// L'usage compté pour l'ancien assistant conversationnel.
pub const USAGE_REQUETE: &str = "requete";
/// L'usage compté pour la traduction des ambiances.
pub const USAGE_TRADUCTION: &str = "traduction";

#[derive(Clone)]
pub struct Passerelle {
    backend: Arc<dyn DbBackend>,
    coffre: Arc<Coffre>,
    fabrique: Fabrique,
    verrou: Arc<tokio::sync::Mutex<()>>,
}

impl Passerelle {
    /// La passerelle de production : la clé maîtresse dans `dossier_secret`
    /// (le dossier de la base, comme pour le greffon), les vrais fournisseurs.
    pub fn new(backend: Arc<dyn DbBackend>, dossier_secret: &Path) -> Self {
        Self::avec_fabrique(backend, dossier_secret, fournisseur::fabrique_reelle())
    }

    /// Pour les essais : un faux fournisseur.
    pub fn avec_fabrique(
        backend: Arc<dyn DbBackend>,
        dossier_secret: &Path,
        fabrique: Fabrique,
    ) -> Self {
        Self {
            backend,
            coffre: Arc::new(Coffre::new(dossier_secret)),
            fabrique,
            verrou: appel::verrou_de_consommation(),
        }
    }

    fn moyens(&self) -> Moyens<'_> {
        Moyens {
            backend: &self.backend,
            coffre: &self.coffre,
            fabrique: &self.fabrique,
            verrou: &self.verrou,
            cache: None,
        }
    }

    /// Migre les clés héritées vers le coffre. Un échec est journalisé (sans
    /// la clé) et laisse la clé en clair à sa place : il sera retenté.
    pub fn migrer(&self) -> migration::Rapport {
        match migration::migrer(&self.backend, &self.coffre) {
            Ok(r) => r,
            Err(e) => {
                warn!(erreur = %e, "assistant_ia_migration_echouee");
                migration::Rapport::default()
            }
        }
    }

    /// L'assistant peut-il faire partir un appel (fournisseur, modèle, clé
    /// qui s'ouvre, tarif connu) ? Le plafond n'est pas regardé ici.
    pub fn pret(&self) -> bool {
        let r = Reglages::charger(&self.backend);
        let modele = r.modele_effectif(false);
        r.famille.is_some()
            && !modele.is_empty()
            && appel::acces_et_tarif(&self.backend, &self.coffre, &r, &modele).is_ok()
    }

    /// L'écran Ambiance peut-il promettre une traduction ? Oui si l'assistant
    /// est prêt, ou si une clé héritée attend sa migration (elle aura lieu au
    /// premier appel). Lecture seule : rien n'est migré ici.
    pub fn traduction_disponible(&self) -> bool {
        self.pret() || migration::reste_une_cle_heritee(&self.backend)
    }

    /// Traduit une requête d'ambiance par le fournisseur de l'assistant.
    /// `None` (l'appelant garde la requête brute) si l'assistant n'est pas
    /// prêt, plafond atteint, ou en cas d'échec.
    pub async fn traduire(&self, consigne: &str, requete: &str) -> Option<String> {
        self.migrer();
        match appel::appeler_avec(
            &self.moyens(),
            USAGE_TRADUCTION,
            false,
            consigne,
            requete,
            100,
            None,
            false,
        )
        .await
        {
            Ok(r) => Some(r.texte),
            Err(e) => {
                info!(code = e.code(), "assistant_ia_traduction_non_faite");
                None
            }
        }
    }

    /// L'ancien assistant peut-il dialoguer ? Il lui faut Anthropic : le
    /// pilotage par outils est écrit pour l'API Messages.
    pub fn anthropic_pret(&self) -> bool {
        Reglages::charger(&self.backend).famille == Some(Famille::Anthropic) && self.pret()
    }

    /// Un tour de dialogue de l'ancien assistant, avec ses outils, au tarif
    /// et sous le plafond de l'assistant. Rend la réponse brute de l'API
    /// Messages (`content`, `stop_reason`, `usage`).
    pub async fn discuter_anthropic(
        &self,
        systeme: &str,
        messages: &Value,
        outils: &Value,
        max_jetons: u32,
    ) -> Result<Value, ErreurAppel> {
        let r = Reglages::charger(&self.backend);
        if r.famille != Some(Famille::Anthropic) {
            return Err(ErreurAppel::NonConfigure);
        }
        let modele = r.modele_effectif(false);
        let (acces, tarif) = appel::acces_et_tarif(&self.backend, &self.coffre, &r, &modele)?;

        let _verrou = self.verrou.lock().await;
        let mut c = reglages::charger_consommation(&self.backend);
        if c.micro_eur >= r.plafond_micro_eur {
            info!(usage = USAGE_REQUETE, "assistant_ia_plafond_atteint");
            return Err(ErreurAppel::PlafondAtteint);
        }
        let mut corps = json!({
            "model": modele,
            "max_tokens": max_jetons,
            "system": systeme,
            "messages": messages,
        });
        if outils.as_array().is_some_and(|o| !o.is_empty()) {
            corps["tools"] = outils.clone();
        }
        let rep = fournisseur::client()
            .post(format!("{}/v1/messages", fournisseur::base(&acces)))
            .header("x-api-key", acces.cle.as_deref().unwrap_or(""))
            .header("anthropic-version", "2023-06-01")
            .timeout(fournisseur::DELAI)
            .json(&corps)
            .send()
            .await
            .map_err(|e| {
                ErreurAppel::Fournisseur(fournisseur::erreur_reseau(Famille::Anthropic, &e))
            })?;
        let statut = rep.status().as_u16();
        let texte = rep.text().await.map_err(|e| {
            ErreurAppel::Fournisseur(fournisseur::erreur_reseau(Famille::Anthropic, &e))
        })?;
        if !(200..300).contains(&statut) {
            return Err(ErreurAppel::Fournisseur(fournisseur::erreur_de_statut(
                Famille::Anthropic,
                statut,
                &texte,
            )));
        }
        let v: Value = serde_json::from_str(&texte)
            .map_err(|_| ErreurAppel::Fournisseur(fournisseur::ErreurFournisseur::Autre(statut)))?;
        let usage = v.get("usage").cloned().unwrap_or(Value::Null);
        appel::compter(
            &self.backend,
            &mut c,
            &r,
            tarif,
            USAGE_REQUETE,
            &modele,
            fournisseur::jetons(&usage, "input_tokens")
                + fournisseur::jetons(&usage, "cache_creation_input_tokens")
                + fournisseur::jetons(&usage, "cache_read_input_tokens"),
            fournisseur::jetons(&usage, "output_tokens"),
        );
        Ok(v)
    }
}
