//! The live tables' definitions, from `schema.json`: columns with their
//! types, NOT NULL and defaults; the primary key and other indexes; CHECK
//! constraints; foreign keys; and the sequences ids are drawn from. Checks
//! and partial-index conditions are named in the JSON and written here.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Deserialize;

use super::table::RowView;
use super::Value;
use crate::sde::ColType as WireType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    SmallInt,
    Integer,
    BigInt,
    Real,
    Double,
    Bool,
    Text,
}

impl ColType {
    fn parse(s: &str) -> Option<ColType> {
        Some(match s {
            "smallint" => ColType::SmallInt,
            "integer" => ColType::Integer,
            "bigint" => ColType::BigInt,
            "real" => ColType::Real,
            "double" => ColType::Double,
            "boolean" => ColType::Bool,
            "text" => ColType::Text,
            _ => return None,
        })
    }

    /// The column's type as the client is sent it (a `real` as a double,
    /// as `svc::rows::cell` sent FLOAT4).
    pub fn wire(self) -> WireType {
        match self {
            ColType::SmallInt => WireType::SmallInt,
            ColType::Integer => WireType::Integer,
            ColType::BigInt => WireType::BigInt,
            ColType::Real | ColType::Double => WireType::Double,
            ColType::Bool => WireType::Bool,
            ColType::Text => WireType::Text,
        }
    }

    pub fn is_integer(self) -> bool {
        matches!(self, ColType::SmallInt | ColType::Integer | ColType::BigInt)
    }

    /// A value as this column holds it: integers widen into a float column,
    /// 0/1 into a boolean one, and a float column's values are rounded to
    /// `real` precision where the column is one.
    pub fn coerce(self, v: Value) -> Value {
        match (self, v) {
            (ColType::Double, Value::Int(i)) => Value::Float(i as f64),
            (ColType::Real, Value::Int(i)) => Value::Float(i as f32 as f64),
            (ColType::Real, Value::Float(f)) => Value::Float(f as f32 as f64),
            (ColType::Bool, Value::Int(i)) => Value::Bool(i != 0),
            (_, v) => v,
        }
    }

    /// Whether `v` (already coerced) fits this column.
    pub fn admits(self, v: &Value) -> bool {
        match (self, v) {
            (_, Value::Null) => true,
            (ColType::SmallInt, Value::Int(i)) => i16::try_from(*i).is_ok(),
            (ColType::Integer, Value::Int(i)) => i32::try_from(*i).is_ok(),
            (ColType::BigInt, Value::Int(_)) => true,
            (ColType::Real | ColType::Double, Value::Float(_)) => true,
            (ColType::Bool, Value::Bool(_)) => true,
            (ColType::Text, Value::Text(_)) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Default {
    Value(Value),
    Sequence(Arc<str>),
}

#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: Arc<str>,
    pub ty: ColType,
    pub not_null: bool,
    pub default: Option<Default>,
}

pub type Predicate = fn(&RowView) -> bool;

#[derive(Clone)]
pub struct IndexDef {
    pub name: Arc<str>,
    pub cols: Vec<usize>,
    pub unique: bool,
    /// For a partial index: the rows it holds.
    pub filter: Option<Predicate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDelete {
    /// The referencing rows stop the delete (NO ACTION).
    Refuse,
    Cascade,
    SetNull,
}

#[derive(Debug, Clone)]
pub struct ForeignKey {
    pub cols: Vec<usize>,
    pub references: Arc<str>,
    pub referenced_cols: Vec<usize>,
    pub on_delete: OnDelete,
}

#[derive(Clone)]
pub struct TableDef {
    pub name: Arc<str>,
    pub columns: Vec<ColumnDef>,
    by_name: HashMap<Arc<str>, usize>,
    /// Position of the primary key among `indexes`, if the table has one.
    pub primary: Option<usize>,
    pub indexes: Vec<IndexDef>,
    pub checks: Vec<(Arc<str>, Predicate)>,
    pub foreign_keys: Vec<ForeignKey>,
}

impl TableDef {
    pub fn col(&self, name: &str) -> usize {
        *self.by_name.get(name).unwrap_or_else(|| panic!("table {} has no column {name:?}", self.name))
    }

    /// The columns that identify a row on disk: the primary key, or (for a
    /// table without one) all of them.
    pub fn identity(&self) -> Vec<usize> {
        match self.primary {
            Some(p) => self.indexes[p].cols.clone(),
            None => (0..self.columns.len()).collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SequenceDef {
    pub name: Arc<str>,
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
}

pub struct Schema {
    pub tables: Vec<Arc<TableDef>>,
    pub sequences: Vec<SequenceDef>,
}

impl Schema {
    pub fn table(&self, name: &str) -> Option<&Arc<TableDef>> {
        self.tables.iter().find(|t| &*t.name == name)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsonSchema {
    sequences: Vec<JsonSequence>,
    tables: Vec<JsonTable>,
}

#[derive(Deserialize)]
struct JsonSequence {
    name: String,
    start: i64,
    increment: i64,
    min: i64,
    max: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsonTable {
    name: String,
    columns: Vec<JsonColumn>,
    #[serde(default)]
    primary_key: Vec<String>,
    #[serde(default)]
    indexes: Vec<JsonIndex>,
    #[serde(default)]
    checks: Vec<String>,
    #[serde(default)]
    foreign_keys: Vec<JsonForeignKey>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsonColumn {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    not_null: bool,
    default: Option<JsonDefault>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsonDefault {
    value: Option<serde_json::Value>,
    seq: Option<String>,
}

#[derive(Deserialize)]
struct JsonIndex {
    name: String,
    columns: Vec<String>,
    #[serde(default)]
    unique: bool,
    #[serde(rename = "where")]
    filter: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JsonForeignKey {
    columns: Vec<String>,
    references: String,
    referenced_columns: Vec<String>,
    on_delete: Option<String>,
}

/// The CHECK constraints by name. A check passes on NULL, as SQL's does.
fn check(name: &str) -> Option<Predicate> {
    fn int(r: &RowView, c: &str) -> Option<i64> {
        r.value(c).int()
    }
    Some(match name {
        "entity_attributes_one_value" => |r| r.value("valueInt").is_null() != r.value("valueFloat").is_null(),
        "loyalty_points_not_negative" => |r| int(r, "loyaltyPoints").is_none_or(|v| v >= 0),
        "recruitment_skill_points_in_range" => |r| int(r, "skillPoints").is_none_or(|v| (0..=200_000_000).contains(&v)),
        "corporation_wallet_division" => |r| int(r, "accountKey").is_none_or(|v| (1001..=1006).contains(&v)),
        "corporation_wallet_not_negative" => |r| r.value("balance").float().is_none_or(|v| v >= 0.0),
        "jump_bridge_not_to_itself" => |r| match (int(r, "sourceID"), int(r, "destinationID")) {
            (Some(a), Some(b)) => a != b,
            _ => true,
        },
        "travel_receipt_state" => |r| int(r, "state").is_none_or(|v| v == 0 || v == 1),
        "portal_receipt_state" => |r| int(r, "state").is_none_or(|v| (0..=2).contains(&v)),
        "booster_slot_positive" => |r| int(r, "slot").is_none_or(|v| v > 0),
        "booster_expires_after_start" => |r| match (int(r, "expires"), int(r, "started")) {
            (Some(e), Some(s)) => e > s,
            _ => true,
        },
        _ => return None,
    })
}

/// The partial indexes' conditions by name.
fn condition(name: &str) -> Option<Predicate> {
    Some(match name {
        "starter_seed" => |r| r.value("starterSeed").bool() == Some(true),
        "state_pending" => |r| r.value("state").int() == Some(0),
        "state_active" => |r| matches!(r.value("state").int(), Some(0 | 1)),
        "rental_slot_set" => |r| !r.value("rentalSlot").is_null(),
        _ => return None,
    })
}

impl Schema {
    pub fn parse(json: &str) -> Result<Schema, String> {
        let s: JsonSchema = serde_json::from_str(json).map_err(|e| format!("schema.json: {e}"))?;
        let sequences = s
            .sequences
            .into_iter()
            .map(|q| SequenceDef { name: q.name.into(), start: q.start, increment: q.increment, min: q.min, max: q.max })
            .collect::<Vec<_>>();
        let mut defs: Vec<TableDef> = Vec::new();
        for t in &s.tables {
            let mut columns = Vec::new();
            for c in &t.columns {
                let ty = ColType::parse(&c.ty).ok_or_else(|| format!("{}.{}: unknown type {:?}", t.name, c.name, c.ty))?;
                let default = match &c.default {
                    None => None,
                    Some(JsonDefault { seq: Some(seq), .. }) => {
                        if !sequences.iter().any(|q| &*q.name == seq) {
                            return Err(format!("{}.{}: no sequence {seq}", t.name, c.name));
                        }
                        Some(Default::Sequence(seq.as_str().into()))
                    }
                    Some(JsonDefault { value: Some(v), .. }) => Some(Default::Value(ty.coerce(json_value(v)))),
                    Some(_) => return Err(format!("{}.{}: an empty default", t.name, c.name)),
                };
                columns.push(ColumnDef { name: c.name.as_str().into(), ty, not_null: c.not_null, default });
            }
            let by_name = columns.iter().enumerate().map(|(i, c)| (c.name.clone(), i)).collect::<HashMap<_, _>>();
            let cols = |names: &[String]| -> Result<Vec<usize>, String> {
                names.iter().map(|n| by_name.get(n.as_str()).copied().ok_or_else(|| format!("{}: no column {n}", t.name))).collect()
            };
            let mut indexes = Vec::new();
            let mut primary = None;
            if !t.primary_key.is_empty() {
                primary = Some(0);
                indexes.push(IndexDef { name: format!("{}_pkey", t.name).into(), cols: cols(&t.primary_key)?, unique: true, filter: None });
            }
            for ix in &t.indexes {
                let filter = match &ix.filter {
                    None => None,
                    Some(f) => Some(condition(f).ok_or_else(|| format!("{}: unknown index condition {f}", t.name))?),
                };
                indexes.push(IndexDef { name: ix.name.as_str().into(), cols: cols(&ix.columns)?, unique: ix.unique, filter });
            }
            let checks = t
                .checks
                .iter()
                .map(|c| check(c).map(|f| (Arc::<str>::from(c.as_str()), f)).ok_or_else(|| format!("{}: unknown check {c}", t.name)))
                .collect::<Result<Vec<_>, _>>()?;
            let mut foreign_keys = Vec::new();
            for fk in &t.foreign_keys {
                let on_delete = match fk.on_delete.as_deref() {
                    None => OnDelete::Refuse,
                    Some("cascade") => OnDelete::Cascade,
                    Some("setNull") => OnDelete::SetNull,
                    Some(o) => return Err(format!("{}: unknown onDelete {o}", t.name)),
                };
                foreign_keys.push(ForeignKey { cols: cols(&fk.columns)?, references: fk.references.as_str().into(), referenced_cols: Vec::new(), on_delete });
            }
            defs.push(TableDef { name: t.name.as_str().into(), columns, by_name, primary, indexes, checks, foreign_keys });
        }
        // Resolve each foreign key's referenced columns, which must be the
        // referenced table's primary key or a unique index of it.
        for (i, t) in s.tables.iter().enumerate() {
            for (k, fk) in t.foreign_keys.iter().enumerate() {
                let parent = defs.iter().find(|d| *d.name == *fk.references).ok_or_else(|| format!("{}: references unknown {}", t.name, fk.references))?;
                let referenced = fk.referenced_columns.iter().map(|c| parent.col(c)).collect::<Vec<_>>();
                if !parent.indexes.iter().any(|ix| ix.unique && ix.filter.is_none() && ix.cols == referenced) {
                    return Err(format!("{}: references {} by columns no unique index holds", t.name, fk.references));
                }
                defs[i].foreign_keys[k].referenced_cols = referenced;
            }
        }
        Ok(Schema { tables: defs.into_iter().map(Arc::new).collect(), sequences })
    }
}

fn json_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => n.as_i64().map(Value::Int).unwrap_or_else(|| Value::Float(n.as_f64().unwrap_or(0.0))),
        serde_json::Value::String(s) => Value::Text(s.as_str().into()),
        other => Value::Text(other.to_string().into()),
    }
}
