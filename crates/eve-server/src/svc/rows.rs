//! The row containers the client unpickles (`util.Row`, `util.Rowset`,
//! `util.IndexRowset`). Values come from store rows (`store::Row::cell` and
//! `line`) and static rows (`sde::Row::cell` and `line`).

use eve_marshal::Value;

/// A `util.KeyVal`: a bag of named attributes.
pub fn key_val(fields: Vec<(&str, Value)>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("util.KeyVal")),
        state: Box::new(Value::Dict(
            fields.into_iter().map(|(k, v)| (Value::bytes(k), v)).collect(),
        )),
    }
}

pub fn row(header: &[&str], line: Vec<Value>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("util.Row")),
        state: Box::new(Value::Dict(vec![
            (Value::bytes("header"), header_value(header)),
            (Value::bytes("line"), Value::List(line)),
        ])),
    }
}

pub fn rowset(header: &[&str], lines: Vec<Value>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("util.Rowset")),
        state: Box::new(Value::Dict(vec![
            (Value::bytes("header"), header_value(header)),
            (Value::bytes("lines"), Value::List(lines)),
        ])),
    }
}

/// A rowset that also carries its header as `columns`, which the market UI reads
/// (marketsvc.py:373 `bids.columns.index('jumps')`).
pub fn rowset_with_columns(header: &[&str], lines: Vec<Value>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("util.Rowset")),
        state: Box::new(Value::Dict(vec![
            (Value::bytes("header"), header_value(header)),
            (Value::bytes("lines"), Value::List(lines)),
            (Value::bytes("columns"), header_value(header)),
        ])),
    }
}

pub fn index_rowset(header: &[&str], id_name: &str, items: Vec<(Value, Value)>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("util.IndexRowset")),
        state: Box::new(Value::Dict(vec![
            (Value::bytes("header"), header_value(header)),
            (Value::bytes("items"), Value::Dict(items)),
            (Value::bytes("idName"), Value::bytes(id_name)),
        ])),
    }
}

/// The `{column: (old, new)}` dict the client's On*Changed handlers take
/// (bco_base.GetAddRemoveFromChange): a row added (`old` None) or removed
/// (`new` None) carries every column, an update only the ones that differ.
pub fn change(header: &[&str], old: Option<&[Value]>, new: Option<&[Value]>) -> Value {
    let at = |l: Option<&[Value]>, i: usize| l.and_then(|l| l.get(i)).cloned().unwrap_or(Value::None);
    let whole = old.is_none() || new.is_none();
    Value::Dict(
        header
            .iter()
            .enumerate()
            .map(|(i, h)| (Value::bytes(*h), (at(old, i), at(new, i))))
            .filter(|(_, (o, n))| whole || o != n)
            .map(|(k, (o, n))| (k, Value::Tuple(vec![o, n])))
            .collect(),
    )
}

pub fn header_value(header: &[&str]) -> Value {
    Value::List(header.iter().map(|h| Value::bytes(*h)).collect())
}
