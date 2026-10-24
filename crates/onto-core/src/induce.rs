//! Sheet induction: one sheet in, one ontology out.
//!
//! [`induce`] reads a CSV sheet (one row per object, a header row naming the
//! columns) and proposes the schema that describes it:
//!
//! * an **object type** whose properties are the sheet's columns, each typed
//!   from the values it holds (int, float, bool, timestamp, enum, text or
//!   string) and required only when every row fills it;
//! * a **key** column (filled and unique in every row) and a **title** column;
//! * a **state** column when a `state` or `status` column reads as an enum;
//! * **link types** for columns whose values are the keys of objects already
//!   in the graph (`customer_id` → `customer`), or of the sheet's own rows
//!   (`manager_id` → `reports_to`);
//! * a **shape** stating what the rows show (required columns, enum values,
//!   datatypes, link classes). The rows satisfy it by construction.
//!
//! Every decision carries an evidence sentence. The result is a description
//! of the data as it is, not of what it should be: its [`SchemaChangeSet`]
//! goes through `schema.plan` and then `schema.apply` or a proposal like any
//! other change, and its [`SheetMapping`] is what [`rows_to_mutations`] and
//! [`Ontology::ingest_sheet`] read the rows with afterwards.
//!
//! Ingestion applies a lexical guard: a cell that does not read as its
//! property's declared type is left out and reported, never coerced; a link
//! to an object that does not exist is left out and reported. Cells are
//! trimmed, and an empty cell means "absent": it leaves an existing value or
//! link as it is.
//!
//! The CSV reader follows RFC 4180 (quoted fields, doubled quotes, newlines
//! inside quotes, CRLF or LF line ends), strips a byte order mark, detects
//! the delimiter (`,` `;` TAB `|`) from the header row and refuses sheets
//! over [`MAX_BYTES`] bytes or [`MAX_ROWS`] rows.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::engine::Extra;
use crate::graph::Graph;
use crate::live_schema::{SchemaChange, SchemaChangeSet, valid_name};
use crate::model::{Entity, EntityInput, LinkInput, Mutation, Provenance};
use crate::modeling::{plural, singular, snake, title_case};
use crate::policy::{self, Access, Principal};
use crate::reason::{KEYWORDS, spell_entity, vocabulary};
use crate::schema::{Cardinality, LinkType, ObjectType, PropType, PropertyDef, Schema};
use crate::shape_model::{PropertyShape, Shape};
use crate::time::{now_ms, parse_ts};
use crate::{ActionContext, Error, Ontology, Result};

/// Sheets larger than this many bytes are refused.
pub const MAX_BYTES: usize = 50 * 1024 * 1024;

/// Sheets with more data rows than this are refused.
pub const MAX_ROWS: usize = 1_000_000;

/// A string column is an enum when it has at least 2 and at most this many
/// distinct values (and at least [`ENUM_ROWS_PER_VALUE`] filled rows per value).
pub const ENUM_MAX_VALUES: usize = 12;

/// Filled rows an enum needs per distinct value.
pub const ENUM_ROWS_PER_VALUE: usize = 5;

/// A string column whose average length exceeds this is text.
pub const TEXT_MIN_AVERAGE: f64 = 80.0;

/// Share of a column's filled cells that must be keys of one kind for the
/// column to become a link.
pub const LINK_MIN_SHARE: f64 = 0.9;

/// Ingest reports list at most this many findings (all are counted).
pub const MAX_FINDINGS: usize = 1000;

/// What an induction is.
pub const MEANS: &str = "describes the data as it is, not what it should be: every type, key, link and shape \
     constraint is a measurement of these rows, and the change set is a proposal to plan and review";

/// What an ingest report is.
pub const INGEST_MEANS: &str = "rows read with the mapping's declared types; a cell that does not read as its \
     type, or a link to a missing object, is left out and listed in findings, never coerced";

/// Column names that read as a key, as a whole name or after `_`.
const KEY_WORDS: &[&str] = &["id", "key", "code", "number"];

/// Column names that read as a title.
const TITLE_WORDS: &[&str] = &["name", "title", "label"];

/// Column names that read as the object state.
const STATE_WORDS: &[&str] = &["state", "status"];

/// Column suffixes that read as a reference to another object.
const REF_SUFFIXES: &[&str] = &["_id", "_key", "_code", "_ref", "_number", "_no"];

/// Self-reference stems that mean "reports to".
const REPORTS_TO: &[&str] = &["manager", "line_manager", "supervisor", "boss", "reports_to"];

/// Column names that refer to a row of the same sheet even without a
/// reference suffix (so integer values may link).
const SELF_WORDS: &[&str] = &["parent", "manager", "line_manager", "supervisor", "boss", "reports_to"];

/// Properties with a meaning of their own (access control); a column with
/// one of these names gets another property name.
const RESERVED_PROPS: &[&str] = &["visibility", "acl"];

// ── CSV ──────────────────────────────────────────────────────────────────

/// A parsed sheet: a header row and the data rows, every row as long as the
/// header.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Sheet {
    pub delimiter: char,
    /// Column names, trimmed; an empty name becomes `column_<n>` (1-based).
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// The line each data row starts on (1-based; the header is line 1 when
    /// it fits on one line).
    pub lines: Vec<usize>,
}

impl Sheet {
    /// The trimmed cell, `None` when empty.
    pub fn cell(&self, row: usize, col: usize) -> Option<&str> {
        self.rows.get(row).and_then(|r| r.get(col)).map(|c| c.trim()).filter(|c| !c.is_empty())
    }

    /// Index of a column by its header name, or by the snake_case of it.
    pub fn column(&self, name: &str) -> Option<usize> {
        self.header.iter().position(|h| h == name).or_else(|| {
            let want = snake(name);
            self.header.iter().position(|h| snake(h) == want)
        })
    }
}

/// The delimiter among `,` `;` TAB `|` that occurs most often in the header
/// row outside quotes (`,` on a tie or when none occurs).
fn detect_delimiter(text: &str) -> char {
    const CANDIDATES: [char; 4] = [',', ';', '\t', '|'];
    let mut counts = [0usize; 4];
    let mut in_quotes = false;
    for c in text.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            '\n' | '\r' if !in_quotes => break,
            _ if !in_quotes => {
                if let Some(i) = CANDIDATES.iter().position(|d| *d == c) {
                    counts[i] += 1;
                }
            }
            _ => {}
        }
    }
    let mut best = 0;
    for i in 1..CANDIDATES.len() {
        if counts[i] > counts[best] {
            best = i;
        }
    }
    CANDIDATES[best]
}

/// Close the current record; a blank line (one empty, unquoted field) is dropped.
fn end_record(
    records: &mut Vec<(usize, Vec<String>)>,
    record: &mut Vec<String>,
    field: &mut String,
    quoted: bool,
    line: usize,
) {
    record.push(std::mem::take(field));
    let rec = std::mem::take(record);
    if quoted || rec.len() > 1 || !rec[0].trim().is_empty() {
        records.push((line, rec));
    }
}

/// Read a CSV sheet (RFC 4180). The first record is the header; blank lines
/// are skipped; a short row is padded with empty cells; a row longer than the
/// header is refused unless the extra cells are empty.
pub fn parse_csv(text: &str) -> Result<Sheet> {
    if text.len() > MAX_BYTES {
        return Err(Error::invalid(format!("sheet is {} bytes; at most {MAX_BYTES} bytes are read", text.len())));
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let delimiter = detect_delimiter(text);
    let mut records: Vec<(usize, Vec<String>)> = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let (mut in_quotes, mut quoted, mut after_quote) = (false, false, false);
    let (mut line, mut record_line, mut quote_line) = (1usize, 1usize, 1usize);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => {
                    in_quotes = false;
                    after_quote = true;
                }
                '\n' => {
                    line += 1;
                    field.push(c);
                }
                '\r' => {
                    if chars.peek() != Some(&'\n') {
                        line += 1;
                    }
                    field.push(c);
                }
                _ => field.push(c),
            }
            continue;
        }
        if c == delimiter {
            record.push(std::mem::take(&mut field));
            (quoted, after_quote) = (false, false);
        } else if c == '\n' || c == '\r' {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            end_record(&mut records, &mut record, &mut field, quoted, record_line);
            (quoted, after_quote) = (false, false);
            line += 1;
            record_line = line;
            if records.len() > MAX_ROWS + 1 {
                return Err(Error::invalid(format!("sheet has more than {MAX_ROWS} rows")));
            }
        } else if after_quote {
            if !c.is_whitespace() {
                return Err(Error::invalid(format!("csv line {line}: text after a closing quote ({c:?})")));
            }
        } else if c == '"' && !quoted && field.is_empty() {
            (in_quotes, quoted, quote_line) = (true, true, line);
        } else {
            field.push(c);
        }
    }
    if in_quotes {
        return Err(Error::invalid(format!("csv line {quote_line}: a quoted field is never closed")));
    }
    if !field.is_empty() || !record.is_empty() || quoted {
        end_record(&mut records, &mut record, &mut field, quoted, record_line);
    }
    let mut records = records.into_iter();
    let Some((_, head)) = records.next() else {
        return Err(Error::invalid("sheet is empty: a header row naming the columns is required"));
    };
    let mut header = Vec::with_capacity(head.len());
    for (i, h) in head.iter().enumerate() {
        let name = h.trim();
        let name = if name.is_empty() { format!("column_{}", i + 1) } else { name.to_string() };
        if header.contains(&name) {
            return Err(Error::invalid(format!("column {name:?} appears twice in the header")));
        }
        header.push(name);
    }
    let mut rows = Vec::new();
    let mut lines = Vec::new();
    for (at, mut rec) in records {
        if rec.len() > header.len() {
            if rec[header.len()..].iter().any(|c| !c.trim().is_empty()) {
                return Err(Error::invalid(format!(
                    "csv line {at}: {} cells, but the header names {} columns",
                    rec.len(),
                    header.len()
                )));
            }
            rec.truncate(header.len());
        }
        rec.resize(header.len(), String::new());
        rows.push(rec);
        lines.push(at);
    }
    if rows.len() > MAX_ROWS {
        return Err(Error::invalid(format!("sheet has {} rows; at most {MAX_ROWS} are read", rows.len())));
    }
    Ok(Sheet { delimiter, header, rows, lines })
}

// ── lexical forms ────────────────────────────────────────────────────────

/// `0` or a digit run without leading zeros, optionally negative.
fn lex_digits(s: &str) -> bool {
    let d = s.strip_prefix('-').unwrap_or(s);
    !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()) && (d == "0" || !d.starts_with('0')) && s != "-0"
}

/// An integer in canonical form (`42`, `-7`; not `007` or `+1`).
pub fn lex_int(s: &str) -> Option<i64> {
    if lex_digits(s) { s.parse().ok() } else { None }
}

/// A decimal number (`1.5`, `-2`, `3e8`; not `.5`, `1.`, `NaN` or `1,5`).
pub fn lex_float(s: &str) -> Option<f64> {
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let (int, frac) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    let ok = lex_digits(int)
        && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
        && exp.is_none_or(|e| {
            let e = e.strip_prefix(['+', '-']).unwrap_or(e);
            !e.is_empty() && e.bytes().all(|b| b.is_ascii_digit())
        });
    if !ok {
        return None;
    }
    s.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// `true`/`false`/`yes`/`no`, in any case.
pub fn lex_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "yes" => Some(true),
        "false" | "no" => Some(false),
        _ => None,
    }
}

/// An RFC 3339 date-time (`2026-10-24T12:00:00Z`, `…T12:00:00.5+02:00`) or a
/// `YYYY-MM-DD` date, on a real calendar day.
pub fn lex_timestamp(s: &str) -> bool {
    let b = s.as_bytes();
    let digits = |r: std::ops::Range<usize>| b.get(r).is_some_and(|x| x.iter().all(u8::is_ascii_digit));
    let date = b.len() >= 10 && digits(0..4) && b[4] == b'-' && digits(5..7) && b[7] == b'-' && digits(8..10);
    if !date {
        return false;
    }
    if b.len() > 10 {
        let time = b.len() >= 19
            && matches!(b[10], b'T' | b't' | b' ')
            && digits(11..13)
            && b[13] == b':'
            && digits(14..16)
            && b[16] == b':'
            && digits(17..19);
        if !time {
            return false;
        }
        let mut i = 19;
        if b.get(i) == Some(&b'.') {
            i += 1;
            let start = i;
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            if i == start {
                return false;
            }
        }
        let zone = match &b[i..] {
            [b'Z' | b'z'] => true,
            [b'+' | b'-', h1, h2, b':', m1, m2] => [h1, h2, m1, m2].iter().all(|x| x.is_ascii_digit()),
            _ => false,
        };
        if !zone {
            return false;
        }
    }
    parse_ts(s).is_some()
}

/// The JSON value of `cell` under `def`, or why it does not read as one.
/// Nothing is coerced: `007` is not an int, `1,5` is not a float, `maybe`
/// is not a bool.
pub fn read_value(def: &PropertyDef, cell: &str) -> std::result::Result<Value, String> {
    let v = match def.ty {
        PropType::Int => lex_int(cell).map(Value::from).ok_or("not an integer")?,
        PropType::Float => {
            lex_float(cell).and_then(serde_json::Number::from_f64).map(Value::Number).ok_or("not a number")?
        }
        PropType::Bool => lex_bool(cell).map(Value::Bool).ok_or("not true, false, yes or no")?,
        PropType::Timestamp => {
            if !lex_timestamp(cell) {
                return Err("not an RFC 3339 timestamp or a YYYY-MM-DD date".into());
            }
            Value::from(cell)
        }
        PropType::String | PropType::Text | PropType::Enum => Value::from(cell),
        PropType::Json => serde_json::from_str(cell).map_err(|e| format!("not JSON ({e})"))?,
        PropType::List => return Err("list properties are not read from a sheet cell".into()),
    };
    if def.accepts(&v) { Ok(v) } else { Err(format!("must be {}", def.expected())) }
}

// ── induction ────────────────────────────────────────────────────────────

/// What to induce.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InduceOptions {
    /// The object type name; else the singular of the file name, else of
    /// the first key-like column (`order_id` → `order`), else `record`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The key column (header name); else the first column filled and unique
    /// in every row, preferring key-like names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The sheet's file name (`orders.csv`), used for the kind name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Only objects of this workspace count as link targets (the engine
    /// wrapper also hides what the principal may not read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

/// What a column became.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnRole {
    /// The object key: ids are `kind:<value>`.
    Key,
    /// The object title.
    Title,
    /// The object state.
    State,
    #[default]
    Property,
    Link,
    /// Empty in every row: nothing to describe.
    Skipped,
}

/// One column and what was read from it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InducedColumn {
    pub column: String,
    pub role: ColumnRole,
    /// The property it fills (role `property`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
    #[serde(rename = "type")]
    pub ty: PropType,
    /// Filled in every row.
    pub required: bool,
    /// Rows with a value.
    pub filled: usize,
    /// Distinct values.
    pub distinct: usize,
    /// Enum values (sorted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    pub evidence: String,
}

/// The column chosen as key or title, and why.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ColumnChoice {
    /// `None`: no column qualifies (a missing key means rows are keyed by
    /// their row number; a missing title leaves titles empty).
    pub column: Option<String>,
    pub evidence: String,
}

/// A column that became a link.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InducedLink {
    pub column: String,
    /// The link type (canonical name).
    pub rel: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse: Option<String>,
    pub target_kind: String,
    /// The link type is new (part of the change set) rather than an existing one.
    pub new_link_type: bool,
    /// Links to the sheet's own rows.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub self_link: bool,
    /// Filled cells whose value is the key of a `target_kind` object.
    pub matched: usize,
    /// Filled cells.
    pub filled: usize,
    pub evidence: String,
}

/// Why a shape constraint was induced.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ShapeEvidence {
    /// The property shape id.
    pub constraint: String,
    pub text: String,
}

/// How a column maps to a link.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkMapping {
    /// A relation name (canonical, inverse or `^rel`).
    pub rel: String,
    /// The kind of the objects the cells name: ids are `target_kind:<value>`.
    pub target_kind: String,
}

/// How a sheet's rows become objects. Column names are header names.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SheetMapping {
    pub kind: String,
    /// The key column; `None` keys rows by their 1-based row number, so a
    /// re-ingest of a reordered sheet would not find its objects again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Column → property.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, String>,
    /// Column → link.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub links: BTreeMap<String, LinkMapping>,
}

/// One sheet's induced ontology.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Induced {
    pub kind: String,
    /// Why the kind has this name.
    pub kind_evidence: String,
    /// Data rows read.
    pub rows: usize,
    pub delimiter: String,
    pub columns: Vec<InducedColumn>,
    pub key: ColumnChoice,
    pub title: ColumnChoice,
    pub links: Vec<InducedLink>,
    /// The shape's name in the change set.
    pub shape_name: String,
    pub shape: Shape,
    pub shape_evidence: Vec<ShapeEvidence>,
    /// `put_object_type`, then `put_link_type` for each new link type, then
    /// `put_shape`.
    pub change_set: SchemaChangeSet,
    pub mapping: SheetMapping,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub means: String,
}

/// What was measured in one column.
struct Stats {
    filled: usize,
    distinct: BTreeSet<String>,
    ty: PropType,
    type_evidence: String,
    /// Every filled value is an integer (link columns of such values need a
    /// reference-like name).
    numeric: bool,
}

fn stats(sheet: &Sheet, col: usize) -> Stats {
    let cells: Vec<&str> = (0..sheet.rows.len()).filter_map(|r| sheet.cell(r, col)).collect();
    let filled = cells.len();
    let distinct: BTreeSet<String> = cells.iter().map(|c| c.to_string()).collect();
    let d = distinct.len();
    let all = |f: &dyn Fn(&str) -> bool| filled > 0 && cells.iter().all(|c| f(c));
    let numeric = all(&|c| lex_int(c).is_some());
    let (ty, type_evidence) = if filled == 0 {
        (PropType::String, "empty in every row".to_string())
    } else if numeric {
        (PropType::Int, format!("all {filled} filled values are integers"))
    } else if all(&|c| lex_float(c).is_some()) {
        (PropType::Float, format!("all {filled} filled values are numbers, not all integers"))
    } else if all(&|c| lex_bool(c).is_some()) {
        (PropType::Bool, format!("all {filled} filled values are true, false, yes or no"))
    } else if all(&lex_timestamp) {
        (PropType::Timestamp, format!("all {filled} filled values are RFC 3339 timestamps or YYYY-MM-DD dates"))
    } else if (2..=ENUM_MAX_VALUES).contains(&d) && filled >= ENUM_ROWS_PER_VALUE * d {
        let shown: Vec<&str> = distinct.iter().map(String::as_str).collect();
        (
            PropType::Enum,
            format!(
                "{d} distinct values ({}) in {filled} filled rows, at least {ENUM_ROWS_PER_VALUE} rows per value",
                shown.join(", ")
            ),
        )
    } else {
        let chars: usize = cells.iter().map(|c| c.chars().count()).sum();
        let avg = chars as f64 / filled as f64;
        if avg > TEXT_MIN_AVERAGE {
            (PropType::Text, format!("strings averaging {avg:.0} characters (over {TEXT_MIN_AVERAGE:.0}): text"))
        } else {
            let why = if d < 2 {
                "one distinct value, too few for an enum".to_string()
            } else if d > ENUM_MAX_VALUES {
                format!("{d} distinct values, too many for an enum")
            } else {
                format!("{d} distinct values in {filled} filled rows, too few rows for an enum")
            };
            (PropType::String, format!("strings: {why}"))
        }
    };
    Stats { filled, distinct, ty, type_evidence, numeric }
}

fn ty_name(ty: PropType) -> String {
    format!("{ty:?}").to_lowercase()
}

/// The part of a column name before a reference suffix (`customer_id` → `customer`).
fn ref_stem(column: &str) -> (String, bool) {
    let c = snake(column);
    for suffix in REF_SUFFIXES {
        if let Some(s) = c.strip_suffix(suffix)
            && !s.is_empty()
        {
            return (s.to_string(), true);
        }
    }
    (c, false)
}

/// `[a-z][a-z0-9_]*`, cut to `max` characters without a trailing `_`.
fn truncate(s: &str, max: usize) -> String {
    let mut s = s.to_string();
    s.truncate(max);
    s.trim_end_matches('_').to_string()
}

/// `base`, else `base_2`, `base_3`, … whichever is free.
fn free_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(base) {
        return base.to_string();
    }
    (2..).map(|i| format!("{}_{i}", truncate(base, 60))).find(|n| !taken(n)).unwrap_or_default()
}

/// `orders.csv`, `/data/Orders 2026.csv` → the file stem.
fn file_stem(file: &str) -> &str {
    let name = file.rsplit(['/', '\\']).next().unwrap_or(file);
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

/// Does a column name read as a key (`id`, `order_id`, `sku_code`, `OrderNumber`)?
fn key_like(column: &str) -> bool {
    let c = snake(column);
    KEY_WORDS.iter().any(|w| c == *w || c.ends_with(&format!("_{w}")))
}

/// A short, quoted list of sample values for messages.
fn sample(values: &[&str]) -> String {
    let shown: Vec<String> = values.iter().take(5).map(|v| format!("{v:?}")).collect();
    let more = if values.len() > 5 { format!(" and {} more", values.len() - 5) } else { String::new() };
    format!("{}{more}", shown.join(", "))
}

/// Induce the ontology one sheet describes. `can_read` decides which graph
/// objects count as link targets.
pub fn induce(
    schema: &Schema,
    g: &Graph,
    csv: &str,
    opts: &InduceOptions,
    can_read: &dyn Fn(&Entity) -> bool,
) -> Result<Induced> {
    let sheet = parse_csv(csv)?;
    induce_parsed(schema, g, &sheet, opts, can_read)
}

/// [`induce`] over a parsed sheet.
pub fn induce_parsed(
    schema: &Schema,
    g: &Graph,
    sheet: &Sheet,
    opts: &InduceOptions,
    can_read: &dyn Fn(&Entity) -> bool,
) -> Result<Induced> {
    let n = sheet.rows.len();
    if n == 0 {
        return Err(Error::invalid("sheet has a header but no rows: there is nothing to describe"));
    }
    let visible = |e: &Entity| can_read(e) && opts.workspace.as_deref().is_none_or(|w| e.workspace == w);
    let mut warnings = Vec::new();
    let vocab = vocabulary(schema);
    let stats: Vec<Stats> = (0..sheet.header.len()).map(|c| stats(sheet, c)).collect();

    // Kind name.
    let (base, kind_why) = match opts.kind.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        Some(k) => {
            if !valid_name(k) {
                return Err(Error::invalid(format!("kind '{k}' must match [a-z][a-z0-9_]* (at most 64 characters)")));
            }
            (k.to_string(), "named by the caller".to_string())
        }
        None => match opts.file.as_deref().map(file_stem).filter(|s| !s.trim().is_empty()) {
            Some(stem) => (singular(&snake(stem)), format!("the singular of the file name {stem:?}")),
            None => {
                let col = sheet.header.iter().find_map(|h| {
                    let (stem, suffixed) = ref_stem(h);
                    (suffixed && !KEY_WORDS.contains(&stem.as_str())).then_some((h, stem))
                });
                match col {
                    Some((h, stem)) => (singular(&stem), format!("the singular of the first key-like column {h:?}")),
                    None => ("record".to_string(), "no file name or key-like column names the rows".to_string()),
                }
            }
        },
    };
    let kind_taken =
        |k: &str| schema.is_type_or_interface(k) || vocab.classes.contains(k) || g.of_kind(k).next().is_some();
    let kind = free_name(&base, kind_taken);
    let kind_evidence = if kind == base {
        format!("{kind}: {kind_why}")
    } else {
        warnings.push(format!(
            "{base} exists already (an object type, interface, derived class or kind in use): the sheet is induced \
             as {kind}; pass kind to choose another name, or ingest into {base} with a mapping of your own"
        ));
        format!("{kind}: {kind_why}, suffixed because {base} exists already")
    };

    // Columns that may hold keys of other objects: integers or strings, and
    // not names (a name column holds names, even when a name equals a key).
    let link_candidate = |c: usize| {
        stats[c].filled > 0
            && matches!(stats[c].ty, PropType::Int | PropType::String | PropType::Enum)
            && !TITLE_WORDS.contains(&snake(&sheet.header[c]).as_str())
    };
    // Which graph objects the cells name: key → kinds, over every visible
    // object of a declared kind whose key appears in a candidate column.
    let cell_values: HashSet<&str> = (0..sheet.header.len())
        .filter(|c| link_candidate(*c))
        .flat_map(|c| stats[c].distinct.iter().map(String::as_str))
        .collect();
    let mut kinds_by_key: HashMap<&str, BTreeSet<&str>> = HashMap::new();
    for e in g.entities() {
        if let Some(key) = cell_values.get(e.key())
            && schema.object_types.contains_key(&e.kind)
            && visible(e)
        {
            kinds_by_key.entry(*key).or_default().insert(e.kind.as_str());
        }
    }

    // External link candidates: >= 90% of filled cells are keys of one kind.
    struct Ext {
        target: String,
        matched: usize,
        unmatched: Vec<String>,
        why: String,
    }
    let mut external: BTreeMap<usize, Ext> = BTreeMap::new();
    for (c, st) in stats.iter().enumerate() {
        if !link_candidate(c) {
            continue;
        }
        let (stem, suffixed) = ref_stem(&sheet.header[c]);
        let hinted_kind = |k: &str| stem == k || schema.canonical_kind(&stem).as_deref() == Some(k);
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for r in 0..n {
            if let Some(v) = sheet.cell(r, c)
                && let Some(ks) = kinds_by_key.get(v)
            {
                for k in ks {
                    *counts.entry(*k).or_default() += 1;
                }
            }
        }
        let qualified: Vec<(&str, usize)> =
            counts.into_iter().filter(|(_, m)| *m as f64 >= LINK_MIN_SHARE * st.filled as f64).collect();
        let pick = match qualified.as_slice() {
            [] => None,
            [one] => Some(*one),
            many => {
                let hinted: Vec<&(&str, usize)> = many.iter().filter(|(k, _)| hinted_kind(k)).collect();
                if let [one] = hinted.as_slice() {
                    Some(**one)
                } else {
                    let names: Vec<&str> = many.iter().map(|(k, _)| *k).collect();
                    warnings.push(format!(
                        "{}: the values are keys of objects of several kinds ({}): no link induced",
                        sheet.header[c],
                        names.join(", ")
                    ));
                    None
                }
            }
        };
        let Some((target, matched)) = pick else { continue };
        if st.numeric && !suffixed && !hinted_kind(target) {
            warnings.push(format!(
                "{}: {matched} of {} integer values are keys of {target} objects, but the column name does not \
                 refer to {target}: kept as a property",
                sheet.header[c], st.filled
            ));
            continue;
        }
        let unmatched: Vec<String> = (0..n)
            .filter_map(|r| sheet.cell(r, c))
            .filter(|v| !kinds_by_key.get(v).is_some_and(|ks| ks.contains(target)))
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let pct = 100.0 * matched as f64 / st.filled as f64;
        let why = format!(
            "{matched} of {} filled values are keys of existing {target} objects ({pct:.0}%, at least {:.0}%)",
            st.filled,
            LINK_MIN_SHARE * 100.0
        );
        external.insert(c, Ext { target: target.to_string(), matched, unmatched, why });
    }

    // Key.
    let unique_full = |c: usize| {
        stats[c].filled == n
            && stats[c].distinct.len() == n
            && stats[c]
                .distinct
                .iter()
                .all(|v| spell_entity(&format!("{kind}:{v}")).is_ok() && kind.len() + v.len() < 256)
    };
    let key_col: Option<usize> = match opts.key.as_deref() {
        Some(name) => {
            let c =
                sheet.column(name).ok_or_else(|| Error::invalid(format!("key column {name:?} is not in the sheet")))?;
            if stats[c].filled < n {
                let r = (0..n).find(|r| sheet.cell(*r, c).is_none()).unwrap_or(0);
                return Err(Error::invalid(format!(
                    "key column {name:?} is empty in row {} (line {})",
                    r + 1,
                    sheet.lines[r]
                )));
            }
            if stats[c].distinct.len() < n {
                return Err(Error::invalid(format!("key column {name:?} repeats values: a key must be unique")));
            }
            if !unique_full(c) {
                return Err(Error::invalid(format!(
                    "key column {name:?} holds values that cannot be part of an id (a TAB, line break or '>', or too long)"
                )));
            }
            Some(c)
        }
        None => {
            let rank = |c: usize| -> usize {
                let (stem, _) = ref_stem(&sheet.header[c]);
                let name = snake(&sheet.header[c]);
                if KEY_WORDS.contains(&name.as_str()) || (key_like(&sheet.header[c]) && (stem == base || stem == kind))
                {
                    0
                } else if external.contains_key(&c) {
                    3
                } else if key_like(&sheet.header[c]) {
                    1
                } else {
                    2
                }
            };
            (0..sheet.header.len()).filter(|c| unique_full(*c)).min_by_key(|c| (rank(*c), *c))
        }
    };
    let key = match key_col {
        Some(c) => {
            let h = &sheet.header[c];
            let why = if opts.key.is_some() {
                "named by the caller".to_string()
            } else if key_like(h) {
                "its name reads as a key".to_string()
            } else {
                "the first such column (no column name reads as a key)".to_string()
            };
            ColumnChoice {
                column: Some(h.clone()),
                evidence: format!("{h} is filled and unique in all {n} rows; {why}: ids are {kind}:<{h}>"),
            }
        }
        None => {
            warnings.push(format!(
                "no column is filled and unique in every row: rows are keyed by their row number, so ingesting a \
                 reordered sheet would create new {kind} objects; add an id column"
            ));
            ColumnChoice {
                column: None,
                evidence: format!("no column is filled and unique in all {n} rows: ids are {kind}:<row number>"),
            }
        }
    };
    if let Some(k) = key_col {
        external.remove(&k);
    }

    // Self links: every filled value is a key of this sheet.
    let mut self_links: BTreeMap<usize, (String, usize)> = BTreeMap::new();
    if let Some(kc) = key_col {
        let keys: BTreeSet<&str> = (0..n).filter_map(|r| sheet.cell(r, kc)).collect();
        for (c, st) in stats.iter().enumerate() {
            if c == kc || st.filled == 0 || TITLE_WORDS.contains(&snake(&sheet.header[c]).as_str()) {
                continue;
            }
            let (stem, suffixed) = ref_stem(&sheet.header[c]);
            let all_keys = (0..n).filter_map(|r| sheet.cell(r, c)).all(|v| keys.contains(v));
            let own = (0..n).filter(|r| sheet.cell(*r, c).is_some() && sheet.cell(*r, c) == sheet.cell(*r, kc)).count();
            if !all_keys || 2 * own > st.filled {
                continue;
            }
            if st.numeric && !suffixed && !SELF_WORDS.contains(&stem.as_str()) {
                continue;
            }
            let why = if own == 0 {
                format!("all {} filled values are keys of this sheet's own rows", st.filled)
            } else {
                warnings.push(format!(
                    "{}: {own} row(s) name themselves: an object cannot link to itself, so they get no link",
                    sheet.header[c]
                ));
                format!(
                    "all {} filled values are keys of this sheet's own rows ({own} of them the row's own key)",
                    st.filled
                )
            };
            if let Some(ext) = external.get(&c) {
                // Both readings fit: the name decides, else the sheet itself.
                if stem == ext.target || schema.canonical_kind(&stem).as_deref() == Some(ext.target.as_str()) {
                    continue;
                }
                external.remove(&c);
            }
            self_links.insert(c, (why, own));
        }
    }

    // State.
    let state_col = (0..sheet.header.len()).find(|c| {
        Some(*c) != key_col
            && !external.contains_key(c)
            && !self_links.contains_key(c)
            && STATE_WORDS.contains(&snake(&sheet.header[*c]).as_str())
            && stats[*c].ty == PropType::Enum
    });

    // Title.
    let free_col = |c: usize| {
        Some(c) != key_col && Some(c) != state_col && !external.contains_key(&c) && !self_links.contains_key(&c)
    };
    let title_rank = |c: usize| -> Option<usize> {
        let name = snake(&sheet.header[c]);
        if TITLE_WORDS.contains(&name.as_str()) {
            Some(0)
        } else if TITLE_WORDS.iter().any(|w| name.ends_with(&format!("_{w}"))) {
            Some(1)
        } else if stats[c].ty == PropType::String && stats[c].filled > 0 {
            Some(2)
        } else {
            None
        }
    };
    let title_col = (0..sheet.header.len())
        .filter(|c| free_col(*c) && stats[*c].filled > 0)
        .filter_map(|c| title_rank(c).map(|r| (r, c)))
        .min()
        .map(|(_, c)| c);
    let title = match title_col {
        Some(c) => {
            let h = &sheet.header[c];
            let why = match title_rank(c) {
                Some(0) | Some(1) => "its name reads as a title",
                _ => "the first string column that is not the key",
            };
            ColumnChoice {
                column: Some(h.clone()),
                evidence: format!("{h}: {why}; filled in {} of {n} rows", stats[c].filled),
            }
        }
        None => ColumnChoice {
            column: None,
            evidence:
                "no column reads as a title and no string column is left: titles stay empty (objects show their id)"
                    .into(),
        },
    };

    // Links: names and link types.
    let mut rels_taken: BTreeSet<String> = KEYWORDS.iter().map(|k| k.to_string()).collect();
    for lt in schema.link_types.values() {
        rels_taken.insert(lt.name.clone());
        rels_taken.extend(lt.inverse.clone());
    }
    rels_taken.extend(vocab.relations.iter().cloned());
    for r in &schema.rules {
        if let Some(h) = r.head_predicate()
            && h != "a"
        {
            rels_taken.insert(h.trim_start_matches('^').to_string());
        }
    }
    let kind_plural = plural(&kind);
    let mut links: Vec<InducedLink> = Vec::new();
    let mut new_link_types: Vec<(String, LinkType)> = Vec::new();
    for (c, (h, st)) in sheet.header.iter().zip(&stats).enumerate() {
        let (stem, _) = ref_stem(h);
        if let Some(ext) = external.get(&c) {
            let target = ext.target.clone();
            // An existing link type of this name that already admits any
            // source and this target is reused as it is.
            let fits = schema.link_type(&stem).is_some_and(|lt| {
                lt.from.is_empty()
                    && !lt.symmetric
                    && !lt.provenance
                    && (lt.to.is_empty() || lt.to.iter().any(|t| schema.kind_is(&target, t)))
            });
            let link = if fits {
                let lt = &schema.link_types[&stem];
                InducedLink {
                    column: h.clone(),
                    rel: stem.clone(),
                    inverse: lt.inverse.clone(),
                    target_kind: target.clone(),
                    new_link_type: false,
                    self_link: false,
                    matched: ext.matched,
                    filled: st.filled,
                    evidence: format!(
                        "{h}: {}; the existing link type {stem} (from any kind to {target}) fits",
                        ext.why
                    ),
                }
            } else {
                let mut name = truncate(&stem, 64);
                if rels_taken.contains(&name) {
                    name = truncate(&format!("{kind}_{stem}"), 64);
                }
                let name = free_name(&name, |x| rels_taken.contains(x));
                rels_taken.insert(name.clone());
                let mut inverse = kind_plural.clone();
                if rels_taken.contains(&inverse) {
                    inverse = truncate(&format!("{name}_{kind_plural}"), 64);
                }
                let inverse = free_name(&inverse, |x| rels_taken.contains(x));
                rels_taken.insert(inverse.clone());
                let def = LinkType {
                    name: name.clone(),
                    label: title_case(&name),
                    description: format!(
                        "The {} a {} refers to (sheet column {h}).",
                        title_case(&target).to_lowercase(),
                        title_case(&kind).to_lowercase()
                    ),
                    from: vec![kind.clone()],
                    to: vec![target.clone()],
                    inverse: Some(inverse.clone()),
                    cardinality: Cardinality::One,
                    ..Default::default()
                };
                new_link_types.push((name.clone(), def));
                InducedLink {
                    column: h.clone(),
                    rel: name.clone(),
                    inverse: Some(inverse.clone()),
                    target_kind: target.clone(),
                    new_link_type: true,
                    self_link: false,
                    matched: ext.matched,
                    filled: st.filled,
                    evidence: format!(
                        "{h}: {}; a new link type {name} ({kind} → {target}, at most one per {kind}: one cell per row), inverse {inverse}",
                        ext.why
                    ),
                }
            };
            if !ext.unmatched.is_empty() {
                let shown: Vec<&str> = ext.unmatched.iter().map(String::as_str).collect();
                warnings.push(format!(
                    "{h}: {} value(s) name no {target} object ({}): those rows get no {} link",
                    ext.unmatched.len(),
                    sample(&shown),
                    link.rel
                ));
            }
            links.push(link);
        } else if let Some((why, own)) = self_links.get(&c) {
            let reports = REPORTS_TO.contains(&stem.as_str());
            let (base_name, base_inv) = if reports {
                ("reports_to".to_string(), "direct_reports".to_string())
            } else {
                (truncate(&stem, 64), truncate(&format!("{stem}_of"), 64))
            };
            let mut name = base_name.clone();
            if rels_taken.contains(&name) {
                name = truncate(&format!("{kind}_{base_name}"), 64);
            }
            let name = free_name(&name, |x| rels_taken.contains(x));
            rels_taken.insert(name.clone());
            let mut inverse = base_inv.clone();
            if rels_taken.contains(&inverse) {
                inverse = truncate(&format!("{kind}_{base_inv}"), 64);
            }
            let inverse = free_name(&inverse, |x| rels_taken.contains(x));
            rels_taken.insert(inverse.clone());
            let def = LinkType {
                name: name.clone(),
                label: title_case(&name),
                description: format!(
                    "The {} a {} refers to among its own kind (sheet column {h}).",
                    title_case(&kind).to_lowercase(),
                    title_case(&kind).to_lowercase()
                ),
                from: vec![kind.clone()],
                to: vec![kind.clone()],
                inverse: Some(inverse.clone()),
                cardinality: Cardinality::One,
                ..Default::default()
            };
            new_link_types.push((name.clone(), def));
            let how = if reports { format!("{stem} reads as reports_to") } else { format!("named after {stem}") };
            links.push(InducedLink {
                column: h.clone(),
                rel: name.clone(),
                inverse: Some(inverse.clone()),
                target_kind: kind.clone(),
                new_link_type: true,
                self_link: true,
                matched: st.filled - own,
                filled: st.filled,
                evidence: format!("{h}: {why}; a new link type {name} ({kind} → {kind}, {how}), inverse {inverse}"),
            });
        }
    }

    // Columns and properties.
    let mut props_taken: BTreeSet<String> = BTreeSet::new();
    let mut columns = Vec::new();
    let mut properties: BTreeMap<String, PropertyDef> = BTreeMap::new();
    let mut prop_of: BTreeMap<usize, String> = BTreeMap::new();
    for (c, st) in stats.iter().enumerate() {
        let h = &sheet.header[c];
        let required = st.filled == n;
        let fill =
            if required { format!("filled in all {n} rows") } else { format!("filled in {} of {n} rows", st.filled) };
        let values: Vec<String> =
            if st.ty == PropType::Enum { st.distinct.iter().cloned().collect() } else { Vec::new() };
        let mut col = InducedColumn {
            column: h.clone(),
            role: ColumnRole::Property,
            property: None,
            ty: st.ty,
            required,
            filled: st.filled,
            distinct: st.distinct.len(),
            values: values.clone(),
            evidence: String::new(),
        };
        let typed = format!("{}: {} ({})", ty_name(st.ty), st.type_evidence, fill);
        if Some(c) == key_col {
            col.role = ColumnRole::Key;
            col.evidence = format!("{h}: the key; {typed}");
        } else if Some(c) == title_col {
            col.role = ColumnRole::Title;
            col.evidence = format!("{h}: the title; {typed}");
        } else if Some(c) == state_col {
            col.role = ColumnRole::State;
            col.evidence = format!("{h}: the state, one of {}; {typed}", values.join(", "));
        } else if let Some(l) = links.iter().find(|l| l.column == *h) {
            col.role = ColumnRole::Link;
            col.evidence = format!("{h}: the link {} to {}; {typed}", l.rel, l.target_kind);
        } else if st.filled == 0 {
            col.role = ColumnRole::Skipped;
            col.evidence = format!("{h}: empty in every row, so there is nothing to describe: no property");
            warnings.push(format!("{h} is empty in every row: no property induced"));
        } else {
            let mut name = snake(h);
            let mut renamed = None;
            if RESERVED_PROPS.contains(&name.as_str()) {
                renamed = Some(format!("{name} controls access to objects"));
                name = truncate(&format!("{name}_value"), 64);
            }
            let free = free_name(&name, |x| props_taken.contains(x));
            if free != name {
                renamed = Some(format!("{name} is taken by another column"));
            }
            props_taken.insert(free.clone());
            if let Some(why) = &renamed {
                warnings.push(format!("{h} becomes property {free}: {why}"));
            }
            let searchable = matches!(st.ty, PropType::String | PropType::Text);
            properties.insert(
                free.clone(),
                PropertyDef {
                    ty: st.ty,
                    required,
                    values: values.clone(),
                    searchable,
                    description: format!("Sheet column {h}."),
                    ..Default::default()
                },
            );
            col.property = Some(free.clone());
            col.evidence =
                format!("{h}: property {free}, {typed}{}", if required { ": required" } else { ": optional" });
            prop_of.insert(c, free);
        }
        columns.push(col);
    }

    // Object type.
    let states: Vec<String> = state_col.map(|c| stats[c].distinct.iter().cloned().collect()).unwrap_or_default();
    let object_type = ObjectType {
        name: kind.clone(),
        label: title_case(&kind),
        description: format!(
            "{} induced from a sheet of {n} rows{}.",
            title_case(&kind),
            opts.file.as_deref().map(|f| format!(" ({})", file_stem(f))).unwrap_or_default()
        ),
        properties,
        states,
        ..Default::default()
    };

    // Shape: what every row shows, so the rows satisfy it by construction.
    let shape_name = free_name(&kind, |x| schema.shapes.contains_key(x));
    let mut shape = Shape {
        description: format!("What the {n} rows of the induced {kind} sheet show."),
        target: Some(kind.clone()),
        ..Default::default()
    };
    let mut shape_evidence = Vec::new();
    let mut push = |p: PropertyShape, text: String| {
        shape_evidence.push(ShapeEvidence { constraint: p.id.clone(), text });
        shape.properties.push(p);
    };
    if let Some(c) = title_col
        && stats[c].filled == n
    {
        let id = format!("{shape_name}/title");
        push(
            PropertyShape { id, path: "title".into(), min_count: Some(1), ..Default::default() },
            format!("min_count 1 on title: {} of {n} rows fill {}", n, sheet.header[c]),
        );
    }
    if let Some(c) = state_col {
        let st = &stats[c];
        let id = format!("{shape_name}/state");
        let values: Vec<Value> = st.distinct.iter().map(|v| Value::from(v.as_str())).collect();
        let full = st.filled == n;
        let text = format!(
            "in [{}] on state{}: the values {} shows",
            st.distinct.iter().cloned().collect::<Vec<_>>().join(", "),
            if full { ", min_count 1" } else { "" },
            sheet.header[c]
        );
        push(
            PropertyShape { id, path: "state".into(), min_count: full.then_some(1), values, ..Default::default() },
            text,
        );
    }
    for (c, prop) in &prop_of {
        let st = &stats[*c];
        let h = &sheet.header[*c];
        let full = st.filled == n;
        let id = format!("{shape_name}/prop.{prop}");
        let mut p = PropertyShape { id, path: format!("prop.{prop}"), ..Default::default() };
        let mut parts = Vec::new();
        if full {
            p.min_count = Some(1);
            parts.push(format!("min_count 1: {h} is filled in all {n} rows"));
        }
        if st.ty == PropType::Enum {
            p.values = st.distinct.iter().map(|v| Value::from(v.as_str())).collect();
            parts.push(format!("in: the {} values {h} shows", st.distinct.len()));
        } else {
            p.datatype = Some(st.ty);
            parts.push(format!("datatype {}: {}", ty_name(st.ty), st.type_evidence));
        }
        push(p, format!("prop.{prop}: {}", parts.join("; ")));
    }
    for l in &links {
        let id = format!("{shape_name}/{}", l.rel);
        // Only links whose target exists are written, so min_count holds
        // only when every row names an existing target.
        let full = l.matched == n;
        let mut parts = vec![format!("class {}: every link written goes to a {} object", l.target_kind, l.target_kind)];
        let lt_one = new_link_types.iter().any(|(name, _)| *name == l.rel)
            || schema.link_type(&l.rel).is_some_and(|lt| lt.cardinality == Cardinality::One);
        if full {
            parts.push(format!("min_count 1: all {n} rows name an existing {}", l.target_kind));
        }
        if lt_one {
            parts.push("max_count 1: one cell per row and a single-valued link type".into());
        }
        push(
            PropertyShape {
                id,
                path: l.rel.clone(),
                class: Some(l.target_kind.clone()),
                min_count: full.then_some(1),
                max_count: lt_one.then_some(1),
                ..Default::default()
            },
            format!("{}: {}", l.rel, parts.join("; ")),
        );
    }

    let mut changes =
        vec![SchemaChange::PutObjectType { name: kind.clone(), def: object_type, migrations: Vec::new() }];
    for (name, def) in new_link_types {
        changes.push(SchemaChange::PutLinkType { name, def, migrations: Vec::new() });
    }
    changes.push(SchemaChange::PutShape { name: shape_name.clone(), def: shape.clone() });
    let change_set =
        SchemaChangeSet { changes, message: format!("Induce {kind} from a sheet of {n} rows"), ..Default::default() };
    let mapping = SheetMapping {
        kind: kind.clone(),
        key: key.column.clone(),
        title: title.column.clone(),
        state: state_col.map(|c| sheet.header[c].clone()),
        properties: prop_of.iter().map(|(c, p)| (sheet.header[*c].clone(), p.clone())).collect(),
        links: links
            .iter()
            .map(|l| (l.column.clone(), LinkMapping { rel: l.rel.clone(), target_kind: l.target_kind.clone() }))
            .collect(),
    };
    Ok(Induced {
        kind,
        kind_evidence,
        rows: n,
        delimiter: sheet.delimiter.to_string(),
        columns,
        key,
        title,
        links,
        shape_name,
        shape,
        shape_evidence,
        change_set,
        mapping,
        warnings,
        means: MEANS.into(),
    })
}

// ── rows → mutations ─────────────────────────────────────────────────────

/// What went wrong with one cell or row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// The key cell is empty: the row is left out.
    MissingKey,
    /// The key cannot be part of an id (TAB, line break, `>`, too long): the row is left out.
    BadKey,
    /// Another row has the same key: both write the same object.
    DuplicateKey,
    /// The cell does not read as the property's type: the value is left out.
    BadValue,
    /// The state is not one the kind allows: the state is left out.
    BadState,
    /// A required property is missing or unreadable on a new object: the row is left out.
    MissingRequired,
    /// No object has the key the link cell names: the link is left out.
    MissingTarget,
    /// The link cell names the row itself: the link is left out.
    SelfLink,
}

/// One cell or row the lexical guard left out.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// 1-based data row.
    pub row: usize,
    /// The line the row starts on.
    pub line: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    pub issue: FindingKind,
    pub message: String,
}

/// A mapping resolved against a sheet and a schema.
struct Resolved {
    key: Option<usize>,
    title: Option<usize>,
    state: Option<usize>,
    props: Vec<(usize, String, PropertyDef)>,
    links: Vec<(usize, String, String)>,
}

fn resolve(schema: &Schema, sheet: &Sheet, m: &SheetMapping) -> Result<Resolved> {
    let ty = schema.object_type(&m.kind).ok_or_else(|| {
        Error::invalid(format!("unknown object type {} (apply the induced change set first)", m.kind))
    })?;
    let col = |what: &str, name: &str| {
        sheet.column(name).ok_or_else(|| Error::invalid(format!("{what} column {name:?} is not in the sheet")))
    };
    let key = m.key.as_deref().map(|k| col("key", k)).transpose()?;
    let title = m.title.as_deref().map(|k| col("title", k)).transpose()?;
    let state = m.state.as_deref().map(|k| col("state", k)).transpose()?;
    if state.is_some() && ty.states.is_empty() {
        return Err(Error::invalid(format!("{} declares no states, so the sheet cannot set one", m.kind)));
    }
    let mut props = Vec::new();
    for (c, p) in &m.properties {
        let def = schema
            .prop_def(&m.kind, p)
            .ok_or_else(|| Error::invalid(format!("{} declares no property {p} (column {c:?})", m.kind)))?;
        props.push((col("property", c)?, p.clone(), def.clone()));
    }
    let mut links = Vec::new();
    for (c, l) in &m.links {
        if schema.resolve_rel(&l.rel).is_none() {
            return Err(Error::invalid(format!("unknown relation {} (column {c:?})", l.rel)));
        }
        if schema.object_type(&l.target_kind).is_none() {
            return Err(Error::invalid(format!("unknown object type {} (column {c:?})", l.target_kind)));
        }
        links.push((col("link", c)?, l.rel.clone(), l.target_kind.clone()));
    }
    Ok(Resolved { key, title, state, props, links })
}

fn shown(v: &str) -> String {
    if v.chars().count() > 200 { format!("{}…", v.chars().take(200).collect::<String>()) } else { v.to_string() }
}

/// The mutations that write a sheet's rows: one upsert per row, then one
/// link per link cell (so a row may name a row further down). `can_read`
/// decides which existing objects count as link targets and as already
/// present (required properties are only demanded of new objects).
pub fn rows_to_mutations(
    schema: &Schema,
    g: &Graph,
    mapping: &SheetMapping,
    csv: &str,
    can_read: &dyn Fn(&Entity) -> bool,
) -> Result<(Vec<Mutation>, Vec<Finding>)> {
    let sheet = parse_csv(csv)?;
    sheet_mutations(schema, g, mapping, &sheet, can_read)
}

/// [`rows_to_mutations`] over a parsed sheet.
pub fn sheet_mutations(
    schema: &Schema,
    g: &Graph,
    mapping: &SheetMapping,
    sheet: &Sheet,
    can_read: &dyn Fn(&Entity) -> bool,
) -> Result<(Vec<Mutation>, Vec<Finding>)> {
    let r = resolve(schema, sheet, mapping)?;
    let kind = &mapping.kind;
    let states = schema.object_type(kind).map(|t| t.states.clone()).unwrap_or_default();
    let required: Vec<&String> =
        schema.all_props(kind).into_iter().filter(|(_, d)| d.required).map(|(name, _)| name).collect();
    let exists = |id: &str| g.get(id).is_some_and(can_read);
    let mut findings = Vec::new();
    let mut upserts = Vec::new();
    let mut ids: Vec<Option<String>> = Vec::with_capacity(sheet.rows.len());
    let mut seen: HashMap<String, usize> = HashMap::new();
    for row in 0..sheet.rows.len() {
        let line = sheet.lines[row];
        let find = |column: Option<usize>, value: Option<&str>, issue, message: String| Finding {
            row: row + 1,
            line,
            column: column.map(|c| sheet.header[c].clone()),
            value: value.map(shown),
            issue,
            message,
        };
        let key = match r.key {
            Some(kc) => match sheet.cell(row, kc) {
                Some(k) => k.to_string(),
                None => {
                    findings.push(find(Some(kc), None, FindingKind::MissingKey, "no key: row left out".into()));
                    ids.push(None);
                    continue;
                }
            },
            None => (row + 1).to_string(),
        };
        let id = format!("{kind}:{key}");
        if let Err(e) = spell_entity(&id).and_then(|_| if id.len() > 256 { Err("too long".into()) } else { Ok(()) }) {
            findings.push(find(
                r.key,
                Some(&key),
                FindingKind::BadKey,
                format!("key cannot be part of an id ({e}): row left out"),
            ));
            ids.push(None);
            continue;
        }
        if let Some(first) = seen.get(&id) {
            findings.push(find(
                r.key,
                Some(&key),
                FindingKind::DuplicateKey,
                format!("row {first} has the same key: both rows write {id}"),
            ));
        } else {
            seen.insert(id.clone(), row + 1);
        }
        let mut props = Map::new();
        for (c, p, def) in &r.props {
            let Some(cell) = sheet.cell(row, *c) else { continue };
            match read_value(def, cell) {
                Ok(v) => {
                    props.insert(p.clone(), v);
                }
                Err(why) => findings.push(find(
                    Some(*c),
                    Some(cell),
                    FindingKind::BadValue,
                    format!("{kind}.{p}: {why}: value left out"),
                )),
            }
        }
        let new = !exists(&id) && seen.get(&id) == Some(&(row + 1));
        if new && let Some(missing) = required.iter().find(|p| !props.contains_key(p.as_str())) {
            let c = r.props.iter().find(|(_, p, _)| p == *missing).map(|(c, _, _)| *c);
            findings.push(find(
                c,
                c.and_then(|c| sheet.cell(row, c)),
                FindingKind::MissingRequired,
                format!("{kind}.{missing} is required and this row has no readable value: row left out"),
            ));
            ids.push(None);
            seen.remove(&id);
            continue;
        }
        let state = match r.state.and_then(|c| sheet.cell(row, c).map(|v| (c, v))) {
            Some((_, v)) if states.iter().any(|s| s == v) => Some(v.to_string()),
            Some((c, v)) => {
                findings.push(find(
                    Some(c),
                    Some(v),
                    FindingKind::BadState,
                    format!("{kind} states are {}: state left out", states.join(", ")),
                ));
                None
            }
            None => None,
        };
        upserts.push(Mutation::Upsert(EntityInput {
            kind: kind.clone(),
            key: Some(key),
            title: r.title.and_then(|c| sheet.cell(row, c)).map(str::to_string),
            state,
            props,
            provenance: Some(Provenance { source: "sheet".into(), ..Default::default() }),
            ..Default::default()
        }));
        ids.push(Some(id));
    }
    let written: BTreeSet<&str> = ids.iter().flatten().map(String::as_str).collect();
    let mut links = Vec::new();
    for (row, id) in ids.iter().enumerate() {
        let Some(id) = id else { continue };
        for (c, rel, target_kind) in &r.links {
            let Some(v) = sheet.cell(row, *c) else { continue };
            let find = |issue, message: String| Finding {
                row: row + 1,
                line: sheet.lines[row],
                column: Some(sheet.header[*c].clone()),
                value: Some(shown(v)),
                issue,
                message,
            };
            let to = format!("{target_kind}:{v}");
            if to == *id {
                findings.push(find(FindingKind::SelfLink, format!("{id} cannot {rel} itself: link left out")));
            } else if !(exists(&to) || (target_kind == kind && written.contains(to.as_str()))) {
                findings.push(find(FindingKind::MissingTarget, format!("no object {to}: {rel} link left out")));
            } else {
                links.push(Mutation::Link(LinkInput::new(id.clone(), rel.clone(), to)));
            }
        }
    }
    findings.sort_by(|a, b| (a.row, a.issue, &a.column).cmp(&(b.row, b.issue, &b.column)));
    upserts.extend(links);
    Ok((upserts, findings))
}

// ── engine ───────────────────────────────────────────────────────────────

/// What an ingest wrote (or, in a dry run, would write).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SheetIngest {
    pub kind: String,
    /// Data rows read.
    pub rows: usize,
    /// Objects upserted (rows not left out).
    pub upserts: usize,
    /// Links written.
    pub links: usize,
    /// What the lexical guard left out (at most [`MAX_FINDINGS`]).
    pub findings: Vec<Finding>,
    /// All findings, including those not listed.
    pub findings_total: usize,
    pub dry_run: bool,
    /// The graph revision after the write (absent in a dry run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    pub means: String,
}

impl Ontology {
    /// Induce the ontology a sheet describes, over what `p` can read
    /// (and, when `opts.workspace` is set, only that workspace's objects).
    pub fn induce_sheet(&self, p: &Principal, csv: &str, opts: &InduceOptions) -> Result<Induced> {
        let sheet = parse_csv(csv)?;
        let g = self.reader(p)?;
        let can = |e: &Entity| policy::visible(p, e, &g);
        induce_parsed(&self.schema(), &g, &sheet, opts, &can)
    }

    /// Write a sheet's rows into `ctx.workspace` with `mapping`, in one
    /// atomic apply (with an `ingest.sheet` audit event). Link targets must
    /// be objects of that workspace the principal can read. A dry run
    /// validates the mutations without writing anything.
    pub fn ingest_sheet(
        &self,
        ctx: &ActionContext,
        csv: &str,
        mapping: &SheetMapping,
        dry_run: bool,
    ) -> Result<SheetIngest> {
        let p = &ctx.principal;
        if !p.allows(Access::Write) {
            let reason = format!("{} may not write objects", p.id);
            let mut ctx = ctx.clone();
            ctx.intent.get_or_insert_with(|| "ingest.sheet".into());
            self.audit_denied(&ctx, &reason, now_ms());
            return Err(Error::Denied(reason));
        }
        let sheet = parse_csv(csv)?;
        let ws = ctx.workspace.as_str();
        let counts = |muts: &[Mutation]| {
            let upserts = muts.iter().filter(|m| matches!(m, Mutation::Upsert(_))).count();
            (upserts, muts.len() - upserts)
        };
        let report = |(upserts, links): (usize, usize), findings: Vec<Finding>, revision| {
            let findings_total = findings.len();
            SheetIngest {
                kind: mapping.kind.clone(),
                rows: sheet.rows.len(),
                upserts,
                links,
                findings: findings.into_iter().take(MAX_FINDINGS).collect(),
                findings_total,
                dry_run,
                revision,
                means: INGEST_MEANS.into(),
            }
        };
        if dry_run {
            let g = self.reader(p)?;
            let schema = self.schema();
            let can = |e: &Entity| e.workspace == ws && policy::visible(p, e, &g);
            let (muts, findings) = sheet_mutations(&schema, &g, mapping, &sheet, &can)?;
            let n = counts(&muts);
            self.stage_only(&g, &schema, ctx, muts)?;
            return Ok(report(n, findings, None));
        }
        let (res, (n, findings)) = self.transact(ctx, |o, g| {
            let can = |e: &Entity| e.workspace == ws && policy::visible(p, e, g);
            let (muts, findings) = sheet_mutations(&o.schema(), g, mapping, &sheet, &can)?;
            let (upserts, links) = counts(&muts);
            let ev = o.event(
                ctx,
                ws,
                "ingest.sheet",
                None,
                now_ms(),
                json!({
                    "kind": mapping.kind,
                    "rows": sheet.rows.len(),
                    "upserts": upserts,
                    "links": links,
                    "findings": findings.len(),
                }),
                Vec::new(),
            );
            Ok((muts, Extra { events: vec![ev], ..Default::default() }, ((upserts, links), findings)))
        })?;
        Ok(report(n, findings, Some(res.revision)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_quotes_newlines_and_crlf() {
        let s = parse_csv("\u{feff}id,note\r\n1,\"a, \"\"quoted\"\"\r\nline\"\r\n2,plain\r\n").unwrap();
        assert_eq!(s.header, vec!["id", "note"]);
        assert_eq!(
            s.rows,
            vec![vec!["1".to_string(), "a, \"quoted\"\r\nline".into()], vec!["2".into(), "plain".into()]]
        );
        assert_eq!(s.lines, vec![2, 4]);
    }

    #[test]
    fn csv_delimiters_are_detected() {
        for (d, text) in [(';', "a;b\n1;2\n"), ('\t', "a\tb\n1\t2\n"), ('|', "a|b\n1|2\n"), (',', "a,b\n1,2")] {
            let s = parse_csv(text).unwrap();
            assert_eq!(s.delimiter, d);
            assert_eq!(s.rows, vec![vec!["1".to_string(), "2".into()]]);
        }
        // A delimiter inside a quoted header name does not count.
        assert_eq!(parse_csv("\"a;b\",c\n1,2\n").unwrap().header, vec!["a;b", "c"]);
    }

    #[test]
    fn csv_ragged_rows_and_errors() {
        let s = parse_csv("a,b,c\n1\n\n2,3,4,,\n").unwrap();
        assert_eq!(s.rows, vec![vec!["1".to_string(), "".into(), "".into()], vec!["2".into(), "3".into(), "4".into()]]);
        assert!(parse_csv("a,b\n1,2,3\n").is_err());
        assert!(parse_csv("a,b\n\"open,2\n").unwrap_err().to_string().contains("never closed"));
        assert!(parse_csv("a,b\n\"x\"y,2\n").unwrap_err().to_string().contains("after a closing quote"));
        assert!(parse_csv("").is_err());
        assert!(parse_csv("a,a\n1,2\n").is_err());
        assert_eq!(parse_csv("a,,b\n1,2,3\n").unwrap().header, vec!["a", "column_2", "b"]);
        // A quote inside an unquoted field is kept as written.
        assert_eq!(parse_csv("a\n5\" pipe\n").unwrap().rows[0][0], "5\" pipe");
        // An empty quoted field on its own line is a row, not a blank line.
        assert_eq!(parse_csv("a\n\"\"\n").unwrap().rows.len(), 1);
    }

    #[test]
    fn lexical_forms() {
        assert_eq!(lex_int("42"), Some(42));
        assert_eq!(lex_int("-7"), Some(-7));
        for bad in ["007", "+1", "1.0", "", "-", "1e3", "99999999999999999999"] {
            assert_eq!(lex_int(bad), None, "{bad}");
        }
        assert_eq!(lex_float("1.5"), Some(1.5));
        assert_eq!(lex_float("-2"), Some(-2.0));
        assert_eq!(lex_float("3e8"), Some(3e8));
        for bad in [".5", "1.", "NaN", "inf", "1,5", "01.5", "1e"] {
            assert_eq!(lex_float(bad), None, "{bad}");
        }
        assert_eq!(lex_bool("YES"), Some(true));
        assert_eq!(lex_bool("False"), Some(false));
        assert_eq!(lex_bool("1"), None);
        for ok in ["2026-10-24", "2026-10-24T12:00:00Z", "2026-10-24T12:00:00.250+02:00", "2026-10-24 08:30:00z"] {
            assert!(lex_timestamp(ok), "{ok}");
        }
        for bad in ["2026-02-30", "2026-10-24T12:00", "2026-10-24T12:00:00", "24/10/2026", "2026-10-24T12:00:00+0200"] {
            assert!(!lex_timestamp(bad), "{bad}");
        }
    }

    #[test]
    fn read_value_never_coerces() {
        let def = |ty| PropertyDef { ty, ..Default::default() };
        assert_eq!(read_value(&def(PropType::Int), "12"), Ok(Value::from(12)));
        assert!(read_value(&def(PropType::Int), "12.5").is_err());
        assert!(read_value(&def(PropType::Float), "abc").is_err());
        assert_eq!(read_value(&def(PropType::Bool), "no"), Ok(Value::Bool(false)));
        let e = PropertyDef { ty: PropType::Enum, values: vec!["a".into()], ..Default::default() };
        assert!(read_value(&e, "b").unwrap_err().contains("one of a"));
        assert!(read_value(&def(PropType::List), "x").is_err());
    }

    #[test]
    fn names() {
        assert_eq!(ref_stem("CustomerID"), ("customer".into(), true));
        assert_eq!(ref_stem("owner"), ("owner".into(), false));
        assert!(key_like("OrderNumber") && key_like("id") && !key_like("paid"));
        assert_eq!(file_stem("/data/Orders.csv"), "Orders");
        assert_eq!(file_stem("people"), "people");
    }
}
