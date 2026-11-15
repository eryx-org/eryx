//! Calls to the `eryx:net` TCP and TLS imports, shared by the guests.
//!
//! The host runs them on a fiber, so they complete synchronously from the
//! guest's perspective.

use wit_dylib_ffi::{Call, Wit};

use crate::{EryxCall, Value};

const TCP_INTERFACE: &str = "eryx:net/tcp@0.1.0";
const TLS_INTERFACE: &str = "eryx:net/tls@0.1.0";

/// A `tcp-error` or `tls-error` returned by the host.
#[derive(Debug)]
pub struct NetError {
    /// The WIT case name, e.g. `timed-out` or `handshake-failed`. A
    /// `tls-error::tcp` is flattened to the `tcp-error` case it wraps.
    pub kind: &'static str,
    /// The case's payload, for the cases that carry one.
    pub message: Option<String>,
}

/// Call a network import and decode its `result<T, tcp-error | tls-error>`.
///
/// `push_args` pushes the arguments in reverse declaration order and `pop_ok`
/// pops the success payload. The outer `Err` is a guest/host protocol failure;
/// the inner one is a network error reported by the host.
fn call_net<T>(
    wit: Wit,
    interface: &str,
    name: &str,
    push_args: impl FnOnce(&mut EryxCall),
    pop_ok: impl FnOnce(Option<Value>) -> Option<T>,
) -> Result<Result<T, NetError>, String> {
    let import_func = wit
        .get_import(Some(interface), name)
        .ok_or_else(|| format!("{interface}.{name} import not found - networking not enabled"))?;

    let mut cx = EryxCall::new();
    push_args(&mut cx);
    import_func.call_import_sync(&mut cx);

    match cx.stack.pop() {
        Some(Value::ResultDiscriminant(true)) => match pop_ok(cx.stack.pop()) {
            Some(value) => Ok(Ok(value)),
            None => Err(format!("{name}: unexpected result value")),
        },
        Some(Value::ResultDiscriminant(false)) => {
            pop_net_error(&mut cx, interface == TLS_INTERFACE).map(Err)
        }
        other => Err(format!("{name}: unexpected result discriminant: {other:?}")),
    }
}

/// Pop a lifted `tcp-error` (or `tls-error` when `tls`) off the stack.
///
/// A lifted variant is its discriminant on top of its payload, if any, so
/// `tls-error::tcp(e)` is the outer discriminant on top of the lifted `e`.
fn pop_net_error(cx: &mut EryxCall, tls: bool) -> Result<NetError, String> {
    let discr = match cx.stack.pop() {
        Some(Value::U32(d)) => d,
        other => return Err(format!("unexpected error discriminant: {other:?}")),
    };
    let (kind, has_payload) = if tls {
        match discr {
            0 => return pop_net_error(cx, false),
            1 => ("handshake-failed", true),
            2 => ("certificate-error", true),
            3 => ("invalid-handle", false),
            _ => ("unknown-error", false),
        }
    } else {
        match discr {
            0 => ("connection-refused", false),
            1 => ("connection-reset", false),
            2 => ("timed-out", false),
            3 => ("host-not-found", false),
            4 => ("io-error", true),
            5 => ("not-permitted", true),
            6 => ("invalid-handle", false),
            _ => ("unknown-error", false),
        }
    };
    let message = if has_payload {
        match cx.stack.pop() {
            Some(Value::String(s)) => Some(s),
            other => return Err(format!("unexpected {kind} payload: {other:?}")),
        }
    } else {
        None
    };
    Ok(NetError { kind, message })
}

fn pop_handle(value: Option<Value>) -> Option<u32> {
    match value {
        Some(Value::U32(v)) => Some(v),
        _ => None,
    }
}

fn pop_bytes(value: Option<Value>) -> Option<Vec<u8>> {
    match value {
        Some(Value::Bytes(b)) => Some(b),
        _ => None,
    }
}

/// Connect to `host:port` over TCP, returning the connection handle.
pub fn tcp_connect(
    wit: Wit,
    host: &str,
    port: u16,
    timeout_ms: u32,
) -> Result<Result<u32, NetError>, String> {
    call_net(
        wit,
        TCP_INTERFACE,
        "connect",
        |cx| {
            // Reverse declaration order (host, port, timeout-ms).
            cx.push_u32(timeout_ms);
            cx.push_u16(port);
            cx.push_string(host.to_string());
        },
        pop_handle,
    )
}

/// Upgrade a TCP connection to TLS, returning the TLS handle.
pub fn tls_upgrade(
    wit: Wit,
    tcp_handle: u32,
    hostname: &str,
    timeout_ms: u32,
) -> Result<Result<u32, NetError>, String> {
    call_net(
        wit,
        TLS_INTERFACE,
        "upgrade",
        |cx| {
            // Reverse declaration order (tcp, hostname, timeout-ms).
            cx.push_u32(timeout_ms);
            cx.push_string(hostname.to_string());
            cx.push_u32(tcp_handle);
        },
        pop_handle,
    )
}

/// Read up to `len` bytes from a TCP (or, with `tls`, TLS) connection.
pub fn net_read(
    wit: Wit,
    tls: bool,
    handle: u32,
    len: u32,
    timeout_ms: u32,
) -> Result<Result<Vec<u8>, NetError>, String> {
    call_net(
        wit,
        if tls { TLS_INTERFACE } else { TCP_INTERFACE },
        "read",
        |cx| {
            // Reverse declaration order (handle, len, timeout-ms).
            cx.push_u32(timeout_ms);
            cx.push_u32(len);
            cx.push_u32(handle);
        },
        pop_bytes,
    )
}

/// Write to a TCP (or, with `tls`, TLS) connection, returning bytes written.
pub fn net_write(
    wit: Wit,
    tls: bool,
    handle: u32,
    data: &[u8],
    timeout_ms: u32,
) -> Result<Result<u32, NetError>, String> {
    call_net(
        wit,
        if tls { TLS_INTERFACE } else { TCP_INTERFACE },
        "write",
        |cx| {
            // Reverse declaration order (handle, timeout-ms, data). The `data`
            // list is the last declared argument so the guest's zero-copy
            // byte-list lowering (which does not pop the value) leaves nothing
            // on the stack for a later scalar pop to trip over.
            cx.stack.push(Value::Bytes(data.to_vec()));
            cx.push_u32(timeout_ms);
            cx.push_u32(handle);
        },
        pop_handle,
    )
}

/// Close a TCP (or, with `tls`, TLS) connection. Does nothing if networking
/// isn't enabled.
pub fn net_close(wit: Wit, tls: bool, handle: u32) {
    let interface = if tls { TLS_INTERFACE } else { TCP_INTERFACE };
    if let Some(import_func) = wit.get_import(Some(interface), "close") {
        let mut cx = EryxCall::new();
        cx.push_u32(handle);
        import_func.call_import_sync(&mut cx);
    }
}
