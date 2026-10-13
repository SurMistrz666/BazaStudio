//! BSON values as the grid holds them, and back.
//!
//! A cell has no room for a BSON type, so the mapping is chosen to lose
//! nothing on the way back:
//!
//! | BSON | cell |
//! | --- | --- |
//! | string | `Text` |
//! | int32, int64 | `Integer` (which of the two is read from the document when it is changed) |
//! | double | `Float`, always with a fraction or an exponent (`1.0`) |
//! | decimal128 | `Decimal`, every digit |
//! | bool, null, a missing field | `Bool`, `Null`, `Null` |
//! | binary, generic subtype | `Bytes` |
//! | ObjectId | `Text`: its 24 hexadecimal digits |
//! | date | `Text`: `2026-05-01T10:00:00Z`, with milliseconds when it has any |
//! | everything else (document, array, UUID, timestamp, …) | `Json`: compact extended JSON |
//!
//! An ObjectId and a date are shown as plain text because that is what a
//! person reads and what a CSV file should hold; a narrow column of
//! `{"$oid":"…"}` shows nothing. The text does not say its type, so it is
//! never trusted to: a change reads the document and keeps the type the
//! field has ([`edited_value`]), and a key is looked up as every value the
//! text may stand for ([`key_values`]).
//!
//! The extended JSON written here is the relaxed form with one difference:
//! an int64 is written as `{"$numberLong":"…"}`, so that a nested number
//! keeps its type when the text is read back. It is strict JSON, which is
//! what the editor of a `Json` cell accepts.
use crate::model::{Cell, ColumnInfo, ResultColumn};
use ::mongodb::bson::spec::BinarySubtype;
use ::mongodb::bson::{Binary, Bson, DateTime, Decimal128, Document};
use anyhow::{Result, anyhow, bail};
use base64::Engine as _;
use std::fmt::Write as _;

/// Name of the BSON type of a value: the aliases of `$type`, except that the
/// two integers are called `int32` and `int64`.
pub(super) fn type_name(value: &Bson) -> &'static str {
    match value {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Array(_) => "array",
        Bson::Document(_) => "object",
        Bson::Boolean(_) => "bool",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) => "javascript",
        Bson::JavaScriptCodeWithScope(_) => "javascriptWithScope",
        Bson::Int32(_) => "int32",
        Bson::Int64(_) => "int64",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::DateTime(_) => "date",
        Bson::Symbol(_) => "symbol",
        Bson::Decimal128(_) => "decimal128",
        Bson::Undefined => "undefined",
        Bson::MaxKey => "maxKey",
        Bson::MinKey => "minKey",
        Bson::DbPointer(_) => "dbPointer",
    }
}

/// The alias `$type` knows a type by.
pub(super) fn type_alias(value: &Bson) -> &'static str {
    match value {
        Bson::Int32(_) => "int",
        Bson::Int64(_) => "long",
        Bson::Decimal128(_) => "decimal",
        other => type_name(other),
    }
}

/// A double as text that reads back as the same double and never looks like
/// an integer: `1.0`, `0.1`, `1e300`, `NaN`, `Infinity`.
pub(super) fn double_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".into()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else {
        format!("{value:?}")
    }
}

/// Read a double the way [`double_text`] and the editor of a cell write it.
pub(super) fn parse_double(text: &str) -> Option<f64> {
    match text.trim() {
        "NaN" => Some(f64::NAN),
        "Infinity" | "+Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        other => other.parse::<f64>().ok().filter(|_| {
            other
                .bytes()
                .all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b))
        }),
    }
}

/// Milliseconds of the dates written as text: the years 1 to 9999.
const TEXT_DATES: std::ops::RangeInclusive<i64> = -62_135_596_800_000..=253_402_300_799_999;

/// A date as ISO-8601 in UTC, with milliseconds only when it has any.
pub(super) fn date_text(millis: i64) -> Option<String> {
    if !TEXT_DATES.contains(&millis) {
        return None;
    }
    let time = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(millis)?;
    Some(if millis.rem_euclid(1000) == 0 {
        time.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    } else {
        time.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
    })
}

/// Milliseconds of a text that is a date exactly as [`date_text`] writes it.
fn shown_date(text: &str) -> Option<i64> {
    let millis = chrono::DateTime::parse_from_rfc3339(text)
        .ok()?
        .timestamp_millis();
    (date_text(millis).as_deref() == Some(text)).then_some(millis)
}

/// Milliseconds of a date written as `2026-05-01`, `2026-05-01T10:20:30`,
/// with a fraction of a second, and with `Z` or an offset. A date without a
/// zone is UTC.
pub(super) fn parse_date(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Ok(time) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(time.timestamp_millis());
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(time) = chrono::NaiveDateTime::parse_from_str(text, format) {
            return Some(time.and_utc().timestamp_millis());
        }
    }
    let day = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()?;
    Some(day.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis())
}

/// The canonical text of a UUID for the sixteen bytes of subtype 4.
fn uuid_text(binary: &Binary) -> Option<String> {
    (binary.subtype == BinarySubtype::Uuid)
        .then(|| uuid::Uuid::from_slice(&binary.bytes).ok())
        .flatten()
        .map(|id| id.hyphenated().to_string())
}

fn json_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A deprecated type this driver can show but cannot write back.
fn unwritable(value: &Bson) -> bool {
    match value {
        Bson::DbPointer(_) => true,
        Bson::Array(items) => items.iter().any(unwritable),
        Bson::Document(document) => document.values().any(unwritable),
        Bson::JavaScriptCodeWithScope(code) => code.scope.values().any(unwritable),
        _ => false,
    }
}

fn write_ext_json(out: &mut String, value: &Bson) {
    match value {
        Bson::Double(number) if number.is_finite() => out.push_str(&double_text(*number)),
        Bson::Double(number) => {
            let _ = write!(out, "{{\"$numberDouble\":\"{}\"}}", double_text(*number));
        }
        Bson::String(text) => json_string(out, text),
        Bson::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_ext_json(out, item);
            }
            out.push(']');
        }
        Bson::Document(document) => write_ext_document(out, document),
        Bson::Boolean(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Bson::Null => out.push_str("null"),
        Bson::RegularExpression(regex) => {
            out.push_str("{\"$regularExpression\":{\"pattern\":");
            json_string(out, regex.pattern.as_str());
            out.push_str(",\"options\":");
            json_string(out, regex.options.as_str());
            out.push_str("}}");
        }
        Bson::JavaScriptCode(code) => {
            out.push_str("{\"$code\":");
            json_string(out, code);
            out.push('}');
        }
        Bson::JavaScriptCodeWithScope(code) => {
            out.push_str("{\"$code\":");
            json_string(out, &code.code);
            out.push_str(",\"$scope\":");
            write_ext_document(out, &code.scope);
            out.push('}');
        }
        Bson::Int32(number) => {
            let _ = write!(out, "{number}");
        }
        Bson::Int64(number) => {
            let _ = write!(out, "{{\"$numberLong\":\"{number}\"}}");
        }
        Bson::Timestamp(stamp) => {
            let _ = write!(
                out,
                "{{\"$timestamp\":{{\"t\":{},\"i\":{}}}}}",
                stamp.time, stamp.increment
            );
        }
        Bson::Binary(binary) => match uuid_text(binary) {
            Some(id) => {
                let _ = write!(out, "{{\"$uuid\":\"{id}\"}}");
            }
            None => {
                let _ = write!(
                    out,
                    "{{\"$binary\":{{\"base64\":\"{}\",\"subType\":\"{:02x}\"}}}}",
                    base64::engine::general_purpose::STANDARD.encode(&binary.bytes),
                    u8::from(binary.subtype)
                );
            }
        },
        Bson::ObjectId(id) => {
            let _ = write!(out, "{{\"$oid\":\"{}\"}}", id.to_hex());
        }
        // Extended JSON writes a date before 1970 as a number.
        Bson::DateTime(date) => {
            match date_text(date.timestamp_millis()).filter(|_| date.timestamp_millis() >= 0) {
                Some(text) => {
                    let _ = write!(out, "{{\"$date\":\"{text}\"}}");
                }
                None => {
                    let _ = write!(
                        out,
                        "{{\"$date\":{{\"$numberLong\":\"{}\"}}}}",
                        date.timestamp_millis()
                    );
                }
            }
        }
        Bson::Symbol(symbol) => {
            out.push_str("{\"$symbol\":");
            json_string(out, symbol);
            out.push('}');
        }
        Bson::Decimal128(number) => {
            let _ = write!(out, "{{\"$numberDecimal\":\"{number}\"}}");
        }
        Bson::Undefined => out.push_str("{\"$undefined\":true}"),
        Bson::MaxKey => out.push_str("{\"$maxKey\":1}"),
        Bson::MinKey => out.push_str("{\"$minKey\":1}"),
        // Shown, never written back: see `unwritable`.
        Bson::DbPointer(_) => out.push_str("{\"$dbPointer\":{}}"),
    }
}

fn write_ext_document(out: &mut String, document: &Document) {
    out.push('{');
    for (index, (key, value)) in document.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        json_string(out, key);
        out.push(':');
        write_ext_json(out, value);
    }
    out.push('}');
}

/// A value as compact extended JSON that reads back as the same value.
pub(super) fn ext_json(value: &Bson) -> String {
    let mut out = String::new();
    write_ext_json(&mut out, value);
    out
}

/// A name that can stand in a command without quotes.
pub(super) fn plain_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn write_shell(out: &mut String, value: &Bson) {
    match value {
        Bson::Double(number) => out.push_str(&double_text(*number)),
        Bson::String(text) => json_string(out, text),
        Bson::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_shell(out, item);
            }
            out.push(']');
        }
        Bson::Document(document) => {
            out.push('{');
            for (index, (key, item)) in document.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                if plain_name(key) {
                    out.push_str(key);
                } else {
                    json_string(out, key);
                }
                out.push_str(": ");
                write_shell(out, item);
            }
            out.push('}');
        }
        Bson::Boolean(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Bson::Null => out.push_str("null"),
        Bson::RegularExpression(regex) => {
            out.push_str("RegExp(");
            json_string(out, regex.pattern.as_str());
            if !regex.options.as_str().is_empty() {
                out.push_str(", ");
                json_string(out, regex.options.as_str());
            }
            out.push(')');
        }
        Bson::Int32(number) => {
            let _ = write!(out, "{number}");
        }
        Bson::Int64(number) => {
            let _ = write!(out, "NumberLong(\"{number}\")");
        }
        Bson::Timestamp(stamp) => {
            let _ = write!(out, "Timestamp({}, {})", stamp.time, stamp.increment);
        }
        Bson::Binary(binary) => match uuid_text(binary) {
            Some(id) => {
                let _ = write!(out, "UUID(\"{id}\")");
            }
            None => {
                let _ = write!(
                    out,
                    "BinData({}, \"{}\")",
                    u8::from(binary.subtype),
                    base64::engine::general_purpose::STANDARD.encode(&binary.bytes)
                );
            }
        },
        Bson::ObjectId(id) => {
            let _ = write!(out, "ObjectId(\"{}\")", id.to_hex());
        }
        Bson::DateTime(date) => match date_text(date.timestamp_millis()) {
            Some(text) => {
                let _ = write!(out, "ISODate(\"{text}\")");
            }
            None => {
                let _ = write!(out, "new Date({})", date.timestamp_millis());
            }
        },
        Bson::Decimal128(number) => {
            let _ = write!(out, "NumberDecimal(\"{number}\")");
        }
        Bson::MaxKey => out.push_str("MaxKey()"),
        Bson::MinKey => out.push_str("MinKey()"),
        Bson::Undefined => out.push_str("undefined"),
        // The rest has no notation of its own in the shell: the extended
        // JSON form, which the console reads as well.
        other => write_ext_json(out, other),
    }
}

/// A value in the notation of the shell, as the console reads it:
/// `{_id: ObjectId("…"), total: NumberLong("5")}`.
pub(super) fn shell(value: &Bson) -> String {
    let mut out = String::new();
    write_shell(&mut out, value);
    out
}

/// The cell of a top-level field. A field the document does not have is
/// shown like a null.
pub(super) fn cell(value: Option<&Bson>) -> Cell {
    match value {
        None | Some(Bson::Null) => Cell::Null,
        Some(Bson::Boolean(flag)) => Cell::Bool(*flag),
        Some(Bson::Int32(number)) => Cell::Integer(number.to_string()),
        Some(Bson::Int64(number)) => Cell::Integer(number.to_string()),
        Some(Bson::Double(number)) => Cell::Float(double_text(*number)),
        Some(Bson::Decimal128(number)) => Cell::Decimal(number.to_string()),
        Some(Bson::String(text)) => Cell::Text(text.clone()),
        Some(Bson::ObjectId(id)) => Cell::Text(id.to_hex()),
        Some(Bson::DateTime(date)) => match date_text(date.timestamp_millis()) {
            Some(text) => Cell::Text(text),
            None => Cell::Json(ext_json(&Bson::DateTime(*date))),
        },
        Some(Bson::Binary(binary)) if binary.subtype == BinarySubtype::Generic => {
            Cell::Bytes(binary.bytes.clone())
        }
        Some(other) if unwritable(other) => Cell::Unsupported(format!("<{}>", type_name(other))),
        Some(other) => Cell::Json(ext_json(other)),
    }
}

fn integer(text: &str) -> Result<Bson> {
    let number: i64 = text
        .trim()
        .parse()
        .map_err(|_| anyhow!("integer_invalid"))?;
    Ok(match i32::try_from(number) {
        Ok(small) => Bson::Int32(small),
        Err(_) => Bson::Int64(number),
    })
}

fn decimal(text: &str) -> Result<Bson> {
    text.trim()
        .parse::<Decimal128>()
        .map(Bson::Decimal128)
        .map_err(|_| anyhow!("decimal_invalid"))
}

fn double(text: &str) -> Result<Bson> {
    parse_double(text)
        .map(Bson::Double)
        .ok_or_else(|| anyhow!("float_invalid"))
}

/// A value written as extended JSON or in the notation of the shell.
pub(super) fn parse_json(text: &str) -> Result<Bson> {
    super::shell::parse_value(text).map_err(|_| anyhow!("json_invalid"))
}

/// Every value an `_id` shown as this cell may be. An ObjectId and a date
/// are shown as text, so a text may be either of them or the text itself;
/// the document that is found says which.
pub(super) fn key_values(cell: &Cell) -> Result<Vec<Bson>> {
    let mut values = vec![exact(cell)?];
    if let Cell::Text(text) = cell {
        if text.len() == 24
            && let Ok(id) = ::mongodb::bson::oid::ObjectId::parse_str(text)
        {
            values.push(Bson::ObjectId(id));
        }
        if let Some(millis) = shown_date(text) {
            values.push(Bson::DateTime(DateTime::from_millis(millis)));
        }
    }
    Ok(values)
}

/// The value a cell stands for by its kind alone: a filter value, the old
/// value in the text of a change. An integer is the smallest type that
/// holds it, which the server compares by value; a text is a text.
pub(super) fn exact(cell: &Cell) -> Result<Bson> {
    Ok(match cell {
        Cell::Null => Bson::Null,
        Cell::Bool(flag) => Bson::Boolean(*flag),
        Cell::Integer(text) => integer(text)?,
        Cell::Decimal(text) => decimal(text)?,
        Cell::Float(text) => double(text)?,
        Cell::Text(text) => Bson::String(text.clone()),
        Cell::Json(text) => parse_json(text)?,
        Cell::Bytes(bytes) => Bson::Binary(Binary {
            subtype: BinarySubtype::Generic,
            bytes: bytes.clone(),
        }),
        Cell::Unsupported(_) => bail!("cell_read_only"),
    })
}

/// What is known about the type of a field from the documents that were
/// looked at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shape {
    /// Every value seen has this type.
    Uniform(&'static str),
    /// Several types, only nulls, or a field nobody has seen.
    Open,
}

/// Where a typed text comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Source {
    /// Typed by a person into a field of the grid: a text that reads as a
    /// value (`42`, `true`, `{"a":1}`, `ObjectId("…")`) is that value.
    Typed,
    /// A cell of an imported file: a text is a text unless the field has
    /// one type everywhere.
    Imported,
}

/// A text as the value of a field whose values all have one type: the text
/// takes that type, like a literal takes the type of its column in SQL.
fn text_as(kind: &'static str, text: &str) -> Result<Bson> {
    let plain = match kind {
        "string" => return Ok(Bson::String(text.to_owned())),
        "int32" => integer(text),
        "int64" => text
            .trim()
            .parse::<i64>()
            .map(Bson::Int64)
            .map_err(|_| anyhow!("integer_invalid")),
        "double" => double(text),
        "decimal128" => decimal(text),
        "bool" => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => Ok(Bson::Boolean(true)),
            "false" | "0" => Ok(Bson::Boolean(false)),
            _ => Err(anyhow!("boolean_invalid")),
        },
        "date" => parse_date(text)
            .map(|millis| Bson::DateTime(DateTime::from_millis(millis)))
            .ok_or_else(|| anyhow!("mongodb_date_invalid")),
        "objectId" => ::mongodb::bson::oid::ObjectId::parse_str(text.trim())
            .map(Bson::ObjectId)
            .map_err(|_| anyhow!("mongodb_objectid_invalid")),
        _ => parse_json(text),
    };
    // The explicit form always works: `{"$date":"…"}` in a date field,
    // `NumberLong(5)` in a number field. It is what an export writes.
    plain.or_else(|error| {
        let explicit = text.trim_start();
        if explicit.starts_with('{') || explicit.ends_with(')') {
            parse_json(text).map_err(|_| error)
        } else {
            Err(error)
        }
    })
}

/// A text that stands alone as a value: a number, `true`, `false`, `null`,
/// a quoted text, a document, an array or a constructor of the shell.
fn reads_as_value(text: &str) -> bool {
    let text = text.trim();
    let Some(first) = text.chars().next() else {
        return false;
    };
    match first {
        '{' | '[' | '"' => true,
        '-' | '0'..='9' => {
            // A number as JSON writes it: `007` and `+48 123` stay texts.
            let digits = text.strip_prefix('-').unwrap_or(text);
            let whole = digits.split(['.', 'e', 'E']).next().unwrap_or_default();
            !whole.is_empty()
                && (whole == "0" || !whole.starts_with('0'))
                && digits
                    .bytes()
                    .all(|b| b.is_ascii_digit() || b".eE+-".contains(&b))
        }
        _ => {
            matches!(text, "true" | "false" | "null")
                || (text.ends_with(')')
                    && text.split('(').next().is_some_and(|name| {
                        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric())
                    }))
        }
    }
}

/// The value a cell of a new or changed field stands for.
///
/// A cell made from a typed text (`Integer`, `Decimal`, `Float`, `Text`)
/// follows the type the field has in the collection; `Json`, `Bool`, `Null`
/// and `Bytes` say their type themselves.
pub(super) fn field_value(cell: &Cell, shape: Shape, source: Source) -> Result<Bson> {
    let text = match cell {
        Cell::Null | Cell::Bool(_) | Cell::Json(_) | Cell::Bytes(_) | Cell::Unsupported(_) => {
            return exact(cell);
        }
        Cell::Integer(text) | Cell::Decimal(text) | Cell::Float(text) | Cell::Text(text) => text,
    };
    match (shape, cell, source) {
        // A decimal or a double from a typed file keeps its type.
        (_, Cell::Decimal(_), Source::Imported) => decimal(text),
        (_, Cell::Float(_), Source::Imported) => double(text),
        (Shape::Uniform(kind), _, _) => text_as(kind, text),
        (Shape::Open, Cell::Integer(_), Source::Imported) => integer(text),
        (Shape::Open, _, Source::Imported) => Ok(Bson::String(text.clone())),
        (Shape::Open, _, Source::Typed) => {
            if reads_as_value(text)
                && let Ok(value) = super::shell::parse_value(text)
            {
                return Ok(value);
            }
            Ok(Bson::String(text.clone()))
        }
    }
}

/// The new value of a field that holds `current`, for a cell the editor
/// made from the old one: a number stays a number of its type, a text a
/// text, and a `Json` cell says its type itself.
pub(super) fn edited_value(new: &Cell, current: Option<&Bson>, shape: Shape) -> Result<Bson> {
    Ok(match (new, current) {
        (Cell::Null | Cell::Bool(_) | Cell::Json(_) | Cell::Bytes(_) | Cell::Unsupported(_), _) => {
            exact(new)?
        }
        (Cell::Integer(text), Some(Bson::Int64(_))) => Bson::Int64(
            text.trim()
                .parse()
                .map_err(|_| anyhow!("integer_invalid"))?,
        ),
        (Cell::Integer(text), Some(Bson::Int32(_))) => integer(text)?,
        (Cell::Float(text), Some(Bson::Double(_))) => double(text)?,
        (Cell::Decimal(text), Some(Bson::Decimal128(_))) => decimal(text)?,
        (Cell::Text(text), Some(Bson::String(_))) => Bson::String(text.clone()),
        // Shown as text, stored as what they are.
        (Cell::Text(text), Some(Bson::ObjectId(_))) => text_as("objectId", text)?,
        (Cell::Text(text), Some(Bson::DateTime(_))) => text_as("date", text)?,
        // The field holds a null or nothing: its type comes from the rest
        // of the collection, and from the text itself when that says nothing.
        _ => field_value(new, shape, Source::Typed)?,
    })
}

/// The fields of a set of documents, in the order they were first seen with
/// `_id` first, and the types each of them holds.
#[derive(Clone, Default)]
pub(super) struct Fields {
    names: Vec<String>,
    index: std::collections::HashMap<String, usize>,
    types: Vec<Vec<&'static str>>,
    /// Documents that have the field.
    present: Vec<usize>,
    /// Documents in which the field is null.
    nulls: Vec<usize>,
    documents: usize,
}

/// More distinct top-level fields than this are not laid out as columns:
/// the documents are shown whole, in one column, and are not edited.
pub(super) const MAX_FIELDS: usize = 256;

/// Name of the column that holds whole documents of a collection with more
/// than [`MAX_FIELDS`] distinct fields.
pub(super) const WHOLE: &str = "document";

impl Fields {
    pub(super) fn observe(&mut self, document: &Document) {
        self.documents += 1;
        for (name, value) in document {
            let slot = match self.index.get(name) {
                Some(slot) => *slot,
                None => {
                    let slot = self.names.len();
                    self.index.insert(name.clone(), slot);
                    self.names.push(name.clone());
                    self.types.push(Vec::new());
                    self.present.push(0);
                    self.nulls.push(0);
                    slot
                }
            };
            self.present[slot] += 1;
            let kind = type_name(value);
            if kind == "null" {
                self.nulls[slot] += 1;
            } else if !self.types[slot].contains(&kind) {
                self.types[slot].push(kind);
            }
        }
    }

    /// Whether a field of this name was seen.
    pub(super) fn knows(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }

    /// Names of the fields, in the order they were first seen.
    pub(super) fn names(&self) -> &[String] {
        &self.names
    }

    /// Keep a field of `other` that no document seen here has: it stays a
    /// column, with the types it had there.
    pub(super) fn adopt(&mut self, other: &Fields, name: &str) {
        if self.knows(name) {
            return;
        }
        let Some(slot) = other.index.get(name) else {
            return;
        };
        self.index.insert(name.to_owned(), self.names.len());
        self.names.push(name.to_owned());
        self.types.push(other.types[*slot].clone());
        self.present.push(0);
        self.nulls.push(0);
    }

    /// Too many distinct fields for columns.
    pub(super) fn wide(&self) -> bool {
        self.names.len() > MAX_FIELDS
    }

    /// Slots in the order of the columns: `_id` first.
    fn order(&self) -> Vec<usize> {
        let id = self.index.get("_id").copied();
        id.into_iter()
            .chain((0..self.names.len()).filter(|slot| Some(*slot) != id))
            .collect()
    }

    /// The type of a column for people: one name, or `mixed: a, b`.
    fn label(&self, slot: usize) -> String {
        match self.types[slot].as_slice() {
            [] => "null".into(),
            [one] => (*one).into(),
            several => format!("mixed: {}", several.join(", ")),
        }
    }

    pub(super) fn shape(&self, name: &str) -> Shape {
        match self
            .index
            .get(name)
            .map(|slot| self.types[*slot].as_slice())
        {
            Some([one]) => Shape::Uniform(one),
            _ => Shape::Open,
        }
    }

    /// Columns of a result made of these documents.
    pub(super) fn result_columns(&self) -> Vec<ResultColumn> {
        if self.wide() {
            let mut columns = Vec::new();
            if let Some(slot) = self.index.get("_id") {
                columns.push(ResultColumn {
                    name: "_id".into(),
                    data_type: self.label(*slot),
                });
            }
            columns.push(ResultColumn {
                name: WHOLE.into(),
                data_type: "object".into(),
            });
            return columns;
        }
        self.order()
            .into_iter()
            .map(|slot| ResultColumn {
                name: self.names[slot].clone(),
                data_type: self.label(slot),
            })
            .collect()
    }

    /// The cells of one document under [`Fields::result_columns`].
    pub(super) fn row(&self, document: &Document) -> Vec<Cell> {
        if self.wide() {
            let mut cells = Vec::new();
            if self.index.contains_key("_id") {
                cells.push(cell(document.get("_id")));
            }
            cells.push(cell(Some(&Bson::Document(document.clone()))));
            return cells;
        }
        self.order()
            .into_iter()
            .map(|slot| cell(document.get(&self.names[slot])))
            .collect()
    }

    /// The fields as the columns of a container. `_id` is the key. A field
    /// is nullable when a null was seen in it, and has a default (it may be
    /// left out of a new document) when a document without it was seen.
    pub(super) fn columns(&self) -> Vec<ColumnInfo> {
        let key = |primary_key| ColumnInfo {
            name: "_id".into(),
            data_type: self
                .index
                .get("_id")
                .map_or_else(|| "objectId".into(), |slot| self.label(*slot)),
            nullable: false,
            primary_key,
            default: None,
        };
        if self.wide() {
            // Shown whole and never edited: the column of whole documents
            // is not a field, and without a key the grid does not edit.
            return vec![key(false)];
        }
        let mut columns = vec![key(true)];
        for slot in self.order() {
            if self.names[slot] == "_id" {
                continue;
            }
            columns.push(ColumnInfo {
                name: self.names[slot].clone(),
                data_type: self.label(slot),
                nullable: self.nulls[slot] > 0,
                primary_key: false,
                default: (self.present[slot] < self.documents).then(String::new),
            });
        }
        columns
    }
}

/// A set of documents as a grid: the union of their top-level fields.
pub(super) fn grid(documents: &[Document]) -> (Vec<ResultColumn>, Vec<Vec<Cell>>) {
    let mut fields = Fields::default();
    for document in documents {
        fields.observe(document);
    }
    (
        fields.result_columns(),
        documents
            .iter()
            .map(|document| fields.row(document))
            .collect(),
    )
}

/// The type of a column of values that are not documents.
pub(super) fn common_type(values: &[Bson]) -> String {
    let mut kinds: Vec<&'static str> = Vec::new();
    for value in values {
        let kind = type_name(value);
        if kind != "null" && !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    match kinds.as_slice() {
        [] => String::new(),
        [one] => (*one).into(),
        several => format!("mixed: {}", several.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::mongodb::bson::{Regex, Timestamp, doc, oid::ObjectId};

    fn samples() -> Vec<Bson> {
        let id = ObjectId::parse_str("65f0c1e2a4b5c6d7e8f90123").unwrap();
        vec![
            Bson::Double(1.0),
            Bson::Double(-0.1),
            Bson::Double(1e300),
            Bson::Double(f64::INFINITY),
            Bson::Double(f64::NEG_INFINITY),
            Bson::String("Zażółć \"gęślą\" \\ jaźń\n🦀".into()),
            Bson::Boolean(true),
            Bson::Null,
            Bson::Int32(-7),
            Bson::Int32(i32::MAX),
            Bson::Int64(7),
            Bson::Int64(i64::MIN),
            Bson::Decimal128("12345678901234567890.1200".parse().unwrap()),
            Bson::Decimal128("0.00".parse().unwrap()),
            Bson::ObjectId(id),
            Bson::DateTime(DateTime::from_millis(1_714_557_600_000)),
            Bson::DateTime(DateTime::from_millis(1_714_557_600_123)),
            Bson::DateTime(DateTime::from_millis(-1)),
            Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes: vec![0, 1, 2, 255],
            }),
            Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes: uuid::Uuid::parse_str("73ffd264-44b3-4c69-90e8-e7d1dfc035d4")
                    .unwrap()
                    .as_bytes()
                    .to_vec(),
            }),
            Bson::Binary(Binary {
                subtype: BinarySubtype::UserDefined(0x80),
                bytes: vec![9, 8, 7],
            }),
            Bson::RegularExpression(Regex {
                pattern: "^a/b\"c".try_into().unwrap(),
                options: "im".try_into().unwrap(),
            }),
            Bson::Timestamp(Timestamp {
                time: 1_714_557_600,
                increment: 3,
            }),
            Bson::MinKey,
            Bson::MaxKey,
            Bson::Undefined,
            Bson::JavaScriptCode("function() { return 1; }".into()),
            Bson::Symbol("sym".into()),
            Bson::Array(vec![Bson::Int32(1), Bson::Int64(1), Bson::Double(1.0)]),
            Bson::Document(doc! {
                "z": 1_i32, "a": 1_i64, "n": {"deep": [{"x": Bson::Decimal128("1.50".parse().unwrap())}]},
                "when": DateTime::from_millis(0), "id": id, "$odd key": "v", "": Bson::Null,
            }),
        ]
    }

    /// Every type comes back from its extended JSON as the same value of the
    /// same type, nested numbers included.
    #[test]
    fn extended_json_reads_back_as_the_same_value() {
        for value in samples() {
            let text = ext_json(&value);
            // It is strict JSON: the editor of a `Json` cell accepts it.
            serde_json::from_str::<serde_json::Value>(&text)
                .unwrap_or_else(|error| panic!("{text}: {error}"));
            let back = parse_json(&text).unwrap_or_else(|error| panic!("{text}: {error}"));
            assert_eq!(back, value, "{text}");
            assert_eq!(type_name(&back), type_name(&value), "{text}");
        }
        let nan = parse_json(&ext_json(&Bson::Double(f64::NAN))).unwrap();
        assert!(matches!(nan, Bson::Double(number) if number.is_nan()));
    }

    /// The notation of the shell, which the texts of pages and changes use,
    /// is read by the console as the same value.
    #[test]
    fn shell_notation_reads_back_as_the_same_value() {
        for value in samples() {
            let text = shell(&value);
            let back = super::super::shell::parse_value(&text)
                .unwrap_or_else(|error| panic!("{text}: {}", error.code));
            assert_eq!(back, value, "{text}");
        }
        assert_eq!(
            shell(&Bson::Document(
                doc! {"_id": 5, "a b": "x", "$set": {"n": 1_i64}}
            )),
            r#"{_id: 5, "a b": "x", $set: {n: NumberLong("1")}}"#
        );
    }

    #[test]
    fn extended_json_is_compact_and_marks_only_what_would_be_lost() {
        let id = ObjectId::parse_str("65f0c1e2a4b5c6d7e8f90123").unwrap();
        let document = doc! {
            "_id": id, "qty": 5_i32, "big": 5_i64, "price": 1.0_f64,
            "when": DateTime::from_millis(1_714_557_600_000), "tags": ["a", "b"],
        };
        assert_eq!(
            ext_json(&Bson::Document(document)),
            r#"{"_id":{"$oid":"65f0c1e2a4b5c6d7e8f90123"},"qty":5,"big":{"$numberLong":"5"},"price":1.0,"when":{"$date":"2024-05-01T10:00:00Z"},"tags":["a","b"]}"#
        );
    }

    /// A cell, and the value a key makes of it again.
    #[test]
    fn cells_keep_the_value_of_every_type() {
        for value in samples() {
            let shown = cell(Some(&value));
            let candidates = key_values(&shown).unwrap();
            match (&value, &candidates[0]) {
                // An integer cell is the smallest integer that holds it; the
                // server compares the two by value.
                (Bson::Int64(wide), Bson::Int32(small)) => assert_eq!(*wide, i64::from(*small)),
                // Shown as text: the value is one of what the text may be,
                // and an edit of the field keeps its type.
                (Bson::ObjectId(_) | Bson::DateTime(_), Bson::String(_))
                    if matches!(shown, Cell::Text(_)) =>
                {
                    assert_eq!(candidates.len(), 2, "{shown:?}");
                    assert_eq!(candidates[1], value, "{shown:?}");
                    assert_eq!(
                        edited_value(&shown, Some(&value), Shape::Open).unwrap(),
                        value
                    );
                }
                _ => assert_eq!(candidates, std::slice::from_ref(&value), "{shown:?}"),
            }
        }
        let id = ObjectId::parse_str("65f0c1e2a4b5c6d7e8f90123").unwrap();
        assert_eq!(
            cell(Some(&Bson::ObjectId(id))),
            Cell::Text("65f0c1e2a4b5c6d7e8f90123".into())
        );
        assert_eq!(
            cell(Some(&Bson::DateTime(DateTime::from_millis(
                1_714_557_600_123
            )))),
            Cell::Text("2024-05-01T10:00:00.123Z".into())
        );
        assert_eq!(
            cell(Some(&Bson::DateTime(DateTime::from_millis(-1)))),
            Cell::Text("1969-12-31T23:59:59.999Z".into())
        );
        // A date no text can hold keeps the form that says what it is.
        assert_eq!(
            cell(Some(&Bson::DateTime(DateTime::from_millis(i64::MAX)))),
            Cell::Json("{\"$date\":{\"$numberLong\":\"9223372036854775807\"}}".into())
        );
        // A text that only resembles a date or an ObjectId is one value.
        for text in [
            "2024-05-01",
            "2024-05-01T10:00:00+00:00",
            "65f0c1e2",
            "hello",
        ] {
            assert_eq!(
                key_values(&Cell::Text(text.into())).unwrap(),
                [Bson::String(text.into())]
            );
        }
        assert_eq!(cell(None), Cell::Null);
        assert_eq!(cell(Some(&Bson::Double(1.0))), Cell::Float("1.0".into()));
        assert_eq!(cell(Some(&Bson::Int64(1))), Cell::Integer("1".into()));
        assert_eq!(
            cell(Some(&Bson::Decimal128("0.00".parse().unwrap()))),
            Cell::Decimal("0.00".into())
        );
        assert_eq!(
            cell(Some(&Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes: vec![1, 2],
            }))),
            Cell::Bytes(vec![1, 2])
        );
    }

    /// The editor hands back a cell of the kind it was given; the value
    /// keeps the BSON type of what is stored.
    #[test]
    fn an_edited_value_keeps_the_type_of_the_stored_one() {
        let open = Shape::Open;
        let edit = |new: Cell, current: Bson| edited_value(&new, Some(&current), open).unwrap();
        assert_eq!(
            edit(Cell::Integer("9".into()), Bson::Int32(1)),
            Bson::Int32(9)
        );
        assert_eq!(
            edit(Cell::Integer("9".into()), Bson::Int64(1)),
            Bson::Int64(9)
        );
        // An int32 grows when the number no longer fits.
        assert_eq!(
            edit(Cell::Integer("3000000000".into()), Bson::Int32(1)),
            Bson::Int64(3_000_000_000)
        );
        // `Cell::from_edited` writes a float without its fraction.
        assert_eq!(
            edit(Cell::Float("2".into()), Bson::Double(1.5)),
            Bson::Double(2.0)
        );
        assert_eq!(
            edit(
                Cell::Decimal("2.50".into()),
                Bson::Decimal128("1".parse().unwrap())
            ),
            Bson::Decimal128("2.50".parse().unwrap())
        );
        // A text stays a text, whatever it looks like.
        assert_eq!(
            edit(Cell::Text("{\"a\":1}".into()), Bson::String("x".into())),
            Bson::String("{\"a\":1}".into())
        );
        assert_eq!(
            edit(Cell::Text("42".into()), Bson::String("x".into())),
            Bson::String("42".into())
        );
        // A `Json` cell says its type itself: a nested value, an array, a date.
        assert_eq!(
            edit(
                Cell::Json("{\"a\":[1,{\"$numberLong\":\"2\"}]}".into()),
                Bson::Null
            ),
            Bson::Document(doc! {"a": [1_i32, 2_i64]})
        );
        assert_eq!(
            edit(
                Cell::Json("{\"$date\":\"2026-01-02T03:04:05Z\"}".into()),
                Bson::Array(vec![])
            ),
            Bson::DateTime(DateTime::from_millis(1_767_323_045_000))
        );
        // A date and an ObjectId are edited as the text they are shown as,
        // and stay what they are.
        let date = Bson::DateTime(DateTime::from_millis(0));
        assert_eq!(
            edit(Cell::Text("2026-01-02T03:04:05Z".into()), date.clone()),
            Bson::DateTime(DateTime::from_millis(1_767_323_045_000))
        );
        assert_eq!(
            edit(Cell::Text("2026-01-02".into()), date.clone()),
            Bson::DateTime(DateTime::from_millis(1_767_312_000_000))
        );
        assert_eq!(
            edit(Cell::Text("ISODate(\"2026-01-02\")".into()), date.clone()),
            Bson::DateTime(DateTime::from_millis(1_767_312_000_000))
        );
        let id = ObjectId::parse_str("65f0c1e2a4b5c6d7e8f90123").unwrap();
        assert_eq!(
            edit(
                Cell::Text("000000000000000000000001".into()),
                Bson::ObjectId(id)
            ),
            Bson::ObjectId(ObjectId::parse_str("000000000000000000000001").unwrap())
        );
        assert_eq!(
            edited_value(&Cell::Text("soon".into()), Some(&date), open)
                .unwrap_err()
                .to_string(),
            "mongodb_date_invalid"
        );
        assert_eq!(
            edited_value(&Cell::Text("xyz".into()), Some(&Bson::ObjectId(id)), open)
                .unwrap_err()
                .to_string(),
            "mongodb_objectid_invalid"
        );
        assert!(edited_value(&Cell::Integer("x".into()), Some(&Bson::Int32(1)), open).is_err());
        assert!(edited_value(&Cell::Json("{oops".into()), Some(&Bson::Null), open).is_err());
    }

    /// A field that holds nothing yet takes the type of the field elsewhere
    /// in the collection, and otherwise what the text reads as.
    #[test]
    fn a_typed_text_takes_the_type_of_its_field() {
        let typed = |text: &str, shape| field_value(&Cell::Text(text.into()), shape, Source::Typed);
        let uniform = Shape::Uniform;
        assert_eq!(typed("5", uniform("int32")).unwrap(), Bson::Int32(5));
        assert_eq!(typed("5", uniform("int64")).unwrap(), Bson::Int64(5));
        assert_eq!(typed("5", uniform("double")).unwrap(), Bson::Double(5.0));
        assert_eq!(
            typed("5", uniform("string")).unwrap(),
            Bson::String("5".into())
        );
        assert_eq!(
            typed("5.10", uniform("decimal128")).unwrap(),
            Bson::Decimal128("5.10".parse().unwrap())
        );
        assert_eq!(typed("TRUE", uniform("bool")).unwrap(), Bson::Boolean(true));
        assert_eq!(
            typed("2026-05-01", uniform("date")).unwrap(),
            Bson::DateTime(DateTime::from_millis(1_777_593_600_000))
        );
        assert_eq!(
            typed("2026-05-01T12:00:00+02:00", uniform("date")).unwrap(),
            Bson::DateTime(DateTime::from_millis(1_777_629_600_000))
        );
        assert_eq!(
            typed("65f0c1e2a4b5c6d7e8f90123", uniform("objectId")).unwrap(),
            Bson::ObjectId(ObjectId::parse_str("65f0c1e2a4b5c6d7e8f90123").unwrap())
        );
        // What does not fit is refused, not stored as a text.
        for (text, kind, code) in [
            ("abc", "int32", "integer_invalid"),
            ("1.5", "int64", "integer_invalid"),
            ("abc", "double", "float_invalid"),
            ("abc", "decimal128", "decimal_invalid"),
            ("maybe", "bool", "boolean_invalid"),
            ("tomorrow", "date", "mongodb_date_invalid"),
            ("xyz", "objectId", "mongodb_objectid_invalid"),
            ("{oops", "array", "json_invalid"),
        ] {
            assert_eq!(
                typed(text, uniform(kind)).unwrap_err().to_string(),
                code,
                "{text}"
            );
        }
        // The explicit form works in a typed field: it is what an export writes.
        assert_eq!(
            typed("{\"$date\":\"2026-05-01T00:00:00Z\"}", uniform("date")).unwrap(),
            Bson::DateTime(DateTime::from_millis(1_777_593_600_000))
        );
        assert_eq!(
            typed("NumberLong(5)", uniform("int32")).unwrap(),
            Bson::Int64(5)
        );
        // A field without one type: the text says what it is.
        for (text, value) in [
            ("42", Bson::Int32(42)),
            ("-3000000000", Bson::Int64(-3_000_000_000)),
            ("1.5", Bson::Double(1.5)),
            ("true", Bson::Boolean(true)),
            ("null", Bson::Null),
            ("\"42\"", Bson::String("42".into())),
            ("[1,2]", Bson::Array(vec![Bson::Int32(1), Bson::Int32(2)])),
            ("{a: 1}", Bson::Document(doc! {"a": 1})),
            (
                "ObjectId(\"65f0c1e2a4b5c6d7e8f90123\")",
                Bson::ObjectId(ObjectId::parse_str("65f0c1e2a4b5c6d7e8f90123").unwrap()),
            ),
            // Everything else is a text, also what only resembles a value.
            ("hello", Bson::String("hello".into())),
            ("007", Bson::String("007".into())),
            ("+48 123 456", Bson::String("+48 123 456".into())),
            ("2026-05-01", Bson::String("2026-05-01".into())),
            ("/usr/bin/", Bson::String("/usr/bin/".into())),
            ("{not json", Bson::String("{not json".into())),
            ("", Bson::String(String::new())),
        ] {
            assert_eq!(typed(text, Shape::Open).unwrap(), value, "{text}");
        }
        // A cell of an imported file is a text unless its field has a type.
        let imported = |cell: Cell, shape| field_value(&cell, shape, Source::Imported).unwrap();
        assert_eq!(
            imported(Cell::Text("42".into()), Shape::Open),
            Bson::String("42".into())
        );
        assert_eq!(
            imported(Cell::Text("42".into()), uniform("int64")),
            Bson::Int64(42)
        );
        assert_eq!(
            imported(Cell::Integer("42".into()), Shape::Open),
            Bson::Int32(42)
        );
        assert_eq!(
            imported(Cell::Integer("42".into()), uniform("int64")),
            Bson::Int64(42)
        );
        assert_eq!(
            imported(Cell::Decimal("1.50".into()), uniform("double")),
            Bson::Decimal128("1.50".parse().unwrap())
        );
        assert_eq!(
            imported(Cell::Float("1.5".into()), Shape::Open),
            Bson::Double(1.5)
        );
        assert_eq!(imported(Cell::Null, uniform("int32")), Bson::Null);
        assert_eq!(
            imported(
                Cell::Json("{\"$oid\":\"65f0c1e2a4b5c6d7e8f90123\"}".into()),
                Shape::Open
            ),
            Bson::ObjectId(ObjectId::parse_str("65f0c1e2a4b5c6d7e8f90123").unwrap())
        );
    }

    #[test]
    fn columns_are_the_union_of_the_fields_with_their_types() {
        let id = ObjectId::new();
        let documents = vec![
            doc! {"name": "a", "qty": 1_i32, "_id": id, "tags": ["x"], "note": Bson::Null},
            doc! {"_id": 2_i32, "name": "b", "qty": "many", "address": {"city": "Łódź"}, "note": Bson::Null},
            doc! {"_id": 3_i32, "name": Bson::Null, "qty": 2_i64},
        ];
        let mut fields = Fields::default();
        for document in &documents {
            fields.observe(document);
        }
        let columns = fields.result_columns();
        let described = columns
            .iter()
            .map(|column| format!("{} {}", column.name, column.data_type))
            .collect::<Vec<_>>();
        assert_eq!(
            described,
            [
                "_id mixed: objectId, int32",
                "name string",
                "qty mixed: int32, string, int64",
                "tags array",
                "note null",
                "address object",
            ]
        );
        assert_eq!(
            fields.row(&documents[1]),
            [
                Cell::Integer("2".into()),
                Cell::Text("b".into()),
                Cell::Text("many".into()),
                Cell::Null,
                Cell::Null,
                Cell::Json("{\"city\":\"Łódź\"}".into()),
            ]
        );
        let infos = fields.columns();
        assert!(infos[0].primary_key && !infos[0].nullable && infos[0].name == "_id");
        // `name` holds a null somewhere; `tags` and `address` are missing
        // somewhere: a new document may leave them out.
        let info = |name: &str| infos.iter().find(|column| column.name == name).unwrap();
        assert!(info("name").nullable && info("name").default.is_none());
        assert!(!info("qty").nullable && info("qty").default.is_none());
        assert!(!info("tags").nullable && info("tags").default.is_some());
        assert!(info("note").nullable && info("note").default.is_some());
        assert_eq!(fields.shape("name"), Shape::Uniform("string"));
        assert_eq!(fields.shape("qty"), Shape::Open);
        assert_eq!(fields.shape("note"), Shape::Open);
        assert_eq!(fields.shape("nobody"), Shape::Open);
        // An empty collection still has its key.
        let empty = Fields::default().columns();
        assert_eq!(empty.len(), 1);
        assert!(empty[0].primary_key && empty[0].data_type == "objectId");
    }

    #[test]
    fn a_collection_with_too_many_fields_is_shown_whole_and_read_only() {
        let mut fields = Fields::default();
        let documents = (0..=MAX_FIELDS)
            .map(|n| {
                let mut document = doc! {"_id": n as i32};
                document.insert(format!("f{n}"), n as i32);
                document
            })
            .collect::<Vec<_>>();
        for document in &documents {
            fields.observe(document);
        }
        assert!(fields.wide());
        let names = fields
            .result_columns()
            .into_iter()
            .map(|column| column.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["_id", WHOLE]);
        assert_eq!(
            fields.row(&documents[3]),
            [
                Cell::Integer("3".into()),
                Cell::Json("{\"_id\":3,\"f3\":3}".into())
            ]
        );
        // The columns of the container do not match the result on purpose:
        // the interface then offers no editing, adding or removing.
        let infos = fields.columns();
        assert_eq!(infos.len(), 1);
        assert!(!infos[0].primary_key);
    }

    /// A field that only a later page showed stays a column of the
    /// collection, so that the grid and the columns keep matching.
    #[test]
    fn a_field_found_later_is_kept_as_a_column() {
        let mut sample = Fields::default();
        sample.observe(&doc! {"_id": 1, "a": 1});
        let mut page = sample.clone();
        page.observe(&doc! {"_id": 9, "a": "text", "rare": true});
        assert!(page.knows("rare") && !sample.knows("rare"));
        let mut fresh = Fields::default();
        fresh.observe(&doc! {"_id": 1, "a": 1});
        fresh.adopt(&page, "rare");
        fresh.adopt(&page, "a");
        assert_eq!(fresh.names(), ["_id", "a", "rare"]);
        let columns = fresh.columns();
        assert_eq!(columns[2].name, "rare");
        assert_eq!(columns[2].data_type, "bool");
        // Nobody in the sample has it: a new document may leave it out.
        assert!(columns[2].default.is_some() && !columns[2].nullable);
        assert_eq!(fresh.shape("a"), Shape::Uniform("int32"));
        assert_eq!(fresh.row(&doc! {"_id": 2}).len(), 3);
    }

    #[test]
    fn dates_and_doubles_are_written_and_read_exactly() {
        assert_eq!(date_text(0).unwrap(), "1970-01-01T00:00:00Z");
        assert_eq!(
            date_text(1_714_557_600_123).unwrap(),
            "2024-05-01T10:00:00.123Z"
        );
        assert_eq!(date_text(-1).unwrap(), "1969-12-31T23:59:59.999Z");
        assert_eq!(date_text(-62_135_596_800_001), None);
        assert_eq!(
            shown_date("2024-05-01T10:00:00.123Z"),
            Some(1_714_557_600_123)
        );
        assert_eq!(shown_date("2024-05-01T10:00:00.000Z"), None);
        assert_eq!(
            parse_date("2024-05-01T10:00:00.123Z"),
            Some(1_714_557_600_123)
        );
        assert_eq!(parse_date("2024-05-01 10:00:00"), Some(1_714_557_600_000));
        assert_eq!(parse_date("2024-05-01T10:00"), Some(1_714_557_600_000));
        assert_eq!(parse_date("01.05.2024"), None);
        for number in [
            0.0,
            -0.0,
            1.0,
            0.1,
            1e21,
            1e-7,
            123456789.125,
            f64::MAX,
            f64::MIN_POSITIVE,
        ] {
            let text = double_text(number);
            assert!(text.contains(['.', 'e']), "{text}");
            assert_eq!(
                parse_double(&text).unwrap().to_bits(),
                number.to_bits(),
                "{text}"
            );
        }
        assert_eq!(parse_double("inf"), None);
        assert_eq!(parse_double("1_000"), None);
    }
}
