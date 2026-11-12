//! MSH-31, MSH-35, MSH-36 and MSH-37 on the real binary against a fake sidecar API: exit codes,
//! readiness, and a restart with the controller silent (spec §7.9, §8 "Mode state").

#[path = "common/mesh_api.rs"]
mod mesh_api;

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mesh_api::{MeshApi, Reply, mesh_api, view_json};
use mimic_runtime::testing::{Process, http_get, run, wait_status};

const BIN: &str = env!("CARGO_BIN_EXE_mimic-sidecar");

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn two() -> String {
    view_json(
        &[
            ("shop/cart", "10.96.0.10", 80),
            ("shop/pay", "10.96.0.11", 80),
        ],
        "none",
        true,
        &[],
    )
}

fn persist(dir: &Path, view: &str, fetched_at: u64) {
    std::fs::create_dir_all(dir.join("state")).unwrap();
    std::fs::write(
        dir.join("state").join("mesh.json"),
        format!("{{\"fetchedAt\":{fetched_at},\"view\":{view}}}"),
    )
    .unwrap();
}

fn sidecar(api: &MeshApi, dir: &Path, extra: &[(&str, &str)]) -> Process {
    let token = dir.join("token");
    std::fs::write(&token, "token").unwrap();
    let url = format!("https://{}", api.addr);
    let mut env = vec![
        ("MIMIC_ADMIN_ADDR", "127.0.0.1:0"),
        ("MIMIC_INBOUND_ADDRS", "127.0.0.1:0"),
        ("MIMIC_OUTBOUND_ADDRS", "127.0.0.1:0"),
        ("MIMIC_CONTROLLER_URL", &url),
        ("MIMIC_CONTROLLER_CA", &api.ca_pem),
        ("MIMIC_TLS_TOKEN_FILE", token.to_str().unwrap()),
        ("MIMIC_TRUST_DIR", dir.to_str().unwrap()),
        ("MIMIC_WORKLOAD", "pay"),
        ("MIMIC_MESH", "on"),
    ];
    // A later entry of the same name wins: the child reads its environment once.
    env.extend_from_slice(extra);
    Process::spawn(BIN, &env)
}

fn metric(admin: std::net::SocketAddr, name: &str) -> Option<f64> {
    let (_, body) = http_get(admin, "/metrics");
    body.lines()
        .find(|l| l.starts_with(name))
        .and_then(|l| l.rsplit(' ').next()?.parse().ok())
}

#[test]
fn msh_31_an_invalid_variable_exits_2() {
    for (var, value) in [
        ("MIMIC_MESH", "maybe"),
        ("MIMIC_MESH_STALE_AFTER", "0s"),
        ("MIMIC_MESH_STARTUP_WAIT", "2h"),
        ("MIMIC_MESH_MAX_SERVICES", "0"),
    ] {
        let (status, _, stderr) = run(BIN, &[], &[(var, value)]);
        assert_eq!(status.code(), Some(2), "{var}={value}");
        assert!(stderr.contains(var), "{stderr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn msh_37_readiness_waits_for_the_first_view_or_the_startup_wait() {
    // A controller that never answers: ready after the wait.
    let dir = tempfile::tempdir().unwrap();
    let api = mesh_api(vec![], Reply::Hold).await;
    let mut process = sidecar(&api, dir.path(), &[("MIMIC_MESH_STARTUP_WAIT", "3s")]);
    let admin = process.admin_addr();
    let begun = Instant::now();
    let (status, _) = http_get(admin, "/readyz");
    assert_eq!(status, 503);
    wait_status(admin, "/readyz", 200, Duration::from_secs(10));
    assert!(
        begun.elapsed() >= Duration::from_secs(1),
        "{:?}",
        begun.elapsed()
    );
    drop(process);

    // A view arrives: ready long before a startup wait of 30 s.
    let dir = tempfile::tempdir().unwrap();
    let api = mesh_api(vec![Reply::View(two(), "\"1-a\"")], Reply::Hold).await;
    let mut process = sidecar(&api, dir.path(), &[("MIMIC_MESH_STARTUP_WAIT", "30s")]);
    let admin = process.admin_addr();
    wait_status(admin, "/readyz", 200, Duration::from_secs(5));
    assert_eq!(metric(admin, "mimic_mesh_view_services"), Some(2.0));
    drop(process);

    // A persisted view: ready at once.
    let dir = tempfile::tempdir().unwrap();
    persist(dir.path(), &two(), unix_now());
    let api = mesh_api(vec![], Reply::Hold).await;
    let mut process = sidecar(&api, dir.path(), &[("MIMIC_MESH_STARTUP_WAIT", "30s")]);
    let admin = process.admin_addr();
    assert_eq!(http_get(admin, "/readyz").0, 200);
    drop(process);

    // The mesh off: no wait, and no request to the route.
    let dir = tempfile::tempdir().unwrap();
    let api = mesh_api(vec![], Reply::Hold).await;
    let mut process = sidecar(
        &api,
        dir.path(),
        &[("MIMIC_MESH", "off"), ("MIMIC_MESH_STARTUP_WAIT", "30s")],
    );
    let admin = process.admin_addr();
    assert_eq!(http_get(admin, "/readyz").0, 200);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(api.count(), 0);
    assert_eq!(metric(admin, "mimic_mesh_view_services"), None);
    drop(process);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn msh_36_a_restart_with_a_silent_controller_keeps_the_view_and_the_mode() {
    let dir = tempfile::tempdir().unwrap();
    // The last view said `none`; the mode state file, written before it, says `mock`.
    persist(dir.path(), &two(), unix_now() - 30);
    std::fs::write(
        dir.path().join("state").join("mode"),
        r#"{"latency_factor":1,"mode":"mock"}"#,
    )
    .unwrap();
    let api = mesh_api(vec![], Reply::Hold).await;
    let mut process = sidecar(&api, dir.path(), &[("MIMIC_MODE", "discovery")]);
    let admin = process.admin_addr();
    wait_status(admin, "/readyz", 200, Duration::from_secs(5));
    assert_eq!(metric(admin, "mimic_mesh_view_services"), Some(2.0));
    assert_eq!(metric(admin, "mimic_mesh_view_endpoints"), Some(2.0));
    assert_eq!(metric(admin, "mimic_mode{mode=\"mock\"}"), Some(1.0));
    assert_eq!(metric(admin, "mimic_mode{mode=\"none\"}"), Some(0.0));
    // Aged by `fetchedAt`, not by the restart.
    let mut age = 0.0;
    for _ in 0..30 {
        age = metric(admin, "mimic_mesh_view_age_seconds").unwrap_or(0.0);
        if age >= 29.0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(age >= 29.0, "{age}");
    process.terminate();
    assert!(process.wait_exit(Duration::from_secs(10)).success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn msh_35_without_a_view_the_mode_is_the_one_of_the_state_file_then_the_environment() {
    // No state file: the environment; a controller that does not answer changes nothing.
    let dir = tempfile::tempdir().unwrap();
    let api = mesh_api(vec![], Reply::Hold).await;
    let mut process = sidecar(&api, dir.path(), &[("MIMIC_MODE", "mock")]);
    let admin = process.admin_addr();
    wait_status(admin, "/readyz", 200, Duration::from_secs(10));
    assert_eq!(metric(admin, "mimic_mode{mode=\"mock\"}"), Some(1.0));
    drop(process);

    // A view with another mode is applied.
    let dir = tempfile::tempdir().unwrap();
    let api = mesh_api(vec![Reply::View(two(), "\"1-a\"")], Reply::Hold).await;
    let mut process = sidecar(&api, dir.path(), &[("MIMIC_MODE", "mock")]);
    let admin = process.admin_addr();
    for _ in 0..100 {
        if metric(admin, "mimic_mode{mode=\"none\"}") == Some(1.0) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(metric(admin, "mimic_mode{mode=\"none\"}"), Some(1.0));
}
