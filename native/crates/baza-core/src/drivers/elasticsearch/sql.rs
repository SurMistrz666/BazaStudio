//! The SQL the servers offer: the `_sql` API of Elasticsearch and the SQL
//! plugin of OpenSearch (and of Open Distro before it). They take the same
//! request and answer in two shapes; both give a cursor for the rows that
//! did not fit the first answer.
use super::mapping::plain_cell;
use crate::model::{Cell, ResultColumn};
use serde_json::{Value, json};

/// Rows asked for at once.
pub(super) const FETCH: usize = 1000;

/// Where the SQL of a server is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Api {
    /// Elasticsearch: `_sql`.
    Elastic,
    /// OpenSearch: `_plugins/_sql`.
    Plugins,
    /// Open Distro for Elasticsearch, and OpenSearch 1.x under its old name.
    OpenDistro,
}

impl Api {
    pub(super) fn path(self) -> &'static str {
        match self {
            Api::Elastic => "_sql?format=json",
            Api::Plugins => "_plugins/_sql",
            Api::OpenDistro => "_opendistro/_sql",
        }
    }

    pub(super) fn close_path(self) -> &'static str {
        match self {
            Api::Elastic => "_sql/close",
            Api::Plugins => "_plugins/_sql/close",
            Api::OpenDistro => "_opendistro/_sql/close",
        }
    }

    /// The places to look in, the likeliest first.
    pub(super) fn candidates(opensearch: bool) -> &'static [Api] {
        if opensearch {
            &[Api::Plugins, Api::OpenDistro]
        } else {
            // A cluster of the 7.10 line on Amazon has the plugin instead.
            &[Api::Elastic, Api::OpenDistro, Api::Plugins]
        }
    }
}

/// Whether a statement only reads rows of documents: no grouping, no
/// aggregate, no join.
///
/// The SQL plugin of OpenSearch pages such a statement through a cursor.
/// Anything else it answers whole, and when it is asked for a page of it
/// all the same, an older part of the plugin takes over that calls every
/// column a `double`.
pub(super) fn plain(statement: &str) -> bool {
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    for c in statement.chars().chain(std::iter::once(' ')) {
        match quote {
            Some(open) if c == open => quote = None,
            Some(_) => {}
            None if matches!(c, '\'' | '"' | '`') => quote = Some(c),
            None if c.is_ascii_alphanumeric() || c == '_' => word.push(c.to_ascii_uppercase()),
            None => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
        }
    }
    words.first().is_some_and(|first| first == "SELECT")
        && !words.iter().any(|word| {
            matches!(
                word.as_str(),
                "GROUP"
                    | "HAVING"
                    | "JOIN"
                    | "DISTINCT"
                    | "UNION"
                    | "MINUS"
                    | "COUNT"
                    | "SUM"
                    | "AVG"
                    | "MIN"
                    | "MAX"
                    | "STATS"
                    | "PERCENTILES"
            )
        })
}

/// The first request of a statement. `paged` asks for the rows a fetch at
/// a time, with a cursor for the rest.
pub(super) fn first_body(statement: &str, paged: bool) -> Value {
    let mut body = json!({ "query": statement.trim().trim_end_matches(';').trim_end() });
    if paged {
        body["fetch_size"] = json!(FETCH);
    }
    body
}

/// The request for the rows after a cursor.
pub(super) fn next_body(cursor: &str) -> Value {
    json!({ "cursor": cursor })
}

/// One answer of the SQL API.
#[derive(Debug)]
pub(super) struct Page {
    /// Empty in an answer that continues a cursor.
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Cell>>,
    /// Present while there are more rows.
    pub cursor: Option<String>,
}

/// Read an answer of either server; nothing when it is not one.
pub(super) fn decode(answer: &Value) -> Option<Page> {
    // Elasticsearch: `columns` and `rows`. OpenSearch: `schema` and
    // `datarows`. A continuation has the rows only.
    let (described, rows) = match (answer.get("rows"), answer.get("datarows")) {
        (Some(rows), _) => (answer.get("columns"), rows.as_array()?),
        (None, Some(rows)) => (answer.get("schema"), rows.as_array()?),
        (None, None) => return None,
    };
    let columns: Vec<ResultColumn> = described
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|column| ResultColumn {
            // OpenSearch names what `AS` gave a column apart from its field.
            name: column["alias"]
                .as_str()
                .filter(|alias| !alias.is_empty())
                .or_else(|| column["name"].as_str())
                .unwrap_or_default()
                .to_owned(),
            data_type: column["type"].as_str().unwrap_or_default().to_owned(),
        })
        .collect();
    let width = rows
        .iter()
        .filter_map(|row| row.as_array().map(Vec::len))
        .max()
        .unwrap_or(0)
        .max(columns.len());
    let rows = rows
        .iter()
        .map(|row| {
            let mut cells: Vec<Cell> = row
                .as_array()
                .into_iter()
                .flatten()
                .map(plain_cell)
                .collect();
            cells.resize(width, Cell::Null);
            cells
        })
        .collect();
    Some(Page {
        columns,
        rows,
        cursor: answer["cursor"]
            .as_str()
            .filter(|cursor| !cursor.is_empty())
            .map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_is_the_same_for_both_servers() {
        assert_eq!(
            first_body("  SELECT * FROM \"bazaqa-orders\" ;\n", true),
            json!({ "query": "SELECT * FROM \"bazaqa-orders\"", "fetch_size": 1000 })
        );
        assert_eq!(
            first_body("SHOW TABLES", false),
            json!({ "query": "SHOW TABLES" })
        );
        // Rows of documents are paged; what groups or counts is not.
        for statement in [
            "SELECT * FROM `bazaqa-orders`",
            "select n, title from orders where title = 'count(*) group by' order by n",
            "SELECT \"count\", `sum` FROM orders",
        ] {
            assert!(plain(statement), "{statement}");
        }
        for statement in [
            "SELECT status, COUNT(*) FROM orders GROUP BY status",
            "select max(n) from orders",
            "SELECT DISTINCT status FROM orders",
            "SELECT a.n FROM orders a JOIN clients b ON a.c = b.c",
            "SHOW TABLES LIKE %",
            "DESCRIBE TABLES LIKE orders",
            "DELETE FROM orders WHERE n = 1",
            "",
        ] {
            assert!(!plain(statement), "{statement}");
        }
        assert_eq!(next_body("abc=="), json!({ "cursor": "abc==" }));
        assert_eq!(Api::candidates(true), [Api::Plugins, Api::OpenDistro]);
        assert_eq!(Api::candidates(false)[0], Api::Elastic);
        assert_eq!(Api::Elastic.path(), "_sql?format=json");
        assert_eq!(Api::Plugins.close_path(), "_plugins/_sql/close");
        assert_eq!(Api::OpenDistro.path(), "_opendistro/_sql");
    }

    #[test]
    fn an_answer_of_elasticsearch_is_columns_rows_and_a_cursor() {
        let answer: Value = serde_json::from_str(
            r#"{"columns":[{"name":"bin","type":"binary"},{"name":"ip","type":"ip"},{"name":"n","type":"long"},{"name":"o.b","type":"integer"},{"name":"ok","type":"boolean"},{"name":"price","type":"double"},{"name":"when","type":"datetime"}],
                "rows":[[null,"10.0.0.1",18446744073709551615,1,true,12.50,"2026-10-11T10:00:00.000Z"]],
                "cursor":"3aLBBERG=="}"#,
        )
        .unwrap();
        let page = decode(&answer).unwrap();
        assert_eq!(
            page.columns
                .iter()
                .map(|column| (column.name.as_str(), column.data_type.as_str()))
                .collect::<Vec<_>>(),
            [
                ("bin", "binary"),
                ("ip", "ip"),
                ("n", "long"),
                ("o.b", "integer"),
                ("ok", "boolean"),
                ("price", "double"),
                ("when", "datetime"),
            ]
        );
        assert_eq!(
            page.rows,
            [vec![
                Cell::Null,
                Cell::Text("10.0.0.1".into()),
                Cell::Integer("18446744073709551615".into()),
                Cell::Integer("1".into()),
                Cell::Bool(true),
                Cell::Decimal("12.50".into()),
                Cell::Text("2026-10-11T10:00:00.000Z".into()),
            ]]
        );
        assert_eq!(page.cursor.as_deref(), Some("3aLBBERG=="));
        // The answer to a cursor: rows without columns, and no cursor at
        // the end.
        let more = decode(&json!({ "rows": [[1], [2]] })).unwrap();
        assert!(more.columns.is_empty() && more.cursor.is_none());
        assert_eq!(more.rows.len(), 2);
        assert_eq!(
            decode(&json!({ "rows": [], "cursor": "" })).unwrap().cursor,
            None
        );
    }

    #[test]
    fn an_answer_of_opensearch_is_a_schema_and_datarows() {
        let answer = json!({
            "schema": [
                { "name": "tag", "type": "keyword" },
                { "name": "COUNT(*)", "alias": "ile", "type": "integer" },
                { "name": "nest", "alias": "", "type": "text" },
            ],
            "datarows": [["a", 5, [{ "x": "y" }]], ["b", 4]],
            "total": 2, "size": 2, "status": 200,
            "cursor": "d:eyJwIjoi",
        });
        let page = decode(&answer).unwrap();
        assert_eq!(
            page.columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["tag", "ile", "nest"]
        );
        assert_eq!(
            page.rows,
            [
                vec![
                    Cell::Text("a".into()),
                    Cell::Integer("5".into()),
                    Cell::Json(r#"[{"x":"y"}]"#.into())
                ],
                // A short row is filled up: every row is as wide as the grid.
                vec![
                    Cell::Text("b".into()),
                    Cell::Integer("4".into()),
                    Cell::Null
                ],
            ]
        );
        assert_eq!(page.cursor.as_deref(), Some("d:eyJwIjoi"));
        // What is not an answer of the SQL API is left to the console.
        assert!(decode(&json!({ "took": 1, "hits": { "hits": [] } })).is_none());
        assert!(decode(&json!({ "rows": "many" })).is_none());
        assert!(decode(&json!([1, 2])).is_none());
    }
}
