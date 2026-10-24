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

/// Adresse de l'application pour le trafic entrant (spec §3.1) : la destination d'origine
/// (IP du pod et port), jamais `127.0.0.1` à sa place, pour qu'un service lié à la boucle
/// locale reste injoignable depuis les autres pods ; `inbound.upstream` l'impose.
pub struct InboundTargets {
    upstream: Option<SocketAddr>,
}

impl InboundTargets {
    pub fn new(upstream: Option<SocketAddr>) -> Self {
        Self { upstream }
    }

    pub fn resolve(&self, dst: SocketAddr) -> SocketAddr {
        self.upstream.unwrap_or(dst)
    }
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

/// Ce que montrent les premiers octets d'une connexion redirigée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detected {
    Protocol(Protocol),
    /// Rien reçu dans le délai : serveur qui parle en premier, ou connexion ouverte d'avance
    /// par un pool et pas encore utilisée (spec §3.1).
    Silent,
}

/// Lit sans consommer les premiers octets de la connexion pour en déduire le protocole.
pub async fn detect(stream: &TcpStream) -> Detected {
    let deadline = tokio::time::Instant::now() + SNIFF_TIMEOUT;
    let mut buf = [0u8; 16];
    let mut seen = false;
    loop {
        match tokio::time::timeout_at(deadline, stream.peek(&mut buf)).await {
            Err(_) if !seen => return Detected::Silent,
            Err(_) | Ok(Err(_)) | Ok(Ok(0)) => return Detected::Protocol(Protocol::Other),
            Ok(Ok(n)) => {
                seen = true;
                if let Some(p) = classify(&buf[..n]) {
                    return Detected::Protocol(p);
                }
                // Début ambigu (« GE ») : on attend la suite sans boucler à vide.
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                if tokio::time::Instant::now() >= deadline {
                    return Detected::Protocol(Protocol::Other);
                }
            }
        }
    }
}

/// Comme [`detect`], une connexion muette comptant comme un autre protocole.
pub async fn sniff(stream: &TcpStream) -> Protocol {
    match detect(stream).await {
        Detected::Protocol(p) => p,
        Detected::Silent => Protocol::Other,
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

    /// Écoute sur `ip` ; renvoie le port et la tâche qui accepte.
    async fn listen(ip: &str) -> (tokio::net::TcpListener, u16) {
        let l = tokio::net::TcpListener::bind((ip, 0)).await.unwrap();
        let port = l.local_addr().unwrap().port();
        (l, port)
    }

    #[tokio::test]
    async fn prx_09_inbound_relay_targets_the_original_destination_only() {
        // 127.0.0.2 joue l'IP du pod : sur Linux, tout 127.0.0.0/8 est local.
        let targets = InboundTargets::new(None);

        // Application liée à l'IP du pod seulement.
        let (_pod_only, port) = listen("127.0.0.2").await;
        let dst: SocketAddr = format!("127.0.0.2:{port}").parse().unwrap();
        assert_eq!(targets.resolve(dst), dst);

        // Service lié à la boucle locale seulement : jamais exposé aux autres pods, aucun
        // repli sur 127.0.0.1 (le relais vers l'IP du pod échouera : 502).
        let (_loopback_only, port) = listen("127.0.0.1").await;
        let dst: SocketAddr = format!("127.0.0.2:{port}").parse().unwrap();
        assert_eq!(targets.resolve(dst), dst);

        // `inbound.upstream` impose l'adresse.
        let forced: SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert_eq!(InboundTargets::new(Some(forced)).resolve(dst), forced);
    }
}
