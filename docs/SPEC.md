# Mimic : sidecar de découverte d'API et de mock pour tirs de perf

> Statut : **v0.2** ; la phase P1 (inférence et redaction) est implémentée dans `crates/mimic-core`.
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
| `mimic-core` | Modèle de données, inférence OpenAPI, matching, redaction | lib, sans I/O, testée en isolation |
| `mimic-injector` | Webhook d'admission qui injecte sidecar + init | binaire (phase 3) |
| `mimicctl` | CLI : bascule de mode, export, diff de specs | binaire (phase 3) |

Principe structurant : **toute la logique métier vit dans `mimic-core`, en fonctions pures**. Le proxy n'est qu'un adaptateur réseau. C'est ce qui permet d'écrire les tests avant le code.

---

## 3. Interception du trafic

### 3.1 Choix : proxy transparent via iptables **[DÉFAUT]**

Même mécanique qu'Istio, éprouvée :

- `mimic-init` (capability `NET_ADMIN`, exécuté avant l'app) pose :
  - **entrant** : `PREROUTING` → `REDIRECT --to-port 15006` pour les ports déclarés de l'app ;
  - **sortant** : `OUTPUT` → `REDIRECT --to-port 15001`, en excluant l'UID du sidecar (`-m owner ! --uid-owner 1337`) pour éviter les boucles, et en excluant `localhost` et les CIDR configurés (ex. DNS kube-system).
- Le proxy récupère la destination d'origine avec `getsockopt(SO_ORIGINAL_DST)` (IPv4) et `IP6T_SO_ORIGINAL_DST` (IPv6).
- Sur K8s ≥ 1.29, le sidecar est déclaré en **native sidecar** (`initContainers` + `restartPolicy: Always`) pour démarrer avant l'app et s'arrêter après elle.

Alternative sans privilège, configurable : **mode proxy explicite** (`HTTP_PROXY`/`HTTPS_PROXY` injectés dans l'app). Utile si `NET_ADMIN` est interdit, mais ne capture que les clients qui respectent ces variables.

### 3.2 Pourquoi pas eBPF

eBPF est excellent pour **observer** passivement, mais :

- il ne permet pas de **répondre** à la place de la dépendance : le mode `mock` exige de toute façon un proxy ;
- le déchiffrement TLS passe par des uprobes sur OpenSSL/BoringSSL/Go crypto/rustls, fragile selon le runtime de l'app ;
- il demande des privilèges noeud (`CAP_BPF`, `CAP_SYS_ADMIN`), plus durs à faire accepter qu'un `NET_ADMIN` limité au pod.

Piste v2 : un mode `observe-ebpf` en lecture seule pour les pods où l'on ne veut rien injecter dans le chemin réseau.

### 3.3 Coexistence avec un service mesh

Si Istio/Linkerd est présent, ses règles iptables et les nôtres se chevauchent. Ordre retenu **[DÉFAUT]** : l'app parle à Mimic, Mimic parle au proxy du mesh (Mimic est le plus proche de l'app, il voit donc le trafic en clair avant le mTLS du mesh). À valider selon le mesh réellement utilisé (question Q3).

### 3.4 Protocoles

| Protocole | v1 |
|-----------|----|
| HTTP/1.1 | record + mock |
| HTTP/2 (h2c et h2 via ALPN) | record + mock |
| gRPC | record + mock au niveau HTTP/2 (bodies binaires, pas d'inférence protobuf) ; inférence via descripteurs en v2 |
| WebSocket / SSE | passthrough, métadonnées seulement (upgrade, durée) |
| TCP non HTTP | passthrough, détecté par sniffing des premiers octets |

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

- Déclarée par annotation de pod : `mimic.platform.io/mode: record`.
- Le sidecar lit les annotations via un **volume downward API** (`fieldRef: metadata.annotations`), que le kubelet met à jour à chaud quand l'annotation change. Aucun droit RBAC sur l'API K8s n'est nécessaire.
- Aussi pilotable via l'API admin (`PUT /mode`) et `mimicctl mode set mock -l app=checkout`.
- La bascule est atomique pour les nouvelles requêtes ; les requêtes en vol finissent dans l'ancien mode.

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
- bodies : au plus `N` exemples par couple (opération, code retour), `N = 20` par défaut, avec remplacement par échantillonnage réservoir pour garder de la diversité ;
- taille max capturée : 1 MiB par body ; au-delà, préfixe + taille ;
- content-encoding (`gzip`, `br`, `zstd`) décodé pour l'inférence, encodage d'origine conservé pour le rejeu ;
- streaming : le proxy ne bufferise jamais la réponse complète avant de la relayer ; la capture se fait en tee.

### 5.3 Mode `mock`

Pour chaque requête sortante, le moteur de correspondance (`mimic-core::matcher`) cherche la meilleure réponse enregistrée.

**Clé de correspondance**, par ordre de priorité :

1. `authority` (hôte:port) — obligatoire ;
2. `method` — obligatoire ;
3. chemin, comparé d'abord exactement, puis au **template** inféré (`/v1/customers/{id}`) ;
4. query : paramètres présents et valeurs ;
5. headers discriminants configurés (ex. `Accept`, `X-Api-Version`) ;
6. body : égalité canonique JSON, puis similarité (champs communs).

Chaque critère donne un score ; le meilleur candidat au-dessus d'un seuil gagne. Le score et le candidat retenu sont exposés dans un header de debug optionnel `x-mimic-match: exact|template|fuzzy;score=0.82`.

**Stratégies de sélection** quand plusieurs exemples correspondent :

| Stratégie | Comportement |
|-----------|--------------|
| `best` **[DÉFAUT]** | meilleur score, puis le plus récent |
| `round-robin` | rotation sur les exemples |
| `sequence` | rejoue dans l'ordre d'enregistrement (scénarios à état : création puis lecture) |
| `weighted-status` | respecte la distribution des codes observés (ex. 2 % de 429) pour reproduire les erreurs réelles |

**Latence rejouée** : la latence n'est pas copiée d'un exemple unique, elle est tirée dans l'histogramme enregistré de l'opération (HDR histogram), avec un multiplicateur configurable (`latency.factor: 1.0`, `0` pour la désactiver, `2.0` pour simuler une dépendance dégradée). Le TTFB et le temps de transfert sont rejoués séparément.

**Requête sans correspondance**, comportement configurable :

| Option | Réponse |
|--------|---------|
| `error` **[DÉFAUT]** | `599` + header `x-mimic-unmatched: true`, compté en métrique |
| `generate` | réponse synthétique générée depuis le schéma OpenAPI de la dépendance |
| `passthrough` | relayée (équivaut à `mock-partial`) |

**Réécriture dynamique** : certains champs ne doivent pas être rejoués tels quels (dates, IDs repris de la requête). Des règles optionnelles permettent de les recalculer :

```yaml
rewrite:
  - match: { authority: api.stripe.com, path: /v1/customers/{id} }
    response:
      body:
        "$.id": "{{request.path.id}}"
        "$.created": "{{now.unix}}"
```

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

## 8. Stockage et export

- **Store local** : fichiers JSONL segmentés et compressés zstd dans un `emptyDir` (`/var/lib/mimic`), rotation par taille (100 MiB par défaut). Index en mémoire reconstruit au démarrage.
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
capture:
  bodies: true
  max_body_bytes: 1048576
  samples_per_operation: 20
redact:
  extra_json_paths: ["$.customer.phone"]
mock:
  strategy: best
  unmatched: error
  latency: { factor: 1.0 }
  match_headers: ["accept", "x-api-version"]
export:
  s3: { bucket: mimic-recordings, interval: 5m }
```

La configuration est validée au démarrage ; une configuration invalide fait échouer le conteneur avec un message explicite (pas de démarrage en mode dégradé silencieux).

### 9.2 API admin (port 15000, localhost + port-forward)

| Méthode | Chemin | Rôle |
|---------|--------|------|
| GET | `/healthz`, `/readyz` | probes |
| GET | `/metrics` | Prometheus |
| GET/PUT | `/mode` | lire / changer le mode |
| GET | `/openapi/inbound` | spec du service |
| GET | `/openapi/outbound/{authority}` | spec d'une dépendance |
| GET | `/exchanges?direction=&path=&limit=` | inspection |
| POST | `/export` | export immédiat |
| DELETE | `/store` | purge |

### 9.3 Observabilité

Métriques Prometheus (scrapables par VictoriaMetrics) :

- `mimic_requests_total{direction, mode, authority, status}`
- `mimic_proxy_overhead_seconds` (histogramme, temps ajouté par Mimic)
- `mimic_mock_match_total{result="exact|template|fuzzy|unmatched"}`
- `mimic_tls_handshake_failures_total{authority, reason}` (détection du pinning)
- `mimic_store_bytes`, `mimic_capture_dropped_total{reason}`

Logs JSON structurés (`tracing`). Propagation de `traceparent` inchangée.

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
│   ├── mimic-injector/     # webhook (phase 3)
│   └── mimicctl/           # CLI (phase 3)
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
| MCK-08 | réponse enregistrée gzip | rejeu | `Content-Encoding: gzip` conservé, body identique |

**Proxy (PRX)**

| ID | Étant donné | Quand | Alors |
|----|-------------|-------|-------|
| PRX-01 | mode record, upstream factice | 10 000 requêtes | toutes relayées sans altération, toutes enregistrées |
| PRX-02 | réponse streamée de 500 MiB | relais | mémoire du proxy < 64 MiB, body tronqué dans le store |
| PRX-03 | upstream HTTP/2 | relais | h2 négocié de bout en bout |
| PRX-04 | trafic TCP non HTTP (protocole Postgres) | relais | passthrough transparent |
| PRX-05 | bascule record → mock pendant une charge | bascule | aucune requête en erreur, nouvelles requêtes mockées |
| PRX-06 | hôte dans `tls.intercept`, app faisant confiance à la CA | requête HTTPS | échange déchiffré et enregistré |
| PRX-07 | hôte hors `tls.intercept` | requête HTTPS | tunnel opaque, seuls SNI et durée enregistrés |

**E2E (E2E)**

| ID | Scénario |
|----|----------|
| E2E-01 | Pod demo (app + Mimic injecté) sur kind, app appelant un faux SaaS HTTPS ; mode record ; la spec inbound et la spec du SaaS sont exportées sur MinIO. |
| E2E-02 | Même pod en mode mock avec NetworkPolicy deny-all egress : le tir k6 réussit à > 99,9 %, aucun paquet ne sort (vérifié par les compteurs du faux SaaS). |
| E2E-03 | Changement d'annotation `record → mock` sans redémarrage du pod. |
| E2E-04 | Namespace labellisé `env=production` : le mode mock est refusé. |

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
| Q3 | Service mesh présent sur les clusters cibles ? Lequel ? | aucun |
| Q4 | Payloads : a-t-on le droit de stocker des exemples redactés, et où (S3, PVC, autre) ? | S3 chiffré, redaction par défaut |
| Q5 | Protocoles à couvrir au-delà d'HTTP (gRPC, Kafka, bases de données) ? | HTTP/1.1, HTTP/2, gRPC en record/mock brut |
| Q6 | Version minimale de Kubernetes (native sidecar à partir de 1.29) ? | ≥ 1.29 |
| Q7 | Génération automatique de scénarios k6 depuis la spec inbound : dans le périmètre ? | oui, phase 5 |

---

## 13. Plan de livraison

Chaque phase commence par l'écriture de ses tests (en rouge), puis de l'implémentation jusqu'au vert.

| Phase | Contenu | Tests de sortie |
|-------|---------|-----------------|
| P0 | Workspace, CI (fmt, clippy, test, deny), modèle `Exchange`, fixtures | build vert |
| P1 | `mimic-core::infer` + `redact` sur fixtures (aucun réseau) | INF-*, RED-* |
| P2 | Proxy HTTP en clair : record entrant/sortant, store local, API admin | PRX-01 à 05 |
| P3 | `mimic-core::matcher` + mode mock + latence | MCK-* |
| P4 | `mimic-init` iptables, manifest d'exemple, E2E kind | E2E-01 à 03 |
| P5 | TLS MITM sortant | PRX-06, 07 |
| P6 | Export S3, chargement cross-environnement, garde-fou production | E2E-04 |
| P7 | Webhook d'injection + `mimicctl` | tests d'admission |
| P8 | Génération de scénarios k6 depuis OpenAPI + `x-mimic-stats` | scénario généré exécutable |

P1 et P3 sont du Rust pur sans I/O : c'est le meilleur point d'entrée pour Claude Code, qui peut avancer en TDD strict sur des fonctions déterministes avant de toucher au réseau.
