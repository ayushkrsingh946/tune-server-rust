//! Multi-artist credit splitting.
//!
//! Concatenated performer credits — common in jazz/classical, e.g.
//! `"Antonio Vivaldi; Alfia Bakieva, Les Musiciennes du Concert des Nations,
//! Jordi Savall"` — currently produce ONE junk `artists` row per credit string,
//! which no image/bio source (mozaiklabs, MusicBrainz, Fanart…) can match.
//!
//! This module splits a raw credit string into individual artist names, while
//! carefully NOT splitting legitimate names that contain a separator
//! (`Simon & Garfunkel`, `Earth, Wind & Fire`, `AC/DC`, …).
//!
//! Phase 0 wires only the read-only analysis (`analyze_artist_credit`) into a
//! dry-run preview endpoint so we can measure the real separator distribution
//! on live libraries before changing any scan/DB behaviour.

use crate::db::engine::fold_diacritics;

/// Legitimate group/artist names that contain a "risky" separator (`,`, `&`,
/// `+`) and must NOT be split. Compared case-insensitively and accent-folded.
/// Seed list; extended at runtime via the `artist_split_allowlist` setting.
pub const KNOWN_GROUP_NAMES: &[&str] = &[
    "simon & garfunkel",
    "earth, wind & fire",
    "crosby, stills & nash",
    "crosby, stills, nash & young",
    "hootie & the blowfish",
    "ac/dc",
    "hall & oates",
    "daryl hall & john oates",
    "blood, sweat & tears",
    "emerson, lake & palmer",
    "kool & the gang",
    "sly & the family stone",
    "mumford & sons",
    "florence + the machine",
    "derek & the dominos",
    "the mamas & the papas",
    "peter, paul and mary",
    "huey lewis & the news",
    "gladys knight & the pips",
    "diana ross & the supremes",
    "martha & the vandellas",
    "bob marley & the wailers",
    "tom petty & the heartbreakers",
    "nick cave & the bad seeds",
    "echo & the bunnymen",
    "booker t & the mgs",
    "prince & the revolution",
    "katrina & the waves",
    "kc & the sunshine band",
    "buddy holly & the crickets",
    "joan jett & the blackhearts",
];

/// Leading articles used to (a) undo the "sort form" `Beatles, The` → `The
/// Beatles`, and (b) keep `X & The/His/Her Y` band names together instead of
/// splitting off the ensemble. Measured on a real library, `, The` sort forms
/// were ~49% of the comma "splits" (false positives).
const ARTICLES: &[&str] = &[
    "the", "his", "her", "their", "les", "los", "las", "die", "das", "der", "la", "le", "el", "il",
    "gli", "i",
];

/// Strong separators — these essentially never occur inside a single legitimate
/// artist name, so we always split on them (case-insensitive, space-padded so
/// `ft.` doesn't match inside a word).
/// Generational and honorific suffixes that follow a comma but belong to the
/// SAME person: `"Grover Washington, Jr."` is one artist, not two. Same family
/// as the `"X, The"` sort form undone by [`reorder_sort_form`].
const NAME_SUFFIXES: &[&str] = &[
    "jr", "jr.", "sr", "sr.", "jnr", "jnr.", "snr", "snr.", "ii", "iii", "iv",
];

const STRONG_MARKERS: &[&str] = &[
    " featuring ",
    " feat. ",
    " feat ",
    " ft. ",
    " ft ",
    " with ",
    " vs. ",
    " vs ",
];

/// The separator tier that produced a token boundary — for telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Separator {
    Semicolon,
    Feat,
    Comma,
    Ampersand,
}

impl Separator {
    pub fn as_str(self) -> &'static str {
        match self {
            Separator::Semicolon => ";",
            Separator::Feat => "feat",
            Separator::Comma => ",",
            Separator::Ampersand => "&",
        }
    }
}

/// Dry-run analysis of a credit string (read-only; no DB, no side effects).
#[derive(Debug, Clone)]
pub struct SplitAnalysis {
    pub original: String,
    /// Split result (== `[original]` when nothing splits).
    pub tokens: Vec<String>,
    /// Which separators contributed to the split.
    pub separators: Vec<Separator>,
    /// True when the whole string matched the allowlist (kept intact).
    pub allowlisted: bool,
}

impl SplitAnalysis {
    pub fn would_split(&self) -> bool {
        self.tokens.len() > 1
    }
}

/// Whether `name` is a known legitimate group name that must not be split.
pub fn is_allowlisted(name: &str, extra: &[String]) -> bool {
    let key = fold_diacritics(name.trim()).to_lowercase();
    KNOWN_GROUP_NAMES.iter().any(|g| *g == key)
        || extra
            .iter()
            .any(|g| fold_diacritics(g.trim()).to_lowercase() == key)
}

/// Case-insensitive `replace`.
///
/// #5236 — la comparaison se fait CARACTÈRE PAR CARACTÈRE, sur `haystack`
/// seul. L'ancienne version indexait `haystack.to_lowercase()` avec des
/// positions en octets de `haystack` : dès qu'une minuscule change de taille
/// (« İ » → « i̇ », 2 → 3 octets ; « ẞ » → « ß », 3 → 2), les positions
/// tombaient au milieu d'un caractère et le serveur paniquait au démarrage
/// (passe `recalculer_les_compilations`, v0.9.166).
fn replace_ci(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        let rest = &haystack[i..];
        if let Some(n) = prefix_len_ci(rest, needle) {
            out.push_str(replacement);
            i += n;
        } else {
            // `i` est toujours une frontière de caractère de `haystack`.
            let Some(ch) = rest.chars().next() else { break };
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Longueur en octets, DANS `hay`, du préfixe de `hay` égal à `needle` sans
/// tenir compte de la casse (caractère par caractère), ou `None`.
fn prefix_len_ci(hay: &str, needle: &str) -> Option<usize> {
    let mut chars = hay.char_indices();
    for nc in needle.chars() {
        let (_, hc) = chars.next()?;
        if hc != nc && !hc.to_lowercase().eq(nc.to_lowercase()) {
            return None;
        }
    }
    Some(chars.next().map_or(hay.len(), |(j, _)| j))
}

/// Whether `s` begins with a leading article (`the`, `his`, `les`, …).
fn starts_with_article(s: &str) -> bool {
    let l = s.trim().to_lowercase();
    ARTICLES
        .iter()
        .any(|a| l == *a || l.starts_with(&format!("{a} ")))
}

/// Undo a trailing "sort form": `"Beatles, The"` → `"The Beatles"`. Only fires
/// when the text after the LAST comma is exactly an article.
fn reorder_sort_form(name: &str) -> String {
    if let Some((head, tail)) = name.rsplit_once(',') {
        let t = tail.trim();
        if !t.is_empty() && ARTICLES.contains(&t.to_lowercase().as_str()) {
            return format!("{} {}", t, head.trim());
        }
    }
    name.to_string()
}

/// Re-attach a bare generational suffix to the part before it, mirroring how
/// [`split_ampersand`] re-attaches an article-led ensemble to its leader.
fn reattach_name_suffixes(parts: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        let key = p.trim().to_lowercase();
        if NAME_SUFFIXES.contains(&key.as_str())
            && let Some(last) = out.last_mut()
        {
            *last = format!("{last}, {}", p.trim());
            continue;
        }
        out.push(p);
    }
    out
}

/// Split on ` & ` / ` and `, but keep `X & The/His/Her Y` together (band name:
/// "Count Basie & His Orchestra", "Art Blakey & The Jazz Messengers"). Returns
/// `(parts, did_split)`.
fn split_ampersand(s: &str) -> (Vec<String>, bool) {
    let norm = replace_ci(s, " and ", " & ");
    if !norm.contains(" & ") {
        return (vec![s.trim().to_string()], false);
    }
    let mut out: Vec<String> = Vec::new();
    let mut did = false;
    for part in norm.split(" & ") {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if let Some(last) = out.last_mut()
            && starts_with_article(p)
        {
            // Re-attach the ensemble to the leader: one performing entity.
            *last = format!("{last} & {p}");
            continue;
        }
        if !out.is_empty() {
            did = true;
        }
        out.push(p.to_string());
    }
    if out.is_empty() {
        out.push(s.trim().to_string());
    }
    (out, did)
}

/// Split a raw artist credit string into individual artist names, in credit
/// order. Returns `[raw]` (unchanged) when it must not split. `extra_allowlist`
/// comes from the `artist_split_allowlist` setting.
///
/// `split_risky` gates the `,` / `&` separators (Phase 0 uses `true` for the
/// dry-run so the preview shows their full effect; scan integration will gate
/// them behind a setting once the telemetry is understood).
pub fn analyze_artist_credit(
    raw: &str,
    extra_allowlist: &[String],
    split_risky: bool,
) -> SplitAnalysis {
    // Undo "Beatles, The" sort form up-front so the trailing article isn't
    // mistaken for a separate artist (the dominant comma false positive).
    let original = reorder_sort_form(raw.trim());
    let mut separators: Vec<Separator> = Vec::new();

    if original.is_empty() {
        return SplitAnalysis {
            original: raw.to_string(),
            tokens: vec![],
            separators,
            allowlisted: false,
        };
    }

    // Never split the compilation sentinel or an allowlisted legit name.
    if original.eq_ignore_ascii_case("various artists") || original.eq_ignore_ascii_case("va") {
        return SplitAnalysis {
            original: original.clone(),
            tokens: vec![original],
            separators,
            allowlisted: false,
        };
    }
    if is_allowlisted(&original, extra_allowlist) {
        return SplitAnalysis {
            original: original.clone(),
            tokens: vec![original],
            separators,
            allowlisted: true,
        };
    }

    // 1) Strong split: normalise `;` and the feat-markers to a sentinel.
    let mut work = original.replace(';', "\u{1}");
    if work.contains('\u{1}') {
        separators.push(Separator::Semicolon);
    }
    for m in STRONG_MARKERS {
        let replaced = replace_ci(&work, m, "\u{1}");
        if replaced != work {
            if !separators.contains(&Separator::Feat) {
                separators.push(Separator::Feat);
            }
            work = replaced;
        }
    }

    let strong_pieces: Vec<String> = work
        .split('\u{1}')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();

    // 2) Risky split (comma, then article-aware ampersand) on each strong piece,
    //    unless the piece itself is an allowlisted legit name.
    let mut tokens: Vec<String> = Vec::new();
    for piece in strong_pieces {
        if !split_risky || is_allowlisted(&piece, extra_allowlist) {
            tokens.push(piece);
            continue;
        }
        let comma_parts: Vec<String> = if piece.contains(',') {
            let parts = reattach_name_suffixes(
                piece
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            );
            // Une virgule qui ne separe rien (suffixe rattache) n'est pas un
            // separateur : ne pas l'inscrire, sinon le rapport ment.
            if parts.len() > 1 {
                separators.push(Separator::Comma);
            }
            parts
        } else {
            vec![piece.clone()]
        };
        for cp in comma_parts {
            if is_allowlisted(&cp, extra_allowlist) {
                tokens.push(cp);
                continue;
            }
            let (parts, did) = split_ampersand(&cp);
            if did && !separators.contains(&Separator::Ampersand) {
                separators.push(Separator::Ampersand);
            }
            for t in parts {
                // Drop empty / single-char noise tokens.
                if t.chars().count() >= 2 {
                    tokens.push(t);
                }
            }
        }
    }

    // De-dup while preserving order (case/accent-insensitive).
    let mut seen: Vec<String> = Vec::new();
    let mut deduped: Vec<String> = Vec::new();
    for t in tokens {
        let key = fold_diacritics(&t).to_lowercase();
        if !seen.contains(&key) {
            seen.push(key);
            deduped.push(t);
        }
    }

    if deduped.is_empty() {
        deduped.push(original.clone());
    }

    SplitAnalysis {
        original,
        tokens: deduped,
        separators,
        allowlisted: false,
    }
}

/// Individual artist names from a credit string (see [`analyze_artist_credit`]).
pub fn split_artist_credit(
    raw: &str,
    extra_allowlist: &[String],
    split_risky: bool,
) -> Vec<String> {
    analyze_artist_credit(raw, extra_allowlist, split_risky).tokens
}

/// The single artist name used for album grouping: the first token after a
/// split, or the whole string when nothing splits. Kept stable so album
/// grouping does not drift for legitimate single-artist tags.
pub fn primary_artist(raw: &str, extra_allowlist: &[String], split_risky: bool) -> String {
    analyze_artist_credit(raw, extra_allowlist, split_risky)
        .tokens
        .into_iter()
        .next()
        .unwrap_or_else(|| raw.trim().to_string())
}

/// Chantier « artistes multiples » (`docs/chantier-artistes-multiples.md`) —
/// les seuls séparateurs que le SCAN suit : `;`, « feat. » et ses variantes,
/// « / » entouré d'espaces (« AC/DC » n'en a pas), et « & ». La virgule,
/// « and », « with » et « vs » ne sont PAS suivis : trop de vrais noms les
/// portent.
const MARQUEURS_SURS_DU_SCAN: &[&str] = &[" featuring ", " feat. ", " feat ", " ft. ", " / "];

/// Les artistes d'une chaîne ARTIST, tels que le scan les relie à la piste.
///
/// Rend `[raw]` (débarrassé de la forme de tri « Beatles, The ») quand rien
/// ne se découpe. Ne découpe jamais :
/// * un nom de la liste d'exceptions ([`KNOWN_GROUP_NAMES`] et le réglage
///   `artist_split_allowlist`), entier ou en morceau ;
/// * « Various Artists » ;
/// * sur « & » un morceau qui porte une virgule (« Peter, Bjorn & John ») :
///   la virgule n'étant pas un séparateur sûr, le « & » d'un tel nom ne
///   l'est pas non plus ;
/// * « X & The/His/Her Y » : l'ensemble reste attaché à son chef.
pub fn decouper_pour_le_scan(raw: &str, extra_allowlist: &[String]) -> Vec<String> {
    let original = reorder_sort_form(raw.trim());
    if original.is_empty() {
        return Vec::new();
    }
    if original.eq_ignore_ascii_case("various artists")
        || original.eq_ignore_ascii_case("va")
        || is_allowlisted(&original, extra_allowlist)
    {
        return vec![original];
    }
    let mut travail = original.replace(';', "\u{1}");
    for m in MARQUEURS_SURS_DU_SCAN {
        travail = replace_ci(&travail, m, "\u{1}");
    }
    let mut noms: Vec<String> = Vec::new();
    for morceau in travail
        .split('\u{1}')
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        if is_allowlisted(morceau, extra_allowlist) || morceau.contains(',') {
            noms.push(morceau.to_string());
            continue;
        }
        let mut parties: Vec<String> = Vec::new();
        for partie in morceau
            .split(" & ")
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            match parties.last_mut() {
                Some(chef) if starts_with_article(partie) => {
                    *chef = format!("{chef} & {partie}");
                }
                _ => parties.push(partie.to_string()),
            }
        }
        noms.extend(parties);
    }
    let mut vus: Vec<String> = Vec::new();
    let mut uniques: Vec<String> = Vec::new();
    for nom in noms {
        if nom.chars().count() < 2 {
            continue;
        }
        let cle = fold_diacritics(&nom).to_lowercase();
        if !vus.contains(&cle) {
            vus.push(cle);
            uniques.push(nom);
        }
    }
    if uniques.is_empty() {
        uniques.push(original);
    }
    uniques
}

/// Les MBID d'artiste d'une balise `MUSICBRAINZ_ARTISTID` : une valeur, ou
/// plusieurs jointes par `;`, `/`, `,` ou le séparateur nul d'ID3v2.4 (un
/// MBID est un UUID, aucun de ces signes n'y figure). Les valeurs qui n'ont
/// pas la forme d'un UUID sont écartées.
pub fn mbids_de_la_balise(brut: Option<&str>) -> Vec<String> {
    brut.unwrap_or("")
        .split([';', '/', ',', '\0'])
        .map(str::trim)
        .filter(|v| v.len() == 36 && v.chars().all(|c| c.is_ascii_hexdigit() || c == '-'))
        .map(|v| v.to_ascii_lowercase())
        .collect()
}

/// Un artiste relié à une piste par le scan : son nom, son MBID quand la
/// balise en apporte un qui lui correspond, et son rang dans le crédit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtisteDeLaPiste {
    pub nom: String,
    pub mbid: Option<String>,
    pub rang: u32,
}

/// Le découpage du scan, MBID compris.
///
/// MusicBrainz l'emporte quand il parle :
/// * autant de MBID que de noms découpés : chaque nom reçoit le sien, dans
///   l'ordre du crédit (c'est ainsi que Picard les écrit) ;
/// * un seul MBID et `nom_du_mbid` (le nom que la base connaît déjà pour ce
///   MBID) égal à la chaîne entière : MusicBrainz dit que c'est UN artiste
///   (« Simon & Garfunkel » hors liste), rien n'est découpé ;
/// * sinon les noms découpés sont gardés sans MBID : un nombre de MBID qui
///   ne correspond pas ne permet pas de savoir lequel va avec quel nom.
pub fn artistes_de_la_piste(
    raw: &str,
    mbids: &[String],
    nom_du_mbid: Option<&str>,
    extra_allowlist: &[String],
) -> Vec<ArtisteDeLaPiste> {
    let entier = reorder_sort_form(raw.trim());
    let un_seul_artiste_pour_musicbrainz = mbids.len() == 1
        && nom_du_mbid.is_some_and(|n| {
            fold_diacritics(n.trim()).to_lowercase() == fold_diacritics(&entier).to_lowercase()
        });
    let noms = if un_seul_artiste_pour_musicbrainz {
        vec![entier]
    } else {
        decouper_pour_le_scan(raw, extra_allowlist)
    };
    let apparies = mbids.len() == noms.len();
    noms.into_iter()
        .enumerate()
        .map(|(i, nom)| ArtisteDeLaPiste {
            nom,
            mbid: apparies.then(|| mbids[i].clone()),
            rang: i as u32,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(s: &str) -> Vec<String> {
        decouper_pour_le_scan(s, &[])
    }

    #[test]
    fn le_scan_decoupe_sur_les_separateurs_surs() {
        assert_eq!(
            scan("The Beatles & Genesis"),
            vec!["The Beatles", "Genesis"]
        );
        assert_eq!(
            scan("Daft Punk feat. Pharrell Williams"),
            vec!["Daft Punk", "Pharrell Williams"]
        );
        assert_eq!(scan("Avicii FEAT Aloe Blacc"), vec!["Avicii", "Aloe Blacc"]);
        assert_eq!(
            scan("Miles Davis; John Coltrane"),
            vec!["Miles Davis", "John Coltrane"]
        );
        assert_eq!(scan("Moby / Gwen Stefani"), vec!["Moby", "Gwen Stefani"]);
        assert_eq!(scan("Jay-Z ft. Alicia Keys"), vec!["Jay-Z", "Alicia Keys"]);
    }

    #[test]
    fn le_scan_garde_les_exceptions_et_les_noms_risques() {
        for nom in [
            "Simon & Garfunkel",
            "Earth, Wind & Fire",
            "Crosby, Stills & Nash",
            "AC/DC",
            "Peter, Bjorn & John",
            "Art Blakey & The Jazz Messengers",
            "Duke Ellington & His Orchestra",
            "Various Artists",
            "Bob Marley and Peter Tosh",
            "Ella Fitzgerald with Chick Webb",
        ] {
            assert_eq!(scan(nom), vec![nom.to_string()], "{nom}");
        }
        assert_eq!(scan("Beatles, The"), vec!["The Beatles"]);
        assert_eq!(
            scan("Simon & Garfunkel feat. Paul Desmond"),
            vec!["Simon & Garfunkel", "Paul Desmond"]
        );
        assert_eq!(
            decouper_pour_le_scan("Tom & Jerry", &["tom & jerry".to_string()]),
            vec!["Tom & Jerry"],
            "le réglage étend la liste"
        );
        assert_eq!(scan("Genesis & genesis"), vec!["Genesis"], "doublon replié");
        assert!(scan("  ").is_empty());
    }

    #[test]
    fn les_mbid_de_la_balise() {
        let a = "b10bbbfc-cf9e-42e0-be17-e2c3e1d2600d";
        let b = "8e66ea2b-b57b-47d9-8df0-df4630aeb8e5";
        assert_eq!(mbids_de_la_balise(Some(&format!("{a}/{b}"))), vec![a, b]);
        assert_eq!(
            mbids_de_la_balise(Some(&format!("{a}; {}", b.to_uppercase()))),
            vec![a, b]
        );
        assert_eq!(
            mbids_de_la_balise(Some("pas un mbid")),
            Vec::<String>::new()
        );
        assert!(mbids_de_la_balise(None).is_empty());
    }

    #[test]
    fn musicbrainz_l_emporte_quand_il_parle() {
        let a = "b10bbbfc-cf9e-42e0-be17-e2c3e1d2600d".to_string();
        let b = "8e66ea2b-b57b-47d9-8df0-df4630aeb8e5".to_string();
        let deux =
            artistes_de_la_piste("The Beatles & Genesis", &[a.clone(), b.clone()], None, &[]);
        assert_eq!(deux.len(), 2);
        assert_eq!(deux[0].mbid.as_deref(), Some(a.as_str()));
        assert_eq!(deux[1].nom, "Genesis");
        assert_eq!(deux[1].mbid.as_deref(), Some(b.as_str()));
        assert_eq!(deux[1].rang, 1);
        // Un seul MBID, que la base connaît sous le nom entier : un artiste.
        let duo = artistes_de_la_piste("Tom & Jerry", &[a.clone()], Some("Tom & Jerry"), &[]);
        assert_eq!(duo.len(), 1);
        assert_eq!(duo[0].mbid.as_deref(), Some(a.as_str()));
        // Un seul MBID d'un AUTRE nom : découpé, sans MBID apparié.
        let mixte = artistes_de_la_piste("The Beatles & Genesis", &[a], Some("The Beatles"), &[]);
        assert_eq!(
            mixte.iter().map(|x| x.nom.as_str()).collect::<Vec<_>>(),
            ["The Beatles", "Genesis"]
        );
        assert!(mixte.iter().all(|x| x.mbid.is_none()));
    }

    fn split(s: &str) -> Vec<String> {
        split_artist_credit(s, &[], true)
    }

    #[test]
    fn splits_classical_concatenation() {
        assert_eq!(
            split(
                "Antonio Vivaldi; Alfia Bakieva, Les Musiciennes du Concert des Nations, Jordi Savall"
            ),
            vec![
                "Antonio Vivaldi",
                "Alfia Bakieva",
                "Les Musiciennes du Concert des Nations",
                "Jordi Savall"
            ]
        );
    }

    #[test]
    fn splits_ampersand_and_comma_mix() {
        assert_eq!(
            split("Jordi Savall, Les Musiciennes du Concert des Nations & Alfia Bakieva"),
            vec![
                "Jordi Savall",
                "Les Musiciennes du Concert des Nations",
                "Alfia Bakieva"
            ]
        );
    }

    #[test]
    fn splits_feat_variants() {
        assert_eq!(
            split("Miles Davis feat. John Coltrane"),
            vec!["Miles Davis", "John Coltrane"]
        );
        assert_eq!(
            split("Kendrick Lamar ft. SZA"),
            vec!["Kendrick Lamar", "SZA"]
        );
        assert_eq!(
            split("Santana featuring Rob Thomas"),
            vec!["Santana", "Rob Thomas"]
        );
    }

    #[test]
    fn keeps_allowlisted_group_names() {
        assert_eq!(split("Simon & Garfunkel"), vec!["Simon & Garfunkel"]);
        assert_eq!(split("Earth, Wind & Fire"), vec!["Earth, Wind & Fire"]);
        assert_eq!(
            split("Crosby, Stills & Nash"),
            vec!["Crosby, Stills & Nash"]
        );
        assert_eq!(
            split("Hootie & the Blowfish"),
            vec!["Hootie & the Blowfish"]
        );
    }

    #[test]
    fn never_splits_slash_or_various() {
        assert_eq!(split("AC/DC"), vec!["AC/DC"]);
        assert_eq!(split("Various Artists"), vec!["Various Artists"]);
    }

    #[test]
    fn single_artist_unchanged() {
        assert_eq!(split("Beatles"), vec!["Beatles"]);
        assert_eq!(split("Keith Jarrett"), vec!["Keith Jarrett"]);
    }

    #[test]
    fn empty_or_whitespace() {
        assert!(split("").is_empty());
        assert!(split("   ").is_empty());
    }

    #[test]
    fn primary_is_first_token() {
        assert_eq!(
            primary_artist("Miles Davis feat. John Coltrane", &[], true),
            "Miles Davis"
        );
        assert_eq!(primary_artist("Keith Jarrett", &[], true), "Keith Jarrett");
        // Allowlisted stays whole.
        assert_eq!(
            primary_artist("Simon & Garfunkel", &[], true),
            "Simon & Garfunkel"
        );
    }

    #[test]
    fn risky_disabled_keeps_comma_groups() {
        // With risky splitting off, only strong separators split.
        assert_eq!(
            split_artist_credit("Herbie Hancock, Wayne Shorter", &[], false),
            vec!["Herbie Hancock, Wayne Shorter"]
        );
        assert_eq!(
            split_artist_credit("Miles Davis feat. John Coltrane", &[], false),
            vec!["Miles Davis", "John Coltrane"]
        );
    }

    #[test]
    fn extra_allowlist_is_honoured() {
        // A runtime-allowlisted name with a comma is kept whole.
        let extra = vec!["Sonny, Cher".to_string()];
        assert_eq!(
            split_artist_credit("Sonny, Cher", &extra, true),
            vec!["Sonny, Cher"]
        );
        // An unrelated name still splits on a strong separator.
        assert_eq!(
            split_artist_credit("Aaa feat. Bbb", &extra, true),
            vec!["Aaa", "Bbb"]
        );
    }

    #[test]
    fn tokens_are_trimmed_and_deduped() {
        assert_eq!(split("Bill Evans; Bill Evans"), vec!["Bill Evans"]);
    }

    // --- Regression tests from the real .18 library dry-run (Phase 0) ---

    #[test]
    fn sort_form_comma_the_is_reordered_not_split() {
        // "~49% of comma splits" were this pattern → junk "The" token.
        assert_eq!(split("Delfonics, The"), vec!["The Delfonics"]);
        assert_eq!(split("Grass Roots, The"), vec!["The Grass Roots"]);
        assert_eq!(split("Brothers Johnson, The"), vec!["The Brothers Johnson"]);
    }

    #[test]
    fn ampersand_ensemble_stays_together() {
        // "X & His/Her/The <ensemble>" is one performing entity, not two.
        assert_eq!(
            split("Count Basie & His Orchestra"),
            vec!["Count Basie & His Orchestra"]
        );
        assert_eq!(
            split("Art Blakey & The Jazz Messengers"),
            vec!["Art Blakey & The Jazz Messengers"]
        );
        assert_eq!(
            split("Bob Marley & The Wailers"),
            vec!["Bob Marley & The Wailers"]
        );
    }

    #[test]
    fn ampersand_two_people_still_splits() {
        assert_eq!(
            split("Ella Fitzgerald & Louis Armstrong"),
            vec!["Ella Fitzgerald", "Louis Armstrong"]
        );
        assert_eq!(
            split("Bill Evans & Jim Hall"),
            vec!["Bill Evans", "Jim Hall"]
        );
        assert_eq!(
            split("Duke Ellington & John Coltrane"),
            vec!["Duke Ellington", "John Coltrane"]
        );
    }

    #[test]
    fn mixed_comma_and_article_ampersand() {
        // Real: comma splits the leaders, ampersand-with-name splits the last two.
        assert_eq!(
            split("Jordi Savall, Les Musiciennes du Concert des Nations & Alfia Bakieva"),
            vec![
                "Jordi Savall",
                "Les Musiciennes du Concert des Nations",
                "Alfia Bakieva"
            ]
        );
    }

    /// #5236 — des caractères dont la minuscule n'a PAS la même taille en
    /// octets que la majuscule : « İ » (2 → 3), « ẞ » (3 → 2), « Ⱥ » (2 → 3),
    /// « Ω » (U+2126, 3 → 2), « K » (U+212A, 3 → 1).
    const TAILLE_CHANGEANTE: &[char] = &['İ', 'ẞ', 'Ⱥ', 'Ⱦ', '\u{2126}', '\u{212A}'];

    #[test]
    fn replace_ci_ne_panique_pas_quand_la_minuscule_change_de_taille() {
        assert_eq!(replace_ci("İlhan and Ömer", " and ", " & "), "İlhan & Ömer");
        assert_eq!(replace_ci("ẞéb AND Ⱥé", " and ", " & "), "ẞéb & Ⱥé");
        assert_eq!(
            replace_ci("İstanbul Devlet Senfoni Orkestrası", " and ", " & "),
            "İstanbul Devlet Senfoni Orkestrası"
        );
        assert_eq!(replace_ci("Ⱥé feat. İİ", " feat. ", "|"), "Ⱥé|İİ");
        // Aucun caractère n'est perdu ni dédoublé quand rien ne correspond.
        for &c in TAILLE_CHANGEANTE {
            let s = format!("{c}é{c} x {c}");
            assert_eq!(replace_ci(&s, " and ", " & "), s);
        }
    }

    #[test]
    fn le_decoupage_d_un_credit_turc_ou_allemand_ne_panique_pas() {
        assert_eq!(
            split("İstanbul Devlet Senfoni Orkestrası feat. Ⱥlpha"),
            vec!["İstanbul Devlet Senfoni Orkestrası", "Ⱥlpha"]
        );
        assert_eq!(split("İlhan and Ömer"), vec!["İlhan", "Ömer"]);
        assert_eq!(split("GROẞE Band & Ⱥnna"), vec!["GROẞE Band", "Ⱥnna"]);
    }

    /// Balayage : TOUT caractère du plan multilingue de base dont la minuscule
    /// change de taille, placé autour de chaque marqueur. Aucune panique.
    #[test]
    fn aucun_caractere_a_minuscule_de_taille_changeante_ne_fait_paniquer() {
        let mut vus = 0;
        for c in (0u32..0x1_0000).filter_map(char::from_u32) {
            let bas: usize = c.to_lowercase().map(char::len_utf8).sum();
            if bas == c.len_utf8() {
                continue;
            }
            vus += 1;
            for gabarit in [
                "{c}x and y{c}é",
                "{c}{c} feat. {c}é, {c}ö & The {c}",
                "é{c} vs {c}; {c} with {c}ä",
            ] {
                let s = gabarit.replace("{c}", &c.to_string());
                let _ = analyze_artist_credit(&s, &[], true);
                let _ = analyze_artist_credit(&s, &[], false);
            }
        }
        assert!(vus > 10, "le balayage doit trouver des cas ({vus})");
    }
}
