# Chantier : la lecture avant les tâches de fond (#4681)

Go de Bertrand le 13/10/2026. Branche du chantier : `chantier/priorite-lecture`,
partie de `batch/bugs-rc5-20261012`.

Le but : pendant qu'une zone joue, le chemin audio (décodage, DSP, envoi) passe
avant le scan, le ReplayGain, la plage dynamique, l'analyse acoustique (CLAP),
les empreintes et l'enrichissement. On veut éviter les micro-coupures.

## 1. Ce qui existe déjà (état de `batch/bugs-rc5-20261012`)

Une bonne partie du mécanisme est en place. Ce chantier part de là et ne le
refait pas.

### Le témoin de lecture

`taches_de_fond::priorite` garde en mémoire l'ensemble des zones qui jouent.
Il est alimenté par le seul point qui écrit l'état de lecture,
`ZoneRepo::save_play_state`. Seul `"playing"` compte : la pause ne compte pas.
Au démarrage, `startup.rs` remet à `stopped` les zones restées à `playing`.

### Les passes qui décodent cèdent tout

| Passe | Où elle regarde | Ce qu'elle fait pendant la lecture |
|---|---|---|
| ReplayGain (piste, album, crêtes) | `any_zone_playing` (requête sur `zones`) à chaque tour et avant chaque fichier, et `veiller_lecture` (une requête toutes les 250 ms) pendant l'analyse d'un fichier | Elle abandonne le fichier en cours sans le marquer analysé (#2495), puis dort 30 s entre deux vérifications |
| Empreintes, rattrapage de la plage dynamique | la même garde, avant chaque fichier | La même chose |
| Plage dynamique à la demande | `any_zone_playing` à chaque tour | Elle dort 30 s |
| Analyse acoustique (CLAP) | le témoin en mémoire, puis la requête hors de l'exécuteur (#5138), avant chaque fichier ; `any_zone_playing` entre deux lots | Elle sort du lot, rend la session ONNX et dort 30 s. ONNX est plafonné à 1 fil (8 cœurs ou moins) ou 2 fils |

Le segment de 30 s déjà parti dans le pool bloquant va jusqu'à sa fin.
`spawn_blocking` ne se rétracte pas. Il tourne alors avec une priorité d'E/S
basse (`baisser_pendant_la_lecture`).

### Les passes freinées (#4699, #5942, publié dans la rc4)

La politique `background_playback_policy` (`PATCH /system/config`) s'applique
quand une zone joue :

- une pause de 5 s entre deux éléments (enrichissement MusicBrainz, images,
  lots de scan, rapports DR) ;
- le scan lit 2 fichiers à la fois au lieu de 32, avec 20 ms de pause entre
  deux paquets ;
- une connexion de lecture SQLite sur 3 est interdite aux fils de fond ;
- l'écrivain de fond laisse d'abord passer celui de premier plan (100 ms au
  plus) ;
- la priorité d'E/S est basse et réversible : `ioprio` best-effort 7 sous
  Linux, `PRIO_DARWIN_BG` sous macOS.

Le relevé `GET /system/background-tasks` → `playback_priority` dit quelles
passes ont cédé, la politique en vigueur et les compteurs des freins.

### L'ordonnancement

- **Exécuteur tokio principal** : multi-fil, réglages par défaut. Il porte
  l'API, l'orchestrateur, les sorties réseau et les boucles des passes de fond.
- **Transport des flux audio HTTP** (#4645, `aiguillage_des_flux`) : un moteur
  tokio à lui (2 fils), avec son propre pool bloquant. Un gel de l'exécuteur
  principal suspend l'API, pas les octets envoyés au renderer.
- **Pool bloquant tokio** : il est partagé. On y trouve les segments de
  décodage des passes, les écritures de fond (`hors_du_fil_async`, fil marqué
  « de fond »), et aussi le travail de premier plan.
- **rayon** : le pool `scan-io` du scan a des fils à lui, à `nice 10` et
  `ioprio` best-effort 7 en permanence, marqués « de fond ». Le pool global
  sert le premier plan (UPnP, tri, DIDL). Le scan automatique a aussi un pool
  de `stat`.
- **Fils à part** : `sqlite-replieur-wal` (repli `PASSIVE` toutes les 2 s,
  priorité normale), la surveillance du système de fichiers, le décodage en
  continu de la sortie locale.
- **Fil de rendu de la sortie locale** (#3206, `audio::ordonnancement_rt`) : il
  demande `SCHED_FIFO` quand `RLIMIT_RTPRIO` le permet (Tune OS :
  `LimitRTPRIO=95`). Sinon, il prend un `nice` négatif si `RLIMIT_NICE` le
  permet. **Ce chantier n'y touche pas, et ne baisse jamais la priorité d'un
  fil de lecture.**

## 2. Les trous qui restent

1. **La veille de lecture des analyses bloque l'exécuteur.** `veiller_lecture`
   court contre chaque fichier analysé. Elle fait toutes les 250 ms une requête
   SQLite *synchrone* sur un fil de l'exécuteur principal. C'est aussi le cas
   des gardes de tour de la cascade ReplayGain, de la plage dynamique et de la
   boucle acoustique. Le pool de lecture n'a que 3 connexions. Quand il est
   saturé (scan, passe d'album), cette requête attend une connexion en tenant
   le fil. Ce défaut est mesuré sur cette requête-là chez un testeur :
   `attente_ms=8419 sql=SELECT name FROM zones WHERE last_play_state =
   'playing'`. #5138 l'a corrigé dans la seule passe acoustique. L'exécuteur
   gelé, c'est la classe #5438 / #5444 : les sorties réseau et l'orchestrateur
   attendent. Et cela arrive précisément au moment où une lecture démarre.
2. **Aucune priorité processeur pour le travail de fond partagé (Linux).**
   Sous Linux, `nice` ne se défait pas sans `RLIMIT_NICE` (absent de l'unité
   Tune OS) ni `CAP_SYS_NICE`. On ne peut donc pas l'appliquer à un fil du pool
   partagé. Seuls les fils `scan-io` sont baissés, parce qu'ils sont à eux.
3. **Windows : rien.** `PrioriteBasse` est neutre.
4. **Les gardes synchrones d'avant chaque fichier** (`en_parallele_borne` du
   ReplayGain, des empreintes et du rattrapage DR) font encore la requête sur
   l'exécuteur, avant chaque fichier.
5. **L'écran « ce qui tourne »** dans État du serveur. C'est l'arbitrage de
   Bertrand du 10/10. Le relevé existe côté serveur, l'écran web n'existe pas.

## 3. Le mécanisme proposé, par étapes

### Étape 1 (livrée ici) : la veille de lecture hors de l'exécuteur

C'est le meilleur rapport gain/risque. Le défaut est mesuré sur le terrain. Le
correctif est déjà éprouvé dans la passe acoustique (#5138). Aucune décision
de cession ne change, seulement le fil qui pose la question.

- Nouvel assistant asynchrone `replaygain::zone_qui_joue` : la même requête
  (`playing_zone_name`), partie par `spawn_blocking`. Un fil de l'exécuteur ne
  l'attend plus jamais.
- Il sert à `veiller_lecture` (4 fois par seconde pendant l'analyse d'un
  fichier), aux gardes de tour de la cascade ReplayGain (le tour et la passe
  d'album), à la boucle de la plage dynamique à la demande et à la boucle de
  l'analyse acoustique.
- La requête de veille **n'est pas** marquée « de fond ». Elle est triviale,
  et sa réponse sert le premier plan : c'est elle qui arrête l'analyse quand la
  lecture démarre. La plafonner sous la réserve SQLite retarderait la cession,
  c'est-à-dire l'effet recherché.
- Le témoin en mémoire n'est **pas** substitué à la requête dans cette étape.
  Les témoins de test simulent la lecture par la base. Le témoin est un état
  global du processus, et une zone laissée « playing » par un autre test du
  même binaire ferait céder des analyses à tort : c'est le risque de flake déjà
  soupçonné en #5942 sur le parcours du scan.
- **Réglage** : `background_playback_policy.playback_watch_off_executor`
  (booléen, vrai par défaut). `false` rend exactement l'ancien comportement (la
  requête sur le fil de l'exécuteur). Il est indépendant de `enabled` : ce
  n'est pas un frein, et `enabled = false` ne doit pas ramener un gel.

### Étape 2 : les gardes d'avant chaque fichier

`en_parallele_borne` prend une garde synchrone (`FnMut() -> bool`). Pour la
sortir de l'exécuteur, il faut une garde asynchrone, ou une valeur rafraîchie
par une tâche à part. Cela touche trois passes et leurs témoins. C'est plus de
risque, pour un gain du même ordre que l'étape 1.

### Étape 3 : une priorité processeur basse qu'on peut tenir

- **Linux** : un pool bloquant **dédié** au calcul de fond (segments de
  décodage ReplayGain/DR/empreintes, ONNX). Ses fils sont à lui, donc
  `nice 10` (ou `SCHED_IDLE`) en permanence, sans avoir à le défaire, comme
  `scan-io`. Il ne prend jamais une écriture en base : l'inversion de priorité
  sur l'écrivain unique est expliquée dans `priorite.rs`. Les fils internes
  d'onnxruntime se baissent par le crochet de création de fils de la session.
- **Windows** : `SetThreadPriority(THREAD_MODE_BACKGROUND_BEGIN/END)` dans
  `PrioriteBasse`. C'est réversible et sans privilège, et cela baisse le
  processeur, les E/S et la mémoire. Il faut une porte Windows (compilation et
  essai sur une machine Windows), que Shrek n'a pas.
- **macOS** : `PRIO_DARWIN_BG` baisse déjà le processeur et les E/S. Il reste
  la porte macOS à jouer sur le Mac (non jouée en #5942).

### Étape 4 : l'écran « ce qui tourne »

Côté web, État du serveur, à partir de `GET /system/background-tasks` : les
passes, leur état (au travail, cédée à la lecture, en pause), la politique et
les compteurs des freins. Le serveur n'a presque rien à ajouter.

## 4. Pause, ralentissement, reprise

Rien ne change dans cette étape. Les passes qui décodent s'arrêtent. Les
autres ralentissent (une pause entre deux éléments, un scan plus étroit).
Toutes reprennent seules à l'arrêt de la dernière zone qui jouait, au plus
30 s après pour les passes qui décodent. Une écoute d'une journée ne gèle pas
l'enrichissement : il avance à un élément toutes les 5 s.

## 5. Risques

| Risque | Parade |
|---|---|
| `spawn_blocking` hors d'un runtime tokio | Tous les sites sont des `async fn` sous tokio. Un `JoinError` (panique, runtime arrêté) répond « rien ne joue », comme une erreur de requête aujourd'hui |
| La veille abandonnée en plein vol (le `select!` gagné par l'analyse) | La requête finit seule sur le pool bloquant, elle est triviale. Rien n'est attendu |
| Le pool bloquant est saturé | Son plafond tokio est de 512 fils. Une veille par fichier en vol, et la largeur est bornée |
| Un changement de cession | Aucun : même requête, même règle (« échec = rien ne joue »), même cadence |
| La flotte qui désactive par erreur | Le réglage est vrai par défaut et publié par `GET /system/config` ; `null` remet le défaut |

## 6. Mesure

- **Banc dans les tests** : un moteur tokio à un seul fil, et une base dont la
  requête de veille dure 600 ms (pool saturé simulé). Une tâche de premier plan
  bat toutes les 10 ms pendant une analyse simulée de 1,5 s. Avec le réglage,
  le plus long trou du battement reste sous 200 ms. Sans lui (contre-épreuve
  dans le même test), il dépasse 500 ms. Dans les deux cas, la veille détecte
  la lecture et l'analyse cède.
- **Terrain** : le chien de garde de l'exécuteur (`gel_executeur`) et
  `slow_query … attente_ms` sur `SELECT name FROM zones` ne doivent plus
  coïncider. À la prochaine coupure signalée, on rapproche son heure de
  `playback_priority` et des lignes INFO `tache_de_fond_*`, comme pour #4567.
- Ce qui reste à prouver : la disparition des micro-coupures. Seule une écoute
  réelle peut le dire.
