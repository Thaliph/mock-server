//! `mimic-init`: first init container of an injected pod. Installs the iptables redirection
//! in the pod network namespace (spec §4), then exits.

use std::collections::BTreeSet;
use std::io::Write;
use std::process::{Command, ExitCode, Stdio};

use mimic_core::redirect::{Family, RedirectConfig, rules};
use mimic_runtime::config::{ConfigError, Env};

const BINARY: &str = "mimic-init";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const IF_INET6: &str = "/proc/net/if_inet6";

struct Config {
    log: String,
    redirect: RedirectConfig,
}

fn parse_port(var: &str, value: &str) -> Result<u16, ConfigError> {
    match value.trim().parse::<u16>() {
        Ok(0) => Err(ConfigError::invalid(var, value, "port must be 1-65535")),
        Ok(port) => Ok(port),
        Err(e) => Err(ConfigError::invalid(var, value, e)),
    }
}

fn port_var(env: &Env, var: &str, default: u16) -> Result<u16, ConfigError> {
    env.optional(var)
        .map_or(Ok(default), |value| parse_port(var, value))
}

fn config(env: &Env) -> Result<Config, ConfigError> {
    let log = env.optional("MIMIC_LOG").unwrap_or("info").to_string();
    mimic_runtime::logging::filter(&log).map_err(|e| ConfigError::invalid("MIMIC_LOG", &log, e))?;
    let var = "MIMIC_EXCLUDE_INBOUND_PORTS";
    let mut exclude_inbound_ports = BTreeSet::new();
    for item in env.optional(var).unwrap_or_default().split(',') {
        if !item.trim().is_empty() {
            exclude_inbound_ports.insert(parse_port(var, item)?);
        }
    }
    Ok(Config {
        log,
        redirect: RedirectConfig {
            inbound_port: port_var(env, "MIMIC_INBOUND_PORT", 15006)?,
            outbound_port: port_var(env, "MIMIC_OUTBOUND_PORT", 15001)?,
            proxy_uid: env.parse_or("MIMIC_PROXY_UID", 1337)?,
            exclude_inbound_ports,
        },
    })
}

/// The pod has IPv6 when `/proc/net/if_inet6` lists an address other than `::1`.
fn has_ipv6(if_inet6: &str) -> bool {
    const LOOPBACK: &str = "00000000000000000000000000000001";
    if_inet6
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .any(|address| address != LOOPBACK)
}

fn restore(family: Family, input: &str) -> Result<(), String> {
    let command = match family {
        Family::V4 => "iptables-restore",
        Family::V6 => "ip6tables-restore",
    };
    let mut child = Command::new(command)
        .arg("--noflush")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("cannot run {command}: {e}"))?;
    let written = child
        .stdin
        .take()
        .ok_or_else(|| format!("{command}: no stdin"))?
        .write_all(input.as_bytes());
    let status = child
        .wait()
        .map_err(|e| format!("{command}: wait failed: {e}"))?;
    if !status.success() {
        return Err(format!("{command} failed: {status}"));
    }
    written.map_err(|e| format!("{command}: cannot write rules: {e}"))
}

fn join<T: ToString>(items: impl IntoIterator<Item = T>) -> String {
    let items: Vec<String> = items.into_iter().map(|i| i.to_string()).collect();
    items.join(",")
}

fn main() -> ExitCode {
    if let Some(code) = mimic_runtime::version_flag(BINARY, VERSION) {
        return code;
    }
    let config = match config(&Env::from_os()) {
        Ok(config) => config,
        Err(e) => return mimic_runtime::config_failure(BINARY, &e),
    };
    mimic_runtime::logging::init(&config.log);

    let mut families = vec![Family::V4];
    if has_ipv6(&std::fs::read_to_string(IF_INET6).unwrap_or_default()) {
        families.push(Family::V6);
    } else {
        tracing::info!("no IPv6 in the pod, skipping the IPv6 rules");
    }
    for family in &families {
        if let Err(e) = restore(*family, &rules(&config.redirect, *family)) {
            tracing::error!(family = family.as_str(), error = %e, "redirection failed");
            return ExitCode::FAILURE;
        }
    }
    tracing::info!(
        families = join(families.iter().map(|f| f.as_str())),
        excluded_ports = join(&config.redirect.exclude_inbound_ports),
        "redirection installed"
    );
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inj_06_ipv6_is_present_only_when_an_address_is_listed() {
        assert!(!has_ipv6(""));
        assert!(!has_ipv6("\n"));
        assert!(has_ipv6(
            "fe80000000000000a8bb3cfffe123456 02 40 20 80 eth0\n"
        ));
    }

    #[test]
    fn inj_22_loopback_alone_is_no_ipv6() {
        let lo = "00000000000000000000000000000001 01 80 10 80 lo\n";
        assert!(!has_ipv6(lo));
        assert!(has_ipv6(&format!(
            "{lo}20010db8000000000000000000000002 02 40 00 80 eth0\n"
        )));
        assert!(has_ipv6(&format!(
            "{lo}fe80000000000000a8bb3cfffe123456 02 40 20 80 eth0\n"
        )));
    }
}
