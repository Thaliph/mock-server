# TLS interception

Only **outbound** TLS is intercepted. Inbound TLS is never
intercepted (the sidecar has no key of your app) and is seen at connection level only: SNI, bytes,
duration. Inbound HTTPS is therefore not discovered.

## How it works

For an outbound HTTPS connection the sidecar:

1. reads the ClientHello (SNI, versions, ALPN);
2. opens its own TLS connection to the real destination **first**, verifying its certificate;
3. answers the app with a certificate it issues for the requested name;
4. relays the decrypted stream like cleartext (HTTP/1 byte-exact, HTTP/2, or an opaque tunnel).

Whenever interception could change what the app or the server sees, the connection is **passed
through** untouched (the app sees the server's own certificate). mimic never vouches for a
certificate it could not verify.

## Certificates and custody

- The controller keeps a random 32-byte **seed** in the Secret `<release>-tls-ca`. It never leaves the
  controller.
- Each deployment has its own **root CA** derived from the seed (ECDSA P-256,
  `CN=mimic <namespace>/<deployment>`, valid 10 years). Its key exists only in the controller's
  memory; the root certificate is public and is written into the pod spec.
- Each sidecar generates an **intermediate** key in memory and asks the controller to sign it
  (`POST /v1/tls/intermediate`). Validity 25 h, renewed when less than half remains. The sidecar
  accepts it only if it chains to the root it was given.
- **Leaf** certificates are issued in memory per SNI (or per destination IP without SNI) and cached.
- No key, seed or token is logged, written to disk or sent to Valkey; the sidecar disables core dumps.

## Trust injection (no code change)

At pod creation the webhook adds one `mimic-trust-<n>` init container per distinct app image. It runs
mimic's static `mimic-trust` binary inside your image to build, from **that image's own trust store**
plus the deployment root, a bundle on a shared volume (`mimic-trust`, `emptyDir` 16Mi). Internal CAs
installed in your image keep working. App containers get the bundle mounted at `/etc/mimic/tls` and,
unless their `env` already defines the variable:

| Variable | Value | Used by |
|---|---|---|
| `SSL_CERT_FILE` | `/etc/mimic/tls/ca-bundle.pem` | OpenSSL, Go, Ruby, .NET |
| `REQUESTS_CA_BUNDLE` | same | Python `requests` |
| `CURL_CA_BUNDLE` | same | curl |
| `NODE_EXTRA_CA_CERTS` | `/etc/mimic/tls/node-extra-ca.pem` | Node.js |
| `JAVA_TOOL_OPTIONS` | `-Djavax.net.ssl.trustStore=/etc/mimic/tls/truststore.p12 -Djavax.net.ssl.trustStoreType=PKCS12 -Djavax.net.ssl.trustStorePassword=mimic` | JVM |

A `JAVA_TOOL_OPTIONS` with a literal `value` gets these options prepended (yours come later and win);
one defined with `valueFrom` is left alone. The truststore password `mimic` is public and protects
nothing secret.

## Excluding hosts

List names that must never be intercepted (internal services, mTLS to a provider) on the
`MimicWorkload`:

```yaml
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: payments, namespace: shop}
spec:
  deployment: payments
  tls:
    exclude:
      - api.example.com        # exact host
      - "*.internal.example"   # any name below, not internal.example itself
      - 10.0.0.12              # IP literal, matched when the ClientHello has no SNI
```

At most 256 entries; compared case-insensitively without trailing dot. The webhook gives the sidecar
the union of all `MimicWorkload`s naming the deployment in `MIMIC_TLS_EXCLUDE`. It applies to pods
created afterwards: `kubectl -n shop rollout restart deploy/payments`.

## Pass-through cases

Decision order for an outbound ClientHello (first match wins). The decision is the `interception`
label of `mimic_tls_connections_total` and the `interception` field of the connection's access line.

| `interception` | When | Remembered |
|---|---|---|
| `unsupported` | SNI is not a valid DNS name; no TLS 1.2/1.3 offered; or ALPN offered without `h2` or `http/1.1` | no |
| `excluded` | SNI (or destination IP) matches `spec.tls.exclude` | |
| `no_ca` | the sidecar has no valid intermediate (none yet, expired, mismatch, trust volume not published) | no |
| `upstream_untrusted` | the sidecar's own handshake with the destination fails (unknown issuer, expired, wrong name, alert) | yes |
| `client_cert_requested` | the destination asks for a client certificate (mTLS) | yes |
| `client_rejected` | the app answers mimic's certificate with an alert or closes right after (pinning, compiled-in roots, ECH) | yes |
| `intercepted` | otherwise | |

"Remembered": in memory, per (SNI or IP, port), at most 4096, lost on sidecar restart. The first time
a destination is remembered the sidecar logs one WARN `tls passthrough` with `sni`, `dst`, `reason`.

Consequences to know:

- The **first** connection to a destination whose client rejects mimic's certificate fails (the client
  gave up on it); later ones pass through. Add such destinations to `spec.tls.exclude` to avoid it.
- A destination signed by a CA that none of your images trust is passed through, so the app judges it
  itself.
- Not intercepted: TLS inside an HTTP `CONNECT` tunnel, STARTTLS, Encrypted Client Hello.
- A container whose spec already defines a trust variable keeps its own value. Variables set in the
  image `ENV` are not seen by the webhook (an image-level `JAVA_TOOL_OPTIONS` is masked by mimic's;
  define it in the pod spec to keep it).
- Without a valid intermediate every outbound TLS connection is passed through (`no_ca`); the sidecar
  stays ready.

## Logs and metrics

| Signal | Meaning |
|---|---|
| WARN `tls passthrough` (`target` `tls`; `sni`, `dst`, `reason`) | A destination is now passed through. |
| ERROR `tls ca mismatch` | The intermediate does not chain to `MIMIC_TLS_CA`; no interception. |
| ERROR `trust volume not published` | The sidecar could not write the trust volume; no interception. |
| WARN `tls roots skipped` (`path`, `skipped`) | An unreadable or unparsable upstream roots file or block was skipped. |
| WARN `intermediate request failed` | The sidecar could not get its intermediate; it retries with backoff 1 s up to 60 s. |
| WARN `no image trust store` (`mimic-trust`) | The image has no trust store; the fallback store was used. |
| `mimic_tls_connections_total{interception}` | Connections by decision. |
| `mimic_tls_passthrough_destinations` | Remembered destinations. |
| `mimic_tls_intermediate_expiry_timestamp_seconds` | `notAfter` of the intermediate, 0 when none. |
| `mimic_tls_intermediate_requests_total{result}` | `ok`, `error`, `mismatch`. |
