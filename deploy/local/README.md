# Mock local (spec §9.7)

Lancer une application contre les dépendances enregistrées par mimic, sans cluster.

```sh
docker compose up
```

`app` appelle `http://quotes.internal:8080/quote?zip=75001` toutes les 5 s par le sidecar
(`HTTP_PROXY`). Ce nom n'existe pas ici : le sidecar répond `200` avec `{"price":42,…}` et
`x-mimic-match: exact`, depuis `snapshot.jsonl`. Un appel non enregistré reçoit `599`.

Avec le snapshot d'un vrai workload :

```sh
mimicctl pull checkout -n shop -o snapshot.jsonl     # snapshot servi, sinon le plus récent
docker compose up
```

Puis remplacer `app` par l'application, avec `HTTP_PROXY`/`HTTPS_PROXY` vers
`http://mimic:15101`. Une dépendance HTTPS doit être dans `tls.intercept` de `mimic.yaml`, et
l'application doit faire confiance à `ca.crt` publié dans `tls.ca_dir` (volume partagé).

Sans Docker : `mimic-proxy --config mimic.yaml --mock-from snapshot.jsonl`, puis
`HTTP_PROXY=http://127.0.0.1:15101` pour l'application. En CI, même chose dans le job.
