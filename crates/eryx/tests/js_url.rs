//! `URL`, `URLSearchParams` and `node:url` in the QuickJS guest (#925).
//! The globals are core-js's (vendored in `crates/eryx-guest-js/src/node/`);
//! these check the behaviour scripts rely on, not the whole standard.
//!
//! ```sh
//! mise run precompile-runtime-javascript
//! cargo nextest run -p eryx --features embedded,javascript --test js_url
//! ```
#![cfg(all(feature = "embedded", feature = "javascript"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use eryx::{Sandbox, SessionExecutor};

async fn run(code: &str) -> String {
    let sandbox = Sandbox::embedded().javascript().build().unwrap();
    match sandbox.execute(code).await {
        Ok(out) => out.stdout_text(),
        Err(e) => panic!("{code}: {e}"),
    }
}

#[tokio::test]
async fn parsing_and_normalization() {
    let out = run(r"
        const show = (u) => console.log(u.href);
        show(new URL('HTTPS://User:Pw@EXAMPLE.com:443/a/./b/../c?x=1#h'));
        show(new URL('../d?y', 'http://example.com/a/b/c'));
        show(new URL('//other.test/p', 'https://example.com/'));
        show(new URL('http://0x7f.1/'));
        show(new URL('http://[0:0::1]:8080/'));
        show(new URL('http://bücher.example/'));
        show(new URL('mailto:someone@example.com'));
        show(new URL('data:text/plain,hi there'));
        show(new URL('file:///tmp/a%20b'));
        const u = new URL('https://example.com:8443/p?q=1#frag');
        console.log(u.protocol, u.host, u.hostname, u.port, u.pathname, u.search, u.hash, u.origin);
        console.log(URL.canParse('nope'), URL.canParse('/rel', 'https://x.test'));
        try { new URL('not a url'); } catch (e) { console.log(e.name); }
        try { new URL('http://exa mple.com/'); } catch (e) { console.log(e.name); }
        ")
    .await;
    assert_eq!(
        out,
        "https://User:Pw@example.com/a/c?x=1#h\n\
         http://example.com/a/d?y\n\
         https://other.test/p\n\
         http://127.0.0.1/\n\
         http://[::1]:8080/\n\
         http://xn--bcher-kva.example/\n\
         mailto:someone@example.com\n\
         data:text/plain,hi there\n\
         file:///tmp/a%20b\n\
         https: example.com:8443 example.com 8443 /p ?q=1 #frag https://example.com:8443\n\
         false true\n\
         TypeError\n\
         TypeError\n"
    );
}

#[tokio::test]
async fn setters_and_search_params() {
    let out = run(r"
        const u = new URL('http://example.com/a?x=1');
        u.pathname = '/b c';
        u.hash = 'top';
        u.port = '8080';
        u.username = 'me';
        u.searchParams.append('y', 'two words');
        console.log(u.href);
        u.search = '?k=v';
        console.log(u.searchParams.get('k'), [...u.searchParams.keys()].join());
        u.protocol = 'https';
        u.host = 'other.test:9';
        console.log(u.href, JSON.stringify(u), String(u) === u.toString());

        const p = new URLSearchParams('b=2&a=1&a=3&c=%20+x');
        p.sort();
        console.log(p.toString(), p.getAll('a').join(), p.get('c'), p.has('b'), p.size);
        p.delete('a');
        p.set('d', 'é&=');
        console.log([...p].map(([k, v]) => `${k}:${v}`).join('|'), p.toString());
        console.log(new URLSearchParams({ x: 1, y: 'z' }).toString(), new URLSearchParams([['k', 'v']]).toString());
        ")
    .await;
    assert_eq!(
        out,
        "http://me@example.com:8080/b%20c?x=1&y=two+words#top\n\
         v k\n\
         https://me@other.test:9/b%20c?k=v#top \"https://me@other.test:9/b%20c?k=v#top\" true\n\
         a=1&a=3&b=2&c=++x 1,3   x true 4\n\
         b:2|c:  x|d:é&= b=2&c=++x&d=%C3%A9%26%3D\n\
         x=1&y=z k=v\n"
    );
}

#[tokio::test]
async fn node_url() {
    let out = run(r"
        import url, { fileURLToPath, pathToFileURL, URL as NodeURL } from 'node:url'
        console.log(NodeURL === URL, require('url').URLSearchParams === URLSearchParams);
        console.log(fileURLToPath('file:///tmp/a%20b'), pathToFileURL('/tmp/a b#c').href);
        // Relative to `/` (there's no working directory); `\` is encoded.
        console.log(pathToFileURL('rel/x').href, pathToFileURL('/tmp/a\\b').href);
        try { fileURLToPath('http://x.test/'); } catch (e) { console.log(e.code); }
        console.log(JSON.stringify(url.urlToHttpOptions(new URL('https://u:p@x.test:81/p?q'))));
        console.log(url.domainToASCII('bücher.example'), url.domainToUnicode('xn--bcher-kva.example'), url.domainToASCII('exa mple'));
        console.log(JSON.stringify(['user@host', 'x:80', 'a/b', '[::1]', 'a\\b'].map((d) => [url.domainToASCII(d), url.domainToUnicode(d)])));
        const parsed = url.parse('https://u:p@x.test:81/a/b?c=1&c=2#h', true);
        console.log(parsed.protocol, parsed.auth, parsed.host, parsed.port, parsed.pathname, parsed.search, JSON.stringify(parsed.query), parsed.hash, parsed.path);
        const ported = url.parse('http://[::1]:80/p?');
        console.log(ported.host, ported.port, ported.hostname, ported.search, ported.path, url.parse('http://x.test:80/').host);
        const rel = url.parse('/p/q?x=1#y');
        console.log(rel.protocol, rel.pathname, rel.query, rel.hash, rel.href);
        console.log(url.format({ protocol: 'https', hostname: 'x.test', port: 8, pathname: 'p', query: { a: 1, b: ['2', '3'] } }));
        // Every `:` in `auth` stays literal, as in Node; `@` is encoded.
        console.log(url.format({ protocol: 'http', slashes: true, host: 'h', auth: 'a:b:c@d' }));
        console.log(url.format(new URL('http://x.test/a')), url.resolve('/one/two', 'three'), url.resolve('http://x.test/a/b', '../c'));
        ")
    .await;
    assert_eq!(
        out,
        "true true\n\
         /tmp/a b file:///tmp/a%20b%23c\n\
         file:///rel/x file:///tmp/a%5Cb\n\
         ERR_INVALID_URL\n\
         {\"protocol\":\"https:\",\"hostname\":\"x.test\",\"hash\":\"\",\"search\":\"?q\",\"pathname\":\"/p\",\"path\":\"/p?q\",\"href\":\"https://u:p@x.test:81/p?q\",\"port\":81,\"auth\":\"u:p\"}\n\
         xn--bcher-kva.example bücher.example \n\
         [[\"\",\"\"],[\"\",\"\"],[\"\",\"\"],[\"\",\"\"],[\"\",\"\"]]\n\
         https: u:p x.test:81 81 /a/b ?c=1&c=2 {\"c\":[\"1\",\"2\"]} #h /a/b?c=1&c=2\n\
         [::1]:80 80 ::1 ? /p? x.test:80\n\
         null /p/q x=1 #y /p/q?x=1#y\n\
         https://x.test:8/p?a=1&b=2&b=3\n\
         http://a:b:c%40d@h\n\
         http://x.test/a /one/three http://x.test/c\n"
    );
}

/// The globals come from core-js's pure build at context creation: nothing
/// of core-js is left on the global object, the built-ins it could have
/// patched are QuickJS's own, and a stored URL is skipped (and named) by a
/// snapshot like any class instance.
#[tokio::test]
async fn no_core_js_state_and_snapshots_skip_urls() {
    let out = run(r"
        const extra = Object.getOwnPropertyNames(globalThis).filter((n) => /core|^__(?!eryx)/i.test(n));
        console.log(JSON.stringify(extra), Object.keys(globalThis).includes('URL'));
        console.log([Array.prototype.includes, String.prototype.at, Object.fromEntries, Symbol.for]
            .every((f) => /\[native code\]/.test(Function.prototype.toString.call(f))));
        // core-js wraps a Request it finds while loading; it loads first.
        console.log(new Request('https://x.test/').constructor === Request, Request.prototype.constructor === Request);
        ")
    .await;
    assert_eq!(out, "[] false\ntrue\ntrue true\n");

    let executor = Sandbox::embedded().javascript().build().unwrap().executor();
    let mut session = SessionExecutor::new(executor.clone(), &[]).await.unwrap();
    session
        .execute("var link = new URL('https://x.test/a'); var href = link.href")
        .run()
        .await
        .unwrap();
    let snapshot = session.snapshot_state().await.unwrap();
    assert_eq!(snapshot.skipped(), ["link"]);
    let mut restored = SessionExecutor::new(executor, &[]).await.unwrap();
    restored.restore_state(&snapshot).await.unwrap();
    let out = restored
        .execute("console.log(new URL(href).pathname, typeof link)")
        .run()
        .await
        .unwrap();
    assert_eq!(out.stdout_text(), "/a undefined\n");
}
