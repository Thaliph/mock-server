//! The generator Job and NetworkPolicy of a load test, printed as the controller builds them
//! (spec §7.8). `scripts/e2e-load.sh` creates them with a server-side dry run as the controller
//! account, so that the admission policy of the chart is checked against the objects of
//! `mimic_core::load::job` (LDT-17), not against a copy of them.
//!
//! stdin: one JSON object with the fields read below; stdout: `{"job": …, "networkPolicy": …}`.

use std::collections::BTreeMap;
use std::io::{self, Read};

use mimic_core::load::job::{self, JobInput, PolicyInput};
use serde_json::{Value, json};

fn field<'a>(input: &'a Value, key: &str) -> Result<&'a Value, String> {
    input.get(key).ok_or_else(|| format!("missing field {key}"))
}

fn text<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    field(input, key)?
        .as_str()
        .ok_or_else(|| format!("{key} is not a string"))
}

fn number(input: &Value, key: &str) -> Result<u64, String> {
    field(input, key)?
        .as_u64()
        .ok_or_else(|| format!("{key} is not an unsigned integer"))
}

fn labels(input: &Value, key: &str) -> Result<BTreeMap<String, String>, String> {
    let object = field(input, key)?
        .as_object()
        .ok_or_else(|| format!("{key} is not an object"))?;
    object
        .iter()
        .map(|(k, v)| {
            v.as_str()
                .map(|v| (k.clone(), v.to_string()))
                .ok_or_else(|| format!("{key}.{k} is not a string"))
        })
        .collect()
}

fn run() -> Result<Value, String> {
    let mut raw = String::new();
    io::stdin()
        .read_to_string(&mut raw)
        .map_err(|e| e.to_string())?;
    let input: Value = serde_json::from_str(&raw).map_err(|e| format!("stdin: {e}"))?;

    let ports = field(&input, "ports")?
        .as_array()
        .ok_or("ports is not an array")?
        .iter()
        .map(|p| {
            p.as_u64()
                .and_then(|p| u16::try_from(p).ok())
                .ok_or_else(|| "a port is out of range".to_string())
        })
        .collect::<Result<Vec<u16>, String>>()?;
    let controller_labels = labels(&input, "controllerLabels")?;
    let dns_labels = labels(&input, "dnsLabels")?;
    let metrics_from = field(&input, "metricsFrom")?
        .as_array()
        .cloned()
        .unwrap_or_default();
    let controller_port =
        u16::try_from(number(&input, "controllerPort")?).map_err(|e| e.to_string())?;
    let max_plan = usize::try_from(number(&input, "maxPlan")?).map_err(|e| e.to_string())?;

    let job_input = JobInput {
        namespace: text(&input, "namespace")?,
        name: text(&input, "name")?,
        uid: text(&input, "uid")?,
        deployment: text(&input, "deployment")?,
        concurrency: number(&input, "concurrency")?,
        duration_ms: number(&input, "durationMs")?,
        pod_concurrency: number(&input, "podConcurrency")?,
        start_timeout_ms: number(&input, "startTimeoutMs")?,
        report_interval_ms: number(&input, "reportIntervalMs")?,
        max_plan,
        image: text(&input, "image")?,
        resources: field(&input, "resources")?,
        controller_url: text(&input, "controllerUrl")?,
        controller_ca: text(&input, "controllerCa")?,
        secret_name: input.get("secretName").and_then(Value::as_str),
    };
    let policy_input = PolicyInput {
        namespace: text(&input, "namespace")?,
        name: text(&input, "name")?,
        uid: text(&input, "uid")?,
        deployment: text(&input, "deployment")?,
        ports: &ports,
        controller_namespace: text(&input, "controllerNamespace")?,
        controller_labels: &controller_labels,
        controller_port,
        dns_namespace: text(&input, "dnsNamespace")?,
        dns_labels: &dns_labels,
        metrics_from: &metrics_from,
    };
    Ok(json!({
        "job": job::job(&job_input),
        "networkPolicy": job::network_policy(&policy_input),
    }))
}

fn main() {
    match run() {
        Ok(objects) => println!("{objects}"),
        Err(error) => {
            eprintln!("load_objects: {error}");
            std::process::exit(2);
        }
    }
}
