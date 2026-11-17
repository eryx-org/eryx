//! Masking secret values out of HTTP responses as they're read.
//!
//! On a connection where requests get real secret values substituted, the
//! responses are framed as they arrive (status line and headers, then a
//! `Content-Length` or chunked body, or none), so that a value is masked
//! wherever the guest's HTTP client will see it whole: in the header block,
//! which is masked as one, and in the body *after* de-chunking, since a value
//! split across chunks is contiguous once the client reassembles them.
//!
//! The last few bytes of a body (one less than the longest value) are held
//! back so a value split across reads is whole before its first byte reaches
//! the guest. When they're released depends only on framing and on when
//! bytes arrive, never on what they are: inside a `Content-Length` body or a
//! chunk's data they wait for more of it or for its end, and in a
//! close-delimited body for the connection to close; between chunks, and
//! after a `101` or a `CONNECT`'s `2xx` (where substitution stops), they
//! also go out after [`CARRY_GRACE`] of quiet.
//!
//! Anything else fails closed: bytes that aren't a response to a recorded
//! request, or framing the masker can't follow, end the connection for the
//! guest (every later read fails) rather than being passed through unframed.
//! The exception is a connection that's another protocol from the start (its
//! first write wasn't a request, or the server spoke first): nothing is ever
//! substituted on it, so it gets the same best-effort masking as after a
//! `101`.
//!
//! A chunked body is re-chunked: the de-chunked bytes the guest gets are the
//! server's, masked, but chunk sizes are the masker's own.

use std::collections::VecDeque;
use std::time::Duration;

/// How long held-back bytes wait for more data where waiting isn't bounded
/// by framing: between chunks (so a token stream's events are late by at
/// most this) and for bytes that aren't framed HTTP.
pub(super) const CARRY_GRACE: Duration = Duration::from_millis(100);

/// Why a connection failed when the server sent something with no request
/// awaiting a response (e.g. a `408` on an idle keep-alive connection, just
/// before closing it): the guest's read ends the stream cleanly instead.
pub(super) const UNSOLICITED: &str = "the server sent a response to no request";

/// Largest header block, chunk-size line or trailer section buffered while
/// waiting for its end. A response past it fails the connection.
const MAX_FRAMING_LEN: usize = 64 * 1024;

/// What a request asked for, which decides how its response is framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RequestKind {
    /// `HEAD`: the response has no body, whatever its headers say.
    Head,
    /// `CONNECT`: a `2xx` response turns the connection into a tunnel.
    Connect,
    /// With an `Upgrade` header: a `101` response switches protocols.
    Upgrade,
    /// Anything else.
    Other,
}

impl RequestKind {
    /// The kind of the request whose header block is `headers`.
    pub(super) fn of(headers: &[u8]) -> Self {
        let method = headers.split(|&b| b == b' ').next().unwrap_or_default();
        if method.eq_ignore_ascii_case(b"HEAD") {
            Self::Head
        } else if method.eq_ignore_ascii_case(b"CONNECT") {
            Self::Connect
        } else if super::switches_protocols(headers) {
            Self::Upgrade
        } else {
            Self::Other
        }
    }
}

/// Where the masker is in the responses on a connection.
#[derive(Debug)]
enum Frame {
    /// Reading a status line and headers, buffered in `pending`.
    Head,
    /// Inside a `Content-Length` body with this many bytes left.
    Length(usize),
    /// Inside a chunked body.
    Chunked(Chunk),
    /// Inside a body that ends when the connection closes.
    UntilClose,
    /// After a `101` or a `CONNECT`'s `2xx`, or on a connection that's
    /// another protocol from the start: everything from here on is passed
    /// through masked.
    Raw,
    /// Responses it couldn't frame: nothing more goes to the guest.
    Failed(&'static str),
}

/// Where a chunked body is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chunk {
    /// On a chunk-size line, buffered in `pending`.
    Size,
    /// Inside chunk data, with this many bytes left.
    Data(usize),
    /// On the `\r\n` after chunk data, buffered in `pending`.
    DataEnd,
    /// In the trailer section, buffered in `pending`.
    Trailers,
}

/// Masks the responses on one connection.
#[derive(Debug)]
pub(super) struct ResponseMasker {
    /// `(real value, placeholder)` of each secret allowed for the host.
    masks: Vec<(Vec<u8>, Vec<u8>)>,
    /// How many trailing body bytes are held back.
    hold: usize,
    frame: Frame,
    /// Framing bytes (a header block, a chunk-size line, ...) not yet
    /// complete.
    pending: Vec<u8>,
    /// Body bytes held back, masked, not yet framed for the guest.
    carry: Vec<u8>,
    /// Bytes ready for the guest.
    pub(super) out: Vec<u8>,
    /// Whether the connection carries a credential, so an encoded response
    /// or an unasked-for `101` fails it (see [`Self::carrying_credential`]).
    credential: bool,
}

impl ResponseMasker {
    /// `None` without masks: then nothing is framed or held back.
    pub(super) fn new(masks: Vec<(Vec<u8>, Vec<u8>)>) -> Option<Self> {
        let hold = masks
            .iter()
            .map(|(real, _)| real.len().saturating_sub(1))
            .max()?;
        Some(Self {
            masks,
            hold,
            frame: Frame::Head,
            pending: Vec::new(),
            carry: Vec::new(),
            out: Vec::new(),
            credential: false,
        })
    }

    /// For connections carrying a child proxy credential, which the guest
    /// must never get back: fail the connection on a response with a
    /// `Content-Encoding` other than `identity` or a transfer coding other
    /// than `chunked`, rather than pass through a body that may hold a value
    /// in a form it can't see, and on a `101` to a request without `Upgrade`
    /// (which got the credential), rather than pass what follows through
    /// unframed.
    pub(super) fn carrying_credential(mut self, credential: bool) -> Self {
        self.credential = credential;
        self
    }

    #[cfg(test)]
    pub(super) fn hold(&self) -> usize {
        self.hold
    }

    /// Whether held-back bytes may go out after [`CARRY_GRACE`] of quiet:
    /// only between chunks and after a `101` or a `CONNECT`'s `2xx`.
    pub(super) fn may_release_on_quiet(&self) -> bool {
        !self.carry.is_empty()
            && match self.frame {
                Frame::Raw => true,
                Frame::Chunked(chunk) => !matches!(chunk, Chunk::Data(_)),
                Frame::Head | Frame::Length(_) | Frame::UntilClose | Frame::Failed(_) => false,
            }
    }

    /// Why the responses couldn't be framed, once they couldn't: the guest
    /// gets this error on every read after the bytes before it (or, for
    /// [`UNSOLICITED`], the end of the stream).
    pub(super) fn failure(&self) -> Option<&'static str> {
        match self.frame {
            Frame::Failed(why) => Some(why),
            _ => None,
        }
    }

    /// Whether no response has begun yet on the connection.
    pub(super) fn at_start(&self) -> bool {
        matches!(self.frame, Frame::Head) && self.pending.is_empty() && self.out.is_empty()
    }

    /// The connection is another protocol from the start, on which nothing
    /// is substituted: pass it through masked, held back until a quiet
    /// spell, as after a `101`.
    pub(super) fn raw_from_start(&mut self) {
        self.frame = Frame::Raw;
    }

    /// Release the held-back bytes after a quiet spell.
    pub(super) fn release_on_quiet(&mut self) {
        self.flush_carry();
    }

    /// The connection ended (or failed): everything still held goes out.
    pub(super) fn finish(&mut self) {
        if self.failure().is_some() {
            return;
        }
        self.flush_carry();
        let pending = std::mem::take(&mut self.pending);
        let masked = self.mask(&pending);
        self.out.extend(masked);
        self.frame = Frame::Raw;
    }

    /// Frame and mask `data`, just read; `requests` are the kinds of the
    /// requests on the connection still awaiting a response, oldest first.
    pub(super) fn feed(&mut self, mut data: &[u8], requests: &mut VecDeque<RequestKind>) {
        while !data.is_empty() {
            match self.frame {
                Frame::Failed(_) => return,
                Frame::Raw | Frame::UntilClose => {
                    self.body(data);
                    return;
                }
                Frame::Length(left) => {
                    let n = left.min(data.len());
                    self.body(&data[..n]);
                    data = &data[n..];
                    if n == left {
                        self.end_message();
                    } else {
                        self.frame = Frame::Length(left - n);
                    }
                }
                Frame::Chunked(Chunk::Data(left)) => {
                    let n = left.min(data.len());
                    self.body(&data[..n]);
                    data = &data[n..];
                    self.frame = Frame::Chunked(if n == left {
                        Chunk::DataEnd
                    } else {
                        Chunk::Data(left - n)
                    });
                }
                Frame::Head if !self.may_be_response(data, requests) => {
                    // Not a response to a request we framed: another
                    // protocol, or bytes the server sent unasked.
                    self.fail(if requests.is_empty() {
                        UNSOLICITED
                    } else {
                        "the server's reply isn't an HTTP/1.x response"
                    });
                }
                Frame::Head | Frame::Chunked(_) => {
                    // Framing bytes: take what this piece of framing needs.
                    let taken = self.framing(data, requests);
                    data = &data[taken..];
                }
            }
        }
    }

    /// Whether the buffered head plus `data` may still start a response:
    /// a request is awaiting one, and the bytes so far start `HTTP/`.
    fn may_be_response(&self, data: &[u8], requests: &VecDeque<RequestKind>) -> bool {
        const START: &[u8] = b"HTTP/";
        let seen: Vec<u8> = self
            .pending
            .iter()
            .chain(data)
            .take(START.len())
            .copied()
            .collect();
        !requests.is_empty() && START.starts_with(&seen)
    }

    /// Consume framing bytes from the start of `data` (buffering an
    /// incomplete piece in `pending`), returning how many were taken.
    fn framing(&mut self, data: &[u8], requests: &mut VecDeque<RequestKind>) -> usize {
        let start = self.pending.len();
        let end = match self.frame {
            Frame::Head => find_head_end(&self.pending, data),
            Frame::Chunked(Chunk::Size) => find(&self.pending, data, b"\r\n"),
            Frame::Chunked(Chunk::DataEnd) => {
                Some(2usize.saturating_sub(start).min(data.len())).filter(|&n| start + n == 2)
            }
            Frame::Chunked(Chunk::Trailers) => {
                // An empty line ends the trailer section, which may be empty.
                let mut joined = self.pending.clone();
                joined.extend_from_slice(&data[..data.len().min(2)]);
                if joined.starts_with(b"\r\n") {
                    Some(2 - start)
                } else {
                    find(&self.pending, data, b"\r\n\r\n")
                }
            }
            _ => unreachable!("framing is only read in a header or chunk position"),
        };
        let Some(n) = end else {
            self.pending.extend_from_slice(data);
            if self.pending.len() > MAX_FRAMING_LEN {
                self.fail("a response's headers or chunk framing are too long");
            }
            return data.len();
        };
        self.pending.extend_from_slice(&data[..n]);
        let piece = std::mem::take(&mut self.pending);
        match self.frame {
            Frame::Head => self.head(&piece, requests),
            Frame::Chunked(Chunk::Size) => match chunk_size(&piece) {
                Some(0) => self.frame = Frame::Chunked(Chunk::Trailers),
                Some(size) => self.frame = Frame::Chunked(Chunk::Data(size)),
                None => self.fail("malformed chunk size in a response"),
            },
            Frame::Chunked(Chunk::DataEnd) => {
                if piece == b"\r\n" {
                    self.frame = Frame::Chunked(Chunk::Size);
                } else {
                    self.fail("malformed chunk in a response");
                }
            }
            Frame::Chunked(Chunk::Trailers) => {
                if trailers_ok(&piece) {
                    self.flush_carry();
                    self.out.extend_from_slice(b"0\r\n");
                    let masked = self.mask(&piece);
                    self.out.extend(masked);
                    self.end_message();
                } else {
                    self.fail("malformed trailers in a response");
                }
            }
            _ => unreachable!(),
        }
        n
    }

    /// A complete status line and header block: mask it whole and decide
    /// how its body is framed.
    fn head(&mut self, head: &[u8], requests: &mut VecDeque<RequestKind>) {
        let Some(status) = status_code(head) else {
            return self.fail("malformed status line in a response");
        };
        // An interim response is followed by the final one to the same
        // request. `may_be_response` saw a request awaiting this one.
        let Some(&request) = requests.front() else {
            return self.fail(UNSOLICITED);
        };
        let Some(body) = response_body(head) else {
            return self.fail("ambiguous or malformed framing in a response");
        };
        // ponytail: fail closed on encoded bodies; decode, mask and send them
        // on uncompressed instead if servers that ignore `identity` matter.
        if self.credential && is_encoded(head) {
            return self.fail("an encoded response on a connection carrying a credential");
        }
        if self.credential && status == 101 && request != RequestKind::Upgrade {
            return self
                .fail("a 101 to a request without Upgrade on a connection carrying a credential");
        }
        let masked = self.mask(head);
        self.out.extend(masked);
        if (100..200).contains(&status) && status != 101 {
            return;
        }
        requests.pop_front();
        if status == 101 || (request == RequestKind::Connect && (200..300).contains(&status)) {
            self.frame = Frame::Raw;
            return;
        }
        if request == RequestKind::Head || status == 204 || status == 304 {
            return;
        }
        self.frame = match body {
            Body::Length(0) => Frame::Head,
            Body::Length(n) => Frame::Length(n),
            Body::Chunked => Frame::Chunked(Chunk::Size),
            Body::UntilClose => Frame::UntilClose,
        };
    }

    /// A message ended: release what's held and expect the next one.
    fn end_message(&mut self) {
        self.flush_carry();
        self.frame = Frame::Head;
    }

    /// Responses the masker can't frame: drop everything not yet released
    /// (it may hold part of a value) and fail every later read.
    fn fail(&mut self, why: &'static str) {
        self.pending.clear();
        self.carry.clear();
        self.frame = Frame::Failed(why);
    }

    /// Body bytes: mask them together with what's held, and hold back the
    /// last `hold` of the result.
    fn body(&mut self, data: &[u8]) {
        let mut joined = std::mem::take(&mut self.carry);
        joined.extend_from_slice(data);
        let mut masked = self.mask(&joined);
        let keep = self.hold.min(masked.len());
        self.carry = masked.split_off(masked.len() - keep);
        self.emit(&masked);
    }

    /// Release the held-back body bytes.
    fn flush_carry(&mut self) {
        let carry = std::mem::take(&mut self.carry);
        self.emit(&carry);
    }

    /// Body bytes for the guest, re-chunked in a chunked body.
    fn emit(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if matches!(self.frame, Frame::Chunked(_)) {
            self.out
                .extend_from_slice(format!("{:x}\r\n", data.len()).as_bytes());
            self.out.extend_from_slice(data);
            self.out.extend_from_slice(b"\r\n");
        } else {
            self.out.extend_from_slice(data);
        }
    }

    fn mask(&self, data: &[u8]) -> Vec<u8> {
        let mut data = data.to_vec();
        for (real, placeholder) in &self.masks {
            data = crate::secrets::replace_bytes(&data, real, placeholder);
        }
        data
    }
}

/// How many bytes of `data` complete `needle` after `buffered` (which
/// doesn't contain it yet), if it ends within `data`.
fn find(buffered: &[u8], data: &[u8], needle: &[u8]) -> Option<usize> {
    let from = buffered.len().saturating_sub(needle.len() - 1);
    let mut joined = buffered[from..].to_vec();
    joined.extend_from_slice(data);
    joined
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|pos| pos + needle.len() - (buffered.len() - from))
}

/// How many bytes of `data` end a response head (an empty line, after
/// `buffered`): servers' heads may end their lines with a bare LF, so an
/// empty line is LF, then an optional CR, then LF.
fn find_head_end(buffered: &[u8], data: &[u8]) -> Option<usize> {
    let from = buffered.len().saturating_sub(2);
    let mut joined = buffered[from..].to_vec();
    joined.extend_from_slice(data);
    let skipped = buffered.len() - from;
    (0..joined.len())
        .find_map(|i| match &joined[i..] {
            [b'\n', b'\n', ..] => Some(i + 2),
            [b'\n', b'\r', b'\n', ..] => Some(i + 3),
            _ => None,
        })
        .filter(|&end| end > skipped)
        .map(|end| end - skipped)
}

/// A response head's lines, without their line endings (CRLF or a bare LF)
/// and the empty line ending the head; `None` if a line has a bare CR.
fn head_lines(head: &[u8]) -> Option<Vec<&str>> {
    let text = std::str::from_utf8(head).ok()?;
    let mut lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    while lines.last() == Some(&"") {
        lines.pop();
    }
    (!lines.iter().any(|line| line.contains('\r'))).then_some(lines)
}

/// The status code of a response head (`HTTP/1.x NNN ...`), if it is one.
fn status_code(head: &[u8]) -> Option<u16> {
    let lines = head_lines(head)?;
    let line = lines.first()?.as_bytes();
    let rest = line
        .strip_prefix(b"HTTP/1.1 ")
        .or_else(|| line.strip_prefix(b"HTTP/1.0 "))?;
    let code = rest.get(..3)?;
    if !code.iter().all(u8::is_ascii_digit) || rest.get(3).is_some_and(|&b| b != b' ') {
        return None;
    }
    std::str::from_utf8(code).ok()?.parse().ok()
}

/// How a response's body is framed.
#[derive(Debug, PartialEq, Eq)]
enum Body {
    Length(usize),
    Chunked,
    UntilClose,
}

/// How the body of the response with header block `head` is framed, or
/// `None` if its framing is ambiguous or malformed: a bare CR, header names
/// that aren't tokens, both `Transfer-Encoding` and `Content-Length`, or
/// disagreeing lengths. (Lines may end with a bare LF: it's the server's
/// output, read the way clients read it.)
fn response_body(head: &[u8]) -> Option<Body> {
    let lines = head_lines(head)?;
    let (mut length, mut encoding) = (None, None);
    for line in lines.iter().skip(1) {
        let (name, value) = line.split_once(':')?;
        if name.is_empty() || !name.bytes().all(is_tchar) {
            return None;
        }
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("transfer-encoding") {
            if encoding.replace(value.to_ascii_lowercase()).is_some() {
                return None;
            }
        } else if name.eq_ignore_ascii_case("content-length") {
            let len: usize = (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                .then(|| value.parse().ok())
                .flatten()?;
            if length.replace(len).is_some_and(|old| old != len) {
                return None;
            }
        }
    }
    match (encoding.as_deref(), length) {
        (Some(_), Some(_)) => None,
        (Some("chunked"), None) => Some(Body::Chunked),
        // Any other coding: the body runs until the connection closes.
        (Some(_), None) | (None, None) => Some(Body::UntilClose),
        (None, Some(len)) => Some(Body::Length(len)),
    }
}

/// Whether the response with header block `head` has a content coding other
/// than `identity`, or a transfer coding other than `chunked`.
fn is_encoded(head: &[u8]) -> bool {
    let Some(lines) = head_lines(head) else {
        return true;
    };
    lines.iter().skip(1).any(|line| {
        let Some((name, value)) = line.split_once(':') else {
            return false;
        };
        let codings = || {
            value
                .split(',')
                .map(|c| c.trim_matches([' ', '\t']).to_ascii_lowercase())
                .filter(|c| !c.is_empty())
        };
        if name.eq_ignore_ascii_case("content-encoding") {
            codings().any(|c| c != "identity")
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            codings().any(|c| c != "chunked")
        } else {
            false
        }
    })
}

/// The size on a chunk-size line (ending in `\r\n`), parsed as RFC 9112
/// writes it: at least one hex digit, then optional whitespace and
/// extensions after a `;`, and no bare CR or LF.
fn chunk_size(line: &[u8]) -> Option<usize> {
    let line = line.strip_suffix(b"\r\n")?;
    if line.contains(&b'\r') || line.contains(&b'\n') {
        return None;
    }
    let digits = line.iter().take_while(|b| b.is_ascii_hexdigit()).count();
    if digits == 0 {
        return None;
    }
    let rest = &line[digits..];
    let rest = &rest[rest
        .iter()
        .take_while(|&&b| b == b' ' || b == b'\t')
        .count()..];
    if !rest.is_empty() && rest[0] != b';' {
        return None;
    }
    let size = std::str::from_utf8(&line[..digits]).ok()?;
    usize::from_str_radix(size, 16).ok()
}

/// Whether a trailer section (ending in an empty line) is well formed:
/// header lines with token names, and no bare CR or LF.
fn trailers_ok(section: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(section) else {
        return false;
    };
    let Some(body) = text.strip_suffix("\r\n") else {
        return false;
    };
    body.split_terminator("\r\n").all(|line| {
        !line.contains(['\r', '\n'])
            && line
                .split_once(':')
                .is_some_and(|(name, _)| !name.is_empty() && name.bytes().all(is_tchar))
    })
}

/// Whether `b` is an HTTP token character (RFC 9110 `tchar`).
// ponytail: copied from advisory B's net.rs; deduplicate once both land.
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"SECRETVALUE";

    fn masker() -> ResponseMasker {
        ResponseMasker::new(vec![(SECRET.to_vec(), b"PH".to_vec())]).unwrap()
    }

    /// Feed `pieces` in order and return everything the guest got.
    fn run(
        m: &mut ResponseMasker,
        requests: &mut VecDeque<RequestKind>,
        pieces: &[&[u8]],
    ) -> Vec<u8> {
        let mut got = Vec::new();
        for piece in pieces {
            m.feed(piece, requests);
            got.append(&mut m.out);
        }
        got
    }

    /// The body a client gets from a chunked message: the chunks' data.
    fn dechunk(mut wire: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        loop {
            let line = wire.windows(2).position(|w| w == b"\r\n").unwrap();
            let size =
                usize::from_str_radix(std::str::from_utf8(&wire[..line]).unwrap(), 16).unwrap();
            wire = &wire[line + 2..];
            if size == 0 {
                return body;
            }
            body.extend_from_slice(&wire[..size]);
            assert_eq!(&wire[size..size + 2], b"\r\n");
            wire = &wire[size + 2..];
        }
    }

    /// Split at every byte, a response's head, `Content-Length` body and
    /// chunked body (with the value split across chunks) come out masked
    /// the same, and nothing is held once the message ends.
    #[test]
    fn every_split_masks_the_same() {
        let head = b"HTTP/1.1 200 OK\r\nX-Echo: a SECRETVALUE b\r\nContent-Length: 17\r\n\r\n";
        let body = b"x SECRETVALUE yz.";
        let length: Vec<u8> = [&head[..], &body[..]].concat();
        let chunked_head = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        let chunked: Vec<u8> = [
            &chunked_head[..],
            b"5;x=1\r\nx SEC\r\n",
            b"8\r\nRETVALUE\r\n",
            b"3\r\n yz\r\n",
            b"0\r\nX-T: SECRETVALUE\r\n\r\n",
        ]
        .concat();
        for i in 0..=length.len() {
            let mut m = masker();
            let mut requests = VecDeque::from([RequestKind::Other]);
            let got = run(&mut m, &mut requests, &[&length[..i], &length[i..]]);
            assert_eq!(
                got, b"HTTP/1.1 200 OK\r\nX-Echo: a PH b\r\nContent-Length: 17\r\n\r\nx PH yz.",
                "split at {i}"
            );
            assert!(requests.is_empty());
        }
        for i in 0..=chunked.len() {
            let mut m = masker();
            let mut requests = VecDeque::from([RequestKind::Other]);
            let got = run(&mut m, &mut requests, &[&chunked[..i], &chunked[i..]]);
            let (got_head, rest) = got.split_at(chunked_head.len());
            assert_eq!(got_head, chunked_head, "split at {i}");
            assert_eq!(dechunk(rest), b"x PH yz", "split at {i}");
            assert!(got.ends_with(b"0\r\nX-T: PH\r\n\r\n"), "split at {i}");
        }
    }

    /// Responses without a body (to HEAD, 204, 304, and interim 1xx ones)
    /// don't wait for one, and the next response is framed.
    #[test]
    fn bodiless_responses_are_framed() {
        let next: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nSECRETVALUE";
        for (requests, first) in [
            (
                vec![RequestKind::Head, RequestKind::Other],
                &b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\n"[..],
            ),
            (
                vec![RequestKind::Other, RequestKind::Other],
                b"HTTP/1.1 204 No Content\r\n\r\n",
            ),
            (
                vec![RequestKind::Other, RequestKind::Other],
                b"HTTP/1.1 304 Not Modified\r\nContent-Length: 99\r\n\r\n",
            ),
            (vec![RequestKind::Other], b"HTTP/1.1 100 Continue\r\n\r\n"),
        ] {
            let mut m = masker();
            let mut requests = VecDeque::from(requests);
            let got = run(&mut m, &mut requests, &[first, next]);
            assert_eq!(
                got,
                [first, b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nPH"].concat(),
                "{:?}",
                String::from_utf8_lossy(first)
            );
            assert!(requests.is_empty());
        }
    }

    /// After a `101` or a `CONNECT`'s `2xx`, bytes are passed through
    /// (masked, held back until a quiet spell). Held bytes in a framed body
    /// (a close-delimited one included) never go out on quiet; between
    /// chunks they do.
    #[test]
    fn only_switched_connections_go_out_on_quiet() {
        for (request, wire) in [
            (
                RequestKind::Other,
                &b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: x\r\n\r\nhello"[..],
            ),
            (RequestKind::Connect, b"HTTP/1.1 200 OK\r\n\r\nhello"),
        ] {
            let mut m = masker();
            let mut requests = VecDeque::from([request]);
            m.feed(wire, &mut requests);
            assert!(
                m.may_release_on_quiet(),
                "{:?}",
                String::from_utf8_lossy(wire)
            );
            m.release_on_quiet();
            assert!(m.out.ends_with(b"hello"));
        }
        for wire in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\nSECRETVA"[..],
            b"HTTP/1.0 200 OK\r\n\r\nSECRETVA",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n14\r\nSECRETVA",
        ] {
            let mut m = masker();
            let mut requests = VecDeque::from([RequestKind::Other]);
            m.feed(wire, &mut requests);
            assert!(
                !m.may_release_on_quiet(),
                "{:?}",
                String::from_utf8_lossy(wire)
            );
        }
        let mut m = masker();
        let mut requests = VecDeque::from([RequestKind::Other]);
        m.feed(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n14\r\nSECRETVALUE and more\r\n",
            &mut requests,
        );
        assert!(m.may_release_on_quiet());
        // A close-delimited body's tail goes out when the connection ends.
        let mut m = masker();
        let mut requests = VecDeque::from([RequestKind::Other]);
        m.feed(b"HTTP/1.0 200 OK\r\n\r\nx SECRETVA", &mut requests);
        m.feed(b"LUE y", &mut requests);
        m.finish();
        assert_eq!(m.out, b"HTTP/1.0 200 OK\r\n\r\nx PH y");
    }

    /// Bytes that aren't a response to a recorded request (another
    /// protocol, an unsolicited response) and framing the masker can't
    /// follow fail the connection: nothing held, nor anything after, goes
    /// to the guest, at the end of the connection either.
    #[test]
    fn unframeable_responses_fail_closed() {
        for (requests, wire) in [
            (vec![RequestKind::Other], &b"+OK SECRETVA"[..]),
            (vec![], b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi"),
            (vec![RequestKind::Other], b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nSECRE\r\nzz\r\n"),
            (vec![RequestKind::Other], b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nSECRETVALUE"),
            (vec![RequestKind::Other], b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\nSECRETVALUE"),
            (vec![RequestKind::Other], b"HTTP/1.1 200 OK\r\nBad Name: x\r\n\r\nSECRETVALUE"),
            (vec![RequestKind::Other], b"HTTP/1.1 200 OK\r\nX: a\rb\r\n\r\nSECRETVALUE"),
            (vec![RequestKind::Other], b"HTTP/1.1 2000 OK\r\n\r\n"),
        ] {
            let mut m = masker();
            let mut requests = VecDeque::from(requests);
            m.feed(wire, &mut requests);
            m.feed(b"LUE and more", &mut requests);
            m.finish();
            assert!(m.failure().is_some(), "{:?}", String::from_utf8_lossy(wire));
            assert!(
                !m.out.windows(5).any(|w| w == b"SECRE"),
                "{:?}",
                String::from_utf8_lossy(&m.out)
            );
            assert!(!m.out.ends_with(b"more"));
        }
    }

    /// With encodings refused (a connection carrying a credential), a
    /// compressed or transfer-coded response fails the connection, and an
    /// `identity` or chunked one is framed as usual.
    #[test]
    fn encoded_responses_fail_closed_when_refused() {
        for wire in [
            &b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 11\r\n\r\nSECRETVALUE"[..],
            b"HTTP/1.1 200 OK\r\ncontent-encoding: identity, br\r\nContent-Length: 11\r\n\r\nSECRETVALUE",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\nb\r\nSECRETVALUE\r\n0\r\n\r\n",
        ] {
            let mut m = masker().carrying_credential(true);
            let mut requests = VecDeque::from([RequestKind::Other]);
            m.feed(wire, &mut requests);
            m.finish();
            assert!(m.failure().is_some(), "{:?}", String::from_utf8_lossy(wire));
            assert!(!m.out.windows(5).any(|w| w == b"SECRE"));
        }
        for wire in [
            &b"HTTP/1.1 200 OK\r\nContent-Encoding: identity\r\nContent-Length: 11\r\n\r\nSECRETVALUE"[..],
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nb\r\nSECRETVALUE\r\n0\r\n\r\n",
        ] {
            let mut m = masker().carrying_credential(true);
            let mut requests = VecDeque::from([RequestKind::Other]);
            let got = run(&mut m, &mut requests, &[wire]);
            assert!(m.failure().is_none(), "{:?}", String::from_utf8_lossy(wire));
            assert!(got.windows(2).any(|w| w == b"PH"));
        }
        // Without the refusal, an encoded response is framed as before.
        let mut m = masker();
        let mut requests = VecDeque::from([RequestKind::Other]);
        run(
            &mut m,
            &mut requests,
            &[b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 2\r\n\r\nhi"],
        );
        assert!(m.failure().is_none());
    }

    /// On a connection carrying a credential, a `101` is only taken for a
    /// request that asked for an upgrade: one to any other request fails the
    /// connection instead of switching it to pass-through.
    #[test]
    fn unasked_for_upgrades_fail_closed_when_carrying_a_credential() {
        let wire = &b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: x\r\n\r\nSECRETVALUE"[..];
        let mut m = masker().carrying_credential(true);
        let mut requests = VecDeque::from([RequestKind::Other]);
        m.feed(wire, &mut requests);
        m.finish();
        assert!(m.failure().is_some());
        assert!(!m.out.windows(5).any(|w| w == b"SECRE"));
        for (credential, request) in [(true, RequestKind::Upgrade), (false, RequestKind::Other)] {
            let mut m = masker().carrying_credential(credential);
            let mut requests = VecDeque::from([request]);
            m.feed(wire, &mut requests);
            assert!(m.failure().is_none(), "{credential} {request:?}");
            assert!(m.may_release_on_quiet(), "{credential} {request:?}");
        }
        assert_eq!(
            RequestKind::of(b"GET / HTTP/1.1\r\nupgrade: websocket\r\n\r\n"),
            RequestKind::Upgrade
        );
        assert_eq!(
            RequestKind::of(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"),
            RequestKind::Other
        );
    }

    /// Bare-LF line endings in a response head are accepted, as clients do.
    #[test]
    fn bare_lf_heads_are_framed() {
        let mut m = masker();
        let mut requests = VecDeque::from([RequestKind::Other, RequestKind::Other]);
        let got = run(
            &mut m,
            &mut requests,
            &[b"HTTP/1.1 200 OK\nX: SECRETVALUE\nContent-Length: 1\n\n!HTTP/1.1 204 No Content\n\r\n"],
        );
        assert_eq!(
            got,
            b"HTTP/1.1 200 OK\nX: PH\nContent-Length: 1\n\n!HTTP/1.1 204 No Content\n\r\n"
        );
        assert!(requests.is_empty() && m.failure().is_none());
    }
}
