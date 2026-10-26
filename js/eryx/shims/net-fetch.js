/**
 * Browser transport for the eryx:net shims: HTTP(S) over fetch().
 *
 * Browsers can't open raw sockets, but TLS is terminated host-side (Python's
 * ssl module calls tls.upgrade), so everything the guest writes is plaintext
 * HTTP. We buffer the request bytes, replay them with fetch() on the first
 * read, and serve the response back as raw HTTP/1.1 bytes.
 *
 * Limits: HTTP only, one request per connection (responses always say
 * `Connection: close`), and CORS applies. Requests always go to the host the
 * guest connected to, and redirects aren't followed: the guest's HTTP client
 * sees the 3xx and connects again, through the policy. Browsers hide a
 * redirect's target, so there a redirect fails instead.
 */

// windows-1252 maps every byte to one UTF-16 unit, so string offsets of the
// decoded request are byte offsets into it.
import { embeddedIPv4, ipv4Bytes, ipv6Bytes } from "./ip.js";

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

/**
 * `(address, family) => allowed` for `blockedNetworks` minus
 * `allowedNetworks`, matching like the Node transport's BlockList. Throws a
 * TypeError on an invalid network address.
 */
export function networkFilter({ blockedNetworks, allowedNetworks }) {
  const parse = (cidrs) =>
    cidrs.map((cidr) => {
      const [addr, prefix] = cidr.split("/");
      const bytes = addr.includes(":") ? ipv6Bytes(addr) : ipv4Bytes(addr);
      if (!bytes) throw new TypeError(`invalid CIDR '${cidr}'`);
      return { bytes, prefix: Number(prefix) };
    });
  const inNet = (b, { bytes, prefix }) => {
    if (b.length !== bytes.length) return false;
    for (let bit = 0; bit < prefix; bit++) {
      const mask = 0x80 >> bit % 8;
      if ((b[bit >> 3] & mask) !== (bytes[bit >> 3] & mask)) return false;
    }
    return true;
  };
  const blocked = parse(blockedNetworks);
  const allowed = parse(allowedNetworks);
  const matches = (nets, b) => {
    const v4 = b.length === 16 ? embeddedIPv4(b) : null;
    return nets.some((n) => inNet(b, n) || (v4 !== null && inNet(v4, n)));
  };
  return (address, family) => {
    const b = family === 6 ? ipv6Bytes(address) : ipv4Bytes(address);
    return b !== null && (!matches(blocked, b) || matches(allowed, b));
  };
}

/** 4 or 6 if `host` is an IP address (in the canonical form net.js passes), else 0. */
const ipFamily = (host) => (host.includes(":") ? 6 : ipv4Bytes(host) ? 4 : 0);

function open(conns, c) {
  const h = conns.next++;
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
  const host = c.host.includes(":") ? `[${c.host}]` : c.host;
  const origin = `${c.scheme}://${c.port === defaultPort ? host : `${host}:${c.port}`}`;
  // The URL always comes from the connection. An absolute-form target (as
  // sent to a proxy) is only accepted for the connected origin.
  let path = req.target;
  if (!path.startsWith("/")) {
    let target = null;
    try {
      target = new URL(path);
    } catch {}
    if (target?.origin !== origin) {
      throw { tag: "not-permitted", val: `request target '${path}' is not on the connected host ${origin}` };
    }
    path = target.pathname + target.search;
  }
  const url = origin + path;

  let resp;
  try {
    resp = await fetch(url, {
      method: req.method,
      headers: req.headers,
      body: req.method === "GET" || req.method === "HEAD" ? undefined : req.body,
      // Never let sandboxed code ride the embedding page's cookies.
      credentials: "omit",
      // Following a redirect would reach hosts the policy never checked.
      redirect: "manual",
      signal: timeoutMs ? AbortSignal.any([c.abort.signal, AbortSignal.timeout(timeoutMs)]) : c.abort.signal,
    });
  } catch (e) {
    if (e?.name === "TimeoutError") throw { tag: "timed-out" };
    throw {
      tag: "io-error",
      val: `fetch ${url} failed: ${e?.message ?? e} (in the browser, the server must allow cross-origin requests)`,
    };
  }

  if (resp.type === "opaqueredirect") {
    throw { tag: "io-error", val: `${url} redirected; browsers hide the redirect target, so it can't be followed` };
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

function write(conns, h, data, wrapErr) {
  const c = conns.get(h);
  if (!c) throw wrapErr({ tag: "invalid-handle" });
  if (c.response) throw wrapErr({ tag: "io-error", val: "only one HTTP request per connection is supported in the browser" });
  c.request.push(data.slice());
  return data.length;
}

async function read(conns, h, len, timeoutMs, wrapErr) {
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

/** `isAllowed` is a networkFilter() result, or null to skip the address check. */
function connect(conns, host, port, isAllowed) {
  // Browsers resolve names themselves, so only IP addresses can be checked.
  const family = ipFamily(host);
  if (family && isAllowed && !isAllowed(host, family)) {
    throw tcpErr({ tag: "not-permitted", val: `host '${host}' resolves to a blocked network` });
  }
  const abort = new AbortController();
  return open(conns, { host, port, scheme: "http", request: [], response: null, offset: 0, abort });
}

// The SNI hostname is ignored: fetch() verifies the certificate against the
// URL host, and keeping the connect() host stops an upgrade from retargeting
// an allowed connection at a host the policy never checked.
function upgrade(conns, tcpHandle) {
  const c = conns.get(tcpHandle);
  if (!c) throw tlsErr({ tag: "invalid-handle" });
  conns.delete(tcpHandle);
  return open(conns, { ...c, scheme: "https" });
}

/**
 * A transport with its own connection table, so each sandbox instance only
 * sees (and closes) its own handles.
 */
export function createTransport() {
  const conns = new Map();
  conns.next = 1;
  const close = (h) => {
    conns.get(h)?.abort.abort();
    conns.delete(h);
  };
  return {
    openConnections: () => conns.size,
    closeAll() {
      for (const h of [...conns.keys()]) close(h);
    },
    tcp: {
      connect: async (host, port, _timeoutMs, isAllowed) => connect(conns, host, port, isAllowed),
      read: (h, len, timeoutMs) => read(conns, h, len, timeoutMs, tcpErr),
      write: async (h, _timeoutMs, data) => write(conns, h, data, tcpErr),
      close,
    },
    tls: {
      upgrade: async (h, _hostname, _timeoutMs) => upgrade(conns, h),
      read: (h, len, timeoutMs) => read(conns, h, len, timeoutMs, tlsErr),
      write: async (h, _timeoutMs, data) => write(conns, h, data, tlsErr),
      close,
    },
  };
}
