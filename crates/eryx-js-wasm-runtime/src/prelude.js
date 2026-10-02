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

    // `await invoke("name", {arg: 1})` — the JS analog of Python's
    // `await invoke("name", arg=1)`. Resolves to the parsed JSON result.
    globalThis.invoke = async (name, args = {}) => {
        const json = await native.invoke(String(name), JSON.stringify(args));
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
        for (const { name } of callbacks) {
            const parts = name.split('.');
            const root = parts[0];
            if (root in globalThis && !installed.includes(root)) continue;
            if (!installed.includes(root)) installed.push(root);
            let target = globalThis;
            for (const part of parts.slice(0, -1)) {
                target = target[part] ??= {};
            }
            target[parts[parts.length - 1]] = (args = {}) => globalThis.invoke(name, args);
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
