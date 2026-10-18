#!/usr/bin/env python3
"""Génère tests/e2e/otel-demo.yaml depuis le manifest Kubernetes de la démo OpenTelemetry.

La démo est gardée telle quelle, générateur de charge compris, sauf ce qu'un runner de CI
ne peut pas porter : la chaîne de télémétrie (collecteur, Jaeger, Prometheus, Grafana,
OpenSearch) et Kafka avec ses consommateurs. Les exports OTLP des services échouent sans
gêner ; checkout n'envoie plus de commandes à Kafka quand KAFKA_ADDR est absent.

Usage : python3 scripts/otel-demo.py [tag] > tests/e2e/otel-demo.yaml (PyYAML requis).
"""
import sys
import urllib.request

import yaml

TAG = sys.argv[1] if len(sys.argv) > 1 else "2.1.3"
NAMESPACE = "otel-demo"
URL = f"https://raw.githubusercontent.com/open-telemetry/opentelemetry-demo/{TAG}/kubernetes/opentelemetry-demo.yaml"

DROPPED = {
    "grafana", "jaeger", "jaeger-agent", "jaeger-collector", "jaeger-query", "otel-collector",
    "prometheus", "opensearch", "opensearch-headless", "opensearch-pdb", "opensearch-config",
    "grafana-dashboards", "grafana-clusterrole", "grafana-clusterrolebinding",
    "kafka", "accounting", "fraud-detection",
}
# Hors injection : le stockage du panier parle RESP, pas HTTP.
OPT_OUT = {"valkey-cart"}
# Deux réplicas : la bascule doit aligner les deux pods sur le même snapshot (spec §5.6).
REPLICAS = {"shipping": 2}
# Variables remplacées ou retirées (None), par composant.
ENV = {
    "checkout": {"KAFKA_ADDR": None},
    "load-generator": {"LOCUST_BROWSER_TRAFFIC_ENABLED": "false"},
    # Sans collecteur, l'export synchrone du SDK PHP ralentit chaque requête.
    "quote": {"OTEL_PHP_AUTOLOAD_ENABLED": "false"},
}


def keep(doc):
    name = doc["metadata"]["name"]
    if name in DROPPED or doc["kind"] in ("ClusterRole", "ClusterRoleBinding", "Role", "RoleBinding"):
        return False
    return not (doc["kind"] == "Secret" and name == "grafana")


def patch(doc):
    meta = doc["metadata"]
    if doc["kind"] == "Namespace":
        meta.setdefault("labels", {})["mimic.platform.io/enabled"] = "true"
    else:
        meta["namespace"] = NAMESPACE
    if doc["kind"] != "Deployment":
        return doc
    name = meta["name"]
    spec = doc["spec"]
    pod = spec["template"]
    if name in REPLICAS:
        spec["replicas"] = REPLICAS[name]
    if name in OPT_OUT:
        pod["metadata"].setdefault("annotations", {})["mimic.platform.io/inject"] = "false"
    pspec = pod["spec"]
    # L'interface des feature flags n'est pas exercée par le générateur de charge.
    pspec["containers"] = [c for c in pspec["containers"] if c["name"] != "flagd-ui"]
    inits = [c for c in pspec.get("initContainers") or [] if c["name"] != "wait-for-kafka"]
    if inits:
        pspec["initContainers"] = inits
    else:
        pspec.pop("initContainers", None)
    for c in pspec["containers"]:
        changes = ENV.get(name, {})
        env = [e for e in c.get("env", []) if changes.get(e["name"], "") is not None]
        for e in env:
            if e["name"] in changes:
                e["value"] = changes[e["name"]]
        c["env"] = env
        for key in ("volumeMounts",):
            if not c.get(key):
                c.pop(key, None)
    if not pspec.get("volumes"):
        pspec.pop("volumes", None)
    return doc


def main():
    text = urllib.request.urlopen(URL).read().decode()
    docs = [patch(d) for d in yaml.safe_load_all(text) if d and keep(d)]
    print(f"# Généré par scripts/otel-demo.py depuis la démo OpenTelemetry {TAG} :")
    print(f"# {URL}")
    print("# Ne pas modifier à la main : changer le script et régénérer.")
    for d in docs:
        print("---")
        print(yaml.safe_dump(d, sort_keys=False, allow_unicode=True, width=120), end="")


if __name__ == "__main__":
    main()
