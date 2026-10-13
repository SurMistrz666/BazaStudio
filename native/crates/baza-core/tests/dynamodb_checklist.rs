//! The driver checklist for Amazon DynamoDB and what speaks its API, through
//! the same `Database` the application uses.
//!
//! It is the document-store form of `driver_checklist.rs`, step for step: a
//! table is a container, an item a row, an attribute a column. Where the
//! SQL checklist uses the table designer and the query builder, which this
//! engine does not have, the step says what is done instead.
//!
//! ```text
//! BAZA_DB_TEST_PORT=38000 BAZA_DB_TEST_USER=bazaqa BAZA_DB_TEST_PASSWORD=any \
//! cargo test -p baza-core --test dynamodb_checklist -- --ignored --nocapture
//! ```
//!
//! Required: `BAZA_DB_TEST_PORT`, `BAZA_DB_TEST_USER` (an access key ID) and
//! `BAZA_DB_TEST_PASSWORD` (its secret key). Optional: `BAZA_DB_TEST_HOST`
//! (default `127.0.0.1`), `BAZA_DB_TEST_PRODUCT` (default `dynamodb-local`),
//! `BAZA_DB_TEST_DATABASE` (the region, default `us-east-1`),
//! `BAZA_DB_TEST_TLS` (`disable`, the default, `require` or `verify_full`)
//! and `BAZA_DB_TEST_OPTIONS` (`key=value` pairs separated by `;`). The
//! tests are written for a server of one's own (DynamoDB Local): they
//! create tables named `bazaqa_…`, on demand, and delete them at the end.
//!
//! `dynamodb_through_an_ssh_tunnel` also needs `BAZA_SSH_TEST_PORT`,
//! `BAZA_SSH_TEST_USER` and `BAZA_SSH_TEST_KEY`. `tls_is_encrypted_and_verified`
//! puts a TLS endpoint of its own in front of the plain one, with a
//! certificate for the name `localhost` (`BAZA_DB_TEST_TLS_CERT`,
//! `BAZA_DB_TEST_TLS_KEY`) and the CA that signed it (`BAZA_DB_TEST_TLS_CA`).
use baza_core::{
    builder::{Filter, FilterOp},
    database::Database,
    drivers::{PageRequest, RowChange, dynamodb},
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

fn profile() -> ConnectionProfile {
    let mut profile = ConnectionProfile {
        name: "Real DynamoDB".into(),
        engine: dynamodb::ENGINE,
        product: variable("BAZA_DB_TEST_PRODUCT").unwrap_or_else(|| "dynamodb-local".into()),
        host: variable("BAZA_DB_TEST_HOST").unwrap_or_else(|| "127.0.0.1".into()),
        port: required("BAZA_DB_TEST_PORT").parse().unwrap(),
        database: variable("BAZA_DB_TEST_DATABASE").unwrap_or_else(|| "us-east-1".into()),
        username: required("BAZA_DB_TEST_USER"),
        password: required("BAZA_DB_TEST_PASSWORD"),
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

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}

fn text(value: &str) -> Cell {
    Cell::Text(value.into())
}

fn number(value: &str) -> Cell {
    Cell::Decimal(value.into())
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
        .unwrap_or_else(|| panic!("no column {name} in {:?}", result.columns))
}

fn column(result: &QueryResult, name: &str) -> Vec<String> {
    let index = at(result, name);
    result.rows.iter().map(|row| row[index].display()).collect()
}

fn ids(result: &QueryResult) -> Vec<i64> {
    column(result, "id")
        .iter()
        .map(|id| id.parse().unwrap())
        .collect()
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

fn code(result: anyhow::Result<()>) -> String {
    result
        .err()
        .map(|error| error.to_string())
        .unwrap_or_default()
}

/// A table with a number key `id`, billed on demand, made with the
/// operation of the API written into the editor.
async fn create(db: &Database, name: &str, keys: &[(&str, &str, &str)]) {
    let definitions = keys
        .iter()
        .map(|(attribute, kind, _)| {
            format!("{{\"AttributeName\": \"{attribute}\", \"AttributeType\": \"{kind}\"}}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let schema = keys
        .iter()
        .map(|(attribute, _, role)| {
            format!("{{\"AttributeName\": \"{attribute}\", \"KeyType\": \"{role}\"}}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let made = run(
        db,
        &format!(
            "CreateTable {{\"TableName\": \"{name}\", \"AttributeDefinitions\": [{definitions}], \"KeySchema\": [{schema}], \"BillingMode\": \"PAY_PER_REQUEST\"}}"
        ),
    )
    .await;
    assert_eq!(made.columns[0].name, "TableDescription");
}

/// Delete the tables a test made, whatever happened in it.
async fn dropped(db: &Database, names: &[String]) {
    for name in names {
        let _ = db
            .query(
                &format!("DeleteTable {{\"TableName\": \"{name}\"}}"),
                CancellationToken::new(),
            )
            .await;
    }
}

/// The steps of `driver_checklist.rs` for a store of items.
async fn checklist(db: &Database, profile: &ConnectionProfile, name: &str) {
    let dialect = *db.dialect();
    let label = profile.product_label();
    let caps = db.capabilities();

    // 1. The table designer. CAPABILITY-LIMITED: a DynamoDB table is a key
    // schema and a billing mode, not a list of typed columns, so the
    // designer is off; the table is made with the CreateTable operation
    // written into the editor.
    assert!(!caps.table_designer, "{label}: the designer is off");
    create(db, name, &[("id", "N", "HASH")]).await;

    // 2. The side panel: the table is listed, once, as a table; the
    // overview of all tables is a view next to them.
    let tables = db.tables().await.unwrap();
    let listed: Vec<&TableRef> = tables.iter().filter(|found| found.name == name).collect();
    assert_eq!(listed.len(), 1, "{label}: listed once");
    let table = listed[0].clone();
    assert_eq!((table.schema.as_str(), table.kind.as_str()), ("", "table"));
    assert!(
        tables
            .iter()
            .any(|found| found.name == "table overview" && found.kind == "view")
    );

    // 3. Its columns. An empty table has its key and nothing else: the
    // other columns are the attributes its items turn out to have.
    let empty = db.columns(&table).await.unwrap();
    assert_eq!(empty.len(), 1);
    assert_eq!(
        (empty[0].name.as_str(), empty[0].data_type.as_str()),
        ("id", "number · partition key")
    );
    assert!(empty[0].primary_key && !empty[0].nullable, "{label}: key");
    let nothing = page(db, &PageRequest::first(table.clone())).await;
    assert_eq!((nothing.rows.len(), nothing.columns.len()), (0, 1));

    // 4. Import: 120 items at once (two transactions of the service), then
    // a batch with a key that exists, which must leave nothing behind.
    // PutItem would have overwritten item 5 silently.
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
    // CONTRACT GAP: the facade checks the columns of an import against the
    // columns of the container before the driver is asked, and an empty
    // table shows its key only. So a first item is put with a JSON
    // document in the editor, and the table has its attributes from then on.
    assert_eq!(
        db.import_rows(&table, &names, &rows, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string(),
        "import_column_unknown"
    );
    let seeded = run(
        db,
        &format!(
            "INSERT INTO \"{name}\" VALUE {{\"id\": 0, \"label\": \"seed\", \"amount\": 0.5, \"kind\": \"seed\"}}"
        ),
    )
    .await;
    assert_eq!(seeded.affected, 1);
    // The same key again is refused, as PartiQL's own INSERT refuses it.
    assert_eq!(
        failure(db, &format!("INSERT INTO \"{name}\" VALUE {{\"id\": 0}}")).await,
        "dynamodb_item_exists"
    );
    // A refresh of the tree is when a table is looked at anew.
    db.tables().await.unwrap();
    let learned = db.columns(&table).await.unwrap();
    assert_eq!(
        learned
            .iter()
            .map(|column| (column.name.as_str(), column.data_type.as_str()))
            .collect::<Vec<_>>(),
        [
            ("id", "number · partition key"),
            ("amount", "number"),
            ("kind", "string"),
            ("label", "string"),
        ],
        "{label}: the attributes that were seen"
    );
    let imported = db
        .import_rows(&table, &names, &rows, CancellationToken::new())
        .await
        .unwrap();
    db.apply(&RowChange::DeleteRow {
        table: table.clone(),
        key: vec![("id".into(), number("0"))],
    })
    .await
    .unwrap();
    assert_eq!(imported.affected, 120, "{label}: imported items");
    let columns = db.columns(&table).await.unwrap();
    assert!(
        columns[1..]
            .iter()
            .all(|column| !column.primary_key && column.nullable)
    );
    let clash = vec![
        vec![number("500"), text("new"), Cell::Null, Cell::Null],
        vec![number("5"), text("duplicate key"), Cell::Null, Cell::Null],
    ];
    assert_eq!(
        db.import_rows(&table, &names, &clash, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string(),
        "dynamodb_import_key_exists",
        "{label}: a key that exists fails the import"
    );
    let twice = vec![clash[0].clone(), clash[0].clone()];
    assert_eq!(
        db.import_rows(&table, &names, &twice, CancellationToken::new())
            .await
            .unwrap_err()
            .to_string(),
        "dynamodb_import_duplicate"
    );

    // 5. Pages. There is no offset and no order of the table as a whole: a
    // page without an order is a Scan read on from the last key of the page
    // before, and an order is made by the driver on what one page may read.
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
        "{label}: every item once; the failed imports added nothing"
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
    let mut jump = PageRequest::first(table.clone());
    jump.limit = 40;
    jump.offset = 80;
    assert_eq!(
        ids(&page(db, &jump).await),
        seen[80..],
        "{label}: a page reached by reading"
    );
    // The page the grid opens with: 500 items, the same the columns were
    // learned from.
    let opened = page(db, &PageRequest::first(table.clone())).await;
    assert_eq!(opened.rows.len(), 120);
    // What was written is what comes back, character and digit for digit.
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
            "{label}: text of item {index}"
        );
        assert!(
            same_number(&all.rows[index][amount_at], &row[2]),
            "{label}: amount of item {index}: {:?}",
            all.rows[index][amount_at]
        );
    }
    // The service keeps the value of a number, not its trailing zeros.
    assert_eq!(all.rows[0][amount_at], number("1.5"));
    assert_eq!(
        all.columns[at(&all, "id")].data_type,
        "number · partition key"
    );
    let large = number("12345678901234567890.12");

    // 6. The "contains" filter. DynamoDB cannot search: the items are read
    // by a Scan and checked by the driver, letter case ignored and numbers
    // as text; a value of the partition key is a Query first.
    for (column, value, expected) in [
        ("label", "gęślą", vec![7]),
        ("label", "100%", vec![9]),
        ("label", "_under_", vec![9]),
        ("label", "\\ back", vec![8]),
        ("label", "it's", vec![8]),
        ("label", "ROW 12", vec![12, 120]),
        ("id", "119", vec![119]),
        ("label", "no such text", vec![]),
        // No item has the key 500: the ids are searched as text instead.
        ("id", "500", vec![]),
        (
            "amount",
            "7.5",
            vec![7, 17, 27, 37, 47, 57, 67, 77, 87, 97, 107, 117],
        ),
        ("kind", "ND 3", (1..=30).map(|n| n * 4 - 1).collect()),
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
    // A key that exists is a Query of one partition: the statement says so.
    let exact = PageRequest {
        filters: vec![contains("id", "12")],
        ..PageRequest::first(table.clone())
    };
    assert!(
        db.page_text(&exact)
            .unwrap()
            .starts_with(&format!("SELECT * FROM \"{name}\" WHERE \"id\" = 12\n")),
        "{}",
        db.page_text(&exact).unwrap()
    );
    assert_eq!(
        ids(&page(db, &exact).await),
        [12],
        "{label}: the exact key first"
    );

    // 7. One attribute, with the conflict check: a ConditionExpression.
    assert!(
        caps.edit_cells && caps.insert_rows && caps.delete_rows,
        "{label}: grid changes"
    );
    let key = |id: i64| vec![("id".to_owned(), number(&id.to_string()))];
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
            "UPDATE \"{name}\" SET \"label\" = 'Kopia D:\\zrzut\\ ''x''' WHERE \"id\" = 20 AND \"label\" = 'row 20'"
        )
    );
    db.apply(&edit).await.unwrap();
    assert_eq!(
        code(db.apply(&edit).await),
        CONFLICT,
        "{label}: the same edit again is a conflict"
    );
    // A missing attribute is a value of its own on both sides of an edit.
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
    // A value typed where there was none takes the type the attribute has
    // in other items: text for a number is refused, digits become a number.
    let typed = |new: &str| RowChange::UpdateCell {
        table: table.clone(),
        column: "amount".into(),
        old: Cell::Null,
        new: text(new),
        key: key(21),
    };
    assert_eq!(
        code(db.apply(&typed("a lot")).await),
        "dynamodb_number_invalid"
    );
    db.apply(&typed("21.75")).await.unwrap();
    // A change of an item that is gone is a conflict, not a new item.
    assert_eq!(
        code(
            db.apply(&RowChange::UpdateCell {
                table: table.clone(),
                column: "label".into(),
                old: Cell::Null,
                new: text("ghost"),
                key: key(999),
            })
            .await
        ),
        CONFLICT
    );
    assert_eq!(
        code(
            db.apply(&RowChange::UpdateCell {
                table: table.clone(),
                column: "id".into(),
                old: number("30"),
                new: number("31"),
                key: key(30),
            })
            .await
        ),
        "dynamodb_key_read_only"
    );

    // 8. A new item and a removed one. CAPABILITY-LIMITED: there is no
    // item that takes only defaults: the primary key has to be given.
    let added = RowChange::InsertRow {
        table: table.clone(),
        values: vec![("id".into(), text("200")), ("label".into(), text("added"))],
    };
    assert_eq!(
        db.change_text(&added).unwrap(),
        format!("INSERT INTO \"{name}\" VALUE {{'id': 200, 'label': 'added'}}")
    );
    db.apply(&added).await.unwrap();
    assert_eq!(code(db.apply(&added).await), "dynamodb_item_exists");
    assert_eq!(
        code(
            db.apply(&RowChange::InsertRow {
                table: table.clone(),
                values: vec![],
            })
            .await
        ),
        "dynamodb_key_required",
        "{label}: an item needs its key"
    );
    let removed = RowChange::DeleteRow {
        table: table.clone(),
        key: key(1),
    };
    db.apply(&removed).await.unwrap();
    assert_eq!(
        code(db.apply(&removed).await),
        CONFLICT,
        "{label}: an item that is gone cannot be removed again"
    );
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
        "{label}: 120 items, one gone, one new"
    );
    let id_at = at(&check, "id");
    check
        .rows
        .retain(|row| ["1", "11", "20", "21", "200"].contains(&row[id_at].display().as_str()));
    let expected = [
        (11, "row 11", large),
        (20, "Kopia D:\\zrzut\\ 'x'", number("20.5")),
        (21, "row 21", number("21.75")),
        (200, "added", Cell::Null),
    ];
    assert_eq!(check.rows.len(), expected.len());
    for (row, (id, label_text, amount)) in check.rows.iter().zip(&expected) {
        assert_eq!(row[id_at], number(&id.to_string()));
        assert_eq!(row[label_at], text(label_text), "{label}: label of {id}");
        assert_eq!(&row[amount_at], amount, "{label}: amount of {id}");
    }

    // 9. The query builder. CAPABILITY-LIMITED: it is off (PartiQL has no
    // joins and no GROUP BY). The items of one kind are asked for in the
    // editor with PartiQL, and counted here.
    assert!(!caps.query_builder, "{label}: the builder is off");
    let counted = run(
        db,
        &format!("SELECT \"kind\", \"id\" FROM \"{name}\" WHERE \"kind\" = 'kind 2'"),
    )
    .await;
    // Ids 2, 6, … 118: thirty items; the one added by the grid has no kind.
    assert_eq!(counted.rows.len(), 30, "{label}: items of one kind");
    assert_eq!(
        counted
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["kind", "id"],
        "{label}: the columns in the order they were asked for"
    );

    // 10. The "Monitor" section, and the overview of the tables.
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
    assert!(stat("Items").is_some_and(|count| count.parse::<u64>().is_ok()));
    assert!(
        stat("table list").is_some_and(|list| list.contains(name)),
        "{:?}",
        stat("table list")
    );
    let overview = page(
        db,
        &PageRequest {
            filters: vec![contains("table", name)],
            ..PageRequest::first(TableRef {
                schema: String::new(),
                name: "table overview".into(),
                kind: "view".into(),
            })
        },
    )
    .await;
    assert_eq!(overview.rows.len(), 1);
    assert_eq!(column(&overview, "status"), ["ACTIVE"]);
    assert_eq!(column(&overview, "billing"), ["on-demand"]);
    assert_eq!(column(&overview, "partition key"), ["id (number)"]);
    assert_eq!(column(&overview, "sort key"), ["NULL"]);

    // 11. Stopping: a call that is dropped closes its connection, and no
    // further call is made.
    let script = (1000..1400)
        .map(|id| format!("INSERT INTO \"{name}\" VALUE {{'id': {id}, 'label': 'slow'}}"))
        .collect::<Vec<_>>()
        .join(";\n");
    let token = CancellationToken::new();
    let (worker, cancel) = (db.clone(), token.clone());
    let task = tokio::spawn(async move { worker.query(&script, cancel).await });
    tokio::time::sleep(Duration::from_millis(60)).await;
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
    let written = page(
        db,
        &PageRequest {
            filters: vec![contains("label", "slow")],
            ..PageRequest::first(table.clone())
        },
    )
    .await
    .rows
    .len();
    assert!(
        written < 400,
        "{label}: the script went on after it was stopped"
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        db.query("SELECT 1", cancelled)
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
        "{label}: designer off (CreateTable in the editor), listing with the overview, columns from the key and the items, import (120 items in two transactions, existing and repeated keys refused), forward paging and reached pages, order by the driver, 11 filters with the exact key first, attribute edit with conflict check, insert, delete, builder off (PartiQL in the editor), monitor ({} columns), cancel in {} ms ({written} of 400 statements had been sent)",
        stats.columns.len(),
        stopped_in.as_millis()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a DynamoDB endpoint described by BAZA_DB_TEST_* variables"]
async fn server_passes_the_driver_checklist() {
    let profile = profile();
    let db = Database::connect(&profile, None).await.unwrap();
    let name = unique("bazaqa_checklist");
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
#[ignore = "needs a real SSH server and a DynamoDB endpoint: BAZA_SSH_TEST_* and BAZA_DB_TEST_*"]
async fn dynamodb_through_an_ssh_tunnel() {
    let mut profile = profile();
    let ssh = ssh_profile();
    profile.ssh = Some(ssh.clone());
    // What the application does for a profile with an SSH tab: a tunnel to
    // the Host and Port of the profile, and the driver dials its loopback
    // end while it signs and addresses the endpoint by its own name.
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
    let name = unique("bazaqa_ssh");
    let outcome = {
        let (db, name) = (db.clone(), name.clone());
        tokio::spawn(async move {
            create(&db, &name, &[("id", "N", "HASH")]).await;
            let script = (1..=60)
                .map(|n| {
                    format!(
                        "INSERT INTO \"{name}\" VALUE {{\"id\": {n}, \"label\": \"through the tunnel {n}\"}}"
                    )
                })
                .collect::<Vec<_>>()
                .join(";\n");
            run(&db, &script).await;
            let table = db
                .tables()
                .await
                .unwrap()
                .into_iter()
                .find(|found| found.name == name)
                .expect("the table is listed through the tunnel");
            db.columns(&table).await.unwrap();
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
                key: vec![("id".into(), number("33"))],
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
    manager.shutdown().await;
    println!(
        "{} over 127.0.0.1:{local}: table made, 60 items put, listing, two pages read on, filter, attribute edit with conflict check, monitor ({columns} columns); tunnel sent {} B, received {} B",
        profile.product_label(),
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

/// The three TLS modes of a profile against an endpoint that speaks HTTPS,
/// and what a connection that cannot be made is called.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a DynamoDB endpoint and BAZA_DB_TEST_TLS_CERT, _KEY and _CA"]
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
    assert_eq!(
        attempt(with("localhost", TlsMode::Require, None)).await,
        "1"
    );
    assert_eq!(
        attempt(with("127.0.0.1", TlsMode::Require, None)).await,
        "1"
    );
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
    // The same through the endpoint URL of the profile.
    let mut by_url = plain.clone();
    by_url.product = String::new();
    by_url.tls = TlsMode::VerifyFull;
    by_url.ca_file = Some(ca.clone());
    by_url.set_option("endpoint", &format!("https://localhost:{port}"));
    assert_eq!(attempt(by_url.clone()).await, "1");
    // The URL and the SSL/TLS tab have to agree.
    by_url.tls = TlsMode::Disable;
    assert_eq!(attempt(by_url).await, "dynamodb_endpoint_tls_mismatch");
    // The whole work of a session through TLS.
    let db = Database::connect(
        &with("localhost", TlsMode::VerifyFull, Some(ca.clone())),
        None,
    )
    .await
    .unwrap();
    assert!(
        db.tables()
            .await
            .unwrap()
            .iter()
            .any(|found| found.name == "table overview")
    );
    assert_eq!(db.server_stats().await.unwrap().rows.len(), 1);
    db.close().await;
    // TLS asked of an endpoint that has none: said in words.
    let mut encrypted = plain.clone();
    encrypted.tls = TlsMode::Require;
    assert_eq!(attempt(encrypted).await, "database_tls_unsupported");
    // An endpoint that only speaks TLS, asked without it.
    let refused = attempt(with("localhost", TlsMode::Disable, None)).await;
    assert!(
        matches!(
            refused.as_str(),
            "database_network_error" | "dynamodb_endpoint_not_dynamodb"
        ),
        "{refused}"
    );
    // A port nobody listens on, and something that is not DynamoDB.
    let mut closed = plain.clone();
    closed.port = 1;
    assert_eq!(attempt(closed).await, "database_network_error");
    println!(
        "{}: HTTPS without verification, with the bundled roots (rejected), with the CA file (accepted for its name only), by the endpoint URL, a session through TLS, TLS on a plain port and a plain connection to a TLS port explained, a closed port",
        plain.product_label()
    );
}

/// Every DynamoDB type, a key of two attributes, and the editor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a DynamoDB endpoint described by BAZA_DB_TEST_* variables"]
async fn every_type_a_key_of_two_attributes_and_the_editor() {
    let profile = profile();
    let db = Database::connect(&profile, None).await.unwrap();
    let name = unique("bazaqa_music");
    let made = [name.clone()];
    let worker = db.clone();
    let outcome = tokio::spawn(async move {
        let db = &worker;
        create(db, &name, &[("artist", "S", "HASH"), ("year", "N", "RANGE")]).await;
        // One item with a value of every type, put with the operation of
        // the API, and five more of one artist.
        run(
            db,
            &format!(
                "PutItem {{\"TableName\": \"{name}\", \"Item\": {{\"artist\": {{\"S\": \"Queen\"}}, \"year\": {{\"N\": \"1975\"}}, \"title\": {{\"S\": \"A Night at the Opera; 'x' \\\"y\\\"\"}}, \"price\": {{\"N\": \"12345678901234567890.123456789012345678\"}}, \"cover\": {{\"B\": \"AP8Q\"}}, \"live\": {{\"BOOL\": false}}, \"nothing\": {{\"NULL\": true}}, \"tracks\": {{\"L\": [{{\"S\": \"Bohemian Rhapsody\"}}, {{\"N\": \"355\"}}, {{\"L\": []}}]}}, \"label\": {{\"M\": {{\"name\": {{\"S\": \"EMI\"}}, \"city\": {{\"S\": \"Łódź\"}}, \"S\": {{\"N\": \"1\"}}}}}}, \"tags\": {{\"SS\": [\"rock\", \"opera\"]}}, \"charts\": {{\"NS\": [\"1\", \"9.5\"]}}, \"scans\": {{\"BS\": [\"AQI=\"]}}}}}}"
            ),
        )
        .await;
        for year in [1973, 1974, 1977, 1980, 1991] {
            run(
                db,
                &format!(
                    "INSERT INTO \"{name}\" VALUE {{'artist': 'Queen', 'year': {year}, 'title': 'album {year}'}}"
                ),
            )
            .await;
        }
        run(
            db,
            &format!("INSERT INTO \"{name}\" VALUE {{\"artist\": \"Abba\", \"year\": 1976, \"title\": \"Arrival\"}}"),
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
        let kinds: Vec<(&str, &str)> = columns
            .iter()
            .map(|column| (column.name.as_str(), column.data_type.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("artist", "string · partition key"),
                ("year", "number · sort key"),
                ("charts", "number set"),
                ("cover", "binary"),
                ("label", "map"),
                ("live", "boolean"),
                ("nothing", "null"),
                ("price", "number"),
                ("scans", "binary set"),
                ("tags", "string set"),
                ("title", "string"),
                ("tracks", "list"),
            ]
        );
        let json = |value: &str| Cell::Json(value.into());
        // A filter on the partition key is a Query: the statement says so,
        // and the service orders the partition by its sort key.
        let mut queen = PageRequest {
            filters: vec![contains("artist", "Queen")],
            order_by: Some(("year".into(), true)),
            ..PageRequest::first(table.clone())
        };
        assert!(
            db.page_text(&queen)
                .unwrap()
                .starts_with(&format!("SELECT * FROM \"{name}\" WHERE \"artist\" = 'Queen'\n")),
        );
        queen.limit = 2;
        let mut years = Vec::new();
        for index in 0..4u64 {
            queen.offset = index * 2;
            let rows = page(db, &queen).await;
            assert_eq!(rows.rows.len(), [2, 2, 2, 0][index as usize]);
            years.extend(column(&rows, "year"));
        }
        assert_eq!(years, ["1991", "1980", "1977", "1975", "1974", "1973"]);
        // A part of a key is not a key: the items are searched instead.
        let part = page(
            db,
            &PageRequest {
                filters: vec![contains("artist", "que")],
                order_by: Some(("year".into(), false)),
                ..PageRequest::first(table.clone())
            },
        )
        .await;
        assert_eq!(part.rows.len(), 6);
        assert_eq!(column(&part, "year")[0], "1973");
        // What each type is shown as, and a value to change it to.
        let cases: Vec<(&str, Cell, Cell)> = vec![
            (
                "title",
                text("A Night at the Opera; 'x' \"y\""),
                text("Noc w operze\n🦀"),
            ),
            (
                "price",
                number("12345678901234567890.123456789012345678"),
                number("-0.000000000000000000000000000001"),
            ),
            ("cover", Cell::Bytes(vec![0, 255, 16]), Cell::Bytes(vec![202, 254])),
            ("live", Cell::Bool(false), Cell::Bool(true)),
            // A value that holds NULL is not a missing one.
            ("nothing", json("null"), json(r#"{"was":"null"}"#)),
            (
                "tracks",
                json(r#"["Bohemian Rhapsody",355,[]]"#),
                json(r#"["Love of My Life",{"B":"AQI="}]"#),
            ),
            (
                "label",
                json(r#"{"S":1,"city":"Łódź","name":"EMI"}"#),
                // A map with one key that is a type name keeps its wrapper.
                json(r#"{"M":{"S":"x"}}"#),
            ),
            ("tags", json(r#"{"SS":["opera","rock"]}"#), json(r#"{"SS":["glam"]}"#)),
            // A set comes back in the order of the service.
            ("charts", json(r#"{"NS":[1,9.5]}"#), json(r#"{"NS":[0.25,2]}"#)),
            ("scans", json(r#"{"BS":["AQI="]}"#), json(r#"{"BS":["AP8=","AQI="]}"#)),
        ];
        let one = |db: &Database, table: &TableRef| {
            let request = PageRequest {
                filters: vec![
                    contains("artist", "Queen"),
                    Filter {
                        column: "year".into(),
                        op: FilterOp::Equal,
                        value: number("1975"),
                    },
                ],
                ..PageRequest::first(table.clone())
            };
            let db = db.clone();
            async move {
                let found = page(&db, &request).await;
                assert_eq!(found.rows.len(), 1);
                found
            }
        };
        let shown = one(db, &table).await;
        for (attribute, expected, _) in &cases {
            assert_eq!(&shown.rows[0][at(&shown, attribute)], expected, "{attribute} as shown");
        }
        let key = vec![("artist".to_owned(), text("Queen")), ("year".to_owned(), number("1975"))];
        for (attribute, old, new) in &cases {
            let change = RowChange::UpdateCell {
                table: table.clone(),
                column: (*attribute).into(),
                old: old.clone(),
                new: new.clone(),
                key: key.clone(),
            };
            db.apply(&change)
                .await
                .unwrap_or_else(|error| panic!("{attribute}: {error}"));
            assert_eq!(code(db.apply(&change).await), CONFLICT, "{attribute} again");
        }
        let changed = one(db, &table).await;
        for (attribute, _, expected) in &cases {
            assert_eq!(
                &changed.rows[0][at(&changed, attribute)],
                expected,
                "{attribute} after the change"
            );
        }
        // An attribute is taken away, and put back from text: it takes the
        // type other items hold in it.
        db.apply(&RowChange::UpdateCell {
            table: table.clone(),
            column: "live".into(),
            old: Cell::Bool(true),
            new: Cell::Null,
            key: key.clone(),
        })
        .await
        .unwrap();
        let gone = one(db, &table).await;
        assert_eq!(gone.rows[0][at(&gone, "live")], Cell::Null);
        db.apply(&RowChange::UpdateCell {
            table: table.clone(),
            column: "live".into(),
            old: Cell::Null,
            new: text("true"),
            key: key.clone(),
        })
        .await
        .unwrap();
        let back = one(db, &table).await;
        assert_eq!(back.rows[0][at(&back, "live")], Cell::Bool(true));
        // What is not a value of its type is refused before it is sent.
        for (attribute, bad, expected) in [
            ("tags", r#"{"SS":[]}"#, "dynamodb_set_empty"),
            ("tags", r#"{"SS":["a","a"]}"#, "dynamodb_set_duplicate"),
            ("charts", r#"{"NS":["x"]}"#, "dynamodb_number_invalid"),
            ("label", "{", "json_invalid"),
        ] {
            let change = RowChange::UpdateCell {
                table: table.clone(),
                column: attribute.into(),
                old: changed.rows[0][at(&changed, attribute)].clone(),
                new: json(bad),
                key: key.clone(),
            };
            assert_eq!(code(db.apply(&change).await), expected, "{attribute} = {bad}");
        }
        // A new item from the fields of the dialog, which are all text.
        let fields = |year: &str| RowChange::InsertRow {
            table: table.clone(),
            values: vec![
                ("artist".into(), text("Abba")),
                ("year".into(), text(year)),
                ("title".into(), text("1981")),
                ("live".into(), text("false")),
                ("tags".into(), text(r#"{"SS":["pop"]}"#)),
                ("price".into(), text("")),
            ],
        };
        db.apply(&fields("1981")).await.unwrap();
        assert_eq!(code(db.apply(&fields("1981")).await), "dynamodb_item_exists");
        assert_eq!(code(db.apply(&fields("next")).await), "dynamodb_number_invalid");
        let abba = run(
            db,
            &format!("SELECT * FROM \"{name}\" WHERE \"artist\" = 'Abba' AND \"year\" = 1981"),
        )
        .await;
        let value = |column: &str| abba.rows[0][at(&abba, column)].clone();
        assert_eq!(value("title"), text("1981"));
        assert_eq!(value("live"), Cell::Bool(false));
        assert_eq!(value("tags"), json(r#"{"SS":["pop"]}"#));
        assert_eq!(value("price"), Cell::Null);
        // A plain SELECT * of one table is what the grid edits.
        let tables = db.tables().await.unwrap();
        let statement = format!("SELECT * FROM \"{name}\" WHERE \"artist\" = 'Queen'");
        let inferred = sql::editable_table(db.dialect(), &statement, &tables).expect("editable");
        let columns = db.columns(&inferred).await.unwrap();
        let result = run(db, &statement).await;
        assert_eq!(result.rows.len(), 6);
        assert!(sql::editable_result_table(Some(inferred), &columns, &result).is_some());
        // PartiQL writes, and what the service refuses, in words.
        run(
            db,
            &format!(
                "UPDATE \"{name}\" SET \"title\" = 'Waterloo' WHERE \"artist\" = 'Abba' AND \"year\" = 1981;\n\
                 -- a comment the service never sees\n\
                 DELETE FROM \"{name}\" WHERE \"artist\" = 'Abba' AND \"year\" = 1976"
            ),
        )
        .await;
        let left = run(db, &format!("SELECT \"year\", \"title\" FROM \"{name}\" WHERE \"artist\" = 'Abba'")).await;
        assert_eq!(left.rows, [vec![number("1981"), text("Waterloo")]]);
        for (statement, expected) in [
            (format!("SELEC * FROM \"{name}\""), "dynamodb_statement_invalid"),
            ("SELECT * FROM \"bazaqa_no_such_table\"".to_owned(), "dynamodb_table_unknown"),
            (
                format!("INSERT INTO \"{name}\" VALUE {{'artist': 'Abba', 'year': 1981}}"),
                "dynamodb_item_exists",
            ),
            (
                format!("INSERT INTO \"{name}\" VALUE {{\"artist\": \"Abba\"}}"),
                "dynamodb_document_invalid",
            ),
            (
                format!("UPDATE \"{name}\" SET \"title\" = 'x' WHERE \"title\" = 'Waterloo'"),
                "dynamodb_statement_needs_key",
            ),
            ("DescribeTable {\"TableName\": \"bazaqa_no_such_table\"}".to_owned(), "dynamodb_table_unknown"),
            ("MakeCoffee {}".to_owned(), "dynamodb_operation_unknown"),
        ] {
            assert_eq!(failure(db, &statement).await, expected, "{statement}");
        }
        // Any operation of the API: a Query by its JSON body.
        let queried = run(
            db,
            &format!(
                "Query {{\"TableName\": \"{name}\", \"KeyConditionExpression\": \"artist = :a AND #y BETWEEN :from AND :to\", \"ExpressionAttributeNames\": {{\"#y\": \"year\"}}, \"ExpressionAttributeValues\": {{\":a\": {{\"S\": \"Queen\"}}, \":from\": {{\"N\": \"1974\"}}, \":to\": {{\"N\": \"1980\"}}}}, \"ProjectionExpression\": \"#y, title, artist\"}}"
            ),
        )
        .await;
        assert_eq!(column(&queried, "year"), ["1974", "1975", "1977", "1980"]);
        assert_eq!(
            queried
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["artist", "year", "title"]
        );
        let described = run(db, &format!("DescribeTable {{\"TableName\": \"{name}\"}}")).await;
        assert_eq!(described.columns[0].name, "Table");
        assert!(described.rows[0][0].display().contains("\"KeySchema\""));
        cases.len()
    })
    .await;
    dropped(&db, &made).await;
    db.close().await;
    match outcome {
        Ok(types) => println!(
            "{}: {types} types shown and changed under a condition; a Query for the partition key, ordered by the sort key by the service and read in pages; a part of a key searched for; an attribute removed and typed back; bad values refused before sending; a new item from text fields; an editable SELECT *; PartiQL writes; 7 refusals in words; Query and DescribeTable by their JSON bodies",
            profile.product_label()
        ),
        Err(failure) => std::panic::resume_unwind(failure.into_panic()),
    }
}

/// The amount one action reads, whatever the size of the table.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a DynamoDB endpoint of one's own; writes 25 MB"]
async fn a_page_reads_a_bounded_amount() {
    let profile = profile();
    let db = Database::connect(&profile, None).await.unwrap();
    let name = unique("bazaqa_large");
    let made = [name.clone()];
    let worker = db.clone();
    let outcome = tokio::spawn(async move {
        let db = &worker;
        create(db, &name, &[("id", "N", "HASH")]).await;
        // 64 items of about 390 kB each: 25 MB, more than a page may read.
        let filler = "x".repeat(390_000);
        for id in 1..=64 {
            run(
                db,
                &format!(
                    "PutItem {{\"TableName\": \"{name}\", \"Item\": {{\"id\": {{\"N\": \"{id}\"}}, \"label\": {{\"S\": \"item {id}\"}}, \"body\": {{\"S\": \"{filler}\"}}}}}}"
                ),
            )
            .await;
        }
        let table = db
            .tables()
            .await
            .unwrap()
            .into_iter()
            .find(|found| found.name == name)
            .unwrap();
        // The table is larger than what one page reads: the page says it
        // is cut, and the next one goes on from where it stopped.
        let mut request = PageRequest::first(table.clone());
        request.limit = 100;
        let first = page(db, &request).await;
        assert!(first.truncated, "{} items", first.rows.len());
        assert!((20..64).contains(&first.rows.len()), "{} items", first.rows.len());
        request.offset = 100;
        let second = page(db, &request).await;
        assert!(!second.truncated);
        let mut seen = ids(&first);
        seen.extend(ids(&second));
        seen.sort_unstable();
        assert_eq!(seen, (1..=64).collect::<Vec<_>>(), "every item once over two pages");
        // A filter that finds nothing in what a page may read says so, and
        // the next page goes on to the end.
        let mut search = PageRequest {
            filters: vec![contains("label", "ITEM 999")],
            ..PageRequest::first(table.clone())
        };
        let started = Instant::now();
        assert_eq!(page_failure(db, &search).await, "dynamodb_scan_limit");
        let scanned = started.elapsed();
        search.offset = 500;
        let rest = page(db, &search).await;
        assert!(rest.rows.is_empty() && !rest.truncated);
        // A text without letters is narrowed by the service: only the
        // items that hold it travel.
        // A filter does not make a Scan read less: on a table this size
        // its first page is cut as well, and the next one brings the rest.
        // A text without letters is narrowed by the service, so that only
        // the items that hold it travel.
        let mut narrowed = PageRequest {
            filters: vec![contains("label", " 4")],
            ..PageRequest::first(table.clone())
        };
        let begun = page(db, &narrowed).await;
        assert!(begun.truncated && !begun.rows.is_empty());
        narrowed.offset = 500;
        let ended = page(db, &narrowed).await;
        assert!(!ended.truncated);
        let mut found = ids(&begun);
        found.extend(ids(&ended));
        found.sort_unstable();
        assert_eq!(found, [4, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49]);
        // An order needs every item: refused beyond what a page reads.
        assert_eq!(
            page_failure(
                db,
                &PageRequest {
                    order_by: Some(("id".into(), false)),
                    ..PageRequest::first(table.clone())
                },
            )
            .await,
            "dynamodb_sort_limit"
        );
        // A key is still found at once.
        let exact = page(
            db,
            &PageRequest {
                filters: vec![contains("id", "37")],
                ..PageRequest::first(table.clone())
            },
        )
        .await;
        assert_eq!(ids(&exact), [37]);
        // PartiQL in the editor follows the continuation of the service
        // and stops at the same amount.
        let listed = run(db, &format!("SELECT \"id\" FROM \"{name}\"")).await;
        assert!(listed.truncated, "{} items", listed.rows.len());
        assert!((20..64).contains(&listed.rows.len()), "{} items", listed.rows.len());
        (first.rows.len(), listed.rows.len(), scanned)
    })
    .await;
    dropped(&db, &made).await;
    db.close().await;
    match outcome {
        Ok((first, listed, scanned)) => println!(
            "{}: of 64 items (25 MB) a page read {first} and said it was cut, the next page the rest; a filter without a match stopped after 16 MB in {} ms and the next page ended it; a filter without letters narrowed by the service; an order refused; a key found at once; PartiQL stopped after {listed} items",
            profile.product_label(),
            scanned.as_millis()
        ),
        Err(failure) => std::panic::resume_unwind(failure.into_panic()),
    }
}
