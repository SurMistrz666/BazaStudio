//! Values of SQL Server as cells of the grid.
//!
//! A cell keeps the text of its value, and that text is written back as a
//! literal when the row is edited. So every text here is one SQL Server
//! reads back as the same value whatever the language of the login: dates
//! and times are ISO 8601, the form `CONVERT(…, 126)` writes, because
//! `'2026-10-12 08:00:00'` is the tenth of December for a `datetime` under a
//! login whose language puts the day first.
use crate::model::Cell;
use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime};
use tiberius::time::{Date, DateTime, DateTime2, DateTimeOffset, SmallDateTime, Time};
use tiberius::{ColumnData, ColumnType};

/// Name of a result column's type, as T-SQL writes it. The length, the
/// precision and the scale are not part of what the protocol library tells.
pub(super) fn type_name(column: ColumnType) -> &'static str {
    match column {
        ColumnType::Null => "",
        ColumnType::Bit | ColumnType::Bitn => "BIT",
        ColumnType::Int1 => "TINYINT",
        ColumnType::Int2 => "SMALLINT",
        ColumnType::Int4 | ColumnType::Intn => "INT",
        ColumnType::Int8 => "BIGINT",
        ColumnType::Float4 => "REAL",
        ColumnType::Float8 | ColumnType::Floatn => "FLOAT",
        ColumnType::Money => "MONEY",
        ColumnType::Money4 => "SMALLMONEY",
        ColumnType::Datetime | ColumnType::Datetimen => "DATETIME",
        ColumnType::Datetime4 => "SMALLDATETIME",
        ColumnType::Guid => "UNIQUEIDENTIFIER",
        ColumnType::Decimaln => "DECIMAL",
        ColumnType::Numericn => "NUMERIC",
        ColumnType::Daten => "DATE",
        ColumnType::Timen => "TIME",
        ColumnType::Datetime2 => "DATETIME2",
        ColumnType::DatetimeOffsetn => "DATETIMEOFFSET",
        ColumnType::BigVarBin => "VARBINARY",
        ColumnType::BigBinary => "BINARY",
        ColumnType::BigVarChar => "VARCHAR",
        ColumnType::BigChar => "CHAR",
        ColumnType::NVarchar => "NVARCHAR",
        ColumnType::NChar => "NCHAR",
        ColumnType::Xml => "XML",
        ColumnType::Udt => "UDT",
        ColumnType::Text => "TEXT",
        ColumnType::NText => "NTEXT",
        ColumnType::Image => "IMAGE",
        ColumnType::SSVariant => "SQL_VARIANT",
    }
}

/// One value of a row as a cell.
pub(super) fn cell(data: ColumnData<'static>, column: ColumnType) -> Cell {
    let cell = match data {
        ColumnData::U8(value) => value.map(|v| Cell::Integer(v.to_string())),
        ColumnData::I16(value) => value.map(|v| Cell::Integer(v.to_string())),
        ColumnData::I32(value) => value.map(|v| Cell::Integer(v.to_string())),
        ColumnData::I64(value) => value.map(|v| Cell::Integer(v.to_string())),
        ColumnData::F32(value) => value.map(|v| Cell::Float(v.to_string())),
        // `money` arrives as a floating point number.
        ColumnData::F64(value) if matches!(column, ColumnType::Money | ColumnType::Money4) => {
            value.map(money)
        }
        ColumnData::F64(value) => value.map(|v| Cell::Float(v.to_string())),
        ColumnData::Bit(value) => value.map(Cell::Bool),
        ColumnData::String(value) => value.map(|v| Cell::Text(v.into_owned())),
        // The way SQL Server itself prints a `uniqueidentifier`.
        ColumnData::Guid(value) => value.map(|v| {
            Cell::Text(
                v.hyphenated()
                    .encode_upper(&mut uuid::Uuid::encode_buffer())
                    .to_owned(),
            )
        }),
        // A CLR type (`geography`, `geometry`, `hierarchyid`) arrives in the
        // serialisation of its class, which is not a value anybody reads.
        ColumnData::Binary(value) if column == ColumnType::Udt => {
            value.map(|_| Cell::Unsupported("<UDT>".into()))
        }
        ColumnData::Binary(value) => value.map(|v| Cell::Bytes(v.into_owned())),
        ColumnData::Numeric(value) => value.map(|v| Cell::Decimal(decimal(v.value(), v.scale()))),
        ColumnData::Xml(value) => value.map(|v| Cell::Text(v.into_owned().into_string())),
        ColumnData::DateTime(value) => value.map(|v| text(datetime(v))),
        ColumnData::SmallDateTime(value) => value.map(|v| text(small_datetime(v))),
        ColumnData::Time(value) => value.map(|v| text(time(v))),
        ColumnData::Date(value) => value.map(|v| text(date(v))),
        ColumnData::DateTime2(value) => value.map(|v| text(datetime2(v))),
        ColumnData::DateTimeOffset(value) => value.map(|v| text(datetime_offset(v))),
    };
    cell.unwrap_or(Cell::Null)
}

/// A date or a time as text; one out of the range of the type is not shown
/// as something else.
fn text(value: Option<String>) -> Cell {
    value.map_or_else(|| Cell::Unsupported("<DATETIME>".into()), Cell::Text)
}

/// A `decimal` with every digit of its scale: the integer the server sends
/// and where its decimal point stands.
pub(super) fn decimal(value: i128, scale: u8) -> String {
    let digits = value.unsigned_abs().to_string();
    let scale = usize::from(scale);
    let sign = if value < 0 { "-" } else { "" };
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    // At least one digit stays in front of the decimal point.
    let digits = format!("{digits:0>width$}", width = scale + 1);
    let (whole, fraction) = digits.split_at(digits.len() - scale);
    format!("{sign}{whole}.{fraction}")
}

/// `money` and `smallmoney` have four decimal places. The protocol library
/// hands them over as a floating point number, which holds the four places
/// exactly only below 2^39 (about 550 billion): a larger amount is shown as
/// a value this application does not read, never as a rounded one.
fn money(value: f64) -> Cell {
    const EXACT_BELOW: f64 = 549_755_813_888.0;
    if !value.is_finite() || value.abs() >= EXACT_BELOW {
        return Cell::Unsupported("<MONEY>".into());
    }
    let text = format!("{value:.4}");
    Cell::Decimal(if text == "-0.0000" {
        "0.0000".into()
    } else {
        text
    })
}

/// `hh:mm:ss` with the fraction of a second in `scale` digits, left out
/// when it is zero.
fn clock(increments: u64, scale: u8) -> Option<(NaiveTime, String)> {
    if scale > 7 {
        return None;
    }
    let unit = 10u64.pow(u32::from(scale));
    let (seconds, fraction) = (increments / unit, increments % unit);
    let time = NaiveTime::from_num_seconds_from_midnight_opt(u32::try_from(seconds).ok()?, 0)?;
    let fraction = if fraction == 0 {
        String::new()
    } else {
        format!(".{fraction:0>width$}", width = usize::from(scale))
    };
    Some((time, fraction))
}

fn day(date: Date) -> Option<NaiveDate> {
    // Days since the first of January of the year 1.
    NaiveDate::from_num_days_from_ce_opt(i32::try_from(date.days()).ok()?.checked_add(1)?)
}

fn date(value: Date) -> Option<String> {
    Some(day(value)?.format("%Y-%m-%d").to_string())
}

fn time(value: Time) -> Option<String> {
    let (time, fraction) = clock(value.increments(), value.scale())?;
    Some(format!("{}{fraction}", time.format("%H:%M:%S")))
}

fn datetime2(value: DateTime2) -> Option<String> {
    Some(format!("{}T{}", date(value.date())?, time(value.time())?))
}

/// The local time of the value and its offset; the protocol carries the
/// time in UTC.
fn datetime_offset(value: DateTimeOffset) -> Option<String> {
    let inner = value.datetime2();
    let (time, fraction) = clock(inner.time().increments(), inner.time().scale())?;
    let offset = i64::from(value.offset());
    let local = NaiveDateTime::new(day(inner.date())?, time)
        .checked_add_signed(Duration::minutes(offset))?;
    Some(format!(
        "{}{fraction}{}{:02}:{:02}",
        local.format("%Y-%m-%dT%H:%M:%S"),
        if offset < 0 { '-' } else { '+' },
        offset.abs() / 60,
        offset.abs() % 60
    ))
}

fn since_1900(days: i64) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(1900, 1, 1)?.checked_add_signed(Duration::days(days))
}

/// `datetime`: days since 1900 and three-hundredths of a second since
/// midnight, shown in milliseconds the way the server rounds them (.000,
/// .003, .007).
fn datetime(value: DateTime) -> Option<String> {
    let ticks = value.seconds_fragments();
    let time = NaiveTime::from_num_seconds_from_midnight_opt(ticks / 300, 0)?;
    let milliseconds = (ticks % 300 * 1000 + 150) / 300;
    let fraction = if milliseconds == 0 {
        String::new()
    } else {
        format!(".{milliseconds:03}")
    };
    Some(format!(
        "{}T{}{fraction}",
        since_1900(i64::from(value.days()))?.format("%Y-%m-%d"),
        time.format("%H:%M:%S")
    ))
}

/// `smalldatetime`: days since 1900 and minutes since midnight.
fn small_datetime(value: SmallDateTime) -> Option<String> {
    let time = NaiveTime::from_num_seconds_from_midnight_opt(
        u32::from(value.seconds_fragments()) * 60,
        0,
    )?;
    Some(format!(
        "{}T{}",
        since_1900(i64::from(value.days()))?.format("%Y-%m-%d"),
        time.format("%H:%M:%S")
    ))
}

/// Text around a quoted column that turns it into the text the grid shows
/// for a value of that type, in lower case: the left side of the "contains"
/// filter. `data_type` is the type as `columns` reports it.
pub(super) fn contains_cast(data_type: &str) -> (&'static str, &'static str) {
    let name = data_type
        .split('(')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match name.as_str() {
        "datetime" | "smalldatetime" | "datetime2" | "date" | "time" | "datetimeoffset" => {
            ("LOWER(CONVERT(NVARCHAR(MAX), ", ", 126))")
        }
        "money" | "smallmoney" => ("CONVERT(NVARCHAR(MAX), ", ", 2)"),
        // The default conversion keeps six digits of a `float`.
        "float" | "real" => ("CAST(TRY_CONVERT(DECIMAL(38,15), ", ") AS NVARCHAR(MAX))"),
        "bit" => ("CASE ", " WHEN 1 THEN N'true' WHEN 0 THEN N'false' END"),
        "binary" | "varbinary" | "image" | "timestamp" | "rowversion" => (
            "LOWER(CONVERT(NVARCHAR(MAX), CAST(",
            " AS VARBINARY(MAX)), 2))",
        ),
        _ => GENERIC_CAST,
    }
}

/// The cast of a column whose type is not known: text as it is, a number
/// in its digits.
pub(super) const GENERIC_CAST: (&str, &str) = ("LOWER(CAST(", " AS NVARCHAR(MAX)))");

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;
    use tiberius::numeric::Numeric;

    #[test]
    fn decimals_keep_every_digit_of_their_scale() {
        for (value, scale, expected) in [
            (0, 0, "0"),
            (0, 2, "0.00"),
            (-50, 2, "-0.50"),
            (5, 3, "0.005"),
            (-5, 3, "-0.005"),
            (31800, 2, "318.00"),
            (42, 0, "42"),
            (-42, 0, "-42"),
            (
                123_456_789_012_345_678_901_200_000_000,
                10,
                "12345678901234567890.1200000000",
            ),
            // The largest DECIMAL(38,0) and the smallest DECIMAL(38,38).
            (
                99_999_999_999_999_999_999_999_999_999_999_999_999,
                0,
                "99999999999999999999999999999999999999",
            ),
            (-1, 38, "-0.00000000000000000000000000000000000001"),
        ] {
            assert_eq!(decimal(value, scale), expected);
            // The text is the same number for a literal.
            let parsed: bigdecimal::BigDecimal = decimal(value, scale).parse().unwrap();
            assert_eq!(
                parsed,
                bigdecimal::BigDecimal::new(value.into(), scale.into())
            );
        }
        assert_eq!(
            cell(
                ColumnData::Numeric(Some(Numeric::new_with_scale(-50, 2))),
                ColumnType::Numericn
            ),
            Cell::Decimal("-0.50".into())
        );
    }

    #[test]
    fn money_is_exact_or_not_shown_as_a_number() {
        let money = |value: f64| cell(ColumnData::F64(Some(value)), ColumnType::Money);
        assert_eq!(money(1234.5), Cell::Decimal("1234.5000".into()));
        assert_eq!(money(-0.0001), Cell::Decimal("-0.0001".into()));
        assert_eq!(money(0.0), Cell::Decimal("0.0000".into()));
        assert_eq!(money(-0.0), Cell::Decimal("0.0000".into()));
        assert_eq!(money(214748.3647), Cell::Decimal("214748.3647".into()));
        // Every amount below the limit is the nearest number to its four
        // decimal places, so printing four places gives it back.
        for scaled in [5_497_558_138_879_999_i64, -5_497_558_138_879_999, 1, 99_999] {
            let text = decimal(scaled.into(), 4);
            assert_eq!(money(scaled as f64 / 1e4), Cell::Decimal(text));
        }
        // What a floating point number cannot hold to the last place.
        assert_eq!(
            money(922_337_203_685_477.6),
            Cell::Unsupported("<MONEY>".into())
        );
        assert_eq!(
            money(549_755_813_888.0),
            Cell::Unsupported("<MONEY>".into())
        );
        // A `float` column is a floating point number, whatever its value.
        assert_eq!(
            cell(ColumnData::F64(Some(0.1)), ColumnType::Float8),
            Cell::Float("0.1".into())
        );
        assert_eq!(
            cell(ColumnData::F32(Some(1.5)), ColumnType::Float4),
            Cell::Float("1.5".into())
        );
    }

    /// The values the test server sent for these literals.
    #[test]
    fn dates_and_times_are_written_in_iso_8601() {
        let text = |data| match cell(data, ColumnType::Null) {
            Cell::Text(text) => text,
            other => panic!("{other:?}"),
        };
        // CAST('2026-10-12T13:45:30.123' AS DATETIME)
        assert_eq!(
            text(ColumnData::DateTime(Some(DateTime::new(46305, 14_859_037)))),
            "2026-10-12T13:45:30.123"
        );
        // The first day of `datetime`, and a whole second.
        assert_eq!(
            text(ColumnData::DateTime(Some(DateTime::new(-53690, 0)))),
            "1753-01-01T00:00:00"
        );
        // Three-hundredths of a second, rounded as the server prints them.
        for (ticks, expected) in [(1, ".003"), (2, ".007"), (3, ".010"), (299, ".997")] {
            assert_eq!(
                text(ColumnData::DateTime(Some(DateTime::new(0, ticks)))),
                format!("1900-01-01T00:00:00{expected}")
            );
        }
        // CAST('2026-10-12T13:45:00' AS SMALLDATETIME)
        assert_eq!(
            text(ColumnData::SmallDateTime(Some(SmallDateTime::new(
                46305, 825
            )))),
            "2026-10-12T13:45:00"
        );
        assert_eq!(
            text(ColumnData::Date(Some(Date::new(739_900)))),
            "2026-10-12"
        );
        assert_eq!(text(ColumnData::Date(Some(Date::new(0)))), "0001-01-01");
        assert_eq!(
            text(ColumnData::Time(Some(Time::new(495_301_234_567, 7)))),
            "13:45:30.1234567"
        );
        assert_eq!(
            text(ColumnData::Time(Some(Time::new(49_530, 0)))),
            "13:45:30"
        );
        // A fraction keeps the digits of the column's scale.
        assert_eq!(
            text(ColumnData::Time(Some(Time::new(495_301_000_000, 7)))),
            "13:45:30.1000000"
        );
        assert_eq!(
            text(ColumnData::DateTime2(Some(DateTime2::new(
                Date::new(739_900),
                Time::new(4_953_012, 2)
            )))),
            "2026-10-12T13:45:30.12"
        );
        assert_eq!(
            text(ColumnData::DateTime2(Some(DateTime2::new(
                Date::new(739_900),
                Time::new(495_300_000_000, 7)
            )))),
            "2026-10-12T13:45:30"
        );
        // CAST('2026-10-12 13:45:30.1234567 +02:00' AS DATETIMEOFFSET): the
        // protocol carries 11:45 UTC and 120 minutes.
        let utc = DateTime2::new(Date::new(739_900), Time::new(423_301_234_567, 7));
        assert_eq!(
            text(ColumnData::DateTimeOffset(Some(DateTimeOffset::new(
                utc, 120
            )))),
            "2026-10-12T13:45:30.1234567+02:00"
        );
        // An offset that crosses midnight backwards.
        let night = DateTime2::new(Date::new(739_900), Time::new(18_000_000_000, 7));
        assert_eq!(
            text(ColumnData::DateTimeOffset(Some(DateTimeOffset::new(
                night, -330
            )))),
            "2026-10-11T19:00:00-05:30"
        );
        // A scale the type does not have is not read as something else.
        assert_eq!(
            cell(ColumnData::Time(Some(Time::new(1, 9))), ColumnType::Timen),
            Cell::Unsupported("<DATETIME>".into())
        );
    }

    #[test]
    fn the_other_types_become_the_cells_the_grid_knows() {
        assert_eq!(
            cell(ColumnData::Bit(Some(true)), ColumnType::Bitn),
            Cell::Bool(true)
        );
        assert_eq!(
            cell(ColumnData::U8(Some(255)), ColumnType::Int1),
            Cell::Integer("255".into())
        );
        assert_eq!(
            cell(
                ColumnData::I64(Some(9_007_199_254_740_993)),
                ColumnType::Int8
            ),
            Cell::Integer("9007199254740993".into())
        );
        assert_eq!(cell(ColumnData::I32(None), ColumnType::Int4), Cell::Null);
        assert_eq!(
            cell(ColumnData::String(None), ColumnType::NVarchar),
            Cell::Null
        );
        assert_eq!(
            cell(
                ColumnData::String(Some(Cow::Borrowed("zażółć 🦀"))),
                ColumnType::NVarchar
            ),
            Cell::Text("zażółć 🦀".into())
        );
        let id = uuid::Uuid::parse_str("77929515-416d-4b18-9268-3883e48a25cd").unwrap();
        assert_eq!(
            cell(ColumnData::Guid(Some(id)), ColumnType::Guid),
            Cell::Text("77929515-416D-4B18-9268-3883E48A25CD".into())
        );
        assert_eq!(
            cell(
                ColumnData::Binary(Some(Cow::Borrowed(&[0, 255]))),
                ColumnType::BigVarBin
            ),
            Cell::Bytes(vec![0, 255])
        );
        assert_eq!(
            cell(
                ColumnData::Binary(Some(Cow::Borrowed(&[1, 2]))),
                ColumnType::Udt
            ),
            Cell::Unsupported("<UDT>".into())
        );
        assert_eq!(cell(ColumnData::Binary(None), ColumnType::Udt), Cell::Null);
        let xml = tiberius::xml::XmlData::new("<a x=\"1\">t</a>");
        assert_eq!(
            cell(ColumnData::Xml(Some(Cow::Owned(xml))), ColumnType::Xml),
            Cell::Text("<a x=\"1\">t</a>".into())
        );
        assert_eq!(type_name(ColumnType::NVarchar), "NVARCHAR");
        assert_eq!(type_name(ColumnType::Datetime2), "DATETIME2");
        assert_eq!(type_name(ColumnType::Null), "");
    }

    #[test]
    fn the_filter_searches_the_text_the_grid_shows() {
        assert_eq!(contains_cast("nvarchar(255)"), GENERIC_CAST);
        assert_eq!(contains_cast("int"), GENERIC_CAST);
        assert_eq!(contains_cast("decimal(38,10)"), GENERIC_CAST);
        assert_eq!(contains_cast("uniqueidentifier"), GENERIC_CAST);
        assert_eq!(contains_cast("xml"), GENERIC_CAST);
        for dated in [
            "datetime",
            "DATETIME2(7)",
            "date",
            "time(0)",
            "datetimeoffset(7)",
        ] {
            assert_eq!(contains_cast(dated).1, ", 126))", "{dated}");
        }
        assert_eq!(contains_cast("money").1, ", 2)");
        assert!(
            contains_cast("float")
                .0
                .contains("TRY_CONVERT(DECIMAL(38,15)")
        );
        assert!(contains_cast("bit").1.contains("N'true'"));
        assert!(contains_cast("varbinary(max)").1.ends_with(", 2))"));
        assert!(contains_cast("timestamp").0.contains("CAST("));
    }
}
