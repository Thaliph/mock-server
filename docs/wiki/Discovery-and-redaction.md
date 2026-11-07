# Discovery and redaction

Covers F4 and F5. Reference: spec §6, §7.3, §11. Discovery records **inbound** exchanges (the
endpoints a deployment exposes) and, from F7, **outbound** ones: see [Egress aggregation](Egress-aggregation).

## What is recorded

Every inbound HTTP/1 and HTTP/2 exchange that got a response head, except:

- probe requests (they never reach the proxy listener);
- `unframed` and `rejected` exchanges, and exchanges with no response head;
- paths over 2048 bytes or 32 segments (`invalid`);
- `404` and `405` answers that do not match an existing endpoint (counted on the matching endpoint if
  there is one, else dropped as `not_found`, so scanners cannot fill the report).

Inbound HTTPS is not decrypted, so it is not discovered. An upgraded exchange (WebSocket) is recorded
without bodies.

Per endpoint (key: direction, port, method, path template) the report keeps: status counters, a latency
histogram of the time to the response head, the `Host` values seen (at most 16), path parameters, query
keys, request and response headers (at most 64 each), request and response body schemas per media
type, volatile fields, and at most 20 redacted samples.

## Path templating

Paths are split on `/`. A segment is a **parameter** when it looks like an id: digits only, UUID, ULID,
hex of 8+ characters with a digit, an RFC 3339 date or date-time, 16+ characters with a letter and a
digit, a value caught by a redaction detector, or anything containing `@`. Other segments are
literals.

| Seen | Template |
|---|---|
| `/users/42/orders/7` | `/users/{users_id}/orders/{orders_id}` |
| `/42` | `/{param1}` |

A parameter is named after the previous literal segment (`{<literal>_id}`), or `{param<n>}`.
Below one node at most `MIMIC_DISCOVERY_MAX_LITERALS` (32) distinct literals are kept; one more makes
the node **absorbing**: its literals merge into one parameter. gRPC paths are never templated. The
result does not depend on the order in which paths were seen. The controller re-applies templating on
the union of all pods (`MIMIC_STORE_MAX_LITERALS`, 32).

## Schemas and volatile fields

Bodies of `application/json`, `*/*+json` and `application/x-www-form-urlencoded` get a schema: types
(`integer` when all numbers are integral), formats (`uuid`, `date-time`, `date`, `email`, `uri`,
`ipv4`, `ipv6`), properties, `required`, array `items`, `additionalProperties`. Any other media type
is opaque: media type and size only. `gzip` and `deflate` bodies are decoded; other
`content-encoding` values (including `br` and `zstd`: no Brotli decoder is linked) make the body
opaque. Schemas hold types and formats, never values.

A field (query key, header, JSON/form leaf) is **volatile** when it was seen at least 3 times and its
distinct-value count is at least half of that (idempotency keys, timestamps, ids). Redacted fields are
always marked `redacted`.

## Limits

| Limit | Value |
|---|---|
| Body parsed | up to 256 KiB (encoded and decoded); larger: media type and size only (`too_large`) |
| Schema size | 512 nodes, 16 levels; an object with more than 128 keys becomes a map |
| Names (query key, header, JSON field) | cut at 128 bytes |
| Query keys / headers per endpoint | 64 / 64 (first in byte order) |
| Volatile fields per endpoint | 128 |
| Samples per endpoint | 20 (deterministic selection: one per status, then one per class, then the rest) |
| Endpoints per sidecar and direction | `MIMIC_DISCOVERY_MAX_ENDPOINTS` (256) |
| Sample bytes per sidecar | `MIMIC_DISCOVERY_SAMPLE_BUDGET` (16Mi) |
| Body bytes in flight | `MIMIC_DISCOVERY_CAPTURE_BUDGET` (8Mi) |
| Records waiting for the recorder | `MIMIC_DISCOVERY_QUEUE` (1024) |
| Endpoints per direction in one push / in the store | 256 / 512 (`MIMIC_STORE_MAX_PUSH_ENDPOINTS`, `MIMIC_STORE_MAX_ENDPOINTS`) |
| Sample bytes per deployment in the store | `MIMIC_STORE_SAMPLE_BUDGET` (4Mi) |
| Deployments with data per namespace | `MIMIC_STORE_MAX_DEPLOYMENTS` (50) |

Discovery is **best effort under load**: a full queue, capture budget, sample budget or the endpoint
limit make counters and samples partial (counted in `mimic_discovery_*` metrics, see
[Operations](Operations)), and never slow or refuse traffic. At a store limit the controller keeps the
endpoints with the most traffic and refuses new samples unless their endpoint has none of that status
class.

## Redaction

Applied in the sidecar before anything is stored, logged or sent, and **again** in the controller
before anything reaches Valkey. The marker is the constant `<redacted>` (no hash of a secret is kept).

**By name** (header name, query key, form field, JSON key). Names are split into tokens (at `-`, `_`,
`.`, case changes, letters/digits) and compared in lower case. Built-in names: `authorization`,
`proxy-authorization`, `cookie`, `set-cookie`, `password`, `passwd`, `secret`, `token`, `apikey`,
`auth`, `session`, `signature`, `credential`, `privatekey`, `cvv`, `cvc`, `cardnumber`, `pan`, `iban`,
`ssn` (plural `s` accepted). `auth`, `pan`, `iban`, `ssn`, `cvv`, `cvc` match whole tokens only; the
others also match inside a token (`JSESSIONID`, `csrftoken`, `clientsecret`). So `X-Api-Key`,
`auth_token`, `client_secret`, `Session-Id` are redacted; `company`, `author`, `panel` are not. The
whole value (a JSON subtree included) becomes the marker, except `Authorization` keeps its scheme
(`Bearer <redacted>`), `Cookie` keeps cookie names, `Set-Cookie` keeps name and attributes.

**By value** (every other string and number): JWTs, PEM blocks, `Bearer`/`Basic` credentials, 13-19
digit card numbers passing Luhn, AWS access key ids (`AKIA`/`ASIA`), tokens with a known prefix
(`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`, `glpat-`, `xox[abprs]-`, `sk_live_`, `rk_live_`,
`sk-`), and a URL's userinfo password. Path segments and query values are percent-decoded for the
detectors. URLs in header values (`Location`, `Referer`, `Link`, ...) are redacted like path plus
query. The access-log `path` goes through the value detectors too.

### Adding names: `spec.discovery.redact`

```yaml
spec:
  deployment: users
  discovery:
    redact: [x-tenant-key, badge, pin]
```

Up to 64 names of `A-Za-z0-9_-`, compared like the built-in ones (case-insensitive, whole tokens,
`-` and `_` ignored). The webhook gives the sidecar the sorted union over every `MimicWorkload`
naming the deployment (`MIMIC_DISCOVERY_REDACT_NAMES`); it applies to pods created afterwards
(`kubectl rollout restart`), but the controller re-redacts every push with the current list, so a name
added later applies to stored data from the next push.

### What is never stored

- Opaque bodies (only media type and size).
- Header values, queries, bodies or samples in logs: no log line holds them.
- A hash of a redacted value.
- Keys, seed and tokens.
- Query strings in the access log.

### What can still be stored

A secret under an unlisted name **and** in a form no detector recognises (for example a random API key
in a custom header) is kept in samples. Personal data that is not a credential (names, e-mail
addresses, amounts) is kept in samples by design; the `@` rule only keeps it out of path templates. Add
such names to `spec.discovery.redact`, and use an encrypted StorageClass for the Valkey PVC. See
[Security](Security).
