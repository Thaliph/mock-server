//! JSON logs on stdout, one object per event (spec §9, FND-04).
//!
//! Lines go through a bounded in-memory queue drained by a worker thread: when stdout cannot
//! keep up, whole lines are dropped and counted (`mimic_log_lines_dropped_total`), so logging
//! never slows traffic (spec §4.1, PRX-12).

use std::io::Write;
use std::sync::OnceLock;

use tracing::Dispatch;
use tracing_appender::non_blocking::{ErrorCounter, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::ParseError;

/// Queue capacity in lines.
const QUEUE_LINES: usize = 8192;

/// Drop counter of the process logger, read by `Metrics` at scrape time.
static DROPPED: OnceLock<ErrorCounter> = OnceLock::new();

pub(crate) fn dropped_lines() -> u64 {
    DROPPED.get().map_or(0, |c| c.dropped_lines() as u64)
}

pub fn filter(spec: &str) -> Result<EnvFilter, ParseError> {
    EnvFilter::try_new(spec)
}

/// Keeps the log worker alive. Dropping it (or `finish`) flushes the queued lines.
#[must_use = "dropping the handle stops logging; keep it until the process exits"]
pub struct LogHandle {
    guard: Option<WorkerGuard>,
    counter: ErrorCounter,
}

impl LogHandle {
    /// Lines dropped so far because the queue was full.
    pub fn dropped_lines(&self) -> usize {
        self.counter.dropped_lines()
    }

    /// Flushes the queue and stops the worker.
    pub fn finish(&mut self) {
        self.guard = None;
    }
}

/// Builds the JSON subscriber writing to `writer` through the lossy queue.
pub fn build<W: Write + Send + 'static>(spec: &str, writer: W) -> (Dispatch, LogHandle) {
    let (non_blocking, guard) = NonBlockingBuilder::default()
        .buffered_lines_limit(QUEUE_LINES)
        .lossy(true)
        .finish(writer);
    let counter = non_blocking.error_counter();
    let _ = DROPPED.set(counter.clone());
    let subscriber = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_span_list(false)
        .with_env_filter(filter(spec).unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(non_blocking)
        .finish();
    (
        Dispatch::new(subscriber),
        LogHandle {
            guard: Some(guard),
            counter,
        },
    )
}

/// Installs the global subscriber. `spec` was validated by `CommonConfig::from_env`.
/// Keep the handle until exit so that queued lines are flushed.
pub fn init(spec: &str) -> LogHandle {
    let (dispatch, handle) = build(spec, std::io::stdout());
    // A second global subscriber is a programming error; keep the first one.
    let _ = tracing::dispatcher::set_global_default(dispatch);
    handle
}
