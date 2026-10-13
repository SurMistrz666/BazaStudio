//! The driver checklist for Apache Cassandra and the servers that speak CQL,
//! through the same `Database` the application uses.
//!
//! It follows `driver_checklist.rs` step by step. CQL is edited as SQL, but
//! that file cannot run here as it is: it creates its table with the shared
//! table designer (`NOT NULL`) and counts with the shared query builder
//! (`GROUP BY` on any column), and it asks for a row that takes only
//! defaults. Each of those steps is answered here by what CQL can do, and
//! the step says so.
//!
//! ```text
//! BAZA_DB_TEST_PORT=39042 cargo test -p baza-core --test cassandra_checklist -- --ignored --nocapture
//! ```
//!
//! Required: `BAZA_DB_TEST_PORT`. Optional: `BAZA_DB_TEST_HOST` (default
//! `127.0.0.1`), `BAZA_DB_TEST_USER`, `BAZA_DB_TEST_PASSWORD`,
//! `BAZA_DB_TEST_PRODUCT` (a product id of the family), `BAZA_DB_TEST_TLS`
//! (`disable`, the default, `require` or `verify_full`),
//! `BAZA_DB_TEST_OPTIONS` (`key=value` pairs separated by `;`) and
//! `BAZA_DB_TEST_DATABASE` (the keyspace to work in, default
//! `bazaqa_checklist`; it is created when missing, with one replica). Only
//! tables created by the run are changed, and they are dropped at the end.
//!
//! `cassandra_through_an_ssh_tunnel` also needs `BAZA_SSH_TEST_PORT`,
//! `BAZA_SSH_TEST_USER` and `BAZA_SSH_TEST_KEY`. `tls_is_encrypted_and_verified`
//! needs no server with TLS: the test puts a TLS endpoint of its own in
//! front of the plain one, with a certificate for the name `localhost`
//! (`BAZA_DB_TEST_TLS_CERT`, `BAZA_DB_TEST_TLS_KEY`) and the CA that signed
//! it (`BAZA_DB_TEST_TLS_CA`), all PEM files.
use baza_core::{
    builder::{Filter, FilterOp},
    database::Database,
    drivers::{PageRequest, RowChange, cassandra},
    model::{
        Cell, ConnectionProfile, QueryResult, SshAuth, SshProfile, TableRef, TlsMode, TunnelProfile,
    },
    sql,
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

fn keyspace() -> String {
    variable("BAZA_DB_TEST_DATABASE").unwrap_or_else(|| "bazaqa_checklist".into())
}

fn profile() -> ConnectionProfile {
    let mut profile = ConnectionProfile {
        name: "Real Cassandra".into(),
        engine: cassandra::ENGINE,
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

async fn failure(db: &Database, statement: &str) -> String {
    db.query(statement, CancellationToken::new())
        .await
        .err()
        .map(|error| error.to_string())
        .unwrap_or_else(|| panic!("no error: {statement}"))
}

/// Connect and make sure the keyspace of the run exists.
async fn connect(profile: &ConnectionProfile) -> Database {
    let db = Database::connect(profile, None).await.unwrap();
    run(
        &db,
        &format!(
            "CREATE KEYSPACE IF NOT EXISTS \"{}\" WITH replication = {{'class': 'SimpleStrategy', 'replication_factor': 1}}",
            keyspace()
        ),
    )
    .await;
    db
}

fn table(name: &str) -> TableRef {
    TableRef {
        schema: keyspace(),
        name: name.to_owned(),
        kind: "table".into(),
    }
}

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}

fn text(value: &str) -> Cell {
    Cell::Text(value.into())
}

fn integer(value: i64) -> Cell {
    Cell::Integer(value.to_string())
}

fn contains(column: &str, value: &str) -> Filter {
    Filter {
        column: column.into(),
        op: FilterOp::Contains,
        value: text(value),
    }
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
        .unwrap_or_else(|| panic!("no column {name}"))
}

fn ids(result: &QueryResult) -> Vec<i64> {
    let id = at(result, "id");
    result
        .rows
        .iter()
        .map(|row| row[id].display().parse().unwrap())
        .collect()
}

fn code(result: anyhow::Result<()>) -> String {
    result
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default()
}

fn same_number(cell: &Cell, expected: &Cell) -> bool {
    let number = |cell: &Cell| match cell {
        Cell::Decimal(text) | Cell::Float(text) | Cell::Integer(text) => {
            text.parse::<bigdecimal::BigDecimal>().ok()
        }
        _ => None,
    };
    match (number(cell), number(expected)) {
        (Some(found), Some(wanted)) => found == wanted,
        (None, None) => cell == expected,
        _ => false,
    }
}

/// Drop the tables a test made, whatever happened in it.
async fn dropped(db: &Database, names: &[String]) {
    for name in names {
        let _ = db
            .query(
                &format!("DROP TABLE IF EXISTS \"{}\".\"{name}\"", keyspace()),
                CancellationToken::new(),
            )
            .await;
    }
}

/// The steps of `driver_checklist.rs`, each as far as CQL goes.
async fn checklist(db: &Database, profile: &ConnectionProfile, name: &str) {
    let dialect = *db.dialect();
    let label = profile.product_label();
    let caps = db.capabilities();
    let ks = keyspace();

    // 1. The table designer. CAPABILITY-LIMITED: the shared generator writes
    // `NOT NULL` after every key column, which CQL does not have, so the
    // designer is off and the table is made with a statement of the editor.
    assert!(!caps.table_designer, "{label}: the designer is off");
    assert!(
        baza_core::builder::create_table(
            &dialect,
            &table(name),
            &[baza_core::builder::CreateColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
                primary_key: true,
            }]
        )
        .unwrap()
        .contains("NOT NULL"),
        "the shared designer still writes what CQL refuses"
    );
    run(
        db,
        &format!(
            "CREATE TABLE \"{ks}\".\"{name}\" (id int PRIMARY KEY, label text, amount decimal, kind text)"
        ),
    )
    .await;

    // 2. The side panel: the table is listed, once, as a table of its
    // keyspace.
    let tables = db.tables().await.unwrap();
    let listed: Vec<&TableRef> = tables.iter().filter(|found| found.name == name).collect();
    assert_eq!(listed.len(), 1, "{label}: listed once");
    let table = listed[0].clone();
    assert_eq!(
        (table.schema.as_str(), table.kind.as_str()),
        (ks.as_str(), "table")
    );
    // The keyspaces of the server itself stay out of the tree by default.
    assert!(
        tables
            .iter()
            .all(|found| !found.schema.starts_with("system")),
        "{label}: system keyspaces are hidden"
    );

    // 3. Its columns: the key first, then the others by name, as SELECT *
    // returns them; each says its type and its part in the key.
    let columns = db.columns(&table).await.unwrap();
    assert_eq!(
        columns
            .iter()
            .map(|column| (column.name.as_str(), column.data_type.as_str()))
            .collect::<Vec<_>>(),
        [
            ("id", "int · partition key"),
            ("amount", "decimal"),
            ("kind", "text"),
            ("label", "text"),
        ],
        "{label}: columns"
    );
    assert!(
        columns[0].primary_key && !columns[0].nullable,
        "{label}: key"
    );
    assert!(
        columns[1..]
            .iter()
            .all(|column| !column.primary_key && column.nullable)
    );

    // 4. Import: 120 rows at once, then a batch with a key that exists,
    // which must leave nothing behind. A plain INSERT of CQL would have
    // overwritten row 5 silently.
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
        vec![integer(500), text("new"), Cell::Null, Cell::Null],
        vec![integer(5), text("duplicate key"), Cell::Null, Cell::Null],
    ];
    assert_eq!(
        db.import_rows(&table, &names, &clash, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string(),
        "cassandra_import_key_exists",
        "{label}: a key that exists fails the import"
    );
    let twice = vec![clash[0].clone(), clash[0].clone()];
    assert_eq!(
        db.import_rows(&table, &names, &twice, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string(),
        "cassandra_import_duplicate"
    );

    // 5. Pages. CQL has no OFFSET and orders rows only inside a partition:
    // the order is made by the driver on up to 20 000 rows, and a page
    // without an order is read forward from where the page before ended.
    assert!(caps.sort && caps.filter, "{label}: sort and filter");
    let mut request = PageRequest::first(table.clone());
    request.order_by = Some(("id".into(), false));
    request.limit = 50;
    assert_eq!(
        ids(&page(db, &request).await),
        (1..=50).collect::<Vec<_>>(),
        "{label}: page 1"
    );
    request.offset = 100;
    assert_eq!(
        ids(&page(db, &request).await),
        (101..=120).collect::<Vec<_>>(),
        "{label}: page 3"
    );
    request.order_by = Some(("id".into(), true));
    request.offset = 0;
    request.limit = 3;
    assert_eq!(
        ids(&page(db, &request).await),
        [120, 119, 118],
        "{label}: descending"
    );
    // Without an order: three pages hold every row once, the fourth is
    // empty, and a page read again holds the same rows. The failed imports
    // added nothing.
    let mut forward = PageRequest::first(table.clone());
    forward.limit = 50;
    let mut seen = Vec::new();
    let mut second = Vec::new();
    for index in 0..4u64 {
        forward.offset = index * 50;
        let read = page(db, &forward).await;
        assert_eq!(
            read.rows.len(),
            [50, 50, 20, 0][index as usize],
            "{label}: page {index}"
        );
        assert!(!read.truncated);
        assert_eq!(
            read.columns.len(),
            4,
            "{label}: an empty page keeps its columns"
        );
        if index == 1 {
            second = ids(&read);
        }
        seen.extend(ids(&read));
    }
    assert_eq!(
        seen.iter().copied().collect::<BTreeSet<_>>(),
        (1..=120).collect::<BTreeSet<_>>(),
        "{label}: every row once"
    );
    assert_eq!(seen.len(), 120);
    forward.offset = 50;
    assert_eq!(
        ids(&page(db, &forward).await),
        second,
        "{label}: page 2 again"
    );
    forward.offset = 0;
    assert_eq!(
        ids(&page(db, &forward).await),
        seen[..50],
        "{label}: back to page 1"
    );
    // A page nobody visited is reached by reading up to it: a view of its
    // own (another page size) asked for its third page first.
    let mut jump = PageRequest::first(table.clone());
    jump.limit = 40;
    jump.offset = 80;
    assert_eq!(
        ids(&page(db, &jump).await),
        seen[80..],
        "{label}: a page reached by reading"
    );
    // What was written is what comes back, character for character.
    let all = page(
        db,
        &PageRequest {
            order_by: Some(("id".into(), false)),
            ..PageRequest::first(table.clone())
        },
    )
    .await;
    assert_eq!(all.rows.len(), 120);
    let (label_at, amount_at) = (at(&all, "label"), at(&all, "amount"));
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(
            all.rows[index][label_at], row[1],
            "{label}: text of row {index}"
        );
        assert!(
            same_number(&all.rows[index][amount_at], &row[2]),
            "{label}: amount of row {index}: {:?}",
            all.rows[index][amount_at]
        );
    }
    // The digits of a decimal are kept as they were written.
    assert_eq!(all.rows[0][amount_at], Cell::Decimal("1.50".into()));
    assert_eq!(all.columns[at(&all, "id")].data_type, "int · partition key");
    let large = Cell::Decimal("12345678901234567890.12".into());

    // 6. The "contains" filter. CQL cannot search for a text, so the rows
    // are read in stored order and checked by the driver (at most 100 000 a
    // page); a value of the partition key is asked as an exact key first.
    for (column, value, expected) in [
        ("label", "gęślą", vec![7]),
        ("label", "100%", vec![9]),
        ("label", "_under_", vec![9]),
        ("label", "\\ back", vec![8]),
        ("label", "it's", vec![8]),
        ("label", "ROW 12", vec![12, 120]),
        ("id", "119", vec![119]),
        ("label", "no such text", vec![]),
        // No row has the key 500: the ids are searched as text instead.
        ("id", "500", vec![]),
        (
            "amount",
            "7.5",
            vec![7, 17, 27, 37, 47, 57, 67, 77, 87, 97, 107, 117],
        ),
    ] {
        let request = PageRequest {
            filters: vec![contains(column, value)],
            order_by: Some(("id".into(), false)),
            ..PageRequest::first(table.clone())
        };
        let mut found = page(db, &request).await;
        assert_eq!(
            ids(&found),
            expected,
            "{label}: {column} contains {value:?}"
        );
        // The same without an order, as the grid first asks.
        let plain = PageRequest {
            order_by: None,
            ..request.clone()
        };
        let mut unordered = ids(&page(db, &plain).await);
        unordered.sort_unstable();
        assert_eq!(
            unordered, expected,
            "{label}: {column} contains {value:?}, unordered"
        );
        // A filtered page is still one statement on one table, so the grid
        // keeps editing it; an empty one still shows its columns.
        let shown = db.page_text(&request).unwrap();
        assert_eq!(sql::statements(&dialect, &shown).unwrap().len(), 1);
        let inferred = sql::editable_table(&dialect, &shown, &tables);
        assert!(inferred.is_some(), "{label}: not editable: {shown}");
        sql::name_empty_result(inferred.as_ref(), &columns, &mut found);
        assert!(
            sql::editable_result_table(inferred, &columns, &found).is_some(),
            "{label}: result not editable: {shown}"
        );
    }
    // A key that exists is one read of one partition: the statement says so.
    let exact = PageRequest {
        filters: vec![contains("id", "12")],
        ..PageRequest::first(table.clone())
    };
    assert!(
        db.page_text(&exact).unwrap().starts_with(&format!(
            "SELECT * FROM \"{ks}\".\"{name}\" WHERE \"id\" = 12\nLIMIT 500"
        )),
        "{}",
        db.page_text(&exact).unwrap()
    );
    // Row 12 is the answer; 112 and 120 are not looked for once the key
    // itself was found.
    assert_eq!(
        ids(&page(db, &exact).await),
        [12],
        "{label}: the exact key first"
    );

    // 7. One cell, with the conflict check: a lightweight transaction.
    assert!(
        caps.edit_cells && caps.insert_rows && caps.delete_rows,
        "{label}: grid changes"
    );
    let key = |id: i64| vec![("id".to_owned(), integer(id))];
    let edit = RowChange::UpdateCell {
        table: table.clone(),
        column: "label".into(),
        old: text("row 20"),
        new: text("Kopia D:\\zrzut\\ 'x'"),
        key: key(20),
    };
    assert_eq!(
        db.change_text(&edit).unwrap(),
        format!(
            "UPDATE \"{ks}\".\"{name}\" SET \"label\" = 'Kopia D:\\zrzut\\ ''x''' WHERE \"id\" = 20 IF \"label\" = 'row 20'"
        )
    );
    db.apply(&edit).await.unwrap();
    assert_eq!(
        code(db.apply(&edit).await),
        CONFLICT,
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
        old: all.rows[20][amount_at].clone(),
        new: Cell::Null,
        key: key(21),
    })
    .await
    .unwrap();
    // A key column says where the row lives: it is not changed.
    assert_eq!(
        code(
            db.apply(&RowChange::UpdateCell {
                table: table.clone(),
                column: "id".into(),
                old: integer(30),
                new: integer(31),
                key: key(30),
            })
            .await
        ),
        "cassandra_key_read_only"
    );

    // 8. A new row and a removed one. CAPABILITY-LIMITED: CQL has no row
    // that takes only defaults: the whole primary key has to be given.
    let added = RowChange::InsertRow {
        table: table.clone(),
        values: vec![("id".into(), integer(200)), ("label".into(), text("added"))],
    };
    db.apply(&added).await.unwrap();
    // The key is taken now: an INSERT would overwrite, so it is refused.
    assert_eq!(code(db.apply(&added).await), "cassandra_row_exists");
    assert_eq!(
        code(
            db.apply(&RowChange::InsertRow {
                table: table.clone(),
                values: vec![],
            })
            .await
        ),
        "cassandra_key_required",
        "{label}: a row needs its key"
    );
    let removed = RowChange::DeleteRow {
        table: table.clone(),
        key: key(1),
    };
    db.apply(&removed).await.unwrap();
    assert_eq!(
        code(db.apply(&removed).await),
        CONFLICT,
        "{label}: a row that is gone cannot be removed again"
    );
    // Read back through a page, like the grid does after every change.
    let mut check = page(
        db,
        &PageRequest {
            order_by: Some(("id".into(), false)),
            ..PageRequest::first(table.clone())
        },
    )
    .await;
    assert_eq!(
        check.rows.len(),
        120,
        "{label}: 120 rows, one gone, one new"
    );
    let id_at = at(&check, "id");
    check
        .rows
        .retain(|row| ["1", "11", "20", "21", "200"].contains(&row[id_at].display().as_str()));
    let expected = [
        (11, "row 11", large),
        (20, "Kopia D:\\zrzut\\ 'x'", Cell::Decimal("20.50".into())),
        (21, "row 21", Cell::Null),
        (200, "added", Cell::Null),
    ];
    assert_eq!(check.rows.len(), expected.len());
    for (row, (id, label_text, amount)) in check.rows.iter().zip(&expected) {
        assert_eq!(row[id_at], integer(*id));
        assert_eq!(row[label_at], text(label_text), "{label}: label of {id}");
        assert!(
            same_number(&row[amount_at], amount),
            "{label}: amount of {id}: {:?}",
            row[amount_at]
        );
    }

    // 9. The query builder. CAPABILITY-LIMITED: it is off, because it
    // writes joins, LIKE and GROUP BY on any column. A count per value is
    // asked in the editor; CQL wants to be told that it reads everything.
    assert!(!caps.query_builder, "{label}: the builder is off");
    let count = format!("SELECT COUNT(*) FROM \"{ks}\".\"{name}\" WHERE kind = 'kind 2'");
    assert_eq!(failure(db, &count).await, "cassandra_filtering_required");
    let counted = run(db, &format!("{count} ALLOW FILTERING")).await;
    // Ids 2, 6, … 118: thirty rows; the row added by the grid has no kind.
    assert_eq!(counted.rows[0][0].display(), "30", "{label}: count");

    // 10. The "Monitor" section.
    assert!(caps.monitor, "{label}: monitor");
    let stats = db.server_stats().await.unwrap();
    assert_eq!(stats.rows.len(), 1, "{label}: one row of statistics");
    let stat = |name: &str| {
        stats
            .columns
            .iter()
            .position(|column| column.name == name)
            .map(|index| stats.rows[0][index].display())
    };
    assert!(
        stat("version").is_some_and(|version| version.len() > 14),
        "{:?}",
        stat("version")
    );
    assert!(stat("tables").is_some_and(|count| count.parse::<u32>().is_ok_and(|n| n >= 1)));
    assert!(stat("nodes").is_some_and(|count| count.parse::<u32>().is_ok_and(|n| n >= 1)));

    // 11. Stopping. CQL has no request that stops another: what is stopped
    // is this side, at once, and no further page or statement is sent. The
    // one request in flight ends by the time limits of the server.
    let script = (1000..1400)
        .map(|id| format!("INSERT INTO \"{ks}\".\"{name}\" (id, label) VALUES ({id}, 'slow')"))
        .collect::<Vec<_>>()
        .join(";\n");
    let token = CancellationToken::new();
    let (worker, cancel) = (db.clone(), token.clone());
    let task = tokio::spawn(async move { worker.query(&script, cancel).await });
    tokio::time::sleep(Duration::from_millis(40)).await;
    let asked = Instant::now();
    token.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap_or_else(|_| panic!("{label}: the script was not stopped"))
        .unwrap();
    assert_eq!(
        stopped.unwrap_err().to_string(),
        "query_cancelled",
        "{label}: cancel"
    );
    let stopped_in = asked.elapsed();
    assert!(stopped_in < Duration::from_secs(2));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let written = run(
        db,
        &format!("SELECT COUNT(*) FROM \"{ks}\".\"{name}\" WHERE label = 'slow' ALLOW FILTERING"),
    )
    .await
    .rows[0][0]
        .display()
        .parse::<u32>()
        .unwrap();
    assert!(
        written < 400,
        "{label}: the script went on after it was stopped"
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        db.query("SELECT * FROM system.local", cancelled)
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
        "{label}: the connection goes on after a cancel"
    );
    println!(
        "{label}: designer off (NOT NULL), listing, columns with key parts, import (120 rows, existing and repeated keys refused), forward paging and reached pages, order by the driver, 10 filters with the exact key first, cell edit with conflict check, insert, delete, builder off (count in the editor), monitor ({} columns), cancel in {} ms ({written} of 400 statements had been sent)",
        stats.columns.len(),
        stopped_in.as_millis()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Cassandra server described by BAZA_DB_TEST_* variables"]
async fn server_passes_the_driver_checklist() {
    let profile = profile();
    let db = connect(&profile).await;
    let name = unique("arch_checklist");
    // Run on a task of its own, so that a failed assertion still drops the table.
    let (worker, used, table) = (db.clone(), profile.clone(), name.clone());
    let outcome = tokio::spawn(async move { checklist(&worker, &used, &table).await }).await;
    dropped(&db, std::slice::from_ref(&name)).await;
    let left = db.tables().await.unwrap();
    db.close().await;
    assert!(left.iter().all(|table| table.name != name));
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
}

/// The address of the tunnel test: what the SSH server is asked to reach.
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
#[ignore = "needs a real SSH server and a Cassandra server: BAZA_SSH_TEST_* and BAZA_DB_TEST_*"]
async fn cassandra_through_an_ssh_tunnel() {
    let mut profile = profile();
    let ssh = ssh_profile();
    profile.ssh = Some(ssh.clone());
    // What the application does for a profile with an SSH tab: a tunnel to
    // the server, and the driver dials its loopback end.
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
    let ks = keyspace();
    run(
        &db,
        &format!(
            "CREATE KEYSPACE IF NOT EXISTS \"{ks}\" WITH replication = {{'class': 'SimpleStrategy', 'replication_factor': 1}}"
        ),
    )
    .await;
    let name = unique("ssh_checklist");
    let outcome = {
        let (db, name, ks) = (db.clone(), name.clone(), ks.clone());
        tokio::spawn(async move {
            run(
                &db,
                &format!("CREATE TABLE \"{ks}\".\"{name}\" (id int PRIMARY KEY, label text)"),
            )
            .await;
            let table = db
                .tables()
                .await
                .unwrap()
                .into_iter()
                .find(|found| found.name == name)
                .expect("the table is listed through the tunnel");
            let names = ["id", "label"].map(str::to_owned);
            let rows: Vec<Vec<Cell>> = (1..=60)
                .map(|n| vec![integer(n), text(&format!("through the tunnel {n}"))])
                .collect();
            db.import_rows(&table, &names, &rows, CancellationToken::new())
                .await
                .unwrap();
            // Two pages, read on through the tunnel.
            let mut request = PageRequest::first(table.clone());
            request.limit = 40;
            let first = page(&db, &request).await;
            request.offset = 40;
            let second = page(&db, &request).await;
            assert_eq!((first.rows.len(), second.rows.len()), (40, 20));
            let found = page(
                &db,
                &PageRequest {
                    filters: vec![contains("label", "TUNNEL 33")],
                    ..PageRequest::first(table.clone())
                },
            )
            .await;
            assert_eq!(ids(&found), [33]);
            let edit = RowChange::UpdateCell {
                table: table.clone(),
                column: "label".into(),
                old: text("through the tunnel 33"),
                new: text("edited through the tunnel"),
                key: vec![("id".into(), integer(33))],
            };
            db.apply(&edit).await.unwrap();
            assert_eq!(code(db.apply(&edit).await), CONFLICT);
            let stats = db.server_stats().await.unwrap();
            assert_eq!(stats.rows.len(), 1);
            stats.columns.len()
        })
        .await
    };
    // The tunnel carried it: every connection of the driver went to its
    // loopback end, although the node reports an address of its own.
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
    assert!(
        after.active_clients >= 1 && after.error.is_none(),
        "{:?}",
        after.error
    );
    // TLS inside the tunnel is asked of the server by its own name: a plain
    // server says it has none, in words, not "the network was lost".
    let mut encrypted = profile.clone();
    encrypted.tls = TlsMode::VerifyFull;
    let refused = Database::connect(&encrypted, Some(local))
        .await
        .err()
        .map(|error| error.to_string());
    assert!(
        matches!(
            refused.as_deref(),
            Some("database_tls_unsupported" | "database_certificate_rejected")
        ),
        "{refused:?}"
    );
    manager.shutdown().await;
    println!(
        "{} over 127.0.0.1:{local}: listing, import, two pages read on, filter, cell edit with conflict check, monitor ({columns} columns); tunnel sent {} B, received {} B, {} forwarded connections at the end",
        profile.product_label(),
        after.bytes_sent,
        after.bytes_received,
        after.active_clients
    );
}

/// Read the one row with this id from a page of the table.
async fn row_of(db: &Database, table: &TableRef, id: i64) -> (QueryResult, Vec<Cell>) {
    let found = page(
        db,
        &PageRequest {
            filters: vec![Filter {
                column: "id".into(),
                op: FilterOp::Equal,
                value: integer(id),
            }],
            ..PageRequest::first(table.clone())
        },
    )
    .await;
    assert_eq!(found.rows.len(), 1, "row {id}");
    let row = found.rows[0].clone();
    (found, row)
}

/// Every CQL type: what a cell shows, and that what is shown, or typed as
/// text, is written back as the same value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Cassandra server described by BAZA_DB_TEST_* variables"]
async fn every_cql_type_is_shown_and_written() {
    let profile = profile();
    let db = connect(&profile).await;
    let ks = keyspace();
    let (name, counters) = (unique("types"), unique("counters"));
    let made = [name.clone(), counters.clone()];
    let worker = db.clone();
    let outcome = tokio::spawn(async move {
        let db = &worker;
        let udt = unique("address");
        run(
            db,
            &format!(
                "CREATE TYPE \"{ks}\".\"{udt}\" (street text, zip int, tags frozen<set<text>>);\n\
                 CREATE TABLE \"{ks}\".\"{name}\" (id int PRIMARY KEY, a_ascii ascii, a_bigint bigint, a_blob blob, a_boolean boolean, a_date date, a_decimal decimal, a_double double, a_duration duration, a_float float, a_inet inet, a_int int, a_smallint smallint, a_text text, a_time time, a_timestamp timestamp, a_timeuuid timeuuid, a_tinyint tinyint, a_uuid uuid, a_varint varint, c_list list<int>, c_set set<text>, c_map map<text, decimal>, c_tuple tuple<int, text>, c_udt frozen<\"{udt}\">, c_nested frozen<map<int, frozen<list<uuid>>>>, c_vector vector<float, 3>);\n\
                 INSERT INTO \"{ks}\".\"{name}\" (id, a_ascii, a_bigint, a_blob, a_boolean, a_date, a_decimal, a_double, a_duration, a_float, a_inet, a_int, a_smallint, a_text, a_time, a_timestamp, a_timeuuid, a_tinyint, a_uuid, a_varint, c_list, c_set, c_map, c_tuple, c_udt, c_nested, c_vector) VALUES (1, 'plain', -9223372036854775808, 0x00ff10, true, '2024-05-01', 12345678901234567890.1200, 1.5e300, 1y2mo3d4h5m6s7ms, 0.1, '2001:db8::1', -7, 32767, 'Zażółć ''x''', '12:34:56.123456789', '2024-05-01T12:34:56.789Z', 5b6962dd-3f90-11ef-8a5a-0242ac120002, -128, 8e8b1c2a-4f3d-4e7b-9c1a-2b3c4d5e6f70, 123456789012345678901234567890123456789012345, [3, 1, 2], {{'b', 'a'}}, {{'k': 1.50, 'ż': -2}}, (7, 'seven'), {{street: 'Hoża 1', zip: 681, tags: {{'x', 'y'}}}}, {{5: [8e8b1c2a-4f3d-4e7b-9c1a-2b3c4d5e6f70]}}, [0.5, -1.25, 3]);\n\
                 INSERT INTO \"{ks}\".\"{name}\" (id) VALUES (2)"
            ),
        )
        .await;
        let table = db
            .tables()
            .await
            .unwrap()
            .into_iter()
            .find(|found| found.name == name)
            .unwrap();
        let columns = db.columns(&table).await.unwrap();
        let kind = |column: &str| {
            columns
                .iter()
                .find(|found| found.name == column)
                .map(|found| found.data_type.clone())
                .unwrap()
        };
        assert_eq!(kind("c_map"), "map<text, decimal>");
        assert_eq!(kind("c_udt"), format!("frozen<{udt}>"));
        assert_eq!(kind("c_nested"), "frozen<map<int, frozen<list<uuid>>>>");
        assert_eq!(kind("c_vector"), "vector<float, 3>");
        let json = |value: &str| Cell::Json(value.into());
        // What each type is shown as, and a value to change it to.
        let cases: Vec<(&str, Cell, Cell)> = vec![
            ("a_ascii", text("plain"), text("other")),
            ("a_bigint", integer(i64::MIN), integer(i64::MAX)),
            ("a_blob", Cell::Bytes(vec![0, 255, 16]), Cell::Bytes(vec![202, 254])),
            ("a_boolean", Cell::Bool(true), Cell::Bool(false)),
            ("a_date", text("2024-05-01"), text("1969-12-31")),
            (
                "a_decimal",
                Cell::Decimal("12345678901234567890.1200".into()),
                Cell::Decimal("-0.000000000000000000000000000001".into()),
            ),
            ("a_double", Cell::Float("1.5e300".into()), Cell::Float("-2.25".into())),
            ("a_duration", text("1y2mo3d4h5m6s7ms"), text("90m")),
            ("a_float", Cell::Float("0.1".into()), Cell::Float("3.5".into())),
            ("a_inet", text("2001:db8::1"), text("10.1.2.3")),
            ("a_int", integer(-7), integer(2_147_483_647)),
            ("a_smallint", integer(32767), integer(-32768)),
            ("a_text", text("Zażółć 'x'"), text("line\nbreak 🦀 \\ \"q\"")),
            ("a_time", text("12:34:56.123456789"), text("23:59:59")),
            (
                "a_timestamp",
                text("2024-05-01T12:34:56.789Z"),
                text("1999-12-31T23:59:59.001Z"),
            ),
            (
                "a_timeuuid",
                text("5b6962dd-3f90-11ef-8a5a-0242ac120002"),
                text("d2177dd0-eaa2-11de-a572-001b779c76e3"),
            ),
            ("a_tinyint", integer(-128), integer(127)),
            (
                "a_uuid",
                text("8e8b1c2a-4f3d-4e7b-9c1a-2b3c4d5e6f70"),
                text("00000000-0000-0000-0000-000000000001"),
            ),
            (
                "a_varint",
                // More than 128 bits: an exact decimal in the grid.
                Cell::Decimal("123456789012345678901234567890123456789012345".into()),
                Cell::Decimal("-98765432109876543210987654321098765432109876".into()),
            ),
            ("c_list", json("[3,1,2]"), json("[9,9]")),
            // A set comes back in its own order.
            ("c_set", json(r#"["a","b"]"#), json(r#"["z","it's"]"#)),
            ("c_map", json(r#"{"k":1.50,"ż":-2}"#), json(r#"{"n":0.001}"#)),
            ("c_tuple", json(r#"[7,"seven"]"#), json(r#"[8,null]"#)),
            (
                "c_udt",
                json(r#"{"street":"Hoża 1","zip":681,"tags":["x","y"]}"#),
                json(r#"{"street":"Wspólna","zip":null,"tags":["q"]}"#),
            ),
            (
                "c_nested",
                json(r#"{"5":["8e8b1c2a-4f3d-4e7b-9c1a-2b3c4d5e6f70"]}"#),
                json(r#"{"1":[],"2":["00000000-0000-0000-0000-000000000001"]}"#),
            ),
            ("c_vector", json("[0.5,-1.25,3]"), json("[1,2,3.5]")),
        ];
        let (shown, row) = row_of(db, &table, 1).await;
        for (column, expected, _) in &cases {
            assert_eq!(&row[at(&shown, column)], expected, "{column} as shown");
        }
        let key = |id: i64| vec![("id".to_owned(), integer(id))];
        // Each cell is changed while it holds what was shown, and the same
        // change again is a conflict.
        let mut conditions = 0;
        for (column, old, new) in &cases {
            let change = RowChange::UpdateCell {
                table: table.clone(),
                column: (*column).into(),
                old: old.clone(),
                new: new.clone(),
                key: key(1),
            };
            match db.apply(&change).await {
                Ok(()) => {
                    conditions += 1;
                    assert_eq!(code(db.apply(&change).await), CONFLICT, "{column} again");
                }
                // A server that takes no condition on a duration says so.
                Err(error) => {
                    assert_eq!(*column, "a_duration", "{column}: {error}");
                    assert_eq!(error.to_string(), "cassandra_invalid_query");
                    run(
                        db,
                        &format!("UPDATE \"{ks}\".\"{name}\" SET a_duration = 90m WHERE id = 1"),
                    )
                    .await;
                }
            }
        }
        let (shown, row) = row_of(db, &table, 1).await;
        for (column, _, expected) in &cases {
            let found = &row[at(&shown, column)];
            match *column {
                "a_duration" => assert_eq!(found, &text("1h30m")),
                "c_set" => assert_eq!(found, &json(r#"["it's","z"]"#)),
                "c_nested" => assert_eq!(
                    found,
                    &json(r#"{"1":[],"2":["00000000-0000-0000-0000-000000000001"]}"#)
                ),
                "c_vector" => assert_eq!(found, &json("[1,2,3.5]")),
                _ => assert_eq!(found, expected, "{column} after the change"),
            }
        }
        // The empty row: every cell is nothing, and each takes a value
        // typed as text, as the new-row and the cell editor hand it over.
        let (shown, empty) = row_of(db, &table, 2).await;
        assert!(empty.iter().skip(1).all(|cell| matches!(cell, Cell::Null)));
        for (column, _, new) in &cases {
            let typed = match new {
                Cell::Bytes(bytes) => text(&format!(
                    "0x{}",
                    bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
                )),
                other => text(&other.display()),
            };
            let change = RowChange::UpdateCell {
                table: table.clone(),
                column: (*column).into(),
                old: Cell::Null,
                new: typed,
                key: key(2),
            };
            match db.apply(&change).await {
                Ok(()) => {}
                Err(error) => assert_eq!(*column, "a_duration", "{column}: {error}"),
            }
        }
        let (_, typed_row) = row_of(db, &table, 2).await;
        let (_, changed_row) = row_of(db, &table, 1).await;
        for (column, _, _) in &cases {
            if *column != "a_duration" {
                let index = at(&shown, column);
                assert_eq!(typed_row[index], changed_row[index], "{column} typed as text");
            }
        }
        // A new row and an imported one, from text.
        let values: Vec<(String, Cell)> = std::iter::once(("id".to_owned(), text("3")))
            .chain(cases.iter().filter(|(column, ..)| *column != "a_blob").map(
                |(column, _, new)| ((*column).to_owned(), text(&new.display())),
            ))
            .collect();
        db.apply(&RowChange::InsertRow {
            table: table.clone(),
            values: values.clone(),
        })
        .await
        .unwrap();
        let names: Vec<String> = values.iter().map(|(name, _)| name.clone()).collect();
        let mut cells: Vec<Cell> = values.iter().map(|(_, cell)| cell.clone()).collect();
        cells[0] = text("4");
        db.import_rows(&table, &names, &[cells], CancellationToken::new())
            .await
            .unwrap();
        let (_, inserted) = row_of(db, &table, 3).await;
        let (_, imported) = row_of(db, &table, 4).await;
        for (column, _, _) in &cases {
            let index = at(&shown, column);
            if *column == "a_blob" {
                assert_eq!(inserted[index], Cell::Null);
            } else if *column == "a_duration" {
                assert_eq!(inserted[index], text("1h30m"));
                assert_eq!(imported[index], text("1h30m"));
            } else {
                assert_eq!(inserted[index], changed_row[index], "{column} inserted");
                assert_eq!(imported[index], changed_row[index], "{column} imported");
            }
        }
        // What does not fit its type is refused before anything is sent.
        for (column, bad, expected) in [
            ("a_uuid", "not-a-uuid", "cassandra_value_invalid"),
            ("a_int", "2147483648", "integer_invalid"),
            ("a_timestamp", "yesterday", "cassandra_value_invalid"),
            ("a_inet", "example.test", "cassandra_value_invalid"),
            ("a_duration", "1 hour", "cassandra_value_invalid"),
            ("c_list", "[1, \"x\"]", "integer_invalid"),
            ("c_map", "[1]", "cassandra_value_invalid"),
            ("c_vector", "[1,2]", "cassandra_value_invalid"),
            ("c_udt", "{\"nope\": 1}", "cassandra_value_invalid"),
            ("c_set", "not json", "json_invalid"),
        ] {
            let change = RowChange::UpdateCell {
                table: table.clone(),
                column: column.into(),
                old: Cell::Null,
                new: text(bad),
                key: key(1),
            };
            assert_eq!(code(db.apply(&change).await), expected, "{column} = {bad}");
        }
        // Counters are shown, and changed only by adding in the editor.
        run(
            db,
            &format!(
                "CREATE TABLE \"{ks}\".\"{counters}\" (page text PRIMARY KEY, hits counter);\n\
                 UPDATE \"{ks}\".\"{counters}\" SET hits = hits + 5 WHERE page = '/'"
            ),
        )
        .await;
        let counter_table = db
            .tables()
            .await
            .unwrap()
            .into_iter()
            .find(|found| found.name == counters)
            .unwrap();
        let hits = page(db, &PageRequest::first(counter_table.clone())).await;
        assert_eq!(hits.rows, [vec![text("/"), integer(5)]]);
        assert_eq!(hits.columns[1].data_type, "counter");
        let page_key = vec![("page".to_owned(), text("/"))];
        for change in [
            RowChange::UpdateCell {
                table: counter_table.clone(),
                column: "hits".into(),
                old: integer(5),
                new: integer(6),
                key: page_key.clone(),
            },
            RowChange::InsertRow {
                table: counter_table.clone(),
                values: vec![("page".into(), text("/x"))],
            },
            RowChange::DeleteRow {
                table: counter_table.clone(),
                key: page_key.clone(),
            },
        ] {
            assert_eq!(code(db.apply(&change).await), "cassandra_counter_read_only");
            assert_eq!(
                db.change_text(&change).unwrap_err().to_string(),
                "cassandra_counter_read_only"
            );
        }
        assert_eq!(
            db.import_rows(
                &counter_table,
                &["page".into()],
                &[vec![text("/y")]],
                CancellationToken::new()
            )
            .await
            .unwrap_err()
            .to_string(),
            "cassandra_counter_read_only"
        );
        run(db, &format!("DROP TABLE \"{ks}\".\"{name}\"; DROP TYPE \"{ks}\".\"{udt}\"")).await;
        (cases.len(), conditions)
    })
    .await;
    dropped(&db, &made).await;
    db.close().await;
    match outcome {
        Ok((types, conditions)) => println!(
            "{}: {types} types shown, {conditions} of them changed under a condition, typed as text, inserted and imported; bad values refused before sending; counters shown and not written",
            profile.product_label()
        ),
        Err(failure) => std::panic::resume_unwind(failure.into_panic()),
    }
}

/// A key of several columns, clustering order, static columns, and what
/// the grid does with each.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Cassandra server described by BAZA_DB_TEST_* variables"]
async fn keys_of_several_columns_and_static_columns() {
    let profile = profile();
    let db = connect(&profile).await;
    let ks = keyspace();
    let name = unique("readings");
    let made = [name.clone()];
    let worker = db.clone();
    let outcome = tokio::spawn(async move {
        let db = &worker;
        run(
            db,
            &format!(
                "CREATE TABLE \"{ks}\".\"{name}\" (tenant int, region text, at timestamp, device int, owner text static, reading double, PRIMARY KEY ((tenant, region), at, device)) WITH CLUSTERING ORDER BY (at DESC, device ASC)"
            ),
        )
        .await;
        let table = db
            .tables()
            .await
            .unwrap()
            .into_iter()
            .find(|found| found.name == name)
            .unwrap();
        let columns = db.columns(&table).await.unwrap();
        assert_eq!(
            columns
                .iter()
                .map(|column| (column.name.as_str(), column.data_type.as_str(), column.primary_key))
                .collect::<Vec<_>>(),
            [
                ("tenant", "int · partition key", true),
                ("region", "text · partition key", true),
                ("at", "timestamp · clustering key desc", true),
                ("device", "int · clustering key", true),
                ("owner", "text · static", false),
                ("reading", "double", false),
            ]
        );
        let names = ["tenant", "region", "at", "device", "reading"].map(str::to_owned);
        let rows: Vec<Vec<Cell>> = (0..40)
            .map(|n| {
                vec![
                    integer(n % 2 + 1),
                    text(if n % 4 < 2 { "eu" } else { "us" }),
                    text(&format!("2024-05-01T12:{:02}:00Z", n)),
                    integer(n),
                    Cell::Float(format!("{}.5", n)),
                ]
            })
            .collect();
        db.import_rows(&table, &names, &rows, CancellationToken::new())
            .await
            .unwrap();
        let key = |n: i64| {
            vec![
                ("tenant".to_owned(), integer(n % 2 + 1)),
                ("region".to_owned(), text(if n % 4 < 2 { "eu" } else { "us" })),
                ("at".to_owned(), text(&format!("2024-05-01T12:{:02}:00Z", n))),
                ("device".to_owned(), integer(n)),
            ]
        };
        // A static column is one value of the partition: set from one
        // row, seen in every row of that partition and in no other.
        let owner = RowChange::UpdateCell {
            table: table.clone(),
            column: "owner".into(),
            old: Cell::Null,
            new: text("Ada"),
            key: key(0),
        };
        assert!(
            db.change_text(&owner)
                .unwrap()
                .ends_with("SET \"owner\" = 'Ada' WHERE \"tenant\" = 1 AND \"region\" = 'eu' IF \"owner\" = null"),
            "{}",
            db.change_text(&owner).unwrap()
        );
        db.apply(&owner).await.unwrap();
        assert_eq!(code(db.apply(&owner).await), CONFLICT);
        let all = page(db, &PageRequest::first(table.clone())).await;
        assert_eq!(all.rows.len(), 40);
        let (tenant, region, owner_at) = (at(&all, "tenant"), at(&all, "region"), at(&all, "owner"));
        for row in &all.rows {
            let same = row[tenant] == integer(1) && row[region] == text("eu");
            assert_eq!(row[owner_at], if same { text("Ada") } else { Cell::Null });
        }
        // Inside a partition the rows come in clustering order: newest first.
        let partition: Vec<String> = all
            .rows
            .iter()
            .filter(|row| row[tenant] == integer(1) && row[region] == text("eu"))
            .map(|row| row[at(&all, "at")].display())
            .collect();
        let mut newest_first = partition.clone();
        newest_first.sort_by(|a, b| b.cmp(a));
        assert_eq!(partition, newest_first);
        // A regular cell is addressed by the whole key.
        let reading = RowChange::UpdateCell {
            table: table.clone(),
            column: "reading".into(),
            old: Cell::Float("4.5".into()),
            new: Cell::Float("-1".into()),
            key: key(4),
        };
        db.apply(&reading).await.unwrap();
        assert_eq!(code(db.apply(&reading).await), CONFLICT);
        // A column of the partition key alone cannot be asked of the
        // server: the rows are read and checked. One with every part of
        // the key is still found.
        let found = page(
            db,
            &PageRequest {
                filters: vec![contains("region", "US"), contains("device", "3")],
                order_by: Some(("device".into(), true)),
                ..PageRequest::first(table.clone())
            },
        )
        .await;
        let devices: Vec<String> = found
            .rows
            .iter()
            .map(|row| row[at(&found, "device")].display())
            .collect();
        assert_eq!(devices, ["39", "38", "35", "34", "31", "30", "23", "3"]);
        assert!(!db.page_text(&PageRequest {
            filters: vec![contains("tenant", "1")],
            ..PageRequest::first(table.clone())
        })
        .unwrap()
        .contains("WHERE"));
        // A new row needs its whole key; one that exists is not overwritten.
        let mut values = key(50);
        values.push(("reading".into(), text("7.25")));
        db.apply(&RowChange::InsertRow {
            table: table.clone(),
            values: values.clone(),
        })
        .await
        .unwrap();
        assert_eq!(
            code(
                db.apply(&RowChange::InsertRow {
                    table: table.clone(),
                    values: values[..3].to_vec(),
                })
                .await
            ),
            "cassandra_key_required"
        );
        db.apply(&RowChange::DeleteRow {
            table: table.clone(),
            key: key(50),
        })
        .await
        .unwrap();
        assert_eq!(
            code(
                db.apply(&RowChange::DeleteRow {
                    table: table.clone(),
                    key: key(50),
                })
                .await
            ),
            CONFLICT
        );
        // A partition with a static value and no row shows one line
        // without a clustering key: it cannot be addressed as a row.
        run(
            db,
            &format!("INSERT INTO \"{ks}\".\"{name}\" (tenant, region, owner) VALUES (9, 'pl', 'Bob')"),
        )
        .await;
        let lone = page(
            db,
            &PageRequest {
                filters: vec![contains("owner", "bob")],
                ..PageRequest::first(table.clone())
            },
        )
        .await;
        assert_eq!(lone.rows.len(), 1);
        assert_eq!(lone.rows[0][at(&lone, "at")], Cell::Null);
        let lone_key: Vec<(String, Cell)> = ["tenant", "region", "at", "device"]
            .iter()
            .map(|column| ((*column).to_owned(), lone.rows[0][at(&lone, column)].clone()))
            .collect();
        assert_eq!(
            code(
                db.apply(&RowChange::DeleteRow {
                    table: table.clone(),
                    key: lone_key.clone(),
                })
                .await
            ),
            "cassandra_static_row"
        );
        // Its static value is still changed: that needs the partition only.
        db.apply(&RowChange::UpdateCell {
            table: table.clone(),
            column: "owner".into(),
            old: text("Bob"),
            new: text("Bobby"),
            key: lone_key,
        })
        .await
        .unwrap();
    })
    .await;
    dropped(&db, &made).await;
    db.close().await;
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
    println!(
        "{}: a key of two partition and two clustering columns with its parts named, clustering order, a static column set by its partition, cells by the whole key, filters on key parts read and checked, insert and delete by the whole key, a static-only row refused",
        profile.product_label()
    );
}

/// What the editor does with CQL, and what the tree lists besides tables.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Cassandra server described by BAZA_DB_TEST_* variables"]
async fn the_editor_runs_cql_and_the_tree_lists_views() {
    let profile = profile();
    let db = connect(&profile).await;
    let ks = keyspace();
    let (name, view) = (unique("editor"), unique("editor_by_label"));
    let made = [name.clone()];
    let worker = db.clone();
    let outcome = tokio::spawn(async move {
        let db = &worker;
        // What the connection dialog and a new editor tab ask.
        let probe = run(db, "-- Ctrl+Enter\nSELECT 1;").await;
        assert_eq!(probe.rows, [vec![integer(1)]]);
        // No keyspace was chosen yet: said in words, then chosen with USE.
        assert_eq!(
            failure(db, &format!("CREATE TABLE \"{name}\" (id int PRIMARY KEY)")).await,
            "cassandra_keyspace_required"
        );
        assert_eq!(failure(db, "USE bazaqa_no_such_keyspace").await, "database_not_found");
        assert_eq!(failure(db, "USE \"bad name\"").await, "cassandra_keyspace_name_invalid");
        run(
            db,
            &format!(
                "USE \"{ks}\";\nCREATE TABLE \"{name}\" (id int PRIMARY KEY, label text, n int);\n\
                 -- a batch is one statement with semicolons inside\n\
                 BEGIN BATCH\n  INSERT INTO \"{name}\" (id, label, n) VALUES (1, 'a;b', 10);\n  INSERT INTO \"{name}\" (id, label, n) VALUES (2, $$it's; $$, 20);\nAPPLY BATCH;\n\
                 INSERT INTO \"{name}\" (id, label, n) VALUES (3, 'c', 30)"
            ),
        )
        .await;
        let all = run(db, &format!("SELECT * FROM \"{name}\"")).await;
        assert_eq!(all.rows.len(), 3);
        assert_eq!(
            all.columns
                .iter()
                .map(|column| (column.name.as_str(), column.data_type.as_str()))
                .collect::<Vec<_>>(),
            [("id", "int"), ("label", "text"), ("n", "int")]
        );
        let two = run(db, &format!("SELECT label FROM \"{name}\" WHERE id = 2")).await;
        assert_eq!(two.rows, [vec![text("it's; ")]]);
        // A plain SELECT * of one table is what the grid edits.
        let tables = db.tables().await.unwrap();
        let statement = format!("SELECT * FROM \"{ks}\".\"{name}\" WHERE id = 2");
        let table = sql::editable_table(db.dialect(), &statement, &tables).expect("editable");
        let columns = db.columns(&table).await.unwrap();
        let result = run(db, &statement).await;
        assert!(sql::editable_result_table(Some(table.clone()), &columns, &result).is_some());
        // A lightweight transaction answers with a row of its own.
        let applied = run(
            db,
            &format!("UPDATE \"{name}\" SET n = 21 WHERE id = 2 IF n = 20"),
        )
        .await;
        assert_eq!(applied.columns[0].name, "[applied]");
        assert_eq!(applied.rows[0][0], Cell::Bool(true));
        let refused = run(
            db,
            &format!("UPDATE \"{name}\" SET n = 22 WHERE id = 2 IF n = 20"),
        )
        .await;
        assert_eq!(refused.rows[0], [Cell::Bool(false), integer(21)]);
        // What CQL cannot do is said, each in its own words.
        for (statement, expected) in [
            (format!("SELEC * FROM \"{name}\""), "cassandra_syntax_error"),
            ("SELECT * FROM bazaqa_no_such_table".to_owned(), "cassandra_table_unknown"),
            (format!("SELECT * FROM \"{name}\" WHERE label = 'c'"), "cassandra_filtering_required"),
            (format!("SELECT * FROM \"{name}\" ORDER BY n"), "cassandra_order_restricted"),
            (format!("SELECT nope FROM \"{name}\""), "cassandra_column_unknown"),
            (format!("CREATE TABLE \"{name}\" (id int PRIMARY KEY)"), "cassandra_already_exists"),
            (format!("UPDATE \"{name}\" SET id = 5 WHERE id = 1"), "cassandra_key_rule"),
            (format!("INSERT INTO \"{name}\" (id, n) VALUES (9, 'text')"), "cassandra_invalid_query"),
            ("SELECT * FROM x.y JOIN z".to_owned(), "cassandra_syntax_error"),
        ] {
            assert_eq!(failure(db, &statement).await, expected, "{statement}");
        }
        assert_eq!(
            run(db, &format!("SELECT * FROM \"{name}\" WHERE label = 'c' ALLOW FILTERING"))
                .await
                .rows
                .len(),
            1
        );
        // A change of the schema is seen by the next look at the table.
        run(db, &format!("ALTER TABLE \"{name}\" ADD extra map<text, int>")).await;
        let columns = db.columns(&table).await.unwrap();
        assert!(
            columns
                .iter()
                .any(|column| column.name == "extra" && column.data_type == "map<text, int>")
        );
        // A materialized view is listed as one and never written. A server
        // has them switched off unless its configuration says otherwise.
        let made = db
            .query(
                &format!(
                    "CREATE MATERIALIZED VIEW \"{ks}\".\"{view}\" AS SELECT * FROM \"{ks}\".\"{name}\" WHERE label IS NOT NULL AND id IS NOT NULL PRIMARY KEY (label, id)"
                ),
                CancellationToken::new(),
            )
            .await;
        match made {
            Err(error) => {
                assert_eq!(error.to_string(), "cassandra_disabled_by_configuration");
                "materialized views are switched off on this server, said in words: not checked"
                    .to_owned()
            }
            Ok(_) => {
                let listed = db.tables().await.unwrap();
                let found = listed
                    .iter()
                    .find(|found| found.name == view)
                    .expect("the view is listed")
                    .clone();
                assert_eq!(found.kind, "materialized view");
                let mut rows = QueryResult::default();
                for _ in 0..40 {
                    rows = page(db, &PageRequest::first(found.clone())).await;
                    if rows.rows.len() == 3 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                assert_eq!(rows.rows.len(), 3);
                assert_eq!(rows.columns[0].data_type, "text · partition key");
                let change = RowChange::DeleteRow {
                    table: found.clone(),
                    key: vec![("label".into(), text("c")), ("id".into(), integer(3))],
                };
                assert_eq!(code(db.apply(&change).await), "cassandra_read_only_object");
                assert_eq!(
                    db.change_text(&change).unwrap_err().to_string(),
                    "cassandra_read_only_object"
                );
                run(db, &format!("DROP MATERIALIZED VIEW \"{ks}\".\"{view}\"")).await;
                "a materialized view listed, read and not written".to_owned()
            }
        }
    })
    .await;
    dropped(&db, &made).await;
    db.close().await;
    let views = match outcome {
        Ok(views) => views,
        Err(failure) => std::panic::resume_unwind(failure.into_panic()),
    };
    // The keyspaces of the server and the virtual tables of the node, when
    // the profile asks for them.
    let mut system = profile.clone();
    system.set_option("system_keyspaces", "true");
    system.database = "system".into();
    let db = Database::connect(&system, None).await.unwrap();
    let tables = db.tables().await.unwrap();
    let settings = tables
        .iter()
        .find(|found| found.schema == "system_views" && found.name == "settings")
        .expect("the virtual tables are listed")
        .clone();
    assert_eq!(settings.kind, "view");
    assert!(
        tables.iter().any(|found| found.schema == "system"
            && found.name == "local"
            && found.kind == "table")
    );
    let shown = page(
        &db,
        &PageRequest {
            filters: vec![contains("name", "native_transport_port")],
            ..PageRequest::first(settings.clone())
        },
    )
    .await;
    assert!(!shown.rows.is_empty());
    assert_eq!(shown.columns[0].data_type, "text · partition key");
    assert_eq!(
        code(
            db.apply(&RowChange::DeleteRow {
                table: settings,
                key: vec![("name".into(), text("native_transport_port"))],
            })
            .await
        ),
        "cassandra_read_only_object"
    );
    // The keyspace of the profile is the one in use.
    assert_eq!(
        run(&db, "SELECT key FROM local").await.rows,
        [vec![text("local")]]
    );
    db.close().await;
    println!(
        "{}: SELECT 1, USE, a batch as one statement, results with their types, an editable SELECT *, lightweight transactions, 9 refusals in words, a schema change seen; {views}; system keyspaces and virtual tables on request",
        profile.product_label()
    );
}

/// The limits that keep a filter and an order from reading a whole cluster.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Cassandra server described by BAZA_DB_TEST_* variables; writes 100 500 rows"]
async fn filters_and_orders_read_a_bounded_number_of_rows() {
    let profile = profile();
    let db = connect(&profile).await;
    let ks = keyspace();
    let name = unique("bounded");
    let made = [name.clone()];
    let worker = db.clone();
    let outcome = tokio::spawn(async move {
        let db = &worker;
        run(
            db,
            &format!(
                "CREATE TABLE \"{ks}\".\"{name}\" (bucket int, id int, label text, PRIMARY KEY (bucket, id))"
            ),
        )
        .await;
        // 100 partitions of 1005 rows, written a hundred rows at a time.
        let started = Instant::now();
        // A text of the editor holds at most 10 000 statements, and every
        // row of a batch counts as one.
        for buckets in (0..100).collect::<Vec<i32>>().chunks(5) {
            let mut script = String::new();
            for bucket in buckets {
                for chunk in (0..1005).collect::<Vec<i32>>().chunks(100) {
                    script.push_str("BEGIN UNLOGGED BATCH\n");
                    for id in chunk {
                        script.push_str(&format!(
                            "INSERT INTO \"{ks}\".\"{name}\" (bucket, id, label) VALUES ({bucket}, {id}, 'row {bucket}/{id}');\n"
                        ));
                    }
                    script.push_str("APPLY BATCH;\n");
                }
            }
            run(db, &script).await;
        }
        let loaded = started.elapsed();
        let table = db
            .tables()
            .await
            .unwrap()
            .into_iter()
            .find(|found| found.name == name)
            .unwrap();
        // The partition the cluster stores last gets the one row to find.
        let order = run(db, &format!("SELECT DISTINCT bucket FROM \"{ks}\".\"{name}\"")).await;
        assert_eq!(order.rows.len(), 100);
        let last = order.rows[99][0].display();
        run(
            db,
            &format!("UPDATE \"{ks}\".\"{name}\" SET label = 'the needle' WHERE bucket = {last} AND id = 1004"),
        )
        .await;
        // A filter reads 100 000 rows a page and says so when it found
        // nothing in them; the next page goes on from there and ends.
        let mut search = PageRequest {
            filters: vec![contains("label", "NEEDLE")],
            ..PageRequest::first(table.clone())
        };
        let started = Instant::now();
        assert_eq!(page_failure(db, &search).await, "cassandra_scan_limit");
        let first = started.elapsed();
        search.offset = 500;
        let found = page(db, &search).await;
        assert_eq!(found.rows.len(), 1);
        assert_eq!(found.rows[0][at(&found, "id")], integer(1004));
        assert!(!found.truncated);
        search.offset = 1000;
        assert!(page(db, &search).await.rows.is_empty());
        // A filter that matches often fills its page long before the limit.
        let often = page(
            db,
            &PageRequest {
                filters: vec![contains("label", "/100")],
                ..PageRequest::first(table.clone())
            },
        )
        .await;
        assert_eq!(often.rows.len(), 500);
        assert!(!often.truncated);
        // An order needs every row: refused beyond 20 000 of them.
        let mut ordered = PageRequest {
            order_by: Some(("label".into(), false)),
            ..PageRequest::first(table.clone())
        };
        assert_eq!(page_failure(db, &ordered).await, "cassandra_sort_limit");
        // The partition key is asked of the server: one partition, read in
        // pages, and small enough to be ordered here.
        ordered.filters = vec![contains("bucket", "7")];
        ordered.order_by = Some(("id".into(), true));
        assert!(db.page_text(&ordered).unwrap().contains("WHERE \"bucket\" = 7"));
        let top = page(db, &ordered).await;
        assert_eq!(top.rows.len(), 500);
        assert_eq!(ids(&top)[..3], [1004, 1003, 1002]);
        ordered.offset = 1000;
        assert_eq!(ids(&page(db, &ordered).await), [4, 3, 2, 1, 0]);
        let mut partition = PageRequest {
            filters: vec![contains("bucket", "7")],
            ..PageRequest::first(table.clone())
        };
        let mut read = 0;
        for index in 0..4u64 {
            partition.offset = index * 500;
            let rows = page(db, &partition).await;
            assert!(rows.rows.iter().all(|row| row[0] == integer(7)));
            read += rows.rows.len();
        }
        assert_eq!(read, 1005);
        // A far page of the whole table is reached by reading up to it,
        // and the editor stops a result at its row limit.
        let mut far = PageRequest::first(table.clone());
        far.offset = 100_000;
        let started = Instant::now();
        assert_eq!(page(db, &far).await.rows.len(), 500);
        let walked = started.elapsed();
        far.offset = 100_500;
        assert!(page(db, &far).await.rows.is_empty());
        let everything = run(db, &format!("SELECT * FROM \"{ks}\".\"{name}\"")).await;
        assert_eq!(everything.rows.len(), 20_000);
        assert!(everything.truncated);
        (loaded, first, walked)
    })
    .await;
    dropped(&db, &made).await;
    db.close().await;
    match outcome {
        Ok((loaded, first, walked)) => println!(
            "{}: 100 500 rows written in {} ms; a filter without a match stopped after 100 000 rows in {} ms and found its row on the next page; an order of the table refused, of one partition made; a partition key filter read one partition; row 100 001 reached by reading in {} ms; the editor stopped at 20 000 rows",
            profile.product_label(),
            loaded.as_millis(),
            first.as_millis(),
            walked.as_millis()
        ),
        Err(failure) => std::panic::resume_unwind(failure.into_panic()),
    }
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

/// Why a profile could not be connected, or what its first statement said.
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

/// The three TLS modes of a profile against a node that speaks TLS, and
/// what a connection that cannot be made is called.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Cassandra server and BAZA_DB_TEST_TLS_CERT, _KEY and _CA"]
async fn tls_is_encrypted_and_verified() {
    let plain = profile();
    let ca = std::path::PathBuf::from(required("BAZA_DB_TEST_TLS_CA"));
    let port = tls_front(&plain).await;
    let with = |host: &str, tls: TlsMode, ca_file: Option<std::path::PathBuf>| {
        let mut profile = plain.clone();
        profile.host = host.into();
        profile.port = port;
        profile.tls = tls;
        profile.ca_file = ca_file;
        profile
    };
    // Encrypted without asking who answers.
    assert_eq!(
        attempt(with("localhost", TlsMode::Require, None)).await,
        "1"
    );
    assert_eq!(
        attempt(with("127.0.0.1", TlsMode::Require, None)).await,
        "1"
    );
    // Verified: the bundled roots do not know the test authority, its own
    // file does, and only for the name the certificate was issued to. The
    // driver dials an address; the name that is verified is the profile's.
    assert_eq!(
        attempt(with("localhost", TlsMode::VerifyFull, None)).await,
        "database_certificate_rejected"
    );
    assert_eq!(
        attempt(with("localhost", TlsMode::VerifyFull, Some(ca.clone()))).await,
        "1"
    );
    assert_eq!(
        attempt(with("127.0.0.1", TlsMode::VerifyFull, Some(ca.clone()))).await,
        "database_certificate_rejected"
    );
    // The whole work of a session through TLS.
    let mut secure = with("localhost", TlsMode::VerifyFull, Some(ca.clone()));
    secure.set_option("system_keyspaces", "true");
    let db = Database::connect(&secure, None).await.unwrap();
    let local = db
        .tables()
        .await
        .unwrap()
        .into_iter()
        .find(|found| found.schema == "system" && found.name == "local")
        .expect("system.local is listed");
    assert_eq!(page(&db, &PageRequest::first(local)).await.rows.len(), 1);
    assert_eq!(db.server_stats().await.unwrap().rows.len(), 1);
    db.close().await;
    // TLS asked of a node that has none: said in words.
    let mut encrypted = plain.clone();
    encrypted.tls = TlsMode::Require;
    assert_eq!(attempt(encrypted).await, "database_tls_unsupported");
    // A node that only speaks TLS, asked without it.
    let refused = attempt(with("localhost", TlsMode::Disable, None)).await;
    assert!(
        matches!(
            refused.as_str(),
            "database_network_error" | "cassandra_handshake_failed" | "database_connection_timeout"
        ),
        "{refused}"
    );
    // A port nobody listens on, a keyspace that is not there, a product
    // this version cannot reach.
    let mut closed = plain.clone();
    closed.port = 1;
    assert_eq!(attempt(closed).await, "database_network_error");
    let mut missing = plain.clone();
    missing.database = "bazaqa_no_such_keyspace".into();
    assert_eq!(attempt(missing).await, "database_not_found");
    let mut astra = plain.clone();
    astra.product = "datastax-astra-db".into();
    assert_eq!(attempt(astra).await, "cassandra_astra_unsupported");
    // An account given to a node without sign-in is not an error.
    let mut account = plain.clone();
    account.username = "cassandra".into();
    account.password = "cassandra".into();
    let signed = attempt(account).await;
    println!(
        "{}: TLS without verification, with the bundled roots (rejected), with the CA file (accepted for its name only), a session through TLS, TLS on a plain port and a plain connection to a TLS port explained, a closed port, a missing keyspace; with the default account: {signed}",
        plain.product_label()
    );
}

/// Sign-in, on a node that asks for an account (`PasswordAuthenticator`):
/// the account of `BAZA_DB_TEST_USER` and `BAZA_DB_TEST_PASSWORD` is let in,
/// a wrong password and no account at all are refused in words.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a Cassandra server with sign-in: BAZA_DB_TEST_USER and BAZA_DB_TEST_PASSWORD"]
async fn an_account_is_signed_in() {
    let right = profile();
    assert!(!right.username.is_empty(), "BAZA_DB_TEST_USER is required");
    assert_eq!(attempt(right.clone()).await, "1");
    let mut wrong = right.clone();
    wrong.password.push_str("-wrong");
    assert_eq!(attempt(wrong).await, "database_login_rejected");
    let mut unknown = right.clone();
    unknown.username = "bazaqa_nobody".into();
    assert_eq!(attempt(unknown).await, "database_login_rejected");
    let mut nobody = right.clone();
    nobody.username.clear();
    nobody.password.clear();
    assert_eq!(attempt(nobody).await, "database_login_rejected");
    // The whole work of a session as that account.
    let db = connect(&right).await;
    assert_eq!(db.server_stats().await.unwrap().rows.len(), 1);
    db.close().await;
    println!(
        "{}: the account signed in; a wrong password, an unknown account and no account refused as a rejected sign-in",
        right.product_label()
    );
}
