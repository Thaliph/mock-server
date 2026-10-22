# Service mesh

[← Sommaire](README.md)

mimic cohabite avec Istio (mode sidecar et mode ambient) et Linkerd. Dans les trois cas, il se
place **entre l'application et le mesh** : il voit le HTTP en clair, avant le mTLS.

```
sortant :  application ──► mimic ──► mesh (mTLS) ──► dépendance
entrant :  client ──► mesh (retire le mTLS) ──► mimic ──► application
```

En `mock`, une requête sortante s'arrête à mimic : le mesh n'en voit rien. Sources :
`crates/mimic-init/src/lib.rs`, `crates/mimic-injector/src/inject.rs`, spec §3.3. Testé par
`scripts/e2e-mesh.sh` (Istio 1.30.6 profil ambient, un namespace en sidecar et un en ambient,
`PeerAuthentication` `STRICT` ; Linkerd `edge-26.10.1`, politique `all-authenticated`).

## Ce qu'il faut faire

| Étape | Istio sidecar | Istio ambient | Linkerd |
|-------|---------------|---------------|---------|
| namespace | `istio-injection=enabled` (ou révision) **et** `mimic.platform.io/enabled=true` | `istio.io/dataplane-mode=ambient` **et** `mimic.platform.io/enabled=true` | annotation `linkerd.io/inject=enabled` **et** label `mimic.platform.io/enabled=true` |
| annotation `mimic.platform.io/mesh` | facultative (déduite des conteneurs `istio-*`) | **recommandée** : `istio-ambient` (aucun conteneur du mesh dans le pod, sinon détection par les règles) | facultative (déduite des conteneurs `linkerd-*`) |
| pods existants | `kubectl rollout restart` après l'activation du mesh **ou** de mimic | idem : un pod passé en ambient après son démarrage garde ses anciennes règles | idem |

Rien d'autre : ports, UID et ordre des init containers sont gérés par l'injecteur et
`mimic-init`.

## Ports et UID

Choisis hors de ceux des mesh, mesh ou non :

| | mimic | Istio | Linkerd |
|-|-------|-------|---------|
| UID du proxy | **1338** (GID 1338) | 1337 | 2102 |
| ports | **15100** (admin), **15101** (sortant), **15106** (entrant) | 15000, 15001, 15006, 15008, 15020, 15021, 15053, 15090 ; ztunnel 15001, 15006, 15008 dans le réseau du pod | 4140, 4143, 4190, 4191 |

## Ordre des init containers

Le webhook mimic place `mimic-init` puis le sidecar `mimic` **juste après le dernier conteneur
du mesh** (`istio-init`, `istio-validation`, `istio-proxy`, `linkerd-init`,
`linkerd-network-validator`, `linkerd-proxy`) parmi les init containers, sinon en tête. Si
le webhook du mesh passe **après** celui de mimic, la réinvocation
(`reinvocationPolicy: IfNeeded`) déplace les conteneurs mimic derrière ceux du mesh, pose
`MIMIC_MESH` et transforme les sondes en `exec`.

Le plugin CNI d'Istio (sidecar ou ambient) pose ses règles à la création du pod, avant tout
init container ; `istio-init` et `linkerd-init` passent avant `mimic-init`. Les règles du mesh
sont donc présentes quand `mimic-init` les lit.

## Règles iptables dans un pod du mesh

`mimic-init` lit les règles existantes (`iptables-save -t nat`) avec le binaire configuré, puis
`iptables-nft`, puis `iptables-legacy`, et pose les siennes **dans la même table** que le mesh.
Hors mesh (`MIMIC_MESH=none`, ou rien détecté), règles classiques : `PREROUTING` →
`MIMIC_INBOUND`, `OUTPUT` → `MIMIC_OUTBOUND`.

Dans un mesh :

- **aucune règle `PREROUTING`** : l'entrant arrive d'abord au mesh, qui retire le mTLS ;
- `MIMIC_OUTBOUND` est sautée **en tête** de `OUTPUT` (`-I OUTPUT 1`), avant les chaînes du
  mesh, et contient dans l'ordre :
  1. le **trajet entrant** (connexion du mesh vers l'application, en local `-o lo`, sur les
     ports entrants interceptés) redirigé vers 15106 ;
  2. le trafic de mimic (UID 1338) : `RETURN`, il continue vers les chaînes du mesh, qui
     l'envoient à leur proxy comme celui de l'application ;
  3. le trafic propre du mesh : `RETURN` ;
  4. les règles sortantes : IP du pod par `lo`, CIDR et ports exclus, puis redirection vers 15101.

| Mesh | Détecté par (table `nat`) | Trajet entrant reconnu par | Trafic du mesh reconnu par |
|------|---------------------------|----------------------------|----------------------------|
| Istio sidecar | `ISTIO_OUTPUT` sans `ISTIO_PRERT` | source `127.0.0.6/32` (`::6/128` en IPv6) | UID et GID du proxy (`--uid-owner`/`--gid-owner` de `ISTIO_OUTPUT`, 1337 par défaut) |
| Istio ambient | `ISTIO_PRERT` | marque de ztunnel (`--mark` de `ISTIO_OUTPUT`, `0x539/0xfff` par défaut) ; envoyé par `DNAT` vers `POD_IP:15106` (ztunnel garde l'adresse du client), redirection simple sans `POD_IP` | la même marque |
| Linkerd | `PROXY_INIT_OUTPUT` | UID du proxy (`--uid-owner` de `PROXY_INIT_OUTPUT`, 2102 par défaut) | le même UID |

Une valeur explicite de `MIMIC_MESH` (`istio`, `istio-ambient`, `linkerd`) garde les valeurs
lues si les règles trouvées sont celles de ce mesh, sinon prend les valeurs par défaut du
tableau. Réappliquer les règles laisse un seul saut vers `MIMIC_OUTBOUND`, toujours en tête.

### Vérifier

```sh
kubectl -n shop logs <pod> -c mimic-init
```

La première ligne donne le mesh retenu et le binaire (celui dont la table contient les règles
du mesh : `iptables`, `iptables-nft` ou `iptables-legacy`) :

```
mimic-init: mesh IstioSidecar { proxy_uid: 1337, proxy_gid: 1337, inbound_source: "127.0.0.6/32" }, binaire iptables
mimic-init: mesh IstioAmbient { mark: "0x539/0xfff" }, binaire iptables
mimic-init: mesh Linkerd { proxy_uid: 2102 }, binaire iptables
mimic-init: mesh None, binaire iptables
```

suivie de `mimic-init: <binaire>-restore --noflush`, des règles posées, puis
`mimic-init: règles <binaire> posées`. Un mesh présent mais logué `None` : les règles du mesh
n'existaient pas encore (ordre des init containers, CNI), ou `MIMIC_MESH=none`.

## Sondes

Avec mTLS strict (`PeerAuthentication` `STRICT`, Linkerd `all-authenticated`), le mesh
refuserait les sondes HTTP du kubelet vers 15100. Quand le webhook voit des conteneurs du mesh
dans le pod, les sondes du sidecar deviennent `exec` : `mimic-proxy --probe /readyz`
(démarrage et disponibilité) et `mimic-proxy --probe /healthz` (vivacité), qui interrogent
l'API admin depuis le conteneur. En ambient, aucun conteneur du mesh n'est dans le pod et le
kubelet est reconnu par ztunnel : les sondes restent HTTP.

## Effets et limites

| Sujet | Comportement |
|-------|--------------|
| connexions vers le contrôleur | sortent par le mesh comme toute connexion du pod ; `mimic-system` étant hors mesh, elles sont relayées telles quelles, chiffrées par mimic. Sous une politique de sortie Istio `REGISTRY_ONLY`, le Service du contrôleur est dans le registre |
| routage L7 du mesh | `VirtualService`, `HTTPRoute`, retries et timeouts s'appliquent à la requête relayée par mimic comme à celle de l'application ; en `mock`, ils ne s'appliquent pas (rien ne sort) |
| waypoints ambient | hors du pod, sans effet sur mimic |
| métriques | celles du mesh voient le trafic relayé ; en `mock`, le trafic sortant mocké disparaît des métriques du mesh |
| collecte des métriques de mimic | le scraper doit joindre 15100 à travers le mesh : en mTLS strict, il doit être dans le mesh, ou le port exclu côté mesh (par exemple `traffic.sidecar.istio.io/excludeInboundPorts: "15100"` pour Istio sidecar) |
| autres mesh (Consul, Kuma, Cilium) | non pris en charge ; `MIMIC_MESH=none` pose les règles classiques, sans garantie d'ordre avec les leurs |
| IPv6 | le webhook ne pose pas `MIMIC_IPV6` : seul l'IPv4 est intercepté |

Diagnostic des cas fréquents : [Dépannage](Depannage.md#service-mesh).
