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
}

/** Localhost and private network ranges, blocked unless `blockedHosts` is overridden. */
export const DEFAULT_BLOCKED_HOSTS: string[];

/**
 * Enable networking for sandboxed code, or disable it again with `null`.
 * Networking is disabled by default.
 */
export function setNetConfig(config: NetConfig | null): void;
