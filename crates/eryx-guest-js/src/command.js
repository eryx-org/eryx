// Evaluated by the run-command export (src/command.rs) on a fresh instance,
// before the program: Node's `process` and CommonJS module globals for
// `node ARGS...`. Receives the native intrinsics (`write`, `exit`,
// `compile`), the program's `process.argv`, its environment and working
// directory, and the main script's absolute path (null for `-e`, `-p` and
// stdin).
(native, argv, env, cwd, main) => {
    main ??= null;
    const define = (name, value) =>
        Object.defineProperty(globalThis, name, { value, writable: true, configurable: true });

    const decoder = new TextDecoder();
    const stream = (fd) => ({
        fd,
        isTTY: false,
        write(chunk, encoding, callback) {
            if (typeof encoding === 'function') callback = encoding;
            native.write(fd, typeof chunk === 'string' ? chunk : decoder.decode(chunk));
            if (typeof callback === 'function') queueMicrotask(callback);
            return true;
        },
    });

    const process = {
        argv,
        argv0: argv[0],
        execPath: argv[0],
        execArgv: [],
        env: Object.assign({}, env),
        exitCode: undefined,
        pid: 1,
        ppid: 0,
        platform: 'wasi',
        arch: 'wasm32',
        versions: { quickjs: native.quickjsVersion },
        stdout: stream(1),
        stderr: stream(2),
        cwd: () => cwd,
        // Ends the command where it is: what's thrown unwinds the script,
        // and the event loop stops at once with the status.
        exit(code) {
            native.exit(code === undefined ? process.exitCode : code);
        },
        nextTick: (fn, ...args) => queueMicrotask(() => fn(...args)),
    };
    define('process', process);

    // CommonJS: `require` of a relative or absolute path loads that file
    // from the sandbox's files as a CommonJS module (or JSON), with its own
    // `require`, `module`, `exports`, `__filename` and `__dirname`; anything
    // else is the runtime's `require` (built-ins and the environment).
    const baseRequire = globalThis.require;
    const dirname = (path) => path.slice(0, path.lastIndexOf('/')) || '/';
    const resolve = (dir, spec) => {
        const parts = spec.startsWith('/') ? [] : dir.split('/').filter(Boolean);
        for (const part of spec.split('/')) {
            if (part === '..') parts.pop();
            else if (part !== '' && part !== '.') parts.push(part);
        }
        return `/${parts.join('/')}`;
    };
    const isFile = (path) => {
        try {
            return __eryx_fs.stat(path, true).kind === 'file';
        } catch {
            return false;
        }
    };
    const cache = new Map();
    const load = (path, spec) => {
        const found = [path, `${path}.js`, `${path}.json`, `${path}.cjs`, `${path}/index.js`].find(isFile);
        if (found === undefined) {
            const e = new Error(`Cannot find module '${spec}'`);
            e.code = 'MODULE_NOT_FOUND';
            throw e;
        }
        if (cache.has(found)) return cache.get(found).exports;
        const module = { id: found, filename: found, loaded: false, exports: {}, children: [], paths: [] };
        cache.set(found, module);
        const source = decoder.decode(__eryx_fs.readFile(found));
        try {
            if (found.endsWith('.json')) {
                module.exports = JSON.parse(source);
            } else {
                // The wrapper goes on the source's first line, so line numbers hold.
                const body = source.startsWith('#!') ? `//${source}` : source;
                const fn = native.compile(
                    `(function (exports, require, module, __filename, __dirname) {${body}\n})`,
                    found,
                );
                fn.call(module.exports, module.exports, makeRequire(dirname(found)), module, found, dirname(found));
            }
        } catch (e) {
            cache.delete(found);
            throw e;
        }
        module.loaded = true;
        return module.exports;
    };
    const makeRequire = (dir) => {
        const require = (spec) => {
            spec = String(spec);
            if (spec.startsWith('./') || spec.startsWith('../') || spec.startsWith('/')) {
                return load(resolve(dir, spec), spec);
            }
            return baseRequire(spec);
        };
        require.resolve = (spec) => resolve(dir, String(spec));
        require.cache = cache;
        return require;
    };

    // The program runs as global code (so leading `import` declarations and
    // top-level `await` work, as in `execute`), with the main module's
    // CommonJS names as globals.
    const module = { id: '.', filename: main, loaded: false, exports: {}, children: [], paths: [] };
    const require = makeRequire(main === null ? cwd : dirname(main));
    require.main = module;
    define('require', require);
    define('module', module);
    define('exports', module.exports);
    if (main !== null) {
        define('__filename', main);
        define('__dirname', dirname(main));
        cache.set(main, module);
    }
};
