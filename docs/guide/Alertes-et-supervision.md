# Alertes et supervision

[← Sommaire](README.md)

Comment savoir si mimic va bien, et quoi faire quand il ne va pas. Les règles ci-dessous
n'utilisent que les métriques de mimic (voir [Métriques](Metriques.md)) et des métriques
standard de kube-state-metrics, de cAdvisor et du kubelet.

## Sondes de santé

| Composant | Sonde | Ce qu'elle vérifie | Ce qu'elle ne vérifie pas |
|-----------|-------|--------------------|---------------------------|
| sidecar | `startupProbe` `GET /healthz` (période 1 s, 30 échecs) ; `readinessProbe` `GET /readyz` et `livenessProbe` `GET /healthz` (période 10 s, 3 échecs), sur `mimic-admin` | l'API d'admin répond (`ok`) ; les deux routes sont identiques | le lien avec le contrôleur, le partage, le moteur de mock |
| sidecar dans un mesh | mêmes sondes en `exec` : `mimic-proxy --probe /healthz` (ou `/readyz`) | la même chose, depuis le conteneur : en mTLS strict, le mesh refuserait une sonde HTTP du kubelet | idem |
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
| mémoire proche de la limite (256 Mi par défaut) | risque d'OOMKill, donc de coupure | `container_memory_working_set_bytes{container="mimic"}` rapporté à `kube_pod_init_container_resource_limits{container="mimic",resource="memory"}` |
| contrôleur injoignable | le pod garde son dernier état, les échanges s'accumulent dans la file de publication puis sont perdus | `mimic_share_sync_total{result="error"}` (seulement s'il y a quelque chose à publier), puis `mimic_capture_dropped_total{reason="share_backlog"}` ; logs `battement vers le contrôleur échoué`, `synchronisation du partage échouée` ; `controller.last_error` de `/api/status` |
| application injoignable par le sidecar | `502 mimic: amont injoignable` aux clients | `mimic_upstream_errors_total{direction="inbound"}` |
| mock incomplet | `599` à l'application | `mimic_mock_responses_total{match="unmatched"}` |
| mock sans snapshot | le pod sert ses seules données locales | `mimic_mode{mode="mock"} == 1` et `mimic_mock_snapshot_exchanges == 0` |
| HTTPS coupé en mock | connexions fermées par mimic | `mimic_passthrough_connections_total{kind="tls",result="refused"}` |
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
| TokenReview ou API indisponible | `503 authentification indisponible` aux sidecars | `mimic_controller_auth_failures_total{reason="unavailable"}`, log `authentification impossible` |
| pods qui ne s'alignent pas | bascule incomplète | `mimic_controller_pods{state="diverging"}`, condition `Aligned=False` |

Une passive répond `503` (`Retry-After: 2`, body
`réplique passive du contrôleur : la réplique active sert les sidecars`) à tout sauf
`/healthz` et `/metrics` : si des sidecars reçoivent ce `503`, c'est que le Service (ou une
EndpointSlice périmée) les envoie à la passive, ce qui ne dure normalement que le temps d'une
bascule.

Rappel : avec les manifests livrés, `/metrics` du contrôleur n'est joignable que par
port-forward (voir [Métriques](Metriques.md#où-les-collecter)). Les règles « contrôleur
(métriques mimic) » ci-dessous ne servent que si vous l'avez rendu collectable ; sinon,
utilisez les règles kube-state-metrics et les signaux des sidecars.

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

Seuils de départ, à ajuster à votre trafic. Le label `namespace` et `pod` des métriques des
sidecars est celui posé par le scraper.

```yaml
apiVersion: monitoring.coreos.com/v1
kind: PrometheusRule
metadata:
  name: mimic
  namespace: mimic-system
spec:
  groups:
    - name: mimic-sidecar
      rules:
        - alert: MimicSidecarRestarting
          expr: increase(kube_pod_init_container_status_restarts_total{container="mimic"}[15m]) > 2
          labels: {severity: critical}
          annotations:
            summary: "Sidecar mimic de {{ $labels.namespace }}/{{ $labels.pod }} redémarre en boucle"
            description: "Pendant chaque redémarrage, le trafic redirigé du pod échoue."
        - alert: MimicSidecarNotReady
          expr: kube_pod_init_container_status_ready{container="mimic"} == 0
          for: 5m
          labels: {severity: warning}
          annotations:
            summary: "Sidecar mimic non prêt sur {{ $labels.namespace }}/{{ $labels.pod }}"
        - alert: MimicSidecarMemoryHigh
          expr: |
            max by (namespace, pod) (container_memory_working_set_bytes{container="mimic"})
              / max by (namespace, pod) (kube_pod_init_container_resource_limits{container="mimic", resource="memory"})
              > 0.85
          for: 15m
          labels: {severity: warning}
          annotations:
            summary: "Sidecar mimic à plus de 85 % de sa limite mémoire ({{ $labels.namespace }}/{{ $labels.pod }})"
        - alert: MimicControllerUnreachableFromSidecars
          expr: |
            sum by (namespace) (rate(mimic_share_sync_total{result="error"}[10m]))
              / sum by (namespace) (rate(mimic_share_sync_total[10m]))
              > 0.5
          for: 10m
          labels: {severity: warning}
          annotations:
            summary: "Les sidecars de {{ $labels.namespace }} n'arrivent plus à publier au contrôleur"
        - alert: MimicShareBacklogLoss
          expr: sum by (namespace, pod) (increase(mimic_capture_dropped_total{reason="share_backlog"}[15m])) > 0
          labels: {severity: warning}
          annotations:
            summary: "Échanges perdus avant publication sur {{ $labels.namespace }}/{{ $labels.pod }}"
        - alert: MimicMockUnmatched
          expr: |
            sum by (namespace, pod) (rate(mimic_mock_responses_total{match="unmatched"}[5m]))
              / sum by (namespace, pod) (rate(mimic_mock_responses_total[5m]))
              > 0.05
          for: 10m
          labels: {severity: warning}
          annotations:
            summary: "Plus de 5 % des appels sortants sans enregistrement en mock ({{ $labels.namespace }}/{{ $labels.pod }})"
        - alert: MimicMockWithoutSnapshot
          expr: (mimic_mode{mode="mock"} == 1) and on (namespace, pod) (mimic_mock_snapshot_exchanges == 0)
          for: 5m
          labels: {severity: warning}
          annotations:
            summary: "{{ $labels.namespace }}/{{ $labels.pod }} est en mock sans snapshot du contrôleur"
        - alert: MimicMockTlsRefused
          expr: sum by (namespace, pod) (increase(mimic_passthrough_connections_total{direction="outbound", kind="tls", result="refused"}[10m])) > 0
          labels: {severity: info}
          annotations:
            summary: "Connexions HTTPS non interceptées coupées en mock ({{ $labels.namespace }}/{{ $labels.pod }})"
        - alert: MimicTlsInterceptRejected
          expr: sum by (namespace, pod, authority) (increase(mimic_tls_handshake_failures_total[10m])) > 0
          labels: {severity: warning}
          annotations:
            summary: "L'application refuse le certificat mimic pour {{ $labels.authority }} ({{ $labels.namespace }}/{{ $labels.pod }})"
        - alert: MimicInboundUpstreamErrors
          expr: sum by (namespace, pod) (rate(mimic_upstream_errors_total{direction="inbound", kind="connect"}[5m])) > 0
          for: 5m
          labels: {severity: warning}
          annotations:
            summary: "Le sidecar n'arrive pas à joindre l'application de {{ $labels.namespace }}/{{ $labels.pod }}"

    - name: mimic-controller-kube
      rules:
        - alert: MimicControllerDown
          expr: kube_deployment_status_replicas_available{namespace="mimic-system", deployment="mimic-controller"} < 1
          for: 2m
          labels: {severity: critical}
          annotations:
            summary: "Aucune réplique du contrôleur mimic disponible"
        - alert: MimicControllerNotRedundant
          expr: kube_deployment_status_replicas_available{namespace="mimic-system", deployment="mimic-controller"} < 2
          for: 30m
          labels: {severity: warning}
          annotations:
            summary: "Une seule réplique du contrôleur mimic : pas de relais possible"
        - alert: MimicControllerRestarting
          expr: increase(kube_pod_container_status_restarts_total{namespace="mimic-system", container="controller"}[30m]) > 2
          labels: {severity: warning}
          annotations:
            summary: "Le contrôleur mimic redémarre souvent (bail perdu, OOM, échec de démarrage)"
        - alert: MimicControllerMemoryHigh
          expr: |
            max by (pod) (container_memory_working_set_bytes{namespace="mimic-system", container="controller"})
              / max by (pod) (kube_pod_container_resource_limits{namespace="mimic-system", container="controller", resource="memory"})
              > 0.85
          for: 15m
          labels: {severity: warning}
          annotations:
            summary: "Contrôleur mimic à plus de 85 % de sa limite mémoire ({{ $labels.pod }})"
        - alert: MimicControllerStorageFull
          expr: |
            kubelet_volume_stats_used_bytes{namespace="mimic-system", persistentvolumeclaim="mimic-controller"}
              / kubelet_volume_stats_capacity_bytes{namespace="mimic-system", persistentvolumeclaim="mimic-controller"}
              > 0.8
          for: 15m
          labels: {severity: warning}
          annotations:
            summary: "Support du contrôleur mimic rempli à plus de 80 %"
        - alert: MimicInjectorDown
          expr: kube_deployment_status_replicas_available{namespace="mimic-system", deployment="mimic-injector"} < 1
          for: 2m
          labels: {severity: critical}
          annotations:
            summary: "Webhook mimic indisponible : les nouveaux pods sont créés sans mimic"

    # Seulement si /metrics du contrôleur et du webhook sont collectés.
    - name: mimic-controller-metrics
      rules:
        - alert: MimicControllerNoLeader
          expr: sum(mimic_controller_leader) < 1 or absent(mimic_controller_leader)
          for: 1m
          labels: {severity: critical}
          annotations:
            summary: "Aucune réplique active du contrôleur mimic"
        - alert: MimicControllerTwoLeaders
          expr: sum(mimic_controller_leader) > 1
          for: 1m
          labels: {severity: critical}
          annotations:
            summary: "Deux répliques du contrôleur mimic se disent actives"
        - alert: MimicControllerSegmentErrors
          expr: sum(rate(mimic_controller_segments_total{result="error"}[5m])) > 0
          for: 10m
          labels: {severity: warning}
          annotations:
            summary: "Le contrôleur mimic refuse ou ne range pas des segments"
        - alert: MimicControllerAuthUnavailable
          expr: sum(increase(mimic_controller_auth_failures_total{reason="unavailable"}[5m])) > 0
          for: 5m
          labels: {severity: warning}
          annotations:
            summary: "Le contrôleur mimic ne peut plus valider les jetons (TokenReview, lecture des pods)"
        - alert: MimicControllerAuthRejected
          expr: sum by (reason) (increase(mimic_controller_auth_failures_total{reason=~"missing|invalid|forbidden"}[15m])) > 10
          labels: {severity: warning}
          annotations:
            summary: "Appels de sidecars refusés par le contrôleur ({{ $labels.reason }})"
        - alert: MimicPodsDiverging
          expr: max(mimic_controller_pods{state="diverging"}) > 0
          for: 5m
          labels: {severity: warning}
          annotations:
            summary: "Des pods n'appliquent pas l'état souhaité depuis 5 minutes"
        - alert: MimicExemplarsRefused
          expr: sum by (reason) (increase(mimic_controller_exemplars_refused_total[1h])) > 0
          labels: {severity: info}
          annotations:
            summary: "Exemples par endpoint refusés ({{ $labels.reason }}) : certains endpoints ne seront plus mockables après la rétention"
        - alert: MimicControllerSlowTakeover
          expr: max(mimic_controller_takeover_seconds) > 30
          labels: {severity: info}
          annotations:
            summary: "La dernière prise de relais du contrôleur a pris plus de 30 s"
        - alert: MimicInjectorErrors
          expr: sum(increase(mimic_injector_admissions_total{result="error"}[10m])) > 0
          labels: {severity: warning}
          annotations:
            summary: "Revues d'admission invalides : des pods sont créés sans mimic"
```

Notes sur ces règles :

- `MimicControllerUnreachableFromSidecars` ne voit que les pods qui ont quelque chose à
  publier : un cycle sans nouvel échange compte `ok` sans appeler le contrôleur. Un pod en
  mock avec `capture.inbound: off` ne publie rien et ne déclenche donc jamais cette alerte.
  Les battements en échec ne sont visibles que dans les logs et `/api/status`.
- `MimicMockWithoutSnapshot` se déclenche aussi pour un sidecar sans contrôleur basculé en
  mock par son API d'admin (cas voulu) : filtrer par namespace si vous utilisez ce mode.
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
| `MimicShareBacklogLoss` | durée de l'indisponibilité du contrôleur | rétablir le contrôleur ; si la perte est structurelle (débit très élevé), monter `share.max_backlog_bytes` ou passer l'entrant en `metadata` |
| `MimicMockUnmatched` | `/ui/<ns>/<wl>` : opérations découvertes ; `kubectl logs` de l'application pour les `599` ; `mimicctl report` | enregistrer plus longtemps (opérations rares), vérifier `tls.intercept`, utiliser `mock-partial` pour compléter, ou `mock.unmatched: passthrough` |
| `MimicMockWithoutSnapshot` | logs du sidecar : `battement vers le contrôleur échoué` avec `snapshot <id>: …` ; `forme de mock du snapshot … altérée` | rétablir l'accès au contrôleur ; le téléchargement est retenté à chaque battement |
| `MimicMockTlsRefused` | `mimic_passthrough_connections_total` par pod, logs de l'application | ajouter l'hôte à `mimic.platform.io/tls-intercept`, ré-enregistrer, ou l'exclure (`exclude-outbound-cidrs`) s'il doit rester réel |
| `MimicTlsInterceptRejected` | log `poignée de main TLS refusée par l'application` avec l'authority | l'application ne lit pas `SSL_CERT_FILE` & co : ajouter `/var/run/mimic/ca/ca.crt` à son magasin (JVM : `keytool`), ou retirer l'hôte de l'interception |
| `MimicInboundUpstreamErrors` | port d'écoute de l'application, adresse (le sidecar essaie l'IP du pod puis `127.0.0.1`) | application pas encore prête (démarrage lent) ou qui écoute ailleurs : `inbound.upstream` ou `mimic.platform.io/inbound-ports` |
| `MimicControllerDown` | `kubectl -n mimic-system get pods -l app.kubernetes.io/name=mimic-controller -L mimic.platform.io/leader`, `describe`, logs | PVC non attaché (multi-attach `ReadWriteOnce`), certificat absent, configuration invalide (code 2) ; voir [Exploiter en production](Exploiter-en-production.md) |
| `MimicControllerNotRedundant` | réplique `Pending` ou `ContainerCreating` | souvent un PVC `ReadWriteOnce` monté sur un autre nœud : `ReadWriteMany` ou S3 |
| `MimicControllerRestarting` | `kubectl logs --previous` : `bail non renouvelé à temps`, `bail tenu par une autre réplique`, OOM | API server lente ou réseau (bail) ; mémoire : `max_exchanges_total` ou limite |
| `MimicControllerMemoryHigh` | `mimic_controller_exchanges`, `mimic_controller_exemplars_bytes` | baisser `max_exchanges_total` (environ 2 Ko par échange) ou monter la limite (4 Gi par défaut) |
| `MimicControllerStorageFull` | taille par workload sous `/var/lib/mimic-controller` ; snapshots accumulés | purger les vieux snapshots (jamais supprimés automatiquement), baisser `max_bytes_per_workload`, agrandir le PVC |
| `MimicInjectorDown` | `kubectl -n mimic-system get pods -l app.kubernetes.io/name=mimic-injector`, logs | rétablir, puis `kubectl rollout restart` des workloads créés pendant la panne pour les réinjecter |
| `MimicControllerNoLeader` | logs : `lecture du bail`, `renouvellement du bail` ; `kubectl -n mimic-system get lease mimic-controller -o yaml` | droits RBAC sur `leases`, API server ; la prise de relais attend aussi la liste des namespaces (`prise de relais`) |
| `MimicControllerTwoLeaders` | labels `mimic.platform.io/leader` des deux pods | doit se résorber seul (la réplique qui perd le bail sort) ; sinon redémarrer l'ancienne active |
| `MimicControllerSegmentErrors` | log `ingestion d'un segment` | support plein ou en lecture seule, droits S3 |
| `MimicControllerAuthUnavailable` | log `authentification impossible` | API server, droits `tokenreviews` `create` et `pods` `get` du ClusterRole `mimic-controller` |
| `MimicControllerAuthRejected` | `reason` : `missing`/`invalid` (jeton, audience `mimic-controller`), `forbidden` (pod sans nom de workload, workload d'un autre) | vérifier le volume `mimic-token` et l'annotation `mimic.platform.io/workload` |
| `MimicPodsDiverging` | `/ui/<ns>/<wl>` : état de chaque pod ; logs des pods concernés | voir [Basculer en mock](Basculer-en-mock.md#vérifier-lalignement) |
| `MimicExemplarsRefused` | `reason` | `count` : monter `max_exemplars_per_workload` ; `bytes` : monter `max_exemplar_bytes_per_workload` / `_total` |
| `MimicInjectorErrors` | log `revue d'admission invalide` | version d'API server, autre webhook qui modifie la revue |
