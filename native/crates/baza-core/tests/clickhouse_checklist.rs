//! The driver checklist for ClickHouse: the steps of `driver_checklist.rs`,
//! on a server that has SQL but neither transactions nor unique keys, so
//! that file cannot be run against it as it is (it creates its table with
//! the table designer and expects a duplicate key to fail an import).
//! Ignored by default; a server is described by the same variables:
//!
//! ```text
//! BAZA_DB_TEST_PORT=8123 BAZA_DB_TEST_USER=default BAZA_DB_TEST_DATABASE=bazaqa_checklist \
//! cargo test -p baza-core --test clickhouse_checklist -- --ignored --nocapture
//! ```
//!
//! Required: `BAZA_DB_TEST_PORT` (the HTTP interface) and
//! `BAZA_DB_TEST_DATABASE` (a database the account may create tables in;
//! only tables made by the run are changed, and they are dropped at the
//! end). Optional: `BAZA_DB_TEST_HOST` (default `127.0.0.1`),
//! `BAZA_DB_TEST_USER`, `BAZA_DB_TEST_PASSWORD`, `BAZA_DB_TEST_PRODUCT`
//! (`clickhouse-cloud`), `BAZA_DB_TEST_TLS` (`disable`, the default,
//! `require` or `verify_full`), `BAZA_DB_TEST_CA_FILE` and
//! `BAZA_DB_TEST_OPTIONS` (engine options as `key=value` pairs separated by
//! `;`).
//!
//! How the steps of the SQL checklist hold here. Every step runs; one is
//! not offered and one cannot fail the way it does elsewhere, and the run
//! prints why:
//!
//! | SQL checklist | here |
//! | --- | --- |
//! | 1. table designer | NOT OFFERED (`table_designer` is off): the designer writes no table engine, no `ORDER BY` and no `Nullable(…)`. The table is made with `CREATE TABLE … ENGINE = MergeTree ORDER BY` in the editor, one column of every type of the dialect |
//! | 2. listing | the same, under its database |
//! | 3. columns | the same; the key is the primary key of the table |
//! | 4. import, all or nothing | all or nothing holds for a row the server cannot read. A duplicate key does NOT fail an import: a key is not unique in ClickHouse, which the run shows on a table of its own |
//! | 5. pages | the same; without an order a page follows the key of the table |
//! | 6. "contains" filter | the same eight, and letter case outside ASCII |
//! | 7. one cell, conflict check | the same, through `ALTER TABLE … UPDATE`; a key that finds two rows and a key column are refused with a sentence |
//! | 8. insert, delete | the same, through `INSERT` and a lightweight `DELETE` |
//! | 9. query builder | the same |
//! | 10. monitor | the same |
//! | 11. stopping a statement | the same, checked in `system.processes` |
//!
//! `through_an_ssh_tunnel` needs `BAZA_SSH_TEST_PORT`, `BAZA_SSH_TEST_USER`
//! and `BAZA_SSH_TEST_KEY` as well (an SSH server that reaches the
//! database).
use baza_core::{
    Engine, TunnelManager,
    builder::{Filter, FilterOp, Projection, QueryBuilder},
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
    let engine = Engine::from_id("clickhouse").expect("the ClickHouse driver is registered");
    let product = variable("BAZA_DB_TEST_PRODUCT").unwrap_or_default();
    assert!(
        product.is_empty() || engine.spec().products.iter().any(|p| p.id == product),
        "BAZA_DB_TEST_PRODUCT {product} is not a product of ClickHouse"
    );
    let mut profile = ConnectionProfile {
        name: "Real ClickHouse".into(),
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

fn ids(result: &QueryResult) -> Vec<i64> {
    result
        .rows
        .iter()
        .map(|row| row[0].display().parse().unwrap())
        .collect()
}

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

fn text(value: &str) -> Cell {
    Cell::Text(value.into())
}
fn key(id: i64) -> Vec<(String, Cell)> {
    vec![("id".to_owned(), Cell::Integer(id.to_string()))]
}

/// A statement that runs for a minute and can be stopped every second.
const SLOW: &str = "SELECT sum(sleepEachRow(1)) FROM numbers(60) SETTINGS max_block_size = 1";

/// How many slow statements other requests are running.
async fn running(db: &Database) -> usize {
    let result = run(
        db,
        "SELECT count() FROM system.processes WHERE query LIKE '%sleepEachRow%' AND query NOT LIKE '%system.processes%'",
    )
    .await;
    result.rows[0][0].display().parse().unwrap()
}

async fn whole_table(db: &Database, table: &TableRef) -> QueryResult {
    db.read_page(
        &PageRequest {
            order_by: Some(("id".into(), false)),
            ..PageRequest::first(table.clone())
        },
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

/// The checklist. `name` is the table it creates, `{name}_twins` a second
/// one; the caller drops both.
async fn checklist(db: &Database, profile: &ConnectionProfile, name: &str) {
    let dialect = *db.dialect();
    let label = profile.product_label();
    let caps = db.capabilities();
    let wanted = TableRef {
        schema: profile.database.clone(),
        name: name.to_owned(),
        kind: "table".into(),
    };
    let quoted = sql::qualified(&dialect, &wanted).unwrap();

    // 1. The table designer is not offered, and says so before anything is
    // sent. The table is typed the way a person would type it, with one
    // column of every type the dialect lists.
    assert!(!caps.table_designer, "{label}: the designer is not offered");
    let mut definitions = vec![
        "`id` Int64".to_owned(),
        "`label` Nullable(String)".to_owned(),
        "`amount` Nullable(Decimal(38, 10))".to_owned(),
        "`kind` Nullable(String)".to_owned(),
    ];
    let mut names = vec![
        "id".to_owned(),
        "label".to_owned(),
        "amount".to_owned(),
        "kind".to_owned(),
    ];
    for (index, data_type) in dialect.column_types.iter().enumerate() {
        definitions.push(format!("`c{index}` Nullable({data_type})"));
        names.push(format!("c{index}"));
    }
    run(
        db,
        &format!(
            "CREATE TABLE {quoted} (\n  {}\n) ENGINE = MergeTree ORDER BY `id`",
            definitions.join(",\n  ")
        ),
    )
    .await;
    println!(
        "{label} 1. designer: NOT OFFERED (no table engine, ORDER BY or Nullable(…) in its DDL); CREATE TABLE with {} types of the dialect ran in the editor",
        dialect.column_types.len()
    );

    // 2. The side panel: the table is listed, as a table, once.
    let tables = db.tables().await.unwrap();
    let table = tables
        .iter()
        .find(|table| table.name == name)
        .unwrap_or_else(|| panic!("{label}: the created table is not listed"))
        .clone();
    assert_eq!(table.schema, profile.database, "{label}: its database");
    assert_eq!(table.kind, "table", "{label}: kind");
    assert_eq!(
        tables.iter().filter(|found| found.name == name).count(),
        1,
        "{label}: listed once"
    );
    println!("{label} 2. listing: passes");

    // 3. Its columns, in order, with the key.
    let columns = db.columns(&table).await.unwrap();
    assert_eq!(
        columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        names.iter().map(String::as_str).collect::<Vec<_>>(),
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
    assert_eq!(columns[2].data_type, "Nullable(Decimal(38, 10))");
    println!("{label} 3. columns: passes");

    // 4. Import: 120 rows at once, then a batch with a row the server
    // cannot read, which must leave nothing behind.
    assert!(caps.import, "{label}: import");
    let import_names = ["id", "label", "amount", "kind"].map(str::to_owned);
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
        .import_rows(&table, &import_names, &rows, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(imported.affected, 120, "{label}: imported rows");
    let unreadable = vec![
        vec![
            Cell::Integer("500".into()),
            text("new"),
            Cell::Null,
            Cell::Null,
        ],
        vec![
            Cell::Integer("501".into()),
            text("an amount that is not a number"),
            text("twelve"),
            Cell::Null,
        ],
    ];
    assert_eq!(
        db.import_rows(&table, &import_names, &unreadable, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string(),
        "clickhouse_value_rejected",
        "{label}: a value of the wrong type fails the import"
    );
    // A duplicate key is not a failure on this server: a key orders the
    // rows and finds them, it does not make them unique.
    let twins = TableRef {
        name: format!("{name}_twins"),
        ..wanted.clone()
    };
    run(
        db,
        &format!(
            "CREATE TABLE {} (`id` Int64, `note` String) ENGINE = MergeTree ORDER BY `id`",
            sql::qualified(&dialect, &twins).unwrap()
        ),
    )
    .await;
    db.columns(&twins).await.unwrap();
    let pair = ["id", "note"].map(str::to_owned);
    let same_key = vec![
        vec![Cell::Integer("5".into()), text("first")],
        vec![Cell::Integer("5".into()), text("second")],
    ];
    let accepted = db
        .import_rows(&twins, &pair, &same_key, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(accepted.affected, 2, "{label}: both rows of one key");
    println!(
        "{label} 4. import: 120 rows; all or nothing on an unreadable row passes; a duplicate key is ACCEPTED by the server (keys are not unique), so that part of the step cannot hold"
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
    // Without an order the pages follow the key, so that they do not overlap.
    let mut unordered = PageRequest::first(table.clone());
    unordered.limit = 60;
    assert!(
        db.page_text(&unordered).unwrap().contains("ORDER BY `id`"),
        "{label}: the key orders a page"
    );
    let mut seen = ids(&db
        .read_page(&unordered, CancellationToken::new())
        .await
        .unwrap());
    unordered.offset = 60;
    seen.extend(ids(&db
        .read_page(&unordered, CancellationToken::new())
        .await
        .unwrap()));
    assert_eq!(seen, (1..=120).collect::<Vec<_>>(), "{label}: two pages");
    // What was written is what comes back, character for character.
    let all = whole_table(db, &table).await;
    assert_eq!(all.rows.len(), 120);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(all.rows[index][1], row[1], "{label}: text of row {index}");
        assert!(
            same_number(&all.rows[index][2], &row[2]),
            "{label}: amount of row {index}: {:?}",
            all.rows[index][2]
        );
    }
    // Digits beyond a floating point number are kept.
    assert!(matches!(all.rows[0][2], Cell::Decimal(_)));
    let large = Cell::Decimal("12345678901234567890.12".into());
    println!("{label} 5. pages: passes");

    // 6. The "contains" filter of "Dane tabeli": text, wildcards as plain
    // characters, letter case, a number searched as text.
    let filters = [
        ("label", "gęślą", vec![7]),
        ("label", "100%", vec![9]),
        ("label", "_under_", vec![9]),
        ("label", "\\ back", vec![8]),
        ("label", "it's", vec![8]),
        ("label", "ROW 12", vec![12, 120]),
        ("id", "119", vec![119]),
        ("label", "no such text", vec![]),
        // Letter case is ignored outside ASCII as well.
        ("label", "GĘŚLĄ JAŹŃ", vec![7]),
    ];
    for (column, value, expected) in &filters {
        let request = PageRequest {
            filters: vec![Filter {
                column: (*column).into(),
                op: FilterOp::Contains,
                value: text(value),
            }],
            order_by: Some(("id".into(), false)),
            ..PageRequest::first(table.clone())
        };
        let mut found = db
            .read_page(&request, CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("{label}: {column} contains {value:?}: {error}"));
        assert_eq!(
            &ids(&found),
            expected,
            "{label}: {column} contains {value:?}"
        );
        // A filtered page is still one statement on one table, so the grid
        // keeps editing it; an empty one still shows its columns.
        let statement = db.page_text(&request).unwrap();
        assert_eq!(sql::statements(&dialect, &statement).unwrap().len(), 1);
        let inferred = sql::editable_table(&dialect, &statement, &tables);
        assert!(inferred.is_some(), "{label}: not editable: {statement}");
        sql::name_empty_result(inferred.as_ref(), &columns, &mut found);
        assert!(
            sql::editable_result_table(inferred, &columns, &found).is_some(),
            "{label}: result not editable: {statement}"
        );
    }
    println!("{label} 6. filters: {} pass", filters.len());

    // 7. One cell, with the conflict check.
    assert!(
        caps.edit_cells && caps.insert_rows && caps.delete_rows,
        "{label}: grid changes"
    );
    let edit = RowChange::UpdateCell {
        table: table.clone(),
        column: "label".into(),
        old: text("row 20"),
        new: text("Kopia D:\\zrzut\\ 'x'"),
        key: key(20),
    };
    assert!(
        db.change_text(&edit)
            .unwrap()
            .starts_with(&format!("ALTER TABLE {quoted} UPDATE `label` = ")),
        "{label}: a cell is changed by a mutation"
    );
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
    // What the server does not allow is said in a sentence of the driver:
    // a key column keeps its value, and a key that finds two rows changes
    // neither of them.
    assert_eq!(
        db.apply(&RowChange::UpdateCell {
            table: table.clone(),
            column: "id".into(),
            old: Cell::Integer("30".into()),
            new: Cell::Integer("3000".into()),
            key: key(30),
        })
        .await
        .unwrap_err()
        .to_string(),
        "clickhouse_key_column",
        "{label}: a key column is not updated"
    );
    for change in [
        RowChange::UpdateCell {
            table: twins.clone(),
            column: "note".into(),
            old: text("first"),
            new: text("changed"),
            key: key(5),
        },
        RowChange::DeleteRow {
            table: twins.clone(),
            key: key(5),
        },
    ] {
        // The cell edit names the old value too, which here finds one row;
        // the removal names the key only, which finds two.
        let outcome = db.apply(&change).await;
        match change {
            RowChange::DeleteRow { .. } => assert_eq!(
                outcome.unwrap_err().to_string(),
                "clickhouse_key_not_unique",
                "{label}: a key of two rows removes neither"
            ),
            _ => outcome.unwrap(),
        }
    }
    let both = run(
        db,
        &format!(
            "SELECT note FROM {} ORDER BY note",
            sql::qualified(&dialect, &twins).unwrap()
        ),
    )
    .await;
    assert_eq!(
        both.rows,
        [vec![text("changed")], vec![text("second")]],
        "{label}: the twins after the changes"
    );
    assert!(
        driver_sentence("clickhouse_key_not_unique") && driver_sentence("clickhouse_key_column"),
        "{label}: the refusals have sentences"
    );
    println!(
        "{label} 7. cell edit with conflict check: passes through ALTER TABLE … UPDATE (mutations_sync); a key column and a key of two rows are refused with a sentence"
    );

    // 8. A new row and a removed one.
    db.apply(&RowChange::InsertRow {
        table: table.clone(),
        values: vec![
            ("id".into(), Cell::Integer("200".into())),
            ("label".into(), text("added")),
        ],
    })
    .await
    .unwrap();
    let removal = RowChange::DeleteRow {
        table: table.clone(),
        key: key(1),
    };
    assert!(
        db.change_text(&removal)
            .unwrap()
            .starts_with(&format!("DELETE FROM {quoted} WHERE ")),
        "{label}: a row is removed by a lightweight delete"
    );
    db.apply(&removal).await.unwrap();
    assert_eq!(
        db.apply(&removal).await.unwrap_err().to_string(),
        "row_changed_reload_required",
        "{label}: a row that is gone cannot be removed again"
    );
    // Read back through a page, like the grid does after every change.
    let mut check = whole_table(db, &table).await;
    assert_eq!(
        check.rows.len(),
        120,
        "{label}: 120 rows, one gone, one new"
    );
    check
        .rows
        .retain(|row| ["1", "11", "20", "21", "30", "200"].contains(&row[0].display().as_str()));
    let expected = [
        (11, "row 11", large),
        (20, "Kopia D:\\zrzut\\ 'x'", Cell::Decimal("20.50".into())),
        (21, "row 21", Cell::Null),
        (30, "row 30", Cell::Decimal("30.50".into())),
        (200, "added", Cell::Null),
    ];
    assert_eq!(
        check.rows.len(),
        expected.len(),
        "{label}: rows after the changes"
    );
    for (row, (id, label_text, amount)) in check.rows.iter().zip(&expected) {
        assert_eq!(row[0], Cell::Integer(id.to_string()), "{label}: id");
        assert_eq!(row[1], text(label_text), "{label}: label of {id}");
        assert!(
            same_number(&row[2], amount),
            "{label}: amount of {id}: {:?}",
            row[2]
        );
    }
    println!("{label} 8. insert and delete: passes through INSERT and a lightweight DELETE");

    // 9. The query builder: a count per group, and its "contains" filter.
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
        value: text("kind 2"),
    });
    grouped.group_by.push("kind".into());
    let counted = run(db, &grouped.sql(&dialect).unwrap()).await;
    assert_eq!(counted.rows.len(), 1, "{label}: one group");
    assert_eq!(counted.rows[0][0], text("kind 2"));
    // Ids 2, 6, … 118: thirty rows; the row added by the grid has no kind.
    assert_eq!(counted.rows[0][1].display(), "30", "{label}: group count");
    let mut searched = QueryBuilder::table(table.clone());
    searched.filters.push(Filter {
        column: "label".into(),
        op: FilterOp::Contains,
        value: text("100% _UNDER_"),
    });
    assert_eq!(
        ids(&run(db, &searched.sql(&dialect).unwrap()).await),
        [9],
        "{label}: the builder's contains filter"
    );
    println!("{label} 9. query builder: passes");

    // 10. The "Monitor" section.
    assert!(caps.monitor, "{label}: monitor");
    let stats = db.server_stats().await.unwrap();
    assert_eq!(stats.rows.len(), 1, "{label}: one row of statistics");
    let figure = |name: &str| {
        stats
            .columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(name))
            .map(|index| stats.rows[0][index].display())
    };
    assert!(!figure("version").unwrap_or_default().is_empty());
    assert_eq!(
        figure("database").as_deref(),
        Some(profile.database.as_str())
    );
    assert!(figure("connections").is_some_and(|text| text.parse::<u64>().is_ok_and(|n| n >= 1)));
    assert!(figure("active").is_some_and(|text| text.parse::<u64>().is_ok()));
    println!(
        "{label} 10. monitor: passes ({})",
        stats
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // 11. A running statement is stopped, and the connection goes on.
    let token = CancellationToken::new();
    let worker = db.clone();
    let cancel = token.clone();
    let task = tokio::spawn(async move { worker.query(SLOW, cancel).await });
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        running(db).await,
        1,
        "{label}: the slow statement is running"
    );
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
    let answered = asked.elapsed();
    assert!(
        db.tables()
            .await
            .unwrap()
            .iter()
            .any(|found| found.name == name),
        "{label}: the connection goes on after a cancel"
    );
    // It is gone from the server as well, not only from this side.
    let mut left = running(db).await;
    for _ in 0..20 {
        if left == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        left = running(db).await;
    }
    assert_eq!(
        left, 0,
        "{label}: cancelled statement(s) still run on the server"
    );
    println!(
        "{label} 11. stopping a statement: passes, answered in {} ms and gone from system.processes",
        answered.as_millis()
    );
}

/// Whether a result code of the driver has a sentence in both languages.
fn driver_sentence(code: &str) -> bool {
    [false, true]
        .into_iter()
        .all(|en| baza_core::drivers::describe(code, en).is_some_and(|text| !text.is_empty()))
}

async fn drop_tables(db: &Database, profile: &ConnectionProfile, name: &str) {
    for name in [name.to_owned(), format!("{name}_twins")] {
        let table = TableRef {
            schema: profile.database.clone(),
            name,
            kind: "table".into(),
        };
        let quoted = sql::qualified(db.dialect(), &table).unwrap();
        let _ = db
            .query(
                &format!("DROP TABLE IF EXISTS {quoted}"),
                CancellationToken::new(),
            )
            .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real ClickHouse server described by BAZA_DB_TEST_* variables"]
async fn clickhouse_passes_the_driver_checklist() {
    let profile = profile();
    let db = Database::connect(&profile, None).await.unwrap();
    let name = format!("arch_checklist_{}", uuid::Uuid::new_v4().simple());
    // Run on a task of its own, so that a failed assertion still drops the
    // tables.
    let worker = db.clone();
    let table = name.clone();
    let described = profile.clone();
    let outcome = tokio::spawn(async move { checklist(&worker, &described, &table).await }).await;
    drop_tables(&db, &profile, &name).await;
    let left = db.tables().await.unwrap();
    db.close().await;
    assert!(left.iter().all(|table| !table.name.starts_with(&name)));
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
}

/// The database through the SSH tab of its connection: the tunnel the
/// application opens, the loopback end the driver dials, and the name of
/// the server that stays in every request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a real SSH server and ClickHouse: BAZA_SSH_TEST_* and BAZA_DB_TEST_*"]
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

    let db = Database::connect(&profile, Some(local)).await.unwrap();
    let name = format!("tunnel_{}", uuid::Uuid::new_v4().simple());
    let table = TableRef {
        schema: profile.database.clone(),
        name: name.clone(),
        kind: "table".into(),
    };
    let quoted = sql::qualified(db.dialect(), &table).unwrap();
    run(
        &db,
        &format!(
            "CREATE TABLE {quoted} (`id` Int64, `city` String) ENGINE = MergeTree ORDER BY `id`; \
             INSERT INTO {quoted} VALUES (1, 'Gdańsk'), (2, 'Łódź')"
        ),
    )
    .await;
    let listed = db
        .tables()
        .await
        .unwrap()
        .into_iter()
        .find(|found| found.name == name)
        .expect("the table is listed through the tunnel");
    assert_eq!(db.columns(&listed).await.unwrap().len(), 2);
    let rows = db
        .read_page(
            &PageRequest::first(listed.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(rows.rows[1], [Cell::Integer("2".into()), text("Łódź")]);
    db.apply(&RowChange::UpdateCell {
        table: listed.clone(),
        column: "city".into(),
        old: text("Łódź"),
        new: text("Kraków"),
        key: key(2),
    })
    .await
    .unwrap();
    let changed = run(&db, &format!("SELECT city FROM {quoted} WHERE id = 2")).await;
    assert_eq!(changed.rows, [vec![text("Kraków")]]);
    assert_eq!(db.server_stats().await.unwrap().rows.len(), 1);
    // A statement is stopped through the tunnel as well.
    let token = CancellationToken::new();
    let worker = db.clone();
    let cancel = token.clone();
    let task = tokio::spawn(async move { worker.query(SLOW, cancel).await });
    tokio::time::sleep(Duration::from_millis(600)).await;
    token.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the statement was stopped")
        .unwrap();
    assert_eq!(stopped.unwrap_err().to_string(), "query_cancelled");
    run(&db, &format!("DROP TABLE {quoted}")).await;
    let after = manager.status(profile.id).unwrap();
    println!(
        "ClickHouse over 127.0.0.1:{local}: tunnel sent {} B, received {} B, {} client(s)",
        after.bytes_sent, after.bytes_received, after.active_clients
    );
    // The traffic of the database is the traffic of the tunnel.
    assert!(after.bytes_sent > before.bytes_sent && after.bytes_received > before.bytes_received);
    assert!(after.error.is_none());
    db.close().await;

    // A wrong password is still the answer of the database, and a closed
    // port behind the tunnel is a network failure, not a hang.
    let mut wrong = profile.clone();
    wrong.password.push_str("-wrong");
    assert_eq!(
        Database::connect(&wrong, Some(local))
            .await
            .err()
            .map(|error| error.to_string())
            .as_deref(),
        Some("database_login_rejected")
    );
    manager.stop(profile.id).await.unwrap();
    assert!(
        Database::connect(&profile, Some(local)).await.is_err(),
        "the tunnel is gone with its connection"
    );
    manager.shutdown().await;
}
