//! #6197 — le coffre des identifiants qu'un greffon source de streaming reçoit.
//!
//! Un greffon WASM qui fournit un service (MusicMe ou autre) se connecte avec
//! les champs que déclare son manifeste — souvent une adresse et un mot de
//! passe. Tune doit les garder pour reconnecter le service au démarrage ; il
//! ne doit pas les garder EN CLAIR dans la base.
//!
//! # Construction
//!
//! - XChaCha20-Poly1305 (la primitive de `db_backup` et `secret_envelope`),
//!   nonce aléatoire de 24 octets par scellement ;
//! - données associées : `tune-greffon-v1|{greffon}|{service}` — un blob
//!   scellé pour un greffon ne s'ouvre pas pour un autre, même copié ligne
//!   pour ligne dans la base ;
//! - clé de 32 octets dans un FICHIER à part de la base (droits 0600 sous
//!   Unix) : une fuite de la base seule (sauvegarde, export, capture d'écran
//!   de `settings`) ne livre aucun mot de passe.
//!
//! Ce n'est pas un coffre contre un attaquant qui lit tout le disque : la clé
//! est à côté. C'est la même promesse que les jetons des autres services,
//! en mieux — eux sont en clair.
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};
use serde_json::{Value, json};

/// Version du format scellé. Un blob d'une autre version est refusé.
pub const VERSION_SCELLE: u8 = 1;

/// Nom du fichier de clé, à côté de la base.
pub const NOM_FICHIER_CLE: &str = "plugin-credentials.key";

/// La variable d'environnement qui désigne un autre fichier de clé.
pub const VARIABLE_FICHIER_CLE: &str = "TUNE_PLUGIN_CREDENTIALS_KEY";

const NONCE_LEN: usize = 24;
const CLE_LEN: usize = 32;

/// La clé du coffre.
pub struct CoffreDIdentifiants {
    cle: [u8; CLE_LEN],
}

impl std::fmt::Debug for CoffreDIdentifiants {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CoffreDIdentifiants(****)")
    }
}

fn aleatoire<const N: usize>() -> Result<[u8; N], String> {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).map_err(|e| format!("aléa indisponible : {e}"))?;
    Ok(b)
}

fn donnees_associees(greffon: &str, service: &str) -> Vec<u8> {
    format!("tune-greffon-v{VERSION_SCELLE}|{greffon}|{service}").into_bytes()
}

impl CoffreDIdentifiants {
    /// Un coffre de clé éphémère : rien ne survit au processus (base en
    /// mémoire, tests).
    pub fn ephemere() -> Result<Self, String> {
        Ok(Self {
            cle: aleatoire::<CLE_LEN>()?,
        })
    }

    /// Le fichier de clé pour une base `db_path` : la variable
    /// [`VARIABLE_FICHIER_CLE`] si elle est posée, sinon
    /// [`NOM_FICHIER_CLE`] dans le dossier de la base. `None` pour une base en
    /// mémoire.
    pub fn chemin_pour_la_base(db_path: &str) -> Option<PathBuf> {
        if let Ok(v) = std::env::var(VARIABLE_FICHIER_CLE)
            && !v.trim().is_empty()
        {
            return Some(PathBuf::from(v));
        }
        if db_path == ":memory:" || db_path.is_empty() {
            return None;
        }
        let dossier = Path::new(db_path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        Some(dossier.join(NOM_FICHIER_CLE))
    }

    /// Ouvre la clé du fichier `chemin`, ou la crée (32 octets aléatoires,
    /// droits 0600) s'il n'existe pas. Un fichier d'une autre taille est
    /// refusé plutôt que remplacé : le remplacer rendrait illisibles tous les
    /// identifiants déjà scellés, sans un mot.
    pub fn ouvrir_ou_creer(chemin: &Path) -> Result<Self, String> {
        match std::fs::read(chemin) {
            Ok(octets) => {
                let cle: [u8; CLE_LEN] = octets.as_slice().try_into().map_err(|_| {
                    format!(
                        "{} : {} octets, {CLE_LEN} attendus",
                        chemin.display(),
                        octets.len()
                    )
                })?;
                Ok(Self { cle })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cle = aleatoire::<CLE_LEN>()?;
                if let Some(parent) = chemin.parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("{} : {e}", parent.display()))?;
                }
                ecrire_en_prive(chemin, &cle)?;
                Ok(Self { cle })
            }
            Err(e) => Err(format!("{} : {e}", chemin.display())),
        }
    }

    fn chiffreur(&self) -> Result<XChaCha20Poly1305, String> {
        XChaCha20Poly1305::new_from_slice(&self.cle).map_err(|_| "clé invalide".to_string())
    }

    /// Sceller `valeur` pour `(greffon, service)`. Le résultat est un objet
    /// JSON sans rien de lisible : `{v, nonce, sealed}` en hexadécimal.
    pub fn sceller(&self, greffon: &str, service: &str, valeur: &Value) -> Result<Value, String> {
        let clair = serde_json::to_vec(valeur).map_err(|e| e.to_string())?;
        let nonce = aleatoire::<NONCE_LEN>()?;
        let chiffre = self
            .chiffreur()?
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &clair,
                    aad: &donnees_associees(greffon, service),
                },
            )
            .map_err(|_| "scellement impossible".to_string())?;
        Ok(json!({
            "v": VERSION_SCELLE,
            "nonce": hex(&nonce),
            "sealed": hex(&chiffre),
        }))
    }

    /// Ouvrir un blob scellé par [`Self::sceller`] pour le même
    /// `(greffon, service)`. Refuse un blob altéré, d'une autre version, ou
    /// scellé pour un autre greffon.
    pub fn ouvrir(&self, greffon: &str, service: &str, scelle: &Value) -> Result<Value, String> {
        if scelle.get("v").and_then(Value::as_u64) != Some(u64::from(VERSION_SCELLE)) {
            return Err("version de scellement inconnue".to_string());
        }
        let nonce = scelle
            .get("nonce")
            .and_then(Value::as_str)
            .and_then(unhex)
            .filter(|n| n.len() == NONCE_LEN)
            .ok_or("nonce illisible")?;
        let chiffre = scelle
            .get("sealed")
            .and_then(Value::as_str)
            .and_then(unhex)
            .ok_or("blob illisible")?;
        let clair = self
            .chiffreur()?
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &chiffre,
                    aad: &donnees_associees(greffon, service),
                },
            )
            .map_err(|_| "ouverture refusée (clé, greffon ou blob)".to_string())?;
        serde_json::from_slice(&clair).map_err(|e| e.to_string())
    }
}

#[cfg(unix)]
fn ecrire_en_prive(chemin: &Path, octets: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(chemin)
        .map_err(|e| format!("{} : {e}", chemin.display()))?;
    f.write_all(octets)
        .map_err(|e| format!("{} : {e}", chemin.display()))
}

#[cfg(not(unix))]
fn ecrire_en_prive(chemin: &Path, octets: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(chemin)
        .map_err(|e| format!("{} : {e}", chemin.display()))?;
    f.write_all(octets)
        .map_err(|e| format!("{} : {e}", chemin.display()))
}

fn hex(octets: &[u8]) -> String {
    octets.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identifiants() -> Value {
        json!({ "email": "charly@exemple.fr", "password": "s3cr3t-MusicMe!" })
    }

    /// Les identifiants sont CHIFFRÉS : ni le mot de passe ni l'adresse ne se
    /// lisent dans ce qui part en base, et le coffre les rend à l'identique.
    #[test]
    fn les_identifiants_sont_chiffres() {
        let coffre = CoffreDIdentifiants::ephemere().unwrap();
        let scelle = coffre
            .sceller("source-exemple", "exemple", &identifiants())
            .unwrap();
        let texte = scelle.to_string();
        assert!(!texte.contains("s3cr3t"), "{texte}");
        assert!(!texte.contains("charly"), "{texte}");
        assert!(!texte.contains("password"), "{texte}");
        assert_eq!(
            coffre.ouvrir("source-exemple", "exemple", &scelle).unwrap(),
            identifiants()
        );
        // Deux scellements du même clair diffèrent (nonce aléatoire).
        let autre = coffre
            .sceller("source-exemple", "exemple", &identifiants())
            .unwrap();
        assert_ne!(scelle["sealed"], autre["sealed"]);
    }

    /// Un blob ne s'ouvre ni pour un autre greffon, ni pour un autre service,
    /// ni sous une autre clé, ni altéré.
    #[test]
    fn un_blob_ne_s_ouvre_que_pour_son_greffon() {
        let coffre = CoffreDIdentifiants::ephemere().unwrap();
        let scelle = coffre
            .sceller("source-exemple", "exemple", &identifiants())
            .unwrap();
        assert!(coffre.ouvrir("autre-greffon", "exemple", &scelle).is_err());
        assert!(coffre.ouvrir("source-exemple", "qobuz", &scelle).is_err());
        assert!(
            CoffreDIdentifiants::ephemere()
                .unwrap()
                .ouvrir("source-exemple", "exemple", &scelle)
                .is_err()
        );
        let mut altere = scelle.clone();
        let s = altere["sealed"].as_str().unwrap().to_string();
        let premier = if s.starts_with('0') { "1" } else { "0" };
        altere["sealed"] = json!(format!("{premier}{}", &s[1..]));
        assert!(coffre.ouvrir("source-exemple", "exemple", &altere).is_err());
        let mut version = scelle;
        version["v"] = json!(2);
        assert!(
            coffre
                .ouvrir("source-exemple", "exemple", &version)
                .is_err()
        );
    }

    /// La clé vit dans un fichier à part, créé en 0600, et se relit : ce que
    /// le coffre a scellé avant un redémarrage s'ouvre après.
    #[test]
    fn la_cle_vit_dans_un_fichier_prive_et_survit() {
        let dir = tempfile::tempdir().unwrap();
        let chemin = dir.path().join("sous").join(NOM_FICHIER_CLE);
        let scelle = CoffreDIdentifiants::ouvrir_ou_creer(&chemin)
            .unwrap()
            .sceller("source-exemple", "exemple", &identifiants())
            .unwrap();
        assert_eq!(std::fs::read(&chemin).unwrap().len(), CLE_LEN);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&chemin).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }
        let relu = CoffreDIdentifiants::ouvrir_ou_creer(&chemin).unwrap();
        assert_eq!(
            relu.ouvrir("source-exemple", "exemple", &scelle).unwrap(),
            identifiants()
        );
        // Un fichier abîmé est refusé, pas remplacé.
        std::fs::write(&chemin, b"court").unwrap();
        assert!(CoffreDIdentifiants::ouvrir_ou_creer(&chemin).is_err());
        assert_eq!(std::fs::read(&chemin).unwrap(), b"court");
    }

    #[test]
    fn le_chemin_suit_la_base() {
        if std::env::var(VARIABLE_FICHIER_CLE).is_ok() {
            return;
        }
        assert_eq!(CoffreDIdentifiants::chemin_pour_la_base(":memory:"), None);
        assert_eq!(
            CoffreDIdentifiants::chemin_pour_la_base("/var/lib/tune/tune.db"),
            Some(PathBuf::from("/var/lib/tune").join(NOM_FICHIER_CLE))
        );
        assert_eq!(
            CoffreDIdentifiants::chemin_pour_la_base("tune.db"),
            Some(PathBuf::from(".").join(NOM_FICHIER_CLE))
        );
    }
}
