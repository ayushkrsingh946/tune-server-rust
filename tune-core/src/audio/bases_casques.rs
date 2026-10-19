//! La recherche de casques unifiée (#6532, #6388) : un nom cherché, une
//! liste où se mêlent les profils d'AutoEq ([`super::autoeq_base`]) et les
//! courbes d'OPRA ([`super::opra_base`]), chacun avec sa source.
//!
//! Les deux bases sont lues en parallèle et classées par la même règle
//! (le nom exact, puis ce qui commence par la requête, puis le reste ; les
//! noms courts d'abord). Une base injoignable n'efface pas l'autre : ses
//! résultats manquent, et [`RechercheCasques::sources_injoignables`] le
//! dit. Les deux injoignables : [`ErreurBasesCasques`], jamais une liste
//! vide qui ferait croire que le casque n'existe pas.

use serde::Serialize;

use super::autoeq_base::{CatalogueAutoEq, EntreeAutoEq, compacter, normaliser};
use super::opra_base::{CatalogueOpra, CourbeOpra};

/// D'où vient une correction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum SourceCasque {
    #[serde(rename = "autoeq")]
    AutoEq,
    #[serde(rename = "opra")]
    Opra,
}

/// Un résultat, quelle que soit sa base.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResultatCasque {
    pub source: SourceCasque,
    /// Ce que l'import attend : le `path` AutoEq (`POST /eq/autoeq/import`)
    /// ou l'`id` OPRA (`POST /eq/opra/import`).
    pub key: String,
    /// Le nom affiché (« Sennheiser HD 650 », « Sennheiser HD650 »).
    pub name: String,
    /// Le fabricant, quand la base le sépare du modèle (OPRA).
    pub vendor: Option<String>,
    pub model: String,
    /// AutoEq : la source de mesure (« oratory1990 ») ; OPRA : l'auteur de
    /// la courbe. C'est le crédit à afficher.
    pub author: String,
    /// AutoEq : le banc de mesure ; OPRA : la cible et les précisions.
    pub details: Option<String>,
    pub form: Option<String>,
    /// OPRA : le lien vers la source de la courbe.
    pub link: Option<String>,
}

impl ResultatCasque {
    pub fn depuis_autoeq(e: &EntreeAutoEq) -> Self {
        Self {
            source: SourceCasque::AutoEq,
            key: e.chemin.clone(),
            name: e.nom.clone(),
            vendor: None,
            model: e.nom.clone(),
            author: e.source.clone(),
            details: e.banc.clone(),
            form: Some(e.forme.clone()),
            link: None,
        }
    }

    pub fn depuis_opra(c: &CourbeOpra) -> Self {
        Self {
            source: SourceCasque::Opra,
            key: c.id.clone(),
            name: c.nom_affiche(),
            vendor: Some(c.fabricant.clone()),
            model: c.modele.clone(),
            author: c.auteur.clone(),
            details: c.details.clone(),
            form: c.forme.clone(),
            link: c.lien.clone(),
        }
    }
}

/// Le résultat d'une recherche.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RechercheCasques {
    pub results: Vec<ResultatCasque>,
    pub total: usize,
    /// Les bases qui n'ont pas répondu (ni réseau ni copie) : leurs
    /// résultats manquent, et l'écran doit le dire.
    pub unavailable_sources: Vec<SourceCasque>,
}

impl RechercheCasques {
    pub fn sources_injoignables(&self) -> &[SourceCasque] {
        &self.unavailable_sources
    }
}

/// Aucune des deux bases n'a répondu.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("aucune base de corrections n'est joignable (AutoEq : {autoeq} ; OPRA : {opra})")]
pub struct ErreurBasesCasques {
    pub autoeq: String,
    pub opra: String,
}

/// La requête, préparée une fois.
struct Requete {
    q: String,
    mots: Vec<String>,
    compacte: String,
}

impl Requete {
    fn new(requete: &str) -> Self {
        let q = normaliser(requete);
        let mots = q.split_whitespace().map(str::to_string).collect();
        let compacte = compacter(&q);
        Self { q, mots, compacte }
    }

    /// Le rang de `nom` (0 : exact, 1 : commence par, 2 : contient), ou
    /// `None` s'il ne correspond pas. `autres` (auteur, précisions) compte
    /// pour la recherche par mots, pas pour le rang. Même règle que
    /// [`super::autoeq_base::rechercher`].
    fn rang(&self, nom: &str, autres: &[&str]) -> Option<u8> {
        let nom = normaliser(nom);
        let nom_compacte = compacter(&nom);
        let mut texte = nom.clone();
        for a in autres {
            texte.push(' ');
            texte.push_str(&normaliser(a));
        }
        let par_mots = self.mots.iter().all(|m| texte.contains(m.as_str()));
        if !par_mots && !nom_compacte.contains(&self.compacte) {
            return None;
        }
        Some(if nom_compacte == self.compacte {
            0
        } else if nom.starts_with(&self.q) || nom_compacte.starts_with(&self.compacte) {
            1
        } else {
            2
        })
    }
}

/// Classe les profils AutoEq et les courbes OPRA qui correspondent à
/// `requete`, et rend les `limite` premiers et le total.
pub fn classer(
    autoeq: &[EntreeAutoEq],
    opra: &[CourbeOpra],
    requete: &str,
    limite: usize,
) -> (Vec<ResultatCasque>, usize) {
    let r = Requete::new(requete);
    if r.compacte.is_empty() {
        return (Vec::new(), 0);
    }
    let mut trouves: Vec<(u8, ResultatCasque)> = autoeq
        .iter()
        .filter_map(|e| {
            r.rang(&e.nom, &[e.source.as_str()])
                .map(|rang| (rang, ResultatCasque::depuis_autoeq(e)))
        })
        .chain(opra.iter().filter_map(|c| {
            let details = c.details.as_deref().unwrap_or("");
            r.rang(&c.nom_affiche(), &[c.auteur.as_str(), details])
                .map(|rang| (rang, ResultatCasque::depuis_opra(c)))
        }))
        .collect();
    trouves.sort_by(|(ra, a), (rb, b)| {
        ra.cmp(rb)
            .then(a.name.len().cmp(&b.name.len()))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.source.cmp(&b.source))
            .then_with(|| a.author.cmp(&b.author))
            .then_with(|| a.key.cmp(&b.key))
    });
    let total = trouves.len();
    (
        trouves.into_iter().take(limite).map(|(_, r)| r).collect(),
        total,
    )
}

/// Cherche dans les deux bases à la fois.
pub async fn rechercher(
    autoeq: &CatalogueAutoEq,
    opra: &CatalogueOpra,
    requete: &str,
    limite: usize,
) -> Result<RechercheCasques, ErreurBasesCasques> {
    let (index, courbes) = tokio::join!(autoeq.index(), opra.courbes());
    let mut unavailable_sources = Vec::new();
    let (index, erreur_autoeq) = match index {
        Ok(i) => (Some(i), None),
        Err(e) => {
            unavailable_sources.push(SourceCasque::AutoEq);
            (None, Some(e.to_string()))
        }
    };
    let (courbes, erreur_opra) = match courbes {
        Ok(c) => (Some(c), None),
        Err(e) => {
            unavailable_sources.push(SourceCasque::Opra);
            (None, Some(e.to_string()))
        }
    };
    if let (Some(autoeq), Some(opra)) = (&erreur_autoeq, &erreur_opra) {
        return Err(ErreurBasesCasques {
            autoeq: autoeq.clone(),
            opra: opra.clone(),
        });
    }
    let (results, total) = classer(
        index.as_deref().map(Vec::as_slice).unwrap_or(&[]),
        courbes.as_deref().map(Vec::as_slice).unwrap_or(&[]),
        requete,
        limite,
    );
    Ok(RechercheCasques {
        results,
        total,
        unavailable_sources,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::autoeq_base::{TelechargeurAutoEq, analyser_index};
    use crate::audio::opra_base::analyser_base;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const INDEX: &str = "\
- [Sennheiser HD 650](./oratory1990/over-ear/Sennheiser%20HD%20650) by oratory1990\n\
- [Sennheiser HD 650](./crinacle/GRAS%2043AG-7%20over-ear/Sennheiser%20HD%20650) by crinacle on GRAS 43AG-7\n\
- [Fostex T-X0](./oratory1990/over-ear/Fostex%20T-X0) by oratory1990\n";

    /// Lignes réelles d'OPRA (CC BY-SA 4.0, `tests/fixtures/opra/`).
    const OPRA: &str = include_str!("../../tests/fixtures/opra/database_v1_extrait.jsonl");

    #[test]
    fn une_recherche_mele_les_deux_bases_avec_leur_source() {
        let (r, total) = classer(&analyser_index(INDEX), &analyser_base(OPRA), "hd650", 50);
        assert_eq!(total, 5, "2 AutoEq + 3 OPRA : {r:#?}");
        // « Sennheiser HD650 » (OPRA) et « Sennheiser HD 650 » (AutoEq)
        // contiennent tous deux la requête : même rang, le nom court d'abord.
        let sources: Vec<SourceCasque> = r.iter().map(|x| x.source).collect();
        assert_eq!(
            sources,
            [
                SourceCasque::Opra,
                SourceCasque::Opra,
                SourceCasque::Opra,
                SourceCasque::AutoEq,
                SourceCasque::AutoEq
            ]
        );
        let opra = &r[0];
        assert_eq!(opra.vendor.as_deref(), Some("Sennheiser"));
        assert_eq!(opra.model, "HD650");
        assert_eq!(opra.author, "oratory1990");
        assert!(opra.key.starts_with("sennheiser:hd650::"));
        let autoeq = r.iter().find(|x| x.author == "crinacle").unwrap();
        assert_eq!(autoeq.details.as_deref(), Some("GRAS 43AG-7"));
        assert_eq!(
            autoeq.key,
            "crinacle/GRAS 43AG-7 over-ear/Sennheiser HD 650"
        );
    }

    #[test]
    fn la_cible_et_l_auteur_comptent_pour_la_recherche_par_mots() {
        let (r, _) = classer(&[], &analyser_base(OPRA), "hd650 graphic", 50);
        assert_eq!(r.len(), 1, "{r:#?}");
        assert_eq!(
            r[0].details.as_deref(),
            Some("Harman Target • 10 band graphic EQ")
        );
        let (r, total) = classer(&analyser_index(INDEX), &analyser_base(OPRA), "t-x0", 50);
        assert_eq!(total, 2);
        assert_eq!(
            r[0].source,
            SourceCasque::AutoEq,
            "« Fostex T-X0 » des deux côtés"
        );
        assert_eq!(
            classer(&analyser_index(INDEX), &analyser_base(OPRA), " - ", 50).1,
            0
        );
    }

    #[derive(Default)]
    struct FauxReseau {
        coupe_autoeq: AtomicBool,
        coupe_opra: AtomicBool,
    }

    #[async_trait]
    impl TelechargeurAutoEq for FauxReseau {
        async fn telecharger(&self, url: &str, _taille_max: usize) -> Result<String, String> {
            if url.ends_with("INDEX.md") {
                if self.coupe_autoeq.load(Ordering::SeqCst) {
                    return Err("AutoEq injoignable".into());
                }
                Ok(INDEX.into())
            } else {
                if self.coupe_opra.load(Ordering::SeqCst) {
                    return Err("OPRA injoignable".into());
                }
                Ok(OPRA.into())
            }
        }
    }

    fn catalogues(r: &Arc<FauxReseau>) -> (CatalogueAutoEq, CatalogueOpra) {
        (
            CatalogueAutoEq::new("https://exemple.test/results", r.clone(), None),
            CatalogueOpra::new("https://exemple.test/database_v1.jsonl", r.clone(), None),
        )
    }

    #[tokio::test]
    async fn une_base_injoignable_n_efface_pas_l_autre_et_c_est_dit() {
        let reseau = Arc::new(FauxReseau::default());
        reseau.coupe_opra.store(true, Ordering::SeqCst);
        let (a, o) = catalogues(&reseau);
        let r = rechercher(&a, &o, "hd 650", 50).await.unwrap();
        assert_eq!(r.total, 2);
        assert!(r.results.iter().all(|x| x.source == SourceCasque::AutoEq));
        assert_eq!(r.sources_injoignables(), [SourceCasque::Opra]);

        let reseau = Arc::new(FauxReseau::default());
        reseau.coupe_autoeq.store(true, Ordering::SeqCst);
        let (a, o) = catalogues(&reseau);
        let r = rechercher(&a, &o, "hd 650", 50).await.unwrap();
        assert_eq!(r.total, 3);
        assert_eq!(r.sources_injoignables(), [SourceCasque::AutoEq]);
    }

    #[tokio::test]
    async fn les_deux_bases_injoignables_donnent_une_erreur_pas_une_liste_vide() {
        let reseau = Arc::new(FauxReseau::default());
        reseau.coupe_autoeq.store(true, Ordering::SeqCst);
        reseau.coupe_opra.store(true, Ordering::SeqCst);
        let (a, o) = catalogues(&reseau);
        let e = rechercher(&a, &o, "hd 650", 50).await.unwrap_err();
        assert!(e.autoeq.contains("AutoEq injoignable"), "{e}");
        assert!(e.opra.contains("OPRA injoignable"), "{e}");
    }
}
