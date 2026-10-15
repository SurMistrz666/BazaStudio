//! Microsoft SQL Server and the services built on it, over TDS, through
//! Tiberius.
//!
//! The products are presets over this one driver. Only a SQL Server login
//! (user name and password) signs in: Windows and Microsoft Entra ID
//! authentication and the browser service of named instances are not
//! implemented, and the product notes say so.
//!
//! How a statement reaches the server decides what it leaves behind, so the
//! driver chooses:
//!
//! * A statement, or a script without `GO`, runs through `sp_executesql`, in
//!   a scope of its own: `SET` options and temporary tables end with it, and
//!   the connection goes back to the pool as it was. What survives even that
//!   scope (a transaction, `USE`, a cursor, whatever a procedure did) is
//!   looked for in the text ([`script::effects`]): such a connection is asked
//!   for open transactions and closed instead of reused.
//! * The batches of a script with `GO` run on one connection in order. A
//!   batch that sets something up for the next one (`USE`, `SET`, a
//!   temporary table, `BEGIN TRANSACTION`) runs as a plain batch at the
//!   level of the session, like in the tools of SQL Server, and the
//!   connection is closed afterwards.
//! * A transaction that a script leaves open is rolled back and reported
//!   (`mssql_transaction_left_open`): the next statement may get another
//!   connection, so there is nothing a later `COMMIT` could commit.
mod connection;
#[cfg(test)]
mod live;
mod script;
mod values;

use super::{PageRequest, RowChange, Session};
use crate::builder::{FilterOp, QueryBuilder};
use crate::engine::{
    Badge, Capabilities, ConnectRequest, ConnectionForm, ContainsFilter, DefaultSchema, Dialect,
    EmptyInsert, Engine, EngineSpec, FieldUse, Group, Lexer, OptionField, OptionKind, Paging,
    ParameterStyle, ProductSpec, TextLiteral,
};
use crate::model::{Cell, ColumnInfo, QueryResult, ResultColumn, TableRef, TlsMode};
use crate::sql::{MAX_RESULT_BYTES, MAX_RESULT_ROWS, MAX_SQL_BYTES, qualified, quote_identifier};
use anyhow::{Result, anyhow};
use connection::{Connector, Lease, Pool, Tds, statement_error};
use futures_util::future::BoxFuture;
use futures_util::stream::{BoxStream, StreamExt, TryStreamExt};
use script::Effects;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiberius::error::Error;
use tiberius::{ColumnType, Command, CommandItem, QueryItem, Row};
use tokio_util::sync::CancellationToken;

pub const ENGINE: Engine = Engine::new(&SPEC);

pub static SPEC: EngineSpec = EngineSpec {
    id: "mssql",
    aliases: &["sqlserver", "sql-server"],
    portable_name: "mssql",
    label: "Microsoft SQL Server",
    available: true,
    form: ConnectionForm {
        // Without a name the login lands in its default database.
        database: FieldUse::Optional,
        username: FieldUse::Required,
        ..ConnectionForm::SERVER
    },
    capabilities: Capabilities::SQL,
    dialect: DIALECT,
    options: OPTIONS,
    products: PRODUCTS,
    connect,
    describe: Some(describe),
};

/// The `ORDER BY` a page gets when nothing else is known: `OFFSET` is only
/// accepted after one.
const DEFAULT_ORDER: &str = "1";

pub const DIALECT: Dialect = Dialect {
    name_quotes: ('[', ']'),
    text: TextLiteral::National,
    bytes: ("0x", ""),
    booleans: ("1", "0"),
    paging: Paging::OffsetFetch {
        default_order: Some(DEFAULT_ORDER),
    },
    // Whether `LIKE` tells letter case apart is a matter of the collation of
    // each column, so both sides are folded; and `[` opens a set of
    // characters in a T-SQL pattern, so it is escaped like `%` and `_`.
    contains: ContainsFilter {
        cast: Some(values::GENERIC_CAST),
        operator: "LIKE",
        pattern: Some(("LOWER(REPLACE(", ", N'[', N'!['))")),
    },
    parameters: ParameterStyle::AtNamed,
    lexer: Lexer {
        bracket_names: true,
        nested_comments: true,
        ..Lexer::STANDARD
    },
    empty_insert: EmptyInsert::DefaultValues,
    default_schema: DefaultSchema::Fixed("dbo"),
    column_types: &[
        "INT",
        "BIGINT",
        "NVARCHAR(255)",
        "NVARCHAR(MAX)",
        "DECIMAL(38,10)",
        "FLOAT",
        "BIT",
        "DATE",
        "DATETIME2",
        "UNIQUEIDENTIFIER",
        "VARBINARY(MAX)",
    ],
};

static OPTIONS: &[OptionField] = &[OptionField {
    key: "strict_encryption",
    label: (
        "Szyfrowanie ścisłe (TDS 8.0)",
        "Strict encryption (TDS 8.0)",
    ),
    kind: OptionKind::Flag,
    default: "false",
    placeholder: "",
    help: Some((
        "Szyfruje połączenie jeszcze przed logowaniem (Encrypt=Strict). Obsługują to SQL Server 2022 i nowsze oraz Azure SQL; wymaga trybu SSL/TLS innego niż „Wyłączony”.",
        "Encrypts the connection before the sign-in (Encrypt=Strict). SQL Server 2022 and later and Azure SQL support it; it needs an SSL/TLS mode other than “Disabled”.",
    )),
    required: false,
}];

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
        default_port: 1433,
        ..ProductSpec::GENERIC
    }
}

static PRODUCTS: &[ProductSpec] = &[
    ProductSpec {
        keywords: &[
            "mssql",
            "microsoft",
            "sql server",
            "sqlserver",
            "tsql",
            "t-sql",
        ],
        note: Some((
            "Logowanie kontem SQL Server; uwierzytelnianie Windows i Entra ID nie jest obsługiwane, a instancję nazwaną wskazuje jej port TCP. Certyfikat, który serwer wygenerował sobie sam, wymaga trybu „Szyfrowanie wymagane” (Trust Server Certificate).",
            "Sign-in with a SQL Server login; Windows and Entra ID authentication are not supported, and a named instance is reached by its TCP port. A certificate the server generated for itself needs the mode “Encryption required” (Trust Server Certificate).",
        )),
        ..product(
            "sql-server",
            "Microsoft SQL Server",
            Group::Relational,
            "Ms",
            [204, 41, 39],
        )
    },
    ProductSpec {
        default_tls: TlsMode::Disable,
        keywords: &[
            "azure",
            "microsoft",
            "mssql",
            "edge",
            "iot",
            "docker",
            "arm64",
        ],
        note: Some((
            "Silnik SQL Server w kontenerze, z logowaniem kontem SQL. Kontener sam generuje swój certyfikat, stąd tryb „Szyfrowanie wymagane”. Typy CLR (geography, geometry, hierarchyid) nie działają w tej edycji.",
            "The SQL Server engine in a container, with sign-in by a SQL login. The container generates its own certificate, hence the mode “Encryption required”. The CLR types (geography, geometry, hierarchyid) do not work in this edition.",
        )),
        ..product(
            "azure-sql-edge",
            "Azure SQL Edge",
            Group::Relational,
            "Ed",
            [0, 164, 239],
        )
    },
    ProductSpec {
        default_tls: TlsMode::VerifyFull,
        keywords: &["azure", "microsoft", "mssql", "sql server", "tsql"],
        note: Some((
            "Host ma postać serwer.database.windows.net; podaj nazwę bazy, bo bez niej logowanie trafia do bazy master. Azure SQL wymaga szyfrowania, a zapora serwera musi dopuszczać Twój adres IP. Logowanie kontem SQL; Entra ID nie jest obsługiwane.",
            "The host has the form server.database.windows.net; enter the database name, because without it the sign-in lands in the master database. Azure SQL requires encryption, and the firewall of the server has to allow your IP address. Sign-in with a SQL login; Entra ID is not supported.",
        )),
        ..product(
            "azure-sql-database",
            "Azure SQL Database",
            Group::Cloud,
            "As",
            [0, 120, 212],
        )
    },
    ProductSpec {
        default_tls: TlsMode::VerifyFull,
        keywords: &["azure", "microsoft", "mssql", "managed instance", "mi"],
        note: Some((
            "Punkt końcowy w sieci wirtualnej używa portu 1433, a publiczny (nazwa.public.strefa.database.windows.net) portu 3342. Logowanie kontem SQL; Entra ID nie jest obsługiwane.",
            "The endpoint inside the virtual network uses port 1433, the public one (name.public.zone.database.windows.net) port 3342. Sign-in with a SQL login; Entra ID is not supported.",
        )),
        ..product(
            "azure-sql-managed-instance",
            "Azure SQL Managed Instance",
            Group::Cloud,
            "Mi",
            [0, 120, 212],
        )
    },
    ProductSpec {
        default_tls: TlsMode::Require,
        keywords: &["aws", "amazon", "rds", "microsoft", "mssql", "sql server"],
        note: Some((
            "Certyfikat tej usługi wystawia urząd Amazon RDS: pełna weryfikacja wymaga pliku CA pobranego od AWS, wskazanego na karcie SSL/TLS. Logowanie kontem SQL, na przykład głównym użytkownikiem instancji; uwierzytelnianie Windows nie jest obsługiwane.",
            "The certificate of this service is issued by the Amazon RDS authority: full verification needs the CA file downloaded from AWS, chosen on the SSL/TLS tab. Sign-in with a SQL login, for example the master user of the instance; Windows authentication is not supported.",
        )),
        ..product(
            "amazon-rds-sql-server",
            "Amazon RDS for SQL Server",
            Group::Cloud,
            "Rq",
            [82, 127, 255],
        )
    },
];

/// Sentences for the result codes of this driver.
fn describe(code: &str, en: bool) -> Option<&'static str> {
    let (pl, english) = match code {
        "mssql_login_required" => (
            "Podaj nazwę użytkownika konta SQL Server. Uwierzytelnianie Windows i Microsoft Entra ID nie jest obsługiwane.",
            "Enter the user name of a SQL Server login. Windows and Microsoft Entra ID authentication are not supported.",
        ),
        "mssql_strict_needs_tls" => (
            "Szyfrowanie ścisłe (TDS 8.0) wymaga szyfrowania. Na karcie SSL/TLS wybierz tryb inny niż „Wyłączony” albo wyłącz szyfrowanie ścisłe.",
            "Strict encryption (TDS 8.0) needs encryption. On the SSL/TLS tab choose a mode other than “Disabled”, or turn strict encryption off.",
        ),
        "mssql_strict_unsupported" => (
            "Serwer nie odpowiedział na szyfrowanie ścisłe (TDS 8.0). Obsługują je SQL Server 2022 i nowsze oraz Azure SQL; dla starszego serwera wyłącz tę opcję na karcie Ogólne.",
            "The server did not answer strict encryption (TDS 8.0). SQL Server 2022 and later and Azure SQL support it; for an older server turn the option off on the General tab.",
        ),
        "mssql_redirect_through_tunnel" => (
            "Serwer przekierował połączenie pod inny adres, do którego tunel SSH nie prowadzi. Ustaw na serwerze Azure SQL politykę połączeń „Proxy” albo połącz się bez tunelu.",
            "The server redirected the connection to another address, which the SSH tunnel does not lead to. Set the connection policy of the Azure SQL server to “Proxy”, or connect without the tunnel.",
        ),
        "mssql_transaction_left_open" => (
            "Skrypt zostawił otwartą transakcję, więc została wycofana i nic nie zostało zapisane. BEGIN TRANSACTION i COMMIT muszą się znaleźć w jednym uruchomieniu.",
            "The script left a transaction open, so it was rolled back and nothing was saved. BEGIN TRANSACTION and COMMIT have to be part of one run.",
        ),
        "mssql_password_expired" => (
            "Hasło tego konta SQL Server wygasło albo musi zostać zmienione. Zmień je w narzędziu administracyjnym serwera i wpisz nowe na karcie Ogólne.",
            "The password of this SQL Server login has expired or has to be changed. Change it with an administration tool of the server and enter the new one on the General tab.",
        ),
        "mssql_account_locked" => (
            "To konto SQL Server jest zablokowane po zbyt wielu nieudanych logowaniach. Odblokowuje je administrator serwera.",
            "This SQL Server login is locked out after too many failed sign-ins. An administrator of the server unlocks it.",
        ),
        "mssql_firewall_blocked" => (
            "Zapora serwera Azure SQL nie dopuszcza Twojego adresu IP. Dodaj go w regułach zapory serwera w portalu Azure.",
            "The firewall of the Azure SQL server does not allow your IP address. Add it to the firewall rules of the server in the Azure portal.",
        ),
        _ => return None,
    };
    Some(if en { english } else { pl })
}

fn connect<'a>(request: &'a ConnectRequest<'a>) -> BoxFuture<'a, Result<Arc<dyn Session>>> {
    Box::pin(async move {
        let connector = Connector::new(request).await?;
        let mut first = connector.open().await?;
        // The views of running sessions show a login without the permission
        // nothing but itself, without saying so: the "Monitor" would count
        // one connection. Ask once what this login may see. The permission
        // on the database is enough only in Azure SQL Database (engine
        // edition 5), where those views are views of the database; a server
        // that does not know the newer permission answers NULL for it.
        let monitored = scalar(
            &mut first,
            "SELECT CASE WHEN HAS_PERMS_BY_NAME(NULL, NULL, 'VIEW SERVER STATE') = 1 \
             OR HAS_PERMS_BY_NAME(NULL, NULL, 'VIEW SERVER PERFORMANCE STATE') = 1 \
             OR (CAST(SERVERPROPERTY('EngineEdition') AS INT) = 5 AND HAS_PERMS_BY_NAME(DB_NAME(), 'DATABASE', 'VIEW DATABASE STATE') = 1) \
             THEN 1 ELSE 0 END",
        )
        .await
        .is_ok_and(|allowed| allowed == 1);
        Ok(Arc::new(MssqlSession {
            pool: Pool::new(connector, first),
            monitored,
            described: Mutex::default(),
        }) as Arc<dyn Session>)
    })
}

/// A statement ends after this long, as in every driver.
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the server gets to acknowledge that it stopped a statement, and
/// to answer the short questions asked after a script.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(3);
/// Rows of an import that travel in one request.
const IMPORT_CHUNK_ROWS: usize = 200;
const IMPORT_CHUNK_BYTES: usize = 256 * 1024;

/// The single integer a plain batch returns.
async fn scalar(client: &mut Tds, batch: &str) -> Result<i32, Error> {
    let results = client.simple_query(batch).await?.into_results().await?;
    results
        .last()
        .and_then(|rows| rows.first())
        .and_then(|row| row.try_get::<i32, _>(0).ok().flatten())
        .ok_or_else(|| Error::Protocol("a number was expected".into()))
}

/// Whether the connection can go on after this failure. An error of the
/// server arrives at the end of its answer, so the connection is in step
/// with it; after any other failure it is not. Nor is it after an error of
/// severity 20 and above, which ends the session on the server.
fn in_step(error: &Error) -> bool {
    matches!(error, Error::Server(token) if token.class() < 20)
}

/// One request to the server, and how many times `GO n` asks for it.
#[derive(Clone, Copy)]
struct Step<'a> {
    text: &'a str,
    repeat: u32,
    /// Run it through `sp_executesql`, in a scope of its own; otherwise as a
    /// plain batch at the level of the session.
    scope: bool,
    /// Its row counts are rows that statements changed.
    counts: bool,
}

/// What a request sends back, whichever way it was sent.
enum Item {
    Columns(Vec<(String, ColumnType)>),
    Row(Row),
    Affected(u64),
}

fn described_columns(columns: &[tiberius::Column]) -> Vec<(String, ColumnType)> {
    columns
        .iter()
        .map(|column| (column.name().to_owned(), column.column_type()))
        .collect()
}

async fn send<'a>(
    client: &'a mut Tds,
    step: Step<'_>,
) -> Result<BoxStream<'a, Result<Item, Error>>, Error> {
    if step.scope {
        let mut command = Command::new("sp_executesql");
        command.bind_param("@stmt", step.text.to_owned());
        let stream = command.exec(client).await?;
        Ok(stream
            .try_filter_map(|item| async move {
                Ok(match item {
                    CommandItem::Metadata(meta) => {
                        Some(Item::Columns(described_columns(meta.columns())))
                    }
                    CommandItem::Row(row) => Some(Item::Row(row)),
                    CommandItem::RowsAffected(rows) => Some(Item::Affected(rows)),
                    CommandItem::ReturnStatus(_) | CommandItem::ReturnValue(_) => None,
                })
            })
            .boxed())
    } else {
        let stream = client.simple_query(step.text.to_owned()).await?;
        Ok(stream
            .map_ok(|item| match item {
                QueryItem::Metadata(meta) => Item::Columns(described_columns(meta.columns())),
                QueryItem::Row(row) => Item::Row(row),
            })
            .boxed())
    }
}

/// How the requests of a run ended.
enum End {
    Complete,
    /// The result holds as much as a result may.
    Truncated,
}

/// Send the steps in order and keep the last result set and the number of
/// changed rows.
async fn drive(
    client: &mut Tds,
    steps: &[Step<'_>],
    result: &mut QueryResult,
) -> Result<End, Error> {
    let mut types = Vec::new();
    let mut bytes = 0usize;
    for step in steps
        .iter()
        .flat_map(|step| std::iter::repeat_n(step, step.repeat as usize))
    {
        let mut stream = send(client, *step).await?;
        // The row count that follows a result set counts its rows.
        let mut selected = false;
        while let Some(item) = stream.try_next().await? {
            match item {
                Item::Columns(columns) => {
                    result.columns = columns
                        .iter()
                        .map(|(name, column)| ResultColumn {
                            name: name.clone(),
                            data_type: values::type_name(*column).into(),
                        })
                        .collect();
                    types = columns.into_iter().map(|(_, column)| column).collect();
                    result.rows.clear();
                    bytes = 0;
                    selected = true;
                }
                Item::Row(row) => {
                    let cells: Vec<Cell> = row
                        .into_iter()
                        .enumerate()
                        .map(|(index, data)| {
                            values::cell(
                                data,
                                types.get(index).copied().unwrap_or(ColumnType::Null),
                            )
                        })
                        .collect();
                    let size = row_bytes(&cells);
                    if result.rows.len() >= MAX_RESULT_ROWS
                        || bytes.saturating_add(size) > MAX_RESULT_BYTES
                    {
                        return Ok(End::Truncated);
                    }
                    bytes += size;
                    result.rows.push(cells);
                }
                Item::Affected(_) if selected => selected = false,
                Item::Affected(rows) if step.counts => {
                    result.affected = result.affected.saturating_add(rows);
                }
                Item::Affected(_) => {}
            }
        }
    }
    Ok(End::Complete)
}

/// Size of a row as it counts towards `sql::MAX_RESULT_BYTES`.
fn row_bytes(cells: &[Cell]) -> usize {
    cells
        .iter()
        .map(|cell| match cell {
            Cell::Bytes(value) => value.len(),
            _ => cell.display().len(),
        })
        .sum()
}

/// What happens to a connection after its requests ran to their end.
#[derive(Clone, Copy, Default)]
struct Aftermath {
    /// Ask whether a transaction is still open.
    check: bool,
    /// Close the connection: its session is no longer what a new one is.
    discard: bool,
}

/// What the driver remembers of a table or view, to write its pages.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Described {
    /// Columns of the primary key, in the order of the key.
    keys: Vec<String>,
    /// Columns that number themselves.
    identity: Vec<String>,
    /// Name and type of every column, once they were read.
    types: Option<Vec<(String, String)>>,
}

struct MssqlSession {
    pool: Pool,
    /// The login may read the views of running sessions.
    monitored: bool,
    /// What `containers` and `columns` learned, by schema and name.
    described: Mutex<HashMap<(String, String), Described>>,
}

/// Stop the statement of a connection on the server and close the
/// connection, without making anybody wait for it: the signal goes out at
/// once, and whether the server acknowledges it in time or not, the
/// connection is never used again. Closing it ends what it ran.
///
/// The acknowledgement cannot be relied on to arrive in step. A result that
/// is left in the middle of a row leaves the reader of the library there
/// too, and it may wait for bytes that never come.
fn stop(mut lease: Lease) {
    tokio::spawn(async move {
        let _ = tokio::time::timeout(SETTLE_TIMEOUT, lease.cancel_query()).await;
    });
}

impl MssqlSession {
    /// Decide what happens to a connection whose requests ran to their end.
    /// Fails when a transaction was left open; the connection is closed
    /// then, which rolls the transaction back.
    async fn settle(&self, mut lease: Lease, after: Aftermath) -> Result<()> {
        if after.check {
            let open =
                tokio::time::timeout(SETTLE_TIMEOUT, scalar(&mut lease, "SELECT @@TRANCOUNT"))
                    .await;
            match open {
                Ok(Ok(0)) => {}
                Ok(Ok(_)) => return Err(anyhow!("mssql_transaction_left_open")),
                // The connection did not say: it is not used again.
                _ => return Ok(()),
            }
        }
        if !after.discard {
            self.pool.release(lease);
        }
        Ok(())
    }

    /// Run requests on one connection, with the limits every driver keeps.
    async fn run(
        &self,
        steps: &[Step<'_>],
        after: Aftermath,
        cancel: CancellationToken,
    ) -> Result<QueryResult> {
        let started = Instant::now();
        let deadline = tokio::time::sleep(STATEMENT_TIMEOUT);
        tokio::pin!(deadline);
        let mut lease = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(anyhow!("query_cancelled")),
            _ = &mut deadline => return Err(anyhow!("query_timeout")),
            lease = self.pool.acquire() => lease?,
        };
        let mut result = QueryResult::default();
        let ended = {
            let work = drive(&mut lease, steps, &mut result);
            tokio::pin!(work);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("query_cancelled"),
                _ = &mut deadline => Err("query_timeout"),
                end = &mut work => Ok(end),
            }
        };
        match ended {
            Err(code) => {
                stop(lease);
                return Err(anyhow!(code));
            }
            Ok(Ok(End::Truncated)) => {
                // The rest of the rows is not wanted: the statement is
                // stopped on the server too.
                stop(lease);
                result.truncated = true;
            }
            Ok(Ok(End::Complete)) => self.settle(lease, after).await?,
            Ok(Err(error)) => {
                // A connection that is out of step with the server is
                // closed; one that is in step may still hold a transaction.
                if let Error::Server(token) = &error
                    && in_step(&error)
                {
                    let left_open = self.settle(lease, after).await.is_err();
                    // 266: the transaction count changed inside the scope.
                    // That is the open transaction itself, in other words.
                    if left_open && token.code() == 266 {
                        return Err(anyhow!("mssql_transaction_left_open"));
                    }
                }
                return Err(statement_error(&error));
            }
        }
        result.duration_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }

    /// Run a statement the driver wrote itself.
    async fn query(&self, text: &str, cancel: CancellationToken) -> Result<QueryResult> {
        let step = Step {
            text,
            repeat: 1,
            scope: true,
            counts: false,
        };
        self.run(&[step], Aftermath::default(), cancel).await
    }

    fn remembered(&self, container: &TableRef) -> Option<Described> {
        let key = (container.schema.clone(), container.name.clone());
        self.described.lock().ok()?.get(&key).cloned()
    }

    /// The statement that describes the columns of a table or view.
    fn columns_statement(container: &TableRef) -> String {
        format!(
            "SELECT c.name, \
             CASE \
             WHEN t.name IN (N'nvarchar', N'nchar') THEN t.name + N'(' + CASE WHEN c.max_length = -1 THEN N'max' ELSE CAST(c.max_length / 2 AS NVARCHAR(10)) END + N')' \
             WHEN t.name IN (N'varchar', N'char', N'varbinary', N'binary') THEN t.name + N'(' + CASE WHEN c.max_length = -1 THEN N'max' ELSE CAST(c.max_length AS NVARCHAR(10)) END + N')' \
             WHEN t.name IN (N'decimal', N'numeric') THEN t.name + N'(' + CAST(c.precision AS NVARCHAR(10)) + N',' + CAST(c.scale AS NVARCHAR(10)) + N')' \
             WHEN t.name IN (N'datetime2', N'time', N'datetimeoffset') THEN t.name + N'(' + CAST(c.scale AS NVARCHAR(10)) + N')' \
             ELSE t.name END, \
             c.is_nullable, \
             CASE WHEN c.is_identity = 1 THEN N'IDENTITY' WHEN c.is_computed = 1 THEN N'AS ' + cc.definition WHEN t.name = N'timestamp' THEN N'rowversion' ELSE dc.definition END, \
             ISNULL(k.key_ordinal, 0), \
             c.is_identity \
             FROM sys.columns c \
             JOIN sys.objects o ON o.object_id = c.object_id \
             JOIN sys.schemas s ON s.schema_id = o.schema_id \
             JOIN sys.types u ON u.user_type_id = c.user_type_id \
             LEFT JOIN sys.types b ON u.is_user_defined = 1 AND u.is_assembly_type = 0 AND b.user_type_id = u.system_type_id \
             CROSS APPLY (SELECT ISNULL(b.name, u.name) AS name) t \
             LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
             LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
             LEFT JOIN (SELECT ic.object_id, ic.column_id, ic.key_ordinal \
             FROM sys.indexes i \
             JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
             WHERE i.is_primary_key = 1) k ON k.object_id = c.object_id AND k.column_id = c.column_id \
             WHERE s.name = {} AND o.name = {} AND o.type IN ('U', 'V') \
             ORDER BY c.column_id",
            script::text_literal(&container.schema),
            script::text_literal(&container.name)
        )
    }

    /// Columns of a container, and what they say about its pages.
    async fn describe(&self, container: &TableRef) -> Result<Vec<ColumnInfo>> {
        let result = self
            .query(
                &Self::columns_statement(container),
                CancellationToken::new(),
            )
            .await?;
        let mut keys = Vec::new();
        let mut described = Described::default();
        let mut columns = Vec::new();
        for row in result.rows.into_iter().filter(|row| row.len() >= 6) {
            let name = row[0].display();
            let data_type = row[1].display();
            let ordinal: u32 = row[4].display().parse().unwrap_or(0);
            if ordinal > 0 {
                keys.push((ordinal, name.clone()));
            }
            if row[5] == Cell::Bool(true) {
                described.identity.push(name.clone());
            }
            columns.push(ColumnInfo {
                name,
                data_type,
                nullable: row[2] == Cell::Bool(true),
                primary_key: ordinal > 0,
                default: if matches!(row[3], Cell::Null) {
                    None
                } else {
                    Some(row[3].display())
                },
            });
        }
        keys.sort();
        described.keys = keys.into_iter().map(|(_, name)| name).collect();
        described.types = Some(
            columns
                .iter()
                .map(|column| (column.name.clone(), column.data_type.clone()))
                .collect(),
        );
        if !columns.is_empty()
            && let Ok(mut known) = self.described.lock()
        {
            known.insert(
                (container.schema.clone(), container.name.clone()),
                described,
            );
        }
        Ok(columns)
    }
}

/// The statement that reads a page, with what is known about the container.
///
/// `OFFSET` needs an order, and pages are only pages of one list when the
/// order is total: so the primary key orders a page that has no order of its
/// own, and breaks the ties of one that has. A table without a key and a view
/// have nothing to order by; they are read in the order the server scans
/// them, which costs no sort of the whole table.
fn page_statement(page: &PageRequest, described: Option<&Described>) -> Result<String> {
    let mut dialect = DIALECT;
    // Search the text the grid shows for the type of the column.
    if let Some(types) = described.and_then(|described| described.types.as_ref()) {
        let mut casts = page
            .filters
            .iter()
            .filter(|filter| filter.op == FilterOp::Contains)
            .map(|filter| {
                types
                    .iter()
                    .find(|(name, _)| *name == filter.column)
                    .map_or(values::GENERIC_CAST, |(_, data_type)| {
                        values::contains_cast(data_type)
                    })
            });
        if let Some(first) = casts.next()
            && casts.all(|cast| cast == first)
        {
            dialect.contains.cast = Some(first);
        }
    }
    let statement = QueryBuilder::page(page).sql(&dialect)?;
    let (Some(described), Some(paging)) = (described, statement.rfind("\nOFFSET ")) else {
        return Ok(statement);
    };
    // A column may stand in ORDER BY once: the key breaks the ties of the
    // columns ordered by, without them.
    let sorted = |name: &String| {
        page.order_by
            .iter()
            .chain(&page.then_by)
            .any(|(column, _)| column == name)
    };
    let key = || {
        described
            .keys
            .iter()
            .filter(|name| !sorted(name))
            .map(|name| quote_identifier(&DIALECT, name))
            .collect::<Result<Vec<_>>>()
            .map(|names| names.join(", "))
    };
    let (head, tail) = statement.split_at(paging);
    Ok(match &page.order_by {
        None => {
            let default = format!("\nORDER BY {DEFAULT_ORDER}");
            let Some(head) = head.strip_suffix(default.as_str()) else {
                return Ok(statement);
            };
            let order = if described.keys.is_empty() {
                "(SELECT NULL)".to_owned()
            } else {
                key()?
            };
            format!("{head}\nORDER BY {order}{tail}")
        }
        Some(_) => {
            let ties = key()?;
            if ties.is_empty() {
                statement
            } else {
                format!("{head}, {ties}{tail}")
            }
        }
    })
}

impl Session for MssqlSession {
    fn dialect(&self) -> &Dialect {
        &DIALECT
    }

    fn execute<'a>(
        &'a self,
        text: &'a str,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            let batches = script::batches(text);
            // More than one request: what a batch sets up, the next one
            // expects to find.
            let script = batches.len() > 1 || batches.iter().any(|batch| batch.repeat > 1);
            let mut steps = Vec::new();
            let mut after = Aftermath::default();
            for batch in &batches {
                let Effects {
                    transaction,
                    session,
                    scoped,
                    variables,
                } = script::effects(&batch.text);
                let scope = !(script && (transaction || session || scoped));
                after.check |= transaction || session;
                after.discard |= session || (scoped && !scope);
                steps.push(Step {
                    text: &batch.text,
                    repeat: batch.repeat,
                    scope,
                    counts: scope && !variables && !session,
                });
            }
            if steps.is_empty() {
                return Ok(QueryResult::default());
            }
            self.run(&steps, after, cancel).await
        })
    }

    fn execute_one<'a>(&'a self, statement: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // One batch: the statement, and a transaction around it that is
            // committed only when exactly one row changed. The row count of
            // the statement itself is read, not what its triggers did.
            let batch = format!(
                "BEGIN TRANSACTION;\n{statement};\nDECLARE @baza_changed INT = @@ROWCOUNT;\nIF @baza_changed = 1 COMMIT TRANSACTION ELSE ROLLBACK TRANSACTION;\nSELECT @baza_changed;"
            );
            anyhow::ensure!(batch.len() <= MAX_SQL_BYTES, "sql_invalid");
            let mut lease = self.pool.acquire().await?;
            let changed = tokio::time::timeout(STATEMENT_TIMEOUT, scalar(&mut lease, &batch)).await;
            match changed {
                Err(_) => {
                    stop(lease);
                    Err(anyhow!("query_timeout"))
                }
                Ok(Ok(changed)) => {
                    self.pool.release(lease);
                    anyhow::ensure!(changed == 1, "row_changed_reload_required");
                    Ok(())
                }
                Ok(Err(error)) => {
                    // An error that ended the batch left the transaction
                    // open: it goes before the connection is used again.
                    if in_step(&error)
                        && tokio::time::timeout(
                            SETTLE_TIMEOUT,
                            scalar(
                                &mut lease,
                                "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION; SELECT @@TRANCOUNT",
                            ),
                        )
                        .await
                        .is_ok_and(|open| open == Ok(0))
                    {
                        self.pool.release(lease);
                    }
                    Err(statement_error(&error))
                }
            }
        })
    }

    fn containers<'a>(&'a self) -> BoxFuture<'a, Result<Vec<TableRef>>> {
        Box::pin(async move {
            // One row per table or view, and per column of its primary key.
            // The table of database diagrams is the tools' own.
            let result = self
                .query(
                    "SELECT s.name, o.name, CASE o.type WHEN 'V' THEN N'view' ELSE N'table' END, c.name \
                     FROM sys.objects o \
                     JOIN sys.schemas s ON s.schema_id = o.schema_id \
                     LEFT JOIN sys.indexes i ON i.object_id = o.object_id AND i.is_primary_key = 1 \
                     LEFT JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                     LEFT JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                     WHERE o.type IN ('U', 'V') AND o.is_ms_shipped = 0 \
                     AND NOT EXISTS (SELECT 1 FROM sys.extended_properties p WHERE p.class = 1 AND p.major_id = o.object_id AND p.minor_id = 0 AND p.name = N'microsoft_database_tools_support') \
                     ORDER BY s.name, o.name, ic.key_ordinal",
                    CancellationToken::new(),
                )
                .await?;
            let mut containers: Vec<TableRef> = Vec::new();
            let mut described: HashMap<(String, String), Described> = HashMap::new();
            for row in result.rows.iter().filter(|row| row.len() >= 4) {
                let (schema, name) = (row[0].display(), row[1].display());
                if containers
                    .last()
                    .is_none_or(|last| last.schema != schema || last.name != name)
                {
                    containers.push(TableRef {
                        schema: schema.clone(),
                        name: name.clone(),
                        kind: row[2].display(),
                    });
                }
                let entry = described.entry((schema, name)).or_default();
                if let Cell::Text(column) = &row[3] {
                    entry.keys.push(column.clone());
                }
            }
            // A list that was cut short knows nothing about what is missing.
            if !result.truncated
                && let Ok(mut known) = self.described.lock()
            {
                *known = described;
            }
            Ok(containers)
        })
    }

    fn columns<'a>(&'a self, container: &'a TableRef) -> BoxFuture<'a, Result<Vec<ColumnInfo>>> {
        Box::pin(self.describe(container))
    }

    fn page_text(&self, page: &PageRequest) -> Result<String> {
        page_statement(page, self.remembered(&page.container).as_ref())
    }

    fn read_page<'a>(
        &'a self,
        page: &'a PageRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            // The types of the columns decide how a filter is written. They
            // are read once per container; a failure leaves the plain form.
            if !page.filters.is_empty()
                && self
                    .remembered(&page.container)
                    .is_none_or(|described| described.types.is_none())
            {
                let _ = self.describe(&page.container).await;
            }
            let text = self.page_text(page)?;
            self.query(&text, cancel).await
        })
    }

    fn change_text(&self, change: &RowChange) -> Result<String> {
        let statement = script::change_statement(&DIALECT, change)?;
        if let RowChange::UpdateCell {
            table, column, old, ..
        } = change
            && !matches!(old, Cell::Null)
            && self
                .remembered(table)
                .and_then(|described| described.types)
                .is_some_and(|types| {
                    types
                        .iter()
                        .any(|(name, data_type)| name == column && !script::comparable(data_type))
                })
        {
            return script::compared_as_text(&DIALECT, statement, column, old);
        }
        Ok(statement)
    }

    fn apply<'a>(&'a self, change: &'a RowChange) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // How the old value of a cell is compared depends on the type of
            // its column, which is read once per table.
            if let RowChange::UpdateCell { table, .. } = change
                && self
                    .remembered(table)
                    .is_none_or(|described| described.types.is_none())
            {
                let _ = self.describe(table).await;
            }
            let statement = self.change_text(change)?;
            self.execute_one(&statement).await
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
            let started = Instant::now();
            let target = qualified(&DIALECT, container)?;
            let names = columns
                .iter()
                .map(|column| quote_identifier(&DIALECT, column))
                .collect::<Result<Vec<_>>>()?
                .join(", ");
            // Every request runs in a scope of its own, so these two options
            // end with it. The first makes any error roll the transaction
            // back and end the request, instead of going on with the next
            // row. The second is needed to write a value into a column that
            // numbers itself, which a file with such a column asks for.
            let mut header = String::from("SET XACT_ABORT ON;\n");
            let numbered = self.remembered(container).is_some_and(|described| {
                described
                    .identity
                    .iter()
                    .any(|identity| columns.contains(identity))
            });
            if numbered {
                header.push_str(&format!("SET IDENTITY_INSERT {target} ON;\n"));
            }
            let mut lease = self.pool.acquire().await?;
            let outcome = {
                let work = insert(&mut lease, &header, &target, &names, rows, &cancel);
                tokio::pin!(work);
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => None,
                    outcome = &mut work => Some(outcome),
                }
            };
            match outcome {
                // Closing the connection rolls back what was inserted.
                None => {
                    stop(lease);
                    Err(anyhow!("query_cancelled"))
                }
                Some(Ok(())) => {
                    self.pool.release(lease);
                    Ok(QueryResult {
                        affected: rows.len() as u64,
                        duration_ms: started.elapsed().as_millis() as u64,
                        ..Default::default()
                    })
                }
                Some(Err(failure)) => {
                    if failure.reusable
                        && tokio::time::timeout(
                            SETTLE_TIMEOUT,
                            scalar(
                                &mut lease,
                                "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION; SELECT @@TRANCOUNT",
                            ),
                        )
                        .await
                        .is_ok_and(|open| open == Ok(0))
                    {
                        self.pool.release(lease);
                    }
                    Err(failure.error)
                }
            }
        })
    }

    fn server_stats<'a>(&'a self) -> BoxFuture<'a, Result<QueryResult>> {
        // `@@VERSION` is four lines; the first one names the product.
        const SERVER: &str = "DB_NAME() AS [database], RTRIM(REPLACE(REPLACE(LEFT(@@VERSION, CHARINDEX(NCHAR(10), @@VERSION + NCHAR(10)) - 1), NCHAR(13), N''), NCHAR(9), N' ')) AS version";
        const LIMITS: &str = "@@MAX_CONNECTIONS AS max_connections, (SELECT COUNT(*) FROM sys.tables WHERE is_ms_shipped = 0) AS tables";
        // Sessions of people and programs, and the requests they are
        // running, without this one.
        const ACTIVITY: &str = "(SELECT COUNT(*) FROM sys.dm_exec_sessions WHERE is_user_process = 1) AS connections, (SELECT COUNT(*) FROM sys.dm_exec_requests r JOIN sys.dm_exec_sessions s ON s.session_id = r.session_id WHERE s.is_user_process = 1 AND r.session_id <> @@SPID) AS active";
        Box::pin(async move {
            let statement = if self.monitored {
                format!("SELECT {SERVER}, {ACTIVITY}, {LIMITS}")
            } else {
                format!("SELECT {SERVER}, {LIMITS}")
            };
            self.query(&statement, CancellationToken::new()).await
        })
    }

    fn close<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(self.pool.close())
    }

    fn container_details<'a>(&'a self, t: &'a TableRef) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            let dialect = self.dialect();
            let schema = crate::sql::literal(dialect, &Cell::Text(t.schema.clone()))?;
            let name = crate::sql::literal(dialect, &Cell::Text(t.name.clone()))?;
            // Sizes need VIEW DATABASE STATE; without it only the dates.
            let statement = format!(
                "SELECT o.create_date AS created, o.modify_date AS updated, (SELECT SUM(p.rows) FROM sys.partitions p WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)) AS row_estimate, CAST(ep.value AS nvarchar(4000)) AS comment FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id LEFT JOIN sys.extended_properties ep ON ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.name = N'MS_Description' WHERE s.name = {schema} AND o.name = {name}"
            );
            self.query(&statement, CancellationToken::new()).await
        })
    }
}

/// Why an import stopped.
struct ImportFailure {
    error: anyhow::Error,
    /// The connection is in step with the server and may be used again once
    /// its transaction is gone.
    reusable: bool,
}

/// Insert rows in one transaction, a few hundred statements per request.
async fn insert(
    client: &mut Tds,
    header: &str,
    target: &str,
    names: &str,
    rows: &[Vec<Cell>],
    cancel: &CancellationToken,
) -> Result<(), ImportFailure> {
    let failed = |error: anyhow::Error| ImportFailure {
        error,
        reusable: true,
    };
    let refused = |error: Error| ImportFailure {
        reusable: in_step(&error),
        error: statement_error(&error),
    };
    client.begin_transaction().await.map_err(refused)?;
    let mut request = String::from(header);
    let mut pending = 0usize;
    for (index, row) in rows.iter().enumerate() {
        let literals = row
            .iter()
            .map(|cell| script::literal(&DIALECT, cell))
            .collect::<Result<Vec<_>>>()
            .map_err(failed)?
            .join(", ");
        let statement = format!("INSERT INTO {target} ({names}) VALUES ({literals});\n");
        if statement.len() > MAX_SQL_BYTES {
            return Err(failed(anyhow!("import_row_too_large")));
        }
        request.push_str(&statement);
        pending += 1;
        let last = index + 1 == rows.len();
        if !(last || pending >= IMPORT_CHUNK_ROWS || request.len() >= IMPORT_CHUNK_BYTES) {
            continue;
        }
        if cancel.is_cancelled() {
            return Err(failed(anyhow!("query_cancelled")));
        }
        let step = Step {
            text: &request,
            repeat: 1,
            scope: true,
            counts: false,
        };
        let mut stream = send(client, step).await.map_err(refused)?;
        while stream.try_next().await.map_err(refused)?.is_some() {}
        drop(stream);
        request.truncate(header.len());
        pending = 0;
    }
    client.commit_transaction().await.map_err(refused)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::Filter;
    use crate::sql::{editable_table, literal, statements};

    fn table(name: &str) -> TableRef {
        TableRef {
            schema: "dbo".into(),
            name: name.into(),
            kind: "table".into(),
        }
    }

    #[test]
    fn the_dialect_writes_t_sql() {
        assert_eq!(
            quote_identifier(&DIALECT, "Order Details").unwrap(),
            "[Order Details]"
        );
        assert_eq!(quote_identifier(&DIALECT, "a]b").unwrap(), "[a]]b]");
        assert_eq!(
            qualified(&DIALECT, &table("zamówienia")).unwrap(),
            "[dbo].[zamówienia]"
        );
        assert_eq!(
            literal(&DIALECT, &Cell::Text("it's Zażółć 🦀".into())).unwrap(),
            "N'it''s Zażółć 🦀'"
        );
        // A backslash is an ordinary character, and so is a double quote.
        assert_eq!(
            literal(&DIALECT, &Cell::Text("C:\\dane \"x\"".into())).unwrap(),
            "N'C:\\dane \"x\"'"
        );
        assert_eq!(literal(&DIALECT, &Cell::Bool(true)).unwrap(), "1");
        assert_eq!(literal(&DIALECT, &Cell::Bool(false)).unwrap(), "0");
        assert_eq!(
            literal(&DIALECT, &Cell::Bytes(vec![0, 0xff, 0x10])).unwrap(),
            "0x00ff10"
        );
        assert_eq!(literal(&DIALECT, &Cell::Bytes(Vec::new())).unwrap(), "0x");
        assert_eq!(
            literal(
                &DIALECT,
                &Cell::Decimal("12345678901234567890.1200000000".into())
            )
            .unwrap(),
            "12345678901234567890.1200000000"
        );
        assert_eq!(literal(&DIALECT, &Cell::Null).unwrap(), "NULL");
        assert_eq!(
            crate::sql::insert_row(&DIALECT, &table("t"), &[]).unwrap(),
            "INSERT INTO [dbo].[t] DEFAULT VALUES"
        );
        assert_eq!(
            crate::builder::create_table(
                &DIALECT,
                &table("t"),
                &[
                    crate::builder::CreateColumn {
                        name: "id".into(),
                        data_type: DIALECT.column_types[0].into(),
                        nullable: false,
                        primary_key: true,
                    },
                    crate::builder::CreateColumn {
                        name: "note".into(),
                        data_type: "NVARCHAR(MAX)".into(),
                        nullable: true,
                        primary_key: false,
                    }
                ]
            )
            .unwrap(),
            "CREATE TABLE [dbo].[t] (\n  [id] INT NOT NULL,\n  [note] NVARCHAR(MAX),\n  PRIMARY KEY ([id])\n)"
        );
    }

    #[test]
    fn scripts_are_read_the_way_sql_server_reads_them() {
        // Brackets and double quotes are names, comments nest, a backslash
        // does not escape.
        assert_eq!(
            statements(
                &DIALECT,
                "SELECT [a;b], \"c;d\", N'e;f''g' /* x; /* y; */ z; */ ; SELECT 'C:\\'; -- ;\nSELECT 3"
            )
            .unwrap()
            .len(),
            3
        );
        assert!(statements(&DIALECT, "SELECT [open").is_err());
        let known = [table("customers")];
        for query in [
            "SELECT * FROM [dbo].[customers]",
            "SELECT * FROM dbo.customers c WHERE c.id = 2",
            "SELECT * FROM \"dbo\".\"customers\" ORDER BY 1 OFFSET 0 ROWS FETCH NEXT 500 ROWS ONLY",
            "SELECT * FROM customers WHERE name = N'a]b' ORDER BY [id] DESC, [name]",
        ] {
            assert!(editable_table(&DIALECT, query, &known).is_some(), "{query}");
        }
        for query in [
            "SELECT * FROM customers c, orders o",
            "SELECT * FROM customers; SELECT 1",
            "SELECT * FROM [dbo].[customers] ORDER BY (SELECT NULL) OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY",
        ] {
            assert!(editable_table(&DIALECT, query, &known).is_none(), "{query}");
        }
    }

    fn keyed(keys: &[&str], types: &[(&str, &str)]) -> Described {
        Described {
            keys: keys.iter().map(|key| (*key).to_owned()).collect(),
            identity: Vec::new(),
            types: (!types.is_empty()).then(|| {
                types
                    .iter()
                    .map(|(name, data_type)| ((*name).to_owned(), (*data_type).to_owned()))
                    .collect()
            }),
        }
    }

    #[test]
    fn a_page_is_ordered_by_the_primary_key() {
        let orders = table("orders");
        let first = PageRequest::first(orders.clone());
        // Nothing is known yet: the order the dialect names.
        assert_eq!(
            page_statement(&first, None).unwrap(),
            "SELECT *\nFROM [dbo].[orders]\nORDER BY 1\nOFFSET 0 ROWS FETCH NEXT 500 ROWS ONLY"
        );
        assert_eq!(
            page_statement(&first, None).unwrap(),
            QueryBuilder::page(&first).sql(&DIALECT).unwrap()
        );
        let single = keyed(&["id"], &[]);
        assert_eq!(
            page_statement(&first, Some(&single)).unwrap(),
            "SELECT *\nFROM [dbo].[orders]\nORDER BY [id]\nOFFSET 0 ROWS FETCH NEXT 500 ROWS ONLY"
        );
        let composite = keyed(&["order id", "line]no"], &[]);
        let second = PageRequest {
            offset: 500,
            ..first.clone()
        };
        assert_eq!(
            page_statement(&second, Some(&composite)).unwrap(),
            "SELECT *\nFROM [dbo].[orders]\nORDER BY [order id], [line]]no]\nOFFSET 500 ROWS FETCH NEXT 500 ROWS ONLY"
        );
        // A chosen order keeps its direction; the key only breaks its ties.
        let by_date = PageRequest {
            order_by: Some(("placed".into(), true)),
            ..first.clone()
        };
        assert_eq!(
            page_statement(&by_date, Some(&composite)).unwrap(),
            "SELECT *\nFROM [dbo].[orders]\nORDER BY [placed] DESC, [order id], [line]]no]\nOFFSET 0 ROWS FETCH NEXT 500 ROWS ONLY"
        );
        let by_key = PageRequest {
            order_by: Some(("order id".into(), false)),
            ..first.clone()
        };
        assert_eq!(
            page_statement(&by_key, Some(&composite)).unwrap(),
            "SELECT *\nFROM [dbo].[orders]\nORDER BY [order id] ASC, [line]]no]\nOFFSET 0 ROWS FETCH NEXT 500 ROWS ONLY"
        );
        // Several columns: a key column ordered by already is not repeated.
        let by_two = PageRequest {
            order_by: Some(("placed".into(), true)),
            then_by: vec![("line]no".into(), false)],
            ..first.clone()
        };
        assert_eq!(
            page_statement(&by_two, Some(&composite)).unwrap(),
            "SELECT *\nFROM [dbo].[orders]\nORDER BY [placed] DESC, [line]]no] ASC, [order id]\nOFFSET 0 ROWS FETCH NEXT 500 ROWS ONLY"
        );
        assert_eq!(
            page_statement(
                &PageRequest {
                    order_by: Some(("id".into(), true)),
                    ..first.clone()
                },
                Some(&single)
            )
            .unwrap(),
            "SELECT *\nFROM [dbo].[orders]\nORDER BY [id] DESC\nOFFSET 0 ROWS FETCH NEXT 500 ROWS ONLY"
        );
        // Every page of a table with a key is one statement on that table,
        // so its rows can be edited.
        for (page, described) in [
            (&first, &single),
            (&second, &composite),
            (&by_date, &composite),
            (&by_key, &composite),
        ] {
            let text = page_statement(page, Some(described)).unwrap();
            assert_eq!(statements(&DIALECT, &text).unwrap().len(), 1, "{text}");
            assert!(
                editable_table(&DIALECT, &text, std::slice::from_ref(&orders)).is_some(),
                "{text}"
            );
        }
        // Nothing to order by: the order of the scan, and whatever order
        // the person chose stays as it is.
        let keyless = keyed(&[], &[]);
        assert_eq!(
            page_statement(&first, Some(&keyless)).unwrap(),
            "SELECT *\nFROM [dbo].[orders]\nORDER BY (SELECT NULL)\nOFFSET 0 ROWS FETCH NEXT 500 ROWS ONLY"
        );
        assert_eq!(
            page_statement(&by_date, Some(&keyless)).unwrap(),
            QueryBuilder::page(&by_date).sql(&DIALECT).unwrap()
        );
    }

    #[test]
    fn the_filter_is_written_for_the_type_of_its_column() {
        let filtered = |column: &str, value: &str, described: Option<&Described>| {
            let page = PageRequest {
                filters: vec![Filter {
                    column: column.into(),
                    op: FilterOp::Contains,
                    value: Cell::Text(value.into()),
                }],
                ..PageRequest::first(table("orders"))
            };
            page_statement(&page, described).unwrap()
        };
        // The form of the dialect: text as it is, letter case folded on
        // both sides, `[` escaped like the other wildcards.
        assert_eq!(
            filtered("label", "50%_[a]!", None),
            "SELECT *\nFROM [dbo].[orders]\nWHERE LOWER(CAST([label] AS NVARCHAR(MAX))) LIKE LOWER(REPLACE(N'%50!%!_[a]!!%', N'[', N'![')) ESCAPE '!'\nORDER BY 1\nOFFSET 0 ROWS FETCH NEXT 500 ROWS ONLY"
        );
        let described = keyed(
            &["id"],
            &[
                ("id", "int"),
                ("label", "nvarchar(255)"),
                ("placed", "datetime"),
                ("price", "money"),
                ("ratio", "float"),
                ("paid", "bit"),
                ("stamp", "timestamp"),
            ],
        );
        assert!(
            filtered("label", "x", Some(&described)).contains(
                "WHERE LOWER(CAST([label] AS NVARCHAR(MAX))) LIKE LOWER(REPLACE(N'%x%', N'[', N'![')) ESCAPE '!'\nORDER BY [id]\n"
            )
        );
        assert!(
            filtered("placed", "2026-10", Some(&described))
                .contains("WHERE LOWER(CONVERT(NVARCHAR(MAX), [placed], 126)) LIKE ")
        );
        assert!(
            filtered("price", "12.5", Some(&described))
                .contains("WHERE CONVERT(NVARCHAR(MAX), [price], 2) LIKE ")
        );
        assert!(
            filtered("ratio", "0.1", Some(&described)).contains(
                "WHERE CAST(TRY_CONVERT(DECIMAL(38,15), [ratio]) AS NVARCHAR(MAX)) LIKE "
            )
        );
        assert!(
            filtered("paid", "tru", Some(&described))
                .contains("WHERE CASE [paid] WHEN 1 THEN N'true' WHEN 0 THEN N'false' END LIKE ")
        );
        assert!(filtered("stamp", "0f", Some(&described)).contains(
            "WHERE LOWER(CONVERT(NVARCHAR(MAX), CAST([stamp] AS VARBINARY(MAX)), 2)) LIKE "
        ));
        // A column the description does not hold gets the plain form.
        assert!(
            filtered("other", "x", Some(&described))
                .contains("WHERE LOWER(CAST([other] AS NVARCHAR(MAX))) LIKE ")
        );
        // Whatever is typed, the filtered page stays one statement on one
        // table, so its rows stay editable.
        let orders = table("orders");
        for column in ["label", "placed", "price", "ratio", "paid", "stamp"] {
            for value in [
                "pend",
                "\\",
                "C:\\",
                "'",
                "'; DROP TABLE a; --",
                "100%",
                "a_b",
                "!",
                "[",
                "]",
                "[a-z]",
                "/* x",
                "-- x",
                "zażółć",
                "N'",
                "GO",
            ] {
                let text = filtered(column, value, Some(&described));
                assert_eq!(statements(&DIALECT, &text).unwrap().len(), 1, "{text}");
                assert!(
                    editable_table(&DIALECT, &text, std::slice::from_ref(&orders)).is_some(),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn the_products_are_presets_of_one_driver() {
        assert_eq!(ENGINE.id(), "mssql");
        assert_eq!(Engine::from_id("SqlServer"), Some(ENGINE));
        assert_eq!(ENGINE.port(), 1433);
        assert_eq!(ENGINE.product("").id, "sql-server");
        for product in SPEC.products {
            assert_eq!(product.default_port, 1433, "{}", product.id);
            // Every product says how to sign in: nothing but a SQL login works.
            let (pl, en) = product.note.expect("a note");
            assert!(pl.contains("kontem SQL"), "{pl}");
            assert!(
                en.contains("SQL login") || en.contains("SQL Server login"),
                "{en}"
            );
            assert_eq!(SPEC.capabilities_of(product), &Capabilities::SQL);
            assert_eq!(SPEC.dialect_of(product), &DIALECT);
            assert!(product.matches("mssql"), "{}", product.id);
        }
        // A server one runs oneself starts without TLS; RDS, whose certificate
        // the bundled roots cannot verify, encrypts without verifying; Azure
        // presents a certificate of a public authority.
        let tls = |id: &str| ENGINE.product(id).default_tls;
        assert_eq!(tls("sql-server"), TlsMode::Disable);
        assert_eq!(tls("azure-sql-database"), TlsMode::VerifyFull);
        assert_eq!(tls("azure-sql-managed-instance"), TlsMode::VerifyFull);
        assert_eq!(tls("amazon-rds-sql-server"), TlsMode::Require);
        assert_eq!(tls("azure-sql-edge"), TlsMode::Disable);
        assert!(
            ENGINE
                .product("azure-sql-managed-instance")
                .note
                .is_some_and(|(pl, en)| pl.contains("3342") && en.contains("3342"))
        );
        assert!(ENGINE.product("amazon-rds-sql-server").matches("aws rds"));
        assert!(ENGINE.product("azure-sql-database").matches("azure sql"));
        // Every code of the driver has a sentence in both languages.
        for code in [
            "mssql_login_required",
            "mssql_strict_needs_tls",
            "mssql_strict_unsupported",
            "mssql_redirect_through_tunnel",
            "mssql_transaction_left_open",
            "mssql_password_expired",
            "mssql_account_locked",
            "mssql_firewall_blocked",
        ] {
            let (pl, en) = (
                describe(code, false).unwrap(),
                describe(code, true).unwrap(),
            );
            assert!(pl != en && pl.ends_with('.') && en.ends_with('.'), "{code}");
            assert_eq!(crate::drivers::describe(code, true), Some(en));
        }
        assert_eq!(describe("database_login_rejected", true), None);
    }

    #[tokio::test]
    async fn a_profile_is_checked_before_anything_is_dialled() {
        let mut profile = crate::model::ConnectionProfile {
            name: "Owned fixture".into(),
            engine: ENGINE,
            host: "db.example.test".into(),
            port: 1433,
            ..Default::default()
        };
        // The database may stay empty; the user name may not.
        profile.validate().unwrap();
        let refused = |profile: crate::model::ConnectionProfile| async move {
            let request = ConnectRequest {
                product: profile.product_spec(),
                profile: &profile,
                forwarded: None,
            };
            Connector::new(&request)
                .await
                .err()
                .map(|error| error.to_string())
        };
        assert_eq!(
            refused(profile.clone()).await.as_deref(),
            Some("mssql_login_required")
        );
        profile.username = "app".into();
        profile.tls = TlsMode::Disable;
        profile.set_option("strict_encryption", "true");
        profile.validate().unwrap();
        assert_eq!(
            refused(profile.clone()).await.as_deref(),
            Some("mssql_strict_needs_tls")
        );
        profile.tls = TlsMode::VerifyFull;
        profile.ca_file = Some("/nonexistent/baza-ca.pem".into());
        assert_eq!(
            refused(profile.clone()).await.as_deref(),
            Some("database_tls_error")
        );
        profile.ca_file = None;
        assert_eq!(refused(profile.clone()).await, None);
        profile.set_option("strict_encryption", "maybe");
        assert_eq!(
            profile.validate().unwrap_err().to_string(),
            "connection_option_invalid"
        );
    }
}
