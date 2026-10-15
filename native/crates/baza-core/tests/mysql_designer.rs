//! The designer's statements for MySQL and MariaDB run on a real server:
//! a table with ENUM and SET columns, indexes, a foreign key, a CHECK, a
//! trigger and the options of its engine is created, read back, changed in
//! every part and read back again; partitions are made, changed and taken
//! away on a table of their own. Ignored by default:
//!
//! ```text
//! BAZA_DB_TEST_ENGINE=mysql BAZA_DB_TEST_PORT=3306 BAZA_DB_TEST_USER=root \
//! BAZA_DB_TEST_PASSWORD=… BAZA_DB_TEST_DATABASE=test \
//! cargo test -p baza-core --test mysql_designer -- --ignored --nocapture
//! ```
//!
//! Only tables created by the run are changed, and they are dropped at the end.
use baza_core::{
    database::Database,
    ddl::{
        DdlFlavor, FieldDraft,
        mysql::{self, *},
    },
    drivers::mysql as engine,
    model::{ConnectionProfile, TableRef, TlsMode},
};
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;

fn variable(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

async fn run(db: &Database, statements: &[String]) {
    for statement in statements {
        db.query(statement, CancellationToken::new())
            .await
            .unwrap_or_else(|error| panic!("{error}: {statement}"));
    }
}

/// The parts of a table and its columns, as the designer reads them.
async fn read_back(db: &Database, table: &TableRef) -> (Vec<FieldDraft>, TableExtras) {
    let columns = db.columns(table).await.unwrap();
    let fields: Vec<FieldDraft> = columns.iter().map(FieldDraft::from_column).collect();
    let queries = mysql::queries(db.dialect(), table).unwrap();
    let mut rows = Vec::new();
    for (index, query) in queries.iter().enumerate() {
        let result = match db.query(query, CancellationToken::new()).await {
            Ok(result) => result,
            // MySQL: no table name in check_constraints.
            Err(_) if index == 2 => db
                .query(
                    &mysql::checks_query_mysql(db.dialect(), table).unwrap(),
                    CancellationToken::new(),
                )
                .await
                .unwrap(),
            Err(error) => panic!("{error}: {query}"),
        };
        rows.push(result.rows);
    }
    let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
    let extras = mysql::read(
        [&rows[0], &rows[1], &rows[2], &rows[3], &rows[4], &rows[5]],
        &names,
    );
    (fields, extras)
}

fn field(name: &str, data_type: &str, length: &str) -> FieldDraft {
    FieldDraft {
        name: name.into(),
        data_type: data_type.into(),
        length: length.into(),
        ..Default::default()
    }
}

async fn designed(db: &Database, schema: &str, parent: &str, child: &str) {
    let dialect = *db.dialect();
    let parent_table = TableRef {
        schema: schema.into(),
        name: parent.into(),
        kind: "table".into(),
    };
    let table = TableRef {
        schema: schema.into(),
        name: child.into(),
        kind: "table".into(),
    };
    let mut key = field("id", "int", "");
    key.primary_key = true;
    key.not_null = true;
    run(
        db,
        &baza_core::ddl::create_table(DdlFlavor::MySql, &dialect, &parent_table, &[key.clone()])
            .unwrap(),
    )
    .await;

    let mut status = field("status", "enum", "");
    status.values = vec!["new".into(), "it's paid".into(), "back\\slash".into()];
    status.not_null = true;
    let mut flags = field("flags", "set", "");
    flags.values = vec!["a".into(), "b".into()];
    let mut auto = key.clone();
    auto.auto_increment = true;
    let fields = vec![
        auto,
        field("customer", "int", ""),
        field("note", "varchar", "100"),
        field("body", "text", ""),
        status,
        flags,
        field("total", "decimal", "10"),
    ];
    let extras = TableExtras {
        indexes: vec![
            IndexDraft {
                name: "by_customer".into(),
                kind: IndexKind::Unique,
                parts: vec![
                    IndexPart {
                        column: "customer".into(),
                        ..Default::default()
                    },
                    IndexPart {
                        column: "note".into(),
                        length: "20".into(),
                        descending: true,
                    },
                ],
                comment: "it's unique".into(),
                original: None,
            },
            IndexDraft {
                name: "search".into(),
                kind: IndexKind::Fulltext,
                parts: vec![IndexPart {
                    column: "body".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        ],
        foreign_keys: vec![ForeignKeyDraft {
            name: "fk_parent".into(),
            columns: vec!["customer".into()],
            ref_table: parent.into(),
            ref_columns: vec!["id".into()],
            on_delete: RefAction::Cascade,
            on_update: RefAction::Restrict,
            ..Default::default()
        }],
        checks: vec![CheckDraft {
            name: "positive".into(),
            expression: "total >= 0".into(),
            original: None,
        }],
        triggers: vec![TriggerDraft {
            name: format!("{child}_stamp"),
            timing: Timing::Before,
            event: Event::Insert,
            body: "BEGIN\n  IF NEW.total IS NULL THEN SET NEW.total = 0; END IF;\nEND".into(),
            original: None,
        }],
        options: TableOptions {
            engine: "InnoDB".into(),
            charset: "utf8mb4".into(),
            collation: "utf8mb4_unicode_ci".into(),
            comment: "designed".into(),
            settings: BTreeMap::from([
                ("ROW_FORMAT".into(), "DYNAMIC".into()),
                ("STATS_PERSISTENT".into(), "1".into()),
            ]),
            ..Default::default()
        },
        partitioning: Partitioning::default(),
    };
    let statements = mysql::create(&dialect, &table, &fields, &extras).unwrap();
    run(db, &statements).await;

    // What was made is what was designed.
    let (read_fields, read) = read_back(db, &table).await;
    assert_eq!(read_fields[4].values, ["new", "it's paid", "back\\slash"]);
    assert_eq!(read_fields[5].values, ["a", "b"]);
    // The foreign key uses `by_customer`, which starts with its column.
    assert_eq!(read.indexes.len(), 2, "{:#?}", read.indexes);
    let by_customer = read
        .indexes
        .iter()
        .find(|i| i.name == "by_customer")
        .unwrap();
    assert_eq!(by_customer.kind, IndexKind::Unique);
    assert_eq!(by_customer.parts[1].length, "20");
    assert_eq!(by_customer.comment, "it's unique");
    assert!(
        read.indexes
            .iter()
            .any(|i| i.name == "search" && i.kind == IndexKind::Fulltext)
    );
    assert_eq!(read.foreign_keys.len(), 1);
    assert_eq!(read.foreign_keys[0].on_delete, RefAction::Cascade);
    assert_eq!(read.checks.len(), 1);
    assert_eq!(read.checks[0].name, "positive");
    assert_eq!(read.triggers.len(), 1);
    assert!(read.triggers[0].body.contains("SET NEW.total = 0"));
    assert_eq!(read.options.engine, "InnoDB");
    assert_eq!(read.options.collation, "utf8mb4_unicode_ci");
    assert_eq!(read.options.comment, "designed");
    assert_eq!(
        read.options.settings.get("ROW_FORMAT").map(String::as_str),
        Some("DYNAMIC")
    );
    // Read back and unchanged: nothing to do.
    assert!(
        mysql::alter(&dialect, &table, &read_fields, &read_fields, &read, &read)
            .unwrap()
            .is_empty()
    );

    // Change every part.
    let mut fields_after = read_fields.clone();
    fields_after[4].values.push("sent".into());
    let mut after = read.clone();
    after.indexes.retain(|i| i.name != "search");
    after
        .indexes
        .iter_mut()
        .find(|i| i.name == "by_customer")
        .unwrap()
        .kind = IndexKind::Index;
    after.indexes.push(IndexDraft {
        name: "by_status".into(),
        kind: IndexKind::Index,
        parts: vec![IndexPart {
            column: "status".into(),
            ..Default::default()
        }],
        ..Default::default()
    });
    after.foreign_keys[0].on_delete = RefAction::SetNull;
    after.checks[0].expression = "total >= 0 AND total < 1000000".into();
    after.checks.push(CheckDraft {
        name: "known_flags".into(),
        expression: "flags <> ''".into(),
        original: None,
    });
    after.triggers[0].timing = Timing::After;
    after.triggers[0].body = "SET @last_order = NEW.id".into();
    after.options.comment = "changed".into();
    after
        .options
        .settings
        .insert("ROW_FORMAT".into(), "COMPACT".into());
    let statements =
        mysql::alter(&dialect, &table, &read_fields, &fields_after, &read, &after).unwrap();
    println!("{}", statements.join(";\n"));
    run(db, &statements).await;

    let (fields_again, again) = read_back(db, &table).await;
    assert_eq!(
        fields_again[4].values,
        ["new", "it's paid", "back\\slash", "sent"]
    );
    assert!(!again.indexes.iter().any(|i| i.name == "search"));
    assert!(again.indexes.iter().any(|i| i.name == "by_status"));
    assert_eq!(
        again
            .indexes
            .iter()
            .find(|i| i.name == "by_customer")
            .unwrap()
            .kind,
        IndexKind::Index
    );
    assert_eq!(again.foreign_keys[0].on_delete, RefAction::SetNull);
    assert_eq!(again.checks.len(), 2);
    assert_eq!(again.triggers[0].timing, Timing::After);
    assert_eq!(again.options.comment, "changed");
    assert_eq!(
        again.options.settings.get("ROW_FORMAT").map(String::as_str),
        Some("COMPACT")
    );
    assert!(
        mysql::alter(
            &dialect,
            &table,
            &fields_again,
            &fields_again,
            &again,
            &again
        )
        .unwrap()
        .is_empty()
    );
}

async fn partitioned(db: &Database, schema: &str, name: &str) {
    let dialect = *db.dialect();
    let table = TableRef {
        schema: schema.into(),
        name: name.into(),
        kind: "table".into(),
    };
    let mut key = field("id", "int", "");
    key.primary_key = true;
    key.not_null = true;
    let fields = vec![key, field("amount", "int", "")];
    let extras = TableExtras {
        partitioning: Partitioning {
            method: PartitionMethod::Range,
            expression: "id".into(),
            count: String::new(),
            subpartitioned: false,
            partitions: vec![
                PartitionDef {
                    name: "low".into(),
                    values: "100".into(),
                    comment: "below a hundred".into(),
                },
                PartitionDef {
                    name: "rest".into(),
                    values: "MAXVALUE".into(),
                    comment: String::new(),
                },
            ],
        },
        ..Default::default()
    };
    run(
        db,
        &mysql::create(&dialect, &table, &fields, &extras).unwrap(),
    )
    .await;
    let (fields, read) = read_back(db, &table).await;
    assert_eq!(read.partitioning.method, PartitionMethod::Range);
    assert_eq!(read.partitioning.partitions.len(), 2);
    assert_eq!(read.partitioning.partitions[0].comment, "below a hundred");
    assert!(
        mysql::alter(&dialect, &table, &fields, &fields, &read, &read)
            .unwrap()
            .is_empty()
    );
    let mut hashed = read.clone();
    hashed.partitioning = Partitioning {
        method: PartitionMethod::Hash,
        expression: "id".into(),
        count: "4".into(),
        ..Default::default()
    };
    run(
        db,
        &mysql::alter(&dialect, &table, &fields, &fields, &read, &hashed).unwrap(),
    )
    .await;
    let (_, again) = read_back(db, &table).await;
    assert_eq!(again.partitioning.method, PartitionMethod::Hash);
    assert_eq!(again.partitioning.count, "4");
    let mut plain = again.clone();
    plain.partitioning = Partitioning::default();
    run(
        db,
        &mysql::alter(&dialect, &table, &fields, &fields, &again, &plain).unwrap(),
    )
    .await;
    let (_, last) = read_back(db, &table).await;
    assert_eq!(last.partitioning.method, PartitionMethod::None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real MariaDB or MySQL server described by BAZA_DB_TEST_* variables"]
async fn designer_statements_run_on_a_real_server() {
    let schema = variable("BAZA_DB_TEST_DATABASE");
    let profile = ConnectionProfile {
        name: "Owned designer fixture".into(),
        engine: engine::ENGINE,
        host: std::env::var("BAZA_DB_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
        port: variable("BAZA_DB_TEST_PORT").parse().unwrap(),
        username: variable("BAZA_DB_TEST_USER"),
        password: variable("BAZA_DB_TEST_PASSWORD"),
        database: schema.clone(),
        tls: TlsMode::Disable,
        ..ConnectionProfile::default()
    };
    let db = Database::connect(&profile, None).await.unwrap();
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
    let (parent, child, parts) = (
        format!("baza_parent_{suffix}"),
        format!("baza_child_{suffix}"),
        format!("baza_parts_{suffix}"),
    );
    let worker = db.clone();
    let (s, p, c, t) = (schema.clone(), parent.clone(), child.clone(), parts.clone());
    let outcome = tokio::spawn(async move {
        designed(&worker, &s, &p, &c).await;
        partitioned(&worker, &s, &t).await;
    })
    .await;
    for name in [&child, &parent, &parts] {
        let _ = db
            .query(
                &format!("DROP TABLE IF EXISTS `{schema}`.`{name}`"),
                CancellationToken::new(),
            )
            .await;
    }
    db.close().await;
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
}
