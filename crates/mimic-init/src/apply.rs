//! Application des règles : analyse des réglages, détection du mesh, `iptables-restore`
//! (spec §3.1). Partagée par `mimic-init` et le plugin CNI `mimic-cni`.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::{
    InboundPorts, MeshMode, RulesConfig, parse_cidrs, parse_pod_ip, parse_ports, plan_jump, render,
    resolve_mesh,
};

/// Réglages de `mimic-init`, tels que reçus (variables d'environnement ou annotation CNI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// UID du sidecar, dont le trafic n'est jamais redirigé.
    pub proxy_uid: u32,
    pub inbound_port: u16,
    pub outbound_port: u16,
    pub admin_port: u16,
    /// Ports entrants à intercepter : `*`, liste `8080,9090`, ou vide pour aucun.
    pub inbound_ports: String,
    pub exclude_inbound_ports: String,
    /// Désactive l'interception sortante.
    pub no_outbound: bool,
    pub exclude_outbound_cidrs: String,
    pub exclude_outbound_ports: String,
    /// Pose aussi les règles IPv6.
    pub ipv6: bool,
    /// Binaire iptables : `iptables` (nft) ou `iptables-legacy`, selon le noyau des noeuds.
    pub iptables: String,
    /// Service mesh du pod (spec §3.3.3).
    pub mesh: String,
    /// IP du pod : cible du trajet entrant en ambient (spec §3.3.2).
    pub pod_ip: Option<String>,
    /// Redirige aussi le trafic local du pod (tests en namespace réseau).
    pub intercept_loopback: bool,
    /// Affiche les règles sans les appliquer.
    pub dry_run: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            proxy_uid: 1338,
            inbound_port: 15106,
            outbound_port: 15101,
            admin_port: 15100,
            inbound_ports: "*".into(),
            exclude_inbound_ports: String::new(),
            no_outbound: false,
            exclude_outbound_cidrs: String::new(),
            exclude_outbound_ports: String::new(),
            ipv6: false,
            iptables: "iptables".into(),
            mesh: "auto".into(),
            pod_ip: None,
            intercept_loopback: false,
            dry_run: false,
        }
    }
}

/// Écrit une ligne du déroulé ; une sortie fermée n'arrête pas l'application des règles.
macro_rules! say {
    ($log:expr, $($arg:tt)*) => {
        let _ = writeln!($log, $($arg)*);
    };
}

/// Échec de `mimic-init`.
#[derive(Debug)]
pub enum Failure {
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

/// Valeurs analysées avant toute commande (spec §3.1) ; l'erreur nomme la variable.
struct Parsed {
    mode: MeshMode,
    inbound_ports: InboundPorts,
    exclude_inbound_ports: Vec<u16>,
    exclude_outbound_cidrs: Vec<String>,
    exclude_outbound_ports: Vec<u16>,
    pod_ip: Option<String>,
}

fn parse(args: &Settings) -> Result<Parsed, String> {
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

/// Pose les règles du pod (spec §3.1) : celles de `mimic-init`, et de `mimic-cni` dans le
/// namespace réseau du pod.
/// Le déroulé est écrit sur `log` : la sortie standard pour `mimic-init`, la sortie d'erreur
/// pour le plugin CNI, dont la sortie standard est réservée à son résultat.
pub fn apply(args: &Settings, log: &mut dyn Write) -> Result<(), Failure> {
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
    say!(
        log,
        "mimic-init: mesh {:?}, binaire {}",
        resolved.mesh,
        resolved.tool
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
        intercept_loopback: args.intercept_loopback,
    };
    let ip6tables = resolved.tool.replacen("iptables", "ip6tables", 1);
    let mut families = vec![(resolved.tool.as_str(), base.clone())];
    if args.ipv6 {
        families.push((ip6tables.as_str(), RulesConfig { ipv6: true, ..base }));
    }
    for (tool, config) in families {
        let rules = render(&config);
        if args.dry_run {
            say!(log, "# {tool}-restore --noflush\n{}", rules.restore);
            for j in &rules.jumps {
                let at = if j.first {
                    format!("-I {} 1", j.chain)
                } else {
                    format!("-A {}", j.chain)
                };
                say!(log, "# {tool} -t nat {at} -p tcp -j {}", j.target);
            }
            continue;
        }
        say!(
            log,
            "mimic-init: {tool}-restore --noflush\n{}",
            rules.restore
        );
        restore(&format!("{tool}-restore"), &rules.restore)?;
        let saved = save(tool)?;
        for j in &rules.jumps {
            for cmd in plan_jump(&saved, j) {
                iptables(tool, &cmd)?;
            }
        }
        say!(log, "mimic-init: règles {tool} posées");
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
