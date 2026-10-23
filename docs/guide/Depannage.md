# Dépannage

[← Sommaire](README.md)

Symptômes, diagnostics et messages réels, composant par composant. Les logs du sidecar et du
contrôleur sont en JSON (filtre `MIMIC_LOG`, `info` par défaut) ; ceux du webhook suivent
`RUST_LOG`. Les messages cités ici sont ceux du code : les chercher tels quels.

## Premiers réflexes

```sh
# Injection et état du pod
kubectl -n shop get pod <pod> -o jsonpath='{.metadata.annotations.mimic\.platform\.io/status}{"\n"}'
kubectl -n shop describe pod <pod>
kubectl -n shop logs <pod> -c mimic-init
kubectl -n shop logs <pod> -c mimic [--previous]

# État vu par le sidecar
kubectl -n shop port-forward pod/<pod> 15100 &
curl -s http://127.0.0.1:15100/api/status | jq .

# État vu par le contrôleur
mimicctl status <workload> -n shop
kubectl -n shop get mw <workload> -o yaml
kubectl -n mimic-system get pods -L mimic.platform.io/leader
kubectl -n mimic-system logs deploy/mimic-controller --all-containers --prefix | grep -v '"INFO"'
```

## Injection

| Symptôme | Message ou indice | Cause | Correction |
|----------|-------------------|-------|------------|
| pod sans `mimic-init` ni `mimic`, aucun avertissement | annotation `mimic.platform.io/status` absente | namespace sans `mimic.platform.io/enabled=true`, webhook indisponible (`failurePolicy: Ignore`), ou pod créé avant l'activation | labelliser, vérifier `kubectl -n mimic-system get pods -l app.kubernetes.io/name=mimic-injector`, puis `kubectl rollout restart` |
| avertissement à la création | `mimic non injecté : injection désactivée par mimic.platform.io/inject` | annotation `inject: "false"` | voulu |
| | `mimic non injecté : hostNetwork : les règles iptables toucheraient le noeud` | pod `hostNetwork: true` | non pris en charge |
| | `mimic non injecté : aucun nom de workload : poser mimic.platform.io/workload ou le label app.kubernetes.io/name` | ni annotation, ni label `app.kubernetes.io/name`, ni `app` | poser l'un d'eux sur le template |
| | `mimic.platform.io/mesh inconnue : X (auto, none, istio, istio-ambient, linkerd) ; mesh détecté par mimic-init` | valeur d'annotation fausse ; pod injecté en `auto` | corriger l'annotation |
| erreur d'admission dans les logs du webhook | `revue d'admission invalide: …` | revue mal formée | version de l'API server, autre webhook |
| webhook en échec au démarrage | `mimic-injector: sidecar_config invalide: …`, `mimic-injector: configuration … invalide: …` | ConfigMap `mimic-injector` incorrecte | corriger, `rollout restart deploy/mimic-injector` |
| API server : `failed calling webhook` dans les événements | | `caBundle` pas encore posé par le cainjector, certificat expiré ou non rechargé | `kubectl get mutatingwebhookconfiguration mimic-injector -o jsonpath='{.webhooks[0].clientConfig.caBundle}'` ; redémarrer le webhook après un renouvellement |
| pod rejeté par Pod Security | `violates PodSecurity "baseline"…` (capacités `NET_ADMIN`, `NET_RAW`) | `mimic-init` a besoin de ces capacités | namespace en `privileged` ou exemption |

## `mimic-init`

Ses logs : `mimic-init: mesh <…>, binaire <…>`, les règles, puis `mimic-init: règles <binaire> posées`.

| Message | Cause | Correction |
|---------|-------|------------|
| `mimic-init: mesh inconnu: X (auto, none, istio, istio-ambient, linkerd)` | `MIMIC_MESH` invalide (ConfigMap ou pod modifié à la main) | corriger |
| `mimic-init: port invalide: X` | annotation `inbound-ports` ou `exclude-outbound-ports` mal formée | liste de ports séparés par des virgules, `*` ou `-` |
| `mimic-init: iptables-save a échoué (…)`, `mimic-init: iptables-restore a échoué (…)`, `mimic-init: iptables: <erreur>` | binaire absent, nœud en `iptables-legacy`, capacités manquantes | `iptables` du webhook (`iptables-legacy`), vérifier `NET_ADMIN` |
| `mimic-init: mesh None` dans un pod du mesh | règles du mesh pas encore posées, ou `MIMIC_MESH=none` | voir [Service mesh](#service-mesh) |

## Démarrage du sidecar

Code 2 : configuration invalide ; code 1 : échec au démarrage. Message sur la sortie
d'erreur, préfixé `mimic: `.

| Message | Cause |
|---------|-------|
| `mimic: configuration /etc/mimic/mimic.yaml invalide: unknown field …` | faute de frappe dans la ConfigMap `mimic.platform.io/config` (les champs inconnus sont refusés ; par exemple `mock.latency` au lieu de `mock.latency_factor`) |
| `mimic: MIMIC_CONFIG_YAML invalide: …` | `sidecar_config` du webhook |
| `mimic: MIMIC_MODE inconnu: X` | variable `MIMIC_MODE` |
| `mimic: share exige un nom de workload (MIMIC_WORKLOAD)` | `workload.name` vide avec `share` |
| `mimic: share.ca_file requis : seule la CA du contrôleur fait foi` | `https://` sans `share.ca_file` |
| `mimic: share.url en http:// refusée : le contrôleur doit être joint en HTTPS (share.allow_insecure pour les tests)` | `http://` sans `allow_insecure` |
| `mimic: share.url non supportée: …` | schéma autre que `https`, `http`, `file`, `s3` |
| `mimic: mock.latency_factor doit être entre 0 et 100`, `mimic: capture.max_exchanges doit être positif`, `mimic: capture.max_store_bytes doit être positif`, `mimic: store.segment_bytes et store.max_segments doivent être positifs` | bornes |
| `mimic: instance invalide: …` | nom d'instance avec des caractères hors lettres, chiffres, `-`, `.`, `_` |
| `mimic: écoute Inbound sur 0.0.0.0:15106: Address already in use` (ou `Outbound`, `Admin`) | port pris par un autre conteneur du pod |
| pod bloqué en `ContainerCreating`, événement `configmap "mimic-controller-ca" not found` | trust-manager n'a pas (encore) publié la CA dans le namespace : namespace labellisé après coup, trust-manager sans `app.trust.namespace=mimic-system` |
| sonde en échec : `mimic: sonde /healthz : connexion à 127.0.0.1:15100 : …` ou `mimic: sonde /readyz : statut …` | sidecar pas encore à l'écoute, ou `admin.listen` modifié |
| `mimic: share.startup.timeout ne peut dépasser 300 s : …`, `mimic: share.startup.timeout doit être une durée positive (en secondes)` | délai de démarrage hors de ]0, 300] |
| `… share.startup.fallback … unknown variant …` | repli inconnu : `record`, `passthrough`, `mock-local` ou `fail` |
| pod bloqué en `Init:1/2` (ou plus), événements `Startup probe failed: HTTP probe failed with statuscode: 503` du conteneur `mimic` | barrière de démarrage fermée : le sidecar attend le premier état souhaité du contrôleur. Normal quelques secondes (snapshot en cours de téléchargement) ; au-delà de `share.startup.timeout` (30 s), seulement avec `share.startup.fallback: fail` et un contrôleur injoignable (voir [Lien sidecar → contrôleur](#lien-sidecar--contrôleur)). `curl http://127.0.0.1:15100/readyz` par port-forward répond `waiting` |
| log `contrôleur injoignable au démarrage : repli share.startup.fallback appliqué, l'application démarre` | pod neuf démarré sans état souhaité : il tourne dans le repli (`record` par défaut) et s'aligne dès que le contrôleur répond ; si son workload est en mock, il a pu joindre les vraies dépendances. Rétablir le contrôleur puis recréer le pod |

## Trafic

| Symptôme | Indice | Cause | Correction |
|----------|--------|-------|------------|
| clients reçoivent `502 mimic: amont injoignable` | `mimic_upstream_errors_total{direction="inbound"}`, log `échec vers l'amont` (une fois par seconde au plus, avec `omitted`) | application pas prête ou qui écoute sur une autre adresse ; le sidecar essaie l'IP du pod puis `127.0.0.1` sur le même port | attendre la readiness de l'application ; `inbound.upstream` |
| `502 mimic: aucune destination pour le trafic entrant` | | connexion arrivée sur 15106 sans redirection iptables | appeler l'application par son port, pas 15106 |
| l'application reçoit `502 mimic: amont injoignable` vers une dépendance | `mimic_upstream_errors_total{direction="outbound"}` | dépendance injoignable (vraie panne, DNS, NetworkPolicy) | comme sans mimic |
| `400 mimic: requête sans destination (ni proxy explicite, ni redirection)` | | requête envoyée directement à 15101 sans URI absolue | |
| tout le trafic du pod échoue | sidecar arrêté (`kubectl describe pod`) | règles iptables actives sans proxy | voir [Alertes et supervision](Alertes-et-supervision.md#conduite-à-tenir) |
| appels vers l'API Kubernetes en échec | | CIDR de l'API absent de `exclude_outbound_cidrs` | compléter `exclude_outbound_cidrs` du webhook ou l'annotation `exclude-outbound-cidrs` |
| connexions MySQL / SMTP lentes (30 s) en mock | | protocoles où le serveur parle d'abord : en mock, mimic attend l'application 30 s | exclure leurs ports (`exclude-outbound-ports`) |
| trafic IPv6 non vu | | le webhook ne pose pas `MIMIC_IPV6` | IPv4 seulement |

## TLS

| Symptôme | Message | Cause | Correction |
|----------|---------|-------|------------|
| l'application refuse le certificat | log `poignée de main TLS refusée par l'application`, `mimic_tls_handshake_failures_total{reason="client_rejected"}` | elle ne fait pas confiance à la CA mimic (JVM, épinglage, variable déjà définie et non écrasée) | ajouter `/var/run/mimic/ca/ca.crt` à son magasin, ou retirer l'hôte de `tls-intercept` |
| HTTPS d'une dépendance absent de la découverte | | hôte non intercepté : tunnel opaque | annotation `mimic.platform.io/tls-intercept` |
| HTTPS coupé en mock | `mimic_passthrough_connections_total{kind="tls",result="refused"}`, log `connexion TLS sortante refusée en mode mock` avec `destination` ; `403 mimic: tunnel refusé en mode mock (TLS non intercepté)` en proxy explicite | hôte non intercepté | intercepter et ré-enregistrer, ou exclure l'hôte |
| base de données, broker ou cache coupés en mock | `mimic_passthrough_connections_total{kind="tcp",result="refused"}`, log `connexion TCP sortante refusée en mode mock` (ou `connexion sortante muette refusée`, après 30 s) avec `destination` | `mock.tcp: refuse`, le défaut : le TCP non HTTP n'est pas simulé | si la dépendance doit rester réelle : `mimic.platform.io/exclude-outbound-ports` (ou `-cidrs`) ; `mock.tcp: relay` relaie tout le TCP du workload |
| agent local (OTel, Dapr, `cloud-sql-proxy`) en `599` ou coupé en mock | `127.0.0.1:<port>` dans la découverte ou dans `mimic_mock_responses_total` | pod démarré avec un `mimic-init` antérieur (règles posées au démarrage, qui redirigeaient `127.0.0.1`), ou `MIMIC_INTERCEPT_LOOPBACK=true` | redémarrer le pod avec l'image à jour ; retirer `MIMIC_INTERCEPT_LOOPBACK` |
| | log `certificat forgé impossible` | nom d'hôte que la CA mimic ne peut pas signer | retirer l'hôte de l'interception |
| dépendance à CA d'entreprise en erreur | `502 mimic: amont injoignable` | le sidecar ne fait confiance qu'aux racines système | `tls.upstream_ca_file` |

## Lien sidecar → contrôleur

Les erreurs apparaissent dans les logs du sidecar (`battement vers le contrôleur échoué`,
`synchronisation du partage échouée`) et dans `controller.last_error` / `share.last_error`
de `/api/status`, sous la forme `<méthode> <chemin>: <code> <motif> <body>`.

| Erreur | Cause | Correction |
|--------|-------|------------|
| `POST /heartbeat: 401 Unauthorized jeton absent ou invalide` | volume `mimic-token` absent, audience différente de `mimic-controller` (`token_audience` du webhook) | vérifier le volume projeté du pod |
| `POST /heartbeat: 403 Forbidden jeton valide pour un autre pod ou workload` | `MIMIC_WORKLOAD` (figé à l'admission) différent du workload que le contrôleur déduit du pod ; pod sans nom de workload | annotation `mimic.platform.io/workload` cohérente, recréer le pod |
| `POST /heartbeat: 503 Service Unavailable authentification indisponible` | TokenReview ou lecture du pod en échec côté contrôleur | logs du contrôleur (`authentification impossible`), RBAC |
| `… 503 Service Unavailable réplique passive du contrôleur : la réplique active sert les sidecars` | le Service route vers la passive | transitoire pendant une bascule ; sinon vérifier le label `mimic.platform.io/leader` des pods |
| `POST /segments?…: 503 Service Unavailable support indisponible` | support plein ou injoignable | PVC, droits S3 |
| `POST /heartbeat: délai dépassé` | NetworkPolicy, Service sans endpoint, contrôleur arrêté | `kubectl -n mimic-system get endpoints mimic-controller` ; le pod porte-t-il `mimic.platform.io/sidecar=true` ? |
| `… invalid peer certificate: BadSignature` | CA réémise sans réémission du certificat du contrôleur, ou sidecar démarré avec l'ancienne CA | procédure de [rotation de la CA](Exploiter-en-production.md#certificats) |
| `… invalid peer certificate: …` mentionnant l'expiration | certificat renouvelé par cert-manager mais contrôleur pas redémarré (il ne recharge pas son certificat) | `kubectl -n mimic-system rollout restart deploy/mimic-controller` |
| `jeton /var/run/secrets/mimic/token: …` | fichier de jeton illisible | volume `mimic-token` |
| `snapshot <id>: …` | téléchargement du snapshot en échec ; le pod est passé en mock sur ses données locales et retente | voir la cause qui suit |
| `forme de mock du snapshot <id> altérée (empreinte …, ETag …)` | contenu reçu différent de son empreinte (proxy intermédiaire, corruption) | retenté au battement suivant |
| `mode souhaité inconnu: X`, `capture de l'entrant inconnue: X`, `réglage WebSocket inconnu: X` | contrôleur plus récent que le sidecar | mettre à jour les sidecars |

## Mock

| Symptôme | Diagnostic | Cause probable |
|----------|------------|----------------|
| `599` avec `x-mimic-unmatched: true` | **quelle requête** : `mimicctl report <wl> -n <ns>` (section « Requêtes sans réponse du mock »), `/ui/<ns>/<wl>`, ou `GET /api/unmatched` sur l'admin du pod (`kubectl port-forward pod/<pod> 15100`) ; log `warn` `mock : requête sans réponse enregistrée` du sidecar (`method`, `authority`, `path`, `trace_id`, une ligne par seconde au plus) ; volume : `mimic_mock_responses_total{match="unmatched"}` par authority | opération (authority `hôte:port`, méthode, forme du chemin) jamais enregistrée, ou enregistrée sans body rejouable (tronqué, JSON illisible) ; authority différente (port, nom) : comparer le `path` et l'`authority` relevés avec `mimicctl report` (opérations découvertes) |
| réponses incohérentes avec la requête | header `x-mimic-match` (`template`, `fuzzy` et score) | body ou query différents : `mock.match_headers`, stratégie `best` vs `round-robin` |
| `413 mimic: body trop gros pour le mode mock` | `mimic_mock_responses_total{match="too_large"}` ; `reason: too_large` dans `/api/unmatched` et `mimicctl report` ; log `warn` `mock : body de requête trop gros` | requête au-delà de `capture.max_body_bytes` |
| `503` avec `x-mimic-websocket: refused` | | `mock.websocket: refuse` (ou `spec.mock.websocket`) |
| latences rejouées plus courtes ou plus longues | `mimic_mock_injected_latency_seconds` vs `mimic_request_duration_seconds{mode="record"}` | `mock.latency_factor` ; distribution tirée de l'enregistrement |
| tous les pods ne servent pas la même chose | `mimic_mock_snapshot_exchanges` par pod, `mimicctl status` | pod sans snapshot (téléchargement en échec) ; sidecar sans contrôleur |
| `Aligned=False` | raison de la condition | voir [Basculer en mock](Basculer-en-mock.md#vérifier-lalignement) |
| `mimicctl mode` refusé en production | `le namespace X est labellisé env=production : …` | voulu |

## Contrôleur

| Symptôme | Message | Cause | Correction |
|----------|---------|-------|------------|
| sort en code 2 | `configuration … invalide: …`, `store non supporté: …`, `auth kubernetes exige tls : les sidecars n'acceptent le contrôleur qu'en HTTPS`, `leader_election exige auth kubernetes (bail Kubernetes)`, `leader_election : lease et namespace requis`, `… doit être positif` | ConfigMap `mimic-controller` | corriger, `rollout restart` |
| sort en code 1 en marche | `bail non renouvelé à temps : la réplique cesse de servir`, `bail tenu par une autre réplique : la réplique cesse de servir` | API server lente, réseau, horloge | normal si isolé : la passive a pris le relais ; récurrent : API server |
| répétition de `renouvellement du bail` / `lecture du bail` | | droits sur `leases/mimic-controller`, API server | RBAC (Role `mimic-controller` dans `mimic-system`) |
| prise de relais lente | `prise de relais` (liste des namespaces en échec, retentée), `liste initiale des MimicWorkload non reçue : l'état persisté sert` | API server, droits `namespaces` / `mimicworkloads` | `mimic_controller_takeover_seconds` |
| passive en `ContainerCreating` | événement `Multi-Attach error` | PVC `ReadWriteOnce` sur deux nœuds | [stockage du contrôleur](Exploiter-en-production.md#stockage-du-contrôleur) |
| statut des `MimicWorkload` figé | `statut non publié` | droit `patch` sur `mimicworkloads/status`, CRD absente | RBAC, `kubectl apply -f deploy/controller/crd.yaml` |
| surveillance en échec | `surveillance des MimicWorkload`, `surveillance des namespaces` | CRD absente, RBAC | idem |
| snapshot non construit | `construction du snapshot échouée`, raison `SnapshotFailed` | support | espace, droits |
| état souhaité non persisté | `écriture de l'état souhaité échouée` | support | |
| segments qui s'accumulent | `écriture des exemples : aucun segment supprimé`, `compaction : …` | écriture du support en échec : aucun segment n'est supprimé tant que les exemples ne sont pas écrits | espace, droits ; puis surveiller le volume |
| `marque de la réplique active non retirée`, `bail non rendu : il expirera` | à l'arrêt | API server injoignable pendant l'arrêt | la bascule attend l'expiration du bail (15 s) |

## `mimicctl`

| Message | Cause |
|---------|-------|
| `mimicctl: kubeconfig: …` | pas de kubeconfig valide |
| `mimicctl: aucun pod du contrôleur en marche dans mimic-system` | contrôleur arrêté, ou droit `list` sur les pods de `mimic-system` manquant |
| `mimicctl: contrôleur : 503 Service Unavailable réplique passive du contrôleur : …` | bascule de réplique : relancer |
| `mimicctl: --inbound ou --outbound <authority> requis` | `export` sans sens |
| `mimicctl: mise à jour de ns/wl: …` | droits sur `mimicworkloads`, conflit de server-side apply avec un autre gestionnaire |

Autres messages : [CLI mimicctl](CLI-mimicctl.md).

## Service mesh

| Symptôme | Cause | Correction |
|----------|-------|------------|
| `mimic-init: mesh None` alors que le pod est dans Istio ou Linkerd | règles du mesh posées après `mimic-init` (ordre des init containers), ou namespace mis dans le mesh après le démarrage du pod | `kubectl rollout restart` ; vérifier l'ordre : `kubectl get pod <pod> -o jsonpath='{.spec.initContainers[*].name}'` (les conteneurs du mesh d'abord) |
| sondes du sidecar refusées en mTLS strict | pod injecté sans conteneur du mesh visible à l'admission | `mimic.platform.io/mesh` explicite, ou recréer le pod (la réinvocation pose des sondes `exec`) |
| ambient : entrant non enregistré | `POD_IP` absent (pod modifié), namespace passé en ambient après le démarrage | recréer le pod ; annotation `mimic.platform.io/mesh: istio-ambient` |
| scraper sans métriques de mimic en mTLS strict | le mesh refuse la collecte en clair sur 15100 | scraper dans le mesh, ou port exclu côté mesh |
| boucle ou trafic qui contourne le mesh | autre mesh (Consul, Kuma, Cilium) | non pris en charge |
