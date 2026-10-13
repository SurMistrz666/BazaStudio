//! The driver checklist for MongoDB: what `driver_checklist.rs` checks for
//! an engine with SQL, for documents. Ignored by default; a server is
//! described by the same variables:
//!
//! ```text
//! BAZA_DB_TEST_PORT=27017 BAZA_DB_TEST_USER=root BAZA_DB_TEST_PASSWORD=… \
//! BAZA_DB_TEST_DATABASE=bazaqa_checklist \
//! cargo test -p baza-core --test mongodb_checklist -- --ignored --nocapture
//! ```
//!
//! Required: `BAZA_DB_TEST_PORT` and `BAZA_DB_TEST_DATABASE` (a database the
//! account may create collections in; only collections made by the run are
//! changed, and they are dropped at the end). Optional: `BAZA_DB_TEST_HOST`
//! (default `127.0.0.1`), `BAZA_DB_TEST_USER`, `BAZA_DB_TEST_PASSWORD`,
//! `BAZA_DB_TEST_PRODUCT` (a product id of the family), `BAZA_DB_TEST_TLS`
//! (`disable`, the default, `require` or `verify_full`),
//! `BAZA_DB_TEST_CA_FILE` and `BAZA_DB_TEST_OPTIONS` (engine options as
//! `key=value` pairs separated by `;`).
//!
//! How the steps of the SQL checklist map:
//!
//! | SQL checklist | here |
//! | --- | --- |
//! | 1. table designer | a collection and a view made in the console (`createCollection`) |
//! | 2. listing | the collection under its database, as a table; the view as a view |
//! | 3. columns | `_id` as the key, the fields that documents have, their BSON types |
//! | 4. import, all or nothing | the same, with a duplicate `_id` |
//! | 5. pages | the same: order, limit, offset |
//! | 6. "contains" filter | the same eight, and letter case outside ASCII, an ObjectId and a date as text |
//! | 7. one cell, conflict check | the same, and the BSON type of every kind of value after an edit |
//! | 8. insert, delete | the same, with a generated ObjectId as the key |
//! | 9. query builder | the console: every command of the list, the notation, what is refused |
//! | 10. monitor | the same |
//! | 11. stopping a statement | the same, checked in `currentOp` |
//!
//! `through_an_ssh_tunnel` needs `BAZA_SSH_TEST_PORT`, `BAZA_SSH_TEST_USER`
//! and `BAZA_SSH_TEST_KEY` as well (an SSH server that reaches the
//! database); `connection_failures_are_told_apart` needs
//! `BAZA_DB_TEST_PASSWORD` and uses `BAZA_DB_TEST_CLOSED_PORT` (default 1).
use baza_core::{
    Engine, TunnelManager,
    builder::{Filter, FilterOp},
    database::Database,
    drivers::{PageRequest, RowChange},
    model::{
        Cell, ConnectionProfile, QueryResult, SshAuth, SshProfile, TableRef, TlsMode, TunnelProfile,
    },
    sql,
};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}
fn required(name: &str) -> String {
    variable(name).unwrap_or_else(|| panic!("{name} is required"))
}

fn profile() -> ConnectionProfile {
    let engine = Engine::from_id("mongodb").expect("the MongoDB driver is registered");
    let product = variable("BAZA_DB_TEST_PRODUCT").unwrap_or_default();
    assert!(product.is_empty() || engine.spec().products.iter().any(|p| p.id == product));
    let mut profile = ConnectionProfile {
        name: "Real MongoDB".into(),
        engine,
        product,
        host: variable("BAZA_DB_TEST_HOST").unwrap_or_else(|| "127.0.0.1".into()),
        port: required("BAZA_DB_TEST_PORT").parse().unwrap(),
        database: required("BAZA_DB_TEST_DATABASE"),
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
async fn refused(db: &Database, statement: &str) -> String {
    db.query(statement, CancellationToken::new())
        .await
        .err()
        .unwrap_or_else(|| panic!("not refused: {statement}"))
        .to_string()
}
fn names(result: &QueryResult) -> Vec<&str> {
    result
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect()
}
fn at<'a>(result: &'a QueryResult, row: usize, name: &str) -> &'a Cell {
    let index = result
        .columns
        .iter()
        .position(|column| column.name == name)
        .unwrap_or_else(|| panic!("no column {name} in {:?}", names(result)));
    &result.rows[row][index]
}
fn kind<'a>(result: &'a QueryResult, name: &str) -> &'a str {
    result
        .columns
        .iter()
        .find(|column| column.name == name)
        .map(|column| column.data_type.as_str())
        .unwrap_or_else(|| panic!("no column {name}"))
}
fn ids(result: &QueryResult) -> Vec<i64> {
    result
        .rows
        .iter()
        .map(|row| row[0].display().parse().unwrap())
        .collect()
}
fn integer(number: i64) -> Cell {
    Cell::Integer(number.to_string())
}
fn text(value: &str) -> Cell {
    Cell::Text(value.into())
}
fn key(id: i64) -> Vec<(String, Cell)> {
    vec![("_id".to_owned(), integer(id))]
}
async fn count(db: &Database, collection: &str, filter: &str) -> i64 {
    run(db, &format!("db.{collection}.countDocuments({filter})"))
        .await
        .rows[0][0]
        .display()
        .parse()
        .unwrap()
}
async fn page(db: &Database, request: &PageRequest) -> QueryResult {
    db.read_page(request, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("{error}: {}", db.page_text(request).unwrap()))
}

/// How many operations on a collection the server is running now.
async fn running(db: &Database, collection: &str) -> usize {
    let reply = run(
        db,
        &format!(
            "db.adminCommand({{currentOp: 1, active: true, \"command.find\": \"{collection}\"}})"
        ),
    )
    .await;
    let operations: serde_json::Value =
        serde_json::from_str(&at(&reply, 0, "inprog").display()).unwrap();
    operations.as_array().unwrap().len()
}

/// The checklist. `name` is the collection it works on; the caller drops
/// what it left.
async fn checklist(db: &Database, profile: &ConnectionProfile, name: &str) {
    let label = profile.product_label();
    let caps = db.capabilities();
    let database = profile.database.as_str();
    let view = format!("{name}_view");
    let copy = format!("{name}_copy");
    assert_eq!(
        run(db, &format!("use {database}")).await.rows[0][0],
        text(&format!("switched to db {database}"))
    );

    // 1. No designer: a collection has no declared columns. It is made,
    // with a view over it, in the console.
    assert!(
        !caps.table_designer && !caps.query_builder,
        "{label}: SQL tools are off"
    );
    run(db, &format!("db.createCollection(\"{name}\")")).await;
    run(
        db,
        &format!("db.createCollection(\"{view}\", {{viewOn: \"{name}\", pipeline: [{{$match: {{kind: \"kind 2\"}}}}]}})"),
    )
    .await;

    // 2. The side panel: the database is the level above, the collection is
    // listed once as a table, the view as a view.
    assert!(caps.schemas, "{label}: databases are the schema level");
    let tables = db.tables().await.unwrap();
    let listed = tables
        .iter()
        .filter(|table| table.name == name)
        .collect::<Vec<_>>();
    assert_eq!(listed.len(), 1, "{label}: listed once");
    let table = listed[0].clone();
    assert_eq!(
        (table.schema.as_str(), table.kind.as_str()),
        (database, "table")
    );
    let listed_view = tables.iter().find(|table| table.name == view).unwrap();
    assert_eq!(listed_view.kind, "view");
    assert!(
        tables
            .iter()
            .all(|table| !table.name.starts_with("system.buckets.")),
        "{label}: the storage of time series is not listed"
    );

    // 3. Columns: an empty collection has its key and nothing else; the
    // fields come from the documents.
    let empty = db.columns(&table).await.unwrap();
    assert_eq!(empty.len(), 1);
    assert!(empty[0].name == "_id" && empty[0].primary_key && !empty[0].nullable);
    let first_page = page(db, &PageRequest::first(table.clone())).await;
    assert_eq!(
        names(&first_page),
        ["_id"],
        "{label}: an empty page has the key column"
    );
    assert!(first_page.rows.is_empty());
    run(
        db,
        &format!("db.{name}.insertOne({{_id: 0, label: \"seed\", amount: NumberDecimal(\"0.50\"), kind: \"kind 0\"}})"),
    )
    .await;
    let columns = db.columns(&table).await.unwrap();
    let described = columns
        .iter()
        .map(|column| format!("{} {}", column.name, column.data_type))
        .collect::<Vec<_>>();
    assert_eq!(
        described,
        [
            "_id int32",
            "label string",
            "amount decimal128",
            "kind string"
        ],
        "{label}: fields and their types"
    );
    assert!(
        columns[0].primary_key && !columns[0].nullable,
        "{label}: key"
    );
    assert!(columns[1..].iter().all(|column| !column.primary_key));

    // 4. Import: 120 documents at once, then a batch with a duplicate key
    // that must leave nothing behind.
    assert!(caps.import, "{label}: import");
    let fields = ["_id", "label", "amount", "kind"].map(str::to_owned);
    let rows = (1..=120)
        .map(|n| {
            vec![
                integer(n),
                Cell::Text(match n {
                    7 => "Zażółć gęślą jaźń 🦀".into(),
                    8 => "it's \"quoted\" \\ back".into(),
                    9 => "100% _under_".into(),
                    10 => String::new(),
                    _ => format!("row {n}"),
                }),
                if n == 11 {
                    Cell::Null
                } else {
                    Cell::Decimal(format!("{n}.50"))
                },
                Cell::Text(format!("kind {}", n % 4)),
            ]
        })
        .collect::<Vec<_>>();
    let imported = db
        .import_rows(&table, &fields, &rows, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(imported.affected, 120, "{label}: imported documents");
    let clash = vec![
        vec![integer(500), text("new"), Cell::Null, Cell::Null],
        vec![integer(5), text("duplicate key"), Cell::Null, Cell::Null],
    ];
    assert_eq!(
        db.import_rows(&table, &fields, &clash, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string(),
        "mongodb_duplicate_key",
        "{label}: a duplicate key fails the import"
    );
    assert_eq!(
        count(db, name, "{}").await,
        121,
        "{label}: the failed import left nothing"
    );
    assert_eq!(count(db, name, "{_id: 500}").await, 0);
    // A field the collection does not have is refused by the facade: the
    // import takes the fields the collection is known to have.
    assert_eq!(
        db.import_rows(
            &table,
            &["_id".into(), "unknown".into()],
            &[vec![integer(600), text("x")]],
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .to_string(),
        "import_column_unknown"
    );
    let removed = run(db, &format!("db.{name}.deleteOne({{_id: 0}})")).await;
    assert_eq!(at(&removed, 0, "deletedCount"), &integer(1));

    // 5. Pages, in order: 50 from the 101st leave the last 20.
    assert!(caps.sort && caps.filter, "{label}: sort and filter");
    let mut request = PageRequest::first(table.clone());
    request.order_by = Some(("_id".into(), false));
    request.limit = 50;
    assert_eq!(ids(&page(db, &request).await), (1..=50).collect::<Vec<_>>());
    request.offset = 100;
    assert_eq!(
        ids(&page(db, &request).await),
        (101..=120).collect::<Vec<_>>()
    );
    request.order_by = Some(("_id".into(), true));
    request.offset = 0;
    request.limit = 3;
    assert_eq!(ids(&page(db, &request).await), [120, 119, 118]);
    let sorted = PageRequest {
        order_by: Some(("_id".into(), false)),
        ..PageRequest::first(table.clone())
    };
    let all = page(db, &sorted).await;
    assert_eq!(all.rows.len(), 120);
    assert_eq!(names(&all), ["_id", "label", "amount", "kind"]);
    // What was written is what comes back, character for character and
    // digit for digit; the field that was left out is not there.
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(all.rows[index], *row, "{label}: document {index}");
    }
    assert_eq!(count(db, name, "{amount: {$exists: false}}").await, 1);
    assert_eq!(count(db, name, "{amount: {$type: \"decimal\"}}").await, 119);
    assert_eq!(count(db, name, "{_id: {$type: \"int\"}}").await, 120);
    // The view shows its part and is read like a collection.
    let through_view = page(db, &PageRequest::first(listed_view.clone())).await;
    assert_eq!(through_view.rows.len(), 30, "{label}: the view");

    // 6. The "contains" filter: text, the characters of a pattern as plain
    // characters, letter case, a number searched as text.
    let columns = db.columns(&table).await.unwrap();
    for (column, value, expected) in [
        ("label", "gęślą", vec![7]),
        ("label", "GĘŚLĄ", vec![7]),
        ("label", "100%", vec![9]),
        ("label", "_under_", vec![9]),
        ("label", "\\ back", vec![8]),
        ("label", "it's", vec![8]),
        ("label", "ROW 12", vec![12, 120]),
        ("label", "row 1.", vec![]),
        ("label", ".*", vec![]),
        ("_id", "119", vec![119]),
        (
            "amount",
            "7.5",
            vec![7, 17, 27, 37, 47, 57, 67, 77, 87, 97, 107, 117],
        ),
        ("label", "no such text", vec![]),
    ] {
        let request = PageRequest {
            filters: vec![Filter {
                column: column.into(),
                op: FilterOp::Contains,
                value: text(value),
            }],
            ..sorted.clone()
        };
        let found = page(db, &request).await;
        assert_eq!(
            ids(&found),
            expected,
            "{label}: {column} contains {value:?}"
        );
        // A filtered page has the columns of the collection, so the grid
        // keeps editing it, and an empty one still shows them.
        assert_eq!(names(&found), ["_id", "label", "amount", "kind"]);
        assert!(
            sql::editable_result_table(Some(table.clone()), &columns, &found).is_some(),
            "{label}: a filtered page is not editable"
        );
        // "Open in console": the text of the page is a command that reads
        // the same documents.
        let command = db.page_text(&request).unwrap();
        assert_eq!(
            ids(&run(db, &command).await),
            expected,
            "{label}: {command}"
        );
    }

    // 7. One cell, with the conflict check.
    assert!(caps.edit_cells && caps.insert_rows && caps.delete_rows);
    let edit = RowChange::UpdateCell {
        table: table.clone(),
        column: "label".into(),
        old: text("row 20"),
        new: text("Kopia D:\\zrzut\\ 'x' {\"a\":1}"),
        key: key(20),
    };
    assert_eq!(
        db.change_text(&edit).unwrap(),
        format!(
            "db.getSiblingDB(\"{database}\").{name}.updateOne({{_id: 20, label: \"row 20\"}}, {{$set: {{label: \"Kopia D:\\\\zrzut\\\\ 'x' {{\\\"a\\\":1}}\"}}}})"
        )
    );
    db.apply(&edit).await.unwrap();
    assert_eq!(
        db.apply(&edit).await.unwrap_err().to_string(),
        "row_changed_reload_required",
        "{label}: the same edit again is a conflict"
    );
    // A field that is not there yet takes the type the field has elsewhere.
    let large = Cell::Decimal("12345678901234567890.12".into());
    db.apply(&RowChange::UpdateCell {
        table: table.clone(),
        column: "amount".into(),
        old: Cell::Null,
        new: text("12345678901234567890.12"),
        key: key(11),
    })
    .await
    .unwrap();
    assert_eq!(
        count(db, name, "{_id: 11, amount: {$type: \"decimal\"}}").await,
        1
    );
    // NULL is a value of its own.
    db.apply(&RowChange::UpdateCell {
        table: table.clone(),
        column: "amount".into(),
        old: all.rows[20][2].clone(),
        new: Cell::Null,
        key: key(21),
    })
    .await
    .unwrap();
    assert_eq!(
        count(db, name, "{_id: 21, amount: {$type: \"null\"}}").await,
        1
    );
    for (change, code) in [
        (
            RowChange::UpdateCell {
                table: table.clone(),
                column: "_id".into(),
                old: integer(30),
                new: integer(31),
                key: key(30),
            },
            "mongodb_id_immutable",
        ),
        (
            RowChange::UpdateCell {
                table: table.clone(),
                column: "a.b".into(),
                old: Cell::Null,
                new: text("x"),
                key: key(30),
            },
            "mongodb_field_name_unsupported",
        ),
        (
            RowChange::UpdateCell {
                table: table.clone(),
                column: "label".into(),
                old: text("row 30"),
                new: text("x"),
                key: vec![],
            },
            "primary_key_required",
        ),
        (
            RowChange::UpdateCell {
                table: table.clone(),
                column: "label".into(),
                old: text("row 30"),
                new: text("x"),
                key: key(3000),
            },
            "row_changed_reload_required",
        ),
    ] {
        assert_eq!(db.apply(&change).await.unwrap_err().to_string(), code);
    }
    assert_eq!(count(db, name, "{_id: 30, label: \"row 30\"}").await, 1);

    // 8. A new document, with values and with nothing but its key, and a
    // removed one.
    db.apply(&RowChange::InsertRow {
        table: table.clone(),
        values: vec![
            ("_id".into(), integer(200)),
            ("label".into(), text("added")),
        ],
    })
    .await
    .unwrap();
    assert_eq!(
        db.apply(&RowChange::InsertRow {
            table: table.clone(),
            values: vec![("_id".into(), integer(200))],
        })
        .await
        .unwrap_err()
        .to_string(),
        "mongodb_duplicate_key"
    );
    // A field of the dialog that was left empty is left out of the
    // document, except a text, where empty is a value.
    let sparse = RowChange::InsertRow {
        table: table.clone(),
        values: vec![
            ("_id".into(), integer(201)),
            ("label".into(), text("")),
            ("amount".into(), Cell::Decimal(String::new())),
        ],
    };
    assert!(
        db.change_text(&sparse)
            .unwrap()
            .ends_with(".insertOne({_id: 201, label: \"\"})")
    );
    db.apply(&sparse).await.unwrap();
    assert_eq!(
        count(
            db,
            name,
            "{_id: 201, label: \"\", amount: {$exists: false}}"
        )
        .await,
        1
    );
    run(db, &format!("db.{name}.deleteOne({{_id: 201}})")).await;
    db.apply(&RowChange::InsertRow {
        table: table.clone(),
        values: vec![],
    })
    .await
    .unwrap();
    // The server gave the empty document an ObjectId: it is the key of a
    // row like any other, and reads back as itself.
    let generated = run(
        db,
        &format!("db.{name}.find({{_id: {{$type: \"objectId\"}}}})"),
    )
    .await;
    assert_eq!(generated.rows.len(), 1);
    let object_id = generated.rows[0][0].clone();
    assert!(
        matches!(&object_id, Cell::Text(id) if id.len() == 24 && id.bytes().all(|b| b.is_ascii_hexdigit()))
    );
    let by_object_id = vec![("_id".to_owned(), object_id)];
    db.apply(&RowChange::UpdateCell {
        table: table.clone(),
        column: "label".into(),
        old: Cell::Null,
        new: text("named later"),
        key: by_object_id.clone(),
    })
    .await
    .unwrap();
    assert_eq!(count(db, name, "{label: \"named later\"}").await, 1);
    let remove = RowChange::DeleteRow {
        table: table.clone(),
        key: by_object_id,
    };
    assert!(db.change_text(&remove).unwrap().contains(", ObjectId(\""));
    db.apply(&remove).await.unwrap();
    db.apply(&RowChange::DeleteRow {
        table: table.clone(),
        key: key(1),
    })
    .await
    .unwrap();
    assert_eq!(
        db.apply(&RowChange::DeleteRow {
            table: table.clone(),
            key: key(1),
        })
        .await
        .unwrap_err()
        .to_string(),
        "row_changed_reload_required",
        "{label}: a document that is gone cannot be removed again"
    );
    let mut check = page(db, &sorted).await;
    assert_eq!(
        check.rows.len(),
        120,
        "{label}: 120 documents, one gone, one new"
    );
    check
        .rows
        .retain(|row| ["1", "11", "20", "21", "200"].contains(&row[0].display().as_str()));
    assert_eq!(
        check.rows,
        [
            vec![integer(11), text("row 11"), large, text("kind 3")],
            vec![
                integer(20),
                text("Kopia D:\\zrzut\\ 'x' {\"a\":1}"),
                Cell::Decimal("20.50".into()),
                text("kind 0")
            ],
            vec![integer(21), text("row 21"), Cell::Null, text("kind 1")],
            vec![integer(200), text("added"), Cell::Null, Cell::Null],
        ]
    );

    // 7b. Every kind of value: how it is shown, and that an edit keeps the
    // BSON type of what is stored. The documents go to a collection of
    // their own, so that the columns above stay as they are.
    let typed = TableRef {
        name: copy.clone(),
        ..table.clone()
    };
    run(
        db,
        &format!(
            r#"db.{copy}.insertMany([
              {{ _id: 1, small: 1, big: NumberLong(1), ratio: 1.0, price: NumberDecimal("1.10"),
                 when: ISODate("2026-05-01T10:00:00Z"), ref: ObjectId("65f0c1e2a4b5c6d7e8f90123"),
                 nested: {{ city: 'Łódź', lines: [1, NumberLong(2), 2.5, {{ sku: "A-1" }}] }},
                 tags: ["a", "b"], raw: BinData(0, "AAEC/w=="),
                 uid: UUID("73ffd264-44b3-4c69-90e8-e7d1dfc035d4"), flag: true, nothing: null,
                 pattern: /^a/i, stamp: Timestamp(7, 3), mixed: 5 }},
              {{ _id: 2, small: 2, big: NumberLong("9007199254740993"), ratio: 0.1, price: NumberDecimal("2"),
                 when: new Date(0), mixed: "five" }}
            ])"#
        ),
    )
    .await;
    let shown = page(db, &PageRequest::first(typed.clone())).await;
    for (column, kind_name, cell) in [
        ("_id", "int32", integer(1)),
        ("small", "int32", integer(1)),
        ("big", "int64", integer(1)),
        ("ratio", "double", Cell::Float("1.0".into())),
        ("price", "decimal128", Cell::Decimal("1.10".into())),
        ("when", "date", text("2026-05-01T10:00:00Z")),
        ("ref", "objectId", text("65f0c1e2a4b5c6d7e8f90123")),
        (
            "nested",
            "object",
            Cell::Json(
                "{\"city\":\"Łódź\",\"lines\":[1,{\"$numberLong\":\"2\"},2.5,{\"sku\":\"A-1\"}]}"
                    .into(),
            ),
        ),
        ("tags", "array", Cell::Json("[\"a\",\"b\"]".into())),
        ("raw", "binData", Cell::Bytes(vec![0, 1, 2, 255])),
        (
            "uid",
            "binData",
            Cell::Json("{\"$uuid\":\"73ffd264-44b3-4c69-90e8-e7d1dfc035d4\"}".into()),
        ),
        ("flag", "bool", Cell::Bool(true)),
        ("nothing", "null", Cell::Null),
        (
            "pattern",
            "regex",
            Cell::Json("{\"$regularExpression\":{\"pattern\":\"^a\",\"options\":\"i\"}}".into()),
        ),
        (
            "stamp",
            "timestamp",
            Cell::Json("{\"$timestamp\":{\"t\":7,\"i\":3}}".into()),
        ),
        ("mixed", "mixed: int32, string", integer(5)),
    ] {
        assert_eq!(kind(&shown, column), kind_name, "{label}: type of {column}");
        assert_eq!(at(&shown, 0, column), &cell, "{label}: {column}");
    }
    assert_eq!(at(&shown, 1, "big"), &integer(9_007_199_254_740_993));
    assert_eq!(at(&shown, 1, "when"), &text("1970-01-01T00:00:00Z"));
    assert_eq!(at(&shown, 1, "mixed"), &text("five"));
    assert_eq!(at(&shown, 1, "nested"), &Cell::Null);
    db.columns(&typed).await.unwrap();
    let before = shown.rows[0].clone();
    // The editor makes the new cell from the old one, as `from_edited`.
    let mut edits = Vec::new();
    for (column, typed_text, bson_type, expected) in [
        ("small", "7", "int", integer(7)),
        ("big", "5", "long", integer(5)),
        ("ratio", "2", "double", Cell::Float("2.0".into())),
        ("price", "2.50", "decimal", Cell::Decimal("2.50".into())),
        (
            "when",
            "2027-01-02T03:04:05.678Z",
            "date",
            text("2027-01-02T03:04:05.678Z"),
        ),
        (
            "ref",
            "000000000000000000000001",
            "objectId",
            text("000000000000000000000001"),
        ),
        (
            "nested",
            "{\"city\":\"Gdańsk\",\"lines\":[{\"$numberLong\":\"9\"}],\"since\":{\"$date\":\"2026-01-01T00:00:00Z\"}}",
            "object",
            Cell::Json(
                "{\"city\":\"Gdańsk\",\"lines\":[{\"$numberLong\":\"9\"}],\"since\":{\"$date\":\"2026-01-01T00:00:00Z\"}}".into(),
            ),
        ),
        ("tags", "[\"a\",\"b\",3]", "array", Cell::Json("[\"a\",\"b\",3]".into())),
        ("flag", "false", "bool", Cell::Bool(false)),
        ("mixed", "6", "int", integer(6)),
    ] {
        let old = at(&shown, 0, column).clone();
        let new = old.from_edited(typed_text, false).unwrap();
        db.apply(&RowChange::UpdateCell {
            table: typed.clone(),
            column: column.into(),
            old,
            new,
            key: key(1),
        })
        .await
        .unwrap_or_else(|error| panic!("{label}: {column}: {error}"));
        assert_eq!(
            count(db, &copy, &format!("{{_id: 1, {column}: {{$type: \"{bson_type}\"}}}}")).await,
            1,
            "{label}: {column} kept its type"
        );
        edits.push((column, expected));
    }
    let after = page(db, &PageRequest::first(typed.clone())).await;
    for (column, expected) in &edits {
        assert_eq!(
            at(&after, 0, column),
            expected,
            "{label}: {column} after the edit"
        );
    }
    // Everything that was not edited is what it was.
    for (index, column) in shown.columns.iter().enumerate() {
        if edits.iter().all(|(edited, _)| *edited != column.name) {
            assert_eq!(
                after.rows[0][index], before[index],
                "{label}: {} untouched",
                column.name
            );
        }
    }
    assert_eq!(
        after.rows[1], shown.rows[1],
        "{label}: the other document untouched"
    );
    // An int32 grows into an int64 when the number no longer fits.
    db.apply(&RowChange::UpdateCell {
        table: typed.clone(),
        column: "small".into(),
        old: integer(7),
        new: integer(3_000_000_000),
        key: key(1),
    })
    .await
    .unwrap();
    assert_eq!(
        count(db, &copy, "{_id: 1, small: {$type: \"long\"}}").await,
        1
    );
    // A field that holds nothing and has no type elsewhere: what is typed
    // says what it is.
    for (typed_text, bson_type) in [
        ("42", "int"),
        ("hello", "string"),
        ("{\"a\": [1, 2]}", "object"),
        ("ISODate(\"2026-05-01\")", "date"),
    ] {
        let shown = page(db, &PageRequest::first(typed.clone())).await;
        let old = at(&shown, 0, "nothing").clone();
        let new = Cell::Null.from_edited(typed_text, false).unwrap();
        db.apply(&RowChange::UpdateCell {
            table: typed.clone(),
            column: "nothing".into(),
            old,
            new,
            key: key(1),
        })
        .await
        .unwrap();
        assert_eq!(
            count(
                db,
                &copy,
                &format!("{{_id: 1, nothing: {{$type: \"{bson_type}\"}}}}")
            )
            .await,
            1,
            "{label}: {typed_text} typed into an empty field"
        );
        run(
            db,
            &format!("db.{copy}.updateOne({{_id: 1}}, {{$set: {{nothing: null}}}})"),
        )
        .await;
    }
    // The conflict check sees a value that changed underneath, also when
    // only its type did.
    run(
        db,
        &format!("db.{copy}.updateOne({{_id: 2}}, {{$set: {{small: \"2\"}}}})"),
    )
    .await;
    assert_eq!(
        db.apply(&RowChange::UpdateCell {
            table: typed.clone(),
            column: "small".into(),
            old: integer(2),
            new: integer(3),
            key: key(2),
        })
        .await
        .unwrap_err()
        .to_string(),
        "row_changed_reload_required"
    );
    // An ObjectId is shown as its digits. When a text with the same
    // characters is a key as well, the grid cannot say which document is
    // meant, and says so instead of choosing.
    run(
        db,
        &format!(
            "db.{copy}.insertMany([{{_id: ObjectId('65f0c1e2a4b5c6d7e8f90124'), mixed: 1}}, {{_id: '65f0c1e2a4b5c6d7e8f90124', mixed: 2}}])"
        ),
    )
    .await;
    let twin = vec![("_id".to_owned(), text("65f0c1e2a4b5c6d7e8f90124"))];
    for change in [
        RowChange::UpdateCell {
            table: typed.clone(),
            column: "mixed".into(),
            old: integer(1),
            new: integer(3),
            key: twin.clone(),
        },
        RowChange::DeleteRow {
            table: typed.clone(),
            key: twin.clone(),
        },
    ] {
        assert_eq!(
            db.apply(&change).await.unwrap_err().to_string(),
            "mongodb_key_ambiguous"
        );
    }
    run(
        db,
        &format!("db.{copy}.deleteOne({{_id: '65f0c1e2a4b5c6d7e8f90124'}})"),
    )
    .await;
    db.apply(&RowChange::DeleteRow {
        table: typed.clone(),
        key: twin,
    })
    .await
    .unwrap();
    assert_eq!(count(db, &copy, "{mixed: {$in: [1, 2]}}").await, 0);
    // What a typed file holds comes back as it was: the cells of a page,
    // imported as a document with another key.
    let exported = page(db, &PageRequest::first(typed.clone())).await;
    let mut kept = exported.rows[0].clone();
    kept[0] = integer(3);
    let exported_columns = names(&exported)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let again = db
        .import_rows(
            &typed,
            &exported_columns,
            std::slice::from_ref(&kept),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(again.affected, 1);
    let back = page(db, &PageRequest::first(typed.clone())).await;
    let restored = back
        .rows
        .iter()
        .find(|row| row[0] == integer(3))
        .expect("the imported document");
    assert_eq!(*restored, kept, "{label}: export and import of a page");
    assert_eq!(
        restored[1..],
        back.rows[0][1..],
        "{label}: the copy shows what the original shows"
    );
    for (column, bson_type) in [
        ("small", "long"),
        ("big", "long"),
        ("ratio", "double"),
        ("price", "decimal"),
        ("when", "date"),
        ("ref", "objectId"),
        ("raw", "binData"),
        ("uid", "binData"),
        ("stamp", "timestamp"),
        ("pattern", "regex"),
    ] {
        assert_eq!(
            count(
                db,
                &copy,
                &format!("{{_id: 3, {column}: {{$type: \"{bson_type}\"}}}}")
            )
            .await,
            1,
            "{label}: {column} after the import"
        );
    }

    // 9. The console.
    assert_eq!(caps.language, baza_core::QueryLanguage::Command);
    let found = run(
        db,
        &format!(
            "db.{name}.find({{kind: 'kind 2'}}, {{label: 1}}).sort({{_id: -1}}).skip(1).limit(3)"
        ),
    )
    .await;
    assert_eq!(names(&found), ["_id", "label"]);
    assert_eq!(ids(&found), [114, 110, 106]);
    let one = run(db, &format!("db.{name}.findOne({{_id: 7}})")).await;
    assert_eq!(at(&one, 0, "label"), &text("Zażółć gęślą jaźń 🦀"));
    assert_eq!(count(db, name, "{kind: \"kind 2\"}").await, 30);
    assert_eq!(
        run(db, &format!("db.{name}.find({{kind: \"kind 2\"}}).count()"))
            .await
            .rows[0][0],
        integer(30)
    );
    assert_eq!(
        run(db, &format!("db.{name}.estimatedDocumentCount()"))
            .await
            .rows[0][0],
        integer(120)
    );
    let grouped = run(
        db,
        &format!(
            "db.{name}.aggregate([\n  {{ $match: {{ kind: {{ $exists: true }} }} }},\n  {{ $group: {{ _id: \"$kind\", n: {{ $sum: 1 }}, top: {{ $max: \"$amount\" }} }} }},\n  {{ $sort: {{ _id: 1 }} }}\n])"
        ),
    )
    .await;
    assert_eq!(names(&grouped), ["_id", "n", "top"]);
    assert_eq!(
        grouped
            .rows
            .iter()
            .map(|row| (row[0].display(), row[1].display()))
            .collect::<Vec<_>>(),
        [
            ("kind 0", "30"),
            ("kind 1", "29"),
            ("kind 2", "30"),
            ("kind 3", "30")
        ]
        .map(|(kind, n)| (kind.to_owned(), n.to_owned()))
    );
    assert_eq!(
        grouped.rows[3][2],
        Cell::Decimal("12345678901234567890.12".into())
    );
    let mut distinct = run(
        db,
        &format!("db.{name}.distinct(\"kind\", {{_id: {{$lt: 100}}}})"),
    )
    .await;
    assert_eq!(names(&distinct), ["value"]);
    assert_eq!(distinct.columns[0].data_type, "string");
    distinct.rows.sort_by_key(|row| row[0].display());
    assert_eq!(
        distinct.rows,
        ["kind 0", "kind 1", "kind 2", "kind 3"].map(|kind| vec![text(kind)])
    );
    // The notation of the shell: a regular expression, constructors, single
    // quotes, comments, a chain over several lines.
    let by_pattern = run(
        db,
        &format!("db.{name}\n  .find({{ label: /^ROW 1\\d$/i }}) // case is ignored\n  .sort({{ _id: 1 }})"),
    )
    .await;
    assert_eq!(ids(&by_pattern), (11..=19).collect::<Vec<_>>());
    let written = run(
        db,
        &format!(
            "db.{name}.insertOne({{ _id: ObjectId('65f0c1e2a4b5c6d7e8f90123'), at: ISODate(\"2026-05-01T10:00:00Z\"), views: NumberLong(5), 'quoted key': {{ $numberDecimal: '1.5' }} }})"
        ),
    )
    .await;
    assert_eq!(names(&written), ["acknowledged", "insertedId"]);
    assert_eq!(
        written.rows[0],
        [Cell::Bool(true), text("65f0c1e2a4b5c6d7e8f90123")]
    );
    assert_eq!(
        count(
            db,
            name,
            "{_id: ObjectId(\"65f0c1e2a4b5c6d7e8f90123\"), at: {$type: \"date\"}, views: {$type: \"long\"}, \"quoted key\": {$type: \"decimal\"}}"
        )
        .await,
        1
    );
    // An ObjectId and a date are found as text by the filter of the grid.
    for (column, value) in [("_id", "65F0C1E2"), ("at", "2026-05-01T10")] {
        let request = PageRequest {
            filters: vec![Filter {
                column: column.into(),
                op: FilterOp::Contains,
                value: text(value),
            }],
            ..PageRequest::first(table.clone())
        };
        assert_eq!(
            page(db, &request).await.rows.len(),
            1,
            "{label}: {column} contains {value}"
        );
    }
    let many = run(
        db,
        &format!("db.{name}.insertMany([{{_id: 301, kind: 'x'}}, {{_id: 302, kind: 'x'}}])"),
    )
    .await;
    assert_eq!(
        names(&many),
        ["acknowledged", "insertedCount", "insertedIds"]
    );
    assert_eq!(
        many.rows[0][1..],
        [integer(2), Cell::Json("[301,302]".into())]
    );
    assert_eq!(
        refused(
            db,
            &format!("db.{name}.insertMany([{{_id: 303}}, {{_id: 301}}, {{_id: 304}}])")
        )
        .await,
        "mongodb_write_failed_11000_after_1"
    );
    let updated = run(
        db,
        &format!("db.{name}.updateOne({{_id: 301}}, {{$set: {{kind: 'y'}}}})"),
    )
    .await;
    assert_eq!(
        names(&updated),
        [
            "acknowledged",
            "matchedCount",
            "modifiedCount",
            "upsertedCount",
            "upsertedId"
        ]
    );
    assert_eq!(updated.rows[0][1..3], [integer(1), integer(1)]);
    let upserted = run(
        db,
        &format!("db.{name}.updateOne({{_id: 305}}, {{$set: {{kind: 'x'}}}}, {{upsert: true}})"),
    )
    .await;
    assert_eq!(
        upserted.rows[0][1..],
        [integer(0), integer(0), integer(1), integer(305)]
    );
    let several = run(
        db,
        &format!("db.{name}.updateMany({{kind: 'x'}}, {{$inc: {{hits: 1}}}})"),
    )
    .await;
    assert_eq!(several.rows[0][1..3], [integer(2), integer(2)]);
    let replaced = run(
        db,
        &format!("db.{name}.replaceOne({{_id: 303}}, {{kind: 'x', hits: 9}})"),
    )
    .await;
    assert_eq!(replaced.rows[0][2], integer(1));
    let deleted = run(db, &format!("db.{name}.deleteOne({{_id: 301}})")).await;
    assert_eq!(names(&deleted), ["acknowledged", "deletedCount"]);
    assert_eq!(deleted.rows[0][1], integer(1));
    let deleted = run(
        db,
        &format!("db.{name}.deleteMany({{hits: {{$exists: true}}}})"),
    )
    .await;
    assert_eq!(deleted.rows[0][1], integer(3));
    run(
        db,
        &format!("db.{name}.deleteOne({{_id: ObjectId(\"65f0c1e2a4b5c6d7e8f90123\")}})"),
    )
    .await;
    assert_eq!(count(db, name, "{}").await, 120);
    let index = run(db, &format!("db.{name}.createIndex({{kind: 1, _id: -1}})")).await;
    assert_eq!(index.rows[0][0], text("kind_1__id_-1"));
    let indexes = run(db, &format!("db.{name}.getIndexes()")).await;
    assert_eq!(indexes.rows.len(), 2);
    assert!(
        (0..2).any(|row| at(&indexes, row, "name") == &text("kind_1__id_-1")),
        "{label}: the index is listed"
    );
    run(db, &format!("db.{name}.dropIndex(\"kind_1__id_-1\")")).await;
    assert_eq!(
        run(db, &format!("db.{name}.getIndexes()")).await.rows.len(),
        1
    );
    let collection_names = run(db, "db.getCollectionNames()").await;
    assert!(collection_names.rows.contains(&vec![text(name)]));
    let collections = run(db, "show collections").await;
    assert_eq!(names(&collections), ["name", "type", "documents"]);
    assert!(
        collections
            .rows
            .contains(&vec![text(name), text("collection"), integer(120)])
    );
    assert!(
        collections
            .rows
            .contains(&vec![text(&view), text("view"), Cell::Null])
    );
    let databases = run(db, "show dbs").await;
    assert!(databases.rows.iter().any(|row| row[0] == text(database)));
    assert_eq!(run(db, "db.getName()").await.rows[0][0], text(database));
    let pong = run(db, "db.runCommand({ping: 1})").await;
    assert_eq!(at(&pong, 0, "ok"), &Cell::Float("1.0".into()));
    let raw = run(
        db,
        &format!("db.runCommand({{count: \"{name}\", query: {{kind: \"kind 1\"}}}})"),
    )
    .await;
    assert_eq!(at(&raw, 0, "n"), &integer(29));
    assert!(
        !run(db, "db.version()").await.rows[0][0]
            .display()
            .is_empty()
    );
    // `use` changes what `db` stands for, for the statements that follow.
    let elsewhere = run(db, &format!("use {database}_other\ndb.getName()")).await;
    assert_eq!(elsewhere.rows[0][0], text(&format!("{database}_other")));
    assert_eq!(count(db, name, "{}").await, 0, "{label}: another database");
    assert_eq!(
        run(
            db,
            &format!("db.getSiblingDB(\"{database}\").{name}.countDocuments()")
        )
        .await
        .rows[0][0],
        integer(120)
    );
    run(db, &format!("use {database}")).await;
    // The connection test of the application.
    assert_eq!(run(db, "SELECT 1").await.rows[0][0], integer(1));
    // What is not understood is refused with its place, and a script with
    // a mistake runs nothing.
    for (statement, code) in [
        (
            format!("db.{name}.find().forEach(printjson)"),
            "mongodb_script_unsupported_1",
        ),
        (
            format!("var n = db.{name}.countDocuments()"),
            "mongodb_script_unsupported_1_1",
        ),
        (
            format!("db.{name}.find({{_id: }})"),
            "mongodb_syntax_error_1",
        ),
        (
            format!("db.{name}.findAndModify({{}})"),
            "mongodb_method_unsupported_1_1",
        ),
        (
            format!("db.{name}.updateOne({{_id: 2}}, {{label: 'x'}})"),
            "mongodb_arguments_invalid_1_1",
        ),
        (
            "SELECT * FROM orders".to_owned(),
            "mongodb_script_unsupported_1_1",
        ),
        (
            format!("db.{name}.insertOne({{_id: 999}})\ndb.{name}.find({{a: b}})"),
            "mongodb_script_unsupported_2",
        ),
        (
            format!("db.{name}.find({{$nope: 1}})"),
            "mongodb_server_error_2",
        ),
        (
            format!("db.{name}.aggregate([{{$nope: 1}}])"),
            "mongodb_server_error_40324",
        ),
        (
            "db.runCommand({nope: 1})".to_owned(),
            "mongodb_command_unknown",
        ),
    ] {
        let answer = refused(db, &statement).await;
        assert!(answer.starts_with(code), "{label}: {statement}: {answer}");
    }
    assert_eq!(
        count(db, name, "{_id: 999}").await,
        0,
        "{label}: nothing of a refused script ran"
    );
    // Removing or rewriting everything needs the word `confirm`.
    for statement in [
        format!("db.{name}.deleteMany({{}})"),
        format!("db.{name}.updateMany({{}}, {{$set: {{kind: 'gone'}}}})"),
        format!("db.{name}.drop()"),
        "db.dropDatabase()".to_owned(),
        format!("db.runCommand({{drop: \"{name}\"}})"),
        format!("db.{name}.find()\ndb.{name}.deleteMany({{}})"),
    ] {
        let answer = refused(db, &statement).await;
        assert!(
            answer.starts_with("mongodb_confirmation_required_"),
            "{statement}: {answer}"
        );
    }
    assert_eq!(count(db, name, "{kind: {$ne: \"gone\"}}").await, 120);
    let emptied = run(db, &format!("confirm db.{copy}.deleteMany({{}})")).await;
    assert_eq!(at(&emptied, 0, "deletedCount"), &integer(3));
    assert_eq!(
        run(db, &format!("confirm db.{copy}.drop()")).await.rows[0][0],
        Cell::Bool(true)
    );
    // Dropping what is not there is no error: a server before 7.0 says
    // so, and the answer is then `false`, as in the shell.
    assert!(matches!(
        run(db, &format!("confirm db.{copy}.drop()")).await.rows[0][0],
        Cell::Bool(_)
    ));
    assert!(
        db.tables()
            .await
            .unwrap()
            .iter()
            .all(|table| table.name != copy)
    );

    // A result stops at the row limit and says so.
    let long = run(
        db,
        &format!(
            "db.{name}.aggregate([{{$limit: 1}}, {{$project: {{n: {{$range: [0, {}]}}}}}}, {{$unwind: \"$n\"}}])",
            sql::MAX_RESULT_ROWS + 50
        ),
    )
    .await;
    assert_eq!(long.rows.len(), sql::MAX_RESULT_ROWS, "{label}: row limit");
    assert!(long.truncated, "{label}: a cut result says so");

    // 9b. A field that only a document deep in the collection has. The
    // fields are learned from the first documents and the newest ones, so
    // a page can show a field the columns did not have: from then on it is
    // a column of the collection, and the grid edits the page again.
    let late = format!("{name}_late");
    let documents = (1..=520)
        .map(|n| {
            if n == 310 {
                format!("{{_id: {n}, rare: true}}")
            } else {
                format!("{{_id: {n}, n: {n}}}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    run(db, &format!("db.{late}.insertMany([{documents}])")).await;
    let late_table = TableRef {
        name: late.clone(),
        ..table.clone()
    };
    let learned = db.columns(&late_table).await.unwrap();
    assert_eq!(
        learned
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["_id", "n"]
    );
    let first = page(db, &PageRequest::first(late_table.clone())).await;
    assert_eq!(first.rows.len(), 500);
    assert_eq!(names(&first), ["_id", "n", "rare"]);
    assert!(
        sql::editable_result_table(Some(late_table.clone()), &learned, &first).is_none(),
        "{label}: a page with a field the columns lack is not edited"
    );
    let learned = db.columns(&late_table).await.unwrap();
    assert_eq!(learned.len(), 3);
    assert!(learned[2].name == "rare" && learned[2].default.is_some());
    let again = page(db, &PageRequest::first(late_table.clone())).await;
    assert!(
        sql::editable_result_table(Some(late_table.clone()), &learned, &again).is_some(),
        "{label}: the next reading has the field as a column"
    );
    // Every page has the same columns, also one without that document.
    let mut second = PageRequest::first(late_table.clone());
    second.offset = 500;
    let rest = page(db, &second).await;
    assert_eq!(rest.rows.len(), 20);
    assert_eq!(names(&rest), ["_id", "n", "rare"]);
    run(db, &format!("confirm db.{late}.drop()")).await;

    // 10. The "Monitor" section.
    assert!(caps.monitor, "{label}: monitor");
    let stats = db.server_stats().await.unwrap();
    assert_eq!(stats.rows.len(), 1, "{label}: one row of statistics");
    assert!(!at(&stats, 0, "version").display().is_empty());
    assert_eq!(at(&stats, 0, "database"), &text(database));
    assert_eq!(
        names(&stats)[..2],
        ["connections", "active"],
        "{label}: the large figures"
    );
    for column in [
        "max_connections",
        "opcounters",
        "memory",
        "currentOp",
        "uptime",
    ] {
        assert!(
            !at(&stats, 0, column).display().is_empty(),
            "{label}: {column}"
        );
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let later = db.server_stats().await.unwrap();
    assert!(at(&later, 0, "ops/s").display().starts_with("insert "));
    let operations = run(db, "db.currentOp()").await;
    assert!(operations.columns.is_empty() || names(&operations).contains(&"opid"));

    // 11. A running statement is stopped, here and on the server.
    let token = CancellationToken::new();
    let worker = db.clone();
    let cancel = token.clone();
    let slow = format!("db.{name}.find({{$where: \"sleep(1000) || true\"}})");
    let task = tokio::spawn(async move { worker.query(&slow, cancel).await });
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        running(db, name).await,
        1,
        "{label}: the slow statement is running"
    );
    let asked = Instant::now();
    token.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap_or_else(|_| panic!("{label}: the statement was not stopped"))
        .unwrap();
    assert_eq!(stopped.unwrap_err().to_string(), "query_cancelled");
    let waited = asked.elapsed();
    assert_eq!(
        count(db, name, "{}").await,
        120,
        "{label}: the connection goes on"
    );
    let mut left = running(db, name).await;
    for _ in 0..20 {
        if left == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        left = running(db, name).await;
    }
    assert_eq!(
        left, 0,
        "{label}: the cancelled statement still runs on the server"
    );
    // The same for a page of the grid.
    let token = CancellationToken::new();
    token.cancel();
    assert_eq!(
        db.read_page(&PageRequest::first(table.clone()), token)
            .await
            .unwrap_err()
            .to_string(),
        "query_cancelled"
    );
    println!(
        "{label} {}: collection and view, listing, columns, import (all or nothing), paging, 12 filters, cell edits with conflict check and kept types, insert, delete, console, confirmations, monitor ({} columns), cancel in {} ms",
        at(&stats, 0, "version").display(),
        stats.columns.len(),
        waited.as_millis()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real MongoDB server described by BAZA_DB_TEST_* variables"]
async fn server_passes_the_document_checklist() {
    let profile = profile();
    let db = Database::connect(&profile, None).await.unwrap();
    let name = format!("checklist_{}", uuid::Uuid::new_v4().simple());
    // On a task of its own, so that a failed assertion still drops what
    // the run made.
    let worker = db.clone();
    let checked = profile.clone();
    let collection = name.clone();
    let outcome =
        tokio::spawn(async move { checklist(&worker, &checked, &collection).await }).await;
    let _ = db
        .query(
            &format!("use {}", profile.database),
            CancellationToken::new(),
        )
        .await;
    for suffix in ["_view", "_copy", "_late", ""] {
        let _ = db
            .query(
                &format!("confirm db.{name}{suffix}.drop()"),
                CancellationToken::new(),
            )
            .await;
    }
    let left = db.tables().await.unwrap();
    db.close().await;
    assert!(left.iter().all(|table| !table.name.starts_with(&name)));
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
}

/// What the dialog shows for a failed connection: a sentence about what
/// is wrong, quickly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real MongoDB server described by BAZA_DB_TEST_* variables"]
async fn connection_failures_are_told_apart() {
    let good = profile();
    let failure = |profile: ConnectionProfile| async move {
        let started = Instant::now();
        let code = match Database::connect(&profile, None).await {
            Ok(_) => panic!(
                "connected: {}:{} as {:?}, {:?}, {:?}",
                profile.host, profile.port, profile.username, profile.tls, profile.options
            ),
            Err(error) => error.to_string(),
        };
        (code, started.elapsed())
    };
    let mut wrong_password = good.clone();
    wrong_password.password = "not-the-password".into();
    assert_eq!(failure(wrong_password).await.0, "database_login_rejected");
    let mut wrong_source = good.clone();
    wrong_source.set_option("auth_source", "bazaqa_no_such_source");
    assert_eq!(failure(wrong_source).await.0, "database_login_rejected");
    // A server that asks for an account, and no account.
    let mut anonymous = good.clone();
    anonymous.username.clear();
    anonymous.password.clear();
    if !good.username.is_empty() {
        assert_eq!(failure(anonymous).await.0, "database_login_rejected");
    }
    // Nothing listens: said at once, not after the client gave up waiting.
    let mut closed = good.clone();
    closed.port = variable("BAZA_DB_TEST_CLOSED_PORT")
        .map(|port| port.parse().unwrap())
        .unwrap_or(1);
    let (code, took) = failure(closed).await;
    assert_eq!(code, "database_network_error");
    assert!(took < Duration::from_secs(5), "{took:?}");
    let mut nowhere = good.clone();
    nowhere.host = "bazaqa-no-such-host.invalid".into();
    assert_eq!(failure(nowhere).await.0, "database_network_error");
    if good.tls == TlsMode::Disable {
        // Encryption asked of a server that has none.
        for mode in [TlsMode::Require, TlsMode::VerifyFull] {
            let mut encrypted = good.clone();
            encrypted.tls = mode;
            let (code, took) = failure(encrypted).await;
            assert_eq!(code, "database_tls_unsupported", "{mode:?}");
            assert!(took < Duration::from_secs(5), "{took:?}");
        }
    } else {
        // No encryption asked of a server that requires it.
        let mut plain = good.clone();
        plain.tls = TlsMode::Disable;
        assert_eq!(failure(plain).await.0, "mongodb_connection_closed");
        // A certificate that the system's authorities did not sign.
        if good.tls == TlsMode::VerifyFull && good.ca_file.is_some() {
            let mut unknown_ca = good.clone();
            unknown_ca.ca_file = None;
            assert_eq!(failure(unknown_ca).await.0, "database_certificate_rejected");
            // The same server under a name its certificate does not carry
            // (`BAZA_DB_TEST_OTHER_HOST`, default 127.0.0.1).
            let mut other_name = good.clone();
            other_name.host =
                variable("BAZA_DB_TEST_OTHER_HOST").unwrap_or_else(|| "127.0.0.1".into());
            other_name.set_option("direct_connection", "true");
            assert_eq!(failure(other_name).await.0, "database_certificate_rejected");
            let mut unverified = good.clone();
            unverified.tls = TlsMode::Require;
            unverified.ca_file = None;
            Database::connect(&unverified, None)
                .await
                .unwrap()
                .close()
                .await;
        }
    }
    // The settings that exclude each other never reach the network.
    let mut pasted = good.clone();
    pasted.host = format!("mongodb://{}:{}", good.host, good.port);
    assert_eq!(failure(pasted).await.0, "mongodb_host_invalid");
    // A replica set name the server does not have.
    let mut other_set = good.clone();
    other_set.set_option("replica_set", "bazaqa_no_such_set");
    let (code, took) = failure(other_set).await;
    assert_eq!(code, "mongodb_no_server_selected");
    assert!(took < Duration::from_secs(15), "{took:?}");
    // After all of that the good profile still connects, directly too.
    let mut direct = good.clone();
    direct.set_option("direct_connection", "true");
    for profile in [good, direct] {
        let db = Database::connect(&profile, None).await.unwrap();
        assert_eq!(run(&db, "SELECT 1").await.rows[0][0], integer(1));
        db.close().await;
    }
}

/// The database through the SSH tab of its connection: the tunnel forwards
/// one port, so the connection is direct, and TLS is made on this side of
/// the tunnel with the name of the real host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a real SSH server and MongoDB: BAZA_SSH_TEST_* and BAZA_DB_TEST_*"]
async fn through_an_ssh_tunnel() {
    let mut profile = profile();
    profile.ssh = Some(SshProfile {
        host: variable("BAZA_SSH_TEST_HOST").unwrap_or_else(|| "127.0.0.1".into()),
        port: required("BAZA_SSH_TEST_PORT").parse().unwrap(),
        username: required("BAZA_SSH_TEST_USER"),
        auth: SshAuth::PrivateKey,
        password: None,
        private_key: Some(std::fs::read_to_string(required("BAZA_SSH_TEST_KEY")).unwrap()),
        passphrase: variable("BAZA_SSH_TEST_KEY_PASSPHRASE"),
        fingerprint: String::new(),
    });
    profile.validate().unwrap();
    // The tunnel, as the application opens it for a connection.
    let manager = TunnelManager::new().await;
    let status = manager
        .start(&TunnelProfile {
            id: profile.id,
            name: profile.name.clone(),
            ssh: profile.ssh.clone().unwrap(),
            target_host: profile.host.clone(),
            target_port: profile.port,
            local_port: 0,
        })
        .await
        .unwrap();
    let local = status.local_port.expect("a forwarded port");
    assert_ne!(local, profile.port);
    let before = manager.status(profile.id).unwrap();

    // A replica set name and further hosts do not matter in a tunnel: the
    // connection is direct, to the forwarded port.
    let mut through = profile.clone();
    through.set_option("replica_set", "bazaqa_no_such_set");
    through.set_option("extra_hosts", "bazaqa-no-such-host.invalid:27017");
    let db = Database::connect(&through, Some(local)).await.unwrap();
    let name = format!("tunnel_{}", uuid::Uuid::new_v4().simple());
    run(
        &db,
        &format!("db.{name}.insertMany([{{_id: 1, city: 'Gdańsk'}}, {{_id: 2, city: 'Łódź'}}])"),
    )
    .await;
    let tables = db.tables().await.unwrap();
    let table = tables
        .iter()
        .find(|table| table.name == name)
        .expect("the collection is listed through the tunnel")
        .clone();
    let rows = db
        .read_page(&PageRequest::first(table.clone()), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(rows.rows[1], [integer(2), text("Łódź")]);
    db.apply(&RowChange::UpdateCell {
        table: table.clone(),
        column: "city".into(),
        old: text("Łódź"),
        new: text("Kraków"),
        key: key(2),
    })
    .await
    .unwrap();
    assert_eq!(count(&db, &name, "{city: 'Kraków'}").await, 1);
    assert_eq!(db.server_stats().await.unwrap().rows.len(), 1);
    run(&db, &format!("confirm db.{name}.drop()")).await;
    let after = manager.status(profile.id).unwrap();
    println!(
        "MongoDB over 127.0.0.1:{local}: tunnel sent {} B, received {} B, {} client(s)",
        after.bytes_sent, after.bytes_received, after.active_clients
    );
    // The traffic of the database is the traffic of the tunnel.
    assert!(after.bytes_sent > before.bytes_sent && after.bytes_received > before.bytes_received);
    assert!(after.active_clients >= 1 && after.error.is_none());
    db.close().await;

    // A DNS seed list has no single port to forward.
    let mut seed_list = profile.clone();
    seed_list.host = "cluster0.example.mongodb.net".into();
    seed_list.set_option("srv", "true");
    assert_eq!(
        Database::connect(&seed_list, Some(local))
            .await
            .err()
            .unwrap()
            .to_string(),
        "mongodb_srv_through_tunnel"
    );
    let mut wrong_password = profile.clone();
    wrong_password.password = "not-the-password".into();
    assert_eq!(
        Database::connect(&wrong_password, Some(local))
            .await
            .err()
            .unwrap()
            .to_string(),
        "database_login_rejected"
    );
    // TLS inside the tunnel is made by this side, with the name of the
    // real host. What the server answers decides: a success, or a sentence
    // about TLS, never "the network was lost".
    for mode in [TlsMode::Require, TlsMode::VerifyFull] {
        if mode == profile.tls {
            continue;
        }
        let mut encrypted = profile.clone();
        encrypted.tls = mode;
        match Database::connect(&encrypted, Some(local)).await {
            Ok(db) => {
                assert_eq!(run(&db, "SELECT 1").await.rows[0][0], integer(1));
                db.close().await;
                println!("  {mode:?} connects inside the tunnel");
            }
            Err(error) => {
                let code = error.to_string();
                assert!(
                    matches!(
                        code.as_str(),
                        "database_tls_unsupported" | "database_certificate_rejected"
                    ),
                    "{mode:?}: {code}"
                );
                println!("  {mode:?} inside the tunnel: {code}");
            }
        }
    }
    manager.shutdown().await;
}
