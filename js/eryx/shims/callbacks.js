/**
 * Callback shims for the eryx sandbox.
 *
 * These provide the host-side implementations of the sandbox's callback imports:
 * - invoke: call a registered callback by name with JSON arguments
 * - listCallbacks: list all registered callbacks
 * - getExecutionOptions: return per-execution behavior options
 * - reportTrace: receive trace events from the Python runtime
 * - reportOutput: receive streaming stdout/stderr output from the Python runtime
 *
 * Users can register callbacks via setCallbackHandler(), setTraceHandler(),
 * and setOutputHandler().
 */

import { canonicalJson, parseJson } from "./canonical-json.js";

/** @type {((name: string, argsJson: string) => string | Promise<string>) | null} */
let _callbackHandler = null;

/** @type {((lineno: number, eventJson: string, contextJson: string) => void) | null} */
let _traceHandler = null;

/** @type {((stream: number, data: string) => void) | null} */
let _outputHandler = null;

/** @type {Array<{name: string, description: string, parametersSchemaJson: string}>} */
let _registeredCallbacks = [];

/**
 * Set the callback handler for sandbox code to invoke.
 *
 * The handler receives a callback name and JSON-encoded arguments,
 * and should return a JSON-encoded result (or a Promise of one).
 *
 * @param {((name: string, argsJson: string) => string | Promise<string>) | null} handler
 */
export function setCallbackHandler(handler) {
  _callbackHandler = handler;
}

/**
 * Register callbacks that will be visible to sandbox code via list_callbacks().
 *
 * @param {Array<{name: string, description: string, parametersSchemaJson?: string}>} callbacks
 */
export function setCallbacks(callbacks) {
  _registeredCallbacks = callbacks.map((cb) => ({
    name: cb.name,
    description: cb.description,
    parametersSchemaJson: cb.parametersSchemaJson ?? "{}",
  }));
}

/**
 * Set a handler for trace events from the Python runtime.
 *
 * @param {((lineno: number, eventJson: string, contextJson: string) => void) | null} handler
 */
export function setTraceHandler(handler) {
  _traceHandler = handler;
}

/**
 * Set a handler for streaming output (stdout/stderr) from the Python runtime.
 *
 * The handler is called in real-time as Python code writes to stdout or stderr,
 * rather than waiting for execution to complete.
 *
 * @param {((stream: number, data: string) => void) | null} handler
 *   stream: 0 = stdout, 1 = stderr
 *   data: the text written
 */
export function setOutputHandler(handler) {
  _outputHandler = handler;
  _resetOutputDecoders();
}

/**
 * Thrown by a callback handler to suspend execution ("retry later").
 *
 * The guest halts immediately: no further Python runs and no further callbacks
 * dispatch. Under executeWithJournal() the suspension is surfaced as
 * `outcome.suspended` and the call is not journaled, so it re-runs live when the
 * recorded journal is replayed. Under plain execute() it rejects like any error.
 */
export class SuspendCallback extends Error {
  /** @param {string} reason - Opaque reason, surfaced to the caller */
  constructor(reason) {
    super(reason);
    this.name = "SuspendCallback";
    this.reason = reason;
  }
}

/**
 * A recorded callback outcome. `ok` is the handler's exact JSON result text (not
 * re-parsed, so replay returns byte-for-byte what Python saw live); `err` is a
 * Python-visible error message, or the message of a thrown Error when `thrown`.
 * @typedef {{ok: string} | {err: string, thrown?: boolean}} RecordedResult
 */

/**
 * Replay state for the current executeWithJournal() run, or null when not
 * journaling. Mirrors the Rust `ReplayState` (crates/eryx/src/replay.rs): cached
 * results bucketed by (name, canonical args) as a FIFO multiset, a sticky
 * divergence guard, and a gate that rejects every call after a suspension.
 * @type {{cached: Map<string, RecordedResult[]>, liveMode: boolean, nextSeq: number, entries: Array<{name: string, argsJson: string, result: RecordedResult}|undefined>, suspended: Object|null, suspendSeq: number|null, replayedCount: number}|null}
 */
let _replay = null;

/**
 * Parse a serialized journal into its entries.
 *
 * Parsed with {@link parseJson} rather than `JSON.parse` so each `Ok` value
 * keeps its exact text (`1.0` stays a float, large integers stay exact).
 * @param {string} journal
 * @returns {Array<{name: string, argsJson: string, result: RecordedResult}>}
 */
function _parseJournal(journal) {
  if (typeof journal !== "string") {
    throw new TypeError(
      "journal must be the JSON string from a previous executeWithJournal() outcome",
    );
  }
  const invalid = () =>
    new TypeError("journal is not a valid callback journal");
  const field = (node, key, kind) => {
    const value = node?.kind === "object" ? node.value.get(key) : undefined;
    if (value?.kind !== kind) throw invalid();
    return value;
  };
  const entries = field(parseJson(journal), "entries", "array").value;
  return entries.map((entry) => {
    const result = field(entry, "result", "object").value;
    const ok = result.get("Ok");
    const err = result.get("Err");
    if (!(ok || err?.kind === "string")) throw invalid();
    return {
      name: field(entry, "name", "string").value,
      argsJson: field(entry, "args_json", "string").value,
      result: ok
        ? { ok: ok.raw }
        : { err: err.value, thrown: entry.value.get("thrown")?.raw === "true" },
    };
  });
}

/**
 * Start journaling callbacks, replaying results from `journal` if given.
 * @internal Used by executeWithJournal().
 * @param {string} [journal] - A journal string from a previous outcome
 */
export function _beginReplay(journal) {
  const cached = new Map();
  for (const entry of journal === undefined ? [] : _parseJournal(journal)) {
    const key = `${entry.name}\0${entry.argsJson}`;
    if (!cached.has(key)) cached.set(key, []);
    cached.get(key).push(entry.result);
  }
  _replay = {
    cached,
    liveMode: false,
    nextSeq: 0,
    entries: [],
    suspended: null,
    suspendSeq: null,
    replayedCount: 0,
  };
}

/** 64-bit FNV-1a, matching the Rust journal's `args_hash`. */
function _fnv1a64(text) {
  let hash = 0xcbf29ce484222325n;
  for (const byte of new TextEncoder().encode(text)) {
    hash = BigInt.asUintN(64, (hash ^ BigInt(byte)) * 0x100000001b3n);
  }
  return hash;
}

/**
 * Stop journaling and return what was recorded.
 *
 * The journal is serialized in the Rust `CallbackJournal` serde shape. Entries
 * keep dispatch order and, if a callback suspended, everything dispatched at or
 * after the suspending call is dropped so the journal is a clean prefix ending
 * before the suspension point.
 * @internal Used by executeWithJournal().
 * @param {string} code - The script that produced the journal
 */
export function _endReplay(code) {
  const state = _replay;
  _replay = null;
  const entries = [];
  state.entries.forEach((entry, index) => {
    if (entry === undefined) return;
    if (state.suspendSeq !== null && index >= state.suspendSeq) return;
    const { name, argsJson, result } = entry;
    const recorded =
      "ok" in result
        ? `{"Ok":${result.ok}}`
        : `{"Err":${JSON.stringify(result.err)}}`;
    // `thrown` is a JS-only extension (serde ignores unknown fields): the Error
    // halted the guest instead of reaching Python, so replay throws it again.
    const thrown = result.thrown ? `,"thrown":true` : "";
    entries.push(
      `{"index":${index},"name":${JSON.stringify(name)},"args_hash":${_fnv1a64(argsJson)},` +
        `"args_json":${JSON.stringify(argsJson)},"result":${recorded}${thrown}}`,
    );
  });
  return {
    journal: `{"code":${JSON.stringify(code)},"entries":[${entries.join(",")}]}`,
    replayedCallbacks: state.replayedCount,
    suspended: state.suspended ?? undefined,
  };
}

/** Deliver a recorded result to the guest exactly as it was delivered live. */
function _replayResult(result) {
  if ("ok" in result) return result.ok;
  if (result.thrown) throw new Error(result.err);
  return { tag: "err", val: result.err };
}

/**
 * Run a live callback, recording its outcome into `state` at `seq`.
 *
 * Every outcome of a call that actually ran is journaled, so replay never runs
 * it twice: a JSON result as `Ok` (its exact text), an `{tag: "err"}` result or
 * thrown non-Error as `Err`, and a thrown Error as `Err` marked `thrown` (it
 * halts the guest; replay halts it the same way). Only a SuspendCallback is not
 * journaled: the call re-runs live on resume, as in Rust.
 */
async function _invokeLive(state, seq, name, argsJson, argumentsJson) {
  const record = (result) => {
    state.entries[seq] = { name, argsJson, result };
  };
  let ret;
  try {
    ret = await _callbackHandler(name, argumentsJson);
  } catch (e) {
    if (e instanceof SuspendCallback) {
      if (state.suspended === null) {
        state.suspended = { name, argsJson, reason: e.reason };
        state.suspendSeq = seq;
      }
    } else if (e instanceof Error) {
      record({ err: e.message, thrown: true });
    } else {
      record({ err: String(e) });
    }
    throw e;
  }
  if (ret !== null && typeof ret === "object" && ret.tag === "err") {
    record({ err: String(ret.val) });
  } else {
    const json = ret !== null && typeof ret === "object" ? ret.val : ret;
    try {
      parseJson(json);
      record({ ok: json });
    } catch {
      // ponytail: a non-JSON result breaks the handler contract and can't be
      // stored as an `Ok` value, so it stays unjournaled (re-runs on replay).
    }
  }
  return ret;
}

/**
 * Invoke a callback by name with JSON arguments.
 * This is called by the sandbox runtime when Python code calls invoke().
 *
 * @param {string} name - Callback name
 * @param {string} argumentsJson - JSON-encoded arguments
 * @returns {string} JSON-encoded result
 */
export function invoke(name, argumentsJson) {
  if (!_callbackHandler) {
    throw new Error(
      `No callback handler registered. Call setCallbackHandler() before executing code that uses callbacks. Attempted to invoke: ${name}`,
    );
  }
  const state = _replay;
  if (!state) {
    return _callbackHandler(name, argumentsJson);
  }

  // Decide synchronously, before any await, exactly as the Rust ReplayCallback.
  if (state.suspended !== null) {
    throw new SuspendCallback(
      "execution already suspended by a previous callback",
    );
  }
  const argsJson = canonicalJson(argumentsJson);
  const seq = state.nextSeq++;
  if (!state.liveMode) {
    const result = state.cached.get(`${name}\0${argsJson}`)?.shift();
    if (result !== undefined) {
      state.replayedCount++;
      state.entries[seq] = { name, argsJson, result };
      return _replayResult(result);
    }
    // First miss: the run has diverged from the journal, so this call and every
    // later one runs live (prevents replaying a now-stale cached result).
    state.liveMode = true;
  }
  return _invokeLive(state, seq, name, argsJson, argumentsJson);
}

/**
 * List all registered callbacks.
 * This is called by the sandbox runtime when Python code calls list_callbacks().
 *
 * @returns {Array<{name: string, description: string, parametersSchemaJson: string}>}
 */
export function listCallbacks() {
  return _registeredCallbacks;
}

/**
 * Return execution behavior options for the JavaScript host.
 *
 * JavaScript instances may execute repeatedly with different callbacks, so
 * callback setup is not reused. Tracing is enabled only while a trace handler
 * is registered.
 *
 * @returns {{pythonTracing: boolean, reuseEmptyCallbacks: boolean}}
 */
export function getExecutionOptions() {
  return {
    pythonTracing: _traceHandler !== null,
    reuseEmptyCallbacks: false,
  };
}

/**
 * Report a trace event from the Python runtime.
 * This is called by the sandbox runtime's sys.settrace hook.
 *
 * @param {number} lineno - Line number
 * @param {string} eventJson - Event type as JSON
 * @param {string} contextJson - Context data as JSON
 */
export function reportTrace(lineno, eventJson, contextJson) {
  if (_traceHandler) {
    _traceHandler(lineno, eventJson, contextJson);
  }
}

const _decoderOpts = { fatal: false, ignoreBOM: true };
let _stdoutDecoder = new TextDecoder("utf-8", _decoderOpts);
let _stderrDecoder = new TextDecoder("utf-8", _decoderOpts);

/**
 * Reset streaming decoders so incomplete multi-byte sequences from one
 * execution don't leak into the next. Called from setOutputHandler.
 */
function _resetOutputDecoders() {
  _stdoutDecoder = new TextDecoder("utf-8", _decoderOpts);
  _stderrDecoder = new TextDecoder("utf-8", _decoderOpts);
}

/**
 * Report streaming output from the Python runtime.
 * This is called by the sandbox runtime on every sys.stdout/stderr.write().
 *
 * @param {number} stream - 0 = stdout, 1 = stderr
 * @param {Uint8Array} data - The raw bytes written
 */
export function reportOutput(stream, data) {
  if (_outputHandler) {
    const decoder = stream === 0 ? _stdoutDecoder : _stderrDecoder;
    _outputHandler(stream, decoder.decode(data, { stream: true }));
  }
}
