//! The driver checklist for Redis and the servers that speak its protocol:
//! what `driver_checklist.rs` checks for an engine with SQL, built from the
//! same steps for an engine of keys, through the same `Database` the
//! application uses.
//!
//! | SQL checklist | here |
//! | --- | --- |
//! | table designer | keys of every type made in the console, with quoting |
//! | listing, columns | the logical databases, the lists of members, the reports |
//! | import, all or nothing | rows of a file into every kind of list |
//! | paging | pages over the `SCAN` cursor, 20 000 keys, each once |
//! | filters | "contains" as a `MATCH` pattern, wildcards literal, any case |
//! | cell edit with conflict check | value, lifetime, name; members of every type |
//! | insert, delete | keys and members |
//! | query builder | none: the console and its replies as grids instead |
//! | monitor | `INFO` and `CLIENT LIST` as one row |
//! | stopping a statement | a waiting `BLPOP`, gone from the server too |
//!
//! Ignored by default and described by the environment:
//!
//! ```text
//! BAZA_DB_TEST_PORT=6379 BAZA_DB_TEST_PASSWORD=… \
//! cargo test -p baza-core --test redis_checklist -- --ignored --nocapture
//! ```
//!
//! Required: `BAZA_DB_TEST_PORT`. Optional: `BAZA_DB_TEST_HOST` (default
//! `127.0.0.1`), `BAZA_DB_TEST_USER`, `BAZA_DB_TEST_PASSWORD`,
//! `BAZA_DB_TEST_PRODUCT` (a product id of the family, such as `valkey`),
//! `BAZA_DB_TEST_TLS` (`disable`, the default, `require` or `verify_full`),
//! `BAZA_DB_TEST_DATABASE` (the database index to work in, default 3; the
//! one below it is used for a moment as well) and `BAZA_DB_TEST_OPTIONS`
//! (engine options as `key=value` pairs separated by `;`).
//!
//! Every key the run makes starts with `bazaqa:<run>:` and is removed at the
//! end. Nothing else of the server is touched: no `FLUSHDB`, no `KEYS *`.
//!
//! `redis_through_an_ssh_tunnel` runs the short version of the list through
//! a tunnel of the application: `BAZA_SSH_TEST_PORT`, `BAZA_SSH_TEST_USER`,
//! `BAZA_SSH_TEST_KEY` (a private key file) and optionally
//! `BAZA_SSH_TEST_HOST` next to the variables above.
//!
//! `tls_is_encrypted_and_verified` needs no server with TLS: the test puts a
//! TLS endpoint of its own in front of the plain one. It takes a certificate
//! for the name `localhost` (`BAZA_DB_TEST_TLS_CERT`, `BAZA_DB_TEST_TLS_KEY`)
//! and the CA that signed it (`BAZA_DB_TEST_TLS_CA`), all PEM files. With
//! them the tunnel test also checks TLS inside the tunnel.
use baza_core::{
    Engine, TunnelManager,
    builder::{Filter, FilterOp},
    database::Database,
    drivers::{self, PageRequest, RowChange},
    engine::{BrowseModel, QueryLanguage},
    model::{
        Cell, ColumnInfo, ConnectionProfile, QueryResult, SshAuth, SshProfile, TableRef, TlsMode,
        TunnelProfile,
    },
};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}
fn required(name: &str) -> String {
    variable(name).unwrap_or_else(|| panic!("{name} is required"))
}

/// The profile the environment describes.
fn profile() -> ConnectionProfile {
    let engine = Engine::from_id("redis").expect("the redis engine is registered");
    let product = variable("BAZA_DB_TEST_PRODUCT").unwrap_or_default();
    assert!(
        product.is_empty() || engine.spec().products.iter().any(|p| p.id == product),
        "BAZA_DB_TEST_PRODUCT {product} is not a product of redis"
    );
    let mut profile = ConnectionProfile {
        name: "Real Redis".into(),
        engine,
        product,
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
    profile.set_option(
        "database_index",
        &variable("BAZA_DB_TEST_DATABASE").unwrap_or_else(|| "3".into()),
    );
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

fn table(name: &str) -> TableRef {
    TableRef {
        schema: String::new(),
        name: name.into(),
        kind: "table".into(),
    }
}
fn text(value: &str) -> Cell {
    Cell::Text(value.into())
}

/// One command or several, as the console sends them.
async fn run(db: &Database, commands: &str) -> QueryResult {
    db.query(commands, CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("{error}: {commands}"))
}
/// The code a console text is refused with.
async fn refused(db: &Database, commands: &str) -> String {
    match db.query(commands, CancellationToken::new()).await {
        Ok(result) => panic!("{commands} was run: {:?}", result.rows),
        Err(error) => error.to_string(),
    }
}
/// The single cell of a reply.
async fn one(db: &Database, command: &str) -> Cell {
    let result = run(db, command).await;
    assert_eq!(result.rows.len(), 1, "{command}: {:?}", result.rows);
    result.rows[0][0].clone()
}

fn names(result: &QueryResult) -> Vec<&str> {
    result
        .columns
        .iter()
        .map(|column| column.name.as_str())
        .collect()
}
fn at(result: &QueryResult, column: &str) -> usize {
    result
        .columns
        .iter()
        .position(|found| found.name == column)
        .unwrap_or_else(|| panic!("no column {column} in {:?}", names(result)))
}
/// The texts of one column.
fn column(result: &QueryResult, name: &str) -> Vec<String> {
    let index = at(result, name);
    result.rows.iter().map(|row| row[index].display()).collect()
}
/// The row whose `name` column shows `value`.
fn row<'a>(result: &'a QueryResult, name: &str, value: &str) -> &'a Vec<Cell> {
    let index = at(result, name);
    result
        .rows
        .iter()
        .find(|row| row[index].display() == value)
        .unwrap_or_else(|| panic!("no row with {name} = {value} in {:?}", column(result, name)))
}

fn contains(name: &str, value: &str) -> Filter {
    Filter {
        column: name.into(),
        op: FilterOp::Contains,
        value: text(value),
    }
}

/// A page of a container, like "Dane tabeli" asks for it.
async fn page(db: &Database, container: &str, filters: Vec<Filter>) -> QueryResult {
    page_of(db, container, filters, None, 500, 0).await.unwrap()
}
async fn page_of(
    db: &Database,
    container: &str,
    filters: Vec<Filter>,
    order: Option<(&str, bool)>,
    limit: u32,
    offset: u64,
) -> anyhow::Result<QueryResult> {
    let request = PageRequest {
        container: table(container),
        filters,
        order_by: order.map(|(name, descending)| (name.to_owned(), descending)),
        limit,
        offset,
    };
    // The text of a page is a command the console reads.
    assert!(!db.page_text(&request)?.is_empty());
    db.read_page(&request, CancellationToken::new()).await
}

/// What addresses a row: the cells of its key columns, as the grid takes
/// them out of the page it shows.
fn key_of(columns: &[ColumnInfo], result: &QueryResult, row: &[Cell]) -> Vec<(String, Cell)> {
    columns
        .iter()
        .filter(|column| column.primary_key)
        .map(|column| (column.name.clone(), row[at(result, &column.name)].clone()))
        .collect()
}

/// Change one cell of the row that shows `value` in `find`, the way the
/// grid does: the old cell and the key come from the page.
async fn edit(
    db: &Database,
    container: &str,
    result: &QueryResult,
    find: (&str, &str),
    name: &str,
    new: Cell,
) -> anyhow::Result<()> {
    let columns = db.columns(&table(container)).await?;
    let found = row(result, find.0, find.1);
    let change = RowChange::UpdateCell {
        table: table(container),
        column: name.into(),
        old: found[at(result, name)].clone(),
        new,
        key: key_of(&columns, result, found),
    };
    assert!(!db.change_text(&change)?.is_empty());
    db.apply(&change).await
}
async fn insert(db: &Database, container: &str, values: &[(&str, Cell)]) -> anyhow::Result<()> {
    let change = RowChange::InsertRow {
        table: table(container),
        values: values
            .iter()
            .map(|(name, cell)| ((*name).to_owned(), cell.clone()))
            .collect(),
    };
    db.change_text(&change)?;
    db.apply(&change).await
}
async fn delete(
    db: &Database,
    container: &str,
    result: &QueryResult,
    find: (&str, &str),
) -> anyhow::Result<()> {
    let columns = db.columns(&table(container)).await?;
    let found = row(result, find.0, find.1);
    let change = RowChange::DeleteRow {
        table: table(container),
        key: key_of(&columns, result, found),
    };
    db.change_text(&change)?;
    db.apply(&change).await
}
fn code<T>(result: anyhow::Result<T>) -> String {
    match result {
        Ok(_) => panic!("the change was made"),
        Err(error) => error.to_string(),
    }
}
const CONFLICT: &str = "row_changed_reload_required";

/// The sentence the interface would show for a code: every code of the
/// driver has one, in both languages.
fn explained(code: &str) {
    if let Some(own) = code.strip_prefix("redis_") {
        for en in [false, true] {
            let sentence = drivers::describe(code, en)
                .unwrap_or_else(|| panic!("redis_{own} has no sentence"));
            assert!(sentence.contains(' '), "{code}");
        }
    }
}

/// Remove every key of the run from a database, through the console.
async fn sweep(db: &Database, index: i64, prefix: &str) -> usize {
    let quote = |key: &str| format!("\"{}\"", key.replace('\\', "\\\\").replace('"', "\\\""));
    run(db, &format!("SELECT {index}")).await;
    let mut removed = 0;
    let mut cursor = "0".to_owned();
    loop {
        let step = run(
            db,
            &format!(
                "SCAN {cursor} MATCH {} COUNT 2000",
                quote(&format!("{prefix}*"))
            ),
        )
        .await;
        cursor = step.rows[0][0].display();
        let keys: Vec<String> = step
            .rows
            .iter()
            .filter(|row| !matches!(row[1], Cell::Null))
            .map(|row| match &row[1] {
                // A key that is not text is written with its bytes escaped.
                Cell::Unsupported(hex) => format!(
                    "\"{}\"",
                    hex.trim_start_matches("0x")
                        .as_bytes()
                        .chunks(2)
                        .map(|pair| format!("\\x{}", String::from_utf8_lossy(pair)))
                        .collect::<String>()
                ),
                other => quote(&other.display()),
            })
            .collect();
        for chunk in keys.chunks(500) {
            removed += run(db, &format!("DEL {}", chunk.join(" "))).await.rows[0][0]
                .display()
                .parse::<usize>()
                .unwrap();
        }
        if cursor == "0" {
            break;
        }
    }
    removed
}

/// The checklist. `p` is the prefix of every key it makes.
async fn checklist(db: &Database, profile: &ConnectionProfile, p: &str) {
    let label = profile.product_label();
    let home: i64 = profile.option("database_index").parse().unwrap();
    let other = if home == 0 { 1 } else { home - 1 };
    let keys = format!("db{home}");
    let caps = db.capabilities();
    assert_eq!(caps.language, QueryLanguage::Command);
    assert_eq!(caps.browse, BrowseModel::Keys);
    assert!(caps.filter && caps.sort && caps.edit_cells && caps.insert_rows && caps.delete_rows);
    assert!(caps.import && caps.export && caps.monitor);
    assert!(!caps.schemas && !caps.table_designer && !caps.query_builder);

    // 1. The console makes keys of every type, with quoting: a key with a
    // space, text in another alphabet, bytes that are not text. Several
    // commands answer with one row each.
    let seeded = run(
        db,
        &format!(
            "# what the checklist works on\n\
             SET \"{p}greeting text\" \"hello world\"\n\
             SET {p}counter 41\n\
             INCR {p}counter\n\
             HSET {p}user:1 name \"Jan Kowalski\" email jan@example.test visits 12\n\
             RPUSH {p}queue first second 'third item'\n\
             SADD {p}tags red green blue\n\
             ZADD {p}scores 1 low 2.5 mid 10 high\n\
             XADD {p}events * sensor a value 21.5\n\
             SET \"{p}bin\\xff\\x00key\" \"\\x00\\x01\\xfe\\xff\"\n\
             SET \"{p}zażółć gęślą\" \"jaźń 🦀\"\n\
             SET \"{p}100%*?[x]\" wildcards\n\
             EXPIRE \"{p}greeting text\" 3600\n"
        ),
    )
    .await;
    assert_eq!(names(&seeded), ["command", "reply"]);
    assert_eq!(seeded.rows.len(), 12, "{label}: one row per command");
    assert_eq!(seeded.rows[0][1], text("OK"));
    assert_eq!(seeded.rows[2][1], Cell::Integer("42".into()));
    assert_eq!(seeded.rows[3][1], Cell::Integer("3".into()));
    assert_eq!(
        seeded.rows[4][0],
        text(&format!("RPUSH {p}queue first second 'third item'"))
    );
    assert_eq!(seeded.rows[11][1], Cell::Integer("1".into()));

    // 2. A single command answers with its reply as a grid.
    assert_eq!(
        one(db, &format!("GET \"{p}greeting text\"")).await,
        text("hello world")
    );
    assert_eq!(one(db, &format!("GET {p}missing")).await, Cell::Null);
    assert_eq!(one(db, &format!("TYPE {p}scores")).await, text("zset"));
    assert_eq!(
        one(db, &format!("LLEN {p}queue")).await,
        Cell::Integer("3".into())
    );
    assert_eq!(
        one(db, &format!("GET \"{p}bin\\xff\\x00key\"")).await,
        Cell::Unsupported("0x0001feff".into()),
        "{label}: bytes that are not text are shown behind a marker"
    );
    assert_eq!(
        one(db, &format!("GET \"{p}zażółć gęślą\"")).await,
        text("jaźń 🦀")
    );
    let hash = run(db, &format!("HGETALL {p}user:1")).await;
    assert_eq!(
        names(&hash),
        ["field", "value"],
        "{label}: a map is two columns"
    );
    assert_eq!(row(&hash, "field", "name")[1], text("Jan Kowalski"));
    assert_eq!(hash.rows.len(), 3);
    let list = run(db, &format!("LRANGE {p}queue 0 -1")).await;
    assert_eq!(column(&list, "value"), ["first", "second", "third item"]);
    let scores = run(db, &format!("ZRANGE {p}scores 0 -1 WITHSCORES")).await;
    assert_eq!(names(&scores), ["member", "score"]);
    assert_eq!(column(&scores, "member"), ["low", "mid", "high"]);
    assert_eq!(column(&scores, "score"), ["1", "2.5", "10"]);
    let entries = run(db, &format!("XRANGE {p}events - +")).await;
    assert_eq!(names(&entries), ["id", "sensor", "value"]);
    assert_eq!(entries.rows[0][2], text("21.5"));
    let nested = run(db, "EVAL \"return {1, {2, 'a b', {3}}, 'x'}\" 0").await;
    assert_eq!(
        column(&nested, "value"),
        ["1", "[2, \"a b\", [3]]", "x"],
        "{label}: nested"
    );
    let info = run(db, "INFO server").await;
    assert_eq!(names(&info), ["section", "property", "value"]);
    assert!(
        column(&info, "property")
            .iter()
            .any(|name| name == "redis_version")
    );
    let clients = run(db, "CLIENT LIST").await;
    assert!(
        names(&clients).starts_with(&["id", "addr", "name"]),
        "{:?}",
        names(&clients)
    );
    assert!(
        column(&clients, "name")
            .iter()
            .any(|name| name == "baza-studio")
    );
    let step = run(db, &format!("SCAN 0 MATCH {p}user* COUNT 100000")).await;
    assert_eq!(names(&step), ["next cursor", "key"]);
    assert!(column(&step, "key").contains(&format!("{p}user:1")));
    let config = run(db, "CONFIG GET maxmemory").await;
    assert_eq!(names(&config), ["field", "value"]);
    assert_eq!(config.rows[0][0], text("maxmemory"));
    // A text of comments only sends nothing.
    assert!(run(db, "# nothing\n  # at all").await.columns.is_empty());

    // 3. What the console refuses, what it asks twice for, and what the
    // server refuses: always a code that has a sentence.
    for (commands, expected) in [
        (format!("LPUSH {p}counter x"), "redis_wrong_type"),
        ("NOSUCHCOMMAND 1".to_owned(), "redis_unknown_command"),
        (format!("SET {p}onlykey"), "redis_wrong_arguments"),
        (format!("INCR \"{p}greeting text\""), "redis_not_an_integer"),
        (format!("GET \"{p}open"), "redis_quote_unterminated"),
        ("EXEC".to_owned(), "redis_exec_without_multi"),
        ("MONITOR".to_owned(), "redis_stream_refused"),
        (format!("SUBSCRIBE {p}channel"), "redis_stream_refused"),
        ("AUTH something".to_owned(), "redis_session_refused"),
        ("HELLO 2".to_owned(), "redis_session_refused"),
        ("QUIT".to_owned(), "redis_session_refused"),
        (format!("BLPOP {p}nothing 0"), "redis_wait_forever"),
        (
            format!("XREAD BLOCK 0 STREAMS {p}events $"),
            "redis_wait_forever",
        ),
        ("FLUSHALL".to_owned(), "redis_confirm_data_loss"),
        ("flushdb".to_owned(), "redis_confirm_data_loss"),
        ("SHUTDOWN NOSAVE".to_owned(), "redis_confirm_server"),
        ("DEBUG SLEEP 5".to_owned(), "redis_confirm_server"),
        ("CONFIG SET maxmemory 1".to_owned(), "redis_confirm_server"),
        ("CONFIRM MONITOR".to_owned(), "redis_stream_refused"),
        ("SELECT 99999".to_owned(), "redis_database_index_invalid"),
        // What a node of a cluster, a replica and a full server answer,
        // said by a script here: the code word decides, the address and the
        // rest of the sentence stay with the server.
        (
            "EVAL \"return redis.error_reply('MOVED 3999 10.0.0.5:6381')\" 0".to_owned(),
            "redis_cluster_moved",
        ),
        (
            "EVAL \"return redis.error_reply('ASK 3999 10.0.0.5:6381')\" 0".to_owned(),
            "redis_cluster_moved",
        ),
        (
            "EVAL \"return redis.error_reply('CLUSTERDOWN The cluster is down')\" 0".to_owned(),
            "redis_cluster_down",
        ),
        (
            "EVAL \"return redis.error_reply('READONLY You cannot write against a read only replica.')\" 0"
                .to_owned(),
            "redis_read_only_replica",
        ),
        (
            "EVAL \"return redis.error_reply('OOM command not allowed when used memory > maxmemory.')\" 0"
                .to_owned(),
            "redis_out_of_memory",
        ),
        (
            "EVAL \"return redis.error_reply('LOADING the dataset in memory')\" 0".to_owned(),
            "redis_server_not_ready",
        ),
    ] {
        assert_eq!(
            refused(db, &commands).await,
            expected,
            "{label}: {commands}"
        );
        explained(expected);
    }
    // Nothing of that reached the server: the keys are all still there.
    assert_eq!(
        one(db, &format!("EXISTS {p}counter {p}user:1")).await,
        Cell::Integer("2".into())
    );
    // Behind the word, a command of that kind runs.
    assert_eq!(one(db, "CONFIRM CONFIG RESETSTAT").await, text("OK"));
    assert_eq!(one(db, "confirm config resetstat").await, text("OK"));
    // The command of a text that failed is named by its number; the ones
    // before it were run, the ones after it were not.
    assert_eq!(
        refused(db, &format!("SET {p}a 1\nLPUSH {p}a x\nSET {p}b 2")).await,
        "redis_wrong_type_at_command_2"
    );
    explained("redis_wrong_type_at_command_2");
    assert_eq!(
        one(db, &format!("EXISTS {p}a")).await,
        Cell::Integer("1".into())
    );
    assert_eq!(
        one(db, &format!("EXISTS {p}b")).await,
        Cell::Integer("0".into())
    );
    // A transaction over several lines, and one that a failed line ends.
    let exec = run(db, &format!("MULTI\nSET {p}t 1\nINCR {p}t\nEXEC")).await;
    assert_eq!(
        column(&exec, "reply"),
        ["OK", "QUEUED", "QUEUED", "[OK, 2]"]
    );
    assert_eq!(
        refused(db, &format!("MULTI\nSET {p}t2 1\nNOSUCHCOMMAND\nEXEC")).await,
        "redis_unknown_command_at_command_3"
    );
    assert_eq!(
        one(db, &format!("SET {p}t3 1")).await,
        text("OK"),
        "{label}: no MULTI left open"
    );
    assert_eq!(
        one(db, &format!("EXISTS {p}t2")).await,
        Cell::Integer("0".into())
    );
    // SELECT moves the console, and it stays there.
    assert_eq!(one(db, &format!("SELECT {other}")).await, text("OK"));
    assert_eq!(one(db, &format!("SET {p}elsewhere 1")).await, text("OK"));
    assert_eq!(
        one(db, &format!("EXISTS {p}counter")).await,
        Cell::Integer("0".into())
    );
    assert_eq!(one(db, &format!("SELECT {home}")).await, text("OK"));
    assert_eq!(
        one(db, &format!("EXISTS {p}counter")).await,
        Cell::Integer("1".into())
    );

    // 4. The side panel: the databases that hold keys, the lists of their
    // members and the reports.
    let containers = db.tables().await.unwrap();
    let listed: Vec<&str> = containers.iter().map(|found| found.name.as_str()).collect();
    for expected in [
        keys.clone(),
        format!("db{other}"),
        format!("{keys} · hashes"),
        format!("{keys} · lists"),
        format!("{keys} · sets"),
        format!("{keys} · sorted sets"),
        format!("{keys} · streams"),
        "clients".to_owned(),
        "databases".to_owned(),
        "server info".to_owned(),
        "slow log".to_owned(),
    ] {
        assert_eq!(
            listed.iter().filter(|name| **name == expected).count(),
            1,
            "{label}: {expected} is listed once: {listed:?}"
        );
    }
    for found in &containers {
        let report = !found.name.starts_with("db");
        assert_eq!(
            found.kind,
            if report { "view" } else { "table" },
            "{}",
            found.name
        );
        assert!(found.schema.is_empty());
        let columns = db.columns(found).await.unwrap();
        // A list of data has a complete key; a report is only read.
        assert_eq!(
            columns.iter().any(|column| column.primary_key),
            !report,
            "{}",
            found.name
        );
    }
    let key_columns = db.columns(&table(&keys)).await.unwrap();
    assert_eq!(
        key_columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        ["key", "type", "ttl", "size", "value"]
    );
    assert!(key_columns[0].primary_key && key_columns[2].nullable);

    // 5. The key list: one row per key, of every type.
    let all = page(db, &keys, vec![contains("key", p)]).await;
    assert_eq!(names(&all), ["key", "type", "ttl", "size", "value"]);
    let key = |name: &str| format!("{p}{name}");
    let greeting = row(&all, "key", &key("greeting text"));
    assert_eq!(greeting[1].display(), "string");
    let left: i64 = greeting[2].display().parse().unwrap();
    assert!((3500..=3600).contains(&left), "{label}: ttl {left}");
    assert_eq!(greeting[3].display(), "11");
    assert_eq!(greeting[4], text("hello world"));
    let counter = row(&all, "key", &key("counter"));
    assert_eq!(
        (counter[2].clone(), counter[4].clone()),
        (Cell::Null, text("42"))
    );
    let user = row(&all, "key", &key("user:1"));
    assert_eq!(
        (user[1].display().as_str(), user[3].display().as_str()),
        ("hash", "3")
    );
    assert!(
        matches!(&user[4], Cell::Json(json) if json.contains("\"name\": \"Jan Kowalski\"")),
        "{:?}",
        user[4]
    );
    assert_eq!(
        row(&all, "key", &key("queue"))[4],
        Cell::Json(r#"["first", "second", "third item"]"#.into())
    );
    assert_eq!(
        row(&all, "key", &key("tags"))[4],
        Cell::Json(r#"["blue", "green", "red"]"#.into())
    );
    assert_eq!(
        row(&all, "key", &key("scores"))[4],
        Cell::Json(r#"{"low": 1, "mid": 2.5, "high": 10}"#.into())
    );
    let events = row(&all, "key", &key("events"));
    assert_eq!(
        (events[1].display().as_str(), events[3].display().as_str()),
        ("stream", "1")
    );
    assert!(events[4].display().contains("\"sensor\": \"a\"") && !events[4].editable());
    // What follows from the value is shown and never typed over.
    assert!(!user[1].editable() && !user[3].editable());
    // Bytes that are not text: the key and the value behind the marker.
    let binary_name = format!(
        "0x{}ff006b6579",
        p.bytes().map(|b| format!("{b:02x}")).collect::<String>() + "62696e"
    );
    let binary = row(&all, "key", &binary_name);
    assert_eq!(binary[0], Cell::Unsupported(binary_name.clone()));
    assert_eq!(binary[4], Cell::Unsupported("0x0001feff".into()));
    assert_eq!(row(&all, "key", &key("zażółć gęślą"))[4], text("jaźń 🦀"));

    // 6. Import: 20 000 keys at once, then batches that must leave nothing.
    let head = ["key", "value"].map(str::to_owned);
    let bulk: Vec<Vec<Cell>> = (0..20_000)
        .map(|n| {
            vec![
                text(&format!("{p}bulk:{n:05}")),
                text(&format!("value {n}")),
            ]
        })
        .collect();
    let started = Instant::now();
    let imported = db
        .import_rows(&table(&keys), &head, &bulk, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(imported.affected, 20_000, "{label}: imported keys");
    let import_ms = started.elapsed().as_millis();
    let clash = vec![
        vec![text(&key("fresh")), text("new")],
        vec![text(&key("counter")), text("taken")],
    ];
    assert_eq!(
        code(
            db.import_rows(&table(&keys), &head, &clash, CancellationToken::new())
                .await
        ),
        "redis_key_exists"
    );
    let twice = vec![
        vec![text(&key("fresh")), text("a")],
        vec![text(&key("fresh")), text("b")],
    ];
    assert_eq!(
        code(
            db.import_rows(&table(&keys), &head, &twice, CancellationToken::new())
                .await
        ),
        "redis_key_exists"
    );
    assert_eq!(
        one(db, &format!("EXISTS {p}fresh")).await,
        Cell::Integer("0".into()),
        "{label}: all or nothing"
    );
    assert_eq!(one(db, &format!("GET {p}counter")).await, text("42"));
    // Typed rows with a lifetime, as a file of the key list holds them.
    let typed = db
        .import_rows(
            &table(&keys),
            &["key", "type", "ttl", "size", "value"].map(str::to_owned),
            &[
                vec![
                    text(&key("sort:a")),
                    text("string"),
                    Cell::Integer("100".into()),
                    Cell::Null,
                    text("1"),
                ],
                vec![
                    text(&key("sort:b")),
                    text("string"),
                    Cell::Null,
                    text("999"),
                    text("333"),
                ],
                vec![
                    text(&key("sort:c")),
                    text("list"),
                    text("50"),
                    Cell::Null,
                    text(r#"["x", "y"]"#),
                ],
            ],
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(typed.affected, 3);
    // Members: every kind of list takes rows, and refuses what is there.
    let hash_fields = format!("{keys} · hashes");
    let fields = ["key", "field", "value"].map(str::to_owned);
    db.import_rows(
        &table(&hash_fields),
        &fields,
        &[
            vec![text(&key("user:2")), text("name"), text("Ewa")],
            vec![
                text(&key("user:2")),
                text("email"),
                text("ewa@example.test"),
            ],
            vec![text(&key("user:1")), text("city"), text("Łódź")],
        ],
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        code(
            db.import_rows(
                &table(&hash_fields),
                &fields,
                &[
                    vec![text(&key("user:3")), text("name"), text("new")],
                    vec![text(&key("user:1")), text("name"), text("taken")],
                ],
                CancellationToken::new(),
            )
            .await
        ),
        "redis_member_exists"
    );
    assert_eq!(
        one(db, &format!("EXISTS {p}user:3")).await,
        Cell::Integer("0".into())
    );
    assert_eq!(
        code(
            db.import_rows(
                &table(&hash_fields),
                &fields,
                &[vec![
                    text(&key("counter")),
                    text("name"),
                    text("not a hash")
                ]],
                CancellationToken::new(),
            )
            .await
        ),
        "redis_wrong_type"
    );
    let import = |container: String, columns: Vec<&'static str>, rows: Vec<Vec<Cell>>| async move {
        let columns: Vec<String> = columns.into_iter().map(str::to_owned).collect();
        db.import_rows(
            &table(&container),
            &columns,
            &rows,
            CancellationToken::new(),
        )
        .await
    };
    import(
        format!("{keys} · lists"),
        vec!["key", "value"],
        vec![
            vec![text(&key("queue")), text("fourth")],
            vec![text(&key("jobs")), text("a")],
            vec![text(&key("jobs")), text("a")],
        ],
    )
    .await
    .unwrap();
    import(
        format!("{keys} · sets"),
        vec!["key", "member"],
        vec![vec![text(&key("tags")), text("yellow")]],
    )
    .await
    .unwrap();
    assert_eq!(
        code(
            import(
                format!("{keys} · sets"),
                vec!["key", "member"],
                vec![
                    vec![text(&key("tags")), text("violet")],
                    vec![text(&key("tags")), text("red")],
                ]
            )
            .await
        ),
        "redis_member_exists"
    );
    import(
        format!("{keys} · sorted sets"),
        vec!["key", "member", "score"],
        vec![vec![
            text(&key("scores")),
            text("top"),
            Cell::Float("99.5".into()),
        ]],
    )
    .await
    .unwrap();
    import(
        format!("{keys} · streams"),
        vec!["key", "entry"],
        vec![
            vec![
                text(&key("events")),
                text(r#"{"sensor": "b", "value": 19}"#),
            ],
            vec![
                text(&key("events")),
                text(r#"{"sensor": "c", "value": 20}"#),
            ],
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        code(
            import(
                format!("{keys} · streams"),
                vec!["key", "id", "entry"],
                vec![vec![
                    text(&key("events")),
                    text("1-1"),
                    text(r#"{"sensor": "old"}"#)
                ],]
            )
            .await
        ),
        "redis_stream_id_invalid"
    );
    assert_eq!(
        one(db, &format!("SCARD {p}tags")).await,
        Cell::Integer("4".into())
    );
    assert_eq!(
        one(db, &format!("XLEN {p}events")).await,
        Cell::Integer("3".into())
    );
    assert_eq!(
        one(db, &format!("LLEN {p}jobs")).await,
        Cell::Integer("2".into())
    );

    // 7. Pages over the scan cursor: every key once, a page read again is
    // the same page, and a deep page goes on where the last one stopped.
    let bulk_filter = || vec![contains("key", &format!("{p}bulk:"))];
    let mut seen = HashSet::new();
    let mut second = Vec::new();
    for number in 0..40u64 {
        let found = page_of(db, &keys, bulk_filter(), None, 500, number * 500)
            .await
            .unwrap();
        assert_eq!(found.rows.len(), 500, "{label}: page {number}");
        for name in column(&found, "key") {
            assert!(seen.insert(name), "{label}: a key on two pages");
        }
        if number == 1 {
            second = column(&found, "key");
        }
    }
    assert_eq!(seen.len(), 20_000);
    assert!(
        page_of(db, &keys, bulk_filter(), None, 500, 20_000)
            .await
            .unwrap()
            .rows
            .is_empty()
    );
    let again = page_of(db, &keys, bulk_filter(), None, 500, 500)
        .await
        .unwrap();
    assert_eq!(
        column(&again, "key"),
        second,
        "{label}: the second page read again"
    );
    let small = page_of(db, &keys, bulk_filter(), None, 50, 100)
        .await
        .unwrap();
    assert_eq!(small.rows.len(), 50);
    // Nothing matches: the columns are still there.
    let none = page(db, &keys, vec![contains("key", &format!("{p}no such key"))]).await;
    assert!(none.rows.is_empty() && none.columns.len() == 5);

    // 8. The "contains" filter: any letter case, wildcards as plain
    // characters, another alphabet, the other columns.
    for (name, value, expected) in [
        (
            "key",
            format!("{}USER:1", p.to_uppercase()),
            vec![key("user:1")],
        ),
        ("key", format!("{p}ZAŻÓŁĆ GĘ"), vec![key("zażółć gęślą")]),
        ("key", format!("{p}100%*?[x"), vec![key("100%*?[x]")]),
        (
            "key",
            format!("{p}bulk:1234"),
            (0..10).map(|n| key(&format!("bulk:1234{n}"))).collect(),
        ),
        ("key", format!("{p}*"), vec![]),
    ] {
        let found = page(db, &keys, vec![contains(name, &value)]).await;
        let mut got = column(&found, "key");
        got.sort();
        assert_eq!(got, expected, "{label}: {name} contains {value:?}");
    }
    let hashes = page(
        db,
        &keys,
        vec![contains("key", p), contains("type", "HASH")],
    )
    .await;
    let mut got = column(&hashes, "key");
    got.sort();
    assert_eq!(got, [key("user:1"), key("user:2")], "{label}: type filter");
    let sets = page(db, &keys, vec![contains("key", p), contains("type", "set")]).await;
    let mut got = column(&sets, "key");
    got.sort();
    assert_eq!(
        got,
        [key("scores"), key("tags")],
        "{label}: set and zset both contain 'set'"
    );
    let by_value = page(
        db,
        &keys,
        vec![
            contains("key", &format!("{p}greet")),
            contains("value", "WORLD"),
        ],
    )
    .await;
    assert_eq!(column(&by_value, "key"), [key("greeting text")]);
    let by_size = page(
        db,
        &keys,
        vec![contains("key", &format!("{p}sort:")), contains("size", "3")],
    )
    .await;
    assert_eq!(
        column(&by_size, "key"),
        [key("sort:b")],
        "{label}: a number searched as text"
    );

    // 9. Order, within its limits.
    let sorted = async |order: (&'static str, bool)| -> String {
        let found = page_of(
            db,
            &keys,
            vec![contains("key", &format!("{p}sort:"))],
            Some(order),
            500,
            0,
        )
        .await
        .unwrap();
        column(&found, "key")
            .into_iter()
            .map(|name| name.rsplit(':').next().unwrap().to_owned())
            .collect::<Vec<_>>()
            .join("")
    };
    assert_eq!(sorted(("key", false)).await, "abc");
    assert_eq!(sorted(("key", true)).await, "cba");
    assert_eq!(
        sorted(("size", false)).await,
        "acb",
        "{label}: 1 byte, 2 members, 3 bytes"
    );
    assert_eq!(
        sorted(("ttl", false)).await,
        "cab",
        "{label}: a key that never expires is last"
    );
    assert_eq!(sorted(("ttl", true)).await, "bac");
    assert_eq!(sorted(("type", false)).await, "cab");
    let ordered = page_of(db, &keys, bulk_filter(), Some(("key", true)), 3, 0)
        .await
        .unwrap();
    assert_eq!(
        column(&ordered, "key"),
        [key("bulk:19999"), key("bulk:19998"), key("bulk:19997")],
        "{label}: 20 000 keys ordered"
    );
    let deep = page_of(db, &keys, bulk_filter(), Some(("key", false)), 50, 19_950)
        .await
        .unwrap();
    assert_eq!(deep.rows.len(), 50);
    assert_eq!(deep.rows[49][0], text(&key("bulk:19999")));
    assert_eq!(
        code(page_of(db, &keys, vec![], Some(("key", false)), 500, 0).await),
        "redis_sort_limit",
        "{label}: more keys than a sorted list holds"
    );
    assert_eq!(
        code(page_of(db, &keys, bulk_filter(), Some(("value", false)), 500, 0).await),
        "redis_sort_column"
    );
    for refusal in ["redis_sort_limit", "redis_sort_column"] {
        explained(refusal);
    }
    // KEYS on a database of this size is asked twice.
    assert_eq!(
        refused(db, &format!("KEYS {p}*")).await,
        "redis_confirm_keys"
    );
    explained("redis_confirm_keys");
    let walked = run(db, &format!("CONFIRM KEYS {p}bulk:1999?")).await;
    assert_eq!(walked.rows.len(), 10);

    // 10. One cell of the key list, with the conflict check.
    let filter = |name: &str| vec![contains("key", &format!("{p}{name}"))];
    let shown = page(db, &keys, filter("greeting")).await;
    let greeting_key = key("greeting text");
    edit(
        db,
        &keys,
        &shown,
        ("key", &greeting_key),
        "value",
        text("Kopia D:\\zrzut\\ 'x' \"y\""),
    )
    .await
    .unwrap();
    assert_eq!(
        code(
            edit(
                db,
                &keys,
                &shown,
                ("key", &greeting_key),
                "value",
                text("again")
            )
            .await
        ),
        CONFLICT,
        "{label}: the same edit again is a conflict"
    );
    let shown = page(db, &keys, filter("greeting")).await;
    let after = row(&shown, "key", &greeting_key);
    assert_eq!(after[4], text("Kopia D:\\zrzut\\ 'x' \"y\""));
    let left: i64 = after[2].display().parse().unwrap();
    assert!(
        (3400..=3600).contains(&left),
        "{label}: a new value keeps the lifetime, {left}"
    );
    // The lifetime: given, changed although it ran down meanwhile, taken away.
    let shown = page(db, &keys, filter("counter")).await;
    let counter_key = key("counter");
    assert_eq!(row(&shown, "key", &counter_key)[2], Cell::Null);
    edit(db, &keys, &shown, ("key", &counter_key), "ttl", text("120"))
        .await
        .unwrap();
    assert_eq!(
        code(edit(db, &keys, &shown, ("key", &counter_key), "ttl", text("300")).await),
        CONFLICT,
        "{label}: the grid still showed a key without a lifetime"
    );
    let shown = page(db, &keys, filter("counter")).await;
    let left: i64 = row(&shown, "key", &counter_key)[2]
        .display()
        .parse()
        .unwrap();
    assert!((110..=120).contains(&left), "{label}: ttl {left}");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    edit(
        db,
        &keys,
        &shown,
        ("key", &counter_key),
        "ttl",
        Cell::Integer("900".into()),
    )
    .await
    .unwrap();
    for invalid in [text("0"), text("soon"), text("-5")] {
        let shown = page(db, &keys, filter("counter")).await;
        assert_eq!(
            code(edit(db, &keys, &shown, ("key", &counter_key), "ttl", invalid).await),
            "redis_ttl_invalid"
        );
    }
    let shown = page(db, &keys, filter("counter")).await;
    edit(db, &keys, &shown, ("key", &counter_key), "ttl", Cell::Null)
        .await
        .unwrap();
    assert_eq!(
        one(db, &format!("TTL {p}counter")).await,
        Cell::Integer("-1".into())
    );
    assert_eq!(
        one(db, &format!("GET {p}counter")).await,
        text("42"),
        "{label}: the value is untouched"
    );
    // The name.
    let shown = page(db, &keys, filter("counter")).await;
    assert_eq!(
        code(
            edit(
                db,
                &keys,
                &shown,
                ("key", &counter_key),
                "key",
                text(&key("user:1"))
            )
            .await
        ),
        "redis_key_exists"
    );
    edit(
        db,
        &keys,
        &shown,
        ("key", &counter_key),
        "key",
        text(&key("renamed counter")),
    )
    .await
    .unwrap();
    assert_eq!(
        code(
            edit(
                db,
                &keys,
                &shown,
                ("key", &counter_key),
                "key",
                text(&key("gone"))
            )
            .await
        ),
        CONFLICT
    );
    assert_eq!(
        one(db, &format!("GET \"{p}renamed counter\"")).await,
        text("42")
    );
    // What follows from the value is not a cell to change.
    let shown = page(db, &keys, filter("renamed")).await;
    for derived in ["type", "size"] {
        assert_eq!(
            code(
                edit(
                    db,
                    &keys,
                    &shown,
                    ("key", &key("renamed counter")),
                    derived,
                    text("9")
                )
                .await
            ),
            "redis_column_read_only"
        );
    }
    // A small collection is edited as the JSON text it is shown as.
    let shown = page(db, &keys, filter("user:2")).await;
    let user2 = key("user:2");
    run(db, &format!("EXPIRE {p}user:2 500")).await;
    edit(
        db,
        &keys,
        &shown,
        ("key", &user2),
        "value",
        Cell::Json(r#"{"name": "Ewa Nowak", "age": 30}"#.into()),
    )
    .await
    .unwrap();
    let stored = run(db, &format!("HGETALL {p}user:2")).await;
    assert_eq!(stored.rows.len(), 2);
    assert_eq!(row(&stored, "field", "name")[1], text("Ewa Nowak"));
    assert_eq!(row(&stored, "field", "age")[1], text("30"));
    let left: i64 = one(db, &format!("TTL {p}user:2"))
        .await
        .display()
        .parse()
        .unwrap();
    assert!(
        (490..=500).contains(&left),
        "{label}: a rewritten hash keeps its lifetime"
    );
    assert_eq!(
        code(
            edit(
                db,
                &keys,
                &shown,
                ("key", &user2),
                "value",
                Cell::Json(r#"{"name": "stale"}"#.into())
            )
            .await
        ),
        CONFLICT
    );
    let shown = page(db, &keys, filter("user:2")).await;
    for (json, expected) in [
        (r#"["not", "an", "object"]"#, "redis_json_shape_invalid"),
        (r#"{"nested": {"a": 1}}"#, "redis_json_shape_invalid"),
        ("{}", "redis_collection_empty"),
    ] {
        assert_eq!(
            code(
                edit(
                    db,
                    &keys,
                    &shown,
                    ("key", &user2),
                    "value",
                    Cell::Json(json.into())
                )
                .await
            ),
            expected
        );
        explained(expected);
    }
    for (name, json, check, expected) in [
        (
            "queue",
            r#"["only", "two"]"#,
            format!("LRANGE {p}queue 0 -1"),
            vec!["only", "two"],
        ),
        (
            "tags",
            r#"["b", "a", "b"]"#,
            format!("SORT {p}tags ALPHA"),
            vec!["a", "b"],
        ),
        (
            "scores",
            r#"{"x": 3, "y": -1.5}"#,
            format!("ZRANGE {p}scores 0 -1"),
            vec!["y", "x"],
        ),
    ] {
        let shown = page(db, &keys, filter(name)).await;
        edit(
            db,
            &keys,
            &shown,
            ("key", &key(name)),
            "value",
            Cell::Json(json.into()),
        )
        .await
        .unwrap();
        assert_eq!(
            column(&run(db, &check).await, "value"),
            expected,
            "{label}: {name}"
        );
    }
    run(
        db,
        &format!("RPUSH {p}queue \"third item\"\nSADD {p}tags red\nZADD {p}scores 2.5 mid"),
    )
    .await;

    // 11. A new key and a removed one.
    insert(
        db,
        &keys,
        &[
            ("key", text(&key("added"))),
            ("ttl", Cell::Null),
            ("value", text("new")),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        code(
            insert(
                db,
                &keys,
                &[("key", text(&key("added"))), ("value", text("again"))]
            )
            .await
        ),
        "redis_key_exists"
    );
    insert(
        db,
        &keys,
        &[
            ("key", text(&key("added hash"))),
            ("type", text("hash")),
            ("ttl", text("600")),
            ("value", text(r#"{"a": "1"}"#)),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        one(db, &format!("HGET \"{p}added hash\" a")).await,
        text("1")
    );
    assert_eq!(
        code(
            insert(
                db,
                &keys,
                &[
                    ("key", text(&key("added hash"))),
                    ("type", text("list")),
                    ("value", text(r#"["x"]"#))
                ]
            )
            .await
        ),
        "redis_key_exists"
    );
    for (values, expected) in [
        (vec![("value", text("no key"))], "redis_key_required"),
        (
            vec![("key", text(&key("x"))), ("type", text("table"))],
            "redis_type_unknown",
        ),
        (
            vec![
                ("key", text(&key("x"))),
                ("type", text("zset")),
                ("value", text("[1]")),
            ],
            "redis_json_shape_invalid",
        ),
    ] {
        assert_eq!(code(insert(db, &keys, &values).await), expected);
        explained(expected);
    }
    // A key made through the grid is on the first page, where it is seen.
    let first = page_of(db, &keys, vec![], None, 500, 0).await.unwrap();
    assert_eq!(first.rows.len(), 500);
    assert_eq!(
        first.rows[0][0],
        text(&key("added hash")),
        "{label}: new keys first"
    );
    assert_eq!(first.rows[1][0], text(&key("added")));
    let shown = page(db, &keys, filter("added")).await;
    delete(db, &keys, &shown, ("key", &key("added")))
        .await
        .unwrap();
    assert_eq!(
        code(delete(db, &keys, &shown, ("key", &key("added"))).await),
        CONFLICT
    );
    assert_eq!(
        one(db, &format!("EXISTS {p}added")).await,
        Cell::Integer("0".into())
    );

    // 12. Bytes that are not text are addressed, never changed.
    let shown = page(db, &keys, filter("bin")).await;
    let binary = row(&shown, "key", &binary_name);
    assert!(!binary[0].editable() && !binary[4].editable());
    edit(db, &keys, &shown, ("key", &binary_name), "ttl", text("700"))
        .await
        .unwrap();
    assert_eq!(
        one(db, &format!("GET \"{p}bin\\xff\\x00key\"")).await,
        Cell::Unsupported("0x0001feff".into()),
        "{label}: a lifetime leaves the bytes alone"
    );
    let left: i64 = one(db, &format!("TTL \"{p}bin\\xff\\x00key\""))
        .await
        .display()
        .parse()
        .unwrap();
    assert!((690..=700).contains(&left));
    let shown = page(db, &keys, filter("bin")).await;
    edit(
        db,
        &keys,
        &shown,
        ("key", &binary_name),
        "key",
        text(&key("was binary")),
    )
    .await
    .unwrap();
    assert_eq!(
        one(db, &format!("GET \"{p}was binary\"")).await,
        Cell::Unsupported("0x0001feff".into())
    );
    assert_eq!(
        one(db, &format!("EXISTS \"{p}bin\\xff\\x00key\"")).await,
        Cell::Integer("0".into())
    );
    run(db, &format!("SET \"{p}bin\\xff\" x")).await;
    let shown = page(db, &keys, filter("bin")).await;
    let hexed = format!(
        "0x{}62696eff",
        p.bytes().map(|b| format!("{b:02x}")).collect::<String>()
    );
    delete(db, &keys, &shown, ("key", &hexed)).await.unwrap();
    assert_eq!(
        one(db, &format!("EXISTS \"{p}bin\\xff\"")).await,
        Cell::Integer("0".into())
    );
    // A value too long for a cell is a preview, and a preview is not saved.
    let long = "ż".repeat(40_000);
    insert(
        db,
        &keys,
        &[("key", text(&key("long"))), ("value", text(&long))],
    )
    .await
    .unwrap();
    let shown = page(db, &keys, filter("long")).await;
    let cell = &row(&shown, "key", &key("long"))[4];
    assert!(
        !cell.editable() && cell.display().ends_with("… (78.1 KiB)"),
        "{}",
        cell.display().len()
    );
    assert_eq!(
        code(
            edit(
                db,
                &keys,
                &shown,
                ("key", &key("long")),
                "value",
                text("short")
            )
            .await
        ),
        "redis_value_read_only"
    );
    explained("redis_value_read_only");
    assert_eq!(
        one(db, &format!("STRLEN {p}long")).await,
        Cell::Integer("80000".into())
    );

    // 13. The members of a hash: the list filtered by its key.
    let of = |name: &str| vec![contains("key", &format!("{p}{name}"))];
    let user1 = key("user:1");
    let shown = page(db, &hash_fields, of("user:1")).await;
    assert_eq!(names(&shown), ["key", "field", "value"]);
    let mut got = column(&shown, "field");
    got.sort();
    assert_eq!(got, ["city", "email", "name", "visits"]);
    assert!(column(&shown, "key").iter().all(|name| *name == user1));
    edit(
        db,
        &hash_fields,
        &shown,
        ("field", "email"),
        "value",
        text("jan@nowy.example"),
    )
    .await
    .unwrap();
    assert_eq!(
        code(
            edit(
                db,
                &hash_fields,
                &shown,
                ("field", "email"),
                "value",
                text("stale")
            )
            .await
        ),
        CONFLICT
    );
    edit(
        db,
        &hash_fields,
        &shown,
        ("field", "city"),
        "field",
        text("miasto"),
    )
    .await
    .unwrap();
    assert_eq!(
        code(
            edit(
                db,
                &hash_fields,
                &shown,
                ("field", "visits"),
                "field",
                text("name")
            )
            .await
        ),
        "redis_member_exists"
    );
    assert_eq!(
        code(
            edit(
                db,
                &hash_fields,
                &shown,
                ("field", "visits"),
                "key",
                text("elsewhere")
            )
            .await
        ),
        "redis_member_key_fixed"
    );
    insert(
        db,
        &hash_fields,
        &[
            ("key", text(&user1)),
            ("field", text("phone")),
            ("value", text("600 100 200")),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        code(
            insert(
                db,
                &hash_fields,
                &[
                    ("key", text(&user1)),
                    ("field", text("phone")),
                    ("value", text("x"))
                ]
            )
            .await
        ),
        "redis_member_exists"
    );
    assert_eq!(
        code(
            insert(
                db,
                &hash_fields,
                &[
                    ("key", text(&key("queue"))),
                    ("field", text("f")),
                    ("value", text("v"))
                ]
            )
            .await
        ),
        "redis_wrong_type"
    );
    delete(db, &hash_fields, &shown, ("field", "visits"))
        .await
        .unwrap();
    assert_eq!(
        code(delete(db, &hash_fields, &shown, ("field", "visits")).await),
        CONFLICT
    );
    let stored = run(db, &format!("HGETALL {p}user:1")).await;
    let mut got: Vec<String> = stored
        .rows
        .iter()
        .map(|row| format!("{}={}", row[0].display(), row[1].display()))
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            "email=jan@nowy.example",
            "miasto=Łódź",
            "name=Jan Kowalski",
            "phone=600 100 200"
        ]
    );
    // The filter of a field and of a value.
    let found = page(
        db,
        &hash_fields,
        vec![contains("key", p), contains("field", "MAIL")],
    )
    .await;
    assert_eq!(
        column(&found, "field"),
        ["email"; 1],
        "{label}: {:?}",
        column(&found, "key")
    );
    let found = page(
        db,
        &hash_fields,
        vec![contains("key", p), contains("value", "łódź")],
    )
    .await;
    assert_eq!(column(&found, "field"), ["miasto"]);
    let ordered = page_of(
        db,
        &hash_fields,
        of("user:1"),
        Some(("field", true)),
        500,
        0,
    )
    .await
    .unwrap();
    assert_eq!(
        column(&ordered, "field"),
        ["phone", "name", "miasto", "email"]
    );

    // 14. A list: positions, the element at a position, both ends.
    let elements = format!("{keys} · lists");
    let shown = page(db, &elements, of("queue")).await;
    assert_eq!(names(&shown), ["key", "index", "value"]);
    assert_eq!(column(&shown, "index"), ["0", "1", "2"]);
    assert_eq!(column(&shown, "value"), ["only", "two", "third item"]);
    edit(
        db,
        &elements,
        &shown,
        ("index", "1"),
        "value",
        text("second"),
    )
    .await
    .unwrap();
    assert_eq!(
        code(
            edit(
                db,
                &elements,
                &shown,
                ("index", "1"),
                "value",
                text("stale")
            )
            .await
        ),
        CONFLICT
    );
    assert_eq!(
        code(edit(db, &elements, &shown, ("index", "1"), "index", text("0")).await),
        "redis_column_read_only"
    );
    let queue = key("queue");
    insert(
        db,
        &elements,
        &[("key", text(&queue)), ("value", text("last"))],
    )
    .await
    .unwrap();
    insert(
        db,
        &elements,
        &[
            ("key", text(&queue)),
            ("index", Cell::Integer("0".into())),
            ("value", text("first")),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        code(
            insert(
                db,
                &elements,
                &[
                    ("key", text(&queue)),
                    ("index", text("2")),
                    ("value", text("x"))
                ]
            )
            .await
        ),
        "redis_list_index_invalid"
    );
    // The list moved under the page: what it shows at index 0 is elsewhere.
    assert_eq!(
        code(delete(db, &elements, &shown, ("index", "0")).await),
        CONFLICT,
        "{label}: a stale position"
    );
    let shown = page(db, &elements, of("queue")).await;
    assert_eq!(
        column(&shown, "value"),
        ["first", "only", "second", "third item", "last"]
    );
    delete(db, &elements, &shown, ("index", "1")).await.unwrap();
    assert_eq!(
        column(&run(db, &format!("LRANGE {p}queue 0 -1")).await, "value"),
        ["first", "second", "third item", "last"]
    );

    // 15. A set and a sorted set.
    let members = format!("{keys} · sets");
    let shown = page(db, &members, of("tags")).await;
    assert_eq!(names(&shown), ["key", "member"]);
    let mut got = column(&shown, "member");
    got.sort();
    assert_eq!(got, ["a", "b", "red"]);
    edit(
        db,
        &members,
        &shown,
        ("member", "a"),
        "member",
        text("amber"),
    )
    .await
    .unwrap();
    assert_eq!(
        code(edit(db, &members, &shown, ("member", "a"), "member", text("x")).await),
        CONFLICT
    );
    assert_eq!(
        code(edit(db, &members, &shown, ("member", "b"), "member", text("red")).await),
        "redis_member_exists"
    );
    let tags = key("tags");
    insert(
        db,
        &members,
        &[("key", text(&tags)), ("member", text("green"))],
    )
    .await
    .unwrap();
    assert_eq!(
        code(
            insert(
                db,
                &members,
                &[("key", text(&tags)), ("member", text("green"))]
            )
            .await
        ),
        "redis_member_exists"
    );
    delete(db, &members, &shown, ("member", "b")).await.unwrap();
    assert_eq!(
        code(delete(db, &members, &shown, ("member", "b")).await),
        CONFLICT
    );
    assert_eq!(
        column(&run(db, &format!("SORT {p}tags ALPHA")).await, "value"),
        ["amber", "green", "red"]
    );

    let ranked = format!("{keys} · sorted sets");
    let shown = page(db, &ranked, of("scores")).await;
    assert_eq!(names(&shown), ["key", "member", "score"]);
    assert_eq!(
        column(&shown, "member"),
        ["y", "mid", "x"],
        "{label}: in the order of the scores"
    );
    assert_eq!(row(&shown, "member", "mid")[2], Cell::Float("2.5".into()));
    edit(
        db,
        &ranked,
        &shown,
        ("member", "mid"),
        "score",
        Cell::Float("7".into()),
    )
    .await
    .unwrap();
    assert_eq!(
        code(
            edit(
                db,
                &ranked,
                &shown,
                ("member", "mid"),
                "score",
                Cell::Float("8".into())
            )
            .await
        ),
        CONFLICT
    );
    assert_eq!(
        code(edit(db, &ranked, &shown, ("member", "x"), "score", text("high")).await),
        "redis_score_invalid"
    );
    edit(db, &ranked, &shown, ("member", "y"), "member", text("why"))
        .await
        .unwrap();
    let scores_key = key("scores");
    insert(
        db,
        &ranked,
        &[
            ("key", text(&scores_key)),
            ("member", text("top")),
            ("score", Cell::Float("100".into())),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        code(
            insert(
                db,
                &ranked,
                &[
                    ("key", text(&scores_key)),
                    ("member", text("top")),
                    ("score", Cell::Float("1".into()))
                ]
            )
            .await
        ),
        "redis_member_exists"
    );
    delete(db, &ranked, &shown, ("member", "x")).await.unwrap();
    let stored = run(db, &format!("ZRANGE {p}scores 0 -1 WITHSCORES")).await;
    assert_eq!(column(&stored, "member"), ["why", "mid", "top"]);
    assert_eq!(column(&stored, "score"), ["-1.5", "7", "100"]);
    let by_score = page_of(db, &ranked, of("scores"), Some(("score", true)), 500, 0)
        .await
        .unwrap();
    assert_eq!(column(&by_score, "member"), ["top", "mid", "why"]);

    // 16. A stream: entries are added and removed, never changed.
    let stream = format!("{keys} · streams");
    let shown = page(db, &stream, of("events")).await;
    assert_eq!(names(&shown), ["key", "id", "entry"]);
    assert_eq!(shown.rows.len(), 3);
    assert_eq!(
        shown.rows[0][2].display(),
        r#"{"sensor": "a", "value": "21.5"}"#
    );
    let first_id = shown.rows[0][1].display();
    assert_eq!(
        code(
            edit(
                db,
                &stream,
                &shown,
                ("id", &first_id),
                "entry",
                text(r#"{"sensor": "z"}"#)
            )
            .await
        ),
        "redis_stream_entry_fixed"
    );
    let events_key = key("events");
    insert(
        db,
        &stream,
        &[
            ("key", text(&events_key)),
            ("entry", text(r#"{"sensor": "d", "value": 22}"#)),
        ],
    )
    .await
    .unwrap();
    assert_eq!(
        code(
            insert(
                db,
                &stream,
                &[("key", text(&events_key)), ("entry", text("not json"))]
            )
            .await
        ),
        "redis_entry_invalid"
    );
    assert_eq!(
        code(
            insert(
                db,
                &stream,
                &[
                    ("key", text(&events_key)),
                    ("id", text("1-1")),
                    ("entry", text(r#"{"a": "b"}"#))
                ]
            )
            .await
        ),
        "redis_stream_id_invalid"
    );
    delete(db, &stream, &shown, ("id", &first_id))
        .await
        .unwrap();
    assert_eq!(
        code(delete(db, &stream, &shown, ("id", &first_id)).await),
        CONFLICT
    );
    let shown = page(db, &stream, of("events")).await;
    assert_eq!(shown.rows.len(), 3);
    assert_eq!(
        shown.rows[2][2].display(),
        r#"{"sensor": "d", "value": "22"}"#
    );
    for refusal in [
        "redis_member_exists",
        "redis_member_key_fixed",
        "redis_list_index_invalid",
        "redis_stream_entry_fixed",
        "redis_entry_invalid",
        "redis_score_invalid",
        "redis_column_read_only",
        "redis_key_exists",
        "redis_ttl_invalid",
    ] {
        explained(refusal);
    }

    // 17. A page of the key list is a file that can be read back: its rows
    // go into another database as they are, types and lifetimes included.
    let source = page(db, &keys, vec![contains("key", &format!("{p}sort:"))]).await;
    let copy = format!("db{other}");
    let columns: Vec<String> = names(&source).into_iter().map(str::to_owned).collect();
    let copied = db
        .import_rows(
            &table(&copy),
            &columns,
            &source.rows,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(copied.affected, 3);
    let target = page(db, &copy, vec![contains("key", &format!("{p}sort:"))]).await;
    for name in ["sort:a", "sort:b", "sort:c"] {
        let (from, to) = (
            row(&source, "key", &key(name)),
            row(&target, "key", &key(name)),
        );
        assert_eq!(
            (&from[1], &from[3], &from[4]),
            (&to[1], &to[3], &to[4]),
            "{label}: {name}"
        );
        assert_eq!(matches!(from[2], Cell::Null), matches!(to[2], Cell::Null));
    }

    // A page that holds a preview is not a file of values: a long text, a
    // large collection and a stream are refused, and nothing is written.
    for name in ["long", "events"] {
        let source = page(db, &keys, filter(name)).await;
        assert_eq!(
            code(
                db.import_rows(
                    &table(&copy),
                    &columns,
                    &source.rows,
                    CancellationToken::new()
                )
                .await
            ),
            "redis_value_read_only",
            "{label}: {name}"
        );
        // The same rows as a CSV file holds them: plain text.
        let as_text: Vec<Vec<Cell>> = source
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| match cell {
                        Cell::Null => Cell::Null,
                        other => text(&other.display()),
                    })
                    .collect()
            })
            .collect();
        assert_eq!(
            code(
                db.import_rows(&table(&copy), &columns, &as_text, CancellationToken::new())
                    .await
            ),
            "redis_value_read_only",
            "{label}: {name} from a text file"
        );
    }
    assert!(
        page(db, &copy, vec![contains("key", &format!("{p}long"))])
            .await
            .rows
            .is_empty()
    );

    // 18. The reports of the server.
    let clients = page(db, "clients", vec![]).await;
    assert!(names(&clients).starts_with(&["id", "addr", "name"]));
    assert!(
        clients.rows.len() >= 2,
        "{label}: the connections of this session"
    );
    let ours = page(db, "clients", vec![contains("name", "BAZA")]).await;
    assert!(
        !ours.rows.is_empty()
            && column(&ours, "name")
                .iter()
                .all(|name| name == "baza-studio")
    );
    let databases = page(db, "databases", vec![]).await;
    assert_eq!(
        names(&databases),
        ["database", "keys", "expiring keys", "average ttl ms"]
    );
    let counted: i64 = row(&databases, "database", &keys)[1]
        .display()
        .parse()
        .unwrap();
    assert!(counted >= 20_000, "{label}: {counted} keys counted");
    let report = page(
        db,
        "server info",
        vec![contains("property", "redis_version")],
    )
    .await;
    assert_eq!(report.rows.len(), 1);
    assert_eq!(names(&page(db, "slow log", vec![]).await).len(), 6);
    assert_eq!(
        code(insert(db, "clients", &[("id", text("1"))]).await),
        "result_read_only"
    );

    // 19. The "Monitor" section.
    let stats = db.server_stats().await.unwrap();
    assert_eq!(stats.rows.len(), 1, "{label}: one row of statistics");
    assert_eq!(names(&stats)[..2], ["connections", "RAM"]);
    let stat = |name: &str| stats.rows[0][at(&stats, name)].display();
    assert!(stat("connections").parse::<i64>().unwrap() >= 2);
    assert!(
        !stat("version").is_empty() && stat("version").len() > 14,
        "{}",
        stat("version")
    );
    assert_eq!(stat("database"), keys);
    assert!(stat("keys").parse::<i64>().unwrap() >= 20_000);
    assert!(stat("keyspace").contains(&format!("{keys}: ")));
    assert!(
        stat("clients").contains("baza-studio"),
        "{}",
        stat("clients")
    );

    // 20. A command that waits is stopped, on the server too, and the
    // console goes on.
    let waiting = |db: &Database| {
        let db = db.clone();
        async move {
            column(&page(&db, "clients", vec![]).await, "cmd")
                .iter()
                .filter(|command| command.starts_with("blpop"))
                .count()
        }
    };
    assert_eq!(waiting(db).await, 0);
    let token = CancellationToken::new();
    let (worker, cancel) = (db.clone(), token.clone());
    let slow = format!("BLPOP {p}nothing 60");
    let task = tokio::spawn(async move { worker.query(&slow, cancel).await });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        waiting(db).await,
        1,
        "{label}: the command waits on the server"
    );
    let asked = Instant::now();
    token.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap_or_else(|_| panic!("{label}: the command was not stopped"))
        .unwrap();
    assert_eq!(stopped.unwrap_err().to_string(), "query_cancelled");
    let cancel_ms = asked.elapsed().as_millis();
    assert_eq!(
        one(db, "PING").await,
        text("PONG"),
        "{label}: the console goes on after a cancel"
    );
    let mut left = waiting(db).await;
    for _ in 0..20 {
        if left == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        left = waiting(db).await;
    }
    assert_eq!(
        left, 0,
        "{label}: the stopped command still waits on the server"
    );
    // The console is still in the database of the profile.
    assert_eq!(
        one(db, &format!("EXISTS \"{p}renamed counter\"")).await,
        Cell::Integer("1".into())
    );
    // A token that is already cancelled sends nothing.
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        db.query(&format!("SET {p}never 1"), cancelled)
            .await
            .unwrap_err()
            .to_string(),
        "query_cancelled"
    );
    assert_eq!(
        one(db, &format!("EXISTS {p}never")).await,
        Cell::Integer("0".into())
    );

    // 21. A JSON document, where the server has the type.
    let json = match db
        .query(
            &format!("JSON.SET {p}doc $ '{{\"a\": [1, 2]}}'"),
            CancellationToken::new(),
        )
        .await
    {
        Ok(_) => {
            let shown = page(db, &keys, filter("doc")).await;
            let doc = row(&shown, "key", &key("doc"));
            assert_eq!(doc[1].display(), "ReJSON-RL");
            assert!(
                matches!(&doc[4], Cell::Json(json) if json.contains("[1,2]")),
                "{:?}",
                doc[4]
            );
            edit(
                db,
                &keys,
                &shown,
                ("key", &key("doc")),
                "value",
                Cell::Json(r#"{"a": [1, 2, 3], "b": "x"}"#.into()),
            )
            .await
            .unwrap();
            assert_eq!(
                code(
                    edit(
                        db,
                        &keys,
                        &shown,
                        ("key", &key("doc")),
                        "value",
                        Cell::Json("{}".into())
                    )
                    .await
                ),
                CONFLICT
            );
            assert_eq!(
                one(db, &format!("JSON.GET {p}doc $.b")).await,
                text("[\"x\"]")
            );
            insert(
                db,
                &keys,
                &[
                    ("key", text(&key("doc2"))),
                    ("type", text("json")),
                    ("value", text(r#"[1, {"k": null}]"#)),
                ],
            )
            .await
            .unwrap();
            assert!(
                one(db, &format!("JSON.TYPE {p}doc2 $"))
                    .await
                    .display()
                    .contains("array")
            );
            "JSON documents"
        }
        Err(error) => {
            assert_eq!(error.to_string(), "redis_unknown_command");
            "no JSON type on this server"
        }
    };

    println!(
        "{label} ({}): console (12 commands, 14 replies as grids, 26 refusals, cluster and replica answers in words, transactions, SELECT), listing ({} lists), 20 000 keys imported in {import_ms} ms, 6 rejected imports, 40 pages over the scan cursor, filters, order within its limits, key edits (value, lifetime, name, JSON of 4 types) with conflict checks, insert and delete, bytes kept, members of 5 types, a page read back as a file, 4 reports, monitor ({} columns), cancel in {cancel_ms} ms, {json}",
        stat("version"),
        containers.len(),
        stats.columns.len()
    );
}

/// A TLS endpoint in front of the plain server of the profile, for as long
/// as the test runs: what a hosted service with TLS looks like to the
/// driver. Returns its port on 127.0.0.1.
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
                let Ok(mut encrypted) = acceptor.accept(socket).await else {
                    return;
                };
                let Ok(mut plain) = tokio::net::TcpStream::connect(target).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut encrypted, &mut plain).await;
            });
        }
    });
    port
}

async fn connect(profile: &ConnectionProfile) -> Database {
    Database::connect(profile, None)
        .await
        .unwrap_or_else(|error| panic!("{error}: connecting"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Redis server described by BAZA_DB_TEST_* variables"]
async fn server_passes_the_redis_checklist() {
    let profile = profile();
    let home: i64 = profile.option("database_index").parse().unwrap();
    let other = if home == 0 { 1 } else { home - 1 };
    let prefix = format!(
        "bazaqa:{}:",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let db = connect(&profile).await;
    // The side panel asks for the containers before anything is typed.
    db.tables().await.unwrap();
    // Run on a task of its own, so that a failed assertion still removes
    // the keys of the run.
    let (worker, owner, p) = (db.clone(), profile.clone(), prefix.clone());
    let outcome = tokio::spawn(async move { checklist(&worker, &owner, &p).await }).await;
    // A failed run may have left the console inside a transaction.
    let cleaner = connect(&profile).await;
    cleaner.tables().await.unwrap();
    let removed = sweep(&cleaner, home, &prefix).await + sweep(&cleaner, other, &prefix).await;
    run(&cleaner, &format!("SELECT {home}")).await;
    let left = run(
        &cleaner,
        &format!("SCAN 0 MATCH \"{prefix}*\" COUNT 100000"),
    )
    .await;
    cleaner.close().await;
    db.close().await;
    println!("{removed} keys of the run removed");
    assert!(left.rows.iter().all(|row| matches!(row[1], Cell::Null)));
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
}

/// What the connection dialog does and what it is told when it fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Redis server described by BAZA_DB_TEST_* variables"]
async fn connecting_succeeds_or_says_why() {
    let profile = profile();
    let home: i64 = profile.option("database_index").parse().unwrap();
    // "Test połączenia": one statement on a session of its own. It is
    // answered without moving to another database.
    let probe = connect(&profile).await;
    let answer = probe
        .query("SELECT 1", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(answer.rows[0][0], text("PONG"));
    probe.tables().await.unwrap();
    let info = run(&probe, "CLIENT INFO").await;
    assert_eq!(info.rows[0][at(&info, "db")].display(), home.to_string());
    // Once the session is the application's, SELECT is the command it is.
    assert_eq!(one(&probe, "SELECT 1").await, text("OK"));
    assert_eq!(
        run(&probe, "CLIENT INFO").await.rows[0][at(&info, "db")].display(),
        "1"
    );
    probe.close().await;

    // Both protocol versions give the same grids.
    for protocol in ["resp2", "resp3"] {
        let mut chosen = profile.clone();
        chosen.set_option("protocol", protocol);
        let db = connect(&chosen).await;
        db.tables().await.unwrap();
        let key = format!("bazaqa:protocol:{protocol}");
        run(
            &db,
            &format!("DEL {key}\nHSET {key} a 1 b 2\nZADD {key}:z 1.5 m"),
        )
        .await;
        let hash = run(&db, &format!("HGETALL {key}")).await;
        assert_eq!(names(&hash), ["field", "value"], "{protocol}");
        assert_eq!(hash.rows.len(), 2);
        let scores = run(&db, &format!("ZRANGE {key}:z 0 -1 WITHSCORES")).await;
        assert_eq!(
            (names(&scores), column(&scores, "score")),
            (vec!["member", "score"], vec!["1.5".to_owned()])
        );
        let members = page(
            &db,
            &format!("db{home} · sorted sets"),
            vec![contains("key", &key)],
        )
        .await;
        assert_eq!(members.rows[0][2], Cell::Float("1.5".into()), "{protocol}");
        let info = run(&db, "CLIENT INFO").await;
        assert_eq!(info.rows[0][at(&info, "resp")].display(), &protocol[4..]);
        run(&db, &format!("DEL {key} {key}:z")).await;
        db.close().await;
    }

    let failure = |changed: ConnectionProfile| async move {
        match Database::connect(&changed, None).await {
            Ok(_) => panic!("connected"),
            Err(error) => error.to_string(),
        }
    };
    // A password the server does not accept. A server without one accepts
    // any (its default user is `nopass`), so there is nothing to refuse.
    if !profile.password.is_empty() {
        let mut wrong = profile.clone();
        wrong.password = "not-the-password".into();
        assert_eq!(failure(wrong).await, "database_login_rejected");
        let mut none = profile.clone();
        none.password.clear();
        assert_eq!(failure(none).await, "database_login_rejected");
    }
    // A database the server does not have.
    let mut far = profile.clone();
    far.set_option("database_index", "9999");
    assert_eq!(failure(far).await, "redis_database_index_invalid");
    explained("redis_database_index_invalid");
    // TLS asked of a server that has none, when this one was reached plainly.
    if profile.tls == TlsMode::Disable {
        let mut encrypted = profile.clone();
        encrypted.tls = TlsMode::Require;
        assert_eq!(failure(encrypted).await, "database_tls_unsupported");
    }
    // Nothing listens there.
    let mut closed = profile.clone();
    closed.port = 1;
    assert_eq!(failure(closed).await, "database_network_error");

    // An account of the server's ACL, allowed its own keys and nothing that
    // looks at the server: it signs in with its name, works on its keys and
    // is told in words what it may not do.
    let admin = connect(&profile).await;
    admin.tables().await.unwrap();
    let account = format!("bazaqa-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let secret = uuid::Uuid::new_v4().simple().to_string();
    assert_eq!(
        refused(
            &admin,
            &format!("ACL SETUSER {account} on >{secret} ~bazaqa:* +@all -@dangerous")
        )
        .await,
        "redis_confirm_server"
    );
    run(
        &admin,
        &format!("CONFIRM ACL SETUSER {account} on >{secret} ~bazaqa:* +@all -@dangerous"),
    )
    .await;
    for protocol in ["resp3", "resp2"] {
        let mut limited = profile.clone();
        limited.username = account.clone();
        limited.password = secret.clone();
        limited.set_option("protocol", protocol);
        let db = connect(&limited).await;
        // INFO is not allowed to it: the list holds the database of the profile.
        let containers = db.tables().await.unwrap();
        assert!(
            containers
                .iter()
                .any(|found| found.name == format!("db{home}"))
        );
        let key = format!("bazaqa:acl:{protocol}");
        assert_eq!(one(&db, &format!("SET {key} allowed")).await, text("OK"));
        let shown = page(&db, &format!("db{home}"), vec![contains("key", &key)]).await;
        assert_eq!(row(&shown, "key", &key)[4], text("allowed"));
        edit(
            &db,
            &format!("db{home}"),
            &shown,
            ("key", &key),
            "value",
            text("changed"),
        )
        .await
        .unwrap();
        assert_eq!(one(&db, &format!("GET {key}")).await, text("changed"));
        assert_eq!(
            refused(&db, "SET elsewhere:key 1").await,
            "redis_no_permission"
        );
        assert_eq!(refused(&db, "INFO").await, "redis_no_permission");
        assert_eq!(
            refused(&db, "CONFIRM FLUSHALL").await,
            "redis_no_permission"
        );
        assert_eq!(
            db.server_stats().await.unwrap_err().to_string(),
            "redis_no_permission",
            "{protocol}: the monitor says why it shows nothing"
        );
        explained("redis_no_permission");
        assert_eq!(
            one(&db, &format!("DEL {key}")).await,
            Cell::Integer("1".into())
        );
        db.close().await;
        let mut wrong = limited.clone();
        wrong.password = "not-the-password".into();
        assert_eq!(
            failure(wrong).await,
            "database_login_rejected",
            "{protocol}"
        );
    }
    assert_eq!(
        one(&admin, &format!("CONFIRM ACL DELUSER {account}")).await,
        Cell::Integer("1".into())
    );
    admin.close().await;
    println!(
        "{}: the probe, both protocol versions, {}, wrong database, TLS on a plain port, a closed port, an ACL account in both protocol versions",
        profile.product_label(),
        if profile.password.is_empty() {
            "no password to refuse"
        } else {
            "wrong and missing password"
        }
    );
}

/// The three TLS modes of a profile against a server that speaks TLS.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real Redis server and BAZA_DB_TEST_TLS_CERT, _KEY and _CA"]
async fn tls_is_encrypted_and_verified() {
    let plain = profile();
    let ca = std::path::PathBuf::from(required("BAZA_DB_TEST_TLS_CA"));
    let port = tls_front(&plain).await;
    let attempt = |host: &'static str, tls: TlsMode, ca_file: Option<std::path::PathBuf>| {
        let mut profile = plain.clone();
        profile.host = host.into();
        profile.port = port;
        profile.tls = tls;
        profile.ca_file = ca_file;
        async move {
            match Database::connect(&profile, None).await {
                Ok(db) => {
                    db.tables().await.unwrap();
                    let answer = one(&db, "PING").await.display();
                    db.close().await;
                    answer
                }
                Err(error) => error.to_string(),
            }
        }
    };
    // "Encryption required": encrypted, whoever answers.
    assert_eq!(attempt("localhost", TlsMode::Require, None).await, "PONG");
    assert_eq!(attempt("127.0.0.1", TlsMode::Require, None).await, "PONG");
    // Full verification against the bundled roots: this CA is not one of them.
    assert_eq!(
        attempt("localhost", TlsMode::VerifyFull, None).await,
        "database_certificate_rejected"
    );
    // … against the CA file of the profile: the chain and the name.
    assert_eq!(
        attempt("localhost", TlsMode::VerifyFull, Some(ca.clone())).await,
        "PONG"
    );
    assert_eq!(
        attempt("127.0.0.1", TlsMode::VerifyFull, Some(ca.clone())).await,
        "database_certificate_rejected",
        "the certificate is for another name"
    );
    // A server that only speaks TLS, asked without it: said in words.
    assert_eq!(
        attempt("localhost", TlsMode::Disable, None).await,
        "redis_handshake_failed"
    );
    explained("redis_handshake_failed");
    // The whole work of a session through TLS.
    let mut profile = plain.clone();
    profile.host = "localhost".into();
    profile.port = port;
    profile.tls = TlsMode::VerifyFull;
    profile.ca_file = Some(ca);
    let db = connect(&profile).await;
    db.tables().await.unwrap();
    let home: i64 = profile.option("database_index").parse().unwrap();
    let prefix = format!(
        "bazaqa:tls:{}:",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let big = "x".repeat(50_000);
    run(&db, &format!("SET {prefix}a {big}\nHSET {prefix}h f v")).await;
    let shown = page(&db, &format!("db{home}"), vec![contains("key", &prefix)]).await;
    assert_eq!(shown.rows.len(), 2);
    assert_eq!(row(&shown, "key", &format!("{prefix}a"))[4], text(&big));
    assert_eq!(db.server_stats().await.unwrap().rows.len(), 1);
    assert_eq!(sweep(&db, home, &prefix).await, 2);
    db.close().await;
    println!(
        "{}: TLS without verification, with the bundled roots (rejected), with the CA file (accepted for its name only), a plain connection to a TLS port explained, a session through TLS",
        plain.product_label()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs a real SSH server and a Redis server: BAZA_SSH_TEST_* and BAZA_DB_TEST_*"]
async fn redis_through_an_ssh_tunnel() {
    let mut profile = profile();
    let ssh = SshProfile {
        host: variable("BAZA_SSH_TEST_HOST").unwrap_or_else(|| "127.0.0.1".into()),
        port: required("BAZA_SSH_TEST_PORT").parse().unwrap(),
        username: required("BAZA_SSH_TEST_USER"),
        auth: SshAuth::PrivateKey,
        password: None,
        private_key: Some(std::fs::read_to_string(required("BAZA_SSH_TEST_KEY")).unwrap()),
        passphrase: variable("BAZA_SSH_TEST_KEY_PASSPHRASE"),
        fingerprint: String::new(),
    };
    profile.ssh = Some(ssh.clone());
    let home: i64 = profile.option("database_index").parse().unwrap();
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
    db.tables().await.unwrap();
    let prefix = format!(
        "bazaqa:ssh:{}:",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let keys = format!("db{home}");
    run(
        &db,
        &format!("SET {prefix}a \"through the tunnel\"\nHSET {prefix}h f v\nRPUSH {prefix}l x y"),
    )
    .await;
    let shown = page(&db, &keys, vec![contains("key", &prefix)]).await;
    assert_eq!(shown.rows.len(), 3);
    let name = format!("{prefix}a");
    edit(
        &db,
        &keys,
        &shown,
        ("key", &name),
        "value",
        text("edited through the tunnel"),
    )
    .await
    .unwrap();
    assert_eq!(
        code(edit(&db, &keys, &shown, ("key", &name), "value", text("stale")).await),
        CONFLICT
    );
    assert_eq!(
        one(&db, &format!("GET {prefix}a")).await,
        text("edited through the tunnel")
    );
    let fields = page(
        &db,
        &format!("{keys} · hashes"),
        vec![contains("key", &prefix)],
    )
    .await;
    assert_eq!(fields.rows.len(), 1);
    let stats = db.server_stats().await.unwrap();
    assert_eq!(stats.rows.len(), 1);
    // The server sees the connections come from the SSH server's side, and
    // the tunnel carried them.
    let after = manager.status(profile.id).unwrap();
    assert!(after.bytes_sent > before.bytes_sent && after.bytes_received > before.bytes_received);
    assert!(
        after.active_clients >= 1 && after.error.is_none(),
        "{:?}",
        after.error
    );
    // A command that waits is stopped through the tunnel as well.
    let token = CancellationToken::new();
    let (worker, cancel) = (db.clone(), token.clone());
    let slow = format!("BLPOP {prefix}nothing 60");
    let task = tokio::spawn(async move { worker.query(&slow, cancel).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    token.cancel();
    let stopped = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stopped.unwrap_err().to_string(), "query_cancelled");
    let mut waiting = usize::MAX;
    for _ in 0..20 {
        waiting = column(&page(&db, "clients", vec![]).await, "cmd")
            .iter()
            .filter(|command| command.starts_with("blpop"))
            .count();
        if waiting == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(waiting, 0, "the stopped command still waits on the server");
    let removed = sweep(&db, home, &prefix).await;
    assert_eq!(removed, 3);
    // TLS inside the tunnel is asked of the server by its own name: a plain
    // server says it has none, in words, not "the network was lost".
    let mut encrypted = profile.clone();
    encrypted.tls = TlsMode::VerifyFull;
    match Database::connect(&encrypted, Some(local)).await {
        Ok(opened) => opened.close().await,
        Err(error) => assert!(
            matches!(
                error.to_string().as_str(),
                "database_tls_unsupported" | "database_certificate_rejected"
            ),
            "{error}"
        ),
    }
    db.close().await;
    // TLS inside the tunnel: the driver dials the loopback end of the
    // tunnel and still verifies the certificate against the name of the
    // server in the profile.
    let tls = if variable("BAZA_DB_TEST_TLS_CERT").is_some() {
        let mut plain = profile.clone();
        plain.ssh = None;
        let front = tls_front(&plain).await;
        let mut secure = profile.clone();
        secure.id = uuid::Uuid::new_v4();
        secure.host = "localhost".into();
        secure.port = front;
        secure.tls = TlsMode::VerifyFull;
        secure.ca_file = Some(required("BAZA_DB_TEST_TLS_CA").into());
        tokio::time::sleep(Duration::from_secs(20)).await;
        let second = manager
            .start(&TunnelProfile {
                id: secure.id,
                name: "TLS inside the tunnel".into(),
                ssh: secure.ssh.clone().unwrap(),
                target_host: "127.0.0.1".into(),
                target_port: front,
                local_port: 0,
            })
            .await
            .unwrap();
        let end = second.local_port.unwrap();
        assert_ne!(end, front);
        let db = Database::connect(&secure, Some(end)).await.unwrap();
        db.tables().await.unwrap();
        assert_eq!(one(&db, "PING").await, text("PONG"));
        db.close().await;
        // The same tunnel, a profile that names the server otherwise: the
        // certificate is not for that name.
        let mut renamed = secure.clone();
        renamed.host = "cache.example.test".into();
        assert_eq!(
            Database::connect(&renamed, Some(end))
                .await
                .err()
                .unwrap()
                .to_string(),
            "database_certificate_rejected"
        );
        "; TLS inside the tunnel verified against the name of the profile"
    } else {
        ""
    };
    println!(
        "{} over 127.0.0.1:{local}: console, key list, cell edit with conflict check, members, monitor ({} columns), cancel; tunnel sent {} B, received {} B{tls}",
        profile.product_label(),
        stats.columns.len(),
        after.bytes_sent - before.bytes_sent,
        after.bytes_received - before.bytes_received
    );
    manager.shutdown().await;
}
