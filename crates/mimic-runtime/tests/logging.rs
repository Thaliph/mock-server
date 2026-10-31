//! PRX-12 (log part): lossy, non-blocking stdout logging with a drop counter.

use std::io::Write;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use mimic_runtime::logging;
use mimic_runtime::metrics::Metrics;

/// A writer that blocks until the gate opens, like a stdout nobody reads.
#[derive(Clone, Default)]
struct Gated {
    open: Arc<(Mutex<bool>, Condvar)>,
    out: Arc<Mutex<Vec<u8>>>,
}

impl Gated {
    fn open(&self) {
        *self.open.0.lock().unwrap() = true;
        self.open.1.notify_all();
    }
}

impl Write for Gated {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let (lock, cvar) = &*self.open;
        let mut open = lock.lock().unwrap();
        while !*open {
            open = cvar.wait(open).unwrap();
        }
        self.out.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn prx_12_unread_stdout_never_blocks_and_drops_are_counted() {
    let writer = Gated::default();
    let (dispatch, mut handle) = logging::build("info", writer.clone());
    let metrics = Metrics::new("sidecar", "1.2.3");
    assert!(metrics.encode().contains("mimic_log_lines_dropped_total 0"));

    let started = Instant::now();
    tracing::dispatcher::with_default(&dispatch, || {
        for i in 0..100_000u32 {
            tracing::info!(i, "access");
        }
    });
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "logging blocked on a stalled writer"
    );

    let dropped = handle.dropped_lines();
    assert!(dropped > 0, "no line dropped");
    writer.open();
    // Flushes the queue and stops the worker.
    drop(dispatch);
    handle.finish();

    let text = String::from_utf8(writer.out.lock().unwrap().clone()).unwrap();
    let mut written = 0u64;
    for line in text.lines() {
        let v: serde_json::Value = serde_json::from_str(line).expect("truncated line");
        assert_eq!(v["message"], "access");
        written += 1;
    }
    let dropped = handle.dropped_lines() as u64;
    assert_eq!(written + dropped, 100_000);
    assert!(
        metrics
            .encode()
            .contains(&format!("mimic_log_lines_dropped_total {dropped}")),
        "{}",
        metrics.encode()
    );
}
