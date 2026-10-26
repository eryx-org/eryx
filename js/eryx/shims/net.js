/**
 * Network shims for the eryx sandbox (eryx:net/tcp and eryx:net/tls).
 *
 * Networking is disabled until the host calls setNetConfig(). Host policy
 * mirrors the Rust crate's NetConfig: blocked patterns are always enforced,
 * then the host must match allowedHosts unless allowAllHosts is set. Hosts are
 * matched in their WHATWG URL form too, so `2130706433` and `0x7f.1` read as
 * `127.0.0.1`. IP addresses are checked against blockedNetworks (minus
 * allowedNetworks); in Node, so are the addresses a name resolves to, unless
 * the host matches internalHosts. Browsers don't expose DNS answers, so there
 * only an allowlist keeps names that resolve to private addresses out.
 *
 * The transport depends on the environment:
 * - Node.js: real TCP/TLS sockets (net-node.js)
 * - Browsers: HTTP(S) only, replayed through fetch() (net-fetch.js)
 *
 * The guest calls these imports synchronously; they return Promises and are
 * transpiled as JSPI async imports, which suspend the wasm stack until they
 * settle. jco lowers `result<T, E>` imports as: return T, or throw
 * `{ payload: E }`.
 */

const isNode =
  typeof globalThis.process !== "undefined" &&
  typeof globalThis.process.versions?.node !== "undefined";

const transport = isNode ? await import("./net-node.js") : await import("./net-fetch.js");

/** The Rust crate's `NetConfig::default().blocked_hosts`, plus `0.0.0.0` and `[::]`. */
export const DEFAULT_BLOCKED_HOSTS = [
  "localhost",
  "*.localhost",
  "127.*",
  "10.*",
  ...Array.from({ length: 16 }, (_, i) => `172.${16 + i}.*`),
  "192.168.*",
  "169.254.*",
  "[::1]",
  "0.0.0.0",
  "[::]",
];

/** Same defaults as the Rust crate's `DEFAULT_BLOCKED_NETWORKS`. */
export const DEFAULT_BLOCKED_NETWORKS = [
  "0.0.0.0/8",
  "10.0.0.0/8",
  "100.64.0.0/10",
  "127.0.0.0/8",
  "169.254.0.0/16",
  "172.16.0.0/12",
  "192.168.0.0/16",
  "224.0.0.0/4",
  "240.0.0.0/4",
  "::/128",
  "::1/128",
  "fc00::/7",
  "fe80::/10",
  "ff00::/8",
];

/** Same defaults as the Rust crate's `NetConfig::default()`. */
const DEFAULT_MAX_CONNECTIONS = 10;
const DEFAULT_CONNECT_TIMEOUT_MS = 30_000;
const DEFAULT_IO_TIMEOUT_MS = 60_000;

/**
 * The active policy; `isAllowed` checks resolved addresses (null where the
 * transport can't see them).
 *
 * @type {(Required<import("./net.js").NetConfig> & { isAllowed: ((address: string, family: number) => boolean) | null }) | null}
 */
let config = null;

/** Strict CIDR syntax, so a typo can't parse as a wider network (`10.0.0.0/` as `/0`). */
function checkCidrs(cidrs) {
  for (const cidr of cidrs) {
    const m = /^([0-9a-fA-F:.]+)\/(\d{1,3})$/.exec(cidr);
    if (!m || Number(m[2]) > (m[1].includes(":") ? 128 : 32)) {
      throw new TypeError(`invalid CIDR '${cidr}'`);
    }
  }
  return cidrs;
}

/** A non-negative integer, or a TypeError. */
function checkCount(name, value) {
  if (!Number.isInteger(value) || value < 0) {
    throw new TypeError(`${name} must be a non-negative integer, got ${value}`);
  }
  return value;
}

/**
 * Enable (or, with `null`, disable) networking for sandboxed code.
 *
 * Throws a TypeError if a network is not a valid CIDR or maxConnections is
 * not a non-negative integer.
 *
 * @param {import("./net.js").NetConfig | null} netConfig
 */
export function setNetConfig(netConfig) {
  if (!netConfig) {
    config = null;
    return;
  }
  const next = {
    allowAllHosts: netConfig.allowAllHosts ?? false,
    allowedHosts: netConfig.allowedHosts ?? [],
    blockedHosts: netConfig.blockedHosts ?? DEFAULT_BLOCKED_HOSTS,
    blockedNetworks: checkCidrs(netConfig.blockedNetworks ?? DEFAULT_BLOCKED_NETWORKS),
    allowedNetworks: checkCidrs(netConfig.allowedNetworks ?? []),
    internalHosts: netConfig.internalHosts ?? [],
    maxConnections: checkCount("maxConnections", netConfig.maxConnections ?? DEFAULT_MAX_CONNECTIONS),
    connectTimeoutMs: netConfig.connectTimeoutMs ?? DEFAULT_CONNECT_TIMEOUT_MS,
    ioTimeoutMs: netConfig.ioTimeoutMs ?? DEFAULT_IO_TIMEOUT_MS,
  };
  config = { ...next, isAllowed: transport.networkFilter(next) };
}

/**
 * The guest's timeout (`0` for none) clamped to the host's, so sandboxed code
 * can shorten a timeout but never extend it.
 */
// setTimeout fires at once for delays past 2^31 - 1 ms.
const clamp = (hostMs, guestMs) => Math.min(guestMs ? Math.min(guestMs, hostMs) : hostMs, 2 ** 31 - 1);
const connectTimeout = (ms) => clamp(config?.connectTimeoutMs ?? DEFAULT_CONNECT_TIMEOUT_MS, ms);
const ioTimeout = (ms) => clamp(config?.ioTimeoutMs ?? DEFAULT_IO_TIMEOUT_MS, ms);

/**
 * `host` in WHATWG URL form (lower-cased, IPv4 in dotted decimal, IPv6
 * bracketed and compressed) without a trailing dot, or null if it isn't a
 * bare host name or address.
 */
function canonicalHost(host) {
  // URL parsing would read these as userinfo, path or port delimiters.
  if (!host || /[\s/?#@\\]/.test(host)) return null;
  try {
    return new URL(`http://${host.includes(":") ? `[${host}]` : host}`).hostname.replace(/\.$/, "");
  } catch {
    return null;
  }
}

/** Glob match with `*` wildcards, case-insensitive (like the Rust matcher). */
function hostMatches(host, pattern) {
  const re = pattern
    .split("*")
    .map((part) => part.replace(/[.+?^${}()|[\]\\]/g, "\\$&"))
    .join(".*");
  return new RegExp(`^${re}$`, "i").test(host);
}

const notPermitted = (val) => ({ payload: { tag: "not-permitted", val } });

/** Check `host` against the policy and return its canonical form. */
function checkHost(host) {
  if (!config) throw notPermitted("networking is disabled; enable it with setNetConfig()");
  const canonical = canonicalHost(host);
  if (canonical === null) throw notPermitted(`host '${host}' is not a valid host name`);
  // Refusals match either spelling; allowances only the canonical one.
  const matches = (patterns) => patterns.some((p) => hostMatches(canonical, p));
  if (config.blockedHosts.some((p) => hostMatches(host, p)) || matches(config.blockedHosts)) {
    throw notPermitted(`host '${host}' is blocked`);
  }
  if (!config.allowAllHosts && !matches(config.allowedHosts)) {
    throw notPermitted(`host '${host}' not in allowed list`);
  }
  return { canonical, internal: matches(config.internalHosts) };
}

/**
 * The eryx:net imports for one sandbox instance, with its own connection
 * table. The policy set with setNetConfig() applies to all of them.
 *
 * `closeConnections()` closes every connection the instance's code left open;
 * the sandbox calls it after each execution.
 */
export function createNet() {
  const t = transport.createTransport();
  return {
    tcp: {
      async connect(host, port, timeoutMs) {
        const { canonical, internal } = checkHost(host);
        // 0 means no limit, as in the Rust crate.
        if (config.maxConnections > 0 && t.openConnections() >= config.maxConnections) {
          throw notPermitted(`connection limit reached (${config.maxConnections})`);
        }
        // The transport checks the address (Node: the resolved addresses)
        // with this; internal hosts skip it.
        const isAllowed = internal ? null : config.isAllowed;
        // Connect to the form that was checked, unbracketed for the transport.
        const target = canonical.replace(/^\[(.*)\]$/, "$1");
        return t.tcp.connect(target, port, connectTimeout(timeoutMs), isAllowed);
      },
      read: (h, len, timeoutMs) => t.tcp.read(h, len, ioTimeout(timeoutMs)),
      write: (h, timeoutMs, data) => t.tcp.write(h, ioTimeout(timeoutMs), data),
      close: t.tcp.close,
    },
    tls: {
      upgrade: (h, hostname, timeoutMs) => t.tls.upgrade(h, hostname, connectTimeout(timeoutMs)),
      read: (h, len, timeoutMs) => t.tls.read(h, len, ioTimeout(timeoutMs)),
      write: (h, timeoutMs, data) => t.tls.write(h, ioTimeout(timeoutMs), data),
      close: t.tls.close,
    },
    closeConnections: t.closeAll,
  };
}

/** The default instance's imports, for the module-level sandbox. */
export const { tcp, tls, closeConnections } = createNet();
