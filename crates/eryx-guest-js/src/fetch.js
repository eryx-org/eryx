// Evaluated once after the prelude, when the QuickJS context is created.
// Receives the native intrinsics and installs the fetch API on globalThis:
// fetch, Headers, Request, Response, AbortController and AbortSignal.
//
// The request itself runs in Rust (src/fetch.rs) over the host's eryx:net
// imports, which are synchronous: `fetch()` blocks the guest until the whole
// response has been read, and returns an already settled promise. Bodies are
// buffered (`body` is always null; use text(), json(), arrayBuffer() or
// bytes()). QuickJS has no timers, so an `AbortSignal.timeout()` signal turns
// aborted when it is next looked at, and fetch passes its deadline to the host.
(native) => {
    const define = (name, value) =>
        Object.defineProperty(globalThis, name, { value, writable: true, configurable: true });

    const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
    const headerName = (name) => {
        name = String(name);
        if (!TOKEN.test(name)) throw new TypeError(`Invalid header name: ${JSON.stringify(name)}`);
        return name.toLowerCase();
    };
    const headerValue = (value) => {
        value = String(value).replace(/^[\t\n\r ]+|[\t\n\r ]+$/g, '');
        if (/[\0\r\n]/.test(value)) throw new TypeError('Invalid header value');
        return value;
    };

    let headerPairs;
    class Headers {
        #list = [];

        static {
            headerPairs = (headers) => headers.#list.map(([name, value]) => [name, value]);
        }

        constructor(init) {
            if (init === undefined || init === null) return;
            if (init instanceof Headers) {
                this.#list = headerPairs(init);
            } else if (typeof init[Symbol.iterator] === 'function') {
                for (const pair of init) {
                    const [name, value, ...rest] = pair;
                    if (pair.length !== 2 || rest.length) {
                        throw new TypeError('Header pairs must have exactly two items');
                    }
                    this.append(name, value);
                }
            } else if (typeof init === 'object') {
                for (const name of Object.keys(init)) this.append(name, init[name]);
            } else {
                throw new TypeError('Headers must be an object, an iterable of pairs or a Headers');
            }
        }

        append(name, value) {
            this.#list.push([headerName(name), headerValue(value)]);
        }

        set(name, value) {
            name = headerName(name);
            value = headerValue(value);
            const first = this.#list.findIndex(([n]) => n === name);
            if (first < 0) {
                this.#list.push([name, value]);
                return;
            }
            this.#list[first][1] = value;
            this.#list = this.#list.filter(([n], i) => i <= first || n !== name);
        }

        get(name) {
            name = headerName(name);
            const values = this.#list.filter(([n]) => n === name).map(([, v]) => v);
            return values.length ? values.join(', ') : null;
        }

        getSetCookie() {
            return this.#list.filter(([n]) => n === 'set-cookie').map(([, v]) => v);
        }

        has(name) {
            name = headerName(name);
            return this.#list.some(([n]) => n === name);
        }

        delete(name) {
            name = headerName(name);
            this.#list = this.#list.filter(([n]) => n !== name);
        }

        forEach(callback, thisArg) {
            for (const [name, value] of this) callback.call(thisArg, value, name, this);
        }

        *entries() {
            const names = [...new Set(this.#list.map(([n]) => n))].sort();
            for (const name of names) {
                if (name === 'set-cookie') {
                    for (const value of this.getSetCookie()) yield [name, value];
                } else {
                    yield [name, this.get(name)];
                }
            }
        }

        *keys() {
            for (const [name] of this) yield name;
        }

        *values() {
            for (const [, value] of this) yield value;
        }

        [Symbol.iterator]() {
            return this.entries();
        }

        get [Symbol.toStringTag]() {
            return 'Headers';
        }
    }

    // A body as bytes, with the Content-Type it implies.
    const extractBody = (body) => {
        if (body === undefined || body === null) return [null, null];
        if (body instanceof ArrayBuffer) return [new Uint8Array(body.slice(0)), null];
        if (ArrayBuffer.isView(body)) {
            const end = body.byteOffset + body.byteLength;
            return [new Uint8Array(body.buffer.slice(body.byteOffset, end)), null];
        }
        return [new Uint8Array(native.encodeUtf8(String(body))), 'text/plain;charset=UTF-8'];
    };

    let bodyBytes;
    class Body {
        #bytes;
        #used = false;

        static {
            bodyBytes = (body) => body.#bytes;
        }

        constructor(bytes) {
            this.#bytes = bytes;
        }

        // Bodies are buffered, so there is no stream to read.
        get body() {
            return null;
        }

        get bodyUsed() {
            return this.#used;
        }

        #consume() {
            if (this.#used) throw new TypeError('Body has already been consumed');
            this.#used = true;
            return this.#bytes ? this.#bytes.slice() : new Uint8Array(0);
        }

        async bytes() {
            return this.#consume();
        }

        async arrayBuffer() {
            return this.#consume().buffer;
        }

        async text() {
            return native.decodeUtf8(this.#consume().buffer);
        }

        async json() {
            return JSON.parse(await this.text());
        }
    }

    const NORMALIZED_METHODS = ['DELETE', 'GET', 'HEAD', 'OPTIONS', 'POST', 'PUT', 'PATCH'];
    const REDIRECTS = ['follow', 'error', 'manual'];

    class Request extends Body {
        #url;
        #method;
        #headers;
        #signal;
        #redirect;

        constructor(input, init = {}) {
            const from = input instanceof Request ? input : null;
            let method = init.method ?? from?.method ?? 'GET';
            method = String(method);
            if (!TOKEN.test(method)) throw new TypeError(`Invalid method: ${method}`);
            if (NORMALIZED_METHODS.includes(method.toUpperCase())) method = method.toUpperCase();
            if (['CONNECT', 'TRACE', 'TRACK'].includes(method.toUpperCase())) {
                throw new TypeError(`Method ${method} is not allowed`);
            }

            let bytes = from ? bodyBytes(from) : null;
            let contentType = null;
            if (init.body !== undefined) [bytes, contentType] = extractBody(init.body);
            if (bytes && (method === 'GET' || method === 'HEAD')) {
                throw new TypeError(`A ${method} request can't have a body`);
            }
            super(bytes);

            this.#url = from ? from.url : String(input);
            this.#method = method;
            this.#headers = new Headers(init.headers ?? from?.headers);
            if (contentType && !this.#headers.has('content-type')) {
                this.#headers.set('content-type', contentType);
            }
            this.#signal = init.signal ?? from?.signal ?? null;
            this.#redirect = String(init.redirect ?? from?.redirect ?? 'follow');
            if (!REDIRECTS.includes(this.#redirect)) {
                throw new TypeError(`Invalid redirect mode: ${this.#redirect}`);
            }
        }

        get url() {
            return this.#url;
        }

        get method() {
            return this.#method;
        }

        get headers() {
            return this.#headers;
        }

        get signal() {
            return this.#signal;
        }

        get redirect() {
            return this.#redirect;
        }

        clone() {
            if (this.bodyUsed) throw new TypeError('Body has already been consumed');
            return new Request(this);
        }

        get [Symbol.toStringTag]() {
            return 'Request';
        }
    }

    let fromNative;
    class Response extends Body {
        #status;
        #statusText;
        #headers;
        #url = '';
        #redirected = false;

        static {
            fromNative = (raw) => {
                const response = new Response(raw.body, {
                    statusText: raw.statusText,
                    headers: raw.headers,
                });
                // Any status the network returns, not just the 200-599 a
                // script may construct.
                response.#status = raw.status;
                response.#url = raw.url;
                response.#redirected = raw.redirected;
                return response;
            };
        }

        constructor(body = null, init = {}) {
            const [bytes, contentType] = extractBody(body);
            super(bytes);
            const status = init.status ?? 200;
            if (!Number.isInteger(status) || status < 200 || status > 599) {
                throw new RangeError(`Invalid status: ${status}`);
            }
            this.#status = status;
            this.#statusText = String(init.statusText ?? '');
            this.#headers = new Headers(init.headers);
            if (contentType && !this.#headers.has('content-type')) {
                this.#headers.set('content-type', contentType);
            }
        }

        static json(data, init = {}) {
            const response = new Response(JSON.stringify(data), init);
            response.headers.set('content-type', 'application/json');
            return response;
        }

        get status() {
            return this.#status;
        }

        get ok() {
            return this.#status >= 200 && this.#status <= 299;
        }

        get statusText() {
            return this.#statusText;
        }

        get headers() {
            return this.#headers;
        }

        get url() {
            return this.#url;
        }

        get redirected() {
            return this.#redirected;
        }

        get type() {
            return this.#url ? 'basic' : 'default';
        }

        clone() {
            if (this.bodyUsed) throw new TypeError('Body has already been consumed');
            return fromNative({
                body: bodyBytes(this),
                status: this.#status,
                statusText: this.#statusText,
                headers: headerPairs(this.#headers),
                url: this.#url,
                redirected: this.#redirected,
            });
        }

        get [Symbol.toStringTag]() {
            return 'Response';
        }
    }

    const ILLEGAL = Symbol('AbortSignal');
    let deadlineOf;
    let abortSignal;
    class AbortSignal {
        #aborted = false;
        #reason = undefined;
        #deadline = undefined;
        #listeners = [];
        onabort = null;

        static {
            deadlineOf = (signal) => {
                signal.aborted; // a passed deadline aborts the signal
                return signal.#deadline;
            };
            abortSignal = (signal, reason) => {
                if (signal.#aborted) return;
                signal.#aborted = true;
                signal.#reason =
                    reason === undefined
                        ? new DOMException('This operation was aborted', 'AbortError')
                        : reason;
                const event = { type: 'abort', target: signal };
                if (typeof signal.onabort === 'function') signal.onabort(event);
                for (const listener of signal.#listeners.splice(0)) listener.call(signal, event);
            };
        }

        constructor(token) {
            if (token !== ILLEGAL) throw new TypeError('Illegal constructor');
        }

        static abort(reason) {
            const signal = new AbortSignal(ILLEGAL);
            abortSignal(signal, reason);
            return signal;
        }

        static timeout(ms) {
            const signal = new AbortSignal(ILLEGAL);
            signal.#deadline = Date.now() + Math.max(0, Number(ms));
            return signal;
        }

        static any(signals) {
            const signal = new AbortSignal(ILLEGAL);
            for (const other of signals) {
                if (other.aborted) {
                    abortSignal(signal, other.reason);
                    return signal;
                }
                const deadline = other.#deadline;
                if (deadline !== undefined) {
                    signal.#deadline = Math.min(signal.#deadline ?? Infinity, deadline);
                }
                other.addEventListener('abort', () => abortSignal(signal, other.reason));
            }
            return signal;
        }

        get aborted() {
            if (!this.#aborted && this.#deadline !== undefined && Date.now() >= this.#deadline) {
                abortSignal(this, new DOMException('The operation timed out.', 'TimeoutError'));
            }
            return this.#aborted;
        }

        get reason() {
            this.aborted;
            return this.#reason;
        }

        throwIfAborted() {
            if (this.aborted) throw this.#reason;
        }

        addEventListener(type, listener) {
            if (type === 'abort' && typeof listener === 'function') this.#listeners.push(listener);
        }

        removeEventListener(type, listener) {
            if (type === 'abort') this.#listeners = this.#listeners.filter((l) => l !== listener);
        }

        get [Symbol.toStringTag]() {
            return 'AbortSignal';
        }
    }

    class AbortController {
        #signal = new AbortSignal(ILLEGAL);

        get signal() {
            return this.#signal;
        }

        abort(reason) {
            abortSignal(this.#signal, reason);
        }
    }

    // Errors in the request itself, reported as `TypeError(message)`; anything
    // else is a network error, `TypeError('fetch failed', { cause })`.
    const REQUEST_ERRORS = ['invalid-url', 'unsupported-scheme', 'invalid-header', 'invalid-method'];

    async function fetch(input, init) {
        const request = new Request(input, init);
        const signal = request.signal;
        signal?.throwIfAborted();
        const deadline = signal ? deadlineOf(signal) : undefined;
        let raw;
        try {
            raw = native.fetch({
                method: request.method,
                url: request.url,
                headers: headerPairs(request.headers),
                body: bodyBytes(request) ?? undefined,
                redirect: request.redirect,
                timeoutMs: deadline === undefined ? undefined : Math.max(0, deadline - Date.now()),
            });
        } catch (e) {
            if (signal?.aborted) throw signal.reason;
            if (e && typeof e.code === 'string' && !(e instanceof Error)) {
                if (REQUEST_ERRORS.includes(e.code)) throw new TypeError(e.message);
                throw new TypeError('fetch failed', {
                    cause: Object.assign(new Error(e.message), { code: e.code }),
                });
            }
            throw e;
        }
        return fromNative(raw);
    }

    define('fetch', fetch);
    define('Headers', Headers);
    define('Request', Request);
    define('Response', Response);
    define('AbortController', AbortController);
    define('AbortSignal', AbortSignal);
};
