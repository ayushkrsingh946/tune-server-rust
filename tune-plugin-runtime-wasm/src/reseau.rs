//! #6197 — la permission `http` : la LISTE DE DOMAINES qu'un greffon peut
//! joindre, jugée ici avant que l'hôte n'ouvre la moindre connexion.
//!
//! La liste vient du manifeste lu par l'hôte (`http_allowed_domains`) et vit
//! dans le `Store` ; le greffon ne peut ni la lire ni l'étendre. Une entrée
//! est un nom exact (`api.exemple.com`, tous ports) ou `*.domaine` (les
//! sous-domaines, pas le domaine lui-même). Les adresses IP s'écrivent telles
//! quelles (`127.0.0.1`).
//!
//! Refusés : un schéma autre que `http`/`https`, une URL sans hôte, des
//! identifiants dans l'URL (`https://user:pass@hôte/`, la forme classique
//! d'une URL qui trompe l'œil), un hôte hors liste.

/// Pourquoi une adresse est refusée. Le `code` part tel quel dans le JSON
/// rendu au greffon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusHttp {
    pub code: &'static str,
    pub detail: String,
}

/// Une liste du manifeste, normalisée : rognée, en minuscules, sans vide ni
/// doublon. Une entrée qui n'est pas un nom d'hôte (schéma, chemin, port,
/// joker ailleurs qu'en tête) est écartée — elle ne doit pas ouvrir plus
/// qu'elle n'en a l'air.
pub fn normaliser_domaines(entrees: &[String]) -> Vec<String> {
    let mut sortie: Vec<String> = Vec::new();
    for e in entrees {
        let e = e.trim().trim_end_matches('.').to_ascii_lowercase();
        let nom = e.strip_prefix("*.").unwrap_or(&e);
        let valide = !nom.is_empty()
            && nom
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
            && !nom.starts_with('.')
            && !nom.contains("..");
        if valide && !sortie.contains(&e) {
            sortie.push(e);
        }
    }
    sortie
}

/// L'hôte de `url` s'il est autorisé par `domaines` (déjà normalisés).
pub fn verifier_l_adresse(url: &str, domaines: &[String]) -> Result<String, RefusHttp> {
    let analysee = url::Url::parse(url).map_err(|e| RefusHttp {
        code: "invalid_url",
        detail: format!("{url} : {e}"),
    })?;
    if !matches!(analysee.scheme(), "http" | "https") {
        return Err(RefusHttp {
            code: "scheme_not_allowed",
            detail: analysee.scheme().to_string(),
        });
    }
    if !analysee.username().is_empty() || analysee.password().is_some() {
        return Err(RefusHttp {
            code: "credentials_in_url",
            detail: "identifiants dans l'URL".to_string(),
        });
    }
    let hote = match analysee.host() {
        Some(url::Host::Domain(d)) => d.trim_end_matches('.').to_ascii_lowercase(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => format!("[{ip}]"),
        None => {
            return Err(RefusHttp {
                code: "invalid_url",
                detail: "URL sans hôte".to_string(),
            });
        }
    };
    let autorise = domaines.iter().any(|d| match d.strip_prefix("*.") {
        Some(parent) => hote.ends_with(&format!(".{parent}")),
        None => &hote == d,
    });
    if autorise {
        Ok(hote)
    } else {
        Err(RefusHttp {
            code: "domain_not_allowed",
            detail: hote,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn liste(d: &[&str]) -> Vec<String> {
        normaliser_domaines(&d.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn la_liste_se_normalise_et_ecarte_ce_qui_ouvrirait_trop() {
        assert_eq!(
            liste(&[
                " API.Exemple.com. ",
                "api.exemple.com",
                "*.cdn.exemple.com",
                "127.0.0.1"
            ]),
            vec!["api.exemple.com", "*.cdn.exemple.com", "127.0.0.1"]
        );
        // Ni joker au milieu, ni schéma, ni chemin, ni port, ni joker seul.
        assert!(
            liste(&[
                "*",
                "*.",
                "a.*.com",
                "https://x.com",
                "x.com/a",
                "x.com:443",
                ".x.com",
                ""
            ])
            .is_empty()
        );
    }

    #[test]
    fn un_domaine_non_autorise_est_refuse() {
        let d = liste(&["api.exemple.com", "*.cdn.exemple.com", "127.0.0.1"]);
        assert_eq!(
            verifier_l_adresse("https://api.exemple.com/v1/search?q=a", &d),
            Ok("api.exemple.com".to_string())
        );
        assert_eq!(
            verifier_l_adresse("http://127.0.0.1:43210/catalogue", &d),
            Ok("127.0.0.1".to_string())
        );
        assert!(verifier_l_adresse("https://a.b.cdn.exemple.com/f.mp3", &d).is_ok());

        for (url, code) in [
            ("https://evil.com/", "domain_not_allowed"),
            ("https://exemple.com/", "domain_not_allowed"),
            ("https://cdn.exemple.com/", "domain_not_allowed"),
            ("https://api.exemple.com.evil.com/", "domain_not_allowed"),
            ("https://evilapi.exemple.com/", "domain_not_allowed"),
            ("http://127.0.0.2/", "domain_not_allowed"),
            ("http://localhost/", "domain_not_allowed"),
            ("https://api.exemple.com@evil.com/", "credentials_in_url"),
            ("https://user:pw@api.exemple.com/", "credentials_in_url"),
            ("file:///etc/passwd", "scheme_not_allowed"),
            ("ftp://api.exemple.com/", "scheme_not_allowed"),
            ("pas une url", "invalid_url"),
        ] {
            assert_eq!(
                verifier_l_adresse(url, &d).map_err(|r| r.code),
                Err(code),
                "{url}"
            );
        }
    }

    #[test]
    fn sans_liste_tout_est_refuse() {
        assert_eq!(
            verifier_l_adresse("https://api.exemple.com/", &[]).map_err(|r| r.code),
            Err("domain_not_allowed")
        );
    }
}
