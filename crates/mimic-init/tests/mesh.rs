//! Cohabitation avec un service mesh (spec §3.3, INI-05 à INI-10).

use mimic_init::{
    InboundPorts, Jump, Mesh, MeshMode, OUTBOUND_CHAIN, RulesConfig, detect_mesh, plan_jump,
    render, resolve_mesh,
};

const ISTIO_SIDECAR: &str = include_str!("fixtures/istio-sidecar.txt");
const ISTIO_AMBIENT: &str = include_str!("fixtures/istio-ambient.txt");
const LINKERD: &str = include_str!("fixtures/linkerd.txt");
const NONE: &str = include_str!("fixtures/none.txt");

fn istio() -> Mesh {
    Mesh::IstioSidecar {
        proxy_uid: 1337,
        proxy_gid: 1337,
        inbound_source: "127.0.0.6/32".into(),
    }
}

fn ambient() -> Mesh {
    Mesh::IstioAmbient {
        mark: "0x539/0xfff".into(),
    }
}

fn linkerd() -> Mesh {
    Mesh::Linkerd { proxy_uid: 2102 }
}

/// Règles de la chaîne `chain`, dans l'ordre.
fn chain<'a>(restore: &'a str, chain: &str) -> Vec<&'a str> {
    let prefix = format!("-A {chain} ");
    restore.lines().filter(|l| l.starts_with(&prefix)).collect()
}

fn position(rules: &[&str], rule: &str) -> usize {
    rules
        .iter()
        .position(|r| *r == rule)
        .unwrap_or_else(|| panic!("règle absente : {rule}\n{rules:#?}"))
}

#[test]
fn ini_05_mesh_is_detected_from_saved_rules() {
    assert_eq!(detect_mesh(ISTIO_SIDECAR), istio());
    assert_eq!(detect_mesh(ISTIO_AMBIENT), ambient());
    assert_eq!(detect_mesh(LINKERD), linkerd());
    assert_eq!(detect_mesh(NONE), Mesh::None);
    assert_eq!(detect_mesh(""), Mesh::None);
}

#[test]
fn ini_05_values_found_in_the_rules_are_kept() {
    let istio = ISTIO_SIDECAR
        .replace("--uid-owner 1337", "--uid-owner 1500")
        .replace("--gid-owner 1337", "--gid-owner 1501")
        .replace("127.0.0.6/32", "127.0.0.7/32");
    assert_eq!(
        detect_mesh(&istio),
        Mesh::IstioSidecar {
            proxy_uid: 1500,
            proxy_gid: 1501,
            inbound_source: "127.0.0.7/32".into(),
        }
    );
    let ambient = ISTIO_AMBIENT.replace("0x539/0xfff", "0x600/0xfff");
    assert_eq!(
        detect_mesh(&ambient),
        Mesh::IstioAmbient {
            mark: "0x600/0xfff".into()
        }
    );
    let linkerd = LINKERD.replace("--uid-owner 2102", "--uid-owner 1500");
    assert_eq!(detect_mesh(&linkerd), Mesh::Linkerd { proxy_uid: 1500 });
}

#[test]
fn ini_06_istio_sidecar_rules() {
    let r = render(&RulesConfig {
        mesh: istio(),
        inbound_ports: InboundPorts::List(vec![8080]),
        exclude_outbound_cidrs: vec!["10.96.0.1/32".into()],
        ..RulesConfig::default()
    });
    assert!(
        !r.restore.contains("MIMIC_INBOUND"),
        "pas de chaîne entrante :\n{}",
        r.restore
    );
    assert_eq!(
        r.jumps,
        vec![Jump {
            chain: "OUTPUT",
            target: OUTBOUND_CHAIN,
            first: true
        }]
    );
    let out = chain(&r.restore, OUTBOUND_CHAIN);
    let leg = position(
        &out,
        "-A MIMIC_OUTBOUND -s 127.0.0.6/32 -o lo -p tcp --dport 8080 -j REDIRECT --to-ports 15106",
    );
    let own = position(
        &out,
        "-A MIMIC_OUTBOUND -m owner --uid-owner 1338 -j RETURN",
    );
    let mesh_uid = position(
        &out,
        "-A MIMIC_OUTBOUND -m owner --uid-owner 1337 -j RETURN",
    );
    let mesh_gid = position(
        &out,
        "-A MIMIC_OUTBOUND -m owner --gid-owner 1337 -j RETURN",
    );
    let cidr = position(&out, "-A MIMIC_OUTBOUND -d 10.96.0.1/32 -j RETURN");
    let redirect = position(
        &out,
        "-A MIMIC_OUTBOUND -p tcp -j REDIRECT --to-ports 15101",
    );
    assert!(leg < own && own < mesh_uid && mesh_uid < mesh_gid && mesh_gid < cidr);
    assert_eq!(redirect, out.len() - 1, "redirection sortante en dernier");
    // Ports du sidecar jamais redirigés sur le trajet entrant.
    for p in [15100, 15101, 15106] {
        assert!(
            out.contains(
                &format!("-A MIMIC_OUTBOUND -s 127.0.0.6/32 -o lo -p tcp --dport {p} -j RETURN")
                    .as_str()
            )
        );
    }

    let v6 = render(&RulesConfig {
        mesh: istio(),
        inbound_ports: InboundPorts::List(vec![8080]),
        ipv6: true,
        ..RulesConfig::default()
    });
    assert!(
        v6.restore.contains(
            "-A MIMIC_OUTBOUND -s ::6/128 -o lo -p tcp --dport 8080 -j REDIRECT --to-ports 15106"
        ),
        "{}",
        v6.restore
    );
}

#[test]
fn ini_07_istio_ambient_rules() {
    let r = render(&RulesConfig {
        mesh: ambient(),
        ..RulesConfig::default()
    });
    assert!(!r.restore.contains("MIMIC_INBOUND"));
    let out = chain(&r.restore, OUTBOUND_CHAIN);
    let excluded = position(
        &out,
        "-A MIMIC_OUTBOUND -o lo -m mark --mark 0x539/0xfff -p tcp --dport 15106 -j RETURN",
    );
    let leg = position(
        &out,
        "-A MIMIC_OUTBOUND -o lo -m mark --mark 0x539/0xfff -p tcp -j REDIRECT --to-ports 15106",
    );
    let own = position(
        &out,
        "-A MIMIC_OUTBOUND -m owner --uid-owner 1338 -j RETURN",
    );
    let mesh = position(
        &out,
        "-A MIMIC_OUTBOUND -m mark --mark 0x539/0xfff -j RETURN",
    );
    let redirect = position(
        &out,
        "-A MIMIC_OUTBOUND -p tcp -j REDIRECT --to-ports 15101",
    );
    assert!(excluded < leg && leg < own && own < mesh && mesh < redirect);
    assert!(r.jumps[0].first);
}

#[test]
fn ini_08_linkerd_rules() {
    let r = render(&RulesConfig {
        mesh: linkerd(),
        inbound_ports: InboundPorts::List(vec![8080, 9090]),
        exclude_inbound_ports: vec![9090],
        ..RulesConfig::default()
    });
    let out = chain(&r.restore, OUTBOUND_CHAIN);
    let leg = position(
        &out,
        "-A MIMIC_OUTBOUND -o lo -m owner --uid-owner 2102 -p tcp --dport 8080 -j REDIRECT --to-ports 15106",
    );
    assert!(!out.contains(
        &"-A MIMIC_OUTBOUND -o lo -m owner --uid-owner 2102 -p tcp --dport 9090 -j REDIRECT --to-ports 15106"
    ));
    let mesh = position(
        &out,
        "-A MIMIC_OUTBOUND -m owner --uid-owner 2102 -j RETURN",
    );
    assert!(leg < mesh);

    // Interception entrante désactivée : seulement le sortant.
    let r = render(&RulesConfig {
        mesh: linkerd(),
        inbound_ports: InboundPorts::None,
        ..RulesConfig::default()
    });
    assert!(!r.restore.contains("REDIRECT --to-ports 15106"));
    assert!(r.restore.contains("REDIRECT --to-ports 15101"));
}

#[test]
fn ini_09_mimic_jump_is_kept_first_in_output() {
    let jump = Jump {
        chain: "OUTPUT",
        target: OUTBOUND_CHAIN,
        first: true,
    };
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let insert = args(&[
        "-t",
        "nat",
        "-I",
        "OUTPUT",
        "1",
        "-p",
        "tcp",
        "-j",
        "MIMIC_OUTBOUND",
    ]);
    let delete = args(&[
        "-t",
        "nat",
        "-D",
        "OUTPUT",
        "-p",
        "tcp",
        "-j",
        "MIMIC_OUTBOUND",
    ]);

    // Absent : inséré en tête.
    assert_eq!(plan_jump(ISTIO_SIDECAR, &jump), vec![insert.clone()]);
    // Déjà en tête : rien.
    let first = ISTIO_SIDECAR.replace(
        "-A OUTPUT -p tcp -j ISTIO_OUTPUT",
        "-A OUTPUT -p tcp -j MIMIC_OUTBOUND\n-A OUTPUT -p tcp -j ISTIO_OUTPUT",
    );
    assert!(plan_jump(&first, &jump).is_empty());
    // Derrière celui du mesh, ou en double : retiré puis réinséré en tête, une fois.
    let behind = ISTIO_SIDECAR.replace(
        "-A OUTPUT -p tcp -j ISTIO_OUTPUT",
        "-A OUTPUT -p tcp -j ISTIO_OUTPUT\n-A OUTPUT -p tcp -j MIMIC_OUTBOUND",
    );
    assert_eq!(
        plan_jump(&behind, &jump),
        vec![delete.clone(), insert.clone()]
    );
    let twice = first.replace(
        "-A OUTPUT -p tcp -j ISTIO_OUTPUT",
        "-A OUTPUT -p tcp -j ISTIO_OUTPUT\n-A OUTPUT -p tcp -j MIMIC_OUTBOUND",
    );
    assert_eq!(
        plan_jump(&twice, &jump),
        vec![delete.clone(), delete, insert]
    );

    // Hors mesh, un saut ajouté en fin s'il manque, comme avant (INI-04).
    let append = Jump {
        chain: "PREROUTING",
        target: "MIMIC_INBOUND",
        first: false,
    };
    assert_eq!(
        plan_jump(NONE, &append),
        vec![args(&[
            "-t",
            "nat",
            "-A",
            "PREROUTING",
            "-p",
            "tcp",
            "-j",
            "MIMIC_INBOUND"
        ])]
    );
    let present = NONE.replace(
        "COMMIT",
        "-A PREROUTING -p tcp -j ISTIO_INBOUND\n-A PREROUTING -p tcp -j MIMIC_INBOUND\nCOMMIT",
    );
    assert!(plan_jump(&present, &append).is_empty());
}

#[test]
fn ini_10_mesh_mode_and_iptables_backend() {
    let saves = vec![
        ("iptables".to_string(), NONE.to_string()),
        ("iptables-nft".to_string(), NONE.to_string()),
        ("iptables-legacy".to_string(), LINKERD.to_string()),
    ];
    let auto = resolve_mesh(MeshMode::Auto, "iptables", &saves);
    assert_eq!(auto.mesh, linkerd());
    assert_eq!(auto.tool, "iptables-legacy");

    let none = resolve_mesh(MeshMode::None, "iptables", &saves);
    assert_eq!(none.mesh, Mesh::None);
    assert_eq!(none.tool, "iptables");

    let explicit = resolve_mesh(MeshMode::Linkerd, "iptables", &saves[..2]);
    assert_eq!(explicit.mesh, linkerd(), "valeurs par défaut de Linkerd");
    assert_eq!(explicit.tool, "iptables");
    // Mode explicite, règles d'un autre mesh : valeurs par défaut du mesh demandé.
    let other = resolve_mesh(
        MeshMode::Istio,
        "iptables",
        &[("iptables".to_string(), LINKERD.to_string())],
    );
    assert_eq!(other.mesh, istio());
    let detected = resolve_mesh(
        MeshMode::IstioAmbient,
        "iptables",
        &[("iptables".to_string(), ISTIO_AMBIENT.to_string())],
    );
    assert_eq!(detected.mesh, ambient());

    assert_eq!(MeshMode::parse("auto"), Ok(MeshMode::Auto));
    assert_eq!(MeshMode::parse(""), Ok(MeshMode::Auto));
    assert_eq!(MeshMode::parse("none"), Ok(MeshMode::None));
    assert_eq!(MeshMode::parse("istio"), Ok(MeshMode::Istio));
    assert_eq!(MeshMode::parse("istio-ambient"), Ok(MeshMode::IstioAmbient));
    assert_eq!(MeshMode::parse("linkerd"), Ok(MeshMode::Linkerd));
    assert!(MeshMode::parse("consul").is_err());
}
