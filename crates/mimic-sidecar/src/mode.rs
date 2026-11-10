//! The mode state of the sidecar (spec §3, §8 "Mode state"): `discovery` or `mock`, with the
//! latency factor. It starts from the state file of the pod, else from `MIMIC_MODE`, and
//! follows the `Mimic-Mode` and `Mimic-Latency-Factor` headers of the egress pulls (§7.7). The
//! mock (`crate::mock`) reads it when an outbound connection begins.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use mimic_core::Mode;
use mimic_core::mock::{
    DEFAULT_FACTOR, factor_number, parse_factor, parse_factor_header, parse_mode_header,
};
use mimic_core::proxy::tls::Exclusions;
use mimic_runtime::config::{ConfigError, Env};
use mimic_runtime::metrics::Metrics;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;

const MODE_VAR: &str = "MIMIC_MODE";
const FACTOR_VAR: &str = "MIMIC_MOCK_LATENCY_FACTOR";
const PASSTHROUGH_VAR: &str = "MIMIC_MOCK_PASSTHROUGH";
const MAX_PENDING_VAR: &str = "MIMIC_MOCK_MAX_PENDING";
const MAX_PENDING_RANGE: std::ops::RangeInclusive<usize> = 1..=65_536;
/// Mocked requests pending at once unless `MIMIC_MOCK_MAX_PENDING` says otherwise.
pub const DEFAULT_MAX_PENDING: usize = 1024;

/// The state file is read up to this many bytes: a valid one has about 40.
const MAX_STATE_BYTES: u64 = 1024;
/// A `value` of `mode unknown` is cut to this many bytes.
const MAX_VALUE_BYTES: usize = 32;

/// `MIMIC_MODE`, `MIMIC_MOCK_LATENCY_FACTOR` and `MIMIC_MOCK_PASSTHROUGH`, and where the
/// mode is saved.
#[derive(Debug, Clone)]
pub struct ModeConfig {
    pub mode: Mode,
    /// Latency factor in hundredths.
    pub factor: u32,
    /// Hosts, suffixes and addresses that mock mode passes through (spec §8).
    pub passthrough: Exclusions,
    /// Mocked requests pending at once (spec §8 "Bounds").
    pub max_pending: usize,
    /// `<MIMIC_TRUST_DIR>/state/mode`.
    pub state_file: PathBuf,
}

impl ModeConfig {
    pub fn from_env(env: &Env, trust_dir: &Path) -> Result<ModeConfig, ConfigError> {
        let mode = match env.optional(MODE_VAR) {
            None => Mode::Discovery,
            Some(value) => value
                .parse()
                .map_err(|reason| ConfigError::invalid(MODE_VAR, value, reason))?,
        };
        let factor = match env.optional(FACTOR_VAR) {
            None => DEFAULT_FACTOR,
            Some(value) => parse_factor(value).ok_or_else(|| {
                ConfigError::invalid(FACTOR_VAR, value, "expected a decimal from 0 to 100")
            })?,
        };
        let passthrough = match env.optional(PASSTHROUGH_VAR) {
            None => Exclusions::default(),
            Some(value) => Exclusions::parse(value)
                .map_err(|reason| ConfigError::invalid(PASSTHROUGH_VAR, value, reason))?,
        };
        let max_pending = match env.optional(MAX_PENDING_VAR) {
            None => DEFAULT_MAX_PENDING,
            Some(value) => value
                .parse::<usize>()
                .ok()
                .filter(|n| MAX_PENDING_RANGE.contains(n))
                .ok_or_else(|| {
                    ConfigError::invalid(
                        MAX_PENDING_VAR,
                        value,
                        "expected a number from 1 to 65536",
                    )
                })?,
        };
        Ok(ModeConfig {
            mode,
            factor,
            passthrough,
            max_pending,
            state_file: trust_dir.join("state").join("mode"),
        })
    }
}

impl Default for ModeConfig {
    fn default() -> ModeConfig {
        ModeConfig {
            mode: Mode::Discovery,
            factor: DEFAULT_FACTOR,
            passthrough: Exclusions::default(),
            max_pending: DEFAULT_MAX_PENDING,
            state_file: PathBuf::new(),
        }
    }
}

/// What a `200` or `304` of the controller says about the mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announced {
    /// `Err` holds the value as received (cut), empty when the header is absent.
    pub mode: Result<Mode, String>,
    pub factor: Option<u32>,
}

impl Announced {
    /// Reads `Mimic-Mode` and `Mimic-Latency-Factor` of an answer.
    pub fn from_headers(headers: &reqwest::header::HeaderMap) -> Announced {
        let values = |name: &str| -> Vec<&str> {
            headers
                .get_all(name)
                .iter()
                .map(|v| v.to_str().unwrap_or("\u{fffd}"))
                .collect()
        };
        let modes = values("mimic-mode");
        let mode = parse_mode_header(&modes).ok_or_else(|| cut(&modes.join(",")));
        Announced {
            mode,
            factor: parse_factor_header(&values("mimic-latency-factor")),
        }
    }
}

fn cut(value: &str) -> String {
    let mut end = value.len().min(MAX_VALUE_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

/// Where the current mode comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Env,
    State,
    Pull,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Env => "env",
            Source::State => "state",
            Source::Pull => "pull",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ToLabel {
    to: &'static str,
}

struct Current {
    mode: Mode,
    factor: u32,
    /// Answers without a valid mode are under way: their warning is written.
    unknown: bool,
    /// The last write of the state file failed: its warning is written.
    unsaved: bool,
}

struct Inner {
    current: Mutex<Current>,
    state_file: PathBuf,
    gauge: Gauge,
    /// `mimic_mock_latency_factor`: the factor held as a decimal, in both modes.
    factor_gauge: Gauge<f64, AtomicU64>,
    /// `mimic_mock_mode_switches_total{to}`.
    switches: Family<ToLabel, Counter>,
    /// Counts the changes of mode: a period of the miss log (spec §8) is one value.
    epoch: AtomicU64,
    /// The mode, for the connections that wait for a change of it (spec §8 "Switching").
    watch: tokio::sync::watch::Sender<Mode>,
}

/// The mode and latency factor of the sidecar, shared between the pull client and its readers.
#[derive(Clone)]
pub struct ModeState(Arc<Inner>);

impl Default for ModeState {
    /// Discovery, factor 1, no state file.
    fn default() -> ModeState {
        ModeState::new(&ModeConfig::default())
    }
}

impl ModeState {
    fn new(config: &ModeConfig) -> ModeState {
        ModeState(Arc::new(Inner {
            current: Mutex::new(Current {
                mode: config.mode,
                factor: config.factor,
                unknown: false,
                unsaved: false,
            }),
            state_file: config.state_file.clone(),
            gauge: Gauge::default(),
            factor_gauge: Gauge::default(),
            switches: Family::default(),
            epoch: AtomicU64::new(0),
            watch: tokio::sync::watch::Sender::new(config.mode),
        }))
    }

    /// The state at start: the state file when it holds a valid one, else the configuration.
    /// Registers `mimic_mock_mode`.
    pub async fn start(config: &ModeConfig, metrics: &mut Metrics) -> ModeState {
        let state = ModeState::new(config);
        metrics.registry_mut().register(
            "mock_mode",
            "1 when the sidecar is in mock mode, 0 in discovery mode",
            state.0.gauge.clone(),
        );
        // Both series exist from the start, at 0.
        for mode in [Mode::Mock, Mode::Discovery] {
            let _ = state
                .0
                .switches
                .get_or_create(&ToLabel { to: mode.as_str() });
        }
        metrics.registry_mut().register(
            "mock_mode_switches",
            "Mode changes applied, by the mode taken",
            state.0.switches.clone(),
        );
        metrics.registry_mut().register(
            "mock_latency_factor",
            "Latency factor held (it applies in mock mode only)",
            state.0.factor_gauge.clone(),
        );
        let mut source = Source::Env;
        if !config.state_file.as_os_str().is_empty() {
            let path = config.state_file.clone();
            let read = tokio::task::spawn_blocking(move || read_state(&path))
                .await
                .unwrap_or_else(|_| Err("read task failed".to_string()));
            match read {
                Ok(Some((mode, factor))) => {
                    let mut current = state.current();
                    (current.mode, current.factor) = (mode, factor);
                    source = Source::State;
                }
                Ok(None) => {}
                Err(reason) => {
                    tracing::warn!(target: "mock", reason, "mode state ignored");
                }
            }
        }
        let (mode, factor) = state.get();
        state.0.watch.send_replace(mode);
        state.0.gauge.set(i64::from(mode == Mode::Mock));
        state.0.factor_gauge.set(f64::from(factor) / 100.0);
        changed(mode, None, factor, source);
        state
    }

    fn current(&self) -> MutexGuard<'_, Current> {
        self.0
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn get(&self) -> (Mode, u32) {
        let current = self.current();
        (current.mode, current.factor)
    }

    pub fn mode(&self) -> Mode {
        self.get().0
    }

    /// Changes with each change of mode.
    pub fn epoch(&self) -> u64 {
        self.0.epoch.load(Ordering::Acquire)
    }

    /// Resolves once the mode is `mode` (at once when it is). Used by open connections to
    /// notice a switch.
    pub async fn reaches(&self, mode: Mode) {
        let mut watch = self.0.watch.subscribe();
        // The sender lives as long as `self`.
        let _ = watch.wait_for(|current| *current == mode).await;
    }

    /// The latency factor in hundredths.
    pub fn factor(&self) -> u32 {
        self.get().1
    }

    /// Applies the mode of a `200` or `304`. An answer without a valid `Mimic-Mode` keeps
    /// the current one (one warning per streak of them).
    pub async fn observe(&self, announced: Announced) {
        let mode = match announced.mode {
            Ok(mode) => mode,
            Err(value) => {
                let mut current = self.current();
                if !std::mem::replace(&mut current.unknown, true) {
                    drop(current);
                    tracing::warn!(target: "mock", value, "mode unknown");
                }
                return;
            }
        };
        let (previous, previous_factor) = {
            let mut current = self.current();
            current.unknown = false;
            (current.mode, current.factor)
        };
        let factor = announced.factor.unwrap_or(previous_factor);
        if (mode, factor) == (previous, previous_factor) {
            return;
        }
        // Saved before it is applied, so that a restart never resumes an older mode.
        let saved = self.save(mode, factor).await;
        {
            let mut current = self.current();
            (current.mode, current.factor) = (mode, factor);
            if mode != previous {
                self.0.epoch.fetch_add(1, Ordering::AcqRel);
            }
        }
        self.0.watch.send_replace(mode);
        self.0.gauge.set(i64::from(mode == Mode::Mock));
        self.0.factor_gauge.set(f64::from(factor) / 100.0);
        // A change whose state file was not written is not counted (spec §9).
        if mode != previous && saved {
            self.0
                .switches
                .get_or_create(&ToLabel { to: mode.as_str() })
                .inc();
        }
        changed(mode, Some(previous), factor, Source::Pull);
    }

    /// Writes the state file through a temporary file and a rename. A failure is one warning
    /// per streak and changes nothing else. Whether the file was written.
    async fn save(&self, mode: Mode, factor: u32) -> bool {
        let path = self.0.state_file.clone();
        let failure = tokio::task::spawn_blocking(move || write_state(&path, mode, factor))
            .await
            .unwrap_or_else(|_| Err("write task failed".to_string()))
            .err();
        let mut current = self.current();
        match failure {
            None => {
                current.unsaved = false;
                true
            }
            Some(reason) => {
                if !std::mem::replace(&mut current.unsaved, true) {
                    drop(current);
                    tracing::warn!(target: "mock", reason, "mode state not saved");
                }
                false
            }
        }
    }
}

fn changed(mode: Mode, previous: Option<Mode>, factor: u32, source: Source) {
    tracing::info!(
        target: "mock",
        mode = mode.as_str(),
        previous = previous.map(Mode::as_str),
        latency_factor = f64::from(factor) / 100.0,
        source = source.as_str(),
        "mode changed"
    );
}

/// The saved mode, `None` without a file.
fn read_state(path: &Path) -> Result<Option<(Mode, u32)>, String> {
    // A FIFO or a device would block the open or the read: only a regular file is read.
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Err("not a regular file".to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read: {}", e.kind())),
    }
    let file = std::fs::File::open(path).map_err(|e| format!("cannot read: {}", e.kind()))?;
    let mut text = String::new();
    file.take(MAX_STATE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|e| format!("cannot read: {}", e.kind()))?;
    if text.len() as u64 > MAX_STATE_BYTES {
        return Err("too large".to_string());
    }
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| "not a JSON object".to_string())?;
    let mode = value
        .get("mode")
        .and_then(|m| m.as_str())
        .and_then(|m| m.parse::<Mode>().ok())
        .ok_or("invalid mode")?;
    let factor = value
        .get("latency_factor")
        .filter(|f| f.is_number())
        .and_then(|f| parse_factor(&f.to_string()))
        .ok_or("invalid latency_factor")?;
    Ok(Some((mode, factor)))
}

fn write_state(path: &Path, mode: Mode, factor: u32) -> Result<(), String> {
    let error = |what: &str, e: std::io::Error| format!("{what}: {}", e.kind());
    let dir = path.parent().ok_or("no directory")?;
    std::fs::create_dir_all(dir).map_err(|e| error("create directory", e))?;
    let text = serde_json::json!({
        "latency_factor": factor_number(factor),
        "mode": mode.as_str(),
    })
    .to_string();
    let temporary = dir.join("mode.tmp");
    std::fs::write(&temporary, text).map_err(|e| error("write", e))?;
    std::fs::rename(&temporary, path).map_err(|e| error("rename", e))
}
