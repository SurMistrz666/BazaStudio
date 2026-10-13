//! Values in both directions: what the server returns becomes a cell of the
//! grid, and a cell (or the text a person typed) becomes a CQL literal of the
//! type of its column.
//!
//! Collections, tuples, user-defined types and vectors are shown as compact
//! JSON and read back from the same JSON. CQL writes several types without
//! quotes (`uuid`, `duration`, numbers), so every such text is checked here
//! before it is put into a statement.
use super::types::{CqlType, Native, Udts};
use crate::model::Cell;
use anyhow::{Result, anyhow, bail, ensure};
use bigdecimal::BigDecimal;
use bigdecimal::num_bigint::BigInt;
use scylla::value::{CqlDuration, CqlValue};
use std::fmt::Write as _;
use std::net::IpAddr;

/// Days from 0001-01-01 (day 1 of the calendar chrono counts in) to 1970-01-01.
const EPOCH_DAYS_FROM_CE: i64 = 719_163;
/// Most digits of a `varint` or zeros of a `decimal` written out in full.
const PLAIN_DIGITS_MAX: i64 = 4096;

/// A value of a result as a cell of the grid.
pub(super) fn cell(value: Option<&CqlValue>) -> Cell {
    let Some(value) = value else {
        return Cell::Null;
    };
    match value {
        CqlValue::Empty => Cell::Null,
        CqlValue::Ascii(text) | CqlValue::Text(text) => Cell::Text(text.clone()),
        CqlValue::Boolean(flag) => Cell::Bool(*flag),
        CqlValue::Blob(bytes) => Cell::Bytes(bytes.clone()),
        CqlValue::Counter(counter) => Cell::Integer(counter.0.to_string()),
        CqlValue::TinyInt(number) => Cell::Integer(number.to_string()),
        CqlValue::SmallInt(number) => Cell::Integer(number.to_string()),
        CqlValue::Int(number) => Cell::Integer(number.to_string()),
        CqlValue::BigInt(number) => Cell::Integer(number.to_string()),
        CqlValue::Varint(number) => {
            let text = BigInt::from_signed_bytes_be(number.as_signed_bytes_be_slice()).to_string();
            // The grid edits whole numbers up to 128 bits; a longer one keeps
            // every digit as an exact decimal.
            if text.parse::<i128>().is_ok() {
                Cell::Integer(text)
            } else {
                Cell::Decimal(text)
            }
        }
        CqlValue::Decimal(number) => {
            let (bytes, scale) = number.as_signed_be_bytes_slice_and_exponent();
            Cell::Decimal(decimal_text(&BigInt::from_signed_bytes_be(bytes), scale))
        }
        CqlValue::Float(number) => Cell::Float(single(*number)),
        CqlValue::Double(number) => Cell::Float(double(*number)),
        CqlValue::List(_)
        | CqlValue::Set(_)
        | CqlValue::Map(_)
        | CqlValue::Tuple(_)
        | CqlValue::Vector(_)
        | CqlValue::UserDefinedType { .. } => {
            let mut out = String::new();
            json(value, &mut out);
            Cell::Json(out)
        }
        other => Cell::Text(plain_text(other)),
    }
}

/// The text of a value that is shown as text: dates, times, addresses and
/// identifiers. Also the text of a map key inside JSON.
fn plain_text(value: &CqlValue) -> String {
    match value {
        CqlValue::Ascii(text) | CqlValue::Text(text) => text.clone(),
        CqlValue::Uuid(id) => id.hyphenated().to_string(),
        CqlValue::Timeuuid(id) => uuid::Uuid::from_bytes(*id.as_bytes())
            .hyphenated()
            .to_string(),
        CqlValue::Inet(address) => address.to_string(),
        CqlValue::Date(date) => date_text(date.0),
        CqlValue::Time(time) => time_text(time.0),
        CqlValue::Timestamp(timestamp) => timestamp_text(timestamp.0),
        CqlValue::Duration(duration) => duration_text(duration),
        CqlValue::Blob(bytes) => format!("0x{}", hex(bytes)),
        CqlValue::Empty => String::new(),
        other => match cell(Some(other)) {
            Cell::Null => String::new(),
            shown => shown.display(),
        },
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// A decimal from its unscaled digits and its scale, digit for digit:
/// trailing zeros are part of the value as it was stored.
pub(super) fn decimal_text(unscaled: &BigInt, scale: i32) -> String {
    let negative = unscaled.sign() == bigdecimal::num_bigint::Sign::Minus;
    let digits = unscaled.magnitude().to_string();
    let scale = i64::from(scale);
    let body = if scale <= 0 {
        if digits == "0" {
            digits
        } else if -scale > PLAIN_DIGITS_MAX {
            format!("{digits}E+{}", -scale)
        } else {
            format!("{digits}{}", "0".repeat(-scale as usize))
        }
    } else if scale > PLAIN_DIGITS_MAX {
        format!("{digits}E-{scale}")
    } else {
        let scale = scale as usize;
        if digits.len() > scale {
            let (whole, fraction) = digits.split_at(digits.len() - scale);
            format!("{whole}.{fraction}")
        } else {
            format!("0.{}{digits}", "0".repeat(scale - digits.len()))
        }
    };
    if negative { format!("-{body}") } else { body }
}

/// A floating point number in its shortest form that reads back as the same
/// value: plain digits while they stay readable, an exponent beyond that,
/// and the words CQL has for what is not a number. `plain` and `exponent`
/// are the two forms of the value in its own width.
fn float_text(value: f64, plain: String, exponent: String) -> String {
    if value.is_nan() {
        "NaN".into()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else if value != 0.0 && !(1e-7..1e17).contains(&value.abs()) {
        exponent
    } else {
        plain
    }
}

fn single(value: f32) -> String {
    float_text(f64::from(value), value.to_string(), format!("{value:e}"))
}

fn double(value: f64) -> String {
    float_text(value, value.to_string(), format!("{value:e}"))
}

/// A `date`: days counted from 2^31 at 1970-01-01.
fn date_text(raw: u32) -> String {
    let days = i64::from(raw) - (1i64 << 31);
    i32::try_from(days + EPOCH_DAYS_FROM_CE)
        .ok()
        .and_then(chrono::NaiveDate::from_num_days_from_ce_opt)
        .map_or_else(
            || days.to_string(),
            |date| date.format("%Y-%m-%d").to_string(),
        )
}

/// A `time`: nanoseconds since midnight, with as many digits as it has.
fn time_text(nanoseconds: i64) -> String {
    if !(0..86_400_000_000_000).contains(&nanoseconds) {
        return nanoseconds.to_string();
    }
    let seconds = nanoseconds / 1_000_000_000;
    let fraction = nanoseconds % 1_000_000_000;
    let mut text = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    );
    if fraction != 0 {
        if fraction % 1_000_000 == 0 {
            let _ = write!(text, ".{:03}", fraction / 1_000_000);
        } else if fraction % 1000 == 0 {
            let _ = write!(text, ".{:06}", fraction / 1000);
        } else {
            let _ = write!(text, ".{fraction:09}");
        }
    }
    text
}

/// A `timestamp`: milliseconds since 1970-01-01, shown in UTC.
fn timestamp_text(milliseconds: i64) -> String {
    match chrono::DateTime::from_timestamp_millis(milliseconds) {
        // Whole seconds are the common case: no ".000" in every cell.
        Some(moment) if milliseconds % 1000 == 0 => moment.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        Some(moment) => moment.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        None => milliseconds.to_string(),
    }
}

/// A `duration` in the units CQL reads back: `1y2mo3d4h5m6s7ms8us9ns`.
pub(super) fn duration_text(duration: &CqlDuration) -> String {
    let negative = duration.months < 0 || duration.days < 0 || duration.nanoseconds < 0;
    let months = i64::from(duration.months).unsigned_abs();
    let days = i64::from(duration.days).unsigned_abs();
    let mut nanoseconds = duration.nanoseconds.unsigned_abs();
    let mut text = String::new();
    let mut unit = |amount: u64, name: &str| {
        if amount != 0 {
            let _ = write!(text, "{amount}{name}");
        }
    };
    unit(months / 12, "y");
    unit(months % 12, "mo");
    unit(days, "d");
    for (size, name) in [
        (3_600_000_000_000, "h"),
        (60_000_000_000, "m"),
        (1_000_000_000, "s"),
        (1_000_000, "ms"),
        (1000, "us"),
        (1, "ns"),
    ] {
        unit(nanoseconds / size, name);
        nanoseconds %= size;
    }
    if text.is_empty() {
        return "0s".into();
    }
    if negative { format!("-{text}") } else { text }
}

fn json_string(text: &str, out: &mut String) {
    out.push_str(&serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into()));
}

/// A value as compact JSON: collections as arrays and objects, numbers with
/// every digit, and what JSON has no notation for as text.
pub(super) fn json(value: &CqlValue, out: &mut String) {
    match value {
        CqlValue::Empty => out.push_str("null"),
        CqlValue::Boolean(flag) => out.push_str(if *flag { "true" } else { "false" }),
        CqlValue::TinyInt(_)
        | CqlValue::SmallInt(_)
        | CqlValue::Int(_)
        | CqlValue::BigInt(_)
        | CqlValue::Counter(_)
        | CqlValue::Varint(_)
        | CqlValue::Decimal(_) => out.push_str(&cell(Some(value)).display()),
        CqlValue::Float(number) if number.is_finite() => out.push_str(&single(*number)),
        CqlValue::Double(number) if number.is_finite() => out.push_str(&double(*number)),
        CqlValue::List(items) | CqlValue::Set(items) | CqlValue::Vector(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                json(item, out);
            }
            out.push(']');
        }
        CqlValue::Tuple(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                match item {
                    Some(item) => json(item, out),
                    None => out.push_str("null"),
                }
            }
            out.push(']');
        }
        CqlValue::Map(pairs) => {
            out.push('{');
            for (index, (key, item)) in pairs.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                // A JSON key is text: a key of another type is written the
                // way it is shown, a composite one as its own JSON.
                let name = match key {
                    CqlValue::List(_)
                    | CqlValue::Set(_)
                    | CqlValue::Map(_)
                    | CqlValue::Tuple(_)
                    | CqlValue::Vector(_)
                    | CqlValue::UserDefinedType { .. } => {
                        let mut text = String::new();
                        json(key, &mut text);
                        text
                    }
                    other => plain_text(other),
                };
                json_string(&name, out);
                out.push(':');
                json(item, out);
            }
            out.push('}');
        }
        CqlValue::UserDefinedType { fields, .. } => {
            out.push('{');
            for (index, (name, item)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                json_string(name, out);
                out.push(':');
                match item {
                    Some(item) => json(item, out),
                    None => out.push_str("null"),
                }
            }
            out.push('}');
        }
        other => json_string(&plain_text(other), out),
    }
}

/// A name in double quotes: exactly as the schema stores it.
pub(super) fn name(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control),
        "identifier_invalid"
    );
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
}

/// `"keyspace"."table"`.
pub(super) fn qualified(keyspace: &str, table: &str) -> Result<String> {
    ensure!(!keyspace.is_empty(), "cassandra_keyspace_required");
    Ok(format!("{}.{}", name(keyspace)?, name(table)?))
}

fn quoted(text: &str) -> Result<String> {
    ensure!(!text.contains('\0'), "text_nul_unsupported");
    Ok(format!("'{}'", text.replace('\'', "''")))
}

fn invalid() -> anyhow::Error {
    anyhow!("cassandra_value_invalid")
}

/// A whole number from what was typed: digits, or a decimal without a
/// fraction (`5.0`).
fn integer(text: &str) -> Result<BigInt> {
    let text = text.trim();
    ensure!(text.len() <= PLAIN_DIGITS_MAX as usize, "integer_invalid");
    if let Ok(number) = text.parse::<BigInt>() {
        return Ok(number);
    }
    let decimal: BigDecimal = text.parse().map_err(|_| anyhow!("integer_invalid"))?;
    ensure!(decimal.is_integer(), "integer_invalid");
    let (unscaled, scale) = decimal.with_scale(0).into_bigint_and_exponent();
    ensure!(scale == 0, "integer_invalid");
    Ok(unscaled)
}

/// Only what a CQL number may be made of, so that the text can stand in a
/// statement without quotes.
fn number_shaped(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.' | b'e' | b'E'))
}

/// Check a duration as CQL reads one without quotes: `1h30m`, `-2d`,
/// `P1Y2M3DT4H5M6S`, `P2W`. Returns it as it may be written.
pub(super) fn duration(text: &str) -> Result<String> {
    let text = text.trim();
    let body = text.strip_prefix('-').unwrap_or(text);
    ensure!(
        !body.is_empty() && body.len() <= 80,
        "cassandra_value_invalid"
    );
    if body.starts_with(['P', 'p']) {
        // ISO 8601: letters and digits only, with at least one number.
        ensure!(
            body.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b':')
                && body.bytes().any(|byte| byte.is_ascii_digit()),
            "cassandra_value_invalid"
        );
        return Ok(text.to_owned());
    }
    let mut rest = body;
    let mut parts = 0;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        ensure!(digits > 0 && digits <= 18, "cassandra_value_invalid");
        rest = &rest[digits..];
        let unit = ["mo", "ms", "us", "µs", "ns", "y", "w", "d", "h", "m", "s"]
            .into_iter()
            .find(|unit| {
                rest.get(..unit.len())
                    .is_some_and(|start| start.eq_ignore_ascii_case(unit))
            })
            .ok_or_else(invalid)?;
        rest = &rest[unit.len()..];
        parts += 1;
    }
    ensure!(parts > 0, "cassandra_value_invalid");
    Ok(text.to_owned())
}

/// A timestamp as a literal: milliseconds, or a date and time that is read
/// as UTC when it names no zone, the way this driver shows them.
fn timestamp(text: &str) -> Result<String> {
    let text = text.trim();
    if text.parse::<i64>().is_ok() {
        return Ok(text.to_owned());
    }
    let utc = |moment: chrono::DateTime<chrono::Utc>| {
        format!("'{}'", moment.format("%Y-%m-%dT%H:%M:%S%.3fZ"))
    };
    if let Ok(moment) = chrono::DateTime::parse_from_rfc3339(text) {
        return Ok(utc(moment.to_utc()));
    }
    for format in [
        "%Y-%m-%d %H:%M:%S%.f%#z",
        "%Y-%m-%dT%H:%M:%S%.f%#z",
        "%Y-%m-%d %H:%M%#z",
        "%Y-%m-%dT%H:%M%#z",
    ] {
        if let Ok(moment) = chrono::DateTime::parse_from_str(text, format) {
            return Ok(utc(moment.to_utc()));
        }
    }
    for format in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(moment) = chrono::NaiveDateTime::parse_from_str(text, format) {
            return Ok(utc(moment.and_utc()));
        }
    }
    let date = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| invalid())?;
    Ok(utc(date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(invalid)?
        .and_utc()))
}

/// The literal of a value of a type without parts.
fn native_literal(native: Native, cell: &Cell) -> Result<String> {
    let text = match cell {
        Cell::Null => return Ok("null".into()),
        Cell::Unsupported(_) => bail!("unsupported_cell_type"),
        Cell::Bytes(bytes) if native == Native::Blob => return Ok(format!("0x{}", hex(bytes))),
        other => other.display(),
    };
    let trimmed = text.trim();
    Ok(match native {
        Native::Ascii => {
            ensure!(text.is_ascii(), "cassandra_value_invalid");
            quoted(&text)?
        }
        Native::Text => quoted(&text)?,
        Native::TinyInt | Native::SmallInt | Native::Int | Native::BigInt | Native::Counter => {
            let number = integer(trimmed)?;
            let fits = match native {
                Native::TinyInt => i8::try_from(&number).is_ok(),
                Native::SmallInt => i16::try_from(&number).is_ok(),
                Native::Int => i32::try_from(&number).is_ok(),
                _ => i64::try_from(&number).is_ok(),
            };
            ensure!(fits, "integer_invalid");
            number.to_string()
        }
        Native::Varint => integer(trimmed)?.to_string(),
        Native::Float | Native::Double => match trimmed.to_ascii_lowercase().as_str() {
            "nan" => "NaN".into(),
            "infinity" | "inf" | "+infinity" | "+inf" => "Infinity".into(),
            "-infinity" | "-inf" => "-Infinity".into(),
            _ => {
                ensure!(
                    number_shaped(trimmed) && trimmed.parse::<f64>().is_ok_and(f64::is_finite),
                    "float_invalid"
                );
                trimmed.to_owned()
            }
        },
        Native::Decimal => {
            ensure!(
                number_shaped(trimmed) && trimmed.parse::<BigDecimal>().is_ok(),
                "decimal_invalid"
            );
            trimmed.to_owned()
        }
        Native::Boolean => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "1" => "true".into(),
            "false" | "0" => "false".into(),
            _ => bail!("boolean_invalid"),
        },
        Native::Uuid | Native::Timeuuid => uuid::Uuid::parse_str(trimmed)
            .map_err(|_| invalid())?
            .hyphenated()
            .to_string(),
        Native::Timestamp => timestamp(trimmed)?,
        Native::Date => {
            if trimmed.parse::<u32>().is_ok() {
                trimmed.to_owned()
            } else {
                chrono::NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").map_err(|_| invalid())?;
                quoted(trimmed)?
            }
        }
        Native::Time => {
            if trimmed.parse::<u64>().is_ok() {
                trimmed.to_owned()
            } else {
                ensure!(
                    ["%H:%M:%S%.f", "%H:%M"]
                        .iter()
                        .any(|format| chrono::NaiveTime::parse_from_str(trimmed, format).is_ok()),
                    "cassandra_value_invalid"
                );
                quoted(trimmed)?
            }
        }
        Native::Inet => quoted(
            &trimmed
                .parse::<IpAddr>()
                .map_err(|_| invalid())?
                .to_string(),
        )?,
        Native::Duration => duration(trimmed)?,
        Native::Blob => {
            let digits = trimmed
                .strip_prefix("0x")
                .or_else(|| trimmed.strip_prefix("0X"))
                .unwrap_or(trimmed);
            ensure!(
                digits.len() % 2 == 0 && digits.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "cassandra_value_invalid"
            );
            format!("0x{}", digits.to_ascii_lowercase())
        }
    })
}

/// The literal of a JSON value for a type: how a collection, a tuple or a
/// user-defined type that the grid shows as JSON is written back.
fn json_literal(ty: &CqlType, value: &serde_json::Value, udts: &Udts) -> Result<String> {
    use serde_json::Value;
    if value.is_null() {
        return Ok("null".into());
    }
    let list = |items: &[Value], item: &CqlType, open: char, close: char| -> Result<String> {
        let parts = items
            .iter()
            .map(|value| {
                ensure!(!value.is_null(), "cassandra_value_invalid");
                json_literal(item, value, udts)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(format!("{open}{}{close}", parts.join(", ")))
    };
    match ty.thawed() {
        CqlType::Native(native) => match value {
            Value::String(text) => native_literal(*native, &Cell::Text(text.clone())),
            Value::Bool(flag) => native_literal(*native, &Cell::Bool(*flag)),
            Value::Number(number) => native_literal(*native, &Cell::Text(number.to_string())),
            _ => Err(invalid()),
        },
        CqlType::List(item) => list(value.as_array().ok_or_else(invalid)?, item, '[', ']'),
        CqlType::Vector(item, dimensions) => {
            let items = value.as_array().ok_or_else(invalid)?;
            ensure!(
                items.len() == *dimensions as usize,
                "cassandra_value_invalid"
            );
            list(items, item, '[', ']')
        }
        CqlType::Set(item) => list(value.as_array().ok_or_else(invalid)?, item, '{', '}'),
        CqlType::Map(key_type, value_type) => {
            let pair = |key: String, value: &Value| -> Result<String> {
                ensure!(!value.is_null(), "cassandra_value_invalid");
                Ok(format!("{key}: {}", json_literal(value_type, value, udts)?))
            };
            let parts = match value {
                Value::Object(entries) => entries
                    .iter()
                    .map(|(key, value)| {
                        // A key of a type with parts was written as JSON text.
                        let key = match key_type.thawed() {
                            CqlType::Native(native) => {
                                native_literal(*native, &Cell::Text(key.clone()))?
                            }
                            composite => json_literal(
                                composite,
                                &serde_json::from_str(key).map_err(|_| invalid())?,
                                udts,
                            )?,
                        };
                        pair(key, value)
                    })
                    .collect::<Result<Vec<_>>>()?,
                // Also `[[key, value], …]`, for keys JSON cannot name.
                Value::Array(pairs) => pairs
                    .iter()
                    .map(|entry| match entry.as_array().map(Vec::as_slice) {
                        Some([key, value]) if !key.is_null() => {
                            pair(json_literal(key_type, key, udts)?, value)
                        }
                        _ => Err(invalid()),
                    })
                    .collect::<Result<Vec<_>>>()?,
                _ => return Err(invalid()),
            };
            Ok(format!("{{{}}}", parts.join(", ")))
        }
        CqlType::Tuple(types) => {
            let items = value.as_array().ok_or_else(invalid)?;
            ensure!(items.len() == types.len(), "cassandra_value_invalid");
            let parts = types
                .iter()
                .zip(items)
                .map(|(ty, value)| json_literal(ty, value, udts))
                .collect::<Result<Vec<_>>>()?;
            Ok(format!("({})", parts.join(", ")))
        }
        CqlType::Udt(type_name) => {
            let fields = udts
                .get(type_name)
                .ok_or_else(|| anyhow!("cassandra_type_unknown"))?;
            let given = value.as_object().ok_or_else(invalid)?;
            ensure!(
                given
                    .keys()
                    .all(|key| fields.iter().any(|(field, _)| field == key)),
                "cassandra_value_invalid"
            );
            let parts = fields
                .iter()
                .filter_map(|(field, ty)| given.get(field).map(|value| (field, ty, value)))
                .map(|(field, ty, value)| {
                    Ok(format!(
                        "{}: {}",
                        name(field)?,
                        json_literal(ty, value, udts)?
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(format!("{{{}}}", parts.join(", ")))
        }
        CqlType::Frozen(_) => unreachable!("thawed"),
        CqlType::Custom(_) => bail!("cassandra_type_unsupported"),
    }
}

/// The CQL literal of a cell for a column of this type. The kind of the cell
/// is not trusted: the grid and an imported file hand over text for most
/// types, so the type of the column decides how it is read.
pub(super) fn literal(ty: &CqlType, cell: &Cell, udts: &Udts) -> Result<String> {
    match (ty.thawed(), cell) {
        (_, Cell::Null) => Ok("null".into()),
        (_, Cell::Unsupported(_)) => bail!("unsupported_cell_type"),
        (CqlType::Native(native), cell) => native_literal(*native, cell),
        (CqlType::Custom(_), _) => bail!("cassandra_type_unsupported"),
        (composite, cell) => {
            let value: serde_json::Value =
                serde_json::from_str(&cell.display()).map_err(|_| anyhow!("json_invalid"))?;
            json_literal(composite, &value, udts)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scylla::value::{Counter, CqlDate, CqlDecimal, CqlTime, CqlTimestamp, CqlVarint};

    fn shown(value: CqlValue) -> Cell {
        cell(Some(&value))
    }

    fn written(ty: &str, cell: Cell) -> Result<String, String> {
        literal(&CqlType::parse(ty), &cell, &Udts::new()).map_err(|error| error.to_string())
    }

    fn text(value: &str) -> Cell {
        Cell::Text(value.into())
    }

    #[test]
    fn numbers_keep_every_digit() {
        assert_eq!(shown(CqlValue::Int(-7)), Cell::Integer("-7".into()));
        assert_eq!(
            shown(CqlValue::BigInt(i64::MIN)),
            Cell::Integer("-9223372036854775808".into())
        );
        assert_eq!(
            shown(CqlValue::Counter(Counter(12))),
            Cell::Integer("12".into())
        );
        let big: BigInt = "123456789012345678901234567890123456789012345678901234567890"
            .parse()
            .unwrap();
        // Beyond 128 bits a whole number is shown as an exact decimal.
        assert_eq!(
            shown(CqlValue::Varint(CqlVarint::from_signed_bytes_be(
                big.to_signed_bytes_be()
            ))),
            Cell::Decimal(big.to_string())
        );
        assert_eq!(
            shown(CqlValue::Varint(CqlVarint::from_signed_bytes_be(vec![
                0xff
            ]))),
            Cell::Integer("-1".into())
        );
        let decimal = |digits: &str, scale: i32| {
            let unscaled: BigInt = digits.parse().unwrap();
            shown(CqlValue::Decimal(
                CqlDecimal::from_signed_be_bytes_and_exponent(unscaled.to_signed_bytes_be(), scale),
            ))
        };
        assert_eq!(decimal("1250", 2), Cell::Decimal("12.50".into()));
        assert_eq!(decimal("-5", 3), Cell::Decimal("-0.005".into()));
        assert_eq!(decimal("12", -3), Cell::Decimal("12000".into()));
        assert_eq!(decimal("0", 0), Cell::Decimal("0".into()));
        assert_eq!(
            decimal("1234567890123456789012", 2),
            Cell::Decimal("12345678901234567890.12".into())
        );
        assert_eq!(decimal("7", -5000), Cell::Decimal("7E+5000".into()));
        assert_eq!(shown(CqlValue::Float(0.1)), Cell::Float("0.1".into()));
        // A very large or very small number is not written out digit by
        // digit: three hundred zeros say nothing.
        assert_eq!(
            shown(CqlValue::Double(1.5e300)),
            Cell::Float("1.5e300".into())
        );
        assert_eq!(
            shown(CqlValue::Double(-2.5e-9)),
            Cell::Float("-2.5e-9".into())
        );
        assert_eq!(shown(CqlValue::Float(1e30)), Cell::Float("1e30".into()));
        assert_eq!(
            shown(CqlValue::Double(1234567.25)),
            Cell::Float("1234567.25".into())
        );
        assert_eq!(shown(CqlValue::Double(0.0)), Cell::Float("0".into()));
        assert_eq!(shown(CqlValue::Double(3.0)), Cell::Float("3".into()));
        assert_eq!(shown(CqlValue::Double(f64::NAN)), Cell::Float("NaN".into()));
        assert_eq!(
            shown(CqlValue::Float(f32::NEG_INFINITY)),
            Cell::Float("-Infinity".into())
        );
    }

    #[test]
    fn dates_times_and_durations_are_text_cql_reads_back() {
        assert_eq!(
            shown(CqlValue::Timestamp(CqlTimestamp(1_714_566_896_789))),
            text("2024-05-01T12:34:56.789Z")
        );
        assert_eq!(
            shown(CqlValue::Timestamp(CqlTimestamp(1_714_566_896_000))),
            text("2024-05-01T12:34:56Z")
        );
        assert_eq!(
            shown(CqlValue::Date(CqlDate((1u32 << 31) + 19_844))),
            text("2024-05-01")
        );
        assert_eq!(shown(CqlValue::Date(CqlDate(1 << 31))), text("1970-01-01"));
        assert_eq!(
            shown(CqlValue::Time(CqlTime(45_296_000_000_000))),
            text("12:34:56")
        );
        assert_eq!(
            shown(CqlValue::Time(CqlTime(45_296_123_000_000))),
            text("12:34:56.123")
        );
        assert_eq!(
            shown(CqlValue::Time(CqlTime(1))),
            text("00:00:00.000000001")
        );
        let duration = |months, days, nanoseconds| {
            duration_text(&CqlDuration {
                months,
                days,
                nanoseconds,
            })
        };
        assert_eq!(duration(14, 3, 3_723_004_005_006), "1y2mo3d1h2m3s4ms5us6ns");
        assert_eq!(duration(0, 0, 0), "0s");
        assert_eq!(duration(-1, -2, -1_000_000_000), "-1mo2d1s");
        // What is shown is accepted again.
        for text in [
            "1y2mo3d1h2m3s4ms5us6ns",
            "0s",
            "-1mo2d1s",
            "90m",
            "P1Y2M3DT4H5M6S",
            "P2W",
        ] {
            assert_eq!(super::duration(text).unwrap(), text);
        }
        for text in [
            "",
            "1",
            "h",
            "1h; DROP TABLE t",
            "1 h",
            "1h'",
            "1x",
            "P",
            "P1Y; x",
        ] {
            assert!(super::duration(text).is_err(), "{text}");
        }
        assert_eq!(
            shown(CqlValue::Inet("2001:db8::1".parse().unwrap())),
            text("2001:db8::1")
        );
        assert_eq!(
            shown(CqlValue::Uuid(
                "8E8B1C2A-4F3D-4E7B-9C1A-2B3C4D5E6F70".parse().unwrap()
            )),
            text("8e8b1c2a-4f3d-4e7b-9c1a-2b3c4d5e6f70")
        );
        assert_eq!(
            shown(CqlValue::Blob(vec![0, 255])),
            Cell::Bytes(vec![0, 255])
        );
        assert_eq!(shown(CqlValue::Empty), Cell::Null);
        assert_eq!(cell(None), Cell::Null);
    }

    #[test]
    fn collections_are_compact_json() {
        let list = CqlValue::List(vec![CqlValue::Int(1), CqlValue::Int(2)]);
        assert_eq!(shown(list), Cell::Json("[1,2]".into()));
        let set = CqlValue::Set(vec![CqlValue::Text("a\"b".into())]);
        assert_eq!(shown(set), Cell::Json(r#"["a\"b"]"#.into()));
        let map = CqlValue::Map(vec![
            (CqlValue::Text("k".into()), CqlValue::BigInt(5)),
            (CqlValue::Text("ż".into()), CqlValue::BigInt(-1)),
        ]);
        assert_eq!(shown(map), Cell::Json(r#"{"k":5,"ż":-1}"#.into()));
        // A key that is not text is written the way it is shown.
        let keyed = CqlValue::Map(vec![(
            CqlValue::Int(7),
            CqlValue::List(vec![CqlValue::Boolean(true)]),
        )]);
        assert_eq!(shown(keyed), Cell::Json(r#"{"7":[true]}"#.into()));
        let tuple = CqlValue::Tuple(vec![Some(CqlValue::Int(1)), None]);
        assert_eq!(shown(tuple), Cell::Json("[1,null]".into()));
        let udt = CqlValue::UserDefinedType {
            keyspace: "ks".into(),
            name: "address".into(),
            fields: vec![
                ("street".into(), Some(CqlValue::Text("Hoża 1".into()))),
                ("zip".into(), None),
                ("at".into(), Some(CqlValue::Timestamp(CqlTimestamp(0)))),
            ],
        };
        assert_eq!(
            shown(udt),
            Cell::Json(r#"{"street":"Hoża 1","zip":null,"at":"1970-01-01T00:00:00Z"}"#.into())
        );
        let vector = CqlValue::Vector(vec![CqlValue::Float(0.5), CqlValue::Float(-1.25)]);
        assert_eq!(shown(vector), Cell::Json("[0.5,-1.25]".into()));
        let blobs = CqlValue::List(vec![CqlValue::Blob(vec![1, 2]), CqlValue::Double(f64::NAN)]);
        assert_eq!(shown(blobs), Cell::Json(r#"["0x0102","NaN"]"#.into()));
    }

    #[test]
    fn a_literal_is_written_for_the_type_of_its_column() {
        assert_eq!(written("text", text("it's")).unwrap(), "'it''s'");
        assert_eq!(written("text", Cell::Integer("5".into())).unwrap(), "'5'");
        assert_eq!(written("int", text(" 42 ")).unwrap(), "42");
        assert_eq!(written("int", text("5.0")).unwrap(), "5");
        assert_eq!(
            written("int", text("2147483648")).unwrap_err(),
            "integer_invalid"
        );
        assert_eq!(
            written("tinyint", text("-129")).unwrap_err(),
            "integer_invalid"
        );
        assert_eq!(
            written("int", text("1; DROP")).unwrap_err(),
            "integer_invalid"
        );
        assert_eq!(
            written("varint", text("123456789012345678901234567890123456789012")).unwrap(),
            "123456789012345678901234567890123456789012"
        );
        assert_eq!(
            written("decimal", Cell::Decimal("12345678901234567890.12".into())).unwrap(),
            "12345678901234567890.12"
        );
        assert_eq!(
            written("decimal", text("1 OR 1")).unwrap_err(),
            "decimal_invalid"
        );
        assert_eq!(written("double", text("1e10")).unwrap(), "1e10");
        assert_eq!(written("float", text("nan")).unwrap(), "NaN");
        assert_eq!(written("double", text("-Infinity")).unwrap(), "-Infinity");
        assert_eq!(written("double", text("abc")).unwrap_err(), "float_invalid");
        assert_eq!(written("boolean", Cell::Bool(true)).unwrap(), "true");
        assert_eq!(written("boolean", text("FALSE")).unwrap(), "false");
        // A uuid stands without quotes, so it is checked first.
        assert_eq!(
            written("uuid", text("8E8B1C2A-4F3D-4E7B-9C1A-2B3C4D5E6F70")).unwrap(),
            "8e8b1c2a-4f3d-4e7b-9c1a-2b3c4d5e6f70"
        );
        assert_eq!(
            written("uuid", text("1 OR true")).unwrap_err(),
            "cassandra_value_invalid"
        );
        assert_eq!(
            written("timestamp", text("2024-05-01T12:34:56.789Z")).unwrap(),
            "'2024-05-01T12:34:56.789Z'"
        );
        assert_eq!(
            written("timestamp", text("2024-05-01 14:34:56+0200")).unwrap(),
            "'2024-05-01T12:34:56.000Z'"
        );
        // Without a zone it is UTC, as the grid shows it.
        assert_eq!(
            written("timestamp", text("2024-05-01 12:34")).unwrap(),
            "'2024-05-01T12:34:00.000Z'"
        );
        assert_eq!(
            written("timestamp", text("2024-05-01")).unwrap(),
            "'2024-05-01T00:00:00.000Z'"
        );
        assert_eq!(
            written("timestamp", text("1714566896789")).unwrap(),
            "1714566896789"
        );
        assert_eq!(
            written("timestamp", text("yesterday")).unwrap_err(),
            "cassandra_value_invalid"
        );
        assert_eq!(written("date", text("2024-05-01")).unwrap(), "'2024-05-01'");
        assert_eq!(
            written("date", text("2024-13-01")).unwrap_err(),
            "cassandra_value_invalid"
        );
        assert_eq!(
            written("time", text("12:34:56.123")).unwrap(),
            "'12:34:56.123'"
        );
        assert_eq!(written("inet", text("::1")).unwrap(), "'::1'");
        assert_eq!(
            written("inet", text("localhost")).unwrap_err(),
            "cassandra_value_invalid"
        );
        assert_eq!(written("duration", text("1h30m")).unwrap(), "1h30m");
        assert_eq!(
            written("blob", Cell::Bytes(vec![0, 255])).unwrap(),
            "0x00ff"
        );
        assert_eq!(written("blob", text("0xCAFE")).unwrap(), "0xcafe");
        assert_eq!(
            written("blob", text("xyz")).unwrap_err(),
            "cassandra_value_invalid"
        );
        assert_eq!(
            written("ascii", text("zażółć")).unwrap_err(),
            "cassandra_value_invalid"
        );
        assert_eq!(written("int", Cell::Null).unwrap(), "null");
        assert_eq!(
            written("int", Cell::Unsupported("x".into())).unwrap_err(),
            "unsupported_cell_type"
        );
    }

    #[test]
    fn json_is_written_back_as_a_collection_literal() {
        let json = |value: &str| Cell::Json(value.into());
        assert_eq!(written("list<int>", json("[1, 2,3]")).unwrap(), "[1, 2, 3]");
        assert_eq!(
            written("frozen<set<text>>", json(r#"["a","it's"]"#)).unwrap(),
            "{'a', 'it''s'}"
        );
        assert_eq!(
            written("map<text, bigint>", json(r#"{"k":5,"ż":-1}"#)).unwrap(),
            "{'k': 5, 'ż': -1}"
        );
        assert_eq!(
            written("map<int, frozen<list<boolean>>>", json(r#"{"7":[true]}"#)).unwrap(),
            "{7: [true]}"
        );
        assert_eq!(
            written(
                "map<uuid, text>",
                json(r#"[["8e8b1c2a-4f3d-4e7b-9c1a-2b3c4d5e6f70","x"]]"#)
            )
            .unwrap(),
            "{8e8b1c2a-4f3d-4e7b-9c1a-2b3c4d5e6f70: 'x'}"
        );
        assert_eq!(
            written("tuple<int, text>", json(r#"[1,null]"#)).unwrap(),
            "(1, null)"
        );
        assert_eq!(
            written("vector<float, 2>", json("[0.5,-1.25]")).unwrap(),
            "[0.5, -1.25]"
        );
        assert_eq!(
            written("vector<float, 3>", json("[0.5,-1.25]")).unwrap_err(),
            "cassandra_value_invalid"
        );
        assert_eq!(written("list<int>", json("{")).unwrap_err(), "json_invalid");
        assert_eq!(
            written("list<int>", json(r#"["1; DROP"]"#)).unwrap_err(),
            "integer_invalid"
        );
        assert_eq!(
            written("list<int>", json("[1,null]")).unwrap_err(),
            "cassandra_value_invalid"
        );
        assert_eq!(written("set<int>", json("[]")).unwrap(), "{}");
        // Digits beyond a double survive the JSON reader.
        assert_eq!(
            written("list<decimal>", json("[12345678901234567890.123456789]")).unwrap(),
            "[12345678901234567890.123456789]"
        );
        let mut udts = Udts::new();
        udts.insert(
            "address".into(),
            vec![
                ("street".into(), CqlType::parse("text")),
                ("Zip Code".into(), CqlType::parse("int")),
                ("tags".into(), CqlType::parse("frozen<set<text>>")),
            ],
        );
        let ty = CqlType::parse("frozen<address>");
        let udt = |value: &str| {
            literal(&ty, &Cell::Json(value.into()), &udts).map_err(|error| error.to_string())
        };
        assert_eq!(
            udt(r#"{"street":"Hoża 1","Zip Code":681,"tags":["a"]}"#).unwrap(),
            r#"{"street": 'Hoża 1', "Zip Code": 681, "tags": {'a'}}"#
        );
        assert_eq!(udt(r#"{"street":null}"#).unwrap(), r#"{"street": null}"#);
        assert_eq!(udt(r#"{"nope":1}"#).unwrap_err(), "cassandra_value_invalid");
        assert_eq!(
            written("frozen<unknown_type>", json("{}")).unwrap_err(),
            "cassandra_type_unknown"
        );
        assert_eq!(
            written("'org.example.Custom'", text("x")).unwrap_err(),
            "cassandra_type_unsupported"
        );
    }

    #[test]
    fn names_are_quoted_exactly() {
        assert_eq!(name("Order Items").unwrap(), "\"Order Items\"");
        assert_eq!(name("a\"b").unwrap(), "\"a\"\"b\"");
        assert!(name("").is_err() && name("a\nb").is_err());
        assert_eq!(qualified("ks", "t").unwrap(), "\"ks\".\"t\"");
        assert_eq!(
            qualified("", "t").unwrap_err().to_string(),
            "cassandra_keyspace_required"
        );
    }
}
