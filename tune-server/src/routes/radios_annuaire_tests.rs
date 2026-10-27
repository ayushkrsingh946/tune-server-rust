//! Essais purs de l'annuaire v2 (#6516) : aucun réseau, aucune base.

use super::*;

const EXTRAIT: &str = include_str!("../../tests/fixtures/annuaire_v2_extrait_20261019.json");

fn fiches() -> Vec<StationAnnuaire> {
    let corps: Value = serde_json::from_str(EXTRAIT).unwrap();
    annuaire_depuis_json(&radios_du_corps(corps))
}

fn station(id: i64, nom: &str, url: &str) -> RadioStation {
    RadioStation {
        id: Some(id),
        name: nom.into(),
        url: url.into(),
        homepage: None,
        logo_url: None,
        country: None,
        language: None,
        genre: None,
        codec: None,
        bitrate: None,
        is_favorite: false,
        last_played: None,
        play_count: 0,
    }
}

#[test]
fn une_fiche_refusee_ou_retiree_n_entre_pas_dans_l_annuaire() {
    let f = fiches();
    let noms: Vec<&str> = f.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(f.len(), 4, "{noms:?}");
    assert!(!noms.contains(&"FIP Latino"), "refusée : {noms:?}");
    assert!(!noms.contains(&"FIP Tout nouveau"), "retirée : {noms:?}");
}

#[test]
fn la_fiche_v2_garde_cle_logo_absolu_et_tous_ses_flux() {
    let f = fiches();
    let fip = f.iter().find(|s| s.name == "FIP").unwrap();
    assert_eq!(fip.external_id.as_deref(), Some("S000973"));
    assert_eq!(fip.radio_metadata_key.as_deref(), Some("fip"));
    assert_eq!(
        fip.logo_url.as_deref(),
        Some("https://mozaiklabs.fr/storage/radio-logos/01KV2K6H8392ZH6JA8NNK4904E.png")
    );
    assert!(
        fip.stream_urls
            .contains(&"http://radio-metadata.fr:8000/fiproon".to_string())
    );
    assert_eq!(
        fip.stream_urls[0], fip.stream_url,
        "le flux principal d'abord"
    );
    assert_eq!(
        (fip.codec.as_deref(), fip.bitrate),
        (Some("AAC"), Some(192))
    );
    assert_eq!(fip.categories, vec!["eclectic".to_string()]);
}

#[test]
fn une_entree_v1_se_lit_toujours() {
    let v1 = serde_json::json!([{"name": "X", "stream_url": "https://x.example/s", "logo_url": "/storage/x.png"}]);
    let f = annuaire_depuis_json(v1.as_array().unwrap());
    assert_eq!(f.len(), 1);
    assert_eq!(
        f[0].logo_url.as_deref(),
        Some("https://mozaiklabs.fr/storage/x.png")
    );
    assert_eq!(f[0].stream_urls, vec!["https://x.example/s".to_string()]);
}

#[test]
fn mes_radios_se_rapprochent_par_flux_puis_par_nom() {
    let f = fiches();
    let index = IndexAnnuaire::new(&f);
    // Le semis : même adresse, autre schéma et casse.
    let fip = index
        .fiche_de("FIP", "HTTPS://icecast.radiofrance.fr/fip-hifi.aac/")
        .unwrap();
    assert_eq!(fip.external_id.as_deref(), Some("S000973"));
    // Le semis de FIP Nouveautés n'a pas le `?id=radiofrance` de l'annuaire :
    // c'est le nom replié qui le rapproche.
    let nouv = index
        .fiche_de(
            "FIP Nouveautés",
            "https://icecast.radiofrance.fr/fipnouveautes-hifi.aac",
        )
        .unwrap();
    assert_eq!(nouv.external_id.as_deref(), Some("S000981"));
    // Une station que l'annuaire a refusée ne se rapproche de rien.
    assert!(
        index
            .fiche_de(
                "FIP Latino",
                "https://icecast.radiofrance.fr/fiplatino-hifi.aac"
            )
            .is_none()
    );
}

#[test]
fn l_annuaire_filtre_par_recherche_pays_et_genre_et_signale_mes_radios() {
    let f = fiches();
    let mes = vec![
        station(7, "FIP", "https://icecast.radiofrance.fr/fip-hifi.aac"),
        station(
            9,
            "FIP Latino",
            "https://icecast.radiofrance.fr/fiplatino-hifi.aac",
        ),
    ];
    let etat = EtatAnnuaire {
        origine: "reseau",
        ..Default::default()
    };
    let tout = corps_annuaire(&f, &mes, &FiltresAnnuaire::default(), &etat);
    assert_eq!(tout["total"], 4);
    assert_eq!(
        tout["attribution"]["support_url"],
        "https://ko-fi.com/aleceiffel"
    );
    let fip = tout["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["external_id"] == "S000973")
        .unwrap();
    assert_eq!(fip["my_radio_id"], 7);
    assert_eq!(fip["now_playing"], true);

    let jazz = corps_annuaire(
        &f,
        &mes,
        &FiltresAnnuaire {
            q: Some("jazz".into()),
            ..Default::default()
        },
        &etat,
    );
    assert_eq!(jazz["total"], 1, "{jazz}");
    assert_eq!(jazz["items"][0]["external_id"], "S000978");

    let us = corps_annuaire(
        &f,
        &mes,
        &FiltresAnnuaire {
            country: Some("us".into()),
            ..Default::default()
        },
        &etat,
    );
    assert_eq!(us["total"], 1, "{us}");
    assert_eq!(us["items"][0]["name"], "Radio Paradise: Main Mix");
    // Les genres proposés sont ceux du pays choisi.
    assert_eq!(us["genres"].as_array().unwrap().len(), 1);

    let latino = corps_annuaire(
        &f,
        &mes,
        &FiltresAnnuaire {
            q: Some("latino".into()),
            ..Default::default()
        },
        &etat,
    );
    assert_eq!(
        latino["total"], 0,
        "une station refusée n'est pas dans l'annuaire"
    );
}

#[test]
fn la_pagination_reste_dans_ses_bornes() {
    let f = fiches();
    let etat = EtatAnnuaire::default();
    let page = corps_annuaire(
        &f,
        &[],
        &FiltresAnnuaire {
            offset: Some(3),
            limit: Some(2),
            ..Default::default()
        },
        &etat,
    );
    assert_eq!(
        (page["total"].as_u64(), page["count"].as_u64()),
        (Some(4), Some(1))
    );
    let loin = corps_annuaire(
        &f,
        &[],
        &FiltresAnnuaire {
            offset: Some(99),
            ..Default::default()
        },
        &etat,
    );
    assert_eq!(loin["count"], 0);
}

#[test]
fn une_station_creee_depuis_l_annuaire_porte_son_flux_principal() {
    let f = fiches();
    let rp = f
        .iter()
        .find(|s| s.external_id.as_deref() == Some("S002479"))
        .unwrap();
    let st = station_depuis_fiche(rp);
    assert_eq!(st.url, rp.stream_url);
    assert_eq!(st.logo_url, rp.logo_url);
    assert_eq!(st.country.as_deref(), Some("US"));
    assert!(!st.is_favorite);
}

#[test]
fn une_fiche_rapprochee_par_le_nom_seul_ne_prete_pas_son_logo() {
    // Fil du 19/10 : le logo d'une station ne vient que de la fiche qui lui
    // est liée (`external_id`) ou qui partage son adresse de flux.
    let f = fiches();
    let fip = f.iter().find(|s| s.name == "FIP").unwrap();
    let ailleurs = "https://ailleurs.example/fip";
    let par_nom = rattachement(fip, Lien::Nom, ailleurs);
    assert!(par_nom["logo_url"].is_null(), "{par_nom}");
    assert_eq!(par_nom["rapproche_par"], "nom");
    let par_flux = rattachement(fip, Lien::Flux, &fip.stream_url);
    assert_eq!(par_flux["logo_url"], serde_json::json!(fip.logo_url));
    assert_eq!(par_flux["flux_change"], false);
    let par_id = rattachement(fip, Lien::Id, ailleurs);
    assert_eq!(par_id["logo_url"], serde_json::json!(fip.logo_url));
    assert_eq!(par_id["flux_change"], true);
    assert_eq!(
        par_id["stream_url_annuaire"],
        serde_json::json!(fip.stream_url)
    );
    // Le nom suffit toujours au rapprochement (pas de doublon à l'ajout).
    let index = IndexAnnuaire::new(&f);
    assert_eq!(
        index.lien(None, "FIP", ailleurs).map(|(_, l)| l),
        Some(Lien::Nom)
    );
    assert_eq!(
        index.lien(Some("S000973"), "x", ailleurs).map(|(_, l)| l),
        Some(Lien::Id)
    );
    // Lié à une fiche sortie de l'annuaire : aucun repli par l'adresse.
    assert_eq!(index.lien(Some("S999001"), "FIP", &fip.stream_url), None);
}

// ── #7177 (web#2443) — les flux d'une fiche, et le flux préféré ─────────────

const FIP_MIDFI: &str = "https://icecast.radiofrance.fr/fip-midfi.mp3?id=radiofrance";

#[test]
fn les_flux_proposes_d_une_fiche_v2_portent_format_et_debit_le_principal_d_abord() {
    let f = fiches();
    let fip = f.iter().find(|s| s.name == "FIP").unwrap();
    let flux = fip.flux_proposes();
    assert_eq!(flux.len(), 4, "{flux:?}");
    assert_eq!(flux[0].url, fip.stream_url);
    assert!(flux[0].primary);
    assert_eq!(flux.iter().filter(|x| x.primary).count(), 1);
    let mp3 = flux.iter().find(|x| x.url == FIP_MIDFI).unwrap();
    assert_eq!(
        (mp3.codec.as_deref(), mp3.bitrate, mp3.sample_rate, mp3.hls),
        (Some("MP3"), Some(128), Some(48000), false)
    );
}

#[test]
fn une_entree_v1_propose_son_seul_flux_sans_format() {
    let v1 = serde_json::json!([{"name": "X", "stream_url": "https://x.example/s"}]);
    let f = annuaire_depuis_json(v1.as_array().unwrap());
    let flux = f[0].flux_proposes();
    assert_eq!(flux.len(), 1);
    assert_eq!(flux[0].url, "https://x.example/s");
    assert!(flux[0].primary);
    assert_eq!((flux[0].codec.as_deref(), flux[0].bitrate), (None, None));
}

#[test]
fn le_flux_prefere_se_choisit_parmi_ceux_de_la_fiche_avec_son_format() {
    let f = fiches();
    let fip = f.iter().find(|s| s.name == "FIP").unwrap();
    let mut st = station(1, "FIP", &fip.stream_url);
    assert!(poser_le_flux_prefere(fip, &mut st, FIP_MIDFI));
    assert_eq!(st.url, FIP_MIDFI);
    assert_eq!((st.codec.as_deref(), st.bitrate), (Some("mp3"), Some(128)));
    // Le rattrapage de fond (#7043) le garde : la fiche le publie toujours.
    assert!(!crate::routes::radios::flux_a_poser(
        &f,
        &mut st,
        Some("S000973")
    ));
    assert_eq!(st.url, FIP_MIDFI);
}

#[test]
fn contre_epreuve_une_adresse_qui_n_est_pas_de_la_fiche_ne_change_rien() {
    let f = fiches();
    let fip = f.iter().find(|s| s.name == "FIP").unwrap();
    let mut st = station(1, "FIP", &fip.stream_url);
    assert!(!poser_le_flux_prefere(
        fip,
        &mut st,
        "https://ailleurs.example/fip.mp3"
    ));
    assert_eq!(st.url, fip.stream_url);
    assert_eq!((st.codec.as_deref(), st.bitrate), (None, None));
}

/// #7194 (web#2400) — la carte des radios fait ses réseaux des mots-clés de
/// la fiche (le diffuseur) : `/radios/directory` doit les rendre.
#[test]
fn l_annuaire_rend_les_mots_cles_de_chaque_fiche_pour_les_reseaux_de_la_carte() {
    let f = fiches();
    let etat = EtatAnnuaire::default();
    let corps = corps_annuaire(&f, &[], &FiltresAnnuaire::default(), &etat);
    let items = corps["items"].as_array().unwrap();
    let fip = items
        .iter()
        .find(|i| i["external_id"] == "S000973")
        .unwrap();
    assert_eq!(
        fip["keywords"],
        serde_json::json!(["Radio France"]),
        "le diffuseur de FIP doit être servi : {fip}"
    );
    let paradise = items
        .iter()
        .find(|i| i["name"] == "Radio Paradise: Main Mix")
        .unwrap();
    assert_eq!(
        paradise["keywords"],
        serde_json::json!([]),
        "aucun mot-clé : liste vide"
    );
}
