//! Native SlimProto output — plays to a Squeezelite/slim2diretta player that is
//! connected to Tune's own SlimProto server (`crate::slimproto`, port 3483).
//!
//! Unlike [`crate::outputs::squeezebox::SqueezeboxOutput`] (which drives a player
//! through an *external* LMS via its CLI), this output speaks the SlimProto wire
//! protocol directly: playback commands (`strm`/`audg`) are pushed to the
//! connected player through a per-player command channel owned by the server, and
//! status is read back from the shared player registry (updated by STAT).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::traits::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia, TransportState};
use crate::slimproto::{
    CommandChannels, FormatStrm, PlayerRegistry, ServerMessage, SlimProtoPlayback,
    build_strm_start, lock_playback, new_playback_state, strm_control,
};

#[derive(Debug, Default)]
struct CurrentMedia {
    uri: Option<String>,
    duration_ms: u64,
    title: Option<String>,
    artist: Option<String>,
}

/// A native SlimProto audio output bound to one connected player (by MAC).
pub struct SlimProtoOutput {
    name: String,
    device_id: String,
    /// MAC string key into the shared player registry and command channels.
    mac_str: String,
    /// Shared registry of connected players (position/state read from here).
    players: PlayerRegistry,
    /// Per-player command senders owned by the server (push `strm`/`audg`).
    command_channels: CommandChannels,
    /// Functional state shared with the SlimProto TCP reader.  STAT confirms
    /// playback and is the authority for decoder completion/final drain.
    playback: SlimProtoPlayback,
    /// Contract of the last accepted `PlayMedia`, which SlimProto STAT does not
    /// carry back to the server.
    current_media: Arc<Mutex<CurrentMedia>>,
    /// Last volume command accepted by the player's command channel.
    volume: Arc<Mutex<f64>>,
    muted: Arc<AtomicBool>,
}

impl SlimProtoOutput {
    pub fn new(
        name: String,
        device_id: String,
        mac_str: String,
        players: PlayerRegistry,
        command_channels: CommandChannels,
    ) -> Self {
        Self::new_with_playback(
            name,
            device_id,
            mac_str,
            players,
            command_channels,
            new_playback_state(),
        )
    }

    pub(crate) fn new_with_playback(
        name: String,
        device_id: String,
        mac_str: String,
        players: PlayerRegistry,
        command_channels: CommandChannels,
        playback: SlimProtoPlayback,
    ) -> Self {
        Self {
            name,
            device_id,
            mac_str,
            players,
            command_channels,
            playback,
            current_media: Arc::new(Mutex::new(CurrentMedia::default())),
            volume: Arc::new(Mutex::new(1.0)),
            muted: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Push a message to this player's writer task via its command channel.
    async fn send(&self, msg: ServerMessage) -> Result<(), String> {
        let tx = {
            let chans = self.command_channels.lock().await;
            chans.get(&self.mac_str).cloned()
        };
        match tx {
            Some(tx) => tx
                .send(msg)
                .await
                .map_err(|_| "slimproto player command channel closed".to_string()),
            None => Err("slimproto player not connected".to_string()),
        }
    }
}

/// Split an `http://host:port/path` stream URL into `(port, path)`. `server_ip=0`
/// in the `strm s` frame makes the player reuse its control-connection server IP,
/// so only the HTTP port and request path are needed.
fn parse_stream_url(url: &str) -> Option<(u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let slash = rest.find('/')?;
    let authority = &rest[..slash];
    let path = rest[slash..].to_string();
    let port = authority
        .rsplit(':')
        .next()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(80);
    Some((port, path))
}

#[async_trait]
impl OutputTarget for SlimProtoOutput {
    fn name(&self) -> &str {
        &self.name
    }
    fn device_id(&self) -> &str {
        &self.device_id
    }
    fn output_type(&self) -> &str {
        "slimproto"
    }

    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, false, true, true, false).with_linear_volume(65536)
    }

    /// Native SlimProto has no internal next-track staging yet (phase 3 wires
    /// `set_next_url` → `strm s` autostart). Rely on the poller's natural-end
    /// advance for now so a single-track Repeat queue still loops.
    fn supports_internal_gapless(&self) -> bool {
        false
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        let (port, path) = parse_stream_url(media.url)
            .ok_or_else(|| format!("slimproto: unparseable stream url {}", media.url))?;
        // #6475 — annoncer le codec réellement servi : un WAV annoncé FLAC
        // reste muet sur Squeezelite. Un flux qu'on ne sait pas nommer garde
        // l'ancienne annonce FLAC, mais le journal le dit.
        let format = FormatStrm::pour_le_flux(
            media.mime_type,
            media.url,
            media.sample_rate,
            media.bit_depth,
            media.channels,
        )
        .unwrap_or_else(|| {
            tracing::warn!(
                device_id = %self.device_id,
                mime = media.mime_type,
                url = media.url,
                "slimproto_format_inconnu_annonce_flac"
            );
            FormatStrm::FLAC
        });
        tracing::info!(
            device_id = %self.device_id,
            mime = media.mime_type,
            codec = %char::from(format.codec),
            "slimproto_strm_s_format"
        );
        self.send(build_strm_start(port, &path, format)).await?;
        {
            let mut current = self.current_media.lock().await;
            current.uri = Some(media.url.to_string());
            current.duration_ms = media.duration_ms.unwrap_or(0);
            current.title = media.title.map(str::to_string);
            current.artist = media.artist.map(str::to_string);
        }
        lock_playback(&self.playback).begin_playback();
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.send(strm_control(b'p')).await?;
        lock_playback(&self.playback).pause();
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.send(strm_control(b'u')).await?;
        lock_playback(&self.playback).resume();
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        // Best-effort: the player may already be gone; ignore a closed channel.
        let _ = self.send(strm_control(b'q')).await;
        lock_playback(&self.playback).stop();
        Ok(())
    }

    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Err("seek not supported on SlimProto".into())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        // SlimProto digital gain is fixed-point with 65536 = unity.
        let volume = volume.clamp(0.0, 1.0);
        let g = (volume * 65536.0).round() as u32;
        self.send(ServerMessage::Audg {
            left_gain: g,
            right_gain: g,
            digital_volume: 1,
        })
        .await?;
        *self.volume.lock().await = volume;
        self.muted.store(false, Ordering::Relaxed);
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let volume = *self.volume.lock().await;
        let g = if muted {
            0
        } else {
            (volume * 65536.0).round() as u32
        };
        self.send(ServerMessage::Audg {
            left_gain: g,
            right_gain: g,
            digital_volume: 1,
        })
        .await?;
        self.muted.store(muted, Ordering::Relaxed);
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let position_ms = {
            let reg = self.players.lock().await;
            reg.get(&self.mac_str).map(|p| p.elapsed_ms as u64)
        };
        let (state, ended_naturally) = {
            let playback = lock_playback(&self.playback);
            (playback.transport, playback.ended_naturally)
        };
        let (duration_ms, current_uri, track_title, track_artist) = {
            let current = self.current_media.lock().await;
            (
                current.duration_ms,
                current.uri.clone(),
                current.title.clone(),
                current.artist.clone(),
            )
        };
        let volume = *self.volume.lock().await;
        Ok(OutputStatus {
            state,
            position_ms: position_ms.unwrap_or(0),
            duration_ms,
            volume,
            muted: self.muted.load(Ordering::Relaxed),
            current_uri,
            track_title,
            track_artist,
            ended_naturally,
            // A renderer plays at 1x: keep the poller's wall-clock guards.
            realtime: true,
            // Aucune sortie hors la locale ne produit du DoP : le DSD y part
            // tel quel ou transcode, jamais empaquete dans du PCM 24 bits.
            dop_active: false,
        })
    }

    async fn is_available(&self) -> bool {
        let reg = self.players.lock().await;
        reg.contains_key(&self.mac_str)
    }

    /// Best-effort diagnostics for remote debugging of a tester's player
    /// (Sandro): last STAT event, position, bytes and staleness. Uses a
    /// non-blocking `try_lock` so it never stalls a status call.
    fn diagnostics_json(&self) -> Option<serde_json::Value> {
        let reg = self.players.try_lock().ok()?;
        let p = reg.get(&self.mac_str)?;
        let playback = lock_playback(&self.playback);
        Some(serde_json::json!({
            "mac": self.mac_str,
            "transport": match playback.transport {
                TransportState::Playing => "playing",
                TransportState::Paused => "paused",
                TransportState::Transitioning => "transitioning",
                TransportState::Stopped => "stopped",
            },
            "decoder_finished": playback.decoder_finished,
            "ended_naturally": playback.ended_naturally,
            "playback_error": playback.failure.map(|failure| failure.diagnostic()),
            "elapsed_ms": p.elapsed_ms,
            "bytes_received": p.bytes_received,
            "last_stat_event": String::from_utf8_lossy(&p.last_event).trim_end_matches('\0'),
            "last_stat_secs_ago": p.last_stat.elapsed().as_secs(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Instant;

    use super::*;
    use crate::slimproto::SlimProtoPlayer;

    #[tokio::test]
    async fn stat_pilote_le_statut_complet_et_la_fin_naturelle() {
        let mac_str = "00:11:22:33:44:55".to_string();
        let playback = new_playback_state();
        let players = Arc::new(Mutex::new(HashMap::new()));
        players.lock().await.insert(
            mac_str.clone(),
            SlimProtoPlayer {
                mac: [0x00, 0x11, 0x22, 0x33, 0x44, 0x55],
                mac_str: mac_str.clone(),
                name: "Squeezelite Test".into(),
                addr: "127.0.0.1:12345".parse().unwrap(),
                device_type: 12,
                firmware_version: 1,
                last_stat: Instant::now(),
                elapsed_ms: 0,
                bytes_received: 0,
                last_event: [0; 4],
                playback: Arc::clone(&playback),
            },
        );

        let channels = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        channels.lock().await.insert(mac_str.clone(), tx);
        let output = SlimProtoOutput::new_with_playback(
            "Squeezelite Test".into(),
            "slimproto-test".into(),
            mac_str.clone(),
            Arc::clone(&players),
            channels,
            Arc::clone(&playback),
        );

        output
            .play_media(&PlayMedia {
                url: "http://127.0.0.1:8080/audio/42",
                mime_type: "audio/flac",
                title: Some("Piste témoin"),
                artist: Some("Artiste témoin"),
                duration_ms: Some(180_000),
                ..PlayMedia::default()
            })
            .await
            .unwrap();
        rx.recv().await.expect("commande strm s");

        let pending = output.get_status().await.unwrap();
        assert_eq!(pending.state, TransportState::Transitioning);
        assert_eq!(pending.duration_ms, 180_000);
        assert_eq!(
            pending.current_uri.as_deref(),
            Some("http://127.0.0.1:8080/audio/42")
        );
        assert_eq!(pending.track_title.as_deref(), Some("Piste témoin"));
        assert_eq!(pending.track_artist.as_deref(), Some("Artiste témoin"));

        {
            let mut player = players.lock().await;
            let player = player.get_mut(&mac_str).unwrap();
            player.elapsed_ms = 12_345;
            player.last_event = *b"STMs";
            lock_playback(&player.playback).apply_stat(*b"STMs");
        }
        let playing = output.get_status().await.unwrap();
        assert_eq!(playing.state, TransportState::Playing);
        assert_eq!(playing.position_ms, 12_345);
        assert!(!playing.ended_naturally);

        lock_playback(&playback).apply_stat(*b"STMd");
        let decoded = output.get_status().await.unwrap();
        assert_eq!(decoded.state, TransportState::Playing);
        assert!(!decoded.ended_naturally);

        lock_playback(&playback).apply_stat(*b"STMu");
        let drained = output.get_status().await.unwrap();
        assert_eq!(drained.state, TransportState::Stopped);
        assert!(drained.ended_naturally);
    }

    /// Sortie branchée sur un faux lecteur : rend la sortie et le récepteur
    /// des commandes que le lecteur recevrait.
    async fn sortie_de_banc() -> (SlimProtoOutput, tokio::sync::mpsc::Receiver<ServerMessage>) {
        let mac_str = "a0:36:9f:00:64:75".to_string();
        let players = Arc::new(Mutex::new(HashMap::new()));
        let channels = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        channels.lock().await.insert(mac_str.clone(), tx);
        let output = SlimProtoOutput::new(
            "Squeezelite AudioLinux".into(),
            format!("slimproto-{mac_str}"),
            mac_str,
            players,
            channels,
        );
        (output, rx)
    }

    /// Les six premiers octets d'un `strm s` : autostart, codec, puis
    /// taille, fréquence, canaux, boutisme.
    fn entete_strm_s(msg: ServerMessage) -> [u8; 6] {
        match msg {
            ServerMessage::Strm { command, payload } => {
                assert_eq!(command, b's', "une lecture part en strm s");
                payload[..6].try_into().unwrap()
            }
            other => panic!("strm s attendu, reçu {other:?}"),
        }
    }

    /// #6475 (fil 2322) — LE défaut : une piste WAV locale (44,1 kHz,
    /// 16 bits, stéréo) était annoncée `f` (FLAC) à Squeezelite, qui rejetait
    /// chaque trame : 59,8 Mo aspirés, position figée à 0. Elle doit partir
    /// en `p` (PCM) avec ses paramètres, petit-boutiste comme tout WAV.
    #[tokio::test]
    async fn une_piste_wav_part_en_pcm_et_non_en_flac() {
        let (output, mut rx) = sortie_de_banc().await;
        output
            .play_media(&PlayMedia {
                url: "http://192.168.1.10:8888/stream/35e7a2df.wav",
                mime_type: "audio/wav",
                sample_rate: Some(44_100),
                bit_depth: Some(16),
                channels: Some(2),
                ..PlayMedia::default()
            })
            .await
            .unwrap();
        let entete = entete_strm_s(rx.recv().await.expect("strm s"));
        assert_eq!(
            entete,
            [b'1', b'p', b'1', b'3', b'2', b'1'],
            "WAV 44,1/16 stéréo : codec p, taille '1' (16 bits), fréquence '3' \
             (44 100), 2 canaux, petit-boutiste '1'"
        );
    }

    /// Le codec annoncé suit le flux servi, pour chaque format que Squeezelite
    /// sait décoder ; le FLAC garde son annonce d'avant.
    #[test]
    fn le_codec_annonce_suit_le_format_servi() {
        let f = |mime: &str, url: &str, sr, bd, ch| FormatStrm::pour_le_flux(mime, url, sr, bd, ch);
        let codec = |m: &str, url: &str| f(m, url, None, None, None).map(|x| x.codec);
        assert_eq!(
            f("audio/flac", "", Some(96_000), Some(24), Some(2)),
            Some(FormatStrm::FLAC)
        );
        assert_eq!(codec("audio/mpeg", ""), Some(b'm'));
        assert_eq!(codec("audio/ogg", ""), Some(b'o'));
        assert_eq!(codec("audio/opus", ""), Some(b'u'));
        assert_eq!(codec("audio/aac", ""), Some(b'a'));
        assert_eq!(codec("application/x-dsd", ""), Some(b'd'));
        assert_eq!(codec("audio/x-dsf", ""), Some(b'd'));
        assert_eq!(codec("audio/x-ms-wma", ""), Some(b'w'));
        // MP4 : une profondeur désigne l'ALAC, son absence l'AAC.
        assert_eq!(
            f("audio/mp4", "", Some(44_100), Some(16), Some(2)).map(|x| x.codec),
            Some(b'l')
        );
        assert_eq!(codec("audio/mp4", ""), Some(b'a'));
        // AIFF : PCM gros-boutiste, 24 bits / 96 kHz.
        assert_eq!(
            f("audio/aiff", "", Some(96_000), Some(24), Some(2)),
            Some(FormatStrm {
                codec: b'p',
                taille: b'2',
                frequence: b'9',
                canaux: b'2',
                boutisme: b'0'
            })
        );
        // audio/L16 porte ses paramètres dans le MIME.
        assert_eq!(
            f("audio/L16;rate=48000;channels=2", "", None, None, None),
            Some(FormatStrm {
                codec: b'p',
                taille: b'1',
                frequence: b'4',
                canaux: b'2',
                boutisme: b'0'
            })
        );
        // Paramètres inconnus : '?', Squeezelite lit l'en-tête RIFF.
        assert_eq!(
            f("audio/wav", "", None, None, None),
            Some(FormatStrm {
                codec: b'p',
                taille: b'?',
                frequence: b'?',
                canaux: b'?',
                boutisme: b'1'
            })
        );
        // 176,4 kHz : rang 11 de la table, soit ';' (88,2 kHz est ':').
        assert_eq!(
            f("audio/wav", "", Some(176_400), Some(24), Some(2)).map(|x| x.frequence),
            Some(b';')
        );
        // MIME absent : l'extension de l'URL du flux décide.
        assert_eq!(codec("", "http://h:1/stream/x.wav?t=1"), Some(b'p'));
        assert_eq!(codec("", "http://h:1/stream/x.mp3"), Some(b'm'));
        assert_eq!(
            codec("application/octet-stream", "http://h:1/stream/x"),
            None
        );
    }
}
