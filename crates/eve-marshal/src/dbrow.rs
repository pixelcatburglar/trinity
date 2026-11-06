//! Packed database rows (`TYPE_DBROW`, 0x2A).
//!
//! Trinity's own bulk caches carry these: `dgmtypeattribs` and `dgmtypeeffects`
//! are bare lists of them, `ramtypematerials` and `ramtyperequirements` are
//! `(DBRowDescriptor, [DBRow, ...])`. A row is a `blue.DBRowDescriptor` (column
//! name + ADO DBTYPE code), a zero-run-length-packed blob of the fixed-width
//! columns, then one marshalled object per variable-width column.
//!
//! Fixed columns are laid out by width — 8-byte first, then 4, 2, 1 — and
//! booleans are packed one bit each after them.

use crate::value::Value;

/// ADO DBTYPE codes used by `DBRowDescriptor`.
pub mod dbtype {
    pub const I2: i64 = 2;
    pub const I4: i64 = 3;
    pub const R4: i64 = 4;
    pub const R8: i64 = 5;
    pub const CY: i64 = 6;
    pub const DATE: i64 = 7;
    pub const BOOL: i64 = 11;
    pub const I1: i64 = 16;
    pub const UI1: i64 = 17;
    pub const UI2: i64 = 18;
    pub const UI4: i64 = 19;
    pub const I8: i64 = 20;
    pub const UI8: i64 = 21;
    pub const FILETIME: i64 = 64;
    pub const GUID: i64 = 72;
    pub const BYTES: i64 = 128;
    pub const STR: i64 = 129;
    pub const WSTR: i64 = 130;
    pub const DBTIMESTAMP: i64 = 135;
}

/// Where a column lives in a packed row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Storage {
    /// Byte offset into the unpacked blob.
    Fixed(usize),
    /// Absolute bit offset of a packed bool.
    Bit(usize),
    /// Index into the objects that follow the blob.
    Object(usize),
}

/// Bytes per fixed field; `Some(0)` is a packed bit, `None` an object.
fn width_of(t: i64) -> Option<usize> {
    use dbtype::*;
    match t {
        BOOL => Some(0),
        I1 | UI1 => Some(1),
        I2 | UI2 => Some(2),
        I4 | UI4 | R4 => Some(4),
        I8 | UI8 | R8 | CY | DATE | FILETIME | DBTIMESTAMP => Some(8),
        GUID => Some(16),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DbRowDescriptor {
    /// `(name, dbtype)` in column order.
    pub columns: Vec<(Vec<u8>, i64)>,
    plan: Vec<Storage>,
    fixed_bytes: usize,
    bits: usize,
    objects: usize,
}

impl DbRowDescriptor {
    pub fn new(columns: Vec<(Vec<u8>, i64)>) -> Self {
        let mut offset = 0usize;
        let mut plan = vec![Storage::Object(0); columns.len()];
        for width in [16usize, 8, 4, 2, 1] {
            for (i, (_, t)) in columns.iter().enumerate() {
                if width_of(*t) == Some(width) {
                    plan[i] = Storage::Fixed(offset);
                    offset += width;
                }
            }
        }
        let fixed_bytes = offset;
        let (mut bits, mut objects) = (0usize, 0usize);
        for (i, (_, t)) in columns.iter().enumerate() {
            match width_of(*t) {
                Some(0) => {
                    plan[i] = Storage::Bit(fixed_bytes * 8 + bits);
                    bits += 1;
                }
                None => {
                    plan[i] = Storage::Object(objects);
                    objects += 1;
                }
                Some(_) => {}
            }
        }
        DbRowDescriptor { columns, plan, fixed_bytes, bits, objects }
    }

    /// Columns named with their types, for building one by hand.
    pub fn of(columns: &[(&str, i64)]) -> Self {
        Self::new(columns.iter().map(|(n, t)| (n.as_bytes().to_vec(), *t)).collect())
    }

    pub fn storage(&self, i: usize) -> Storage {
        self.plan[i]
    }

    pub fn blob_len(&self) -> usize {
        self.fixed_bytes + self.bits.div_ceil(8)
    }

    pub fn object_count(&self) -> usize {
        self.objects
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|(n, _)| n == name.as_bytes())
    }

    /// The wire form: `blue.DBRowDescriptor((('name', type), ...),)` as a reduce.
    pub fn to_value(&self) -> Value {
        let cols = self
            .columns
            .iter()
            .map(|(n, t)| Value::Tuple(vec![Value::Bytes(n.clone()), Value::Int(*t)]))
            .collect();
        Value::reduce("blue.DBRowDescriptor", vec![Value::Tuple(cols)])
    }

    /// Parse the wire form back; `None` if it is not a descriptor.
    pub fn from_value(v: &Value) -> Option<Self> {
        let Value::Reduce { args, .. } = v else { return None };
        let cols = args.seq()?.first()?.seq()?;
        let mut out = Vec::with_capacity(cols.len());
        for c in cols {
            let pair = c.seq()?;
            out.push((pair.first()?.as_bytes()?.to_vec(), pair.get(1)?.as_i64()?));
        }
        Some(Self::new(out))
    }

    /// Pack one row's values (in column order) into blob + objects.
    pub(crate) fn pack(&self, values: &[Value]) -> (Vec<u8>, Vec<Value>) {
        let mut blob = vec![0u8; self.blob_len()];
        let mut objects = vec![Value::None; self.objects];
        for (i, (_, t)) in self.columns.iter().enumerate() {
            let v = values.get(i).unwrap_or(&Value::None);
            match self.plan[i] {
                Storage::Object(ix) => objects[ix] = v.clone(),
                Storage::Bit(bit) => {
                    if v.as_i64().unwrap_or(0) != 0 {
                        blob[bit >> 3] |= 1 << (bit & 7);
                    }
                }
                Storage::Fixed(off) => {
                    let bytes = encode_scalar(*t, v);
                    blob[off..off + bytes.len()].copy_from_slice(&bytes);
                }
            }
        }
        (rle_pack(&blob), objects)
    }

    pub(crate) fn unpack(&self, packed: &[u8], objects: Vec<Value>) -> Vec<Value> {
        let blob = rle_unpack(packed, self.blob_len());
        self.columns
            .iter()
            .enumerate()
            .map(|(i, (_, t))| match self.plan[i] {
                Storage::Object(ix) => objects.get(ix).cloned().unwrap_or(Value::None),
                Storage::Bit(bit) => Value::Bool(blob[bit >> 3] & (1 << (bit & 7)) != 0),
                Storage::Fixed(off) => decode_scalar(*t, &blob[off..]),
            })
            .collect()
    }
}

/// One packed row: shares its descriptor with the rest of its rowset.
#[derive(Clone, Debug, PartialEq)]
pub struct DbRow {
    pub descriptor: std::sync::Arc<DbRowDescriptor>,
    pub values: Vec<Value>,
}

impl DbRow {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.descriptor.index_of(name).map(|i| &self.values[i])
    }
}

fn encode_scalar(t: i64, v: &Value) -> Vec<u8> {
    use dbtype::*;
    let int = || v.as_i64().unwrap_or_else(|| v.as_f64().unwrap_or(0.0) as i64);
    match t {
        I1 | UI1 => vec![int() as u8],
        I2 | UI2 => (int() as i16).to_le_bytes().to_vec(),
        I4 | UI4 => (int() as i32).to_le_bytes().to_vec(),
        R4 => (v.as_f64().unwrap_or(0.0) as f32).to_le_bytes().to_vec(),
        R8 => v.as_f64().unwrap_or(0.0).to_le_bytes().to_vec(),
        CY => ((v.as_f64().unwrap_or(0.0) * 10000.0).round() as i64).to_le_bytes().to_vec(),
        GUID => vec![0u8; 16],
        _ => int().to_le_bytes().to_vec(),
    }
}

fn decode_scalar(t: i64, b: &[u8]) -> Value {
    use dbtype::*;
    let le = |n: usize| {
        let mut a = [0u8; 8];
        a[..n].copy_from_slice(&b[..n]);
        a
    };
    match t {
        I1 => Value::Int(b[0] as i8 as i64),
        UI1 => Value::Int(b[0] as i64),
        I2 => Value::Int(i16::from_le_bytes([b[0], b[1]]) as i64),
        UI2 => Value::Int(u16::from_le_bytes([b[0], b[1]]) as i64),
        I4 => Value::Int(i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64),
        UI4 => Value::Int(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i64),
        R4 => Value::Double(f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64),
        R8 => Value::Double(f64::from_le_bytes(le(8))),
        CY => Value::Double(i64::from_le_bytes(le(8)) as f64 / 10000.0),
        GUID => Value::Bytes(b[..16].to_vec()),
        _ => Value::Long(i64::from_le_bytes(le(8))),
    }
}

/// Zero run-length unpacking: each control byte holds two nibbles, low first.
/// A nibble `n >= 8` emits `n - 7` zero bytes; `n < 8` copies `8 - n` literals.
pub fn rle_unpack(packed: &[u8], expect: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(expect);
    let mut i = 0usize;
    while i < packed.len() {
        let control = packed[i];
        i += 1;
        for nibble in [control & 0x0F, control >> 4] {
            if nibble >= 8 {
                out.extend(std::iter::repeat_n(0u8, (nibble - 7) as usize));
            } else {
                let end = (i + (8 - nibble) as usize).min(packed.len());
                out.extend_from_slice(&packed[i..end]);
                i = end;
            }
        }
    }
    out.resize(expect, 0);
    out
}

/// The inverse of [`rle_unpack`].
pub fn rle_pack(blob: &[u8]) -> Vec<u8> {
    let mut runs: Vec<(bool, usize)> = Vec::new();
    let mut i = 0;
    while i < blob.len() {
        let zero = blob[i] == 0;
        let mut n = 0;
        while i + n < blob.len() && (blob[i + n] == 0) == zero && n < 8 {
            n += 1;
        }
        runs.push((zero, n));
        i += n;
    }
    let mut out = Vec::with_capacity(blob.len() + 1);
    let mut src = 0;
    for pair in runs.chunks(2) {
        let mut control = 0u8;
        let mut lits = Vec::new();
        for (half, run) in pair.iter().enumerate() {
            let code = if run.0 {
                (run.1 + 7) as u8
            } else {
                lits.extend_from_slice(&blob[src..src + run.1]);
                (8 - run.1) as u8
            };
            src += run.1;
            control |= code << (4 * half);
        }
        if pair.len() == 1 {
            // An unused high nibble: a one-zero run past the end, which the
            // reader trims back to the blob length.
            control |= 8 << 4;
        }
        out.push(control);
        out.extend_from_slice(&lits);
    }
    out
}
