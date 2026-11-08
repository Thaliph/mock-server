//! F11 step 6: the run's keys (LDT-09, store part). Tests that count runs of the whole cluster
//! use a Valkey database of their own (`loads:active` is global).

use mimic_core::load::summary::{PodResults, Stats};

use super::*;
use crate::store::load::*;

const UID: &str = "0b9c1d52-6a3f-4c58-9e11-2f7d3a8b4c60";

fn uid(n: u32) -> String {
    format!("{n:08x}-6a3f-4c58-9e11-2f7d3a8b4c60")
}

const LIMITS_OPEN: AdmitLimits = AdmitLimits {
    max_runs: 1000,
    max_namespace_runs: 1000,
    budget: 1 << 40,
};

fn admit<'a>(ns: &'a str, dep: &'a str, uid: &'a str, plan: &'a str) -> Admit<'a> {
    Admit {
        namespace: ns,
        deployment: dep,
        uid,
        spec: r#"{"name":"t","deployment":"pay"}"#,
        plan,
        ticket: "v1.payload.signature",
        now_ms: 1_000_000,
        deadline_ms: 1_000_000 + 600_000,
        reserve: 1 << 20,
        limits: LIMITS_OPEN,
    }
}

fn post(final_post: bool, endpoints: usize) -> StoredPost {
    StoredPost {
        final_post,
        reason: None,
        results: PodResults {
            endpoints: vec![Stats::default(); endpoints],
        },
    }
}

fn summary(bytes: usize) -> String {
    format!(r#"{{"pad":"{}"}}"#, "x".repeat(bytes - 10))
}

fn ended<'a>(summary: &'a str, reason: &'a str) -> Ended<'a> {
    Ended {
        phase: "Succeeded",
        reason,
        summary,
        end_ms: 1_100_000,
    }
}

async fn memory(key: &str, db: Option<u8>) -> u64 {
    cmd(&mut conn(db).await, &["MEMORY", "USAGE", key]).await
}

#[tokio::test]
async fn ldt_09_admission_refuses_in_order_and_frees_the_slot_at_the_end() {
    let db = Some(11);
    let (store, _m) = replica(db, config(Duration::from_secs(3600)));
    let mut c = conn(db).await;
    let _: () = cmd(&mut c, &["FLUSHDB"]).await;
    let limits = AdmitLimits {
        max_runs: 3,
        max_namespace_runs: 2,
        budget: 3 << 20,
    };
    let go = |ns: &'static str, dep: &'static str, uid: String, reserve: u64| {
        let store = store.clone();
        async move {
            let mut a = admit(ns, dep, &uid, "{}");
            a.limits = limits;
            a.reserve = reserve;
            store.load_admit(&a).await
        }
    };
    assert_eq!(go("a", "x", uid(1), 1 << 20).await, Ok(()));
    // The same deployment, even in a namespace that is full as well: Busy first.
    assert_eq!(go("a", "x", uid(2), 1 << 20).await, Err(AdmitError::Busy));
    assert_eq!(go("a", "y", uid(3), 1 << 20).await, Ok(()));
    assert_eq!(
        go("a", "z", uid(4), 1 << 20).await,
        Err(AdmitError::NamespaceBusy)
    );
    assert_eq!(go("b", "x", uid(5), 1 << 20).await, Ok(()));
    assert_eq!(
        go("c", "x", uid(6), 1 << 20).await,
        Err(AdmitError::ClusterBusy)
    );
    // Retrying an admitted run is not a second slot.
    assert_eq!(go("a", "x", uid(1), 1 << 20).await, Ok(()));
    let reserved: BTreeMap<String, u64> =
        cmd(&mut c, &["HGETALL", "mimic:v1:loads:reserved"]).await;
    assert_eq!(reserved.len(), 3, "{reserved:?}");

    // Past the budget: one run of 2 MiB does not fit next to 1 MiB, with a slot free.
    let _: () = cmd(&mut c, &["FLUSHDB"]).await;
    assert_eq!(go("a", "x", uid(1), 2 << 20).await, Ok(()));
    assert_eq!(
        go("b", "x", uid(2), 2 << 20).await,
        Err(AdmitError::StoreFull)
    );
    assert!(
        store
            .load_end("a", "x", &uid(1), &ended("{}", "Completed"))
            .await
            .unwrap()
    );
    assert_eq!(go("b", "x", uid(2), 2 << 20).await, Ok(()));
}

#[tokio::test]
async fn ldt_09_a_member_past_its_deadline_frees_its_slot() {
    let db = Some(12);
    let (store, _m) = replica(db, config(Duration::from_secs(3600)));
    let _: () = cmd(&mut conn(db).await, &["FLUSHDB"]).await;
    let limits = AdmitLimits {
        max_runs: 1,
        ..LIMITS_OPEN
    };
    let (u1, u2) = (uid(1), uid(2));
    let mut first = admit("a", "x", &u1, "{}");
    first.limits = limits;
    assert_eq!(store.load_admit(&first).await, Ok(()));
    let mut second = admit("b", "x", &u2, "{}");
    second.limits = limits;
    assert_eq!(
        store.load_admit(&second).await,
        Err(AdmitError::ClusterBusy)
    );
    // The first run's deadline passed: its member and reservation are dropped.
    second.now_ms = first.deadline_ms + 1;
    second.deadline_ms = second.now_ms + 600_000;
    assert_eq!(store.load_admit(&second).await, Ok(()));
    let mut c = conn(db).await;
    let reserved: BTreeMap<String, u64> =
        cmd(&mut c, &["HGETALL", "mimic:v1:loads:reserved"]).await;
    assert_eq!(
        reserved.keys().cloned().collect::<Vec<_>>(),
        ["b/x/".to_string() + &uid(2)]
    );
}

#[tokio::test]
async fn ldt_09_posts_replace_and_the_same_post_twice_changes_nothing() {
    let (store, _m) = replica(None, config(Duration::from_secs(3600)));
    let ns = unique("ldt");
    let pod = "mimic-load-0b9c1d526a3f-0-bcdfg";
    assert_eq!(
        store.load_admit(&admit(&ns, "pay", UID, "{}")).await,
        Ok(())
    );
    let first = post(false, 2);
    assert_eq!(
        store.load_post(&ns, "pay", UID, pod, &first).await.unwrap(),
        Some(false)
    );
    let before: Vec<String> = cmd(
        &mut conn(None).await,
        &["HGETALL", &format!("mimic:v1:{{{ns}/pay}}:load:{UID}")],
    )
    .await;
    store.load_post(&ns, "pay", UID, pod, &first).await.unwrap();
    let after: Vec<String> = cmd(
        &mut conn(None).await,
        &["HGETALL", &format!("mimic:v1:{{{ns}/pay}}:load:{UID}")],
    )
    .await;
    assert_eq!(before, after);

    // A later cumulative post replaces; a second pod has its own field.
    let other = "mimic-load-0b9c1d526a3f-1-bcdfg";
    store
        .load_post(&ns, "pay", UID, pod, &post(true, 3))
        .await
        .unwrap();
    store
        .load_post(&ns, "pay", UID, other, &post(false, 1))
        .await
        .unwrap();
    let posts = store.load_posts(&ns, "pay", UID).await.unwrap();
    assert_eq!(
        posts.keys().map(String::as_str).collect::<Vec<_>>(),
        [pod, other]
    );
    assert_eq!(posts[pod], post(true, 3));
    assert_eq!(posts[other], post(false, 1));
    // A late non-final post never takes the final one back.
    store
        .load_post(&ns, "pay", UID, pod, &post(false, 3))
        .await
        .unwrap();
    assert!(store.load_posts(&ns, "pay", UID).await.unwrap()[pod].final_post);
}

#[tokio::test]
async fn ldt_09_the_stop_flag_comes_with_the_plan_and_the_posts() {
    let (store, _m) = replica(None, config(Duration::from_secs(3600)));
    let ns = unique("ldt");
    store
        .load_admit(&admit(&ns, "pay", UID, r#"{"plan":1}"#))
        .await
        .unwrap();
    let run = store.load_run(&ns, "pay", UID).await.unwrap().unwrap();
    assert_eq!(run.plan, r#"{"plan":1}"#);
    assert_eq!(run.ticket, "v1.payload.signature");
    assert!(!run.stop);
    assert_eq!(
        store.load_active(&ns, "pay", UID).await.unwrap(),
        Some(false)
    );
    store.load_stop(&ns, "pay", UID).await.unwrap();
    assert_eq!(
        store.load_active(&ns, "pay", UID).await.unwrap(),
        Some(true)
    );
    assert_eq!(
        store
            .load_post(
                &ns,
                "pay",
                UID,
                "mimic-load-0b9c1d526a3f-0-bcdfg",
                &post(false, 1)
            )
            .await
            .unwrap(),
        Some(true)
    );
    assert_eq!(store.load_run(&ns, "pay", &uid(9)).await.unwrap(), None);
}

#[tokio::test]
async fn ldt_09_size_while_running_and_after_the_end() {
    let (store, _m) = replica(None, config(Duration::from_secs(3600)));
    let ns = unique("ldt");
    let plan = format!(r#"{{"pad":"{}"}}"#, "p".repeat(300 << 10));
    store
        .load_admit(&admit(&ns, "pay", UID, &plan))
        .await
        .unwrap();
    // Two pods, each close to the 256 KiB bound.
    for index in 0..2 {
        let pod = format!("mimic-load-0b9c1d526a3f-{index}-bcdfg");
        let big = post(false, 3000);
        let bytes = serde_json::to_vec(&big).unwrap().len();
        assert!(bytes > 200 << 10 && bytes <= MAX_POST, "{bytes}");
        store.load_post(&ns, "pay", UID, &pod, &big).await.unwrap();
    }
    let key = format!("mimic:v1:{{{ns}/pay}}:load:{UID}");
    let running = memory(&key, None).await;
    assert!(
        running <= plan.len() as u64 + 2 * MAX_POST as u64 + (16 << 10),
        "{running}"
    );

    let big = summary(MAX_SUMMARY);
    assert!(
        store
            .load_end(&ns, "pay", UID, &ended(&big, "Completed"))
            .await
            .unwrap()
    );
    let fields = store.load_fields(&ns, "pay", UID).await.unwrap();
    for gone in ["plan", "ticket"] {
        assert!(!fields.contains_key(gone), "{gone}");
    }
    assert!(
        fields.keys().all(|k| !k.starts_with("mimic-load-")),
        "{fields:?}"
    );
    assert_eq!(fields["summary"], big);
    assert_eq!(fields["phase"], "Succeeded");
    assert_eq!(fields["reason"], "Completed");
    assert_eq!(fields["end"], "1100000");
    assert_eq!(fields["start"], "1000000");
    assert_eq!(fields["stop"], "0");
    assert!(fields["spec"].contains("pay"));
    let ended_size = memory(&key, None).await;
    assert!(
        ended_size <= (MAX_SUMMARY + (16 << 10)) as u64,
        "{ended_size}"
    );

    // Gone from the active set and the reservations; a post is refused; a second end is a no-op.
    let mut c = conn(None).await;
    let member = format!("{ns}/pay/{UID}");
    let active: Option<f64> = cmd(&mut c, &["ZSCORE", "mimic:v1:loads:active", &member]).await;
    assert_eq!(active, None);
    let reserved: Option<String> = cmd(&mut c, &["HGET", "mimic:v1:loads:reserved", &member]).await;
    assert_eq!(reserved, None);
    assert_eq!(
        store
            .load_post(
                &ns,
                "pay",
                UID,
                "mimic-load-0b9c1d526a3f-0-bcdfg",
                &post(true, 1)
            )
            .await
            .unwrap(),
        None
    );
    assert_eq!(store.load_run(&ns, "pay", UID).await.unwrap(), None);
    assert!(
        !store
            .load_end(&ns, "pay", UID, &ended("{}", "Stopped"))
            .await
            .unwrap()
    );
    assert_eq!(
        store.load_fields(&ns, "pay", UID).await.unwrap()["reason"],
        "Completed"
    );
}

#[tokio::test]
async fn ldt_09_loads_keep_20_runs_and_everything_expires() {
    let ttl = Duration::from_secs(3600);
    let (store, _m) = replica(None, config(ttl));
    let ns = unique("ldt");
    for n in 1..=21u32 {
        let id = uid(n);
        let mut a = admit(&ns, "pay", &id, "{}");
        a.now_ms = 1_000_000 + u64::from(n);
        a.deadline_ms = a.now_ms + 1000;
        store.load_admit(&a).await.unwrap();
        store
            .load_end(&ns, "pay", &id, &ended("{}", "Completed"))
            .await
            .unwrap();
    }
    let mut c = conn(None).await;
    let loads = format!("mimic:v1:{{{ns}/pay}}:loads");
    let members: Vec<String> = cmd(&mut c, &["ZRANGE", &loads, "0", "-1"]).await;
    assert_eq!(members.len(), 20);
    assert_eq!(members[0], uid(2));
    assert_eq!(members[19], uid(21));
    let evicted: u64 = cmd(
        &mut c,
        &["EXISTS", &format!("mimic:v1:{{{ns}/pay}}:load:{}", uid(1))],
    )
    .await;
    assert_eq!(evicted, 0);
    for key in [loads, format!("mimic:v1:{{{ns}/pay}}:load:{}", uid(21))] {
        let pttl: i64 = cmd(&mut c, &["PTTL", &key]).await;
        assert!(pttl > 0 && pttl <= 3_600_000, "{key}: {pttl}");
    }
}

#[tokio::test]
async fn ldt_09_names_that_are_not_dns_1123_are_refused() {
    let (store, _m) = replica(None, config(Duration::from_secs(3600)));
    assert_eq!(
        store.load_admit(&admit("a:b", "pay", UID, "{}")).await,
        Err(AdmitError::Name)
    );
    assert_eq!(
        store.load_admit(&admit("a", "pay", "x}y", "{}")).await,
        Err(AdmitError::Name)
    );
}
