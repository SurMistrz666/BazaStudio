//! Values of Oracle as cells: what a column of a result holds and how the
//! grid shows it.
//!
//! Text is what the session writes and reads back: the connection sets its
//! date and timestamp formats to the ones used here, so a value shown in the
//! grid is a value Oracle converts back without a format of its own.
use crate::model::Cell;
use oracledb::{
    DbType, Error, JsonValue, Lob, Metadata, OracleIntervalDS, OracleIntervalYM, OracleNumber,
    OracleTimestamp, Row, Vector, VectorData,
};
use std::fmt::Write;
use std::io::Read;

/// The formats of the session, set once per connection. `TO_CHAR` and the
/// conversion of a text to a date then agree with what the grid shows.
pub(super) const SESSION_SETUP: &str = "BEGIN \
EXECUTE IMMEDIATE q'[ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD HH24:MI:SS' \
NLS_TIMESTAMP_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF' \
NLS_TIMESTAMP_TZ_FORMAT = 'YYYY-MM-DD HH24:MI:SS.FF TZH:TZM' \
NLS_NUMERIC_CHARACTERS = '. ']'; \
EXECUTE IMMEDIATE 'ALTER SESSION SET TIME_ZONE = DBTIMEZONE'; \
END;";

/// A `CLOB`, `NCLOB` or `BLOB` up to this many characters or bytes is read
/// whole; a longer one is shown as its type and length and is not edited.
pub(super) const LOB_LIMIT: usize = 256 * 1024;

/// What a column of a result holds, as far as reading it goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// `NUMBER(p)` and `NUMBER(p, s)` with a scale of zero or less.
    Integer,
    Decimal,
    BinaryFloat,
    BinaryDouble,
    Text,
    Date,
    /// `TIMESTAMP` and `TIMESTAMP WITH LOCAL TIME ZONE`: the fields as the
    /// server sends them, which is the time zone of the session.
    Timestamp,
    TimestampTz,
    IntervalYm,
    IntervalDs,
    Raw,
    CharacterLob,
    BinaryLob,
    Json,
    Boolean,
    Vector,
    Unsupported,
}

/// A column of a result: how to read it and how to write what was read.
#[derive(Clone, Debug)]
pub(super) struct Column {
    pub(super) kind: Kind,
    /// Digits of the year or the day of an interval.
    leading: u8,
    /// Digits of the fraction of a second.
    fraction: u8,
    /// The type as a table definition names it.
    pub(super) data_type: String,
}

fn is(found: &'static DbType, wanted: &'static DbType) -> bool {
    found == wanted
}

impl Column {
    pub(super) fn of(metadata: &Metadata) -> Self {
        let found = metadata.db_type();
        let kind = if is(found, oracledb::DB_TYPE_NUMBER) {
            // A precision of zero is a NUMBER without one: any number.
            if metadata.precision() > 0 && metadata.scale() <= 0 && metadata.scale() > -127 {
                Kind::Integer
            } else {
                Kind::Decimal
            }
        } else if is(found, oracledb::DB_TYPE_BINARY_INTEGER) {
            Kind::Integer
        } else if is(found, oracledb::DB_TYPE_BINARY_FLOAT) {
            Kind::BinaryFloat
        } else if is(found, oracledb::DB_TYPE_BINARY_DOUBLE) {
            Kind::BinaryDouble
        } else if [
            oracledb::DB_TYPE_VARCHAR,
            oracledb::DB_TYPE_NVARCHAR,
            oracledb::DB_TYPE_CHAR,
            oracledb::DB_TYPE_NCHAR,
            oracledb::DB_TYPE_LONG,
            oracledb::DB_TYPE_LONG_NVARCHAR,
            oracledb::DB_TYPE_ROWID,
            oracledb::DB_TYPE_UROWID,
        ]
        .iter()
        .any(|wanted| is(found, wanted))
        {
            Kind::Text
        } else if is(found, oracledb::DB_TYPE_DATE) {
            Kind::Date
        } else if is(found, oracledb::DB_TYPE_TIMESTAMP)
            || is(found, oracledb::DB_TYPE_TIMESTAMP_LTZ)
        {
            Kind::Timestamp
        } else if is(found, oracledb::DB_TYPE_TIMESTAMP_TZ) {
            Kind::TimestampTz
        } else if is(found, oracledb::DB_TYPE_INTERVAL_YM) {
            Kind::IntervalYm
        } else if is(found, oracledb::DB_TYPE_INTERVAL_DS) {
            Kind::IntervalDs
        } else if is(found, oracledb::DB_TYPE_RAW) || is(found, oracledb::DB_TYPE_LONG_RAW) {
            Kind::Raw
        } else if is(found, oracledb::DB_TYPE_CLOB) || is(found, oracledb::DB_TYPE_NCLOB) {
            Kind::CharacterLob
        } else if is(found, oracledb::DB_TYPE_BLOB) {
            Kind::BinaryLob
        } else if is(found, oracledb::DB_TYPE_JSON) {
            Kind::Json
        } else if is(found, oracledb::DB_TYPE_BOOLEAN) {
            Kind::Boolean
        } else if is(found, oracledb::DB_TYPE_VECTOR) {
            Kind::Vector
        } else {
            Kind::Unsupported
        };
        Self {
            kind,
            leading: metadata.precision(),
            fraction: u8::try_from(metadata.scale()).unwrap_or(0),
            data_type: metadata.data_type(),
        }
    }

    /// Read the value at `index` of a row. A large object costs a request
    /// to the server of its own.
    pub(super) fn cell(&self, row: &mut Row, index: usize) -> Result<Cell, Error> {
        Ok(match self.kind {
            Kind::Integer => row
                .get::<Option<OracleNumber>>(index)?
                .map_or(Cell::Null, |number| Cell::Integer(number.to_string())),
            Kind::Decimal => row
                .get::<Option<OracleNumber>>(index)?
                .map_or(Cell::Null, |number| Cell::Decimal(number.to_string())),
            Kind::BinaryFloat => row.get::<Option<f32>>(index)?.map_or(Cell::Null, |value| {
                binary(f64::from(value), value.to_string())
            }),
            Kind::BinaryDouble => row
                .get::<Option<f64>>(index)?
                .map_or(Cell::Null, |value| binary(value, value.to_string())),
            Kind::Text => row
                .take::<Option<String>>(index)?
                .map_or(Cell::Null, Cell::Text),
            Kind::Date => row
                .get::<Option<OracleTimestamp>>(index)?
                .map_or(Cell::Null, |value| Cell::Text(date_time(&value, None))),
            Kind::Timestamp => row
                .get::<Option<OracleTimestamp>>(index)?
                .map_or(Cell::Null, |value| {
                    Cell::Text(date_time(&value, Some(value.nanoseconds())))
                }),
            Kind::TimestampTz => {
                row.get::<Option<OracleTimestamp>>(index)?
                    .map_or(Cell::Null, |value| {
                        zoned(&value).map_or_else(
                            || Cell::Unsupported("<TIMESTAMP WITH TIME ZONE>".into()),
                            Cell::Text,
                        )
                    })
            }
            Kind::IntervalYm => row
                .get::<Option<OracleIntervalYM>>(index)?
                .map_or(Cell::Null, |value| {
                    Cell::Text(year_month(value.years(), value.months(), self.leading))
                }),
            Kind::IntervalDs => row
                .get::<Option<OracleIntervalDS>>(index)?
                .map_or(Cell::Null, |value| {
                    Cell::Text(day_second(&value, self.leading, self.fraction))
                }),
            Kind::Raw => row
                .take::<Option<Vec<u8>>>(index)?
                .map_or(Cell::Null, Cell::Bytes),
            Kind::CharacterLob | Kind::BinaryLob => match row.take::<Option<Lob>>(index)? {
                None => Cell::Null,
                Some(lob) => self.large(lob)?,
            },
            Kind::Json => row
                .get::<Option<JsonValue>>(index)?
                .map_or(Cell::Null, |value| {
                    let mut text = String::new();
                    json(&value, &mut text);
                    Cell::Json(text)
                }),
            Kind::Boolean => row
                .get::<Option<bool>>(index)?
                .map_or(Cell::Null, Cell::Bool),
            Kind::Vector => row
                .get::<Option<Vector>>(index)?
                .map_or(Cell::Null, |value| match vector(&value) {
                    Some(text) => Cell::Json(text),
                    None => Cell::Unsupported("<VECTOR>".into()),
                }),
            Kind::Unsupported => Cell::Unsupported(format!("<{}>", self.data_type)),
        })
    }

    /// A large object: its content when it is small enough to be shown and
    /// edited, otherwise its type and its length.
    fn large(&self, mut lob: Lob) -> Result<Cell, Error> {
        let size = lob.get_size()?;
        if size > LOB_LIMIT {
            return Ok(Cell::Unsupported(format!("<{} {size}>", self.data_type)));
        }
        if self.kind == Kind::BinaryLob {
            let mut bytes = vec![0u8; size];
            let mut filled = 0;
            while filled < size {
                let read = lob.read(&mut bytes[filled..])?;
                if read == 0 {
                    break;
                }
                filled += read;
            }
            bytes.truncate(filled);
            return Ok(Cell::Bytes(bytes));
        }
        // The length counts UTF-16 units, each at most three bytes of UTF-8.
        let mut text = Vec::with_capacity(size * 3);
        let mut buffer = vec![0u8; (size * 3).max(64)];
        loop {
            let read = lob.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            text.extend_from_slice(&buffer[..read]);
        }
        Ok(match String::from_utf8(text) {
            Ok(text) => Cell::Text(text),
            Err(_) => Cell::Unsupported(format!("<{}>", self.data_type)),
        })
    }
}

/// `Inf`, `-Inf` and `Nan` are how Oracle writes what is not a number, and
/// what it reads back; the grid has no number for them.
fn binary(value: f64, text: String) -> Cell {
    if value.is_nan() {
        Cell::Text("Nan".into())
    } else if value.is_infinite() {
        Cell::Text(if value > 0.0 { "Inf" } else { "-Inf" }.into())
    } else {
        Cell::Float(text)
    }
}

/// `2024-02-29 13:14:15`, with the fraction of a second when there is one
/// to show. A year before the common era keeps its sign.
fn date_time(value: &OracleTimestamp, nanoseconds: Option<u32>) -> String {
    let mut text = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        value.year(),
        value.month(),
        value.day(),
        value.hour(),
        value.minute(),
        value.second()
    );
    if let Some(nanoseconds) = nanoseconds.filter(|nanoseconds| *nanoseconds > 0) {
        let digits = format!("{nanoseconds:09}");
        let _ = write!(text, ".{}", digits.trim_end_matches('0'));
    }
    text
}

/// A timestamp with its time zone: the local time and the offset. The
/// server sends the time in UTC next to the offset.
fn zoned(value: &OracleTimestamp) -> Option<String> {
    let (hours, minutes) = (
        i64::from(value.tz_hour_offset()),
        i64::from(value.tz_minute_offset()),
    );
    let utc = chrono::NaiveDate::from_ymd_opt(
        i32::from(value.year()),
        u32::from(value.month()),
        u32::from(value.day()),
    )?
    .and_hms_nano_opt(
        u32::from(value.hour()),
        u32::from(value.minute()),
        u32::from(value.second()),
        value.nanoseconds(),
    )?;
    let offset = hours * 60 + minutes;
    let local = utc.checked_add_signed(chrono::Duration::minutes(offset))?;
    let mut text = local.format("%Y-%m-%d %H:%M:%S").to_string();
    if value.nanoseconds() > 0 {
        let digits = format!("{:09}", value.nanoseconds());
        let _ = write!(text, ".{}", digits.trim_end_matches('0'));
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let _ = write!(text, " {sign}{:02}:{:02}", hours.abs(), minutes.abs());
    Some(text)
}

/// `+01-02`, as `TO_CHAR` writes an interval of years and months.
fn year_month(years: i32, months: i8, digits: u8) -> String {
    let sign = if years < 0 || months < 0 { '-' } else { '+' };
    format!(
        "{sign}{:0width$}-{:02}",
        years.unsigned_abs(),
        months.unsigned_abs(),
        width = usize::from(digits.max(1))
    )
}

/// `+03 04:05:06.789000`, as `TO_CHAR` writes an interval of days and
/// seconds: the days and the fraction in the digits of the column.
fn day_second(value: &OracleIntervalDS, days: u8, fraction: u8) -> String {
    let negative = value.days() < 0
        || value.hours() < 0
        || value.minutes() < 0
        || value.seconds() < 0
        || value.nanoseconds() < 0;
    let mut text = format!(
        "{}{:0width$} {:02}:{:02}:{:02}",
        if negative { '-' } else { '+' },
        value.days().unsigned_abs(),
        value.hours().unsigned_abs(),
        value.minutes().unsigned_abs(),
        value.seconds().unsigned_abs(),
        width = usize::from(days.max(1))
    );
    let fraction = usize::from(fraction.min(9));
    if fraction > 0 {
        let digits = format!("{:09}", value.nanoseconds().unsigned_abs());
        let _ = write!(text, ".{}", &digits[..fraction]);
    }
    text
}

fn json_text(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

fn json_float(value: f64, out: &mut String) {
    // JSON has no word for what is not a number.
    if value.is_finite() {
        let _ = write!(out, "{value}");
    } else {
        out.push_str("null");
    }
}

/// A JSON value of Oracle as JSON text. A number keeps every digit; the
/// members of an object are written in the order of their names, because
/// the client library does not keep the order of the document.
fn json(value: &JsonValue, out: &mut String) {
    match value {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Boolean(value) => out.push_str(if *value { "true" } else { "false" }),
        JsonValue::Number(number) => out.push_str(&number.to_string()),
        JsonValue::BinaryDouble(value) => json_float(*value, out),
        JsonValue::BinaryFloat(value) => json_float(f64::from(*value), out),
        JsonValue::String(text) => json_text(text, out),
        JsonValue::Timestamp(value) => json_text(
            &date_time(value, Some(value.nanoseconds())).replacen(' ', "T", 1),
            out,
        ),
        JsonValue::IntervalYM(value) => {
            json_text(&year_month(value.years(), value.months(), 1), out);
        }
        JsonValue::IntervalDS(value) => json_text(&day_second(value, 1, 9), out),
        JsonValue::Raw(bytes) | JsonValue::JsonId(bytes) => json_text(
            &bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            out,
        ),
        JsonValue::Vector(value) => match vector(value) {
            Some(text) => out.push_str(&text),
            None => out.push_str("null"),
        },
        JsonValue::JsonArray(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                json(item, out);
            }
            out.push(']');
        }
        JsonValue::JsonObject(members) => {
            let mut names: Vec<&String> = members.keys().collect();
            names.sort();
            out.push('{');
            for (index, name) in names.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                json_text(name, out);
                out.push(':');
                json(&members[name], out);
            }
            out.push('}');
        }
    }
}

/// A dense vector as the list of numbers Oracle reads back: `[1.5,2,3]`.
/// A sparse one has a text form of its own, which is not written here.
fn vector(value: &Vector) -> Option<String> {
    let Vector::Dense(data) = value else {
        return None;
    };
    let items: Vec<String> = match data {
        VectorData::Float32(items) => items.iter().map(ToString::to_string).collect(),
        VectorData::Float64(items) => items.iter().map(ToString::to_string).collect(),
        VectorData::Int8(items) => items.iter().map(ToString::to_string).collect(),
        VectorData::Binary(items) => items.iter().map(ToString::to_string).collect(),
    };
    Some(format!("[{}]", items.join(",")))
}

/// Size of a row as it counts towards `sql::MAX_RESULT_BYTES`.
pub(super) fn row_bytes(cells: &[Cell]) -> usize {
    cells
        .iter()
        .map(|cell| match cell {
            Cell::Bytes(value) => value.len(),
            Cell::Null => 4,
            Cell::Bool(_) => 5,
            Cell::Integer(value)
            | Cell::Decimal(value)
            | Cell::Float(value)
            | Cell::Text(value)
            | Cell::Json(value)
            | Cell::Unsupported(value) => value.len(),
        })
        .sum()
}

/// The type of a column as the catalog describes it, written the way a
/// table definition writes it.
pub(super) fn declared_type(
    data_type: &str,
    length: Option<i64>,
    characters: Option<i64>,
    character_semantics: bool,
    precision: Option<i64>,
    scale: Option<i64>,
) -> String {
    match data_type {
        "VARCHAR2" | "CHAR" => match (characters, character_semantics) {
            (Some(characters), true) => format!("{data_type}({characters} CHAR)"),
            _ => format!("{data_type}({})", length.unwrap_or(1)),
        },
        "NVARCHAR2" | "NCHAR" => format!("{data_type}({})", characters.or(length).unwrap_or(1)),
        "RAW" => format!("RAW({})", length.unwrap_or(1)),
        "NUMBER" => match (precision, scale) {
            (Some(precision), Some(0) | None) => format!("NUMBER({precision})"),
            (Some(precision), Some(scale)) => format!("NUMBER({precision},{scale})"),
            (None, Some(scale)) if scale != 0 => format!("NUMBER(*,{scale})"),
            _ => "NUMBER".into(),
        },
        "FLOAT" => match precision {
            Some(precision) if precision != 126 => format!("FLOAT({precision})"),
            _ => "FLOAT".into(),
        },
        // Timestamps and intervals carry their digits in the name already.
        other => other.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_written_the_way_the_session_reads_them() {
        let date = OracleTimestamp::new_date(2024, 2, 29);
        assert_eq!(date_time(&date, None), "2024-02-29 00:00:00");
        let stamp = OracleTimestamp::new_timestamp(2024, 2, 29, 13, 14, 15, 123_400_000);
        assert_eq!(date_time(&stamp, None), "2024-02-29 13:14:15");
        assert_eq!(
            date_time(&stamp, Some(stamp.nanoseconds())),
            "2024-02-29 13:14:15.1234"
        );
        let whole = OracleTimestamp::new_timestamp(1, 1, 1, 0, 0, 0, 0);
        assert_eq!(date_time(&whole, Some(0)), "0001-01-01 00:00:00");
        // The fields of a zoned timestamp are UTC; the offset moves them.
        let east = OracleTimestamp::new_timestamp_tz(2024, 2, 29, 7, 44, 15, 500_000_000, 5, 30);
        assert_eq!(zoned(&east).unwrap(), "2024-02-29 13:14:15.5 +05:30");
        let west = OracleTimestamp::new_timestamp_tz(2024, 3, 1, 1, 0, 0, 0, -8, 0);
        assert_eq!(zoned(&west).unwrap(), "2024-02-29 17:00:00 -08:00");
        let utc = OracleTimestamp::new_timestamp_tz(2024, 12, 31, 23, 59, 59, 1, 0, 0);
        assert_eq!(zoned(&utc).unwrap(), "2024-12-31 23:59:59.000000001 +00:00");
    }

    #[test]
    fn intervals_are_written_like_to_char_writes_them() {
        assert_eq!(year_month(1, 2, 2), "+01-02");
        assert_eq!(year_month(-123, -11, 3), "-123-11");
        assert_eq!(year_month(0, 0, 2), "+00-00");
        let positive = OracleIntervalDS::new(3, 4, 5, 6, 789_000_000);
        assert_eq!(day_second(&positive, 2, 6), "+03 04:05:06.789000");
        assert_eq!(day_second(&positive, 2, 0), "+03 04:05:06");
        let negative = OracleIntervalDS::new(-3, -4, -5, -6, -789_000_000);
        assert_eq!(day_second(&negative, 2, 3), "-03 04:05:06.789");
        let long = OracleIntervalDS::new(100_000, 0, 0, 0, 1);
        assert_eq!(day_second(&long, 6, 9), "+100000 00:00:00.000000001");
    }

    #[test]
    fn what_is_not_a_number_is_text_oracle_reads_back() {
        assert_eq!(binary(1.5, "1.5".into()), Cell::Float("1.5".into()));
        assert_eq!(binary(f64::NAN, "NaN".into()), Cell::Text("Nan".into()));
        assert_eq!(
            binary(f64::INFINITY, "inf".into()),
            Cell::Text("Inf".into())
        );
        assert_eq!(
            binary(f64::NEG_INFINITY, "-inf".into()),
            Cell::Text("-Inf".into())
        );
        // A single-precision number keeps its own shortest text.
        assert_eq!(1.1f32.to_string(), "1.1");
    }

    #[test]
    fn json_keeps_its_digits_and_escapes_its_text() {
        let mut members = std::collections::HashMap::new();
        members.insert(
            "b".to_owned(),
            JsonValue::Number("12345678901234567890.125".parse().unwrap()),
        );
        members.insert(
            "a".to_owned(),
            JsonValue::JsonArray(vec![
                JsonValue::Null,
                JsonValue::Boolean(true),
                JsonValue::String("zażółć \"x\"\n\\".into()),
                JsonValue::BinaryDouble(1.5),
            ]),
        );
        let mut text = String::new();
        json(&JsonValue::JsonObject(members), &mut text);
        assert_eq!(
            text,
            "{\"a\":[null,true,\"zażółć \\\"x\\\"\\n\\\\\",1.5],\"b\":12345678901234567890.125}"
        );
        assert!(serde_json::from_str::<serde_json::Value>(&text).is_ok());
        assert_eq!(
            vector(&Vector::Dense(VectorData::Float32(vec![1.5, 2.0, -3.25]))).unwrap(),
            "[1.5,2,-3.25]"
        );
    }

    #[test]
    fn a_type_is_named_as_a_table_definition_names_it() {
        let named = |data_type: &str, length, characters, semantics, precision, scale| {
            declared_type(data_type, length, characters, semantics, precision, scale)
        };
        assert_eq!(
            named("VARCHAR2", Some(1020), Some(255), true, None, None),
            "VARCHAR2(255 CHAR)"
        );
        assert_eq!(
            named("VARCHAR2", Some(50), Some(50), false, None, None),
            "VARCHAR2(50)"
        );
        assert_eq!(
            named("NVARCHAR2", Some(40), Some(20), true, None, None),
            "NVARCHAR2(20)"
        );
        assert_eq!(
            named("CHAR", Some(5), Some(5), false, None, None),
            "CHAR(5)"
        );
        assert_eq!(named("RAW", Some(16), None, false, None, None), "RAW(16)");
        assert_eq!(named("NUMBER", Some(22), None, false, None, None), "NUMBER");
        assert_eq!(
            named("NUMBER", Some(22), None, false, Some(19), Some(0)),
            "NUMBER(19)"
        );
        assert_eq!(
            named("NUMBER", Some(22), None, false, Some(38), Some(10)),
            "NUMBER(38,10)"
        );
        assert_eq!(
            named("FLOAT", Some(22), None, false, Some(126), None),
            "FLOAT"
        );
        assert_eq!(
            named("FLOAT", Some(22), None, false, Some(53), None),
            "FLOAT(53)"
        );
        assert_eq!(
            named(
                "TIMESTAMP(6) WITH TIME ZONE",
                Some(13),
                None,
                false,
                None,
                Some(6)
            ),
            "TIMESTAMP(6) WITH TIME ZONE"
        );
        assert_eq!(named("CLOB", Some(4000), None, false, None, None), "CLOB");
    }
}
