# Chantier « Assistant IA » — étape 1

GO de Bertrand du 12/10/2026. Fonction **gratuite**, pour tout le monde :
l'utilisateur fournit **sa** clé, les appels partent de **son** serveur vers
**son** fournisseur. Rien ne passe par mozaiklabs.fr.

Deux usages :

1. une **playlist intelligente décrite par une phrase** (« jazz calme des
   années 60 sans les live ») devient des **règles du moteur existant** de
   playlists et collections intelligentes ;
2. des **suggestions d'enrichissement sur un album**, depuis sa fiche
   (bouton « Suggestions IA »).

## 1. Greffon natif ou cœur ?

**Greffon natif in-tree** : `plugins/tune-assistant-ia`, un `TunePlugin`
comme `tune-circle` ou `tune-concerts`. Les points d'accroche existent tous :

| besoin | point d'accroche |
|---|---|
| routes HTTP | `PluginContext::register_router`, monté par l'hôte sous `/api/v1/ext/assistant-ia`, derrière `auth_middleware` |
| la base (réglages, albums, `album_metadata`) | `HostServices::backend` (`Arc<dyn DbBackend>`) |
| un dossier de données (cache) | `PluginContext::data_dir` |
| les critères du moteur des règles | `HostServices::criteres`, rempli par l'hôte depuis `tune_smart_http::criteres::criteres_exposes()` |

Le système de greffons **WASM** a été écarté : il n'offre ni l'accès à
`album_metadata` ni le tableau des critères, et un appel HTTP sortant vers un
fournisseur arbitraire n'y est pas une permission prévue. Le cœur aussi :
rien ici n'a besoin d'y vivre.

Le greffon n'est **pas derrière une feature cargo** : il est compilé dans
tous les binaires (dépendance non optionnelle de `tune-server`). Une feature
de plus aurait été une ligne de plus à tenir dans chaque build livré
(`features_livrees_3355.rs`) pour une fonction que tout le monde reçoit. Il est
inscrit au workspace et aux portes `cargo test` / `cargo clippy` de `ci.yml`
(gardes `workflows_bornes.rs`).

## 2. Fournisseurs : une interface, une implémentation par famille

```rust
#[async_trait]
pub trait Fournisseur: Send + Sync {
    async fn completer(&self, demande: &Demande) -> Result<Reponse, ErreurFournisseur>;
}
```

| famille | point d'accès | authentification | modèle par défaut |
|---|---|---|---|
| `anthropic` | `POST {base}/v1/messages`, base `https://api.anthropic.com` | `x-api-key`, `anthropic-version: 2023-06-01` | `claude-haiku-5-5` ; `claude-sonnet-5-5` réglable pour le classique |
| `compatible_openai` | `POST {base}/chat/completions`, base `https://api.openai.com/v1` (Mistral : `https://api.mistral.ai/v1`…) | `Authorization: Bearer` | au choix de l'utilisateur |
| `gemini` | préréglage de `compatible_openai` : `https://generativelanguage.googleapis.com/v1beta/openai` | `Authorization: Bearer <clé Google AI Studio>` | `gemini-3.5-flash-lite` |
| `ollama` | `POST {base}/api/chat`, base `http://127.0.0.1:11434` | aucune | au choix de l'utilisateur |

`Fabrique` (une fermeture `Acces -> Box<dyn Fournisseur>`) est injectée dans
l'état des routes : la vraie en production, une fausse dans les essais.

### Gemini : préréglage compatible OpenAI, pas `generateContent`

Relevé le 12/10/2026 dans la documentation officielle
(<https://ai.google.dev/gemini-api/docs/openai>, page mise à jour le
02/09/2026) : adresse `https://generativelanguage.googleapis.com/v1beta/openai/`,
clé en `Authorization: Bearer`, modèle nommé sans préfixe `models/`. La page dit
la compatibilité « encore en bêta » et **ne documente pas**
`response_format: json_object` : ce champ n'est donc pas envoyé à Gemini (il
l'est aux autres points d'accès compatibles).

Ce n'est pas une perte : **aucune famille n'est crue sur parole**. Le JSON est
extrait de la réponse puis validé côté serveur contre le schéma (§ 5) ; une
réponse hors schéma rend `422 reponse_invalide`. Le mode JSON du fournisseur
n'est qu'une aide. D'où le préréglage plutôt qu'une implémentation dédiée de
`generateContent`, qui n'apporterait rien que la validation ne donne déjà.

Modèle par défaut : `gemini-3.5-flash-lite`, « fastest, most cost-effective »,
statut Stable, recommandé par Google pour un projet nouveau (page des modèles,
relevée le 12/10/2026).

## 3. La clé

- Saisie dans **Réglages ▸ Assistant IA** (`PUT /reglages {cle}`, `""` l'efface).
- **Chiffrée au repos.** Le mécanisme existant pour les jetons Qobuz/Tidal
  (`auth_tokens_<service>` dans `settings`) est **réutilisé** pour la
  classification : le réglage s'appelle `assistant_ia_api_key_chiffree`, son
  nom porte `api_key`, donc `tune_core::secrets::est_secret` le masque dans
  `/system/config`, le retire des exports et de la sauvegarde dans le nuage.
  Mais ces jetons sont en clair dans la base ; la clé d'API, elle, est en plus
  **scellée** : XChaCha20-Poly1305 (les primitives de `secret_envelope`) sous
  une clé maîtresse de 256 bits posée dans un **fichier à côté de la base**
  (`assistant-ia.cle`, droits 0600). Une copie de `tune.db` seule ne la livre
  plus. Fichier perdu : `cle_illisible`, l'écran demande de la ressaisir.
- **Jamais renvoyée** : l'écran ne reçoit que `cle_definie` et `cle_fin`
  (4 derniers caractères).
- **Jamais journalisée** : le module des fournisseurs ne journalise que la
  famille et le statut HTTP ; une erreur de transport est réduite à sa nature
  (délai, connexion) ; `Acces` a un `Debug` qui masque la clé.
- **« Tester la clé »** (`POST /tester`) fait un vrai petit appel et rend
  `{ok: true}` ou `{ok: false, error, message}` avec `error` parmi
  `cle_refusee` (401/403), `quota` (429, crédit épuisé), `surcharge` (5xx,
  529), `modele_inconnu`, `reseau`, `plafond_atteint`, `non_configure`,
  `tarif_inconnu`, `cle_illisible`. **Jamais 401** côté Tune : le client web y
  lit une session expirée et déconnecte.

## 4. Coût et plafond

- Chaque appel compte ses jetons d'entrée et de sortie (`usage` du
  fournisseur).
- Table des prix dans `couts.rs`, **seulement des tarifs documentés** :
  - Anthropic, relevé du 12/10/2026 (référence de l'API Claude) :
    `claude-haiku-5-5` 0,10 $ / 0,50 $ par million de jetons (requêtes de
    moins de 100 000 jetons), `claude-sonnet-5-5` 2 $ / 10 $ ;
  - Gemini, <https://ai.google.dev/gemini-api/docs/pricing> (page du
    09/10/2026, relevée le 12/10/2026) : `gemini-3.5-flash-lite` 0,30 $ /
    2,50 $, `gemini-3.1-flash-lite` 0,25 $ / 1,50 $. La page annonce des prix
    à peu près doublés au 01/01/2027 : **à relever avant**.
  - compatible OpenAI : **aucun prix inventé** — l'utilisateur saisit les deux
    prix de son modèle ; sans eux l'appel est refusé (`tarif_inconnu`) plutôt
    que compté pour zéro.
  - Ollama : 0.
- Les prix sont en dollars, le plafond en euros : taux `taux_usd_eur`
  réglable, **1 par défaut**, ce qui surestime la dépense tant que le dollar
  vaut moins d'un euro (choix de prudence, pas un taux relevé).
- **Plafond mensuel** réglable (`plafond_eur`, 2 € par défaut, un plafond
  illisible vaut 0). Consommation du mois dans `assistant_ia_consommation`,
  remise à zéro au changement de mois. Plafond atteint : **plus aucun appel**,
  `409 plafond_atteint` avec un message clair. La lecture du plafond, l'appel
  et le comptage tiennent sous un même verrou.
- Pas de garde Premium.

## 5. Cache

Clé : SHA-256 de (version des consignes, usage, famille, modèle, entrée
normalisée). L'entrée normalisée d'une playlist est la phrase en minuscules aux
espaces repliés **plus la consigne entière** (critères, genres de la
bibliothèque, langue) ; celle d'un album, son contexte complet. Une entrée par
fichier dans `data_dir/cache` (500 au plus). Une réponse du cache ne coûte
rien, même plafond atteint, et elle est **revalidée** à chaque lecture contre
l'état présent de la base.

## 6. Playlist par phrase

`POST /api/v1/ext/assistant-ia/playlist {phrase, cible?: "playlist"|"collection", langue?}`

Consigne : la liste **fermée** des critères du moteur
(`tune_smart_http::criteres`, le tableau unique #5547), avec, pour chaque
opérateur, la forme de sa valeur ; les genres les plus fréquents de la
bibliothèque, pour reprendre leur orthographe. Les familles qui désignent un
objet de la base par identifiant (`collection_ref`, `playlist_ref`,
`tag_ref`) et `credit` ne sont pas offertes au modèle.

Schéma de réponse demandé :

```json
{"name": "…", "match_mode": "all|any",
 "rules": [{"field": "…", "op": "…", "value": …}],
 "sort_by": "title|artist|album|year|duration|added_at|play_count|random|null",
 "sort_order": "asc|desc|null", "max_tracks": 1-10000 | null,
 "untranslated": ["ce que nulle règle n'exprime"]}
```

Validation serveur, règle par règle : critère connu du moteur **et** offert,
opérateur de sa famille, valeur de la bonne forme (`between` : deux bornes,
`in_last` : entier ≥ 1, dates `AAAA-MM-JJ` ou `now-Nd`…). Une règle inconnue est
**refusée** et rendue dans `refusees` avec sa raison (`critere_inconnu`,
`operateur_inconnu`, `valeur_invalide`). Réponse :

```json
{"ok": true, "cible": "playlist",
 "proposition": {"name", "match_mode", "rules", "sort_by", "sort_order",
                 "max_tracks", "untranslated", "refusees"},
 "depuis_cache": false, "cout_eur": 0.0004}
```

**Rien n'est créé** : l'écran montre les règles, l'utilisateur les modifie,
puis enregistre par la route existante `POST /api/v1/library/smart-playlists`.

## 7. Suggestions sur un album

`POST /albums/{id}/suggestions {langue?}` → `{suggestions: [{champ, avant,
apres, confiance, raison}], ecartees: [{champ, valeur, raison}]}`

Champs : `title`, `artist`, `genre`, `label` (colonnes de `albums`) ;
`subgenre`, `work`, `movement`, `orchestra`, `composer`, `conductor`,
`performer`, `lyricist`, `producer` (magasin `album_metadata` ; les cinq
derniers sont les clés « Crédits » du catalogue des champs). Schéma demandé :
`{"suggestions": [{"field", "value", "confidence": 0..1, "reason"}]}`.

**Garde-fous** (rejoués à l'application) :

- champ tenu à la main (`album_metadata.edition_manuelle`, C3) →
  `edition_manuelle` ;
- album identifié par MusicBrainz (`musicbrainz_release_id`) : tout champ déjà
  rempli → `musicbrainz` (remplir un vide reste permis) ; l'artiste dès que
  l'artiste porte un `musicbrainz_id` ;
- un changement de la seule casse de l'artiste → `renommer_l_artiste` (c'est un
  renommage de l'artiste, pour tous ses albums : geste de la fiche artiste) ;
- champ inconnu, valeur vide ou trop longue, valeur identique → écartés.

`POST /albums/{id}/appliquer {lignes: [{champ, valeur}]}` écrit par le chemin
de l'édition manuelle (`AlbumRepo::update`, `AlbumMetadataRepo::set`) — **base
seulement**, comme l'édition manuelle ; écrire dans les fichiers reste
`POST /library/albums/{id}/edition/write-tags`. Le marqueur d'édition manuelle
n'est **pas** posé : l'utilisateur a accepté, il n'a pas saisi. Les champs
acceptés sont nommés dans `album_metadata.source_ia` (« source : IA »).

**Annulable** : chaque application est une opération de
`album_metadata.assistant_ia_journal` (20 par album) avec la valeur précédente
de chaque ligne. `POST /albums/{id}/annuler {operation_id?}` restaure ; un champ
modifié depuis n'est pas écrasé (`modifie_depuis`). `GET /albums/{id}/historique`.

## 8. Essais (faux fournisseur, aucun appel réseau réel)

`plugins/tune-assistant-ia/tests/assistant.rs` et les essais de module :

- règle inconnue refusée (route et validateur, sur les **vrais** critères du
  moteur) ;
- plafond atteint qui bloque (playlist, album, test de clé) ; plafond 0 ;
  tarif inconnu refusé ;
- cache qui évite un second appel ;
- donnée MusicBrainz jamais proposée en écrasement, ni appliquée ; champ édité
  à la main jamais proposé ; appliquer puis annuler ;
- clé jamais dans une réponse d'API ni dans un journal : abonné `tracing`
  capturant tout, vrai client HTTP contre un faux « Anthropic » sur
  127.0.0.1 qui confirme recevoir la clé ;
- Gemini : vrai client contre un faux serveur local, `Bearer`, pas de
  `response_format`, coût au tarif de la table.

## 9. Étapes suivantes

**Étape 2**
- étendre les suggestions à une sélection d'albums (lot, aperçu, plafond par
  lot) et aux pistes (œuvre / mouvement par piste) ;
- propager le genre accepté aux pistes comme l'édition manuelle (#5314), avec
  son annulation ;
- offrir les critères par référence (étiquettes, collections) en donnant au
  modèle la liste des noms existants ;
- migrer l'ancien assistant `POST /ai/query` (clé `anthropic_api_key` en clair,
  modèle figé) et la traduction des ambiances vers ce coffre et ce plafond ;
- réserver les réglages de l'assistant aux profils administrateurs.

**Étape 3**
- recherche conversationnelle dans la bibliothèque, radio « dans l'esprit
  de… » ;
- suivi de consommation détaillé par usage, alerte à 80 % du plafond ;
- relevé des tarifs automatisé ou daté à l'écran, avant le changement de prix
  Gemini du 01/01/2027.
