//! #6197 — les greffons WASM qui FOURNISSENT une source de streaming.
//!
//! Trois pièces, décrites dans `docs/chantier-sources-wasm.md` :
//!
//! - [`ReseauDesGreffons`] : la requête HTTP qu'un greffon demande par
//!   `host_http`, faite par l'hôte (le runtime a déjà jugé la permission et la
//!   liste de domaines), avec une [`BoiteACookies`] cloisonnée par greffon et
//!   par hôte distant. Les `Set-Cookie` sont absorbés et jamais montrés au
//!   greffon ; les redirections ne sont pas suivies (la suivante repasse par
//!   la liste de domaines). Étape 2 : chaque requête passe un QUOTA par
//!   greffon et le BUDGET de l'appel en cours ([`BorneDAppel`]), elle est
//!   notée dans le [`journal`](ReseauDesGreffons::journal) que l'écran montre,
//!   et les cookies sont persistés SCELLÉS.
//! - [`WasmStreamingService`] : l'adaptateur qui implémente
//!   [`StreamingService`] en appelant les fonctions EXPORTÉES du greffon
//!   (`login`, `search`, `get_album`, `get_track_url`…). Chaque appel se joue
//!   dans `spawn_blocking` : le greffon est synchrone, et `host_http` y fait
//!   `Handle::block_on` sur la requête asynchrone — le pont de #4716.
//! - [`inscrire_la_source`] : l'inscription dans le registre des services au
//!   chargement des greffons, et le retour de la session au démarrage depuis
//!   les identifiants SCELLÉS ([`CoffreDIdentifiants`]).
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::TuneError;
use tune_core::coffre_greffons::CoffreDIdentifiants;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::plugins::{ChampDeConnexion, PluginManifest};
use tune_core::streaming::traits::{
    AuthStatus, SearchPage, SearchResults, SearchTotals, StreamAlbum, StreamArtist, StreamGenre,
    StreamPlaylist, StreamTrack, StreamUrl, StreamingService,
};
use tune_plugin_runtime_wasm::WasmPlugin;

/// La permission qui autorise un greffon à s'inscrire comme service.
pub const PERMISSION_SOURCE: &str = "streaming_source";

/// Plafond du corps d'une réponse rendue à un greffon.
pub const TAILLE_MAX_REPONSE: usize = 4 * 1024 * 1024;

/// Délai d'une requête d'un greffon, connexion comprise.
pub const DELAI_HTTP: Duration = Duration::from_secs(20);

/// Étape 2 — le temps que dure, AU PLUS, un appel au greffon (une fonction
/// exportée ou une route) côté réseau : passé ce délai, `host_http` refuse
/// (`call_timeout`) et la requête en cours est coupée à l'échéance. Le
/// carburant du runtime borne déjà le calcul ; ceci borne l'attente.
pub const DELAI_PAR_APPEL: Duration = Duration::from_secs(45);

/// Étape 2 — le nombre de requêtes qu'UN appel au greffon peut faire.
pub const REQUETES_PAR_APPEL: u32 = 32;

/// Étape 2 — le quota d'un greffon, toutes requêtes confondues, sur une
/// fenêtre glissante d'une minute.
pub const REQUETES_PAR_MINUTE: usize = 240;

/// Le nombre d'hôtes distincts que le journal d'un greffon retient ; au-delà,
/// les suivants sont comptés ensemble sous [`HOTE_AUTRES`].
pub const HOTES_DANS_LE_JOURNAL: usize = 64;

/// La ligne du journal qui regroupe les hôtes au-delà de
/// [`HOTES_DANS_LE_JOURNAL`].
pub const HOTE_AUTRES: &str = "(autres)";

/// Le « service » sous lequel les cookies d'un greffon sont scellés. Un `#`
/// n'est jamais dans un nom de service (`[a-z0-9_-]`) : le scellé des cookies
/// ne s'ouvre pas comme celui des identifiants, ni l'inverse.
const SCELLE_COOKIES: &str = "#cookies";

/// Les méthodes qu'un greffon peut employer.
const METHODES: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];

/// En-têtes qu'un greffon ne pose pas lui-même : l'hôte les calcule.
const EN_TETES_INTERDITS: &[&str] = &["host", "content-length", "transfer-encoding", "connection"];

fn maintenant_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// La clé `settings` des cookies scellés d'un greffon.
pub fn cle_des_cookies(greffon: &str) -> String {
    format!("plugin_cookies_{greffon}")
}

// ───────────────────────────────────────────────────────────────────────────
// La boîte à cookies
// ───────────────────────────────────────────────────────────────────────────

/// Les cookies des greffons, cloisonnés par greffon PUIS par hôte distant.
///
/// Volontairement simple : un cookie vaut pour l'hôte exact qui l'a posé
/// (pas de `Domain=` élargi, pas de `Path=`). Un `Max-Age=0` ou une valeur
/// vide l'efface. Persistés scellés depuis l'étape 2 (voir
/// [`ReseauDesGreffons::brancher_la_persistance`]).
#[derive(Debug, Default)]
pub struct BoiteACookies {
    par_greffon: HashMap<String, HashMap<String, BTreeMap<String, String>>>,
}

impl BoiteACookies {
    /// Ranger les `Set-Cookie` qu'a rendus `hote` pour `greffon`. Rend vrai si
    /// la boîte de ce greffon a changé.
    pub fn absorber<'a>(
        &mut self,
        greffon: &str,
        hote: &str,
        set_cookies: impl IntoIterator<Item = &'a str>,
    ) -> bool {
        let jarre = self
            .par_greffon
            .entry(greffon.to_string())
            .or_default()
            .entry(hote.to_ascii_lowercase())
            .or_default();
        let mut change = false;
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
                change |= jarre.remove(nom).is_some();
            } else if jarre.get(nom).map(String::as_str) != Some(valeur) {
                jarre.insert(nom.to_string(), valeur.to_string());
                change = true;
            }
        }
        change
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

    /// La boîte d'un greffon, `{hôte: {nom: valeur}}`, pour la sceller.
    fn exporter(&self, greffon: &str) -> Value {
        let mut sortie = serde_json::Map::new();
        if let Some(hotes) = self.par_greffon.get(greffon) {
            for (hote, jarre) in hotes {
                if !jarre.is_empty() {
                    sortie.insert(hote.clone(), json!(jarre));
                }
            }
        }
        Value::Object(sortie)
    }

    /// Remettre la boîte d'un greffon depuis [`Self::exporter`].
    fn importer(&mut self, greffon: &str, v: &Value) {
        let mut hotes: HashMap<String, BTreeMap<String, String>> = HashMap::new();
        for (hote, jarre) in v.as_object().into_iter().flatten() {
            let jarre: BTreeMap<String, String> = jarre
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(n, v)| v.as_str().map(|v| (n.clone(), v.to_string())))
                .collect();
            if !jarre.is_empty() {
                hotes.insert(hote.to_ascii_lowercase(), jarre);
            }
        }
        if hotes.is_empty() {
            self.par_greffon.remove(greffon);
        } else {
            self.par_greffon.insert(greffon.to_string(), hotes);
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Le journal réseau, le quota et le budget d'un appel
// ───────────────────────────────────────────────────────────────────────────

/// Une ligne du journal réseau d'un greffon : un hôte distant.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct EntreeDuJournal {
    pub host: String,
    /// Requêtes parties vers cet hôte (réponse reçue ou échec réseau).
    pub requests: u64,
    /// Requêtes REFUSÉES avant tout réseau : domaine hors liste, quota,
    /// budget de l'appel…
    pub refused: u64,
    /// Le dernier statut HTTP reçu.
    pub last_status: Option<u16>,
    /// La dernière erreur : le code d'un refus, ou l'échec réseau.
    pub last_error: Option<String>,
    /// Quand, en millisecondes depuis l'époque Unix.
    pub last_at_ms: u64,
}

/// Les bornes du réseau des greffons. [`Default`] : les constantes du module.
#[derive(Debug, Clone, Copy)]
pub struct Limites {
    pub delai_par_appel: Duration,
    pub requetes_par_appel: u32,
    pub requetes_par_minute: usize,
}

impl Default for Limites {
    fn default() -> Self {
        Self {
            delai_par_appel: DELAI_PAR_APPEL,
            requetes_par_appel: REQUETES_PAR_APPEL,
            requetes_par_minute: REQUETES_PAR_MINUTE,
        }
    }
}

/// Le budget de l'appel en cours d'un greffon.
#[derive(Debug, Clone, Copy)]
struct Budget {
    echeance: Instant,
    restantes: u32,
}

/// Tant qu'elle vit, les requêtes de son greffon comptent sur le budget d'UN
/// appel ([`Limites::delai_par_appel`], [`Limites::requetes_par_appel`]).
/// L'appel au greffon est sérialisé par son verrou : une borne à la fois.
pub struct BorneDAppel {
    reseau: Arc<ReseauDesGreffons>,
    greffon: String,
}

impl Drop for BorneDAppel {
    fn drop(&mut self) {
        if let Ok(mut b) = self.reseau.budgets.lock() {
            b.remove(&self.greffon);
        }
    }
}

/// Où les cookies sont persistés : la base, scellés par le coffre.
struct Persistance {
    backend: Arc<dyn DbBackend>,
    coffre: Arc<CoffreDIdentifiants>,
}

// ───────────────────────────────────────────────────────────────────────────
// La requête HTTP d'un greffon
// ───────────────────────────────────────────────────────────────────────────

/// Le client HTTP des greffons, leur boîte à cookies, leur journal et leurs
/// quotas.
pub struct ReseauDesGreffons {
    client: reqwest::Client,
    limites: Limites,
    cookies: std::sync::Mutex<BoiteACookies>,
    budgets: std::sync::Mutex<HashMap<String, Budget>>,
    fenetres: std::sync::Mutex<HashMap<String, VecDeque<Instant>>>,
    journal: std::sync::Mutex<HashMap<String, BTreeMap<String, EntreeDuJournal>>>,
    persistance: OnceLock<Persistance>,
}

impl Default for ReseauDesGreffons {
    fn default() -> Self {
        Self::new()
    }
}

impl ReseauDesGreffons {
    pub fn new() -> Self {
        Self::avec_limites(Limites::default())
    }

    pub fn avec_limites(limites: Limites) -> Self {
        let client = tune_core::http::client::builder()
            // Une redirection suivie ici sauterait la liste de domaines.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(DELAI_HTTP)
            .user_agent(concat!("Tune/", env!("CARGO_PKG_VERSION"), " (greffon)"))
            .build()
            .unwrap_or_default();
        Self {
            client,
            limites,
            cookies: std::sync::Mutex::new(BoiteACookies::default()),
            budgets: std::sync::Mutex::new(HashMap::new()),
            fenetres: std::sync::Mutex::new(HashMap::new()),
            journal: std::sync::Mutex::new(HashMap::new()),
            persistance: OnceLock::new(),
        }
    }

    /// Étape 2 — persister les cookies des greffons dans `settings`
    /// ([`cle_des_cookies`]), scellés par `coffre`. Sans cet appel, ils vivent
    /// en mémoire seulement. Un second appel est sans effet.
    pub fn brancher_la_persistance(
        &self,
        backend: Arc<dyn DbBackend>,
        coffre: Arc<CoffreDIdentifiants>,
    ) {
        let _ = self.persistance.set(Persistance { backend, coffre });
    }

    /// Étape 2 — relire les cookies scellés de `greffon`. Un scellé illisible
    /// (autre clé, altéré) est oublié : il ne rouvrirait jamais.
    pub fn restaurer_les_cookies(&self, greffon: &str) {
        let Some(p) = self.persistance.get() else {
            return;
        };
        let settings = SettingsRepo::with_backend(p.backend.clone());
        let Some(brut) = settings.get(&cle_des_cookies(greffon)).ok().flatten() else {
            return;
        };
        let ouvert = serde_json::from_str::<Value>(&brut)
            .map_err(|e| e.to_string())
            .and_then(|scelle| p.coffre.ouvrir(greffon, SCELLE_COOKIES, &scelle));
        match ouvert {
            Ok(v) => {
                if let Ok(mut b) = self.cookies.lock() {
                    b.importer(greffon, &v);
                }
            }
            Err(e) => {
                warn!(plugin = %greffon, error = %e, "plugin_cookies_unreadable");
                let _ = settings.delete(&cle_des_cookies(greffon));
            }
        }
    }

    /// Écrire la boîte de `greffon`, scellée — ou effacer la ligne si elle est
    /// vide.
    fn persister(&self, greffon: &str) {
        let Some(p) = self.persistance.get() else {
            return;
        };
        let Some(boite) = self.cookies.lock().ok().map(|b| b.exporter(greffon)) else {
            return;
        };
        let settings = SettingsRepo::with_backend(p.backend.clone());
        let cle = cle_des_cookies(greffon);
        if boite.as_object().is_none_or(|o| o.is_empty()) {
            let _ = settings.delete(&cle);
            return;
        }
        match p.coffre.sceller(greffon, SCELLE_COOKIES, &boite) {
            Ok(scelle) => {
                if let Err(e) = settings.set(&cle, &scelle.to_string()) {
                    warn!(plugin = %greffon, error = %e, "plugin_cookies_not_saved");
                }
            }
            Err(e) => warn!(plugin = %greffon, error = %e, "plugin_cookies_not_sealed"),
        }
    }

    /// Oublier les cookies d'un greffon, en mémoire ET en base.
    pub fn oublier(&self, greffon: &str) {
        if let Ok(mut b) = self.cookies.lock() {
            b.oublier(greffon);
        }
        self.persister(greffon);
    }

    /// L'en-tête `Cookie` que la boîte de `greffon` tient pour `hote` — pour
    /// l'adresse de lecture (le téléchargeur de l'hôte le rejoue ; le greffon
    /// ne le voit toujours pas).
    pub fn cookie_pour(&self, greffon: &str, hote: &str) -> Option<String> {
        self.cookies.lock().ok()?.en_tete(greffon, hote)
    }

    /// Ouvrir le budget d'UN appel de `greffon` : il se referme quand la
    /// borne rendue est lâchée.
    pub fn borner(self: &Arc<Self>, greffon: &str) -> BorneDAppel {
        if let Ok(mut b) = self.budgets.lock() {
            b.insert(
                greffon.to_string(),
                Budget {
                    echeance: Instant::now() + self.limites.delai_par_appel,
                    restantes: self.limites.requetes_par_appel,
                },
            );
        }
        BorneDAppel {
            reseau: self.clone(),
            greffon: greffon.to_string(),
        }
    }

    /// Le journal réseau de `greffon`, un hôte par ligne, dans l'ordre des
    /// noms.
    pub fn journal(&self, greffon: &str) -> Vec<EntreeDuJournal> {
        self.journal
            .lock()
            .ok()
            .and_then(|j| j.get(greffon).map(|h| h.values().cloned().collect()))
            .unwrap_or_default()
    }

    fn noter(&self, greffon: &str, hote: &str, f: impl FnOnce(&mut EntreeDuJournal)) {
        let Ok(mut j) = self.journal.lock() else {
            return;
        };
        let hotes = j.entry(greffon.to_string()).or_default();
        let cle = if hotes.contains_key(hote) || hotes.len() < HOTES_DANS_LE_JOURNAL {
            hote.to_string()
        } else {
            HOTE_AUTRES.to_string()
        };
        let e = hotes.entry(cle.clone()).or_insert_with(|| EntreeDuJournal {
            host: cle,
            ..Default::default()
        });
        e.last_at_ms = maintenant_ms();
        f(e);
    }

    /// Noter un refus (du runtime ou de l'hôte) dans le journal de `greffon`.
    pub fn noter_un_refus(&self, greffon: &str, hote: &str, code: &str) {
        self.noter(greffon, hote, |e| {
            e.refused += 1;
            e.last_error = Some(code.to_string());
        });
    }

    /// La requête peut-elle partir ? Rend le délai qui lui reste, ou le code
    /// du refus. Compte la requête dans le budget de l'appel et le quota.
    fn admettre(&self, greffon: &str) -> Result<Duration, &'static str> {
        let maintenant = Instant::now();
        let mut delai = DELAI_HTTP;
        if let Ok(mut budgets) = self.budgets.lock()
            && let Some(b) = budgets.get_mut(greffon)
        {
            if maintenant >= b.echeance {
                return Err("call_timeout");
            }
            if b.restantes == 0 {
                return Err("too_many_requests");
            }
            b.restantes -= 1;
            delai = delai.min(b.echeance - maintenant);
        }
        if let Ok(mut fenetres) = self.fenetres.lock() {
            let f = fenetres.entry(greffon.to_string()).or_default();
            while f
                .front()
                .is_some_and(|t| maintenant.duration_since(*t) >= Duration::from_secs(60))
            {
                f.pop_front();
            }
            if f.len() >= self.limites.requetes_par_minute {
                return Err("rate_limited");
            }
            f.push_back(maintenant);
        }
        Ok(delai)
    }

    /// Faire la requête `requete` (`{method, url, headers?, body?}`) pour
    /// `greffon`. Rend `{status, headers, body}` ; le corps est du texte
    /// (UTF-8 avec remplacement), borné à [`TAILLE_MAX_REPONSE`]. Un refus de
    /// l'hôte (quota, budget de l'appel) rend `{error, detail}`, comme un
    /// refus du runtime, sans aucun réseau.
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

        let delai = match self.admettre(greffon) {
            Ok(d) => d,
            Err(code) => {
                self.noter_un_refus(greffon, &hote, code);
                return Ok(json!({ "error": code, "detail": hote }));
            }
        };

        let mut construite = self.client.request(methode, analysee).timeout(delai);
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
        let cookie = match (self.cookie_pour(greffon, &hote), cookie_du_greffon) {
            (Some(a), Some(b)) => Some(format!("{a}; {b}")),
            (a, b) => a.or(b),
        };
        if let Some(c) = cookie {
            construite = construite.header(reqwest::header::COOKIE, c);
        }
        if let Some(corps) = requete.get("body").and_then(Value::as_str) {
            construite = construite.body(corps.to_string());
        }

        let mut reponse = match construite.send().await {
            Ok(r) => r,
            Err(e) => {
                let texte = format!("http: {e}");
                self.noter(greffon, &hote, |j| {
                    j.requests += 1;
                    j.last_status = None;
                    j.last_error = Some(texte.clone());
                });
                return Err(texte);
            }
        };
        let statut = reponse.status().as_u16();
        self.noter(greffon, &hote, |j| {
            j.requests += 1;
            j.last_status = Some(statut);
            j.last_error = None;
        });
        let set_cookies: Vec<String> = reponse
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_string))
            .collect();
        let change = self
            .cookies
            .lock()
            .is_ok_and(|mut b| b.absorber(greffon, &hote, set_cookies.iter().map(String::as_str)));
        if change {
            self.persister(greffon);
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

/// Les fonctions qu'un greffon source PEUT exporter, en plus des obligatoires
/// (`login`, `search`, `get_album`, `get_track_url`). Une absente rend ce que
/// le trait rend par défaut : une liste vide, ou « non pris en charge » pour
/// un objet seul. Voir le tableau de `docs/chantier-sources-wasm.md`.
pub const EXPORTS_FACULTATIFS: &[&str] = &[
    "logout",
    "search_page",
    "get_track",
    "get_album_tracks",
    "get_artist",
    "get_artist_albums",
    "get_artist_top_tracks",
    "get_playlist",
    "get_playlist_tracks",
    "get_user_playlists",
    "get_user_albums",
    "get_user_artists",
    "get_user_tracks",
    "add_favorite",
    "remove_favorite",
    "get_featured",
    "get_new_releases",
    "get_genres",
    "get_genre_albums",
];

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
    /// La liste de domaines du manifeste, normalisée : l'adresse de lecture
    /// doit y être, comme toute requête du greffon.
    domaines: Vec<String>,
    /// Les fonctions exportées, relevées à l'inscription. `None` : inconnues,
    /// chaque appel est tenté.
    exports: Option<HashSet<String>>,
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
            domaines: tune_plugin_runtime_wasm::reseau::normaliser_domaines(
                &manifeste.http_allowed_domains,
            ),
            exports: None,
            greffon,
            coffre,
            reseau,
            actif: true,
            session: std::sync::Mutex::new(Session::default()),
        })
    }

    /// Relever les fonctions facultatives que le greffon exporte.
    pub async fn relever_les_exports(&mut self) {
        let mut g = self.greffon.lock().await;
        self.exports = Some(
            EXPORTS_FACULTATIFS
                .iter()
                .filter(|e| g.has_export(e))
                .map(|e| e.to_string())
                .collect(),
        );
    }

    /// Le greffon exporte-t-il `export` ? Vrai tant que c'est inconnu.
    pub fn fournit(&self, export: &str) -> bool {
        self.exports.as_ref().is_none_or(|e| e.contains(export))
    }

    /// Le pont : jouer la fonction exportée `export` sur un fil bloquant, et
    /// rendre son JSON — ou l'erreur que le greffon a rendue (`{"error"}`).
    ///
    /// L'appel a son budget réseau ([`ReseauDesGreffons::borner`]), ouvert
    /// APRÈS la prise du verrou : une route du greffon qui le tient encore ne
    /// mange pas le délai de cet appel.
    ///
    /// Une erreur qui porte `"status": 401` ou `403` est rendue avec le
    /// statut dans son texte (`HTTP 401 : …`) : c'est à ce texte que la
    /// lecture reconnaît une session expirée et appelle
    /// [`StreamingService::refresh_if_needed`] — la même règle que pour
    /// Qobuz ou TIDAL.
    async fn appeler(&self, export: &str, entree: Value) -> Result<Value, TuneError> {
        let greffon = self.greffon.clone();
        let reseau = self.reseau.clone();
        let id = self.id_greffon.clone();
        let export_nom = export.to_string();
        let entree = entree.to_string();
        let sortie = tokio::task::spawn_blocking(move || {
            let mut g = greffon.blocking_lock();
            let _borne = reseau.as_ref().map(|r| r.borner(&id));
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
            return Err(match v.get("status").and_then(Value::as_u64) {
                Some(s @ (401 | 403)) => TuneError::Streaming(format!("HTTP {s} : {texte}")),
                _ => TuneError::Streaming(texte),
            });
        }
        Ok(v)
    }

    /// Un appel à une fonction FACULTATIVE qui rend une liste : vide si le
    /// greffon ne l'exporte pas, comme le défaut du trait.
    async fn liste<T: serde::de::DeserializeOwned>(
        &self,
        export: &str,
        entree: Value,
        defauts: &[(&str, Value)],
    ) -> Result<Vec<T>, TuneError> {
        if !self.fournit(export) {
            return Ok(Vec::new());
        }
        match self.appeler(export, entree).await {
            Ok(v) => lire_liste(v, defauts),
            Err(TuneError::Unsupported(_)) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Un appel à une fonction FACULTATIVE qui rend un objet : « non pris en
    /// charge » si le greffon ne l'exporte pas.
    async fn objet<T: serde::de::DeserializeOwned>(
        &self,
        export: &str,
        entree: Value,
        defauts: &[(&str, Value)],
    ) -> Result<T, TuneError> {
        if !self.fournit(export) {
            return Err(TuneError::Unsupported(format!(
                "le greffon {} ne fournit pas `{export}`",
                self.id_greffon
            )));
        }
        lire(self.appeler(export, entree).await?, defauts)
    }

    /// Les pistes que l'objet rendu par `export` (`get_album`,
    /// `get_playlist`) porte sous `tracks` — vide s'il n'en porte pas.
    async fn pistes_de(&self, export: &str, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let v = self.appeler(export, json!({ "id": id })).await?;
        match v.get("tracks") {
            Some(t) => lire_liste(t.clone(), &defauts_piste()),
            None => Ok(Vec::new()),
        }
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

    /// L'adresse de lecture que rend le greffon, mise en état d'être LUE par
    /// l'hôte (étape 2, « lecture réelle ») :
    ///
    /// - elle doit être dans la liste de domaines du manifeste, comme toute
    ///   requête du greffon — sans quoi un greffon ferait télécharger à Tune
    ///   ce que `host_http` lui refuse ;
    /// - les en-têtes que l'hôte calcule (`host`, `content-length`…) sont
    ///   écartés ;
    /// - les cookies que la boîte tient pour CET hôte sont ajoutés : un CDN
    ///   qui exige la session la reçoit du téléchargeur de l'hôte, et le
    ///   greffon ne la voit toujours pas.
    fn preparer_l_adresse(&self, mut flux: StreamUrl) -> Result<StreamUrl, TuneError> {
        let hote =
            match tune_plugin_runtime_wasm::reseau::verifier_l_adresse(&flux.url, &self.domaines) {
                Ok(h) => h,
                Err(refus) => {
                    if let Some(r) = &self.reseau {
                        r.noter_un_refus(&self.id_greffon, &refus.detail, refus.code);
                    }
                    return Err(TuneError::Streaming(format!(
                        "adresse de lecture refusée ({}) : {}",
                        refus.code, refus.detail
                    )));
                }
            };
        let mut cookie_du_greffon: Option<String> = None;
        flux.headers.retain(|(nom, valeur)| {
            let nom = nom.to_ascii_lowercase();
            if nom == "cookie" {
                cookie_du_greffon = Some(valeur.clone());
                return false;
            }
            !EN_TETES_INTERDITS.contains(&nom.as_str())
        });
        let de_la_boite = self
            .reseau
            .as_ref()
            .and_then(|r| r.cookie_pour(&self.id_greffon, &hote));
        let cookie = match (de_la_boite, cookie_du_greffon) {
            (Some(a), Some(b)) => Some(format!("{a}; {b}")),
            (a, b) => a.or(b),
        };
        if let Some(c) = cookie {
            flux.headers.push(("Cookie".to_string(), c));
        }
        Ok(flux)
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

fn lire_liste<T: serde::de::DeserializeOwned>(
    v: Value,
    defauts: &[(&str, Value)],
) -> Result<Vec<T>, TuneError> {
    let liste = v.as_array().cloned().ok_or_else(|| {
        TuneError::Streaming("réponse du greffon : une liste est attendue".into())
    })?;
    liste.into_iter().map(|p| lire(p, defauts)).collect()
}

fn defauts_piste() -> [(&'static str, Value); 2] {
    [("explicit", json!(false)), ("duration_ms", json!(0))]
}

fn defauts_album() -> [(&'static str, Value); 1] {
    [("track_count", json!(0))]
}

fn defauts_playlist() -> [(&'static str, Value); 1] {
    [("track_count", json!(0))]
}

fn defauts_genre() -> [(&'static str, Value); 1] {
    [("has_children", json!(false))]
}

/// Les résultats d'une recherche, complétés des champs que le greffon peut
/// omettre (listes vides, `explicit`, `duration_ms`, `track_count`).
fn completer_les_resultats(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        for cle in ["tracks", "albums", "artists", "playlists"] {
            o.entry(cle).or_insert_with(|| json!([]));
        }
        let completer = |o: &mut serde_json::Map<String, Value>, cle: &str, d: &[(&str, Value)]| {
            if let Some(liste) = o.get_mut(cle).and_then(Value::as_array_mut) {
                for e in liste.iter_mut().filter_map(Value::as_object_mut) {
                    for (c, defaut) in d {
                        e.entry(c.to_string()).or_insert_with(|| defaut.clone());
                    }
                }
            }
        };
        completer(o, "tracks", &defauts_piste());
        completer(o, "albums", &defauts_album());
        completer(o, "playlists", &defauts_playlist());
    }
    v
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
        if self.fournit("logout") {
            let _ = self.appeler("logout", json!({})).await;
        }
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
        lire(completer_les_resultats(r), &[])
    }

    /// La pagination (étape 2) : si le greffon exporte `search_page`, il
    /// reçoit `{query, limit, offset}` et rend une page de recherche avec,
    /// s'il les connaît, `totals` et `has_more`. Sinon, le défaut du trait :
    /// une page unique, et rien au-delà.
    async fn search_page(
        &self,
        query: &str,
        limit: usize,
        offset: usize,
    ) -> Result<SearchPage, TuneError> {
        if !self.fournit("search_page") {
            if offset > 0 {
                return Ok(SearchPage::au_dela(offset));
            }
            let borne = tune_core::streaming::traits::limite_hors_pagination(limit);
            let mut page = SearchPage::page_unique_bornee(self.search(query, borne).await?, borne);
            page.has_more = false;
            return Ok(page);
        }
        let limite = tune_core::streaming::traits::limite_hors_pagination(limit);
        let mut v = completer_les_resultats(
            self.appeler(
                "search_page",
                json!({ "query": query, "limit": limite, "offset": offset }),
            )
            .await?,
        );
        let totals = v
            .get("totals")
            .cloned()
            .and_then(|t| serde_json::from_value::<SearchTotals>(t).ok());
        let has_more = v.get("has_more").and_then(Value::as_bool);
        if let Some(o) = v.as_object_mut() {
            for cle in ["totals", "has_more", "offset", "truncated"] {
                o.remove(cle);
            }
        }
        let results: SearchResults = lire(v, &[])?;
        let mut page = SearchPage::page_unique(results);
        page.offset = offset;
        if let Some(t) = totals {
            page.totals = t;
        }
        // Sans `has_more`, une catégorie PLEINE laisse supposer une suite :
        // la même règle que `page_unique_bornee`, sans inventer de total.
        page.has_more = has_more.unwrap_or_else(|| {
            [
                page.results.tracks.len(),
                page.results.albums.len(),
                page.results.artists.len(),
                page.results.playlists.len(),
            ]
            .iter()
            .any(|n| limite > 0 && *n >= limite)
        });
        Ok(page)
    }

    async fn get_track(&self, track_id: &str) -> Result<StreamTrack, TuneError> {
        self.objet("get_track", json!({ "id": track_id }), &defauts_piste())
            .await
    }

    async fn get_track_url(
        &self,
        track_id: &str,
        quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        let flux: StreamUrl = lire(
            self.appeler(
                "get_track_url",
                json!({ "id": track_id, "quality": quality }),
            )
            .await?,
            &[("headers", json!([])), ("expires_at", Value::Null)],
        )?;
        self.preparer_l_adresse(flux)
    }

    async fn get_album(&self, album_id: &str) -> Result<StreamAlbum, TuneError> {
        lire(
            self.appeler("get_album", json!({ "id": album_id })).await?,
            &defauts_album(),
        )
    }

    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        // Sans `get_album_tracks`, les pistes sont celles que `get_album`
        // rend sous `tracks` (le contrat du tableau des exports).
        if !self.fournit("get_album_tracks") {
            return self.pistes_de("get_album", album_id).await;
        }
        lire_liste(
            self.appeler("get_album_tracks", json!({ "id": album_id }))
                .await?,
            &defauts_piste(),
        )
    }

    async fn get_artist(&self, artist_id: &str) -> Result<StreamArtist, TuneError> {
        self.objet("get_artist", json!({ "id": artist_id }), &[])
            .await
    }

    async fn get_artist_albums(&self, artist_id: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        self.get_artist_albums_page(artist_id, 0).await
    }

    async fn get_artist_albums_page(
        &self,
        artist_id: &str,
        offset: u32,
    ) -> Result<Vec<StreamAlbum>, TuneError> {
        self.liste(
            "get_artist_albums",
            json!({ "id": artist_id, "offset": offset }),
            &defauts_album(),
        )
        .await
    }

    async fn get_artist_top_tracks(&self, artist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        self.liste(
            "get_artist_top_tracks",
            json!({ "id": artist_id }),
            &defauts_piste(),
        )
        .await
    }

    async fn get_playlist(&self, playlist_id: &str) -> Result<StreamPlaylist, TuneError> {
        self.objet(
            "get_playlist",
            json!({ "id": playlist_id }),
            &defauts_playlist(),
        )
        .await
    }

    async fn get_playlist_tracks(&self, playlist_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        if !self.fournit("get_playlist_tracks") {
            if !self.fournit("get_playlist") {
                return Err(TuneError::Unsupported(format!(
                    "le greffon {} ne fournit pas `get_playlist_tracks`",
                    self.id_greffon
                )));
            }
            return self.pistes_de("get_playlist", playlist_id).await;
        }
        lire_liste(
            self.appeler("get_playlist_tracks", json!({ "id": playlist_id }))
                .await?,
            &defauts_piste(),
        )
    }

    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        self.liste("get_user_playlists", json!({}), &defauts_playlist())
            .await
    }

    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        self.liste("get_user_albums", json!({}), &defauts_album())
            .await
    }

    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        self.liste("get_user_artists", json!({}), &[]).await
    }

    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
        self.liste("get_user_tracks", json!({}), &defauts_piste())
            .await
    }

    /// Le cœur ne se coche que si le greffon sait l'écrire DANS LES DEUX
    /// SENS : un ajout sans retrait laisserait un favori impossible à ôter.
    fn favoris_ecrivables(&self) -> bool {
        self.fournit("add_favorite") && self.fournit("remove_favorite")
    }

    async fn add_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        if !self.favoris_ecrivables() {
            return Err(TuneError::Unsupported("not supported".into()));
        }
        self.appeler("add_favorite", json!({ "type": fav_type, "id": item_id }))
            .await
            .map(|_| ())
    }

    async fn remove_favorite(&mut self, fav_type: &str, item_id: &str) -> Result<(), TuneError> {
        if !self.favoris_ecrivables() {
            return Err(TuneError::Unsupported("not supported".into()));
        }
        self.appeler(
            "remove_favorite",
            json!({ "type": fav_type, "id": item_id }),
        )
        .await
        .map(|_| ())
    }

    async fn get_featured(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        self.liste("get_featured", json!({}), &defauts_playlist())
            .await
    }

    async fn get_new_releases(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        self.liste("get_new_releases", json!({}), &defauts_album())
            .await
    }

    async fn get_genres(&self, parent_id: Option<&str>) -> Result<Vec<StreamGenre>, TuneError> {
        self.liste(
            "get_genres",
            json!({ "parent_id": parent_id }),
            &defauts_genre(),
        )
        .await
    }

    async fn get_genre_albums(
        &self,
        genre_id: &str,
        limit: usize,
    ) -> Result<Vec<StreamAlbum>, TuneError> {
        self.liste(
            "get_genre_albums",
            json!({ "id": genre_id, "limit": limit }),
            &defauts_album(),
        )
        .await
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

    /// Étape 2 — la session a été refusée (`HTTP 401`/`403`, voir
    /// [`Self::appeler`]) : rejouer `login` avec les identifiants SCELLÉS.
    /// Rend vrai si le service a accepté ; la lecture redemande alors
    /// l'adresse, une fois.
    async fn refresh_if_needed(&mut self) -> Result<bool, TuneError> {
        let scelles = self.session.lock().ok().and_then(|s| s.scelles.clone());
        let Some(scelles) = scelles else {
            return Ok(false);
        };
        let identifiants = self
            .coffre
            .ouvrir(&self.id_greffon, &self.nom, &scelles)
            .map_err(TuneError::Streaming)?;
        let statut = self.connecter(&identifiants).await?;
        let accepte = statut.authenticated;
        if let Ok(mut s) = self.session.lock() {
            s.statut = statut;
        }
        info!(service = %self.nom, accepte, "source_wasm_session_renewed");
        Ok(accepte)
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
    service.relever_les_exports().await;
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

// ───────────────────────────────────────────────────────────────────────────
// Ce que l'écran lit : `GET /api/v1/plugins/sources`
// ───────────────────────────────────────────────────────────────────────────

/// Étape 2 — les sources fournies par les greffons chargés, et le journal
/// réseau de chaque greffon `http`, pour l'écran :
///
/// ```json
/// { "sources": [ { "plugin_id", "service", "display_name", "login_fields",
///                  "http_allowed_domains", "registered" } ],
///   "network": { "<plugin_id>": [ EntreeDuJournal… ] } }
/// ```
///
/// `registered` : le nom est inscrit dans le registre PAR CE greffon (un nom
/// pris par un autre service est refusé au chargement). Rien d'autre du
/// manifeste ne sort, et jamais un identifiant : l'état de connexion se lit,
/// comme pour tout service, sur `/streaming/services`.
pub async fn fiches_des_sources(state: &crate::state::AppState) -> Value {
    let Some(registre) = state.wasm_plugins.get() else {
        return json!({ "sources": [], "network": {} });
    };
    let mut charges: Vec<_> = registre.iter().collect();
    charges.sort_by(|a, b| a.0.cmp(b.0));
    let mut sources = Vec::new();
    let mut reseau = serde_json::Map::new();
    for (id, charge) in charges {
        let m = &charge.manifest;
        if m.permissions.iter().any(|p| p == "http") {
            let journal = registre.reseau().map(|r| r.journal(id)).unwrap_or_default();
            reseau.insert(id.clone(), json!(journal));
        }
        let Some(source) = &m.streaming_source else {
            continue;
        };
        let service = state.services.lock().await.get(&source.name);
        let inscrite = match service {
            Some(s) => s
                .read()
                .await
                .as_any()
                .downcast_ref::<WasmStreamingService>()
                .is_some_and(|w| w.id_greffon == *id),
            None => false,
        };
        let affiche = if source.display_name.is_empty() {
            &source.name
        } else {
            &source.display_name
        };
        sources.push(json!({
            "plugin_id": id,
            "service": source.name,
            "display_name": affiche,
            "login_fields": source.login_fields,
            "http_allowed_domains": m.http_allowed_domains,
            "registered": inscrite,
        }));
    }
    json!({ "sources": sources, "network": reseau })
}

#[cfg(test)]
#[path = "sources_wasm_tests.rs"]
mod tests;
