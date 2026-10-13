//! What every driver has to get right, checked through the same `Database`
//! the application uses: listing tables and columns, paging, filtering,
//! ordering, changing one cell with the conflict check, adding and removing
//! a row, the query builder, a bulk import that is all or nothing, the
//! statistics of the "Monitor" section, the table designer's column types
//! and stopping a running statement.
//!
//! SQLite runs with every `cargo test`. A server is ignored by default and
//! described by the environment:
//!
//! ```text
//! cargo test -p baza-core --test driver_checklist -- --ignored --nocapture
//! ```
//!
//! Required: `BAZA_DB_TEST_ENGINE` (the stored name of an engine family or
//! one of its aliases), `BAZA_DB_TEST_PORT`, `BAZA_DB_TEST_USER` and
//! `BAZA_DB_TEST_DATABASE` (the account must be allowed to create a table
//! there). Optional: `BAZA_DB_TEST_PRODUCT` (a product id of that family,
//! such as `cockroachdb`), `BAZA_DB_TEST_PASSWORD`, `BAZA_DB_TEST_HOST`
//! (default `127.0.0.1`), `BAZA_DB_TEST_TLS` (`disable`, the default,
//! `require` or `verify_full`), `BAZA_DB_TEST_OPTIONS` (engine options as
//! `key=value` pairs separated by `;`), `BAZA_DB_TEST_SLOW_SQL` (a statement
//! that runs for a minute, to be stopped) and `BAZA_DB_TEST_RUNNING_SQL` (a
//! statement that counts the slow statements other connections are running,
//! to see that a stopped one is gone from the server). Only a table created by the run is
//! changed, and it is dropped at the end.
//!
//! Apart from `CREATE TABLE` and `DROP TABLE`, every step goes through the
//! operations of the facade, so a new engine with SQL passes this file as it
//! is, with its slow statement given in the environment. An engine without
//! SQL gets a checklist of its own next to this one, built from the same
//! steps.
use baza_core::{
    Engine,
    builder::{self, CreateColumn, Filter, FilterOp, Projection, QueryBuilder},
    database::Database,
    drivers::{PageRequest, RowChange},
    engine::DefaultSchema,
    model::{Cell, ConnectionProfile, QueryResult, TableRef, TlsMode},
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

async fn run(db: &Database, statement: &str) -> QueryResult {
    db.query(statement, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("{error}: {statement}"))
}

fn ids(result: &QueryResult) -> Vec<i64> {
    result
        .rows
        .iter()
        .map(|row| row[0].display().parse().unwrap())
        .collect()
}

/// The number in a cell. An engine with exact decimals returns every digit;
/// SQLite keeps a `NUMERIC` value as a floating point number.
fn number(cell: &Cell) -> Option<bigdecimal::BigDecimal> {
    match cell {
        Cell::Decimal(text) | Cell::Float(text) | Cell::Integer(text) => text.parse().ok(),
        _ => None,
    }
}
fn same_number(cell: &Cell, expected: &Cell) -> bool {
    match (number(cell), number(expected)) {
        (Some(found), Some(wanted)) => found == wanted,
        (None, None) => cell == expected,
        _ => false,
    }
}

/// A statement that runs for a long time on the engine, to be stopped:
/// `BAZA_DB_TEST_SLOW_SQL`, otherwise the one of the first three engines.
fn slow_statement(engine: Engine) -> String {
    variable("BAZA_DB_TEST_SLOW_SQL").unwrap_or_else(|| {
        match engine.id() {
            "postgres" => "SELECT pg_sleep(60)",
            "mysql" => "SELECT SLEEP(60)",
            _ => {
                "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM c WHERE n<1000000000) SELECT sum(n) FROM c"
            }
        }
        .to_owned()
    })
}

/// How many slow statements other connections are running, for a server
/// that can say; `None` for one that cannot. `BAZA_DB_TEST_RUNNING_SQL` is a
/// statement that returns that count; without it the first engines are
/// asked in their own way. The server is asked what it is: a profile of the
/// generic product may point at any server of the family.
async fn running(db: &Database, needle: &str) -> Option<usize> {
    let statement = match (variable("BAZA_DB_TEST_RUNNING_SQL"), db.engine().id()) {
        (Some(statement), _) => statement,
        (None, "postgres") => {
            let version = run(db, "SELECT version()").await.rows[0][0].display();
            if version.contains("CockroachDB") {
                format!(
                    "SELECT count(*) FROM [SHOW CLUSTER STATEMENTS] WHERE query LIKE '%{needle}%' AND query NOT LIKE '%CLUSTER STATEMENTS%'"
                )
            } else {
                format!(
                    "SELECT count(*) FROM pg_stat_activity WHERE state = 'active' AND query LIKE '%{needle}%' AND pid <> pg_backend_pid()"
                )
            }
        }
        (None, "mysql") => format!(
            "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE INFO LIKE '%{needle}%' AND ID <> CONNECTION_ID()"
        ),
        (None, _) => return None,
    };
    let result = run(db, &statement).await;
    result.rows[0][0].display().parse().ok()
}

/// The checklist. `name` is the table it creates; the caller drops it.
async fn checklist(db: &Database, profile: &ConnectionProfile, name: &str) {
    let dialect = *db.dialect();
    let label = profile.product_label();
    let caps = db.capabilities();

    // 1. The table designer: one column of every type the dialect offers.
    let schema = match dialect.default_schema {
        DefaultSchema::Fixed(schema) => schema.to_owned(),
        DefaultSchema::Database => profile.database.clone(),
        DefaultSchema::None => String::new(),
    };
    let wanted = TableRef {
        schema,
        name: name.to_owned(),
        kind: "table".into(),
    };
    let text_type = dialect
        .column_types
        .iter()
        .find(|name| **name == "TEXT")
        .or_else(|| {
            dialect
                .column_types
                .iter()
                .find(|name| name.contains("CHAR"))
        })
        .expect("a text type");
    let decimal_type = dialect
        .column_types
        .iter()
        .find(|name| name.contains("DECIMAL") || name.contains("NUMERIC"))
        .expect("a decimal type");
    let mut design = vec![
        CreateColumn {
            name: "id".into(),
            data_type: dialect.column_types[0].into(),
            nullable: false,
            primary_key: true,
        },
        CreateColumn {
            name: "label".into(),
            data_type: (*text_type).into(),
            nullable: true,
            primary_key: false,
        },
        CreateColumn {
            name: "amount".into(),
            data_type: (*decimal_type).into(),
            nullable: true,
            primary_key: false,
        },
        CreateColumn {
            name: "kind".into(),
            data_type: (*text_type).into(),
            nullable: true,
            primary_key: false,
        },
    ];
    for (index, data_type) in dialect.column_types.iter().enumerate() {
        design.push(CreateColumn {
            name: format!("c{index}"),
            data_type: (*data_type).into(),
            nullable: true,
            primary_key: false,
        });
    }
    assert!(caps.table_designer, "{label}: designer");
    run(
        db,
        &builder::create_table(&dialect, &wanted, &design).unwrap(),
    )
    .await;

    // 2. The side panel: the table is listed, as a table.
    let tables = db.tables().await.unwrap();
    let table = tables
        .iter()
        .find(|table| table.name == name)
        .unwrap_or_else(|| panic!("{label}: the created table is not listed"))
        .clone();
    assert!(table.kind.contains("table"), "{label}: kind {}", table.kind);
    assert_eq!(
        tables.iter().filter(|found| found.name == name).count(),
        1,
        "{label}: listed once"
    );

    // 3. Its columns, in order, with the primary key.
    let columns = db.columns(&table).await.unwrap();
    assert_eq!(
        columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        design
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        "{label}: column names"
    );
    assert!(
        columns[0].primary_key && !columns[0].nullable,
        "{label}: key"
    );
    assert!(
        columns[1..]
            .iter()
            .all(|column| !column.primary_key && column.nullable),
        "{label}: the other columns are nullable and not keys"
    );
    assert!(columns.iter().all(|column| !column.data_type.is_empty()));

    // 4. Import: 120 rows at once, then a batch with a duplicate key that
    // must leave nothing behind.
    assert!(caps.import, "{label}: import");
    let names = ["id", "label", "amount", "kind"].map(str::to_owned);
    let rows = (1..=120)
        .map(|n| {
            vec![
                Cell::Integer(n.to_string()),
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
        .import_rows(&table, &names, &rows, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(imported.affected, 120, "{label}: imported rows");
    let clash = vec![
        vec![
            Cell::Integer("500".into()),
            Cell::Text("new".into()),
            Cell::Null,
            Cell::Null,
        ],
        vec![
            Cell::Integer("5".into()),
            Cell::Text("duplicate key".into()),
            Cell::Null,
            Cell::Null,
        ],
    ];
    assert!(
        db.import_rows(&table, &names, &clash, CancellationToken::new())
            .await
            .is_err(),
        "{label}: a duplicate key fails the import"
    );

    // 5. Pages, in order: 50 rows from the 101st leave the last 20; the
    // failed import added nothing.
    assert!(caps.sort && caps.filter, "{label}: sort and filter");
    let mut page = PageRequest::first(table.clone());
    page.order_by = Some(("id".into(), false));
    page.limit = 50;
    let first = db.read_page(&page, CancellationToken::new()).await.unwrap();
    assert_eq!(ids(&first), (1..=50).collect::<Vec<_>>(), "{label}: page 1");
    page.offset = 100;
    let last = db.read_page(&page, CancellationToken::new()).await.unwrap();
    assert_eq!(
        ids(&last),
        (101..=120).collect::<Vec<_>>(),
        "{label}: page 3"
    );
    page.order_by = Some(("id".into(), true));
    page.offset = 0;
    page.limit = 3;
    let descending = db.read_page(&page, CancellationToken::new()).await.unwrap();
    assert_eq!(ids(&descending), [120, 119, 118], "{label}: descending");
    // What was written is what comes back, character for character.
    let all = db
        .read_page(
            &PageRequest {
                order_by: Some(("id".into(), false)),
                ..PageRequest::first(table.clone())
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(all.rows.len(), 120);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(all.rows[index][1], row[1], "{label}: text of row {index}");
        assert!(
            same_number(&all.rows[index][2], &row[2]),
            "{label}: amount of row {index}: {:?}",
            all.rows[index][2]
        );
    }
    // Digits beyond a floating point number, where the engine keeps them.
    let exact = matches!(all.rows[0][2], Cell::Decimal(_));
    let large = Cell::Decimal(
        if exact {
            "12345678901234567890.12"
        } else {
            "1234.25"
        }
        .into(),
    );

    // 6. The "contains" filter of "Dane tabeli": text, wildcards as plain
    // characters, letter case, a number searched as text.
    for (column, value, expected) in [
        ("label", "gęślą", vec![7]),
        ("label", "100%", vec![9]),
        ("label", "_under_", vec![9]),
        ("label", "\\ back", vec![8]),
        ("label", "it's", vec![8]),
        ("label", "ROW 12", vec![12, 120]),
        ("id", "119", vec![119]),
        ("label", "no such text", vec![]),
    ] {
        let request = PageRequest {
            filters: vec![Filter {
                column: column.into(),
                op: FilterOp::Contains,
                value: Cell::Text(value.into()),
            }],
            order_by: Some(("id".into(), false)),
            ..PageRequest::first(table.clone())
        };
        let mut found = db
            .read_page(&request, CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("{label}: {column} contains {value:?}: {error}"));
        assert_eq!(
            ids(&found),
            expected,
            "{label}: {column} contains {value:?}"
        );
        // A filtered page is still one statement on one table, so the grid
        // keeps editing it; an empty one still shows its columns.
        let text = db.page_text(&request).unwrap();
        assert_eq!(sql::statements(&dialect, &text).unwrap().len(), 1);
        let inferred = sql::editable_table(&dialect, &text, &tables);
        assert!(inferred.is_some(), "{label}: not editable: {text}");
        sql::name_empty_result(inferred.as_ref(), &columns, &mut found);
        assert!(
            sql::editable_result_table(inferred, &columns, &found).is_some(),
            "{label}: result not editable: {text}"
        );
    }

    // 7. One cell, with the conflict check.
    assert!(
        caps.edit_cells && caps.insert_rows && caps.delete_rows,
        "{label}: grid changes"
    );
    let key = |id: i64| vec![("id".to_owned(), Cell::Integer(id.to_string()))];
    let edit = RowChange::UpdateCell {
        table: table.clone(),
        column: "label".into(),
        old: Cell::Text("row 20".into()),
        new: Cell::Text("Kopia D:\\zrzut\\ 'x'".into()),
        key: key(20),
    };
    db.apply(&edit).await.unwrap();
    assert_eq!(
        db.apply(&edit).await.unwrap_err().to_string(),
        "row_changed_reload_required",
        "{label}: the same edit again is a conflict"
    );
    // NULL is a value of its own on both sides of an edit.
    db.apply(&RowChange::UpdateCell {
        table: table.clone(),
        column: "amount".into(),
        old: Cell::Null,
        new: large.clone(),
        key: key(11),
    })
    .await
    .unwrap();
    db.apply(&RowChange::UpdateCell {
        table: table.clone(),
        column: "amount".into(),
        old: all.rows[20][2].clone(),
        new: Cell::Null,
        key: key(21),
    })
    .await
    .unwrap();

    // 8. A new row, with values and with defaults only, and a removed one.
    db.apply(&RowChange::InsertRow {
        table: table.clone(),
        values: vec![
            ("id".into(), Cell::Integer("200".into())),
            ("label".into(), Cell::Text("added".into())),
        ],
    })
    .await
    .unwrap();
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
        "{label}: a row that is gone cannot be removed again"
    );
    // Read back through a page, like the grid does after every change.
    let mut check = db
        .read_page(
            &PageRequest {
                order_by: Some(("id".into(), false)),
                ..PageRequest::first(table.clone())
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        check.rows.len(),
        120,
        "{label}: 120 rows, one gone, one new"
    );
    check
        .rows
        .retain(|row| ["1", "11", "20", "21", "200"].contains(&row[0].display().as_str()));
    let expected = [
        (11, "row 11", large),
        (20, "Kopia D:\\zrzut\\ 'x'", Cell::Decimal("20.50".into())),
        (21, "row 21", Cell::Null),
        (200, "added", Cell::Null),
    ];
    assert_eq!(
        check.rows.len(),
        expected.len(),
        "{label}: rows after the changes"
    );
    for (row, (id, text, amount)) in check.rows.iter().zip(&expected) {
        assert_eq!(row[0], Cell::Integer(id.to_string()), "{label}: id");
        assert_eq!(row[1], Cell::Text((*text).into()), "{label}: label of {id}");
        assert!(
            same_number(&row[2], amount),
            "{label}: amount of {id}: {:?}",
            row[2]
        );
    }

    // 9. The query builder: a count per group.
    assert!(caps.query_builder, "{label}: builder");
    let mut grouped = QueryBuilder::table(table.clone());
    grouped.projections.push(Projection {
        column: "kind".into(),
        aggregate: None,
        alias: None,
    });
    grouped.projections.push(Projection {
        column: "*".into(),
        aggregate: Some("COUNT".into()),
        alias: Some("result".into()),
    });
    grouped.filters.push(Filter {
        column: "kind".into(),
        op: FilterOp::Equal,
        value: Cell::Text("kind 2".into()),
    });
    grouped.group_by.push("kind".into());
    let counted = run(db, &grouped.sql(&dialect).unwrap()).await;
    assert_eq!(counted.rows.len(), 1, "{label}: one group");
    assert_eq!(counted.rows[0][0], Cell::Text("kind 2".into()));
    // Ids 2, 6, … 118: thirty rows; the row added by the grid has no kind.
    assert_eq!(counted.rows[0][1].display(), "30", "{label}: group count");

    // 10. The "Monitor" section.
    assert!(caps.monitor, "{label}: monitor");
    let stats = db.server_stats().await.unwrap();
    assert_eq!(stats.rows.len(), 1, "{label}: one row of statistics");
    let version = stats
        .columns
        .iter()
        .position(|column| column.name.eq_ignore_ascii_case("version"))
        .unwrap_or_else(|| panic!("{label}: no version in the statistics"));
    assert!(!stats.rows[0][version].display().is_empty());

    // 11. A running statement is stopped, and the connection goes on.
    let token = CancellationToken::new();
    let worker = db.clone();
    let cancel = token.clone();
    let slow = slow_statement(db.engine());
    let needle = slow.split('(').next().unwrap_or(&slow).to_owned();
    let needle = needle.as_str();
    let task = tokio::spawn(async move { worker.query(&slow, cancel).await });
    tokio::time::sleep(Duration::from_millis(400)).await;
    if let Some(count) = running(db, needle).await {
        assert_eq!(count, 1, "{label}: the slow statement is running");
    }
    let asked = Instant::now();
    token.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap_or_else(|_| panic!("{label}: the statement was not stopped"))
        .unwrap();
    assert_eq!(
        stopped.unwrap_err().to_string(),
        "query_cancelled",
        "{label}: cancel"
    );
    assert!(
        db.tables()
            .await
            .unwrap()
            .iter()
            .any(|found| found.name == name),
        "{label}: the connection goes on after a cancel"
    );
    // It is gone from the server as well, not only from this side.
    let mut left = running(db, needle).await;
    for _ in 0..20 {
        if left.is_none_or(|count| count == 0) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        left = running(db, needle).await;
    }
    assert!(
        left.is_none_or(|count| count == 0),
        "{label}: {left:?} cancelled statement(s) still run on the server"
    );
    println!(
        "{label}: designer ({} types), listing, columns, import, paging, 8 filters, cell edit with conflict check, insert, delete, builder, monitor ({} columns), cancel in {} ms",
        dialect.column_types.len(),
        stats.columns.len(),
        asked.elapsed().as_millis()
    );
}

async fn checked(db: Database, profile: ConnectionProfile) {
    let name = format!("arch_checklist_{}", uuid::Uuid::new_v4().simple());
    // Run on a task of its own, so that a failed assertion still drops the table.
    let worker = db.clone();
    let table = name.clone();
    let outcome = tokio::spawn(async move { checklist(&worker, &profile, &table).await }).await;
    if let Some(found) = db
        .tables()
        .await
        .unwrap()
        .into_iter()
        .find(|table| table.name == name)
    {
        let quoted = sql::qualified(db.dialect(), &found).unwrap();
        let _ = db
            .query(&format!("DROP TABLE {quoted}"), CancellationToken::new())
            .await;
    }
    let left = db.tables().await.unwrap();
    db.close().await;
    assert!(left.iter().all(|table| table.name != name));
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_passes_the_driver_checklist() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("checklist.sqlite");
    std::fs::File::create(&path).unwrap();
    let profile = ConnectionProfile {
        name: "Owned checklist fixture".into(),
        engine: baza_core::drivers::sqlite::ENGINE,
        sqlite_path: Some(path),
        ..ConnectionProfile::default()
    };
    let db = Database::connect(&profile, None).await.unwrap();
    checked(db, profile).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real database server described by BAZA_DB_TEST_* variables"]
async fn server_passes_the_driver_checklist() {
    let engine = required("BAZA_DB_TEST_ENGINE");
    let engine = Engine::from_id(&engine)
        .unwrap_or_else(|| panic!("BAZA_DB_TEST_ENGINE {engine} is not a registered engine"));
    let product = variable("BAZA_DB_TEST_PRODUCT").unwrap_or_default();
    assert!(
        product.is_empty() || engine.spec().products.iter().any(|p| p.id == product),
        "BAZA_DB_TEST_PRODUCT {product} is not a product of {}",
        engine.id()
    );
    let mut profile = ConnectionProfile {
        name: "Real database".into(),
        engine,
        product,
        host: variable("BAZA_DB_TEST_HOST").unwrap_or_else(|| "127.0.0.1".into()),
        port: required("BAZA_DB_TEST_PORT").parse().unwrap(),
        database: required("BAZA_DB_TEST_DATABASE"),
        username: required("BAZA_DB_TEST_USER"),
        password: variable("BAZA_DB_TEST_PASSWORD").unwrap_or_default(),
        tls: match variable("BAZA_DB_TEST_TLS").as_deref() {
            None | Some("disable") => TlsMode::Disable,
            Some("require") => TlsMode::Require,
            Some("verify_full") => TlsMode::VerifyFull,
            Some(other) => panic!("BAZA_DB_TEST_TLS {other} is not a TLS mode"),
        },
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
    let db = Database::connect(&profile, None).await.unwrap();
    checked(db, profile).await;
}
