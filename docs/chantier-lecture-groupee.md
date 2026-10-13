# Chantier « lecture groupée » (#6132)

Go de Bertrand le 13/10/2026, après l'étude `docs/etude-6132.md` (PR #6302,
branche `chantier/entree-usb`). Branches du chantier : `chantier/lecture-groupee`
sur le serveur (depuis `batch/bugs-rc5-20261012`) et sur le client web (depuis
`main`).

## Décisions de Bertrand (13/10)

1. Une **vraie lecture groupée** : la même écoute part sur plusieurs zones.
   Aujourd'hui le groupement enregistre une liste et règle le volume ensemble,
   rien de plus.
2. Pour **deux sorties locales** (deux cartes son de la même machine), une
   **sortie double** qui écrit le même flux sur les deux périphériques, avec
   correction de dérive d'horloge (rééchantillonnage fin piloté par l'écart
   mesuré), pour rester synchrones.
3. **ASIO** garde « une zone à la fois » (limite du pilote) ; l'écran
   l'explique.
4. Les écrans Audio / Zones / Appareils deviennent deux écrans : **« Zones »**
   (ce que joue chaque zone) et **« Cette machine »** (les sorties de la
   machine). L'onglet Appareils disparaît. La Lecture groupée devient visible
   ailleurs qu'au clic droit.

## Étape 1 — la sortie double synchrone (serveur) : livrée par cette PR

### Le principe

Un pont d'horloges, comme `zita-ajbridge` ou `alsa_out` :

- le périphérique de la zone (**A**) est le maître : son rappel de rendu donne
  la cadence ;
- ce que A joue est recopié, période par période, vers le second périphérique
  (**B**), par un **rééchantillonneur sinc polyphasé** (64 coefficients,
  fenêtre de Blackman-Harris) dont le rapport vaut `cadence B / cadence A`
  corrigé de quelques ppm ;
- la correction est **pilotée par l'écart mesuré** : le remplissage de
  l'anneau de B, lissé, comparé à sa consigne. Un régulateur
  proportionnel-intégral (amorti critique) le ramène à la consigne ; sa part
  intégrale est l'estimation de la dérive des deux quartz ;
- A est retardé de la consigne (40 ms) par une ligne à retard, pour jouer au
  même instant que B. La consigne retranche les deux retards propres à B (la
  demi-largeur du filtre, la demi-période de B) ;
- deux régimes : un accrochage rapide pendant les 15 premières secondes (l'écart
  de départ vaut jusqu'à une période de A), puis une boucle lente qui ne suit
  plus que la dérive, sans moduler la hauteur.

### Où c'est dans le code

| Fichier | Rôle |
|---|---|
| `tune-core/src/outputs/sortie_double.rs` | le recopieur (filtre, régulateur, ligne à retard), sans cpal ni `local-audio` : jugé sous Linux |
| `tune-core/src/outputs/sortie_double_tests.rs` | le banc : deux périphériques simulés, d'horloges différentes, plusieurs minutes |
| `tune-core/src/outputs/local/sortie_double_locale.rs` | ouverture de B sur le même hôte cpal, son rappel, l'emplacement que le rappel de A consulte, les refus |
| `tune-core/src/outputs/local/backend.rs` | le rappel `f32` partagé de A appelle `recopier_la_periode` ; `BackendCpal::brancher_la_seconde_sortie` |
| `tune-core/src/outputs/local.rs` | `LocalOutput::set_seconde_sortie` ; `play_url` branche B après l'ouverture de A, avant son démarrage |
| `tune-core/src/orchestrator/transport.rs` | à chaque lecture locale, pose la seconde sortie lue dans le réglage `zone_{id}_sortie_double` |

Le chemin d'appel réel : `transport.rs` (lecture d'une zone locale) →
`LocalOutput::set_seconde_sortie` → `play_url` → `BackendCpal::ouvrir` →
`brancher_la_seconde_sortie` → `ouvrir_la_seconde_sortie` (B ouvert, recopie
installée) → à chaque période, le rappel de A → `recopier_la_periode`.

Le réglage `zone_{id}_sortie_double` (valeur `local:<nom>` ou `<nom>`) est de
la plomberie : aucun écran ni route ne l'écrit encore. L'étape 2 le posera
depuis un groupe.

### Ce qui est refusé, et dit dans le journal (`sortie_double_refusee`)

Dans ces cas la zone joue sur A seule, jamais de silence :

- **ASIO** (`asio_un_seul_flux`) : un seul flux par processus ;
- **mode exclusif** WASAPI ou CoreAudio (`mode_exclusif`), **chemin compressé**
  (`chemin_compresse`), **format entier** de A (`format_de_a`) : la recopie
  n'existe que sur le rappel `f32` partagé ;
- **« bit-perfect strict »** (`bitperfect_strict`) : B est rééchantillonné ;
- B introuvable sur l'hôte ouvert (aucun repli sur la sortie par défaut), B
  identique à A, ou B sans le nombre de canaux de A.

À l'arrêt, `sortie_double_releve` journalise l'écart, la dérive estimée, la
correction et les échantillons refusés par l'anneau de B.

### Mesure (banc simulé, Shrek)

Passage du 13/10 (`cargo test -p tune-core --lib sortie_double`) :

- 48 kHz / 48 kHz, A à +100 ppm, B à −80 ppm (10,8 ms de dérive par minute sans
  correction), périodes de 480 et 512 trames, **10 minutes** : écart A/B de
  0,19 ms au pire entre 20 s et 10 min (0,03 ms après la première minute),
  0,008 ms à la fin, dérive estimée −179,4 ppm pour −180, aucune famine de B ;
- 44,1 kHz vers 48 kHz, ±250 ppm, stéréo, 4 minutes : écart de 0,05 ms au pire
  entre 20 s et 4 min, dérive estimée +500,4 ppm pour +500 ;
- le signal recopié est à la position annoncée à 5,5·10⁻⁷ près (sinus 1 kHz,
  +1 000 ppm, correction en mouvement) ;
- au départ, l'écart vaut jusqu'à une période de A (B ne sait pas où A en est
  dans sa période) et se résorbe en moins de 5 s.

Ce que le banc ne mesure pas : la **latence matérielle** de chaque carte (tampon
du pilote, convertisseur). Elle est constante, elle ne dérive pas, mais elle
diffère d'une carte à l'autre (quelques ms). Voir les décisions.

## Étape 2 — la lecture groupée (serveur)

1. **Propager la lecture d'un groupe.** À la lecture sur le meneur, les suiveurs
   suivent : même élément, même position, mêmes commandes (pause, reprise,
   saut, suivante). Le groupe devient un état du transport, pas seulement une
   liste dans `zone_groups`.
2. **Choisir le transport par paire de membres** :
   - deux zones locales de la même machine : la **sortie double** de l'étape 1
     (le meneur porte la seconde sortie ; généraliser à N sorties) ;
   - des points de diffusion OAAT : le groupe OAAT existant ;
   - des renderers réseau indépendants (UPnP, AirPlay, Chromecast…) : départs
     alignés au mieux, sans garantie d'horloge — à dire dans l'interface.
3. **ASIO** : `validate_group` refuse, avec un motif lisible, un groupe qui
   demanderait deux flux ASIO ; le motif sert à l'écran.
4. **Brancher ce qui est enregistré sans consommateur** : `sync_delay_ms`
   (compensation statique par membre, y compris la latence matérielle d'une
   carte), `calibrate_group` (501 aujourd'hui), `GroupManager` (sans appelant).
5. **Une seule famille de routes** : `/zone-manager/groups` écrit le même
   réglage que `/zones/group` sans validation ni contrôle Premium ; la ramener
   sur la validation commune ou la retirer.
6. Mettre à jour `generic_group_synchronization_contract()` (aujourd'hui
   `supported: false`) et `docs/getting-started` (chemin « Groupes de zones »
   qui n'existe plus).

## Étape 3 — les écrans (client web)

1. **« Zones »** : ce que joue chaque zone. Une carte par zone qui ouvre tous ses
   réglages (sortie, audio, canaux, correction de pièce) ; la fenêtre du clic
   droit devient un raccourci vers cette carte. Les deux interrupteurs de
   Réglages ▸ Zones y viennent.
2. **« Cette machine »** : les sorties de la machine — backend et mode exclusif,
   visibilité des sorties et des appareils, services réseau.
3. L'onglet **Appareils** disparaît (la sauvegarde des réglages trouve une
   autre place).
4. **Lecture groupée visible** : un geste « Grouper » sur la page Zones, les
   groupes affichés comme tels (membres, transport utilisé, synchronisé ou
   non), dégrouper ; la pastille de la barre de lecture montre le groupe.
5. **ASIO** : l'écran explique « une zone à la fois sur cette machine » là où
   le groupement est refusé.

## Reste ouvert sur l'étape 1

- latence matérielle par carte (horodatages cpal `OutputCallbackInfo`, ou
  réglage manuel `sync_delay_ms`) ;
- chemin compressé, mode exclusif, adaptation des canaux, plus de deux sorties ;
- un relevé lisible par l'API de diagnostic (aujourd'hui : le journal).
- l'essai sur deux vraies cartes (Windows WASAPI partagé, Linux ALSA).

Refs #6132
