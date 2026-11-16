//! Node compatibility in the QuickJS guest (#827): the `crypto`,
//! `TextEncoder` and `TextDecoder` globals, and the `node:*` built-ins eryx
//! provides (`crates/eryx-guest-js/src/node/`). None of it needs an
//! environment.
//!
//! ```sh
//! mise run precompile-runtime-javascript
//! cargo nextest run -p eryx --features embedded,javascript --test js_node
//! ```
#![cfg(all(feature = "embedded", feature = "javascript"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use eryx::{Sandbox, SessionExecutor};

async fn run(code: &str) -> Result<String, eryx::Error> {
    let sandbox = Sandbox::embedded().javascript().build().unwrap();
    sandbox.execute(code).await.map(|out| out.stdout_text())
}

async fn ok(code: &str) -> String {
    run(code).await.unwrap_or_else(|e| panic!("{code}: {e}"))
}

#[tokio::test]
async fn crypto_random_values() {
    let out = ok(r"
        const uuid = crypto.randomUUID();
        console.log(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(uuid));
        console.log(uuid !== crypto.randomUUID());
        const a = crypto.getRandomValues(new Uint32Array(8));
        const b = crypto.getRandomValues(new Uint32Array(8));
        console.log(a.some((x) => x !== 0), a.join() !== b.join());
        const view = new Uint8Array(new ArrayBuffer(8), 2, 4);
        crypto.getRandomValues(view);
        console.log(new Uint8Array(view.buffer).slice(0, 2).join(), new Uint8Array(view.buffer).slice(6).join());
        for (const bad of [() => crypto.getRandomValues(new Float64Array(1)), () => crypto.getRandomValues(new Uint8Array(65537))]) {
            try { bad(); } catch (e) { console.log(e.name); }
        }
        ")
    .await;
    assert_eq!(
        out,
        "true\ntrue\ntrue true\n0,0 0,0\nTypeMismatchError\nQuotaExceededError\n"
    );
}

/// Each instance draws its own values, including ones started from the same
/// preinitialized snapshot.
#[tokio::test]
async fn instances_draw_different_values() {
    let first = ok("console.log(crypto.randomUUID())").await;
    let second = ok("console.log(crypto.randomUUID())").await;
    assert_ne!(first, second);
}

#[tokio::test]
async fn text_encoder_and_decoder() {
    let out = ok(r"
        const bytes = new TextEncoder().encode('héllo 🌍');
        console.log(bytes.length, new TextDecoder().decode(bytes));
        // A character split across chunks, decoded as a stream.
        const decoder = new TextDecoder();
        console.log(decoder.decode(bytes.slice(0, 2), { stream: true }) + decoder.decode(bytes.slice(2)));
        console.log(new TextDecoder().decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x61])));
        // Lone surrogates encode as U+FFFD.
        console.log(new TextEncoder().encode('a\uD800b').join(), new TextEncoder().encode('🌍'.slice(0, 1)).join());
        // encodeInto stops before a sequence that doesn't fit whole.
        const into = new Uint8Array(4);
        console.log(JSON.stringify(new TextEncoder().encodeInto('abc€', into)), into.join());
        console.log(JSON.stringify(new TextEncoder().encodeInto('a\uD800', new Uint8Array(8))));
        // Only the stream's first bytes can be a BOM, even a BOM-only chunk.
        const bom = new TextDecoder();
        const twice = bom.decode(new Uint8Array([0xef, 0xbb, 0xbf]), { stream: true }) + bom.decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x62]));
        console.log(twice.length, twice.charCodeAt(0));
        try { new TextDecoder('utf-8', { fatal: true }).decode(new Uint8Array([0xff])); } catch (e) { console.log(e.name); }
        try { new TextDecoder('latin1'); } catch (e) { console.log(e.name); }
        ")
    .await;
    assert_eq!(
        out,
        "11 héllo 🌍\nhéllo 🌍\na\n97,239,191,189,98 239,191,189\n\
         {\"read\":3,\"written\":3} 97,98,99,0\n{\"read\":2,\"written\":4}\n\
         2 65279\nTypeError\nRangeError\n"
    );
}

#[tokio::test]
async fn buffer_events_path_and_string_decoder() {
    let out = ok(r"
        import { Buffer } from 'node:buffer'
        import { EventEmitter, once } from 'events'
        import path from 'node:path'
        import { StringDecoder } from 'string_decoder'
        const encoded = Buffer.from('héllo').toString('base64');
        console.log(encoded, Buffer.from(encoded, 'base64').toString());
        class Bus extends require('events') {}
        const bus = new Bus();
        bus.on('ping', (n) => console.log('ping', n));
        bus.emit('ping', 1);
        console.log(bus instanceof EventEmitter);
        const later = once(bus, 'done');
        bus.emit('done', 'ok');
        console.log((await later)[0]);
        console.log(path.join('/a', '../b', 'c.txt'), path.extname('x.tar.gz'), require('path/posix').sep);
        const euro = Buffer.from('€');
        const decoder = new StringDecoder('utf8');
        console.log(decoder.write(euro.subarray(0, 1)) + decoder.write(euro.subarray(1)) + decoder.end());
        // string_decoder's Buffer is node:buffer's.
        console.log(require('buffer').Buffer === Buffer);
        ")
    .await;
    assert_eq!(
        out,
        "aMOpbGxv héllo\nping 1\ntrue\nok\n/b/c.txt .gz /\n€\ntrue\n"
    );
}

#[tokio::test]
async fn util_and_crypto_modules() {
    let out = ok(r"
        import util from 'node:util'
        import { randomBytes, randomInt, createHash } from 'node:crypto'
        import { Buffer } from 'node:buffer'
        console.log(util.format('%s=%d %j', 'n', 42, { a: [1] }), util.inspect({ a: 'x', b: [1, { c: null }] }));
        const wait = util.promisify((value, done) => done(null, value * 2));
        console.log(await wait(21));
        function Base() {} Base.prototype.hi = () => 'hi';
        function Child() {} util.inherits(Child, Base);
        console.log(new Child().hi(), util.types.isPromise(Promise.resolve()), util.isDeepStrictEqual({ a: [1] }, { a: [1] }));
        const same = (a, b) => util.isDeepStrictEqual(a, b);
        console.log(
            same(new Date(1), new Date(2)), same(new Map([[1, 'a']]), new Map([[1, 'b']])),
            same(new Set([1]), new Set([2])), same(/a/g, /a/i), same(new Uint8Array([1]), new Uint8Array([2])),
            same(new Map([[1, { x: 1 }]]), new Map([[1, { x: 1 }]])), same(new Date(1), new Date(1)),
        );
        const bytes = randomBytes(70000);
        console.log(bytes.length, Buffer.isBuffer(bytes), randomInt(5, 6));
        try { createHash('sha256'); } catch (e) { console.log(e.message); }
        ")
    .await;
    assert_eq!(
        out,
        "n=42 {\"a\":[1]} { a: 'x', b: [ 1, { c: null } ] }\n42\nhi true true\n\
         false false false false false true true\n70000 true 5\n\
         crypto.createHash isn't available in eryx yet (only random values are)\n"
    );
}

/// No `Buffer` (or `process.nextTick`) global, unlike Node: code that uses
/// one without importing it gets a ReferenceError.
#[tokio::test]
async fn buffer_is_not_a_global() {
    let err = run("Buffer.from('x')").await.unwrap_err();
    assert!(err.to_string().contains("Buffer is not defined"), "{err}");
}

#[tokio::test]
async fn other_builtins_still_fail_clearly() {
    for module in ["fs", "node:url", "child_process", "net"] {
        let err = run(&format!("await import('{module}')")).await.unwrap_err();
        let name = module.trim_start_matches("node:");
        assert!(
            err.to_string()
                .contains(&format!("node:{name} isn't available in eryx yet")),
            "{module}: {err}"
        );
    }
}

/// The globals are the runtime's, so never session state, and built-in
/// imports come back on restore like an environment's.
#[tokio::test]
async fn snapshots_keep_builtin_imports() {
    let executor = Sandbox::embedded().javascript().build().unwrap().executor();
    let mut session = SessionExecutor::new(executor.clone(), &[]).await.unwrap();
    session
        .execute("import { EventEmitter } from 'node:events'\nconst id = crypto.randomUUID()")
        .run()
        .await
        .unwrap();
    let snapshot = session.snapshot_state().await.unwrap();
    assert_eq!(snapshot.skipped(), &[] as &[String]);

    // Class instances would come back as plain objects, so they're skipped
    // (and named) instead; plain data around Dates and Maps is kept.
    let mut classes = SessionExecutor::new(executor.clone(), &[]).await.unwrap();
    classes
        .execute(
            "import { Buffer } from 'node:buffer'\n\
             var bytes = Buffer.from('x'), bus = new Bus(), decoder = new TextDecoder()\n\
             var nested = { list: [bytes] }, plain = { when: new Date(0), map: new Map([[1, [2]]]) }\n\
             function Bus() {}",
        )
        .run()
        .await
        .unwrap();
    let mut skipped = classes.snapshot_state().await.unwrap().skipped().to_vec();
    skipped.sort();
    assert_eq!(skipped, ["Bus", "bus", "bytes", "decoder", "nested"]);

    let mut restored = SessionExecutor::new(executor, &[]).await.unwrap();
    restored.restore_state(&snapshot).await.unwrap();
    let out = restored
        .execute("console.log(typeof EventEmitter, id.length, typeof crypto.randomUUID)")
        .run()
        .await
        .unwrap();
    assert_eq!(out.stdout_text(), "function 36 function\n");
}
