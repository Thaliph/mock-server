# Modes

[← Sommaire](README.md)

Le mode d'un sidecar décide de ce qu'il fait du trafic **sortant** (appels de l'application
vers ses dépendances). Le trafic **entrant** est toujours relayé vers l'application, dans tous
les modes : on mesure le vrai service.

Sources : `crates/mimic-proxy/src/proxy.rs`, `engine.rs`, `crates/mimic-core/src/mock.rs`.

## Les quatre modes

| Mode | Entrant | Sortant HTTP | Enregistrement | Usage |
|------|---------|--------------|----------------|-------|
| `passthrough` | relayé | relayé | aucun : métriques seules (durée jusqu'au premier octet) | coût minimal, désactiver mimic sans retirer le sidecar |
| `record` (défaut) | relayé | relayé | entrant selon `capture.inbound`, sortant complet | découverte de l'API, constitution des enregistrements |
| `mock` | relayé | **servi par mimic** depuis les enregistrements ; jamais envoyé à la dépendance | entrant selon `capture.inbound` ; sortant : rien (sauf `mock.unmatched: passthrough`) | tir de charge isolé des dépendances |
| `mock-partial` | relayé | servi par mimic si un enregistrement correspond, **sinon relayé** et enregistré | entrant selon `capture.inbound`, sortant relayé enregistré | compléter les enregistrements pendant un tir, migrer progressivement |

Le mode de démarrage est `mode` de la configuration (`record` avec l'injecteur). Il change :

- sous contrôleur : par la ressource `MimicWorkload` (voir [Basculer en mock](Basculer-en-mock.md)) ;
- sans contrôleur : par `PUT /mode` de l'API d'admin (voir
  [Administration et API](Administration-et-API.md)) ou par l'annotation `mimic.platform.io/mode`
  quand `annotations_file` est configuré.

Le changement est immédiat, sans redémarrage ; chaque requête prend le mode courant à son
arrivée. Log : `changement de mode`, métrique `mimic_mode{mode}`.

## Ce que fait chaque mode, cas par cas

### Trafic sortant

| Trafic sortant | `passthrough` | `record` | `mock` | `mock-partial` |
|----------------|---------------|----------|--------|----------------|
| HTTP/1, HTTP/2, gRPC en clair | relayé | relayé, enregistré | moteur de mock | moteur, sinon relayé et enregistré |
| HTTPS d'un hôte de `tls.intercept` | déchiffré, relayé | déchiffré, relayé, enregistré | déchiffré, moteur de mock | déchiffré, moteur, sinon relayé |
| HTTPS non intercepté (redirigé) | tunnel opaque | tunnel opaque | **connexion fermée** (`mimic_passthrough_connections_total{kind="tls",result="refused"}`) | tunnel opaque |
| `CONNECT` (proxy explicite) non intercepté | tunnel | tunnel | **`403`** `mimic: tunnel refusé en mode mock (TLS non intercepté)` | tunnel |
| Poignée de main WebSocket | relayée | relayée, enregistrée | relayée et enregistrée, ou `503` avec `mock.websocket: refuse` | idem `mock` |
| TCP non HTTP (Redis, PostgreSQL, Kafka…) | tunnel opaque | tunnel opaque | **tunnel opaque** (la dépendance est jointe) | tunnel opaque |
| Connexion muette (l'application n'écrit rien) | après 300 ms, le premier qui parle décide (serveur d'abord : tunnel) | idem | attend l'application **30 s** sans rien ouvrir, puis tunnel | idem `mock` |

À retenir pour un tir en `mock` :

- **Seul le HTTP est mocké.** Les bases de données, files de messages et autres protocoles TCP
  restent joints réellement. Les exclure de la mesure (ou les fournir) relève du plan de tir.
- **Un HTTPS non listé dans `tls.intercept` est coupé.** L'application voit une connexion
  fermée (ou un `403` sur `CONNECT`). Ajouter l'hôte à `mimic.platform.io/tls-intercept` avant
  l'enregistrement, pas seulement avant le mock : sans interception, rien n'a été enregistré.
- **Les protocoles où le serveur parle en premier** (MySQL, SMTP) attendent 30 s en `mock` et
  `mock-partial` avant d'être relayés : en mock, mimic ne joint pas la dépendance pour savoir
  qui parle d'abord. Exclure leurs ports (`mimic.platform.io/exclude-outbound-ports`) si ces
  connexions doivent rester rapides.

### Trafic entrant

Toujours relayé vers l'application. Hors `passthrough`, enregistré selon `capture.inbound`
(réglable pendant un tir par `spec.capture.inbound`) :

| `capture.inbound` | Gardé | Coût |
|-------------------|-------|------|
| `full` (défaut) | échange complet (query, headers, bodies échantillonnés, redactés), journalisé et publié | le plus élevé |
| `metadata` | méthode, chemin, code, durées, tailles ; **en mémoire seulement**, ni journalisé ni publié au contrôleur | faible |
| `off` | rien ; seules les métriques (durée mesurée jusqu'au dernier octet) | minimal |

Pendant un tir à fort débit, `metadata` ou `off` évite que la capture de l'entrant ne pèse sur
la mesure. Les requêtes de rejeu exportées par `mimicctl requests` ne viennent que des échanges
`full`.

### Réponses produites par mimic lui-même

| Code | Body | Quand |
|------|------|-------|
| `599` | `mimic: aucun enregistrement ne correspond à cette requête`, header `x-mimic-unmatched: true` | `mock`, aucun enregistrement pour l'opération, `mock.unmatched: error` |
| `413` | `mimic: body trop gros pour le mode mock` | `mock`/`mock-partial`, body de requête au-delà de `capture.max_body_bytes` |
| `503` | `mimic: WebSocket sortant refusé en mock (mock.websocket: refuse)`, header `x-mimic-websocket: refused` | poignée de main WebSocket en mock avec `refuse` |
| `403` | `mimic: tunnel refusé en mode mock (TLS non intercepté)` | `CONNECT` vers un hôte non intercepté, en `mock` |
| `502` | `mimic: amont injoignable` | dépendance (ou application, en entrant) injoignable, ou erreur pendant la requête |
| `502` | `mimic: aucune destination pour le trafic entrant` | connexion entrante sans destination d'origine ni `inbound.upstream` |
| `400` | `mimic: requête sans destination (ni proxy explicite, ni redirection)` | requête sur l'écoute sortante sans URI absolue ni destination iptables |
| `400` | `mimic: CONNECT sans destination`, `mimic: URI invalide` | requêtes de proxy mal formées |

Le `599` est volontairement hors des codes HTTP usuels : on le distingue d'une vraie erreur de
dépendance dans les métriques (`mimic_requests_total{status="599"}`) et dans les logs de
l'application.

## Le moteur de mock

### Ce qui est rejouable

Le moteur est construit à l'entrée en mock (ou au changement de snapshot), sur :

- le **snapshot** reçu du contrôleur s'il y en a un (jeu figé, identique pour tous les pods du
  workload) ;
- sinon les échanges sortants du **store local** du sidecar (ce que ce pod a vu, plus les
  échanges importés en partage direct).

Seuls sont rejouables les échanges sortants avec une réponse finale (code ≥ 200) et des
bodies complets ou vides. Un échange gardé sans body (au-delà de
`capture.samples_per_operation` par couple opération/code, ou body tronqué, ou body non
redactable) n'est pas rejoué, mais sa **durée** entre dans la distribution de latence de son
opération.

Sans snapshot, le store continue d'évoluer (en `mock-partial`, chaque échange relayé y entre) :
le moteur est reconstruit en tâche de fond au plus une fois par seconde, et une requête sans
correspondance est d'abord comparée aux échanges arrivés depuis la dernière construction.
Avec un snapshot, le moteur est figé : en `mock-partial`, une requête sans correspondance est
relayée **à chaque fois**. Compteur : `mimic_mock_engine_builds_total{trigger=switch|refresh|unmatched}`.

### Choix de la réponse

1. **Opération.** Les candidats sont les enregistrements de même authority (`hôte:port`), même
   méthode et même **forme de chemin** : un segment qui ressemble à un identifiant (entier,
   UUID, ULID, hexadécimal, base64url, date ISO) est remplacé par sa nature, donc
   `/users/42` et `/users/7` ont la même forme, pas `/users/42` et `/users/abc`. Aucun
   candidat : pas de correspondance (`599`, ou relais).
2. **Score** de chaque candidat, entre 0 et 1 :

   | Critère | Poids | Calcul |
   |---------|-------|--------|
   | chemin | 0,4 | 1 si identique, 0,5 si seulement de même forme |
   | query | 0,3 | indice de Jaccard des couples `clé=valeur` (1 si aucune des deux n'en a) |
   | headers | 0,1 | part des headers de `mock.match_headers` de même valeur (1 si la liste est vide) |
   | body | 0,2 | 1 si identiques (ou tous deux vides) ; JSON : 1 si égaux, sinon Jaccard des couples de premier niveau ; autre : 0 |

3. **Stratégie** (`mock.strategy`) parmi les candidats :

   | Stratégie | Choix |
   |-----------|-------|
   | `best` (défaut) | meilleur score ; à égalité, la réponse (code, body) la plus fréquente, puis la plus récente |
   | `round-robin` | rotation sur les candidats de meilleur score |
   | `sequence` | rejeu dans l'ordre d'enregistrement de **tous** les candidats, quel que soit le score |
   | `weighted-status` | tirage uniforme parmi les candidats de meilleur score : reproduit la répartition des codes observée |

4. **Nature de la correspondance** (header `x-mimic-match` et
   `mimic_mock_responses_total{match}`) : `template` si le chemin n'est que de même forme,
   `exact` si le score vaut 1, `fuzzy` sinon. Exemple : `x-mimic-match: fuzzy;score=0.85`.

La réponse rejouée reprend code, headers (hors headers de connexion) et body enregistrés ; le
body est réencodé selon le `content-encoding` d'origine.

### Latence rejouée

La latence est **tirée** dans la distribution des durées enregistrées de l'opération (tous ses
échanges, rejouables ou non), multipliée par `mock.latency_factor` (0 : aucune attente), et
comptée **depuis l'arrivée de la requête** : la préparation de la réponse est prise dessus.
L'attente est faite à la microseconde. Le tirage est déterministe à graine égale
(`mock.seed`). Métrique : `mimic_mock_injected_latency_seconds{authority}`.

`mimic_request_duration_seconds{direction="outbound",mode="mock"}` mesure donc la latence
servie, à comparer à la même série en `record`.

## WebSocket

Les trames ne sont jamais enregistrées : il n'y a rien à rejouer. La poignée de main HTTP/1.1
(`Upgrade: websocket`) est relayée dans tous les modes et enregistrée hors `passthrough` ; en
`mock` et `mock-partial`, `mock.websocket: refuse` (ou `spec.mock.websocket: refuse`) la refuse
avec un `503` sans joindre la dépendance. Le moteur de mock ignore les réponses `101`.
Métrique : `mimic_websocket_connections_total{direction,result=relayed|refused}`.

## Interception TLS

Pour un hôte de `tls.intercept` (comparaison sans casse, sur le SNI ou l'hôte du `CONNECT` :
`api.stripe.com` exact ; `*.salesforce.com` couvre tous les sous-domaines, à toute
profondeur, mais pas `salesforce.com` lui-même ; `*` couvre tout), le sidecar termine le
TLS avec un certificat forgé à la volée par sa CA, puis traite la requête comme du HTTP et
rouvre un TLS vers la vraie dépendance (racines système, plus `tls.upstream_ca_file`).

L'application doit faire confiance à la CA mimic : le webhook monte `ca.crt` et
`ca-bundle.crt` dans `/var/run/mimic/ca` et pose `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`,
`CURL_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS` (voir [Configurer](Configurer.md#annotations-de-pod)).
Une JVM ou une application qui épingle ses certificats ne lit pas ces variables : il faut
ajouter la CA à son magasin. Un refus par l'application se voit dans
`mimic_tls_handshake_failures_total{reason="client_rejected"}` et le log
`poignée de main TLS refusée par l'application`.

La CA est éphémère par défaut (`CN=mimic sidecar CA`, régénérée à chaque démarrage du
sidecar) ; `tls.ca_cert_file` et `tls.ca_key_file` fournissent une CA stable.
