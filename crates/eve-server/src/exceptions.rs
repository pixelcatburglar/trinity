//! Exceptions the client can unmarshal, sent back inside a wrapped-exception error.
//!
//! In Trinity's Python 2.5 these are new-style classes (they derive from
//! `StandardError`), so they travel as a reduce: a global naming the class and
//! its constructor arguments, which is how the client writes its own. The
//! globals are the ones the client's marshal whitelist resolves
//! (`common/script/net/whitelist.py`).

use eve_marshal::Value;

/// `ccp_exceptions.UserError(msg[, dict])`: a refusal the client shows as a message box.
///
/// The global names the module the class lives in, not its guid
/// (`exceptions.UserError`): `ccp_exceptions` publishes the class into
/// `__builtin__`, not into `exceptions`, and the whitelist lists it this way.
/// `__init__` sets `msg`, `dict` and `args` from the arguments, so no state
/// follows them.
pub fn user_error(msg: &str, args: &[(&str, Value)]) -> Value {
    let mut ctor = vec![Value::bytes(msg)];
    if !args.is_empty() {
        ctor.push(Value::Dict(args.iter().map(|(k, v)| (Value::bytes(*k), v.clone())).collect()));
    }
    Value::reduce("ccp_exceptions.UserError", ctor)
}

/// A `CustomError` user error carrying free text.
pub fn custom_error(text: &str) -> Value {
    user_error("CustomError", &[("error", Value::bytes(text))])
}

/// `exceptions.ServiceNotFound(serviceName)`: what the server answers for a
/// call it does not implement.
pub fn service_not_found(what: &str) -> Value {
    Value::reduce("exceptions.ServiceNotFound", vec![Value::bytes(what)])
}
