//! Génération des règles iptables de redirection vers le sidecar (spec §3.1).
//!
//! La génération est pure et testée ; l'application est faite par le binaire `mimic-init`.

use std::fmt::Write as _;
use std::net::IpAddr;

/// Chaîne des règles entrantes.
pub const INBOUND_CHAIN: &str = "MIMIC_INBOUND";
/// Chaîne des règles sortantes.
pub const OUTBOUND_CHAIN: &str = "MIMIC_OUTBOUND";

/// Ports redirigés en entrée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundPorts {
    /// Tout le TCP entrant.
    All,
    /// Aucun : interception entrante désactivée.
    None,
    List(Vec<u16>),
}

impl InboundPorts {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim() {
            "*" => Ok(InboundPorts::All),
            "" | "-" => Ok(InboundPorts::None),
            list => parse_ports(list).map(InboundPorts::List),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesConfig {
    /// UID du sidecar : son trafic n'est jamais redirigé.
    pub proxy_uid: u32,
    pub inbound_port: u16,
    pub outbound_port: u16,
    /// Port de l'API admin du sidecar.
    pub admin_port: u16,
    pub inbound_ports: InboundPorts,
    /// Ports entrants jamais redirigés, en plus des ports du sidecar (exclus dans les deux sens).
    pub exclude_inbound_ports: Vec<u16>,
    /// Interception sortante.
    pub outbound: bool,
    pub exclude_outbound_cidrs: Vec<String>,
    pub exclude_outbound_ports: Vec<u16>,
    /// Règles pour `ip6tables` : les CIDR IPv4 sont ignorés et inversement.
    pub ipv6: bool,
    /// Service mesh du pod (spec §3.3) : change les règles posées.
    pub mesh: Mesh,
    /// IP du pod (`POD_IP`), cible du trajet entrant en ambient (spec §3.3.2).
    pub pod_ip: Option<String>,
}

impl Default for RulesConfig {
    fn default() -> Self {
        Self {
            proxy_uid: 1338,
            inbound_port: 15106,
            outbound_port: 15101,
            admin_port: 15100,
            inbound_ports: InboundPorts::All,
            exclude_inbound_ports: vec![],
            outbound: true,
            exclude_outbound_cidrs: vec![],
            exclude_outbound_ports: vec![],
            ipv6: false,
            mesh: Mesh::None,
            pod_ip: None,
        }
    }
}

/// Service mesh détecté dans les règles déjà posées, avec ses valeurs (spec §3.3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mesh {
    None,
    IstioSidecar {
        proxy_uid: u32,
        proxy_gid: u32,
        /// Adresse d'où Envoy rappelle l'application (IPv4).
        inbound_source: String,
    },
    IstioAmbient {
        /// Marque des paquets de ztunnel.
        mark: String,
    },
    Linkerd {
        proxy_uid: u32,
    },
}

/// Valeur de `MIMIC_MESH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeshMode {
    Auto,
    None,
    Istio,
    IstioAmbient,
    Linkerd,
}

/// Source des connexions d'Envoy vers l'application, en IPv6.
pub const ISTIO_INBOUND_SOURCE_V6: &str = "::6/128";

impl MeshMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim() {
            "" | "auto" => Ok(MeshMode::Auto),
            "none" => Ok(MeshMode::None),
            "istio" => Ok(MeshMode::Istio),
            "istio-ambient" => Ok(MeshMode::IstioAmbient),
            "linkerd" => Ok(MeshMode::Linkerd),
            other => Err(format!(
                "mesh inconnu: {other} (auto, none, istio, istio-ambient, linkerd)"
            )),
        }
    }
}

impl Mesh {
    /// Valeurs par défaut du mesh, quand ses règles ne sont pas (encore) posées.
    pub fn defaults(mode: MeshMode) -> Mesh {
        match mode {
            MeshMode::Auto | MeshMode::None => Mesh::None,
            MeshMode::Istio => Mesh::IstioSidecar {
                proxy_uid: 1337,
                proxy_gid: 1337,
                inbound_source: "127.0.0.6/32".into(),
            },
            MeshMode::IstioAmbient => Mesh::IstioAmbient {
                mark: "0x539/0xfff".into(),
            },
            MeshMode::Linkerd => Mesh::Linkerd { proxy_uid: 2102 },
        }
    }

    fn mode(&self) -> MeshMode {
        match self {
            Mesh::None => MeshMode::None,
            Mesh::IstioSidecar { .. } => MeshMode::Istio,
            Mesh::IstioAmbient { .. } => MeshMode::IstioAmbient,
            Mesh::Linkerd { .. } => MeshMode::Linkerd,
        }
    }
}

/// Valeur qui suit `option` dans la première règle de `chain` qui la porte.
fn rule_value<'a>(saved: &'a str, chain: &str, option: &str) -> Option<&'a str> {
    let prefix = format!("-A {chain} ");
    saved
        .lines()
        .filter(|l| l.starts_with(&prefix))
        .find_map(|l| {
            let mut words = l.split_whitespace();
            words.find(|w| *w == option)?;
            words.next()
        })
}

/// Mesh dont les règles figurent dans la sortie de `iptables-save -t nat` (spec §3.3.3).
pub fn detect_mesh(saved: &str) -> Mesh {
    let declared = |chain: &str| saved.lines().any(|l| l.starts_with(&format!(":{chain} ")));
    let number = |chain: &str, option: &str, default: u32| {
        rule_value(saved, chain, option)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    if declared("ISTIO_PRERT") {
        return Mesh::IstioAmbient {
            mark: rule_value(saved, "ISTIO_OUTPUT", "--mark")
                .filter(|m| *m != "0x111/0xfff")
                .or_else(|| rule_value(saved, "ISTIO_PRERT", "--mark"))
                .unwrap_or("0x539/0xfff")
                .into(),
        };
    }
    if declared("ISTIO_OUTPUT") {
        // `-A ISTIO_OUTPUT -s 127.0.0.6/32 -o lo -j RETURN`
        let source = saved
            .lines()
            .filter(|l| {
                l.starts_with("-A ISTIO_OUTPUT ")
                    && l.contains(" -o lo ")
                    && l.ends_with("-j RETURN")
            })
            .find_map(|l| {
                let mut words = l.split_whitespace();
                words.find(|w| *w == "-s")?;
                words.next()
            })
            .unwrap_or("127.0.0.6/32");
        return Mesh::IstioSidecar {
            proxy_uid: number("ISTIO_OUTPUT", "--uid-owner", 1337),
            proxy_gid: number("ISTIO_OUTPUT", "--gid-owner", 1337),
            inbound_source: source.into(),
        };
    }
    if declared("PROXY_INIT_OUTPUT") {
        return Mesh::Linkerd {
            proxy_uid: number("PROXY_INIT_OUTPUT", "--uid-owner", 2102),
        };
    }
    Mesh::None
}

/// Binaire iptables à utiliser et mesh retenu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub tool: String,
    pub mesh: Mesh,
}

/// Mesh retenu pour `mode`, et binaire iptables à utiliser : celui où le mesh a posé ses
/// règles, pour que les deux jeux soient dans la même table (spec §3.3.3). `saves` donne la
/// sortie de `<binaire>-save -t nat` pour chaque binaire essayé, dans l'ordre de préférence.
pub fn resolve_mesh(mode: MeshMode, configured: &str, saves: &[(String, String)]) -> Resolved {
    let fallback = Resolved {
        tool: configured.into(),
        mesh: Mesh::defaults(mode),
    };
    if mode == MeshMode::None {
        return fallback;
    }
    saves
        .iter()
        .map(|(tool, saved)| Resolved {
            tool: tool.clone(),
            mesh: detect_mesh(saved),
        })
        .find(|r| r.mesh != Mesh::None && (mode == MeshMode::Auto || r.mesh.mode() == mode))
        .unwrap_or(fallback)
}

/// Saut depuis une chaîne du noyau vers une chaîne mimic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Jump {
    pub chain: &'static str,
    pub target: &'static str,
    /// En tête de la chaîne, avant celles du mesh.
    pub first: bool,
}

impl Jump {
    fn rule(&self) -> [&str; 4] {
        ["-p", "tcp", "-j", self.target]
    }
}

/// Commandes `iptables` qui posent le saut, d'après la table `nat` actuelle (`saved`) :
/// rien s'il est déjà là (en tête s'il doit l'être), sinon ajout en fin, ou retrait des
/// occurrences mal placées puis insertion en position 1.
pub fn plan_jump(saved: &str, jump: &Jump) -> Vec<Vec<String>> {
    let rule = jump.rule();
    let wanted = format!("-A {} {}", jump.chain, rule.join(" "));
    let prefix = format!("-A {} ", jump.chain);
    let chain: Vec<&str> = saved
        .lines()
        .map(str::trim_end)
        .filter(|l| l.starts_with(&prefix))
        .collect();
    let present = chain.iter().filter(|l| **l == wanted).count();
    let command = |head: &[&str]| {
        ["-t", "nat"]
            .iter()
            .chain(head)
            .chain(rule.iter())
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    };
    if !jump.first {
        return if present > 0 {
            vec![]
        } else {
            vec![command(&["-A", jump.chain])]
        };
    }
    if present == 1 && chain.first() == Some(&wanted.as_str()) {
        return vec![];
    }
    let mut plan = vec![command(&["-D", jump.chain]); present];
    plan.push(command(&["-I", jump.chain, "1"]));
    plan
}

/// Règles à poser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rules {
    /// Entrée de `iptables-restore --noflush` : déclare (et vide) les chaînes mimic puis les remplit.
    pub restore: String,
    /// Sauts depuis les chaînes du noyau, posés selon `plan_jump` pour qu'une deuxième
    /// application ne duplique rien.
    pub jumps: Vec<Jump>,
}

pub fn render(c: &RulesConfig) -> Rules {
    if c.mesh != Mesh::None {
        return render_mesh(c);
    }
    let mut r = String::from("*nat\n");
    let inbound = c.inbound_ports != InboundPorts::None;
    if inbound {
        let _ = writeln!(r, ":{INBOUND_CHAIN} - [0:0]");
    }
    if c.outbound {
        let _ = writeln!(r, ":{OUTBOUND_CHAIN} - [0:0]");
    }
    let mut jumps = vec![];

    if inbound {
        let mut excluded = sidecar_ports(c);
        excluded.extend(&c.exclude_inbound_ports);
        excluded.sort_unstable();
        excluded.dedup();
        for p in &excluded {
            let _ = writeln!(r, "-A {INBOUND_CHAIN} -p tcp --dport {p} -j RETURN");
        }
        match &c.inbound_ports {
            InboundPorts::All => {
                let _ = writeln!(
                    r,
                    "-A {INBOUND_CHAIN} -p tcp -j REDIRECT --to-ports {}",
                    c.inbound_port
                );
            }
            InboundPorts::List(ports) => {
                for p in ports.iter().filter(|p| !excluded.contains(p)) {
                    let _ = writeln!(
                        r,
                        "-A {INBOUND_CHAIN} -p tcp --dport {p} -j REDIRECT --to-ports {}",
                        c.inbound_port
                    );
                }
            }
            InboundPorts::None => {}
        }
        jumps.push(jump("PREROUTING", INBOUND_CHAIN));
    }

    if c.outbound {
        let _ = writeln!(
            r,
            "-A {OUTBOUND_CHAIN} -m owner --uid-owner {} -j RETURN",
            c.proxy_uid
        );
        outbound_rules(c, &mut r);
        jumps.push(jump("OUTPUT", OUTBOUND_CHAIN));
    }
    r.push_str("COMMIT\n");
    Rules { restore: r, jumps }
}

/// Règles dans un pod du mesh (spec §3.3.2) : rien en `PREROUTING`, tout dans
/// `MIMIC_OUTBOUND`, sautée en tête de `OUTPUT`.
/// Cible du trajet entrant dans un mesh. En ambient, ztunnel se connecte à l'application
/// avec l'adresse source du client : une redirection vers 127.0.0.1 rendrait la réponse
/// impossible à router, d'où un DNAT vers l'IP du pod quand elle est connue (spec §3.3.2).
fn inbound_target(c: &RulesConfig) -> String {
    let port = c.inbound_port;
    match (&c.mesh, c.pod_ip.as_deref()) {
        (Mesh::IstioAmbient { .. }, Some(ip)) if ip.contains(':') == c.ipv6 => {
            if c.ipv6 {
                format!("DNAT --to-destination [{ip}]:{port}")
            } else {
                format!("DNAT --to-destination {ip}:{port}")
            }
        }
        _ => format!("REDIRECT --to-ports {port}"),
    }
}

fn render_mesh(c: &RulesConfig) -> Rules {
    let inbound = c.inbound_ports != InboundPorts::None;
    let mut r = String::from("*nat\n");
    if !inbound && !c.outbound {
        r.push_str("COMMIT\n");
        return Rules {
            restore: r,
            jumps: vec![],
        };
    }
    let _ = writeln!(r, ":{OUTBOUND_CHAIN} - [0:0]");

    if inbound {
        // Connexion du mesh vers l'application, sur la boucle locale.
        let leg = match &c.mesh {
            Mesh::IstioSidecar { inbound_source, .. } => {
                let source = if c.ipv6 {
                    ISTIO_INBOUND_SOURCE_V6
                } else {
                    inbound_source.as_str()
                };
                format!("-s {source} -o lo")
            }
            Mesh::IstioAmbient { mark } => format!("-o lo -m mark --mark {mark}"),
            Mesh::Linkerd { proxy_uid } => format!("-o lo -m owner --uid-owner {proxy_uid}"),
            Mesh::None => unreachable!("render_mesh hors mesh"),
        };
        let target = inbound_target(c);
        let mut excluded = sidecar_ports(c);
        excluded.extend(&c.exclude_inbound_ports);
        excluded.sort_unstable();
        excluded.dedup();
        for p in &excluded {
            let _ = writeln!(r, "-A {OUTBOUND_CHAIN} {leg} -p tcp --dport {p} -j RETURN");
        }
        match &c.inbound_ports {
            InboundPorts::All => {
                let _ = writeln!(r, "-A {OUTBOUND_CHAIN} {leg} -p tcp -j {target}");
            }
            InboundPorts::List(ports) => {
                for p in ports.iter().filter(|p| !excluded.contains(p)) {
                    let _ = writeln!(
                        r,
                        "-A {OUTBOUND_CHAIN} {leg} -p tcp --dport {p} -j {target}"
                    );
                }
            }
            InboundPorts::None => {}
        }
    }

    if c.outbound {
        // Le trafic de Mimic continue vers les chaînes du mesh ; celui du mesh n'entre jamais.
        let _ = writeln!(
            r,
            "-A {OUTBOUND_CHAIN} -m owner --uid-owner {} -j RETURN",
            c.proxy_uid
        );
        match &c.mesh {
            Mesh::IstioSidecar {
                proxy_uid,
                proxy_gid,
                ..
            } => {
                let _ = writeln!(
                    r,
                    "-A {OUTBOUND_CHAIN} -m owner --uid-owner {proxy_uid} -j RETURN"
                );
                let _ = writeln!(
                    r,
                    "-A {OUTBOUND_CHAIN} -m owner --gid-owner {proxy_gid} -j RETURN"
                );
            }
            Mesh::IstioAmbient { mark } => {
                let _ = writeln!(r, "-A {OUTBOUND_CHAIN} -m mark --mark {mark} -j RETURN");
            }
            Mesh::Linkerd { proxy_uid } => {
                let _ = writeln!(
                    r,
                    "-A {OUTBOUND_CHAIN} -m owner --uid-owner {proxy_uid} -j RETURN"
                );
            }
            Mesh::None => {}
        }
        outbound_rules(c, &mut r);
    }
    r.push_str("COMMIT\n");
    Rules {
        restore: r,
        jumps: vec![Jump {
            chain: "OUTPUT",
            target: OUTBOUND_CHAIN,
            first: true,
        }],
    }
}

/// Règles sortantes communes, après le `RETURN` des UID exclus (spec §3.1).
fn outbound_rules(c: &RulesConfig, r: &mut String) {
    let loopback = if c.ipv6 { "::1/128" } else { "127.0.0.1/32" };
    // Appel de l'application vers l'IP du pod : c'est du trafic entrant, pas une dépendance.
    let _ = writeln!(r, "-A {OUTBOUND_CHAIN} -o lo ! -d {loopback} -j RETURN");
    for cidr in c
        .exclude_outbound_cidrs
        .iter()
        .filter(|cidr| cidr.contains(':') == c.ipv6)
    {
        let _ = writeln!(r, "-A {OUTBOUND_CHAIN} -d {cidr} -j RETURN");
    }
    let mut ports = c.exclude_outbound_ports.clone();
    ports.extend(sidecar_ports(c));
    ports.sort_unstable();
    ports.dedup();
    for p in ports {
        let _ = writeln!(r, "-A {OUTBOUND_CHAIN} -p tcp --dport {p} -j RETURN");
    }
    let _ = writeln!(
        r,
        "-A {OUTBOUND_CHAIN} -p tcp -j REDIRECT --to-ports {}",
        c.outbound_port
    );
}

fn sidecar_ports(c: &RulesConfig) -> Vec<u16> {
    vec![c.admin_port, c.inbound_port, c.outbound_port]
}

fn jump(chain: &'static str, target: &'static str) -> Jump {
    Jump {
        chain,
        target,
        first: false,
    }
}

/// Éléments non vides d'une liste séparée par des virgules.
fn items(s: &str) -> impl Iterator<Item = &str> {
    s.split(',').map(str::trim).filter(|p| !p.is_empty())
}

/// Port TCP, de 1 à 65535.
pub fn parse_port(s: &str) -> Result<u16, String> {
    match s.trim().parse::<u16>() {
        Ok(p) if p > 0 => Ok(p),
        _ => Err(format!("port invalide: {s:?} (entier de 1 à 65535)")),
    }
}

pub fn parse_ports(s: &str) -> Result<Vec<u16>, String> {
    items(s).map(parse_port).collect()
}

/// CIDR IPv4 ou IPv6 (`10.0.0.0/8`, `fd00::/8`) ; une adresse seule vaut `/32` ou `/128`.
/// Rendu sous la forme canonique `adresse/préfixe` : rien n'est recopié tel quel dans les
/// règles (spec §3.1).
pub fn parse_cidr(s: &str) -> Result<String, String> {
    let invalid = || format!("CIDR invalide: {s:?} (adresse IPv4 ou IPv6, /préfixe facultatif)");
    let (addr, prefix) = match s.trim().split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (s.trim(), None),
    };
    let ip: IpAddr = addr.parse().map_err(|_| invalid())?;
    let max = if ip.is_ipv6() { 128 } else { 32 };
    let len = match prefix {
        None => max,
        // Des chiffres seulement : ni signe, ni espace, ni second `/`.
        Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            p.parse::<u8>().map_err(|_| invalid())?
        }
        Some(_) => return Err(invalid()),
    };
    if len > max {
        return Err(invalid());
    }
    Ok(format!("{ip}/{len}"))
}

pub fn parse_cidrs(s: &str) -> Result<Vec<String>, String> {
    items(s).map(parse_cidr).collect()
}

/// IP du pod (`POD_IP`), cible du DNAT en ambient.
pub fn parse_pod_ip(s: &str) -> Result<String, String> {
    s.trim()
        .parse::<IpAddr>()
        .map(|ip| ip.to_string())
        .map_err(|_| format!("adresse IP invalide: {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(r: &Rules) -> Vec<&str> {
        r.restore.lines().collect()
    }

    #[test]
    fn ini_01_listed_inbound_ports_are_redirected() {
        let r = render(&RulesConfig {
            inbound_ports: InboundPorts::List(vec![8080, 9090, 15100]),
            exclude_inbound_ports: vec![9100],
            ..RulesConfig::default()
        });
        let l = lines(&r);
        assert!(l.contains(&"-A MIMIC_INBOUND -p tcp --dport 8080 -j REDIRECT --to-ports 15106"));
        assert!(l.contains(&"-A MIMIC_INBOUND -p tcp --dport 9090 -j REDIRECT --to-ports 15106"));
        assert!(!l.contains(&"-A MIMIC_INBOUND -p tcp --dport 15100 -j REDIRECT --to-ports 15106"));
        for p in [9100, 15100, 15101, 15106] {
            assert!(l.contains(&format!("-A MIMIC_INBOUND -p tcp --dport {p} -j RETURN").as_str()));
        }
    }

    #[test]
    fn ini_02_all_inbound_ports() {
        let r = render(&RulesConfig::default());
        let l = lines(&r);
        let redirect = l
            .iter()
            .position(|x| *x == "-A MIMIC_INBOUND -p tcp -j REDIRECT --to-ports 15106")
            .unwrap();
        let ret = l
            .iter()
            .position(|x| *x == "-A MIMIC_INBOUND -p tcp --dport 15106 -j RETURN")
            .unwrap();
        assert!(ret < redirect, "les exclusions précèdent la redirection");
        assert_eq!(r.jumps[0], jump("PREROUTING", INBOUND_CHAIN));
    }

    #[test]
    fn ini_03_outbound_exclusions_come_first() {
        let r = render(&RulesConfig {
            exclude_outbound_cidrs: vec!["10.96.0.1/32".into(), "fd00::/8".into()],
            exclude_outbound_ports: vec![5432, 53],
            ..RulesConfig::default()
        });
        let l = lines(&r);
        let expected = [
            "-A MIMIC_OUTBOUND -m owner --uid-owner 1338 -j RETURN",
            "-A MIMIC_OUTBOUND -o lo -j RETURN",
            "-A MIMIC_OUTBOUND -d 10.96.0.1/32 -j RETURN",
            "-A MIMIC_OUTBOUND -p tcp --dport 53 -j RETURN",
            "-A MIMIC_OUTBOUND -p tcp --dport 5432 -j RETURN",
            "-A MIMIC_OUTBOUND -p tcp --dport 15100 -j RETURN",
            "-A MIMIC_OUTBOUND -p tcp --dport 15101 -j RETURN",
            "-A MIMIC_OUTBOUND -p tcp --dport 15106 -j RETURN",
            "-A MIMIC_OUTBOUND -p tcp -j REDIRECT --to-ports 15101",
        ];
        let outbound: Vec<&str> = l
            .iter()
            .copied()
            .filter(|x| x.starts_with("-A MIMIC_OUTBOUND"))
            .collect();
        assert_eq!(outbound, expected);
        assert!(
            !r.restore.contains("fd00::/8"),
            "CIDR IPv6 absent des règles IPv4"
        );
        assert_eq!(r.jumps[1], jump("OUTPUT", OUTBOUND_CHAIN));
    }

    #[test]
    fn ipv6_rules_use_v6_loopback_and_cidrs() {
        let r = render(&RulesConfig {
            ipv6: true,
            exclude_outbound_cidrs: vec!["10.0.0.0/8".into(), "fd00::/8".into()],
            ..RulesConfig::default()
        });
        assert!(!r.restore.contains("! -d ::1/128"));
        assert!(r.restore.contains("-A MIMIC_OUTBOUND -o lo -j RETURN"));
        let r = render(&RulesConfig {
            ipv6: true,
            intercept_loopback: true,
            ..RulesConfig::default()
        });
        assert!(r.restore.contains("-o lo ! -d ::1/128 -j RETURN"));
        assert!(r.restore.contains("-d fd00::/8 -j RETURN"));
        assert!(!r.restore.contains("10.0.0.0/8"));
    }

    #[test]
    fn interception_can_be_limited() {
        let r = render(&RulesConfig {
            inbound_ports: InboundPorts::None,
            outbound: false,
            ..RulesConfig::default()
        });
        assert_eq!(r.restore, "*nat\nCOMMIT\n");
        assert!(r.jumps.is_empty());
    }

    #[test]
    fn parsing() {
        assert_eq!(InboundPorts::parse("*"), Ok(InboundPorts::All));
        assert_eq!(InboundPorts::parse(""), Ok(InboundPorts::None));
        assert_eq!(
            InboundPorts::parse("80, 8080"),
            Ok(InboundPorts::List(vec![80, 8080]))
        );
        assert!(InboundPorts::parse("http").is_err());
    }
}
