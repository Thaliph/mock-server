//! Pure logic of mimic: no tokio, no I/O, no clock. Everything testable without a network
//! lives here; the binaries are adapters around it.

pub mod fd;
pub mod inject;
pub mod proxy;
pub mod redirect;

use std::fmt;
use std::str::FromStr;

/// Operating mode of a deployment's sidecars (spec §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Mode {
    /// Forward and record inbound and outbound traffic.
    #[default]
    Discovery,
    /// Answer outbound traffic from recordings; inbound is still forwarded and recorded.
    Mock,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
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
            "discovery" => Ok(Mode::Discovery),
            "mock" => Ok(Mode::Mock),
            _ => Err(format!("unknown mode {s:?}, expected discovery or mock")),
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
        for mode in [Mode::Discovery, Mode::Mock] {
            assert_eq!(mode.to_string().parse::<Mode>(), Ok(mode));
        }
        assert_eq!(Mode::default(), Mode::Discovery);
        assert!("record".parse::<Mode>().is_err());
    }
}
