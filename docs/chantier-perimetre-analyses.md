# Chantier : le périmètre des analyses de fond (#5593)

Go de Bertrand le 13/10/2026. Branche du chantier : `chantier/perimetre-analyses`
(serveur, prise sur `batch/bugs-rc5-20261012` ; web, prise sur `main`).

## La demande

Sur une très grande bibliothèque (plus de 500 000 pistes), les trois passes de
fond (ReplayGain, plage dynamique, analyse acoustique CLAP) durent des
semaines. Le testeur demande à choisir ce qu'elles analysent : « je n'ai pas
besoin que mon NAS soit analysé », « le CLAP ne serait utile que pour Jazz et
Pop », et, plus tard, « circonscrire l'analyse à un répertoire » (fil 2173).

## Ce qui existe déjà (ne pas refaire)

| Livré | Où | Ce que ça fait |
|---|---|---|
| rc3, #5825 / web#1954 | `background_analysis_excluded_roots` | Exclut des racines de bibliothèque des passes qui décodent (ReplayGain, plage dynamique, empreintes, CLAP) et de leurs jauges. Une case par racine dans Réglages › Bibliothèque. |
| lot rc5, #6215 / web#2131 (#6198) | `audio_embedding_genres_included`, `audio_embedding_genres_excluded` | Réserve le CLAP à certains genres, ou en écarte. Ne touche QUE le CLAP. Deux champs de saisie sous les cases des racines. |

Tout passe par `tune-core/src/taches_de_fond/perimetre.rs` : une CLAUSE SQL
ajoutée au prédicat partagé de chaque passe, le même texte pour sa sélection et
pour le compteur de sa jauge. Une piste hors périmètre n'est ni sélectionnée
(donc jamais décodée) ni comptée comme du travail à faire.

## Le modèle

Le périmètre est une liste d'EXCLUSIONS. Rien de réglé : tout est analysé, le
comportement d'avant.

| Portée | Dossiers | Genres |
|---|---|---|
| Périmètre commun (toutes les passes) | `background_analysis_excluded_roots` (étape 1 : racines **et sous-dossiers**) | `background_analysis_excluded_genres` (étape 1) |
| Une passe à part (étape 2) | `background_analysis_scope_by_pass.<passe>.excluded_roots` | `background_analysis_scope_by_pass.<passe>.excluded_genres` |
| CLAP seul, en plus | — | `audio_embedding_genres_included` / `_excluded` (#6198, livré) |

Une piste est analysée par une passe si elle passe TOUTES les clauses qui
s'appliquent à cette passe.

### Périmètre par passe (étape 2, décision de Bertrand du 13/10)

`background_analysis_scope_by_pass` est un objet dont les clés sont les
passes : `replaygain` (gain de piste et d'album, crêtes, remesures),
`dynamic_range`, `fingerprints`, `audio_embedding`. Chaque passe peut porter
`excluded_roots` et `excluded_genres`.

- Un champ **absent** (ou `null`) **hérite** : du périmètre commun, et, pour
  les **empreintes, du ReplayGain** (elles décodent avec lui ; décision du
  13/10 : on garde).
- Un tableau, **même vide**, **remplace** la valeur héritée pour cette passe :
  `{"replaygain": {"excluded_roots": []}}` garde le ReplayGain sur le NAS
  exclu des autres passes.
- Vide (`{}`, le défaut) : tout hérite, le comportement de l'étape 1.
- Le moteur résout le périmètre d'une passe par `perimetre::perimetre_de` ;
  chaque prédicat de passe (sélection et jauge) prend la clause de SA passe
  (`clause_decodage(backend, Passe::…)`), et la page Santé compte le « hors
  périmètre » de la plage dynamique sur le périmètre de la plage dynamique.

### Dossiers

- Une entrée est un PRÉFIXE de chemin, borné par le séparateur : exclure
  `Z:\NAS\Podcasts` n'exclut pas `Z:\NAS\Podcasts2`. Les deux séparateurs sont
  normalisés (`\` → `/`) des deux côtés : une base Windows porte l'un ou
  l'autre selon le chemin d'arrivée de la piste.
- La clé garde son nom `…_excluded_roots` : le serveur a toujours accepté
  n'importe quel préfixe, et les clients rc3 l'écrivent déjà. Seule l'interface
  se limitait aux racines.
- La casse compte. L'interface web n'accepte qu'un dossier situé SOUS une
  racine déclarée (même casse que la racine), pour éviter une entrée qui ne
  correspondrait à rien.
- Le CLAP compare le chemin OUVRABLE (le support d'une piste de feuille CUE),
  les autres passes `tracks.file_path`.

### Genres

- Le test est celui des genres du CLAP et du filtre Genre des facettes : la
  colonne `tracks.genre` comparée en entier, OU le tableau JSON
  `tracks.genres`, casse ignorée.
- Une piste SANS genre n'est jamais écartée par une liste d'exclusion. (Avec la
  liste « Réserver aux genres » du CLAP, une piste sans genre n'est pas
  analysée : l'écran le dit.)
- **Le genre se juge À L'ALBUM** (décision de Bertrand du 13/10). Une piste
  d'un album qui porte un genre (`albums.genre` ou `albums.genres`) est jugée
  sur le genre de son album : un album dont le genre n'est pas exclu est
  analysé **en entier**, même ses pistes d'un genre exclu, et son gain
  d'album reste calculable (la garde de complétude de `analyze_album_batch`
  trouve toutes ses pistes). Un album d'un genre exclu sort en entier. Une
  piste d'un album SANS genre d'album n'est JAMAIS exclue par une règle de
  genre (décision du 13/10, pour que le gain d'album reste calculable). Seule
  une piste sans album est jugée sur ses propres genres. Vaut pour les listes d'exclusion du périmètre (commun et
  par passe) ; les deux listes du CLAP (#6198) restent jugées sur la piste.
- **Avertissement** (décision du 13/10) : quand un genre est exclu du
  ReplayGain, l'écran affiche sous le réglage « Ces pistes seront lues sans
  normalisation du volume. »

### Où il se règle

- Serveur : `PATCH /system/config`, publié par `GET /system/config`. Chaque
  liste est un tableau de chaînes, rognées, sans vide ni doublon, au plus 200
  entrées de 4 096 caractères ; `null` vaut la liste vide ; toute autre forme
  est refusée en 400 en nommant la clé. Le réglage vit dans `settings` : **pas
  de migration**.
- Changer les dossiers ou les genres de toutes les passes referme la campagne
  ReplayGain en cours : son total avait été compté sur l'ancien périmètre.
- **Étape 3** : `GET /system/analysis-scope/counts` rend, pour chaque entrée
  du périmètre commun et des réglages propres de chaque passe, le nombre de
  pistes (avec fichier) qu'elle désigne à elle seule :
  `{"common": {"excluded_roots": [{"value", "tracks"}], "excluded_genres": [...]},
  "by_pass": {"<passe>": {...}}}`. Une entrée mal orthographiée se lit 0. Un
  seul parcours de `tracks` par liste (`SUM(CASE …)` par entrée), hors de
  l'exécuteur. Les genres se comptent à l'album, comme ils s'appliquent.
- Web : Réglages › Bibliothèque, sous la liste des dossiers.
  - une case « Analyses de fond » par racine (rc3) ;
  - **étape 1** : « Dossiers exclus des analyses », la liste des sous-dossiers
    exclus, chacun avec « Réinclure », et un champ pour en ajouter un ;
  - **étape 1** : « Genres exclus de toutes les analyses », un champ à
    virgules ;
  - les deux champs du CLAP (#6198) ;
  - **étape 2** : « Périmètre par analyse », une case « Comme le périmètre
    commun » (« Comme le ReplayGain » pour les empreintes) par passe et par
    champ ; décochée, un champ à la passe ;
  - **étape 3** : « Pistes retirées : N » à côté de chaque entrée exclue,
    recompté après chaque réglage ; rien si le serveur n'a pas la route.

## Ce qui arrive aux résultats déjà calculés

Exclure ne JETTE rien. Les gains, plages dynamiques, empreintes et embeddings
CLAP déjà mesurés restent en base et servent à la lecture, à l'écran Santé et
aux ambiances. Réinclure un dossier ou un genre reprend là où la passe en
était : les témoins par piste (`rg_analyzed`, `dr_track`, sentinelle CLAP)
n'ont pas bougé.

Les jauges : une piste exclue sort du « reste à faire ». La page Santé la
compte à part (« hors périmètre », fil 2157), par la clause même qui l'écarte,
pour qu'aucune jauge n'attende une piste qu'aucune passe ne prendra.

Un bouton « oublier les analyses de ce dossier » n'est PAS prévu : rien ne le
demande.

## Hors périmètre de ce chantier

- **#5594**, récupérer auprès de la communauté les résultats d'analyse déjà
  calculés par d'autres. C'est l'autre façon de ne pas mesurer, mais elle
  suppose une clé de rapprochement par master (un gain ou un DR dépend du
  pressage), qui n'existe pas. Chantier distinct.
- **#5519**, la vitesse des passes (déjà livrée) et l'idée « crête → gain ».
- **#5597**, la jauge ReplayGain qui repart de zéro au redémarrage.

## Étapes

1. **Étape 1 (livrée, #6297 / web#2170).** Exclusion par dossier (racine ou sous-dossier) et par
   genre, appliquée par les trois passes (et les empreintes), avec son réglage
   web. Témoins : sélection, compteurs, partition dedans/dehors, passes réelles
   sur un vrai fichier, chacun avec sa contre-épreuve.
2. **Étape 2 (go du 13/10, ce lot).** Périmètre PAR PASSE, le commun par
   défaut ; genre jugé à l'album ; avertissement « sans normalisation ».
3. **Étape 3 (go du 13/10, ce lot).** Combien de pistes chaque entrée retire
   (`GET /system/analysis-scope/counts`).

## Décisions de Bertrand (13/10)

- Le genre se juge à l'album (voir « Genres »).
- Un avertissement visible quand un genre est exclu du ReplayGain.
- Étape 2 (par passe, le commun par défaut) et étape 3 (comptage) : go.
- Les empreintes suivent le ReplayGain : on garde.

## Décisions de Bertrand (13/10, suite)

- Une piste d'un album sans genre d'album n'est jamais exclue par une règle
  de genre.
- « Pistes retirées » compte les pistes que chaque entrée DÉSIGNE : une piste
  désignée par deux entrées compte deux fois. Le texte d'aide de l'écran le
  dit.
