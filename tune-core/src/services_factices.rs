//! #6336 — remplacer, EN TEST SEULEMENT, les programmes externes qu'appellent
//! les routes de matériel : le démon AirPlay 2 (`airplay-daemon`) et le
//! montage SMB (`mount.cifs`, ou l'assistant par `sudo`).
//!
//! Ces routes lançaient un vrai programme système, introuvable ou dangereux
//! dans un banc d'essai : on ne pouvait les éprouver qu'en modifiant le code de
//! production. Ici, un test déclare « quand le serveur lance `mount.cifs`,
//! lance plutôt ce faux », puis passe par le VRAI routeur.
//!
//! La feature `services-factices` n'est allumée que par les
//! `[dev-dependencies]` de `tune-server` (resolver 2), comme
//! `journal-de-test` : dans tout binaire publié, [`substitut`] rend toujours
//! `None`, sans registre ni verrou, et le chemin de production est celui
//! d'avant.
//!
//! Les services réseau (Qobuz, TIDAL, Deezer) ne passent PAS par ici : leurs
//! adresses de base se donnent à la construction du service
//! (`avec_bases_de_test`), service par service.

#[cfg(any(test, feature = "services-factices"))]
mod registre {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    fn table() -> &'static Mutex<HashMap<String, String>> {
        static TABLE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
        TABLE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub fn substituer_programme(nom: &str, remplacant: &str) {
        table()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(nom.to_string(), remplacant.to_string());
    }

    pub fn substitut(nom: &str) -> Option<String> {
        table()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(nom)
            .cloned()
    }
}

/// Fait lancer `remplacant` partout où le serveur lancerait `nom`.
///
/// Le registre vit le temps du PROCESSUS : un banc qui s'en sert tourne dans
/// son propre binaire de test, et chaque substitution y est la même pour tous
/// ses tests (le faux programme lit ses arguments pour décider de sa réponse).
#[cfg(any(test, feature = "services-factices"))]
pub fn substituer_programme(nom: &str, remplacant: &str) {
    registre::substituer_programme(nom, remplacant);
}

/// Le programme à lancer à la place de `nom`, s'il en a été déclaré un.
#[cfg(any(test, feature = "services-factices"))]
pub fn substitut(nom: &str) -> Option<String> {
    registre::substitut(nom)
}

/// Hors construction de tests : jamais de substitut.
#[cfg(not(any(test, feature = "services-factices")))]
#[inline(always)]
pub fn substitut(_nom: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_programme_non_declare_n_a_pas_de_substitut() {
        assert_eq!(substitut("programme-jamais-declare-6336"), None);
    }

    #[test]
    fn un_programme_declare_rend_son_remplacant() {
        substituer_programme("programme-declare-6336", "/chemin/du/faux");
        assert_eq!(
            substitut("programme-declare-6336").as_deref(),
            Some("/chemin/du/faux")
        );
    }
}
