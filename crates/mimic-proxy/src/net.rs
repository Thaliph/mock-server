//! Destination d'origine d'une connexion redirigée par iptables (`SO_ORIGINAL_DST`).

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::time::Duration;

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

/// Délai d'une sonde de connexion vers l'application.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Adresse de l'application pour le trafic entrant (spec §3.1) : la destination d'origine
/// (IP du pod), sinon `127.0.0.1` sur le même port. Le choix est mémorisé par port.
pub struct InboundTargets {
    upstream: Option<SocketAddr>,
    cache: Mutex<BTreeMap<u16, SocketAddr>>,
}

impl InboundTargets {
    pub fn new(upstream: Option<SocketAddr>) -> Self {
        Self {
            upstream,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    pub async fn resolve(&self, dst: SocketAddr) -> SocketAddr {
        if let Some(u) = self.upstream {
            return u;
        }
        if let Some(t) = self.cached(dst.port()) {
            return t;
        }
        let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), dst.port());
        let mut candidates = vec![dst];
        if dst != loopback {
            candidates.push(loopback);
        }
        for c in candidates {
            if let Ok(Ok(_)) = tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect(c)).await {
                self.lock().insert(dst.port(), c);
                return c;
            }
        }
        dst
    }

    /// À appeler après un échec de connexion : le prochain relais refait le choix.
    pub fn forget(&self, port: u16) {
        self.lock().remove(&port);
    }

    pub fn cached(&self, port: u16) -> Option<SocketAddr> {
        self.lock().get(&port).copied()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u16, SocketAddr>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
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

    /// Écoute sur `ip` ; renvoie le port et la tâche qui accepte.
    async fn listen(ip: &str) -> (tokio::net::TcpListener, u16) {
        let l = tokio::net::TcpListener::bind((ip, 0)).await.unwrap();
        let port = l.local_addr().unwrap().port();
        (l, port)
    }

    #[tokio::test]
    async fn prx_09_inbound_relay_targets_the_pod_ip_then_loopback() {
        // 127.0.0.2 joue l'IP du pod : sur Linux, tout 127.0.0.0/8 est local.
        let targets = InboundTargets::new(None);

        // Application liée à l'IP du pod seulement.
        let (_pod_only, port) = listen("127.0.0.2").await;
        let dst: SocketAddr = format!("127.0.0.2:{port}").parse().unwrap();
        assert_eq!(targets.resolve(dst).await, dst);

        // Application liée à la boucle locale seulement : repli, mémorisé.
        let (loopback_only, port) = listen("127.0.0.1").await;
        let dst: SocketAddr = format!("127.0.0.2:{port}").parse().unwrap();
        let local: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        assert_eq!(targets.resolve(dst).await, local);
        drop(loopback_only);
        assert_eq!(targets.resolve(dst).await, local, "choix mémorisé par port");
        // Après un échec, le choix est réévalué : plus rien n'écoute, l'IP du pod est retenue.
        targets.forget(port);
        assert_eq!(targets.resolve(dst).await, dst);
        assert!(
            targets.cached(port).is_none(),
            "rien n'est mémorisé sans écoute"
        );

        // `inbound.upstream` impose l'adresse.
        let forced: SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert_eq!(InboundTargets::new(Some(forced)).resolve(dst).await, forced);
    }
}
