/**
 * Network shims for the eryx sandbox.
 *
 * These provide stub implementations for TCP and TLS imports.
 * Networking is not currently supported in the JS bindings -
 * all operations return "not permitted" errors.
 *
 * Future work: implement networking via fetch() or WebSocket bridges.
 */

export const tcp = {
  connect(_host, _port) {
    return { tag: "err", val: { tag: "not-permitted", val: "networking is not available in the JavaScript bindings" } };
  },
  read(_handle, _len) {
    return { tag: "err", val: { tag: "invalid-handle" } };
  },
  write(_handle, _data) {
    return { tag: "err", val: { tag: "invalid-handle" } };
  },
  close(_handle) {},
};

export const tls = {
  upgrade(_tcp, _hostname) {
    return { tag: "err", val: { tag: "tcp", val: { tag: "not-permitted", val: "networking is not available in the JavaScript bindings" } } };
  },
  read(_handle, _len) {
    return { tag: "err", val: { tag: "invalid-handle" } };
  },
  write(_handle, _data) {
    return { tag: "err", val: { tag: "invalid-handle" } };
  },
  close(_handle) {},
};
