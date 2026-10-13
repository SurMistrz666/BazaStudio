//! Values of ClickHouse as cells, and cells as text the server reads.
//!
//! Results arrive as `TabSeparatedWithNamesAndTypes`: every type of the
//! server has a text form there, a field never holds a raw tab or line end,
//! and the bytes of a `String` come through untouched, which no JSON format
//! promises. The type names of the second line decide what a field becomes.
use crate::model::Cell;
use anyhow::{Result, bail, ensure};

/// What a field of a column is read as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// `Nothing`: a column that only holds NULL.
    Nothing,
    Bool,
    /// `Int8` … `Int256`, `UInt8` … `UInt256` and the interval types.
    Integer,
    Float,
    Decimal,
    /// `String`: bytes that usually are text.
    Text,
    /// `FixedString(n)`: the same, padded with zero bytes.
    Fixed,
    /// A value with one text form of its own: a date, a time, a UUID, an
    /// address, a name of an enumeration.
    Plain,
    /// `JSON` and `Object`.
    Json,
    /// `Array`, `Tuple` and the like, written the way a ClickHouse literal
    /// is: strings inside are quoted and escaped already.
    Composite,
    /// `Map`: the same, and the one composite type the server does not
    /// cast from a text.
    Map,
    /// A state of an aggregate function: bytes that mean nothing outside
    /// the server.
    State,
}

/// A type as the server names it, and how its values are read and written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Type {
    /// The full name: `LowCardinality(Nullable(String))`.
    pub name: String,
    pub kind: Kind,
    pub nullable: bool,
}

impl Type {
    pub(super) fn parse(name: &str) -> Self {
        let mut inner = name.trim();
        let mut nullable = false;
        // The wrappers change how a column is stored, not what it holds.
        loop {
            if let Some(rest) = argument(inner, "Nullable") {
                nullable = true;
                inner = rest;
            } else if let Some(rest) = argument(inner, "LowCardinality") {
                inner = rest;
            } else if let Some(rest) = argument(inner, "SimpleAggregateFunction") {
                // `SimpleAggregateFunction(max, UInt64)` is stored as its
                // second argument.
                inner = match split_top_level(rest).get(1) {
                    Some(value) => value,
                    None => break,
                };
            } else {
                break;
            }
        }
        let base = inner.split('(').next().unwrap_or(inner).trim();
        let kind = match base {
            "Nothing" => Kind::Nothing,
            "Bool" => Kind::Bool,
            "Float32" | "Float64" | "BFloat16" => Kind::Float,
            "String" => Kind::Text,
            "FixedString" => Kind::Fixed,
            "JSON" | "Object" => Kind::Json,
            "AggregateFunction" => Kind::State,
            "Map" => Kind::Map,
            "Array" | "Tuple" | "Nested" | "Point" | "Ring" | "LineString" | "MultiLineString"
            | "Polygon" | "MultiPolygon" | "Geometry" | "QBit" => Kind::Composite,
            _ if base.starts_with("Decimal") => Kind::Decimal,
            _ if base.starts_with("Interval") => Kind::Integer,
            _ if base
                .strip_prefix("UInt")
                .or_else(|| base.strip_prefix("Int"))
                .is_some_and(|bits| matches!(bits, "8" | "16" | "32" | "64" | "128" | "256")) =>
            {
                Kind::Integer
            }
            _ => Kind::Plain,
        };
        Self {
            name: name.trim().to_owned(),
            kind,
            nullable: nullable || kind == Kind::Nothing,
        }
    }

    /// A field of a result as a cell. `field` is the text between two tabs,
    /// still escaped.
    pub(super) fn cell(&self, field: &[u8]) -> Cell {
        if field == b"\\N" {
            return Cell::Null;
        }
        match self.kind {
            Kind::Nothing => Cell::Null,
            Kind::State => Cell::Unsupported(format!("<{}>", self.name)),
            Kind::Composite | Kind::Map => match std::str::from_utf8(field) {
                Ok(text) => Cell::Text(text.to_owned()),
                Err(_) => Cell::Unsupported(format!("<{}>", self.name)),
            },
            Kind::Bool => match field {
                b"true" | b"1" => Cell::Bool(true),
                b"false" | b"0" => Cell::Bool(false),
                _ => text_or_bytes(unescape(field)),
            },
            Kind::Integer => number(field, Cell::Integer),
            Kind::Decimal => number(field, Cell::Decimal),
            Kind::Float => number(field, Cell::Float),
            Kind::Fixed => {
                let mut bytes = unescape(field);
                // The padding is not part of what was stored.
                while bytes.last() == Some(&0) {
                    bytes.pop();
                }
                text_or_bytes(bytes)
            }
            Kind::Json => match String::from_utf8(unescape(field)) {
                Ok(text) if serde_json::from_str::<serde_json::Value>(&text).is_ok() => {
                    Cell::Json(text)
                }
                Ok(text) => Cell::Text(text),
                Err(error) => Cell::Bytes(error.into_bytes()),
            },
            Kind::Text | Kind::Plain => text_or_bytes(unescape(field)),
        }
    }

    /// A cell as an expression of this type, for a statement.
    ///
    /// The kind of the cell is not trusted, only its text: the interface
    /// guesses the kind of a typed value from the name of the type. So the
    /// text is handed to the server as a string and cast by it, which also
    /// keeps every digit of a 256-bit integer and of a decimal.
    pub(super) fn literal(&self, cell: &Cell) -> Result<String> {
        let text = match cell {
            Cell::Null => return Ok("NULL".into()),
            Cell::Unsupported(_) => bail!("unsupported_cell_type"),
            Cell::Bytes(bytes) => {
                let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
                return Ok(match self.kind {
                    Kind::Text => format!("unhex('{hex}')"),
                    _ => format!("CAST(unhex('{hex}') AS {})", self.name),
                });
            }
            Cell::Bool(value) => value.to_string(),
            Cell::Integer(text)
            | Cell::Decimal(text)
            | Cell::Float(text)
            | Cell::Text(text)
            | Cell::Json(text) => text.clone(),
        };
        ensure!(self.kind != Kind::State, "unsupported_cell_type");
        Ok(match self.kind {
            Kind::Text => quote_text(&text),
            // Read the way an import reads a field of this type: the text
            // stays a text all the way, and only the server takes it apart.
            Kind::Map => format!(
                "(SELECT value FROM format(TabSeparated, {}, {}))",
                quote_text(&format!("value {}", self.name)),
                quote_text(&composite_field(&text))
            ),
            _ => format!("CAST({} AS {})", quote_text(&text), self.name),
        })
    }

    /// `column = value`, or what stands for it where the type has no
    /// equality of its own.
    pub(super) fn predicate(&self, column: &str, cell: &Cell) -> Result<String> {
        let name = quote_name(column)?;
        Ok(match cell {
            Cell::Null => format!("{name} IS NULL"),
            // A JSON value is compared by its text.
            _ if self.kind == Kind::Json => {
                format!("toString({name}) = {}", quote_text(&cell.display()))
            }
            _ => format!("{name} = {}", self.literal(cell)?),
        })
    }

    /// A cell as a field of `TabSeparated` input, for an import.
    pub(super) fn field(&self, cell: &Cell, out: &mut Vec<u8>) -> Result<()> {
        match cell {
            Cell::Null => out.extend_from_slice(b"\\N"),
            Cell::Unsupported(_) => bail!("unsupported_cell_type"),
            Cell::Bool(value) => out.extend_from_slice(value.to_string().as_bytes()),
            Cell::Bytes(bytes) => escape(bytes, out),
            Cell::Integer(text)
            | Cell::Decimal(text)
            | Cell::Float(text)
            | Cell::Text(text)
            | Cell::Json(text) => {
                ensure!(self.kind != Kind::State, "unsupported_cell_type");
                if matches!(self.kind, Kind::Composite | Kind::Map) {
                    out.extend_from_slice(composite_field(text).as_bytes());
                } else {
                    escape(text.as_bytes(), out);
                }
            }
        }
        Ok(())
    }
}

/// A composite value as a field of `TabSeparated` input. It is already
/// written as the server reads it: only what would end the field is
/// replaced.
fn composite_field(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// The text inside `Name( … )`, when `text` is that wrapper.
fn argument<'a>(text: &'a str, wrapper: &str) -> Option<&'a str> {
    text.strip_prefix(wrapper)?
        .trim_start()
        .strip_prefix('(')?
        .strip_suffix(')')
        .map(str::trim)
}

/// The arguments of a type, split at the commas that stand outside brackets
/// and quotes.
fn split_top_level(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut quoted, mut start) = (0usize, false, 0usize);
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if quoted => index += 1,
            b'\'' => quoted = !quoted,
            b'(' if !quoted => depth += 1,
            b')' if !quoted => depth = depth.saturating_sub(1),
            b',' if !quoted && depth == 0 => {
                parts.push(text[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    parts.push(text[start..].trim());
    parts
}

fn number(field: &[u8], make: fn(String) -> Cell) -> Cell {
    match std::str::from_utf8(field) {
        Ok(text) => make(text.to_owned()),
        Err(_) => Cell::Bytes(field.to_vec()),
    }
}

fn text_or_bytes(bytes: Vec<u8>) -> Cell {
    match String::from_utf8(bytes) {
        Ok(text) => Cell::Text(text),
        Err(error) => Cell::Bytes(error.into_bytes()),
    }
}

/// The bytes of a `TabSeparated` field.
pub(super) fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut index = 0;
    while index < field.len() {
        let byte = field[index];
        index += 1;
        if byte != b'\\' || index >= field.len() {
            out.push(byte);
            continue;
        }
        let escaped = field[index];
        index += 1;
        out.push(match escaped {
            b'b' => 0x08,
            b'f' => 0x0c,
            b'r' => b'\r',
            b'n' => b'\n',
            b't' => b'\t',
            b'0' => 0,
            b'a' => 0x07,
            b'v' => 0x0b,
            b'e' => 0x1b,
            b'x' => {
                let digits = field
                    .get(index..index + 2)
                    .and_then(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok());
                match digits {
                    Some(value) => {
                        index += 2;
                        value
                    }
                    None => b'x',
                }
            }
            other => other,
        });
    }
    out
}

/// Bytes as a `TabSeparated` field.
fn escape(bytes: &[u8], out: &mut Vec<u8>) {
    for byte in bytes {
        match byte {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0 => out.extend_from_slice(b"\\0"),
            other => out.push(*other),
        }
    }
}

/// A text literal. A backslash escapes inside it, a quote is escaped too,
/// and a zero byte is written the way the server reads one.
pub(super) fn quote_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\0' => out.push_str("\\0"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

/// A quoted name. The server reads a backslash inside backticks as an
/// escape, so it is doubled like the backtick itself is escaped.
pub(super) fn quote_name(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty()
            && name.len() <= 256
            && !name
                .chars()
                .any(|character| character == '\0' || character.is_control()),
        "identifier_invalid"
    );
    let mut out = String::with_capacity(name.len() + 2);
    out.push('`');
    for character in name.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '`' => out.push_str("\\`"),
            other => out.push(other),
        }
    }
    out.push('`');
    Ok(out)
}

/// `database`.`table`, or the table alone when it names no database.
pub(super) fn qualified(table: &crate::model::TableRef) -> Result<String> {
    if table.schema.is_empty() {
        quote_name(&table.name)
    } else {
        Ok(format!(
            "{}.{}",
            quote_name(&table.schema)?,
            quote_name(&table.name)?
        ))
    }
}

/// A cell of a column whose type is not known, as an expression: a number
/// that the server reads exactly as written, otherwise a text.
pub(super) fn untyped_literal(cell: &Cell) -> Result<String> {
    Ok(match cell {
        Cell::Null => "NULL".into(),
        Cell::Bool(value) => value.to_string(),
        Cell::Unsupported(_) => bail!("unsupported_cell_type"),
        Cell::Bytes(bytes) => {
            let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("unhex('{hex}')")
        }
        // Up to 64 bits a bare number is an integer to the server; a longer
        // one would be read as a floating point number.
        Cell::Integer(text) if text.parse::<i64>().is_ok() || text.parse::<u64>().is_ok() => {
            text.clone()
        }
        Cell::Integer(text)
        | Cell::Decimal(text)
        | Cell::Float(text)
        | Cell::Text(text)
        | Cell::Json(text) => quote_text(text),
    })
}

/// The lines of an answer in `TabSeparatedWithNamesAndTypes`, taken apart as
/// they arrive.
#[derive(Default)]
pub(super) struct Reader {
    /// Bytes of a line that has not ended yet.
    pending: Vec<u8>,
    pub names: Vec<String>,
    pub types: Vec<Type>,
    /// Lines read so far.
    lines: usize,
    /// The server puts no frame around a failure that comes after the first
    /// rows: its message is then looked for among the lines.
    unframed: bool,
}

/// What a line of an answer turned out to be.
pub(super) enum Line {
    /// Names or types: nothing to keep yet.
    Header,
    Row(Vec<Cell>),
    /// The server gave up in the middle of the answer. This is the part of
    /// its message that was read already; the rest follows in the body.
    Exception(Vec<u8>),
}

impl Reader {
    /// A reader for the answer of a server that frames a late failure
    /// (`framed`), or of an older one that only appends its message.
    pub(super) fn new(framed: bool) -> Self {
        Self {
            unframed: !framed,
            ..Self::default()
        }
    }

    /// Add bytes of the body and hand every complete line to `each`, which
    /// says whether to go on. Returns how many bytes of `chunk` were taken
    /// when `each` said stop, and `None` when all of them were.
    pub(super) fn feed(
        &mut self,
        chunk: &[u8],
        mut each: impl FnMut(Line) -> Result<bool>,
    ) -> Result<Option<usize>> {
        let mut start = 0;
        while let Some(length) = chunk[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + length;
            let line = if self.pending.is_empty() {
                self.line(&chunk[start..end])?
            } else {
                self.pending.extend_from_slice(&chunk[start..end]);
                let whole = std::mem::take(&mut self.pending);
                self.line(&whole)?
            };
            start = end + 1;
            if !each(line)? {
                return Ok(Some(start));
            }
        }
        self.pending.extend_from_slice(&chunk[start..]);
        Ok(None)
    }

    /// What is left when the body has ended: nothing, after a complete
    /// answer.
    pub(super) fn leftover(&self) -> &[u8] {
        &self.pending
    }

    fn line(&mut self, line: &[u8]) -> Result<Line> {
        // A field never holds a raw carriage return: this is the frame the
        // server puts around a failure that came after the first rows.
        if line.ends_with(b"\r") {
            return Ok(Line::Exception(Vec::new()));
        }
        self.lines += 1;
        match self.lines {
            1 => {
                self.names = line
                    .split(|byte| *byte == b'\t')
                    .map(|field| String::from_utf8_lossy(&unescape(field)).into_owned())
                    .collect();
                Ok(Line::Header)
            }
            2 => {
                self.types = line
                    .split(|byte| *byte == b'\t')
                    .map(|field| Type::parse(&String::from_utf8_lossy(&unescape(field))))
                    .collect();
                ensure!(
                    self.types.len() == self.names.len(),
                    "clickhouse_answer_unreadable"
                );
                Ok(Line::Header)
            }
            _ => {
                let fields = line.split(|byte| *byte == b'\t').count();
                // Without a frame the message of a failure is one more line.
                // In a result of one column it cannot be told from a row
                // that holds such a text, and a failure is the safer reading.
                if self.unframed
                    && (fields != self.types.len() || fields == 1)
                    && line.starts_with(b"Code: ")
                    && line.windows(13).any(|window| window == b"DB::Exception")
                {
                    return Ok(Line::Exception(line.to_vec()));
                }
                ensure!(fields == self.types.len(), "clickhouse_answer_unreadable");
                Ok(Line::Row(
                    line.split(|byte| *byte == b'\t')
                        .zip(&self.types)
                        .map(|(field, column)| column.cell(field))
                        .collect(),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(name: &str) -> (Kind, bool) {
        let parsed = Type::parse(name);
        (parsed.kind, parsed.nullable)
    }

    #[test]
    fn types_are_read_through_their_wrappers() {
        assert_eq!(kind("UInt256"), (Kind::Integer, false));
        assert_eq!(kind("Nullable(Int8)"), (Kind::Integer, true));
        assert_eq!(kind("LowCardinality(Nullable(String))"), (Kind::Text, true));
        assert_eq!(kind("Decimal(38, 10)"), (Kind::Decimal, false));
        assert_eq!(kind("Nullable(Decimal64(4))"), (Kind::Decimal, true));
        assert_eq!(kind("Float32"), (Kind::Float, false));
        assert_eq!(kind("Bool"), (Kind::Bool, false));
        assert_eq!(kind("FixedString(16)"), (Kind::Fixed, false));
        assert_eq!(kind("DateTime64(3, 'Europe/Warsaw')"), (Kind::Plain, false));
        assert_eq!(kind("Date32"), (Kind::Plain, false));
        assert_eq!(kind("UUID"), (Kind::Plain, false));
        assert_eq!(kind("IPv6"), (Kind::Plain, false));
        assert_eq!(kind("Enum8('a' = 1, 'b' = 2)"), (Kind::Plain, false));
        assert_eq!(kind("IntervalDay"), (Kind::Integer, false));
        assert_eq!(kind("Array(Nullable(String))"), (Kind::Composite, false));
        assert_eq!(kind("Map(String, UInt8)"), (Kind::Map, false));
        assert_eq!(kind("Array(Map(String, UInt8))"), (Kind::Composite, false));
        assert_eq!(kind("Tuple(a String, b UInt8)"), (Kind::Composite, false));
        assert_eq!(kind("JSON"), (Kind::Json, false));
        assert_eq!(kind("JSON(max_dynamic_paths = 8)"), (Kind::Json, false));
        assert_eq!(
            kind("AggregateFunction(uniq, UInt64)"),
            (Kind::State, false)
        );
        assert_eq!(
            kind("SimpleAggregateFunction(max, Nullable(UInt64))"),
            (Kind::Integer, true)
        );
        assert_eq!(kind("Nullable(Nothing)"), (Kind::Nothing, true));
        // A type of a later server still has a text form.
        assert_eq!(kind("SomethingNew(3)"), (Kind::Plain, false));
        // Names that merely start like an integer type are not one.
        assert_eq!(kind("Int"), (Kind::Plain, false));
        assert_eq!(kind("Interned"), (Kind::Plain, false));
    }

    #[test]
    fn fields_become_cells_without_losing_a_digit_or_a_byte() {
        let cell = |name: &str, field: &[u8]| Type::parse(name).cell(field);
        assert_eq!(
            cell(
                "UInt256",
                b"115792089237316195423570985008687907853269984665640564039457584007913129639935"
            ),
            Cell::Integer(
                "115792089237316195423570985008687907853269984665640564039457584007913129639935"
                    .into()
            )
        );
        assert_eq!(
            cell("Decimal(38, 2)", b"12345678901234567890.12"),
            Cell::Decimal("12345678901234567890.12".into())
        );
        assert_eq!(cell("Float64", b"inf"), Cell::Float("inf".into()));
        assert_eq!(cell("Bool", b"true"), Cell::Bool(true));
        assert_eq!(cell("Nullable(String)", b"\\N"), Cell::Null);
        // A text that reads `\N` is escaped by the server, so it stays text.
        assert_eq!(cell("String", b"\\\\N"), Cell::Text("\\N".into()));
        assert_eq!(
            cell("String", b"tab\\there\\nnl\\\\bs\\'q"),
            Cell::Text("tab\there\nnl\\bs'q".into())
        );
        assert_eq!(
            cell("String", b"Za\xc5\xbc\xc3\xb3\xc5\x82\xc4\x87"),
            Cell::Text("Zażółć".into())
        );
        // Bytes that are not text stay bytes.
        assert_eq!(
            cell("String", b"\xff\\0\xfeA"),
            Cell::Bytes(vec![0xff, 0, 0xfe, b'A'])
        );
        assert_eq!(cell("FixedString(4)", b"ab\\0\\0"), Cell::Text("ab".into()));
        assert_eq!(
            cell("DateTime64(3, 'Europe/Warsaw')", b"2024-05-06 07:08:09.123"),
            Cell::Text("2024-05-06 07:08:09.123".into())
        );
        assert_eq!(
            cell("JSON", b"{\"a\":[1,2],\"b\":\"x\\\\ty\"}"),
            Cell::Json("{\"a\":[1,2],\"b\":\"x\\ty\"}".into())
        );
        // A composite value is already a literal of the server.
        assert_eq!(
            cell("Array(String)", b"['a\\tb','it\\'s']"),
            Cell::Text("['a\\tb','it\\'s']".into())
        );
        // A state of an aggregate function is named, never shown.
        assert_eq!(
            cell("AggregateFunction(uniq, UInt64)", b"\\0\xe8\x07garbage"),
            Cell::Unsupported("<AggregateFunction(uniq, UInt64)>".into())
        );
    }

    #[test]
    fn cells_are_written_as_expressions_of_the_column_type() {
        let literal = |name: &str, cell: Cell| Type::parse(name).literal(&cell).unwrap();
        assert_eq!(
            literal("String", Cell::Text("it's \\ \0".into())),
            "'it\\'s \\\\ \\0'"
        );
        assert_eq!(
            literal(
                "Nullable(Decimal(38, 10))",
                Cell::Decimal("12345678901234567890.12".into())
            ),
            "CAST('12345678901234567890.12' AS Nullable(Decimal(38, 10)))"
        );
        // The interface calls anything whose type name holds "INT" an
        // integer: the text decides, not the kind.
        assert_eq!(
            literal("Point", Cell::Integer("(1,2)".into())),
            "CAST('(1,2)' AS Point)"
        );
        assert_eq!(
            literal(
                "DateTime64(3, 'UTC')",
                Cell::Text("2024-01-02 03:04:05.678".into())
            ),
            "CAST('2024-01-02 03:04:05.678' AS DateTime64(3, 'UTC'))"
        );
        assert_eq!(literal("Bool", Cell::Bool(true)), "CAST('true' AS Bool)");
        // The server casts a text to every composite type but a map: that
        // one is read like a field of an import.
        assert_eq!(
            literal("Array(String)", Cell::Text("['a\\tb','it\\'s']".into())),
            "CAST('[\\'a\\\\tb\\',\\'it\\\\\\'s\\']' AS Array(String))"
        );
        assert_eq!(
            literal("Map(String, Int32)", Cell::Integer("{'k':1}".into())),
            "(SELECT value FROM format(TabSeparated, 'value Map(String, Int32)', '{\\'k\\':1}'))"
        );
        assert_eq!(
            literal("String", Cell::Bytes(vec![0xff, 0])),
            "unhex('ff00')"
        );
        assert_eq!(
            literal("FixedString(2)", Cell::Bytes(vec![0xff, 0])),
            "CAST(unhex('ff00') AS FixedString(2))"
        );
        assert_eq!(literal("Int8", Cell::Null), "NULL");
        assert!(
            Type::parse("String")
                .literal(&Cell::Unsupported("<x>".into()))
                .is_err()
        );
        let predicate =
            |name: &str, cell: Cell| Type::parse(name).predicate("a`b\\c", &cell).unwrap();
        assert_eq!(
            predicate("Nullable(String)", Cell::Null),
            "`a\\`b\\\\c` IS NULL"
        );
        assert_eq!(
            predicate("Int64", Cell::Integer("7".into())),
            "`a\\`b\\\\c` = CAST('7' AS Int64)"
        );
        assert_eq!(
            predicate("JSON", Cell::Json("{\"a\":1}".into())),
            "toString(`a\\`b\\\\c`) = '{\"a\":1}'"
        );
        assert!(quote_name("bad\0").is_err());
        assert!(quote_name("").is_err());
    }

    #[test]
    fn cells_are_written_as_fields_of_an_import() {
        let field = |name: &str, cell: Cell| {
            let mut out = Vec::new();
            Type::parse(name).field(&cell, &mut out).unwrap();
            out
        };
        assert_eq!(field("Nullable(String)", Cell::Null), b"\\N");
        assert_eq!(
            field("String", Cell::Text("a\tb\nc\\d\0".into())),
            b"a\\tb\\nc\\\\d\\0"
        );
        // A text that reads `\N` is not NULL.
        assert_eq!(field("String", Cell::Text("\\N".into())), b"\\\\N");
        assert_eq!(
            field("String", Cell::Bytes(vec![0xff, b'\t', 0])),
            b"\xff\\t\\0"
        );
        assert_eq!(
            field("Decimal(38, 10)", Cell::Decimal("1.50".into())),
            b"1.50"
        );
        // A composite value keeps the escapes it is written with.
        assert_eq!(
            field("Array(String)", Cell::Text("['a\\tb','c\td']".into())),
            b"['a\\tb','c\\td']"
        );
        // What the server wrote is what an import writes back.
        for (name, raw) in [
            ("String", &b"tab\\there\\\\bs"[..]),
            ("Array(String)", &b"['a\\tb','it\\'s']"[..]),
            ("Nullable(Int64)", &b"\\N"[..]),
            ("String", &b"\xff\\0\xfeA"[..]),
        ] {
            let parsed = Type::parse(name);
            assert_eq!(field(name, parsed.cell(raw)), raw, "{name}");
        }
    }

    #[test]
    fn an_answer_is_read_line_by_line_whatever_the_chunks_are() {
        let body = b"id\tname\nUInt8\tNullable(String)\n1\ta\\tb\n2\t\\N\n";
        for size in [1, 2, 3, 7, body.len()] {
            let mut reader = Reader::new(true);
            let mut rows = Vec::new();
            for chunk in body.chunks(size) {
                let stopped = reader
                    .feed(chunk, |line| {
                        if let Line::Row(cells) = line {
                            rows.push(cells);
                        }
                        Ok(true)
                    })
                    .unwrap();
                assert_eq!(stopped, None);
            }
            assert!(reader.leftover().is_empty());
            assert_eq!(reader.names, ["id", "name"]);
            assert_eq!(reader.types[1].name, "Nullable(String)");
            assert_eq!(
                rows,
                [
                    vec![Cell::Integer("1".into()), Cell::Text("a\tb".into())],
                    vec![Cell::Integer("2".into()), Cell::Null],
                ]
            );
        }
        // The reader stops when it is told to, and says where.
        let mut reader = Reader::new(true);
        let mut seen = 0;
        let stopped = reader
            .feed(body, |line| {
                seen += usize::from(matches!(line, Line::Row(_)));
                Ok(seen < 1)
            })
            .unwrap();
        assert_eq!(stopped, Some(body.len() - 5));
        assert_eq!(seen, 1);
        // A row with another number of fields is not an answer.
        let mut reader = Reader::new(true);
        assert!(
            reader
                .feed(b"a\tb\nUInt8\tUInt8\n1\n", |_| Ok(true))
                .is_err()
        );
    }

    #[test]
    fn a_failure_after_the_first_rows_is_told_from_the_rows() {
        let failure = |reader: &mut Reader, body: &[u8]| {
            let mut found = None;
            let stopped = reader
                .feed(body, |line| {
                    if let Line::Exception(text) = line {
                        found = Some(text);
                    }
                    Ok(found.is_none())
                })
                .unwrap();
            found.map(|text| (text, stopped))
        };
        // A current server frames it with carriage returns; the message
        // follows the place where the reader stopped.
        let framed = b"n\nUInt8\n1\n\r\n__exception__\r\ntag\r\nCode: 395. DB::Exception: boom\n";
        let (text, stopped) = failure(&mut Reader::new(true), framed).unwrap();
        assert!(text.is_empty());
        assert!(framed[stopped.unwrap()..].starts_with(b"__exception__"));
        // Behind a frame, a row that reads like a message is a row.
        let row = b"exception\nString\nCode: 60. DB::Exception: Unknown table (version 26.9)\n";
        assert!(failure(&mut Reader::new(true), row).is_none());
        // An older server appends the message as one more line.
        let appended =
            b"a\tb\nUInt8\tUInt8\n1\t2\nCode: 241. DB::Exception: Memory limit (version 22.8)\n";
        let (text, _) = failure(&mut Reader::new(false), appended).unwrap();
        assert!(text.starts_with(b"Code: 241."));
        // With one column it cannot be told from a row: it counts as a failure.
        assert!(failure(&mut Reader::new(false), row).is_some());
        // Any other line of an older server is a row or a broken answer.
        assert!(failure(&mut Reader::new(false), b"a\nString\nCode: red\n").is_none());
    }
}
