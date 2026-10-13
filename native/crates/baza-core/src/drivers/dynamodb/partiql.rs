//! The text side of the driver: what the editor sends as PartiQL, and what
//! a change of the grid looks like as a PartiQL statement in the history.
use super::browse::TableInfo;
use super::value;
use serde_json::{Map, Value};

/// A statement without its comments. PartiQL of DynamoDB takes none; the
/// page texts of this driver and people's notes carry them.
pub(super) fn without_comments(statement: &str) -> String {
    let bytes = statement.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            quote @ (b'\'' | b'"') => {
                // A quoted text or name, with a doubled quote inside.
                let start = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == quote {
                        if bytes.get(i + 1) == Some(&quote) {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                out.extend_from_slice(&bytes[start..i.min(bytes.len())]);
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !bytes[i..].starts_with(b"*/") {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                out.push(b' ');
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).trim().to_owned()
}

/// The first words of a statement, in upper case.
pub(super) fn leading_words(statement: &str, count: usize) -> Vec<String> {
    statement
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|word| !word.is_empty())
        .take(count)
        .map(str::to_uppercase)
        .collect()
}

/// `SELECT 1`: what the connection dialog asks every engine, and what a
/// new editor tab holds. PartiQL of DynamoDB has no such statement.
pub(super) fn is_probe(statement: &str) -> bool {
    statement
        .trim()
        .trim_end_matches(';')
        .split_whitespace()
        .map(str::to_uppercase)
        .collect::<Vec<_>>()
        == ["SELECT", "1"]
}

/// A name at the start of a text: `"Quoted ""name"""` or a plain word.
/// Returns the name and what follows it.
fn name_at(text: &str) -> Option<(String, &str)> {
    let text = text.trim_start();
    if let Some(rest) = text.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = rest.char_indices();
        while let Some((at, c)) = chars.next() {
            if c != '"' {
                name.push(c);
            } else if rest[at + 1..].starts_with('"') {
                name.push('"');
                chars.next();
            } else {
                return Some((name, &rest[at + 1..]));
            }
        }
        return None;
    }
    let end = text
        .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.')))
        .unwrap_or(text.len());
    (end > 0).then(|| (text[..end].to_owned(), &text[end..]))
}

/// `INSERT INTO "table" VALUE { … }` with a JSON document after `VALUE`:
/// the table and the document. PartiQL writes a document with single
/// quotes (`{'id': 1}`); JSON as it is copied from anywhere else has double
/// quotes, which PartiQL reads as names, so such a statement is put as an
/// item by this driver instead of being refused by the service.
pub(super) fn json_insert(statement: &str) -> Option<(String, Map<String, Value>)> {
    let text = statement.trim().trim_end_matches(';').trim_end();
    let rest = strip_word(strip_word(text, "INSERT")?, "INTO")?;
    let (table, rest) = name_at(rest)?;
    let document = strip_word(rest, "VALUE")?.trim_start();
    if !document.starts_with('{') {
        return None;
    }
    match serde_json::from_str::<Value>(document).ok()? {
        Value::Object(item) => Some((table, item)),
        _ => None,
    }
}

/// The text after a leading keyword, in any letter case.
fn strip_word<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let text = text.trim_start();
    let start = text.get(..word.len())?;
    let rest = &text[word.len()..];
    (start.eq_ignore_ascii_case(word)
        && !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
    .then_some(rest)
}

/// The attributes a `SELECT a, "b" FROM …` names, in its order; nothing
/// for `SELECT *` and for anything that is not a plain list of names.
pub(super) fn projection(statement: &str) -> Vec<String> {
    let Some(mut rest) = strip_word(statement, "SELECT") else {
        return Vec::new();
    };
    let mut names = Vec::new();
    loop {
        let Some((name, after)) = name_at(rest) else {
            return Vec::new();
        };
        names.push(name);
        let after = after.trim_start();
        if let Some(next) = after.strip_prefix(',') {
            rest = next;
        } else if strip_word(after, "FROM").is_some() {
            return names;
        } else {
            return Vec::new();
        }
    }
}

/// `"name" = literal AND …` for the key of an item.
fn key_condition(table: &TableInfo, key: &Map<String, Value>) -> String {
    table
        .keys
        .iter()
        .filter_map(|attribute| {
            key.get(&attribute.name).map(|value| {
                format!(
                    "{} = {}",
                    value::name(&attribute.name),
                    value::partiql(value)
                )
            })
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// A change of one attribute as PartiQL: the condition after the key is
/// what the service checks before it writes.
pub(super) fn update_text(
    table: &TableInfo,
    key: &Map<String, Value>,
    attribute: &str,
    old: Option<&Value>,
    new: Option<&Value>,
) -> String {
    let target = value::name(&table.name);
    let name = value::name(attribute);
    let change = match new {
        Some(new) => format!("SET {name} = {}", value::partiql(new)),
        None => format!("REMOVE {name}"),
    };
    let condition = match old {
        Some(old) => format!("{name} = {}", value::partiql(old)),
        None => format!("{name} IS MISSING"),
    };
    format!(
        "UPDATE {target} {change} WHERE {} AND {condition}",
        key_condition(table, key)
    )
}

/// A new item as PartiQL, which like this driver refuses a key that exists.
pub(super) fn insert_text(table: &TableInfo, item: &Map<String, Value>) -> String {
    format!(
        "INSERT INTO {} VALUE {}",
        value::name(&table.name),
        value::partiql(&serde_json::json!({ "M": item }))
    )
}

/// A removed item as PartiQL, with the check that it is still there.
pub(super) fn delete_text(table: &TableInfo, key: &Map<String, Value>) -> String {
    format!(
        "DELETE FROM {} WHERE {} AND attribute_exists({})",
        value::name(&table.name),
        key_condition(table, key),
        value::name(&table.partition().name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn music() -> TableInfo {
        TableInfo::of(&json!({
            "TableName": "Music",
            "KeySchema": [
                {"AttributeName": "Artist", "KeyType": "HASH"},
                {"AttributeName": "Year", "KeyType": "RANGE"},
            ],
            "AttributeDefinitions": [
                {"AttributeName": "Artist", "AttributeType": "S"},
                {"AttributeName": "Year", "AttributeType": "N"},
            ],
        }))
        .unwrap()
    }

    fn object(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn comments_are_taken_out_before_a_statement_is_sent() {
        assert_eq!(
            without_comments("SELECT * FROM \"Music\"\n-- 500 items a page\n-- filter x"),
            "SELECT * FROM \"Music\""
        );
        assert_eq!(
            without_comments("/* all */ SELECT * /* cols */ FROM t -- end"),
            "SELECT *   FROM t"
        );
        // Dashes and slashes inside quotes are data.
        assert_eq!(
            without_comments("SELECT * FROM \"a--b\" WHERE x = 'it''s -- not /* a */ comment'"),
            "SELECT * FROM \"a--b\" WHERE x = 'it''s -- not /* a */ comment'"
        );
        assert_eq!(without_comments("-- only a note"), "");
        assert_eq!(without_comments("SELECT 'open"), "SELECT 'open");
    }

    #[test]
    fn statements_the_driver_answers_itself_are_recognised() {
        for probe in ["SELECT 1", "select 1;", "  SELECT   1 ; "] {
            assert!(is_probe(probe), "{probe}");
        }
        for other in ["SELECT 1 FROM t", "SELECT 10", "SELECT * FROM t"] {
            assert!(!is_probe(other), "{other}");
        }
        assert_eq!(leading_words("  update \"t\" set", 2), ["UPDATE", "T"]);
        assert_eq!(leading_words("", 2), Vec::<String>::new());
    }

    #[test]
    fn a_json_document_after_value_is_an_item_to_put() {
        let (table, item) =
            json_insert("insert into \"Music\" value {\"Artist\": \"Queen\", \"Year\": 1975, \"Tags\": [\"rock\"]};")
                .unwrap();
        assert_eq!(table, "Music");
        assert_eq!(
            Value::Object(item),
            json!({"Artist": "Queen", "Year": 1975, "Tags": ["rock"]})
        );
        assert_eq!(
            json_insert("INSERT INTO Music VALUE {}").unwrap().0,
            "Music"
        );
        assert_eq!(
            json_insert("INSERT INTO \"Order \"\"x\"\"\" VALUE {\"id\": 1}")
                .unwrap()
                .0,
            "Order \"x\""
        );
        // PartiQL's own notation goes to the service as it is.
        assert!(json_insert("INSERT INTO \"Music\" VALUE {'Artist': 'Queen'}").is_none());
        assert!(json_insert("INSERT INTO \"Music\" VALUES {\"a\": 1}").is_none());
        assert!(json_insert("INSERT INTO \"Music\" VALUE [1]").is_none());
        assert!(json_insert("SELECT * FROM \"Music\"").is_none());
        assert!(json_insert("INSERTINTO t VALUE {}").is_none());
    }

    #[test]
    fn the_order_of_a_projection_is_read_from_the_statement() {
        assert_eq!(
            projection("SELECT \"Year\", Artist , \"Song \"\"x\"\"\" FROM \"Music\" WHERE a = 1"),
            ["Year", "Artist", "Song \"x\""]
        );
        assert_eq!(projection("select Title from t"), ["Title"]);
        assert!(projection("SELECT * FROM t").is_empty());
        assert!(projection("SELECT a.b[0] FROM t").is_empty());
        assert!(projection("SELECT size(a) FROM t").is_empty());
        assert!(projection("UPDATE t SET a = 1").is_empty());
    }

    #[test]
    fn a_change_of_the_grid_reads_as_partiql_in_the_history() {
        let table = music();
        let key = object(json!({"Artist": {"S": "Queen"}, "Year": {"N": "1975"}}));
        assert_eq!(
            update_text(
                &table,
                &key,
                "Title",
                Some(&json!({"S": "old"})),
                Some(&json!({"S": "it's new"}))
            ),
            "UPDATE \"Music\" SET \"Title\" = 'it''s new' WHERE \"Artist\" = 'Queen' AND \"Year\" = 1975 AND \"Title\" = 'old'"
        );
        // An attribute that was not there, and one that is taken away.
        assert_eq!(
            update_text(&table, &key, "Rating", None, Some(&json!({"N": "4.5"}))),
            "UPDATE \"Music\" SET \"Rating\" = 4.5 WHERE \"Artist\" = 'Queen' AND \"Year\" = 1975 AND \"Rating\" IS MISSING"
        );
        assert_eq!(
            update_text(&table, &key, "Rating", Some(&json!({"N": "4.5"})), None),
            "UPDATE \"Music\" REMOVE \"Rating\" WHERE \"Artist\" = 'Queen' AND \"Year\" = 1975 AND \"Rating\" = 4.5"
        );
        assert_eq!(
            insert_text(
                &table,
                &object(
                    json!({"Artist": {"S": "Queen"}, "Year": {"N": "1975"}, "Tags": {"SS": ["rock"]}})
                )
            ),
            "INSERT INTO \"Music\" VALUE {'Artist': 'Queen', 'Tags': <<'rock'>>, 'Year': 1975}"
        );
        assert_eq!(
            delete_text(&table, &key),
            "DELETE FROM \"Music\" WHERE \"Artist\" = 'Queen' AND \"Year\" = 1975 AND attribute_exists(\"Artist\")"
        );
    }
}
