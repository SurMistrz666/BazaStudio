# Database drivers

An engine family is one driver: one module in this folder with a `pub static SPEC: EngineSpec`, and its name on one line of `engines!` in `mod.rs`. Nothing else in the workspace names an engine. The interface, the SQL helpers, the vault and the profile files read the descriptor.

A product (Amazon Aurora PostgreSQL, TiDB, …) is not a driver. It is a preset of a family: a `ProductSpec` in the family's `products` list.

## What you may touch

| File | What you add |
| --- | --- |
| `drivers/<engine>.rs` (or `drivers/<engine>/mod.rs` with files of its own) | the driver, new |
| `drivers/mod.rs` | one line in `engines! { … }` |
| `native/Cargo.toml`, `crates/baza-core/Cargo.toml` | dependency lines, when the driver needs a crate |
| `native/Cargo.lock` | whatever `cargo build` writes |
| `tests/<engine>_checklist.rs` | new, only for an engine without SQL |
| the "Verified against" table below | one row per product |

`mod.rs` and the two `Cargo.toml` files merge by union (`.gitattributes`): branches that add different engines merge without a conflict. After a merge of `Cargo.lock`, take either side and run `cargo build` once without `--locked`; never edit it by hand.

Everything else is shared: `engine.rs`, `session.rs`, `database.rs`, `sql.rs`, `builder.rs`, `model.rs`, `profiles.rs` and all of `baza-studio`. If the driver cannot be written without changing one of them, the contract has a gap. Say so and change the contract in a commit of its own, for every engine at once; do not add a branch for your engine.

`THIRD-PARTY-NOTICES.txt`, `THIRD-PARTY-INVENTORY.json` and `third-party-licenses/` are regenerated later, once for all new crates.

## The descriptor (`engine.rs`)

```rust
pub static SPEC: EngineSpec = EngineSpec {
    id: "redis",                    // stored in profiles, never changes: a-z 0-9 _ -
    aliases: &["valkey"],           // other names accepted in imported profile files
    portable_name: "redis",         // name written into exported profile files
    label: "Redis",
    available: true,
    form: ConnectionForm { database: FieldUse::Hidden, ..ConnectionForm::SERVER },
    capabilities: Capabilities { … },
    dialect: Dialect::ANSI,         // or the SQL dialect of the engine
    options: &[OptionField { key: "database_index", … }],
    products: &[ProductSpec { id: "redis", label: "Redis", …, ..ProductSpec::GENERIC }, …],
    connect,                        // fn(&ConnectRequest) -> BoxFuture<Result<Arc<dyn Session>>>
    describe: Some(describe),       // sentences for the driver's own result codes
};
pub const ENGINE: Engine = Engine::new(&SPEC);
```

`products[0]` is the generic product: the one a profile without a product id belongs to. Product ids are unique in the whole registry. A product carries its label, picker group (`Group::{Relational, Cloud, Analytics, KeyValue}`), badge (one or two letters on a colour, painted by the interface, no image needed), default port, TLS mode, database and user name, search keywords, an optional note shown under the picker, and, only where it differs from the family, its own `capabilities` and `dialect`.

`ConnectionForm` says which standard fields the dialog shows: `address` (`Server`: host and port; `File`: a local file, stored in `ConnectionProfile::sqlite_path` whatever the engine), `database`, `username`, `password` (`Required`, `Optional`, `Hidden`), `tls` and `ssh` (whether those tabs apply), `database_label` and `file_extensions`. `ConnectionProfile::validate` checks a profile against it.

Fields of your own are `OptionField`s: `key`, a Polish and an English `label`, `kind` (`Text`, `Number { min, max }`, `Flag`, `Choice(&[…])`), `default`, `placeholder`, optional `help`, `required`. The dialog renders them under the standard fields; the value is stored as text in `ConnectionProfile::options` and read with `request.option("key")`, which returns the default when nothing is stored. Never put a secret into an option: passwords, tokens and keys go into `ConnectionProfile::password`.

## What `Capabilities` switch in the interface

| Field | What the interface offers when it is set |
| --- | --- |
| `language` | `Sql`: the SQL editor, statement splitting, editing of a plain `SELECT *` result, SQL export. `Command`: the same section named "Konsola", started empty. `None`: the section is greyed out |
| `browse` | names in the side panel and messages: `Tables`, `Documents` (collections), `Keys` |
| `schemas` | the schema level of the side panel tree. Without it the containers hang directly under the connection and `TableRef::schema` is empty |
| `filter`, `sort` | the filter field and the sorting headers of "Dane tabeli" |
| `edit_cells`, `insert_rows`, `delete_rows` | in-place editing, "Wiersz" and "Usuń wiersz" in the grid |
| `table_designer` | "Nowa tabela" everywhere |
| `query_builder` | the "Kreator zapytań" section and its links |
| `import`, `export` | "Import danych", "Eksportuj wynik", the "Import i eksport" section |
| `monitor` | the "Monitor" section |

A missing capability greys the control out and says why; nothing calls the driver for it. Start from `Capabilities::SQL` or `Capabilities::NONE` and be honest: a flag that is set promises that the checklist below passes for it.

## The session (`session.rs`)

`connect` returns an `Arc<dyn Session>`. Methods return `BoxFuture`; write them as `Box::pin(async move { … })`.

```rust
pub trait Session: Send + Sync {
    // required
    fn dialect(&self) -> &Dialect;
    fn execute<'a>(&'a self, text: &'a str, cancel: CancellationToken) -> BoxFuture<'a, Result<QueryResult>>;
    fn containers<'a>(&'a self) -> BoxFuture<'a, Result<Vec<TableRef>>>;
    fn columns<'a>(&'a self, container: &'a TableRef) -> BoxFuture<'a, Result<Vec<ColumnInfo>>>;
    fn server_stats<'a>(&'a self) -> BoxFuture<'a, Result<QueryResult>>;
    fn close<'a>(&'a self) -> BoxFuture<'a, ()>;
    // with a default
    fn execute_one<'a>(&'a self, statement: &'a str) -> BoxFuture<'a, Result<()>>;        // operation_unsupported
    fn page_text(&self, page: &PageRequest) -> Result<String>;                            // SELECT from the dialect
    fn read_page<'a>(&'a self, page: &'a PageRequest, cancel: CancellationToken) -> BoxFuture<'a, Result<QueryResult>>;
    fn change_text(&self, change: &RowChange) -> Result<String>;                          // UPDATE / INSERT / DELETE
    fn apply<'a>(&'a self, change: &'a RowChange) -> BoxFuture<'a, Result<()>>;           // execute_one(change_text)
    fn import_rows<'a>(&'a self, container: &'a TableRef, columns: &'a [String], rows: &'a [Vec<Cell>],
                       cancel: CancellationToken) -> BoxFuture<'a, Result<QueryResult>>;  // operation_unsupported
}
```

An engine with SQL implements the required methods, `execute_one` (one statement in a transaction that is committed only when exactly one row changed, otherwise `row_changed_reload_required`) and `import_rows` (all rows or none). Pages and grid changes then come from its `Dialect`.

An engine without SQL also overrides `page_text` and `read_page` (filters, order, limit, offset of a `PageRequest`), `change_text` and `apply` (`RowChange::{UpdateCell, InsertRow, DeleteRow}`). `UpdateCell` carries the old value: change the record only while it still holds it, otherwise `row_changed_reload_required`. The texts are what the tab and the history show.

Rules every driver keeps:

- `execute` honours `cancel` (`query_cancelled`), ends after 120 s (`query_timeout`), stops at `sql::MAX_RESULT_ROWS` rows or `sql::MAX_RESULT_BYTES` bytes with `truncated` set, and stops the statement on the server too, not only on this side.
- Errors are short stable codes (`anyhow!("database_login_rejected")`), never the server's text, which can repeat an address or an account. Use the existing codes where they fit: `database_login_rejected`, `database_not_found`, `database_access_denied`, `database_certificate_rejected`, `database_tls_unsupported`, `database_network_error`, `database_connection_timeout`, `database_error [SQLSTATE]`. Codes of your own start with the engine id and get a Polish and an English sentence in `SPEC.describe`.
- `TableRef::kind` is `table` (or contains `table`) for a container whose records can be edited, `view` for a read-only one, `materialized view`, anything else lands under "Inne obiekty". `ColumnInfo::primary_key` marks the key of a record: without a complete key the grid does not edit.
- Values become `Cell`s: `Integer`, `Decimal` and `Float` keep the text of the number (no precision is lost), `Json`, `Bytes`, `Text`, `Bool`, `Null`; what the driver cannot read is `Unsupported("<TYPE>")`, which the grid shows and never edits.
- `server_stats` is one light row, asked every 5 s. Columns `connections`, `active`, `max_connections` and `tables` have translated names, `version` and `database` describe the server; any other column is shown under the name you give it.

## Connecting

```rust
pub struct ConnectRequest<'a> {
    pub profile: &'a ConnectionProfile,     // validated: account, database, TLS mode, CA file, options
    pub product: &'static ProductSpec,
    pub forwarded: Option<SocketAddr>,      // loopback end of an SSH tunnel, when the profile has one
}
```

Tunnels and TLS are generic. Open the TCP connection to `request.dial_host()` / `request.dial_port()`; use `request.server_name()` (always the host of the profile) for SNI, for certificate verification and for anything the protocol sends. A driver whose client library takes only one host needs the same kind of dial-address override the vendored SQLx drivers have (`vendor/SOURCE-PATCH.md`); until it has one, set `form.ssh: false`.

Honour `profile.tls`: `Disable`, `Require` (encrypt, do not verify), `VerifyFull` (verify the chain and the name against the bundled Mozilla roots, or against `profile.ca_file` when it is set).

Where servers of one family differ (catalogs, how a statement is stopped, literals), ask the server what it is after connecting and keep the answer in the session, as `postgres.rs` and `mysql.rs` do with their `Flavor`. A profile of the generic product may point at any server of the family, so the product id is only the hint when the server would not say.

## The dialect (`engine.rs`)

`Dialect` is data: `name_quotes`, `text` (`Standard`, `National`, `PostgresEscape`, `Backslash`, `MysqlHex`), `bytes`, `booleans`, `paging` (`LimitOffset`, `OffsetFetch { default_order }`, `Top`, `None`), `contains` (cast, operator and pattern wrap of the "contains" filter), `parameters`, `lexer` (where a literal, a quoted name and a comment end), `empty_insert`, `default_schema`, `column_types` (what the table designer offers; the first is the type of a new key column). Start from `..Dialect::ANSI`.

Two things the registry test checks for you, and two it cannot:

- The `lexer` must know the quotes `name_quotes` writes (`bracket_names`, `backtick_names`), or a page of a table is not read back as one editable statement.
- A `default_order` of `OffsetFetch` must not contain a subquery: a `SELECT` inside makes the page read-only. An ordinal such as `1` works.
- A text literal must take the collation of the column it is compared with, or the conflict check of a cell edit fails on columns with another collation.
- The "contains" filter ignores letter case and searches numbers as text on every engine. Where `LIKE` does not, use `cast` and `pattern` (TiDB folds both sides).

If SQL of your engine cannot be described by the fields that exist, add a field or a variant for every engine in a commit of its own; never branch on an engine in `sql.rs` or `builder.rs`.

## Tests

- `cargo test -p baza-core` runs `engine::tests::the_registry_is_consistent` over every registered engine: unique ids, a listed product, option defaults, and for every SQL dialect that the first page of a table is one statement that reads back as editable.
- The live checklist is `tests/driver_checklist.rs`, ignored by default and described by `BAZA_DB_TEST_*` (its header lists them): the designer's column types, listing, columns, import (all or nothing), paging, filters, a cell edit with its conflict check, insert, delete, the builder, the monitor and stopping a statement, checked on the server too. A new engine with SQL passes it as it is; give its slow statement in `BAZA_DB_TEST_SLOW_SQL`.

  ```text
  BAZA_DB_TEST_ENGINE=postgres BAZA_DB_TEST_PRODUCT=cockroachdb BAZA_DB_TEST_PORT=26257 \
  BAZA_DB_TEST_USER=root BAZA_DB_TEST_DATABASE=defaultdb \
  cargo test -p baza-core --test driver_checklist -- --ignored --nocapture
  ```

- An engine without SQL gets `tests/<engine>_checklist.rs` built from the same steps, env-driven and `#[ignore]` in the same way.
- A database through the SSH tab of its connection: `cargo test -p baza-studio over_ssh -- --ignored --nocapture` with `BAZA_SSH_TEST_*` and `BAZA_DB_TEST_*`.
- To look at the interface on a real server, run a self-test with `BAZA_QA_CONNECTIONS_FILE`, `BAZA_QA_CONNECTION` and `BAZA_QA_TABLE` (top of `baza-studio/src/app/qa.rs`). Never start the application without a QA switch: a plain launch opens the real vault.

## Verified against

"live" means the checklist above passed against that server; "preset" means the product is a label, a badge and defaults over a driver that was verified on other servers, and nobody connected to the service itself.

| Product | Family | State | Notes |
| --- | --- | --- | --- |
| PostgreSQL | postgres | live: 17 | also through an SSH tunnel |
| TimescaleDB | postgres | live: on PostgreSQL 17 | the `_timescaledb_*` schemas are hidden from the side panel |
| CockroachDB | postgres | live: v26.3 | no `pg_cancel_backend`: a statement is stopped with `CANCEL SESSION`; the monitor reads `SHOW CLUSTER SESSIONS`; a hidden `rowid` key means read-only |
| YugabyteDB | postgres | preset | port 5433 |
| Greenplum | postgres | preset | its internal schemas are hidden |
| Amazon RDS for PostgreSQL | postgres | preset | TLS "required": full verification needs the AWS CA file |
| Amazon Aurora PostgreSQL | postgres | preset | as RDS |
| Azure Database for PostgreSQL | postgres | preset | |
| Google Cloud SQL for PostgreSQL | postgres | preset | TLS "required": full verification needs the instance's CA file |
| Supabase | postgres | preset | nothing is prepared, so the transaction pooler (6543) is expected to work |
| Neon | postgres | preset | |
| Amazon Redshift | postgres | preset, NOT VERIFIED | own dialect and catalog queries written from the documentation: `svv_tables` and `svv_columns` with the standard views as fallback, backslash-escaping literals, `FROM_HEX`, `VARCHAR(65535)` cast in filters, no `pg_stat_activity` in the monitor |
| MySQL | mysql | live: 8.4, with and without TLS | also through an SSH tunnel |
| MariaDB | mysql | live: 11 | also through an SSH tunnel |
| MariaDB / MySQL (profiles of earlier versions) | mysql | live | the same driver; not offered for new connections |
| TiDB | mysql | live: v7.5 | byte-wise collation by default: the filter folds both sides |
| Percona Server for MySQL | mysql | preset | |
| Amazon RDS for MySQL / MariaDB, Aurora MySQL | mysql | preset | TLS "required" |
| Azure Database for MySQL | mysql | preset | |
| Google Cloud SQL for MySQL | mysql | preset | TLS "required" |
| PlanetScale | mysql | preset | Vitess: the monitor reports the version only |
| SingleStore | mysql | preset | `_utf8mb4 0x…` literals were not tried there |
| OceanBase | mysql | preset | MySQL mode, port 2881 |
| SQLite | sqlite | live: every test run | |
| Azure SQL Edge | mssql | live: 15.0 (the SQL Server 2019 engine, ARM64), without TLS, without verification and fully verified against a CA file | also through an SSH tunnel; the edition has no CLR types (`geography`, `geometry`, `hierarchyid`) and no TDS 8.0, so those two were not seen working |
| Microsoft SQL Server | mssql | preset | the same engine as Azure SQL Edge, and the checklist passes with this product against that server; no server on Windows, no 2022 or 2025 and no strict encryption (TDS 8.0) was reached. SQL login only: no Windows or Entra ID sign-in, no named instances |
| Azure SQL Database | mssql | preset | written from the documentation and not tried: the redirect a gateway answers the sign-in with is followed once (never through an SSH tunnel), and the monitor counts sessions with `VIEW DATABASE STATE` |
| Azure SQL Managed Instance | mssql | preset | the public endpoint (port 3342) is a note of the product |
| Amazon RDS for SQL Server | mssql | preset | TLS "required": full verification needs the AWS CA file |
| Redis | redis | live: 8, RESP3 and RESP2 | `tests/redis_checklist.rs`; also through an SSH tunnel, and with TLS (all three modes, a TLS endpoint in front of the server); JSON documents edited where the server has the type |
| Valkey | redis | live: 8.1, RESP3 and RESP2 | the same checklist, with TLS as above |
| KeyDB | redis | preset | reports the Redis version it is compatible with |
| Dragonfly | redis | preset | told apart by `dragonfly_version` in `INFO` |
| Amazon ElastiCache | redis | preset | TLS with full verification by default; cluster mode is not followed: one node, the keys of the others answer `MOVED` |
| Amazon MemoryDB | redis | preset | always cluster mode: complete with one shard only |
| Azure Cache for Redis | redis | preset | port 6380 with TLS; the access key is the password; no Entra ID sign-in |
| Google Memorystore | redis | preset | TLS off by default; with it, port 6378 and the CA file of the instance |
| Upstash | redis | preset | TLS with full verification; password only |
| MongoDB | mongodb | live: 8.3, stand-alone and a replica set, with and without TLS | no SQL: `tests/mongodb_checklist.rs`; also through an SSH tunnel, where the driver makes the TLS connection itself |
| MongoDB Atlas | mongodb | preset | a DNS seed list (the `srv` option), which does not go through a tunnel |
| Amazon DocumentDB | mongodb | preset | TLS "required": full verification needs Amazon's CA file; no retryable writes; the "contains" filter searches text fields only |
| Azure Cosmos DB for MongoDB | mongodb | preset | port 10255; no retryable writes; the "contains" filter searches text fields only; the monitor falls back to the version when `serverStatus` is refused |
| FerretDB | mongodb | preset | told apart by `buildInfo`; the "contains" filter searches text fields only |
| ClickHouse | clickhouse | live: 26.9 over the HTTP interface, without TLS and, through a TLS endpoint in front of the server, without verification and fully verified against a CA file | `tests/clickhouse_checklist.rs`: no table designer (a table needs an engine and a sorting key, and `Nullable` is part of a type), and a duplicate key does not fail an import, because a key is not unique. A cell is changed with `ALTER TABLE … UPDATE` and a row removed with a lightweight `DELETE`, when the key finds exactly one row. Also through an SSH tunnel, and with an account that may not change settings (`readonly = 1`) |
| ClickHouse Cloud | clickhouse | preset | port 8443 and full verification by default. Nobody connected to the service: replicated tables, a session of a script behind the load balancer and the wake-up of an idle service were not seen |
| DuckDB | duckdb | live: 1.5.6, the bundled library, with every test run | `tests/duckdb_checklist.rs` (the steps of the driver checklist, on a file and in memory); CSV, Parquet and JSON files read from a statement. One file is one database for every connection of the process. No extension is installed by the application, so MotherDuck is not reached and time zones other than UTC need `INSTALL icu` |
| Apache Cassandra | cassandra | live: 5.0.9 | `tests/cassandra_checklist.rs` (the SQL checklist step by step; the table designer and the query builder are off, because the shared generators write `NOT NULL`, joins, `LIKE` and `GROUP BY`); also through an SSH tunnel, with TLS (all three modes, a TLS endpoint in front of the node) and with `PasswordAuthenticator`. One node per connection: the driver is pinned to the address of the profile. No `OFFSET`: pages are read forward from remembered paging states; a filter is an exact partition key first, otherwise up to 100 000 rows a page checked by the driver; an order is made by the driver on up to 20 000 rows. Grid changes are lightweight transactions; counters are read only; a request in flight cannot be stopped on the server (CQL has no cancel) |
| ScyllaDB | cassandra | preset | the same driver (the `scylla` crate is ScyllaDB's own); the per-shard port is not used; told apart by `system.versions`; lightweight transactions in keyspaces with tablets depend on the server version |
| Amazon Keyspaces | cassandra | preset | port 9142, TLS with full verification, service-specific credentials only (no SigV4 sign-in); the notes of the product come from the AWS documentation |
| Azure Managed Instance for Apache Cassandra | cassandra | preset | TLS without verification by default (the nodes carry certificates of the cluster's own authority) |
| DataStax Astra DB | cassandra | preset, cannot connect | needs a secure connect bundle (client certificate, SNI proxy), which is not read: a connection is refused with `cassandra_astra_unsupported` before anything is dialled |
| DynamoDB Local | dynamodb | live: 3.3.1 | `tests/dynamodb_checklist.rs` (the checklist for a store of items); also through an SSH tunnel and over HTTPS (all three modes, a TLS endpoint in front of the emulator). It does not check signatures: Signature Version 4 is checked against the published AWS test vectors in `dynamodb/sigv4.rs` |
| Amazon DynamoDB | dynamodb | preset | the service itself was not connected to. Host and Port are not used: the endpoint follows from the region, or from the "endpoint" option. No `OFFSET` and no search: a page is a `Scan` read forward from the last key, or a `Query` when the filter names the partition key; one page, one filter, one order and one PartiQL statement each read at most 16 calls of 1 MB. The session token is an engine option, stored like one |
| ScyllaDB Alternator | dynamodb | preset | written from its documentation: without `TransactWriteItems` an import falls back to conditional `PutItem`s, and without PartiQL the editor still sends API operations by name and JSON body |
| Oracle Database Free / XE | oracle | live: Oracle AI Database 26ai Free 23.26.3, as an ordinary account and as SYSTEM, by service name, by SID and as SYSDBA, without TLS | `tests/oracle_checklist.rs`: the steps of the driver checklist with the one row whose text is empty expected back as NULL, which is what Oracle stores (`driver_checklist.rs` stops at that line). Also through an SSH tunnel. TLS was checked through an endpoint of the test in front of the listener: without verification, fully verified against a CA file, and with the second handshake a TCPS listener asks for when it hands a connection over; no TCPS listener of Oracle itself was reached. A statement that computes is stopped on the server at once; one that only waits (`DBMS_SESSION.SLEEP`, a row lock) is given up at once and ends on the server when its wait does. The client library is a beta: a `TIMESTAMP WITH TIME ZONE` with a region name, an object type and `XMLTYPE` are shown in "Dane tabeli" and refused in the editor with a sentence. XE 11g is refused by name: the client speaks to 12.1 and later only |
| Oracle Database | oracle | preset | the same driver, and the product of a profile that names none. Versions 12.1 to 21c were not reached: the designer leaves out `BOOLEAN` before 23ai and `JSON` before 21c, a flag is written as `1` and `0` there, and the `SELECT 1` of the connection test is sent as `SELECT 1 FROM dual`. A redirect of the listener (the SCAN listener of a RAC cluster, a connection manager) is followed by the driver itself, checked against a redirecting listener of the test in front of the real one; no cluster was reached, and through an SSH tunnel a redirect is refused by name. A server that requires Oracle native network encryption is refused by name. Sign-in by user name and password only: no wallet, Kerberos or token |
| Oracle Autonomous Database | oracle | preset, NOT VERIFIED | TLS without a wallet on port 1521 with full verification against the bundled roots; nobody connected to the service, whose listener is expected to redirect and to ask for a second TLS handshake, both of which were only seen working against listeners of the test. A wallet (mTLS, port 1522) is not supported |
| Amazon RDS for Oracle | oracle | preset | port 1521 without encryption by default; the SSL option listens on 2484 and full verification needs the AWS CA file; `NATIVE_NETWORK_ENCRYPTION` set to `REQUIRED` is refused by name; the master user has no SYSDBA |
| Elasticsearch | elasticsearch | live: 9.5.5, 8.19.23 and 7.17.29 | `tests/elasticsearch_checklist.rs` (the SQL checklist step by step; the table designer and the query builder are off, because an index is a mapping and a query is not SQL with joins: both are typed in the console, as requests of Kibana Dev Tools or as the SQL of the server). With the TLS of the node itself on 8.x and 9.x (without verification, verified against its CA file, another CA and another name rejected) and without TLS on 7.x; sign-in by user and password and by API key; also through an SSH tunnel and behind a proxy under a path prefix. Indices are tables; aliases, data streams and closed indices are listed and read only. No offset: a page is a search read on with `search_after` inside a point in time, ordered by `_shard_doc`; a page nobody visited is reached by passing up to 2 000 000 documents; a cluster that opens no point in time is read without one. "Contains" is the query the type of the field answers (words of a text, a fragment of a keyword, a value or an interval of numbers and dates). A cell is changed with `_update` and `if_seq_no`, which makes the server write the whole `_source` anew in its own notation of numbers. An import is a `_bulk` of `create` that is deleted again when a document is refused: an index has no transactions, so a search in between may have seen the documents |
| OpenSearch | elasticsearch | live: 2.19.6, without its security plugin | the same checklist; TLS (all three modes) through a TLS endpoint in front of the server, an SSH tunnel, a proxy under a path prefix. It has no `_shard_doc`: pages are ordered by `_doc` inside a point in time, and the driver takes care of the positions two shards share. SQL is the plugin (`_plugins/_sql`). Sign-in through the security plugin (HTTP Basic) was not seen working, because the test server has none |
| Amazon OpenSearch Service | elasticsearch | preset | the service was not connected to. Sign-in only as the master user or an internal user of fine-grained access control (HTTP Basic): requests are not signed with IAM keys (SigV4), and OpenSearch Serverless collections are not supported. Domains that still run Elasticsearch (7.10 and older) are the Elasticsearch side of the driver with the SQL of Open Distro, which was not tried |
| Elastic Cloud | elasticsearch | preset | the service was not connected to. Port 443 and full verification by default; an API key in the password field, or the user `elastic`. A whole `https://…` address may be pasted into Host (not through a tunnel). Serverless projects were not tried: the parts of the monitor they do not offer are left out |

Engines not implemented yet: libSQL / Turso (another protocol), Snowflake and BigQuery (no local test server; need accounts).
