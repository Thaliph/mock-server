use clap::Parser;
use mimic_init::apply::{Failure, Settings, apply};

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
    /// Redirige aussi le trafic vers `127.0.0.1` et `::1` (tests en namespace réseau) ; par
    /// défaut, le trafic local du pod n'est pas intercepté (spec §3.1).
    #[arg(long, env = "MIMIC_INTERCEPT_LOOPBACK")]
    intercept_loopback: bool,
    /// Affiche les règles sans les appliquer.
    #[arg(long)]
    dry_run: bool,
}

fn main() {
    let a = Args::parse();
    let settings = Settings {
        proxy_uid: a.proxy_uid,
        inbound_port: a.inbound_port,
        outbound_port: a.outbound_port,
        admin_port: a.admin_port,
        inbound_ports: a.inbound_ports,
        exclude_inbound_ports: a.exclude_inbound_ports,
        no_outbound: a.no_outbound,
        exclude_outbound_cidrs: a.exclude_outbound_cidrs,
        exclude_outbound_ports: a.exclude_outbound_ports,
        ipv6: a.ipv6,
        iptables: a.iptables,
        mesh: a.mesh,
        pod_ip: a.pod_ip,
        intercept_loopback: a.intercept_loopback,
        dry_run: a.dry_run,
    };
    let code = match apply(&settings, &mut std::io::stdout()) {
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
