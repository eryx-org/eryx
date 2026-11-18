//! Modules from the sandbox's environment.
//!
//! A sandbox given an environment (`SandboxBuilder::with_environment`) sees
//! it read-only at [`MOUNT`]. Its manifest's `exports` map bare specifiers
//! (`zod`, `date-fns/format`) to ES modules there; those modules import each
//! other's shared chunks by relative path. That's all scripts can import:
//!
//! - a specifier in `exports`, and relative imports between environment
//!   modules, which stay inside the environment;
//! - Node built-ins (`node:fs`, or a bare `fs` the environment doesn't
//!   provide), as stubs that throw when imported, until there's a compat
//!   layer for them;
//! - nothing else ("Cannot find module").
//!
//! Scripts run as global code, where `import` declarations aren't allowed,
//! so [`rewrite_imports`] turns a script's leading ones into dynamic
//! imports. `require()` imports synchronously (`src/modules.js`).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::promise::PromiseState;
use rquickjs::{Ctx, Error, Function, IntoJs, Module, Object, Result, Value};

/// Where the host mounts the environment (`eryx::environment::MOUNT_PATH`).
const MOUNT: &str = "/eryx/environment";

/// The prefix stub modules for Node built-ins resolve to.
const BUILTIN: &str = "node:";

const MODULES_JS: &str = include_str!("modules.js");

/// The built-ins eryx provides (`src/node/`), by name after `node:`: npm
/// polyfills vendored by `src/node/regen.sh`, and hand-written subsets.
/// Each is evaluated on its first import. The rest throw on import.
fn node_module(name: &str) -> Option<&'static str> {
    Some(match name {
        "buffer" => include_str!("node/buffer.js"),
        "crypto" => include_str!("node/crypto.js"),
        "events" => include_str!("node/events.js"),
        "path" | "path/posix" => include_str!("node/path.js"),
        "string_decoder" => include_str!("node/string_decoder.js"),
        "timers" => include_str!("node/timers.js"),
        "url" => include_str!("node/url.js"),
        "timers/promises" => include_str!("node/timers_promises.js"),
        "util" => include_str!("node/util.js"),
        _ => return None,
    })
}

/// Node's built-in modules, importable without `node:` (as in eryx-bundler).
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

/// The environment's exports, read from its manifest on first use (never
/// at preinit, which imports nothing, so the snapshot never holds a map).
enum Environment {
    /// No environment is mounted.
    Missing,
    /// Bare specifier to absolute path under [`MOUNT`], and the content dir.
    Present {
        exports: BTreeMap<String, String>,
        content_dir: String,
    },
}

thread_local! {
    static ENVIRONMENT: RefCell<Option<Rc<Environment>>> = const { RefCell::new(None) };
}

fn environment(ctx: &Ctx<'_>) -> Result<Rc<Environment>> {
    if let Some(environment) = ENVIRONMENT.with(|e| e.borrow().clone()) {
        return Ok(environment);
    }
    let manifest = format!("{MOUNT}/eryx-environment.json");
    let environment = match std::fs::read_to_string(&manifest) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Environment::Missing,
        Err(e) => return Err(Error::new_loading_message(manifest, e.to_string())),
        Ok(text) => {
            let manifest = ctx.json_parse(text)?.into_object().ok_or_else(|| {
                Error::new_loading_message("eryx-environment.json", "not an object")
            })?;
            let listed: Object<'_> = manifest.get("exports")?;
            let mut exports = BTreeMap::new();
            for key in listed.keys::<String>() {
                let key = key?;
                let path: String = listed.get(&key)?;
                exports.insert(key, format!("{MOUNT}/{path}"));
            }
            let content: Object<'_> = manifest.get("content")?;
            let dir: String = content.get("dir")?;
            Environment::Present {
                exports,
                content_dir: format!("{MOUNT}/{dir}"),
            }
        }
    };
    let environment = Rc::new(environment);
    ENVIRONMENT.with(|e| *e.borrow_mut() = Some(Rc::clone(&environment)));
    Ok(environment)
}

/// The Node built-in `specifier` names, if it names one.
fn builtin(specifier: &str) -> Option<&str> {
    if let Some(name) = specifier.strip_prefix(BUILTIN) {
        return Some(name);
    }
    let top = specifier.split('/').next().unwrap_or(specifier);
    NODE_BUILTINS.contains(&top).then_some(specifier)
}

/// `relative` resolved against the directory of `base`, if it stays inside
/// `root`.
fn join_inside(root: &str, base: &str, relative: &str) -> Option<String> {
    let mut parts: Vec<&str> = base.split('/').collect();
    parts.pop(); // the importing file
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    let path = parts.join("/");
    path.strip_prefix(root)
        .is_some_and(|rest| rest.starts_with('/'))
        .then_some(path)
}

/// Resolves specifiers against the environment.
#[derive(Debug)]
pub(crate) struct EnvResolver;

impl Resolver for EnvResolver {
    fn resolve<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> Result<String> {
        let environment = environment(ctx)?;
        // An environment export wins over a built-in of the same name (a
        // polyfill, say).
        if let Environment::Present { exports, .. } = &*environment
            && let Some(path) = exports.get(name)
        {
            return Ok(path.clone());
        }
        if let Some(builtin) = builtin(name) {
            return Ok(format!("{BUILTIN}{builtin}"));
        }
        if name.starts_with("./") || name.starts_with("../") {
            if let Environment::Present { content_dir, .. } = &*environment
                && base.starts_with(content_dir.as_str())
                && let Some(path) = join_inside(content_dir, base, name)
            {
                return Ok(path);
            }
            return Err(Error::new_resolving_message(
                base,
                name,
                "relative imports only work between the environment's own modules",
            ));
        }
        let message = match &*environment {
            Environment::Missing => "Cannot find module: this sandbox has no environment (see \
                 SandboxBuilder::with_environment)"
                .to_string(),
            Environment::Present { exports, .. } => {
                let names: Vec<&str> = exports.keys().map(String::as_str).collect();
                format!(
                    "Cannot find module: this sandbox's environment provides {}",
                    names.join(", ")
                )
            }
        };
        Err(Error::new_resolving_message(base, name, message))
    }
}

/// Loads environment modules from the mount, and the built-in stubs.
#[derive(Debug)]
pub(crate) struct EnvLoader;

impl Loader for EnvLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> Result<Module<'js, Declared>> {
        if let Some(builtin) = name.strip_prefix(BUILTIN) {
            if let Some(source) = node_module(builtin) {
                return Module::declare(ctx.clone(), name, source);
            }
            let message = format!("{BUILTIN}{builtin} isn't available in eryx yet");
            let source = format!("throw new Error({});", quote(&message));
            return Module::declare(ctx.clone(), name, source);
        }
        let source =
            std::fs::read(name).map_err(|e| Error::new_loading_message(name, e.to_string()))?;
        Module::declare(ctx.clone(), name, source)
    }
}

/// A JavaScript string literal for `text`.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Install `require` and the import interop helper (`src/modules.js`).
pub(crate) fn install(ctx: &Ctx<'_>) -> Result<()> {
    let native = Object::new(ctx.clone())?;
    native.set("requireSync", Function::new(ctx.clone(), require_sync)?)?;
    let install: Function<'_> = ctx.eval(MODULES_JS)?;
    install.call((native,))
}

/// `require(name)`'s module namespace: import it and run the job queue
/// until it settles. A module that awaits at top level can't settle that
/// way, so it's an error.
fn require_sync<'js>(ctx: Ctx<'js>, name: String) -> Result<Value<'js>> {
    // A built-in is CommonJS in Node, so `require` gives its default export
    // (`class X extends require("events")`), not the namespace.
    let builtin_module = builtin(&name).is_some()
        && !matches!(&*environment(&ctx)?, Environment::Present { exports, .. } if exports.contains_key(&name));
    let promise = Module::import(&ctx, name.clone())?;
    loop {
        match promise.state() {
            PromiseState::Resolved | PromiseState::Rejected => {
                // A failure is thrown from here instead, so the promise
                // isn't an unhandled rejection the execute would report.
                // SAFETY: `promise` is alive in `ctx`.
                unsafe {
                    rquickjs::qjs::JS_PromiseMarkAsHandled(
                        ctx.as_raw().as_ptr(),
                        promise.as_value().as_raw(),
                    );
                }
                let namespace = promise.result::<Value<'js>>().unwrap_or_else(|| {
                    Err(Error::new_loading_message(
                        &name,
                        "the import didn't settle",
                    ))
                })?;
                return match namespace.as_object() {
                    Some(object) if builtin_module => object.get("default"),
                    _ => Ok(namespace),
                };
            }
            PromiseState::Pending => {
                if !ctx.execute_pending_job() {
                    let message = format!(
                        "require({}) can't finish synchronously: the module uses top-level \
                         await. Use `await import({})` instead",
                        quote(&name),
                        quote(&name)
                    );
                    let error = rquickjs::Exception::from_message(ctx.clone(), &message)?;
                    return Err(ctx.throw(error.into_value()));
                }
            }
        }
    }
}

// -----------------------------------------------------------------------------
// The import rewrite.
// -----------------------------------------------------------------------------

/// What an `import` declaration binds, kept so a session snapshot can
/// re-import it on restore (see `restore_state` in js.rs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportRecord {
    pub(crate) specifier: String,
    pub(crate) form: ImportForm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImportForm {
    /// `import { a, b as c }` (and `import x, { ... }`, as `default as x`):
    /// `(imported, local)` pairs. Empty for `import "m"`.
    Named(Vec<(String, String)>),
    /// `import x`.
    Default(String),
    /// `import * as ns`.
    Namespace(String),
}

impl ImportRecord {
    /// The statement that performs this import in global code. Around it,
    /// `__eryx_loading()` and `__eryx_bound({local, ...})` tell the prelude
    /// what the import bound and which globals loading its module created,
    /// for snapshots; `__eryx_import` checks the imported names exist, as
    /// ES modules do.
    pub(crate) fn statement(&self) -> String {
        let specifier = quote(&self.specifier);
        let import = format!("await import({specifier})");
        let checked = |names: &[&str]| {
            let names: Vec<String> = names.iter().map(|n| quote(n)).collect();
            format!(
                "__eryx_import({import}, [{}], {specifier})",
                names.join(", ")
            )
        };
        let (statement, locals) = match &self.form {
            ImportForm::Named(bindings) => {
                let pattern: Vec<String> = bindings
                    .iter()
                    .map(|(imported, local)| {
                        if imported == local {
                            local.clone()
                        } else if is_identifier(imported) {
                            format!("{imported}: {local}")
                        } else {
                            format!("{}: {local}", quote(imported))
                        }
                    })
                    .collect();
                let imported: Vec<&str> = bindings.iter().map(|(i, _)| i.as_str()).collect();
                (
                    format!("var {{{}}} = {};", pattern.join(", "), checked(&imported)),
                    bindings.iter().map(|(_, l)| l.as_str()).collect(),
                )
            }
            ImportForm::Default(local) => (
                format!("var {local} = {}.default;", checked(&["default"])),
                vec![local.as_str()],
            ),
            ImportForm::Namespace(local) => {
                (format!("var {local} = {import};"), vec![local.as_str()])
            }
        };
        format!(
            "__eryx_loading(); {statement} __eryx_bound({{{}}});",
            locals.join(", ")
        )
    }

    /// As `{specifier, form, bindings}`, the prelude's session-state shape:
    /// `bindings` is `[[imported, local], ...]` for `named`, `[local]`
    /// otherwise.
    pub(crate) fn to_js<'js>(&self, ctx: &Ctx<'js>) -> Result<Object<'js>> {
        let object = Object::new(ctx.clone())?;
        object.set("specifier", self.specifier.as_str())?;
        let (form, bindings): (&str, Vec<Vec<String>>) = match &self.form {
            ImportForm::Named(pairs) => (
                "named",
                pairs
                    .iter()
                    .map(|(i, l)| vec![i.clone(), l.clone()])
                    .collect(),
            ),
            ImportForm::Default(local) => ("default", vec![vec![local.clone()]]),
            ImportForm::Namespace(local) => ("namespace", vec![vec![local.clone()]]),
        };
        object.set("form", form)?;
        let bindings: Vec<Value<'js>> = bindings
            .into_iter()
            .map(|b| match <[String; 1]>::try_from(b) {
                Ok([local]) => local.into_js(ctx),
                Err(pair) => pair.into_js(ctx),
            })
            .collect::<Result<_>>()?;
        object.set("bindings", bindings)?;
        Ok(object)
    }

    /// The reverse of [`to_js`](Self::to_js), for records the prelude has
    /// already validated.
    pub(crate) fn from_js(object: &Object<'_>) -> Result<Self> {
        let specifier: String = object.get("specifier")?;
        let form: String = object.get("form")?;
        let form = match form.as_str() {
            "named" => ImportForm::Named(
                object
                    .get::<_, Vec<Vec<String>>>("bindings")?
                    .into_iter()
                    .filter_map(|pair| <[String; 2]>::try_from(pair).ok())
                    .map(|[imported, local]| (imported, local))
                    .collect(),
            ),
            "default" | "namespace" => {
                let [local]: [String; 1] = object
                    .get::<_, Vec<String>>("bindings")?
                    .try_into()
                    .map_err(|_| Error::new_from_js("bindings", "one local name"))?;
                if form == "default" {
                    ImportForm::Default(local)
                } else {
                    ImportForm::Namespace(local)
                }
            }
            _ => return Err(Error::new_from_js("form", "an import form")),
        };
        Ok(Self { specifier, form })
    }
}

/// An ASCII identifier.
fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
}

/// Run an import statement (from [`ImportRecord::statement`]) as global
/// code and settle it, as `require` does: the loader reads from the mount,
/// so nothing waits on the host.
pub(crate) fn run_import(ctx: &Ctx<'_>, statement: &str) -> std::result::Result<(), String> {
    let mut options = rquickjs::context::EvalOptions::default();
    options.strict = false;
    options.promise = true;
    options.filename = Some("<eryx>".to_string());
    let promise: rquickjs::Promise<'_> = ctx
        .eval_with_options(statement, options)
        .map_err(|e| caught(ctx, e))?;
    loop {
        match promise.state() {
            PromiseState::Resolved => return Ok(()),
            PromiseState::Rejected => {
                return Err(match promise.result::<Value<'_>>() {
                    Some(Err(e)) => caught(ctx, e),
                    _ => "the import failed".to_string(),
                });
            }
            PromiseState::Pending => {
                if !ctx.execute_pending_job() {
                    return Err("the import uses top-level await, so it can't be restored".into());
                }
            }
        }
    }
}

fn caught(ctx: &Ctx<'_>, error: Error) -> String {
    match error {
        Error::Exception => {
            let value = ctx.catch();
            value
                .as_exception()
                .and_then(rquickjs::Exception::message)
                .or_else(|| value.as_string().and_then(|s| s.to_string().ok()))
                .unwrap_or_else(|| "an exception".to_string())
        }
        other => other.to_string(),
    }
}

/// Rewrite a script's leading `import` declarations into dynamic imports,
/// which global code allows, keeping every line where it was (see
/// [`ImportRecord::statement`] for each form's rewrite):
///
/// - `import x from "m"`, `import { a, b as c } from "m"`,
///   `import x, { a } from "m"`, `import * as ns from "m"`,
///   `import x, * as ns from "m"` and `import "m"`.
///
/// Returns the code unchanged unless it starts (after comments and
/// whitespace) with imports this understands completely: anything else
/// (import attributes, string export names, escapes in the specifier,
/// non-ASCII names) is left for QuickJS to report as a syntax error, rather
/// than rewritten wrongly. Also returns what the imports bind, in order.
pub(crate) fn rewrite_imports(code: &str) -> (std::borrow::Cow<'_, str>, Vec<ImportRecord>) {
    let mut out = String::new();
    let mut records = Vec::new();
    let mut scanner = Scanner { code, pos: 0 };
    let mut copied = 0;
    loop {
        scanner.skip_trivia();
        let start = scanner.pos;
        if !scanner.at_import_declaration() {
            break;
        }
        let Some(declared) = scanner.import_declaration() else {
            return (std::borrow::Cow::Borrowed(code), Vec::new());
        };
        out.push_str(&code[copied..start]);
        let statements: Vec<String> = declared.iter().map(ImportRecord::statement).collect();
        out.push_str(&statements.join(" "));
        // Keep the lines the declaration took.
        let newlines = code[start..scanner.pos].matches('\n').count();
        out.push_str(&"\n".repeat(newlines));
        copied = scanner.pos;
        records.extend(declared);
    }
    if records.is_empty() {
        return (std::borrow::Cow::Borrowed(code), records);
    }
    out.push_str(&code[copied..]);
    (std::borrow::Cow::Owned(out), records)
}

struct Scanner<'a> {
    code: &'a str,
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn rest(&self) -> &'a str {
        &self.code[self.pos..]
    }

    /// Skip whitespace and comments.
    fn skip_trivia(&mut self) {
        loop {
            let rest = self.rest();
            let trimmed = rest.trim_start();
            self.pos += rest.len() - trimmed.len();
            if let Some(after) = self.rest().strip_prefix("//") {
                self.pos += 2 + after.find('\n').unwrap_or(after.len());
            } else if let Some(after) = self.rest().strip_prefix("/*") {
                match after.find("*/") {
                    Some(end) => self.pos += 2 + end + 2,
                    None => return,
                }
            } else {
                return;
            }
        }
    }

    /// At `import` followed by something that makes it a declaration (not
    /// `import(` or `import.meta`).
    fn at_import_declaration(&self) -> bool {
        let Some(after) = self.rest().strip_prefix("import") else {
            return false;
        };
        let next = after.trim_start_matches([' ', '\t', '\r', '\n']);
        after.len() != next.len() && !next.starts_with(['(', '.'])
            || next.starts_with(['{', '*', '"', '\''])
    }

    fn eat(&mut self, token: &str) -> bool {
        self.skip_trivia();
        if self.rest().starts_with(token) {
            self.pos += token.len();
            true
        } else {
            false
        }
    }

    /// An ASCII identifier (anything else isn't rewritten).
    fn identifier(&mut self) -> Option<String> {
        self.skip_trivia();
        let rest = self.rest();
        let len = rest
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$')
            .count();
        let name = &rest[..len];
        if !is_identifier(name) || rest[len..].chars().next().is_some_and(|c| !c.is_ascii()) {
            return None;
        }
        self.pos += len;
        Some(name.to_string())
    }

    /// A module specifier: a plain string literal without escapes.
    fn specifier(&mut self) -> Option<String> {
        self.skip_trivia();
        let rest = self.rest();
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let body = &rest[1..];
        let end = body.find(quote)?;
        let specifier = &body[..end];
        if specifier.contains(['\\', '\n', '\r']) {
            return None;
        }
        self.pos += 1 + end + 1;
        Some(specifier.to_string())
    }

    /// `{ a, b as c, }` as `(imported, local)` pairs.
    fn named(&mut self) -> Option<Vec<(String, String)>> {
        if !self.eat("{") {
            return None;
        }
        let mut pairs = Vec::new();
        loop {
            if self.eat("}") {
                return Some(pairs);
            }
            let imported = self.identifier()?;
            let local = if self.keyword("as") {
                self.identifier()?
            } else {
                imported.clone()
            };
            pairs.push((imported, local));
            if !self.eat(",") {
                return self.eat("}").then_some(pairs);
            }
        }
    }

    /// The keyword `word`, not a longer identifier starting with it.
    fn keyword(&mut self, word: &str) -> bool {
        self.skip_trivia();
        let rest = self.rest();
        let follows = rest[word.len().min(rest.len())..].chars().next();
        if rest.starts_with(word)
            && !follows.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
        {
            self.pos += word.len();
            true
        } else {
            false
        }
    }

    /// `from "m"`.
    fn after_from(&mut self) -> Option<String> {
        self.keyword("from").then_some(())?;
        self.specifier()
    }

    /// Parse one declaration after `import`, returning what it binds.
    fn import_declaration(&mut self) -> Option<Vec<ImportRecord>> {
        self.pos += "import".len();
        self.skip_trivia();
        let record = |specifier: &str, form| ImportRecord {
            specifier: specifier.to_string(),
            form,
        };
        let records = if self.rest().starts_with(['"', '\'']) {
            let module = self.specifier()?;
            vec![record(&module, ImportForm::Named(Vec::new()))]
        } else if self.eat("*") {
            self.keyword("as").then_some(())?;
            let ns = self.identifier()?;
            let module = self.after_from()?;
            vec![record(&module, ImportForm::Namespace(ns))]
        } else if self.rest().starts_with('{') {
            let named = self.named()?;
            let module = self.after_from()?;
            vec![record(&module, ImportForm::Named(named))]
        } else {
            let default = self.identifier()?;
            if default == "type" {
                return None; // TypeScript's `import type`
            }
            if self.eat(",") {
                self.skip_trivia();
                if self.eat("*") {
                    self.keyword("as").then_some(())?;
                    let ns = self.identifier()?;
                    let module = self.after_from()?;
                    vec![
                        record(&module, ImportForm::Namespace(ns)),
                        record(&module, ImportForm::Default(default)),
                    ]
                } else {
                    let mut named = self.named()?;
                    let module = self.after_from()?;
                    named.insert(0, ("default".to_string(), default));
                    vec![record(&module, ImportForm::Named(named))]
                }
            } else {
                let module = self.after_from()?;
                vec![record(&module, ImportForm::Default(default))]
            }
        };
        // `with { ... }` attributes, or anything else before the end of the
        // statement: not handled.
        let before = self.pos;
        self.skip_horizontal();
        let rest = self.rest();
        if rest.starts_with(';') {
            self.pos += 1;
        } else if rest.is_empty()
            || rest.starts_with(['\n', '\r'])
            || rest.starts_with("//")
            || rest.starts_with("/*")
        {
            // The end of the line, or a comment: leave the whitespace before
            // it as it was.
            self.pos = before;
        } else {
            return None;
        }
        Some(records)
    }

    fn skip_horizontal(&mut self) {
        let rest = self.rest();
        let trimmed = rest.trim_start_matches([' ', '\t']);
        self.pos += rest.len() - trimmed.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(code: &str) -> String {
        rewrite_imports(code).0.into_owned()
    }

    #[test]
    fn rewrites_each_form() {
        for (code, expected) in [
            (
                r#"import x from "m";"#,
                r#"__eryx_loading(); var x = __eryx_import(await import("m"), ["default"], "m").default; __eryx_bound({x});"#,
            ),
            (
                "import { a, b as c } from 'm'",
                r#"__eryx_loading(); var {a, b: c} = __eryx_import(await import("m"), ["a", "b"], "m"); __eryx_bound({a, c});"#,
            ),
            (
                r#"import x, { a, } from "m";"#,
                r#"__eryx_loading(); var {default: x, a} = __eryx_import(await import("m"), ["default", "a"], "m"); __eryx_bound({x, a});"#,
            ),
            (
                r#"import * as ns from "m";"#,
                r#"__eryx_loading(); var ns = await import("m"); __eryx_bound({ns});"#,
            ),
            (
                r#"import x, * as ns from "m";"#,
                r#"__eryx_loading(); var ns = await import("m"); __eryx_bound({ns}); __eryx_loading(); var x = __eryx_import(await import("m"), ["default"], "m").default; __eryx_bound({x});"#,
            ),
            (
                r#"import "m";"#,
                r#"__eryx_loading(); var {} = __eryx_import(await import("m"), [], "m"); __eryx_bound({});"#,
            ),
            (
                r#"import {} from "m";"#,
                r#"__eryx_loading(); var {} = __eryx_import(await import("m"), [], "m"); __eryx_bound({});"#,
            ),
        ] {
            assert_eq!(rewrite(code), expected, "{code}");
        }
    }

    #[test]
    fn keeps_lines_comments_and_the_rest_of_the_script() {
        let code = "// a script\n/* imports: */ import {\n  a,\n  b\n} from \"m\"\nimport z from 'z' // why\nconsole.log(a, b, z)\n";
        let (rewritten, statements) = rewrite_imports(code);
        assert_eq!(
            rewritten,
            "// a script\n/* imports: */ __eryx_loading(); var {a, b} = __eryx_import(await import(\"m\"), [\"a\", \"b\"], \"m\"); __eryx_bound({a, b});\n\n\n\n__eryx_loading(); var z = __eryx_import(await import(\"z\"), [\"default\"], \"z\").default; __eryx_bound({z}); // why\nconsole.log(a, b, z)\n"
        );
        assert_eq!(rewritten.lines().count(), code.lines().count());
        assert_eq!(statements.len(), 2);
    }

    #[test]
    fn leaves_everything_else_alone() {
        for code in [
            "console.log(1)",
            "const x = await import('m')",
            "import('m').then(console.log)",
            "import.meta",
            "importantThing()",
            r#"import x from "m" with { type: "json" }"#,
            r#"import { "a-b" as c } from "m""#,
            "import x from \"m\\u0041\"",
            r#"import type { T } from "m""#,
            r#"import x from "m" extra"#,
            r#"import x from "m" / 2"#,
            r#"import é from "m""#,
            r#"import x from m"#,
            r#"import { a b } from "m""#,
            r#"import * from "m""#,
            "export const x = 1",
        ] {
            let (rewritten, statements) = rewrite_imports(code);
            assert_eq!(rewritten, code, "{code}");
            assert!(statements.is_empty(), "{code}");
        }
        // One it can't handle leaves the whole script for QuickJS to report.
        let code = "import a from 'a'\nimport b from 'b' with { type: 'json' }\n";
        assert_eq!(rewrite(code), code);
    }

    #[test]
    fn only_leading_imports() {
        let code = "const x = 1;\nimport a from 'a'\n";
        assert_eq!(rewrite(code), code);
    }

    #[test]
    fn resolves_relative_paths_inside_the_content_dir() {
        let root = "/eryx/environment/modules";
        let base = "/eryx/environment/modules/zod.js";
        assert_eq!(
            join_inside(root, base, "./rolldown-runtime-x.js").as_deref(),
            Some("/eryx/environment/modules/rolldown-runtime-x.js")
        );
        assert_eq!(join_inside(root, base, "../eryx-npm.lock"), None);
        assert_eq!(join_inside(root, base, "../../../etc/passwd"), None);
        assert_eq!(
            join_inside(root, "/eryx/environment/modules/a/b.js", "../c.js").as_deref(),
            Some("/eryx/environment/modules/c.js")
        );
    }

    #[test]
    fn builtins() {
        assert_eq!(builtin("node:fs"), Some("fs"));
        assert_eq!(builtin("fs/promises"), Some("fs/promises"));
        assert_eq!(builtin("zod"), None);
        assert_eq!(quote("a\"b\\c\n"), r#""a\"b\\c\n""#);
    }
}
