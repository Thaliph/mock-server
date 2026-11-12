//! Pure logic of mimic: no tokio, no I/O, no clock. Everything testable without a network
//! lives here; the binaries are adapters around it.

pub mod compact;
pub mod discovery;
pub mod fd;
pub mod grpc;
pub mod identity;
pub mod inject;
pub mod load;
pub mod mock;
pub mod proxy;
pub mod redirect;
pub mod trust;

use std::fmt;
use std::str::FromStr;

/// Operating mode of a deployment's sidecars (spec §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Mode {
    /// Proxy only: forward inbound and outbound traffic, record nothing, pass outbound TLS
    /// through (M0).
    None,
    /// Forward and record inbound and outbound traffic.
    #[default]
    Discovery,
    /// Answer outbound traffic from recordings; inbound is still forwarded and recorded.
    Mock,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::None => "none",
            Mode::Discovery => "discovery",
            Mode::Mock => "mock",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Mode, String> {
        match s {
            "none" => Ok(Mode::None),
            "discovery" => Ok(Mode::Discovery),
            "mock" => Ok(Mode::Mock),
            _ => Err(format!(
                "unknown mode {s:?}, expected none, discovery or mock"
            )),
        }
    }
}

/// Direction of an exchange, seen from the injected pod.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Direction {
    /// Ingress: a client calls the app.
    Inbound,
    /// Egress: the app calls a dependency.
    Outbound,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Inbound => "inbound",
            Direction::Outbound => "outbound",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_round_trips_through_its_name() {
        for mode in [Mode::None, Mode::Discovery, Mode::Mock] {
            assert_eq!(mode.to_string().parse::<Mode>(), Ok(mode));
        }
        assert_eq!(Mode::default(), Mode::Discovery);
        assert!("record".parse::<Mode>().is_err());
    }
}
