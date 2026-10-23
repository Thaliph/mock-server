# Architecture

[← Sommaire](README.md)

## Vue d'ensemble

```
                         Namespace applicatif (label mimic.platform.io/enabled=true)
  ┌──────────────────────────────────── Pod ────────────────────────────────────┐
  │                                                                             │
  │  client ──► [iptables PREROUTING ─► :15106] ──► mimic ──► application :8080 │
  │                                                  │  ▲                       │
  │  application ──► [iptables OUTPUT ─► :15101] ────┘  │                       │
  │                                                  │  │ admin :15100          │
  │                     mimic (UID/GID 1338) ────────┼──┘ /metrics /healthz     │
  │                                                  │                          │
  └──────────────────────────────────────────────────┼──────────────────────────┘
                 record : vers la vraie dépendance ◄─┤
                 mock   : réponse servie par mimic   │
                                                     │ HTTPS :8443, jeton de ServiceAccount
                                                     ▼ (battement, segments, snapshot)
  ┌──────────────────────── mimic-system ─────────────────────────────────────────┐
  │  Service mimic-controller:8443 ──► réplique active (label leader=true)        │
  │                                     réplique passive (prête, répond 503)      │
  │  support : PVC file:///var/lib/mimic-controller ou s3://…                     │
  │  interface 127.0.0.1:8081 (kubectl port-forward)                              │
  │  mimic-injector:443 ◄── API server (MutatingWebhookConfiguration)             │
  └───────────────────────────────────────────────────────────────────────────────┘
        ▲ watch MimicWorkload, Namespaces ; patch status ; TokenReview ; Lease
        │
     API Kubernetes ◄── kubectl / mimicctl (RBAC sur mimicworkloads)
```

## Composants

| Composant | Binaire | Où il tourne | Rôle |
|-----------|---------|--------------|------|
| sidecar | `mimic-proxy` | conteneur `mimic`, native sidecar (init container `restartPolicy: Always`) de chaque pod injecté | proxy transparent L7 : relaie, enregistre, mocke ; découverte locale ; API et page d'admin ; métriques |
| init | `mimic-init` | init container `mimic-init` (root, `NET_ADMIN`, `NET_RAW`), avant le sidecar | pose les règles iptables (table `nat`, chaînes `MIMIC_INBOUND` et `MIMIC_OUTBOUND`), détecte un mesh |
| contrôleur | `mimic-controller` | Deployment `mimic-system/mimic-controller`, 2 réplicas dont 1 active | ingestion des segments, découverte du workload entier, snapshots, état souhaité, statut des `MimicWorkload`, interface web |
| webhook | `mimic-injector` | Deployment `mimic-system/mimic-injector`, 2 réplicas | ajoute `mimic-init`, le sidecar, les volumes et variables à la création des pods |
| CLI | `mimicctl` | poste ou CI | bascule, statut, export, rejeu, rapport, diff |
| bibliothèque | `mimic-core` | dans tous les binaires | inférence OpenAPI, redaction, moteur de mock, snapshots, élection : fonctions pures |

Les quatre premiers binaires sont dans **une seule image** (`Dockerfile`) dont le point d'entrée
est `mimic-proxy` et l'utilisateur par défaut `1338:1338`.

## Ports et identités

| Port / identité | Composant | Écoute par défaut | Usage |
|-----------------|-----------|-------------------|-------|
| 15100 (`mimic-admin`) | sidecar | `0.0.0.0:15100` (configuration de l'injecteur) | API et page d'admin, `/metrics`, `/healthz`, `/readyz` |
| 15101 | sidecar | `127.0.0.1:15101` | écoute sortante : trafic redirigé par `OUTPUT`, ou proxy explicite (`HTTP_PROXY`) |
| 15106 | sidecar | `0.0.0.0:15106` | écoute entrante : trafic redirigé par `PREROUTING` (ou par `OUTPUT` dans un mesh) |
| UID et GID 1338 | sidecar | | trafic du sidecar jamais redirigé (`-m owner --uid-owner 1338 -j RETURN`), ce qui évite les boucles |
| 8443 (`api`) | contrôleur | `0.0.0.0:8443`, HTTPS | API des sidecars ; Service `mimic-controller:8443` |
| 8081 | contrôleur | `127.0.0.1:8081`, HTTP | interface en lecture seule, API de lecture, `/metrics` ; joignable seulement par port-forward |
| 8443 (`webhook`) | webhook | `0.0.0.0:8443`, HTTPS | `POST /mutate`, `/healthz`, `/metrics` ; Service `mimic-injector:443` |

Ces valeurs sont choisies hors de celles des mesh : Istio utilise l'UID 1337 et les ports
15000, 15001, 15006, 15008, 15020, 15021, 15053, 15090 ; Linkerd l'UID 2102 et les ports 4140,
4143, 4190, 4191 (voir [Service mesh](Service-mesh.md)). Les ports 15100, 15101 et 15106 sont
toujours exclus des redirections par `mimic-init`.

## Flux

### Entrant (client → application)

1. Le paquet arrive sur le pod ; la chaîne `MIMIC_INBOUND` (sautée depuis `PREROUTING`) le
   redirige vers `:15106` pour les ports interceptés (`MIMIC_INBOUND_PORTS` : liste ou `*`).
2. Le sidecar lit la destination d'origine (`SO_ORIGINAL_DST`), regarde les premiers octets
   (HTTP/1, préface HTTP/2, TLS, autre, ou rien au bout de 300 ms) et relaie vers
   l'application : d'abord l'IP du pod sur le port d'origine, sinon `127.0.0.1` sur ce port
   (choix mémorisé par port, refait après un échec ; `inbound.upstream` impose une adresse).
3. Hors `passthrough`, l'échange est enregistré selon `capture.inbound` (`full`, `metadata`,
   `off`). Le mode `mock` ne touche **jamais** l'entrant.

### Sortant (application → dépendance)

1. La chaîne `MIMIC_OUTBOUND` (sautée depuis `OUTPUT`) laisse passer le trafic de l'UID 1338,
   le trafic local du pod (`-o lo` : `127.0.0.1`, `::1`, IP du pod, donc les autres
   conteneurs du pod, comme sous Istio), les CIDR et ports exclus, puis redirige le reste du
   TCP vers `:15101`.
2. Le sidecar détecte le protocole. HTTP : traité selon le mode (relayé et enregistré, ou servi
   par le moteur de mock). TLS vers un hôte de `tls.intercept` : terminé par le sidecar avec un
   certificat forgé, puis traité comme HTTP. Autre TLS : tunnel opaque (refusé en `mock`).
   Autre TCP : tunnel opaque (refusé en `mock`, sauf `mock.tcp: relay`).
3. Les headers propres à la connexion ne sont jamais relayés ; HTTP/2 repart en HTTP/2 (gRPC).

Détail par mode : [Modes](Modes.md).

### Sidecar ↔ contrôleur

Le sidecar appelle `https://mimic-controller.mimic-system.svc:8443/v1/workloads/<namespace>/<workload>/…`
avec un jeton de ServiceAccount projeté (audience `mimic-controller`), en ne faisant confiance
qu'à la CA du contrôleur (`share.ca_file`).

| Appel | Fréquence | Contenu |
|-------|-----------|---------|
| `POST …/heartbeat` | toutes les `share.mode_poll` (2 s) | annonce `{instance, mode, snapshot, capture_inbound, mock_websocket}` ; la réponse est l'état souhaité **pour ce pod** |
| `POST …/segments?instance=&segment=` | toutes les `share.interval` (10 s), avec reprise exponentielle après échec | échanges déjà redactés, en segments JSONL d'au plus 8 MiB, sous un identifiant stable (envoi idempotent) |
| `GET …/snapshots/{id}/mock` | une fois par snapshot | forme de mock du snapshot, compressée (`zstd` ou `gzip`), vérifiée par son `ETag` |

Sous contrôleur, un sidecar ne lit **jamais** les segments des autres pods : son store ne
contient que ce que son pod a vu ; le workload entier est vu par le contrôleur.

### Contrôleur ↔ API Kubernetes

| Action | Objet |
|--------|-------|
| watch | `mimicworkloads` (état souhaité), `namespaces` (garde-fou `env=production`) |
| patch | `mimicworkloads/status`, toutes les 5 s si l'alignement a changé |
| create | `tokenreviews` (authentification des sidecars, cache 60 s) |
| get | `pods` (rattacher le jeton à son pod, son workload et ses labels) |
| create, get, patch | bail `leases/mimic-controller` dans `mimic-system` (élection) |
| list, patch | pods du contrôleur (label `mimic.platform.io/leader` et `pod-deletion-cost`) |

### Snapshots

1. Une `MimicWorkload` passe en `mock` (ou `mock-partial`) sans `spec.snapshot`.
2. La réplique active construit un **snapshot** : jeu figé des échanges sortants du workload
   (mémoire + exemples par endpoint), identifié par l'empreinte de son contenu (16 caractères
   hexadécimaux), écrit sur le support et publié dans `status.snapshot`.
3. Au battement suivant, chaque pod sélectionné reçoit `{mode: mock, snapshot: <id>}`,
   télécharge la **forme de mock** (échanges rejouables et durées par opération, mêmes octets
   pour tous les pods), la garde dans `store.dir`, construit son moteur dessus, puis bascule.
4. Le pod annonce le snapshot chargé : quand tous les pods sélectionnés annoncent mode,
   snapshot et réglages demandés, la condition `Aligned` passe à `True`.

Le snapshot reste le même tant que le workload ne quitte pas le mock ; `spec.snapshot`
permet de rejouer un snapshot précis d'un tir à l'autre. Voir
[Collecte et stockage](Collecte-et-stockage.md).

## Haute disponibilité du contrôleur en bref

Deux réplicas, une seule active, élue par le bail `mimic-system/mimic-controller`. L'active
porte le label `mimic.platform.io/leader: "true"`, que le Service sélectionne ; la passive est
`Ready` mais répond `503` (`Retry-After: 2`) à tout sauf `/healthz` et `/metrics`. Le contrôleur
n'est jamais dans le chemin des requêtes : s'il tombe, le trafic continue dans le dernier état
reçu. Détails : [Exploiter en production](Exploiter-en-production.md).
