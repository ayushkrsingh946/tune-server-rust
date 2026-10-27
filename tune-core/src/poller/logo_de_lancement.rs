//! #7051 — le logo avec lequel une station a été LANCÉE.
//!
//! Fil forum 2443 (Radio Bergeijk, 25/10/2026) : « le son est bon, titre et
//! artiste s'affichent », mais un fond gris à la place du logo. La station se
//! jouait depuis l'annuaire (`POST /radios/directory/{id}/play/{zone}`), sans
//! être dans « Mes radios » : la lecture partait bien avec le logo de la fiche
//! (`cover_url`), puis le sondeur, qui ne cherche le logo qu'en base
//! ([`super::station_du_now_playing`]), ne trouvait rien et publiait, au
//! premier titre reçu du flux, une vignette vide à la place du logo.
//!
//! Le logo de lancement est donc retenu ici, par adresse de flux (à la
//! normalisation près), et le sondeur s'y rabat quand la base ne lui en donne
//! aucun. Ce n'est PAS la pochette courante : le repli reste le logo de la
//! station, jamais la pochette du titre d'avant (voir
//! [`super::vignette_du_pas_radio`]).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::db::radio_repo::RadioStation;
use crate::radio_metadata_relais::flux_normalise;

/// Au-delà, la mémoire repart de zéro : quelques stations jouent à la fois,
/// et une station relancée se retient de nouveau à son lancement.
const STATIONS_RETENUES_AU_PLUS: usize = 256;

static LOGOS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn renseigne(v: Option<&str>) -> Option<&str> {
    v.map(str::trim).filter(|s| !s.is_empty())
}

/// Retient le logo avec lequel le flux `url_flux` vient d'être lancé. Un
/// lancement sans logo efface ce qui était retenu pour ce flux.
pub fn retenir_le_logo_de_lancement(url_flux: &str, logo: Option<&str>) {
    let cle = flux_normalise(url_flux);
    if cle.is_empty() {
        return;
    }
    let Ok(mut logos) = LOGOS.lock() else {
        return;
    };
    match renseigne(logo) {
        Some(logo) => {
            if logos.len() >= STATIONS_RETENUES_AU_PLUS && !logos.contains_key(&cle) {
                logos.clear();
            }
            logos.insert(cle, logo.to_string());
        }
        None => {
            logos.remove(&cle);
        }
    }
}

/// Le logo retenu au lancement du flux `url_flux`, s'il y en a un.
pub(crate) fn logo_de_lancement(url_flux: &str) -> Option<String> {
    LOGOS.lock().ok()?.get(&flux_normalise(url_flux)).cloned()
}

/// Le logo de la station qui joue : celui de « Mes radios » quand elle y est
/// et en a un, sinon celui avec lequel elle a été lancée.
pub(crate) fn logo_de_la_station(station: Option<&RadioStation>, url_flux: &str) -> Option<String> {
    renseigne(station.and_then(|s| s.logo_url.as_deref()))
        .map(str::to_string)
        .or_else(|| logo_de_lancement(url_flux))
}
