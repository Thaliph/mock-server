//! The `x-mimic-error` reasons of spec §9: the one enumeration every writer of a reason takes.

use super::retry::Outcome;

/// A reason of the §9 `x-mimic-error` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reason {
    Overloaded,
    QueueTimeout,
    ConnectFailed,
    NoEndpoint,
    NoRoute,
    NoBackend,
    InvalidBackend,
    Timeout,
    BreakerOpen,
    FaultInjected,
    InvalidResponse,
    InvalidRequest,
    Unsupported,
}

impl Reason {
    /// The §9 table in its row order.
    pub const ALL: [Reason; 13] = [Reason::Overloaded; 13];

    pub const fn as_str(self) -> &'static str {
        todo!()
    }

    pub const fn status(self) -> Option<u16> {
        todo!()
    }

    pub fn parse(_value: &[u8]) -> Option<Reason> {
        todo!()
    }

    pub const fn trusted_inbound() -> [Reason; 3] {
        todo!()
    }

    pub const fn of_outcome(_outcome: Outcome) -> Option<Reason> {
        todo!()
    }
}
