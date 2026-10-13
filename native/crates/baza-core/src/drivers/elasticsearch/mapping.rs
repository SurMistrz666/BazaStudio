//! An index as a grid: the fields of its mapping as columns, the `_source`
//! of a document as a row, and a cell back into JSON.
//!
//! A column is the dotted path of a leaf field (`address.city`). Objects
//! are walked into; a `nested` field, a disabled object and everything
//! that is not a plain value (a geo point, a range, a vector) is one column
//! of compact JSON. The cells show what `_source` holds, which is what was
//! sent to the index: a number field may hold `"42"` there, and then the
//! grid shows a text.
use crate::model::{Cell, ColumnInfo, ResultColumn};
use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

/// The column of the document id: the key of a record.
pub(super) const ID: &str = "_id";
/// Most fields of a mapping that become columns. An index may have more
/// (the server allows 1000 by default); the rest is reached in the console.
pub(super) const MOST_COLUMNS: usize = 1000;
/// Most fields that are not in the mapping and are still shown, because
/// documents turned out to hold them.
pub(super) const MOST_EXTRA: usize = 64;
/// The type the grid names a field without a mapping with.
pub(super) const UNMAPPED: &str = "unmapped";

/// One field of an index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Field {
    /// Dotted path from the root of the document.
    pub path: String,
    /// The type of the mapping (`keyword`, `long`, `nested`, …); `object`
    /// for an object that is not walked into.
    pub kind: String,
    /// A sub-field of type `keyword` (`name.keyword`): the exact form of an
    /// analysed text, which is what the server orders and compares.
    pub keyword: Option<String>,
}

/// What a type means for a filter, an order and a cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Class {
    /// The `_id` of the document.
    Id,
    /// Analysed text: searched by its words.
    Text,
    /// Exact text.
    Keyword,
    Integer,
    Float,
    Date,
    Boolean,
    Ip,
    Binary,
    /// An object, a nested list, a geo shape, a range, a vector: JSON.
    Json,
    /// Not in the mapping: the server does not search it.
    Unmapped,
}

impl Field {
    pub(super) fn class(&self) -> Class {
        class(&self.kind)
    }
}

pub(super) fn class(kind: &str) -> Class {
    match kind {
        ID => Class::Id,
        "text" | "match_only_text" | "search_as_you_type" | "annotated_text" | "semantic_text" => {
            Class::Text
        }
        "keyword" | "constant_keyword" | "wildcard" | "version" => Class::Keyword,
        "long" | "integer" | "short" | "byte" | "unsigned_long" | "token_count" => Class::Integer,
        "double" | "float" | "half_float" | "scaled_float" | "rank_feature" => Class::Float,
        "date" | "date_nanos" => Class::Date,
        "boolean" => Class::Boolean,
        "ip" => Class::Ip,
        "binary" => Class::Binary,
        UNMAPPED => Class::Unmapped,
        _ => Class::Json,
    }
}

/// The leaf fields under `properties`, with their paths.
fn walk(properties: &Map<String, Value>, prefix: &str, out: &mut Vec<Field>) {
    for (name, definition) in properties {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        let kind = definition["type"].as_str().unwrap_or("object");
        // An alias points at another field and holds nothing itself.
        if kind == "alias" {
            continue;
        }
        let inner = definition["properties"].as_object();
        let enabled = definition["enabled"].as_bool().unwrap_or(true);
        match inner {
            Some(inner) if kind == "object" && enabled && !inner.is_empty() => {
                walk(inner, &path, out);
            }
            _ => {
                let keyword = definition["fields"].as_object().and_then(|fields| {
                    fields
                        .iter()
                        .filter(|(_, field)| field["type"].as_str() == Some("keyword"))
                        .map(|(sub, _)| sub.as_str())
                        // The usual name first, otherwise the first by name.
                        .min_by_key(|sub| (*sub != "keyword", *sub))
                        .map(|sub| format!("{path}.{sub}"))
                });
                out.push(Field {
                    path,
                    kind: kind.to_owned(),
                    keyword,
                });
            }
        }
    }
}

/// The fields of the answer to `GET <target>/_mapping`: the mappings of
/// every index behind the name, as one list. A field that two indices
/// define differently keeps the type of the first.
pub(super) fn fields(answer: &Value) -> Vec<Field> {
    let mut out: Vec<Field> = Vec::new();
    let mut seen = BTreeSet::new();
    for index in answer.as_object().into_iter().flat_map(Map::values) {
        let mappings = &index["mappings"];
        // Servers before 7.0 put the name of a type between the two.
        let properties = match mappings["properties"].as_object() {
            Some(properties) => Some(properties),
            None => mappings
                .as_object()
                .into_iter()
                .flat_map(Map::values)
                .find_map(|typed| typed["properties"].as_object()),
        };
        let mut found = Vec::new();
        if let Some(properties) = properties {
            walk(properties, "", &mut found);
        }
        for field in found {
            if seen.insert(field.path.clone()) {
                out.push(field);
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.truncate(MOST_COLUMNS);
    out
}

/// The columns of a container: `_id`, the fields of the mapping, and the
/// fields documents hold without a mapping.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Layout {
    pub fields: Vec<Field>,
}

impl Layout {
    pub(super) fn new(mut fields: Vec<Field>, extra: impl IntoIterator<Item = String>) -> Layout {
        for path in extra.into_iter().take(MOST_EXTRA) {
            if fields.iter().all(|field| field.path != path) {
                fields.push(Field {
                    path,
                    kind: UNMAPPED.into(),
                    keyword: None,
                });
            }
        }
        Layout { fields }
    }

    pub(super) fn field(&self, path: &str) -> Option<&Field> {
        self.fields.iter().find(|field| field.path == path)
    }

    /// The class of a column, the id included.
    pub(super) fn class(&self, column: &str) -> Option<Class> {
        if column == ID {
            return Some(Class::Id);
        }
        self.field(column).map(Field::class)
    }

    pub(super) fn result_columns(&self) -> Vec<ResultColumn> {
        std::iter::once(ResultColumn {
            name: ID.into(),
            data_type: ID.into(),
        })
        .chain(self.fields.iter().map(|field| ResultColumn {
            name: field.path.clone(),
            data_type: field.kind.clone(),
        }))
        .collect()
    }

    pub(super) fn column_info(&self) -> Vec<ColumnInfo> {
        std::iter::once(ColumnInfo {
            name: ID.into(),
            data_type: ID.into(),
            nullable: false,
            primary_key: true,
            // Left out, the server makes an id of its own.
            default: Some(String::new()),
        })
        .chain(self.fields.iter().map(|field| ColumnInfo {
            name: field.path.clone(),
            data_type: field.kind.clone(),
            nullable: true,
            primary_key: false,
            // Every field of a document may be missing.
            default: Some(String::new()),
        }))
        .collect()
    }

    /// The cells of a hit in the order of the columns.
    pub(super) fn row(&self, id: &str, source: &Value) -> Vec<Cell> {
        std::iter::once(Cell::Text(id.to_owned()))
            .chain(
                self.fields
                    .iter()
                    .map(|field| cell(field.class(), &lookup(source, &field.path))),
            )
            .collect()
    }

    /// The paths documents hold that no column shows yet.
    pub(super) fn missing_from<'a>(
        &self,
        sources: impl IntoIterator<Item = &'a Value>,
    ) -> Vec<String> {
        let mut found = BTreeSet::new();
        for source in sources {
            if let Some(object) = source.as_object() {
                leaves(object, "", &mut found);
            }
        }
        found
            .into_iter()
            .filter(|path| {
                !self.fields.iter().any(|field| {
                    field.path == *path
                        // A column of JSON shows everything below it.
                        || path
                            .strip_prefix(field.path.as_str())
                            .is_some_and(|rest| rest.starts_with('.'))
                })
            })
            .take(MOST_EXTRA)
            .collect()
    }
}

/// The paths of the values of a document: objects are walked into, a list
/// is a value.
fn leaves(object: &Map<String, Value>, prefix: &str, out: &mut BTreeSet<String>) {
    for (name, value) in object {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        match value {
            Value::Object(inner) if !inner.is_empty() && path.len() < 200 => {
                leaves(inner, &path, out);
            }
            _ => {
                out.insert(path);
            }
        }
    }
}

/// What a document holds under a path.
#[derive(Debug, PartialEq)]
pub(super) enum Found<'a> {
    Missing,
    One(&'a Value),
    /// The path leads through a list of objects: one value per element.
    Many(Vec<&'a Value>),
}

fn collect<'a>(value: &'a Value, segments: &[&str], out: &mut Vec<&'a Value>, listed: &mut bool) {
    if segments.is_empty() {
        out.push(value);
        return;
    }
    match value {
        // A document may spell a path with dots in a key, with nested
        // objects, or with both: `{"a.b": 1}` and `{"a": {"b": 1}}` are the
        // same field to the server.
        Value::Object(object) => {
            for taken in 1..=segments.len() {
                if let Some(child) = object.get(&segments[..taken].join(".")) {
                    collect(child, &segments[taken..], out, listed);
                }
            }
        }
        Value::Array(elements) => {
            *listed = true;
            for element in elements {
                collect(element, segments, out, listed);
            }
        }
        _ => {}
    }
}

/// The value of a field in a document.
pub(super) fn lookup<'a>(source: &'a Value, path: &str) -> Found<'a> {
    let segments: Vec<&str> = path.split('.').collect();
    let mut out = Vec::new();
    let mut listed = false;
    collect(source, &segments, &mut out, &mut listed);
    match (out.len(), listed) {
        (0, _) => Found::Missing,
        (1, false) => Found::One(out[0]),
        _ => Found::Many(out),
    }
}

/// A number of JSON as a cell: its digits as they were written.
fn number_cell(number: &serde_json::Number) -> Cell {
    let text = number.to_string();
    if text.contains(['.', 'e', 'E']) {
        Cell::Decimal(text)
    } else {
        Cell::Integer(text)
    }
}

/// A value of JSON as a cell, when nothing is known about its field.
pub(super) fn plain_cell(value: &Value) -> Cell {
    match value {
        Value::Null => Cell::Null,
        Value::Bool(flag) => Cell::Bool(*flag),
        Value::Number(number) => number_cell(number),
        Value::String(text) => Cell::Text(text.clone()),
        other => Cell::Json(other.to_string()),
    }
}

/// What a document holds in a field, as a cell.
pub(super) fn cell(class: Class, found: &Found<'_>) -> Cell {
    match found {
        Found::Missing => Cell::Null,
        Found::Many(values) => Cell::Json(
            Value::Array(values.iter().map(|value| (*value).clone()).collect()).to_string(),
        ),
        Found::One(Value::String(text)) if class == Class::Binary => {
            use base64::Engine as _;
            match base64::engine::general_purpose::STANDARD.decode(text.as_bytes()) {
                Ok(bytes) => Cell::Bytes(bytes),
                Err(_) => Cell::Text(text.clone()),
            }
        }
        Found::One(value) => plain_cell(value),
    }
}

/// A text as a JSON number, when it is written like one.
fn number(text: &str) -> Option<Value> {
    let text = text.trim();
    let first = text.strip_prefix('-').unwrap_or(text).bytes().next()?;
    // `NaN`, `Infinity` and a leading `+` are not JSON.
    if !first.is_ascii_digit() {
        return None;
    }
    serde_json::from_str::<serde_json::Number>(text)
        .ok()
        .map(Value::Number)
}

/// A typed text as the JSON value a field of this class takes.
fn typed(class: Class, text: &str) -> Result<Value> {
    Ok(match class {
        Class::Integer | Class::Float => {
            number(text).ok_or_else(|| anyhow!("elasticsearch_value_invalid"))?
        }
        Class::Boolean => match text.trim().to_ascii_lowercase().as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => bail!("elasticsearch_value_invalid"),
        },
        // An object or a list is typed as JSON; a geo point may also be
        // written as `lat,lon` and a shape as WKT, which stay texts.
        Class::Json => match serde_json::from_str::<Value>(text) {
            Ok(value @ (Value::Object(_) | Value::Array(_))) => value,
            _ => Value::String(text.to_owned()),
        },
        _ => Value::String(text.to_owned()),
    })
}

/// A cell as the JSON value that is written into a document.
///
/// The interface guesses the kind of a new value from the name of the
/// column type, so the class of the field decides and the kind of the cell
/// is only a hint: a number stays a number where it is written like one.
pub(super) fn json(class: Class, cell: &Cell) -> Result<Value> {
    Ok(match cell {
        Cell::Null => Value::Null,
        Cell::Bool(flag) => Value::Bool(*flag),
        Cell::Bytes(bytes) => {
            use base64::Engine as _;
            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes))
        }
        Cell::Json(text) => {
            serde_json::from_str(text).map_err(|_| anyhow!("elasticsearch_value_invalid"))?
        }
        Cell::Integer(text) | Cell::Decimal(text) | Cell::Float(text) => match number(text) {
            Some(value) if class != Class::Boolean => value,
            _ => typed(class, text)?,
        },
        Cell::Text(text) => typed(class, text)?,
        Cell::Unsupported(_) => bail!("cell_read_only"),
    })
}

/// A path of nested objects that ends in a value.
fn nested(segments: &[&str], value: Value) -> Value {
    segments.iter().rev().fold(value, |inner, name| {
        let mut object = Map::new();
        object.insert((*name).to_owned(), inner);
        Value::Object(object)
    })
}

/// The partial document that gives one field a new value: what `_update`
/// merges into the document.
///
/// It is written the way the document spells the path, because a second
/// spelling of the same field would give the field two values. A path that
/// leads through a list cannot be written this way: the merge would replace
/// the whole list.
pub(super) fn partial(source: &Value, path: &str, value: Value) -> Result<Value> {
    fn place(current: Option<&Value>, segments: &[&str], value: Value) -> Result<Value> {
        match current {
            Some(Value::Object(object)) => {
                for taken in 1..=segments.len() {
                    let key = segments[..taken].join(".");
                    if let Some(child) = object.get(&key) {
                        let inner = if taken == segments.len() {
                            value
                        } else {
                            place(Some(child), &segments[taken..], value)?
                        };
                        let mut merged = Map::new();
                        merged.insert(key, inner);
                        return Ok(Value::Object(merged));
                    }
                }
                Ok(nested(segments, value))
            }
            None | Some(Value::Null) => Ok(nested(segments, value)),
            Some(_) => bail!("elasticsearch_field_in_list"),
        }
    }
    let segments: Vec<&str> = path.split('.').collect();
    ensure!(
        segments.iter().all(|segment| !segment.is_empty()),
        "column_not_found"
    );
    place(Some(source), &segments, value)
}

/// Put a value into a new document under its dotted path.
pub(super) fn put(document: &mut Map<String, Value>, path: &str, value: Value) -> Result<()> {
    let mut segments = path.split('.').peekable();
    let mut object = document;
    while let Some(segment) = segments.next() {
        ensure!(!segment.is_empty(), "column_not_found");
        if segments.peek().is_none() {
            // `a` after `a.b`: the same place cannot be an object and a value.
            ensure!(
                !matches!(object.get(segment), Some(Value::Object(_))),
                "elasticsearch_value_invalid"
            );
            object.insert(segment.to_owned(), value);
            return Ok(());
        }
        let child = object
            .entry(segment.to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        object = match child {
            Value::Object(inner) => inner,
            _ => bail!("elasticsearch_value_invalid"),
        };
    }
    Ok(())
}

/// The document a row of values stands for. A value that is nothing is
/// left out: a missing field and a null are the same to the index.
pub(super) fn document<'a>(
    layout: &Layout,
    values: impl IntoIterator<Item = (&'a str, &'a Cell)>,
) -> Result<(Option<String>, Map<String, Value>)> {
    let mut id = None;
    let mut document = Map::new();
    for (name, cell) in values {
        if name == ID {
            match cell {
                Cell::Null => {}
                Cell::Unsupported(_) | Cell::Bytes(_) | Cell::Json(_) | Cell::Bool(_) => {
                    bail!("elasticsearch_id_invalid")
                }
                other => {
                    let text = other.display();
                    // The server takes an id of up to 512 bytes.
                    ensure!(
                        !text.is_empty() && text.len() <= 512,
                        "elasticsearch_id_invalid"
                    );
                    id = Some(text);
                }
            }
            continue;
        }
        if matches!(cell, Cell::Null) {
            continue;
        }
        let class = layout
            .class(name)
            .ok_or_else(|| anyhow!("column_not_found"))?;
        put(&mut document, name, json(class, cell)?)?;
    }
    Ok((id, document))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn layout_of(mappings: Value) -> Layout {
        Layout::new(fields(&json!({ "idx": { "mappings": mappings } })), [])
    }

    fn sample() -> Layout {
        layout_of(json!({ "properties": {
            "title": { "type": "text", "fields": {
                "raw": { "type": "keyword" },
                "keyword": { "type": "keyword", "ignore_above": 256 },
                "english": { "type": "text", "analyzer": "english" },
            }},
            "body": { "type": "text" },
            "tag": { "type": "keyword" },
            "count": { "type": "long" },
            "price": { "type": "scaled_float", "scaling_factor": 100 },
            "seen": { "type": "date" },
            "active": { "type": "boolean" },
            "client": { "type": "ip" },
            "blob": { "type": "binary" },
            "place": { "type": "geo_point" },
            "address": { "properties": {
                "city": { "type": "keyword" },
                "geo": { "properties": { "zip": { "type": "keyword" } } },
            }},
            "items": { "type": "nested", "properties": { "sku": { "type": "keyword" } } },
            "raw": { "type": "object", "enabled": false },
            "empty": { "type": "object" },
            "label": { "type": "alias", "path": "tag" },
        }}))
    }

    #[test]
    fn the_fields_of_a_mapping_become_columns_by_their_dotted_paths() {
        let layout = sample();
        let listed: Vec<(&str, &str)> = layout
            .fields
            .iter()
            .map(|field| (field.path.as_str(), field.kind.as_str()))
            .collect();
        assert_eq!(
            listed,
            [
                ("active", "boolean"),
                ("address.city", "keyword"),
                ("address.geo.zip", "keyword"),
                ("blob", "binary"),
                ("body", "text"),
                ("client", "ip"),
                ("count", "long"),
                ("empty", "object"),
                ("items", "nested"),
                ("place", "geo_point"),
                ("price", "scaled_float"),
                ("raw", "object"),
                ("seen", "date"),
                ("tag", "keyword"),
                ("title", "text"),
            ],
            "objects are walked into; a nested list, a disabled object and an alias are not"
        );
        // The exact form of a text: the sub-field called `keyword` first.
        assert_eq!(
            layout.field("title").unwrap().keyword.as_deref(),
            Some("title.keyword")
        );
        assert_eq!(layout.field("body").unwrap().keyword, None);
        // `_id` comes first and is the key; every field may be missing.
        let info = layout.column_info();
        assert_eq!((info[0].name.as_str(), info[0].primary_key), (ID, true));
        assert!(!info[0].nullable);
        assert!(
            info[1..]
                .iter()
                .all(|column| column.nullable && !column.primary_key && column.default.is_some())
        );
        assert_eq!(layout.result_columns().len(), info.len());
        assert_eq!(layout.class(ID), Some(Class::Id));
        assert_eq!(layout.class("price"), Some(Class::Float));
        assert_eq!(layout.class("place"), Some(Class::Json));
        assert_eq!(layout.class("nothing"), None);
    }

    #[test]
    fn several_indices_and_typed_mappings_are_read_as_one_list() {
        let answer = json!({
            "logs-1": { "mappings": { "properties": {
                "message": { "type": "text" },
                "level": { "type": "keyword" },
            }}},
            "logs-2": { "mappings": { "properties": {
                "level": { "type": "text" },
                "host": { "type": "keyword" },
            }}},
            // A server before 7.0 names a type between the two.
            "old": { "mappings": { "doc": { "properties": { "legacy": { "type": "integer" } } } } },
            "bare": { "mappings": {} },
        });
        let found = fields(&answer);
        let listed: Vec<(&str, &str)> = found
            .iter()
            .map(|field| (field.path.as_str(), field.kind.as_str()))
            .collect();
        assert_eq!(
            listed,
            [
                ("host", "keyword"),
                ("legacy", "integer"),
                ("level", "keyword"),
                ("message", "text"),
            ]
        );
        assert!(fields(&json!({})).is_empty());
        assert!(fields(&json!("not a mapping")).is_empty());
    }

    #[test]
    fn every_type_has_a_class() {
        for (kind, expected) in [
            ("text", Class::Text),
            ("match_only_text", Class::Text),
            ("keyword", Class::Keyword),
            ("constant_keyword", Class::Keyword),
            ("wildcard", Class::Keyword),
            ("long", Class::Integer),
            ("unsigned_long", Class::Integer),
            ("half_float", Class::Float),
            ("scaled_float", Class::Float),
            ("date_nanos", Class::Date),
            ("boolean", Class::Boolean),
            ("ip", Class::Ip),
            ("binary", Class::Binary),
            ("geo_point", Class::Json),
            ("geo_shape", Class::Json),
            ("integer_range", Class::Json),
            ("dense_vector", Class::Json),
            ("knn_vector", Class::Json),
            ("flattened", Class::Json),
            ("nested", Class::Json),
            ("object", Class::Json),
            ("something_new", Class::Json),
            (UNMAPPED, Class::Unmapped),
            (ID, Class::Id),
        ] {
            assert_eq!(class(kind), expected, "{kind}");
        }
    }

    #[test]
    fn a_document_becomes_a_row_of_typed_cells() {
        let layout = sample();
        let source = json!({
            "title": "Zażółć gęślą jaźń",
            "tag": ["a", "b"],
            "count": 18446744073709551615u64,
            "price": 12.50,
            "seen": "2026-10-12T08:30:00Z",
            "active": true,
            "client": "10.0.0.7",
            "blob": "AAEC/w==",
            "place": { "lat": 52.23, "lon": 21.01 },
            "address": { "city": "Warszawa", "geo": { "zip": "00-001" } },
            "items": [{ "sku": "a" }, { "sku": "b" }],
            "raw": { "anything": [1, 2] },
        });
        let row = layout.row("doc/1", &source);
        let at = |name: &str| {
            let index = layout
                .result_columns()
                .iter()
                .position(|column| column.name == name)
                .unwrap();
            row[index].clone()
        };
        assert_eq!(at(ID), Cell::Text("doc/1".into()));
        assert_eq!(at("title"), Cell::Text("Zażółć gęślą jaźń".into()));
        assert_eq!(at("tag"), Cell::Json(r#"["a","b"]"#.into()));
        // A number keeps every digit it was written with.
        assert_eq!(at("count"), Cell::Integer("18446744073709551615".into()));
        assert_eq!(at("seen"), Cell::Text("2026-10-12T08:30:00Z".into()));
        assert_eq!(at("active"), Cell::Bool(true));
        assert_eq!(at("client"), Cell::Text("10.0.0.7".into()));
        assert_eq!(at("blob"), Cell::Bytes(vec![0, 1, 2, 255]));
        assert_eq!(
            at("place"),
            Cell::Json(r#"{"lat":52.23,"lon":21.01}"#.into())
        );
        assert_eq!(at("address.city"), Cell::Text("Warszawa".into()));
        assert_eq!(at("address.geo.zip"), Cell::Text("00-001".into()));
        assert_eq!(
            at("items"),
            Cell::Json(r#"[{"sku":"a"},{"sku":"b"}]"#.into())
        );
        assert_eq!(at("raw"), Cell::Json(r#"{"anything":[1,2]}"#.into()));
        // What a document does not hold is nothing.
        assert_eq!(at("body"), Cell::Null);
        assert_eq!(at("empty"), Cell::Null);
        // `_source` holds what was sent, whatever the mapping says.
        let loose = layout.row(
            "2",
            &json!({ "count": "42", "active": "true", "blob": "not base64!", "price": 1e3, "seen": 1760000000000u64 }),
        );
        let loose_at = |name: &str| {
            let index = layout
                .result_columns()
                .iter()
                .position(|column| column.name == name)
                .unwrap();
            loose[index].clone()
        };
        assert_eq!(loose_at("count"), Cell::Text("42".into()));
        assert_eq!(loose_at("active"), Cell::Text("true".into()));
        assert_eq!(loose_at("blob"), Cell::Text("not base64!".into()));
        assert_eq!(loose_at("seen"), Cell::Integer("1760000000000".into()));
        assert!(matches!(loose_at("price"), Cell::Decimal(_)));
    }

    #[test]
    fn a_decimal_keeps_the_digits_of_the_document() {
        // Read from text, as an answer of the server is.
        let source: Value = serde_json::from_str(r#"{"price": 12.50}"#).unwrap();
        let layout = sample();
        let row = layout.row("1", &source);
        let price = layout
            .result_columns()
            .iter()
            .position(|column| column.name == "price")
            .unwrap();
        assert_eq!(row[price], Cell::Decimal("12.50".into()));
    }

    #[test]
    fn a_path_is_found_however_the_document_spells_it() {
        let nested = json!({ "a": { "b": { "c": 1 } } });
        let dotted = json!({ "a.b.c": 1 });
        let mixed = json!({ "a": { "b.c": 1 } });
        let half = json!({ "a.b": { "c": 1 } });
        for source in [&nested, &dotted, &mixed, &half] {
            assert_eq!(lookup(source, "a.b.c"), Found::One(&json!(1)), "{source}");
        }
        assert_eq!(lookup(&nested, "a.b.x"), Found::Missing);
        assert_eq!(lookup(&nested, "a.b"), Found::One(&json!({ "c": 1 })));
        assert_eq!(lookup(&json!({ "a": null }), "a"), Found::One(&Value::Null));
        assert_eq!(lookup(&json!({ "a": 5 }), "a.b"), Found::Missing);
        // Through a list of objects: the values of its elements.
        let listed = json!({ "items": [{ "sku": "a", "qty": 1 }, { "sku": "b" }, 7] });
        assert_eq!(
            lookup(&listed, "items.sku"),
            Found::Many(vec![&json!("a"), &json!("b")])
        );
        assert_eq!(lookup(&listed, "items.qty"), Found::Many(vec![&json!(1)]));
        assert_eq!(
            cell(Class::Keyword, &lookup(&listed, "items.sku")),
            Cell::Json(r#"["a","b"]"#.into())
        );
        assert_eq!(lookup(&listed, "items.none"), Found::Missing);
    }

    #[test]
    fn a_cell_goes_back_as_the_json_its_field_takes() {
        let text = |value: &str| Cell::Text(value.into());
        assert_eq!(json(Class::Keyword, &text("a b")).unwrap(), json!("a b"));
        assert_eq!(json(Class::Integer, &text(" 42 ")).unwrap(), json!(42));
        assert_eq!(
            json(
                Class::Integer,
                &Cell::Integer("18446744073709551615".into())
            )
            .unwrap()
            .to_string(),
            "18446744073709551615"
        );
        assert_eq!(
            json(Class::Float, &Cell::Decimal("12.50".into()))
                .unwrap()
                .to_string(),
            "12.50"
        );
        assert_eq!(json(Class::Boolean, &text("TRUE")).unwrap(), json!(true));
        assert_eq!(
            json(Class::Boolean, &Cell::Bool(false)).unwrap(),
            json!(false)
        );
        assert_eq!(
            json(Class::Date, &text("2026-10-12")).unwrap(),
            json!("2026-10-12")
        );
        assert_eq!(json(Class::Ip, &text("::1")).unwrap(), json!("::1"));
        assert_eq!(json(Class::Text, &Cell::Null).unwrap(), Value::Null);
        // JSON for what is not a plain value; a geo point may be a text.
        assert_eq!(
            json(Class::Json, &text(r#"{"lat": 1, "lon": 2}"#)).unwrap(),
            json!({ "lat": 1, "lon": 2 })
        );
        assert_eq!(
            json(Class::Json, &text("52.2,21.0")).unwrap(),
            json!("52.2,21.0")
        );
        assert_eq!(
            json(Class::Keyword, &Cell::Json(r#"["a","b"]"#.into())).unwrap(),
            json!(["a", "b"])
        );
        assert_eq!(
            json(Class::Binary, &Cell::Bytes(vec![0, 1, 2, 255])).unwrap(),
            json!("AAEC/w==")
        );
        // The dialog of a new row calls a geo point an integer: the class
        // of the field decides, not the kind of the cell.
        assert_eq!(
            json(Class::Json, &Cell::Integer("52.2,21.0".into())).unwrap(),
            json!("52.2,21.0")
        );
        assert_eq!(
            json(Class::Keyword, &Cell::Integer("7".into())).unwrap(),
            json!(7)
        );
        assert_eq!(
            json(Class::Unmapped, &text("free text")).unwrap(),
            json!("free text")
        );
        for (class, cell) in [
            (Class::Integer, text("many")),
            (Class::Integer, text("")),
            (Class::Float, text("NaN")),
            (Class::Float, text("+1")),
            (Class::Boolean, text("yes")),
            (Class::Boolean, Cell::Integer("1".into())),
            (Class::Keyword, Cell::Json("{broken".into())),
        ] {
            assert_eq!(
                json(class, &cell).unwrap_err().to_string(),
                "elasticsearch_value_invalid",
                "{class:?} {cell:?}"
            );
        }
        assert_eq!(
            json(Class::Keyword, &Cell::Unsupported("x".into()))
                .unwrap_err()
                .to_string(),
            "cell_read_only"
        );
    }

    #[test]
    fn one_field_is_changed_in_the_spelling_of_the_document() {
        let change = |source: Value, path: &str| partial(&source, path, json!("new"));
        assert_eq!(
            change(json!({ "a": { "b": 1, "c": 2 } }), "a.b").unwrap(),
            json!({ "a": { "b": "new" } })
        );
        assert_eq!(
            change(json!({ "a.b": 1 }), "a.b").unwrap(),
            json!({ "a.b": "new" })
        );
        assert_eq!(
            change(json!({ "a": { "b.c": 1 } }), "a.b.c").unwrap(),
            json!({ "a": { "b.c": "new" } })
        );
        // A field the document does not hold yet is nested from where the
        // document ends.
        assert_eq!(
            change(json!({}), "a.b").unwrap(),
            json!({ "a": { "b": "new" } })
        );
        assert_eq!(
            change(json!({ "a": { "x": 1 } }), "a.b.c").unwrap(),
            json!({ "a": { "b": { "c": "new" } } })
        );
        assert_eq!(
            change(json!({ "a": null }), "a.b").unwrap(),
            json!({ "a": { "b": "new" } })
        );
        assert_eq!(
            change(json!({ "top": 1 }), "top").unwrap(),
            json!({ "top": "new" })
        );
        // Through a list the merge of the server would replace the list.
        assert_eq!(
            change(json!({ "items": [{ "sku": "a" }] }), "items.sku")
                .unwrap_err()
                .to_string(),
            "elasticsearch_field_in_list"
        );
        assert_eq!(
            change(json!({ "a": 5 }), "a.b").unwrap_err().to_string(),
            "elasticsearch_field_in_list"
        );
    }

    #[test]
    fn a_row_of_values_becomes_a_document() {
        let layout = sample();
        let values = [
            (ID, Cell::Text("order 7".into())),
            ("title", Cell::Text("Nowe".into())),
            ("count", Cell::Integer("3".into())),
            ("address.city", Cell::Text("Gdańsk".into())),
            ("address.geo.zip", Cell::Text("80-001".into())),
            ("active", Cell::Text("true".into())),
            ("body", Cell::Null),
            ("place", Cell::Integer("54.35,18.65".into())),
        ];
        let (id, made) =
            document(&layout, values.iter().map(|(name, cell)| (*name, cell))).unwrap();
        assert_eq!(id.as_deref(), Some("order 7"));
        assert_eq!(
            Value::Object(made),
            json!({
                "title": "Nowe",
                "count": 3,
                "address": { "city": "Gdańsk", "geo": { "zip": "80-001" } },
                "active": true,
                "place": "54.35,18.65",
            })
        );
        // Without an id the server makes one.
        let (id, _) = document(&layout, [("tag", &Cell::Text("x".into()))]).unwrap();
        assert_eq!(id, None);
        let code =
            |name: &str, cell: Cell| document(&layout, [(name, &cell)]).unwrap_err().to_string();
        assert_eq!(code("nothing", Cell::Text("x".into())), "column_not_found");
        assert_eq!(
            code(ID, Cell::Text(String::new())),
            "elasticsearch_id_invalid"
        );
        assert_eq!(
            code(ID, Cell::Text("x".repeat(513))),
            "elasticsearch_id_invalid"
        );
        assert_eq!(
            code("count", Cell::Text("x".into())),
            "elasticsearch_value_invalid"
        );
        // The same place cannot be a value and an object.
        let mut clash = Map::new();
        put(&mut clash, "a.b", json!(1)).unwrap();
        assert_eq!(
            put(&mut clash, "a", json!(2)).unwrap_err().to_string(),
            "elasticsearch_value_invalid"
        );
        assert_eq!(
            put(&mut clash, "a.b.c", json!(2)).unwrap_err().to_string(),
            "elasticsearch_value_invalid"
        );
    }

    #[test]
    fn fields_without_a_mapping_are_found_in_documents() {
        let layout = sample();
        let sources = [
            json!({ "title": "t", "extra": 1, "address": { "city": "x", "street": "y" } }),
            json!({ "place": { "lat": 1, "lon": 2 }, "raw": { "deep": { "er": 1 } }, "more": { "inner": [1, 2] } }),
            json!({ "items": [{ "sku": "a", "unknown": 1 }], "extra": 2 }),
        ];
        assert_eq!(
            layout.missing_from(sources.iter()),
            ["address.street", "extra", "more.inner"],
            "what a JSON column shows is not listed again"
        );
        let wider = Layout::new(layout.fields.clone(), layout.missing_from(sources.iter()));
        assert_eq!(wider.class("extra"), Some(Class::Unmapped));
        assert_eq!(wider.field("address.street").unwrap().kind, UNMAPPED);
        assert!(wider.missing_from(sources.iter()).is_empty());
        // A mapped field is never listed twice.
        assert_eq!(
            Layout::new(layout.fields.clone(), ["tag".to_owned()])
                .fields
                .len(),
            layout.fields.len()
        );
    }
}
