//! #4201, phase 3 du chantier `unifier-serveurs-upnp-et-bibliotheque` — une
//! piste de serveur multimédia décodée en WAV par Tune, à la position
//! demandée, vers une sortie locale (après un saut) ou OAAT.
//!
//! Avant : la sortie locale recevait l'URL distante et la décodait depuis le
//! début, si bien qu'un saut à 5 s relançait la piste ; OAAT refusait toute
//! piste qui n'était pas déjà en WAV.
//!
//! Le banc : un faux serveur multimédia (HTTP/1.1, `Range` honoré) qui sert un
//! vrai FLAC stéréo de 10 s — 5 s douces puis 5 s fortes —, et une ligne
//! indexée comme l'indexation la pose (`source = 'upnp'`, `upnp_res_url`).
//! Ce que la sortie reçoit est relu et décodé : la durée servie et le niveau
//! du début disent d'où le flux est parti.
//!
//! Contre-épreuve : faire rendre `false` à `doit_decoder_la_piste_upnp_en_wav`
//! fait rougir les deux témoins de lecture (le saut repart de l'URL brute, et
//! OAAT retombe sur le refus).

use super::resolve_direct::{codec_d_un_mime_upnp, doit_decoder_la_piste_upnp_en_wav};
use super::*;
use crate::db::zone_repo::ZoneRepo;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CADENCE: u32 = 8_000;
const SECONDES: u32 = 10;
const DOUX: f64 = 0.05;
const FORT: f64 = 0.5;

/// Un faux serveur multimédia qui sert `corps` en `audio/flac`, `Range`
/// compris.
async fn faux_serveur(corps: Vec<u8>) -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    let corps = Arc::new(corps);
    tokio::spawn(async move {
        loop {
            let Ok((mut flux, _)) = ecoute.accept().await else {
                return;
            };
            let corps = corps.clone();
            tokio::spawn(async move {
                let mut tampon = Vec::new();
                let mut morceau = [0u8; 4096];
                while !tampon.windows(4).any(|w| w == b"\r\n\r\n") {
                    match flux.read(&mut morceau).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => tampon.extend_from_slice(&morceau[..n]),
                    }
                }
                let requete = String::from_utf8_lossy(&tampon).to_ascii_lowercase();
                let total = corps.len();
                let plage = requete
                    .lines()
                    .find_map(|l| l.strip_prefix("range: bytes="))
                    .and_then(|r| {
                        let (a, b) = r.trim().split_once('-')?;
                        let a: usize = a.parse().ok()?;
                        let b: usize = b.parse().unwrap_or(total - 1);
                        Some((a, b.min(total - 1)))
                    });
                let (statut, debut, fin) = match plage {
                    Some((a, b)) if a < total => ("206 Partial Content", a, b),
                    Some(_) => ("416 Range Not Satisfiable", 0, 0),
                    None => ("200 OK", 0, total - 1),
                };
                let tranche: &[u8] = if statut.starts_with("416") {
                    &[]
                } else {
                    &corps[debut..=fin]
                };
                let mut entete = format!(
                    "HTTP/1.1 {statut}\r\nContent-Type: audio/flac\r\nContent-Length: {}\r\n\
                     Accept-Ranges: bytes\r\n",
                    tranche.len()
                );
                if statut.starts_with("206") {
                    entete.push_str(&format!("Content-Range: bytes {debut}-{fin}/{total}\r\n"));
                }
                entete.push_str("Connection: close\r\n\r\n");
                let _ = flux.write_all(entete.as_bytes()).await;
                let _ = flux.write_all(tranche).await;
                let _ = flux.shutdown().await;
            });
        }
    });
    // Comme le `<res>` du serveur média de Tune : aucune extension.
    format!("http://{adresse}/api/v1/library/tracks/1/audio")
}

/// Un vrai FLAC stéréo : 5 s à `DOUX`, puis 5 s à `FORT`, 440 Hz.
fn flac_doux_puis_fort() -> Vec<u8> {
    let trames = (CADENCE * SECONDES) as usize;
    let mut pcm = Vec::with_capacity(trames * 4);
    for i in 0..trames {
        let amplitude = if i < trames / 2 { DOUX } else { FORT };
        let v = (2.0 * std::f64::consts::PI * 440.0 * i as f64 / CADENCE as f64).sin() * amplitude;
        let e = ((v * i16::MAX as f64) as i16).to_le_bytes();
        pcm.extend_from_slice(&e);
        pcm.extend_from_slice(&e);
    }
    let mut e = crate::audio::encoder::AudioEncoder::new("flac", CADENCE, 16, 2);
    e.start_sync().unwrap();
    e.write_sync(&pcm).unwrap();
    e.finish_sync().unwrap()
}

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

/// La ligne que l'indexation pose. `cadence` à `None` : un serveur qui n'a pas
/// publié `res@sampleFrequency`.
fn inscrire_la_piste(orch: &PlaybackOrchestrator, url: &str, cadence: Option<u32>) {
    let sr = cadence.map_or("NULL".to_string(), |c| c.to_string());
    orch.db
        .execute_batch(&format!(
            "INSERT INTO tracks (id,title,source,source_id,format,sample_rate,bit_depth,channels,duration_ms) \
             VALUES (1,'Doux puis fort','upnp','uuid:serveur|1','flac',{sr},16,2,{}); \
             INSERT INTO track_metadata (track_id,key,value) VALUES (1,'upnp_res_url','{url}');",
            SECONDES * 1000
        ))
        .unwrap();
}

fn zone(orch: &PlaybackOrchestrator, type_sortie: &str, device: &str) -> i64 {
    ZoneRepo::with_backend(orch.db.clone())
        .create("Z", Some(type_sortie), Some(device))
        .unwrap()
}

/// Ce que le bouton Lecture et la reprise envoient : le `track_id`, sans
/// `source` ni fréquence.
fn demande(zone_id: i64, device: &str, seek_ms: Option<u64>) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some(device.into()),
        track_id: Some(1),
        title: Some("Doux puis fort".into()),
        seek_ms,
        ..Default::default()
    }
}

/// Les octets du canal d'une session, jusqu'à sa fermeture.
async fn octets_de_la_session(orch: &PlaybackOrchestrator, sid: &str) -> Vec<u8> {
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(sid)
        .cloned()
        .expect("session inscrite");
    let mut octets = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(10), session.recv_chunk()).await {
            Ok(Some(c)) => octets.extend_from_slice(&c),
            Ok(None) => break,
            Err(_) => {
                assert!(!octets.is_empty(), "aucun octet servi en 10 s");
                break;
            }
        }
    }
    octets
}

/// Durée (s) et niveau RMS de la première demi-seconde du WAV servi.
fn mesurer(wav: &[u8]) -> (f64, f64) {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("servi.wav");
    std::fs::write(&chemin, wav).unwrap();
    let d = crate::audio::decode::decode_to_pcm(&chemin.to_string_lossy(), None, None, 0.0, 0.0)
        .unwrap();
    assert_eq!(d.channels, 2, "WAV stéréo attendu");
    assert_eq!(d.sample_rate, CADENCE, "la cadence de la source est gardée");
    let echelle = 2f64.powi(d.bit_depth as i32 - 1);
    let trames = d.samples_i32.len() / 2;
    let duree = trames as f64 / d.sample_rate as f64;
    let tete = (d.sample_rate as usize / 2).min(trames);
    let somme: f64 = d
        .samples_i32
        .iter()
        .step_by(2)
        .take(tete)
        .map(|&s| {
            let x = s as f64 / echelle;
            x * x
        })
        .sum();
    (duree, (somme / tete.max(1) as f64).sqrt())
}

// ── La règle, sans orchestrateur ────────────────────────────────────────

#[test]
fn la_regle_ne_decode_que_pour_oaat_ou_un_saut_en_local() {
    // OAAT : tout ce qui n'est pas déjà du WAV, et un WAV repris à une position.
    assert!(doit_decoder_la_piste_upnp_en_wav(false, true, false, None));
    assert!(doit_decoder_la_piste_upnp_en_wav(
        false,
        true,
        true,
        Some(5_000)
    ));
    assert!(
        !doit_decoder_la_piste_upnp_en_wav(false, true, true, None),
        "un WAV lu depuis le début sur OAAT passe tel quel, comme avant"
    );
    // Locale : seulement avec une position.
    assert!(doit_decoder_la_piste_upnp_en_wav(
        true,
        false,
        false,
        Some(5_000)
    ));
    assert!(
        !doit_decoder_la_piste_upnp_en_wav(true, false, false, None),
        "une lecture locale depuis le début garde le chemin de LocalOutput"
    );
    assert!(!doit_decoder_la_piste_upnp_en_wav(
        true,
        false,
        false,
        Some(0)
    ));
    // Réseau et navigateur : jamais ici.
    assert!(!doit_decoder_la_piste_upnp_en_wav(
        false,
        false,
        false,
        Some(5_000)
    ));
}

#[test]
fn le_codec_se_lit_dans_le_mime_et_le_dsd_reste_a_part() {
    assert_eq!(codec_d_un_mime_upnp("audio/flac"), Some("flac"));
    assert_eq!(codec_d_un_mime_upnp("audio/x-flac"), Some("flac"));
    assert_eq!(codec_d_un_mime_upnp("audio/mpeg"), Some("mp3"));
    assert_eq!(codec_d_un_mime_upnp("audio/mp4"), Some("m4a"));
    assert_eq!(codec_d_un_mime_upnp("audio/wav; charset=x"), Some("wav"));
    assert_eq!(codec_d_un_mime_upnp("application/x-dsd"), None);
    assert_eq!(codec_d_un_mime_upnp("audio/inconnu"), None);
}

// ── Témoins de lecture ──────────────────────────────────────────────────

/// Sortie locale, saut à 5 s : le flux servi part de 5 s (5 s restantes, la
/// moitié forte), au lieu de l'URL brute décodée depuis le début.
#[tokio::test(flavor = "multi_thread")]
async fn un_saut_en_sortie_locale_reprend_a_la_position_demandee() {
    let url = faux_serveur(flac_doux_puis_fort()).await;
    let orch = orchestrateur();
    inscrire_la_piste(&orch, &url, Some(CADENCE));
    let zid = zone(&orch, "local", "local:Casque");

    // Jumeau : depuis le début, rien ne change — l'URL distante telle quelle.
    let r = orch
        .resolve_stream(&demande(zid, "local:Casque", None))
        .await
        .unwrap();
    assert_eq!(
        r.url, url,
        "lecture depuis le début : chemin de LocalOutput"
    );
    assert!(r.stream_id.is_none());

    // Le saut.
    let r = orch
        .resolve_stream(&demande(zid, "local:Casque", Some(5_000)))
        .await
        .unwrap();
    assert_eq!(
        r.mime_type, "audio/wav",
        "url={} mime={}",
        r.url, r.mime_type
    );
    let sid = r
        .stream_id
        .clone()
        .expect("le saut doit passer par une session décodée");
    assert_eq!(r.origin_url.as_deref(), Some(url.as_str()));
    assert_eq!(r.sample_rate, Some(CADENCE));
    let (duree, niveau) = mesurer(&octets_de_la_session(&orch, &sid).await);
    println!("saut local : {duree:.2} s servies, niveau de tête {niveau:.3}");
    assert!(
        (duree - 5.0).abs() < 0.3,
        "après un saut à 5 s, 5 s doivent rester ; servies : {duree:.2} s"
    );
    let fort = FORT / std::f64::consts::SQRT_2;
    assert!(
        (niveau - fort).abs() < fort * 0.2,
        "la tête du flux doit être la moitié FORTE (RMS ≈ {fort:.3}) ; mesuré {niveau:.3}"
    );
}

/// OAAT, fréquence connue : la piste joue, en WAV, depuis le début.
#[tokio::test(flavor = "multi_thread")]
async fn oaat_joue_une_piste_flac_dont_la_frequence_est_connue() {
    let url = faux_serveur(flac_doux_puis_fort()).await;
    let orch = orchestrateur();
    inscrire_la_piste(&orch, &url, Some(CADENCE));
    let zid = zone(&orch, "oaat", "oaat:endpoint-4201");

    let r = orch
        .resolve_stream(&demande(zid, "oaat:endpoint-4201", None))
        .await
        .expect("OAAT doit jouer une piste FLAC dont la fréquence est connue");
    assert_eq!(r.mime_type, "audio/wav");
    let sid = r.stream_id.clone().expect("session WAV");
    let (duree, niveau) = mesurer(&octets_de_la_session(&orch, &sid).await);
    println!("OAAT : {duree:.2} s servies, niveau de tête {niveau:.3}");
    assert!(
        (duree - SECONDES as f64).abs() < 0.3,
        "la piste entière : {duree:.2} s servies"
    );
    let doux = DOUX / std::f64::consts::SQRT_2;
    assert!(
        (niveau - doux).abs() < doux * 0.25,
        "depuis le début, la tête est DOUCE (RMS ≈ {doux:.3}) ; mesuré {niveau:.3}"
    );
}

/// OAAT, fréquence inconnue : le refus motivé d'avant, mot pour mot — Tune ne
/// devine pas une fréquence pour dimensionner son WAV.
#[tokio::test(flavor = "multi_thread")]
async fn oaat_sans_frequence_garde_le_refus_motive() {
    let url = faux_serveur(flac_doux_puis_fort()).await;
    let orch = orchestrateur();
    inscrire_la_piste(&orch, &url, None);
    let zid = zone(&orch, "oaat", "oaat:endpoint-4201");

    let erreur = match orch
        .resolve_stream(&demande(zid, "oaat:endpoint-4201", None))
        .await
    {
        Ok(r) => panic!("refus attendu, obtenu {} ({})", r.url, r.mime_type),
        Err(e) => e.to_string(),
    };
    for phrase in [
        "un point de sortie OAAT ne lit que du PCM en conteneur WAV",
        "la piste n'a pas été lancée, elle n'aurait produit qu'un silence",
        "Elle joue en revanche sur une zone réseau, navigateur ou locale",
    ] {
        assert!(erreur.contains(phrase), "« {phrase} » absent : {erreur}");
    }
    assert!(!erreur.contains("  "), "suite d'espaces : {erreur}");
}
