//! The listeners of the gateway role.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mimic_runtime::admin::Readiness;
use tokio::net::TcpListener;
use tokio::time::Instant;

use super::config::GatewayConfig;

#[derive(Debug)]
pub struct BindError {
    pub port: u16,
    pub addr: SocketAddr,
    pub source: io::Error,
}

pub struct Gateway {
    _config: GatewayConfig,
}

impl Gateway {
    pub fn new(config: GatewayConfig) -> Arc<Gateway> {
        Arc::new(Gateway { _config: config })
    }

    pub fn bind(&self, _ipv6: bool) -> Result<Vec<(TcpListener, SocketAddr)>, BindError> {
        Ok(Vec::new())
    }

    pub fn readiness(self: &Arc<Self>) -> Readiness {
        Readiness::always()
    }

    pub fn spawn(self: &Arc<Self>, _listeners: Vec<(TcpListener, SocketAddr)>) {}

    pub fn shutdown(&self) {}

    pub async fn drain(&self, _grace: Duration) {}
}

pub(crate) enum Phase {
    Head(Instant),
    Busy(usize),
    Idle(Instant),
}

pub(crate) fn deadline(_phase: &Phase, _header: Duration, _idle: Duration) -> Option<Instant> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gw_19_deadlines_follow_the_connection_phase() {
        let t = Instant::now();
        let (header, idle) = (Duration::from_secs(1), Duration::from_secs(2));
        assert_eq!(deadline(&Phase::Head(t), header, idle), Some(t + header));
        assert_eq!(deadline(&Phase::Idle(t), header, idle), Some(t + idle));
        assert_eq!(deadline(&Phase::Busy(2), header, idle), None);
    }
}
