//! What only ClickHouse has, checked against a real server. Ignored by
//! default and described by the variables of the driver checklist
//! (`tests/driver_checklist.rs`): `BAZA_DB_TEST_PORT` (the HTTP interface),
//! `BAZA_DB_TEST_USER`, `BAZA_DB_TEST_PASSWORD`, `BAZA_DB_TEST_DATABASE`,
//! optionally `BAZA_DB_TEST_HOST` and `BAZA_DB_TEST_TLS`.
//!
//! ```text
//! cargo test -p baza-core --lib clickhouse::live -- --ignored --nocapture --test-threads 1
//! ```
//!
//! Optional: `BAZA_DB_TEST_NATIVE_PORT` (the native protocol port of the
//! same server, which the driver has to name as the wrong one), and for
//! `certificates_are_verified_against_the_name_of_the_profile` a TLS
//! endpoint in front of the server: `BAZA_DB_TEST_TLS_PORT`,
//! `BAZA_DB_TEST_CA_FILE` (the authority of its certificate),
//! `BAZA_DB_TEST_TLS_NAME` (a DNS name in the certificate that does not
//! have to resolve), `BAZA_DB_TEST_TLS_ADDRESS` (an IP address in the
//! certificate that nothing has to answer at) and
//! `BAZA_DB_TEST_OTHER_CA_FILE` (any other authority). `BAZA_DB_TEST_HOST`
//! has to be a loopback address named in the certificate.
//!
//! The account has to be allowed to create tables in that database, and
//! for the test of a read-only account to create a user. Only tables whose
//! names begin with `baza_live_` and the user `baza_live_reader` are
//! created, and they are dropped at the end.
use super::*;
use crate::builder::{Filter, FilterOp};
use crate::database::Database;
use crate::model::ConnectionProfile;

fn variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn profile() -> ConnectionProfile {
    let required = |name: &str| variable(name).unwrap_or_else(|| panic!("{name} is required"));
    ConnectionProfile {
        name: "Real ClickHouse".into(),
        engine: ENGINE,
        host: variable("BAZA_DB_TEST_HOST").unwrap_or_else(|| "127.0.0.1".into()),
        port: required("BAZA_DB_TEST_PORT").parse().unwrap(),
        database: required("BAZA_DB_TEST_DATABASE"),
        username: required("BAZA_DB_TEST_USER"),
        password: variable("BAZA_DB_TEST_PASSWORD").unwrap_or_default(),
        tls: match variable("BAZA_DB_TEST_TLS").as_deref() {
            Some("require") => TlsMode::Require,
            Some("verify_full") => TlsMode::VerifyFull,
            _ => TlsMode::Disable,
        },
        ..ConnectionProfile::default()
    }
}

async fn open() -> Database {
    Database::connect(&profile(), None).await.unwrap()
}

async fn refused_connection(profile: &ConnectionProfile) -> String {
    match Database::connect(profile, None).await {
        Ok(_) => panic!("the connection was opened"),
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

fn live(name: &str) -> TableRef {
    TableRef {
        schema: profile().database,
        name: name.into(),
        kind: "table".into(),
    }
}

async fn drop_tables(db: &Database, names: &[&str]) {
    for name in names {
        run(
            db,
            &format!("DROP TABLE IF EXISTS {}", qualified(&live(name)).unwrap()),
        )
        .await;
    }
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

async fn page(db: &Database, request: &PageRequest) -> QueryResult {
    db.read_page(request, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("{error}: {}", db.page_text(request).unwrap()))
}

/// The statements of other requests that are running and hold `needle`.
async fn running(db: &Database, needle: &str) -> usize {
    let result = run(
        db,
        &format!(
            "SELECT count() FROM system.processes WHERE query LIKE '%{needle}%' AND query NOT LIKE '%system.processes%'"
        ),
    )
    .await;
    result.rows[0][0].display().parse().unwrap()
}

async fn gone(db: &Database, needle: &str) {
    for _ in 0..40 {
        if running(db, needle).await == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("a statement with {needle} still runs on the server");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real ClickHouse server described by BAZA_DB_TEST_* variables"]
async fn values_of_every_type_arrive_as_typed_cells() {
    let db = open().await;
    let result = run(
        &db,
        "SELECT \
         toInt8(-8) AS i8, toInt64(-9223372036854775808) AS i64, \
         toInt128('-170141183460469231731687303715884105728') AS i128, \
         toInt256('-57896044618658097711785492504343953926634992332820282019728792003956564819968') AS i256, \
         toUInt8(255) AS u8, toUInt64(18446744073709551615) AS u64, \
         toUInt256('115792089237316195423570985008687907853269984665640564039457584007913129639935') AS u256, \
         toFloat32(0.1) AS f32, 1.5e300 AS f64, 0 / 0 AS not_a_number, \
         toDecimal32('-12.50', 2) AS d32, toDecimal128('12345678901234567890.1200', 4) AS d128, \
         toDecimal256('1234567890123456789012345678901234567890.123456789', 9) AS d256, \
         true AS yes, \
         toDate('2024-05-06') AS day, toDate32('1900-01-01') AS old_day, \
         toDateTime('2024-05-06 07:08:09', 'UTC') AS moment, \
         toDateTime64('2024-05-06 07:08:09.123456', 6, 'Europe/Warsaw') AS exact, \
         'Zażółć gęślą jaźń 🦀' AS plain, 'tab\\there\\nline \\\\ \\'q\\'' AS escaped, \
         unhex('FF00FE41') AS bytes, toFixedString('ab', 4) AS fixed, \
         toUUID('01234567-89ab-cdef-0123-456789abcdef') AS id, \
         toIPv4('192.168.1.42') AS v4, toIPv6('2001:db8::1') AS v6, \
         CAST('b' AS Enum8('a' = 1, 'b' = 2)) AS choice, \
         CAST(NULL AS Nullable(Int32)) AS nothing, toLowCardinality('low') AS low, \
         ['a\\tb', 'it\\'s'] AS list, (1, 'x') AS pair, map('k', 2) AS dictionary, \
         CAST('{\"a\":[1,2],\"b\":\"x\"}' AS JSON) AS document, \
         uniqState(toUInt64(1)) AS state, toIntervalDay(3) AS span",
    )
    .await;
    let types: Vec<&str> = result
        .columns
        .iter()
        .map(|column| column.data_type.as_str())
        .collect();
    assert_eq!(
        types,
        [
            "Int8",
            "Int64",
            "Int128",
            "Int256",
            "UInt8",
            "UInt64",
            "UInt256",
            "Float32",
            "Float64",
            "Float64",
            "Decimal(9, 2)",
            "Decimal(38, 4)",
            "Decimal(76, 9)",
            "Bool",
            "Date",
            "Date32",
            "DateTime('UTC')",
            "DateTime64(6, 'Europe/Warsaw')",
            "String",
            "String",
            "String",
            "FixedString(4)",
            "UUID",
            "IPv4",
            "IPv6",
            "Enum8('a' = 1, 'b' = 2)",
            "Nullable(Int32)",
            "LowCardinality(String)",
            "Array(String)",
            "Tuple(UInt8, String)",
            "Map(String, UInt8)",
            "JSON",
            "AggregateFunction(uniq, UInt64)",
            "IntervalDay",
        ]
    );
    let decimal = |value: &str| Cell::Decimal(value.into());
    let whole = |value: &str| Cell::Integer(value.into());
    assert_eq!(
        result.rows[0],
        [
            whole("-8"),
            whole("-9223372036854775808"),
            whole("-170141183460469231731687303715884105728"),
            whole("-57896044618658097711785492504343953926634992332820282019728792003956564819968"),
            whole("255"),
            whole("18446744073709551615"),
            whole("115792089237316195423570985008687907853269984665640564039457584007913129639935"),
            Cell::Float("0.1".into()),
            Cell::Float("1.5e300".into()),
            Cell::Float("nan".into()),
            decimal("-12.50"),
            decimal("12345678901234567890.1200"),
            decimal("1234567890123456789012345678901234567890.123456789"),
            Cell::Bool(true),
            text("2024-05-06"),
            text("1900-01-01"),
            text("2024-05-06 07:08:09"),
            text("2024-05-06 07:08:09.123456"),
            text("Zażółć gęślą jaźń 🦀"),
            text("tab\there\nline \\ 'q'"),
            Cell::Bytes(vec![0xff, 0, 0xfe, b'A']),
            text("ab"),
            text("01234567-89ab-cdef-0123-456789abcdef"),
            text("192.168.1.42"),
            text("2001:db8::1"),
            text("b"),
            Cell::Null,
            text("low"),
            text("['a\\tb','it\\'s']"),
            text("(1,'x')"),
            text("{'k':2}"),
            Cell::Json("{\"a\":[1,2],\"b\":\"x\"}".into()),
            Cell::Unsupported("<AggregateFunction(uniq, UInt64)>".into()),
            whole("3"),
        ]
    );
    db.close().await;
}

/// A session of the driver itself, for what `Database` does not show.
async fn session(profile: &ConnectionProfile) -> ClickHouseSession {
    let request = ConnectRequest {
        profile,
        product: profile.product_spec(),
        forwarded: None,
    };
    super::open(&request).await.unwrap()
}

/// Whatever a column holds, the same text and the same bytes come back
/// through an insert of the grid, through an import and through a filter,
/// and a cell is found again by the value the grid showed for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real ClickHouse server described by BAZA_DB_TEST_* variables"]
async fn text_and_values_are_written_back_exactly() {
    let db = open().await;
    drop_tables(&db, &["baza_live_exact"]).await;
    let table = live("baza_live_exact");
    let quoted = qualified(&table).unwrap();
    run(
        &db,
        &format!(
            "CREATE TABLE {quoted} (id Int64, note Nullable(String), code FixedString(4), \
             big UInt256, exact Nullable(Decimal(38, 10)), ratio Float32, flag Bool, \
             day Date, moment DateTime64(3, 'Europe/Warsaw'), ident UUID, address IPv6, \
             choice Enum8('new' = 1, 'paid' = 2), tags Array(String), pairs Map(String, Int32), \
             point Tuple(x Float64, y Float64), low LowCardinality(Nullable(String))) \
             ENGINE = MergeTree ORDER BY id"
        ),
    )
    .await;
    let columns = db.columns(&table).await.unwrap();
    assert_eq!(columns.len(), 16);
    assert!(columns[0].primary_key && !columns[0].nullable);
    assert!(columns[1].nullable && columns[15].nullable && !columns[2].nullable);
    assert_eq!(columns[8].data_type, "DateTime64(3, 'Europe/Warsaw')");
    // Texts that are hard on a parser: quotes, backslashes, tabs, line
    // ends, what looks like NULL, what looks like a statement.
    let hard = [
        "it's \"quoted\" \\ back",
        "tab\there\nnew line\r\nand a return",
        "\\N",
        "'; DROP TABLE x; --",
        "100% _under_ !bang",
        "Zażółć gęślą jaźń 🦀",
        "",
        "{name:Type} $1 ? `tick`",
    ];
    // One row through the insert of the grid, with every column. The
    // interface hands a value over by the look of its type name, so the
    // kinds of these cells are deliberately not the kinds of the columns.
    let whole = |value: &str| Cell::Integer(value.to_owned());
    db.apply(&RowChange::InsertRow {
        table: table.clone(),
        values: vec![
            ("id".into(), integer(1)),
            ("note".into(), text(hard[0])),
            ("code".into(), text("ab")),
            (
                "big".into(),
                whole("115792089237316195423570985008687907853269984665640564039457584007913129639935"),
            ),
            ("exact".into(), Cell::Decimal("12345678901234567890.12".into())),
            ("ratio".into(), Cell::Float("0.1".into())),
            ("flag".into(), text("true")),
            ("day".into(), text("2024-05-06")),
            ("moment".into(), text("2024-05-06 07:08:09.123")),
            ("ident".into(), text("01234567-89ab-cdef-0123-456789abcdef")),
            ("address".into(), text("2001:db8::1")),
            ("choice".into(), text("paid")),
            ("tags".into(), text("['a\\tb','it\\'s']")),
            // "POINT" and "INT32" hold "INT": these arrive as integers.
            ("pairs".into(), whole("{'k':1,'q':2}")),
            ("point".into(), whole("(54.35,18.64)")),
            ("low".into(), Cell::Null),
        ],
    })
    .await
    .unwrap();
    // The rest through an import: texts, NULL, an empty text, and bytes
    // that are not text.
    let names = ["id", "note", "code", "tags"].map(str::to_owned);
    let mut rows: Vec<Vec<Cell>> = hard
        .iter()
        .enumerate()
        .map(|(index, value)| {
            vec![
                integer(index as i64 + 10),
                text(value),
                text("zz"),
                text(&format!("[{}]", quote_text(value))),
            ]
        })
        .collect();
    rows.push(vec![integer(30), Cell::Null, text(""), text("[]")]);
    rows.push(vec![
        integer(31),
        Cell::Bytes(vec![0xff, 0, b'\t', b'\n', 0xfe]),
        Cell::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
        text("['x']"),
    ]);
    let imported = db
        .import_rows(&table, &names, &rows, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(imported.affected, rows.len() as u64);
    let all = page(
        &db,
        &PageRequest {
            order_by: Some(("id".into(), false)),
            ..PageRequest::first(table.clone())
        },
    )
    .await;
    assert_eq!(all.rows.len(), 1 + rows.len());
    assert_eq!(
        all.rows[0],
        [
            integer(1),
            text(hard[0]),
            text("ab"),
            whole("115792089237316195423570985008687907853269984665640564039457584007913129639935"),
            Cell::Decimal("12345678901234567890.1200000000".into()),
            Cell::Float("0.1".into()),
            Cell::Bool(true),
            text("2024-05-06"),
            text("2024-05-06 07:08:09.123"),
            text("01234567-89ab-cdef-0123-456789abcdef"),
            text("2001:db8::1"),
            text("paid"),
            text("['a\\tb','it\\'s']"),
            text("{'k':1,'q':2}"),
            text("(54.35,18.64)"),
            Cell::Null,
        ]
    );
    for (index, value) in hard.iter().enumerate() {
        let row = &all.rows[index + 1];
        assert_eq!(row[1], text(value), "note {index}");
        assert_eq!(row[2], text("zz"));
        // The element of the array is the same text, read back by the server.
        let element = run(
            &db,
            &format!("SELECT tags[1] FROM {quoted} WHERE id = {}", index + 10),
        )
        .await;
        assert_eq!(element.rows[0][0], text(value), "array element {index}");
    }
    assert_eq!(all.rows[9][1], Cell::Null);
    assert_eq!(all.rows[9][2], text(""));
    assert_eq!(
        all.rows[10][1],
        Cell::Bytes(vec![0xff, 0, b'\t', b'\n', 0xfe])
    );
    assert_eq!(all.rows[10][2], Cell::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));

    // The filter of the grid finds every one of the texts as it is written.
    for (index, value) in hard
        .iter()
        .enumerate()
        .filter(|(_, value)| !value.is_empty())
    {
        let found = page(
            &db,
            &PageRequest {
                filters: vec![Filter {
                    column: "note".into(),
                    op: FilterOp::Contains,
                    value: text(value),
                }],
                order_by: Some(("id".into(), false)),
                ..PageRequest::first(table.clone())
            },
        )
        .await;
        let ids: Vec<String> = found.rows.iter().map(|row| row[0].display()).collect();
        assert!(
            ids.contains(&(index + 10).to_string()),
            "{value:?} found in {ids:?}"
        );
    }
    // The other kinds of filter take literals of the column types.
    for (column, op, value, expected) in [
        (
            "exact",
            FilterOp::Equal,
            Cell::Decimal("12345678901234567890.12".into()),
            1,
        ),
        (
            "moment",
            FilterOp::Greater,
            text("2024-05-06 07:08:09.000"),
            1,
        ),
        (
            "ident",
            FilterOp::Equal,
            text("01234567-89ab-cdef-0123-456789abcdef"),
            1,
        ),
        ("choice", FilterOp::NotEqual, text("paid"), rows.len()),
        ("note", FilterOp::IsNull, Cell::Null, 1),
        ("id", FilterOp::Less, integer(12), 3),
    ] {
        let found = page(
            &db,
            &PageRequest {
                filters: vec![Filter {
                    column: column.into(),
                    op,
                    value,
                }],
                ..PageRequest::first(table.clone())
            },
        )
        .await;
        assert_eq!(found.rows.len(), expected, "{column}");
    }

    // Every cell of the first row is changed, with the value the grid
    // showed as the old one: the conflict check has to find the row by it.
    let changes = [
        (1, text("changed\t'again'")),
        (2, text("wxyz")),
        (3, whole("7")),
        (4, Cell::Decimal("0.0000000001".into())),
        (5, Cell::Float("2.5".into())),
        (6, Cell::Bool(false)),
        (7, text("1999-12-31")),
        (8, text("2030-01-01 00:00:00.000")),
        (9, text("89abcdef-0123-4567-89ab-cdef01234567")),
        (10, text("::ffff:192.168.1.42")),
        (11, text("new")),
        (12, text("['z']")),
        (13, text("{'only':9}")),
        (14, text("(1,2)")),
        (15, text("now set")),
    ];
    for (index, new) in &changes {
        let edit = RowChange::UpdateCell {
            table: table.clone(),
            column: columns[*index].name.clone(),
            old: all.rows[0][*index].clone(),
            new: new.clone(),
            key: key(1),
        };
        db.apply(&edit).await.unwrap_or_else(|error| {
            panic!(
                "{}: {error}: {}",
                columns[*index].name,
                db.change_text(&edit).unwrap()
            )
        });
        assert_eq!(
            db.apply(&edit).await.unwrap_err().to_string(),
            "row_changed_reload_required",
            "{}: the old value is gone",
            columns[*index].name
        );
    }
    let after = run(&db, &format!("SELECT * FROM {quoted} WHERE id = 1")).await;
    for (index, new) in &changes {
        assert_eq!(&after.rows[0][*index], new, "{}", columns[*index].name);
    }
    // A value the type does not take is refused, and nothing changes.
    for (column, value) in [("day", "not a date"), ("choice", "refunded"), ("big", "-1")] {
        let index = columns
            .iter()
            .position(|found| found.name == column)
            .unwrap();
        assert_eq!(
            db.apply(&RowChange::UpdateCell {
                table: table.clone(),
                column: column.into(),
                old: after.rows[0][index].clone(),
                new: text(value),
                key: key(1),
            })
            .await
            .unwrap_err()
            .to_string(),
            "clickhouse_value_rejected",
            "{column}"
        );
    }
    assert_eq!(
        run(&db, &format!("SELECT * FROM {quoted} WHERE id = 1"))
            .await
            .rows,
        after.rows
    );
    // A row that takes every default.
    db.apply(&RowChange::InsertRow {
        table: table.clone(),
        values: Vec::new(),
    })
    .await
    .unwrap();
    assert_eq!(
        run(&db, &format!("SELECT count() FROM {quoted} WHERE id = 0"))
            .await
            .rows,
        [vec![integer(1)]]
    );
    drop_tables(&db, &["baza_live_exact"]).await;
    db.close().await;
}

/// What can be done to a row depends on the engine of its table, and what
/// cannot be done is said in a sentence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real ClickHouse server described by BAZA_DB_TEST_* variables"]
async fn rows_are_changed_as_far_as_the_table_engine_allows() {
    let db = open().await;
    let names = [
        "baza_live_merge",
        "baza_live_replacing",
        "baza_live_projection",
        "baza_live_keyless",
        "baza_live_memory",
        "baza_live_log",
        "baza_live_twins",
    ];
    drop_tables(&db, &names).await;
    let create = |name: &str, tail: &str| {
        format!(
            "CREATE TABLE {} (id Int64, note String{tail}",
            qualified(&live(name)).unwrap()
        )
    };
    run(
        &db,
        &create("baza_live_merge", ") ENGINE = MergeTree ORDER BY id"),
    )
    .await;
    run(
        &db,
        &create(
            "baza_live_replacing",
            ") ENGINE = ReplacingMergeTree ORDER BY id",
        ),
    )
    .await;
    run(
        &db,
        &create(
            "baza_live_projection",
            ", PROJECTION by_note (SELECT note, count() GROUP BY note)) ENGINE = MergeTree ORDER BY id",
        ),
    )
    .await;
    run(
        &db,
        &create("baza_live_keyless", ") ENGINE = MergeTree ORDER BY tuple()"),
    )
    .await;
    run(&db, &create("baza_live_memory", ") ENGINE = Memory")).await;
    run(&db, &create("baza_live_log", ") ENGINE = Log")).await;
    run(
        &db,
        &create("baza_live_twins", ") ENGINE = MergeTree ORDER BY id"),
    )
    .await;
    let update = |name: &str, old: &str| RowChange::UpdateCell {
        table: live(name),
        column: "note".into(),
        old: text(old),
        new: text("changed"),
        key: key(1),
    };
    let delete = |name: &str, id: i64| RowChange::DeleteRow {
        table: live(name),
        key: key(id),
    };
    let notes = |name: &str| {
        let statement = format!(
            "SELECT id, note FROM {} ORDER BY id, note",
            qualified(&live(name)).unwrap()
        );
        let db = db.clone();
        async move { run(&db, &statement).await.rows }
    };
    for name in names {
        let table = live(name);
        // Every engine takes new rows.
        for id in [1, 2] {
            db.apply(&RowChange::InsertRow {
                table: table.clone(),
                values: vec![("id".into(), integer(id)), ("note".into(), text("first"))],
            })
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        }
        let listed = db
            .tables()
            .await
            .unwrap()
            .into_iter()
            .find(|found| found.name == name)
            .unwrap();
        assert_eq!(listed.kind, "table", "{name}");
        let columns = db.columns(&table).await.unwrap();
        let keyed = columns[0].primary_key;
        match name {
            // The MergeTree family: a mutation for the cell, a lightweight
            // delete for the row. A table with a projection refuses the
            // lightweight delete and gets the mutation instead.
            "baza_live_merge" | "baza_live_replacing" | "baza_live_projection" => {
                assert!(keyed, "{name}");
                db.apply(&update(name, "first")).await.unwrap();
                assert_eq!(
                    db.apply(&update(name, "first"))
                        .await
                        .unwrap_err()
                        .to_string(),
                    "row_changed_reload_required"
                );
                assert!(
                    db.change_text(&delete(name, 2))
                        .unwrap()
                        .starts_with("DELETE FROM")
                );
                db.apply(&delete(name, 2)).await.unwrap();
                assert_eq!(
                    db.apply(&delete(name, 2)).await.unwrap_err().to_string(),
                    "row_changed_reload_required"
                );
                assert_eq!(
                    notes(name).await,
                    [vec![integer(1), text("changed")]],
                    "{name}"
                );
                // A key column keeps its value.
                assert_eq!(
                    db.apply(&RowChange::UpdateCell {
                        table: table.clone(),
                        column: "id".into(),
                        old: integer(1),
                        new: integer(9),
                        key: key(1),
                    })
                    .await
                    .unwrap_err()
                    .to_string(),
                    "clickhouse_key_column"
                );
            }
            // Without a key the grid has nothing to point at a row with:
            // the columns say so, and a change that comes anyway is refused.
            "baza_live_keyless" => {
                assert!(columns.iter().all(|column| !column.primary_key));
                let keyless = RowChange::DeleteRow {
                    table: table.clone(),
                    key: Vec::new(),
                };
                assert_eq!(
                    db.apply(&keyless).await.unwrap_err().to_string(),
                    "clickhouse_table_without_key"
                );
                assert_eq!(notes(name).await.len(), 2);
            }
            // Memory has no key either, but its rows can be changed by a
            // statement: with a key handed over it is the mutation that
            // runs, and the delete is the heavier form, because the engine
            // has no lightweight one.
            "baza_live_memory" => {
                assert!(!keyed);
                db.apply(&update(name, "first")).await.unwrap();
                assert!(
                    db.change_text(&delete(name, 2))
                        .unwrap()
                        .starts_with("ALTER TABLE")
                );
                db.apply(&delete(name, 2)).await.unwrap();
                assert_eq!(notes(name).await, [vec![integer(1), text("changed")]]);
            }
            // Log only takes new rows.
            "baza_live_log" => {
                assert!(!keyed);
                for change in [update(name, "first"), delete(name, 2)] {
                    assert_eq!(
                        db.apply(&change).await.unwrap_err().to_string(),
                        "clickhouse_engine_no_mutations"
                    );
                }
                assert_eq!(notes(name).await.len(), 2);
            }
            // A key is not unique: two rows of one key are both left alone.
            "baza_live_twins" => {
                db.apply(&RowChange::InsertRow {
                    table: table.clone(),
                    values: vec![("id".into(), integer(1)), ("note".into(), text("first"))],
                })
                .await
                .unwrap();
                for change in [update(name, "first"), delete(name, 1)] {
                    assert_eq!(
                        db.apply(&change).await.unwrap_err().to_string(),
                        "clickhouse_key_not_unique"
                    );
                }
                assert_eq!(notes(name).await.len(), 3);
                // The row with a key of its own is still changed.
                db.apply(&delete(name, 2)).await.unwrap();
                assert_eq!(notes(name).await.len(), 2);
            }
            _ => unreachable!(),
        }
    }
    // A view is listed as one, and so is what is neither table nor view.
    let view = qualified(&live("baza_live_view")).unwrap();
    run(
        &db,
        &format!(
            "CREATE OR REPLACE VIEW {view} AS SELECT * FROM {}",
            qualified(&live("baza_live_merge")).unwrap()
        ),
    )
    .await;
    let listed = db.tables().await.unwrap();
    assert_eq!(
        listed
            .iter()
            .find(|found| found.name == "baza_live_view")
            .map(|found| found.kind.as_str()),
        Some("view")
    );
    // The system databases stay out of the side panel unless asked for.
    assert!(listed.iter().all(|found| found.schema != "system"));
    let mut with_system = profile();
    with_system.set_option("show_system", "true");
    let wide = Database::connect(&with_system, None).await.unwrap();
    let all = wide.tables().await.unwrap();
    assert!(
        all.iter()
            .any(|found| found.schema == "system" && found.name == "processes")
    );
    assert!(
        all.iter()
            .filter(|found| found.schema == "system" && found.name == "processes")
            .all(|found| !found.kind.contains("table")),
        "a view of the server onto itself is not something to edit"
    );
    wide.close().await;
    run(&db, &format!("DROP VIEW {view}")).await;
    drop_tables(&db, &names).await;
    db.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a real ClickHouse server described by BAZA_DB_TEST_* variables"]
async fn a_statement_is_stopped_and_a_large_result_is_cut() {
    let db = open().await;
    const SLOW: &str = "SELECT sum(sleepEachRow(1)) FROM numbers(60) SETTINGS max_block_size = 1";
    // Cancelled before it starts: nothing is sent.
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        db.query("SELECT 1", cancelled)
            .await
            .unwrap_err()
            .to_string(),
        "query_cancelled"
    );
    // Cancelled while it runs: gone from the server as well.
    let token = CancellationToken::new();
    let worker = db.clone();
    let cancel = token.clone();
    let task = tokio::spawn(async move { worker.query(SLOW, cancel).await });
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(running(&db, "sleepEachRow").await, 1);
    let asked = Instant::now();
    token.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the statement was stopped")
        .unwrap();
    assert_eq!(stopped.unwrap_err().to_string(), "query_cancelled");
    assert!(asked.elapsed() < Duration::from_secs(2));
    gone(&db, "sleepEachRow").await;

    // A statement that runs out of time is ended by the server itself.
    let own = session(&profile()).await;
    let started = Instant::now();
    let late = own
        .request(
            SLOW.as_bytes().to_vec(),
            Sending {
                deadline: Instant::now() + Duration::from_secs(2),
                ..Sending::now()
            },
            &CancellationToken::new(),
        )
        .await
        .map(|_| ())
        .map_err(plain);
    assert_eq!(late.unwrap_err().to_string(), "query_timeout");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    gone(&db, "sleepEachRow").await;

    // A result without an end: cut at what a result may hold, and the
    // statement behind it is stopped.
    let started = Instant::now();
    let cut = run(
        &db,
        "SELECT number, toString(number) AS label FROM system.numbers",
    )
    .await;
    assert!(cut.truncated);
    assert_eq!(cut.rows.len(), MAX_RESULT_ROWS);
    assert_eq!(
        cut.rows[MAX_RESULT_ROWS - 1][0],
        integer(MAX_RESULT_ROWS as i64 - 1)
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    gone(&db, "system.numbers").await;
    // A result that fits is not cut, to the last row.
    let whole = run(
        &db,
        &format!("SELECT number FROM numbers({MAX_RESULT_ROWS})"),
    )
    .await;
    assert!(!whole.truncated);
    assert_eq!(whole.rows.len(), MAX_RESULT_ROWS);
    // The limit is on what comes back, never on what a statement reads or
    // writes on the way.
    let counted = run(
        &db,
        "SELECT count() FROM (SELECT number FROM numbers(1000000))",
    )
    .await;
    assert_eq!(counted.rows, [vec![integer(1_000_000)]]);
    drop_tables(&db, &["baza_live_many"]).await;
    let many = qualified(&live("baza_live_many")).unwrap();
    let written = run(
        &db,
        &format!(
            "CREATE TABLE {many} (n UInt64) ENGINE = MergeTree ORDER BY n; \
             INSERT INTO {many} SELECT number FROM numbers(150000)"
        ),
    )
    .await;
    assert_eq!(written.affected, 150_000);
    assert_eq!(
        run(&db, &format!("SELECT count() FROM {many}")).await.rows,
        [vec![integer(150_000)]]
    );
    // A page of the grid is never cut: its limit is part of the statement.
    let last = page(
        &db,
        &PageRequest {
            limit: 20_000,
            offset: 140_000,
            ..PageRequest::first(live("baza_live_many"))
        },
    )
    .await;
    assert!(!last.truncated);
    assert_eq!(last.rows.len(), 10_000);
    assert_eq!(last.rows[9_999], [integer(149_999)]);
    // A failure that comes after the first rows were sent is a failure,
    // not a shorter result.
    assert_eq!(
        fails(
            &db,
            "SELECT repeat('x', 300) AS filler, throwIf(number = 15000, 'late') AS check \
             FROM numbers(18000) SETTINGS max_block_size = 500"
        )
        .await,
        "database_error [395]"
    );
    drop_tables(&db, &["baza_live_many"]).await;
    db.close().await;
}

/// One request is one statement: a script goes statement by statement,
/// inside a session of the server that lasts for the script.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real ClickHouse server described by BAZA_DB_TEST_* variables"]
async fn a_script_shares_a_session_of_the_server() {
    let db = open().await;
    let database = profile().database;
    // A setting, a temporary table and the current database of one
    // statement are there for the next.
    let result = run(
        &db,
        "SET max_threads = 3; -- a comment; with a semicolon\n\
         CREATE TEMPORARY TABLE scratch (n UInt8, label String); \
         INSERT INTO scratch VALUES (1, 'a;b'), (2, 'it''s'), (3, 'c\\'d'); \
         USE system; \
         SELECT count() AS rows, max(label) AS last, getSetting('max_threads') AS threads, currentDatabase() AS here FROM scratch",
    )
    .await;
    assert_eq!(result.affected, 3);
    assert_eq!(
        result.rows,
        [vec![integer(3), text("it's"), integer(3), text("system")]]
    );
    // The next run starts in the database of the profile again, and the
    // temporary table went with the script.
    assert_eq!(
        run(&db, "SELECT currentDatabase()").await.rows,
        [vec![text(&database)]]
    );
    assert_eq!(
        fails(&db, "SELECT * FROM scratch").await,
        "clickhouse_unknown_table"
    );
    // The last result set is the one that is shown; a statement without
    // one in between does not take it away.
    let last = run(
        &db,
        "SELECT 1 AS first; SELECT 2 AS second, 'x' AS mark; SET max_threads = 1",
    )
    .await;
    assert_eq!(last.columns[0].name, "second");
    assert_eq!(last.rows, [vec![integer(2), text("x")]]);
    // A failure stops the script where it happens.
    drop_tables(&db, &["baza_live_steps"]).await;
    let steps = qualified(&live("baza_live_steps")).unwrap();
    assert_eq!(
        fails(
            &db,
            &format!(
                "CREATE TABLE {steps} (n UInt8) ENGINE = MergeTree ORDER BY n; \
                 INSERT INTO {steps} VALUES (1); SELEC 2; INSERT INTO {steps} VALUES (3)"
            )
        )
        .await,
        "clickhouse_syntax_error"
    );
    assert_eq!(
        run(&db, &format!("SELECT n FROM {steps}")).await.rows,
        [vec![integer(1)]]
    );
    // What a statement names by itself is answered as text.
    let csv = run(
        &db,
        &format!("SELECT n, 'a,b' AS label FROM {steps} FORMAT CSVWithNames"),
    )
    .await;
    assert_eq!(csv.columns[0].name, "CSVWithNames");
    assert_eq!(
        csv.rows,
        [vec![text("\"n\",\"label\"")], vec![text("1,\"a,b\"")]]
    );
    let json = run(&db, "SELECT 1 AS n FORMAT JSONEachRow").await;
    assert_eq!(json.rows, [vec![text("{\"n\":1}")]]);
    // A binary format is named, never shown.
    let binary = run(
        &db,
        "SELECT toUInt64(18446744073709551615) AS n FROM numbers(2) FORMAT Native",
    )
    .await;
    assert_eq!(binary.rows, [vec![Cell::Unsupported("<Native>".into())]]);
    // Statements that answer with rows of their own.
    let shown = run(&db, &format!("SHOW CREATE TABLE {steps}")).await;
    assert!(shown.rows[0][0].display().contains("ENGINE = MergeTree"));
    assert!(
        shown.rows[0][0].display().contains('\n'),
        "line ends are line ends"
    );
    let described = run(&db, &format!("DESCRIBE TABLE {steps}")).await;
    assert_eq!(described.rows[0][..2], [text("n"), text("UInt8")]);
    assert!(!run(&db, "EXPLAIN SELECT 1").await.rows.is_empty());
    assert_eq!(
        run(&db, &format!("EXISTS TABLE {steps}")).await.rows,
        [vec![integer(1)]]
    );
    // An empty result still has its columns.
    let empty = run(
        &db,
        &format!("SELECT n, toString(n) AS label FROM {steps} WHERE n > 5"),
    )
    .await;
    assert!(empty.rows.is_empty());
    assert_eq!(
        empty
            .columns
            .iter()
            .map(|column| column.data_type.as_str())
            .collect::<Vec<_>>(),
        ["UInt8", "String"]
    );
    drop_tables(&db, &["baza_live_steps"]).await;
    db.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real ClickHouse server described by BAZA_DB_TEST_* variables"]
async fn refusals_have_codes_of_their_own() {
    let base = profile();
    // Why a connection is not opened.
    let mut wrong = base.clone();
    wrong.password.push_str("-wrong");
    assert_eq!(refused_connection(&wrong).await, "database_login_rejected");
    let mut nobody = base.clone();
    nobody.username = "baza_live_no_such_user".into();
    assert_eq!(refused_connection(&nobody).await, "database_login_rejected");
    let mut nowhere = base.clone();
    nowhere.database = "baza_live_no_such_database".into();
    assert_eq!(refused_connection(&nowhere).await, "database_not_found");
    let mut closed = base.clone();
    closed.port = 1;
    assert_eq!(refused_connection(&closed).await, "database_network_error");
    // The port answers, but not in TLS.
    let mut encrypted = base.clone();
    encrypted.tls = TlsMode::Require;
    assert_eq!(
        refused_connection(&encrypted).await,
        "database_tls_unsupported"
    );
    // The native protocol port says what it is, and so does the driver.
    match variable("BAZA_DB_TEST_NATIVE_PORT") {
        Some(port) => {
            let mut native = base.clone();
            native.port = port.parse().unwrap();
            assert_eq!(refused_connection(&native).await, "clickhouse_native_port");
        }
        None => println!("skipped: BAZA_DB_TEST_NATIVE_PORT is not set"),
    }
    // Without a database the account lands in its own.
    let mut anywhere = base.clone();
    anywhere.database.clear();
    let landed = Database::connect(&anywhere, None).await.unwrap();
    assert!(
        !run(&landed, "SELECT currentDatabase()").await.rows[0][0]
            .display()
            .is_empty()
    );
    landed.close().await;

    // Why a statement is refused.
    let db = open().await;
    for (statement, code) in [
        ("SELEC 1", "clickhouse_syntax_error"),
        (
            "SELECT * FROM baza_live_no_such_table",
            "clickhouse_unknown_table",
        ),
        (
            "SELECT * FROM baza_live_no_such_database.t",
            "clickhouse_unknown_database",
        ),
        (
            "SELECT no_such_column FROM system.one",
            "clickhouse_unknown_column",
        ),
        ("SELECT noSuchFunction(1)", "clickhouse_unknown_function"),
        ("SELECT toDate('not a date')", "clickhouse_value_rejected"),
        ("SELECT 1; SELECT throwIf(1, 'x')", "database_error [395]"),
    ] {
        assert_eq!(fails(&db, statement).await, code, "{statement}");
        // What the driver names has a sentence in both languages; the rest
        // carries the number of the server.
        if code.starts_with("clickhouse_") {
            assert!(describe(code, false).is_some() && describe(code, true).is_some());
        }
    }
    // The connection goes on after every one of them.
    assert_eq!(run(&db, "SELECT 1").await.rows, [vec![integer(1)]]);
    db.close().await;
}

/// An account that may not change settings (`readonly = 1`) still reads:
/// the limits of a result are then kept on this side alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real ClickHouse server described by BAZA_DB_TEST_* variables"]
async fn a_read_only_account_reads_and_is_told_why_it_cannot_write() {
    let db = open().await;
    let password = format!("Reader-{}", uuid::Uuid::new_v4().simple());
    let created = db
        .query(
            &format!(
                "DROP USER IF EXISTS baza_live_reader; \
                 CREATE USER baza_live_reader IDENTIFIED WITH sha256_password BY '{password}' SETTINGS readonly = 1; \
                 GRANT SELECT ON *.* TO baza_live_reader"
            ),
            CancellationToken::new(),
        )
        .await;
    if let Err(error) = created {
        println!("skipped: the account may not create users ({error})");
        db.close().await;
        return;
    }
    drop_tables(&db, &["baza_live_guarded"]).await;
    let guarded = qualified(&live("baza_live_guarded")).unwrap();
    run(
        &db,
        &format!(
            "CREATE TABLE {guarded} (id Int64, note String) ENGINE = MergeTree ORDER BY id; \
             INSERT INTO {guarded} VALUES (1, 'kept')"
        ),
    )
    .await;
    let mut reader = profile();
    reader.username = "baza_live_reader".into();
    reader.password = password;
    let own = session(&reader).await;
    assert!(own.limited, "the driver noticed that settings are refused");
    let limited = Database::connect(&reader, None).await.unwrap();
    assert_eq!(
        page(&limited, &PageRequest::first(live("baza_live_guarded")))
            .await
            .rows,
        [vec![integer(1), text("kept")]]
    );
    assert!(
        limited
            .tables()
            .await
            .unwrap()
            .iter()
            .any(|found| found.name == "baza_live_guarded")
    );
    assert_eq!(
        limited
            .columns(&live("baza_live_guarded"))
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(limited.server_stats().await.unwrap().rows.len(), 1);
    // A result is still cut at what a result may hold, and its statement
    // is stopped when the connection goes.
    let started = Instant::now();
    let cut = run(&limited, "SELECT number FROM system.numbers").await;
    assert!(cut.truncated && cut.rows.len() == MAX_RESULT_ROWS);
    assert!(started.elapsed() < Duration::from_secs(10));
    gone(&db, "system.numbers").await;
    // A script still shares its session.
    assert_eq!(
        run(&limited, "USE system; SELECT currentDatabase()")
            .await
            .rows,
        [vec![text("system")]]
    );
    // Nothing can be changed, and the reason is the account.
    for change in [
        RowChange::InsertRow {
            table: live("baza_live_guarded"),
            values: vec![("id".into(), integer(2)), ("note".into(), text("no"))],
        },
        RowChange::UpdateCell {
            table: live("baza_live_guarded"),
            column: "note".into(),
            old: text("kept"),
            new: text("no"),
            key: key(1),
        },
        RowChange::DeleteRow {
            table: live("baza_live_guarded"),
            key: key(1),
        },
    ] {
        assert_eq!(
            limited.apply(&change).await.unwrap_err().to_string(),
            "clickhouse_readonly"
        );
    }
    assert_eq!(
        limited
            .import_rows(
                &live("baza_live_guarded"),
                &["id".to_owned(), "note".to_owned()],
                &[vec![integer(3), text("no")]],
                CancellationToken::new()
            )
            .await
            .unwrap_err()
            .to_string(),
        "clickhouse_readonly"
    );
    // What the account was never granted is refused for that reason.
    assert_eq!(
        fails(&limited, &format!("DROP TABLE {guarded}")).await,
        "clickhouse_access_denied"
    );
    assert_eq!(
        run(&db, &format!("SELECT * FROM {guarded}")).await.rows,
        [vec![integer(1), text("kept")]]
    );
    limited.close().await;
    run(&db, "DROP USER IF EXISTS baza_live_reader").await;
    drop_tables(&db, &["baza_live_guarded"]).await;
    db.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real ClickHouse server described by BAZA_DB_TEST_* variables"]
async fn the_monitor_reads_the_server() {
    let db = open().await;
    // Something for the monitor to see.
    let token = CancellationToken::new();
    let worker = db.clone();
    let cancel = token.clone();
    let busy = tokio::spawn(async move {
        worker
            .query(
                "SELECT sum(sleepEachRow(1)) FROM numbers(30) SETTINGS max_block_size = 1",
                cancel,
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let stats = db.server_stats().await.unwrap();
    token.cancel();
    let _ = busy.await;
    assert_eq!(stats.rows.len(), 1);
    let figure = |name: &str| {
        stats
            .columns
            .iter()
            .position(|column| column.name == name)
            .map(|index| stats.rows[0][index].display())
            .unwrap_or_else(|| panic!("no {name} in the statistics"))
    };
    // The two large figures come first.
    assert_eq!(stats.columns[0].name, "connections");
    assert_eq!(stats.columns[1].name, "active");
    assert!(figure("connections").parse::<u64>().unwrap() >= 1);
    assert_eq!(
        figure("active"),
        "1",
        "the slow statement, without the monitor's own"
    );
    assert!(figure("max_connections").parse::<u64>().unwrap() > 0);
    assert!(figure("tables").parse::<u64>().is_ok());
    assert_eq!(figure("database"), profile().database);
    assert!(figure("version").split('.').count() >= 3);
    assert!(figure("version").contains(", time zone "));
    assert!(figure("longest query (s)").parse::<f64>().unwrap() >= 1.0);
    for name in [
        "memory",
        "merges",
        "mutations",
        "parts",
        "rows",
        "data size",
        "uptime",
    ] {
        assert!(!figure(name).is_empty(), "{name}");
    }
    // Nothing that is missing is shown as NULL.
    assert!(stats.rows[0].iter().all(|cell| *cell != Cell::Null));
    println!(
        "{}",
        stats
            .columns
            .iter()
            .zip(&stats.rows[0])
            .map(|(column, cell)| format!("{}: {}", column.name, cell.display()))
            .collect::<Vec<_>>()
            .join(" · ")
    );
    db.close().await;
}

/// TLS as the three modes of a profile promise it, and the name a
/// certificate is checked against: always the host of the profile, also
/// when the connection is dialled somewhere else, as behind an SSH tunnel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a TLS endpoint in front of ClickHouse: BAZA_DB_TEST_TLS_PORT and BAZA_DB_TEST_CA_FILE"]
async fn certificates_are_verified_against_the_name_of_the_profile() {
    let (Some(port), Some(authority)) = (
        variable("BAZA_DB_TEST_TLS_PORT"),
        variable("BAZA_DB_TEST_CA_FILE"),
    ) else {
        println!("skipped: BAZA_DB_TEST_TLS_PORT and BAZA_DB_TEST_CA_FILE are not set");
        return;
    };
    let port: u16 = port.parse().unwrap();
    let mut base = profile();
    base.port = port;
    let works = |profile: ConnectionProfile, forwarded: Option<u16>| async move {
        let db = Database::connect(&profile, forwarded)
            .await
            .unwrap_or_else(|error| panic!("{} with {:?}: {error}", profile.host, profile.tls));
        assert_eq!(run(&db, "SELECT 40 + 2").await.rows, [vec![integer(42)]]);
        db.close().await;
    };
    let refused = |profile: ConnectionProfile, forwarded: Option<u16>| async move {
        match Database::connect(&profile, forwarded).await {
            Ok(_) => panic!("{} with {:?} was opened", profile.host, profile.tls),
            Err(error) => error.to_string(),
        }
    };

    // "Encryption required": encrypted, whoever answers.
    let mut required = base.clone();
    required.tls = TlsMode::Require;
    works(required.clone(), None).await;
    // The same with a statement that takes a while, a cut result and a change.
    let db = Database::connect(&required, None).await.unwrap();
    let cut = run(&db, "SELECT number FROM system.numbers").await;
    assert!(cut.truncated && cut.rows.len() == MAX_RESULT_ROWS);
    drop_tables(&db, &["baza_live_tls"]).await;
    let table = qualified(&live("baza_live_tls")).unwrap();
    run(
        &db,
        &format!(
            "CREATE TABLE {table} (id Int64, note String) ENGINE = MergeTree ORDER BY id; \
             INSERT INTO {table} VALUES (1, 'plain')"
        ),
    )
    .await;
    db.columns(&live("baza_live_tls")).await.unwrap();
    db.apply(&RowChange::UpdateCell {
        table: live("baza_live_tls"),
        column: "note".into(),
        old: text("plain"),
        new: text("encrypted"),
        key: key(1),
    })
    .await
    .unwrap();
    assert_eq!(
        run(&db, &format!("SELECT note FROM {table}")).await.rows,
        [vec![text("encrypted")]]
    );
    drop_tables(&db, &["baza_live_tls"]).await;
    db.close().await;

    // "Full verification" without a CA file trusts the bundled Mozilla
    // roots, which did not issue the certificate of a test.
    let mut verified = base.clone();
    verified.tls = TlsMode::VerifyFull;
    assert_eq!(
        refused(verified.clone(), None).await,
        "database_certificate_rejected"
    );
    // With the authority that issued it, the certificate is accepted for
    // the address it names.
    verified.ca_file = Some(authority.clone().into());
    works(verified.clone(), None).await;
    // Another authority did not issue it.
    if let Some(other) = variable("BAZA_DB_TEST_OTHER_CA_FILE") {
        let mut foreign = verified.clone();
        foreign.ca_file = Some(other.into());
        assert_eq!(
            refused(foreign, None).await,
            "database_certificate_rejected"
        );
    }
    // A CA file that holds no certificate is a setting to correct.
    let mut empty = verified.clone();
    empty.ca_file = Some(std::env::current_exe().unwrap());
    assert_eq!(refused(empty, None).await, "database_tls_error");

    // Dialled somewhere else, as behind an SSH tunnel: the name of the
    // profile is what the certificate has to be for.
    if let Some(name) = variable("BAZA_DB_TEST_TLS_NAME") {
        let mut named = verified.clone();
        named.host = name;
        // The profile names the real port of the server; the tunnel has
        // another one.
        named.port = 8443;
        works(named.clone(), Some(port)).await;
        let mut stranger = named.clone();
        stranger.host = "baza-live-another-name.test".into();
        assert_eq!(
            refused(stranger, Some(port)).await,
            "database_certificate_rejected"
        );
        // Without verification the name does not matter.
        let mut unverified = named.clone();
        unverified.host = "baza-live-another-name.test".into();
        unverified.tls = TlsMode::Require;
        unverified.ca_file = None;
        works(unverified, Some(port)).await;
    } else {
        println!("skipped: BAZA_DB_TEST_TLS_NAME is not set");
    }
    // A server addressed by its IP address: behind a tunnel the socket goes
    // to 127.0.0.1, and the certificate is still checked against the
    // address of the profile.
    if let Some(address) = variable("BAZA_DB_TEST_TLS_ADDRESS") {
        let mut addressed = verified.clone();
        addressed.host = address;
        addressed.port = 8443;
        works(addressed.clone(), Some(port)).await;
        let mut elsewhere = addressed.clone();
        elsewhere.host = "10.99.99.99".into();
        assert_eq!(
            refused(elsewhere, Some(port)).await,
            "database_certificate_rejected"
        );
    } else {
        println!("skipped: BAZA_DB_TEST_TLS_ADDRESS is not set");
    }
    // Without encryption the endpoint does not answer in HTTP.
    let mut plain = base.clone();
    plain.tls = TlsMode::Disable;
    assert!(Database::connect(&plain, None).await.is_err());
}
