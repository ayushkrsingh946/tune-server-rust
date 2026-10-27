//! #7051 — Radio Bergeijk (fil forum 2443) : une station jouée depuis
//! l'annuaire, hors de « Mes radios », perdait son logo au premier titre.
//!
//! La fiche est celle relevée le 27/10 sur l'annuaire, anonymisée : logo
//! RELATIF en `/storage/…`, flux Ogg FLAC.

use super::*;

/// La fiche réelle donne un logo relatif ; c'est la version absolue, dans le
/// dossier des logos de l'annuaire, que l'écoute en un clic transmet.
#[test]
fn la_fiche_au_logo_relatif_donne_un_logo_absolu_a_la_lecture() {
    let brut = vec![json!({
        "id": 9901,
        "name": "Radio Exemple",
        "stream_url": "https://stream.radio-exemple.nl/listen/radio_exemple/flac",
        "logo_url": "/storage/radio-logos/radio-metadata/S009901.webp",
        "quality": "flac",
        "country": "NL",
    })];
    let fiches = annuaire_depuis_json(&brut);
    assert_eq!(fiches.len(), 1);
    let logo = fiches[0].logo_url.as_deref().expect("la fiche a un logo");
    assert_eq!(
        logo,
        "https://mozaiklabs.fr/storage/radio-logos/radio-metadata/S009901.webp"
    );
    assert!(logo.starts_with(LOGOS_DE_L_ANNUAIRE));
}

/// La lecture partagée (« Mes radios » et écoute depuis l'annuaire) retient
/// le logo de lancement : sans cet appel, le sondeur ne trouve rien pour une
/// station hors de « Mes radios ». La fonction appelée est définie dans
/// tune-core : sa présence ici ne peut pas venir de sa définition.
#[test]
fn jouer_un_flux_retient_le_logo_de_lancement() {
    let source = include_str!("radios.rs");
    let debut = source
        .find("pub(crate) async fn jouer_un_flux(")
        .expect("jouer_un_flux existe");
    let corps = &source[debut..];
    let fin = corps.find("\n}\n").expect("fin de jouer_un_flux");
    let corps = &corps[..fin];
    let appel = corps
        .find("retenir_le_logo_de_lancement(")
        .expect("jouer_un_flux doit retenir le logo de lancement (#7051)");
    let lecture = corps
        .find("state.orchestrator.play(")
        .expect("jouer_un_flux lance la lecture");
    assert!(
        appel < lecture,
        "le logo doit être retenu AVANT le lancement : le sondeur peut passer aussitôt"
    );
}

/// La lecture générique d'une zone (`POST /zones/{id}/play` avec
/// `source: "radio"` et une pochette) retient aussi le logo de lancement,
/// avant de lancer la lecture.
#[test]
fn la_lecture_generique_d_une_radio_retient_le_logo_de_lancement() {
    let source = include_str!("playback.rs");
    let debut = source
        .find("async fn play(")
        .expect("le play générique existe");
    let corps = &source[debut..];
    let fin = corps.find("\n}\n").expect("fin du play générique");
    let corps = &corps[..fin];
    let appel = corps
        .find("retenir_le_logo_de_lancement(")
        .expect("le play générique doit retenir le logo d'une radio (#7051)");
    let lecture = corps[appel..]
        .find("lancer_la_lecture(&state, orch_req")
        .is_some();
    assert!(
        lecture,
        "le logo doit être retenu AVANT de lancer la lecture de la radio"
    );
}
