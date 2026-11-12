//! The reflectors of the mesh (design §4.1): one watch per kind, on every replica, each event
//! converted at once into the input structs of the hub, so that only the fields the view uses
//! are kept in memory.

use std::sync::Arc;
use std::time::Duration;

use futures::{Stream, StreamExt};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::discovery::v1::EndpointSlice;
use kube::ResourceExt;
use kube::api::{Api, ApiResource, DynamicObject, GroupVersionKind, ListParams};
use kube::core::PartialObjectMeta;
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher::{self, Event as Watched};

use super::convert::{self, Key};
use super::hub::{Event, Hub};

/// How long to wait before a watch that ended is started again, and between two looks for the
/// `ReferenceGrant` CRD.
const RETRY: Duration = Duration::from_secs(10);

fn key_of<K: kube::Resource>(object: &K) -> Option<Key> {
    Some((object.namespace()?, object.name_any()))
}

/// Feeds the hub from a watch until it ends or the resource is gone (404). `convert` is `None`
/// for an object that is not (or no longer) one of ours: it is removed.
async fn forward<K, T, S>(
    events: S,
    convert: impl Fn(&K) -> Option<(Key, T)>,
    feed: impl Fn(Event<T>),
) where
    K: kube::Resource,
    S: Stream<Item = Result<Watched<K>, watcher::Error>>,
{
    let mut events = std::pin::pin!(events);
    while let Some(event) = events.next().await {
        match event {
            Ok(Watched::Init) => feed(Event::Init),
            Ok(Watched::InitApply(object)) => {
                if let Some((key, value)) = convert(&object) {
                    feed(Event::InitApply(key, value));
                }
            }
            Ok(Watched::InitDone) => feed(Event::InitDone),
            Ok(Watched::Apply(object)) => match convert(&object) {
                Some((key, value)) => feed(Event::Apply(key, value)),
                None => {
                    if let Some(key) = key_of(&object) {
                        feed(Event::Delete(key));
                    }
                }
            },
            Ok(Watched::Delete(object)) => {
                if let Some(key) = key_of(&object) {
                    feed(Event::Delete(key));
                }
            }
            Err(error) => {
                if gone(&error) {
                    return;
                }
                tracing::warn!(error = %error, "mesh watch error");
            }
        }
    }
}

/// The API server says the resource does not exist (a CRD that is not installed).
fn gone(error: &watcher::Error) -> bool {
    matches!(
        error,
        watcher::Error::InitialListFailed(kube::Error::Api(status))
            | watcher::Error::WatchStartFailed(kube::Error::Api(status))
            | watcher::Error::WatchFailed(kube::Error::Api(status))
            if status.code == 404
    )
}

async fn services(client: kube::Client, hub: Arc<Hub>) {
    loop {
        let api = Api::<Service>::all(client.clone());
        let events = watcher::watcher(api, watcher::Config::default()).default_backoff();
        forward(events, convert::service, |e| hub.feed_services(e)).await;
        tokio::time::sleep(RETRY).await;
    }
}

async fn slices(client: kube::Client, hub: Arc<Hub>) {
    loop {
        let api = Api::<EndpointSlice>::all(client.clone());
        let events = watcher::watcher(api, watcher::Config::default()).default_backoff();
        forward(events, convert::slice, |e| hub.feed_slices(e)).await;
        tokio::time::sleep(RETRY).await;
    }
}

async fn deployments(client: kube::Client, hub: Arc<Hub>) {
    loop {
        // Metadata only: the view needs to know that a Deployment exists, nothing of its spec.
        let api = Api::<PartialObjectMeta<Deployment>>::all(client.clone());
        let events = watcher::watcher(api, watcher::Config::default()).default_backoff();
        forward(
            events,
            |o| key_of(o).map(|key| (key, ())),
            |e| hub.feed_deployments(e),
        )
        .await;
        tokio::time::sleep(RETRY).await;
    }
}

async fn configs(client: kube::Client, hub: Arc<Hub>) {
    let resource = ApiResource::from_gvk(&GroupVersionKind::gvk(
        "mimic.platform.io",
        "v1alpha1",
        "MimicConfig",
    ));
    loop {
        let api = Api::<DynamicObject>::all_with(client.clone(), &resource);
        let events = watcher::watcher(api, watcher::Config::default()).default_backoff();
        forward(events, convert::config, |e| hub.feed_configs(e)).await;
        tokio::time::sleep(RETRY).await;
    }
}

/// `ReferenceGrant`s, when the Gateway API CRD exists: it may be installed after the controller
/// started (and removed), so it is looked for again every [`RETRY`].
async fn grants(client: kube::Client, hub: Arc<Hub>) {
    let resource = ApiResource::from_gvk(&GroupVersionKind::gvk(
        "gateway.networking.k8s.io",
        "v1beta1",
        "ReferenceGrant",
    ));
    loop {
        let api = Api::<DynamicObject>::all_with(client.clone(), &resource);
        match api.list(&ListParams::default().limit(1)).await {
            Ok(_) => {
                let events = watcher::watcher(api, watcher::Config::default()).default_backoff();
                forward(events, convert::grant, |e| hub.feed_grants(e)).await;
                // Gone while watched: nobody consents until it is back.
                hub.grants_absent();
            }
            Err(kube::Error::Api(status)) if status.code == 404 => hub.grants_absent(),
            Err(error) => tracing::warn!(error = %error, "ReferenceGrant CRD lookup failed"),
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Wakes the hub when the `MimicWorkload`s change what a sidecar's mode is.
async fn modes(mut changes: tokio::sync::watch::Receiver<u64>, hub: Arc<Hub>) {
    while changes.changed().await.is_ok() {
        hub.dirty();
    }
}

/// Starts the reflectors, the debouncer and the status writer.
pub fn spawn(
    client: kube::Client,
    hub: Arc<Hub>,
    mode_changes: tokio::sync::watch::Receiver<u64>,
    sink: Arc<dyn super::status::StatusSink>,
) {
    tokio::spawn(services(client.clone(), hub.clone()));
    tokio::spawn(slices(client.clone(), hub.clone()));
    tokio::spawn(deployments(client.clone(), hub.clone()));
    tokio::spawn(configs(client.clone(), hub.clone()));
    tokio::spawn(grants(client, hub.clone()));
    tokio::spawn(modes(mode_changes, hub.clone()));
    tokio::spawn(hub.clone().run());
    tokio::spawn(super::status::run(hub, sink));
}
