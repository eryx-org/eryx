// node:util, the parts packages commonly use. Written for eryx: the npm
// `util` package pulls in a dozen more for the rest.

export const inspect = (value, options) => describe(value, new Set(), options?.depth ?? 2);
inspect.custom = Symbol.for('nodejs.util.inspect.custom');

const describe = (value, seen, depth) => {
    switch (typeof value) {
        case 'string':
            return `'${value}'`;
        case 'bigint':
            return `${value}n`;
        case 'symbol':
            return value.toString();
        case 'function':
            return `[${value.toString().startsWith('class') ? 'class' : 'Function'}: ${value.name || '(anonymous)'}]`;
        case 'object':
            break;
        default:
            return String(value);
    }
    if (value === null) return 'null';
    if (typeof value[inspect.custom] === 'function') return String(value[inspect.custom](depth));
    if (value instanceof Error) return value.stack || String(value);
    if (value instanceof Date) return value.toISOString();
    if (value instanceof RegExp) return String(value);
    if (seen.has(value)) return '[Circular]';
    if (depth < 0) return Array.isArray(value) ? '[Array]' : '[Object]';
    seen.add(value);
    try {
        if (Array.isArray(value)) {
            return `[ ${value.map((v) => describe(v, seen, depth - 1)).join(', ')} ]`;
        }
        const entries = Object.keys(value).map((key) => {
            const name = /^[A-Za-z_$][\w$]*$/.test(key) ? key : `'${key}'`;
            return `${name}: ${describe(value[key], seen, depth - 1)}`;
        });
        return entries.length === 0 ? '{}' : `{ ${entries.join(', ')} }`;
    } finally {
        seen.delete(value);
    }
};

export const format = (first, ...args) => {
    if (typeof first !== 'string') {
        return [first, ...args].map((v) => (typeof v === 'string' ? v : inspect(v))).join(' ');
    }
    let rest = args;
    const text = first.replace(/%[sdifjoO%]/g, (spec) => {
        if (spec === '%%') return '%';
        if (rest.length === 0) return spec;
        const [arg] = rest;
        rest = rest.slice(1);
        switch (spec) {
            case '%s':
                return typeof arg === 'string' ? arg : inspect(arg);
            case '%d':
            case '%i': {
                const n = Number(arg);
                return String(spec === '%i' ? Math.trunc(n) : n);
            }
            case '%f':
                return String(parseFloat(arg));
            case '%j':
                try {
                    return JSON.stringify(arg);
                } catch {
                    return '[Circular]';
                }
            default:
                return inspect(arg);
        }
    });
    return [text, ...rest.map((v) => (typeof v === 'string' ? v : inspect(v)))].join(' ');
};

export const inherits = (ctor, superCtor) => {
    Object.defineProperty(ctor, 'super_', { value: superCtor, writable: true, configurable: true });
    Object.setPrototypeOf(ctor.prototype, superCtor.prototype);
};

const CUSTOM_PROMISIFY = Symbol.for('nodejs.util.promisify.custom');
export const promisify = (fn) => {
    if (typeof fn !== 'function') throw new TypeError('promisify takes a function');
    if (typeof fn[CUSTOM_PROMISIFY] === 'function') return fn[CUSTOM_PROMISIFY];
    return function (...args) {
        return new Promise((resolve, reject) => {
            fn.call(this, ...args, (error, value) => (error ? reject(error) : resolve(value)));
        });
    };
};
promisify.custom = CUSTOM_PROMISIFY;

export const callbackify = (fn) =>
    function (...args) {
        const callback = args.pop();
        fn.apply(this, args).then(
            (value) => callback(null, value),
            (error) => callback(error),
        );
    };

export const deprecate = (fn, message) => {
    let warned = false;
    return function (...args) {
        if (!warned) {
            warned = true;
            console.error(`DeprecationWarning: ${message}`);
        }
        return new.target ? Reflect.construct(fn, args, new.target) : fn.apply(this, args);
    };
};

export const debuglog = () => {
    const log = () => {};
    log.enabled = false;
    return log;
};

const tag = (value) => Object.prototype.toString.call(value).slice(8, -1);
export const types = {
    isDate: (v) => v instanceof Date,
    isRegExp: (v) => v instanceof RegExp,
    isPromise: (v) => v instanceof Promise,
    isMap: (v) => v instanceof Map,
    isSet: (v) => v instanceof Set,
    isWeakMap: (v) => v instanceof WeakMap,
    isWeakSet: (v) => v instanceof WeakSet,
    isNativeError: (v) => v instanceof Error,
    isArrayBuffer: (v) => v instanceof ArrayBuffer,
    isTypedArray: (v) => ArrayBuffer.isView(v) && !(v instanceof DataView),
    isUint8Array: (v) => v instanceof Uint8Array,
    isAsyncFunction: (v) => tag(v) === 'AsyncFunction' || tag(v) === 'AsyncGeneratorFunction',
    isGeneratorFunction: (v) => tag(v) === 'GeneratorFunction' || tag(v) === 'AsyncGeneratorFunction',
};

export const isArray = Array.isArray;
// Plain objects, arrays, Dates, RegExps, Maps, Sets and typed arrays.
// Map keys and Set members match by identity (SameValueZero), not deeply.
export const isDeepStrictEqual = (a, b) => {
    if (Object.is(a, b)) return true;
    if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) return false;
    if (Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) return false;
    if (a instanceof Date) return Object.is(a.getTime(), b.getTime());
    if (a instanceof RegExp) return String(a) === String(b) && a.lastIndex === b.lastIndex;
    if (a instanceof Map) {
        if (a.size !== b.size) return false;
        for (const [key, value] of a) {
            if (!b.has(key) || !isDeepStrictEqual(value, b.get(key))) return false;
        }
    } else if (a instanceof Set) {
        if (a.size !== b.size) return false;
        for (const value of a) if (!b.has(value)) return false;
    } else if (ArrayBuffer.isView(a) && !(a instanceof DataView)) {
        if (a.length !== b.length) return false;
        for (let i = 0; i < a.length; i++) if (!Object.is(a[i], b[i])) return false;
    }
    const keys = Object.keys(a);
    return (
        keys.length === Object.keys(b).length &&
        keys.every((key) => Object.hasOwn(b, key) && isDeepStrictEqual(a[key], b[key]))
    );
};

export const { TextEncoder, TextDecoder } = globalThis;

export default {
    inspect,
    format,
    inherits,
    promisify,
    callbackify,
    deprecate,
    debuglog,
    types,
    isArray,
    isDeepStrictEqual,
    TextEncoder,
    TextDecoder,
};
