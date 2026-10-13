//! The "contains" filter of a table and single-cell editing of its filtered
//! rows, checked on real engines. SQLite runs with every `cargo test`; the
//! server engines are ignored by default and described by the environment:
//!
//! ```text
//! cargo test -p baza-core --test real_database -- --ignored --nocapture
//! ```
//!
//! Required: `BAZA_DB_TEST_ENGINE` (`mysql` or `postgres`),
//! `BAZA_DB_TEST_PORT`, `BAZA_DB_TEST_USER`, `BAZA_DB_TEST_PASSWORD` and
//! `BAZA_DB_TEST_DATABASE` (the account must be allowed to create a table
//! there). Optional: `BAZA_DB_TEST_HOST` (default `127.0.0.1`). Only a table
//! created by the run is changed, and it is dropped at the end.
use baza_core::{
    builder::{Filter, FilterOp, QueryBuilder},
    database::Database,
    drivers::{mysql, postgres, sqlite},
    model::{Cell, ConnectionProfile, TableRef, TlsMode},
    sql,
};
use tokio_util::sync::CancellationToken;

fn variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}
fn required(name: &str) -> String {
    variable(name).unwrap_or_else(|| panic!("{name} is required"))
}

async fn run(db: &Database, statement: &str) -> baza_core::QueryResult {
    db.query(statement, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("{error}: {statement}"))
}

/// Values a filter must find literally: every character that means something
/// to LIKE or to a string literal of one of the engines.
const NAMES: [&str; 9] = [
    "Zielony Ogród",
    "100% Bawełna",
    "pod_kreska",
    "Archiwum C:\\dane",
    "Hurra! Zabawki",
    "It's \"quoted\"",
    "podXkreska",
    "100 procent",
    "ZIELONY las",
];

/// Create the table `name`, filter it the way "Dane tabeli" does and edit a
/// filtered row. The caller drops the table.
async fn filters_and_edits(db: &Database, name: &str) {
    let engine = db.engine();
    let dialect = db.dialect();
    let quoted = sql::quote_identifier(dialect, name).unwrap();
    // A collation other than the default of the connection, where the engine
    // has such a thing: the generated literals must still compare.
    let text = if engine == mysql::ENGINE {
        "VARCHAR(80) CHARACTER SET utf8mb4 COLLATE utf8mb4_polish_ci"
    } else {
        "VARCHAR(80)"
    };
    run(
        db,
        &format!(
            "CREATE TABLE {quoted} (id INTEGER NOT NULL PRIMARY KEY, name {text} NOT NULL, amount DECIMAL(12,2) NOT NULL)"
        ),
    )
    .await;
    let table = db
        .tables()
        .await
        .unwrap()
        .into_iter()
        .find(|table| table.name == name)
        .expect("the created table is listed");
    let rows = NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| {
            vec![
                Cell::Integer((index + 1).to_string()),
                Cell::Text((*name).into()),
                Cell::Decimal(format!("{}.45", (index + 1) * 100)),
            ]
        })
        .collect::<Vec<_>>();
    db.import_rows(
        &table,
        &["id".into(), "name".into(), "amount".into()],
        &rows,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let tables = db.tables().await.unwrap();
    let columns = db.columns(&table).await.unwrap();
    assert!(columns[0].primary_key);

    // The query "Dane tabeli" runs for a filter, and the ids it returns.
    let filtered = |column: &str, value: &str| {
        let mut builder = QueryBuilder::table(table.clone());
        builder.filters.push(Filter {
            column: column.into(),
            op: FilterOp::Contains,
            value: Cell::Text(value.into()),
        });
        builder.order_by = Some(("id".into(), false));
        builder.sql(dialect).unwrap()
    };
    let ids = |result: &baza_core::QueryResult| {
        result
            .rows
            .iter()
            .map(|row| row[0].display().parse::<u32>().unwrap())
            .collect::<Vec<_>>()
    };
    for (column, value, expected) in [
        ("name", "Ogr", vec![1]),
        // `%` and `_` are characters here, not wildcards.
        ("name", "100%", vec![2]),
        ("name", "d_k", vec![3]),
        // A backslash is a character too, on its own and before a letter.
        ("name", "C:\\d", vec![4]),
        ("name", "\\", vec![4]),
        // So is the escape character of the generated pattern.
        ("name", "a!", vec![5]),
        ("name", "!", vec![5]),
        ("name", "It's", vec![6]),
        ("name", "\"quoted\"", vec![6]),
        // Letter case does not matter, as in the other two engines.
        ("name", "zielony", vec![1, 9]),
        ("name", "ogró", vec![1]),
        ("name", "nothing like this", vec![]),
        // Numbers are searched as text.
        ("id", "9", vec![9]),
        ("amount", "00.4", vec![1, 2, 3, 4, 5, 6, 7, 8, 9]),
        ("amount", "300.45", vec![3]),
    ] {
        let statement = filtered(column, value);
        let mut result = run(db, &statement).await;
        assert_eq!(
            ids(&result),
            expected,
            "{engine:?} {column} contains {value:?}"
        );
        // The filtered rows stay editable: one statement on one known table,
        // with the columns of that table.
        assert_eq!(sql::statements(dialect, &statement).unwrap().len(), 1);
        let inferred = sql::editable_table(dialect, &statement, &tables);
        assert!(
            inferred.as_ref().is_some_and(|found| found.name == name),
            "{engine:?} not editable: {statement}"
        );
        // A filter without a match still shows the columns of the table.
        sql::name_empty_result(inferred.as_ref(), &columns, &mut result);
        assert_eq!(result.columns.len(), 3);
        assert!(
            sql::editable_result_table(inferred, &columns, &result).is_some(),
            "{engine:?} result not editable: {statement}"
        );
    }

    // Filter, then change one cell of the row that was found. The new value
    // ends in a backslash, the hardest thing to put into a literal.
    let found = run(db, &filtered("name", "C:\\d")).await;
    assert_eq!(found.rows.len(), 1);
    let key = [("id".to_owned(), found.rows[0][0].clone())];
    let update = sql::update_cell(
        dialect,
        &table,
        "name",
        &found.rows[0][1],
        &Cell::Text("Kopia D:\\zrzut\\".into()),
        &key,
    )
    .unwrap();
    db.mutate_one(&update).await.unwrap();
    // The same edit again no longer matches the old value.
    assert_eq!(
        db.mutate_one(&update).await.unwrap_err().to_string(),
        "row_changed_reload_required"
    );
    let after = run(db, &filtered("name", "zrzut\\")).await;
    assert_eq!(ids(&after), [4]);
    assert_eq!(after.rows[0][1], Cell::Text("Kopia D:\\zrzut\\".into()));
    // Every other row is as it was.
    let all = run(db, &format!("SELECT id, name FROM {quoted} ORDER BY id")).await;
    for (index, row) in all.rows.iter().enumerate() {
        if index != 3 {
            assert_eq!(row[1], Cell::Text(NAMES[index].into()));
        }
    }
    println!(
        "{engine:?}: 15 filters returned the expected rows and stayed editable; a filtered cell was changed"
    );
}

async fn checked(db: Database, name: String) {
    let quoted = sql::quote_identifier(db.dialect(), &name).unwrap();
    // Run on a task of its own, so that a failed assertion still drops the table.
    let worker = db.clone();
    let table = name.clone();
    let outcome = tokio::spawn(async move { filters_and_edits(&worker, &table).await }).await;
    let _ = db
        .query(&format!("DROP TABLE {quoted}"), CancellationToken::new())
        .await;
    let left = db.tables().await.unwrap();
    db.close().await;
    assert!(left.iter().all(|table: &TableRef| table.name != name));
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
}

fn table_name() -> String {
    format!("baza_filter_qa_{}", uuid::Uuid::new_v4().simple())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_filters_find_literal_text_and_filtered_rows_stay_editable() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("filters.sqlite");
    std::fs::File::create(&path).unwrap();
    let profile = ConnectionProfile {
        name: "Owned filter fixture".into(),
        engine: sqlite::ENGINE,
        sqlite_path: Some(path),
        ..ConnectionProfile::default()
    };
    let db = Database::connect(&profile, None).await.unwrap();
    checked(db, table_name()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real MariaDB/MySQL or PostgreSQL server described by BAZA_DB_TEST_* variables"]
async fn server_filters_find_literal_text_and_filtered_rows_stay_editable() {
    let engine = match required("BAZA_DB_TEST_ENGINE").as_str() {
        "mysql" | "mariadb" => mysql::ENGINE,
        "postgres" | "postgresql" => postgres::ENGINE,
        other => panic!("BAZA_DB_TEST_ENGINE {other} is not mysql or postgres"),
    };
    let profile = ConnectionProfile {
        name: "Real database".into(),
        engine,
        host: variable("BAZA_DB_TEST_HOST").unwrap_or_else(|| "127.0.0.1".into()),
        port: required("BAZA_DB_TEST_PORT").parse().unwrap(),
        database: required("BAZA_DB_TEST_DATABASE"),
        username: required("BAZA_DB_TEST_USER"),
        password: required("BAZA_DB_TEST_PASSWORD"),
        tls: TlsMode::Disable,
        ..ConnectionProfile::default()
    };
    let db = Database::connect(&profile, None).await.unwrap();
    checked(db, table_name()).await;
}
