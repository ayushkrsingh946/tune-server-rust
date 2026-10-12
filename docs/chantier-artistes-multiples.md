# Chantier : artistes multiples dans ARTIST

Go de Bertrand du 12/10/2026, fil 2218 (Didier). « The Beatles & Genesis » doit
compter pour les deux artistes.

## Constat

- Une piste n'a qu'un artiste : `tracks.artist_id`. Le scan
  (`TrackImporter::import`, `tune-server/src/scan_import.rs`) y met l'artiste
  d'ALBUM, sauf pour une compilation ou un ALBUMARTIST neutre, où il met
  l'artiste de la piste. La chaîne ARTIST brute n'est pas conservée.
- `tune-core/src/metadata/artist_split.rs` sait découper un crédit, mais il ne
  sert qu'à l'aperçu `GET /system/artist-split-preview` et à la règle des
  compilations. Le scan ne s'en sert pas.
- Issues liées : #179 (proposition du forum, fermée sur la livraison de
  l'aperçu), #5236 (panique de `replace_ci`, corrigée), #6121 (règle « Artiste
  de l'album »), #3914 (pont Roon, consolidation des artistes).

## Conception

1. **On garde `tracks.artist_id`.** Il reste l'artiste principal. Toutes les
   pages, l'historique, les favoris et les règles existantes continuent de le
   lire.
2. **La table `track_artists` vient s'y ajouter** : `(track_id, artist_id,
   rang)`, clé `(track_id, artist_id)`, `ON DELETE CASCADE` des deux côtés, et
   un index sur `artist_id`. Elle contient tous les artistes d'un crédit
   découpé, dans l'ordre du crédit. Une piste dont le crédit ne se découpe pas
   n'y a aucune ligne. Ainsi, une bibliothèque déjà scannée reste juste sans
   rescan : son seul artiste est `artist_id`.
   - Migration SQLite **125**.
   - Migration PostgreSQL **089** (`migrations/postgres/089_track_artists.sql`).
   - Copiée à la bascule SQLite → PG (`MIGRATION_TABLES_APRES_SCRIPTS`).
3. **Le découpage se fait au scan, et seulement sur des séparateurs sûrs**
   (`artist_split::decouper_pour_le_scan`) :
   - Séparateurs suivis :
     - `;` ;
     - `feat.`, `feat`, `ft.`, `featuring` ;
     - ` / ` entouré d'espaces (« AC/DC » n'en a pas) ;
     - ` & `.
   - Séparateurs ignorés : la virgule, `and`, `with` et `vs`. Trop de vrais
     noms les portent.
   - Le « & » d'un morceau qui contient une virgule n'est pas suivi non plus
     (« Peter, Bjorn & John »).
   - « X & The/His/Her Y » reste attaché à son chef (« Duke Ellington & His
     Orchestra »).
   - « Beatles, The » est remis à l'endroit.
4. **Une liste d'exceptions.**
   - Elle contient `KNOWN_GROUP_NAMES` (« Simon & Garfunkel », « Earth, Wind &
     Fire », « Crosby, Stills & Nash »…).
   - Le réglage `artist_split_allowlist` (tableau JSON) l'étend, sans
     recompiler.
   - Un nom de la liste n'est jamais découpé, qu'il soit entier ou en morceau.
5. **Le MBID d'artiste MusicBrainz l'emporte quand il est présent**
   (`artist_split::artistes_de_la_piste`) :
   - S'il y a autant de MBID que de noms découpés, chaque nom reçoit le sien,
     dans l'ordre du crédit. `get_or_create` résout alors par MBID d'abord.
   - S'il n'y a qu'un MBID et que la base connaît cet artiste sous le nom
     entier, MusicBrainz dit qu'il n'y a qu'un artiste : rien n'est découpé.
   - Dans les autres cas, les noms découpés sont gardés sans MBID.
6. **Le cycle de vie de la liaison :**
   - Elle est réécrite à chaque relecture du fichier :
     `TrackImporter::ecrire_les_artistes_de_piste` est appelée après
     `create_batch` / `update_batch`, dans la transaction du lot, et retrouve
     la piste par son chemin.
   - Une piste éditée à la main (`Tenues`) perd ses liaisons.
   - La purge des artistes orphelins (#5007) tient compte de `track_artists`.
   - La fusion d'artistes (`ArtistRepo::absorber`) reporte les liaisons du
     doublon sur la cible.
7. **Les règles « Artiste » des collections et playlists intelligentes**
   s'appuient sur la liaison. Une piste correspond si `ar.name` correspond
   (comme aujourd'hui) **ou** si l'un de ses artistes reliés correspond :
   `t.id IN (SELECT ta.track_id FROM track_artists ta JOIN artists a ON
   a.id = ta.artist_id WHERE <condition sur a.name>)`.
   - C'est le même modèle que la règle « crédit » (`regles_sql::conditions_credit`).
   - Les opérateurs négatifs (« n'est pas », « ne contient pas ») deviennent :
     ni `ar.name`, ni aucun artiste relié.
   - Côté collection (niveau album) : `al.id IN (SELECT t2.album_id FROM
     tracks t2 JOIN track_artists …)`.
   - La règle « Artiste de l'album » (#6121) ne change pas.

## Livré dans cette PR (première étape)

- Ce document.
- La table `track_artists`, avec les migrations SQLite 125 et PostgreSQL 089,
  la copie à la bascule et l'assertion `pg_latest_version`.
- `tune-core/src/db/track_artist_repo.rs` : `remplacer_par_chemin` et
  `artistes_de_la_piste`.
- Le découpage au scan : `decouper_pour_le_scan`, `mbids_de_la_balise` et
  `artistes_de_la_piste`, dans `artist_split.rs`.
- Le branchement de ce découpage dans le scan manuel et le scan au démarrage.
- La purge des orphelins et la fusion d'artistes.
- Les tests unitaires de tout ce qui précède.

## Reste à faire

1. **Les règles « Artiste »** des playlists intelligentes
   (`tune-smart-http/src/regles_sql.rs`, `colonne_piste`) et des collections
   intelligentes (`smart_collections.rs`, `build_album_query`), selon le
   point 7.
2. **Le surveillant de fichiers** (`auto_scan.rs`,
   `build_track_from_metadata_opts`) : ce chemin, qui traite un fichier à la
   fois, ne remplit pas encore la liaison. Il faut aussi brancher l'import CUE
   (`scanner/cue_bibliotheque.rs`) et les imports (`routes/system/import.rs`).
3. **Plusieurs MBID.** lofty ne rend que la première valeur de
   `MUSICBRAINZ_ARTISTID` (`metadata/mod.rs`, `get_string`). Il faut lire
   toutes les valeurs (`get_strings`) pour que l'appariement du point 5 serve
   aux fichiers taggés par Picard.
4. **L'affichage** : la page d'un artiste doit lister les pistes où il est
   relié, et la fiche d'une piste doit montrer ses artistes. Ce travail
   concerne le serveur (routes) et le web.
5. **Mesurer** sur une vraie bibliothèque, avant d'activer la règle, ce que le
   découpage relie. L'aperçu `/system/artist-split-preview` peut être aligné
   sur `decouper_pour_le_scan`.
6. Les numéros de migration (125 / 089) sont à revoir à la fusion si une autre
   migration entre avant dans le lot.
