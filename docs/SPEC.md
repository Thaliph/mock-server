# Mimic : sidecar de découverte d'API et de mock pour tirs de perf

> Statut : **v0.23** ; lecture par namespace et page développeur (§7.1, §9.2c, §9.5, phase P12) : les données enregistrées se lisent par une API agrégée de Kubernetes (`data.mimic.platform.io/v1alpha1`), autorisée par le RBAC du namespace du workload (`SubjectAccessReview`), sans `pods/portforward` dans `mimic-system` ; `mimicctl` la préfère au port-forward, vérifie avant `mode` que le workload existe et lit les requêtes sortantes (`requests --direction outbound`) ; le guide gagne une page pour les équipes applicatives (glossaire, prérequis, headers `x-mimic-*` et codes de réponse du sidecar). v0.22 : supervision livrée (§9.3, §9.6, phase P11) : règles d'alerte dans `deploy/monitoring/` (`PrometheusRule` et règles brutes identiques, testées par `promtool`), `mimic_build_info` sur le sidecar, le contrôleur et le webhook, `mimic_controller_pods` par workload, date d'expiration du certificat servi par le contrôleur et le webhook, occupation du disque du store du sidecar rapportée à la taille de son `emptyDir`, filtre de logs du sidecar réglable par le webhook et par pod (`mimic.platform.io/log`), alertes d'expiration de certificat, de disque du store et d'éviction. v0.21 : cycle de vie des snapshots (§5.6, phase P10) : métadonnées versionnées (date, échanges, épinglé) écrites une seule fois, purge au-delà de `snapshot.keep_last` côté contrôleur et de `store.max_cached_snapshots` côté sidecar, jamais des référencés ni des épinglés, `status.snapshotExchanges` et condition `SnapshotEmpty`, API d'écriture (`pin`, import, purge immédiate) et `mimicctl snapshot ls|pin|export|import|gc`. v0.20 : guide d'exploitation réaligné sur le code (phase P9) : la documentation décrit le comportement livré (métriques exhaustives, image des manifests, UID 1338 du contrôleur, mémoire demandée, `/metrics` du contrôleur collectable sur 8443, certificats relus à chaud, alertes et requêtes PromQL corrigées), `docs/cli.md` fusionné dans la page `mimicctl` du guide, et la cohérence documentation ↔ manifests est vérifiée par des tests (DOC-01 à DOC-07). v0.19 : requêtes sans réponse visibles (§5.3, phase P3j) : chaque requête que le mock ne sait pas servir (`599`, ou `413` pour un body trop gros) est journalisée en débit limité avec sa méthode, son authority et la forme de son chemin, comptée, gardée par le sidecar dans un relevé borné (`GET /api/unmatched`) et remontée au contrôleur par le battement, jusqu'à `mimicctl status` et `mimicctl report`. v0.18 : démarrage d'un pod piloté (§5.5.5, phase P6h) : sous contrôleur, l'application ne démarre qu'une fois l'état souhaité appliqué, par la `startupProbe` du native sidecar, avec un délai et un repli configurables (`share.startup`). v0.17 : mock étanche (§3.1, §3.4, phase P3i) : en `mock`, le TCP sortant non HTTP et les connexions restées muettes sont refusés par défaut (`mock.tcp: refuse`) au lieu d'être relayés vers la vraie dépendance, refus comptés et journalisés ; le trafic local du pod (`127.0.0.1`, `::1`) n'est plus intercepté, comme sous Istio. v0.16 : tir de charge intégré (§5.9, phase P8b) : `mimicctl load` rejoue les requêtes enregistrées contre un workload en mock, à concurrence fixe ou croissante jusqu'à saturation, et en déduit une taille de CPU et de mémoire par pod. v0.15 : cohabitation avec un service mesh (§3.3, phase P4b) : Istio en sidecar, Istio ambient et Linkerd ; Mimic se place entre l'application et le mesh, détecte le mesh dans les règles déjà posées et prend des ports et un UID hors de ceux des mesh (admin 15100, sortant 15101, entrant 15106, UID 1338). v0.14 : contrôleur en haute disponibilité (§5.5.4, phase P6g) : deux réplicas, une seule active, élue par un bail `Lease` ; la nouvelle active recharge l'état persisté avant de recevoir le trafic, que le Service ne route que vers elle, et les sidecars traversent la bascule sans intervention, le mock continuant d'être servi. v0.13 : WebSocket en mock réglable (§3.4, phase P3g) : `mock.websocket: relay` (défaut) relaie toujours la poignée de main sortante, `refuse` la refuse en `503` sans joindre la dépendance, ce qui rend O3 strict ; réglable par la configuration du sidecar, la `MimicWorkload` (`spec.mock.websocket`) et `mimicctl mode --mock-websocket`. v0.12 : snapshot de mock léger (§5.6) : un pod qui passe en mock ne télécharge plus le snapshot complet, mais sa forme de mock, réduite à ce que son moteur en garde (échanges rejouables et durées par opération), compressée et identique sur tous les pods, pour les mêmes réponses ; chemin chaud du sidecar (phase P3f) : métriques comptées sans allocation, curseurs de `round-robin` et `sequence` bornés, mock-partial sans reconstruction du moteur par échange relayé. v0.11 : mock centralisé (§5.4, §5.5.3) : sous contrôleur, les sidecars publient leurs segments sans plus importer ceux de leurs pairs, et le mock de chaque pod sert le snapshot poussé par le contrôleur ; le contrôleur garde au moins un exemple rejouable par endpoint de chaque workload, hors rétention ; l'historique par défaut passe de 7 jours à 24 heures. v0.10 : parcours SRE de tir de charge (§5.8) : garde-fou production côté contrôleur et `mimicctl`, capture entrante réglable pendant un tir (`capture.inbound`), bascule limitée aux pods sélectionnés (`spec.selector`) et métriques comparables par version. v0.9 : rapport de découverte `mimicctl report` (§9.5) et démo OpenTelemetry complète, avec son générateur de charge, en validation des PR (E2E-05). v0.8 : injection automatique par webhook d'admission et CLI `mimicctl` (§9.4, §9.5). v0.7 : le contrôleur devient la seule autorité de bascule, pilotée par la ressource `MimicWorkload`, avec authentification des sidecars (§5.5, §7.1). v0.6 : ajout du contrôleur (§5.5), des snapshots de mock identiques sur tous les pods (§5.6) et de la liste des requêtes rejouables (§5.7). Périmètre réduit à HTTP et HTTPS.
> Langage : Rust. Développement avec Claude Code sur dépôt git privé.
> Nom de travail : `mimic` (à renommer librement).

Les choix marqués **[DÉFAUT]** sont des hypothèses prises pour avancer ; ils sont repris dans la section 12 (questions ouvertes) quand ils méritent une validation.

---

## 1. Problème et objectifs

Les équipes SRE doivent lancer des tirs de performance sur des services dont elles ne connaissent ni l'API ni les dépendances, sur des environnements hors production où les dépendances externes (SaaS) ont des quotas très faibles.

Mimic est un sidecar qui :

1. **Observe** (mode `record`) tout le trafic HTTP d'un pod, entrant et sortant : méthode, chemin, query, headers, payloads, code retour, latence.
2. **Décrit** l'API du service observé (trafic entrant) en **OpenAPI 3.1**, avec des exemples réels et des statistiques de latence.
3. **Remplace** les dépendances (mode `mock`) : il répond lui-même au trafic sortant du pod avec les réponses enregistrées (même code, headers, payload, latence), sans que rien ne quitte le pod.

### Objectifs mesurables

| ID | Objectif |
|----|----------|
| O1 | Surcoût de latence en mode `record` : p99 < 1 ms par requête (hors capture de body > 64 KiB). |
| O2 | Empreinte : < 64 MiB RAM et < 100 m CPU au repos, < 250 m CPU à 1 000 req/s. |
| O3 | En mode `mock`, **zéro** connexion sortante vers une destination enregistrée (vérifiable par NetworkPolicy deny-all egress), hors connexions WebSocket, qui n'ont pas de réponse rejouable et sont relayées par défaut (§3.4). Avec `mock.websocket: refuse`, l'objectif est strict : aucune exception. Le TCP non HTTP est refusé par défaut (`mock.tcp: refuse`, §3.4) ; `mock.tcp: relay` le relaie, en connaissance de cause. Ce qui n'est pas intercepté (UDP dont le DNS, CIDR et ports exclus dont l'API Kubernetes, §9.4) ne passe pas par Mimic : seule une NetworkPolicy le borne. |
| O4 | Le document OpenAPI généré passe un validateur OpenAPI 3.1 sans erreur. |
| O5 | Fidélité du mock : code HTTP, headers (hors headers hop-by-hop) et body identiques à l'enregistrement ; latence rejouée à ±10 % de la distribution enregistrée. |
| O6 | Basculer de mode sans redémarrer le pod. |

### Hors périmètre v1

- Protocoles non HTTP (Postgres, Redis, Kafka, AMQP) : passés en **passthrough** TCP, ni enregistrés ni mockés.
- Mock du trafic **entrant** (on ne remplace pas l'application elle-même).
- Interface graphique.
- Production : Mimic est conçu pour les environnements hors production. Un garde-fou refuse les modes `mock` et `mock-partial` dans un namespace labellisé `env=production` (§5.5.1, §9.5) : le contrôleur ne les transmet jamais aux sidecars de ce namespace, et `mimicctl mode` les refuse.

---

## 2. Vue d'ensemble

```
                    Pod
 ┌──────────────────────────────────────────────────────────┐
 │                                                          │
 │  trafic entrant ──► [iptables REDIRECT :15106] ──┐       │
 │                                                  ▼       │
 │                                         ┌──────────────┐ │
 │   ┌──────────────┐   localhost:8080     │    mimic     │ │
 │   │ application  │ ◄────────────────────│  (sidecar)   │ │
 │   │              │                      │              │ │
 │   │              │ ── trafic sortant ──►│ :15101       │──┼──► dépendances (record)
 │   └──────────────┘  [iptables :15101]   │              │ │
 │                                         │  store local │ │    ✗ rien ne sort (mock)
 │                                         │  admin :15100│ │
 │                                         └──────┬───────┘ │
 └────────────────────────────────────────────────┼─────────┘
                                                  ▼
                                   export (S3 / PVC / API admin)
```

Composants livrés :

| Composant | Rôle | Crate |
|-----------|------|-------|
| `mimic-proxy` | Proxy transparent L7, capture, mock | binaire principal |
| `mimic-init` | Init container qui pose les règles iptables | binaire (ou mode du binaire principal) |
| `mimic-core` | Modèle de données, inférence OpenAPI, matching, redaction, snapshots, rejeu | lib, sans I/O, testée en isolation |
| `mimic-controller` | Plan de contrôle : agrège les enregistrements par workload, interface de découverte centrale, snapshots de mock, bascule de mode | binaire, un Deployment par cluster (deux réplicas, une active, §5.5.4) |
| `mimic-injector` | Webhook d'admission qui injecte sidecar + init (§9.4) | binaire |
| `mimicctl` | CLI : bascule de mode, export, diff de specs (§9.5) | binaire |

Principe structurant : **toute la logique métier vit dans `mimic-core`, en fonctions pures**. Le proxy n'est qu'un adaptateur réseau. C'est ce qui permet d'écrire les tests avant le code.

---

## 3. Interception du trafic

### 3.1 Choix : proxy transparent via iptables **[DÉFAUT]**

Même mécanique qu'Istio, éprouvée :

- `mimic-init` (capability `NET_ADMIN`, exécuté avant l'app) pose, dans la table `nat` et des chaînes dédiées `MIMIC_INBOUND` / `MIMIC_OUTBOUND` :
  - **entrant** : `PREROUTING` → `REDIRECT --to-ports 15106` pour les ports déclarés de l'app (ou tous les ports, `*`), sauf les ports du sidecar ;
  - **sortant** : `OUTPUT` → `REDIRECT --to-ports 15101` pour tout TCP, sauf : le trafic de l'UID du sidecar (`-m owner --uid-owner 1338 -j RETURN`, ce qui évite les boucles), le trafic local du pod (`-o lo -j RETURN` : boucle locale `127.0.0.0/8` et `::1`, IP du pod), les CIDR et ports exclus (ex. API Kubernetes, bases de données si on préfère ne pas les faire transiter).
- **Trafic local** : comme Istio, Mimic n'intercepte pas ce qui reste dans le pod. Un appel de l'application vers `127.0.0.1` vise un autre conteneur du pod (agent OpenTelemetry, Dapr, `cloud-sql-proxy`), pas une dépendance : l'intercepter le ferait enregistrer comme une dépendance `127.0.0.1:<port>` et, en `mock`, recevoir un `599` ou un refus à la place du conteneur voisin. L'isolement n'y perd rien : les connexions que ce conteneur ouvre à son tour vers l'extérieur ne viennent pas de l'UID 1338 et passent par Mimic comme celles de l'application. Un appel de l'application vers l'IP de son propre pod n'est pas intercepté non plus : c'est un appel à elle-même, et `PREROUTING` ne voit pas le trafic émis localement. Dans un pod du mesh, le trajet entrant (connexion du mesh vers l'application, `-o lo`) reste redirigé vers 15106 par ses règles propres, placées avant (§3.3.2). `MIMIC_INTERCEPT_LOOPBACK=true` (`--intercept-loopback`) rétablit la redirection du trafic vers `127.0.0.1` et `::1` (règle `-o lo ! -d 127.0.0.1/32 -j RETURN`, `::1/128` en IPv6), le comportement antérieur à v0.17 : elle sert aux tests en namespace réseau jetable, où les dépendances factices écoutent sur la boucle locale. L'injecteur ne la pose pas.
- Les règles sont générées par une fonction pure (testée) et appliquées en une fois par `iptables-restore --noflush` ; une deuxième exécution ne duplique rien.
- Chaque valeur reprise dans une règle est analysée avant toute commande : CIDR sortants exclus (adresse IPv4 ou IPv6, avec un préfixe d'au plus 32 ou 128 bits ; une adresse seule vaut `/32` ou `/128`), ports (entier de 1 à 65535) et IP du pod (`POD_IP`). Une valeur invalide fait échouer `mimic-init` avant toute commande `iptables`, avec un message qui nomme la variable et la valeur, et le code de sortie 2 (1 : échec d'`iptables`). Les CIDR sont écrits sous leur forme canonique `adresse/préfixe` : aucune valeur n'est recopiée telle quelle dans l'entrée de `iptables-restore`.
- **Détection du protocole** : le trafic redirigé n'est pas forcément HTTP. Pour chaque connexion redirigée, le proxy lit sans les consommer les premiers octets (`peek`) :
  - méthode HTTP/1 ou préface HTTP/2 : traitement HTTP (record, mock) ;
  - enregistrement TLS (`0x16`) : tunnel TCP opaque tant que l'interception TLS (§4.2) n'est pas activée ; **refusé en mode `mock`** pour garantir qu'aucun appel ne sort (O3) ;
  - autre protocole : tunnel TCP opaque vers la destination d'origine ; en sortie et en mode `mock`, **refusé** par défaut (`mock.tcp: refuse`, §3.4) ;
  - rien reçu au bout de 300 ms : connexion **muette**. C'est le cas d'un protocole où le serveur parle en premier (MySQL, SMTP), mais aussi d'une connexion HTTP ouverte d'avance par un pool de connexions (hyper, Go, Java…) et utilisée plus tard. Le proxy ne tranche qu'au premier octet : il ouvre la connexion vers la destination d'origine et attend le premier qui parle. Si c'est l'application et que ses octets sont du HTTP, la connexion vers la destination est refermée sans avoir servi et la connexion est traitée en HTTP (record, mock) ; sinon, tunnel opaque. En sortie, en `mock` et `mock-partial`, rien n'est ouvert vers la dépendance (O3) : le proxy attend jusqu'à 30 s que l'application parle, puis la traite comme ci-dessus ; une application muette au-delà est traitée comme un protocole inconnu : refusée en `mock` (sauf `mock.tcp: relay`), relayée en tunnel en `mock-partial`.
  Chaque tunnel est compté dans `mimic_passthrough_connections_total{result="relayed"}`, chaque refus dans `mimic_passthrough_connections_total{result="refused"}` (§9.3) : la connexion de l'application est refermée sans que rien ne soit ouvert vers la destination, avec un log `warn` qui nomme la destination d'origine (adresse et port), le type (`tls`, `tcp`, ou connexion muette) et l'option en cause, limité à une ligne par seconde avec le nombre de lignes omises (§9.3).
- Le proxy récupère la destination d'origine avec `getsockopt(SO_ORIGINAL_DST)` (IPv4) et `IP6T_SO_ORIGINAL_DST` (IPv6).
- **Relais entrant** : le trafic entrant est envoyé à sa destination d'origine (IP du pod et port), comme Istio depuis 1.10, pour qu'une application qui n'écoute que sur l'IP du pod reste joignable (Next.js en mode standalone écoute sur `$HOSTNAME`, résolu en IP du pod). Si rien n'y écoute pour ce port, le proxy se rabat sur `127.0.0.1` (application liée à la boucle locale). Le choix est mémorisé par port et réévalué après un échec de connexion ; `inbound.upstream` impose une adresse.
- Sur K8s ≥ 1.29, le sidecar est déclaré en **native sidecar** (`initContainers` + `restartPolicy: Always`) pour démarrer avant l'app et s'arrêter après elle.

Alternative sans privilège, configurable : **mode proxy explicite** (`HTTP_PROXY`/`HTTPS_PROXY` injectés dans l'app). Utile si `NET_ADMIN` est interdit, mais ne capture que les clients qui respectent ces variables.

### 3.2 Pourquoi pas eBPF

eBPF est excellent pour **observer** passivement, mais :

- il ne permet pas de **répondre** à la place de la dépendance : le mode `mock` exige de toute façon un proxy ;
- le déchiffrement TLS passe par des uprobes sur OpenSSL/BoringSSL/Go crypto/rustls, fragile selon le runtime de l'app ;
- il demande des privilèges noeud (`CAP_BPF`, `CAP_SYS_ADMIN`), plus durs à faire accepter qu'un `NET_ADMIN` limité au pod.

Piste v2 : un mode `observe-ebpf` en lecture seule pour les pods où l'on ne veut rien injecter dans le chemin réseau.

### 3.3 Service mesh

Mimic cohabite avec un service mesh déjà présent dans le pod (décision du 18/10, Q3) : Istio en mode sidecar, Istio en mode ambient et Linkerd, qui n'a pas de mode sans sidecar. Le principe est le même dans les trois cas : **Mimic se place entre l'application et le mesh**, pour voir le HTTP en clair, avant le mTLS du mesh.

```
sortant :  application ──► mimic ──► mesh (mTLS) ──► dépendance
entrant :  client ──► mesh (retire le mTLS) ──► mimic ──► application
```

En `mock`, une requête sortante s'arrête à Mimic : le mesh n'en voit rien et O3 tient comme hors mesh.

#### 3.3.1 Ports et UID

Istio prend pour son proxy l'UID 1337 et, dans le pod, les ports 15000, 15001, 15006, 15008, 15020, 15021, 15053 et 15090 ; le ztunnel d'Istio ambient écoute dans le réseau du pod sur 15001, 15006 et 15008 ; Linkerd prend l'UID 2102 et les ports 4140, 4143, 4190 et 4191. Pour cohabiter avec chacun sans dépendre de l'admission, Mimic utilise partout, mesh ou non : **UID et GID 1338**, API admin sur **15100**, écoute sortante sur **15101**, écoute entrante sur **15106**. Jusqu'à v0.14, Mimic reprenait les valeurs d'Istio (1337, 15000, 15001, 15006) : sous Istio, il aurait passé pour le proxy du mesh et l'aurait contourné, et ses ports auraient été déjà pris.

#### 3.3.2 Règles dans un pod du mesh

Hors mesh, les règles sont celles du §3.1. Dans un pod du mesh, `mimic-init` pose à la place :

- **aucune règle `PREROUTING`** : le trafic entrant arrive d'abord au mesh, qui retire le mTLS ;
- une chaîne `MIMIC_OUTBOUND`, sautée **en tête** de `OUTPUT` (`-I OUTPUT 1`), avant les chaînes du mesh, qui contient dans l'ordre :
  1. **le trajet entrant** : la connexion que le mesh ouvre vers l'application, en local (`-o lo`), sur les ports entrants interceptés (§3.1, ports du sidecar et ports exclus retirés), est redirigée vers 15106. En ambient, ztunnel ouvre cette connexion avec l'adresse source du client : une redirection vers `127.0.0.1` rendrait la réponse de Mimic impossible à router (source de boucle locale vers une adresse distante), si bien que la connexion est envoyée par `DNAT` vers l'IP du pod, port 15106. `mimic-init` lit cette IP dans `POD_IP` (API downward, `status.podIP`) ; sans elle, ou pour l'autre famille d'adresses, il se rabat sur la redirection ;
  2. le trafic de Mimic (UID 1338) : `RETURN`. Il continue vers les chaînes du mesh, qui l'envoient à son proxy comme le trafic de l'application ;
  3. le trafic propre du mesh : `RETURN`, il n'entre jamais dans Mimic ;
  4. les règles sortantes du §3.1 : trafic local (`-o lo`, boucle locale et IP du pod), CIDR et ports exclus, puis redirection vers 15101.

Ce qui distingue les mesh tient dans trois valeurs, lues dans les règles du mesh quand elles sont posées, sinon prises par défaut :

| Mesh | Chaînes reconnues (table `nat`) | Trajet entrant (mesh → application) | Trafic propre du mesh |
|------|-------------------------------|-------------------------------------|-----------------------|
| Istio sidecar | `ISTIO_OUTPUT`, sans `ISTIO_PRERT` | source `127.0.0.6/32` (`::6/128` en IPv6), adresse d'où Envoy rappelle l'application | UID et GID du proxy (`--uid-owner`, `--gid-owner` de `ISTIO_OUTPUT`, 1337) |
| Istio ambient | `ISTIO_PRERT` | marque de ztunnel (`--mark` de `ISTIO_OUTPUT`, `0x539/0xfff`) | la même marque |
| Linkerd | `PROXY_INIT_OUTPUT` | UID du proxy (`--uid-owner` de `PROXY_INIT_OUTPUT`, 2102) | le même UID |

Mimic relaie l'entrant et le sortant à la destination d'origine (§3.1) : chaque connexion qu'il ouvre repasse par `OUTPUT`, où les chaînes du mesh la traitent comme celle de l'application (redirection vers le proxy du mesh en sortie, vers l'application en entrée).

#### 3.3.3 Détection et ordre

- `mimic-init` lit les règles déjà posées (`iptables-save -t nat`), avec le binaire configuré puis `iptables-nft` et `iptables-legacy`. Il en tire le mesh, ses valeurs du tableau ci-dessus et le binaire que le mesh utilise : ses règles vont dans la même table que celles du mesh, sans quoi leur ordre ne serait pas garanti.
- `MIMIC_MESH` : `auto` (défaut, détection), `none` (règles du §3.1, sans détection), `istio`, `istio-ambient` ou `linkerd`. Une valeur explicite garde les valeurs lues si les règles trouvées sont celles de ce mesh, sinon prend ses valeurs par défaut.
- Les règles du mesh doivent exister quand `mimic-init` s'exécute. Le plugin CNI d'Istio (sidecar ou ambient) les pose à la création du pod, avant tout init container ; `istio-init` et `linkerd-init` sont des init containers, que l'injecteur fait passer avant `mimic-init` (§9.4).
- Deuxième application : le saut vers `MIMIC_OUTBOUND` reste unique et en tête de `OUTPUT`, même si le mesh a ajouté le sien entre-temps.
- Un pod déjà démarré garde ses règles : un namespace passé en ambient (ou mis dans un mesh) après le démarrage de ses pods demande leur redémarrage (`kubectl rollout restart`), comme pour l'injection elle-même.

#### 3.3.4 Limites et effets

- **mTLS strict** : avec une `PeerAuthentication` Istio en `STRICT`, ou une politique Linkerd `all-authenticated`, le mesh refuserait les sondes HTTP du kubelet vers l'API admin de Mimic. Quand l'injecteur voit le mesh à l'admission, les sondes de Mimic sont donc des sondes `exec` (`mimic-proxy --probe /healthz`), qui interrogent l'API admin depuis le conteneur. En ambient, le kubelet est reconnu par ztunnel (source `169.254.7.127`) : les sondes HTTP passent.
- **Connexions de Mimic vers le contrôleur** : elles sortent par le mesh comme toute connexion du pod. `mimic-system` étant hors mesh, le mesh les relaie telles quelles, chiffrées par Mimic (§7.1) ; sous une politique de sortie Istio `REGISTRY_ONLY`, le Service du contrôleur fait partie du registre.
- **Routage du mesh** : Mimic garde la destination d'origine et les en-têtes ; le routage L7 du mesh (`VirtualService`, `HTTPRoute`), ses retries et ses timeouts s'appliquent à la requête relayée comme à celle de l'application. Les waypoints d'Istio ambient sont hors du pod et ne changent rien pour Mimic.
- **Autres mesh** (Consul, Kuma, Cilium) : non pris en charge ; `MIMIC_MESH=none` garde les règles du §3.1, sans garantie d'ordre avec les leurs.

### 3.4 Protocoles

Seuls HTTP et HTTPS sont gérés (décision du 17/10, Q5). Le reste traverse Mimic sans être enregistré ni mocké.

| Protocole | v1 |
|-----------|----|
| HTTP/1.1 | record + mock |
| HTTP/2 (h2c et h2 via ALPN) | record + mock |
| HTTPS vers un hôte de `tls.intercept` | record + mock (§4.2) |
| HTTPS vers un autre hôte | tunnel opaque en record, refusé en mock |
| WebSocket (HTTP/1.1 `Upgrade: websocket`) | relayé dans tous les modes, sauf en sortie en mock avec `mock.websocket: refuse` ; poignée de main enregistrée, jamais les trames (voir ci-dessous) |
| SSE | réponse HTTP streamée, relayée sans être mise en mémoire (§5.2) |
| TCP non HTTP | tunnel opaque, détecté par sniffing des premiers octets ; en sortie en mock, refusé sauf `mock.tcp: relay` (voir ci-dessous) ; à exclure par port ou CIDR dans `mimic-init` si on veut le sortir du chemin |

**WebSocket.** Une requête HTTP/1.1 avec `Upgrade: websocket` et le jeton `upgrade` dans `Connection` est relayée avec ces deux headers, en entrée comme en sortie et dans les quatre modes (sauf refus en mock, ci-dessous). Si l'amont répond `101 Switching Protocols`, le proxy renvoie ce `101` (headers `Sec-WebSocket-*` compris) à l'appelant, puis copie les octets dans les deux sens, sans les interpréter, jusqu'à ce que les deux côtés aient fermé. Toute autre réponse de l'amont (refus `400`, `401`, `426`…) est relayée comme une réponse HTTP ordinaire.

- **Enregistrement** (record, mock, mock-partial ; jamais en passthrough) : un échange par connexion, écrit à la fermeture : méthode, chemin, query, code `101`, headers de requête et de réponse redactés (§7), bodies vides, `ttfb` = durée de la poignée de main, `total` = durée de la connexion. Les trames ne sont ni lues ni enregistrées.
- **Mock** : faute de trames enregistrées, aucune connexion WebSocket n'a de réponse rejouable. Une poignée de main **sortante** en `mock` ou en `mock-partial` suit l'option `mock.websocket`, quel que soit `mock.unmatched` (§5.3) :

  | `mock.websocket` | Poignée de main sortante en mock |
  |------------------|----------------------------------|
  | `relay` **[DÉFAUT]** | relayée vers l'amont et enregistrée, comme en record. C'est la seule exception à « jamais relayé » du mode `mock` (O3) |
  | `refuse` | refusée par le sidecar, sans joindre la dépendance : `503 Service Unavailable`, header `x-mimic-websocket: refused`, body texte qui nomme l'option. Rien n'est enregistré (aucun échange n'a eu lieu avec la dépendance). O3 devient strict pour ce workload |

  Le réglage vient de la configuration du sidecar (§9.1), et la `MimicWorkload` peut l'imposer (`spec.mock.websocket`, §5.5.1). Il ne touche ni les poignées de main entrantes (l'application reçoit toujours ses WebSockets : le mode `mock` ne touche jamais l'entrant), ni les modes `record` et `passthrough`, qui relaient toujours.

  **Pourquoi `503`** : pour le client, toute réponse autre que `101` fait échouer la poignée de main (RFC 6455 §4.1) ; le code dit seulement pourquoi. `503` décrit la situation simulée, une dépendance indisponible pendant le tir, et les clients WebSocket la traitent comme une panne passagère du serveur (reconnexion avec attente), le chemin de code qu'on veut voir sous charge. Un `4xx` (`400`, `401`, `403`, `426`) accuserait la requête ou les identifiants de l'application et déclencherait d'autres chemins (réauthentification, abandon définitif). `502` reste réservé à un amont que le sidecar a tenté de joindre sans y parvenir, et `599` (avec `x-mimic-unmatched`) à une requête ordinaire sans correspondance : une poignée de main n'est jamais une absence de correspondance, puisqu'aucune n'est rejouable par construction. Le header `x-mimic-websocket: refused` distingue ce refus d'un `503` de la vraie dépendance. Pas de `Retry-After` : le refus dure autant que le mock.

  Un échange `101` enregistré n'est jamais rejoué, ni pour une poignée de main, ni pour une requête ordinaire sur le même chemin.
- **Comptage** : chaque `101` relayé incrémente `mimic_websocket_connections_total{direction, result="relayed"}`, et chaque refus en mock `mimic_websocket_connections_total{direction="outbound", result="refused"}` (§9.3), en plus de `mimic_requests_total` avec `status="101"` ou `status="503"`. Un refus ne passe pas par le moteur de mock : ni `mimic_mock_responses_total`, ni latence injectée.
- WebSocket sur HTTP/2 (`CONNECT` étendu, RFC 8441) : hors périmètre v1.

**TCP non HTTP en mock.** Mimic ne sait rejouer que du HTTP : une base de données, un broker ou un cache joints en TCP ne sont pas simulés. En `mock`, une connexion sortante redirigée dont les premiers octets ne sont ni HTTP ni TLS, ou restée muette au-delà de l'attente de 30 s (§3.1), suit l'option `mock.tcp` :

| `mock.tcp` | Connexion TCP sortante non HTTP en `mock` |
|------------|-------------------------------------------|
| `refuse` **[DÉFAUT]** | refermée par le sidecar sans rien ouvrir vers la destination ; comptée dans `mimic_passthrough_connections_total{direction="outbound", kind="tcp", result="refused"}`, log `warn` limité en débit avec la destination. O3 tient pour le TCP |
| `relay` | relayée en tunnel opaque vers la destination d'origine, comme en `record` : la dépendance reçoit la charge du tir. Comportement antérieur à v0.17 |

- **Pourquoi refuser par défaut** : un tir « isolé » qui envoie sa charge à la vraie base ou au vrai broker ne l'est pas ; il fausse la mesure (latence de la vraie dépendance, partagée avec d'autres) et charge un système qui n'est pas le sujet du tir. Un refus échoue vite et se voit (métrique, log), comme une dépendance indisponible ; un relais silencieux ne se voit pas.
- **Garder une dépendance TCP réelle** pendant un tir : l'exclure de l'interception (`mimic.platform.io/exclude-outbound-ports` ou `exclude-outbound-cidrs`, §9.4). Son trafic ne passe alors plus du tout par Mimic, dans aucun mode, et l'exception est écrite dans la spec du pod. `mock.tcp: relay` relaie, lui, tout le TCP non HTTP du workload.
- **Portée** : seulement la sortie en `mock`. `record` et `passthrough` relaient toujours ; `mock-partial`, qui relaie ce qu'il ne sait pas servir, relaie aussi le TCP non HTTP, quel que soit `mock.tcp`. L'entrant n'est jamais concerné (le mode `mock` ne touche jamais l'entrant). Le réglage ne dépend pas de `mock.unmatched`. Le TLS non intercepté reste refusé en `mock` dans les deux cas (tableau ci-dessus), et une connexion muette qui finit par parler HTTP est servie par le mock (§3.1).
- **Réglage** : configuration du sidecar seulement (§9.1), lue au démarrage. Contrairement à `mock.websocket`, ni la `MimicWorkload` ni le battement ne le transmettent : le défaut est déjà l'isolement complet, et l'exception (« cette dépendance reste réelle ») est une propriété du workload, qui s'exprime mieux par une exclusion dans `mimic-init` que par un réglage de tir.

Les headers propres à la connexion (`Connection`, `Keep-Alive`, `Proxy-Authorization`, `Proxy-Connection`, `Transfer-Encoding`, `Upgrade` hors WebSocket, et ceux que `Connection` nomme, RFC 9110 §7.6.1) ne sont jamais relayés.

Le relais garde la version de la requête reçue : une requête HTTP/2 part en HTTP/2 vers l'amont (h2c en connaissance préalable, h2 via ALPN en HTTPS), trailers compris, car un serveur gRPC refuse HTTP/1.1 ; une requête HTTP/1.x part en HTTP/1.1.

### 3.5 Délais et plafonds du sidecar

Le sidecar est dans le chemin de tout le trafic du pod : une dépendance muette ou un client lent ne doivent pas lui faire garder indéfiniment des connexions, des tâches et de la mémoire.

- **Connexion vers l'amont** : toute connexion que le sidecar ouvre vers une dépendance ou vers l'application est bornée par `outbound.connect_timeout` (10 s par défaut) : client HTTP (résolution DNS, TCP et poignée de main TLS compris), tunnel `CONNECT`, tunnels des connexions redirigées (§3.1). Une dépendance dont le SYN reste sans réponse (NetworkPolicy qui jette les paquets, hôte disparu) libère ainsi l'application en 10 s, et non au délai TCP du système (plusieurs minutes). Au-delà du délai :
  - une requête HTTP reçoit `504 Gateway Timeout`, avec un body texte qui nomme l'option, et l'échec est compté dans `mimic_upstream_errors_total{kind="connect_timeout"}` (les autres échecs de connexion restent `502`, `kind="connect"`) ;
  - un tunnel `CONNECT` est refermé et compté de même (`kind="connect_timeout"`) ;
  - le tunnel d'une connexion redirigée est refermé et compté dans `mimic_passthrough_connections_total{result="error"}`, comme les autres échecs de tunnel.

  **Aucun délai ne borne une connexion établie** : ni la durée d'une réponse, ni celle d'un flux (SSE, gRPC en streaming, gros téléchargement), d'une connexion WebSocket ou d'un tunnel. Ils durent ce que l'application et la dépendance décident. Les sondes qui choisissent l'adresse de l'application (§3.1) gardent leur délai d'une seconde.
- **Lecture des en-têtes** : sur les trois écoutes (entrant, sortant, admin), une connexion HTTP/1 doit transmettre les en-têtes complets d'une requête en moins de `server.header_read_timeout` (60 s par défaut), sinon le sidecar la ferme sans réponse : un client lent (slowloris) ne garde pas une connexion ouverte. Le délai court dès que la connexion attend une requête, donc aussi entre deux requêtes d'une connexion persistante : une connexion keep-alive inactive plus longtemps est refermée par le sidecar, comme le fait un serveur (nginx ferme au bout de 75 s, un ALB au bout de 60 s), et les pools de connexions des clients HTTP le gèrent. Le délai ne touche ni la lecture d'un body de requête, ni l'écriture d'une réponse, ni une connexion basculée en WebSocket. La poignée de main TLS d'un hôte intercepté (§4.2) est bornée par le même délai. HTTP/2 n'est pas concerné.
- **Connexions simultanées** : `server.max_connections` (2 048 par défaut) borne ensemble les connexions ouvertes sur l'entrant et le sortant, tunnels et WebSocket compris. Au-delà, une nouvelle connexion est acceptée puis refermée aussitôt, sans être lue, et comptée dans `mimic_connections_refused_total{listener}`, avec au plus une ligne de log par seconde. L'admin a son propre plafond, fixe, de 64 connexions : le trafic de l'application ne peut pas priver le kubelet de ses sondes.
  - **Refus plutôt qu'attente** : un sidecar qui cesserait d'accepter laisserait les connexions dans la file du noyau, sans délai. Et comme l'entrant et le sortant partagent le plafond, une requête entrante qui attend un appel sortant jamais accepté bloquerait jusqu'aux délais des clients. Un refus échoue vite, comme une dépendance indisponible, et le reste du trafic continue.
  - **Dimensionnement** : une connexion coûte de l'ordre de quelques dizaines de KiB (tampons HTTP, état TLS), soit quelques dizaines de MiB au plafond, dans la limite de 256 Mi posée par l'injecteur (§9.4).

---

## 4. TLS

### 4.1 Trafic entrant

Cas le plus fréquent : TLS terminé en amont (ingress, mesh), l'app reçoit du HTTP clair. Mimic le voit directement. Si l'app termine elle-même son TLS, Mimic ne voit que du chiffré : passthrough et métadonnées L4 (option v2 : monter le certificat de l'app dans Mimic pour qu'il termine le TLS à sa place).

### 4.2 Trafic sortant vers les SaaS : interception MITM **[DÉFAUT]**

C'est le cas central du mode `mock` (appels HTTPS vers des SaaS).

- Une **CA éphémère par pod** est générée au démarrage (`rcgen`), jamais persistée ; option : CA par namespace fournie via Secret (cert-manager).
- Le certificat de la CA est publié dans un volume partagé `emptyDir` et l'app doit lui faire confiance. Moyens d'injection, par ordre de préférence :
  1. variables d'environnement standard (`SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE`) ;
  2. bundle complet (CA système + CA Mimic) monté sur `/etc/ssl/certs/ca-certificates.crt` ;
  3. pour la JVM : truststore généré + `JAVA_TOOL_OPTIONS=-Djavax.net.ssl.trustStore=...`.
- Mimic lit le SNI, forge à la volée un certificat pour ce nom (cache LRU), termine le TLS côté app et ouvre un vrai TLS vers la destination (`rustls` + racines système) en mode `record`.
- **MITM opt-in par hôte** : `tls.intercept: ["api.stripe.com", "*.salesforce.com"]`. Tout hôte non listé passe en tunnel TLS opaque (on n'enregistre que SNI, durée, octets).
- En proxy explicite, un `CONNECT` vers un hôte intercepté est terminé par Mimic ; en transparent, le SNI est lu dans le ClientHello sans consommer les octets.
- En mode `mock`, un hôte intercepté est servi depuis les enregistrements ; un hôte non intercepté est refusé (O3).
- Le sidecar publie `ca.crt` (CA Mimic seule) et `ca-bundle.crt` (racines système + CA Mimic) dans `tls.ca_dir`.
- Limite connue : le **certificate pinning** côté app rend le MITM impossible. Ces hôtes sont détectés (échec de handshake côté app) et signalés dans les métriques.

---

## 5. Modes de fonctionnement

| Mode | Entrant | Sortant | Usage |
|------|---------|---------|-------|
| `passthrough` | relayé, métriques seulement | relayé, métriques seulement | désactivation à chaud |
| `record` | relayé + enregistré | relayé + enregistré | découverte |
| `mock` | relayé + enregistré | **servi depuis le store**, jamais relayé (sauf WebSocket avec `mock.websocket: relay`, le défaut, et TCP non HTTP avec `mock.tcp: relay`, §3.4) ; TLS non intercepté et TCP non HTTP refusés | tir de perf isolé |
| `mock-partial` | relayé + enregistré | servi si correspondance, sinon relayé + enregistré ; TLS non intercepté et TCP non HTTP relayés | enrichir progressivement le store |

Le mode `mock` ne touche jamais le trafic entrant : le tir de perf frappe la vraie application, ce sont ses dépendances qui sont simulées.

### 5.1 Bascule de mode

Aucune bascule ne redémarre de pod, et **toute bascule passe par le contrôleur** (§5.5) :

- l'état souhaité d'un workload est déclaré par une ressource Kubernetes `MimicWorkload` (CRD, §5.5.1) : `kubectl patch mimicworkload checkout -n shop --type merge -p '{"spec":{"mode":"mock"}}'`, ou un manifest versionné en GitOps ;
- qui a le droit de basculer est donc décidé par le RBAC Kubernetes sur cette ressource (verbes `create`, `update`, `patch` sur `mimicworkloads.mimic.platform.io`), et chaque changement est tracé dans l'audit de l'API server ;
- le contrôleur propage l'état souhaité aux sidecars par leur battement (§5.5).

Un sidecar rattaché à un contrôleur (`share.url` vers le contrôleur) n'accepte aucun autre ordre : `PUT /mode` et `DELETE /store` de son API admin répondent `403`, et l'annotation `mimic.platform.io/mode` est ignorée. Il démarre dans le dernier état souhaité reçu (gardé dans `store.dir`), sinon dans le mode de sa configuration, puis s'aligne au premier battement ; l'application de son pod ne démarre qu'une fois l'état souhaité connu, ou après un délai de repli (§5.5.5).

Sans contrôleur (partage par bucket, ou sidecar isolé en développement), l'annotation de pod et `PUT /mode` restent disponibles. Il faut alors annoter les **pods**, pas le template du Deployment : modifier le template déclenche un rollout.

La bascule est atomique pour les nouvelles requêtes ; les requêtes en vol finissent dans l'ancien mode.

### 5.2 Mode `record`

Pour chaque échange, Mimic produit un `Exchange` :

```rust
struct Exchange {
    id: Ulid,
    direction: Direction,          // Inbound | Outbound
    started_at: DateTime<Utc>,
    // destination
    scheme: Scheme, authority: String, original_dst: SocketAddr,
    // requête
    method: Method, path: String, query: Vec<(String, String)>,
    req_headers: Vec<(HeaderName, HeaderValue)>,
    req_body: Body,                // Captured(bytes) | Truncated{size, prefix} | Skipped{size}
    // réponse
    status: u16,
    resp_headers: Vec<(HeaderName, HeaderValue)>,
    resp_body: Body,
    // timing
    ttfb: Duration,                // premier octet de réponse
    total: Duration,               // dernier octet
    // trace
    trace_id: Option<String>,      // traceparent si présent
    redactions: Vec<RedactionMark>,
}
```

Règles de capture :

- métadonnées : 100 % des échanges ;
- bodies : au plus `N` exemples par couple (opération, code retour), `N = 20` par défaut (`capture.samples_per_operation`), avec remplacement par échantillonnage réservoir pour garder de la diversité :
  - l'opération est (sens, authority, méthode, **forme** du chemin) : un segment identifiant (entier, UUID, ULID, hex, base64url, date) y est remplacé par sa nature, comme pour la correspondance du mock (§5.3). `/users/1` et `/users/2` comptent pour la même opération ;
  - le réservoir est déterministe : chaque échange reçoit une priorité, hachage stable de son identifiant, et le réservoir garde les `N` échanges de plus petite priorité. Chaque échange du flux a la même chance d'être gardé, qu'il arrive pendant le warm-up ou à la fin du tir, et le résultat ne dépend pas de l'ordre d'arrivée (fusion entre sidecars comprise, §5.4). Un exemple remplacé perd ses bodies et garde ses métadonnées ;
  - la décision est prise **avant** tout décodage, parsing JSON et redaction du body : un body qui ne serait pas gardé n'est pas traité (compté `reason="sample_limit"`) ; les headers et la query sont redactés dans tous les cas ;
  - les compteurs d'un couple disparaissent avec son dernier exemple : leur nombre est borné par celui des bodies gardés ;
- mémoire : en plus de `capture.max_exchanges` (50 000), le store a un budget en octets estimés, `capture.max_store_bytes` (64 MiB par défaut). Les bodies gardés en occupent au plus la moitié : au-delà, les bodies des échanges les plus anciens sont retirés en premier. Au-delà du budget, les échanges les plus anciens sont évincés. Un nouvel échange garde donc ses bodies, même quand les métadonnées remplissent le budget. Chaque retrait est compté (`reason="store_budget"`). Les défauts tiennent dans la limite mémoire de 256 Mi posée par l'injecteur (§9.4) : store et file de partage (§5.4) en occupent au plus un tiers, le reste couvre l'index du moteur de mock (§5.3), la découverte, les bodies en vol (ci-dessous) et le téléchargement d'un snapshot (§5.6) ;
- taille max capturée : 1 MiB par body ; au-delà, préfixe + taille ;
- bodies en vol : avant la décision du réservoir, le tee copie jusqu'à `max_body_bytes` de chaque body de requête et de réponse en cours. Ces copies comptent dans un budget global, `capture.max_inflight_bytes` (64 MiB par défaut) : sans lui, 200 requêtes concurrentes de 1 MiB feraient 400 MiB. Un fragment qui ne tient plus dans le budget n'est pas copié, ni aucun des suivants de ce body : s'il est gardé, le body l'est tronqué (`Truncated`, avec le préfixe déjà copié), et il est compté `reason="inflight_budget"` (et non `truncated`). Le budget est rendu dès que l'échange est enregistré ou abandonné. Le relais n'attend jamais le budget : seule la copie est sautée, le trafic de l'application n'est ni bloqué ni ralenti ;
- dimensionnement de la limite mémoire du sidecar : au moins `max_store_bytes` + `share.max_backlog_bytes` + `max_inflight_bytes`, plus, au téléchargement d'un snapshot, environ deux fois `share.max_snapshot_bytes` (document décompressé et forme chargée) et le moteur de mock construit dessus. Les défauts (64 + 16 + 64 MiB) laissent un peu plus de 100 MiB sous 256 Mi : de quoi charger une forme de mock de quelques dizaines de MiB. Au-delà, relever la limite (`resources` de l'injecteur) ou abaisser les budgets ;
- content-encoding (`gzip`, `br`, `zstd`) décodé pour l'inférence, encodage d'origine conservé pour le rejeu ;
- streaming : le proxy ne bufferise jamais la réponse complète avant de la relayer ; la capture se fait en tee ;
- **capture de l'entrant** (`capture.inbound`, que la `MimicWorkload` peut imposer, §5.5.1 et §5.8) : pendant un tir de charge, on mesure le pod, pas le sidecar. Le sortant n'est jamais concerné, et en `passthrough` rien n'est enregistré quel que soit le réglage.

  | Valeur | Échange entrant gardé |
  |--------|-----------------------|
  | `full` **[DÉFAUT]** | tel que décrit ci-dessus |
  | `metadata` | sens, authority, méthode, chemin, code, `ttfb`, `total` et taille des bodies (`Skipped { size }`, ou `Empty`) ; ni query, ni headers, ni body. Le tee ne copie aucun octet, il les compte ; rien n'est décodé ni redacté. L'échange reste dans le store **en mémoire** du sidecar (inspection, statistiques de latence de la page d'admin), sous ses plafonds, sans entrer dans le réservoir : il n'est ni écrit dans le journal local (§8), ni publié (§5.4). La découverte du workload et les requêtes rejouables (§5.7) restent donc celles du trafic réel |
  | `off` | aucun : seules les métriques sont tenues (`mimic_requests_total`, `mimic_request_duration_seconds`, mesurée jusqu'au dernier octet de la réponse, comme en `full`) |

### 5.3 Mode `mock`

Pour chaque requête sortante, le moteur de correspondance (`mimic-core::mock`) cherche la meilleure réponse enregistrée.

**Clé de correspondance**, par ordre de priorité :

1. `authority` (hôte:port) — obligatoire ;
2. `method` — obligatoire ;
3. chemin, comparé d'abord exactement, puis au **template** inféré (`/v1/customers/{id}`) ;
4. query : paramètres présents et valeurs ;
5. headers discriminants configurés (ex. `Accept`, `X-Api-Version`) ;
6. body : égalité canonique JSON, puis similarité (champs communs).

Le chemin est éliminatoire : un candidat doit avoir la même authority, la même méthode et un chemin égal ou de même template. Parmi les candidats restants, le score (entre 0 et 1) est la moyenne pondérée de :

| Critère | Poids | Mesure |
|---------|-------|--------|
| chemin | 0,4 | 1 si égal, 0,5 si seul le template correspond |
| query | 0,3 | indice de Jaccard sur les couples (nom, valeur) |
| headers discriminants | 0,1 | part des headers configurés de même valeur |
| body | 0,2 | 1 si JSON canonique égal, sinon Jaccard sur les couples (clé de premier niveau, valeur) |

Le type de correspondance vaut `exact` si le score est 1, `template` si le chemin n'est égal qu'au template, `fuzzy` sinon. Il est exposé dans le header `x-mimic-match: exact;score=1.00` (désactivable).

**Stratégies de sélection** quand plusieurs exemples correspondent :

| Stratégie | Comportement |
|-----------|--------------|
| `best` **[DÉFAUT]** | meilleur score, puis la réponse la plus fréquente, puis la plus récente (§5.6) |
| `round-robin` | rotation sur les exemples ; un curseur par chemin enregistré, et un seul par opération pour les chemins jamais enregistrés |
| `sequence` | rejoue dans l'ordre d'enregistrement (scénarios à état : création puis lecture) |
| `weighted-status` | respecte la distribution des codes observés (ex. 2 % de 429) pour reproduire les erreurs réelles |

**Latence rejouée** : la latence n'est pas copiée d'un exemple unique, elle est tirée dans la distribution empirique des durées enregistrées pour l'opération (même authority, méthode et template), avec un multiplicateur configurable (`mock.latency_factor: 1.0`, `0` pour la désactiver, `2.0` pour simuler une dépendance dégradée). La durée tirée est comptée depuis l'arrivée de la requête (fin de ses headers) : la réponse part à arrivée + latence, et la lecture du body de requête, le choix de l'enregistrement et la préparation de la réponse sont pris sur cette attente au lieu de s'y ajouter, comme pour la durée mesurée en record. L'attente est faite par un minuteur à la microseconde : le temporisateur de tokio arrondit à la milliseconde supérieure, ce qui rejouerait une dépendance de 140 µs à plus d'une milliseconde. Le minuteur est armé en avance de son retard moyen de réveil, mesuré en continu (plafonné à 1 ms), pour que les réponses partent en moyenne à l'heure. Une latence nulle (ou déjà écoulée) n'attend pas. En v1, la durée totale est appliquée avant l'envoi de la réponse ; le rejeu séparé du TTFB et du temps de transfert viendra avec le streaming des réponses mockées.

Le tirage aléatoire (latence, `weighted-status`) utilise un générateur à graine (`mock.seed`) : un même tir rejoué en série avec la même graine produit la même séquence. Sous concurrence, le générateur avance d'un pas atomique par tirage : la même graine donne le même ensemble de tirages, distribué aux requêtes dans leur ordre d'arrivée.

**Tenue en charge du mock** : le coût d'une requête mockée ne dépend ni de la taille du store ni du nombre de requêtes concurrentes.

- **Index** : le moteur range les enregistrements par (authority, méthode, forme du chemin), la forme étant celle du §5.2 (segment identifiant remplacé par sa nature) : deux chemins sont « égaux ou de même template » si et seulement s'ils ont la même forme. Une requête ne compare que les enregistrements **rejouables** de son opération ; les autres ne servent qu'à la distribution de latence, extraite une fois à la construction. Le moteur partage les échanges du store sans les copier.
- **Vue immuable, sans verrou** : le moteur est construit hors du chemin des requêtes, puis publié d'un bloc ; une requête lit le moteur courant sans prendre de verrou et ne le construit jamais quand il sait lui répondre. Les curseurs de `round-robin` et `sequence` sont des compteurs atomiques fixés à la construction : un par chemin enregistré, et un par opération pour les requêtes dont le chemin n'a jamais été enregistré (identifiants uniques). Leur nombre ne dépend donc pas du nombre de chemins demandés.
- **Construction à la bascule** : passer en `mock` ou `mock-partial`, ou recevoir un snapshot en mock, construit le moteur avant que le nouveau mode ou le nouveau snapshot ne serve une requête. Repasser en `record` ou `passthrough` libère le moteur.
- **Données locales qui changent** : sans snapshot, le store évolue encore pendant le mock (imports des pairs en partage direct, §5.4 ; requêtes relayées puis enregistrées en `mock-partial`). Le moteur courant continue de servir, et un moteur à jour est reconstruit en tâche de fond, au plus une fois par seconde. Une requête sans correspondance dans un moteur construit avant son arrivée ne déclenche pas de construction : tant que le mock sert sans snapshot, le store tient un index, par opération, des échanges sortants ajoutés depuis la construction du moteur en service. La requête est comparée aux enregistrements de son opération arrivés depuis (un petit moteur construit sur eux seuls), puis conclut, et la reconstruction complète reste celle de la tâche de fond. En `mock-partial`, une requête relayée puis enregistrée est donc servie par le mock dès la suivante, sans que chaque échange relayé reconstruise le moteur. L'index est vidé à chaque publication d'un moteur ; au-delà de 4 096 ajouts sans publication, il est abandonné et une requête sans correspondance attend, comme repli, un moteur qui connaît tous les enregistrements présents à son arrivée (les requêtes qui attendent ensemble partagent la même construction).
- **Réponse préparée** : headers convertis et body réencodé dans son `Content-Encoding` d'origine (`gzip`, `deflate`, `br`, `zstd`) une seule fois par enregistrement, au premier rejeu, puis servis tels quels.
- **Constructions comptées** : `mimic_mock_engine_builds_total{trigger}`, avec `switch` (bascule de mode ou de snapshot), `refresh` (reconstruction en tâche de fond) et `unmatched` (requête sans correspondance sur un moteur périmé, quand l'index des ajouts a été abandonné).

**WebSocket** : une poignée de main WebSocket n'est jamais servie depuis le store, puisque les trames ne sont pas enregistrées (§3.4). En `mock` comme en `mock-partial`, quel que soit `mock.unmatched`, elle ne passe pas par le moteur (ni `mimic_mock_responses_total`, ni latence injectée) : avec `mock.websocket: relay` (défaut), elle est relayée vers l'amont et enregistrée ; avec `refuse`, le sidecar répond `503` avec `x-mimic-websocket: refused`, sans joindre la dépendance ni rien enregistrer. Le moteur ignore les échanges de code `1xx` : ils ne sont ni rejoués ni comptés dans la distribution de latence, dont la durée d'une connexion WebSocket fausserait le tirage.

**Requête sans correspondance**, comportement configurable :

| Option | Réponse |
|--------|---------|
| `error` **[DÉFAUT]** | `599` + header `x-mimic-unmatched: true`, compté en métrique et relevé (ci-dessous) |
| `generate` | réponse synthétique générée depuis le schéma OpenAPI de la dépendance (v2) |
| `passthrough` | relayée (équivaut à `mock-partial`) |

**Body trop gros** : en `mock` comme en `mock-partial`, le body d'une requête sortante est lu en entier avant la recherche, dans la limite de `capture.max_body_bytes` ; au-delà, le sidecar répond `413` sans chercher de correspondance ni relayer. Ce refus est compté dans `mimic_requests_total{status="413"}` et `mimic_mock_responses_total{match="too_large"}`, et relevé comme une requête sans correspondance.

**Requêtes sans réponse visibles** : un développeur qui reçoit un `599` ou un `413` du mock doit pouvoir savoir quelle requête n'avait pas de réponse enregistrée, sans accès aux logs de l'application. Chaque requête **refusée** par le mock (`599` avec `unmatched: error`, ou `413`) est :

- **journalisée** au niveau `warn` (`mock : requête sans réponse enregistrée`, ou `mock : body de requête trop gros`), avec `method`, `authority`, `path` (forme du chemin, ci-dessous), `trace_id` si la requête porte un `traceparent`, et `reason` (`unmatched` ou `too_large`). Au plus une ligne par seconde et par raison, avec `omitted`, le nombre de lignes omises depuis la précédente (§9.3) ;
- **relevée** dans un relevé en mémoire du sidecar, agrégé par clé (authority, méthode, forme du chemin, raison) : nombre d'occurrences, premier et dernier horodatage (ms Unix). Le relevé garde au plus **100** clés ; une clé nouvelle au-delà évince celle vue le moins récemment (à égalité, la plus petite clé). Le total des requêtes refusées reste exact, évictions comprises. Le relevé est servi par `GET /api/unmatched` (§9.2) et remonté au contrôleur par le battement (§5.5) ;
- **remis à zéro** quand le sidecar entre en `mock` ou `mock-partial` depuis un autre mode, ou charge un autre snapshot : il décrit le tir en cours, puis reste lisible après le retour en `record`, jusqu'au mock suivant.

Une requête relayée faute de correspondance (`mock-partial`, `unmatched: passthrough`) n'est ni relevée ni journalisée : elle a reçu la réponse de la dépendance. Les `CONNECT` refusés en `mock` et les connexions non HTTP refusées ont leurs propres compteurs et logs (§3.1).

**Forme du chemin affichée** (`mimic-core::unmatched::path_template`, fonction pure) : le chemin sans sa query, segment par segment, comme la clé du moteur (§5.3, index) mais lisible et sans valeur. Un identifiant devient `{int}`, `{uuid}` ou `{id}` (ULID, hexadécimal, base64url, date) ; un segment qui serait redacté dans un body (e-mail, numéro de carte, JWT, IBAN, §7), une fois décodé de ses `%XX`, ou un marqueur de redaction, devient `{redacted}` ; un autre segment de plus de 32 caractères, ou d'au moins 16 caractères mêlant majuscules et minuscules (jeton), devient `{opaque}`. La forme est tronquée à 256 caractères (`/…` final), l'authority à 255, la méthode à 16. Ni la query, ni les headers, ni le body n'y figurent : le relevé, le battement et les logs ne portent aucune valeur sensible (RED-04).

**Réécriture dynamique** (v2) : certains champs ne doivent pas être rejoués tels quels (dates, IDs repris de la requête). Des règles optionnelles permettent de les recalculer :

```yaml
rewrite:
  - match: { authority: api.stripe.com, path: /v1/customers/{id} }
    response:
      body:
        "$.id": "{{request.path.id}}"
        "$.created": "{{now.unix}}"
```

### 5.4 Partage entre les sidecars d'un même workload

Un Deployment a souvent plusieurs réplicas, et chaque pod ne voit qu'une partie du trafic. Les sidecars d'un même workload mettent donc leurs enregistrements en commun : la découverte couvre tout le trafic du workload, et n'importe quel pod peut répondre en mode mock avec un enregistrement fait par un autre. Deux façons de le faire, selon `share.url` :

- **Avec un contrôleur** (recommandé, §5.5) : chaque sidecar publie ses segments au contrôleur, seul à les agréger. Il ne liste ni n'importe ceux de ses pairs : son store ne contient que ce que son pod a vu. La découverte du workload entier est celle du contrôleur, et en mock chaque pod sert le snapshot que le contrôleur lui pousse (§5.6), le même sur tous les pods.
- **En partage direct**, sur un stockage objet sans contrôleur (`s3://`, `file://`) : chaque sidecar importe les segments de ses pairs, et sa découverte comme son mock couvrent le workload.

Raison de la différence : importer les segments des pairs faisait télécharger à chaque pod, dès son arrivée, tout l'historique du workload encore en rétention, puis chaque nouveau segment de chaque pair, pour un résultat que le snapshot du contrôleur rend déjà identique sur tous les pods. Sous contrôleur, le trafic d'un sidecar vers le support se réduit à ses publications, ses battements et le téléchargement de la forme de mock d'un snapshot (§5.6).

- **Identité du workload** : `namespace` + nom du workload, lus via l'API downward. Le nom vient de l'annotation `mimic.platform.io/workload`, sinon du label `app.kubernetes.io/name`, sinon du label `app`. Chaque sidecar a un identifiant d'instance (nom du pod).
- **Support partagé** : un stockage objet (S3-compatible en production, répertoire local ou PVC `ReadWriteMany` en test). Aucun appel à l'API Kubernetes, aucune connexion entre pods.
- **Disposition** : `<prefix>/<namespace>/<workload>/exchanges/<instance>/<segment>.jsonl`, où `<segment>` est un ULID choisi par le sidecar. Un segment ne contient que des échanges déjà redactés. Avec un contrôleur, les segments d'une heure révolue sont fusionnés en un segment horaire `exchanges/_hour/<AAAAMMJJHH>.jsonl` (§5.5.2) : les noms d'instance qui commencent par `_` sont réservés et ne désignent jamais un pair. Le contrôleur range aussi, hors de `exchanges/`, les exemples par endpoint du workload (`<namespace>/<workload>/exemplars.jsonl`, §5.5.3).
- **Synchronisation** toutes les `share.interval` (10 s par défaut) : le sidecar publie ses nouveaux échanges. En partage direct seulement, il liste aussi les segments des autres instances et importe ceux qu'il n'a pas encore lus.
- **Publication** : les échanges de la file de publication sont envoyés du plus ancien au plus récent, en segments d'au plus 8 MiB, chacun avec son identifiant. Un segment dont l'envoi échoue reste en tête de la file et sera renvoyé tel quel, avec le même identifiant : une reprise après un délai dépassé ne crée pas de doublon (§5.5.2). Seul un segment entamé par la borne de la file (ses plus anciens échanges jetés) repart sous un nouvel identifiant. Après un échec, la synchronisation suivante attend `share.interval` × 2ⁿ (n échecs consécutifs), plafonné à 5 minutes, avec une gigue de 50 % : les sidecars ne reviennent pas tous en même temps quand le support revient.
- **Liste incrémentale** (partage direct) : après une première liste complète, le sidecar ne considère que les segments datés d'au moins la date du plus récent déjà listé moins 60 s, une marge qui couvre les écritures concurrentes ; les plus anciens ne sont ni relus ni gardés dans sa liste des segments importés (filtre local, le stockage objet listant tout). Ce curseur n'avance qu'après l'import réussi de tous les segments listés. Le contrôleur accepte encore `?since_ms=` sur sa liste (§9.2b) pour les sidecars d'une version antérieure, qui importaient aussi sous contrôleur.
- **Fusion** (partage direct) : dédoublonnage par identifiant d'échange (ULID préfixé par l'instance, donc unique dans le workload). Les plafonds de capture (§5.2) s'appliquent après fusion, ce qui borne la mémoire quel que soit le nombre de réplicas.
- **Panne du support** : la synchronisation échoue sans bloquer le trafic ; le sidecar continue avec ses données locales et l'échec est compté en métrique.
- **File de publication** : les échanges locaux pas encore publiés attendent dans une file unique, bornée en octets (`share.max_backlog_bytes`, 16 MiB par défaut) et en nombre (`share.max_pending`, 50 000 échanges par défaut). Quand le support reste indisponible, les plus anciens sont jetés au profit des plus récents dès que l'une des deux bornes est dépassée, et chaque échange jeté est compté (`mimic_capture_dropped_total{reason="share_backlog"}`) ; une publication échouée laisse ses échanges en tête de file, sous ces mêmes bornes. Sans partage configuré, aucune file n'est tenue.
- **Rétention** : en partage direct, les segments plus vieux que `share.retention` (24 heures par défaut) ne sont plus importés, et leur suppression est laissée à une règle de cycle de vie du bucket. Rien n'y garantit d'exemple par endpoint : un endpoint que personne n'a appelé depuis `share.retention` n'est pas importé par un pod qui arrive. Avec un contrôleur, le sidecar n'importe rien : c'est la `retention` du contrôleur qui supprime les segments (§5.5.2), et ses exemples par endpoint (§5.5.3) gardent chaque endpoint mockable au-delà.
- **Deux supports possibles** : `share.url` vaut soit l'URL du contrôleur (`https://mimic-controller.mimic-system:8443`, recommandé), soit un stockage objet partagé (`s3://…`, `file://…`) sans contrôleur. Sans contrôleur, il n'y a ni interface centrale, ni snapshot, ni bascule de workload, ni exemple par endpoint.

### 5.5 Contrôleur

`mimic-controller` est le plan de contrôle, à la manière de Kiali pour Istio : un Deployment par cluster (namespace `mimic-system`), en deux réplicas dont une seule est active (§5.5.4), qui ne se trouve jamais dans le chemin des requêtes. Si le contrôleur tombe, le trafic continue dans le dernier état reçu ; seuls le partage et la bascule attendent son retour.

- **Seule autorité** : le contrôleur est le seul à modifier le mode et le snapshot des sidecars. Il lit l'état souhaité dans les ressources `MimicWorkload` (§5.5.1), qu'il surveille par l'API Kubernetes.
- **Ingestion** : chaque sidecar lui envoie ses segments d'échanges, déjà redactés, sans lire ceux des autres instances (§5.4). Le contrôleur les range sur son support (`file://` sur un PVC, ou `s3://`) avec la même disposition qu'au §5.4. L'ingestion est idempotente (§5.5.2).
- **Battement** : chaque sidecar envoie toutes les `share.mode_poll` (2 s par défaut) son instance, son mode, son snapshot chargé, sa capture de l'entrant (`capture_inbound`, §5.2) et son traitement des WebSockets en mock (`mock_websocket`, §3.4) et le relevé de ses requêtes sans réponse (`unmatched`, §5.3 : le total et les 20 clés vues le plus récemment). La réponse porte l'état souhaité **pour ce pod** : `{mode, snapshot, capture_inbound, mock_websocket}`, chacun pouvant être absent. Sans `MimicWorkload`, l'état souhaité est vide et chaque sidecar reste dans le mode, la capture et le réglage WebSocket de sa configuration ; il y revient quand l'état souhaité redevient vide. Un pod que `spec.selector` ne sélectionne pas reçoit lui aussi un état vide (§5.5.1), et un pod d'un namespace de production ne reçoit jamais `mock` ni `mock-partial` (garde-fou, §5.5.1). Le premier battement auquel le contrôleur répond ouvre la barrière de démarrage du pod (§5.5.5).
- **Authentification** : chaque appel d'un sidecar est authentifié par un jeton de ServiceAccount, et le contrôleur ne lui permet d'agir que pour son propre pod et son propre workload (§7.1).
- **Interface de découverte centrale**, en lecture seule : la page du contrôleur liste les workloads, puis pour chacun les pods (mode, snapshot, dernier battement, aligné ou non, requêtes sans réponse), les requêtes sans réponse du mock agrégées sur le workload (§5.3), les opérations du service et de chaque dépendance avec leurs paramètres requis (même rendu que la page d'admin du sidecar, §9.2), les snapshots, les requêtes enregistrées (§5.7) et la commande `kubectl` qui bascule le workload. Elle est servie sur un port distinct lié à `127.0.0.1`, donc joignable seulement par `kubectl port-forward`, lui-même soumis au RBAC (`pods/portforward` dans `mimic-system`).
- **Persistance** : segments, snapshots, snapshot courant (`<namespace>/<workload>/desired.json`) et exemples par endpoint (`<namespace>/<workload>/exemplars.jsonl`, §5.5.3) de chaque workload sont sur le support ; un redémarrage du contrôleur ne perd rien. L'état des pods est reconstruit par les battements.
- **Tenue en charge** : 500 pods sur 100 workloads avec 24 heures d'historique (rétention par défaut), sur la seule réplique active (§5.5.2, §5.5.4). Le contrôleur tenait déjà cette charge avec 7 jours d'historique et des sidecars qui importaient les segments de leurs pairs.

#### 5.5.1 Ressource `MimicWorkload`

```yaml
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata:
  name: checkout          # nom du workload (§5.4), dans le namespace de l'application
  namespace: shop
spec:
  mode: mock              # passthrough | record | mock | mock-partial
  snapshot: 3f9a0c1e5b7d2a46   # facultatif : rejoue ce snapshot au lieu d'en construire un
  selector:               # facultatif : seuls ces pods appliquent l'état souhaité (label selector standard)
    matchLabels:
      pod-template-hash: 7d9f8c6b5
  capture:
    inbound: metadata     # facultatif : full | metadata | off (§5.2) ; absent, celle de la configuration
  mock:
    websocket: refuse     # facultatif : relay | refuse (§3.4) ; absent, celui de la configuration
status:
  snapshot: 3f9a0c1e5b7d2a46   # snapshot effectivement servi
  pods: 3                 # pods sélectionnés vus récemment
  aligned: 3              # parmi eux, ceux qui appliquent l'état souhaité
  unmatched: 12           # requêtes sans réponse du mock relevées par ces pods (§5.3)
  observedGeneration: 4
  conditions:
    - type: Aligned       # tous les pods sélectionnés vus récemment appliquent l'état souhaité
      status: "True"
    - type: Allowed       # False quand le garde-fou refuse le mode (raison ProductionNamespace)
      status: "True"
```

Réconciliation (fonction pure dans le contrôleur, testée sans cluster) :

- `spec.mode` en `mock` ou `mock-partial` sans `spec.snapshot` : le contrôleur construit un snapshot si `status.snapshot` est vide, puis l'écrit dans `status.snapshot`. Le snapshot reste le même tant que le mode ne quitte pas le mock, ce qui garde les pods alignés même si de nouveaux échanges arrivent.
- `spec.snapshot` renseigné : ce snapshot est servi ; s'il n'existe pas, la condition `Aligned` passe à `False` avec la raison `SnapshotNotFound` et les sidecars gardent leur état.
- `spec.mode` en `record` ou `passthrough` : aucun snapshot, `status.snapshot` est vidé.
- **Garde-fou production** : si le namespace de la ressource porte le label `env=production`, `mock` et `mock-partial` sont refusés. Aucun snapshot n'est construit ; l'état souhaité reste celui d'avant s'il n'était pas un mock, sinon il devient `record` sans snapshot (un namespace labellisé après coup sort donc du mock). Les conditions `Aligned` et `Allowed` passent à `False` avec la raison `ProductionNamespace` et un message explicite. Le contrôleur surveille les labels des namespaces : poser ou retirer le label réconcilie aussitôt les ressources du namespace. En plus, dans un tel namespace, le battement ne répond jamais `mock` ni `mock-partial`, et un pod qui annonce l'un de ces modes (configuration, état rechargé du disque) reçoit `record`. Le contrôleur lit les labels des namespaces avant de servir les sidecars : un redémarrage ne laisse pas de fenêtre sans garde-fou. En option (Kubernetes ≥ 1.30), la `ValidatingAdmissionPolicy` de `deploy/controller/production-guard.yaml` refuse la ressource dès l'admission ; elle ne remplace pas le garde-fou du contrôleur, qui couvre aussi un label posé après coup.
- **Sélection** (`spec.selector`, même sémantique qu'un label selector Kubernetes : `matchLabels` et `matchExpressions` avec `In`, `NotIn`, `Exists`, `DoesNotExist` ; un sélecteur vide sélectionne tous les pods, une expression invalide aucun) : seuls les pods sélectionnés reçoivent l'état souhaité. Les autres reçoivent un état vide, donc le **mode et la capture de leur configuration** (`record` avec la configuration de l'injecteur) : c'est le mode de repli, sans snapshot. Un pod est sélectionné d'après les labels lus par le contrôleur dans l'API Kubernetes à l'authentification de son jeton (§7.1, gardés 60 s) ; ce que le sidecar annonce n'entre pas en compte. `status.pods` et `status.aligned` ne comptent que les pods sélectionnés ; l'interface montre les autres comme « non sélectionnés ». Le snapshot est construit à partir des échanges et des exemples par endpoint (§5.5.3) de tout le workload, pods non sélectionnés compris.
- **Capture** (`spec.capture.inbound`) : transmise aux pods sélectionnés avec le mode ; absente, chaque sidecar garde celle de sa configuration. Un pod n'est aligné que s'il applique aussi la capture demandée.
- **WebSocket en mock** (`spec.mock.websocket`, `relay` ou `refuse`, §3.4) : même règle que la capture. Transmis aux pods sélectionnés avec le mode, quel qu'il soit (il ne prend effet qu'en `mock` et `mock-partial`) ; absent, chaque sidecar garde celui de sa configuration. Un pod n'est aligné que s'il l'applique ; un sidecar d'une version antérieure, qui ne l'annonce pas, reste donc non aligné. Une valeur inconnue laisse l'état souhaité inchangé, avec la raison `InvalidMock` dans la condition `Aligned`. Changer ce réglage sans quitter le mock garde le snapshot courant.
- Ressource supprimée : l'état souhaité redevient vide.
- `status` est mis à jour à chaque changement d'alignement des pods, ou du nombre de requêtes sans réponse.
- **Requêtes sans réponse** (`status.unmatched`) : somme des totaux annoncés dans leur battement (§5.3) par les pods sélectionnés que le contrôleur connaît encore (jusqu'à `pod_forget`) ; un sidecar d'une version antérieure, qui n'annonce rien, compte pour 0. Toujours publié, 0 compris, pour qu'un patch de fusion remette le compteur à zéro. Colonne `Unmatched` de `kubectl get mw`.

#### 5.5.2 Tenue en charge

Aucun coût du contrôleur ne grandit avec l'historique : ni la liste des segments, ni la mémoire, ni le disque, ni la durée d'un battement.

- **Seul écrivant, index en mémoire** : le contrôleur est le seul processus qui écrit sur son support : seule sa réplique active y écrit, et une seule l'est à la fois (§5.5.4). Il tient en mémoire l'index des segments de chaque workload, construit par **une seule** liste du support au premier accès, puis mis à jour à chaque ingestion, compaction ou suppression. La date d'un segment brut est celle de son écriture ; celle d'un segment horaire est la fin de son heure moins une milliseconde, de sorte qu'un curseur `since_ms` antérieur à l'un de ses échanges le liste encore. `GET …/segments` répond depuis cet index sans relire le support : segments encore dans la rétention, du plus ancien au plus récent, et seulement ceux datés d'au moins `since_ms` s'il est donné. Un sidecar à jour ne liste plus (§5.4) : la liste et la lecture des segments restent servies aux sidecars d'une version antérieure, le temps d'une montée de version. Un fichier déposé sur le support par un autre processus n'est vu qu'après un redémarrage ou une prise de relais (§5.5.4). La liste des workloads suit la même règle : le support est listé une fois, puis un nouveau workload entre par son premier battement ou son premier segment.
- **Ingestion idempotente** : le sidecar donne l'identifiant de son segment (`?segment=`, un ULID). Un segment déjà rangé sous cet identifiant n'est ni réécrit ni réimporté, et la réponse est `200` au lieu de `201` (`mimic_controller_segments_total{result="duplicate"}`). Sans identifiant (ancien sidecar), le contrôleur en génère un.
- **Verrous courts** : les pods, l'état souhaité et la ressource d'un workload sont protégés par un verrou court, jamais tenu pendant une lecture ou une écriture du support, ni pendant un calcul. Les échanges en mémoire ont le leur, tenu le temps d'ajouter ou d'évincer des échanges. Un battement, la liste des workloads, le statut des ressources et `/metrics` n'attendent donc jamais une inférence, un snapshot ou un import.
- **Calculs hors du serveur** : l'inférence, la construction d'un snapshot et les requêtes enregistrées (tri et mise en forme compris) s'exécutent hors des threads qui servent les requêtes, sur une vue figée des échanges en mémoire, sans les copier. L'ingestion continue pendant ce temps.
- **Pods oubliés** : un pod sans battement depuis `pod_forget` (10 minutes par défaut) disparaît de l'interface, du statut et des métriques.
- **Mémoire bornée** : un workload garde au plus `max_exchanges_per_workload` échanges, les plus récents, et l'ensemble des workloads au plus `max_exchanges_total` : au-delà, les plus anciens échanges du workload qui en a le plus sont évincés. Comme dans le sidecar (§5.2), avec la même clé `samples_per_operation` (20 par défaut) et le même couple (opération, code retour), l'opération étant (sens, authority, méthode, forme du chemin), seuls `samples_per_operation` échanges par couple gardent leurs bodies en mémoire : les premiers reçus, les suivants étant gardés sans body jusqu'à ce qu'une éviction libère une place. Les exemples par endpoint (§5.5.3) sont gardés à part, hors de ces plafonds : aucune éviction ne les retire. Après un redémarrage, les échanges d'un workload ne sont relus qu'à sa première vue (interface, requêtes, snapshot) : ingérer un segment ne relit pas l'historique, l'index suffit à dédoublonner. La relecture va du plus récent au plus ancien et s'arrête au plafond du workload.
- **Découverte en tâche de fond** : les documents OpenAPI d'un workload sont recalculés au plus une fois par `discovery_interval` (30 s par défaut), en tâche de fond. Une vue sert le dernier document calculé, même si de nouveaux échanges sont arrivés depuis, et déclenche le recalcul si l'intervalle est écoulé ; seule la première vue d'un workload attend le calcul.
- **Rétention et compaction** : toutes les minutes, le contrôleur écrit les exemples par endpoint qui ont changé (§5.5.3), supprime les segments plus vieux que `retention` (24 heures par défaut), puis fusionne les segments bruts de chaque heure UTC révolue depuis plus de 10 minutes en un segment horaire `exchanges/_hour/<AAAAMMJJHH>.jsonl` : échanges dédoublonnés par identifiant, au plus `samples_per_operation` bodies par couple (opération, code retour) au sens du §5.2, choisis par le même réservoir déterministe, les autres gardés sans body. Le segment horaire ne dépend donc pas de l'ordre dans lequel les segments bruts sont relus. Le segment horaire est écrit avant la suppression des segments bruts : un arrêt entre les deux ne perd rien et la passe suivante refait la fusion. Les segments relus sont déjà redactés et la compaction ne fait qu'en retirer des données. Enfin, si les segments d'un workload dépassent `max_bytes_per_workload`, les plus anciens sont supprimés. Un workload garde ainsi au plus un fichier par heure de rétention, plus les segments bruts des 70 dernières minutes au plus, dans un volume borné. Aucune de ces suppressions ne retire un exemple par endpoint : un segment n'est supprimé qu'une fois ses échanges proposés aux exemples et les exemples écrits (§5.5.3). Les snapshots, `desired.json` et `exemplars.jsonl` ne sont jamais supprimés.

#### 5.5.3 Exemples par endpoint

Le mock sert à tester l'application, pas à garder un historique complet : l'historique est court (`retention`, 24 heures par défaut), mais chaque endpoint que le workload a appelé doit rester mockable, même s'il ne l'a été qu'une fois, longtemps avant la bascule. Le contrôleur garde donc, pour chaque workload, **au moins un exemple rejouable par endpoint**, hors de l'historique.

- **Endpoint** : couple (opération, code retour), l'opération étant (sens, authority, méthode, forme du chemin) comme pour `samples_per_operation` (§5.2) : `GET /users/1` et `GET /users/2` vers la même authority, en `200`, sont le même endpoint.
- **Exemple** : un échange **rejouable** de l'endpoint. En sortie, sa réponse a un body capturé ou vide (c'est ce que le mock rejoue) ; en entrée, sa requête a un body capturé ou vide (c'est ce que le rejeu envoie, §5.7). Un échange sans code ou de code `1xx` (poignée de main WebSocket, jamais rejouée, §3.4) n'est jamais un exemple.
- **Sélection** (`mimic-core::exemplar`, fonction pure) : l'exemple d'un endpoint est le plus récent des échanges rejouables reçus pour lui (date de début, puis identifiant). Il n'est remplacé que par un échange rejouable plus récent du même endpoint : un échange plus ancien, ou sans body rejouable, ne le remplace jamais. Sous la borne ci-dessous, les exemples ne dépendent pas de l'ordre d'arrivée des échanges.
- **Source** : chaque échange est proposé tel qu'il est ingéré, avant que le plafond de bodies en mémoire ne retire les siens (§5.5.2), puis, s'il ne l'a pas déjà été, à la relecture du support, à la compaction et avant la suppression de son segment. Les échanges viennent de segments que le sidecar a redactés avant de les publier (§7, RED-04) : le contrôleur ne fait que recopier des échanges déjà redactés, sans rien y ajouter.
- **Ce qui ne les retire pas** : ni la rétention, ni la compaction, ni `max_bytes_per_workload`, ni l'éviction mémoire (`max_exchanges_per_workload`, `max_exchanges_total`). Les exemples sont tenus en mémoire à part et ne comptent dans aucun de ces plafonds. Avant de supprimer un segment, le contrôleur propose ses échanges aux exemples s'il ne l'a pas déjà fait, puis écrit les exemples sur le support ; si cette écriture échoue, aucun segment du workload n'est supprimé à cette passe.
- **Persistance** : `<namespace>/<workload>/exemplars.jsonl`, un exemple par ligne, dans l'ordre des endpoints. Le fichier est réécrit en entier, seulement s'il a changé, à chaque passe de maintenance et à l'arrêt du contrôleur. Il est relu une fois, avant que le workload reçoive son premier exemple après un démarrage : il survit au redémarrage du contrôleur et à la rétention. Les exemples relus sont repris tels quels : ceux avec body d'abord, puis ceux sans body (borne en octets, ci-dessous), qui restent non rejouables et ne sont comptés ni comme refus ni deux fois dans la taille gardée. L'ordre des endpoints dans le fichier ne décide donc pas lesquels gardent leur body.
- **Usage** : le snapshot (§5.6) est construit à partir des échanges en mémoire **et** des exemples : un endpoint appelé une seule fois, il y a plus longtemps que la rétention, reste mockable. La découverte (§6) et les requêtes enregistrées (§5.7) y ajoutent les exemples dont l'échange n'est plus en mémoire. Les exemples ne sont pas des segments : ils ne sont ni listés ni servis aux sidecars, qui ne les reçoivent qu'à travers un snapshot.
- **Par workload** : chaque workload a ses propres exemples, alimentés seulement par les segments que ses pods ont publiés (authentification, §7.1) ; un endpoint vu par les pods d'un autre workload n'entre jamais dans ses exemples ni dans son snapshot.
- **Borne de sécurité** : au plus `max_exemplars_per_workload` endpoints par workload (10 000 par défaut), contre l'explosion de cardinalité (chemins dont un segment varie sans être reconnu comme identifiant, §6.2). Au-delà, un nouvel endpoint n'a pas d'exemple ; les exemples déjà gardés restent, et chacun peut toujours être remplacé par un plus récent. Chaque refus est compté dans `mimic_controller_exemplars_refused_total{reason="count"}` (au plus un par endpoint et par segment relu ou reçu, seul le candidat le plus récent de chaque endpoint d'un segment étant proposé) ; la jauge `mimic_controller_exemplars` donne le nombre d'exemples gardés, tous workloads confondus.
- **Borne en octets** : la taille sérialisée (une ligne de `exemplars.jsonl`) des exemples d'un workload ne dépasse pas `max_exemplar_bytes_per_workload` (64 MiB par défaut), ni celle de tous les workloads `max_exemplar_bytes_total` (512 MiB par défaut). Un nouvel endpoint dont l'exemple ferait dépasser l'une des deux garde un exemple **sans body** (bodies remplacés par leur taille, comme au §5.2) : il reste visible dans la découverte et ses durées servent la distribution de latence, mais il n'est pas rejouable, jusqu'à ce qu'un échange rejouable plus récent tienne dans la borne. Un remplacement plus récent qui ferait dépasser la borne n'a pas lieu : l'ancien exemple est gardé. Chaque cas est compté dans `mimic_controller_exemplars_refused_total{reason="bytes"}` ; la jauge `mimic_controller_exemplars_bytes` donne la taille gardée, tous workloads confondus. Un exemple sans body compte aussi dans la taille mais est toujours gardé : son poids, celui des métadonnées, reste borné par `max_exemplars_per_workload`. La mémoire tenue par les exemples est ainsi bornée par `max_exemplar_bytes_total`, plus les métadonnées des exemples sans body.

#### 5.5.4 Haute disponibilité : une réplique active, une passive

Le contrôleur tourne en deux réplicas, dont **une seule est active** à la fois : elle seule sert les sidecars, surveille les ressources et les namespaces, publie les statuts, fait la maintenance et écrit sur le support. L'autre, **passive**, n'a aucun état, ne lit ni n'écrit le support, et attend de prendre le relais. Pas d'actif/actif : l'index des segments, les exemples, le budget mémoire et la construction des snapshots supposent un seul écrivant (§5.5.2), et une réplique tient déjà la charge visée.

- **Élection** (`mimic-core::lease`, fonction pure) : par un bail `Lease` (`coordination.k8s.io/v1`, `mimic-system/mimic-controller`), comme les contrôleurs de Kubernetes. L'identité d'une réplique est le nom de son pod (`POD_NAME`, API downward). Toutes les `retry_period` (2 s), l'active renouvelle le bail et la passive le relit. Chaque écriture est conditionnelle (`resourceVersion`) : de deux répliques qui prennent le bail en même temps, une seule réussit. La passive prend le bail s'il n'existe pas, s'il n'a pas de détenteur (rendu), ou s'il est **expiré** : vu inchangé (même `resourceVersion`) pendant sa durée (`lease_duration`, 15 s), mesurée par l'horloge monotone de la passive depuis qu'elle a vu cette version, jamais en comparant les dates écrites dans le bail. Un décalage d'horloge entre nœuds ne change donc rien.
- **Perte du bail** : une active dont le dernier renouvellement réussi a **commencé** il y a `renew_deadline` (10 s) ou plus, ou qui trouve le bail à un autre, cesse aussitôt de servir et d'écrire, sans écrire ses exemples, puis son processus s'arrête (code 1) ; Kubernetes le relance en passif. Un renouvellement est écrit après son début et vu par la passive après son écriture, et `renew_deadline` < `lease_duration` (exigé par la configuration) : l'ancienne active a cessé d'écrire avant que la nouvelle puisse prendre le bail. Jamais deux écrivants.
- **Arrêt propre** (`SIGTERM` : `kubectl delete pod`, montée de version, drain) : l'active retire sa marque (ci-dessous), cesse de servir, écrit ses exemples qui ont changé (§5.5.3), puis **rend** le bail (détenteur vide, durée 1 s) : la passive le prend à sa relecture suivante, sans attendre son expiration.
- **Prise de relais** : la nouvelle active recharge l'état persisté **avant** de se déclarer prête : labels des namespaces (garde-fou, §5.5.1), liste des workloads, état souhaité de chacun (`desired.json`, donc son snapshot courant), index des segments et exemples par endpoint ; puis elle suit les `MimicWorkload` et attend leur liste initiale (30 s au plus). Elle sert alors les sidecars, et pose sa marque en dernier. Comme après un redémarrage (CTL-04), les échanges d'un workload ne sont relus qu'à sa première vue (§5.5.2) et la forme de mock d'un snapshot est recalculée à sa première demande, identique octet pour octet, avec le même `ETag` (§5.6). La durée de la prise de relais est publiée (`mimic_controller_takeover_seconds`).
- **Routage** : l'active porte le label `mimic.platform.io/leader: "true"`, que le Service sélectionne : il ne route que vers elle. Elle le pose sur son pod à la prise de relais, après l'avoir retiré de tout autre pod du contrôleur (ancienne active arrêtée sans avoir pu le retirer), et le retire à l'arrêt propre. Elle pose aussi `controller.kubernetes.io/pod-deletion-cost: "100"` : une réduction du nombre de réplicas ou une mise à jour progressive retire la passive d'abord, ce qui limite une mise à jour à une seule bascule. Les **deux répliques sont `Ready`** (`/healthz` répond `200`) : une passive jamais prête bloquerait `kubectl rollout status` et la mise à jour progressive, et le PodDisruptionBudget, qui ne compte que les pods prêts, interdirait d'évincer l'active, donc de drainer son nœud. Une passive appelée malgré tout (endpoints pas encore à jour, port-forward) répond `503` avec `Retry-After: 2` à tout sauf `/healthz` et `/metrics`, sans toucher au support.
- **Côté sidecars, rien ne change** : ils s'adressent au Service. Pendant la bascule, un battement ou une publication échoue comme quand le contrôleur est absent (§5.5) : le sidecar garde son mode, sa capture et son snapshot ; un sidecar en mock continue de servir son moteur, sans le reconstruire ni retélécharger son snapshot ; la publication garde ses segments dans sa file bornée (§5.4) et les renvoie sous le même identifiant (ingestion idempotente, §5.5.2). Les battements reprennent au premier `mode_poll` après la prise de relais, et les pods s'y alignent de nouveau sur le même snapshot ; la publication reprend après son attente de reprise (`share.interval` doublé par échec, avec gigue : de 10 à 40 s après une coupure de quelques secondes). Un sidecar qui n'avait pas encore son snapshot le télécharge de la nouvelle active.
- **Délai de bascule** : après un arrêt propre (bail rendu), relecture par la passive (≤ `retry_period`, 2 s), prise de relais (rechargement : quelques secondes avec l'historique par défaut), mise à jour des endpoints du Service (de l'ordre d'une seconde), battement suivant (≤ `mode_poll`, 2 s) : moins de 10 s en pratique. Après un arrêt brutal (nœud perdu, processus tué sans `SIGTERM`), il faut d'abord que le bail expire : jusqu'à `lease_duration` + `retry_period` (17 s) de plus. Le test de bout en bout borne la reprise à 30 s (E2E-06), ce qui couvre aussi le cas brutal.
- **Ce qui se perd à la bascule**, comme à un redémarrage : l'état des pods, reconstruit par leurs battements en `mode_poll` ; les documents de découverte et les formes de mock en cache, recalculés à la demande ; la place des bodies en mémoire, refaite à la relecture. Après un arrêt brutal seulement, les exemples changés depuis la dernière passe de maintenance (une minute au plus) ne sont pas écrits ; mais un segment n'est supprimé qu'une fois ses exemples écrits (§5.5.3) : ses échanges sont encore sur le support et la nouvelle active les propose de nouveau aux exemples à la relecture. **Ce qui ne se perd pas** : segments, snapshots, état souhaité, exemples, statut des ressources (republié), et le mock que servent les sidecars.
- **Stockage** : les deux répliques doivent joindre le support, une seule y écrit. Avec `s3://`, rien à prévoir : c'est le choix recommandé en production, sur plusieurs nœuds. Avec `file://` sur un PVC, la passive monte le même volume sans y toucher : sur plusieurs nœuds, le PVC doit être `ReadWriteMany` (EFS, Filestore, Azure Files, CephFS, NFS…). Un PVC `ReadWriteOnce` (défaut des manifests) ne se monte que sur un nœud : il suffit sur un cluster d'un nœud (kind) ou avec un volume local, qui attache les deux répliques au même nœud ; la passive couvre alors la perte du processus ou du pod, pas celle du nœud. Avec un disque réseau `ReadWriteOnce` sur plusieurs nœuds, une passive placée sur un autre nœud reste en `ContainerCreating` (`Multi-Attach`) : il faut alors un PVC `ReadWriteMany` ou `s3://`, ou bien retirer l'anti-affinité et ne couvrir que la perte du pod.
- **Déploiement** : `replicas: 2` ; `RollingUpdate` avec `maxSurge: 1` et `maxUnavailable: 0` (une nouvelle réplique est prête avant qu'une ancienne parte, l'active part en dernier) ; PodDisruptionBudget `minAvailable: 1` (un drain n'évince jamais les deux à la fois) ; anti-affinité **préférentielle** par nœud (deux nœuds quand c'est possible, un seul sinon). Sans `leader_election` dans la configuration, le contrôleur tourne comme avant, en une seule réplique active sans bail.

#### 5.5.5 Démarrage d'un pod piloté

Un pod neuf (mise à l'échelle, éviction, rollout) n'a aucun état souhaité dans son `emptyDir`. Sans précaution, son application démarrerait avant le premier battement, dans le mode de la configuration (`record`), et appellerait les vraies dépendances alors que son workload est en mock ; avec un contrôleur injoignable, il y resterait. Sous contrôleur, l'application d'un pod injecté n'émet donc aucun trafic sortant avant que son sidecar connaisse l'état souhaité.

- **Barrière de démarrage** : le sidecar est un native sidecar (init container `restartPolicy: Always`, §9.4) dont la `startupProbe` interroge `/readyz`. Le kubelet ne démarre l'init container suivant, puis les conteneurs de l'application, qu'une fois la `startupProbe` du sidecar réussie (Kubernetes ≥ 1.29) ; tant qu'elle échoue, le pod reste en `Init`. `/readyz` répond `503` tant que la barrière est fermée. `/healthz`, la sonde de vivacité, répond toujours `200` : elle ne dépend ni du contrôleur ni de la barrière.
- **Ouverture** : la barrière s'ouvre au premier battement auquel le contrôleur répond, une fois l'état souhaité appliqué (mode, capture, WebSocket) **et**, s'il désigne un snapshot, ce snapshot chargé (forme de mock téléchargée, ou relue du cache de `store.dir`). L'état est alors `aligned`. Tant que la barrière est fermée, l'état reçu s'applique dès la réponse, avant le téléchargement du snapshot (en mock, sur les données locales, comme au §5.6) ; une fois la barrière ouverte, le snapshot est chargé avant le mode, comme pour toute bascule.
- **En attendant** : le dernier état souhaité gardé dans `store.dir` (`controller/desired.json`) est rechargé et appliqué **avant** que les écoutes n'acceptent leur première connexion. Il n'existe qu'au redémarrage d'un conteneur, quand l'application peut déjà tourner : si c'est un mock, rien de ce qu'elle envoie ne sort. Sans état connu, le sidecar attend dans le mode de sa configuration, ou en `mock` sur ses données locales avec le repli `mock-local` ou `fail` (ci-dessous), pour qu'un redémarrage du seul sidecar ne laisse rien sortir. Le premier battement auquel le contrôleur répond est écrit dans `desired.json`, même vide : un redémarrage du conteneur le retrouve comme état connu.
- **Délai** (`share.startup.timeout`, 30 s par défaut, au plus 300 s, compté depuis le démarrage du processus) : si la barrière est encore fermée à son échéance,
  - un état est connu (reçu pendant ce démarrage sans que son snapshot soit encore chargé, ou rechargé de `store.dir`) : il reste appliqué et la barrière s'ouvre, état `last-known` ; le snapshot est retenté à chaque battement ;
  - aucun état n'est connu : le repli `share.startup.fallback` s'applique, état `fallback` :

    | Repli | Mode appliqué | Barrière |
    |-------|---------------|----------|
    | `record` **[DÉFAUT]** | `record` | ouverte |
    | `passthrough` | `passthrough` | ouverte |
    | `mock-local` | `mock` sur les données locales (vides pour un pod neuf : `599`, §5.3) ; rien ne sort | ouverte |
    | `fail` | `mock` sur les données locales, rien ne sort | **fermée** : l'application ne démarre pas tant que le contrôleur n'a pas répondu |

  Les battements continuent dans tous les cas : dès que le contrôleur répond, l'état souhaité s'applique et l'état devient `aligned` (ou `last-known` si son snapshot manque encore). Une barrière ouverte ne se referme jamais.
- **Pourquoi `record` par défaut** : Mimic ne doit jamais rendre une application dépendante de son plan de contrôle. Le contrôleur n'est pas dans le chemin des requêtes (§5.5), et l'injection est en `failurePolicy: Ignore` pour la même raison (§9.4). Le repli ne joue qu'au cumul d'un pod neuf et d'un contrôleur injoignable plus longtemps que le délai, lequel couvre une bascule entre ses répliques (moins de 30 s, E2E-06). Or Mimic est aussi injecté dans les namespaces de production, où le mock est interdit (§5.5.1) : avec `fail` par défaut, une panne du contrôleur y bloquerait les rollouts et la mise à l'échelle ; avec `mock-local`, leurs pods neufs répondraient `599` à chaque appel sortant. `record` est enfin le mode de la configuration de l'injecteur, celui que reçoit déjà un pod sans `MimicWorkload` ou non sélectionné (§5.5.1). Un workload dont les dépendances ne doivent jamais recevoir de tir choisit `mock-local` ou `fail` dans sa propre configuration (`mimic.platform.io/config`, §9.4).
- **Après l'ouverture**, `/readyz` répond `200` quel que soit l'état du contrôleur : une readiness qui le suivrait retirerait des Services tous les pods injectés pendant sa panne. L'alignement de chaque pod reste visible dans le statut de la ressource (`Aligned`, §5.5.1), dans `/api/status` (`controller.startup`) et dans les métriques (§9.3).
- **Fenêtre de la sonde** : la `startupProbe` posée par l'injecteur (période 1 s) tolère `share.startup.timeout` + 30 échecs, soit 60 par défaut : le sidecar ouvre sa barrière avant que le kubelet ne le relance, la marge couvrant le démarrage du processus et la construction du moteur de mock. Le délai est lu dans `sidecar_config` ; avec une configuration montée par `mimic.platform.io/config`, que le webhook ne lit pas, la fenêtre vaut 330 (le délai maximal de 300 s, plus la marge). Avec `fail`, le kubelet relance le sidecar à la fin de chaque fenêtre (`Startup probe failed`, puis attente croissante entre les relances) : le pod reste en `Init` jusqu'au retour du contrôleur.
- **Hors contrôleur** (partage direct, sidecar isolé), aucun état souhaité n'est attendu : la barrière est ouverte dès que les écoutes sont prêtes, et `share.startup` est ignoré.
- **Observabilité** : `/readyz` répond l'état de la barrière en texte : `waiting` (fermée), `aligned`, `last-known` ou `fallback` ; `/api/status` le donne avec le repli et le délai (`controller.startup`). Métriques `mimic_startup{state}`, `mimic_heartbeats_total{result}` et `mimic_heartbeat_last_success_timestamp_seconds` (§9.3). Un battement en échec est journalisé en `warn` au plus une fois toutes les 30 s, avec le nombre d'échecs omis, et son rétablissement en `info`. L'échéance du délai sans alignement est journalisée en `warn` (état connu ou repli appliqué), ou en `error` avec `fail`.

### 5.6 Cohérence du mock entre les pods : snapshots

Plusieurs pods d'un même workload doivent répondre de la même manière en mock, y compris sur un endpoint qu'un pod n'a jamais vu passer. Le partage seul (§5.4) ne suffit pas : à un instant donné, deux pods n'ont pas forcément importé les mêmes segments, et leurs stores bornés n'évincent pas les mêmes échanges. Sous contrôleur, le snapshot est d'ailleurs la seule source commune : les sidecars ne s'importent plus de segments (§5.4).

- **Snapshot** : jeu figé d'échanges sortants d'un workload, construit par le contrôleur au moment de la bascule en mock à partir des échanges du workload en mémoire et de ses exemples par endpoint (§5.5.3). Il est immuable et identifié par une empreinte de son contenu (`sha256`, 16 caractères hexadécimaux) : mêmes échanges, même identifiant, quel que soit l'ordre d'arrivée.
- **Construction** (`mimic-core::snapshot`, fonction pure) : échanges sortants terminés, dédoublonnés par identifiant (de deux versions d'un même échange, l'une sans body en mémoire et l'autre gardée comme exemple, celle dont la réponse est rejouable l'emporte), triés par date puis identifiant, au plus `snapshot.max_per_key` (50) par couple (authority, méthode, chemin, code), les plus récents gardés. Le plafond est compté à part pour les échanges dont la réponse est rejouable (body capturé ou vide) et pour les autres (body tronqué ou écarté, notamment au-delà de `capture.samples_per_operation`, §5.2) : sur un endpoint très sollicité, les plus récents n'ont plus de body, et ils ne doivent pas évincer les exemples rejouables. Les seconds ne servent qu'à la distribution de latence.
- **Bascule** : passer `spec.mode` à `mock` construit un snapshot et le désigne comme état souhaité ; renseigner `spec.snapshot` réutilise un snapshot existant, ce qui permet de rejouer un tir de perf dans des conditions identiques d'une semaine à l'autre. Chaque sidecar télécharge une fois la **forme de mock** du snapshot (ci-dessous), la garde en cache (sur disque si `store.dir` est configuré), puis construit son moteur de mock **uniquement** à partir d'elle. Tant que le snapshot n'est pas chargé, y compris quand son téléchargement échoue (il est retenté à chaque battement), le sidecar mocke avec ses données locales, c'est-à-dire ce que son pod a vu (§5.4), et le signale (`snapshot: null` dans son battement) : un passage en mock ne laisse jamais le trafic sortir.
- **Forme de mock** (`mimic-core::snapshot::MockSnapshot`, fonction pure du snapshot) : ce que le moteur du sidecar (§5.3) garde du snapshot, et rien d'autre. Le moteur ne compare à une requête que les échanges rejouables de son opération ; des autres, il ne garde que la durée, dans la distribution de latence de l'opération. La forme de mock contient donc l'identifiant du snapshot, son nombre d'échanges, ses échanges rejouables de code ≥ 200 **tels quels**, dans l'ordre du snapshot, et, pour chaque opération (authority, méthode, forme du chemin, §5.2) désignée par son premier échange, la liste des durées de tous ses échanges de code ≥ 200, dans l'ordre chronologique. Un moteur construit sur la forme de mock est le même qu'un moteur construit sur le snapshot : mêmes candidats et mêmes réponses pour toute requête, avec toute stratégie, et, à graine égale, mêmes latences tirées. Déduite du seul contenu du snapshot et sérialisée sous une forme canonique (un document JSON), elle donne les mêmes octets à tous les pods.
  - **Pourquoi pas les plafonds du store du sidecar** (`capture.samples_per_operation` par opération et code) : le moteur bâti sur un snapshot n'en applique aucun, tous les échanges rejouables du snapshot sont des candidats, et en retirer changerait des réponses (un chemin exact servi par un autre de même template). Ce qui pèse dans un snapshot, ce sont les échanges sans body : au-delà du plafond de bodies (§5.2, §5.5.2) presque tous les échanges d'un endpoint très sollicité en sont, et un chemin de forte cardinalité (`/v1/charges/ch_…`, un segment non reconnu comme identifiant) en garde jusqu'à `snapshot.max_per_key` par chemin. Chacun ne compte que par sa durée.
  - **Téléchargement** : `GET …/snapshots/{id}/mock` (§9.2b), compressée selon l'`Accept-Encoding` de la requête (`zstd` de préférence, puis `gzip`, sinon telle quelle), avec un `ETag` (empreinte `sha256` de la forme sérialisée, 16 caractères hexadécimaux) et `Cache-Control: immutable` ; `If-None-Match` sur cet `ETag` répond `304`. Le sidecar vérifie l'identifiant du snapshot et l'`ETag` de ce qu'il reçoit, une fois décompressé, et ne retélécharge jamais un snapshot déjà chargé : son identifiant, empreinte du contenu, suffit à le reconnaître. Le contrôleur calcule la forme de mock d'un snapshot une seule fois, à la première demande, hors des threads du serveur, puis n'en garde en mémoire que les versions compressées (128 MiB au plus pour l'ensemble, les plus anciennes calculées évincées d'abord ; la forme sans compression est décompressée à la demande) : des pods qui la demandent ensemble attendent le même calcul.
  - **Taille bornée** : ce que le sidecar reçoit du contrôleur, forme de mock ou snapshot complet, ne dépasse pas `share.max_snapshot_bytes` (64 MiB par défaut), compressé comme décompressé ; le corps compressé est libéré avant la lecture du document. Au-delà, le téléchargement est abandonné sans rien garder et traité comme un snapshot indisponible (SNP-07) : le sidecar mocke sur ses données locales, l'erreur nomme l'option dans son statut, et le téléchargement est retenté au battement suivant. La borne protège le sidecar d'un arrêt par manque de mémoire, qui couperait tout le réseau du pod (les règles iptables restent posées) jusqu'à son redémarrage.
  - **Compatibilité** : `GET …/snapshots/{id}` sert toujours le snapshot complet, inchangé, aux sidecars d'une version antérieure. Un sidecar à jour face à un contrôleur d'une version antérieure (`404` sur `…/mock`) télécharge le snapshot complet, vérifie son empreinte, et en déduit lui-même la même forme de mock. Le cache sur disque du sidecar (`<store.dir>/controller/snapshots/<id>.mock.json`) garde la forme de mock ; un cache d'une version antérieure (`<id>.jsonl`) est encore relu. Elle ne contient que des échanges du snapshot, déjà redactés (§7).
- **Même appel vu par plusieurs sidecars** : chaque pod enregistre son exemplaire, avec un identifiant propre ; rien n'est fusionné à tort. Au rejeu, ces exemplaires sont des candidats de même score. La stratégie `best` choisit alors la **réponse la plus fréquente** (même code et même body), puis la plus récente. Une erreur ponctuelle vue par un seul pod ne masque donc pas la réponse habituelle ; pour la reproduire en proportion, utiliser `weighted-status`. Toutes les durées des exemplaires alimentent la distribution de latence de l'opération.
- **Ce qui est identique d'un pod à l'autre** : à requête égale, le code, les headers et le body servis (stratégie `best`). La latence suit la même distribution sur chaque pod, mais le tirage est propre à chaque pod. Les stratégies à état (`round-robin`, `sequence`) avancent indépendamment sur chaque pod.
- **Cycle de vie** : un snapshot (construit ou importé) porte des **métadonnées** à côté de son contenu immuable (`<id>.meta.json`, `mimic_core::snapshot::SnapshotMeta`, format versionné) : date de construction ou d'import, nombre d'échanges, et `pinned` (épinglé), seule à changer ensuite (`mimicctl snapshot pin`). Elles sont écrites une seule fois, à la première apparition du contenu : reconstruire un snapshot déjà connu (même contenu, même identifiant) ne les réécrit jamais, pour ne pas effacer un épinglage posé entre-temps.
  - **Purge côté contrôleur** (CTL-44) : à chaque passe de maintenance (toutes les minutes), pour chaque workload, les snapshots au-delà de `snapshot.keep_last` (5 par défaut) sont supprimés — contenu, métadonnées et forme de mock en cache — sauf les **référencés** (`spec.snapshot` ou `status.snapshot` courants) et les **épinglés**, jamais purgés quel que soit leur âge. `mimicctl snapshot gc` déclenche la même purge immédiatement. Un snapshot d'avant cette fonctionnalité (sans métadonnées) reçoit des métadonnées à la première passe (non épinglé, daté de la dernière écriture connue du support), plutôt que d'être protégé indéfiniment.
  - **Purge côté sidecar** (SNP-12) : le cache local des formes de mock (`<store.dir>/controller/snapshots/*.mock.json`, dans l'`emptyDir` de 512 Mi) est borné par `store.max_cached_snapshots` (5 par défaut) : au-delà, les plus anciennes (par date de dernière écriture) sont supprimées à chaque sauvegarde de l'état souhaité, jamais celle du snapshot actif.
  - **API d'écriture** (CTL-46, §9.2b) : `pin`/`unpin` (`mimicctl snapshot pin`), import d'un snapshot exporté ailleurs (`mimicctl snapshot import`, épinglé par défaut : il n'est référencé par aucun workload et serait sinon purgé aussitôt), et purge immédiate (`mimicctl snapshot gc`). Protégées comme le reste de l'interface, par le seul RBAC `pods/portforward` : aucune n'ouvre le mode ou le trafic, qui restent pilotés par la ressource `MimicWorkload`.
  - **Visibilité** : `status.snapshotExchanges` (nombre d'échanges du snapshot servi) et la condition `SnapshotEmpty` (vraie en mock avec un snapshot sans aucun échange sortant rejouable, signe d'un snapshot construit sans trafic encore enregistré) sont résolus par le contrôleur hors de son verrou court, jamais par une relecture pendant un battement ou `/metrics` (CTL-45, §5.5.2).

### 5.7 Requêtes enregistrées et rejeu

Le contrôleur expose la liste des requêtes entrantes enregistrées pour le workload, toutes instances confondues, les plus récentes d'abord : méthode, chemin, query, headers, body, code, durée, instance. Elle sert à rejouer le trafic réel contre l'application pendant un tir de perf.

- `GET …/requests?direction=inbound&limit=&format=json` : liste JSON, affichée aussi dans l'interface ;
- `format=jsonl` : un échange par ligne, rejouable par n'importe quel outil ;
- `format=k6` : script k6 prêt à lancer (`k6 run -e TARGET=http://checkout:8080 script.js`), qui rejoue les requêtes dans l'ordre d'enregistrement. Les headers redactés et les headers propres à la connexion ne sont pas rejoués ; le script le signale en commentaire et accepte un header d'authentification par `-e AUTHORIZATION=…`.

### 5.8 Tir de charge d'un workload (parcours SRE)

But : mesurer la latence et la consommation d'un pod sous charge, isolé de ses dépendances, pour dimensionner le workload ; et, dans une stratégie de déploiement blue/green hors production, comparer la nouvelle version à l'ancienne avant de la promouvoir. Guide pas à pas : `docs/sre-load-test.md`.

1. **Enregistrer** : le workload tourne en `record` ; le contrôleur accumule ses requêtes entrantes et les réponses de ses dépendances.
2. **Isoler** : `mimicctl mode <workload> mock --capture-inbound metadata [--selector k=v]`. Seules les dépendances sont mockées, depuis un snapshot ; l'entrant arrive toujours à l'application. Les WebSockets sortants restent relayés vers leurs dépendances ; `--mock-websocket refuse` les refuse aussi, pour un isolement sans exception (O3, §3.4). Le TCP non HTTP (bases, brokers) est refusé (`mock.tcp: refuse`, §3.4) : une dépendance TCP à garder réelle s'exclut de l'interception avant le tir. Avec `metadata` (ou `off`), le sidecar ne copie plus les bodies entrants et n'en publie rien (§5.2) : son coût reste négligeable devant celui de l'application. Avec `--selector`, seuls les pods sélectionnés basculent (par exemple le ReplicaSet de la nouvelle version, par `pod-template-hash`), les autres restent dans le mode de leur configuration (§5.5.1).
3. **Tirer** : attendre que `ALIGNED` soit complet, puis rejouer `mimicctl requests <workload> --format k6` contre le Service des pods sélectionnés.
4. **Mesurer** : latence vue par l'appelant, `mimic_request_duration_seconds{direction="inbound"}` ; CPU et mémoire des conteneurs par les métriques du kubelet. Les métriques du sidecar portent le label `version` (§9.3), ce qui permet de comparer deux versions sur un même tableau de bord ou dans une analyse automatique (exemple d'`AnalysisTemplate` Argo Rollouts dans le guide, sans dépendance du code à Argo).
5. **Revenir** : `mimicctl mode <workload> record` (sans `--selector` ni `--capture-inbound`, ce qui les retire).

Dans un namespace labellisé `env=production`, l'étape 2 est refusée (garde-fou, §1) : le blue/green avec mock se joue dans un environnement de préproduction ou de perf, et la promotion en production se fait ensuite sans mock.


### 5.9 Tir de charge intégré : `mimicctl load`

But : rejouer les requêtes entrantes enregistrées contre un workload isolé (§5.8), à une concurrence fixe ou croissante, pour mesurer sa capacité et en déduire une taille de CPU et de mémoire, sans outil externe. Le rejeu k6 (§5.7) reste disponible.

**Requêtes.** Le corpus vient du contrôleur (`…/requests?direction=inbound&format=jsonl&limit=`, comme `mimicctl requests`) ou d'un fichier JSONL d'échanges (`--from`, sortie de `mimicctl requests --format jsonl`), ce qui permet un tir depuis un Job sans port-forward. Chaque requête rejoue la méthode, le chemin, la query et le body de l'échange ; les headers propres à la connexion ne sont pas rejoués, ni les headers redactés (§7), comme pour k6 (§5.7). `--header 'nom: valeur'`, répétable, ajoute ou remplace un header (un jeton valide par exemple). Un échange dont le body de requête est tronqué ou non capturé (§5.2) est écarté : le rejouer enverrait une requête incomplète ; le rapport compte les échanges écartés. Les requêtes sont jouées dans l'ordre d'enregistrement, en boucle, et partagées entre les connexions : la n-ième requête lancée est la requête `n mod taille` du corpus. Le `Host` est celui de la cible. `--http2` parle HTTP/2 sans TLS (prior knowledge), pour un service gRPC ; une cible `https://` est refusée en v1.

**Charge.** Boucle fermée : `C` tâches envoient chacune une requête, attendent la réponse, puis la suivante, chacune sur sa connexion persistante ; au plus `C` requêtes sont donc en cours.

- **Fixe** : `--concurrency C` (défaut 8) avec `--requests N` (exactement `N` requêtes lancées) ou `--duration D` (défaut 60 s).
- **Montée** : `--ramp` enchaîne des paliers de concurrence `--start` (défaut 1), puis multipliée par `--factor` (défaut 2), jusqu'à `--max` (défaut 256, toujours joué en dernier s'il n'est pas atteint par la multiplication), chacun pendant `--step-duration` (défaut 60 s). La montée s'arrête au premier palier saturé.

Une requête qui n'aboutit pas dans `--timeout` (défaut 10 s), une erreur de connexion ou une réponse 5xx est une erreur ; les autres codes sont des succès. Par palier : concurrence, durée, requêtes, débit (succès par seconde), taux d'erreur, latence p50, p95, p99 et maximum (histogramme log-linéaire, précision relative meilleure que 1 %, fusionnable entre tâches).

**Saturation.** Un palier est saturé si son taux d'erreur dépasse `--max-error-rate` (défaut 1 %), si son p99 dépasse `--max-p99` (facultatif), ou si, la concurrence ayant augmenté par rapport au palier précédent, son débit progresse de moins de `--plateau` (défaut 10 %) de cette hausse : en doublant la concurrence, moins de 10 % de débit en plus. Le seuil suit la hausse de concurrence, ce qui rend le critère indépendant de `--factor`. La **capacité** est le palier non saturé de plus haut débit ; si aucun palier ne l'est, il n'y a pas de capacité et le rapport le dit. Saturé par plateau, le palier montre que le workload ne sert pas plus vite avec plus de concurrence : sa ressource limitante (CPU limité, pool, verrou) est atteinte.

**Ressources.** Avec un workload nommé, `mimicctl` relève pendant chaque palier la consommation des pods du workload par l'API `metrics.k8s.io` (metrics-server), toutes les `--sample-interval` (défaut 15 s) ; les pods sont ceux que le contrôleur connaît pour le workload (sélectionnés, §5.5.1), ou ceux de `--pod-selector`. Les conteneurs `mimic` et ceux des mesh (§3.3) sont exclus : seuls ceux de l'application comptent. Les relevés du premier tiers de chaque palier (montée en température) sont ignorés. Par palier : CPU moyen et mémoire maximale (working set) par pod. Sans accès à `metrics.k8s.io` (API absente ou droit refusé), le tir continue et le rapport le signale.

**Recommandation.** Au palier de capacité, par pod : `requests.cpu` = CPU moyen × (1 + `--cpu-headroom`, défaut 30 %), arrondi au millicœur supérieur ; `requests.memory` = mémoire maximale × (1 + `--memory-headroom`, défaut 20 %), arrondie au MiB supérieur, à reprendre comme limite mémoire ; débit par pod et débit par cœur. Elle suppose une répartition égale entre les pods (le Service). Une capacité atteinte par un CPU limité (`limits.cpu`) se voit à un CPU moyen proche de la limite : relever la limite et refaire le tir.

**Isolement.** Avec un workload nommé, `mimicctl load` refuse (code 2) si son mode souhaité n'est pas `mock` ou si ses pods sélectionnés ne sont pas tous alignés : un tir en `record` ou `passthrough` frapperait les vraies dépendances. `--allow-live-dependencies` lève ce refus. Sans workload (`--from` seul), aucune vérification n'est possible : le rapport le rappelle.

**Rapport.** Texte (défaut), `--format markdown` (résumé d'un job GitHub Actions) ou `--format json`. Il liste les paliers, le verdict (palier de capacité, palier et raison de la saturation) et la recommandation. Mêmes mesures, même rapport (déterministe).

Le cœur (requêtes à rejouer, paliers, histogramme, saturation, lecture des quantités Kubernetes, agrégation des relevés, recommandation, rapport) est dans `mimic-core::load`, pur ; `mimicctl` n'y ajoute que le client HTTP, les minuteries et les appels au cluster. Depuis un Job (`deploy/load/job.yaml`), l'image de Mimic contient `mimicctl`.

---

## 6. Inférence de l'API (OpenAPI)

Implémentée dans `mimic-core::infer`, fonction pure : `fn infer(exchanges: impl Iterator<Item = &Exchange>) -> OpenApi`.

Deux documents sont produits :

- **API du service** (trafic entrant) : c'est le livrable principal, base des tirs de perf ;
- **une API par dépendance** (trafic sortant, groupé par `authority`) : alimente le mode `mock` et documente ce que l'app consomme.

### 6.1 Format **[DÉFAUT : OpenAPI 3.1]**

OpenAPI 3.1 parce qu'il est aligné sur JSON Schema 2020-12 (types nullables propres, `examples` natifs). Export 3.0.3 disponible (`mimicctl export --openapi-version 3.0`) pour les outils qui ne lisent pas encore la 3.1. Swagger 2.0 n'est pas proposé.

### 6.2 Templating des chemins

Problème : `/users/42`, `/users/43`, `/users/a1b2...` doivent donner `/users/{userId}`.

Algorithme (implémenté dans `mimic-core::infer::path`) :

1. découper chaque chemin en segments et construire un arbre préfixe par authority, **toutes méthodes confondues** (`GET /carts/1` et `DELETE /carts/2` partagent le template `/carts/{cartId}`) ;
2. un segment **ressemble à un identifiant** s'il est entier, UUID, ULID, hex ≥ 8 (avec au moins un chiffre), base64url ≥ 16 (lettres et chiffres) ou date ISO ; il est alors toujours variable ;
3. un segment qui ne ressemble pas à un identifiant et qui représente plus de 10 % des passages par son noeud reste **littéral** (`/users/me`) ;
4. les autres segments sont variables ; un mot purement alphabétique (`users`, `orders`) ne l'est qu'à partir de 20 mots distincts, pour ne pas fusionner des collections ;
5. un noeud crée un paramètre quand il a au moins **5** valeurs variables distinctes ; leurs sous-arbres sont fusionnés et le traitement continue récursivement ;
6. le nom du paramètre est dérivé du segment littéral précédent singularisé en camelCase (`users` → `userId`, `line-items` → `lineItemId`), sinon `param{n}` où `n` est son rang dans le chemin ; un nom déjà pris reçoit ce rang en suffixe ;
7. le type du paramètre vient des formes observées : `integer` si toutes entières, `string`/`uuid` si toutes UUID, `string` sinon.

### 6.3 Inférence de schéma des bodies

- JSON : fusion incrémentale des types observés. Un champ présent dans 100 % des exemples est `required`. Plusieurs types → `oneOf` (sauf `null` → `type: [T, "null"]`).
- Formats détectés : `date-time`, `date`, `uuid`, `email`, `uri`, `ipv4`.
- Enum : un `enum` ferme le contrat (toute autre valeur devient invalide). Il n'est déclaré que si l'échantillon le justifie, sinon la chaîne reste un `type: string` ouvert. Toutes les conditions suivantes doivent tenir :
  - au moins `min_observations` observations de la chaîne (**20** par défaut ; c'est le nombre de bodies gardés par couple (opération, code) en capture, §5.2) ;
  - au moins deux valeurs distinctes, et au plus `max_values` (**10** par défaut) : une valeur unique est une constante observée, pas un ensemble fermé ;
  - chaque valeur vue au moins `min_value_count` fois (**2** par défaut) : une valeur rare laisse supposer que d'autres n'ont pas encore été vues ;
  - pas de série numérotée : deux valeurs qui ne diffèrent que par leurs chiffres (`user0` et `user6`, `v1` et `v2`) décrivent un nom ou un compteur, pas une liste de constantes.

  Les trois seuils sont réglables dans les options d'inférence (`InferOptions::enums`, §6). Un format détecté (`uuid`, `date`…) l'emporte sur l'`enum`. Les valeurs de l'`enum` sont triées, ce qui garde la sortie déterministe (INF-08).
- Tableaux : schéma des items fusionné sur tous les éléments.
- `application/x-www-form-urlencoded` et `multipart/form-data` : propriétés à plat.
- Autres types : `format: binary`, pas d'inférence.
- Query et headers : paramètres avec type inféré ; un paramètre présent dans 100 % des appels est `required`.

### 6.4 Extensions Mimic

Données utiles aux tirs de perf, ajoutées en extensions `x-mimic-*` (ignorées par les outils standards) :

```yaml
paths:
  /v1/orders/{orderId}:
    get:
      x-mimic-stats:
        count: 18234
        rate_rps_p95: 41.2
        status: { "200": 0.97, "404": 0.025, "500": 0.005 }
        latency_ms: { p50: 12, p90: 30, p99: 120, max: 840 }
      responses:
        "200":
          content:
            application/json:
              schema: { ... }
              examples:
                sample-1: { value: { ... } }
```

`rate_rps_p95` et la distribution des codes servent à générer un profil de charge réaliste (phase 5).

---

## 7. Données sensibles

Par défaut, Mimic **suppose que tout payload contient des données sensibles**. **[DÉFAUT]**

Redaction appliquée **avant** toute écriture sur disque (dans `mimic-core::redact`, testable en isolation) :

| Cible | Règle par défaut |
|-------|------------------|
| Headers | `Authorization`, `Proxy-Authorization`, `Cookie`, `Set-Cookie`, et tout header contenant `token`, `secret`, `key`, `session`, `password` → valeur remplacée par `<redacted:sha256:8 premiers caractères>` |
| Query | mêmes motifs sur les noms de paramètres |
| Body JSON | clés correspondant aux mêmes motifs ; valeurs détectées comme e-mail, numéro de carte (contrôle de Luhn), IBAN, JWT |
| Règles projet | JSONPath et regex additionnels déclarés par service |

Le hash est calculé sur `sel || valeur`, avec un sel propre à l'installation, puis tronqué à 8 caractères hexadécimaux. Le sel vient de `redact.salt`, ou de `MIMIC_REDACT_SALT` (Secret `<workload>-mimic`, §9.4). À défaut, il est tiré au hasard au démarrage et le sidecar l'annonce par un avertissement dans ses logs, qui nomme `MIMIC_REDACT_SALT` : les marqueurs diffèrent alors d'un pod à l'autre et à chaque redémarrage, ce qui fausse le dédoublonnage, les snapshots et les comparaisons entre pods. Il garde la cohérence (deux requêtes avec le même token portent le même marqueur) sans permettre de retrouver une valeur courte par dictionnaire. La redaction est idempotente : un marqueur n'est jamais re-hashé.

Bodies que la redaction ne peut pas analyser : un body JSON ou formulaire tronqué, ou un JSON invalide, est remplacé par `Skipped { size }`, car on ne peut pas garantir qu'il ne contient pas de secret. Les bodies binaires ne sont pas modifiés.

Un body JSON ou formulaire où aucune valeur n'est remplacée est gardé octet pour octet (ordre des clés, espaces, écriture des nombres) : seul un body redacté est réécrit, ce qui garde au mock sa fidélité (O5).

Conséquence pour le mode `mock` : un body redacté est rejoué redacté. Les headers d'authentification sortants ne participent jamais au matching. Si une réponse doit contenir une valeur réaliste, on passe par les règles `rewrite`.

Option `capture.bodies: false` pour ne garder que métadonnées et schémas (inférence faite en mémoire, exemples jetés).

---

### 7.1 Authentification et autorité du contrôleur

Le contrôleur doit être sûr de parler au bon sidecar, et le sidecar sûr de recevoir ses ordres du vrai contrôleur.

- **Sidecar vers contrôleur** : le sidecar monte un jeton de ServiceAccount projeté, d'audience `mimic-controller` et de courte durée (`serviceAccountToken`, renouvelé par le kubelet), et l'envoie en `Authorization: Bearer` à chaque appel. Le contrôleur le valide par `TokenReview` (audience comprise), puis vérifie que :
  - le namespace du ServiceAccount est celui du chemin `/v1/workloads/{namespace}/…` ;
  - le pod lié au jeton (`authentication.kubernetes.io/pod-name`) existe, appartient à ce workload (§5.4) et correspond à l'instance déclarée.
  Sinon : `401` (jeton absent ou invalide) ou `403` (jeton valide pour un autre namespace, pod ou workload). Un pod compromis ne peut donc ni lire, ni polluer les enregistrements d'un autre workload. Les validations sont gardées en cache 60 s, par empreinte du jeton, avec les labels du pod lus à cette occasion : ce sont eux que `spec.selector` compare (§5.5.1), jamais ce qu'annonce le sidecar. En authentification `static` (tests), chaque jeton déclare ses labels (`labels`).
- **Contrôleur vers sidecar** : l'API des sidecars n'est servie qu'en HTTPS, avec un certificat signé par une CA dédiée (Secret géré par cert-manager). Le sidecar ne fait confiance qu'à cette CA (`share.ca_file`), pas aux racines système : un serveur qui se ferait passer pour le contrôleur est refusé. `http://` n'est accepté qu'avec `share.allow_insecure: true`, réservé aux tests.
- **Humains** : aucun ordre ne passe par l'API HTTP du contrôleur. La bascule passe par `MimicWorkload` (RBAC et audit Kubernetes). La consultation passe par l'API agrégée `data.mimic.platform.io/v1alpha1` (§9.2c) : l'API server authentifie l'utilisateur (quelle que soit la méthode de son kubeconfig), le contrôleur l'autorise par `SubjectAccessReview` sur le namespace et le workload demandés. Le ClusterRole `mimic-data-viewer` fourni (`deploy/controller/rbac.yaml`) se lie par RoleBinding dans le namespace d'une équipe : elle lit les données de ses workloads, et de rien d'autre. Le Role `mimic-cli` (`get` et `list` sur `pods`, `create` sur `pods/portforward`, dans `mimic-system`) ne sert plus qu'aux écritures du cycle de vie des snapshots (`mimicctl snapshot pin|import|gc`) et aux clusters sans l'API agrégée : il ouvre l'interface du contrôleur sur **tous** les namespaces, et se réserve donc aux exploitants.
- **Droits du contrôleur** : `get`, `list`, `watch` sur `mimicworkloads` et `patch` sur leur `status` ; `create` sur `tokenreviews` ; `get` sur `pods` ; `get`, `list`, `watch` sur `namespaces`, pour le garde-fou production (§5.5.1) ; dans `mimic-system` seulement (Role), `create` sur `leases`, `get` et `patch` sur le bail `leases/mimic-controller` (écriture conditionnelle par `resourceVersion`), `list` et `patch` sur `pods`, pour l'élection et la marque de la réplique active (§5.5.4) ; `create` sur `subjectaccessreviews` et, dans `kube-system`, la lecture du ConfigMap `extension-apiserver-authentication` (Role standard `extension-apiserver-authentication-reader`), pour l'API agrégée (§9.2c). Rien d'autre.

## 8. Stockage et export

- **Store local** : fichiers JSONL segmentés `local-<n>.jsonl` (compression zstd en v2) dans un `emptyDir` (`/var/lib/mimic`), seulement si `store.dir` est configuré. Ils ne contiennent que des échanges déjà redactés (RED-04).
  - **Écriture** : par un fil dédié, alimenté par une file bornée (4 096 échanges et 16 MiB). Ni sérialisation, ni écriture, ni appel système ne se font sous le verrou du store ou sur un worker du proxy. Le tampon est vidé toutes les 100 ms, et à l'arrêt. File pleine (disque lent) : l'échange reste en mémoire, mais sa ligne n'est pas écrite, et c'est compté (`reason="local_backlog"`).
  - **Rotation** par taille : un segment est fermé au-delà de `store.segment_bytes` (32 MiB par défaut), et seuls les `store.max_segments` (4) segments les plus récents sont gardés, le plus ancien étant supprimé. Le journal occupe donc au plus 128 MiB (plus une ligne), le quart de l'`emptyDir` de 512 Mi posé par l'injecteur : le reste est laissé aux snapshots imposés gardés dans le même répertoire (§5.6, jusqu'à 256 MiB).
  - **Redémarrage** : le store en mémoire est reconstruit à partir des segments présents, du plus ancien au plus récent, dans les limites du §5.2. `DELETE /store` supprime les segments.
- **Export** **[DÉFAUT : stockage objet S3-compatible]** : envoi périodique et à l'arrêt du pod (`preStop`) des segments et des documents OpenAPI vers `s3://<bucket>/<cluster>/<namespace>/<workload>/<date>/`. Chiffrement côté serveur requis.
- **Chargement en mode mock** : depuis le store local si présent, sinon téléchargé depuis S3 pour le workload (`mimic.platform.io/mock-source: s3://...`). C'est ce qui permet d'enregistrer en environnement A et de mocker en environnement B.
- Les ConfigMaps ne sont pas utilisées (limite de 1 MiB).

Format du store : versionné (`schema_version` dans chaque segment) pour que les enregistrements restent lisibles après une montée de version de Mimic.

---

## 9. Configuration et API admin

### 9.1 Configuration

Fichier YAML monté (ConfigMap), surchargeable par annotations. Exemple :

```yaml
mode: record
inbound:
  ports: [8080]
outbound:
  exclude_cidrs: [10.96.0.10/32]
  connect_timeout: 10          # secondes : établissement d'une connexion vers l'amont (§3.5)
server:
  header_read_timeout: 60      # secondes : en-têtes HTTP/1 d'une requête, inactivité keep-alive comprise (§3.5)
  max_connections: 2048        # connexions simultanées, entrant et sortant ensemble (§3.5)
tls:
  intercept: ["api.stripe.com", "*.salesforce.com"]
  ca_dir: /var/run/mimic/ca            # ca.crt et ca-bundle.crt publiés pour l'app
  # ca_cert_file / ca_key_file : CA fournie (Secret cert-manager) ; absente, CA éphémère
  # upstream_ca_file : CA supplémentaire de confiance vers les dépendances
capture:
  bodies: true
  max_body_bytes: 1048576
  samples_per_operation: 20
  max_exchanges: 50000
  max_store_bytes: 67108864   # 64 MiB, budget mémoire du store (§5.2)
  max_inflight_bytes: 67108864  # 64 MiB, bodies copiés en vol par la capture (§5.2)
  inbound: full               # full | metadata | off : capture de l'entrant (§5.2)
version: v1.4.2               # label `version` des métriques (§9.3) ; MIMIC_VERSION l'emporte, MIMIC_POD_TEMPLATE_HASH à défaut
store:
  dir: /var/lib/mimic          # absent : mémoire seule
  segment_bytes: 33554432      # 32 MiB par segment local-<n>.jsonl (§8)
  max_segments: 4
  size_limit_bytes: 536870912  # taille de l'emptyDir (mimic_store_disk_limit_bytes) ; MIMIC_STORE_SIZE_LIMIT_BYTES l'emporte
redact:
  extra_json_paths: ["$.customer.phone"]
mock:
  strategy: best
  unmatched: error
  latency_factor: 1.0        # multiplicateur de la latence rejouée (0 à 100)
  match_headers: ["accept", "x-api-version"]
  websocket: relay            # relay | refuse : poignée de main WebSocket sortante en mock (§3.4)
  tcp: refuse                 # refuse | relay : TCP sortant non HTTP et connexions muettes en mock (§3.4)
share:
  url: https://mimic-controller.mimic-system:8443   # ou s3://bucket/prefixe, file:///var/lib/mimic-shared
  ca_file: /var/run/mimic/controller-ca.crt        # seule CA de confiance pour le contrôleur
  token_file: /var/run/secrets/mimic/token         # jeton de ServiceAccount projeté
  interval: 10       # secondes
  retention: 86400   # secondes (24 heures) : import des pairs en partage direct (§5.4)
  mode_poll: 2       # secondes, battement vers le contrôleur
  max_backlog_bytes: 16777216  # 16 MiB, file des échanges pas encore publiés (§5.4)
  max_pending: 50000           # même file, en nombre d'échanges
  max_snapshot_bytes: 67108864 # 64 MiB, forme de mock téléchargée, compressée comme décompressée (§5.6)
  startup:           # sous contrôleur : barrière de démarrage de l'application (§5.5.5)
    timeout: 30      # secondes (au plus 300) : attente du premier état souhaité
    fallback: record # record | passthrough | mock-local | fail : sans état connu à l'échéance
```

Configuration du contrôleur (`mimic-controller --config`) :

```yaml
listen: 0.0.0.0:8443            # API des sidecars
ui_listen: 127.0.0.1:8081       # interface, par port-forward
tls: { cert_file: /etc/mimic/tls/tls.crt, key_file: /etc/mimic/tls/tls.key, reload_interval: 10 }   # relus à chaud (§9.2b)
auth: { kind: kubernetes }      # TokenReview ; { kind: static, tokens: … } pour les tests
store: file:///var/lib/mimic-controller   # ou s3://bucket/prefixe
retention: 86400                   # secondes (24 heures) : segments supprimés au-delà
max_exchanges_per_workload: 200000
max_exchanges_total: 1000000       # tous workloads confondus
samples_per_operation: 20          # bodies gardés par (opération, code), en mémoire et après compaction
max_bytes_per_workload: 268435456  # segments d'un workload sur le support (256 MiB)
max_exemplars_per_workload: 10000  # endpoints gardés avec un exemple, hors rétention (§5.5.3)
max_exemplar_bytes_per_workload: 67108864  # taille sérialisée des exemples d'un workload (64 MiB)
max_exemplar_bytes_total: 536870912        # tous workloads confondus (512 MiB)
discovery_interval: 30             # secondes, au plus un recalcul de la découverte par intervalle
snapshot:
  max_per_key: 50
pod_timeout: 30   # un pod sans battement depuis ce délai est affiché absent
pod_forget: 600   # un pod sans battement depuis ce délai est oublié
leader_election:  # absent : une seule réplique, sans bail (§5.5.4)
  lease: mimic-controller     # bail coordination.k8s.io/v1
  namespace: mimic-system     # namespace du bail et des pods du contrôleur
  lease_duration: 15          # secondes : bail inchangé depuis ce délai, expiré
  renew_deadline: 10          # secondes : l'active s'arrête si elle n'a pas renouvelé (< lease_duration)
  retry_period: 2             # secondes : renouvellement et relecture (< renew_deadline)
```

L'identité d'une réplique dans l'élection est le nom de son pod, lu dans `POD_NAME` (API downward), sinon `HOSTNAME`.

La configuration est validée au démarrage ; une configuration invalide fait échouer le conteneur avec un message explicite (pas de démarrage en mode dégradé silencieux). Côté sidecar, une période ou un délai nul (`share.interval`, `share.mode_poll`, `share.startup.timeout`, `outbound.connect_timeout`, `server.header_read_timeout`) est refusé, avec un message qui nomme le champ : une période nulle ferait tourner la boucle de partage ou de battement sans attente, et inonderait le contrôleur ou le support. Un plafond nul (`server.max_connections`, `capture.max_inflight_bytes`, `share.max_snapshot_bytes`) l'est aussi, de même qu'un `share.startup.timeout` au-delà de 300 s (fenêtre de la `startupProbe` posée par l'injecteur, §5.5.5) et un `share.startup.fallback` inconnu (le message liste les valeurs admises).

### 9.2 API admin (port 15100, localhost + port-forward)

**Accès** : l'API admin écoute sur toutes les adresses (`0.0.0.0:15100`) pour que le kubelet (sondes) et Prometheus la joignent, mais seules `/healthz`, `/readyz` et `/metrics` répondent à toute adresse. Toutes les autres routes (la page, `/api/*`, `/openapi/*`, `/exchanges`, `/mode` en lecture comme en écriture, `/store`, `/share/sync`, et les chemins inconnus) ne répondent qu'aux connexions venues de la boucle locale, `127.0.0.1` ou `::1` (adresse IPv4 mappée en IPv6 comprise) : un conteneur du pod, `kubectl port-forward` (soumis au RBAC `pods/portforward`) ou `kubectl exec`. Les autres reçoivent `403` sans que la requête soit traitée.

- Seules ces deux adresses comptent comme locales : Istio en mode sidecar relaie vers l'application les connexions des autres pods depuis `127.0.0.6` (§3.3.2), qui est donc refusée comme une adresse distante. Un mesh qui relaierait des connexions distantes depuis `127.0.0.1` les ferait passer pour locales : il faut alors exclure le port 15100 de son interception entrante.
- `/share/sync` n'a pas d'appelant distant : le partage passe par le support ou par le contrôleur, jamais par une connexion entre pods (§5.4). Ni le contrôleur ni `mimicctl` n'appellent l'admin des sidecars ; la route sert aux opérateurs, par port-forward, et aux tests.
- Le proxy de l'API Kubernetes (`kubectl get --raw /api/v1/namespaces/<ns>/pods/<pod>:15100/proxy/…`) se connecte depuis l'API server : il n'obtient que les sondes et `/metrics`.

| Méthode | Chemin | Rôle |
|---------|--------|------|
| GET | `/` | page d'admin (voir ci-dessous) |
| GET | `/api/operations` | opérations découvertes, en JSON |
| GET | `/api/unmatched?limit=` | requêtes sans réponse du mock (§5.3) : `{total, entries}`, au plus `limit` clés (100 par défaut et au plus), de la plus récemment vue à la plus ancienne |
| GET | `/healthz` | vivacité : toujours `200` |
| GET | `/readyz` | barrière de démarrage (§5.5.5) : `503` `waiting` tant qu'elle est fermée, puis `200` avec son état (`aligned`, `last-known`, `fallback`) ; `200` `ok` hors contrôleur |
| GET | `/metrics` | Prometheus |
| GET/PUT | `/mode` | lire / changer le mode |
| GET | `/openapi/inbound` | spec du service |
| GET | `/openapi/outbound/{authority}` | spec d'une dépendance |
| GET | `/exchanges?direction=&path=&limit=` | inspection |
| POST | `/share/sync` | synchronisation immédiate : publication, et import des pairs en partage direct (§5.4) |
| DELETE | `/store` | purge |

**Page d'admin** : page HTML autonome (aucune ressource externe, utilisable par `kubectl port-forward` dans un cluster sans accès Internet), rendue côté serveur à partir des documents OpenAPI du store du sidecar. Sous contrôleur, ce store ne contient que ce que **ce pod** a vu (§5.4) : la page le dit, et renvoie à l'interface du contrôleur pour le workload entier. En partage direct, elle couvre aussi le trafic importé des pairs. Elle montre :

- le mode courant, l'instance, le workload et l'état du partage : sous contrôleur, le nombre d'échanges vus par ce pod, la publication vers le contrôleur (dernière synchro, dernière erreur) et le snapshot chargé ; en partage direct, les échanges locaux et importés, le nombre de pairs et la dernière synchro ;
- pour le service puis pour chaque dépendance : la liste des opérations (méthode, template, nombre d'appels, répartition des codes, latences p50/p99) ;
- pour chaque opération, dépliable : les paramètres (chemin, query, headers) avec leur type et un marqueur **requis**, le schéma du body de requête, les codes de réponse avec schéma et exemples ;
- des liens de téléchargement des documents OpenAPI.

**Relevé des requêtes sans réponse** (`GET /api/unmatched`) : `total` compte les requêtes refusées par le mock depuis la dernière remise à zéro (§5.3), évictions comprises ; chaque élément d'`entries` porte `authority`, `method`, `path` (forme du chemin, sans valeur), `reason` (`unmatched` pour un `599`, `too_large` pour un `413`), `count`, `first_ms` et `last_ms`. Tri : `last_ms` décroissant, puis la clé (authority, méthode, chemin, raison) ; deux lectures du même relevé sont identiques. La route est réservée à la boucle locale, comme le reste de l'admin.

Les documents OpenAPI de la page, de `/api/operations` et de `/openapi/*` sont recalculés quand le store a changé, sur le pool de threads bloquants du runtime et hors du verrou de leur cache : un calcul long ne retient aucun worker, donc ni le trafic, ni les sondes, ni le battement. Des requêtes simultanées attendent le même calcul.

### 9.2b API du contrôleur

Deux ports :

- **8443, API des sidecars**, HTTPS, jeton de ServiceAccount obligatoire (§7.1). Préfixe `/v1/workloads/{namespace}/{workload}` noté `…`.

| Méthode | Chemin | Rôle |
|---------|--------|------|
| POST | `…/segments?instance=&segment=` | ingestion d'un segment JSONL de son propre pod ; `segment` (ULID) rend l'envoi idempotent : `201` à la première réception, `200` ensuite (§5.5.2) |
| GET | `…/segments?since_ms=` | liste des segments encore dans la rétention (`key`, `instance`, `modified_ms`, `size`), du plus ancien au plus récent ; avec `since_ms`, seulement ceux datés d'au moins cette valeur (ms Unix). Gardée pour les sidecars d'une version antérieure : un sidecar à jour ne liste plus (§5.4) |
| GET | `…/segments/{instance}/{file}` | contenu d'un segment (même remarque) |
| POST | `…/heartbeat` | battement `{instance, mode, snapshot, capture_inbound, mock_websocket, unmatched}`, réponse : état souhaité du pod `{mode, snapshot, capture_inbound, mock_websocket}` (§5.5). `unmatched` (`{total, entries}`, au plus 20 clés, §5.3 ; le contrôleur ne garde que les 20 premières) est facultatif : un sidecar d'une version antérieure ne l'envoie pas, et un contrôleur d'une version antérieure l'ignore, comme tout champ inconnu du battement |
| GET | `…/snapshots/{id}` | contenu JSONL d'un snapshot complet, inchangé : gardé pour les sidecars d'une version antérieure (§5.6) |
| GET | `…/snapshots/{id}/mock` | forme de mock du snapshot (§5.6), un document JSON, compressé selon `Accept-Encoding` (`zstd`, `gzip`), avec `ETag` ; `If-None-Match` : `304` |
| GET | `/healthz` | probe |
| GET | `/metrics` | métriques Prometheus du contrôleur (ci-dessous), **sans jeton**, comme `/healthz` : collectées sur chaque réplique, active ou passive (§5.5.4) |

Sur les routes `…`, le jeton est validé et l'appelant rattaché au namespace et au workload du chemin **avant** que le corps de la requête ne soit lu : une requête sans jeton valide reçoit `401` ou `403` sans qu'un octet de son corps soit attendu ni gardé en mémoire. L'instance déclarée dans le corps du battement est vérifiée ensuite ; le corps d'un battement est limité à 64 KiB, celui d'un segment à 64 MiB.

L'état souhaité d'un workload (`desired.json` sur le support) est relu à son premier usage. Absent : état vide. Illisible (JSON invalide) : état vide, avec une erreur au journal. Si le support échoue (erreur autre que l'absence du fichier), rien n'est retenu : l'appel répond `503`, l'erreur est journalisée et la lecture est retentée à l'appel suivant ; une `MimicWorkload` reçue entre-temps est réappliquée à la passe de maintenance suivante. Une erreur passagère du support ne fait donc jamais oublier l'état souhaité ni construire un nouveau snapshot à la place du snapshot courant.

La NetworkPolicy fournie ouvre le port 8443 aux sidecars (label `mimic.platform.io/sidecar: "true"`) et aux pods des namespaces labellisés `mimic.platform.io/monitoring: "true"` (collecteur Prometheus ou VictoriaMetrics) ; `deploy/example/vmpodscrape-controller.yaml` collecte le contrôleur et le webhook (HTTPS, CA `mimic-controller-ca`).

**Serveurs HTTPS du plan de contrôle** (API du contrôleur, webhook §9.4), même code (`mimic-net`) :

- le certificat et la clé (`tls.cert_file`, `tls.key_file`) sont relus toutes les `tls.reload_interval` secondes (10 par défaut) ; un contenu changé est chargé et sert dès la poignée de main suivante, sans redémarrage, ce qui suit le renouvellement de cert-manager. Un nouveau contenu illisible (PEM invalide, clé qui ne correspond pas au certificat) est ignoré avec un avertissement au journal, une fois par contenu : l'ancien certificat reste servi. Au démarrage, un certificat illisible fait échouer le processus ;
- une poignée de main TLS non terminée en 10 s, ou des en-têtes HTTP/1 non reçus en 10 s, ferment la connexion ;
- une erreur d'`accept` (par exemple `EMFILE`) suspend l'écoute 50 ms avant de réessayer, journalisée au plus une fois par seconde (comme le sidecar, §9.3) ;
- à l'arrêt (`SIGTERM`), l'écoute cesse, les connexions en cours terminent leur requête (au plus 10 s) puis le processus sort avec le code 0.

- **8444, API agrégée de lecture** (§9.2c), si `aggregated` est configuré.
- **8081, interface du contrôleur**, liée à `127.0.0.1`, joignable par un port-forward vers la réplique active (§5.5.4) : `kubectl port-forward -n mimic-system "$(kubectl -n mimic-system get pod -l mimic.platform.io/leader=true -o name)" 8081`. En lecture, et en écriture pour le cycle de vie des snapshots (CTL-46, §5.6) ; jamais pour le mode ou le trafic, qui restent pilotés par la ressource `MimicWorkload`. Protégée comme le reste par le seul RBAC `pods/portforward`.

Une réplique passive (§5.5.4) répond `503` avec `Retry-After: 2` sur les deux ports, sauf à `/healthz` et `/metrics`.

| Méthode | Chemin | Rôle |
|---------|--------|------|
| GET | `/` | liste des workloads |
| GET | `/ui/{namespace}/{workload}` | page d'un workload |
| GET | `/v1/workloads` | workloads connus, avec pods et état souhaité ; pour chaque pod, son relevé `unmatched` tel qu'annoncé ; pour le workload, `unmatched` : le relevé agrégé des pods sélectionnés connus (totaux additionnés, clés fusionnées en sommant les occurrences, premier et dernier horodatage conservés, au plus 50 clés, de la plus récemment vue à la plus ancienne) |
| GET | `…/mode` | état souhaité courant |
| GET | `…/snapshots`, `…/snapshots/{id}` | snapshots (identifiant, taille, date, échanges et `pinned` quand connus, CTL-43) |
| POST | `…/snapshots` | import (`mimicctl snapshot import`, CTL-46) : contenu JSONL canonicalisé, épinglé par défaut ; `400` si invalide |
| POST | `…/snapshots/{id}/pin` | `{"pinned": bool}` (`mimicctl snapshot pin`/`--unpin`, CTL-46) ; `404` sur un identifiant inconnu |
| POST | `…/snapshots/gc` | purge immédiate (`mimicctl snapshot gc`, CTL-44, CTL-46), hors de la passe de maintenance ; répond les identifiants supprimés |
| GET | `…/operations` | opérations découvertes sur tout le workload |
| GET | `…/openapi/inbound`, `…/openapi/outbound/{authority}` | documents OpenAPI du workload |
| GET | `…/requests?direction=&limit=&format=` | requêtes enregistrées (§5.7) |
| GET | `/metrics` | Prometheus |

Métriques du contrôleur : `mimic_controller_leader` (1 pour la réplique active, 0 pour la passive, §5.5.4), `mimic_controller_takeover_seconds` (durée de la dernière prise de relais : rechargement de l'état persisté jusqu'à la marque posée), `mimic_controller_segments_total{result}` (`ok`, `duplicate`, `error`), `mimic_controller_auth_failures_total{reason}`, `mimic_controller_workloads`, `mimic_controller_pods{workload, state}` (`workload` : `<namespace>/<nom>`, pas `namespace`, que le collecteur pose déjà pour le pod du contrôleur ; `state` : `aligned`, `diverging`, `absent` ; les trois états de chaque workload connu sont exposés, à zéro s'il le faut : un workload sans pod, ou dont les pods ont été oubliés après `pod_forget`, garde ses séries à 0), `mimic_controller_snapshots_total`, `mimic_controller_snapshots_deleted_total` (purgés, CTL-44), `mimic_controller_exchanges` (échanges en mémoire, tous workloads), `mimic_controller_exemplars` (exemples par endpoint gardés, tous workloads), `mimic_controller_exemplars_bytes` (leur taille sérialisée) et `mimic_controller_exemplars_refused_total{reason}` (`count` : candidat sans exemple, borne `max_exemplars_per_workload` ; `bytes` : exemple gardé sans body ou remplacement refusé, bornes en octets, §5.5.3), `mimic_controller_certificate_expiry_timestamp_seconds` (date Unix d'expiration du certificat servi, relue à chaque rechargement à chaud ; 0 sans TLS) et `mimic_build_info` (§9.3).

### 9.2c API agrégée de lecture (`data.mimic.platform.io/v1alpha1`)

Les lectures de `mimicctl` (export, requêtes, rapport, liste et contenu des snapshots) passent par l'API server, comme `kubectl top` passe par metrics-server : une `APIService` `v1alpha1.data.mimic.platform.io` renvoie le groupe vers le Service `mimic-controller`, port `aggregated` (8444), servi par la réplique active.

- **Authentification** : l'API server joint le contrôleur avec son certificat client de front proxy. Le port 8444 exige un certificat client signé par la CA `requestheader-client-ca-file` du ConfigMap `kube-system/extension-apiserver-authentication` (ou `aggregated.client_ca_file`), dont le nom commun figure dans `requestheader-allowed-names` (liste vide : tout nom accepté) ; sinon la poignée de main échoue. L'utilisateur est alors celui des en-têtes `X-Remote-User`, `X-Remote-Group` (répétable) et `X-Remote-Extra-<clé>`, posés par l'API server après sa propre authentification : jamais lus sur une connexion sans ce certificat. La CA est lue au démarrage.
- **Autorisation** : une `SubjectAccessReview` par requête (utilisateur, groupes, extras), groupe `data.mimic.platform.io`, ressource `workloads`, verbe `get` (ou `list` pour une liste), namespace et nom du workload, sous-ressource `operations`, `openapi`, `requests` ou `snapshots`. Refus : `403` avec un `Status` Kubernetes (`reason: Forbidden`) qui nomme l'accès refusé ; `SubjectAccessReview` impossible : `503`. Les décisions sont gardées 10 s, par utilisateur et accès.
- **Routes**, préfixe `/apis/data.mimic.platform.io/v1alpha1`, toutes en `GET` (autre méthode : `405`) et servies par les mêmes fonctions que l'interface (§9.2b), même corps et mêmes codes :

| Chemin | Accès vérifié | Équivalent de l'interface |
|--------|---------------|---------------------------|
| `` (racine) | aucun | `APIResourceList` du groupe (découverte, sonde de disponibilité de l'APIService) |
| `/workloads` | `list workloads` sur tout le cluster | `/v1/workloads` |
| `/namespaces/{ns}/workloads` | `list workloads` dans `{ns}` | `/v1/workloads`, réduit aux workloads de `{ns}` |
| `/namespaces/{ns}/workloads/{wl}/operations` | `get workloads/operations` | `…/operations` |
| `/namespaces/{ns}/workloads/{wl}/openapi/inbound`, `…/openapi/outbound/{authority}` | `get workloads/openapi` | `…/openapi/…` |
| `/namespaces/{ns}/workloads/{wl}/requests?direction=&format=&limit=` | `get workloads/requests` | `…/requests` |
| `/namespaces/{ns}/workloads/{wl}/snapshots`, `…/snapshots/{id}` | `get workloads/snapshots` | `…/snapshots`, `…/snapshots/{id}` |

- Les écritures (`pin`, import, purge des snapshots) restent sur l'interface (port-forward, Role `mimic-cli`), comme le mode reste sur `MimicWorkload`.
- **Configuration** : `aggregated: {listen: 0.0.0.0:8444, client_ca_file?, allowed_names?}` ; absente, pas d'API agrégée. Exige `tls` (le même certificat, dont les noms couvrent `mimic-controller.mimic-system.svc`) ; `client_ca_file` absent, l'authentification `kubernetes` est requise pour lire le ConfigMap. La NetworkPolicy ouvre 8444 à toute source : l'API server n'est pas un pod, et le certificat client est exigé.

### 9.3 Observabilité

Métriques Prometheus sur `/metrics` (scrapables par VictoriaMetrics), actives dans tous les modes :

| Métrique | Type | Labels | Mode |
|----------|------|--------|------|
| `mimic_requests_total` | compteur | `direction`, `mode`, `authority`, `method`, `status` | tous |
| `mimic_request_duration_seconds` | histogramme | `direction`, `mode`, `authority` | tous |
| `mimic_upstream_errors_total` | compteur | `direction`, `authority`, `kind` (`connect`, `connect_timeout`, `request`, §3.5) | record |
| `mimic_mock_responses_total` | compteur | `authority`, `match` (`exact`, `template`, `fuzzy`, `unmatched`, `too_large` : body au-delà de `capture.max_body_bytes`, `413`, §5.3) | mock |
| `mimic_mock_injected_latency_seconds` | histogramme | `authority` | mock |
| `mimic_discovered_operations` | jauge | `direction`, `authority` | tous |
| `mimic_store_exchanges` | jauge | `origin` (`local`, `peer` : toujours 0 sous contrôleur, §5.4) | tous |
| `mimic_store_bytes` | jauge (octets estimés des échanges en mémoire, §5.2) | | tous |
| `mimic_store_disk_bytes` | jauge (octets des fichiers sous `store.dir` : journal, état souhaité, formes de mock gardées, §5.6, §8 ; 0 sans `store.dir`) | | tous |
| `mimic_store_disk_limit_bytes` | jauge (taille de l'`emptyDir` du store, `store.size_limit_bytes` ; 0 si inconnue) | | tous |
| `mimic_capture_dropped_total` | compteur | `reason` (`truncated`, `sample_limit`, `store_budget`, `share_backlog`, `local_backlog`, `inflight_budget`) | tous |
| `mimic_share_sync_total` | compteur | `result` (`ok`, `error`) | tous |
| `mimic_share_peers` | jauge (toujours 0 sous contrôleur, §5.4) | | tous |
| `mimic_mode` | jauge (1 pour le mode courant) | `mode` | tous |
| `mimic_passthrough_connections_total` | compteur (connexions redirigées non HTTP, §3.1 ; `refused` : refermée en `mock` sans rien ouvrir vers la destination, TLS non intercepté ou TCP avec `mock.tcp: refuse`, connexions muettes comprises) | `direction`, `kind` (`tls`, `tcp`), `result` (`relayed`, `refused`, `error`) | tous |
| `mimic_tls_handshake_failures_total` | compteur | `authority`, `reason` | tous |
| `mimic_websocket_connections_total` | compteur (poignées de main WebSocket, §3.4 : `relayed`, `101` de l'amont relayé ; `refused`, sortante refusée en mock par `mock.websocket: refuse`) | `direction`, `result` (`relayed`, `refused`) | tous |
| `mimic_mock_snapshot_exchanges` | jauge (0 sans snapshot) | | mock |
| `mimic_mock_engine_builds_total` | compteur | `trigger` (`switch`, `refresh`, `unmatched`, §5.3) | mock |
| `mimic_connections_refused_total` | compteur (connexions refermées au plafond, §3.5) | `listener` (`inbound`, `outbound`, `admin`) | tous |
| `mimic_heartbeats_total` | compteur (battements vers le contrôleur, §5.5 : `ok` quand il a répondu un état souhaité, `error` sinon : réseau, délai, code autre que `200`, corps illisible) | `result` (`ok`, `error`) | sous contrôleur |
| `mimic_heartbeat_last_success_timestamp_seconds` | jauge (date Unix du dernier battement `ok`, 0 avant le premier) | | sous contrôleur |
| `mimic_startup` | jauge (1 pour l'état courant de la barrière de démarrage, §5.5.5) | `state` (`waiting`, `aligned`, `last-known`, `fallback`) | sous contrôleur |
| `mimic_build_info` | jauge (toujours 1) | `mimic_version` (version du binaire), `revision` (commit de la construction, `unknown` hors image) | tous |

`authority` est borné : au-delà de 100 valeurs distinctes, les suivantes sont regroupées sous `other` pour protéger la cardinalité. `method` aussi : il ne prend que les neuf méthodes standard (`GET`, `POST`, `PUT`, `DELETE`, `PATCH`, `HEAD`, `OPTIONS`, `CONNECT`, `TRACE`), toute autre méthode est comptée sous `other`, si bien qu'un client qui invente des méthodes ne crée aucune série.

**Coût sur le chemin des requêtes** : compter une requête n'alloue rien et ne touche aucun compteur de références partagé entre les requêtes. Les labels fixes (`direction`, `mode`, `match`, `kind`, `reason`, méthodes standard) sont des chaînes statiques et le code est un entier ; une authority est internée à sa première apparition (au plus 100, la mémoire reste bornée), puis résolue sous un verrou pris en lecture. Les jauges du store (`mimic_store_exchanges`, `mimic_store_bytes`, `mimic_store_disk_bytes`) sont calculées à la lecture de `/metrics`, et non à chaque échange enregistré ; la taille du disque est la somme des fichiers de `store.dir`, quelques dizaines au plus (segments bornés par `store.max_segments`, formes de mock par `store.max_cached_snapshots`).

**`mimic_build_info`** est exposée par le sidecar, le contrôleur et le webhook, avec la même valeur de `mimic_version` (version du workspace) et de `revision` (variable `MIMIC_REVISION` à la compilation, posée par l'image ; `unknown` sinon). Le label s'appelle `mimic_version` et non `version`, que porte déjà chaque série du sidecar (version de l'application, ci-dessous). Elle sert à repérer, après une mise à jour, les pods qui tournent encore une ancienne version du sidecar (il ne change qu'au redémarrage du pod).

**Label `version`** : quand la version du pod est connue, toutes les métriques du sidecar portent le label constant `version`, pour comparer deux versions d'un workload (p99, débit, codes ; CPU et mémoire du kubelet par jointure sur `pod`, §5.8). La valeur vient, dans l'ordre, de `MIMIC_VERSION`, de `version` dans la configuration, puis de `MIMIC_POD_TEMPLATE_HASH` ; l'injecteur pose les deux variables depuis les labels du pod (§9.4). Valeur vide : pas de label. Elle est fixée au démarrage et tronquée à 63 caractères (taille maximale d'un label Kubernetes) : un pod n'a qu'une valeur, le label ne multiplie donc aucune série d'un pod, et le nombre de valeurs suit celui des versions déployées, pas le trafic.

Logs JSON structurés (`tracing`), filtrés par `MIMIC_LOG` pour le sidecar et le contrôleur, par `RUST_LOG` pour le webhook (syntaxe `EnvFilter` de `tracing-subscriber`, `info` par défaut ou si la valeur est invalide) ; pour le sidecar, le webhook pose `MIMIC_LOG` (§9.4). Propagation de `traceparent` inchangée.

Sous charge, les logs d'erreurs répétitives sont limités en débit : au plus une ligne par seconde pour les échecs vers l'amont, pour les connexions refusées en mock (§3.1, avec la destination d'origine), pour les requêtes sans réponse du mock et pour les bodies trop gros du mock (§5.3, une limite par raison) et pour les erreurs d'`accept`, et une toutes les 30 s pour les battements vers le contrôleur en échec (§5.5.5), avec le nombre de lignes omises depuis la précédente. Les compteurs restent exacts (`mimic_upstream_errors_total`, `mimic_passthrough_connections_total`, `mimic_mock_responses_total`), comme le relevé `/api/unmatched`. Une erreur d'`accept` (par exemple `EMFILE`, plus de descripteurs) suspend l'écoute concernée pendant 50 ms avant de réessayer, au lieu de boucler à 100 % de CPU au moment où l'on mesure le pod.

### 9.4 Injection automatique (`mimic-injector`)

Webhook d'admission mutant (`MutatingWebhookConfiguration`), déployé dans `mimic-system`, qui ajoute à la création d'un pod ce que `deploy/example/deployment.yaml` écrit à la main.

- **Portée** : les pods des namespaces labellisés `mimic.platform.io/enabled=true`, le label qui leur fait déjà recevoir la CA du contrôleur, sauf `kube-system` et `mimic-system`, exclus par leur nom (`kubernetes.io/metadata.name`) même s'ils portent le label. Un pod s'en retire par l'annotation `mimic.platform.io/inject: "false"`.
- **Pod Security** : `mimic-init` tourne en root avec `NET_ADMIN` et `NET_RAW`, ce que seul le niveau `privileged` de Pod Security Admission accepte (comme `istio-init`) : le namespace doit porter `pod-security.kubernetes.io/enforce: privileged`, ou être exempté. Dans un namespace `baseline` ou `restricted`, le pod muté est refusé par l'API : le workload ne démarre pas.
- **Fonction pure, sans appel à l'API** : la mutation ne dépend que du pod et de la configuration du webhook (`mimic-injector::inject`), et se teste sans cluster. `sideEffects: None` ; `failurePolicy: Ignore`, si bien qu'un webhook indisponible laisse créer le pod sans Mimic au lieu de le bloquer ; `timeoutSeconds: 5`.
- **Ce qui est ajouté** (patch JSON) :
  - l'init container `mimic-init` (root, seules capacités `NET_ADMIN` et `NET_RAW`, `allowPrivilegeEscalation: false`, `seccompProfile: RuntimeDefault`, ressources `init_resources` : par défaut `10m` et `16Mi` demandés, `64Mi` au plus), en tête, ou juste après ceux du mesh si le pod en a déjà (`istio-init`, `istio-validation`, `istio-proxy`, `linkerd-init`, `linkerd-network-validator`, `linkerd-proxy`), pour que les règles du mesh soient posées avant les siennes (§3.3.3). `MIMIC_MESH` : annotation `mimic.platform.io/mesh`, sinon `istio` ou `linkerd` si leurs conteneurs sont là, sinon `auto` ; `POD_IP` par l'API downward (§3.3.2). Ports entrants : annotation `mimic.platform.io/inbound-ports`, sinon les `containerPort` TCP des conteneurs de l'app, sinon `*`. CIDR sortants exclus : l'IP du Service `kubernetes` (API Kubernetes), lue dans `KUBERNETES_SERVICE_HOST` au démarrage du webhook (`/32` ou `/128`), puis ceux de la configuration du webhook (`exclude_outbound_cidrs`), puis l'annotation `mimic.platform.io/exclude-outbound-cidrs`, sans doublon. Ports sortants exclus : annotation `mimic.platform.io/exclude-outbound-ports`. Avec `ipv6: true` dans la configuration du webhook, `MIMIC_IPV6=true` : `mimic-init` pose aussi les règles `ip6tables` (cluster double pile) ;
  - **validation des annotations** reprises dans les règles iptables (`inbound-ports`, `exclude-outbound-cidrs`, `exclude-outbound-ports`) : chaque élément est analysé comme le fait `mimic-init` (§3.1) et réécrit sous sa forme canonique ; un élément invalide est ignoré, avec un avertissement d'admission qui nomme l'annotation et la valeur, et n'atteint jamais le pod. `inbound-ports` vaut `*` (tout), `-` (aucun) ou une liste de ports ; si aucun port de la liste n'est valide, l'annotation est ignorée (ports déclarés). Les CIDR de `exclude_outbound_cidrs` sont validés au démarrage du webhook ;
  - le native sidecar `mimic`, juste après `mimic-init` et donc avant les init containers de l'app, sous l'UID et le GID 1338, `seccompProfile: RuntimeDefault`, avec une `startupProbe` sur `/readyz` (période 1 s, `share.startup.timeout` + 30 échecs, §5.5.5) : l'app ne démarre qu'une fois le proxy prêt et, sous contrôleur, l'état souhaité connu ; `readinessProbe` sur `/readyz` et `livenessProbe` sur `/healthz` (période 10 s, 3 échecs). Sondes HTTP sur le port `mimic-admin` (15100), ou sondes `exec` (`mimic-proxy --probe <chemin>`) quand un conteneur du mesh est présent (§3.3.4). Variables `POD_NAME`, `POD_NAMESPACE`, `MIMIC_WORKLOAD` (valeur figée à l'admission), `MIMIC_REDACT_SALT` lue dans le Secret `<workload>-mimic`, clé `redact-salt`, facultatif, et, par l'API downward, `MIMIC_VERSION` (label `version_label` du pod, `app.kubernetes.io/version` par défaut) et `MIMIC_POD_TEMPLATE_HASH` (label `pod-template-hash`), vides si le label manque (label `version` des métriques, §9.3), `MIMIC_STORE_SIZE_LIMIT_BYTES` (taille de l'`emptyDir` du store en octets, `mimic_store_disk_limit_bytes`) et `MIMIC_LOG` : annotation `mimic.platform.io/log`, sinon `sidecar_log` de la configuration du webhook, sinon absente (`info`). Une annotation que `EnvFilter` refuse est ignorée avec un avertissement d'admission, comme une valeur iptables invalide ; un `sidecar_log` invalide empêche le webhook de démarrer ;
  - la configuration du sidecar : par défaut celle du webhook (`sidecar_config`), passée en ligne par `MIMIC_CONFIG_YAML` ; l'annotation `mimic.platform.io/config: <configmap>` monte ce ConfigMap (clé `mimic.yaml`) à la place ;
  - les volumes : store (`emptyDir` de 512 Mi), jeton projeté d'audience `mimic-controller`, CA du contrôleur (ConfigMap `mimic-controller-ca`, `optional: true` : le pod est créé même si trust-manager ne l'a pas encore publié, au lieu d'attendre le volume ; le sidecar, qui exige la CA, est relancé par le kubelet jusqu'à ce que le ConfigMap apparaisse dans le volume), répertoire de CA Mimic (`emptyDir`, `/var/run/mimic/ca`) ;
  - si l'annotation `mimic.platform.io/tls-intercept` liste des hôtes (séparés par des virgules) : `MIMIC_TLS_INTERCEPT` pour le sidecar, et dans chaque conteneur de l'app le répertoire de CA monté en lecture seule avec `SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE` et `CURL_CA_BUNDLE` (§4.2). Une variable déjà définie par l'app n'est pas écrasée ;
  - l'annotation `mimic.platform.io/status: injected` et le label `mimic.platform.io/sidecar: "true"` (NetworkPolicy du contrôleur).
- **Nom du workload** : annotation `mimic.platform.io/workload`, sinon label `app.kubernetes.io/name`, sinon label `app` (§5.4).
- **Pods laissés tels quels**, admis avec un avertissement d'admission visible par `kubectl` : pod déjà injecté (idempotence), `hostNetwork: true`, aucun nom de workload trouvé.
- **Réinvocation** (`reinvocationPolicy: IfNeeded`) : les webhooks sont appelés dans l'ordre des noms de leurs configurations, si bien que ceux d'Istio et de Linkerd passent d'habitude avant `mimic-injector`. Si le mesh injecte après Mimic, le webhook est rappelé et adapte le pod déjà injecté : `mimic-init` et `mimic` repassent après les conteneurs du mesh, `MIMIC_MESH` et les sondes suivent la règle ci-dessus. Un pod déjà adapté ne reçoit aucun patch.
- Le webhook est servi en HTTPS (certificat cert-manager relu à chaud, `caBundle` posé par le cainjector ; serveur décrit au §9.2b). Métriques `mimic_injector_admissions_total{result}` (`injected`, `skipped`, `error`), `mimic_injector_certificate_expiry_timestamp_seconds` (comme celle du contrôleur, §9.2b) et `mimic_build_info` (§9.3). À `SIGTERM`, il termine les admissions en cours puis sort.
- **Déploiement** (`deploy/injector/injector.yaml`) : deux réplicas sous l'UID et le GID 1338 (jamais 1337, l'UID d'istio-proxy), anti-affinité préférentielle par nœud, `priorityClassName: mimic-control-plane` (PriorityClass fournie, comme pour le contrôleur), image `ghcr.io/thaliph/mimic:0.1.0`.

Côté sidecar, `MIMIC_CONFIG_YAML` contient le YAML de la configuration quand aucun fichier n'est donné (`--config` ou `MIMIC_CONFIG` l'emportent) ; `MIMIC_TLS_INTERCEPT` remplace `tls.intercept`, `MIMIC_STORE_SIZE_LIMIT_BYTES` remplace `store.size_limit_bytes`.

Le webhook ne pose ni `mock.tcp` ni `MIMIC_INTERCEPT_LOOPBACK` : leurs défauts (refus du TCP en mock, trafic local non intercepté) sont ceux d'un pod Kubernetes. `mock.tcp: relay` se règle dans `sidecar_config` ou le ConfigMap de `mimic.platform.io/config` ; une dépendance TCP à garder réelle s'exclut par `mimic.platform.io/exclude-outbound-ports` ou `exclude-outbound-cidrs` (§3.4).

Configuration du webhook (`mimic-injector --config`) :

```yaml
listen: 0.0.0.0:8443
tls: { cert_file: /etc/mimic/tls/tls.crt, key_file: /etc/mimic/tls/tls.key, reload_interval: 10 }
image: ghcr.io/thaliph/mimic:0.1.0
exclude_outbound_cidrs: []               # en plus de l'IP du Service kubernetes, ajoutée d'office
ipv6: false                              # true : MIMIC_IPV6=true, règles ip6tables aussi
init_resources:                          # ressources de mimic-init
  requests: { cpu: 10m, memory: 16Mi }
  limits: { memory: 64Mi }
sidecar_config: |                       # configuration par défaut des sidecars (§9.1)
  mode: record
  share: { url: https://mimic-controller.mimic-system.svc:8443, ... }
resources:
  requests: { cpu: 50m, memory: 64Mi }
  limits: { memory: 256Mi }
version_label: app.kubernetes.io/version   # label du pod repris dans MIMIC_VERSION
```

### 9.5 CLI `mimicctl`

Outil des humains. Il parle à l'API Kubernetes avec le kubeconfig courant et n'a que les droits de son utilisateur (RBAC, §7.1).

| Commande | Effet |
|----------|-------|
| `mimicctl mode <workload> <mode> [--snapshot id] [--selector k=v,…] [--capture-inbound full\|metadata\|off] [--mock-websocket relay\|refuse] [--force] [-n ns]` | crée ou met à jour la `MimicWorkload` (server-side apply, gestionnaire `mimicctl`), après avoir vérifié que le workload existe |
| `mimicctl status [<workload>] [-n ns \| -A]` | mode souhaité, snapshot servi, requêtes sans réponse du mock, pods alignés |
| `mimicctl export <workload> (--inbound \| --outbound <authority>) [--openapi-version 3.1\|3.0] [-o fichier]` | document OpenAPI depuis l'interface du contrôleur |
| `mimicctl requests <workload> [--direction inbound\|outbound] [--format json\|jsonl\|k6] [--limit n]` | requêtes enregistrées (§5.7), entrantes par défaut ; `outbound` : les appels de l'application à ses dépendances, tels que le mock les rejouera |
| `mimicctl diff <ancien> <nouveau> [--json] [--fail-on-breaking]` | différence entre deux documents OpenAPI (YAML ou JSON) |
| `mimicctl report [<workload>] [-n ns \| -A] [--top n]` | rapport Markdown de ce que les sidecars ont découvert (ci-dessous) |
| `mimicctl load [<workload>] --target URL [--from fichier] [--concurrency C (--requests N \| --duration D) \| --ramp …] [-n ns]` | tir de charge par rejeu des requêtes enregistrées, palier fixe ou montée jusqu'à saturation, avec recommandation de CPU et de mémoire (§5.9) |
| `mimicctl snapshot ls <workload> [-n ns]` | liste les snapshots du workload : identifiant, date, échanges, référencé, épinglé (§5.6) |
| `mimicctl snapshot pin <workload> <id> [--unpin] [-n ns]` | épingle ou désépingle un snapshot, le protégeant ou non de la purge (CTL-46) |
| `mimicctl snapshot export <workload> <id> [-o fichier] [-n ns]` | contenu JSONL complet d'un snapshot |
| `mimicctl snapshot import <workload> <fichier> [-n ns]` | importe un snapshot exporté ailleurs, épinglé par défaut (CTL-46) |
| `mimicctl snapshot gc <workload> [-n ns]` | purge immédiate, hors référencés et épinglés, hors des `keep_last` plus récents (CTL-44, CTL-46) |

- `mode` : `--selector` prend des égalités `clé=valeur` séparées par des virgules (`spec.selector.matchLabels`) ; `--capture-inbound` renseigne `spec.capture.inbound` ; `--mock-websocket` renseigne `spec.mock.websocket` (§3.4). Une valeur invalide est refusée avant tout appel. Un apply sans ces options les retire (server-side apply), donc `mimicctl mode checkout record` remet tout le workload dans le même état. Avant d'appliquer `mock` ou `mock-partial`, `mimicctl` lit les labels du namespace : s'il porte `env=production`, il refuse avec un message qui nomme le namespace et le label, et le code de sortie 2, sans rien appliquer. S'il ne peut pas lire le namespace (droit `get` absent), il le signale et applique : le contrôleur refuse de toute façon (§5.5.1). **Existence du workload** : sans `MimicWorkload` de ce nom dans le namespace, `mimicctl` cherche un pod injecté du workload (label `mimic.platform.io/sidecar: "true"`, variable `MIMIC_WORKLOAD` du sidecar égale au nom) ; s'il n'y en a aucun, il refuse avec le code de sortie 2 et liste les workloads injectés du namespace (ou dit qu'il n'y en a aucun), sans rien appliquer : une faute de frappe ne crée plus une `MimicWorkload` qui ne pilote rien. `--force` applique quand même (workload pas encore déployé, ou à zéro réplica). Une `MimicWorkload` existante est toujours mise à jour. Pods illisibles (droit `list` absent) : signalé, puis appliqué.
- `status` : `ALIGNED` ne compte que les pods sélectionnés (§5.5.1) ; un mode refusé par le garde-fou est affiché suivi de `(refusé)`. `UNMATCHED` est `status.unmatched` (§5.5.1), 0 si le contrôleur ne le publie pas.
- `status` n'affiche le snapshot et les pods alignés que si le contrôleur a publié le statut de la génération courante (`status.observedGeneration` égal à `metadata.generation`) ; sinon ces colonnes valent `?` (par exemple `?/2`), car un statut antérieur décrit l'état souhaité d'avant la bascule.
- `export --outbound` prend une authority `hôte:port`, comme les clés de la découverte (`api.stripe.com:443`). Un hôte seul est accepté s'il ne correspond qu'à une authority connue du workload (lue dans `…/operations`) ; sinon, avant tout téléchargement du document, `mimicctl` répond par une erreur (code 2) qui liste les authorities connues : aucune ne correspond, ou plusieurs ports pour le même hôte.
- **Lectures** (`export`, `requests`, `report`, `snapshot ls`, `snapshot export`) : par l'API agrégée (§9.2c), avec les droits du kubeconfig dans le namespace du workload (`report -A` : `list` sur tout le cluster). Si le groupe `data.mimic.platform.io` n'est pas servi (`404` de l'API server : APIService absente), `mimicctl` se replie sur le port-forward ; un `403` est une vraie réponse, jamais suivie d'un repli. **Écritures** (`snapshot pin`, `import`, `gc`) et repli : un port-forward vers le pod du contrôleur (port 8081) : la réplique active, marquée `mimic.platform.io/leader: "true"` (§5.5.4), sinon le premier pod en marche (contrôleur d'une version antérieure, sans marque), comme `kubectl port-forward` : même RBAC (`pods/portforward` dans `mimic-system`), aucun port exposé.
- `--openapi-version 3.0` convertit le document 3.1 (`mimic-core::openapi::to_3_0`) : `type: [T, "null"]` devient `type: T` avec `nullable: true`, les `examples` d'un schéma deviennent `example` (le premier), `openapi` vaut `3.0.3`.
- **Rapport** : pour chaque workload connu du contrôleur (filtré par nom ou namespace, ou tous avec `-A`), son mode et ses pods, la liste des API découvertes (le service, puis chaque dépendance par authority) avec leur nombre d'opérations et d'appels, puis les `n` opérations les plus appelées (10 par défaut) avec leurs codes, leur latence p50 et p99 et les headers d'une requête enregistrée de cette opération (même sens, même dépendance, même méthode, chemin conforme au template). Les headers sont ceux du store, donc déjà redactés (§7) ; une valeur est tronquée à 80 caractères. Sans requête enregistrée, les headers déclarés comme paramètres de l'opération sont listés. Tri par appels décroissants, puis sens, API, chemin et méthode : deux rapports sur les mêmes données sont identiques. Le tableau récapitulatif porte aussi, par workload, le total des requêtes sans réponse du mock (§5.3) ; une section « Requêtes sans réponse du mock » liste ensuite les clés du relevé agrégé (`unmatched` de `/v1/workloads`) : méthode, API, forme du chemin, raison (`599` ou `413`), occurrences et dernière occurrence (UTC), dans l'ordre du relevé. Sans requête sans réponse, la section le dit ; un contrôleur d'une version antérieure, qui ne publie pas le relevé, donne 0. Le Markdown est fait pour le résumé d'un job GitHub Actions (`>> $GITHUB_STEP_SUMMARY`).
- **Diff** (`mimic-core::diff`, fonction pure) : opérations ajoutées et supprimées, puis pour chaque opération commune les paramètres ajoutés, supprimés ou devenus requis, et les codes de réponse ajoutés ou supprimés. Sont **cassants** : une opération supprimée, un paramètre devenu requis, un paramètre requis ajouté, un code 2xx supprimé. Sortie texte triée, `--json` pour une sortie machine. Avec `--fail-on-breaking`, le code de sortie vaut 1 s'il y a un changement cassant, ce qui sert en CI à comparer la découverte d'une semaine à l'autre.

---

### 9.6 Supervision livrée (`deploy/monitoring/`)

- `rules.yaml` : règles d'alerte au format Prometheus (`groups:`), lisibles par `promtool`, par vmalert et par la règle `PrometheusRule` ;
- `prometheus-rule.yaml` : la `PrometheusRule` (`monitoring.coreos.com/v1`) de `mimic-system`, dont `spec.groups` est exactement `rules.yaml` (VictoriaMetrics operator la convertit en `VMRule`) ;
- `tests.yaml` : tests unitaires `promtool test rules` des alertes dont l'expression a un piège (filtre de mode, division par une limite nulle, cumul par workload, absence de série) ;
- `scripts/check-monitoring.sh` lance `promtool check rules` et `promtool test rules`, en CI.

Les règles n'utilisent que les métriques de Mimic et celles, standard, de kube-state-metrics, cAdvisor et du kubelet. `docs/guide/Alertes-et-supervision.md` ne recopie plus les règles : il renvoie à ces fichiers et garde, pour chaque alerte, sa ligne de conduite à tenir. Alertes ajoutées en v0.22 :

- **expiration de certificat** : certificat servi par le contrôleur ou le webhook à moins de 7 jours de son expiration, alors que cert-manager le renouvelle 15 jours avant (`renewBefore: 360h`) : renouvellement en échec, ou nouveau certificat refusé par le rechargement à chaud ;
- **disque du store** : `mimic_store_disk_bytes` au-delà de 80 % de `mimic_store_disk_limit_bytes` (seulement si la limite est connue) : au-delà de la taille de l'`emptyDir`, le kubelet évince le pod ;
- **éviction** : pod injecté (`kube_pod_init_container_info{container="mimic"}`) évincé ;
- **versions mêlées** : plus d'une `mimic_version` exposée pendant 24 h (pods jamais redémarrés depuis une mise à jour) ;
- **pods qui divergent**, désormais par workload.

## 10. Stack Rust

| Besoin | Crate |
|--------|-------|
| Runtime | `tokio` |
| HTTP/1 + HTTP/2 | `hyper` 1.x, `hyper-util` |
| TLS | `rustls`, `tokio-rustls`, `rustls-native-certs` |
| Génération de certificats | `rcgen` |
| Sérialisation | `serde`, `serde_json`, `serde_yaml` |
| Histogrammes | `hdrhistogram` |
| Métriques | `prometheus-client` |
| Logs | `tracing`, `tracing-subscriber` |
| iptables / sockopt | `nix` (`SO_ORIGINAL_DST`), appel à `iptables-restore` |
| K8s (injector, CLI) | `kube`, `k8s-openapi` |
| S3 | `object_store` |
| CLI | `clap` |
| Tests | `proptest`, `insta`, `wiremock`, `criterion`, `cargo-fuzz` |

Workspace Cargo :

```
mimic/
├── Cargo.toml              # workspace
├── crates/
│   ├── mimic-core/         # modèle, infer, matcher, redact, latency (pur, sans tokio)
│   │   └── tests/          # tests d'acceptation INF-*, RED-*, fixtures, snapshots
│   ├── mimic-proxy/        # adaptateur réseau, store, admin API
│   ├── mimic-init/         # règles iptables
│   ├── mimic-injector/     # webhook d'injection
│   └── mimicctl/           # CLI
├── tests/e2e/              # scénarios kind (phase P4)
├── deploy/                 # manifests, chart Helm du webhook
└── docs/SPEC.md            # ce document
```

---

## 11. Stratégie de tests

Les tests sont écrits **avant** l'implémentation de chaque phase. Chaque critère d'acceptation porte un identifiant repris dans le nom du test (`#[test] fn infer_t01_numeric_segment_becomes_param()`), ce qui donne une traçabilité spec ↔ test et permet à Claude Code de travailler en TDD phase par phase.

### 11.1 Pyramide

| Niveau | Cible | Outils | Exécution |
|--------|-------|--------|-----------|
| Unitaire | `mimic-core` : templating, schémas, redaction, matching, latence | `cargo test`, `proptest`, `insta` | chaque commit, < 30 s |
| Golden | corpus d'échanges → OpenAPI attendue | `insta` (snapshots YAML) | chaque commit |
| Intégration | proxy réel en process contre des upstreams factices | `tokio::test`, `wiremock`, `axum` | chaque commit |
| Conformité | validité des specs générées | parse `openapiv3` + `vacuum lint` en CI | chaque commit |
| Documentation | guide ↔ code et manifests : métriques, image, UID, ressources, port de collecte, alertes, options de `mimicctl` | `cargo test` (`include_str!` sur `docs/` et `deploy/`) | chaque commit |
| E2E | cluster kind, iptables réels, NetworkPolicy | `kind`, script Rust ou `bats` | PR + nightly |
| Performance | surcoût du proxy | `criterion` (micro), `k6`/`oha` (macro) | nightly, seuils bloquants |
| Fuzzing | parsing HTTP, matcher, inférence de schéma | `cargo-fuzz` | nightly |

### 11.2 Critères d'acceptation (extraits, à compléter par phase)

**Inférence (INF)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| INF-01 | 10 `GET /users/{n}` avec n entier distinct | inférence | un seul chemin `/users/{userId}`, paramètre `integer` |
| INF-02 | 50 `GET /users/{n}` + 20 `GET /users/me` | inférence | deux chemins : `/users/me` et `/users/{userId}` |
| INF-03 | 3 `GET /items/{n}` | inférence | trois chemins littéraux (sous le seuil de cardinalité) |
| INF-04 | bodies où `email` est toujours présent, `phone` dans 50 % | inférence | `required: [email]`, `phone` optionnel |
| INF-05 | champ valant tantôt `"x"`, tantôt `null` | inférence | `type: [string, "null"]` |
| INF-06 | champ `status` avec 3 valeurs sur 100 observations | inférence | `enum` de 3 valeurs |
| INF-06b | champ `status` toujours `paid` sur 10 observations | inférence | pas d'`enum` : l'échantillon ne suffit pas |
| INF-07 | n'importe quel corpus (proptest) | inférence | la spec produite est un OpenAPI 3.1 valide |
| INF-08 | même corpus, ordre des échanges permuté (proptest) | inférence | spec identique (déterminisme) |
| INF-09 | champs texte de réponse sur 60 appels : `name` ∈ `user0`…`user6` ; `status` à 3 valeurs sur 20 observations, chacune vue au moins deux fois ; `kind` à 3 valeurs dont une vue une seule fois ; `tier` à 2 valeurs sur 19 observations ; `plan` toujours `basic` ; puis les mêmes `tier` avec `min_observations: 10` | inférence | `enum` seulement pour `status` (et `tier` avec le seuil abaissé) ; `name`, `kind`, `tier` et `plan` restent `type: string` sans `enum` |

**Redaction (RED)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| RED-01 | header `Authorization: Bearer abc` | capture | valeur stockée `<redacted:sha256:…>`, jamais `abc` |
| RED-01b | headers `Cookie`, `X-Api-Key`, `X-Session-Id`, `Set-Cookie` et `Accept` | capture | les quatre premiers redactés, `Accept` intact |
| RED-02 | deux requêtes avec le même token | capture | même marqueur de redaction |
| RED-03 | body avec un numéro de carte valide (Luhn) | capture | remplacé ; un numéro invalide de même longueur ne l'est pas |
| RED-03b | body avec une adresse e-mail, un JWT, un IBAN, dans un objet et dans une liste | capture | remplacés, les autres valeurs intactes, chaque marque située (`response.body$.contact`) |
| RED-04 | n'importe quel body contenant une valeur secrète injectée (proptest) | capture puis lecture du store brut | la valeur n'apparaît dans aucun octet du store |
| RED-05 | body JSON sans valeur à redacter, aux clés non triées, avec espaces et `1.0` ; même body avec une adresse e-mail | capture, puis rejeu en mock | le premier est stocké et rejoué octet pour octet ; le second est réécrit avec sa marque |
| RED-06 | aucun sel (ni `redact.salt`, ni `MIMIC_REDACT_SALT`) ; puis un sel fourni | démarrage du sidecar | un avertissement dans les logs explique que les marqueurs changent d'un pod et d'un redémarrage à l'autre et nomme `MIMIC_REDACT_SALT` ; avec un sel, aucun avertissement |
| RED-07 | chemins avec un entier, un UUID, un ULID, un hexadécimal, une date, une adresse e-mail (en clair et `%40`), un JWT, un numéro de carte, un marqueur de redaction, un segment de 40 caractères, un jeton de 32 caractères sans chiffre mêlant majuscules et minuscules, des segments littéraux (`/v1/customers/me`), un chemin de 600 caractères ; n'importe quel chemin portant une valeur sensible injectée (proptest) | forme affichée (`path_template`) | `{int}`, `{uuid}`, `{id}`, `{redacted}`, `{opaque}` ; littéraux gardés ; racine `/` ; forme d'au plus 256 caractères terminée par `/…` ; la valeur injectée n'apparaît jamais dans la forme |

**Matching et mock (MCK)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| MCK-01 | enregistrement `GET api.x.com/v1/a → 200 {…}` | même requête en mode mock | même code, headers et body, octet pour octet (hors hop-by-hop) |
| MCK-02 | enregistrement sur `/v1/c/1` | requête `/v1/c/2` | réponse issue du template, `x-mimic-match: template` |
| MCK-03 | aucun enregistrement pour l'hôte | requête | `599`, `x-mimic-unmatched: true`, compteur incrémenté |
| MCK-04 | latences enregistrées p50 = 100 ms | 1 000 requêtes mockées | p50 rejoué entre 90 et 110 ms |
| MCK-05 | `mock.latency_factor: 2` | 1 000 requêtes | p50 entre 180 et 220 ms |
| MCK-06 | stratégie `sequence`, enregistrement POST puis GET | rejeu | réponses servies dans l'ordre d'origine |
| MCK-06b | stratégie `round-robin`, deux enregistrements équivalents | trois requêtes | 0, 1, 0 : rotation entre les enregistrements |
| MCK-07 | distribution 98 % 200 / 2 % 429, `weighted-status` | 10 000 requêtes | 429 entre 1,5 % et 2,5 % |
| MCK-08 | réponse enregistrée gzip, br ou zstd | rejeu | `Content-Encoding` conservé, body identique une fois décodé |

**Moteur de mock, niveau `mimic-core` (MCK, suite)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| MCK-09 | deux enregistrements de même chemin, query `?page=1` et `?page=2` | requête `?page=2` | l'enregistrement `page=2` est choisi |
| MCK-09b | deux enregistrements POST de même chemin, bodies JSON `{"sku":"A"}` et `{"sku":"B"}` | requête au body `{"sku":"A"}` | l'enregistrement `A` est choisi |
| MCK-10 | enregistrement sur une autre méthode ou une autre authority | requête | aucune correspondance |
| MCK-11 | même graine, mêmes requêtes | deux exécutions | décisions et latences identiques |
| MCK-12 | trois exemplaires du même appel, deux en `200 {a}` et un en `500`, le `500` le plus récent | stratégie `best` | `200 {a}` |

**Mock concurrent et fidèle (MCK, suite, §5.3)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| MCK-13 | 3 enregistrements rejouables de `GET /users/{id}`, 10 000 sans body de la même opération et 5 000 sur d'autres chemins de la même authority | requête `GET /users/9` | seuls les 3 rejouables sont comparés à la requête ; la latence est tirée parmi les 10 003 durées de l'opération |
| MCK-14 | un moteur partagé par 8 threads, même graine qu'une exécution en série | 8 000 requêtes concurrentes, stratégies `best`, `weighted-status` et `round-robin` | `best` : mêmes réponses qu'en série ; `weighted-status` : même ensemble de latences tirées qu'en série ; `round-robin` sur 4 candidats : chacun servi 2 000 fois |
| MCK-15 | une dépendance réelle d'environ 3,5 ms et une sans délai, enregistrées ; un sidecar en mock sur ces enregistrements (`mock.latency_factor: 1`), un autre sans latence (`mock.latency_factor: 0`) | appels en série entrelacés via le sidecar en record (dépendance réelle) et via les sidecars en mock, durée vue par l'application | 3,5 ms : p50 rejoué à ±10 % de celui de la dépendance ; sans délai : p50 rejoué au plus 1,5 fois celui de la dépendance ; sans latence : p50 sous 1 ms |
| MCK-16 | store de 2 000 échanges sortants, mode record | bascule en mock, puis 200 requêtes mockées concurrentes | moteur construit une fois, à la bascule : `mimic_mock_engine_builds_total{trigger="switch"}` vaut 1 avant la première requête et aucune requête n'en construit |
| MCK-17 | moteur en service, puis nouvel échange sortant dans le store (import d'un pair) ; mode `mock-partial` | requêtes avec correspondance ; requête sur le nouvel échange ; requête relayée puis la même | les premières servies par le moteur courant, sans construction ; la deuxième servie depuis le nouvel échange, pas `599` ; la troisième mockée dès la seconde fois |
| MCK-18 | enregistrement dont la réponse est en `gzip` | rejeux successifs | le body est encodé une seule fois : les mêmes octets, sans nouvel encodage, à chaque rejeu |
| MCK-19 | poignée de main WebSocket enregistrée (`101`, body vide, durée de connexion de 60 s) et un `200` de 10 ms sur le même chemin | construction du moteur, requête sur ce chemin | le `101` n'est ni compté ni rejouable : le `200` est servi, avec une latence de 10 ms ; seul, le `101` donne une absence de correspondance |
| MCK-20 | échanges sortants quelconques (proptest) : rejouables ou non, chemins de même forme ou de forte cardinalité, codes `1xx`, plusieurs exemplaires d'un même appel | moteur construit sur le snapshot ; moteur construit sur sa forme de mock (§5.6), sérialisée puis relue | pour chaque stratégie et à graine égale, mêmes décisions pour toute requête (échange rejoué, code, score, type de correspondance) et mêmes latences tirées ; même nombre d'enregistrements, de rejouables et d'authorities ; la forme de mock ne contient que les échanges rejouables, en a autant que le moteur, et ses octets ne dépendent que du snapshot |
| MCK-21 | stratégie `round-robin`, puis `sequence` ; deux enregistrements rejouables de `GET /users/1` et deux de `GET /users/2` | 10 000 requêtes sur des identifiants jamais enregistrés, puis 100 requêtes alternées sur `/users/1` et `/users/2` | le moteur tient au plus un curseur par chemin enregistré et un par opération (ici 2 + 1), quel que soit le nombre de chemins demandés ; `round-robin` : les 10 000 sont réparties à parts égales sur les quatre enregistrements, et `/users/1` comme `/users/2` alternent entre leurs deux enregistrements ; `sequence` : les 10 000 parcourent les quatre enregistrements dans l'ordre d'enregistrement |
| MCK-22 | mode `mock-partial`, moteur en service sur 2 000 échanges | 200 requêtes vers 200 opérations inconnues, chacune relayée puis enregistrée, suivie de la même ; requêtes avec correspondance entre elles | aucune construction déclenchée par une requête (`mimic_mock_engine_builds_total{trigger="unmatched"}` à 0) et au plus une reconstruction en tâche de fond par seconde écoulée (plus une) ; chaque requête répétée est mockée (`x-mimic-match`), l'amont n'en reçoit que 200 ; les requêtes avec correspondance sont servies |
| MCK-23 | configuration du sidecar sans `mock.websocket`, puis avec `websocket: refuse`, puis `websocket: drop` | chargement | `relay` par défaut, `refuse` accepté, valeur inconnue refusée au démarrage avec un message explicite |
| MCK-24 | configuration du sidecar sans `mock.tcp`, puis avec `tcp: relay`, puis `tcp: drop` | chargement | `refuse` par défaut, `relay` accepté, valeur inconnue refusée au démarrage avec un message qui nomme le champ, la valeur et les deux valeurs admises |
| MCK-25 | relevé des requêtes sans réponse (`mimic-core::unmatched`), capacité 3 | `GET /users/1` puis `GET /users/2` (même forme), `POST /orders`, `413` sur `POST /upload`, puis deux clés nouvelles ; mêmes requêtes dans un autre ordre aux mêmes horodatages ; rapport à 2 clés | une clé `GET /users/{int}` à 2 occurrences, premier et dernier horodatage ; `reason` `unmatched` ou `too_large` ; au-delà de 3 clés, la moins récemment vue est évincée ; `total` à 6, évictions comprises ; tri par dernière occurrence décroissante puis par clé ; rapport identique quel que soit l'ordre d'arrivée à horodatages égaux ; `limit` respecté ; remise à zéro : `total` 0, aucune clé |
| MCK-26 | mode `mock`, `unmatched: error`, `capture.max_body_bytes: 1024`, un enregistrement `GET /users/{id}` vers la dépendance ; journal capturé | `GET /orders/42?email=a@b.io` avec `traceparent`, puis `POST /echo` de 2 KiB, puis 50 `GET /orders/{n}` dans la même seconde, puis un dernier une seconde plus tard ; un `GET /users/7` | `599` avec `x-mimic-unmatched`, puis `413` ; `mimic_mock_responses_total{match="unmatched"}` et `{match="too_large"}` comptés, `mimic_requests_total` avec `status="599"` et `status="413"` ; `/api/unmatched` liste `GET /orders/{int}` (52 occurrences) et `POST /echo` (`too_large`) ; une ligne `warn` avec `method`, `authority`, `path=/orders/{int}` et le `trace_id`, une ligne pour le `413`, aucune pour les 50 suivantes, puis la dernière avec `omitted=50` ; ni `42`, ni l'e-mail, ni la query dans le relevé ni dans le journal ; la requête avec correspondance n'est pas relevée |
| MCK-27 | sidecar en `mock` avec des requêtes sans réponse relevées ; sidecar piloté par un contrôleur factice | passage en `record`, puis retour en `mock` ; chargement d'un autre snapshot ; battement ; `mock-partial` sans correspondance | le relevé reste lisible en `record` ; il est vide au retour en `mock` et après le nouveau snapshot ; le battement porte `unmatched` (`total`, au plus 20 clés) ; en `mock-partial`, la requête relayée n'est pas relevée |

**Snapshots (SNP)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| SNP-01 | un ensemble d'échanges, dans n'importe quel ordre, avec doublons (proptest) | construction | même identifiant et même contenu |
| SNP-02 | plus de `max_per_key` échanges sur un même couple | construction | les `max_per_key` plus récents sont gardés |
| SNP-03 | deux sidecars ; A a vu `GET /users/7` de la dépendance, B ne l'a jamais vu | bascule en mock par le contrôleur | A et B renvoient la même réponse, octet pour octet |
| SNP-04 | snapshot existant, puis nouveaux enregistrements | `spec.snapshot: id` | les pods chargent ce snapshot, sans les nouveaux enregistrements |
| SNP-05 | snapshot chargé, contrôleur arrêté | requêtes mockées | toujours servies depuis le snapshot |
| SNP-06 | endpoint appelé bien plus de `samples_per_operation` + `max_per_key` fois en record (seuls les premiers exemples gardent leur body) | bascule en mock | la réponse enregistrée est rejouée, pas de `599` |
| SNP-07 | état souhaité `mock` avec un snapshot que le sidecar ne peut pas télécharger | battement | le sidecar passe quand même en mock, sur ses données locales, et rien ne sort ; l'erreur est visible dans son statut et le téléchargement est retenté |
| SNP-08 | forme de mock plus grosse que `share.max_snapshot_bytes`, telle quelle, ou compressée sous la borne mais au-delà une fois décompressée ; puis sous la borne | téléchargement | refusée sans rien charger, erreur qui nomme `share.max_snapshot_bytes` ; sous la borne : chargée. Par défaut, `share.max_snapshot_bytes` vaut 64 MiB |

**Snapshots, cycle de vie (SNP, suite, §5.6)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| SNP-09 | métadonnées (`format`, `created_ms`, `exchanges`, `pinned`) | aller-retour JSON | mêmes métadonnées ; un `format` futur (ou un champ inconnu) est refusé, jamais accepté silencieusement |
| SNP-10 | des snapshots candidats, certains référencés, certains épinglés, dans n'importe quel ordre (proptest) | purge | jamais un référencé ni un épinglé ; au-delà d'eux, seuls les `keep_last` plus récents (par date, puis identifiant) survivent ; résultat indépendant de l'ordre des candidats |
| SNP-11 | un contenu JSONL exporté, puis réimporté ; une ligne invalide | import (`mimicctl snapshot import`) | contenu canonicalisé, même identifiant qu'à l'export ; ligne invalide refusée avec son numéro |
| SNP-12 | cache sidecar des formes de mock (`<store.dir>/controller/snapshots/*.mock.json`), au-delà de `store.max_cached_snapshots` | un nouvel état souhaité est sauvegardé sur disque | les plus anciennes (par date de dernière écriture) sont supprimées ; jamais celle du snapshot actif, même si elle est la plus ancienne ; sans snapshot actif, seules les `max_cached_snapshots` plus récentes survivent |

**Contrôleur (CTL)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| CTL-01 | deux sidecars d'un workload avec `share.url` vers le contrôleur | trafic sur chacun | le contrôleur liste le workload, ses deux pods et l'union des opérations |
| CTL-02 | pods en record | `MimicWorkload` passé en `mock` | tous les pods passent en mock sans redémarrage, avec le même snapshot, et apparaissent alignés |
| CTL-03 | pods en mock | `MimicWorkload` passé en `record` | tous repassent en record, sans snapshot |
| CTL-04 | contrôleur sur `file://` avec segments, snapshot et état souhaité | redémarrage | tout est retrouvé |
| CTL-05 | trafic enregistré avec une query toujours présente | `GET /ui/{ns}/{wl}` | la page liste l'opération, marque le paramètre requis, montre les pods et la commande de bascule |

**Contrôleur à l'échelle (CTL, suite, §5.5.2)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| CTL-06 | segments et workloads rangés sur le support avant le démarrage du contrôleur | listes successives, puis ingestion d'un segment | les segments et workloads rangés sont listés ; le support n'est plus relu (un fichier déposé ensuite par un autre processus n'apparaît pas) ; le segment ingéré apparaît |
| CTL-07 | segments datés d'il y a 2 jours, de 2 heures et de maintenant, rétention par défaut (24 heures) | `GET …/segments`, puis `?since_ms=` | le segment hors rétention n'est jamais listé ; avec `since_ms`, seuls les segments datés d'au moins cette valeur, du plus ancien au plus récent |
| CTL-08 | lecture du support bloquée pendant le chargement des échanges d'un workload (vue de l'interface en cours) | battement de ce workload, `/v1/workloads`, `/metrics` | réponses immédiates, battement avec l'état souhaité courant |
| CTL-09 | pod sans battement depuis plus de `pod_forget` | battement d'un nouveau pod, ou passe de maintenance | le pod disparu n'est plus listé ni compté |
| CTL-10 | segments plus vieux que `retention`, un snapshot et un état souhaité | passe de maintenance | ces segments sont supprimés du support et de la liste ; snapshot et état souhaité sont gardés |
| CTL-11 | segments bruts de deux instances dans une heure révolue, avec un échange en double et plus de `samples_per_operation` bodies pour une opération | passe de maintenance | un seul segment `_hour/<AAAAMMJJHH>.jsonl` pour cette heure, sans doublon, au plus `samples_per_operation` bodies par couple (opération, code), choisis par le réservoir du §5.2 : mêmes exemples quel que soit l'ordre des segments bruts ; segments bruts supprimés ; un pod qui liste sans curseur reçoit le segment horaire |
| CTL-12 | plus de `samples_per_operation` échanges avec body pour une même opération, chemins de même forme (`/users/1`, `/users/2`) | ingestion | seuls `samples_per_operation` gardent leur body en mémoire, les autres sont gardés sans body ; une éviction libère une place |
| CTL-13 | deux workloads dont le total dépasse `max_exchanges_total` ; workload dont le support dépasse `max_exchanges_per_workload` | ingestion ; redémarrage, ingestion, puis vue | total en mémoire ≤ `max_exchanges_total`, les plus anciens du plus gros workload évincés ; après le redémarrage, l'ingestion ne relit rien ; à la vue, seuls les `max_exchanges_per_workload` plus récents sont relus |
| CTL-14 | découverte déjà calculée, puis nouveaux échanges | vue dans le `discovery_interval` du calcul ; vues après l'intervalle | même document servi sans recalcul ; recalcul en tâche de fond, puis les nouvelles opérations apparaissent |
| CTL-15 | segment déjà reçu (délai dépassé côté sidecar) | même segment renvoyé avec le même identifiant | `200`, une seule copie sur le support et en mémoire, `segments_total{result="duplicate"}` |

**Exemples par endpoint et historique court (CTL, suite, §5.5.3)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| CTL-16 | échanges d'endpoints variés, dans n'importe quel ordre, avec doublons (proptest) : rejouables ou non, plus ou moins récents, chemins de même forme, codes `1xx` | sélection des exemples (`mimic-core::exemplar`) | un exemple par endpoint (opération, code) qui a au moins un échange rejouable : le plus récent (date, puis identifiant) ; aucun pour un endpoint sans échange rejouable ni pour un `1xx` ; mêmes exemples quel que soit l'ordre ; un exemple n'est remplacé que par un échange rejouable plus récent du même endpoint ; de deux versions d'un même échange, le snapshot garde la rejouable |
| CTL-17 | `GET /rare` sortant appelé une seule fois, dans un segment ingéré puis sorti de la `retention` ; `GET /old` dans un segment déposé sur le support avant le démarrage, hors rétention ; un endpoint d'une heure révolue dont le plus récent perd son body à la compaction ; trafic chaud au-delà de `max_exchanges_per_workload`, `max_exchanges_total` et `max_bytes_per_workload` | passes de maintenance (rétention, compaction, plafond de volume), redémarrage du contrôleur, bascule en mock | segments supprimés et échanges évincés de la mémoire, mais `exemplars.jsonl` garde un exemple de chaque endpoint, avec son body, le plus récent ; après le redémarrage, le snapshot sert `/rare`, `/old` et l'endpoint compacté avec leur réponse ; les requêtes enregistrées listent `/rare` |
| CTL-18 | `max_exemplars_per_workload: 2`, trois endpoints, puis un échange plus récent du premier | ingestion | deux exemples gardés, ceux des deux premiers endpoints ; le troisième est refusé et compté dans `mimic_controller_exemplars_refused_total{reason="count"}` ; `mimic_controller_exemplars` vaut 2 ; le premier endpoint prend le nouvel exemple |
| CTL-22 | `max_exemplar_bytes_per_workload` juste au-dessus d'un exemple ; puis `max_exemplar_bytes_total` qui tient un seul exemple pour deux workloads | ingestion d'un endpoint, d'un deuxième, d'un échange plus récent et plus gros du premier, puis d'un plus récent et plus petit ; endpoint d'un second workload ; passe de maintenance | le deuxième endpoint garde un exemple sans body, présent dans la découverte et dans `exemplars.jsonl` ; le remplacement trop gros est refusé (l'ancien exemple reste), le petit est accepté ; l'endpoint du second workload n'a qu'un exemple sans body ; chaque cas compté dans `mimic_controller_exemplars_refused_total{reason="bytes"}` ; la taille gardée reste sous les bornes, hors exemples sans body ; après un redémarrage, un exemple sans body relu de `exemplars.jsonl`, même rangé avant un exemple avec body, est repris tel quel : même fichier, même taille gardée, aucun refus compté, l'exemple avec body garde le sien, le snapshot le porte sans body ; un échange plus récent du même endpoint qui tient dans la borne le rend rejouable |
| CTL-19 | deux workloads du même namespace, chacun avec ses propres endpoints | ingestion, maintenance, bascule de chacun en mock | chaque `exemplars.jsonl` et chaque snapshot ne contient que les endpoints vus par les pods de son workload ; un pod ne publie pas pour l'autre workload (`403`, SEC-03) |
| CTL-20 | sidecar réel piloté par le contrôleur ; appel sortant avec `Authorization: Bearer <secret>`, réponse JSON contenant une adresse e-mail | publication, passe de maintenance | `exemplars.jsonl` contient l'endpoint, sans le secret ni l'adresse : seulement les marques de redaction posées par le sidecar |
| CTL-21 | configuration par défaut du contrôleur et du sidecar ; manifests de `deploy/` | lecture | `retention` et `share.retention` valent 24 heures, `max_exemplars_per_workload` 10 000, `max_exemplar_bytes_per_workload` 64 MiB et `max_exemplar_bytes_total` 512 MiB ; le ConfigMap du contrôleur reprend la rétention et les bornes |

**Snapshot de mock léger (CTL, suite, §5.6)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| CTL-23 | snapshot d'un workload dont un endpoint a été appelé 2 000 fois sur des chemins distincts de même forme, les premiers seulement avec body | `GET …/snapshots/{id}/mock` par deux pods du workload, avec `Accept-Encoding: zstd`, `gzip` et sans ; puis avec `If-None-Match` ; `GET …/snapshots/{id}` ; sans jeton, et pour un snapshot inconnu | mêmes octets et même `ETag` pour les deux pods ; décodée, la même forme de mock quel que soit l'encodage, égale à celle que `mimic-core` déduit du snapshot complet ; compressée, au moins 20 fois plus petite que le snapshot complet ; `304` sur l'`ETag` ; le chemin d'avant sert le JSONL complet, octet pour octet ; `401` sans jeton ; `404` si inconnu |

**Haute disponibilité (CTL, suite, §5.5.4)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| CTL-24 | bail absent, rendu (sans détenteur), tenu par une autre réplique et vu inchangé depuis moins, puis au moins `lease_duration` sur l'horloge locale, dates écrites dans le bail quelconques ; bail tenu par la réplique ; active dont le dernier renouvellement réussi a commencé il y a moins, puis au moins `renew_deadline` ; deux répliques dont les requêtes sont retardées, perdues ou en conflit, dans n'importe quel ordre (proptest) ; timings incohérents | décision d'élection (`mimic-core::lease`) ; validation de la configuration | prise du bail s'il est absent, rendu ou expiré, transitions comptées ; attente sinon ; renouvellement par son détenteur ; l'active se tient pour perdue dès `renew_deadline` ; jamais deux répliques ne se croient actives au même instant ; `retry_period` < `renew_deadline` < `lease_duration` exigé |
| CTL-25 | deux répliques sur le même bail (en mémoire) et le même support | démarrage simultané ; arrêt propre de l'active ; arrêt brutal de la nouvelle active (plus de renouvellement) ; active dont les écritures du bail échouent | une seule active à tout instant ; après l'arrêt propre, la passive prend le relais en moins de 2 × `retry_period` ; après l'arrêt brutal, pas avant `lease_duration` ; l'active qui ne renouvelle plus répond `503` et signale la perte avant que l'autre ne prenne le bail, sans avoir écrit ses exemples |
| CTL-26 | réplique passive, support qui échoue à toute opération | `/healthz`, battement, segment, `GET …/snapshots/{id}/mock`, `/v1/workloads`, `/metrics` | `200` pour `/healthz` ; `503` avec `Retry-After: 2` pour le reste, sauf `/metrics` qui donne `mimic_controller_leader 0` ; aucune opération sur le support ; une fois active, `mimic_controller_leader 1` et `mimic_controller_takeover_seconds` renseigné |
| CTL-27 | active A avec segments, exemples, snapshot de mock et état souhaité ; passive B sur le même support | arrêt propre de A ; puis, dès la marque de B posée, support coupé : battement, segment déjà reçu, `/metrics` ; support rétabli : `GET …/snapshots/{id}/mock` | A a écrit ses exemples puis rendu le bail ; B a rechargé l'état souhaité, l'index et les exemples avant de poser sa marque : le battement répond le même snapshot, le segment renvoyé est un doublon (`200`), `mimic_controller_exemplars` compte les exemples de A ; même forme de mock, mêmes octets, même `ETag` qu'avant la bascule |
| CTL-28 | manifests de `deploy/controller/` | lecture | Deployment à 2 réplicas, `RollingUpdate` avec `maxUnavailable: 0`, anti-affinité préférentielle par nœud, `POD_NAME` par l'API downward, probe de vivacité ; Service qui sélectionne `mimic.platform.io/leader: "true"` ; PodDisruptionBudget `minAvailable: 1` sur les pods du contrôleur ; configuration avec `leader_election` valide, bail `mimic-controller` dans `mimic-system` ; Role dans `mimic-system` : `create`, `get`, `patch` sur `leases`, `list`, `patch` sur `pods`, et le ClusterRole sans droit sur les leases ni écriture sur les pods |

**Contrôleur en production (CTL, suite, §9.2b)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| CTL-29 | contrôleur en HTTPS, `tls.reload_interval` court ; certificat remplacé sur disque, puis contenu illisible | poignées de main suivantes | nouveau certificat présenté sans redémarrage ; contenu illisible ignoré, l'ancien reste servi |
| CTL-30 | API du contrôleur | client qui ouvre une connexion TLS sans envoyer d'en-têtes ; client TCP muet | la connexion est fermée après le délai des en-têtes, ou de la poignée de main (mêmes délais que INJ-15) |
| CTL-31 | requête `POST …/segments` puis `POST …/heartbeat` qui annonce un corps de 64 MiB (`Content-Length`) sans l'envoyer, sans jeton puis avec le jeton d'un autre workload | envoi des seuls en-têtes | `401`, puis `403`, aussitôt, sans attendre le corps ; un battement de plus de 64 KiB est refusé (`413`) |
| CTL-32 | contrôleur actif, puis passif ; manifests de `deploy/controller/` et `deploy/example/` | `GET /metrics` sur le port de l'API, sans jeton ; lecture | `200` avec `mimic_controller_leader` (1, puis 0) ; la NetworkPolicy ouvre 8443 aux namespaces labellisés `mimic.platform.io/monitoring: "true"` ; un exemple de collecte vise `/metrics` du contrôleur en HTTPS |
| CTL-33 | `desired.json` d'un workload en `mock` sur un support qui échoue à la première lecture ; puis support rétabli ; `desired.json` absent ; `desired.json` illisible | battements | premier battement `503`, l'erreur journalisée ; battement suivant : l'état souhaité persisté (même snapshot) ; absent ou illisible : état vide |
| CTL-34 | pod annoté `mimic.platform.io/workload: " checkout "`, ou annotation vide avec le label `app.kubernetes.io/name: web` | rattachement au workload (`workload_of`) | `checkout`, puis `web` : même règle que l'injecteur (§5.4) |
| CTL-35 | manifests de `deploy/controller/` | lecture | image `ghcr.io/thaliph/mimic:0.1.0` ; UID et GID 1338 ; `priorityClassName: mimic-control-plane` ; mémoire demandée d'au moins 1Gi ; Role `mimic-cli` dans `mimic-system` : `get` et `list` sur `pods`, `create` sur `pods/portforward`, rien d'autre |
| CTL-42 | deux pods sélectionnés : `checkout-a` annonce un relevé `unmatched` (total 3 : `GET /users/{int}` ×2, `POST /orders` ×1, sur `api.saas.io:443`), `checkout-b` le même `GET` ×4 plus une clé `413` ; puis un battement sans `unmatched` (sidecar antérieur) et un battement avec un champ inconnu | `GET /v1/workloads`, `GET /ui/shop/checkout`, statut publié (`status_of`) | chaque pod expose son relevé ; le workload : total 8, `GET /users/{int}` à 6 occurrences avec le premier et le dernier horodatage des deux pods, clés triées par dernière occurrence ; la page montre la section des requêtes sans réponse ; `status.unmatched: 8` ; sans `unmatched`, le pod compte pour 0 et le battement est accepté ; champ inconnu ignoré (`200`) ; un pod qui annonce 30 clés n'en garde que les 20 premières |

**Cycle de vie des snapshots (CTL, suite, §5.6)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| CTL-43 | snapshot construit, puis épinglé (`pin`), puis rebasculé en mock sans nouveau trafic | construction, écriture, reconstruction du même contenu | métadonnées (`created_ms`, `exchanges`, `pinned: false`) écrites à la première écriture ; la reconstruction retombe sur le même identifiant (content-addressed) sans réécrire les métadonnées : `pinned: true` posé entre-temps survit |
| CTL-44 | plusieurs snapshots construits dans le temps, `snapshot.keep_last` petit, l'un explicitement référencé (`--snapshot`) malgré son âge | passe de maintenance (toutes les minutes) | les référencés et les `keep_last` plus récents survivent, le reste est supprimé (contenu, métadonnées et forme de mock en cache) ; chaque suppression incrémente `mimic_controller_snapshots_deleted_total` |
| CTL-45 | snapshot servi en mock avec 0 échange, puis 1 ; hors mock ; sans snapshot ; nombre d'échanges inconnu | statut publié | `status.snapshotExchanges` reflète le nombre résolu (jamais une I/O derrière le verrou court de l'état) ; condition `SnapshotEmpty` vraie seulement en mock ou mock-partial avec un snapshot à 0 échange, jamais vraie si le nombre est inconnu |
| CTL-47 | API agrégée configurée avec une CA de front proxy de test ; autorisation simulée | `GET` avec un certificat client de cette CA, `X-Remote-User: alice`, `X-Remote-Group: dev` (deux fois), `X-Remote-Extra-Scopes: a`, sur la racine, `…/namespaces/shop/workloads`, `…/namespaces/shop/workloads/checkout/{operations,openapi/inbound,requests?direction=outbound,snapshots}` ; puis accès refusé ; puis autorisation indisponible ; puis `POST` | racine : `APIResourceList` sans autorisation demandée ; chaque route : l'accès vérifié est celui du tableau (§9.2c), avec l'utilisateur, les deux groupes et l'extra, et le corps est celui de la route de l'interface ; la liste du namespace ne contient que `shop` ; refus : `403` `Status` `Forbidden` ; indisponible : `503` ; `POST` : `405` ; une même décision n'est demandée qu'une fois en 10 s |
| CTL-48 | API agrégée configurée, `allowed_names: [front-proxy-client]` | connexion sans certificat client ; avec un certificat d'une autre CA ; avec un certificat de la bonne CA au nom `intrus` ; `X-Remote-User` sur l'API des sidecars (8443) | trois poignées de main refusées ; sur 8443, l'en-tête n'a aucun effet (jeton exigé, `401`) ; `aggregated` sans `tls` : configuration refusée au démarrage |
| CTL-46 | snapshot inconnu ; contenu JSONL invalide ; contenu JSONL valide ; snapshot non référencé et non épinglé | `POST …/snapshots/{id}/pin`, `POST …/snapshots` (import), `POST …/snapshots/gc` | `pin` d'un inconnu : `404` ; import invalide : `400` ; import valide : `200` avec l'identifiant et le nombre d'échanges, épinglé par défaut ; `gc` : purge immédiate, répond les identifiants supprimés |

**Démarrage d'un pod piloté (CTL, suite, §5.5.5)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| CTL-36 | barrière de démarrage (`mimic-core::startup`) : sans état connu avec chaque repli, état rechargé, état reçu sans son snapshot ; réceptions, alignements et échéances dans n'importe quel ordre (proptest) | décisions | fermée jusqu'à l'alignement ou l'échéance ; à l'échéance : ouverte sur l'état connu (`last-known`) s'il y en a un, sinon repli (`record`, `passthrough` et `mock-local` ouvrent, `fail` reste fermée) ; un état reçu après l'échéance ouvre, même avec `fail` ; attente en mock sans état connu avec `mock-local` et `fail` seulement ; l'alignement ouvre toujours ; une barrière ouverte ne se referme jamais |
| CTL-37 | workload en mock sur un snapshot ; pod neuf (store vide) ; forme de mock indisponible, puis rétablie | démarrage du sidecar, appel sortant | `/healthz` `200` d'emblée ; `/readyz` `503` `waiting` tant que le snapshot n'est pas chargé, le sidecar déjà en mock sur ses données locales ; puis `200` `aligned`, en mock sur le snapshot ; aucun appel n'a atteint la dépendance ; `desired.json` écrit |
| CTL-38 | contrôleur qui ne répond pas (`503`), pod neuf, `share.startup.timeout` court ; chaque repli | démarrage, appel sortant après l'échéance, puis contrôleur qui répond un état vide | `/readyz` `503` jusqu'à l'échéance ; `record` et `passthrough` : `200` `fallback` dans ce mode, l'appel atteint la dépendance ; `mock-local` : `200` `fallback`, `599` sans joindre la dépendance ; `fail` : toujours `503`, en mock, rien ne sort ; contrôleur rétabli : `200` `aligned`, mode de la configuration ; `desired.json` écrit, même vide |
| CTL-39 | pod en mock sur un snapshot dont seul le sidecar redémarre, contrôleur injoignable, repli `fail` ; pod dont le premier battement a reçu un état vide, redémarré avec `mock-local` | redémarrage du sidecar | mock et snapshot rechargés dès le démarrage : réponse rejouée sans sortir ; `/readyz` `503` puis `200` `last-known` à l'échéance malgré `fail` ; état vide rechargé : mode de la configuration, pas de mock |
| CTL-40 | battements en échec, puis rétablis | `GET /metrics` | `mimic_heartbeats_total{result="error"}`, puis `{result="ok"}`, augmentent ; `mimic_heartbeat_last_success_timestamp_seconds` à la date du dernier battement réussi (0 avant) ; `mimic_startup` vaut 1 pour l'état courant de la barrière, 0 pour les autres |
| CTL-41 | configuration du sidecar | chargement | `share.startup.timeout` 30 s et `share.startup.fallback` `record` par défaut ; délai nul ou au-delà de 300 s refusé avec un message qui nomme le champ ; repli inconnu refusé avec les valeurs admises ; les manifests de `deploy/` donnent `share.startup` explicitement |

**Ressource MimicWorkload (CRD)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| CRD-01 | `spec.mode: mock` sans snapshot, `status` vide | réconciliation | snapshot construit, `status.snapshot` renseigné, état souhaité `mock` + ce snapshot |
| CRD-02 | `status.snapshot` déjà renseigné, nouveaux échanges | réconciliation | même snapshot gardé |
| CRD-03 | `spec.snapshot` inconnu | réconciliation | condition `Aligned=False`, raison `SnapshotNotFound`, état souhaité inchangé |
| CRD-04 | `spec.mode: record` après du mock | réconciliation | `status.snapshot` vidé, état souhaité `record` sans snapshot |
| CRD-05 | ressource supprimée | réconciliation | état souhaité vide |
| CRD-06 | deux pods, un seul aligné | calcul du statut | `pods: 2`, `aligned: 1`, `Aligned=False` |
| CRD-07 | namespace labellisé `env=production` ; `spec.mode: mock` ou `mock-partial`, depuis `record` puis depuis `mock` | réconciliation, battement, statut | aucun snapshot construit ; état souhaité inchangé depuis `record`, `record` sans snapshot depuis `mock` ; raison `ProductionNamespace`, conditions `Aligned` et `Allowed` à `False` avec un message ; un pod qui annonce `mock` reçoit `record` ; label retiré, le mock est de nouveau accepté |
| CRD-08 | `spec.selector.matchLabels: {version: v2}`, puis des `matchExpressions` ; pods `v1` et `v2` | réconciliation, battements, statut | le pod `v2` reçoit l'état souhaité, le pod `v1` un état vide ; `pods: 1` et `aligned` ne comptent que `v2` ; `In`, `NotIn`, `Exists`, `DoesNotExist` suivent la sémantique Kubernetes ; sélecteur vide : tous les pods ; opérateur inconnu : aucun |
| CRD-09 | `spec.capture.inbound: metadata` | réconciliation, battement | état souhaité avec `capture_inbound: metadata` ; un pod qui annonce `full` diverge, un pod qui annonce `metadata` est aligné ; sans `spec.capture`, aucune capture imposée |
| CRD-10 | `spec.mock.websocket: refuse` en `mock`, puis en `record` ; valeur inconnue ; sans `spec.mock` ; puis passage de `relay` à `refuse` sans quitter le mock | réconciliation, battement ; génération de la CRD | état souhaité avec `mock_websocket: refuse` ; un pod qui annonce `relay`, ou rien (sidecar d'une version antérieure), diverge, un pod qui annonce `refuse` est aligné ; valeur inconnue : état souhaité inchangé, raison `InvalidMock` ; sans `spec.mock`, aucun réglage imposé ; le changement de réglage garde le snapshot courant ; le schéma de la CRD (`mimic-controller crd`, identique à `deploy/controller/crd.yaml`) n'accepte que `relay` et `refuse` |

**Sécurité (SEC)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| SEC-01 | sidecar rattaché à un contrôleur | `PUT /mode` ou `DELETE /store` sur son admin, ou annotation de mode | `403` / ignorée, mode inchangé |
| SEC-02 | appel à l'API des sidecars sans jeton ou avec un jeton invalide | toute route sauf `/healthz` | `401`, compteur `auth_failures` |
| SEC-03 | jeton valide d'un pod du workload A | écriture ou lecture sur le workload B, ou ingestion au nom d'une autre instance | `403` |
| SEC-04 | `share.url` en `http://` sans `allow_insecure` | démarrage du sidecar | configuration refusée |
| SEC-05 | contrôleur servi avec un certificat d'une autre CA que `share.ca_file` | battement | refusé, mode inchangé, erreur visible dans le statut |
| SEC-06 | interface du contrôleur | écoute | liée à `127.0.0.1` uniquement |
| SEC-08 | `deploy/controller/` | lecture | `APIService` `v1alpha1.data.mimic.platform.io` vers le Service `mimic-controller`, port `aggregated`, `caBundle` injecté par cert-manager ; le Service et le Deployment exposent 8444, la NetworkPolicy l'ouvre ; le ClusterRole du contrôleur crée des `subjectaccessreviews`, un RoleBinding de `kube-system` lui donne `extension-apiserver-authentication-reader` ; le ClusterRole `mimic-data-viewer` n'accorde que `get` et `list` sur `workloads` et ses sous-ressources du groupe `data.mimic.platform.io` ; la configuration du contrôleur déclare `aggregated` |
| SEC-07 | admin du sidecar à l'écoute sur toutes les adresses | requêtes depuis `127.0.0.1`, puis depuis une autre adresse (`127.0.0.6`, d'où Istio relaie les autres pods) : `/`, `/api/status`, `/api/operations`, `/openapi/inbound`, `/openapi/outbound`, `/exchanges`, `GET` et `PUT /mode`, `DELETE /store`, `POST /share/sync`, chemin inconnu ; puis `/healthz`, `/readyz`, `/metrics` | depuis `127.0.0.1` : servies ; depuis l'autre adresse : `403`, mode et store inchangés, aucune synchronisation demandée ; sondes et `/metrics` : `200` depuis toute adresse |

**Rejeu (RPL)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| RPL-01 | requêtes entrantes enregistrées par deux pods | `GET …/requests?direction=inbound` | toutes listées, les plus récentes d'abord, avec l'instance |
| RPL-02 | requête avec un header `authorization` redacté | `format=k6` | script k6 valide qui rejoue méthode, chemin, query et body, sans le header redacté |
| RPL-03 | requêtes enregistrées | `format=jsonl` | un échange par ligne, relisible tel quel |

**Partage (SHR)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| SHR-01 | deux sidecars du même workload sur un stockage objet partagé, sans contrôleur, chacun ayant vu des opérations différentes | synchronisation | chaque sidecar documente l'union des opérations |
| SHR-02 | enregistrement d'une dépendance fait par le sidecar A, en partage direct (sans contrôleur ; sous contrôleur, voir SNP-03) | requête mockée sur le sidecar B | B répond avec l'enregistrement de A |
| SHR-03 | segments déjà importés | nouvelle synchronisation | aucun échange dupliqué |
| SHR-04 | sidecar d'un autre workload sur le même support | synchronisation | ses échanges ne sont pas importés |
| SHR-05 | support indisponible | synchronisation | le trafic n'est pas affecté, `mimic_share_sync_total{result="error"}` augmente |
| SHR-06 | sidecar sans partage configuré | trafic enregistré | aucun échange n'est gardé pour publication |
| SHR-07 | partage configuré, support indisponible, trafic au-delà de `share.max_backlog_bytes` | synchronisations échouées répétées | la file reste sous sa borne et garde les échanges les plus récents ; les échanges jetés sont comptés dans `mimic_capture_dropped_total{reason="share_backlog"}` |
| SHR-08 | sidecar en partage direct qui a déjà importé des segments ; puis un segment de pair daté d'avant le plus récent importé moins 60 s, et un autre daté dans cette marge (sous contrôleur, le sidecar ne liste plus : SHR-10) | synchronisations suivantes | le premier n'est ni lu ni importé, le second l'est ; un import échoué ne fait pas avancer le curseur |
| SHR-09 | contrôleur qui refuse les publications, puis revient ; trafic au-delà de `share.max_pending` | publications successives | même identifiant de segment à chaque reprise, attente croissante entre les tentatives ; la même file reste sous `share.max_pending`, garde les plus récents et compte les jetés dans `mimic_capture_dropped_total{reason="share_backlog"}` ; segments d'au plus 8 MiB |
| SHR-10 | sidecars rattachés à un contrôleur dont la liste contient des segments d'autres instances ; A a enregistré un appel sortant et un appel entrant, B rien | synchronisations en record, puis bascule en mock | A publie ses segments ; aucun sidecar ne liste ni ne lit de segment : `store.peer` à 0, aucun pair, chacun ne documente que son propre trafic et sa page d'admin le dit ; en mock, B sert l'appel de A depuis le snapshot du contrôleur |
| SHR-11 | deux sidecars pilotés par le contrôleur, `store.dir` configuré ; A seul a appelé une dépendance bien plus de `samples_per_operation` fois, sur des chemins de même forme ; puis un contrôleur d'une version antérieure, qui ne sert que le snapshot complet | bascule en mock | A et B gardent la même forme de mock, octet pour octet (cache sur disque), celle que sert le contrôleur, plus petite que le snapshot complet ; ils servent la même réponse, celle enregistrée, sur un chemin avec body comme sur un chemin sans, et rien ne sort ; face au contrôleur antérieur, le sidecar télécharge le snapshot complet, en déduit la même forme de mock, et sert la même réponse |

**Capture et store (CAP)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| CAP-01 | 100 réponses `GET /users/{n}` avec n distinct, `samples_per_operation: 20` | capture | au plus 20 bodies gardés pour l'opération, les autres échanges en métadonnées seules |
| CAP-02 | même flux d'échanges présenté dans deux ordres différents | capture | mêmes exemples gardés ; parmi eux, des échanges arrivés après les `N` premiers |
| CAP-03 | réservoir d'une opération plein (ici `samples_per_operation: 0`), réponse JSON avec une adresse e-mail et plus grosse que `max_body_bytes` | capture | body ni décodé ni redacté ni tronqué : `Skipped`, aucune marque de redaction de body, `reason="sample_limit"` compté et `reason="truncated"` inchangé |
| CAP-04 | ids uniques dans les chemins, échanges évincés | capture prolongée | les compteurs d'échantillonnage n'existent que pour les couples qui ont encore un exemple |
| CAP-05 | `capture.max_store_bytes` dépassé | ajout d'échanges | les bodies les plus anciens sont retirés dès que les bodies dépassent la moitié du budget, et les échanges les plus anciens sont évincés au-delà du budget ; le store reste sous le budget, le dernier échange garde ses bodies, et `reason="store_budget"` est compté |
| CAP-06 | `store.dir`, `segment_bytes` et `max_segments` petits | trafic au-delà de plusieurs segments | au plus `max_segments` fichiers `local-<n>.jsonl`, chacun sous `segment_bytes` plus une ligne ; après redémarrage, les échanges les plus récents sont rechargés et les plus anciens ne le sont plus |
| CAP-07 | file du journal local pleine | ajout d'échanges | l'ajout ne bloque pas, l'échange est en mémoire, la ligne non écrite est comptée (`reason="local_backlog"`) ; sans demande explicite, une ligne écrite est sur disque en moins d'une seconde |
| CAP-08 | configuration par défaut du webhook (§9.4) | injection d'un pod | journal local au plus le quart de l'`emptyDir` du store ; budget du store et file de partage au plus le tiers de la limite mémoire du sidecar |
| CAP-09 | `capture.inbound: metadata`, journal local et partage configurés | requête entrante `POST /orders?token=x` avec headers et body JSON, puis un appel sortant | un seul échange entrant en mémoire : méthode, chemin, code, durées et taille des bodies (`Skipped`), sans query, headers ni body ; aucune ligne entrante dans le journal local ni dans la file de publication ; l'appel sortant est enregistré en entier |
| CAP-10 | `capture.inbound: off` | requêtes entrantes | aucun échange entrant ; `mimic_requests_total{direction="inbound"}` et les observations de `mimic_request_duration_seconds{direction="inbound"}` les comptent toutes |
| CAP-11 | `capture.max_inflight_bytes` qui tient le body d'une réponse mais pas celui de deux, `max_body_bytes` plus grand ; dépendance qui garde sa réponse ouverte après l'avoir envoyée | deux requêtes concurrentes, puis une requête seule après leur fin | réponses relayées en entier, sans attente ; au moins un body au-delà du budget gardé tronqué (`Truncated`) et compté `reason="inflight_budget"` ; budget rendu : la requête seule est capturée en entier. Par défaut, `capture.max_inflight_bytes` vaut 64 MiB |

**Admin et métriques (ADM)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| ADM-01 | trafic enregistré sur `GET /users/{userId}` avec query `page` toujours présente | `GET /` | la page liste l'opération et marque `page` comme requis |
| ADM-02 | trafic entrant et sortant, en record puis en mock | `GET /metrics` | les métriques du tableau §9.3 sont présentes avec les bons labels |
| ADM-03 | `PUT /mode` avec `mock` | requête suivante | servie en mock, `mimic_mode{mode="mock"} 1` |
| ADM-04 | `MIMIC_VERSION=v2` ; puis `MIMIC_VERSION` vide et `MIMIC_POD_TEMPLATE_HASH=7d9f` ; puis aucune version | trafic, `GET /metrics` | chaque métrique porte `version="v2"`, puis `version="7d9f"`, puis aucun label `version` ; valeur tronquée à 63 caractères |
| ADM-05 | authority et séries déjà vues | comptage d'une requête : `mimic_requests_total`, durée, réponse et latence injectée du mock, erreur amont, capture abandonnée | aucune allocation mémoire ; l'authority est résolue sans verrou exclusif ; après du trafic, `/metrics` donne des jauges du store exactes, calculées à la lecture |
| ADM-06 | requêtes entrantes de méthodes non standard (`PROPFIND`, puis 20 méthodes inventées) | `GET /metrics` | toutes comptées sous `method="other"` dans `mimic_requests_total` ; aucune série ne porte une méthode inventée |
| ADM-07 | mode `mock`, `unmatched: error`, store vide | 150 requêtes sortantes vers 150 chemins littéraux distincts, puis `GET /api/unmatched`, `?limit=5`, et la même route depuis une adresse autre que la boucle locale | `total` à 150 ; exactement 100 clés, les 100 plus récentes, triées de la plus récente à la plus ancienne ; `?limit=5` : les 5 premières ; hors boucle locale : `403` |

**Init (INI)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| INI-01 | ports entrants `8080,9090` | génération | une règle `REDIRECT` par port vers 15106, ports du sidecar exclus |
| INI-02 | ports entrants `*` | génération | redirection de tout le TCP entrant sauf les ports du sidecar |
| INI-03 | UID, CIDR et ports sortants exclus | génération | règles `RETURN` placées avant la redirection sortante |
| INI-04 | règles déjà posées | deuxième application | aucune règle dupliquée |
| INI-05 | sorties de `iptables-save -t nat` d'un pod Istio sidecar (CNI), Istio ambient, Linkerd, et d'un pod hors mesh | détection | Istio sidecar UID et GID 1337, source `127.0.0.6/32` ; ambient marque `0x539/0xfff` ; Linkerd UID 2102 ; aucun mesh. Valeurs différentes dans les règles (UID 1500) reprises telles quelles |
| INI-06 | mesh Istio sidecar, ports entrants `8080` | génération | aucune règle `PREROUTING` ; saut vers `MIMIC_OUTBOUND` en tête de `OUTPUT` ; `-s 127.0.0.6/32 -o lo -p tcp --dport 8080 -j REDIRECT --to-ports 15106` avant le `RETURN` de l'UID 1338, puis `RETURN` pour l'UID et le GID 1337 avant la redirection sortante vers 15101 ; en IPv6, source `::6/128` |
| INI-07 | mesh Istio ambient, ports entrants `*`, IP du pod `10.244.0.20` | génération | trajet entrant `-o lo -m mark --mark 0x539/0xfff`, ports du sidecar exclus, envoyé par `-j DNAT --to-destination 10.244.0.20:15106`, puis `RETURN` de la marque avant la redirection sortante ; sans IP du pod, ou en IPv6 avec une IP IPv4, `REDIRECT --to-ports 15106` |
| INI-08 | mesh Linkerd | génération | trajet entrant `-o lo -m owner --uid-owner 2102` ; `RETURN` de l'UID 2102 |
| INI-09 | règles du mesh posées, saut de Mimic absent, puis présent mais derrière celui du mesh | plan d'application | saut inséré en position 1 ; s'il est déjà en tête, rien ; sinon retiré puis réinséré en tête, une seule fois |
| INI-10 | `MIMIC_MESH` `auto`, `none`, `linkerd` ; règles Linkerd visibles seulement par `iptables-legacy` | choix | `auto` : Linkerd, binaire `iptables-legacy` ; `none` : aucun mesh, binaire configuré ; `linkerd` sans règles trouvées : UID 2102 par défaut ; valeur inconnue refusée |
| INI-11 | `MIMIC_EXCLUDE_OUTBOUND_CIDRS` avec `10.0.0.0/8`, `fd00::/8` et `10.96.0.1` ; puis une valeur `10.0.0.0/33`, `example.com`, ou une valeur qui contient un saut de ligne suivi d'une règle ; ports `0`, `70000` ou `http` ; `POD_IP` invalide | analyse ; `mimic-init --dry-run` | CIDR canoniques (`10.96.0.1/32`) ; valeur invalide refusée : message qui nomme la variable et la valeur, code de sortie 2, aucune règle affichée ni posée |
| INI-12 | règles par défaut hors mesh, sous Istio sidecar, Istio ambient et Linkerd, en IPv4 et en IPv6 ; puis `--intercept-loopback` (`MIMIC_INTERCEPT_LOOPBACK=true`) ; puis en namespace réseau (`scripts/e2e-netns.sh`), règles par défaut, sidecar en mock | génération ; connexion TCP de l'application vers un serveur de `127.0.0.1` | par défaut, `-A MIMIC_OUTBOUND -o lo -j RETURN` en tête des règles sortantes du §3.1, après les `RETURN` de Mimic et du mesh (donc après le trajet entrant du mesh, inchangé) et avant toute redirection sortante, sans `! -d 127.0.0.1/32` ni `! -d ::1/128` ; avec l'option, `-o lo ! -d 127.0.0.1/32 -j RETURN` (`::1/128` en IPv6) à la même place ; en namespace réseau, la connexion arrive à son serveur sans passer par le sidecar (aucun refus compté) |

**Proxy (PRX)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| PRX-01 | mode record, upstream factice | 10 000 requêtes | toutes relayées sans altération, toutes enregistrées |
| PRX-01b | mode record, body JSON avec accents, query encodée `q=a%20b&page=2` | requêtes | body et query relayés à l'identique, query décodée dans l'échange enregistré |
| PRX-02 | réponse streamée de 500 MiB | relais | mémoire du proxy < 64 MiB, body tronqué dans le store |
| PRX-03 | client et upstream gRPC en h2c | relais | HTTP/2 de bout en bout, `te: trailers` et trailers (`grpc-status`) relayés, échange enregistré |
| PRX-04 | trafic TCP non HTTP redirigé (client qui parle en premier, ou serveur qui parle en premier) | relais | tunnel transparent, octets identiques |
| PRX-04b | connexion TLS redirigée | mode record / mode mock | tunnel opaque / connexion refusée, compteur `refused` ; en mock, une connexion TCP non HTTP est refusée de même (`kind="tcp"`, PRX-23) |
| PRX-15 | connexion HTTP redirigée restée muette plus de 300 ms (ouverte d'avance par un pool), en record puis en mock | la requête arrive ensuite sur la même connexion | traitée en HTTP : enregistrée en record ; servie depuis le store en mock sans joindre la dépendance ; aucun tunnel compté |
| PRX-09 | application à l'écoute sur l'IP du pod seulement, puis sur `127.0.0.1` seulement | relais entrant | la première est jointe sur l'IP du pod, la seconde par repli sur `127.0.0.1` ; sans aucune écoute, l'IP du pod est retenue et rien n'est mémorisé |
| PRX-08 | règles iptables posées par `mimic-init` dans un namespace réseau, sidecar sous l'UID 1338 | l'application appelle une dépendance HTTP sans proxy configuré | l'appel est enregistré avec l'authority d'origine, puis servi en mock |
| PRX-05 | bascule record → mock pendant une charge | bascule | aucune requête en erreur, nouvelles requêtes mockées |
| PRX-06 | hôte dans `tls.intercept`, app faisant confiance à la CA | requête HTTPS | échange déchiffré et enregistré |
| PRX-07 | hôte hors `tls.intercept` | requête HTTPS | tunnel opaque, seuls SNI et durée enregistrés |
| PRX-10 | `accept` en échec répété (EMFILE) | boucle d'écoute | 50 ms au moins entre deux tentatives, au plus une ligne de log par seconde ; l'écoute reprend dès que l'erreur cesse |
| PRX-11 | amont injoignable | rafale de 200 requêtes | 200 réponses `502` et `mimic_upstream_errors_total` à 200, mais au plus une ligne de log par seconde, la suivante indiquant le nombre de lignes omises |
| PRX-12 | serveur WebSocket d'écho derrière le sidecar, en entrée et en sortie ; modes `passthrough`, `record`, `mock` et `mock-partial` | client WebSocket : messages texte et binaires, puis fermeture | `101` relayé, chaque message renvoyé à l'identique, fermeture propre des deux côtés ; `mimic_websocket_connections_total{direction, result="relayed"}` compte chaque connexion |
| PRX-13 | mode `record`, connexion WebSocket avec `Authorization: Bearer <secret>` et messages échangés | fermeture, puis lecture du store | un échange par connexion et par sens : `GET`, chemin, `101`, headers redactés (le secret n'apparaît pas), bodies vides (aucune trame), durée ≥ celle de la connexion ; rien n'est enregistré en `passthrough` |
| PRX-14 | mode `mock` avec `unmatched: error` et store vide, puis `mock-partial` ; `mock.websocket` par défaut (`relay`) ; amont WebSocket joignable | poignée de main WebSocket sortante ; puis requête ordinaire `GET` sur le même chemin en `mock` | la connexion est relayée vers l'amont et fonctionne, sans `x-mimic-unmatched` ; la requête ordinaire n'est pas servie par le `101` enregistré (`599`) |
| PRX-16 | `mock.websocket: refuse` ; modes `mock` (avec `unmatched: error`, puis `passthrough`) et `mock-partial` ; serveur WebSocket joignable qui compte les connexions TCP acceptées | poignée de main WebSocket sortante, puis entrante ; mêmes poignées de main en `record` et `passthrough` | sortante en mock : `503`, `x-mimic-websocket: refused`, aucune connexion acceptée par le serveur, rien d'enregistré, `mimic_websocket_connections_total{direction="outbound",result="refused"}` à 1, `mimic_requests_total` avec `status="503"`, aucun `mimic_mock_responses_total` ; entrante : relayée (`101`, écho) ; en `record` et `passthrough`, relayées (`101`) |
| PRX-17 | sidecar piloté par le contrôleur, configuré en `relay` ; `MimicWorkload` en `mock` avec `spec.mock.websocket: refuse`, puis sans `spec.mock` | battements, poignée de main WebSocket sortante | le sidecar applique `refuse`, l'annonce dans son battement et son statut (`mock_websocket`), le pod est aligné ; la poignée de main reçoit `503` sans joindre le serveur ; sans `spec.mock`, retour au `relay` de sa configuration et `101` |
| PRX-18 | sidecar démarré, API admin sur un port quelconque de la configuration | `mimic-proxy --probe /healthz` avec la même configuration, puis sidecar arrêté | code 0 ; puis code 1 avec un message, sans attendre plus de 2 s |
| PRX-19 | dépendance dont la file d'acceptation est pleine (le SYN reste sans réponse), `outbound.connect_timeout` court ; puis dépendance qui accepte mais finit sa réponse bien après ce délai | requête HTTP sortante, tunnel `CONNECT`, tunnel d'une connexion redirigée vers cette destination ; puis requête vers la dépendance lente | `504` dans le délai et non au délai TCP du système, `mimic_upstream_errors_total{kind="connect_timeout"}` compté pour la requête et le `CONNECT`, tunnel refermé et compté `result="error"` ; la réponse lente est relayée en entier, sans délai sur sa durée. Par défaut, `outbound.connect_timeout` vaut 10 s |
| PRX-20 | `server.header_read_timeout` court | connexion qui envoie une partie des en-têtes puis se tait ; connexion keep-alive restée inactive après une réponse ; réponse streamée plus longue que le délai | les deux premières fermées par le sidecar après le délai ; la réponse streamée est relayée en entier. Par défaut, `server.header_read_timeout` vaut 60 s |
| PRX-21 | `server.max_connections: 2`, deux connexions ouvertes sur l'entrant | troisième connexion, sur l'entrant puis sur le sortant ; `/healthz` ; puis fermeture d'une des deux | la troisième est refermée sans réponse et comptée dans `mimic_connections_refused_total{listener}` ; `/healthz` répond ; une nouvelle connexion est ensuite servie. Par défaut, `server.max_connections` vaut 2 048 |
| PRX-22 | `share.interval`, `share.mode_poll`, `outbound.connect_timeout` ou `server.header_read_timeout` à 0 ; `server.max_connections`, `capture.max_inflight_bytes` ou `share.max_snapshot_bytes` à 0 | démarrage | configuration refusée, avec un message qui nomme le champ |
| PRX-23 | dépendance TCP qui compte les connexions acceptées et renvoie ce qu'elle reçoit ; connexion sortante redirigée non HTTP où le client parle en premier, puis connexion TLS non interceptée ; modes `record`, `mock` (`mock.tcp` absent), `mock` avec `mock.tcp: relay`, `mock-partial` ; même connexion TCP en entrée en `mock` ; puis 50 connexions TCP sortantes en `mock` en rafale | relais | `mock` par défaut : connexion refermée, la dépendance ne compte aucune connexion, `mimic_passthrough_connections_total{direction="outbound",kind="tcp",result="refused"}` incrémenté, log `warn` qui nomme la destination (adresse et port) et `mock.tcp` ; TLS refusé de même (`kind="tls"`) dans les deux réglages ; dans les autres cas, octets relayés à l'identique, compteur `relayed` ; rafale : 50 refus comptés, au plus une ligne de log par seconde, la suivante avec le nombre de lignes omises. Vérifié aussi en namespace réseau, avec de vraies règles iptables |
| PRX-24 | connexion sortante redirigée restée muette, attente de l'application raccourcie ; dépendance qui parle la première et compte ses connexions ; modes `mock` (`mock.tcp` absent), `mock` avec `mock.tcp: relay`, `mock-partial` ; puis, en `mock`, une connexion qui envoie du HTTP pendant l'attente | l'application ne dit rien jusqu'à la fin de l'attente | `mock` : connexion refermée à la fin de l'attente, jamais avant, aucune connexion vers la dépendance, refus compté (`kind="tcp"`), log qui nomme la destination et la connexion muette ; `relay` et `mock-partial` : dépendance jointe à la fin de l'attente seulement, son salut relayé, compteur `relayed` ; une connexion qui parle HTTP pendant l'attente est rendue pour être traitée en HTTP, sans refus (PRX-15) |

**Injection (INJ)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| INJ-01 | pod avec le label `app.kubernetes.io/name: checkout` et un conteneur sur le port 8080 | admission | `mimic-init` en tête avec `MIMIC_INBOUND_PORTS=8080`, native sidecar `mimic` juste après (`restartPolicy: Always`, UID 1338, `startupProbe` sur `/readyz`), `MIMIC_WORKLOAD=checkout`, volumes, annotation et label ajoutés |
| INJ-02 | pod déjà injecté | nouvelle admission | aucun patch |
| INJ-03 | annotation `mimic.platform.io/inject: "false"`, ou `hostNetwork` | admission | aucun patch, un avertissement |
| INJ-04 | pod sans nom de workload | admission | aucun patch, un avertissement |
| INJ-05 | annotation `mimic.platform.io/tls-intercept: api.stripe.com` | admission | `MIMIC_TLS_INTERCEPT` dans le sidecar ; CA montée et variables de confiance dans l'app, sans écraser une variable existante |
| INJ-06 | annotations `inbound-ports`, `exclude-outbound-cidrs`, `exclude-outbound-ports` et `config` | admission | valeurs reprises par `mimic-init`, ConfigMap monté à la place de la configuration en ligne |
| INJ-07 | pod avec ses propres init containers | admission | `mimic-init` puis `mimic` passent avant eux, leur ordre est conservé |
| INJ-08 | `AdmissionReview` d'un pod | `POST /mutate` en HTTPS | `allowed: true`, même `uid`, patch JSON qui s'applique au pod et donne le pod de INJ-01 |
| INJ-09 | `MIMIC_CONFIG_YAML` et `MIMIC_TLS_INTERCEPT` définis, sans fichier | démarrage du sidecar | configuration lue comme depuis un fichier, puis surchargée par l'environnement |
| INJ-10 | pod quelconque, puis `version_label: app.example.com/release` dans la configuration du webhook | admission | `MIMIC_VERSION` lu par l'API downward dans `metadata.labels['app.kubernetes.io/version']`, puis `['app.example.com/release']` ; `MIMIC_POD_TEMPLATE_HASH` dans `metadata.labels['pod-template-hash']` |
| INJ-11 | pod déjà injecté par Istio (`istio-init`, `istio-proxy` en native sidecar) ; pod injecté par Linkerd (`linkerd-init`, `linkerd-proxy` dans les conteneurs) | admission | injecté ; `mimic-init` et `mimic` juste après les init containers du mesh, les autres init containers ensuite ; `MIMIC_MESH` `istio` ou `linkerd` ; sondes `exec` `mimic-proxy --probe` |
| INJ-12 | pod injecté par Mimic sans mesh, puis Istio ajoute ses conteneurs en tête ; ou les ajoute derrière ceux de Mimic | réinvocation | patch : `MIMIC_MESH=istio`, sondes `exec`, conteneurs du mesh avant ceux de Mimic ; nouvelle réinvocation : aucun patch |
| INJ-13 | annotation `mimic.platform.io/mesh: istio-ambient`, puis `none` sur un pod Istio, puis une valeur inconnue | admission | `MIMIC_MESH` reprend l'annotation ; valeur inconnue : pod injecté en `auto`, avec un avertissement |
| INJ-14 | webhook servi en HTTPS, `tls.reload_interval` court ; certificat et clé remplacés sur disque (renouvellement) ; puis remplacés par un contenu illisible | poignées de main suivantes | le nouveau certificat est présenté sans redémarrage ; le contenu illisible est ignoré, l'ancien certificat reste servi ; au démarrage, un certificat illisible est refusé |
| INJ-15 | serveur du plan de contrôle (`mimic-net`) en HTTPS ; client TCP qui n'envoie rien ; client TLS qui n'envoie pas d'en-têtes ; `accept` en échec répété | délais de 10 s (ici raccourcis) | connexion fermée après le délai de poignée de main, puis après celui des en-têtes ; 50 ms au moins entre deux `accept` en échec, une ligne de journal au plus par seconde |
| INJ-16 | binaire `mimic-injector` démarré | `SIGTERM` | sortie en code 0 en moins de 5 s |
| INJ-17 | `KUBERNETES_SERVICE_HOST=10.100.0.1` au chargement de la configuration, `exclude_outbound_cidrs: [10.0.0.0/8]`, annotation `exclude-outbound-cidrs: 10.100.0.1/32,192.168.0.0/16` ; puis `KUBERNETES_SERVICE_HOST=fd00::1` ; puis `ipv6: true` | admission | `MIMIC_EXCLUDE_OUTBOUND_CIDRS=10.100.0.1/32,10.0.0.0/8,192.168.0.0/16` ; puis `fd00::1/128` en tête ; `MIMIC_IPV6=true` seulement avec `ipv6: true` ; CIDR invalide dans la configuration refusé au démarrage |
| INJ-18 | annotations `inbound-ports: 8080,http,70000`, `exclude-outbound-cidrs: 10.0.0.0/8,bad,10.1.0.0/33`, `exclude-outbound-ports: 5432,x`, une valeur avec un saut de ligne suivi d'une règle ; puis `inbound-ports: http` | admission | `MIMIC_INBOUND_PORTS=8080`, `MIMIC_EXCLUDE_OUTBOUND_CIDRS` sans les valeurs invalides, `MIMIC_EXCLUDE_OUTBOUND_PORTS=5432` ; un avertissement d'admission par valeur ignorée, aucune n'atteint le pod ; `inbound-ports` sans port valide : ports déclarés |
| INJ-19 | pod injecté | admission | `mimic-init` : root, `NET_ADMIN` et `NET_RAW` seules, `allowPrivilegeEscalation: false`, `seccompProfile: RuntimeDefault`, `resources` (`init_resources`) ; sidecar : `seccompProfile: RuntimeDefault` ; volume `mimic-controller-ca` en `optional: true`, de même dans `deploy/example/deployment.yaml` |
| INJ-20 | manifests de `deploy/injector/` et de `deploy/example/` | lecture | image `ghcr.io/thaliph/mimic:0.1.0` partout (aucune `registry.example.com/platform`) ; injecteur en UID et GID 1338 ; anti-affinité préférentielle par nœud et `priorityClassName: mimic-control-plane`, PriorityClass fournie ; webhook limité au label `mimic.platform.io/enabled` et excluant `kube-system` et `mimic-system` par `kubernetes.io/metadata.name` ; aucun CIDR codé en dur dans la configuration du webhook |
| INJ-21 | pod injecté avec la configuration en ligne de l'injecteur ; puis `share.startup.timeout: 90` dans `sidecar_config` ; puis annotation `mimic.platform.io/config` ; pod d'un mesh ; réinvocation ; `deploy/example/` | admission ; lecture | `startupProbe` sur `/readyz`, période 1 s, `failureThreshold` 60, puis 120, puis 330 ; `readinessProbe` sur `/readyz`, `livenessProbe` sur `/healthz` ; sondes `exec` (`--probe /readyz` au démarrage) dans le mesh, mêmes seuils après réinvocation ; `deployment.yaml` porte la `startupProbe` `/readyz` à 60 échecs, cohérente avec le `share.startup.timeout` de `configmap.yaml` |

**CLI (CLI)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| CLI-01 | `mimicctl mode checkout mock -n shop` | construction de la requête | apply d'une `MimicWorkload` `shop/checkout` avec `spec.mode: mock`, sans snapshot |
| CLI-02 | mode inconnu | `mimicctl mode checkout fast` | refus avant tout appel, avec la liste des modes valides |
| CLI-03 | export d'une dépendance, puis requêtes en k6 | construction du chemin | `/v1/workloads/shop/checkout/openapi/outbound/api.stripe.com`, `/v1/workloads/shop/checkout/requests?direction=inbound&format=k6&limit=100` |
| CLI-04 | un workload avec 12 opérations (service et dépendance `quote:8080`), une requête enregistrée de `GET /items/{id}` | `mimicctl report --top 10` | les deux API avec leurs opérations et appels ; exactement 10 opérations, triées par appels décroissants ; les headers de la requête, `\|` échappé et valeurs longues tronquées ; une opération sans requête liste ses paramètres header |
| CLI-05 | `MimicWorkload` en génération 5, statut publié pour la génération 4 (2/2 alignés, snapshot) | `mimicctl status` | snapshot `?` et pods alignés `?/2` ; une fois la génération 5 publiée, le snapshot et `2/2` |
| CLI-06 | namespace labellisé `env=production` | `mimicctl mode checkout mock` (ou `mock-partial`) | refus avant l'apply, message qui nomme le namespace et `env=production`, code de sortie 2 ; `record` et `passthrough` acceptés ; labels illisibles : accepté |
| CLI-07 | `mimicctl mode checkout mock --selector pod-template-hash=7d9f,track=green --capture-inbound metadata` ; puis `--selector track` ou `--capture-inbound fast` ; `status` d'une ressource refusée | construction de la requête, tableau | `spec.selector.matchLabels` à deux entrées et `spec.capture.inbound: metadata` ; sélecteur ou capture invalides refusés avant tout appel ; `MODE` suivi de `(refusé)` |
| CLI-08 | `mimicctl mode checkout mock --mock-websocket refuse` ; puis `--mock-websocket drop` ; puis sans l'option | construction de la requête | `spec.mock.websocket: refuse` ; valeur inconnue refusée avant tout appel, avec les valeurs valides ; sans l'option, pas de `spec.mock` |
| CLI-09 | pods du contrôleur : une passive puis l'active marquée `mimic.platform.io/leader: "true"`, ou une passive en marche et une active marquée qui ne l'est plus ; puis aucun pod marqué (contrôleur d'une version antérieure) | choix du pod de l'interface (`export`, `requests`, `report`) | la réplique active marquée et en marche, même listée après la passive ; sans pod marqué en marche, le premier pod en marche ; aucun pod en marche : erreur |
| CLI-10 | authorities connues `api.stripe.com:443`, `quote:8080`, `quote:9090` | `export --outbound` avec `api.stripe.com:443`, puis `api.stripe.com`, puis `quote`, puis `inconnu.io` (résolution) | `api.stripe.com:443` deux fois ; `quote` refusé, message qui cite `quote:8080` et `quote:9090` ; `inconnu.io` refusé, message qui liste les trois authorities ; l'aide de `--outbound` cite un exemple `hôte:port` |
| CLI-11 | `MimicWorkload` avec `status.unmatched: 7`, puis sans le champ (contrôleur antérieur) ; workload dont `/v1/workloads` donne un relevé `unmatched` de 2 clés (`599` et `413`), puis aucun | `mimicctl status`, `mimicctl report` | colonne `UNMATCHED` à 7, puis 0, avant `ALIGNED` ; le rapport donne le total dans son tableau, puis une section des requêtes sans réponse avec méthode, API, forme du chemin, raison, occurrences et dernière occurrence en UTC, dans l'ordre du relevé ; sans relevé, la section dit qu'il n'y en a pas ; rapport identique d'une exécution à l'autre |
| CLI-12 | listing de snapshots (identifiant, date, échanges, épinglé), `MimicWorkload` avec `status.snapshot` posé | `mimicctl snapshot ls` | tableau du plus récent au plus ancien, colonne `REFERENCED` vraie pour le snapshot courant, `PINNED` d'après le listing |
| CLI-13 | `mimicctl snapshot pin checkout abc123`, puis `--unpin` ; identifiant invalide | construction de la requête | `POST …/snapshots/abc123/pin` avec `{"pinned":true}`, puis `{"pinned":false}` ; identifiant invalide refusé avant tout appel |
| CLI-14 | `mimicctl snapshot export checkout abc123 -o s.jsonl` ; sans `-o` | construction de la requête, écriture | `GET …/snapshots/abc123` ; contenu écrit dans `s.jsonl`, ou sur la sortie standard sans `-o` |
| CLI-15 | fichier local JSONL valide, puis illisible | `mimicctl snapshot import checkout fichier.jsonl` | `POST …/snapshots` avec le contenu du fichier ; fichier illisible refusé avant tout appel |
| CLI-17 | chemins de l'interface de `export`, `requests`, `report`, `snapshot ls`, `snapshot export` ; réponses de l'API server `404`, `403`, `503` | traduction vers l'API agrégée ; décision de repli | `/v1/workloads/shop/checkout/openapi/outbound/api.stripe.com:443` devient `/apis/data.mimic.platform.io/v1alpha1/namespaces/shop/workloads/checkout/openapi/outbound/api.stripe.com:443` (query gardée), `/v1/workloads` devient `…/workloads` (`-A`) ou `…/namespaces/shop/workloads` ; repli sur le port-forward pour le seul `404` |
| CLI-18 | namespace `shop` : pods injectés des workloads `checkout` et `web`, un pod non injecté `batch` ; aucune `MimicWorkload` | `mimicctl mode chekout mock`, puis `mimicctl mode checkout mock`, `mode batch mock`, `mode chekout mock --force`, et `mode` sur une `MimicWorkload` existante sans pod | `chekout` : refusé (code 2), message qui cite `checkout` et `web` ; `checkout` : appliqué ; `batch` : refusé (non injecté) ; `--force` : appliqué ; `MimicWorkload` existante : appliqué |
| CLI-19 | — | `mimicctl requests checkout --direction outbound --format jsonl`, puis `--direction sideways` | chemin `…/requests?direction=outbound&format=jsonl&limit=1000` ; `sideways` refusé avant tout appel |
| CLI-16 | réponse `{"deleted": ["a", "b"]}`, puis `{"deleted": []}` | `mimicctl snapshot gc checkout` | affiche les deux identifiants supprimés, un par ligne ; puis « rien à purger » |

**Conversion OpenAPI (OAS)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| OAS-01 | document 3.1 avec `type: [string, "null"]` et des `examples` de schéma | conversion en 3.0 | `openapi: 3.0.3`, `type: string` avec `nullable: true`, `example` ; aucun type tableau ni `examples` de schéma ne subsiste |

**Diff de specs (DIF)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| DIF-01 | deux fois le même document | diff | aucune différence |
| DIF-02 | opération supprimée, opération ajoutée | diff | les deux listées, seule la suppression est cassante |
| DIF-03 | paramètre de query devenu requis, paramètre facultatif ajouté | diff | le premier cassant, le second non |
| DIF-04 | code 404 ajouté, code 200 supprimé | diff | l'ajout n'est pas cassant, la suppression du 200 l'est |

**Tir de charge (LOD)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| LOD-01 | échange entrant avec query, body binaire, `authorization` redacté, `connection` et `host` ; `--header 'authorization: Bearer t'` | requête à rejouer | méthode, chemin, query et body identiques ; ni `connection`, ni `host`, ni la valeur redactée ; `authorization: Bearer t` ; les échanges sortants sont ignorés, un body tronqué écarte l'échange et est compté |
| LOD-02 | corpus de trois requêtes enregistrées dans le désordre | requêtes 0 à 6 | ordre d'enregistrement, en boucle : 0, 1, 2, 0, 1, 2, 0 |
| LOD-03 | `--start 1 --factor 2 --max 20` ; `--start 4 --factor 3 --max 4` ; `--factor 1` | paliers | `1, 2, 4, 8, 16, 20` ; `4` ; refus (facteur > 1 exigé) |
| LOD-04 | latences de 1 µs à 60 s, deux histogrammes fusionnés | p50, p95, p99, max | à moins de 1 % des valeurs exactes ; la fusion vaut l'histogramme de toutes les mesures |
| LOD-05 | paliers 1, 2, 4 : débits 100, 190, 370, erreurs 0 %, 0 %, 3 % | verdict | palier 4 saturé (erreurs), capacité au palier 2 |
| LOD-06 | `--max-p99 200ms`, p99 de 50, 120, 400 ms | verdict | saturé au troisième palier (p99), capacité au deuxième |
| LOD-07 | débits 100, 195, 200 pour les concurrences 1, 2, 4 | verdict | troisième palier saturé (plateau : +2,6 % de débit pour une concurrence doublée, seuil 10 %), capacité au deuxième, le plus haut débit non saturé |
| LOD-08 | quantités `250m`, `1`, `1500000n`, `128Mi`, `1Gi`, `500k`, `12345` ; relevés d'un pod avec les conteneurs `app`, `mimic`, `istio-proxy`, dont un au premier tiers du palier | lecture, agrégation, recommandation à 30 % / 20 % | 0,25, 1, 0,0015 cœur ; 134217728, 1073741824, 500000, 12345 octets ; seul `app` compte, relevé précoce ignoré ; CPU et mémoire arrondis au millicœur et au MiB supérieurs |
| LOD-09 | serveur local qui compte les requêtes en cours | `--concurrency 4 --requests 200` | exactement 200 requêtes reçues, jamais plus de 4 en cours, 4 atteint ; rapport d'un palier sans erreur |
| LOD-10 | workload en `record` ; en `mock` avec 1 pod aligné sur 2 ; en `mock` aligné | `mimicctl load <workload>` | refus code 2 dans les deux premiers cas, avec la raison ; tir dans le dernier ; `--allow-live-dependencies` lève le refus |
| LOD-11 | serveur local qui traite au plus 4 requêtes à la fois (5 ms chacune) | `--ramp --start 1 --factor 2 --max 64` | montée arrêtée au palier 8, saturé (plateau), sans jouer les suivants ; capacité au palier 4 |
| LOD-12 | mêmes paliers mesurés | rapport texte, Markdown, JSON | identiques d'une exécution à l'autre ; le JSON se relit |

**E2E (E2E)**

| ID | Scénario |
|----|----------|
| E2E-01 | *Non automatisé : la démo OpenTelemetry (E2E-05) couvre la découverte et l'export ; le stockage MinIO reste à scénariser.* Pod demo (app + Mimic injecté) sur kind, app appelant un faux SaaS HTTPS ; mode record ; la spec inbound et la spec du SaaS sont exportées sur MinIO. |
| E2E-02 | *Couvert en partie par E2E-05 (dépendance arrêtée, toutes les requêtes réussissent) et E2E-07 (NetworkPolicy qui ne laisse sortir que le DNS et le contrôleur).* Même pod en mode mock avec NetworkPolicy deny-all egress : le tir k6 réussit à > 99,9 %, aucun paquet ne sort (vérifié par les compteurs du faux SaaS). |
| E2E-03 | Bascule `record → mock` par la `MimicWorkload` (`mimicctl mode`), sans redémarrage du pod : les sidecars s'alignent et le nombre de redémarrages des pods ne change pas. Sous contrôleur, l'annotation de mode est ignorée (§5.1). Vérifié dans `scripts/e2e-kind.sh` lors de la bascule de shipping (E2E-05). |
| E2E-04 | Namespace labellisé `env=production` : `mimicctl mode … mock` est refusé (code 2) ; une `MimicWorkload` en `mock` créée avant le label passe à `Allowed=False` (raison `ProductionNamespace`) dès que le label est posé ; avec la `ValidatingAdmissionPolicy` de `deploy/controller/production-guard.yaml`, une `MimicWorkload` en mock y est refusée à l'admission. Vérifié en fin de `scripts/e2e-kind.sh` ; sans cluster, par CRD-07 et CLI-06. |
| E2E-05 | Démo OpenTelemetry sur kind (`scripts/e2e-kind.sh`, à chaque PR vers `main`) : les services de la démo, sans la chaîne de télémétrie ni Kafka, avec son générateur de charge actif ; tous injectés par le webhook sauf `valkey-cart` (opt-out). Les requêtes du test passent par le frontend (`/api/shipping`), qui appelle shipping (2 réplicas) en gRPC, qui appelle quote en HTTP ; en record les deux API de shipping (gRPC entrant, quote sortant) sont découvertes par le contrôleur. Après cinq minutes de charge, `mimicctl report -A` est publié dans le résumé du job et les documents OpenAPI en artefact. Tous les workloads injectés passent ensuite en mock, sauf le générateur (le client) : une fois les pods alignés, pendant 60 s, `mimic_requests_total` des sidecars ne compte plus aucune requête entrante, sauf sur les services que le générateur appelle lui-même (frontend-proxy, et flagd pour ses feature flags) ; frontend-proxy reçoit son trafic et répond depuis le snapshot (`mimic_mock_responses_total`) ; le bilan est ajouté au résumé du job, puis retour en record. Après `mimicctl mode shipping mock`, les deux pods s'alignent sur un snapshot, quote est arrêté et toutes les requêtes réussissent ; retour en record. |
| E2E-07 | WebSocket en mock, en fin de `scripts/e2e-kind.sh` (`tests/e2e/websocket.yaml`) : un pod injecté `ws-client` (curl) et un serveur WebSocket non injecté `ws-echo`, qui compte les connexions et les poignées de main reçues. Avec `mimicctl mode ws-client mock` (`relay`, le défaut), la poignée de main sortante de `ws-client` aboutit (`101`), le serveur la compte et `mimic_websocket_connections_total{direction="outbound",result="relayed"}` augmente. Puis, sous une NetworkPolicy qui ne laisse sortir de `ws-client` que le DNS et le contrôleur, `mimicctl mode ws-client mock --mock-websocket refuse` : une fois le sidecar sur `refuse` et le pod aligné, la poignée de main reçoit `503` avec `x-mimic-websocket: refused`, le serveur ne voit aucune nouvelle connexion et `result="refused"` augmente. |
| E2E-06 | Bascule du contrôleur sur kind (`scripts/e2e-kind.sh`) : contrôleur en deux réplicas sur un PVC `ReadWriteOnce` (un seul nœud) ; shipping en mock sur un snapshot, quote arrêté, trafic continu par le frontend. `kubectl delete pod` de la réplique active : en moins de 30 s, l'autre détient le bail, porte la marque et est le seul endpoint du Service, et les deux pods de shipping s'y alignent de nouveau sur le même snapshot (battements repris) ; le snapshot est servi par la nouvelle active à l'identique (même empreinte des octets) ; les sidecars ne l'ont ni retéléchargé ni perdu (`mimic_mock_snapshot_exchanges` inchangé) ; toutes les requêtes du trafic réussissent ; la découverte de shipping et son `exemplars.jsonl` sont toujours là ; la nouvelle active reçoit de nouveau des segments ; le Deployment revient à deux réplicas prêtes, dont une passive (`mimic_controller_leader 0`). |
| E2E-08 | Istio sur kind (`scripts/e2e-mesh.sh istio`, à chaque PR vers `main`) : profil ambient, un namespace en sidecar et un en ambient, `PeerAuthentication` `STRICT` dans chacun. Un client (curl) et un serveur HTTP, tous deux injectés par le mesh et par Mimic. Le client appelle le serveur par son Service : réponses 200 ; un pod hors mesh est refusé par le serveur, preuve que le trafic du client passe par le mTLS du mesh. Les sidecars Mimic du client et du serveur ont enregistré l'échange (`mimicctl export`). Client en mock et serveur arrêté : le client reçoit la même réponse, servie par son sidecar. |
| E2E-09 | Linkerd sur kind (`scripts/e2e-mesh.sh linkerd`) : même scénario, politique entrante `all-authenticated` à la place de `STRICT`. |

**Documentation (DOC)**

La documentation d'exploitation (`docs/guide/`, `docs/`) décrit le comportement du code livré : un écart est un défaut, vérifié sans cluster.

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| DOC-01 | registre de métriques du sidecar, `docs/guide/Metriques.md` | lecture | chaque métrique enregistrée y figure sous le nom exposé (`_total` pour un compteur) |
| DOC-02 | registres du contrôleur et du webhook, `docs/guide/Metriques.md` | lecture | idem ; et tout nom `mimic_*` cité par le guide (suffixes `_total`, `_bucket`, `_sum`, `_count` compris) est une métrique enregistrée ou un nom de crate : aucune métrique inventée dans une requête PromQL |
| DOC-03 | `deploy/controller/controller.yaml`, `docs/guide/` | lecture | l'UID et le GID, la mémoire demandée et la limite mémoire du contrôleur cités par le guide sont ceux du manifest |
| DOC-04 | NetworkPolicy du contrôleur, `deploy/example/vmpodscrape-controller.yaml`, `docs/guide/Metriques.md` | lecture | le guide donne le port ouvert au monitoring (8443) comme port de collecte du contrôleur, et ne le déclare pas non collectable |
| DOC-05 | manifests de `deploy/`, `docs/` | lecture | aucune image `registry.example.com/platform/mimic` ; l'image mimic citée par la documentation est celle des manifests |
| DOC-06 | règles de `deploy/monitoring/rules.yaml`, `docs/guide/Alertes-et-supervision.md` | lecture | chaque alerte a une ligne dans « Conduite à tenir », et réciproquement |
| DOC-07 | `crates/mimicctl/src/main.rs`, `docs/guide/CLI-mimicctl.md` | lecture | chaque sous-commande et chaque option longue de `mimicctl` y figure ; `docs/cli.md` n'existe plus (une seule page de référence) |
| DOC-08 | `docs/guide/Developpeur-applicatif.md`, code du sidecar, `deploy/controller/rbac.yaml` | lecture | la page existe et le sommaire la cite ; elle a un glossaire et une section « Ce que la plateforme doit fournir » qui nomme le ClusterRole `mimic-data-viewer` livré ; chaque en-tête `x-mimic-*` posé par le sidecar sur une réponse et chaque corps `mimic: …` d'une réponse d'erreur du sidecar y figurent, avec son code (`599`, `502`, `504`, `413`, `503`, `400`) |

**Supervision (MON)** (§9.3, §9.6) :

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| MON-01 | sidecar, contrôleur, webhook | `GET /metrics` | `mimic_build_info{mimic_version="<version du workspace>",revision="…"} 1` |
| MON-02 | contrôleur, deux workloads, un pod aligné dans l'un et un pod qui diverge dans l'autre | `GET /metrics`, puis les pods oubliés (`pod_forget`) | `mimic_controller_pods{workload="<ns>/<nom>",state}` pour les trois états de chaque workload, aux bons comptes ; puis les trois séries de chaque workload à 0 |
| MON-03 | webhook avec `sidecar_log`, pod avec ou sans `mimic.platform.io/log` | injection | `MIMIC_LOG` du sidecar : l'annotation, sinon `sidecar_log`, sinon absente ; une annotation invalide est ignorée avec un avertissement d'admission ; un `sidecar_log` invalide est refusé au chargement de la configuration |
| MON-04 | serveur TLS relu à chaud (contrôleur, webhook) | démarrage, puis nouveau certificat | la date d'expiration exposée est celle du certificat servi, puis celle du nouveau ; un certificat refusé ne la change pas |
| MON-05 | sidecar avec `store.dir` et `MIMIC_STORE_SIZE_LIMIT_BYTES` ; injection d'un pod | `GET /metrics` ; mutation | `mimic_store_disk_bytes` vaut la taille des fichiers du store, `mimic_store_disk_limit_bytes` la limite ; le webhook pose `MIMIC_STORE_SIZE_LIMIT_BYTES` égale au `sizeLimit` de l'`emptyDir` du store |
| MON-06 | `deploy/monitoring/` | lecture | `spec.groups` de la `PrometheusRule` égale `rules.yaml` ; toute métrique `mimic_*` des règles est enregistrée ; toute règle sur `mimic_controller_pods` agrège par `workload` ; le guide ne recopie pas les règles |
| MON-07 | `deploy/monitoring/` | `scripts/check-monitoring.sh` (CI) | `promtool check rules` et `promtool test rules` passent |

**Performance (PRF)** : seuils bloquants en CI nightly : O1 et O2 mesurés avec `oha` à 1 000 req/s sur 60 s, comparés à une exécution sans sidecar.

### 11.3 Fixtures

Un corpus d'échanges versionné dans `tests/fixtures/` sert aux tests golden et à l'inférence :

- `petstore/` : API REST classique (CRUD, pagination, 404) ;
- `saas-like/` : API type Stripe (form-urlencoded, idempotency keys, 429) ;
- `edge-cases/` : bodies vides, JSON invalide, chemins avec encodage, query répétées, headers multi-valeurs.

Un petit générateur (`cargo xtask gen-fixtures`) produit ces corpus à partir d'un serveur `axum` de référence, pour que les fixtures soient reproductibles.

---

## 12. Questions ouvertes

Ces points changent la conception ; les défauts ci-dessus restent appliqués tant qu'ils ne sont pas tranchés.

| # | Question | Défaut retenu |
|---|----------|---------------|
| Q1 | Interception : proxy transparent iptables (`NET_ADMIN` dans le pod) acceptable, ou faut-il viser sans privilège (proxy explicite) ou eBPF ? | iptables |
| Q2 | TLS sortant vers les SaaS : peut-on faire confiance à une CA Mimic dans les apps (MITM), avec quels runtimes majoritaires (JVM, Go, Node, Python) ? | MITM opt-in par hôte |
| Q3 | Service mesh présent sur les clusters cibles ? Lequel ? | tranché : Istio (sidecar et ambient) et Linkerd, Mimic entre l'application et le mesh (§3.3) |
| Q4 | Payloads : a-t-on le droit de stocker des exemples redactés, et où (S3, PVC, autre) ? | S3 chiffré, redaction par défaut |
| Q5 | Protocoles à couvrir au-delà d'HTTP (gRPC, Kafka, bases de données) ? | tranché : HTTP et HTTPS uniquement |
| Q6 | Version minimale de Kubernetes (native sidecar à partir de 1.29) ? | ≥ 1.29 |
| Q7 | Génération automatique de scénarios k6 depuis la spec inbound : dans le périmètre ? | oui, phase 5 |

---

## 13. Plan de livraison

Chaque phase commence par l'écriture de ses tests (en rouge), puis de l'implémentation jusqu'au vert.

| Phase | Contenu | Tests de sortie |
|-------|---------|-----------------|
| P0 | Workspace, CI (fmt, clippy, test, deny), modèle `Exchange`, fixtures | build vert |
| P1 | `mimic-core::infer` + `redact` sur fixtures (aucun réseau) | INF-*, RED-* |
| P2 | `mimic-core::mock` : matching, stratégies, latence | MCK-01 à 11 (niveau cœur) |
| P3 | Proxy HTTP en clair : record et mock, store, API et page d'admin, métriques | PRX-01 à 05, MCK-* de bout en bout, ADM-* |
| P3b | Partage entre sidecars d'un workload | SHR-* |
| P3c | Sidecar borné : réservoir par opération, budget mémoire, journal local segmenté, file de partage bornée, logs limités | CAP-*, SHR-06, SHR-07, PRX-10, PRX-11 |
| P3d | Mock fidèle et concurrent : moteur indexé par forme de chemin et sans verrou, construit à la bascule, latence comptée depuis l'arrivée avec un minuteur précis, réponse préparée une fois, redaction sans réécriture des bodies intacts | MCK-13 à 18, RED-05 |
| P3e | Correctifs fonctionnels : relais WebSocket dans tous les modes, poignée de main enregistrée, jamais rejouée ; énumérations inférées seulement sur un échantillon suffisant | PRX-12 à 14, MCK-19, INF-09 |
| P3f | Chemin chaud du sidecar : métriques sans allocation, curseurs de `round-robin` et `sequence` bornés, mock-partial sans reconstruction du moteur par échange relayé | MCK-21, MCK-22, ADM-05 |
| P3g | WebSocket en mock réglable : `mock.websocket: relay \| refuse` dans la configuration du sidecar, la `MimicWorkload` (`spec.mock.websocket`) et `mimicctl mode --mock-websocket` ; refus en `503` compté par `mimic_websocket_connections_total{result="refused"}` | MCK-23, PRX-16, PRX-17, CRD-10, CLI-08, E2E-07 |
| P3h | Sidecar durci pour la production : délai de connexion vers l'amont, délai de lecture des en-têtes, plafond de connexions, admin réservé à la boucle locale hors sondes et métriques, budget des bodies en vol, taille des snapshots bornée, cardinalité de `method`, validation des périodes, avertissement sans sel de redaction (§3.5, §5.2, §5.6, §9.2) | PRX-19 à 22, SEC-07, CAP-11, SNP-08, ADM-06, RED-06 |
| P3i | Mock étanche : en `mock`, TCP sortant non HTTP et connexions muettes refusés par défaut (`mock.tcp: refuse \| relay`), refus comptés dans `mimic_passthrough_connections_total{result="refused"}` et journalisés avec la destination, en débit limité ; trafic local du pod non intercepté, comme sous Istio (`MIMIC_INTERCEPT_LOOPBACK` pour les tests) ; guides et parcours SRE corrigés (§3.1, §3.4, §9.1, §9.3) | MCK-24, PRX-23, PRX-24, INI-12 |
| P3j | Requêtes sans réponse visibles : `599` et `413` du mock journalisés en débit limité avec la forme du chemin, comptés (`match="too_large"`), relevés par le sidecar (`GET /api/unmatched`, borné, agrégé par clé), remontés par le battement jusqu'à `/v1/workloads`, la page du workload, `status.unmatched`, `mimicctl status` et `mimicctl report` ; `mimicctl export --outbound` accepte un hôte sans port (§5.3, §9.2, §9.2b, §9.5) | MCK-25 à MCK-27, RED-07, ADM-07, CTL-42, CLI-10, CLI-11 |
| P4 | `mimic-init` iptables, détection de protocole (connexions muettes comprises), image, manifests d'exemple, E2E en namespace réseau | INI-*, PRX-04, PRX-08, PRX-15 ; E2E kind ensuite |
| P4b | Service mesh : Istio sidecar, Istio ambient et Linkerd ; détection dans les règles posées, Mimic entre l'application et le mesh, ports et UID hors de ceux des mesh, injection après le mesh et réinvocation, sondes `exec` | INI-05 à INI-10, PRX-18, INJ-11 à INJ-13, E2E-08, E2E-09 |
| P5 | TLS MITM sortant | PRX-06, 07 |
| P6 | Contrôleur : ingestion, interface centrale, snapshots, CRD `MimicWorkload`, authentification, requêtes rejouables | MCK-12, SNP-*, CTL-*, CRD-*, SEC-*, RPL-* |
| P6b | Chargement cross-environnement, garde-fou production (contrôleur, `mimicctl`, politique d'admission facultative) | E2E-04, CRD-07, CLI-06 |
| P6c | Contrôleur à l'échelle : index des segments, liste incrémentale, verrous courts, GC et compaction, budget mémoire, ingestion idempotente | CTL-06 à 15, SHR-08, SHR-09 |
| P6d | Parcours SRE de tir de charge : capture entrante réglable, bascule des seuls pods sélectionnés, label `version` des métriques, guide `docs/sre-load-test.md` | CAP-09, CAP-10, CRD-08, CRD-09, CLI-07, ADM-04, INJ-10 |
| P6e | Mock centralisé : plus d'import entre sidecars sous contrôleur (le mock sert le snapshot), exemples par endpoint persistés hors rétention et bornés en nombre et en octets, historique de 24 heures par défaut | SHR-08 (réécrit), SHR-10, CTL-07 (réécrit), CTL-16 à CTL-22 |
| P6f | Snapshot de mock léger : forme de mock du snapshot, compressée, avec `ETag`, identique sur tous les pods ; snapshot complet gardé pour les anciens sidecars, repli d'un sidecar à jour sur un ancien contrôleur | MCK-20, CTL-23, SHR-11 |
| P6g | Contrôleur en haute disponibilité : deux réplicas actif/passif élues par un bail `Lease`, prise de relais après rechargement de l'état persisté, Service vers la seule active, PodDisruptionBudget, anti-affinité, stockage partagé (`s3://` ou PVC `ReadWriteMany` sur plusieurs nœuds) | CTL-24 à CTL-28, CLI-09, E2E-06 |
| P6h | Démarrage d'un pod piloté : barrière de démarrage par la `startupProbe` du native sidecar (`/readyz`), état rechargé avant les écoutes, délai et repli configurables (`share.startup`), métriques des battements, logs d'échec limités | CTL-36 à CTL-41, INJ-21 |
| P7 | Webhook d'injection + `mimicctl` (dont le rapport) | INJ-*, CLI-*, OAS-*, DIF-*, E2E-05 |
| P7b | Plan de contrôle en production : certificats relus à chaud, serveurs bornés (poignée de main, en-têtes, `accept`), arrêt propre du webhook, authentification avant le corps, `/metrics` du contrôleur collectable, erreurs du support distinguées de l'absence, valeurs iptables validées dans le webhook et `mimic-init`, IP de l'API Kubernetes exclue d'office, Pod Security, manifests de production | INI-11, INJ-14 à INJ-20, CTL-29 à CTL-35 |
| P8b | Tir de charge intégré `mimicctl load` : rejeu en boucle fermée, palier fixe ou montée jusqu'à saturation, relevé CPU et mémoire par `metrics.k8s.io`, recommandation de taille, Job d'exemple | LOD-01 à LOD-12 |
| P8 | Génération de scénarios k6 depuis OpenAPI + `x-mimic-stats` | scénario généré exécutable |
| P9 | Guide réaligné sur le code (métriques, image, UID, ressources, port de collecte du contrôleur, certificats relus à chaud, alertes et PromQL corrigées), référence `mimicctl` unique, cohérence doc ↔ manifests vérifiée par des tests | DOC-01 à DOC-07 |
| P10 | Cycle de vie des snapshots : métadonnées versionnées, purge côté contrôleur (`snapshot.keep_last`, référencés et épinglés jamais purgés) et côté sidecar (`store.max_cached_snapshots`), `status.snapshotExchanges` et condition `SnapshotEmpty`, API d'écriture (`pin`, import, purge immédiate) et `mimicctl snapshot ls\|pin\|export\|import\|gc` | SNP-09 à SNP-12, CTL-43 à CTL-46, CLI-12 à CLI-16 |
| P11 | Supervision livrée : `deploy/monitoring/` (`PrometheusRule`, règles brutes, tests `promtool`), `mimic_build_info`, `mimic_controller_pods` par workload, expiration des certificats servis, disque du store, `MIMIC_LOG` réglable par le webhook, alertes d'expiration, de disque et d'éviction | MON-01 à MON-07, DOC-06 (réécrit) |
| P12 | Lecture par namespace et page développeur : API agrégée `data.mimic.platform.io/v1alpha1` autorisée par `SubjectAccessReview`, ClusterRole `mimic-data-viewer`, `mimicctl` par l'API agrégée avec repli sur le port-forward, vérification d'existence avant `mode`, `requests --direction`, page « Développeur applicatif » | CTL-47, CTL-48, SEC-08, CLI-17 à CLI-19, DOC-08 |

P1 et P3 sont du Rust pur sans I/O : c'est le meilleur point d'entrée pour Claude Code, qui peut avancer en TDD strict sur des fonctions déterministes avant de toucher au réseau.
