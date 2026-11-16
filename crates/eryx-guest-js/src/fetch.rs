//! The HTTP/1.1 client behind the guest's `fetch` (`src/fetch.js`).
//!
//! It speaks plain HTTP/1.1 over the host's `eryx:net` TCP and TLS imports,
//! so the host applies its network policy, HTTP rules and secret
//! substitution to the bytes it writes, and does the TLS itself. Requests are
//! what the host can frame unambiguously: one per connection
//! (`Connection: close`), bodies with `Content-Length`, never chunked.
//!
//! The imports are synchronous, so a fetch blocks the guest until its
//! response (after any redirects) has been read in full.

use std::time::{Duration, Instant};

use rquickjs::{ArrayBuffer, Ctx, Function, IntoJs, Object, TypedArray};

use crate::url::Url;

/// Redirects followed before giving up, as in the fetch spec.
const MAX_REDIRECTS: usize = 20;

/// Largest response header block read before giving up.
const MAX_HEAD_LEN: usize = 64 * 1024;

/// Bytes asked for per read.
const READ_LEN: u32 = 64 * 1024;

/// Request headers `fetch` sets itself; ones the script passes are dropped,
/// as browsers drop forbidden headers.
const MANAGED_HEADERS: [&str; 9] = [
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
    "expect",
];

/// Headers describing a request body, dropped when a redirect drops the body.
const BODY_HEADERS: [&str; 4] = [
    "content-encoding",
    "content-language",
    "content-location",
    "content-type",
];

/// What to do with a redirect response (`RequestInit.redirect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Redirect {
    Follow,
    Error,
    Manual,
}

#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub(crate) method: String,
    pub(crate) url: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Option<Vec<u8>>,
    pub(crate) redirect: Redirect,
    /// When the whole fetch must be done by (`AbortSignal.timeout`).
    pub(crate) deadline: Option<Instant>,
}

#[derive(Debug)]
pub(crate) struct Response {
    pub(crate) status: u16,
    pub(crate) status_text: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) url: String,
    pub(crate) redirected: bool,
    pub(crate) body: Vec<u8>,
}

/// Why a fetch failed. `code` is the host's `tcp-error`/`tls-error` case
/// name for network errors, or one of this module's own.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Error {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl Error {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// An open connection. Dropping it closes it.
pub(crate) trait Connection {
    /// Read some bytes; empty at end of stream.
    fn read(&mut self, timeout_ms: u32) -> Result<Vec<u8>, Error>;
    /// Write some of `data`, returning how much.
    fn write(&mut self, data: &[u8], timeout_ms: u32) -> Result<usize, Error>;
}

/// Opens connections: `eryx:net` in the guest, a fake in tests.
pub(crate) trait Connector {
    type Conn: Connection;
    fn connect(
        &mut self,
        host: &str,
        port: u16,
        tls: bool,
        timeout_ms: u32,
    ) -> Result<Self::Conn, Error>;
}

/// Run `request`, following redirects as it asks.
pub(crate) fn fetch(connector: &mut impl Connector, request: Request) -> Result<Response, Error> {
    let Request {
        mut method,
        url,
        headers,
        mut body,
        redirect,
        deadline,
    } = request;
    method = request_method(&method)?;
    let mut url = Url::parse(&url)?;
    let mut headers = request_headers(headers)?;
    let mut redirected = false;

    for _ in 0..=MAX_REDIRECTS {
        let (status, status_text, response_headers, response_body) = exchange(
            connector,
            &method,
            &url,
            &headers,
            body.as_deref(),
            deadline,
        )?;
        let location = header(&response_headers, "location");
        let is_redirect = matches!(status, 301 | 302 | 303 | 307 | 308) && location.is_some();
        if !is_redirect || redirect == Redirect::Manual {
            return Ok(Response {
                status,
                status_text,
                headers: response_headers,
                url: url.to_string(),
                redirected,
                body: response_body,
            });
        }
        if redirect == Redirect::Error {
            return Err(Error::new(
                "redirect",
                format!("{url} redirected with {status}, and redirect is 'error'"),
            ));
        }
        let location = location.unwrap_or_default();
        let next = url.join(location)?;
        // 301/302 turn a POST into a GET, 303 turns anything but GET/HEAD
        // into one; either way the body goes.
        if (matches!(status, 301 | 302) && method == "POST")
            || (status == 303 && method != "GET" && method != "HEAD")
        {
            method = "GET".to_string();
            body = None;
            headers.retain(|(name, _)| !BODY_HEADERS.contains(&name.as_str()));
        }
        if !next.same_origin(&url) {
            headers.retain(|(name, _)| name != "authorization" && name != "proxy-authorization");
        }
        url = next;
        redirected = true;
    }
    Err(Error::new(
        "too-many-redirects",
        format!("more than {MAX_REDIRECTS} redirects"),
    ))
}

/// Whether `b` may appear in an HTTP token (a method or header name).
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Validate the method, uppercased. Checked here, not just in `fetch.js`,
/// whose checks a script can monkeypatch: the method goes into the request
/// line as is.
fn request_method(method: &str) -> Result<String, Error> {
    if method.is_empty() || !method.bytes().all(is_tchar) {
        return Err(Error::new(
            "invalid-method",
            format!("invalid method {method:?}"),
        ));
    }
    let method = method.to_ascii_uppercase();
    if matches!(method.as_str(), "CONNECT" | "TRACE" | "TRACK") {
        return Err(Error::new(
            "invalid-method",
            format!("method {method} is not allowed"),
        ));
    }
    Ok(method)
}

/// Validate the script's headers, lowercasing names and trimming values.
fn request_headers(headers: Vec<(String, String)>) -> Result<Vec<(String, String)>, Error> {
    let mut out = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        if name.is_empty() || !name.bytes().all(is_tchar) {
            return Err(Error::new(
                "invalid-header",
                format!("invalid header name {name:?}"),
            ));
        }
        let value = value.trim_matches([' ', '\t']);
        if value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
            return Err(Error::new(
                "invalid-header",
                format!("invalid value for header {name:?}"),
            ));
        }
        let name = name.to_ascii_lowercase();
        if !MANAGED_HEADERS.contains(&name.as_str()) {
            out.push((name, value.to_string()));
        }
    }
    Ok(out)
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Milliseconds left before `deadline`, for an `eryx:net` call: `0` (the
/// host's default) without one.
fn timeout_ms(deadline: Option<Instant>) -> Result<u32, Error> {
    let Some(deadline) = deadline else {
        return Ok(0);
    };
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(Error::new("timed-out", "the request timed out"));
    }
    // Round up, so a sub-millisecond remainder isn't the host default.
    Ok(u32::try_from(left.as_millis()).unwrap_or(u32::MAX).max(1))
}

type Exchange = (u16, String, Vec<(String, String)>, Vec<u8>);

/// Send one request on a new connection and read its response.
fn exchange(
    connector: &mut impl Connector,
    method: &str,
    url: &Url,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    deadline: Option<Instant>,
) -> Result<Exchange, Error> {
    let mut conn = connector.connect(&url.host, url.port(), url.https, timeout_ms(deadline)?)?;

    let request = serialize(method, url, headers, body);
    let mut written = 0;
    while written < request.len() {
        let n = conn.write(&request[written..], timeout_ms(deadline)?)?;
        if n == 0 {
            return Err(Error::new(
                "io-error",
                "the connection stopped accepting data",
            ));
        }
        written += n;
    }

    read_response(&mut conn, method == "HEAD", deadline)
}

fn serialize(
    method: &str,
    url: &Url,
    headers: &[(String, String)],
    body: Option<&[u8]>,
) -> Vec<u8> {
    let mut head = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\n",
        url.target,
        url.authority()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    for (name, default) in [
        ("accept", "*/*"),
        ("accept-encoding", "identity"),
        ("user-agent", "eryx-fetch"),
    ] {
        if header(headers, name).is_none() {
            head.push_str(&format!("{name}: {default}\r\n"));
        }
    }
    // A POST or PUT without a body still says so, as the fetch spec does.
    if body.is_some() || matches!(method, "POST" | "PUT" | "PATCH") {
        head.push_str(&format!(
            "Content-Length: {}\r\n",
            body.map_or(0, <[u8]>::len)
        ));
    }
    head.push_str("Connection: close\r\n\r\n");
    let mut request = head.into_bytes();
    request.extend_from_slice(body.unwrap_or_default());
    request
}

/// Read more into `buf`; `false` at end of stream.
fn read_more(
    conn: &mut impl Connection,
    buf: &mut Vec<u8>,
    deadline: Option<Instant>,
) -> Result<bool, Error> {
    let chunk = conn.read(timeout_ms(deadline)?)?;
    buf.extend_from_slice(&chunk);
    Ok(!chunk.is_empty())
}

fn read_response(
    conn: &mut impl Connection,
    head_request: bool,
    deadline: Option<Instant>,
) -> Result<Exchange, Error> {
    let mut buf = Vec::new();
    let (status, status_text, headers) = loop {
        let mut slots = [httparse::EMPTY_HEADER; 128];
        let mut parsed = httparse::Response::new(&mut slots);
        match parsed.parse(&buf) {
            Ok(httparse::Status::Complete(len)) => {
                let status = parsed.code.unwrap_or_default();
                let status_text = parsed.reason.unwrap_or_default().to_string();
                let headers: Vec<(String, String)> = parsed
                    .headers
                    .iter()
                    .map(|h| {
                        (
                            h.name.to_ascii_lowercase(),
                            String::from_utf8_lossy(h.value).into_owned(),
                        )
                    })
                    .collect();
                buf.drain(..len);
                // Informational responses (100 Continue, 103 Early Hints)
                // precede the real one.
                if (100..200).contains(&status) && status != 101 {
                    continue;
                }
                break (status, status_text, headers);
            }
            Ok(httparse::Status::Partial) => {
                if buf.len() > MAX_HEAD_LEN {
                    return Err(Error::new("bad-response", "response headers too large"));
                }
                if !read_more(conn, &mut buf, deadline)? {
                    return Err(Error::new(
                        "bad-response",
                        "the connection closed before the response headers",
                    ));
                }
            }
            Err(e) => return Err(Error::new("bad-response", format!("bad response: {e}"))),
        }
    };

    if head_request || status == 101 || status == 204 || status == 304 {
        return Ok((status, status_text, headers, Vec::new()));
    }
    let chunked = header(&headers, "transfer-encoding").is_some_and(|te| {
        te.rsplit(',')
            .next()
            .is_some_and(|last| last.trim().eq_ignore_ascii_case("chunked"))
    });
    let body = if chunked {
        loop {
            // ponytail: re-decodes from the start after each read, quadratic in
            // the number of reads; track the decode position if bodies get big.
            if let Some(body) = decode_chunked(&buf)? {
                break body;
            }
            if !read_more(conn, &mut buf, deadline)? {
                return Err(Error::new("bad-response", "the chunked body ended early"));
            }
        }
    } else if let Some(length) = content_length(&headers)? {
        while buf.len() < length {
            if !read_more(conn, &mut buf, deadline)? {
                return Err(Error::new("bad-response", "the body ended early"));
            }
        }
        buf.truncate(length);
        buf
    } else {
        while read_more(conn, &mut buf, deadline)? {}
        buf
    };
    Ok((status, status_text, headers, body))
}

fn content_length(headers: &[(String, String)]) -> Result<Option<usize>, Error> {
    let mut length = None;
    for (name, value) in headers {
        if name != "content-length" {
            continue;
        }
        for part in value.split(',') {
            let parsed: usize = part
                .trim()
                .parse()
                .map_err(|_| Error::new("bad-response", format!("bad Content-Length {value:?}")))?;
            if length.is_some_and(|l| l != parsed) {
                return Err(Error::new("bad-response", "conflicting Content-Length"));
            }
            length = Some(parsed);
        }
    }
    Ok(length)
}

/// Decode a chunked body, or `None` if `data` doesn't hold all of it yet.
fn decode_chunked(data: &[u8]) -> Result<Option<Vec<u8>>, Error> {
    let bad = |what: &str| Error::new("bad-response", format!("bad chunked body: {what}"));
    let mut body = Vec::new();
    let mut pos = 0;
    loop {
        let (len, size) = match httparse::parse_chunk_size(&data[pos..]) {
            Ok(httparse::Status::Complete(parsed)) => parsed,
            Ok(httparse::Status::Partial) => return Ok(None),
            Err(_) => return Err(bad("invalid chunk size")),
        };
        pos += len;
        if size == 0 {
            // Trailers, if any, end with an empty line.
            let rest = &data[pos..];
            if rest.starts_with(b"\r\n") {
                return Ok(Some(body));
            }
            return Ok(rest.windows(4).any(|w| w == b"\r\n\r\n").then_some(body));
        }
        let size = usize::try_from(size).map_err(|_| bad("chunk too large"))?;
        let end = pos
            .checked_add(size)
            .ok_or_else(|| bad("chunk too large"))?;
        if data.len() < end + 2 {
            return Ok(None);
        }
        if &data[end..end + 2] != b"\r\n" {
            return Err(bad("missing CRLF after a chunk"));
        }
        body.extend_from_slice(&data[pos..end]);
        pos = end + 2;
    }
}

// -----------------------------------------------------------------------------
// The guest side: `eryx:net` connections and the `__eryx.fetch` native.
// -----------------------------------------------------------------------------

fn net_error(error: eryx_guest_call::net::NetError) -> Error {
    Error::new(
        error.kind,
        error
            .message
            .unwrap_or_else(|| error.kind.replace('-', " ")),
    )
}

fn net<T>(result: Result<Result<T, eryx_guest_call::net::NetError>, String>) -> Result<T, Error> {
    result
        .map_err(|e| Error::new("io-error", e))?
        .map_err(net_error)
}

/// A TCP or TLS connection over `eryx:net`.
struct NetConnection {
    tcp: u32,
    tls: Option<u32>,
}

impl Connection for NetConnection {
    fn read(&mut self, timeout_ms: u32) -> Result<Vec<u8>, Error> {
        let wit = crate::wit::wit();
        let (tls, handle) = self.tls.map_or((false, self.tcp), |h| (true, h));
        net(eryx_guest_call::net::net_read(
            wit, tls, handle, READ_LEN, timeout_ms,
        ))
    }

    fn write(&mut self, data: &[u8], timeout_ms: u32) -> Result<usize, Error> {
        let wit = crate::wit::wit();
        let (tls, handle) = self.tls.map_or((false, self.tcp), |h| (true, h));
        net(eryx_guest_call::net::net_write(
            wit, tls, handle, data, timeout_ms,
        ))
        .map(|n| n as usize)
    }
}

impl Drop for NetConnection {
    fn drop(&mut self) {
        let wit = crate::wit::wit();
        if let Some(tls) = self.tls {
            eryx_guest_call::net::net_close(wit, true, tls);
        }
        eryx_guest_call::net::net_close(wit, false, self.tcp);
    }
}

struct NetConnector;

impl Connector for NetConnector {
    type Conn = NetConnection;

    fn connect(
        &mut self,
        host: &str,
        port: u16,
        tls: bool,
        timeout_ms: u32,
    ) -> Result<NetConnection, Error> {
        let wit = crate::wit::wit();
        let tcp = net(eryx_guest_call::net::tcp_connect(
            wit, host, port, timeout_ms,
        ))?;
        let mut conn = NetConnection { tcp, tls: None };
        if tls {
            conn.tls = Some(net(eryx_guest_call::net::tls_upgrade(
                wit, tcp, host, timeout_ms,
            ))?);
        }
        Ok(conn)
    }
}

const FETCH_JS: &str = include_str!("fetch.js");

/// Install the fetch API (`src/fetch.js`) on the context's globals.
pub(crate) fn install(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    let native = Object::new(ctx.clone())?;
    native.set("fetch", Function::new(ctx.clone(), native_fetch)?)?;
    native.set("encodeUtf8", Function::new(ctx.clone(), encode_utf8)?)?;
    native.set("decodeUtf8", Function::new(ctx.clone(), decode_utf8)?)?;
    let install: Function<'_> = ctx.eval(FETCH_JS)?;
    install.call((native,))
}

/// `encodeUtf8(string)`: the string's UTF-8 bytes, as an `ArrayBuffer`.
pub(crate) fn encode_utf8(ctx: Ctx<'_>, text: String) -> rquickjs::Result<ArrayBuffer<'_>> {
    ArrayBuffer::new(ctx, text.into_bytes())
}

/// `decodeUtf8(arrayBuffer)`: the bytes as a string, invalid UTF-8 replaced
/// with U+FFFD as `TextDecoder` does.
fn decode_utf8(buffer: ArrayBuffer<'_>) -> String {
    // SAFETY: the bytes are copied before any JavaScript runs.
    let bytes = unsafe { buffer.as_bytes() }.unwrap_or_default();
    String::from_utf8_lossy(bytes).into_owned()
}

/// `fetch(options)`: run a request and return the response as a plain
/// object, or throw `{code, message}`. `src/fetch.js` builds the `fetch` API
/// on it.
fn native_fetch<'js>(ctx: Ctx<'js>, options: Object<'js>) -> rquickjs::Result<Object<'js>> {
    let request = (|| {
        let headers: Vec<Vec<String>> = options.get("headers")?;
        let body: Option<TypedArray<'js, u8>> = options.get("body")?;
        let timeout: Option<f64> = options.get("timeoutMs")?;
        let redirect = match options.get::<_, String>("redirect")?.as_str() {
            "error" => Redirect::Error,
            "manual" => Redirect::Manual,
            _ => Redirect::Follow,
        };
        Ok::<_, rquickjs::Error>(Request {
            method: options.get("method")?,
            url: options.get("url")?,
            headers: headers
                .into_iter()
                .filter_map(|pair| {
                    let mut pair = pair.into_iter();
                    Some((pair.next()?, pair.next()?))
                })
                .collect(),
            // SAFETY: the bytes are copied before any JavaScript runs.
            body: body.and_then(|b| unsafe { b.as_bytes() }.map(<[u8]>::to_vec)),
            redirect,
            deadline: timeout
                .map(|ms| Instant::now() + Duration::from_secs_f64(ms.max(0.0) / 1000.0)),
        })
    })()?;

    match fetch(&mut NetConnector, request) {
        Ok(response) => {
            let out = Object::new(ctx.clone())?;
            out.set("status", response.status)?;
            out.set("statusText", response.status_text)?;
            out.set("url", response.url)?;
            out.set("redirected", response.redirected)?;
            let headers: Vec<Vec<String>> = response
                .headers
                .into_iter()
                .map(|(name, value)| vec![name, value])
                .collect();
            out.set("headers", headers)?;
            out.set("body", ArrayBuffer::new(ctx.clone(), response.body)?)?;
            Ok(out)
        }
        Err(error) => {
            let thrown = Object::new(ctx.clone())?;
            thrown.set("code", error.code)?;
            thrown.set("message", error.message)?;
            Err(ctx.throw(thrown.into_js(&ctx)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use super::*;

    /// `(host, port, tls, request bytes)` per connection.
    type Sent = Rc<RefCell<Vec<(String, u16, bool, Vec<u8>)>>>;

    /// Connections that each serve one canned response, a few bytes per read,
    /// and record what was written to them.
    #[derive(Default)]
    struct Fake {
        responses: VecDeque<&'static [u8]>,
        sent: Sent,
    }

    struct FakeConn {
        response: &'static [u8],
        sent: Sent,
    }

    impl Connection for FakeConn {
        fn read(&mut self, _: u32) -> Result<Vec<u8>, Error> {
            let n = self.response.len().min(7);
            let (chunk, rest) = self.response.split_at(n);
            self.response = rest;
            Ok(chunk.to_vec())
        }

        fn write(&mut self, data: &[u8], _: u32) -> Result<usize, Error> {
            let n = data.len().min(10);
            self.sent
                .borrow_mut()
                .last_mut()
                .unwrap()
                .3
                .extend_from_slice(&data[..n]);
            Ok(n)
        }
    }

    impl Connector for Fake {
        type Conn = FakeConn;

        fn connect(&mut self, host: &str, port: u16, tls: bool, _: u32) -> Result<FakeConn, Error> {
            self.sent
                .borrow_mut()
                .push((host.to_string(), port, tls, Vec::new()));
            Ok(FakeConn {
                response: self.responses.pop_front().expect("unexpected connection"),
                sent: Rc::clone(&self.sent),
            })
        }
    }

    fn fake(responses: &[&'static [u8]]) -> Fake {
        Fake {
            responses: responses.iter().copied().collect(),
            ..Fake::default()
        }
    }

    fn request(method: &str, url: &str) -> Request {
        Request {
            method: method.to_string(),
            url: url.to_string(),
            headers: Vec::new(),
            body: None,
            redirect: Redirect::Follow,
            deadline: None,
        }
    }

    fn sent(fake: &Fake, i: usize) -> (String, u16, bool, String) {
        let (host, port, tls, bytes) = fake.sent.borrow()[i].clone();
        (host, port, tls, String::from_utf8(bytes).unwrap())
    }

    #[test]
    fn get_with_content_length() {
        let mut net = fake(&[b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX-A: 1\r\n\r\nhello"]);
        let mut req = request("get", "https://example.com/p?q=1#frag");
        req.headers = vec![("X-Token".into(), " abc ".into())];
        let res = fetch(&mut net, req).unwrap();

        assert_eq!((res.status, res.status_text.as_str()), (200, "OK"));
        assert_eq!(res.body, b"hello");
        assert_eq!(header(&res.headers, "x-a"), Some("1"));
        assert!(!res.redirected);
        let (host, port, tls, wire) = sent(&net, 0);
        assert_eq!((host.as_str(), port, tls), ("example.com", 443, true));
        assert_eq!(
            wire,
            "GET /p?q=1 HTTP/1.1\r\nHost: example.com\r\nx-token: abc\r\naccept: */*\r\n\
             accept-encoding: identity\r\nuser-agent: eryx-fetch\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn post_body_ipv6_and_port() {
        let mut net = fake(&[b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n"]);
        let mut req = request("POST", "http://[::1]:8080/x");
        req.body = Some(b"{}".to_vec());
        req.headers = vec![
            ("content-type".into(), "application/json".into()),
            ("Host".into(), "evil".into()),
            ("Content-Length".into(), "99".into()),
        ];
        assert_eq!(fetch(&mut net, req).unwrap().status, 201);
        let (host, port, tls, wire) = sent(&net, 0);
        assert_eq!((host.as_str(), port, tls), ("::1", 8080, false));
        assert!(
            wire.starts_with(
                "POST /x HTTP/1.1\r\nHost: [::1]:8080\r\ncontent-type: application/json\r\n"
            ),
            "{wire}"
        );
        assert!(
            wire.ends_with("Content-Length: 2\r\nConnection: close\r\n\r\n{}"),
            "{wire}"
        );
        assert!(!wire.contains("evil") && !wire.contains("99"));
    }

    #[test]
    fn chunked_with_extensions_and_trailers() {
        let mut net = fake(&[b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
            5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nX-Trailer: t\r\n\r\n"]);
        let res = fetch(&mut net, request("GET", "http://a.test/")).unwrap();
        assert_eq!(res.body, b"hello world");
    }

    #[test]
    fn body_read_to_close_and_informational_skipped() {
        let mut net = fake(&[b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\n\r\nuntil close"]);
        let res = fetch(&mut net, request("GET", "http://a.test/")).unwrap();
        assert_eq!(
            (res.status, res.body.as_slice()),
            (200, &b"until close"[..])
        );
    }

    #[test]
    fn no_body_for_head_204_304() {
        for (method, response) in [
            (
                "HEAD",
                &b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n"[..],
            ),
            (
                "GET",
                b"HTTP/1.1 204 No Content\r\nContent-Length: 10\r\n\r\n",
            ),
            (
                "GET",
                b"HTTP/1.1 304 Not Modified\r\nContent-Length: 10\r\n\r\n",
            ),
        ] {
            let mut net = fake(&[response]);
            assert!(
                fetch(&mut net, request(method, "http://a.test/"))
                    .unwrap()
                    .body
                    .is_empty()
            );
        }
    }

    #[test]
    fn redirect_303_post_becomes_get_and_cross_origin_drops_authorization() {
        let mut net = fake(&[
            b"HTTP/1.1 303 See Other\r\nLocation: /next\r\nContent-Length: 0\r\n\r\n",
            b"HTTP/1.1 302 Found\r\nLocation: https://other.test/end\r\nContent-Length: 0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        ]);
        let mut req = request("POST", "https://a.test/start");
        req.body = Some(b"data".to_vec());
        req.headers = vec![
            ("authorization".into(), "Bearer t".into()),
            ("content-type".into(), "text/plain".into()),
        ];
        let res = fetch(&mut net, req).unwrap();

        assert_eq!(res.body, b"ok");
        assert!(res.redirected);
        assert_eq!(res.url, "https://other.test/end");
        let (_, _, _, second) = sent(&net, 1);
        assert!(second.starts_with("GET /next HTTP/1.1\r\n"), "{second}");
        assert!(
            second.contains("authorization: Bearer t"),
            "same origin keeps it"
        );
        assert!(!second.contains("content-type") && !second.contains("data"));
        let (host, _, _, third) = sent(&net, 2);
        assert_eq!(host, "other.test");
        assert!(!third.contains("authorization"), "{third}");
    }

    #[test]
    fn redirect_307_keeps_method_and_body() {
        let mut net = fake(&[
            b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /b\r\nContent-Length: 0\r\n\r\n",
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
        ]);
        let mut req = request("PUT", "http://a.test/a");
        req.body = Some(b"x".to_vec());
        fetch(&mut net, req).unwrap();
        let (_, _, _, second) = sent(&net, 1);
        assert!(second.starts_with("PUT /b HTTP/1.1\r\n") && second.ends_with("\r\n\r\nx"));
    }

    #[test]
    fn redirect_modes() {
        let redirect: &[u8] = b"HTTP/1.1 302 Found\r\nLocation: /b\r\nContent-Length: 0\r\n\r\n";
        let mut req = request("GET", "http://a.test/a");
        req.redirect = Redirect::Manual;
        assert_eq!(
            fetch(&mut fake(&[redirect]), req.clone()).unwrap().status,
            302
        );
        req.redirect = Redirect::Error;
        assert_eq!(
            fetch(&mut fake(&[redirect]), req).unwrap_err().code,
            "redirect"
        );

        let mut net = fake(&[redirect; MAX_REDIRECTS + 1]);
        let err = fetch(&mut net, request("GET", "http://a.test/a")).unwrap_err();
        assert_eq!(err.code, "too-many-redirects");
    }

    #[test]
    fn rejects_bad_urls_and_headers_before_connecting() {
        for (url, code) in [
            ("ftp://a.test/", "unsupported-scheme"),
            ("not a url", "invalid-url"),
            ("https://user:pw@a.test/", "invalid-url"),
        ] {
            assert_eq!(
                fetch(&mut fake(&[]), request("GET", url)).unwrap_err().code,
                code
            );
        }
        for method in [
            "GET /x HTTP/1.1\r\nX:",
            "",
            "G T",
            "connect",
            "TRACE",
            "track",
        ] {
            let err = fetch(&mut fake(&[]), request(method, "http://a.test/")).unwrap_err();
            assert_eq!(err.code, "invalid-method", "{method:?}");
        }
        for (name, value) in [("x", "a\r\nInjected: 1"), ("bad name", "v"), ("", "v")] {
            let mut req = request("GET", "http://a.test/");
            req.headers = vec![(name.into(), value.into())];
            assert_eq!(
                fetch(&mut fake(&[]), req).unwrap_err().code,
                "invalid-header"
            );
        }
    }

    #[test]
    fn bad_responses() {
        for response in [
            &b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort"[..],
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nab",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhel",
            b"HTTP/1.1 200 OK\r\nContent",
            b"garbage\r\n\r\n",
        ] {
            let err = fetch(&mut fake(&[response]), request("GET", "http://a.test/")).unwrap_err();
            assert_eq!(
                err.code,
                "bad-response",
                "{}",
                String::from_utf8_lossy(response)
            );
        }
    }

    #[test]
    fn expired_deadline_times_out() {
        let mut req = request("GET", "http://a.test/");
        req.deadline = Some(Instant::now());
        assert_eq!(fetch(&mut fake(&[]), req).unwrap_err().code, "timed-out");
    }
}
