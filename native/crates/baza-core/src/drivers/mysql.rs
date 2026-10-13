//! MySQL, MariaDB and the servers and services that speak their protocol,
//! through SQLx.
//!
//! The products are presets over this one driver. Where a server differs
//! from MySQL in something the driver does, it is told apart by the answer
//! to `SELECT VERSION()` ([`Flavor`]), not by the product the user picked:
//! a profile saved as "MySQL" that points at TiDB works as well.
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
use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions, MySqlRow, MySqlSslMode};
use sqlx::{AssertSqlSafe, Column, ConnectOptions, Row, TypeInfo, ValueRef};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const ENGINE: Engine = Engine::new(&SPEC);

pub static SPEC: EngineSpec = EngineSpec {
    id: "mysql",
    aliases: &["mariadb"],
    // The name the first version of the profile file format used.
    portable_name: "mariadb",
    label: "MariaDB / MySQL",
    available: true,
    // The database is optional: without one the side panel
    // lists every database of the server the account can see.
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
    name_quotes: ('`', '`'),
    text: TextLiteral::MysqlHex,
    bytes: ("X'", "'"),
    booleans: ("TRUE", "FALSE"),
    paging: Paging::LimitOffset,
    contains: ContainsFilter::LIKE,
    parameters: ParameterStyle::Question,
    lexer: Lexer {
        backslash_escapes: true,
        double_quote_strings: true,
        backtick_names: true,
        hash_comments: true,
        dashes_need_space: true,
        ..Lexer::STANDARD
    },
    empty_insert: EmptyInsert::EmptyLists,
    default_schema: DefaultSchema::Database,
    column_types: &[
        "INTEGER",
        "BIGINT",
        "TEXT",
        "VARCHAR(255)",
        "DECIMAL(38,10)",
        "BOOLEAN",
        "DATE",
        "TIMESTAMP",
        "JSON",
        "BLOB",
    ],
};

/// TiDB compares text byte by byte unless a column says otherwise
/// (`utf8mb4_bin` is its default collation), so `LIKE` tells letter case
/// apart there. Both sides of the "contains" filter are folded instead.
pub const TIDB_DIALECT: Dialect = Dialect {
    contains: ContainsFilter {
        cast: Some(("LOWER(", ")")),
        operator: "LIKE",
        pattern: Some(("LOWER(", ")")),
    },
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
        default_port: 3306,
        ..ProductSpec::GENERIC
    }
}

static PRODUCTS: &[ProductSpec] = &[
    // The product of profiles saved before products existed: it does not
    // say which of the two servers it is, and is not offered for new ones.
    ProductSpec {
        listed: false,
        ..product(
            "mariadb-mysql",
            "MariaDB / MySQL",
            Group::Relational,
            "My",
            [0, 117, 143],
        )
    },
    product("mysql", "MySQL", Group::Relational, "My", [0, 117, 143]),
    product(
        "mariadb",
        "MariaDB",
        Group::Relational,
        "Ma",
        [192, 118, 90],
    ),
    ProductSpec {
        keywords: &["percona", "mysql"],
        ..product(
            "percona-server",
            "Percona Server for MySQL",
            Group::Relational,
            "Pe",
            [255, 126, 26],
        )
    },
    ProductSpec {
        default_port: 4000,
        default_username: "root",
        keywords: &["pingcap", "distributed", "htap"],
        dialect: Some(TIDB_DIALECT),
        ..product("tidb", "TiDB", Group::Relational, "Ti", [220, 21, 10])
    },
    ProductSpec {
        default_port: 2881,
        keywords: &["oceanbase", "alibaba", "distributed"],
        note: Some((
            "Tryb zgodności z MySQL. Port 2881 to bezpośrednie połączenie z serwerem, 2883 przez OBProxy; nazwa użytkownika ma postać użytkownik@dzierżawca.",
            "MySQL compatibility mode. Port 2881 is a direct connection to the server, 2883 goes through OBProxy; the user name has the form user@tenant.",
        )),
        ..product(
            "oceanbase",
            "OceanBase",
            Group::Relational,
            "Ob",
            [0, 107, 255],
        )
    },
    ProductSpec {
        default_tls: TlsMode::Require,
        keywords: &["aws", "amazon", "rds"],
        note: Some(PRIVATE_CA_NOTE),
        ..product(
            "amazon-rds-mysql",
            "Amazon RDS for MySQL",
            Group::Cloud,
            "Rm",
            [82, 127, 255],
        )
    },
    ProductSpec {
        default_tls: TlsMode::Require,
        keywords: &["aws", "amazon", "rds"],
        note: Some(PRIVATE_CA_NOTE),
        ..product(
            "amazon-rds-mariadb",
            "Amazon RDS for MariaDB",
            Group::Cloud,
            "Rd",
            [82, 127, 255],
        )
    },
    ProductSpec {
        default_tls: TlsMode::Require,
        keywords: &["aws", "amazon", "rds", "aurora"],
        note: Some(PRIVATE_CA_NOTE),
        ..product(
            "amazon-aurora-mysql",
            "Amazon Aurora MySQL",
            Group::Cloud,
            "Am",
            [130, 80, 223],
        )
    },
    ProductSpec {
        default_tls: TlsMode::VerifyFull,
        keywords: &["azure", "microsoft", "flexible"],
        ..product(
            "azure-mysql",
            "Azure Database for MySQL",
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
            "google-cloud-sql-mysql",
            "Google Cloud SQL for MySQL",
            Group::Cloud,
            "Gc",
            [66, 133, 244],
        )
    },
    ProductSpec {
        default_tls: TlsMode::VerifyFull,
        keywords: &["vitess", "serverless"],
        note: Some((
            "PlanetScale wymaga szyfrowania. Zmiany schematu na gałęzi produkcyjnej z bezpiecznymi migracjami serwer odrzuca; wykonuj je na gałęzi deweloperskiej.",
            "PlanetScale requires encryption. Schema changes on a production branch with safe migrations are refused by the server; make them on a development branch.",
        )),
        ..product(
            "planetscale",
            "PlanetScale",
            Group::Cloud,
            "Ps",
            [112, 112, 124],
        )
    },
    ProductSpec {
        keywords: &["memsql", "singlestore", "htap"],
        ..product(
            "singlestore",
            "SingleStore",
            Group::Analytics,
            "S2",
            [170, 0, 255],
        )
    },
];

/// What the server turned out to be, where the driver has to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavor {
    /// MySQL, MariaDB, Percona Server and the hosted services built on them.
    Mysql,
    TiDb,
    /// Vitess, which PlanetScale is.
    Vitess,
    SingleStore,
    OceanBase,
}
impl Flavor {
    fn dialect(self) -> &'static Dialect {
        match self {
            Flavor::TiDb => &TIDB_DIALECT,
            _ => &DIALECT,
        }
    }
    /// Tell the server apart by its version text; the product is the hint
    /// when the text says nothing (SingleStore reports a MySQL version).
    fn detect(version: &str, product: &str) -> Self {
        let version = version.to_lowercase();
        if version.contains("tidb") {
            Flavor::TiDb
        } else if version.contains("vitess") || product == "planetscale" {
            Flavor::Vitess
        } else if version.contains("oceanbase") || product == "oceanbase" {
            Flavor::OceanBase
        } else if version.contains("singlestore")
            || version.contains("memsql")
            || product == "singlestore"
        {
            Flavor::SingleStore
        } else {
            Flavor::Mysql
        }
    }
}

fn connect<'a>(request: &'a ConnectRequest<'a>) -> BoxFuture<'a, Result<Arc<dyn Session>>> {
    Box::pin(async move {
        let profile = request.profile;
        let mut options = MySqlConnectOptions::new()
            .host(request.server_name())
            .port(profile.port)
            .username(&profile.username)
            .ssl_mode(match profile.tls {
                TlsMode::Disable => MySqlSslMode::Disabled,
                TlsMode::Require => MySqlSslMode::Required,
                TlsMode::VerifyFull => MySqlSslMode::VerifyIdentity,
            })
            .disable_statement_logging();
        if !profile.database.is_empty() {
            options = options.database(&profile.database);
        }
        // An account without a password answers the server with nothing at
        // all; a scramble of the empty text is refused as a wrong password.
        if !profile.password.is_empty() {
            options = options.password(&profile.password);
        }
        if let Some(address) = request.forwarded {
            options = options.dial_address(address);
        }
        if let Some(path) = &profile.ca_file {
            options = options.ssl_ca(path);
        }
        let pool = MySqlPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(15))
            .idle_timeout(Duration::from_secs(60))
            .connect_with(options)
            .await
            .map_err(connect_error)?;
        // A server that will not say its version is treated as MySQL.
        let version = match sqlx::raw_sql("SELECT VERSION()").fetch_one(&pool).await {
            Ok(row) => row.try_get::<String, _>(0).unwrap_or_default(),
            Err(_) => String::new(),
        };
        let flavor = Flavor::detect(&version, request.product.id);
        Ok(Arc::new(MysqlSession { pool, flavor }) as Arc<dyn Session>)
    })
}

struct MysqlSession {
    pool: sqlx::MySqlPool,
    flavor: Flavor,
}

impl MysqlSession {
    /// The id of the connection, for `KILL QUERY` from another one.
    async fn setup(
        &self,
        connection: &mut sqlx::MySqlConnection,
        _: CancellationToken,
    ) -> Result<Option<u64>> {
        let id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(connection)
            .await
            .map_err(safe_db_error)?;
        Ok(Some(id))
    }
    async fn cleanup(&self, _: &mut sqlx::MySqlConnection) -> Result<()> {
        Ok(())
    }
    async fn abort(&self, id: Option<u64>) -> Result<()> {
        if let Some(id) = id {
            sqlx::raw_sql(AssertSqlSafe(format!("KILL QUERY {id}")))
                .execute(&self.pool)
                .await
                .map_err(safe_db_error)?;
        }
        Ok(())
    }
}

impl Session for MysqlSession {
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
            let result = self
                .execute(
                    // One database when the profile names one, otherwise
                    // every one but those of the server itself.
                    "SELECT table_schema,table_name,table_type FROM information_schema.tables WHERE table_schema=DATABASE() OR (DATABASE() IS NULL AND table_schema NOT IN ('information_schema','mysql','performance_schema','sys')) ORDER BY table_schema,table_name",
                    CancellationToken::new(),
                )
                .await?;
            Ok(containers_of(&result))
        })
    }
    fn columns<'a>(&'a self, container: &'a TableRef) -> BoxFuture<'a, Result<Vec<ColumnInfo>>> {
        Box::pin(async move {
            let statement = format!(
                "SELECT column_name,column_type,is_nullable,column_default,CASE WHEN column_key='PRI' THEN 'YES' ELSE 'NO' END FROM information_schema.columns WHERE table_schema={} AND table_name={} ORDER BY ordinal_position",
                literal(&DIALECT, &Cell::Text(container.schema.clone()))?,
                literal(&DIALECT, &Cell::Text(container.name.clone()))?
            );
            let result = self.execute(&statement, CancellationToken::new()).await?;
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
                // A Vitess gateway has no process list and no connection limit
                // of one server to report.
                Flavor::Vitess => "SELECT DATABASE() AS `database`,VERSION() AS version",
                // The alias is quoted: MySQL and TiDB read a bare `database`
                // as the keyword, MariaDB as a name.
                _ => {
                    "SELECT DATABASE() AS `database`,VERSION() AS version, @@max_connections AS max_connections, (SELECT COUNT(*) FROM information_schema.PROCESSLIST) AS connections"
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
            let dialect = self.flavor.dialect();
            let statement = format!(
                "SELECT ENGINE AS engine, AUTO_INCREMENT AS auto_increment, ROW_FORMAT AS row_format, TABLE_ROWS AS row_estimate, CREATE_TIME AS created, UPDATE_TIME AS updated, CHECK_TIME AS checked, INDEX_LENGTH AS index_size, DATA_LENGTH AS data_size, MAX_DATA_LENGTH AS max_data_size, DATA_FREE AS free_size, TABLE_COLLATION AS collation, CREATE_OPTIONS AS create_options, TABLE_COMMENT AS comment FROM information_schema.TABLES WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}",
                literal(dialect, &Cell::Text(t.schema.clone()))?,
                literal(dialect, &Cell::Text(t.name.clone()))?
            );
            self.execute(&statement, CancellationToken::new()).await
        })
    }
}

fn cells(row: &MySqlRow) -> Vec<Cell> {
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
            let cell = if name.contains("INT") {
                get!(i64, |v| Cell::Integer(v.to_string()))
                    .or_else(|| get!(u64, |v| Cell::Integer(v.to_string())))
            } else if name.contains("DECIMAL") {
                get!(bigdecimal::BigDecimal, |v| Cell::Decimal(decimal_text(&v)))
            } else {
                match name {
                    "FLOAT" => get!(f32, |v| Cell::Float(v.to_string())),
                    "DOUBLE" => get!(f64, |v| Cell::Float(v.to_string())),
                    "JSON" => get!(serde_json::Value, |v| Cell::Json(v.to_string())),
                    "DATE" => get!(chrono::NaiveDate, |v| Cell::Text(v.to_string())),
                    "DATETIME" | "TIMESTAMP" => {
                        get!(chrono::NaiveDateTime, |v| Cell::Text(v.to_string()))
                    }
                    "BLOB" | "BINARY" | "VARBINARY" | "MEDIUMBLOB" | "LONGBLOB" | "TINYBLOB" => {
                        // MariaDB JSON/LONGTEXT can carry the BINARY flag in
                        // its reported name while retaining a text collation.
                        // SQLx's compatibility check preserves actual binary
                        // blobs and allows lossless Unicode JSON text here.
                        get!(String, Cell::Text).or_else(|| get!(Vec<u8>, Cell::Bytes))
                    }
                    _ => get!(String, Cell::Text),
                }
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
            ("8.4.3", "", Flavor::Mysql),
            ("11.4.2-MariaDB-ubu2404", "mariadb", Flavor::Mysql),
            ("8.0.11-TiDB-v8.5.0", "mysql", Flavor::TiDb),
            ("8.0.34-Vitess", "", Flavor::Vitess),
            ("8.0.34", "planetscale", Flavor::Vitess),
            ("5.7.25-OceanBase_CE-v4.3.0", "", Flavor::OceanBase),
            ("5.7.32", "singlestore", Flavor::SingleStore),
            ("", "", Flavor::Mysql),
        ] {
            assert_eq!(Flavor::detect(version, product), expected, "{version}");
        }
    }
    #[tokio::test]
    async fn forwarding_preserves_original_mysql_identity_options() {
        let options = MySqlConnectOptions::new()
            .host("original-db.example.test")
            .port(3306)
            .ssl_mode(MySqlSslMode::VerifyIdentity)
            .dial_address(std::net::SocketAddr::from(([127, 0, 0, 1], 32002)));
        assert_eq!(options.get_host(), "original-db.example.test");
        assert_eq!(options.get_port(), 3306);
        assert!(matches!(
            options.get_ssl_mode(),
            MySqlSslMode::VerifyIdentity
        ));
        assert_eq!(options.get_dial_address().unwrap().port(), 32002);
    }
}
