//! Web globals QuickJS lacks: `TextEncoder`, `TextDecoder` and `crypto`
//! (`src/web.js`), on these intrinsics.

use rquickjs::{ArrayBuffer, Ctx, Exception, Function, Object};

const WEB_JS: &str = include_str!("web.js");

pub(crate) fn install(ctx: &Ctx<'_>) -> rquickjs::Result<()> {
    let native = Object::new(ctx.clone())?;
    native.set(
        "encodeUtf8",
        Function::new(ctx.clone(), crate::fetch::encode_utf8)?,
    )?;
    native.set("decodeUtf8", Function::new(ctx.clone(), decode_utf8)?)?;
    native.set("randomBytes", Function::new(ctx.clone(), random_bytes)?)?;
    let install: Function<'_> = ctx.eval(WEB_JS)?;
    install.call((native,))
}

/// `decodeUtf8(arrayBuffer, fatal)`: the bytes as a string. Invalid UTF-8 is
/// replaced with U+FFFD, or with `fatal` throws a `TypeError`.
fn decode_utf8<'js>(
    ctx: Ctx<'js>,
    buffer: ArrayBuffer<'js>,
    fatal: bool,
) -> rquickjs::Result<String> {
    // SAFETY: the bytes are copied before any JavaScript runs.
    let bytes = unsafe { buffer.as_bytes() }.unwrap_or_default();
    if fatal {
        return match std::str::from_utf8(bytes) {
            Ok(text) => Ok(text.to_owned()),
            Err(_) => Err(Exception::throw_type(
                &ctx,
                "The encoded data was not valid utf-8",
            )),
        };
    }
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// `randomBytes(n)`: `n` bytes from the host's `wasi:random`, as an
/// `ArrayBuffer`. `crypto.getRandomValues` caps `n` at 65536.
fn random_bytes(ctx: Ctx<'_>, len: usize) -> rquickjs::Result<ArrayBuffer<'_>> {
    let mut bytes = vec![0u8; len];
    crate::fresh::fill_random(&mut bytes);
    ArrayBuffer::new(ctx, bytes)
}
