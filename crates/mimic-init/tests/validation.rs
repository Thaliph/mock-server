//! Valeurs reprises dans les règles iptables : analysées avant toute commande (INI-11).

use std::process::{Command, Output};

use mimic_init::{InboundPorts, parse_cidr, parse_cidrs, parse_pod_ip, parse_ports};

#[test]
fn ini_11_cidrs_are_parsed_and_written_canonically() {
    assert_eq!(
        parse_cidrs(" 10.0.0.0/8, fd00::/8 ,10.96.0.1,,"),
        Ok(vec![
            "10.0.0.0/8".to_string(),
            "fd00::/8".to_string(),
            "10.96.0.1/32".to_string()
        ])
    );
    assert_eq!(parse_cidr("fd00::1"), Ok("fd00::1/128".to_string()));
    for bad in [
        "10.0.0.0/33",
        "fd00::/129",
        "example.com",
        "10.0.0.0/",
        "10.0.0.0/8/8",
        "10.0.0.0/8\n-A OUTPUT -j ACCEPT",
        "10.0.0.0/-1",
    ] {
        let err = parse_cidrs(bad).unwrap_err();
        assert!(err.contains("CIDR invalide"), "{bad:?}: {err}");
    }
}

#[test]
fn ini_11_ports_must_be_between_1_and_65535() {
    assert_eq!(parse_ports("80, 8080,"), Ok(vec![80, 8080]));
    for bad in ["0", "70000", "http", "80\n-A OUTPUT -j ACCEPT", "-1"] {
        let err = parse_ports(bad).unwrap_err();
        assert!(err.contains("port invalide"), "{bad:?}: {err}");
    }
    assert!(InboundPorts::parse("8080,0").is_err());
    assert_eq!(parse_pod_ip("10.244.0.20"), Ok("10.244.0.20".to_string()));
    assert!(parse_pod_ip("10.244.0.20 -j ACCEPT").is_err());
}

fn dry_run(env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mimic-init"));
    cmd.arg("--dry-run").env_clear().env("MIMIC_MESH", "none");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

#[test]
fn ini_11_invalid_values_stop_mimic_init_with_code_2() {
    let ok = dry_run(&[("MIMIC_EXCLUDE_OUTBOUND_CIDRS", "10.96.0.1")]);
    assert!(ok.status.success());
    assert!(
        String::from_utf8_lossy(&ok.stdout).contains("-A MIMIC_OUTBOUND -d 10.96.0.1/32 -j RETURN")
    );

    for (var, value) in [
        (
            "MIMIC_EXCLUDE_OUTBOUND_CIDRS",
            "10.0.0.0/8\n-A OUTPUT -j ACCEPT",
        ),
        ("MIMIC_EXCLUDE_OUTBOUND_CIDRS", "example.com"),
        ("MIMIC_EXCLUDE_OUTBOUND_PORTS", "70000"),
        ("MIMIC_EXCLUDE_INBOUND_PORTS", "0"),
        ("MIMIC_INBOUND_PORTS", "http"),
        ("POD_IP", "10.0.0.1 -j ACCEPT"),
        ("MIMIC_IPTABLES", "/tmp/x"),
        ("MIMIC_IPTABLES", "iptables -w"),
    ] {
        let out = dry_run(&[(var, value)]);
        assert_eq!(out.status.code(), Some(2), "{var}={value:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(var), "{var}: {err}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(!stdout.contains("*nat"), "aucune règle affichée : {stdout}");
    }
}
