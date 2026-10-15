//! Oracle Database, over Oracle Net, through the thin client that Oracle
//! maintains (`oracledb`): no Oracle client libraries on the machine.
//!
//! The products are presets over this one driver. Sign-in is by user name
//! and password; a wallet, Kerberos, an IAM token and Oracle's own native
//! network encryption are not implemented, and the product notes say so.
//! The oldest server is Oracle Database 12.1: the client speaks nothing
//! older.
//!
//! What the driver decides, because Oracle leaves it open:
//!
//! * A run is one transaction. Its statements see each other, it is
//!   committed when the last one succeeded and rolled back when one failed;
//!   `COMMIT`, `ROLLBACK` and `SAVEPOINT` of a script do what they say, and
//!   DDL commits by itself as it always does in Oracle. Nothing stays open
//!   between two runs, which may use different connections.
//! * The session reads and writes dates and timestamps in one fixed form
//!   (`2024-02-29 13:14:15.5 +01:00`) and numbers with a point, so that a
//!   value shown in the grid is a value Oracle converts back, and its time
//!   zone is the one of the database.
//! * An empty text is `NULL`, as Oracle stores it: it is read as `NULL`,
//!   written as `NULL` and compared with `IS NULL`.
//! * A page is ordered by the primary key, or by `ROWID` when there is no
//!   key, so that `OFFSET … FETCH` pages are pages of one list.
//!
//! The network belongs to the driver ([`net`]): the client library talks to
//! a relay on the loopback interface, which dials, encrypts, follows a
//! redirect of the listener and stops a statement. A statement that computes
//! is stopped at once. One that only waits (`DBMS_SESSION.SLEEP`, the row
//! lock of another session) takes the break when its wait is over: the
//! person is free at once, the server a little later, and nothing the
//! statement changed is kept.
//!
//! The client library is a beta. It gives up, instead of failing, on a few
//! things it cannot read yet; that is caught, reported as
//! `oracle_client_unsupported`, and the connection is replaced. The pages
//! of a table avoid the known cases by asking the server for text.
mod connection;
#[cfg(test)]
mod live;
mod net;
mod script;
mod values;

use super::{PageRequest, RowChange, Session};
use crate::engine::{
    Badge, Capabilities, ConnectRequest, ConnectionForm, ContainsFilter, DefaultSchema, Dialect,
    EmptyInsert, Engine, EngineSpec, FieldUse, Group, Lexer, OptionField, OptionKind, Paging,
    ParameterStyle, ProductSpec, TextLiteral,
};
use crate::model::{Cell, ColumnInfo, QueryResult, ResultColumn, TableRef, TlsMode};
use crate::sql::{MAX_RESULT_BYTES, MAX_RESULT_ROWS, MAX_SQL_BYTES, qualified, quote_identifier};
use anyhow::{Result, anyhow};
use connection::{Conn, Connector, Failure, Pool, statement_failure};
use futures_util::future::BoxFuture;
use oracledb::Connection;
use script::{Columns, Kind, Order};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use values::Column;

pub const ENGINE: Engine = Engine::new(&SPEC);

pub static SPEC: EngineSpec = EngineSpec {
    id: "oracle",
    aliases: &["oracledb", "oci"],
    portable_name: "oracle",
    label: "Oracle Database",
    available: true,
    form: ConnectionForm {
        // The service of the listener is what other engines call the
        // database; a server without one is named by its SID instead.
        database: FieldUse::Optional,
        database_label: ("Nazwa usługi", "Service name"),
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

/// What the table designer offers on every server the client reaches.
const TYPES: &[&str] = &[
    "NUMBER(19)",
    "NUMBER",
    "DECIMAL(38,10)",
    "VARCHAR2(255 CHAR)",
    "VARCHAR2(4000)",
    "CLOB",
    "BINARY_DOUBLE",
    "DATE",
    "TIMESTAMP",
    "TIMESTAMP WITH TIME ZONE",
    "RAW(16)",
    "BLOB",
];
/// The same with the JSON type of Oracle Database 21c.
const TYPES_21: &[&str] = &[
    "NUMBER(19)",
    "NUMBER",
    "DECIMAL(38,10)",
    "VARCHAR2(255 CHAR)",
    "VARCHAR2(4000)",
    "CLOB",
    "BINARY_DOUBLE",
    "DATE",
    "TIMESTAMP",
    "TIMESTAMP WITH TIME ZONE",
    "RAW(16)",
    "BLOB",
    "JSON",
];
/// And with the boolean of 23ai.
const TYPES_23: &[&str] = &[
    "NUMBER(19)",
    "NUMBER",
    "DECIMAL(38,10)",
    "VARCHAR2(255 CHAR)",
    "VARCHAR2(4000)",
    "CLOB",
    "BINARY_DOUBLE",
    "DATE",
    "TIMESTAMP",
    "TIMESTAMP WITH TIME ZONE",
    "RAW(16)",
    "BLOB",
    "JSON",
    "BOOLEAN",
];

/// The dialect of every server: what the session refines after connecting
/// is the list of column types and the boolean literals.
pub const DIALECT: Dialect = Dialect {
    name_quotes: ('"', '"'),
    text: TextLiteral::Standard,
    bytes: ("HEXTORAW('", "')"),
    // No boolean before 23ai: a flag is a number there.
    booleans: ("1", "0"),
    // Oracle accepts `OFFSET` without an order; the driver names one for
    // the pages it reads itself.
    paging: Paging::OffsetFetch {
        default_order: None,
    },
    // `LIKE` compares bytes, so both sides are folded; a number and a date
    // become the text of the session.
    contains: ContainsFilter {
        cast: Some(("LOWER(", ")")),
        operator: "LIKE",
        pattern: Some(("LOWER(", ")")),
    },
    parameters: ParameterStyle::Colon,
    lexer: Lexer::STANDARD,
    // Oracle has neither form; the driver writes `(column) VALUES (DEFAULT)`.
    empty_insert: EmptyInsert::DefaultValues,
    // A table without a schema lands in the schema of the account.
    default_schema: DefaultSchema::None,
    column_types: TYPES,
};

static OPTIONS: &[OptionField] = &[
    OptionField {
        key: "sid",
        label: ("SID", "SID"),
        kind: OptionKind::Text,
        default: "",
        placeholder: "ORCL",
        help: Some((
            "Identyfikator instancji, dla serwera wskazywanego przez SID zamiast nazwy usługi. Wypełnij jedno z tych dwóch pól.",
            "The instance identifier, for a server that is named by its SID instead of a service name. Fill in one of the two fields.",
        )),
        required: false,
    },
    OptionField {
        key: "role",
        label: ("Rola", "Role"),
        kind: OptionKind::Choice(&["default", "sysdba", "sysoper"]),
        default: "default",
        placeholder: "",
        help: Some((
            "Logowanie administracyjne: konto SYS wymaga roli SYSDBA albo SYSOPER.",
            "Administrative sign-in: the SYS account needs the role SYSDBA or SYSOPER.",
        )),
        required: false,
    },
    OptionField {
        key: "system_schemas",
        label: ("Schematy systemowe", "System schemas"),
        kind: OptionKind::Flag,
        default: "false",
        placeholder: "",
        help: Some((
            "Pokazuje także schematy utrzymywane przez Oracle (SYS, SYSTEM i inne).",
            "Also lists the schemas Oracle maintains itself (SYS, SYSTEM and others).",
        )),
        required: false,
    },
];

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
        default_port: 1521,
        // A listener speaks plain Oracle Net unless it was set up for TLS.
        default_tls: TlsMode::Disable,
        ..ProductSpec::GENERIC
    }
}

static PRODUCTS: &[ProductSpec] = &[
    ProductSpec {
        keywords: &["oracle", "oracledb", "plsql", "pl/sql", "tns", "exadata"],
        note: Some((
            "Logowanie nazwą użytkownika i hasłem, do serwera w wersji 12.1 lub nowszej, bez bibliotek klienta Oracle. Port 1521 to zwykle połączenie nieszyfrowane; nasłuch TCPS (zwykle 2484) wymaga trybu innego niż „Wyłączony” na karcie SSL/TLS. Natywne szyfrowanie sieci Oracle (SQLNET.ENCRYPTION) i portfele nie są obsługiwane.",
            "Sign-in with a user name and a password, to a server of version 12.1 or later, without Oracle client libraries. Port 1521 is usually an unencrypted connection; a TCPS listener (usually 2484) needs a mode other than “Disabled” on the SSL/TLS tab. Oracle native network encryption (SQLNET.ENCRYPTION) and wallets are not supported.",
        )),
        ..product(
            "oracle-database",
            "Oracle Database",
            Group::Relational,
            "Or",
            [199, 70, 52],
        )
    },
    ProductSpec {
        default_database: "FREEPDB1",
        keywords: &[
            "oracle", "oracledb", "xe", "express", "free", "docker", "plsql",
        ],
        note: Some((
            "Logowanie nazwą użytkownika i hasłem. Usługa bazy podłączanej to FREEPDB1 (Oracle Database Free) albo XEPDB1 (XE 21c i 18c); XE 11g jest zbyt stare dla tego klienta. Konto SYS wymaga roli SYSDBA.",
            "Sign-in with a user name and a password. The service of the pluggable database is FREEPDB1 (Oracle Database Free) or XEPDB1 (XE 21c and 18c); XE 11g is too old for this client. The SYS account needs the role SYSDBA.",
        )),
        ..product(
            "oracle-database-free",
            "Oracle Database Free / XE",
            Group::Relational,
            "Ox",
            [199, 70, 52],
        )
    },
    ProductSpec {
        default_tls: TlsMode::VerifyFull,
        default_username: "ADMIN",
        keywords: &[
            "oracle",
            "oracledb",
            "oci",
            "adb",
            "atp",
            "adw",
            "autonomous",
            "cloud",
        ],
        note: Some((
            "Logowanie nazwą użytkownika i hasłem przez TLS bez portfela: w konsoli OCI wyłącz „Require mutual TLS (mTLS)” i dopuść swój adres IP na liście dostępu. Host i nazwę usługi (kończy się na adb.oraclecloud.com) podaje ciąg połączenia TLS, port to 1521. Połączenie z portfelem (mTLS, port 1522) nie jest obsługiwane: nie ma gdzie bezpiecznie zapisać hasła portfela.",
            "Sign-in with a user name and a password over TLS without a wallet: in the OCI console turn “Require mutual TLS (mTLS)” off and allow your IP address in the access control list. The TLS connection string names the host and the service name (it ends with adb.oraclecloud.com); the port is 1521. A connection with a wallet (mTLS, port 1522) is not supported: there is no safe place for the password of the wallet.",
        )),
        ..product(
            "oracle-autonomous-database",
            "Oracle Autonomous Database",
            Group::Cloud,
            "Ad",
            [199, 70, 52],
        )
    },
    ProductSpec {
        default_database: "ORCL",
        default_username: "admin",
        keywords: &["aws", "amazon", "rds", "oracle", "oracledb"],
        note: Some((
            "Logowanie nazwą użytkownika i hasłem, na przykład głównym użytkownikiem instancji; nazwa usługi to nazwa bazy podana przy jej tworzeniu (domyślnie ORCL). Port 1521 nie jest szyfrowany; opcja SSL nasłuchuje na porcie 2484, a pełna weryfikacja wymaga pliku CA pobranego od AWS, wskazanego na karcie SSL/TLS. Opcja NATIVE_NETWORK_ENCRYPTION ustawiona na REQUIRED nie jest obsługiwana.",
            "Sign-in with a user name and a password, for example the master user of the instance; the service name is the database name given when it was created (ORCL by default). Port 1521 is not encrypted; the SSL option listens on port 2484, and full verification needs the CA file downloaded from AWS, chosen on the SSL/TLS tab. The option NATIVE_NETWORK_ENCRYPTION set to REQUIRED is not supported.",
        )),
        ..product(
            "amazon-rds-oracle",
            "Amazon RDS for Oracle",
            Group::Cloud,
            "Ro",
            [82, 127, 255],
        )
    },
];

/// Sentences for the result codes of this driver.
fn describe(code: &str, en: bool) -> Option<&'static str> {
    let (pl, english) = match code {
        "oracle_login_required" => (
            "Podaj nazwę użytkownika bazy Oracle. Logowanie bez hasła (portfel, Kerberos, system operacyjny) nie jest obsługiwane.",
            "Enter the user name of an Oracle account. Sign-in without a password (wallet, Kerberos, operating system) is not supported.",
        ),
        "oracle_service_required" => (
            "Podaj nazwę usługi bazy Oracle, na przykład FREEPDB1, albo SID instancji na karcie Ogólne.",
            "Enter the service name of the Oracle database, for example FREEPDB1, or the SID of the instance on the General tab.",
        ),
        "oracle_service_and_sid" => (
            "Podano i nazwę usługi, i SID. Zostaw jedno z tych dwóch pól: nazwę usługi, a SID tylko dla serwera, który jej nie podaje.",
            "Both a service name and a SID were given. Keep one of the two fields: the service name, or the SID only for a server that has no service name.",
        ),
        "oracle_service_invalid" => (
            "Nazwa usługi albo SID zawiera niedozwolone znaki. Dozwolone są litery, cyfry, kropka, podkreślenie, myślnik, $ i #.",
            "The service name or the SID contains characters that are not allowed. Letters, digits, the dot, the underscore, the hyphen, $ and # are.",
        ),
        "oracle_sid_unknown" => (
            "Nasłuch Oracle nie zna instancji o tym SID. Sprawdź SID albo podaj nazwę usługi zamiast niego.",
            "The Oracle listener does not know an instance with this SID. Check the SID, or give a service name instead.",
        ),
        "oracle_listener_busy" => (
            "Nasłuch Oracle nie ma teraz wolnego procesu obsługi dla tej usługi (limit procesów albo sesji serwera). Spróbuj ponownie za chwilę.",
            "The Oracle listener has no free handler for this service right now (the process or session limit of the server). Try again in a moment.",
        ),
        "oracle_instance_restricted" => (
            "Instancja Oracle przyjmuje teraz tylko połączenia administracyjne (tryb ograniczony albo blokada nowych połączeń).",
            "The Oracle instance accepts only administrative connections right now (restricted mode, or new connections are blocked).",
        ),
        "oracle_listener_refused" => (
            "Nasłuch Oracle odrzucił połączenie. Sprawdź nazwę usługi, port oraz to, czy baza jest uruchomiona i zarejestrowana w nasłuchu.",
            "The Oracle listener refused the connection. Check the service name, the port, and that the database is running and registered with the listener.",
        ),
        "oracle_instance_unavailable" => (
            "Baza Oracle jest w trakcie uruchamiania albo zamykania i nie przyjmuje jeszcze połączeń. Spróbuj ponownie za chwilę.",
            "The Oracle database is starting up or shutting down and does not accept connections yet. Try again in a moment.",
        ),
        "oracle_account_locked" => (
            "To konto Oracle jest zablokowane. Odblokowuje je administrator bazy poleceniem ALTER USER … ACCOUNT UNLOCK.",
            "This Oracle account is locked. An administrator of the database unlocks it with ALTER USER … ACCOUNT UNLOCK.",
        ),
        "oracle_password_expired" => (
            "Hasło tego konta Oracle wygasło. Zmień je w narzędziu administracyjnym bazy (ALTER USER … IDENTIFIED BY …) i wpisz nowe na karcie Ogólne.",
            "The password of this Oracle account has expired. Change it with an administration tool of the database (ALTER USER … IDENTIFIED BY …) and enter the new one on the General tab.",
        ),
        "oracle_sys_needs_role" => (
            "Konto SYS loguje się tylko z rolą administracyjną. Wybierz rolę SYSDBA albo SYSOPER na karcie Ogólne.",
            "The SYS account signs in only with an administrative role. Choose the role SYSDBA or SYSOPER on the General tab.",
        ),
        "oracle_server_too_old" => (
            "Ten serwer Oracle jest starszy niż wersja 12.1, a klient wbudowany w Baza Studio nie zna starszego protokołu.",
            "This Oracle server is older than version 12.1, and the client built into Baza Studio does not speak an older protocol.",
        ),
        "oracle_native_encryption_required" => (
            "Serwer wymaga natywnego szyfrowania sieci Oracle (SQLNET.ENCRYPTION_SERVER = REQUIRED), którego ten klient nie obsługuje. Połącz się przez TCPS albo tunel SSH, albo poproś administratora o ustawienie REQUESTED lub ACCEPTED.",
            "The server requires Oracle native network encryption (SQLNET.ENCRYPTION_SERVER = REQUIRED), which this client does not support. Connect over TCPS or an SSH tunnel, or ask the administrator for the setting REQUESTED or ACCEPTED.",
        ),
        "oracle_redirect_unsupported" => (
            "Nasłuch przekierował połączenie w sposób, za którym nie da się podążyć: nie podał adresu albo przekierowywał zbyt wiele razy. Połącz się bezpośrednio z nasłuchem węzła bazy.",
            "The listener redirected the connection in a way that cannot be followed: it named no address, or it redirected too many times. Connect directly to the listener of a database node.",
        ),
        "oracle_redirect_through_tunnel" => (
            "Nasłuch przekierował połączenie pod inny adres, do którego tunel SSH nie prowadzi. Wskaż w połączeniu bezpośrednio nasłuch węzła bazy zamiast nasłuchu SCAN.",
            "The listener redirected the connection to another address, which the SSH tunnel does not lead to. Point the connection directly at the listener of a database node instead of the SCAN listener.",
        ),
        "oracle_ca_file_unreadable" => (
            "Nie można odczytać pliku CA albo nie zawiera on żadnego certyfikatu w formacie PEM. Wskaż właściwy plik na karcie SSL/TLS.",
            "The CA file cannot be read, or it holds no certificate in PEM format. Choose the right file on the SSL/TLS tab.",
        ),
        "oracle_client_unsupported" => (
            "Klient Oracle wbudowany w Baza Studio nie potrafi jeszcze odczytać tej odpowiedzi serwera: najczęściej to wartość TIMESTAMP WITH TIME ZONE z nazwą regionu (na przykład Europe/Warsaw) albo kolumna z adnotacjami. Zamień taką kolumnę w zapytaniu na tekst, na przykład TO_CHAR(kolumna), albo otwórz tabelę z panelu bocznego.",
            "The Oracle client built into Baza Studio cannot read this answer of the server yet: most often a TIMESTAMP WITH TIME ZONE value with a region name (for example Europe/Warsaw), or a column with annotations. Turn such a column into text in the query, for example TO_CHAR(column), or open the table from the side panel.",
        ),
        "oracle_type_unsupported" => (
            "Wynik zawiera kolumnę typu, którego ten klient nie odczytuje (typ obiektowy, XMLTYPE, kolekcja, BFILE). Zamień ją w zapytaniu na tekst, na przykład XMLSERIALIZE(CONTENT kolumna AS CLOB) albo JSON_OBJECT(kolumna), albo otwórz tabelę z panelu bocznego.",
            "The result has a column of a type this client does not read (an object type, XMLTYPE, a collection, BFILE). Turn it into text in the query, for example XMLSERIALIZE(CONTENT column AS CLOB) or JSON_OBJECT(column), or open the table from the side panel.",
        ),
        "oracle_placeholder_unsupported" => (
            "Polecenie zawiera zmienną wiązaną (na przykład :id), dla której edytor nie ma wartości. Wpisz wartość wprost w miejsce zmiennej.",
            "The statement contains a bind variable (for example :id) that the editor has no value for. Write the value itself in its place.",
        ),
        "oracle_binary_too_long" => (
            "Wartość binarna dłuższa niż 2000 bajtów nie mieści się w poleceniu SQL dla Oracle. Wstaw ją narzędziem, które przekazuje dane binarne osobno.",
            "A binary value longer than 2000 bytes does not fit into an SQL statement for Oracle. Insert it with a tool that passes binary data separately.",
        ),
        "oracle_compiled_with_errors" => (
            "Skrypt został wykonany, ale co najmniej jedna procedura, funkcja, pakiet, wyzwalacz albo typ ma błędy kompilacji i nie działa. Ich listę pokazuje SELECT name, type, line, position, text FROM user_errors ORDER BY name, sequence.",
            "The script was run, but at least one procedure, function, package, trigger or type has compilation errors and does not work. SELECT name, type, line, position, text FROM user_errors ORDER BY name, sequence lists them.",
        ),
        "oracle_columns_unknown" => (
            "Nie udało się odczytać kolumn tej tabeli, więc nie można dodać wiersza z samymi wartościami domyślnymi. Odśwież listę tabel i spróbuj ponownie.",
            "The columns of this table could not be read, so a row of default values cannot be added. Refresh the list of tables and try again.",
        ),
        _ => return None,
    };
    Some(if en { english } else { pl })
}

/// A statement ends after this long, as in every driver.
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(120);
/// Rows the server sends in one answer to a fetch.
const FETCH_ROWS: u32 = 250;
/// Rows of an import that travel in one request.
const IMPORT_CHUNK_ROWS: usize = 100;
const IMPORT_CHUNK_BYTES: usize = 256 * 1024;

/// What the server said about itself when the session was opened.
#[derive(Clone, Debug, Default)]
struct Facts {
    /// The first number of the version: 19, 21, 23.
    major: usize,
    /// The banner of the server with its full version.
    version: String,
    /// The pluggable database, or the database when there is none.
    database: String,
    /// The schema of the account.
    schema: String,
    /// The account may read `V$SESSION`.
    sessions: bool,
    /// The session limit of the instance, when the account may read it.
    limit: Option<String>,
}

fn text_at(row: &oracledb::Row, index: usize) -> String {
    row.get::<Option<String>>(index)
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Ask the server what it is and what this account may see.
fn facts(client: &Connection) -> Result<Facts, Failure> {
    let fail = |error: oracledb::Error| statement_failure(&error);
    let version = client.version().map_err(fail)?;
    let row = client
        .query_row(
            "SELECT SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA'), SYS_CONTEXT('USERENV', 'CON_NAME'), SYS_CONTEXT('USERENV', 'DB_NAME') FROM dual",
            &[],
        )
        .map_err(fail)?;
    let container = text_at(&row, 1);
    // Every account may read the banner; a server that says otherwise is
    // still named by its version.
    let banner = client
        .query_row("SELECT banner FROM v$version WHERE ROWNUM = 1", &[])
        .map(|row| text_at(&row, 0))
        .unwrap_or_default();
    let number = version.to_string();
    let named = if banner.is_empty() {
        format!("Oracle Database {number}")
    } else if banner.contains(&number) {
        banner
    } else {
        format!("{banner} ({number})")
    };
    // The views of running sessions belong to administrators. Without them
    // the "Monitor" shows what every account may know.
    let sessions = client
        .query_row("SELECT COUNT(*) FROM v$session WHERE ROWNUM = 1", &[])
        .is_ok();
    let limit = client
        .query_row("SELECT value FROM v$parameter WHERE name = 'sessions'", &[])
        .ok()
        .map(|row| text_at(&row, 0))
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()) && !value.is_empty());
    Ok(Facts {
        major: version.0,
        version: named,
        database: if container.is_empty() {
            text_at(&row, 2)
        } else {
            container
        },
        schema: text_at(&row, 0),
        sessions,
        limit,
    })
}

fn connect<'a>(request: &'a ConnectRequest<'a>) -> BoxFuture<'a, Result<Arc<dyn Session>>> {
    Box::pin(async move {
        let connector = Connector::new(request)?;
        let mut first = connector.open().await?;
        let facts = first.work(facts).await.map_err(|failure| failure.error)?;
        let dialect = Dialect {
            booleans: if facts.major >= 23 {
                ("TRUE", "FALSE")
            } else {
                DIALECT.booleans
            },
            column_types: match facts.major {
                23.. => TYPES_23,
                21 | 22 => TYPES_21,
                _ => TYPES,
            },
            ..DIALECT
        };
        Ok(Arc::new(OracleSession {
            pool: Pool::new(connector, first),
            dialect,
            facts,
            system_schemas: request.option("system_schemas") == "true",
            described: Mutex::default(),
        }) as Arc<dyn Session>)
    })
}

/// How a column of a page is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reading {
    /// As it is.
    Plain,
    /// A timestamp with a time zone, as text: the client library gives up
    /// on a value that carries the name of a region.
    Zoned,
    /// A type the client library does not read: only whether it is NULL.
    Opaque,
}

/// Whether the client library reads a column of this type, named as the
/// catalog names it.
fn readable(data_type: &str) -> bool {
    // Without the digits some types carry: `TIMESTAMP(6) WITH TIME ZONE`.
    let family = data_type.split('(').next().unwrap_or("").trim();
    matches!(
        family,
        "NUMBER"
            | "FLOAT"
            | "BINARY_FLOAT"
            | "BINARY_DOUBLE"
            | "VARCHAR2"
            | "VARCHAR"
            | "NVARCHAR2"
            | "CHAR"
            | "NCHAR"
            | "LONG"
            | "LONG RAW"
            | "RAW"
            | "DATE"
            | "ROWID"
            | "UROWID"
            | "CLOB"
            | "NCLOB"
            | "BLOB"
            | "JSON"
            | "BOOLEAN"
            | "VECTOR"
            | "TIMESTAMP"
    ) || family.starts_with("INTERVAL")
}

fn reading(data_type: &str, owned: bool) -> Reading {
    if owned || !readable(data_type) {
        Reading::Opaque
    } else if data_type.starts_with("TIMESTAMP")
        && data_type.contains("WITH TIME ZONE")
        && !data_type.contains("LOCAL")
    {
        Reading::Zoned
    } else {
        Reading::Plain
    }
}

/// The format a zoned timestamp is asked for in, and read back from.
const ZONED_FORMAT: &str = "SYYYY-MM-DD HH24:MI:SS.FF9 TZH:TZM";

/// `2024-02-29 13:14:15.500000000 +05:30` as the grid writes it: without
/// the zeros at the end of the fraction.
fn zoned_text(text: &str) -> String {
    let text = text.trim();
    let Some((moment, zone)) = text.rsplit_once(' ') else {
        return text.to_owned();
    };
    let moment = match moment.split_once('.') {
        Some((whole, fraction)) => {
            let fraction = fraction.trim_end_matches('0');
            if fraction.is_empty() {
                whole.to_owned()
            } else {
                format!("{whole}.{fraction}")
            }
        }
        None => moment.to_owned(),
    };
    format!("{moment} {zone}")
}

/// What the driver remembers of a table or view, to write its statements.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Described {
    /// Columns of the primary key, in the order of the key.
    keys: Vec<String>,
    /// Name and declared type of every column, in the order of `SELECT *`.
    types: Vec<(String, String)>,
    readings: Vec<Reading>,
}

impl Described {
    /// The select list that stands for `*` when not every column can be
    /// read as it is; `None` when `*` will do.
    fn projection(&self) -> Result<Option<String>> {
        if self
            .readings
            .iter()
            .all(|reading| *reading == Reading::Plain)
        {
            return Ok(None);
        }
        let mut items = Vec::with_capacity(self.types.len());
        for ((name, _), reading) in self.types.iter().zip(&self.readings) {
            let quoted = quote_identifier(&DIALECT, name)?;
            items.push(match reading {
                Reading::Plain => quoted,
                Reading::Zoned => format!("TO_CHAR({quoted}, '{ZONED_FORMAT}') AS {quoted}"),
                Reading::Opaque => {
                    format!("CASE WHEN {quoted} IS NULL THEN NULL ELSE 'x' END AS {quoted}")
                }
            });
        }
        Ok(Some(items.join(", ")))
    }

    /// Turn what the projection returned into what `*` would have shown.
    fn restore(&self, result: &mut QueryResult) {
        if result.columns.len() != self.types.len() {
            return;
        }
        for (index, reading) in self.readings.iter().enumerate() {
            if *reading == Reading::Plain {
                continue;
            }
            let declared = &self.types[index].1;
            result.columns[index].data_type.clone_from(declared);
            for row in &mut result.rows {
                let Some(Cell::Text(text)) = row.get(index) else {
                    continue;
                };
                row[index] = match reading {
                    Reading::Zoned => Cell::Text(zoned_text(text)),
                    _ => Cell::Unsupported(format!("<{declared}>")),
                };
            }
        }
    }
}

/// What a run does once its statements are through.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Finish {
    /// Commit what was changed.
    Commit,
    /// Commit when exactly one row was changed, otherwise roll back.
    ExactlyOne,
}

/// One statement of a run.
struct Statement {
    text: String,
    /// The stored unit it creates, to ask afterwards whether it compiled.
    unit: Option<script::Unit>,
}
impl From<String> for Statement {
    fn from(text: String) -> Self {
        Self { text, unit: None }
    }
}

/// The statements of one run.
struct Job {
    statements: Vec<Statement>,
    finish: Finish,
    /// A query of the run may have started a transaction: row locks of
    /// `FOR UPDATE`, or a table of another database.
    locking: bool,
}

/// Whether a run of queries only may still leave a transaction open.
fn may_lock(text: &str) -> bool {
    let upper = text.to_ascii_uppercase();
    upper.contains('@') || upper.contains("UPDATE")
}

/// Read the rows of a query into `result`, up to what a result may hold.
fn read(
    cursor: oracledb::Cursor,
    result: &mut QueryResult,
    cancel: &CancellationToken,
) -> Result<(), Failure> {
    let fail = |error: oracledb::Error| statement_failure(&error);
    let columns: Vec<Column> = cursor.columns().iter().map(Column::of).collect();
    result.columns = cursor
        .columns()
        .iter()
        .zip(&columns)
        .map(|(metadata, column)| ResultColumn {
            name: metadata.name().to_owned(),
            data_type: column.data_type.clone(),
        })
        .collect();
    result.rows.clear();
    result.truncated = false;
    let mut bytes = 0usize;
    for row in cursor {
        if cancel.is_cancelled() {
            return Err(Failure::code("query_cancelled"));
        }
        let mut row = row.map_err(fail)?;
        let mut cells = Vec::with_capacity(columns.len());
        for (index, column) in columns.iter().enumerate() {
            cells.push(column.cell(&mut row, index).map_err(fail)?);
        }
        let size = values::row_bytes(&cells);
        if result.rows.len() >= MAX_RESULT_ROWS || bytes.saturating_add(size) > MAX_RESULT_BYTES {
            // The rest of the rows is not wanted: the cursor is closed
            // with the next request, which the caller sends at once.
            result.truncated = true;
            break;
        }
        bytes += size;
        result.rows.push(cells);
    }
    Ok(())
}

/// Run the statements of a job on the calling thread: the last result set,
/// the number of changed rows, and the end of the transaction.
fn perform(
    client: &Connection,
    job: &Job,
    cancel: &CancellationToken,
) -> Result<QueryResult, Failure> {
    let fail = |error: oracledb::Error| statement_failure(&error);
    let mut result = QueryResult::default();
    let mut changed = false;
    // A unit was created that does not compile.
    let mut flawed = false;
    let ran = (|| {
        for Statement { text, unit } in &job.statements {
            if cancel.is_cancelled() {
                return Err(Failure::code("query_cancelled"));
            }
            // Large objects come as locators, so that none is read whole
            // before its length is known. The rows that travel with the
            // answer to the statement itself stay at the two of the client
            // library: with more, it misreads a result of more than eight
            // columns that has a large object or JSON among them.
            let mut statement = client
                .statement(text)
                .map_err(fail)?
                .fetch_lobs()
                .fetch_array_size(FETCH_ROWS)
                .exclude_from_cache()
                .build()
                .map_err(fail)?;
            if statement.is_query() {
                read(statement.query(&[]).map_err(fail)?, &mut result, cancel)?;
            } else {
                changed = true;
                let done = statement.execute(&[]).map_err(fail)?;
                if statement.is_dml() {
                    result.affected = result.affected.saturating_add(done.rows_affected());
                }
                // Oracle creates a unit that does not compile and calls
                // that a success: the catalog knows better.
                if let Some(unit) = unit {
                    flawed |= compiled_with_errors(client, unit).map_err(fail)?;
                }
            }
        }
        Ok(())
    })();
    match ran {
        Ok(()) => {
            if job.finish == Finish::ExactlyOne && result.affected != 1 {
                client.rollback().map_err(fail)?;
                return Err(Failure::code("row_changed_reload_required"));
            }
            if changed {
                client.commit().map_err(fail)?;
            } else if job.locking || result.truncated {
                // Ends the locks of a query, and closes a cursor whose rows
                // were not all read.
                client.rollback().map_err(fail)?;
            }
            // The script ran to its end; what it created is there, broken.
            if flawed {
                return Err(Failure::code("oracle_compiled_with_errors"));
            }
            Ok(result)
        }
        Err(failure) => {
            // Nothing of a run that failed stays, and nothing stays open.
            if !failure.broken && (changed || job.locking) && client.rollback().is_err() {
                return Err(Failure {
                    broken: true,
                    ..failure
                });
            }
            Err(failure)
        }
    }
}

/// Whether the catalog lists compilation errors for a unit.
fn compiled_with_errors(client: &Connection, unit: &script::Unit) -> Result<bool, oracledb::Error> {
    let row = client.query_row(
        "SELECT COUNT(*) FROM all_errors WHERE owner = NVL(:1, SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')) AND name = :2 AND type = :3 AND attribute = 'ERROR'",
        &[&unit.owner, &unit.name, &unit.kind],
    )?;
    Ok(number_at(&row, 0).is_some_and(|errors| errors > 0))
}

struct OracleSession {
    pool: Pool,
    /// The dialect of this server: its column types and booleans.
    dialect: Dialect,
    facts: Facts,
    /// List the schemas Oracle maintains as well.
    system_schemas: bool,
    /// What `columns` learned, by schema and name.
    described: Mutex<HashMap<(String, String), Arc<Described>>>,
}

impl OracleSession {
    /// Run `work` on a connection of the pool, with the limits every driver
    /// keeps. When the caller gives up, the server is told to stop and the
    /// connection is not used again.
    async fn on_connection<T, F>(&self, cancel: CancellationToken, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection, &CancellationToken) -> Result<T, Failure> + Send + 'static,
    {
        let deadline = tokio::time::sleep(STATEMENT_TIMEOUT);
        tokio::pin!(deadline);
        let mut lease = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(anyhow!("query_cancelled")),
            () = &mut deadline => return Err(anyhow!("query_timeout")),
            lease = self.pool.acquire() => lease?,
        };
        let breaker = lease.breaker();
        let ended = {
            let flag = cancel.clone();
            let running = Conn::work(&mut lease, move |client| work(client, &flag));
            tokio::pin!(running);
            tokio::select! {
                biased;
                () = cancel.cancelled() => Err("query_cancelled"),
                () = &mut deadline => Err("query_timeout"),
                outcome = &mut running => Ok(outcome),
            }
        };
        match ended {
            // The statement still runs on its thread. The relay asks the
            // server to stop it and closes the connection, which ends the
            // call of the library; nobody waits for that here.
            Err(code) => {
                if let Some(breaker) = breaker {
                    breaker.interrupt();
                }
                Err(anyhow!(code))
            }
            Ok(Ok(value)) => {
                self.pool.release(lease);
                Ok(value)
            }
            Ok(Err(failure)) => {
                self.pool.release(lease);
                Err(failure.error)
            }
        }
    }

    async fn run(&self, job: Job, cancel: CancellationToken) -> Result<QueryResult> {
        let started = Instant::now();
        let mut result = self
            .on_connection(cancel, move |client, cancel| perform(client, &job, cancel))
            .await?;
        result.duration_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }

    /// Run a query the driver wrote itself.
    async fn query(&self, text: String, cancel: CancellationToken) -> Result<QueryResult> {
        self.run(
            Job {
                statements: vec![text.into()],
                finish: Finish::Commit,
                locking: false,
            },
            cancel,
        )
        .await
    }

    fn remembered(&self, container: &TableRef) -> Option<Arc<Described>> {
        let key = (container.schema.clone(), container.name.clone());
        self.described.lock().ok()?.get(&key).cloned()
    }

    /// The schema a container without one belongs to.
    fn schema_of<'a>(&'a self, container: &'a TableRef) -> &'a str {
        if container.schema.is_empty() {
            &self.facts.schema
        } else {
            &container.schema
        }
    }

    /// Columns of a container, and what they say about its statements.
    async fn describe(&self, container: &TableRef) -> Result<(Vec<ColumnInfo>, Arc<Described>)> {
        let owner = self.schema_of(container).to_owned();
        let name = container.name.clone();
        let (columns, described) = self
            .on_connection(CancellationToken::new(), move |client, _| {
                describe_container(client, &owner, &name)
            })
            .await?;
        let described = Arc::new(described);
        if !columns.is_empty()
            && let Ok(mut known) = self.described.lock()
        {
            known.insert(
                (container.schema.clone(), container.name.clone()),
                Arc::clone(&described),
            );
        }
        Ok((columns, described))
    }

    /// What is known about a container, read once when nothing is.
    async fn known(&self, container: &TableRef) -> Option<Arc<Described>> {
        match self.remembered(container) {
            Some(described) => Some(described),
            None => self
                .describe(container)
                .await
                .ok()
                .map(|(_, described)| described)
                .filter(|described| !described.types.is_empty()),
        }
    }

    fn columns_of<'a>(&self, described: Option<&'a Described>) -> Columns<'a> {
        Columns {
            types: described.map(|described| described.types.as_slice()),
            booleans: self.facts.major >= 23,
        }
    }

    fn page(&self, page: &PageRequest, described: Option<&Described>) -> Result<String> {
        let order = match described {
            None => Order::Unknown,
            Some(described) if !described.keys.is_empty() => Order::Key(&described.keys),
            // A view has no address of a row to order by.
            Some(_) if page.container.kind.contains("table") => Order::Rowid,
            Some(_) => Order::Unknown,
        };
        script::page_statement(page, &self.columns_of(described), order)
    }
}

fn number_at(row: &oracledb::Row, index: usize) -> Option<i64> {
    row.get::<Option<oracledb::OracleNumber>>(index)
        .ok()
        .flatten()
        .and_then(|number| number.to_string().parse().ok())
}

/// Read the columns of a table or view and the columns of its primary key.
fn describe_container(
    client: &Connection,
    owner: &str,
    name: &str,
) -> Result<(Vec<ColumnInfo>, Described), Failure> {
    let fail = |error: oracledb::Error| statement_failure(&error);
    let owner = owner.to_owned();
    let name = name.to_owned();
    // The key first: the constraint, then its columns in their order.
    let mut keys: Vec<(i64, String)> = Vec::new();
    let cursor = client
        .query(
            "SELECT cc.column_name, cc.position FROM all_constraints k \
             JOIN all_cons_columns cc ON cc.owner = k.owner AND cc.constraint_name = k.constraint_name AND cc.table_name = k.table_name \
             WHERE k.owner = :1 AND k.table_name = :2 AND k.constraint_type = 'P'",
            &[&owner, &name],
        )
        .map_err(fail)?;
    for row in cursor {
        let row = row.map_err(fail)?;
        keys.push((number_at(&row, 1).unwrap_or(0), text_at(&row, 0)));
    }
    keys.sort();
    let keys: Vec<String> = keys.into_iter().map(|(_, column)| column).collect();
    // The default of a column is a LONG, which a join would not let through.
    let cursor = client
        .query(
            "SELECT c.column_name, c.data_type, c.data_length, c.char_length, c.char_used, c.data_precision, c.data_scale, \
             c.nullable, c.identity_column, c.virtual_column, c.data_type_owner, c.data_default \
             FROM all_tab_cols c \
             WHERE c.owner = :1 AND c.table_name = :2 AND c.hidden_column = 'NO' AND c.user_generated = 'YES' \
             ORDER BY c.column_id",
            &[&owner, &name],
        )
        .map_err(fail)?;
    let mut columns = Vec::new();
    let mut described = Described {
        keys,
        ..Described::default()
    };
    for row in cursor {
        let row = row.map_err(fail)?;
        let column = text_at(&row, 0);
        let raw = text_at(&row, 1);
        let declared = values::declared_type(
            &raw,
            number_at(&row, 2),
            number_at(&row, 3),
            text_at(&row, 4) == "C",
            number_at(&row, 5),
            number_at(&row, 6),
        );
        let default = text_at(&row, 11).trim().to_owned();
        let default = if text_at(&row, 8) == "YES" {
            Some("IDENTITY".to_owned())
        } else if default.is_empty() {
            None
        } else if text_at(&row, 9) == "YES" {
            Some(format!("AS ({default})"))
        } else {
            Some(default)
        };
        described
            .readings
            .push(reading(&raw, !text_at(&row, 10).is_empty()));
        described.types.push((column.clone(), declared.clone()));
        columns.push(ColumnInfo {
            primary_key: described.keys.contains(&column),
            name: column,
            data_type: declared,
            nullable: text_at(&row, 7) == "Y",
            default,
        });
    }
    Ok((columns, described))
}

impl Session for OracleSession {
    fn dialect(&self) -> &Dialect {
        &self.dialect
    }

    fn execute<'a>(
        &'a self,
        text: &'a str,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            // What the connection dialog tries a connection with: `SELECT 1`
            // on every engine, which Oracle before 23ai refuses for its
            // missing FROM. Every engine answers it with the integer 1; a
            // number without a declared precision is a decimal here.
            if script::is_probe(text) {
                let mut result = self.query("SELECT 1 FROM dual".into(), cancel).await?;
                for cell in result.rows.iter_mut().flatten() {
                    if let Cell::Decimal(one) = cell {
                        *cell = Cell::Integer(std::mem::take(one));
                    }
                }
                return Ok(result);
            }
            let mut statements = Vec::new();
            for step in script::steps(text)? {
                // The client library would wait for the value of anything
                // that looks like a placeholder. In a stored unit it is
                // none (`:NEW` of a trigger), so the unit travels as text.
                let unit = match step.kind {
                    Kind::Unit | Kind::Sql => script::unit(&step.text),
                    Kind::Block => None,
                };
                let text = match (script::has_placeholder(&step.text), step.kind) {
                    (false, _) => step.text,
                    (true, Kind::Unit) => script::dynamic(&step.text),
                    (true, _) => return Err(anyhow!("oracle_placeholder_unsupported")),
                };
                anyhow::ensure!(text.len() <= MAX_SQL_BYTES, "sql_invalid");
                statements.push(Statement { text, unit });
            }
            if statements.is_empty() {
                return Ok(QueryResult::default());
            }
            self.run(
                Job {
                    statements,
                    finish: Finish::Commit,
                    locking: may_lock(text),
                },
                cancel,
            )
            .await
        })
    }

    fn execute_one<'a>(&'a self, statement: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            anyhow::ensure!(statement.len() <= MAX_SQL_BYTES, "sql_invalid");
            self.run(
                Job {
                    statements: vec![statement.to_owned().into()],
                    finish: Finish::ExactlyOne,
                    locking: true,
                },
                CancellationToken::new(),
            )
            .await
            .map(|_| ())
        })
    }

    fn containers<'a>(&'a self) -> BoxFuture<'a, Result<Vec<TableRef>>> {
        Box::pin(async move {
            // Tables without the ones Oracle keeps for something else: the
            // storage of a nested table or of a materialized view, the
            // overflow of an index-organized table, what was dropped. The
            // schema of the account comes first.
            let maintained = if self.system_schemas {
                ""
            } else {
                "WHERE u.oracle_maintained = 'N' OR c.owner = SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') "
            };
            let statement = format!(
                "SELECT c.owner, c.name, c.kind FROM (\
                 SELECT t.owner, t.table_name AS name, 'table' AS kind FROM all_tables t \
                 WHERE t.nested = 'NO' AND t.secondary = 'N' AND t.dropped = 'NO' \
                 AND (t.iot_type IS NULL OR t.iot_type = 'IOT') \
                 AND NOT EXISTS (SELECT 1 FROM all_mviews m WHERE m.owner = t.owner AND m.mview_name = t.table_name) \
                 UNION ALL SELECT v.owner, v.view_name, 'view' FROM all_views v \
                 UNION ALL SELECT m.owner, m.mview_name, 'materialized view' FROM all_mviews m\
                 ) c JOIN all_users u ON u.username = c.owner \
                 {maintained}\
                 ORDER BY CASE WHEN c.owner = SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') THEN 0 ELSE 1 END, c.owner, c.name"
            );
            let result = self.query(statement, CancellationToken::new()).await?;
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
        Box::pin(async move { Ok(self.describe(container).await?.0) })
    }

    fn page_text(&self, page: &PageRequest) -> Result<String> {
        self.page(page, self.remembered(&page.container).as_deref())
    }

    fn read_page<'a>(
        &'a self,
        page: &'a PageRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            // The key orders the page and the types decide how a filter is
            // written. They are read once per container; when that fails
            // the page is read in the order of the scan.
            let described = self.known(&page.container).await;
            let text = self.page(page, described.as_deref())?;
            let projection = match &described {
                Some(described) => described.projection()?,
                None => None,
            };
            let Some(projection) = projection else {
                return self.query(text, cancel).await;
            };
            // Columns the client library cannot read are asked for as
            // something it can: the statement the tab shows stays `*`.
            let asked = text.replacen("SELECT *", &format!("SELECT {projection}"), 1);
            let mut result = self.query(asked, cancel).await?;
            if let Some(described) = &described {
                described.restore(&mut result);
            }
            Ok(result)
        })
    }

    fn change_text(&self, change: &RowChange) -> Result<String> {
        let (RowChange::UpdateCell { table, .. }
        | RowChange::InsertRow { table, .. }
        | RowChange::DeleteRow { table, .. }) = change;
        let described = self.remembered(table);
        script::change_statement(change, &self.columns_of(described.as_deref()))
    }

    fn apply<'a>(&'a self, change: &'a RowChange) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // How a value is written and compared depends on the type of
            // its column, which is read once per table.
            let (RowChange::UpdateCell { table, .. }
            | RowChange::InsertRow { table, .. }
            | RowChange::DeleteRow { table, .. }) = change;
            let _ = self.known(table).await;
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
            let described = self.known(container).await;
            let known = self.columns_of(described.as_deref());
            // Rows travel as the statements of PL/SQL blocks, a hundred at
            // a time, all in the transaction of one run.
            let mut statements = Vec::new();
            let mut block = String::from("BEGIN\n");
            let mut pending = 0usize;
            for (index, row) in rows.iter().enumerate() {
                let statement = script::import_statement(&target, &names, columns, row, &known)?;
                anyhow::ensure!(statement.len() <= MAX_SQL_BYTES / 2, "import_row_too_large");
                block.push_str(&statement);
                pending += 1;
                if index + 1 == rows.len()
                    || pending >= IMPORT_CHUNK_ROWS
                    || block.len() >= IMPORT_CHUNK_BYTES
                {
                    block.push_str("END;");
                    statements.push(std::mem::replace(&mut block, String::from("BEGIN\n")).into());
                    pending = 0;
                }
            }
            if !statements.is_empty() {
                self.run(
                    Job {
                        statements,
                        finish: Finish::Commit,
                        locking: true,
                    },
                    cancel,
                )
                .await?;
            }
            Ok(QueryResult {
                affected: rows.len() as u64,
                duration_ms: started.elapsed().as_millis() as u64,
                ..Default::default()
            })
        })
    }

    fn server_stats<'a>(&'a self) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            let server = format!(
                "{} AS \"database\", {} AS \"version\"",
                script::short_text(&self.facts.database),
                script::short_text(&self.facts.version)
            );
            // Sessions of people and programs, and the ones that run
            // something right now, without this one.
            let activity = if self.facts.sessions {
                ", (SELECT COUNT(*) FROM v$session WHERE type = 'USER') AS \"connections\", \
                 (SELECT COUNT(*) FROM v$session WHERE type = 'USER' AND status = 'ACTIVE' AND sid <> SYS_CONTEXT('USERENV', 'SID')) AS \"active\""
            } else {
                ""
            };
            let limit = self
                .facts
                .limit
                .as_ref()
                .map(|limit| format!(", {limit} AS \"max_connections\""))
                .unwrap_or_default();
            self.query(
                format!(
                    "SELECT {server}{activity}{limit}, (SELECT COUNT(*) FROM user_tables) AS \"tables\" FROM dual"
                ),
                CancellationToken::new(),
            )
            .await
        })
    }

    fn close<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move { self.pool.close() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{Filter, FilterOp, QueryBuilder};
    use crate::sql::{editable_table, literal, statements};

    fn table(name: &str) -> TableRef {
        TableRef {
            schema: "BAZA".into(),
            name: name.into(),
            kind: "table".into(),
        }
    }

    #[test]
    fn the_dialect_writes_oracle_sql() {
        assert_eq!(
            quote_identifier(&DIALECT, "Order Details").unwrap(),
            "\"Order Details\""
        );
        assert_eq!(quote_identifier(&DIALECT, "a\"b").unwrap(), "\"a\"\"b\"");
        // A name is written as the catalog holds it: Oracle folds only
        // what is not quoted.
        assert_eq!(
            qualified(&DIALECT, &table("EMPLOYEES")).unwrap(),
            "\"BAZA\".\"EMPLOYEES\""
        );
        assert_eq!(
            qualified(
                &DIALECT,
                &TableRef {
                    schema: String::new(),
                    ..table("zamówienia")
                }
            )
            .unwrap(),
            "\"zamówienia\""
        );
        assert_eq!(
            literal(&DIALECT, &Cell::Bytes(vec![0, 0xff])).unwrap(),
            "HEXTORAW('00ff')"
        );
        assert_eq!(literal(&DIALECT, &Cell::Bool(true)).unwrap(), "1");
        assert_eq!(
            crate::builder::create_table(
                &DIALECT,
                &TableRef {
                    schema: String::new(),
                    ..table("t")
                },
                &[
                    crate::builder::CreateColumn {
                        name: "id".into(),
                        data_type: DIALECT.column_types[0].into(),
                        nullable: false,
                        primary_key: true,
                    },
                    crate::builder::CreateColumn {
                        name: "note".into(),
                        data_type: "CLOB".into(),
                        nullable: true,
                        primary_key: false,
                    }
                ]
            )
            .unwrap(),
            "CREATE TABLE \"t\" (\n  \"id\" NUMBER(19) NOT NULL,\n  \"note\" CLOB,\n  PRIMARY KEY (\"id\")\n)"
        );
        // Every list of types starts with the same key type and has what
        // the checklist looks for.
        for types in [TYPES, TYPES_21, TYPES_23] {
            assert_eq!(types[0], TYPES[0]);
            assert!(types.starts_with(TYPES) || types == TYPES);
            assert!(types.iter().any(|name| name.contains("DECIMAL")));
            assert!(types.iter().find(|name| name.contains("CHAR")) == Some(&"VARCHAR2(255 CHAR)"));
        }
        // The query builder pages with OFFSET and FETCH.
        let built = QueryBuilder::table(table("orders")).sql(&DIALECT).unwrap();
        assert_eq!(
            built,
            "SELECT *\nFROM \"BAZA\".\"orders\"\nOFFSET 0 ROWS FETCH NEXT 500 ROWS ONLY"
        );
    }

    fn described(keys: &[&str], types: &[(&str, &str)]) -> Described {
        Described {
            keys: keys.iter().map(|key| (*key).to_owned()).collect(),
            types: types
                .iter()
                .map(|(name, data_type)| ((*name).to_owned(), (*data_type).to_owned()))
                .collect(),
            readings: types
                .iter()
                .map(|(_, data_type)| reading(data_type, false))
                .collect(),
        }
    }

    fn page_of(page: &PageRequest, described: Option<&Described>) -> String {
        let columns = Columns {
            types: described.map(|described| described.types.as_slice()),
            booleans: false,
        };
        let order = match described {
            None => Order::Unknown,
            Some(described) if !described.keys.is_empty() => Order::Key(&described.keys),
            Some(_) if page.container.kind.contains("table") => Order::Rowid,
            Some(_) => Order::Unknown,
        };
        script::page_statement(page, &columns, order).unwrap()
    }

    #[test]
    fn a_page_is_ordered_by_the_key_or_by_the_address_of_the_row() {
        let orders = table("orders");
        let first = PageRequest::first(orders.clone());
        // Nothing is known yet: what the shared builder writes.
        assert_eq!(
            page_of(&first, None),
            QueryBuilder::page(&first).sql(&DIALECT).unwrap()
        );
        let single = described(&["ID"], &[("ID", "NUMBER(10)")]);
        assert_eq!(
            page_of(&first, Some(&single)),
            "SELECT *\nFROM \"BAZA\".\"orders\"\nORDER BY \"ID\"\nOFFSET 0 ROWS FETCH NEXT 500 ROWS ONLY"
        );
        let composite = described(&["order id", "line\"no"], &[]);
        let by_date = PageRequest {
            order_by: Some(("placed".into(), true)),
            offset: 500,
            ..first.clone()
        };
        assert_eq!(
            page_of(&by_date, Some(&composite)),
            "SELECT *\nFROM \"BAZA\".\"orders\"\nORDER BY \"placed\" DESC, \"order id\", \"line\"\"no\"\nOFFSET 500 ROWS FETCH NEXT 500 ROWS ONLY"
        );
        let by_key = PageRequest {
            order_by: Some(("order id".into(), false)),
            ..first.clone()
        };
        assert!(
            page_of(&by_key, Some(&composite))
                .contains("ORDER BY \"order id\" ASC, \"line\"\"no\"\n")
        );
        // A typed condition goes in parentheses, and only a checked one.
        let typed = PageRequest {
            condition: Some("\"ID\" = 19 OR note IS NULL".into()),
            ..first.clone()
        };
        assert!(
            page_of(&typed, Some(&single))
                .contains("\nWHERE (\"ID\" = 19 OR note IS NULL)\nORDER BY \"ID\"\n")
        );
        let refused = PageRequest {
            condition: Some("1 = 1 -- x".into()),
            ..first.clone()
        };
        let columns = Columns {
            types: None,
            booleans: false,
        };
        assert!(script::page_statement(&refused, &columns, Order::Unknown).is_err());
        // A table without a key, and a view.
        let keyless = described(&[], &[("A", "NUMBER")]);
        assert!(page_of(&first, Some(&keyless)).contains("\nORDER BY ROWID\n"));
        assert!(page_of(&by_date, Some(&keyless)).contains("\nORDER BY \"placed\" DESC, ROWID\n"));
        let view = PageRequest::first(TableRef {
            kind: "view".into(),
            ..orders.clone()
        });
        assert!(!page_of(&view, Some(&keyless)).contains("ORDER BY"));
        // Every page is one statement on its table, so its rows are edited.
        for (page, known) in [
            (&first, &single),
            (&by_date, &composite),
            (&by_key, &composite),
            (&first, &keyless),
        ] {
            let text = page_of(page, Some(known));
            assert_eq!(statements(&DIALECT, &text).unwrap().len(), 1, "{text}");
            assert!(
                editable_table(&DIALECT, &text, std::slice::from_ref(&orders)).is_some(),
                "{text}"
            );
        }
    }

    #[test]
    fn a_filter_is_written_for_the_type_of_its_column() {
        let known = described(
            &["ID"],
            &[
                ("ID", "NUMBER(10)"),
                ("LABEL", "VARCHAR2(50)"),
                ("PRICE", "NUMBER(12,2)"),
                ("PLACED", "DATE"),
                ("NOTE", "CLOB"),
                ("DOC", "JSON"),
                ("PHOTO", "BLOB"),
            ],
        );
        let filtered = |column: &str, op: FilterOp, value: Cell| {
            let page = PageRequest {
                filters: vec![Filter {
                    column: column.into(),
                    op,
                    value,
                }],
                ..PageRequest::first(table("orders"))
            };
            page_of(&page, Some(&known))
        };
        let contains = |column: &str, value: &str| {
            filtered(column, FilterOp::Contains, Cell::Text(value.into()))
        };
        assert!(contains("LABEL", "50%_x!").contains(
            "WHERE LOWER(\"LABEL\") LIKE LOWER('%50!%!_x!!%') ESCAPE '!'\nORDER BY \"ID\"\n"
        ));
        // A number is searched as the grid shows it, with its zero.
        assert!(contains("PRICE", "0.5").contains(
            "WHERE LTRIM(REPLACE(REPLACE(' ' || TO_CHAR(\"PRICE\"), ' -.', ' -0.'), ' .', ' 0.')) LIKE LOWER('%0.5%')"
        ));
        assert!(contains("PLACED", "2026-10").contains("WHERE LOWER(\"PLACED\") LIKE "));
        assert!(contains("NOTE", "x").contains("WHERE LOWER(\"NOTE\") LIKE "));
        assert!(contains("DOC", "x").contains("WHERE LOWER(\"DOC\") LIKE "));
        assert!(
            contains("PHOTO", "ff")
                .contains("WHERE LOWER(RAWTOHEX(DBMS_LOB.SUBSTR(\"PHOTO\", 2000, 1))) LIKE ")
        );
        // An empty text is NULL: equal to it means "is NULL".
        assert!(
            filtered("LABEL", FilterOp::Equal, Cell::Text(String::new()))
                .contains("WHERE \"LABEL\" IS NULL\n")
        );
        assert!(
            filtered("LABEL", FilterOp::NotEqual, Cell::Text(String::new()))
                .contains("WHERE \"LABEL\" IS NOT NULL\n")
        );
        assert!(
            filtered("LABEL", FilterOp::NotEqual, Cell::Text("a".into()))
                .contains("WHERE NOT (\"LABEL\" = 'a')\n")
        );
        assert!(
            filtered("NOTE", FilterOp::Equal, Cell::Text("a".into()))
                .contains("WHERE DBMS_LOB.COMPARE(\"NOTE\", TO_CLOB('a')) = 0\n")
        );
        assert!(
            filtered("PRICE", FilterOp::Greater, Cell::Decimal("1.50".into()))
                .contains("WHERE \"PRICE\" > 1.50\n")
        );
        // Whatever is typed, the page stays one statement on one table.
        let orders = table("orders");
        for column in ["LABEL", "PRICE", "PLACED", "NOTE", "DOC", "PHOTO"] {
            for value in [
                "pend",
                "\\",
                "'",
                "'; DROP TABLE a; --",
                "100%",
                "a_b",
                "!",
                "/* x",
                "-- x",
                "zażółć",
                "q'[",
                ":name",
                "/",
            ] {
                let text = contains(column, value);
                assert_eq!(statements(&DIALECT, &text).unwrap().len(), 1, "{text}");
                assert_eq!(script::steps(&text).unwrap().len(), 1, "{text}");
                assert!(!script::has_placeholder(&text), "{text}");
                assert!(
                    editable_table(&DIALECT, &text, std::slice::from_ref(&orders)).is_some(),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn columns_the_client_cannot_read_are_asked_for_as_text() {
        let plain = described(&["ID"], &[("ID", "NUMBER(10)"), ("NOTE", "CLOB")]);
        assert_eq!(plain.projection().unwrap(), None);
        let mut special = described(
            &["ID"],
            &[
                ("ID", "NUMBER(10)"),
                ("SEEN", "TIMESTAMP(6) WITH TIME ZONE"),
                ("LOCAL", "TIMESTAMP(6) WITH LOCAL TIME ZONE"),
                ("DOC", "XMLTYPE"),
            ],
        );
        assert_eq!(
            special.readings,
            [
                Reading::Plain,
                Reading::Zoned,
                Reading::Plain,
                Reading::Opaque
            ]
        );
        // A type of a user is one the catalog names with its owner.
        assert_eq!(reading("NUMBER", true), Reading::Opaque);
        special.readings[3] = Reading::Opaque;
        assert_eq!(
            special.projection().unwrap().unwrap(),
            "\"ID\", TO_CHAR(\"SEEN\", 'SYYYY-MM-DD HH24:MI:SS.FF9 TZH:TZM') AS \"SEEN\", \"LOCAL\", CASE WHEN \"DOC\" IS NULL THEN NULL ELSE 'x' END AS \"DOC\""
        );
        let column = |name: &str| ResultColumn {
            name: name.into(),
            data_type: "VARCHAR2(40)".into(),
        };
        let mut result = QueryResult {
            columns: vec![column("ID"), column("SEEN"), column("LOCAL"), column("DOC")],
            rows: vec![
                vec![
                    Cell::Integer("1".into()),
                    Cell::Text(" 2024-02-29 13:14:15.500000000 +05:30".into()),
                    Cell::Text("2024-02-29 13:14:15".into()),
                    Cell::Text("x".into()),
                ],
                vec![
                    Cell::Integer("2".into()),
                    Cell::Text(" 2024-02-29 13:14:15.000000000 -08:00".into()),
                    Cell::Null,
                    Cell::Null,
                ],
            ],
            ..Default::default()
        };
        special.restore(&mut result);
        assert_eq!(
            result.rows[0][1],
            Cell::Text("2024-02-29 13:14:15.5 +05:30".into())
        );
        assert_eq!(
            result.rows[1][1],
            Cell::Text("2024-02-29 13:14:15 -08:00".into())
        );
        assert_eq!(result.rows[0][3], Cell::Unsupported("<XMLTYPE>".into()));
        assert_eq!(result.rows[1][3], Cell::Null);
        assert_eq!(result.columns[1].data_type, "TIMESTAMP(6) WITH TIME ZONE");
        assert_eq!(result.columns[3].data_type, "XMLTYPE");
        assert_eq!(result.columns[2].data_type, "VARCHAR2(40)");
        assert_eq!(
            zoned_text("-4712-01-01 00:00:00.000000000 +00:00"),
            "-4712-01-01 00:00:00 +00:00"
        );
    }

    #[test]
    fn the_products_are_presets_of_one_driver() {
        assert_eq!(ENGINE.id(), "oracle");
        assert_eq!(Engine::from_id("OracleDB"), Some(ENGINE));
        assert_eq!(ENGINE.port(), 1521);
        assert_eq!(ENGINE.product("").id, "oracle-database");
        for product in SPEC.products {
            assert_eq!(product.default_port, 1521, "{}", product.id);
            // Every product says how to sign in.
            let (pl, en) = product.note.expect("a note");
            assert!(pl.contains("nazwą użytkownika i hasłem"), "{pl}");
            assert!(en.contains("a user name and a password"), "{en}");
            assert_eq!(SPEC.capabilities_of(product), &Capabilities::SQL);
            assert_eq!(SPEC.dialect_of(product), &DIALECT);
            assert!(product.matches("oracle"), "{}", product.id);
        }
        let tls = |id: &str| ENGINE.product(id).default_tls;
        assert_eq!(tls("oracle-database"), TlsMode::Disable);
        assert_eq!(tls("oracle-database-free"), TlsMode::Disable);
        assert_eq!(tls("amazon-rds-oracle"), TlsMode::Disable);
        assert_eq!(tls("oracle-autonomous-database"), TlsMode::VerifyFull);
        assert_eq!(
            ENGINE.product("oracle-database-free").default_database,
            "FREEPDB1"
        );
        assert!(ENGINE.product("amazon-rds-oracle").matches("aws rds"));
        assert!(ENGINE.product("oracle-autonomous-database").matches("adb"));
        // What the service cannot do here is said where it is chosen.
        let (pl, en) = ENGINE.product("oracle-autonomous-database").note.unwrap();
        assert!(pl.contains("portfel") && en.contains("wallet"));
        // The service name is the database field of the dialog, and the
        // other fields are the options.
        assert_eq!(SPEC.form.database_label, ("Nazwa usługi", "Service name"));
        assert_eq!(
            SPEC.options
                .iter()
                .map(|field| field.key)
                .collect::<Vec<_>>(),
            ["sid", "role", "system_schemas"]
        );
        // Every code of the driver has a sentence in both languages.
        for code in [
            "oracle_login_required",
            "oracle_service_required",
            "oracle_service_and_sid",
            "oracle_service_invalid",
            "oracle_sid_unknown",
            "oracle_listener_busy",
            "oracle_instance_restricted",
            "oracle_listener_refused",
            "oracle_instance_unavailable",
            "oracle_account_locked",
            "oracle_password_expired",
            "oracle_sys_needs_role",
            "oracle_server_too_old",
            "oracle_native_encryption_required",
            "oracle_redirect_unsupported",
            "oracle_redirect_through_tunnel",
            "oracle_ca_file_unreadable",
            "oracle_client_unsupported",
            "oracle_type_unsupported",
            "oracle_placeholder_unsupported",
            "oracle_binary_too_long",
            "oracle_compiled_with_errors",
            "oracle_columns_unknown",
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
}
