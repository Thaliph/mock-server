# Alertes et supervision

[← Sommaire](README.md)

Comment savoir si mimic va bien, et quoi faire quand il ne va pas. Les règles ci-dessous
n'utilisent que les métriques de mimic (voir [Métriques](Metriques.md)) et des métriques
standard de kube-state-metrics, de cAdvisor et du kubelet.

## Sondes de santé

| Composant | Sonde | Ce qu'elle vérifie | Ce qu'elle ne vérifie pas |
|-----------|-------|--------------------|---------------------------|
| sidecar | `startupProbe` `GET /readyz` (période 1 s, `share.startup.timeout` + 30 échecs : 60 par défaut, 330 avec une configuration montée par ConfigMap) ; `readinessProbe` `GET /readyz` et `livenessProbe` `GET /healthz` (période 10 s, 3 échecs), sur `mimic-admin` | `/healthz` : l'API d'admin répond. `/readyz` : la barrière de démarrage est ouverte (`503 waiting` tant que le sidecar piloté n'a pas appliqué l'état souhaité, snapshot compris, ou atteint son délai) ; le kubelet ne démarre l'application qu'après le succès de la `startupProbe` | après le démarrage, le lien avec le contrôleur (`/readyz` reste `200` pendant une panne du contrôleur, pour ne pas vider les Services), le partage, le moteur de mock |
| sidecar dans un mesh | mêmes sondes en `exec` : `mimic-proxy --probe /readyz` (ou `/healthz`) | la même chose, depuis le conteneur : en mTLS strict, le mesh refuserait une sonde HTTP du kubelet | idem |
| contrôleur | `readinessProbe` et `livenessProbe` `GET /healthz` en HTTPS sur `api` (8443) | le serveur répond ; **la passive répond aussi** (`/healthz` n'est pas soumis à l'élection) | quelle réplique est active, l'état du support |
| webhook | `readinessProbe` et `livenessProbe` `GET /healthz` en HTTPS sur `webhook` (8443) | le serveur répond | la validité du `caBundle` côté API server |

La sonde `exec` passe par l'adresse de `admin.listen` (la boucle locale si elle écoute sur
`0.0.0.0`) avec 500 ms par étape ; en échec, elle affiche
`mimic: sonde <chemin> : connexion à <adresse> : <erreur>` ou `mimic: sonde <chemin> : statut <code>`
et sort en 1. Le webhook ne pose des sondes `exec` que si des conteneurs Istio ou Linkerd sont
dans le pod ; en Istio ambient, les sondes restent HTTP.

## Panne franche ou dégradation : quoi regarder

### Sidecar

| Situation | Effet | Signal |
|-----------|-------|--------|
| conteneur `mimic` arrêté ou en redémarrage | les règles iptables restent : le trafic entrant et sortant redirigé du pod n'a plus d'écoute et **échoue** jusqu'au redémarrage | `kube_pod_init_container_status_restarts_total{container="mimic"}` (native sidecar : compté comme init container), `kube_pod_init_container_status_ready{container="mimic"} == 0`, pod non `Ready` |
| store sur disque proche de la taille de son `emptyDir` (512 Mi) | au-delà, le kubelet **évince le pod** | `mimic_store_disk_bytes` rapporté à `mimic_store_disk_limit_bytes` ; puis `kube_pod_status_reason{reason="Evicted"}` |
| mémoire proche de la limite (256 Mi par défaut) | risque d'OOMKill, donc de coupure | `container_memory_working_set_bytes{container="mimic"}` rapporté à `kube_pod_init_container_resource_limits{container="mimic",resource="memory"}` |
| contrôleur injoignable | le pod garde son dernier état, les échanges s'accumulent dans la file de publication puis sont perdus | `mimic_heartbeats_total{result="error"}`, `time() - mimic_heartbeat_last_success_timestamp_seconds` ; `mimic_share_sync_total{result="error"}` (seulement s'il y a quelque chose à publier), puis `mimic_capture_dropped_total{reason="share_backlog"}` ; logs `battement vers le contrôleur échoué` (au plus un toutes les 30 s, avec `omitted`), `synchronisation du partage échouée` ; `controller.last_error` de `/api/status` |
| contrôleur injoignable au démarrage d'un pod neuf | l'application attend jusqu'à `share.startup.timeout` (30 s), puis démarre dans le repli (`record` par défaut) ; avec `fail`, le pod reste en `Init` | `mimic_startup{state="waiting"}` puis `{state="fallback"}` ; log `contrôleur injoignable au démarrage : repli share.startup.fallback appliqué` ; événements `Startup probe failed` du sidecar |
| application injoignable par le sidecar | `502 mimic: amont injoignable` aux clients | `mimic_upstream_errors_total{direction="inbound"}` |
| mock incomplet | `599` à l'application | `mimic_mock_responses_total{match="unmatched"}` |
| mock sans snapshot | le pod sert ses seules données locales | `mimic_mode{mode="mock"} == 1` et `mimic_mock_snapshot_exchanges == 0` |
| HTTPS coupé en mock | connexions fermées par mimic | `mimic_passthrough_connections_total{kind="tls",result="refused"}` |
| TCP non HTTP coupé en mock (base, broker, cache) | connexions fermées par mimic (`mock.tcp: refuse`) | `mimic_passthrough_connections_total{kind="tcp",result="refused"}` ; log `… refusée en mode mock` avec `destination` |
| interception TLS refusée par l'application | appels HTTPS de la dépendance en échec | `mimic_tls_handshake_failures_total` |

### Contrôleur et élection

Deux réplicas, une seule active (bail `mimic-system/mimic-controller`, `lease_duration` 15 s,
`renew_deadline` 10 s, `retry_period` 2 s). L'active porte le label
`mimic.platform.io/leader=true` que le Service sélectionne.

| Situation | Effet | Signal |
|-----------|-------|--------|
| active arrêtée proprement (rollout, drain) | le bail est rendu (`bail rendu`), la passive le prend en quelques secondes | redémarrage attendu ; `mimic_controller_takeover_seconds` |
| active tuée ou isolée | la passive attend l'expiration du bail (jusqu'à ~17 s), puis prend le relais | log `bail pris : prise de relais` puis `réplique active` sur la nouvelle |
| active qui ne renouvelle plus à temps | elle cesse de servir et **sort en code 1** (`bail non renouvelé à temps : la réplique cesse de servir`) ; Kubernetes la relance en passive | `kube_pod_container_status_restarts_total{namespace="mimic-system",container="controller"}` |
| aucune réplique prête | sidecars sans bascule ni publication ; le trafic des pods continue dans le dernier état | `kube_deployment_status_replicas_available{deployment="mimic-controller"} == 0` |
| support plein ou injoignable | segments refusés (`503 support indisponible`), sidecars en reprise exponentielle | `mimic_controller_segments_total{result="error"}`, log `ingestion d'un segment` ; `kubelet_volume_stats_used_bytes` du PVC |
| certificat servi proche de l'expiration (renouvellement cert-manager en échec, ou nouveau certificat refusé) | une fois expiré, les sidecars refusent la connexion au contrôleur (`invalid peer certificate`) et l'API server celle du webhook | `mimic_controller_certificate_expiry_timestamp_seconds`, `mimic_injector_certificate_expiry_timestamp_seconds` ; log `nouveau certificat illisible : l'ancien reste servi` |
| TokenReview ou API indisponible | `503 authentification indisponible` aux sidecars | `mimic_controller_auth_failures_total{reason="unavailable"}`, log `authentification impossible` |
| pods qui ne s'alignent pas | bascule incomplète | `mimic_controller_pods{state="diverging"}`, condition `Aligned=False` |

Une passive répond `503` (`Retry-After: 2`, body
`réplique passive du contrôleur : la réplique active sert les sidecars`) à tout sauf
`/healthz` et `/metrics` : si des sidecars reçoivent ce `503`, c'est que le Service (ou une
EndpointSlice périmée) les envoie à la passive, ce qui ne dure normalement que le temps d'une
bascule.

Rappel : `/metrics` du contrôleur est servi sur le port `api` (8443, HTTPS, sans jeton) et la
NetworkPolicy livrée l'ouvre aux namespaces labellisés `mimic.platform.io/monitoring: "true"`
(voir [Métriques](Metriques.md#où-les-collecter)). Les règles « contrôleur (métriques mimic) »
ci-dessous marchent donc avec les manifests du dépôt, dès que le namespace du collecteur porte
le label et qu'un `VMPodScrape` ou `PodMonitor` vise le port `api`
(`deploy/example/vmpodscrape-controller.yaml`). Sans ce collecteur, s'en tenir aux règles
kube-state-metrics et aux signaux des sidecars.

### Webhook

| Situation | Effet | Signal |
|-----------|-------|--------|
| webhook indisponible | `failurePolicy: Ignore` : les pods sont **créés sans mimic**, sans erreur visible pour l'équipe applicative | `kube_deployment_status_replicas_available{deployment="mimic-injector"} == 0` ; pods sans l'annotation `mimic.platform.io/status: injected` dans un namespace activé |
| pod ignoré volontairement | avertissement d'admission `mimic non injecté : <raison>` | `mimic_injector_admissions_total{result="skipped"}` (informatif) |
| revue invalide | pod créé sans mimic | `mimic_injector_admissions_total{result="error"}`, log `revue d'admission invalide` |

Pods d'un namespace activé créés sans mimic :

```sh
for ns in $(kubectl get ns -l mimic.platform.io/enabled=true -o name | cut -d/ -f2); do
  kubectl -n "$ns" get pods -o json | jq -r --arg ns "$ns" \
    '.items[] | select(.metadata.annotations["mimic.platform.io/status"] != "injected") | "\($ns)/\(.metadata.name)"'
done
```

## Règles `PrometheusRule`

Les règles sont livrées dans `deploy/monitoring/`, testées en CI par `promtool`
(`scripts/check-monitoring.sh`) :

| Fichier | Contenu |
|---------|---------|
| `deploy/monitoring/prometheus-rule.yaml` | la `PrometheusRule` `mimic-system/mimic`, à appliquer telle quelle (`kubectl apply -f`) ; VictoriaMetrics operator la convertit en `VMRule` |
| `deploy/monitoring/rules.yaml` | les mêmes groupes au format Prometheus brut, pour vmalert ou un Prometheus sans opérateur |
| `deploy/monitoring/tests.yaml` | tests unitaires `promtool test rules` des expressions à piège |

Trois groupes : `mimic-sidecar` (métriques des sidecars et de kube-state-metrics sur les pods
injectés), `mimic-controller-kube` (kube-state-metrics, cAdvisor et kubelet sur
`mimic-system`, sans collecte de mimic) et `mimic-controller-metrics` (`/metrics` du
contrôleur et du webhook, port `api`, 8443). Les seuils sont des valeurs de départ, à ajuster à
votre trafic : modifier `rules.yaml` et `prometheus-rule.yaml` ensemble (un test vérifie qu'ils
sont identiques). Les labels `namespace` et `pod` des métriques des sidecars sont ceux posés par
le collecteur.

Notes sur ces règles :

- `MimicControllerUnreachableFromSidecars` ne voit que les pods qui ont quelque chose à
  publier : un cycle sans nouvel échange compte `ok` sans appeler le contrôleur. Un pod en
  mock avec `capture.inbound: off` ne publie rien et ne déclenche donc jamais cette alerte.
  Les battements en échec se suivent par `MimicHeartbeatsFailing`, qui voit tous les pods
  pilotés, qu'ils publient ou non.
- `MimicStartedWithoutDesiredState` reste active tant que le pod n'a pas eu de réponse du
  contrôleur : dès la première, `mimic_startup` passe à `aligned` (ou `last-known` si le
  snapshot manque encore).
- `MimicStartupBlocked` ne concerne que le repli `fail` : avec les autres, la barrière s'ouvre
  au plus tard à `share.startup.timeout`. Le sidecar sert `/metrics` pendant que le pod est
  en `Init`, mais seul un scraper qui collecte aussi les pods `Pending` le voit ; à défaut,
  `MimicSidecarNotReady` (`kube_pod_init_container_status_ready{container="mimic"} == 0`)
  couvre ce cas.
- `MimicMockUnmatched` ne vaut qu'en `mock` : en `mock-partial`, `match="unmatched"` compte
  les requêtes **relayées faute d'enregistrement**, ce qui est le fonctionnement voulu du mode
  et ferait sonner l'alerte en permanence. Le `and on (namespace, pod) mimic_mode{mode="mock"}`
  l'écarte. Pour suivre quand même la couverture du snapshot en `mock-partial`, regarder la
  part de `match="unmatched"` comme un indicateur, sans alerte.
- `MimicMockWithoutSnapshot` se déclenche aussi pour un sidecar sans contrôleur basculé en
  mock par son API d'admin (cas voulu) : filtrer par namespace si vous utilisez ce mode.
- `MimicStoreDiskHigh` ne vaut que pour les sidecars injectés par le webhook, qui leur donne
  la taille de leur `emptyDir` (`mimic_store_disk_limit_bytes`) ; sans elle, la limite vaut 0
  et l'alerte ne rend rien. Avec la configuration par défaut, le journal (128 MiB) et les
  formes de mock gardées tiennent dans les 512 Mi : l'alerte signale une configuration qui
  agrandit `store.max_segments`, `store.segment_bytes` ou `store.max_cached_snapshots`.
- `MimicPodEvicted` sonne pour toute éviction d'un pod injecté, que mimic en soit la cause ou
  non : le message de l'éviction le dit (`ephemeral local storage usage exceeds the total limit
  of containers` ou `Usage of EmptyDir volume "mimic-store" exceeds the limit` si c'est le
  store).
- `MimicVersionSkew` est normale pendant une mise à jour : un sidecar garde sa version jusqu'au
  redémarrage de son pod, d'où les 24 h. Elle compte les valeurs de `mimic_version`, pas celles
  du label `version`, qui est la version de l'application.
- Les alertes d'expiration de certificat ignorent la valeur 0 (serveur sans TLS) ; elles
  sonnent à 7 jours de l'expiration, alors que cert-manager renouvelle à 15 jours.
- `mimic_controller_pods{state="absent"}` n'est pas une bonne alerte : un pod supprimé reste
  `absent` jusqu'à `pod_forget` (10 min), ce qui arrive à chaque déploiement.
- `MimicSidecarNotReady` peut se déclencher pendant l'arrêt d'un pod ; le `for: 5m` absorbe
  les cas normaux.

## Conduite à tenir

| Alerte | Diagnostic | Action |
|--------|------------|--------|
| `MimicSidecarRestarting` | `kubectl -n <ns> describe pod <pod>` (raison `OOMKilled` ?), `kubectl -n <ns> logs <pod> -c mimic --previous` (code 2 : configuration invalide, message `mimic: …` ; code 1 : écoute impossible) | OOM : monter `resources` du webhook ou baisser `capture.max_store_bytes` ; configuration : corriger la ConfigMap `mimic.platform.io/config` ou `sidecar_config`. En urgence, `mimic.platform.io/inject: "false"` sur le template et redéploiement |
| `MimicSidecarNotReady` | `kubectl -n <ns> logs <pod> -c mimic`, état de `mimic-init` (`kubectl -n <ns> logs <pod> -c mimic-init`) | voir [Dépannage](Depannage.md) |
| `MimicSidecarMemoryHigh` | `mimic_store_bytes`, `mimic_store_exchanges` ; nombre d'authorities | le store est borné par `capture.max_store_bytes` (64 MiB) ; le reste vient des requêtes en vol et des bodies (1 MiB chacun au plus). Monter la limite ou baisser `max_store_bytes` / `max_body_bytes` |
| `MimicControllerUnreachableFromSidecars` | logs du sidecar (`synchronisation du partage échouée`, `battement vers le contrôleur échoué` avec le code et le motif) ; `curl http://127.0.0.1:15100/api/status` par port-forward | `401` : jeton (volume `mimic-token`, audience) ; `403` : pod d'un autre workload ; `503` : passive ou support ; `délai dépassé` : NetworkPolicy (label `mimic.platform.io/sidecar=true`), Service sans endpoint ; `invalid peer certificate` : CA (voir [Dépannage](Depannage.md)) |
| `MimicHeartbeatsFailing` | comme `MimicControllerUnreachableFromSidecars` ; `controller.last_error` de `/api/status` | mêmes actions |
| `MimicStartedWithoutDesiredState` | logs du sidecar au démarrage (`contrôleur injoignable au démarrage`), puis comme `MimicHeartbeatsFailing` | rétablir le contrôleur : le pod s'aligne au battement suivant. Si le workload est en mock, ce pod a pu joindre les vraies dépendances : le recréer une fois le contrôleur revenu (`kubectl delete pod`), ou choisir `share.startup.fallback: mock-local` ou `fail` pour ce workload |
| `MimicStartupBlocked` | `kubectl -n <ns> describe pod <pod>` (`Startup probe failed`), logs du sidecar | rétablir le contrôleur : l'application démarre dès sa première réponse (le kubelet relance le sidecar à chaque fin de fenêtre, avec une attente croissante) |
| `MimicShareBacklogLoss` | durée de l'indisponibilité du contrôleur | rétablir le contrôleur ; si la perte est structurelle (débit très élevé), monter `share.max_backlog_bytes` ou passer l'entrant en `metadata` |
| `MimicMockUnmatched` | `mimicctl report` (section « Requêtes sans réponse du mock ») ou `/ui/<ns>/<wl>` : quelles opérations répondent `599` ; logs `warn` du sidecar `mock : requête sans réponse enregistrée` (avec `trace_id`) | enregistrer plus longtemps (opérations rares), vérifier `tls.intercept`, utiliser `mock-partial` pour compléter, ou `mock.unmatched: passthrough` |
| `MimicMockWithoutSnapshot` | logs du sidecar : `battement vers le contrôleur échoué` avec `snapshot <id>: …` ; `forme de mock du snapshot … altérée` | rétablir l'accès au contrôleur ; le téléchargement est retenté à chaque battement |
| `MimicMockTlsRefused` | `mimic_passthrough_connections_total` par pod, logs de l'application | ajouter l'hôte à `mimic.platform.io/tls-intercept`, ré-enregistrer, ou l'exclure (`exclude-outbound-cidrs`) s'il doit rester réel |
| `MimicMockTcpRefused` | log du sidecar `connexion TCP sortante refusée en mode mock` : `destination` (adresse et port) | attendu si la dépendance doit être coupée pendant le tir ; sinon l'exclure (`exclude-outbound-ports` ou `-cidrs`), elle recevra alors la charge |
| `MimicTlsInterceptRejected` | log `poignée de main TLS refusée par l'application` avec l'authority | l'application ne lit pas `SSL_CERT_FILE` & co : ajouter `/var/run/mimic/ca/ca.crt` à son magasin (JVM : `keytool`), ou retirer l'hôte de l'interception |
| `MimicStoreDiskHigh` | `kubectl -n <ns> exec <pod> -c mimic -- du -a /var/lib/mimic` ; configuration du store (`store.segment_bytes` × `store.max_segments`, `store.max_cached_snapshots`) | revenir aux bornes par défaut, ou agrandir l'`emptyDir` ; un pod évincé repart d'un store vide (le contrôleur garde ce qui a été publié) |
| `MimicPodEvicted` | `kubectl -n <ns> describe pod <pod>` : message de l'éviction (volume `mimic-store`, mémoire, disque du nœud) | store : comme `MimicStoreDiskHigh` ; nœud saturé : hors de mimic |
| `MimicVersionSkew` | `count by (mimic_version) (mimic_build_info)`, puis les pods de l'ancienne version | `kubectl rollout restart` des workloads concernés pour qu'ils reçoivent le sidecar à jour |
| `MimicInboundUpstreamErrors` | port d'écoute de l'application, adresse (le sidecar ne joint que l'IP du pod, jamais `127.0.0.1`) | application pas encore prête (démarrage lent), ou qui n'écoute que sur `127.0.0.1` : la faire écouter sur l'IP du pod ou `0.0.0.0`, ou ne pas intercepter ce port (`mimic.platform.io/inbound-ports`) |
| `MimicControllerDown` | `kubectl -n mimic-system get pods -l app.kubernetes.io/name=mimic-controller -L mimic.platform.io/leader`, `describe`, logs | PVC non attaché (multi-attach `ReadWriteOnce`), certificat absent, configuration invalide (code 2) ; voir [Exploiter en production](Exploiter-en-production.md) |
| `MimicControllerNotRedundant` | réplique `Pending` ou `ContainerCreating` | PVC `ReadWriteMany` sans classe partagée (`Pending`), ou PVC `ReadWriteOnce` sans `controller-rwo` (`Multi-Attach`) : `ReadWriteMany` ou S3 |
| `MimicControllerRestarting` | `kubectl logs --previous` : `bail non renouvelé à temps`, `bail tenu par une autre réplique`, OOM | API server lente ou réseau (bail) ; mémoire : `max_exchanges_total` ou limite |
| `MimicControllerMemoryHigh` | `mimic_controller_exchanges`, `mimic_controller_exemplars_bytes` | baisser `max_exchanges_total` (environ 2 Ko par échange) ou monter la limite (4 Gi par défaut) |
| `MimicControllerStorageFull` | taille par workload sous `/var/lib/mimic-controller` ; snapshots accumulés | les snapshots au-delà de `snapshot.keep_last` sont purgés à chaque maintenance (jamais les référencés ni les épinglés) : vérifier que la purge réussit (`suppression d'un segment`, [runbook du contrôleur](../runbook/controller.md#3-storage)), épingler moins, baisser `snapshot.keep_last` ou `max_bytes_per_workload`, agrandir le PVC |
| `MimicInjectorDown` | `kubectl -n mimic-system get pods -l app.kubernetes.io/name=mimic-injector`, logs | rétablir, puis `kubectl rollout restart` des workloads créés pendant la panne pour les réinjecter |
| `MimicControllerNoLeader` | logs : `lecture du bail`, `renouvellement du bail` ; `kubectl -n mimic-system get lease mimic-controller -o yaml` | droits RBAC sur `leases`, API server ; la prise de relais attend aussi la liste des namespaces (`prise de relais`) |
| `MimicControllerTwoLeaders` | labels `mimic.platform.io/leader` des deux pods | doit se résorber seul (la réplique qui perd le bail sort) ; sinon redémarrer l'ancienne active |
| `MimicControllerSegmentErrors` | log `ingestion d'un segment` | support plein ou en lecture seule, droits S3 |
| `MimicControllerSlowTakeover` | `mimic_controller_takeover_seconds` ; logs de la nouvelle active (`prise de relais`) | le rechargement de l'état persisté est long : support lent (S3 distant, PVC saturé) ou beaucoup d'exemples. Informatif ; si c'est régulier, réduire `max_exemplars_per_workload` ou rapprocher le support |
| `MimicControllerAuthUnavailable` | log `authentification impossible` | API server, droits `tokenreviews` `create` et `pods` `get` du ClusterRole `mimic-controller` |
| `MimicControllerAuthRejected` | `reason` : `missing`/`invalid` (jeton, audience `mimic-controller`), `forbidden` (pod sans nom de workload, workload d'un autre) | vérifier le volume `mimic-token` et l'annotation `mimic.platform.io/workload` |
| `MimicPodsDiverging` | le label `workload` (`<namespace>/<nom>`), puis `/ui/<ns>/<wl>` : état de chaque pod ; logs des pods concernés | voir [Basculer en mock](Basculer-en-mock.md#vérifier-lalignement) |
| `MimicExemplarsRefused` | `reason` | `count` : monter `max_exemplars_per_workload` ; `bytes` : monter `max_exemplar_bytes_per_workload` / `_total` |
| `MimicControllerCertificateExpiring` | `kubectl -n mimic-system get certificate mimic-controller-tls` (condition `Ready`, `renewalTime`), logs de cert-manager ; log du contrôleur `nouveau certificat illisible : l'ancien reste servi` | réparer l'émetteur `mimic-ca` ou le Secret ; un nouveau certificat est relu à chaud, sans redémarrage |
| `MimicInjectorCertificateExpiring` | `kubectl -n mimic-system get certificate mimic-injector-tls`, logs de cert-manager et du webhook | idem ; une fois expiré, l'API server ne joint plus le webhook et les pods sont créés sans mimic (`failurePolicy: Ignore`) |
| `MimicInjectorErrors` | log `revue d'admission invalide` | version d'API server, autre webhook qui modifie la revue |
