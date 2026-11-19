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
//! [`Executor::precompile`](crate::Executor::precompile) appends the figure
//! to it, in the trailer format runtimes' language stamps use.

use std::collections::HashSet;

use wasmparser::{
    ComponentAlias, ComponentExternalKind, ComponentInstance, ComponentOuterAliasKind,
    ComponentTypeRef, ConstExpr, ElementItems, ExternalKind, Instance, Operator, Parser, Payload,
    TypeRef,
};
use wasmtime_environ::{HostPtr, VMOffsets, VMOffsetsFields};

/// What a precompiled command without a stamp is charged for its instances'
/// metadata: a conservative guess, usually enough (gh, the largest measured,
/// needs 1.3 MB across its instances), since there's no wasm left to size it
/// from. It's charged to the child memory pool, which is what bounds every
/// child's metadata.
#[cfg(any(feature = "embedded", feature = "preinit", feature = "env-pack"))]
pub(super) const UNSTAMPED: u64 = UNMODELLED_INSTANCE;

/// What each core instance is charged in a component [`estimate`] doesn't
/// model (nested components, imported or aliased core modules): the same
/// conservative guess as for an unstamped command, per instance, or the
/// largest core module's size if that's more.
const UNMODELLED_INSTANCE: u64 = 4 << 20;

/// The most core instances a child's store can create: wasmtime's default,
/// which [`ChildMemory`](super::ChildMemory) doesn't change. A component
/// whose core instances can't be counted here (one imported into a nested
/// component) is taken to create this many.
const MAX_INSTANCES: u64 = wasmtime::DEFAULT_INSTANCE_LIMIT as u64;

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
    let instances = match instance_sizes(wasm) {
        Ok(sizes) => sizes.iter().sum::<u64>(),
        Err(Unmodelled { instances, largest }) => UNMODELLED_INSTANCE
            .max(largest)
            .saturating_mul(instances.max(1)),
    };
    instances + COMPONENT_HEADER
}

/// The largest core instance's share of [`estimate`].
#[cfg(test)]
pub(super) fn largest(wasm: &[u8]) -> u64 {
    instance_sizes(wasm)
        .ok()
        .and_then(|sizes| sizes.into_iter().max())
        .unwrap_or(0)
}

/// A component [`instance_sizes`] doesn't model, with how many core
/// instances it creates (anywhere in it, counting each time a nested
/// component is instantiated) and its largest core module's size.
struct Unmodelled {
    instances: u64,
    largest: u64,
}

/// A component being walked: the core instances each component in its
/// index space creates, and how many it creates itself per instantiation.
#[derive(Default)]
struct Frame {
    components: Vec<u64>,
    instances: u64,
}

impl Frame {
    /// The core instances component `index` creates.
    fn component(&self, index: u32) -> u64 {
        self.components
            .get(index as usize)
            .copied()
            .unwrap_or(MAX_INSTANCES)
    }
}

/// Each core instance's metadata, in the order the component creates them.
///
/// Only the shape `wasm-component-ld` and `wit-component` produce is
/// modelled: core modules defined in the component itself, instantiated
/// there. Core modules or instances in a nested component, and imported or
/// aliased core modules (whose sizes, or instantiations, aren't where this
/// looks), are [`Unmodelled`], and charged conservatively.
fn instance_sizes(wasm: &[u8]) -> Result<Vec<u64>, Unmodelled> {
    // The sizes of the core modules the component defines, by module index.
    let mut modules: Vec<u64> = Vec::new();
    let mut instances = Vec::new();
    let mut module: Option<Counts> = None;
    let mut modelled = true;
    // The component being walked and those enclosing it, innermost last.
    // wit-component nests one to re-export `wasi:cli/run` with its types,
    // holding no core modules; only core modules or instances inside one
    // are unmodelled.
    let mut frames = vec![Frame::default()];
    for payload in Parser::new(0).parse_all(wasm) {
        let Ok(payload) = payload else { break };
        if let Some(counts) = module.as_mut() {
            if counts.add(&payload) {
                modules.push(counts.size());
                module = None;
            }
            continue;
        }
        let depth = frames.len() - 1;
        let Some(frame) = frames.last_mut() else {
            break;
        };
        match payload {
            Payload::ModuleSection { .. } => {
                modelled &= depth == 0;
                module = Some(Counts::default());
            }
            Payload::ComponentSection { .. } => frames.push(Frame::default()),
            Payload::End(_) if depth > 0 => {
                let instances = frame.instances;
                frames.pop();
                if let Some(parent) = frames.last_mut() {
                    parent.components.push(instances);
                }
            }
            Payload::InstanceSection(section) => {
                for instance in section.into_iter().flatten() {
                    if let Instance::Instantiate { module_index, .. } = instance {
                        frame.instances = frame.instances.saturating_add(1);
                        match modules.get(module_index as usize) {
                            Some(&size) if depth == 0 => instances.push(size),
                            _ => modelled = false,
                        }
                    }
                }
            }
            // Each instantiation of a nested component creates its core
            // instances again.
            Payload::ComponentInstanceSection(section) => {
                for instance in section.into_iter().flatten() {
                    if let ComponentInstance::Instantiate {
                        component_index, ..
                    } = instance
                    {
                        let created = frame.component(component_index);
                        frame.instances = frame.instances.saturating_add(created);
                    }
                }
            }
            // Core modules from elsewhere take module indices too, and
            // components from elsewhere component indices.
            Payload::ComponentImportSection(section) => {
                for import in section.into_iter().flatten() {
                    match import.ty {
                        ComponentTypeRef::Module(_) => modelled = false,
                        ComponentTypeRef::Component(_) => frame.components.push(MAX_INSTANCES),
                        _ => {}
                    }
                }
            }
            Payload::ComponentAliasSection(section) => {
                for alias in section.into_iter().flatten() {
                    match alias {
                        ComponentAlias::InstanceExport {
                            kind: ComponentExternalKind::Module,
                            ..
                        }
                        | ComponentAlias::Outer {
                            kind: ComponentOuterAliasKind::CoreModule,
                            ..
                        } => modelled = false,
                        ComponentAlias::InstanceExport {
                            kind: ComponentExternalKind::Component,
                            ..
                        } => frames[depth].components.push(MAX_INSTANCES),
                        ComponentAlias::Outer {
                            kind: ComponentOuterAliasKind::Component,
                            count,
                            index,
                        } => {
                            let created = depth
                                .checked_sub(count as usize)
                                .map_or(MAX_INSTANCES, |outer| frames[outer].component(index));
                            frames[depth].components.push(created);
                        }
                        _ => {}
                    }
                }
            }
            // An exported component takes a new index too.
            Payload::ComponentExportSection(section) => {
                for export in section.into_iter().flatten() {
                    if export.kind == ComponentExternalKind::Component {
                        let created = frame.component(export.index);
                        frame.components.push(created);
                    }
                }
            }
            _ => {}
        }
    }
    if modelled {
        Ok(instances)
    } else {
        Err(Unmodelled {
            instances: frames
                .first()
                .map_or(MAX_INSTANCES, |root| root.instances)
                .min(MAX_INSTANCES),
            largest: modules.iter().copied().max().unwrap_or(0),
        })
    }
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
#[cfg(any(test, feature = "embedded", feature = "preinit", feature = "env-pack"))]
pub(super) const STAMP_LEN: usize = 8 + 1 + 8;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A component this doesn't model is charged the conservative figure
    /// per core instance it creates, rather than mis-sized.
    #[test]
    fn unmodelled_components_are_charged_conservatively() {
        let nested = wat::parse_str(
            "(component (component $c (core module $m) \
             (core instance (instantiate $m)) (core instance (instantiate $m))) \
             (instance (instantiate $c)))",
        )
        .unwrap();
        assert_eq!(
            estimate(&nested),
            2 * UNMODELLED_INSTANCE + COMPONENT_HEADER
        );
        let imported = wat::parse_str(
            "(component (import \"m\" (core module $m)) (core instance (instantiate $m)))",
        )
        .unwrap();
        assert_eq!(estimate(&imported), UNMODELLED_INSTANCE + COMPONENT_HEADER);
        // What wit-component makes is modelled: a module defined and
        // instantiated here.
        let plain = wat::parse_str("(component (core module $m) (core instance (instantiate $m)))")
            .unwrap();
        assert!(estimate(&plain) < UNMODELLED_INSTANCE);
    }

    /// A nested component's core instances are charged each time it's
    /// instantiated, including through an outer alias or an export, and one
    /// whose instances can't be counted is charged as many as a store holds.
    #[test]
    fn nested_instantiations_are_each_charged() {
        let repeated = wat::parse_str(
            "(component \
               (component $c (core module $m) (core instance (instantiate $m))) \
               (component $d (alias outer 1 $c (component $c)) \
                 (instance (instantiate $c)) (instance (instantiate $c))) \
               (export $e \"e\" (component $d)) \
               (instance (instantiate $d)) (instance (instantiate $d)) \
               (instance (instantiate $e)))",
        )
        .unwrap();
        assert_eq!(
            estimate(&repeated),
            6 * UNMODELLED_INSTANCE + COMPONENT_HEADER
        );
        let imported = wat::parse_str(
            "(component \
               (component $c (core module $m) (core instance (instantiate $m))) \
               (component $d (import \"c\" (component $i)) (instance (instantiate $i))) \
               (instance (instantiate $d (with \"c\" (component $c)))))",
        )
        .unwrap();
        assert_eq!(
            estimate(&imported),
            MAX_INSTANCES * UNMODELLED_INSTANCE + COMPONENT_HEADER
        );
    }

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
