# Collecte et stockage

[← Sommaire](README.md)

Ce que mimic garde du trafic, où, combien de temps, et comment il en tire l'OpenAPI et les
snapshots. Sources : `crates/mimic-proxy/src/{proxy,capture,store,journal,share}.rs`,
`crates/mimic-core/src/{redact,sample,compact,exemplar,snapshot}.rs`,
`crates/mimic-controller/src/state.rs`.

## Chemin d'un échange

```
requête/réponse relayées ──► copie bornée des bodies (capture.max_body_bytes)
        │
        ▼
  réservoir par (sens, authority, méthode, forme du chemin, code) : body gardé ou non
        │
        ▼
  redaction (mimic_core::redact) ──► store mémoire du sidecar ──► journal local (store.dir)
                                                │
                                                └──► segments publiés au contrôleur (share)
                                                          │
                                                          ▼
                                     support du contrôleur (PVC ou S3) + mémoire + exemples
                                                          │
                                       découverte OpenAPI, snapshots, export, rejeu
```

**Rien n'est écrit (mémoire, disque, réseau) avant la redaction.** Un body qui ne sera pas
gardé n'est ni décodé, ni parsé, ni redacté : il ne laisse que sa taille.

## Ce qui est capturé

| Élément | Gardé | Réglage |
|---------|-------|---------|
| méthode, authority (`hôte:port`), chemin, query, code, horodatage, durée jusqu'au premier et au dernier octet | toujours (hors `passthrough`) | |
| headers de requête et de réponse | oui, redactés | |
| identifiant de trace | lu dans les headers de traçage de la requête | |
| bodies | décodés (`gzip`, `deflate`, `br`, `zstd`), jusqu'à `capture.max_body_bytes` (1 MiB) ; au-delà, préfixe et taille (`Truncated`) | `capture.bodies`, `capture.max_body_bytes` |
| bodies par couple (opération, code) | au plus `capture.samples_per_operation` (20), choisis par un réservoir déterministe (même chance pour chaque échange, quel que soit son rang) ; les autres échanges gardent leurs métadonnées sans body | `capture.samples_per_operation` |
| trafic entrant | `full`, `metadata` (mémoire seule, rien de publié) ou `off` | `capture.inbound`, `spec.capture.inbound` |
| trames WebSocket, flux TCP opaques, TLS non intercepté | jamais | |

L'opération est (sens, authority, méthode, **forme** du chemin) : les segments identifiants
(entier, UUID, ULID, hexadécimal, base64url, date) sont remplacés par leur nature, donc
`/orders/1` et `/orders/2` partagent les mêmes 20 places de body.

## Redaction

Appliquée par le sidecar à chaque échange avant le store (règle RED-04 du projet). Chaque
valeur sensible est remplacée par `<redacted:sha256:xxxxxxxx>` : les 8 premiers caractères
hexadécimaux de `sha256(sel ‖ valeur)`. Deux requêtes portant le même secret restent
reconnaissables entre elles sans que la valeur soit retrouvable.

| Cible | Règle |
|-------|-------|
| headers | `authorization`, `proxy-authorization`, `cookie`, `set-cookie`, et tout nom contenant `token`, `secret`, `key`, `session`, `password` ou `passwd` (sans casse) |
| paramètres de query | noms contenant ces mêmes sous-chaînes |
| bodies JSON | clés contenant ces sous-chaînes (la valeur entière, objet compris, est remplacée) ; chemins de `redact.extra_json_paths` (`$.customer.ssn`, sans index de tableau) ; toute chaîne qui ressemble à un e-mail, un numéro de carte (13 à 19 chiffres valides au contrôle de Luhn), un IBAN valide ou un JWT, quel que soit le nom du champ |
| bodies `application/x-www-form-urlencoded` | mêmes règles de nom et de valeur, paramètre par paramètre |
| body JSON ou formulaire tronqué ou illisible | remplacé par sa seule taille (`Skipped`) : on ne peut pas garantir qu'il ne contient pas de secret |
| **autres bodies** (texte, XML, `multipart/form-data`, binaire, protobuf/gRPC, sans `Content-Type`) | **non capturés** : seule leur taille est gardée (`mimic_capture_dropped_total{reason="unredactable"}`), sauf pour les hôtes de `redact.raw_bodies` (et `inbound` pour l'entrant), gardés tels quels sans redaction |

Un body où rien n'a été remplacé est gardé octet pour octet (ordre des clés, espaces) : le mock
le rejoue à l'identique.

Conséquences pour l'exploitation :

- Une réponse dont le body n'est pas capturé **n'est pas rejouable** : en `mock`, l'appel
  reçoit `599`, comme un appel jamais enregistré. Pour mocker une dépendance XML, texte ou gRPC,
  la lister dans `redact.raw_bodies` (annotation `mimic.platform.io/capture-raw-bodies:
  api.partner.io,inbound`) : ses bodies sont alors stockés **en clair**, en connaissance de
  cause.
- **Requête du mock.** En `mock` et `mock-partial`, la requête de l'application est redactée
  avec le même sel avant d'être comparée aux enregistrements : une requête qui porte un e-mail
  ou un jeton correspond à son enregistrement. Celle qui est relayée à la vraie dépendance
  (`mock-partial`) part intacte.
- **Contrôleur.** Chaque segment reçu est redacté de nouveau avec le sel du workload : un
  sidecar d'une version antérieure ne fait rien écrire de sensible sur le support.
- La sous-chaîne `key` rend sensibles des noms comme `keywords` ou `monkey` : leurs valeurs sont
  remplacées, ce qui peut appauvrir les schémas ou les exemples.
- **Sel, obligatoire.** Dans l'ordre : le Secret `<workload>-mimic`, clé `redact-salt`
  (`MIMIC_REDACT_SALT`) ; sinon, sous contrôleur, le **sel du workload**, que le contrôleur tire
  une fois, garde sur son support (`<ns>/<wl>/redact-salt`) et donne à chaque battement aux seuls
  pods du workload (le sidecar le garde en mémoire, jamais sur disque). Sans contrôleur ni sel,
  le sidecar refuse de démarrer. Le Secret reste utile pour partager un sel entre clusters
  (enregistrer en A, mocker en B) :

  ```sh
  kubectl -n shop create secret generic checkout-mimic \
    --from-literal=redact-salt="$(openssl rand -hex 32)"
  ```

  Un pod démarré par repli, sans réponse du contrôleur, n'enregistre rien tant qu'il n'a pas
  reçu le sel (`mimic_capture_dropped_total{reason="no_salt"}`).

> **Écart avec la spec.** La spec (§7) prévoit des « regex additionnels » par projet : seuls
> `redact.extra_json_paths` (sans index de tableau) existent. Le code ajoute `passwd` aux motifs
> de la spec, et ne redacte que les bodies JSON et formulaire.

## Stockage côté sidecar

| Niveau | Contenu | Bornes | Perte |
|--------|---------|--------|-------|
| mémoire | échanges du pod (et, en partage direct, ceux des pairs) | `capture.max_exchanges` (50 000), `capture.max_store_bytes` (64 MiB, bodies au plus pour moitié) ; au-delà, bodies retirés puis échanges évincés, du plus ancien au plus récent (`mimic_capture_dropped_total{reason="store_budget"}`) | au redémarrage du conteneur, sauf rechargement du journal |
| journal local | segments `local-<n>.jsonl` dans `store.dir` (`/var/lib/mimic`, `emptyDir` de 512 Mi avec l'injecteur), rechargés au démarrage (`store local rechargé`) | `store.segment_bytes` (32 MiB) × `store.max_segments` (4) ; file d'écriture bornée, pleine : ligne abandonnée (`reason="local_backlog"`) | à la suppression du pod |
| file de publication | échanges pas encore publiés au contrôleur | `share.max_backlog_bytes` (16 MiB), `share.max_pending` (50 000) ; au-delà, les plus anciens sont abandonnés (`reason="share_backlog"`) | **perte définitive** pour le contrôleur |
| cache contrôleur | `controller/desired.json` (dernier état souhaité), `controller/snapshots/<id>.mock.json` (formes de mock) dans `store.dir` | `store.max_cached_snapshots` (5) ; au-delà, les formes les plus anciennes sont supprimées, jamais celle active (SNP-12) | à la suppression du pod, ou plus tôt si purgé |

Le store mémoire est une fenêtre glissante : `store_budget` monte normalement dès qu'il est
plein, ce n'est pas un signal d'alerte. `share_backlog`, lui, signifie que des échanges
n'atteindront jamais le contrôleur.

## Publication vers le contrôleur

Toutes les `share.interval` (10 s), le sidecar envoie ses nouveaux échanges en segments JSONL
d'au plus 8 MiB, chacun sous un identifiant stable (`?instance=&segment=`) : un renvoi après
un échec réseau est reconnu comme doublon (`200`) au lieu d'être compté deux fois. Après n
échecs consécutifs, l'attente passe à `interval` × 2ⁿ, plafonnée à 300 s, avec 50 % de gigue.
À l'arrêt du pod, une dernière publication est tentée pendant 5 s au plus.

Sous contrôleur, le sidecar **ne lit jamais** les segments de ses pairs. Sans contrôleur, le
partage direct (`share.url: file:///…` ou `s3://bucket/prefixe`) écrit sous
`<prefixe>/<namespace>/<workload>/exchanges/<instance>/` et importe les segments des autres
instances de moins de `share.retention` (24 h) ; chaque pod construit alors son mock sur ce
qu'il a importé, sans garantie que tous les pods servent les mêmes données.

## Stockage côté contrôleur

Support : `file:///var/lib/mimic-controller` (PVC `mimic-controller`, 10 Gi, `ReadWriteOnce` dans les manifests : voir [Exploiter en production](Exploiter-en-production.md#stockage-du-contrôleur) pour plusieurs nœuds)
ou `s3://bucket/prefixe` (identifiants `AWS_*`, IRSA par exemple). Seule la réplique active
y écrit. Arborescence par workload :

| Chemin | Contenu | Durée de vie |
|--------|---------|--------------|
| `<ns>/<wl>/exchanges/<instance>/<segment>.jsonl` | segments reçus des sidecars | `retention` (24 h) ; plafond `max_bytes_per_workload` (256 MiB), les plus anciens supprimés au-delà |
| `<ns>/<wl>/exchanges/_hour/<AAAAMMJJHH>.jsonl` | segments d'une heure compactés en un seul, une fois l'heure révolue depuis 10 min, avec au plus `samples_per_operation` bodies par (opération, code) | idem |
| `<ns>/<wl>/exemplars.jsonl` | un **exemple** rejouable par endpoint (opération, code) | **hors rétention** |
| `<ns>/<wl>/snapshots/<id>.jsonl`, `<id>.meta.json` | snapshots construits ou importés, et leurs métadonnées (date, échanges, épinglé) | purgés au-delà de `snapshot.keep_last` (5), hors référencés et épinglés (§5.6) |
| `<ns>/<wl>/desired.json` | dernier état souhaité | permanent |

Passe de maintenance toutes les 60 s : exemples, rétention, compaction, plafond de volume. Un
segment n'est supprimé qu'après que ses échanges ont été proposés aux exemples et que ceux-ci
ont été écrits (`écriture des exemples : aucun segment supprimé` sinon).

### Mémoire du contrôleur

Au démarrage (et à chaque prise de relais), la réplique recharge du support les segments dans
la rétention et les exemples (`état persisté rechargé`). En mémoire : au plus
`max_exchanges_per_workload` (200 000) échanges par workload, `max_exchanges_total`
(1 000 000) au total (le workload qui en a le plus perd ses plus anciens) ; les manifests
estiment environ 2 Ko par échange.

### Exemples par endpoint

Pour chaque endpoint, le contrôleur garde l'échange rejouable le plus récent, même au-delà de
la rétention : un appel rare (job de nuit, cas d'erreur) reste mockable des semaines plus tard.
Bornes : `max_exemplars_per_workload` (10 000 endpoints ; au-delà, nouvel endpoint sans
exemple, `mimic_controller_exemplars_refused_total{reason="count"}`),
`max_exemplar_bytes_per_workload` (64 MiB) et `max_exemplar_bytes_total` (512 MiB) (au-delà,
exemple gardé **sans body**, visible mais pas rejouable, `reason="bytes"`).

## Snapshots

Un snapshot est construit par la réplique active quand une `MimicWorkload` entre en `mock` ou
`mock-partial` sans `spec.snapshot` :

- échanges **sortants** avec réponse, en mémoire et parmi les exemples, au plus
  `snapshot.max_per_key` (50) par (authority, méthode, chemin, code), les plus récents ;
- triés par date puis identifiant, dédoublonnés : le résultat ne dépend pas de l'ordre
  d'arrivée ;
- identifié par les 16 premiers caractères hexadécimaux du sha256 de son contenu JSONL (log
  `snapshot construit`) ;
- servi aux sidecars sous sa **forme de mock** : échanges rejouables et durées par opération,
  compressée (`zstd` ou `gzip`), avec un `ETag`, gardée en mémoire jusqu'à 128 MiB.

Le snapshot reste celui du workload tant qu'il ne quitte pas le mock. Pour rejouer exactement
le même jeu d'un tir à l'autre, noter `status.snapshot` et le reporter dans `spec.snapshot`
(voir [Basculer en mock](Basculer-en-mock.md)). Lister les snapshots d'un workload :
`mimicctl snapshot ls` (ou `/v1/workloads/<ns>/<wl>/snapshots` sur l'interface du contrôleur,
voir [Administration et API](Administration-et-API.md)).

**Cycle de vie.** À chaque passe de maintenance (toutes les minutes), les snapshots au-delà de
`snapshot.keep_last` (5 par défaut) sont purgés, sauf les **référencés** (`spec.snapshot` ou
`status.snapshot` courants d'une `MimicWorkload`) et les **épinglés**
(`mimicctl snapshot pin`), jamais purgés quel que soit leur âge. `mimicctl snapshot gc`
déclenche la même purge immédiatement ; `mimicctl snapshot import` recharge un snapshot
exporté ailleurs, épinglé par défaut. Voir
[Exploiter en production](Exploiter-en-production.md#stockage-du-contrôleur).

## Découverte OpenAPI

Calculée à la demande (page d'admin du sidecar, interface du contrôleur, `mimicctl export`),
au plus une fois par `discovery_interval` (30 s) par workload côté contrôleur. Le sidecar ne
décrit que son pod ; le contrôleur décrit le workload entier (mémoire et exemples).

| Élément | Règle |
|---------|-------|
| documents | un pour l'entrant (API du service), un par authority sortante (`info.x-mimic-direction`) ; OpenAPI 3.1.0, conversion 3.0 à l'export |
| templates de chemin | arbre par authority, toutes méthodes confondues : segment identifiant toujours variable ; segment présent dans plus de 10 % des passages et non identifiant, littéral (`/users/me`) ; slug variable ; mot alphabétique variable au-delà de 20 valeurs distinctes ; paramètre créé à partir de 5 valeurs distinctes |
| schémas | fusion des bodies JSON observés, formats `date-time`, `date`, `uuid`, `email`, `uri`, `ipv4`, `enum` sous seuils |
| exemples | 3 par body et type de contenu, déjà redactés |
| headers | headers de transport, de proxy, de traçage et d'authentification ignorés (`x-forwarded-*`, `x-b3-*`, `x-envoy-*`, `x-amzn-*`, `sec-*`…) |
| statistiques | `x-mimic-stats` par opération : `count`, `rate_rps_p95`, `status`, `latency_ms` (`p50`, `p90`, `p99`, `max`) |

Le résultat est déterministe : mêmes échanges, même document, quel que soit leur ordre.
