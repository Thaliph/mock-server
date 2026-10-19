Le guide complet du tir de charge d'un workload isolé de ses dépendances est
**`docs/sre-load-test.md`** dans le dépôt : prérequis, préparation du rejeu, isolement,
tir, mesure, retour en `record`, et exemple blue/green avec Argo Rollouts.

> Cette page sera complétée par l'équipe SRE.

## Le parcours en bref

| Étape | Commande | Page de référence |
|-------|----------|-------------------|
| 1. préparer le rejeu | `mimicctl requests checkout -n perf --format k6 --limit 2000 > replay.js` | [CLI mimicctl](CLI-mimicctl#requests) |
| 2. isoler le workload | `mimicctl mode checkout mock --capture-inbound metadata -n perf` | [Basculer en mock](Basculer-en-mock) |
| 3. attendre l'alignement | `until mimicctl status checkout -n perf \| tail -1 \| grep -qE ' ([0-9]+)/\1$'; do sleep 2; done` | [Basculer en mock](Basculer-en-mock#vérifier-lalignement) |
| 4. tirer | `k6 run -e TARGET=http://checkout.perf:8080 -e VUS=20 -e ITERATIONS=5000 replay.js` | `docs/sre-load-test.md` |
| 5. mesurer | latences entrantes, `599`, CPU et mémoire | [Métriques](Metriques#requêtes-promql-utiles) |
| 6. revenir | `mimicctl mode checkout record -n perf` | [Basculer en mock](Basculer-en-mock#revenir) |

## À vérifier avant de tirer

- `ALIGNED` à `n/n` et `mimic_mock_snapshot_exchanges > 0` sur chaque pod.
- Taux de `599` (`mimic_mock_responses_total{match="unmatched"}`) proche de zéro.
- Les dépendances non HTTP (bases, files de messages) et les HTTPS non interceptés : les
  premières restent jointes réellement, les seconds sont coupés en `mock` (voir
  [Modes](Modes#trafic-sortant)).
- Pas de redéploiement ni d'autoscaling pendant le tir : un pod neuf démarre en `record`
  jusqu'à son premier battement (voir [Exploiter en production](Exploiter-en-production#pannes)).
- Pour rejouer exactement le même jeu au tir suivant : noter `status.snapshot` et le passer en
  `--snapshot` (voir [Basculer en mock](Basculer-en-mock#rejouer-exactement-le-même-jeu)).
