// node:timers/promises, on the timer globals (src/timers.js).

const aborted = (signal) =>
    signal.reason ?? Object.assign(new Error('The operation was aborted'), { name: 'AbortError' });

// Resolve with `value` after the timer `start` sets, unless `signal` aborts
// first. `ref: false` lets the execute finish without it, as in Node.
const later = (start, value, { signal, ref = true } = {}) =>
    new Promise((resolve, reject) => {
        if (signal?.aborted) return reject(aborted(signal));
        const onAbort = () => {
            clearTimeout(handle);
            reject(aborted(signal));
        };
        const handle = start(() => {
            signal?.removeEventListener?.('abort', onAbort);
            resolve(value);
        });
        if (!ref) handle.unref();
        signal?.addEventListener?.('abort', onAbort, { once: true });
    });

const timeout = (delay, value, options) =>
    later((done) => globalThis.setTimeout(done, delay), value, options);
const immediate = (value, options) => later((done) => globalThis.setImmediate(done), value, options);

// An async iterator that yields `value` every `delay` ms until it's
// returned from or `signal` aborts.
async function* interval(delay, value, options = {}) {
    while (true) {
        yield await timeout(delay, value, options);
    }
}

export { timeout as setTimeout, immediate as setImmediate, interval as setInterval };
export const scheduler = { wait: (delay, options) => timeout(delay, undefined, options) };

export default { setTimeout: timeout, setImmediate: immediate, setInterval: interval, scheduler };
