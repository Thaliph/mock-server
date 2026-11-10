//! Prometheus registry of a component (spec §9, FND-03).

use prometheus_client::collector::Collector;
use prometheus_client::encoding::text::{encode_eof, encode_registry};
use prometheus_client::encoding::{DescriptorEncoder, EncodeMetric};
use prometheus_client::metrics::counter::ConstCounter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

/// `mimic_log_lines_dropped_total`, read from the process logger at scrape time.
#[derive(Debug)]
struct LogDrops;

impl Collector for LogDrops {
    fn encode(&self, mut encoder: DescriptorEncoder) -> std::fmt::Result {
        let counter = ConstCounter::new(crate::logging::dropped_lines());
        let metric = encoder.encode_descriptor(
            "log_lines_dropped",
            "Log lines dropped because stdout could not keep up",
            None,
            counter.metric_type(),
        )?;
        counter.encode(metric)
    }
}

/// `process_*` families (spec §9, #385), read from `/proc/self` at scrape time. A family whose
/// read fails is left out of the scrape, with one DEBUG line.
#[derive(Debug)]
struct ProcessMetrics;

#[cfg(target_os = "linux")]
impl ProcessMetrics {
    fn unavailable(file: &str, error: &dyn std::fmt::Display) {
        tracing::debug!(file, %error, "process metrics unavailable");
    }
}

#[cfg(target_os = "linux")]
impl Collector for ProcessMetrics {
    fn encode(&self, mut encoder: DescriptorEncoder) -> std::fmt::Result {
        use prometheus_client::metrics::gauge::ConstGauge;

        let process = match procfs::process::Process::myself() {
            Ok(process) => process,
            Err(error) => {
                Self::unavailable("/proc/self", &error);
                return Ok(());
            }
        };
        match process.stat() {
            Ok(stat) => {
                let ticks = procfs::ticks_per_second() as f64;
                let cpu = ConstCounter::new((stat.utime + stat.stime) as f64 / ticks);
                let metric = encoder.encode_descriptor(
                    "process_cpu_seconds",
                    "Total user and system CPU time spent in seconds",
                    None,
                    cpu.metric_type(),
                )?;
                cpu.encode(metric)?;

                let rss = ConstGauge::new(stat.rss.saturating_mul(procfs::page_size()) as i64);
                let metric = encoder.encode_descriptor(
                    "process_resident_memory_bytes",
                    "Resident memory size in bytes",
                    None,
                    rss.metric_type(),
                )?;
                rss.encode(metric)?;

                match procfs::boot_time_secs() {
                    Ok(btime) => {
                        let start = ConstGauge::new(btime as f64 + stat.starttime as f64 / ticks);
                        let metric = encoder.encode_descriptor(
                            "process_start_time_seconds",
                            "Start time of the process since unix epoch in seconds",
                            None,
                            start.metric_type(),
                        )?;
                        start.encode(metric)?;
                    }
                    Err(error) => Self::unavailable("/proc/stat", &error),
                }
            }
            Err(error) => Self::unavailable("/proc/self/stat", &error),
        }
        match process.fd_count() {
            Ok(count) => {
                let fds = ConstGauge::new(count as i64);
                let metric = encoder.encode_descriptor(
                    "process_open_fds",
                    "Number of open file descriptors",
                    None,
                    fds.metric_type(),
                )?;
                fds.encode(metric)?;
            }
            Err(error) => Self::unavailable("/proc/self/fd", &error),
        }
        match process.limits() {
            Ok(limits) => {
                if let procfs::process::LimitValue::Value(soft) = limits.max_open_files.soft_limit {
                    let max = ConstGauge::new(soft as i64);
                    let metric = encoder.encode_descriptor(
                        "process_max_fds",
                        "Maximum number of open file descriptors",
                        None,
                        max.metric_type(),
                    )?;
                    max.encode(metric)?;
                }
            }
            Err(error) => Self::unavailable("/proc/self/limits", &error),
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
impl Collector for ProcessMetrics {
    fn encode(&self, _encoder: DescriptorEncoder) -> std::fmt::Result {
        Ok(())
    }
}

pub struct Metrics {
    registry: Registry,
    /// Unprefixed: the `process_*` names are those of the Prometheus client libraries.
    process: Registry,
}

impl Metrics {
    /// Creates the registry with `mimic_build_info{component,version} 1`.
    pub fn new(component: &str, version: &str) -> Metrics {
        let mut registry = Registry::with_prefix("mimic");
        let build_info = Family::<Vec<(String, String)>, Gauge>::default();
        build_info
            .get_or_create(&vec![
                ("component".to_string(), component.to_string()),
                ("version".to_string(), version.to_string()),
            ])
            .set(1);
        registry.register(
            "build_info",
            "Build information of the component",
            build_info,
        );
        registry.register_collector(Box::new(LogDrops));
        let mut process = Registry::default();
        process.register_collector(Box::new(ProcessMetrics));
        Metrics { registry, process }
    }

    pub fn registry_mut(&mut self) -> &mut Registry {
        &mut self.registry
    }

    /// OpenMetrics text exposition.
    pub fn encode(&self) -> String {
        let mut out = String::new();
        // Writing into a String cannot fail.
        let _ = encode_registry(&mut out, &self.registry);
        let _ = encode_registry(&mut out, &self.process);
        let _ = encode_eof(&mut out);
        out
    }
}
