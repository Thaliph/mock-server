**mimic** est un sidecar Kubernetes qui observe le trafic HTTP d'un pod, en déduit l'API
(OpenAPI 3.1, avec exemples et latences), puis remplace les dépendances du pod par leurs
réponses enregistrées pour lancer des tirs de performance isolés. Le service mesuré reçoit
toujours son vrai trafic entrant ; ce sont ses appels sortants (SaaS, autres services) qui sont
rejoués.

Ce wiki est la documentation d'exploitation. La spécification qui fait foi est
`docs/SPEC.md` ; en cas d'écart, le comportement décrit ici est celui du code (les écarts
connus sont signalés dans les pages concernées).

## À qui il sert

| Public | Ce qu'il y trouve |
|--------|-------------------|
| Équipes SRE / performance | découverte de l'API d'un service et de ses dépendances sans documentation préalable ; tir de charge d'un pod isolé de ses dépendances (quotas SaaS, environnements hors production) |
| Équipes plateforme | installation du contrôleur et du webhook, supervision, mises à jour, sécurité |
| Équipes applicatives | liste des appels réellement faits et reçus, documents OpenAPI exportables, diff d'API d'une semaine à l'autre |

mimic est conçu pour les environnements **hors production** : les modes `mock` et
`mock-partial` sont refusés dans un namespace labellisé `env=production` (voir
[Basculer en mock](Basculer-en-mock)).

## Composants en une phrase

| Composant | Rôle |
|-----------|------|
| `mimic-proxy` (conteneur `mimic`) | sidecar : proxy transparent, enregistrement, mock, page d'admin, métriques |
| `mimic-init` | init container qui pose les règles iptables (y compris dans Istio ou Linkerd) |
| `mimic-controller` | plan de contrôle : agrégation par workload, snapshots, bascule, interface web |
| `mimic-injector` | webhook d'admission qui ajoute `mimic-init` et le sidecar aux pods |
| `MimicWorkload` | ressource Kubernetes (CRD) qui déclare le mode souhaité d'un workload |
| `mimicctl` | CLI : bascule, statut, export OpenAPI, rejeu k6, rapport, diff |

## Plan du wiki

| Page | Contenu |
|------|---------|
| [Architecture](Architecture) | composants, flux entrant, sortant, contrôleur et snapshots, ports, schéma |
| [Installer](Installer) | prérequis, ordre d'installation, image, activation de l'injection, vérification, désinstallation |
| [Configurer](Configurer) | sidecar, annotations, webhook, contrôleur, `mimic-init`, CRD `MimicWorkload` champ par champ |
| [Modes](Modes) | `passthrough`, `record`, `mock`, `mock-partial` : entrant, sortant, TLS, WebSocket, TCP |
| [Collecte et stockage](Collecte-et-stockage) | ce qui est capturé, redaction, plafonds, rétention, exemples par endpoint, snapshots, inférence |
| [Basculer en mock](Basculer-en-mock) | par `mimicctl` ou la CRD, sélection de pods, garde-fou production, alignement, retour |
| [Métriques](Metriques) | toutes les métriques du sidecar, du contrôleur et du webhook, PromQL utiles |
| [Alertes et supervision](Alertes-et-supervision) | sondes, signaux de panne et de dégradation, règles `PrometheusRule` et conduite à tenir |
| [Administration et API](Administration-et-API) | page d'admin du sidecar, interface et API du contrôleur, droits |
| [Service mesh](Service-mesh) | Istio sidecar, Istio ambient, Linkerd : ce qui change, limites |
| [CLI mimicctl](CLI-mimicctl) | installation, chaque commande et option, exemples |
| [Exploiter en production](Exploiter-en-production) | dimensionnement, haute disponibilité, mises à jour, certificats, pannes, sécurité, sauvegarde |
| [Dépannage](Depannage) | symptômes, diagnostics et messages d'erreur réels |
| [Tir de charge](Tir-de-charge) | renvoi vers le guide SRE `docs/sre-load-test.md` |

## Démarrage rapide

```sh
# 1. Socle (une fois par cluster) : voir Installer pour le détail
kubectl apply -f deploy/controller/namespace.yaml -f deploy/controller/crd.yaml -f deploy/controller/rbac.yaml
kubectl apply -f deploy/controller/certificates.yaml -f deploy/controller/controller.yaml
kubectl apply -f deploy/injector/injector.yaml

# 2. Activer un namespace, puis recréer ses pods
kubectl label namespace shop mimic.platform.io/enabled=true
kubectl -n shop rollout restart deploy/checkout

# 3. Laisser enregistrer, regarder ce qui a été découvert
mimicctl report checkout -n shop

# 4. Isoler le workload de ses dépendances, tirer, revenir
mimicctl mode checkout mock -n shop
mimicctl status checkout -n shop
mimicctl mode checkout record -n shop
```
