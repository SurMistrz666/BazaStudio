//! Values of DuckDB as cells.
//!
//! A result arrives as Arrow arrays, which DuckDB fills itself. Numbers
//! keep the text of their digits (a HUGEINT and a DECIMAL(38, …) arrive as
//! 128-bit integers with a scale), dates and times are written the way
//! DuckDB writes them, and a list, a struct or a map becomes the text
//! DuckDB shows for it, which it also reads back.
use crate::model::Cell;
use duckdb::arrow::array::{
    Array, ArrayRef, AsArray, BooleanArray, FixedSizeListArray, GenericListArray, MapArray,
    OffsetSizeTrait, StructArray, UnionArray,
};
use duckdb::arrow::datatypes::{
    DataType, Date32Type, Decimal128Type, Decimal256Type, Float16Type, Float32Type, Float64Type,
    Int8Type, Int16Type, Int32Type, Int64Type, IntervalDayTimeType, IntervalMonthDayNanoType,
    IntervalUnit, IntervalYearMonthType, Time32MillisecondType, Time32SecondType,
    Time64MicrosecondType, Time64NanosecondType, TimeUnit, TimestampMicrosecondType,
    TimestampMillisecondType, TimestampNanosecondType, TimestampSecondType, UInt8Type, UInt16Type,
    UInt32Type, UInt64Type,
};
use duckdb::core::{LogicalTypeHandle, LogicalTypeId};

/// What the logical type of a column says beyond its Arrow type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hint {
    None,
    /// A HUGEINT: a whole number, though it arrives as a decimal.
    Whole,
    /// A UHUGEINT: the same, and its 128 bits have no sign.
    Unsigned,
    /// A VARCHAR that holds a JSON document.
    Json,
    /// A BIT: the bits behind a count of padding bits.
    Bits,
    /// A value that arrives in a form of DuckDB's own (a BIGNUM): named,
    /// never shown as bytes.
    Opaque,
}

/// The name DuckDB gives a logical type, and what to remember about it.
pub(super) fn describe(logical: &LogicalTypeHandle) -> (String, Hint) {
    let hint = match logical.id() {
        LogicalTypeId::Hugeint => Hint::Whole,
        LogicalTypeId::UHugeint => Hint::Unsigned,
        LogicalTypeId::Bit => Hint::Bits,
        LogicalTypeId::Bignum | LogicalTypeId::Variant => Hint::Opaque,
        LogicalTypeId::Varchar
            if logical
                .get_alias()
                .is_some_and(|alias| alias.eq_ignore_ascii_case("json")) =>
        {
            Hint::Json
        }
        _ => Hint::None,
    };
    (type_name(logical, 0), hint)
}

fn type_name(logical: &LogicalTypeHandle, depth: usize) -> String {
    // A type cannot nest deeper than this in a statement a person writes.
    if depth > 16 {
        return "…".into();
    }
    let child = |index: usize| {
        if index < logical.num_children() {
            type_name(&logical.child(index), depth + 1)
        } else {
            "?".into()
        }
    };
    if let Some(alias) = logical.get_alias().filter(|alias| !alias.is_empty()) {
        return alias.to_uppercase();
    }
    match logical.id() {
        LogicalTypeId::Boolean => "BOOLEAN".into(),
        LogicalTypeId::Tinyint => "TINYINT".into(),
        LogicalTypeId::Smallint => "SMALLINT".into(),
        LogicalTypeId::Integer => "INTEGER".into(),
        LogicalTypeId::Bigint => "BIGINT".into(),
        LogicalTypeId::Hugeint => "HUGEINT".into(),
        LogicalTypeId::UTinyint => "UTINYINT".into(),
        LogicalTypeId::USmallint => "USMALLINT".into(),
        LogicalTypeId::UInteger => "UINTEGER".into(),
        LogicalTypeId::UBigint => "UBIGINT".into(),
        LogicalTypeId::UHugeint => "UHUGEINT".into(),
        LogicalTypeId::Float => "FLOAT".into(),
        LogicalTypeId::Double => "DOUBLE".into(),
        LogicalTypeId::Decimal => format!(
            "DECIMAL({},{})",
            logical.decimal_width(),
            logical.decimal_scale()
        ),
        LogicalTypeId::Varchar | LogicalTypeId::StringLiteral => "VARCHAR".into(),
        LogicalTypeId::Blob => "BLOB".into(),
        LogicalTypeId::Bit => "BIT".into(),
        LogicalTypeId::Date => "DATE".into(),
        LogicalTypeId::Time => "TIME".into(),
        LogicalTypeId::TimeNs => "TIME_NS".into(),
        LogicalTypeId::TimeTZ => "TIME WITH TIME ZONE".into(),
        LogicalTypeId::Timestamp => "TIMESTAMP".into(),
        LogicalTypeId::TimestampS => "TIMESTAMP_S".into(),
        LogicalTypeId::TimestampMs => "TIMESTAMP_MS".into(),
        LogicalTypeId::TimestampNs => "TIMESTAMP_NS".into(),
        LogicalTypeId::TimestampTZ => "TIMESTAMP WITH TIME ZONE".into(),
        LogicalTypeId::Interval => "INTERVAL".into(),
        LogicalTypeId::Uuid => "UUID".into(),
        LogicalTypeId::Enum => "ENUM".into(),
        LogicalTypeId::Bignum => "BIGNUM".into(),
        LogicalTypeId::Geometry => "GEOMETRY".into(),
        LogicalTypeId::Variant => "VARIANT".into(),
        LogicalTypeId::SqlNull => "NULL".into(),
        LogicalTypeId::IntegerLiteral => "INTEGER".into(),
        LogicalTypeId::List => format!("{}[]", child(0)),
        LogicalTypeId::Array => format!("{}[]", child(0)),
        LogicalTypeId::Map => format!("MAP({}, {})", child(0), child(1)),
        LogicalTypeId::Struct | LogicalTypeId::Union => {
            let fields = (0..logical.num_children())
                .map(|index| format!("{} {}", logical.child_name(index), child(index)))
                .collect::<Vec<_>>()
                .join(", ");
            if logical.id() == LogicalTypeId::Union {
                format!("UNION({fields})")
            } else {
                format!("STRUCT({fields})")
            }
        }
        other => format!("{other:?}").to_uppercase(),
    }
}

/// One value of a column as a cell. `name` is the name of its type.
pub(super) fn cell(array: &dyn Array, row: usize, name: &str, hint: Hint) -> Cell {
    if is_null(array, row) {
        return Cell::Null;
    }
    let unsupported = || Cell::Unsupported(format!("<{name}>"));
    if hint == Hint::Opaque {
        return unsupported();
    }
    match array.data_type() {
        DataType::Null => Cell::Null,
        DataType::Boolean => Cell::Bool(array.as_boolean().value(row)),
        DataType::Int8
        | DataType::Int16
        | DataType::Int32
        | DataType::Int64
        | DataType::UInt8
        | DataType::UInt16
        | DataType::UInt32
        | DataType::UInt64 => text(array, row).map_or_else(unsupported, Cell::Integer),
        DataType::Float16 | DataType::Float32 | DataType::Float64 => {
            text(array, row).map_or_else(unsupported, Cell::Float)
        }
        // All 128 bits of a UHUGEINT are digits.
        DataType::Decimal128(_, 0) if hint == Hint::Unsigned => {
            Cell::Integer((array.as_primitive::<Decimal128Type>().value(row) as u128).to_string())
        }
        DataType::Decimal128(_, _) | DataType::Decimal256(_, _) => {
            let digits = text(array, row);
            match hint {
                Hint::Whole | Hint::Unsigned => digits.map_or_else(unsupported, Cell::Integer),
                _ => digits.map_or_else(unsupported, Cell::Decimal),
            }
        }
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView if hint == Hint::Bits => {
            bytes(array, row)
                .and_then(bits)
                .map_or_else(unsupported, Cell::Text)
        }
        DataType::Binary
        | DataType::LargeBinary
        | DataType::BinaryView
        | DataType::FixedSizeBinary(_) => {
            bytes(array, row).map_or_else(unsupported, |value| Cell::Bytes(value.to_vec()))
        }
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => match text(array, row) {
            Some(value)
                if hint == Hint::Json
                    && serde_json::from_str::<serde_json::Value>(&value).is_ok() =>
            {
                Cell::Json(value)
            }
            Some(value) => Cell::Text(value),
            None => unsupported(),
        },
        _ => text(array, row).map_or_else(unsupported, Cell::Text),
    }
}

/// Whether a value is NULL. A UNION has no NULLs of its own: it is NULL
/// when the member it holds is.
fn is_null(array: &dyn Array, row: usize) -> bool {
    match array.as_any().downcast_ref::<UnionArray>() {
        Some(union) => is_null(
            union.child(union.type_id(row)).as_ref(),
            union.value_offset(row),
        ),
        None => array.is_null(row),
    }
}

fn bytes(array: &dyn Array, row: usize) -> Option<&[u8]> {
    Some(match array.data_type() {
        DataType::Binary => array.as_binary::<i32>().value(row),
        DataType::LargeBinary => array.as_binary::<i64>().value(row),
        DataType::BinaryView => array.as_binary_view().value(row),
        DataType::FixedSizeBinary(_) => array.as_fixed_size_binary().value(row),
        _ => return None,
    })
}

/// The bits of a BIT value: its first byte counts the padding bits at the
/// front of the bytes that follow.
fn bits(stored: &[u8]) -> Option<String> {
    let (padding, data) = stored.split_first()?;
    let all: String = data.iter().map(|byte| format!("{byte:08b}")).collect();
    all.get(usize::from(*padding)..).map(str::to_owned)
}

/// A value as DuckDB writes it. `None` for a type this driver does not know.
fn text(array: &dyn Array, row: usize) -> Option<String> {
    if is_null(array, row) {
        return Some("NULL".into());
    }
    macro_rules! primitive {
        ($kind:ty) => {
            array.as_primitive::<$kind>().value(row)
        };
    }
    Some(match array.data_type() {
        DataType::Null => "NULL".into(),
        DataType::Boolean => array
            .as_any()
            .downcast_ref::<BooleanArray>()?
            .value(row)
            .to_string(),
        DataType::Int8 => primitive!(Int8Type).to_string(),
        DataType::Int16 => primitive!(Int16Type).to_string(),
        DataType::Int32 => primitive!(Int32Type).to_string(),
        DataType::Int64 => primitive!(Int64Type).to_string(),
        DataType::UInt8 => primitive!(UInt8Type).to_string(),
        DataType::UInt16 => primitive!(UInt16Type).to_string(),
        DataType::UInt32 => primitive!(UInt32Type).to_string(),
        DataType::UInt64 => primitive!(UInt64Type).to_string(),
        DataType::Float16 => float(f64::from(primitive!(Float16Type)), |value| {
            format!("{:?}", value as f32)
        }),
        DataType::Float32 => {
            let value = primitive!(Float32Type);
            float(f64::from(value), |_| format!("{value:?}"))
        }
        DataType::Float64 => float(primitive!(Float64Type), |value| format!("{value:?}")),
        DataType::Decimal128(_, scale) => scaled(&primitive!(Decimal128Type).to_string(), *scale),
        DataType::Decimal256(_, scale) => scaled(&primitive!(Decimal256Type).to_string(), *scale),
        DataType::Utf8 => array.as_string::<i32>().value(row).to_owned(),
        DataType::LargeUtf8 => array.as_string::<i64>().value(row).to_owned(),
        DataType::Utf8View => array.as_string_view().value(row).to_owned(),
        DataType::Binary
        | DataType::LargeBinary
        | DataType::BinaryView
        | DataType::FixedSizeBinary(_) => hex(bytes(array, row)?),
        DataType::Date32 => date(primitive!(Date32Type)),
        DataType::Timestamp(unit, zone) => {
            let value = match unit {
                TimeUnit::Second => primitive!(TimestampSecondType),
                TimeUnit::Millisecond => primitive!(TimestampMillisecondType),
                TimeUnit::Microsecond => primitive!(TimestampMicrosecondType),
                TimeUnit::Nanosecond => primitive!(TimestampNanosecondType),
            };
            timestamp(value, *unit, zone.is_some())
        }
        DataType::Time32(TimeUnit::Second) => {
            time_of_day(i128::from(primitive!(Time32SecondType)) * 1_000_000_000)
        }
        DataType::Time32(_) => {
            time_of_day(i128::from(primitive!(Time32MillisecondType)) * 1_000_000)
        }
        DataType::Time64(TimeUnit::Nanosecond) => {
            time_of_day(i128::from(primitive!(Time64NanosecondType)))
        }
        DataType::Time64(_) => time_of_day(i128::from(primitive!(Time64MicrosecondType)) * 1_000),
        DataType::Interval(IntervalUnit::MonthDayNano) => {
            let value = primitive!(IntervalMonthDayNanoType);
            interval(value.months, value.days, i128::from(value.nanoseconds))
        }
        DataType::Interval(IntervalUnit::DayTime) => {
            let value = primitive!(IntervalDayTimeType);
            interval(0, value.days, i128::from(value.milliseconds) * 1_000_000)
        }
        DataType::Interval(IntervalUnit::YearMonth) => {
            interval(primitive!(IntervalYearMonthType), 0, 0)
        }
        // An ENUM: the name behind the number.
        DataType::Dictionary(_, _) => {
            let dictionary = array.as_any_dictionary_opt()?;
            let key = dictionary.normalized_keys().get(row).copied()?;
            text(dictionary.values().as_ref(), key)?
        }
        DataType::List(_) => list(array.as_any().downcast_ref::<GenericListArray<i32>>()?, row)?,
        DataType::LargeList(_) => {
            list(array.as_any().downcast_ref::<GenericListArray<i64>>()?, row)?
        }
        DataType::FixedSizeList(_, _) => {
            let values = array
                .as_any()
                .downcast_ref::<FixedSizeListArray>()?
                .value(row);
            elements(&values)?
        }
        DataType::Struct(_) => {
            let fields = array.as_any().downcast_ref::<StructArray>()?;
            let mut parts = Vec::with_capacity(fields.num_columns());
            for (name, column) in fields.column_names().iter().zip(fields.columns()) {
                parts.push(format!("'{name}': {}", text(column.as_ref(), row)?));
            }
            format!("{{{}}}", parts.join(", "))
        }
        DataType::Map(_, _) => {
            let entries = array.as_any().downcast_ref::<MapArray>()?.value(row);
            let (keys, values) = (entries.column(0), entries.column(1));
            let mut parts = Vec::with_capacity(entries.len());
            for index in 0..entries.len() {
                parts.push(format!(
                    "{}={}",
                    text(keys.as_ref(), index)?,
                    text(values.as_ref(), index)?
                ));
            }
            format!("{{{}}}", parts.join(", "))
        }
        DataType::Union(_, _) => {
            let union = array.as_any().downcast_ref::<UnionArray>()?;
            let member = union.child(union.type_id(row));
            text(member.as_ref(), union.value_offset(row))?
        }
        _ => return None,
    })
}

fn list<O: OffsetSizeTrait>(array: &GenericListArray<O>, row: usize) -> Option<String> {
    elements(&array.value(row))
}

fn elements(values: &ArrayRef) -> Option<String> {
    let mut parts = Vec::with_capacity(values.len());
    for index in 0..values.len() {
        parts.push(text(values.as_ref(), index)?);
    }
    Some(format!("[{}]", parts.join(", ")))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("\\x{byte:02X}")).collect()
}

/// A floating point number in the shortest text that reads back as the
/// same number, with the names DuckDB has for what is not a number.
fn float(value: f64, shortest: impl Fn(f64) -> String) -> String {
    if value.is_nan() {
        "nan".into()
    } else if value.is_infinite() {
        if value > 0.0 { "inf" } else { "-inf" }.into()
    } else {
        shortest(value)
    }
}

/// The digits of an integer with a decimal point `scale` places from the
/// right. Every digit of the scale is kept, as the other drivers show it.
pub(super) fn scaled(digits: &str, scale: i8) -> String {
    let (sign, digits) = match digits.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", digits),
    };
    if scale <= 0 {
        let zeros = "0".repeat(usize::from(scale.unsigned_abs()));
        return format!("{sign}{digits}{zeros}");
    }
    let scale = usize::from(scale.unsigned_abs());
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let (whole, fraction) = padded.split_at(padded.len() - scale);
    format!("{sign}{whole}.{fraction}")
}

/// Days since 1970-01-01 as a date. The two ends of the range are how
/// DuckDB stores `infinity` and `-infinity`.
fn date(days: i32) -> String {
    match days {
        i32::MAX => return "infinity".into(),
        days if days == -i32::MAX => return "-infinity".into(),
        _ => {}
    }
    match chrono::NaiveDate::from_num_days_from_ce_opt(days.saturating_add(719_163)) {
        Some(date) => date.format("%Y-%m-%d").to_string(),
        None => days.to_string(),
    }
}

/// A point in time. One with a time zone is an instant: it is shown in UTC
/// with its offset, which reads back as the same instant whatever the time
/// zone of the session is.
fn timestamp(value: i64, unit: TimeUnit, zoned: bool) -> String {
    match value {
        i64::MAX => return "infinity".into(),
        value if value == -i64::MAX => return "-infinity".into(),
        _ => {}
    }
    let per_second: i64 = match unit {
        TimeUnit::Second => 1,
        TimeUnit::Millisecond => 1_000,
        TimeUnit::Microsecond => 1_000_000,
        TimeUnit::Nanosecond => 1_000_000_000,
    };
    let seconds = value.div_euclid(per_second);
    let fraction = value.rem_euclid(per_second) * (1_000_000_000 / per_second);
    let Some(moment) = chrono::DateTime::from_timestamp(seconds, fraction as u32) else {
        return value.to_string();
    };
    let mut text = moment.format("%Y-%m-%d %H:%M:%S").to_string();
    text.push_str(&fraction_of_second(i128::from(fraction)));
    if zoned {
        text.push_str("+00");
    }
    text
}

/// `.5`, `.123456`: the digits after the seconds, without trailing zeros.
fn fraction_of_second(nanoseconds: i128) -> String {
    if nanoseconds == 0 {
        return String::new();
    }
    let digits = format!("{nanoseconds:09}");
    format!(".{}", digits.trim_end_matches('0'))
}

/// Nanoseconds as `HH:MM:SS` with a fraction; the hours go on beyond 24
/// for the time part of an interval.
fn time_of_day(nanoseconds: i128) -> String {
    let sign = if nanoseconds < 0 { "-" } else { "" };
    let nanoseconds = nanoseconds.abs();
    let seconds = nanoseconds / 1_000_000_000;
    format!(
        "{sign}{:02}:{:02}:{:02}{}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        fraction_of_second(nanoseconds % 1_000_000_000)
    )
}

/// An interval the way DuckDB writes one: `1 year 2 months 3 days 04:05:06`.
fn interval(months: i32, days: i32, nanoseconds: i128) -> String {
    let mut parts = Vec::new();
    let mut unit = |value: i32, name: &str| {
        if value != 0 {
            let plural = if value.abs() == 1 { "" } else { "s" };
            parts.push(format!("{value} {name}{plural}"));
        }
    };
    unit(months / 12, "year");
    unit(months % 12, "month");
    unit(days, "day");
    if nanoseconds != 0 || parts.is_empty() {
        parts.push(time_of_day(nanoseconds));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scaled_integer_keeps_every_digit() {
        assert_eq!(
            scaled("1234567890123456789012", 2),
            "12345678901234567890.12"
        );
        assert_eq!(scaled("150", 2), "1.50");
        assert_eq!(scaled("5", 2), "0.05");
        assert_eq!(scaled("-5", 2), "-0.05");
        assert_eq!(scaled("0", 3), "0.000");
        assert_eq!(scaled("42", 0), "42");
        assert_eq!(scaled("-42", -2), "-4200");
        assert_eq!(
            scaled("-170141183460469231731687303715884105728", 0),
            "-170141183460469231731687303715884105728"
        );
    }

    #[test]
    fn dates_and_times_read_as_duckdb_writes_them() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(19_849), "2024-05-06");
        assert_eq!(date(-25_567), "1900-01-01");
        assert_eq!(date(i32::MAX), "infinity");
        assert_eq!(date(-i32::MAX), "-infinity");
        assert_eq!(
            timestamp(1_714_979_289_123_456, TimeUnit::Microsecond, false),
            "2024-05-06 07:08:09.123456"
        );
        assert_eq!(
            timestamp(1_714_979_289_500_000, TimeUnit::Microsecond, true),
            "2024-05-06 07:08:09.5+00"
        );
        assert_eq!(
            timestamp(1_714_979_289, TimeUnit::Second, false),
            "2024-05-06 07:08:09"
        );
        // Before 1970 the fraction still counts forwards.
        assert_eq!(
            timestamp(-1_500, TimeUnit::Millisecond, false),
            "1969-12-31 23:59:58.5"
        );
        assert_eq!(timestamp(i64::MAX, TimeUnit::Microsecond, true), "infinity");
        assert_eq!(time_of_day(25_509_123_000_000), "07:05:09.123");
        assert_eq!(time_of_day(0), "00:00:00");
        assert_eq!(
            interval(14, 3, 14_706_000_000_000),
            "1 year 2 months 3 days 04:05:06"
        );
        assert_eq!(interval(0, 1, 0), "1 day");
        assert_eq!(interval(0, 0, 0), "00:00:00");
        assert_eq!(interval(0, 0, -3_600_000_000_000), "-01:00:00");
        assert_eq!(interval(-1, 0, 0), "-1 month");
    }

    #[test]
    fn a_bit_value_drops_its_padding() {
        // `10101`: three padding bits, then the five that were stored.
        assert_eq!(bits(&[3, 0b1111_0101]).as_deref(), Some("10101"));
        assert_eq!(bits(&[0, 0xff, 0x00]).as_deref(), Some("1111111100000000"));
        assert_eq!(bits(&[]), None);
        assert_eq!(bits(&[9, 0xff]), None);
    }

    #[test]
    fn numbers_that_are_not_numbers_have_names() {
        assert_eq!(float(f64::NAN, |_| unreachable!()), "nan");
        assert_eq!(float(f64::NEG_INFINITY, |_| unreachable!()), "-inf");
        assert_eq!(float(1.5e300, |value| format!("{value:?}")), "1.5e300");
        assert_eq!(float(0.1, |value| format!("{value:?}")), "0.1");
    }
}
