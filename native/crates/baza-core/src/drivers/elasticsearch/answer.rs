//! The answer of a request of the console as a grid.
//!
//! Hits are rows, and so are the elements of a list of objects (`_cat`),
//! the items of `_bulk` and the buckets of an aggregation that stands
//! alone. Anything else is listed value by value, by its path.
//!
//! The answer is read with its names in the order the server wrote them:
//! a `_cat` table keeps the order of its columns and a document the order
//! of its fields. (The JSON reader of the workspace sorts names, which is
//! right for everything the driver looks values up in, and wrong for what
//! a person reads.)
use crate::model::{Cell, QueryResult, ResultColumn};
use crate::sql::MAX_RESULT_ROWS;

/// How deep an answer may be nested.
const DEPTH: usize = 128;

/// A value of JSON, with the names of an object in their order.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Node {
    Null,
    Bool(bool),
    /// The digits as they were written.
    Number(String),
    Text(String),
    List(Vec<Node>),
    Object(Vec<(String, Node)>),
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn peek(&mut self) -> Option<u8> {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.at += 1;
        }
        self.bytes.get(self.at).copied()
    }

    /// A string that starts here, with its escapes resolved.
    fn string(&mut self) -> Option<String> {
        let start = self.at;
        let mut end = start + 1;
        loop {
            match self.bytes.get(end)? {
                b'\\' => end += 2,
                b'"' => break,
                _ => end += 1,
            }
        }
        self.at = end + 1;
        serde_json::from_slice(&self.bytes[start..=end]).ok()
    }

    fn word(&mut self, word: &str, node: Node) -> Option<Node> {
        let end = self.at + word.len();
        (self.bytes.get(self.at..end)? == word.as_bytes()).then(|| {
            self.at = end;
            node
        })
    }

    fn value(&mut self, depth: usize) -> Option<Node> {
        match self.peek()? {
            b'{' => {
                self.at += 1;
                let mut entries = Vec::new();
                if self.peek()? == b'}' {
                    self.at += 1;
                    return Some(Node::Object(entries));
                }
                loop {
                    if self.peek()? != b'"' {
                        return None;
                    }
                    let name = self.string()?;
                    if self.peek()? != b':' {
                        return None;
                    }
                    self.at += 1;
                    entries.push((name, self.value(depth.checked_sub(1)?)?));
                    match self.peek()? {
                        b',' => self.at += 1,
                        b'}' => {
                            self.at += 1;
                            return Some(Node::Object(entries));
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.at += 1;
                let mut elements = Vec::new();
                if self.peek()? == b']' {
                    self.at += 1;
                    return Some(Node::List(elements));
                }
                loop {
                    elements.push(self.value(depth.checked_sub(1)?)?);
                    match self.peek()? {
                        b',' => self.at += 1,
                        b']' => {
                            self.at += 1;
                            return Some(Node::List(elements));
                        }
                        _ => return None,
                    }
                }
            }
            b'"' => self.string().map(Node::Text),
            b't' => self.word("true", Node::Bool(true)),
            b'f' => self.word("false", Node::Bool(false)),
            b'n' => self.word("null", Node::Null),
            b'-' | b'0'..=b'9' => {
                let start = self.at;
                while matches!(
                    self.bytes.get(self.at),
                    Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                ) {
                    self.at += 1;
                }
                let text = std::str::from_utf8(&self.bytes[start..self.at]).ok()?;
                serde_json::from_str::<serde_json::Number>(text).ok()?;
                Some(Node::Number(text.to_owned()))
            }
            _ => None,
        }
    }
}

/// Read a whole answer; nothing when it is not JSON.
pub(super) fn read(body: &[u8]) -> Option<Node> {
    let mut parser = Parser { bytes: body, at: 0 };
    let node = parser.value(DEPTH)?;
    (parser.peek().is_none()).then_some(node)
}

impl Node {
    fn get(&self, name: &str) -> Option<&Node> {
        match self {
            Node::Object(entries) => entries
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, node)| node),
            _ => None,
        }
    }

    fn object(&self) -> Option<&[(String, Node)]> {
        match self {
            Node::Object(entries) => Some(entries),
            _ => None,
        }
    }

    fn list(&self) -> Option<&[Node]> {
        match self {
            Node::List(elements) => Some(elements),
            _ => None,
        }
    }

    fn text(&self) -> Option<&str> {
        match self {
            Node::Text(text) => Some(text),
            _ => None,
        }
    }

    fn whole(&self) -> Option<u64> {
        match self {
            Node::Number(digits) => digits.parse().ok(),
            _ => None,
        }
    }

    fn is_null(&self) -> bool {
        matches!(self, Node::Null)
    }

    /// The value as compact JSON.
    fn write(&self, out: &mut String) {
        match self {
            Node::Null => out.push_str("null"),
            Node::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
            Node::Number(digits) => out.push_str(digits),
            Node::Text(text) => out.push_str(&serde_json::Value::String(text.clone()).to_string()),
            Node::List(elements) => {
                out.push('[');
                for (index, element) in elements.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    element.write(out);
                }
                out.push(']');
            }
            Node::Object(entries) => {
                out.push('{');
                for (index, (name, node)) in entries.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::Value::String(name.clone()).to_string());
                    out.push(':');
                    node.write(out);
                }
                out.push('}');
            }
        }
    }

    fn cell(&self) -> Cell {
        match self {
            Node::Null => Cell::Null,
            Node::Bool(flag) => Cell::Bool(*flag),
            Node::Number(digits) if digits.contains(['.', 'e', 'E']) => {
                Cell::Decimal(digits.clone())
            }
            Node::Number(digits) => Cell::Integer(digits.clone()),
            Node::Text(text) => Cell::Text(text.clone()),
            other => {
                let mut text = String::new();
                other.write(&mut text);
                Cell::Json(text)
            }
        }
    }
}

fn column(name: &str, kind: &str) -> ResultColumn {
    ResultColumn {
        name: name.to_owned(),
        data_type: kind.to_owned(),
    }
}

/// The type of a column, from the cells it holds.
fn kind_of<'a>(cells: impl IntoIterator<Item = &'a Cell>) -> &'static str {
    let mut kind = "";
    for cell in cells {
        let found = match cell {
            Cell::Null => continue,
            Cell::Bool(_) => "boolean",
            Cell::Integer(_) | Cell::Decimal(_) | Cell::Float(_) => "number",
            Cell::Json(_) => "json",
            _ => "text",
        };
        if kind.is_empty() {
            kind = found;
        } else if kind != found {
            return "mixed";
        }
    }
    kind
}

/// One row before it is laid out: its values by their paths.
type Flat<'a> = Vec<(String, &'a Node)>;

/// The values of an object by their dotted paths: objects are walked into,
/// a list is one value.
fn flatten<'a>(entries: &'a [(String, Node)], prefix: &str, out: &mut Flat<'a>) {
    for (name, node) in entries {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        match node {
            Node::Object(inner) if !inner.is_empty() => flatten(inner, &path, out),
            other => out.push((path, other)),
        }
    }
}

fn flat(entries: &[(String, Node)]) -> Flat<'_> {
    let mut out = Vec::new();
    flatten(entries, "", &mut out);
    out
}

/// Rows as a table: the columns are the paths that were found, in the
/// order they were first seen, after the `first` ones.
fn table(mut rows: Vec<Flat<'_>>, first: &[&str]) -> QueryResult {
    let truncated = rows.len() > MAX_RESULT_ROWS;
    rows.truncate(MAX_RESULT_ROWS);
    let mut names: Vec<&str> = Vec::new();
    let mut known = std::collections::HashSet::new();
    for row in &rows {
        for (name, _) in row {
            if known.insert(name.as_str()) {
                names.push(name);
            }
        }
    }
    names.sort_by_key(|name| {
        first
            .iter()
            .position(|known| known == name)
            .unwrap_or(first.len())
    });
    let place: std::collections::HashMap<&str, usize> = names
        .iter()
        .enumerate()
        .map(|(index, name)| (*name, index))
        .collect();
    let cells: Vec<Vec<Cell>> = rows
        .iter()
        .map(|row| {
            let mut cells = vec![Cell::Null; names.len()];
            for (name, node) in row {
                let at = place[name.as_str()];
                // A path spelled twice in one object: the first stands.
                if matches!(cells[at], Cell::Null) {
                    cells[at] = node.cell();
                }
            }
            cells
        })
        .collect();
    let columns = names
        .iter()
        .enumerate()
        .map(|(index, name)| column(name, kind_of(cells.iter().map(|row| &row[index]))))
        .collect();
    QueryResult {
        columns,
        rows: cells,
        truncated,
        ..QueryResult::default()
    }
}

/// The hits of a search as rows: the id, then the fields of the documents.
fn hits(found: &[Node]) -> QueryResult {
    let index_of = |hit: &Node| hit.get("_index").and_then(Node::text).map(str::to_owned);
    let several = found.iter().any(|hit| index_of(hit) != index_of(&found[0]));
    let rows = found
        .iter()
        .map(|hit| {
            let mut row: Flat<'_> = Vec::new();
            for (name, node) in hit.object().into_iter().flatten() {
                match (name.as_str(), node) {
                    ("_source", Node::Object(source)) => flatten(source, "", &mut row),
                    ("_index", _) if !several => {}
                    ("_score", Node::Null) | ("_type" | "_ignored", _) => {}
                    // What was asked for by name comes back as lists.
                    ("fields", Node::Object(fields)) => {
                        for (field, values) in fields {
                            let value = match values {
                                Node::List(list) if list.len() == 1 => &list[0],
                                other => other,
                            };
                            row.push((field.clone(), value));
                        }
                    }
                    _ => row.push((name.clone(), node)),
                }
            }
            row
        })
        .collect();
    table(rows, &["_index", "_id", "_score", "_routing", "_version"])
}

/// An answer that is neither a list nor hits: its values by their paths.
fn tree(answer: &[(String, Node)]) -> QueryResult {
    fn walk(node: &Node, path: String, rows: &mut Vec<Vec<Cell>>) {
        if rows.len() > MAX_RESULT_ROWS {
            return;
        }
        match node {
            Node::Object(entries) if !entries.is_empty() => {
                for (name, inner) in entries {
                    let path = if path.is_empty() {
                        name.clone()
                    } else {
                        format!("{path}.{name}")
                    };
                    walk(inner, path, rows);
                }
            }
            Node::List(elements)
                if !elements.is_empty()
                    && elements
                        .iter()
                        .all(|element| matches!(element, Node::Object(_))) =>
            {
                for (index, inner) in elements.iter().enumerate() {
                    walk(inner, format!("{path}[{index}]"), rows);
                }
            }
            other => rows.push(vec![Cell::Text(path), other.cell()]),
        }
    }
    let mut rows = Vec::new();
    for (name, node) in answer {
        walk(node, name.clone(), &mut rows);
    }
    let truncated = rows.len() > MAX_RESULT_ROWS;
    rows.truncate(MAX_RESULT_ROWS);
    QueryResult {
        columns: vec![column("key", "text"), column("value", "mixed")],
        rows,
        truncated,
        ..QueryResult::default()
    }
}

/// The documents an answer says were written or removed.
fn affected(answer: &Node) -> u64 {
    if let Some(items) = answer.get("items").and_then(Node::list) {
        return items
            .iter()
            .filter(|item| {
                item.object()
                    .and_then(|item| item.first())
                    .is_some_and(|(_, outcome)| outcome.get("error").is_none_or(Node::is_null))
            })
            .count() as u64;
    }
    if matches!(
        answer.get("result").and_then(Node::text),
        Some("created" | "updated" | "deleted")
    ) {
        return 1;
    }
    // `_delete_by_query`, `_update_by_query`, `_reindex`.
    if answer.get("took").is_some() && answer.get("batches").is_some() {
        return ["created", "updated", "deleted"]
            .iter()
            .filter_map(|name| answer.get(name).and_then(Node::whole))
            .sum();
    }
    0
}

/// The elements of a list when all of them are objects, and there are some.
fn objects(list: &[Node]) -> Option<Vec<Flat<'_>>> {
    if list.is_empty() {
        return None;
    }
    list.iter()
        .map(|element| element.object().map(flat))
        .collect()
}

/// The answer of a request as a grid.
pub(super) fn shown(status: u16, body: &[u8]) -> QueryResult {
    if body.iter().all(u8::is_ascii_whitespace) {
        return QueryResult {
            columns: vec![column("status", "number")],
            rows: vec![vec![Cell::Integer(status.to_string())]],
            ..QueryResult::default()
        };
    }
    let Some(answer) = read(body) else {
        // Text: `_cat` asked for as text, a help page.
        let text = String::from_utf8_lossy(body);
        let mut rows: Vec<Vec<Cell>> = text
            .lines()
            .map(|line| vec![Cell::Text(line.to_owned())])
            .collect();
        let truncated = rows.len() > MAX_RESULT_ROWS;
        rows.truncate(MAX_RESULT_ROWS);
        return QueryResult {
            columns: vec![column("text", "text")],
            rows,
            truncated,
            ..QueryResult::default()
        };
    };
    let values = |list: &[Node]| QueryResult {
        columns: vec![column("value", "mixed")],
        rows: list
            .iter()
            .take(MAX_RESULT_ROWS)
            .map(|node| vec![node.cell()])
            .collect(),
        truncated: list.len() > MAX_RESULT_ROWS,
        ..QueryResult::default()
    };
    let entries = match &answer {
        Node::List(list) => {
            return match objects(list) {
                Some(rows) => table(rows, &[]),
                None => values(list),
            };
        }
        Node::Object(entries) => entries,
        other => return values(std::slice::from_ref(other)),
    };
    let found = answer
        .get("hits")
        .and_then(|hits| hits.get("hits"))
        .and_then(Node::list)
        .filter(|found| !found.is_empty());
    let mut result = None;
    if let Some(found) = found {
        result = Some(hits(found));
    } else if answer.get("rows").is_some() || answer.get("datarows").is_some() {
        // The SQL API by its path answers like the SQL of the console.
        result = serde_json::from_slice(body)
            .ok()
            .and_then(|answer| super::sql::decode(&answer))
            .map(|page| QueryResult {
                columns: page.columns,
                rows: page.rows,
                ..QueryResult::default()
            });
    } else if let Some(items) = answer.get("items").and_then(Node::list) {
        // `_bulk`: what became of every action.
        let null = Node::Null;
        let outcomes: Vec<Vec<(String, Node)>> = items
            .iter()
            .filter_map(|item| {
                let (action, outcome) = item.object()?.first()?;
                let mut row = vec![("action".to_owned(), Node::Text(action.clone()))];
                for name in ["_index", "_id", "status", "result"] {
                    row.push((name.to_owned(), outcome.get(name).unwrap_or(&null).clone()));
                }
                let error = outcome
                    .get("error")
                    .and_then(|error| error.get("type"))
                    .unwrap_or(&null);
                row.push(("error".to_owned(), error.clone()));
                Some(row)
            })
            .collect();
        if !outcomes.is_empty() {
            let mut listed = table(outcomes.iter().map(|row| flat(row)).collect(), &[]);
            listed.affected = affected(&answer);
            return listed;
        }
    } else if let Some(buckets) = answer
        .get("aggregations")
        .and_then(Node::object)
        .filter(|aggregations| aggregations.len() == 1)
        .and_then(|aggregations| aggregations[0].1.get("buckets"))
        .and_then(Node::list)
        .and_then(objects)
    {
        result = Some(table(buckets, &["key", "key_as_string", "doc_count"]));
    } else if let [(_, Node::List(list))] = entries.as_slice()
        && let Some(rows) = objects(list)
    {
        // One list of objects under one name: `docs`, `tokens`, ….
        result = Some(table(rows, &[]));
    }
    let mut result = result.unwrap_or_else(|| tree(entries));
    result.affected = affected(&answer);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(result: &QueryResult) -> Vec<&str> {
        result
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect()
    }

    fn show(answer: &str) -> QueryResult {
        shown(200, answer.as_bytes())
    }

    #[test]
    fn an_answer_is_read_with_its_names_in_their_order() {
        let node = read(
            r#" { "zebra" : 1 , "alpha": [true, false, null, -1.50e3, "a\"b\u017c"], "m": {} , "e": [] } "#
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            node,
            Node::Object(vec![
                ("zebra".into(), Node::Number("1".into())),
                (
                    "alpha".into(),
                    Node::List(vec![
                        Node::Bool(true),
                        Node::Bool(false),
                        Node::Null,
                        Node::Number("-1.50e3".into()),
                        Node::Text("a\"bż".into()),
                    ])
                ),
                ("m".into(), Node::Object(Vec::new())),
                ("e".into(), Node::List(Vec::new())),
            ])
        );
        // Written back, it is the same document without its spaces.
        let mut text = String::new();
        node.write(&mut text);
        assert_eq!(
            text,
            r#"{"zebra":1,"alpha":[true,false,null,-1.50e3,"a\"bż"],"m":{},"e":[]}"#
        );
        assert_eq!(
            read(b"18446744073709551615").unwrap().cell(),
            Cell::Integer("18446744073709551615".into())
        );
        assert_eq!(read(b"\"x\"").unwrap(), Node::Text("x".into()));
        for broken in [
            "",
            "{",
            "{\"a\": }",
            "{\"a\": 1,}",
            "{\"a\" 1}",
            "{a: 1}",
            "[1, 2",
            "[1 2]",
            "tru",
            "1 2",
            "-",
            "1.2.3",
            "\"open",
            "{\"a\": 1} trailing",
        ] {
            assert_eq!(read(broken.as_bytes()), None, "{broken}");
        }
        // Nesting has an end: an answer cannot run the reader out of stack.
        let deep = format!("{}1{}", "[".repeat(5000), "]".repeat(5000));
        assert_eq!(read(deep.as_bytes()), None);
        assert!(read(format!("{}1{}", "[".repeat(100), "]".repeat(100)).as_bytes()).is_some());
    }

    #[test]
    fn hits_are_rows_and_their_fields_are_columns() {
        let result = show(
            r#"{"took": 3, "hits": {"total": {"value": 2, "relation": "eq"}, "max_score": 1.5, "hits": [
                {"_index": "bazaqa-orders", "_id": "1", "_score": 1.5,
                 "_source": {"title": "Nowe", "n": 1, "client": {"name": "Anna", "tags": ["a", "b"]}}},
                {"_index": "bazaqa-orders", "_id": "2", "_score": 0.5,
                 "_source": {"paid": true, "title": "Stare"}, "sort": [5]}
            ]}}"#,
        );
        // The id first, then the fields in the order of the documents.
        assert_eq!(
            names(&result),
            [
                "_id",
                "_score",
                "title",
                "n",
                "client.name",
                "client.tags",
                "paid",
                "sort"
            ]
        );
        assert_eq!(
            result.rows[0],
            [
                Cell::Text("1".into()),
                Cell::Decimal("1.5".into()),
                Cell::Text("Nowe".into()),
                Cell::Integer("1".into()),
                Cell::Text("Anna".into()),
                Cell::Json(r#"["a","b"]"#.into()),
                Cell::Null,
                Cell::Null,
            ]
        );
        assert_eq!(result.rows[1][6], Cell::Bool(true));
        assert_eq!(result.rows[1][7], Cell::Json("[5]".into()));
        let kinds: Vec<&str> = result
            .columns
            .iter()
            .map(|column| column.data_type.as_str())
            .collect();
        assert_eq!(
            kinds,
            [
                "text", "number", "text", "number", "text", "json", "boolean", "json"
            ]
        );
        // Hits of several indices say where each one is from; a score of
        // nothing and fields asked for by name are read as well.
        let several = show(
            r#"{"hits": {"hits": [
                {"_index": "a", "_id": "1", "_score": null, "fields": {"tags": ["x", "y"], "tag": ["x"]}},
                {"_id": "2", "_index": "b", "_score": null, "_source": {}}
            ]}}"#,
        );
        assert_eq!(names(&several), ["_index", "_id", "tags", "tag"]);
        assert_eq!(several.rows[0][2], Cell::Json(r#"["x","y"]"#.into()));
        assert_eq!(several.rows[0][3], Cell::Text("x".into()));
        assert_eq!(several.rows[1][0], Cell::Text("b".into()));
    }

    #[test]
    fn lists_buckets_and_bulk_items_are_tables() {
        // `_cat` as a list of rows: its columns in the order of the server.
        let cat = show(
            r#"[{"health": "green", "status": "open", "index": "a", "docs.count": "5"},
                {"health": "yellow", "status": "open", "index": "b", "docs.count": "7", "extra": null}]"#,
        );
        assert_eq!(
            names(&cat),
            ["health", "status", "index", "docs.count", "extra"]
        );
        assert_eq!(cat.rows.len(), 2);
        assert_eq!(cat.rows[1][0], Cell::Text("yellow".into()));
        assert_eq!(cat.rows[0][4], Cell::Null);
        // The buckets of the one aggregation of a search without hits.
        let buckets = show(
            r#"{"took": 1, "hits": {"total": {"value": 9}, "hits": []},
                "aggregations": {"by_tag": {"doc_count_error_upper_bound": 0, "buckets": [
                    {"key": "a", "doc_count": 5, "avg_n": {"value": 2.5}},
                    {"key": "b", "doc_count": 4, "avg_n": {"value": null}}
                ]}}}"#,
        );
        assert_eq!(names(&buckets), ["key", "doc_count", "avg_n.value"]);
        assert_eq!(
            buckets.rows[0],
            [
                Cell::Text("a".into()),
                Cell::Integer("5".into()),
                Cell::Decimal("2.5".into())
            ]
        );
        // What became of every action of `_bulk`.
        let bulk = show(
            r#"{"took": 5, "errors": true, "items": [
                {"index": {"_index": "a", "_id": "1", "status": 201, "result": "created"}},
                {"create": {"_index": "a", "_id": "1", "status": 409,
                    "error": {"type": "version_conflict_engine_exception", "reason": "[1]: version conflict"}}},
                {"delete": {"_index": "a", "_id": "9", "status": 404, "result": "not_found"}}
            ]}"#,
        );
        assert_eq!(
            names(&bulk),
            ["action", "_index", "_id", "status", "result", "error"]
        );
        assert_eq!(
            bulk.rows[1][5],
            Cell::Text("version_conflict_engine_exception".into())
        );
        assert_eq!(bulk.rows[1][4], Cell::Null);
        assert_eq!(bulk.affected, 2, "the actions without an error");
        // One list under one name.
        let docs = show(r#"{"tokens": [{"token": "quick", "position": 0}]}"#);
        assert_eq!(names(&docs), ["token", "position"]);
        // A list of plain values.
        let plain = show(r#"["a", 1, null]"#);
        assert_eq!(names(&plain), ["value"]);
        assert_eq!(plain.rows.len(), 3);
    }

    #[test]
    fn anything_else_is_listed_value_by_value() {
        let health = show(
            r#"{"cluster_name": "docker-cluster", "status": "green", "timed_out": false, "number_of_nodes": 1,
                "indices": {"count": 3, "shards": {"total": 5}}, "roles": ["data", "ingest"],
                "nodes": [{"name": "n1"}, {"name": "n2"}], "nothing": null, "empty": {}}"#,
        );
        assert_eq!(names(&health), ["key", "value"]);
        let listed: Vec<(String, Cell)> = health
            .rows
            .iter()
            .map(|row| (row[0].display(), row[1].clone()))
            .collect();
        assert_eq!(
            listed,
            [
                (
                    "cluster_name".to_owned(),
                    Cell::Text("docker-cluster".into())
                ),
                ("status".to_owned(), Cell::Text("green".into())),
                ("timed_out".to_owned(), Cell::Bool(false)),
                ("number_of_nodes".to_owned(), Cell::Integer("1".into())),
                ("indices.count".to_owned(), Cell::Integer("3".into())),
                ("indices.shards.total".to_owned(), Cell::Integer("5".into())),
                (
                    "roles".to_owned(),
                    Cell::Json(r#"["data","ingest"]"#.into())
                ),
                ("nodes[0].name".to_owned(), Cell::Text("n1".into())),
                ("nodes[1].name".to_owned(), Cell::Text("n2".into())),
                ("nothing".to_owned(), Cell::Null),
                ("empty".to_owned(), Cell::Json("{}".into())),
            ],
            "in the order the server wrote them"
        );
        // A search that found nothing says so in its figures.
        let nothing = show(r#"{"took": 1, "hits": {"total": {"value": 0}, "hits": []}}"#);
        assert_eq!(names(&nothing), ["key", "value"]);
        assert!(
            nothing
                .rows
                .iter()
                .any(|row| row[0].display() == "hits.total.value")
        );
        // What a write says it did.
        assert_eq!(show(r#"{"_id": "1", "result": "created"}"#).affected, 1);
        assert_eq!(show(r#"{"_id": "1", "result": "noop"}"#).affected, 0);
        assert_eq!(
            show(r#"{"took": 9, "batches": 1, "deleted": 40, "updated": 2, "total": 42}"#).affected,
            42
        );
        assert_eq!(show(r#"{"acknowledged": true}"#).affected, 0);
        // No body: the status. Text: its lines.
        let head = shown(404, b"");
        assert_eq!(
            (names(&head), head.rows[0][0].display()),
            (vec!["status"], "404".to_owned())
        );
        let text = shown(200, b"epoch timestamp cluster\n1 12:00 docker\n");
        assert_eq!(names(&text), ["text"]);
        assert_eq!(text.rows.len(), 2);
        assert_eq!(show("\"pong\"").rows[0][0], Cell::Text("pong".into()));
    }

    #[test]
    fn an_answer_of_the_sql_api_is_its_columns_and_rows() {
        let elastic = show(
            r#"{"columns": [{"name": "n", "type": "long"}, {"name": "title", "type": "text"}],
                "rows": [[1, "a"], [2, null]]}"#,
        );
        assert_eq!(names(&elastic), ["n", "title"]);
        assert_eq!(elastic.rows[1], [Cell::Integer("2".into()), Cell::Null]);
        let opensearch = show(
            r#"{"schema": [{"name": "n", "type": "long"}, {"name": "t", "alias": "title", "type": "text"}],
                "datarows": [[1, "a"]], "total": 1, "size": 1, "status": 200}"#,
        );
        assert_eq!(names(&opensearch), ["n", "title"]);
        // A document that only has such a name is still listed.
        let other = show(r#"{"rows": 3, "note": "x"}"#);
        assert_eq!(names(&other), ["key", "value"]);
    }
}
