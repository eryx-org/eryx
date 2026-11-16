//! Per-instance state that QuickJS seeds when it creates a context.
//!
//! `JS_NewContext` seeds `Math.random()` from the time of day and starts the
//! `performance` clock. A preinit snapshot captures the context, so every
//! instance restored from it would replay the same `Math.random()` sequence
//! and measure `performance.now()` from the snapshot's clock. [`reseed`]
//! replaces both on the first execute in every context, with or without
//! preinit, so the two behave the same.
//!
//! Not reseeded: QuickJS's internal `hash_seed`, derived from the same
//! state, which no API exposes. Instances from one snapshot share it; it
//! keys `Map`/`Set` hashing, so it only matters for hash-flooding resistance
//! of those collections.

use std::cell::Cell;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rquickjs::object::Property;
use rquickjs::{Ctx, Function, Object};

unsafe extern "C" {
    /// wasi-libc: fills `buf` from `wasi:random`.
    fn getentropy(buf: *mut u8, len: usize) -> i32;
}

thread_local! {
    static RESEEDED: Cell<bool> = const { Cell::new(false) };
    static RANDOM_STATE: Cell<u64> = const { Cell::new(1) };
    static TIME_ORIGIN: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Fill `buf` from the host's `wasi:random`.
pub(crate) fn fill_random(buf: &mut [u8]) {
    // getentropy takes at most 256 bytes a call.
    for chunk in buf.chunks_mut(256) {
        // SAFETY: `chunk` is a valid, writable buffer of the given length.
        let status = unsafe { getentropy(chunk.as_mut_ptr(), chunk.len()) };
        assert_eq!(status, 0, "getentropy failed");
    }
}

/// Give this instance its own `Math.random()` seed and `performance` clock,
/// once.
pub(crate) fn reseed(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    if RESEEDED.replace(true) {
        return Ok(());
    }
    let mut seed = [0u8; 8];
    fill_random(&mut seed);
    // xorshift64* needs a nonzero state.
    RANDOM_STATE.set(u64::from_ne_bytes(seed).max(1));
    TIME_ORIGIN.set(Some(Instant::now()));

    let globals = ctx.globals();
    let math: Object<'_> = globals.get("Math")?;
    // Assignment keeps the property's attributes (writable, configurable,
    // not enumerable), as for the built-in.
    math.set(
        "random",
        Function::new(ctx.clone(), random)?.with_name("random")?,
    )?;

    let origin_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64() * 1000.0);
    let performance = Object::new(ctx.clone())?;
    performance.prop(
        "now",
        Property::from(Function::new(ctx.clone(), now)?.with_name("now")?).enumerable(),
    )?;
    performance.prop("timeOrigin", Property::from(origin_ms).enumerable())?;
    globals.set("performance", performance)
}

/// Reseed again on the next execute: before a preinit snapshot is taken, so
/// each instance restored from it reseeds, and when the context is replaced.
pub(crate) fn forget() {
    RESEEDED.set(false);
}

/// `Math.random()`: QuickJS's own generator (xorshift64*, 52 bits of
/// mantissa), on this instance's state.
fn random() -> f64 {
    let mut x = RANDOM_STATE.get();
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    RANDOM_STATE.set(x);
    let v = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
    f64::from_bits((0x3ff << 52) | (v >> 12)) - 1.0
}

/// `performance.now()`: milliseconds since this instance's first execute.
fn now() -> f64 {
    TIME_ORIGIN
        .get()
        .map_or(0.0, |origin| origin.elapsed().as_secs_f64() * 1000.0)
}
