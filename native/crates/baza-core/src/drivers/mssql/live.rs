//! What only SQL Server has, checked against a real server. Ignored by
//! default and described by the variables of the driver checklist
//! (`tests/driver_checklist.rs`): `BAZA_DB_TEST_PORT`, `BAZA_DB_TEST_USER`,
//! `BAZA_DB_TEST_PASSWORD`, `BAZA_DB_TEST_DATABASE`, optionally
//! `BAZA_DB_TEST_HOST` and `BAZA_DB_TEST_TLS`.
//!
//! ```text
//! cargo test -p baza-core --lib mssql::live -- --ignored --nocapture --test-threads 1
//! ```
//!
//! `certificates_are_verified_against_the_name_of_the_profile` also needs a
//! server whose certificate was issued by an authority of the test:
//! `BAZA_DB_TEST_CA_FILE` (that authority), `BAZA_DB_TEST_TLS_NAME` (a DNS
//! name in the certificate that does not have to resolve) and
//! `BAZA_DB_TEST_OTHER_CA_FILE` (any other authority). `BAZA_DB_TEST_HOST`
//! has to be a loopback address named in the certificate.
//!
//! The account has to be allowed to create tables in that database. The
//! tests of other logins also need the right to create a login
//! (`sysadmin` or `securityadmin`); they say so and pass without it. Only
//! objects whose names begin with `baza_live_` are created, and they are
//! dropped at the end.
use super::*;
use crate::builder::Filter;
use crate::database::Database;
use crate::model::ConnectionProfile;

fn variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn profile() -> ConnectionProfile {
    let required = |name: &str| variable(name).unwrap_or_else(|| panic!("{name} is required"));
    ConnectionProfile {
        name: "Real SQL Server".into(),
        engine: ENGINE,
        host: variable("BAZA_DB_TEST_HOST").unwrap_or_else(|| "127.0.0.1".into()),
        port: required("BAZA_DB_TEST_PORT").parse().unwrap(),
        database: required("BAZA_DB_TEST_DATABASE"),
        username: required("BAZA_DB_TEST_USER"),
        password: variable("BAZA_DB_TEST_PASSWORD").unwrap_or_default(),
        tls: match variable("BAZA_DB_TEST_TLS").as_deref() {
            Some("disable") => TlsMode::Disable,
            Some("verify_full") => TlsMode::VerifyFull,
            _ => TlsMode::Require,
        },
        ..ConnectionProfile::default()
    }
}

async fn open() -> Database {
    Database::connect(&profile(), None).await.unwrap()
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
        schema: "dbo".into(),
        name: name.into(),
        kind: "table".into(),
    }
}

async fn drop_objects(db: &Database) {
    for (kind, name) in [
        ("VIEW", "baza_live_view"),
        ("TABLE", "baza_live_types"),
        ("TABLE", "baza_live_notes"),
        ("TABLE", "baza_live_lines"),
        ("TABLE", "baza_live_heap"),
        ("TABLE", "baza_live_numbered"),
        ("TABLE", "baza_live_state"),
    ] {
        run(
            db,
            &format!("IF OBJECT_ID(N'dbo.{name}') IS NOT NULL DROP {kind} dbo.{name}"),
        )
        .await;
    }
}

fn text(value: &str) -> Cell {
    Cell::Text(value.into())
}
fn key(id: i64) -> Vec<(String, Cell)> {
    vec![("id".to_owned(), Cell::Integer(id.to_string()))]
}

async fn page(db: &Database, request: &PageRequest) -> QueryResult {
    db.read_page(request, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("{error}: {}", db.page_text(request).unwrap()))
}

/// The slow statements other connections are running.
async fn running(db: &Database, needle: &str) -> usize {
    let result = run(
        db,
        &format!(
            "SELECT COUNT(*) FROM sys.dm_exec_requests r CROSS APPLY sys.dm_exec_sql_text(r.sql_handle) t WHERE r.session_id <> @@SPID AND t.text LIKE '%{needle}%'"
        ),
    )
    .await;
    result.rows[0][0].display().parse().unwrap()
}

/// Wait until nothing with this text runs on the server any more.
async fn gone(db: &Database, needle: &str) -> bool {
    for _ in 0..20 {
        if running(db, needle).await == 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

const TYPES: &str = "CREATE TABLE dbo.baza_live_types (
    id INT NOT NULL PRIMARY KEY,
    c_bit BIT, c_tiny TINYINT, c_small SMALLINT, c_big BIGINT,
    c_real REAL, c_float FLOAT, c_money MONEY, c_smallmoney SMALLMONEY,
    c_decimal DECIMAL(38,10), c_numeric NUMERIC(10,2),
    c_date DATE, c_time TIME(7), c_time0 TIME(0), c_datetime DATETIME,
    c_small_dt SMALLDATETIME, c_datetime2 DATETIME2(7), c_datetime2_3 DATETIME2(3),
    c_offset DATETIMEOFFSET(7), c_guid UNIQUEIDENTIFIER,
    c_char CHAR(5), c_varchar VARCHAR(50), c_nchar NCHAR(5), c_nvarchar NVARCHAR(50),
    c_nmax NVARCHAR(MAX), c_binary BINARY(4), c_varbinary VARBINARY(MAX),
    c_xml XML, c_text TEXT, c_ntext NTEXT, c_image IMAGE, c_variant SQL_VARIANT,
    c_version ROWVERSION
);
INSERT INTO dbo.baza_live_types (id, c_bit, c_tiny, c_small, c_big, c_real, c_float, c_money,
    c_smallmoney, c_decimal, c_numeric, c_date, c_time, c_time0, c_datetime, c_small_dt,
    c_datetime2, c_datetime2_3, c_offset, c_guid, c_char, c_varchar, c_nchar, c_nvarchar, c_nmax,
    c_binary, c_varbinary, c_xml, c_text, c_ntext, c_image, c_variant)
VALUES (1, 1, 255, -5, 9007199254740993, 1.5, 123456.789, 1234.5, -0.0001,
    12345678901234567890.12, -0.5, '2026-10-12', '13:45:30.1234567', '13:45:30',
    '2026-10-12T13:45:30.123', '2026-10-12T13:45:00', '2026-10-12T13:45:30.1234567',
    '2026-10-12T13:45:30', '2026-10-12T13:45:30.1234567+02:00',
    '77929515-416D-4B18-9268-3883E48A25CD', 'ab', 'Zolc', N'żó', N'Zażółć gęślą jaźń 🦀',
    N'długi tekst', 0x01, 0x00FF10, N'<a x=\"1\">t</a>', 'stary tekst', N'stary tekst ż',
    0x0102, CAST(42 AS INT));
INSERT INTO dbo.baza_live_types (id) VALUES (2);
INSERT INTO dbo.baza_live_types (id, c_money, c_datetime, c_float, c_nvarchar)
VALUES (3, 922337203685477.5807, '2026-03-04T05:06:07', 0.1, N'[a-z] 100%');";

/// Every type as the cell the grid shows, and every editable one changed in
/// place: the text of a cell is a literal the server reads back as the same
/// value, so the conflict check of an edit finds its row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real SQL Server described by BAZA_DB_TEST_* variables"]
async fn types_are_shown_and_edited_in_place() {
    let db = open().await;
    drop_objects(&db).await;
    run(&db, TYPES).await;
    let tables = db.tables().await.unwrap();
    let table = tables
        .iter()
        .find(|table| table.name == "baza_live_types")
        .unwrap()
        .clone();
    assert_eq!(
        (table.schema.as_str(), table.kind.as_str()),
        ("dbo", "table")
    );
    let columns = db.columns(&table).await.unwrap();
    let described: Vec<(&str, &str)> = columns
        .iter()
        .map(|column| (column.name.as_str(), column.data_type.as_str()))
        .collect();
    assert_eq!(
        described,
        [
            ("id", "int"),
            ("c_bit", "bit"),
            ("c_tiny", "tinyint"),
            ("c_small", "smallint"),
            ("c_big", "bigint"),
            ("c_real", "real"),
            ("c_float", "float"),
            ("c_money", "money"),
            ("c_smallmoney", "smallmoney"),
            ("c_decimal", "decimal(38,10)"),
            ("c_numeric", "numeric(10,2)"),
            ("c_date", "date"),
            ("c_time", "time(7)"),
            ("c_time0", "time(0)"),
            ("c_datetime", "datetime"),
            ("c_small_dt", "smalldatetime"),
            ("c_datetime2", "datetime2(7)"),
            ("c_datetime2_3", "datetime2(3)"),
            ("c_offset", "datetimeoffset(7)"),
            ("c_guid", "uniqueidentifier"),
            ("c_char", "char(5)"),
            ("c_varchar", "varchar(50)"),
            ("c_nchar", "nchar(5)"),
            ("c_nvarchar", "nvarchar(50)"),
            ("c_nmax", "nvarchar(max)"),
            ("c_binary", "binary(4)"),
            ("c_varbinary", "varbinary(max)"),
            ("c_xml", "xml"),
            ("c_text", "text"),
            ("c_ntext", "ntext"),
            ("c_image", "image"),
            ("c_variant", "sql_variant"),
            ("c_version", "timestamp"),
        ]
    );
    assert!(columns[0].primary_key && !columns[0].nullable);
    assert!(columns[1..].iter().all(|column| !column.primary_key));
    // A column the server fills in may be left out of a new row.
    assert_eq!(columns[32].default.as_deref(), Some("rowversion"));

    let first = PageRequest::first(table.clone());
    assert_eq!(
        db.page_text(&first).unwrap(),
        "SELECT *\nFROM [dbo].[baza_live_types]\nORDER BY [id]\nOFFSET 0 ROWS FETCH NEXT 500 ROWS ONLY"
    );
    let read = page(&db, &first).await;
    let types: Vec<&str> = read
        .columns
        .iter()
        .map(|column| column.data_type.as_str())
        .collect();
    assert_eq!(
        types,
        [
            "INT",
            "BIT",
            "TINYINT",
            "SMALLINT",
            "BIGINT",
            "REAL",
            "FLOAT",
            "MONEY",
            "MONEY",
            "DECIMAL",
            "NUMERIC",
            "DATE",
            "TIME",
            "TIME",
            "DATETIME",
            "DATETIME",
            "DATETIME2",
            "DATETIME2",
            "DATETIMEOFFSET",
            "UNIQUEIDENTIFIER",
            "CHAR",
            "VARCHAR",
            "NCHAR",
            "NVARCHAR",
            "NVARCHAR",
            "BINARY",
            "VARBINARY",
            "XML",
            "TEXT",
            "NTEXT",
            "IMAGE",
            "SQL_VARIANT",
            "BINARY"
        ]
    );
    assert_eq!(read.rows.len(), 3);
    let expected = [
        Cell::Integer("1".into()),
        Cell::Bool(true),
        Cell::Integer("255".into()),
        Cell::Integer("-5".into()),
        Cell::Integer("9007199254740993".into()),
        Cell::Float("1.5".into()),
        Cell::Float("123456.789".into()),
        Cell::Decimal("1234.5000".into()),
        Cell::Decimal("-0.0001".into()),
        Cell::Decimal("12345678901234567890.1200000000".into()),
        Cell::Decimal("-0.50".into()),
        text("2026-10-12"),
        text("13:45:30.1234567"),
        text("13:45:30"),
        text("2026-10-12T13:45:30.123"),
        text("2026-10-12T13:45:00"),
        text("2026-10-12T13:45:30.1234567"),
        text("2026-10-12T13:45:30"),
        text("2026-10-12T13:45:30.1234567+02:00"),
        text("77929515-416D-4B18-9268-3883E48A25CD"),
        text("ab   "),
        text("Zolc"),
        text("żó   "),
        text("Zażółć gęślą jaźń 🦀"),
        text("długi tekst"),
        Cell::Bytes(vec![1, 0, 0, 0]),
        Cell::Bytes(vec![0, 0xff, 0x10]),
        text("<a x=\"1\">t</a>"),
        text("stary tekst"),
        text("stary tekst ż"),
        Cell::Bytes(vec![1, 2]),
        Cell::Integer("42".into()),
    ];
    for (index, cell) in expected.iter().enumerate() {
        assert_eq!(&read.rows[0][index], cell, "{}", read.columns[index].name);
    }
    assert!(matches!(&read.rows[0][32], Cell::Bytes(stamp) if stamp.len() == 8));
    assert!(read.rows[1][1..32].iter().all(|cell| *cell == Cell::Null));
    // An amount a floating point number cannot hold is not shown rounded.
    assert_eq!(read.rows[2][7], Cell::Unsupported("<MONEY>".into()));
    assert_eq!(read.rows[2][14], text("2026-03-04T05:06:07"));

    // One cell after another, with the value on screen as the old value.
    let edits = [
        ("c_bit", Cell::Bool(false)),
        ("c_tiny", Cell::Integer("7".into())),
        ("c_small", Cell::Integer("-32768".into())),
        ("c_big", Cell::Integer("-9223372036854775808".into())),
        ("c_real", Cell::Float("0.1".into())),
        ("c_float", Cell::Float("0.30000000000000004".into())),
        ("c_money", Cell::Decimal("-99.9900".into())),
        ("c_smallmoney", Cell::Decimal("214748.3647".into())),
        (
            "c_decimal",
            Cell::Decimal("-9999999999999999999999999999.9999999999".into()),
        ),
        ("c_numeric", Cell::Decimal("318.00".into())),
        ("c_date", text("1999-12-31")),
        ("c_time", text("23:59:59.9999999")),
        ("c_time0", text("00:00:01")),
        ("c_datetime", text("2001-02-03T04:05:06.007")),
        ("c_small_dt", text("2001-02-03T04:05:00")),
        ("c_datetime2", text("2001-02-03T04:05:06.0000001")),
        ("c_datetime2_3", text("2001-02-03T04:05:06.789")),
        ("c_offset", text("2001-02-03T04:05:06.0000001-05:30")),
        ("c_guid", text("00000000-0000-0000-0000-00000000000A")),
        ("c_char", text("x'y  ")),
        ("c_varchar", text("it's \\ back")),
        ("c_nchar", text("ąę   ")),
        ("c_nvarchar", text("Kopia D:\\zrzut\\ 'x' [1]")),
        ("c_nmax", text("wiele\nlinii\\\nz ukośnikiem na końcu")),
        ("c_xml", text("<b>nowy</b>")),
        ("c_text", text("nowy tekst")),
        ("c_ntext", text("nowy tekst ż")),
        ("c_variant", Cell::Integer("43".into())),
    ];
    for (column, new) in &edits {
        let index = read
            .columns
            .iter()
            .position(|found| found.name == *column)
            .unwrap();
        let change = RowChange::UpdateCell {
            table: table.clone(),
            column: (*column).into(),
            old: read.rows[0][index].clone(),
            new: new.clone(),
            key: key(1),
        };
        db.apply(&change).await.unwrap_or_else(|error| {
            panic!("{column}: {error}: {}", db.change_text(&change).unwrap())
        });
        // The old value is gone: the same edit again is a conflict.
        assert_eq!(
            db.apply(&change).await.unwrap_err().to_string(),
            "row_changed_reload_required",
            "{column}"
        );
    }
    let after = page(&db, &first).await;
    for (column, new) in &edits {
        let index = after
            .columns
            .iter()
            .position(|found| found.name == *column)
            .unwrap();
        assert_eq!(&after.rows[0][index], new, "{column}");
    }
    // A value back to NULL and from NULL, and nothing else moved.
    db.apply(&RowChange::UpdateCell {
        table: table.clone(),
        column: "c_datetime".into(),
        old: Cell::Null,
        new: text("2026-01-02T03:04:05"),
        key: key(2),
    })
    .await
    .unwrap();
    db.apply(&RowChange::UpdateCell {
        table: table.clone(),
        column: "c_datetime".into(),
        old: text("2026-01-02T03:04:05"),
        new: Cell::Null,
        key: key(2),
    })
    .await
    .unwrap();
    let untouched = page(&db, &first).await;
    assert!(
        untouched.rows[1][1..32]
            .iter()
            .all(|cell| *cell == Cell::Null)
    );
    assert_eq!(untouched.rows[2][14], text("2026-03-04T05:06:07"));

    // The filter of "Dane tabeli" searches the text the grid shows.
    run(
        &db,
        "UPDATE dbo.baza_live_types SET c_bit = 1, c_money = 1234.5, c_float = 123456.789, c_guid = '77929515-416D-4B18-9268-3883E48A25CD', c_datetime = '2026-10-12T13:45:30.123', c_varbinary = 0x00FF10, c_xml = N'<a x=\"1\">t</a>', c_nvarchar = N'Zażółć gęślą' WHERE id = 1",
    )
    .await;
    for (column, value, found) in [
        ("c_datetime", "2026-10-12T13", vec![1]),
        ("c_datetime", "2026-", vec![1, 3]),
        ("c_datetime", "t05:06", vec![3]),
        ("c_datetime2_3", "06.789", vec![1]),
        ("c_date", "1999-12", vec![1]),
        ("c_offset", "-05:30", vec![1]),
        ("c_money", "1234.5", vec![1]),
        ("c_money", "5477.5807", vec![3]),
        ("c_float", "123456.789", vec![1]),
        ("c_float", "0.1", vec![3]),
        ("c_real", "0.1", vec![1]),
        ("c_bit", "tru", vec![1]),
        ("c_bit", "false", vec![]),
        ("c_guid", "416d-4b18", vec![1]),
        ("c_varbinary", "00ff", vec![1]),
        ("c_version", "0", vec![1, 2, 3]),
        ("c_xml", "X=\"1\"", vec![1]),
        ("c_text", "NOWY", vec![1]),
        ("c_ntext", "tekst ż", vec![1]),
        ("c_image", "0102", vec![1]),
        ("c_nvarchar", "GĘŚLĄ", vec![1]),
        ("c_nvarchar", "[a-z]", vec![3]),
        ("c_nvarchar", "100%", vec![3]),
        ("c_nvarchar", "a-z] 1", vec![3]),
        ("c_decimal", "9999.99", vec![1]),
        ("c_big", "9223372", vec![1]),
        ("id", "3", vec![3]),
        ("c_variant", "43", vec![1]),
    ] {
        let request = PageRequest {
            filters: vec![Filter {
                column: column.into(),
                op: FilterOp::Contains,
                value: text(value),
            }],
            ..PageRequest::first(table.clone())
        };
        let rows = page(&db, &request).await;
        let ids: Vec<i64> = rows
            .rows
            .iter()
            .map(|row| row[0].display().parse().unwrap())
            .collect();
        assert_eq!(
            ids,
            found,
            "{column} contains {value:?}: {}",
            db.page_text(&request).unwrap()
        );
    }
    drop_objects(&db).await;
    db.close().await;
    println!(
        "{} types shown, {} edited in place, 28 filters",
        expected.len() + 1,
        edits.len()
    );
}

/// A backslash at the end of a line is a line continuation inside a T-SQL
/// literal: the server drops it together with the line break. Whatever
/// writes a value keeps both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real SQL Server described by BAZA_DB_TEST_* variables"]
async fn a_backslash_before_a_line_break_survives_every_way_in() {
    let db = open().await;
    drop_objects(&db).await;
    run(
        &db,
        "CREATE TABLE dbo.baza_live_notes (id INT NOT NULL PRIMARY KEY, body NVARCHAR(MAX), short VARCHAR(40))",
    )
    .await;
    let table = live("baza_live_notes");
    // The server really does it: this is what the plain literal loses.
    let lost = run(&db, "SELECT N'a\\\nb', N'a\\\r\nb'").await;
    assert_eq!(lost.rows[0], [text("ab"), text("ab")]);
    let tricky = [
        "cd /tmp \\\nls -la",
        "C:\\dane\\\r\nD:\\kopie\\",
        "\\\n",
        "\\\r",
        "koniec \\",
        "it's \\\n'quoted' \\\\\n",
    ];
    let names = ["id".to_owned(), "body".to_owned(), "short".to_owned()];
    let rows: Vec<Vec<Cell>> = tricky
        .iter()
        .enumerate()
        .map(|(index, body)| {
            vec![
                Cell::Integer((index + 1).to_string()),
                text(body),
                text(body),
            ]
        })
        .collect();
    let imported = db
        .import_rows(&table, &names, &rows, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(imported.affected, tricky.len() as u64);
    let read = page(&db, &PageRequest::first(table.clone())).await;
    for (row, body) in read.rows.iter().zip(tricky) {
        assert_eq!(row[1], text(body));
        assert_eq!(row[2], text(body));
    }
    // A cell edit finds the row by such a value and writes another one.
    db.apply(&RowChange::UpdateCell {
        table: table.clone(),
        column: "body".into(),
        old: text(tricky[0]),
        new: text("echo \\\r\necho"),
        key: key(1),
    })
    .await
    .unwrap();
    db.apply(&RowChange::InsertRow {
        table: table.clone(),
        values: vec![
            ("id".into(), Cell::Integer("50".into())),
            ("body".into(), text("nowy \\\nwiersz")),
        ],
    })
    .await
    .unwrap();
    // A value longer than a short literal is not cut where it is joined.
    let long = format!("{}\\\n{}", "ż".repeat(5000), "ó".repeat(5000));
    db.apply(&RowChange::InsertRow {
        table: table.clone(),
        values: vec![
            ("id".into(), Cell::Integer("51".into())),
            ("body".into(), text(&long)),
        ],
    })
    .await
    .unwrap();
    db.apply(&RowChange::DeleteRow {
        table: table.clone(),
        key: vec![("short".to_owned(), text(tricky[1]))],
    })
    .await
    .unwrap();
    let read = page(&db, &PageRequest::first(table.clone())).await;
    let bodies: Vec<(String, Cell)> = read
        .rows
        .iter()
        .map(|row| (row[0].display(), row[1].clone()))
        .collect();
    assert_eq!(bodies.len(), tricky.len() + 2 - 1);
    assert_eq!(bodies[0], ("1".to_owned(), text("echo \\\r\necho")));
    assert!(bodies.iter().all(|(id, _)| id != "2"));
    assert_eq!(bodies[bodies.len() - 2].1, text("nowy \\\nwiersz"));
    assert_eq!(bodies[bodies.len() - 1].1, text(&long));
    drop_objects(&db).await;
    db.close().await;
    println!(
        "{} values with a backslash before a line break kept",
        tricky.len() + 3
    );
}

/// Pages are pages of one list: every row once, in the order of the key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real SQL Server described by BAZA_DB_TEST_* variables"]
async fn pages_follow_the_primary_key() {
    let db = open().await;
    drop_objects(&db).await;
    run(
        &db,
        "CREATE TABLE dbo.baza_live_lines ([order id] INT NOT NULL, [line]]no] INT NOT NULL, status NVARCHAR(10) NOT NULL, PRIMARY KEY ([line]]no], [order id]));
         WITH n AS (SELECT 1 AS i UNION ALL SELECT i + 1 FROM n WHERE i < 60)
         INSERT INTO dbo.baza_live_lines SELECT i % 6, i, CASE WHEN i % 3 = 0 THEN N'b' ELSE N'a' END FROM n;
         CREATE TABLE dbo.baza_live_heap (body XML, note NVARCHAR(20));
         INSERT INTO dbo.baza_live_heap VALUES (N'<a/>', N'one'), (N'<b/>', N'two');",
    )
    .await;
    run(
        &db,
        "CREATE VIEW dbo.baza_live_view AS SELECT status, COUNT(*) AS lines FROM dbo.baza_live_lines GROUP BY status",
    )
    .await;
    let tables = db.tables().await.unwrap();
    let find = |name: &str| {
        tables
            .iter()
            .find(|table| table.name == name)
            .unwrap()
            .clone()
    };
    let lines = find("baza_live_lines");
    let mut request = PageRequest::first(lines.clone());
    request.limit = 7;
    // The key in the order of the key, not of the columns.
    assert_eq!(
        db.page_text(&request).unwrap(),
        "SELECT *\nFROM [dbo].[baza_live_lines]\nORDER BY [line]]no], [order id]\nOFFSET 0 ROWS FETCH NEXT 7 ROWS ONLY"
    );
    let mut seen = Vec::new();
    for offset in (0..63).step_by(7) {
        request.offset = offset;
        let rows = page(&db, &request).await;
        seen.extend(
            rows.rows
                .iter()
                .map(|row| row[1].display().parse::<i64>().unwrap()),
        );
    }
    assert_eq!(seen, (1..=60).collect::<Vec<_>>());
    // Ordered by a column with two values: the key breaks the ties, so the
    // pages still hold every row once.
    request.order_by = Some(("status".into(), true));
    assert!(
        db.page_text(&request)
            .unwrap()
            .contains("ORDER BY [status] DESC, [line]]no], [order id]\n")
    );
    let mut seen = Vec::new();
    for offset in (0..63).step_by(7) {
        request.offset = offset;
        let rows = page(&db, &request).await;
        seen.extend(
            rows.rows
                .iter()
                .map(|row| row[1].display().parse::<i64>().unwrap()),
        );
    }
    let threes: Vec<i64> = (1..=60).filter(|i| i % 3 == 0).collect();
    let others: Vec<i64> = (1..=60).filter(|i| i % 3 != 0).collect();
    assert_eq!(seen, [threes, others].concat());
    // A table without a key, whose first column cannot even be sorted, and
    // a view: read in the order of the scan.
    let heap = find("baza_live_heap");
    let text_of = db.page_text(&PageRequest::first(heap.clone())).unwrap();
    assert!(text_of.contains("ORDER BY (SELECT NULL)"), "{text_of}");
    assert_eq!(
        page(&db, &PageRequest::first(heap.clone()))
            .await
            .rows
            .len(),
        2
    );
    let view = find("baza_live_view");
    assert_eq!(view.kind, "view");
    let counted = page(&db, &PageRequest::first(view.clone())).await;
    assert_eq!(counted.rows.len(), 2);
    assert_eq!(
        counted
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["status", "lines"]
    );
    // A filter on a view: its columns are described when they are needed.
    let filtered = PageRequest {
        filters: vec![Filter {
            column: "lines".into(),
            op: FilterOp::Contains,
            value: text("40"),
        }],
        ..PageRequest::first(view)
    };
    assert_eq!(page(&db, &filtered).await.rows[0][0], text("a"));
    // A page that is empty still names its columns.
    let nothing = PageRequest {
        filters: vec![Filter {
            column: "status".into(),
            op: FilterOp::Equal,
            value: text("no such status"),
        }],
        ..PageRequest::first(lines)
    };
    let empty = page(&db, &nothing).await;
    assert!(empty.rows.is_empty());
    assert_eq!(empty.columns.len(), 3);
    drop_objects(&db).await;
    db.close().await;
    println!("60 rows read once in pages of 7, in key order and with ties broken");
}

/// What a script leaves on its connection never reaches the next statement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real SQL Server described by BAZA_DB_TEST_* variables"]
async fn scripts_run_in_batches_and_leave_the_pool_clean() {
    let db = open().await;
    drop_objects(&db).await;
    let home = run(&db, "SELECT DB_NAME()").await.rows[0][0].display();
    run(
        &db,
        "CREATE TABLE dbo.baza_live_state (id INT NOT NULL PRIMARY KEY, n INT NOT NULL);
         INSERT INTO dbo.baza_live_state VALUES (1, 0), (2, 0), (3, 0)",
    )
    .await;
    // The pool holds four connections: asked six times, each of them
    // answers at least once.
    let clean = |db: Database, home: String| async move {
        for _ in 0..6 {
            let state = run(
                &db,
                "SELECT DB_NAME(), @@TRANCOUNT, @@LANGUAGE, (SELECT COUNT(*) FROM dbo.baza_live_state), OBJECT_ID('tempdb..#baza_live')",
            )
            .await;
            assert_eq!(state.rows[0][0].display(), home);
            assert_eq!(state.rows[0][1].display(), "0");
            assert_eq!(state.rows[0][2].display(), "us_english");
            assert_eq!(state.rows[0][3].display(), "3");
            assert_eq!(state.rows[0][4], Cell::Null);
        }
    };

    // Batches of a script share a connection: what one sets up, the next
    // one finds. The last result set is the result.
    let script = run(
        &db,
        "CREATE TABLE #baza_live (a INT)\nGO\nINSERT INTO #baza_live VALUES (1), (2)\nGO 3\nSET ROWCOUNT 4\nGO\nSELECT a FROM #baza_live\ngo\n",
    )
    .await;
    assert_eq!(script.rows.len(), 4);
    clean(db.clone(), home.clone()).await;
    let moved = run(&db, "USE master\nGO\nSELECT DB_NAME()").await;
    assert_eq!(moved.rows[0][0], text("master"));
    clean(db.clone(), home.clone()).await;
    // Without GO the statement runs in a scope of its own, and the
    // connection it ran on is not used again where the scope does not help.
    assert_eq!(
        run(&db, "USE master; SELECT DB_NAME()").await.rows[0][0],
        text("master")
    );
    clean(db.clone(), home.clone()).await;
    let limited = run(
        &db,
        "SET ROWCOUNT 1; SET LANGUAGE polski; SELECT id INTO #baza_live FROM dbo.baza_live_state; SELECT @@LANGUAGE",
    )
    .await;
    assert_eq!(limited.rows[0][0], text("polski"));
    clean(db.clone(), home.clone()).await;
    let two = run(&db, "SELECT 1 AS a\nGO\nSELECT 2 AS b, 3 AS c").await;
    assert_eq!(two.columns.len(), 2);
    assert_eq!(two.rows[0][0], Cell::Integer("2".into()));
    assert!(run(&db, "PRINT 'nothing to show'").await.columns.is_empty());
    assert!(run(&db, "-- only a comment\nGO").await.columns.is_empty());

    // Rows changed, as far as the server tells them apart.
    assert_eq!(
        run(&db, "UPDATE dbo.baza_live_state SET n = 1")
            .await
            .affected,
        3
    );
    assert_eq!(
        run(&db, "UPDATE dbo.baza_live_state SET n = 2 WHERE id = 1; DELETE FROM dbo.baza_live_state WHERE id = 99")
            .await
            .affected,
        1
    );
    assert_eq!(
        run(
            &db,
            "UPDATE dbo.baza_live_state SET n = n + 1 WHERE id < 3\nGO 3"
        )
        .await
        .affected,
        6
    );
    let selected = run(
        &db,
        "UPDATE dbo.baza_live_state SET n = 0; SELECT * FROM dbo.baza_live_state",
    )
    .await;
    assert_eq!((selected.rows.len(), selected.affected), (3, 3));
    // An assignment reports a row as well: nothing is claimed then.
    let assigned = run(
        &db,
        "DECLARE @n INT = 5; UPDATE dbo.baza_live_state SET n = @n",
    )
    .await;
    assert_eq!(assigned.affected, 0);
    assert_eq!(
        run(&db, "SELECT SUM(n) FROM dbo.baza_live_state")
            .await
            .rows[0][0]
            .display(),
        "15"
    );
    let called = run(&db, "EXEC sp_executesql N'SELECT 7 AS seven'").await;
    assert_eq!(called.rows[0][0], Cell::Integer("7".into()));
    clean(db.clone(), home.clone()).await;

    // A transaction is whole inside one run …
    let committed = run(
        &db,
        "BEGIN TRANSACTION; UPDATE dbo.baza_live_state SET n = 1; COMMIT TRANSACTION",
    )
    .await;
    assert_eq!(committed.affected, 3);
    run(
        &db,
        "BEGIN TRANSACTION\nGO\nUPDATE dbo.baza_live_state SET n = 2\nGO\nCOMMIT\nGO",
    )
    .await;
    assert_eq!(
        run(&db, "SELECT SUM(n) FROM dbo.baza_live_state")
            .await
            .rows[0][0]
            .display(),
        "6"
    );
    // … and one that is left open is rolled back and reported, never left
    // on a connection the next statement might get.
    for open in [
        "BEGIN TRANSACTION; UPDATE dbo.baza_live_state SET n = 9",
        "BEGIN TRANSACTION\nGO\nUPDATE dbo.baza_live_state SET n = 9\nGO",
        "SET IMPLICIT_TRANSACTIONS ON; UPDATE dbo.baza_live_state SET n = 9",
        "EXEC sp_executesql N'BEGIN TRANSACTION; UPDATE dbo.baza_live_state SET n = 9'",
    ] {
        assert_eq!(
            fails(&db, open).await,
            "mssql_transaction_left_open",
            "{open}"
        );
        clean(db.clone(), home.clone()).await;
        assert_eq!(
            run(&db, "SELECT SUM(n) FROM dbo.baza_live_state")
                .await
                .rows[0][0]
                .display(),
            "6",
            "{open}"
        );
    }
    // The error of a statement is the number SQL Server gives it, and the
    // connection goes on.
    assert_eq!(
        fails(&db, "SELECT * FROM dbo.baza_live_nothing").await,
        "database_error [208]"
    );
    assert_eq!(
        fails(&db, "SELECT 1; SELEC 2").await,
        "database_error [102]"
    );
    assert_eq!(fails(&db, "SELECT 1/0").await, "database_error [8134]");
    assert_eq!(
        fails(&db, "INSERT INTO dbo.baza_live_state VALUES (1, 1)").await,
        "database_error [2627]"
    );
    assert_eq!(
        fails(
            &db,
            "BEGIN TRANSACTION\nGO\nSELECT * FROM dbo.baza_live_nothing\nGO\nCOMMIT"
        )
        .await,
        "database_error [208]"
    );
    clean(db.clone(), home.clone()).await;

    // One row or nothing: the guarded change of the grid.
    let table = live("baza_live_state");
    assert_eq!(
        db.mutate_one("UPDATE dbo.baza_live_state SET n = 7")
            .await
            .unwrap_err()
            .to_string(),
        "row_changed_reload_required"
    );
    assert_eq!(
        db.mutate_one("UPDATE dbo.baza_live_state SET n = 7 WHERE id = 99")
            .await
            .unwrap_err()
            .to_string(),
        "row_changed_reload_required"
    );
    assert_eq!(
        db.mutate_one("UPDATE dbo.baza_live_nothing SET n = 7")
            .await
            .unwrap_err()
            .to_string(),
        "database_error [208]"
    );
    assert_eq!(
        db.mutate_one("UPDATE dbo.baza_live_state SET n = N'seven' WHERE id = 1")
            .await
            .unwrap_err()
            .to_string(),
        "database_error [245]"
    );
    assert_eq!(
        db.apply(&RowChange::InsertRow {
            table: table.clone(),
            values: vec![
                ("id".into(), Cell::Integer("1".into())),
                ("n".into(), Cell::Integer("1".into()))
            ]
        })
        .await
        .unwrap_err()
        .to_string(),
        "database_error [2627]"
    );
    db.mutate_one("UPDATE dbo.baza_live_state SET n = 7 WHERE id = 1 -- the first")
        .await
        .unwrap();
    clean(db.clone(), home.clone()).await;
    assert_eq!(
        run(&db, "SELECT SUM(n) FROM dbo.baza_live_state")
            .await
            .rows[0][0]
            .display(),
        "11"
    );
    drop_objects(&db).await;
    db.close().await;
    println!("batches, session state, row counts, open transactions and guarded changes behave");
}

/// A result stops at the limit, a statement stops when asked, and in both
/// cases the server stops working on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real SQL Server described by BAZA_DB_TEST_* variables"]
async fn long_results_and_long_statements_are_stopped_on_the_server() {
    let db = open().await;
    let endless = "SELECT a.object_id, REPLICATE(N'baza_live_endless', 20) AS filler FROM sys.all_columns a CROSS JOIN sys.all_columns b CROSS JOIN sys.all_columns c";
    let started = Instant::now();
    let cut = run(&db, endless).await;
    let cut_in = started.elapsed();
    assert!(cut.truncated);
    assert_eq!(cut.rows.len(), MAX_RESULT_ROWS);
    assert!(
        gone(&db, "baza_live_endless").await,
        "the cut statement still runs"
    );
    // Stopped while the rows are streaming.
    let token = CancellationToken::new();
    let (worker, cancel) = (db.clone(), token.clone());
    // A count over a product of system views: minutes of work without a row.
    let slow = "SELECT COUNT_BIG(*) FROM sys.all_columns a CROSS JOIN sys.all_columns b CROSS JOIN sys.all_columns c WHERE a.name + b.name + c.name = N'baza_live_never'";
    let task = tokio::spawn(async move { worker.query(slow, cancel).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(running(&db, "live_never").await, 1);
    let asked = Instant::now();
    token.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stopped.unwrap_err().to_string(), "query_cancelled");
    let cancelled_in = asked.elapsed();
    assert!(
        gone(&db, "live_never").await,
        "the cancelled statement still runs"
    );
    // A token that is already cancelled never reaches the server.
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        db.query("SELECT 1", cancelled)
            .await
            .unwrap_err()
            .to_string(),
        "query_cancelled"
    );
    assert_eq!(
        run(&db, "SELECT 5").await.rows[0][0],
        Cell::Integer("5".into())
    );
    db.close().await;
    println!(
        "a result cut at {MAX_RESULT_ROWS} rows in {} ms, a running statement cancelled in {} ms",
        cut_in.as_millis(),
        cancelled_in.as_millis()
    );
}

/// An import is all or nothing, in requests of a few hundred rows, and may
/// carry the values of a column that numbers itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real SQL Server described by BAZA_DB_TEST_* variables"]
async fn an_import_is_whole_and_may_carry_identity_values() {
    let db = open().await;
    drop_objects(&db).await;
    run(
        &db,
        "CREATE TABLE dbo.baza_live_numbered (id INT IDENTITY(1,1) NOT NULL PRIMARY KEY, label NVARCHAR(40) NOT NULL, total AS (LEN(label)), added DATETIME2 NOT NULL DEFAULT SYSUTCDATETIME())",
    )
    .await;
    let table = live("baza_live_numbered");
    let columns = db.columns(&table).await.unwrap();
    assert_eq!(columns[0].default.as_deref(), Some("IDENTITY"));
    assert_eq!(columns[2].default.as_deref(), Some("AS (len([label]))"));
    assert_eq!(columns[3].default.as_deref(), Some("(sysutcdatetime())"));
    let both = ["id".to_owned(), "label".to_owned()];
    let rows = |ids: std::ops::RangeInclusive<i64>| {
        ids.map(|id| vec![Cell::Integer(id.to_string()), text(&format!("wiersz {id}"))])
            .collect::<Vec<_>>()
    };
    let started = Instant::now();
    let imported = db
        .import_rows(&table, &both, &rows(1001..=1650), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(imported.affected, 650);
    let took = started.elapsed();
    // The option that allowed it ended with the import.
    assert_eq!(
        fails(
            &db,
            "INSERT INTO dbo.baza_live_numbered (id, label) VALUES (5, N'x')"
        )
        .await,
        "database_error [544]"
    );
    // A duplicate in the third request of 200 rows: nothing of it stays.
    let mut clash = rows(2001..=2450);
    clash.push(vec![Cell::Integer("1001".into()), text("duplicate")]);
    assert_eq!(
        db.import_rows(&table, &both, &clash, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string(),
        "database_error [2627]"
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        db.import_rows(&table, &both, &rows(3001..=3300), cancelled)
            .await
            .unwrap_err()
            .to_string(),
        "query_cancelled"
    );
    // Without the numbered column the server numbers the rows itself.
    db.import_rows(
        &table,
        &["label".to_owned()],
        &[vec![text("bez numeru")], vec![text("drugi bez numeru")]],
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let counted = run(
        &db,
        "SELECT COUNT(*), MIN(id), (SELECT COUNT(*) FROM dbo.baza_live_numbered WHERE id BETWEEN 2001 AND 2450 OR id BETWEEN 3001 AND 3300), (SELECT COUNT(*) FROM dbo.baza_live_numbered WHERE id > 1650), @@TRANCOUNT FROM dbo.baza_live_numbered",
    )
    .await;
    assert_eq!(
        counted.rows[0]
            .iter()
            .map(Cell::display)
            .collect::<Vec<_>>(),
        ["652", "1001", "0", "2", "0"]
    );
    // A new row of the grid leaves the columns with a default out.
    db.apply(&RowChange::InsertRow {
        table: table.clone(),
        values: vec![("label".into(), text("z siatki"))],
    })
    .await
    .unwrap();
    drop_objects(&db).await;
    db.close().await;
    println!(
        "650 rows imported in {} ms; a failed and a cancelled import left nothing",
        took.as_millis()
    );
}

/// What a person can do about a refused connection has a code of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real SQL Server described by BAZA_DB_TEST_* variables"]
async fn refused_connections_say_why() {
    let refused = |profile: ConnectionProfile| async move {
        match Database::connect(&profile, None).await {
            Ok(db) => {
                db.close().await;
                "connected".to_owned()
            }
            Err(error) => error.to_string(),
        }
    };
    let good = profile();
    let mut wrong = good.clone();
    wrong.password = "not-the-password".into();
    assert_eq!(refused(wrong).await, "database_login_rejected");
    let mut missing = good.clone();
    missing.database = "baza_live_no_such_database".into();
    assert_eq!(refused(missing).await, "database_not_found");
    let mut nameless = good.clone();
    nameless.username.clear();
    assert_eq!(refused(nameless).await, "mssql_login_required");
    let mut closed = good.clone();
    closed.port = 1;
    assert_eq!(refused(closed).await, "database_network_error");
    // Without a database name the login lands in its default database.
    let mut default = good.clone();
    default.database.clear();
    let db = Database::connect(&default, None).await.unwrap();
    assert!(
        !run(&db, "SELECT DB_NAME()").await.rows[0][0]
            .display()
            .is_empty()
    );
    db.close().await;
    // Every mode of the SSL/TLS tab connects or says something about TLS.
    let mut modes = Vec::new();
    for mode in [TlsMode::Disable, TlsMode::Require, TlsMode::VerifyFull] {
        let mut profile = good.clone();
        profile.tls = mode;
        let outcome = match Database::connect(&profile, None).await {
            Ok(db) => {
                // Whether the session is encrypted, as the server sees it.
                let seen = run(
                    &db,
                    "SELECT encrypt_option FROM sys.dm_exec_connections WHERE session_id = @@SPID",
                )
                .await
                .rows[0][0]
                    .display();
                db.close().await;
                format!("encrypted {seen}")
            }
            Err(error) => error.to_string(),
        };
        modes.push(format!("{mode:?}: {outcome}"));
    }
    assert_eq!(modes[0], "Disable: encrypted FALSE", "{modes:?}");
    assert_eq!(modes[1], "Require: encrypted TRUE", "{modes:?}");
    assert!(
        modes[2] == "VerifyFull: encrypted TRUE"
            || modes[2] == "VerifyFull: database_certificate_rejected",
        "{modes:?}"
    );
    // TDS 8.0 exists from SQL Server 2022 on: an older server is named as
    // such, after the time a handshake may take.
    let mut strict = good.clone();
    strict.set_option("strict_encryption", "true");
    let outcome = refused(strict).await;
    assert!(
        [
            "connected",
            "mssql_strict_unsupported",
            "database_certificate_rejected"
        ]
        .contains(&outcome.as_str()),
        "{outcome}"
    );
    println!("{}; strict: {outcome}", modes.join("; "));
}

/// Full verification: the chain up to the CA file of the profile, and the
/// name of the profile, wherever the socket leads. The loopback end of an
/// SSH tunnel is stood in for by the port of the server itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real SQL Server with a certificate of a test authority"]
async fn certificates_are_verified_against_the_name_of_the_profile() {
    let (Some(authority), Some(name), Some(other)) = (
        variable("BAZA_DB_TEST_CA_FILE"),
        variable("BAZA_DB_TEST_TLS_NAME"),
        variable("BAZA_DB_TEST_OTHER_CA_FILE"),
    ) else {
        println!(
            "skipped: BAZA_DB_TEST_CA_FILE, BAZA_DB_TEST_TLS_NAME and BAZA_DB_TEST_OTHER_CA_FILE are not set"
        );
        return;
    };
    let port = profile().port;
    // What the server says about the session, or why there is none.
    let outcome = |profile: ConnectionProfile, forwarded: Option<u16>| async move {
        match Database::connect(&profile, forwarded).await {
            Ok(db) => {
                let seen = run(
                    &db,
                    "SELECT encrypt_option FROM sys.dm_exec_connections WHERE session_id = @@SPID",
                )
                .await
                .rows[0][0]
                    .display();
                db.close().await;
                format!("encrypted {seen}")
            }
            Err(error) => error.to_string(),
        }
    };
    let mut verified = profile();
    verified.tls = TlsMode::VerifyFull;
    verified.ca_file = Some(authority.clone().into());
    // The address of the profile is named in the certificate.
    assert_eq!(outcome(verified.clone(), None).await, "encrypted TRUE");
    // Not issued by an authority everybody knows, nor by another one.
    let mut public = verified.clone();
    public.ca_file = None;
    assert_eq!(outcome(public, None).await, "database_certificate_rejected");
    let mut foreign = verified.clone();
    foreign.ca_file = Some(other.into());
    assert_eq!(
        outcome(foreign, None).await,
        "database_certificate_rejected"
    );
    // Through a tunnel the socket goes to a loopback port, and the name
    // that is verified is still the host of the profile.
    let mut named = verified.clone();
    named.host = name.clone();
    assert_eq!(outcome(named.clone(), Some(port)).await, "encrypted TRUE");
    let mut misnamed = verified.clone();
    misnamed.host = "another-server.example.test".into();
    assert_eq!(
        outcome(misnamed.clone(), Some(port)).await,
        "database_certificate_rejected"
    );
    // "Encryption required" encrypts without asking who answers.
    misnamed.tls = TlsMode::Require;
    misnamed.ca_file = None;
    assert_eq!(
        outcome(misnamed.clone(), Some(port)).await,
        "encrypted TRUE"
    );
    misnamed.tls = TlsMode::Disable;
    assert_eq!(outcome(misnamed, Some(port)).await, "encrypted FALSE");
    // A refusal leaves nothing behind: the verified connection still works.
    assert_eq!(outcome(named, Some(port)).await, "encrypted TRUE");
    println!(
        "verified against the CA file and the name of the profile; another name and another authority are refused"
    );
}

/// Logins that are not administrators: one whose language puts the day
/// first, and one that may only read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real SQL Server described by BAZA_DB_TEST_* variables"]
async fn other_logins_see_and_change_what_they_may() {
    let admin = open().await;
    drop_objects(&admin).await;
    let secret = script::text_literal(&profile().password);
    let logins = [
        ("baza_live_pl", "DEFAULT_LANGUAGE = polski", "db_owner"),
        (
            "baza_live_reader",
            "DEFAULT_LANGUAGE = us_english",
            "db_datareader",
        ),
    ];
    let cleanup = |admin: Database| async move {
        for (login, _, _) in logins {
            let _ = admin
                .query(
                    &format!(
                        "IF USER_ID(N'{login}') IS NOT NULL DROP USER {login}; IF SUSER_ID(N'{login}') IS NOT NULL DROP LOGIN {login}"
                    ),
                    CancellationToken::new(),
                )
                .await;
        }
    };
    cleanup(admin.clone()).await;
    for (login, language, role) in logins {
        let created = admin
            .query(
                &format!(
                    "CREATE LOGIN {login} WITH PASSWORD = {secret}, CHECK_POLICY = OFF, {language}; CREATE USER {login} FOR LOGIN {login}; ALTER ROLE {role} ADD MEMBER {login}"
                ),
                CancellationToken::new(),
            )
            .await;
        if let Err(error) = created {
            cleanup(admin.clone()).await;
            admin.close().await;
            println!("skipped: this account may not create a login ({error})");
            return;
        }
    }
    run(&admin, TYPES).await;
    let table = live("baza_live_types");

    // A login whose language reads 2026-10-12 as the tenth of December.
    let mut polish = profile();
    polish.username = "baza_live_pl".into();
    let db = Database::connect(&polish, None).await.unwrap();
    let session = run(
        &db,
        "SELECT @@LANGUAGE, CAST('2026-10-12 00:00:00' AS DATETIME)",
    )
    .await;
    assert_eq!(session.rows[0][0], text("polski"));
    assert_eq!(session.rows[0][1], text("2026-12-10T00:00:00"));
    let read = page(&db, &PageRequest::first(table.clone())).await;
    for (column, new) in [
        ("c_datetime", "2026-11-05T06:07:08.003"),
        ("c_small_dt", "2026-11-05T06:07:00"),
        ("c_datetime2", "2026-11-05T06:07:08.1234567"),
        ("c_date", "2026-11-05"),
        ("c_offset", "2026-11-05T06:07:08+01:00"),
    ] {
        let index = read
            .columns
            .iter()
            .position(|found| found.name == column)
            .unwrap();
        db.apply(&RowChange::UpdateCell {
            table: table.clone(),
            column: column.into(),
            old: read.rows[0][index].clone(),
            new: text(new),
            key: key(1),
        })
        .await
        .unwrap_or_else(|error| panic!("{column}: {error}"));
    }
    let after = page(&db, &PageRequest::first(table.clone())).await;
    assert_eq!(after.rows[0][14], text("2026-11-05T06:07:08.003"));
    assert_eq!(after.rows[0][15], text("2026-11-05T06:07:00"));
    assert_eq!(after.rows[0][11], text("2026-11-05"));
    assert_eq!(after.rows[0][18], text("2026-11-05T06:07:08+01:00"));
    // Owning a database is not the permission to look at the sessions of
    // the server: this login would count itself and nobody else.
    let stats = db.server_stats().await.unwrap();
    let names: Vec<&str> = stats
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(names, ["database", "version", "max_connections", "tables"]);
    db.close().await;
    // The administrator sees them.
    let stats = admin.server_stats().await.unwrap();
    let names: Vec<&str> = stats
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "database",
            "version",
            "connections",
            "active",
            "max_connections",
            "tables"
        ]
    );

    // A login that may only read: the monitor shows what it can see, a
    // change is refused by the server, and nothing is listed twice.
    let mut reader = profile();
    reader.username = "baza_live_reader".into();
    let db = Database::connect(&reader, None).await.unwrap();
    let stats = db.server_stats().await.unwrap();
    let names: Vec<&str> = stats
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    assert_eq!(names, ["database", "version", "max_connections", "tables"]);
    assert!(stats.rows[0][1].display().starts_with("Microsoft "));
    assert!(!stats.rows[0][1].display().contains('\n'));
    let tables = db.tables().await.unwrap();
    assert_eq!(
        tables
            .iter()
            .filter(|found| found.name == "baza_live_types")
            .count(),
        1
    );
    assert_eq!(
        page(&db, &PageRequest::first(table.clone()))
            .await
            .rows
            .len(),
        3
    );
    assert_eq!(
        db.apply(&RowChange::DeleteRow {
            table: table.clone(),
            key: key(2)
        })
        .await
        .unwrap_err()
        .to_string(),
        "database_error [229]"
    );
    assert_eq!(page(&db, &PageRequest::first(table)).await.rows.len(), 3);
    db.close().await;

    drop_objects(&admin).await;
    cleanup(admin.clone()).await;
    admin.close().await;
    println!(
        "a login with day-first dates edits dates; a read-only login reads and is refused changes"
    );
}
