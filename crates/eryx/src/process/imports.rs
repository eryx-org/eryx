//! What a command imports, against what children are given (#994), so a
//! command that can't run is refused at load with a reason per interface.

use wasmtime::Engine;
use wasmtime::component::Component;

use crate::error::{Error, ErrorKind};

/// WASI packages children get, at 0.2.x (wasmtime-wasi links 0.2.12, and
/// older 0.2 imports resolve to it) and 0.3.x.
const WASI_P2: &[&str] = &[
    "wasi:cli",
    "wasi:clocks",
    "wasi:filesystem",
    "wasi:io",
    "wasi:random",
    "wasi:sockets",
];
const WASI_P3: &[&str] = &[
    "wasi:cli",
    "wasi:clocks",
    "wasi:filesystem",
    "wasi:random",
    "wasi:sockets",
];

/// What [`Command::inspect`](crate::Command::inspect) found in a command.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CommandInfo {
    /// The WASI version it runs as: `"0.3"` if it exports 0.3's
    /// `wasi:cli/run`, `"0.2"` if 0.2's, `None` if neither (not a command).
    pub wasi: Option<&'static str>,
    /// Each interface it imports, and whether children get it.
    pub imports: Vec<CommandImport>,
    /// The names it exports.
    pub exports: Vec<String>,
    /// Why it can't be registered as a command, if it can't: an import
    /// children don't get, or no `wasi:cli/run` export. Loading it is still
    /// the final word: it also checks each interface's functions and types.
    pub problem: Option<String>,
}

impl CommandInfo {
    /// Whether it loads as a command.
    pub fn runnable(&self) -> bool {
        self.problem.is_none()
    }
}

/// One interface a command imports.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommandImport {
    /// Its name, e.g. `wasi:http/outgoing-handler@0.2.0`.
    pub name: String,
    /// Whether children get it.
    pub support: ImportSupport,
}

/// Whether children get an interface a command imports.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImportSupport {
    /// Children get it.
    Provided,
    /// `eryx:process/run`: linked, but its calls are refused unless the
    /// command is registered with [`allow_spawn`](crate::Command::allow_spawn).
    NeedsSpawn,
    /// `wasi:filesystem`: linked, but children see files only with eryx's
    /// `unstable-vfs` feature; without it they have no preopened
    /// directories, so opening files fails.
    NeedsVfs,
    /// Children don't get it, and why.
    Missing(String),
}

impl std::fmt::Display for CommandImport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.support {
            ImportSupport::Provided => write!(f, "{}: provided", self.name),
            ImportSupport::NeedsSpawn => write!(
                f,
                "{}: provided, but only commands registered with allow_spawn may run commands",
                self.name
            ),
            ImportSupport::NeedsVfs => write!(
                f,
                "{}: provided; children see files only with eryx's unstable-vfs feature",
                self.name
            ),
            ImportSupport::Missing(why) => write!(f, "{}: not provided: {why}", self.name),
        }
    }
}

/// Whether children get the interface `name`.
pub(super) fn support(name: &str) -> ImportSupport {
    let (path, version) = name.split_once('@').unwrap_or((name, ""));
    let package = path.split_once('/').map_or(path, |(package, _)| package);
    // A prerelease (0.3.0-rc-...) only matches itself, and none is linked.
    let minor = |prefix: &str| version.starts_with(prefix) && !version.contains('-');
    let known = |list: &[&str]| list.contains(&package);
    if package == "eryx:process" {
        return if path == "eryx:process/run" && minor("0.1.") {
            ImportSupport::NeedsSpawn
        } else {
            ImportSupport::Missing("children get only eryx:process/run@0.1".into())
        };
    }
    if path == "wasi:cli/run" {
        return ImportSupport::Missing(
            "wasi:cli/run is what a command exports, not something children get".into(),
        );
    }
    if (minor("0.2.") && known(WASI_P2)) || (minor("0.3.") && known(WASI_P3)) {
        return if package == "wasi:filesystem" {
            ImportSupport::NeedsVfs
        } else {
            ImportSupport::Provided
        };
    }
    ImportSupport::Missing(if package == "wasi:http" {
        "children get no wasi:http; they reach the network over sockets, \
         through the sandbox's proxy (HTTPS_PROXY), when it gives them network"
            .into()
    } else if known(WASI_P2) || known(WASI_P3) {
        let versions = if known(WASI_P3) {
            "0.2.x and 0.3.x"
        } else {
            "0.2.x"
        };
        format!(
            "children get {package} {versions}, not {}",
            version_or_none(version)
        )
    } else if package.starts_with("wasi:") {
        format!("children don't get {package}")
    } else {
        "children get only WASI (cli, clocks, filesystem, io, random, sockets) \
         and eryx:process/run"
            .into()
    })
}

fn version_or_none(version: &str) -> &str {
    if version.is_empty() {
        "unversioned"
    } else {
        version
    }
}

/// What `imports` and `exports` say about a command.
fn info<'a>(
    imports: impl IntoIterator<Item = &'a str>,
    exports: impl IntoIterator<Item = &'a str>,
) -> CommandInfo {
    let exports: Vec<String> = exports.into_iter().map(str::to_owned).collect();
    let wasi = if exports.iter().any(|e| e.starts_with("wasi:cli/run@0.3.")) {
        Some("0.3")
    } else if exports.iter().any(|e| e.starts_with("wasi:cli/run@0.2.")) {
        Some("0.2")
    } else {
        None
    };
    let imports: Vec<CommandImport> = imports
        .into_iter()
        .map(|name| CommandImport {
            name: name.to_owned(),
            support: support(name),
        })
        .collect();
    let missing: Vec<&CommandImport> = imports
        .iter()
        .filter(|i| matches!(i.support, ImportSupport::Missing(_)))
        .collect();
    let problem = if !missing.is_empty() {
        Some(missing_message(missing))
    } else if wasi.is_none() {
        Some(NOT_A_COMMAND.to_owned())
    } else {
        None
    };
    CommandInfo {
        wasi,
        imports,
        exports,
        problem,
    }
}

const NOT_A_COMMAND: &str =
    "not a WASI command: it exports no wasi:cli/run (0.2 or 0.3), so it's a library or a reactor";

pub(crate) fn missing_message<'a>(missing: impl IntoIterator<Item = &'a CommandImport>) -> String {
    let mut message = String::from("the command imports what children don't get:");
    for import in missing {
        message.push_str(&format!("\n  {import}"));
    }
    message
}

/// Inspect a command component's bytes, reading its top-level imports and
/// exports without compiling it.
pub(super) fn inspect(wasm: &[u8]) -> Result<CommandInfo, Error> {
    use wasmparser::{Encoding, Parser, Payload};

    let invalid = |e: wasmparser::BinaryReaderError| {
        Error::from(ErrorKind::Initialization(format!("invalid command: {e}")))
    };
    let (mut imports, mut exports): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    // How deep in nested modules and components the walk is: only the
    // outermost component's imports and exports are the command's.
    let mut depth = 0usize;
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.map_err(invalid)? {
            Payload::Version { encoding, .. } if depth == 0 => {
                if encoding != Encoding::Component {
                    return Err(ErrorKind::Initialization(
                        "not a component: a core module isn't a WASI command".into(),
                    )
                    .into());
                }
            }
            Payload::ModuleSection { .. } | Payload::ComponentSection { .. } => depth += 1,
            Payload::End(_) if depth > 0 => depth -= 1,
            Payload::ComponentImportSection(section) if depth == 0 => {
                for import in section {
                    imports.push(import.map_err(invalid)?.name.full_name().into_owned());
                }
            }
            Payload::ComponentExportSection(section) if depth == 0 => {
                for export in section {
                    exports.push(export.map_err(invalid)?.name.full_name().into_owned());
                }
            }
            _ => {}
        }
    }
    Ok(info(
        imports.iter().map(String::as_str),
        exports.iter().map(String::as_str),
    ))
}

/// Explain why `component` couldn't be pre-instantiated as a command
/// (`err`): the imports children don't get, or that it isn't a command.
/// Anything else passes wasmtime's error through.
pub(super) fn explain(engine: &Engine, component: &Component, err: wasmtime::Error) -> Error {
    let ty = component.component_type();
    let info = info(
        ty.imports(engine).map(|(name, _)| name),
        ty.exports(engine).map(|(name, _)| name),
    );
    let missing: Vec<CommandImport> = info
        .imports
        .into_iter()
        .filter(|i| matches!(i.support, ImportSupport::Missing(_)))
        .collect();
    if !missing.is_empty() {
        return ErrorKind::CommandImports { missing }.into();
    }
    if info.wasi.is_none() {
        return ErrorKind::Initialization(NOT_A_COMMAND.into()).into();
    }
    ErrorKind::wasm_component(err).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_are_classified() {
        for name in [
            "wasi:cli/environment@0.2.0",
            "wasi:io/streams@0.2.12",
            "wasi:sockets/tcp@0.2.3",
            "wasi:cli/stdout@0.3.0",
        ] {
            assert_eq!(support(name), ImportSupport::Provided, "{name}");
        }
        assert_eq!(support("eryx:process/run@0.1.0"), ImportSupport::NeedsSpawn);
        assert_eq!(
            support("wasi:filesystem/preopens@0.2.0"),
            ImportSupport::NeedsVfs
        );
        for (name, why) in [
            ("wasi:http/outgoing-handler@0.2.0", "no wasi:http"),
            ("wasi:cli/environment@0.1.0", "0.2.x and 0.3.x, not 0.1.0"),
            ("wasi:io/streams@0.3.0", "wasi:io 0.2.x, not 0.3.0"),
            (
                "wasi:random/random@0.3.0-rc-2025-09-16",
                "0.2.x and 0.3.x, not 0.3.0-rc-2025-09-16",
            ),
            ("wasi:keyvalue/store@0.2.0", "don't get wasi:keyvalue"),
            ("eryx:process/process@0.1.0", "only eryx:process/run"),
            ("my:app/thing", "only WASI"),
            ("wasi:cli/run@0.2.0", "what a command exports"),
        ] {
            match support(name) {
                ImportSupport::Missing(m) => assert!(m.contains(why), "{name}: {m}"),
                other => panic!("{name}: {other:?}"),
            }
        }
    }
}
