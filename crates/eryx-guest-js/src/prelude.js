// Evaluated once when the QuickJS context is created. Receives the native
// intrinsics object (`invoke`, `write`) and returns the hooks the Rust side
// calls per execution. Everything user-visible is installed on globalThis.
(native) => {
    const inspect = (v) => {
        if (typeof v === 'string') return v;
        if (v instanceof Error) return formatError(v);
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

    // `await invoke("name", {arg: 1})` (or `invoke("name", 1, 2)`) — the JS
    // analog of Python's `await invoke("name", arg=1)`. Resolves to the parsed
    // JSON result.
    globalThis.invoke = async (name, ...args) => {
        const payload = JSON.stringify(callArgs(String(name), args));
        const json = await native.invoke(String(name), payload);
        return json === '' ? null : JSON.parse(json);
    };

    let callbacks = [];
    globalThis.listCallbacks = () =>
        callbacks.map((cb) => ({
            name: cb.name,
            description: cb.description,
            parametersSchema: cb.parameters_schema_json ? JSON.parse(cb.parameters_schema_json) : null,
        }));

    // Top-level names we installed last time, so a changed callback set can
    // remove them. Anything else already on globalThis is never shadowed.
    let installed = [];
    const installCallbacks = (list) => {
        for (const name of installed) Reflect.deleteProperty(globalThis, name);
        installed = [];
        callbacks = list;
        paramCache = new Map();
        for (const { name } of callbacks) {
            const parts = name.split('.');
            const root = parts[0];
            if (root in globalThis && !installed.includes(root)) continue;
            if (!installed.includes(root)) installed.push(root);
            let target = globalThis;
            for (const part of parts.slice(0, -1)) {
                target = target[part] ??= {};
            }
            target[parts[parts.length - 1]] = (...args) => globalThis.invoke(name, ...args);
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

    return { formatError, installCallbacks, captureResult, discardResult };
};
