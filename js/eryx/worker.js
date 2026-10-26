/**
 * Worker entry for a Sandbox created with `timeoutMs` (see index.js).
 *
 * Runs one sandbox instance off the creating thread, so that thread can
 * terminate the Worker when a call overruns its deadline. Callbacks, output,
 * traces and networking are forwarded to the creating thread, which keeps the
 * handlers, the journal and the network policy.
 *
 * Protocol (all messages are plain objects):
 * - in:  {type: "call", id, method, args, callbacks, options}
 *        {type: "reply", id, value} | {type: "reply", id, error}
 * - out: {type: "ready"} | {type: "failed", error}
 *        {type: "result", id, value} | {type: "result", id, error}
 *        {type: "request", id, target, args}  awaits a "reply"
 *        {type: "notify", target, args}       no reply
 */

import { _fileTree, decodeError, encodeError, instantiateSandbox, isNode } from "./instance.js";
import { sleep } from "./shims/callbacks.js";

const port = isNode ? (await import("node:worker_threads")).parentPort : globalThis;
const post = (message) => port.postMessage(message);
const onMessage = (fn) =>
  isNode ? port.on("message", fn) : port.addEventListener("message", (e) => fn(e.data));

let nextId = 0;
const pending = new Map();

/** Run `target` on the creating thread and wait for its result. */
function request(target, args) {
  const id = nextId++;
  post({ type: "request", id, target, args });
  return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
}

const forward = (target) => (...args) => request(target, args);
const notify = (target) => (...args) => post({ type: "notify", target, args });

// Set from each call, so the sync imports answer like the host's own would.
let callbacks = [];
let options = { pythonTracing: false, reuseEmptyCallbacks: false };

const cb = {
  invoke: forward("invoke"),
  sleep,
  listCallbacks: () => callbacks,
  getExecutionOptions: () => options,
  reportTrace: notify("reportTrace"),
  reportOutput: notify("reportOutput"),
};

const net = {
  tcp: {
    connect: forward("tcp.connect"),
    read: forward("tcp.read"),
    write: forward("tcp.write"),
    close: notify("tcp.close"),
  },
  tls: {
    upgrade: forward("tls.upgrade"),
    read: forward("tls.read"),
    write: forward("tls.write"),
    close: notify("tls.close"),
  },
};

let instance;

onMessage(async (message) => {
  switch (message.type) {
    case "call":
      callbacks = message.callbacks;
      options = message.options;
      try {
        const value = await instance[message.method](...message.args);
        post({ type: "result", id: message.id, value });
      } catch (e) {
        post({ type: "result", id: message.id, ...encodeError(e) });
      }
      break;
    case "reply": {
      const { resolve, reject } = pending.get(message.id);
      pending.delete(message.id);
      if ("value" in message) resolve(message.value);
      else reject(decodeError(message));
      break;
    }
  }
});

// Instantiate as soon as the listener is up; the creating thread waits for
// "ready" before its first call.
try {
  instance = await instantiateSandbox(_fileTree, cb, {
    net,
    // Compiled by the creating thread, so workers don't compile again.
    getModule: (name) => request("module", [name]),
  });
  post({ type: "ready" });
} catch (e) {
  post({ type: "failed", ...encodeError(e) });
}
