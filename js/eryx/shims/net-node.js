/**
 * Node.js transport for the eryx:net shims: real TCP/TLS via node:net and
 * node:tls. Policy checks happen in net.js before this is reached.
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

export const tcp = {
  async connect(host, port, _timeoutMs) {
    const net = await import(/* webpackIgnore: true */ "node:net");
    return new Promise((resolve, reject) => {
      const s = net.connect({ host, port });
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
