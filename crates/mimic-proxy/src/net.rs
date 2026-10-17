//! Destination d'origine d'une connexion redirigée par iptables (`SO_ORIGINAL_DST`).

use std::net::SocketAddr;

use tokio::net::TcpStream;

/// Destination avant redirection, ou `None` si la connexion n'a pas été redirigée.
pub fn original_dst(stream: &TcpStream) -> Option<SocketAddr> {
    let local = stream.local_addr().ok()?;
    lookup(stream, local).filter(|dst| *dst != local)
}

#[cfg(target_os = "linux")]
fn lookup(stream: &TcpStream, local: SocketAddr) -> Option<SocketAddr> {
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

    use nix::sys::socket::getsockopt;
    use nix::sys::socket::sockopt::{Ip6tOriginalDst, OriginalDst};

    if local.is_ipv4() {
        let sin = getsockopt(stream, OriginalDst).ok()?;
        Some(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)),
            u16::from_be(sin.sin_port),
        )))
    } else {
        let sin6 = getsockopt(stream, Ip6tOriginalDst).ok()?;
        Some(SocketAddr::V6(SocketAddrV6::new(
            Ipv6Addr::from(sin6.sin6_addr.s6_addr),
            u16::from_be(sin6.sin6_port),
            sin6.sin6_flowinfo,
            sin6.sin6_scope_id,
        )))
    }
}

#[cfg(not(target_os = "linux"))]
fn lookup(_stream: &TcpStream, _local: SocketAddr) -> Option<SocketAddr> {
    None
}

/// Protocole détecté sur une connexion redirigée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Http,
    Tls,
    /// Autre protocole, ou serveur qui parle en premier.
    Other,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::Http => "http",
            Protocol::Tls => "tls",
            Protocol::Other => "tcp",
        }
    }
}

const HTTP_PREFIXES: &[&[u8]] = &[
    b"GET ",
    b"POST ",
    b"PUT ",
    b"DELETE ",
    b"HEAD ",
    b"OPTIONS ",
    b"PATCH ",
    b"TRACE ",
    b"CONNECT ",
    b"PRI * HTTP/2",
];

/// Classe les premiers octets ; `None` s'il en faut davantage pour conclure.
pub fn classify(head: &[u8]) -> Option<Protocol> {
    if head.is_empty() {
        return None;
    }
    if head[0] == 0x16 {
        return Some(Protocol::Tls);
    }
    let mut undecided = false;
    for p in HTTP_PREFIXES {
        if head.len() >= p.len() {
            if head.starts_with(p) {
                return Some(Protocol::Http);
            }
        } else if p.starts_with(head) {
            undecided = true;
        }
    }
    if undecided {
        None
    } else {
        Some(Protocol::Other)
    }
}

/// Délai au-delà duquel une connexion muette est relayée telle quelle.
pub const SNIFF_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// Lit sans consommer les premiers octets de la connexion pour en déduire le protocole.
pub async fn sniff(stream: &TcpStream) -> Protocol {
    let deadline = tokio::time::Instant::now() + SNIFF_TIMEOUT;
    let mut buf = [0u8; 16];
    loop {
        match tokio::time::timeout_at(deadline, stream.peek(&mut buf)).await {
            Err(_) | Ok(Err(_)) | Ok(Ok(0)) => return Protocol::Other,
            Ok(Ok(n)) => {
                if let Some(p) = classify(&buf[..n]) {
                    return p;
                }
                // Début ambigu (« GE ») : on attend la suite sans boucler à vide.
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                if tokio::time::Instant::now() >= deadline {
                    return Protocol::Other;
                }
            }
        }
    }
}

/// SNI du ClientHello en attente sur la connexion, lu sans le consommer.
pub async fn peek_sni(stream: &TcpStream) -> Option<String> {
    let deadline = tokio::time::Instant::now() + SNIFF_TIMEOUT;
    let mut buf = vec![0u8; 16 * 1024 + 5];
    loop {
        let n = tokio::time::timeout_at(deadline, stream.peek(&mut buf))
            .await
            .ok()?
            .ok()?;
        if n == 0 {
            return None;
        }
        if let Some(name) = crate::tls::parse_sni(&buf[..n]) {
            return Some(name);
        }
        // Enregistrement complet sans SNI : inutile d'attendre davantage.
        if n >= 5 && n >= 5 + u16::from_be_bytes([buf[3], buf[4]]) as usize {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert_eq!(classify(b"GET / HTTP/1.1"), Some(Protocol::Http));
        assert_eq!(classify(b"PRI * HTTP/2.0"), Some(Protocol::Http));
        assert_eq!(classify(&[0x16, 0x03, 0x01]), Some(Protocol::Tls));
        assert_eq!(classify(b"GE"), None);
        assert_eq!(
            classify(b"\x00\x00\x00\x08\x04\xd2\x16\x2f"),
            Some(Protocol::Other)
        );
        assert_eq!(classify(b"SSH-2.0"), Some(Protocol::Other));
    }
}
