//! ClickHouse and ClickHouse Cloud, over the HTTP interface of the server
//! (ports 8123 and 8443), not its native protocol.
//!
//! One request carries one statement, so a script is sent statement by
//! statement, inside a session of the server that lasts for the script:
//! `SET`, `USE` and temporary tables of one statement are there for the
//! next. Answers are read as `TabSeparatedWithNamesAndTypes` while they
//! arrive (see `values.rs` for why), and a result that is larger than a
//! result may be is cut here and stopped on the server.
//!
//! What ClickHouse is not, and what the driver does about it:
//!
//! * There is no `UPDATE` of a row and no transaction. A cell is changed
//!   with `ALTER TABLE … UPDATE` and a row is removed with `DELETE FROM`
//!   (or `ALTER TABLE … DELETE` where the table engine has no lightweight
//!   delete), both awaited (`mutations_sync`), so that the page read after
//!   the change shows it.
//! * A key is not unique. The key of a record is the primary key of the
//!   table (its `ORDER BY`), and a change is made only when the key, with
//!   the old value of the cell, finds exactly one row. That is counted
//!   first and checked again afterwards; between the two nothing locks the
//!   row, which is as far as the server lets a client go.
//! * Whether rows can be changed at all is a matter of the table engine:
//!   the MergeTree family and a few others can, `Log`, `Distributed` and
//!   the engines of other systems can only take new rows. The server says
//!   so and the answer becomes a sentence of this driver.
//! * A table needs an engine and a sorting key, and whether a column takes
//!   NULL is part of its type. The table designer of the interface writes
//!   neither, so it is not offered; `CREATE TABLE` is typed in the editor.
mod http;
#[cfg(test)]
mod live;
mod values;

use super::{PageRequest, RowChange, Session};
use crate::builder::FilterOp;
use crate::engine::{
    Badge, Capabilities, ConnectRequest, ConnectionForm, ContainsFilter, DefaultSchema, Dialect,
    EmptyInsert, Engine, EngineSpec, FieldUse, Group, Lexer, OptionField, OptionKind, Paging,
    ParameterStyle, ProductSpec, TextLiteral,
};
use crate::model::{Cell, ColumnInfo, QueryResult, ResultColumn, TableRef, TlsMode};
use crate::sql::{MAX_RESULT_BYTES, MAX_RESULT_ROWS};
use anyhow::{Result, anyhow, bail, ensure};
use futures_util::future::BoxFuture;
use http::{Client, ServerCode};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use values::{Line, Reader, Type, qualified, quote_name, quote_text};

pub const ENGINE: Engine = Engine::new(&SPEC);

pub static SPEC: EngineSpec = EngineSpec {
    id: "clickhouse",
    aliases: &["click-house"],
    portable_name: "clickhouse",
    label: "ClickHouse",
    available: true,
    form: ConnectionForm {
        // Without a name the account lands in its default database.
        database: FieldUse::Optional,
        ..ConnectionForm::SERVER
    },
    capabilities: CAPABILITIES,
    dialect: DIALECT,
    options: OPTIONS,
    products: PRODUCTS,
    connect,
    describe: Some(describe),
};

/// Everything but the table designer: a `CREATE TABLE` of ClickHouse names
/// a table engine and a sorting key, and a column that takes NULL has a
/// type of its own (`Nullable(String)`), none of which the designer of the
/// interface can express.
const CAPABILITIES: Capabilities = Capabilities {
    table_designer: false,
    ..Capabilities::SQL
};

pub const DIALECT: Dialect = Dialect {
    name_quotes: ('`', '`'),
    text: TextLiteral::Backslash,
    bytes: ("unhex('", "')"),
    booleans: ("true", "false"),
    paging: Paging::LimitOffset,
    // `LIKE` takes text only and tells letter case apart. The pages of
    // "Dane tabeli" do not use it (`page_text` below); the query builder
    // does, and `ESCAPE` needs a server that knows the clause.
    contains: ContainsFilter {
        cast: Some(("toString(", ")")),
        operator: "ILIKE",
        pattern: None,
    },
    parameters: ParameterStyle::Question,
    lexer: Lexer {
        backslash_escapes: true,
        backtick_names: true,
        dollar_quotes: true,
        nested_comments: true,
        hash_comments: true,
        ..Lexer::STANDARD
    },
    // The server takes neither form of an empty row: the session writes
    // `INSERT INTO t (first column) VALUES (DEFAULT)` itself.
    empty_insert: EmptyInsert::EmptyLists,
    default_schema: DefaultSchema::Database,
    column_types: &[
        "Int64",
        "UInt64",
        "String",
        "Decimal(38, 10)",
        "Float64",
        "Bool",
        "Date",
        "DateTime",
        "DateTime64(3)",
        "UUID",
    ],
};

static OPTIONS: &[OptionField] = &[OptionField {
    key: "show_system",
    label: ("Pokaż bazy systemowe", "Show system databases"),
    kind: OptionKind::Flag,
    default: "false",
    placeholder: "",
    help: Some((
        "Dodaje do panelu bazy system i INFORMATION_SCHEMA: dzienniki zapytań, części tabel, ustawienia serwera.",
        "Adds the databases system and INFORMATION_SCHEMA to the side panel: query logs, table parts, server settings.",
    )),
    required: false,
}];

static PRODUCTS: &[ProductSpec] = &[
    ProductSpec {
        id: "clickhouse",
        label: "ClickHouse",
        group: Group::Analytics,
        badge: Badge {
            text: "Ch",
            color: [252, 196, 25],
        },
        default_port: 8123,
        default_tls: TlsMode::Disable,
        default_database: "default",
        default_username: "default",
        keywords: &[
            "olap",
            "analytics",
            "analityka",
            "columnar",
            "kolumnowa",
            "http",
            "mergetree",
            "altinity",
        ],
        note: Some((
            "Połączenie przez interfejs HTTP serwera: port 8123, z szyfrowaniem 8443 (porty 9000 i 9440 to protokół natywny). Wiersz w siatce wskazuje klucz tabeli (ORDER BY), który nie musi być unikalny; nową tabelę tworzy CREATE TABLE w edytorze SQL.",
            "The connection uses the HTTP interface of the server: port 8123, or 8443 with encryption (ports 9000 and 9440 are the native protocol). A row of the grid is found by the key of the table (ORDER BY), which need not be unique; a new table is made with CREATE TABLE in the SQL editor.",
        )),
        ..ProductSpec::GENERIC
    },
    ProductSpec {
        id: "clickhouse-cloud",
        label: "ClickHouse Cloud",
        group: Group::Cloud,
        badge: Badge {
            text: "Cc",
            color: [252, 196, 25],
        },
        default_port: 8443,
        default_tls: TlsMode::VerifyFull,
        default_database: "default",
        default_username: "default",
        keywords: &[
            "olap",
            "analytics",
            "analityka",
            "cloud",
            "chmura",
            "aws",
            "gcp",
            "azure",
            "https",
        ],
        note: Some((
            "Host to nazwa usługi z konsoli ClickHouse Cloud (Connect → HTTPS), na przykład abc123.eu-central-1.aws.clickhouse.cloud; port 8443, zawsze z szyfrowaniem. Uśpiona usługa budzi się przy pierwszym połączeniu przez kilkadziesiąt sekund, a jej lista dozwolonych adresów IP musi obejmować Twój.",
            "The host is the name of the service from the ClickHouse Cloud console (Connect → HTTPS), for example abc123.eu-central-1.aws.clickhouse.cloud; port 8443, always encrypted. An idle service takes some tens of seconds to wake up on the first connection, and its IP access list has to include your address.",
        )),
        ..ProductSpec::GENERIC
    },
];

/// Sentences for the result codes of this driver.
fn describe(code: &str, en: bool) -> Option<&'static str> {
    let (pl, english) = match code {
        "clickhouse_native_port" => (
            "Ten port obsługuje protokół natywny ClickHouse, a Baza Studio łączy się przez interfejs HTTP. Wpisz port 8123, a przy szyfrowaniu 8443.",
            "This port speaks the native protocol of ClickHouse, and Baza Studio connects through the HTTP interface. Enter port 8123, or 8443 with encryption.",
        ),
        "clickhouse_not_http_interface" => (
            "Pod tym adresem nie odpowiada interfejs HTTP ClickHouse. Sprawdź host i port: zwykle 8123, a przy szyfrowaniu 8443.",
            "The HTTP interface of ClickHouse does not answer at this address. Check the host and the port: usually 8123, or 8443 with encryption.",
        ),
        "clickhouse_credentials_invalid" => (
            "Nazwa użytkownika albo hasło zawiera znak, którego nie da się przesłać w nagłówku HTTP, na przykład znak nowej linii.",
            "The user name or the password holds a character that cannot be sent in an HTTP header, for example a line break.",
        ),
        "clickhouse_answer_unreadable" => (
            "Odpowiedzi serwera ClickHouse nie udało się odczytać. Spróbuj ponownie.",
            "The answer of the ClickHouse server could not be read. Try again.",
        ),
        "clickhouse_syntax_error" => (
            "ClickHouse nie rozumie tego polecenia: błąd składni. Sprawdź SQL, cudzysłowy i średniki między poleceniami.",
            "ClickHouse does not understand this statement: a syntax error. Check the SQL, the quotes and the semicolons between statements.",
        ),
        "clickhouse_unknown_table" => (
            "Nie ma takiej tabeli. Sprawdź nazwę i bazę, w której tabela się znajduje (baza.tabela).",
            "There is no such table. Check the name and the database the table is in (database.table).",
        ),
        "clickhouse_unknown_database" => (
            "Na serwerze nie ma bazy danych o tej nazwie.",
            "The server has no database with this name.",
        ),
        "clickhouse_unknown_column" => (
            "Polecenie używa nazwy kolumny, której nie ma w tej tabeli albo w tym zapytaniu.",
            "The statement uses a column name that this table or this query does not have.",
        ),
        "clickhouse_unknown_function" => (
            "Polecenie używa funkcji, której ten serwer ClickHouse nie zna.",
            "The statement uses a function this ClickHouse server does not know.",
        ),
        "clickhouse_access_denied" => (
            "Ten użytkownik nie ma na serwerze uprawnień do tej operacji.",
            "This user is not allowed to do this on the server.",
        ),
        "clickhouse_readonly" => (
            "To konto działa w trybie tylko do odczytu, więc nie może zmieniać danych, tabel ani ustawień.",
            "This account works in read-only mode, so it cannot change data, tables or settings.",
        ),
        "clickhouse_memory_limit" => (
            "Zapytanie przekroczyło limit pamięci serwera. Zawęź je warunkiem WHERE albo LIMIT, albo wybierz mniej kolumn.",
            "The query went over the memory limit of the server. Narrow it with WHERE or LIMIT, or select fewer columns.",
        ),
        "clickhouse_too_many_queries" => (
            "Serwer wykonuje teraz zbyt wiele zapytań naraz. Spróbuj ponownie za chwilę.",
            "The server is running too many queries at once. Try again in a moment.",
        ),
        "clickhouse_value_rejected" => (
            "Wartość nie pasuje do typu kolumny. Sprawdź liczbę, datę albo zapis wartości złożonej.",
            "The value does not fit the type of its column. Check the number, the date or the way a composite value is written.",
        ),
        "clickhouse_table_without_key" => (
            "Ta tabela nie ma klucza (ORDER BY), więc siatka nie wskaże w niej jednego wiersza. Wiersze można dodawać, a zmieniać i usuwać poleceniem ALTER TABLE w edytorze SQL.",
            "This table has no key (ORDER BY), so the grid cannot point at one of its rows. Rows can be added; change and remove them with ALTER TABLE in the SQL editor.",
        ),
        "clickhouse_key_not_unique" => (
            "Klucz tej tabeli (ORDER BY) wskazuje tu kilka wierszy, a siatka zmienia tylko jeden. Nic nie zmieniono: użyj ALTER TABLE … UPDATE albo DELETE w edytorze SQL.",
            "The key of this table (ORDER BY) finds several rows here, and the grid changes only one. Nothing was changed: use ALTER TABLE … UPDATE or DELETE in the SQL editor.",
        ),
        "clickhouse_key_column" => (
            "ClickHouse nie zmienia wartości w kolumnach klucza tabeli (ORDER BY, PARTITION BY). Dodaj wiersz z nową wartością i usuń stary.",
            "ClickHouse does not change values in the key columns of a table (ORDER BY, PARTITION BY). Add a row with the new value and remove the old one.",
        ),
        "clickhouse_engine_no_mutations" => (
            "Silnik tej tabeli (na przykład Log albo Distributed) nie pozwala zmieniać ani usuwać wierszy. Wiersze można tylko dodawać.",
            "The engine of this table (Log or Distributed, for example) does not let rows be changed or removed. Rows can only be added.",
        ),
        _ => return None,
    };
    Some(if en { english } else { pl })
}

/// The format every answer is asked for in.
const FORMAT: &str = "TabSeparatedWithNamesAndTypes";
/// A statement ends after this long, as in every driver.
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(120);
/// How much longer the driver waits for the server to end a statement by
/// itself before it does so from outside.
const TIMEOUT_GRACE: Duration = Duration::from_secs(3);
/// How long the server gets to take a request to stop a statement.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// Bytes of a refusal that are read to find its code.
const REFUSAL_BYTES: usize = 64 * 1024;
/// Seconds a session of the server outlives its last statement.
const SESSION_SECONDS: &str = "300";

/// Codes of the server the driver tells apart.
mod code {
    use super::ServerCode;
    pub const UNKNOWN_IDENTIFIER: ServerCode = 47;
    pub const NOT_IMPLEMENTED: ServerCode = 48;
    pub const BAD_ARGUMENTS: ServerCode = 36;
    pub const UNKNOWN_TABLE: ServerCode = 60;
    pub const SYNTAX_ERROR: ServerCode = 62;
    pub const UNKNOWN_DATABASE: ServerCode = 81;
    pub const UNKNOWN_SETTING: ServerCode = 115;
    pub const TIMEOUT_EXCEEDED: ServerCode = 159;
    pub const READONLY: ServerCode = 164;
    pub const SUPPORT_IS_DISABLED: ServerCode = 344;
    pub const QUERY_WAS_CANCELLED: ServerCode = 394;
    pub const CANNOT_UPDATE_COLUMN: ServerCode = 420;
    pub const SETTING_CONSTRAINT_VIOLATION: ServerCode = 452;
    pub const ACCESS_DENIED: ServerCode = 497;
    /// The sign-in was refused, in the words of four versions.
    pub const LOGIN: [ServerCode; 4] = [192, 193, 194, 516];
    /// A value that the type of its column does not take.
    pub const VALUE: [ServerCode; 15] = [
        6, 26, 27, 38, 41, 53, 69, 70, 72, 117, 131, 321, 349, 407, 691,
    ];
}

/// What a refusal of the server is called. The text of the server is never
/// passed on: it repeats names of tables, of accounts and values.
fn refusal(code: Option<ServerCode>) -> anyhow::Error {
    let Some(code) = code else {
        return anyhow!("database_operation_failed");
    };
    anyhow!(match code {
        code::QUERY_WAS_CANCELLED => "query_cancelled",
        code::TIMEOUT_EXCEEDED => "query_timeout",
        code::SYNTAX_ERROR => "clickhouse_syntax_error",
        code::UNKNOWN_TABLE => "clickhouse_unknown_table",
        code::UNKNOWN_DATABASE => "clickhouse_unknown_database",
        code::UNKNOWN_IDENTIFIER | 16 => "clickhouse_unknown_column",
        46 => "clickhouse_unknown_function",
        code::ACCESS_DENIED => "clickhouse_access_denied",
        code::READONLY => "clickhouse_readonly",
        241 => "clickhouse_memory_limit",
        202 => "clickhouse_too_many_queries",
        code::CANNOT_UPDATE_COLUMN => "clickhouse_key_column",
        other if code::LOGIN.contains(&other) => "database_login_rejected",
        other if code::VALUE.contains(&other) => "clickhouse_value_rejected",
        other => return anyhow!("database_error [{other}]"),
    })
}

/// The code of the server behind a failure of [`ClickHouseSession::request`],
/// for the callers that act on it.
#[derive(Debug)]
struct Refused(Option<ServerCode>);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        refusal(self.0).fmt(f)
    }
}
impl std::error::Error for Refused {}

fn refused(error: &anyhow::Error) -> Option<ServerCode> {
    error
        .downcast_ref::<Refused>()
        .and_then(|refused| refused.0)
}

/// A failure as its result code only, the way a caller of the driver gets it.
fn plain(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<Refused>() {
        Some(inner) => refusal(inner.0),
        None => error,
    }
}

fn connect<'a>(request: &'a ConnectRequest<'a>) -> BoxFuture<'a, Result<Arc<dyn Session>>> {
    Box::pin(async move { Ok(Arc::new(open(request).await?) as Arc<dyn Session>) })
}

/// Open a session: reach the server, sign in and learn what it is.
async fn open(request: &ConnectRequest<'_>) -> Result<ClickHouseSession> {
    let mut session = ClickHouseSession {
        client: Client::new(request).await?,
        limited: false,
        framed: true,
        version: (0, 0),
        show_system: request.option("show_system") == "true",
        described: Mutex::default(),
        stats: Mutex::new(0),
    };
    let greeting = match session.greet().await {
        // An account that may not change settings (`readonly = 1`, or a
        // profile with constraints) still reads: without the settings. So
        // does a server too old to know one of them.
        Err(error)
            if matches!(
                refused(&error),
                Some(code::READONLY | code::SETTING_CONSTRAINT_VIOLATION | code::UNKNOWN_SETTING)
            ) =>
        {
            session.limited = true;
            session.greet().await
        }
        other => other,
    }
    .map_err(|error| match refused(&error) {
        Some(code::UNKNOWN_DATABASE) => anyhow!("database_not_found"),
        Some(code::ACCESS_DENIED) => anyhow!("database_access_denied"),
        _ => plain(error),
    })?;
    session.framed = greeting.framed;
    session.version = greeting.version;
    Ok(session)
}

/// What the server said about itself when the session was opened.
struct Greeting {
    version: (u32, u32),
    framed: bool,
}

/// The columns of a table and the engine that stores it: what a change of
/// one record has to know.
struct Described {
    engine: String,
    columns: Vec<(String, Type)>,
    /// Names of the key columns, in their order.
    key: Vec<String>,
}
impl Described {
    fn column(&self, name: &str) -> Option<&Type> {
        self.columns
            .iter()
            .find(|(column, _)| column == name)
            .map(|(_, kind)| kind)
    }
}

/// How one request is sent.
#[derive(Clone, Copy)]
struct Sending<'a> {
    /// Settings the statement needs. They are sent whatever the account may
    /// change: without them the statement would mean something else.
    settings: &'a [(&'a str, &'a str)],
    /// Session of the server the statement belongs to, and whether it is
    /// the first statement of that session.
    session: Option<(&'a str, bool)>,
    /// When the statement has to be over.
    deadline: Instant,
}
impl Sending<'_> {
    fn now() -> Self {
        Self {
            settings: &[],
            session: None,
            deadline: Instant::now() + STATEMENT_TIMEOUT,
        }
    }
}

/// What a request brought back.
#[derive(Default)]
struct Answer {
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<Cell>>,
    truncated: bool,
    /// Rows the statement wrote, as the server counted them.
    written: u64,
    framed: bool,
}

struct ClickHouseSession {
    client: Client,
    /// The account may not change settings: the limits of a result and of
    /// the running time are then kept on this side only.
    limited: bool,
    /// The server frames a failure that comes after the first rows.
    framed: bool,
    /// Major and minor version of the server.
    version: (u32, u32),
    show_system: bool,
    /// The tables whose columns were read, by database and name: the types
    /// that literals of a page and of a change are written with.
    described: Mutex<HashMap<(String, String), Arc<Described>>>,
    /// Which statistics query the server answered last: the full one, or a
    /// shorter one where a system table is missing or closed to the account.
    stats: Mutex<usize>,
}

impl ClickHouseSession {
    /// Ask the server for its version: the first request of a session.
    async fn greet(&self) -> Result<Greeting> {
        let answer = self
            .request(
                b"SELECT version()".to_vec(),
                Sending::now(),
                &CancellationToken::new(),
            )
            .await
            .map_err(|error| {
                // Something answered, but nothing that is a ClickHouse.
                if error.to_string() == "clickhouse_answer_unreadable" {
                    anyhow!("clickhouse_not_http_interface")
                } else {
                    error
                }
            })?;
        let text = answer
            .rows
            .first()
            .and_then(|row| row.first())
            .map(Cell::display)
            .ok_or_else(|| anyhow!("clickhouse_not_http_interface"))?;
        let mut parts = text.split('.').map(|part| part.parse::<u32>().unwrap_or(0));
        Ok(Greeting {
            version: (parts.next().unwrap_or(0), parts.next().unwrap_or(0)),
            framed: answer.framed,
        })
    }

    /// Send one statement and read its answer. Honours `cancel` and the
    /// deadline, cuts a result at what a result may hold, and in each of
    /// those cases stops the statement on the server as well.
    async fn request(
        &self,
        body: Vec<u8>,
        sending: Sending<'_>,
        cancel: &CancellationToken,
    ) -> Result<Answer> {
        if cancel.is_cancelled() {
            bail!("query_cancelled");
        }
        let id = uuid::Uuid::new_v4().to_string();
        let left = sending.deadline.saturating_duration_since(Instant::now());
        ensure!(!left.is_zero(), "query_timeout");
        let seconds = left.as_secs().max(1).to_string();
        let rows = (MAX_RESULT_ROWS + 1).to_string();
        let mut parameters = vec![("query_id", id.as_str()), ("default_format", FORMAT)];
        if !self.limited {
            parameters.extend([
                // The server ends the statement itself and stops producing a
                // result nobody will read; a result with more rows than it
                // may hold arrives with at least one row too many, which is
                // how this side knows that it was cut.
                ("max_execution_time", seconds.as_str()),
                ("max_result_rows", rows.as_str()),
                ("result_overflow_mode", "break"),
                ("cancel_http_readonly_queries_on_client_close", "1"),
                // Every digit of the scale, as the other drivers show it.
                ("output_format_decimal_trailing_zeros", "1"),
            ]);
        }
        parameters.extend_from_slice(sending.settings);
        let with_database = match sending.session {
            Some((session, first)) => {
                parameters.extend([
                    ("session_id", session),
                    ("session_timeout", SESSION_SECONDS),
                ]);
                // A later statement of a script keeps the database an
                // earlier `USE` chose.
                first
            }
            None => true,
        };
        let mut work = Box::pin(self.exchange(&parameters, body, with_database));
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(anyhow!("query_cancelled")),
            _ = tokio::time::sleep(left + TIMEOUT_GRACE) => Err(anyhow!("query_timeout")),
            result = &mut work => result,
        };
        // Dropping the request closes its connection and frees its place
        // among the requests of the session.
        drop(work);
        // Whatever ended early may still run on the server: stop it there.
        let stop = match &outcome {
            Ok(answer) => answer.truncated,
            Err(error) => {
                refused(error).is_none()
                    && matches!(
                        error.to_string().as_str(),
                        "query_cancelled" | "query_timeout"
                    )
            }
        };
        if stop {
            self.stop(&id).await;
        }
        outcome
    }

    /// Stop a statement by its id, from another request.
    async fn stop(&self, id: &str) {
        let statement = format!("KILL QUERY WHERE query_id = {}", quote_text(id));
        let _ = tokio::time::timeout(STOP_TIMEOUT, async {
            if let Ok(mut response) = self
                .client
                .send_now(&[("default_format", "Null")], statement.into_bytes())
                .await
            {
                while let Ok(Some(_)) = response.chunk().await {}
            }
        })
        .await;
    }

    /// One request and its answer, without the watch over it.
    async fn exchange(
        &self,
        parameters: &[(&str, &str)],
        body: Vec<u8>,
        with_database: bool,
    ) -> Result<Answer> {
        let mut reply = self
            .client
            .send(parameters, body, with_database)
            .await
            .map_err(|error| http::transport_error(&error))?;
        let headers = reply.response.headers().clone();
        let lost = |_| anyhow!("database_network_error");
        if !reply.response.status().is_success() {
            let mut text = Vec::new();
            while text.len() < REFUSAL_BYTES {
                match reply.response.chunk().await {
                    Ok(Some(chunk)) => text.extend_from_slice(&chunk),
                    _ => break,
                }
            }
            return Err(match http::server_code(&headers, &text) {
                Some(code) => Refused(Some(code)).into(),
                // The native port answers a request in HTTP with a note
                // that says so.
                None if text
                    .windows(21)
                    .any(|window| window == b"for clickhouse-client") =>
                {
                    anyhow!("clickhouse_native_port")
                }
                None => anyhow!("clickhouse_not_http_interface"),
            });
        }
        let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
        let mut answer = Answer {
            written: header("x-clickhouse-summary")
                .and_then(|summary| serde_json::from_str::<serde_json::Value>(summary).ok())
                .and_then(|summary| summary["written_rows"].as_str()?.parse().ok())
                .unwrap_or(0),
            framed: header("x-clickhouse-exception-tag").is_some(),
            ..Answer::default()
        };
        // An answer without a single header of ClickHouse comes from
        // something else that speaks HTTP on this port.
        ensure!(
            header("x-clickhouse-summary").is_some()
                || header("x-clickhouse-query-id").is_some()
                || header("x-clickhouse-format").is_some(),
            "clickhouse_answer_unreadable"
        );
        // A statement that names a format of its own (`… FORMAT CSV`) is
        // answered in it: the text is shown as it is.
        if let Some(format) = header("x-clickhouse-format").filter(|format| *format != FORMAT) {
            let mut text = Vec::new();
            let mut ended = Ok(());
            loop {
                match reply.response.chunk().await {
                    Ok(Some(chunk)) if text.len() + chunk.len() > MAX_RESULT_BYTES => {
                        answer.truncated = true;
                        break;
                    }
                    Ok(Some(chunk)) => text.extend_from_slice(&chunk),
                    Ok(None) => break,
                    Err(error) => {
                        ended = Err(error);
                        break;
                    }
                }
            }
            // After a failure the server ends the answer without closing
            // it properly: what it wrote before that says why.
            if let Some(code) = late_failure(&text) {
                return Err(Refused(Some(code)).into());
            }
            ended.map_err(lost)?;
            answer.columns = vec![ResultColumn {
                name: format.to_owned(),
                data_type: "String".into(),
            }];
            match String::from_utf8(text) {
                Ok(text) => {
                    for line in text.lines() {
                        if answer.rows.len() >= MAX_RESULT_ROWS {
                            answer.truncated = true;
                            break;
                        }
                        answer.rows.push(vec![Cell::Text(line.to_owned())]);
                    }
                }
                // A binary format is not something to show.
                Err(_) => answer
                    .rows
                    .push(vec![Cell::Unsupported(format!("<{format}>"))]),
            }
            return Ok(answer);
        }
        let mut reader = Reader::new(answer.framed);
        let mut bytes = 0usize;
        // The message of a failure that came after the first rows.
        let mut failure: Option<Vec<u8>> = None;
        loop {
            let chunk = match reply.response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                // After a failure the server ends the answer without
                // closing it properly: its message was read already.
                Err(_) if failure.is_some() => break,
                Err(error) => return Err(lost(error)),
            };
            if let Some(text) = &mut failure {
                text.extend_from_slice(&chunk);
                if text.len() >= REFUSAL_BYTES {
                    break;
                }
                continue;
            }
            let stopped = reader.feed(&chunk, |line| {
                Ok(match line {
                    Line::Header => true,
                    Line::Exception(text) => {
                        failure = Some(text);
                        false
                    }
                    Line::Row(cells) => {
                        let size: usize = cells
                            .iter()
                            .map(|cell| match cell {
                                Cell::Bytes(value) => value.len(),
                                other => other.display().len(),
                            })
                            .sum();
                        if answer.rows.len() >= MAX_RESULT_ROWS
                            || bytes.saturating_add(size) > MAX_RESULT_BYTES
                        {
                            answer.truncated = true;
                            false
                        } else {
                            bytes += size;
                            answer.rows.push(cells);
                            true
                        }
                    }
                })
            })?;
            match (stopped, &mut failure) {
                (Some(taken), Some(text)) => text.extend_from_slice(&chunk[taken..]),
                // The result is full: the rest is not read, and dropping the
                // answer closes the connection it would arrive on.
                (Some(_), None) => break,
                (None, _) => {}
            }
        }
        if let Some(text) = failure {
            return Err(Refused(http::code_in_text(&text)).into());
        }
        // An answer that ends in the middle of a line was cut off.
        ensure!(
            answer.truncated || reader.leftover().is_empty(),
            "database_network_error"
        );
        answer.columns = reader
            .names
            .iter()
            .zip(&reader.types)
            .map(|(name, kind)| ResultColumn {
                name: name.clone(),
                data_type: kind.name.clone(),
            })
            .collect();
        Ok(answer)
    }

    /// Run one statement of the driver itself and return its rows.
    async fn ask(&self, statement: &str) -> Result<Vec<Vec<Cell>>> {
        self.ask_with(statement, &[]).await
    }

    async fn ask_with(&self, statement: &str, settings: &[(&str, &str)]) -> Result<Vec<Vec<Cell>>> {
        let sending = Sending {
            settings,
            ..Sending::now()
        };
        Ok(self
            .request(
                statement.as_bytes().to_vec(),
                sending,
                &CancellationToken::new(),
            )
            .await?
            .rows)
    }

    /// The database a table without one belongs to, as an expression.
    fn database_of(table: &TableRef) -> String {
        if table.schema.is_empty() {
            "currentDatabase()".into()
        } else {
            quote_text(&table.schema)
        }
    }

    /// Read the columns of a table as `SELECT *` returns them, with their
    /// types, the key and the engine, and remember them.
    async fn describe(&self, table: &TableRef) -> Result<(Arc<Described>, Vec<ColumnInfo>)> {
        let database = Self::database_of(table);
        let name = quote_text(&table.name);
        // A materialized, alias or ephemeral column is not part of
        // `SELECT *` and takes no value of an `INSERT`.
        let rows = self
            .ask(&format!(
                "SELECT name, type, default_kind, default_expression, is_in_primary_key, \
                 (SELECT any(engine) FROM system.tables WHERE database = {database} AND name = {name}) \
                 FROM system.columns WHERE database = {database} AND table = {name} \
                 AND default_kind NOT IN ('MATERIALIZED', 'ALIAS', 'EPHEMERAL') ORDER BY position"
            ))
            .await
            .map_err(plain)?;
        let mut described = Described {
            engine: String::new(),
            columns: Vec::new(),
            key: Vec::new(),
        };
        let mut columns = Vec::new();
        for row in rows.iter().filter(|row| row.len() >= 6) {
            let name = row[0].display();
            let kind = Type::parse(&row[1].display());
            let key = row[4].display() == "1";
            described.engine = row[5].display();
            columns.push(ColumnInfo {
                name: name.clone(),
                data_type: kind.name.clone(),
                nullable: kind.nullable,
                primary_key: key,
                default: (!row[2].display().is_empty()).then(|| row[3].display()),
            });
            if key {
                described.key.push(name.clone());
            }
            described.columns.push((name, kind));
        }
        let described = Arc::new(described);
        if let Ok(mut known) = self.described.lock() {
            // A session that walks through thousands of tables keeps the
            // types of the last ones only.
            if known.len() >= 512 {
                known.clear();
            }
            known.insert(
                (table.schema.clone(), table.name.clone()),
                described.clone(),
            );
        }
        Ok((described, columns))
    }

    /// What is remembered about a table, if its columns were read before.
    fn known(&self, table: &TableRef) -> Option<Arc<Described>> {
        self.described
            .lock()
            .ok()?
            .get(&(table.schema.clone(), table.name.clone()))
            .cloned()
    }

    /// The statistics queries, from the full one to the one every account
    /// may run.
    fn stats_statements() -> [String; 3] {
        // With the time zone values of `DateTime` columns are shown in. (The
        // monitor lists a version under its figures only when it is longer
        // than a number.)
        const ABOUT: &str = "concat(version(), ', time zone ', timezone()) AS version, currentDatabase() AS database";
        const TABLES: &str = "(SELECT count() FROM system.tables WHERE database NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') AND NOT is_temporary) AS tables";
        // Connections of clients, whatever protocol they speak, and the
        // statements they are running, without this one.
        const ACTIVITY: &str = "(SELECT sum(value) FROM system.metrics WHERE metric IN ('TCPConnection', 'HTTPConnection', 'MySQLConnection', 'PostgreSQLConnection')) AS connections, \
             (SELECT count() FROM system.processes WHERE query_id != queryID()) AS active";
        const LIMIT: &str = "(SELECT toUInt64OrNull(any(value)) FROM system.server_settings WHERE name = 'max_connections') AS max_connections";
        const MORE: &str = "(SELECT formatReadableSize(any(value)) FROM system.metrics WHERE metric = 'MemoryTracking') AS memory, \
             (SELECT nullIf(round(max(elapsed), 1), 0) FROM system.processes WHERE query_id != queryID()) AS `longest query (s)`, \
             (SELECT any(value) FROM system.metrics WHERE metric = 'Merge') AS merges, \
             (SELECT any(value) FROM system.metrics WHERE metric = 'PartMutation') AS mutations, \
             (SELECT toUInt64(any(value)) FROM system.asynchronous_metrics WHERE metric = 'TotalPartsOfMergeTreeTables') AS parts, \
             (SELECT formatReadableQuantity(any(value)) FROM system.asynchronous_metrics WHERE metric = 'TotalRowsOfMergeTreeTables') AS `rows`, \
             (SELECT formatReadableSize(any(value)) FROM system.asynchronous_metrics WHERE metric = 'TotalBytesOfMergeTreeTables') AS `data size`, \
             (SELECT round(any(value), 2) FROM system.asynchronous_metrics WHERE metric = 'LoadAverage1') AS `load`, \
             formatReadableTimeDelta(uptime()) AS uptime";
        [
            format!("SELECT {ACTIVITY}, {LIMIT}, {TABLES}, {ABOUT}, {MORE}"),
            format!("SELECT {ACTIVITY}, {TABLES}, {ABOUT}"),
            format!("SELECT {TABLES}, {ABOUT}"),
        ]
    }

    /// The condition of a page as text, with literals of the column types
    /// where they are known.
    fn condition(described: Option<&Described>, filter: &crate::builder::Filter) -> Result<String> {
        let column = quote_name(&filter.column)?;
        let kind = described.and_then(|table| table.column(&filter.column));
        let literal = |cell: &Cell| match kind {
            Some(kind) => kind.literal(cell),
            None => values::untyped_literal(cell),
        };
        Ok(match (&filter.op, &filter.value) {
            (FilterOp::IsNull, _) | (FilterOp::Equal, Cell::Null) => format!("{column} IS NULL"),
            (FilterOp::IsNotNull, _) | (FilterOp::NotEqual, Cell::Null) => {
                format!("{column} IS NOT NULL")
            }
            // No pattern, so nothing of the text is a wildcard; letter case
            // is ignored for every alphabet, and a number is searched as
            // the text the grid shows.
            (FilterOp::Contains, value) => format!(
                "positionCaseInsensitiveUTF8(toString({column}), {}) > 0",
                quote_text(&value.display())
            ),
            (FilterOp::Equal, value) => format!("{column} = {}", literal(value)?),
            (FilterOp::NotEqual, value) => format!("{column} <> {}", literal(value)?),
            (FilterOp::Greater, value) => format!("{column} > {}", literal(value)?),
            (FilterOp::Less, value) => format!("{column} < {}", literal(value)?),
        })
    }

    /// `key = value AND …` for the key of a record.
    fn key_condition(described: &Described, key: &[(String, Cell)]) -> Result<String> {
        ensure!(
            !key.is_empty()
                && key
                    .iter()
                    .all(|(_, value)| !matches!(value, Cell::Null | Cell::Unsupported(_))),
            "clickhouse_table_without_key"
        );
        Ok(key
            .iter()
            .map(|(name, value)| match described.column(name) {
                Some(kind) => kind.predicate(name, value),
                None => Err(anyhow!("column_not_found")),
            })
            .collect::<Result<Vec<_>>>()?
            .join(" AND "))
    }

    /// The statement of a change, written with what is known about the table.
    fn change(described: &Described, change: &RowChange) -> Result<String> {
        match change {
            RowChange::UpdateCell {
                table,
                column,
                old,
                new,
                key,
            } => {
                let kind = described
                    .column(column)
                    .ok_or_else(|| anyhow!("column_not_found"))?;
                Ok(format!(
                    "ALTER TABLE {} UPDATE {} = {} WHERE {} AND {}",
                    qualified(table)?,
                    quote_name(column)?,
                    kind.literal(new)?,
                    Self::key_condition(described, key)?,
                    kind.predicate(column, old)?
                ))
            }
            RowChange::InsertRow { table, values } => {
                let target = qualified(table)?;
                if values.is_empty() {
                    let first = described
                        .columns
                        .first()
                        .ok_or_else(|| anyhow!("column_not_found"))?;
                    return Ok(format!(
                        "INSERT INTO {target} ({}) VALUES (DEFAULT)",
                        quote_name(&first.0)?
                    ));
                }
                let mut names = Vec::new();
                let mut literals = Vec::new();
                for (name, value) in values {
                    let kind = described
                        .column(name)
                        .ok_or_else(|| anyhow!("column_not_found"))?;
                    names.push(quote_name(name)?);
                    literals.push(kind.literal(value)?);
                }
                Ok(format!(
                    "INSERT INTO {target} ({}) VALUES ({})",
                    names.join(", "),
                    literals.join(", ")
                ))
            }
            RowChange::DeleteRow { table, key } => {
                let target = qualified(table)?;
                let condition = Self::key_condition(described, key)?;
                Ok(if described.engine.ends_with("MergeTree") {
                    format!("DELETE FROM {target} WHERE {condition}")
                } else {
                    format!("ALTER TABLE {target} DELETE WHERE {condition}")
                })
            }
        }
    }

    /// How many rows a condition finds: none, one, or more than one.
    async fn found(&self, table: &TableRef, condition: &str) -> Result<usize> {
        let rows = self
            .ask(&format!(
                "SELECT count() FROM (SELECT 1 FROM {} WHERE {condition} LIMIT 2)",
                qualified(table)?
            ))
            .await?;
        rows.first()
            .and_then(|row| row.first())
            .and_then(|cell| cell.display().parse().ok())
            .ok_or_else(|| anyhow!("clickhouse_answer_unreadable"))
    }

    /// Run a statement that changes or removes rows and wait until every
    /// replica has done it, so that the next page shows the change.
    async fn mutate(&self, statement: &str) -> Result<()> {
        self.ask_with(statement, &[("mutations_sync", "2")])
            .await
            .map(|_| ())
            .map_err(|error| match refused(&error) {
                // The engine has no mutations, or none of this kind.
                Some(code::NOT_IMPLEMENTED | code::SUPPORT_IS_DISABLED) => {
                    anyhow!("clickhouse_engine_no_mutations")
                }
                _ => error,
            })
    }

    /// Make one change of one record. See the head of this file for what
    /// "one record" can mean on this server.
    async fn change_one(&self, change: &RowChange) -> Result<()> {
        let table = match change {
            RowChange::UpdateCell { table, .. }
            | RowChange::InsertRow { table, .. }
            | RowChange::DeleteRow { table, .. } => table,
        };
        // Read again: a type may have been altered since the page was read.
        let (described, _) = self.describe(table).await?;
        ensure!(!described.columns.is_empty(), "clickhouse_unknown_table");
        let statement = Self::change(&described, change)?;
        match change {
            RowChange::InsertRow { .. } => {
                // A replicated table drops a block it has seen before: the
                // same row added twice would be added once.
                self.ask_with(&statement, &[("insert_deduplicate", "0")])
                    .await?;
            }
            RowChange::UpdateCell {
                column,
                old,
                new,
                key,
                ..
            } => {
                let kind = described
                    .column(column)
                    .ok_or_else(|| anyhow!("column_not_found"))?;
                let record = Self::key_condition(&described, key)?;
                let before = format!("{record} AND {}", kind.predicate(column, old)?);
                match self.found(table, &before).await? {
                    0 => bail!("row_changed_reload_required"),
                    1 => {}
                    _ => bail!("clickhouse_key_not_unique"),
                }
                self.mutate(&statement).await?;
                // Nothing held the row between the count and the change:
                // see that it holds the new value now.
                let after = format!("{record} AND {}", kind.predicate(column, new)?);
                ensure!(
                    self.found(table, &after).await? >= 1,
                    "row_changed_reload_required"
                );
            }
            RowChange::DeleteRow { key, .. } => {
                let record = Self::key_condition(&described, key)?;
                match self.found(table, &record).await? {
                    0 => bail!("row_changed_reload_required"),
                    1 => {}
                    _ => bail!("clickhouse_key_not_unique"),
                }
                match self.mutate(&statement).await {
                    // A MergeTree table with a projection, and a server
                    // before lightweight deletes, take the heavier form.
                    Err(error)
                        if statement.starts_with("DELETE FROM")
                            && (error.to_string() == "clickhouse_engine_no_mutations"
                                || matches!(
                                    refused(&error),
                                    Some(code::BAD_ARGUMENTS | code::SYNTAX_ERROR)
                                )) =>
                    {
                        self.mutate(&format!(
                            "ALTER TABLE {} DELETE WHERE {record}",
                            qualified(table)?
                        ))
                        .await?
                    }
                    other => other?,
                }
            }
        }
        Ok(())
    }
}

/// The code in a failure that arrived inside an answer of another format.
fn late_failure(text: &[u8]) -> Option<ServerCode> {
    const FRAME: &[u8] = b"__exception__\r\n";
    let start = text
        .windows(FRAME.len())
        .position(|window| window == FRAME)?;
    http::code_in_text(&text[start..])
}

impl Session for ClickHouseSession {
    fn dialect(&self) -> &Dialect {
        &DIALECT
    }

    fn execute<'a>(
        &'a self,
        text: &'a str,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            let started = Instant::now();
            let statements = crate::sql::statements(&DIALECT, text)?;
            ensure!(!statements.is_empty(), "sql_invalid");
            // The statements of a script share a session of the server.
            let session = (statements.len() > 1).then(|| uuid::Uuid::new_v4().to_string());
            let deadline = started + STATEMENT_TIMEOUT;
            let mut result = QueryResult::default();
            for (index, statement) in statements.iter().enumerate() {
                let sending = Sending {
                    settings: &[],
                    session: session.as_deref().map(|id| (id, index == 0)),
                    deadline,
                };
                let answer = self
                    .request(statement.as_bytes().to_vec(), sending, &cancel)
                    .await
                    .map_err(plain)?;
                result.affected = result.affected.saturating_add(answer.written);
                // The last result set is the one that is shown.
                if !answer.columns.is_empty() {
                    result.columns = answer.columns;
                    result.rows = answer.rows;
                    result.truncated = answer.truncated;
                }
            }
            result.duration_ms = started.elapsed().as_millis() as u64;
            Ok(result)
        })
    }

    fn containers<'a>(&'a self) -> BoxFuture<'a, Result<Vec<TableRef>>> {
        Box::pin(async move {
            let hidden = if self.show_system {
                ""
            } else {
                " AND database NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema')"
            };
            // The table a materialized view keeps its rows in (`.inner…`)
            // is part of the view, not something to open.
            let rows = self
                .ask(&format!(
                    "SELECT database, name, engine FROM system.tables WHERE NOT is_temporary AND name NOT LIKE '.inner%'{hidden} ORDER BY database, name"
                ))
                .await
                .map_err(plain)?;
            Ok(rows
                .into_iter()
                .filter(|row| row.len() >= 3)
                .map(|row| TableRef {
                    schema: row[0].display(),
                    name: row[1].display(),
                    kind: container_kind(&row[2].display()).into(),
                })
                .collect())
        })
    }

    fn columns<'a>(&'a self, container: &'a TableRef) -> BoxFuture<'a, Result<Vec<ColumnInfo>>> {
        Box::pin(async move { Ok(self.describe(container).await?.1) })
    }

    fn page_text(&self, page: &PageRequest) -> Result<String> {
        ensure!(
            (1..=20_000).contains(&page.limit) && page.offset <= 1_000_000_000,
            "page_invalid"
        );
        let described = self.known(&page.container);
        let mut text = format!("SELECT *\nFROM {}", qualified(&page.container)?);
        let mut conditions = page
            .filters
            .iter()
            .map(|filter| Self::condition(described.as_deref(), filter))
            .collect::<Result<Vec<_>>>()?;
        if let Some(typed) = &page.condition {
            conditions.push(crate::sql::typed_condition(&DIALECT, typed)?);
        }
        if !conditions.is_empty() {
            text.push_str(&format!("\nWHERE {}", conditions.join(" AND ")));
        }
        match &page.order_by {
            Some(first) => {
                let terms = std::iter::once(first)
                    .chain(&page.then_by)
                    .map(|(column, descending)| {
                        Ok(format!(
                            "{} {}",
                            quote_name(column)?,
                            if *descending { "DESC" } else { "ASC" }
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                text.push_str(&format!("\nORDER BY {}", terms.join(", ")));
            }
            // Without an order the server returns rows as its threads find
            // them, and two pages would overlap. The key of the table is
            // the order its data is stored in, so it costs next to nothing.
            None => {
                let key = described.map(|table| table.key.clone()).unwrap_or_default();
                if !key.is_empty() {
                    let names = key
                        .iter()
                        .map(|name| quote_name(name))
                        .collect::<Result<Vec<_>>>()?;
                    text.push_str(&format!("\nORDER BY {}", names.join(", ")));
                }
            }
        }
        text.push_str(&format!("\nLIMIT {} OFFSET {}", page.limit, page.offset));
        Ok(text)
    }

    fn change_text(&self, change: &RowChange) -> Result<String> {
        let table = match change {
            RowChange::UpdateCell { table, .. }
            | RowChange::InsertRow { table, .. }
            | RowChange::DeleteRow { table, .. } => table,
        };
        match self.known(table) {
            Some(described) => Self::change(&described, change),
            // The columns were never read through this session: the text
            // for the history is the plain one of the dialect.
            None => crate::sql::change_statement(&DIALECT, change),
        }
    }

    fn apply<'a>(&'a self, change: &'a RowChange) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move { self.change_one(change).await.map_err(plain) })
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
            if rows.is_empty() {
                return Ok(QueryResult::default());
            }
            let (described, _) = self.describe(container).await?;
            let kinds = columns
                .iter()
                .map(|name| {
                    described
                        .column(name)
                        .ok_or_else(|| anyhow!("import_column_unknown"))
                })
                .collect::<Result<Vec<_>>>()?;
            let names = columns
                .iter()
                .map(|name| quote_name(name))
                .collect::<Result<Vec<_>>>()?;
            // One statement with its rows behind it. The server reads them
            // all before it writes any: a row it cannot read leaves nothing
            // behind.
            let mut body = format!(
                "INSERT INTO {} ({}) FORMAT TabSeparated\n",
                qualified(container)?,
                names.join(", ")
            )
            .into_bytes();
            for row in rows {
                for (index, (cell, kind)) in row.iter().zip(&kinds).enumerate() {
                    if index > 0 {
                        body.push(b'\t');
                    }
                    kind.field(cell, &mut body)?;
                }
                body.push(b'\n');
            }
            let mut settings = vec![
                // The same rows imported twice are meant twice.
                ("insert_deduplicate", "0"),
                // Dates as files hold them: `2024-05-06T07:08:09Z` as well.
                ("date_time_input_format", "best_effort"),
            ];
            if self.version >= (21, 11) {
                // Not handed to a queue that would write them later.
                settings.push(("async_insert", "0"));
            }
            let sending = Sending {
                settings: &settings,
                ..Sending::now()
            };
            let answer = self.request(body, sending, &cancel).await.map_err(plain)?;
            Ok(QueryResult {
                affected: if answer.written > 0 {
                    answer.written
                } else {
                    rows.len() as u64
                },
                duration_ms: started.elapsed().as_millis() as u64,
                ..QueryResult::default()
            })
        })
    }

    fn server_stats<'a>(&'a self) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            let started = Instant::now();
            let statements = Self::stats_statements();
            let first = self.stats.lock().map(|level| *level).unwrap_or(0);
            let mut failure = anyhow!("database_operation_failed");
            for (level, statement) in statements.iter().enumerate().skip(first) {
                match self
                    .request(
                        statement.as_bytes().to_vec(),
                        Sending::now(),
                        &CancellationToken::new(),
                    )
                    .await
                {
                    Ok(answer) => {
                        if let Ok(mut known) = self.stats.lock() {
                            *known = level;
                        }
                        let mut result = QueryResult {
                            columns: answer.columns,
                            rows: answer.rows,
                            duration_ms: started.elapsed().as_millis() as u64,
                            ..QueryResult::default()
                        };
                        drop_missing(&mut result);
                        return Ok(result);
                    }
                    // A system table that is missing on this version or
                    // closed to this account: ask for less.
                    Err(error) if refused(&error).is_some() => failure = plain(error),
                    Err(error) => return Err(error),
                }
            }
            Err(failure)
        })
    }

    fn close<'a>(&'a self) -> BoxFuture<'a, ()> {
        // Nothing is held open: every request is a connection of its own,
        // or one that the client closes two seconds after it was last used.
        Box::pin(async {})
    }
}

/// What a table engine is in the side panel.
fn container_kind(engine: &str) -> &'static str {
    match engine {
        "View" | "LiveView" | "WindowView" => "view",
        "MaterializedView" => "materialized view",
        "Dictionary" => "dictionary",
        // Reading from one of these takes messages off a queue.
        "Kafka" | "RabbitMQ" | "NATS" | "S3Queue" | "AzureQueue" | "FileLog" => "queue",
        // The views of the server onto itself: nothing to change there.
        _ if engine.starts_with("System") => "system view",
        _ => "table",
    }
}

/// Take a figure the server has no value for out of the statistics.
fn drop_missing(result: &mut QueryResult) {
    let Some(row) = result.rows.first() else {
        return;
    };
    let keep: Vec<bool> = row.iter().map(|cell| !matches!(cell, Cell::Null)).collect();
    let mut index = 0;
    result.columns.retain(|_| {
        index += 1;
        keep[index - 1]
    });
    for row in &mut result.rows {
        let mut index = 0;
        row.retain(|_| {
            index += 1;
            keep.get(index - 1).copied().unwrap_or(true)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::Filter;

    fn table() -> TableRef {
        TableRef {
            schema: "shop".into(),
            name: "orders".into(),
            kind: "table".into(),
        }
    }

    fn described() -> Described {
        Described {
            engine: "ReplicatedMergeTree".into(),
            columns: [
                ("id", "Int64"),
                ("day", "Date"),
                ("label", "Nullable(String)"),
                ("amount", "Nullable(Decimal(38, 10))"),
                ("tags", "Array(String)"),
            ]
            .into_iter()
            .map(|(name, kind)| (name.to_owned(), Type::parse(kind)))
            .collect(),
            key: vec!["id".into(), "day".into()],
        }
    }

    fn key() -> Vec<(String, Cell)> {
        vec![
            ("id".into(), Cell::Integer("7".into())),
            ("day".into(), Cell::Text("2024-05-06".into())),
        ]
    }

    #[test]
    fn a_change_is_written_for_the_types_and_the_engine_of_the_table() {
        let update = RowChange::UpdateCell {
            table: table(),
            column: "amount".into(),
            old: Cell::Null,
            new: Cell::Decimal("12345678901234567890.12".into()),
            key: key(),
        };
        assert_eq!(
            ClickHouseSession::change(&described(), &update).unwrap(),
            "ALTER TABLE `shop`.`orders` UPDATE `amount` = CAST('12345678901234567890.12' AS Nullable(Decimal(38, 10))) \
             WHERE `id` = CAST('7' AS Int64) AND `day` = CAST('2024-05-06' AS Date) AND `amount` IS NULL"
        );
        let insert = RowChange::InsertRow {
            table: table(),
            values: vec![
                ("id".into(), Cell::Integer("8".into())),
                ("label".into(), Cell::Text("it's \\ here".into())),
                ("tags".into(), Cell::Text("['a','b']".into())),
            ],
        };
        assert_eq!(
            ClickHouseSession::change(&described(), &insert).unwrap(),
            "INSERT INTO `shop`.`orders` (`id`, `label`, `tags`) VALUES (CAST('8' AS Int64), 'it\\'s \\\\ here', CAST('[\\'a\\',\\'b\\']' AS Array(String)))"
        );
        // The server has no `DEFAULT VALUES`: an empty row names a column.
        let empty = RowChange::InsertRow {
            table: table(),
            values: Vec::new(),
        };
        assert_eq!(
            ClickHouseSession::change(&described(), &empty).unwrap(),
            "INSERT INTO `shop`.`orders` (`id`) VALUES (DEFAULT)"
        );
        let delete = RowChange::DeleteRow {
            table: table(),
            key: key(),
        };
        assert_eq!(
            ClickHouseSession::change(&described(), &delete).unwrap(),
            "DELETE FROM `shop`.`orders` WHERE `id` = CAST('7' AS Int64) AND `day` = CAST('2024-05-06' AS Date)"
        );
        // An engine without lightweight deletes gets the mutation.
        let memory = Described {
            engine: "Memory".into(),
            ..described()
        };
        assert!(
            ClickHouseSession::change(&memory, &delete)
                .unwrap()
                .starts_with("ALTER TABLE `shop`.`orders` DELETE WHERE ")
        );
        // Without a key there is no record to point at.
        let keyless = RowChange::DeleteRow {
            table: table(),
            key: Vec::new(),
        };
        assert_eq!(
            ClickHouseSession::change(&described(), &keyless)
                .unwrap_err()
                .to_string(),
            "clickhouse_table_without_key"
        );
        let unknown = RowChange::UpdateCell {
            table: table(),
            column: "missing".into(),
            old: Cell::Null,
            new: Cell::Null,
            key: key(),
        };
        assert_eq!(
            ClickHouseSession::change(&described(), &unknown)
                .unwrap_err()
                .to_string(),
            "column_not_found"
        );
    }

    #[test]
    fn a_filter_is_written_without_a_pattern() {
        let filter = |op: FilterOp, value: Cell| Filter {
            column: "label".into(),
            op,
            value,
        };
        let table = described();
        let condition =
            |op, value| ClickHouseSession::condition(Some(&table), &filter(op, value)).unwrap();
        // Nothing of the text is a wildcard or an escape of a pattern.
        assert_eq!(
            condition(FilterOp::Contains, Cell::Text("100%_ \\ 'x'".into())),
            "positionCaseInsensitiveUTF8(toString(`label`), '100%_ \\\\ \\'x\\'') > 0"
        );
        assert_eq!(condition(FilterOp::Equal, Cell::Null), "`label` IS NULL");
        assert_eq!(
            condition(FilterOp::NotEqual, Cell::Null),
            "`label` IS NOT NULL"
        );
        assert_eq!(
            condition(FilterOp::Equal, Cell::Text("a".into())),
            "`label` = 'a'"
        );
        // A column whose type is not known gets a literal that needs none.
        let unknown = Filter {
            column: "n".into(),
            op: FilterOp::Greater,
            value: Cell::Integer("5".into()),
        };
        assert_eq!(
            ClickHouseSession::condition(None, &unknown).unwrap(),
            "`n` > 5"
        );
    }

    #[test]
    fn table_engines_land_where_the_side_panel_expects_them() {
        assert_eq!(container_kind("MergeTree"), "table");
        assert_eq!(container_kind("ReplicatedReplacingMergeTree"), "table");
        assert_eq!(container_kind("Memory"), "table");
        assert_eq!(container_kind("View"), "view");
        assert_eq!(container_kind("MaterializedView"), "materialized view");
        // Neither a table nor a view: listed apart and never edited.
        for engine in ["Dictionary", "Kafka", "SystemProcesses"] {
            assert!(!container_kind(engine).contains("table"), "{engine}");
        }
    }

    #[test]
    fn refusals_of_the_server_become_result_codes() {
        let code = |number| refusal(Some(number)).to_string();
        assert_eq!(code(516), "database_login_rejected");
        assert_eq!(code(394), "query_cancelled");
        assert_eq!(code(159), "query_timeout");
        assert_eq!(code(62), "clickhouse_syntax_error");
        assert_eq!(code(420), "clickhouse_key_column");
        assert_eq!(code(27), "clickhouse_value_rejected");
        // Anything else keeps the number of the server.
        assert_eq!(code(395), "database_error [395]");
        assert_eq!(refusal(None).to_string(), "database_operation_failed");
        // Every code of the driver has a sentence in both languages.
        for name in [
            "clickhouse_native_port",
            "clickhouse_not_http_interface",
            "clickhouse_credentials_invalid",
            "clickhouse_answer_unreadable",
            "clickhouse_syntax_error",
            "clickhouse_unknown_table",
            "clickhouse_unknown_database",
            "clickhouse_unknown_column",
            "clickhouse_unknown_function",
            "clickhouse_access_denied",
            "clickhouse_readonly",
            "clickhouse_memory_limit",
            "clickhouse_too_many_queries",
            "clickhouse_value_rejected",
            "clickhouse_table_without_key",
            "clickhouse_key_not_unique",
            "clickhouse_key_column",
            "clickhouse_engine_no_mutations",
        ] {
            assert!(
                describe(name, false).is_some_and(|text| !text.is_empty()),
                "{name}"
            );
            assert_ne!(describe(name, true), describe(name, false), "{name}");
        }
        assert_eq!(describe("database_error", true), None);
    }

    #[test]
    fn a_figure_without_a_value_leaves_the_statistics() {
        let mut result = QueryResult {
            columns: ["connections", "max_connections", "version"]
                .into_iter()
                .map(|name| ResultColumn {
                    name: name.into(),
                    data_type: String::new(),
                })
                .collect(),
            rows: vec![vec![
                Cell::Integer("3".into()),
                Cell::Null,
                Cell::Text("26.9".into()),
            ]],
            ..QueryResult::default()
        };
        drop_missing(&mut result);
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["connections", "version"]
        );
        assert_eq!(result.rows[0].len(), 2);
    }
}
