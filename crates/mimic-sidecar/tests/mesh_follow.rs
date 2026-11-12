//! MSH-33 to MSH-36 of the mesh view client (spec §7.9), against a fake sidecar API served over
//! TLS in the test process.

mod common;
#[path = "common/mesh_api.rs"]
mod mesh_api;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::capture_logs;
use mesh_api::{MeshApi, Reply, mesh_api, view_json};
use mimic_core::Mode;
use mimic_runtime::config::Env;
use mimic_runtime::metrics::Metrics;
use mimic_sidecar::discovery::Target;
use mimic_sidecar::mesh::{Follower, MeshConfig, MeshState};
use mimic_sidecar::mode::{ModeConfig, ModeState};

const TOKEN: &str = "canary-token-7e31b2";

struct Rig {
    api: Arc<MeshApi>,
    mode: ModeState,
    metrics: Arc<Metrics>,
    dir: tempfile::TempDir,
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

fn config(api: &MeshApi, dir: &std::path::Path, vars: &[(&str, &str)]) -> MeshConfig {
    let env = Env::from_pairs(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let mut config = MeshConfig::from_env(&env, dir).unwrap();
    let token = dir.join("token");
    std::fs::write(&token, TOKEN).unwrap();
    config.target = Some(Target {
        url: format!("https://{}", api.addr),
        ca: api.ca_pem.clone(),
        token_file: token,
        workload: "pay".into(),
    });
    config
}

impl Rig {
    async fn start(script: Vec<Reply>, fallback: Reply) -> Rig {
        Rig::with(script, fallback, |_| {}).await
    }

    async fn with(script: Vec<Reply>, fallback: Reply, tune: impl FnOnce(&mut MeshConfig)) -> Rig {
        let api = Arc::new(mesh_api(script, fallback).await);
        let dir = tempfile::tempdir().unwrap();
        let mut config = config(&api, dir.path(), &[]);
        tune(&mut config);
        let mut metrics = Metrics::new("sidecar", "test");
        let mode_config = ModeConfig {
            mode: Mode::Discovery,
            state_file: dir.path().join("state").join("mode"),
            ..ModeConfig::default()
        };
        let mode = ModeState::start(&mode_config, &mut metrics).await;
        let state = MeshState::new(&config);
        state.register(&mut metrics);
        let follower = Follower::new(config, state, mode.clone())
            .unwrap()
            .expect("a target");
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            follower
                .run(async move {
                    let _ = stopped.wait_for(|s| *s).await;
                })
                .await;
        });
        Rig {
            api,
            mode,
            metrics: Arc::new(metrics),
            dir,
            stop,
            task,
        }
    }

    fn metric(&self, line: &str) -> Option<f64> {
        self.metrics
            .encode()
            .lines()
            .find(|l| l.starts_with(line))
            .and_then(|l| l.rsplit(' ').next()?.parse().ok())
    }

    fn pulls(&self, result: &str) -> u64 {
        self.metric(&format!(
            "mimic_mesh_view_pulls_total{{result=\"{result}\"}}"
        ))
        .unwrap_or(-1.0) as u64
    }

    fn services(&self) -> u64 {
        self.metric("mimic_mesh_view_services").unwrap_or(-1.0) as u64
    }

    fn state_file(&self) -> PathBuf {
        self.dir.path().join("state").join("mesh.json")
    }

    async fn stop(self) {
        let _ = self.stop.send(true);
        tokio::time::timeout(Duration::from_secs(2), self.task)
            .await
            .unwrap()
            .unwrap();
    }
}

async fn wait(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timeout: {what}");
}

fn two() -> String {
    view_json(
        &[
            ("shop/cart", "10.96.0.10", 80),
            ("shop/pay", "10.96.0.11", 80),
        ],
        "discovery",
        true,
        &[("mimicconfig/shop/cart", 3)],
    )
}

fn one() -> String {
    view_json(&[("shop/cart", "10.96.0.10", 80)], "discovery", true, &[])
}

#[tokio::test]
async fn msh_33_the_first_request_and_the_view_applied() {
    let logs = capture_logs();
    let rig = Rig::start(vec![Reply::View(two(), "\"1-aaa\"")], Reply::Hold).await;
    wait("two requests", || rig.api.count() >= 2).await;
    assert_eq!(rig.api.target(0), "/v1/workloads/pay/mesh?format=1");
    assert_eq!(rig.api.authorization(0), format!("Bearer {TOKEN}"));
    assert_eq!(rig.api.accept_encoding(0).as_deref(), Some("gzip"));
    assert_eq!(rig.api.if_none_match(0), None);
    // The next request carries the ETag, after 100 ms at least.
    assert_eq!(rig.api.if_none_match(1).as_deref(), Some("\"1-aaa\""));
    assert!(rig.api.at(1) - rig.api.at(0) >= Duration::from_millis(100));
    assert_eq!(rig.services(), 2);
    assert_eq!(rig.metric("mimic_mesh_view_endpoints"), Some(2.0));
    assert_eq!(rig.pulls("ok"), 1);
    // Every series exists from the start.
    for result in ["not_modified", "older", "invalid", "error"] {
        assert_eq!(rig.pulls(result), 0, "{result}");
    }
    logs.settled().await;
    let applied: Vec<serde_json::Value> = logs
        .text()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter(|v: &serde_json::Value| v["message"] == "mesh view applied")
        .collect();
    assert_eq!(applied.len(), 1, "{}", logs.text());
    assert_eq!(applied[0]["level"], "INFO");
    assert_eq!(applied[0]["services"], 2);
    assert_eq!(applied[0]["endpoints"], 2);
    assert_eq!(applied[0]["etag"], "\"1-aaa\"");
    assert!(!logs.text().contains(TOKEN));
    rig.stop().await;
}

#[tokio::test]
async fn msh_33_a_gzip_body_is_read() {
    let rig = Rig::start(vec![Reply::Gzip(two(), "\"1-gz\"")], Reply::Hold).await;
    wait("view", || rig.services() == 2).await;
    assert_eq!(rig.pulls("ok"), 1);
    rig.stop().await;
}

#[tokio::test]
async fn msh_33_a_304_keeps_the_view() {
    let rig = Rig::start(
        vec![
            Reply::View(two(), "\"1-aaa\""),
            Reply::NotModified,
            Reply::NotModified,
        ],
        Reply::Hold,
    )
    .await;
    wait("304s", || rig.pulls("not_modified") == 2).await;
    assert_eq!(rig.services(), 2);
    assert_eq!(rig.api.if_none_match(2).as_deref(), Some("\"1-aaa\""));
    rig.stop().await;
}

#[tokio::test]
async fn msh_33_the_token_is_read_for_each_request() {
    let rig = Rig::start(vec![], Reply::NotModified).await;
    wait("a request", || rig.api.count() >= 1).await;
    std::fs::write(rig.dir.path().join("token"), "rotated-token-2\n").unwrap();
    let seen = rig.api.count();
    wait("later requests", || rig.api.count() >= seen + 3).await;
    assert_eq!(
        rig.api.authorization(rig.api.count() - 1),
        "Bearer rotated-token-2"
    );
    assert_eq!(rig.api.authorization(0), format!("Bearer {TOKEN}"));
    rig.stop().await;
}

#[tokio::test]
async fn msh_33_an_invalid_view_is_refused_and_the_previous_one_kept() {
    let logs = capture_logs();
    let unknown = two().replacen("\"cluster\"", "\"surprise\":1,\"cluster\"", 1);
    // Over the bound after decompression (a gzip bomb), and over it as sent.
    let bomb = format!("{{\"cluster\":\"{}\"}}", "a".repeat(128 * 1024));
    let plain = bomb.clone();
    let rig = Rig::with(
        vec![
            Reply::View(two(), "\"1-aaa\""),
            Reply::View("not json".into(), "\"1-b\""),
            Reply::View(unknown, "\"1-c\""),
            Reply::Gzip(bomb, "\"1-d\""),
            Reply::View(plain, "\"1-e\""),
            Reply::View(one(), "\"1-f\""),
        ],
        Reply::Hold,
        |config| config.limits.max_view_bytes = 64 * 1024,
    )
    .await;
    wait("the good view after the bad ones", || rig.services() == 1).await;
    assert_eq!(rig.pulls("invalid"), 4);
    assert_eq!(rig.pulls("ok"), 2);
    // Each request after a refusal waits for the retry delay, and keeps the held ETag.
    for n in 2..=5 {
        assert!(
            rig.api.at(n) - rig.api.at(n - 1) >= Duration::from_millis(950),
            "request {n}"
        );
        assert_eq!(rig.api.if_none_match(n).as_deref(), Some("\"1-aaa\""));
    }
    logs.settled().await;
    // One warning for the streak of four.
    let rejected = logs
        .text()
        .lines()
        .filter(|l| l.contains("mesh view rejected"))
        .count();
    assert_eq!(rejected, 1, "{}", logs.text());
    rig.stop().await;
}

#[tokio::test]
async fn msh_33_errors_keep_the_view_and_the_delay_resets_after_a_200() {
    let rig = Rig::start(
        vec![
            Reply::View(two(), "\"1-aaa\""),
            Reply::Status(500),
            Reply::Reset,
            Reply::View(one(), "\"1-bbb\""),
        ],
        Reply::Hold,
    )
    .await;
    wait("the second view", || rig.services() == 1).await;
    assert_eq!(rig.pulls("error"), 2);
    // 1 s at least after the first failure, 1 s at least (up to 2) after the second.
    assert!(rig.api.at(2) - rig.api.at(1) >= Duration::from_millis(950));
    assert!(rig.api.at(3) - rig.api.at(2) >= Duration::from_millis(950));
    wait("the next request", || rig.api.count() >= 5).await;
    // After the 200 the delay is the 100 ms of a long-poll again, not the failure delay.
    assert!(rig.api.at(4) - rig.api.at(3) < Duration::from_millis(900));
    rig.stop().await;
}

#[tokio::test]
async fn msh_33_a_missing_route_and_retry_after() {
    let rig = Rig::start(
        vec![Reply::Status(404), Reply::RetryAfter(503, 2)],
        Reply::Hold,
    )
    .await;
    wait("three requests", || rig.api.count() >= 3).await;
    assert_eq!(rig.pulls("error"), 2);
    assert_eq!(rig.services(), 0);
    // The second failure: 1 s to 2 s by the backoff, 2 s by Retry-After.
    assert!(rig.api.at(2) - rig.api.at(1) >= Duration::from_millis(1950));
    rig.stop().await;
}

#[tokio::test]
async fn msh_33_the_stop_ends_a_held_request() {
    let rig = Rig::start(vec![], Reply::Hold).await;
    wait("a request", || rig.api.count() >= 1).await;
    let begun = Instant::now();
    rig.stop().await;
    assert!(begun.elapsed() < Duration::from_secs(1));
}

fn gens(a: i64) -> String {
    view_json(
        &[("shop/cart", "10.96.0.10", 80)],
        "discovery",
        true,
        &[("mimicconfig/shop/cart", a)],
    )
}

#[tokio::test]
async fn msh_34_an_older_generation_is_refused_and_the_view_kept() {
    let older = view_json(
        &[
            ("shop/cart", "10.96.0.10", 80),
            ("shop/pay", "10.96.0.11", 80),
        ],
        "discovery",
        true,
        &[("mimicconfig/shop/cart", 4)],
    );
    let rig = Rig::start(
        vec![
            Reply::View(gens(5), "\"1-aaa\""),
            Reply::View(older, "\"1-old\""),
        ],
        Reply::Hold,
    )
    .await;
    wait("the refusal", || rig.pulls("older") == 1).await;
    assert_eq!(rig.services(), 1, "the view held is kept");
    assert_eq!(rig.pulls("ok"), 1);
    wait("the next request", || rig.api.count() >= 3).await;
    // After the retry delay, with the ETag of the view held.
    assert!(rig.api.at(2) - rig.api.at(1) >= Duration::from_millis(950));
    assert_eq!(rig.api.if_none_match(2).as_deref(), Some("\"1-aaa\""));
    rig.stop().await;
}

#[tokio::test]
async fn msh_34_equal_higher_and_unknown_generations_are_applied() {
    let rig = Rig::start(
        vec![
            Reply::View(gens(5), "\"1-a\""),
            Reply::View(gens(5), "\"1-b\""),
            Reply::View(gens(6), "\"1-c\""),
            // An object the held view does not know.
            Reply::View(two(), "\"1-d\""),
        ],
        Reply::Hold,
    )
    .await;
    wait("four views", || rig.pulls("ok") == 4).await;
    assert_eq!(rig.pulls("older"), 0);
    assert_eq!(rig.services(), 2);
    rig.stop().await;
}

#[tokio::test]
async fn msh_34_a_service_generation_counts() {
    // The Service's slice goes from generation 1 to 0 in the second view.
    let lower = gens(1).replace(
        "\"endpointslice/shop/cart-x\":1",
        "\"endpointslice/shop/cart-x\":0",
    );
    let rig = Rig::start(
        vec![
            Reply::View(gens(1), "\"1-a\""),
            Reply::View(lower, "\"1-lower\""),
        ],
        Reply::Hold,
    )
    .await;
    wait("the refusal", || rig.pulls("older") == 1).await;
    assert_eq!(rig.pulls("ok"), 1);
    rig.stop().await;
}

#[tokio::test]
async fn msh_34_the_same_older_view_served_again_after_the_grace_is_applied() {
    let older = view_json(
        &[
            ("shop/cart", "10.96.0.10", 80),
            ("shop/pay", "10.96.0.11", 80),
        ],
        "discovery",
        true,
        &[("mimicconfig/shop/cart", 1)],
    );
    let rig = Rig::with(
        vec![Reply::View(gens(5), "\"1-aaa\"")],
        Reply::View(older, "\"1-recreated\""),
        |config| config.older_grace = Duration::from_millis(1500),
    )
    .await;
    wait("the refusal", || rig.pulls("older") >= 1).await;
    assert_eq!(rig.services(), 1);
    let refused_at = Instant::now();
    wait("the view is applied", || rig.services() == 2).await;
    assert!(refused_at.elapsed() < Duration::from_secs(4));
    rig.stop().await;
}

#[tokio::test]
async fn msh_35_the_mode_of_an_applied_view_is_applied() {
    let logs = capture_logs();
    let mock = view_json(&[("shop/cart", "10.96.0.10", 80)], "mock", true, &[]);
    let rig = Rig::start(
        vec![
            Reply::View(mock, "\"1-a\""),
            // Refused: a bad body with a mode in it changes nothing.
            Reply::View("{\"mode\":\"discovery\",x".into(), "\"1-bad\""),
        ],
        Reply::Hold,
    )
    .await;
    wait("mock", || rig.mode.mode() == Mode::Mock).await;
    wait("the refusal", || rig.pulls("invalid") == 1).await;
    assert_eq!(rig.mode.mode(), Mode::Mock);
    logs.settled().await;
    let changed = logs
        .text()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["message"] == "mode changed" && v["mode"] == "mock")
        .expect("mode changed");
    assert_eq!(changed["source"], "pull");
    assert_eq!(changed["previous"], "discovery");
    let saved = std::fs::read_to_string(rig.dir.path().join("state").join("mode")).unwrap();
    assert!(saved.contains("\"mode\":\"mock\""), "{saved}");
    rig.stop().await;

    // The view of a workload with the mesh off carries a mode as well.
    let off = view_json(&[], "none", false, &[]);
    let rig = Rig::start(vec![Reply::View(off, "\"1-n\"")], Reply::Hold).await;
    wait("none", || rig.mode.mode() == Mode::None).await;
    rig.stop().await;
}

#[tokio::test]
async fn msh_35_a_silent_controller_changes_no_mode_and_off_pulls_nothing() {
    let rig = Rig::start(vec![], Reply::Hold).await;
    wait("a request", || rig.api.count() >= 1).await;
    assert_eq!(rig.mode.mode(), Mode::Discovery);
    rig.stop().await;

    let api = mesh_api(vec![], Reply::Hold).await;
    let dir = tempfile::tempdir().unwrap();
    let config = config(&api, dir.path(), &[("MIMIC_MESH", "off")]);
    assert!(!config.enabled);
    let follower = Follower::new(config, MeshState::default(), ModeState::default()).unwrap();
    assert!(follower.is_none());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(api.count(), 0);
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test]
async fn msh_36_an_applied_view_is_persisted_atomically() {
    let rig = Rig::start(vec![Reply::View(two(), "\"1-aaa\"")], Reply::Hold).await;
    wait("the file", || rig.state_file().exists()).await;
    let text = std::fs::read_to_string(rig.state_file()).unwrap();
    let file: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(file["fetchedAt"].as_u64().unwrap().abs_diff(unix_now()) < 10);
    assert_eq!(file["view"]["mode"], "discovery");
    assert_eq!(file["view"]["services"].as_object().unwrap().len(), 2);
    let leftovers: Vec<_> = std::fs::read_dir(rig.dir.path().join("state"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.contains("tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    rig.stop().await;
}

async fn restored(dir: &std::path::Path, vars: &[(&str, &str)]) -> (MeshState, Arc<Metrics>) {
    let env = Env::from_pairs(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let config = MeshConfig::from_env(&env, dir).unwrap();
    let state = MeshState::new(&config);
    let mut metrics = Metrics::new("sidecar", "test");
    state.register(&mut metrics);
    state.restore().await;
    (state, Arc::new(metrics))
}

fn write_file(dir: &std::path::Path, text: &str) {
    std::fs::create_dir_all(dir.join("state")).unwrap();
    std::fs::write(dir.join("state").join("mesh.json"), text).unwrap();
}

fn file_of(view: &str, fetched_at: u64) -> String {
    format!("{{\"fetchedAt\":{fetched_at},\"view\":{view}}}")
}

fn value(metrics: &Metrics, name: &str) -> f64 {
    metrics
        .encode()
        .lines()
        .find(|l| l.starts_with(name))
        .and_then(|l| l.rsplit(' ').next()?.parse().ok())
        .unwrap_or(-1.0)
}

#[tokio::test]
async fn msh_36_a_persisted_view_is_reloaded_aged_and_goes_stale_once() {
    let logs = capture_logs();
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), &file_of(&two(), unix_now() - 120));
    let (state, metrics) = restored(dir.path(), &[("MIMIC_MESH_STALE_AFTER", "60s")]).await;
    assert_eq!(value(&metrics, "mimic_mesh_view_services"), 2.0);
    assert_eq!(value(&metrics, "mimic_mesh_view_endpoints"), 2.0);
    assert!(state.ready().is_ok());
    state.tick();
    state.tick();
    assert!(value(&metrics, "mimic_mesh_view_age_seconds") >= 119.0);
    logs.settled().await;
    let text = logs.text();
    assert_eq!(text.matches("mesh view stale").count(), 1, "{text}");
    let applied = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["message"] == "mesh view applied")
        .expect("mesh view applied");
    assert_eq!(applied["source"], "state");
}

#[tokio::test]
async fn msh_36_a_fresh_persisted_view_is_not_stale() {
    let logs = capture_logs();
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), &file_of(&two(), unix_now() - 5));
    let (state, metrics) = restored(dir.path(), &[]).await;
    state.tick();
    let age = value(&metrics, "mimic_mesh_view_age_seconds");
    assert!((4.0..30.0).contains(&age), "{age}");
    logs.settled().await;
    assert!(!logs.text().contains("mesh view stale"));
}

#[tokio::test]
async fn msh_36_a_file_that_does_not_parse_is_ignored_with_one_warning() {
    let big = format!(
        "{{\"fetchedAt\":1,\"view\":{{\"cluster\":\"{}\"}}}}",
        "a".repeat(80 * 1024)
    );
    let unknown = file_of(&two(), 1).replacen("\"cluster\"", "\"surprise\":1,\"cluster\"", 1);
    let cases = [
        ("garbage", "not json".to_string()),
        ("unknown field in the view", unknown),
        (
            "extra field in the file",
            format!("{{\"extra\":1,\"fetchedAt\":1,\"view\":{}}}", two()),
        ),
        ("no fetchedAt", format!("{{\"view\":{}}}", two())),
        ("over the bound", big),
    ];
    for (what, text) in cases {
        let logs = capture_logs();
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), &text);
        let (state, metrics) =
            restored(dir.path(), &[("MIMIC_MESH_MAX_VIEW_BYTES", "64Ki")]).await;
        assert_eq!(value(&metrics, "mimic_mesh_view_services"), 0.0, "{what}");
        let _ = &state;
        logs.settled().await;
        let warnings = logs
            .text()
            .lines()
            .filter(|l| l.contains("mesh state ignored"))
            .count();
        assert_eq!(warnings, 1, "{what}: {}", logs.text());
    }
    // Not a regular file.
    let logs = capture_logs();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("state").join("mesh.json")).unwrap();
    let (_state, metrics) = restored(dir.path(), &[]).await;
    assert_eq!(value(&metrics, "mimic_mesh_view_services"), 0.0);
    logs.settled().await;
    assert_eq!(logs.text().matches("mesh state ignored").count(), 1);
    // No file: nothing to say.
    let logs = capture_logs();
    let dir = tempfile::tempdir().unwrap();
    let _ = restored(dir.path(), &[]).await;
    logs.settled().await;
    assert!(!logs.text().contains("mesh state ignored"));
}
