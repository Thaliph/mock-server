//! The HTTP/1 byte relay (spec §4.1).

use std::future::Future;
use std::io;

use mimic_core::proxy::http1::Event;
use tokio::net::TcpStream;

use crate::access::Outcome;

/// How a relay ended.
pub struct Relayed {
    pub outcome: Outcome,
    pub error: Option<io::Error>,
    pub to_upstream: u64,
    pub to_client: u64,
    pub unanswered: usize,
}

pub async fn relay(
    _client: &mut TcpStream,
    _upstream: &mut TcpStream,
    _first: Vec<u8>,
    _on_event: impl FnMut(Event) + Send,
    _abort_signal: impl Future<Output = ()> + Send,
) -> Relayed {
    todo!()
}
