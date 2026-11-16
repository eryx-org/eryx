// node:crypto, random values only: hashes, HMAC, ciphers and keys aren't
// available in eryx yet and throw saying so.
import { Buffer } from 'node:buffer';

const { crypto } = globalThis;

export const getRandomValues = (array) => crypto.getRandomValues(array);
export const randomUUID = () => crypto.randomUUID();

export const randomBytes = (size, callback) => {
    if (!Number.isSafeInteger(size) || size < 0) {
        throw new RangeError(`The "size" argument must be a non-negative integer, not ${size}`);
    }
    const bytes = Buffer.alloc(size);
    for (let at = 0; at < size; at += 65536) {
        crypto.getRandomValues(bytes.subarray(at, Math.min(size, at + 65536)));
    }
    if (typeof callback !== 'function') return bytes;
    Promise.resolve().then(() => callback(null, bytes));
};

export const randomInt = (min, max, callback) => {
    if (max === undefined || typeof max === 'function') {
        [min, max, callback] = [0, min, max];
    }
    const range = max - min;
    if (!Number.isSafeInteger(min) || !Number.isSafeInteger(max) || range <= 0 || range > 2 ** 48) {
        throw new RangeError('randomInt needs safe integers with 0 < max - min <= 2^48');
    }
    // Rejection sampling over 48 bits, so every value is equally likely.
    const limit = 2 ** 48 - (2 ** 48 % range);
    let value;
    do {
        const bytes = crypto.getRandomValues(new Uint8Array(6));
        value = bytes.reduce((n, b) => n * 256 + b, 0);
    } while (value >= limit);
    const result = min + (value % range);
    if (typeof callback !== 'function') return result;
    Promise.resolve().then(() => callback(null, result));
};

export const webcrypto = crypto;

const unavailable = (name) => () => {
    throw new Error(`crypto.${name} isn't available in eryx yet (only random values are)`);
};
export const createHash = unavailable('createHash');
export const createHmac = unavailable('createHmac');
export const createCipheriv = unavailable('createCipheriv');
export const createDecipheriv = unavailable('createDecipheriv');
export const pbkdf2 = unavailable('pbkdf2');
export const pbkdf2Sync = unavailable('pbkdf2Sync');
export const scrypt = unavailable('scrypt');
export const scryptSync = unavailable('scryptSync');
export const generateKeyPair = unavailable('generateKeyPair');
export const createSign = unavailable('createSign');
export const createVerify = unavailable('createVerify');

export default {
    getRandomValues,
    randomUUID,
    randomBytes,
    randomInt,
    webcrypto,
    createHash,
    createHmac,
    createCipheriv,
    createDecipheriv,
    pbkdf2,
    pbkdf2Sync,
    scrypt,
    scryptSync,
    generateKeyPair,
    createSign,
    createVerify,
};
