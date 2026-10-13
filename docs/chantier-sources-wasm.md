# Chantier — greffons WASM fournisseurs de source de streaming (#6197)

Branche de chantier : `chantier/sources-wasm`, partie de `batch/bugs-rc5-20261012`.
Origine : fil 2238 (Charly33, MusicMe) et l'étude qui a suivi. MusicMe lui-même
reste une **décision de Bertrand** (pas d'API publique) : ce chantier ne livre
que le socle, et un faux service d'exemple.

## Ce qui manquait

Les 21 imports `"tune"` de `tune-plugin-runtime-wasm` permettent de CONSOMMER
les services existants (#4716), pas d'en FOURNIR un : aucun accès réseau, aucun
moyen de s'inscrire dans le `ServiceRegistry`.

## Le contrat

### Manifeste

```json
{
  "id": "source-exemple",
  "permissions": ["http", "streaming_source"],
  "http_allowed_domains": ["api.exemple.com", "*.cdn.exemple.com"],
  "streaming_source": {
    "name": "exemple",
    "display_name": "Exemple",
    "login_fields": [
      { "key": "email", "label": "Adresse", "type": "email", "required": true },
      { "key": "password", "label": "Mot de passe", "type": "password", "required": true }
    ]
  }
}
```

- `http` ouvre l'import `host_http`. **Liste de domaines** : un nom exact
  (`api.exemple.com`, tous ports) ou `*.domaine` (les sous-domaines seulement).
  La liste vient du manifeste lu par l'HÔTE et est rangée dans le `Store` ;
  le greffon ne peut ni la lire ni l'étendre. Refusés avant tout réseau :
  domaine hors liste, schéma autre que `http`/`https`, identifiants dans
  l'URL (`user@hôte`). Les redirections ne sont **pas suivies** : la 3xx et
  son `Location` reviennent au greffon, qui rappelle `host_http` — l'adresse
  suivante repasse donc par la liste.
- **Boîte à cookies cloisonnée** : tenue par l'hôte, une par greffon et par
  hôte distant. Les `Set-Cookie` sont absorbés et ne sont jamais montrés au
  greffon ; le `Cookie` est ajouté par l'hôte. Un greffon ne voit ni les
  cookies d'un autre, ni ceux de Tune.
- `streaming_source` inscrit le greffon comme service. Son `name` ne doit pas
  être celui d'un service existant (refus au chargement).

### Fonctions exportées par le greffon

Même convention que `plugin_dispatch` : `(ptr, len) -> (ptr << 32) | len`,
JSON UTF-8.

| Export | Entrée | Sortie | Obligatoire |
|---|---|---|---|
| `login` | les champs de connexion | `{authenticated, username?, subscription?}` | oui |
| `search` | `{query, limit}` | `SearchResults` (forme du cœur) | oui |
| `get_album` | `{id}` | `StreamAlbum` | oui |
| `get_album_tracks` | `{id}` | `[StreamTrack]` | non (sinon `get_album.tracks`) |
| `get_track` | `{id}` | `StreamTrack` | non |
| `get_track_url` | `{id, quality?}` | `StreamUrl` | oui |
| `logout` | `{}` | `{}` | non |

Les champs que le greffon peut omettre (`explicit`, `duration_ms`,
`track_count`, les listes vides de `search`) sont complétés par l'adaptateur.
Le service est identifié par le nom que l'HÔTE a inscrit, lu dans le
manifeste : un greffon ne peut pas se faire passer pour Qobuz, et un nom déjà
pris est refusé au chargement.

### L'adaptateur `WasmStreamingService`

`tune-server/src/sources_wasm.rs` implémente `StreamingService` et s'inscrit
dans `AppState::services` au chargement des greffons. Toutes les routes
`/streaming/{service}/…` (connexion, recherche, album, adresse de lecture) le
servent sans changement.

**Pont synchrone/asynchrone.** Un appel wasm est synchrone ; `host_http` doit
faire une requête `reqwest` asynchrone. L'adaptateur joue chaque appel dans
`tokio::task::spawn_blocking` ; l'hôte y fait `Handle::block_on` — le même
pont que `now_playing`/`play` (#4716). Le greffon partage son `Store` avec ses
routes : un seul `tokio::sync::Mutex` par greffon.

### Identifiants chiffrés

`save_tokens` rend les champs de connexion **scellés** (XChaCha20-Poly1305,
données associées = identifiant du greffon + nom du service) ; la clé de 32
octets vit dans un fichier à part (`plugin-credentials.key`, 0600, à côté de
la base, ou `TUNE_PLUGIN_CREDENTIALS_KEY`). Une fuite de la base seule ne
livre pas les mots de passe. Au démarrage, `restore_tokens` les ouvre et
`post_restore` rejoue `login`.

## Étape 1 (ce chantier, livrée)

- runtime : permission `http`, import `host_http`, liste de domaines,
  `WasmPlugin::call_export`.
- cœur : champs `http_allowed_domains` et `streaming_source` du manifeste ;
  coffre d'identifiants.
- serveur : `host_http` réel (reqwest, boîte à cookies, plafond de 4 Mio),
  `WasmStreamingService`, inscription au chargement.
- greffon d'exemple `tune-server/tests/fixtures/plugins/source-exemple/`
  (source Rust + `main.wasm`) : un faux service qui lit son catalogue — deux
  albums — et ses adresses de flux sur un serveur HTTP de test.
- tests : domaine non autorisé refusé, identifiants chiffrés, de bout en bout
  par les routes `/streaming/…`.

## Étape 2 (livrée)

Le guide de l'auteur de greffon est
[`docs/plugins/wasm-streaming-source.md`](plugins/wasm-streaming-source.md) :
il tient le tableau complet des exports, des bornes et de la lecture.

1. **Écran** : `GET /api/v1/plugins/sources` rend, pour chaque greffon
   chargé, la source qu'il fournit (`service`, `display_name`,
   `login_fields`, `http_allowed_domains`, `registered`) et le journal réseau
   de chaque greffon `http` (`network`). Le client web en tire le formulaire
   de connexion de Réglages › Services (les champs du manifeste, dans leur
   ordre) et le journal de la carte du greffon dans Extensions. Rien d'autre
   du manifeste ne sort, et jamais un identifiant.
2. **Couverture du trait** : exports FACULTATIFS `search_page` (pagination,
   `totals`, `has_more`), `get_artist`, `get_artist_albums` (avec `offset`),
   `get_artist_top_tracks`, `get_playlist`, `get_playlist_tracks`,
   `get_user_albums|artists|playlists|tracks`, `add_favorite` +
   `remove_favorite` (le cœur ne se coche que si LES DEUX existent),
   `get_new_releases`, `get_featured`, `get_genres`, `get_genre_albums`. Les
   exports présents sont relevés à l'inscription ; un absent rend le défaut du
   trait. Sans `get_album_tracks` (ou `get_playlist_tracks`), les pistes sont
   les `tracks` de `get_album` (ou `get_playlist`).
3. **Lecture réelle** : l'adresse de `get_track_url` doit être dans la liste
   de domaines ; l'hôte y ajoute les cookies qu'il tient pour l'hôte du CDN
   (le téléchargeur les rejoue, `StreamUrl::headers`), et la route
   `/streaming/{service}/tracks/{id}/url` les retire de ce qu'elle rend au
   client (`Cookie`, `Authorization`). Une erreur du greffon portant
   `"status": 401|403` devient `HTTP 401 : …` : c'est le motif que la lecture,
   le préchargement et la route reconnaissent pour appeler
   `refresh_if_needed`, qui rejoue `login` avec les identifiants scellés.
   `expires_at` n'est lu par aucun chemin du cœur : l'adresse est demandée au
   moment de jouer et consommée aussitôt. La qualité annoncée est celle que
   rend le greffon ; une cadence nulle est complétée par
   `cadence_du_flux` ou la piste n'est pas préchargée, comme pour tout
   service.
4. **Durcissement** (`ReseauDesGreffons`) : budget d'UN appel au greffon —
   32 requêtes, 45 s (`too_many_requests`, `call_timeout`), ouvert après la
   prise du verrou du greffon, pour ses fonctions exportées comme pour ses
   routes ; quota de 240 requêtes par minute et par greffon
   (`rate_limited`) ; chaque refus rend `{error, detail}` sans réseau. Le
   journal compte, par hôte, requêtes, refus (du runtime — nouvelle méthode
   `HostContext::http_refused` — comme de l'hôte), dernier statut, dernière
   erreur ; il retient 64 hôtes, les suivants sous `(autres)`. Les cookies
   sont persistés dans `settings` (`plugin_cookies_{greffon}`), scellés par
   le coffre (service `#cookies`, qui ne s'ouvre pas comme un identifiant),
   relus au chargement ; un scellé illisible est effacé.
5. **SDK** : `cargo tune-plugin new <id> --source --output <dir>` engendre le
   greffon d'exemple sous un autre identifiant (source incluse depuis
   `tests/fixtures/plugins/source-exemple/`, l'unique exemplaire), sans test
   d'hôte dans sa liste de domaines.

## Reste

- MusicMe, ou tout connecteur vers un vrai service : décision de Bertrand
  sur les conditions d'utilisation avant tout connecteur. Le fil 2238 attend
  la réponse (`tune-parole`).
- Les valeurs des bornes (32 requêtes / 45 s par appel, 240 par minute) sont
  des choix techniques, à revoir sur un vrai connecteur.
- Les cookies persistés n'ont pas d'expiration (`Expires`/`Max-Age` autre que
  0 ne sont pas lus) : une session que le service a close se renouvelle par
  le 401 et `refresh_if_needed`.
