// Evaluated once when the QuickJS context is created. Receives the native
// intrinsics object (`invoke`, `write`, `trace`, `evalScript`) and returns
// the hooks the Rust side calls per execution. Everything user-visible is
// installed on globalThis.
(native) => {
    const inspect = (v) => {
        if (typeof v === 'string') return v;
        if (v instanceof Error) return formatError(v);
        if (prefixNamespaces.has(v)) return `[Namespace ${prefixNamespaces.get(v)}]`;
        if (typeof v === 'function') return `[Function ${v.name || '(anonymous)'}]`;
        if (typeof v === 'bigint') return `${v}n`;
        if (typeof v === 'symbol' || v === undefined) return String(v);
        try {
            const s = JSON.stringify(v);
            return s === undefined ? String(v) : s;
        } catch {
            return String(v);
        }
    };

    const formatError = (e) => {
        if (!(e instanceof Error)) return `Uncaught ${inspect(e)}`;
        const stack = e.stack ? `\n${e.stack.trimEnd()}` : '';
        return `${e.name}: ${e.message}${stack}`;
    };

    const line = (stream) => (...args) => native.write(stream, args.map(inspect).join(' ') + '\n');
    globalThis.console = {
        log: line(0),
        info: line(0),
        debug: line(0),
        error: line(1),
        warn: line(1),
    };

    // A call's arguments as the JSON object the host takes. A single plain
    // object whose keys are all parameters of the callback (or of a callback
    // with no declared parameters) is keyword arguments: `add({a: 1, b: 2})`,
    // like Python's `add(a=1, b=2)`. Anything else is positional, `add(1, 2)`
    // or `configure({retries: 3})` for a callback whose first parameter,
    // `options`, takes an object, sent under POSITIONAL for the host to bind to the
    // callback's schema in order (see `POSITIONAL_ARGS_KEY` in eryx).
    const POSITIONAL = '__eryx_args__';
    const isPlainObject = (v) =>
        v !== null &&
        typeof v === 'object' &&
        [Object.prototype, null].includes(Object.getPrototypeOf(v));
    // Parameter names from a callback's schema, parsed on first use.
    let paramCache = new Map();
    const paramNames = (name) => {
        if (!paramCache.has(name)) {
            const cb = callbacks.find((c) => c.name === name);
            let names = [];
            try {
                const schema = cb?.parameters_schema_json ? JSON.parse(cb.parameters_schema_json) : null;
                names = Object.keys(schema?.properties ?? {});
            } catch {
                // An unparseable schema declares nothing.
            }
            paramCache.set(name, names);
        }
        return paramCache.get(name);
    };
    // Trailing `undefined` arguments are dropped, as if they had not been
    // passed (`f(1, undefined)` is `f(1)`, `invoke(name, undefined)` is
    // `invoke(name)`); an `undefined` before another argument is sent as null.
    const callArgs = (name, args) => {
        while (args.length && args.at(-1) === undefined) args.pop();
        if (args.length === 1 && isPlainObject(args[0])) {
            if (Object.hasOwn(args[0], POSITIONAL)) {
                throw new TypeError(
                    `${name}() got the keyword argument '${POSITIONAL}', which is reserved for positional arguments`,
                );
            }
            const names = paramNames(name);
            if (names.length === 0 || Object.keys(args[0]).every((k) => names.includes(k))) {
                return args[0];
            }
        }
        return args.length === 0 ? {} : { [POSITIONAL]: args };
    };

    // The arguments as JSON with every object's keys sorted, so equal
    // arguments are equal text however the script ordered them: callback
    // replay journals key on this text (as the CPython guest's sorted
    // json.dumps does).
    const canonicalJson = (value) =>
        JSON.stringify(value, (_key, v) =>
            v !== null && typeof v === 'object' && !Array.isArray(v)
                ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, v[k]]))
                : v,
        );

    // `await invoke("name", {arg: 1})` (or `invoke("name", 1, 2)`) — the JS
    // analog of Python's `await invoke("name", arg=1)`. Resolves to the parsed
    // JSON result.
    // Each call reports callback_start/callback_end trace events in the
    // CPython guest's shape, the arguments as callback_start's context.
    globalThis.invoke = async (name, ...args) => {
        name = String(name);
        const payload = canonicalJson(callArgs(name, args));
        native.trace(0, JSON.stringify({ type: 'callback_start', name }), payload);
        let json;
        try {
            json = await native.invoke(name, payload);
        } catch (e) {
            const error = e instanceof Error ? e.message : String(e);
            native.trace(0, JSON.stringify({ type: 'callback_end', name, error }), '');
            throw e;
        }
        native.trace(0, JSON.stringify({ type: 'callback_end', name }), '');
        return json === '' ? null : JSON.parse(json);
    };

    let callbacks = [];
    globalThis.listCallbacks = () =>
        callbacks.map((cb) => ({
            name: cb.name,
            description: cb.description,
            parametersSchema: cb.parameters_schema_json ? JSON.parse(cb.parameters_schema_json) : null,
        }));

    // A prefix callback (`mcp.svc.*`) handles every name below its prefix,
    // so its namespace is a Proxy: any property is another such namespace,
    // and calling one invokes its full dotted name. `invoke` binds the
    // arguments by name, so an exact callback below the prefix keeps its
    // schema; anything else sends positional arguments as `__eryx_args__`
    // for the host to bind. Some names aren't tool names, so looking one up
    // never calls the host: symbols, Object.prototype's (`toString`, ...),
    // `then` (awaiting a namespace), `toJSON` (JSON.stringify, so
    // console.log and the result capture), and `call`, `apply` and `bind`.
    // `invoke("mcp.svc.toJSON")` still reaches them.
    const NOT_TOOLS = new Set(['then', 'toJSON', 'call', 'apply', 'bind']);
    // Every namespace, by its dotted name.
    const prefixNamespaces = new WeakMap();
    const prefixNamespace = (name) => {
        // Its own `toString`: Function.prototype's reads `name`, another
        // namespace, whose string is its `name`'s, ... without end.
        const target = () => {};
        Object.defineProperty(target, 'toString', { value: () => `[Namespace ${name}]` });
        const namespace = new Proxy(target, {
            get(target, key) {
                if (typeof key === 'symbol' || NOT_TOOLS.has(key) || key in Object.prototype) {
                    return Reflect.get(target, key);
                }
                return prefixNamespace(`${name}.${key}`);
            },
            apply(_target, _this, args) {
                return globalThis.invoke(name, ...args);
            },
        });
        prefixNamespaces.set(namespace, name);
        return namespace;
    };

    // Top-level names we installed last time, so a changed callback set can
    // remove them. Anything else already on globalThis is never shadowed.
    let installed = [];
    const installCallbacks = (list) => {
        baseline(); // before the first callbacks are installed
        for (const name of installed) Reflect.deleteProperty(globalThis, name);
        installed = [];
        callbacks = list;
        paramCache = new Map();
        const prefixes = [];
        for (const { name } of callbacks) {
            const parts = name.split('.');
            const root = parts[0];
            if (root in globalThis && !installed.includes(root)) continue;
            if (!installed.includes(root)) installed.push(root);
            if (parts.length > 1 && parts.at(-1) === '*') {
                prefixes.push(parts.slice(0, -1));
                continue;
            }
            let target = globalThis;
            for (const part of parts.slice(0, -1)) {
                target = target[part] ??= {};
            }
            target[parts[parts.length - 1]] = (...args) => globalThis.invoke(name, ...args);
        }
        // After the exact callbacks, so a prefix's namespace replaces theirs
        // (it still reaches them by name); shorter prefixes first, so a
        // longer one inside another is already covered.
        prefixes.sort((a, b) => a.length - b.length);
        for (const parts of prefixes) {
            let target = globalThis;
            for (const part of parts.slice(0, -1)) {
                target = target[part] ??= {};
                if (prefixNamespaces.has(target)) break;
            }
            if (prefixNamespaces.has(target)) continue;
            target[parts.at(-1)] = prefixNamespace(parts.join('.'));
        }
    };

    const IDENT = /^[A-Za-z_$][\w$]*$/;
    // Read and consume the result variable, whether it is a global property
    // (`var`, bare assignment) or a top-level lexical binding (`let`/`const`),
    // which only indirect eval can see. Returns [json, error], '' for absent.
    const captureResult = (name) => {
        if (!IDENT.test(name)) return ['', `invalid result variable name: ${name}`];
        const read = (0, eval)(`typeof ${name} === 'undefined' ? undefined : ${name}`);
        discardResult(name);
        if (read === undefined) return ['', ''];
        try {
            const json = JSON.stringify(read);
            return json === undefined ? ['', `${name} is not JSON-serializable`] : [json, ''];
        } catch (e) {
            return ['', `${name} is not JSON-serializable: ${e.message}`];
        }
    };
    const discardResult = (name) => {
        if (!IDENT.test(name)) return;
        // Removes plain properties; `var` globals are non-configurable and
        // lexical bindings aren't properties, so both fall through to assignment.
        Reflect.deleteProperty(globalThis, name);
        try {
            (0, eval)(`if (typeof ${name} !== 'undefined') ${name} = undefined`);
        } catch {
            // const bindings can't be consumed; nothing else to do.
        }
    };

    // Session state for snapshot-state/restore-state. Top-level variables
    // are the global object's own properties (`var`, `function`, bare
    // assignment) added after this prelude, minus installed callbacks, and
    // the global `let`/`const`/`class` bindings, which aren't properties and
    // can't be listed: any identifier in code run so far is a candidate,
    // and one is a binding if indirect eval resolves it but it isn't a
    // property of the global object.
    // Taken on first use, after every setup script (this prelude, then
    // fetch.js) has run, and before any callback or user code.
    let baselineMap;
    const baseline = () =>
        (baselineMap ??= new Map(
            Object.getOwnPropertyNames(globalThis).map((name) => [
                name,
                Object.getOwnPropertyDescriptor(globalThis, name).value,
            ]),
        ));
    // Set by the host's preamble before each execute (secret placeholders),
    // so never session state. `markHostGlobals` records what the host set,
    // after its preamble runs; anything else under these names is the
    // user's, and is named in `skipped` since it isn't kept either.
    const HOST_GLOBALS = new Set(['process']);
    // Replaced by the guest itself on each instance's first execute (see
    // fresh.rs), so never session state either.
    const INSTANCE_GLOBALS = new Set(['performance']);
    const hostValues = new Map();
    const markHostGlobals = () => {
        for (const name of HOST_GLOBALS) hostValues.set(name, globalThis[name]);
    };
    // Stands in for a variable that can't be kept, so it is named in
    // `skipped`: functions don't serialize.
    const UNKEPT = () => {};
    // The prototypes a snapshot keeps. JS_WriteObject writes any other
    // object (a class instance: a Buffer, an EventEmitter) as a plain one,
    // so a value holding one is skipped rather than restored without its
    // class.
    const KEPT_PROTOTYPES = new Set(
        [
            Object, Array, Date, RegExp, Map, Set, ArrayBuffer, DataView, Boolean, Number,
            String, BigInt, Error, EvalError, RangeError, ReferenceError, SyntaxError,
            TypeError, URIError, Int8Array, Uint8Array, Uint8ClampedArray, Int16Array,
            Uint16Array, Int32Array, Uint32Array, Float32Array, Float64Array, BigInt64Array,
            BigUint64Array,
        ].map((type) => type.prototype),
    );
    KEPT_PROTOTYPES.add(null);
    const losesClass = (root) => {
        const seen = new Set();
        const pending = [root];
        while (pending.length > 0) {
            const value = pending.pop();
            if (value === null || typeof value !== 'object' || seen.has(value)) continue;
            seen.add(value);
            if (!KEPT_PROTOTYPES.has(Object.getPrototypeOf(value))) return true;
            if (ArrayBuffer.isView(value)) continue;
            if (value instanceof Map) {
                for (const [key, item] of value) pending.push(key, item);
            } else if (value instanceof Set) {
                for (const item of value) pending.push(item);
            }
            for (const key of Object.getOwnPropertyNames(value)) {
                const desc = Object.getOwnPropertyDescriptor(value, key);
                if ('value' in desc) pending.push(desc.value);
            }
        }
        return false;
    };
    const candidates = new Set();
    // Expressions that look like identifiers but aren't bindings.
    const NOT_BINDINGS = new Set(['this', 'true', 'false', 'null']);
    const IDENTIFIERS = /[\p{ID_Start}$_][\p{ID_Continue}$\u200c\u200d]*/gu;
    const IDENTIFIER = /^[\p{ID_Start}$_][\p{ID_Continue}$\u200c\u200d]*$/u;
    const noteCode = (code) => {
        for (const [name] of code.matchAll(IDENTIFIERS)) candidates.add(name);
    };
    // What the session's `import` declarations bound (`{specifier, form,
    // bindings}` records from src/modules.rs), and each local's value then:
    // a snapshot re-imports these rather than keeping module objects.
    let imports = [];
    const importBound = new Map();
    // What each import statement bound, as it ran (the rewrite calls
    // `__eryx_bound({local, ...})` right after it), so a reassignment later
    // in the same script still counts as one.
    const pendingBound = new Map();
    // Globals that loading an imported module created (zod's
    // `__zod_globalConfig`, say): the module's, made again when a restore
    // re-imports it, so never session state. The rewrite calls
    // `__eryx_loading()` before each import; its `var`s already exist then.
    const importOwned = new Set();
    let loadingFrom = null;
    const defineHidden = (name, value) =>
        Object.defineProperty(globalThis, name, { value, writable: true, configurable: true });
    defineHidden('__eryx_loading', () => {
        loadingFrom = new Set(Object.getOwnPropertyNames(globalThis));
    });
    defineHidden('__eryx_bound', (values) => {
        for (const name of Object.keys(values)) pendingBound.set(name, values[name]);
        if (loadingFrom === null) return;
        for (const name of Object.getOwnPropertyNames(globalThis)) {
            if (!loadingFrom.has(name) && !Object.hasOwn(values, name)) importOwned.add(name);
        }
        loadingFrom = null;
    });
    // A failed execute commits none of its imports.
    const discardImports = () => {
        pendingBound.clear();
        loadingFrom = null;
    };
    const importLocals = (record) =>
        record.form === 'named' ? record.bindings.map((pair) => pair[1]) : record.bindings;
    // After an execute (or restore) whose imports all ran.
    const commitImports = (records) => {
        for (const record of records) {
            const key = JSON.stringify(record);
            if (!imports.some((r) => JSON.stringify(r) === key)) imports.push(record);
            for (const local of importLocals(record)) {
                if (pendingBound.has(local)) importBound.set(local, pendingBound.get(local));
            }
        }
        pendingBound.clear();
    };
    // [name, kind, value] per variable, kind 'var', 'let' or 'const', then
    // ['', 'imports', records] if anything was imported. An
    // accessor property's getter stands in for its value, so it is skipped.
    // Global properties go by any string key; bindings by identifier.
    const stateEntries = () => {
        const entries = [];
        for (const name of Object.getOwnPropertyNames(globalThis)) {
            if (installed.includes(name) || INSTANCE_GLOBALS.has(name) || importOwned.has(name)) {
                continue;
            }
            if (HOST_GLOBALS.has(name)) {
                if (!hostValues.has(name) || !Object.is(globalThis[name], hostValues.get(name))) {
                    entries.push([name, 'var', UNKEPT]);
                }
                continue;
            }
            const desc = Object.getOwnPropertyDescriptor(globalThis, name);
            if (baseline().has(name)) {
                // A replaced builtin isn't restored over the new one.
                if (!Object.is(desc.value, baseline().get(name))) entries.push([name, 'var', UNKEPT]);
                continue;
            }
            // Still what its import bound: the import brings it back.
            if (importBound.has(name) && Object.is(desc.value, importBound.get(name))) continue;
            entries.push([name, 'var', 'value' in desc ? desc.value : UNKEPT]);
        }
        for (const name of candidates) {
            if (NOT_BINDINGS.has(name) || Object.hasOwn(globalThis, name)) continue;
            let value;
            try {
                value = (0, eval)(name);
            } catch (e) {
                // Declared by a script that failed before initializing it.
                if (String(e?.message).endsWith('is not initialized')) {
                    entries.push([name, 'let', UNKEPT]);
                }
                continue; // otherwise not declared
            }
            let kind = 'let';
            try {
                (0, eval)(`${name} = ${name}`);
            } catch {
                kind = 'const';
            }
            entries.push([name, kind, value]);
        }
        for (const entry of entries) {
            if (entry[2] !== UNKEPT && losesClass(entry[2])) entry[2] = UNKEPT;
        }
        if (imports.length > 0) entries.push(['', 'imports', imports]);
        return entries;
    };
    // The reverse, into a fresh context, in two steps so a malformed
    // snapshot fails before anything runs: `validateEntries` checks the
    // entries' shape (they come from untrusted bytes) and returns the import
    // records, which the guest re-imports and commits; then `applyEntries`
    // defines the variables, over what the imports bound.
    const RESTORING = '__eryx_restoring__';
    // A name interpolated into source must be an identifier.
    const binding = (name) =>
        typeof name === 'string' &&
        IDENTIFIER.test(name) &&
        !NOT_BINDINGS.has(name) &&
        !baseline().has(name) &&
        !HOST_GLOBALS.has(name) &&
        !INSTANCE_GLOBALS.has(name);
    const validRecord = (r) =>
        r !== null &&
        typeof r === 'object' &&
        typeof r.specifier === 'string' &&
        Array.isArray(r.bindings) &&
        (r.form === 'named'
            ? r.bindings.every(
                  (pair) =>
                      Array.isArray(pair) &&
                      pair.length === 2 &&
                      typeof pair[0] === 'string' &&
                      binding(pair[1]),
              )
            : ['default', 'namespace'].includes(r.form) &&
              r.bindings.length === 1 &&
              binding(r.bindings[0]));
    const isImports = (e) => Array.isArray(e) && e.length === 3 && e[0] === '' && e[1] === 'imports';
    const validateEntries = (entries) => {
        // A property's name is only ever a key; a binding's goes into source.
        const valid = (e) =>
            isImports(e)
                ? Array.isArray(e[2]) && e[2].every(validRecord)
                : Array.isArray(e) &&
                  e.length === 3 &&
                  typeof e[0] === 'string' &&
                  !baseline().has(e[0]) &&
                  !HOST_GLOBALS.has(e[0]) &&
                  !INSTANCE_GLOBALS.has(e[0]) &&
                  e[0] !== '__proto__' &&
                  (e[1] === 'var' || (['let', 'const'].includes(e[1]) && binding(e[0])));
        if (
            !Array.isArray(entries) ||
            !entries.every(valid) ||
            entries.filter(isImports).length > 1
        ) {
            throw new TypeError('malformed session state');
        }
        return entries.find(isImports)?.[2] ?? [];
    };
    const applyEntries = (entries) => {
        for (const [name, kind, value] of entries) {
            if (kind === 'imports') continue;
            if (kind === 'var') {
                globalThis[name] = value;
                continue;
            }
            globalThis[RESTORING] = value;
            try {
                // A global script, not eval: eval's let/const don't outlive it.
                native.evalScript(`${kind} ${name} = globalThis.${RESTORING};`);
            } finally {
                delete globalThis[RESTORING];
            }
            candidates.add(name);
        }
    };

    return {
        formatError,
        installCallbacks,
        captureResult,
        discardResult,
        noteCode,
        markHostGlobals,
        commitImports,
        discardImports,
        stateEntries,
        validateEntries,
        applyEntries,
    };
};
