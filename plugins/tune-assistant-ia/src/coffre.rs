//! La clé d'API, chiffrée au repos — par le coffre UNIQUE du serveur,
//! [`tune_core::coffre`].
//!
//! # Un seul coffre (décision du 12/10/2026)
//!
//! Le greffon avait son propre coffre, écrit à l'étape 1. Le chantier
//! « Enrichissement MetaRust » (#4805) a remonté la même construction dans
//! `tune-core` pour les clés AcoustID et Discogs : même fichier de clé
//! maîtresse, même format de valeur scellée (`{"v":1,"nonce":…,"ct":…}`),
//! mêmes primitives. Il n'en reste qu'UN : ce module n'est plus qu'un accès
//! au coffre de `tune-core`, ouvert sur l'ancien fichier du greffon
//! ([`FICHIER_CLE`]) — aucune valeur déjà scellée n'a à être migrée, le
//! format étant le même (voir l'essai `coffre_avant_jonction`).
//!
//! # Ce qui existait, et pourquoi ce n'est pas suffisant ici
//!
//! Les jetons Qobuz, Tidal ou Deezer vivent EN CLAIR dans la table `settings`
//! (`auth_tokens_<service>`). Ce qui les protège, c'est
//! [`tune_core::secrets::est_secret`] : tout réglage dont le NOM porte
//! `token`, `secret`, `api_key`… est masqué par `/system/config`, retiré des
//! exports et de la sauvegarde de configuration dans le nuage. Ce mécanisme
//! est réutilisé tel quel : la valeur chiffrée vit sous
//! [`crate::reglages::CLE_API_CHIFFREE`], dont le nom porte `api_key`.
//!
//! Mais une copie de `tune.db` — une sauvegarde, un WAL recopié, un paquet de
//! support — emporte ces jetons. Une clé d'API payante se révoque moins vite
//! qu'un jeton de session : elle est donc en plus SCELLÉE (clé maîtresse dans
//! un fichier à côté de la base, droits 0600 ; XChaCha20-Poly1305).

use std::path::Path;

pub use tune_core::coffre::{Coffre, fin_de_cle};

/// Le fichier de la clé maîtresse que le greffon utilisait avant la jonction.
pub const FICHIER_CLE: &str = "assistant-ia.cle";

/// Le coffre du greffon, dont la clé maîtresse vit dans `dossier` (le dossier
/// de la base).
pub fn du_greffon(dossier: &Path) -> Coffre {
    Coffre::avec_fichier(dossier, FICHIER_CLE)
}
