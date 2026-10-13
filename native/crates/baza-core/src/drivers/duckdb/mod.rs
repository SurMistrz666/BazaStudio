//! DuckDB: a database in a local file or in memory, run inside the
//! application by the bundled DuckDB library.
//!
//! The library blocks while it works, so every call runs on a blocking
//! thread of the runtime and a running statement is stopped through the
//! interrupt of its connection.
//!
//! A session is one open database and several connections to it. The
//! statements of the editor run on one of them, one after another, so that
//! `SET`, `USE`, `ATTACH` and temporary tables of one run are there for the
//! next; when that connection is busy, a statement gets a connection of its
//! own instead of waiting. Everything else the application asks for (the
//! side panel, a change of one record, an import, the monitor) has its own
//! connection as well.
//!
//! A transaction is not kept between two runs: a script that leaves one
//! open has it rolled back and is told so (`duckdb_transaction_left_open`),
//! because the next statement may run on another connection.
//!
//! MotherDuck (`md:`) is not a product of this family: it is reached through
//! an extension that DuckDB downloads at run time, and this build installs
//! nothing by itself.
#[cfg(test)]
mod tests;
mod values;

use super::{RowChange, Session};
use crate::engine::{
    Badge, Capabilities, ConnectRequest, ConnectionForm, ContainsFilter, DefaultSchema, Dialect,
    EmptyInsert, Engine, EngineSpec, Group, Lexer, OptionField, OptionKind, Paging, ParameterStyle,
    ProductSpec, TextLiteral,
};
use crate::model::{Cell, ColumnInfo, QueryResult, ResultColumn, TableRef, TlsMode};
use crate::sql::{MAX_RESULT_BYTES, MAX_RESULT_ROWS, MAX_SQL_BYTES, literal, qualified};
use anyhow::{Result, anyhow, bail, ensure};
use duckdb::arrow::array::{Array, ArrayRef};
use duckdb::arrow::datatypes::DataType;
use duckdb::{AccessMode, Config, Connection, InterruptHandle, params};
use futures_util::future::BoxFuture;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;

pub const ENGINE: Engine = Engine::new(&SPEC);

pub static SPEC: EngineSpec = EngineSpec {
    id: "duckdb",
    aliases: &["duck"],
    portable_name: "duckdb",
    label: "DuckDB",
    available: true,
    form: ConnectionForm {
        file_extensions: &["duckdb", "ddb", "db"],
        ..ConnectionForm::FILE
    },
    capabilities: Capabilities::SQL,
    dialect: DIALECT,
    options: OPTIONS,
    products: &[ProductSpec {
        id: "duckdb",
        label: "DuckDB",
        group: Group::Analytics,
        badge: Badge {
            text: "Du",
            color: [255, 240, 0],
        },
        default_tls: TlsMode::Disable,
        keywords: &[
            "file",
            "plik",
            "local",
            "lokalna",
            "embedded",
            "olap",
            "analytics",
            "analityka",
            "parquet",
            "csv",
            "json",
            "motherduck",
        ],
        note: Some((
            "Plik bazy DuckDB otwierany wprost przez program; wpisz :memory: zamiast ścieżki, aby pracować w pamięci. Edytor SQL czyta pliki bez importu: SELECT * FROM 'dane.parquet', read_csv('plik.csv'), read_json('plik.json'). Rozszerzeń program nie pobiera sam, więc MotherDuck (md:) nie jest obsługiwany.",
            "A DuckDB database file opened directly by the application; enter :memory: instead of a path to work in memory. The SQL editor reads files without an import: SELECT * FROM 'data.parquet', read_csv('file.csv'), read_json('file.json'). The application downloads no extensions by itself, so MotherDuck (md:) is not supported.",
        )),
        ..ProductSpec::GENERIC
    }],
    connect,
    describe: Some(describe),
};

pub const DIALECT: Dialect = Dialect {
    name_quotes: ('"', '"'),
    text: TextLiteral::Standard,
    bytes: ("from_hex('", "')"),
    booleans: ("TRUE", "FALSE"),
    paging: Paging::LimitOffset,
    // `LIKE` takes text only and tells letter case apart.
    contains: ContainsFilter {
        cast: Some(("CAST(", " AS VARCHAR)")),
        operator: "ILIKE",
        pattern: None,
    },
    parameters: ParameterStyle::Question,
    lexer: Lexer {
        dollar_quotes: true,
        escape_strings: true,
        nested_comments: true,
        ..Lexer::STANDARD
    },
    empty_insert: EmptyInsert::DefaultValues,
    default_schema: DefaultSchema::Fixed("main"),
    column_types: &[
        "INTEGER",
        "BIGINT",
        "VARCHAR",
        "DECIMAL(38,10)",
        "DOUBLE",
        "BOOLEAN",
        "DATE",
        "TIMESTAMP",
        "TIMESTAMPTZ",
        "UUID",
        "JSON",
        "BLOB",
    ],
};

/// The path that stands for a database in memory.
const MEMORY: &str = ":memory:";

static OPTIONS: &[OptionField] = &[
    OptionField {
        key: "read_only",
        label: ("Tylko do odczytu", "Read-only"),
        kind: OptionKind::Flag,
        default: "false",
        placeholder: "",
        help: Some((
            "Otwiera plik bez prawa zapisu. Wiele programów może wtedy czytać ten sam plik jednocześnie.",
            "Opens the file without the right to write. Several programs can then read the same file at once.",
        )),
        required: false,
    },
    OptionField {
        key: "create",
        label: (
            "Utwórz plik, jeśli nie istnieje",
            "Create the file when it does not exist",
        ),
        kind: OptionKind::Flag,
        default: "false",
        placeholder: "",
        help: Some((
            "Bez tej opcji literówka w ścieżce jest błędem, a nie nową, pustą bazą.",
            "Without this option a mistyped path is an error, not a new, empty database.",
        )),
        required: false,
    },
];

/// Sentences for the result codes of this driver.
fn describe(code: &str, en: bool) -> Option<&'static str> {
    let (pl, english) = match code {
        "duckdb_file_not_found" => (
            "Nie ma takiego pliku. Wskaż istniejący plik bazy DuckDB albo zaznacz „Utwórz plik, jeśli nie istnieje”.",
            "There is no such file. Choose an existing DuckDB database file, or tick “Create the file when it does not exist”.",
        ),
        "duckdb_file_locked" => (
            "Ten plik bazy ma teraz otwarty inny program albo inne połączenie. DuckDB pozwala zapisywać tylko jednemu naraz: zamknij tamto połączenie, a jeśli ono tylko czyta, otwórz plik tutaj także tylko do odczytu.",
            "Another program or another connection has this database file open. DuckDB lets only one of them write: close that connection, or, when it only reads, open the file read-only here as well.",
        ),
        "duckdb_file_invalid" => (
            "To nie jest plik bazy DuckDB.",
            "This is not a DuckDB database file.",
        ),
        "duckdb_file_version" => (
            "Ten plik zapisała wersja DuckDB, której formatu ta wersja programu nie odczyta. Wyeksportuj bazę tamtą wersją (EXPORT DATABASE) i zaimportuj ją na nowo.",
            "This file was written by a version of DuckDB whose format this version of the application cannot read. Export the database with that version (EXPORT DATABASE) and import it again.",
        ),
        "duckdb_file_unreadable" => (
            "Nie można otworzyć pliku bazy. Sprawdź, czy ścieżka wskazuje plik i czy masz do niego uprawnienia.",
            "The database file cannot be opened. Check that the path names a file and that you may access it.",
        ),
        "duckdb_open_failed" => (
            "Nie udało się otworzyć bazy DuckDB.",
            "The DuckDB database could not be opened.",
        ),
        "duckdb_read_only" => (
            "Baza jest otwarta tylko do odczytu. Wyłącz opcję „Tylko do odczytu” w ustawieniach połączenia, aby zmieniać dane.",
            "The database is open read-only. Turn off “Read-only” in the settings of the connection to change data.",
        ),
        "duckdb_syntax_error" => (
            "DuckDB nie rozumie tego polecenia: błąd składni. Sprawdź SQL, cudzysłowy i średniki między poleceniami.",
            "DuckDB does not understand this statement: a syntax error. Check the SQL, the quotes and the semicolons between statements.",
        ),
        "duckdb_unknown_object" => (
            "Polecenie używa tabeli, widoku, schematu albo funkcji, których w tej bazie nie ma.",
            "The statement uses a table, a view, a schema or a function this database does not have.",
        ),
        "duckdb_binder_error" => (
            "Polecenie odwołuje się do kolumny, której tu nie ma, albo łączy wartości typów, które do siebie nie pasują.",
            "The statement refers to a column that is not there, or combines values whose types do not fit together.",
        ),
        "duckdb_constraint" => (
            "Zmiana narusza ograniczenie tabeli: powtórzony klucz, brak wymaganej wartości, klucz obcy albo warunek CHECK. Nic nie zostało zapisane.",
            "The change breaks a constraint of the table: a duplicate key, a missing required value, a foreign key or a CHECK condition. Nothing was saved.",
        ),
        "duckdb_value_rejected" => (
            "Wartość nie pasuje do typu kolumny albo wyrażenia. Sprawdź liczbę, datę albo zapis wartości złożonej.",
            "The value does not fit the type of its column or expression. Check the number, the date or the way a composite value is written.",
        ),
        "duckdb_io_error" => (
            "Nie udało się odczytać albo zapisać pliku wskazanego w poleceniu. Przy odczycie ścieżka względna liczy się od folderu bazy; przy zapisie (COPY … TO) podaj pełną ścieżkę.",
            "A file named in the statement could not be read or written. For reading, a relative path starts at the folder of the database; for writing (COPY … TO) give a full path.",
        ),
        "duckdb_transaction_conflict" => (
            "Inne połączenie zmieniło te same dane w tym samym czasie. Odśwież dane i spróbuj ponownie.",
            "Another connection changed the same data at the same time. Reload the data and try again.",
        ),
        "duckdb_transaction_left_open" => (
            "Skrypt zostawił otwartą transakcję, więc została wycofana i nic z niej nie zostało zapisane. BEGIN i COMMIT muszą się znaleźć w jednym uruchomieniu.",
            "The script left a transaction open, so it was rolled back and nothing of it was saved. BEGIN and COMMIT have to be part of one run.",
        ),
        "duckdb_out_of_memory" => (
            "Zapytanie przekroczyło limit pamięci DuckDB. Zawęź je warunkiem WHERE albo LIMIT, albo wybierz mniej kolumn.",
            "The query went over the memory limit of DuckDB. Narrow it with WHERE or LIMIT, or select fewer columns.",
        ),
        "duckdb_extension_missing" => (
            "To polecenie wymaga rozszerzenia DuckDB, którego program nie ma wbudowanego i nie pobiera sam (na przykład icu dla stref czasowych albo httpfs dla adresów sieciowych). Zainstaluj je w edytorze SQL: INSTALL nazwa; LOAD nazwa;",
            "This statement needs a DuckDB extension that is not built into the application and is not downloaded by it (for example icu for time zones, or httpfs for network addresses). Install it in the SQL editor: INSTALL name; LOAD name;",
        ),
        "duckdb_not_implemented" => (
            "DuckDB nie obsługuje tej operacji.",
            "DuckDB does not support this operation.",
        ),
        _ => return None,
    };
    Some(if en { english } else { pl })
}

/// A statement ends after this long, as in every driver.
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(120);
/// Rows of an import that travel in one statement.
const IMPORT_CHUNK_ROWS: usize = 250;
const IMPORT_CHUNK_BYTES: usize = 512 * 1024;

/// What a failure of the library is called. Its text is never passed on:
/// it repeats paths, names and values.
fn failure(error: &duckdb::Error) -> anyhow::Error {
    let text = match error {
        duckdb::Error::DuckDBFailure(_, Some(text)) => text.as_str(),
        _ => return anyhow!("database_error"),
    };
    let kind = text.split(':').next().unwrap_or_default().trim();
    // What the library says itself, without the line of the statement it
    // repeats below: a value of the user must not be read as its words.
    let said = text.split("\n\nLINE ").next().unwrap_or(text);
    // An extension that is not there is named in more than one way:
    // `INSTALL icu;` on a line of its own, or `Install it first using …`.
    let wants_extension = matches!(
        kind,
        "Missing Extension Error" | "Extension Autoloading Error"
    ) || said.contains("Install it first using \"INSTALL ")
        || said
            .lines()
            .any(|line| line.starts_with("INSTALL ") && line.ends_with(';'));
    anyhow!(if wants_extension {
        "duckdb_extension_missing"
    } else if kind != "Conversion Error" && said.contains("in read-only mode") {
        "duckdb_read_only"
    } else {
        match kind {
            "INTERRUPT Error" => "query_cancelled",
            "Parser Error" => "duckdb_syntax_error",
            "Catalog Error" => "duckdb_unknown_object",
            "Binder Error" => "duckdb_binder_error",
            "Constraint Error" => "duckdb_constraint",
            "Conversion Error" | "Out of Range Error" | "Invalid Input Error" => {
                "duckdb_value_rejected"
            }
            "IO Error" | "HTTP Error" => "duckdb_io_error",
            "TransactionContext Error" | "Transaction Error" => "duckdb_transaction_conflict",
            "Out of Memory Error" => "duckdb_out_of_memory",
            "Not implemented Error" | "Not Implemented Error" => "duckdb_not_implemented",
            _ => "database_error",
        }
    })
}

/// Why a database was not opened.
fn open_failure(error: &duckdb::Error) -> anyhow::Error {
    let text = match error {
        duckdb::Error::DuckDBFailure(_, Some(text)) => text.to_ascii_lowercase(),
        _ => return anyhow!("duckdb_open_failed"),
    };
    anyhow!(if text.contains("lock") {
        "duckdb_file_locked"
    } else if text.contains("not a valid duckdb database") {
        "duckdb_file_invalid"
    } else if text.contains("version number") || text.contains("storage version") {
        "duckdb_file_version"
    } else if text.contains("permission denied")
        || text.contains("cannot open file")
        || text.contains("is a directory")
    {
        "duckdb_file_unreadable"
    } else {
        "duckdb_open_failed"
    })
}

/// Open the database of a profile. Blocks.
fn open(path: &Path, read_only: bool, create: bool) -> Result<Connection> {
    let memory = path.as_os_str() == MEMORY;
    let mut config = Config::default()
        // What a statement needs and the build does not have is asked for
        // by the person who runs it, never fetched behind their back.
        .with("autoinstall_known_extensions", "false")
        .and_then(|config| config.custom_user_agent("BazaStudio"))
        .map_err(|_| anyhow!("duckdb_open_failed"))?;
    if memory {
        return Connection::open_in_memory_with_flags(config).map_err(|error| open_failure(&error));
    }
    match std::fs::metadata(path) {
        Ok(found) => ensure!(found.is_file(), "duckdb_file_unreadable"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(create && !read_only, "duckdb_file_not_found");
        }
        Err(_) => bail!("duckdb_file_unreadable"),
    }
    if read_only {
        config = config
            .access_mode(AccessMode::ReadOnly)
            .map_err(|_| anyhow!("duckdb_open_failed"))?;
    }
    // A file a statement reads without naming a folder is looked for next
    // to the database, not in the folder the application was started from.
    // (A file a statement writes has no such rule in DuckDB: `COPY … TO`
    // needs a full path.)
    if let Some(folder) = path
        .parent()
        .filter(|folder| !folder.as_os_str().is_empty())
        .and_then(Path::to_str)
    {
        config = config
            .with("file_search_path", folder)
            .map_err(|_| anyhow!("duckdb_open_failed"))?;
    }
    Connection::open_with_flags(path, config).map_err(|error| open_failure(&error))
}

/// One database this process has open, and the connection that makes
/// further connections to it.
struct Opened {
    /// The file, as the system names it; `None` for a database in memory.
    path: Option<PathBuf>,
    read_only: bool,
    root: Mutex<Connection>,
}

/// The database files this process has open.
///
/// The library locks a file against other processes only. Opened twice in
/// one process, a file would get two instances of the library that each
/// write it as if they were alone. So a second connection to a file joins
/// the database that is open already, and the file is closed when the last
/// one is done with it.
static OPENED: Mutex<Vec<Arc<Opened>>> = Mutex::new(Vec::new());

/// A share in an open database. Every connection lives next to one, so
/// that the file is closed after its last connection and not before.
struct Hold(Option<Arc<Opened>>);
impl Hold {
    /// One more connection to the database.
    fn connection(&self) -> Result<Connection> {
        self.0
            .as_ref()
            .ok_or_else(|| anyhow!("connection_unavailable"))?
            .root
            .lock()
            .map_err(|_| anyhow!("connection_unavailable"))?
            .try_clone()
            .map_err(|error| failure(&error))
    }
}
impl Clone for Hold {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl Drop for Hold {
    fn drop(&mut self) {
        let Some(mine) = self.0.take() else {
            return;
        };
        let Ok(mut opened) = OPENED.lock() else {
            return;
        };
        let listed = opened.iter().position(|entry| Arc::ptr_eq(entry, &mine));
        drop(mine);
        // Nobody but the list holds it any more: close the file, still
        // under the lock, so that it is closed before it can be opened again.
        if let Some(index) = listed
            && Arc::strong_count(&opened[index]) == 1
        {
            drop(opened.swap_remove(index));
        }
    }
}

/// The path of a database file as the system names it, whatever links and
/// relative parts the profile wrote it with.
fn canonical(path: &Path, create: bool) -> Result<PathBuf> {
    match std::fs::canonicalize(path) {
        Ok(found) => Ok(found),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(create, "duckdb_file_not_found");
            let name = path
                .file_name()
                .ok_or_else(|| anyhow!("duckdb_file_unreadable"))?;
            let folder = match path
                .parent()
                .filter(|folder| !folder.as_os_str().is_empty())
            {
                Some(folder) => {
                    std::fs::canonicalize(folder).map_err(|_| anyhow!("duckdb_file_not_found"))?
                }
                None => std::env::current_dir().map_err(|_| anyhow!("duckdb_file_unreadable"))?,
            };
            Ok(folder.join(name))
        }
        Err(_) => bail!("duckdb_file_unreadable"),
    }
}

/// Open the database of a profile, or join it when this process has it
/// open already. Blocks.
fn hold(path: &Path, read_only: bool, create: bool) -> Result<Hold> {
    if path.as_os_str() == MEMORY {
        // Every connection to memory is a database of its own.
        return Ok(Hold(Some(Arc::new(Opened {
            path: None,
            read_only: false,
            root: Mutex::new(open(path, false, false)?),
        }))));
    }
    let mut opened = OPENED.lock().map_err(|_| anyhow!("duckdb_open_failed"))?;
    let path = canonical(path, create && !read_only)?;
    if let Some(existing) = opened
        .iter()
        .find(|entry| entry.path.as_deref() == Some(path.as_path()))
    {
        // One database cannot be read-only for one connection and not for
        // another.
        ensure!(existing.read_only == read_only, "duckdb_file_locked");
        return Ok(Hold(Some(existing.clone())));
    }
    let entry = Arc::new(Opened {
        root: Mutex::new(open(&path, read_only, create)?),
        path: Some(path),
        read_only,
    });
    opened.push(entry.clone());
    Ok(Hold(Some(entry)))
}

fn connect<'a>(request: &'a ConnectRequest<'a>) -> BoxFuture<'a, Result<Arc<dyn Session>>> {
    Box::pin(async move {
        ensure!(request.forwarded.is_none(), "sqlite_ssh_unsupported");
        let path: PathBuf = request
            .profile
            .sqlite_path
            .clone()
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or_else(|| anyhow!("sqlite_path_required"))?;
        let read_only = request.option("read_only") == "true";
        let create = request.option("create") == "true";
        let (hold, primary) = tokio::task::spawn_blocking(move || {
            let hold = hold(&path, read_only, create)?;
            let primary = hold.connection()?;
            Ok::<_, anyhow::Error>((hold.clone(), (primary, hold)))
        })
        .await
        .map_err(|_| anyhow!("duckdb_open_failed"))??;
        Ok(Arc::new(DuckSession {
            primary: Arc::new(tokio::sync::Mutex::new(Some(primary))),
            hold: Mutex::new(Some(hold)),
        }) as Arc<dyn Session>)
    })
}

/// Tells the work on a blocking thread when to give up, and why.
#[derive(Clone)]
struct Watch {
    cancel: CancellationToken,
    deadline: Instant,
}
impl Watch {
    fn new(cancel: CancellationToken) -> Self {
        Self {
            cancel,
            deadline: Instant::now() + STATEMENT_TIMEOUT,
        }
    }
    /// The code to end with, once the statement was cancelled or ran out
    /// of time.
    fn stopped(&self) -> Option<&'static str> {
        if self.cancel.is_cancelled() {
            Some("query_cancelled")
        } else if Instant::now() >= self.deadline {
            Some("query_timeout")
        } else {
            None
        }
    }
    /// What a failure of the library is called while this watch is on: an
    /// interrupt is the cancel or the timeout that asked for it.
    fn failure(&self, error: &duckdb::Error) -> anyhow::Error {
        match self.stopped() {
            Some(code) => anyhow!(code),
            None => failure(error),
        }
    }
}

/// A connection of the editor with its share in the database: the share
/// is given back after the connection is closed, in that order.
type Editor = (Connection, Hold);

/// Which connection a piece of work runs on.
enum Lease {
    /// The connection of the editor, held until the work is done.
    Primary(OwnedMutexGuard<Option<Editor>>),
    /// A new connection to the same database, closed after the work.
    Fresh(Hold),
}

struct DuckSession {
    /// The connection the statements of the editor share; `None` once the
    /// session is closed.
    primary: Arc<tokio::sync::Mutex<Option<Editor>>>,
    /// Makes further connections; `None` once the session is closed.
    hold: Mutex<Option<Hold>>,
}

impl DuckSession {
    fn fresh(&self) -> Result<Lease> {
        self.hold
            .lock()
            .ok()
            .and_then(|hold| hold.clone())
            .map(Lease::Fresh)
            .ok_or_else(|| anyhow!("connection_unavailable"))
    }

    /// The connection of the editor when it is free, otherwise a new one.
    fn editor(&self) -> Result<Lease> {
        match self.primary.clone().try_lock_owned() {
            Ok(guard) if guard.is_some() => Ok(Lease::Primary(guard)),
            _ => self.fresh(),
        }
    }

    /// Run `work` on a blocking thread with a connection, and interrupt the
    /// connection when the watch says the statement has to stop.
    async fn blocking<T: Send + 'static>(
        &self,
        lease: Result<Lease>,
        watch: Watch,
        work: impl FnOnce(&Connection, &Watch) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let lease = lease?;
        if let Some(code) = watch.stopped() {
            bail!(code);
        }
        let inner = watch.clone();
        let (ready, handle) = tokio::sync::oneshot::channel::<Arc<InterruptHandle>>();
        let mut task = tokio::task::spawn_blocking(move || match &lease {
            Lease::Primary(guard) => {
                let (connection, _) = guard
                    .as_ref()
                    .ok_or_else(|| anyhow!("connection_unavailable"))?;
                let _ = ready.send(connection.interrupt_handle());
                work(connection, &inner)
            }
            Lease::Fresh(hold) => {
                // Closed at the end of this arm, before the lease gives
                // its share in the database back.
                let connection = hold.connection()?;
                let _ = ready.send(connection.interrupt_handle());
                work(&connection, &inner)
            }
        });
        let left = watch.deadline.saturating_duration_since(Instant::now());
        let code = tokio::select! {
            biased;
            _ = watch.cancel.cancelled() => "query_cancelled",
            _ = tokio::time::sleep(left) => "query_timeout",
            result = &mut task => {
                return result.map_err(|_| anyhow!("database_operation_failed"))?;
            }
        };
        // The library clears an interrupt when the next statement of the
        // work starts, so it is repeated until the work has ended.
        let handle = handle.await.ok();
        loop {
            if let Some(handle) = &handle {
                handle.interrupt();
            }
            if tokio::time::timeout(Duration::from_millis(25), &mut task)
                .await
                .is_ok()
            {
                bail!(code);
            }
        }
    }

    /// Run a statement of the driver itself on a connection of its own.
    async fn ask(&self, statement: String) -> Result<QueryResult> {
        let watch = Watch::new(CancellationToken::new());
        self.blocking(self.fresh(), watch, move |connection, watch| {
            let mut result = QueryResult::default();
            run_one(connection, &statement, watch, &mut result)?;
            Ok(result)
        })
        .await
    }
}

/// The first word of a statement, in capitals: what kind of statement it is.
fn first_word(statement: &str) -> String {
    let mut rest = statement.trim_start();
    loop {
        if let Some(comment) = rest.strip_prefix("--") {
            rest = comment.split_once('\n').map_or("", |(_, after)| after);
        } else if let Some(comment) = rest.strip_prefix("/*") {
            rest = comment.split_once("*/").map_or("", |(_, after)| after);
        } else {
            break;
        }
        rest = rest.trim_start();
    }
    rest.trim_start_matches('(')
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_uppercase()
}

/// A statement whose answer is a result set, even an empty one.
fn is_query(word: &str) -> bool {
    matches!(
        word,
        "SELECT"
            | "WITH"
            | "FROM"
            | "VALUES"
            | "TABLE"
            | "SHOW"
            | "DESCRIBE"
            | "DESC"
            | "EXPLAIN"
            | "SUMMARIZE"
            | "PIVOT"
            | "UNPIVOT"
            | "CALL"
            | "PRAGMA"
            | "EXECUTE"
    )
}

/// Run one statement and put its result set, or the number of rows it
/// changed, into `result`.
fn run_one(
    connection: &Connection,
    statement: &str,
    watch: &Watch,
    result: &mut QueryResult,
) -> Result<()> {
    let word = first_word(statement);
    let mut prepared = connection
        .prepare(statement)
        .map_err(|error| watch.failure(&error))?;
    // Streaming: rows are produced while they are read, so that a result
    // that is cut does not cost the rest of its rows.
    drop(
        prepared
            .stream_arrow(params![])
            .map_err(|error| watch.failure(&error))?,
    );
    let schema = prepared.schema();
    let described: Vec<(String, values::Hint)> = (0..schema.fields().len())
        .map(|index| values::describe(&prepared.column_logical_type(index)))
        .collect();
    let mut rows = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;
    'chunks: while let Some(chunk) = prepared.step().map_err(|error| watch.failure(&error))? {
        if let Some(code) = watch.stopped() {
            bail!(code);
        }
        // The names of an ENUM are looked up once for the chunk.
        let columns: Vec<ArrayRef> = chunk
            .columns()
            .iter()
            .map(|column| match column.data_type() {
                DataType::Dictionary(_, value) => {
                    duckdb::arrow::compute::cast(column, value).unwrap_or_else(|_| column.clone())
                }
                _ => column.clone(),
            })
            .collect();
        for row in 0..chunk.len() {
            let cells: Vec<Cell> = columns
                .iter()
                .zip(&described)
                .map(|(column, (name, hint))| values::cell(column.as_ref(), row, name, *hint))
                .collect();
            let size: usize = cells
                .iter()
                .map(|cell| match cell {
                    Cell::Bytes(value) => value.len(),
                    other => other.display().len(),
                })
                .sum();
            if rows.len() >= MAX_RESULT_ROWS || bytes.saturating_add(size) > MAX_RESULT_BYTES {
                truncated = true;
                break 'chunks;
            }
            bytes += size;
            rows.push(cells);
        }
    }
    // A statement that changes rows answers with their number in a column
    // called `Count`; one that changes none answers with nothing to show.
    if !is_query(&word) {
        let counted = schema.fields().len() == 1
            && schema.field(0).name() == "Count"
            && rows.len() == 1
            && matches!(rows[0][0], Cell::Integer(_));
        if counted {
            let count = rows[0][0].display().parse::<u64>().unwrap_or(0);
            result.affected = result.affected.saturating_add(count);
            return Ok(());
        }
        if rows.is_empty() {
            return Ok(());
        }
    }
    result.columns = schema
        .fields()
        .iter()
        .zip(described)
        .map(|(field, (data_type, _))| ResultColumn {
            name: field.name().clone(),
            data_type,
        })
        .collect();
    result.rows = rows;
    result.truncated = truncated;
    Ok(())
}

/// Run the statements of a script in order on one connection.
fn run_script(
    connection: &Connection,
    statements: &[String],
    watch: &Watch,
) -> Result<QueryResult> {
    let started = Instant::now();
    let mut result = QueryResult::default();
    let mut began = false;
    let mut outcome = Ok(());
    for statement in statements {
        if let Some(code) = watch.stopped() {
            outcome = Err(anyhow!(code));
            break;
        }
        began |= matches!(first_word(statement).as_str(), "BEGIN" | "START");
        outcome = run_one(connection, statement, watch, &mut result);
        if outcome.is_err() {
            break;
        }
    }
    if began {
        // Nothing of a transaction outlives the run: the next statement
        // may get another connection. A rollback that succeeds means the
        // script left one open.
        let open = connection.execute_batch("ROLLBACK").is_ok();
        if open && outcome.is_ok() {
            bail!("duckdb_transaction_left_open");
        }
    }
    outcome?;
    result.duration_ms = started.elapsed().as_millis() as u64;
    Ok(result)
}

/// A cell as the statement of a change writes it. The interface guesses
/// the kind of a typed value from the name of the type (`INTERVAL` and
/// `INTEGER[]` hold "INT"), so a number that is not one is written as
/// text; so is every number with a fraction, which DuckDB then casts to
/// the type of the column without a detour through a floating point value.
fn fit(cell: &Cell) -> Cell {
    match cell {
        Cell::Integer(text) if text.parse::<i128>().is_err() => Cell::Text(text.clone()),
        Cell::Decimal(text) | Cell::Float(text) => Cell::Text(text.clone()),
        other => other.clone(),
    }
}

fn fit_all(values: &[(String, Cell)]) -> Vec<(String, Cell)> {
    values
        .iter()
        .map(|(name, value)| (name.clone(), fit(value)))
        .collect()
}

impl Session for DuckSession {
    fn dialect(&self) -> &Dialect {
        &DIALECT
    }

    fn execute<'a>(
        &'a self,
        text: &'a str,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            let statements = crate::sql::statements(&DIALECT, text)?;
            ensure!(!statements.is_empty(), "sql_invalid");
            self.blocking(
                self.editor(),
                Watch::new(cancel),
                move |connection, watch| run_script(connection, &statements, watch),
            )
            .await
        })
    }

    fn execute_one<'a>(&'a self, statement: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let statement = statement.to_owned();
            let watch = Watch::new(CancellationToken::new());
            self.blocking(self.fresh(), watch, move |connection, watch| {
                let run = |text: &str| {
                    connection
                        .execute_batch(text)
                        .map_err(|error| watch.failure(&error))
                };
                run("BEGIN TRANSACTION")?;
                match connection.execute(&statement, params![]) {
                    Ok(1) => run("COMMIT"),
                    Ok(_) => {
                        let _ = run("ROLLBACK");
                        bail!("row_changed_reload_required")
                    }
                    Err(error) => {
                        let _ = run("ROLLBACK");
                        Err(watch.failure(&error))
                    }
                }
            })
            .await
        })
    }

    fn change_text(&self, change: &RowChange) -> Result<String> {
        let fitted = match change {
            RowChange::UpdateCell {
                table,
                column,
                old,
                new,
                key,
            } => RowChange::UpdateCell {
                table: table.clone(),
                column: column.clone(),
                old: fit(old),
                new: fit(new),
                key: fit_all(key),
            },
            RowChange::InsertRow { table, values } => RowChange::InsertRow {
                table: table.clone(),
                values: fit_all(values),
            },
            RowChange::DeleteRow { table, key } => RowChange::DeleteRow {
                table: table.clone(),
                key: fit_all(key),
            },
        };
        crate::sql::change_statement(&DIALECT, &fitted)
    }

    fn containers<'a>(&'a self) -> BoxFuture<'a, Result<Vec<TableRef>>> {
        Box::pin(async move {
            let result = self
                .ask(
                    "SELECT schema_name, table_name, 'table' FROM duckdb_tables() \
                     WHERE database_name = current_database() AND NOT internal AND NOT temporary \
                     UNION ALL \
                     SELECT schema_name, view_name, 'view' FROM duckdb_views() \
                     WHERE database_name = current_database() AND NOT internal AND NOT temporary \
                     ORDER BY 1, 2"
                        .to_owned(),
                )
                .await?;
            Ok(result
                .rows
                .iter()
                .filter(|row| row.len() >= 3)
                .map(|row| TableRef {
                    schema: row[0].display(),
                    name: row[1].display(),
                    kind: row[2].display(),
                })
                .collect())
        })
    }

    fn columns<'a>(&'a self, container: &'a TableRef) -> BoxFuture<'a, Result<Vec<ColumnInfo>>> {
        Box::pin(async move {
            let schema = literal(
                &DIALECT,
                &Cell::Text(if container.schema.is_empty() {
                    "main".into()
                } else {
                    container.schema.clone()
                }),
            )?;
            let table = literal(&DIALECT, &Cell::Text(container.name.clone()))?;
            let result = self
                .ask(format!(
                    "SELECT c.column_name, c.data_type, c.is_nullable, c.column_default, \
                     coalesce(list_contains(k.names, c.column_name), false) \
                     FROM duckdb_columns() c LEFT JOIN (\
                     SELECT constraint_column_names AS names FROM duckdb_constraints() \
                     WHERE database_name = current_database() AND schema_name = {schema} \
                     AND table_name = {table} AND constraint_type = 'PRIMARY KEY' LIMIT 1) k ON true \
                     WHERE c.database_name = current_database() AND c.schema_name = {schema} \
                     AND c.table_name = {table} ORDER BY c.column_index"
                ))
                .await?;
            Ok(result
                .rows
                .into_iter()
                .filter(|row| row.len() >= 5)
                .map(|row| {
                    let key = row[4] == Cell::Bool(true);
                    ColumnInfo {
                        name: row[0].display(),
                        data_type: row[1].display(),
                        nullable: row[2] == Cell::Bool(true) && !key,
                        primary_key: key,
                        default: match &row[3] {
                            Cell::Null => None,
                            other => Some(other.display()),
                        },
                    }
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
            let started = Instant::now();
            let names = columns
                .iter()
                .map(|column| crate::sql::quote_identifier(&DIALECT, column))
                .collect::<Result<Vec<_>>>()?
                .join(", ");
            let head = format!(
                "INSERT INTO {} ({names}) VALUES ",
                qualified(&DIALECT, container)?
            );
            // The statements are written before anything runs: a value
            // that cannot be written fails the import before its first row.
            let mut statements = Vec::new();
            let mut current = String::new();
            let mut count = 0usize;
            for row in rows {
                let tuple = format!(
                    "({})",
                    row.iter()
                        .map(|cell| literal(&DIALECT, &fit(cell)))
                        .collect::<Result<Vec<_>>>()?
                        .join(", ")
                );
                ensure!(tuple.len() <= MAX_SQL_BYTES, "import_row_too_large");
                if count == IMPORT_CHUNK_ROWS || current.len() + tuple.len() > IMPORT_CHUNK_BYTES {
                    statements.push(std::mem::take(&mut current));
                    count = 0;
                }
                current.push_str(if count == 0 { &head } else { ", " });
                current.push_str(&tuple);
                count += 1;
            }
            if count > 0 {
                statements.push(current);
            }
            let affected = self
                .blocking(
                    self.fresh(),
                    Watch::new(cancel),
                    move |connection, watch| {
                        let run = |text: &str| {
                            connection
                                .execute_batch(text)
                                .map_err(|error| watch.failure(&error))
                        };
                        run("BEGIN TRANSACTION")?;
                        let mut affected = 0u64;
                        for statement in &statements {
                            let outcome = match watch.stopped() {
                                Some(code) => Err(anyhow!(code)),
                                None => connection
                                    .execute(statement, params![])
                                    .map_err(|error| watch.failure(&error)),
                            };
                            match outcome {
                                Ok(changed) => affected += changed as u64,
                                Err(error) => {
                                    // All rows or none.
                                    let _ = connection.execute_batch("ROLLBACK");
                                    return Err(error);
                                }
                            }
                        }
                        run("COMMIT")?;
                        Ok(affected)
                    },
                )
                .await?;
            Ok(QueryResult {
                affected,
                duration_ms: started.elapsed().as_millis() as u64,
                ..QueryResult::default()
            })
        })
    }

    fn server_stats<'a>(&'a self) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            let started = Instant::now();
            // Tables first, then the size of the database: the two large
            // figures. A database in memory has no file to measure.
            let mut result = self
                .ask(
                    "SELECT (SELECT count(*) FROM duckdb_tables() WHERE database_name = current_database() AND NOT internal) AS tables, \
                     s.database_size AS size, \
                     (SELECT library_version || ' ' || codename FROM pragma_version()) AS version, \
                     current_database() AS database, \
                     (SELECT count(*) FROM duckdb_views() WHERE database_name = current_database() AND NOT internal) AS views, \
                     s.memory_usage AS memory, s.memory_limit AS \"memory limit\", s.wal_size AS wal, \
                     current_setting('threads') AS threads \
                     FROM pragma_database_size() s WHERE s.database_name = current_database()"
                        .to_owned(),
                )
                .await?;
            result.duration_ms = started.elapsed().as_millis() as u64;
            Ok(result)
        })
    }

    fn close<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // The file is written and released when its last connection
            // goes. A statement that is still running keeps its connection
            // until it ends; nothing waits for it here.
            let primary = self
                .primary
                .try_lock()
                .ok()
                .and_then(|mut primary| primary.take());
            let hold = self.hold.lock().ok().and_then(|mut hold| hold.take());
            let _ = tokio::task::spawn_blocking(move || {
                drop(primary);
                drop(hold);
            })
            .await;
        })
    }
}
