//! FND-01: configuration from `MIMIC_*` environment variables only.

use std::net::SocketAddr;
use std::time::Duration;

use mimic_runtime::config::{CommonConfig, Env};

fn env(pairs: &[(&str, &str)]) -> Env {
    Env::from_pairs(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())))
}

fn default_admin() -> SocketAddr {
    "0.0.0.0:15100".parse().unwrap()
}

#[test]
fn fnd_01_defaults_apply_when_nothing_is_set() {
    let config = CommonConfig::from_env(&env(&[]), default_admin()).unwrap();
    assert_eq!(config.log, "info");
    assert_eq!(config.admin_addr, default_admin());
    assert_eq!(config.shutdown_grace, Duration::from_secs(10));
}

#[test]
fn fnd_01_values_are_read_from_the_environment() {
    let config = CommonConfig::from_env(
        &env(&[
            ("MIMIC_LOG", "debug"),
            ("MIMIC_ADMIN_ADDR", "127.0.0.1:9000"),
            ("MIMIC_SHUTDOWN_GRACE", "250ms"),
        ]),
        default_admin(),
    )
    .unwrap();
    assert_eq!(config.log, "debug");
    assert_eq!(config.admin_addr, "127.0.0.1:9000".parse().unwrap());
    assert_eq!(config.shutdown_grace, Duration::from_millis(250));
}

#[test]
fn fnd_01_an_invalid_value_names_the_variable_and_the_value() {
    for (var, value) in [
        ("MIMIC_ADMIN_ADDR", "not-an-address"),
        ("MIMIC_SHUTDOWN_GRACE", "ten"),
        ("MIMIC_SHUTDOWN_GRACE", "10"),
        ("MIMIC_LOG", "[[["),
    ] {
        let err = CommonConfig::from_env(&env(&[(var, value)]), default_admin()).unwrap_err();
        let message = err.to_string();
        assert!(message.contains(var), "{message}");
        assert!(message.contains(value), "{message}");
    }
}

#[test]
fn fnd_01_a_missing_required_variable_names_it() {
    let err = env(&[]).required("MIMIC_VALKEY_URL").unwrap_err();
    assert!(err.to_string().contains("MIMIC_VALKEY_URL"), "{err}");
}

#[test]
fn fnd_01_an_empty_value_counts_as_unset() {
    let config =
        CommonConfig::from_env(&env(&[("MIMIC_ADMIN_ADDR", "")]), default_admin()).unwrap();
    assert_eq!(config.admin_addr, default_admin());
    assert!(
        env(&[("MIMIC_VALKEY_URL", "")])
            .required("MIMIC_VALKEY_URL")
            .is_err()
    );
}

#[test]
fn fnd_01_secrets_are_never_echoed_in_errors() {
    let err = env(&[]).required("MIMIC_VALKEY_PASSWORD").unwrap_err();
    assert!(!err.to_string().contains("hunter2"));
}

#[test]
fn ctl_14_durations_take_the_suffixes_ms_s_m_h_and_d() {
    for (value, expected) in [
        ("500ms", Duration::from_millis(500)),
        ("10s", Duration::from_secs(10)),
        ("1m", Duration::from_secs(60)),
        ("2h", Duration::from_secs(7200)),
        ("30d", Duration::from_secs(30 * 86_400)),
        ("0m", Duration::ZERO),
    ] {
        let parsed = env(&[("MIMIC_SHUTDOWN_GRACE", value)])
            .duration_or("MIMIC_SHUTDOWN_GRACE", Duration::from_secs(1))
            .unwrap_or_else(|e| panic!("{value}: {e}"));
        assert_eq!(parsed, expected, "{value}");
    }
}

#[test]
fn ctl_14_bad_duration_suffixes_and_numbers_are_refused() {
    for value in [
        "1w",
        "30",
        "d",
        "m",
        "1.5h",
        "-1m",
        "+1m",
        "1 m",
        "1M",
        "1D",
        "1mm",
        "18446744073709551615d",
    ] {
        let result = env(&[("MIMIC_SHUTDOWN_GRACE", value)])
            .duration_or("MIMIC_SHUTDOWN_GRACE", Duration::from_secs(1));
        assert!(result.is_err(), "{value:?} was accepted");
    }
}
