//! Le PÉRIMÈTRE des analyses de fond (#5593).
//!
//! Tades (1.0.0-rc1, 528 352 pistes, quatre racines dont un partage réseau)
//! trouvait la tâche « sans fin » : « je n'ai pas besoin que mon NAS soit
//! analysé ». Ce module permet d'**exclure des racines de bibliothèque** des
//! passes qui DÉCODENT : ReplayGain, plage dynamique, empreintes et CLAP.
//!
//! Un réglage, écrit par `PATCH /system/config` et publié par
//! `GET /system/config` :
//!
//! | Clé                                   | Valeur                     | Vide          |
//! |---------------------------------------|----------------------------|---------------|
//! | `background_analysis_excluded_roots`  | tableau de racines exclues | rien d'exclu  |
//! | `audio_embedding_genres_included`     | genres retenus pour le CLAP | tous les genres |
//! | `audio_embedding_genres_excluded`     | genres écartés du CLAP     | aucun écarté  |
//! | `background_analysis_excluded_genres` | genres écartés de TOUTES les passes | aucun écarté |
//!
//! Les deux listes de genres (#6198, Thierry, plus de 550 000 pistes) ne
//! touchent QUE la passe CLAP : ReplayGain, plage dynamique et empreintes
//! servent à la lecture de chaque piste, le CLAP seulement à la recherche par
//! ambiance. Une piste écartée n'est ni sélectionnée ni comptée : la passe ne
//! l'attend pas et se termine sans elle.
//!
//! # Dossiers et genres de toutes les passes (chantier « périmètre des analyses »)
//!
//! Décision de Bertrand du 13/10 : l'exclusion par DOSSIER et par GENRE vaut
//! pour les trois passes de fond (ReplayGain, plage dynamique, CLAP), et
//! les empreintes, qui décodent avec le ReplayGain, suivent comme pour les
//! racines. Voir `docs/chantier-perimetre-analyses.md`.
//!
//! * Un DOSSIER se règle dans `background_analysis_excluded_roots`, qui n'a
//!   jamais exigé une racine : toute entrée est un préfixe de chemin, borné
//!   par le séparateur. `Z:\NAS\Podcasts` exclut ce sous-dossier et lui seul.
//!   La clé garde son nom : les clients rc3 l'écrivent déjà.
//! * Un GENRE se règle dans `background_analysis_excluded_genres`, testé
//!   comme les genres du CLAP (colonne `genre` ou tableau `genres`, casse
//!   ignorée). Une piste SANS genre n'est jamais écartée par cette liste.
//!
//! Le genre se juge À L'ALBUM (décision du 13/10, [`termes_des_genres_a_l_album`]) :
//! un album dont le genre n'est pas exclu est analysé en entier, et son gain
//! d'album — calculé seulement quand TOUTES ses pistes ont leur gain de piste
//! (garde de complétude de `analyze_album_batch`) — reste calculable.
//!
//! Étape 2 : chaque passe peut avoir son périmètre ([`CLE_PAR_PASSE`],
//! [`perimetre_de`]) ; le commun vaut par défaut, et les empreintes suivent le
//! ReplayGain.
//!
//! Le réglage vit dans `settings`, à côté de `music_dirs` qui porte les racines
//! elles-mêmes : aucune table, donc aucune migration. Une racine retirée de
//! `music_dirs` laisse au pire une entrée qui ne correspond plus à aucun
//! chemin, sans effet.
//!
//! # Une CLAUSE, ajoutée au prédicat partagé — jamais une seconde requête
//!
//! Chaque passe a UN prédicat, partagé entre sa sélection et le compteur de sa
//! jauge (`CANDIDATS_RG_WHERE`, `CANDIDATS_DR_WHERE`,
//! `CANDIDATS_EMPREINTE_WHERE`, `ELIGIBLE_WHERE`). Le périmètre s'y ajoute en
//! fin de texte, au même endroit pour les deux : une piste exclue n'est donc
//! ni sélectionnée — donc jamais décodée — ni comptée. Une jauge qui
//! continuerait de la compter annoncerait du travail qui ne se fera jamais.
//!
//! Exclure ne JETTE rien : les valeurs déjà mesurées sur la racine restent en
//! base et servent à la lecture. Réinclure la racine reprend là où elle en
//! était.
//!
//! # Des littéraux, pas des `?`
//!
//! Les prédicats sont liés par position (`?` du seuil de report, du modèle, du
//! `LIMIT`). Ajouter des marqueurs décalerait tous les appelants, et leur
//! nombre varie avec le réglage. La clause porte donc ses valeurs en
//! LITTÉRAUX SQL, guillemets simples doublés : c'est la forme standard, comprise
//! à l'identique par SQLite et PostgreSQL (`standard_conforming_strings`, la
//! barre oblique inverse d'un chemin Windows n'y est pas un échappement), et la
//! traduction `?` → `$n` du moteur PostgreSQL saute l'intérieur des littéraux.
//!
//! Les racines se comparent par PRÉFIXE avec `substr`, pas avec `LIKE` : un `%`
//! ou un `_` dans un nom de dossier serait un joker. Le préfixe porte le
//! séparateur : exclure `/music` ne doit pas exclure `/music2`. Et les deux
//! côtés sont comparés séparateurs NORMALISÉS (`\` → `/`) : une base Windows
//! porte l'un ou l'autre selon le chemin d'arrivée de la piste (scan,
//! surveillance, import), à l'intérieur de la racine comme après elle.
use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Les racines de bibliothèque exclues des passes qui décodent.
pub const CLE_RACINES_EXCLUES: &str = "background_analysis_excluded_roots";

/// Les genres auxquels la passe CLAP est réservée (#6198). Vide : tous.
pub const CLE_GENRES_CLAP_INCLUS: &str = "audio_embedding_genres_included";

/// Les genres que la passe CLAP n'analyse pas (#6198). Vide : aucun.
pub const CLE_GENRES_CLAP_EXCLUS: &str = "audio_embedding_genres_excluded";

/// Les genres écartés de TOUTES les passes de fond : ReplayGain, plage
/// dynamique, empreintes et CLAP. Vide : aucun.
pub const CLE_GENRES_EXCLUS: &str = "background_analysis_excluded_genres";

/// Étape 2 — le périmètre PROPRE à une passe, par-dessus le périmètre commun.
/// Un objet `{ "<passe>": { "excluded_roots": [...], "excluded_genres": [...] } }`
/// (voir [`Passe::id`]). Un champ absent (ou `null`) HÉRITE : du périmètre
/// commun, ou du ReplayGain pour les empreintes. Un tableau, même vide,
/// REMPLACE la valeur héritée pour cette passe. Vide : tout hérite.
pub const CLE_PAR_PASSE: &str = "background_analysis_scope_by_pass";

/// Champ « dossiers exclus » d'une passe dans [`CLE_PAR_PASSE`].
pub const CHAMP_DOSSIERS: &str = "excluded_roots";

/// Champ « genres exclus » d'une passe dans [`CLE_PAR_PASSE`].
pub const CHAMP_GENRES: &str = "excluded_genres";

/// Les passes de fond qui ont un périmètre.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Passe {
    /// ReplayGain : gain de piste, gain d'album, crêtes, remesures.
    ReplayGain,
    /// La plage dynamique (DR).
    PlageDynamique,
    /// Les empreintes acoustiques. Héritent du ReplayGain (décision du 13/10).
    Empreintes,
    /// L'analyse acoustique (CLAP).
    Clap,
}

impl Passe {
    /// Toutes les passes, dans l'ordre de l'écran.
    pub const TOUTES: [Passe; 4] = [
        Passe::ReplayGain,
        Passe::PlageDynamique,
        Passe::Empreintes,
        Passe::Clap,
    ];

    /// Le nom de la passe dans [`CLE_PAR_PASSE`] et dans les réponses.
    pub fn id(self) -> &'static str {
        match self {
            Passe::ReplayGain => "replaygain",
            Passe::PlageDynamique => "dynamic_range",
            Passe::Empreintes => "fingerprints",
            Passe::Clap => "audio_embedding",
        }
    }

    /// L'inverse de [`Passe::id`].
    pub fn depuis_id(id: &str) -> Option<Passe> {
        Passe::TOUTES.into_iter().find(|p| p.id() == id)
    }

    /// De qui la passe hérite un champ qu'elle ne règle pas : `None` = le
    /// périmètre commun. Les empreintes décodent avec le ReplayGain et
    /// suivent son périmètre (décision de Bertrand du 13/10).
    fn heritee(self) -> Option<Passe> {
        match self {
            Passe::Empreintes => Some(Passe::ReplayGain),
            _ => None,
        }
    }

    /// Le chemin comparé aux dossiers exclus : le chemin OUVRABLE pour le
    /// CLAP (le support d'une piste de feuille CUE), `t.file_path` sinon.
    fn expr_chemin(self) -> &'static str {
        match self {
            Passe::Clap => crate::db::track_repo::sql::CHEMIN_OUVRABLE,
            _ => "t.file_path",
        }
    }
}

/// Le périmètre EN VIGUEUR d'une passe : ce qu'elle n'analyse pas.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Perimetre {
    /// Préfixes de chemin exclus.
    pub dossiers: Vec<String>,
    /// Genres exclus, jugés à l'album (voir [`termes_des_genres_a_l_album`]).
    pub genres: Vec<String>,
}

/// Borne du nombre d'entrées d'une liste : une clause de mille préfixes
/// coûterait à chaque `COUNT(*)` de la page État, pour un réglage qui en compte
/// une poignée.
pub const ENTREES_MAX: usize = 200;

/// Borne de longueur d'une entrée, en caractères.
pub const LONGUEUR_MAX: usize = 4096;

/// Lit une liste de chaînes en base. Absente, illisible ou d'un autre type :
/// vide — c'est-à-dire « rien d'exclu », le comportement d'avant le réglage.
fn liste(backend: &Arc<dyn DbBackend>, cle: &str) -> Vec<String> {
    let brut = SettingsRepo::with_backend(backend.clone())
        .get(cle)
        .ok()
        .flatten()
        .unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(&brut)
        .ok()
        .and_then(|v| normaliser(&v).ok())
        .unwrap_or_default()
}

/// Les racines exclues, telles que réglées.
pub fn racines_exclues(backend: &Arc<dyn DbBackend>) -> Vec<String> {
    liste(backend, CLE_RACINES_EXCLUES)
}

/// Les genres écartés de toutes les passes, tels que réglés. Vide : aucun.
pub fn genres_exclus(backend: &Arc<dyn DbBackend>) -> Vec<String> {
    liste(backend, CLE_GENRES_EXCLUS)
}

/// L'objet [`CLE_PAR_PASSE`] tel que rangé. Absent ou illisible : vide (tout
/// hérite du périmètre commun).
fn par_passe(backend: &Arc<dyn DbBackend>) -> serde_json::Map<String, serde_json::Value> {
    SettingsRepo::with_backend(backend.clone())
        .get(CLE_PAR_PASSE)
        .ok()
        .flatten()
        .and_then(|brut| serde_json::from_str::<serde_json::Value>(&brut).ok())
        .and_then(|v| normaliser_par_passe(&v).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// Le champ PROPRE d'une passe, s'il est réglé.
fn champ_propre(
    obj: &serde_json::Map<String, serde_json::Value>,
    passe: Passe,
    champ: &str,
) -> Option<Vec<String>> {
    let v = obj.get(passe.id())?.get(champ)?;
    if v.is_null() {
        return None;
    }
    normaliser(v).ok()
}

/// Résout le périmètre d'une passe : ses champs propres, sinon ceux dont elle
/// hérite.
fn resoudre(
    obj: &serde_json::Map<String, serde_json::Value>,
    passe: Passe,
    commun: &Perimetre,
) -> Perimetre {
    let base = match passe.heritee() {
        Some(p) => resoudre(obj, p, commun),
        None => commun.clone(),
    };
    Perimetre {
        dossiers: champ_propre(obj, passe, CHAMP_DOSSIERS).unwrap_or(base.dossiers),
        genres: champ_propre(obj, passe, CHAMP_GENRES).unwrap_or(base.genres),
    }
}

/// Le périmètre commun : [`CLE_RACINES_EXCLUES`] et [`CLE_GENRES_EXCLUS`].
pub fn perimetre_commun(backend: &Arc<dyn DbBackend>) -> Perimetre {
    Perimetre {
        dossiers: racines_exclues(backend),
        genres: genres_exclus(backend),
    }
}

/// Le périmètre en vigueur de `passe` (étape 2) : le commun, sauf là où la
/// passe a le sien.
pub fn perimetre_de(backend: &Arc<dyn DbBackend>, passe: Passe) -> Perimetre {
    resoudre(&par_passe(backend), passe, &perimetre_commun(backend))
}

/// Les réglages PROPRES de chaque passe, tels que rangés (pour le comptage).
pub fn reglages_par_passe(backend: &Arc<dyn DbBackend>) -> Vec<(Passe, &'static str, Vec<String>)> {
    let obj = par_passe(backend);
    let mut sortie = Vec::new();
    for passe in Passe::TOUTES {
        for champ in [CHAMP_DOSSIERS, CHAMP_GENRES] {
            if let Some(l) = champ_propre(&obj, passe, champ) {
                sortie.push((passe, champ, l));
            }
        }
    }
    sortie
}

/// La forme unique de [`CLE_PAR_PASSE`] : un objet dont les clés sont des
/// passes connues, chacune un objet aux seuls champs [`CHAMP_DOSSIERS`] et
/// [`CHAMP_GENRES`], chacun une liste normalisée par [`normaliser`]. Un
/// champ `null` est retiré (il hérite) ; une passe sans champ aussi. `null`
/// vaut l'objet vide. Toute autre forme est refusée.
pub fn normaliser_par_passe(valeur: &serde_json::Value) -> Result<serde_json::Value, String> {
    let obj = match valeur {
        serde_json::Value::Null => return Ok(serde_json::json!({})),
        serde_json::Value::Object(o) => o,
        autre => return Err(format!("un objet par passe est attendu, reçu {autre}")),
    };
    let mut sortie = serde_json::Map::new();
    for (cle, v) in obj {
        let Some(passe) = Passe::depuis_id(cle) else {
            return Err(format!(
                "passe inconnue « {cle} » (attendues : replaygain, dynamic_range, fingerprints, audio_embedding)"
            ));
        };
        let champs = match v {
            serde_json::Value::Null => continue,
            serde_json::Value::Object(c) => c,
            autre => return Err(format!("{cle} : un objet est attendu, reçu {autre}")),
        };
        let mut propre = serde_json::Map::new();
        for (champ, liste) in champs {
            if champ != CHAMP_DOSSIERS && champ != CHAMP_GENRES {
                return Err(format!(
                    "{cle} : champ inconnu « {champ} » (attendus : {CHAMP_DOSSIERS}, {CHAMP_GENRES})"
                ));
            }
            if liste.is_null() {
                continue;
            }
            let l = normaliser(liste).map_err(|e| format!("{cle}.{champ} : {e}"))?;
            propre.insert(champ.clone(), serde_json::json!(l));
        }
        if !propre.is_empty() {
            sortie.insert(passe.id().to_string(), serde_json::Value::Object(propre));
        }
    }
    Ok(serde_json::Value::Object(sortie))
}

/// Les genres retenus pour le CLAP, tels que réglés. Vide : tous.
pub fn genres_clap_inclus(backend: &Arc<dyn DbBackend>) -> Vec<String> {
    liste(backend, CLE_GENRES_CLAP_INCLUS)
}

/// Les genres écartés du CLAP, tels que réglés. Vide : aucun.
pub fn genres_clap_exclus(backend: &Arc<dyn DbBackend>) -> Vec<String> {
    liste(backend, CLE_GENRES_CLAP_EXCLUS)
}

/// La forme unique d'une liste du périmètre : un tableau de chaînes, rognées,
/// sans vide ni doublon, dans l'ordre reçu. `null` vaut la liste vide.
///
/// Sert à `PATCH /system/config`, qui REFUSE ce qu'elle refuse (400) au lieu de
/// laisser en base une valeur qui retomberait en silence sur « rien d'exclu ».
pub fn normaliser(valeur: &serde_json::Value) -> Result<Vec<String>, String> {
    let elements = match valeur {
        serde_json::Value::Null => return Ok(Vec::new()),
        serde_json::Value::Array(a) => a,
        autre => return Err(format!("un tableau de chaînes est attendu, reçu {autre}")),
    };
    if elements.len() > ENTREES_MAX {
        return Err(format!(
            "{} entrées, au plus {ENTREES_MAX} sont admises",
            elements.len()
        ));
    }
    let mut vues = std::collections::HashSet::new();
    let mut sortie = Vec::with_capacity(elements.len());
    for e in elements {
        let Some(s) = e.as_str() else {
            return Err(format!("entrée {e} : une chaîne est attendue"));
        };
        let s = s.trim();
        if s.is_empty() {
            continue;
        }
        if s.chars().count() > LONGUEUR_MAX {
            return Err(format!("entrée de plus de {LONGUEUR_MAX} caractères"));
        }
        if vues.insert(s.to_string()) {
            sortie.push(s.to_string());
        }
    }
    Ok(sortie)
}

/// Un littéral SQL : guillemets simples doublés. Voir l'en-tête du module.
fn litteral(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Un chemin, séparateurs normalisés : `\` devient `/`.
fn normaliser_les_separateurs(chemin: &str) -> String {
    chemin.replace('\\', "/")
}

/// `" AND NOT (…)"` : écarte les pistes dont `expr` (un chemin) est l'une des
/// `racines` ou se trouve dessous. Vide si aucune racine.
///
/// Les deux côtés sont comparés séparateurs normalisés (`\` → `/`) : une base
/// Windows porte l'un ou l'autre, y compris À L'INTÉRIEUR de la racine
/// (`Z:\NAS` réglé, `Z:/NAS/f.flac` en base). La racine est prise sans son
/// séparateur final ; `/` seul devient le préfixe vide suivi d'un séparateur :
/// tout chemin absolu.
///
/// La casse compte : la racine réglée vient de `music_dirs`, la même chaîne
/// que le scan a mise en tête des chemins.
pub fn clause_hors_racines(expr: &str, racines: &[String]) -> String {
    match termes_des_racines(expr, racines) {
        Some(termes) => format!(" AND NOT ({termes})"),
        None => String::new(),
    }
}

/// `" AND (…)"` : l'inverse exact de [`clause_hors_racines`], sur les MÊMES
/// termes — les pistes dont `expr` est l'une des `racines` ou se trouve
/// dessous. Vide si aucune racine.
///
/// Fil 2157 : une piste d'une racine exclue n'est candidate d'aucune passe,
/// mais elle reste « sans plage dynamique ». La compter à part, avec le texte
/// même qui l'écarte, est la seule façon qu'une jauge ne l'attende pas.
pub fn clause_dans_les_racines(expr: &str, racines: &[String]) -> String {
    match termes_des_racines(expr, racines) {
        Some(termes) => format!(" AND ({termes})"),
        None => String::new(),
    }
}

/// Les termes partagés par [`clause_hors_racines`] et
/// [`clause_dans_les_racines`], joints par `OR`. `None` si aucune racine.
fn termes_des_racines(expr: &str, racines: &[String]) -> Option<String> {
    let chemin = format!("REPLACE(COALESCE({expr}, ''), '\\', '/')");
    let termes: Vec<String> = racines
        .iter()
        .filter_map(|r| {
            let r = r.trim();
            if r.is_empty() {
                return None;
            }
            let normale = normaliser_les_separateurs(r);
            let base = normale.trim_end_matches('/');
            let n = base.chars().count() + 1;
            let mut t = format!(
                "substr({chemin}, 1, {n}) = {}",
                litteral(&format!("{base}/"))
            );
            if !base.is_empty() {
                t = format!("{chemin} = {} OR {t}", litteral(base));
            }
            Some(format!("({t})"))
        })
        .collect();
    if termes.is_empty() {
        return None;
    }
    Some(termes.join(" OR "))
}

/// Le test « la piste porte l'un des `genres` » : la colonne `t.genre`
/// comparée en entier, OU le tableau JSON `t.genres` — l'union même que teste
/// le filtre Genre des facettes (`TrackRepo::conditions_du_filtre`).
/// Insensible à la casse des deux côtés (`LOWER`), sur les deux moteurs.
/// `COALESCE` partout : le test vaut vrai ou faux, jamais NULL, ce qui rend
/// son `NOT` sûr. `None` si la liste est vide.
///
/// Le motif `LIKE` ne porte que des genres sans joker : un `%` ou un `_` dans
/// un nom de genre élargirait la recherche au tableau. Ces genres-là ne sont
/// cherchés que dans la colonne, par égalité.
fn termes_des_genres(genres: &[String]) -> Option<String> {
    termes_des_genres_sur("t", genres)
}

/// [`termes_des_genres`] sur la table d'alias `alias` (`t` : la piste, ou
/// l'album du test « à l'album »).
fn termes_des_genres_sur(alias: &str, genres: &[String]) -> Option<String> {
    let genres: Vec<&str> = genres
        .iter()
        .map(|g| g.trim())
        .filter(|g| !g.is_empty())
        .collect();
    if genres.is_empty() {
        return None;
    }
    let dans_la_colonne = genres
        .iter()
        .map(|g| format!("LOWER({})", litteral(g)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut termes = vec![format!(
        "LOWER(COALESCE({alias}.genre, '')) IN ({dans_la_colonne})"
    )];
    termes.extend(genres.iter().filter(|g| !g.contains(['%', '_'])).map(|g| {
        format!(
            "LOWER(COALESCE({alias}.genres, '')) LIKE LOWER({})",
            litteral(&format!("%\"{g}\"%"))
        )
    }));
    Some(termes.join(" OR "))
}

/// `" AND (…)"` : ne garde que les pistes portant l'un des `genres`. Vide si
/// aucun genre. Une piste SANS genre n'est donc pas retenue quand la liste est
/// réglée : c'est ce que l'écran du réglage dit à l'utilisateur.
pub fn clause_genres_inclus(genres: &[String]) -> String {
    match termes_des_genres(genres) {
        Some(t) => format!(" AND ({t})"),
        None => String::new(),
    }
}

/// `" AND NOT (…)"` : écarte les pistes portant l'un des `genres`. Vide si
/// aucun genre. Une piste sans genre reste analysée.
pub fn clause_genres_exclus(genres: &[String]) -> String {
    match termes_des_genres(genres) {
        Some(t) => format!(" AND NOT ({t})"),
        None => String::new(),
    }
}

/// Le test « la piste est d'un des `genres` », JUGÉ À L'ALBUM (décision de
/// Bertrand du 13/10) : une piste d'un album qui porte un genre (colonne
/// `albums.genre` ou tableau `albums.genres`) est jugée sur le genre de son
/// ALBUM — un album dont le genre n'est pas exclu est analysé EN ENTIER, et
/// son gain d'album reste calculable, même si une piste d'une compilation
/// porte un genre exclu. Une piste sans album, ou dont l'album ne porte aucun
/// genre, est jugée sur ses propres genres. Vrai ou faux, jamais NULL.
/// `None` si la liste est vide.
fn termes_des_genres_a_l_album(genres: &[String]) -> Option<String> {
    let piste = termes_des_genres_sur("t", genres)?;
    let album = termes_des_genres_sur("alb_p", genres)?;
    let a_un_genre = "(TRIM(COALESCE(alb_p.genre, '')) != '' \
                      OR TRIM(COALESCE(alb_p.genres, '')) NOT IN ('', '[]'))";
    Some(format!(
        "(EXISTS (SELECT 1 FROM albums alb_p WHERE alb_p.id = t.album_id AND {a_un_genre} AND ({album})) \
         OR (NOT EXISTS (SELECT 1 FROM albums alb_p WHERE alb_p.id = t.album_id AND {a_un_genre}) \
             AND ({piste})))"
    ))
}

/// `" AND NOT (…)"` : écarte les pistes des `genres`, jugés à l'album. Vide si
/// aucun genre.
fn clause_genres_exclus_a_l_album(genres: &[String]) -> String {
    match termes_des_genres_a_l_album(genres) {
        Some(t) => format!(" AND NOT ({t})"),
        None => String::new(),
    }
}

/// La clause du périmètre d'une passe qui décode (ReplayGain, plage
/// dynamique, empreintes) : ses dossiers exclus, sur `t.file_path`, puis ses
/// genres exclus, jugés à l'album. Le périmètre est celui de la passe
/// ([`perimetre_de`]) : le commun, sauf là où elle a le sien.
pub fn clause_decodage(backend: &Arc<dyn DbBackend>, passe: Passe) -> String {
    let p = perimetre_de(backend, passe);
    let mut c = clause_hors_racines(passe.expr_chemin(), &p.dossiers);
    c.push_str(&clause_genres_exclus_a_l_album(&p.genres));
    c
}

/// L'inverse EXACT de [`clause_decodage`] pour la même passe : `" AND (…)"`
/// sur les pistes d'un dossier exclu OU d'un genre écarté, sur les mêmes
/// termes. Vide si rien n'est exclu.
///
/// `NOT A AND NOT B` a pour complément `A OR B` : chaque piste est dans l'une
/// des deux clauses, jamais dans les deux (les termes valent vrai ou faux,
/// jamais NULL : `COALESCE` partout).
pub fn clause_hors_perimetre_decodage(backend: &Arc<dyn DbBackend>, passe: Passe) -> String {
    let p = perimetre_de(backend, passe);
    let termes: Vec<String> = [
        termes_des_racines(passe.expr_chemin(), &p.dossiers),
        termes_des_genres_a_l_album(&p.genres),
    ]
    .into_iter()
    .flatten()
    .map(|t| format!("({t})"))
    .collect();
    if termes.is_empty() {
        return String::new();
    }
    format!(" AND ({})", termes.join(" OR "))
}

/// La clause du périmètre du CLAP : son périmètre (dossiers sur le chemin
/// OUVRABLE, genres jugés à l'album), puis les genres retenus et écartés du
/// seul CLAP (#6198), jugés sur la piste. Une piste doit passer les quatre.
pub fn clause_clap(backend: &Arc<dyn DbBackend>) -> String {
    let mut c = clause_decodage(backend, Passe::Clap);
    c.push_str(&clause_genres_inclus(&genres_clap_inclus(backend)));
    c.push_str(&clause_genres_exclus(&genres_clap_exclus(backend)));
    c
}

/// Étape 3 — une entrée de périmètre et le nombre de pistes qu'elle retire.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Retrait {
    /// L'entrée telle que réglée.
    pub value: String,
    /// Combien de pistes (avec fichier) elle désigne, à elle seule.
    pub tracks: i64,
}

/// Le comptage d'une liste : dossiers (sur `expr`) ou genres (à l'album).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Retraits {
    /// Les dossiers exclus.
    pub excluded_roots: Vec<Retrait>,
    /// Les genres exclus.
    pub excluded_genres: Vec<Retrait>,
}

/// Étape 3 — combien de pistes chaque entrée retire : le périmètre commun, et
/// les réglages PROPRES de chaque passe. Une entrée compte les pistes qu'elle
/// désigne à elle seule, sur les termes mêmes des clauses (une piste désignée
/// par deux entrées compte dans les deux). Une entrée mal orthographiée se
/// voit : 0. Une seule requête par liste (un `SUM(CASE …)` par entrée), donc
/// un seul parcours de `tracks`. Synchrone : hors des fils async. Une erreur
/// de requête rend 0 partout, journalisée.
pub fn compter_les_retraits(backend: &Arc<dyn DbBackend>) -> (Retraits, Vec<(Passe, Retraits)>) {
    let commun = perimetre_commun(backend);
    let commun = Retraits {
        excluded_roots: compter(backend, "t.file_path", &commun.dossiers, true),
        excluded_genres: compter(backend, "t.file_path", &commun.genres, false),
    };
    let mut par_passe: Vec<(Passe, Retraits)> = Vec::new();
    for (passe, champ, liste) in reglages_par_passe(backend) {
        let est_dossier = champ == CHAMP_DOSSIERS;
        let comptes = compter(backend, passe.expr_chemin(), &liste, est_dossier);
        let entree = match par_passe.iter_mut().find(|(p, _)| *p == passe) {
            Some((_, r)) => r,
            None => {
                par_passe.push((passe, Retraits::default()));
                &mut par_passe.last_mut().expect("poussé").1
            }
        };
        if est_dossier {
            entree.excluded_roots = comptes;
        } else {
            entree.excluded_genres = comptes;
        }
    }
    (commun, par_passe)
}

fn compter(
    backend: &Arc<dyn DbBackend>,
    expr: &str,
    entrees: &[String],
    dossiers: bool,
) -> Vec<Retrait> {
    if entrees.is_empty() {
        return Vec::new();
    }
    let termes: Vec<String> = entrees
        .iter()
        .map(|e| {
            let une = std::slice::from_ref(e);
            let t = if dossiers {
                termes_des_racines(expr, une)
            } else {
                termes_des_genres_a_l_album(une)
            };
            match t {
                Some(t) => format!("COALESCE(SUM(CASE WHEN ({t}) THEN 1 ELSE 0 END), 0)"),
                None => "0".to_string(),
            }
        })
        .collect();
    let sql = format!(
        "SELECT {} FROM tracks t WHERE t.file_path IS NOT NULL AND t.file_path != ''",
        termes.join(", ")
    );
    let ligne = match backend.query_one(&sql, &[]) {
        Ok(l) => l.unwrap_or_default(),
        Err(e) => {
            tracing::warn!(erreur = %e, "perimetre_comptage_en_echec");
            Vec::new()
        }
    };
    entrees
        .iter()
        .enumerate()
        .map(|(i, e)| Retrait {
            value: e.clone(),
            tracks: ligne.get(i).and_then(|v| v.as_i64()).unwrap_or(0),
        })
        .collect()
}

#[cfg(test)]
#[path = "perimetre_tests.rs"]
mod tests;
