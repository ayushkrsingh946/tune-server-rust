//! « Pas pour moi » — les suggestions d'accueil écartées, PAR PROFIL
//! (renesenses/tune-web-client#916, FabienM, fil forum 1527).
//!
//! FabienM, 23/08/2026 : « Une découverte — une seule proposition, grande
//! pochette, avec les deux boutons "Écouter" / "Pas pour moi" ». Go de
//! Bertrand le 12/10/2026.
//!
//! # Ce qu'on écarte
//!
//! Trois sortes d'objets, et rien d'autre :
//!
//! | `item_type` | `source`              | `item_id`                       |
//! |-------------|-----------------------|---------------------------------|
//! | `album`     | `local`               | l'`albums.id` de la bibliothèque |
//! | `album`     | un service (`qobuz`…) | son `source_id` chez le service  |
//! | `playlist`  | un service            | son `source_id` chez le service  |
//! | `artist`    | `nom`                 | le NOM normalisé ([`nom_normalise`]) |
//!
//! Un artiste se désigne par son NOM, et non par un identifiant : les rangées
//! qui le proposent (« Nouveautés de vos artistes », recommandations) n'en
//! portent pas d'autre, et un artiste écarté doit l'être chez TOUS les
//! services à la fois — « Miles Davis, pas pour moi » ne veut pas dire « pas
//! sur Qobuz ».
//!
//! Une playlist LOCALE ne s'écarte pas : aucune rangée de suggestion n'en
//! propose ; c'est l'auditeur qui les a faites.
//!
//! # Par profil, écrit ET lu
//!
//! Le profil est celui de l'action (`X-Profile-Id`). Les rangées de
//! recommandation le lisent pour se filtrer : un autre profil de la maison
//! voit toujours l'élément. C'était l'objection n° 2 du dossier d'instruction
//! de web#916 (27/08) : un « Pas pour moi » global masquerait la proposition
//! pour toute la maisonnée.
//!
//! # Ce n'est NI un masquage NI un bannissement
//!
//! `hidden_items` retire un album de la BIBLIOTHÈQUE ; `streaming_hidden_items`
//! empêche un TITRE d'être joué automatiquement. Ici, rien ne disparaît
//! ailleurs que dans les rangées de SUGGESTION de l'accueil : l'album reste
//! dans la bibliothèque, se cherche, se joue, s'ouvre. D'où une table à part
//! plutôt qu'un `item_type` de plus dans `hidden_items` — le jour où un
//! filtre de bibliothèque lirait `hidden_items` sans regarder le type, un
//! « pas pour moi » ferait disparaître un disque qu'on possède.
//!
//! # L'instantané d'affichage
//!
//! `title`, `subtitle` et `cover_url` sont figés à l'écart : la liste des
//! Réglages (« Suggestions écartées ») doit pouvoir dire QUOI a été écarté
//! sans réinterroger un service, ni retrouver un album dont l'id aurait
//! changé.

use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::info;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

/// Un album, local ou de service.
pub const TYPE_ALBUM: &str = "album";
/// Un artiste, désigné par son nom normalisé.
pub const TYPE_ARTISTE: &str = "artist";
/// Une playlist de SERVICE.
pub const TYPE_PLAYLIST: &str = "playlist";

/// La source d'un album de la bibliothèque.
pub const SOURCE_LOCALE: &str = "local";
/// La « source » d'un artiste : il est désigné par son nom, chez tous les
/// services à la fois.
pub const SOURCE_NOM: &str = "nom";

/// Le nom d'un artiste tel qu'on le compare : minuscules, espaces réduits.
///
/// Volontairement modeste. Chaque règle ajoutée ici élargit ce qu'un seul
/// clic retire de l'accueil ; retirer à tort « The Doors » parce qu'on a
/// écarté « Doors » serait pire que de laisser passer une graphie voisine.
pub fn nom_normalise(nom: &str) -> String {
    nom.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Une suggestion écartée — ce que la route reçoit et ce qu'elle rend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestionEcartee {
    pub item_type: String,
    /// `local`, un service en minuscules, ou `nom` pour un artiste. Le client
    /// peut l'omettre pour un artiste.
    #[serde(default)]
    pub source: String,
    /// Identifiant dans l'espace de `source` ; pour un artiste, son nom.
    pub item_id: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub cover_url: Option<String>,
    /// Posé par la base ; ignoré à l'écriture.
    #[serde(default, skip_deserializing)]
    pub created_at: Option<String>,
}

/// Valide et met en forme une désignation. `None` = désignation refusée
/// (type inconnu, identifiant vide, playlist locale, album local dont l'id
/// n'est pas un entier) : la route rend 400, rien n'est écrit.
pub fn normaliser(s: &SuggestionEcartee) -> Option<SuggestionEcartee> {
    let item_type = s.item_type.trim().to_lowercase();
    let source = s.source.trim().to_lowercase();
    let brut = s.item_id.trim();
    if brut.is_empty() {
        return None;
    }
    let (source, item_id) = match item_type.as_str() {
        TYPE_ARTISTE => {
            let nom = nom_normalise(brut);
            if nom.is_empty() {
                return None;
            }
            (SOURCE_NOM.to_string(), nom)
        }
        TYPE_ALBUM if source.is_empty() || source == SOURCE_LOCALE => {
            // Un album local se désigne par un entier positif — l'id que lit
            // la rangée. Une chaîne qui n'en est pas un ne désignerait rien.
            let id: i64 = brut.parse().ok().filter(|n| *n > 0)?;
            (SOURCE_LOCALE.to_string(), id.to_string())
        }
        TYPE_ALBUM => (source, brut.to_string()),
        TYPE_PLAYLIST if source.is_empty() || source == SOURCE_LOCALE || source == SOURCE_NOM => {
            return None;
        }
        TYPE_PLAYLIST => (source, brut.to_string()),
        _ => return None,
    };
    if source == SOURCE_NOM && item_type != TYPE_ARTISTE {
        return None;
    }
    let propre = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    Some(SuggestionEcartee {
        item_type,
        source,
        item_id,
        title: propre(&s.title).or_else(|| {
            (s.item_type.trim().eq_ignore_ascii_case(TYPE_ARTISTE)).then(|| brut.to_string())
        }),
        subtitle: propre(&s.subtitle),
        cover_url: propre(&s.cover_url),
        created_at: None,
    })
}

/// Tout ce qu'un profil a écarté, en mémoire, pour filtrer une rangée sans
/// une requête par élément. La liste d'un profil se compte en dizaines.
#[derive(Debug, Default, Clone)]
pub struct Ecartees {
    albums_locaux: HashSet<i64>,
    /// `(item_type, source, item_id)` des objets de service.
    distants: HashSet<(String, String, String)>,
    artistes: HashSet<String>,
}

impl Ecartees {
    pub fn est_vide(&self) -> bool {
        self.albums_locaux.is_empty() && self.distants.is_empty() && self.artistes.is_empty()
    }

    /// Combien d'objets sont écartés — la marge qu'une rangée demande en plus
    /// à sa requête pour rester pleine après filtrage.
    pub fn len(&self) -> usize {
        self.albums_locaux.len() + self.distants.len() + self.artistes.len()
    }

    pub fn album_local(&self, id: i64) -> bool {
        self.albums_locaux.contains(&id)
    }

    pub fn album_de_service(&self, source: &str, source_id: &str) -> bool {
        self.distant(TYPE_ALBUM, source, source_id)
    }

    pub fn playlist_de_service(&self, source: &str, source_id: &str) -> bool {
        self.distant(TYPE_PLAYLIST, source, source_id)
    }

    pub fn artiste(&self, nom: &str) -> bool {
        !self.artistes.is_empty() && self.artistes.contains(&nom_normalise(nom))
    }

    fn distant(&self, item_type: &str, source: &str, source_id: &str) -> bool {
        !self.distants.is_empty()
            && self.distants.contains(&(
                item_type.to_string(),
                source.trim().to_lowercase(),
                source_id.trim().to_string(),
            ))
    }

    fn ajouter(&mut self, s: &SuggestionEcartee) {
        match (s.item_type.as_str(), s.source.as_str()) {
            (TYPE_ARTISTE, _) => {
                self.artistes.insert(nom_normalise(&s.item_id));
            }
            (TYPE_ALBUM, SOURCE_LOCALE) => {
                if let Ok(id) = s.item_id.parse() {
                    self.albums_locaux.insert(id);
                }
            }
            _ => {
                self.distants
                    .insert((s.item_type.clone(), s.source.clone(), s.item_id.clone()));
            }
        }
    }
}

mod sql {
    use super::SqlDialect;

    /// Réécarter rafraîchit l'instantané sans toucher à la date du premier
    /// écart.
    pub fn ecarter<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO suggestions_ecartees \
             (profile_id, item_type, source, item_id, title, subtitle, cover_url, created_at) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT (profile_id, item_type, source, item_id) DO UPDATE SET \
             title = COALESCE(excluded.title, suggestions_ecartees.title), \
             subtitle = COALESCE(excluded.subtitle, suggestions_ecartees.subtitle), \
             cover_url = COALESCE(excluded.cover_url, suggestions_ecartees.cover_url)",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.now_iso8601(),
        )
    }

    pub fn retablir<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM suggestions_ecartees \
             WHERE profile_id = {} AND item_type = {} AND source = {} AND item_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
        )
    }

    pub fn lister<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT item_type, source, item_id, title, subtitle, cover_url, created_at \
             FROM suggestions_ecartees WHERE profile_id = {} \
             ORDER BY created_at DESC, item_type ASC, source ASC, item_id ASC",
            d.placeholder(1),
        )
    }
}

pub struct SuggestionsEcarteesRepo {
    db: Arc<dyn DbBackend>,
}

impl SuggestionsEcarteesRepo {
    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    fn dialect_sql<F1, F2>(&self, sqlite: F1, postgres: F2) -> String
    where
        F1: FnOnce(&SqliteDialect) -> String,
        F2: FnOnce(&PostgresDialect) -> String,
    {
        match self.db.engine() {
            Engine::Sqlite => sqlite(&SqliteDialect),
            Engine::Postgres => postgres(&PostgresDialect),
        }
    }

    /// Écarte une suggestion pour ce profil. `Ok(None)` = désignation refusée
    /// (voir [`normaliser`]) : rien n'est écrit. Idempotent.
    pub fn ecarter(
        &self,
        profile_id: i64,
        s: &SuggestionEcartee,
    ) -> Result<Option<SuggestionEcartee>, String> {
        let Some(n) = normaliser(s) else {
            return Ok(None);
        };
        let sql = self.dialect_sql(sql::ecarter, sql::ecarter);
        let params: [&dyn ToSqlValue; 7] = [
            &profile_id,
            &n.item_type,
            &n.source,
            &n.item_id,
            &n.title,
            &n.subtitle,
            &n.cover_url,
        ];
        self.db.execute(&sql, &params)?;
        info!(profile_id, item_type = %n.item_type, source = %n.source, item_id = %n.item_id, "suggestion_ecartee");
        Ok(Some(n))
    }

    /// Rétablit une suggestion. `Ok(false)` = rien n'était écarté (ou
    /// désignation refusée). Le geste « Annuler » du message et le bouton des
    /// Réglages passent tous deux ici.
    pub fn retablir(&self, profile_id: i64, s: &SuggestionEcartee) -> Result<bool, String> {
        let Some(n) = normaliser(s) else {
            return Ok(false);
        };
        let sql = self.dialect_sql(sql::retablir, sql::retablir);
        let params: [&dyn ToSqlValue; 4] = [&profile_id, &n.item_type, &n.source, &n.item_id];
        let lignes = self.db.execute(&sql, &params)?;
        if lignes > 0 {
            info!(profile_id, item_type = %n.item_type, source = %n.source, item_id = %n.item_id, "suggestion_retablie");
        }
        Ok(lignes > 0)
    }

    /// Tout ce que ce profil a écarté, le plus récent d'abord.
    pub fn lister(&self, profile_id: i64) -> Result<Vec<SuggestionEcartee>, String> {
        let sql = self.dialect_sql(sql::lister, sql::lister);
        let params: [&dyn ToSqlValue; 1] = [&profile_id];
        Ok(self
            .db
            .query_many(&sql, &params)?
            .iter()
            .filter_map(|r| {
                let texte = |i: usize| r.get(i).and_then(|v| v.as_string());
                Some(SuggestionEcartee {
                    item_type: texte(0)?,
                    source: texte(1)?,
                    item_id: texte(2)?,
                    title: texte(3),
                    subtitle: texte(4),
                    cover_url: texte(5),
                    created_at: texte(6),
                })
            })
            .collect())
    }

    /// L'ensemble en mémoire que lisent les rangées. Une erreur de lecture
    /// rend un ensemble VIDE : une rangée qui ne filtre pas vaut mieux qu'un
    /// accueil vide.
    pub fn ensemble(&self, profile_id: i64) -> Ecartees {
        let mut e = Ecartees::default();
        match self.lister(profile_id) {
            Ok(liste) => liste.iter().for_each(|s| e.ajouter(s)),
            Err(err) => tracing::warn!(profile_id, error = %err, "suggestions_ecartees_illisibles"),
        }
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations::run_migrations;
    use crate::db::sqlite::SqliteDb;

    fn repo() -> SuggestionsEcarteesRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        SuggestionsEcarteesRepo::with_backend(Arc::new(db))
    }

    fn s(item_type: &str, source: &str, item_id: &str) -> SuggestionEcartee {
        SuggestionEcartee {
            item_type: item_type.into(),
            source: source.into(),
            item_id: item_id.into(),
            title: Some("Titre".into()),
            subtitle: None,
            cover_url: None,
            created_at: None,
        }
    }

    #[test]
    fn les_designations_absurdes_sont_refusees() {
        assert!(normaliser(&s("album", "local", "abc")).is_none());
        assert!(normaliser(&s("album", "local", "0")).is_none());
        assert!(normaliser(&s("album", "", "  ")).is_none());
        assert!(normaliser(&s("playlist", "local", "3")).is_none());
        assert!(normaliser(&s("playlist", "", "3")).is_none());
        assert!(normaliser(&s("track", "qobuz", "3")).is_none());
        assert!(normaliser(&s("album", "nom", "x")).is_none());
        let a = normaliser(&s("artist", "", "  Miles   DAVIS ")).unwrap();
        assert_eq!(
            (a.source.as_str(), a.item_id.as_str()),
            ("nom", "miles davis")
        );
        let q = normaliser(&s("album", " Qobuz", " 0060253 ")).unwrap();
        assert_eq!(
            (q.source.as_str(), q.item_id.as_str()),
            ("qobuz", "0060253")
        );
    }

    #[test]
    fn ecarter_lister_retablir_par_profil() {
        let r = repo();
        assert!(r.ecarter(1, &s("album", "local", "42")).unwrap().is_some());
        // Deux fois : pas de doublon.
        assert!(r.ecarter(1, &s("album", "local", "42")).unwrap().is_some());
        assert!(
            r.ecarter(1, &s("artist", "", "Miles Davis"))
                .unwrap()
                .is_some()
        );
        assert!(
            r.ecarter(1, &s("playlist", "qobuz", "pl-9"))
                .unwrap()
                .is_some()
        );
        assert_eq!(r.lister(1).unwrap().len(), 3);
        assert!(
            r.lister(2).unwrap().is_empty(),
            "le profil 2 n'a rien écarté"
        );

        let e = r.ensemble(1);
        assert!(e.album_local(42) && !e.album_local(43));
        assert!(e.artiste("MILES  davis"));
        assert!(e.playlist_de_service("Qobuz", "pl-9"));
        assert!(
            !e.album_de_service("qobuz", "pl-9"),
            "une playlist n'est pas un album"
        );
        assert!(r.ensemble(2).est_vide());

        assert!(r.retablir(1, &s("album", "local", "42")).unwrap());
        assert!(!r.retablir(1, &s("album", "local", "42")).unwrap());
        assert!(!r.ensemble(1).album_local(42));
    }
}
