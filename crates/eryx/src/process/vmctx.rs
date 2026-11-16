//! The host memory a command's instances need for their metadata (#942).
//!
//! Each core instance's `VMContext` is allocated on the host heap at its
//! actual size, outside the child's linear memory, so the child memory
//! limiter never sees it. It's sized by the module: 32 bytes per imported
//! function and per function referenced from outside code (gh's 40,000 take
//! 1.3 MB), plus a little per table, memory and global. [`estimate`] works it out from a component's core
//! modules with wasmtime's own layout ([`VMOffsets`]), so a child can be
//! charged it before it runs.
//!
//! A precompiled command has no wasm left to read, so
//! [`Command::stamp_precompiled`](super::Command::stamp_precompiled) appends
//! the figure to it, in the trailer format runtimes' language stamps use.

use std::collections::HashSet;

use wasmparser::{
    ComponentExternalKind, ConstExpr, ElementItems, ExternalKind, Instance, Operator, Parser,
    Payload, TypeRef,
};
use wasmtime_environ::{HostPtr, VMOffsets, VMOffsetsFields};

/// What a precompiled command without a stamp is charged: wasmtime's pooling
/// limit on one core instance's metadata (`CORE_INSTANCE_SIZE`).
#[cfg(any(feature = "embedded", feature = "preinit", feature = "packages"))]
pub(super) const UNSTAMPED: u64 = 4 << 20;

/// Allowance per core instance for wasmtime's `Instance` itself, which
/// precedes its `VMContext` in the same allocation.
const INSTANCE_HEADER: u64 = 512;

/// Allowance per component for its `VMComponentContext`.
const COMPONENT_HEADER: u64 = 16 * 1024;

/// The stamp's tag in the language-stamp trailer: not a language.
const STAMP_TAG: u8 = 0x80;

/// Bytes of metadata `wasm`'s core instances need, summed over every core
/// instance the component creates.
pub(super) fn estimate(wasm: &[u8]) -> u64 {
    instance_sizes(wasm).iter().sum::<u64>() + COMPONENT_HEADER
}

/// The largest core instance's share of [`estimate`].
#[cfg(test)]
pub(super) fn largest(wasm: &[u8]) -> u64 {
    instance_sizes(wasm).into_iter().max().unwrap_or(0)
}

/// Each core instance's metadata, in the order the component creates them.
fn instance_sizes(wasm: &[u8]) -> Vec<u64> {
    // Per component scope (nested components have their own), the sizes of
    // the core modules it defines, by module index.
    let mut scopes: Vec<Vec<u64>> = vec![Vec::new()];
    let mut instances = Vec::new();
    let mut module: Option<Counts> = None;
    let mut depth = 0usize;
    for payload in Parser::new(0).parse_all(wasm) {
        let Ok(payload) = payload else { break };
        if let Some(counts) = module.as_mut() {
            if counts.add(&payload) {
                let size = counts.size();
                module = None;
                if let Some(scope) = scopes.last_mut() {
                    scope.push(size);
                }
            }
            continue;
        }
        match payload {
            Payload::ModuleSection { .. } => module = Some(Counts::default()),
            Payload::ComponentSection { .. } => {
                depth += 1;
                scopes.push(Vec::new());
            }
            Payload::End(_) if depth > 0 => {
                depth -= 1;
                scopes.pop();
            }
            Payload::InstanceSection(section) => {
                let Some(scope) = scopes.last() else { continue };
                for instance in section.into_iter().flatten() {
                    if let Instance::Instantiate { module_index, .. } = instance
                        && let Some(&size) = scope.get(module_index as usize)
                    {
                        instances.push(size);
                    }
                }
            }
            // Aliased or imported core modules take an index too.
            Payload::ComponentAliasSection(section) => {
                for alias in section.into_iter().flatten() {
                    if let wasmparser::ComponentAlias::InstanceExport {
                        kind: ComponentExternalKind::Module,
                        ..
                    } = alias
                        && let Some(scope) = scopes.last_mut()
                    {
                        scope.push(0);
                    }
                }
            }
            _ => {}
        }
    }
    instances
}

/// The counts `VMOffsets` needs, for one core module.
#[derive(Default)]
struct Counts {
    imported_functions: u32,
    imported_tables: u32,
    imported_memories: u32,
    imported_globals: u32,
    imported_tags: u32,
    /// Functions referenced from outside code (imported, exported, in an
    /// element segment or a constant expression), which get a `VMFuncRef`
    /// each.
    escaped: HashSet<u32>,
    tables: u32,
    memories: u32,
    owned_memories: u32,
    globals: u32,
    tags: u32,
    /// Passive data segments.
    segments: u32,
    start: bool,
}

impl Counts {
    /// Count one of the module's payloads; true at its end.
    fn add(&mut self, payload: &Payload<'_>) -> bool {
        match payload {
            Payload::ImportSection(section) => {
                for import in section.clone().into_imports().flatten() {
                    match import.ty {
                        // Imported functions always escape.
                        TypeRef::Func(_) | TypeRef::FuncExact(_) => {
                            self.escaped.insert(self.imported_functions);
                            self.imported_functions += 1;
                        }
                        TypeRef::Table(_) => self.imported_tables += 1,
                        TypeRef::Memory(_) => self.imported_memories += 1,
                        TypeRef::Global(_) => self.imported_globals += 1,
                        TypeRef::Tag(_) => self.imported_tags += 1,
                    }
                }
            }
            Payload::TableSection(section) => {
                for table in section.clone().into_iter().flatten() {
                    self.tables += 1;
                    if let wasmparser::TableInit::Expr(expr) = table.init {
                        self.escape_in(&expr);
                    }
                }
            }
            Payload::ExportSection(section) => {
                for export in section.clone().into_iter().flatten() {
                    if matches!(export.kind, ExternalKind::Func | ExternalKind::FuncExact) {
                        self.escaped.insert(export.index);
                    }
                }
            }
            Payload::MemorySection(section) => {
                for memory in section.clone().into_iter().flatten() {
                    self.memories += 1;
                    self.owned_memories += u32::from(!memory.shared);
                }
            }
            Payload::GlobalSection(section) => {
                for global in section.clone().into_iter().flatten() {
                    self.globals += 1;
                    self.escape_in(&global.init_expr);
                }
            }
            Payload::TagSection(section) => self.tags += section.count(),
            // wasmtime keeps passive segments at run time; active ones go
            // into each memory's initial image (one entry each).
            Payload::DataSection(section) => {
                for data in section.clone().into_iter().flatten() {
                    if matches!(data.kind, wasmparser::DataKind::Passive) {
                        self.segments += 1;
                    }
                }
            }
            Payload::ElementSection(section) => {
                for element in section.clone().into_iter().flatten() {
                    match element.items {
                        ElementItems::Functions(functions) => {
                            self.escaped.extend(functions.into_iter().flatten());
                        }
                        ElementItems::Expressions(_, exprs) => {
                            for expr in exprs.into_iter().flatten() {
                                self.escape_in(&expr);
                            }
                        }
                    }
                }
            }
            Payload::StartSection { .. } => self.start = true,
            Payload::End(_) => return true,
            _ => {}
        }
        false
    }

    /// Count the functions `expr` takes references to. (A body's `ref.func`
    /// must name one declared in an element segment, so these cover them.)
    fn escape_in(&mut self, expr: &ConstExpr<'_>) {
        for op in expr.get_operators_reader().into_iter().flatten() {
            if let Operator::RefFunc { function_index } = op {
                self.escaped.insert(function_index);
            }
        }
    }

    /// The instance's size: its `VMContext` and wasmtime's header.
    fn size(&self) -> u64 {
        let offsets = VMOffsets::from(VMOffsetsFields {
            ptr: HostPtr,
            num_imported_functions: self.imported_functions,
            num_imported_tables: self.imported_tables,
            num_imported_memories: self.imported_memories,
            num_imported_globals: self.imported_globals,
            num_imported_tags: self.imported_tags,
            num_defined_tables: self.tables,
            num_defined_memories: self.memories,
            num_owned_memories: self.owned_memories,
            num_defined_globals: self.globals,
            num_defined_tags: self.tags,
            num_escaped_funcs: u32::try_from(self.escaped.len()).unwrap_or(u32::MAX),
            num_runtime_data: self.segments + self.memories,
            has_startup_func: self.start,
        });
        u64::from(offsets.size_of_vmctx()) + INSTANCE_HEADER
    }
}

/// Append `bytes` (from [`estimate`]) to a precompiled command.
pub(super) fn stamp(precompiled: &mut Vec<u8>, bytes: u64) {
    precompiled.extend_from_slice(&bytes.to_le_bytes());
    precompiled.push(STAMP_TAG);
    precompiled.extend_from_slice(crate::wasm::LANGUAGE_STAMP_MAGIC);
}

/// The figure a precompiled command ending in `tail` was stamped with.
#[cfg(any(test, feature = "embedded", feature = "preinit", feature = "packages"))]
pub(super) fn stamped(tail: &[u8]) -> Option<u64> {
    let (rest, magic) = tail.split_last_chunk::<8>()?;
    let (&tag, rest) = rest.split_last()?;
    if magic != crate::wasm::LANGUAGE_STAMP_MAGIC || tag != STAMP_TAG {
        return None;
    }
    let (_, bytes) = rest.split_last_chunk::<8>()?;
    Some(u64::from_le_bytes(*bytes))
}

/// How many bytes at the end of a file [`stamped`] reads.
#[cfg(any(test, feature = "embedded", feature = "preinit", feature = "packages"))]
pub(super) const STAMP_LEN: usize = 8 + 1 + 8;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_round_trip() {
        let mut bytes = b"component".to_vec();
        stamp(&mut bytes, 1_276_940);
        assert_eq!(stamped(&bytes), Some(1_276_940));
        assert_eq!(stamped(&bytes[bytes.len() - STAMP_LEN..]), Some(1_276_940));
        assert_eq!(stamped(b"component"), None);
        // A runtime's language stamp isn't one.
        let mut runtime = b"component".to_vec();
        crate::Executor::stamp_language(&mut runtime, crate::Language::Shell);
        assert_eq!(stamped(&runtime), None);
    }
}
