# Inventaire des erreurs et avertissements visibles par l'exploitant (mimic)

Relevé en lecture seule de `main` (a714ebe). Les messages sont copiés tels quels du code ; `<…>` marque une valeur substituée.
Docs comparées : `Depannage.md` (Dep), `Developpeur-applicatif.md` (Dev), `Alertes-et-supervision.md` (Alr), `Exploiter-en-production.md` (Exp), toutes sous `docs/guide/`.
Colonne « Doc » : nom(s) court(s) de la doc = documenté ; `partiel : …` = le comportement est décrit mais sans le message exact, ou le texte de la doc est inexact ; `non` = absent des 4 docs (`ailleurs : X.md` quand une autre page du guide en parle).

Conventions de canal :
- **exit N** : message sur stderr, puis sortie du processus en code N (le conteneur redémarre en CrashLoopBackOff). Le préfixe est `mimic: ` pour le sidecar, `mimic-init: ` pour l'init, `mimic-injector: ` pour le webhook, et aucun pour le contrôleur.
- **log WARN/ERROR/INFO/DEBUG** : ligne JSON (`tracing`) ; le texte du message est dans `fields.message`, et `error`, `namespace`, `workload`… sont des champs à part.
- **HTTP NNN → X** : réponse HTTP vers le destinataire X (body entre guillemets).
- **CRD** : champ ou condition de `MimicWorkload.status`, publié toutes les 5 s (kube.rs:98-141).

---

## 1. Contrôleur (`crates/mimic-controller`, plus `mimic-net` pour TLS et serveur)

### 1.1 Démarrage et configuration

`main.rs:35-47` : les erreurs de `ControllerConfig::load` sortent en **code 2**, celles de `start()` en **code 1**.

| # | Message exact | Où | Canal | Déclencheur | Cause probable en cluster | Action | Doc |
|---|---|---|---|---|---|---|---|
| C01 | `lecture de <fichier>: <err io>` | config.rs:194 | exit 2 | `--config`/`MIMIC_CONTROLLER_CONFIG` pointe vers un fichier illisible | ConfigMap `mimic-controller` non montée, mauvaise clé, mauvais `subPath` | vérifier le volume et la clé de la ConfigMap | non |
| C02 | `configuration <fichier> invalide: <err serde_yaml>` | config.rs:196 | exit 2 | YAML invalide, champ inconnu (`deny_unknown_fields`), durée négative ou non numérique (config.rs:251), `auth.kind` inconnu | faute de frappe dans la ConfigMap | corriger, puis `rollout restart deploy/mimic-controller` | Dep |
| C03 | `aggregated exige tls (l'API server joint le contrôleur en HTTPS)` | config.rs:206 | exit 2 | `aggregated` présent sans `tls` | overlay partiel | ajouter `tls` (cert/clé du Secret `mimic-controller-tls`) | non |
| C04 | `store non supporté: <url>` | config.rs:209 (aussi lib.rs:380) | exit 2 | `store` sans préfixe `file://` ni `s3://` | chemin nu (`/var/lib/...`) | préfixer avec `file://` | Dep |
| C05 | `max_exchanges_per_workload et max_exchanges_total doivent être positifs` | config.rs:213 | exit 2 | l'une des deux valeurs vaut 0 | réglage | valeur > 0 | Dep (générique « … doit être positif ») |
| C06 | `max_exemplars_per_workload doit être positif` | config.rs:217 | exit 2 | valeur 0 | réglage | valeur > 0 | Dep (générique) |
| C07 | `max_exemplar_bytes_per_workload et max_exemplar_bytes_total doivent être positifs` | config.rs:221 | exit 2 | valeur 0 | réglage | valeur > 0 | Dep (générique) |
| C08 | `snapshot.max_per_key doit être positif` | config.rs:226 | exit 2 | valeur 0 | réglage | valeur > 0 | Dep (générique) |
| C09 | `élection : il faut 0 < retry_period (<n> ms) < renew_deadline (<n> ms) < lease_duration (<n> ms)` | mimic-core/src/lease.rs:38, appelé par config.rs:229 | exit 2 | durées de `leader_election` incohérentes | réglage du bail modifié (par exemple renew_deadline ≥ lease_duration) | respecter l'ordre (défaut 2 < 10 < 15 s) | non |
| C10 | `leader_election : lease et namespace requis` | config.rs:231 | exit 2 | `lease` ou `namespace` vide | réglage | renseigner | Dep |
| C11 | `auth kubernetes exige tls : les sidecars n'acceptent le contrôleur qu'en HTTPS` | config.rs:236 | exit 2 | `auth: kubernetes` (le défaut) sans `tls` ; c'est aussi le cas d'une configuration absente | ConfigMap absente ou sans section `tls` | ajouter `tls` | Dep |
| C12 | `reload_interval doit être positif` (dans C02) | mimic-net/src/tls.rs:54 | exit 2 | `tls.reload_interval: 0` | réglage | valeur > 0 (défaut 10) | non |
| C13 | `client Kubernetes: <err>` | lib.rs:195 | exit 1 | `kube::Client::try_default()` échoue | `automountServiceAccountToken: false`, hors cluster sans kubeconfig, `KUBERNETES_SERVICE_HOST` absent | remonter le token du SA, vérifier le ServiceAccount | non (ailleurs : Configurer.md) |
| C14 | `certificat <fichier>: <err>` / `clé <fichier>: <err>` / `certificat <fichier>: aucun certificat` / `certificat <c> et clé <k>: <err>` | mimic-net/src/tls.rs:102-130 | exit 1 | cert ou clé TLS illisible, PEM vide, ou clé qui ne correspond pas au cert | Secret `mimic-controller-tls` pas encore émis par cert-manager (le pod démarre avant), mauvais chemin | `kubectl -n mimic-system get certificate`, attendre `Ready`, vérifier le montage | partiel : Exp (« au démarrage, un certificat illisible fait échouer le processus », sans le message) |
| C15 | `aggregated.client_ca_file <chemin>: <err>` | lib.rs:212 | exit 1 | fichier de CA du front proxy illisible | volume absent | monter le fichier ou retirer `client_ca_file` | non |
| C16 | `aggregated sans client_ca_file exige auth kubernetes` | lib.rs:219 | exit 1 | `aggregated` sans CA avec `auth: static` | config de test poussée en cluster | `auth: kubernetes` ou `client_ca_file` | non |
| C17 | `kube-system/extension-apiserver-authentication: <err>` | aggregated.rs:108 | exit 1 | lecture de la ConfigMap du front proxy refusée | RoleBinding `extension-apiserver-authentication-reader` (kube-system) manquant | appliquer le RBAC livré | non |
| C18 | `requestheader-client-ca-file absent : l'API server n'a pas de front proxy` | aggregated.rs:113 | exit 1 | clé absente de la ConfigMap | API server sans `--requestheader-client-ca-file` (certaines distributions ou kind custom) | activer l'aggregation layer, ou donner `aggregated.client_ca_file` | non |
| C19 | `requestheader-allowed-names illisible: <err>` | aggregated.rs:116 | exit 1 | valeur non JSON | ConfigMap modifiée | `aggregated.allowed_names` explicite | non |
| C20 | `CA cliente: <err>` / `CA cliente: aucun certificat` | mimic-net/src/tls.rs:241-251 | exit 1 | CA du front proxy vide ou invalide | ConfigMap ou fichier vide | idem C17/C18 | non |
| C21 | `aggregated exige auth kubernetes` | lib.rs:232 | exit 1 | `aggregated` avec `auth: static` | config de test | `auth: kubernetes` | non |
| C22 | `leader_election exige auth kubernetes (bail Kubernetes)` | lib.rs:250 | **exit 1** | élection avec `auth: static` | config de test | `auth: kubernetes` | partiel : Dep (la doc le range à tort en code 2) |
| C23 | `leader_election : identité absente (POD_NAME)` | config.rs:116 via lib.rs:253 | exit 1 | ni `identity`, ni `POD_NAME`, ni `HOSTNAME` | Deployment modifié sans `POD_NAME` (downward API) | remettre `env POD_NAME` | non |
| C24 | `liste des namespaces (garde-fou production): <err>` | kube.rs:32 via lib.rs:265 | exit 1 (sans élection seulement) | `list namespaces` en échec avant de servir | ClusterRole sans `namespaces list/watch`, API server injoignable | RBAC du ClusterRole `mimic-controller` | non (avec élection, voir C38) |
| C25 | `écoute sur <adresse>: <err>` | lib.rs:272, 275, 282 | exit 1 | bind impossible (api, ui ou aggregated) | port déjà pris, `ui_listen` sur une IP absente | changer `listen`/`ui_listen`/`aggregated.listen` | non (Dep ne cite que la variante sidecar) |
| C26 | `répertoire <dir>: <err>` | lib.rs:360-362 | exit 1 | `create_dir_all` du support `file://` en échec | PVC non monté, permissions (fsGroup 1338 absent), racine en lecture seule | vérifier PVC, `fsGroup` | non |
| C27 | `bucket <bucket>: <err>` | lib.rs:371 | exit 1 | `AmazonS3Builder::build` en échec | région ou endpoint manquant (`AWS_REGION`), URL mal formée | renseigner les `AWS_*` du Secret `mimic-controller-s3` ou l'IRSA | non |
| C28 | `signal SIGTERM: <err>` | main.rs:52 | exit 1 | handler de signal impossible | quasi impossible | aucune | non |

### 1.2 Logs en marche

| # | Message exact | Où | Canal | Déclencheur | Cause probable | Action | Doc |
|---|---|---|---|---|---|---|---|
| C29 | `surveillance des namespaces` (`error`) | kube.rs:56 | log WARN, répété avec backoff | erreur du watcher Namespace | RBAC `namespaces watch`, API server | RBAC | Dep |
| C30 | `surveillance des MimicWorkload` (`error`) | kube.rs:92 | log WARN | erreur du watcher de la CRD | CRD absente ou ancienne version, RBAC | `kubectl apply -f deploy/controller/crd.yaml`, RBAC | Dep |
| C31 | `statut non publié` (`error`, `namespace`, `name`) | kube.rs:137 | log WARN, retenté toutes les 5 s | `patch_status` refusé | droit `mimicworkloads/status patch` manquant ; CRD sans sous-ressource status | RBAC, CRD | Dep |
| C32 | `lecture du bail` (`error` = `lecture du bail <nom>: <err>`) | leader.rs:300, kube.rs:192 | log WARN (passive) | `get lease` en échec | RBAC `leases get`, API server | Role `mimic-controller` dans mimic-system | Dep, Alr |
| C33 | `prise du bail` (`error` = `écriture du bail <nom>: <err>`) | leader.rs:290, kube.rs:252 | log **DEBUG** | create/patch du bail refusé (le 409 est normal en concurrence) | RBAC `leases create/patch` : invisible au niveau `info` | `MIMIC_LOG=debug` si aucune réplique ne devient active | non |
| C34 | `bail non renouvelé à temps : la réplique cesse de servir` | leader.rs:329 | log ERROR, puis exit 1 (main.rs:60) | aucun renouvellement réussi avant `renew_deadline` | API server lente, réseau, throttling client | normal si isolé ; récurrent → API server | Dep, Alr |
| C35 | `bail tenu par une autre réplique : la réplique cesse de servir` | leader.rs:335 | log ERROR, puis exit 1 | le bail a changé de détenteur | partition, horloge, double déploiement | idem | Dep, Alr |
| C36 | `renouvellement du bail` (`error`) | leader.rs:338 | log WARN | un renouvellement en échec (retenté) | API server | si C34 suit : API server | Dep |
| C37 | `bail pris : prise de relais` / `réplique active` / `réplique passive` (`holder`) | leader.rs:304, 384, 294 | log INFO | changement de rôle | — | repère de chronologie | Alr (les deux premiers) |
| C38 | `prise de relais` (`error` = `liste des namespaces (garde-fou production): …`) | leader.rs:354 | log WARN, retenté toutes les `retry_period` ; la réplique ne sert pas pendant ce temps | `list namespaces` en échec pendant la prise de relais | RBAC namespaces, API server | RBAC | Dep, Alr |
| C39 | `liste initiale des MimicWorkload non reçue : l'état persisté sert` | leader.rs:373 | log WARN après 30 s | le watcher n'a pas livré `InitDone` | CRD absente, RBAC, API server lente | comme C30 | Dep |
| C40 | `marque de la réplique active` (`error` = `pods marqués: <err>` ou `marque du pod <pod>: <err>`) | leader.rs:378, kube.rs:266, 181 | log WARN en boucle ; **la réplique sert, mais le Service (sélecteur `mimic.platform.io/leader=true`) n'a pas d'endpoint** | list/patch des pods de mimic-system refusé | Role sans `pods list/patch` | RBAC ; sinon les sidecars voient `délai dépassé` (S38) | non (Dep ne cite que la variante d'arrêt C41) |
| C41 | `marque de la réplique active non retirée` | leader.rs:249 | log WARN à l'arrêt | patch du pod en échec pendant l'arrêt | API server injoignable | la bascule attend l'expiration du bail | Dep |
| C42 | `bail non rendu : il expirera` | leader.rs:266 | log WARN à l'arrêt | écriture du bail en échec | idem | idem | Dep |
| C43 | `authentification impossible` (`error`) | api.rs:75 | log WARN, et 503 au sidecar (C66) | TokenReview ou `get pod` en échec | ClusterRole sans `tokenreviews create`/`pods get`, API server | RBAC | Dep, Alr |
| C44 | `ingestion d'un segment` (`error` = `écriture de <path>: …`, `lecture de …`) | api.rs:157 | log WARN, et 503 au sidecar | erreur du support pendant l'ingestion (index, exemples, sel, put) | PVC plein ou en lecture seule, droits S3 | espace, droits | Dep, Alr |
| C45 | `état souhaité illisible sur le support` (`namespace`, `workload`, `error`) | api.rs:270 ; ui.rs:122 | log WARN, et **503 `support indisponible` au battement** ou à l'UI | lecture de `desired.json` en erreur (autre que NotFound) | support injoignable (S3, NFS) | rétablir le support ; le sidecar garde son état | non |
| C46 | `sel du workload illisible sur le support` | api.rs:286 | log WARN ; battement répondu **sans `redact_salt`** | lecture ou écriture de `<ns>/<wl>/redact-salt` en échec, ou fichier vide (`<path> vide`, state.rs:1028) | support ; fichier de sel tronqué | rétablir ; un fichier vide doit être supprimé pour qu'un sel soit retiré ; en attendant, les sidecars sans sel perdent leurs captures (S86 `no_salt`) | non |
| C47 | `desired.json illisible : état souhaité vide` | state.rs:798 | log **ERROR** | `desired.json` présent mais JSON invalide | fichier corrompu ou écrit par une version incompatible | les pods retombent sur le mode de leur configuration (un mock peut être perdu) ; réappliquer la `MimicWorkload` (`kubectl annotate`/`mimicctl mode`) pour réécrire le fichier | non |
| C48 | `lecture de l'état souhaité, retentée au prochain appel` | state.rs:803 | log WARN | erreur du support à la lecture de `desired.json` | support | rétablir | non |
| C49 | `ressource réappliquée à la prochaine maintenance` | state.rs:1589 | log WARN ; spec gardée, réappliquée dans la minute | idem C48 au moment d'un événement `MimicWorkload` | support | aucune ; vérifier que le mode suit | non |
| C50 | `index des segments` | state.rs:656, 1853 | log WARN | liste des segments du support en échec (prise de relais, maintenance) | support | rétablir | non |
| C51 | `relecture des exemples` | state.rs:659, 907, 1971 | log WARN | lecture de `exemplars.jsonl` en échec | support, fichier corrompu | rétablir ; sinon les endpoints rares ne sont plus mockables | non |
| C52 | `relecture des échanges` | state.rs:943 | log WARN | lecture des segments en échec à la première vue d'un workload | support | découverte et snapshot appauvris | non |
| C53 | `liste des workloads du support` | state.rs:858 | log WARN | listage de la racine du support en échec | support | rétablir | non |
| C54 | `état persisté rechargé` (`workloads`) | state.rs:663 | log INFO | fin du warm-up | — | repère | Exp |
| C55 | `snapshot construit` / `état souhaité` | state.rs:1633, 1666 | log INFO | construction ou changement d'état | — | repère | non |
| C56 | `construction du snapshot échouée` (`error` = `construction du snapshot: …`, `écriture du snapshot <id>: …`, `métadonnées du snapshot <id>: …`) | state.rs:1636 ; 1383, 1428, 1440 | log WARN + raison CRD `SnapshotFailed` | build ou écriture en échec | support plein | espace, droits | Dep |
| C57 | `écriture de l'état souhaité échouée` | state.rs:1669 | log WARN | put de `desired.json` en échec | support | rétablir (sinon l'état est perdu au redémarrage) | Dep |
| C58 | `forme de mock impossible` (`snapshot`) | state.rs:1323, 1327 | log WARN ; `/snapshots/<id>/mock` répond alors 404 (le sidecar se replie sur le snapshot complet) | `EncodedMock::from_full` en échec, par exemple `contenu du snapshot <id> altéré (empreinte <x>)` (snapshot.rs:129) | fichier de snapshot modifié ou corrompu sur le support | supprimer ou réimporter le snapshot, changer `spec.snapshot` | non |
| C59 | `forme de mock calculée` | state.rs:1311 | log INFO | — | — | repère | non |
| C60 | `écriture des exemples : aucun segment supprimé` | state.rs:1857 | log WARN | flush des exemples en échec pendant la rétention | support | espace ; surveiller le volume | Dep |
| C61 | `écriture des exemples` | state.rs:1899 | log WARN | flush en échec (arrêt, maintenance) | support | idem | Dep |
| C62 | `relecture avant suppression` (`segment`) | state.rs:1991 | log WARN | lecture d'un segment à purger en échec | support | idem | non |
| C63 | `écriture des exemples : segments gardés` | state.rs:2004 | log WARN | idem, branche borne de volume | support | idem | partiel : Dep (seule la variante « aucun segment supprimé » est citée) |
| C64 | `suppression d'un segment` (`segment`) | state.rs:2019 | log WARN | delete refusé | droits S3 `DeleteObject`, FS en lecture seule | droits ; sinon le volume croît | non |
| C65 | `compaction : lecture` / `compaction : écriture` / `compaction : exemples non écrits, segments bruts gardés` | state.rs:2089, 2105, 2113 | log WARN | erreur du support pendant la compaction | support | espace, droits | Dep |
| C66 | `arrêt d'urgence` (`namespace`, `stop`) | state.rs:694 | log WARN | label `mimic.platform.io/emergency-stop` posé ou retiré | `mimicctl emergency-stop` | voulu ; tracer qui l'a fait | non (Exp décrit la commande, pas le log) |
| C67 | `nouveau certificat illisible : l'ancien reste servi` | mimic-net/src/tls.rs:202 | log WARN, une fois par contenu refusé | rechargement à chaud du cert ou de la clé en échec | Secret réécrit à moitié, clé qui ne correspond pas | réparer le Certificate ; l'ancien expire ensuite | Dep, Alr, Exp |
| C68 | `certificat rechargé` | mimic-net/src/tls.rs:197 | log INFO | rechargement réussi | renouvellement cert-manager | repère | Exp |
| C69 | `accept échoué, écoute suspendue 50 ms` (`omitted`) | mimic-net/src/serve.rs:89 | log WARN, limité | `accept()` en erreur | EMFILE (trop de connexions ou de FD), limite de descripteurs | ulimit, connexions, `max_connections` | non |
| C70 | `arrêt : connexions coupées après le délai de drainage` (`remaining`) | mimic-net/src/serve.rs:175 | log WARN à l'arrêt | connexions encore ouvertes après le drainage | téléchargements longs, sidecars en keep-alive | aucune | non |
| C71 | `poignée de main TLS refusée` / `poignée de main TLS trop longue` | mimic-net/src/serve.rs:148-149 | log **DEBUG** | handshake TLS en échec ou trop long côté serveur | sidecar avec une ancienne CA, scanner, client HTTP en clair | passer en debug pour confirmer une rotation de CA ratée | non |

### 1.3 Réponses HTTP de l'API des sidecars (port 8443, `api.rs`), vues côté sidecar dans `controller.last_error`

Elles arrivent au sidecar sous la forme `<METHODE> <chemin>: <code> <motif> <body>` (control.rs:273).

| # | Message exact | Où | Canal | Déclencheur | Cause probable | Action | Doc |
|---|---|---|---|---|---|---|---|
| C72 | `jeton absent ou invalide` | api.rs:67 | HTTP 401 → sidecar ; `mimic_controller_auth_failures_total{reason="missing"\|"invalid"}` | pas de Bearer, TokenReview non authentifié, audience ≠ `mimic-controller`, user hors `system:serviceaccount:`, pod introuvable (auth.rs:391-418) | volume `mimic-token` absent, `token_audience` différente, pod supprimé | volume projeté, audience | Dep, Alr |
| C73 | `jeton valide pour un autre pod ou workload` | api.rs:71 | HTTP 403 → sidecar ; `reason="forbidden"` | namespace, workload ou instance ≠ identité du jeton ; pod sans nom de workload (auth.rs:419) | `MIMIC_WORKLOAD` figé à l'admission ≠ annotation ou label actuel, `POD_NAME` modifié | annotation cohérente, recréer le pod | Dep, Alr |
| C74 | `authentification indisponible` | api.rs:78 | HTTP 503 → sidecar ; `reason="unavailable"` | voir C43 | RBAC, API server | RBAC | Dep, Alr |
| C75 | `réplique passive du contrôleur : la réplique active sert les sidecars` | leader.rs:136 | HTTP 503 + `Retry-After: 2` → sidecar, UI, mimicctl | requête vers une réplique non active (hors `/healthz` et `/metrics`) | bascule, EndpointSlice périmée, Service sans sélecteur de leader | transitoire ; sinon le label leader (C40) | Dep, Alr |
| C76 | `nom invalide` | api.rs:103, 193, 246 ; ui.rs:65 | HTTP 400 | namespace, workload, instance ou fichier hors charset (`valid_name`) | `POD_NAME`/`MIMIC_WORKLOAD` exotiques | — | non (ailleurs : Administration-et-API.md) |
| C77 | `identifiant de segment invalide: <id>` | state.rs:1085 | HTTP 400 → sidecar (`synchronisation du partage échouée`, S44) | `segment=` mal formé | sidecar d'une version incompatible | aligner les versions | non (ailleurs : Administration-et-API.md) |
| C78 | `instance réservée: <instance>` | state.rs:1090 | HTTP 400 | instance qui commence par `_` | nom de pod forgé | — | non (ailleurs : Administration-et-API.md) |
| C79 | `ligne <n>: <err serde>` | state.rs:1102 | HTTP 400 à l'ingestion ; **le segment reste en tête de file et bloque la publication** (share.rs:349) | ligne JSONL non décodable en `Exchange` | sidecar plus récent ou plus ancien que le contrôleur (format), corruption | mettre à niveau le contrôleur ; sinon `DELETE /store` impossible sous contrôleur → recréer le pod | non |
| C80 | `support indisponible` (segments) | api.rs:158 | HTTP 503 ; `mimic_controller_segments_total{result="error"}` | C44 | support | espace, droits | Dep, Alr |
| C81 | `support indisponible` (battement) | api.rs:271 | HTTP 503 au battement ; le sidecar reste dans son état | C45 | support | rétablir | non |
| C82 | `length limit exceeded` (texte axum par défaut) | api.rs:22-24, 50, 43 | HTTP 413 | segment > 64 MiB ou battement > 64 KiB | relevé `unmatched` démesuré d'un sidecar ancien | — | non |
| C83 | `battement invalide: <err serde>` | api.rs:242 | HTTP 400 | JSON du battement illisible ou champ requis manquant | version incompatible | aligner les versions | non |
| C84 | `Failed to deserialize query string: …` (texte axum) | api.rs:131 (`Query<SegmentQuery>`) | HTTP 400 | `instance` manquant sur POST segments | client non mimic | — | non |
| C85 | `<err>` (ex. `lecture de …`) | api.rs:180 | HTTP 502 sur GET segments | index illisible | support | rétablir | non |
| C86 | `segment inconnu` | api.rs:197 | HTTP 404 | segment absent (purgé) | rétention | — | non (ailleurs : Administration-et-API.md) |
| C87 | `snapshot inconnu` | api.rs:315, 355 | HTTP 404 → sidecar ; d'abord sur `/mock` (repli silencieux vers le complet), puis sur `/snapshots/<id>`, ce qui donne au sidecar `snapshot <id>: GET /snapshots/<id>: 404 Not Found snapshot inconnu` | snapshot désigné absent du support | purgé à la main, support restauré sans lui, snapshot corrompu (C58) | `mimicctl snapshot ls`, réimporter ou changer `spec.snapshot` | partiel : Dep (`snapshot <id>: …` générique) |
| C88 | `forme de mock illisible` | api.rs:340 | HTTP 500 | décompression de la forme en cache en échec (client sans `Accept-Encoding`) | rare (mémoire) | réessayer | non |

### 1.4 UI (8081, port-forward), API agrégée (8444) et mimicctl

| # | Message exact | Où | Canal | Déclencheur | Cause probable | Action | Doc |
|---|---|---|---|---|---|---|---|
| C89 | `support indisponible` | ui.rs:123 | HTTP 503 (mimicctl status/mode, UI) | C45 sur GET mode | support | rétablir | non |
| C90 | JSON `{"namespace":…, "name":…, "error":"lecture de <ns>/<wl>/desired.json: …"}` | ui.rs:115 | élément de GET /v1/workloads (mimicctl status) | C45 | support | rétablir | non |
| C91 | `snapshot inconnu` / `<err>` | ui.rs:177 / 178 | HTTP 404 / 500 sur pin | `mimicctl snapshot pin` d'un id inexistant / écriture en échec | faute de frappe / support | — | non (ailleurs : CLI-mimicctl.md) |
| C92 | `<err>` (ex. `ligne <n>: …`, `contenu …`) | ui.rs:191 | HTTP 400 sur import | `mimicctl snapshot import` d'un fichier invalide | fichier non JSONL de snapshot | — | non |
| C93 | `dépendance inconnue` | ui.rs:228 | HTTP 404 | `export --outbound <authority>` inconnue | authority sans le port (`hôte:port` attendu) | `mimicctl report` pour la liste | non (ailleurs : CLI-mimicctl.md) |
| C94 | `direction: inbound ou outbound` / `format: json, jsonl ou k6` | ui.rs:250, 255 | HTTP 400 | paramètre invalide | — | — | non (ailleurs : Administration-et-API.md) |
| C95 | (body vide) | ui.rs:296 | HTTP 500 | mise en forme des requêtes en échec (tâche bloquante) | mémoire | — | non |
| C96 | `<chemin> : route inconnue` (Status `NotFound`) | aggregated.rs:354-356 | HTTP 404 Status k8s → kubectl/mimicctl | chemin hors API | version de mimicctl ou APIService | aligner les versions | non |
| C97 | `API de lecture seule : GET uniquement` (`MethodNotAllowed`) | aggregated.rs:361-363 | HTTP 405 | verbe ≠ GET | — | — | non |
| C98 | `X-Remote-User absent` (`Unauthorized`) | aggregated.rs:379-381 | HTTP 401 | requête sans en-têtes du front proxy | appel direct ou front proxy mal configuré | passer par l'API server | non |
| C99 | `<user> ne peut pas <verbe> workloads/<sub> <ns>/<wl> (data.mimic.platform.io) : demander un RoleBinding du ClusterRole mimic-data-viewer` (aussi `… workloads dans <ns>`, `… workloads sur le cluster`) | aggregated.rs:390-406 | HTTP 403 Status → `mimicctl export/requests/report/snapshot ls`, `kubectl get --raw` | SubjectAccessReview refusée | RoleBinding manquant | lier `mimic-data-viewer` dans le namespace | partiel : Dev (le besoin du RoleBinding, pas le message) |
| C100 | `SubjectAccessReview impossible` + `autorisation impossible : <err>` (`ServiceUnavailable`) | aggregated.rs:411-416 | log WARN + HTTP 503 | SAR en échec | ClusterRole sans `subjectaccessreviews create`, API server | RBAC | non |
| C101 | 400 `BadRequest` / 500 vide | aggregated.rs:425, 441, 444 | HTTP | URI interne invalide / liste illisible | rare | — | non |
| C102 | `mimic : mock et mock-partial sont refusés dans un namespace labellisé env=production` | deploy/controller/production-guard.yaml:23 | refus d'admission (ValidatingAdmissionPolicy) à `kubectl apply`/`mimicctl mode` | `spec.mode` mock dans un namespace `env=production` (k8s ≥ 1.30) | voulu | — | partiel : Exp (fichier cité), Dep (message mimicctl différent) |

### 1.5 Statut CRD `MimicWorkload` (reconcile.rs, publié par kube.rs:98)

| # | Valeur exacte | Où | Canal | Déclencheur | Cause probable | Action | Doc |
|---|---|---|---|---|---|---|---|
| C103 | `Aligned=False`, reason `InvalidMode` | reconcile.rs:189 | CRD | `spec.mode` hors des modes connus | faute (la CRD valide normalement l'enum ; CRD ancienne) | corriger, CRD à jour | non (ailleurs : Configurer.md) |
| C104 | reason `InvalidCapture` | reconcile.rs:193 | CRD | `spec.capture.inbound` inconnu | idem | idem | non (ailleurs : Configurer.md) |
| C105 | reason `InvalidMock` | reconcile.rs:197 | CRD | `spec.mock.websocket` inconnu | idem | idem | non (ailleurs : Configurer.md) |
| C106 | reason `SnapshotNotFound` | reconcile.rs:207 | CRD ; l'état précédent reste appliqué | `spec.snapshot` absent du support | snapshot purgé, mauvais id, autre cluster | `mimicctl snapshot ls`/`import`, épingler | Exp |
| C107 | reason `SnapshotFailed` | state.rs:1637 | CRD | C56 | support | espace, droits | Dep |
| C108 | reason `NoPods` | reconcile.rs:298 | CRD | aucun pod sélectionné vu depuis `pod_timeout` | pods non injectés, `spec.selector` trop strict, battements en échec | vérifier l'injection et `/ui` | non (ailleurs : Basculer-en-mock.md, Configurer.md) |
| C109 | reason `PodsDiverging` | reconcile.rs:299 | CRD ; `mimic_controller_pods{state="diverging"}` | un pod n'annonce pas le mode, snapshot, capture ou websocket souhaités | téléchargement du snapshot en échec (S47-S55), sidecar trop ancien pour la capture ou le websocket | logs du pod divergent | Alr |
| C110 | reason `ProductionNamespace` ; `Allowed=False` message `mock et mock-partial sont refusés dans un namespace labellisé env=production` | reconcile.rs:14-16, 343-352 | CRD | mock demandé dans un namespace `env=production` ; l'état repasse en `record` | voulu | retirer le mock | non (ailleurs : Basculer-en-mock.md, Configurer.md) |
| C111 | reason `EmergencyStop` ; `Allowed=False` message `arrêt d'urgence : label mimic.platform.io/emergency-stop=true sur le namespace ou sur mimic-system, pods en passthrough` | reconcile.rs:334-341 | CRD | label d'arrêt d'urgence | `mimicctl emergency-stop` | `--off` | partiel : Exp (commande décrite, pas la condition) |
| C112 | `SnapshotEmpty=True`, reason `Empty`, message `le snapshot servi ne contient aucun échange sortant rejouable` | reconcile.rs:322-331 | CRD | mode mock avec un snapshot à 0 échange | snapshot construit avant tout trafic sortant enregistré, capture `off` | enregistrer du trafic, puis repasser en `record` → `mock` (nouveau snapshot) | non |
| C113 | `status.unmatched` > 0 | crd.rs:133-136 | CRD (et `UNMATCHED` de mimicctl status) | 599/413 relevés par les pods sélectionnés | couverture du snapshot | `mimicctl report` | Dev |

### 1.6 Métriques du contrôleur liées à une erreur (metrics.rs)

| # | Métrique | Où | Signification | Doc |
|---|---|---|---|---|
| C114 | `mimic_controller_auth_failures_total{reason}` | metrics.rs:97 | C72-C74 | Alr |
| C115 | `mimic_controller_segments_total{result="error"}` | metrics.rs:92 | ingestion refusée (400 ou 503) | Alr |
| C116 | `mimic_controller_exemplars_refused_total{reason="count"\|"bytes"}` | metrics.rs:132 | exemples refusés par la borne : **aucun log ne l'accompagne** | Alr |
| C117 | `mimic_controller_leader` | metrics.rs:142 | 1 sur l'active | partiel : Alr (alertes NoLeader/TwoLeaders, sans le nom de la métrique) |
| C118 | `mimic_controller_takeover_seconds` | metrics.rs:147 | durée de la dernière prise de relais | Dep, Alr |
| C119 | `mimic_controller_certificate_expiry_timestamp_seconds` | metrics.rs:152 | expiration du certificat servi | Alr |
| C120 | `mimic_controller_pods{state="diverging"\|"absent"}` | metrics.rs:107 | alignement | Alr |

---

## 2. Sidecar (`crates/mimic-proxy`)

### 2.1 Démarrage (main.rs:47-62)

Les erreurs de `Config::load_with` sortent en **code 2**, celles de `start()` en **code 1**. Le message est sur stderr avec le préfixe `mimic: `.

| # | Message exact | Où | Canal | Déclencheur | Cause probable | Action | Doc |
|---|---|---|---|---|---|---|---|
| S01 | `mimic: lecture de <fichier>: <err>` | config.rs:595 | exit 2 | `--config`/`MIMIC_CONFIG` illisible | ConfigMap `mimic.platform.io/config` absente ou mauvaise clé | vérifier la ConfigMap | non |
| S02 | `mimic: configuration <fichier> invalide: <err>` | config.rs:597 | exit 2 | YAML ou champ inconnu, variante inconnue (`share.startup.fallback`, `mock.tcp`…) | faute de frappe | corriger | Dep |
| S03 | `mimic: MIMIC_CONFIG_YAML invalide: <err>` | config.rs:600 | exit 2 | `sidecar_config` du webhook invalide | ConfigMap `mimic-injector` | corriger (le webhook valide aussi au démarrage, J17) | Dep |
| S04 | `mimic: MIMIC_MODE inconnu: <m>` | config.rs:610 | exit 2 | variable invalide | pod modifié à la main | corriger | Dep |
| S05 | `mimic: MIMIC_STORE_SIZE_LIMIT_BYTES invalide (octets) : <v>` | config.rs:640 | exit 2 | valeur non entière (par exemple `512Mi`) | resourceFieldRef ou valeur posée à la main | entier en octets | non (ailleurs : Configurer.md) |
| S06 | `mimic: mock.from (mock local) et share sont incompatibles : le mock local se passe de contrôleur` | config.rs:675-678 | exit 2 | `--mock-from`/`MIMIC_MOCK_FROM` avec `share` | `MIMIC_MOCK_FROM` posé dans un pod injecté | retirer l'un des deux | non |
| S07 | `mimic: redact.salt requis sans contrôleur : fournir MIMIC_REDACT_SALT (Secret <workload>-mimic, clé redact-salt) ou redact.salt` | config.rs:687-691 | exit 2 | ni sel, ni mock local, ni `share` https | sidecar hors contrôleur (`share` file/s3, ou sans `share`) sans Secret `<workload>-mimic` | créer le Secret, ou passer sous contrôleur | non (ailleurs : Configurer.md) |
| S08 | `mimic: mock.latency_factor doit être entre 0 et 100` / `capture.max_exchanges doit être positif` / `capture.max_store_bytes doit être positif` / `store.segment_bytes et store.max_segments doivent être positifs` | config.rs:694-703 | exit 2 | bornes | réglage | corriger | Dep |
| S09 | `mimic: store.max_cached_snapshots doit être positif` | config.rs:706 | exit 2 | 0 | réglage | ≥ 1 | non |
| S10 | `mimic: <nom> doit être une durée positive (en secondes)` (`outbound.connect_timeout`, `server.header_read_timeout`, `share.interval`, `share.mode_poll`, `share.startup.timeout`) | config.rs:730 | exit 2 | durée nulle | réglage | > 0 | partiel : Dep (seulement `share.startup.timeout`) |
| S11 | `mimic: <nom> doit être positif` (`server.max_connections`, `capture.max_inflight_bytes`, `share.max_snapshot_bytes`) | config.rs:733 | exit 2 | 0 | réglage | > 0 | non |
| S12 | `mimic: share.startup.timeout ne peut dépasser 300 s : la startupProbe du sidecar ne l'attendrait pas` | config.rs:738-741 | exit 2 | > 300 s | réglage | ≤ 300 | Dep |
| S13 | `mimic: server.max_connections ne peut dépasser <n>` | config.rs:744 | exit 2 | > `Semaphore::MAX_PERMITS` | réglage | baisser | non |
| S14 | `mimic: share exige un nom de workload (MIMIC_WORKLOAD)` | config.rs:751 | exit 2 | `share` sans workload | — | — | Dep |
| S15 | `mimic: share.url non supportée: <url>` | config.rs:757 | exit 2 | schéma inconnu | — | — | Dep |
| S16 | `mimic: share.url en http:// refusée : le contrôleur doit être joint en HTTPS (share.allow_insecure pour les tests)` | config.rs:760-763 | exit 2 | — | — | — | Dep |
| S17 | `mimic: share.ca_file requis : seule la CA du contrôleur fait foi` | config.rs:766 | exit 2 | — | — | — | Dep |
| S18 | `mimic: instance invalide: <instance>` | config.rs:773 | exit 2 | `POD_NAME` hors charset | — | — | Dep |
| S19 | `mimic: tls.ca_cert_file et tls.ca_key_file vont ensemble` | tls.rs:93 | exit 1 | une seule des deux CA fournie | ConfigMap incomplète | fournir les deux ou aucune | non |
| S20 | `mimic: <fichier>: <err io>` | tls.rs:87, 89, 196, 211 | exit 1 | CA d'interception, `tls.upstream_ca_file` ou `share.ca_file` illisible | ConfigMap `mimic-controller-ca` sans la clé attendue, Secret de CA absent | vérifier trust-manager et les volumes | non |
| S21 | `mimic: <fichier>: aucun certificat` | tls.rs:217 | exit 1 | `share.ca_file` sans aucun PEM | ConfigMap `mimic-controller-ca` publiée vide (trust-manager en échec) | `kubectl -n <ns> get cm mimic-controller-ca -o yaml` | non |
| S22 | `mimic: <err rcgen ou rustls>` | tls.rs:46-70, 196-230 | exit 1 | PEM de CA ou de CA additionnelle invalide | clé chiffrée, format PKCS#1 inattendu | fournir un PEM PKCS#8 non chiffré | non |
| S23 | `mimic: <dir>: <err>` (ou `<err io>` sans chemin) | tls.rs:103-120 | exit 1 | écriture de `ca.crt` ou `ca-bundle.crt` dans `tls.ca_dir` en échec | `emptyDir` `/var/run/mimic/ca` absent ou racine en lecture seule | volume du webhook | non |
| S24 | `mimic: <fichier>: <err>` / `mimic: snapshot <fichier>: <err>` (par ex. `ligne <n>: …`) | lib.rs:100, 102 | exit 1 | `--mock-from` illisible ou invalide | mauvais fichier (pas un `mimicctl pull`) | refaire `mimicctl pull` | non |
| S25 | `mimic: store <dir>: <err io>` | lib.rs:117 | exit 1 | création ou relecture du journal local en échec (store.rs:164-188) | `emptyDir` `mimic-store` non monté, permission (UID 1338), segment illisible (I/O) | volume ; en dernier recours, vider l'emptyDir (recréer le pod) | non |
| S26 | `mimic: écoute <Inbound\|Outbound\|Admin> sur <adresse>: <err>` | lib.rs:133 | exit 1 | bind impossible | port pris par l'application ou par un autre sidecar | changer les ports | Dep |
| S27 | `mimic: sonde <chemin> : connexion à <adresse> : <err>` / `… : requête vers <adresse> : <err>` / `… : réponse de <adresse> : <err>` / `mimic: sonde <chemin> : <chemin> : statut <code\|absent>` | main.rs:28, probe.rs:30-59 | exit 1 de la sonde `exec` (événement `Startup/Readiness/Liveness probe failed`) | admin injoignable ou non 2xx | sidecar pas encore prêt, barrière fermée (`503`) | voir S67 | Dep, Alr |

### 2.2 Logs en marche

| # | Message exact | Où | Canal | Déclencheur | Cause probable | Action | Doc |
|---|---|---|---|---|---|---|---|
| S28 | `plafond de connexions simultanées atteint, connexion refusée` (`listener`, `omitted`) | lib.rs:276 | log WARN (1/s) + `mimic_connections_refused_total{listener}` ; la connexion est **fermée sans réponse** (reset vu par le client) | plus de `server.max_connections` connexions (ou plafond admin) | tir de charge, pool de connexions qui fuit, keep-alive | monter `server.max_connections`, ou réduire la concurrence | non (ailleurs : Metriques.md) |
| S29 | `poignée de main TLS interceptée hors délai` (`authority`) | lib.rs:298 | log **DEBUG** + `mimic_tls_handshake_failures_total{reason="timeout"}` | handshake TLS côté application > `server.header_read_timeout` | client lent, CPU saturé | — | non |
| S30 | `poignée de main TLS refusée par l'application` (`authority`) | lib.rs:318 | log WARN + `{reason="client_rejected"}` | l'application refuse le certificat forgé | CA mimic absente du magasin (JVM), épinglage | ajouter `/var/run/mimic/ca/ca.crt` au magasin, ou retirer l'hôte | Dep, Alr |
| S31 | `certificat forgé impossible` (`name`) | tls.rs:182 | log WARN, handshake refusé | rcgen ne peut pas signer pour ce SNI | SNI exotique (IP, joker) | retirer l'hôte de `tls.intercept` | Dep |
| S32 | `échec vers l'amont` (`authority`, `error`, `omitted`) | proxy.rs:309 | log WARN (1/s) + `mimic_upstream_errors_total{kind}` + 502/504 à l'application | requête vers l'amont en échec | voir S74/S75 | — | Dep |
| S33 | `tunnel vers l'amont impossible` (`authority`) | proxy.rs:881 | log WARN (1/s) + `mimic_upstream_errors_total{kind="connect"\|"connect_timeout"}` ; le client a déjà reçu `200` au CONNECT, la connexion est coupée | CONNECT relayé, mais la connexion vers l'amont échoue | dépendance HTTPS injoignable via `HTTPS_PROXY`, DNS, NetworkPolicy | comme une panne de la dépendance | non |
| S34 | `tunnel impossible` (`target`, `error`) | proxy.rs:1116 | log WARN (1/s) + `mimic_passthrough_connections_total{result="error"}` ; connexion fermée | relais TCP opaque (non HTTP, entrant ou sortant) en échec de connexion | base ou broker injoignable ; en entrant, l'application n'écoute pas sur ce port | vérifier la dépendance, `inbound-ports` | non |
| S35 | `connexion TLS sortante refusée en mode mock (hôte hors de tls.intercept)` (`destination`, `kind`) | proxy.rs:1024 | log WARN (1/s) + `{kind="tls",result="refused"}` | voir le message | hôte non intercepté | intercepter ou exclure | Dep |
| S36 | `connexion TCP sortante refusée en mode mock (mock.tcp: refuse)` | proxy.rs:1033 | idem `{kind="tcp"}` | — | base, broker, cache | exclure les ports, ou `mock.tcp: relay` | Dep, Alr |
| S37 | `connexion sortante muette refusée en mode mock (mock.tcp: refuse)` | proxy.rs:954 | idem, après 30 s | connexion où le serveur parle en premier | MySQL, SMTP | exclure les ports | Dep |
| S38 | `mock : requête sans réponse enregistrée` (`method`, `authority`, `path`, `trace_id`, `status`=599) | state.rs:424-438 | log WARN (1/s) | voir S70 | — | — | Dep, Alr |
| S39 | `mock : body de requête trop gros` (`status`=413) | state.rs:426 | log WARN (1/s) | voir S71 | — | — | Dep |
| S40 | `écriture du journal local échouée` (`dir`, `omitted`) | journal.rs:248 | log WARN, limité | écriture, rotation ou suppression d'un segment du journal en échec | `emptyDir` plein (`sizeLimit`) → éviction proche, FS en lecture seule | `MimicStoreDiskHigh`, bornes du store | non |
| S41 | `annotation de mode invalide` (`value`) | watch.rs:39 | log WARN | annotation `mimic.platform.io/mode` invalide (sidecar non piloté seulement) | faute de frappe | `passthrough`, `record`, `mock`, `mock-partial` | non |
| S42 | `synchronisation du partage échouée` (`error`) | share.rs:244 | log WARN à chaque cycle en échec + `mimic_share_sync_total{result="error"}` + `share.last_error` | publication vers le contrôleur (erreurs S46-S50, C72-C82) ou vers le support objet | voir l'erreur | voir l'erreur | Dep, Alr |
| S43 | (dans S42, support objet) `liste du partage: <err>` / `lecture de <clé>: <err>` / `publication de <path>: <err>` / `répertoire de partage <dir>: <err>` / `bucket <bucket>: <err>` / `URL de partage non supportée: <url>` | share.rs:306, 324-327, 359, 376-378, 387, 398 | idem | partage pair à pair `file://` ou `s3://` (hors contrôleur) | droits S3, région, volume partagé | droits ; le support est retenté au cycle suivant | non |
| S44 | `battement vers le contrôleur échoué` (`error`, `omitted`) | control.rs:504 | log WARN (≤ 1 par 30 s) + `mimic_heartbeats_total{result="error"}` + `controller.last_error` | battement `Failed` (pas de réponse) ou `Incomplete` (état reçu mais pas appliqué en entier) | voir S45-S56 | voir le détail | Dep, Alr |
| S45 | `jeton <fichier>: <err>` | control.rs:170 | dans S42/S44 | jeton projeté illisible | volume `mimic-token` absent | volume | Dep |
| S46 | `<METHODE> <chemin>: délai dépassé` | control.rs:182, 190 | dans S42/S44 | pas de réponse en `TIMEOUT` (ou 6× pour le corps) | NetworkPolicy, Service sans endpoint (C40), contrôleur arrêté | endpoints, labels | Dep, Alr |
| S47 | `<METHODE> <chemin>: <chaîne d'erreurs hyper/rustls>` (`client error (Connect): dns error…`, `tcp connect error: Connection refused`, `invalid peer certificate: UnknownIssuer\|BadSignature\|Expired`, `invalid URL, scheme is not https`) | control.rs:183, 309-318 | dans S42/S44 | erreur de connexion, de TLS ou d'URL | DNS du Service, CA du sidecar ≠ CA de l'émetteur (UnknownIssuer : ConfigMap `mimic-controller-ca` ancienne ou d'un autre cluster), cert expiré | selon le cas ; rotation de CA (Exp) | partiel : Dep (BadSignature et expiration seulement ; UnknownIssuer, DNS et refus non cités) |
| S48 | `<METHODE> <chemin>: réponse au-delà de share.max_snapshot_bytes (<n> octets)` | control.rs:193 | dans S44 (`snapshot <id>: …`) ; le pod reste en mock sur ses données locales, `Aligned=False PodsDiverging` | forme de mock compressée ou snapshot complet > `share.max_snapshot_bytes` | gros workload, `snapshot.max_per_key` élevé | monter `share.max_snapshot_bytes` (ConfigMap), réduire le snapshot | non |
| S49 | `<METHODE> <chemin>: <code> <motif> <body ≤ 200 o>` | control.rs:273-277 | dans S42/S44 | réponse non 200/201 | voir C72-C88 | voir C72-C88 | Dep |
| S50 | `battement: <err serde>` | control.rs:241 | dans S44 | réponse 200 du battement non JSON ou de forme inattendue | proxy HTTP intermédiaire qui répond une page, versions incompatibles | aligner les versions, vérifier l'URL | non |
| S51 | `snapshot <id>: forme de mock du snapshot <id>: illisible en <encodage>, ou au-delà de share.max_snapshot_bytes (<n> octets) une fois décompressée` | control.rs:294 | dans S44 | décompression en échec ou trop grosse | gros snapshot | monter `share.max_snapshot_bytes` | non |
| S52 | `snapshot <id>: forme de mock du snapshot <id> altérée (empreinte <x>, ETag <y>)` | control.rs:305 | dans S44 | empreinte ≠ ETag | proxy intermédiaire, corruption | retenté | Dep, Alr |
| S53 | `snapshot <id>: forme de mock illisible: <e>` / `format de forme de mock inconnu: <n>` / `forme de mock du snapshot <x> au lieu de <id>` | mimic-core/src/snapshot.rs:226-234 | dans S44 | JSON de la forme invalide, format plus récent, id différent | contrôleur plus récent que le sidecar | mettre à jour les sidecars | non |
| S54 | `snapshot <id>: contenu du snapshot <id> altéré (empreinte <x>)` | mimic-core/src/snapshot.rs:129 via control.rs:262 | dans S44 | repli sur le snapshot complet (contrôleur ancien, ou C58) dont l'empreinte ne correspond pas | snapshot corrompu sur le support | réimporter ou reconstruire | non |
| S55 | `snapshot <id>: GET /snapshots/<id>: 404 Not Found snapshot inconnu` | control.rs:258-262 | dans S44 | C87 | snapshot purgé | C87 | partiel : Dep (`snapshot <id>: …` générique) |
| S56 | `mode souhaité inconnu: <m>` / `capture de l'entrant inconnue: <c>` / `réglage WebSocket inconnu: <w>` | control.rs:599, 604, 609 | dans S44 (`Incomplete`) | valeur inconnue du sidecar | contrôleur plus récent | mettre à jour les sidecars | Dep |
| S57 | `battement vers le contrôleur rétabli` (`failures`) | control.rs:497 | log INFO | premier battement réussi après des échecs | — | fin d'incident | non |
| S58 | `état souhaité non gardé sur disque` (`error`) | control.rs:424 | log WARN | écriture de `controller/desired.json` ou de la forme de mock dans `store.dir` en échec | `emptyDir` plein ou absent | sinon, au prochain redémarrage du conteneur, le pod repart en attente (barrière) au lieu de son dernier état | non |
| S59 | `dernier état souhaité rechargé` / `snapshot chargé` | control.rs:368, 693 | log INFO | — | — | repères | non |
| S60 | `aucun état souhaité connu : attente du contrôleur en mock, rien ne sort` (`fallback`) | control.rs:386-389 | log INFO ; le pod est en **mock** le temps du démarrage | pod neuf sous contrôleur avec un repli `mock-local` ou `fail` | — | explique des 599 au démarrage | non |
| S61 | `contrôleur injoignable au démarrage : repli share.startup.fallback appliqué, l'application démarre` (`timeout_s`, `fallback`, `mode`) | control.rs:550-555 | log WARN ; `mimic_startup{state="fallback"}` | échéance sans état connu | contrôleur injoignable pour un pod neuf | rétablir, recréer le pod si son workload est en mock | Dep, Alr |
| S62 | `pas aligné à l'échéance du démarrage : dernier état souhaité connu appliqué, l'application démarre` | control.rs:556-560 | log WARN ; `mimic_startup{state="last-known"}` | état reçu (ou rechargé) mais snapshot pas téléchargé à l'échéance | S48, S51-S55, contrôleur redevenu injoignable | voir la `last_error` | non (Alr cite l'état `last-known`, pas le message) |
| S63 | `contrôleur injoignable au démarrage avec share.startup.fallback: fail : l'application attend sa réponse` | control.rs:561-564 | log **ERROR** ; `/readyz` 503, pod en `Init` | échéance avec `fail` sans état connu | contrôleur injoignable | rétablir le contrôleur | partiel : Dep, Alr (le cas `fail` est décrit, pas ce message) |
| S64 | `accept échoué, écoute suspendue 50 ms` (`omitted`) | throttle.rs:58 | log WARN (1/s) | `accept()` en erreur | EMFILE sous un tir | ulimit, `server.max_connections` | non |

### 2.3 API admin du sidecar (15100)

| # | Message exact | Où | Canal | Déclencheur | Cause probable | Action | Doc |
|---|---|---|---|---|---|---|---|
| S65 | `admin mimic : route réservée à la boucle locale (kubectl port-forward)` | admin.rs:47-50 | HTTP 403 | appel non local hors `/healthz`, `/readyz` et `/metrics` en GET | curl depuis un autre pod ou par l'IP du pod | `kubectl port-forward pod/<pod> 15100` | non |
| S66 | `sidecar piloté par le contrôleur : basculer via la ressource MimicWorkload` | admin.rs:78-81 | HTTP 403 | `PUT /mode` ou `DELETE /store` sous contrôleur | ancien runbook | `mimicctl mode` | non (ailleurs : Administration-et-API.md) |
| S67 | `waiting` (ou `aligned`, `last-known`, `fallback`) | admin.rs:161-171 | HTTP 503 tant que la barrière est fermée → startupProbe | sidecar piloté pas encore aligné | voir S60-S63 | — | Dep, Alr |
| S68 | `body trop gros` / `mode inconnu (passthrough, record, mock, mock-partial)` / `direction: inbound ou outbound` | admin.rs:85, 97-100, 116 | HTTP 413 / 400 / 400 | appel admin mal formé | — | — | non (ailleurs : Administration-et-API.md) |
| S69 | `partage désactivé` / `introuvable` / `dépendance inconnue` / `<err serde>` | admin.rs:131, 133, 74, 154 | HTTP 404 / 404 / 404 / 500 | — | — | — | non (ailleurs : Administration-et-API.md) |

### 2.4 Réponses fabriquées pour l'application, et en-têtes

| # | Message exact | Où | Canal | Déclencheur | Cause probable | Action | Doc |
|---|---|---|---|---|---|---|---|
| S70 | `mimic: aucun enregistrement ne correspond à cette requête` + `x-mimic-unmatched: true` | proxy.rs:777-785 | HTTP **599** → application ; `mimic_mock_responses_total{match="unmatched"}` | mock, aucune décision `Matched`, `mock.unmatched` ≠ passthrough | appel jamais enregistré, body non capturé (non JSON), HTTPS non intercepté | enregistrer plus, `mock-partial`, `tls-intercept`, `capture-raw-bodies` | Dev, Dep, Alr |
| S71 | `mimic: body trop gros pour le mode mock` | proxy.rs:689-691 | HTTP 413 ; `{match="too_large"}` | body de requête > `capture.max_body_bytes` en mock | gros upload | monter la borne | Dev, Dep |
| S72 | `mimic: WebSocket sortant refusé en mock (mock.websocket: refuse)` + `x-mimic-websocket: refused` | proxy.rs:420-425 | HTTP 503 ; `mimic_websocket_connections{result="refused"}` | poignée de main WS sortante en mock avec `refuse` | voulu | `relay` | Dev, Dep |
| S73 | `mimic: tunnel refusé en mode mock (TLS non intercepté)` | proxy.rs:860-861 | HTTP 403 au CONNECT | proxy explicite, hôte non intercepté, mode `mock` | — | intercepter ou exclure | Dev, Dep |
| S74 | `mimic: amont injoignable` | proxy.rs:336 | HTTP 502 ; `mimic_upstream_errors_total{kind="connect"\|"request"}` | erreur de connexion ou de requête vers l'amont | dépendance en panne, application pas prête ; CA d'entreprise (TLS) | `tls.upstream_ca_file`, readiness | Dev, Dep |
| S75 | `mimic: connexion vers l'amont non établie dans le délai (outbound.connect_timeout)` | proxy.rs:331-333 | HTTP 504 ; `{kind="connect_timeout"}` | connexion non établie dans le délai | NetworkPolicy qui DROP, dépendance lente | réseau | Dev |
| S76 | `mimic: aucune destination pour le trafic entrant` | proxy.rs:79-81 | HTTP 502 | entrant sans destination d'origine ni `inbound.upstream` | appel direct sur 15106 | — | Dev, Dep |
| S77 | `mimic: requête sans destination (ni proxy explicite, ni redirection)` | proxy.rs:146-148 | HTTP 400 | — | appel direct sur 15101 | — | Dev, Dep |
| S78 | `mimic: URI invalide` | proxy.rs:247 | HTTP 400 | URI reconstruite non analysable | — | — | Dev |
| S79 | `mimic: CONNECT sans destination` | proxy.rs:838 | HTTP 400 | — | client mal configuré | — | Dev |
| S80 | connexion fermée sans réponse (TCP ou TLS refusé en mock, plafond de connexions) | proxy.rs:1015-1038, lib.rs:263-280 | reset ou EOF vu par l'application | S28, S35-S37 | — | — | Dev (mock) ; non (plafond) |
| S81 | `x-mimic-match: <exact\|template\|fuzzy>;score=<s>` | proxy.rs:823-825 | en-tête | réponse mockée | — | diagnostic | Dev, Dep |

### 2.5 Métriques du sidecar liées à une erreur (metrics.rs)

| # | Métrique et label | Où | Signification | Doc |
|---|---|---|---|---|
| S82 | `mimic_capture_dropped_total{reason="share_backlog"}` | state.rs:252 | file de publication pleine (contrôleur injoignable) | Alr |
| S83 | `{reason="local_backlog"}` | state.rs:253, journal.rs:140-150 | file du journal local pleine : échanges non écrits sur disque (disque lent) | non (ailleurs : Collecte-et-stockage.md, Metriques.md) |
| S84 | `{reason="store_budget"}` | state.rs:251 | store mémoire plein (`capture.max_store_bytes`) | non (ailleurs : idem) |
| S85 | `{reason="inflight_budget"}` / `{reason="truncated"}` / `{reason="sample_limit"}` | proxy.rs:580-603, 630 | bodies non copiés (budget en vol), tronqués (> `max_body_bytes`), non gardés (échantillonnage) : rend des mocks non rejouables (599) | non |
| S86 | `{reason="no_salt"}` | state.rs:137 | **échange abandonné faute de sel** (sous contrôleur, avant le premier battement avec sel, ou C46) | non (ailleurs : Collecte-et-stockage.md) |
| S87 | `{reason="unredactable"}` | state.rs:148 | champ non redactable retiré | non (ailleurs : idem) |
| S88 | `mimic_upstream_errors_total{direction,kind}` | metrics.rs:234 | S32-S34, S74, S75 | Dep, Alr |
| S89 | `mimic_tls_handshake_failures_total{reason="client_rejected"\|"timeout"}` | metrics.rs:310 | S29, S30 | Alr (sans le détail `timeout`) |
| S90 | `mimic_passthrough_connections_total{result="refused"\|"error"}` | metrics.rs:305 | S34-S37 | Dep, Alr (`refused` seulement) |
| S91 | `mimic_connections_refused_total{listener}` | metrics.rs:320 | S28 | non (ailleurs : Metriques.md) |
| S92 | `mimic_heartbeats_total{result="error"}`, `mimic_heartbeat_last_success_timestamp_seconds`, `mimic_share_sync_total{result="error"}`, `mimic_startup{state}` | metrics.rs:325-335, 284 | S42, S44, S60-S63 | Alr |
| S93 | `mimic_mock_responses_total{match="unmatched"\|"too_large"}`, `mimic_mock_snapshot_exchanges` | metrics.rs:239, 294 | S70, S71, mock sans snapshot | Dep, Alr |
| S94 | `mimic_store_disk_bytes` / `mimic_store_disk_limit_bytes` | metrics.rs:264-269 | risque d'éviction (S40) | Alr |

---

## 3. `mimic-init` (`crates/mimic-init`)

main.rs:69-80 : `Failure::Config` sort en **code 2**, `Failure::Apply` en **code 1**, le message sur stderr avec le préfixe `mimic-init: `. Le pod reste en `Init:Error`/`Init:CrashLoopBackOff` et l'application ne démarre pas.

| # | Message exact | Où | Canal | Déclencheur | Cause probable | Action | Doc |
|---|---|---|---|---|---|---|---|
| I01 | `mimic-init: MIMIC_MESH: mesh inconnu: <v> (auto, none, istio, istio-ambient, linkerd)` | apply.rs:98, lib.rs:122 | exit 2 | `MIMIC_MESH` invalide | pod ou CNI modifié à la main (le webhook filtre l'annotation) | corriger | partiel : Dep (texte sans le préfixe `MIMIC_MESH: `) |
| I02 | `mimic-init: MIMIC_INBOUND_PORTS: port invalide: "<v>" (entier de 1 à 65535)` | apply.rs:99-100, lib.rs:530 | exit 2 | élément non entier ou 0 | valeur posée à la main (le webhook filtre l'annotation) | `*`, `-` ou une liste | partiel : Dep (sans le nom de variable ni le détail) |
| I03 | `mimic-init: MIMIC_EXCLUDE_INBOUND_PORTS: port invalide: …` | apply.rs:101-102 | exit 2 | idem | idem | idem | partiel : Dep (générique « port invalide ») |
| I04 | `mimic-init: MIMIC_EXCLUDE_OUTBOUND_PORTS: port invalide: …` | apply.rs:105-106 | exit 2 | idem | idem | idem | partiel : Dep (générique) |
| I05 | `mimic-init: MIMIC_EXCLUDE_OUTBOUND_CIDRS: CIDR invalide: "<v>" (adresse IPv4 ou IPv6, /préfixe facultatif)` | apply.rs:103-104, lib.rs:542 | exit 2 | CIDR mal formé | valeur posée à la main | corriger | non |
| I06 | `mimic-init: POD_IP: adresse IP invalide: "<v>"` | apply.rs:107-113, lib.rs:572 | exit 2 | `POD_IP` non vide et invalide | pod modifié | retirer ou corriger | non |
| I07 | erreur clap : `error: invalid value '<v>' for '--inbound-port <INBOUND_PORT>': …` (ou `--proxy-uid`, `--outbound-port`, `--admin-port`) | main.rs:9-16 | exit 2 (clap) | `MIMIC_*_PORT`/`MIMIC_PROXY_UID` non numériques | pod modifié | corriger | non |
| I08 | `mimic-init: <outil>-restore: <err io>` (par ex. `No such file or directory`) | apply.rs:212, 218, 219 | exit 1 | binaire introuvable ou stdin cassé | `MIMIC_IPTABLES` vers un binaire absent de l'image | `iptables`, `iptables-legacy`, `iptables-nft` | Dep (`iptables: <erreur>`) |
| I09 | `mimic-init: <outil>-restore a échoué (<status>)` | apply.rs:223 | exit 1 | restore refusé | noyau en legacy contre nft, `NET_ADMIN` manquant, module `nat` absent | `iptables` du webhook, capacités, PSA `privileged` | Dep |
| I10 | `mimic-init: <outil>-save: <err>` / `mimic-init: <outil>-save a échoué (<status>)` | apply.rs:200-204 | exit 1 (la détection du mesh en mode `auto` ignore cet échec, apply.rs:131) | relecture après restore en échec | mêmes causes que I09 | idem | Dep |
| I11 | `mimic-init: <outil> <args> a échoué (<status>)` (par ex. `iptables -t nat -I PREROUTING 1 -p tcp -j MIMIC_INBOUND`) | apply.rs:234 | exit 1 | insertion des sauts en échec | règle concurrente du mesh, table verrouillée (`xtables lock`) | relancer le pod, ordre des init containers | partiel : Dep (générique « a échoué ») |
| I12 | `mimic-init: ip6tables-restore a échoué (…)` | apply.rs:156-158, 182 | exit 1 | `MIMIC_IPV6=true` sur un nœud sans IPv6 ou `ip6tables` | dual-stack mal annoncé | retirer `MIMIC_IPV6` | non |
| I13 | `mimic-init: mesh <None\|IstioSidecar\|…>, binaire <outil>` | apply.rs:136-141 | stdout INFO | toujours | `mesh None` dans un pod du mesh = règles du mesh pas encore posées | ordre des init containers, `rollout restart` | Dep |
| I14 | `mimic-init: règles <outil> posées` | apply.rs:189 | stdout INFO | succès | — | repère | Dep |

---

## 4. Webhook d'injection (`crates/mimic-injector`)

Le webhook est en `failurePolicy: Ignore` (deploy/injector/injector.yaml:199, 236) : s'il est en panne, les pods sont créés **sans mimic** et sans aucun message.

| # | Message exact | Où | Canal | Déclencheur | Cause probable | Action | Doc |
|---|---|---|---|---|---|---|---|
| J01 | `mimic non injecté : injection désactivée par mimic.platform.io/inject` | inject.rs:151, server.rs:86 | Warning d'admission (kubectl, événements du ReplicaSet) ; `mimic_injector_admissions_total{result="skipped"}` | annotation `inject: "false"` | voulu | — | Dep, Alr |
| J02 | `mimic non injecté : hostNetwork : les règles iptables toucheraient le noeud` | inject.rs:154 | idem | `hostNetwork: true` | — | non pris en charge | Dep |
| J03 | `mimic non injecté : aucun nom de workload : poser mimic.platform.io/workload ou le label app.kubernetes.io/name` | inject.rs:712-714 | idem | ni annotation, ni `app.kubernetes.io/name`, ni `app` (valeurs vides ignorées) | chart sans labels standards | poser le label ou l'annotation sur le template | Dep |
| J04 | `mimic non injecté : pod sans spec` | inject.rs:703 | idem | objet sans spec | revue anormale | — | non |
| J05 | `mimic.platform.io/mesh inconnue : <v> (auto, none, istio, istio-ambient, linkerd) ; mesh détecté par mimic-init` | inject.rs:87-94 | Warning d'admission (pod **injecté**) | valeur inconnue | faute | corriger | Dep |
| J06 | `mimic.platform.io/inbound-ports : port invalide: "<v>" (entier de 1 à 65535), valeur ignorée` | inject.rs:184 | Warning (injecté, élément écarté) | élément invalide | faute | corriger | non |
| J07 | `mimic.platform.io/inbound-ports : aucun port valide, ports déclarés interceptés` | inject.rs:202-204 | Warning ; ce sont alors les ports déclarés des conteneurs qui sont interceptés | aucun élément valide | faute | corriger ; vérifier les `containerPort` | non |
| J08 | `mimic.platform.io/exclude-outbound-cidrs : CIDR invalide: "<v>" (adresse IPv4 ou IPv6, /préfixe facultatif), valeur ignorée` | inject.rs:184 (via 211-219) | Warning ; **ce CIDR n'est pas exclu** (son trafic est intercepté, voire coupé en mock) | CIDR invalide | faute | corriger et recréer le pod | non |
| J09 | `mimic.platform.io/exclude-outbound-ports : port invalide: …, valeur ignorée` | inject.rs:184 (via 220-227) | Warning ; port non exclu | idem | faute | idem | non |
| J10 | `mimic.platform.io/log : <v> : <err EnvFilter>, valeur ignorée` | inject.rs:233 | Warning ; `MIMIC_LOG` non posé | filtre invalide | faute (`DEBUG`, virgule finale…) | syntaxe `EnvFilter` | non |
| J11 | `revue d'admission invalide: <err>` | server.rs:54-58 | **réponse d'admission** `invalid` (`allowed: false`) et `result="error"` ; aucun log de ce texte, seulement `admission` result=error (server.rs:112) | `AdmissionReview` non convertible | version d'API, autre webhook qui modifie la revue | — | partiel : Dep, Alr (présenté comme un log, et comme un « pod créé sans mimic ») |
| J12 | `patch non sérialisable` (`error`) | server.rs:79 | log WARN ; `result="error"` ; pod admis **sans** mimic | `with_patch` en échec (et server.rs:70, silencieux) | rare | — | non |
| J13 | `mimic-injector: lecture de <f>: <err>` | config.rs:78 | exit 1 | config illisible | ConfigMap `mimic-injector` absente | — | non |
| J14 | `mimic-injector: configuration <f> invalide: <err>` | config.rs:80 | exit 1 | YAML invalide | — | — | Dep |
| J15 | `mimic-injector: image requise` | config.rs:103 | exit 1 | `image` vide | ConfigMap incomplète | renseigner `image` | non |
| J16 | `mimic-injector: sidecar_config invalide: <err>` | config.rs:106 | exit 1 | `sidecar_config` refusé par le parseur du sidecar | — | — | Dep |
| J17 | `mimic-injector: sidecar_log: <err>` / `mimic-injector: exclude_outbound_cidrs: CIDR invalide: …` | config.rs:108, 111 | exit 1 | filtre de logs ou CIDR global invalide | — | corriger | non (ailleurs : Configurer.md pour `sidecar_log`) |
| J18 | `mimic-injector: certificat …` / `clé …` (cf. C14) | server.rs:200 (mimic-net tls.rs:102-130) | exit 1 | cert TLS du webhook illisible au démarrage | Secret `mimic-injector-tls` pas encore émis | attendre cert-manager | partiel : Exp (comportement) |
| J19 | `mimic-injector: écoute sur <addr>: <err>` | server.rs:203 | exit 1 | bind impossible | port | — | non |
| J20 | `mimic-injector: signal SIGTERM: <err>` | main.rs:43 | exit 1 | — | rare | — | non |
| J21 | `KUBERNETES_SERVICE_HOST n'est pas une adresse IP : API Kubernetes non exclue d'office` | config.rs:90-93 | log WARN au démarrage ; **les pods injectés intercepteront le trafic vers l'API Kubernetes** | variable sous forme de nom DNS | cluster où le kubelet pose un nom (certains managés ou proxys) | ajouter le CIDR de l'API à `exclude_outbound_cidrs` | non (ailleurs : Installer.md) |
| J22 | `nouveau certificat illisible : l'ancien reste servi` (webhook) | mimic-net tls.rs:202 | log WARN | cf. C67 | — | — | partiel : Alr (« logs du webhook ») |
| J23 | `mimic_injector_admissions_total{result="injected"\|"skipped"\|"error"}`, `mimic_injector_certificate_expiry_timestamp_seconds` | server.rs:144-150 | métriques | — | — | — | Alr |

---

## 5. Comptage

| Composant | Entrées | Documentées | Partielles | Non documentées (dans les 4 docs) |
|---|---|---|---|---|
| Contrôleur (C01-C120) | 120 | 45 | 8 | 67 |
| Sidecar (S01-S94) | 94 | 48 | 4 | 42 |
| mimic-init (I01-I14) | 14 | 5 | 5 | 4 |
| Injecteur (J01-J23) | 23 | 7 | 3 | 13 |
| **Total** | **251** | **105** | **20** | **126** |

(Une ligne qui regroupe des variantes d'un même message compte pour une entrée.)

## 6. Écarts doc ↔ code relevés en passant

- Depannage.md, « Contrôleur, sort en code 2 » : `leader_election exige auth kubernetes (bail Kubernetes)` sort en réalité en **code 1** (lib.rs:250, erreur de `start()`).
- Depannage.md et Alertes-et-supervision.md présentent `revue d'admission invalide` comme un **log** du webhook : c'est le message de la réponse d'admission (`AdmissionResponse::invalid`, server.rs:57). Le log n'est que `admission` avec `result="error"`.
- Depannage.md cite `mimic-init: mesh inconnu: …` et `mimic-init: port invalide: X` : les vrais messages sont préfixés du nom de la variable (`mimic-init: MIMIC_MESH: mesh inconnu: …`, `mimic-init: MIMIC_INBOUND_PORTS: port invalide: "X" (entier de 1 à 65535)`).
- Alertes-et-supervision.md, `MimicControllerStorageFull` : « purger les vieux snapshots (jamais supprimés automatiquement) », ce que contredit Exploiter-en-production.md (purge au-delà de `snapshot.keep_last` à chaque maintenance).
- `mimic_controller_exemplars_refused_total` n'a aucun log associé : l'alerte est le seul signal.
