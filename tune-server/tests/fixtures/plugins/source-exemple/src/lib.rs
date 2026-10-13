//! #6197 — greffon d'exemple : un FAUX service de streaming.
//!
//! Il sert de modèle à qui voudra écrire un connecteur (MusicMe ou un autre)
//! et de banc d'essai au socle des sources WASM. Tout passe par l'hôte :
//!
//! - `host_http` (permission `http`) pour parler au service — l'hôte vérifie
//!   la liste de domaines du manifeste et tient la boîte à cookies ; ce
//!   greffon ne voit jamais le cookie de session que le service pose ;
//! - les fonctions exportées `login`, `search`, `get_album`,
//!   `get_album_tracks`, `get_track`, `get_track_url`, `logout`, que
//!   l'adaptateur `WasmStreamingService` de l'hôte appelle — et, depuis
//!   l'étape 2, les FACULTATIVES : `search_page` (pagination), artistes,
//!   playlists, favoris (lecture et écriture), nouveautés et sélections.
//!
//! Une session refusée par le service (401/403) est rendue avec son statut
//! (`{"error", "status": 401}`) : l'hôte rejoue alors `login` avec les
//! identifiants qu'il a scellés, et redemande. L'adresse de lecture est sur
//! un « CDN » qui exige le cookie de session : l'hôte l'y ajoute lui-même.
//!
//! Le « service » est un serveur HTTP de test, qui parle son propre dialecte
//! (`resultats`, `titre`, `artiste`…) : le travail du greffon est de le
//! traduire dans la forme du cœur (`StreamAlbum`, `StreamTrack`,
//! `StreamUrl`).
use serde_json::{Value, json};
use std::sync::Mutex;

/// La version d'ABI que l'hôte exige.
const ABI: u32 = 1;

/// L'adresse du service quand la connexion n'en donne pas.
const ADRESSE_PAR_DEFAUT: &str = "https://api.exemple.invalid";

#[link(wasm_import_module = "tune")]
unsafe extern "C" {
    fn host_http(ptr: i32, len: i32) -> i64;
    fn host_log(ptr: i32, len: i32);
}

/// La session : l'adresse du service et le nom de l'abonné. Le jeton de
/// session, lui, est un cookie que l'HÔTE garde.
struct Session {
    base: String,
    abonne: String,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

// ── ABI ──────────────────────────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn abi_version() -> u32 {
    ABI
}

#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: u32) -> u32 {
    let boite = vec![0u8; len as usize].into_boxed_slice();
    Box::into_raw(boite) as *mut u8 as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn dealloc(ptr: u32, len: u32) {
    if ptr == 0 {
        return;
    }
    // SAFETY : `ptr`/`len` viennent de `alloc` (boîte de taille exacte).
    unsafe {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            ptr as *mut u8,
            len as usize,
        )));
    }
}

fn lire(ptr: u32, len: u32) -> Value {
    // SAFETY : l'hôte a écrit `len` octets à `ptr`, dans un tampon `alloc`.
    let octets = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    serde_json::from_slice(octets).unwrap_or(Value::Null)
}

fn rendre(v: &Value) -> u64 {
    let octets = serde_json::to_vec(v).unwrap_or_default().into_boxed_slice();
    let len = octets.len() as u64;
    let ptr = Box::into_raw(octets) as *mut u8 as u64;
    (ptr << 32) | len
}

/// Une erreur rendue à l'hôte : un message, et le statut HTTP du service
/// quand c'est lui qui a refusé. `401`/`403` disent « session expirée » :
/// l'hôte se reconnecte et redemande.
struct Erreur {
    message: String,
    status: Option<u64>,
}

impl From<String> for Erreur {
    fn from(message: String) -> Self {
        Self {
            message,
            status: None,
        }
    }
}

impl From<&str> for Erreur {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

fn exporter(ptr: u32, len: u32, f: impl FnOnce(Value) -> Result<Value, Erreur>) -> u64 {
    let entree = lire(ptr, len);
    match f(entree) {
        Ok(v) => rendre(&v),
        Err(e) => rendre(&json!({ "error": e.message, "status": e.status })),
    }
}

// ── Appels à l'hôte ──────────────────────────────────────────────────────

fn journal(msg: &str) {
    let v = serde_json::to_vec(&json!({ "level": "info", "msg": msg })).unwrap_or_default();
    // SAFETY : `v` vit jusqu'au retour de l'appel.
    unsafe { host_log(v.as_ptr() as i32, v.len() as i32) };
}

/// Une requête par l'hôte. Rend `{status, headers, body}` ou l'erreur de
/// l'hôte (`domain_not_allowed`, `permission_denied`…).
fn http(requete: &Value) -> Value {
    let v = serde_json::to_vec(requete).unwrap_or_default();
    // SAFETY : `v` vit jusqu'au retour ; l'hôte écrit sa réponse dans un
    // tampon obtenu par `alloc`, qu'on rend par `dealloc` après lecture.
    let packe = unsafe { host_http(v.as_ptr() as i32, v.len() as i32) } as u64;
    let (ptr, len) = ((packe >> 32) as u32, (packe & 0xFFFF_FFFF) as u32);
    let reponse = lire(ptr, len);
    dealloc(ptr, len);
    reponse
}

/// GET ou POST JSON, et le corps rendu analysé — ou une erreur lisible.
fn appeler(methode: &str, url: &str, corps: Option<&Value>) -> Result<Value, Erreur> {
    let mut requete =
        json!({ "method": methode, "url": url, "headers": { "accept": "application/json" } });
    if let Some(c) = corps {
        requete["headers"]["content-type"] = json!("application/json");
        requete["body"] = json!(c.to_string());
    }
    let reponse = http(&requete);
    if let Some(e) = reponse.get("error").and_then(Value::as_str) {
        return Err(format!("{e}: {}", reponse["detail"].as_str().unwrap_or("")).into());
    }
    let statut = reponse["status"].as_u64().unwrap_or(0);
    if statut == 401 || statut == 403 {
        return Err(Erreur {
            message: "session refusée par le service".to_string(),
            status: Some(statut),
        });
    }
    if !(200..300).contains(&statut) {
        return Err(format!("le service a répondu {statut}").into());
    }
    if reponse["body"].as_str().is_none_or(str::is_empty) {
        return Ok(Value::Null);
    }
    serde_json::from_str(reponse["body"].as_str().unwrap_or("null"))
        .map_err(|e| format!("réponse illisible : {e}").into())
}

fn base() -> Result<String, Erreur> {
    SESSION
        .lock()
        .map_err(|_| Erreur::from("session indisponible"))?
        .as_ref()
        .map(|s| s.base.clone())
        .ok_or_else(|| "non connecté".into())
}

fn encoder(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// ── Traduction du dialecte du service vers la forme du cœur ──────────────

fn qualite() -> Value {
    json!({ "codec": "mp3", "sample_rate": 44100, "bit_depth": 16, "bitrate": 320, "channels": 2 })
}

fn album(a: &Value) -> Value {
    json!({
        "id": a["ref"],
        "title": a["titre"],
        "artist": a["artiste"],
        "artist_id": null,
        "cover_path": a["pochette"],
        "year": a["annee"],
        "track_count": a["pistes"].as_array().map_or(0, Vec::len),
        "quality": qualite(),
    })
}

fn piste(p: &Value, a: &Value) -> Value {
    json!({
        "id": p["ref"],
        "title": p["titre"],
        "artist": a["artiste"],
        "album": a["titre"],
        "album_id": a["ref"],
        "duration_ms": p["duree_s"].as_u64().unwrap_or(0) * 1000,
        "cover_path": a["pochette"],
        "track_number": p["numero"],
        "disc_number": 1,
        "explicit": false,
        "quality": qualite(),
    })
}

fn pistes(a: &Value) -> Vec<Value> {
    a["pistes"]
        .as_array()
        .map(|ps| ps.iter().map(|p| piste(p, a)).collect())
        .unwrap_or_default()
}

fn fiche_album(id: &str) -> Result<Value, Erreur> {
    appeler("GET", &format!("{}/albums/{}", base()?, encoder(id)), None)
}

fn artiste(a: &Value) -> Value {
    json!({ "id": a["ref"], "name": a["nom"], "image_path": a["photo"] })
}

fn playlist(l: &Value) -> Value {
    json!({
        "id": l["ref"],
        "name": l["nom"],
        "description": l["texte"],
        "cover_path": null,
        "track_count": l["pistes"].as_array().map_or(0, Vec::len),
        "owner": l["auteur"],
    })
}

/// Une piste d'une playlist : le service la donne avec son album.
fn piste_de_liste(p: &Value) -> Value {
    piste(p, &p["album"])
}

fn liste_de(v: &Value, f: impl Fn(&Value) -> Value) -> Value {
    json!(v.as_array().map(|a| a.iter().map(f).collect::<Vec<_>>()).unwrap_or_default())
}

fn id_de(v: &Value) -> String {
    encoder(v["id"].as_str().unwrap_or(""))
}

// ── Les fonctions exportées ──────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn login(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |v| {
        let base = v["base_url"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(ADRESSE_PAR_DEFAUT)
            .trim_end_matches('/')
            .to_string();
        let corps = json!({ "email": v["email"], "password": v["password"] });
        let reponse = appeler("POST", &format!("{base}/login"), Some(&corps))?;
        let abonne = reponse["abonne"].as_str().unwrap_or("").to_string();
        journal(&format!("source-exemple : connecté ({abonne})"));
        *SESSION.lock().map_err(|_| "session indisponible")? = Some(Session {
            base,
            abonne: abonne.clone(),
        });
        Ok(json!({
            "authenticated": true,
            "username": abonne,
            "subscription": reponse["offre"],
        }))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn logout(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |_| {
        if let Ok(mut s) = SESSION.lock() {
            if let Some(session) = s.take() {
                journal(&format!("source-exemple : déconnecté ({})", session.abonne));
            }
        }
        Ok(json!({}))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn search(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |v| {
        let q = v["query"].as_str().unwrap_or("");
        let r = appeler(
            "GET",
            &format!("{}/catalogue?q={}", base()?, encoder(q)),
            None,
        )?;
        Ok(resultats(&r))
    })
}

/// Les résultats du catalogue dans la forme du cœur.
fn resultats(r: &Value) -> Value {
    let albums: Vec<Value> = r["resultats"]
        .as_array()
        .map(|a| a.iter().map(album).collect())
        .unwrap_or_default();
    let tracks: Vec<Value> = r["resultats"]
        .as_array()
        .map(|a| a.iter().flat_map(pistes).collect())
        .unwrap_or_default();
    json!({ "tracks": tracks, "albums": albums, "artists": [], "playlists": [] })
}

/// Étape 2 — la recherche PAGINÉE : le service compte en albums
/// (`debut`, `nombre`) et annonce son `total` ; le greffon le dit au cœur
/// (`totals`, `has_more`).
#[unsafe(no_mangle)]
pub extern "C" fn search_page(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |v| {
        let q = v["query"].as_str().unwrap_or("");
        let debut = v["offset"].as_u64().unwrap_or(0);
        let nombre = v["limit"].as_u64().unwrap_or(50);
        let r = appeler(
            "GET",
            &format!(
                "{}/catalogue?q={}&debut={debut}&nombre={nombre}",
                base()?,
                encoder(q)
            ),
            None,
        )?;
        let total = r["total"].as_u64().unwrap_or(0);
        let mut page = resultats(&r);
        let rendus = page["albums"].as_array().map_or(0, Vec::len) as u64;
        page["totals"] = json!({ "tracks": 0, "albums": total, "artists": 0, "playlists": 0 });
        page["has_more"] = json!(debut + rendus < total);
        Ok(page)
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn get_artist(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |v| {
        let a = appeler("GET", &format!("{}/artistes/{}", base()?, id_de(&v)), None)?;
        Ok(artiste(&a))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn get_artist_albums(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |v| {
        if v["offset"].as_u64().unwrap_or(0) > 0 {
            return Ok(json!([]));
        }
        let a = appeler("GET", &format!("{}/artistes/{}", base()?, id_de(&v)), None)?;
        Ok(liste_de(&a["albums"], album))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn get_artist_top_tracks(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |v| {
        let a = appeler("GET", &format!("{}/artistes/{}", base()?, id_de(&v)), None)?;
        let toutes: Vec<Value> = a["albums"]
            .as_array()
            .map(|al| al.iter().flat_map(pistes).collect())
            .unwrap_or_default();
        Ok(json!(toutes))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn get_playlist(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |v| {
        let l = appeler("GET", &format!("{}/listes/{}", base()?, id_de(&v)), None)?;
        Ok(playlist(&l))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn get_playlist_tracks(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |v| {
        let l = appeler("GET", &format!("{}/listes/{}", base()?, id_de(&v)), None)?;
        Ok(liste_de(&l["pistes"], piste_de_liste))
    })
}

/// Les favoris du compte : `{albums: [...], artistes: [...], listes: [...],
/// pistes: [...]}` dans le dialecte du service.
fn favoris() -> Result<Value, Erreur> {
    appeler("GET", &format!("{}/favoris", base()?), None)
}

#[unsafe(no_mangle)]
pub extern "C" fn get_user_albums(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |_| Ok(liste_de(&favoris()?["albums"], album)))
}

#[unsafe(no_mangle)]
pub extern "C" fn get_user_artists(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |_| Ok(liste_de(&favoris()?["artistes"], artiste)))
}

#[unsafe(no_mangle)]
pub extern "C" fn get_user_playlists(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |_| Ok(liste_de(&favoris()?["listes"], playlist)))
}

#[unsafe(no_mangle)]
pub extern "C" fn get_user_tracks(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |_| {
        Ok(liste_de(&favoris()?["pistes"], piste_de_liste))
    })
}

/// Le type de favori du cœur (`tracks`, `albums`, `artists`, `playlists`,
/// celui des routes `/streaming/{service}/favorites/{type}/{id}`) dans le
/// dialecte du service.
fn type_de_favori(v: &Value) -> Result<&'static str, Erreur> {
    match v["type"].as_str().unwrap_or("") {
        "tracks" => Ok("pistes"),
        "albums" => Ok("albums"),
        "artists" => Ok("artistes"),
        "playlists" => Ok("listes"),
        autre => Err(format!("type de favori inconnu : {autre}").into()),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn add_favorite(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |v| {
        let t = type_de_favori(&v)?;
        appeler("PUT", &format!("{}/favoris/{t}/{}", base()?, id_de(&v)), None)?;
        Ok(json!({}))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn remove_favorite(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |v| {
        let t = type_de_favori(&v)?;
        appeler(
            "DELETE",
            &format!("{}/favoris/{t}/{}", base()?, id_de(&v)),
            None,
        )?;
        Ok(json!({}))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn get_new_releases(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |_| {
        let r = appeler("GET", &format!("{}/nouveautes", base()?), None)?;
        Ok(liste_de(&r, album))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn get_featured(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |_| {
        let r = appeler("GET", &format!("{}/selections", base()?), None)?;
        Ok(liste_de(&r, playlist))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn get_album(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |v| {
        Ok(album(&fiche_album(v["id"].as_str().unwrap_or(""))?))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn get_album_tracks(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |v| {
        Ok(json!(pistes(&fiche_album(v["id"].as_str().unwrap_or(""))?)))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn get_track(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |v| {
        let id = v["id"].as_str().unwrap_or("");
        let p = appeler("GET", &format!("{}/pistes/{}", base()?, encoder(id)), None)?;
        Ok(piste(&p, &p["album"]))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn get_track_url(ptr: u32, len: u32) -> u64 {
    exporter(ptr, len, |v| {
        let id = v["id"].as_str().unwrap_or("");
        let f = appeler("GET", &format!("{}/flux/{}", base()?, encoder(id)), None)?;
        Ok(json!({
            "url": f["adresse"],
            "mime_type": "audio/mpeg",
            "quality": qualite(),
            "expires_at": f["expire_a"],
            "headers": [],
        }))
    })
}

/// Les routes HTTP du greffon. Une seule, `POST /sonde {url}`, qui demande à
/// l'hôte une adresse quelconque et rend sa réponse telle quelle : elle sert
/// au test de la liste de domaines À TRAVERS le vrai hôte.
#[unsafe(no_mangle)]
pub extern "C" fn plugin_dispatch(ptr: u32, len: u32) -> u64 {
    let requete = lire(ptr, len);
    let chemin = requete["path"].as_str().unwrap_or("").trim_matches('/');
    let reponse = if chemin == "sonde" {
        let url = requete["body"]["url"].as_str().unwrap_or("");
        json!({ "status": 200, "body": http(&json!({ "method": "GET", "url": url })) })
    } else {
        json!({ "status": 404, "body": { "error": "route inconnue" } })
    };
    rendre(&reponse)
}
