//! Keeps the webhook certificates (spec §7.1): reads the Secret, issues what `certs::decide`
//! asks for, loads the serving certificate and writes the CA into the `caBundle` of the
//! `MutatingWebhookConfiguration`. Replicas converge through the `resourceVersion` of the Secret.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use futures::StreamExt;
use k8s_openapi::ByteString;
use k8s_openapi::api::admissionregistration::v1::MutatingWebhookConfiguration;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher::{self, Event};
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use serde_json::json;
use tokio::sync::Notify;

use crate::certs::{self, Decision, Stored};
use crate::tls::CertSlot;

/// How often the Secret is looked at again once everything is in place.
const RECHECK: Duration = Duration::from_secs(600);
const RETRY: Duration = Duration::from_secs(2);

pub struct CertMetrics {
    expiry: Gauge,
}

impl CertMetrics {
    pub fn register(registry: &mut Registry) -> CertMetrics {
        let metrics = CertMetrics {
            expiry: Gauge::default(),
        };
        registry.register(
            "webhook_certificate_expiry_timestamp_seconds",
            "Expiry of the webhook serving certificate, in seconds since the Unix epoch",
            metrics.expiry.clone(),
        );
        metrics
    }
}

pub struct CertConfig {
    pub namespace: String,
    pub service: String,
    pub secret: String,
    pub configuration: String,
}

impl CertConfig {
    fn dns_name(&self) -> String {
        format!("{}.{}.svc", self.service, self.namespace)
    }
}

/// Runs for ever: every pass makes the Secret, the loaded certificate and the `caBundle` agree.
pub async fn manage(
    client: kube::Client,
    config: CertConfig,
    slot: Arc<CertSlot>,
    metrics: CertMetrics,
) {
    let secrets = Api::<Secret>::namespaced(client.clone(), &config.namespace);
    let configurations = Api::<MutatingWebhookConfiguration>::all(client);
    // Any change of the configuration, made by anyone, triggers a pass at once (INJ-20).
    let changed = Arc::new(Notify::new());
    tokio::spawn(watch_configuration(
        configurations.clone(),
        config.configuration.clone(),
        changed.clone(),
    ));
    loop {
        let wait = match reconcile(&secrets, &configurations, &config, &slot, &metrics).await {
            Ok(()) => RECHECK,
            Err(e) => {
                tracing::warn!(error = %e, "webhook certificate not in place yet");
                RETRY
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = changed.notified() => {}
        }
    }
}

/// Wakes `changed` each time the configuration `name` is seen, created or modified.
async fn watch_configuration(
    configurations: Api<MutatingWebhookConfiguration>,
    name: String,
    changed: Arc<Notify>,
) {
    let config = watcher::Config::default().fields(&format!("metadata.name={name}"));
    let mut events = std::pin::pin!(watcher::watcher(configurations, config).default_backoff());
    while let Some(event) = events.next().await {
        match event {
            Ok(Event::Apply(_) | Event::InitApply(_)) => changed.notify_one(),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "webhook configuration watch error"),
        }
    }
}

async fn reconcile(
    secrets: &Api<Secret>,
    configurations: &Api<MutatingWebhookConfiguration>,
    config: &CertConfig,
    slot: &CertSlot,
    metrics: &CertMetrics,
) -> Result<(), String> {
    let mut secret = secrets
        .get_opt(&config.secret)
        .await
        .map_err(|e| format!("read secret: {e}"))?
        .ok_or_else(|| format!("secret {} does not exist", config.secret))?;
    let mut stored = from_secret(&secret);
    let now = SystemTime::now();
    let decision = certs::decide(&stored, &config.dns_name(), now);
    if decision != Decision::Keep {
        stored = certs::issue(&stored, decision, &config.dns_name(), now)?;
        secret.data = Some(to_data(&stored));
        // The resourceVersion read above makes a concurrent writer win: we then start over.
        secrets
            .replace(&config.secret, &PostParams::default(), &secret)
            .await
            .map_err(|e| format!("write secret: {e}"))?;
        tracing::info!(?decision, "webhook certificate issued");
    }
    slot.set_pem(&stored.tls_crt, &stored.tls_key)?;
    if let Some(end) = certs::not_after(&stored.tls_crt) {
        let seconds = end.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        metrics
            .expiry
            .set(i64::try_from(seconds).unwrap_or(i64::MAX));
    }
    publish_ca(configurations, &config.configuration, &stored.ca_crt).await
}

/// Sets the `caBundle` of every webhook of the configuration to `ca`, when it differs.
async fn publish_ca(
    configurations: &Api<MutatingWebhookConfiguration>,
    name: &str,
    ca: &[u8],
) -> Result<(), String> {
    let current = configurations
        .get(name)
        .await
        .map_err(|e| format!("read webhook configuration: {e}"))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(ca);
    let operations: Vec<_> = current
        .webhooks
        .iter()
        .flatten()
        .enumerate()
        .filter(|(_, webhook)| {
            webhook
                .client_config
                .ca_bundle
                .as_ref()
                .map(|b| b.0.as_slice())
                != Some(ca)
        })
        .map(|(i, _)| {
            json!({"op": "add", "path": format!("/webhooks/{i}/clientConfig/caBundle"),
                "value": encoded})
        })
        .collect();
    if operations.is_empty() {
        return Ok(());
    }
    let patch: json_patch::Patch =
        serde_json::from_value(json!(operations)).map_err(|e| format!("caBundle patch: {e}"))?;
    configurations
        .patch(name, &PatchParams::default(), &Patch::Json::<()>(patch))
        .await
        .map_err(|e| format!("patch webhook configuration: {e}"))?;
    tracing::info!(webhooks = operations.len(), "caBundle updated");
    Ok(())
}

fn from_secret(secret: &Secret) -> Stored {
    let part = |key: &str| {
        secret
            .data
            .as_ref()
            .and_then(|data| data.get(key))
            .map(|value| value.0.clone())
            .unwrap_or_default()
    };
    Stored {
        ca_crt: part("ca.crt"),
        ca_key: part("ca.key"),
        tls_crt: part("tls.crt"),
        tls_key: part("tls.key"),
    }
}

fn to_data(stored: &Stored) -> BTreeMap<String, ByteString> {
    [
        ("ca.crt", &stored.ca_crt),
        ("ca.key", &stored.ca_key),
        ("tls.crt", &stored.tls_crt),
        ("tls.key", &stored.tls_key),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), ByteString(value.clone())))
    .collect()
}
