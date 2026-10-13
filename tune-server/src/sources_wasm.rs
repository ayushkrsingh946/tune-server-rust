//! #6197 — les greffons WASM qui FOURNISSENT une source de streaming.
//!
//! Trois pièces, décrites dans `docs/chantier-sources-wasm.md` :
//!
//! - [`ReseauDesGreffons`] : la requête HTTP qu'un greffon demande par
//!   `host_http`, faite par l'hôte (le runtime a déjà jugé la permission et la
//!   liste de domaines), avec une [`BoiteACookies`] cloisonnée par greffon et
//!   par hôte distant. Les `Set-Cookie` sont absorbés et jamais montrés au
//!   greffon ; les redirections ne sont pas suivies (la suivante repasse par
//!   la liste de domaines).
//! - [`WasmStreamingService`] : l'adaptateur qui implémente
//!   [`StreamingService`] en appelant les fonctions EXPORTÉES du greffon
//!   (`login`, `search`, `get_album`, `get_track_url`…). Chaque appel se joue
//!   dans `spawn_blocking` : le greffon est synchrone, et `host_http` y fait
//!   `Handle::block_on` sur la requête asynchrone — le pont de #4716.
//! - [`inscrire_la_source`] : l'inscription dans le registre des services au
//!   chargement des greffons, et le retour de la session au démarrage depuis
//!   les identifiants SCELLÉS ([`CoffreDIdentifiants`]).
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::TuneError;
use tune_core::coffre_greffons::CoffreDIdentifiants;
use tune_core::plugins::{ChampDeConnexion, PluginManifest};
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use tune_plugin_runtime_wasm::WasmPlugin;

/// La permission qui autorise un greffon à s'inscrire comme service.
pub const PERMISSION_SOURCE: &str = "streaming_source";

/// Plafond du corps d'une réponse rendue à un greffon.
pub const TAILLE_MAX_REPONSE: usize = 4 * 1024 * 1024;

/// Délai d'une requête d'un greffon, connexion comprise.
pub const DELAI_HTTP: Duration = Duration::from_secs(20);

/// Les méthodes qu'un greffon peut employer.
const METHODES: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];

/// En-têtes qu'un greffon ne pose pas lui-même : l'hôte les calcule.
const EN_TETES_INTERDITS: &[&str] = &["host", "content-length", "transfer-encoding", "connection"];

// ───────────────────────────────────────────────────────────────────────────
// La boîte à cookies
// ───────────────────────────────────────────────────────────────────────────

/// Les cookies des greffons, cloisonnés par greffon PUIS par hôte distant.
///
/// Volontairement simple : un cookie vaut pour l'hôte exact qui l'a posé
/// (pas de `Domain=` élargi, pas de `Path=`), et vit en mémoire. Un
/// `Max-Age=0` ou une valeur vide l'efface.
#[derive(Debug, Default)]
pub struct BoiteACookies {
    par_greffon: HashMap<String, HashMap<String, BTreeMap<String, String>>>,
}

impl BoiteACookies {
    /// Ranger les `Set-Cookie` qu'a rendus `hote` pour `greffon`.
    pub fn absorber<'a>(
        &mut self,
        greffon: &str,
        hote: &str,
        set_cookies: impl IntoIterator<Item = &'a str>,
    ) {
        let jarre = self
            .par_greffon
            .entry(greffon.to_string())
            .or_default()
            .entry(hote.to_ascii_lowercase())
            .or_default();
        for ligne in set_cookies {
            let mut morceaux = ligne.split(';');
            let Some((nom, valeur)) = morceaux.next().and_then(|c| c.split_once('=')) else {
                continue;
            };
            let (nom, valeur) = (nom.trim(), valeur.trim());
            if nom.is_empty() {
                continue;
            }
            let efface =
                valeur.is_empty() || morceaux.any(|a| a.trim().eq_ignore_ascii_case("max-age=0"));
            if efface {
                jarre.remove(nom);
            } else {
                jarre.insert(nom.to_string(), valeur.to_string());
            }
        }
    }

    /// L'en-tête `Cookie` à envoyer à `hote` pour `greffon`.
    pub fn en_tete(&self, greffon: &str, hote: &str) -> Option<String> {
        let jarre = self
            .par_greffon
            .get(greffon)?
            .get(&hote.to_ascii_lowercase())?;
        if jarre.is_empty() {
            return None;
        }
        Some(
            jarre
                .iter()
                .map(|(n, v)| format!("{n}={v}"))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }

    /// Oublier les cookies d'un greffon (déconnexion).
    pub fn oublier(&mut self, greffon: &str) {
        self.par_greffon.remove(greffon);
    }
}

// ───────────────────────────────────────────────────────────────────────────
// La requête HTTP d'un greffon
// ───────────────────────────────────────────────────────────────────────────

/// Le client HTTP des greffons et leur boîte à cookies.
pub struct ReseauDesGreffons {
    client: reqwest::Client,
    cookies: std::sync::Mutex<BoiteACookies>,
}

impl Default for ReseauDesGreffons {
    fn default() -> Self {
        Self::new()
    }
}

impl ReseauDesGreffons {
    pub fn new() -> Self {
        let client = tune_core::http::client::builder()
            // Une redirection suivie ici sauterait la liste de domaines.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(DELAI_HTTP)
            .user_agent(concat!("Tune/", env!("CARGO_PKG_VERSION"), " (greffon)"))
            .build()
            .unwrap_or_default();
        Self {
            client,
            cookies: std::sync::Mutex::new(BoiteACookies::default()),
        }
    }

    /// Oublier les cookies d'un greffon.
    pub fn oublier(&self, greffon: &str) {
        if let Ok(mut b) = self.cookies.lock() {
            b.oublier(greffon);
        }
    }

    /// Faire la requête `requete` (`{method, url, headers?, body?}`) pour
    /// `greffon`. Rend `{status, headers, body}` ; le corps est du texte
    /// (UTF-8 avec remplacement), borné à [`TAILLE_MAX_REPONSE`].
    pub async fn executer(&self, greffon: &str, requete: Value) -> Result<Value, String> {
        let methode = requete
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_ascii_uppercase();
        if !METHODES.contains(&methode.as_str()) {
            return Err(format!("http: méthode refusée : {methode}"));
        }
        let url = requete
            .get("url")
            .and_then(Value::as_str)
            .ok_or("http: url manquante")?;
        let analysee = reqwest::Url::parse(url).map_err(|e| format!("http: {e}"))?;
        let hote = analysee
            .host_str()
            .ok_or("http: URL sans hôte")?
            .to_ascii_lowercase();
        let methode =
            reqwest::Method::from_bytes(methode.as_bytes()).map_err(|e| format!("http: {e}"))?;

        let mut construite = self.client.request(methode, analysee);
        let mut cookie_du_greffon: Option<String> = None;
        if let Some(entetes) = requete.get("headers").and_then(Value::as_object) {
            for (nom, valeur) in entetes {
                let Some(valeur) = valeur.as_str() else {
                    continue;
                };
                let nom = nom.to_ascii_lowercase();
                if EN_TETES_INTERDITS.contains(&nom.as_str()) {
                    continue;
                }
                if nom == "cookie" {
                    cookie_du_greffon = Some(valeur.to_string());
                    continue;
                }
                construite = construite.header(nom, valeur);
            }
        }
        let de_la_boite = self
            .cookies
            .lock()
            .ok()
            .and_then(|b| b.en_tete(greffon, &hote));
        let cookie = match (de_la_boite, cookie_du_greffon) {
            (Some(a), Some(b)) => Some(format!("{a}; {b}")),
            (a, b) => a.or(b),
        };
        if let Some(c) = cookie {
            construite = construite.header(reqwest::header::COOKIE, c);
        }
        if let Some(corps) = requete.get("body").and_then(Value::as_str) {
            construite = construite.body(corps.to_string());
        }

        let mut reponse = construite.send().await.map_err(|e| format!("http: {e}"))?;
        let statut = reponse.status().as_u16();
        let set_cookies: Vec<String> = reponse
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_string))
            .collect();
        if let Ok(mut b) = self.cookies.lock() {
            b.absorber(greffon, &hote, set_cookies.iter().map(String::as_str));
        }
        let mut entetes = serde_json::Map::new();
        for (nom, valeur) in reponse.headers() {
            if *nom == reqwest::header::SET_COOKIE {
                continue;
            }
            if let Ok(v) = valeur.to_str() {
                entetes.insert(nom.as_str().to_string(), json!(v));
            }
        }
        let mut corps: Vec<u8> = Vec::new();
        while let Some(morceau) = reponse.chunk().await.map_err(|e| format!("http: {e}"))? {
            if corps.len() + morceau.len() > TAILLE_MAX_REPONSE {
                return Err(format!(
                    "http: réponse de plus de {} Mio",
                    TAILLE_MAX_REPONSE / (1024 * 1024)
                ));
            }
            corps.extend_from_slice(&morceau);
        }
        Ok(json!({
            "status": statut,
            "headers": entetes,
            "body": String::from_utf8_lossy(&corps),
        }))
    }
}

// ───────────────────────────────────────────────────────────────────────────
// L'adaptateur
// ───────────────────────────────────────────────────────────────────────────

/// Ce que l'adaptateur sait de la session.
#[derive(Default)]
struct Session {
    statut: AuthStatus,
    /// Les identifiants SCELLÉS, ce que `save_tokens` publie.
    scelles: Option<Value>,
    /// Les identifiants en clair, gardés en mémoire seulement le temps de
    /// rejouer `login` après un redémarrage (`post_restore`).
    a_rejouer: Option<Value>,
}

/// Un greffon WASM vu comme un service de streaming.
pub struct WasmStreamingService {
    nom: String,
    id_greffon: String,
    champs: Vec<ChampDeConnexion>,
    greffon: Arc<tokio::sync::Mutex<WasmPlugin>>,
    coffre: Arc<CoffreDIdentifiants>,
    reseau: Option<Arc<ReseauDesGreffons>>,
    actif: bool,
    session: std::sync::Mutex<Session>,
}

impl WasmStreamingService {
    /// L'adaptateur d'un greffon chargé. `None` si son manifeste ne déclare
    /// pas de source, ou un nom de service invalide.
    pub fn new(
        manifeste: &PluginManifest,
        greffon: Arc<tokio::sync::Mutex<WasmPlugin>>,
        coffre: Arc<CoffreDIdentifiants>,
        reseau: Option<Arc<ReseauDesGreffons>>,
    ) -> Option<Self> {
        let source = manifeste.streaming_source.as_ref()?;
        if !source.nom_valide() {
            return None;
        }
        Some(Self {
            nom: source.name.clone(),
            id_greffon: manifeste.id.clone(),
            champs: source.login_fields.clone(),
            greffon,
            coffre,
            reseau,
            actif: true,
            session: std::sync::Mutex::new(Session::default()),
        })
    }

    /// Le pont : jouer la fonction exportée `export` sur un fil bloquant, et
    /// rendre son JSON — ou l'erreur que le greffon a rendue (`{"error"}`).
    async fn appeler(&self, export: &str, entree: Value) -> Result<Value, TuneError> {
        let greffon = self.greffon.clone();
        let export_nom = export.to_string();
        let entree = entree.to_string();
        let sortie = tokio::task::spawn_blocking(move || {
            let mut g = greffon.blocking_lock();
            g.call_export(&export_nom, &entree)
        })
        .await
        .map_err(|e| TuneError::Streaming(format!("greffon {}: {e}", self.id_greffon)))?;
        let sortie = match sortie {
            Ok(s) => s,
            Err(e) if e.starts_with("export_missing") => {
                return Err(TuneError::Unsupported(format!(
                    "le greffon {} ne fournit pas `{export}`",
                    self.id_greffon
                )));
            }
            Err(e) => return Err(TuneError::Streaming(e)),
        };
        let v: Value = serde_json::from_str(&sortie)?;
        if let Some(e) = v.get("error") {
            let texte = e.as_str().map_or_else(|| e.to_string(), str::to_string);
            return Err(TuneError::Streaming(texte));
        }
        Ok(v)
    }

    /// Les identifiants reçus, validés contre les champs du manifeste : un
    /// objet, chaque champ requis présent et non vide, rien d'autre que des
    /// champs déclarés (le greffon ne reçoit pas ce qu'il n'a pas demandé).
    fn valider(&self, credentials: &Value) -> Result<Value, TuneError> {
        let objet = credentials
            .as_object()
            .ok_or_else(|| TuneError::Streaming("identifiants : un objet est attendu".into()))?;
        let mut garde = serde_json::Map::new();
        for champ in &self.champs {
            let valeur = objet.get(&champ.key).and_then(Value::as_str).unwrap_or("");
            if champ.required && valeur.trim().is_empty() {
                return Err(TuneError::Streaming(format!(
                    "identifiants : « {} » est requis",
                    if champ.label.is_empty() {
                        &champ.key
                    } else {
                        &champ.label
                    }
                )));
            }
            if !valeur.is_empty() {
                garde.insert(champ.key.clone(), json!(valeur));
            }
        }
        Ok(Value::Object(garde))
    }

    async fn connecter(&self, identifiants: &Value) -> Result<AuthStatus, TuneError> {
        let r = self.appeler("login", identifiants.clone()).await?;
        let statut = AuthStatus {
            authenticated: r.get("authenticated").and_then(Value::as_bool) == Some(true),
            username: r
                .get("username")
                .and_then(Value::as_str)
                .map(str::to_string),
            subscription: r
                .get("subscription")
                .and_then(Value::as_str)
                .map(str::to_string),
            ..Default::default()
        };
        Ok(statut)
    }

    fn statut(&self) -> AuthStatus {
        self.session
            .lock()
            .map(|s| s.statut.clone())
            .unwrap_or_default()
    }
}

/// Ce qu'un greffon rend, complété des champs qu'il peut omettre, puis lu
/// dans la forme du cœur.
fn lire<T: serde::de::DeserializeOwned>(
    mut v: Value,
    defauts: &[(&str, Value)],
) -> Result<T, TuneError> {
    if let Some(o) = v.as_object_mut() {
        for (cle, defaut) in defauts {
            o.entry(cle.to_string()).or_insert_with(|| defaut.clone());
        }
    }
    serde_json::from_value(v).map_err(|e| TuneError::Streaming(format!("réponse du greffon : {e}")))
}

fn defauts_piste() -> [(&'static str, Value); 2] {
    [("explicit", json!(false)), ("duration_ms", json!(0))]
}

fn lire_pistes(v: Value) -> Result<Vec<StreamTrack>, TuneError> {
    let liste = v.as_array().cloned().ok_or_else(|| {
        TuneError::Streaming("réponse du greffon : une liste est attendue".into())
    })?;
    liste
        .into_iter()
        .map(|p| lire(p, &defauts_piste()))
        .collect()
}

#[async_trait::async_trait]
impl StreamingService for WasmStreamingService {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        &self.nom
    }
    fn enabled(&self) -> bool {
        self.actif
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.actif = enabled;
    }

    async fn authenticate(&mut self, credentials: &Value) -> Result<AuthStatus, TuneError> {
        let identifiants = self.valider(credentials)?;
        let statut = self.connecter(&identifiants).await?;
        let scelles = if statut.authenticated {
            Some(
                self.coffre
                    .sceller(&self.id_greffon, &self.nom, &identifiants)
                    .map_err(TuneError::Streaming)?,
            )
        } else {
            None
        };
        if let Ok(mut s) = self.session.lock() {
            s.statut = statut.clone();
            s.scelles = scelles;
            s.a_rejouer = None;
        }
        Ok(statut)
    }

    async fn auth_status(&self) -> AuthStatus {
        self.statut()
    }

    async fn logout(&mut self) -> Result<(), TuneError> {
        let _ = self.appeler("logout", json!({})).await;
        if let Some(r) = &self.reseau {
            r.oublier(&self.id_greffon);
        }
        if let Ok(mut s) = self.session.lock() {
            *s = Session::default();
        }
        Ok(())
    }

    async fn search(&self, query: &str, limit: usize) -> Result<SearchResults, TuneError> {
        let r = self
            .appeler("search", json!({ "query": query, "limit": limit }))
            .await?;
        let mut v = r;
        if let Some(o) = v.as_object_mut() {
            for cle in ["tracks", "albums", "artists", "playlists"] {
                o.entry(cle).or_insert_with(|| json!([]));
            }
            if let Some(pistes) = o.get_mut("tracks").and_then(Value::as_array_mut) {
                for p in pistes.iter_mut().filter_map(Value::as_object_mut) {
                    for (cle, defaut) in defauts_piste() {
                        p.entry(cle).or_insert(defaut);
                    }
                }
            }
            if let Some(albums) = o.get_mut("albums").and_then(Value::as_array_mut) {
                for a in albums.iter_mut().filter_map(Value::as_object_mut) {
                    a.entry("track_count").or_insert(json!(0));
                }
            }
        }
        lire(v, &[])
    }

    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, TuneError> {
        lire(
            self.appeler("get_track", json!({ "id": track_id })).await?,
            &defauts_piste(),
        )
    }

    async fn get_track_url(
        &self,
        track_id: &str,
        quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        lire(
            self.appeler(
                "get_track_url",
                json!({ "id": track_id, "quality": quality }),
            )
            .await?,
            &[("headers", json!([])), ("expires_at", Value::Null)],
        )
    }

    async fn get_album(&self, album_id: &str) -> Result<StreamAlbum, TuneError> {
        lire(
            self.appeler("get_album", json!({ "id": album_id })).await?,
            &[("track_count", json!(0))],
        )
    }

    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        lire_pistes(
            self.appeler("get_album_tracks", json!({ "id": album_id }))
                .await?,
        )
    }

    async fn get_artist(&self, _artist_id: &str) -> Result<StreamArtist, TuneError> {
        Err(TuneError::Unsupported(
            "les artistes des sources WASM viennent à l'étape suivante (#6197)".into(),
        ))
    }

    async fn get_playlist(&self, _playlist_id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(TuneError::Unsupported(
            "les playlists des sources WASM viennent à l'étape suivante (#6197)".into(),
        ))
    }

    async fn get_playlist_tracks(&self, _playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(TuneError::Unsupported(
            "les playlists des sources WASM viennent à l'étape suivante (#6197)".into(),
        ))
    }

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(Vec::new())
    }

    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(Vec::new())
    }

    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(Vec::new())
    }

    /// Les identifiants SCELLÉS : rien de lisible ne part en base.
    fn save_tokens(&self) -> Option<Value> {
        let scelles = self.session.lock().ok()?.scelles.clone()?;
        Some(json!({ "sealed_credentials": scelles }))
    }

    /// Ouvre les identifiants scellés ; `post_restore` rejoue la connexion.
    fn restore_tokens(&mut self, tokens: &Value) -> bool {
        let Some(scelles) = tokens.get("sealed_credentials") else {
            return false;
        };
        match self.coffre.ouvrir(&self.id_greffon, &self.nom, scelles) {
            Ok(clair) => {
                if let Ok(mut s) = self.session.lock() {
                    s.scelles = Some(scelles.clone());
                    s.a_rejouer = Some(clair);
                }
                true
            }
            Err(e) => {
                warn!(service = %self.nom, error = %e, "source_wasm_credentials_unreadable");
                false
            }
        }
    }

    async fn post_restore(&mut self) {
        let a_rejouer = self
            .session
            .lock()
            .ok()
            .and_then(|mut s| s.a_rejouer.take());
        let Some(identifiants) = a_rejouer else {
            return;
        };
        match self.connecter(&identifiants).await {
            Ok(statut) => {
                if let Ok(mut s) = self.session.lock() {
                    s.statut = statut;
                }
            }
            Err(e) => warn!(service = %self.nom, error = %e, "source_wasm_relogin_failed"),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// L'inscription
// ───────────────────────────────────────────────────────────────────────────

/// Le coffre du serveur : la clé à côté de la base (ou désignée par
/// `TUNE_PLUGIN_CREDENTIALS_KEY`), éphémère pour une base en mémoire.
pub fn coffre_du_serveur(db_path: &str) -> Arc<CoffreDIdentifiants> {
    let coffre = match CoffreDIdentifiants::chemin_pour_la_base(db_path) {
        Some(chemin) => CoffreDIdentifiants::ouvrir_ou_creer(&chemin).inspect_err(|e| {
            warn!(error = %e, "plugin_credentials_key_unavailable — clé éphémère");
        }),
        None => Err(String::new()),
    };
    Arc::new(
        coffre
            .or_else(|_| CoffreDIdentifiants::ephemere())
            .expect("aléa du système indisponible"),
    )
}

/// Pourquoi une source n'est pas inscrite.
#[derive(Debug, PartialEq, Eq)]
pub enum RefusDInscription {
    SansSource,
    SansPermission,
    NomInvalide(String),
    NomPris(String),
}

/// Inscrire la source d'un greffon chargé dans le registre des services, puis
/// rendre sa session si des identifiants scellés l'attendent en base.
pub async fn inscrire_la_source(
    services: &Arc<tokio::sync::Mutex<tune_core::streaming::ServiceRegistry>>,
    backend: &Arc<dyn tune_core::db::backend::DbBackend>,
    manifeste: &PluginManifest,
    greffon: Arc<tokio::sync::Mutex<WasmPlugin>>,
    coffre: Arc<CoffreDIdentifiants>,
    reseau: Option<Arc<ReseauDesGreffons>>,
) -> Result<String, RefusDInscription> {
    let Some(source) = manifeste.streaming_source.as_ref() else {
        return Err(RefusDInscription::SansSource);
    };
    if !manifeste.permissions.iter().any(|p| p == PERMISSION_SOURCE) {
        return Err(RefusDInscription::SansPermission);
    }
    let Some(mut service) = WasmStreamingService::new(manifeste, greffon, coffre, reseau) else {
        return Err(RefusDInscription::NomInvalide(source.name.clone()));
    };
    let nom = source.name.clone();
    // Le registre n'est PAS tenu pendant la reconnexion : elle passe par le
    // greffon et le réseau, et rien ne doit attendre derrière.
    if services.lock().await.get(&nom).is_some() {
        return Err(RefusDInscription::NomPris(nom));
    }
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
    if let Some(v) = settings
        .get(&format!("streaming_{nom}_enabled"))
        .ok()
        .flatten()
    {
        service.set_enabled(v != "false");
    }
    let jetons = settings
        .get(&format!("auth_tokens_{nom}"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    if let Some(jetons) = jetons
        && service.restore_tokens(&jetons)
    {
        service.post_restore().await;
    }
    let mut registre = services.lock().await;
    if registre.get(&nom).is_some() {
        return Err(RefusDInscription::NomPris(nom));
    }
    registre.register(Box::new(service));
    info!(service = %nom, plugin = %manifeste.id, "source_wasm_registered");
    Ok(nom)
}

#[cfg(test)]
#[path = "sources_wasm_tests.rs"]
mod tests;
