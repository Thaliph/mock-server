use std::io::Write;
use std::process::{Command, Stdio};

use clap::Parser;
use mimic_init::{InboundPorts, RulesConfig, parse_list, parse_ports, render};

/// Pose les règles iptables qui redirigent le trafic du pod vers le sidecar mimic.
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// UID du sidecar, dont le trafic n'est jamais redirigé.
    #[arg(long, env = "MIMIC_PROXY_UID", default_value_t = 1337)]
    proxy_uid: u32,
    #[arg(long, env = "MIMIC_INBOUND_PORT", default_value_t = 15006)]
    inbound_port: u16,
    #[arg(long, env = "MIMIC_OUTBOUND_PORT", default_value_t = 15001)]
    outbound_port: u16,
    #[arg(long, env = "MIMIC_ADMIN_PORT", default_value_t = 15000)]
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
    /// Affiche les règles sans les appliquer.
    #[arg(long)]
    dry_run: bool,
}

fn main() {
    let args = Args::parse();
    if let Err(e) = run(&args) {
        eprintln!("mimic-init: {e}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> Result<(), String> {
    let base = RulesConfig {
        proxy_uid: args.proxy_uid,
        inbound_port: args.inbound_port,
        outbound_port: args.outbound_port,
        admin_port: args.admin_port,
        inbound_ports: InboundPorts::parse(&args.inbound_ports)?,
        exclude_inbound_ports: parse_ports(&args.exclude_inbound_ports)?,
        outbound: !args.no_outbound,
        exclude_outbound_cidrs: parse_list(&args.exclude_outbound_cidrs),
        exclude_outbound_ports: parse_ports(&args.exclude_outbound_ports)?,
        ipv6: false,
    };
    let ip6tables = args.iptables.replacen("iptables", "ip6tables", 1);
    let mut families = vec![(args.iptables.as_str(), base.clone())];
    if args.ipv6 {
        families.push((ip6tables.as_str(), RulesConfig { ipv6: true, ..base }));
    }
    for (tool, config) in families {
        let rules = render(&config);
        if args.dry_run {
            println!("# {tool}-restore --noflush\n{}", rules.restore);
            for j in &rules.jumps {
                println!("# {tool} -t nat -A {}", j[2..].join(" "));
            }
            continue;
        }
        restore(&format!("{tool}-restore"), &rules.restore)?;
        for j in &rules.jumps {
            ensure_jump(tool, j)?;
        }
        println!("mimic-init: règles {tool} posées");
    }
    Ok(())
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

/// Ajoute un saut vers une chaîne mimic s'il n'existe pas déjà.
fn ensure_jump(tool: &str, jump: &[String]) -> Result<(), String> {
    // jump = ["-t", "nat", CHAÎNE, règle...]
    let (table, rest) = jump.split_at(2);
    let (chain, rule) = rest.split_first().expect("chaîne présente");
    let exists = Command::new(tool)
        .args(table)
        .arg("-C")
        .arg(chain)
        .args(rule)
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("{tool}: {e}"))?
        .success();
    if exists {
        return Ok(());
    }
    let status = Command::new(tool)
        .args(table)
        .arg("-A")
        .arg(chain)
        .args(rule)
        .status()
        .map_err(|e| format!("{tool}: {e}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("{tool} -A {chain} a échoué ({status})"))
}
