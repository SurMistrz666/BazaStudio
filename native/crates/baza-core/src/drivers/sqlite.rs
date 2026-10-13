//! SQLite: a local database file, through SQLx.
//!
//! libSQL and Turso speak another protocol (HTTP and WebSocket) and are a
//! later engine of their own, not a product of this family.
use super::Session;
use super::sqlx_support::{
    connect_error, containers_of, execute_exactly_one, insert_rows, run_statement, safe_db_error,
};
use crate::engine::{
    Badge, Capabilities, ConnectRequest, ConnectionForm, ContainsFilter, DefaultSchema, Dialect,
    EmptyInsert, Engine, EngineSpec, Group, Lexer, Paging, ParameterStyle, ProductSpec,
    TextLiteral,
};
use crate::model::{Cell, ColumnInfo, QueryResult, TableRef, TlsMode};
use crate::sql::quote_identifier;
use anyhow::Result;
use futures_util::future::BoxFuture;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{ConnectOptions, Row, TypeInfo, ValueRef};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub const ENGINE: Engine = Engine::new(&SPEC);

pub static SPEC: EngineSpec = EngineSpec {
    id: "sqlite",
    aliases: &[],
    portable_name: "sqlite",
    label: "SQLite",
    available: true,
    form: ConnectionForm {
        file_extensions: &["sqlite", "sqlite3", "db", "db3"],
        ..ConnectionForm::FILE
    },
    capabilities: Capabilities::SQL,
    dialect: DIALECT,
    options: &[],
    products: &[ProductSpec {
        id: "sqlite",
        label: "SQLite",
        group: Group::Relational,
        badge: Badge {
            text: "Sq",
            color: [15, 128, 204],
        },
        default_tls: TlsMode::Disable,
        keywords: &["file", "plik", "local", "lokalna", "embedded"],
        ..ProductSpec::GENERIC
    }],
    connect,
    describe: None,
};

pub const DIALECT: Dialect = Dialect {
    name_quotes: ('"', '"'),
    text: TextLiteral::Standard,
    bytes: ("X'", "'"),
    booleans: ("TRUE", "FALSE"),
    paging: Paging::LimitOffset,
    contains: ContainsFilter::LIKE,
    parameters: ParameterStyle::Question,
    lexer: Lexer {
        backtick_names: true,
        bracket_names: true,
        ..Lexer::STANDARD
    },
    empty_insert: EmptyInsert::DefaultValues,
    default_schema: DefaultSchema::Fixed("main"),
    column_types: &["INTEGER", "TEXT", "REAL", "BLOB", "NUMERIC"],
};

fn connect<'a>(request: &'a ConnectRequest<'a>) -> BoxFuture<'a, Result<Arc<dyn Session>>> {
    Box::pin(async move {
        anyhow::ensure!(request.forwarded.is_none(), "sqlite_ssh_unsupported");
        let path = request
            .profile
            .sqlite_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("sqlite_path_required"))?;
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(false)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5))
            .disable_statement_logging();
        let pool = SqlitePoolOptions::new()
            .max_connections(2)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(options)
            .await
            .map_err(connect_error)?;
        Ok(Arc::new(SqliteSession { pool }) as Arc<dyn Session>)
    })
}

struct SqliteSession {
    pool: sqlx::SqlitePool,
}

impl SqliteSession {
    /// SQLite has no second connection that could stop a statement: a
    /// progress handler on the connection itself looks at the token.
    async fn setup(
        &self,
        connection: &mut sqlx::SqliteConnection,
        cancel: CancellationToken,
    ) -> Result<Option<u64>> {
        let started = Instant::now();
        connection
            .lock_handle()
            .await
            .map_err(safe_db_error)?
            .set_progress_handler(1000, move || {
                !cancel.is_cancelled() && started.elapsed() < Duration::from_secs(120)
            });
        Ok(None)
    }
    async fn cleanup(&self, connection: &mut sqlx::SqliteConnection) -> Result<()> {
        connection
            .lock_handle()
            .await
            .map_err(safe_db_error)?
            .remove_progress_handler();
        Ok(())
    }
    async fn abort(&self, _: Option<u64>) -> Result<()> {
        Ok(())
    }
}

impl Session for SqliteSession {
    fn dialect(&self) -> &Dialect {
        &DIALECT
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
                    "SELECT 'main', name, type FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY name",
                    CancellationToken::new(),
                )
                .await?;
            Ok(containers_of(&result))
        })
    }
    fn columns<'a>(&'a self, container: &'a TableRef) -> BoxFuture<'a, Result<Vec<ColumnInfo>>> {
        Box::pin(async move {
            let statement = format!(
                "PRAGMA {}.table_info({})",
                quote_identifier(&DIALECT, &container.schema)?,
                quote_identifier(&DIALECT, &container.name)?
            );
            let result = self.execute(&statement, CancellationToken::new()).await?;
            // cid, name, type, notnull, dflt_value, pk
            Ok(result
                .rows
                .into_iter()
                .filter(|row| row.len() >= 6)
                .map(|row| ColumnInfo {
                    name: row[1].display(),
                    data_type: row[2].display(),
                    nullable: row[3].display() == "0" && row[5].display() == "0",
                    primary_key: row[5].display() != "0",
                    default: if matches!(row[4], Cell::Null) {
                        None
                    } else {
                        Some(row[4].display())
                    },
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
                self.pool, &DIALECT, container, columns, rows, cancel
            ))
        })
    }
    fn server_stats<'a>(&'a self) -> BoxFuture<'a, Result<QueryResult>> {
        self.execute(
            "SELECT sqlite_version() AS version,(SELECT count(*) FROM sqlite_master WHERE type='table') AS tables",
            CancellationToken::new(),
        )
    }
    fn close<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(self.pool.close())
    }
    fn container_details<'a>(&'a self, t: &'a TableRef) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            let schema = crate::sql::quote_identifier(&DIALECT, if t.schema.is_empty() { "main" } else { &t.schema })?;
            let statement = format!(
                "SELECT sql AS definition FROM {schema}.sqlite_master WHERE name = {}",
                crate::sql::literal(&DIALECT, &Cell::Text(t.name.clone()))?
            );
            self.execute(&statement, CancellationToken::new()).await
        })
    }
}

fn cells(row: &SqliteRow) -> Vec<Cell> {
    (0..row.len())
        .map(|index| {
            if row
                .try_get_raw(index)
                .map(|value| value.is_null())
                .unwrap_or(false)
            {
                return Cell::Null;
            }
            let name = row
                .try_get_raw(index)
                .map(|value| value.type_info().name().to_owned())
                .unwrap_or_default();
            match name.as_str() {
                "INTEGER" => row
                    .try_get::<i64, _>(index)
                    .map(|value| Cell::Integer(value.to_string())),
                "REAL" => row
                    .try_get::<f64, _>(index)
                    .map(|value| Cell::Float(value.to_string())),
                "BLOB" => row.try_get::<Vec<u8>, _>(index).map(Cell::Bytes),
                _ => row.try_get::<String, _>(index).map(Cell::Text),
            }
            .unwrap_or_else(|_| Cell::Unsupported(format!("<{name}>")))
        })
        .collect()
}
