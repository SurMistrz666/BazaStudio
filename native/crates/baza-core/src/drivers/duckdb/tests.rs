//! The DuckDB driver on databases of its own: files in a temporary folder
//! and databases in memory. They run with every `cargo test`; the steps
//! every driver has to pass are in `tests/duckdb_checklist.rs`.
use super::*;
use crate::builder::{Filter, FilterOp};
use crate::database::Database;
use crate::drivers::PageRequest;
use crate::model::ConnectionProfile;

fn profile(path: &Path, options: &[(&str, &str)]) -> ConnectionProfile {
    let mut profile = ConnectionProfile {
        name: "Owned DuckDB".into(),
        engine: ENGINE,
        sqlite_path: Some(path.to_owned()),
        ..ConnectionProfile::default()
    };
    for (key, value) in options {
        profile.set_option(key, value);
    }
    profile
}

async fn open_new(path: &Path) -> Database {
    Database::connect(&profile(path, &[("create", "true")]), None)
        .await
        .unwrap()
}

async fn refused(path: &Path, options: &[(&str, &str)]) -> String {
    match Database::connect(&profile(path, options), None).await {
        Ok(_) => panic!("the database was opened"),
        Err(error) => error.to_string(),
    }
}

async fn run(db: &Database, statement: &str) -> QueryResult {
    db.query(statement, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("{error}: {statement}"))
}

async fn fails(db: &Database, statement: &str) -> String {
    db.query(statement, CancellationToken::new())
        .await
        .expect_err(statement)
        .to_string()
}

fn text(value: &str) -> Cell {
    Cell::Text(value.into())
}
fn integer(value: i64) -> Cell {
    Cell::Integer(value.to_string())
}
fn key(id: i64) -> Vec<(String, Cell)> {
    vec![("id".to_owned(), integer(id))]
}
fn table(name: &str) -> TableRef {
    TableRef {
        schema: "main".into(),
        name: name.into(),
        kind: "table".into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_is_created_only_when_asked_and_read_back_after_closing() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("shop.duckdb");
    // A path that names nothing is a mistake, not a new database.
    assert_eq!(refused(&path, &[]).await, "duckdb_file_not_found");
    assert_eq!(
        refused(&path, &[("create", "true"), ("read_only", "true")]).await,
        "duckdb_file_not_found"
    );
    assert!(!path.exists());
    let db = open_new(&path).await;
    run(
        &db,
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, city VARCHAR, total DECIMAL(12,2)); \
         INSERT INTO orders VALUES (1, 'Gdańsk', 318.00), (2, 'Łódź', 0.50); \
         CREATE VIEW cities AS SELECT DISTINCT city FROM orders",
    )
    .await;
    assert_eq!(
        db.tables()
            .await
            .unwrap()
            .into_iter()
            .map(|found| (found.schema, found.name, found.kind))
            .collect::<Vec<_>>(),
        [
            ("main".into(), "cities".into(), "view".into()),
            ("main".into(), "orders".into(), "table".into()),
        ]
    );
    db.close().await;
    assert!(std::fs::metadata(&path).unwrap().len() > 0);

    // Read-only: the rows are there, and nothing can be changed.
    let reader = Database::connect(&profile(&path, &[("read_only", "true")]), None)
        .await
        .unwrap();
    let rows = run(&reader, "SELECT * FROM orders ORDER BY id").await;
    assert_eq!(
        rows.rows,
        [
            vec![integer(1), text("Gdańsk"), Cell::Decimal("318.00".into())],
            vec![integer(2), text("Łódź"), Cell::Decimal("0.50".into())],
        ]
    );
    assert_eq!(
        fails(&reader, "INSERT INTO orders VALUES (3, 'x', 1)").await,
        "duckdb_read_only"
    );
    assert_eq!(
        reader
            .apply(&RowChange::DeleteRow {
                table: table("orders"),
                key: key(1),
            })
            .await
            .unwrap_err()
            .to_string(),
        "duckdb_read_only"
    );
    assert_eq!(reader.server_stats().await.unwrap().rows.len(), 1);
    reader.close().await;
    // After a close every operation says that the connection is gone.
    assert_eq!(fails(&reader, "SELECT 1").await, "connection_unavailable");

    // What is not a database of DuckDB, and what is not a file.
    let other = folder.path().join("notes.duckdb");
    std::fs::write(&other, "id;city\n1;Gdańsk\n".repeat(400)).unwrap();
    assert_eq!(refused(&other, &[]).await, "duckdb_file_invalid");
    assert_eq!(refused(folder.path(), &[]).await, "duckdb_file_unreadable");
    for code in [
        "duckdb_file_not_found",
        "duckdb_file_invalid",
        "duckdb_file_unreadable",
        "duckdb_file_locked",
        "duckdb_read_only",
        "duckdb_transaction_left_open",
    ] {
        assert!(describe(code, false).is_some() && describe(code, true).is_some());
        assert_ne!(describe(code, false), describe(code, true));
    }
}

/// Two instances of the library on one file would each write it as if
/// alone. Every connection of the process to a file shares one database.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_file_is_one_database_for_every_connection_of_the_process() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("shared.duckdb");
    let first = open_new(&path).await;
    run(&first, "CREATE TABLE one AS SELECT 1 AS x").await;
    // The same file, spelled another way.
    let spelled = folder.path().join(".").join("shared.duckdb");
    let second = Database::connect(&profile(&spelled, &[]), None)
        .await
        .unwrap();
    assert_eq!(
        run(&second, "SELECT x FROM one").await.rows,
        [vec![integer(1)]]
    );
    run(&second, "CREATE TABLE two AS SELECT 2 AS y").await;
    // One database: what either writes, the other sees at once.
    assert_eq!(
        run(&first, "SELECT y FROM two").await.rows,
        [vec![integer(2)]]
    );
    // It cannot be read-only for one connection and not for another.
    assert_eq!(
        refused(&path, &[("read_only", "true")]).await,
        "duckdb_file_locked"
    );
    first.close().await;
    // The file stays open for the connection that is left.
    run(&second, "INSERT INTO two VALUES (3)").await;
    second.close().await;
    // Closed by its last connection: it opens again, read-only this time.
    let reader = Database::connect(&profile(&path, &[("read_only", "true")]), None)
        .await
        .unwrap();
    assert_eq!(
        run(&reader, "SELECT count(*) FROM two").await.rows,
        [vec![integer(2)]]
    );
    reader.close().await;
    let listed = OPENED
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| {
            entry
                .path
                .as_deref()
                .is_some_and(|open| open.ends_with("shared.duckdb"))
        })
        .count();
    assert_eq!(listed, 0, "the file was closed with its last connection");

    // Memory is not shared: every connection is a database of its own.
    let memory = Path::new(MEMORY);
    let a = Database::connect(&profile(memory, &[]), None)
        .await
        .unwrap();
    let b = Database::connect(&profile(memory, &[("read_only", "true")]), None)
        .await
        .unwrap();
    run(&a, "CREATE TABLE only_here (x INTEGER)").await;
    assert_eq!(
        fails(&b, "SELECT * FROM only_here").await,
        "duckdb_unknown_object"
    );
    run(&b, "CREATE TABLE writable_in_memory (x INTEGER)").await;
    a.close().await;
    b.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn values_of_every_type_arrive_as_typed_cells() {
    let db = Database::connect(&profile(Path::new(MEMORY), &[]), None)
        .await
        .unwrap();
    run(
        &db,
        "CREATE TABLE kinds (id INTEGER PRIMARY KEY, tiny TINYINT, big BIGINT, huge HUGEINT, \
         uhuge UHUGEINT, ubig UBIGINT, single FLOAT, twice DOUBLE, exact DECIMAL(38,10), \
         short DECIMAL(4,2), yes BOOLEAN, label VARCHAR, document JSON, choice ENUM('a', 'b'), \
         span INTERVAL, list INTEGER[], fixed INTEGER[3], record STRUCT(a INTEGER, b VARCHAR), \
         pairs MAP(VARCHAR, INTEGER), id2 UUID, bytes BLOB, flags BIT, day DATE, clock TIME, \
         moment TIMESTAMP, seconds TIMESTAMP_S, nanos TIMESTAMP_NS, instant TIMESTAMPTZ, \
         either UNION(n INTEGER, s VARCHAR), huge_number BIGNUM); \
         INSERT INTO kinds VALUES (1, -128, -9223372036854775808, \
         -170141183460469231731687303715884105728, 340282366920938463463374607431768211455, \
         18446744073709551615, 0.1, 1.5e300, 12345678901234567890.12, -0.05, true, \
         'Zażółć gęślą jaźń 🦀', '{\"a\":[1,2],\"b\":\"x\"}', 'b', \
         INTERVAL '1 year 2 months 3 days 04:05:06.5', [1, 2, NULL], [1, 2, 3], \
         {'a': 1, 'b': 'x y'}, MAP {'k': 1, 'q': 2}, '01234567-89ab-cdef-0123-456789abcdef', \
         '\\xFF\\x00A'::BLOB, '10101'::BIT, '2024-05-06', '07:08:09.25', \
         '2024-05-06 07:08:09.5', '2024-05-06 07:08:09', '2024-05-06 07:08:09.123456789', \
         '2024-05-06 07:08:09.123456+02', 'str'::VARCHAR, \
         '123456789012345678901234567890123456789012345678901234567890'::BIGNUM); \
         INSERT INTO kinds (id) VALUES (2)",
    )
    .await;
    let result = run(&db, "SELECT * FROM kinds ORDER BY id").await;
    let types: Vec<&str> = result
        .columns
        .iter()
        .map(|column| column.data_type.as_str())
        .collect();
    assert_eq!(
        types,
        [
            "INTEGER",
            "TINYINT",
            "BIGINT",
            "HUGEINT",
            "UHUGEINT",
            "UBIGINT",
            "FLOAT",
            "DOUBLE",
            "DECIMAL(38,10)",
            "DECIMAL(4,2)",
            "BOOLEAN",
            "VARCHAR",
            "JSON",
            "ENUM",
            "INTERVAL",
            "INTEGER[]",
            "INTEGER[]",
            "STRUCT(a INTEGER, b VARCHAR)",
            "MAP(VARCHAR, INTEGER)",
            "UUID",
            "BLOB",
            "BIT",
            "DATE",
            "TIME",
            "TIMESTAMP",
            "TIMESTAMP_S",
            "TIMESTAMP_NS",
            "TIMESTAMP WITH TIME ZONE",
            "UNION(n INTEGER, s VARCHAR)",
            "BIGNUM",
        ]
    );
    let whole = |value: &str| Cell::Integer(value.into());
    assert_eq!(
        result.rows[0],
        [
            whole("1"),
            whole("-128"),
            whole("-9223372036854775808"),
            whole("-170141183460469231731687303715884105728"),
            whole("340282366920938463463374607431768211455"),
            whole("18446744073709551615"),
            Cell::Float("0.1".into()),
            Cell::Float("1.5e300".into()),
            Cell::Decimal("12345678901234567890.1200000000".into()),
            Cell::Decimal("-0.05".into()),
            Cell::Bool(true),
            text("Zażółć gęślą jaźń 🦀"),
            Cell::Json("{\"a\":[1,2],\"b\":\"x\"}".into()),
            text("b"),
            text("1 year 2 months 3 days 04:05:06.5"),
            text("[1, 2, NULL]"),
            text("[1, 2, 3]"),
            text("{'a': 1, 'b': x y}"),
            text("{k=1, q=2}"),
            text("01234567-89ab-cdef-0123-456789abcdef"),
            Cell::Bytes(vec![0xff, 0, b'A']),
            text("10101"),
            text("2024-05-06"),
            text("07:08:09.25"),
            text("2024-05-06 07:08:09.5"),
            text("2024-05-06 07:08:09"),
            text("2024-05-06 07:08:09.123456789"),
            // An instant: shown in UTC with its offset.
            text("2024-05-06 05:08:09.123456+00"),
            text("str"),
            // A form of DuckDB's own is named, never shown as bytes.
            Cell::Unsupported("<BIGNUM>".into()),
        ]
    );
    // Every column of the second row is NULL, whatever its type.
    assert!(result.rows[1][1..].iter().all(|cell| *cell == Cell::Null));
    // The side panel names the types the way DuckDB does.
    let columns = db.columns(&table("kinds")).await.unwrap();
    assert!(columns[0].primary_key && !columns[0].nullable);
    assert_eq!(columns[8].data_type, "DECIMAL(38,10)");
    assert_eq!(columns[13].data_type, "ENUM('a', 'b')");
    assert_eq!(columns[27].data_type, "TIMESTAMP WITH TIME ZONE");
    db.close().await;
}

/// The interface guesses the kind of a typed value from the name of its
/// type, and a grid change is checked against the old value of the cell:
/// both have to work for types that are not plain numbers and texts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changes_fit_the_types_of_their_columns() {
    let db = Database::connect(&profile(Path::new(MEMORY), &[]), None)
        .await
        .unwrap();
    run(
        &db,
        "CREATE TABLE odd (id INTEGER PRIMARY KEY, single FLOAT, span INTERVAL, list INTEGER[], \
         record STRUCT(a INTEGER, b VARCHAR), exact DECIMAL(38,10), uhuge UHUGEINT, \
         instant TIMESTAMPTZ, choice ENUM('a', 'b'), label VARCHAR); \
         INSERT INTO odd VALUES (1, 0.1, INTERVAL '3 days', [1, 2, NULL], {'a': 1, 'b': 'x y'}, \
         20.50, 5, '2024-05-06 07:08:09+02', 'a', 'it''s \\ here')",
    )
    .await;
    let odd = table("odd");
    // "INTERVAL" and "INTEGER[]" hold "INT": the insert dialog hands their
    // values over as integers.
    let insert = RowChange::InsertRow {
        table: odd.clone(),
        values: vec![
            ("id".into(), integer(2)),
            ("span".into(), Cell::Integer("1 day 02:00:00".into())),
            ("list".into(), Cell::Integer("[4, 5]".into())),
            (
                "uhuge".into(),
                Cell::Integer("340282366920938463463374607431768211455".into()),
            ),
            ("single".into(), Cell::Float("2.5".into())),
            (
                "exact".into(),
                Cell::Decimal("12345678901234567890.12".into()),
            ),
        ],
    };
    assert_eq!(
        db.change_text(&insert).unwrap(),
        "INSERT INTO \"main\".\"odd\" (\"id\", \"span\", \"list\", \"uhuge\", \"single\", \"exact\") \
         VALUES (2, '1 day 02:00:00', '[4, 5]', '340282366920938463463374607431768211455', '2.5', '12345678901234567890.12')"
    );
    db.apply(&insert).await.unwrap();
    let before = run(&db, "SELECT * FROM odd ORDER BY id").await;
    assert_eq!(
        before.rows[1][..7],
        [
            integer(2),
            Cell::Float("2.5".into()),
            text("1 day 02:00:00"),
            text("[4, 5]"),
            Cell::Null,
            Cell::Decimal("12345678901234567890.1200000000".into()),
            Cell::Integer("340282366920938463463374607431768211455".into()),
        ]
    );
    // Every cell of the first row is changed with its shown value as the
    // old one: the conflict check has to find the row by it.
    let changes = [
        ("single", Cell::Float("0.25".into())),
        ("span", text("2 years")),
        ("list", text("[7]")),
        ("record", text("{'a': 2, 'b': z}")),
        ("exact", Cell::Decimal("0.0000000001".into())),
        ("uhuge", Cell::Integer("6".into())),
        ("instant", text("2030-01-01 00:00:00+00")),
        ("choice", text("b")),
        ("label", text("new 'text'")),
    ];
    for (index, (column, new)) in changes.iter().enumerate() {
        let edit = RowChange::UpdateCell {
            table: odd.clone(),
            column: (*column).into(),
            old: before.rows[0][index + 1].clone(),
            new: new.clone(),
            key: key(1),
        };
        db.apply(&edit).await.unwrap_or_else(|error| {
            panic!("{column}: {error}: {}", db.change_text(&edit).unwrap())
        });
        // The old value is gone: the same change is a conflict now.
        assert_eq!(
            db.apply(&edit).await.unwrap_err().to_string(),
            "row_changed_reload_required",
            "{column}"
        );
    }
    let after = run(&db, "SELECT * FROM odd WHERE id = 1").await;
    assert_eq!(
        after.rows[0][1..],
        [
            Cell::Float("0.25".into()),
            text("2 years"),
            text("[7]"),
            text("{'a': 2, 'b': z}"),
            Cell::Decimal("0.0000000001".into()),
            Cell::Integer("6".into()),
            text("2030-01-01 00:00:00+00"),
            text("b"),
            text("new 'text'"),
        ]
    );
    // What a column cannot hold is refused, and nothing changes.
    assert_eq!(
        db.apply(&RowChange::UpdateCell {
            table: odd.clone(),
            column: "choice".into(),
            old: text("b"),
            new: text("c"),
            key: key(1),
        })
        .await
        .unwrap_err()
        .to_string(),
        "duckdb_value_rejected"
    );
    assert_eq!(
        db.apply(&RowChange::InsertRow {
            table: odd.clone(),
            values: vec![("id".into(), integer(1))],
        })
        .await
        .unwrap_err()
        .to_string(),
        "duckdb_constraint"
    );
    // A row that takes every default.
    run(
        &db,
        "CREATE SEQUENCE numbers START 10; \
         CREATE TABLE plain (id INTEGER PRIMARY KEY DEFAULT nextval('numbers'), note VARCHAR DEFAULT 'none')",
    )
    .await;
    db.apply(&RowChange::InsertRow {
        table: table("plain"),
        values: Vec::new(),
    })
    .await
    .unwrap();
    assert_eq!(
        run(&db, "SELECT * FROM plain").await.rows,
        [vec![integer(10), text("none")]]
    );
    let columns = db.columns(&table("plain")).await.unwrap();
    assert_eq!(columns[1].default.as_deref(), Some("'none'"));
    assert!(columns[0].default.is_some());
    db.close().await;
}

/// DuckDB reads CSV, Parquet and JSON files in a statement, without an
/// import. A path without a folder is looked for next to the database.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn files_are_queried_directly() {
    let folder = tempfile::tempdir().unwrap();
    let db = open_new(&folder.path().join("files.duckdb")).await;
    std::fs::write(
        folder.path().join("orders.csv"),
        "id,city,total,paid\n1,Gdańsk,318.00,2024-05-06\n2,Łódź,0.50,2024-05-07\n3,\"Kraków, Stare Miasto\",12.25,\n",
    )
    .unwrap();
    let csv = run(&db, "SELECT * FROM 'orders.csv' ORDER BY id").await;
    assert_eq!(
        csv.columns
            .iter()
            .map(|column| (column.name.as_str(), column.data_type.as_str()))
            .collect::<Vec<_>>(),
        [
            ("id", "BIGINT"),
            ("city", "VARCHAR"),
            ("total", "DOUBLE"),
            ("paid", "DATE"),
        ]
    );
    assert_eq!(
        csv.rows[2],
        [
            integer(3),
            text("Kraków, Stare Miasto"),
            Cell::Float("12.25".into()),
            Cell::Null,
        ]
    );
    // The same file with its options spelled out, and an absolute path.
    let absolute = folder.path().join("orders.csv");
    let counted = run(
        &db,
        &format!(
            "SELECT count(*) AS orders, sum(total) AS total FROM read_csv('{}', header = true, delim = ',')",
            absolute.display()
        ),
    )
    .await;
    assert_eq!(
        counted.rows,
        [vec![integer(3), Cell::Float("330.75".into())]]
    );
    // Parquet: written by one statement (to a full path: DuckDB looks next
    // to the database only for what it reads), read by the next, joined
    // with a table.
    let written = folder.path().join("orders.parquet");
    let copied = run(
        &db,
        &format!(
            "COPY (SELECT id, city, total::DECIMAL(12,2) AS total FROM 'orders.csv') TO '{}' (FORMAT parquet); \
             CREATE TABLE regions (city VARCHAR PRIMARY KEY, region VARCHAR); \
             INSERT INTO regions VALUES ('Gdańsk', 'pomorskie'), ('Łódź', 'łódzkie')",
            written.display()
        ),
    )
    .await;
    assert_eq!(copied.affected, 5, "three rows copied, two inserted");
    assert!(written.is_file());
    let parquet = run(
        &db,
        "SELECT o.id, o.total, r.region FROM 'orders.parquet' o LEFT JOIN regions r USING (city) ORDER BY o.id",
    )
    .await;
    assert_eq!(parquet.columns[1].data_type, "DECIMAL(12,2)");
    assert_eq!(
        parquet.rows,
        [
            vec![
                integer(1),
                Cell::Decimal("318.00".into()),
                text("pomorskie")
            ],
            vec![integer(2), Cell::Decimal("0.50".into()), text("łódzkie")],
            vec![integer(3), Cell::Decimal("12.25".into()), Cell::Null],
        ]
    );
    let described = run(&db, "DESCRIBE SELECT * FROM read_parquet('orders.parquet')").await;
    assert_eq!(described.rows.len(), 3);
    // JSON: one document per line, with a nested value.
    std::fs::write(
        folder.path().join("events.json"),
        "{\"id\": 1, \"tags\": [\"a\", \"b\"], \"who\": {\"name\": \"Ola\"}}\n{\"id\": 2, \"tags\": [], \"who\": null}\n",
    )
    .unwrap();
    let json = run(
        &db,
        "SELECT id, tags, who.name AS name FROM read_json('events.json') ORDER BY id",
    )
    .await;
    assert_eq!(
        json.rows,
        [
            vec![integer(1), text("[a, b]"), text("Ola")],
            vec![integer(2), text("[]"), Cell::Null],
        ]
    );
    // A file becomes a table in one statement, and a file that is not there
    // is a failure with a sentence.
    let made = run(
        &db,
        "CREATE TABLE imported AS SELECT * FROM 'orders.parquet'",
    )
    .await;
    assert_eq!(made.affected, 3);
    assert_eq!(
        fails(&db, "SELECT * FROM 'missing.csv'").await,
        "duckdb_io_error"
    );
    assert!(describe("duckdb_io_error", true).is_some());
    db.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_statement_is_stopped_and_a_large_result_is_cut() {
    let folder = tempfile::tempdir().unwrap();
    let db = open_new(&folder.path().join("long.duckdb")).await;
    const SLOW: &str = "SELECT count(*) FROM range(100000000000) t(n) WHERE n % 7 = 3";
    // Cancelled before it starts.
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        db.query("SELECT 1", cancelled)
            .await
            .unwrap_err()
            .to_string(),
        "query_cancelled"
    );
    // Cancelled while it runs: the library is interrupted on its thread.
    let token = CancellationToken::new();
    let worker = db.clone();
    let cancel = token.clone();
    let task = tokio::spawn(async move { worker.query(SLOW, cancel).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!task.is_finished());
    // The editor's connection is busy: another statement gets its own and
    // does not wait.
    let meanwhile = Instant::now();
    assert_eq!(run(&db, "SELECT 41 + 1").await.rows, [vec![integer(42)]]);
    assert!(db.tables().await.unwrap().is_empty());
    assert!(meanwhile.elapsed() < Duration::from_secs(2));
    let asked = Instant::now();
    token.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("the statement was stopped")
        .unwrap();
    assert_eq!(stopped.unwrap_err().to_string(), "query_cancelled");
    assert!(asked.elapsed() < Duration::from_secs(2));
    // A script is stopped inside its statements, and what it did inside its
    // transaction is gone.
    run(&db, "CREATE TABLE marks (n BIGINT)").await;
    let token = CancellationToken::new();
    let worker = db.clone();
    let cancel = token.clone();
    let script = format!(
        "BEGIN; INSERT INTO marks VALUES (1); {SLOW}; INSERT INTO marks VALUES (2); COMMIT"
    );
    let task = tokio::spawn(async move { worker.query(&script, cancel).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    token.cancel();
    assert_eq!(
        task.await.unwrap().unwrap_err().to_string(),
        "query_cancelled"
    );
    assert_eq!(
        run(&db, "SELECT count(*) FROM marks").await.rows,
        [vec![integer(0)]]
    );

    // A result with more rows than a result may hold is cut, and the rest
    // of it is never produced.
    let started = Instant::now();
    let cut = run(
        &db,
        "SELECT n, n::VARCHAR AS label FROM range(100000000000) t(n)",
    )
    .await;
    assert!(cut.truncated);
    assert_eq!(cut.rows.len(), MAX_RESULT_ROWS);
    assert_eq!(
        cut.rows[MAX_RESULT_ROWS - 1][0],
        integer(MAX_RESULT_ROWS as i64 - 1)
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    // A page of the grid is never cut: its limit is part of the statement.
    run(
        &db,
        "CREATE TABLE numbers AS SELECT n::INTEGER AS id FROM range(30000) t(n)",
    )
    .await;
    let page = db
        .read_page(
            &PageRequest {
                limit: 20_000,
                order_by: Some(("id".into(), true)),
                filters: vec![Filter {
                    column: "id".into(),
                    op: FilterOp::Contains,
                    value: text("9"),
                }],
                ..PageRequest::first(table("numbers"))
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(!page.truncated);
    assert_eq!(page.rows[0], [integer(29_999)]);
    db.close().await;
}

/// `SET`, `USE` and temporary tables of one run are there for the next; a
/// transaction is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_editor_keeps_its_session_but_no_open_transaction() {
    let db = Database::connect(&profile(Path::new(MEMORY), &[]), None)
        .await
        .unwrap();
    run(
        &db,
        "CREATE TEMPORARY TABLE scratch AS SELECT 7 AS n; SET VARIABLE answer = 42; CREATE SCHEMA sales; USE sales",
    )
    .await;
    assert_eq!(
        run(
            &db,
            "SELECT n, getvariable('answer'), current_schema() FROM scratch"
        )
        .await
        .rows,
        [vec![integer(7), integer(42), text("sales")]]
    );
    // A temporary table is not something the side panel lists.
    assert!(db.tables().await.unwrap().is_empty());
    run(
        &db,
        "USE main; CREATE TABLE ledger (id INTEGER PRIMARY KEY, amount INTEGER)",
    )
    .await;
    // A transaction that begins and ends in one run is kept.
    let committed = run(
        &db,
        "BEGIN TRANSACTION; INSERT INTO ledger VALUES (1, 10), (2, 20); UPDATE ledger SET amount = amount + 1; COMMIT; SELECT sum(amount) AS total FROM ledger",
    )
    .await;
    assert_eq!(committed.affected, 4);
    assert_eq!(committed.rows, [vec![Cell::Integer("32".into())]]);
    // One that is left open is rolled back, and the run says so.
    assert_eq!(
        fails(&db, "BEGIN; DELETE FROM ledger").await,
        "duckdb_transaction_left_open"
    );
    assert_eq!(
        fails(
            &db,
            "/* later */ START TRANSACTION; INSERT INTO ledger VALUES (3, 30)"
        )
        .await,
        "duckdb_transaction_left_open"
    );
    // A statement that fails inside a transaction leaves none behind either.
    assert_eq!(
        fails(&db, "BEGIN; INSERT INTO ledger VALUES (1, 99); COMMIT").await,
        "duckdb_constraint"
    );
    assert_eq!(
        run(&db, "SELECT count(*), sum(amount) FROM ledger")
            .await
            .rows,
        [vec![integer(2), Cell::Integer("32".into())]]
    );
    // What a statement answers: rows, a count of changed rows, or nothing.
    assert_eq!(
        run(&db, "UPDATE ledger SET amount = 0 WHERE id > 5")
            .await
            .affected,
        0
    );
    let returned = run(&db, "DELETE FROM ledger WHERE id = 2 RETURNING id, amount").await;
    assert_eq!(returned.rows, [vec![integer(2), integer(21)]]);
    let empty = run(&db, "SELECT id, amount FROM ledger WHERE false").await;
    assert_eq!(
        empty.columns.len(),
        2,
        "an empty result still has its columns"
    );
    assert!(empty.rows.is_empty());
    // A column that is merely called "Count" is a result like any other.
    assert_eq!(
        run(&db, "SELECT count(*) AS \"Count\" FROM ledger")
            .await
            .rows,
        [vec![integer(1)]]
    );
    // Failures have codes, never the text of the library.
    for (statement, code) in [
        ("SELEC 1", "duckdb_syntax_error"),
        ("SELECT * FROM nowhere", "duckdb_unknown_object"),
        ("SELECT missing FROM ledger", "duckdb_binder_error"),
        ("SELECT 'x'::INTEGER", "duckdb_value_rejected"),
        ("INSERT INTO ledger VALUES (1, 1)", "duckdb_constraint"),
    ] {
        assert_eq!(fails(&db, statement).await, code, "{statement}");
        assert!(describe(code, false).is_some() && describe(code, true).is_some());
    }
    db.close().await;
}

/// The library, left to its defaults, downloads an extension the moment a
/// statement needs one. The driver turns that off: what the build does not
/// have is installed by the person who asks for it, or not at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_is_installed_behind_the_users_back() {
    let folder = tempfile::tempdir().unwrap();
    for path in [folder.path().join("quiet.duckdb"), PathBuf::from(MEMORY)] {
        let db = open_new(&path).await;
        assert_eq!(
            run(
                &db,
                "SELECT current_setting('autoinstall_known_extensions')"
            )
            .await
            .rows,
            [vec![Cell::Bool(false)]]
        );
        // The readers of JSON and Parquet files are part of the build.
        let built_in = run(
            &db,
            "SELECT extension_name FROM duckdb_extensions() WHERE loaded AND extension_name IN ('json', 'parquet') ORDER BY 1",
        )
        .await;
        assert_eq!(built_in.rows, [vec![text("json")], vec![text("parquet")]]);
        // Time zones are not: the statement works where the person has
        // installed the extension, and says what is missing where not.
        match db
            .query(
                "SELECT (TIMESTAMPTZ '2024-05-06 07:08:09+00' AT TIME ZONE 'Europe/Warsaw')::VARCHAR",
                CancellationToken::new(),
            )
            .await
        {
            Ok(result) => assert_eq!(result.rows, [vec![text("2024-05-06 09:08:09")]]),
            Err(error) => assert_eq!(error.to_string(), "duckdb_extension_missing"),
        }
        db.close().await;
    }
    assert!(describe("duckdb_extension_missing", false).is_some());
}

#[test]
fn a_statement_is_told_by_its_first_word() {
    assert_eq!(first_word("  select 1"), "SELECT");
    assert_eq!(
        first_word("-- why\n/* and how */ WITH x AS (SELECT 1) SELECT * FROM x"),
        "WITH"
    );
    assert_eq!(first_word("(SELECT 1) UNION ALL (SELECT 2)"), "SELECT");
    assert_eq!(first_word("-- only a comment"), "");
    assert!(is_query("FROM") && is_query("SHOW") && !is_query("INSERT") && !is_query(""));
    // A number that is not one, and every number with a fraction, is
    // written as text for DuckDB to cast.
    assert_eq!(fit(&Cell::Integer("12".into())), Cell::Integer("12".into()));
    assert_eq!(fit(&Cell::Integer("1 day".into())), text("1 day"));
    assert_eq!(fit(&Cell::Float("0.1".into())), text("0.1"));
    assert_eq!(fit(&Cell::Decimal("1.50".into())), text("1.50"));
    assert_eq!(fit(&Cell::Null), Cell::Null);
}

#[test]
fn failures_of_the_library_become_result_codes() {
    let failed = |text: &str| {
        failure(&duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error::new(duckdb::ffi::DuckDBError),
            Some(text.to_owned()),
        ))
        .to_string()
    };
    assert_eq!(
        failed("Parser Error: syntax error at or near \"x\""),
        "duckdb_syntax_error"
    );
    assert_eq!(
        failed("Constraint Error: PRIMARY KEY or UNIQUE constraint violation: duplicate key \"1\""),
        "duckdb_constraint"
    );
    assert_eq!(
        failed(
            "Invalid Input Error: Cannot execute statement of type \"INSERT\" on database \"a\" which is attached in read-only mode!"
        ),
        "duckdb_read_only"
    );
    assert_eq!(
        failed(
            "Catalog Error: Scalar Function with name x is not in the catalog, but it exists in the icu extension.\n\nPlease try installing and loading the icu extension:\nINSTALL icu;\nLOAD icu;"
        ),
        "duckdb_extension_missing"
    );
    assert_eq!(
        failed(
            "Extension Autoloading Error: An error occurred while trying to automatically install the required extension 'icu':\nExtension \"/home/x/.duckdb/extensions/v1.5.6/osx_arm64/icu.duckdb_extension\" not found.\nExtension \"icu\" is an existing extension.\n\nInstall it first using \"INSTALL icu\"."
        ),
        "duckdb_extension_missing"
    );
    assert_eq!(
        failed(
            "IO Error: Extension \"/home/x/.duckdb/extensions/v1.5.6/osx_arm64/motherduck.duckdb_extension\" not found.\nExtension \"md\" is an existing extension.\n\nInstall it first using \"INSTALL md\"."
        ),
        "duckdb_extension_missing"
    );
    // A value of the user that reads like the words of the library is
    // still a value.
    assert_eq!(
        failed(
            "Conversion Error: Could not convert string 'attached in read-only mode' to INT32\n\nLINE 1: SELECT 'x'::INT -- \nINSTALL icu;"
        ),
        "duckdb_value_rejected"
    );
    assert_eq!(failed("INTERRUPT Error: Interrupted!"), "query_cancelled");
    assert_eq!(failed("Something of a later version"), "database_error");
    let opened = |text: &str| {
        open_failure(&duckdb::Error::DuckDBFailure(
            duckdb::ffi::Error::new(duckdb::ffi::DuckDBError),
            Some(text.to_owned()),
        ))
        .to_string()
    };
    assert_eq!(
        opened(
            "IO Error: Could not set lock on file \"/x/a.duckdb\": Conflicting lock is held in /usr/bin/duckdb (PID 4242)"
        ),
        "duckdb_file_locked"
    );
    assert_eq!(
        opened(
            "IO Error: The file \"/x/a.duckdb\" exists, but it is not a valid DuckDB database file!"
        ),
        "duckdb_file_invalid"
    );
    assert_eq!(
        opened(
            "IO Error: Trying to read a database file with version number 99, but we can only read versions between 64 and 67."
        ),
        "duckdb_file_version"
    );
    assert_eq!(opened("anything else"), "duckdb_open_failed");
}
