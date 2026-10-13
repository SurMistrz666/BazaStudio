//! The driver checklist for Elasticsearch and OpenSearch, through the same
//! `Database` the application uses. Ignored by default; a server is
//! described by the variables of the other checklists:
//!
//! ```text
//! BAZA_DB_TEST_PORT=39200 BAZA_DB_TEST_PRODUCT=opensearch \
//! cargo test -p baza-core --test elasticsearch_checklist -- --ignored --nocapture
//! ```
//!
//! Required: `BAZA_DB_TEST_PORT`. Optional: `BAZA_DB_TEST_HOST` (default
//! `127.0.0.1`), `BAZA_DB_TEST_PRODUCT` (default `elasticsearch`),
//! `BAZA_DB_TEST_USER` and `BAZA_DB_TEST_PASSWORD`, `BAZA_DB_TEST_TLS`
//! (`disable`, the default, `require` or `verify_full`),
//! `BAZA_DB_TEST_CA_FILE` and `BAZA_DB_TEST_OPTIONS` (`key=value` pairs
//! separated by `;`). The tests are written for a server of one's own: they
//! make indices named `bazaqa-…` and delete them at the end.
//!
//! How the steps of the SQL checklist map:
//!
//! | SQL checklist | here |
//! | --- | --- |
//! | 1. table designer | off: an index with its mapping is made by `PUT index` in the console |
//! | 2. listing | the index as a table, an alias under the other objects, three overviews as views, hidden indices only when asked for |
//! | 3. columns | `_id` as the key, then the fields of the mapping by their dotted paths and with their types |
//! | 4. import, all or nothing | `_bulk` of `create`: an id that exists, an id given twice and a document the mapping refuses each leave the index as it was |
//! | 5. pages | `search_after` in an order that tells every two documents apart, on an index of three shards; order, a page reached without the pages before it, a page read again |
//! | 6. "contains" filter | by the type of the field: words, a fragment of a keyword, a number or an interval, a period of dates, a block of addresses; and the other six operators |
//! | 7. one cell, conflict check | `_update` of one field with `if_seq_no`, after the field was compared with what the grid showed |
//! | 8. insert, delete | a document with a given id (`create`) and with one of the server; `DELETE` with `if_seq_no` |
//! | 9. query builder | off: SQL of the server and requests of the API in the console |
//! | 10. monitor | the same, and the overviews of indices, nodes and tasks |
//! | 11. stopping a statement | a throttled `_update_by_query` and a slow search, both gone from the tasks of the server |
//!
//! `search_through_an_ssh_tunnel` also needs `BAZA_SSH_TEST_PORT`,
//! `BAZA_SSH_TEST_USER` and `BAZA_SSH_TEST_KEY`.
//! `tls_is_encrypted_and_verified` puts a TLS endpoint of its own in front
//! of a plain server, with a certificate for the name `localhost`
//! (`BAZA_DB_TEST_TLS_CERT`, `BAZA_DB_TEST_TLS_KEY`) and the CA that signed
//! it (`BAZA_DB_TEST_TLS_CA`); against a server that speaks TLS itself it
//! checks the certificate of the server instead.
use baza_core::{
    builder::{Filter, FilterOp},
    database::Database,
    drivers::{PageRequest, RowChange, elasticsearch},
    model::{
        Cell, ConnectionProfile, QueryResult, SshAuth, SshProfile, TableRef, TlsMode, TunnelProfile,
    },
    ssh::TunnelManager,
};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const CONFLICT: &str = "row_changed_reload_required";

fn variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}
fn required(name: &str) -> String {
    variable(name).unwrap_or_else(|| panic!("{name} is required"))
}

fn profile() -> ConnectionProfile {
    let mut profile = ConnectionProfile {
        name: "Real search server".into(),
        engine: elasticsearch::ENGINE,
        product: variable("BAZA_DB_TEST_PRODUCT").unwrap_or_default(),
        host: variable("BAZA_DB_TEST_HOST").unwrap_or_else(|| "127.0.0.1".into()),
        port: required("BAZA_DB_TEST_PORT").parse().unwrap(),
        database: String::new(),
        username: variable("BAZA_DB_TEST_USER").unwrap_or_default(),
        password: variable("BAZA_DB_TEST_PASSWORD").unwrap_or_default(),
        tls: match variable("BAZA_DB_TEST_TLS").as_deref() {
            None | Some("disable") => TlsMode::Disable,
            Some("require") => TlsMode::Require,
            Some("verify_full") => TlsMode::VerifyFull,
            Some(other) => panic!("BAZA_DB_TEST_TLS {other} is not a TLS mode"),
        },
        ca_file: variable("BAZA_DB_TEST_CA_FILE").map(Into::into),
        ..ConnectionProfile::default()
    };
    if profile.product == "elasticsearch" {
        // The generic product of the family is stored without a name.
        profile.product.clear();
    }
    for pair in variable("BAZA_DB_TEST_OPTIONS")
        .unwrap_or_default()
        .split(';')
        .filter(|pair| !pair.is_empty())
    {
        let (key, value) = pair.split_once('=').expect("key=value");
        profile.set_option(key, value);
    }
    profile
}

async fn run(db: &Database, statement: &str) -> QueryResult {
    db.query(statement, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("{error}: {statement}"))
}

async fn failure(db: &Database, statement: &str) -> String {
    db.query(statement, CancellationToken::new())
        .await
        .err()
        .map(|error| error.to_string())
        .unwrap_or_else(|| panic!("no error: {statement}"))
}

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}

fn text(value: &str) -> Cell {
    Cell::Text(value.into())
}

fn filter(column: &str, op: FilterOp, value: &str) -> Filter {
    Filter {
        column: column.into(),
        op,
        value: text(value),
    }
}

fn contains(column: &str, value: &str) -> Filter {
    filter(column, FilterOp::Contains, value)
}

async fn page(db: &Database, request: &PageRequest) -> QueryResult {
    db.read_page(request, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("{error}: {}", db.page_text(request).unwrap()))
}

async fn page_failure(db: &Database, request: &PageRequest) -> String {
    db.read_page(request, CancellationToken::new())
        .await
        .err()
        .map(|error| error.to_string())
        .unwrap_or_else(|| panic!("no error: {}", db.page_text(request).unwrap()))
}

fn at(result: &QueryResult, name: &str) -> usize {
    result
        .columns
        .iter()
        .position(|column| column.name == name)
        .unwrap_or_else(|| panic!("no column {name} in {:?}", result.columns))
}

fn column(result: &QueryResult, name: &str) -> Vec<String> {
    let index = at(result, name);
    result.rows.iter().map(|row| row[index].display()).collect()
}

fn numbers(result: &QueryResult) -> Vec<i64> {
    column(result, "n")
        .iter()
        .map(|n| n.parse().unwrap())
        .collect()
}

fn code(result: anyhow::Result<()>) -> String {
    result
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default()
}

/// The value of a key of an answer that was listed value by value.
fn value_of(result: &QueryResult, key: &str) -> Option<Cell> {
    result
        .rows
        .iter()
        .find(|row| row[0].display() == key)
        .map(|row| row[1].clone())
}

/// A page of a filter, ordered by `n`.
async fn found(db: &Database, table: &TableRef, filters: Vec<Filter>) -> Vec<i64> {
    let request = PageRequest {
        filters,
        order_by: Some(("n".into(), false)),
        ..PageRequest::first(table.clone())
    };
    numbers(&page(db, &request).await)
}

fn key(id: &str) -> Vec<(String, Cell)> {
    vec![("_id".into(), text(id))]
}

/// The name of an index in an SQL statement, in the quotes of the server.
fn quoted(profile: &ConnectionProfile, name: &str) -> String {
    if profile.product.contains("opensearch") {
        format!("`{name}`")
    } else {
        format!("\"{name}\"")
    }
}

/// The index of the checklist: three shards, so that positions in a shard
/// are shared, and a field of every kind the grid treats differently.
async fn create(db: &Database, name: &str) {
    let made = run(
        db,
        &format!(
            r#"PUT {name}
{{
  "settings": {{ "number_of_shards": 3, "number_of_replicas": 0 }},
  "mappings": {{ "properties": {{
    "n": {{ "type": "long" }},
    "label": {{ "type": "text", "fields": {{ "keyword": {{ "type": "keyword", "ignore_above": 256 }} }} }},
    "body": {{ "type": "text" }},
    "kind": {{ "type": "keyword" }},
    "amount": {{ "type": "double" }},
    "day": {{ "type": "date" }},
    "active": {{ "type": "boolean" }},
    "ip": {{ "type": "ip" }},
    "place": {{ "type": "geo_point" }},
    "blob": {{ "type": "binary" }},
    "tags": {{ "type": "keyword" }},
    "o": {{ "properties": {{ "a": {{ "type": "keyword" }}, "b": {{ "type": "integer" }} }} }},
    "items": {{ "type": "nested", "properties": {{ "sku": {{ "type": "keyword" }} }} }}
  }} }}
}}"#
        ),
    )
    .await;
    assert_eq!(value_of(&made, "acknowledged"), Some(Cell::Bool(true)));
}

/// Delete the indices a test made, whatever happened in it.
async fn dropped(db: &Database, names: &[String]) {
    for name in names {
        let _ = db
            .query(&format!("confirm DELETE {name}"), CancellationToken::new())
            .await;
    }
}

fn label(n: i64) -> String {
    match n {
        7 => "Zażółć gęślą jaźń 🦀".into(),
        8 => "it's \"quoted\" \\ back".into(),
        9 => "100% _under_".into(),
        10 => String::new(),
        _ => format!("row {n}"),
    }
}

fn kind(n: i64) -> String {
    if n == 12 {
        "a*b?c".into()
    } else {
        format!("kind {}", n % 4)
    }
}

/// The steps of `driver_checklist.rs` for an index of documents.
async fn checklist(db: &Database, profile: &ConnectionProfile, name: &str) {
    let label_of = profile.product_label();
    let caps = db.capabilities();

    // 1. The table designer. CAPABILITY-LIMITED: an index is a mapping,
    // analysers and settings, not a list of typed columns, so the designer
    // is off; the index is made with `PUT` in the console.
    assert!(!caps.table_designer, "{label_of}: the designer is off");
    create(db, name).await;
    let alias = format!("{name}-alias");
    run(db, &format!("PUT {name}/_alias/{alias}")).await;

    // 2. The side panel: the index once, as a table; its alias among the
    // other objects; the overviews of the cluster as views; no index of
    // the system.
    let tables = db.tables().await.unwrap();
    let listed: Vec<&TableRef> = tables.iter().filter(|found| found.name == name).collect();
    assert_eq!(listed.len(), 1, "{label_of}: listed once");
    let table = listed[0].clone();
    assert_eq!((table.schema.as_str(), table.kind.as_str()), ("", "table"));
    let kind_of = |wanted: &str| {
        tables
            .iter()
            .find(|found| found.name == wanted)
            .map(|found| found.kind.clone())
    };
    assert_eq!(kind_of(&alias).as_deref(), Some("alias"));
    for overview in ["index overview", "cluster nodes", "running tasks"] {
        assert_eq!(kind_of(overview).as_deref(), Some("view"), "{overview}");
    }
    assert!(
        tables.iter().all(|found| !found.name.starts_with('.')),
        "{label_of}: hidden indices are left out"
    );

    // 3. Its columns: `_id` is the key, the fields follow by their paths.
    let columns = db.columns(&table).await.unwrap();
    let described: Vec<(&str, &str)> = columns
        .iter()
        .map(|column| (column.name.as_str(), column.data_type.as_str()))
        .collect();
    assert_eq!(
        described,
        [
            ("_id", "_id"),
            ("active", "boolean"),
            ("amount", "double"),
            ("blob", "binary"),
            ("body", "text"),
            ("day", "date"),
            ("ip", "ip"),
            ("items", "nested"),
            ("kind", "keyword"),
            ("label", "text"),
            ("n", "long"),
            ("o.a", "keyword"),
            ("o.b", "integer"),
            ("place", "geo_point"),
            ("tags", "keyword"),
        ],
        "{label_of}: columns"
    );
    assert!(columns[0].primary_key && !columns[0].nullable);
    assert!(
        columns[1..]
            .iter()
            .all(|column| !column.primary_key && column.nullable)
    );
    let nothing = page(db, &PageRequest::first(table.clone())).await;
    assert_eq!((nothing.rows.len(), nothing.columns.len()), (0, 15));
    // An alias reads the same documents and takes no change.
    let through = TableRef {
        schema: String::new(),
        name: alias.clone(),
        kind: "alias".into(),
    };
    assert!(
        db.columns(&through)
            .await
            .unwrap()
            .iter()
            .all(|column| !column.primary_key),
        "{label_of}: an alias has no key to edit by"
    );

    // 4. Import: 120 documents at once, then three files that must each
    // leave the index as it was.
    assert!(caps.import, "{label_of}: import");
    let names = [
        "_id", "n", "label", "body", "kind", "amount", "day", "active", "ip", "o.a", "o.b",
    ]
    .map(str::to_owned);
    let row = |n: i64| {
        vec![
            text(&format!("doc-{n}")),
            Cell::Integer(n.to_string()),
            text(&label(n)),
            text(&label(n)),
            text(&kind(n)),
            if n == 11 {
                Cell::Null
            } else {
                Cell::Decimal(format!("{n}.50"))
            },
            text(&format!(
                "2026-{:02}-{:02}T08:00:00Z",
                1 + (n - 1) / 28,
                1 + (n - 1) % 28
            )),
            Cell::Bool(n % 2 == 0),
            text(&format!("10.0.{}.{}", n / 100, n % 100)),
            text("x"),
            Cell::Integer(n.to_string()),
        ]
    };
    let rows: Vec<Vec<Cell>> = (1..=120).map(row).collect();
    let imported = db
        .import_rows(&table, &names, &rows, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(imported.affected, 120, "{label_of}: imported documents");
    let count = |db: Database, table: TableRef| async move {
        let mut total = 0;
        let mut request = PageRequest::first(table);
        loop {
            let read = page(&db, &request).await.rows.len();
            total += read;
            if read < 500 {
                return total;
            }
            request.offset += 500;
        }
    };
    assert_eq!(count(db.clone(), table.clone()).await, 120);
    // An id that exists: `create` never overwrites, and nothing is added.
    let mut clash = vec![row(500), row(5)];
    clash[1][2] = text("overwritten?");
    assert_eq!(
        db.import_rows(&table, &names, &clash, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string(),
        "elasticsearch_import_id_exists",
        "{label_of}: an id that exists fails the import"
    );
    assert_eq!(
        db.import_rows(
            &table,
            &names,
            &[row(600), row(600)],
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .to_string(),
        "elasticsearch_import_duplicate"
    );
    // A document the mapping refuses, after 1200 that it takes: the server
    // says which one, and the ones that were written are removed again.
    let mut refused: Vec<Vec<Cell>> = (1000..2300).map(row).collect();
    for document in &mut refused {
        document[6] = text("2026-06-01T08:00:00Z");
    }
    refused[1234][6] = text("not a date");
    assert_eq!(
        db.import_rows(&table, &names, &refused, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string(),
        "elasticsearch_import_value_rejected_at_row_1235",
        "{label_of}: a refused document fails the import"
    );
    // A value that cannot be written is found before anything is sent.
    let mut unreadable = vec![row(700), row(701)];
    unreadable[1][1] = text("many");
    assert_eq!(
        db.import_rows(&table, &names, &unreadable, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string(),
        "elasticsearch_value_invalid"
    );
    assert_eq!(
        count(db.clone(), table.clone()).await,
        120,
        "{label_of}: every failed import left the index as it was"
    );
    assert_eq!(
        column(
            &page(
                db,
                &PageRequest {
                    filters: vec![filter("_id", FilterOp::Equal, "doc-5")],
                    ..PageRequest::first(table.clone())
                }
            )
            .await,
            "label"
        ),
        ["row 5"],
        "{label_of}: nothing was overwritten"
    );
    // Without an id the server makes one.
    let anonymous = ["n", "label", "kind"].map(str::to_owned);
    let made = db
        .import_rows(
            &table,
            &anonymous,
            &[vec![
                Cell::Integer("9001".into()),
                text("no id"),
                text("generated"),
            ]],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(made.affected, 1);
    let generated = page(
        db,
        &PageRequest {
            filters: vec![filter("kind", FilterOp::Equal, "generated")],
            ..PageRequest::first(table.clone())
        },
    )
    .await;
    assert_eq!(numbers(&generated), [9001]);
    let generated_id = column(&generated, "_id")[0].clone();
    assert!(generated_id.len() >= 16, "{generated_id}");
    db.apply(&RowChange::DeleteRow {
        table: table.clone(),
        key: key(&generated_id),
    })
    .await
    .unwrap();
    // An alias takes no import.
    assert_eq!(
        db.import_rows(&through, &names, &[row(800)], CancellationToken::new())
            .await
            .unwrap_err()
            .to_string(),
        "elasticsearch_read_only_object"
    );

    // 5. Pages. Without an order every document is read exactly once,
    // whatever the size of a page: on three shards the same position is
    // held by up to three documents, which a plain `search_after` skips.
    assert!(caps.sort && caps.filter, "{label_of}: sort and filter");
    for limit in [7_u32, 50] {
        let mut request = PageRequest::first(table.clone());
        request.limit = limit;
        let mut seen = BTreeSet::new();
        let mut read = 0;
        loop {
            let rows = numbers(&page(db, &request).await);
            read += rows.len();
            seen.extend(rows.iter().copied());
            if rows.len() < limit as usize {
                break;
            }
            request.offset += u64::from(limit);
        }
        assert_eq!(
            (
                read,
                seen.len(),
                seen.first().copied(),
                seen.last().copied()
            ),
            (120, 120, Some(1), Some(120)),
            "{label_of}: pages of {limit} read every document once"
        );
    }
    let mut request = PageRequest::first(table.clone());
    request.order_by = Some(("n".into(), false));
    request.limit = 50;
    assert_eq!(
        numbers(&page(db, &request).await),
        (1..=50).collect::<Vec<_>>()
    );
    request.offset = 50;
    assert_eq!(
        numbers(&page(db, &request).await),
        (51..=100).collect::<Vec<_>>()
    );
    // The same page again is read again, not answered from what was kept.
    assert_eq!(
        numbers(&page(db, &request).await),
        (51..=100).collect::<Vec<_>>()
    );
    request.offset = 100;
    assert_eq!(
        numbers(&page(db, &request).await),
        (101..=120).collect::<Vec<_>>()
    );
    request.offset = 150;
    assert!(
        page(db, &request).await.rows.is_empty(),
        "{label_of}: past the end"
    );
    // A page nobody visited is reached by passing the documents before it.
    let mut jump = PageRequest::first(table.clone());
    jump.order_by = Some(("n".into(), true));
    (jump.limit, jump.offset) = (10, 95);
    assert_eq!(
        numbers(&page(db, &jump).await),
        (16..=25).rev().collect::<Vec<_>>(),
        "{label_of}: a page reached without the pages before it"
    );
    // And back: a page before the last one that was read.
    jump.offset = 5;
    assert_eq!(
        numbers(&page(db, &jump).await),
        (106..=115).rev().collect::<Vec<_>>()
    );
    // Other orders: a date, an analysed text by its exact sub-field, a
    // number with a missing value, which comes last in either direction.
    let ordered = |column: &str, descending: bool| PageRequest {
        order_by: Some((column.to_owned(), descending)),
        limit: 3,
        ..PageRequest::first(table.clone())
    };
    assert_eq!(
        numbers(&page(db, &ordered("day", true)).await),
        [120, 119, 118]
    );
    assert_eq!(
        numbers(&page(db, &ordered("label", false)).await),
        [10, 9, 7]
    );
    assert_eq!(
        numbers(&page(db, &ordered("amount", true)).await),
        [120, 119, 118]
    );
    let mut last = ordered("amount", false);
    (last.limit, last.offset) = (5, 118);
    assert_eq!(numbers(&page(db, &last).await), [120, 11]);
    let mut last = ordered("amount", true);
    (last.limit, last.offset) = (5, 118);
    assert_eq!(numbers(&page(db, &last).await), [1, 11]);
    for unordered in ["body", "place", "_id", "items"] {
        assert_eq!(
            page_failure(db, &ordered(unordered, false)).await,
            "elasticsearch_sort_unsupported",
            "{label_of}: {unordered}"
        );
    }
    // The text of a page is a request the console runs.
    let shown = db.page_text(&request).unwrap();
    assert!(shown.contains(&format!("GET {name}/_search")), "{shown}");
    let first_text = db
        .page_text(&PageRequest {
            limit: 3,
            order_by: Some(("n".into(), false)),
            ..PageRequest::first(table.clone())
        })
        .unwrap();
    let rerun = run(db, &first_text).await;
    assert_eq!(numbers(&rerun), [1, 2, 3]);
    assert_eq!(column(&rerun, "_id"), ["doc-1", "doc-2", "doc-3"]);
    // The alias reads the same documents.
    let mut by_alias = PageRequest::first(through.clone());
    by_alias.order_by = Some(("n".into(), false));
    by_alias.limit = 2;
    assert_eq!(numbers(&page(db, &by_alias).await), [1, 2]);

    // 6. The filter. "Contains" is what the type of the field can answer.
    // Analysed text: its words, the last one as a beginning; with an exact
    // sub-field also a fragment anywhere, whatever the letter case.
    let eleven: Vec<i64> = std::iter::once(11).chain(110..=119).collect();
    assert_eq!(
        found(db, &table, vec![contains("label", "ROW 11")]).await,
        eleven
    );
    assert_eq!(
        found(db, &table, vec![contains("body", "ROW 11")]).await,
        eleven
    );
    assert_eq!(
        found(db, &table, vec![contains("label", "AŻÓŁ")]).await,
        [7]
    );
    assert_eq!(
        found(db, &table, vec![contains("body", "ażół")]).await,
        Vec::<i64>::new(),
        "{label_of}: a text without an exact form is searched by its words only"
    );
    assert_eq!(found(db, &table, vec![contains("body", "zaż")]).await, [7]);
    assert_eq!(
        found(db, &table, vec![contains("label", "\"quoted\" \\")]).await,
        [8]
    );
    // The words of an analysed text hold no punctuation: "100%" is the
    // word 100, which "row 100" has as well. The fragment with its signs
    // is found in the exact form only.
    assert_eq!(
        found(db, &table, vec![contains("label", "100% _")]).await,
        [9, 100]
    );
    assert_eq!(
        found(db, &table, vec![contains("label", "0% _UNDER")]).await,
        [9]
    );
    // A keyword: the fragment anywhere; `*` and `?` are characters.
    assert_eq!(
        found(db, &table, vec![contains("kind", "ND 3")])
            .await
            .len(),
        30
    );
    assert_eq!(found(db, &table, vec![contains("kind", "*b?")]).await, [12]);
    assert_eq!(
        found(db, &table, vec![contains("o.a", "X")]).await.len(),
        120
    );
    // Numbers: a value, an interval, a comparison.
    assert_eq!(found(db, &table, vec![contains("n", "42")]).await, [42]);
    assert_eq!(
        found(db, &table, vec![contains("n", "10..12")]).await,
        [10, 11, 12]
    );
    assert_eq!(
        found(db, &table, vec![contains("n", ">= 118")]).await,
        [118, 119, 120]
    );
    assert_eq!(
        found(db, &table, vec![contains("amount", "5.5")]).await,
        [5]
    );
    assert_eq!(
        found(db, &table, vec![contains("o.b", "..2")]).await,
        [1, 2]
    );
    // Dates: one cut short stands for its whole period.
    assert_eq!(
        found(db, &table, vec![contains("day", "2026-02")]).await,
        (29..=56).collect::<Vec<_>>()
    );
    assert_eq!(
        found(db, &table, vec![contains("day", "2026-02-03")]).await,
        [31]
    );
    assert_eq!(
        found(db, &table, vec![contains("day", "2026-01-27..2026-02-02")]).await,
        [27, 28, 29, 30]
    );
    assert_eq!(
        found(db, &table, vec![contains("day", "2026")]).await.len(),
        120
    );
    assert_eq!(
        found(db, &table, vec![contains("day", "< 2026-01-03")]).await,
        [1, 2]
    );
    assert_eq!(
        found(db, &table, vec![contains("active", "true")])
            .await
            .len(),
        60
    );
    assert_eq!(
        found(db, &table, vec![contains("ip", "10.0.1.0/24")]).await,
        (100..=120).collect::<Vec<_>>()
    );
    assert_eq!(
        found(db, &table, vec![contains("ip", "10.0.0.7")]).await,
        [7]
    );
    assert_eq!(
        found(db, &table, vec![contains("_id", "doc-33")]).await,
        [33]
    );
    // The other operators.
    assert_eq!(
        found(db, &table, vec![filter("kind", FilterOp::Equal, "kind 1")])
            .await
            .len(),
        30
    );
    assert_eq!(
        found(db, &table, vec![filter("label", FilterOp::Equal, "row 20")]).await,
        [20]
    );
    assert_eq!(
        found(db, &table, vec![filter("label", FilterOp::Equal, "ROW 20")]).await,
        Vec::<i64>::new(),
        "{label_of}: equal compares the exact value"
    );
    assert_eq!(
        found(db, &table, vec![filter("body", FilterOp::Equal, "Row 20")]).await,
        [20]
    );
    assert_eq!(
        found(db, &table, vec![filter("n", FilterOp::Equal, "5")]).await,
        [5]
    );
    assert_eq!(
        found(
            db,
            &table,
            vec![filter("day", FilterOp::Equal, "2026-01-05T08:00:00Z")]
        )
        .await,
        [5]
    );
    assert_eq!(
        found(
            db,
            &table,
            vec![filter("kind", FilterOp::NotEqual, "kind 1")]
        )
        .await
        .len(),
        90
    );
    assert_eq!(
        found(db, &table, vec![filter("n", FilterOp::Greater, "115")]).await,
        [116, 117, 118, 119, 120]
    );
    assert_eq!(
        found(db, &table, vec![filter("n", FilterOp::Less, "4")]).await,
        [1, 2, 3]
    );
    assert_eq!(
        found(
            db,
            &table,
            vec![filter("kind", FilterOp::Greater, "kind 2")]
        )
        .await
        .len(),
        30
    );
    assert_eq!(
        found(
            db,
            &table,
            vec![filter("day", FilterOp::Less, "2026-01-03")]
        )
        .await,
        [1, 2]
    );
    assert_eq!(
        found(db, &table, vec![filter("amount", FilterOp::IsNull, "")]).await,
        [11]
    );
    assert_eq!(
        found(db, &table, vec![filter("amount", FilterOp::IsNotNull, "")])
            .await
            .len(),
        119
    );
    assert_eq!(
        found(db, &table, vec![filter("place", FilterOp::IsNotNull, "")])
            .await
            .len(),
        0
    );
    // All of several filters hold.
    assert_eq!(
        found(
            db,
            &table,
            vec![
                contains("kind", "kind 1"),
                contains("n", "..20"),
                filter("active", FilterOp::Equal, "false"),
            ]
        )
        .await,
        [1, 5, 9, 13, 17]
    );
    // What a type cannot answer is said in words, before anything is sent.
    for (wrong, expected) in [
        (contains("n", "abc"), "elasticsearch_filter_number"),
        (contains("day", "yesterday"), "elasticsearch_filter_date"),
        (contains("active", "yes"), "elasticsearch_filter_boolean"),
        (contains("ip", "10.0."), "elasticsearch_filter_ip"),
        (contains("place", "52"), "elasticsearch_filter_unsupported"),
        (
            filter("body", FilterOp::Greater, "m"),
            "elasticsearch_filter_unsupported",
        ),
        (contains("nothing", "x"), "column_not_found"),
    ] {
        let request = PageRequest {
            filters: vec![wrong],
            ..PageRequest::first(table.clone())
        };
        assert_eq!(page_failure(db, &request).await, expected, "{label_of}");
    }
    // The filter is part of the text of the page.
    let filtered_text = db
        .page_text(&PageRequest {
            filters: vec![contains("label", "ROW 11")],
            ..PageRequest::first(table.clone())
        })
        .unwrap();
    assert!(filtered_text.contains("match_phrase_prefix") && filtered_text.contains("regexp"));
    assert_eq!(run(db, &filtered_text).await.rows.len(), 11);

    // 7. One field, with the conflict check.
    assert!(caps.edit_cells, "{label_of}: edit");
    let untouched = page(
        db,
        &PageRequest {
            filters: vec![filter("_id", FilterOp::Equal, "doc-20")],
            ..PageRequest::first(table.clone())
        },
    )
    .await;
    assert_eq!(
        untouched.rows[0][at(&untouched, "amount")],
        Cell::Decimal("20.50".into()),
        "{label_of}: every digit of the document as it was written"
    );
    let edit = RowChange::UpdateCell {
        table: table.clone(),
        column: "label".into(),
        old: text("row 20"),
        new: text("edited"),
        key: key("doc-20"),
    };
    let edit_text = db.change_text(&edit).unwrap();
    assert!(
        edit_text.contains(&format!("POST {name}/_update/doc-20"))
            && edit_text.contains("\"label\": \"edited\""),
        "{edit_text}"
    );
    db.apply(&edit).await.unwrap();
    assert_eq!(
        found(db, &table, vec![filter("label", FilterOp::Equal, "edited")]).await,
        [20]
    );
    // The grid still shows the old value: nothing is written.
    assert_eq!(
        code(db.apply(&edit).await),
        CONFLICT,
        "{label_of}: conflict"
    );
    // The other fields of the document are untouched.
    let twenty = page(
        db,
        &PageRequest {
            filters: vec![filter("_id", FilterOp::Equal, "doc-20")],
            ..PageRequest::first(table.clone())
        },
    )
    .await;
    assert_eq!(twenty.rows.len(), 1);
    let cell = |result: &QueryResult, name: &str| result.rows[0][at(result, name)].clone();
    assert_eq!(cell(&twenty, "body"), text("row 20"));
    // The server wrote the document anew when it merged the field in, and
    // writes its numbers in its own way: 20.50 is 20.5 from now on.
    assert_eq!(cell(&twenty, "amount"), Cell::Decimal("20.5".into()));
    assert_eq!(cell(&twenty, "active"), Cell::Bool(true));
    assert_eq!(cell(&twenty, "o.a"), text("x"));
    assert_eq!(cell(&twenty, "o.b"), Cell::Integer("20".into()));
    assert_eq!(cell(&twenty, "place"), Cell::Null);
    let change = |column: &str, old: Cell, new: Cell| RowChange::UpdateCell {
        table: table.clone(),
        column: column.into(),
        old,
        new,
        key: key("doc-20"),
    };
    // A number, a field inside an object, a boolean, nothing, and back.
    db.apply(&change(
        "amount",
        cell(&twenty, "amount"),
        Cell::Decimal("99.25".into()),
    ))
    .await
    .unwrap();
    db.apply(&change(
        "o.b",
        Cell::Integer("20".into()),
        Cell::Integer("21".into()),
    ))
    .await
    .unwrap();
    db.apply(&change("active", Cell::Bool(true), Cell::Bool(false)))
        .await
        .unwrap();
    db.apply(&change("kind", text("kind 0"), Cell::Null))
        .await
        .unwrap();
    // A field without a value takes one: a text for a number is read as
    // the number, a list and an object are typed as JSON, a geo point as
    // the text the server reads.
    db.apply(&change("kind", Cell::Null, text("back")))
        .await
        .unwrap();
    db.apply(&change(
        "tags",
        Cell::Null,
        Cell::Json(r#"["a","b"]"#.into()),
    ))
    .await
    .unwrap();
    db.apply(&change(
        "tags",
        Cell::Json(r#"[ "a", "b" ]"#.into()),
        Cell::Json(r#"["c"]"#.into()),
    ))
    .await
    .unwrap();
    db.apply(&change("place", Cell::Null, text("52.23,21.01")))
        .await
        .unwrap();
    db.apply(&change(
        "items",
        Cell::Null,
        text(r#"[{"sku": "a-1"}, {"sku": "b-2"}]"#),
    ))
    .await
    .unwrap();
    db.apply(&change("blob", Cell::Null, text("AAEC/w==")))
        .await
        .unwrap();
    let twenty = page(
        db,
        &PageRequest {
            filters: vec![filter("_id", FilterOp::Equal, "doc-20")],
            ..PageRequest::first(table.clone())
        },
    )
    .await;
    assert_eq!(cell(&twenty, "amount"), Cell::Decimal("99.25".into()));
    assert_eq!(cell(&twenty, "o.b"), Cell::Integer("21".into()));
    assert_eq!(
        cell(&twenty, "o.a"),
        text("x"),
        "{label_of}: the rest of the object stays"
    );
    assert_eq!(cell(&twenty, "active"), Cell::Bool(false));
    assert_eq!(cell(&twenty, "kind"), text("back"));
    assert_eq!(cell(&twenty, "tags"), Cell::Json(r#"["c"]"#.into()));
    assert_eq!(cell(&twenty, "place"), text("52.23,21.01"));
    assert_eq!(
        cell(&twenty, "items"),
        Cell::Json(r#"[{"sku":"a-1"},{"sku":"b-2"}]"#.into())
    );
    assert_eq!(cell(&twenty, "blob"), Cell::Bytes(vec![0, 1, 2, 255]));
    assert_eq!(
        found(db, &table, vec![filter("place", FilterOp::IsNotNull, "")]).await,
        [20]
    );
    assert_eq!(
        found(db, &table, vec![filter("items", FilterOp::IsNotNull, "")]).await,
        [20]
    );
    // A change made elsewhere since the grid read the document.
    run(
        db,
        &format!(
            "POST {name}/_update/doc-30?refresh=true\n{{\"doc\": {{\"label\": \"outside\"}}}}"
        ),
    )
    .await;
    let stale = RowChange::UpdateCell {
        table: table.clone(),
        column: "label".into(),
        old: text("row 30"),
        new: text("mine"),
        key: key("doc-30"),
    };
    assert_eq!(code(db.apply(&stale).await), CONFLICT);
    assert_eq!(
        found(
            db,
            &table,
            vec![filter("label", FilterOp::Equal, "outside")]
        )
        .await,
        [30]
    );
    // What the grid refuses to write, and what the server refuses to take.
    assert_eq!(
        code(
            db.apply(&change("_id", text("doc-20"), text("doc-x")))
                .await
        ),
        "elasticsearch_id_read_only"
    );
    assert_eq!(
        code(
            db.apply(&change("n", Cell::Integer("20".into()), text("twenty")))
                .await
        ),
        "elasticsearch_value_invalid"
    );
    assert_eq!(
        code(
            db.apply(&change(
                "day",
                text("2026-01-20T08:00:00Z"),
                text("not a date")
            ))
            .await
        ),
        "elasticsearch_value_rejected"
    );
    assert_eq!(
        code(
            db.apply(&RowChange::UpdateCell {
                table: table.clone(),
                column: "label".into(),
                old: text("x"),
                new: text("y"),
                key: key("doc-nowhere"),
            })
            .await
        ),
        CONFLICT
    );
    assert_eq!(
        code(
            db.apply(&RowChange::UpdateCell {
                table: through.clone(),
                column: "label".into(),
                old: text("row 40"),
                new: text("y"),
                key: key("doc-40"),
            })
            .await
        ),
        "elasticsearch_read_only_object"
    );

    // 8. A new document and a removed one.
    assert!(
        caps.insert_rows && caps.delete_rows,
        "{label_of}: insert and delete"
    );
    let insert = RowChange::InsertRow {
        table: table.clone(),
        values: vec![
            ("_id".into(), text("new/1 ?&#")),
            ("n".into(), Cell::Integer("1001".into())),
            ("label".into(), text("it's new")),
            ("o.b".into(), Cell::Integer("7".into())),
            // The dialog of a new row calls a geo point a number.
            ("place".into(), Cell::Integer("54.35,18.65".into())),
            ("amount".into(), Cell::Null),
        ],
    };
    let insert_text = db.change_text(&insert).unwrap();
    assert!(
        insert_text.starts_with(&format!(
            "PUT {name}/_doc/new%2F1%20%3F%26%23?op_type=create"
        )),
        "{insert_text}"
    );
    db.apply(&insert).await.unwrap();
    let new = page(
        db,
        &PageRequest {
            filters: vec![filter("n", FilterOp::Equal, "1001")],
            ..PageRequest::first(table.clone())
        },
    )
    .await;
    assert_eq!(column(&new, "_id"), ["new/1 ?&#"]);
    assert_eq!(cell(&new, "label"), text("it's new"));
    assert_eq!(cell(&new, "o.b"), Cell::Integer("7".into()));
    assert_eq!(cell(&new, "place"), text("54.35,18.65"));
    assert_eq!(cell(&new, "amount"), Cell::Null);
    // The same id again is refused, not overwritten.
    assert_eq!(
        code(db.apply(&insert).await),
        "elasticsearch_document_exists"
    );
    // An id with every awkward character is still one document to edit.
    db.apply(&RowChange::UpdateCell {
        table: table.clone(),
        column: "label".into(),
        old: text("it's new"),
        new: text("renamed"),
        key: key("new/1 ?&#"),
    })
    .await
    .unwrap();
    // Without an id the server makes one.
    db.apply(&RowChange::InsertRow {
        table: table.clone(),
        values: vec![("n".into(), Cell::Integer("1002".into()))],
    })
    .await
    .unwrap();
    let made = page(
        db,
        &PageRequest {
            filters: vec![filter("n", FilterOp::Equal, "1002")],
            ..PageRequest::first(table.clone())
        },
    )
    .await;
    assert_eq!(made.rows.len(), 1);
    let made_id = column(&made, "_id")[0].clone();
    assert_eq!(
        code(
            db.apply(&RowChange::InsertRow {
                table: table.clone(),
                values: vec![("day".into(), text("not a date"))],
            })
            .await
        ),
        "elasticsearch_value_rejected"
    );
    assert_eq!(count(db.clone(), table.clone()).await, 122);
    for id in ["new/1 ?&#", made_id.as_str()] {
        let delete = RowChange::DeleteRow {
            table: table.clone(),
            key: key(id),
        };
        assert!(db.change_text(&delete).unwrap().contains("DELETE "));
        db.apply(&delete).await.unwrap();
        assert_eq!(
            code(db.apply(&delete).await),
            CONFLICT,
            "{label_of}: deleted twice"
        );
    }
    assert_eq!(count(db.clone(), table.clone()).await, 120);

    // 9. The query builder. CAPABILITY-LIMITED: it is off; SQL of the
    // server and requests of the API are typed in the console.
    assert!(!caps.query_builder, "{label_of}: the builder is off");
    let from = quoted(profile, name);
    let grouped = run(
        db,
        &format!(
            "SELECT kind, COUNT(*) AS c FROM {from} WHERE n > 100 GROUP BY kind ORDER BY kind;"
        ),
    )
    .await;
    assert_eq!(
        grouped
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["kind", "c"]
    );
    assert_eq!(
        column(&grouped, "kind"),
        ["kind 0", "kind 1", "kind 2", "kind 3"]
    );
    assert_eq!(column(&grouped, "c"), ["5", "5", "5", "5"]);
    assert!(matches!(grouped.rows[0][1], Cell::Integer(_)));
    // The types of the columns are the ones of the server.
    assert_eq!(grouped.columns[0].data_type, "keyword", "{label_of}");
    assert!(
        matches!(grouped.columns[1].data_type.as_str(), "long" | "integer"),
        "{label_of}: {}",
        grouped.columns[1].data_type
    );
    let selected = run(
        db,
        &format!("select n, amount, active, day\n  from {from}\n  where n < 4\n  order by n"),
    )
    .await;
    assert_eq!(numbers(&selected), [1, 2, 3]);
    assert_eq!(selected.rows[0][1], Cell::Decimal("1.5".into()));
    assert_eq!(selected.rows[0][2], Cell::Bool(false));
    assert!(selected.rows[0][3].display().starts_with("2026-01-01"));
    assert_eq!(
        failure(db, &format!("SELECT nothing_like_this FROM {from} WHERE")).await,
        "elasticsearch_sql_invalid"
    );
    let missing = failure(db, "SELECT * FROM bazaqa_missing_index_xyz").await;
    assert!(
        matches!(
            missing.as_str(),
            "elasticsearch_sql_invalid" | "elasticsearch_index_unknown"
        ),
        "{missing}"
    );
    // Requests of the API, as Dev Tools takes them: hits are rows.
    let hits = run(
        db,
        &format!(
            "GET {name}/_search\n{{\n  \"query\": {{ \"term\": {{ \"kind\": \"kind 1\" }} }},  // exact\n  \"sort\": [\"n\"],\n  \"size\": 4\n}}"
        ),
    )
    .await;
    assert_eq!(numbers(&hits), [1, 5, 9, 13]);
    assert_eq!(column(&hits, "_id"), ["doc-1", "doc-5", "doc-9", "doc-13"]);
    assert_eq!(column(&hits, "o.b"), ["1", "5", "9", "13"]);
    // The buckets of an aggregation are rows as well.
    let buckets = run(
        db,
        &format!(
            "POST {name}/_search\n{{\"size\": 0, \"query\": {{\"range\": {{\"n\": {{\"lte\": 40}}}}}}, \"aggs\": {{\"kinds\": {{\"terms\": {{\"field\": \"kind\", \"order\": {{\"_key\": \"asc\"}}}}, \"aggs\": {{\"top\": {{\"max\": {{\"field\": \"n\"}}}}}}}}}}}}"
        ),
    )
    .await;
    assert_eq!(
        column(&buckets, "key"),
        ["a*b?c", "back", "kind 0", "kind 1", "kind 2", "kind 3"]
    );
    assert_eq!(
        column(&buckets, "doc_count"),
        ["1", "1", "8", "10", "10", "10"]
    );
    assert!(column(&buckets, "top.value")[3].starts_with("37"));
    // Anything else is listed value by value; a `_cat` table is a table.
    let health = run(db, "GET _cluster/health").await;
    assert!(
        value_of(&health, "status").is_some() && value_of(&health, "number_of_nodes").is_some()
    );
    let cat = run(
        db,
        &format!("GET _cat/indices/{name}?h=index,docs.count,pri"),
    )
    .await;
    assert_eq!(column(&cat, "index"), [name]);
    assert_eq!(column(&cat, "pri"), ["3"]);
    // The columns of a table, and the values of a document, stand in the
    // order the server wrote them.
    assert_eq!(
        cat.columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["index", "docs.count", "pri"]
    );
    assert_eq!(health.rows[0][0], text("cluster_name"));
    assert_eq!(health.rows[1][0], text("status"));
    let head = run(db, &format!("HEAD {name}")).await;
    assert_eq!(column(&head, "status"), ["200"]);
    assert_eq!(
        column(&run(db, &format!("HEAD {name}-nothing")).await, "status"),
        ["404"]
    );
    let document = run(db, &format!("GET {name}/_doc/doc-5")).await;
    assert_eq!(value_of(&document, "_source.label"), Some(text("row 5")));
    assert_eq!(value_of(&document, "found"), Some(Cell::Bool(true)));
    // A document that is not there is an answer, not a failure.
    let absent = run(db, &format!("GET {name}/_doc/doc-nowhere")).await;
    assert_eq!(value_of(&absent, "found"), Some(Cell::Bool(false)));
    let root = run(db, "GET /").await;
    assert!(value_of(&root, "version.number").is_some());
    // A script: every request is run, the last answer is shown, and what
    // the writes did is counted.
    let script = run(
        db,
        &format!(
            "# two documents and a look\nPOST _bulk?refresh=true\n{{\"index\": {{\"_index\": \"{name}\", \"_id\": \"bulk-1\"}}}}\n{{\"n\": 2001, \"kind\": \"bulk\",\n  \"label\": \"\"\"multi\nline \"text\" \"\"\"}}\n{{\"create\": {{\"_index\": \"{name}\", \"_id\": \"bulk-2\"}}}}\n{{\"n\": 2002, \"kind\": \"bulk\"}}\n\nGET {name}/_count?q=kind:bulk\n"
        ),
    )
    .await;
    assert_eq!(value_of(&script, "count"), Some(Cell::Integer("2".into())));
    assert_eq!(script.affected, 2);
    let bulk = found(db, &table, vec![filter("kind", FilterOp::Equal, "bulk")]).await;
    assert_eq!(
        bulk,
        [2001, 2002],
        "{label_of}: a page after a write of the console"
    );
    let multi = page(
        db,
        &PageRequest {
            filters: vec![filter("_id", FilterOp::Equal, "bulk-1")],
            ..PageRequest::first(table.clone())
        },
    )
    .await;
    assert_eq!(cell(&multi, "label"), text("multi\nline \"text\" "));
    // What every action of a `_bulk` became.
    let items = run(
        db,
        &format!(
            "POST {name}/_bulk?refresh=true\n{{\"create\": {{\"_id\": \"bulk-1\"}}}}\n{{\"n\": 1}}\n{{\"delete\": {{\"_id\": \"bulk-2\"}}}}\n"
        ),
    )
    .await;
    assert_eq!(column(&items, "action"), ["create", "delete"]);
    assert_eq!(column(&items, "status"), ["409", "200"]);
    assert_eq!(
        column(&items, "error"),
        ["version_conflict_engine_exception", "NULL"]
    );
    assert_eq!(items.affected, 1);
    // What is refused, in words and with its line.
    assert_eq!(
        failure(db, "GET bazaqa-missing-index-xyz/_search").await,
        "elasticsearch_index_unknown"
    );
    assert_eq!(
        failure(
            db,
            &format!("GET {name}/_search\n{{\"query\": {{\"nonsense\": {{}}}}}}")
        )
        .await,
        "elasticsearch_request_invalid"
    );
    assert_eq!(
        failure(
            db,
            &format!("GET _cat/health\n\nGET {name}/_search\n{{\"query\": }}")
        )
        .await,
        "elasticsearch_console_json_at_line_4"
    );
    assert_eq!(
        failure(db, "FETCH everything").await,
        "elasticsearch_console_syntax_at_line_1"
    );
    // In a script, a refusal names the request it is about.
    assert_eq!(
        failure(
            db,
            &format!("GET _cat/health\nGET bazaqa-missing-index-xyz/_count\nGET {name}/_count")
        )
        .await,
        "elasticsearch_index_unknown_at_line_2"
    );
    // Removing everything needs the word `confirm`, and a script that
    // lacks it runs nothing at all.
    for (text, line) in [
        (format!("DELETE {name}"), 1),
        (
            format!("POST {name}/_delete_by_query\n{{\"query\": {{\"match_all\": {{}}}}}}"),
            1,
        ),
        (
            format!(
                "PUT {name}/_doc/never?refresh=true\n{{\"n\": 3001}}\nPOST {name}/_delete_by_query"
            ),
            3,
        ),
    ] {
        assert_eq!(
            failure(db, &text).await,
            format!("elasticsearch_confirmation_required_at_line_{line}"),
            "{label_of}: {text}"
        );
    }
    assert_eq!(
        found(db, &table, vec![filter("n", FilterOp::Equal, "3001")])
            .await
            .len(),
        0
    );
    assert_eq!(count(db.clone(), table.clone()).await, 121);
    let removed = run(
        db,
        &format!("confirm POST {name}/_delete_by_query?refresh=true\n{{\"query\": {{\"term\": {{\"kind\": \"bulk\"}}}}}}"),
    )
    .await;
    assert_eq!(removed.affected, 1);
    assert_eq!(count(db.clone(), table.clone()).await, 120);
    // The statement the connection test sends is answered by the server.
    assert_eq!(
        run(db, "SELECT 1").await.rows[0][0],
        Cell::Integer("1".into())
    );

    // 10. The "Monitor" section, and the overviews.
    assert!(caps.monitor, "{label_of}: monitor");
    let stats = db.server_stats().await.unwrap();
    assert_eq!(stats.rows.len(), 1, "{label_of}: one row of statistics");
    let stat = |name: &str| {
        stats
            .columns
            .iter()
            .position(|column| column.name == name)
            .map(|index| stats.rows[0][index].display())
    };
    assert_eq!(
        (
            stats.columns[0].name.as_str(),
            stats.columns[1].name.as_str()
        ),
        ("connections", "active"),
        "{label_of}: the two large figures"
    );
    assert!(stat("connections").is_some_and(|count| count.parse::<u32>().is_ok_and(|n| n >= 1)));
    assert!(stat("active").is_some_and(|count| count.parse::<u32>().is_ok()));
    assert!(
        stat("version").is_some_and(|version| version.len() > 14 && version.contains("earch ")),
        "{:?}",
        stat("version")
    );
    assert!(stat("database").is_some_and(|cluster| !cluster.is_empty()));
    assert!(
        stat("status").is_some_and(|status| ["green", "yellow", "red"].contains(&status.as_str()))
    );
    assert!(stat("nodes").is_some_and(|nodes| nodes.contains("data")));
    assert!(stat("indices").is_some_and(|count| count.parse::<u32>().is_ok_and(|n| n >= 1)));
    assert!(stat("documents").is_some_and(|count| count.parse::<u64>().is_ok_and(|n| n >= 120)));
    assert!(stat("shards").is_some_and(|shards| shards.contains("active")));
    assert!(stat("heap").is_some_and(|heap| heap.contains('%')));
    assert!(stat("node list").is_some_and(|nodes| nodes.contains("heap")));
    let view = |name: &str| TableRef {
        schema: String::new(),
        name: name.into(),
        kind: "view".into(),
    };
    let overview = page(
        db,
        &PageRequest {
            filters: vec![filter("index", FilterOp::Equal, name)],
            ..PageRequest::first(view("index overview"))
        },
    )
    .await;
    assert_eq!(overview.rows.len(), 1);
    assert_eq!(column(&overview, "status"), ["open"]);
    // The count of the server: its 120 documents and the two elements of
    // the nested list of one of them, which an index stores as documents.
    assert_eq!(column(&overview, "documents"), ["122"]);
    assert_eq!(column(&overview, "primary shards"), ["3"]);
    assert!(
        column(&overview, "size in bytes")[0]
            .parse::<u64>()
            .unwrap()
            > 0
    );
    assert!(column(&overview, "store size")[0].contains('B'));
    let all = page(db, &PageRequest::first(view("index overview"))).await;
    assert!(
        column(&all, "index")
            .iter()
            .all(|index| !index.starts_with('.'))
    );
    assert_eq!(
        db.columns(&view("index overview")).await.unwrap().len(),
        all.columns.len()
    );
    let nodes = page(db, &PageRequest::first(view("cluster nodes"))).await;
    assert!(!nodes.rows.is_empty());
    assert!(
        column(&nodes, "version")[0]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    );
    assert!(column(&nodes, "elected master").contains(&"true".to_owned()));
    assert!(column(&nodes, "heap %")[0].parse::<u32>().is_ok());
    let tasks = page(db, &PageRequest::first(view("running tasks"))).await;
    assert_eq!(tasks.columns.len(), 9);
    assert_eq!(
        code(
            db.apply(&RowChange::DeleteRow {
                table: view("cluster nodes"),
                key: key("x"),
            })
            .await
        ),
        "elasticsearch_read_only_object"
    );

    // 11. Stopping. A throttled rewrite of every document would take two
    // minutes; stopped, it is gone from the tasks of the server as well.
    let by_query = |db: Database| async move {
        let tasks = page(
            &db,
            &PageRequest {
                filters: vec![contains("action", "byquery")],
                ..PageRequest::first(TableRef {
                    schema: String::new(),
                    name: "running tasks".into(),
                    kind: "view".into(),
                })
            },
        )
        .await;
        column(&tasks, "description")
    };
    let slow = format!("POST {name}/_update_by_query?requests_per_second=1&scroll_size=10");
    let token = CancellationToken::new();
    let (worker, cancel) = (db.clone(), token.clone());
    let task = tokio::spawn(async move { worker.query(&slow, cancel).await });
    let mut seen_running = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if by_query(db.clone())
            .await
            .iter()
            .any(|task| task.contains(name))
        {
            seen_running = true;
            break;
        }
    }
    assert!(
        seen_running,
        "{label_of}: the rewrite is a task of the server"
    );
    let asked = Instant::now();
    token.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap_or_else(|_| panic!("{label_of}: the request was not stopped"))
        .unwrap();
    assert_eq!(
        stopped.unwrap_err().to_string(),
        "query_cancelled",
        "{label_of}: cancel"
    );
    let stopped_in = asked.elapsed();
    assert!(stopped_in < Duration::from_secs(5), "{stopped_in:?}");
    let mut gone_in = None;
    for _ in 0..80 {
        if by_query(db.clone())
            .await
            .iter()
            .all(|task| !task.contains(name))
        {
            gone_in = Some(asked.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let gone_in = gone_in.unwrap_or_else(|| panic!("{label_of}: the task went on on the server"));
    assert!(gone_in < Duration::from_secs(20), "{gone_in:?}");
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        db.query("GET _cluster/health", cancelled)
            .await
            .unwrap_err()
            .to_string(),
        "query_cancelled"
    );
    assert!(
        db.tables()
            .await
            .unwrap()
            .iter()
            .any(|found| found.name == name),
        "{label_of}: the connection goes on after a cancel"
    );
    println!(
        "{label_of} ({}): designer off (PUT in the console), listing with an alias and the overviews, 15 columns from the mapping, import (120 documents; an existing id, a repeated id and a refused document each left nothing behind), pages of 7 and 50 over three shards read every document once, order and reached pages, {} filters, field edits with the conflict check, insert, delete, builder off (SQL and API requests in the console, confirm for removals), monitor ({} columns) and three overviews, cancel in {} ms and the task gone from the server after {} ms",
        stat("version").unwrap_or_default(),
        46,
        stats.columns.len(),
        stopped_in.as_millis(),
        gone_in.as_millis()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a search server described by BAZA_DB_TEST_* variables"]
async fn server_passes_the_driver_checklist() {
    let profile = profile();
    let db = Database::connect(&profile, None).await.unwrap();
    let name = unique("bazaqa-checklist");
    // Run on a task of its own, so that a failed assertion still drops the index.
    let (worker, used, index) = (db.clone(), profile.clone(), name.clone());
    let outcome = tokio::spawn(async move { checklist(&worker, &used, &index).await }).await;
    dropped(&db, std::slice::from_ref(&name)).await;
    let left = db.tables().await.unwrap();
    db.close().await;
    assert!(left.iter().all(|table| !table.name.starts_with(&name)));
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
}

/// Deep pages, the cursor of SQL, a slow search that is stopped, large
/// answers and hidden indices: what needs more documents than the
/// checklist has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a search server of one's own: BAZA_DB_TEST_*; writes 30 000 documents"]
async fn deep_pages_cursors_and_a_stopped_search() {
    let profile = profile();
    let label_of = profile.product_label();
    let db = Database::connect(&profile, None).await.unwrap();
    let name = unique("bazaqa-deep");
    let outcome = {
        let (db, name, profile) = (db.clone(), name.clone(), profile.clone());
        tokio::spawn(async move {
            run(
                &db,
                &format!(
                    "PUT {name}\n{{\"settings\": {{\"number_of_shards\": 2, \"number_of_replicas\": 0}}, \"mappings\": {{\"dynamic\": false, \"properties\": {{\"n\": {{\"type\": \"long\"}}, \"tag\": {{\"type\": \"keyword\"}}}}}}}}"
                ),
            )
            .await;
            // 30 000 documents, written with requests of the console.
            for batch in 0..6 {
                let mut script = format!("POST {name}/_bulk\n");
                for n in (batch * 5000)..((batch + 1) * 5000) {
                    script.push_str(&format!(
                        "{{\"index\": {{\"_id\": \"d{n}\"}}}}\n{{\"n\": {n}, \"tag\": \"t{}\", \"loose\": \"not in the mapping {n}\", \"pad\": {{\"deep\": \"{}\"}}}}\n",
                        n % 10,
                        "x".repeat(40)
                    ));
                }
                let written = run(&db, &script).await;
                assert_eq!(written.affected, 5000);
            }
            run(&db, &format!("POST {name}/_refresh")).await;
            let table = db
                .tables()
                .await
                .unwrap()
                .into_iter()
                .find(|found| found.name == name)
                .expect("the index is listed");
            // Fields outside the mapping are found in documents and shown,
            // and the server cannot search or order them.
            let columns: Vec<(String, String)> = db
                .columns(&table)
                .await
                .unwrap()
                .into_iter()
                .map(|column| (column.name, column.data_type))
                .collect();
            let listed: Vec<(&str, &str)> = columns
                .iter()
                .map(|(name, kind)| (name.as_str(), kind.as_str()))
                .collect();
            assert_eq!(
                listed,
                [
                    ("_id", "_id"),
                    ("n", "long"),
                    ("tag", "keyword"),
                    ("loose", "unmapped"),
                    ("pad.deep", "unmapped"),
                ]
            );
            let mut request = PageRequest::first(table.clone());
            request.filters = vec![contains("loose", "mapping")];
            assert_eq!(page_failure(&db, &request).await, "elasticsearch_filter_unmapped");
            // Every document once, in pages of 500 without an order.
            let mut request = PageRequest::first(table.clone());
            let mut seen = BTreeSet::new();
            let mut read = 0usize;
            let started = Instant::now();
            loop {
                let rows = numbers(&page(&db, &request).await);
                read += rows.len();
                seen.extend(rows.iter().copied());
                if rows.len() < 500 {
                    break;
                }
                request.offset += 500;
            }
            assert_eq!((read, seen.len()), (30_000, 30_000), "{label_of}: 60 pages");
            let walked = started.elapsed();
            // Past what `from` may address: a page 25 000 documents deep,
            // reached at once by passing the documents before it.
            let mut deep = PageRequest::first(table.clone());
            deep.order_by = Some(("n".into(), false));
            (deep.limit, deep.offset) = (20, 25_000);
            let started = Instant::now();
            assert_eq!(
                numbers(&page(&db, &deep).await),
                (25_000..25_020).collect::<Vec<_>>(),
                "{label_of}: a page past the window"
            );
            let jumped = started.elapsed();
            assert!(db.page_text(&deep).unwrap().contains("search_after"));
            // The page after it goes on from where that one ended.
            deep.offset = 25_020;
            assert_eq!(numbers(&page(&db, &deep).await)[0], 25_020);
            // A large page: as many documents as a page may have.
            let mut large = PageRequest::first(table.clone());
            large.order_by = Some(("n".into(), true));
            large.limit = 20_000;
            let rows = page(&db, &large).await;
            assert_eq!(rows.rows.len(), 20_000);
            assert_eq!(numbers(&rows)[..2], [29_999, 29_998]);
            assert!(!rows.truncated);
            // SQL reads on through the cursor of the server, up to the rows
            // a result may hold.
            let from = quoted(&profile, &name);
            let some = run(&db, &format!("SELECT n FROM {from} WHERE n < 2500")).await;
            assert_eq!(some.rows.len(), 2500, "{label_of}: three fetches of SQL");
            assert!(!some.truncated);
            let every = run(&db, &format!("SELECT n, tag FROM {from}")).await;
            assert_eq!(every.rows.len(), 20_000, "{label_of}: the rows of a result");
            assert!(every.truncated);
            // A statement that groups is not paged everywhere: where the
            // server cuts its answer at a limit of its own, that is said.
            let distinct = run(&db, &format!("SELECT n FROM {from} GROUP BY n")).await;
            assert!(
                distinct.truncated && (200..=20_000).contains(&distinct.rows.len()),
                "{label_of}: {} of 30 000 groups, truncated {}",
                distinct.rows.len(),
                distinct.truncated
            );
            let counted = run(
                &db,
                &format!("SELECT tag, COUNT(*) AS c FROM {from} GROUP BY tag ORDER BY tag"),
            )
            .await;
            assert_eq!(column(&counted, "c"), vec!["3000"; 10]);
            assert!(!counted.truncated);
            assert_eq!(counted.columns[0].data_type, "keyword");
            // A slow search is stopped on the server when it is given up:
            // its task is gone long before it would have ended.
            let slow = format!(
                "GET {name}/_search\n{{\"size\": 1, \"query\": {{\"script\": {{\"script\": {{\"source\": \"long s = 0; for (int i = 0; i < 900000; i++) {{ s += i % 7; }} return s >= 0;\"}}}}}}}}"
            );
            let searching = |db: Database, name: String| async move {
                let tasks = page(
                    &db,
                    &PageRequest {
                        filters: vec![contains("action", "indices:data/read/search")],
                        ..PageRequest::first(TableRef {
                            schema: String::new(),
                            name: "running tasks".into(),
                            kind: "view".into(),
                        })
                    },
                )
                .await;
                column(&tasks, "description")
                    .iter()
                    .any(|description| description.contains(&name))
            };
            let token = CancellationToken::new();
            let (worker, cancel, text) = (db.clone(), token.clone(), slow.clone());
            let task = tokio::spawn(async move { worker.query(&text, cancel).await });
            let mut seen_running = false;
            for _ in 0..50 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if searching(db.clone(), name.clone()).await {
                    seen_running = true;
                    break;
                }
            }
            assert!(seen_running, "{label_of}: the search is a task of the server");
            let asked = Instant::now();
            token.cancel();
            let stopped = tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .expect("the search was not stopped")
                .unwrap();
            assert_eq!(stopped.unwrap_err().to_string(), "query_cancelled");
            let mut gone_in = None;
            for _ in 0..100 {
                if !searching(db.clone(), name.clone()).await {
                    gone_in = Some(asked.elapsed());
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let gone_in = gone_in.expect("the search went on on the server");
            // How long the same search takes when nobody stops it.
            let started = Instant::now();
            let whole = db.query(&slow, CancellationToken::new()).await;
            let natural = started.elapsed();
            assert!(whole.is_ok(), "{:?}", whole.err().map(|error| error.to_string()));
            assert!(
                gone_in < natural / 2,
                "{label_of}: stopped after {gone_in:?} of {natural:?}"
            );
            // An answer larger than a result may be is refused, not read.
            let huge = failure(
                &db,
                &format!("GET {name}/_search?size=10000\n{{\"_source\": true, \"script_fields\": {{\"big\": {{\"script\": \"String s = 'x'; for (int i = 0; i < 13; i++) {{ s += s; }} return s;\"}}}}}}"),
            )
            .await;
            assert_eq!(huge, "elasticsearch_answer_too_large");
            // Hidden indices show only when the profile asks for them.
            let mut with_hidden = profile.clone();
            with_hidden.set_option("show_hidden", "true");
            let hidden_name = format!(".{name}");
            run(&db, &format!("PUT {hidden_name}\n{{\"settings\": {{\"index.hidden\": true, \"number_of_replicas\": 0}}}}")).await;
            let plain = db.tables().await.unwrap();
            assert!(plain.iter().all(|found| found.name != hidden_name));
            let other = Database::connect(&with_hidden, None).await.unwrap();
            let shown = other.tables().await.unwrap();
            other.close().await;
            assert!(shown.iter().any(|found| found.name == hidden_name));
            run(&db, &format!("confirm DELETE {hidden_name}")).await;
            println!(
                "{label_of}: 30 000 documents in 60 pages in {} ms, a page 25 000 deep in {} ms, a page of 20 000, SQL through its cursor (2500 rows whole, 20 000 of 30 000 with the limit said), a search stopped after {} ms of {} ms, a 64 MB answer refused, a hidden index shown on request",
                walked.as_millis(),
                jumped.as_millis(),
                gone_in.as_millis(),
                natural.as_millis()
            );
        })
        .await
    };
    dropped(&db, std::slice::from_ref(&name)).await;
    dropped(&db, &[format!(".{name}")]).await;
    db.close().await;
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
}

/// A data stream and an alias over two indices: containers that are read
/// like an index and take no change from the grid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a search server of one's own: BAZA_DB_TEST_*"]
async fn a_data_stream_and_an_alias_are_read_only_containers() {
    let profile = profile();
    let label_of = profile.product_label();
    let db = Database::connect(&profile, None).await.unwrap();
    let name = unique("bazaqa-streams");
    let (stream, template, alias) = (
        format!("{name}-stream"),
        format!("{name}-template"),
        format!("{name}-all"),
    );
    let outcome = {
        let (db, name, stream, template, alias) = (
            db.clone(),
            name.clone(),
            stream.clone(),
            template.clone(),
            alias.clone(),
        );
        tokio::spawn(async move {
            run(
                &db,
                &format!(
                    "PUT _index_template/{template}\n{{\"index_patterns\": [\"{stream}*\"], \"data_stream\": {{}}, \"priority\": 500, \"template\": {{\"settings\": {{\"number_of_replicas\": 0}}, \"mappings\": {{\"properties\": {{\"message\": {{\"type\": \"text\"}}, \"level\": {{\"type\": \"keyword\"}}}}}}}}}}\nPUT _data_stream/{stream}"
                ),
            )
            .await;
            let mut script = format!("POST {stream}/_bulk?refresh=true\n");
            for n in 1..=30 {
                script.push_str(&format!(
                    "{{\"create\": {{}}}}\n{{\"@timestamp\": \"2026-10-12T10:{n:02}:00Z\", \"message\": \"event {n}\", \"level\": \"{}\"}}\n",
                    if n % 5 == 0 { "error" } else { "info" }
                ));
            }
            assert_eq!(run(&db, &script).await.affected, 30);
            // Two indices of different fields behind one alias.
            for (index, field) in [("a", "left"), ("b", "right")] {
                run(
                    &db,
                    &format!(
                        "PUT {name}-{index}\n{{\"settings\": {{\"number_of_replicas\": 0}}, \"mappings\": {{\"properties\": {{\"n\": {{\"type\": \"long\"}}, \"{field}\": {{\"type\": \"keyword\"}}}}}}, \"aliases\": {{\"{alias}\": {{}}}}}}\nPOST {name}-{index}/_bulk?refresh=true\n{{\"index\": {{\"_id\": \"same\"}}}}\n{{\"n\": 1, \"{field}\": \"x\"}}\n{{\"index\": {{\"_id\": \"{index}-2\"}}}}\n{{\"n\": 2, \"{field}\": \"y\"}}"
                    ),
                )
                .await;
            }
            let tables = db.tables().await.unwrap();
            let listed = |wanted: &str| {
                tables
                    .iter()
                    .find(|found| found.name == wanted)
                    .cloned()
                    .unwrap_or_else(|| panic!("{wanted} is not listed"))
            };
            let (streamed, joined) = (listed(&stream), listed(&alias));
            assert_eq!(streamed.kind, "data stream");
            assert_eq!(joined.kind, "alias");
            assert!(
                tables.iter().all(|found| !found.name.starts_with(".ds-")),
                "{label_of}: the indices behind a data stream are part of it"
            );
            // The data stream: its fields, its documents, a filter, an order.
            let columns = db.columns(&streamed).await.unwrap();
            assert_eq!(
                columns
                    .iter()
                    .map(|column| (column.name.as_str(), column.data_type.as_str()))
                    .collect::<Vec<_>>(),
                [
                    ("_id", "_id"),
                    ("@timestamp", "date"),
                    ("level", "keyword"),
                    ("message", "text"),
                ]
            );
            assert!(columns.iter().all(|column| !column.primary_key));
            let mut request = PageRequest::first(streamed.clone());
            request.order_by = Some(("@timestamp".into(), true));
            request.filters = vec![filter("level", FilterOp::Equal, "error")];
            let errors = page(&db, &request).await;
            assert_eq!(
                column(&errors, "message"),
                ["event 30", "event 25", "event 20", "event 15", "event 10", "event 5"]
            );
            let id = column(&errors, "_id")[0].clone();
            for change in [
                RowChange::UpdateCell {
                    table: streamed.clone(),
                    column: "message".into(),
                    old: text("event 30"),
                    new: text("changed"),
                    key: key(&id),
                },
                RowChange::DeleteRow {
                    table: streamed.clone(),
                    key: key(&id),
                },
                RowChange::InsertRow {
                    table: joined.clone(),
                    values: vec![("n".into(), Cell::Integer("3".into()))],
                },
            ] {
                assert_eq!(
                    code(db.apply(&change).await),
                    "elasticsearch_read_only_object",
                    "{label_of}"
                );
            }
            // The alias: the fields of both indices, the documents of both,
            // each once, although two of them share an id.
            let columns: Vec<String> = db
                .columns(&joined)
                .await
                .unwrap()
                .into_iter()
                .map(|column| column.name)
                .collect();
            assert_eq!(columns, ["_id", "left", "n", "right"]);
            let mut request = PageRequest::first(joined.clone());
            request.limit = 3;
            let first = page(&db, &request).await;
            request.offset = 3;
            let second = page(&db, &request).await;
            assert_eq!((first.rows.len(), second.rows.len()), (3, 1));
            let mut ids: Vec<String> = column(&first, "_id");
            ids.extend(column(&second, "_id"));
            ids.sort();
            assert_eq!(ids, ["a-2", "b-2", "same", "same"]);
            // An order by a field one of the two indices does not have.
            let mut by_left = PageRequest::first(joined.clone());
            by_left.order_by = Some(("left".into(), true));
            assert_eq!(
                column(&page(&db, &by_left).await, "left"),
                ["y", "x", "NULL", "NULL"]
            );
            // In the index itself, an id is one document to change.
            let index = listed(&format!("{name}-a"));
            db.apply(&RowChange::UpdateCell {
                table: index.clone(),
                column: "left".into(),
                old: text("x"),
                new: text("changed"),
                key: key("same"),
            })
            .await
            .unwrap();
            assert_eq!(
                column(&page(&db, &by_left).await, "left"),
                ["y", "changed", "NULL", "NULL"]
            );
            // A closed index is listed as one, and says why it is not read.
            run(&db, &format!("POST {name}-b/_close")).await;
            let closed = db
                .tables()
                .await
                .unwrap()
                .into_iter()
                .find(|found| found.name == format!("{name}-b"))
                .expect("a closed index is listed");
            assert_eq!(closed.kind, "closed index");
            assert!(
                db.columns(&closed)
                    .await
                    .unwrap()
                    .iter()
                    .all(|column| !column.primary_key)
            );
            assert_eq!(
                page_failure(&db, &PageRequest::first(closed.clone())).await,
                "elasticsearch_index_closed"
            );
            assert_eq!(
                code(
                    db.apply(&RowChange::DeleteRow {
                        table: closed,
                        key: key("same"),
                    })
                    .await
                ),
                "elasticsearch_read_only_object"
            );
        })
        .await
    };
    for request in [
        format!("confirm DELETE _data_stream/{stream}"),
        format!("DELETE _index_template/{template}"),
    ] {
        let _ = db.query(&request, CancellationToken::new()).await;
    }
    dropped(&db, &[format!("{name}-a"), format!("{name}-b")]).await;
    let left = db.tables().await.unwrap();
    db.close().await;
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
    assert!(left.iter().all(|table| !table.name.starts_with(&name)));
    println!(
        "{label_of}: a data stream (listed as one, its backing indices left out, read with a filter and an order, no change taken), an alias over two indices (the fields and the documents of both, an id they share read twice, no change taken) and a closed index (listed as one, its page explained)"
    );
}

/// A proxy that serves the API under a path of its own. It takes one
/// request per connection, cuts the prefix off its path and passes it on.
async fn prefix_front(profile: &ConnectionProfile, prefix: &'static str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let target = (profile.host.clone(), profile.port);
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let target = target.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0_u8; 16384];
                // The head of the request, then the body it announces.
                let (head_end, length) = loop {
                    match client.read(&mut buffer).await {
                        Ok(read) if read > 0 => request.extend_from_slice(&buffer[..read]),
                        _ => return,
                    }
                    if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (end + 4, length);
                    }
                };
                while request.len() < head_end + length {
                    match client.read(&mut buffer).await {
                        Ok(read) if read > 0 => request.extend_from_slice(&buffer[..read]),
                        _ => return,
                    }
                }
                let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
                let Some((line, headers)) = head.split_once("\r\n") else {
                    return;
                };
                let mut parts = line.splitn(3, ' ');
                let (Some(method), Some(path), Some(version)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    return;
                };
                let inner = match path.strip_prefix(prefix) {
                    Some("") => "/",
                    Some(inner) if inner.starts_with(['/', '?']) => inner,
                    _ => {
                        let _ = client
                            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                };
                let Ok(mut upstream) = tokio::net::TcpStream::connect(target).await else {
                    return;
                };
                let passed =
                    format!("{method} {inner} {version}\r\nConnection: close\r\n{headers}");
                if upstream.write_all(passed.as_bytes()).await.is_err()
                    || upstream.write_all(&request[head_end..]).await.is_err()
                {
                    return;
                }
                // The answer says that this connection ends with it, so
                // that the client does not send its next request into it.
                let mut answer = Vec::new();
                let line_end = loop {
                    match upstream.read(&mut buffer).await {
                        Ok(read) if read > 0 => answer.extend_from_slice(&buffer[..read]),
                        _ => return,
                    }
                    if let Some(end) = answer.windows(2).position(|part| part == b"\r\n") {
                        break end + 2;
                    }
                };
                if client.write_all(&answer[..line_end]).await.is_err()
                    || client.write_all(b"Connection: close\r\n").await.is_err()
                    || client.write_all(&answer[line_end..]).await.is_err()
                {
                    return;
                }
                let _ = tokio::io::copy(&mut upstream, &mut client).await;
            });
        }
    });
    port
}

/// A cluster behind a proxy under a path of its own: the prefix of the
/// profile is put in front of every request, and a typed path cannot
/// leave it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a search server without TLS: BAZA_DB_TEST_*"]
async fn a_path_prefix_reaches_a_server_behind_a_proxy() {
    let plain = profile();
    assert_eq!(
        plain.tls,
        TlsMode::Disable,
        "the proxy of this test speaks plain HTTP to the server"
    );
    let port = prefix_front(&plain, "/search/es").await;
    let mut behind = plain.clone();
    behind.host = "127.0.0.1".into();
    behind.port = port;
    // Without the prefix nothing that is a search server answers.
    assert_eq!(
        attempt(behind.clone()).await,
        "elasticsearch_not_search_api"
    );
    behind.set_option("path_prefix", "/search/es/");
    assert_eq!(attempt(behind.clone()).await, "1");
    let db = Database::connect(&behind, None).await.unwrap();
    let name = unique("bazaqa-prefix");
    let outcome = {
        let (db, name) = (db.clone(), name.clone());
        tokio::spawn(async move {
            create(&db, &name).await;
            let table = db
                .tables()
                .await
                .unwrap()
                .into_iter()
                .find(|found| found.name == name)
                .expect("the index is listed through the proxy");
            assert_eq!(db.columns(&table).await.unwrap().len(), 15);
            db.apply(&RowChange::InsertRow {
                table: table.clone(),
                values: vec![
                    ("_id".into(), text("a/b c")),
                    ("n".into(), Cell::Integer("1".into())),
                    ("label".into(), text("behind the proxy")),
                ],
            })
            .await
            .unwrap();
            assert_eq!(
                found(&db, &table, vec![contains("label", "PROXY")]).await,
                [1]
            );
            db.apply(&RowChange::UpdateCell {
                table: table.clone(),
                column: "label".into(),
                old: text("behind the proxy"),
                new: text("edited"),
                key: key("a/b c"),
            })
            .await
            .unwrap();
            let health = run(&db, "GET /_cluster/health?filter_path=status,cluster_name").await;
            assert!(value_of(&health, "status").is_some());
            assert_eq!(
                column(
                    &run(&db, &format!("GET _cat/indices/{name}?h=index")).await,
                    "index"
                ),
                std::slice::from_ref(&name)
            );
            assert_eq!(db.server_stats().await.unwrap().rows.len(), 1);
            // A typed path stays under the prefix of the profile.
            for path in ["../_cat/indices", "a/../../_cluster/health", "/../.."] {
                assert_eq!(
                    failure(&db, &format!("GET {path}")).await,
                    "elasticsearch_path_invalid",
                    "{path}"
                );
            }
        })
        .await
    };
    dropped(&db, std::slice::from_ref(&name)).await;
    db.close().await;
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
    // The whole address pasted into the Host field names the prefix too.
    let mut pasted = plain.clone();
    pasted.host = format!("http://127.0.0.1:{port}/search/es");
    assert_eq!(attempt(pasted).await, "1");
    println!(
        "{}: behind a proxy under /search/es (the prefix as an option and in a pasted address): index made, document added, filter, field edit, console, monitor; a typed path that leaves the prefix is refused",
        plain.product_label()
    );
}

/// A cluster that gives no point in time: pages are read in the order of
/// the shards, held to the same copies by a preference, and the positions
/// two shards share are still read once each.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs an Elasticsearch of one's own: it changes search.max_keep_alive of the cluster for a moment"]
async fn pages_are_read_without_a_point_in_time() {
    let profile = profile();
    let label_of = profile.product_label();
    let db = Database::connect(&profile, None).await.unwrap();
    let name = unique("bazaqa-nopit");
    // A point in time is asked for three minutes; a cluster that keeps
    // none for longer than one refuses to open it.
    run(
        &db,
        "PUT _cluster/settings\n{\"transient\": {\"search.default_keep_alive\": \"30s\", \"search.max_keep_alive\": \"1m\"}}",
    )
    .await;
    let outcome = {
        let (db, name) = (db.clone(), name.clone());
        tokio::spawn(async move {
            create(&db, &name).await;
            let mut script = format!("POST {name}/_bulk?refresh=true\n");
            for n in 1..=200 {
                script.push_str(&format!(
                    "{{\"index\": {{\"_id\": \"doc-{n}\"}}}}\n{{\"n\": {n}, \"kind\": \"kind {}\"}}\n",
                    n % 4
                ));
            }
            assert_eq!(run(&db, &script).await.affected, 200);
            let refused = failure(&db, &format!("POST {name}/_pit?keep_alive=3m")).await;
            assert_eq!(refused, "elasticsearch_request_invalid", "no point in time");
            let table = db
                .tables()
                .await
                .unwrap()
                .into_iter()
                .find(|found| found.name == name)
                .expect("the index is listed");
            for limit in [7_u32, 60] {
                let mut request = PageRequest::first(table.clone());
                request.limit = limit;
                let mut seen = BTreeSet::new();
                let mut read = 0;
                loop {
                    let rows = numbers(&page(&db, &request).await);
                    read += rows.len();
                    seen.extend(rows.iter().copied());
                    if rows.len() < limit as usize {
                        break;
                    }
                    request.offset += u64::from(limit);
                }
                assert_eq!((read, seen.len()), (200, 200), "pages of {limit}");
            }
            let mut ordered = PageRequest::first(table.clone());
            ordered.order_by = Some(("kind".into(), true));
            ordered.filters = vec![contains("n", "..100")];
            (ordered.limit, ordered.offset) = (10, 20);
            let third = page(&db, &ordered).await;
            assert_eq!(column(&third, "kind"), vec!["kind 3"; 5].into_iter().chain(vec!["kind 2"; 5]).collect::<Vec<_>>());
            let mut all = BTreeSet::new();
            ordered.offset = 0;
            ordered.limit = 9;
            loop {
                let rows = numbers(&page(&db, &ordered).await);
                all.extend(rows.iter().copied());
                if rows.len() < 9 {
                    break;
                }
                ordered.offset += 9;
            }
            assert_eq!(all.len(), 100, "an order with ties, read once each");
        })
        .await
    };
    run(
        &db,
        "PUT _cluster/settings\n{\"transient\": {\"search.default_keep_alive\": null, \"search.max_keep_alive\": null}}",
    )
    .await;
    dropped(&db, std::slice::from_ref(&name)).await;
    db.close().await;
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
    println!(
        "{label_of}: without a point in time (the cluster refuses one of three minutes): 200 documents on three shards read once each in pages of 7 and 60, and in an order with ties"
    );
}

/// An account that may use a few indices and nothing of the cluster: the
/// root of the API is closed to it, and so is everything the monitor asks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs an Elasticsearch with security and a user that may make roles: BAZA_DB_TEST_*"]
async fn an_account_without_cluster_rights_works_with_what_it_may_see() {
    let profile = profile();
    let label_of = profile.product_label();
    let admin = Database::connect(&profile, None).await.unwrap();
    let name = unique("bazaqa-limited");
    // The password of a user that lives for this test only; never shown.
    let secret = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    run(
        &admin,
        &format!(
            "PUT _security/role/{name}\n{{\"indices\": [{{\"names\": [\"{name}*\"], \"privileges\": [\"all\"]}}]}}\nPUT _security/user/{name}\n{{\"password\": \"{secret}\", \"roles\": [\"{name}\"]}}"
        ),
    )
    .await;
    let outcome = {
        let (admin, name, mut limited) = (admin.clone(), name.clone(), profile.clone());
        limited.username = name.clone();
        limited.password = secret;
        tokio::spawn(async move {
            create(&admin, &name).await;
            let mut script = format!("POST {name}/_bulk?refresh=true\n");
            for n in 1..=60 {
                script.push_str(&format!(
                    "{{\"index\": {{\"_id\": \"doc-{n}\"}}}}\n{{\"n\": {n}, \"label\": \"row {n}\"}}\n"
                ));
            }
            assert_eq!(run(&admin, &script).await.affected, 60);
            // The connection test passes: the account has signed in, even
            // though the server would not tell it what it is.
            assert_eq!(attempt(limited.clone()).await, "1");
            let db = Database::connect(&limited, None).await.unwrap();
            assert_eq!(failure(&db, "GET /").await, "elasticsearch_forbidden");
            let tables = db.tables().await.unwrap();
            let indices: Vec<&str> = tables
                .iter()
                .filter(|found| found.kind != "view")
                .map(|found| found.name.as_str())
                .collect();
            assert_eq!(indices, [name.as_str()], "{label_of}: only what the account may see");
            let table = tables
                .iter()
                .find(|found| found.name == name)
                .cloned()
                .unwrap();
            assert_eq!(db.columns(&table).await.unwrap().len(), 15);
            let mut request = PageRequest::first(table.clone());
            request.order_by = Some(("n".into(), false));
            request.limit = 25;
            let mut read = Vec::new();
            loop {
                let rows = numbers(&page(&db, &request).await);
                read.extend(rows.iter().copied());
                if rows.len() < 25 {
                    break;
                }
                request.offset += 25;
            }
            assert_eq!(read, (1..=60).collect::<Vec<_>>());
            assert_eq!(found(&db, &table, vec![contains("label", "ROW 33")]).await, [33]);
            let edit = RowChange::UpdateCell {
                table: table.clone(),
                column: "label".into(),
                old: text("row 33"),
                new: text("edited"),
                key: key("doc-33"),
            };
            db.apply(&edit).await.unwrap();
            assert_eq!(code(db.apply(&edit).await), CONFLICT);
            db.apply(&RowChange::InsertRow {
                table: table.clone(),
                values: vec![("n".into(), Cell::Integer("61".into()))],
            })
            .await
            .unwrap();
            db.apply(&RowChange::DeleteRow {
                table: table.clone(),
                key: key("doc-1"),
            })
            .await
            .unwrap();
            // What belongs to the cluster is closed, and said so; the
            // monitor shows what is left.
            assert_eq!(
                page_failure(
                    &db,
                    &PageRequest::first(TableRef {
                        schema: String::new(),
                        name: "cluster nodes".into(),
                        kind: "view".into(),
                    })
                )
                .await,
                "elasticsearch_forbidden"
            );
            let stats = db.server_stats().await.unwrap();
            assert_eq!(stats.rows.len(), 1);
            let names: Vec<&str> = stats
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect();
            assert!(names.contains(&"version") && !names.contains(&"connections"), "{names:?}");
            let other = failure(&db, "GET bazaqa-zamowienia/_count").await;
            assert!(
                matches!(
                    other.as_str(),
                    "elasticsearch_forbidden" | "elasticsearch_index_unknown"
                ),
                "{other}"
            );
            db.close().await;
        })
        .await
    };
    for request in [
        format!("DELETE _security/user/{name}"),
        format!("DELETE _security/role/{name}"),
    ] {
        let _ = admin.query(&request, CancellationToken::new()).await;
    }
    dropped(&admin, std::slice::from_ref(&name)).await;
    admin.close().await;
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
    println!(
        "{label_of}: an account with rights to its own indices only: the root and the monitor of the cluster are closed to it, and it lists, reads, filters, edits, adds and removes what it may see"
    );
}

fn ssh_profile() -> SshProfile {
    SshProfile {
        host: variable("BAZA_SSH_TEST_HOST").unwrap_or_else(|| "127.0.0.1".into()),
        port: required("BAZA_SSH_TEST_PORT").parse().unwrap(),
        username: required("BAZA_SSH_TEST_USER"),
        auth: SshAuth::PrivateKey,
        password: None,
        private_key: Some(std::fs::read_to_string(required("BAZA_SSH_TEST_KEY")).unwrap()),
        passphrase: variable("BAZA_SSH_TEST_KEY_PASSPHRASE"),
        fingerprint: String::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real SSH server and a search server: BAZA_SSH_TEST_* and BAZA_DB_TEST_*"]
async fn search_through_an_ssh_tunnel() {
    let mut profile = profile();
    let ssh = ssh_profile();
    profile.ssh = Some(ssh.clone());
    // What the application does for a profile with an SSH tab: a tunnel to
    // the Host and Port of the profile, and the driver dials its loopback
    // end while it names and verifies the server by its own name.
    let manager = TunnelManager::new().await;
    let tunnel = TunnelProfile {
        id: profile.id,
        name: profile.name.clone(),
        ssh,
        target_host: profile.host.clone(),
        target_port: profile.port,
        local_port: 0,
    };
    let status = manager.start(&tunnel).await.unwrap();
    let local = status.local_port.expect("the local end of the tunnel");
    assert_ne!(
        local, profile.port,
        "the driver dials the tunnel, not the server"
    );
    let before = manager.status(profile.id).unwrap();
    let db = Database::connect(&profile, Some(local)).await.unwrap();
    let name = unique("bazaqa-ssh");
    let outcome = {
        let (db, name) = (db.clone(), name.clone());
        tokio::spawn(async move {
            create(&db, &name).await;
            let mut script = format!("POST {name}/_bulk?refresh=true\n");
            for n in 1..=60 {
                script.push_str(&format!(
                    "{{\"create\": {{\"_id\": \"doc-{n}\"}}}}\n{{\"n\": {n}, \"label\": \"through the tunnel {n}\"}}\n"
                ));
            }
            assert_eq!(run(&db, &script).await.affected, 60);
            let table = db
                .tables()
                .await
                .unwrap()
                .into_iter()
                .find(|found| found.name == name)
                .expect("the index is listed through the tunnel");
            db.columns(&table).await.unwrap();
            let mut request = PageRequest::first(table.clone());
            request.order_by = Some(("n".into(), false));
            request.limit = 40;
            let first = page(&db, &request).await;
            request.offset = 40;
            let second = page(&db, &request).await;
            assert_eq!((first.rows.len(), second.rows.len()), (40, 20));
            assert_eq!(numbers(&second)[0], 41);
            assert_eq!(found(&db, &table, vec![contains("label", "TUNNEL 33")]).await, [33]);
            let edit = RowChange::UpdateCell {
                table: table.clone(),
                column: "label".into(),
                old: text("through the tunnel 33"),
                new: text("edited through the tunnel"),
                key: key("doc-33"),
            };
            db.apply(&edit).await.unwrap();
            assert_eq!(code(db.apply(&edit).await), CONFLICT);
            let stats = db.server_stats().await.unwrap();
            assert_eq!(stats.rows.len(), 1);
            stats.columns.len()
        })
        .await
    };
    let after = manager.status(profile.id).unwrap();
    dropped(&db, std::slice::from_ref(&name)).await;
    db.close().await;
    let columns = match outcome {
        Ok(columns) => columns,
        Err(failure) => {
            manager.shutdown().await;
            std::panic::resume_unwind(failure.into_panic());
        }
    };
    assert!(after.bytes_sent > before.bytes_sent && after.bytes_received > before.bytes_received);
    assert!(after.error.is_none(), "{:?}", after.error);
    // A server named by its address is dialled at the tunnel as well, and
    // its certificate is still checked against the address of the profile.
    let mut by_address = profile.clone();
    by_address.host = "127.0.0.1".into();
    let verified = match Database::connect(&by_address, Some(local)).await {
        Ok(session) => {
            let answer = session.query("SELECT 1", CancellationToken::new()).await;
            session.close().await;
            answer.map(|result| result.rows[0][0].display())
        }
        Err(error) => Err(error),
    };
    assert_eq!(
        verified.map_err(|error| error.to_string()),
        Ok("1".to_owned())
    );
    // A whole address in the Host field cannot go through a tunnel, which
    // is opened to what that field holds.
    let mut pasted = profile.clone();
    pasted.host = format!(
        "{}://{}",
        if profile.tls == TlsMode::Disable {
            "http"
        } else {
            "https"
        },
        profile.host
    );
    assert_eq!(
        Database::connect(&pasted, Some(local))
            .await
            .err()
            .map(|error| error.to_string())
            .as_deref(),
        Some("elasticsearch_endpoint_url_tunnel")
    );
    manager.shutdown().await;
    println!(
        "{} over 127.0.0.1:{local} (TLS {:?}): index made, 60 documents written, listing, two pages read on, filter, field edit with conflict check, monitor ({columns} columns); tunnel sent {} B, received {} B",
        profile.product_label(),
        profile.tls,
        after.bytes_sent,
        after.bytes_received
    );
}

/// A TLS endpoint in front of the plain server of the profile, for as long
/// as the test runs: what a server with TLS looks like to the driver.
async fn tls_front(profile: &ConnectionProfile) -> u16 {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certificates: Vec<CertificateDer<'static>> =
        CertificateDer::pem_file_iter(required("BAZA_DB_TEST_TLS_CERT"))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
    let key = PrivateKeyDer::from_pem_file(required("BAZA_DB_TEST_TLS_KEY")).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certificates, key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(std::sync::Arc::new(config));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let target = (profile.host.clone(), profile.port);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let (acceptor, target) = (acceptor.clone(), target.clone());
            tokio::spawn(async move {
                let Ok(mut secure) = acceptor.accept(socket).await else {
                    return;
                };
                let Ok(mut plain) = tokio::net::TcpStream::connect(target).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut secure, &mut plain).await;
            });
        }
    });
    port
}

/// Why a profile could not be connected, or what its first request said.
async fn attempt(profile: ConnectionProfile) -> String {
    match Database::connect(&profile, None).await {
        Err(error) => error.to_string(),
        Ok(db) => {
            let answer = db
                .query("SELECT 1", CancellationToken::new())
                .await
                .map(|result| result.rows[0][0].display())
                .unwrap_or_else(|error| error.to_string());
            db.close().await;
            answer
        }
    }
}

/// The three TLS modes of a profile, and what a connection that cannot be
/// made is called.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a search server and BAZA_DB_TEST_TLS_CERT, _KEY and _CA"]
async fn tls_is_encrypted_and_verified() {
    let own = profile();
    let other_ca = std::path::PathBuf::from(required("BAZA_DB_TEST_TLS_CA"));
    let mut said = Vec::new();
    if own.tls == TlsMode::Disable {
        // A plain server: an endpoint with a certificate for the name
        // `localhost` is put in front of it.
        let port = tls_front(&own).await;
        let with = |host: &str, tls: TlsMode, ca_file: Option<std::path::PathBuf>| {
            let mut profile = own.clone();
            profile.host = host.into();
            profile.port = port;
            profile.tls = tls;
            profile.ca_file = ca_file;
            profile
        };
        assert_eq!(
            attempt(with("localhost", TlsMode::Require, None)).await,
            "1"
        );
        assert_eq!(
            attempt(with("127.0.0.1", TlsMode::Require, None)).await,
            "1"
        );
        // The bundled roots do not know the authority of the test.
        assert_eq!(
            attempt(with("localhost", TlsMode::VerifyFull, None)).await,
            "database_certificate_rejected"
        );
        assert_eq!(
            attempt(with(
                "localhost",
                TlsMode::VerifyFull,
                Some(other_ca.clone())
            ))
            .await,
            "1"
        );
        // The right authority, the wrong name.
        assert_eq!(
            attempt(with(
                "127.0.0.1",
                TlsMode::VerifyFull,
                Some(other_ca.clone())
            ))
            .await,
            "database_certificate_rejected"
        );
        // The whole address pasted into the Host field.
        let mut pasted = with("localhost", TlsMode::VerifyFull, Some(other_ca.clone()));
        pasted.host = format!("https://localhost:{port}/");
        pasted.port = 9200;
        assert_eq!(attempt(pasted.clone()).await, "1");
        pasted.tls = TlsMode::Disable;
        assert_eq!(attempt(pasted).await, "elasticsearch_endpoint_tls_mismatch");
        // The whole work of a session through TLS.
        let db = Database::connect(
            &with("localhost", TlsMode::VerifyFull, Some(other_ca.clone())),
            None,
        )
        .await
        .unwrap();
        assert!(
            db.tables()
                .await
                .unwrap()
                .iter()
                .any(|found| found.name == "index overview")
        );
        assert_eq!(db.server_stats().await.unwrap().rows.len(), 1);
        db.close().await;
        // TLS asked of a server that has none: said in words.
        let mut encrypted = own.clone();
        encrypted.tls = TlsMode::Require;
        assert_eq!(attempt(encrypted).await, "database_tls_unsupported");
        // An endpoint that only speaks TLS, asked without it.
        let refused = attempt(with("localhost", TlsMode::Disable, None)).await;
        assert!(
            matches!(
                refused.as_str(),
                "database_network_error" | "elasticsearch_not_search_api"
            ),
            "{refused}"
        );
        said.push("an endpoint of the test in front of the plain server: without verification, with the bundled roots (rejected), with the CA file (accepted for its name only), by a pasted address, a session through TLS, TLS on a plain port and a plain connection to a TLS port explained");
    } else {
        // A server that speaks TLS itself, with a certificate of its own
        // authority (`BAZA_DB_TEST_CA_FILE`) for the name of the profile.
        let ca = own.ca_file.clone().expect("BAZA_DB_TEST_CA_FILE");
        let with = |host: &str, tls: TlsMode, ca_file: Option<std::path::PathBuf>| {
            let mut profile = own.clone();
            profile.host = host.into();
            profile.tls = tls;
            profile.ca_file = ca_file;
            profile
        };
        assert_eq!(attempt(with(&own.host, TlsMode::Require, None)).await, "1");
        assert_eq!(
            attempt(with(&own.host, TlsMode::VerifyFull, Some(ca.clone()))).await,
            "1"
        );
        // The bundled roots do not know the authority of the node, and
        // another authority did not sign its certificate.
        assert_eq!(
            attempt(with(&own.host, TlsMode::VerifyFull, None)).await,
            "database_certificate_rejected"
        );
        assert_eq!(
            attempt(with(&own.host, TlsMode::VerifyFull, Some(other_ca.clone()))).await,
            "database_certificate_rejected"
        );
        // The right authority, a name the certificate is not for. The
        // server is reached all the same: the name of the machine is one
        // of its own, and not one the node wrote into its certificate.
        let wrong = variable("BAZA_DB_TEST_WRONG_NAME");
        if let Some(wrong) = &wrong {
            assert_eq!(attempt(with(wrong, TlsMode::Require, None)).await, "1");
            assert_eq!(
                attempt(with(wrong, TlsMode::VerifyFull, Some(ca.clone()))).await,
                "database_certificate_rejected"
            );
        }
        // A server that speaks TLS, asked without it.
        let refused = attempt(with(&own.host, TlsMode::Disable, None)).await;
        assert!(
            matches!(
                refused.as_str(),
                "database_network_error" | "elasticsearch_not_search_api"
            ),
            "{refused}"
        );
        // A wrong password is told from everything else.
        if !own.password.is_empty() {
            let mut wrong_password = own.clone();
            wrong_password.password.push('x');
            assert_eq!(attempt(wrong_password).await, "database_login_rejected");
            let mut nobody = own.clone();
            nobody.username.clear();
            nobody.password.clear();
            assert_eq!(attempt(nobody).await, "database_login_rejected");
        }
        said.push(if wrong.is_some() {
            "the certificate of the server itself: without verification, with its CA file, with the bundled roots and with another CA (both rejected), under a name the certificate is not for (rejected), a plain connection to the TLS port explained, a wrong password told apart"
        } else {
            "the certificate of the server itself: without verification, with its CA file, with the bundled roots and with another CA (both rejected), a plain connection to the TLS port explained, a wrong password told apart"
        });
    }
    // A port nobody listens on.
    let mut closed = own.clone();
    closed.port = 1;
    assert_eq!(attempt(closed).await, "database_network_error");
    println!(
        "{}: {}; a closed port",
        own.product_label(),
        said.join("; ")
    );
}

/// An API key signs in instead of a user: the key in the password field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs an Elasticsearch with security: BAZA_DB_TEST_* with a user that may create API keys"]
async fn an_api_key_signs_in() {
    let profile = profile();
    let db = Database::connect(&profile, None).await.unwrap();
    let made = run(
        &db,
        "POST _security/api_key\n{\"name\": \"bazaqa-checklist\", \"expiration\": \"10m\"}",
    )
    .await;
    let field = |name: &str| value_of(&made, name).map(|cell| cell.display()).unwrap();
    let (id, secret, encoded) = (field("id"), field("api_key"), field("encoded"));
    let outcome = {
        let profile = profile.clone();
        tokio::spawn(async move {
            // The encoded key alone: the user name stays empty.
            let mut by_key = profile.clone();
            by_key.username.clear();
            by_key.password = encoded.clone();
            assert_eq!(attempt(by_key.clone()).await, "1");
            let session = Database::connect(&by_key, None).await.unwrap();
            assert!(!session.tables().await.unwrap().is_empty());
            let who = run(&session, "GET _security/_authenticate").await;
            assert_eq!(value_of(&who, "authentication_type"), Some(text("api_key")));
            session.close().await;
            // The id and the secret of the key, given apart.
            let mut by_parts = profile.clone();
            by_parts.set_option("auth", "api_key");
            by_parts.username = id.clone();
            by_parts.password = secret.clone();
            assert_eq!(attempt(by_parts).await, "1");
            // A key that is not one.
            let mut wrong = by_key.clone();
            wrong.password = "bm90IGEga2V5Om5vdCBhIHNlY3JldA==".into();
            assert_eq!(attempt(wrong).await, "database_login_rejected");
            // The same text sent as a password of a user is refused too.
            let mut as_password = by_key.clone();
            as_password.set_option("auth", "basic");
            as_password.username = "elastic".into();
            assert_eq!(attempt(as_password).await, "database_login_rejected");
            let mut bearer = by_key;
            bearer.set_option("auth", "bearer");
            assert_eq!(attempt(bearer).await, "database_login_rejected");
        })
        .await
    };
    let _ = db
        .query(
            "DELETE _security/api_key\n{\"name\": \"bazaqa-checklist\"}",
            CancellationToken::new(),
        )
        .await;
    db.close().await;
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
    println!(
        "{}: an API key in the password field signs in (encoded, and as id and secret), a wrong key and the key as a password or a bearer token are refused",
        profile.product_label()
    );
}
