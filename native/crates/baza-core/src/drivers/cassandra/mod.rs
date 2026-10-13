//! Apache Cassandra and the servers and services that speak the CQL native
//! protocol: ScyllaDB, Amazon Keyspaces, Azure Managed Instance for Apache
//! Cassandra.
//!
//! CQL looks like SQL and is edited in the SQL editor, but it is a query
//! language of a partitioned store, and this driver does not pretend
//! otherwise:
//!
//! * **Keyspaces** are the schema level, tables, materialized views and the
//!   virtual tables of a node its containers ([`catalog`]).
//! * **Pages** ([`browse`]): CQL has no `OFFSET`, no text search and orders
//!   rows only inside a partition. Pages are read forward with the paging
//!   state of the driver; a filter is an exact key where it can be and a
//!   bounded read otherwise; an order is made here on a bounded number of
//!   rows.
//! * **Changes** ([`edit`]): every write of CQL is an upsert, so a change of
//!   the grid is a lightweight transaction with the condition of the
//!   contract. Counters are refused.
//! * **Values** ([`value`], [`types`]): every CQL type has a cell; lists,
//!   sets, maps, tuples, user-defined types and vectors are JSON.
//! * **One node** ([`link`]): the connection talks to the node the profile
//!   names and to no other, so it works through an SSH tunnel.
//!
//! The table designer and the query builder are off: the shared generators
//! write `NOT NULL`, joins, `LIKE` and `GROUP BY` on any column, none of
//! which CQL has.
use super::{PageRequest, RowChange, Session};
use crate::engine::{
    Badge, Capabilities, ConnectRequest, ConnectionForm, ContainsFilter, DefaultSchema, Dialect,
    EmptyInsert, Engine, EngineSpec, FieldUse, Group, Lexer, OptionField, OptionKind, Paging,
    ParameterStyle, ProductSpec, TextLiteral,
};
use crate::model::{Cell, ColumnInfo, QueryResult, ResultColumn, TableRef, TlsMode};
use crate::sql::{MAX_RESULT_BYTES, MAX_RESULT_ROWS};
use anyhow::{Result, anyhow, bail, ensure};
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use scylla::client::session::Session as Driver;
use scylla::response::query_result::QueryRowsResult;
use scylla::response::{PagingState, PagingStateResponse};
use scylla::statement::Statement;
use scylla::value::Row;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

mod browse;
mod catalog;
mod edit;
mod link;
mod types;
mod value;

use browse::{Cursors, Mark, Mode, Plan, Test};
use catalog::Table;
use types::{CqlType, Udts};

pub const ENGINE: Engine = Engine::new(&SPEC);

/// How CQL is written. Pages and grid changes do not come from here: the
/// session writes them itself (`page_text`, `change_text`).
pub static DIALECT: Dialect = Dialect {
    name_quotes: ('"', '"'),
    text: TextLiteral::Standard,
    bytes: ("0x", ""),
    booleans: ("true", "false"),
    // There is no OFFSET; the session pages with the state of the driver.
    paging: Paging::None,
    contains: ContainsFilter::LIKE,
    parameters: ParameterStyle::Question,
    lexer: Lexer {
        // `$$ … $$` is a string.
        dollar_quotes: true,
        ..Lexer::STANDARD
    },
    empty_insert: EmptyInsert::DefaultValues,
    default_schema: DefaultSchema::Database,
    column_types: &[
        "uuid",
        "text",
        "int",
        "bigint",
        "decimal",
        "double",
        "boolean",
        "timestamp",
        "date",
        "time",
        "timeuuid",
        "varint",
        "float",
        "smallint",
        "tinyint",
        "inet",
        "blob",
        "duration",
        "ascii",
        "counter",
    ],
};

pub static SPEC: EngineSpec = EngineSpec {
    id: "cassandra",
    aliases: &["cql", "scylla", "scylladb"],
    portable_name: "cassandra",
    label: "Apache Cassandra",
    available: true,
    form: ConnectionForm {
        database: FieldUse::Optional,
        database_label: ("Keyspace", "Keyspace"),
        ..ConnectionForm::SERVER
    },
    capabilities: Capabilities {
        // Keyspaces are the level above tables.
        schemas: true,
        // Within limits: see `browse`.
        filter: true,
        sort: true,
        edit_cells: true,
        insert_rows: true,
        delete_rows: true,
        // The shared generator writes NOT NULL, which CQL does not have.
        table_designer: false,
        // Joins, LIKE and GROUP BY on any column: none of it is CQL.
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
        key: "consistency",
        label: ("Poziom spójności", "Consistency level"),
        kind: OptionKind::Choice(&[
            "local_quorum",
            "quorum",
            "local_one",
            "one",
            "each_quorum",
            "all",
        ]),
        default: "local_quorum",
        placeholder: "",
        help: Some((
            "Ile replik musi odpowiedzieć na odczyt i potwierdzić zapis. „local_quorum” to większość replik w centrum danych węzła; gdy część węzłów nie działa, „one” pozwala nadal czytać.",
            "How many replicas have to answer a read and acknowledge a write. “local_quorum” is the majority of the replicas in the data centre of the node; while some nodes are down, “one” still lets you read.",
        )),
        required: false,
    },
    OptionField {
        key: "system_keyspaces",
        label: ("Keyspace'y systemowe", "System keyspaces"),
        kind: OptionKind::Flag,
        default: "false",
        placeholder: "",
        help: Some((
            "Pokazuje w drzewie także keyspace'y serwera (system, system_schema, system_views…) z ich tabelami wirtualnymi.",
            "Also lists the keyspaces of the server itself (system, system_schema, system_views…) with their virtual tables.",
        )),
        required: false,
    },
];

/// What every product of the family shares: one node per connection.
const ONE_NODE: (&str, &str) = (
    "Połączenie rozmawia z jednym węzłem: tym, którego adres podasz. Węzeł ten obsługuje zapytania do całego klastra (tak jak w cqlsh), a pozostałych węzłów aplikacja nie wybiera, więc działa to także przez tunel SSH, przekierowanie portu i NAT.",
    "The connection talks to one node: the one whose address you give. That node serves the queries for the whole cluster (as with cqlsh) and the other nodes are not dialled, so it also works through an SSH tunnel, a port mapping and NAT.",
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
        default_port: 9042,
        default_tls: TlsMode::Disable,
        note: Some(ONE_NODE),
        ..ProductSpec::GENERIC
    }
}

static PRODUCTS: &[ProductSpec] = &[
    ProductSpec {
        keywords: &[
            "cassandra",
            "cql",
            "apache",
            "nosql",
            "wide column",
            "datastax",
        ],
        ..product(
            "cassandra",
            "Apache Cassandra",
            Group::KeyValue,
            "Ca",
            [18, 135, 184],
        )
    },
    ProductSpec {
        keywords: &["scylla", "cql", "cassandra", "nosql", "wide column"],
        note: Some((
            "ScyllaDB mówi protokołem CQL. Połączenie rozmawia z jednym węzłem przez zwykły port CQL (9042; port per shard 19042 nie jest używany), więc działa także przez tunel SSH. Zmiany w siatce to lekkie transakcje (LWT): w keyspace'ach z tabletami starsze wersje ScyllaDB ich nie obsługują.",
            "ScyllaDB speaks the CQL protocol. The connection talks to one node through the ordinary CQL port (9042; the per-shard port 19042 is not used), so it also works through an SSH tunnel. Changes in the grid are lightweight transactions (LWT): older ScyllaDB versions do not support them in keyspaces with tablets.",
        )),
        ..product(
            "scylladb",
            "ScyllaDB",
            Group::KeyValue,
            "Sc",
            [62, 167, 196],
        )
    },
    ProductSpec {
        default_port: 9142,
        default_tls: TlsMode::VerifyFull,
        keywords: &["aws", "amazon", "keyspaces", "cassandra", "cql", "mcs"],
        note: Some((
            "Host: cassandra.<region>.amazonaws.com, port 9142, szyfrowanie wymagane. Użytkownik i hasło to „poświadczenia specyficzne dla usługi” wygenerowane w IAM (logowanie SigV4 nie jest obsługiwane). Według dokumentacji AWS: zapisy tylko na poziomie local_quorum, tabele tworzą się asynchronicznie, nie ma widoków zmaterializowanych ani indeksów. Nie sprawdzono na żywej usłudze.",
            "Host: cassandra.<region>.amazonaws.com, port 9142, encryption required. The user and the password are the “service-specific credentials” generated in IAM (SigV4 sign-in is not supported). According to the AWS documentation: writes at local_quorum only, tables are created asynchronously, and there are no materialized views or indexes. Not tried against the live service.",
        )),
        ..product(
            "amazon-keyspaces",
            "Amazon Keyspaces",
            Group::Cloud,
            "Ks",
            [82, 127, 255],
        )
    },
    ProductSpec {
        default_tls: TlsMode::Require,
        default_username: "cassandra",
        keywords: &["azure", "microsoft", "managed instance", "cassandra", "cql"],
        note: Some((
            "Węzły mają adresy prywatne w sieci wirtualnej: podaj adres jednego węzła i zwykle użyj tunelu SSH przez maszynę w tej sieci. Szyfrowanie jest włączone, a certyfikaty węzłów są wystawione przez klaster, dlatego domyślnie bez weryfikacji; pełna weryfikacja wymaga pliku CA klastra. Konto to „cassandra” z hasłem nadanym przy tworzeniu klastra. Nie sprawdzono na żywej usłudze.",
            "The nodes have private addresses in a virtual network: give the address of one node and usually use an SSH tunnel through a machine in that network. Encryption is on and the node certificates are issued by the cluster itself, hence no verification by default; full verification needs the CA file of the cluster. The account is “cassandra” with the password set when the cluster was created. Not tried against the live service.",
        )),
        ..product(
            "azure-managed-cassandra",
            "Azure Managed Instance for Apache Cassandra",
            Group::Cloud,
            "Az",
            [0, 120, 212],
        )
    },
    ProductSpec {
        default_port: 29042,
        default_tls: TlsMode::VerifyFull,
        keywords: &["datastax", "astra", "cassandra", "cql", "serverless"],
        note: Some((
            "Jeszcze nieobsługiwane. Astra DB przyjmuje połączenia CQL tylko z „secure connect bundle”: archiwum z certyfikatem klienta i adresem pośrednika, który kieruje ruch po nazwie SNI. Ta wersja nie czyta takiego archiwum, więc połączenie zostanie odrzucone z tym wyjaśnieniem.",
            "Not supported yet. Astra DB accepts CQL connections only with a “secure connect bundle”: an archive with a client certificate and the address of a proxy that routes by SNI name. This version does not read such an archive, so a connection is refused with this explanation.",
        )),
        ..product(
            "datastax-astra-db",
            "DataStax Astra DB",
            Group::Cloud,
            "As",
            [130, 87, 229],
        )
    },
];

/// Longest a statement of the editor, a page or the reading part of an
/// import may take.
const DEADLINE: Duration = Duration::from_secs(120);
/// Longest a change of the grid or a look at the node may take.
const SHORT_DEADLINE: Duration = Duration::from_secs(30);
/// Rows asked of the server at once by the editor.
const EDITOR_FETCH: usize = 5000;
/// Most pages read on the way to one nobody visited yet.
const WALK_STEPS: usize = 400;
/// Key lookups of an import that run at the same time.
const IMPORT_CHECKS: usize = 16;
/// Writes of an import that run at the same time.
const IMPORT_WRITES: usize = 8;

/// A sentence for a result code of this driver.
fn describe(code: &str, en: bool) -> Option<&'static str> {
    let (pl, english) = match code {
        "cassandra_handshake_failed" => (
            "Serwer nie odpowiedział jak węzeł CQL. Sprawdź adres i port (zwykle 9042; Amazon Keyspaces 9142) oraz to, czy serwer wymaga szyfrowania.",
            "The server did not answer like a CQL node. Check the address and the port (usually 9042; Amazon Keyspaces 9142) and whether the server requires encryption.",
        ),
        "cassandra_astra_unsupported" => (
            "DataStax Astra DB wymaga archiwum „secure connect bundle” (certyfikat klienta i pośrednik SNI), którego ta wersja jeszcze nie obsługuje. Połączenie nie zostało nawiązane.",
            "DataStax Astra DB needs a “secure connect bundle” (a client certificate and an SNI proxy), which this version does not support yet. No connection was made.",
        ),
        "cassandra_syntax_error" => (
            "Serwer nie zrozumiał składni polecenia CQL. Sprawdź słowa kluczowe, nawiasy i cudzysłowy: nazwy w \"podwójnych\", teksty w 'pojedynczych'.",
            "The server did not understand the syntax of the CQL statement. Check the keywords, brackets and quotes: names in \"double\" quotes, texts in 'single' ones.",
        ),
        "cassandra_invalid_query" => (
            "Polecenie jest poprawne składniowo, ale serwer go nie przyjął. Najczęściej warunek WHERE nie pasuje do klucza tabeli albo wartość ma inny typ niż kolumna.",
            "The statement is well formed, but the server did not accept it. Most often the WHERE condition does not fit the key of the table, or a value has another type than its column.",
        ),
        "cassandra_filtering_required" => (
            "To zapytanie filtruje po kolumnie, która nie jest kluczem ani nie ma indeksu, więc serwer musiałby przejrzeć całą tabelę. Zawęź warunek do klucza partycji albo, świadomie, dopisz na końcu ALLOW FILTERING.",
            "This query filters by a column that is neither a key nor indexed, so the server would have to read the whole table. Narrow the condition to the partition key or, knowingly, add ALLOW FILTERING at the end.",
        ),
        "cassandra_order_restricted" => (
            "CQL sortuje tylko po kolumnach klastrujących i tylko wtedy, gdy warunek WHERE wskazuje klucz partycji. Dodaj taki warunek albo posortuj dane w siatce tabeli.",
            "CQL orders by clustering columns only, and only when the WHERE condition names the partition key. Add such a condition, or sort the data in the table grid.",
        ),
        "cassandra_keyspace_required" => (
            "Nie wybrano keyspace'u. Podaj go w nazwie tabeli (keyspace.tabela), wykonaj USE keyspace albo wpisz keyspace w profilu połączenia.",
            "No keyspace is selected. Name it with the table (keyspace.table), run USE keyspace, or enter a keyspace in the connection profile.",
        ),
        "cassandra_table_unknown" => (
            "Nie ma takiej tabeli w tym keyspace'ie. Sprawdź nazwę i keyspace; nazwy z wielkimi literami wymagają cudzysłowu.",
            "There is no such table in this keyspace. Check the name and the keyspace; names with capital letters need double quotes.",
        ),
        "cassandra_column_unknown" => (
            "Tabela nie ma kolumny o tej nazwie. Odśwież dane tabeli; nazwy z wielkimi literami wymagają cudzysłowu.",
            "The table has no column with this name. Reload the table data; names with capital letters need double quotes.",
        ),
        "cassandra_batch_too_large" => (
            "Paczka poleceń (BATCH) przekracza limit rozmiaru serwera. Podziel ją na mniejsze.",
            "The batch is larger than the server allows. Split it into smaller ones.",
        ),
        "cassandra_counter_rule" => (
            "To polecenie łamie zasady liczników: licznik można tylko zwiększać i zmniejszać (SET c = c + 1), a tabela z licznikami nie może mieć zwykłych kolumn.",
            "This statement breaks the rules of counters: a counter can only be added to and subtracted from (SET c = c + 1), and a table with counters cannot have ordinary columns.",
        ),
        "cassandra_key_rule" => (
            "Polecenie narusza zasady klucza głównego: w WHERE trzeba podać cały klucz partycji, kolumny klastrujące po kolei, a kolumn klucza nie da się zmieniać.",
            "The statement breaks the rules of the primary key: WHERE needs the whole partition key and the clustering columns in their order, and key columns cannot be changed.",
        ),
        "cassandra_condition_unsupported" => (
            "Serwer nie przyjął warunku IF (lekkiej transakcji) dla tej tabeli. Zmiany w siatce wymagają lekkich transakcji; taką tabelę zmieniaj poleceniami w edytorze.",
            "The server did not accept the IF condition (a lightweight transaction) for this table. Changes in the grid need lightweight transactions; change such a table with statements in the editor.",
        ),
        "cassandra_already_exists" => (
            "Keyspace albo tabela o tej nazwie już istnieje.",
            "A keyspace or a table with this name already exists.",
        ),
        "cassandra_function_failed" => (
            "Funkcja użyta w zapytaniu zakończyła się błędem na serwerze.",
            "A function used in the query failed on the server.",
        ),
        "cassandra_disabled_by_configuration" => (
            "Ta funkcja jest wyłączona w konfiguracji serwera (na przykład widoki zmaterializowane, indeksy SASI albo funkcje użytkownika).",
            "This feature is switched off in the configuration of the server (for example materialized views, SASI indexes or user-defined functions).",
        ),
        "cassandra_unavailable" => (
            "Za mało działających replik dla wybranego poziomu spójności. Sprawdź stan klastra albo wybierz niższy poziom (na przykład „one”) w ustawieniach połączenia.",
            "Too few replicas are alive for the chosen consistency level. Check the state of the cluster, or choose a lower level (for example “one”) in the connection settings.",
        ),
        "cassandra_overloaded" => (
            "Węzeł jest przeciążony albo dopiero się uruchamia i odrzucił polecenie. Spróbuj ponownie za chwilę.",
            "The node is overloaded or still starting and refused the statement. Try again in a moment.",
        ),
        "cassandra_read_timeout" => (
            "Repliki nie odpowiedziały na odczyt w czasie ustawionym na serwerze. Zapytanie czyta zbyt wiele danych: zawęź je do klucza partycji albo dodaj LIMIT.",
            "The replicas did not answer the read within the time set on the server. The query reads too much: narrow it to a partition key or add a LIMIT.",
        ),
        "cassandra_write_timeout" => (
            "Repliki nie potwierdziły zapisu w czasie ustawionym na serwerze. Zapis mógł zostać wykonany częściowo: odśwież dane i sprawdź.",
            "The replicas did not acknowledge the write within the time set on the server. The write may have been applied in part: reload the data and check.",
        ),
        "cassandra_server_error" => (
            "Serwer zgłosił błąd wewnętrzny przy tym poleceniu. Szczegóły są w dzienniku serwera.",
            "The server reported an internal error for this statement. The details are in the server log.",
        ),
        "cassandra_schema_disagreement" => (
            "Zmiana schematu została wykonana, ale węzły klastra nie uzgodniły jeszcze jego wersji. Odśwież drzewo za chwilę.",
            "The schema change was made, but the nodes of the cluster have not agreed on its version yet. Refresh the tree in a moment.",
        ),
        "cassandra_result_unreadable" => (
            "Wynik zawiera wartość typu, którego ta wersja nie potrafi odczytać (typ własny klasy Java).",
            "The result holds a value of a type this version cannot read (a custom type of a Java class).",
        ),
        "cassandra_value_invalid" => (
            "Wartość nie pasuje do typu kolumny. uuid: 8-4-4-4-12 znaków szesnastkowych; timestamp: 2024-05-01T12:30:00Z; date: 2024-05-01; duration: 1h30m; inet: adres IP; blob: 0x…; listy, zbiory i mapy jako JSON.",
            "The value does not fit the type of the column. uuid: 8-4-4-4-12 hex digits; timestamp: 2024-05-01T12:30:00Z; date: 2024-05-01; duration: 1h30m; inet: an IP address; blob: 0x…; lists, sets and maps as JSON.",
        ),
        "cassandra_type_unknown" => (
            "Kolumna ma typ użytkownika, którego definicji nie udało się odczytać. Odśwież drzewo połączenia.",
            "The column has a user-defined type whose definition could not be read. Refresh the tree of the connection.",
        ),
        "cassandra_type_unsupported" => (
            "Kolumna ma typ własny (klasę Java), którego wartości ta wersja nie potrafi zapisać.",
            "The column has a custom type (a Java class) whose values this version cannot write.",
        ),
        "cassandra_scan_limit" => (
            "Filtr przejrzał 100 000 kolejnych wierszy i nie znalazł tekstu, a tabela ma ich więcej. Cassandra nie wyszukuje tekstu, więc filtr czyta wiersze po kolei. Przejdź do następnej strony, aby szukać dalej, albo wpisz cały klucz partycji: taki filtr trafia wprost do wiersza.",
            "The filter read the next 100,000 rows and did not find the text, and the table has more. Cassandra does not search for text, so a filter reads rows one after another. Go to the next page to keep looking, or type a whole partition key: such a filter goes straight to the row.",
        ),
        "cassandra_sort_limit" => (
            "Cassandra sortuje tylko wewnątrz partycji, więc siatka sortuje sama i robi to dla najwyżej 20 000 wierszy. Zawęź dane filtrem, na przykład kluczem partycji, i posortuj ponownie.",
            "Cassandra orders rows only inside a partition, so the grid sorts by itself, and does so for at most 20,000 rows. Narrow the data with a filter, for example a partition key, and sort again.",
        ),
        "cassandra_page_too_deep" => (
            "CQL nie pozwala przeskoczyć do dowolnej strony: trzeba przeczytać wszystkie wcześniejsze. Ta strona jest zbyt daleko. Wróć na początek i zawęź dane filtrem.",
            "CQL cannot jump to an arbitrary page: every page before it has to be read. This page is too far away. Go back to the start and narrow the data with a filter.",
        ),
        "cassandra_key_required" => (
            "Nowy wiersz musi mieć wartość w każdej kolumnie klucza głównego (klucz partycji i kolumny klastrujące). W oknie nowego wiersza odznacz przy nich „Pomiń” i wpisz wartości.",
            "A new row needs a value in every column of the primary key (the partition key and the clustering columns). In the new row dialog untick “Omit” next to them and enter the values.",
        ),
        "cassandra_key_read_only" => (
            "Kolumny klucza głównego nie da się zmienić: wyznacza ona miejsce wiersza. Dodaj wiersz z nowym kluczem i usuń stary.",
            "A column of the primary key cannot be changed: it says where the row lives. Add a row with the new key and delete the old one.",
        ),
        "cassandra_row_exists" => (
            "Wiersz o tym kluczu już istnieje. W CQL zwykły INSERT nadpisałby go bez pytania, dlatego siatka dodaje tylko nowe klucze. Zmień istniejący wiersz albo podaj inny klucz.",
            "A row with this key already exists. A plain INSERT of CQL would overwrite it without asking, so the grid only adds new keys. Change the existing row or give another key.",
        ),
        "cassandra_counter_read_only" => (
            "Tabela z licznikami nie jest zmieniana w siatce: licznika nie da się ustawić, można go tylko zwiększyć albo zmniejszyć. Użyj edytora: UPDATE tabela SET licznik = licznik + 1 WHERE klucz = …",
            "A table of counters is not changed in the grid: a counter cannot be set, only added to or subtracted from. Use the editor: UPDATE table SET counter = counter + 1 WHERE key = …",
        ),
        "cassandra_static_row" => (
            "Ten wiersz pokazuje tylko kolumny statyczne partycji i nie ma klucza klastrującego, więc nie da się go zaadresować. Kolumnę statyczną zmienisz w każdym wierszu tej partycji; pozostałe zmiany wykonaj w edytorze.",
            "This row only shows the static columns of its partition and has no clustering key, so it cannot be addressed. A static column can be changed in any row of the partition; make other changes in the editor.",
        ),
        "cassandra_read_only_object" => (
            "To jest widok zmaterializowany albo tabela wirtualna serwera: jego wierszy nie zmienia się wprost.",
            "This is a materialized view or a virtual table of the server: its rows are not changed directly.",
        ),
        "cassandra_import_key_missing" => (
            "Plik musi zawierać wszystkie kolumny klucza głównego tabeli (klucz partycji i kolumny klastrujące).",
            "The file has to contain every column of the primary key of the table (the partition key and the clustering columns).",
        ),
        "cassandra_import_key_exists" => (
            "Co najmniej jeden wiersz pliku ma klucz, który już istnieje w tabeli. W CQL import nadpisałby te wiersze, więc nic nie zostało zapisane. Usuń je z pliku albo z tabeli.",
            "At least one row of the file has a key that already exists in the table. In CQL an import would overwrite those rows, so nothing was written. Remove them from the file or from the table.",
        ),
        "cassandra_import_duplicate" => (
            "Dwa wiersze pliku mają ten sam klucz główny. Nic nie zostało zapisane.",
            "Two rows of the file have the same primary key. Nothing was written.",
        ),
        "cassandra_import_incomplete" => (
            "Serwer odrzucił część importu już w trakcie zapisu. CQL nie ma transakcji obejmującej wiele partycji, więc część wierszy została zapisana: odśwież tabelę i sprawdź dane.",
            "The server refused a part of the import while it was being written. CQL has no transaction across partitions, so some rows were written: reload the table and check the data.",
        ),
        "cassandra_keyspace_name_invalid" => (
            "USE przyjmuje nazwę jednego keyspace'u: litery, cyfry i podkreślenia, do 48 znaków.",
            "USE takes the name of one keyspace: letters, digits and underscores, up to 48 characters.",
        ),
        _ => return None,
    };
    Some(if en { english } else { pl })
}

/// Every code this driver can answer with that has a sentence here.
#[cfg(test)]
const CODES: &[&str] = &[
    "cassandra_handshake_failed",
    "cassandra_astra_unsupported",
    "cassandra_syntax_error",
    "cassandra_invalid_query",
    "cassandra_filtering_required",
    "cassandra_order_restricted",
    "cassandra_keyspace_required",
    "cassandra_table_unknown",
    "cassandra_column_unknown",
    "cassandra_batch_too_large",
    "cassandra_counter_rule",
    "cassandra_key_rule",
    "cassandra_condition_unsupported",
    "cassandra_already_exists",
    "cassandra_function_failed",
    "cassandra_disabled_by_configuration",
    "cassandra_unavailable",
    "cassandra_overloaded",
    "cassandra_read_timeout",
    "cassandra_write_timeout",
    "cassandra_server_error",
    "cassandra_schema_disagreement",
    "cassandra_result_unreadable",
    "cassandra_value_invalid",
    "cassandra_type_unknown",
    "cassandra_type_unsupported",
    "cassandra_scan_limit",
    "cassandra_sort_limit",
    "cassandra_page_too_deep",
    "cassandra_key_required",
    "cassandra_key_read_only",
    "cassandra_row_exists",
    "cassandra_counter_read_only",
    "cassandra_static_row",
    "cassandra_read_only_object",
    "cassandra_import_key_missing",
    "cassandra_import_key_exists",
    "cassandra_import_duplicate",
    "cassandra_import_incomplete",
    "cassandra_keyspace_name_invalid",
];

/// What the node said it is.
#[derive(Clone, Debug, Default, PartialEq)]
struct Server {
    /// `Apache Cassandra 5.0.9`, `ScyllaDB 6.2.0`.
    version: String,
    cluster: String,
    datacenter: String,
    rack: String,
    partitioner: String,
    cql: String,
}

impl Server {
    /// Read `system.local`, and for ScyllaDB the version it reports of
    /// itself next to the Cassandra version it is compatible with.
    fn of(local: &Grid, scylla: Option<&str>, product: &ProductSpec) -> Server {
        let get = |name: &str| local.first(name).unwrap_or_default();
        let release = get("release_version");
        let engine = match scylla {
            Some(version) => format!("ScyllaDB {version}"),
            None if release.is_empty() => "Apache Cassandra".to_owned(),
            None => format!("Apache Cassandra {release}"),
        };
        // A hosted service answers with the version it is compatible with:
        // it follows the name of the service.
        let version = match product.id {
            "cassandra" | "scylladb" => engine,
            _ => format!("{} ({engine})", product.label),
        };
        Server {
            version,
            cluster: get("cluster_name"),
            datacenter: get("data_center"),
            rack: get("rack"),
            partitioner: get("partitioner")
                .rsplit('.')
                .next()
                .unwrap_or_default()
                .to_owned(),
            cql: get("cql_version"),
        }
    }
}

/// A small result: the names of its columns and its rows as cells.
#[derive(Debug, Default)]
struct Grid {
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<Cell>>,
}

impl Grid {
    fn index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    /// The text of a column of the first row; nothing for a null.
    fn first(&self, name: &str) -> Option<String> {
        let cell = self.rows.first()?.get(self.index(name)?)?;
        (!matches!(cell, Cell::Null)).then(|| cell.display())
    }
}

/// The rows of a page of a result as cells.
fn grid_of(rows: &QueryRowsResult) -> Result<Grid> {
    let columns = rows
        .column_specs()
        .iter()
        .map(|spec| ResultColumn {
            name: spec.name().to_owned(),
            data_type: CqlType::of(spec.typ()).name(),
        })
        .collect();
    let mut out = Vec::with_capacity(rows.rows_num());
    let typed = rows
        .rows::<Row>()
        .map_err(|_| anyhow!("cassandra_result_unreadable"))?;
    for row in typed {
        let row = row.map_err(|_| anyhow!("cassandra_result_unreadable"))?;
        out.push(
            row.columns
                .iter()
                .map(|value| value::cell(value.as_ref()))
                .collect(),
        );
    }
    Ok(Grid { columns, rows: out })
}

/// One answer of the server to one request for a page.
struct Fetched {
    /// `None` for a statement without a result set.
    grid: Option<Grid>,
    /// Where the next page starts; `None` after the last one.
    next: Option<Arc<[u8]>>,
    bytes: usize,
}

/// What reading rows from a mark gave.
struct Read {
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<Cell>>,
    /// Where the rows after these start.
    next: Mark,
    /// The reading stopped at the limit of rows one page checks.
    capped: bool,
}

/// The first words of a statement in upper case, comments skipped: enough
/// to tell what kind of statement it is.
fn leading_words(statement: &str, count: usize) -> Vec<String> {
    let mut words = Vec::new();
    let mut rest = statement;
    while words.len() < count {
        rest = rest.trim_start();
        if rest.starts_with("--") || rest.starts_with("//") {
            rest = rest.split_once('\n').map_or("", |(_, rest)| rest);
        } else if rest.starts_with("/*") {
            rest = rest.split_once("*/").map_or("", |(_, rest)| rest);
        } else {
            let end = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            if end == 0 {
                break;
            }
            words.push(rest[..end].to_uppercase());
            rest = &rest[end..];
        }
    }
    words
}

/// Split the text of the editor into statements. A batch is one statement
/// with `;` inside: `BEGIN BATCH … APPLY BATCH` is put together again.
fn statements(text: &str) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut batch: Option<String> = None;
    for piece in crate::sql::statements(&DIALECT, text)? {
        let words = leading_words(&piece, 2);
        let first = words.first().map(String::as_str);
        if let Some(open) = batch.as_mut() {
            open.push_str(";\n");
            open.push_str(&piece);
            if first == Some("APPLY") {
                out.extend(batch.take());
            }
        } else if first == Some("BEGIN") {
            batch = Some(piece);
        } else if !words.is_empty() {
            out.push(piece);
        }
    }
    // A batch that was never closed: the server says what is wrong with it.
    out.extend(batch);
    Ok(out)
}

/// The keyspace of a `USE` statement and whether its name was quoted.
fn use_target(statement: &str) -> Option<(String, bool)> {
    let words = leading_words(statement, 1);
    if words.first().map(String::as_str) != Some("USE") {
        return None;
    }
    let at = statement.to_uppercase().find("USE")? + 3;
    let name = statement[at..].trim().trim_end_matches(';').trim();
    if let Some(inner) = name
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        Some((inner.replace("\"\"", "\""), true))
    } else {
        Some((name.to_lowercase(), false))
    }
}

/// `SELECT 1`: what the connection dialog asks every engine, and what a new
/// editor tab holds. CQL has no such statement.
fn is_probe(statement: &str) -> bool {
    let words = leading_words(statement, 3);
    words.len() == 2
        && words[0] == "SELECT"
        && words[1] == "1"
        && statement
            .trim()
            .trim_end_matches(';')
            .trim_end()
            .ends_with('1')
}

struct CassandraSession {
    /// The session of the CQL driver; gone after `close`.
    driver: RwLock<Option<Arc<Driver>>>,
    server: Server,
    /// The keyspace of the profile.
    home: String,
    consistency: &'static str,
    show_system: bool,
    /// The tables that were described, and the user-defined types of their
    /// keyspaces.
    tables: Mutex<HashMap<(String, String), Arc<Table>>>,
    udts: Mutex<HashMap<String, Arc<Udts>>>,
    cursors: tokio::sync::Mutex<Cursors>,
    /// Statements of the monitor this server refused: not asked again.
    absent: Mutex<HashSet<&'static str>>,
}

fn connect<'a>(request: &'a ConnectRequest<'a>) -> BoxFuture<'a, Result<Arc<dyn Session>>> {
    Box::pin(async move {
        let profile = request.profile;
        ensure!(
            request.product.id != "datastax-astra-db",
            "cassandra_astra_unsupported"
        );
        let consistency = request.option("consistency");
        let level =
            link::consistency(consistency).ok_or_else(|| anyhow!("connection_option_invalid"))?;
        let consistency = SPEC
            .option("consistency")
            .and_then(|field| match field.kind {
                OptionKind::Choice(values) => values.iter().find(|value| **value == consistency),
                _ => None,
            })
            .copied()
            .unwrap_or("local_quorum");
        let target = link::Target {
            dial_host: request.dial_host(),
            dial_port: request.dial_port(),
            server_name: request.server_name().to_owned(),
            tls: profile.tls,
            ca_file: profile.ca_file.clone(),
            username: profile.username.clone(),
            password: profile.password.clone(),
            keyspace: profile.database.clone(),
            consistency: level,
        };
        let driver = link::open(&target).await?;
        let mut session = CassandraSession {
            driver: RwLock::new(Some(Arc::new(driver))),
            server: Server::default(),
            home: profile.database.clone(),
            consistency,
            show_system: request.option("system_keyspaces") == "true",
            tables: Mutex::new(HashMap::new()),
            udts: Mutex::new(HashMap::new()),
            cursors: tokio::sync::Mutex::new(Cursors::default()),
            absent: Mutex::new(HashSet::new()),
        };
        let local = session.small("SELECT * FROM system.local").await?;
        // Only ScyllaDB has this table.
        let scylla = session
            .small("SELECT version FROM system.versions WHERE key = 'local'")
            .await
            .ok()
            .and_then(|grid| grid.first("version"));
        session.server = Server::of(&local, scylla.as_deref(), request.product);
        Ok(Arc::new(session) as Arc<dyn Session>)
    })
}

/// How a run ended, before its futures are gone.
enum Ended<T> {
    Done(T),
    Cancelled,
    TimedOut,
}

impl CassandraSession {
    fn driver(&self) -> Result<Arc<Driver>> {
        self.driver
            .read()
            .ok()
            .and_then(|driver| driver.clone())
            .ok_or_else(|| anyhow!("database_network_error"))
    }

    /// Run something until it is done, stopped or late. CQL has no way to
    /// stop a request that was sent: the one in flight ends on the server by
    /// the server's own time limits, and no further page is asked for.
    async fn bounded<T>(
        &self,
        cancel: CancellationToken,
        work: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        if cancel.is_cancelled() {
            bail!("query_cancelled");
        }
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

    /// Ask for one page of a statement.
    async fn fetch(&self, cql: &str, size: usize, state: Option<&Arc<[u8]>>) -> Result<Fetched> {
        let statement = Statement::new(cql).with_page_size(size.clamp(1, EDITOR_FETCH) as i32);
        let state = match state {
            Some(bytes) => PagingState::new_from_raw_bytes(bytes.clone()),
            None => PagingState::start(),
        };
        let (result, response) = self
            .driver()?
            .query_single_page(statement, &[], state)
            .await
            .map_err(link::failed)?;
        let next = match response {
            PagingStateResponse::HasMorePages { state } => state.as_bytes_slice().cloned(),
            PagingStateResponse::NoMorePages => None,
        };
        if !result.is_rows() {
            return Ok(Fetched {
                grid: None,
                next: None,
                bytes: 0,
            });
        }
        let rows = result
            .into_rows_result()
            .map_err(|_| anyhow!("cassandra_result_unreadable"))?;
        Ok(Fetched {
            bytes: rows.rows_bytes_size(),
            grid: Some(grid_of(&rows)?),
            next,
        })
    }

    /// A small result in one piece: the schema tables and the like.
    async fn small(&self, cql: &str) -> Result<Grid> {
        let mut grid = Grid::default();
        let mut state = None;
        loop {
            let fetched = self.fetch(cql, EDITOR_FETCH, state.as_ref()).await?;
            if let Some(page) = fetched.grid {
                grid.columns = page.columns;
                grid.rows.extend(page.rows);
            }
            state = fetched.next;
            if state.is_none() || grid.rows.len() >= MAX_RESULT_ROWS {
                return Ok(grid);
            }
        }
    }

    /// Run one statement of the editor to its end, or to the size limit of
    /// a result.
    async fn run_statement(&self, statement: &str) -> Result<QueryResult> {
        if is_probe(statement) {
            self.small("SELECT key FROM system.local").await?;
            return Ok(QueryResult {
                columns: vec![ResultColumn {
                    name: "1".into(),
                    data_type: "int".into(),
                }],
                rows: vec![vec![Cell::Integer("1".into())]],
                ..QueryResult::default()
            });
        }
        if let Some((keyspace, quoted)) = use_target(statement) {
            self.driver()?
                .use_keyspace(keyspace, quoted)
                .await
                .map_err(|error| match error {
                    scylla::errors::UseKeyspaceError::BadKeyspaceName(_) => {
                        anyhow!("cassandra_keyspace_name_invalid")
                    }
                    _ => anyhow!("database_not_found"),
                })?;
            return Ok(QueryResult::default());
        }
        let mut result = QueryResult::default();
        let mut state = None;
        let mut bytes = 0;
        loop {
            let fetched = self.fetch(statement, EDITOR_FETCH, state.as_ref()).await?;
            let Some(page) = fetched.grid else {
                break;
            };
            result.columns = page.columns;
            bytes += fetched.bytes;
            let room = MAX_RESULT_ROWS - result.rows.len();
            let more = page.rows.len() > room;
            result.rows.extend(page.rows.into_iter().take(room));
            state = fetched.next;
            if more
                || (state.is_some()
                    && (result.rows.len() >= MAX_RESULT_ROWS || bytes >= MAX_RESULT_BYTES))
            {
                result.truncated = true;
                break;
            }
            if state.is_none() {
                break;
            }
        }
        // What was described may no longer be true after a schema change.
        if matches!(
            leading_words(statement, 1).first().map(String::as_str),
            Some("CREATE" | "ALTER" | "DROP" | "TRUNCATE")
        ) {
            self.forget().await;
        }
        Ok(result)
    }

    async fn run_script(&self, text: &str) -> Result<QueryResult> {
        let started = Instant::now();
        let mut last = QueryResult::default();
        for statement in statements(text)? {
            last = self.run_statement(&statement).await?;
        }
        last.duration_ms = started.elapsed().as_millis() as u64;
        Ok(last)
    }

    /// Drop what is remembered of the schema and of the pages.
    async fn forget(&self) {
        if let Ok(mut tables) = self.tables.lock() {
            tables.clear();
        }
        if let Ok(mut udts) = self.udts.lock() {
            udts.clear();
        }
        self.cursors.lock().await.clear();
    }

    fn remembered(&self, container: &TableRef) -> Option<Arc<Table>> {
        self.tables
            .lock()
            .ok()?
            .get(&(container.schema.clone(), container.name.clone()))
            .cloned()
    }

    /// The user-defined types of a keyspace.
    async fn types(&self, keyspace: &str) -> Arc<Udts> {
        if let Some(known) = self
            .udts
            .lock()
            .ok()
            .and_then(|udts| udts.get(keyspace).cloned())
        {
            return known;
        }
        let Ok(literal) = value::literal(
            &CqlType::Native(types::Native::Text),
            &Cell::Text(keyspace.into()),
            &Udts::new(),
        ) else {
            return Arc::new(Udts::new());
        };
        // A service without the table has no user-defined types.
        let udts = match self
            .small(&format!(
                "SELECT type_name, field_names, field_types FROM system_schema.types WHERE keyspace_name = {literal}"
            ))
            .await
        {
            Ok(grid) => Arc::new(catalog::udts_of(&grid.rows)),
            Err(_) => Arc::new(Udts::new()),
        };
        if let Ok(mut known) = self.udts.lock() {
            known.insert(keyspace.to_owned(), udts.clone());
        }
        udts
    }

    /// Describe a table, a materialized view or a virtual table.
    async fn table(&self, container: &TableRef) -> Result<Arc<Table>> {
        if let Some(known) = self.remembered(container) {
            return Ok(known);
        }
        ensure!(!container.schema.is_empty(), "cassandra_keyspace_required");
        let text = CqlType::Native(types::Native::Text);
        let none = Udts::new();
        let keyspace = value::literal(&text, &Cell::Text(container.schema.clone()), &none)?;
        let name = value::literal(&text, &Cell::Text(container.name.clone()), &none)?;
        let columns = |schema: &str| {
            format!(
                "SELECT column_name, kind, position, type, clustering_order FROM {schema}.columns WHERE keyspace_name = {keyspace} AND table_name = {name}"
            )
        };
        let mut grid = self.small(&columns("system_schema")).await?;
        let mut writable = true;
        if grid.rows.is_empty() {
            // A virtual table of the node is described elsewhere.
            grid = self
                .small(&columns("system_virtual_schema"))
                .await
                .unwrap_or_default();
            writable = false;
        }
        ensure!(!grid.rows.is_empty(), "cassandra_table_unknown");
        if writable {
            // A materialized view has columns like a table.
            let view = self
                .small(&format!(
                    "SELECT view_name FROM system_schema.views WHERE keyspace_name = {keyspace} AND view_name = {name}"
                ))
                .await
                .is_ok_and(|views| !views.rows.is_empty());
            writable = !view;
        }
        let table = Arc::new(Table::new(
            &container.schema,
            &container.name,
            grid.rows
                .iter()
                .filter_map(|row| catalog::column_of(row))
                .collect(),
            self.types(&container.schema).await,
            writable,
        ));
        if let Ok(mut tables) = self.tables.lock() {
            tables.insert(
                (container.schema.clone(), container.name.clone()),
                table.clone(),
            );
        }
        Ok(table)
    }

    /// Read rows of a statement from a mark until `want` of them pass the
    /// tests, the statement ends, or `cap` rows were checked.
    async fn read(
        &self,
        cql: &str,
        from: &Mark,
        tests: &[Test],
        want: usize,
        cap: usize,
    ) -> Result<Read> {
        let mut read = Read {
            columns: Vec::new(),
            rows: Vec::new(),
            next: Mark::End,
            capped: false,
        };
        let Mark::At { state, skip } = from else {
            return Ok(read);
        };
        let mut state = state.clone();
        let mut skip = *skip;
        let mut checked = 0;
        loop {
            let size = if tests.is_empty() {
                want - read.rows.len() + skip
            } else {
                browse::FETCH_ROWS
            };
            let fetched = self.fetch(cql, size, state.as_ref()).await?;
            let Some(page) = fetched.grid else {
                return Ok(read);
            };
            read.columns = page.columns;
            let count = page.rows.len();
            for (index, row) in page.rows.into_iter().enumerate() {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                checked += 1;
                if !tests
                    .iter()
                    .all(|test| row.get(test.column).is_some_and(|cell| test.passes(cell)))
                {
                    continue;
                }
                read.rows.push(row);
                if read.rows.len() == want {
                    // The next page starts after this row: inside this
                    // answer of the server, or with its next one.
                    read.next = if index + 1 < count {
                        Mark::At {
                            state,
                            skip: index + 1,
                        }
                    } else {
                        match fetched.next {
                            Some(next) => Mark::At {
                                state: Some(next),
                                skip: 0,
                            },
                            None => Mark::End,
                        }
                    };
                    return Ok(read);
                }
            }
            match fetched.next {
                None => return Ok(read),
                Some(next) => state = Some(next),
            }
            if !tests.is_empty() && checked >= cap {
                read.next = Mark::At { state, skip };
                read.capped = true;
                return Ok(read);
            }
        }
    }

    async fn page(&self, request: &PageRequest) -> Result<QueryResult> {
        let started = Instant::now();
        let table = self.table(&request.container).await?;
        let plan = Plan::new(&table, request)?;
        let limit = request.limit as usize;
        let mut cursors = self.cursors.lock().await;
        let cursor = cursors.of(&browse::identity(request), request.offset == 0);
        if cursor.mode.is_none() {
            cursor.mode = Some(match &plan.exact {
                // One row tells whether the key that was typed exists.
                Some(exact) if !Plan::exact_only(request, &table) => {
                    let found = self.fetch(exact, 1, None).await?;
                    if found.grid.is_some_and(|grid| !grid.rows.is_empty()) {
                        Mode::Exact
                    } else {
                        Mode::Walk
                    }
                }
                Some(_) => Mode::Exact,
                None => Mode::Walk,
            });
        }
        let (statement, tests) = match (cursor.mode, &plan.exact) {
            (Some(Mode::Exact), Some(exact)) => (exact.as_str(), plan.exact_tests.as_slice()),
            _ => (plan.base.as_str(), plan.tests.as_slice()),
        };
        let mut result = QueryResult::default();
        if let Some((column, descending)) = plan.order {
            // CQL orders rows inside one partition only: the rows of the
            // view are read once, up to a limit, and ordered here.
            let sorted = match &cursor.sorted {
                Some(sorted) => sorted.clone(),
                None => {
                    let read = self
                        .read(
                            statement,
                            &Mark::START,
                            tests,
                            browse::SORT_ROWS + 1,
                            browse::SCAN_ROWS,
                        )
                        .await?;
                    ensure!(
                        !read.capped && read.rows.len() <= browse::SORT_ROWS,
                        "cassandra_sort_limit"
                    );
                    let mut rows = read.rows;
                    rows.sort_by(|left, right| {
                        let order = browse::compare(&left[column], &right[column]);
                        if descending { order.reverse() } else { order }
                    });
                    cursor.columns = read.columns;
                    let sorted = Arc::new(rows);
                    cursor.sorted = Some(sorted.clone());
                    sorted
                }
            };
            result.rows = sorted
                .iter()
                .skip(usize::try_from(request.offset).unwrap_or(usize::MAX))
                .take(limit)
                .cloned()
                .collect();
        } else {
            let (mut at, mut mark) = cursor.nearest(request.offset);
            // A page nobody visited yet is reached by reading up to it.
            let mut steps = 0;
            while at < request.offset && mark != Mark::End {
                ensure!(steps < WALK_STEPS, "cassandra_page_too_deep");
                steps += 1;
                let step = (request.offset - at).min(u64::from(request.limit));
                let read = self
                    .read(statement, &mark, tests, step as usize, browse::SCAN_ROWS)
                    .await?;
                at += step;
                mark = read.next;
                cursor.mark(at, mark.clone());
            }
            let read = self
                .read(statement, &mark, tests, limit, browse::SCAN_ROWS)
                .await?;
            if !read.columns.is_empty() {
                cursor.columns = read.columns;
            }
            cursor.mark(request.offset + u64::from(request.limit), read.next);
            // Nothing found in what one page may read, and more to read:
            // said in words; the next page goes on from there.
            ensure!(
                !(read.capped && read.rows.is_empty()),
                "cassandra_scan_limit"
            );
            result.truncated = read.capped;
            result.rows = read.rows;
        }
        result.columns = if cursor.columns.is_empty() {
            table
                .columns
                .iter()
                .map(|column| ResultColumn {
                    name: column.name.clone(),
                    data_type: column.ty.name(),
                })
                .collect()
        } else {
            cursor.columns.clone()
        };
        browse::labelled(&table, &mut result.columns);
        result.duration_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }

    /// Run a statement with a condition and say whether it was applied.
    async fn conditional(&self, cql: &str) -> Result<bool> {
        let grid = self.small(cql).await?;
        Ok(match grid.rows.first().and_then(|row| row.first()) {
            Some(Cell::Bool(applied)) => *applied,
            // Without the answer of a lightweight transaction the write
            // was a plain one and went through.
            _ => true,
        })
    }

    async fn change(&self, change: &RowChange) -> Result<()> {
        edit::check_kind(change)?;
        let table = self.table(edit::table_of(change)).await?;
        let statement = edit::statement(&table, change)?;
        if !self.conditional(&statement).await? {
            return Err(edit::refused(change));
        }
        Ok(())
    }

    async fn import(
        &self,
        container: &TableRef,
        columns: &[String],
        rows: &[Vec<Cell>],
        cancel: CancellationToken,
    ) -> Result<QueryResult> {
        let started = Instant::now();
        ensure!(
            container.kind.contains("table"),
            "cassandra_read_only_object"
        );
        let table = self.table(container).await?;
        // Everything is turned into statements first: a value that cannot
        // be written stops the import before anything is sent.
        let planned = edit::import_rows(&table, columns, rows)?;
        // An INSERT of CQL overwrites a row with the same key. An import
        // adds rows, so every key is looked up first; this part can be
        // stopped.
        let driver = self.driver()?;
        let taken = self
            .bounded(cancel, async {
                let questions: Vec<String> = planned.iter().map(|row| row.exists.clone()).collect();
                let mut lookups =
                    futures_util::stream::iter(questions.into_iter().map(|question| {
                        let driver = driver.clone();
                        async move {
                            let found = driver
                                .query_unpaged(question, &[])
                                .await
                                .map_err(link::failed)?
                                .into_rows_result()
                                .map_err(|_| anyhow!("cassandra_result_unreadable"))?;
                            Ok::<bool, anyhow::Error>(found.rows_num() > 0)
                        }
                    }))
                    .buffer_unordered(IMPORT_CHECKS);
                while let Some(found) = lookups.next().await {
                    if found? {
                        return Ok(true);
                    }
                }
                Ok(false)
            })
            .await?;
        ensure!(!taken, "cassandra_import_key_exists");
        // The writing is not stopped half way: CQL has no transaction that
        // would take back what was written.
        let mut batches = edit::batches(&planned).into_iter();
        if let Some(first) = batches.next() {
            // Alone, so that a refusal of the first write leaves nothing.
            driver
                .query_unpaged(first, &[])
                .await
                .map_err(link::failed)?;
        }
        let mut writes = futures_util::stream::iter(batches.map(|batch| {
            let driver = driver.clone();
            async move { driver.query_unpaged(batch, &[]).await }
        }))
        .buffer_unordered(IMPORT_WRITES);
        while let Some(written) = writes.next().await {
            if written.is_err() {
                bail!("cassandra_import_incomplete");
            }
        }
        self.cursors.lock().await.clear();
        Ok(QueryResult {
            affected: rows.len() as u64,
            duration_ms: started.elapsed().as_millis() as u64,
            ..QueryResult::default()
        })
    }

    /// A statement of the monitor that a server may not know: asked until
    /// it fails once.
    async fn optional(&self, cql: &'static str) -> Option<Grid> {
        if self.absent.lock().ok()?.contains(cql) {
            return None;
        }
        match self.small(cql).await {
            Ok(grid) => Some(grid),
            Err(_) => {
                if let Ok(mut absent) = self.absent.lock() {
                    absent.insert(cql);
                }
                None
            }
        }
    }

    async fn stats(&self) -> Result<QueryResult> {
        let started = Instant::now();
        // The requests the node is working on are read first and alone:
        // the other readings would be counted among them.
        let pools = self
            .optional("SELECT name, active_tasks, pending_tasks FROM system_views.thread_pools")
            .await;
        // The node answers the rest from its own memory and local tables.
        let (peers, tables, clients, limit, disk) = tokio::join!(
            self.small("SELECT peer FROM system.peers"),
            self.small("SELECT keyspace_name FROM system_schema.tables"),
            self.optional("SELECT COUNT(*) FROM system_views.clients"),
            self.optional(
                "SELECT value FROM system_views.settings WHERE name = 'native_transport_max_concurrent_connections'"
            ),
            self.optional("SELECT keyspace_name, mebibytes FROM system_views.disk_usage"),
        );
        let duration = started.elapsed().as_millis() as u64;
        let tables = tables?;
        let user_tables = tables
            .rows
            .iter()
            .filter(|row| {
                row.first()
                    .is_some_and(|cell| !catalog::is_system(&cell.display()))
            })
            .count();
        let keyspaces: HashSet<String> = tables
            .rows
            .iter()
            .filter_map(|row| row.first().map(Cell::display))
            .filter(|keyspace| !catalog::is_system(keyspace))
            .collect();
        let number = |cell: Option<&Cell>| cell.and_then(|cell| cell.display().parse::<i64>().ok());
        let requests = pools.as_ref().and_then(|grid| {
            grid.rows
                .iter()
                .find(|row| {
                    row.first()
                        .is_some_and(|name| name.display() == "Native-Transport-Requests")
                })
                .map(|row| {
                    (
                        number(row.get(1)).unwrap_or(0),
                        number(row.get(2)).unwrap_or(0),
                    )
                })
        });
        let keyspace = self
            .driver()?
            .get_keyspace()
            .map(|keyspace| keyspace.to_string())
            .unwrap_or_default();
        let mut columns: Vec<(&str, Cell)> = Vec::new();
        // The two large figures.
        if let Some(count) = clients
            .as_ref()
            .and_then(|grid| number(grid.rows.first()?.first()))
        {
            columns.push(("connections", Cell::Integer(count.to_string())));
        }
        if let Some((active, _)) = requests {
            // This reading is one of them.
            columns.push(("active", Cell::Integer((active - 1).max(0).to_string())));
        }
        columns.push(("nodes", Cell::Integer((peers?.rows.len() + 1).to_string())));
        columns.push(("version", Cell::Text(self.server.version.clone())));
        columns.push((
            "database",
            Cell::Text(if keyspace.is_empty() {
                "no keyspace selected".into()
            } else {
                keyspace
            }),
        ));
        columns.push(("tables", Cell::Integer(user_tables.to_string())));
        if let Some(limit) = limit
            .as_ref()
            .and_then(|grid| number(grid.rows.first()?.first()))
        {
            // Below one there is no limit, which the interface writes for 0.
            columns.push(("max_connections", Cell::Integer(limit.max(0).to_string())));
        }
        columns.push(("keyspaces", Cell::Integer(keyspaces.len().to_string())));
        if let Some((_, pending)) = requests {
            columns.push(("waiting requests", Cell::Integer(pending.to_string())));
        }
        if let Some(disk) = &disk {
            let used: i64 = disk
                .rows
                .iter()
                .filter(|row| {
                    row.first()
                        .is_some_and(|cell| !catalog::is_system(&cell.display()))
                })
                .filter_map(|row| number(row.get(1)))
                .sum();
            columns.push(("data on this node", Cell::Text(format!("{used} MiB"))));
        }
        columns.push(("cluster", Cell::Text(self.server.cluster.clone())));
        columns.push((
            "data centre",
            Cell::Text(if self.server.rack.is_empty() {
                self.server.datacenter.clone()
            } else {
                format!("{}, rack {}", self.server.datacenter, self.server.rack)
            }),
        ));
        columns.push(("consistency", Cell::Text(self.consistency.to_owned())));
        columns.push(("partitioner", Cell::Text(self.server.partitioner.clone())));
        columns.push(("CQL", Cell::Text(self.server.cql.clone())));
        // What the node does not report is left out, not shown empty.
        columns.retain(|(_, cell)| !matches!(cell, Cell::Text(text) if text.is_empty()));
        Ok(QueryResult {
            columns: columns
                .iter()
                .map(|(name, cell)| ResultColumn {
                    name: (*name).into(),
                    data_type: match cell {
                        Cell::Integer(_) => "bigint",
                        _ => "text",
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

impl Session for CassandraSession {
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
            // A refresh of the tree is also when the schema is read anew.
            self.forget().await;
            let mut containers = Vec::new();
            let mut list = |grid: Grid, kind: &str| {
                for row in grid.rows {
                    if let [keyspace, name, ..] = row.as_slice() {
                        containers.push(TableRef {
                            schema: keyspace.display(),
                            name: name.display(),
                            kind: kind.into(),
                        });
                    }
                }
            };
            list(
                self.small("SELECT keyspace_name, table_name FROM system_schema.tables")
                    .await?,
                "table",
            );
            // A service may have neither of these.
            if let Ok(views) = self
                .small("SELECT keyspace_name, view_name FROM system_schema.views")
                .await
            {
                list(views, "materialized view");
            }
            if self.show_system
                && let Ok(tables) = self
                    .small("SELECT keyspace_name, table_name FROM system_virtual_schema.tables")
                    .await
            {
                // Tables the node computes: read like views, never written.
                list(tables, "view");
            }
            containers.retain(|container| {
                self.show_system
                    || !catalog::is_system(&container.schema)
                    || container.schema == self.home
            });
            containers.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
            Ok(containers)
        }))
    }

    fn columns<'a>(&'a self, container: &'a TableRef) -> BoxFuture<'a, Result<Vec<ColumnInfo>>> {
        Box::pin(self.briefly(async move { Ok(self.table(container).await?.column_info()) }))
    }

    fn server_stats<'a>(&'a self) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(self.briefly(self.stats()))
    }

    fn close<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // The connections close when the last request holding the
            // driver is done.
            if let Ok(mut driver) = self.driver.write() {
                driver.take();
            }
        })
    }

    fn page_text(&self, page: &PageRequest) -> Result<String> {
        browse::page_text(self.remembered(&page.container).as_deref(), page)
    }

    fn read_page<'a>(
        &'a self,
        page: &'a PageRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(self.bounded(cancel, self.page(page)))
    }

    fn change_text(&self, change: &RowChange) -> Result<String> {
        edit::check_kind(change)?;
        // The grid described the table before it showed its rows.
        let table = self
            .remembered(edit::table_of(change))
            .ok_or_else(|| anyhow!("result_changed_reload_required"))?;
        edit::statement(&table, change)
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
                assert!(
                    text.contains(' ') && text.ends_with(['.', '…']),
                    "{code}: {text}"
                );
                assert!(!text.contains(code), "{code}");
            }
            assert_ne!(describe(code, false), describe(code, true), "{code}");
            // The interface takes it for a code and asks the driver.
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            );
        }
        assert_eq!(describe("database_login_rejected", true), None);
        assert_eq!(describe("cassandra_made_up", true), None);
    }

    #[test]
    fn the_products_are_presets_of_one_driver() {
        let spec = ENGINE.spec();
        assert_eq!(spec.generic().id, "cassandra");
        assert_eq!(Engine::from_id("ScyllaDB"), Some(ENGINE));
        assert_eq!(Engine::from_id("cql"), Some(ENGINE));
        let ids: Vec<&str> = spec.products.iter().map(|product| product.id).collect();
        assert_eq!(
            ids,
            [
                "cassandra",
                "scylladb",
                "amazon-keyspaces",
                "azure-managed-cassandra",
                "datastax-astra-db",
            ]
        );
        for product in spec.products {
            // Every product says what one connection reaches.
            assert!(product.note.is_some(), "{}", product.id);
            assert!(product.capabilities.is_none() && product.dialect.is_none());
        }
        let keyspaces = ENGINE.product("amazon-keyspaces");
        assert_eq!(
            (keyspaces.default_port, keyspaces.default_tls),
            (9142, TlsMode::VerifyFull)
        );
        assert!(keyspaces.matches("aws") && keyspaces.matches("keyspaces cql"));
        assert_eq!(ENGINE.product("cassandra").default_tls, TlsMode::Disable);
        let azure = ENGINE.product("azure-managed-cassandra");
        assert_eq!(
            (azure.default_tls, azure.default_username),
            (TlsMode::Require, "cassandra")
        );
        // CQL is typed into the SQL editor; what it cannot say is off.
        let caps = spec.capabilities;
        assert_eq!(caps.language, QueryLanguage::Sql);
        assert_eq!(caps.browse, BrowseModel::Tables);
        assert!(caps.schemas && caps.filter && caps.sort && caps.monitor);
        assert!(caps.edit_cells && caps.insert_rows && caps.delete_rows && caps.import);
        assert!(!caps.table_designer && !caps.query_builder);
        assert_eq!(spec.form.database, FieldUse::Optional);
        assert!(spec.form.tls && spec.form.ssh);
    }

    #[tokio::test]
    async fn a_profile_is_checked_before_anything_is_dialled() {
        let mut profile = crate::model::ConnectionProfile {
            name: "Astra".into(),
            engine: ENGINE,
            product: "datastax-astra-db".into(),
            host: "astra.example.test".into(),
            port: 29042,
            database: String::new(),
            ..crate::model::ConnectionProfile::default()
        };
        profile.validate().unwrap();
        assert_eq!(profile.option("consistency"), "local_quorum");
        assert_eq!(profile.option("system_keyspaces"), "false");
        // The bundle of Astra is not read: said before any connection.
        let failure = crate::database::Database::connect(&profile, None)
            .await
            .err()
            .map(|error| error.to_string());
        assert_eq!(failure.as_deref(), Some("cassandra_astra_unsupported"));
        profile.set_option("consistency", "serial");
        assert_eq!(
            profile.validate().unwrap_err().to_string(),
            "connection_option_invalid"
        );
    }

    #[test]
    fn the_editor_text_is_split_into_statements_with_batches_whole() {
        let text = "-- two rows\nINSERT INTO t (id) VALUES (1);\nBEGIN BATCH\n  INSERT INTO t (id) VALUES (2);\n  UPDATE t SET v = 'a;b' WHERE id = 2;\nAPPLY BATCH;\nSELECT * FROM t; -- done";
        let split = statements(text).unwrap();
        assert_eq!(split.len(), 3, "{split:?}");
        assert!(split[0].ends_with("VALUES (1)"));
        assert!(split[1].starts_with("BEGIN BATCH") && split[1].ends_with("APPLY BATCH"));
        assert_eq!(split[1].matches(";\n").count(), 2);
        assert_eq!(split[2], "SELECT * FROM t");
        assert_eq!(
            statements("BEGIN UNLOGGED BATCH INSERT INTO t (id) VALUES (1) APPLY BATCH")
                .unwrap()
                .len(),
            1
        );
        // Only comments: nothing to send.
        assert!(statements("-- nothing\n/* at all */").unwrap().is_empty());
        assert_eq!(
            statements("SELECT $$a;b$$ FROM t; SELECT 2").unwrap().len(),
            2
        );
        assert!(statements("SELECT 'open").is_err());
    }

    #[test]
    fn statements_the_driver_answers_itself_are_recognised() {
        for probe in [
            "SELECT 1",
            "select 1;",
            "  -- x\nSELECT 1 ;",
            "/* a */ SELECT 1",
        ] {
            assert!(is_probe(probe), "{probe}");
        }
        for other in [
            "SELECT 1 FROM t",
            "SELECT 10",
            "SELECT 1, 2",
            "SELECT * FROM t",
        ] {
            assert!(!is_probe(other), "{other}");
        }
        assert_eq!(use_target("USE shop"), Some(("shop".into(), false)));
        assert_eq!(use_target("use Shop;"), Some(("shop".into(), false)));
        assert_eq!(use_target("USE \"Shop\""), Some(("Shop".into(), true)));
        assert_eq!(
            use_target("-- pick\nUSE shop"),
            Some(("shop".into(), false))
        );
        assert_eq!(use_target("SELECT * FROM used"), None);
        assert_eq!(
            leading_words("/* c */ create   TABLE x", 2),
            ["CREATE", "TABLE"]
        );
    }

    #[test]
    fn the_node_says_what_it_is() {
        let local = Grid {
            columns: [
                "release_version",
                "cluster_name",
                "data_center",
                "rack",
                "partitioner",
                "cql_version",
            ]
            .iter()
            .map(|name| ResultColumn {
                name: (*name).into(),
                data_type: "text".into(),
            })
            .collect(),
            rows: vec![
                [
                    "5.0.9",
                    "Test Cluster",
                    "datacenter1",
                    "rack1",
                    "org.apache.cassandra.dht.Murmur3Partitioner",
                    "3.4.7",
                ]
                .iter()
                .map(|value| Cell::Text((*value).into()))
                .collect(),
            ],
        };
        let server = Server::of(&local, None, ENGINE.product("cassandra"));
        assert_eq!(server.version, "Apache Cassandra 5.0.9");
        assert_eq!(server.partitioner, "Murmur3Partitioner");
        assert_eq!(
            (server.cluster.as_str(), server.cql.as_str()),
            ("Test Cluster", "3.4.7")
        );
        // ScyllaDB reports the Cassandra version it is compatible with in
        // the same column; its own version is what is shown.
        let scylla = Server::of(&local, Some("6.2.1"), ENGINE.product("cassandra"));
        assert_eq!(scylla.version, "ScyllaDB 6.2.1");
        let keyspaces = Server::of(&local, None, ENGINE.product("amazon-keyspaces"));
        assert_eq!(
            keyspaces.version,
            "Amazon Keyspaces (Apache Cassandra 5.0.9)"
        );
        assert_eq!(
            Server::of(&Grid::default(), None, ENGINE.product("cassandra")).version,
            "Apache Cassandra"
        );
    }

    #[test]
    fn the_dialect_reads_back_what_the_driver_writes() {
        let known = [TableRef {
            schema: "shop".into(),
            name: "Order Items; -- x".into(),
            kind: "table".into(),
        }];
        for text in [
            "SELECT * FROM \"shop\".\"Order Items; -- x\"",
            "SELECT * FROM \"shop\".\"Order Items; -- x\" WHERE id = 7 ALLOW FILTERING",
            "SELECT * FROM shop.\"Order Items; -- x\" WHERE note = $$it's; here$$ LIMIT 5",
        ] {
            assert_eq!(crate::sql::statements(&DIALECT, text).unwrap().len(), 1);
            assert!(
                crate::sql::editable_table(&DIALECT, text, &known).is_some(),
                "{text}"
            );
        }
        assert!(!DIALECT.column_types.is_empty());
    }
}
