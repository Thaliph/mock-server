//! The admission signals of the pod (spec §4.1 "Signals", §9 `mimic_admission_*`): a sample of
//! the admitter every tick feeds the 15 s utilisation and a snapshot, and a scrape encodes the
//! snapshot only. The counters exist from the first contract on; the gauges only while a
//! contract is in force.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use mimic_core::mesh::admission::{Contract, Counts};
use mimic_core::mesh::reason::Reason;
use mimic_core::mesh::utilization::Utilization;
use prometheus_client::collector::Collector;
use prometheus_client::encoding::{DescriptorEncoder, EncodeMetric};
use prometheus_client::metrics::MetricType;
use prometheus_client::metrics::counter::ConstCounter;
use prometheus_client::metrics::gauge::ConstGauge;
use prometheus_client::registry::Unit;

use super::admit::Admitter;
use super::{KindLabel, ResultLabel};

/// The signals of the pod, cheap to clone: the timer and `install` tick them, a scrape reads
/// them. Lock order: the signals, then the admitter.
#[derive(Debug, Clone, Default)]
pub(crate) struct Signals(Arc<Mutex<State>>);

#[derive(Debug, Default)]
struct State {
    utilization: Utilization,
    /// The instant of the last sample taken.
    at: Option<Duration>,
    totals: Option<Totals>,
    gauges: Option<Gauges>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Totals {
    admitted: u64,
    queued: u64,
    overloaded: u64,
    queue_timeout: u64,
    cancelled: u64,
    in_flight_seconds: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Gauges {
    in_flight: u32,
    depth: u32,
    limit: Contract,
    utilization: f64,
}

impl Signals {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One sample of `admitter`, taken under the signals' lock so that two ticks at once are
    /// serialised and the later instant wins.
    pub(crate) fn tick(&self, admitter: &Admitter) {
        let mut state = self.lock();
        let (now, contract, counts) = admitter.sample();
        Signals::update(&mut state, now, contract, counts);
    }

    /// What one sample makes of the state. A sample not after the last one keeps the snapshot,
    /// so a counter never goes back.
    fn update(state: &mut State, now: Duration, contract: Option<Contract>, counts: Counts) {
        if state.at.is_some_and(|at| now <= at) {
            return;
        }
        state.at = Some(now);
        state.utilization.tick(now, contract, counts);
        // The counters exist from the first contract on: one seen by a sample, or one applied
        // and removed between two samples, whose offers are counted.
        if state.totals.is_some() || contract.is_some() || counts != Counts::default() {
            state.totals = Some(Totals {
                admitted: counts
                    .admitted_at_once
                    .saturating_add(counts.admitted_from_queue),
                queued: counts.queued,
                overloaded: counts.overloaded,
                queue_timeout: counts.queue_timeout,
                cancelled: counts.cancelled,
                in_flight_seconds: counts.in_flight_ns as f64 / 1e9,
            });
        }
        state.gauges = contract.map(|limit| Gauges {
            in_flight: counts.in_flight,
            depth: counts.depth,
            limit,
            utilization: state.utilization.value().unwrap_or(0.0),
        });
    }
}

/// One gauge without labels.
fn gauge<N>(encoder: &mut DescriptorEncoder, name: &str, help: &str, value: N) -> std::fmt::Result
where
    ConstGauge<N>: EncodeMetric,
{
    ConstGauge::new(value).encode(encoder.encode_descriptor(name, help, None, MetricType::Gauge)?)
}

impl Collector for Signals {
    fn encode(&self, mut encoder: DescriptorEncoder) -> std::fmt::Result {
        let (totals, gauges) = {
            let state = self.lock();
            (state.totals, state.gauges)
        };
        let Some(t) = totals else {
            return Ok(());
        };
        let mut family = encoder.encode_descriptor(
            "admission_requests",
            "Requests offered to the admission contract, by result",
            None,
            MetricType::Counter,
        )?;
        for (result, n) in [
            ("admitted", t.admitted),
            ("queued", t.queued),
            (Reason::Overloaded.as_str(), t.overloaded),
            (Reason::QueueTimeout.as_str(), t.queue_timeout),
            ("cancelled", t.cancelled),
        ] {
            ConstCounter::new(n).encode(family.encode_family(&ResultLabel { result })?)?;
        }
        let seconds = Unit::Seconds;
        ConstCounter::new(t.in_flight_seconds).encode(encoder.encode_descriptor(
            "admission_in_flight",
            "Integral of the admitted requests in flight over time, in seconds",
            Some(&seconds),
            MetricType::Counter,
        )?)?;
        let Some(g) = gauges else {
            return Ok(());
        };
        gauge(
            &mut encoder,
            "admission_in_flight",
            "Admitted requests in flight",
            i64::from(g.in_flight),
        )?;
        gauge(
            &mut encoder,
            "admission_queue_depth",
            "Requests waiting in the admission queue",
            i64::from(g.depth),
        )?;
        let mut family = encoder.encode_descriptor(
            "admission_limit",
            "Admission contract in force, by kind",
            None,
            MetricType::Gauge,
        )?;
        for (kind, value) in [("concurrency", g.limit.concurrency), ("rps", g.limit.rps)] {
            ConstGauge::new(i64::from(value)).encode(family.encode_family(&KindLabel { kind })?)?;
        }
        gauge(
            &mut encoder,
            "admission_utilization",
            "Share of the admission contract used, smoothed over 15 s, refused demand included",
            g.utilization,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use mimic_core::mesh::admission::{Contract, Counts};
    use prometheus_client::registry::Registry;

    use super::Signals;

    /// The text of the registry holding only `signals`, without `# EOF`.
    fn text(signals: &Signals) -> String {
        let mut registry = Registry::with_prefix("mimic");
        registry.register_collector(Box::new(signals.clone()));
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &registry).unwrap();
        out.replace("# EOF\n", "")
    }

    const EXPOSITION: &str = "\
# HELP mimic_admission_requests Requests offered to the admission contract, by result
# TYPE mimic_admission_requests counter
mimic_admission_requests_total{result=\"admitted\"} 3
mimic_admission_requests_total{result=\"queued\"} 2
mimic_admission_requests_total{result=\"overloaded\"} 1
mimic_admission_requests_total{result=\"queue-timeout\"} 1
mimic_admission_requests_total{result=\"cancelled\"} 1
# HELP mimic_admission_in_flight_seconds Integral of the admitted requests in flight over time, in seconds
# TYPE mimic_admission_in_flight_seconds counter
# UNIT mimic_admission_in_flight_seconds seconds
mimic_admission_in_flight_seconds_total 0.9
# HELP mimic_admission_in_flight Admitted requests in flight
# TYPE mimic_admission_in_flight gauge
mimic_admission_in_flight 2
# HELP mimic_admission_queue_depth Requests waiting in the admission queue
# TYPE mimic_admission_queue_depth gauge
mimic_admission_queue_depth 1
# HELP mimic_admission_limit Admission contract in force, by kind
# TYPE mimic_admission_limit gauge
mimic_admission_limit{kind=\"concurrency\"} 2
mimic_admission_limit{kind=\"rps\"} 1000
# HELP mimic_admission_utilization Share of the admission contract used, smoothed over 15 s, refused demand included
# TYPE mimic_admission_utilization gauge
";

    #[test]
    fn adm_53_update_maps_counts_and_drops_the_gauges() {
        let signals = Signals::default();
        assert_eq!(text(&signals), "");
        Signals::update(
            &mut signals.lock(),
            Duration::from_secs(1),
            None,
            Counts::default(),
        );
        assert_eq!(text(&signals), "", "no contract and nothing offered");

        let counts = Counts {
            in_flight: 2,
            depth: 1,
            admitted_at_once: 2,
            queued: 2,
            admitted_from_queue: 1,
            overloaded: 1,
            queue_timeout: 1,
            cancelled: 1,
            in_flight_ns: 900_000_000,
        };
        let c = Some(Contract {
            concurrency: 2,
            rps: 1000,
        });
        Signals::update(&mut signals.lock(), Duration::from_secs(2), c, counts);
        let with = text(&signals);
        let (head, last) = with.rsplit_once('\n').map_or((&*with, ""), |(h, _)| {
            h.rsplit_once('\n').map_or((h, ""), |(h, l)| (h, l))
        });
        assert_eq!(format!("{head}\n"), EXPOSITION);
        assert!(last.starts_with("mimic_admission_utilization "), "{last}");

        // Removed: the gauges go, the counters stay.
        Signals::update(&mut signals.lock(), Duration::from_secs(3), None, counts);
        let without = text(&signals);
        let counters: String = EXPOSITION
            .lines()
            .take(11)
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(without, counters);

        // An older sample keeps the snapshot.
        let fewer = Counts {
            admitted_at_once: 0,
            ..counts
        };
        Signals::update(&mut signals.lock(), Duration::from_secs(2), c, fewer);
        assert_eq!(text(&signals), counters);
    }

    #[test]
    fn adm_53_a_contract_gone_between_two_samples_keeps_its_counters() {
        let signals = Signals::default();
        let counts = Counts {
            admitted_at_once: 1,
            ..Counts::default()
        };
        Signals::update(&mut signals.lock(), Duration::from_secs(1), None, counts);
        let scrape = text(&signals);
        assert!(
            scrape.contains("mimic_admission_requests_total{result=\"admitted\"} 1\n"),
            "{scrape}"
        );
        assert!(!scrape.contains("mimic_admission_in_flight "), "{scrape}");
    }
}
