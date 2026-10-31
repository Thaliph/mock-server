//! Incremental scanner for a head or a trailer section: finds its end, enforces the limits
//! and checks the line syntax byte by byte, so an unframed input is detected at the byte
//! that decides it.

use super::{HEAD_LIMIT, LINE_LIMIT, Reason};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Request,
    Response,
    Trailer,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Scan {
    More,
    /// The blank line was read; [`HeadScanner::lines`] gives the head.
    Done,
    Unframed(Reason),
}

#[derive(Debug)]
pub(super) struct HeadScanner {
    kind: Kind,
    buf: Vec<u8>,
    lines: usize,
    line_start: usize,
    cr: bool,
    /// The previous header line is `content-length` or `transfer-encoding`.
    prev_framing: bool,
}

impl HeadScanner {
    pub(super) fn new(kind: Kind) -> Self {
        Self {
            kind,
            buf: Vec::new(),
            lines: 0,
            line_start: 0,
            cr: false,
            prev_framing: false,
        }
    }

    /// Consume bytes up to and including the end of the head. Returns how many were used.
    pub(super) fn feed(&mut self, data: &[u8]) -> (usize, Scan) {
        for (i, &b) in data.iter().enumerate() {
            let used = i + 1;
            let had_cr = self.cr;
            let eol = if self.cr {
                if b != b'\n' {
                    return (used, Scan::Unframed(Reason::BareCr));
                }
                true
            } else if b == b'\n' {
                true
            } else if b == b'\r' {
                self.cr = true;
                false
            } else {
                if self.buf.len() == self.line_start && self.lines >= LINE_LIMIT {
                    return (used, Scan::Unframed(self.too_many_lines()));
                }
                false
            };
            // A leading blank line before a start line is skipped, not buffered.
            let empty = eol && self.buf.len() == self.line_start + usize::from(had_cr);
            if empty && self.lines == 0 && self.kind != Kind::Trailer {
                self.buf.clear();
                self.line_start = 0;
                self.cr = false;
                continue;
            }
            self.buf.push(b);
            if self.buf.len() > HEAD_LIMIT {
                return (used, Scan::Unframed(self.too_large()));
            }
            if !eol {
                continue;
            }
            self.cr = false;
            if empty {
                return (used, Scan::Done);
            }
            let end = self.buf.len() - 1 - usize::from(had_cr);
            let line = &self.buf[self.line_start..end];
            if let Err(reason) = check_line(self.kind, self.lines, line, &mut self.prev_framing) {
                return (used, Scan::Unframed(reason));
            }
            self.lines += 1;
            self.line_start = self.buf.len();
        }
        (data.len(), Scan::More)
    }

    fn too_large(&self) -> Reason {
        if self.kind == Kind::Trailer {
            Reason::TrailersTooLarge
        } else {
            Reason::HeadTooLarge
        }
    }

    fn too_many_lines(&self) -> Reason {
        if self.kind == Kind::Trailer {
            Reason::TooManyTrailerLines
        } else {
            Reason::TooManyHeaderLines
        }
    }

    /// The lines read so far, line endings removed.
    pub(super) fn lines(&self) -> impl Iterator<Item = &[u8]> {
        self.buf
            .split(|&b| b == b'\n')
            .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
            .filter(|l| !l.is_empty())
    }
}

/// RFC 9110 `tchar`.
pub(super) fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

pub(super) fn is_framing_header(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"content-length") || name.eq_ignore_ascii_case(b"transfer-encoding")
}

fn check_line(
    kind: Kind,
    index: usize,
    line: &[u8],
    prev_framing: &mut bool,
) -> Result<(), Reason> {
    match (kind, index) {
        (Kind::Request, 0) => {
            return parse_request_line(line)
                .map(|_| ())
                .ok_or(Reason::BadStartLine);
        }
        (Kind::Response, 0) => {
            return parse_status_line(line)
                .map(|_| ())
                .ok_or(Reason::BadStartLine);
        }
        _ => {}
    }
    if matches!(line[0], b' ' | b'\t') {
        let first_header = if kind == Kind::Trailer {
            index == 0
        } else {
            index == 1
        };
        return if first_header || *prev_framing {
            Err(Reason::ObsFold)
        } else {
            Ok(())
        };
    }
    let Some(colon) = line.iter().position(|&b| b == b':') else {
        return Err(Reason::BadHeaderLine);
    };
    let name = &line[..colon];
    if name.is_empty() || !name.iter().copied().all(is_tchar) {
        return Err(Reason::BadHeaderLine);
    }
    *prev_framing = is_framing_header(name);
    Ok(())
}

/// `(method, target, minor version)` of a request line.
pub(super) fn parse_request_line(line: &[u8]) -> Option<(&[u8], &[u8], u8)> {
    let mut parts = line.splitn(3, |&b| b == b' ');
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if method.is_empty() || !method.iter().copied().all(is_tchar) {
        return None;
    }
    if target.is_empty() || target.iter().any(|&b| b <= 0x20 || b == 0x7f) {
        return None;
    }
    Some((method, target, minor_version(version)?))
}

/// `(minor version, status)` of a status line.
pub(super) fn parse_status_line(line: &[u8]) -> Option<(u8, u16)> {
    let minor = minor_version(line.get(..8)?)?;
    let rest = &line[8..];
    let digits = rest.strip_prefix(b" ")?.get(..3)?;
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    match rest.get(4..) {
        None | Some([]) => {}
        Some([b' ', ..]) => {}
        Some(_) => return None,
    }
    let status = digits
        .iter()
        .fold(0u16, |n, d| n * 10 + u16::from(d - b'0'));
    (100..=599).contains(&status).then_some((minor, status))
}

fn minor_version(v: &[u8]) -> Option<u8> {
    match v {
        b"HTTP/1.0" => Some(0),
        b"HTTP/1.1" => Some(1),
        _ => None,
    }
}
