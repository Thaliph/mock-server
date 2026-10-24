# TODO — mimic, de « ça marche sur kind » à « installable et exploitable en production »

Issu de la revue du 25 octobre 2026 : trois revues indépendantes (sécurité du sidecar, sécurité
du plan de contrôle, image et manifests), l'inventaire des messages d'erreur
([docs/revue/inventaire-erreurs.md](docs/revue/inventaire-erreurs.md)) et une installation
faite comme un nouvel administrateur, sur kind, sans cloner le dépôt.

Priorités : **P0** bloque tout usage par quelqu'un d'autre que l'auteur, ou est une faille ;
**P1** nécessaire avant une première équipe en production ; **P2** simplification du produit ;
**P3** plus tard. Chaque ligne cite sa source : `[sidecar n]`, `[plan n]`, `[image n]`,
`[inventaire]`, `[install]` (essai sur kind).

Fait pendant la revue : binaire iptables limité à trois noms dans `mimic-cni`, qui tourne en
root sur le nœud (#55) ; tests instables MCK-15, LOD-11, PRX-11 (#53, #54).

## P0 — personne d'autre ne peut installer ni tester

- [ ] **CI réparée** : aucun job ne démarre depuis des jours (« recent account payments have
  failed or your spending limit needs to be increased »). Toutes les PR récentes sont
  fusionnées sur tests locaux. Réparer la facturation, ou rendre le dépôt public, ou un runner
  auto-hébergé. `[install]`
- [ ] **Première release `v0.1.0`** : aucune image sur ghcr.io (pull anonyme refusé, `403`),
  aucun binaire `mimicctl`, aucun tag. Les manifests visent une image qui n'existe pas :
  `ErrImagePull` sur tout cluster. `[install]`
- [ ] **Dépôt privé** : `kubectl apply -k github.com/Thaliph/mock-server//deploy/controller`
  demande un identifiant. Décider : public, ou manifests et image publiés ailleurs. `[install]`
- [ ] **Ordre d'installation qui casse en silence** : trust-manager installé avant que
  `mimic-system` existe n'obtient jamais le droit de lire la CA (Role absent) ; la ConfigMap
  `mimic-controller-ca` n'est jamais publiée et **chaque pod injecté boucle**
  (`/etc/mimic-ca/ca.crt: No such file or directory`). Reproduit ; corrigé en relançant
  `helm upgrade`. À détecter (`mimicctl check --pre`) et à éliminer (voir Q3). `[install]`
- [ ] **Plugin CNI lié à la glibc 2.34** mais exécuté par le runtime du nœud : échec de création
  de **tous** les pods du nœud sur Amazon Linux 2, RHEL 8, Ubuntu 20.04. Binaires statiques
  musl. `[image 1]`
- [ ] **Jeton du ServiceAccount CNI lisible par tous sur le nœud** (`0644` dans
  `/etc/cni/net.d/mimic-cni.d/`) : lecture de tous les pods du cluster. `0600`, répertoire
  `0700`. `[plan 2]` `[image 6]`

## P0 — le sidecar peut nuire à l'application

- [ ] **Services de `127.0.0.1` exposés au réseau** : un port refusé sur l'IP du pod est relayé
  vers `127.0.0.1` (`net.rs:63-80`, PRX-09). Un sidecar de base de données, un pprof ou un
  actuator, volontairement en loopback, devient joignable par tout pod, et voit la connexion
  venir de `127.0.0.1`. Istio a retiré ce comportement en 1.10. Spec §3.1 et PRX-09 d'abord.
  `[sidecar 1]`
- [ ] **Déni de service par connexions inactives** : un seul plafond de 2048 pour l'entrant et
  le sortant, aucune limite d'inactivité en h2c, TCP muet ni tunnel. Un client distant épuise
  le plafond ; tout le sortant de l'application et les sondes du kubelet sont refusés, alors
  que `/healthz` du sidecar reste vert. Reproduit. Plafonds séparés, délais d'inactivité,
  durée de vie maximale. `[sidecar 2]`
- [ ] **Sidecar arrêté = application redémarrée** : les règles de redirection restent quand le
  sidecar est OOM ou redémarre ; les sondes du kubelet vers l'application échouent et le
  kubelet la tue. Exclure les ports de sonde (le webhook les connaît) ou les réécrire comme
  Istio ; documenter le comportement fail-closed. `[sidecar 3]`

## P1 — sécurité avant production

- [ ] Redaction : le **chemin** n'est jamais redacté (`/bot<token>/…`, webhooks Slack, liens de
  réinitialisation) ; la détection de valeurs (JWT, e-mail, carte) ne s'applique pas aux
  paramètres ni aux headers ; ajouter `signature`, `sig`, `code`, `jwt`, `assertion`,
  `credential`, `auth` aux noms ; marqueurs de 32 bits. `[sidecar 5]`
- [ ] Inondation entrante qui efface les enregistrements sortants (file d'éviction commune) et
  les métriques par authority (`Host` choisi par le client) : budgets par sens. `[sidecar 4]`
- [ ] Partage direct par stockage objet : segments de pairs importés sans re-redaction ni limite
  de taille (empoisonnement du mock, OOM). Limiter, re-redacter, ou réserver au contrôleur.
  `[sidecar 6]`
- [ ] API agrégée : `allowed_names` vide accepte tout certificat de la CA fournie, puis fait
  confiance aux `X-Remote-*` ; refuser la configuration. `[plan 3]`
- [ ] Sel de redaction en clair sur le support (PVC, S3) : Secret Kubernetes, ou chiffrement
  documenté comme frontière de confiance. `[plan 4]`
- [ ] Isolation réelle **par namespace**, pas par workload (un pod peut se déclarer d'un autre
  workload du namespace) : corriger la spec §7.1 ou lier l'appartenance au `selector` de la
  `MimicWorkload`. `[plan 5]`
- [ ] Page d'admin : pas de contrôle de `Host` (rebinding DNS pendant un port-forward) ;
  annotation de mesh pour sortir 15100 de Linkerd ; jeton pour les routes d'écriture.
  `[sidecar 8]`
- [ ] Application : perte de l'IP cliente (pas de `X-Forwarded-For`) ; magasin de confiance de
  l'image applicative remplacé quand `tls-intercept` est posé (CA internes perdues).
  `[sidecar 7]`
- [ ] Petits : boucle du proxy sortant vers lui-même ; conteneur applicatif en UID 1338 hors
  interception ; IPv6 et UDP (QUIC) non interceptés ; lecture non bornée dans le filtre de
  l'API agrégée. `[sidecar 9, 10]` `[plan 6]`

## P1 — image et chaîne de publication

- [ ] Chart Helm (OCI), en plus de `mimicctl install` (décision du 25 octobre).
- [ ] Documentation des consommateurs en anglais : README, guide.

- [ ] Deux images : `runtime` sur distroless static (sans shell, ni apt, ni perl) et `init`
  (iptables) sur `debian:trixie-slim` ; images de base épinglées par digest ; bookworm est en
  fin de vie. Proposition construite et vérifiée :
  [docs/revue/Dockerfile.propose](docs/revue/Dockerfile.propose) (278 Mo → 119 Mo). Demande
  `init_image` dans la configuration du webhook (spec §9.4). `[image 2]`
- [ ] Construction reproductible : `rust-toolchain.toml` en `stable` fait télécharger la
  dernière version **pendant** le build ; épingler, caches BuildKit, binaires strippés.
  `[image 4]`
- [ ] Signature cosign, SBOM, provenance, scan trivy bloquant, `cargo deny`, actions épinglées
  par SHA, images référencées par digest dans les manifests. `[image 3, 8, 9]`
- [ ] DaemonSet CNI non privilégié (root sans capacités suffit) dans son propre namespace ;
  `mimic-system` en Pod Security `restricted`. `[image 5, 7]`
- [ ] Contrôleur : `startupProbe`, `timeoutSeconds`, `fsGroupChangePolicy: OnRootMismatch`
  sur RWX ; NetworkPolicy du webhook ; `runAsGroup` du Job de charge. `[image 10-12]`

## P1 — documentation et exploitation (PEX)

- [ ] **126 messages d'erreur sur 251 non documentés** (contrôleur 67, sidecar 42, webhook 13,
  mimic-init 4). Une page de dépannage par composant, une ligne par message : cause, effet,
  correction. Les dix premiers à traiter sont en tête de l'inventaire (réplique active non
  marquée, snapshot au-delà de `max_snapshot_bytes`, sel absent, `desired.json` illisible…).
  Un test DOC qui vérifie que chaque message du code figure dans la page. `[inventaire]`
- [ ] Cinq erreurs de la documentation actuelle (code de sortie faux, messages tronqués,
  contradiction sur la purge des snapshots). `[inventaire §6]`
- [ ] **README pour les consommateurs** : installation, `mimicctl install | kubectl apply -f -`,
  `mimicctl check`, une ligne pour activer un namespace ou un workload ; aucune commande
  `cargo` ; le reste dans `docs/guide/`. Spec v0.31 rédigée, branche `feat/install`
  (non poussée). Bloqué par la release (P0).
- [ ] La CA réémise juste après sa création (feuille signée par l'ancienne CA) se corrige à la
  main aujourd'hui : à corriger dans le produit, pas dans la doc.

## P2 — produit : simplifier

- [ ] Trois mécanismes de partage (pairs, stockage objet direct, contrôleur) : garder le
  contrôleur, supprimer les deux autres ?
- [ ] Deux outils de charge (`mimicctl load` et génération k6) : en garder un ?
- [ ] Trois accès aux données (API agrégée, port-forward, interface web) et trois stockages
  (RWX, RWO, S3) : réduire.
- [ ] Trois service mesh supportés et deux modes d'interception (init, CNI) : chaque
  combinaison est à maintenir et à tester.
- [ ] La spec (1 700 lignes, 290 critères) sert aussi de journal des modifications ; le README,
  de tableau d'avancement. Un `CHANGELOG.md`, une spec qui décrit l'état courant.

## P3 — plus tard

- [ ] Validation sur un vrai cluster géré (EKS ou GKE, plusieurs nœuds), chiffres de surcoût du
  sidecar en release (latence, CPU, mémoire) publiés.
- [ ] Mise à jour et retour arrière testés (versions mêlées, CRD `v1alpha1` sans conversion).

## Décisions (25 octobre 2026)

- **Versions** : base kind actuelle (Kubernetes 1.32), puis compatibilité vérifiée version par
  version jusqu'à la dernière (1.37). Matrice de test : voir « Tests de versions » ci-dessous.
- **Dépôt** : public à terme ; privé pendant le développement (choix du passage en public en
  attente, voir avantages dans la discussion).
- **cert-manager et trust-manager** restent des prérequis : commandes d'installation dans la
  partie installation, **namespace `mimic-system` créé avant trust-manager**.
- **Sidecar arrêté** : le trafic doit aller vers le vrai backend si possible (faisabilité avec
  iptables et les mesh à étudier, voir discussion).
- **Installation** : `mimicctl install` **et** un chart Helm.
- **Isolation** : par namespace, suffisante. Corriger la spec §7.1 en conséquence.
- **Documentation** : en anglais.
- **mimicctl** : distribué en binaires sur les releases GitHub (aucun publié à ce jour).

## Tests de versions

- [ ] e2e kind en matrice : 1.32 (base), puis chaque version mineure suivante disponible en
  image `kindest/node` (1.33 à 1.37 aujourd'hui), avec la version de kind qui la publie.
- [ ] Contrôle statique des manifests contre chaque version (API retirées ou dépréciées).
- [ ] Job planifié chaque semaine sur la dernière image kind publiée : une nouvelle version
  casse le job avant de casser un utilisateur.
- [ ] Version minimale déclarée (aujourd'hui 1.29) testée, ou relevée à 1.32.

## Questions restantes

1. Passer le dépôt en public maintenant ?
2. Repli vers `127.0.0.1` (Q4) : le retirer, avec une annotation pour les applications qui en
   ont besoin ?
3. Découverte d'API (Q6) : produit à part entière ou bonus ?

## Questions posées le 25 octobre (historique)

1. **Qui installe, et sur quoi ?** Clusters gérés (lesquels), versions, OS des nœuds, Pod
   Security en place : décide CNI ou init, musl, images.
2. **Public ou privé ?** Dépôt, image et binaires : qui doit pouvoir les télécharger ?
3. **Garder cert-manager et trust-manager comme prérequis**, ou le contrôleur gère sa CA et le
   webhook monte la CA dans les pods (un prérequis et un piège d'ordre en moins) ?
4. **Le loopback est-il une fonctionnalité ?** Retirer le repli vers `127.0.0.1` casse les
   applications qui n'écoutent qu'en local ; on les déclare (annotation) ou on les refuse ?
5. **Fail-open ou fail-closed** quand le sidecar tombe : l'application continue sans mimic
   (trafic réel vers les dépendances, même en mock), ou s'arrête ?
6. **Périmètre** : la découverte d'API (OpenAPI, diff, rapport) est-elle un produit à part
   entière, ou un bonus du mock pour tirs de charge ? Quelles fonctionnalités P2 supprimer ?
7. **Helm ou `mimicctl install`** (ou les deux) pour les équipes plateforme ?
8. **Isolation par namespace** suffit-elle, ou faut-il garantir l'isolation par workload ?
9. **Langue** de la documentation des consommateurs : français, anglais, les deux ?
