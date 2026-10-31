//! PRX-12: an `accept` that fails (no descriptor left) is counted and retried; the connection
//! is served once descriptors are available again. Own test binary: it lowers the process-wide
//! open-file limit.

mod common;

use std::fs::File;
use std::net::TcpStream as StdStream;
use std::time::Duration;

use common::{counted, echo_server, fixture_with};
use mimic_sidecar::listener::Direction;
use nix::sys::resource::{Resource, getrlimit, setrlimit};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn prx_12_accept_errors_are_counted_and_the_connection_is_served_later() {
    let echo = echo_server().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    // Completed by the kernel, waiting in the backlog until the listener accepts.
    let client = StdStream::connect(addr).unwrap();
    client.set_nonblocking(true).unwrap();

    let (soft, hard) = getrlimit(Resource::RLIMIT_NOFILE).unwrap();
    let open = std::fs::read_dir("/proc/self/fd").unwrap().count() as u64;
    setrlimit(Resource::RLIMIT_NOFILE, open + 4, hard).unwrap();
    let mut filler = Vec::new();
    while let Ok(f) = File::open("/dev/null") {
        filler.push(f);
    }

    let f = fixture_with(listener, Direction::Inbound, move |_| Some(echo)).await;
    counted(
        &f.metrics,
        r#"mimic_accept_errors_total{direction="inbound"} "#,
    )
    .await;

    drop(filler);
    setrlimit(Resource::RLIMIT_NOFILE, soft, hard).unwrap();
    let mut c = tokio::net::TcpStream::from_std(client).unwrap();
    c.write_all(b"ping\n").await.unwrap();
    let mut buf = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(5), c.read_exact(&mut buf))
        .await
        .expect("served after the descriptors are back")
        .unwrap();
    assert_eq!(&buf, b"ping\n");
}
