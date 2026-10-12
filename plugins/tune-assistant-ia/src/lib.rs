//! L'Assistant IA, étape 1 — greffon natif (chantier « assistant-ia », GO de
//! Bertrand du 12/10/2026). Conception : `docs/chantier-assistant-ia.md`.
//!
//! Deux usages, avec la clé de l'UTILISATEUR, depuis SON serveur, vers SON
//! fournisseur :
//!
//! 1. une playlist intelligente décrite par une phrase — des RÈGLES du moteur
//!    existant, validées ici, que l'écran montre et que l'utilisateur
//!    enregistre lui-même ([`regles`]) ;
//! 2. des suggestions d'enrichissement sur UN album, acceptées ligne par
//!    ligne, annulables ([`suggestions`]).
//!
//! # Les routes
//!
//! Montées par l'hôte sous `/api/v1/ext/assistant-ia` :
//!
//! | Route | Rôle |
//! |---|---|
//! | `GET /reglages` | réglages, 4 derniers caractères de la clé, consommation du mois |
//! | `PUT /reglages` | change fournisseur, modèle, adresse, plafond, prix, clé |
//! | `POST /tester` | un petit appel réel : `{ok: true}` ou `{ok: false, error, message}` |
//! | `POST /playlist` | `{phrase, cible?}` → règles proposées |
//! | `POST /albums/{id}/suggestions` | suggestions, écart avant/après |
//! | `POST /albums/{id}/appliquer` | `{lignes: [{champ, valeur}]}` |
//! | `POST /albums/{id}/annuler` | `{operation_id?}` |
//! | `GET /albums/{id}/historique` | les opérations de l'album |
//!
//! Gratuit, pour tout le monde : aucune garde Premium.

pub mod appel;
pub mod cache;
pub mod coffre;
pub mod couts;
pub mod fournisseur;
pub mod reglages;
pub mod regles;
mod routes;
pub mod suggestions;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tune_core::db::backend::DbBackend;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

pub use regles::CritereMoteur;
pub use routes::router_vers;

/// Ce que l'hôte fournit.
#[derive(Clone)]
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
    /// Le dossier de la BASE : la clé maîtresse du coffre y vit, hors de la
    /// base elle-même (voir [`coffre`]).
    pub dossier_secret: PathBuf,
    /// Les critères du moteur des règles intelligentes
    /// (`tune_smart_http::criteres::criteres_exposes`).
    pub criteres: Vec<CritereMoteur>,
}

/// L'état partagé des routes.
#[derive(Clone)]
pub struct Etat {
    pub backend: Arc<dyn DbBackend>,
    pub coffre: Arc<coffre::Coffre>,
    pub cache: Arc<cache::Cache>,
    pub criteres: Arc<Vec<CritereMoteur>>,
    pub fabrique: fournisseur::Fabrique,
    /// Tient la lecture du plafond, l'appel et son comptage ensemble.
    pub verrou_conso: Arc<tokio::sync::Mutex<()>>,
    /// Sérialise les écritures sur les albums (appliquer, annuler).
    pub verrou_albums: Arc<tokio::sync::Mutex<()>>,
}

impl Etat {
    pub fn new(
        backend: Arc<dyn DbBackend>,
        dossier_secret: &std::path::Path,
        dossier_cache: PathBuf,
        criteres: Vec<CritereMoteur>,
        fabrique: fournisseur::Fabrique,
    ) -> Self {
        Self {
            backend,
            coffre: Arc::new(coffre::Coffre::new(dossier_secret)),
            cache: Arc::new(cache::Cache::new(dossier_cache)),
            criteres: Arc::new(criteres),
            fabrique,
            verrou_conso: Arc::new(tokio::sync::Mutex::new(())),
            verrou_albums: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

pub struct AssistantIaPlugin {
    services: HostServices,
}

impl AssistantIaPlugin {
    pub fn new(services: HostServices) -> Self {
        Self { services }
    }
}

#[async_trait]
impl TunePlugin for AssistantIaPlugin {
    fn name(&self) -> &str {
        "assistant-ia"
    }

    fn display_name(&self) -> &str {
        "Assistant IA"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn description(&self) -> &str {
        "Playlists intelligentes décrites en une phrase et suggestions d'enrichissement des albums, avec votre propre clé (Anthropic, compatible OpenAI, Ollama)"
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        let services = self.services.clone();
        let etat = Etat::new(
            services.backend,
            &services.dossier_secret,
            ctx.data_dir.join("cache"),
            services.criteres,
            fournisseur::fabrique_reelle(),
        );
        ctx.register_router(router_vers(etat));
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }
}
