//! Keys and values of Redis are bytes. This module decides how they are
//! shown and how what the grid hands back is turned into the same bytes
//! again, so that nothing is changed by being looked at.
//!
//! * Text is UTF-8 without control characters (tab, line feed and carriage
//!   return are text): a `Cell::Text`, edited in place.
//! * Anything else is shown as hexadecimal digits behind the marker `0x`, in
//!   a `Cell::Unsupported`, which the grid shows and never edits. The cell
//!   keeps every byte, so a row is still addressed, renamed, given a
//!   lifetime or removed by exactly the bytes the server holds.
//! * A value that is too long for a cell is a preview that ends in `…` and
//!   the size: also read only, and never taken for the value itself.
//!
//! A small hash, list, set or sorted set is one JSON text in the `value`
//! column of the key list: complete, so it can be edited and exported there.
use crate::model::Cell;
use anyhow::{Result, anyhow, bail, ensure};
use redis::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Marker in front of the hexadecimal digits of bytes that are not text.
pub(super) const HEX_MARK: &str = "0x";
/// Longest text value shown whole in a cell, in bytes.
pub(super) const TEXT_CELL_MAX: usize = 64 * 1024;
/// Longest binary value shown whole (as twice as many digits).
pub(super) const BINARY_CELL_MAX: usize = 4 * 1024;
/// Most members of a collection shown as one complete JSON text.
pub(super) const INLINE_MEMBERS_MAX: usize = 64;
/// Longest complete JSON text of a collection.
pub(super) const INLINE_JSON_MAX: usize = 16 * 1024;
/// Characters of a preview before the ellipsis.
const PREVIEW_CHARS: usize = 240;

/// The text of bytes that are text: UTF-8 without control characters other
/// than tab, line feed and carriage return.
pub(super) fn text_of(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    text.chars()
        .all(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r'))
        .then_some(text)
}

pub(super) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(HEX_MARK.len() + bytes.len() * 2);
    out.push_str(HEX_MARK);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The bytes behind `0x…`, when the text is exactly a marker and an even
/// number of hexadecimal digits.
pub(super) fn unhex(text: &str) -> Option<Vec<u8>> {
    let digits = text.strip_prefix(HEX_MARK)?.as_bytes();
    if digits.len() % 2 != 0 {
        return None;
    }
    digits
        .chunks(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some((high * 16 + low) as u8)
        })
        .collect()
}

/// A name (key, field, member): always complete, whatever its length,
/// because a row is addressed by it.
pub(super) fn name_cell(bytes: &[u8]) -> Cell {
    match text_of(bytes) {
        Some(text) => Cell::Text(text.to_owned()),
        None => Cell::Unsupported(hex(bytes)),
    }
}

/// A value: complete when it fits a cell, otherwise a preview.
pub(super) fn value_cell(bytes: &[u8]) -> Cell {
    match text_of(bytes) {
        Some(text) if bytes.len() <= TEXT_CELL_MAX => Cell::Text(text.to_owned()),
        Some(text) => Cell::Unsupported(preview(text, bytes.len())),
        None if bytes.len() <= BINARY_CELL_MAX => Cell::Unsupported(hex(bytes)),
        None => Cell::Unsupported(preview(&hex(&bytes[..PREVIEW_CHARS / 2]), bytes.len())),
    }
}

/// The start of a long value, an ellipsis and its size.
pub(super) fn preview(text: &str, size: usize) -> String {
    let head: String = text.chars().take(PREVIEW_CHARS).collect();
    format!("{head}… ({})", human_size(size))
}

/// The start of something that has no size in bytes: a collection.
pub(super) fn preview_of(text: &str) -> String {
    let head: String = text.chars().take(PREVIEW_CHARS).collect();
    format!("{head}…")
}

/// Whether a text reads like one of the previews above: what a file made
/// from a page holds in place of a value that did not fit its cell.
pub(super) fn looks_cut(text: &str) -> bool {
    if text.ends_with('…') {
        return true;
    }
    text.rsplit_once("… (").is_some_and(|(_, size)| {
        size.strip_suffix(')')
            .and_then(|size| size.split_once(' '))
            .is_some_and(|(number, unit)| {
                number.parse::<f64>().is_ok() && matches!(unit, "B" | "KiB" | "MiB" | "GiB")
            })
    })
}

pub(super) fn human_size(bytes: usize) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The bytes a cell stands for, when it stands for all of them: text, a
/// number as it is written, or `0x…` in a read-only cell. `None` for NULL,
/// for a preview and for a cell that is not a value.
pub(super) fn bytes_of(cell: &Cell) -> Option<Vec<u8>> {
    match cell {
        Cell::Text(text) | Cell::Json(text) => Some(text.clone().into_bytes()),
        Cell::Integer(text) | Cell::Decimal(text) | Cell::Float(text) => {
            Some(text.clone().into_bytes())
        }
        Cell::Bool(value) => Some(value.to_string().into_bytes()),
        Cell::Bytes(bytes) => Some(bytes.clone()),
        Cell::Unsupported(text) => unhex(text),
        Cell::Null => None,
    }
}

/// A name out of the key of a row.
pub(super) fn name_of(key: &[(String, Cell)], column: &str) -> Result<Vec<u8>> {
    key.iter()
        .find(|(name, _)| name == column)
        .and_then(|(_, cell)| bytes_of(cell))
        .ok_or_else(|| anyhow!("primary_key_required"))
}

/// A bulk string, a simple string or a number of a reply as bytes.
pub(super) fn reply_bytes(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::BulkString(bytes) => Some(bytes.clone()),
        Value::SimpleString(text) => Some(text.clone().into_bytes()),
        Value::VerbatimString { text, .. } => Some(text.clone().into_bytes()),
        Value::Okay => Some(b"OK".to_vec()),
        Value::Int(number) => Some(number.to_string().into_bytes()),
        Value::Double(number) => Some(float_text(*number).into_bytes()),
        Value::Boolean(flag) => Some(if *flag { b"1".to_vec() } else { b"0".to_vec() }),
        _ => None,
    }
}

pub(super) fn reply_text(value: &Value) -> Option<String> {
    reply_bytes(value).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

pub(super) fn reply_int(value: &Value) -> Option<i64> {
    match value {
        Value::Int(number) => Some(*number),
        Value::Boolean(flag) => Some(i64::from(*flag)),
        Value::BulkString(_) | Value::SimpleString(_) => reply_text(value)?.parse().ok(),
        _ => None,
    }
}

/// The elements of an array, a set or a push; a map flattened to its pairs.
pub(super) fn reply_items(value: Value) -> Option<Vec<Value>> {
    match value {
        Value::Array(items) | Value::Set(items) => Some(items),
        Value::Push { data, .. } => Some(data),
        Value::Map(pairs) => Some(pairs.into_iter().flat_map(|(k, v)| [k, v]).collect()),
        Value::Attribute { data, .. } => reply_items(*data),
        _ => None,
    }
}

/// Pairs of a reply that is a map (RESP3) or a flat list of alternating
/// names and values (RESP2), or a list of two-element lists.
pub(super) fn reply_pairs(value: Value) -> Option<Vec<(Value, Value)>> {
    match value {
        Value::Map(pairs) => Some(pairs),
        Value::Attribute { data, .. } => reply_pairs(*data),
        Value::Array(items) | Value::Set(items) => {
            if !items.is_empty()
                && items
                    .iter()
                    .all(|item| matches!(item, Value::Array(pair) if pair.len() == 2))
            {
                return Some(
                    items
                        .into_iter()
                        .filter_map(|item| match item {
                            Value::Array(pair) => {
                                let mut pair = pair.into_iter();
                                Some((pair.next()?, pair.next()?))
                            }
                            _ => None,
                        })
                        .collect(),
                );
            }
            if items.len() % 2 != 0 {
                return None;
            }
            let mut pairs = Vec::with_capacity(items.len() / 2);
            let mut items = items.into_iter();
            while let (Some(name), Some(value)) = (items.next(), items.next()) {
                pairs.push((name, value));
            }
            Some(pairs)
        }
        _ => None,
    }
}

/// A number the way Redis writes a score: whole numbers without a fraction.
pub(super) fn float_text(number: f64) -> String {
    if number.is_infinite() {
        return if number > 0.0 { "inf" } else { "-inf" }.into();
    }
    if number.is_nan() {
        return "nan".into();
    }
    number.to_string()
}

/// A score from what the grid or a file gives: a number, `inf` or `-inf`.
pub(super) fn parse_score(text: &str) -> Result<String> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    if matches!(lower.as_str(), "inf" | "+inf" | "-inf") {
        return Ok(lower);
    }
    let number: f64 = text.parse().map_err(|_| anyhow!("redis_score_invalid"))?;
    ensure!(number.is_finite(), "redis_score_invalid");
    Ok(text.to_owned())
}

fn quoted(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".into())
}

/// A member inside a preview: text quoted, bytes as `0x…`.
fn loose(bytes: &[u8]) -> String {
    match text_of(bytes) {
        Some(text) => quoted(text),
        None => hex(bytes),
    }
}

/// A score as a JSON number where it is one.
fn score_json(score: &str) -> String {
    match score.parse::<f64>() {
        Ok(number) if number.is_finite() => score.to_owned(),
        _ => quoted(score),
    }
}

/// What a collection holds, read from the server.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Collection {
    Hash(Vec<(Vec<u8>, Vec<u8>)>),
    List(Vec<Vec<u8>>),
    Set(Vec<Vec<u8>>),
    /// Member and score, in the order of the scores.
    Zset(Vec<(Vec<u8>, String)>),
}

impl Collection {
    pub(super) fn len(&self) -> usize {
        match self {
            Collection::Hash(pairs) => pairs.len(),
            Collection::List(items) | Collection::Set(items) => items.len(),
            Collection::Zset(pairs) => pairs.len(),
        }
    }

    fn all_text(&self) -> bool {
        let text = |bytes: &Vec<u8>| text_of(bytes).is_some();
        match self {
            Collection::Hash(pairs) => pairs.iter().all(|(name, value)| text(name) && text(value)),
            Collection::List(items) | Collection::Set(items) => items.iter().all(text),
            Collection::Zset(pairs) => pairs.iter().all(|(member, _)| text(member)),
        }
    }

    /// The JSON text of the collection: an object for a hash and a sorted
    /// set (member to score), an array for a list and a set. Members of a
    /// set are sorted, so that the same set always reads the same.
    fn json(&self) -> String {
        let mut out = String::new();
        match self {
            Collection::Hash(pairs) => {
                out.push('{');
                for (index, (name, value)) in pairs.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&loose(name));
                    out.push_str(": ");
                    out.push_str(&loose(value));
                }
                out.push('}');
            }
            Collection::List(items) => {
                out.push('[');
                out.push_str(
                    &items
                        .iter()
                        .map(|item| loose(item))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
                out.push(']');
            }
            Collection::Set(items) => {
                let mut sorted = items.clone();
                sorted.sort();
                out.push('[');
                out.push_str(
                    &sorted
                        .iter()
                        .map(|item| loose(item))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
                out.push(']');
            }
            Collection::Zset(pairs) => {
                out.push('{');
                for (index, (member, score)) in pairs.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&loose(member));
                    out.push_str(": ");
                    out.push_str(&score_json(score));
                }
                out.push('}');
            }
        }
        out
    }

    /// The cell of the `value` column for a collection of `total` members of
    /// which this is the part that was read: complete and editable JSON, or
    /// a read-only preview.
    pub(super) fn cell(&self, total: usize) -> Cell {
        let json = self.json();
        if total == self.len()
            && total <= INLINE_MEMBERS_MAX
            && json.len() <= INLINE_JSON_MAX
            && self.all_text()
        {
            Cell::Json(json)
        } else if total == self.len() && json.chars().count() <= PREVIEW_CHARS {
            // Complete but not text: shown whole, never edited here.
            Cell::Unsupported(json)
        } else {
            Cell::Unsupported(preview_of(&json))
        }
    }

    /// Whether two readings hold the same members, whatever order a hash or
    /// a set returned them in.
    pub(super) fn same(&self, other: &Collection) -> bool {
        match (self, other) {
            (Collection::Hash(a), Collection::Hash(b)) => {
                let map = |pairs: &'_ [(Vec<u8>, Vec<u8>)]| {
                    pairs
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone()))
                        .collect::<BTreeMap<_, _>>()
                };
                a.len() == b.len() && map(a) == map(b)
            }
            (Collection::List(a), Collection::List(b)) => a == b,
            (Collection::Set(a), Collection::Set(b)) => {
                a.iter().collect::<BTreeSet<_>>() == b.iter().collect::<BTreeSet<_>>()
            }
            (Collection::Zset(a), Collection::Zset(b)) => {
                let scores = |pairs: &[(Vec<u8>, String)]| {
                    pairs
                        .iter()
                        .map(|(member, score)| {
                            (
                                member.clone(),
                                score.parse::<f64>().unwrap_or(f64::NAN).to_bits(),
                            )
                        })
                        .collect::<BTreeMap<_, _>>()
                };
                scores(a) == scores(b)
            }
            _ => false,
        }
    }

    /// Read the JSON text of a collection of this kind, as the grid or a
    /// file gives it. Numbers and booleans are taken as their text; a nested
    /// object or array, or `null`, is not a member.
    pub(super) fn parse(kind: &str, json: &str) -> Result<Collection> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|_| anyhow!("redis_json_shape_invalid"))?;
        let scalar = |value: &serde_json::Value| -> Result<Vec<u8>> {
            Ok(match value {
                serde_json::Value::String(text) => text.clone().into_bytes(),
                serde_json::Value::Number(number) => number.to_string().into_bytes(),
                serde_json::Value::Bool(flag) => flag.to_string().into_bytes(),
                _ => bail!("redis_json_shape_invalid"),
            })
        };
        let collection = match (kind, &value) {
            ("hash", serde_json::Value::Object(map)) => Collection::Hash(
                map.iter()
                    .map(|(name, value)| Ok((name.clone().into_bytes(), scalar(value)?)))
                    .collect::<Result<_>>()?,
            ),
            ("list", serde_json::Value::Array(items)) => {
                Collection::List(items.iter().map(scalar).collect::<Result<_>>()?)
            }
            ("set", serde_json::Value::Array(items)) => {
                let members: BTreeSet<Vec<u8>> = items.iter().map(scalar).collect::<Result<_>>()?;
                Collection::Set(members.into_iter().collect())
            }
            ("zset", serde_json::Value::Object(map)) => Collection::Zset(
                map.iter()
                    .map(|(member, score)| {
                        let score = match score {
                            serde_json::Value::Number(number) => number.to_string(),
                            serde_json::Value::String(text) => text.clone(),
                            _ => bail!("redis_json_shape_invalid"),
                        };
                        Ok((member.clone().into_bytes(), parse_score(&score)?))
                    })
                    .collect::<Result<_>>()?,
            ),
            _ => bail!("redis_json_shape_invalid"),
        };
        ensure!(collection.len() > 0, "redis_collection_empty");
        Ok(collection)
    }
}

/// The fields of a stream entry as one JSON object; a field or a value that
/// is not text is written as `0x…`.
pub(super) fn entry_json(fields: &[(Vec<u8>, Vec<u8>)]) -> String {
    Collection::Hash(fields.to_vec()).json()
}

/// The fields of a new stream entry from a JSON object.
pub(super) fn parse_entry(json: &str) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    match Collection::parse("hash", json) {
        Ok(Collection::Hash(fields)) => Ok(fields),
        Err(error) if error.to_string() == "redis_collection_empty" => {
            bail!("redis_entry_invalid")
        }
        _ => bail!("redis_entry_invalid"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_utf8_without_control_characters() {
        assert_eq!(
            text_of("Zażółć gęślą 🦀".as_bytes()),
            Some("Zażółć gęślą 🦀")
        );
        assert_eq!(text_of(b"line\none\ttab\r"), Some("line\none\ttab\r"));
        assert_eq!(text_of(b""), Some(""));
        // A NUL, an escape character and a broken UTF-8 sequence are bytes.
        assert_eq!(text_of(b"a\0b"), None);
        assert_eq!(text_of(b"\x1b[0m"), None);
        assert_eq!(text_of(&[0xff, 0xfe]), None);
        assert_eq!(text_of(&[0xc3]), None);
    }

    #[test]
    fn bytes_that_are_not_text_keep_every_byte_behind_a_marker() {
        let raw: Vec<u8> = (0..=255).collect();
        let cell = name_cell(&raw);
        let Cell::Unsupported(shown) = &cell else {
            panic!("not read only: {cell:?}");
        };
        assert!(shown.starts_with("0x000102") && shown.ends_with("fdfeff"));
        assert!(!cell.editable());
        // What the grid hands back is the same bytes again.
        assert_eq!(bytes_of(&cell), Some(raw.clone()));
        // A name is never cut, a value is: its preview is not the value.
        let long = vec![0xffu8; BINARY_CELL_MAX + 1];
        assert_eq!(bytes_of(&name_cell(&long)), Some(long.clone()));
        let cut = value_cell(&long);
        assert!(cut.display().ends_with("(4.0 KiB)"), "{}", cut.display());
        assert_eq!(bytes_of(&cut), None);
        // Text that only looks like the marker stays text.
        assert_eq!(name_cell(b"0xff"), Cell::Text("0xff".into()));
        assert_eq!(bytes_of(&Cell::Text("0xff".into())), Some(b"0xff".to_vec()));
        assert_eq!(unhex("0xff"), Some(vec![0xff]));
        assert_eq!(unhex("0xf"), None);
        assert_eq!(unhex("0xzz"), None);
        assert_eq!(unhex("ff"), None);
        assert_eq!(unhex("0x"), Some(vec![]));
    }

    #[test]
    fn a_long_text_value_is_a_preview_that_cannot_be_saved_back() {
        let long = "ż".repeat(TEXT_CELL_MAX);
        let cell = value_cell(long.as_bytes());
        assert!(!cell.editable());
        assert!(
            cell.display().ends_with("… (128.0 KiB)"),
            "{}",
            cell.display()
        );
        assert_eq!(bytes_of(&cell), None);
        let fits = "a".repeat(TEXT_CELL_MAX);
        assert_eq!(value_cell(fits.as_bytes()), Cell::Text(fits));
        // A file made from a page holds the preview: it is known again.
        assert!(looks_cut(&cell.display()));
        assert!(looks_cut(&preview_of("[\"a\"")) && looks_cut("0xff00… (5.0 MiB)"));
        for whole in ["hello", "50% (5 KiB)", "a … b", "… (many)", ""] {
            assert!(!looks_cut(whole), "{whole}");
        }
        assert_eq!(human_size(12), "12 B");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn a_small_collection_is_complete_json_and_reads_back() {
        let hash = Collection::Hash(vec![
            (b"name".to_vec(), "Jan \"Kot\"".as_bytes().to_vec()),
            (b"visits".to_vec(), b"12".to_vec()),
        ]);
        let cell = hash.cell(2);
        assert_eq!(
            cell,
            Cell::Json(r#"{"name": "Jan \"Kot\"", "visits": "12"}"#.into())
        );
        let back = Collection::parse("hash", &cell.display()).unwrap();
        assert!(back.same(&hash));
        // Numbers and booleans typed by hand are taken as text.
        let typed = Collection::parse("hash", r#"{"visits": 13, "ok": true}"#).unwrap();
        assert_eq!(
            typed,
            Collection::Hash(vec![
                (b"ok".to_vec(), b"true".to_vec()),
                (b"visits".to_vec(), b"13".to_vec())
            ])
        );
        let list = Collection::List(vec![b"b".to_vec(), b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(list.cell(3), Cell::Json(r#"["b", "a", "b"]"#.into()));
        assert!(
            Collection::parse("list", r#"["b","a","b"]"#)
                .unwrap()
                .same(&list)
        );
        // A set reads the same whatever order the server returned.
        let set = Collection::Set(vec![b"zebra".to_vec(), b"ant".to_vec()]);
        assert_eq!(set.cell(2), Cell::Json(r#"["ant", "zebra"]"#.into()));
        assert!(set.same(&Collection::Set(vec![b"ant".to_vec(), b"zebra".to_vec()])));
        let zset = Collection::Zset(vec![
            (b"low".to_vec(), "1".into()),
            (b"high".to_vec(), "2.5".into()),
            (b"top".to_vec(), "inf".into()),
        ]);
        assert_eq!(
            zset.cell(3),
            Cell::Json(r#"{"low": 1, "high": 2.5, "top": "inf"}"#.into())
        );
        let back = Collection::parse("zset", &zset.cell(3).display()).unwrap();
        assert!(back.same(&zset));
        assert!(!back.same(&Collection::Zset(vec![(b"low".to_vec(), "2".into())])));
    }

    #[test]
    fn a_collection_that_is_cut_or_not_text_is_read_only() {
        let many = Collection::List(
            (0..INLINE_MEMBERS_MAX + 1)
                .map(|n| n.to_string().into_bytes())
                .collect(),
        );
        assert!(!many.cell(many.len()).editable());
        // The first members of a large one.
        let part = Collection::List(vec![b"a".to_vec()]);
        let cell = part.cell(1_000_000);
        assert_eq!(cell, Cell::Unsupported("[\"a\"]…".into()));
        // A member that is not text: shown, with the marker, never edited.
        let binary = Collection::Set(vec![vec![0xff, 0x00]]);
        assert_eq!(binary.cell(1), Cell::Unsupported("[0xff00]".into()));
    }

    #[test]
    fn json_of_another_shape_is_refused() {
        for (kind, json) in [
            ("hash", "[1,2]"),
            ("hash", r#"{"a": {"nested": 1}}"#),
            ("hash", r#"{"a": null}"#),
            ("list", r#"{"a": 1}"#),
            ("list", "[[1]]"),
            ("zset", r#"["a"]"#),
            ("set", "5"),
            ("hash", "not json"),
        ] {
            assert_eq!(
                Collection::parse(kind, json).unwrap_err().to_string(),
                "redis_json_shape_invalid",
                "{kind}: {json}"
            );
        }
        assert_eq!(
            Collection::parse("zset", r#"{"a": "high"}"#)
                .unwrap_err()
                .to_string(),
            "redis_score_invalid"
        );
        assert_eq!(
            Collection::parse("list", "[]").unwrap_err().to_string(),
            "redis_collection_empty"
        );
        assert_eq!(
            parse_entry("{}").unwrap_err().to_string(),
            "redis_entry_invalid"
        );
        assert_eq!(
            parse_entry(r#"{"sensor": "a", "value": 21.5}"#).unwrap(),
            vec![
                (b"sensor".to_vec(), b"a".to_vec()),
                (b"value".to_vec(), b"21.5".to_vec())
            ]
        );
    }

    #[test]
    fn scores_are_numbers_or_infinities() {
        assert_eq!(parse_score(" 1.50 ").unwrap(), "1.50");
        assert_eq!(parse_score("-INF").unwrap(), "-inf");
        assert_eq!(parse_score("1e3").unwrap(), "1e3");
        for bad in ["", "abc", "nan", "1,5"] {
            assert_eq!(
                parse_score(bad).unwrap_err().to_string(),
                "redis_score_invalid",
                "{bad}"
            );
        }
        assert_eq!(float_text(1.0), "1");
        assert_eq!(float_text(2.5), "2.5");
        assert_eq!(float_text(f64::NEG_INFINITY), "-inf");
    }

    #[test]
    fn replies_are_read_in_both_protocol_versions() {
        let flat = Value::Array(vec![
            Value::BulkString(b"a".to_vec()),
            Value::BulkString(b"1".to_vec()),
            Value::BulkString(b"b".to_vec()),
            Value::BulkString(b"2".to_vec()),
        ]);
        let map = Value::Map(vec![
            (
                Value::BulkString(b"a".to_vec()),
                Value::BulkString(b"1".to_vec()),
            ),
            (
                Value::BulkString(b"b".to_vec()),
                Value::BulkString(b"2".to_vec()),
            ),
        ]);
        let nested = Value::Array(vec![
            Value::Array(vec![Value::BulkString(b"a".to_vec()), Value::Double(1.0)]),
            Value::Array(vec![Value::BulkString(b"b".to_vec()), Value::Double(2.0)]),
        ]);
        assert_eq!(reply_pairs(flat).unwrap().len(), 2);
        assert_eq!(reply_pairs(map).unwrap().len(), 2);
        let pairs = reply_pairs(nested).unwrap();
        assert_eq!(reply_text(&pairs[1].1).as_deref(), Some("2"));
        assert_eq!(reply_int(&Value::BulkString(b"42".to_vec())), Some(42));
        assert_eq!(reply_int(&Value::Nil), None);
        assert!(reply_pairs(Value::Array(vec![Value::Int(1)])).is_none());
    }
}
