//! Bounded metric/log labels for HTTP exchanges: authority normalisation, status class and the
//! authority budget (spec §4.1, §9).

use std::collections::BTreeSet;
use std::net::Ipv6Addr;

/// Longest accepted authority, in bytes (maximum length of a DNS name).
const MAX_AUTHORITY_LEN: usize = 253;

/// Label used for anything that is not a host name or IP literal. The parentheses cannot
/// appear in a normalised authority, so no real host collides with it.
pub const INVALID_AUTHORITY: &str = "(invalid)";

/// Label used once the [`AuthorityBudget`] is exhausted; outside the host grammar like
/// [`INVALID_AUTHORITY`].
pub const OTHER_AUTHORITY: &str = "(other)";

/// Normalises a `Host` / `:authority` value: lower case, no trailing dot, `default_port`
/// removed, bracketed IPv6 literals kept. Anything that is not a host name or an IP literal
/// (optionally with a port), or longer than 253 bytes, becomes `(invalid)`.
pub fn normalise_authority(raw: &str, default_port: u16) -> String {
    parse(raw, default_port).unwrap_or_else(|| INVALID_AUTHORITY.to_string())
}

fn parse(raw: &str, default_port: u16) -> Option<String> {
    if raw.is_empty() || raw.len() > MAX_AUTHORITY_LEN {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    let (host, port) = if let Some(rest) = lower.strip_prefix('[') {
        let (addr, after) = rest.split_once(']')?;
        addr.parse::<Ipv6Addr>().ok()?;
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':')?),
        };
        (format!("[{addr}]"), port)
    } else {
        let (host, port) = match lower.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (lower.as_str(), None),
        };
        let host = host.strip_suffix('.').unwrap_or(host);
        if !valid_host(host) {
            return None;
        }
        (host.to_string(), port)
    };
    match port {
        None => Some(host),
        Some(p) => {
            let n = parse_port(p)?;
            if n == default_port {
                Some(host)
            } else {
                Some(format!("{host}:{n}"))
            }
        }
    }
}

fn parse_port(p: &str) -> Option<u16> {
    if p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    p.parse().ok()
}

/// Host name or IPv4 literal: non-empty dot-separated labels of `[a-z0-9_-]`.
fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// Status class label of an HTTP status: `1xx`…`5xx`, `none` outside 100–599.
pub fn status_class(status: u16) -> &'static str {
    match status {
        100..=199 => "1xx",
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "none",
    }
}

/// The authority an exchange is about: the one of the request target (HTTP/1 absolute-form,
/// HTTP/2 `:authority`), else the `Host` header (RFC 9110 §7.2, RFC 9113 §8.3.1).
pub fn target_authority<'a>(uri: Option<&'a str>, host: Option<&'a str>) -> Option<&'a str> {
    uri.or(host)
}

/// Keeps the first `max` distinct authorities; later ones are reported as `(other)`. The sentinels take no slot.
/// One budget per direction and metric.
#[derive(Debug, Clone)]
pub struct AuthorityBudget {
    max: usize,
    seen: BTreeSet<String>,
}

impl AuthorityBudget {
    pub fn new(max: usize) -> Self {
        AuthorityBudget {
            max,
            seen: BTreeSet::new(),
        }
    }

    /// Label to use for `authority`: itself when kept, `(other)` otherwise.
    pub fn label(&mut self, authority: &str) -> String {
        if authority == INVALID_AUTHORITY || authority == OTHER_AUTHORITY {
            return authority.to_string();
        }
        if self.seen.contains(authority) {
            return authority.to_string();
        }
        if self.seen.len() < self.max {
            self.seen.insert(authority.to_string());
            return authority.to_string();
        }
        OTHER_AUTHORITY.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prx_02_normalise_lowercase_dot_and_default_port() {
        assert_eq!(normalise_authority("Example.COM", 80), "example.com");
        assert_eq!(normalise_authority("example.com.", 80), "example.com");
        assert_eq!(normalise_authority("example.com:80", 80), "example.com");
        assert_eq!(normalise_authority("Example.com.:80", 80), "example.com");
        assert_eq!(
            normalise_authority("example.com:8080", 80),
            "example.com:8080"
        );
        assert_eq!(
            normalise_authority("example.com:443", 80),
            "example.com:443"
        );
        assert_eq!(normalise_authority("10.0.0.1:80", 80), "10.0.0.1");
    }

    #[test]
    fn prx_02_normalise_ipv6_literals_kept() {
        assert_eq!(normalise_authority("[::1]", 80), "[::1]");
        assert_eq!(normalise_authority("[::1]:80", 80), "[::1]");
        assert_eq!(normalise_authority("[FE80::1]:8080", 80), "[fe80::1]:8080");
        assert_eq!(normalise_authority("[::1", 80), "(invalid)");
        assert_eq!(normalise_authority("[nope]", 80), "(invalid)");
        assert_eq!(normalise_authority("[::1]x", 80), "(invalid)");
    }

    #[test]
    fn prx_02_normalise_invalid() {
        for raw in [
            "",
            ".",
            "a..b",
            "a b",
            "a/b",
            "a@b",
            "a:",
            "a:x",
            "a:99999",
            "a:80:80",
            "ex\u{e9}.com",
            "a\r\nb",
            ":80",
        ] {
            assert_eq!(normalise_authority(raw, 80), "(invalid)", "{raw:?}");
        }
        assert_eq!(normalise_authority(&"a".repeat(254), 80), "(invalid)");
        let ok = format!("{}.com", "a".repeat(63));
        assert_eq!(normalise_authority(&ok, 80), ok);
        assert_eq!(normalise_authority(&"a".repeat(253), 80), "a".repeat(253));
    }

    #[test]
    fn prx_02_status_class() {
        assert_eq!(status_class(100), "1xx");
        assert_eq!(status_class(199), "1xx");
        assert_eq!(status_class(200), "2xx");
        assert_eq!(status_class(302), "3xx");
        assert_eq!(status_class(404), "4xx");
        assert_eq!(status_class(599), "5xx");
        assert_eq!(status_class(99), "none");
        assert_eq!(status_class(600), "none");
        assert_eq!(status_class(0), "none");
    }

    #[test]
    fn prx_02_budget_keeps_first_n_then_other_deterministically() {
        let inputs = ["b", "a", "b", "c", "d", "a", "c"];
        let run = || {
            let mut b = AuthorityBudget::new(2);
            inputs.iter().map(|i| b.label(i)).collect::<Vec<_>>()
        };
        assert_eq!(run(), ["b", "a", "b", "(other)", "(other)", "a", "(other)"]);
        assert_eq!(run(), run());
    }

    #[test]
    fn prx_02_budget_zero_keeps_nothing() {
        let mut b = AuthorityBudget::new(0);
        assert_eq!(b.label("a"), "(other)");
    }

    #[test]
    fn prx_23_sentinels_are_outside_the_host_grammar() {
        assert_eq!(INVALID_AUTHORITY, "(invalid)");
        assert_eq!(OTHER_AUTHORITY, "(other)");
        assert_eq!(normalise_authority("other", 80), "other");
        assert_eq!(normalise_authority("(invalid)", 80), "(invalid)");
        assert_eq!(normalise_authority("(other)", 80), "(invalid)");
        assert_eq!(normalise_authority("(invalid)", 80), "(invalid)");
    }

    #[test]
    fn prx_23_sentinels_take_no_slot_and_real_other_does() {
        let mut b = AuthorityBudget::new(2);
        assert_eq!(b.label("(invalid)"), "(invalid)");
        assert_eq!(b.label("(other)"), "(other)");
        assert_eq!(b.label("other"), "other");
        assert_eq!(b.label("a"), "a");
        assert_eq!(b.label("c"), "(other)");
        assert_eq!(b.label("(invalid)"), "(invalid)");
        assert_eq!(b.label("other"), "other");
    }

    #[test]
    fn prx_23_target_authority_prefers_the_uri_then_host() {
        assert_eq!(
            target_authority(Some("a.example"), Some("b.example")),
            Some("a.example")
        );
        assert_eq!(target_authority(None, Some("b.example")), Some("b.example"));
        assert_eq!(target_authority(None, None), None);
    }
}
