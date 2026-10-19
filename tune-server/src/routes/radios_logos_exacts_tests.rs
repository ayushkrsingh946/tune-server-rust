//! Fil du 19/10/2026 (iPhone de Bertrand) : « BBC Radio 3 » portait le logo de
//! Berliner Rundfunk 91.4. Le logo d'une station ne vient plus QUE de la fiche
//! qui partage son adresse de flux ; un logo de l'annuaire déjà posé suit sa
//! fiche ; un logo choisi par l'utilisateur n'est jamais touché.

use super::*;

const L_BERLINER: &str = "https://mozaiklabs.fr/storage/radio-logos/bbc-radio-3.png";
const L_BBC: &str = "https://mozaiklabs.fr/storage/radio-logos/radio-metadata/S000365.webp";
const U_BBC3: &str = "http://stream.live.vc.bbcmedia.co.uk/bbc_radio_three";

fn fiche(nom: &str, flux: &str, logo: &str) -> StationAnnuaire {
    StationAnnuaire {
        name: nom.into(),
        stream_url: flux.into(),
        stream_urls: vec![flux.into()],
        logo_url: Some(logo.into()),
        ..Default::default()
    }
}

fn station(nom: &str, url: &str, logo: Option<&str>) -> RadioStation {
    RadioStation {
        id: Some(1),
        name: nom.into(),
        url: url.into(),
        homepage: None,
        logo_url: logo.map(str::to_string),
        country: None,
        language: None,
        genre: None,
        codec: None,
        bitrate: None,
        is_favorite: true,
        last_played: None,
        play_count: 0,
    }
}

#[test]
fn un_logo_de_l_annuaire_deja_pose_suit_sa_fiche_corrigee() {
    // La fiche BBC Radio 3 a reçu son vrai logo : la station qui portait
    // l'ancien (le fichier de Berliner Rundfunk) le reçoit à son tour.
    let annuaire = [fiche("BBC Radio 3", U_BBC3, L_BBC)];
    let st = station(
        "BBC Radio 3",
        "HTTP://stream.live.vc.bbcmedia.co.uk/bbc_radio_three/",
        Some(L_BERLINER),
    );
    assert_eq!(
        logo_a_poser(&annuaire, &st, None),
        DecisionLogo::Poser(Some(L_BBC.into()))
    );
}

#[test]
fn le_nom_seul_ne_prete_jamais_de_logo() {
    // Même nom, AUTRE adresse : ce n'est pas la même station, aucun logo.
    let annuaire = [fiche(
        "Berliner Rundfunk 91.4",
        "https://stream.berliner-rundfunk.de/brf",
        L_BERLINER,
    )];
    let st = station(
        "Berliner Rundfunk 91.4",
        "https://ailleurs.example/flux",
        None,
    );
    assert_eq!(logo_a_poser(&annuaire, &st, None), DecisionLogo::Garder);
}

#[test]
fn le_logo_d_une_autre_station_est_retire() {
    let annuaire = [fiche(
        "Berliner Rundfunk 91.4",
        "https://stream.berliner-rundfunk.de/brf",
        L_BERLINER,
    )];
    let st = station("BBC Radio 3", U_BBC3, Some(L_BERLINER));
    assert_eq!(
        logo_a_poser(&annuaire, &st, None),
        DecisionLogo::Poser(None)
    );
}

#[test]
fn le_logo_choisi_par_l_utilisateur_n_est_jamais_touche() {
    let annuaire = [fiche("BBC Radio 3", U_BBC3, L_BBC)];
    for logo in [
        "https://exemple.invalid/mon-logo.png",
        "/artwork/radio/1.jpg",
    ] {
        let st = station("BBC Radio 3", U_BBC3, Some(logo));
        assert_eq!(
            logo_a_poser(&annuaire, &st, None),
            DecisionLogo::Garder,
            "{logo}"
        );
    }
}

#[test]
fn une_station_sortie_de_l_annuaire_garde_son_logo() {
    // FIP Latino, refusée : plus de fiche, son ancien logo n'est celui de
    // personne d'autre — il reste.
    let annuaire = [fiche(
        "FIP",
        "https://icecast.radiofrance.fr/fip-hifi.aac",
        L_BBC,
    )];
    let ancien = "https://mozaiklabs.fr/storage/radio-logos/fip-latino.png";
    let st = station(
        "FIP Latino",
        "https://icecast.radiofrance.fr/fiplatino-hifi.aac",
        Some(ancien),
    );
    assert_eq!(logo_a_poser(&annuaire, &st, None), DecisionLogo::Garder);
}

#[test]
fn une_station_sans_logo_recoit_celui_de_sa_fiche_par_l_adresse() {
    let annuaire = [fiche("BBC Radio 3", U_BBC3, L_BBC)];
    let st = station("Ma Radio 3", U_BBC3, None);
    assert_eq!(
        logo_a_poser(&annuaire, &st, None),
        DecisionLogo::Poser(Some(L_BBC.into()))
    );
}

#[test]
fn une_station_liee_suit_sa_fiche_meme_apres_un_changement_d_adresse() {
    // #6516 — l'annuaire a changé l'adresse du flux : le lien `external_id`
    // retrouve quand même la fiche, et son logo corrigé.
    let mut f = fiche("BBC Radio 3", "https://nouveau.example/bbc3", L_BBC);
    f.external_id = Some("S000365".into());
    let st = station("BBC Radio 3", U_BBC3, Some(L_BERLINER));
    assert_eq!(
        logo_a_poser(std::slice::from_ref(&f), &st, Some("S000365")),
        DecisionLogo::Poser(Some(L_BBC.into()))
    );
    // Sans le lien, l'adresse ne la retrouve plus : rien n'est posé.
    assert_eq!(logo_a_poser(&[f], &st, None), DecisionLogo::Garder);
}
