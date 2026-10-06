/**
 * Canonical JSON for callback replay, byte-identical to the Rust journal.
 *
 * The Rust host (crates/eryx/src/replay.rs) keys journal entries on
 * `serde_json::from_str::<Value>(args)` re-serialized with object keys sorted.
 * Reproducing that exactly lets journals move between the Rust, Python and
 * JavaScript hosts without spurious misses (which would re-run callbacks).
 *
 * `JSON.parse` + `JSON.stringify` is not enough: it loses number lexemes
 * (`1.0` -> `1`, integers beyond 2^53), so this tokenizes the text itself and
 * ports serde_json 1.0.151's number handling (default features: no
 * `float_roundtrip`, no `arbitrary_precision`, so non-trivial floats go
 * through its approximate POW10 multiply and are printed by `zmij`).
 *
 * ponytail: pinned to serde_json's current number parsing/formatting; if a
 * serde_json bump changes either, update this and re-run the differential check
 * against a real serde_json build. #524 removes this port by keying every host
 * on the guest's raw canonical args JSON instead.
 */

const U64_MAX = 2n ** 64n - 1n;
const I64_MIN_ABS = 2n ** 63n;
const I32_MAX = 2 ** 31 - 1;
// serde_json's POW10 table: correctly-rounded 1e0..1e308.
const POW10 = Array.from({ length: 309 }, (_, i) => Number(`1e${i}`));

/** Port of serde_json's `f64_from_parts` (without `float_roundtrip`). */
function f64FromParts(positive, significand, exponent) {
  let f = Number(significand);
  for (;;) {
    const pow = POW10[Math.abs(exponent)];
    if (pow !== undefined) {
      if (exponent >= 0) {
        f *= pow;
        if (!Number.isFinite(f)) return null;
      } else {
        f /= pow;
      }
      break;
    }
    if (f === 0) break;
    if (exponent >= 0) return null;
    f /= 1e308;
    exponent += 308;
  }
  return positive ? f : -f;
}

/** Format an f64 as serde_json does (shortest digits, zmij layout). */
function formatF64(f) {
  if (f === 0) return Object.is(f, -0) ? "-0.0" : "0.0";
  // toExponential() with no argument yields the shortest round-trip digits.
  const [mantissa, e] = Math.abs(f).toExponential().split("e");
  const digits = mantissa.replace(".", "");
  const exp = Number(e);
  let out;
  if (exp >= -5 && exp <= 15) {
    if (digits.length - 1 <= exp) {
      out = `${digits}${"0".repeat(exp + 1 - digits.length)}.0`;
    } else if (exp >= 0) {
      out = `${digits.slice(0, exp + 1)}.${digits.slice(exp + 1)}`;
    } else {
      out = `0.${"0".repeat(-exp - 1)}${digits}`;
    }
  } else {
    const m = digits.length > 1 ? `${digits[0]}.${digits.slice(1)}` : digits;
    out = `${m}e${exp < 0 ? "-" : "+"}${Math.abs(exp)}`;
  }
  return f < 0 ? `-${out}` : out;
}

/**
 * Re-serialize a JSON number lexeme the way serde_json's `Value` would: u64 /
 * i64 when it is an in-range integer literal, otherwise an f64. A lexeme serde
 * rejects (out of range) is returned unchanged; the Rust host would fail that
 * call outright, so there is nothing to match.
 */
function canonicalNumber(source) {
  const m = /^(-?)(\d+)(?:\.(\d+))?(?:[eE]([+-]?)(\d+))?$/.exec(source);
  const [, sign, intDigits, fracDigits, expSign, expDigits] = m;
  const positive = sign === "";
  let significand = 0n;
  let exponent = 0;
  let overflowed = false;
  for (const c of intDigits) {
    const next = significand * 10n + BigInt(c);
    if (overflowed || next > U64_MAX) {
      // Like serde: drop further integer digits, bumping the exponent.
      overflowed = true;
      exponent++;
    } else {
      significand = next;
    }
  }
  if (fracDigits === undefined && expDigits === undefined && !overflowed) {
    if (positive) return significand.toString();
    if (significand === 0n) return "-0.0";
    if (significand <= I64_MIN_ABS) return `-${significand}`;
    return formatF64(-Number(significand));
  }
  for (const c of fracDigits ?? "") {
    const next = significand * 10n + BigInt(c);
    // Like serde: on overflow, ignore the remaining fraction digits.
    if (next > U64_MAX) break;
    significand = next;
    exponent--;
  }
  if (expDigits !== undefined) {
    const exp = Number(expDigits);
    if (exp > I32_MAX) {
      if (significand !== 0n && expSign !== "-") return source;
      return positive ? "0.0" : "-0.0";
    }
    exponent = expSign === "-" ? exponent - exp : exponent + exp;
    exponent = Math.max(-I32_MAX - 1, Math.min(I32_MAX, exponent));
  }
  const f = f64FromParts(positive, significand, exponent);
  return f === null ? source : formatF64(f);
}

/** Compare by Unicode code point, matching Rust's byte-wise `String` order. */
function compareCodePoints(a, b) {
  const x = Array.from(a, (c) => c.codePointAt(0));
  const y = Array.from(b, (c) => c.codePointAt(0));
  for (let i = 0; i < Math.min(x.length, y.length); i++) {
    if (x[i] !== y[i]) return x[i] - y[i];
  }
  return x.length - y.length;
}

const TOKEN =
  /\s*(?:("(?:[^"\\]|\\.)*")|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)|([{}[\]:,])|(true|false|null))/y;

/**
 * A parsed JSON value that keeps its source text.
 * @typedef {Object} JsonNode
 * @property {"string"|"number"|"literal"|"array"|"object"} kind
 * @property {*} value - Decoded string, number lexeme, literal text, array of
 *   nodes, or Map of key to node (duplicate keys: last wins)
 * @property {string} raw - The node's exact source text
 */

/**
 * Parse JSON without losing number lexemes or source text.
 *
 * (Not `JSON.parse` with a reviver's `context.source`: besides being missing on
 * older engines, it intermittently returned wrong keys and no source under
 * Node 24 during differential testing.)
 *
 * @param {string} text - A JSON document
 * @returns {JsonNode}
 * @throws {SyntaxError} If `text` is not valid JSON
 */
export function parseJson(text) {
  let pos = 0;
  const next = () => {
    TOKEN.lastIndex = pos;
    const m = TOKEN.exec(text);
    if (!m) throw new SyntaxError(`Invalid JSON at position ${pos}`);
    pos = TOKEN.lastIndex;
    return m;
  };
  const expect = (m, punct) => {
    if (m[3] !== punct) {
      throw new SyntaxError(`Expected '${punct}' at position ${pos}`);
    }
  };
  const node = (m) => {
    const start = pos - m[0].trimStart().length;
    const done = (kind, value) => ({
      kind,
      value,
      raw: text.slice(start, pos),
    });
    if (m[1]) return done("string", JSON.parse(m[1]));
    if (m[2]) return done("number", m[2]);
    if (m[4]) return done("literal", m[4]);
    if (m[3] === "[") {
      const items = [];
      for (let t = next(); t[3] !== "]";) {
        if (items.length) {
          expect(t, ",");
          t = next();
        }
        items.push(node(t));
        t = next();
      }
      return done("array", items);
    }
    if (m[3] === "{") {
      const entries = new Map();
      let first = true;
      for (let t = next(); t[3] !== "}"; first = false) {
        if (!first) {
          expect(t, ",");
          t = next();
        }
        if (!t[1]) throw new SyntaxError(`Expected a key at position ${pos}`);
        const key = JSON.parse(t[1]);
        expect(next(), ":");
        entries.set(key, node(next()));
        t = next();
      }
      return done("object", entries);
    }
    throw new SyntaxError(`Unexpected token at position ${pos}`);
  };
  const root = node(next());
  if (text.slice(pos).trim() !== "") {
    throw new SyntaxError(`Unexpected data at position ${pos}`);
  }
  return root;
}

/**
 * Serialize a node canonically: compact, object keys sorted, numbers as
 * serde_json would print them.
 * @param {JsonNode} node
 * @returns {string}
 */
function canonical(node) {
  switch (node.kind) {
    case "string":
      return JSON.stringify(node.value);
    case "number":
      return canonicalNumber(node.value);
    case "literal":
      return node.value;
    case "array":
      return `[${node.value.map(canonical).join(",")}]`;
    default: {
      const keys = [...node.value.keys()].sort(compareCodePoints);
      return `{${keys.map((k) => `${JSON.stringify(k)}:${canonical(node.value.get(k))}`).join(",")}}`;
    }
  }
}

/**
 * Canonicalize a JSON document the way the Rust journal keys arguments.
 *
 * @param {string} text - A JSON document
 * @returns {string}
 * @throws {SyntaxError} If `text` is not valid JSON
 */
export function canonicalJson(text) {
  return canonical(parseJson(text));
}
