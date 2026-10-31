//! Helpers for integration tests that start a real mimic binary (feature `testing`).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

/// A mimic process started for a test, with its stdout lines collected.
pub struct Process {
    child: Child,
    stdout: Receiver<String>,
    lines: Vec<String>,
}

impl Process {
    /// Starts `bin` with only `PATH` and the given variables in its environment.
    pub fn spawn(bin: &str, env: &[(&str, &str)]) -> Process {
        let mut command = Command::new(bin);
        command
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (k, v) in env {
            command.env(k, v);
        }
        let mut child = command.spawn().expect("binary starts");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Process {
            child,
            stdout: rx,
            lines: Vec::new(),
        }
    }

    /// Waits for the first log line (parsed as JSON) that satisfies `pred`.
    pub fn wait_log(&mut self, timeout: Duration, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(found) = self.logs().into_iter().find(|v| pred(v)) {
            return found;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.stdout.recv_timeout(left) {
                Ok(line) => {
                    self.lines.push(line.clone());
                    if let Ok(v) = serde_json::from_str::<Value>(&line)
                        && pred(&v)
                    {
                        return v;
                    }
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                    panic!(
                        "no matching log line; stdout so far:\n{}",
                        self.lines.join("\n")
                    )
                }
            }
        }
    }

    /// Waits for the "admin listening" line and returns the bound address.
    pub fn admin_addr(&mut self) -> SocketAddr {
        let line = self.wait_log(Duration::from_secs(10), |v| {
            v["message"] == "admin listening"
        });
        line["addr"].as_str().expect("addr field").parse().unwrap()
    }

    /// Raw stdout lines received so far.
    pub fn lines(&mut self) -> &[String] {
        while let Ok(line) = self.stdout.try_recv() {
            self.lines.push(line);
        }
        &self.lines
    }

    /// Stdout lines received so far that parse as JSON.
    pub fn logs(&mut self) -> Vec<Value> {
        self.lines()
            .iter()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Process id of the child.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Sends `SIGTERM`.
    pub fn terminate(&self) {
        let status = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Waits for the process to exit, then drains its stdout.
    pub fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                // The reader thread ends with the pipe; give it a moment to deliver the tail.
                thread::sleep(Duration::from_millis(50));
                self.lines();
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "process still running after {timeout:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs `bin` to completion and returns its exit status, stdout and stderr.
pub fn run(bin: &str, args: &[&str], env: &[(&str, &str)]) -> (ExitStatus, String, String) {
    let mut command = Command::new(bin);
    command
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default());
    for (k, v) in env {
        command.env(k, v);
    }
    let out = command.output().expect("binary runs");
    (
        out.status,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Minimal HTTP/1.1 GET; returns the status code and the body.
pub fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status line");
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, body)
}

/// Polls `GET path` until it answers `want` or the timeout expires.
pub fn wait_status(addr: SocketAddr, path: &str, want: u16, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut last = 0;
    while Instant::now() < deadline {
        last = http_get(addr, path).0;
        if last == want {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("GET {path}: wanted {want}, last status {last}");
}
