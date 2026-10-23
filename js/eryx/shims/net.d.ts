/**
 * Network policy for sandboxed code. Mirrors the Rust crate's `NetConfig`.
 *
 * In Node.js, sandboxed code gets real TCP/TLS sockets. In browsers only
 * HTTP(S) works: requests are replayed through `fetch()`, so CORS applies.
 */
export interface NetConfig {
  /** Allow every host not matched by `blockedHosts` (default false). */
  allowAllHosts?: boolean;
  /** Host patterns to allow; `*` is a wildcard (e.g. `*.example.com`). */
  allowedHosts?: string[];
  /** Host patterns always refused. Defaults to {@link DEFAULT_BLOCKED_HOSTS}. */
  blockedHosts?: string[];
  /**
   * CIDRs (e.g. `"10.0.0.0/8"`) that a host's resolved addresses must not fall
   * in, so a public name pointing at an internal address is refused.
   * Defaults to {@link DEFAULT_BLOCKED_NETWORKS}. Node.js only: browsers
   * don't expose the resolved address.
   */
  blockedNetworks?: string[];
  /** CIDRs exempt from `blockedNetworks`. */
  allowedNetworks?: string[];
  /**
   * Host patterns allowed to resolve into `blockedNetworks`, for trusted names
   * routed internally on purpose. Only list names whose DNS you control.
   */
  internalHosts?: string[];
}

/** Localhost and private network ranges, blocked unless `blockedHosts` is overridden. */
export const DEFAULT_BLOCKED_HOSTS: string[];

/** Every non-public address range, blocked unless `blockedNetworks` is overridden. */
export const DEFAULT_BLOCKED_NETWORKS: string[];

/**
 * Enable networking for sandboxed code, or disable it again with `null`.
 * Networking is disabled by default.
 */
export function setNetConfig(config: NetConfig | null): void;
