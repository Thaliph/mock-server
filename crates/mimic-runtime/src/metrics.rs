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
                let max = ConstGauge::new(max_fds_value(&limits.max_open_files.soft_limit));
                let metric = encoder.encode_descriptor(
                    "process_max_fds",
                    "Maximum number of open file descriptors",
                    None,
                    max.metric_type(),
                )?;
                max.encode(metric)?;
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

/// `process_max_fds` for a soft `RLIMIT_NOFILE`: unlimited is `+Inf`.
#[cfg(target_os = "linux")]
fn max_fds_value(soft: &procfs::process::LimitValue) -> f64 {
    match soft {
        procfs::process::LimitValue::Value(soft) => *soft as f64,
        procfs::process::LimitValue::Unlimited => f64::INFINITY,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use procfs::process::LimitValue;

    #[derive(Debug)]
    struct InfGauge(prometheus_client::metrics::gauge::ConstGauge<f64>);

    impl Collector for InfGauge {
        fn encode(&self, mut encoder: DescriptorEncoder) -> std::fmt::Result {
            let metric = encoder.encode_descriptor("x", "x", None, self.0.metric_type())?;
            self.0.encode(metric)
        }
    }

    #[test]
    fn mon_04_max_fds_is_the_soft_limit_or_infinite_when_unlimited() {
        assert_eq!(max_fds_value(&LimitValue::Value(1024)), 1024.0);
        assert_eq!(max_fds_value(&LimitValue::Unlimited), f64::INFINITY);
        let gauge = prometheus_client::metrics::gauge::ConstGauge::new(f64::INFINITY);
        let mut registry = Registry::default();
        registry.register_collector(Box::new(InfGauge(gauge)));
        let mut out = String::new();
        encode_registry(&mut out, &registry).unwrap();
        assert!(open_metrics_inf(&out).contains("x +Inf\n"), "{out}");
    }
}

/// The client library writes an infinite float as `inf`; OpenMetrics spells it `+Inf`.
fn open_metrics_inf(text: &str) -> String {
    text.lines()
        .map(|line| match line.strip_suffix(" inf") {
            Some(series) => format!("{series} +Inf\n"),
            None => format!("{line}\n"),
        })
        .collect()
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
        let mut process = String::new();
        let _ = encode_registry(&mut process, &self.process);
        out.push_str(&open_metrics_inf(&process));
        let _ = encode_eof(&mut out);
        out
    }
}
