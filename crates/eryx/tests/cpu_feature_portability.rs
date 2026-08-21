//! Guards that a precompiled artifact cannot inherit the build machine's CPU
//! features.
//!
//! Cranelift infers every feature flag from the host unless a target triple is
//! set. eryx used to rely on that and then disable a hand-written list of
//! "too new" flags, so any flag the list did not mention was baked into
//! artifacts that were supposed to be portable — which is how `has_avx512vnni`
//! reached the `x86-64-v2` cwasm shipped in release wheels the moment Cranelift
//! started detecting it.
//!
//! Note that loading an artifact is *not* a useful check here: wasmtime
//! validates each enabled ISA flag against the real host CPU, so a leaked flag
//! loads perfectly well on the machine that produced it and only fails
//! somewhere else. These tests therefore assert on the compiled bytes instead,
//! which is independent of what the CPU running them happens to support.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg(all(target_arch = "x86_64", any(feature = "embedded", feature = "preinit")))]

use eryx::{CpuFeatureLevel, PythonExecutor};
use wasmtime::{Config, Engine};

/// Every psABI level, i.e. everything except `Native`.
const PINNED_LEVELS: &[CpuFeatureLevel] = &[
    CpuFeatureLevel::X86_64,
    CpuFeatureLevel::X86_64v2,
    CpuFeatureLevel::X86_64v3,
    CpuFeatureLevel::X86_64v4,
];

/// The smallest thing wasmtime will precompile as a component.
fn tiny_component() -> Vec<u8> {
    wat::parse_str("(component)").expect("an empty component should parse")
}

/// A deliberately minimal, explicitly target-pinned engine for `level`.
///
/// Built straight from wasmtime rather than through eryx so it stays an
/// independent reference: pinning the triple is what stops Cranelift consulting
/// the host, and enabling the psABI preset is an allowlist on top of the
/// target's baseline.
fn reference_engine(level: CpuFeatureLevel) -> Engine {
    Engine::new(&reference_config(level)).expect("reference engine should build")
}

/// The config behind [`reference_engine`], exposed so a test can add flags.
fn reference_config(level: CpuFeatureLevel) -> Config {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    config.epoch_interruption(true);
    config.consume_fuel(true);
    config.memory_init_cow(true);
    config.cranelift_opt_level(wasmtime::OptLevel::SpeedAndSize);
    config.async_stack_size(512 * 1024);
    config
        .target(&target_lexicon::Triple::host().to_string())
        .expect("the host triple should be a valid target");
    // SAFETY: `as_str` yields a Cranelift x86 psABI preset name.
    unsafe {
        config.cranelift_flag_enable(level.as_str());
    }
    config
}

/// eryx's output for a feature level must match the target-pinned reference
/// exactly.
///
/// This guards the *pinning* rather than any particular flag, which is the
/// point: while the target is pinned, Cranelift never inspects the build
/// machine, so no CPU feature can leak in — including ones added in future
/// Cranelift releases, which is exactly what the old denylist could not
/// promise. If the production path ever regresses to host inference, these
/// bytes diverge on any machine richer than `level`.
#[test]
fn precompiled_output_matches_a_target_pinned_reference() {
    let wasm = tiny_component();

    for &level in PINNED_LEVELS {
        let actual = PythonExecutor::precompile_with_options(&wasm, None, level)
            .unwrap_or_else(|e| panic!("precompile at {level} failed: {e}"));
        let expected = reference_engine(level)
            .precompile_component(&wasm)
            .unwrap_or_else(|e| panic!("reference precompile at {level} failed: {e}"));

        assert!(
            actual == expected,
            "precompiled output at {level} does not match a target-pinned \
             reference build ({} vs {} bytes), so compilation is still \
             influenced by this machine's CPU features",
            actual.len(),
            expected.len()
        );
    }
}

/// The original regression, stated directly: the VNNI flags must not be able to
/// reach a portable artifact.
///
/// Forcing them off on top of the requested level has to make no difference. If
/// it does, something is switching them on behind our back — which is exactly
/// what happened to the `x86-64-v2` cwasm in release wheels once Cranelift
/// started detecting `has_avx512vnni`. The flag names here record that bug; no
/// production code consults them, and this test is not a list to maintain.
///
/// Both flags arrived in Cranelift 0.135 (wasmtime 48), so on an older
/// Cranelift this fails at `Engine::new` with an unknown-flag error rather than
/// an assertion.
#[test]
fn vnni_cannot_reach_a_portable_artifact() {
    let wasm = tiny_component();

    for &level in &[
        CpuFeatureLevel::X86_64,
        CpuFeatureLevel::X86_64v2,
        CpuFeatureLevel::X86_64v3,
    ] {
        let plain = reference_engine(level)
            .precompile_component(&wasm)
            .unwrap_or_else(|e| panic!("precompile at {level} failed: {e}"));

        let mut forced = reference_config(level);
        // SAFETY: both are Cranelift x86 flag names as of 0.135.
        unsafe {
            forced.cranelift_flag_set("has_avx512vnni", "false");
            forced.cranelift_flag_set("has_avx_vnni", "false");
        }
        let forced = Engine::new(&forced)
            .expect("engine with the VNNI flags forced off should build")
            .precompile_component(&wasm)
            .unwrap_or_else(|e| panic!("forced precompile at {level} failed: {e}"));

        assert!(
            plain == forced,
            "at {level}, explicitly disabling the VNNI flags changed the output \
             ({} vs {} bytes) — they are being enabled from the build machine",
            plain.len(),
            forced.len()
        );
    }
}

/// Naming a level must actually constrain the build.
///
/// Guards against the levels silently becoming no-ops — if `Native` and the
/// baseline `x86-64` ever produced identical output on a machine with features
/// above SSE2, the flags would not be reaching Cranelift at all.
#[test]
fn a_feature_level_constrains_the_build() {
    let wasm = tiny_component();

    let baseline = PythonExecutor::precompile_with_options(&wasm, None, CpuFeatureLevel::X86_64)
        .expect("baseline precompile should succeed");
    let native = PythonExecutor::precompile_with_options(&wasm, None, CpuFeatureLevel::Native)
        .expect("native precompile should succeed");

    // Any x86-64 CPU new enough to run the test suite has something above SSE2,
    // so the two must differ.
    assert!(
        baseline != native,
        "x86-64 and native produced identical artifacts ({} bytes), so the \
         requested feature level is not being applied",
        baseline.len()
    );
}

/// `Native` must keep inferring host features rather than being pinned to a
/// baseline, since it exists precisely to get the fastest code for this machine.
#[test]
fn native_is_not_pinned_to_a_level() {
    let wasm = tiny_component();

    let native = PythonExecutor::precompile_with_options(&wasm, None, CpuFeatureLevel::Native)
        .expect("native precompile should succeed");

    for &level in PINNED_LEVELS {
        let pinned = reference_engine(level)
            .precompile_component(&wasm)
            .unwrap_or_else(|e| panic!("reference precompile at {level} failed: {e}"));
        if native != pinned {
            return;
        }
    }

    panic!(
        "native output matched every pinned level, which suggests host feature \
         inference is no longer happening for CpuFeatureLevel::Native"
    );
}
