//! WASI preview 1 command modules, adapted into WASI 0.2 command components
//! on load (#993), as `wasm-tools component new --adapt` does.

use std::borrow::Cow;

use wasmparser::{ExternalKind, Parser, Payload};

use crate::error::{Error, ErrorKind};

/// Wasmtime's `wasi_snapshot_preview1.command.wasm`, from the release of
/// the wasmtime eryx links (see third_party/manual.toml, which pins both).
const ADAPTER: &[u8] = include_bytes!("../../adapters/wasi_snapshot_preview1.command.wasm");

/// `wasm` as a component: unchanged if it is one, adapted if it's a WASI
/// preview 1 command module, and encoded from its `component-type` section
/// if it's a core module carrying one (as a wasip3 Go binary does).
pub(super) fn to_component(wasm: &[u8]) -> Result<Cow<'_, [u8]>, Error> {
    if !Parser::is_core_wasm(wasm) {
        return Ok(Cow::Borrowed(wasm));
    }
    let Scan {
        preview1,
        start,
        component_type,
    } = scan(wasm)?;
    let refuse = |why: &str| Err(ErrorKind::Initialization(why.into()).into());
    if !preview1 && component_type {
        return encode(wasm);
    }
    if !preview1 {
        return refuse(
            "a core module that doesn't import wasi_snapshot_preview1 isn't a WASI command",
        );
    }
    if !start {
        return refuse("a WASI preview 1 reactor (it exports no _start), not a command");
    }
    adapt(wasm)
}

/// Whether `wasm` is a WASI preview 1 module: a core module importing
/// `wasi_snapshot_preview1`, whether or not it's a command eryx can adapt.
pub(super) fn is_preview1(wasm: &[u8]) -> bool {
    Parser::is_core_wasm(wasm) && scan(wasm).is_ok_and(|scan| scan.preview1)
}

/// What [`scan`] finds in a core module.
struct Scan {
    /// It imports `wasi_snapshot_preview1`.
    preview1: bool,
    /// It exports a `_start` function.
    start: bool,
    /// It has a `component-type` custom section, which says what component
    /// it's the core of.
    component_type: bool,
}

/// What core module `wasm` imports, exports and carries (see [`Scan`]).
fn scan(wasm: &[u8]) -> Result<Scan, Error> {
    let invalid = |e: wasmparser::BinaryReaderError| {
        Error::from(ErrorKind::Initialization(format!("invalid module: {e}")))
    };
    let (mut preview1, mut start, mut component_type) = (false, false, false);
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.map_err(invalid)? {
            Payload::ImportSection(section) => {
                for imports in section {
                    for import in imports.map_err(invalid)? {
                        preview1 |= import.map_err(invalid)?.1.module == "wasi_snapshot_preview1";
                    }
                }
            }
            Payload::ExportSection(section) => {
                for export in section {
                    let export = export.map_err(invalid)?;
                    start |= export.name == "_start" && export.kind == ExternalKind::Func;
                }
            }
            Payload::CustomSection(section) => {
                component_type |= section.name().starts_with("component-type");
            }
            _ => {}
        }
    }
    Ok(Scan {
        preview1,
        start,
        component_type,
    })
}

/// Encode core module `wasm` as the component its `component-type` section
/// describes.
fn encode(wasm: &[u8]) -> Result<Cow<'_, [u8]>, Error> {
    wit_component_wasmtime::ComponentEncoder::default()
        .validate(true)
        .module(wasm)
        .and_then(|encoder| encoder.encode())
        .map(Cow::Owned)
        .map_err(|e| {
            ErrorKind::Initialization(format!(
                "couldn't encode the core module as a component: {e:#}"
            ))
            .into()
        })
}

fn adapt(wasm: &[u8]) -> Result<Cow<'_, [u8]>, Error> {
    wit_component_wasmtime::ComponentEncoder::default()
        .validate(true)
        .module(wasm)
        .and_then(|encoder| encoder.adapter("wasi_snapshot_preview1", ADAPTER))
        .and_then(|encoder| encoder.encode())
        .map(Cow::Owned)
        .map_err(|e| {
            ErrorKind::Initialization(format!(
                "couldn't adapt the WASI preview 1 module into a component: {e:#}"
            ))
            .into()
        })
}
