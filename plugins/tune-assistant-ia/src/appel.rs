//! Le chemin unique de tout appel : cache, plafond, fournisseur, comptage.
//!
//! L'ordre est la garantie :
//!
//! 1. le cache d'abord — une question déjà posée ne coûte rien, même plafond
//!    atteint ;
//! 2. le plafond ensuite, sous verrou — deux appels simultanés ne peuvent pas
//!    franchir ensemble un plafond que chacun aurait vu ouvert ;
//! 3. l'appel, puis le comptage de SES jetons, au tarif de la table.

use axum::http::StatusCode;
use serde_json::{Value, json};
use tracing::info;

use crate::Etat;
use crate::cache;
use crate::couts;
use crate::fournisseur::{Demande, ErreurFournisseur};
use crate::reglages::{self, EtatCle, Reglages};

/// Version des consignes : la changer invalide le cache.
pub const VERSION_CONSIGNES: &str = "v1";

/// Pourquoi un appel n'a pas eu lieu, ou a échoué.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErreurAppel {
    NonConfigure,
    CleIllisible,
    TarifInconnu,
    PlafondAtteint,
    Fournisseur(ErreurFournisseur),
}

impl ErreurAppel {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NonConfigure => "non_configure",
            Self::CleIllisible => "cle_illisible",
            Self::TarifInconnu => "tarif_inconnu",
            Self::PlafondAtteint => "plafond_atteint",
            Self::Fournisseur(e) => e.code(),
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::NonConfigure => {
                "Assistant IA non configuré : choisissez un fournisseur et saisissez votre clé dans Réglages ▸ Assistant IA.".into()
            }
            Self::CleIllisible => {
                "La clé enregistrée ne s'ouvre plus sur ce serveur : saisissez-la de nouveau.".into()
            }
            Self::TarifInconnu => {
                "Tarif inconnu pour ce modèle : renseignez ses prix dans Réglages ▸ Assistant IA.".into()
            }
            Self::PlafondAtteint => {
                "Plafond mensuel de dépenses atteint : plus aucun appel ce mois-ci. Relevez-le dans Réglages ▸ Assistant IA.".into()
            }
            Self::Fournisseur(e) => e.message(),
        }
    }

    /// Jamais 401 : le client web y lit une session expirée et déconnecte.
    pub fn statut(&self) -> StatusCode {
        match self {
            Self::NonConfigure | Self::CleIllisible | Self::TarifInconnu => StatusCode::BAD_REQUEST,
            Self::PlafondAtteint => StatusCode::CONFLICT,
            Self::Fournisseur(ErreurFournisseur::CleRefusee) => StatusCode::BAD_REQUEST,
            Self::Fournisseur(_) => StatusCode::BAD_GATEWAY,
        }
    }

    pub fn corps(&self) -> Value {
        json!({ "ok": false, "error": self.code(), "message": self.message() })
    }
}

/// Une réponse, et d'où elle vient.
#[derive(Clone, Debug)]
pub struct Resultat {
    pub texte: String,
    pub depuis_cache: bool,
    pub cout_micro_eur: u64,
}

/// Un appel complet. `entree_cache` identifie la question (déjà normalisée) ;
/// `None` = ne pas utiliser le cache (« Tester la clé »).
pub async fn appeler(
    etat: &Etat,
    usage: &str,
    classique: bool,
    systeme: &str,
    message: &str,
    max_jetons: u32,
    entree_cache: Option<&str>,
) -> Result<Resultat, ErreurAppel> {
    let r = Reglages::charger(&etat.backend);
    let famille = r.famille.ok_or(ErreurAppel::NonConfigure)?;
    let modele = r.modele_effectif(classique);
    if modele.is_empty() {
        return Err(ErreurAppel::NonConfigure);
    }
    let cle_cache =
        entree_cache.map(|e| cache::cle(&[VERSION_CONSIGNES, usage, famille.nom(), &modele, e]));

    // 1. Le cache.
    if let Some(k) = &cle_cache
        && let Some(texte) = etat.cache.lire(k)
    {
        let _verrou = etat.verrou_conso.lock().await;
        let mut c = reglages::charger_consommation(&etat.backend);
        c.depuis_cache += 1;
        let _ = reglages::ranger_consommation(&etat.backend, &c);
        info!(usage, "assistant_ia_reponse_du_cache");
        return Ok(Resultat {
            texte,
            depuis_cache: true,
            cout_micro_eur: 0,
        });
    }

    // 2. La clé et le tarif.
    let cle = match reglages::cle_en_clair(&etat.backend, &etat.coffre) {
        EtatCle::Ouverte(k) => Some(k),
        EtatCle::Illisible => return Err(ErreurAppel::CleIllisible),
        EtatCle::Absente if famille.exige_une_cle() => return Err(ErreurAppel::NonConfigure),
        EtatCle::Absente => None,
    };
    let tarif = r.tarif(&modele).ok_or(ErreurAppel::TarifInconnu)?;
    let acces = reglages::acces(&r, cle).ok_or(ErreurAppel::NonConfigure)?;

    // 3. Le plafond, sous verrou jusqu'au comptage.
    let _verrou = etat.verrou_conso.lock().await;
    let mut c = reglages::charger_consommation(&etat.backend);
    if c.micro_eur >= r.plafond_micro_eur {
        info!(usage, "assistant_ia_plafond_atteint");
        return Err(ErreurAppel::PlafondAtteint);
    }

    let fournisseur = (etat.fabrique)(acces);
    let demande = Demande {
        modele: modele.clone(),
        systeme: systeme.to_string(),
        message: message.to_string(),
        max_jetons,
        json: true,
    };
    let rep = fournisseur
        .completer(&demande)
        .await
        .map_err(ErreurAppel::Fournisseur)?;

    let cout = couts::cout_micro_eur(tarif, rep.jetons_entree, rep.jetons_sortie, r.taux_usd_eur);
    c.appels += 1;
    c.jetons_entree += rep.jetons_entree;
    c.jetons_sortie += rep.jetons_sortie;
    c.micro_eur += cout;
    let _ = reglages::ranger_consommation(&etat.backend, &c);
    info!(
        usage,
        fournisseur = famille.nom(),
        modele = %modele,
        jetons_entree = rep.jetons_entree,
        jetons_sortie = rep.jetons_sortie,
        cout_micro_eur = cout,
        "assistant_ia_appel"
    );
    if let Some(k) = &cle_cache {
        etat.cache.ecrire(k, &rep.texte);
    }
    Ok(Resultat {
        texte: rep.texte,
        depuis_cache: false,
        cout_micro_eur: cout,
    })
}

/// Retire d'une réponse ce qui entoure l'objet JSON (bloc ```json, phrase
/// d'introduction) et le lit.
pub fn objet_json(texte: &str) -> Option<Value> {
    let debut = texte.find('{')?;
    let fin = texte.rfind('}')?;
    if fin < debut {
        return None;
    }
    serde_json::from_str::<Value>(&texte[debut..=fin])
        .ok()
        .filter(Value::is_object)
}

/// Le message HTTP d'une erreur d'appel.
pub fn reponse_erreur(e: &ErreurAppel) -> axum::response::Response {
    use axum::response::IntoResponse;
    (e.statut(), axum::Json(e.corps())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_objet_json_se_retrouve_dans_un_bloc_de_code() {
        let v = objet_json("Voici :\n```json\n{\"a\": 1}\n```").unwrap();
        assert_eq!(v["a"], 1);
        assert!(objet_json("pas de json").is_none());
        assert!(objet_json("[1, 2]").is_none());
    }

    #[test]
    fn aucune_erreur_ne_rend_401() {
        for e in [
            ErreurAppel::NonConfigure,
            ErreurAppel::CleIllisible,
            ErreurAppel::TarifInconnu,
            ErreurAppel::PlafondAtteint,
            ErreurAppel::Fournisseur(ErreurFournisseur::CleRefusee),
            ErreurAppel::Fournisseur(ErreurFournisseur::Quota),
        ] {
            assert_ne!(e.statut(), StatusCode::UNAUTHORIZED, "{e:?}");
        }
    }
}
