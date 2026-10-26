/**
 * IP address parsing shared by the network transports.
 */

/** Bytes of an IPv4 address in dotted decimal, or null. */
export function ipv4Bytes(addr) {
  const parts = addr.split(".");
  if (parts.length !== 4 || !parts.every((p) => /^\d{1,3}$/.test(p) && Number(p) <= 255)) return null;
  return parts.map(Number);
}

/** Bytes of an IPv6 address (with an optional dotted IPv4 tail), or null. */
export function ipv6Bytes(addr) {
  const tail = /^(.*:)(\d+\.\d+\.\d+\.\d+)$/.exec(addr);
  if (tail) {
    const b = ipv4Bytes(tail[2]);
    if (!b) return null;
    addr = `${tail[1]}${((b[0] << 8) | b[1]).toString(16)}:${((b[2] << 8) | b[3]).toString(16)}`;
  }
  const halves = addr.split("::");
  if (halves.length > 2) return null;
  const groups = (s) => (s ? s.split(":") : []);
  const head = groups(halves[0]);
  const rest = halves.length === 2 ? groups(halves[1]) : [];
  const fill = 8 - head.length - rest.length;
  if (halves.length === 2 ? fill < 1 : fill !== 0) return null;
  const all = [...head, ...Array(halves.length === 2 ? fill : 0).fill("0"), ...rest];
  if (!all.every((g) => /^[0-9a-f]{1,4}$/i.test(g))) return null;
  return all.flatMap((g) => [parseInt(g, 16) >> 8, parseInt(g, 16) & 255]);
}

/**
 * The bytes of the IPv4 address that IPv6 address bytes `b` stand for on the
 * wire, or null, like the Rust crate's `embedded_ipv4`: IPv4-mapped
 * (`::ffff:a.b.c.d`), IPv4-compatible (`::a.b.c.d`), well-known-prefix NAT64
 * (`64:ff9b::a.b.c.d`, RFC 6052), or the client address of a Teredo address
 * (`2001::/32`, RFC 4380: the last 32 bits, inverted).
 */
export function embeddedIPv4(b) {
  const zero = (from, to) => b.slice(from, to).every((x) => x === 0);
  if (zero(0, 10) && b[10] === 255 && b[11] === 255) return b.slice(12);
  const nat64 = b[0] === 0 && b[1] === 0x64 && b[2] === 0xff && b[3] === 0x9b;
  if ((nat64 || zero(0, 4)) && zero(4, 12)) return b.slice(12);
  if (b[0] === 0x20 && b[1] === 0x01 && b[2] === 0 && b[3] === 0) return b.slice(12).map((x) => ~x & 255);
  return null;
}
