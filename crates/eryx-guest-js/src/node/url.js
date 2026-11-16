// node:url: the WHATWG `URL` and `URLSearchParams` globals, Node's
// helpers on them, and a subset of the legacy API (`parse`, `format`,
// `resolve`). Written for eryx.
import { posix } from 'node:path';

export const { URL, URLSearchParams } = globalThis;

const invalidUrl = (message) => Object.assign(new TypeError(message), { code: 'ERR_INVALID_URL' });

export const fileURLToPath = (url) => {
    const parsed = typeof url === 'string' ? new URL(url) : url;
    if (!(parsed instanceof URL)) throw new TypeError('The "url" argument must be a string or URL');
    if (parsed.protocol !== 'file:') throw invalidUrl('The URL must be of scheme file');
    if (parsed.hostname !== '' && parsed.hostname !== 'localhost') {
        throw invalidUrl(`File URL host must be "localhost" or empty on this platform`);
    }
    if (/%2f/i.test(parsed.pathname)) throw invalidUrl('File URL path must not include encoded / characters');
    return decodeURIComponent(parsed.pathname);
};

export const pathToFileURL = (path) => {
    let resolved = posix.resolve(String(path));
    // posix.resolve drops a trailing slash, which Node keeps.
    if (String(path).endsWith('/') && !resolved.endsWith('/')) resolved += '/';
    const url = new URL('file://');
    url.pathname = resolved.replace(/%/g, '%25').replace(/\n/g, '%0A').replace(/\r/g, '%0D').replace(/\t/g, '%09');
    return url;
};

export const urlToHttpOptions = (url) => {
    const options = {
        protocol: url.protocol,
        hostname: url.hostname.startsWith('[') ? url.hostname.slice(1, -1) : url.hostname,
        hash: url.hash,
        search: url.search,
        pathname: url.pathname,
        path: `${url.pathname || ''}${url.search || ''}`,
        href: url.href,
    };
    if (url.port !== '') options.port = Number(url.port);
    if (url.username || url.password) {
        options.auth = `${decodeURIComponent(url.username)}:${decodeURIComponent(url.password)}`;
    }
    return options;
};

export const domainToASCII = (domain) => {
    try {
        return new URL(`http://${domain}`).hostname;
    } catch {
        return '';
    }
};

// Punycode labels back to Unicode (RFC 3492), for domainToUnicode.
const decodePunycode = (input) => {
    const base = 36;
    let n = 128;
    let bias = 72;
    let i = 0;
    const output = [];
    const basic = input.lastIndexOf('-');
    for (let j = 0; j < Math.max(basic, 0); j++) output.push(input.charCodeAt(j));
    for (let index = basic > 0 ? basic + 1 : 0; index < input.length; ) {
        const oldi = i;
        for (let w = 1, k = base; ; k += base) {
            if (index >= input.length) throw new RangeError('invalid punycode');
            const c = input.charCodeAt(index++);
            const digit =
                c >= 48 && c <= 57 ? c - 22 : c >= 65 && c <= 90 ? c - 65 : c >= 97 && c <= 122 ? c - 97 : base;
            if (digit >= base) throw new RangeError('invalid punycode');
            i += digit * w;
            const t = k <= bias ? 1 : k >= bias + 26 ? 26 : k - bias;
            if (digit < t) break;
            w *= base - t;
        }
        const length = output.length + 1;
        let delta = oldi === 0 ? Math.floor((i - oldi) / 700) : (i - oldi) >> 1;
        delta += Math.floor(delta / length);
        let k = 0;
        for (; delta > 455; k += base) delta = Math.floor(delta / 35);
        bias = Math.floor(k + (36 * delta) / (delta + 38));
        n += Math.floor(i / length);
        i %= length;
        output.splice(i++, 0, n);
    }
    return String.fromCodePoint(...output);
};

export const domainToUnicode = (domain) => {
    const ascii = domainToASCII(domain);
    if (ascii === '') return '';
    try {
        return ascii
            .split('.')
            .map((label) => (label.startsWith('xn--') ? decodePunycode(label.slice(4)) : label))
            .join('.');
    } catch {
        return '';
    }
};

// The legacy `url.parse` result. Fields Node leaves null stay null.
class Url {
    constructor() {
        this.protocol = null;
        this.slashes = null;
        this.auth = null;
        this.host = null;
        this.port = null;
        this.hostname = null;
        this.hash = null;
        this.search = null;
        this.query = null;
        this.pathname = null;
        this.path = null;
        this.href = null;
    }
}

const queryOf = (search, parseQueryString) => {
    if (!parseQueryString) return search ? search.slice(1) : search;
    return Object.fromEntries(new URLSearchParams(search ?? ''));
};

// A subset of the legacy parser: absolute URLs go through the WHATWG
// parser, anything else is split into path, query and hash.
export const parse = (input, parseQueryString = false) => {
    const text = String(input).trim();
    const result = new Url();
    let parsed;
    try {
        parsed = /^[a-z][a-z0-9+.-]*:/i.test(text) ? new URL(text) : undefined;
    } catch {
        parsed = undefined;
    }
    if (parsed) {
        result.protocol = parsed.protocol;
        result.slashes = /^[a-z][a-z0-9+.-]*:\/\//i.test(text) || null;
        if (parsed.username || parsed.password) {
            result.auth = decodeURIComponent(parsed.password ? `${parsed.username}:${parsed.password}` : parsed.username);
        }
        if (parsed.host) {
            result.host = parsed.host;
            result.hostname = parsed.hostname;
            result.port = parsed.port || null;
        }
        result.pathname = parsed.pathname || (parsed.host ? '/' : null);
        result.search = parsed.search || null;
        result.hash = parsed.hash || null;
    } else {
        const hashAt = text.indexOf('#');
        const beforeHash = hashAt === -1 ? text : text.slice(0, hashAt);
        result.hash = hashAt === -1 ? null : text.slice(hashAt);
        const queryAt = beforeHash.indexOf('?');
        result.pathname = (queryAt === -1 ? beforeHash : beforeHash.slice(0, queryAt)) || null;
        result.search = queryAt === -1 ? null : beforeHash.slice(queryAt);
    }
    result.query = queryOf(result.search === '?' ? '' : result.search, parseQueryString);
    if (!parseQueryString && result.search === null) result.query = null;
    result.path = result.pathname === null && result.search === null ? null : `${result.pathname ?? ''}${result.search ?? ''}`;
    result.href = format(result);
    return result;
};

// A URL, or a legacy object like `parse`'s.
export const format = (object) => {
    if (typeof object === 'string') object = parse(object);
    if (object instanceof URL) return object.href;
    const protocol = object.protocol ? (object.protocol.endsWith(':') ? object.protocol : `${object.protocol}:`) : '';
    const auth = object.auth ? `${encodeURIComponent(object.auth).replace(/%3A/i, ':')}@` : '';
    let host = '';
    if (object.host) {
        host = object.host;
    } else if (object.hostname) {
        host = object.hostname.includes(':') ? `[${object.hostname}]` : object.hostname;
        if (object.port) host += `:${object.port}`;
    }
    const slashes = object.slashes || host ? '//' : '';
    let search = object.search ?? '';
    if (!search && object.query && typeof object.query === 'object') {
        const query = new URLSearchParams(object.query).toString();
        search = query ? `?${query}` : '';
    }
    if (search && !search.startsWith('?')) search = `?${search}`;
    let pathname = object.pathname ?? '';
    if (host && pathname && !pathname.startsWith('/')) pathname = `/${pathname}`;
    let hash = object.hash ?? '';
    if (hash && !hash.startsWith('#')) hash = `#${hash}`;
    return `${protocol}${slashes}${auth}${host}${pathname}${search}${hash}`;
};

// As Node's docs suggest: resolve against a placeholder origin when `from`
// is relative, and take the placeholder off again.
export const resolve = (from, to) => {
    const resolved = new URL(to, new URL(from, 'resolve://'));
    if (resolved.protocol === 'resolve:') {
        const { pathname, search, hash } = resolved;
        return pathname + search + hash;
    }
    return resolved.toString();
};

export default {
    URL,
    URLSearchParams,
    fileURLToPath,
    pathToFileURL,
    urlToHttpOptions,
    domainToASCII,
    domainToUnicode,
    parse,
    format,
    resolve,
    Url,
};
