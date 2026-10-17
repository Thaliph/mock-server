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
