//! GW-19, the memory budget of the gateway role (spec §7.10, #574): with the configuration of the
//! generated pod, a client holds the cap of connections in the three costliest shapes, and the
//! process stays inside the budget of `gateway.resources`. The criterion names the release build
//! (`cargo test --release -p mimic-sidecar --test gateway_budget -- --nocapture`); the debug
//! build of `cargo test` checks the same bounds.
//!
//! When `systemd-run --user` works, the process runs in a scope with the pod's memory limit, as
//! in the pod; elsewhere (a CI runner without a user manager) the RSS bounds alone apply.

mod common;

use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;

use common::capture_logs;
use common::gateway_rig::{BIN, closed, connect, free_port, server_connections, server_holds};
use common::h2_frames::{self, DATA, END_HEADERS, HEADERS};
use mimic_runtime::testing::{Process, wait_status};

/// `gateway.maxConnections`, the cap of the generated pod.
const CAP: usize = 1024;
/// `limits.memory` of the default `gateway.resources`.
const LIMIT: u64 = 192 << 20;
/// The budget of §7.10: per held connection, and for the process itself.
const PER_CONNECTION: u64 = 96 << 10;
const BASE: u64 = 32 << 20;

/// What each held connection has sent.
#[derive(Debug, Clone, Copy)]
enum Held {
    /// A 63 KiB HTTP/1 head without its end.
    Head,
    /// The HTTP/2 preface and `SETTINGS`, no `HEADERS`.
    Preface,
    /// One HTTP/2 stream with 64 KiB of data the server does not read (the client's whole
    /// initial window).
    Stream,
}

impl Held {
    fn bytes(self) -> Vec<u8> {
        match self {
            Held::Head => {
                let mut head = b"GET / HTTP/1.1\r\nHost: a\r\nx-big: ".to_vec();
                head.resize(63 * 1024, b'a');
                head
            }
            Held::Preface => h2_frames::client_start(),
            Held::Stream => {
                let mut out = h2_frames::client_start();
                out.extend(h2_frames::frame(
                    HEADERS,
                    END_HEADERS,
                    1,
                    h2_frames::GET_ROOT,
                ));
                let mut left = 65_535usize;
                while left > 0 {
                    let n = left.min(16_384);
                    out.extend(h2_frames::frame(DATA, 0, 1, &vec![b'x'; n]));
                    left -= n;
                }
                out
            }
        }
    }
}

/// `VmRSS` and `VmHWM` of `pid`, in bytes.
fn rss(pid: u32) -> (u64, u64) {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let field = |name: &str| -> u64 {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .expect("a status field")
            * 1024
    };
    (field("VmRSS:"), field("VmHWM:"))
}

/// The environment `systemd-run --user` needs to reach the user manager.
fn user_manager_env() -> Vec<(&'static str, String)> {
    ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k, v)))
        .collect()
}

/// The arguments that run a command in a scope with the pod's memory limit, when this host can.
fn scope_args() -> Option<Vec<String>> {
    let args: Vec<String> = [
        "--user",
        "--scope",
        "--quiet",
        "-p",
        &format!("MemoryMax={LIMIT}"),
        "-p",
        "MemorySwapMax=0",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let mut probe = std::process::Command::new("systemd-run");
    probe
        .args(&args)
        .arg("true")
        .env_clear()
        .envs(user_manager_env())
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    probe.status().ok().filter(|s| s.success()).map(|_| args)
}

/// The gateway of the generated pod on `port`: `MIMIC_MAX_CONNECTIONS` at the cap, and timeouts
/// long enough that no connection closes during the measure.
fn pod_gateway(port: u16) -> (Process, bool) {
    let port = port.to_string();
    let manager = user_manager_env();
    let mut env: Vec<(&str, &str)> = vec![
        ("MIMIC_ADMIN_ADDR", "127.0.0.1:0"),
        ("MIMIC_ROLE", "gateway"),
        ("MIMIC_GATEWAY_PORTS", port.as_str()),
        ("MIMIC_MAX_CONNECTIONS", "1024"),
        ("MIMIC_GATEWAY_HEADER_TIMEOUT", "60s"),
        ("MIMIC_GATEWAY_IDLE_TIMEOUT", "60s"),
    ];
    let mut process = match scope_args() {
        Some(scope) => {
            env.extend(manager.iter().map(|(k, v)| (*k, v.as_str())));
            let mut args: Vec<&str> = scope.iter().map(String::as_str).collect();
            args.push(BIN);
            (Process::spawn_args("systemd-run", &args, &env), true)
        }
        None => (Process::spawn(BIN, &env), false),
    };
    let admin = process.0.admin_addr();
    wait_status(admin, "/readyz", 200, Duration::from_secs(10));
    process
}

/// One run: the cap of connections held as `held`; returns the RSS with no connection and the
/// RSS per held connection.
fn run(held: Held) -> (u64, u64) {
    let port = free_port();
    let (gateway, scoped) = pod_gateway(port);
    // The allocator and the runtime settle after the first `/readyz`.
    std::thread::sleep(Duration::from_millis(300));
    let (base, _) = rss(gateway.pid());

    let bytes = held.bytes();
    let mut open: Vec<TcpStream> = Vec::with_capacity(CAP);
    for _ in 0..CAP {
        let mut c = connect(port);
        c.write_all(&bytes).expect("the bytes are sent");
        open.push(c);
    }
    server_holds(port, CAP, Duration::from_secs(30));
    std::thread::sleep(Duration::from_millis(300));
    let (now, peak) = rss(gateway.pid());
    assert_eq!(
        server_connections(port),
        CAP,
        "{held:?}: a connection closed"
    );

    let mut over = connect(port);
    over.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    assert!(
        closed(&mut over),
        "{held:?}: the connection over the cap is open"
    );

    let per = now.saturating_sub(base) / CAP as u64;
    println!(
        "gw_19 budget {held:?} (scoped: {scoped}): base RSS {} KiB, RSS {} KiB, peak {} KiB, \
         {} B per held connection",
        base >> 10,
        now >> 10,
        peak >> 10,
        per
    );
    assert!(
        peak < LIMIT / 4 * 3,
        "{held:?}: peak RSS {} KiB over 75% of the limit",
        peak >> 10
    );
    drop(open);
    (base, per)
}

#[test]
fn gw_19_the_cap_of_held_connections_fits_the_memory_budget() {
    let _logs = capture_logs();
    for held in [Held::Head, Held::Preface, Held::Stream] {
        let (base, per) = run(held);
        assert!(base <= BASE, "{held:?}: base RSS {} KiB", base >> 10);
        assert!(
            per <= PER_CONNECTION,
            "{held:?}: {per} B per held connection"
        );
    }
}
