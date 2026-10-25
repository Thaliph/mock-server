# Configurer

[← Sommaire](README.md)

Référence de configuration de chaque composant, d'après le code
(`crates/mimic-proxy/src/config.rs`, `crates/mimic-injector/src/{config,inject}.rs`,
`crates/mimic-init/src/main.rs`, `crates/mimic-controller/src/{config,main,crd}.rs`).

Règles communes :

- toutes les configurations sont en YAML et **refusent les champs inconnus** : une faute de
  frappe fait échouer le démarrage avec un message explicite, jamais un mode dégradé silencieux ;
- les durées sont en **secondes** (décimales acceptées) ;
- les tailles sont en **octets**.

## Sidecar (`mimic-proxy`)

### Source de la configuration

Dans cet ordre :

1. fichier donné par `--config <fichier>` (ou `-c`, ou la variable `MIMIC_CONFIG`) ;
2. sinon YAML en ligne de la variable `MIMIC_CONFIG_YAML` (c'est ce que pose le webhook, à partir
   de son `sidecar_config`) ;
3. sinon les valeurs par défaut ;

puis les variables d'environnement du tableau ci-dessous s'appliquent par-dessus, et la
configuration est validée. Une configuration invalide arrête le conteneur avec le code 2 et un
message `mimic: …` ; un échec au démarrage (port déjà pris…) avec le code 1.

Avec le webhook, un pod peut remplacer toute la configuration par une ConfigMap à lui :
annotation `mimic.platform.io/config: <nom>`, clé `mimic.yaml`, montée dans `/etc/mimic`.

### Champs

| Champ | Défaut | Valeurs, effet |
|-------|--------|----------------|
| `mode` | `record` | `passthrough`, `record`, `mock`, `mock-partial` : mode de démarrage (voir [Modes](Modes.md)). Sous contrôleur, le dernier état souhaité gardé dans `store.dir` l'emporte au démarrage |
| `instance` | `local` | identifiant de l'instance ; en pratique le nom du pod (`POD_NAME`). Caractères permis : lettres, chiffres, `-`, `.`, `_` |
| `workload.namespace` | vide | namespace du workload (`POD_NAMESPACE`) ; vide, `default` dans les URL du contrôleur |
| `workload.name` | vide | nom du workload (`MIMIC_WORKLOAD`) ; **obligatoire** si `share` est renseigné |
| `inbound.enabled` | `true` | écoute entrante |
| `inbound.listen` | `0.0.0.0:15106` | adresse de l'écoute entrante (cible des redirections de `mimic-init`) |
| `inbound.upstream` | absent | adresse imposée de l'application. Absent : destination d'origine (IP du pod), sinon `127.0.0.1` sur le même port |
| `outbound.enabled` | `true` | écoute sortante |
| `outbound.listen` | `127.0.0.1:15101` | écoute sortante : trafic redirigé, ou proxy explicite (`HTTP_PROXY`, `CONNECT`) |
| `admin.listen` | `0.0.0.0:15100` | API et page d'admin, `/metrics`, sondes |
| `capture.bodies` | `true` | `false` : aucun body gardé, métadonnées et schémas seulement |
| `capture.max_body_bytes` | `1048576` (1 MiB) | taille capturée par body ; au-delà, préfixe et taille. C'est aussi la taille maximale d'un body de requête en mock (`413` au-delà) |
| `capture.samples_per_operation` | `20` | bodies gardés par couple (opération, code), choisis par réservoir déterministe |
| `capture.max_exchanges` | `50000` | échanges en mémoire ; les plus anciens sont évincés. Doit être positif |
| `capture.max_store_bytes` | `67108864` (64 MiB) | budget mémoire du store (octets estimés), bodies au plus pour moitié. Doit être positif |
| `capture.inbound` | `full` | `full`, `metadata` (méthode, chemin, code, durées, tailles ; en mémoire seulement), `off` (métriques seules). Le contrôleur peut l'imposer |
| `redact.extra_json_paths` | `[]` | chemins JSON supplémentaires à redacter, forme `$.a.b` (sans index de tableau) |
| `redact.salt` | absent | sel des marques de redaction (`MIMIC_REDACT_SALT`). Absent sous contrôleur : sel du workload reçu à chaque battement ; absent sans contrôleur : refus de démarrer (`redact.salt requis sans contrôleur`) |
| `redact.raw_bodies` | `[]` | bodies que la redaction ne sait pas lire (XML, texte, `multipart`, gRPC, binaire, sans `Content-Type`) gardés tels quels, sans redaction, pour ces hôtes (`api.partner.io`, `*.partner.io`, `*`) et `inbound` pour l'entrant ; ailleurs, non capturés et donc non rejouables (`599` en mock) |
| `mock.strategy` | `best` | `best`, `round-robin`, `sequence`, `weighted-status` (voir [Modes](Modes.md#choix-de-la-réponse)) |
| `mock.unmatched` | `error` | `error` : `599` et `x-mimic-unmatched: true` ; `passthrough` : relayée et enregistrée |
| `mock.latency_factor` | `1.0` | multiplicateur de la latence rejouée, entre 0 et 100 ; `0` : aucune attente |
| `mock.match_headers` | `[]` | headers discriminants comparés pour choisir la réponse (`accept`, `x-api-version`…) |
| `mock.seed` | `24301` (`0x5EED`) | graine du tirage (latence, `weighted-status`) |
| `mock.debug_header` | `true` | ajoute `x-mimic-match: <exact\|template\|fuzzy>;score=<s>` aux réponses mockées |
| `mock.from` | absent | mock local (sans contrôleur) : snapshot JSONL servi au démarrage (`mimicctl pull`), mode `mock` (ou `mock-partial` si demandé) ; incompatible avec `share` ; sel facultatif. Voir `deploy/local/` |
| `mock.websocket` | `relay` | poignée de main WebSocket sortante en mock : `relay` (relayée) ou `refuse` (`503`). Le contrôleur peut l'imposer |
| `mock.tcp` | `refuse` | connexion TCP sortante non HTTP en `mock`, connexions restées muettes 30 s comprises : `refuse` (refermée sans joindre la dépendance, comptée dans `mimic_passthrough_connections_total{result="refused"}`) ou `relay` (tunnel vers la vraie dépendance, qui reçoit la charge). Sans effet en `mock-partial` (toujours relayé) et sur l'entrant. Ni le contrôleur ni l'injecteur ne le transmettent : pour garder une seule dépendance réelle, préférer `mimic.platform.io/exclude-outbound-ports` (voir [Modes](Modes.md#trafic-sortant)) |
| `store.dir` | absent | répertoire du journal local et du cache des snapshots ; absent : mémoire seule |
| `store.segment_bytes` | `33554432` (32 MiB) | taille d'un segment `local-<n>.jsonl` avant rotation. Doit être positif |
| `store.max_segments` | `4` | segments gardés ; le plus ancien est supprimé. Doit être positif |
| `store.size_limit_bytes` | absent | taille de l'`emptyDir` de `store.dir`, exposée par `mimic_store_disk_limit_bytes` (alerte `MimicStoreDiskHigh`) ; le webhook la pose par `MIMIC_STORE_SIZE_LIMIT_BYTES` (512 Mi) |
| `tls.intercept` | `[]` | hôtes HTTPS sortants interceptés : `api.stripe.com`, `*.salesforce.com`, ou `*`. Remplacé par `MIMIC_TLS_INTERCEPT` |
| `tls.ca_cert_file`, `tls.ca_key_file` | absents | CA fournie (les deux ensemble) ; absents : CA éphémère générée au démarrage |
| `tls.ca_dir` | absent | répertoire où publier `ca.crt` (CA mimic) et `ca-bundle.crt` (racines système + CA mimic) pour l'application |
| `tls.upstream_ca_file` | absent | CA supplémentaire de confiance vers les dépendances (CA d'entreprise) |
| `share` | absent | aucun partage : sidecar isolé, bascule par l'API admin ou l'annotation de mode |
| `share.url` | requis | URL du contrôleur : `https://…`, ou `http://…` avec `allow_insecure`. Toute autre valeur, dont `s3://…` et `file://…` (partage direct, retiré), arrête le sidecar au démarrage (code 2) avec `share.url "…" refusée : un sidecar ne partage que par le contrôleur …`. Sans cluster, servir un snapshot avec `mimic-proxy --mock-from <fichier>` |
| `share.interval` | `10` | période de publication ; après n échecs, `interval` × 2ⁿ plafonné à 300 s, gigue de 50 % |
| `share.retention` | absent | sans effet : accepté pour les configurations existantes, avec un WARN `share.retention ignoré : la rétention est celle du contrôleur` au démarrage. La rétention est la `retention` du contrôleur |
| `share.mode_poll` | `2` | période du battement vers le contrôleur |
| `share.ca_file` | absent | **seule** CA de confiance pour le contrôleur ; requise avec `https://` |
| `share.token_file` | absent | jeton de ServiceAccount projeté, relu à chaque appel |
| `share.allow_insecure` | `false` | autorise `http://` vers le contrôleur (tests uniquement) |
| `share.max_backlog_bytes` | `16777216` (16 MiB) | borne en octets de la file des échanges pas encore publiés |
| `share.max_pending` | `50000` | borne en nombre de la même file |
| `share.startup.timeout` | `30` | sous contrôleur : attente du premier état souhaité avant d'ouvrir la barrière de démarrage (l'application ne démarre qu'après, voir [Basculer en mock](Basculer-en-mock.md#pods-neufs-pendant-un-mock)). Entre 0 (exclu) et 300 ; la `startupProbe` posée par le webhook tolère ce délai + 30 s |
| `share.startup.fallback` | `record` | sans état souhaité connu à l'échéance : `record`, `passthrough`, `mock-local` (mock sur les données locales, rien ne sort ; un pod neuf répond `599`) ou `fail` (l'application ne démarre pas tant que le contrôleur n'a pas répondu). Avec `mock-local` et `fail`, le sidecar attend aussi en mock |
| `annotations_file` | absent | fichier d'annotations du pod (API downward) surveillé chaque seconde pour `mimic.platform.io/mode` ; ignoré sous contrôleur. Le webhook ne le pose pas |
| `version` | absent | valeur du label `version` des métriques (voir ci-dessous) |

Configuration posée par défaut par le webhook (`sidecar_config` de `deploy/injector/injector.yaml`) :

```yaml
mode: record
inbound:  {listen: 0.0.0.0:15106}
outbound: {listen: 127.0.0.1:15101}
admin:    {listen: 0.0.0.0:15100}
capture:  {samples_per_operation: 20}
mock:     {strategy: best, unmatched: error, latency_factor: 1.0}
store:    {dir: /var/lib/mimic}
tls:      {ca_dir: /var/run/mimic/ca}
share:
  url: https://mimic-controller.mimic-system.svc:8443
  ca_file: /etc/mimic-ca/ca.crt
  token_file: /var/run/secrets/mimic/token
  interval: 10
  mode_poll: 2
  startup: {timeout: 30, fallback: record}
```

> **Écart avec la spec.** Le multiplicateur de latence s'écrit `mock.latency_factor`. La forme
> `mock.latency: { factor: 1.0 }` de l'exemple de la spec (§9.1) est refusée (champ inconnu). De
> même, `inbound.ports` et `outbound.exclude_cidrs` n'existent pas dans la configuration du
> sidecar : les ports interceptés et les exclusions sont des réglages de `mimic-init` (ci-dessous).

### Variables d'environnement

| Variable | Effet |
|----------|-------|
| `MIMIC_CONFIG` | fichier de configuration (équivaut à `--config`) |
| `MIMIC_CONFIG_YAML` | configuration en ligne, quand aucun fichier n'est donné |
| `MIMIC_MODE` | remplace `mode` (valeur inconnue : `MIMIC_MODE inconnu: …`) |
| `POD_NAME`, `POD_NAMESPACE`, `MIMIC_WORKLOAD` | remplacent `instance`, `workload.namespace`, `workload.name` |
| `MIMIC_REDACT_SALT` | remplace `redact.salt` |
| `MIMIC_MOCK_FROM` | remplace `mock.from` (comme `--mock-from`) |
| `MIMIC_REDACT_RAW_BODIES` | remplace `redact.raw_bodies` (liste séparée par des virgules) |
| `MIMIC_VERSION`, `MIMIC_POD_TEMPLATE_HASH` | label `version` : `MIMIC_VERSION` non vide, sinon `version` de la configuration, sinon `MIMIC_POD_TEMPLATE_HASH` ; tronqué à 63 caractères ; vide : pas de label |
| `MIMIC_TLS_INTERCEPT` | remplace `tls.intercept` (liste séparée par des virgules) |
| `MIMIC_LOG` | filtre des logs JSON (`info` par défaut ; par exemple `debug`, `mimic_proxy=debug`). Posée par le webhook depuis l'annotation `mimic.platform.io/log` ou `sidecar_log` |
| `MIMIC_STORE_SIZE_LIMIT_BYTES` | remplace `store.size_limit_bytes` (taille de l'`emptyDir` du store, en octets), posée par le webhook ; valeur non numérique : erreur au démarrage |

Options de la ligne de commande : `--config <fichier>`, `--mock-from <fichier>` (mock local,
comme `mock.from`), `--probe <chemin>` (sonde `exec` :
interroge l'API admin locale et sort en 0 sur une réponse 2xx, en 1 sinon, en moins de 2 s),
`--version`.

## Annotations de pod

Lues par le webhook à la création du pod (`crates/mimic-injector/src/inject.rs`). À poser sur
le **template** du pod (Deployment, StatefulSet…).

| Annotation | Valeur | Effet |
|------------|--------|-------|
| `mimic.platform.io/inject` | `"false"` | pas d'injection ; avertissement `mimic non injecté : injection désactivée par mimic.platform.io/inject`. En **label** du pod : `"false"` le soustrait au webhook d'un namespace `enabled=true`, `"true"` le désigne dans un namespace `enabled=opt-in` |
| `mimic.platform.io/workload` | nom | nom du workload (`MIMIC_WORKLOAD`) ; sinon label `app.kubernetes.io/name`, sinon label `app`. Sans nom : pas d'injection |
| `mimic.platform.io/inbound-ports` | `8080,9090`, `*`, `-` | `MIMIC_INBOUND_PORTS`, l'emporte sur tout le reste ; défaut : aucune interception entrante (`-`), sauf `inbound: true` dans la configuration du webhook ou namespace labellisé `mimic.platform.io/inbound=true` : ports `containerPort` TCP des conteneurs de l'app (hors conteneurs du mesh), sinon `*` ; `-` : aucune interception entrante, même dans ces cas |
| `mimic.platform.io/exclude-outbound-cidrs` | liste de CIDR | ajoutés à `exclude_outbound_cidrs` du webhook dans `MIMIC_EXCLUDE_OUTBOUND_CIDRS` |
| `mimic.platform.io/exclude-outbound-ports` | liste de ports | `MIMIC_EXCLUDE_OUTBOUND_PORTS` |
| `mimic.platform.io/tls-intercept` | liste d'hôtes | `MIMIC_TLS_INTERCEPT` pour le sidecar ; dans chaque conteneur de l'app, volume `mimic-ca` monté en lecture seule sur `/var/run/mimic/ca`, et `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE` (`ca-bundle.crt`), `NODE_EXTRA_CA_CERTS` (`ca.crt`), sans écraser une variable déjà définie |
| `mimic.platform.io/config` | nom de ConfigMap | ConfigMap (clé `mimic.yaml`) montée sur `/etc/mimic`, sidecar lancé avec `--config /etc/mimic/mimic.yaml` au lieu de `MIMIC_CONFIG_YAML` |
| `mimic.platform.io/mesh` | `auto`, `none`, `istio`, `istio-ambient`, `linkerd` | `MIMIC_MESH` ; sinon `istio` ou `linkerd` si leurs conteneurs sont déjà dans le pod, sinon `auto`. Valeur inconnue : injecté en `auto` avec un avertissement |
| `mimic.platform.io/capture-raw-bodies` | hôtes séparés par des virgules, `inbound` | `MIMIC_REDACT_RAW_BODIES` : bodies non analysables gardés en clair pour ces dépendances, pour pouvoir les mocker |
| `mimic.platform.io/log` | filtre `EnvFilter` (`debug`, `warn,mimic_proxy=debug`) | `MIMIC_LOG` du sidecar, à la place de `sidecar_log` ; pris en compte à la création du pod (redéployer pour changer). Filtre invalide : ignoré avec un avertissement d'admission |

Posées **par** le webhook : annotation `mimic.platform.io/status: injected` et label
`mimic.platform.io/sidecar: "true"` (ce label ouvre l'accès à l'API du contrôleur par sa
NetworkPolicy).

`mimic.platform.io/mode` n'est lue que par un sidecar **sans** contrôleur dont la
configuration renseigne `annotations_file` ; un sidecar piloté l'ignore.

### Ce que le webhook ajoute

| Élément | Détail |
|---------|--------|
| init container `mimic-init` | en tête des init containers, ou juste après ceux du mesh ; root, `NET_ADMIN` et `NET_RAW`, toutes les autres capacités retirées ; variables `MIMIC_MESH`, `POD_IP` (`status.podIP`), `MIMIC_INBOUND_PORTS`, `MIMIC_EXCLUDE_OUTBOUND_CIDRS`, et si besoin `MIMIC_EXCLUDE_OUTBOUND_PORTS`, `MIMIC_IPTABLES` |
| native sidecar `mimic` | juste après `mimic-init`, `restartPolicy: Always`, UID et GID 1338, racine en lecture seule, port `mimic-admin` (15100) ; variables `POD_NAME`, `POD_NAMESPACE`, `MIMIC_WORKLOAD` (figée à l'admission), `MIMIC_VERSION` (label `version_label`), `MIMIC_POD_TEMPLATE_HASH` (label `pod-template-hash`), `MIMIC_REDACT_SALT` (Secret `<workload>-mimic`, clé `redact-salt`, facultatif), `MIMIC_STORE_SIZE_LIMIT_BYTES` (taille de l'`emptyDir` du store), `MIMIC_LOG` (annotation `log` ou `sidecar_log`, sinon absente), `MIMIC_CONFIG_YAML` ou `--config` |
| sondes du sidecar | `startupProbe` `/readyz` (période 1 s, `share.startup.timeout` + 30 échecs : 60 avec `sidecar_config` par défaut, 330 avec l'annotation `config`, que le webhook ne lit pas), `readinessProbe` `/readyz` et `livenessProbe` `/healthz` (période 10 s, 3 échecs) ; HTTP sur `mimic-admin`, ou `exec` `mimic-proxy --probe <chemin>` si un conteneur du mesh est présent |
| ressources du sidecar | celles de `resources` du webhook, sinon requêtes `cpu: 50m`, `memory: 64Mi`, limite `memory: 256Mi` |
| volumes | `mimic-store` (`emptyDir`, `sizeLimit: 512Mi`, sur `/var/lib/mimic`), `mimic-token` (jeton projeté d'audience `mimic-controller`, 3600 s, sur `/var/run/secrets/mimic`), `mimic-controller-ca` (ConfigMap, sur `/etc/mimic-ca`), `mimic-ca` (`emptyDir`, sur `/var/run/mimic/ca`), `mimic-config` (avec l'annotation `config`) |

Pods laissés tels quels, avec un avertissement d'admission `mimic non injecté : <raison>` :
`hostNetwork: true`, sans nom de workload, `inject: "false"`. Un pod déjà injecté (rappel du
webhook par l'API server, sauf adaptation à un mesh injecté après) est admis sans patch ni
avertissement.

## Webhook (`mimic-injector`)

Fichier passé par `--config` (ou `-c`, ou `MIMIC_INJECTOR_CONFIG`), obligatoire. Logs : filtre
`RUST_LOG` (`info` par défaut).

| Champ | Défaut | Effet |
|-------|--------|-------|
| `listen` | `0.0.0.0:8443` | écoute du webhook |
| `tls.cert_file`, `tls.key_file` | absent | certificat servi à l'API server ; absent, HTTP en clair (tests uniquement) |
| `image` | **requis** | image du sidecar injecté (`ghcr.io/thaliph/mimic`) |
| `init_image` | **requis** sauf `cni: true` | image de `mimic-init` (`ghcr.io/thaliph/mimic-init`, avec iptables) |
| `image_pull_policy` | absent | `imagePullPolicy` des conteneurs injectés |
| `exclude_outbound_cidrs` | `[]` | CIDR jamais redirigés (API Kubernetes en tête) |
| `iptables` | absent | `MIMIC_IPTABLES` de `mimic-init` (`iptables-legacy` selon les nœuds) |
| `sidecar_config` | **requis** | YAML de la configuration par défaut des sidecars, vérifié au démarrage (`sidecar_config invalide: …`) |
| `resources` | absent | `resources` du sidecar (format Kubernetes) ; absent : 50m / 64Mi, limite 256Mi |
| `controller_ca_configmap` | `mimic-controller-ca` | ConfigMap de la CA du contrôleur, publiée par trust-manager |
| `token_audience` | `mimic-controller` | audience du jeton projeté ; le contrôleur vérifie `mimic-controller` |
| `inbound` | `false` | `true` : interception entrante de tous les pods injectés (découverte de l'API du service, `mimicctl load`) ; sinon par namespace (label `mimic.platform.io/inbound=true`) ou par pod (annotation `inbound-ports`). Le mock ne sert jamais que le sortant |
| `cni` | `false` | `true` : pas de `mimic-init` ; réglages passés au plugin `mimic-cni` par l'annotation `mimic.platform.io/cni-rules` (exige le DaemonSet de `deploy/cni/`) |
| `version_label` | `app.kubernetes.io/version` | label du pod repris dans `MIMIC_VERSION` (`rollouts-pod-template-hash` avec Argo Rollouts, par exemple) |
| `sidecar_log` | absent | `MIMIC_LOG` des sidecars (filtre `EnvFilter`), sauf annotation `mimic.platform.io/log` ; absent : `info`. Filtre invalide : refusé au démarrage (`sidecar_log: …`) |

`MutatingWebhookConfiguration` livrée : `failurePolicy: Ignore`, `timeoutSeconds: 5`,
`sideEffects: None`, `reinvocationPolicy: IfNeeded`, opérations `CREATE` sur `pods`,
`namespaceSelector` `mimic.platform.io/enabled: "true"`, `caBundle` posé par le cainjector
(`cert-manager.io/inject-ca-from: mimic-system/mimic-injector-tls`).

## Init container (`mimic-init`)

Chaque option a sa variable d'environnement ; le webhook ne pose que celles marquées ✓.

| Option | Variable | Défaut | Effet |
|--------|----------|--------|-------|
| `--proxy-uid` | `MIMIC_PROXY_UID` | `1338` | UID du sidecar, jamais redirigé |
| `--inbound-port` | `MIMIC_INBOUND_PORT` | `15106` | port d'écoute entrante du sidecar |
| `--outbound-port` | `MIMIC_OUTBOUND_PORT` | `15101` | port d'écoute sortante du sidecar |
| `--admin-port` | `MIMIC_ADMIN_PORT` | `15100` | port d'admin, exclu des redirections |
| `--inbound-ports` | `MIMIC_INBOUND_PORTS` ✓ | `*` | ports entrants interceptés : `*`, liste, ou vide / `-` pour aucun |
| `--exclude-inbound-ports` | `MIMIC_EXCLUDE_INBOUND_PORTS` | vide | ports entrants jamais redirigés |
| `--no-outbound` | `MIMIC_NO_OUTBOUND` | faux | désactive l'interception sortante |
| `--exclude-outbound-cidrs` | `MIMIC_EXCLUDE_OUTBOUND_CIDRS` ✓ | vide | CIDR sortants jamais redirigés |
| `--exclude-outbound-ports` | `MIMIC_EXCLUDE_OUTBOUND_PORTS` ✓ | vide | ports sortants jamais redirigés |
| `--ipv6` | `MIMIC_IPV6` | faux | pose aussi les règles `ip6tables` |
| `--iptables` | `MIMIC_IPTABLES` ✓ | `iptables` | binaire iptables (`iptables-legacy` selon les nœuds) |
| `--mesh` | `MIMIC_MESH` ✓ | `auto` | `auto`, `none`, `istio`, `istio-ambient`, `linkerd` |
| `--pod-ip` | `POD_IP` ✓ | absent | IP du pod, cible du trajet entrant en Istio ambient |
| `--intercept-loopback` | `MIMIC_INTERCEPT_LOOPBACK` | faux | redirige aussi le trafic vers `127.0.0.1` et `::1`. Par défaut, le trafic local du pod (`-o lo` : autres conteneurs, IP du pod) n'est pas intercepté, comme sous Istio. Sert aux tests en namespace réseau ; à ne pas poser dans un pod (un agent OTel ou un `cloud-sql-proxy` voisin recevrait des `599` en mock) |
| `--dry-run` | | | affiche les règles sans les appliquer |

Les ports du sidecar (15100, 15101, 15106) sont toujours exclus. Si on change
`MIMIC_INBOUND_PORT`, `MIMIC_OUTBOUND_PORT` ou `MIMIC_ADMIN_PORT`, adapter la configuration du
sidecar en conséquence : le webhook ne le fait pas. Le webhook ne pose pas `MIMIC_IPV6` : le
trafic IPv6 d'un pod injecté n'est pas intercepté. Il ne pose pas non plus
`MIMIC_INTERCEPT_LOOPBACK`.

## Contrôleur (`mimic-controller`)

Ligne de commande : `mimic-controller --config <fichier>` (variable `MIMIC_CONTROLLER_CONFIG`) ;
`mimic-controller crd` affiche le manifest de la CRD (celui de `deploy/controller/crd.yaml`).
Sans fichier, les défauts s'appliquent, mais ils échouent à la validation (`auth: kubernetes`
exige `tls`) : un fichier est nécessaire en pratique.

| Code de sortie | Cause |
|----------------|-------|
| 2 | configuration invalide (message sur la sortie d'erreur) |
| 1 | échec du démarrage (écoute, certificat, client Kubernetes), ou bail perdu : la réplique cesse de servir et Kubernetes la relance en passive |

Variables : `MIMIC_LOG` (filtre des logs, `info`), `POD_NAME` puis `HOSTNAME` (identité dans
l'élection), `AWS_*` (support `s3://`).

| Champ | Défaut | Effet |
|-------|--------|-------|
| `listen` | `0.0.0.0:8443` | API des sidecars (HTTPS si `tls` est renseigné) |
| `ui_listen` | `127.0.0.1:8081` | interface en lecture seule, API de lecture et `/metrics` ; joignable par port-forward |
| `tls.cert_file`, `tls.key_file` | absent | certificat de l'API des sidecars ; **requis** avec `auth` `kubernetes` |
| `auth` | `{kind: kubernetes}` | `kubernetes` : jetons validés par `TokenReview`, surveillance des `MimicWorkload` et des namespaces. `{kind: static, tokens: {<jeton>: {namespace, pod, workload, labels}}}` : tests et développement hors cluster |
| `store` | `file:///var/lib/mimic-controller` | support : `file:///chemin` ou `s3://bucket/prefixe` |
| `retention` | `86400` (24 h) | segments supprimés au-delà |
| `max_exchanges_per_workload` | `200000` | échanges en mémoire par workload, les plus récents. Doit être positif |
| `max_exchanges_total` | `1000000` | tous workloads confondus ; au-delà, le workload qui en a le plus perd ses plus anciens |
| `samples_per_operation` | `20` | bodies gardés par couple (opération, code), en mémoire et après compaction |
| `max_bytes_per_workload` | `268435456` (256 MiB) | volume des segments d'un workload sur le support ; au-delà, les plus anciens sont supprimés |
| `max_exemplars_per_workload` | `10000` | endpoints gardés avec un exemple, hors rétention. Doit être positif |
| `max_exemplar_bytes_per_workload` | `67108864` (64 MiB) | taille sérialisée des exemples d'un workload |
| `max_exemplar_bytes_total` | `536870912` (512 MiB) | tous workloads confondus |
| `discovery_interval` | `30` | au plus un recalcul de la découverte d'un workload par intervalle |
| `snapshot.max_per_key` | `50` | échanges gardés par (authority, méthode, chemin, code) dans un snapshot. Doit être positif |
| `pod_timeout` | `30` | pod sans battement depuis ce délai : `absent` |
| `pod_forget` | `600` | pod sans battement depuis ce délai : oublié (interface, statut, métriques) |
| `leader_election` | absent | absent : une seule réplique, toujours active, sans bail |
| `leader_election.lease` | `mimic-controller` | nom du bail `coordination.k8s.io/v1` |
| `leader_election.namespace` | `mimic-system` | namespace du bail et des pods du contrôleur |
| `leader_election.identity` | absent | identité de la réplique ; absente : `POD_NAME`, sinon `HOSTNAME` |
| `leader_election.lease_duration` | `15` | bail inchangé depuis ce délai : expiré |
| `leader_election.renew_deadline` | `10` | l'active s'arrête si son dernier renouvellement réussi a commencé il y a plus longtemps |
| `leader_election.retry_period` | `2` | renouvellement par l'active, relecture par la passive |

Contraintes vérifiées au démarrage : `0 < retry_period < renew_deadline < lease_duration` ;
`leader_election` exige `auth` `kubernetes` ; `lease` et `namespace` non vides.

> **Écart avec la spec.** La spec (§9.1) écrit `auth: kubernetes` ; le code attend la forme
> étiquetée `auth: {kind: kubernetes}` (c'est celle de `deploy/controller/controller.yaml`).

Valeurs fixes, non réglables : passe de maintenance (exemples, rétention, compaction, plafond
de volume) toutes les 60 s ; statut des `MimicWorkload` publié toutes les 5 s s'il a changé ;
cache des validations de jetons 60 s ; segment reçu de 64 MiB au plus ; compaction des heures
révolues depuis plus de 10 min ; formes de mock gardées en mémoire, compressées, 128 MiB au
plus ; attente de la liste initiale des `MimicWorkload` à la prise de relais, 30 s au plus.

## Ressource `MimicWorkload`

`apiVersion: mimic.platform.io/v1alpha1`, `kind: MimicWorkload`, ressource **namespacée**, nom
court `mw`. Son nom est celui du workload (`mimic.platform.io/workload`, sinon
`app.kubernetes.io/name`, sinon `app`) et elle vit dans le namespace de l'application. Une
ressource par workload ; sans ressource, chaque sidecar reste dans le mode de sa configuration.

```yaml
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata:
  name: checkout
  namespace: shop
spec:
  mode: mock
  snapshot: 3f9a0c1e5b7d2a46        # facultatif
  selector:                          # facultatif
    matchLabels: {pod-template-hash: 7d9f8c6b5}
  capture: {inbound: metadata}       # facultatif
  mock: {websocket: refuse}          # facultatif
```

### `spec`

| Champ | Type | Obligatoire | Effet |
|-------|------|-------------|-------|
| `spec.mode` | `passthrough`, `record`, `mock`, `mock-partial` | oui | mode souhaité des pods sélectionnés |
| `spec.snapshot` | chaîne | non | snapshot à servir en mock ; absent, le contrôleur en construit un à l'entrée en mock et le garde tant que le mode reste un mock. Inconnu : `Aligned=False`, raison `SnapshotNotFound`, état souhaité inchangé |
| `spec.selector.matchLabels` | table clé → valeur | non | seuls les pods qui portent ces labels appliquent l'état souhaité |
| `spec.selector.matchExpressions[]` | `key`, `operator`, `values` | non | opérateurs `In`, `NotIn` (avec `values`), `Exists`, `DoesNotExist` (sans `values`) ; combinés par un ET avec `matchLabels`. Opérateur inconnu ou `values` mal renseigné : aucun pod sélectionné. Sélecteur vide : tous les pods |
| `spec.capture.inbound` | `full`, `metadata`, `off` | non | capture de l'entrant imposée aux pods sélectionnés ; absente, celle de leur configuration |
| `spec.mock.websocket` | `relay`, `refuse` | non | poignée de main WebSocket sortante en mock ; absente, celle de leur configuration |

Les pods non sélectionnés reçoivent un état vide : ils reviennent au mode, à la capture et au
réglage WebSocket de **leur configuration** (`record` avec l'injecteur). Les labels comparés
sont ceux que le contrôleur lit dans l'API Kubernetes lors de l'authentification du pod (gardés
60 s), jamais ce qu'annonce le sidecar.

### `status` (écrit par le contrôleur)

| Champ | Contenu |
|-------|---------|
| `status.snapshot` | snapshot servi (vide hors mock) |
| `status.pods` | pods sélectionnés vus depuis moins de `pod_timeout` |
| `status.aligned` | parmi eux, ceux qui appliquent mode, snapshot, capture et réglage WebSocket demandés |
| `status.observedGeneration` | génération de la `spec` décrite par ce statut |
| `status.conditions[]` | `type`, `status` (`"True"`/`"False"`), `reason`, `message` |

| Condition | `status` | `reason` |
|-----------|----------|----------|
| `Aligned` | `True` | tous les pods sélectionnés vus récemment sont alignés |
| `Aligned` | `False` | `PodsDiverging` (au moins un pod pas encore aligné), `NoPods` (aucun pod sélectionné vu), `SnapshotNotFound`, `SnapshotFailed` (construction ou écriture du snapshot échouée, log `construction du snapshot échouée`), `ProductionNamespace`, `InvalidMode`, `InvalidCapture`, `InvalidMock` |
| `Allowed` | `True` | mode autorisé |
| `Allowed` | `False` | `ProductionNamespace`, message `mock et mock-partial sont refusés dans un namespace labellisé env=production` |

Colonnes de `kubectl get mw` : `Mode` (`.spec.mode`), `Snapshot`, `Aligned`, `Pods`.

Le schéma de la CRD n'accepte que les valeurs énumérées ci-dessus pour `mode`,
`capture.inbound` et `mock.websocket` ; les raisons `Invalid*` ne se voient que si une valeur
contourne ce schéma.
