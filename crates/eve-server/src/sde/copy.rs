//! Reading the `COPY ... FROM stdin` block of a gzipped pg_dump, the format
//! `db/*.sql.gz` is kept in: one row per line, fields split by tabs, `\N` for
//! NULL, and backslash escapes for tabs, newlines and backslashes in text.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read;

use flate2::read::GzDecoder;

use super::table::ColType;

/// One table's rows, still as text; columns are found by name, so a dump
/// whose columns come in another order still reads the same.
pub struct Table {
    name: &'static str,
    columns: Vec<String>,
    /// Each column's type from the dump's CREATE TABLE, in `columns` order.
    types: Vec<ColType>,
    /// The lines between the COPY header and its `\.` terminator.
    body: String,
}

impl Table {
    /// The rows of `name` in a gzipped dump.
    pub fn from_gz(name: &'static str, gz: &[u8]) -> Result<Table, String> {
        let mut text = String::new();
        GzDecoder::new(gz).read_to_string(&mut text).map_err(|e| format!("{name}: cannot decompress: {e}"))?;
        Table::from_dump(name, &unix_lines(text))
    }

    /// The rows of `name` in an uncompressed dump.
    pub fn from_dump(name: &'static str, dump: &str) -> Result<Table, String> {
        let mut t = Table::untyped(name, dump)?;
        t.types = column_types(name, dump, &t.columns)?;
        Ok(t)
    }

    /// The rows of `name` in an uncompressed dump, its columns' types left
    /// unread (for a reader that knows them from elsewhere); None when the
    /// dump has no COPY block for it.
    pub fn untyped(name: &'static str, dump: &str) -> Result<Table, String> {
        let quoted = format!("COPY public.\"{name}\" (");
        let bare = format!("COPY public.{name} (");
        let start = dump
            .find(&quoted)
            .map(|i| i + quoted.len())
            .or_else(|| dump.find(&bare).map(|i| i + bare.len()))
            .ok_or_else(|| format!("{name}: the dump has no COPY block for it"))?;
        let header_end = dump[start..].find(") FROM stdin;\n").ok_or_else(|| format!("{name}: unterminated COPY header"))?;
        let columns: Vec<String> = dump[start..start + header_end].split(", ").map(|c| c.trim_matches('"').to_string()).collect();
        let body_start = start + header_end + ") FROM stdin;\n".len();
        let body_len = if dump[body_start..].starts_with("\\.\n") {
            0
        } else {
            dump[body_start..].find("\n\\.\n").map(|i| i + 1).ok_or_else(|| format!("{name}: unterminated COPY data"))?
        };
        Ok(Table { name, columns, types: Vec::new(), body: dump[body_start..body_start + body_len].to_string() })
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    pub fn types(&self) -> &[ColType] {
        &self.types
    }

    /// The position of a column, for [`Row`]'s getters.
    pub fn col(&self, column: &str) -> Result<usize, String> {
        self.columns.iter().position(|c| c == column).ok_or_else(|| format!("{}: no column {column:?}", self.name))
    }

    pub fn rows(&self) -> impl Iterator<Item = Row<'_>> {
        self.body.lines().enumerate().map(move |(i, line)| Row { table: self.name, line: i + 1, fields: line.split('\t').collect() })
    }
}

/// One line of a COPY block.
pub struct Row<'a> {
    table: &'static str,
    line: usize,
    fields: Vec<&'a str>,
}

impl<'a> Row<'a> {
    /// A field as the dump has it (still escaped), None for NULL.
    pub fn raw(&self, col: usize) -> Result<Option<&'a str>, String> {
        match self.fields.get(col) {
            Some(&"\\N") => Ok(None),
            Some(f) => Ok(Some(f)),
            None => Err(self.error(col, "is missing")),
        }
    }

    fn error(&self, col: usize, what: &str) -> String {
        format!("{} row {} field {}: {what}", self.table, self.line, col + 1)
    }

    pub fn opt_int(&self, col: usize) -> Result<Option<i64>, String> {
        self.raw(col)?.map(|f| f.parse().map_err(|_| self.error(col, &format!("{f:?} is not an integer")))).transpose()
    }

    pub fn int(&self, col: usize) -> Result<i64, String> {
        self.opt_int(col)?.ok_or_else(|| self.error(col, "is NULL"))
    }

    /// A float column; Postgres writes `Infinity`, `-Infinity` and `NaN`,
    /// which Rust's parser takes as they are.
    pub fn opt_float(&self, col: usize) -> Result<Option<f64>, String> {
        self.raw(col)?.map(|f| f.parse().map_err(|_| self.error(col, &format!("{f:?} is not a number")))).transpose()
    }

    /// A boolean column: `t`/`f` as Postgres writes a boolean.
    pub fn opt_bool(&self, col: usize) -> Result<Option<bool>, String> {
        match self.raw(col)? {
            Some("t") => Ok(Some(true)),
            Some("f") => Ok(Some(false)),
            Some(f) => Err(self.error(col, &format!("{f:?} is not a boolean"))),
            None => Ok(None),
        }
    }

    pub fn bool(&self, col: usize) -> Result<bool, String> {
        self.opt_bool(col)?.ok_or_else(|| self.error(col, "is NULL"))
    }

    pub fn opt_text(&self, col: usize) -> Result<Option<Cow<'a, str>>, String> {
        Ok(self.raw(col)?.map(unescape))
    }

    pub fn text(&self, col: usize) -> Result<Cow<'a, str>, String> {
        self.opt_text(col)?.ok_or_else(|| self.error(col, "is NULL"))
    }
}

/// A dump's text with Windows line ends made Unix ones (some dumps were
/// saved on Windows). COPY data escapes a carriage return inside a value as
/// `\r`, so a raw one can only end a line.
pub fn unix_lines(text: String) -> String {
    if text.contains('\r') {
        text.replace("\r\n", "\n")
    } else {
        text
    }
}

/// Each of `columns`' type, from the `CREATE TABLE` of `name` in the dump.
fn column_types(name: &str, dump: &str, columns: &[String]) -> Result<Vec<ColType>, String> {
    let quoted = format!("CREATE TABLE public.\"{name}\" (\n");
    let bare = format!("CREATE TABLE public.{name} (\n");
    let start = dump
        .find(&quoted)
        .map(|i| i + quoted.len())
        .or_else(|| dump.find(&bare).map(|i| i + bare.len()))
        .ok_or_else(|| format!("{name}: the dump has no CREATE TABLE for it"))?;
    let end = dump[start..].find("\n);").ok_or_else(|| format!("{name}: unterminated CREATE TABLE"))?;
    let mut declared = HashMap::new();
    for line in dump[start..start + end].lines() {
        let line = line.trim().trim_end_matches(',');
        if line.starts_with("CONSTRAINT ") {
            continue;
        }
        let (column, rest) = match line.strip_prefix('"') {
            Some(q) => q.split_once("\" ").ok_or_else(|| format!("{name}: cannot read column {line:?}"))?,
            None => line.split_once(' ').ok_or_else(|| format!("{name}: cannot read column {line:?}"))?,
        };
        let sql = rest.split(" NOT NULL").next().unwrap_or(rest).split(" DEFAULT ").next().unwrap_or(rest).trim();
        let ty = ColType::parse(sql).ok_or_else(|| format!("{name}.{column}: unsupported column type {sql:?}"))?;
        declared.insert(column.to_string(), ty);
    }
    columns
        .iter()
        .map(|c| declared.get(c).copied().ok_or_else(|| format!("{name}: COPY names {c:?}, which CREATE TABLE lacks")))
        .collect()
}

/// Undo COPY's text escapes: `\b \f \n \r \t \v \\`, octal `\NNN` and hex
/// `\xHH`; any other escaped character stands for itself.
pub fn unescape(field: &str) -> Cow<'_, str> {
    if !field.contains('\\') {
        return Cow::Borrowed(field);
    }
    let mut bytes = Vec::with_capacity(field.len());
    let raw = field.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] != b'\\' || i + 1 == raw.len() {
            bytes.push(raw[i]);
            i += 1;
            continue;
        }
        let c = raw[i + 1];
        i += 2;
        match c {
            b'b' => bytes.push(8),
            b'f' => bytes.push(12),
            b'n' => bytes.push(b'\n'),
            b'r' => bytes.push(b'\r'),
            b't' => bytes.push(b'\t'),
            b'v' => bytes.push(11),
            b'0'..=b'7' => {
                let mut v = (c - b'0') as u32;
                for _ in 0..2 {
                    match raw.get(i) {
                        Some(d @ b'0'..=b'7') => {
                            v = v * 8 + (d - b'0') as u32;
                            i += 1;
                        }
                        _ => break,
                    }
                }
                bytes.push(v as u8);
            }
            b'x' if raw.get(i).is_some_and(u8::is_ascii_hexdigit) => {
                let mut v = 0u32;
                for _ in 0..2 {
                    match raw.get(i).and_then(|d| (*d as char).to_digit(16)) {
                        Some(d) => {
                            v = v * 16 + d;
                            i += 1;
                        }
                        None => break,
                    }
                }
                bytes.push(v as u8);
            }
            other => bytes.push(other),
        }
    }
    Cow::Owned(String::from_utf8_lossy(&bytes).into_owned())
}
