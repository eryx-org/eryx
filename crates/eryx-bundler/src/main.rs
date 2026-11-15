//! `eryx-bundler`: bundles installed npm packages into ES modules for the
//! eryx JavaScript guest.
//!
//! A WASI command (wasm32-wasip2) around the rolldown library, so the parsing
//! of untrusted package source happens in a sandbox: eryx runs it with only
//! the package tree and an output directory mounted, and no network.
//!
//! ```text
//! eryx-bundler --cwd <dir> --out <dir> --entry <specifier> [--entry ...]
//! ```
//!
//! `--cwd` holds the `node_modules` tree; each `--entry` is a bare specifier
//! to make importable (`zod`, `date-fns/format`, `@scope/pkg`). The chunks go
//! to `--out`, with `bundle.json`:
//!
//! ```json
//! {"bundler": "rolldown 1.3.2", "entries": {"zod": "zod.js"}, "warnings": []}
//! ```
//!
//! Bundles are ES modules, unminified (readable stack traces), with shared
//! code split into chunks, and `process.env.NODE_ENV` defined as
//! `"production"`. Packages resolve with the conditions `worker`, `browser`,
//! `import`, `module` and `default`, as for a Worker, and their `browser`
//! field mappings apply. A Node built-in (`node:fs`, `fs`, `path`, ...) that
//! nothing provides stays external, for the guest to resolve at run time;
//! one that a `browser` mapping or an installed polyfill (`events`, at any
//! depth) provides is bundled. Bare specifiers that can't be resolved (a
//! missing optional peer) stay external too, with a warning.
//!
//! Exit status: 0 when bundled, 1 otherwise, with the diagnostics or a usage
//! message on stderr (`wasi:cli/exit` only carries success or failure).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use rolldown::{Bundler, BundlerOptions, InputItem, OutputFormat, Platform};
use rolldown_common::{IsExternal, Output, ResolveOptions};

/// The rolldown version, recorded in `bundle.json` (and so in environments).
const BUNDLER: &str = "rolldown 1.3.2";

/// Export conditions, in priority order: what a Worker gets.
const CONDITIONS: [&str; 5] = ["worker", "browser", "import", "module", "default"];

/// Node's built-in modules, importable without `node:`.
const NODE_BUILTINS: &[&str] = &[
    "assert",
    "async_hooks",
    "buffer",
    "child_process",
    "cluster",
    "console",
    "constants",
    "crypto",
    "dgram",
    "diagnostics_channel",
    "dns",
    "domain",
    "events",
    "fs",
    "http",
    "http2",
    "https",
    "inspector",
    "module",
    "net",
    "os",
    "path",
    "perf_hooks",
    "process",
    "punycode",
    "querystring",
    "readline",
    "repl",
    "stream",
    "string_decoder",
    "sys",
    "timers",
    "tls",
    "trace_events",
    "tty",
    "url",
    "util",
    "v8",
    "vm",
    "wasi",
    "worker_threads",
    "zlib",
];

struct Args {
    cwd: PathBuf,
    out: PathBuf,
    entries: Vec<String>,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let (mut cwd, mut out, mut entries) = (None, None, Vec::new());
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--cwd" => cwd = Some(PathBuf::from(value()?)),
            "--out" => out = Some(PathBuf::from(value()?)),
            "--entry" => entries.push(value()?),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if entries.is_empty() {
        return Err("no --entry given".to_string());
    }
    Ok(Args {
        cwd: cwd.ok_or("--cwd is required")?,
        out: out.ok_or("--out is required")?,
        entries,
    })
}

/// The chunk name for an entry: its specifier, flattened so every chunk
/// lands directly in the output directory (`@s/p/x` → `@s__p__x`).
fn entry_name(specifier: &str) -> String {
    specifier.replace('/', "__")
}

/// Whether `specifier` names a Node built-in: `node:<anything>`, or a bare
/// built-in name (`fs`, `fs/promises`).
fn is_node_builtin(specifier: &str) -> bool {
    if specifier.starts_with("node:") {
        return true;
    }
    let name = specifier.split('/').next().unwrap_or(specifier);
    NODE_BUILTINS.contains(&name)
}

/// rolldown's `external` check. It asks before resolving (`resolved` false),
/// again with the resolved id after resolving, and with the bare specifier
/// (`resolved` true) when it can't be resolved. Only that last case makes a
/// built-in external, so resolution runs first: a package's `browser` field
/// (`"fs": false`, or a shim) and installed polyfills (`events`, at any
/// depth) win, and only a built-in nothing provides is left to the guest.
fn is_external(specifier: &str, resolved: bool) -> bool {
    resolved && is_node_builtin(specifier)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("eryx-bundler: {e}");
            eprintln!("usage: eryx-bundler --cwd <dir> --out <dir> --entry <specifier>...");
            return ExitCode::FAILURE;
        }
    };
    match bundle(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

async fn bundle(args: &Args) -> Result<(), String> {
    let external = IsExternal::Fn(Some(Arc::new(|specifier, _importer, resolved| {
        let external = is_external(specifier, resolved);
        Box::pin(async move { Ok(external) })
    })));
    let mut bundler = Bundler::new(BundlerOptions {
        input: Some(
            args.entries
                .iter()
                .map(|entry| InputItem {
                    name: Some(entry_name(entry)),
                    import: entry.clone(),
                })
                .collect(),
        ),
        cwd: Some(args.cwd.clone()),
        dir: Some(args.out.display().to_string()),
        format: Some(OutputFormat::Esm),
        platform: Some(Platform::Browser),
        external: Some(external),
        resolve: Some(ResolveOptions {
            condition_names: Some(CONDITIONS.iter().map(ToString::to_string).collect()),
            ..Default::default()
        }),
        // Production builds of packages that branch on it (React and the
        // like). rolldown would otherwise derive "development" from minify
        // being off; the output stays unminified for readable stack traces.
        define: Some(
            [(
                "process.env.NODE_ENV".to_string(),
                r#""production""#.to_string(),
            )]
            .into_iter()
            .collect(),
        ),
        ..Default::default()
    })
    .map_err(|e| format!("eryx-bundler: {e:?}"))?;

    let output = bundler.write().await.map_err(|diagnostics| {
        diagnostics
            .into_vec()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    })?;

    let mut entries = BTreeMap::new();
    for asset in &output.assets {
        if let Output::Chunk(chunk) = asset
            && chunk.is_entry
            && let Some(entry) = args.entries.iter().find(|e| entry_name(e) == *chunk.name)
        {
            entries.insert(entry.clone(), chunk.filename.to_string());
        }
    }
    if let Some(missing) = args.entries.iter().find(|e| !entries.contains_key(*e)) {
        return Err(format!("eryx-bundler: no chunk was written for {missing}"));
    }
    let report = serde_json::json!({
        "bundler": BUNDLER,
        "entries": entries,
        "warnings": output.warnings.iter().map(ToString::to_string).collect::<Vec<_>>(),
    });
    let report = serde_json::to_vec_pretty(&report).map_err(|e| e.to_string())?;
    std::fs::write(args.out.join("bundle.json"), report)
        .map_err(|e| format!("eryx-bundler: writing bundle.json: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_arguments() {
        let args = |list: &[&str]| parse_args(list.iter().map(ToString::to_string));
        let parsed = args(&[
            "--cwd", "/src", "--out", "/out", "--entry", "zod", "--entry", "a/b",
        ])
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(parsed.entries, ["zod", "a/b"]);
        assert!(args(&["--cwd", "/src", "--out", "/out"]).is_err());
        assert!(args(&["--cwd"]).is_err());
        assert!(args(&["--bogus"]).is_err());
    }

    #[test]
    fn names_and_builtins() {
        assert_eq!(entry_name("@s/p/x"), "@s__p__x");
        assert!(is_external("node:events", true));
        assert!(is_external("fs/promises", true));
        // Before resolution nothing is external, so the `browser` field and
        // polyfills get their chance; a resolved path is never a built-in.
        assert!(!is_external("fs", false));
        assert!(!is_external("/src/node_modules/events/events.js", true));
        assert!(!is_external("zod", true));
    }
}
