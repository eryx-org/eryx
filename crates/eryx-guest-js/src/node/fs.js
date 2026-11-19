// node:fs (and node:fs/promises, its `promises`): written for eryx, a
// subset on the guest's own files (src/fs.rs). Whole-file reads and writes,
// directories, stat, rename, copy and remove, each as a sync function, a
// promise (`fs.promises`) and a Node-style callback. No file descriptors
// beyond the standard streams (`readFileSync(0)` is stdin), no streams and
// no watching.
import { Buffer } from 'node:buffer';

const native = globalThis.__eryx_fs;

const pathOf = (path) => {
    if (typeof path === 'number' || typeof path === 'string') return path;
    if (path instanceof URL) {
        if (path.protocol !== 'file:') throw new TypeError('The URL must be of scheme file');
        return decodeURIComponent(path.pathname);
    }
    if (path instanceof Uint8Array) return Buffer.from(path).toString();
    throw new TypeError('The "path" argument must be of type string, Buffer or URL');
};

const encodingOf = (options) => (typeof options === 'string' ? options : options?.encoding) ?? null;

const bytesOf = (data, encoding) => {
    if (typeof data === 'string') return Buffer.from(data, encoding ?? 'utf8');
    if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    throw new TypeError('The "data" argument must be of type string or an instance of Buffer, TypedArray, or DataView');
};

class Stats {
    constructor(raw) {
        this._kind = raw.kind;
        this.size = raw.size;
        this.mode = { dir: 0o40755, symlink: 0o120777 }[raw.kind] ?? 0o100644;
        for (const name of ['atime', 'mtime', 'birthtime']) {
            this[`${name}Ms`] = raw[`${name}Ms`];
            this[name] = new Date(raw[`${name}Ms`]);
        }
        this.ctimeMs = this.mtimeMs;
        this.ctime = this.mtime;
    }
    isFile() { return this._kind === 'file'; }
    isDirectory() { return this._kind === 'dir'; }
    isSymbolicLink() { return this._kind === 'symlink'; }
    isFIFO() { return false; }
    isSocket() { return false; }
    isBlockDevice() { return false; }
    isCharacterDevice() { return false; }
}

class Dirent {
    constructor(name, parentPath, stats) {
        this.name = name;
        this.parentPath = parentPath;
        this.path = parentPath;
        this._stats = stats;
    }
    isFile() { return this._stats.isFile(); }
    isDirectory() { return this._stats.isDirectory(); }
    isSymbolicLink() { return this._stats.isSymbolicLink(); }
    isFIFO() { return false; }
    isSocket() { return false; }
    isBlockDevice() { return false; }
    isCharacterDevice() { return false; }
}

const join = (dir, name) => (dir.endsWith('/') ? dir + name : `${dir}/${name}`);

export const readFileSync = (path, options) => {
    const bytes = Buffer.from(native.readFile(pathOf(path)));
    const encoding = encodingOf(options);
    return encoding ? bytes.toString(encoding) : bytes;
};
export const writeFileSync = (path, data, options) => {
    const append = (typeof options === 'object' && options?.flag?.startsWith('a')) ?? false;
    native.writeFile(pathOf(path), bytesOf(data, encodingOf(options)), append);
};
export const appendFileSync = (path, data, options) =>
    native.writeFile(pathOf(path), bytesOf(data, encodingOf(options)), true);
export const existsSync = (path) => {
    try {
        return native.exists(pathOf(path));
    } catch {
        return false;
    }
};
const statWith = (follow) => (path, options) => {
    try {
        return new Stats(native.stat(pathOf(path), follow));
    } catch (e) {
        if (options?.throwIfNoEntry === false && e.code === 'ENOENT') return undefined;
        throw e;
    }
};
export const statSync = statWith(true);
export const lstatSync = statWith(false);
export const readdirSync = (path, options) => {
    path = pathOf(path);
    const names = native.readdir(path);
    if (options?.withFileTypes) return names.map((name) => new Dirent(name, path, lstatSync(join(path, name))));
    return names;
};
export const mkdirSync = (path, options) => {
    const recursive = typeof options === 'object' && Boolean(options?.recursive);
    path = pathOf(path);
    const existed = recursive && existsSync(path);
    native.mkdir(path, recursive);
    return recursive && !existed ? path : undefined;
};
export const unlinkSync = (path) => native.unlink(pathOf(path));
export const rmdirSync = (path, options) => native.rmdir(pathOf(path), Boolean(options?.recursive));
export const rmSync = (path, options) => {
    path = pathOf(path);
    const stats = lstatSync(path, { throwIfNoEntry: !options?.force });
    if (stats === undefined) return;
    if (!stats.isDirectory()) return native.unlink(path);
    if (!options?.recursive) {
        const e = new Error(`Path is a directory: rm returned EISDIR (is a directory) ${path}`);
        e.code = 'ERR_FS_EISDIR';
        throw e;
    }
    native.rmdir(path, true);
};
export const renameSync = (from, to) => native.rename(pathOf(from), pathOf(to));
export const copyFileSync = (from, to) => native.copyFile(pathOf(from), pathOf(to));
export const accessSync = (path) => {
    statSync(path);
};

const SYNC = {
    readFile: readFileSync,
    writeFile: writeFileSync,
    appendFile: appendFileSync,
    stat: statSync,
    lstat: lstatSync,
    readdir: readdirSync,
    mkdir: mkdirSync,
    unlink: unlinkSync,
    rmdir: rmdirSync,
    rm: rmSync,
    rename: renameSync,
    copyFile: copyFileSync,
    access: accessSync,
};

// The promise API runs the sync call: the host's filesystem calls block.
export const promises = Object.fromEntries(
    Object.entries(SYNC).map(([name, sync]) => [name, async (...args) => sync(...args)]),
);

// The callback API: the result (or error) arrives on a later tick.
const callbacks = Object.fromEntries(
    Object.entries(SYNC).map(([name, sync]) => [
        name,
        (...args) => {
            const callback = args.pop();
            if (typeof callback !== 'function') throw new TypeError('The "cb" argument must be of type function');
            let result;
            let error = null;
            try {
                result = sync(...args);
            } catch (e) {
                error = e;
            }
            queueMicrotask(() => callback(error, result));
        },
    ]),
);
export const {
    readFile, writeFile, appendFile, stat, lstat, readdir, mkdir, unlink, rmdir, rm, rename, copyFile, access,
} = callbacks;
export const exists = (path, callback) => queueMicrotask(() => callback(existsSync(path)));

export const constants = { F_OK: 0, R_OK: 4, W_OK: 2, X_OK: 1 };

export { Stats, Dirent };

export default {
    ...callbacks,
    ...Object.fromEntries(Object.entries(SYNC).map(([name, sync]) => [`${name}Sync`, sync])),
    existsSync,
    exists,
    promises,
    constants,
    Stats,
    Dirent,
};
