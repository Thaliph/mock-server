# Mimic : sidecar de découverte d'API et de mock pour tirs de perf

> Statut : **v0.9** ; rapport de découverte `mimicctl report` (§9.5) et démo OpenTelemetry complète, avec son générateur de charge, en validation des PR (E2E-05). v0.8 : injection automatique par webhook d'admission et CLI `mimicctl` (§9.4, §9.5). v0.7 : le contrôleur devient la seule autorité de bascule, pilotée par la ressource `MimicWorkload`, avec authentification des sidecars (§5.5, §7.1). v0.6 : ajout du contrôleur (§5.5), des snapshots de mock identiques sur tous les pods (§5.6) et de la liste des requêtes rejouables (§5.7). Périmètre réduit à HTTP et HTTPS, hors service mesh.
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
| O3 | En mode `mock`, **zéro** connexion sortante vers une destination enregistrée (vérifiable par NetworkPolicy deny-all egress). |
| O4 | Le document OpenAPI généré passe un validateur OpenAPI 3.1 sans erreur. |
| O5 | Fidélité du mock : code HTTP, headers (hors headers hop-by-hop) et body identiques à l'enregistrement ; latence rejouée à ±10 % de la distribution enregistrée. |
| O6 | Basculer de mode sans redémarrer le pod. |

### Hors périmètre v1

- Protocoles non HTTP (Postgres, Redis, Kafka, AMQP) : passés en **passthrough** TCP, ni enregistrés ni mockés.
- Mock du trafic **entrant** (on ne remplace pas l'application elle-même).
- Interface graphique.
- Production : Mimic est conçu pour les environnements hors production. Un garde-fou refuse le mode `mock` dans un namespace labellisé `env=production`.

---

## 2. Vue d'ensemble

```
                    Pod
 ┌──────────────────────────────────────────────────────────┐
 │                                                          │
 │  trafic entrant ──► [iptables REDIRECT :15006] ──┐       │
 │                                                  ▼       │
 │                                         ┌──────────────┐ │
 │   ┌──────────────┐   localhost:8080     │    mimic     │ │
 │   │ application  │ ◄────────────────────│  (sidecar)   │ │
 │   │              │                      │              │ │
 │   │              │ ── trafic sortant ──►│ :15001       │──┼──► dépendances (record)
 │   └──────────────┘  [iptables :15001]   │              │ │
 │                                         │  store local │ │    ✗ rien ne sort (mock)
 │                                         │  admin :15000│ │
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
| `mimic-controller` | Plan de contrôle : agrège les enregistrements par workload, interface de découverte centrale, snapshots de mock, bascule de mode | binaire, un Deployment par cluster |
| `mimic-injector` | Webhook d'admission qui injecte sidecar + init (§9.4) | binaire |
| `mimicctl` | CLI : bascule de mode, export, diff de specs (§9.5) | binaire |

Principe structurant : **toute la logique métier vit dans `mimic-core`, en fonctions pures**. Le proxy n'est qu'un adaptateur réseau. C'est ce qui permet d'écrire les tests avant le code.

---

## 3. Interception du trafic

### 3.1 Choix : proxy transparent via iptables **[DÉFAUT]**

Même mécanique qu'Istio, éprouvée :

- `mimic-init` (capability `NET_ADMIN`, exécuté avant l'app) pose, dans la table `nat` et des chaînes dédiées `MIMIC_INBOUND` / `MIMIC_OUTBOUND` :
  - **entrant** : `PREROUTING` → `REDIRECT --to-ports 15006` pour les ports déclarés de l'app (ou tous les ports, `*`), sauf les ports du sidecar ;
  - **sortant** : `OUTPUT` → `REDIRECT --to-ports 15001` pour tout TCP, sauf : le trafic de l'UID du sidecar (`-m owner --uid-owner 1337 -j RETURN`, ce qui évite les boucles), le trafic vers l'IP du pod via `lo`, les CIDR et ports exclus (ex. API Kubernetes, bases de données si on préfère ne pas les faire transiter).
- Les règles sont générées par une fonction pure (testée) et appliquées en une fois par `iptables-restore --noflush` ; une deuxième exécution ne duplique rien.
- **Détection du protocole** : le trafic redirigé n'est pas forcément HTTP. Pour chaque connexion redirigée, le proxy lit sans les consommer les premiers octets (`peek`) :
  - méthode HTTP/1 ou préface HTTP/2 : traitement HTTP (record, mock) ;
  - enregistrement TLS (`0x16`) : tunnel TCP opaque tant que l'interception TLS (§4.2) n'est pas activée ; **refusé en mode `mock`** pour garantir qu'aucun appel ne sort (O3) ;
  - autre protocole, ou rien reçu au bout de 300 ms (protocole où le serveur parle en premier, comme MySQL) : tunnel TCP opaque vers la destination d'origine.
  Chaque tunnel est compté dans `mimic_passthrough_connections_total`.
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

### 3.3 Service mesh : hors périmètre

Les pods déjà dans un service mesh (Istio, Linkerd) ne sont pas pris en charge : leurs règles iptables et celles de Mimic se chevaucheraient. Mimic vise les workloads hors mesh (décision du 17/10, Q3).

### 3.4 Protocoles

Seuls HTTP et HTTPS sont gérés (décision du 17/10, Q5). Le reste traverse Mimic sans être enregistré ni mocké.

| Protocole | v1 |
|-----------|----|
| HTTP/1.1 | record + mock |
| HTTP/2 (h2c et h2 via ALPN) | record + mock |
| HTTPS vers un hôte de `tls.intercept` | record + mock (§4.2) |
| HTTPS vers un autre hôte | tunnel opaque en record, refusé en mock |
| WebSocket / SSE | passthrough, métadonnées seulement (upgrade, durée) |
| TCP non HTTP | tunnel opaque, détecté par sniffing des premiers octets ; à exclure par port dans `mimic-init` si on veut le sortir du chemin |

Le relais garde la version de la requête reçue : une requête HTTP/2 part en HTTP/2 vers l'amont (h2c en connaissance préalable, h2 via ALPN en HTTPS), trailers compris, car un serveur gRPC refuse HTTP/1.1 ; une requête HTTP/1.x part en HTTP/1.1.

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
| `mock` | relayé + enregistré | **servi depuis le store**, jamais relayé | tir de perf isolé |
| `mock-partial` | relayé + enregistré | servi si correspondance, sinon relayé + enregistré | enrichir progressivement le store |

Le mode `mock` ne touche jamais le trafic entrant : le tir de perf frappe la vraie application, ce sont ses dépendances qui sont simulées.

### 5.1 Bascule de mode

Aucune bascule ne redémarre de pod, et **toute bascule passe par le contrôleur** (§5.5) :

- l'état souhaité d'un workload est déclaré par une ressource Kubernetes `MimicWorkload` (CRD, §5.5.1) : `kubectl patch mimicworkload checkout -n shop --type merge -p '{"spec":{"mode":"mock"}}'`, ou un manifest versionné en GitOps ;
- qui a le droit de basculer est donc décidé par le RBAC Kubernetes sur cette ressource (verbes `create`, `update`, `patch` sur `mimicworkloads.mimic.platform.io`), et chaque changement est tracé dans l'audit de l'API server ;
- le contrôleur propage l'état souhaité aux sidecars par leur battement (§5.5).

Un sidecar rattaché à un contrôleur (`share.url` vers le contrôleur) n'accepte aucun autre ordre : `PUT /mode` et `DELETE /store` de son API admin répondent `403`, et l'annotation `mimic.platform.io/mode` est ignorée. Il démarre dans le dernier état souhaité reçu (gardé dans `store.dir`), sinon dans le mode de sa configuration, puis s'aligne au premier battement.

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
- mémoire : en plus de `capture.max_exchanges` (50 000), le store a un budget en octets estimés, `capture.max_store_bytes` (64 MiB par défaut). Les bodies gardés en occupent au plus la moitié : au-delà, les bodies des échanges les plus anciens sont retirés en premier. Au-delà du budget, les échanges les plus anciens sont évincés. Un nouvel échange garde donc ses bodies, même quand les métadonnées remplissent le budget. Chaque retrait est compté (`reason="store_budget"`). Les défauts tiennent dans la limite mémoire de 256 Mi posée par l'injecteur (§9.4) : store et file de partage (§5.4) en occupent au plus un tiers, le reste couvre la copie du moteur de mock, la découverte et les bodies en vol ;
- taille max capturée : 1 MiB par body ; au-delà, préfixe + taille ;
- content-encoding (`gzip`, `br`, `zstd`) décodé pour l'inférence, encodage d'origine conservé pour le rejeu ;
- streaming : le proxy ne bufferise jamais la réponse complète avant de la relayer ; la capture se fait en tee.

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
| `round-robin` | rotation sur les exemples |
| `sequence` | rejoue dans l'ordre d'enregistrement (scénarios à état : création puis lecture) |
| `weighted-status` | respecte la distribution des codes observés (ex. 2 % de 429) pour reproduire les erreurs réelles |

**Latence rejouée** : la latence n'est pas copiée d'un exemple unique, elle est tirée dans la distribution empirique des durées enregistrées pour l'opération (même authority, méthode et template), avec un multiplicateur configurable (`latency.factor: 1.0`, `0` pour la désactiver, `2.0` pour simuler une dépendance dégradée). En v1, la durée totale est appliquée avant l'envoi de la réponse ; le rejeu séparé du TTFB et du temps de transfert viendra avec le streaming des réponses mockées.

Le tirage aléatoire (latence, `weighted-status`) utilise un générateur à graine (`mock.seed`) : un même tir rejoué avec la même graine produit la même séquence.

**Requête sans correspondance**, comportement configurable :

| Option | Réponse |
|--------|---------|
| `error` **[DÉFAUT]** | `599` + header `x-mimic-unmatched: true`, compté en métrique |
| `generate` | réponse synthétique générée depuis le schéma OpenAPI de la dépendance (v2) |
| `passthrough` | relayée (équivaut à `mock-partial`) |

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

Un Deployment a souvent plusieurs réplicas, et chaque pod ne voit qu'une partie du trafic. Les sidecars d'un même workload mettent donc leurs enregistrements en commun : la découverte couvre tout le trafic du workload, et n'importe quel pod peut répondre en mode mock avec un enregistrement fait par un autre.

- **Identité du workload** : `namespace` + nom du workload, lus via l'API downward. Le nom vient de l'annotation `mimic.platform.io/workload`, sinon du label `app.kubernetes.io/name`, sinon du label `app`. Chaque sidecar a un identifiant d'instance (nom du pod).
- **Support partagé** : un stockage objet (S3-compatible en production, répertoire local ou PVC `ReadWriteMany` en test). Aucun appel à l'API Kubernetes, aucune connexion entre pods.
- **Disposition** : `<prefix>/<namespace>/<workload>/exchanges/<instance>/<segment>.jsonl`, où `<segment>` est un ULID choisi par le sidecar. Un segment ne contient que des échanges déjà redactés. Avec un contrôleur, les segments d'une heure révolue sont fusionnés en un segment horaire `exchanges/_hour/<AAAAMMJJHH>.jsonl` (§5.5.2) : les noms d'instance qui commencent par `_` sont réservés et ne désignent jamais un pair.
- **Synchronisation** toutes les `share.interval` (10 s par défaut) : le sidecar publie ses nouveaux échanges, liste les segments des autres instances et importe ceux qu'il n'a pas encore lus.
- **Publication** : les échanges de la file de publication sont envoyés du plus ancien au plus récent, en segments d'au plus 8 MiB, chacun avec son identifiant. Un segment dont l'envoi échoue reste en tête de la file et sera renvoyé tel quel, avec le même identifiant : une reprise après un délai dépassé ne crée pas de doublon (§5.5.2). Seul un segment entamé par la borne de la file (ses plus anciens échanges jetés) repart sous un nouvel identifiant. Après un échec, la synchronisation suivante attend `share.interval` × 2ⁿ (n échecs consécutifs), plafonné à 5 minutes, avec une gigue de 50 % : les sidecars ne reviennent pas tous en même temps quand le support revient.
- **Liste incrémentale** : après une première liste complète, le sidecar ne demande que les segments datés d'au moins la date du plus récent déjà listé moins 60 s, une marge qui couvre les écritures concurrentes (`?since_ms=` vers le contrôleur, filtre local sur un stockage objet ou un contrôleur qui ignore le paramètre). Ce curseur n'avance qu'après l'import réussi de tous les segments listés.
- **Fusion** : dédoublonnage par identifiant d'échange (ULID préfixé par l'instance, donc unique dans le workload). Les plafonds de capture (§5.2) s'appliquent après fusion, ce qui borne la mémoire quel que soit le nombre de réplicas.
- **Panne du support** : la synchronisation échoue sans bloquer le trafic ; le sidecar continue avec ses données locales et l'échec est compté en métrique.
- **File de publication** : les échanges locaux pas encore publiés attendent dans une file unique, bornée en octets (`share.max_backlog_bytes`, 16 MiB par défaut) et en nombre (`share.max_pending`, 50 000 échanges par défaut). Quand le support reste indisponible, les plus anciens sont jetés au profit des plus récents dès que l'une des deux bornes est dépassée, et chaque échange jeté est compté (`mimic_capture_dropped_total{reason="share_backlog"}`) ; une publication échouée laisse ses échanges en tête de file, sous ces mêmes bornes. Sans partage configuré, aucune file n'est tenue.
- **Rétention** : les segments plus vieux que `share.retention` (7 jours par défaut) ne sont plus importés. Avec un contrôleur, c'est lui qui les supprime (§5.5.2) ; avec un stockage objet sans contrôleur, leur suppression est laissée à une règle de cycle de vie du bucket.
- **Deux supports possibles** : `share.url` vaut soit l'URL du contrôleur (`https://mimic-controller.mimic-system:8443`, recommandé), soit un stockage objet partagé (`s3://…`, `file://…`) sans contrôleur. Sans contrôleur, il n'y a ni interface centrale, ni snapshot, ni bascule de workload.

### 5.5 Contrôleur

`mimic-controller` est le plan de contrôle, à la manière de Kiali pour Istio : un Deployment unique par cluster (namespace `mimic-system`), qui ne se trouve jamais dans le chemin des requêtes. Si le contrôleur tombe, le trafic continue dans le dernier état reçu ; seuls le partage et la bascule attendent son retour.

- **Seule autorité** : le contrôleur est le seul à modifier le mode et le snapshot des sidecars. Il lit l'état souhaité dans les ressources `MimicWorkload` (§5.5.1), qu'il surveille par l'API Kubernetes.
- **Ingestion** : chaque sidecar lui envoie ses segments d'échanges, déjà redactés, et lit ceux des autres instances (§5.4). Le contrôleur les range sur son support (`file://` sur un PVC, ou `s3://`) avec la même disposition qu'au §5.4. L'ingestion est idempotente (§5.5.2).
- **Battement** : chaque sidecar envoie toutes les `share.mode_poll` (2 s par défaut) son instance, son mode et son snapshot chargé. La réponse porte l'état souhaité du workload : `{mode, snapshot}`, chacun pouvant être absent. Sans `MimicWorkload`, l'état souhaité est vide et chaque sidecar reste dans le mode de sa configuration.
- **Authentification** : chaque appel d'un sidecar est authentifié par un jeton de ServiceAccount, et le contrôleur ne lui permet d'agir que pour son propre pod et son propre workload (§7.1).
- **Interface de découverte centrale**, en lecture seule : la page du contrôleur liste les workloads, puis pour chacun les pods (mode, snapshot, dernier battement, aligné ou non), les opérations du service et de chaque dépendance avec leurs paramètres requis (même rendu que la page d'admin du sidecar, §9.2), les snapshots, les requêtes enregistrées (§5.7) et la commande `kubectl` qui bascule le workload. Elle est servie sur un port distinct lié à `127.0.0.1`, donc joignable seulement par `kubectl port-forward`, lui-même soumis au RBAC (`pods/portforward` dans `mimic-system`).
- **Persistance** : segments, snapshots et snapshot courant de chaque workload (`<namespace>/<workload>/desired.json`) sont sur le support ; un redémarrage du contrôleur ne perd rien. L'état des pods est reconstruit par les battements.
- **Tenue en charge** : 500 pods sur 100 workloads avec 7 jours d'historique, sur un seul réplica (§5.5.2).

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
status:
  snapshot: 3f9a0c1e5b7d2a46   # snapshot effectivement servi
  pods: 3
  aligned: 3
  observedGeneration: 4
  conditions:
    - type: Aligned       # tous les pods vus récemment appliquent l'état souhaité
      status: "True"
```

Réconciliation (fonction pure dans le contrôleur, testée sans cluster) :

- `spec.mode` en `mock` ou `mock-partial` sans `spec.snapshot` : le contrôleur construit un snapshot si `status.snapshot` est vide, puis l'écrit dans `status.snapshot`. Le snapshot reste le même tant que le mode ne quitte pas le mock, ce qui garde les pods alignés même si de nouveaux échanges arrivent.
- `spec.snapshot` renseigné : ce snapshot est servi ; s'il n'existe pas, la condition `Aligned` passe à `False` avec la raison `SnapshotNotFound` et les sidecars gardent leur état.
- `spec.mode` en `record` ou `passthrough` : aucun snapshot, `status.snapshot` est vidé.
- Ressource supprimée : l'état souhaité redevient vide.
- `status` est mis à jour à chaque changement d'alignement des pods.

#### 5.5.2 Tenue en charge

Aucun coût du contrôleur ne grandit avec l'historique : ni la liste des segments, ni la mémoire, ni le disque, ni la durée d'un battement.

- **Seul écrivant, index en mémoire** : le contrôleur est le seul processus qui écrit sur son support, et il tourne en un seul réplica (`Recreate`). Il tient en mémoire l'index des segments de chaque workload, construit par **une seule** liste du support au premier accès, puis mis à jour à chaque ingestion, compaction ou suppression. La date d'un segment brut est celle de son écriture ; celle d'un segment horaire est la fin de son heure moins une milliseconde, de sorte qu'un curseur `since_ms` antérieur à l'un de ses échanges le liste encore. `GET …/segments` répond depuis cet index sans relire le support : segments encore dans la rétention, du plus ancien au plus récent, et seulement ceux datés d'au moins `since_ms` s'il est donné. Un fichier déposé sur le support par un autre processus n'est vu qu'après un redémarrage. La liste des workloads suit la même règle : le support est listé une fois, puis un nouveau workload entre par son premier battement ou son premier segment.
- **Ingestion idempotente** : le sidecar donne l'identifiant de son segment (`?segment=`, un ULID). Un segment déjà rangé sous cet identifiant n'est ni réécrit ni réimporté, et la réponse est `200` au lieu de `201` (`mimic_controller_segments_total{result="duplicate"}`). Sans identifiant (ancien sidecar), le contrôleur en génère un.
- **Verrous courts** : les pods, l'état souhaité et la ressource d'un workload sont protégés par un verrou court, jamais tenu pendant une lecture ou une écriture du support, ni pendant un calcul. Les échanges en mémoire ont le leur, tenu le temps d'ajouter ou d'évincer des échanges. Un battement, la liste des workloads, le statut des ressources et `/metrics` n'attendent donc jamais une inférence, un snapshot ou un import.
- **Calculs hors du serveur** : l'inférence, la construction d'un snapshot et les requêtes enregistrées (tri et mise en forme compris) s'exécutent hors des threads qui servent les requêtes, sur une vue figée des échanges en mémoire, sans les copier. L'ingestion continue pendant ce temps.
- **Pods oubliés** : un pod sans battement depuis `pod_forget` (10 minutes par défaut) disparaît de l'interface, du statut et des métriques.
- **Mémoire bornée** : un workload garde au plus `max_exchanges_per_workload` échanges, les plus récents, et l'ensemble des workloads au plus `max_exchanges_total` : au-delà, les plus anciens échanges du workload qui en a le plus sont évincés. Comme dans le sidecar (§5.2), avec la même clé `samples_per_operation` (20 par défaut) et le même couple (opération, code retour), l'opération étant (sens, authority, méthode, forme du chemin), seuls `samples_per_operation` échanges par couple gardent leurs bodies en mémoire : les premiers reçus, les suivants étant gardés sans body jusqu'à ce qu'une éviction libère une place. Après un redémarrage, les échanges d'un workload ne sont relus qu'à sa première vue (interface, requêtes, snapshot) : ingérer un segment ne relit pas l'historique, l'index suffit à dédoublonner. La relecture va du plus récent au plus ancien et s'arrête au plafond du workload.
- **Découverte en tâche de fond** : les documents OpenAPI d'un workload sont recalculés au plus une fois par `discovery_interval` (30 s par défaut), en tâche de fond. Une vue sert le dernier document calculé, même si de nouveaux échanges sont arrivés depuis, et déclenche le recalcul si l'intervalle est écoulé ; seule la première vue d'un workload attend le calcul.
- **Rétention et compaction** : toutes les minutes, le contrôleur supprime les segments plus vieux que `retention`, puis fusionne les segments bruts de chaque heure UTC révolue depuis plus de 10 minutes en un segment horaire `exchanges/_hour/<AAAAMMJJHH>.jsonl` : échanges dédoublonnés par identifiant, au plus `samples_per_operation` bodies par couple (opération, code retour) au sens du §5.2, choisis par le même réservoir déterministe, les autres gardés sans body. Le segment horaire ne dépend donc pas de l'ordre dans lequel les segments bruts sont relus. Le segment horaire est écrit avant la suppression des segments bruts : un arrêt entre les deux ne perd rien et la passe suivante refait la fusion. Les segments relus sont déjà redactés et la compaction ne fait qu'en retirer des données. Enfin, si les segments d'un workload dépassent `max_bytes_per_workload`, les plus anciens sont supprimés. Un workload garde ainsi au plus un fichier par heure de rétention, plus les segments bruts des 70 dernières minutes au plus, dans un volume borné. Les snapshots et `desired.json` ne sont jamais supprimés.

### 5.6 Cohérence du mock entre les pods : snapshots

Plusieurs pods d'un même workload doivent répondre de la même manière en mock, y compris sur un endpoint qu'un pod n'a jamais vu passer. Le partage seul (§5.4) ne suffit pas : à un instant donné, deux pods n'ont pas forcément importé les mêmes segments, et leurs stores bornés n'évincent pas les mêmes échanges.

- **Snapshot** : jeu figé d'échanges sortants d'un workload, construit par le contrôleur au moment de la bascule en mock à partir de tous les segments du workload. Il est immuable et identifié par une empreinte de son contenu (`sha256`, 16 caractères hexadécimaux) : mêmes échanges, même identifiant, quel que soit l'ordre d'arrivée.
- **Construction** (`mimic-core::snapshot`, fonction pure) : échanges sortants terminés, dédoublonnés par identifiant, triés par date puis identifiant, au plus `snapshot.max_per_key` (50) par couple (authority, méthode, chemin, code), les plus récents gardés. Le plafond est compté à part pour les échanges dont la réponse est rejouable (body capturé ou vide) et pour les autres (body tronqué ou écarté, notamment au-delà de `capture.samples_per_operation`, §5.2) : sur un endpoint très sollicité, les plus récents n'ont plus de body, et ils ne doivent pas évincer les exemples rejouables. Les seconds ne servent qu'à la distribution de latence.
- **Bascule** : passer `spec.mode` à `mock` construit un snapshot et le désigne comme état souhaité ; renseigner `spec.snapshot` réutilise un snapshot existant, ce qui permet de rejouer un tir de perf dans des conditions identiques d'une semaine à l'autre. Chaque sidecar télécharge le snapshot une fois, le garde en cache (sur disque si `store.dir` est configuré), puis construit son moteur de mock **uniquement** à partir du snapshot. Tant que le snapshot n'est pas chargé, y compris quand son téléchargement échoue (il est retenté à chaque battement), le sidecar mocke avec ses données locales et le signale (`snapshot: null` dans son battement) : un passage en mock ne laisse jamais le trafic sortir.
- **Même appel vu par plusieurs sidecars** : chaque pod enregistre son exemplaire, avec un identifiant propre ; rien n'est fusionné à tort. Au rejeu, ces exemplaires sont des candidats de même score. La stratégie `best` choisit alors la **réponse la plus fréquente** (même code et même body), puis la plus récente. Une erreur ponctuelle vue par un seul pod ne masque donc pas la réponse habituelle ; pour la reproduire en proportion, utiliser `weighted-status`. Toutes les durées des exemplaires alimentent la distribution de latence de l'opération.
- **Ce qui est identique d'un pod à l'autre** : à requête égale, le code, les headers et le body servis (stratégie `best`). La latence suit la même distribution sur chaque pod, mais le tirage est propre à chaque pod. Les stratégies à état (`round-robin`, `sequence`) avancent indépendamment sur chaque pod.

### 5.7 Requêtes enregistrées et rejeu

Le contrôleur expose la liste des requêtes entrantes enregistrées pour le workload, toutes instances confondues, les plus récentes d'abord : méthode, chemin, query, headers, body, code, durée, instance. Elle sert à rejouer le trafic réel contre l'application pendant un tir de perf.

- `GET …/requests?direction=inbound&limit=&format=json` : liste JSON, affichée aussi dans l'interface ;
- `format=jsonl` : un échange par ligne, rejouable par n'importe quel outil ;
- `format=k6` : script k6 prêt à lancer (`k6 run -e TARGET=http://checkout:8080 script.js`), qui rejoue les requêtes dans l'ordre d'enregistrement. Les headers redactés et les headers propres à la connexion ne sont pas rejoués ; le script le signale en commentaire et accepte un header d'authentification par `-e AUTHORIZATION=…`.

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
- Enum : chaîne avec ≤ 10 valeurs distinctes sur ≥ 30 observations.
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

Le hash est calculé sur `sel || valeur`, avec un sel propre à l'installation, puis tronqué à 8 caractères hexadécimaux. Il garde la cohérence (deux requêtes avec le même token portent le même marqueur) sans permettre de retrouver une valeur courte par dictionnaire. La redaction est idempotente : un marqueur n'est jamais re-hashé.

Bodies que la redaction ne peut pas analyser : un body JSON ou formulaire tronqué, ou un JSON invalide, est remplacé par `Skipped { size }`, car on ne peut pas garantir qu'il ne contient pas de secret. Les bodies binaires ne sont pas modifiés.

Conséquence pour le mode `mock` : un body redacté est rejoué redacté. Les headers d'authentification sortants ne participent jamais au matching. Si une réponse doit contenir une valeur réaliste, on passe par les règles `rewrite`.

Option `capture.bodies: false` pour ne garder que métadonnées et schémas (inférence faite en mémoire, exemples jetés).

---

### 7.1 Authentification et autorité du contrôleur

Le contrôleur doit être sûr de parler au bon sidecar, et le sidecar sûr de recevoir ses ordres du vrai contrôleur.

- **Sidecar vers contrôleur** : le sidecar monte un jeton de ServiceAccount projeté, d'audience `mimic-controller` et de courte durée (`serviceAccountToken`, renouvelé par le kubelet), et l'envoie en `Authorization: Bearer` à chaque appel. Le contrôleur le valide par `TokenReview` (audience comprise), puis vérifie que :
  - le namespace du ServiceAccount est celui du chemin `/v1/workloads/{namespace}/…` ;
  - le pod lié au jeton (`authentication.kubernetes.io/pod-name`) existe, appartient à ce workload (§5.4) et correspond à l'instance déclarée.
  Sinon : `401` (jeton absent ou invalide) ou `403` (jeton valide pour un autre namespace, pod ou workload). Un pod compromis ne peut donc ni lire, ni polluer les enregistrements d'un autre workload. Les validations sont gardées en cache 60 s, par empreinte du jeton.
- **Contrôleur vers sidecar** : l'API des sidecars n'est servie qu'en HTTPS, avec un certificat signé par une CA dédiée (Secret géré par cert-manager). Le sidecar ne fait confiance qu'à cette CA (`share.ca_file`), pas aux racines système : un serveur qui se ferait passer pour le contrôleur est refusé. `http://` n'est accepté qu'avec `share.allow_insecure: true`, réservé aux tests.
- **Humains** : aucun ordre ne passe par l'API HTTP du contrôleur. La bascule passe par `MimicWorkload` (RBAC et audit Kubernetes), la consultation par `kubectl port-forward` (RBAC `pods/portforward`).
- **Droits du contrôleur** : `get`, `list`, `watch` sur `mimicworkloads` et `patch` sur leur `status` ; `create` sur `tokenreviews` ; `get` sur `pods`. Rien d'autre.

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
store:
  dir: /var/lib/mimic          # absent : mémoire seule
  segment_bytes: 33554432      # 32 MiB par segment local-<n>.jsonl (§8)
  max_segments: 4
redact:
  extra_json_paths: ["$.customer.phone"]
mock:
  strategy: best
  unmatched: error
  latency: { factor: 1.0 }
  match_headers: ["accept", "x-api-version"]
share:
  url: https://mimic-controller.mimic-system:8443   # ou s3://bucket/prefixe, file:///var/lib/mimic-shared
  ca_file: /var/run/mimic/controller-ca.crt        # seule CA de confiance pour le contrôleur
  token_file: /var/run/secrets/mimic/token         # jeton de ServiceAccount projeté
  interval: 10       # secondes
  retention: 604800  # secondes (7 jours)
  mode_poll: 2       # secondes, battement vers le contrôleur
  max_backlog_bytes: 16777216  # 16 MiB, file des échanges pas encore publiés (§5.4)
  max_pending: 50000           # même file, en nombre d'échanges
```

Configuration du contrôleur (`mimic-controller --config`) :

```yaml
listen: 0.0.0.0:8443            # API des sidecars
ui_listen: 127.0.0.1:8081       # interface, par port-forward
tls: { cert_file: /etc/mimic/tls/tls.crt, key_file: /etc/mimic/tls/tls.key }
auth: kubernetes                # TokenReview ; static pour les tests
store: file:///var/lib/mimic-controller   # ou s3://bucket/prefixe
retention: 604800                  # secondes : segments supprimés au-delà
max_exchanges_per_workload: 200000
max_exchanges_total: 1000000       # tous workloads confondus
samples_per_operation: 20          # bodies gardés par (opération, code), en mémoire et après compaction
max_bytes_per_workload: 268435456  # segments d'un workload sur le support (256 MiB)
discovery_interval: 30             # secondes, au plus un recalcul de la découverte par intervalle
snapshot:
  max_per_key: 50
pod_timeout: 30   # un pod sans battement depuis ce délai est affiché absent
pod_forget: 600   # un pod sans battement depuis ce délai est oublié
```

La configuration est validée au démarrage ; une configuration invalide fait échouer le conteneur avec un message explicite (pas de démarrage en mode dégradé silencieux).

### 9.2 API admin (port 15000, localhost + port-forward)

| Méthode | Chemin | Rôle |
|---------|--------|------|
| GET | `/` | page d'admin (voir ci-dessous) |
| GET | `/api/operations` | opérations découvertes, en JSON |
| GET | `/healthz`, `/readyz` | probes |
| GET | `/metrics` | Prometheus |
| GET/PUT | `/mode` | lire / changer le mode |
| GET | `/openapi/inbound` | spec du service |
| GET | `/openapi/outbound/{authority}` | spec d'une dépendance |
| GET | `/exchanges?direction=&path=&limit=` | inspection |
| POST | `/share/sync` | synchronisation immédiate avec les autres sidecars |
| DELETE | `/store` | purge |

**Page d'admin** : page HTML autonome (aucune ressource externe, utilisable par `kubectl port-forward` dans un cluster sans accès Internet), rendue côté serveur à partir des documents OpenAPI du workload. Elle montre :

- le mode courant, l'instance, le workload et l'état de la synchronisation (nombre de pairs, dernière synchro) ;
- pour le service puis pour chaque dépendance : la liste des opérations (méthode, template, nombre d'appels, répartition des codes, latences p50/p99) ;
- pour chaque opération, dépliable : les paramètres (chemin, query, headers) avec leur type et un marqueur **requis**, le schéma du body de requête, les codes de réponse avec schéma et exemples ;
- des liens de téléchargement des documents OpenAPI.

### 9.2b API du contrôleur

Deux ports :

- **8443, API des sidecars**, HTTPS, jeton de ServiceAccount obligatoire (§7.1). Préfixe `/v1/workloads/{namespace}/{workload}` noté `…`.

| Méthode | Chemin | Rôle |
|---------|--------|------|
| POST | `…/segments?instance=&segment=` | ingestion d'un segment JSONL de son propre pod ; `segment` (ULID) rend l'envoi idempotent : `201` à la première réception, `200` ensuite (§5.5.2) |
| GET | `…/segments?since_ms=` | liste des segments encore dans la rétention (`key`, `instance`, `modified_ms`, `size`), du plus ancien au plus récent ; avec `since_ms`, seulement ceux datés d'au moins cette valeur (ms Unix) |
| GET | `…/segments/{instance}/{file}` | contenu d'un segment |
| POST | `…/heartbeat` | battement, réponse : état souhaité `{mode, snapshot}` |
| GET | `…/snapshots/{id}` | contenu JSONL d'un snapshot |
| GET | `/healthz` | probe |

- **8081, interface en lecture seule**, liée à `127.0.0.1`, joignable par `kubectl port-forward -n mimic-system deploy/mimic-controller 8081`.

| Méthode | Chemin | Rôle |
|---------|--------|------|
| GET | `/` | liste des workloads |
| GET | `/ui/{namespace}/{workload}` | page d'un workload |
| GET | `/v1/workloads` | workloads connus, avec pods et état souhaité |
| GET | `…/mode` | état souhaité courant |
| GET | `…/snapshots`, `…/snapshots/{id}` | snapshots |
| GET | `…/operations` | opérations découvertes sur tout le workload |
| GET | `…/openapi/inbound`, `…/openapi/outbound/{authority}` | documents OpenAPI du workload |
| GET | `…/requests?direction=&limit=&format=` | requêtes enregistrées (§5.7) |
| GET | `/metrics` | Prometheus |

Métriques du contrôleur : `mimic_controller_segments_total{result}` (`ok`, `duplicate`, `error`), `mimic_controller_auth_failures_total{reason}`, `mimic_controller_workloads`, `mimic_controller_pods{state}` (`aligned`, `diverging`, `absent`), `mimic_controller_snapshots_total`, `mimic_controller_exchanges` (échanges en mémoire, tous workloads).

### 9.3 Observabilité

Métriques Prometheus sur `/metrics` (scrapables par VictoriaMetrics), actives dans tous les modes :

| Métrique | Type | Labels | Mode |
|----------|------|--------|------|
| `mimic_requests_total` | compteur | `direction`, `mode`, `authority`, `method`, `status` | tous |
| `mimic_request_duration_seconds` | histogramme | `direction`, `mode`, `authority` | tous |
| `mimic_upstream_errors_total` | compteur | `direction`, `authority`, `kind` | record |
| `mimic_mock_responses_total` | compteur | `authority`, `match` (`exact`, `template`, `fuzzy`, `unmatched`) | mock |
| `mimic_mock_injected_latency_seconds` | histogramme | `authority` | mock |
| `mimic_discovered_operations` | jauge | `direction`, `authority` | tous |
| `mimic_store_exchanges` | jauge | `origin` (`local`, `peer`) | tous |
| `mimic_store_bytes` | jauge (octets estimés des échanges en mémoire, §5.2) | | tous |
| `mimic_capture_dropped_total` | compteur | `reason` (`truncated`, `sample_limit`, `store_budget`, `share_backlog`, `local_backlog`) | tous |
| `mimic_share_sync_total` | compteur | `result` (`ok`, `error`) | tous |
| `mimic_share_peers` | jauge | | tous |
| `mimic_mode` | jauge (1 pour le mode courant) | `mode` | tous |
| `mimic_passthrough_connections_total` | compteur | `direction`, `kind` (`tls`, `tcp`), `result` (`relayed`, `refused`, `error`) | tous |
| `mimic_tls_handshake_failures_total` | compteur | `authority`, `reason` | tous |
| `mimic_mock_snapshot_exchanges` | jauge (0 sans snapshot) | | mock |

`authority` est borné : au-delà de 100 valeurs distinctes, les suivantes sont regroupées sous `other` pour protéger la cardinalité.

Logs JSON structurés (`tracing`). Propagation de `traceparent` inchangée.

Sous charge, les logs d'erreurs répétitives sont limités en débit : au plus une ligne par seconde pour les échecs vers l'amont et pour les erreurs d'`accept`, avec le nombre de lignes omises depuis la précédente. Les compteurs restent exacts (`mimic_upstream_errors_total`). Une erreur d'`accept` (par exemple `EMFILE`, plus de descripteurs) suspend l'écoute concernée pendant 50 ms avant de réessayer, au lieu de boucler à 100 % de CPU au moment où l'on mesure le pod.

### 9.4 Injection automatique (`mimic-injector`)

Webhook d'admission mutant (`MutatingWebhookConfiguration`), déployé dans `mimic-system`, qui ajoute à la création d'un pod ce que `deploy/example/deployment.yaml` écrit à la main.

- **Portée** : les pods des namespaces labellisés `mimic.platform.io/enabled=true`, le label qui leur fait déjà recevoir la CA du contrôleur. Un pod s'en retire par l'annotation `mimic.platform.io/inject: "false"`.
- **Fonction pure, sans appel à l'API** : la mutation ne dépend que du pod et de la configuration du webhook (`mimic-injector::inject`), et se teste sans cluster. `sideEffects: None` ; `failurePolicy: Ignore`, si bien qu'un webhook indisponible laisse créer le pod sans Mimic au lieu de le bloquer ; `timeoutSeconds: 5`.
- **Ce qui est ajouté** (patch JSON) :
  - l'init container `mimic-init` (`NET_ADMIN`, `NET_RAW`), en tête. Ports entrants : annotation `mimic.platform.io/inbound-ports`, sinon les `containerPort` TCP des conteneurs de l'app, sinon `*`. CIDR sortants exclus : ceux de la configuration du webhook (API Kubernetes) puis l'annotation `mimic.platform.io/exclude-outbound-cidrs`. Ports sortants exclus : annotation `mimic.platform.io/exclude-outbound-ports` ;
  - le native sidecar `mimic`, juste après `mimic-init` et donc avant les init containers de l'app, avec une `startupProbe` sur `/healthz` : l'app ne démarre qu'une fois le proxy prêt. Variables `POD_NAME`, `POD_NAMESPACE`, `MIMIC_WORKLOAD` (valeur figée à l'admission) et `MIMIC_REDACT_SALT` lue dans le Secret `<workload>-mimic`, clé `redact-salt`, facultatif ;
  - la configuration du sidecar : par défaut celle du webhook (`sidecar_config`), passée en ligne par `MIMIC_CONFIG_YAML` ; l'annotation `mimic.platform.io/config: <configmap>` monte ce ConfigMap (clé `mimic.yaml`) à la place ;
  - les volumes : store (`emptyDir`), jeton projeté d'audience `mimic-controller`, CA du contrôleur (ConfigMap `mimic-controller-ca`), répertoire de CA Mimic (`emptyDir`, `/var/run/mimic/ca`) ;
  - si l'annotation `mimic.platform.io/tls-intercept` liste des hôtes (séparés par des virgules) : `MIMIC_TLS_INTERCEPT` pour le sidecar, et dans chaque conteneur de l'app le répertoire de CA monté en lecture seule avec `SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS`, `REQUESTS_CA_BUNDLE` et `CURL_CA_BUNDLE` (§4.2). Une variable déjà définie par l'app n'est pas écrasée ;
  - l'annotation `mimic.platform.io/status: injected` et le label `mimic.platform.io/sidecar: "true"` (NetworkPolicy du contrôleur).
- **Nom du workload** : annotation `mimic.platform.io/workload`, sinon label `app.kubernetes.io/name`, sinon label `app` (§5.4).
- **Pods laissés tels quels**, admis avec un avertissement d'admission visible par `kubectl` : pod déjà injecté (idempotence, réinvocation), `hostNetwork: true`, conteneur `istio-proxy` ou `linkerd-proxy` présent (§3.3), aucun nom de workload trouvé.
- Le webhook est servi en HTTPS (certificat cert-manager, `caBundle` posé par le cainjector). Métrique `mimic_injector_admissions_total{result}` (`injected`, `skipped`, `error`).

Côté sidecar, `MIMIC_CONFIG_YAML` contient le YAML de la configuration quand aucun fichier n'est donné (`--config` ou `MIMIC_CONFIG` l'emportent) ; `MIMIC_TLS_INTERCEPT` remplace `tls.intercept`.

Configuration du webhook (`mimic-injector --config`) :

```yaml
listen: 0.0.0.0:8443
tls: { cert_file: /etc/mimic/tls/tls.crt, key_file: /etc/mimic/tls/tls.key }
image: registry.example.com/platform/mimic:0.1.0
exclude_outbound_cidrs: [10.96.0.1/32]   # API Kubernetes
sidecar_config: |                       # configuration par défaut des sidecars (§9.1)
  mode: record
  share: { url: https://mimic-controller.mimic-system.svc:8443, ... }
resources:
  requests: { cpu: 50m, memory: 64Mi }
  limits: { memory: 256Mi }
```

### 9.5 CLI `mimicctl`

Outil des humains. Il parle à l'API Kubernetes avec le kubeconfig courant et n'a que les droits de son utilisateur (RBAC, §7.1).

| Commande | Effet |
|----------|-------|
| `mimicctl mode <workload> <mode> [--snapshot id] [-n ns]` | crée ou met à jour la `MimicWorkload` (server-side apply, gestionnaire `mimicctl`) |
| `mimicctl status [<workload>] [-n ns \| -A]` | mode souhaité, snapshot servi, pods alignés |
| `mimicctl export <workload> (--inbound \| --outbound <authority>) [--openapi-version 3.1\|3.0] [-o fichier]` | document OpenAPI depuis l'interface du contrôleur |
| `mimicctl requests <workload> [--format json\|jsonl\|k6] [--limit n]` | requêtes enregistrées (§5.7) |
| `mimicctl diff <ancien> <nouveau> [--json] [--fail-on-breaking]` | différence entre deux documents OpenAPI (YAML ou JSON) |
| `mimicctl report [<workload>] [-n ns \| -A] [--top n]` | rapport Markdown de ce que les sidecars ont découvert (ci-dessous) |

- `status` n'affiche le snapshot et les pods alignés que si le contrôleur a publié le statut de la génération courante (`status.observedGeneration` égal à `metadata.generation`) ; sinon ces colonnes valent `?` (par exemple `?/2`), car un statut antérieur décrit l'état souhaité d'avant la bascule.
- `export` et `requests` ouvrent un port-forward vers le pod du contrôleur (port 8081), comme `kubectl port-forward` : même RBAC (`pods/portforward` dans `mimic-system`), aucun port exposé.
- `--openapi-version 3.0` convertit le document 3.1 (`mimic-core::openapi::to_3_0`) : `type: [T, "null"]` devient `type: T` avec `nullable: true`, les `examples` d'un schéma deviennent `example` (le premier), `openapi` vaut `3.0.3`.
- **Rapport** : pour chaque workload connu du contrôleur (filtré par nom ou namespace, ou tous avec `-A`), son mode et ses pods, la liste des API découvertes (le service, puis chaque dépendance par authority) avec leur nombre d'opérations et d'appels, puis les `n` opérations les plus appelées (10 par défaut) avec leurs codes, leur latence p50 et p99 et les headers d'une requête enregistrée de cette opération (même sens, même dépendance, même méthode, chemin conforme au template). Les headers sont ceux du store, donc déjà redactés (§7) ; une valeur est tronquée à 80 caractères. Sans requête enregistrée, les headers déclarés comme paramètres de l'opération sont listés. Tri par appels décroissants, puis sens, API, chemin et méthode : deux rapports sur les mêmes données sont identiques. Le Markdown est fait pour le résumé d'un job GitHub Actions (`>> $GITHUB_STEP_SUMMARY`).
- **Diff** (`mimic-core::diff`, fonction pure) : opérations ajoutées et supprimées, puis pour chaque opération commune les paramètres ajoutés, supprimés ou devenus requis, et les codes de réponse ajoutés ou supprimés. Sont **cassants** : une opération supprimée, un paramètre devenu requis, un paramètre requis ajouté, un code 2xx supprimé. Sortie texte triée, `--json` pour une sortie machine. Avec `--fail-on-breaking`, le code de sortie vaut 1 s'il y a un changement cassant, ce qui sert en CI à comparer la découverte d'une semaine à l'autre.

---

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
| INF-07 | n'importe quel corpus (proptest) | inférence | la spec produite est un OpenAPI 3.1 valide |
| INF-08 | même corpus, ordre des échanges permuté (proptest) | inférence | spec identique (déterminisme) |

**Redaction (RED)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| RED-01 | header `Authorization: Bearer abc` | capture | valeur stockée `<redacted:sha256:…>`, jamais `abc` |
| RED-02 | deux requêtes avec le même token | capture | même marqueur de redaction |
| RED-03 | body avec un numéro de carte valide (Luhn) | capture | remplacé ; un numéro invalide de même longueur ne l'est pas |
| RED-04 | n'importe quel body contenant une valeur secrète injectée (proptest) | capture puis lecture du store brut | la valeur n'apparaît dans aucun octet du store |

**Matching et mock (MCK)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| MCK-01 | enregistrement `GET api.x.com/v1/a → 200 {…}` | même requête en mode mock | même code, headers et body, octet pour octet (hors hop-by-hop) |
| MCK-02 | enregistrement sur `/v1/c/1` | requête `/v1/c/2` | réponse issue du template, `x-mimic-match: template` |
| MCK-03 | aucun enregistrement pour l'hôte | requête | `599`, `x-mimic-unmatched: true`, compteur incrémenté |
| MCK-04 | latences enregistrées p50 = 100 ms | 1 000 requêtes mockées | p50 rejoué entre 90 et 110 ms |
| MCK-05 | `latency.factor: 2` | 1 000 requêtes | p50 entre 180 et 220 ms |
| MCK-06 | stratégie `sequence`, enregistrement POST puis GET | rejeu | réponses servies dans l'ordre d'origine |
| MCK-07 | distribution 98 % 200 / 2 % 429, `weighted-status` | 10 000 requêtes | 429 entre 1,5 % et 2,5 % |
| MCK-08 | réponse enregistrée gzip, br ou zstd | rejeu | `Content-Encoding` conservé, body identique une fois décodé |

**Moteur de mock, niveau `mimic-core` (MCK, suite)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| MCK-09 | deux enregistrements de même chemin, query `?page=1` et `?page=2` | requête `?page=2` | l'enregistrement `page=2` est choisi |
| MCK-10 | enregistrement sur une autre méthode ou une autre authority | requête | aucune correspondance |
| MCK-11 | même graine, mêmes requêtes | deux exécutions | décisions et latences identiques |
| MCK-12 | trois exemplaires du même appel, deux en `200 {a}` et un en `500`, le `500` le plus récent | stratégie `best` | `200 {a}` |

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
| CTL-07 | segments datés d'il y a 8 jours, de 2 heures et de maintenant, rétention de 7 jours | `GET …/segments`, puis `?since_ms=` | le segment hors rétention n'est jamais listé ; avec `since_ms`, seuls les segments datés d'au moins cette valeur, du plus ancien au plus récent |
| CTL-08 | lecture du support bloquée pendant le chargement des échanges d'un workload (vue de l'interface en cours) | battement de ce workload, `/v1/workloads`, `/metrics` | réponses immédiates, battement avec l'état souhaité courant |
| CTL-09 | pod sans battement depuis plus de `pod_forget` | battement d'un nouveau pod, ou passe de maintenance | le pod disparu n'est plus listé ni compté |
| CTL-10 | segments plus vieux que `retention`, un snapshot et un état souhaité | passe de maintenance | ces segments sont supprimés du support et de la liste ; snapshot et état souhaité sont gardés |
| CTL-11 | segments bruts de deux instances dans une heure révolue, avec un échange en double et plus de `samples_per_operation` bodies pour une opération | passe de maintenance | un seul segment `_hour/<AAAAMMJJHH>.jsonl` pour cette heure, sans doublon, au plus `samples_per_operation` bodies par couple (opération, code), choisis par le réservoir du §5.2 : mêmes exemples quel que soit l'ordre des segments bruts ; segments bruts supprimés ; un pod qui liste sans curseur reçoit le segment horaire |
| CTL-12 | plus de `samples_per_operation` échanges avec body pour une même opération, chemins de même forme (`/users/1`, `/users/2`) | ingestion | seuls `samples_per_operation` gardent leur body en mémoire, les autres sont gardés sans body ; une éviction libère une place |
| CTL-13 | deux workloads dont le total dépasse `max_exchanges_total` ; workload dont le support dépasse `max_exchanges_per_workload` | ingestion ; redémarrage, ingestion, puis vue | total en mémoire ≤ `max_exchanges_total`, les plus anciens du plus gros workload évincés ; après le redémarrage, l'ingestion ne relit rien ; à la vue, seuls les `max_exchanges_per_workload` plus récents sont relus |
| CTL-14 | découverte déjà calculée, puis nouveaux échanges | vue dans le `discovery_interval` du calcul ; vues après l'intervalle | même document servi sans recalcul ; recalcul en tâche de fond, puis les nouvelles opérations apparaissent |
| CTL-15 | segment déjà reçu (délai dépassé côté sidecar) | même segment renvoyé avec le même identifiant | `200`, une seule copie sur le support et en mémoire, `segments_total{result="duplicate"}` |

**Ressource MimicWorkload (CRD)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| CRD-01 | `spec.mode: mock` sans snapshot, `status` vide | réconciliation | snapshot construit, `status.snapshot` renseigné, état souhaité `mock` + ce snapshot |
| CRD-02 | `status.snapshot` déjà renseigné, nouveaux échanges | réconciliation | même snapshot gardé |
| CRD-03 | `spec.snapshot` inconnu | réconciliation | condition `Aligned=False`, raison `SnapshotNotFound`, état souhaité inchangé |
| CRD-04 | `spec.mode: record` après du mock | réconciliation | `status.snapshot` vidé, état souhaité `record` sans snapshot |
| CRD-05 | ressource supprimée | réconciliation | état souhaité vide |
| CRD-06 | deux pods, un seul aligné | calcul du statut | `pods: 2`, `aligned: 1`, `Aligned=False` |

**Sécurité (SEC)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| SEC-01 | sidecar rattaché à un contrôleur | `PUT /mode` ou `DELETE /store` sur son admin, ou annotation de mode | `403` / ignorée, mode inchangé |
| SEC-02 | appel à l'API des sidecars sans jeton ou avec un jeton invalide | toute route sauf `/healthz` | `401`, compteur `auth_failures` |
| SEC-03 | jeton valide d'un pod du workload A | écriture ou lecture sur le workload B, ou ingestion au nom d'une autre instance | `403` |
| SEC-04 | `share.url` en `http://` sans `allow_insecure` | démarrage du sidecar | configuration refusée |
| SEC-05 | contrôleur servi avec un certificat d'une autre CA que `share.ca_file` | battement | refusé, mode inchangé, erreur visible dans le statut |
| SEC-06 | interface du contrôleur | écoute | liée à `127.0.0.1` uniquement |

**Rejeu (RPL)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| RPL-01 | requêtes entrantes enregistrées par deux pods | `GET …/requests?direction=inbound` | toutes listées, les plus récentes d'abord, avec l'instance |
| RPL-02 | requête avec un header `authorization` redacté | `format=k6` | script k6 valide qui rejoue méthode, chemin, query et body, sans le header redacté |
| RPL-03 | requêtes enregistrées | `format=jsonl` | un échange par ligne, relisible tel quel |

**Partage (SHR)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| SHR-01 | deux sidecars du même workload sur un support partagé, chacun ayant vu des opérations différentes | synchronisation | chaque sidecar documente l'union des opérations |
| SHR-02 | enregistrement d'une dépendance fait par le sidecar A | requête mockée sur le sidecar B | B répond avec l'enregistrement de A |
| SHR-03 | segments déjà importés | nouvelle synchronisation | aucun échange dupliqué |
| SHR-04 | sidecar d'un autre workload sur le même support | synchronisation | ses échanges ne sont pas importés |
| SHR-05 | support indisponible | synchronisation | le trafic n'est pas affecté, `mimic_share_sync_total{result="error"}` augmente |
| SHR-06 | sidecar sans partage configuré | trafic enregistré | aucun échange n'est gardé pour publication |
| SHR-07 | partage configuré, support indisponible, trafic au-delà de `share.max_backlog_bytes` | synchronisations échouées répétées | la file reste sous sa borne et garde les échanges les plus récents ; les échanges jetés sont comptés dans `mimic_capture_dropped_total{reason="share_backlog"}` |
| SHR-08 | sidecar qui a déjà listé des segments | synchronisations suivantes | liste demandée avec `since_ms` = date du plus récent listé moins 60 s ; un import échoué ne fait pas avancer le curseur |
| SHR-09 | contrôleur qui refuse les publications, puis revient ; trafic au-delà de `share.max_pending` | publications successives | même identifiant de segment à chaque reprise, attente croissante entre les tentatives ; la même file reste sous `share.max_pending`, garde les plus récents et compte les jetés dans `mimic_capture_dropped_total{reason="share_backlog"}` ; segments d'au plus 8 MiB |

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

**Admin et métriques (ADM)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| ADM-01 | trafic enregistré sur `GET /users/{userId}` avec query `page` toujours présente | `GET /` | la page liste l'opération et marque `page` comme requis |
| ADM-02 | trafic entrant et sortant, en record puis en mock | `GET /metrics` | les métriques du tableau §9.3 sont présentes avec les bons labels |
| ADM-03 | `PUT /mode` avec `mock` | requête suivante | servie en mock, `mimic_mode{mode="mock"} 1` |

**Init (INI)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| INI-01 | ports entrants `8080,9090` | génération | une règle `REDIRECT` par port vers 15006, ports du sidecar exclus |
| INI-02 | ports entrants `*` | génération | redirection de tout le TCP entrant sauf les ports du sidecar |
| INI-03 | UID, CIDR et ports sortants exclus | génération | règles `RETURN` placées avant la redirection sortante |
| INI-04 | règles déjà posées | deuxième application | aucune règle dupliquée |

**Proxy (PRX)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| PRX-01 | mode record, upstream factice | 10 000 requêtes | toutes relayées sans altération, toutes enregistrées |
| PRX-02 | réponse streamée de 500 MiB | relais | mémoire du proxy < 64 MiB, body tronqué dans le store |
| PRX-03 | client et upstream gRPC en h2c | relais | HTTP/2 de bout en bout, `te: trailers` et trailers (`grpc-status`) relayés, échange enregistré |
| PRX-04 | trafic TCP non HTTP redirigé (client qui parle en premier, ou serveur qui parle en premier) | relais | tunnel transparent, octets identiques |
| PRX-04b | connexion TLS redirigée | mode record / mode mock | tunnel opaque / connexion refusée, compteur `refused` |
| PRX-09 | application à l'écoute sur l'IP du pod seulement, puis sur `127.0.0.1` seulement | relais entrant | la première est jointe sur l'IP du pod, la seconde par repli sur `127.0.0.1` ; sans aucune écoute, l'IP du pod est retenue et rien n'est mémorisé |
| PRX-08 | règles iptables posées par `mimic-init` dans un namespace réseau, sidecar sous l'UID 1337 | l'application appelle une dépendance HTTP sans proxy configuré | l'appel est enregistré avec l'authority d'origine, puis servi en mock |
| PRX-05 | bascule record → mock pendant une charge | bascule | aucune requête en erreur, nouvelles requêtes mockées |
| PRX-06 | hôte dans `tls.intercept`, app faisant confiance à la CA | requête HTTPS | échange déchiffré et enregistré |
| PRX-07 | hôte hors `tls.intercept` | requête HTTPS | tunnel opaque, seuls SNI et durée enregistrés |
| PRX-10 | `accept` en échec répété (EMFILE) | boucle d'écoute | 50 ms au moins entre deux tentatives, au plus une ligne de log par seconde ; l'écoute reprend dès que l'erreur cesse |
| PRX-11 | amont injoignable | rafale de 200 requêtes | 200 réponses `502` et `mimic_upstream_errors_total` à 200, mais au plus une ligne de log par seconde, la suivante indiquant le nombre de lignes omises |

**Injection (INJ)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| INJ-01 | pod avec le label `app.kubernetes.io/name: checkout` et un conteneur sur le port 8080 | admission | `mimic-init` en tête avec `MIMIC_INBOUND_PORTS=8080`, native sidecar `mimic` juste après (`restartPolicy: Always`, UID 1337, `startupProbe`), `MIMIC_WORKLOAD=checkout`, volumes, annotation et label ajoutés |
| INJ-02 | pod déjà injecté | nouvelle admission | aucun patch |
| INJ-03 | annotation `mimic.platform.io/inject: "false"`, ou `hostNetwork`, ou conteneur `istio-proxy` | admission | aucun patch, un avertissement |
| INJ-04 | pod sans nom de workload | admission | aucun patch, un avertissement |
| INJ-05 | annotation `mimic.platform.io/tls-intercept: api.stripe.com` | admission | `MIMIC_TLS_INTERCEPT` dans le sidecar ; CA montée et variables de confiance dans l'app, sans écraser une variable existante |
| INJ-06 | annotations `inbound-ports`, `exclude-outbound-cidrs`, `exclude-outbound-ports` et `config` | admission | valeurs reprises par `mimic-init`, ConfigMap monté à la place de la configuration en ligne |
| INJ-07 | pod avec ses propres init containers | admission | `mimic-init` puis `mimic` passent avant eux, leur ordre est conservé |
| INJ-08 | `AdmissionReview` d'un pod | `POST /mutate` en HTTPS | `allowed: true`, même `uid`, patch JSON qui s'applique au pod et donne le pod de INJ-01 |
| INJ-09 | `MIMIC_CONFIG_YAML` et `MIMIC_TLS_INTERCEPT` définis, sans fichier | démarrage du sidecar | configuration lue comme depuis un fichier, puis surchargée par l'environnement |

**CLI (CLI)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| CLI-01 | `mimicctl mode checkout mock -n shop` | construction de la requête | apply d'une `MimicWorkload` `shop/checkout` avec `spec.mode: mock`, sans snapshot |
| CLI-02 | mode inconnu | `mimicctl mode checkout fast` | refus avant tout appel, avec la liste des modes valides |
| CLI-03 | export d'une dépendance, puis requêtes en k6 | construction du chemin | `/v1/workloads/shop/checkout/openapi/outbound/api.stripe.com`, `/v1/workloads/shop/checkout/requests?direction=inbound&format=k6&limit=100` |
| CLI-04 | un workload avec 12 opérations (service et dépendance `quote:8080`), une requête enregistrée de `GET /items/{id}` | `mimicctl report --top 10` | les deux API avec leurs opérations et appels ; exactement 10 opérations, triées par appels décroissants ; les headers de la requête, `\|` échappé et valeurs longues tronquées ; une opération sans requête liste ses paramètres header |
| CLI-05 | `MimicWorkload` en génération 5, statut publié pour la génération 4 (2/2 alignés, snapshot) | `mimicctl status` | snapshot `?` et pods alignés `?/2` ; une fois la génération 5 publiée, le snapshot et `2/2` |

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

**E2E (E2E)**

| ID | Scénario |
|----|----------|
| E2E-01 | Pod demo (app + Mimic injecté) sur kind, app appelant un faux SaaS HTTPS ; mode record ; la spec inbound et la spec du SaaS sont exportées sur MinIO. |
| E2E-02 | Même pod en mode mock avec NetworkPolicy deny-all egress : le tir k6 réussit à > 99,9 %, aucun paquet ne sort (vérifié par les compteurs du faux SaaS). |
| E2E-03 | Changement d'annotation `record → mock` sans redémarrage du pod. |
| E2E-04 | Namespace labellisé `env=production` : le mode mock est refusé. |
| E2E-05 | Démo OpenTelemetry sur kind (`scripts/e2e-kind.sh`, à chaque PR vers `main`) : les services de la démo, sans la chaîne de télémétrie ni Kafka, avec son générateur de charge actif ; tous injectés par le webhook sauf `valkey-cart` (opt-out). Les requêtes du test passent par le frontend (`/api/shipping`), qui appelle shipping (2 réplicas) en gRPC, qui appelle quote en HTTP ; en record les deux API de shipping (gRPC entrant, quote sortant) sont découvertes par le contrôleur. Après cinq minutes de charge, `mimicctl report -A` est publié dans le résumé du job et les documents OpenAPI en artefact. Tous les workloads injectés passent ensuite en mock, sauf le générateur (le client) : une fois les pods alignés, pendant 60 s, `mimic_requests_total` des sidecars ne compte plus aucune requête entrante, sauf sur les services que le générateur appelle lui-même (frontend-proxy, et flagd pour ses feature flags) ; frontend-proxy reçoit son trafic et répond depuis le snapshot (`mimic_mock_responses_total`) ; le bilan est ajouté au résumé du job, puis retour en record. Après `mimicctl mode shipping mock`, les deux pods s'alignent sur un snapshot, quote est arrêté et toutes les requêtes réussissent ; retour en record. |

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
| Q3 | Service mesh présent sur les clusters cibles ? Lequel ? | tranché : hors périmètre |
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
| P4 | `mimic-init` iptables, détection de protocole, image, manifests d'exemple, E2E en namespace réseau | INI-*, PRX-04, PRX-08 ; E2E kind ensuite |
| P5 | TLS MITM sortant | PRX-06, 07 |
| P6 | Contrôleur : ingestion, interface centrale, snapshots, CRD `MimicWorkload`, authentification, requêtes rejouables | MCK-12, SNP-*, CTL-*, CRD-*, SEC-*, RPL-* |
| P6b | Chargement cross-environnement, garde-fou production | E2E-04 |
| P6c | Contrôleur à l'échelle : index des segments, liste incrémentale, verrous courts, GC et compaction, budget mémoire, ingestion idempotente | CTL-06 à 15, SHR-08, SHR-09 |
| P7 | Webhook d'injection + `mimicctl` (dont le rapport) | INJ-*, CLI-*, OAS-*, DIF-*, E2E-05 |
| P8 | Génération de scénarios k6 depuis OpenAPI + `x-mimic-stats` | scénario généré exécutable |

P1 et P3 sont du Rust pur sans I/O : c'est le meilleur point d'entrée pour Claude Code, qui peut avancer en TDD strict sur des fonctions déterministes avant de toucher au réseau.
