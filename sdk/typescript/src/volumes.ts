/**
 * Volumes: local directories served to runs, read-only. What needs no
 * filesystem is here, so `node:fs` is only loaded for runs that use them.
 *
 * @module
 */

/**
 * Files and directories a volume doesn't serve unless `hidden` says
 * otherwise. A pattern without `/` matches any path component; one with `/`
 * matches a path from the volume's root, or a directory above it. `*` and `?`
 * are wildcards. Matching ignores case, as the filesystem may (`.ENV` opens
 * `.env` on macOS).
 */
export const DEFAULT_HIDDEN: readonly string[] = [
  ".env",
  ".env.*",
  "*.pem",
  "*.key",
  "*.p12",
  "*.pfx",
  "*.tfstate",
  "*.tfstate.*",
  "*.tfvars",
  ".pgpass",
  ".htpasswd",
  ".kube",
  "id_rsa*",
  "id_ecdsa*",
  "id_ed25519*",
  ".netrc",
  ".npmrc",
  ".pypirc",
  ".aws",
  ".ssh",
  ".gnupg",
  ".docker",
  ".git/config",
  ".git-credentials",
];

/** A local directory to serve to runs, read-only. */
export interface Volume {
  /** The directory on this machine. */
  hostPath: string;
  /** Where the code sees it: a path under `/mnt/`. */
  guestPath: string;
}

/** Check volumes' guest paths, which needs no filesystem */
export function checkVolumes(volumes: readonly Volume[]): void {
  const seen = new Set<string>();
  for (const { guestPath } of volumes) {
    if (!guestPath.startsWith("/mnt/")) throw new TypeError(`volume guest path ${JSON.stringify(guestPath)} must be under /mnt/`);
    if (seen.has(guestPath)) throw new TypeError(`two volumes at ${JSON.stringify(guestPath)}`);
    seen.add(guestPath);
  }
}
