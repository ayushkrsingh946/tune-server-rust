//! #6335 — la façade MediaRenderer d'une zone confie la suivante à
//! l'enchaînement sans blanc de la zone, au lieu de la relancer.
//!
//! Le cas de terrain (Tades, fil 2263) : un Tune Windows pilote la zone
//! « DDC-0 C19 » d'un Tune OS par sa façade UPnP ; derrière, un Diretta
//! Renderer tire le flux. Le Tune Windows arme sa suivante
//! (`SetNextAVTransportURI`, acceptée et tenue), puis demande `Next` à la fin.
//! La façade promouvait alors la suivante par un `orchestrator.play` complet :
//! nouvelle résolution, `SetAVTransportURI` + `Play` vers le Diretta, qui
//! repart de zéro — un blanc à chaque piste. Le watcher de fin de piste
//! faisait de même, au sondage de 2 s.
//!
//! Ici, comme pour #5304, les deux moitiés de production se parlent par
//! HTTP : le `DlnaOutput` du contrôleur envoie ses requêtes SOAP au routeur
//! de production de la façade. Seule la sortie audio de la zone est factice
//! (`MockOutput`) : aucun son. L'enchaînement de la zone est celui du
//! sondeur : sa décision d'armement (`prochaine_position_jouable`, la ligne
//! que `prepare_gapless` résout et arme) puis l'avance qu'il exécute à la
//! transition constatée (`advance_queue_metadata`).
use super::*;
use std::sync::atomic::{AtomicI64, Ordering};
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::outputs::dlna::DlnaOutput;
use tune_core::outputs::{OutputTarget, PlayMedia};
use tune_core::playback::PlayState;
use tune_core::poller::PositionPoller;

const COURANTE: &str = "http://127.0.0.1:9/stream/courante-6335.wav";
/// Le flux que le Tune Windows arme ~30 s avant la fin (`94a9f1ca…`).
const ARMEE: &str = "http://127.0.0.1:9/stream/armee-6335.wav";
const DUREE_MS: i64 = 69_625;

struct Banc {
    state: AppState,
    zone: i64,
    device: String,
    controleur: DlnaOutput,
    serveur: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

impl Drop for Banc {
    fn drop(&mut self) {
        self.serveur.abort();
    }
}

impl Banc {
    async fn new() -> Self {
        // `sessions()` est une statique de processus : une plage d'ids à nous.
        static ZONE: AtomicI64 = AtomicI64::new(6_335_000);
        let zone = ZONE.fetch_add(1, Ordering::Relaxed);
        let device = format!("mock-6335-{zone}");
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("facade.db");
        let config = crate::config::TuneConfig {
            db_path: db.to_str().unwrap().to_owned(),
            ..Default::default()
        };
        let state = AppState::new(db.to_str().unwrap(), 0, config).unwrap();
        state
            .backend
            .execute(
                "INSERT INTO zones (id,name,output_type,output_device_id) VALUES (?1,?2,?3,?4)",
                &[&zone, &"DDC-0 C19", &"mock", &device],
            )
            .unwrap();
        SettingsRepo::with_backend(state.backend.clone())
            .set(&format!("zone_{zone}_upnp_renderer"), "true")
            .unwrap();
        state
            .outputs
            .lock()
            .await
            .register(Box::new(tune_core::outputs::mock::MockOutput::new(
                &device,
                "Diretta (factice)",
            )));
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hote = format!("http://{}", ecoute.local_addr().unwrap());
        let app = crate::routes::router(state.clone());
        let serveur = tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
        let base = format!("{hote}{}/{zone}", upnp_renderer::RENDERER_MOUNT);
        let controleur = DlnaOutput::new(
            "DDC-0 C19 (Tune)".into(),
            format!("uuid:6335-{zone}"),
            hote.clone(),
            format!("{base}/AVTransport/control"),
            format!("{base}/RenderingControl/control"),
            None,
        );
        Self {
            state,
            zone,
            device,
            controleur,
            serveur,
            _dir: dir,
        }
    }

    /// Les lectures lancées sur la sortie de la zone : chacune est un arrêt
    /// suivi d'une relance chez le renderer réel.
    async fn lectures(&self) -> usize {
        let outputs = self.state.outputs.lock().await;
        let output = outputs.get(&self.device).unwrap();
        let output = output.lock().await;
        output
            .as_any()
            .downcast_ref::<tune_core::outputs::mock::MockOutput>()
            .unwrap()
            .play_call_count()
            .await
    }

    /// La courante lancée par le contrôleur, et sa suivante armée.
    async fn courante_puis_suivante(&self) {
        self.controleur
            .play_media(&media(COURANTE, "Piste 1"))
            .await
            .expect("la façade doit accepter SetAVTransportURI + Play");
        self.controleur
            .set_next_media(&media(ARMEE, "Piste 2"))
            .await
            .expect("la façade doit acquitter SetNextAVTransportURI");
    }

    fn file(&self) -> Vec<(Option<String>, Option<String>)> {
        PlayQueueRepo::with_backend(self.state.backend.clone())
            .get_ordered(self.zone)
            .unwrap()
            .into_iter()
            .map(|l| (l.source, l.source_id))
            .collect()
    }

    /// Ce que le sondeur armerait : la ligne que `prepare_gapless` résout.
    async fn ligne_que_le_sondeur_armerait(&self) -> Option<String> {
        let ps = self.state.playback.get_state(self.zone).await;
        let position =
            PositionPoller::prochaine_position_jouable(&self.state.backend, self.zone, &ps)?;
        PlayQueueRepo::with_backend(self.state.backend.clone())
            .get_at(self.zone, position)
            .ok()
            .flatten()
            .and_then(|l| l.source_id)
    }

    /// La transition sans blanc constatée par le sondeur de la zone.
    async fn la_zone_enchaine(&self) {
        let ps = self.state.playback.get_state(self.zone).await;
        let position = PositionPoller::prochaine_position_jouable(&self.state.backend, self.zone, &ps)
            .expect("la zone doit avoir une suivante à enchaîner");
        self.state
            .orchestrator
            .advance_queue_metadata(self.zone, position)
            .await
            .expect("l'avance sans blanc de la zone doit aboutir");
    }

    async fn vers_la_fin(&self, position_ms: i64) {
        self.state.playback.reset_position(self.zone, position_ms).await;
    }
}

fn media(url: &'static str, titre: &'static str) -> PlayMedia<'static> {
    PlayMedia {
        url,
        mime_type: "audio/wav",
        title: Some(titre),
        artist: Some("Tades"),
        duration_ms: Some(DUREE_MS as u64),
        ..Default::default()
    }
}

/// 🔴 #6335 — témoin principal. La suivante tenue est jouée par
/// l'enchaînement de la ZONE, à la fin de la piste : la sortie ne voit
/// aucune relance, et la façade publie la nouvelle piste.
#[tokio::test]
async fn la_suivante_tenue_est_enchainee_par_la_zone_sans_relance() {
    let b = Banc::new().await;
    b.courante_puis_suivante().await;
    let lectures_avant = b.lectures().await;
    assert_eq!(lectures_avant, 1, "une seule lecture : la courante");

    assert_eq!(
        b.file(),
        vec![
            (Some("upnp".into()), Some(COURANTE.into())),
            (Some("upnp".into()), Some(ARMEE.into())),
        ],
        "#6335 : la suivante posée par SetNextAVTransportURI doit entrer dans \
         la file de la zone, derrière la courante. Sinon personne ne la \
         précharge, et la façade la relance par un play complet — un arrêt \
         puis une relance chez le Diretta à chaque piste."
    );
    assert_eq!(
        b.ligne_que_le_sondeur_armerait().await.as_deref(),
        Some(ARMEE),
        "le sondeur de la zone doit armer la suivante du contrôleur"
    );

    // Fin réelle de la piste : la zone enchaîne d'elle-même.
    b.vers_la_fin(DUREE_MS - 300).await;
    b.la_zone_enchaine().await;

    assert_eq!(
        b.lectures().await,
        lectures_avant,
        "#6335 : l'enchaînement ne doit relancer AUCUNE lecture sur la sortie"
    );
    let vue = snapshot(&b.state, b.zone).await;
    assert_eq!(vue.transport_state, "PLAYING");
    assert_eq!(
        vue.uri, ARMEE,
        "la façade doit annoncer la suivante devenue courante (CurrentURI) : \
         c'est ce que le contrôleur surveille pour adopter son flux armé"
    );
    assert!(vue.next_uri.is_empty(), "la suivante consommée ne doit plus être publiée");
    assert!(
        vue.position_ms < 1_000,
        "la position doit repartir pour la nouvelle piste, pas rester à la \
         fin de l'ancienne : {} ms",
        vue.position_ms
    );
    let media = b.controleur.media_du_transport().await.unwrap();
    assert_eq!(media.courante.as_deref(), Some(ARMEE));
    assert_eq!(media.suivante, None);

    // Le watcher de la façade passe : il ne doit rien relancer non plus.
    tokio::time::sleep(std::time::Duration::from_millis(2_500)).await;
    assert_eq!(
        b.lectures().await,
        lectures_avant,
        "#6335 : le watcher de 2 s a relancé la piste que la zone jouait déjà"
    );

    // Le `Next` du contrôleur, parti à SON horloge après la fin : acquitté,
    // rien à relancer.
    b.controleur
        .basculer_sur_la_suivante_preparee()
        .await
        .expect("un Next juste après l'enchaînement de la zone doit être acquitté");
    assert_eq!(b.lectures().await, lectures_avant);
    assert_eq!(snapshot(&b.state, b.zone).await.uri, ARMEE);
}

/// #6335 — le cas exact du journal : `Next` part quand la zone a encore
/// 1,6 s à jouer. Il ne doit pas couper cette fin ni relancer.
#[tokio::test]
async fn un_next_juste_avant_la_fin_laisse_la_zone_enchainer() {
    let b = Banc::new().await;
    b.courante_puis_suivante().await;
    let lectures_avant = b.lectures().await;
    b.vers_la_fin(68_000).await;

    b.controleur
        .basculer_sur_la_suivante_preparee()
        .await
        .expect("Next en fin de piste doit être acquitté");
    assert_eq!(
        b.lectures().await,
        lectures_avant,
        "#6335 : un Next à 1,6 s de la fin a relancé la suivante par-dessus \
         la fin de la courante"
    );
    let vue = snapshot(&b.state, b.zone).await;
    assert_eq!(vue.uri, COURANTE, "la courante finit de jouer");
    assert_eq!(vue.next_uri, ARMEE, "la suivante reste tenue jusqu'à l'enchaînement");

    b.la_zone_enchaine().await;
    assert_eq!(b.lectures().await, lectures_avant);
    assert_eq!(snapshot(&b.state, b.zone).await.uri, ARMEE);
}

/// #6335 — un `Next` en milieu de piste est un saut voulu : la suivante part
/// tout de suite (comme avant), et le curseur de la file la suit — la fin de
/// cette piste ne doit pas la rejouer.
#[tokio::test]
async fn un_next_en_milieu_de_piste_saute_tout_de_suite() {
    let b = Banc::new().await;
    b.courante_puis_suivante().await;
    let lectures_avant = b.lectures().await;
    b.vers_la_fin(20_000).await;

    b.controleur
        .basculer_sur_la_suivante_preparee()
        .await
        .expect("Next en milieu de piste doit être acquitté");
    assert_eq!(b.lectures().await, lectures_avant + 1, "le saut lance la suivante");
    assert_eq!(snapshot(&b.state, b.zone).await.uri, ARMEE);
    let ps = b.state.playback.get_state(b.zone).await;
    assert_eq!(
        PositionPoller::next_position(&ps),
        None,
        "après le saut, la file ne doit plus rien avoir à enchaîner : la fin \
         de la suivante la rejouerait (curseur {} / {})",
        ps.queue_position,
        ps.queue_length
    );
}

/// #6335 — sans `SetNextAVTransportURI`, rien ne change : la file de la zone
/// n'est pas touchée, et `Stop` ou une nouvelle URI ne retirent que la file
/// que la façade a écrite.
#[tokio::test]
async fn sans_suivante_la_file_de_la_zone_n_est_pas_touchee() {
    use tune_core::db::play_queue_repo::StreamingQueueItem;
    let b = Banc::new().await;
    let ligne = |uri: &str| -> StreamingQueueItem {
        (uri.into(), "x".into(), String::new(), None, None, 0, Some("qobuz".into()), None, None)
    };
    PlayQueueRepo::with_backend(b.state.backend.clone())
        .set_streaming_queue(b.zone, &[ligne("q1"), ligne("q2"), ligne("q3")])
        .unwrap();
    let file_de_l_auditeur = b.file();

    b.controleur
        .play_media(&media(COURANTE, "Piste 1"))
        .await
        .expect("SetAVTransportURI + Play");
    assert_eq!(b.lectures().await, 1);
    assert_eq!(b.file(), file_de_l_auditeur, "SetURI + Play ne touche pas la file");
    assert!(snapshot(&b.state, b.zone).await.next_uri.is_empty());

    // Stop sur une file qui n'est pas la nôtre : elle reste.
    b.controleur.stop().await.ok();
    assert_eq!(b.file(), file_de_l_auditeur, "Stop ne retire pas la file de Tune");

    // Avec une suivante, puis un Stop : la file écrite par la façade part.
    b.courante_puis_suivante().await;
    assert_eq!(b.file().len(), 2);
    b.controleur.stop().await.ok();
    assert!(
        b.file().is_empty(),
        "#6335 : après un Stop, la suivante effacée doit sortir de la file — \
         un Play de la courante l'enchaînerait sinon"
    );
    assert_eq!(b.state.playback.get_state(b.zone).await.state, PlayState::Stopped);
}

/// La décision du `Next`, cas par cas.
#[test]
fn la_decision_du_next() {
    use DecisionNext::*;
    let joue = PlayState::Playing;
    assert_eq!(decider_le_next(None, true, true, joue, 0, 0), DejaEnchainee);
    assert_eq!(decider_le_next(None, false, true, joue, 0, 0), Enchainer);
    assert_eq!(decider_le_next(Some(true), false, true, joue, 68_000, DUREE_MS), LaZoneEnchaine);
    assert_eq!(decider_le_next(Some(true), false, true, joue, 20_000, DUREE_MS), Enchainer);
    // Suivante non confiée (répétition, aléatoire) : la façade enchaîne.
    assert_eq!(decider_le_next(Some(false), false, true, joue, 68_000, DUREE_MS), Enchainer);
    // Zone reprise par Tune, arrêtée ou durée inconnue : pas d'attente.
    assert_eq!(decider_le_next(Some(true), false, false, joue, 68_000, DUREE_MS), Enchainer);
    assert_eq!(
        decider_le_next(Some(true), false, true, PlayState::Stopped, 68_000, DUREE_MS),
        Enchainer
    );
    assert_eq!(decider_le_next(Some(true), false, true, joue, 68_000, 0), Enchainer);
}
