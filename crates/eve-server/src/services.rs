//! Service calls: parsing a CallReq, the context a handler runs in, and the
//! registry that routes a call to the handlers for its service.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use eve_marshal::{StringTable, Value};
use eve_proto::{MachoPacket, ProtoError};
use std::sync::Arc;

use crate::bulkdata::BulkData;
use crate::session::Session;
use crate::world::World;
use crate::store::Store;
use crate::svc::bound;

pub struct Call {
    /// The service name, or for a bound object its full ID (`N=1:invbroker#60000004`).
    pub service: String,
    pub method: String,
    pub args: Vec<Value>,
    pub kwargs: Vec<(Value, Value)>,
}

impl Call {
    /// The service name with any bound-object ID stripped.
    pub fn service_name(&self) -> &str {
        bound::parse(&self.service).map_or(self.service.as_str(), |b| b.service)
    }

    pub fn arg_i64(&self, i: usize) -> Option<i64> {
        self.args.get(i).and_then(|v| v.as_i64())
    }

    /// An ID argument; 0 when missing or not an integer.
    pub fn arg_id(&self, i: usize) -> i64 {
        self.arg_i64(i).unwrap_or(0)
    }

    pub fn arg_text(&self, i: usize) -> Option<String> {
        self.args.get(i).and_then(|v| v.as_string())
    }

    pub fn kwarg(&self, name: &str) -> Option<&Value> {
        let key = Value::bytes(name);
        self.kwargs.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }
}

pub fn parse_call(packet: &MachoPacket, table: &StringTable) -> Result<Call, ProtoError> {
    let payload = packet.body.first().ok_or(ProtoError::Shape("call body"))?;
    let items = payload.seq().ok_or(ProtoError::Shape("call payload"))?;
    let bound = items.first().and_then(|v| v.as_i64()) == Some(1);
    let blob = match items.get(1) {
        Some(Value::MarshalStream(b)) | Some(Value::Bytes(b)) | Some(Value::Buffer(b)) => b,
        _ => return Err(ProtoError::Shape("call payload blob")),
    };

    let inner = eve_marshal::load(blob, table)?;
    let parts = inner.seq().ok_or(ProtoError::Shape("inner call tuple"))?;

    let service = packet
        .destination
        .as_ref()
        .and_then(|d| d.service())
        .or_else(|| if bound { parts.first().and_then(|v| v.as_bytes()) } else { None })
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .ok_or(ProtoError::Shape("call destination has no service"))?;

    let method = parts
        .get(1)
        .and_then(|m| m.as_bytes())
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .ok_or(ProtoError::Shape("call method name"))?;
    let args = parts.get(2).and_then(|a| a.seq()).map(|s| s.to_vec()).unwrap_or_default();
    let mut kwargs = match parts.get(3) {
        Some(Value::Dict(d)) => d.clone(),
        _ => Vec::new(),
    };
    // machoVersion is transport metadata, not an argument of the method.
    kwargs.retain(|(k, _)| *k != Value::bytes("machoVersion"));

    Ok(Call { service, method, args, kwargs })
}

pub fn make_response(
    req: &MachoPacket,
    value: &Value,
    table: &StringTable,
) -> Result<MachoPacket, ProtoError> {
    let blob = eve_marshal::save(value, table)?;
    Ok(req.respond(eve_proto::MessageType::CallRsp, vec![Value::Bytes(blob)]))
}

pub enum Outcome {
    Value(Value),
    /// Not implemented; the text says what, and goes back as ServiceNotFound.
    Unknown(String),
    UserError { msg: String, args: Vec<(String, Value)> },
}

impl Outcome {
    /// A `CustomError` refusal: the client shows `text` as is.
    pub fn custom(text: impl Into<String>) -> Self {
        Outcome::UserError {
            msg: "CustomError".into(),
            args: vec![("error".into(), Value::bytes(text.into()))],
        }
    }

    /// `NotEnoughMoney`, for a payment of `need` from a wallet holding `have`.
    pub fn not_enough_money(need: f64, have: f64) -> Self {
        Outcome::UserError {
            msg: "NotEnoughMoney".into(),
            args: vec![("amount".into(), Value::Double(need)), ("balance".into(), Value::Double(have))],
        }
    }
}

pub struct Ctx<'a> {
    pub bulk: &'a BulkData,
    pub dogma: &'a crate::dogma::Catalog,
    pub session: &'a Session,
    /// The live data (`store.read` / `store.write`); an `Arc` so a task that
    /// outlives the call can keep it.
    pub store: &'a Arc<Store>,
    pub node_id: i64,
    pub world: &'a World,
    /// The caller's connection, to queue notifications for it.
    pub client_id: i64,
}

pub type HandlerFuture<'a> = Pin<Box<dyn Future<Output = Option<Outcome>> + Send + 'a>>;

/// Serves the methods of one service it knows, and returns `None` for the rest.
pub type Handler = for<'a> fn(&'a Call, &'a Ctx<'a>) -> HandlerFuture<'a>;

/// Turn an `async fn(&Call, &Ctx) -> Option<Outcome>` into a [`Handler`].
macro_rules! handler {
    ($f:path) => {{
        let h: $crate::services::Handler = |call, ctx| Box::pin($f(call, ctx));
        h
    }};
}
pub(crate) use handler;

struct Service {
    name: &'static str,
    bound: bool,
    handlers: Vec<Handler>,
}

impl Service {
    async fn call(&self, call: &Call, ctx: &Ctx<'_>) -> Outcome {
        for handler in &self.handlers {
            if let Some(outcome) = handler(call, ctx).await {
                return outcome;
            }
        }
        Outcome::Unknown(format!("{}::{}", self.name, call.method))
    }

    /// `MachoBindObject(params, (method, args))`: hand out an object reference and,
    /// if the client piggybacked a call on the bind, its result.
    async fn bind(&self, call: &Call, ctx: &Ctx<'_>) -> Outcome {
        let params = bound::params_of(call);
        let retval = match bound::piggyback(call) {
            Some((method, args)) => {
                let inner = Call {
                    service: bound::object_id(ctx.node_id, self.name, &params),
                    method,
                    args,
                    kwargs: Vec::new(),
                };
                bound::piggyback_value(self.name, self.call(&inner, ctx).await)
            }
            None => Value::None,
        };
        Outcome::Value(bound::bind_result(ctx.node_id, self.name, &params, retval))
    }
}

/// Every service the server answers, by name.
#[derive(Default)]
pub struct Registry {
    services: HashMap<&'static str, Service>,
}

impl Registry {
    /// Add a service. Several handlers may share a name; they are tried in the
    /// order they were added.
    pub fn add(&mut self, name: &'static str, handler: Handler) -> &mut Self {
        self.entry(name, false).handlers.push(handler);
        self
    }

    /// Add a bound service: the client binds an object with `MachoResolveObject` /
    /// `MachoBindObject` before calling methods on it. The registry answers those two.
    pub fn add_bound(&mut self, name: &'static str, handler: Handler) -> &mut Self {
        self.entry(name, true).handlers.push(handler);
        self
    }

    fn entry(&mut self, name: &'static str, bound: bool) -> &mut Service {
        let svc = self
            .services
            .entry(name)
            .or_insert_with(|| Service { name, bound, handlers: Vec::new() });
        assert_eq!(svc.bound, bound, "service {name} registered as both bound and unbound");
        svc
    }

    pub fn len(&self) -> usize {
        self.services.len()
    }

    pub async fn dispatch(&self, call: &Call, ctx: &Ctx<'_>) -> Outcome {
        let name = call.service_name();
        let Some(svc) = self.services.get(name) else {
            return Outcome::Unknown(format!("{name}::{} (no such service)", call.method));
        };
        if svc.bound {
            match call.method.as_str() {
                "MachoResolveObject" => return Outcome::Value(Value::Long(ctx.node_id)),
                "MachoBindObject" => return svc.bind(call, ctx).await,
                _ => {}
            }
        }
        svc.call(call, ctx).await
    }
}
