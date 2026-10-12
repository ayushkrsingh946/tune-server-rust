//! La clé d'API, chiffrée au repos.
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
//! qu'un jeton de session : elle est donc en plus SCELLÉE.
//!
//! # Construction
//!
//! - une clé maîtresse de 256 bits, tirée du générateur du système, posée
//!   dans un FICHIER à côté de la base (`assistant-ia.cle`, droits 0600) —
//!   jamais dans la base elle-même : une copie de la base seule ne suffit
//!   plus ;
//! - XChaCha20-Poly1305 (les primitives de [`tune_core::secret_envelope`]),
//!   un nonce aléatoire de 192 bits par scellement.
//!
//! # Ce que cela ne protège pas
//!
//! Qui détient la machine détient le fichier ET la base : le chiffrement
//! protège la copie de la base, pas la machine. Si le fichier disparaît (base
//! déplacée sans lui), la clé ne s'ouvre plus : l'écran le dit
//! (`cle_illisible`) et demande de la ressaisir. Rien n'est perdu d'autre.

use std::io::Write;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

fn hex_encode(octets: &[u8]) -> String {
    octets.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(texte: &str) -> Result<Vec<u8>, String> {
    if !texte.len().is_multiple_of(2) || !texte.is_ascii() {
        return Err("hexadécimal invalide".into());
    }
    (0..texte.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&texte[i..i + 2], 16).map_err(|_| "hexadécimal invalide".into())
        })
        .collect()
}

/// Le nom du fichier de la clé maîtresse.
pub const FICHIER_CLE: &str = "assistant-ia.cle";

#[derive(Serialize, Deserialize)]
struct Scelle {
    v: u8,
    nonce: String,
    ct: String,
}

pub struct Coffre {
    chemin: PathBuf,
}

impl Coffre {
    /// Un coffre dont la clé maîtresse vit dans `dossier`.
    pub fn new(dossier: &Path) -> Self {
        Self {
            chemin: dossier.join(FICHIER_CLE),
        }
    }

    fn lire_la_cle(&self) -> Result<Option<[u8; 32]>, String> {
        match std::fs::read_to_string(&self.chemin) {
            Ok(t) => {
                let octets = hex_decode(t.trim())?;
                let cle: [u8; 32] = octets
                    .try_into()
                    .map_err(|_| "clé maîtresse de longueur inattendue".to_string())?;
                Ok(Some(cle))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("clé maîtresse illisible : {}", e.kind())),
        }
    }

    fn creer_la_cle(&self) -> Result<[u8; 32], String> {
        let mut cle = [0u8; 32];
        getrandom::getrandom(&mut cle).map_err(|e| e.to_string())?;
        if let Some(parent) = self.chemin.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut f = options.open(&self.chemin).map_err(|e| e.to_string())?;
        f.write_all(hex_encode(&cle).as_bytes())
            .map_err(|e| e.to_string())?;
        Ok(cle)
    }

    /// Scelle `clair`. Crée la clé maîtresse au premier usage.
    pub fn sceller(&self, clair: &str) -> Result<String, String> {
        let cle = match self.lire_la_cle()? {
            Some(c) => c,
            None => self.creer_la_cle()?,
        };
        let mut nonce = [0u8; 24];
        getrandom::getrandom(&mut nonce).map_err(|e| e.to_string())?;
        let chiffre = XChaCha20Poly1305::new((&cle).into())
            .encrypt(XNonce::from_slice(&nonce), clair.as_bytes())
            .map_err(|_| "scellement impossible".to_string())?;
        serde_json::to_string(&Scelle {
            v: 1,
            nonce: hex_encode(&nonce),
            ct: hex_encode(&chiffre),
        })
        .map_err(|e| e.to_string())
    }

    /// Ouvre une valeur scellée par [`Coffre::sceller`].
    pub fn ouvrir(&self, scelle: &str) -> Result<String, String> {
        let s: Scelle = serde_json::from_str(scelle).map_err(|_| "valeur scellée illisible")?;
        if s.v != 1 {
            return Err(format!("version de scellement inconnue : {}", s.v));
        }
        let cle = self
            .lire_la_cle()?
            .ok_or_else(|| "clé maîtresse absente".to_string())?;
        let nonce = hex_decode(&s.nonce)?;
        if nonce.len() != 24 {
            return Err("nonce de longueur inattendue".into());
        }
        let clair = XChaCha20Poly1305::new((&cle).into())
            .decrypt(XNonce::from_slice(&nonce), hex_decode(&s.ct)?.as_slice())
            .map_err(|_| "la valeur ne s'ouvre pas avec cette clé maîtresse".to_string())?;
        String::from_utf8(clair).map_err(|_| "valeur ouverte non UTF-8".to_string())
    }
}

/// Les quatre derniers caractères d'une clé, seuls montrés à l'écran.
pub fn fin_de_cle(cle: &str) -> String {
    let car: Vec<char> = cle.trim().chars().collect();
    let debut = car.len().saturating_sub(4);
    car[debut..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sceller_puis_ouvrir_rend_la_cle_et_le_scelle_ne_la_contient_pas() {
        let dir = tempfile::tempdir().unwrap();
        let coffre = Coffre::new(dir.path());
        let scelle = coffre.sceller("sk-ant-api03-SECRET-abcd").unwrap();
        assert!(!scelle.contains("SECRET"), "{scelle}");
        assert_eq!(coffre.ouvrir(&scelle).unwrap(), "sk-ant-api03-SECRET-abcd");
        // Deux scellements de la même clé ne se ressemblent pas (nonce).
        assert_ne!(scelle, coffre.sceller("sk-ant-api03-SECRET-abcd").unwrap());
    }

    #[test]
    fn sans_le_fichier_la_valeur_ne_s_ouvre_plus() {
        let dir = tempfile::tempdir().unwrap();
        let scelle = Coffre::new(dir.path()).sceller("cle").unwrap();
        let autre = tempfile::tempdir().unwrap();
        assert!(Coffre::new(autre.path()).ouvrir(&scelle).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn le_fichier_n_est_lisible_que_par_son_proprietaire() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        Coffre::new(dir.path()).sceller("x").unwrap();
        let mode = std::fs::metadata(dir.path().join(FICHIER_CLE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn la_fin_de_cle_ne_garde_que_quatre_caracteres() {
        assert_eq!(fin_de_cle("sk-ant-1234567890wxyz"), "wxyz");
        assert_eq!(fin_de_cle("ab"), "ab");
    }
}
