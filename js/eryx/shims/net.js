/**
 * Network shims for the eryx sandbox (eryx:net/tcp and eryx:net/tls).
 *
 * Networking is disabled until the host sets a policy, per sandbox or as the
 * default with setNetConfig(). Host policy mirrors the Rust crate's NetConfig:
 * blocked patterns are always enforced, then the host must match allowedHosts
 * unless allowAllHosts is set. Hosts are
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
export const DEFAULT_BLOCKED_HOSTS = Object.freeze([
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
]);

/**
 * The Rust crate's `DEFAULT_BLOCKED_NETWORKS`. IPv4-mapped, IPv4-compatible,
 * NAT64 and Teredo addresses are also checked by the IPv4 address they embed.
 */
export const DEFAULT_BLOCKED_NETWORKS = Object.freeze([
  "0.0.0.0/8",
  "10.0.0.0/8",
  "100.64.0.0/10",
  "127.0.0.0/8",
  "169.254.0.0/16",
  "172.16.0.0/12",
  "192.0.0.0/24",
  "192.168.0.0/16",
  "198.18.0.0/15",
  "224.0.0.0/4",
  "240.0.0.0/4",
  "::/96",
  "2001::/32",
  "2001:db8::/32",
  "2002::/16",
  "64:ff9b:1::/48",
  "fc00::/7",
  "fe80::/10",
  "fec0::/10",
  "ff00::/8",
]);

/** Same defaults as the Rust crate's `NetConfig::default()`. */
const DEFAULT_MAX_CONNECTIONS = 10;
const DEFAULT_CONNECT_TIMEOUT_MS = 30_000;
const DEFAULT_IO_TIMEOUT_MS = 60_000;

/**
 * A checked policy; `isAllowed` checks resolved addresses (null where the
 * transport can't see them).
 *
 * @typedef {Required<import("./net.js").NetConfig> & { isAllowed: ((address: string, family: number) => boolean) | null }} Policy
 */

/**
 * The policy new instances start with, set by setNetConfig().
 * @type {Policy | null}
 */
let defaultPolicy = null;

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
 * Check `netConfig` and fill in the defaults; null disables networking.
 *
 * Throws a TypeError if a network is not a valid CIDR or maxConnections is
 * not a non-negative integer.
 *
 * @param {import("./net.js").NetConfig | null} netConfig
 * @returns {Policy | null}
 */
function toPolicy(netConfig) {
  if (!netConfig) return null;
  // Copies, so changing the caller's arrays later can't change the policy.
  const list = (xs, fallback) => Object.freeze([...(xs ?? fallback)]);
  const next = {
    allowAllHosts: netConfig.allowAllHosts ?? false,
    allowedHosts: list(netConfig.allowedHosts, []),
    blockedHosts: list(netConfig.blockedHosts, DEFAULT_BLOCKED_HOSTS),
    blockedNetworks: checkCidrs(list(netConfig.blockedNetworks, DEFAULT_BLOCKED_NETWORKS)),
    allowedNetworks: checkCidrs(list(netConfig.allowedNetworks, [])),
    internalHosts: list(netConfig.internalHosts, []),
    maxConnections: checkCount("maxConnections", netConfig.maxConnections ?? DEFAULT_MAX_CONNECTIONS),
    connectTimeoutMs: netConfig.connectTimeoutMs ?? DEFAULT_CONNECT_TIMEOUT_MS,
    ioTimeoutMs: netConfig.ioTimeoutMs ?? DEFAULT_IO_TIMEOUT_MS,
  };
  return Object.freeze({ ...next, isAllowed: transport.networkFilter(next) });
}

/**
 * Set the default policy (`null` disables networking), which each sandbox
 * copies when it is created; sandboxes that already exist keep theirs.
 *
 * @param {import("./net.js").NetConfig | null} netConfig
 */
export function setNetConfig(netConfig) {
  defaultPolicy = toPolicy(netConfig);
  defaultNet.setNetConfig(netConfig);
}

/**
 * The guest's timeout (`0` for none) clamped to the host's, so sandboxed code
 * can shorten a timeout but never extend it.
 */
// setTimeout fires at once for delays past 2^31 - 1 ms.
const clamp = (hostMs, guestMs) => Math.min(guestMs ? Math.min(guestMs, hostMs) : hostMs, 2 ** 31 - 1);

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

/** Check `host` against `config` and return its canonical form. */
function checkHost(config, host) {
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
 * table and policy. The policy starts as the default set with setNetConfig();
 * `setNetConfig()` on the result replaces it for this instance only, for
 * connections opened after the call.
 *
 * `closeConnections()` closes every connection the instance's code left open;
 * the sandbox calls it after each call into the instance.
 */
export function createNet() {
  const t = transport.createTransport();
  let config = defaultPolicy;
  const connectTimeout = (ms) => clamp(config?.connectTimeoutMs ?? DEFAULT_CONNECT_TIMEOUT_MS, ms);
  const ioTimeout = (ms) => clamp(config?.ioTimeoutMs ?? DEFAULT_IO_TIMEOUT_MS, ms);
  return {
    tcp: {
      async connect(host, port, timeoutMs) {
        const { canonical, internal } = checkHost(config, host);
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
    /** @param {import("./net.js").NetConfig | null} netConfig */
    setNetConfig(netConfig) {
      config = toPolicy(netConfig);
    },
  };
}

/**
 * The default instance, for code that imports the shims directly. It follows
 * setNetConfig().
 */
const defaultNet = createNet();
export const { tcp, tls, closeConnections } = defaultNet;
