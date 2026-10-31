//! Protocol detection for the HTTP-aware proxy (spec §4.1). Pure functions of the bytes read
//! so far.

pub mod authority;

/// Most bytes detection looks at before it gives up.
pub const DETECT_LIMIT: usize = 16 * 1024;

/// The HTTP/2 client connection preface (RFC 9113 §3.4).
pub const H2_PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// Result of [`detect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    /// Not enough bytes to decide.
    NeedMore,
    /// A TLS ClientHello; `sni` is the server name when the hello carries one.
    Tls { sni: Option<String> },
    Http1,
    H2,
    Opaque,
}

/// Classify the first bytes sent by a client.
pub fn detect(_buf: &[u8]) -> Detection {
    Detection::NeedMore
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn tls_sni(name: &str) -> Vec<u8> {
        let host = name.as_bytes();
        let mut sni = Vec::new();
        sni.extend((host.len() as u16 + 3).to_be_bytes());
        sni.push(0);
        sni.extend((host.len() as u16).to_be_bytes());
        sni.extend(host);
        let mut ext = Vec::new();
        ext.extend(0u16.to_be_bytes());
        ext.extend((sni.len() as u16).to_be_bytes());
        ext.extend(sni);
        ext
    }

    fn client_hello(extensions: &[u8]) -> Vec<u8> {
        let mut body = vec![3, 3];
        body.extend([7u8; 32]);
        body.push(0);
        body.extend(2u16.to_be_bytes());
        body.extend([0x13, 0x01]);
        body.extend([1, 0]);
        body.extend((extensions.len() as u16).to_be_bytes());
        body.extend(extensions);
        let mut hs = vec![1];
        hs.extend(&(body.len() as u32).to_be_bytes()[1..]);
        hs.extend(body);
        hs
    }

    fn record(fragment: &[u8]) -> Vec<u8> {
        let mut r = vec![0x16, 3, 1];
        r.extend((fragment.len() as u16).to_be_bytes());
        r.extend(fragment);
        r
    }

    fn sni_of(name: &str) -> Detection {
        Detection::Tls {
            sni: Some(name.to_string()),
        }
    }

    #[test]
    fn prx_01_empty_needs_more() {
        assert_eq!(detect(b""), Detection::NeedMore);
    }

    #[test]
    fn prx_01_client_hello_with_sni() {
        let buf = record(&client_hello(&tls_sni("example.com")));
        assert_eq!(detect(&buf), sni_of("example.com"));
    }

    #[test]
    fn prx_01_client_hello_without_sni() {
        let buf = record(&client_hello(&[0, 10, 0, 2, 0, 0]));
        assert_eq!(detect(&buf), Detection::Tls { sni: None });
        let buf = record(&client_hello(&[]));
        assert_eq!(detect(&buf), Detection::Tls { sni: None });
    }

    #[test]
    fn prx_01_split_client_hello_needs_more_until_complete() {
        let buf = record(&client_hello(&tls_sni("split.example.org")));
        for n in 0..buf.len() {
            assert_eq!(detect(&buf[..n]), Detection::NeedMore, "prefix {n}");
        }
        assert_eq!(detect(&buf), sni_of("split.example.org"));
    }

    #[test]
    fn prx_01_client_hello_over_several_records() {
        let hello = client_hello(&tls_sni("multi.example.org"));
        let (a, rest) = hello.split_at(10);
        let (b, c) = rest.split_at(20);
        let mut buf = record(a);
        buf.extend(record(b));
        assert_eq!(detect(&buf), Detection::NeedMore);
        buf.extend(record(c));
        assert_eq!(detect(&buf[..buf.len() - 1]), Detection::NeedMore);
        assert_eq!(detect(&buf), sni_of("multi.example.org"));
    }

    #[test]
    fn prx_01_tls_garbage_after_record_header() {
        // Not a ClientHello handshake message.
        assert_eq!(detect(&record(&[2, 0, 0, 1, 0])), Detection::Opaque);
        // Truncated hello inside a complete record.
        assert_eq!(
            detect(&record(&[1, 0, 0, 4, 3, 3, 0, 0])),
            Detection::Tls { sni: None }
        );
        // Not TLS: bad version.
        assert_eq!(detect(&[0x16, 9, 9, 0, 5, 1]), Detection::Opaque);
    }

    #[test]
    fn prx_01_http1_request_lines() {
        for line in [
            &b"GET / HTTP/1.1\r\n"[..],
            b"POST /a?b=c HTTP/1.0\r\nHost: x\r\n",
            b"OPTIONS * HTTP/1.1\r\n",
            b"CONNECT example.com:443 HTTP/1.1\r\n",
            b"PURGE /cache HTTP/1.1\r\n",
            b"M-SEARCH * HTTP/1.1\r\n",
            b"PUT /x HTTP/1.1\r\n",
        ] {
            assert_eq!(detect(line), Detection::Http1, "{line:?}");
        }
    }

    #[test]
    fn prx_01_http1_partial_needs_more() {
        let line = b"DELETE /some/path HTTP/1.1\r\n";
        for n in 1..line.len() {
            assert_eq!(detect(&line[..n]), Detection::NeedMore, "prefix {n}");
        }
        assert_eq!(detect(line), Detection::Http1);
    }

    #[test]
    fn prx_01_h2_preface() {
        assert_eq!(detect(H2_PREFACE), Detection::H2);
        let mut more = H2_PREFACE.to_vec();
        more.extend([0, 0, 0, 4, 0, 0, 0, 0, 0, 0]);
        assert_eq!(detect(&more), Detection::H2);
        for n in 1..24 {
            assert_eq!(detect(&H2_PREFACE[..n]), Detection::NeedMore, "prefix {n}");
        }
    }

    #[test]
    fn prx_01_near_preface_is_not_h2() {
        assert_eq!(detect(b"PRI * HTTP/2.0\r\n\r\nSX"), Detection::Opaque);
    }

    #[test]
    fn prx_01_opaque_at_once() {
        for first in [
            &b"\x00"[..],
            b"\x05\x01",
            b"\xff",
            b" GET",
            b"\r\n",
            b"{\"a\":1}",
            b"\x17\x03\x03",
        ] {
            assert_eq!(detect(first), Detection::Opaque, "{first:?}");
        }
        assert_eq!(detect(b"SSH-2.0-OpenSSH_9.6\r\n"), Detection::Opaque);
        assert_eq!(detect(b"GET  / HTTP/1.1\r\n"), Detection::Opaque);
        assert_eq!(detect(b"GET / HTTP/2.0\r\n"), Detection::Opaque);
        assert_eq!(detect(b"GET / FTP/1.1\r\n"), Detection::Opaque);
    }

    #[test]
    fn prx_01_limit_without_decision() {
        let mut buf = b"GET /".to_vec();
        buf.resize(DETECT_LIMIT - 1, b'a');
        assert_eq!(detect(&buf), Detection::NeedMore);
        buf.push(b'a');
        assert_eq!(detect(&buf), Detection::Opaque);

        let mut tls = vec![0x16, 3, 1, 0x3f, 0xff, 1, 0, 0x3f, 0xfb];
        tls.resize(DETECT_LIMIT - 1, 0);
        assert_eq!(detect(&tls), Detection::NeedMore);
        tls.push(0);
        assert_eq!(detect(&tls), Detection::Tls { sni: None });
    }

    proptest! {
        #[test]
        fn prx_01_never_panics(buf in proptest::collection::vec(any::<u8>(), 0..300)) {
            let _ = detect(&buf);
        }

        #[test]
        fn prx_01_never_panics_tls_shaped(
            tail in proptest::collection::vec(any::<u8>(), 0..300),
            len in 0u16..400,
        ) {
            let mut buf = vec![0x16, 3, 1];
            buf.extend(len.to_be_bytes());
            buf.extend(tail);
            let _ = detect(&buf);
        }

        #[test]
        fn prx_01_prefixes_of_a_hello_never_decide_early(cut in 0usize..1000) {
            let buf = record(&client_hello(&tls_sni("prop.example.org")));
            let cut = cut.min(buf.len() - 1);
            prop_assert_eq!(detect(&buf[..cut]), Detection::NeedMore);
        }
    }
}
