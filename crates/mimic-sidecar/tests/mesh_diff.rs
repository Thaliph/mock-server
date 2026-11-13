//! MSH-67: the differential corpus. The same requests go through the byte relay (a destination
//! outside the view) and through the mesh (a pod of the view) to a raw server that records the
//! bytes; the relay's equal the request, the mesh's differ exactly as spec §4.1 says.

mod common;

use std::net::SocketAddr;
use std::time::Duration;

use common::mesh_pods::{RawPod, cluster_ip, view_on};
use common::{capture_logs, fixture_mesh};
use mimic_sidecar::mesh::{MeshConfig, MeshState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// What the mesh path sends to the pod for a corpus entry.
enum Mesh {
    /// The bytes of the request.
    Same,
    /// These bytes instead.
    Changed(&'static [u8]),
    /// Nothing; the client is answered `400` `invalid-request`.
    Refused,
}

struct Entry {
    name: &'static str,
    request: &'static [u8],
    mesh: Mesh,
}

const CORPUS: &[Entry] = &[
    Entry {
        name: "a plain GET",
        request: b"GET /a?x=1 HTTP/1.1\r\nHost: cart.shop.svc\r\nAccept: */*\r\n\r\n",
        mesh: Mesh::Same,
    },
    Entry {
        name: "header case and repeated fields of one name",
        request: b"GET /b HTTP/1.1\r\nhOsT: cart.shop.svc\r\nX-Multi: one\r\nx-multi: two\r\nX-MULTI: three\r\n\r\n",
        mesh: Mesh::Same,
    },
    Entry {
        name: "a POST with Content-Length",
        request: b"POST /c HTTP/1.1\r\nHost: cart.shop.svc\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello",
        mesh: Mesh::Same,
    },
    Entry {
        name: "a chunked body without extensions",
        request: b"POST /d HTTP/1.1\r\nHost: cart.shop.svc\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        mesh: Mesh::Same,
    },
    Entry {
        name: "hop-by-hop fields",
        request: b"GET /e HTTP/1.1\r\nHost: cart.shop.svc\r\nConnection: keep-alive, X-Hop\r\nKeep-Alive: timeout=5\r\nX-Hop: 1\r\nX-Kept: 1\r\n\r\n",
        mesh: Mesh::Changed(b"GET /e HTTP/1.1\r\nhost: cart.shop.svc\r\nx-kept: 1\r\n\r\n"),
    },
    Entry {
        name: "chunk extensions",
        request: b"POST /f HTTP/1.1\r\nHost: cart.shop.svc\r\nTransfer-Encoding: chunked\r\n\r\n5;ext=1\r\nhello\r\n0\r\n\r\n",
        mesh: Mesh::Changed(b"POST /f HTTP/1.1\r\nhost: cart.shop.svc\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"),
    },
    Entry {
        name: "Content-Length with Transfer-Encoding",
        request: b"POST /g HTTP/1.1\r\nHost: cart.shop.svc\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        mesh: Mesh::Changed(b"POST /g HTTP/1.1\r\nhost: cart.shop.svc\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n"),
    },
    Entry {
        name: "a header name that is not a token",
        request: b"GET /h HTTP/1.1\r\nHost: cart.shop.svc\r\nBad Header: 1\r\n\r\n",
        mesh: Mesh::Refused,
    },
    Entry {
        name: "two different Content-Length",
        request: b"POST /i HTTP/1.1\r\nHost: cart.shop.svc\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nab",
        mesh: Mesh::Refused,
    },
];

fn state_for(cluster: SocketAddr, pods: &[SocketAddr]) -> MeshState {
    let state = MeshState::new(&MeshConfig {
        enabled: true,
        seed: Some(2),
        ..MeshConfig::default()
    });
    state.hold(view_on(cluster, pods, 0, true), "\"1-a\"");
    state
}

fn ok(_head: &[u8]) -> Vec<u8> {
    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec()
}

/// Sends `request` on a new connection and returns what the client got back until the server
/// answered or closed.
async fn send(addr: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut c = TcpStream::connect(addr).await.unwrap();
    c.write_all(request).await.unwrap();
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    let ended = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match c.read(&mut buf).await {
                Ok(n) if n > 0 => {
                    got.extend_from_slice(&buf[..n]);
                    if got.windows(4).any(|w| w == b"\r\n\r\n") {
                        // The head is whole; a body of 2 bytes or none follows.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        return;
                    }
                }
                _ => return,
            }
        }
    })
    .await;
    assert!(ended.is_ok(), "answered in time");
    got
}

#[tokio::test]
async fn msh_67_the_mesh_path_differs_from_the_byte_relay_exactly_as_the_spec_says() {
    let _logs = capture_logs();
    for entry in CORPUS {
        // The byte relay: the destination is outside the view.
        let destination = RawPod::start(ok).await;
        let other = RawPod::start(ok).await;
        let state = state_for(cluster_ip(), &[other.addr]);
        let relay = fixture_mesh(destination.addr, &state, None).await;
        send(relay.addr, entry.request).await;
        assert_eq!(
            destination.received(),
            entry.request,
            "{}: the byte relay forwards the bytes as sent",
            entry.name
        );

        // The mesh: the destination is the ClusterIP of the Service whose pod records.
        let pod = RawPod::start(ok).await;
        let state = state_for(cluster_ip(), &[pod.addr]);
        let mesh = fixture_mesh(cluster_ip(), &state, None).await;
        let answer = send(mesh.addr, entry.request).await;
        let sent = pod.received();
        match &entry.mesh {
            Mesh::Same => assert!(
                sent == entry.request,
                "{}: the mesh sends the same bytes, got {:?}",
                entry.name,
                String::from_utf8_lossy(&sent)
            ),
            Mesh::Changed(expected) => {
                // `hyper` writes header names it keeps in their own case; compare without case.
                assert_eq!(
                    String::from_utf8_lossy(&sent).to_lowercase(),
                    String::from_utf8_lossy(expected).to_lowercase(),
                    "{}",
                    entry.name
                );
            }
            Mesh::Refused => {
                assert!(sent.is_empty(), "{}: nothing reached the pod", entry.name);
                let text = String::from_utf8_lossy(&answer).to_lowercase();
                assert!(
                    text.starts_with("http/1.1 400")
                        && text.contains("x-mimic-error: invalid-request"),
                    "{}: {text}",
                    entry.name
                );
            }
        }
    }
}
