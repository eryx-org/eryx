// Evaluated once after web.js, when the QuickJS context is created.
// Receives the native clock (src/timers.rs) and installs, as non-enumerable
// globals, `setTimeout`, `setInterval`, `setImmediate` and their `clear*`.
// Returns the event loop's side, which src/js.rs drives: it runs due timers
// one at a time (draining microtasks between them) and sleeps on the host's
// `sleep` import until the next deadline.
(native) => {
    const define = (name, value) =>
        Object.defineProperty(globalThis, name, { value, writable: true, configurable: true });

    // Node clamps delays outside [1, 2^31 - 1] ms to 1 ms.
    const TIMEOUT_MAX = 2 ** 31 - 1;
    const delayOf = (delay) => {
        const ms = Number(delay);
        return ms >= 1 && ms <= TIMEOUT_MAX ? ms : 1;
    };

    // Active timers by id, and a min-heap of them by (deadline, seq): seq
    // keeps equal deadlines first-in, first-out. A cleared or rescheduled
    // timer's old heap entry is skipped when it reaches the top.
    const active = new Map();
    let heap = [];
    let nextId = 1;
    let nextSeq = 0;
    let referenced = 0;
    let passTime = -Infinity;
    let passSeq = 0;

    const before = (a, b) => a.deadline < b.deadline || (a.deadline === b.deadline && a.seq < b.seq);
    const push = (entry) => {
        heap.push(entry);
        let i = heap.length - 1;
        while (i > 0) {
            const parent = (i - 1) >> 1;
            if (!before(heap[i], heap[parent])) break;
            [heap[i], heap[parent]] = [heap[parent], heap[i]];
            i = parent;
        }
    };
    const pop = () => {
        const top = heap[0];
        const last = heap.pop();
        if (heap.length > 0) {
            heap[0] = last;
            let i = 0;
            for (;;) {
                const l = 2 * i + 1;
                const r = l + 1;
                let m = i;
                if (l < heap.length && before(heap[l], heap[m])) m = l;
                if (r < heap.length && before(heap[r], heap[m])) m = r;
                if (m === i) break;
                [heap[i], heap[m]] = [heap[m], heap[i]];
                i = m;
            }
        }
        return top;
    };
    // The earliest live heap entry, dropping stale ones.
    const peek = () => {
        while (heap.length > 0) {
            const top = heap[0];
            if (active.get(top.timer.id) === top.timer && top.timer.entry === top) return top;
            pop();
        }
        return undefined;
    };

    const schedule = (timer, delay) => {
        const entry = { timer, deadline: native.now() + delay, seq: nextSeq++ };
        timer.entry = entry;
        push(entry);
        // Rescheduling (`refresh`, intervals) leaves the old entry behind;
        // rebuild the heap from live entries once they're the minority, so
        // it stays proportional to the timers.
        if (heap.length > 2 * active.size + 32) {
            const live = heap.filter((e) => active.get(e.timer.id) === e.timer && e.timer.entry === e);
            heap = [];
            for (const e of live) push(e);
        }
    };

    const remove = (timer) => {
        if (active.get(timer.id) !== timer) return;
        active.delete(timer.id);
        if (timer.ref) referenced--;
    };

    // What `setTimeout` returns, as in Node: `ref`/`unref` say whether it
    // keeps the execute running, and it converts to its numeric id.
    class Timeout {
        #timer;
        constructor(timer) {
            this.#timer = timer;
        }
        ref() {
            const timer = this.#timer;
            if (!timer.ref && active.get(timer.id) === timer) referenced++;
            timer.ref = true;
            return this;
        }
        unref() {
            const timer = this.#timer;
            if (timer.ref && active.get(timer.id) === timer) referenced--;
            timer.ref = false;
            return this;
        }
        hasRef() {
            return this.#timer.ref;
        }
        refresh() {
            if (active.get(this.#timer.id) === this.#timer) schedule(this.#timer, this.#timer.delay);
            return this;
        }
        close() {
            remove(this.#timer);
            return this;
        }
        [Symbol.toPrimitive]() {
            return this.#timer.id;
        }
    }
    class Immediate extends Timeout {}

    const start = (Kind, callback, delay, args, repeat) => {
        if (typeof callback !== 'function') {
            throw new TypeError(`The "callback" argument must be of type function. Received ${typeof callback}`);
        }
        const timer = { id: nextId++, callback, args, delay, repeat, ref: true, entry: null };
        timer.handle = new Kind(timer);
        active.set(timer.id, timer);
        referenced++;
        schedule(timer, delay);
        return timer.handle;
    };

    // Takes a handle or its numeric id; anything else is ignored, as in Node.
    const clear = (handle) => {
        if (handle === null || handle === undefined) return;
        const id = typeof handle === 'object' ? handle[Symbol.toPrimitive]?.() : Number(handle);
        const timer = active.get(id);
        if (timer) remove(timer);
    };

    define('setTimeout', (callback, delay, ...args) =>
        start(Timeout, callback, delayOf(delay), args, false),
    );
    define('setInterval', (callback, delay, ...args) =>
        start(Timeout, callback, delayOf(delay), args, true),
    );
    define('setImmediate', (callback, ...args) => start(Immediate, callback, 0, args, false));
    define('clearTimeout', clear);
    define('clearInterval', clear);
    define('clearImmediate', clear);

    return {
        // Start a pass of the event loop's timers phase: it runs the timers
        // due now, not ones their callbacks schedule (or reschedule), so a
        // pass always ends and the host gets a turn between passes.
        startPass() {
            passTime = native.now();
            passSeq = nextSeq;
        },
        // Run the earliest timer due in this pass, if one is; true if one
        // ran. A callback's exception propagates (and fails the execute).
        runDue() {
            const top = peek();
            if (!top || top.deadline > passTime || top.seq >= passSeq) return false;
            pop();
            const { timer } = top;
            if (timer.repeat) {
                schedule(timer, timer.delay);
            } else {
                remove(timer);
            }
            timer.callback.apply(timer.handle, timer.args);
            return true;
        },
        // [ms until the next timer (-1 if none), whether a referenced timer
        // is pending].
        state() {
            const top = peek();
            return [top ? Math.max(0, top.deadline - native.now()) : -1, referenced > 0];
        },
        // Drop every timer: none outlives its execute.
        clear() {
            active.clear();
            heap = [];
            referenced = 0;
        },
    };
};
