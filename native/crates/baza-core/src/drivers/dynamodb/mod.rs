//! Amazon DynamoDB and what speaks its API elsewhere: DynamoDB Local and
//! ScyllaDB Alternator.
//!
//! The service is one HTTPS endpoint with a JSON API, signed with AWS
//! Signature Version 4. This driver speaks it directly ([`client`],
//! [`sigv4`]); no AWS SDK is involved.
//!
//! * **Tables** are the containers; an item is a row whose attributes are
//!   the columns, found by looking at items ([`browse`]). Values keep their
//!   DynamoDB types exactly ([`value`]).
//! * **Pages** are a `Scan` read forward from the key of the last item, or
//!   a `Query` when the filter names the partition key. There is no offset
//!   and no search: a filter is checked on what a scan reads, and a page
//!   reads at most [`browse::PAGE_CALLS`] MB.
//! * **Changes** are `UpdateItem`, `PutItem` and `DeleteItem` by primary
//!   key, each with the condition of the contract.
//! * **The editor** runs PartiQL (`ExecuteStatement`), puts a JSON document
//!   after `INSERT INTO "table" VALUE`, and sends any operation of the API
//!   written as its name and its JSON body ([`partiql`]).
use super::{PageRequest, RowChange, Session};
use crate::engine::{
    Badge, Capabilities, ConnectRequest, ConnectionForm, Dialect, Engine, EngineSpec, FieldUse,
    Group, OptionField, OptionKind, Paging, ProductSpec,
};
use crate::model::{Cell, ColumnInfo, QueryResult, ResultColumn, TableRef, TlsMode};
use crate::sql::MAX_RESULT_ROWS;
use anyhow::{Result, anyhow, bail, ensure};
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

mod browse;
mod client;
mod partiql;
mod sigv4;
mod value;

use browse::{Attributes, Cursors, Mark, Mode, OVERVIEW, PAGE_CALLS, Plan, TableInfo, Test};
use client::Client;
use value::Hint;

pub const ENGINE: Engine = Engine::new(&SPEC);

/// How PartiQL is written, as far as the shared reader of SQL text needs
/// it. Pages and grid changes do not come from here.
pub static DIALECT: Dialect = Dialect {
    // DynamoDB has no OFFSET and PartiQL no LIMIT: the session pages itself.
    paging: Paging::None,
    // The types a key attribute may have.
    column_types: &["S", "N", "B"],
    ..Dialect::ANSI
};

pub static SPEC: EngineSpec = EngineSpec {
    id: "dynamodb",
    aliases: &["dynamo", "alternator"],
    portable_name: "dynamodb",
    label: "Amazon DynamoDB",
    available: true,
    form: ConnectionForm {
        database: FieldUse::Required,
        database_label: ("Region", "Region"),
        username: FieldUse::Required,
        password: FieldUse::Required,
        ..ConnectionForm::SERVER
    },
    capabilities: Capabilities {
        // PartiQL in the SQL editor; tables directly under the connection.
        schemas: false,
        filter: true,
        // Within limits: see `browse`.
        sort: true,
        edit_cells: true,
        insert_rows: true,
        delete_rows: true,
        // A table is a key schema and a billing mode, not a list of
        // columns, and PartiQL has neither joins nor GROUP BY.
        table_designer: false,
        query_builder: false,
        import: true,
        export: true,
        monitor: true,
        ..Capabilities::SQL
    },
    dialect: DIALECT,
    options: OPTIONS,
    products: PRODUCTS,
    connect,
    describe: Some(describe),
};

static OPTIONS: &[OptionField] = &[
    OptionField {
        key: "endpoint",
        label: ("Adres URL punktu końcowego", "Endpoint URL"),
        kind: OptionKind::Text,
        default: "",
        placeholder: "https://dynamodb.eu-central-1.amazonaws.com",
        help: Some((
            "Puste: usługa AWS w podanym regionie (pola Host i Port nie są wtedy używane), a dla DynamoDB Local i Alternatora adres z pól Host i Port. Wpisz adres URL tylko dla innego punktu końcowego, na przykład VPC. Przez tunel SSH pola Host i Port wskazują punkt końcowy widziany z serwera SSH.",
            "Empty: the AWS service of the region (the Host and Port fields are not used then), and for DynamoDB Local and Alternator the address of the Host and Port fields. Enter a URL only for another endpoint, for example a VPC one. Through an SSH tunnel the Host and Port fields name the endpoint as the SSH server reaches it.",
        )),
        required: false,
    },
    OptionField {
        key: "session_token",
        label: ("Token sesji", "Session token"),
        kind: OptionKind::Text,
        default: "",
        placeholder: "",
        help: Some((
            "Tylko dla poświadczeń tymczasowych (STS, IAM Identity Center): trzecia wartość obok klucza dostępu i klucza tajnego. Wygasa po kilku godzinach.",
            "Only for temporary credentials (STS, IAM Identity Center): the third value next to the access key and the secret key. It expires after a few hours.",
        )),
        required: false,
    },
    OptionField {
        key: "consistent_read",
        label: ("Odczyty spójne", "Consistent reads"),
        kind: OptionKind::Flag,
        default: "false",
        placeholder: "",
        help: Some((
            "Odczyt spójny zawsze widzi ostatni zapis, ale kosztuje dwa razy więcej jednostek odczytu. Bez niego dane odświeżone tuż po zmianie mogą przez chwilę pokazywać starą wartość.",
            "A consistent read always sees the latest write but costs twice the read units. Without it, data reloaded right after a change may show the old value for a moment.",
        )),
        required: false,
    },
];

/// What the fields of the dialog mean here, and what a read costs.
const KEYS: (&str, &str) = (
    "Nazwa użytkownika to identyfikator klucza dostępu (AKIA…), hasło to tajny klucz dostępu, a Region to region AWS (np. eu-central-1). Pola Host i Port nie są używane: adres wynika z regionu. Przeglądanie tabeli to operacja Scan, a każda strona czyta najwyżej 16 MB; filtr nie zmniejsza kosztu odczytu, chyba że wskazuje klucz partycji (wtedy jest to Query).",
    "The user name is the access key ID (AKIA…), the password is the secret access key and Region is the AWS region (for example eu-central-1). The Host and Port fields are not used: the address follows from the region. Browsing a table is a Scan and every page reads at most 16 MB; a filter does not make a read cheaper unless it names the partition key (then it is a Query).",
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
        default_port: 8000,
        default_tls: TlsMode::Disable,
        default_database: "us-east-1",
        ..ProductSpec::GENERIC
    }
}

static PRODUCTS: &[ProductSpec] = &[
    ProductSpec {
        default_port: 443,
        default_tls: TlsMode::VerifyFull,
        keywords: &[
            "aws", "amazon", "dynamo", "nosql", "key", "value", "document", "partiql",
        ],
        note: Some(KEYS),
        ..product(
            "amazon-dynamodb",
            "Amazon DynamoDB",
            Group::Cloud,
            "Dy",
            [64, 83, 214],
        )
    },
    ProductSpec {
        default_username: "local",
        keywords: &[
            "aws", "amazon", "dynamo", "local", "docker", "nosql", "emulator",
        ],
        note: Some((
            "Emulator DynamoDB uruchamiany lokalnie (zwykle port 8000, bez szyfrowania). Przyjmuje dowolny klucz dostępu i klucz tajny; bez opcji -sharedDb każda para klucz i region ma osobny zestaw tabel. Region i tak trzeba podać: jest częścią podpisu.",
            "The DynamoDB emulator run locally (usually port 8000, no encryption). It accepts any access key and secret key; without its -sharedDb option every pair of key and region has a set of tables of its own. A region still has to be given: it is part of the signature.",
        )),
        ..product(
            "dynamodb-local",
            "DynamoDB Local",
            Group::KeyValue,
            "Dl",
            [86, 110, 160],
        )
    },
    ProductSpec {
        keywords: &["scylla", "alternator", "dynamo", "nosql"],
        note: Some((
            "Interfejs ScyllaDB zgodny z API DynamoDB (zwykle port 8000, z szyfrowaniem 8043). Gdy autoryzacja jest włączona, nazwą użytkownika jest rola, a hasłem jej salted_hash z tabeli ról. Alternator nie zna transakcji ani PartiQL: import zapisuje wtedy wiersz po wierszu, a edytor przyjmuje operacje API zapisane jako nazwa i treść JSON. Nie sprawdzono na żywym serwerze.",
            "The interface of ScyllaDB that is compatible with the DynamoDB API (usually port 8000, 8043 with encryption). With authorization on, the user name is a role and the password its salted_hash from the roles table. Alternator knows neither transactions nor PartiQL: an import then writes row by row, and the editor takes API operations written as a name and a JSON body. Not tried against a live server.",
        )),
        ..product(
            "scylladb-alternator",
            "ScyllaDB Alternator",
            Group::KeyValue,
            "Al",
            [62, 167, 196],
        )
    },
];

/// Longest a statement of the editor, a page or an import may take.
const DEADLINE: Duration = Duration::from_secs(120);
/// Longest a change of the grid or a look at the tables may take.
const SHORT_DEADLINE: Duration = Duration::from_secs(45);
/// Items read to learn the attributes of a table: one page of the grid.
const SAMPLE_ITEMS: u32 = 500;
/// How long a sample stands in for the first page of the grid.
const SAMPLE_FRESH: Duration = Duration::from_secs(10);
/// How long what `DescribeTable` said is believed.
const INFO_FRESH: Duration = Duration::from_secs(60);
/// Most items that are read to be put in order.
const SORT_ITEMS: usize = 20_000;
/// Most pages read on the way to one nobody visited yet.
const WALK_STEPS: usize = 400;
/// Most tables the overview and the monitor describe one by one.
const DESCRIBED_TABLES: usize = 200;
const MONITOR_TABLES: usize = 25;
/// Most tables a listing asks for.
const LISTED_TABLES: usize = 5000;
/// Items of one transaction, and of one `BatchGetItem`.
const TRANSACTION_ITEMS: usize = 100;
/// Most data of one transaction (the service allows 4 MB).
const TRANSACTION_BYTES: usize = 3 * 1024 * 1024;

/// A sentence for a result code of this driver.
fn describe(code: &str, en: bool) -> Option<&'static str> {
    let (pl, english) = match code {
        "dynamodb_region_invalid" => (
            "Podaj region AWS, na przykład eu-central-1 albo us-east-1. Dla DynamoDB Local wystarczy dowolny region, np. us-east-1.",
            "Enter an AWS region, for example eu-central-1 or us-east-1. For DynamoDB Local any region will do, such as us-east-1.",
        ),
        "dynamodb_endpoint_invalid" => (
            "Adres punktu końcowego jest nieprawidłowy. Wpisz adres URL w postaci http://host:port albo https://host, bez konta, parametrów i znaków specjalnych w ścieżce.",
            "The endpoint address is not valid. Enter a URL of the form http://host:port or https://host, without an account, a query or special characters in the path.",
        ),
        "dynamodb_endpoint_tls_mismatch" => (
            "Adres URL punktu końcowego i karta SSL/TLS mówią co innego. Dla adresu http:// wybierz tryb „Wyłączony”, a dla https:// tryb z szyfrowaniem.",
            "The endpoint URL and the SSL/TLS tab disagree. For an http:// address choose the mode “Disabled”, for https:// a mode with encryption.",
        ),
        "dynamodb_credentials_required" => (
            "Podaj identyfikator klucza dostępu w polu nazwy użytkownika i tajny klucz dostępu w polu hasła. DynamoDB Local przyjmuje dowolne wartości, ale nie puste.",
            "Enter the access key ID in the user name field and the secret access key in the password field. DynamoDB Local accepts any values, but not empty ones.",
        ),
        "dynamodb_credentials_expired" => (
            "Poświadczenia tymczasowe wygasły. Pobierz nowy klucz dostępu, klucz tajny i token sesji, a potem wpisz je w ustawieniach połączenia.",
            "The temporary credentials have expired. Get a new access key, secret key and session token and enter them in the connection settings.",
        ),
        "dynamodb_clock_skew" => (
            "Usługa odrzuciła podpis żądania, bo zegar tego komputera różni się od jej zegara o ponad pięć minut. Ustaw w systemie automatyczną datę i godzinę.",
            "The service rejected the signature of the request because the clock of this computer differs from its own by more than five minutes. Set the date and time of the system to automatic.",
        ),
        "dynamodb_endpoint_not_dynamodb" => (
            "Serwer pod tym adresem nie odpowiada jak DynamoDB. Sprawdź adres i port oraz to, czy punkt końcowy używa http czy https.",
            "The server at this address does not answer like DynamoDB. Check the address and the port, and whether the endpoint uses http or https.",
        ),
        "dynamodb_table_unknown" => (
            "Nie ma takiej tabeli w tym regionie i na tym koncie (albo nie ma takiego indeksu). Sprawdź nazwę, wielkość liter i region.",
            "There is no such table in this region and account (or no such index). Check the name, its letter case and the region.",
        ),
        "dynamodb_condition_failed" => (
            "Warunek polecenia nie został spełniony, więc nic nie zostało zapisane.",
            "The condition of the statement did not hold, so nothing was written.",
        ),
        "dynamodb_item_exists" => (
            "Element o tym kluczu głównym już istnieje. Zmień istniejący element albo podaj inny klucz.",
            "An item with this primary key already exists. Change the existing item or give another key.",
        ),
        "dynamodb_throttled" => (
            "Usługa ogranicza liczbę żądań: przekroczono przepustowość tabeli albo limit konta. Spróbuj ponownie za chwilę albo zwiększ przepustowość tabeli.",
            "The service is throttling requests: the capacity of the table or a limit of the account was exceeded. Try again in a moment, or raise the capacity of the table.",
        ),
        "dynamodb_table_busy" => (
            "Tabela jest właśnie tworzona, zmieniana albo usuwana, albo tabela o tej nazwie już istnieje. Spróbuj ponownie za chwilę.",
            "The table is being created, changed or deleted right now, or a table with this name already exists. Try again in a moment.",
        ),
        "dynamodb_transaction_cancelled" => (
            "Transakcja została anulowana: jeden z jej zapisów nie spełnił warunku albo koliduje z innym zapisem. Nic nie zostało zapisane.",
            "The transaction was cancelled: one of its writes did not meet its condition or conflicts with another write. Nothing was written.",
        ),
        "dynamodb_item_too_large" => (
            "Element przekracza limit rozmiaru DynamoDB (400 KB razem z nazwami atrybutów).",
            "The item is larger than DynamoDB allows (400 KB including the names of its attributes).",
        ),
        "dynamodb_operation_unknown" => (
            "Serwer nie zna tej operacji API. Sprawdź jej nazwę (np. DescribeTable, Query, CreateTable); Alternator i DynamoDB Local nie obsługują wszystkich operacji usługi.",
            "The server does not know this API operation. Check its name (for example DescribeTable, Query, CreateTable); Alternator and DynamoDB Local do not support every operation of the service.",
        ),
        "dynamodb_statement_invalid" => (
            "Usługa nie zrozumiała polecenia PartiQL. Nazwy tabel i atrybutów pisz w \"podwójnych\" cudzysłowach, teksty w 'pojedynczych'; słowa zastrzeżone jako nazwy też wymagają cudzysłowu.",
            "The service did not understand the PartiQL statement. Write table and attribute names in \"double\" quotes and texts in 'single' ones; reserved words used as names need quotes as well.",
        ),
        "dynamodb_key_mismatch" => (
            "Klucz nie pasuje do schematu klucza tabeli: podaj wszystkie atrybuty klucza głównego (klucz partycji i, jeśli tabela go ma, klucz sortowania) w ich typach.",
            "The key does not match the key schema of the table: give every attribute of the primary key (the partition key and, when the table has one, the sort key) in its type.",
        ),
        "dynamodb_statement_needs_key" => (
            "UPDATE i DELETE w PartiQL dotyczą jednego elementu: warunek WHERE musi podawać równość dla każdego atrybutu klucza głównego.",
            "UPDATE and DELETE of PartiQL address one item: the WHERE clause has to give an equality for every attribute of the primary key.",
        ),
        "dynamodb_key_required" => (
            "Element musi mieć niepustą wartość w każdym atrybucie klucza głównego. W oknie nowego wiersza odznacz przy nich „Pomiń” i wpisz wartości.",
            "An item needs a non-empty value in every attribute of the primary key. In the new row dialog untick “Omit” next to them and enter the values.",
        ),
        "dynamodb_request_invalid" => (
            "Usługa odrzuciła żądanie jako nieprawidłowe. Sprawdź nazwy atrybutów, typy wartości i wyrażenia.",
            "The service rejected the request as invalid. Check the attribute names, the types of the values and the expressions.",
        ),
        "dynamodb_service_error" => (
            "Usługa zgłosiła błąd wewnętrzny albo jest chwilowo niedostępna. Spróbuj ponownie za chwilę.",
            "The service reported an internal error or is unavailable for the moment. Try again shortly.",
        ),
        "dynamodb_number_invalid" => (
            "Wartość nie jest liczbą. Liczba DynamoDB to cyfry z opcjonalnym znakiem, częścią ułamkową i wykładnikiem, do 38 cyfr znaczących.",
            "The value is not a number. A DynamoDB number is digits with an optional sign, fraction and exponent, up to 38 significant digits.",
        ),
        "dynamodb_value_invalid" => (
            "Wartość nie jest poprawnym zapisem typu DynamoDB. Listy i mapy wpisuj jako JSON; zbiory jako {\"SS\": [\"a\", \"b\"]} albo {\"NS\": [1, 2]}, dane binarne jako {\"B\": \"base64\"}.",
            "The value is not a valid notation of a DynamoDB type. Enter lists and maps as JSON; sets as {\"SS\": [\"a\", \"b\"]} or {\"NS\": [1, 2]}, binary data as {\"B\": \"base64\"}.",
        ),
        "dynamodb_set_empty" => (
            "Zbiór nie może być pusty: DynamoDB nie przechowuje pustych zbiorów. Usuń atrybut zamiast tego.",
            "A set cannot be empty: DynamoDB does not store empty sets. Remove the attribute instead.",
        ),
        "dynamodb_set_duplicate" => (
            "Zbiór zawiera tę samą wartość dwa razy.",
            "The set holds the same value twice.",
        ),
        "dynamodb_key_type_mismatch" => (
            "Wartość ma inny typ niż atrybut klucza (S to tekst, N liczba, B dane binarne).",
            "The value has another type than the key attribute (S is text, N a number, B binary data).",
        ),
        "dynamodb_key_read_only" => (
            "Atrybutu klucza głównego nie da się zmienić: wyznacza on miejsce elementu. Dodaj element z nowym kluczem i usuń stary.",
            "An attribute of the primary key cannot be changed: it says where the item lives. Add an item with the new key and delete the old one.",
        ),
        "dynamodb_scan_limit" => (
            "Filtr przeczytał 16 MB tabeli i nie znalazł tekstu, a tabela ma więcej danych. DynamoDB nie wyszukuje tekstu, więc filtr czyta elementy po kolei i każdy odczyt kosztuje. Przejdź do następnej strony, aby szukać dalej, albo filtruj po kluczu partycji: taki filtr to Query i trafia wprost do elementów.",
            "The filter read 16 MB of the table and did not find the text, and the table has more. DynamoDB does not search for text, so a filter reads items one after another and every read costs. Go to the next page to keep looking, or filter by the partition key: such a filter is a Query and goes straight to the items.",
        ),
        "dynamodb_sort_limit" => (
            "DynamoDB porządkuje elementy tylko według klucza sortowania w jednej partycji, więc siatka sortuje sama: najwyżej 20 000 elementów i 16 MB danych. Zawęź dane filtrem, na przykład kluczem partycji, i posortuj ponownie.",
            "DynamoDB orders items only by the sort key inside one partition, so the grid sorts by itself: at most 20,000 items and 16 MB of data. Narrow the data with a filter, for example a partition key, and sort again.",
        ),
        "dynamodb_page_too_deep" => (
            "DynamoDB nie pozwala przeskoczyć do dowolnej strony: trzeba przeczytać wszystkie wcześniejsze. Ta strona jest zbyt daleko. Wróć na początek i zawęź dane filtrem.",
            "DynamoDB cannot jump to an arbitrary page: every page before it has to be read. This page is too far away. Go back to the start and narrow the data with a filter.",
        ),
        "dynamodb_read_only_object" => (
            "To jest zestawienie tabel, a nie tabela: jego wierszy nie zmienia się wprost.",
            "This is the overview of the tables, not a table: its rows are not changed directly.",
        ),
        "dynamodb_import_key_missing" => (
            "Plik musi zawierać wszystkie atrybuty klucza głównego tabeli (klucz partycji i, jeśli tabela go ma, klucz sortowania).",
            "The file has to contain every attribute of the primary key of the table (the partition key and, when the table has one, the sort key).",
        ),
        "dynamodb_import_key_exists" => (
            "Co najmniej jeden element pliku ma klucz, który już istnieje w tabeli. PutItem nadpisałby te elementy, więc nic nie zostało zapisane. Usuń je z pliku albo z tabeli.",
            "At least one item of the file has a key that already exists in the table. PutItem would overwrite those items, so nothing was written. Remove them from the file or from the table.",
        ),
        "dynamodb_import_duplicate" => (
            "Dwa wiersze pliku mają ten sam klucz główny. Nic nie zostało zapisane.",
            "Two rows of the file have the same primary key. Nothing was written.",
        ),
        "dynamodb_import_incomplete" => (
            "Usługa odrzuciła część importu już w trakcie zapisu. Jedna transakcja DynamoDB obejmuje najwyżej 100 elementów, więc wcześniejsze setki zostały zapisane: odśwież tabelę i sprawdź dane.",
            "The service refused a part of the import while it was being written. One DynamoDB transaction holds at most 100 items, so the hundreds before it were written: reload the table and check the data.",
        ),
        "dynamodb_document_invalid" => (
            "Dokument po słowie VALUE musi być obiektem JSON z atrybutami klucza głównego tabeli, na przykład INSERT INTO \"tabela\" VALUE {\"id\": 1, \"nazwa\": \"x\"}.",
            "The document after VALUE has to be a JSON object with the primary key attributes of the table, for example INSERT INTO \"table\" VALUE {\"id\": 1, \"name\": \"x\"}.",
        ),
        _ => return None,
    };
    Some(if en { english } else { pl })
}

/// Every code this driver can answer with that has a sentence here.
#[cfg(test)]
const CODES: &[&str] = &[
    "dynamodb_region_invalid",
    "dynamodb_endpoint_invalid",
    "dynamodb_endpoint_tls_mismatch",
    "dynamodb_credentials_required",
    "dynamodb_credentials_expired",
    "dynamodb_clock_skew",
    "dynamodb_endpoint_not_dynamodb",
    "dynamodb_table_unknown",
    "dynamodb_condition_failed",
    "dynamodb_item_exists",
    "dynamodb_throttled",
    "dynamodb_table_busy",
    "dynamodb_transaction_cancelled",
    "dynamodb_item_too_large",
    "dynamodb_operation_unknown",
    "dynamodb_statement_invalid",
    "dynamodb_key_mismatch",
    "dynamodb_statement_needs_key",
    "dynamodb_key_required",
    "dynamodb_request_invalid",
    "dynamodb_service_error",
    "dynamodb_number_invalid",
    "dynamodb_value_invalid",
    "dynamodb_set_empty",
    "dynamodb_set_duplicate",
    "dynamodb_key_type_mismatch",
    "dynamodb_key_read_only",
    "dynamodb_scan_limit",
    "dynamodb_sort_limit",
    "dynamodb_page_too_deep",
    "dynamodb_read_only_object",
    "dynamodb_import_key_missing",
    "dynamodb_import_key_exists",
    "dynamodb_import_duplicate",
    "dynamodb_import_incomplete",
    "dynamodb_document_invalid",
];

/// The first items of a table, kept for a moment after they were read to
/// learn its attributes: the grid asks for the same page right after.
struct Sample {
    at: Instant,
    items: Vec<Map<String, Value>>,
    next: Mark,
}

/// What is known of a table.
struct Known {
    info: Arc<TableInfo>,
    attributes: Attributes,
    /// Items were looked at to learn the attributes.
    sampled: bool,
    sample: Option<Sample>,
}

/// A change of one attribute as typed values: the key of the item, what
/// the attribute held and what it is to hold.
type UpdateParts = (Map<String, Value>, Option<Value>, Option<Value>);

/// What reading items from a mark gave.
struct Read {
    items: Vec<Map<String, Value>>,
    /// Where the items after these start.
    next: Mark,
    /// The reading stopped at the amount one page may read.
    capped: bool,
}

struct DynamoSession {
    client: Client,
    consistent: bool,
    known: Mutex<HashMap<String, Known>>,
    /// The tables of the last listing.
    names: Mutex<Vec<String>>,
    cursors: tokio::sync::Mutex<Cursors>,
}

fn connect<'a>(request: &'a ConnectRequest<'a>) -> BoxFuture<'a, Result<Arc<dyn Session>>> {
    Box::pin(async move {
        let session = DynamoSession {
            client: Client::new(request)?,
            consistent: request.option("consistent_read") == "true",
            known: Mutex::new(HashMap::new()),
            names: Mutex::new(Vec::new()),
            cursors: tokio::sync::Mutex::new(Cursors::default()),
        };
        // One small call says whether the endpoint and the keys are right.
        session
            .briefly(session.call("ListTables", json!({ "Limit": 1 })))
            .await?;
        Ok(Arc::new(session) as Arc<dyn Session>)
    })
}

/// How a run ended, before its futures are gone.
enum Ended<T> {
    Done(T),
    Cancelled,
    TimedOut,
}

/// The objects of an array of a reply.
fn objects(value: &Value) -> Vec<Map<String, Value>> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item.as_object().cloned())
        .collect()
}

/// Split the text of the editor into statements at the `;` that stand
/// outside quotes, comments and the braces of a JSON body.
fn statements(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut depth, mut i) = (0, 0usize, 0);
    while i < bytes.len() {
        match bytes[i] {
            quote @ (b'\'' | b'"') => {
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    // JSON escapes a quote with a backslash.
                    if bytes[i] == b'\\' && quote == b'"' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                while i < bytes.len() && !bytes[i..].starts_with(b"*/") {
                    i += 1;
                }
                i += 1;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth = depth.saturating_sub(1),
            b';' if depth == 0 => {
                out.push(text[start..i].to_owned());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    out.push(text[start.min(text.len())..].to_owned());
    out.into_iter()
        .map(|statement| partiql::without_comments(&statement))
        .filter(|statement| !statement.is_empty())
        .collect()
}

/// An operation of the API written out: its name and its JSON body, as the
/// AWS documentation gives them (`DescribeTable {"TableName": "Music"}`).
fn api_call(statement: &str) -> Option<(String, Value)> {
    let statement = statement.trim();
    let end = statement.find(|c: char| c.is_whitespace() || c == '{')?;
    let (name, rest) = (&statement[..end], statement[end..].trim());
    let known = name.len() >= 4
        && name.starts_with(|c: char| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_alphanumeric())
        && name.chars().any(|c| c.is_ascii_lowercase());
    if !known || !rest.starts_with('{') {
        return None;
    }
    match serde_json::from_str::<Value>(rest).ok()? {
        body @ Value::Object(_) => Some((name.to_owned(), body)),
        _ => None,
    }
}

impl DynamoSession {
    async fn call(&self, operation: &str, body: Value) -> Result<Value> {
        Ok(self.client.call(operation, &body).await?)
    }

    async fn bounded<T>(
        &self,
        cancel: CancellationToken,
        work: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        if cancel.is_cancelled() {
            bail!("query_cancelled");
        }
        // A call that is dropped closes its connection: the service stops
        // serving it, and no further call is made.
        let ended = tokio::select! {
            biased;
            _ = cancel.cancelled() => Ended::Cancelled,
            _ = tokio::time::sleep(DEADLINE) => Ended::TimedOut,
            result = work => Ended::Done(result),
        };
        match ended {
            Ended::Done(result) => result,
            Ended::Cancelled => bail!("query_cancelled"),
            Ended::TimedOut => bail!("query_timeout"),
        }
    }

    async fn briefly<T>(&self, work: impl Future<Output = Result<T>>) -> Result<T> {
        match tokio::time::timeout(SHORT_DEADLINE, work).await {
            Ok(result) => result,
            Err(_) => bail!("database_connection_timeout"),
        }
    }

    /// The names of the tables of the account in this region.
    async fn list(&self) -> Result<Vec<String>> {
        let mut names: Vec<String> = Vec::new();
        let mut start: Option<String> = None;
        loop {
            let mut body = json!({ "Limit": 100 });
            if let Some(start) = &start {
                body["ExclusiveStartTableName"] = json!(start);
            }
            let reply = self.call("ListTables", body).await?;
            names.extend(
                reply["TableNames"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|name| name.as_str().map(str::to_owned)),
            );
            start = reply["LastEvaluatedTableName"].as_str().map(str::to_owned);
            if start.is_none() || names.len() >= LISTED_TABLES {
                break;
            }
        }
        names.sort();
        if let Ok(mut known) = self.names.lock() {
            known.clone_from(&names);
        }
        Ok(names)
    }

    /// What `DescribeTable` says of a table, asked again when it is older
    /// than `fresh`.
    async fn info(&self, name: &str, fresh: Duration) -> Result<Arc<TableInfo>> {
        if let Some(info) = self
            .known
            .lock()
            .ok()
            .and_then(|known| known.get(name).map(|known| known.info.clone()))
            .filter(|info| info.read_at.elapsed() < fresh)
        {
            return Ok(info);
        }
        let reply = self
            .call("DescribeTable", json!({ "TableName": name }))
            .await?;
        let info = Arc::new(TableInfo::of(&reply["Table"])?);
        if let Ok(mut known) = self.known.lock() {
            match known.get_mut(name) {
                Some(known) => known.info = info.clone(),
                None => {
                    known.insert(
                        name.to_owned(),
                        Known {
                            info: info.clone(),
                            attributes: Attributes::new(),
                            sampled: false,
                            sample: None,
                        },
                    );
                }
            }
        }
        Ok(info)
    }

    /// Note the attributes items of a table turned out to have.
    fn note(&self, table: &str, items: &[Map<String, Value>]) {
        if let Ok(mut known) = self.known.lock()
            && let Some(known) = known.get_mut(table)
        {
            browse::note(&mut known.attributes, items);
        }
    }

    fn attributes(&self, table: &str) -> Attributes {
        self.known
            .lock()
            .ok()
            .and_then(|known| known.get(table).map(|known| known.attributes.clone()))
            .unwrap_or_default()
    }

    fn remembered(&self, table: &str) -> Option<Arc<TableInfo>> {
        self.known
            .lock()
            .ok()?
            .get(table)
            .map(|known| known.info.clone())
    }

    /// A table with its attributes: described, and its first items looked
    /// at when nobody has yet.
    async fn learned(&self, name: &str) -> Result<Arc<TableInfo>> {
        let info = self.info(name, INFO_FRESH * 5).await?;
        let sampled = self
            .known
            .lock()
            .ok()
            .and_then(|known| known.get(name).map(|known| known.sampled))
            .unwrap_or(false);
        if sampled {
            return Ok(info);
        }
        let reply = self
            .call(
                "Scan",
                json!({
                    "TableName": name,
                    "Limit": SAMPLE_ITEMS,
                    "ConsistentRead": self.consistent,
                }),
            )
            .await?;
        let items = objects(&reply["Items"]);
        let next = match reply.get("LastEvaluatedKey").filter(|key| key.is_object()) {
            Some(key) => Mark::At(Some(key.clone())),
            None => Mark::End,
        };
        if let Ok(mut known) = self.known.lock()
            && let Some(known) = known.get_mut(name)
        {
            browse::note(&mut known.attributes, &items);
            known.sampled = true;
            known.sample = Some(Sample {
                at: Instant::now(),
                items,
                next,
            });
        }
        Ok(info)
    }

    /// The sample of a table when it is the page that is asked for: the
    /// first page of the whole table, of the size of the sample, right
    /// after the sample was read. It is used once.
    fn sample_for(&self, request: &PageRequest) -> Option<Sample> {
        if request.offset != 0
            || request.limit != SAMPLE_ITEMS
            || !request.filters.is_empty()
            || request.order_by.is_some()
        {
            return None;
        }
        self.known
            .lock()
            .ok()?
            .get_mut(&request.container.name)?
            .sample
            .take()
            .filter(|sample| sample.at.elapsed() < SAMPLE_FRESH)
    }

    /// Drop the samples that stand in for a first page: something was
    /// written, or another page was asked for.
    fn drop_samples(&self) {
        if let Ok(mut known) = self.known.lock() {
            for known in known.values_mut() {
                known.sample = None;
            }
        }
    }

    /// Something was written: what was read before no longer stands.
    async fn written(&self) {
        self.drop_samples();
        self.cursors.lock().await.clear();
    }

    /// Read items from a mark until `want` of them pass the tests, the
    /// table ends, or the page has made its calls.
    async fn read(
        &self,
        table: &TableInfo,
        mode: Mode,
        plan: &Plan,
        from: &Mark,
        want: usize,
        forward: bool,
    ) -> Result<Read> {
        let mut read = Read {
            items: Vec::new(),
            next: Mark::End,
            capped: false,
        };
        let Mark::At(start) = from else {
            return Ok(read);
        };
        let mut start = start.clone();
        let exact = plan.exact.as_ref().filter(|_| mode == Mode::Query);
        let tests: &[Test] = if exact.is_some() {
            &plan.exact_tests
        } else {
            &plan.tests
        };
        let key_names: Vec<&str> = table.keys.iter().map(|key| key.name.as_str()).collect();
        for _ in 0..PAGE_CALLS {
            let mut body = json!({
                "TableName": table.name,
                "ConsistentRead": self.consistent,
            });
            let mut names = Map::new();
            let mut values = Map::new();
            if let Some(key) = exact {
                body["KeyConditionExpression"] = json!("#k = :k");
                body["ScanIndexForward"] = json!(forward);
                names.insert("#k".into(), json!(table.partition().name));
                values.insert(":k".into(), key.clone());
            }
            // A Query may not filter on its own key attributes.
            let skip: &[&str] = if exact.is_some() { &key_names } else { &[] };
            if let Some((expression, more_names, more_values)) = browse::narrowing(tests, skip) {
                body["FilterExpression"] = json!(expression);
                names.extend(more_names.as_object().cloned().unwrap_or_default());
                values.extend(more_values.as_object().cloned().unwrap_or_default());
            }
            if !names.is_empty() {
                body["ExpressionAttributeNames"] = Value::Object(names);
            }
            if !values.is_empty() {
                body["ExpressionAttributeValues"] = Value::Object(values);
            }
            if tests.is_empty() {
                body["Limit"] = json!(want - read.items.len());
            }
            if let Some(key) = &start {
                body["ExclusiveStartKey"] = key.clone();
            }
            let reply = self
                .call(if exact.is_some() { "Query" } else { "Scan" }, body)
                .await?;
            let batch = objects(&reply["Items"]);
            self.note(&table.name, &batch);
            let last = reply
                .get("LastEvaluatedKey")
                .filter(|key| key.is_object())
                .cloned();
            let count = batch.len();
            for (index, item) in batch.into_iter().enumerate() {
                if !tests.iter().all(|test| test.passes(&item)) {
                    continue;
                }
                let key = table.key_of(&item);
                read.items.push(item);
                if read.items.len() == want {
                    // The next page starts after this item: with the rest
                    // of this answer, or with the next one.
                    read.next = match (index + 1 < count, key, last) {
                        (true, Some(key), _) => Mark::At(Some(key)),
                        (_, _, Some(last)) => Mark::At(Some(last)),
                        _ => Mark::End,
                    };
                    return Ok(read);
                }
            }
            match last {
                None => return Ok(read),
                Some(last) => start = Some(last),
            }
        }
        read.next = Mark::At(start);
        read.capped = true;
        Ok(read)
    }

    async fn page(&self, request: &PageRequest) -> Result<QueryResult> {
        let started = Instant::now();
        if request.container.name == OVERVIEW {
            return self.overview(request, started).await;
        }
        let name = request.container.name.as_str();
        let table = self.learned(name).await?;
        let plan = Plan::new(&table, request)?;
        let limit = request.limit as usize;
        let mut result = QueryResult::default();
        let items = if let Some(sample) = self.sample_for(request) {
            let mut cursors = self.cursors.lock().await;
            let cursor = cursors.of(&browse::identity(request), true);
            cursor.mode = Some(Mode::Scan);
            cursor.mark(u64::from(request.limit), sample.next);
            sample.items
        } else {
            self.drop_samples();
            let mut cursors = self.cursors.lock().await;
            let cursor = cursors.of(&browse::identity(request), request.offset == 0);
            if cursor.mode.is_none() {
                cursor.mode = Some(match &plan.exact {
                    Some(_) if plan.exact_only => Mode::Query,
                    // One item tells whether the key that was typed exists.
                    Some(_) => {
                        let probe = Plan {
                            exact_tests: Vec::new(),
                            ..plan.clone()
                        };
                        let found = self
                            .read(&table, Mode::Query, &probe, &Mark::At(None), 1, true)
                            .await?;
                        if found.items.is_empty() {
                            Mode::Scan
                        } else {
                            Mode::Query
                        }
                    }
                    None => Mode::Scan,
                });
            }
            let mode = cursor.mode.unwrap_or(Mode::Scan);
            // The service orders the items of one partition by its sort
            // key; any other order is made here.
            let by_sort_key = mode == Mode::Query
                && plan.order.as_ref().is_some_and(|(column, _)| {
                    table.sort().is_some_and(|sort| sort.name == *column)
                });
            let forward = !(by_sort_key && plan.order.as_ref().is_some_and(|order| order.1));
            match &plan.order {
                Some((column, descending)) if !by_sort_key => {
                    let sorted = match &cursor.sorted {
                        Some(sorted) => sorted.clone(),
                        None => {
                            let read = self
                                .read(&table, mode, &plan, &Mark::At(None), SORT_ITEMS + 1, true)
                                .await?;
                            ensure!(
                                !read.capped && read.items.len() <= SORT_ITEMS,
                                "dynamodb_sort_limit"
                            );
                            let mut items = read.items;
                            let cell = |item: &Map<String, Value>| {
                                item.get(column).map_or(Cell::Null, value::cell)
                            };
                            items.sort_by(|left, right| {
                                let order = browse::compare(&cell(left), &cell(right));
                                if *descending { order.reverse() } else { order }
                            });
                            let sorted = Arc::new(items);
                            cursor.sorted = Some(sorted.clone());
                            sorted
                        }
                    };
                    sorted
                        .iter()
                        .skip(usize::try_from(request.offset).unwrap_or(usize::MAX))
                        .take(limit)
                        .cloned()
                        .collect()
                }
                _ => {
                    let (mut at, mut mark) = cursor.nearest(request.offset);
                    // A page nobody visited yet is reached by reading up to it.
                    let mut steps = 0;
                    while at < request.offset && mark != Mark::End {
                        ensure!(steps < WALK_STEPS, "dynamodb_page_too_deep");
                        steps += 1;
                        let step = (request.offset - at).min(u64::from(request.limit));
                        let read = self
                            .read(&table, mode, &plan, &mark, step as usize, forward)
                            .await?;
                        at += step;
                        mark = read.next;
                        cursor.mark(at, mark.clone());
                    }
                    let read = self
                        .read(&table, mode, &plan, &mark, limit, forward)
                        .await?;
                    cursor.mark(request.offset + u64::from(request.limit), read.next);
                    // Nothing found in what one page may read, and more to
                    // read: said in words; the next page goes on from there.
                    ensure!(
                        !(read.capped && read.items.is_empty()),
                        "dynamodb_scan_limit"
                    );
                    result.truncated = read.capped;
                    read.items
                }
            }
        };
        self.note(name, &items);
        result.columns = browse::result_columns(&table, &self.attributes(name));
        result.rows = items
            .iter()
            .map(|item| browse::row(&result.columns, item))
            .collect();
        result.duration_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }

    /// The tables of the account with what `DescribeTable` says of each.
    async fn described(&self, most: usize) -> Result<(usize, Vec<Arc<TableInfo>>)> {
        let names = self.list().await?;
        let total = names.len();
        let mut tables: Vec<Arc<TableInfo>> = futures_util::stream::iter(
            names
                .into_iter()
                .take(most)
                .map(|name| async move { self.info(&name, INFO_FRESH).await }),
        )
        .buffer_unordered(6)
        .filter_map(|info| async move { info.ok() })
        .collect()
        .await;
        tables.sort_by(|a, b| a.name.cmp(&b.name));
        Ok((total, tables))
    }

    const OVERVIEW_COLUMNS: [(&'static str, &'static str); 10] = [
        ("table", "string"),
        ("status", "string"),
        ("billing", "string"),
        ("items", "number"),
        ("size in bytes", "number"),
        ("partition key", "string"),
        ("sort key", "string"),
        ("global indexes", "string"),
        ("local indexes", "string"),
        ("created", "string"),
    ];

    async fn overview(&self, request: &PageRequest, started: Instant) -> Result<QueryResult> {
        let (_, tables) = self.described(DESCRIBED_TABLES).await?;
        let key = |key: Option<&browse::Key>| match key {
            Some(key) => Cell::Text(format!("{} ({})", key.name, browse::word(&key.kind))),
            None => Cell::Null,
        };
        let mut rows: Vec<Vec<Cell>> = tables
            .iter()
            .map(|table| {
                vec![
                    Cell::Text(table.name.clone()),
                    Cell::Text(table.status.clone()),
                    Cell::Text(table.billing.clone()),
                    Cell::Decimal(table.items.to_string()),
                    Cell::Decimal(table.bytes.to_string()),
                    key(Some(table.partition())),
                    key(table.sort()),
                    Cell::Text(table.global_indexes.join(", ")),
                    Cell::Text(table.local_indexes.join(", ")),
                    Cell::Text(table.created.clone()),
                ]
            })
            .collect();
        let at = |name: &str| {
            Self::OVERVIEW_COLUMNS
                .iter()
                .position(|(column, _)| *column == name)
                .ok_or_else(|| anyhow!("dynamodb_request_invalid"))
        };
        for filter in &request.filters {
            let (column, test) = (at(&filter.column)?, Test::new(filter));
            rows.retain(|row| test.passes_cell(&row[column]));
        }
        if let Some((column, descending)) = &request.order_by {
            let column = at(column)?;
            rows.sort_by(|left, right| {
                let order = browse::compare(&left[column], &right[column]);
                if *descending { order.reverse() } else { order }
            });
        }
        Ok(QueryResult {
            columns: Self::OVERVIEW_COLUMNS
                .iter()
                .map(|(name, kind)| ResultColumn {
                    name: (*name).into(),
                    data_type: (*kind).into(),
                })
                .collect(),
            rows: rows
                .into_iter()
                .skip(usize::try_from(request.offset).unwrap_or(usize::MAX))
                .take(request.limit as usize)
                .collect(),
            duration_ms: started.elapsed().as_millis() as u64,
            ..QueryResult::default()
        })
    }

    /// The items of a result as a grid: the columns of the table when the
    /// statement is a plain `SELECT *` of one, so that its rows can be
    /// edited; otherwise the attributes in the order they were asked for.
    async fn grid(
        &self,
        statement: &str,
        items: Vec<Map<String, Value>>,
        named: Option<&str>,
    ) -> Result<QueryResult> {
        let tables: Vec<TableRef> = self
            .names
            .lock()
            .map(|names| {
                names
                    .iter()
                    .map(|name| TableRef {
                        schema: String::new(),
                        name: name.clone(),
                        kind: "table".into(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let columns = match crate::sql::editable_table(&DIALECT, statement, &tables) {
            Some(table) => {
                let info = self.learned(&table.name).await?;
                self.note(&table.name, &items);
                browse::result_columns(&info, &self.attributes(&table.name))
            }
            None => {
                let mut seen = Attributes::new();
                browse::note(&mut seen, &items);
                let mut names = partiql::projection(statement);
                // Without an order that was asked for, the key of the
                // table comes first, as in its own grid.
                if names.is_empty()
                    && let Some(table) = named.and_then(|name| self.remembered(name))
                {
                    names = table.keys.iter().map(|key| key.name.clone()).collect();
                }
                names.retain(|name| seen.contains_key(name));
                let rest: Vec<String> = seen
                    .keys()
                    .filter(|name| !names.contains(name))
                    .cloned()
                    .collect();
                names.extend(rest);
                names
                    .into_iter()
                    .map(|name| ResultColumn {
                        data_type: browse::words(seen.get(&name)),
                        name,
                    })
                    .collect()
            }
        };
        Ok(QueryResult {
            rows: items
                .iter()
                .map(|item| browse::row(&columns, item))
                .collect(),
            columns,
            ..QueryResult::default()
        })
    }

    /// The answer of an operation that is not a list of items: one row with
    /// what the service said, each part as JSON.
    fn answer(reply: &Value) -> QueryResult {
        let Some(parts) = reply.as_object().filter(|parts| !parts.is_empty()) else {
            return QueryResult::default();
        };
        QueryResult {
            columns: parts
                .keys()
                .map(|name| ResultColumn {
                    name: name.clone(),
                    data_type: "json".into(),
                })
                .collect(),
            rows: vec![
                parts
                    .values()
                    .map(|value| match value {
                        Value::String(text) => Cell::Text(text.clone()),
                        Value::Bool(flag) => Cell::Bool(*flag),
                        Value::Number(number) => Cell::Decimal(number.to_string()),
                        Value::Null => Cell::Null,
                        other => Cell::Json(other.to_string()),
                    })
                    .collect(),
            ],
            ..QueryResult::default()
        }
    }

    /// Put one item, unless its key is taken.
    async fn put(&self, table: &TableInfo, item: Map<String, Value>) -> Result<()> {
        for key in &table.keys {
            ensure!(
                item.get(&key.name)
                    .is_some_and(|value| value::type_of(value) == Some(key.kind.as_str())),
                "dynamodb_key_mismatch"
            );
        }
        let put = self
            .client
            .call(
                "PutItem",
                &json!({
                    "TableName": table.name,
                    "Item": item,
                    "ConditionExpression": "attribute_not_exists(#k)",
                    "ExpressionAttributeNames": { "#k": table.partition().name },
                }),
            )
            .await;
        match put {
            Ok(_) => Ok(()),
            Err(fail) if fail.is_condition() => bail!("dynamodb_item_exists"),
            Err(fail) => Err(fail.into()),
        }
    }

    /// Run one statement of the editor.
    async fn run_statement(&self, statement: &str) -> Result<QueryResult> {
        if partiql::is_probe(statement) {
            self.call("ListTables", json!({ "Limit": 1 })).await?;
            return Ok(QueryResult {
                columns: vec![ResultColumn {
                    name: "1".into(),
                    data_type: "number".into(),
                }],
                rows: vec![vec![Cell::Decimal("1".into())]],
                ..QueryResult::default()
            });
        }
        // A JSON document after VALUE: an item to put.
        if let Some((name, document)) = partiql::json_insert(statement) {
            let table = self.info(&name, INFO_FRESH).await?;
            let item = document
                .iter()
                .map(|(attribute, value)| Ok((attribute.clone(), value::typed(value)?)))
                .collect::<Result<Map<String, Value>>>()
                .map_err(|_| anyhow!("dynamodb_document_invalid"))?;
            ensure!(
                table.keys.iter().all(|key| item.contains_key(&key.name)),
                "dynamodb_document_invalid"
            );
            self.put(&table, item).await?;
            self.written().await;
            return Ok(QueryResult {
                affected: 1,
                ..QueryResult::default()
            });
        }
        // An operation of the API by its name, with its JSON body.
        if let Some((operation, body)) = api_call(statement) {
            let table = body["TableName"].as_str().map(str::to_owned);
            let reply = self.call(&operation, body).await?;
            self.cursors.lock().await.clear();
            if let Ok(mut known) = self.known.lock() {
                // A table may have been made, changed or removed.
                known.clear();
            }
            return match reply.get("Items").filter(|items| items.is_array()) {
                Some(items) => {
                    let table = table.as_deref();
                    if let Some(name) = table {
                        // Its key, for the order of the columns.
                        let _ = self.info(name, INFO_FRESH).await;
                    }
                    let mut result = self.grid("", objects(items), table).await?;
                    // More to read: the caller goes on with the key the
                    // service names.
                    result.truncated = reply.get("LastEvaluatedKey").is_some();
                    Ok(result)
                }
                None => Ok(Self::answer(&reply)),
            };
        }
        // PartiQL. A read goes on with the continuation of the service
        // until the result is whole or the amount one statement reads is
        // reached.
        let select = partiql::leading_words(statement, 1)
            .first()
            .map(String::as_str)
            == Some("SELECT");
        let mut items = Vec::new();
        let mut next: Option<String> = None;
        let mut truncated = false;
        for call in 0.. {
            let mut body = json!({ "Statement": statement });
            if select && self.consistent {
                body["ConsistentRead"] = json!(true);
            }
            if let Some(next) = &next {
                body["NextToken"] = json!(next);
            }
            let reply = self.call("ExecuteStatement", body).await?;
            items.extend(objects(&reply["Items"]));
            next = reply["NextToken"].as_str().map(str::to_owned);
            if next.is_none() {
                break;
            }
            if items.len() >= MAX_RESULT_ROWS || call + 1 >= PAGE_CALLS {
                truncated = true;
                break;
            }
        }
        if !select {
            self.written().await;
            if items.is_empty() {
                return Ok(QueryResult::default());
            }
        }
        if items.len() > MAX_RESULT_ROWS {
            items.truncate(MAX_RESULT_ROWS);
            truncated = true;
        }
        let mut result = self.grid(statement, items, None).await?;
        result.truncated = truncated;
        Ok(result)
    }

    async fn run_script(&self, text: &str) -> Result<QueryResult> {
        let started = Instant::now();
        let mut last = QueryResult::default();
        for statement in statements(text) {
            last = self.run_statement(&statement).await?;
        }
        last.duration_ms = started.elapsed().as_millis() as u64;
        Ok(last)
    }

    /// The typed values of a change of one attribute: the key, what the
    /// attribute held and what it is to hold.
    fn update_parts(
        &self,
        table: &TableInfo,
        column: &str,
        old: &Cell,
        new: &Cell,
        key: &[(String, Cell)],
    ) -> Result<UpdateParts> {
        ensure!(table.key(column).is_none(), "dynamodb_key_read_only");
        let key = table.key_from(key)?;
        // What a cell shows says what the attribute holds: a text is S, a
        // number N. Only a value typed where there was none is read by
        // what other items hold in that attribute.
        let seen = self
            .attributes(&table.name)
            .remove(column)
            .unwrap_or_default();
        let shown = |cell: &Cell| -> Result<Option<Value>> {
            Ok(match cell {
                Cell::Null => None,
                Cell::Text(text) => Some(json!({ "S": text })),
                other => value::attribute(other, Hint::Seen(&BTreeSet::new()))?,
            })
        };
        let old_value = shown(old)?;
        let new_value = match (old, new) {
            (Cell::Null, new @ Cell::Text(_)) => value::attribute(new, Hint::Seen(&seen))?,
            (_, new) => shown(new)?,
        };
        Ok((key, old_value, new_value))
    }

    /// The item a new row stands for.
    fn new_item(&self, table: &TableInfo, values: &[(String, Cell)]) -> Result<Map<String, Value>> {
        let attributes = self.attributes(&table.name);
        let none = BTreeSet::new();
        let mut item = Map::new();
        for (name, cell) in values {
            ensure!(!name.is_empty(), "dynamodb_request_invalid");
            let hint = match table.key(name) {
                Some(key) => Hint::Key(&key.kind),
                None => Hint::Seen(attributes.get(name).unwrap_or(&none)),
            };
            if let Some(value) = value::attribute(cell, hint)? {
                item.insert(name.clone(), value);
            }
        }
        ensure!(
            table.keys.iter().all(|key| item.contains_key(&key.name)),
            "dynamodb_key_required"
        );
        Ok(item)
    }

    async fn change(&self, change: &RowChange) -> Result<()> {
        let name = &table_of(change).name;
        ensure!(name != OVERVIEW, "dynamodb_read_only_object");
        let table = self.info(name, INFO_FRESH * 5).await?;
        let partition = table.partition().name.clone();
        let outcome = match change {
            RowChange::UpdateCell {
                column,
                old,
                new,
                key,
                ..
            } => {
                let (key, old, new) = self.update_parts(&table, column, old, new, key)?;
                let mut names = json!({ "#k": partition, "#c": column });
                let mut values = Map::new();
                // The item has to be there, and the attribute has to hold
                // what the grid showed.
                let condition = match old {
                    Some(old) => {
                        values.insert(":old".into(), old);
                        "attribute_exists(#k) AND #c = :old"
                    }
                    None => "attribute_exists(#k) AND attribute_not_exists(#c)",
                };
                let update = match new {
                    Some(new) => {
                        values.insert(":new".into(), new);
                        "SET #c = :new"
                    }
                    None => "REMOVE #c",
                };
                let mut body = json!({
                    "TableName": table.name,
                    "Key": key,
                    "UpdateExpression": update,
                    "ConditionExpression": condition,
                });
                body["ExpressionAttributeNames"] = names.take();
                if !values.is_empty() {
                    body["ExpressionAttributeValues"] = Value::Object(values);
                }
                self.client.call("UpdateItem", &body).await
            }
            RowChange::InsertRow { values, .. } => {
                self.drop_samples();
                return self.put(&table, self.new_item(&table, values)?).await;
            }
            RowChange::DeleteRow { key, .. } => {
                self.client
                    .call(
                        "DeleteItem",
                        &json!({
                            "TableName": table.name,
                            "Key": table.key_from(key)?,
                            "ConditionExpression": "attribute_exists(#k)",
                            "ExpressionAttributeNames": { "#k": partition },
                        }),
                    )
                    .await
            }
        };
        self.drop_samples();
        match outcome {
            Ok(_) => Ok(()),
            // The item is gone, or the attribute holds something else.
            Err(fail) if fail.is_condition() => bail!("row_changed_reload_required"),
            Err(fail) => Err(fail.into()),
        }
    }

    /// Whether any of these keys is the key of an item.
    async fn any_exists(&self, table: &TableInfo, keys: &[Value]) -> Result<bool> {
        let mut names = Map::new();
        for (index, key) in table.keys.iter().enumerate() {
            names.insert(format!("#k{index}"), json!(key.name));
        }
        let projection = names.keys().cloned().collect::<Vec<_>>().join(", ");
        let mut pending: Vec<Value> = keys.to_vec();
        // The service may answer a part and hand the rest back.
        for _ in 0..20 {
            if pending.is_empty() {
                return Ok(false);
            }
            let reply = self
                .call(
                    "BatchGetItem",
                    json!({ "RequestItems": { table.name.as_str(): {
                        "Keys": pending,
                        "ProjectionExpression": projection,
                        "ExpressionAttributeNames": names,
                        "ConsistentRead": true,
                    }}}),
                )
                .await?;
            if reply["Responses"][table.name.as_str()]
                .as_array()
                .is_some_and(|found| !found.is_empty())
            {
                return Ok(true);
            }
            pending = reply["UnprocessedKeys"][table.name.as_str()]["Keys"]
                .as_array()
                .cloned()
                .unwrap_or_default();
        }
        bail!("dynamodb_throttled")
    }

    async fn import(
        &self,
        container: &TableRef,
        columns: &[String],
        rows: &[Vec<Cell>],
        cancel: CancellationToken,
    ) -> Result<QueryResult> {
        let started = Instant::now();
        ensure!(container.name != OVERVIEW, "dynamodb_read_only_object");
        let table = self.learned(&container.name).await?;
        for key in &table.keys {
            ensure!(columns.contains(&key.name), "dynamodb_import_key_missing");
        }
        // Every row becomes an item first: a value that cannot be written
        // stops the import before anything is sent.
        let mut items = Vec::with_capacity(rows.len());
        let mut keys = Vec::with_capacity(rows.len());
        let mut seen = HashSet::new();
        for row in rows {
            ensure!(row.len() == columns.len(), "import_shape_invalid");
            let values: Vec<(String, Cell)> =
                columns.iter().cloned().zip(row.iter().cloned()).collect();
            let item = self.new_item(&table, &values)?;
            let key = table
                .key_of(&item)
                .ok_or_else(|| anyhow!("dynamodb_key_required"))?;
            ensure!(seen.insert(key.to_string()), "dynamodb_import_duplicate");
            keys.push(key);
            items.push(item);
        }
        // PutItem overwrites an item with the same key. An import adds
        // items, so every key is looked up first; this part can be stopped.
        let taken = self
            .bounded(cancel, async {
                for chunk in keys.chunks(TRANSACTION_ITEMS) {
                    if self.any_exists(&table, chunk).await? {
                        return Ok(true);
                    }
                }
                Ok(false)
            })
            .await?;
        ensure!(!taken, "dynamodb_import_key_exists");
        // Written a transaction at a time: up to a hundred items that are
        // all written or none, each only when its key is still free.
        let partition = table.partition().name.clone();
        let mut chunks: Vec<Vec<Map<String, Value>>> = Vec::new();
        let mut bytes = 0;
        for item in items {
            let size = Value::Object(item.clone()).to_string().len();
            if chunks.last().is_none_or(|chunk| {
                chunk.len() >= TRANSACTION_ITEMS || bytes + size > TRANSACTION_BYTES
            }) {
                chunks.push(Vec::new());
                bytes = 0;
            }
            bytes += size;
            if let Some(chunk) = chunks.last_mut() {
                chunk.push(item);
            }
        }
        self.drop_samples();
        let mut transactions = true;
        let mut written_any = false;
        for chunk in &chunks {
            let mut outcome: Option<Result<()>> = None;
            if transactions {
                let writes: Vec<Value> = chunk
                    .iter()
                    .map(|item| {
                        json!({ "Put": {
                            "TableName": table.name,
                            "Item": item,
                            "ConditionExpression": "attribute_not_exists(#k)",
                            "ExpressionAttributeNames": { "#k": partition },
                        }})
                    })
                    .collect();
                match self
                    .client
                    .call("TransactWriteItems", &json!({ "TransactItems": writes }))
                    .await
                {
                    Ok(_) => outcome = Some(Ok(())),
                    // A server without transactions: item by item below.
                    Err(fail) if fail.is_unknown_operation() => transactions = false,
                    Err(fail) if fail.is_cancelled_by_condition() => {
                        outcome = Some(Err(anyhow!("dynamodb_import_key_exists")));
                    }
                    Err(fail) => outcome = Some(Err(fail.into())),
                }
            }
            let outcome = match outcome {
                Some(outcome) => outcome,
                None => {
                    // Each item still only when its key is free.
                    let mut outcome = Ok(());
                    for item in chunk {
                        match self.put(&table, item.clone()).await {
                            Ok(()) => written_any = true,
                            Err(error) => {
                                outcome = Err(error);
                                break;
                            }
                        }
                    }
                    outcome
                }
            };
            match outcome {
                Ok(()) => written_any = true,
                // Nothing was written yet: the import failed as a whole.
                Err(error) if !written_any => return Err(error),
                Err(_) => bail!("dynamodb_import_incomplete"),
            }
        }
        self.written().await;
        Ok(QueryResult {
            affected: rows.len() as u64,
            duration_ms: started.elapsed().as_millis() as u64,
            ..QueryResult::default()
        })
    }

    /// Where the service is, for the line under the chart: the interface
    /// already names the product and the region there.
    fn server(&self) -> String {
        let endpoint = &self.client.endpoint;
        if endpoint.is_aws() {
            endpoint.host.clone()
        } else {
            endpoint.url()
        }
    }

    async fn stats(&self) -> Result<QueryResult> {
        let started = Instant::now();
        let (total, tables) = self.described(MONITOR_TABLES).await?;
        let duration = started.elapsed().as_millis() as u64;
        let items: u64 = tables.iter().map(|table| table.items).sum();
        let bytes: u64 = tables.iter().map(|table| table.bytes).sum();
        let mut listed: Vec<String> = tables
            .iter()
            .map(|table| {
                let mut text = format!(
                    "{}: {}, {}, {} items, {}",
                    table.name,
                    table.status,
                    table.billing,
                    table.items,
                    browse::size(table.bytes)
                );
                let indexes = table.global_indexes.len();
                if indexes > 0 {
                    text.push_str(&format!(", {indexes} GSI"));
                }
                text
            })
            .collect();
        if total > tables.len() {
            listed.push(format!("+{} more", total - tables.len()));
        }
        let columns: Vec<(&str, Cell)> = vec![
            // The two large figures.
            ("tables", Cell::Integer(total.to_string())),
            ("Items", Cell::Integer(items.to_string())),
            ("version", Cell::Text(self.server())),
            ("database", Cell::Text(self.client.endpoint.region.clone())),
            ("size", Cell::Text(browse::size(bytes))),
            (
                "reads",
                Cell::Text(
                    if self.consistent {
                        "consistent"
                    } else {
                        "eventually consistent"
                    }
                    .into(),
                ),
            ),
            ("table list", Cell::Text(listed.join("; "))),
        ];
        let mut columns = columns;
        columns.retain(|(_, cell)| !matches!(cell, Cell::Text(text) if text.is_empty()));
        Ok(QueryResult {
            columns: columns
                .iter()
                .map(|(name, cell)| ResultColumn {
                    name: (*name).into(),
                    data_type: match cell {
                        Cell::Integer(_) => "number",
                        _ => "string",
                    }
                    .into(),
                })
                .collect(),
            rows: vec![columns.into_iter().map(|(_, cell)| cell).collect()],
            duration_ms: duration,
            ..QueryResult::default()
        })
    }
}

/// The table a change is made in.
fn table_of(change: &RowChange) -> &TableRef {
    match change {
        RowChange::UpdateCell { table, .. }
        | RowChange::InsertRow { table, .. }
        | RowChange::DeleteRow { table, .. } => table,
    }
}

impl Session for DynamoSession {
    fn dialect(&self) -> &Dialect {
        &DIALECT
    }

    fn execute<'a>(
        &'a self,
        text: &'a str,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(self.bounded(cancel, self.run_script(text)))
    }

    fn containers<'a>(&'a self) -> BoxFuture<'a, Result<Vec<TableRef>>> {
        Box::pin(self.briefly(async move {
            // A refresh of the tree is also when the tables are looked at
            // anew.
            if let Ok(mut known) = self.known.lock() {
                known.clear();
            }
            self.cursors.lock().await.clear();
            let mut containers: Vec<TableRef> = self
                .list()
                .await?
                .into_iter()
                .map(|name| TableRef {
                    schema: String::new(),
                    name,
                    kind: "table".into(),
                })
                .collect();
            containers.push(TableRef {
                schema: String::new(),
                name: OVERVIEW.into(),
                kind: "view".into(),
            });
            Ok(containers)
        }))
    }

    fn columns<'a>(&'a self, container: &'a TableRef) -> BoxFuture<'a, Result<Vec<ColumnInfo>>> {
        Box::pin(self.briefly(async move {
            if container.name == OVERVIEW {
                return Ok(Self::OVERVIEW_COLUMNS
                    .iter()
                    .map(|(name, kind)| ColumnInfo {
                        name: (*name).into(),
                        data_type: (*kind).into(),
                        nullable: true,
                        primary_key: false,
                        default: None,
                    })
                    .collect());
            }
            let table = self.learned(&container.name).await?;
            Ok(browse::column_info(
                &table,
                &self.attributes(&container.name),
            ))
        }))
    }

    fn server_stats<'a>(&'a self) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(self.briefly(self.stats()))
    }

    fn close<'a>(&'a self) -> BoxFuture<'a, ()> {
        // Nothing is held open but idle HTTP connections, which go with
        // the session.
        Box::pin(async {})
    }

    fn page_text(&self, page: &PageRequest) -> Result<String> {
        browse::page_text(self.remembered(&page.container.name).as_deref(), page)
    }

    fn read_page<'a>(
        &'a self,
        page: &'a PageRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(self.bounded(cancel, self.page(page)))
    }

    fn change_text(&self, change: &RowChange) -> Result<String> {
        let name = &table_of(change).name;
        ensure!(name != OVERVIEW, "dynamodb_read_only_object");
        // The grid described the table before it showed its items.
        let table = self
            .remembered(name)
            .ok_or_else(|| anyhow!("result_changed_reload_required"))?;
        Ok(match change {
            RowChange::UpdateCell {
                column,
                old,
                new,
                key,
                ..
            } => {
                let (key, old, new) = self.update_parts(&table, column, old, new, key)?;
                partiql::update_text(&table, &key, column, old.as_ref(), new.as_ref())
            }
            RowChange::InsertRow { values, .. } => {
                partiql::insert_text(&table, &self.new_item(&table, values)?)
            }
            RowChange::DeleteRow { key, .. } => partiql::delete_text(&table, &table.key_from(key)?),
        })
    }

    fn apply<'a>(&'a self, change: &'a RowChange) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.briefly(self.change(change)))
    }

    fn import_rows<'a>(
        &'a self,
        container: &'a TableRef,
        columns: &'a [String],
        rows: &'a [Vec<Cell>],
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(self.import(container, columns, rows, cancel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{BrowseModel, QueryLanguage};

    #[test]
    fn every_code_has_a_sentence_in_both_languages() {
        for code in CODES {
            for en in [false, true] {
                let text = describe(code, en).unwrap_or_else(|| panic!("{code} has no sentence"));
                assert!(text.contains(' ') && text.ends_with('.'), "{code}: {text}");
                assert!(!text.contains(code), "{code}");
            }
            assert_ne!(describe(code, false), describe(code, true), "{code}");
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            );
        }
        // Every code the client maps an exception to is one of them, or a
        // code of the application.
        for name in [
            "ResourceNotFoundException",
            "ConditionalCheckFailedException",
            "DuplicateItemException",
            "ExpiredTokenException",
            "ThrottlingException",
            "ResourceInUseException",
            "TransactionCanceledException",
            "ItemCollectionSizeLimitExceededException",
            "UnknownOperationException",
            "ValidationException",
            "InternalServerError",
        ] {
            let code = client::service_code(name, "");
            assert!(CODES.contains(&code), "{name}: {code}");
        }
        assert_eq!(describe("database_login_rejected", true), None);
        assert_eq!(describe("dynamodb_made_up", true), None);
    }

    #[test]
    fn the_products_are_presets_of_one_driver() {
        let spec = ENGINE.spec();
        assert_eq!(spec.generic().id, "amazon-dynamodb");
        assert_eq!(Engine::from_id("Dynamo"), Some(ENGINE));
        let ids: Vec<&str> = spec.products.iter().map(|product| product.id).collect();
        assert_eq!(
            ids,
            ["amazon-dynamodb", "dynamodb-local", "scylladb-alternator"]
        );
        for product in spec.products {
            assert!(product.note.is_some(), "{}", product.id);
            assert!(product.capabilities.is_none() && product.dialect.is_none());
            // A region is part of every signature.
            assert_eq!(product.default_database, "us-east-1");
        }
        let aws = ENGINE.product("amazon-dynamodb");
        assert_eq!(
            (aws.default_port, aws.default_tls),
            (443, TlsMode::VerifyFull)
        );
        assert!(aws.matches("aws dynamo") && aws.matches("partiql"));
        let local = ENGINE.product("dynamodb-local");
        assert_eq!(
            (local.default_port, local.default_tls),
            (8000, TlsMode::Disable)
        );
        // PartiQL is typed into the SQL editor; tables hang directly under
        // the connection.
        let caps = spec.capabilities;
        assert_eq!(caps.language, QueryLanguage::Sql);
        assert_eq!(caps.browse, BrowseModel::Tables);
        assert!(!caps.schemas && !caps.table_designer && !caps.query_builder);
        assert!(caps.filter && caps.sort && caps.monitor && caps.import);
        assert!(caps.edit_cells && caps.insert_rows && caps.delete_rows);
        assert_eq!(spec.form.database, FieldUse::Required);
        assert_eq!(spec.form.database_label, ("Region", "Region"));
        assert!(spec.form.tls && spec.form.ssh);
        // The session token is a text field of the engine; the keys are
        // the account of the profile.
        assert!(spec.option("session_token").is_some() && spec.option("endpoint").is_some());
    }

    #[tokio::test]
    async fn a_profile_is_checked_before_anything_is_dialled() {
        let mut profile = crate::model::ConnectionProfile {
            name: "Dynamo".into(),
            engine: ENGINE,
            host: "localhost".into(),
            port: 443,
            database: "eu-central-1".into(),
            username: "AKIDEXAMPLE".into(),
            password: String::new(),
            // Encrypted, so that a plain http endpoint below is a mismatch.
            tls: crate::model::TlsMode::VerifyFull,
            ..crate::model::ConnectionProfile::default()
        };
        profile.validate().unwrap();
        let failure = |profile: crate::model::ConnectionProfile| async move {
            crate::database::Database::connect(&profile, None)
                .await
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default()
        };
        assert_eq!(
            failure(profile.clone()).await,
            "dynamodb_credentials_required"
        );
        profile.password = "secret".into();
        profile.database = "Europe (Frankfurt)".into();
        assert_eq!(failure(profile.clone()).await, "dynamodb_region_invalid");
        profile.database = "eu-central-1".into();
        profile.set_option("endpoint", "localhost:8000");
        assert_eq!(failure(profile.clone()).await, "dynamodb_endpoint_invalid");
        profile.set_option("endpoint", "http://localhost:8000");
        assert_eq!(
            failure(profile.clone()).await,
            "dynamodb_endpoint_tls_mismatch"
        );
        profile.set_option("consistent_read", "maybe");
        assert_eq!(
            profile.validate().unwrap_err().to_string(),
            "connection_option_invalid"
        );
    }

    #[test]
    fn the_editor_text_is_split_outside_quotes_comments_and_json() {
        let text = "-- two\nSELECT * FROM \"Music\" WHERE \"Artist\" = 'a;b';\nPutItem {\"TableName\": \"Music\", \"Item\": {\"Artist\": {\"S\": \"x;\\\"y\"}}};\n/* done; */ SELECT 1";
        let split = statements(text);
        assert_eq!(split.len(), 3, "{split:?}");
        assert_eq!(split[0], "SELECT * FROM \"Music\" WHERE \"Artist\" = 'a;b'");
        assert!(split[1].starts_with("PutItem {") && split[1].ends_with("}}}"));
        assert_eq!(split[2], "SELECT 1");
        assert!(statements("-- nothing\n/* at all */ ;;").is_empty());
        assert_eq!(statements("SELECT 1").len(), 1);
    }

    #[test]
    fn an_operation_of_the_api_is_its_name_and_its_json_body() {
        assert_eq!(
            api_call("DescribeTable {\"TableName\": \"Music\"}"),
            Some(("DescribeTable".into(), json!({"TableName": "Music"})))
        );
        assert_eq!(
            api_call("  ListTables{}  "),
            Some(("ListTables".into(), json!({})))
        );
        // PartiQL is never mistaken for one.
        for statement in [
            "SELECT * FROM \"Music\"",
            "INSERT INTO \"Music\" VALUE {\"a\": 1}",
            "UPDATE {\"a\": 1}",
            "DescribeTable \"Music\"",
            "DescribeTable [1]",
            "describeTable {}",
            "Describe Table {}",
        ] {
            assert!(api_call(statement).is_none(), "{statement}");
        }
    }

    #[test]
    fn an_answer_without_items_is_one_row() {
        let shown = DynamoSession::answer(&json!({
            "TableNames": ["a", "b"],
            "Count": 2,
            "Last": null,
            "Name": "x",
        }));
        assert_eq!(
            shown
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["Count", "Last", "Name", "TableNames"]
        );
        assert_eq!(
            shown.rows,
            [vec![
                Cell::Decimal("2".into()),
                Cell::Null,
                Cell::Text("x".into()),
                Cell::Json(r#"["a","b"]"#.into()),
            ]]
        );
        assert!(DynamoSession::answer(&json!({})).columns.is_empty());
    }

    #[test]
    fn the_dialect_reads_back_what_the_driver_writes() {
        let known = [TableRef {
            schema: String::new(),
            name: "Order Items; -- x".into(),
            kind: "table".into(),
        }];
        for text in [
            "SELECT * FROM \"Order Items; -- x\"",
            "SELECT * FROM \"Order Items; -- x\" WHERE \"id\" = 'it''s'",
        ] {
            assert_eq!(crate::sql::statements(&DIALECT, text).unwrap().len(), 1);
            assert!(
                crate::sql::editable_table(&DIALECT, text, &known).is_some(),
                "{text}"
            );
        }
        assert_eq!(DIALECT.column_types, ["S", "N", "B"]);
    }
}
