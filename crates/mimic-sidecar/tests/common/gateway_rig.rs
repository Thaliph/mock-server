//! Helpers of the gateway role tests (GW-18 to GW-20): free ports, the process, raw HTTP/1
//! answers and the kernel's view of the sockets.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use mimic_runtime::testing::{Process, wait_status};

pub const BIN: &str = env!("CARGO_BIN_EXE_mimic-sidecar");

/// A port no socket holds now: bound on `0.0.0.0:0`, read, closed.
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The comma-separated value of `MIMIC_GATEWAY_PORTS` for `ports`.
pub fn ports_value(ports: &[u16]) -> String {
    ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// A gateway on `ports` with `extra`, ready (`/readyz` 200); returns it with its admin address.
pub fn gateway(ports: &[u16], extra: &[(&str, &str)]) -> (Process, SocketAddr) {
    let ports = ports_value(ports);
    let mut env = vec![
        ("MIMIC_ADMIN_ADDR", "127.0.0.1:0"),
        ("MIMIC_ROLE", "gateway"),
        ("MIMIC_GATEWAY_PORTS", ports.as_str()),
    ];
    env.extend_from_slice(extra);
    let mut process = Process::spawn(BIN, &env);
    let admin = process.admin_addr();
    wait_status(admin, "/readyz", 200, Duration::from_secs(5));
    (process, admin)
}

/// Runs the binary with only `PATH` and `env`, like `testing::run`, but kills it and fails
/// after 10 s: a value wrongly accepted starts a gateway that never exits.
pub fn run_bounded(env: &[(&str, &str)]) -> (Option<i32>, String, String) {
    use std::process::{Command, Stdio};
    let mut command = Command::new(BIN);
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        command.env(k, v);
    }
    let mut child = command.spawn().expect("binary starts");
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("still running after 10 s: {env:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    (status.code(), out.join().unwrap(), err.join().unwrap())
}

pub fn local(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

pub fn connect(port: u16) -> TcpStream {
    let stream = TcpStream::connect_timeout(&local(port), Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
}

/// An HTTP/1 answer head as the client read it.
#[derive(Debug)]
pub struct Head {
    pub status: u16,
    /// Every field, the name in lower case, in the order received.
    pub fields: Vec<(String, String)>,
    /// Bytes read after the head.
    pub rest: Vec<u8>,
}

impl Head {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Exactly `x-mimic-error: no-route` and `content-length: 0`, then nothing, plus `extra`.
    pub fn assert_no_route(&self, extra: &[(&str, &str)]) {
        assert_eq!(self.status, 404, "{self:?}");
        let mut fields = self.fields.clone();
        fields.sort();
        let mut want: Vec<(String, String)> =
            [("content-length", "0"), ("x-mimic-error", "no-route")]
                .iter()
                .chain(extra)
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect();
        want.sort();
        assert_eq!(fields, want, "{self:?}");
        assert!(self.rest.is_empty(), "{self:?}");
    }
}

/// Reads one answer head from `stream` (an error at EOF before it ends).
pub fn read_head(stream: &mut TcpStream) -> io::Result<Head> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .expect("status line");
    let fields = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Ok(Head {
        status,
        fields,
        rest: buf[end + 4..].to_vec(),
    })
}

/// Sends `GET path` (keep-alive) and reads the answer head.
pub fn get(stream: &mut TcpStream, path: &str) -> Head {
    write!(stream, "GET {path} HTTP/1.1\r\nHost: a\r\n\r\n").unwrap();
    read_head(stream).expect("an answer")
}

/// True when the peer closed `stream` (a FIN or a reset); false at the read timeout.
pub fn closed(stream: &mut TcpStream) -> bool {
    let mut buf = [0u8; 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return false;
            }
            Err(_) => return true,
        }
    }
}

/// Waits for `stream` to be closed by the peer; the time since `start`. Panics after 5 s.
pub fn closed_after(stream: &mut TcpStream, start: Instant) -> Duration {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert!(closed(stream), "still open after 5 s");
    start.elapsed()
}

/// The sockets of `/proc/net/tcp` (IPv4) as `(local port, remote port, state, rx_queue)`.
fn tcp_sockets() -> Vec<(u16, u16, u8, u32)> {
    let table = std::fs::read_to_string("/proc/net/tcp").unwrap();
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut cols = line.split_whitespace().skip(1);
            let port = |addr: &str| u16::from_str_radix(addr.rsplit(':').next()?, 16).ok();
            let local = port(cols.next()?)?;
            let remote = port(cols.next()?)?;
            let state = u8::from_str_radix(cols.next()?, 16).ok()?;
            let rx = u32::from_str_radix(cols.next()?.split(':').nth(1)?, 16).ok()?;
            Some((local, remote, state, rx))
        })
        .collect()
}

const LISTEN: u8 = 0x0a;

/// Waits (at most 5 s) until no connection waits in the accept queue of the IPv4 listener on
/// `port`: the `rx_queue` of a listening socket is its accept backlog.
pub fn accept_queue_empty(port: u16) {
    wait_kernel(&format!("the accept queue of {port} drained"), || {
        tcp_sockets()
            .iter()
            .any(|&(l, _, state, rx)| l == port && state == LISTEN && rx == 0)
    });
}

/// Waits (at most 5 s) until the server side of `client` (connected to `port`) has read every
/// byte the client sent.
pub fn server_read_all(port: u16, client: &TcpStream) {
    let peer = client.local_addr().unwrap().port();
    wait_kernel(&format!("the server read {peer}"), || {
        tcp_sockets()
            .iter()
            .any(|&(l, r, state, rx)| l == port && r == peer && state != LISTEN && rx == 0)
    });
}

fn wait_kernel(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("never: {what}");
}
