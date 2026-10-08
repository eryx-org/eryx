/**
 * A JSON parser that keeps each value's source text.
 *
 * Callback replay needs this to store and replay callback results exactly as
 * they were returned (`1.0` stays a float, large integers stay exact), which
 * `JSON.parse` cannot do. Journal keys need no parsing: they are the guest's
 * canonical argument text, used verbatim.
 */

const SPACE = /\s*/y;
// Strings are scanned by hand in parseJson (linear by construction, and
// guest-controlled input never meets a backtracking string pattern).
const TOKEN =
  /(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)|([{}[\]:,])|(true|false|null)/y;

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
  // Returns [text consumed, string, number, punctuation, literal].
  const next = () => {
    const from = pos;
    SPACE.lastIndex = pos;
    SPACE.exec(text);
    const start = SPACE.lastIndex;
    if (text[start] === '"') {
      let end = start + 1;
      while (end < text.length && text[end] !== '"') {
        end += text[end] === "\\" ? 2 : 1;
      }
      if (end >= text.length) {
        throw new SyntaxError(`Unterminated string at position ${start}`);
      }
      pos = end + 1;
      return [text.slice(from, pos), text.slice(start, pos)];
    }
    TOKEN.lastIndex = start;
    const m = TOKEN.exec(text);
    if (!m) throw new SyntaxError(`Invalid JSON at position ${start}`);
    pos = TOKEN.lastIndex;
    return [text.slice(from, pos), undefined, m[1], m[2], m[3]];
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
