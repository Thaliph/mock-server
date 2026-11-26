//! The reflectors of the gateway reconciler (spec §7.10 "What is read"): `gatewayclasses`,
//! `gateways`, and the `deployments` and `services` with the gateway label, cluster-wide, each
//! event converted at once, so that only the fields the reconciler uses are kept in memory.

use std::sync::Arc;

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Service;
use kube::api::{Api, DynamicObject};
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher;
use mimic_core::gateway::GATEWAY_LABEL;

use super::cluster::ApiError;
use super::convert;
use super::reconcile::Reconciler;
use crate::mesh::Hub;
use crate::mesh::watch::{CRD_LOOK, RETRY, forward, gateway_api, while_crd};

/// The log line of a failed gateway watch: the API status reason and code only.
fn failed(error: &watcher::Error) {
    tracing::warn!(error = %ApiError::from(error), "gateway watch error");
}

async fn classes(client: kube::Client, reconciler: Arc<Reconciler>) {
    let resource = gateway_api("v1", "GatewayClass");
    loop {
        let api = Api::<DynamicObject>::all_with(client.clone(), &resource);
        let events = watcher::watcher(api.clone(), watcher::Config::default()).default_backoff();
        forward(
            events,
            convert::class,
            |e| reconciler.feed_classes(e),
            Some(&api),
            failed,
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
            failed,
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
            failed,
        )
        .await;
        tokio::time::sleep(RETRY).await;
    }
}

/// `Gateway`s, when the CRD exists: it may be installed after the controller started (and
/// removed), so it is looked for again every [`CRD_LOOK`] (GW-15). The hub logs the changes of
/// presence. A Gateway mimic does not serve is kept as a stub; when a mimic class appears, the
/// watch lists again, so that the stubs of that class are converted in full (#576).
pub(crate) async fn gateways(client: kube::Client, hub: Arc<Hub>, reconciler: Arc<Reconciler>) {
    let api = Api::<DynamicObject>::all_with(client, &gateway_api("v1", "Gateway"));
    let reconciler = &*reconciler;
    while_crd(
        api,
        CRD_LOOK,
        // Absent, or gone while watched: nothing is written until a new full list.
        |present| {
            reconciler.crd(present);
            hub.gateway_crd(present);
        },
        |api| async move {
            let mut relists = reconciler.relists();
            loop {
                relists.borrow_and_update();
                let events =
                    watcher::watcher(api.clone(), watcher::Config::default()).default_backoff();
                let watched = forward(
                    events,
                    |object| convert::entry(object, |ns, class| reconciler.serves(ns, class)),
                    |e| reconciler.feed_gateways(e),
                    Some(&api),
                    failed,
                );
                tokio::select! {
                    () = watched => return,
                    // A new mimic class: a new list; the table keeps its content until it ends.
                    relisted = relists.changed() => {
                        if relisted.is_err() {
                            return;
                        }
                    }
                }
            }
        },
        |error| tracing::warn!(error = %ApiError::from(error), "gateway CRD lookup failed"),
    )
    .await;
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
