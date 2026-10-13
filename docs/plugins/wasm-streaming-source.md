# Écrire un greffon WASM source de streaming (#6197)

Un greffon WASM peut **fournir** un service de streaming : Tune l'inscrit dans
son registre, et toutes les routes `/api/v1/streaming/{service}/…` le servent
comme Qobuz ou TIDAL (recherche, albums, artistes, playlists, favoris,
découverte, lecture). Le serveur doit être compilé avec `plugins-wasm`.

Ce n'est PAS le SDK audio natif de `sdk/` : pas d'ABI C, pas de caisse du
SDK. Un greffon source est un module `wasm32-unknown-unknown` qui ne dépend
que de `serde_json`.

La conception et l'historique du chantier sont dans
[`docs/chantier-sources-wasm.md`](../chantier-sources-wasm.md). Le greffon
d'exemple, chargé par les tests du serveur contre le vrai hôte, est
[`tune-server/tests/fixtures/plugins/source-exemple/`](../../tune-server/tests/fixtures/plugins/source-exemple/).

## Partir du modèle

```sh
cargo install --path sdk/cargo-tune-plugin --locked
cargo tune-plugin new ma-source --source --output ./ma-source
cd ma-source
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/ma_source.wasm main.wasm
```

Le projet engendré est le greffon d'exemple, sous un autre identifiant. On
installe `manifest.json` et `main.wasm` dans `$TUNE_PLUGINS_DIR/ma-source/`,
puis on redémarre Tune : le service apparaît dans **Réglages › Services**,
avec les champs de connexion du manifeste, et dans **Extensions** avec son
journal réseau.

## Le manifeste

```json
{
  "id": "ma-source",
  "entry_point": "main.wasm",
  "permissions": ["http", "streaming_source"],
  "http_allowed_domains": ["api.mon-service.example", "*.cdn.mon-service.example"],
  "streaming_source": {
    "name": "ma-source",
    "display_name": "Mon service",
    "login_fields": [
      { "key": "email", "label": "Adresse", "type": "email", "required": true },
      { "key": "password", "label": "Mot de passe", "type": "password", "required": true }
    ]
  }
}
```

- `streaming_source.name` : la clé du service (`[a-z0-9_-]`, 40 caractères
  au plus). Un nom déjà pris (Qobuz, un autre greffon) est refusé au
  chargement.
- `login_fields` : l'écran les demande dans cet ordre ; `type` vaut `text`,
  `email` ou `password` (masqué). Le greffon ne reçoit QUE ces clés.
- `http_allowed_domains` : un nom exact (tous ports) ou `*.domaine` (les
  sous-domaines seulement). N'y mettre que les hôtes du service.

## Les fonctions exportées

Convention de `plugin_dispatch` : `(ptr, len) -> (ptr << 32) | len`, JSON
UTF-8 à l'entrée et à la sortie. Une erreur se rend `{"error": "texte"}` ; avec
`"status": 401` (ou `403`), elle dit « session expirée » : l'hôte rejoue
`login` avec les identifiants qu'il a scellés, puis redemande une fois.

| Export | Entrée | Sortie | Absent |
|---|---|---|---|
| `login` | les champs de connexion | `{authenticated, username?, subscription?}` | obligatoire |
| `search` | `{query, limit}` | `{tracks, albums, artists, playlists}` | obligatoire |
| `get_album` | `{id}` | album (avec `tracks` facultatif) | obligatoire |
| `get_track_url` | `{id, quality?}` | `{url, mime_type, quality, headers?, expires_at?}` | obligatoire |
| `search_page` | `{query, limit, offset}` | comme `search`, plus `totals?`, `has_more?` | une page unique |
| `get_album_tracks` | `{id}` | `[piste]` | les `tracks` de `get_album` |
| `get_track` | `{id}` | piste | « non pris en charge » |
| `get_artist` | `{id}` | `{id, name, image_path?, bio?}` | « non pris en charge » |
| `get_artist_albums` | `{id, offset}` | `[album]` | liste vide |
| `get_artist_top_tracks` | `{id}` | `[piste]` | liste vide |
| `get_playlist` | `{id}` | `{id, name, description?, cover_path?, track_count, owner?}` | « non pris en charge » |
| `get_playlist_tracks` | `{id}` | `[piste]` | les `tracks` de `get_playlist` |
| `get_user_albums`, `get_user_artists`, `get_user_playlists`, `get_user_tracks` | `{}` | liste | liste vide |
| `add_favorite`, `remove_favorite` | `{type, id}` (`tracks`, `albums`, `artists`, `playlists`) | `{}` | cœur non cochable (il faut LES DEUX) |
| `get_new_releases` | `{}` | `[album]` | liste vide |
| `get_featured` | `{}` | `[playlist]` | liste vide |
| `get_genres` | `{parent_id}` | `[{id, name, has_children?, image_url?}]` | liste vide |
| `get_genre_albums` | `{id, limit}` | `[album]` | liste vide |
| `logout` | `{}` | `{}` | rien |

Les formes `piste` et `album` sont celles du cœur (`StreamTrack`,
`StreamAlbum` de `tune-core/src/streaming/traits.rs`). L'hôte complète ce que
le greffon peut omettre : `explicit`, `duration_ms`, `track_count`, les listes
vides de la recherche. Une `quality.sample_rate` nulle est complétée par la
lecture, ou la piste n'est pas préchargée : la déclarer.

## Le réseau

Le greffon n'a pas de réseau à lui : il appelle l'import `host_http` du module
`"tune"` (permission `http`) avec `{method, url, headers?, body?}` et reçoit
`{status, headers, body}`.

- **Liste de domaines** : vérifiée AVANT toute connexion. Refusés aussi : un
  schéma autre que `http`/`https`, des identifiants dans l'URL. Le refus
  rend `{"error": "domain_not_allowed", "detail": "<hôte>"}`.
- **Pas de redirection suivie** : la 3xx et son `Location` reviennent au
  greffon, qui rappelle `host_http` — l'adresse suivante repasse la liste.
- **Cookies** : tenus par l'hôte, par greffon et par hôte distant ; les
  `Set-Cookie` ne sont jamais montrés au greffon. Ils sont persistés en base,
  scellés par la clé du serveur (`plugin-credentials.key`) : un redémarrage
  retrouve la session.
- **Bornes** (refus sans réseau, avec ce code d'erreur) :
  - `too_many_requests` : plus de 32 requêtes pendant UN appel au greffon ;
  - `call_timeout` : l'appel dure depuis plus de 45 s ;
  - `rate_limited` : plus de 240 requêtes du greffon sur une minute.
  Une requête est coupée à 20 s, et une réponse plafonnée à 4 Mio.
- **Journal** : chaque hôte contacté ou refusé est compté (requêtes, refus,
  dernier statut, dernière erreur) et montré dans **Extensions**, sur la
  carte du greffon (`GET /api/v1/plugins/sources`, clé `network`).

## La lecture

L'adresse que rend `get_track_url` est téléchargée par l'HÔTE :

- elle doit être dans `http_allowed_domains`, sinon la lecture est refusée ;
- les `headers` rendus sont rejoués par le téléchargeur (sauf `host`,
  `content-length`, `transfer-encoding`, `connection`) ;
- les cookies que l'hôte tient pour l'hôte de l'adresse y sont ajoutés : un
  CDN qui exige la session la reçoit, sans que le greffon la voie ;
- la route `/streaming/{service}/tracks/{id}/url` rend l'adresse au client
  SANS les en-têtes `Cookie` et `Authorization`.

L'adresse est demandée au moment de jouer (ou de précharger) et consommée
aussitôt ; `expires_at` est informatif. Une adresse refusée par le service
avec 401/403 déclenche la reconnexion décrite plus haut.

## Les identifiants

Ce que l'écran envoie est filtré sur `login_fields`, puis scellé
(XChaCha20-Poly1305, données associées = greffon + service) avant d'aller en
base. Au démarrage, l'hôte les ouvre et rejoue `login`. Un greffon ne voit
jamais les identifiants d'un autre.
