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

## Étape suivante

1. Écran web : champs de connexion lus du manifeste (route
   `GET /plugins/sources`), carte du service dans Réglages › Services.
2. Couverture du trait : artistes, playlists, favoris, découverte, pagination.
3. Lecture réelle : vérifier le chemin `resolve_stream` pour une source wasm
   (en-têtes ou cookies exigés par le CDN, URL qui expire), et la qualité
   annoncée.
4. Durcissement : quota de requêtes par greffon, délai par appel, journal des
   domaines contactés visible dans l'écran du greffon, cookies persistés
   (aujourd'hui en mémoire).
5. Documentation SDK et modèle de greffon (`cargo tune-plugin new --source`).
6. MusicMe : décision de Bertrand sur les conditions d'utilisation avant tout
   connecteur.
