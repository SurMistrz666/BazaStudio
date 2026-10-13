//! A reply of the server as a grid: one cell for a single value, rows for a
//! list, two columns for a map, a table for records, and readable text for
//! whatever is nested deeper. The reports of the server that are text
//! (`INFO`, `CLIENT LIST`, `CLUSTER NODES`) become tables as well.
use super::value::{self, float_text, hex, human_size, reply_pairs, reply_text, text_of};
use crate::model::{Cell, QueryResult, ResultColumn};
use crate::sql::{MAX_RESULT_BYTES, MAX_RESULT_ROWS};
use redis::Value;

/// Longest text of a reply shown whole in a cell of the console.
const REPLY_TEXT_MAX: usize = 1024 * 1024;
/// Longest binary reply shown whole (as twice as many digits).
const REPLY_BINARY_MAX: usize = 64 * 1024;
/// Longest text of a nested reply inside one cell.
const NESTED_MAX: usize = 4000;
/// Most columns a list of records is spread over.
const RECORD_COLUMNS_MAX: usize = 40;

/// Columns and rows before they become a result.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Grid {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
}

impl Grid {
    fn one(column: &str, cell: Cell) -> Grid {
        Grid {
            columns: vec![column.into()],
            rows: vec![vec![cell]],
        }
    }

    /// The result: columns named after what their cells hold, rows cut at
    /// the limits of a result.
    pub(super) fn finish(mut self) -> QueryResult {
        let mut truncated = false;
        if self.rows.len() > MAX_RESULT_ROWS {
            self.rows.truncate(MAX_RESULT_ROWS);
            truncated = true;
        }
        let mut bytes = 0usize;
        let mut keep = self.rows.len();
        for (index, row) in self.rows.iter().enumerate() {
            bytes = bytes.saturating_add(row.iter().map(|cell| cell.display().len()).sum());
            if bytes > MAX_RESULT_BYTES {
                keep = index;
                truncated = true;
                break;
            }
        }
        self.rows.truncate(keep);
        let columns = self
            .columns
            .iter()
            .enumerate()
            .map(|(index, name)| ResultColumn {
                name: name.clone(),
                data_type: type_name(self.rows.iter().filter_map(|row| row.get(index))).into(),
            })
            .collect();
        QueryResult {
            columns,
            rows: self.rows,
            truncated,
            ..QueryResult::default()
        }
    }
}

/// What the cells of a column hold, when they agree.
fn type_name<'a>(cells: impl Iterator<Item = &'a Cell>) -> &'static str {
    let mut found = "";
    for cell in cells {
        let name = match cell {
            Cell::Null => continue,
            Cell::Integer(_) => "integer",
            Cell::Float(_) | Cell::Decimal(_) => "double",
            Cell::Bool(_) => "boolean",
            Cell::Json(_) => "json",
            Cell::Text(_) => "string",
            Cell::Bytes(_) | Cell::Unsupported(_) => "bytes",
        };
        if found.is_empty() {
            found = name;
        } else if found != name {
            return "";
        }
    }
    found
}

fn bulk_cell(bytes: &[u8]) -> Cell {
    match text_of(bytes) {
        Some(text) if bytes.len() <= REPLY_TEXT_MAX => Cell::Text(text.to_owned()),
        Some(text) => Cell::Unsupported(value::preview(text, bytes.len())),
        None if bytes.len() <= REPLY_BINARY_MAX => Cell::Unsupported(hex(bytes)),
        None => Cell::Unsupported(value::preview(&hex(&bytes[..120]), bytes.len())),
    }
}

fn is_scalar(value: &Value) -> bool {
    !matches!(
        value,
        Value::Array(_)
            | Value::Set(_)
            | Value::Map(_)
            | Value::Push { .. }
            | Value::Attribute { .. }
    )
}

/// A value as one cell: typed when it is a single value, text when nested.
pub(super) fn cell(value: &Value) -> Cell {
    match value {
        Value::Nil => Cell::Null,
        Value::Int(number) => Cell::Integer(number.to_string()),
        Value::Double(number) => Cell::Float(float_text(*number)),
        Value::Boolean(flag) => Cell::Bool(*flag),
        Value::BulkString(bytes) => bulk_cell(bytes),
        Value::SimpleString(text) => Cell::Text(text.clone()),
        Value::Okay => Cell::Text("OK".into()),
        Value::VerbatimString { text, .. } => Cell::Text(text.clone()),
        Value::Attribute { data, .. } => cell(data),
        Value::ServerError(_) | Value::Array(_) | Value::Set(_) | Value::Map(_) => {
            Cell::Text(nested(value))
        }
        other => Cell::Text(nested(other)),
    }
}

/// A value inside another one, as text.
fn nested(value: &Value) -> String {
    let mut out = String::new();
    write_nested(value, &mut out);
    if out.len() > NESTED_MAX {
        let mut end = NESTED_MAX;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push('…');
    }
    out
}

fn write_nested(value: &Value, out: &mut String) {
    if out.len() > NESTED_MAX {
        return;
    }
    let list = |items: &[Value], out: &mut String| {
        out.push('[');
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            write_nested(item, out);
        }
        out.push(']');
    };
    match value {
        Value::Nil => out.push_str("(nil)"),
        Value::Int(number) => out.push_str(&number.to_string()),
        Value::Double(number) => out.push_str(&float_text(*number)),
        Value::Boolean(flag) => out.push_str(&flag.to_string()),
        Value::Okay => out.push_str("OK"),
        Value::SimpleString(text) | Value::VerbatimString { text, .. } => out.push_str(&bare(text)),
        Value::BulkString(bytes) => match text_of(bytes) {
            Some(text) => out.push_str(&bare(text)),
            None if bytes.len() <= 64 => out.push_str(&hex(bytes)),
            None => out.push_str(&format!(
                "{}… ({})",
                hex(&bytes[..64]),
                human_size(bytes.len())
            )),
        },
        Value::Array(items) | Value::Set(items) => list(items, out),
        Value::Push { data, .. } => list(data, out),
        Value::Map(pairs) => {
            out.push('{');
            for (index, (name, item)) in pairs.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_nested(name, out);
                out.push_str(": ");
                write_nested(item, out);
            }
            out.push('}');
        }
        Value::Attribute { data, .. } => write_nested(data, out),
        // Only the code word: the sentence of the server stays behind.
        Value::ServerError(error) => out.push_str(&format!("(error) {}", error.code())),
        _ => out.push_str("(?)"),
    }
}

/// Text inside a nested reply: bare when it reads as one word, otherwise
/// quoted like a JSON string.
fn bare(text: &str) -> String {
    let simple = !text.is_empty()
        && text.len() <= 200
        && text
            .chars()
            .all(|c| !c.is_whitespace() && !matches!(c, '"' | ',' | '[' | ']' | '{' | '}'));
    if simple {
        text.to_owned()
    } else {
        serde_json::to_string(text).unwrap_or_default()
    }
}

fn unwrap(value: Value) -> Value {
    match value {
        Value::Attribute { data, .. } => unwrap(*data),
        Value::Push { data, .. } => Value::Array(data),
        other => other,
    }
}

/// Names of the fields of a record: a map, or a flat list of alternating
/// names and values with at least two fields.
fn record_names(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Map(pairs) => pairs.iter().map(|(name, _)| reply_text(name)).collect(),
        Value::Array(items) if items.len() >= 4 && items.len() % 2 == 0 => items
            .iter()
            .step_by(2)
            .map(|name| match name {
                Value::BulkString(bytes) => text_of(bytes).map(str::to_owned),
                Value::SimpleString(text) => Some(text.clone()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// A list of records with the same fields as a table.
fn records(items: &[Value]) -> Option<Grid> {
    let names = record_names(items.first()?)?;
    if names.is_empty() || names.len() > RECORD_COLUMNS_MAX {
        return None;
    }
    let unique: std::collections::HashSet<_> = names.iter().collect();
    if unique.len() != names.len() {
        return None;
    }
    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        if record_names(item)? != names {
            return None;
        }
        let pairs = reply_pairs(item.clone())?;
        rows.push(pairs.iter().map(|(_, value)| cell(value)).collect());
    }
    Some(Grid {
        columns: names,
        rows,
    })
}

/// A reply of any shape.
pub(super) fn generic(value: Value) -> Grid {
    match unwrap(value) {
        Value::Map(pairs) => pairs_grid(pairs, "field", "value"),
        Value::Array(items) | Value::Set(items) => {
            if items.is_empty() {
                return Grid {
                    columns: vec!["value".into()],
                    rows: Vec::new(),
                };
            }
            if items.iter().all(is_scalar) {
                return Grid {
                    columns: vec!["value".into()],
                    rows: items.iter().map(|item| vec![cell(item)]).collect(),
                };
            }
            if let Some(grid) = records(&items) {
                return grid;
            }
            // Lists of the same length side by side.
            let width = match &items[0] {
                Value::Array(first) => first.len(),
                _ => 0,
            };
            if (2..=16).contains(&width)
                && items
                    .iter()
                    .all(|item| matches!(item, Value::Array(inner) if inner.len() == width))
            {
                return Grid {
                    columns: (1..=width).map(|n| n.to_string()).collect(),
                    rows: items
                        .iter()
                        .map(|item| match item {
                            Value::Array(inner) => inner.iter().map(cell).collect(),
                            _ => Vec::new(),
                        })
                        .collect(),
                };
            }
            Grid {
                columns: vec!["value".into()],
                rows: items.iter().map(|item| vec![cell(item)]).collect(),
            }
        }
        scalar => Grid::one("reply", cell(&scalar)),
    }
}

fn pairs_grid(pairs: Vec<(Value, Value)>, name: &str, value: &str) -> Grid {
    Grid {
        columns: vec![name.into(), value.into()],
        rows: pairs
            .iter()
            .map(|(name, value)| vec![cell(name), cell(value)])
            .collect(),
    }
}

/// `INFO`: one row per property, with its section.
pub(super) fn info(text: &str) -> Vec<(String, String, String)> {
    let mut section = String::new();
    let mut rows = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix('#') {
            section = name.trim().to_owned();
        } else if let Some((name, value)) = line.split_once(':') {
            rows.push((section.clone(), name.to_owned(), value.to_owned()));
        }
    }
    rows
}

/// The value of an `INFO` property.
pub(super) fn info_value<'a>(rows: &'a [(String, String, String)], name: &str) -> Option<&'a str> {
    rows.iter()
        .find(|(_, property, _)| property == name)
        .map(|(_, _, value)| value.as_str())
}

fn info_grid(text: &str) -> Grid {
    Grid {
        columns: vec!["section".into(), "property".into(), "value".into()],
        rows: info(text)
            .into_iter()
            .map(|(section, name, value)| {
                vec![
                    Cell::Text(section),
                    Cell::Text(name),
                    number_or_text(&value),
                ]
            })
            .collect(),
    }
}

fn number_or_text(text: &str) -> Cell {
    if !text.is_empty()
        && text.len() <= 18
        && text.bytes().all(|byte| byte.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'))
    {
        Cell::Integer(text.to_owned())
    } else {
        Cell::Text(text.to_owned())
    }
}

/// Lines of `name=value` words (`CLIENT LIST`) as a table: the columns
/// people look for first, then the rest in the order of the server.
pub(super) fn clients(text: &str) -> Grid {
    const FIRST: [&str; 9] = [
        "id", "addr", "name", "user", "db", "age", "idle", "cmd", "flags",
    ];
    let records: Vec<Vec<(&str, &str)>> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            line.split(' ')
                .filter_map(|word| word.split_once('='))
                .collect()
        })
        .collect();
    let mut columns: Vec<String> = Vec::new();
    for name in FIRST {
        if records
            .iter()
            .any(|record| record.iter().any(|(key, _)| *key == name))
        {
            columns.push(name.into());
        }
    }
    for record in &records {
        for (key, _) in record {
            if !columns.iter().any(|column| column == key) {
                columns.push((*key).into());
            }
        }
    }
    let numeric: Vec<bool> = columns
        .iter()
        .map(|column| {
            records.iter().all(|record| {
                record
                    .iter()
                    .find(|(key, _)| key == column)
                    .is_none_or(|(_, value)| value.parse::<i64>().is_ok())
            })
        })
        .collect();
    let rows = records
        .iter()
        .map(|record| {
            columns
                .iter()
                .zip(&numeric)
                .map(
                    |(column, numeric)| match record.iter().find(|(key, _)| key == column) {
                        None => Cell::Null,
                        Some((_, value)) if *numeric => Cell::Integer((*value).into()),
                        Some((_, value)) => Cell::Text((*value).into()),
                    },
                )
                .collect()
        })
        .collect();
    Grid { columns, rows }
}

/// `CLUSTER NODES`: one row per node.
fn cluster_nodes(text: &str) -> Grid {
    const NAMES: [&str; 9] = [
        "id",
        "address",
        "flags",
        "master",
        "ping sent",
        "pong received",
        "config epoch",
        "link",
        "slots",
    ];
    Grid {
        columns: NAMES.iter().map(|name| (*name).into()).collect(),
        rows: text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let mut words = line.split(' ');
                let mut row: Vec<Cell> = (0..8)
                    .map(|_| Cell::Text(words.next().unwrap_or_default().to_owned()))
                    .collect();
                row.push(Cell::Text(words.collect::<Vec<_>>().join(" ")));
                row
            })
            .collect(),
    }
}

/// `SCAN` and its relatives: the next cursor beside every element.
fn scan(value: Value, columns: &[&str]) -> Option<Grid> {
    let Value::Array(parts) = unwrap(value) else {
        return None;
    };
    let mut parts = parts.into_iter();
    let cursor = cell(&parts.next()?);
    let items = value::reply_items(parts.next()?)?;
    let width = columns.len();
    let mut names = vec!["next cursor".to_owned()];
    names.extend(columns.iter().map(|name| (*name).to_owned()));
    let mut rows: Vec<Vec<Cell>> = if width == 1 {
        items
            .iter()
            .map(|item| vec![cursor.clone(), cell(item)])
            .collect()
    } else {
        items
            .chunks(width)
            .map(|chunk| {
                let mut row = vec![cursor.clone()];
                row.extend(chunk.iter().map(cell));
                row.resize(width + 1, Cell::Null);
                row
            })
            .collect()
    };
    if rows.is_empty() {
        // Nothing in this step: the cursor still has to be seen.
        let mut row = vec![cursor];
        row.resize(width + 1, Cell::Null);
        rows.push(row);
    }
    Some(Grid {
        columns: names,
        rows,
    })
}

/// One entry of a stream: its id and its fields.
pub(super) type Entry = (String, Vec<(Vec<u8>, Vec<u8>)>);

/// Read one entry of a stream out of a reply.
pub(super) fn entry(value: Value) -> Option<Entry> {
    let Value::Array(parts) = unwrap(value) else {
        return None;
    };
    let mut parts = parts.into_iter();
    let id = reply_text(&parts.next()?)?;
    let fields = match parts.next() {
        Some(Value::Nil) | None => Vec::new(),
        Some(fields) => reply_pairs(fields)?
            .iter()
            .filter_map(|(name, value)| {
                Some((value::reply_bytes(name)?, value::reply_bytes(value)?))
            })
            .collect(),
    };
    Some((id, fields))
}

/// Entries of a stream as a table: the id, then one column per field name
/// when the entries share a few of them, otherwise the fields as one text.
fn entries(items: Vec<Value>, lead: Option<(&str, Cell)>) -> Option<(Vec<String>, Vec<Vec<Cell>>)> {
    let parsed: Vec<_> = items.into_iter().map(entry).collect::<Option<_>>()?;
    let mut names: Vec<Vec<u8>> = Vec::new();
    for (_, fields) in &parsed {
        for (name, _) in fields {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
    }
    let spread = !names.is_empty()
        && names.len() <= 24
        && names
            .iter()
            .all(|name| text_of(name).is_some_and(|text| text != "id" && text != "stream"));
    let mut columns: Vec<String> = lead.iter().map(|(name, _)| (*name).to_owned()).collect();
    columns.push("id".into());
    if spread {
        columns.extend(
            names
                .iter()
                .map(|name| String::from_utf8_lossy(name).into_owned()),
        );
    } else {
        columns.push("fields".into());
    }
    let rows = parsed
        .into_iter()
        .map(|(id, fields)| {
            let mut row: Vec<Cell> = lead.iter().map(|(_, cell)| cell.clone()).collect();
            row.push(Cell::Text(id));
            if spread {
                for name in &names {
                    row.push(
                        fields
                            .iter()
                            .find(|(field, _)| field == name)
                            .map_or(Cell::Null, |(_, value)| bulk_cell(value)),
                    );
                }
            } else {
                row.push(Cell::Text(value::entry_json(&fields)));
            }
            row
        })
        .collect();
    Some((columns, rows))
}

fn has_word(words: &[Vec<u8>], name: &str) -> bool {
    words
        .iter()
        .skip(1)
        .any(|word| word.eq_ignore_ascii_case(name.as_bytes()))
}

/// The reply of a command of the console as a grid. `name` and `sub` are the
/// first two words in upper case.
pub(super) fn render(name: &str, sub: &str, words: &[Vec<u8>], value: Value) -> Grid {
    let value = unwrap(value);
    let report = |value: &Value| match value {
        Value::BulkString(bytes) => text_of(bytes).map(str::to_owned),
        Value::VerbatimString { text, .. } | Value::SimpleString(text) => Some(text.clone()),
        _ => None,
    };
    let shaped = match (name, sub) {
        ("INFO", _) => report(&value).map(|text| info_grid(&text)),
        ("CLIENT", "LIST") | ("CLIENT", "INFO") => report(&value).map(|text| clients(&text)),
        ("CLUSTER", "NODES") => report(&value).map(|text| cluster_nodes(&text)),
        ("CLUSTER", "INFO") => report(&value).map(|text| Grid {
            columns: vec!["property".into(), "value".into()],
            rows: info(&text)
                .into_iter()
                .map(|(_, name, value)| vec![Cell::Text(name), number_or_text(&value)])
                .collect(),
        }),
        ("SCAN", _) => scan(value.clone(), &["key"]),
        ("SSCAN", _) => scan(value.clone(), &["member"]),
        ("HSCAN", _) if has_word(words, "NOVALUES") => scan(value.clone(), &["field"]),
        ("HSCAN", _) => scan(value.clone(), &["field", "value"]),
        ("ZSCAN", _) if has_word(words, "NOSCORES") => scan(value.clone(), &["member"]),
        ("ZSCAN", _) => scan(value.clone(), &["member", "score"]),
        ("XRANGE", _) | ("XREVRANGE", _) => match &value {
            Value::Array(items) => {
                entries(items.clone(), None).map(|(columns, rows)| Grid { columns, rows })
            }
            _ => None,
        },
        ("XREAD", _) | ("XREADGROUP", _) => reply_pairs(value.clone()).and_then(|streams| {
            let mut columns = Vec::new();
            let mut rows = Vec::new();
            for (stream, items) in streams {
                let Value::Array(items) = items else {
                    return None;
                };
                let (names, mut part) = entries(items, Some(("stream", cell(&stream))))?;
                if columns.is_empty() {
                    columns = names;
                } else if columns != names {
                    return None;
                }
                rows.append(&mut part);
            }
            (!columns.is_empty()).then_some(Grid { columns, rows })
        }),
        ("HGETALL", _)
        | ("CONFIG", "GET")
        | ("MEMORY", "STATS")
        | ("XINFO", "STREAM")
        | ("ACL", "GETUSER")
        | ("FUNCTION", "STATS")
        | ("CLIENT", "TRACKINGINFO") => {
            reply_pairs(value.clone()).map(|pairs| pairs_grid(pairs, "field", "value"))
        }
        ("HRANDFIELD", _) if has_word(words, "WITHVALUES") => {
            reply_pairs(value.clone()).map(|pairs| pairs_grid(pairs, "field", "value"))
        }
        ("ZPOPMIN", _) | ("ZPOPMAX", _) => {
            reply_pairs(value.clone()).map(|pairs| pairs_grid(pairs, "member", "score"))
        }
        (name, _) if name.starts_with('Z') && has_word(words, "WITHSCORES") => {
            reply_pairs(value.clone()).map(|pairs| pairs_grid(pairs, "member", "score"))
        }
        ("SLOWLOG", "GET") => match &value {
            Value::Array(items) => Some(slow_log(items)),
            _ => None,
        },
        _ => None,
    };
    shaped.unwrap_or_else(|| generic(value))
}

/// `SLOWLOG GET`: one row per slow command.
pub(super) fn slow_log(items: &[Value]) -> Grid {
    let rows = items
        .iter()
        .filter_map(|item| match item {
            Value::Array(parts) => Some(parts),
            _ => None,
        })
        .map(|parts| {
            let at = |index: usize| parts.get(index).map_or(Cell::Null, cell);
            let time = parts
                .get(1)
                .and_then(value::reply_int)
                .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
                .map_or(Cell::Null, |time| {
                    Cell::Text(time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                });
            let command = match parts.get(3) {
                Some(Value::Array(words)) => Cell::Text(
                    words
                        .iter()
                        .filter_map(value::reply_bytes)
                        .map(|word| super::command::quote(&word))
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                _ => Cell::Null,
            };
            vec![at(0), time, at(2), command, at(4), at(5)]
        })
        .collect();
    Grid {
        columns: [
            "id",
            "time",
            "microseconds",
            "command",
            "client",
            "client name",
        ]
        .map(String::from)
        .to_vec(),
        rows,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(text: &str) -> Value {
        Value::BulkString(text.as_bytes().to_vec())
    }
    fn texts(grid: &Grid) -> Vec<Vec<String>> {
        grid.rows
            .iter()
            .map(|row| row.iter().map(Cell::display).collect())
            .collect()
    }
    fn run(line: &str, value: Value) -> Grid {
        let words = super::super::command::words(line).unwrap();
        let name = String::from_utf8_lossy(&words[0]).to_ascii_uppercase();
        let sub = words
            .get(1)
            .map(|word| String::from_utf8_lossy(word).to_ascii_uppercase())
            .unwrap_or_default();
        render(&name, &sub, &words, value)
    }

    #[test]
    fn a_single_value_is_one_typed_cell() {
        for (value, expected) in [
            (Value::Okay, Cell::Text("OK".into())),
            (
                Value::SimpleString("PONG".into()),
                Cell::Text("PONG".into()),
            ),
            (Value::Int(-7), Cell::Integer("-7".into())),
            (Value::Double(1.5), Cell::Float("1.5".into())),
            (Value::Boolean(true), Cell::Bool(true)),
            (Value::Nil, Cell::Null),
            (bulk("zażółć 🦀"), Cell::Text("zażółć 🦀".into())),
            (
                Value::BulkString(vec![0xff, 0x00]),
                Cell::Unsupported("0xff00".into()),
            ),
        ] {
            let grid = run("GET k", value);
            assert_eq!(grid.columns, ["reply"]);
            assert_eq!(grid.rows, [vec![expected]]);
        }
        let result = run("INCR n", Value::Int(4)).finish();
        assert_eq!(result.columns[0].data_type, "integer");
        assert!(!result.truncated);
    }

    #[test]
    fn a_list_is_rows_and_an_empty_one_keeps_its_column() {
        let grid = run(
            "LRANGE l 0 -1",
            Value::Array(vec![bulk("a"), Value::Nil, bulk("c")]),
        );
        assert_eq!(grid.columns, ["value"]);
        assert_eq!(texts(&grid), [["a"], ["NULL"], ["c"]]);
        assert_eq!(grid.rows[1][0], Cell::Null);
        let empty = run("KEYS none*", Value::Array(vec![]));
        assert_eq!(empty.columns, ["value"]);
        assert!(empty.rows.is_empty());
        // A RESP3 set is a list as well.
        let set = run("SMEMBERS s", Value::Set(vec![bulk("x")]));
        assert_eq!(texts(&set), [["x"]]);
    }

    #[test]
    fn a_map_is_two_columns_in_both_protocol_versions() {
        let flat = Value::Array(vec![bulk("name"), bulk("Jan"), bulk("age"), bulk("41")]);
        let map = Value::Map(vec![(bulk("name"), bulk("Jan")), (bulk("age"), bulk("41"))]);
        for value in [flat, map] {
            let grid = run("HGETALL user:1", value);
            assert_eq!(grid.columns, ["field", "value"]);
            assert_eq!(texts(&grid), [["name", "Jan"], ["age", "41"]]);
        }
        let config = run(
            "CONFIG GET max*",
            Value::Array(vec![bulk("maxmemory"), bulk("0")]),
        );
        assert_eq!(config.columns, ["field", "value"]);
        // Members with their scores: flat in RESP2, pairs in RESP3.
        let flat = Value::Array(vec![bulk("a"), bulk("1"), bulk("b"), bulk("2.5")]);
        let pairs = Value::Array(vec![
            Value::Array(vec![bulk("a"), Value::Double(1.0)]),
            Value::Array(vec![bulk("b"), Value::Double(2.5)]),
        ]);
        for value in [flat, pairs] {
            let grid = run("ZRANGE z 0 -1 WITHSCORES", value);
            assert_eq!(grid.columns, ["member", "score"]);
            assert_eq!(texts(&grid), [["a", "1"], ["b", "2.5"]]);
        }
        // Without the word it is a plain list.
        assert_eq!(
            run("ZRANGE z 0 -1", Value::Array(vec![bulk("a")])).columns,
            ["value"]
        );
    }

    #[test]
    fn nested_replies_stay_readable() {
        let value = Value::Array(vec![
            Value::Int(1),
            Value::Array(vec![
                bulk("a b"),
                Value::Nil,
                Value::Array(vec![Value::Int(2)]),
            ]),
            Value::Map(vec![(bulk("k"), Value::Double(0.5))]),
            Value::BulkString(vec![0xff]),
        ]);
        let grid = run("EVAL x 0", value);
        assert_eq!(grid.columns, ["value"]);
        assert_eq!(
            texts(&grid),
            [["1"], ["[\"a b\", (nil), [2]]"], ["{k: 0.5}"], ["0xff"]]
        );
        // Lists of one length stand side by side.
        let table = run(
            "GEOPOS g a b",
            Value::Array(vec![
                Value::Array(vec![bulk("13.36"), bulk("38.11")]),
                Value::Array(vec![bulk("15.08"), bulk("37.50")]),
            ]),
        );
        assert_eq!(table.columns, ["1", "2"]);
        assert_eq!(texts(&table), [["13.36", "38.11"], ["15.08", "37.50"]]);
        // An error inside a transaction shows its code word only.
        let exec = run(
            "EXEC",
            Value::Array(vec![
                Value::Okay,
                redis::parse_redis_value(b"-WRONGTYPE secret detail\r\n").unwrap(),
            ]),
        );
        assert_eq!(texts(&exec), [["OK"], ["(error) WRONGTYPE"]]);
    }

    #[test]
    fn records_with_the_same_fields_are_a_table() {
        let group = |name: &str, pending: i64| {
            Value::Array(vec![
                bulk("name"),
                bulk(name),
                bulk("consumers"),
                Value::Int(2),
                bulk("pending"),
                Value::Int(pending),
            ])
        };
        let grid = run(
            "XINFO GROUPS s",
            Value::Array(vec![group("a", 0), group("b", 3)]),
        );
        assert_eq!(grid.columns, ["name", "consumers", "pending"]);
        assert_eq!(texts(&grid), [["a", "2", "0"], ["b", "2", "3"]]);
        assert_eq!(grid.rows[1][2], Cell::Integer("3".into()));
        // The same from RESP3 maps.
        let map = |name: &str| {
            Value::Map(vec![
                (bulk("name"), bulk(name)),
                (bulk("ver"), Value::Int(1)),
            ])
        };
        let grid = run(
            "MODULE LIST",
            Value::Array(vec![map("json"), map("search")]),
        );
        assert_eq!(grid.columns, ["name", "ver"]);
    }

    #[test]
    fn info_and_the_client_list_are_tables() {
        let text = "# Server\r\nredis_version:8.2.1\r\ntcp_port:6379\r\n\r\n# Keyspace\r\ndb0:keys=3,expires=1,avg_ttl=0\r\n";
        let grid = run("INFO", bulk(text));
        assert_eq!(grid.columns, ["section", "property", "value"]);
        assert_eq!(
            texts(&grid),
            [
                ["Server", "redis_version", "8.2.1"],
                ["Server", "tcp_port", "6379"],
                ["Keyspace", "db0", "keys=3,expires=1,avg_ttl=0"],
            ]
        );
        assert_eq!(grid.rows[1][2], Cell::Integer("6379".into()));
        assert_eq!(info_value(&info(text), "redis_version"), Some("8.2.1"));
        // RESP3 sends the same report as a verbatim string.
        let verbatim = Value::VerbatimString {
            format: redis::VerbatimFormat::Text,
            text: text.into(),
        };
        assert_eq!(run("info server", verbatim).rows.len(), 3);

        let list = "id=7 addr=172.18.0.1:40112 laddr=172.18.0.2:6379 fd=9 name=baza-studio age=12 idle=0 flags=N db=0 cmd=client|list user=default resp=3\nid=9 addr=172.18.0.1:40200 laddr=172.18.0.2:6379 fd=10 name= age=3 idle=3 flags=b db=2 cmd=blpop user=app resp=2\n";
        let grid = run("CLIENT LIST", bulk(list));
        assert_eq!(
            grid.columns,
            [
                "id", "addr", "name", "user", "db", "age", "idle", "cmd", "flags", "laddr", "fd",
                "resp"
            ]
        );
        assert_eq!(grid.rows.len(), 2);
        assert_eq!(grid.rows[1][0], Cell::Integer("9".into()));
        assert_eq!(grid.rows[1][2], Cell::Text(String::new()));
        assert_eq!(grid.rows[1][7], Cell::Text("blpop".into()));
    }

    #[test]
    fn a_scan_step_shows_its_cursor_and_stream_entries_their_fields() {
        let step = Value::Array(vec![bulk("17"), Value::Array(vec![bulk("a"), bulk("b")])]);
        let grid = run("SCAN 0", step);
        assert_eq!(grid.columns, ["next cursor", "key"]);
        assert_eq!(texts(&grid), [["17", "a"], ["17", "b"]]);
        let done = run(
            "SCAN 17",
            Value::Array(vec![bulk("0"), Value::Array(vec![])]),
        );
        assert_eq!(texts(&done), [["0", "NULL"]]);
        let hash = run(
            "HSCAN h 0",
            Value::Array(vec![bulk("0"), Value::Array(vec![bulk("f"), bulk("v")])]),
        );
        assert_eq!(hash.columns, ["next cursor", "field", "value"]);
        assert_eq!(texts(&hash), [["0", "f", "v"]]);

        let item = |id: &str, fields: &[&str]| {
            Value::Array(vec![
                bulk(id),
                Value::Array(fields.iter().map(|field| bulk(field)).collect()),
            ])
        };
        let grid = run(
            "XRANGE s - +",
            Value::Array(vec![
                item("1-0", &["sensor", "a", "value", "21.5"]),
                item("2-0", &["sensor", "b"]),
            ]),
        );
        assert_eq!(grid.columns, ["id", "sensor", "value"]);
        assert_eq!(texts(&grid), [["1-0", "a", "21.5"], ["2-0", "b", "NULL"]]);
        let read = run(
            "XREAD STREAMS s 0",
            Value::Array(vec![Value::Array(vec![
                bulk("s"),
                Value::Array(vec![item("1-0", &["sensor", "a"])]),
            ])]),
        );
        assert_eq!(read.columns, ["stream", "id", "sensor"]);
        assert_eq!(texts(&read), [["s", "1-0", "a"]]);
    }

    #[test]
    fn a_result_is_cut_at_the_row_limit() {
        let many = Value::Array(
            (0..MAX_RESULT_ROWS + 5)
                .map(|n| Value::Int(n as i64))
                .collect(),
        );
        let result = run("LRANGE l 0 -1", many).finish();
        assert_eq!(result.rows.len(), MAX_RESULT_ROWS);
        assert!(result.truncated);
        assert_eq!(result.columns[0].data_type, "integer");
    }
}
