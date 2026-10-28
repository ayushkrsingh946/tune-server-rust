//! Un lecteur DLNA simulé annonce ses formats, reçoit le DIDL puis télécharge
//! le flux par le vrai serveur HTTP. Aucune sortie audio ni découverte réseau.
use axum::{extract::State, http::HeaderMap, routing::post};
use std::sync::Arc;
use tokio::sync::Mutex;
use tune_core::{
    audio::{decode::decode_to_pcm, encoder::AudioEncoder},
    db::{backend::DbBackend, sqlite::SqliteDb, zone_repo::ZoneRepo},
    http::streamer::AudioStreamer,
    orchestrator::{PlayRequest, PlaybackOrchestrator},
    outputs::{dlna::DlnaOutput, registry::OutputRegistry},
    playback::PlaybackManager,
    streaming::registry::ServiceRegistry,
};

#[derive(Clone)]
struct Renderer {
    sink: String,
    reception: Arc<Mutex<Option<Reception>>>,
}

struct Reception {
    didl: String,
    octets: Vec<u8>,
}

fn balise(xml: &str, nom: &str) -> String {
    xml.split_once(&format!("<{nom}>"))
        .unwrap()
        .1
        .split_once(&format!("</{nom}>"))
        .unwrap()
        .0
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

async fn soap(State(etat): State<Renderer>, entetes: HeaderMap, corps: String) -> String {
    let action = entetes
        .get("soapaction")
        .unwrap()
        .to_str()
        .unwrap()
        .trim_matches('"')
        .rsplit('#')
        .next()
        .unwrap();
    let resultat = match action {
        "GetProtocolInfo" => format!("<Source></Source><Sink>{}</Sink>", etat.sink),
        "GetTransportInfo" => "<CurrentTransportState>STOPPED</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>".into(),
        "SetAVTransportURI" => {
            let url = balise(&corps, "CurrentURI");
            assert!(url.starts_with("http://127.0.0.1:"), "le banc reste sur la boucle locale : {url}");
            let didl = balise(&corps, "CurrentURIMetaData");
            let octets = reqwest::get(url).await.unwrap().error_for_status().unwrap()
                .bytes().await.unwrap().to_vec();
            *etat.reception.lock().await = Some(Reception { didl, octets });
            String::new()
        }
        _ => String::new(),
    };
    format!(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:{action}Response xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">{resultat}</u:{action}Response></s:Body></s:Envelope>"
    )
}

struct Serveur(tokio::task::JoinHandle<()>);
impl Drop for Serveur {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn servir(ecoute: tokio::net::TcpListener, routeur: axum::Router) -> Serveur {
    Serveur(tokio::spawn(async move {
        axum::serve(ecoute, routeur).await.unwrap();
    }))
}

async fn essai(sink: &str, attendus: u16) {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("six-canaux.flac");
    // Six signaux distincts : un canal perdu, dupliqué ou réordonné se voit.
    let pcm: Vec<u8> = (0_i32..9600)
        .flat_map(|i| {
            (0..6).flat_map(move |c| {
                let valeur = (((i * (c + 1)) % 997) - 498) * 1000;
                let octets = valeur.to_le_bytes();
                [octets[0], octets[1], octets[2]]
            })
        })
        .collect();
    let mut encodeur = AudioEncoder::new("flac", 96000, 24, 6);
    encodeur.start().await.unwrap();
    encodeur.write(&pcm).await.unwrap();
    let original = encodeur.finish().await.unwrap();
    std::fs::write(&source, &original).unwrap();

    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    db.execute("INSERT INTO artists (id, name) VALUES (1, 'Essai')", &[])
        .unwrap();
    db.execute(
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Six voies', 1)",
        &[],
    )
    .unwrap();
    db.execute("INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, duration_ms, sample_rate, bit_depth, channels) VALUES (1, 'Six voies', 1, 1, ?, 'flac', 100, 96000, 24, 6)", &[&source.to_str().unwrap()]).unwrap();
    let device = format!(
        "uuid:7275-{}",
        dir.path().file_name().unwrap().to_string_lossy()
    );
    let zones = ZoneRepo::with_backend(db.clone());
    let zone = zones
        .create("Lecteur simulé", Some("dlna"), Some(&device))
        .unwrap();
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let streamer = Arc::new(AudioStreamer::new(ecoute.local_addr().unwrap().port()));
    let _http = servir(ecoute, super::router(streamer.sessions_state())).await;
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    let renderer = Renderer {
        sink: sink.into(),
        reception: Arc::new(Mutex::new(None)),
    };
    let _soap = servir(
        ecoute,
        axum::Router::new()
            .route("/control", post(soap))
            .with_state(renderer.clone()),
    )
    .await;
    let controle = format!("http://{adresse}/control");
    let mut outputs = OutputRegistry::new();
    outputs.register(Box::new(DlnaOutput::new(
        "Lecteur simulé".into(),
        device.clone(),
        "127.0.0.1".into(),
        controle.clone(),
        controle.clone(),
        Some(controle),
    )));
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        streamer.clone(),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(outputs)),
        Some("127.0.0.1".into()),
    );
    // Réchauffe volontairement le cache avec un AUTRE codec : sa limite ne
    // doit pas contaminer le FLAC qui suit sur le même appareil.
    assert_eq!(
        orch.canaux_annonces_du_renderer(&device, "audio/L16").await,
        Some(2)
    );
    let resultat = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        orch.play(PlayRequest {
            zone_id: zone,
            output_device_id: Some(device),
            track_id: Some(1),
            source: Some("local".into()),
            ..Default::default()
        }),
    )
    .await
    .expect("la lecture simulée doit aboutir")
    .expect("lecture acceptée");
    let Reception { didl, octets: recu } = renderer
        .reception
        .lock()
        .await
        .take()
        .expect("le lecteur a reçu le flux");
    assert!(
        didl.contains(&format!("nrAudioChannels=\"{attendus}\"")),
        "DIDL incohérent : {didl}"
    );
    assert!(
        didl.contains("audio/flac"),
        "le format servi reste le FLAC : {didl}"
    );
    let recu_path = dir.path().join("recu.flac");
    std::fs::write(&recu_path, &recu).unwrap();
    let decode = decode_to_pcm(recu_path.to_str().unwrap(), None, None, 0.0, 0.0).unwrap();
    assert_eq!(
        decode.channels,
        u32::from(attendus),
        "les canaux reçus doivent suivre le FLAC annoncé, pas le LPCM"
    );
    assert_eq!((decode.sample_rate, decode.bit_depth), (96000, 24));
    let ids: Vec<String> = streamer
        .sessions_state()
        .lock()
        .await
        .keys()
        .cloned()
        .collect();
    assert_eq!(ids.len(), 1);
    let fil = streamer.stream_output_wire(&ids[0]).await.unwrap();
    assert_eq!(
        u32::from(fil.channels),
        decode.channels,
        "le chemin du signal décrit les canaux reçus"
    );
    if attendus == 6 {
        assert_eq!(
            recu, original,
            "les six canaux doivent passer sans transcodage"
        );
        assert_eq!(decode.pcm_bytes(), pcm, "aucune voie perdue ou réordonnée");
    } else {
        let libelle = tune_core::audio::canaux_reseau_4573::etiquette_de_reduction_reseau(
            6,
            fil.channels,
            None,
        );
        assert_eq!(
            libelle.as_deref(),
            Some("6 → 2 canaux (annoncés par le lecteur)")
        );
    }
    assert!(
        resultat.output_sent,
        "la sortie doit accepter la lecture : {resultat:?}"
    );
    assert!(resultat.error.is_none(), "{resultat:?}");
    orch.stop(zone, None).await;
    for id in ids {
        streamer.remove_session(&id).await;
    }
}

#[tokio::test]
async fn multicanal_7275_flac_six_canaux_recu_intact() {
    essai(
        "http-get:*:audio/L16;channels=2:*,http-get:*:audio/flac;channels=6:*",
        6,
    )
    .await;
}

#[tokio::test]
async fn multicanal_7275_flac_inconnu_ne_reprend_pas_la_limite_lpcm() {
    // Partie utile de l'annonce DMP-A6 du rapport ; le reste est sans lien
    // avec le FLAC et les entrées après la borne du journal restent inconnues.
    essai("http-get:*:audio/L16;rate=44100;channels=1:*,http-get:*:audio/L16;rate=48000;channels=2:*,http-get:*:audio/x-flac:*,http-get:*:audio/flac:*,http-get:*:audio/mp4:DLNA.ORG_PN=AAC_MULT5_ISO", 6).await;
}

#[tokio::test]
async fn multicanal_7275_stereo_repli_explicite_malgre_lpcm_huit_canaux() {
    essai("http-get:*:audio/L16;channels=2:*,http-get:*:audio/L24;channels=8:*,http-get:*:audio/flac;channels=2:*", 2).await;
}
