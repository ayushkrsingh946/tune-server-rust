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

### Étape 1 (livrée, #6301) : la veille de lecture hors de l'exécuteur

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

### Étape 2 (livrée, go du 13/10) : les gardes d'avant chaque fichier

`en_parallele_borne` relit ses gardes (réglage, pause, lecture) avant CHAQUE
fichier lancé. Elles posaient sur un fil de l'exécuteur la même requête que
la veille, et, pour le ReplayGain, la lecture du réglage.

- La garde est maintenant un FUTUR (`peut_lancer: FnMut() -> impl Future<Output = bool>`).
- `replaygain::garde_hors_executeur` pose la garde ENTIÈRE sur le pool
  bloquant, d'un seul tenant : mêmes lectures, même ordre, même réponse.
- Les quatre sites : la passe nominale ReplayGain, les empreintes, le
  rattrapage de la plage dynamique, le rattrapage des crêtes vraies.
- Une garde qui panique répond « ne lance pas » : la passe s'arrête à la
  frontière de fichier et le tour suivant reprend.
- Même réglage que la veille (`playback_watch_off_executor`).
- Témoin `les_gardes_d_avant_chaque_fichier_ne_gelent_pas_l_executeur` : la
  vraie passe nominale sur trois fichiers, un moteur à un fil, une requête de
  600 ms ; trou < 200 ms avec le réglage, ≥ 500 ms sans (contre-épreuve dans
  le test).

Hors de ce chantier, d'autres passes posent encore la même requête sur place :
les paroles (`library/lyrics_pass.rs`), la page du scan
(`routes/system/scan.rs`). Elles ne décodent pas ; à traiter si une mesure les
désigne.

### Étape 3 (livrée, go du 13/10) : une basse priorité processeur et disque qu'on peut tenir

`taches_de_fond::priorite::calcul_de_fond` : des fils À LUI pour le calcul de
fond, baissés une fois pour toutes à leur naissance, comme `scan-io` pour le
scan.

| Système | Processeur | Disque |
|---|---|---|
| Linux | `nice 10` | `ioprio` classe IDLE, à défaut best-effort 7 |
| macOS | QoS `BACKGROUND` | idem (la QoS bride aussi les E/S) |
| Windows | `THREAD_MODE_BACKGROUND_BEGIN` | idem (E/S et mémoire aussi) |

- **Ce qui y va** : les segments de décodage et de mesure du ReplayGain et de
  la plage dynamique (`analyzer::parcourir_les_segments`, la tête partagée
  avec l'empreinte, `MesureEnCours::finir`), l'empreinte seule
  (`replaygain::calculer_l_empreinte`), le décodage de l'analyse acoustique
  et son inférence ONNX.
- **Ce qui n'y va jamais** : une écriture en base. L'inférence acoustique a
  été séparée de ses écritures : le calcul part sur les fils baissés, le
  vecteur et le témoin s'écrivent ensuite sur le pool bloquant ordinaire. Un
  fil de lecture non plus : rien ne baisse le fil appelant
  (`seuls_les_fils_du_pool_sont_baisses`), et aucun fichier de `outputs/`,
  `streaming/`, `orchestrator` ni `audio/` hors des trois passes n'appelle
  le pool (`aucun_chemin_de_lecture_n_y_passe`). `audio::ordonnancement_rt`
  n'est pas touché.
- **IDLE et non best-effort 7** : ces passes cèdent TOUT à la lecture ; hors
  lecture, un disque occupé par le scan les fait attendre, c'est voulu. Le
  scan, lui, reste en best-effort 7 (il doit avancer pendant la lecture).
- **Élastique** : un fil naît quand aucun n'est libre (64 au plus), meurt
  après 60 s sans travail. Un pool fixe se boucherait sur un décodeur qui
  tourne en rond (la passe abandonne l'attente, pas le fil).
- **Armé par le serveur** : `spawn_background_tasks` appelle
  `calcul_de_fond::armer()` avant toute passe. Les binaires de test ne
  l'appellent pas : des milliers de tests en parallèle ne doivent pas
  affamer les analyses qu'ils chronomètrent. Le témoin de la baisse prend
  son propre pool armé.
- **`PrioriteBasse`** (la baisse réversible des fils PARTAGÉS pendant la
  lecture) gagne Windows (`THREAD_MODE_BACKGROUND_BEGIN/END`), et devient
  neutre sur un fil du calcul de fond (son `drop` le relèverait).
- **Réglage** : `background_playback_policy.background_compute_low_priority`
  (vrai par défaut, publié par `GET /system/config`). `false` renvoie le
  calcul sur le pool bloquant de tokio. Indépendant de `enabled` : ce n'est
  pas un frein de lecture, il vaut tout le temps.
- **Portes** : Linux sur Shrek (témoins réels). Windows : compilation croisée
  sur Shrek (`cargo check --target x86_64-pc-windows-msvc`), **l'essai sur
  une machine Windows reste à faire**. macOS : la porte se joue sur le Mac.
- **Reste** : les fils internes d'onnxruntime au-delà du premier (2 fils
  d'inférence sur plus de 8 cœurs) naissent à la priorité normale ; le crochet
  de création de fils de la session ne l'est pas dans ce lot.

### Étape 4 : l'écran « Ce qui tourne »

Côté web, État du serveur, à partir de `GET /system/background-tasks` (et du
même corps rendu par les routes de pause). Le serveur ajoute, sans rien
retirer :

```jsonc
{
  "pausable": [
    {
      "id": "replaygain",            // replaygain, fingerprints, dynamic_range, acoustic,
                                     // enrichment, artist_images, identification
      "state": "en_cours",           // existant : en_cours | en_pause | au_repos
      "paused": false,               // existant
      "throttled_for_playback": true,// existant
      "activity": "yielded",         // NOUVEAU : working | throttled | yielded | paused | idle
      "reason": "playback"           // NOUVEAU : playback | user_pause | null
    }
  ],
  "scan": {
    "running": true, "throttled_for_playback": true,   // existant
    "activity": "throttled", "reason": "playback"      // NOUVEAU (working | throttled | idle)
  },
  "playback_priority": {
    "playback_active": true, "playing_zone_ids": [3], "since_epoch_s": 1760360000,
    "throttled": ["replaygain", "scan"], "pause_between_items_ms": 5000,
    "policy": { "...": "...", "playback_watch_off_executor": true,
                "background_compute_low_priority": true },          // NOUVEAU champ
    "brakes": { "...": "..." },
    "background_compute": {                                         // NOUVEAU bloc
      "low_priority_pool": true,  // la politique envoie le calcul aux fils baissés
      "armed": true,              // le serveur a armé la baisse
      "cpu": "nice_10",           // nice_10 | qos_background | thread_mode_background | none | null
      "io": "idle",               // idle | best_effort_7 | qos_background | thread_mode_background | none | null
      "threads": 2, "idle_threads": 1, "jobs_done": 18342
    }
  }
}
```

- `yielded` : une passe qui DÉCODE (ReplayGain, empreintes, plage dynamique,
  acoustique) a tout cédé à une zone qui joue ; elle reprend seule à l'arrêt.
- `throttled` : une passe qui ne décode pas (enrichissement, images,
  identification, scan) avance au ralenti pendant la lecture.
- `paused` : l'utilisateur l'a suspendue (`reason: user_pause`).
- Une cession notée ne vaut que tant qu'une zone joue
  (`playback_active`).
- `cpu`/`io` valent `null` tant qu'aucun fil du calcul de fond n'est né.

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
