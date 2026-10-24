# Développeur applicatif

[← Sommaire](README.md)

Cette page s'adresse à l'équipe qui possède un service dont les pods reçoivent mimic : ce qui
change dans le pod, ce qu'il faut demander à l'équipe plateforme, comment lire ce que mimic a
enregistré, et comment interpréter les réponses que le sidecar fabrique lui-même (`599`,
`502`, `504`, `413`…). Aucune modification du code de l'application n'est nécessaire.

## Glossaire

| Terme | Sens |
|-------|------|
| **workload** | le service vu par mimic : tous les pods d'un même nom (annotation `mimic.platform.io/workload`, sinon label `app.kubernetes.io/name`, sinon `app`). Les enregistrements, le mode et les snapshots sont par workload, pas par pod |
| **sidecar** | le conteneur `mimic` ajouté au pod (native sidecar : il démarre avant l'application et s'arrête après elle). Le trafic du pod le traverse grâce aux règles iptables posées par l'init container `mimic-init` |
| **entrant / sortant** | entrant : les requêtes **reçues** par l'application ; sortant : les appels de l'application **vers ses dépendances** (autres services, SaaS) |
| **échange** | une requête et sa réponse, enregistrées (headers et bodies redactés, durées) |
| **opération** | une route découverte : méthode et forme de chemin (`GET /users/{id}`) |
| **authority** | `hôte:port` d'une dépendance (`api.stripe.com:443`) |
| **`record`** | mode par défaut : le trafic passe normalement et est enregistré |
| **`mock`** | les appels sortants ne quittent plus le pod : le sidecar répond avec les réponses enregistrées. L'entrant reste réel |
| **`mock-partial`** | comme `mock`, mais un appel sans enregistrement est relayé à la vraie dépendance (et enregistré) au lieu d'échouer |
| **`passthrough`** | le sidecar relaie sans rien enregistrer |
| **snapshot** | jeu d'échanges figé, identique sur tous les pods du workload, servi en `mock` |
| **`MimicWorkload`** | ressource Kubernetes de votre namespace qui déclare le mode souhaité du workload ; `mimicctl mode` l'écrit |
| **aligné** | un pod est aligné quand il applique le mode et le snapshot souhaités |
| **contrôleur** | composant central (`mimic-system`) qui agrège les enregistrements, construit les snapshots et pilote les bascules |
| **interception TLS** | pour les hôtes HTTPS listés, le sidecar déchiffre le trafic avec une CA mimic à laquelle l'application doit faire confiance ; sans elle, un appel HTTPS est relayé chiffré, sans être enregistré ni mockable |

## Ce que la plateforme doit fournir

À demander à l'équipe plateforme, une fois par namespace :

| Besoin | Ce que la plateforme fait | Vérifier |
|--------|---------------------------|----------|
| injection dans le namespace | label `mimic.platform.io/enabled=true` (tous les pods), ou `=opt-in` (seulement vos pods labellisés `mimic.platform.io/inject: "true"`), et Pod Security `privileged` (l'init container pose des règles iptables avec `NET_ADMIN`) ; puis vos pods sont recréés | `kubectl get pod <pod> -o jsonpath='{.metadata.annotations.mimic\.platform\.io/status}'` vaut `injected` |
| lire vos données (`export`, `requests`, `report`, `snapshot ls`) | RoleBinding du ClusterRole `mimic-data-viewer` dans **votre** namespace : vous lisez vos workloads, rien d'autre | `kubectl auth can-i get workloads.data.mimic.platform.io/requests -n <ns>` |
| voir le mode et l'alignement (`mimicctl status`) | ClusterRole `mimic-viewer`, déjà agrégé au rôle standard `view` | `kubectl auth can-i list mimicworkloads -n <ns>` |
| basculer (`mimicctl mode`) | RoleBinding du ClusterRole `mimic-operator` dans votre namespace | `kubectl auth can-i patch mimicworkloads -n <ns>` |
| sel de redaction partagé entre clusters (facultatif) | Secret `<workload>-mimic`, clé `redact-salt` ; sans lui, le contrôleur fournit un sel propre au workload | `kubectl -n <ns> get secret <workload>-mimic` |

Vous n'avez besoin d'**aucun** droit dans `mimic-system` : les lectures passent par l'API
Kubernetes (`kubectl get --raw /apis/data.mimic.platform.io/v1alpha1/namespaces/<ns>/workloads/<wl>/operations`
fonctionne aussi), qui vérifie vos droits sur votre namespace.

## Ce qui change dans le pod

- un init container `mimic-init` et un sidecar `mimic` (UID 1338), qui demande 50m de CPU et
  64 Mi de mémoire, limité à 256 Mi ;
- l'application ne démarre qu'une fois le sidecar prêt (et, sous contrôleur, le mode souhaité
  connu) : quelques secondes de plus au démarrage ;
- le trafic local du pod (`127.0.0.1`, `::1`) n'est pas intercepté ;
- avec l'annotation `mimic.platform.io/tls-intercept: api.stripe.com,*.salesforce.com`, vos
  conteneurs reçoivent `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE` et
  `NODE_EXTRA_CA_CERTS` vers la CA mimic (sauf si vous les définissez déjà). Une JVM ne les lit
  pas : importer `/var/run/mimic/ca/ca.crt` dans son magasin (`keytool`). Un client qui épingle
  le certificat du serveur refusera l'interception ;
- annotations utiles : `mimic.platform.io/inject: "false"` (pas de mimic pour ce pod),
  `mimic.platform.io/log: debug` (logs du sidecar plus bavards). Toutes les annotations :
  [Configurer](Configurer.md#annotations-de-pod).

Les headers d'authentification, cookies, paramètres et champs JSON dont le nom contient
`token`, `secret`, `key`, `session`, `password`, et les e-mails, numéros de carte, IBAN et JWT
des bodies JSON, sont **redactés avant d'être enregistrés** (`<redacted:sha256:…>`). Les bodies
qui ne sont ni JSON ni formulaire (XML, texte, multipart, gRPC, binaire, ou sans
`Content-Type`) ne sont **pas enregistrés** : une dépendance qui répond ainsi n'est pas
mockable (`599`) sauf exception, `mimic.platform.io/capture-raw-bodies: api.partner.io`
(et `inbound` pour vos propres requêtes, à rejouer en tir de charge), qui les enregistre en
clair. Détail : [Collecte et stockage](Collecte-et-stockage.md#redaction).

## Lire ce que mimic a enregistré

```sh
mimicctl status checkout -n shop                                      # mode, snapshot, pods alignés, requêtes sans réponse
mimicctl report checkout -n shop > report.md                          # API découvertes, appels les plus fréquents, headers
mimicctl export checkout -n shop --inbound -o checkout.openapi.yaml   # l'API de votre service
mimicctl export checkout -n shop --outbound api.stripe.com -o stripe.yaml
mimicctl requests checkout -n shop --direction outbound --limit 20    # appels à vos dépendances, tels que le mock les rejouera
mimicctl requests checkout -n shop --format k6 > replay.js            # rejeu des requêtes reçues
```

Avant de basculer en `mock`, vérifier avec `requests --direction outbound` (ou le rapport)
que chaque appel que fera le test a été enregistré : un appel jamais vu en `record` recevra un
`599`. Référence complète : [CLI mimicctl](CLI-mimicctl.md).

Le sidecar a aussi sa page, pour **ce pod** seulement :
`kubectl -n shop port-forward pod/<pod> 15100`, puis `http://127.0.0.1:15100/`.

## Mocker vos dépendances sur votre poste ou en CI

Le snapshot de votre workload sert aussi hors du cluster : `mimicctl pull` le télécharge,
`mimic-proxy --mock-from` le sert, et votre application passe par lui en proxy explicite.

```sh
mimicctl pull checkout -n shop -o snapshot.jsonl
mimic-proxy --mock-from snapshot.jsonl &                    # écoute 127.0.0.1:15101
HTTP_PROXY=http://127.0.0.1:15101 HTTPS_PROXY=http://127.0.0.1:15101 ./mon-service
```

Les appels enregistrés reçoivent leur réponse, les autres `599`. Une dépendance HTTPS doit
être dans `tls.intercept` (et l'application faire confiance à la CA publiée dans
`tls.ca_dir`). Exemple docker-compose prêt à l'emploi : `deploy/local/`.

## Headers ajoutés par le sidecar

| Header | Sur | Valeur | Sens |
|--------|-----|--------|------|
| `x-mimic-match` | réponse mockée | `exact;score=1.00`, `template;score=…`, `fuzzy;score=…` | la réponse vient d'un enregistrement : `exact` pour la même requête, `template` pour la même route avec d'autres valeurs (`/users/8` servi par `/users/7`), `fuzzy` pour un body ou une query différents. Absent si `mock.debug_header: false` |
| `x-mimic-unmatched` | réponse `599` | `true` | aucun enregistrement ne correspond : voir ci-dessous |
| `x-mimic-websocket` | réponse `503` | `refused` | poignée de main WebSocket sortante refusée en mock (`mock.websocket: refuse`) |

Une réponse **sans** `x-mimic-match` en mode `mock` est une réponse d'erreur du sidecar
(tableau suivant) ou, en `mock-partial`, celle de la vraie dépendance. Les documents OpenAPI
exportés portent aussi des extensions `x-mimic-stats` (appels, codes, latences) et
`x-mimic-direction`, ignorées par les outils standards.

## Réponses fabriquées par le sidecar

Le sidecar répond lui-même dans les cas suivants, avec un body texte qui commence par
`mimic: ` : c'est le moyen sûr de les distinguer d'une réponse de votre dépendance.

| Code | Body | Quand | Que faire |
|------|------|-------|-----------|
| `599` | `mimic: aucun enregistrement ne correspond à cette requête` | `mock` : aucun échange rejouable pour cet appel sortant (header `x-mimic-unmatched: true`). Compté dans `mimicctl status` (`UNMATCHED`) et listé dans `mimicctl report` | enregistrer plus longtemps pour couvrir l'appel, passer en `mock-partial`, vérifier que l'hôte HTTPS est dans `tls-intercept`, ou, pour une réponse XML, texte ou gRPC, l'ajouter à `capture-raw-bodies` |
| `413` | `mimic: body trop gros pour le mode mock` | `mock` : body de requête au-delà de `capture.max_body_bytes` (1 MiB), qui ne peut pas être comparé aux enregistrements | réduire le body, ou demander une limite plus haute |
| `502` | `mimic: amont injoignable` | `record`/`passthrough`/`mock-partial` : la dépendance (sortant) ou votre application (entrant) ne répond pas : connexion refusée, coupée, réponse illisible | c'est la vraie dépendance qui est en panne, ou votre application qui n'écoute pas encore (démarrage lent) ou sur un autre port |
| `504` | `mimic: connexion vers l'amont non établie dans le délai (outbound.connect_timeout)` | la connexion vers la dépendance ou l'application n'a pas abouti dans le délai | vraie dépendance lente ou filtrée (NetworkPolicy, pare-feu) |
| `503` | `mimic: WebSocket sortant refusé en mock (mock.websocket: refuse)` | `mock` avec `mock.websocket: refuse` : poignée de main WebSocket sortante (header `x-mimic-websocket: refused`) | attendu pendant un tir qui coupe les WebSocket ; sinon repasser `--mock-websocket relay` |
| `403` | `mimic: tunnel refusé en mode mock (TLS non intercepté)` | `mock` : `CONNECT` (proxy explicite) vers un hôte HTTPS non intercepté, qui joindrait sinon la vraie dépendance | ajouter l'hôte à `tls-intercept` et ré-enregistrer, ou l'exclure s'il doit rester réel |
| `502` | `mimic: aucune destination pour le trafic entrant` | requête entrante dont la destination d'origine est inconnue | configuration du pod (ports entrants) : voir [Dépannage](Depannage.md) |
| `400` | `mimic: requête sans destination (ni proxy explicite, ni redirection)` | requête sortante qui n'a ni URL absolue ni destination redirigée | appel fait directement au port du sidecar : passer par l'URL réelle |
| `400` | `mimic: URI invalide` | URL de la requête non analysable | corriger l'URL envoyée |
| `400` | `mimic: CONNECT sans destination` | `CONNECT` sans `hôte:port` | client mal configuré |

Hors HTTP, en `mock`, une connexion sortante TCP non HTTP (base de données, broker, cache) ou
HTTPS non interceptée est **fermée sans réponse** : votre client voit une connexion
réinitialisée. C'est voulu (aucune dépendance réelle ne reçoit la charge) ; exclure ces ports
(`mimic.platform.io/exclude-outbound-ports`) s'ils doivent rester joignables.

En `mock`, la latence enregistrée est rejouée : vos délais d'attente doivent la tolérer comme
en `record`.
