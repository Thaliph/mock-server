//! Trafic local du pod (spec §3.1, INI-12) : la boucle locale n'est pas interceptée, sauf
//! `--intercept-loopback`.

use std::process::Command;

use mimic_init::{InboundPorts, Mesh, OUTBOUND_CHAIN, RulesConfig, render};

/// Règles de `MIMIC_OUTBOUND`, dans l'ordre.
fn outbound(c: &RulesConfig) -> Vec<String> {
    let prefix = format!("-A {OUTBOUND_CHAIN} ");
    render(c)
        .restore
        .lines()
        .filter(|l| l.starts_with(&prefix))
        .map(String::from)
        .collect()
}

fn position(rules: &[String], rule: &str) -> usize {
    rules
        .iter()
        .position(|r| r == rule)
        .unwrap_or_else(|| panic!("règle absente : {rule}\n{rules:#?}"))
}

/// Hors mesh, puis sous chacun des mesh pris en charge.
fn meshes() -> Vec<Mesh> {
    vec![
        Mesh::None,
        Mesh::IstioSidecar {
            proxy_uid: 1337,
            proxy_gid: 1337,
            inbound_source: "127.0.0.6/32".into(),
        },
        Mesh::IstioAmbient {
            mark: "0x539/0xfff".into(),
        },
        Mesh::Linkerd { proxy_uid: 2102 },
    ]
}

#[test]
fn ini_12_loopback_is_not_intercepted_by_default() {
    for mesh in meshes() {
        for ipv6 in [false, true] {
            let c = RulesConfig {
                mesh: mesh.clone(),
                ipv6,
                inbound_ports: InboundPorts::List(vec![8080]),
                exclude_outbound_cidrs: vec!["10.96.0.1/32".into(), "fd00::/8".into()],
                exclude_outbound_ports: vec![5432],
                pod_ip: Some("10.244.0.20".into()),
                ..RulesConfig::default()
            };
            assert!(
                !c.intercept_loopback,
                "défaut : boucle locale non interceptée"
            );
            let out = outbound(&c);
            let lo = position(&out, "-A MIMIC_OUTBOUND -o lo -j RETURN");
            // Après les RETURN de Mimic (et du mesh), donc après le trajet entrant du mesh.
            let own = position(
                &out,
                "-A MIMIC_OUTBOUND -m owner --uid-owner 1338 -j RETURN",
            );
            assert!(own < lo, "{mesh:?} ipv6={ipv6} : {out:#?}");
            for (i, rule) in out.iter().enumerate() {
                if rule.contains("--to-ports 15106") || rule.contains("DNAT") {
                    assert!(i < own, "trajet entrant avant le reste : {out:#?}");
                }
                if rule.contains("--to-ports 15101") {
                    assert!(lo < i, "trafic local avant la redirection : {out:#?}");
                }
                assert!(
                    !rule.contains("! -d 127.0.0.1/32") && !rule.contains("! -d ::1/128"),
                    "{mesh:?} ipv6={ipv6} : {rule}"
                );
            }
            // Le RETURN local précède les exclusions de CIDR et de ports.
            let excluded = position(&out, "-A MIMIC_OUTBOUND -p tcp --dport 5432 -j RETURN");
            assert!(lo < excluded);
        }
    }
}

#[test]
fn ini_12_intercept_loopback_restores_the_loopback_redirect() {
    for mesh in meshes() {
        for (ipv6, loopback) in [(false, "127.0.0.1/32"), (true, "::1/128")] {
            let c = RulesConfig {
                mesh: mesh.clone(),
                ipv6,
                intercept_loopback: true,
                ..RulesConfig::default()
            };
            let out = outbound(&c);
            let lo = position(
                &out,
                &format!("-A MIMIC_OUTBOUND -o lo ! -d {loopback} -j RETURN"),
            );
            let own = position(
                &out,
                "-A MIMIC_OUTBOUND -m owner --uid-owner 1338 -j RETURN",
            );
            let redirect = position(
                &out,
                "-A MIMIC_OUTBOUND -p tcp -j REDIRECT --to-ports 15101",
            );
            assert!(own < lo && lo < redirect, "{mesh:?} ipv6={ipv6} : {out:#?}");
            assert!(
                !out.iter().any(|r| r == "-A MIMIC_OUTBOUND -o lo -j RETURN"),
                "{out:#?}"
            );
        }
    }
}

/// `MIMIC_INTERCEPT_LOOPBACK` et `--intercept-loopback` arrivent jusqu'aux règles.
#[test]
fn ini_12_intercept_loopback_from_environment_and_flag() {
    let run = |args: &[&str], env: &[(&str, &str)]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mimic-init"));
        cmd.arg("--dry-run")
            .args(args)
            .env_clear()
            .env("MIMIC_MESH", "none");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let default = run(&[], &[]);
    assert!(
        default.contains("-A MIMIC_OUTBOUND -o lo -j RETURN"),
        "{default}"
    );
    let false_env = run(&[], &[("MIMIC_INTERCEPT_LOOPBACK", "false")]);
    assert!(
        false_env.contains("-A MIMIC_OUTBOUND -o lo -j RETURN"),
        "{false_env}"
    );
    for out in [
        run(&["--intercept-loopback"], &[]),
        run(&[], &[("MIMIC_INTERCEPT_LOOPBACK", "true")]),
    ] {
        assert!(
            out.contains("-A MIMIC_OUTBOUND -o lo ! -d 127.0.0.1/32 -j RETURN"),
            "{out}"
        );
    }
}
