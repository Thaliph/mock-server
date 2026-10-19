use std::io::Write;
use std::process::{Command, Stdio};

use clap::Parser;
use mimic_init::{
    InboundPorts, MeshMode, RulesConfig, parse_cidrs, parse_pod_ip, parse_ports, plan_jump, render,
    resolve_mesh,
};

/// Pose les règles iptables qui redirigent le trafic du pod vers le sidecar mimic.
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// UID du sidecar, dont le trafic n'est jamais redirigé.
    #[arg(long, env = "MIMIC_PROXY_UID", default_value_t = 1338)]
    proxy_uid: u32,
    #[arg(long, env = "MIMIC_INBOUND_PORT", default_value_t = 15106)]
    inbound_port: u16,
    #[arg(long, env = "MIMIC_OUTBOUND_PORT", default_value_t = 15101)]
    outbound_port: u16,
    #[arg(long, env = "MIMIC_ADMIN_PORT", default_value_t = 15100)]
    admin_port: u16,
    /// Ports entrants à intercepter : `*`, liste `8080,9090`, ou vide pour aucun.
    #[arg(long, env = "MIMIC_INBOUND_PORTS", default_value = "*")]
    inbound_ports: String,
    #[arg(long, env = "MIMIC_EXCLUDE_INBOUND_PORTS", default_value = "")]
    exclude_inbound_ports: String,
    /// Désactive l'interception sortante.
    #[arg(long, env = "MIMIC_NO_OUTBOUND")]
    no_outbound: bool,
    #[arg(long, env = "MIMIC_EXCLUDE_OUTBOUND_CIDRS", default_value = "")]
    exclude_outbound_cidrs: String,
    #[arg(long, env = "MIMIC_EXCLUDE_OUTBOUND_PORTS", default_value = "")]
    exclude_outbound_ports: String,
    /// Pose aussi les règles IPv6.
    #[arg(long, env = "MIMIC_IPV6")]
    ipv6: bool,
    /// Binaire iptables : `iptables` (nft) ou `iptables-legacy`, selon le noyau des noeuds.
    #[arg(long, env = "MIMIC_IPTABLES", default_value = "iptables")]
    iptables: String,
    /// Service mesh du pod (spec §3.3.3) : `auto`, `none`, `istio`, `istio-ambient`, `linkerd`.
    #[arg(long, env = "MIMIC_MESH", default_value = "auto")]
    mesh: String,
    /// IP du pod (`status.podIP`) : cible du trajet entrant en ambient (spec §3.3.2).
    #[arg(long, env = "POD_IP")]
    pod_ip: Option<String>,
    /// Affiche les règles sans les appliquer.
    #[arg(long)]
    dry_run: bool,
}

/// Échec de `mimic-init`.
enum Failure {
    /// Valeur invalide, refusée avant toute commande : code 2.
    Config(String),
    /// Commande `iptables` en échec : code 1.
    Apply(String),
}

impl From<String> for Failure {
    fn from(e: String) -> Self {
        Failure::Apply(e)
    }
}

fn main() {
    let args = Args::parse();
    let code = match run(&args) {
        Ok(()) => return,
        Err(Failure::Config(e)) => {
            eprintln!("mimic-init: {e}");
            2
        }
        Err(Failure::Apply(e)) => {
            eprintln!("mimic-init: {e}");
            1
        }
    };
    std::process::exit(code);
}

/// Valeurs analysées avant toute commande (spec §3.1) ; l'erreur nomme la variable.
struct Parsed {
    mode: MeshMode,
    inbound_ports: InboundPorts,
    exclude_inbound_ports: Vec<u16>,
    exclude_outbound_cidrs: Vec<String>,
    exclude_outbound_ports: Vec<u16>,
    pod_ip: Option<String>,
}

fn parse(args: &Args) -> Result<Parsed, String> {
    let named = |var: &'static str| move |e: String| format!("{var}: {e}");
    Ok(Parsed {
        mode: MeshMode::parse(&args.mesh).map_err(named("MIMIC_MESH"))?,
        inbound_ports: InboundPorts::parse(&args.inbound_ports)
            .map_err(named("MIMIC_INBOUND_PORTS"))?,
        exclude_inbound_ports: parse_ports(&args.exclude_inbound_ports)
            .map_err(named("MIMIC_EXCLUDE_INBOUND_PORTS"))?,
        exclude_outbound_cidrs: parse_cidrs(&args.exclude_outbound_cidrs)
            .map_err(named("MIMIC_EXCLUDE_OUTBOUND_CIDRS"))?,
        exclude_outbound_ports: parse_ports(&args.exclude_outbound_ports)
            .map_err(named("MIMIC_EXCLUDE_OUTBOUND_PORTS"))?,
        pod_ip: args
            .pod_ip
            .as_deref()
            .filter(|ip| !ip.trim().is_empty())
            .map(parse_pod_ip)
            .transpose()
            .map_err(named("POD_IP"))?,
    })
}

fn run(args: &Args) -> Result<(), Failure> {
    let parsed = parse(args).map_err(Failure::Config)?;
    let mode = parsed.mode;
    // Règles déjà posées, par binaire : le mesh et la table où il les a mises.
    let mut saves = vec![];
    if mode != MeshMode::None {
        for tool in [args.iptables.as_str(), "iptables-nft", "iptables-legacy"] {
            if saves.iter().any(|(t, _)| t == tool) {
                continue;
            }
            if let Ok(saved) = save(tool) {
                saves.push((tool.to_string(), saved));
            }
        }
    }
    let resolved = resolve_mesh(mode, &args.iptables, &saves);
    println!(
        "mimic-init: mesh {:?}, binaire {}",
        resolved.mesh, resolved.tool
    );
    let base = RulesConfig {
        proxy_uid: args.proxy_uid,
        inbound_port: args.inbound_port,
        outbound_port: args.outbound_port,
        admin_port: args.admin_port,
        inbound_ports: parsed.inbound_ports,
        exclude_inbound_ports: parsed.exclude_inbound_ports,
        outbound: !args.no_outbound,
        exclude_outbound_cidrs: parsed.exclude_outbound_cidrs,
        exclude_outbound_ports: parsed.exclude_outbound_ports,
        ipv6: false,
        mesh: resolved.mesh,
        pod_ip: parsed.pod_ip,
    };
    let ip6tables = resolved.tool.replacen("iptables", "ip6tables", 1);
    let mut families = vec![(resolved.tool.as_str(), base.clone())];
    if args.ipv6 {
        families.push((ip6tables.as_str(), RulesConfig { ipv6: true, ..base }));
    }
    for (tool, config) in families {
        let rules = render(&config);
        if args.dry_run {
            println!("# {tool}-restore --noflush\n{}", rules.restore);
            for j in &rules.jumps {
                let at = if j.first {
                    format!("-I {} 1", j.chain)
                } else {
                    format!("-A {}", j.chain)
                };
                println!("# {tool} -t nat {at} -p tcp -j {}", j.target);
            }
            continue;
        }
        println!("mimic-init: {tool}-restore --noflush\n{}", rules.restore);
        restore(&format!("{tool}-restore"), &rules.restore)?;
        let saved = save(tool)?;
        for j in &rules.jumps {
            for cmd in plan_jump(&saved, j) {
                iptables(tool, &cmd)?;
            }
        }
        println!("mimic-init: règles {tool} posées");
    }
    Ok(())
}

/// Table `nat` telle que `<binaire>-save` la donne.
fn save(tool: &str) -> Result<String, String> {
    let out = Command::new(format!("{tool}-save"))
        .args(["-t", "nat"])
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("{tool}-save: {e}"))?;
    if !out.status.success() {
        return Err(format!("{tool}-save a échoué ({})", out.status));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("{tool}-save: {e}"))
}

fn restore(tool: &str, input: &str) -> Result<(), String> {
    let mut child = Command::new(tool)
        .arg("--noflush")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{tool}: {e}"))?;
    child
        .stdin
        .take()
        .expect("stdin capturé")
        .write_all(input.as_bytes())
        .map_err(|e| format!("{tool}: {e}"))?;
    let status = child.wait().map_err(|e| format!("{tool}: {e}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("{tool} a échoué ({status})"))
}

fn iptables(tool: &str, args: &[String]) -> Result<(), String> {
    let status = Command::new(tool)
        .args(args)
        .status()
        .map_err(|e| format!("{tool}: {e}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("{tool} {} a échoué ({status})", args.join(" ")))
}
