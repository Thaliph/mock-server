# Déploiement

`example/` montre un workload `checkout` avec mimic ajouté à la main :

| Fichier | Rôle |
|---------|------|
| `configmap.yaml` | configuration du sidecar (spec §9.1) |
| `deployment.yaml` | init container iptables, native sidecar, API downward pour la bascule de mode |
| `vmpodscrape.yaml` | collecte des métriques par VictoriaMetrics |
| `networkpolicy-mock.yaml` | à appliquer pendant un tir en mode mock pour garantir qu'aucun appel ne sort |

Prérequis : Kubernetes 1.29 ou plus, un Secret `checkout-mimic` avec la clé `redact-salt`,
et un bucket S3 (ou un PVC `ReadWriteMany` monté et `share.url: file:///chemin`) pour le
partage entre réplicas.

Passer un workload en mock :

```sh
kubectl annotate pod -l app.kubernetes.io/name=checkout mimic.platform.io/mode=mock --overwrite
kubectl apply -f example/networkpolicy-mock.yaml
```

Voir ce que la découverte a trouvé :

```sh
kubectl port-forward deploy/checkout 15000:15000
# puis http://127.0.0.1:15000/
```

Si les noeuds utilisent iptables en mode legacy, ajouter `MIMIC_IPTABLES=iptables-legacy`
à l'init container.

L'API admin permet de changer de mode et de lire les échanges (redactés) : limiter son
accès par une NetworkPolicy d'entrée, ou la lier à `127.0.0.1` et passer par
`kubectl port-forward`.
