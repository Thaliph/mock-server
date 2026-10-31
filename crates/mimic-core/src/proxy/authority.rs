//! Bounded metric/log labels for HTTP exchanges: authority normalisation, status class and the
//! authority budget (spec §4.1, §9).

use std::collections::BTreeSet;

/// Label used for anything that is not a host name or IP literal.
pub const INVALID_AUTHORITY: &str = "invalid";

/// Label used once the [`AuthorityBudget`] is exhausted.
pub const OTHER_AUTHORITY: &str = "other";

pub fn normalise_authority(raw: &str, _default_port: u16) -> String {
    raw.to_string()
}

pub fn status_class(_status: u16) -> &'static str {
    "none"
}

#[derive(Debug, Clone)]
pub struct AuthorityBudget {
    _max: usize,
    _seen: BTreeSet<String>,
}

impl AuthorityBudget {
    pub fn new(max: usize) -> Self {
        AuthorityBudget {
            _max: max,
            _seen: BTreeSet::new(),
        }
    }

    pub fn label(&mut self, authority: &str) -> String {
        authority.to_string()
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
        assert_eq!(normalise_authority("[::1", 80), "invalid");
        assert_eq!(normalise_authority("[nope]", 80), "invalid");
        assert_eq!(normalise_authority("[::1]x", 80), "invalid");
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
            assert_eq!(normalise_authority(raw, 80), "invalid", "{raw:?}");
        }
        assert_eq!(normalise_authority(&"a".repeat(254), 80), "invalid");
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
        assert_eq!(run(), ["b", "a", "b", "other", "other", "a", "other"]);
        assert_eq!(run(), run());
    }

    #[test]
    fn prx_02_budget_zero_keeps_nothing() {
        let mut b = AuthorityBudget::new(0);
        assert_eq!(b.label("a"), "other");
    }
}
