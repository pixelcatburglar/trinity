//! Filling an empty store from PostgreSQL dumps: the `db/*.sql.gz` files a
//! fresh world starts from, or a `pg_dump` of an existing database being
//! moved into the store. Each table's rows come from its `COPY` block (by
//! column name; a column the dump lacks takes its default), and sequences
//! from the dumps' `setval` calls.

use std::io::Read;
use std::path::Path;

use flate2::read::GzDecoder;

use super::schema::{ColType, Default};
use super::tx::Db;
use super::Value;
use crate::sde::copy;

/// The text of a dump, gunzipped where it is gzipped.
pub fn read_dump(path: &Path) -> Result<String, String> {
    let raw = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if raw.starts_with(&[0x1f, 0x8b]) {
        let mut text = String::new();
        GzDecoder::new(&raw[..]).read_to_string(&mut text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(copy::unix_lines(text))
    } else {
        String::from_utf8(raw).map(copy::unix_lines).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// Load every table and sequence `dumps` hold into `db` (which should be
/// empty): how many rows.
pub fn load(db: &mut Db, dumps: &[String]) -> Result<usize, String> {
    // Sequences first, so a column a dump lacks but numbers from a sequence
    // continues it (below).
    for dump in dumps {
        for (name, value, called) in setvals(dump) {
            let Some(def) = db.schema.sequences.iter().find(|s| *s.name == *name) else { continue };
            let last = if called { value } else { value - def.increment };
            if let Some(slot) = db.sequences.get_mut(name.as_str()) {
                *slot = Some(last);
            }
        }
    }
    let mut rows = 0;
    for t in 0..db.tables.len() {
        let def = db.tables[t].def.clone();
        let name: &'static str = Box::leak(def.name.to_string().into_boxed_str());
        for dump in dumps {
            let Ok(table) = copy::Table::untyped(name, dump) else { continue };
            let from: Vec<Option<usize>> = def.columns.iter().map(|c| table.columns().iter().position(|n| *n == *c.name)).collect();
            for r in table.rows() {
                let mut cells = Vec::with_capacity(def.columns.len());
                for (c, col) in def.columns.iter().enumerate() {
                    let v = match from[c] {
                        Some(i) => field(r.raw(i)?, col.ty).map_err(|e| format!("{name}.{}: {e}", col.name))?,
                        None => match &col.default {
                            Some(Default::Value(v)) => v.clone(),
                            // A column added since the dump numbers its rows
                            // in order, as PostgreSQL's `ADD COLUMN ... serial`
                            // numbered the rows already there.
                            Some(Default::Sequence(seq)) => Value::Int(next(db, seq)?),
                            None => Value::Null,
                        },
                    };
                    cells.push(col.ty.coerce(v));
                }
                db.table_mut(t).put(None, cells.into());
                rows += 1;
            }
        }
    }
    Ok(rows)
}

/// A sequence's next value (`nextval`), for rows being loaded.
fn next(db: &mut Db, seq: &str) -> Result<i64, String> {
    let def = db.schema.sequences.iter().find(|s| *s.name == *seq).ok_or_else(|| format!("no sequence {seq}"))?.clone();
    let slot = db.sequences.get_mut(seq).ok_or_else(|| format!("no sequence {seq}"))?;
    let value = slot.map_or(def.start, |v| v + def.increment);
    *slot = Some(value);
    Ok(value)
}

/// A COPY field as a value of a column of type `ty`.
fn field(raw: Option<&str>, ty: ColType) -> Result<Value, String> {
    let Some(raw) = raw else { return Ok(Value::Null) };
    Ok(match ty {
        ColType::SmallInt | ColType::Integer | ColType::BigInt => match raw.parse::<i64>() {
            Ok(v) => Value::Int(v),
            // A timestamp column (ramAssemblyLines.nextFreeTime) is kept as
            // a FILETIME, as the rest of the server keeps times.
            Err(_) => Value::Int(filetime(raw).ok_or_else(|| format!("{raw:?} is not an integer or a time"))?),
        },
        ColType::Real | ColType::Double => Value::Float(raw.parse().map_err(|_| format!("{raw:?} is not a number"))?),
        ColType::Bool => match raw {
            "t" => Value::Bool(true),
            "f" => Value::Bool(false),
            _ => return Err(format!("{raw:?} is not a boolean")),
        },
        ColType::Text => Value::Text(copy::unescape(raw).into()),
    })
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff]` (UTC) as a Windows FILETIME.
fn filetime(s: &str) -> Option<i64> {
    let (date, time) = s.split_once(' ')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    // Without a zone (`timestamp`), or UTC (`+00`, `Z`).
    let time = time.split(['+', 'Z']).next()?;
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut t = hms.split(':').map(|p| p.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    let frac: String = frac.chars().take_while(char::is_ascii_digit).collect();
    let ticks_frac = if frac.is_empty() { 0 } else { format!("{frac:0<7}")[..7].parse::<i64>().ok()? };
    // Days since 1970-01-01 (civil-from-days, proleptic Gregorian).
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss;
    Some((secs + 11_644_473_600) * 10_000_000 + ticks_frac)
}

/// Every `setval('seq', value, called)` in a dump.
fn setvals(dump: &str) -> Vec<(String, i64, bool)> {
    let mut out = Vec::new();
    for line in dump.lines().filter(|l| l.starts_with("SELECT pg_catalog.setval(")) {
        let inner = &line["SELECT pg_catalog.setval(".len()..line.rfind(')').unwrap_or(line.len())];
        let mut parts = inner.splitn(3, ',').map(str::trim);
        let (Some(name), Some(value)) = (parts.next(), parts.next()) else { continue };
        let name = name.trim_matches('\'').trim_start_matches("public.").trim_matches('"').to_string();
        let called = parts.next().is_none_or(|c| c == "true");
        if let Ok(value) = value.parse() {
            out.push((name, value, called));
        }
    }
    out
}
