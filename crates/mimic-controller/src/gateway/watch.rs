//! The reflectors of the gateway reconciler (spec §7.10 "What is read"): `gatewayclasses`,
//! `gateways`, and the `deployments` and `services` with the gateway label, cluster-wide, each
//! event converted at once, so that only the fields the reconciler uses are kept in memory.

use std::sync::Arc;

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Service;
use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind, ListParams};
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher;
use mimic_core::gateway::GATEWAY_LABEL;

use super::cluster::ApiError;
use super::convert;
use super::reconcile::Reconciler;
use crate::mesh::Hub;
use crate::mesh::watch::{CRD_LOOK, RETRY, forward};

fn resource(kind: &str) -> ApiResource {
    ApiResource::from_gvk(&GroupVersionKind::gvk(
        "gateway.networking.k8s.io",
        "v1",
        kind,
    ))
}

async fn classes(client: kube::Client, reconciler: Arc<Reconciler>) {
    let resource = resource("GatewayClass");
    loop {
        let api = Api::<DynamicObject>::all_with(client.clone(), &resource);
        let events = watcher::watcher(api.clone(), watcher::Config::default()).default_backoff();
        forward(
            events,
            convert::class,
            |e| reconciler.feed_classes(e),
            Some(&api),
        )
        .await;
        tokio::time::sleep(RETRY).await;
    }
}

/// The objects with the gateway label: the full objects, for the Deployment status and the
/// Service addresses; only their [`convert::Seen`] is kept.
async fn deployments(client: kube::Client, reconciler: Arc<Reconciler>) {
    loop {
        let api = Api::<Deployment>::all(client.clone());
        let config = watcher::Config::default().labels(GATEWAY_LABEL);
        let events = watcher::watcher(api, config).default_backoff();
        forward(
            events,
            convert::deployment,
            |e| reconciler.feed_deployments(e),
            None,
        )
        .await;
        tokio::time::sleep(RETRY).await;
    }
}

async fn services(client: kube::Client, reconciler: Arc<Reconciler>) {
    loop {
        let api = Api::<Service>::all(client.clone());
        let config = watcher::Config::default().labels(GATEWAY_LABEL);
        let events = watcher::watcher(api, config).default_backoff();
        forward(
            events,
            convert::service,
            |e| reconciler.feed_services(e),
            None,
        )
        .await;
        tokio::time::sleep(RETRY).await;
    }
}

/// `Gateway`s, when the CRD exists: it may be installed after the controller started (and
/// removed), so it is looked for again every [`CRD_LOOK`] (GW-15). The hub logs the changes of
/// presence.
pub(crate) async fn gateways(client: kube::Client, hub: Arc<Hub>, reconciler: Arc<Reconciler>) {
    let resource = resource("Gateway");
    loop {
        let api = Api::<DynamicObject>::all_with(client.clone(), &resource);
        match api.list(&ListParams::default().limit(1)).await {
            Ok(_) => {
                reconciler.crd(true);
                hub.gateway_crd(true);
                let events =
                    watcher::watcher(api.clone(), watcher::Config::default()).default_backoff();
                forward(
                    events,
                    convert::gateway,
                    |e| reconciler.feed_gateways(e),
                    Some(&api),
                )
                .await;
                // Gone while watched: nothing is written until a new full list.
                reconciler.crd(false);
                hub.gateway_crd(false);
            }
            Err(kube::Error::Api(status)) if status.code == 404 => {
                reconciler.crd(false);
                hub.gateway_crd(false);
            }
            Err(error) => {
                let error = ApiError::from(&error);
                tracing::warn!(error = %error, "gateway CRD lookup failed");
            }
        }
        tokio::time::sleep(CRD_LOOK).await;
    }
}

/// Starts the reflectors and the reconciler; nothing without a reconciler (the gateway is off).
pub fn start(client: kube::Client, hub: Arc<Hub>, reconciler: Option<Arc<Reconciler>>) {
    let Some(reconciler) = reconciler else {
        return;
    };
    tokio::spawn(classes(client.clone(), reconciler.clone()));
    tokio::spawn(deployments(client.clone(), reconciler.clone()));
    tokio::spawn(services(client.clone(), reconciler.clone()));
    tokio::spawn(gateways(client, hub, reconciler.clone()));
    tokio::spawn(reconciler.run());
}
