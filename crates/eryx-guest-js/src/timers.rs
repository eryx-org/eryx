//! `setTimeout` and friends (`src/timers.js`): the guest keeps the timers,
//! and `src/js.rs` runs them, waiting on the host's `sleep` import for the
//! next deadline.

use rquickjs::{Ctx, Function, Object};

const TIMERS_JS: &str = include_str!("timers.js");

/// Install the timer globals, and return the event loop's side of them
/// (`runDue`, `state`, `clear`).
pub(crate) fn install<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Object<'js>> {
    let native = Object::new(ctx.clone())?;
    native.set("now", Function::new(ctx.clone(), crate::fresh::now)?)?;
    let install: Function<'_> = ctx.eval(TIMERS_JS)?;
    install.call((native,))
}
