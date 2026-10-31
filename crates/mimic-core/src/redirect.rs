//! Transparent redirection rules (spec §4): the `iptables-restore` input that `mimic-init`
//! installs in the pod network namespace. Pure text generation.

use std::collections::BTreeSet;

/// IP family of a rule set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    V4,
    V6,
}

impl Family {
    pub fn as_str(self) -> &'static str {
        match self {
            Family::V4 => "ipv4",
            Family::V6 => "ipv6",
        }
    }
}

/// What the rules redirect, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectConfig {
    pub inbound_port: u16,
    pub outbound_port: u16,
    /// UID whose outbound traffic is never redirected (the sidecar).
    pub proxy_uid: u32,
    /// Inbound ports never redirected.
    pub exclude_inbound_ports: BTreeSet<u16>,
}

/// `iptables-restore --noflush` input for `family`. Idempotent: the `MIMIC_*` chains are
/// re-declared (which empties them) and the built-in chains are flushed before their single
/// jump, so a second run, or a run after a partial failure, leaves one jump per chain.
pub fn rules(config: &RedirectConfig, family: Family) -> String {
    let reject = match family {
        Family::V4 => "icmp-port-unreachable",
        Family::V6 => "icmp6-port-unreachable",
    };
    let mut out = String::from(
        "*nat\n:MIMIC_INBOUND - [0:0]\n:MIMIC_OUTPUT - [0:0]\n\
         -F PREROUTING\n\
         -F OUTPUT\n\
         -A PREROUTING -p tcp -m addrtype --dst-type LOCAL -j MIMIC_INBOUND\n\
         -A OUTPUT -p tcp -j MIMIC_OUTPUT\n",
    );
    for port in &config.exclude_inbound_ports {
        out.push_str(&format!(
            "-A MIMIC_INBOUND -p tcp --dport {port} -j RETURN\n"
        ));
    }
    out.push_str(&format!(
        "-A MIMIC_INBOUND -p tcp -j REDIRECT --to-ports {}\n\
         -A MIMIC_OUTPUT -m owner --uid-owner {} -j RETURN\n\
         -A MIMIC_OUTPUT -o lo -j RETURN\n\
         -A MIMIC_OUTPUT -p tcp -j REDIRECT --to-ports {}\n\
         COMMIT\n\
         *filter\n:MIMIC_OUTPUT - [0:0]\n\
         -F OUTPUT\n\
         -A OUTPUT -p udp --dport 443 -j MIMIC_OUTPUT\n\
         -A MIMIC_OUTPUT -p udp --dport 443 -j REJECT --reject-with {reject}\n\
         COMMIT\n",
        config.inbound_port, config.proxy_uid, config.outbound_port
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(exclude: &[u16]) -> RedirectConfig {
        RedirectConfig {
            inbound_port: 15006,
            outbound_port: 15001,
            proxy_uid: 1337,
            exclude_inbound_ports: exclude.iter().copied().collect(),
        }
    }

    #[test]
    fn inj_05_nat_table_redirects_inbound_and_outbound_tcp() {
        let text = rules(&config(&[8081, 15100]), Family::V4);
        let l: Vec<&str> = text.lines().collect();
        let start = l.iter().position(|x| *x == "*nat").expect("nat table");
        let end = l[start..]
            .iter()
            .position(|x| *x == "COMMIT")
            .expect("commit")
            + start;
        assert_eq!(
            &l[start..=end],
            [
                "*nat",
                ":MIMIC_INBOUND - [0:0]",
                ":MIMIC_OUTPUT - [0:0]",
                "-F PREROUTING",
                "-F OUTPUT",
                "-A PREROUTING -p tcp -m addrtype --dst-type LOCAL -j MIMIC_INBOUND",
                "-A OUTPUT -p tcp -j MIMIC_OUTPUT",
                "-A MIMIC_INBOUND -p tcp --dport 8081 -j RETURN",
                "-A MIMIC_INBOUND -p tcp --dport 15100 -j RETURN",
                "-A MIMIC_INBOUND -p tcp -j REDIRECT --to-ports 15006",
                "-A MIMIC_OUTPUT -m owner --uid-owner 1337 -j RETURN",
                "-A MIMIC_OUTPUT -o lo -j RETURN",
                "-A MIMIC_OUTPUT -p tcp -j REDIRECT --to-ports 15001",
                "COMMIT",
            ]
        );
    }

    #[test]
    fn inj_05_filter_table_rejects_outbound_udp_443() {
        let text = rules(&config(&[]), Family::V4);
        let l: Vec<&str> = text.lines().collect();
        let start = l
            .iter()
            .position(|x| *x == "*filter")
            .expect("filter table");
        assert_eq!(
            &l[start..],
            [
                "*filter",
                ":MIMIC_OUTPUT - [0:0]",
                "-F OUTPUT",
                "-A OUTPUT -p udp --dport 443 -j MIMIC_OUTPUT",
                "-A MIMIC_OUTPUT -p udp --dport 443 -j REJECT --reject-with icmp-port-unreachable",
                "COMMIT",
            ]
        );
    }

    #[test]
    fn inj_05_ipv6_differs_only_by_the_reject_type() {
        let v4 = rules(&config(&[]), Family::V4);
        let v6 = rules(&config(&[]), Family::V6);
        assert!(v6.contains("-j REJECT --reject-with icmp6-port-unreachable"));
        assert!(!v6.contains(" icmp-port-unreachable"));
        assert_eq!(v4.replace("icmp-port", "icmp6-port"), v6);
    }

    #[test]
    fn inj_05_rules_are_deterministic_and_only_jump_from_built_in_chains() {
        let a = rules(&config(&[9000, 80, 80]), Family::V4);
        let b = rules(&config(&[80, 9000]), Family::V4);
        assert_eq!(a, b);
        assert!(a.find("--dport 80 ").unwrap() < a.find("--dport 9000 ").unwrap());
        for line in a.lines().filter(|l| l.starts_with("-A ")) {
            let chain = line.split_whitespace().nth(1).unwrap();
            if !chain.starts_with("MIMIC_") {
                assert!(
                    line.ends_with("-j MIMIC_INBOUND") || line.ends_with("-j MIMIC_OUTPUT"),
                    "built-in chain rule that is not a jump: {line}"
                );
            }
        }
        let count = |prefix: &str| a.lines().filter(|l| l.starts_with(prefix)).count();
        assert_eq!(count("-A PREROUTING "), 1);
        assert_eq!(count("-A OUTPUT "), 2, "one in nat, one in filter");
    }

    #[test]
    fn inj_22_built_in_chains_are_flushed_before_their_single_jump() {
        let text = rules(&config(&[80]), Family::V4);
        let l: Vec<&str> = text.lines().collect();
        let at = |x: &str| {
            l.iter()
                .position(|y| *y == x)
                .unwrap_or_else(|| panic!("{x}"))
        };
        assert!(
            at("-F PREROUTING")
                < at("-A PREROUTING -p tcp -m addrtype --dst-type LOCAL -j MIMIC_INBOUND")
        );
        assert!(at("-F OUTPUT") < at("-A OUTPUT -p tcp -j MIMIC_OUTPUT"));
        let filter = at("*filter");
        let flush = l.iter().rposition(|x| *x == "-F OUTPUT").unwrap();
        assert!(filter < flush && flush < at("-A OUTPUT -p udp --dport 443 -j MIMIC_OUTPUT"));
        assert_eq!(l.iter().filter(|x| x.starts_with("-F ")).count(), 3);
    }

    #[test]
    fn inj_05_ports_and_uid_come_from_the_config() {
        let c = RedirectConfig {
            inbound_port: 1111,
            outbound_port: 2222,
            proxy_uid: 4242,
            exclude_inbound_ports: BTreeSet::new(),
        };
        let text = rules(&c, Family::V4);
        assert!(text.contains("--to-ports 1111"));
        assert!(text.contains("--to-ports 2222"));
        assert!(text.contains("--uid-owner 4242"));
    }
}
