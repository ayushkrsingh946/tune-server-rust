//! #6132 — la sortie double branchée sur le chemin partagé cpal.
//!
//! La zone joue sur son périphérique (A) comme d'habitude. Quand une seconde
//! sortie est réglée (`zone_{id}_sortie_double`), le fil de lecture ouvre en
//! plus le périphérique B, en mode partagé, et installe un [`Recopieur`] dans
//! l'emplacement que le rappel de A consulte à chaque période : ce que A joue
//! part aussi vers B, l'horloge de B asservie à celle de A
//! (`outputs::sortie_double`).
//!
//! Limites de cette étape, dites dans le journal plutôt que tues :
//!
//! - **ASIO** : un seul flux dans tout le processus (`ASIO_DEVICE_LOCK`) ;
//!   la seconde sortie est refusée, la zone joue sur A seule ;
//! - **mode exclusif** (WASAPI, CoreAudio) et format entier : la seconde
//!   sortie n'est ouverte que sur le chemin partagé en `f32` ;
//! - **« bit-perfect strict »** : B est rééchantillonné (de quelques ppm) ;
//!   une zone qui interdit toute conversion ne l'ouvre pas ;
//! - B doit accepter le même nombre de canaux que A.

use super::*;
use crate::outputs::sortie_double::{AnneauSecondaire, CONSIGNE_PAR_DEFAUT_S, Recopieur};
use std::sync::atomic::AtomicUsize;

/// L'anneau SPSC de B, et la période que son rappel a lue en dernier.
pub(super) struct AnneauDeB {
    anneau: RingBuf,
    periode: AtomicUsize,
}

impl AnneauSecondaire for AnneauDeB {
    fn pousser(&self, echantillons: &[f32]) -> usize {
        self.anneau.push(echantillons)
    }
    fn en_attente(&self) -> usize {
        self.anneau.available()
    }
    fn periode_du_lecteur(&self) -> usize {
        self.periode.load(Ordering::Relaxed)
    }
}

pub(super) struct Recopie {
    recopieur: Recopieur,
    anneau: Arc<AnneauDeB>,
}

/// Ce que le rappel de A consulte à chaque période. Vide : A joue seul,
/// exactement comme avant.
pub(super) type EmplacementDeRecopie = Arc<std::sync::Mutex<Option<Recopie>>>;

pub(super) fn emplacement_vide() -> EmplacementDeRecopie {
    Arc::new(std::sync::Mutex::new(None))
}

/// Dans le rappel de A, après le rendu (volume, rampe et silence compris) :
/// recopier la période vers B, et la remplacer par sa version retardée.
///
/// `try_lock` : le rappel temps réel n'attend jamais. Le verrou n'est pris
/// ailleurs qu'à l'installation et au retrait de la recopie.
pub(super) fn recopier_la_periode(emplacement: &EmplacementDeRecopie, data: &mut [f32]) {
    if let Ok(mut garde) = emplacement.try_lock()
        && let Some(recopie) = garde.as_mut()
    {
        recopie.recopieur.recopier(data, &*recopie.anneau);
    }
}

/// Pourquoi la seconde sortie n'a pas été ouverte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefusDeSecondeSortie {
    /// ASIO ne tient qu'un flux par machine.
    Asio,
    /// La zone est en « bit-perfect strict » : B serait rééchantillonné.
    BitPerfectStrict,
    /// A n'a pas été ouvert en `f32` partagé (cascade entière).
    FormatDeA,
    /// La seconde sortie est le périphérique même de A.
    MemePeripherique,
    /// Aucun périphérique de ce nom sur l'hôte ouvert.
    Introuvable,
    /// B n'offre pas le nombre de canaux de A.
    Canaux { a: u16, b: u16 },
    /// cpal a refusé d'ouvrir ou de démarrer B.
    Ouverture(String),
}

impl RefusDeSecondeSortie {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Asio => "asio_un_seul_flux",
            Self::BitPerfectStrict => "bitperfect_strict",
            Self::FormatDeA => "format_de_a",
            Self::MemePeripherique => "meme_peripherique",
            Self::Introuvable => "introuvable",
            Self::Canaux { .. } => "canaux",
            Self::Ouverture(_) => "ouverture",
        }
    }
}

/// La décision qui ne demande pas cpal : peut-on même essayer ?
pub(crate) fn refus_prealable(
    hote: &str,
    strict_bitperfect: bool,
    a_en_f32: bool,
    nom_a: &str,
    nom_b: &str,
) -> Option<RefusDeSecondeSortie> {
    if hote.eq_ignore_ascii_case("asio") {
        return Some(RefusDeSecondeSortie::Asio);
    }
    if strict_bitperfect {
        return Some(RefusDeSecondeSortie::BitPerfectStrict);
    }
    if !a_en_f32 {
        return Some(RefusDeSecondeSortie::FormatDeA);
    }
    if nom_a.trim().eq_ignore_ascii_case(nom_b.trim()) {
        return Some(RefusDeSecondeSortie::MemePeripherique);
    }
    None
}

/// B ouvert et en marche. Le garder vivant garde B qui joue ; le lâcher
/// retire la recopie de l'emplacement de A, puis ferme B.
pub(super) struct SecondeSortie {
    emplacement: EmplacementDeRecopie,
    nom: String,
    // Dernier champ : fermé APRÈS le retrait de la recopie (ordre de `Drop`).
    _flux: cpal::Stream,
}

impl Drop for SecondeSortie {
    fn drop(&mut self) {
        let recopie = self
            .emplacement
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(recopie) = recopie {
            let releve = recopie.recopieur.releve();
            info!(
                seconde_sortie = %self.nom,
                ecart_ms = releve.ecart_ms,
                derive_ppm = releve.derive_ppm,
                correction_ppm = releve.correction_ppm,
                echantillons_refuses = releve.echantillons_refuses,
                trames_produites = releve.trames_produites,
                "sortie_double_releve"
            );
        }
    }
}

/// Le périphérique `nom` sur `host`, SANS repli et sans rien noter pour
/// l'écran : l'observation du périphérique ouvert est celle de A.
fn trouver_sans_repli(host: &cpal::Host, nom: &str) -> Option<cpal::Device> {
    let open_host: &'static str = host.id().name();
    let (devices, identities): (Vec<cpal::Device>, Vec<DeviceIdentity>) = host
        .output_devices()
        .map(|devs| devs.collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|device| {
            let identity = DeviceIdentity {
                endpoint_id: device.id().map(|id| id.to_string()).unwrap_or_default(),
                raw_name: device
                    .description()
                    .map(|desc| desc.name().to_string())
                    .unwrap_or_else(|_| "Unknown".into()),
                host: open_host.to_string(),
            };
            (device, identity)
        })
        .filter(|(_, identity)| !is_null_sink(&identity.endpoint_id, &identity.raw_name))
        .unzip();
    match resolve_device(nom, None, None, open_host, &identities) {
        DeviceResolution::Matched(m) => devices.into_iter().nth(m.index()),
        _ => None,
    }
}

/// Ouvrir B à la cadence de A si B l'accepte, sinon à sa cadence par défaut
/// (le recopieur convertit), installer la recopie, démarrer B.
pub(super) fn ouvrir_la_seconde_sortie(
    host: &cpal::Host,
    nom: &str,
    cadence_a: u32,
    canaux: u16,
    emplacement: &EmplacementDeRecopie,
) -> Result<SecondeSortie, RefusDeSecondeSortie> {
    let device = trouver_sans_repli(host, nom).ok_or(RefusDeSecondeSortie::Introuvable)?;
    let config = match find_matching_config(&device, canaux, cadence_a) {
        Some(c) if c.channels == canaux => avec_periode(c),
        _ => {
            let defaut = device
                .default_output_config()
                .map_err(|e| RefusDeSecondeSortie::Ouverture(e.to_string()))?;
            if defaut.channels() != canaux {
                return Err(RefusDeSecondeSortie::Canaux {
                    a: canaux,
                    b: defaut.channels(),
                });
            }
            config_de_flux(canaux, defaut.sample_rate())
        }
    };
    let cadence_b = config.sample_rate;
    let recopieur = Recopieur::nouveau(
        usize::from(canaux),
        cadence_a,
        cadence_b,
        CONSIGNE_PAR_DEFAUT_S,
    );
    let consigne = recopieur.consigne();
    let capacite = ((CONSIGNE_PAR_DEFAUT_S * 4.0 * f64::from(cadence_b)) as usize).max(16_384)
        * usize::from(canaux);
    let anneau = Arc::new(AnneauDeB {
        anneau: RingBuf::new(capacite),
        periode: AtomicUsize::new(0),
    });
    let anneau_cb = anneau.clone();
    let mut demarre = false;
    let nom_erreur = nom.to_string();
    let flux = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                if !demarre {
                    if anneau_cb.anneau.available() < consigne.seuil_de_depart(data.len()) {
                        data.fill(0.0);
                        return;
                    }
                    demarre = true;
                }
                let lu = anneau_cb.anneau.pop(data);
                anneau_cb.periode.store(data.len(), Ordering::Relaxed);
                data[lu..].fill(0.0);
            },
            move |e: cpal::StreamError| {
                warn!(seconde_sortie = %nom_erreur, erreur = %e, "sortie_double_erreur_de_flux");
            },
            None,
        )
        .map_err(|e| RefusDeSecondeSortie::Ouverture(e.to_string()))?;
    *emplacement.lock().unwrap_or_else(|e| e.into_inner()) = Some(Recopie { recopieur, anneau });
    if let Err(e) = flux.play() {
        emplacement.lock().unwrap_or_else(|e| e.into_inner()).take();
        return Err(RefusDeSecondeSortie::Ouverture(e.to_string()));
    }
    info!(
        seconde_sortie = %nom,
        cadence_a,
        cadence_b,
        canaux,
        consigne_ms = CONSIGNE_PAR_DEFAUT_S * 1000.0,
        "sortie_double_ouverte"
    );
    Ok(SecondeSortie {
        emplacement: emplacement.clone(),
        nom: nom.to_string(),
        _flux: flux,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asio_strict_et_format_entier_refusent_la_seconde_sortie() {
        assert_eq!(
            refus_prealable("Asio", false, true, "A", "B"),
            Some(RefusDeSecondeSortie::Asio)
        );
        assert_eq!(
            refus_prealable("Wasapi", true, true, "A", "B"),
            Some(RefusDeSecondeSortie::BitPerfectStrict)
        );
        assert_eq!(
            refus_prealable("Wasapi", false, false, "A", "B"),
            Some(RefusDeSecondeSortie::FormatDeA)
        );
        assert_eq!(
            refus_prealable("Alsa", false, true, "DAC", " dac "),
            Some(RefusDeSecondeSortie::MemePeripherique)
        );
        assert_eq!(
            refus_prealable("Wasapi", false, true, "DAC", "Interface"),
            None
        );
    }

    /// Le chemin réel : le rappel de A et le rappel de B tels que la sortie
    /// locale les monte, mais appelés à la main sur un `RingBuf` réel.
    #[test]
    fn le_rappel_de_a_recopie_vers_l_anneau_de_b() {
        let emplacement = emplacement_vide();
        let mut periode = vec![0.25f32; 960];
        // Vide : A joue seul, rien ne change.
        recopier_la_periode(&emplacement, &mut periode);
        assert!(periode.iter().all(|s| *s == 0.25));

        let anneau = Arc::new(AnneauDeB {
            anneau: RingBuf::new(48_000),
            periode: AtomicUsize::new(0),
        });
        *emplacement.lock().unwrap() = Some(Recopie {
            recopieur: Recopieur::nouveau(2, 48_000, 48_000, 0.010),
            anneau: anneau.clone(),
        });
        recopier_la_periode(&emplacement, &mut periode);
        // A est retardé de 10 ms (480 trames) : sa première période est muette.
        assert!(periode.iter().all(|s| *s == 0.0));
        // B a reçu la période, au filtre près.
        assert!(anneau.en_attente() >= 2 * (480 - 32));
    }
}
