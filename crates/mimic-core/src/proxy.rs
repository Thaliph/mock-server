//! Protocol detection for the HTTP-aware proxy (spec §4.1). Pure functions of the bytes read
//! so far.

pub mod authority;
pub mod http1;

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
    Tls {
        sni: Option<String>,
    },
    Http1,
    H2,
    Opaque,
}

/// Classify the first bytes sent by a client.
///
/// `buf` holds everything read so far; the caller keeps reading while the answer is
/// [`Detection::NeedMore`]. Once `buf` reaches [`DETECT_LIMIT`] the answer is final.
pub fn detect(buf: &[u8]) -> Detection {
    let Some(&first) = buf.first() else {
        return Detection::NeedMore;
    };
    let undecided = if first == 0x16 {
        match detect_tls(buf) {
            Some(d) => return d,
            None => Detection::Tls { sni: None },
        }
    } else {
        match detect_http(buf) {
            Some(d) => return d,
            None => Detection::Opaque,
        }
    };
    if buf.len() >= DETECT_LIMIT {
        undecided
    } else {
        Detection::NeedMore
    }
}

/// HTTP/1.x request line or the HTTP/2 preface. `None` means more bytes are needed.
fn detect_http(buf: &[u8]) -> Option<Detection> {
    if buf.starts_with(H2_PREFACE) {
        return Some(Detection::H2);
    }
    // A proper prefix of the preface may still become it.
    if buf.len() < H2_PREFACE.len() && H2_PREFACE.starts_with(buf) {
        return None;
    }
    // Method: a non-empty token, then a space.
    let mut pos = 0;
    loop {
        match buf.get(pos) {
            None => return None,
            Some(b' ') if pos > 0 => break,
            Some(&b) if is_tchar(b) => pos += 1,
            Some(_) => return Some(Detection::Opaque),
        }
    }
    pos += 1;
    // Request target: visible bytes, non-empty.
    let start = pos;
    while let Some(&b) = buf.get(pos) {
        if b == b' ' {
            break;
        }
        if b <= 0x20 || b == 0x7f {
            return Some(Detection::Opaque);
        }
        pos += 1;
    }
    if pos == start && pos < buf.len() {
        return Some(Detection::Opaque);
    }
    if pos == buf.len() {
        return None;
    }
    pos += 1;
    // `HTTP/1.0\r\n` or `HTTP/1.1\r\n`.
    let rest = &buf[pos..];
    for (i, &want) in b"HTTP/1.".iter().enumerate() {
        match rest.get(i) {
            None => return None,
            Some(&b) if b == want => {}
            Some(_) => return Some(Detection::Opaque),
        }
    }
    match rest.get(7) {
        None => return None,
        Some(b'0' | b'1') => {}
        Some(_) => return Some(Detection::Opaque),
    }
    match rest.get(8) {
        None => None,
        Some(b'\r') => match rest.get(9) {
            None => None,
            Some(b'\n') => Some(Detection::Http1),
            Some(_) => Some(Detection::Opaque),
        },
        Some(_) => Some(Detection::Opaque),
    }
}

/// RFC 9110 `tchar`.
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// A TLS ClientHello, possibly split over several handshake records. `None` means more bytes
/// are needed.
fn detect_tls(buf: &[u8]) -> Option<Detection> {
    let mut hello = Vec::new();
    let mut pos = 0;
    let mut first = true;
    loop {
        let header = buf.get(pos..pos + 5)?;
        let len = usize::from(u16::from_be_bytes([header[3], header[4]]));
        if first {
            if header[1] != 3 || header[2] > 4 || len == 0 || len > 1 << 14 {
                return Some(Detection::Opaque);
            }
            first = false;
        } else if header[0] != 0x16 {
            return Some(Detection::Tls { sni: None });
        }
        pos += 5;
        let end = (pos + len).min(buf.len());
        hello.extend_from_slice(&buf[pos..end]);
        if hello.len() >= 4 && hello[0] != 1 {
            return Some(Detection::Opaque);
        }
        if hello.len() >= 4 {
            let want = 4
                + (usize::from(hello[1]) << 16
                    | usize::from(hello[2]) << 8
                    | usize::from(hello[3]));
            if hello.len() >= want {
                return Some(Detection::Tls {
                    sni: client_hello_sni(&hello[4..want]),
                });
            }
        }
        if end < pos + len {
            return None;
        }
        pos = end;
    }
}

/// Bounds-checked big-endian reader.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(n)?;
        self.0 = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<usize> {
        Some(usize::from(self.take(1)?[0]))
    }

    fn u16(&mut self) -> Option<usize> {
        let b = self.take(2)?;
        Some(usize::from(u16::from_be_bytes([b[0], b[1]])))
    }
}

/// The `host_name` of the `server_name` extension of a ClientHello body, if well formed.
fn client_hello_sni(body: &[u8]) -> Option<String> {
    let mut r = Reader(body);
    r.take(2 + 32)?; // version, random
    let n = r.u8()?;
    r.take(n)?; // session id
    let n = r.u16()?;
    r.take(n)?; // cipher suites
    let n = r.u8()?;
    r.take(n)?; // compression methods
    let n = r.u16()?;
    let mut exts = Reader(r.take(n)?);
    while !exts.0.is_empty() {
        let kind = exts.u16()?;
        let n = exts.u16()?;
        let data = exts.take(n)?;
        if kind != 0 {
            continue;
        }
        let mut list = Reader(data);
        let n = list.u16()?;
        let mut list = Reader(list.take(n)?);
        while !list.0.is_empty() {
            let name_type = list.u8()?;
            let n = list.u16()?;
            let name = list.take(n)?;
            if name_type == 0 {
                return host_name(name);
            }
        }
        return None;
    }
    None
}

fn host_name(name: &[u8]) -> Option<String> {
    if name.is_empty() || !name.iter().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    String::from_utf8(name.to_vec()).ok()
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
