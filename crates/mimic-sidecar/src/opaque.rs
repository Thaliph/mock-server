//! The opaque tunnel (spec §4.1): bytes are relayed unchanged in both directions, with no idle
//! timeout; a close or half-close on one side reaches the other.

use std::io;

use tokio::io::{AsyncWriteExt, copy_bidirectional};
use tokio::net::TcpStream;

/// Writes the bytes read during detection to their side, then relays until both sides are done.
pub async fn tunnel(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    to_upstream: Vec<u8>,
    to_client: Vec<u8>,
) -> io::Result<()> {
    let (up, down) = tokio::join!(
        upstream.write_all(&to_upstream),
        client.write_all(&to_client)
    );
    up?;
    down?;
    copy_bidirectional(client, upstream).await.map(|_| ())
}
