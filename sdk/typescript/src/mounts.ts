/**
 * Serving local directories to a run, read-only, over `/v1/stream`.
 *
 * The sandboxed code can ask for any path, and it may be untrusted, so this is
 * the security boundary: paths are checked here, not by eryx.run. The rules
 * are the Python SDK's (`sdk/python/src/eryx_run/_mounts.py`).
 *
 * @module
 */

import { open, readdir, realpath, stat } from "node:fs/promises";
import type { Stats } from "node:fs";
import { join, relative, sep } from "node:path";

import type { Volume } from "./volumes.ts";

/** Files up to this size come with their directory's listing */
const INLINE_MAX = 64 * 1024;
/** Most file data one listing sends; the rest is read when the code asks */
const INLINE_TOTAL = 1024 * 1024;
/** Most one read sends, whatever it asks for */
const READ_MAX = 4 * 1024 * 1024;

/** An operation the client won't do, with the API's error code */
class Refused extends Error {
  readonly code: string;

  constructor(code: string, message = "") {
    super(message);
    this.code = code;
  }
}

function globToRegExp(pattern: string): RegExp {
  const source = pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*").replace(/\?/g, ".");
  return new RegExp(`^${source}$`, "s");
}

function isHidden(rel: string, patterns: readonly RegExp[], nested: readonly boolean[]): boolean {
  const parts = rel.toLowerCase().split("/");
  return patterns.some((pattern, i) =>
    nested[i]
      ? parts.some((_, j) => pattern.test(parts.slice(0, j + 1).join("/")))
      : parts.some((part) => pattern.test(part)),
  );
}

interface Stat {
  is_dir: boolean;
  size?: number;
  modified: number;
}

function toStat(st: Stats): Stat {
  if (st.isDirectory()) return { is_dir: true, modified: st.mtimeMs / 1000 };
  if (!st.isFile()) throw new Refused("permission_denied", "not a regular file");
  return { is_dir: false, size: st.size, modified: st.mtimeMs / 1000 };
}

async function readAt(path: string, offset: number, length: number): Promise<Buffer> {
  const file = await open(path, "r");
  try {
    const buffer = Buffer.alloc(length);
    let filled = 0;
    while (filled < length) {
      const { bytesRead } = await file.read(buffer, filled, length - filled, offset + filled);
      if (bytesRead === 0) break;
      filled += bytesRead;
    }
    return buffer.subarray(0, filled);
  } finally {
    await file.close();
  }
}

/** One local directory, served read-only at a guest path */
class Mount {
  private readonly root: string;
  private readonly patterns: RegExp[];
  private readonly nested: boolean[];

  constructor(root: string, hidden: readonly string[]) {
    this.root = root;
    this.patterns = hidden.map((p) => globToRegExp(p.toLowerCase()));
    this.nested = hidden.map((p) => p.includes("/"));
  }

  /**
   * The host path for `rel` (relative to the mount), refusing what it
   * mustn't serve: `..`, symlinks out of the mount, and hidden files.
   */
  async resolve(rel: string): Promise<string> {
    if (rel && (rel.startsWith("/") || rel.includes("\0") || rel.split("/").some((p) => p === "" || p === "." || p === ".."))) {
      throw new Refused("permission_denied", `bad path ${JSON.stringify(rel)}`);
    }
    if (rel && isHidden(rel, this.patterns, this.nested)) throw new Refused("not_found");
    const real = await realpath(join(this.root, rel));
    const inner = relative(this.root, real);
    if (inner.startsWith("..") || inner.startsWith(sep) || /^[a-zA-Z]:/.test(inner)) throw new Refused("not_found");
    // A symlink inside the mount mustn't lead to a hidden file either
    if (inner && isHidden(inner.split(sep).join("/"), this.patterns, this.nested)) throw new Refused("not_found");
    return real;
  }

  async stat(rel: string): Promise<Stat> {
    return toStat(await stat(await this.resolve(rel)));
  }

  /** The directory's entries, with small files' contents inline */
  async list(rel: string): Promise<[Record<string, unknown>[], Buffer]> {
    const path = await this.resolve(rel);
    const names = (await readdir(path)).sort();
    const entries: Record<string, unknown>[] = [];
    const data: Buffer[] = [];
    let total = 0;
    for (const name of names) {
      let real: string;
      let entry: Record<string, unknown> & Stat;
      try {
        real = await this.resolve(rel ? `${rel}/${name}` : name);
        entry = { name, ...toStat(await stat(real)) };
      } catch {
        continue; // hidden, outside the mount, special or gone
      }
      const room = Math.min(INLINE_MAX, INLINE_TOTAL - total);
      if (!entry.is_dir && entry.size! <= room) {
        const contents = await readAt(real, 0, INLINE_MAX + 1).catch(() => undefined);
        if (contents && contents.length <= room) {
          entry.size = contents.length;
          entry.inline = true;
          data.push(contents);
          total += contents.length;
        }
      }
      entries.push(entry);
    }
    return [entries, Buffer.concat(data)];
  }

  async read(rel: string, offset: number, length: number): Promise<Buffer> {
    if (!(offset >= 0 && length >= 0)) throw new Refused("io", "negative offset or length");
    const path = await this.resolve(rel);
    toStat(await stat(path)); // refuse special files, which could block
    return readAt(path, offset, Math.min(length, READ_MAX));
  }
}

/** The volumes by guest path, checking they can be served */
export async function mounts(volumes: readonly Volume[], hidden: readonly string[]): Promise<Map<string, Mount>> {
  const result = new Map<string, Mount>();
  for (const { hostPath, guestPath } of volumes) {
    let root: string;
    try {
      root = await realpath(hostPath);
      if (!(await stat(root)).isDirectory()) throw new Error();
    } catch {
      throw new TypeError(`volume ${JSON.stringify(hostPath)} isn't a directory`);
    }
    result.set(guestPath, new Mount(root, hidden));
  }
  return result;
}

const FS_ERRORS: Record<string, string> = {
  ENOENT: "not_found",
  ENOTDIR: "not_a_directory",
  EISDIR: "is_a_directory",
  EACCES: "permission_denied",
  EPERM: "permission_denied",
};

/** The `fs_response` for an `fs` message, and the data to send after it */
export async function answer(
  mounts: Map<string, Mount>,
  message: Record<string, unknown>,
): Promise<[Record<string, unknown>, Buffer | undefined]> {
  const reply: Record<string, unknown> = { type: "fs_response", op_id: message.op_id };
  let data: Buffer | undefined;
  try {
    const mount = mounts.get(String(message.mount ?? ""));
    if (!mount) throw new Refused("not_found", "no such mount");
    const rel = String(message.path ?? "");
    switch (message.op) {
      case "stat":
        reply.stat = await mount.stat(rel);
        break;
      case "list":
        [reply.entries, data] = await mount.list(rel);
        break;
      case "read":
        data = await mount.read(rel, Number(message.offset), Number(message.len));
        break;
      default:
        throw new Refused("unsupported", `unknown operation ${JSON.stringify(message.op)}`);
    }
  } catch (e) {
    // A bad message too: the code must get an answer
    if (e instanceof Refused) return [{ ...reply, error: e.code, message: e.message }, undefined];
    const code = FS_ERRORS[(e as NodeJS.ErrnoException).code ?? ""];
    return [code ? { ...reply, error: code } : { ...reply, error: "io", message: String(e) }, undefined];
  }
  reply.data = data?.length ?? 0;
  return [reply, data?.length ? data : undefined];
}
