//! FND-01, FND-04, FND-05, FND-06, FND-07 on the real `mimic-controller` binary.
//!
//! Valkey is replaced by a minimal RESP server that answers `PING` with `PONG` and anything
//! else with `OK`, and records the commands it receives.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use mimic_runtime::testing::{Process, http_get, run, wait_status};

const BIN: &str = env!("CARGO_BIN_EXE_mimic-controller");
const ADMIN: (&str, &str) = ("MIMIC_ADMIN_ADDR", "127.0.0.1:0");

struct FakeValkey {
    addr: SocketAddr,
    commands: Arc<Mutex<Vec<Vec<String>>>>,
    stop: Arc<AtomicBool>,
    clients: Arc<Mutex<Vec<TcpStream>>>,
}

impl FakeValkey {
    fn start(addr: SocketAddr) -> FakeValkey {
        let listener = TcpListener::bind(addr).unwrap();
        listener.set_nonblocking(true).unwrap();
        let fake = FakeValkey {
            addr: listener.local_addr().unwrap(),
            commands: Arc::default(),
            stop: Arc::default(),
            clients: Arc::default(),
        };
        let (commands, stop, clients) = (
            fake.commands.clone(),
            fake.stop.clone(),
            fake.clients.clone(),
        );
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        clients.lock().unwrap().push(stream.try_clone().unwrap());
                        let commands = commands.clone();
                        thread::spawn(move || serve(stream, commands));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        fake
    }

    fn stop(self) -> SocketAddr {
        self.stop.store(true, Ordering::SeqCst);
        for client in self.clients.lock().unwrap().iter() {
            let _ = client.shutdown(Shutdown::Both);
        }
        // Let the accept loop notice the flag and drop the listener.
        thread::sleep(Duration::from_millis(100));
        self.addr
    }
}

fn serve(stream: TcpStream, commands: Arc<Mutex<Vec<Vec<String>>>>) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    while let Some(command) = read_command(&mut reader) {
        let reply: &[u8] = if command
            .first()
            .is_some_and(|c| c.eq_ignore_ascii_case("PING"))
        {
            b"+PONG\r\n"
        } else {
            b"+OK\r\n"
        };
        commands.lock().unwrap().push(command);
        if writer.write_all(reply).is_err() {
            return;
        }
    }
}

fn read_command(reader: &mut BufReader<TcpStream>) -> Option<Vec<String>> {
    let mut line = String::new();
    reader.read_line(&mut line).ok().filter(|n| *n > 0)?;
    let count: usize = line.trim().strip_prefix('*')?.parse().ok()?;
    let mut args = Vec::with_capacity(count);
    for _ in 0..count {
        line.clear();
        reader.read_line(&mut line).ok()?;
        let len: usize = line.trim().strip_prefix('$')?.parse().ok()?;
        let mut buf = vec![0; len + 2];
        reader.read_exact(&mut buf).ok()?;
        buf.truncate(len);
        args.push(String::from_utf8_lossy(&buf).into_owned());
    }
    Some(args)
}

fn free_addr() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

#[test]
fn fnd_01_missing_valkey_url_exits_2() {
    let (status, _, stderr) = run(BIN, &[], &[ADMIN]);
    assert_eq!(status.code(), Some(2));
    assert!(stderr.contains("MIMIC_VALKEY_URL"), "{stderr}");
}

#[test]
fn fnd_01_invalid_valkey_url_exits_2() {
    let (status, _, stderr) = run(BIN, &[], &[ADMIN, ("MIMIC_VALKEY_URL", "http://nope")]);
    assert_eq!(status.code(), Some(2));
    assert!(
        stderr.contains("MIMIC_VALKEY_URL") && stderr.contains("http://nope"),
        "{stderr}"
    );
}

#[test]
fn fnd_01_credentials_in_the_url_are_refused() {
    let (status, _, stderr) = run(
        BIN,
        &[],
        &[
            ADMIN,
            ("MIMIC_VALKEY_URL", "redis://:hunter2@127.0.0.1:6379"),
        ],
    );
    assert_eq!(status.code(), Some(2));
    assert!(stderr.contains("MIMIC_VALKEY_PASSWORD"), "{stderr}");
    assert!(!stderr.contains("hunter2"), "{stderr}");
}

#[test]
fn fnd_06_ready_only_while_valkey_answers() {
    let valkey_addr = free_addr();
    let url = format!("redis://{valkey_addr}");
    let mut controller = Process::spawn(BIN, &[ADMIN, ("MIMIC_VALKEY_URL", &url)]);
    let addr = controller.admin_addr();

    // Valkey down at start: alive but not ready, and still running.
    assert_eq!(http_get(addr, "/healthz").0, 200);
    wait_status(addr, "/readyz", 503, Duration::from_secs(5));

    let valkey = FakeValkey::start(valkey_addr);
    wait_status(addr, "/readyz", 200, Duration::from_secs(10));

    let valkey_addr = valkey.stop();
    wait_status(addr, "/readyz", 503, Duration::from_secs(10));
    assert_eq!(http_get(addr, "/healthz").0, 200);

    let _valkey = FakeValkey::start(valkey_addr);
    wait_status(addr, "/readyz", 200, Duration::from_secs(10));

    controller.wait_log(Duration::from_secs(1), |v| {
        v["level"] == "WARN" && v["message"] == "valkey unreachable"
    });
    controller.wait_log(Duration::from_secs(1), |v| {
        v["message"] == "valkey reachable"
    });
}

#[test]
fn fnd_06_the_password_is_sent_and_never_logged() {
    let valkey = FakeValkey::start("127.0.0.1:0".parse().unwrap());
    let url = format!("redis://{}", valkey.addr);
    let mut controller = Process::spawn(
        BIN,
        &[
            ADMIN,
            ("MIMIC_VALKEY_URL", &url),
            ("MIMIC_VALKEY_PASSWORD", "hunter2"),
            ("MIMIC_LOG", "trace"),
        ],
    );
    let addr = controller.admin_addr();
    wait_status(addr, "/readyz", 200, Duration::from_secs(10));

    let commands = valkey.commands.lock().unwrap().clone();
    assert!(
        commands.iter().any(
            |c| c.first().is_some_and(|n| n.eq_ignore_ascii_case("AUTH"))
                && c.last().is_some_and(|p| p == "hunter2")
        ),
        "{commands:?}"
    );
    controller.terminate();
    controller.wait_exit(Duration::from_secs(10));
    assert!(controller.lines().iter().all(|l| !l.contains("hunter2")));
}

#[test]
fn fnd_05_sigterm_exits_0() {
    let valkey = FakeValkey::start("127.0.0.1:0".parse().unwrap());
    let url = format!("redis://{}", valkey.addr);
    let mut controller = Process::spawn(
        BIN,
        &[
            ADMIN,
            ("MIMIC_VALKEY_URL", &url),
            ("MIMIC_SHUTDOWN_GRACE", "2s"),
        ],
    );
    let addr = controller.admin_addr();
    wait_status(addr, "/readyz", 200, Duration::from_secs(10));
    controller.terminate();
    assert!(controller.wait_exit(Duration::from_secs(3)).success());
    controller.wait_log(Duration::from_secs(1), |v| {
        v["message"] == "shutdown complete"
    });
}

#[test]
fn fnd_04_controller_logs_are_json() {
    let mut controller = Process::spawn(BIN, &[ADMIN, ("MIMIC_VALKEY_URL", "redis://127.0.0.1:1")]);
    controller.admin_addr();
    controller.terminate();
    controller.wait_exit(Duration::from_secs(10));
    for line in controller.lines() {
        let v: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert!(
            v.get("timestamp").is_some() && v.get("message").is_some(),
            "{line}"
        );
    }
}

#[test]
fn fnd_07_version_flag_prints_the_crate_version() {
    let (status, stdout, _) = run(BIN, &["--version"], &[]);
    assert!(status.success());
    assert_eq!(
        stdout.trim(),
        format!("mimic-controller {}", env!("CARGO_PKG_VERSION"))
    );
}
