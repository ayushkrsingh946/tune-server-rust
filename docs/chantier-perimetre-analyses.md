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
| Toutes les passes (ReplayGain, plage dynamique, empreintes, CLAP) | `background_analysis_excluded_roots` (étape 1 : racines **et sous-dossiers**) | `background_analysis_excluded_genres` (**étape 1, nouveau**) |
| CLAP seul | (étape 2, si demandé) | `audio_embedding_genres_included` / `_excluded` (#6198, livré) |
| ReplayGain seul, plage dynamique seule | (étape 2, si demandé) | (étape 2, si demandé) |

Une piste est analysée par une passe si elle passe TOUTES les clauses qui
s'appliquent à cette passe.

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
- **Conséquence sur le gain d'album.** Le gain d'album n'est calculé que
  lorsque TOUTES les pistes de l'album ont leur gain de piste (garde de
  complétude de `analyze_album_batch`, une moyenne partielle n'est pas une
  moyenne). Une piste d'un genre écarté du ReplayGain prive donc son album de
  gain d'album. Sur un album d'un seul genre, c'est sans effet (tout l'album
  est écarté). Sur une compilation aux genres mêlés, l'album n'aura qu'un gain
  de piste. Voir les décisions ouvertes.

### Où il se règle

- Serveur : `PATCH /system/config`, publié par `GET /system/config`. Chaque
  liste est un tableau de chaînes, rognées, sans vide ni doublon, au plus 200
  entrées de 4 096 caractères ; `null` vaut la liste vide ; toute autre forme
  est refusée en 400 en nommant la clé. Le réglage vit dans `settings` : **pas
  de migration**.
- Changer les dossiers ou les genres de toutes les passes referme la campagne
  ReplayGain en cours : son total avait été compté sur l'ancien périmètre.
- Web : Réglages › Bibliothèque, sous la liste des dossiers.
  - une case « Analyses de fond » par racine (rc3) ;
  - **étape 1** : « Dossiers exclus des analyses », la liste des sous-dossiers
    exclus, chacun avec « Réinclure », et un champ pour en ajouter un ;
  - **étape 1** : « Genres exclus de toutes les analyses », un champ à
    virgules ;
  - les deux champs du CLAP (#6198).

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

1. **Étape 1 (ce lot).** Exclusion par dossier (racine ou sous-dossier) et par
   genre, appliquée par les trois passes (et les empreintes), avec son réglage
   web. Témoins : sélection, compteurs, partition dedans/dehors, passes réelles
   sur un vrai fichier, chacun avec sa contre-épreuve.
2. **Étape 2 (sur décision).** Périmètre PAR PASSE : par exemple garder le
   ReplayGain sur le NAS mais pas le CLAP. Le testeur n'a pas répondu à cette
   question (#5593, commentaire du 01/10).
3. **Étape 3 (sur décision).** Afficher, à côté de chaque entrée, combien de
   pistes elle exclut (une route de comptage), pour qu'une entrée mal
   orthographiée se voie.

## Décisions laissées à Bertrand

- Le genre écarté des passes de lecture (ReplayGain, plage dynamique) : faut-il
  un avertissement dans l'écran (« ces pistes seront lues sans
  normalisation ») ? Aujourd'hui le texte d'aide le dit en une phrase.
- Le gain d'album d'une compilation aux genres mêlés (voir plus haut) : on le
  laisse manquer, ou le genre se juge à l'album (tout l'album écarté si une
  majorité de pistes l'est) ?
- Les empreintes suivent les dossiers et les genres comme le ReplayGain, par
  cohérence avec les racines de la rc3. À garder ?
- L'étape 2 (périmètre par passe) et l'étape 3 (comptage par entrée).
