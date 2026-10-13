//! What only Oracle has, checked against a real server. Ignored by default
//! and described by the variables of the driver checklist
//! (`tests/driver_checklist.rs`): `BAZA_DB_TEST_PORT`, `BAZA_DB_TEST_USER`,
//! `BAZA_DB_TEST_PASSWORD`, `BAZA_DB_TEST_DATABASE` (the service name),
//! optionally `BAZA_DB_TEST_HOST`, `BAZA_DB_TEST_PRODUCT` and
//! `BAZA_DB_TEST_OPTIONS`.
//!
//! ```text
//! cargo test -p baza-core --lib oracle::live -- --ignored --nocapture --test-threads 1
//! ```
//!
//! The account is an ordinary one that may create tables, views, procedures
//! and triggers in its own schema; a server of version 23ai is assumed,
//! because the tests use its JSON, BOOLEAN and VECTOR types. Tests that have
//! to look at the sessions of the server from outside also need
//! `BAZA_DB_TEST_ADMIN_USER`, an account with the same password that may
//! read `V$SESSION`; they say so and pass without it.
//! `the_instance_and_the_administrator_sign_in` needs `BAZA_DB_TEST_SID`.
//!
//! `certificates_are_verified_against_the_name_of_the_profile` puts a TLS
//! endpoint of its own in front of the listener. It needs a certificate for
//! that endpoint and its authority: `BAZA_DB_TEST_TLS_CERT` and
//! `BAZA_DB_TEST_TLS_KEY` (PEM, issued for the DNS name
//! `BAZA_DB_TEST_TLS_NAME`, which does not have to resolve),
//! `BAZA_DB_TEST_CA_FILE` (the authority) and `BAZA_DB_TEST_OTHER_CA_FILE`
//! (any other authority).
//!
//! Only objects whose names begin with `BAZA_LIVE_` are created, and they
//! are dropped at the end.
use super::*;
use crate::builder::{Filter, FilterOp};
use crate::database::Database;
use crate::model::ConnectionProfile;

fn variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn profile() -> ConnectionProfile {
    let required = |name: &str| variable(name).unwrap_or_else(|| panic!("{name} is required"));
    let mut profile = ConnectionProfile {
        name: "Real Oracle".into(),
        engine: ENGINE,
        product: variable("BAZA_DB_TEST_PRODUCT").unwrap_or_default(),
        host: variable("BAZA_DB_TEST_HOST").unwrap_or_else(|| "127.0.0.1".into()),
        port: required("BAZA_DB_TEST_PORT").parse().unwrap(),
        database: required("BAZA_DB_TEST_DATABASE"),
        username: required("BAZA_DB_TEST_USER"),
        password: variable("BAZA_DB_TEST_PASSWORD").unwrap_or_default(),
        tls: TlsMode::Disable,
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

/// The schema of the account: its name as Oracle folds it.
fn schema() -> String {
    profile().username.to_uppercase()
}

async fn open() -> Database {
    Database::connect(&profile(), None).await.unwrap()
}

/// A connection of an administrator, to look at the server from outside.
async fn admin() -> Option<Database> {
    let username = variable("BAZA_DB_TEST_ADMIN_USER")?;
    let profile = ConnectionProfile {
        username,
        ..profile()
    };
    Some(Database::connect(&profile, None).await.unwrap())
}

async fn refused(profile: &ConnectionProfile) -> String {
    Database::connect(profile, None)
        .await
        .err()
        .expect("a refused connection")
        .to_string()
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

/// The single value a statement returns, as the grid would show it.
async fn value(db: &Database, statement: &str) -> String {
    run(db, statement).await.rows[0][0].display()
}

fn live(name: &str) -> TableRef {
    TableRef {
        schema: schema(),
        name: name.into(),
        kind: "table".into(),
    }
}

/// Drop what a test of this file may have left behind.
async fn drop_objects(db: &Database) {
    for (kind, name) in [
        ("VIEW", "BAZA_LIVE_VIEW"),
        ("MATERIALIZED VIEW", "BAZA_LIVE_MVIEW"),
        ("TRIGGER", "BAZA_LIVE_TRIGGER"),
        ("PROCEDURE", "BAZA_LIVE_PROCEDURE"),
        ("FUNCTION", "BAZA_LIVE_FUNCTION"),
        ("PACKAGE", "BAZA_LIVE_PACKAGE"),
        ("TYPE", "BAZA_LIVE_TYPE"),
        ("TABLE", "BAZA_LIVE_TYPES"),
        ("TABLE", "BAZA_LIVE_NOTES"),
        ("TABLE", "BAZA_LIVE_LINES"),
        ("TABLE", "BAZA_LIVE_HEAP"),
        ("TABLE", "BAZA_LIVE_LARGE"),
        ("TABLE", "BAZA_LIVE_STATE"),
        ("TABLE", "\"baza_live_Mixed Case\""),
    ] {
        let purge = if kind == "TABLE" { " PURGE" } else { "" };
        run(
            db,
            &format!(
                "BEGIN EXECUTE IMMEDIATE '{}'; EXCEPTION WHEN OTHERS THEN NULL; END;",
                format!("DROP {kind} {name}{purge}").replace('\'', "''")
            ),
        )
        .await;
    }
}

fn text(value: &str) -> Cell {
    Cell::Text(value.into())
}
fn key(id: i64) -> Vec<(String, Cell)> {
    vec![("ID".to_owned(), Cell::Integer(id.to_string()))]
}

async fn page(db: &Database, request: &PageRequest) -> QueryResult {
    db.read_page(request, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("{error}: {}", db.page_text(request).unwrap()))
}

const TYPES: &str = "CREATE TABLE baza_live_types (
  id NUMBER(10) PRIMARY KEY, c_num NUMBER, c_num2 NUMBER(12,2), c_int NUMBER(38), c_float FLOAT,
  c_neg NUMBER(10,-2), c_bf BINARY_FLOAT, c_bd BINARY_DOUBLE, c_vc VARCHAR2(50 CHAR),
  c_nvc NVARCHAR2(50), c_char CHAR(5), c_nchar NCHAR(5), c_date DATE, c_ts TIMESTAMP(9),
  c_ts0 TIMESTAMP(0), c_tz TIMESTAMP(6) WITH TIME ZONE, c_ltz TIMESTAMP(3) WITH LOCAL TIME ZONE,
  c_ym INTERVAL YEAR(3) TO MONTH, c_ds INTERVAL DAY(3) TO SECOND(4), c_raw RAW(16), c_clob CLOB,
  c_nclob NCLOB, c_blob BLOB, c_rowid ROWID, c_json JSON, c_bool BOOLEAN, c_vec VECTOR,
  c_xml XMLTYPE, c_list SYS.ODCINUMBERLIST);
INSERT INTO baza_live_types VALUES (1, 12345678901234567890123456789012345678, -1234567890.12,
  -99999999999999999999999999999999999999, 1/3, 12345, 1.1, 1.1, 'zażółć 🦀 it''s', N'gęślą jaźń',
  'ab', N'żó', DATE '2024-02-29', TIMESTAMP '2024-02-29 13:14:15.123456789',
  TIMESTAMP '2024-02-29 13:14:15', TIMESTAMP '2024-02-29 13:14:15.5 +05:30',
  TIMESTAMP '2024-02-29 13:14:15.25', INTERVAL '123-11' YEAR(3) TO MONTH,
  INTERVAL '-3 04:05:06.7891' DAY(3) TO SECOND(4), HEXTORAW('00FF10'), 'clob zażółć', N'nclob 🦀',
  HEXTORAW('DEADBEEF'), NULL, '{\"b\":[1,2.50,{\"c\":null}],\"a\":\"x\"}', TRUE, '[1.5,2,-3]',
  XMLTYPE('<a>1</a>'), SYS.ODCINUMBERLIST(1,2));
INSERT INTO baza_live_types (id) VALUES (2);
INSERT INTO baza_live_types (id, c_num, c_bf, c_bd, c_date, c_tz, c_vc, c_bool, c_ym, c_ds)
  VALUES (3, 0.000000000000000000000000000000000000001, BINARY_FLOAT_INFINITY, BINARY_DOUBLE_NAN,
  TO_DATE('-4712-01-01', 'SYYYY-MM-DD'), TIMESTAMP '2024-02-29 13:14:15 Europe/Warsaw', '', FALSE,
  INTERVAL '-0-1' YEAR TO MONTH, INTERVAL '0 00:00:00.5' DAY TO SECOND);
UPDATE baza_live_types SET c_rowid = ROWID WHERE id = 1";

/// Every type as the cell the grid shows, and every editable one changed in
/// place: the text of a cell is a literal the server reads back as the same
/// value, so the conflict check of an edit finds its row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Oracle server described by BAZA_DB_TEST_* variables"]
async fn types_are_shown_and_edited_in_place() {
    let db = open().await;
    drop_objects(&db).await;
    let made = run(&db, TYPES).await;
    assert_eq!(made.affected, 4, "three rows inserted and one updated");
    // A name that was not quoted is listed the way Oracle keeps it.
    let tables = db.tables().await.unwrap();
    let table = tables
        .iter()
        .find(|table| table.name == "BAZA_LIVE_TYPES")
        .unwrap()
        .clone();
    assert_eq!(
        (table.schema.as_str(), table.kind.as_str()),
        (schema().as_str(), "table")
    );
    let columns = db.columns(&table).await.unwrap();
    let described: Vec<(&str, &str)> = columns
        .iter()
        .map(|column| (column.name.as_str(), column.data_type.as_str()))
        .collect();
    assert_eq!(
        described,
        [
            ("ID", "NUMBER(10)"),
            ("C_NUM", "NUMBER"),
            ("C_NUM2", "NUMBER(12,2)"),
            ("C_INT", "NUMBER(38)"),
            ("C_FLOAT", "FLOAT"),
            ("C_NEG", "NUMBER(10,-2)"),
            ("C_BF", "BINARY_FLOAT"),
            ("C_BD", "BINARY_DOUBLE"),
            ("C_VC", "VARCHAR2(50 CHAR)"),
            ("C_NVC", "NVARCHAR2(50)"),
            ("C_CHAR", "CHAR(5)"),
            ("C_NCHAR", "NCHAR(5)"),
            ("C_DATE", "DATE"),
            ("C_TS", "TIMESTAMP(9)"),
            ("C_TS0", "TIMESTAMP(0)"),
            ("C_TZ", "TIMESTAMP(6) WITH TIME ZONE"),
            ("C_LTZ", "TIMESTAMP(3) WITH LOCAL TIME ZONE"),
            ("C_YM", "INTERVAL YEAR(3) TO MONTH"),
            ("C_DS", "INTERVAL DAY(3) TO SECOND(4)"),
            ("C_RAW", "RAW(16)"),
            ("C_CLOB", "CLOB"),
            ("C_NCLOB", "NCLOB"),
            ("C_BLOB", "BLOB"),
            ("C_ROWID", "ROWID"),
            ("C_JSON", "JSON"),
            ("C_BOOL", "BOOLEAN"),
            ("C_VEC", "VECTOR"),
            ("C_XML", "XMLTYPE"),
            ("C_LIST", "ODCINUMBERLIST"),
        ]
    );
    assert!(columns[0].primary_key && !columns[0].nullable);
    assert!(
        columns[1..]
            .iter()
            .all(|column| !column.primary_key && column.nullable)
    );

    let first = PageRequest::first(table.clone());
    assert_eq!(
        db.page_text(&first).unwrap(),
        format!(
            "SELECT *\nFROM \"{}\".\"BAZA_LIVE_TYPES\"\nORDER BY \"ID\"\nOFFSET 0 ROWS FETCH NEXT 500 ROWS ONLY",
            schema()
        )
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
            "NUMBER(10)",
            "NUMBER",
            "NUMBER(12,2)",
            "NUMBER(38)",
            "NUMBER(126,-127)",
            "NUMBER(10,-2)",
            "BINARY_FLOAT",
            "BINARY_DOUBLE",
            "VARCHAR2(50)",
            "NVARCHAR2(50)",
            "CHAR(5)",
            "NCHAR(5)",
            "DATE",
            "TIMESTAMP(9)",
            "TIMESTAMP(0)",
            "TIMESTAMP(6) WITH TIME ZONE",
            "TIMESTAMP(3) WITH LOCAL TIME ZONE",
            "INTERVAL YEAR(3) TO MONTH",
            "INTERVAL DAY(3) TO SECOND(4)",
            "RAW(16)",
            "CLOB",
            "NCLOB",
            "BLOB",
            "ROWID",
            "JSON",
            "BOOLEAN",
            "VECTOR",
            "XMLTYPE",
            "ODCINUMBERLIST",
        ]
    );
    assert_eq!(read.rows.len(), 3);
    let expected = [
        Cell::Integer("1".into()),
        Cell::Decimal("12345678901234567890123456789012345678".into()),
        Cell::Decimal("-1234567890.12".into()),
        Cell::Integer("-99999999999999999999999999999999999999".into()),
        Cell::Decimal("0.33333333333333333333333333333333333333".into()),
        Cell::Integer("12300".into()),
        Cell::Float("1.1".into()),
        Cell::Float("1.1".into()),
        text("zażółć 🦀 it's"),
        text("gęślą jaźń"),
        text("ab   "),
        text("żó   "),
        text("2024-02-29 00:00:00"),
        text("2024-02-29 13:14:15.123456789"),
        text("2024-02-29 13:14:15"),
        text("2024-02-29 13:14:15.5 +05:30"),
        text("2024-02-29 13:14:15.25"),
        text("+123-11"),
        text("-003 04:05:06.7891"),
        Cell::Bytes(vec![0, 0xff, 0x10]),
        text("clob zażółć"),
        text("nclob 🦀"),
        Cell::Bytes(vec![0xde, 0xad, 0xbe, 0xef]),
    ];
    for (index, cell) in expected.iter().enumerate() {
        assert_eq!(&read.rows[0][index], cell, "{}", read.columns[index].name);
    }
    assert!(matches!(&read.rows[0][23], Cell::Text(rowid) if rowid.len() == 18));
    // The members of a JSON object come in the order of their names.
    assert_eq!(
        read.rows[0][24],
        Cell::Json("{\"a\":\"x\",\"b\":[1,2.5,{\"c\":null}]}".into())
    );
    assert_eq!(read.rows[0][25], Cell::Bool(true));
    assert_eq!(read.rows[0][26], Cell::Json("[1.5,2,-3]".into()));
    // What the client cannot read is named, and whether it is there.
    assert_eq!(read.rows[0][27], Cell::Unsupported("<XMLTYPE>".into()));
    assert_eq!(
        read.rows[0][28],
        Cell::Unsupported("<ODCINUMBERLIST>".into())
    );
    assert!(read.rows[1][1..].iter().all(|cell| *cell == Cell::Null));
    // What is not a number, a date before the common era, an empty text
    // (which is NULL) and a time zone given as a region.
    assert_eq!(
        read.rows[2][1],
        Cell::Decimal("0.000000000000000000000000000000000000001".into())
    );
    assert_eq!(read.rows[2][6], text("Inf"));
    assert_eq!(read.rows[2][7], text("Nan"));
    assert_eq!(read.rows[2][8], Cell::Null);
    assert_eq!(read.rows[2][12], text("-4712-01-01 00:00:00"));
    assert_eq!(read.rows[2][15], text("2024-02-29 13:14:15 +01:00"));
    assert_eq!(read.rows[2][17], text("-000-01"));
    assert_eq!(read.rows[2][18], text("+000 00:00:00.5000"));
    assert_eq!(read.rows[2][25], Cell::Bool(false));
    assert_eq!(read.rows[2][27], Cell::Null);

    // One cell after another, with the value on screen as the old value.
    let long = "długi tekst 'x' ż🦀\n".repeat(400);
    let edits = [
        ("C_NUM", Cell::Decimal("-0.000000001234".into())),
        ("C_NUM2", Cell::Decimal("99.99".into())),
        (
            "C_INT",
            Cell::Integer("99999999999999999999999999999999999999".into()),
        ),
        ("C_FLOAT", Cell::Decimal("2.5".into())),
        ("C_NEG", Cell::Integer("500".into())),
        ("C_BF", Cell::Float("0.1".into())),
        ("C_BD", Cell::Float("0.30000000000000004".into())),
        ("C_VC", text("nowy 'tekst' \\ 100% _x_ \"q\" :b")),
        ("C_NVC", text("ŻÓŁW 🦀")),
        ("C_CHAR", text("xy   ")),
        ("C_NCHAR", text("ąę   ")),
        ("C_DATE", text("2025-01-02 03:04:05")),
        ("C_TS", text("2025-01-02 03:04:05.000000001")),
        ("C_TS0", text("1999-12-31 23:59:59")),
        ("C_TZ", text("2025-06-01 12:00:00.25 -07:00")),
        ("C_LTZ", text("2025-01-02 03:04:05.125")),
        ("C_YM", text("+001-06")),
        ("C_DS", text("+001 02:03:04.5000")),
        ("C_RAW", Cell::Bytes(vec![1, 2, 3])),
        ("C_CLOB", text(&long)),
        ("C_NCLOB", text(&long)),
        ("C_BLOB", Cell::Bytes(vec![9; 1500])),
        ("C_JSON", Cell::Json("{\"k\":[true,null,\"ż\"]}".into())),
        ("C_BOOL", Cell::Bool(false)),
        ("C_VEC", Cell::Json("[0.5,1,2]".into())),
    ];
    for (column, new) in &edits {
        let index = columns
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
        // The old value is gone: the same edit again finds no row.
        assert_eq!(
            db.apply(&change).await.unwrap_err().to_string(),
            "row_changed_reload_required",
            "{column}"
        );
    }
    let after = page(&db, &first).await;
    for (column, new) in &edits {
        let index = columns
            .iter()
            .position(|found| found.name == *column)
            .unwrap();
        assert_eq!(&after.rows[0][index], new, "{column}");
    }
    // NULL is a value of its own on both sides of an edit, in every type.
    for (column, new) in &edits {
        let filled = RowChange::UpdateCell {
            table: table.clone(),
            column: (*column).into(),
            old: Cell::Null,
            new: new.clone(),
            key: key(2),
        };
        db.apply(&filled)
            .await
            .unwrap_or_else(|error| panic!("{column} from NULL: {error}"));
        let emptied = RowChange::UpdateCell {
            table: table.clone(),
            column: (*column).into(),
            old: new.clone(),
            new: Cell::Null,
            key: key(2),
        };
        db.apply(&emptied)
            .await
            .unwrap_or_else(|error| panic!("{column} to NULL: {error}"));
    }
    let emptied = page(&db, &first).await;
    assert!(emptied.rows[1][1..].iter().all(|cell| *cell == Cell::Null));
    // What is not a number is edited as the text it is shown as.
    for (column, old, new) in [
        ("C_BF", text("Inf"), text("-Inf")),
        ("C_BD", text("Nan"), text("Inf")),
    ] {
        db.apply(&RowChange::UpdateCell {
            table: table.clone(),
            column: column.into(),
            old,
            new,
            key: key(3),
        })
        .await
        .unwrap_or_else(|error| panic!("{column}: {error}"));
    }
    let special = page(&db, &first).await;
    assert_eq!(special.rows[2][6], text("-Inf"));
    assert_eq!(special.rows[2][7], text("Inf"));
    // The same values through the editor, where nothing is rewritten: the
    // client reads what it can, and says what it cannot.
    let plain = run(
        &db,
        "SELECT c_num, c_bf, c_ts, c_tz, c_ds, c_clob, c_json FROM baza_live_types WHERE id = 1",
    )
    .await;
    assert_eq!(
        plain.rows[0],
        [
            Cell::Decimal("-0.000000001234".into()),
            Cell::Float("0.1".into()),
            text("2025-01-02 03:04:05.000000001"),
            text("2025-06-01 12:00:00.25 -07:00"),
            text("+001 02:03:04.5000"),
            text(&long),
            Cell::Json("{\"k\":[true,null,\"ż\"]}".into()),
        ]
    );
    assert_eq!(
        fails(&db, "SELECT c_xml FROM baza_live_types").await,
        "oracle_type_unsupported"
    );
    assert_eq!(
        fails(&db, "SELECT c_tz FROM baza_live_types WHERE id = 3").await,
        "oracle_client_unsupported"
    );
    // The session goes on, on another connection.
    assert_eq!(
        value(&db, "SELECT COUNT(*) FROM baza_live_types").await,
        "3"
    );
    assert_eq!(
        value(
            &db,
            "SELECT TO_CHAR(c_tz, 'YYYY-MM-DD HH24:MI TZR') FROM baza_live_types WHERE id = 3"
        )
        .await,
        "2024-02-29 13:14 EUROPE/WARSAW"
    );
    drop_objects(&db).await;
    db.close().await;
}

/// An empty text is NULL in Oracle. Nothing the driver writes or compares
/// may pretend otherwise.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Oracle server described by BAZA_DB_TEST_* variables"]
async fn an_empty_text_is_null() {
    let db = open().await;
    drop_objects(&db).await;
    run(
        &db,
        "CREATE TABLE baza_live_notes (id NUMBER(10) PRIMARY KEY, label VARCHAR2(40), body CLOB)",
    )
    .await;
    let notes = live("BAZA_LIVE_NOTES");
    // Imported and inserted as an empty text, stored as NULL.
    let names = ["ID", "LABEL", "BODY"].map(str::to_owned);
    let rows = vec![
        vec![Cell::Integer("1".into()), text(""), text("")],
        vec![Cell::Integer("2".into()), text("a"), text("b")],
    ];
    db.import_rows(&notes, &names, &rows, CancellationToken::new())
        .await
        .unwrap();
    db.apply(&RowChange::InsertRow {
        table: notes.clone(),
        values: vec![
            ("ID".into(), Cell::Integer("3".into())),
            ("LABEL".into(), text("")),
        ],
    })
    .await
    .unwrap();
    let first = PageRequest::first(notes.clone());
    let read = page(&db, &first).await;
    assert_eq!(read.rows[0][1..], [Cell::Null, Cell::Null]);
    assert_eq!(read.rows[2][1..], [Cell::Null, Cell::Null]);
    assert_eq!(
        value(
            &db,
            "SELECT COUNT(*) FROM baza_live_notes WHERE label IS NULL"
        )
        .await,
        "2"
    );
    // "Equal to nothing" finds them, "not equal to nothing" the other one.
    let filtered = |op: FilterOp, value: Cell| PageRequest {
        filters: vec![Filter {
            column: "LABEL".into(),
            op,
            value,
        }],
        ..first.clone()
    };
    let ids = |result: &QueryResult| -> Vec<String> {
        result.rows.iter().map(|row| row[0].display()).collect()
    };
    assert_eq!(
        ids(&page(&db, &filtered(FilterOp::Equal, text(""))).await),
        ["1", "3"]
    );
    assert_eq!(
        ids(&page(&db, &filtered(FilterOp::NotEqual, text(""))).await),
        ["2"]
    );
    assert_eq!(
        ids(&page(&db, &filtered(FilterOp::Equal, Cell::Null)).await),
        ["1", "3"]
    );
    assert_eq!(
        ids(&page(&db, &filtered(FilterOp::Contains, text(""))).await),
        ["2"]
    );
    // A grid that still holds the empty text as the old value changes the
    // cell all the same, and an edit to an empty text makes it NULL.
    let edit = |id: i64, column: &str, old: Cell, new: Cell| RowChange::UpdateCell {
        table: notes.clone(),
        column: column.into(),
        old,
        new,
        key: key(id),
    };
    let change = edit(1, "LABEL", text(""), text("now"));
    assert!(
        db.change_text(&change)
            .unwrap()
            .ends_with("AND \"LABEL\" IS NULL")
    );
    db.apply(&change).await.unwrap();
    db.apply(&edit(1, "BODY", text(""), text("text")))
        .await
        .unwrap();
    db.apply(&edit(2, "LABEL", text("a"), text("")))
        .await
        .unwrap();
    db.apply(&edit(2, "BODY", text("b"), text("")))
        .await
        .unwrap();
    assert_eq!(
        db.apply(&edit(2, "LABEL", text("a"), text("c")))
            .await
            .unwrap_err()
            .to_string(),
        "row_changed_reload_required"
    );
    let read = page(&db, &first).await;
    assert_eq!(read.rows[0][1..], [text("now"), text("text")]);
    assert_eq!(read.rows[1][1..], [Cell::Null, Cell::Null]);
    drop_objects(&db).await;
    db.close().await;
}

/// What a person sees when the connection does not come about.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Oracle server described by BAZA_DB_TEST_* variables"]
async fn a_refused_sign_in_says_why() {
    let good = profile();
    let wrong_password = ConnectionProfile {
        password: "not the password".into(),
        ..good.clone()
    };
    assert_eq!(refused(&wrong_password).await, "database_login_rejected");
    let no_such_user = ConnectionProfile {
        username: "BAZA_LIVE_NOBODY".into(),
        ..good.clone()
    };
    assert_eq!(refused(&no_such_user).await, "database_login_rejected");
    let wrong_service = ConnectionProfile {
        database: "BAZA_NO_SUCH_SERVICE".into(),
        ..good.clone()
    };
    assert_eq!(refused(&wrong_service).await, "database_not_found");
    let mut wrong_sid = ConnectionProfile {
        database: String::new(),
        ..good.clone()
    };
    wrong_sid.set_option("sid", "NOSUCHSID");
    assert_eq!(refused(&wrong_sid).await, "oracle_sid_unknown");
    let mut both = good.clone();
    both.set_option("sid", "FREE");
    assert_eq!(refused(&both).await, "oracle_service_and_sid");
    let neither = ConnectionProfile {
        database: String::new(),
        ..good.clone()
    };
    assert_eq!(refused(&neither).await, "oracle_service_required");
    // A port nothing listens on.
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let closed = listener.local_addr().unwrap().port();
    drop(listener);
    let closed_port = ConnectionProfile {
        port: closed,
        ..good.clone()
    };
    assert_eq!(refused(&closed_port).await, "database_network_error");
    // A listener that speaks plain Oracle Net, asked for TLS.
    for tls in [TlsMode::Require, TlsMode::VerifyFull] {
        let encrypted = ConnectionProfile {
            tls,
            ..good.clone()
        };
        assert_eq!(refused(&encrypted).await, "database_tls_unsupported");
    }
    // Something that is not Oracle at all: it hangs up.
    let silent = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let silent_port = silent.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = silent.accept().await {
            drop(stream);
        }
    });
    let not_oracle = ConnectionProfile {
        port: silent_port,
        ..good.clone()
    };
    assert_eq!(refused(&not_oracle).await, "database_network_error");
    // And the good profile still connects, with the statement the
    // connection dialog tries it with.
    let db = Database::connect(&good, None).await.unwrap();
    assert_eq!(value(&db, "SELECT 1").await, "1");
    db.close().await;
}

/// A large object is read whole only while it is small; a longer one is
/// named with its length, and the cells next to it stay editable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Oracle server described by BAZA_DB_TEST_* variables"]
async fn large_objects_are_bounded() {
    let db = open().await;
    drop_objects(&db).await;
    run(
        &db,
        "CREATE TABLE baza_live_large (id NUMBER(10) PRIMARY KEY, label VARCHAR2(20), body CLOB, wide NCLOB, data BLOB);
         INSERT INTO baza_live_large VALUES (1, 'small', 'text', N'tekst ż', HEXTORAW('0102'));
         INSERT INTO baza_live_large (id, label) VALUES (2, 'large');
         INSERT INTO baza_live_large VALUES (3, 'empty', EMPTY_CLOB(), EMPTY_CLOB(), EMPTY_BLOB());
         DECLARE
           piece VARCHAR2(32767) := RPAD('ż', 10000, 'x');
           text CLOB;
           bytes BLOB;
         BEGIN
           DBMS_LOB.CREATETEMPORARY(text, TRUE);
           DBMS_LOB.CREATETEMPORARY(bytes, TRUE);
           FOR i IN 1..30 LOOP
             DBMS_LOB.WRITEAPPEND(text, LENGTH(piece), piece);
             DBMS_LOB.WRITEAPPEND(bytes, 10000, UTL_RAW.COPIES(HEXTORAW('AB'), 10000));
           END LOOP;
           UPDATE baza_live_large SET body = text, wide = text, data = bytes WHERE id = 2;
         END;",
    )
    .await;
    let large = live("BAZA_LIVE_LARGE");
    let first = PageRequest::first(large.clone());
    let read = page(&db, &first).await;
    assert_eq!(
        read.rows[0][2..],
        [text("text"), text("tekst ż"), Cell::Bytes(vec![1, 2])]
    );
    // 300 000 characters and bytes: more than is read for a cell.
    const { assert!(300_000 > values::LOB_LIMIT) };
    assert_eq!(
        read.rows[1][2..],
        [
            Cell::Unsupported("<CLOB 300000>".into()),
            Cell::Unsupported("<NCLOB 300000>".into()),
            Cell::Unsupported("<BLOB 300000>".into()),
        ]
    );
    // An object without content is one, not NULL: its text is empty.
    assert_eq!(
        read.rows[2][2..],
        [text(""), text(""), Cell::Bytes(Vec::new())]
    );
    // The row of the long values is edited all the same, and found by a
    // filter that searches the long text on the server.
    db.apply(&RowChange::UpdateCell {
        table: large.clone(),
        column: "LABEL".into(),
        old: text("large"),
        new: text("changed"),
        key: key(2),
    })
    .await
    .unwrap();
    let found = page(
        &db,
        &PageRequest {
            filters: vec![Filter {
                column: "BODY".into(),
                op: FilterOp::Contains,
                value: text("ŻXXX"),
            }],
            ..first.clone()
        },
    )
    .await;
    assert_eq!(found.rows.len(), 1);
    assert_eq!(found.rows[0][1], text("changed"));
    // A text just under the limit is read whole, written back in pieces a
    // literal may hold, and compared as a whole.
    let long: String = "aż'\n".repeat(60_000);
    assert!(long.chars().count() < values::LOB_LIMIT);
    for (column, old) in [("BODY", "text"), ("WIDE", "tekst ż")] {
        db.apply(&RowChange::UpdateCell {
            table: large.clone(),
            column: column.into(),
            old: text(old),
            new: text(&long),
            key: key(1),
        })
        .await
        .unwrap_or_else(|error| panic!("{column}: {error}"));
    }
    let read = page(&db, &first).await;
    assert_eq!(read.rows[0][2], text(&long));
    assert_eq!(read.rows[0][3], text(&long));
    let back = RowChange::UpdateCell {
        table: large.clone(),
        column: "BODY".into(),
        old: text(&long),
        new: text("short again"),
        key: key(1),
    };
    db.apply(&back).await.unwrap();
    assert_eq!(
        db.apply(&back).await.unwrap_err().to_string(),
        "row_changed_reload_required"
    );
    // The editor reads the same way.
    let plain = run(&db, "SELECT body, data FROM baza_live_large ORDER BY id").await;
    assert_eq!(plain.rows[0][0], text("short again"));
    assert_eq!(plain.rows[1][0], Cell::Unsupported("<CLOB 300000>".into()));
    assert_eq!(
        value(
            &db,
            "SELECT DBMS_LOB.GETLENGTH(body) FROM baza_live_large WHERE id = 2"
        )
        .await,
        "300000"
    );
    drop_objects(&db).await;
    db.close().await;
}

/// A script is cut the way SQL*Plus cuts one, and a run is a transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Oracle server described by BAZA_DB_TEST_* variables"]
async fn a_script_runs_like_in_sql_plus() {
    let db = open().await;
    drop_objects(&db).await;
    let result = run(
        &db,
        "CREATE TABLE baza_live_state (id NUMBER(10) PRIMARY KEY, note VARCHAR2(100), changed DATE);
CREATE OR REPLACE PROCEDURE baza_live_procedure(p_id IN NUMBER, p_note IN VARCHAR2) AS
BEGIN
  INSERT INTO baza_live_state (id, note) VALUES (p_id, p_note);
END;
/
-- :NEW of a trigger is not a value somebody has to supply.
CREATE OR REPLACE TRIGGER baza_live_trigger BEFORE INSERT OR UPDATE ON baza_live_state FOR EACH ROW
BEGIN
  :NEW.changed := DATE '2024-01-01';
END;
/
CREATE OR REPLACE FUNCTION baza_live_function(p IN NUMBER) RETURN NUMBER AS
BEGIN
  RETURN CASE WHEN p > 2 THEN p * 10 ELSE 0 END;
END baza_live_function;
/
BEGIN
  baza_live_procedure(1, 'from a block; with a semicolon');
END;
/
EXEC baza_live_procedure(2, q'[it's from exec]')
DECLARE
  n NUMBER := 0;
BEGIN
  FOR i IN 3..5 LOOP
    IF i > 0 THEN
      baza_live_procedure(i, 'loop ' || baza_live_function(i));
    END IF;
  END LOOP;
END;
UPDATE baza_live_state SET note = note || '!' WHERE id >= 4;
SELECT id, note, changed FROM baza_live_state ORDER BY id",
    )
    .await;
    assert_eq!(result.affected, 2, "the rows of the UPDATE");
    let notes: Vec<String> = result.rows.iter().map(|row| row[1].display()).collect();
    assert_eq!(
        notes,
        [
            "from a block; with a semicolon",
            "it's from exec",
            "loop 30",
            "loop 40!",
            "loop 50!"
        ]
    );
    assert!(
        result
            .rows
            .iter()
            .all(|row| row[2] == text("2024-01-01 00:00:00"))
    );
    let count = || async { value(&db, "SELECT COUNT(*) FROM baza_live_state").await };
    // A run that fails leaves nothing behind.
    assert_eq!(
        fails(
            &db,
            "INSERT INTO baza_live_state (id) VALUES (6); INSERT INTO baza_live_state (id) VALUES (6)"
        )
        .await,
        "database_error [ORA-00001]"
    );
    assert_eq!(count().await, "5");
    // COMMIT, ROLLBACK and SAVEPOINT do what they say.
    assert_eq!(
        fails(
            &db,
            "INSERT INTO baza_live_state (id) VALUES (6); COMMIT; INSERT INTO baza_live_state (id) VALUES (6)"
        )
        .await,
        "database_error [ORA-00001]"
    );
    assert_eq!(count().await, "6");
    run(
        &db,
        "INSERT INTO baza_live_state (id) VALUES (7); ROLLBACK; INSERT INTO baza_live_state (id) VALUES (8);
         SAVEPOINT here; INSERT INTO baza_live_state (id) VALUES (9); ROLLBACK TO here",
    )
    .await;
    assert_eq!(
        value(
            &db,
            "SELECT LISTAGG(id, ',') WITHIN GROUP (ORDER BY id) FROM baza_live_state"
        )
        .await,
        "1,2,3,4,5,6,8"
    );
    // Nothing stays open between two runs.
    run(
        &db,
        "SELECT id FROM baza_live_state WHERE id = 1 FOR UPDATE",
    )
    .await;
    run(
        &db,
        "UPDATE baza_live_state SET note = 'not blocked' WHERE id = 1",
    )
    .await;
    // DDL commits by itself, as it does in Oracle, whatever comes after.
    assert_eq!(
        fails(
            &db,
            "INSERT INTO baza_live_state (id) VALUES (10); COMMENT ON TABLE baza_live_state IS 'x'; SELECT * FROM baza_live_missing"
        )
        .await,
        "database_error [ORA-00942]"
    );
    assert_eq!(count().await, "8");
    // A bind variable has no value here, and says so before anything runs.
    assert_eq!(
        fails(
            &db,
            "DELETE FROM baza_live_state; SELECT * FROM baza_live_state WHERE id = :id"
        )
        .await,
        "oracle_placeholder_unsupported"
    );
    assert_eq!(count().await, "8");
    // A unit that does not compile is created by Oracle all the same; the
    // rest of the script runs, and the run says what happened.
    assert_eq!(
        fails(
            &db,
            "CREATE OR REPLACE PROCEDURE baza_live_procedure AS\nBEGIN\n  no_such_thing;\nEND;\n/\nINSERT INTO baza_live_state (id) VALUES (11)"
        )
        .await,
        "oracle_compiled_with_errors"
    );
    assert_eq!(count().await, "9");
    assert_eq!(
        value(
            &db,
            "SELECT status FROM user_objects WHERE object_name = 'BAZA_LIVE_PROCEDURE'"
        )
        .await,
        "INVALID"
    );
    // Compiled again, it is as good as new.
    run(
        &db,
        "CREATE OR REPLACE PROCEDURE baza_live_procedure AS BEGIN NULL; END;",
    )
    .await;
    // Errors of the server keep their number, and the session goes on.
    assert_eq!(
        fails(&db, "SELEC 1 FROM dual").await,
        "database_error [ORA-00900]"
    );
    assert_eq!(
        fails(&db, "SELECT 1/0 FROM dual").await,
        "database_error [ORA-01476]"
    );
    assert_eq!(
        fails(&db, "BEGIN RAISE_APPLICATION_ERROR(-20001, 'no'); END;").await,
        "database_error [ORA-20001]"
    );
    assert_eq!(
        fails(&db, "SELECT 'open FROM dual").await,
        "sql_unterminated_literal"
    );
    // Queries of every shape are queries.
    assert_eq!(
        value(&db, "WITH c AS (SELECT 2 n FROM dual) SELECT n * 21 FROM c").await,
        "42"
    );
    assert_eq!(
        value(
            &db,
            "(SELECT 1 FROM dual) UNION ALL (SELECT 2 FROM dual) ORDER BY 1 DESC"
        )
        .await,
        "2"
    );
    assert_eq!(
        value(&db, " /* first */ -- a comment\nSELECT 'x' FROM dual;").await,
        "x"
    );
    let merged = run(
        &db,
        "MERGE INTO baza_live_state t USING (SELECT 1 id FROM dual UNION ALL SELECT 99 FROM dual) s ON (t.id = s.id)
         WHEN MATCHED THEN UPDATE SET t.note = 'merged' WHEN NOT MATCHED THEN INSERT (id, note) VALUES (s.id, 'new')",
    )
    .await;
    assert_eq!(merged.affected, 2);
    assert!(
        run(&db, "-- nothing but a comment\n;")
            .await
            .columns
            .is_empty()
    );
    drop_objects(&db).await;
    db.close().await;
}

/// The statements of other sessions of the account that an administrator
/// sees running, by a comment in their text.
async fn running(watcher: &Database, marker: &str) -> usize {
    let statement = format!(
        "SELECT COUNT(*) FROM v$session s JOIN v$sqlarea q ON q.sql_id = s.sql_id WHERE s.status = 'ACTIVE' AND s.username = '{}' AND q.sql_text LIKE '%{marker}%' AND q.sql_text NOT LIKE '%sqlarea%'",
        schema()
    );
    value(watcher, &statement).await.parse().unwrap()
}

/// Wait until nothing with this marker runs on the server any more.
async fn gone(watcher: &Database, marker: &str, seconds: u64) -> bool {
    for _ in 0..seconds * 4 {
        if running(watcher, marker).await == 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

/// A statement that is stopped is stopped on the server, and what it had
/// changed is rolled back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Oracle server described by BAZA_DB_TEST_* variables"]
async fn a_running_statement_is_stopped_on_the_server() {
    let db = open().await;
    drop_objects(&db).await;
    run(
        &db,
        "CREATE TABLE baza_live_state (id NUMBER(10) PRIMARY KEY, note VARCHAR2(100), changed DATE)",
    )
    .await;
    let watcher = admin().await;
    if watcher.is_none() {
        println!("BAZA_DB_TEST_ADMIN_USER is not set: the server is not watched from outside");
    }
    for (name, statement) in [
        (
            "a query",
            "SELECT /* baza_live_slow */ COUNT(*) FROM all_objects a, all_objects b, all_objects c WHERE a.object_name || b.object_name || c.object_name = 'x'",
        ),
        (
            "a block",
            "DECLARE /* baza_live_slow */ n NUMBER := 0; BEGIN LOOP n := n + 1; EXIT WHEN n > 100000000000; END LOOP; END;",
        ),
        (
            "an insert",
            "INSERT /* baza_live_slow */ INTO baza_live_state (id, note) SELECT ROWNUM, a.object_name FROM all_objects a, all_objects b, all_objects c WHERE ROWNUM <= 50000000",
        ),
    ] {
        let token = CancellationToken::new();
        let worker = db.clone();
        let cancel = token.clone();
        let task = tokio::spawn(async move { worker.query(statement, cancel).await });
        tokio::time::sleep(Duration::from_millis(1200)).await;
        if let Some(watcher) = &watcher {
            assert_eq!(running(watcher, "baza_live_slow").await, 1, "{name} runs");
        }
        let asked = Instant::now();
        token.cancel();
        let stopped = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap_or_else(|_| panic!("{name} was not stopped"))
            .unwrap();
        assert_eq!(
            stopped.unwrap_err().to_string(),
            "query_cancelled",
            "{name}"
        );
        assert!(asked.elapsed() < Duration::from_secs(1), "{name}");
        // The session goes on at once, on another connection.
        assert_eq!(
            value(&db, "SELECT COUNT(*) FROM baza_live_state").await,
            "0",
            "{name}"
        );
        if let Some(watcher) = &watcher {
            assert!(
                gone(watcher, "baza_live_slow", 5).await,
                "{name} still runs"
            );
        }
        println!(
            "{name}: stopped {} ms after it was asked",
            asked.elapsed().as_millis()
        );
    }
    // A token that was cancelled before anything was sent.
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        db.query("SELECT 1 FROM dual", cancelled)
            .await
            .unwrap_err()
            .to_string(),
        "query_cancelled"
    );
    // A statement that only waits does not look at the network. The break
    // reaches it when its wait is over: the person is free at once, the
    // server a little later.
    let token = CancellationToken::new();
    let worker = db.clone();
    let cancel = token.clone();
    let task = tokio::spawn(async move {
        worker
            .query(
                "BEGIN /* baza_live_wait */ DBMS_SESSION.SLEEP(6); END;",
                cancel,
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(800)).await;
    let asked = Instant::now();
    token.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("the wait was not given up")
        .unwrap();
    assert_eq!(stopped.unwrap_err().to_string(), "query_cancelled");
    assert!(asked.elapsed() < Duration::from_secs(1));
    assert_eq!(value(&db, "SELECT 1 FROM dual").await, "1");
    if let Some(watcher) = &watcher {
        assert!(
            gone(watcher, "baza_live_wait", 12).await,
            "the wait never ended"
        );
        println!(
            "a wait: given up at once, gone from the server after {} ms",
            asked.elapsed().as_millis()
        );
        watcher.close().await;
    }
    drop_objects(&db).await;
    db.close().await;
}

/// A result holds as much as a result may, and says when it was cut.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Oracle server described by BAZA_DB_TEST_* variables"]
async fn a_result_is_cut_at_its_limits() {
    let db = open().await;
    let whole = run(&db, "SELECT LEVEL n FROM dual CONNECT BY LEVEL <= 1500").await;
    assert_eq!((whole.rows.len(), whole.truncated), (1500, false));
    assert_eq!(whole.rows[1499][0], Cell::Decimal("1500".into()));
    let rows = run(&db, "SELECT LEVEL n FROM dual CONNECT BY LEVEL <= 25000").await;
    assert_eq!((rows.rows.len(), rows.truncated), (MAX_RESULT_ROWS, true));
    // 4000 bytes a row: the bytes run out before the rows do.
    let bytes = run(
        &db,
        "SELECT RPAD('x', 4000, 'x') t FROM dual CONNECT BY LEVEL <= 19000",
    )
    .await;
    assert!(bytes.truncated);
    assert_eq!(bytes.rows.len(), MAX_RESULT_BYTES / 4000);
    // The connection is as good as before, and a cut result in the middle
    // of a script does not end the script.
    let after = run(
        &db,
        "SELECT LEVEL n FROM dual CONNECT BY LEVEL <= 25000; SELECT 'after' FROM dual",
    )
    .await;
    assert_eq!((after.rows.len(), after.truncated), (1, false));
    assert_eq!(after.rows[0][0], text("after"));
    db.close().await;
}

/// An import is all of its rows or none of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Oracle server described by BAZA_DB_TEST_* variables"]
async fn an_import_is_all_or_nothing() {
    let db = open().await;
    drop_objects(&db).await;
    run(
        &db,
        "CREATE TABLE baza_live_lines (id NUMBER(10) PRIMARY KEY, amount NUMBER(12,2), label VARCHAR2(40 CHAR),
         placed DATE, seen TIMESTAMP WITH TIME ZONE, paid BOOLEAN, ratio BINARY_DOUBLE, tag RAW(4), body CLOB, doc JSON)",
    )
    .await;
    let lines = live("BAZA_LIVE_LINES");
    let names = [
        "ID", "AMOUNT", "LABEL", "PLACED", "SEEN", "PAID", "RATIO", "TAG", "BODY", "DOC",
    ]
    .map(str::to_owned);
    let long = "wiersz 'ż' \\ %\n".repeat(900);
    let row = |id: i64| {
        vec![
            Cell::Integer(id.to_string()),
            // A file gives a number as text as often as not.
            if id % 2 == 0 {
                Cell::Decimal(format!("{id}.25"))
            } else {
                text(&format!("{id}.25"))
            },
            text(&format!("Zażółć {id} 🦀")),
            text("2026-10-12 08:30:00"),
            text("2026-10-12 08:30:00.5 +02:00"),
            Cell::Bool(id % 3 == 0),
            Cell::Float("0.1".into()),
            Cell::Bytes(vec![0xca, 0xfe]),
            text(&long),
            Cell::Json(format!("{{\"id\":{id}}}")),
        ]
    };
    let mut rows: Vec<Vec<Cell>> = (1..=250).map(row).collect();
    let imported = db
        .import_rows(&lines, &names, &rows, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(imported.affected, 250);
    let read = page(
        &db,
        &PageRequest {
            limit: 3,
            ..PageRequest::first(lines.clone())
        },
    )
    .await;
    assert_eq!(
        read.rows[2],
        [
            Cell::Integer("3".into()),
            Cell::Decimal("3.25".into()),
            text("Zażółć 3 🦀"),
            text("2026-10-12 08:30:00"),
            text("2026-10-12 08:30:00.5 +02:00"),
            Cell::Bool(true),
            Cell::Float("0.1".into()),
            Cell::Bytes(vec![0xca, 0xfe]),
            text(&long),
            Cell::Json("{\"id\":3}".into()),
        ]
    );
    run(&db, "DELETE FROM baza_live_lines").await;
    // A value that is no number, in the last piece of the import.
    rows[240][1] = text("not a number");
    assert_eq!(
        db.import_rows(&lines, &names, &rows, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string(),
        "database_error [ORA-01722]"
    );
    assert_eq!(
        value(&db, "SELECT COUNT(*) FROM baza_live_lines").await,
        "0"
    );
    // A key twice, in the first piece.
    rows[240][1] = Cell::Null;
    rows[7][0] = Cell::Integer("3".into());
    assert_eq!(
        db.import_rows(&lines, &names, &rows, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string(),
        "database_error [ORA-00001]"
    );
    assert_eq!(
        value(&db, "SELECT COUNT(*) FROM baza_live_lines").await,
        "0"
    );
    // Given up before it started.
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    rows[7][0] = Cell::Integer("8".into());
    assert_eq!(
        db.import_rows(&lines, &names, &rows, cancelled)
            .await
            .unwrap_err()
            .to_string(),
        "query_cancelled"
    );
    assert_eq!(
        value(&db, "SELECT COUNT(*) FROM baza_live_lines").await,
        "0"
    );
    // And the same rows, now sound, go in.
    db.import_rows(&lines, &names, &rows, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        value(&db, "SELECT COUNT(*) FROM baza_live_lines").await,
        "250"
    );
    drop_objects(&db).await;
    db.close().await;
}

/// The side panel: the schema of the account first, tables, views and
/// materialized views, the schemas of Oracle only when they are asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Oracle server described by BAZA_DB_TEST_* variables"]
async fn the_side_panel_lists_what_belongs_to_people() {
    let db = open().await;
    drop_objects(&db).await;
    run(
        &db,
        "CREATE TABLE baza_live_state (id NUMBER GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
           note VARCHAR2(100) DEFAULT 'new' NOT NULL, changed DATE DEFAULT SYSDATE,
           shout VARCHAR2(100) GENERATED ALWAYS AS (UPPER(note)) VIRTUAL, hidden NUMBER INVISIBLE);
         CREATE TABLE baza_live_lines (order_id NUMBER(10), line_no NUMBER(5), label VARCHAR2(20),
           CONSTRAINT baza_live_lines_pk PRIMARY KEY (order_id, line_no));
         CREATE TABLE baza_live_heap (a NUMBER, b VARCHAR2(10));
         CREATE TABLE \"baza_live_Mixed Case\" (\"Id\" NUMBER(5) PRIMARY KEY, \"Opis ż\" VARCHAR2(20));
         CREATE VIEW baza_live_view AS SELECT id, note FROM baza_live_state;
         CREATE MATERIALIZED VIEW baza_live_mview AS SELECT a, b FROM baza_live_heap",
    )
    .await;
    let tables = db.tables().await.unwrap();
    let own = schema();
    // The schema of the account is the first one.
    assert_eq!(tables[0].schema, own);
    let kind = |name: &str| {
        let found: Vec<&TableRef> = tables
            .iter()
            .filter(|table| table.schema == own && table.name == name)
            .collect();
        assert_eq!(found.len(), 1, "{name} is listed once");
        found[0].kind.clone()
    };
    assert_eq!(kind("BAZA_LIVE_STATE"), "table");
    assert_eq!(kind("BAZA_LIVE_LINES"), "table");
    assert_eq!(kind("baza_live_Mixed Case"), "table");
    assert_eq!(kind("BAZA_LIVE_VIEW"), "view");
    // The table that holds the rows of a materialized view is not a second
    // entry next to it.
    assert_eq!(kind("BAZA_LIVE_MVIEW"), "materialized view");
    // Nothing of Oracle's own.
    for hidden in ["SYS", "SYSTEM", "XDB", "MDSYS", "CTXSYS", "AUDSYS"] {
        if hidden != own {
            assert!(
                tables.iter().all(|table| table.schema != hidden),
                "{hidden}"
            );
        }
    }
    let mut asked = profile();
    asked.set_option("system_schemas", "true");
    let everything = Database::connect(&asked, None).await.unwrap();
    let all = everything.tables().await.unwrap();
    assert_eq!(all[0].schema, own);
    assert!(
        all.iter()
            .any(|table| table.schema == "SYS" && table.name == "DUAL")
    );
    assert!(all.len() > tables.len());
    everything.close().await;

    // A column the server fills in, a default, a computed column; a column
    // that `SELECT *` does not return is not listed.
    let state = db.columns(&live("BAZA_LIVE_STATE")).await.unwrap();
    let described: Vec<(&str, &str, bool, bool, Option<&str>)> = state
        .iter()
        .map(|column| {
            (
                column.name.as_str(),
                column.data_type.as_str(),
                column.nullable,
                column.primary_key,
                column.default.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        described,
        [
            ("ID", "NUMBER", false, true, Some("IDENTITY")),
            ("NOTE", "VARCHAR2(100)", false, false, Some("'new'")),
            ("CHANGED", "DATE", true, false, Some("SYSDATE")),
            (
                "SHOUT",
                "VARCHAR2(100)",
                true,
                false,
                Some("AS (UPPER(\"NOTE\"))")
            ),
        ]
    );
    // A row of defaults, as the grid adds one: Oracle has no DEFAULT
    // VALUES, so the first column is given its default by name.
    let defaults = RowChange::InsertRow {
        table: live("BAZA_LIVE_STATE"),
        values: Vec::new(),
    };
    assert_eq!(
        db.change_text(&defaults).unwrap(),
        format!("INSERT INTO \"{own}\".\"BAZA_LIVE_STATE\" (\"ID\") VALUES (DEFAULT)")
    );
    db.apply(&defaults).await.unwrap();
    db.apply(&RowChange::InsertRow {
        table: live("BAZA_LIVE_STATE"),
        values: vec![("NOTE".into(), text("typed"))],
    })
    .await
    .unwrap();
    let rows = page(&db, &PageRequest::first(live("BAZA_LIVE_STATE"))).await;
    assert_eq!(rows.rows[0][..2], [Cell::Decimal("1".into()), text("new")]);
    assert_eq!(rows.rows[1][0], Cell::Decimal("2".into()));
    assert_eq!(rows.rows[1][1], text("typed"));
    assert_eq!(rows.rows[1][3], text("TYPED"));
    assert_eq!(rows.columns.len(), 4);

    // A key of two columns orders the page and names the row.
    let lines = live("BAZA_LIVE_LINES");
    let columns = db.columns(&lines).await.unwrap();
    assert_eq!(
        columns
            .iter()
            .map(|column| column.primary_key)
            .collect::<Vec<_>>(),
        [true, true, false]
    );
    run(
        &db,
        "INSERT INTO baza_live_lines SELECT TRUNC((LEVEL + 2) / 3), MOD(LEVEL, 3) + 1, 'line ' || LEVEL FROM dual CONNECT BY LEVEL <= 9",
    )
    .await;
    assert!(
        db.page_text(&PageRequest::first(lines.clone()))
            .unwrap()
            .contains("\nORDER BY \"ORDER_ID\", \"LINE_NO\"\n")
    );
    let second = page(
        &db,
        &PageRequest {
            limit: 4,
            offset: 4,
            ..PageRequest::first(lines.clone())
        },
    )
    .await;
    let keys: Vec<String> = second
        .rows
        .iter()
        .map(|row| format!("{}/{}", row[0].display(), row[1].display()))
        .collect();
    assert_eq!(keys, ["2/2", "2/3", "3/1", "3/2"]);
    db.apply(&RowChange::DeleteRow {
        table: lines.clone(),
        key: vec![
            ("ORDER_ID".into(), Cell::Integer("2".into())),
            ("LINE_NO".into(), Cell::Integer("3".into())),
        ],
    })
    .await
    .unwrap();
    assert_eq!(
        value(&db, "SELECT COUNT(*) FROM baza_live_lines").await,
        "8"
    );

    // A name with capitals, a space and a Polish letter is quoted as it is.
    let mixed = live("baza_live_Mixed Case");
    db.apply(&RowChange::InsertRow {
        table: mixed.clone(),
        values: vec![
            ("Id".into(), Cell::Integer("1".into())),
            ("Opis ż".into(), text("zażółć")),
        ],
    })
    .await
    .unwrap();
    let found = page(
        &db,
        &PageRequest {
            filters: vec![Filter {
                column: "Opis ż".into(),
                op: FilterOp::Contains,
                value: text("ŻÓŁ"),
            }],
            order_by: Some(("Opis ż".into(), true)),
            ..PageRequest::first(mixed.clone())
        },
    )
    .await;
    assert_eq!(
        found.rows,
        [vec![Cell::Integer("1".into()), text("zażółć")]]
    );

    // A table without a key and a view have pages too: one is ordered by
    // the address of its rows, the other as the server returns it.
    run(
        &db,
        "INSERT INTO baza_live_heap SELECT LEVEL, 'h' || LEVEL FROM dual CONNECT BY LEVEL <= 7",
    )
    .await;
    let heap = live("BAZA_LIVE_HEAP");
    assert!(
        db.columns(&heap)
            .await
            .unwrap()
            .iter()
            .all(|column| !column.primary_key)
    );
    let text_of = db.page_text(&PageRequest::first(heap.clone())).unwrap();
    assert!(text_of.contains("\nORDER BY ROWID\n"), "{text_of}");
    let mut seen = Vec::new();
    for offset in [0, 3, 6] {
        let part = page(
            &db,
            &PageRequest {
                limit: 3,
                offset,
                ..PageRequest::first(heap.clone())
            },
        )
        .await;
        seen.extend(part.rows.iter().map(|row| row[0].display()));
    }
    seen.sort();
    assert_eq!(seen, ["1", "2", "3", "4", "5", "6", "7"]);
    let view = TableRef {
        kind: "view".into(),
        ..live("BAZA_LIVE_VIEW")
    };
    let through = page(&db, &PageRequest::first(view.clone())).await;
    assert_eq!(through.rows.len(), 2);
    assert!(
        !db.page_text(&PageRequest::first(view))
            .unwrap()
            .contains("ORDER BY")
    );
    drop_objects(&db).await;
    db.close().await;
}

/// The "Monitor": what every account may know, and the sessions of the
/// server for an account that may read them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Oracle server described by BAZA_DB_TEST_* variables"]
async fn the_monitor_shows_what_the_account_may_see() {
    let db = open().await;
    let stats = db.server_stats().await.unwrap();
    assert_eq!(stats.rows.len(), 1);
    let names: Vec<&str> = stats
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect();
    let at = |name: &str| {
        let index = names.iter().position(|found| *found == name).unwrap();
        stats.rows[0][index].display()
    };
    assert!(at("version").starts_with("Oracle"), "{}", at("version"));
    assert_eq!(at("database"), profile().database.to_uppercase());
    assert!(at("tables").parse::<u32>().is_ok());
    println!("{}: {names:?} {}", profile().username, at("version"));
    let Some(watcher) = admin().await else {
        println!("BAZA_DB_TEST_ADMIN_USER is not set: only this account was asked");
        db.close().await;
        return;
    };
    // An ordinary account is not shown the sessions of others, and is not
    // told an error either.
    if !profile()
        .username
        .eq_ignore_ascii_case(&variable("BAZA_DB_TEST_ADMIN_USER").unwrap())
    {
        assert!(!names.contains(&"connections") && !names.contains(&"active"));
        assert_eq!(names[..2], ["database", "version"]);
    }
    let full = watcher.server_stats().await.unwrap();
    let names: Vec<&str> = full
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
    let number = |name: &str| -> u32 {
        let index = names.iter().position(|found| *found == name).unwrap();
        full.rows[0][index].display().parse().unwrap()
    };
    // This test holds two sessions itself.
    assert!(number("connections") >= 2);
    assert!(number("max_connections") >= number("connections"));
    // A statement of the other account is counted while it runs.
    let before = number("active");
    let token = CancellationToken::new();
    let worker = db.clone();
    let cancel = token.clone();
    let task = tokio::spawn(async move {
        worker
            .query(
                "SELECT /* baza_live_slow */ COUNT(*) FROM all_objects a, all_objects b, all_objects c WHERE a.object_name || b.object_name || c.object_name = 'x'",
                cancel,
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let during = watcher.server_stats().await.unwrap();
    let active: u32 = during.rows[0][3].display().parse().unwrap();
    assert!(active > before, "{active} active after {before}");
    token.cancel();
    let _ = task.await;
    watcher.close().await;
    db.close().await;
}

/// A server named by its SID, and the administrative roles.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Oracle server described by BAZA_DB_TEST_* variables"]
async fn the_instance_and_the_administrator_sign_in() {
    let (Some(sid), Some(administrator)) = (
        variable("BAZA_DB_TEST_SID"),
        variable("BAZA_DB_TEST_ADMIN_USER"),
    ) else {
        println!("BAZA_DB_TEST_SID and BAZA_DB_TEST_ADMIN_USER are not set: skipped");
        return;
    };
    // By SID the connection lands in the instance itself, where only a
    // common account exists.
    let mut by_sid = ConnectionProfile {
        database: String::new(),
        username: administrator,
        ..profile()
    };
    by_sid.set_option("sid", &sid);
    let db = Database::connect(&by_sid, None).await.unwrap();
    assert_eq!(
        value(
            &db,
            "SELECT SYS_CONTEXT('USERENV', 'INSTANCE_NAME') FROM dual"
        )
        .await,
        sid
    );
    assert!(db.server_stats().await.is_ok());
    assert!(!db.tables().await.unwrap().is_empty());
    db.close().await;
    // SYS signs in with a role only.
    let sys = ConnectionProfile {
        username: "sys".into(),
        ..profile()
    };
    assert_eq!(refused(&sys).await, "oracle_sys_needs_role");
    let mut sysdba = sys.clone();
    sysdba.set_option("role", "sysdba");
    let db = Database::connect(&sysdba, None).await.unwrap();
    assert_eq!(
        value(&db, "SELECT SYS_CONTEXT('USERENV', 'ISDBA') FROM dual").await,
        "TRUE"
    );
    assert_eq!(value(&db, "SELECT USER FROM dual").await, "SYS");
    db.close().await;
    // An ordinary account has no such role; Oracle answers that as it
    // answers a wrong password.
    let mut pretender = profile();
    if !pretender.username.eq_ignore_ascii_case("sys") {
        pretender.set_option("role", "sysdba");
        assert_eq!(refused(&pretender).await, "database_login_rejected");
    }
}

/// A TLS endpoint in front of the listener of the test. With `hand_over`
/// it behaves like a TCPS listener of Oracle that hands a connection to a
/// server process: it answers the first connect packet with "resend" and
/// the flag that announces a new TLS session, and then expects a second
/// handshake on the same socket. Returns its port and the number of
/// handshakes it completed.
async fn tls_endpoint(
    target: (String, u16),
    hand_over: bool,
) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let required = |name: &str| variable(name).unwrap_or_else(|| panic!("{name} is required"));
    let certificates: Vec<CertificateDer<'static>> =
        CertificateDer::pem_file_iter(required("BAZA_DB_TEST_TLS_CERT"))
            .unwrap()
            .map(Result::unwrap)
            .collect();
    let key = PrivateKeyDer::from_pem_file(required("BAZA_DB_TEST_TLS_KEY")).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certificates, key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let handshakes = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&handshakes);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let counted = Arc::clone(&counted);
            let target = target.clone();
            tokio::spawn(async move {
                let Ok(mut encrypted) = acceptor.accept(stream).await else {
                    return;
                };
                counted.fetch_add(1, Ordering::SeqCst);
                if hand_over {
                    // The connect packet, and the packet with its data when
                    // the data did not fit into it.
                    let mut header = [0u8; 8];
                    if encrypted.read_exact(&mut header).await.is_err() {
                        return;
                    }
                    let length = usize::from(u16::from_be_bytes([header[0], header[1]]));
                    let mut rest = vec![0u8; length.saturating_sub(8)];
                    if encrypted.read_exact(&mut rest).await.is_err() {
                        return;
                    }
                    let data = rest.get(16..18).map_or(0, |bytes| {
                        usize::from(u16::from_be_bytes([bytes[0], bytes[1]]))
                    });
                    if data > 230 {
                        let mut header = [0u8; 8];
                        if encrypted.read_exact(&mut header).await.is_err() {
                            return;
                        }
                        let length = usize::from(u16::from_be_bytes([header[0], header[1]]));
                        let mut rest = vec![0u8; length.saturating_sub(8)];
                        if encrypted.read_exact(&mut rest).await.is_err() {
                            return;
                        }
                    }
                    let resend = [0u8, 8, 0, 0, 11, 0x08, 0, 0];
                    if encrypted.write_all(&resend).await.is_err()
                        || encrypted.flush().await.is_err()
                    {
                        return;
                    }
                    let (stream, _) = encrypted.into_inner();
                    encrypted = match acceptor.accept(stream).await {
                        Ok(encrypted) => encrypted,
                        Err(_) => return,
                    };
                    counted.fetch_add(1, Ordering::SeqCst);
                }
                let Ok(mut plain) =
                    tokio::net::TcpStream::connect((target.0.as_str(), target.1)).await
                else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut encrypted, &mut plain).await;
            });
        }
    });
    (port, handshakes)
}

/// TLS in the three modes of a profile, with the address that is dialled
/// and the name in the certificate being two things, as behind an SSH
/// tunnel; and the second handshake a TCPS listener asks for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Oracle server described by BAZA_DB_TEST_* variables"]
async fn certificates_are_verified_against_the_name_of_the_profile() {
    use std::sync::atomic::Ordering;
    let (Some(authority), Some(other), Some(name)) = (
        variable("BAZA_DB_TEST_CA_FILE"),
        variable("BAZA_DB_TEST_OTHER_CA_FILE"),
        variable("BAZA_DB_TEST_TLS_NAME"),
    ) else {
        println!(
            "BAZA_DB_TEST_CA_FILE, BAZA_DB_TEST_OTHER_CA_FILE and BAZA_DB_TEST_TLS_NAME are not set: skipped"
        );
        return;
    };
    let good = profile();
    for hand_over in [false, true] {
        let (port, handshakes) = tls_endpoint((good.host.clone(), good.port), hand_over).await;
        // The name of the certificate does not resolve: the connection is
        // dialled the way a tunnel is, at a port of this machine.
        let through = |tls: TlsMode, ca_file: Option<&str>, host: &str| ConnectionProfile {
            host: host.into(),
            tls,
            ca_file: ca_file.map(Into::into),
            ..good.clone()
        };
        let each = if hand_over { 2 } else { 1 };
        // Encrypted, whoever signed the certificate.
        let db = Database::connect(
            &through(TlsMode::Require, None, "some-other-name.test"),
            Some(port),
        )
        .await
        .unwrap();
        assert_eq!(value(&db, "SELECT 1 FROM dual").await, "1");
        assert_eq!(handshakes.load(Ordering::SeqCst), each);
        db.close().await;
        // Verified against the authority of the profile and its host.
        let verified = through(TlsMode::VerifyFull, Some(&authority), &name);
        let db = Database::connect(&verified, Some(port)).await.unwrap();
        assert_eq!(
            value(&db, "SELECT COUNT(*) FROM all_objects WHERE ROWNUM <= 5000").await,
            "5000"
        );
        assert_eq!(handshakes.load(Ordering::SeqCst), 2 * each);
        // A statement is stopped through the encrypted connection as well.
        let token = CancellationToken::new();
        let worker = db.clone();
        let cancel = token.clone();
        let task = tokio::spawn(async move {
            worker
                .query(
                    "SELECT /* baza_live_slow */ COUNT(*) FROM all_objects a, all_objects b, all_objects c WHERE a.object_name || b.object_name || c.object_name = 'x'",
                    cancel,
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(800)).await;
        token.cancel();
        assert_eq!(
            task.await.unwrap().unwrap_err().to_string(),
            "query_cancelled"
        );
        if let Some(watcher) = admin().await {
            assert!(gone(&watcher, "baza_live_slow", 5).await);
            watcher.close().await;
        }
        assert_eq!(value(&db, "SELECT 2 FROM dual").await, "2");
        db.close().await;
        // Another authority, the bundled roots, another name.
        for refused_profile in [
            through(TlsMode::VerifyFull, Some(&other), &name),
            through(TlsMode::VerifyFull, None, &name),
            through(
                TlsMode::VerifyFull,
                Some(&authority),
                "some-other-name.test",
            ),
        ] {
            assert_eq!(
                Database::connect(&refused_profile, Some(port))
                    .await
                    .err()
                    .expect("a refused certificate")
                    .to_string(),
                "database_certificate_rejected"
            );
        }
        // Plain Oracle Net at an endpoint that speaks TLS only.
        assert_eq!(
            Database::connect(&through(TlsMode::Disable, None, &name), Some(port))
                .await
                .err()
                .expect("no answer to a plain connection")
                .to_string(),
            "database_network_error"
        );
        println!(
            "TLS {}: required, verified, refused three ways",
            if hand_over {
                "with a second handshake after the listener handed over"
            } else {
                "with one handshake"
            }
        );
    }
}

/// A listener that redirects every connection to the listener of the test,
/// the way the SCAN listener of a cluster sends a client to a node.
async fn redirecting_listener(target: (String, u16), service: String) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let target = target.clone();
            let service = service.clone();
            tokio::spawn(async move {
                // The connect packet, and the packet with its data when the
                // data did not fit into it.
                for _ in 0..2 {
                    let mut header = [0u8; 8];
                    if stream.read_exact(&mut header).await.is_err() {
                        return;
                    }
                    let length = usize::from(u16::from_be_bytes([header[0], header[1]]));
                    let mut rest = vec![0u8; length.saturating_sub(8)];
                    if stream.read_exact(&mut rest).await.is_err() {
                        return;
                    }
                    let data = rest.get(16..18).map_or(0, |bytes| {
                        usize::from(u16::from_be_bytes([bytes[0], bytes[1]]))
                    });
                    if header[4] != 1 || data <= 230 {
                        break;
                    }
                }
                let text = format!(
                    "(ADDRESS=(PROTOCOL=tcp)(HOST={})(PORT={}))\0(DESCRIPTION=(ADDRESS=(PROTOCOL=tcp)(HOST={})(PORT={}))(CONNECT_DATA=(SERVICE_NAME={service})(CID=(PROGRAM=BazaStudio)(HOST=BazaStudio)(USER=BazaStudio))))",
                    target.0, target.1, target.0, target.1
                );
                let mut answer = vec![0u8, 10, 0, 0, 5, 0, 0, 0];
                answer.extend_from_slice(&u16::try_from(text.len()).unwrap().to_be_bytes());
                answer.extend_from_slice(&u16::try_from(10 + text.len()).unwrap().to_be_bytes());
                answer.extend_from_slice(&[0, 0, 6, 0, 0, 0, 0, 0]);
                answer.extend_from_slice(text.as_bytes());
                let _ = stream.write_all(&answer).await;
                let mut rest = [0u8; 64];
                let _ = stream.read(&mut rest).await;
            });
        }
    });
    port
}

/// A redirect is followed by the driver itself, so that the connection it
/// leads to is like any other: it can be stopped, and it would be encrypted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Oracle server described by BAZA_DB_TEST_* variables"]
async fn a_redirect_of_the_listener_is_followed() {
    let good = profile();
    let port = redirecting_listener((good.host.clone(), good.port), good.database.clone()).await;
    let redirected = ConnectionProfile {
        host: "127.0.0.1".into(),
        port,
        ..good.clone()
    };
    let db = Database::connect(&redirected, None).await.unwrap();
    assert_eq!(value(&db, "SELECT USER FROM dual").await, schema());
    assert_eq!(
        value(&db, "SELECT COUNT(*) FROM all_objects WHERE ROWNUM <= 3000").await,
        "3000"
    );
    // A statement on the redirected connection is stopped on the server.
    let token = CancellationToken::new();
    let worker = db.clone();
    let cancel = token.clone();
    let task = tokio::spawn(async move {
        worker
            .query(
                "SELECT /* baza_live_slow */ COUNT(*) FROM all_objects a, all_objects b, all_objects c WHERE a.object_name || b.object_name || c.object_name = 'x'",
                cancel,
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let watcher = admin().await;
    if let Some(watcher) = &watcher {
        assert_eq!(running(watcher, "baza_live_slow").await, 1);
    }
    token.cancel();
    assert_eq!(
        task.await.unwrap().unwrap_err().to_string(),
        "query_cancelled"
    );
    if let Some(watcher) = &watcher {
        assert!(gone(watcher, "baza_live_slow", 5).await);
        watcher.close().await;
    }
    assert_eq!(value(&db, "SELECT 1 FROM dual").await, "1");
    db.close().await;
    // A wrong password is still a wrong password behind a redirect.
    let wrong = ConnectionProfile {
        password: "not the password".into(),
        ..redirected.clone()
    };
    assert_eq!(refused(&wrong).await, "database_login_rejected");
    // Through an SSH tunnel there is one address only: the redirect is
    // named, not followed past the tunnel.
    assert_eq!(
        Database::connect(&good, Some(port))
            .await
            .err()
            .expect("a redirect behind a tunnel")
            .to_string(),
        "oracle_redirect_through_tunnel"
    );
}

/// A statement that waits for the row lock of another session. Whether the
/// server takes the break while it waits or only afterwards, the person is
/// free at once and the change is never made.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Oracle server described by BAZA_DB_TEST_* variables"]
async fn a_statement_that_waits_for_a_lock_is_given_up() {
    let db = open().await;
    drop_objects(&db).await;
    run(
        &db,
        "CREATE TABLE baza_live_state (id NUMBER(10) PRIMARY KEY, note VARCHAR2(100), changed DATE);
         INSERT INTO baza_live_state (id, note) VALUES (1, 'first')",
    )
    .await;
    // One run is one transaction: this one holds the lock of the row for
    // six seconds and then rolls its change back.
    let holder = db.clone();
    let holding = tokio::spawn(async move {
        holder
            .query(
                "UPDATE baza_live_state SET note = 'held' WHERE id = 1;
                 BEGIN DBMS_SESSION.SLEEP(6); END;
                 ROLLBACK",
                CancellationToken::new(),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let token = CancellationToken::new();
    let worker = db.clone();
    let cancel = token.clone();
    let waiting = tokio::spawn(async move {
        worker
            .query(
                "UPDATE /* baza_live_blocked */ baza_live_state SET note = 'second' WHERE id = 1",
                cancel,
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let watcher = admin().await;
    let blocked = |watcher: &Database| {
        let statement = format!(
            "SELECT COUNT(*) FROM v$session WHERE username = '{}' AND blocking_session IS NOT NULL",
            schema()
        );
        let watcher = watcher.clone();
        async move { value(&watcher, &statement).await }
    };
    if let Some(watcher) = &watcher {
        assert_eq!(blocked(watcher).await, "1", "the second update waits");
    }
    let asked = Instant::now();
    token.cancel();
    assert_eq!(
        waiting.await.unwrap().unwrap_err().to_string(),
        "query_cancelled"
    );
    assert!(asked.elapsed() < Duration::from_secs(1));
    if let Some(watcher) = &watcher {
        let mut left = blocked(watcher).await;
        let mut waited = 0;
        while left != "0" && waited < 40 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            waited += 1;
            left = blocked(watcher).await;
        }
        println!(
            "a lock wait: given up at once, gone from the server after {} ms",
            asked.elapsed().as_millis()
        );
        assert_eq!(left, "0");
        watcher.close().await;
    }
    holding.await.unwrap().unwrap();
    // Neither change was made.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        value(&db, "SELECT note FROM baza_live_state WHERE id = 1").await,
        "first"
    );
    drop_objects(&db).await;
    db.close().await;
}
