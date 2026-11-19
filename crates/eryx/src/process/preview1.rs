//! WASI preview 1 command modules, adapted into WASI 0.2 command components
//! on load (#993), as `wasm-tools component new --adapt` does.

use std::borrow::Cow;

use wasmparser::{ExternalKind, Parser, Payload};

use crate::error::{Error, ErrorKind};

/// Wasmtime's `wasi_snapshot_preview1.command.wasm`, from the release of
/// the wasmtime eryx links (see third_party/manual.toml, which pins both).
const ADAPTER: &[u8] = include_bytes!("../../adapters/wasi_snapshot_preview1.command.wasm");

/// `wasm` as a component: unchanged if it is one, adapted if it's a WASI
/// preview 1 command module.
pub(super) fn to_component(wasm: &[u8]) -> Result<Cow<'_, [u8]>, Error> {
    if !Parser::is_core_wasm(wasm) {
        return Ok(Cow::Borrowed(wasm));
    }
    let (preview1, start) = scan(wasm)?;
    let refuse = |why: &str| Err(ErrorKind::Initialization(why.into()).into());
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
    Parser::is_core_wasm(wasm) && scan(wasm).is_ok_and(|(preview1, _)| preview1)
}

/// Whether core module `wasm` imports `wasi_snapshot_preview1`, and whether
/// it exports a `_start` function.
fn scan(wasm: &[u8]) -> Result<(bool, bool), Error> {
    let invalid = |e: wasmparser::BinaryReaderError| {
        Error::from(ErrorKind::Initialization(format!("invalid module: {e}")))
    };
    let (mut preview1, mut start) = (false, false);
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
            _ => {}
        }
    }
    Ok((preview1, start))
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
