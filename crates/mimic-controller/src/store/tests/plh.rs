//! PLH-06 (store half): a report stored in the F12 form (legacy markers) is read as placeholders,
//! and a read never writes.

use super::v2::push_v2;
use super::*;

const MARKER: &str = "<redacted>";

/// Any push, which creates `:meta`.
fn with_credentials() -> Push {
    push_v2("pod-a", 1, &["/a"], &[("api.example.com:443", "/v1/x", 1)])
}

/// The F12 golden report (legacy markers, consistent keys and sizes) as `:report` and
/// `:outbound` bytes.
fn f12() -> (Vec<u8>, Vec<u8>) {
    let path = format!(
        "{}/../mimic-core/tests/fixtures/f12-golden/single-below.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(path).expect("fixture");
    let (inbound, outbound) = Report::from_json(&text).expect("parses").into_split();
    let outbound = outbound.expect("outbound part");
    (
        inbound.to_json().expect("json").into_bytes(),
        outbound.to_json().expect("json").into_bytes(),
    )
}

/// Overwrites `:report` and `:outbound` with the F12 form; returns the stored bytes.
async fn downgrade(ns: &str, dep: &str) -> (Vec<u8>, Vec<u8>) {
    let k = keys(ns, dep).expect("keys");
    let mut c = conn(None).await;
    let (report, outbound) = f12();
    for (key, bytes) in [(&k.report, &report), (&k.outbound, &outbound)] {
        let _: () = redis::cmd("SET")
            .arg(key)
            .arg(bytes)
            .query_async(&mut c)
            .await
            .expect("set");
    }
    (report, outbound)
}

/// The markers that are not an authority host.
fn stray(text: &str) -> usize {
    text.matches(MARKER).count() - text.matches("<redacted>:443").count()
}

#[tokio::test]
async fn plh_06_reads_answer_placeholders_and_never_write() {
    let (store, _) = replica(None, config(Duration::from_secs(30 * DAY)));
    let (ns, dep) = (unique("plh06a"), "pay");
    assert_eq!(
        store.push(&ns, dep, with_credentials()).await,
        Ok(Outcome::Applied)
    );
    let digest = store.outbound_digest(&ns, dep).await.expect("digest");
    let (report, outbound) = downgrade(&ns, dep).await;
    assert!(stray(&String::from_utf8_lossy(&report)) > 0);
    assert!(stray(&String::from_utf8_lossy(&outbound)) > 0);

    let text = String::from_utf8(store.report(&ns, dep).await.expect("report")).expect("utf-8");
    assert_eq!(stray(&text), 0, "no legacy marker but in authority hosts");
    for half in [Half::Both, Half::Inbound, Half::Outbound] {
        let parsed = store
            .stored_report(&ns, dep, half)
            .await
            .expect("stored")
            .parse()
            .expect("parses");
        assert_eq!(stray(&parsed.to_json().expect("json")), 0);
    }
    let bytes = store
        .outbound_bytes(&ns, dep)
        .await
        .expect("bytes")
        .expect("some");
    let parsed = parse_outbound(&bytes).expect("parses");
    assert_eq!(stray(&parsed.to_json().expect("json")), 0);

    // The reads wrote nothing: the stored bytes and the digest are what they were.
    let k = keys(&ns, dep).expect("keys");
    let mut c = conn(None).await;
    let again: Vec<u8> = cmd(&mut c, &["GET", &k.report]).await;
    let again_out: Vec<u8> = cmd(&mut c, &["GET", &k.outbound]).await;
    assert_eq!((again, again_out), (report, outbound));
    assert_eq!(store.outbound_digest(&ns, dep).await, Ok(digest));
}

#[tokio::test]
async fn plh_06_a_push_after_legacy_data_leaves_no_marker_and_changes_the_digest() {
    let (store, _) = replica(None, config(Duration::from_secs(30 * DAY)));
    let (ns, dep) = (unique("plh06b"), "pay");
    assert_eq!(
        store.push(&ns, dep, with_credentials()).await,
        Ok(Outcome::Applied)
    );
    let digest = store.outbound_digest(&ns, dep).await.expect("digest");
    downgrade(&ns, dep).await;
    let out = [("api.example.com:443", "/v1/y", 1)];
    let got = store
        .push(&ns, dep, push_v2("pod-b", 1, &["/b"], &out))
        .await;
    assert_eq!(got, Ok(Outcome::Applied));
    let k = keys(&ns, dep).expect("keys");
    let mut c = conn(None).await;
    for key in [&k.report, &k.outbound] {
        let bytes: Vec<u8> = cmd(&mut c, &["GET", key]).await;
        assert_eq!(stray(&String::from_utf8(bytes).expect("utf-8")), 0);
    }
    assert_ne!(store.outbound_digest(&ns, dep).await, Ok(digest));
}
