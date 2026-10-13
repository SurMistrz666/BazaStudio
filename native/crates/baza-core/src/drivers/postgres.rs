//! PostgreSQL and the servers and services that speak its protocol, through
//! SQLx.
//!
//! The products are presets over this one driver. Where a server differs
//! from PostgreSQL in something the driver does (its catalogs, how a running
//! statement is stopped, how a literal is written), it is told apart by the
//! answer to `SELECT version()` ([`Flavor`]), not by the product the user
//! picked: a profile saved as "PostgreSQL" that points at CockroachDB works
//! as well.
//!
//! Every statement goes through the simple query protocol. Nothing is
//! prepared, so a connection pooler in transaction mode (PgBouncer, the
//! poolers of Supabase and Neon) is not a problem.
use super::Session;
use super::sqlx_support::{
    connect_error, containers_of, decimal_text, execute_exactly_one, insert_rows, run_statement,
    safe_db_error,
};
use crate::engine::{
    Badge, Capabilities, ConnectRequest, ConnectionForm, ContainsFilter, DefaultSchema, Dialect,
    EmptyInsert, Engine, EngineSpec, FieldUse, Group, Lexer, Paging, ParameterStyle, ProductSpec,
    TextLiteral,
};
use crate::model::{Cell, ColumnInfo, QueryResult, TableRef, TlsMode};
use crate::sql::literal;
use anyhow::Result;
use futures_util::future::BoxFuture;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgRow, PgSslMode};
use sqlx::{AssertSqlSafe, Column, ConnectOptions, Row, TypeInfo, ValueRef};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const ENGINE: Engine = Engine::new(&SPEC);

pub static SPEC: EngineSpec = EngineSpec {
    id: "postgres",
    aliases: &["postgresql"],
    portable_name: "postgres",
    label: "PostgreSQL",
    available: true,
    // PostgreSQL always connects to one database; an empty field means the
    // default one of the product (`postgres` on a plain server).
    form: ConnectionForm {
        database: FieldUse::Optional,
        ..ConnectionForm::SERVER
    },
    capabilities: Capabilities::SQL,
    dialect: DIALECT,
    options: &[],
    products: PRODUCTS,
    connect,
    describe: None,
};

pub const DIALECT: Dialect = Dialect {
    name_quotes: ('"', '"'),
    text: TextLiteral::PostgresEscape,
    bytes: ("decode('", "', 'hex')"),
    booleans: ("TRUE", "FALSE"),
    paging: Paging::LimitOffset,
    // PostgreSQL has LIKE for text only, and its LIKE tells letter case
    // apart; the other engines search numbers as text and ignore case with
    // their usual collations.
    contains: ContainsFilter {
        cast: Some(("CAST(", " AS TEXT)")),
        operator: "ILIKE",
        pattern: None,
    },
    parameters: ParameterStyle::Dollar,
    lexer: Lexer {
        dollar_quotes: true,
        escape_strings: true,
        nested_comments: true,
        ..Lexer::STANDARD
    },
    empty_insert: EmptyInsert::DefaultValues,
    default_schema: DefaultSchema::Fixed("public"),
    column_types: &[
        "INTEGER",
        "BIGINT",
        "TEXT",
        "VARCHAR(255)",
        "DECIMAL(38,10)",
        "BOOLEAN",
        "DATE",
        "TIMESTAMP",
        "JSONB",
        "BYTEA",
    ],
};

/// Amazon Redshift descends from PostgreSQL 8.0: a backslash escapes inside
/// an ordinary string, `TEXT` is a short `VARCHAR`, and binary and JSON
/// values have types of its own. NOT VERIFIED against a Redshift cluster.
pub const REDSHIFT_DIALECT: Dialect = Dialect {
    text: TextLiteral::Backslash,
    bytes: ("FROM_HEX('", "')"),
    contains: ContainsFilter {
        cast: Some(("CAST(", " AS VARCHAR(65535))")),
        operator: "ILIKE",
        pattern: None,
    },
    lexer: Lexer {
        backslash_escapes: true,
        dollar_quotes: true,
        ..Lexer::STANDARD
    },
    column_types: &[
        "INTEGER",
        "BIGINT",
        "VARCHAR(255)",
        "VARCHAR(65535)",
        "DECIMAL(38,10)",
        "BOOLEAN",
        "DATE",
        "TIMESTAMP",
        "SUPER",
        "VARBYTE",
    ],
    ..DIALECT
};

/// A hosted service whose certificate is issued by the provider's own
/// authority: encryption is on, and full verification needs the provider's
/// CA file chosen on the SSL/TLS tab.
const PRIVATE_CA_NOTE: (&str, &str) = (
    "Certyfikat tej usługi wystawia urząd dostawcy. Pełna weryfikacja wymaga wskazania pobranego od dostawcy pliku CA na karcie SSL/TLS.",
    "The certificate of this service is issued by the provider's own authority. Full verification needs the CA file downloaded from the provider, chosen on the SSL/TLS tab.",
);

const fn product(
    id: &'static str,
    label: &'static str,
    group: Group,
    text: &'static str,
    color: [u8; 3],
) -> ProductSpec {
    ProductSpec {
        id,
        label,
        group,
        badge: Badge { text, color },
        default_port: 5432,
        ..ProductSpec::GENERIC
    }
}

static PRODUCTS: &[ProductSpec] = &[
    ProductSpec {
        keywords: &["postgres", "pg"],
        ..product(
            "postgresql",
            "PostgreSQL",
            Group::Relational,
            "Pg",
            [51, 103, 145],
        )
    },
    ProductSpec {
        default_port: 26257,
        default_database: "defaultdb",
        keywords: &["cockroach", "crdb", "distributed"],
        ..product(
            "cockroachdb",
            "CockroachDB",
            Group::Relational,
            "Cr",
            [105, 51, 255],
        )
    },
    ProductSpec {
        keywords: &["timescale", "tiger", "time series", "szeregi czasowe"],
        ..product(
            "timescaledb",
            "TimescaleDB",
            Group::Relational,
            "Ts",
            [253, 184, 21],
        )
    },
    ProductSpec {
        default_port: 5433,
        default_database: "yugabyte",
        default_username: "yugabyte",
        keywords: &["yugabyte", "ysql", "distributed"],
        ..product(
            "yugabytedb",
            "YugabyteDB",
            Group::Relational,
            "Yb",
            [255, 95, 59],
        )
    },
    ProductSpec {
        default_tls: TlsMode::Require,
        keywords: &["aws", "amazon", "rds"],
        note: Some(PRIVATE_CA_NOTE),
        ..product(
            "amazon-rds-postgresql",
            "Amazon RDS for PostgreSQL",
            Group::Cloud,
            "Rp",
            [82, 127, 255],
        )
    },
    ProductSpec {
        default_tls: TlsMode::Require,
        keywords: &["aws", "amazon", "rds", "aurora"],
        note: Some(PRIVATE_CA_NOTE),
        ..product(
            "amazon-aurora-postgresql",
            "Amazon Aurora PostgreSQL",
            Group::Cloud,
            "Ap",
            [130, 80, 223],
        )
    },
    ProductSpec {
        default_tls: TlsMode::VerifyFull,
        keywords: &["azure", "microsoft", "flexible"],
        ..product(
            "azure-postgresql",
            "Azure Database for PostgreSQL",
            Group::Cloud,
            "Az",
            [0, 120, 212],
        )
    },
    ProductSpec {
        default_tls: TlsMode::Require,
        keywords: &["google", "gcp", "cloud sql", "cloudsql"],
        note: Some(PRIVATE_CA_NOTE),
        ..product(
            "google-cloud-sql-postgresql",
            "Google Cloud SQL for PostgreSQL",
            Group::Cloud,
            "Gc",
            [66, 133, 244],
        )
    },
    ProductSpec {
        default_tls: TlsMode::Require,
        default_database: "postgres",
        keywords: &["supabase", "pooler", "supavisor"],
        note: Some((
            "Port 5432 to połączenie bezpośrednie albo pooler w trybie sesji, 6543 to pooler w trybie transakcji. Pełna weryfikacja certyfikatu wymaga pliku CA pobranego z panelu Supabase.",
            "Port 5432 is the direct connection or the session pooler, 6543 the transaction pooler. Full certificate verification needs the CA file downloaded from the Supabase dashboard.",
        )),
        ..product("supabase", "Supabase", Group::Cloud, "Sb", [62, 207, 142])
    },
    ProductSpec {
        default_tls: TlsMode::VerifyFull,
        default_database: "neondb",
        keywords: &["neon", "serverless"],
        ..product("neon", "Neon", Group::Cloud, "Ne", [0, 229, 153])
    },
    ProductSpec {
        default_port: 5439,
        default_tls: TlsMode::Require,
        default_database: "dev",
        keywords: &["aws", "amazon", "redshift", "warehouse", "hurtownia"],
        note: Some((
            "Redshift ma własne katalogi i typy danych. Klucz główny nie jest tam wymuszany, więc komórkę można zmienić tylko wtedy, gdy klucz wskazuje dokładnie jeden wiersz.",
            "Redshift has catalogs and data types of its own. A primary key is not enforced there, so a cell can be changed only when the key identifies exactly one row.",
        )),
        dialect: Some(REDSHIFT_DIALECT),
        ..product(
            "amazon-redshift",
            "Amazon Redshift",
            Group::Analytics,
            "Rs",
            [140, 79, 255],
        )
    },
    ProductSpec {
        keywords: &["greenplum", "mpp", "warehouse", "hurtownia"],
        ..product(
            "greenplum",
            "Greenplum",
            Group::Analytics,
            "Gp",
            [0, 135, 116],
        )
    },
];

/// What the server turned out to be, where the driver has to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavor {
    /// PostgreSQL itself, and every fork and service that keeps its
    /// catalogs: TimescaleDB, YugabyteDB, RDS, Aurora, Azure, Cloud SQL,
    /// Supabase, Neon.
    Postgres,
    Cockroach,
    Redshift,
    Greenplum,
}
impl Flavor {
    /// Tell the server apart by its version text; the product is the hint
    /// when the server would not say.
    fn detect(version: &str, product: &str) -> Self {
        let version = version.to_lowercase();
        if version.contains("cockroachdb") {
            Flavor::Cockroach
        } else if version.contains("redshift") {
            Flavor::Redshift
        } else if version.contains("greenplum") {
            Flavor::Greenplum
        } else if !version.is_empty() {
            Flavor::Postgres
        } else {
            match product {
                "cockroachdb" => Flavor::Cockroach,
                "amazon-redshift" => Flavor::Redshift,
                "greenplum" => Flavor::Greenplum,
                _ => Flavor::Postgres,
            }
        }
    }
    fn dialect(self) -> &'static Dialect {
        match self {
            Flavor::Redshift => &REDSHIFT_DIALECT,
            _ => &DIALECT,
        }
    }
    /// Schemas that hold nothing a person browses, as a SQL list.
    fn hidden_schemas(self) -> &'static str {
        match self {
            Flavor::Postgres => "'pg_catalog','information_schema'",
            Flavor::Cockroach => "'pg_catalog','information_schema','crdb_internal','pg_extension'",
            Flavor::Redshift => "'pg_catalog','information_schema','pg_internal','pg_automv'",
            Flavor::Greenplum => {
                "'pg_catalog','information_schema','gp_toolkit','pg_aoseg','pg_bitmapindex'"
            }
        }
    }
}

fn connect<'a>(request: &'a ConnectRequest<'a>) -> BoxFuture<'a, Result<Arc<dyn Session>>> {
    Box::pin(async move {
        let profile = request.profile;
        let mut options = PgConnectOptions::new_without_pgpass()
            .host(request.server_name())
            .port(profile.port)
            .database(if profile.database.is_empty() {
                match request.product.default_database {
                    "" => "postgres",
                    name => name,
                }
            } else {
                &profile.database
            })
            .username(&profile.username)
            .password(&profile.password)
            .application_name("Baza Studio")
            .ssl_mode(match profile.tls {
                TlsMode::Disable => PgSslMode::Disable,
                TlsMode::Require => PgSslMode::Require,
                TlsMode::VerifyFull => PgSslMode::VerifyFull,
            })
            .disable_statement_logging();
        if let Some(address) = request.forwarded {
            options = options.dial_address(address);
        }
        if let Some(path) = &profile.ca_file {
            options = options.ssl_root_cert(path);
        }
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(15))
            .idle_timeout(Duration::from_secs(60))
            .connect_with(options)
            .await
            .map_err(connect_error)?;
        let version = scalar_text(&pool, "SELECT version()").await;
        let flavor = Flavor::detect(version.as_deref().unwrap_or(""), request.product.id);
        // The internal schemas of the TimescaleDB extension hold its
        // chunks and bookkeeping, hundreds of tables nobody opens.
        let timescale = flavor == Flavor::Postgres
            && scalar_text(
                &pool,
                "SELECT extname FROM pg_extension WHERE extname = 'timescaledb'",
            )
            .await
            .is_some();
        Ok(Arc::new(PostgresSession {
            pool,
            flavor,
            timescale,
        }) as Arc<dyn Session>)
    })
}

/// First column of the first row of a statement as text; `None` when the
/// statement fails or returns nothing.
async fn scalar_text(pool: &sqlx::PgPool, statement: &'static str) -> Option<String> {
    let row = sqlx::raw_sql(statement)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()?;
    row.try_get_unchecked::<String, _>(0).ok()
}

/// What stops the statement of another connection.
enum Backend {
    /// `pg_backend_pid()`, for `pg_cancel_backend`.
    Process(i64),
    /// `SHOW session_id` of CockroachDB, for `CANCEL SESSION`.
    Session(String),
    /// The server would not say; the connection is only closed.
    Unknown,
}

struct PostgresSession {
    pool: sqlx::PgPool,
    flavor: Flavor,
    timescale: bool,
}

impl PostgresSession {
    /// What identifies the running statement to another connection: the
    /// process id of the backend, or the session id on CockroachDB, which
    /// has no `pg_cancel_backend`.
    async fn setup(
        &self,
        connection: &mut sqlx::PgConnection,
        _: CancellationToken,
    ) -> Result<Backend> {
        let statement = match self.flavor {
            Flavor::Cockroach => "SHOW session_id",
            _ => "SELECT pg_backend_pid()",
        };
        let row = sqlx::raw_sql(statement)
            .fetch_one(connection)
            .await
            .map_err(safe_db_error)?;
        let Ok(text) = row.try_get_unchecked::<String, _>(0) else {
            return Ok(Backend::Unknown);
        };
        Ok(match self.flavor {
            // A hexadecimal number; anything else is not put into a statement.
            Flavor::Cockroach
                if !text.is_empty() && text.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                Backend::Session(text)
            }
            Flavor::Cockroach => Backend::Unknown,
            _ => text.parse().map_or(Backend::Unknown, Backend::Process),
        })
    }
    async fn cleanup(&self, _: &mut sqlx::PgConnection) -> Result<()> {
        Ok(())
    }
    async fn abort(&self, backend: Backend) -> Result<()> {
        let statement = match backend {
            Backend::Process(id) => format!("SELECT pg_cancel_backend({id})"),
            // The connection is closed right after: ending its session is
            // the same thing, and stops whatever it runs.
            Backend::Session(id) => format!("CANCEL SESSION IF EXISTS '{id}'"),
            Backend::Unknown => return Ok(()),
        };
        sqlx::raw_sql(AssertSqlSafe(statement))
            .execute(&self.pool)
            .await
            .map_err(safe_db_error)?;
        Ok(())
    }
    async fn run(&self, statement: &str) -> Result<QueryResult> {
        self.execute(statement, CancellationToken::new()).await
    }
    fn text(&self, value: &str) -> Result<String> {
        literal(self.flavor.dialect(), &Cell::Text(value.to_owned()))
    }
    /// Columns of a table from `source`, a view with the columns of
    /// `information_schema.columns`, with the primary key joined in. The key
    /// is an uncorrelated derived table: Redshift refuses the correlated
    /// form, and this one reads the same everywhere.
    fn columns_statement(&self, source: &str, container: &TableRef) -> Result<String> {
        let schema = self.text(&container.schema)?;
        let name = self.text(&container.name)?;
        // CockroachDB gives a table without a primary key a hidden `rowid`
        // column that `SELECT *` does not return.
        let visible = if self.flavor == Flavor::Cockroach {
            " AND c.is_hidden = 'NO'"
        } else {
            ""
        };
        Ok(format!(
            "SELECT c.column_name, c.data_type, c.is_nullable, c.column_default, \
             CASE WHEN k.column_name IS NULL THEN 'NO' ELSE 'YES' END \
             FROM {source} c \
             LEFT JOIN (SELECT kcu.column_name \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
             ON kcu.constraint_schema = tc.constraint_schema \
             AND kcu.constraint_name = tc.constraint_name \
             AND kcu.table_schema = tc.table_schema \
             AND kcu.table_name = tc.table_name \
             WHERE tc.constraint_type = 'PRIMARY KEY' \
             AND tc.table_schema = {schema} AND tc.table_name = {name}) k \
             ON k.column_name = c.column_name \
             WHERE c.table_schema = {schema} AND c.table_name = {name}{visible} \
             ORDER BY c.ordinal_position"
        ))
    }
}

impl Session for PostgresSession {
    fn dialect(&self) -> &Dialect {
        self.flavor.dialect()
    }
    fn execute<'a>(
        &'a self,
        text: &'a str,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            Ok(run_statement!(
                self,
                &self.pool,
                text,
                cancel,
                cells,
                Self::setup,
                Self::cleanup,
                Self::abort
            ))
        })
    }
    fn execute_one<'a>(&'a self, statement: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            execute_exactly_one!(self.pool, statement);
            Ok(())
        })
    }
    fn containers<'a>(&'a self) -> BoxFuture<'a, Result<Vec<TableRef>>> {
        Box::pin(async move {
            let hidden = self.flavor.hidden_schemas();
            let internal = if self.timescale {
                " AND table_schema NOT LIKE '!_timescaledb!_%' ESCAPE '!'"
            } else {
                ""
            };
            let listing = |source: &str| {
                format!(
                    "SELECT table_schema,table_name,table_type FROM {source} WHERE table_schema NOT IN ({hidden}){internal} ORDER BY table_schema,table_name"
                )
            };
            // Redshift lists external and late-binding objects only in its
            // own view; the standard one is the fallback.
            if self.flavor == Flavor::Redshift
                && let Ok(result) = self.run(&listing("svv_tables")).await
            {
                return Ok(containers_of(&result));
            }
            let result = self.run(&listing("information_schema.tables")).await?;
            Ok(containers_of(&result))
        })
    }
    fn columns<'a>(&'a self, container: &'a TableRef) -> BoxFuture<'a, Result<Vec<ColumnInfo>>> {
        Box::pin(async move {
            let standard = self.columns_statement("information_schema.columns", container)?;
            let result = match self.flavor {
                Flavor::Redshift => {
                    match self
                        .run(&self.columns_statement("svv_columns", container)?)
                        .await
                    {
                        Ok(result) if !result.rows.is_empty() => result,
                        _ => self.run(&standard).await?,
                    }
                }
                _ => self.run(&standard).await?,
            };
            Ok(result
                .rows
                .into_iter()
                .filter(|row| row.len() >= 5)
                .map(|row| ColumnInfo {
                    name: row[0].display(),
                    data_type: row[1].display(),
                    nullable: row[2].display() == "YES",
                    default: if matches!(row[3], Cell::Null) {
                        None
                    } else {
                        Some(row[3].display())
                    },
                    primary_key: row[4].display() == "YES",
                })
                .collect())
        })
    }
    fn import_rows<'a>(
        &'a self,
        container: &'a TableRef,
        columns: &'a [String],
        rows: &'a [Vec<Cell>],
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            Ok(insert_rows!(
                self.pool,
                self.flavor.dialect(),
                container,
                columns,
                rows,
                cancel
            ))
        })
    }
    fn server_stats<'a>(&'a self) -> BoxFuture<'a, Result<QueryResult>> {
        self.execute(
            match self.flavor {
                // The activity view of Redshift is not the PostgreSQL one.
                Flavor::Redshift => "SELECT current_database() AS database,version() AS version",
                // CockroachDB keeps `pg_stat_activity` empty; its sessions
                // and running statements are in views of its own.
                Flavor::Cockroach => {
                    "SELECT current_database() AS database,version() AS version,(SELECT count(*) FROM [SHOW CLUSTER SESSIONS]) AS connections,(SELECT count(*) FROM [SHOW CLUSTER STATEMENTS]) AS active"
                }
                _ => {
                    "SELECT current_database() AS database,version() AS version,(SELECT count(*) FROM pg_stat_activity) AS connections,(SELECT count(*) FROM pg_stat_activity WHERE state='active') AS active"
                }
            },
            CancellationToken::new(),
        )
    }
    fn close<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(self.pool.close())
    }
    fn container_details<'a>(&'a self, t: &'a TableRef) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            let dialect = self.dialect();
            let schema = crate::sql::literal(dialect, &Cell::Text(t.schema.clone()))?;
            let name = crate::sql::literal(dialect, &Cell::Text(t.name.clone()))?;
            let statement = format!(
                "SELECT pg_get_userbyid(c.relowner) AS owner, GREATEST(c.reltuples, 0)::bigint AS row_estimate, pg_relation_size(c.oid) AS data_size, pg_indexes_size(c.oid) AS index_size, pg_total_relation_size(c.oid) AS total_size, COALESCE(sp.spcname, 'pg_default') AS tablespace, s.last_vacuum AS vacuumed, s.last_analyze AS analyzed, obj_description(c.oid, 'pg_class') AS comment FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace LEFT JOIN pg_stat_all_tables s ON s.relid = c.oid LEFT JOIN pg_tablespace sp ON sp.oid = c.reltablespace WHERE n.nspname = {schema} AND c.relname = {name}"
            );
            self.execute(&statement, CancellationToken::new()).await
        })
    }
}

fn cells(row: &PgRow) -> Vec<Cell> {
    (0..row.len())
        .map(|index| {
            if row
                .try_get_raw(index)
                .map(|value| value.is_null())
                .unwrap_or(false)
            {
                return Cell::Null;
            }
            let name = row.columns()[index].type_info().name();
            macro_rules! get {
                ($ty:ty, $map:expr) => {
                    row.try_get::<$ty, _>(index).ok().map($map)
                };
            }
            let cell = match name {
                "BOOL" => get!(bool, Cell::Bool),
                "INT2" => get!(i16, |v| Cell::Integer(v.to_string())),
                "INT4" => get!(i32, |v| Cell::Integer(v.to_string())),
                "INT8" => get!(i64, |v| Cell::Integer(v.to_string())),
                "NUMERIC" => get!(bigdecimal::BigDecimal, |v| Cell::Decimal(decimal_text(&v))),
                "FLOAT4" => get!(f32, |v| Cell::Float(v.to_string())),
                "FLOAT8" => get!(f64, |v| Cell::Float(v.to_string())),
                "BYTEA" => get!(Vec<u8>, Cell::Bytes),
                "JSON" | "JSONB" => get!(serde_json::Value, |v| Cell::Json(v.to_string())),
                "UUID" => get!(uuid::Uuid, |v| Cell::Text(v.to_string())),
                "DATE" => get!(chrono::NaiveDate, |v| Cell::Text(v.to_string())),
                "TIME" => get!(chrono::NaiveTime, |v| Cell::Text(v.to_string())),
                "TIMESTAMP" => get!(chrono::NaiveDateTime, |v| Cell::Text(v.to_string())),
                "TIMESTAMPTZ" => get!(chrono::DateTime<chrono::Utc>, |v| Cell::Text(
                    v.to_rfc3339()
                )),
                _ => get!(String, Cell::Text),
            };
            cell.unwrap_or_else(|| Cell::Unsupported(format!("<{name}>")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_server_is_told_apart_by_its_version() {
        for (version, product, expected) in [
            (
                "PostgreSQL 17.2 on aarch64-unknown-linux-gnu",
                "",
                Flavor::Postgres,
            ),
            (
                "CockroachDB CCL v25.1.0 (aarch64-unknown-linux-gnu)",
                "postgresql",
                Flavor::Cockroach,
            ),
            (
                "PostgreSQL 8.0.2 on i686-pc-linux-gnu, Redshift 1.0.63269",
                "",
                Flavor::Redshift,
            ),
            (
                "PostgreSQL 12.12 (Greenplum Database 7.0.0 build dev)",
                "",
                Flavor::Greenplum,
            ),
            (
                "PostgreSQL 11.2-YB-2.20.1.0-b0 on x86_64",
                "yugabytedb",
                Flavor::Postgres,
            ),
            // The version wins over the product the profile was saved as.
            ("PostgreSQL 16.4", "amazon-redshift", Flavor::Postgres),
            ("", "amazon-redshift", Flavor::Redshift),
            ("", "", Flavor::Postgres),
        ] {
            assert_eq!(Flavor::detect(version, product), expected, "{version}");
        }
    }
    #[tokio::test]
    async fn forwarding_preserves_original_pg_identity_options() {
        let options = PgConnectOptions::new_without_pgpass()
            .host("original-db.example.test")
            .port(5432)
            .ssl_mode(PgSslMode::VerifyFull)
            .dial_address(std::net::SocketAddr::from(([127, 0, 0, 1], 32001)));
        assert_eq!(options.get_host(), "original-db.example.test");
        assert_eq!(options.get_port(), 5432);
        assert!(matches!(options.get_ssl_mode(), PgSslMode::VerifyFull));
        assert_eq!(options.get_dial_address().unwrap().port(), 32001);
    }
}
