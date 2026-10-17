/**
 * Browser transport for the eryx:net shims: HTTP(S) over fetch().
 *
 * Browsers can't open raw sockets, but TLS is terminated host-side (Python's
 * ssl module calls tls.upgrade), so everything the guest writes is plaintext
 * HTTP. We buffer the request bytes, replay them with fetch() on the first
 * read, and serve the response back as raw HTTP/1.1 bytes.
 *
 * Limits: HTTP only, one request per connection (responses always say
 * `Connection: close`), CORS applies, and fetch() follows redirects itself.
 */

const conns = new Map();
let nextHandle = 1;

// windows-1252 maps every byte to one UTF-16 unit, so string offsets of the
// decoded request are byte offsets into it.
const latin1 = new TextDecoder("latin1");
const utf8 = new TextEncoder();

const tcpErr = (val) => ({ payload: val });
// tls-error wraps tcp-level failures in its `tcp` case.
const tlsErr = (val) => ({ payload: { tag: "tcp", val } });

// Set by the browser (or rejected by fetch) rather than taken from the guest.
const SKIP_REQUEST_HEADERS = new Set([
  "host", "connection", "content-length", "transfer-encoding", "keep-alive",
  "proxy-connection", "upgrade", "te", "trailer", "accept-encoding",
]);
// fetch() has already decoded and de-chunked the body; we re-frame it.
const SKIP_RESPONSE_HEADERS = new Set([
  "content-encoding", "content-length", "transfer-encoding", "connection", "keep-alive",
]);

function open(c) {
  const h = nextHandle++;
  conns.set(h, c);
  return h;
}

function concat(chunks) {
  const out = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.length;
  }
  return out;
}

function parseRequest(buf) {
  const text = latin1.decode(buf);
  const headEnd = text.indexOf("\r\n\r\n");
  const [requestLine, ...lines] = text.slice(0, Math.max(headEnd, 0)).split("\r\n");
  const m = /^([A-Z]+) (\S+) HTTP\/1\.[01]$/.exec(requestLine);
  if (headEnd < 0 || !m) {
    throw { tag: "not-permitted", val: "only HTTP(S) requests are supported in the browser" };
  }
  const headers = new Headers();
  let contentLength = 0;
  for (const line of lines) {
    const i = line.indexOf(":");
    if (i <= 0) continue;
    const name = line.slice(0, i).trim().toLowerCase();
    const value = line.slice(i + 1).trim();
    if (name === "content-length") contentLength = Number(value);
    if (name === "transfer-encoding" && value.toLowerCase() !== "identity") {
      throw { tag: "io-error", val: "chunked request bodies are not supported in the browser" };
    }
    if (!SKIP_REQUEST_HEADERS.has(name)) headers.append(name, value);
  }
  const body = buf.subarray(headEnd + 4, headEnd + 4 + contentLength);
  if (body.length < contentLength) {
    throw { tag: "io-error", val: "connection read before the HTTP request body was fully written" };
  }
  return { method: m[1], target: m[2], headers, body };
}

async function send(c, timeoutMs) {
  const req = parseRequest(concat(c.request));
  const defaultPort = c.scheme === "https" ? 443 : 80;
  const authority = c.port === defaultPort ? c.host : `${c.host}:${c.port}`;
  const url = /^https?:\/\//i.test(req.target) ? req.target : `${c.scheme}://${authority}${req.target}`;

  let resp;
  try {
    resp = await fetch(url, {
      method: req.method,
      headers: req.headers,
      body: req.method === "GET" || req.method === "HEAD" ? undefined : req.body,
      // Never let sandboxed code ride the embedding page's cookies.
      credentials: "omit",
      redirect: "follow",
      signal: timeoutMs ? AbortSignal.timeout(timeoutMs) : undefined,
    });
  } catch (e) {
    if (e?.name === "TimeoutError") throw { tag: "timed-out" };
    throw {
      tag: "io-error",
      val: `fetch ${url} failed: ${e?.message ?? e} (in the browser, the server must allow cross-origin requests)`,
    };
  }

  const body = req.method === "HEAD" ? new Uint8Array(0) : new Uint8Array(await resp.arrayBuffer());
  let head = `HTTP/1.1 ${resp.status} ${resp.statusText}\r\n`;
  for (const [name, value] of resp.headers) {
    if (!SKIP_RESPONSE_HEADERS.has(name)) head += `${name}: ${value}\r\n`;
  }
  if (req.method !== "HEAD") head += `content-length: ${body.length}\r\n`;
  head += "connection: close\r\n\r\n";
  return concat([utf8.encode(head), body]);
}

function write(h, data, wrapErr) {
  const c = conns.get(h);
  if (!c) throw wrapErr({ tag: "invalid-handle" });
  if (c.response) throw wrapErr({ tag: "io-error", val: "only one HTTP request per connection is supported in the browser" });
  c.request.push(data.slice());
  return data.length;
}

async function read(h, len, timeoutMs, wrapErr) {
  const c = conns.get(h);
  if (!c) throw wrapErr({ tag: "invalid-handle" });
  c.response ??= send(c, timeoutMs);
  let bytes;
  try {
    bytes = await c.response;
  } catch (e) {
    throw wrapErr(e);
  }
  const out = bytes.subarray(c.offset, c.offset + len);
  c.offset += out.length;
  return out;
}

const close = (h) => void conns.delete(h);

export const tcp = {
  connect: async (host, port, _timeoutMs) =>
    open({ host, port, scheme: "http", request: [], response: null, offset: 0 }),
  read: (h, len, timeoutMs) => read(h, len, timeoutMs, tcpErr),
  write: async (h, _timeoutMs, data) => write(h, data, tcpErr),
  close,
};

export const tls = {
  // The SNI hostname is ignored: fetch() verifies the certificate against the
  // URL host, and keeping the connect() host stops an upgrade from retargeting
  // an allowed connection at a host the policy never checked.
  async upgrade(tcpHandle, _hostname, _timeoutMs) {
    const c = conns.get(tcpHandle);
    if (!c) throw tlsErr({ tag: "invalid-handle" });
    conns.delete(tcpHandle);
    return open({ ...c, scheme: "https" });
  },
  read: (h, len, timeoutMs) => read(h, len, timeoutMs, tlsErr),
  write: async (h, _timeoutMs, data) => write(h, data, tlsErr),
  close,
};
