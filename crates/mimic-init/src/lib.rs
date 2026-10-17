//! Génération des règles iptables de redirection vers le sidecar (spec §3.1).
//!
//! La génération est pure et testée ; l'application est faite par le binaire `mimic-init`.

use std::fmt::Write as _;

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
}

impl Default for RulesConfig {
    fn default() -> Self {
        Self {
            proxy_uid: 1337,
            inbound_port: 15006,
            outbound_port: 15001,
            admin_port: 15000,
            inbound_ports: InboundPorts::All,
            exclude_inbound_ports: vec![],
            outbound: true,
            exclude_outbound_cidrs: vec![],
            exclude_outbound_ports: vec![],
            ipv6: false,
        }
    }
}

/// Règles à poser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rules {
    /// Entrée de `iptables-restore --noflush` : déclare (et vide) les chaînes mimic puis les remplit.
    pub restore: String,
    /// Sauts depuis les chaînes du noyau, à ajouter seulement s'ils sont absents
    /// (`iptables -t nat -C` puis `-A`), pour qu'une deuxième application ne duplique rien.
    pub jumps: Vec<Vec<String>>,
}

pub fn render(c: &RulesConfig) -> Rules {
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
        let loopback = if c.ipv6 { "::1/128" } else { "127.0.0.1/32" };
        let _ = writeln!(
            r,
            "-A {OUTBOUND_CHAIN} -m owner --uid-owner {} -j RETURN",
            c.proxy_uid
        );
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
        jumps.push(jump("OUTPUT", OUTBOUND_CHAIN));
    }
    r.push_str("COMMIT\n");
    Rules { restore: r, jumps }
}

fn sidecar_ports(c: &RulesConfig) -> Vec<u16> {
    vec![c.admin_port, c.inbound_port, c.outbound_port]
}

fn jump(from: &str, to: &str) -> Vec<String> {
    ["-t", "nat", from, "-p", "tcp", "-j", to]
        .map(String::from)
        .to_vec()
}

pub fn parse_ports(s: &str) -> Result<Vec<u16>, String> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| p.parse::<u16>().map_err(|_| format!("port invalide: {p}")))
        .collect()
}

pub fn parse_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
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
            inbound_ports: InboundPorts::List(vec![8080, 9090, 15000]),
            exclude_inbound_ports: vec![9100],
            ..RulesConfig::default()
        });
        let l = lines(&r);
        assert!(l.contains(&"-A MIMIC_INBOUND -p tcp --dport 8080 -j REDIRECT --to-ports 15006"));
        assert!(l.contains(&"-A MIMIC_INBOUND -p tcp --dport 9090 -j REDIRECT --to-ports 15006"));
        assert!(!l.contains(&"-A MIMIC_INBOUND -p tcp --dport 15000 -j REDIRECT --to-ports 15006"));
        for p in [9100, 15000, 15001, 15006] {
            assert!(l.contains(&format!("-A MIMIC_INBOUND -p tcp --dport {p} -j RETURN").as_str()));
        }
    }

    #[test]
    fn ini_02_all_inbound_ports() {
        let r = render(&RulesConfig::default());
        let l = lines(&r);
        let redirect = l
            .iter()
            .position(|x| *x == "-A MIMIC_INBOUND -p tcp -j REDIRECT --to-ports 15006")
            .unwrap();
        let ret = l
            .iter()
            .position(|x| *x == "-A MIMIC_INBOUND -p tcp --dport 15006 -j RETURN")
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
            "-A MIMIC_OUTBOUND -m owner --uid-owner 1337 -j RETURN",
            "-A MIMIC_OUTBOUND -o lo ! -d 127.0.0.1/32 -j RETURN",
            "-A MIMIC_OUTBOUND -d 10.96.0.1/32 -j RETURN",
            "-A MIMIC_OUTBOUND -p tcp --dport 53 -j RETURN",
            "-A MIMIC_OUTBOUND -p tcp --dport 5432 -j RETURN",
            "-A MIMIC_OUTBOUND -p tcp --dport 15000 -j RETURN",
            "-A MIMIC_OUTBOUND -p tcp --dport 15001 -j RETURN",
            "-A MIMIC_OUTBOUND -p tcp --dport 15006 -j RETURN",
            "-A MIMIC_OUTBOUND -p tcp -j REDIRECT --to-ports 15001",
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
        assert!(r.restore.contains("! -d ::1/128"));
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
