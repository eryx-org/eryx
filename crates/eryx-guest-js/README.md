# eryx-guest-js

The JavaScript guest runtime for the eryx sandbox: an implementation of the
`sandbox` world (`crates/eryx-runtime/wit/runtime.wit`) on QuickJS
(quickjs-ng, via `rquickjs`).

It is built like the CPython guest: a PIC staticlib for wasm32-wasip1, linked
with WASI SDK clang into `liberyx_js_runtime.so`, then into a component with
wit-dylib bindings and the vendored libc. Both guests share the wit-dylib
value stack in `eryx-guest-call`.

```sh
mise run build-runtime-javascript       # crates/eryx-runtime/runtime-javascript.wasm
mise run precompile-runtime-javascript  # ... and runtime-javascript.cwasm
cargo nextest run -p eryx --features embedded,javascript --test js_guest
```

The JavaScript the sandboxed code sees (`console`, `invoke`, one function per
callback, `listCallbacks`, the result variable) is set up by `src/prelude.js`;
`src/js.rs` drives QuickJS's job queue and parks promises on host subtasks.
See the crate docs for the semantics and what isn't implemented yet.
