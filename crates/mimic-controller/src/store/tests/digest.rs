//! F9 step 2: the `:meta` `outbound_digest` field (PUL-06, store half).

use sha2::{Digest, Sha256};

use super::v2::push_v2;
use super::*;

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `:meta` `outbound_digest` and `version`, and the `:outbound` bytes, as stored.
async fn stored(ns: &str, dep: &str) -> (Option<String>, String, Vec<u8>) {
    let k = keys(ns, dep).expect("keys");
    let mut c = conn(None).await;
    let digest: Option<String> = cmd(&mut c, &["HGET", &k.meta, "outbound_digest"]).await;
    let version: String = cmd(&mut c, &["HGET", &k.meta, "version"]).await;
    let outbound: Option<Vec<u8>> = cmd(&mut c, &["GET", &k.outbound]).await;
    (digest, version, outbound.unwrap_or_default())
}

fn hash_part(digest: &Option<String>) -> Option<&str> {
    digest
        .as_deref()
        .and_then(|d| d.split_once(':'))
        .map(|d| d.1)
}

#[tokio::test]
async fn pul_06_a_version_2_push_stores_version_and_hash_of_outbound() {
    let (store, _) = replica(None, config(Duration::from_secs(30 * DAY)));
    let (ns, dep) = (unique("pul06a"), "pay");
    let out = [("api.example.com:443", "/v1/x", 3)];
    let got = store
        .push(&ns, dep, push_v2("pod-a", 1, &["/a"], &out))
        .await;
    assert_eq!(got, Ok(Outcome::Applied));
    let (digest, version, outbound) = stored(&ns, dep).await;
    assert!(!outbound.is_empty());
    assert_eq!(digest, Some(format!("{version}:{}", hex(&outbound))));

    // A second push that changes the outbound part moves both halves.
    let out = [("api.example.com:443", "/v1/y", 1)];
    let got = store
        .push(&ns, dep, push_v2("pod-a", 2, &["/a"], &out))
        .await;
    assert_eq!(got, Ok(Outcome::Applied));
    let (digest2, version2, outbound2) = stored(&ns, dep).await;
    assert_ne!(version, version2);
    assert_ne!(outbound, outbound2);
    assert_eq!(digest2, Some(format!("{version2}:{}", hex(&outbound2))));
}

#[tokio::test]
async fn pul_06_a_version_1_push_keeps_the_hash_and_follows_the_version() {
    let (store, _) = replica(None, config(Duration::from_secs(30 * DAY)));
    let (ns, dep) = (unique("pul06b"), "pay");
    let out = [("api.example.com:443", "/v1/x", 3)];
    store
        .push(&ns, dep, push_v2("pod-a", 1, &["/a"], &out))
        .await
        .expect("applied");
    let (digest, version, outbound) = stored(&ns, dep).await;
    // Inbound only: `:outbound` is unchanged.
    store
        .push(&ns, dep, push_v2("pod-a", 2, &["/b"], &[]))
        .await
        .expect("applied");
    let (digest2, version2, outbound2) = stored(&ns, dep).await;
    assert_ne!(version, version2);
    assert_eq!(outbound, outbound2);
    assert_eq!(hash_part(&digest), hash_part(&digest2));
    assert_eq!(digest2, Some(format!("{version2}:{}", hex(&outbound2))));
}

#[tokio::test]
async fn pul_06_a_deployment_without_outbound_holds_the_empty_string_digest() {
    let (store, _) = replica(None, config(Duration::from_secs(30 * DAY)));
    let (ns, dep) = (unique("pul06c"), "pay");
    store
        .push(&ns, dep, push_v2("pod-a", 1, &["/a"], &[]))
        .await
        .expect("applied");
    let (digest, version, outbound) = stored(&ns, dep).await;
    assert!(outbound.is_empty());
    assert_eq!(digest, Some(format!("{version}:{}", hex(b""))));
}

#[tokio::test]
async fn pul_06_the_reader_returns_the_hash_and_treats_a_mismatched_version_as_stale() {
    let (store, _) = replica(None, config(Duration::from_secs(30 * DAY)));
    let (ns, dep) = (unique("pul06d"), "pay");
    let out = [("api.example.com:443", "/v1/x", 3)];
    store
        .push(&ns, dep, push_v2("pod-a", 1, &["/a"], &out))
        .await
        .expect("applied");
    let (_, version, outbound) = stored(&ns, dep).await;
    assert_eq!(
        store.outbound_digest(&ns, dep).await,
        Ok(Some(hex(&outbound)))
    );

    let k = keys(&ns, dep).expect("keys");
    let mut c = conn(None).await;
    // Left by an F8 controller after a rollback: an older version, none, or garbage.
    let older = format!(
        "{}:{}",
        version.parse::<u64>().expect("number") - 1,
        hex(&outbound)
    );
    let _: i64 = cmd(&mut c, &["HSET", &k.meta, "outbound_digest", &older]).await;
    assert_eq!(store.outbound_digest(&ns, dep).await, Ok(None));
    let _: i64 = cmd(&mut c, &["HDEL", &k.meta, "outbound_digest"]).await;
    assert_eq!(store.outbound_digest(&ns, dep).await, Ok(None));
    let _: i64 = cmd(&mut c, &["HSET", &k.meta, "outbound_digest", "garbage"]).await;
    assert_eq!(store.outbound_digest(&ns, dep).await, Ok(None));
}

#[tokio::test]
async fn pul_06_the_other_meta_fields_are_what_f8_writes() {
    let (store, _) = replica(None, config(Duration::from_secs(30 * DAY)));
    let (ns, dep) = (unique("pul06e"), "pay");
    let out = [("api.example.com:443", "/v1/x", 3)];
    store
        .push(&ns, dep, push_v2("pod-a", 1, &["/a"], &out))
        .await
        .expect("applied");
    let k = keys(&ns, dep).expect("keys");
    let mut c = conn(None).await;
    let mut fields: Vec<String> = cmd(&mut c, &["HKEYS", &k.meta]).await;
    fields.sort();
    assert_eq!(
        fields,
        [
            "endpoints_inbound",
            "endpoints_outbound",
            "exchanges",
            "outbound_digest",
            "report_bytes",
            "sample_bytes",
            "updated",
            "version"
        ]
    );
}
