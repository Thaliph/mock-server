//! Sonde `exec` du sidecar (spec §3.3.4) : `mimic-proxy --probe <chemin>` interroge l'API
//! admin depuis le conteneur, là où une sonde HTTP du kubelet serait refusée par un mesh en
//! mTLS strict.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;

use crate::config::Config;

/// Délai de chaque étape (connexion, écriture, lecture) : la sonde répond bien avant le
/// `timeoutSeconds` du kubelet (1 s par défaut).
const STEP_TIMEOUT: Duration = Duration::from_millis(500);

/// Adresse interrogée : celle de l'API admin, sur la boucle locale si elle écoute partout.
pub fn probe_addr(config: &Config) -> SocketAddr {
    let mut addr = config.admin.listen;
    match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => addr.set_ip(Ipv4Addr::LOCALHOST.into()),
        IpAddr::V6(ip) if ip.is_unspecified() => addr.set_ip(Ipv6Addr::LOCALHOST.into()),
        _ => {}
    }
    addr
}

/// `GET <chemin>` sur l'API admin : `Ok` pour une réponse 2xx.
pub fn probe(config: &Config, path: &str) -> Result<(), String> {
    let addr = probe_addr(config);
    let mut stream = TcpStream::connect_timeout(&addr, STEP_TIMEOUT)
        .map_err(|e| format!("connexion à {addr} : {e}"))?;
    stream
        .set_read_timeout(Some(STEP_TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(STEP_TIMEOUT)))
        .map_err(|e| e.to_string())?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| format!("requête vers {addr} : {e}"))?;
    // La ligne de statut suffit : `HTTP/1.1 200 OK`.
    let mut head = [0u8; 64];
    let mut read = 0;
    while read < 12 {
        match stream.read(&mut head[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(e) => return Err(format!("réponse de {addr} : {e}")),
        }
    }
    let line = String::from_utf8_lossy(&head[..read]);
    let status = line.split_whitespace().nth(1).unwrap_or("");
    if status.starts_with('2') && status.len() == 3 {
        Ok(())
    } else {
        Err(format!(
            "{path} : statut {}",
            if status.is_empty() { "absent" } else { status }
        ))
    }
}
