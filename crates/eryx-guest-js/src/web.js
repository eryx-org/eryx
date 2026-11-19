// Evaluated once after fetch.js, when the QuickJS context is created.
// Receives the native intrinsics (src/web.rs) and installs the web globals
// QuickJS lacks that packages commonly use:
//
// - `TextEncoder` and `TextDecoder` (UTF-8 only);
// - `crypto.getRandomValues()` and `crypto.randomUUID()`, from the host's
//   `wasi:random` (no `crypto.subtle`);
// - `URL` and `URLSearchParams` (core-js's pure build, `native.url`).
(native) => {
    const define = (name, value) =>
        Object.defineProperty(globalThis, name, { value, writable: true, configurable: true });

    const bytesOf = (input) => {
        if (input instanceof ArrayBuffer) return new Uint8Array(input);
        if (ArrayBuffer.isView(input)) {
            return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
        }
        throw new TypeError('The "input" argument must be an ArrayBuffer or ArrayBufferView');
    };

    class TextEncoder {
        get encoding() {
            return 'utf-8';
        }
        // Lone surrogates become U+FFFD, as the spec's USVString conversion
        // does (the native side only takes well-formed strings).
        encode(input = '') {
            return new Uint8Array(native.encodeUtf8(String(input).toWellFormed()));
        }
        encodeInto(source, destination) {
            const text = String(source).toWellFormed();
            const bytes = new Uint8Array(native.encodeUtf8(text));
            if (bytes.length <= destination.length) {
                destination.set(bytes);
                return { read: text.length, written: bytes.length };
            }
            // Cut before the sequence that doesn't fit whole.
            let cut = destination.length;
            while (cut > 0 && (bytes[cut] & 0xc0) === 0x80) cut--;
            const fits = bytes.subarray(0, cut);
            destination.set(fits);
            return { read: native.decodeUtf8(fits.slice().buffer, false).length, written: cut };
        }
    }

    const UTF8_LABELS = ['utf-8', 'utf8', 'unicode-1-1-utf-8'];

    // How many bytes at the end of `bytes` start a sequence they don't finish.
    const incompleteTail = (bytes) => {
        for (let back = 1; back <= Math.min(3, bytes.length); back++) {
            const byte = bytes[bytes.length - back];
            if ((byte & 0xc0) === 0x80) continue; // a continuation byte
            const needs = byte >= 0xf0 ? 4 : byte >= 0xe0 ? 3 : byte >= 0xc0 ? 2 : 1;
            return needs > back ? back : 0;
        }
        return 0;
    };

    class TextDecoder {
        #fatal;
        #ignoreBOM;
        #pending = new Uint8Array(0);
        #started = false;
        constructor(label = 'utf-8', options = {}) {
            if (!UTF8_LABELS.includes(String(label).trim().toLowerCase())) {
                throw new RangeError(`The "${label}" encoding isn't supported in eryx (UTF-8 only)`);
            }
            this.#fatal = Boolean(options.fatal);
            this.#ignoreBOM = Boolean(options.ignoreBOM);
        }
        get encoding() {
            return 'utf-8';
        }
        get fatal() {
            return this.#fatal;
        }
        get ignoreBOM() {
            return this.#ignoreBOM;
        }
        decode(input, options = {}) {
            let bytes = input === undefined ? new Uint8Array(0) : bytesOf(input);
            if (this.#pending.length > 0) {
                const joined = new Uint8Array(this.#pending.length + bytes.length);
                joined.set(this.#pending);
                joined.set(bytes, this.#pending.length);
                bytes = joined;
            }
            const stream = Boolean(options.stream);
            const keep = stream ? incompleteTail(bytes) : 0;
            this.#pending = bytes.slice(bytes.length - keep);
            const complete = bytes.slice(0, bytes.length - keep);
            let text = native.decodeUtf8(complete.buffer, this.#fatal);
            if (!this.#started && !this.#ignoreBOM && text.startsWith('﻿')) {
                text = text.slice(1);
            }
            // Only the stream's first bytes may be a BOM, even when a chunk
            // was nothing but one.
            this.#started = stream && (this.#started || complete.length > 0);
            return text;
        }
    }

    const domError = (name, message) => Object.assign(new Error(message), { name });
    const INTEGER_ARRAYS = [
        Int8Array,
        Uint8Array,
        Uint8ClampedArray,
        Int16Array,
        Uint16Array,
        Int32Array,
        Uint32Array,
        BigInt64Array,
        BigUint64Array,
    ];

    const crypto = {
        getRandomValues(array) {
            if (!INTEGER_ARRAYS.some((type) => array instanceof type)) {
                throw domError(
                    'TypeMismatchError',
                    'getRandomValues takes an integer-typed array',
                );
            }
            if (array.byteLength > 65536) {
                throw domError(
                    'QuotaExceededError',
                    `getRandomValues takes at most 65536 bytes, not ${array.byteLength}`,
                );
            }
            bytesOf(array).set(new Uint8Array(native.randomBytes(array.byteLength)));
            return array;
        },
        randomUUID() {
            const bytes = new Uint8Array(native.randomBytes(16));
            bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
            bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 9562 variant
            const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
            return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
        },
    };

    // Loaded with the context, so they're in the preinitialized snapshot:
    // loading them on first use instead costs about 8 ms per instance.
    // ponytail: the snapshot is ~0.7 MB bigger and a fresh instance ~18 µs
    // slower (#953); if JS cold start matters, compile url_globals.js to
    // QuickJS bytecode at build time and load that on first use instead.
    define('URL', native.url.URL);
    define('URLSearchParams', native.url.URLSearchParams);
    // core-js keeps its shared state on this global while it loads; its
    // modules hold their own reference, so the global itself can go.
    delete globalThis['__core-js_shared__'];

    define('TextEncoder', TextEncoder);
    define('TextDecoder', TextDecoder);
    define('crypto', crypto);
};
