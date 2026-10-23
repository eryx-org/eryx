/**
 * Node.js transport for the eryx:net shims: real TCP/TLS via node:net and
 * node:tls. Host policy is checked in net.js; the resolved-address check
 * happens here, since only the transport sees the address it connects to.
 */

const conns = new Map();
let nextHandle = 1;

const tcpErr = (val) => ({ payload: val });
// tls-error wraps tcp-level failures in its `tcp` case.
const tlsErr = (val) => ({ payload: { tag: "tcp", val } });

function track(socket) {
  const c = { socket, chunks: [], ended: false, error: null, wake: null };
  const notify = () => {
    const w = c.wake;
    c.wake = null;
    w?.();
  };
  c.onData = (b) => { c.chunks.push(new Uint8Array(b)); notify(); };
  c.onEnd = () => { c.ended = true; notify(); };
  c.onError = (e) => { c.error = e; notify(); };
  socket.on("data", c.onData).on("end", c.onEnd).on("error", c.onError);
  const h = nextHandle++;
  conns.set(h, c);
  return h;
}

function untrack(h) {
  const c = conns.get(h);
  conns.delete(h);
  if (c) {
    c.socket.off("data", c.onData).off("end", c.onEnd).off("error", c.onError);
  }
  return c;
}

function mapTcpError(e) {
  switch (e?.code) {
    case "ECONNREFUSED": return { tag: "connection-refused" };
    case "ECONNRESET": return { tag: "connection-reset" };
    case "ETIMEDOUT": return { tag: "timed-out" };
    case "ENOTFOUND":
    case "EAI_AGAIN": return { tag: "host-not-found" };
    case "EERYX_BLOCKED": return { tag: "not-permitted", val: e.message };
    default: return { tag: "io-error", val: String(e?.message ?? e) };
  }
}

// ponytail: guest timeout-ms ignored; honour it (clamped to a host ceiling) before shipping Node support.
async function read(h, len, wrapErr) {
  const c = conns.get(h);
  if (!c) throw wrapErr({ tag: "invalid-handle" });
  while (!c.chunks.length && !c.ended && !c.error) {
    await new Promise((r) => { c.wake = r; });
  }
  if (c.chunks.length) {
    const chunk = c.chunks.shift();
    if (chunk.length > len) c.chunks.unshift(chunk.subarray(len));
    return chunk.subarray(0, len);
  }
  if (c.error) throw wrapErr(mapTcpError(c.error));
  return new Uint8Array(0);
}

async function write(h, data, wrapErr) {
  const c = conns.get(h);
  if (!c) throw wrapErr({ tag: "invalid-handle" });
  await new Promise((resolve, reject) =>
    c.socket.write(data, (e) => (e ? reject(wrapErr(mapTcpError(e))) : resolve())),
  );
  return data.length;
}

function close(h) {
  untrack(h)?.socket.destroy();
}

function blockedError(host, address) {
  const e = new Error(`host '${host}' resolves to blocked address ${address}`);
  e.code = "EERYX_BLOCKED";
  return e;
}

/**
 * `(address, family) => allowed` for `blockedNetworks` minus `allowedNetworks`.
 * Throws on an invalid CIDR, which connect() turns into a refusal.
 */
function networkFilter(net, { blockedNetworks, allowedNetworks }) {
  const list = (cidrs) => {
    const bl = new net.BlockList();
    for (const cidr of cidrs) {
      const [addr, prefix] = cidr.split("/");
      bl.addSubnet(addr, Number(prefix), net.isIPv6(addr) ? "ipv6" : "ipv4");
    }
    return bl;
  };
  const blocked = list(blockedNetworks);
  const allowed = list(allowedNetworks);
  return (address, family) => {
    const type = family === 6 ? "ipv6" : "ipv4";
    // BlockList matches IPv4-mapped IPv6 addresses against IPv4 subnets.
    return !blocked.check(address, type) || allowed.check(address, type);
  };
}

export const tcp = {
  async connect(host, port, _timeoutMs, networks) {
    const net = await import(/* webpackIgnore: true */ "node:net");
    const dns = await import(/* webpackIgnore: true */ "node:dns");
    let isAllowed = () => true;
    if (networks) {
      try {
        isAllowed = networkFilter(net, networks);
      } catch (e) {
        throw tcpErr({ tag: "not-permitted", val: `invalid network policy: ${e.message}` });
      }
    }
    // Node skips `lookup` for IP literals, so check those here.
    const literal = net.isIP(host);
    if (literal && !isAllowed(host, literal)) {
      throw tcpErr(mapTcpError(blockedError(host, host)));
    }
    // Filter the DNS answer before Node picks an address, so the address
    // connected to is one that was checked.
    const lookup = (hostname, options, cb) =>
      dns.lookup(hostname, { ...options, all: true }, (err, addrs) => {
        if (err) return cb(err);
        const ok = addrs.filter((a) => isAllowed(a.address, a.family));
        if (!ok.length) return cb(blockedError(hostname, addrs[0].address));
        if (options.all) return cb(null, ok);
        cb(null, ok[0].address, ok[0].family);
      });
    return new Promise((resolve, reject) => {
      const s = net.connect({ host, port, lookup });
      s.once("connect", () => { s.off("error", reject); resolve(track(s)); });
      s.once("error", (e) => reject(tcpErr(mapTcpError(e))));
    });
  },
  read: (h, len, _timeoutMs) => read(h, len, tcpErr),
  write: (h, _timeoutMs, data) => write(h, data, tcpErr),
  close,
};

export const tls = {
  async upgrade(tcpHandle, hostname, _timeoutMs) {
    const c = untrack(tcpHandle);
    if (!c) throw tlsErr({ tag: "invalid-handle" });
    const tlsMod = await import(/* webpackIgnore: true */ "node:tls");
    return new Promise((resolve, reject) => {
      const s = tlsMod.connect({ socket: c.socket, servername: hostname });
      s.once("secureConnect", () => resolve(track(s)));
      s.once("error", (e) => reject({ payload: { tag: "handshake-failed", val: String(e.message) } }));
    });
  },
  read: (h, len, _timeoutMs) => read(h, len, tlsErr),
  write: (h, _timeoutMs, data) => write(h, data, tlsErr),
  close,
};
