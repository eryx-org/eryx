/**
 * Network policy for sandboxed code. Mirrors the Rust crate's `NetConfig`.
 *
 * In Node.js, sandboxed code gets real TCP/TLS sockets. In browsers only
 * HTTP(S) works: requests are replayed through `fetch()`, so CORS applies, and
 * the browser resolves names itself, so a name that resolves to a private
 * address can't be refused there. Use `allowedHosts` rather than
 * `allowAllHosts` in browsers.
 *
 * Connections still open when an execution finishes are closed.
 */
export interface NetConfig {
  /** Allow every host not matched by `blockedHosts` (default false). */
  allowAllHosts?: boolean;
  /** Host patterns to allow; `*` is a wildcard (e.g. `*.example.com`). */
  allowedHosts?: string[];
  /**
   * Host patterns always refused. Defaults to {@link DEFAULT_BLOCKED_HOSTS}.
   * Hosts are matched as written and in WHATWG URL form, so `2130706433` and
   * `0x7f.1` match `127.*`.
   */
  blockedHosts?: string[];
  /**
   * CIDRs (e.g. `"10.0.0.0/8"`) that an IP address host, or (in Node.js) the
   * addresses a host name resolves to, must not fall in, so a public name
   * pointing at an internal address is refused. Defaults to
   * {@link DEFAULT_BLOCKED_NETWORKS}. Browsers don't expose resolved
   * addresses, so there only IP address hosts are checked.
   */
  blockedNetworks?: string[];
  /** CIDRs exempt from `blockedNetworks`. */
  allowedNetworks?: string[];
  /**
   * Host patterns allowed to resolve into `blockedNetworks`, for trusted names
   * routed internally on purpose. Only list names whose DNS you control.
   */
  internalHosts?: string[];
  /** Most connections open at once (default 10). */
  maxConnections?: number;
  /**
   * Ceiling for connect and TLS handshake timeouts, in milliseconds (default
   * 30000). Sandboxed code can set a shorter timeout, never a longer one.
   */
  connectTimeoutMs?: number;
  /** Ceiling for read and write timeouts, in milliseconds (default 60000). */
  ioTimeoutMs?: number;
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

/** Close every connection sandboxed code left open. Executions do this when they finish. */
export function closeConnections(): void;

/** A transport's TCP and TLS imports. Internal: the shapes follow the eryx:net WIT. */
type NetImports = Record<string, (...args: any[]) => unknown>;

/**
 * The eryx:net imports for one sandbox instance, with its own connection
 * table. The policy set with {@link setNetConfig} applies to every instance.
 */
export function createNet(): { tcp: NetImports; tls: NetImports; closeConnections(): void };
