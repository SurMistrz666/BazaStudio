//! DynamoDB values in both directions, with their types kept exactly.
//!
//! The service speaks "DynamoDB JSON": every value is an object with one
//! key that names its type (`{"S": "text"}`, `{"N": "12.50"}`, `{"M": {…}}`).
//! A cell of the grid shows a text as text, a number with every digit, a
//! boolean, bytes, and everything with parts as compact JSON:
//!
//! * a list is an array and a map an object, with texts, numbers, booleans
//!   and `null` inside as JSON has them;
//! * what JSON has no notation for keeps its type name: `{"SS": ["a", "b"]}`,
//!   `{"NS": [1, 2.5]}`, `{"BS": ["…"]}` and `{"B": "…"}` (base64);
//! * a map that would read as one of those (a single key that is a type
//!   name) is written `{"M": {…}}`, so nothing is ever ambiguous.
//!
//! The same text is read back into the same typed value, and full DynamoDB
//! JSON is accepted wherever a value is typed.
use crate::model::Cell;
use anyhow::{Result, anyhow, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

/// The type names of DynamoDB, in the order they are listed for a column.
pub(super) const TYPES: [&str; 10] = ["S", "N", "B", "BOOL", "NULL", "L", "M", "SS", "NS", "BS"];

/// The type of a typed value, or nothing for what is not one.
pub(super) fn type_of(value: &Value) -> Option<&'static str> {
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }
    let (name, inner) = object.iter().next()?;
    let known = TYPES.iter().find(|known| *known == name)?;
    let strings = |value: &Value| {
        value
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string))
    };
    let valid = match *known {
        "S" | "N" | "B" => inner.is_string(),
        "BOOL" | "NULL" => inner.is_boolean(),
        "L" => inner.is_array(),
        "M" => inner.is_object(),
        _ => strings(inner),
    };
    valid.then_some(*known)
}

/// A number of DynamoDB: digits with an optional sign, fraction and
/// exponent. The service checks the range (38 digits).
pub(super) fn number(text: &str) -> Result<String> {
    let text = text.trim();
    ensure!(
        !text.is_empty()
            && text.len() <= 128
            && text.parse::<bigdecimal::BigDecimal>().is_ok()
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E')),
        "dynamodb_number_invalid"
    );
    Ok(text.to_owned())
}

/// A number as a JSON number with every digit. One with an exponent stays
/// text with its type: a JSON reader may rewrite `1E+2`, and nothing here
/// changes a digit of what the service stored.
fn json_number(text: &str) -> Value {
    let plain = text
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'.'));
    serde_json::from_str::<Value>(text)
        .ok()
        .filter(|value| {
            plain
                && value
                    .as_number()
                    .is_some_and(|number| number.as_str() == text)
        })
        .unwrap_or_else(|| json!({ "N": text }))
}

/// A typed value as the compact JSON of the grid.
pub(super) fn plain(value: &Value) -> Value {
    let Some(kind) = type_of(value) else {
        return value.clone();
    };
    let inner = &value[kind];
    match kind {
        "S" => inner.clone(),
        "N" => json_number(inner.as_str().unwrap_or_default()),
        "BOOL" => inner.clone(),
        "NULL" => Value::Null,
        "L" => Value::Array(
            inner
                .as_array()
                .map(|items| items.iter().map(plain).collect())
                .unwrap_or_default(),
        ),
        "M" => {
            let entries: Map<String, Value> = inner
                .as_object()
                .map(|entries| {
                    entries
                        .iter()
                        .map(|(key, value)| (key.clone(), plain(value)))
                        .collect()
                })
                .unwrap_or_default();
            // A map with one key that is a type name would be read back as
            // a value of that type.
            if entries.len() == 1 && entries.keys().all(|key| TYPES.contains(&key.as_str())) {
                json!({ "M": entries })
            } else {
                Value::Object(entries)
            }
        }
        "NS" => json!({
            "NS": inner
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .map(|item| json_number(item.as_str().unwrap_or_default()))
                        .map(|item| if item.is_number() { item } else { item["N"].clone() })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        }),
        // Sets of texts and bytes, and bytes: as the service writes them.
        _ => value.clone(),
    }
}

/// Compact JSON, or full DynamoDB JSON, as a typed value.
pub(super) fn typed(value: &Value) -> Result<Value> {
    let invalid = || anyhow!("dynamodb_value_invalid");
    Ok(match value {
        Value::Null => json!({ "NULL": true }),
        Value::Bool(flag) => json!({ "BOOL": flag }),
        Value::Number(digits) => json!({ "N": number(&digits.to_string())? }),
        Value::String(text) => json!({ "S": text }),
        Value::Array(items) => {
            json!({ "L": items.iter().map(typed).collect::<Result<Vec<_>>>()? })
        }
        Value::Object(entries) => {
            let single = entries
                .iter()
                .next()
                .filter(|_| entries.len() == 1)
                .filter(|(key, _)| TYPES.contains(&key.as_str()));
            let Some((kind, inner)) = single else {
                return Ok(json!({ "M": map(entries)? }));
            };
            match kind.as_str() {
                "S" => json!({ "S": inner.as_str().ok_or_else(invalid)? }),
                "N" => json!({ "N": scalar_number(inner)? }),
                "B" => json!({ "B": base64(inner.as_str().ok_or_else(invalid)?)? }),
                "BOOL" => json!({ "BOOL": inner.as_bool().ok_or_else(invalid)? }),
                "NULL" => {
                    ensure!(inner.as_bool() == Some(true), "dynamodb_value_invalid");
                    json!({ "NULL": true })
                }
                "L" => json!({
                    "L": inner
                        .as_array()
                        .ok_or_else(invalid)?
                        .iter()
                        .map(typed)
                        .collect::<Result<Vec<_>>>()?
                }),
                "M" => json!({ "M": map(inner.as_object().ok_or_else(invalid)?)? }),
                set => {
                    let items = inner.as_array().ok_or_else(invalid)?;
                    // The service has no empty set.
                    ensure!(!items.is_empty(), "dynamodb_set_empty");
                    let members = items
                        .iter()
                        .map(|item| match set {
                            "SS" => Ok(item.as_str().ok_or_else(invalid)?.to_owned()),
                            "NS" => scalar_number(item),
                            _ => base64(item.as_str().ok_or_else(invalid)?),
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let distinct: BTreeSet<&String> = members.iter().collect();
                    ensure!(distinct.len() == members.len(), "dynamodb_set_duplicate");
                    json!({ set: members })
                }
            }
        }
    })
}

/// The entries of a map, each typed; inside `{"M": …}` and for an object
/// that is not a typed value itself.
fn map(entries: &Map<String, Value>) -> Result<Map<String, Value>> {
    entries
        .iter()
        .map(|(key, value)| Ok((key.clone(), typed(value)?)))
        .collect()
}

/// A number written as a JSON number or as text.
fn scalar_number(value: &Value) -> Result<String> {
    match value {
        Value::Number(digits) => number(&digits.to_string()),
        Value::String(text) => number(text),
        _ => bail!("dynamodb_value_invalid"),
    }
}

/// Check that a text is base64 and return it as the service wants it.
fn base64(text: &str) -> Result<String> {
    let bytes = BASE64
        .decode(text.trim())
        .map_err(|_| anyhow!("dynamodb_value_invalid"))?;
    Ok(BASE64.encode(bytes))
}

/// A typed value as a cell of the grid.
pub(super) fn cell(value: &Value) -> Cell {
    match type_of(value) {
        Some("S") => Cell::Text(value["S"].as_str().unwrap_or_default().to_owned()),
        Some("N") => Cell::Decimal(value["N"].as_str().unwrap_or_default().to_owned()),
        Some("BOOL") => Cell::Bool(value["BOOL"].as_bool().unwrap_or_default()),
        Some("B") => match BASE64.decode(value["B"].as_str().unwrap_or_default()) {
            Ok(bytes) => Cell::Bytes(bytes),
            Err(_) => Cell::Unsupported("B".into()),
        },
        Some(_) => Cell::Json(plain(value).to_string()),
        None => Cell::Unsupported(value.to_string()),
    }
}

/// What is known about the type of an attribute when a cell is written.
#[derive(Clone, Copy, Debug)]
pub(super) enum Hint<'a> {
    /// A key attribute: its type is declared (`S`, `N` or `B`).
    Key(&'a str),
    /// The types items were seen to hold in this attribute.
    Seen(&'a BTreeSet<&'static str>),
}

/// The typed value of a cell for an attribute; `None` when the item is to
/// have no such attribute. The grid and a CSV file hand over text for
/// everything, so what is known about the attribute decides how it is read.
pub(super) fn attribute(cell: &Cell, hint: Hint<'_>) -> Result<Option<Value>> {
    let text = match cell {
        Cell::Null => return Ok(None),
        Cell::Unsupported(_) => bail!("unsupported_cell_type"),
        Cell::Bool(flag) => return scalar(json!({ "BOOL": flag }), hint).map(Some),
        Cell::Bytes(bytes) => return scalar(json!({ "B": BASE64.encode(bytes) }), hint).map(Some),
        Cell::Integer(text) | Cell::Decimal(text) | Cell::Float(text) => {
            return match hint {
                // A key of texts whose value happens to be digits.
                Hint::Key("S") => Ok(Some(json!({ "S": text }))),
                _ => scalar(json!({ "N": number(text)? }), hint).map(Some),
            };
        }
        Cell::Json(text) => {
            let value: Value = serde_json::from_str(text).map_err(|_| anyhow!("json_invalid"))?;
            return scalar(typed(&value)?, hint).map(Some);
        }
        Cell::Text(text) => text,
    };
    let kind = match hint {
        Hint::Key(kind) => kind,
        Hint::Seen(seen) => {
            let has = |kind: &str| seen.contains(kind);
            let looks_json = text.trim_start().starts_with(['{', '[']);
            if seen.len() == 1 {
                seen.iter().next().copied().unwrap_or("S")
            } else if has("N") && number(text).is_ok() {
                "N"
            } else if has("BOOL") && matches!(text.trim(), "true" | "false") {
                "BOOL"
            } else if looks_json && seen.iter().any(|kind| !matches!(*kind, "S" | "N" | "B")) {
                "M"
            } else {
                "S"
            }
        }
    };
    // An empty field of anything but a text is no value at all.
    if text.trim().is_empty() && kind != "S" {
        ensure!(!matches!(hint, Hint::Key(_)), "dynamodb_key_required");
        return Ok(None);
    }
    Ok(Some(match kind {
        "S" => json!({ "S": text }),
        "N" => json!({ "N": number(text)? }),
        "B" => json!({ "B": base64(text)? }),
        "BOOL" => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "1" => json!({ "BOOL": true }),
            "false" | "0" => json!({ "BOOL": false }),
            _ => bail!("boolean_invalid"),
        },
        // Everything with parts, and NULL, is typed as JSON.
        _ => typed(&serde_json::from_str(text).map_err(|_| anyhow!("json_invalid"))?)?,
    }))
}

/// A key attribute takes only a value of its declared type.
fn scalar(value: Value, hint: Hint<'_>) -> Result<Value> {
    if let Hint::Key(kind) = hint {
        ensure!(type_of(&value) == Some(kind), "dynamodb_key_type_mismatch");
    }
    Ok(value)
}

/// A typed value as a PartiQL literal, for the text of a change in the
/// history. Bytes have no literal in PartiQL: they are named, not written.
pub(super) fn partiql(value: &Value) -> String {
    let quoted = |text: &str| format!("'{}'", text.replace('\'', "''"));
    let Some(kind) = type_of(value) else {
        return "NULL".into();
    };
    let inner = &value[kind];
    let list =
        |open: &str, close: &str, items: Vec<String>| format!("{open}{}{close}", items.join(", "));
    let strings = || -> Vec<&str> {
        inner
            .as_array()
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    };
    match kind {
        "S" => quoted(inner.as_str().unwrap_or_default()),
        "N" => inner.as_str().unwrap_or_default().to_owned(),
        "BOOL" => inner.as_bool().unwrap_or_default().to_string(),
        "NULL" => "NULL".into(),
        "B" => "<binary>".into(),
        "L" => list(
            "[",
            "]",
            inner
                .as_array()
                .map(|items| items.iter().map(partiql).collect())
                .unwrap_or_default(),
        ),
        "M" => list(
            "{",
            "}",
            inner
                .as_object()
                .map(|entries| {
                    entries
                        .iter()
                        .map(|(key, value)| format!("{}: {}", quoted(key), partiql(value)))
                        .collect()
                })
                .unwrap_or_default(),
        ),
        "SS" => list("<<", ">>", strings().into_iter().map(quoted).collect()),
        "NS" => list(
            "<<",
            ">>",
            strings().into_iter().map(str::to_owned).collect(),
        ),
        _ => "<<binary>>".into(),
    }
}

/// A name in double quotes, as PartiQL quotes one.
pub(super) fn name(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(kinds: &[&'static str]) -> BTreeSet<&'static str> {
        kinds.iter().copied().collect()
    }

    /// Every type of DynamoDB JSON, as the service returns an item.
    fn item() -> Value {
        json!({
            "text": {"S": "Zażółć 'x' \"y\""},
            "number": {"N": "12345678901234567890.123456789012345678"},
            "negative": {"N": "-0.50"},
            "bytes": {"B": "AP8="},
            "flag": {"BOOL": true},
            "nothing": {"NULL": true},
            "list": {"L": [{"S": "a"}, {"N": "1"}, {"BOOL": false}, {"NULL": true}, {"L": []}]},
            "map": {"M": {"street": {"S": "Hoża 1"}, "zip": {"N": "681"}, "tags": {"SS": ["a", "b"]}}},
            "strings": {"SS": ["b", "a"]},
            "numbers": {"NS": ["1", "2.50", "-3E+2"]},
            "blobs": {"BS": ["AP8=", "AQI="]},
        })
    }

    #[test]
    fn every_type_has_a_cell() {
        let item = item();
        assert_eq!(cell(&item["text"]), Cell::Text("Zażółć 'x' \"y\"".into()));
        // A number keeps every digit and its trailing zeros.
        assert_eq!(
            cell(&item["number"]),
            Cell::Decimal("12345678901234567890.123456789012345678".into())
        );
        assert_eq!(cell(&item["negative"]), Cell::Decimal("-0.50".into()));
        assert_eq!(cell(&item["bytes"]), Cell::Bytes(vec![0, 255]));
        assert_eq!(cell(&item["flag"]), Cell::Bool(true));
        // An attribute that holds NULL is not an attribute that is missing.
        assert_eq!(cell(&item["nothing"]), Cell::Json("null".into()));
        assert_eq!(
            cell(&item["list"]),
            Cell::Json(r#"["a",1,false,null,[]]"#.into())
        );
        assert_eq!(
            cell(&item["map"]),
            Cell::Json(r#"{"street":"Hoża 1","tags":{"SS":["a","b"]},"zip":681}"#.into())
        );
        assert_eq!(
            cell(&item["strings"]),
            Cell::Json(r#"{"SS":["b","a"]}"#.into())
        );
        assert_eq!(
            cell(&item["numbers"]),
            Cell::Json(r#"{"NS":[1,2.50,"-3E+2"]}"#.into())
        );
        // An exponent is kept as the service wrote it.
        assert_eq!(
            cell(&json!({"N": "1E+100"})),
            Cell::Decimal("1E+100".into())
        );
        assert_eq!(
            cell(&json!({"L": [{"N": "1E+100"}]})),
            Cell::Json(r#"[{"N":"1E+100"}]"#.into())
        );
        assert_eq!(
            cell(&item["blobs"]),
            Cell::Json(r#"{"BS":["AP8=","AQI="]}"#.into())
        );
        assert!(matches!(cell(&json!({"X": 1})), Cell::Unsupported(_)));
        assert!(matches!(cell(&json!({"S": 1})), Cell::Unsupported(_)));
    }

    #[test]
    fn what_a_cell_shows_is_read_back_as_the_same_typed_value() {
        let item = item();
        for (name, value) in item.as_object().unwrap() {
            // Through the compact JSON of the grid…
            assert_eq!(&typed(&plain(value)).unwrap(), value, "{name}");
            // …and as full DynamoDB JSON pasted into a cell.
            assert_eq!(&typed(value).unwrap(), value, "{name} as DynamoDB JSON");
            // A cell with parts goes back through its text.
            if let Cell::Json(text) = cell(value) {
                let written = attribute(&Cell::Json(text), Hint::Seen(&seen(&[]))).unwrap();
                assert_eq!(written.as_ref(), Some(value), "{name}");
            }
        }
        // A map with a single key that is a type name stays a map.
        for ambiguous in [
            json!({"M": {"S": {"S": "x"}}}),
            json!({"M": {"SS": {"L": [{"S": "a"}]}}}),
            json!({"M": {"N": {"M": {"M": {"S": "deep"}}}}}),
            json!({"M": {"NULL": {"BOOL": true}}}),
        ] {
            let shown = plain(&ambiguous);
            assert!(shown.get("M").is_some(), "{shown}");
            assert_eq!(typed(&shown).unwrap(), ambiguous, "{shown}");
        }
        assert_eq!(
            plain(&json!({"M": {"S": {"S": "x"}}})).to_string(),
            r#"{"M":{"S":"x"}}"#
        );
        // Two keys, or a name that is not a type, need no wrapper.
        assert_eq!(
            plain(&json!({"M": {"S": {"S": "x"}, "t": {"N": "1"}}})).to_string(),
            r#"{"S":"x","t":1}"#
        );
        assert_eq!(
            typed(&json!({"S": "x", "t": 1})).unwrap(),
            json!({"M": {"S": {"S": "x"}, "t": {"N": "1"}}})
        );
    }

    #[test]
    fn typed_json_is_checked() {
        let failed = |value: Value| typed(&value).unwrap_err().to_string();
        assert_eq!(failed(json!({"SS": []})), "dynamodb_set_empty");
        assert_eq!(failed(json!({"SS": ["a", "a"]})), "dynamodb_set_duplicate");
        assert_eq!(failed(json!({"SS": [1]})), "dynamodb_value_invalid");
        assert_eq!(failed(json!({"N": "abc"})), "dynamodb_number_invalid");
        assert_eq!(
            failed(json!({"B": "not base64!"})),
            "dynamodb_value_invalid"
        );
        assert_eq!(failed(json!({"NULL": false})), "dynamodb_value_invalid");
        assert_eq!(failed(json!({"L": {"a": 1}})), "dynamodb_value_invalid");
        assert_eq!(
            typed(&json!({"NS": [1, "2.5"]})).unwrap(),
            json!({"NS": ["1", "2.5"]})
        );
        assert_eq!(typed(&json!({"N": 5})).unwrap(), json!({"N": "5"}));
        assert_eq!(number(" 1e3 ").unwrap(), "1e3");
        assert!(number("").is_err() && number("1 OR 1").is_err() && number("NaN").is_err());
        assert_eq!(type_of(&json!({"S": "x"})), Some("S"));
        assert_eq!(type_of(&json!({"S": "x", "N": "1"})), None);
        assert_eq!(type_of(&json!("x")), None);
    }

    #[test]
    fn a_cell_becomes_a_value_of_the_type_of_its_attribute() {
        let text = |value: &str| Cell::Text(value.into());
        let write = |cell: Cell, hint: Hint<'_>| attribute(&cell, hint).map_err(|e| e.to_string());
        // A key attribute has a declared type.
        assert_eq!(
            write(text("42"), Hint::Key("N")).unwrap(),
            Some(json!({"N": "42"}))
        );
        assert_eq!(
            write(text("42"), Hint::Key("S")).unwrap(),
            Some(json!({"S": "42"}))
        );
        assert_eq!(
            write(Cell::Decimal("42".into()), Hint::Key("S")).unwrap(),
            Some(json!({"S": "42"}))
        );
        assert_eq!(
            write(text("AP8="), Hint::Key("B")).unwrap(),
            Some(json!({"B": "AP8="}))
        );
        assert_eq!(
            write(text("abc"), Hint::Key("N")).unwrap_err(),
            "dynamodb_number_invalid"
        );
        assert_eq!(
            write(text(""), Hint::Key("N")).unwrap_err(),
            "dynamodb_key_required"
        );
        assert_eq!(
            write(Cell::Bool(true), Hint::Key("S")).unwrap_err(),
            "dynamodb_key_type_mismatch"
        );
        // Elsewhere what items were seen to hold decides how text is read.
        let numbers = seen(&["N"]);
        assert_eq!(
            write(text("7.5"), Hint::Seen(&numbers)).unwrap(),
            Some(json!({"N": "7.5"}))
        );
        assert_eq!(
            write(text("seven"), Hint::Seen(&numbers)).unwrap_err(),
            "dynamodb_number_invalid"
        );
        // An empty field of a number is no value; an empty text is a text.
        assert_eq!(write(text(" "), Hint::Seen(&numbers)).unwrap(), None);
        let texts = seen(&["S"]);
        assert_eq!(
            write(text(""), Hint::Seen(&texts)).unwrap(),
            Some(json!({"S": ""}))
        );
        assert_eq!(
            write(text("42"), Hint::Seen(&texts)).unwrap(),
            Some(json!({"S": "42"}))
        );
        assert_eq!(
            write(text("TRUE"), Hint::Seen(&seen(&["BOOL"]))).unwrap(),
            Some(json!({"BOOL": true}))
        );
        assert_eq!(
            write(text(r#"{"a":[1,"x"]}"#), Hint::Seen(&seen(&["M"]))).unwrap(),
            Some(json!({"M": {"a": {"L": [{"N": "1"}, {"S": "x"}]}}}))
        );
        assert_eq!(
            write(text(r#"{"SS":["a"]}"#), Hint::Seen(&seen(&["SS"]))).unwrap(),
            Some(json!({"SS": ["a"]}))
        );
        // An attribute nobody has seen yet, or one of several types.
        let open = seen(&[]);
        assert_eq!(
            write(text("42"), Hint::Seen(&open)).unwrap(),
            Some(json!({"S": "42"}))
        );
        let mixed = seen(&["S", "N", "M"]);
        assert_eq!(
            write(text("42"), Hint::Seen(&mixed)).unwrap(),
            Some(json!({"N": "42"}))
        );
        assert_eq!(
            write(text("x42"), Hint::Seen(&mixed)).unwrap(),
            Some(json!({"S": "x42"}))
        );
        assert_eq!(
            write(text(r#"{"a":1}"#), Hint::Seen(&mixed)).unwrap(),
            Some(json!({"M": {"a": {"N": "1"}}}))
        );
        // Cells that carry their kind.
        assert_eq!(write(Cell::Null, Hint::Seen(&texts)).unwrap(), None);
        assert_eq!(
            write(Cell::Bool(false), Hint::Seen(&open)).unwrap(),
            Some(json!({"BOOL": false}))
        );
        assert_eq!(
            write(Cell::Bytes(vec![0, 255]), Hint::Seen(&open)).unwrap(),
            Some(json!({"B": "AP8="}))
        );
        assert_eq!(
            write(Cell::Json("null".into()), Hint::Seen(&open)).unwrap(),
            Some(json!({"NULL": true}))
        );
        assert_eq!(
            write(Cell::Json("{".into()), Hint::Seen(&open)).unwrap_err(),
            "json_invalid"
        );
        assert_eq!(
            write(Cell::Unsupported("x".into()), Hint::Seen(&open)).unwrap_err(),
            "unsupported_cell_type"
        );
    }

    #[test]
    fn a_value_is_written_as_a_partiql_literal_for_the_history() {
        let item = item();
        assert_eq!(partiql(&item["text"]), "'Zażółć ''x'' \"y\"'");
        assert_eq!(partiql(&item["negative"]), "-0.50");
        assert_eq!(partiql(&item["flag"]), "true");
        assert_eq!(partiql(&item["nothing"]), "NULL");
        assert_eq!(partiql(&item["list"]), "['a', 1, false, NULL, []]");
        assert_eq!(
            partiql(&item["map"]),
            "{'street': 'Hoża 1', 'tags': <<'a', 'b'>>, 'zip': 681}"
        );
        assert_eq!(partiql(&item["numbers"]), "<<1, 2.50, -3E+2>>");
        assert_eq!(partiql(&item["bytes"]), "<binary>");
        assert_eq!(name("Order \"x\""), "\"Order \"\"x\"\"\"");
    }
}
