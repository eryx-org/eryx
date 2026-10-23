/**
 * Network shims for the eryx sandbox (eryx:net/tcp and eryx:net/tls).
 *
 * Networking is disabled until the host calls setNetConfig(). Host policy
 * mirrors the Rust crate's NetConfig: blocked patterns are always enforced,
 * then the host must match allowedHosts unless allowAllHosts is set. In Node,
 * resolved addresses are also checked against blockedNetworks (minus
 * allowedNetworks) unless the host matches internalHosts.
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

/** Same defaults as the Rust crate's `NetConfig::default().blocked_hosts`. */
export const DEFAULT_BLOCKED_HOSTS = [
  "localhost",
  "*.localhost",
  "127.*",
  "10.*",
  ...Array.from({ length: 16 }, (_, i) => `172.${16 + i}.*`),
  "192.168.*",
  "169.254.*",
  "[::1]",
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

/** @type {Required<import("./net.js").NetConfig> | null} */
let config = null;

/**
 * Enable (or, with `null`, disable) networking for sandboxed code.
 *
 * @param {import("./net.js").NetConfig | null} netConfig
 */
export function setNetConfig(netConfig) {
  config = netConfig && {
    allowAllHosts: netConfig.allowAllHosts ?? false,
    allowedHosts: netConfig.allowedHosts ?? [],
    blockedHosts: netConfig.blockedHosts ?? DEFAULT_BLOCKED_HOSTS,
    blockedNetworks: netConfig.blockedNetworks ?? DEFAULT_BLOCKED_NETWORKS,
    allowedNetworks: netConfig.allowedNetworks ?? [],
    internalHosts: netConfig.internalHosts ?? [],
  };
}

/** Glob match with `*` wildcards, case-insensitive (like the Rust matcher). */
function hostMatches(host, pattern) {
  const re = pattern
    .split("*")
    .map((part) => part.replace(/[.+?^${}()|[\]\\]/g, "\\$&"))
    .join(".*");
  return new RegExp(`^${re}$`, "i").test(host);
}

function checkHost(host) {
  const notPermitted = (val) => ({ payload: { tag: "not-permitted", val } });
  if (!config) throw notPermitted("networking is disabled; enable it with setNetConfig()");
  if (config.blockedHosts.some((p) => hostMatches(host, p))) {
    throw notPermitted(`host '${host}' is blocked`);
  }
  if (!config.allowAllHosts && !config.allowedHosts.some((p) => hostMatches(host, p))) {
    throw notPermitted(`host '${host}' not in allowed list`);
  }
}

export const tcp = {
  ...transport.tcp,
  async connect(host, port, timeoutMs) {
    checkHost(host);
    // The transport checks resolved addresses against this; internal hosts skip it.
    const networks = config.internalHosts.some((p) => hostMatches(host, p)) ? null : config;
    return transport.tcp.connect(host, port, timeoutMs, networks);
  },
};

export const tls = transport.tls;
