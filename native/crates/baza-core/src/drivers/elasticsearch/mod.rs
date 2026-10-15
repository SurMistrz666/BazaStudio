//! Elasticsearch, OpenSearch and the services that run them, over the REST
//! API of the server. No client library is involved: a request is a method,
//! a path and JSON ([`http`]).
//!
//! * **Indices** are the containers, next to their aliases and data
//!   streams (read only) and three overviews of the cluster ([`views`]).
//!   A document is a row: the fields of the mapping are its columns, by
//!   their dotted paths, and `_id` is its key ([`mapping`]).
//! * **Pages** are searches in a fixed order, read on with `search_after`
//!   inside a point in time; a filter is the query the type of its field
//!   answers ([`browse`]).
//! * **Changes** go by `_id` and are made only while the document is the
//!   one that was read (`if_seq_no`, `if_primary_term`); an import is a
//!   `_bulk` that is taken back when one of its documents is refused
//!   ([`edit`]).
//! * **The console** takes requests the way Kibana Dev Tools does, and the
//!   SQL either server offers ([`console`], [`sql`]); an answer becomes a
//!   grid in [`answer`].
//!
//! What a server is (Elasticsearch or OpenSearch, and which version) is
//! asked of the server when the session opens, and where the two differ
//! the answer decides: the point in time API, the tiebreaker of a page,
//! where the SQL is, how the elected node is called.
//!
//! What one action reads at most:
//!
//! * a page: the documents it was asked for (20 000 at most) and 64 MB of
//!   answers; a page that is cut at its bytes says so, and the next one
//!   goes on where it ended;
//! * an answer of the console and a result of SQL: 64 MB and 20 000 rows,
//!   with the limit said; a larger answer is refused, not read;
//! * a page nobody visited: 2 000 000 documents passed on the way, 5000 at
//!   a time and without their sources;
//! * the fields of a container: 1000 of its mapping, 64 more that only
//!   documents hold (looked for in 50 of them); a listing: 20 000 indices;
//! * an import: requests of 1000 documents or 8 MB.
use super::{PageRequest, RowChange, Session};
use crate::engine::{
    Badge, BrowseModel, Capabilities, ConnectRequest, ConnectionForm, Dialect, Engine, EngineSpec,
    FieldUse, Group, OptionField, OptionKind, ProductSpec, QueryLanguage,
};
use crate::model::{Cell, ColumnInfo, QueryResult, ResultColumn, TableRef, TlsMode};
use crate::sql::{MAX_RESULT_BYTES, MAX_RESULT_ROWS};
use anyhow::{Result, anyhow, bail, ensure};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

mod answer;
mod browse;
mod console;
mod edit;
mod http;
mod mapping;
mod sql;
mod views;

use browse::{Cursor, Cursors, Hit, Mark, Plan, Position, Tiebreak};
use http::{Client, Hint, Payload, Refusal};
use mapping::Layout;
use views::Task;

pub const ENGINE: Engine = Engine::new(&SPEC);

pub static SPEC: EngineSpec = EngineSpec {
    id: "elasticsearch",
    aliases: &["opensearch", "elastic", "es"],
    portable_name: "elasticsearch",
    label: "Elasticsearch",
    available: true,
    form: ConnectionForm {
        // A cluster has no databases: its indices hang under the connection.
        database: FieldUse::Hidden,
        ..ConnectionForm::SERVER
    },
    capabilities: Capabilities {
        // Requests of the API and SQL, typed into a console.
        language: QueryLanguage::Command,
        browse: BrowseModel::Documents,
        schemas: false,
        filter: true,
        condition: false,
        sort: true,
        sort_many: false,
        edit_cells: true,
        insert_rows: true,
        delete_rows: true,
        // An index is made of a mapping, analysers and settings, and a
        // query is not SQL with joins: both are typed in the console.
        table_designer: false,
        query_builder: false,
        import: true,
        export: true,
        monitor: true,
    },
    dialect: Dialect::ANSI,
    options: OPTIONS,
    products: PRODUCTS,
    connect,
    describe: Some(describe),
};

static OPTIONS: &[OptionField] = &[
    OptionField {
        key: "auth",
        label: ("Sposób logowania", "Sign-in method"),
        kind: OptionKind::Choice(&["auto", "basic", "api_key", "bearer"]),
        default: "auto",
        placeholder: "",
        help: Some((
            "auto: nazwa użytkownika z hasłem to logowanie Basic, a samo hasło bez nazwy to klucz API. api_key: w polu hasła zakodowany klucz API (albo identyfikator klucza jako nazwa użytkownika i jego sekret jako hasło). bearer: w polu hasła token dostępu.",
            "auto: a user name with a password signs in with Basic, and a password alone is an API key. api_key: the encoded API key in the password field (or the key id as the user name and its secret as the password). bearer: an access token in the password field.",
        )),
        required: false,
    },
    OptionField {
        key: "path_prefix",
        label: ("Prefiks ścieżki", "Path prefix"),
        kind: OptionKind::Text,
        default: "",
        placeholder: "/elasticsearch",
        help: Some((
            "Tylko gdy klaster stoi za serwerem pośredniczącym pod własną ścieżką. Zwykle puste.",
            "Only when the cluster is behind a proxy under a path of its own. Usually empty.",
        )),
        required: false,
    },
    OptionField {
        key: "show_hidden",
        label: (
            "Pokaż indeksy ukryte i systemowe",
            "Show hidden and system indices",
        ),
        kind: OptionKind::Flag,
        default: "false",
        placeholder: "",
        help: Some((
            "Dodaje do panelu indeksy o nazwach zaczynających się kropką (.kibana, .security, .ds-…) oraz indeksy oznaczone jako ukryte.",
            "Adds the indices whose names start with a dot (.kibana, .security, .ds-…) and the ones marked as hidden to the side panel.",
        )),
        required: false,
    },
];

/// The note of a product, with one sentence on the console after it: said
/// once here, under every product. A note is read in the connection dialog,
/// above the fields, so it stays short.
macro_rules! note {
    ($pl:literal, $en:literal) => {
        Some((
            concat!(
                $pl,
                " Konsola przyjmuje żądania jak w Kibana Dev Tools oraz SQL."
            ),
            concat!(
                $en,
                " The console takes requests as in Kibana Dev Tools, and SQL."
            ),
        ))
    };
}

static PRODUCTS: &[ProductSpec] = &[
    ProductSpec {
        id: "elasticsearch",
        label: "Elasticsearch",
        group: Group::Analytics,
        badge: Badge {
            text: "Es",
            color: [0, 191, 179],
        },
        default_port: 9200,
        // Since 8.0 a node encrypts with a certificate of its own authority.
        default_tls: TlsMode::Require,
        default_username: "elastic",
        keywords: &[
            "elastic",
            "elk",
            "search",
            "wyszukiwarka",
            "wyszukiwanie",
            "lucene",
            "kibana",
            "index",
            "indeks",
            "logs",
            "logi",
        ],
        note: note!(
            "Port 9200. Od wersji 8 węzeł szyfruje własnym certyfikatem: tryb „Wymagane” łączy bez sprawdzania, pełna weryfikacja wymaga pliku http_ca.crt serwera; wersja 7 i węzły bez zabezpieczeń: „Wyłączony”. Logowanie użytkownikiem i hasłem albo kluczem API w polu hasła (bez nazwy użytkownika).",
            "Port 9200. From version 8 a node encrypts with a certificate of its own: the mode “Required” connects without checking it, full verification needs the http_ca.crt file of the server; version 7 and nodes without security: “Disabled”. Sign in with a user and a password, or with an API key in the password field (no user name)."
        ),
        ..ProductSpec::GENERIC
    },
    ProductSpec {
        id: "opensearch",
        label: "OpenSearch",
        group: Group::Analytics,
        badge: Badge {
            text: "Os",
            color: [0, 94, 184],
        },
        default_port: 9200,
        // The demo certificates of an installation verify against nothing.
        default_tls: TlsMode::Require,
        default_username: "admin",
        keywords: &[
            "open search",
            "search",
            "wyszukiwarka",
            "wyszukiwanie",
            "lucene",
            "dashboards",
            "opendistro",
            "index",
            "indeks",
            "logs",
            "logi",
        ],
        note: note!(
            "Port 9200. Z wtyczką bezpieczeństwa: szyfrowanie i użytkownik z hasłem (domyślnie admin); certyfikaty demonstracyjne nie przechodzą weryfikacji, stąd tryb „Wymagane”. Bez wtyczki: tryb „Wyłączony” i puste pola logowania.",
            "Port 9200. With the security plugin: encryption and a user with a password (admin by default); the demo certificates do not pass verification, hence the mode “Required”. Without the plugin: the mode “Disabled” and empty sign-in fields."
        ),
        ..ProductSpec::GENERIC
    },
    ProductSpec {
        id: "amazon-opensearch-service",
        label: "Amazon OpenSearch Service",
        group: Group::Cloud,
        badge: Badge {
            text: "Ao",
            color: [140, 79, 255],
        },
        default_port: 443,
        default_tls: TlsMode::VerifyFull,
        keywords: &[
            "aws",
            "amazon",
            "open search",
            "elasticsearch service",
            "search",
            "wyszukiwarka",
            "managed",
            "chmura",
        ],
        note: note!(
            "Host to punkt końcowy domeny (search-….region.es.amazonaws.com), port 443. Logowanie tylko użytkownikiem głównym albo wewnętrznym szczegółowej kontroli dostępu (nazwa i hasło). Podpisywanie kluczami IAM (SigV4) i kolekcje Serverless nie są obsługiwane. Domena w sieci VPC: przez tunel SSH.",
            "The host is the endpoint of the domain (search-….region.es.amazonaws.com), port 443. Sign-in only as the master user or an internal user of fine-grained access control (name and password). Signing with IAM keys (SigV4) and Serverless collections are not supported. A domain inside a VPC: through an SSH tunnel."
        ),
        ..ProductSpec::GENERIC
    },
    ProductSpec {
        id: "elastic-cloud",
        label: "Elastic Cloud",
        group: Group::Cloud,
        badge: Badge {
            text: "Ec",
            color: [254, 197, 20],
        },
        default_port: 443,
        default_tls: TlsMode::VerifyFull,
        keywords: &[
            "elastic",
            "elasticsearch",
            "cloud",
            "chmura",
            "search",
            "wyszukiwarka",
            "serverless",
            "hosted",
            "managed",
        ],
        note: note!(
            "Host to punkt końcowy Elasticsearch z konsoli Elastic Cloud (można wkleić cały adres https://…), port 443. Logowanie kluczem API: pole użytkownika puste, w polu hasła zakodowany klucz; działa też użytkownik elastic z hasłem.",
            "The host is the Elasticsearch endpoint from the Elastic Cloud console (the whole https://… address may be pasted), port 443. Sign in with an API key: the user name empty, the encoded key in the password field; the user elastic with its password works as well."
        ),
        ..ProductSpec::GENERIC
    },
];

/// Longest a script of the console, a page or an import request may take.
const DEADLINE: Duration = Duration::from_secs(120);
/// Longest a change of the grid or a look at the cluster may take.
const SHORT_DEADLINE: Duration = Duration::from_secs(45);
/// How long the server gets to take a request to stop a task.
const STOP_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a point in time is kept after its last page.
const KEEP_ALIVE: &str = "3m";
/// How long the fields of a container are believed.
const LAYOUT_FRESH: Duration = Duration::from_secs(15);
/// Documents looked at to find fields without a mapping.
const SAMPLE: usize = 50;
/// Hits passed at once on the way to a page nobody visited yet.
const WALK_HITS: u64 = 5000;
/// Most such steps: a page is reached up to 2 000 000 documents deep.
const WALK_STEPS: usize = 400;
/// Bytes of an answer that is not a page or a result: listings, mappings.
const ANSWER_BYTES: usize = 32 * 1024 * 1024;
/// Most containers a listing returns.
const MOST_CONTAINERS: usize = 20_000;
/// Nodes and tasks the monitor names.
const MONITOR_NODES: usize = 12;
const MONITOR_TASKS: usize = 5;

/// The codes of this driver and their sentences.
const SENTENCES: &[(&str, (&str, &str))] = &[
    (
        "elasticsearch_endpoint_invalid",
        (
            "Adres serwera jest nieprawidłowy. W polu Host wpisz nazwę albo adres IP, ewentualnie cały adres w postaci https://host:port, bez konta, parametrów i znaków specjalnych; prefiks ścieżki może zawierać tylko litery, cyfry i znaki - _ . /.",
            "The address of the server is not valid. Enter a name or an IP address in the Host field, or a whole address of the form https://host:port, without an account, a query or special characters; a path prefix may hold only letters, digits and - _ . /.",
        ),
    ),
    (
        "elasticsearch_endpoint_tls_mismatch",
        (
            "Adres w polu Host i karta SSL/TLS mówią co innego. Dla adresu http:// wybierz tryb „Wyłączony”, a dla https:// tryb z szyfrowaniem.",
            "The address in the Host field and the SSL/TLS tab disagree. For an http:// address choose the mode “Disabled”, for https:// a mode with encryption.",
        ),
    ),
    (
        "elasticsearch_endpoint_url_tunnel",
        (
            "Przez tunel SSH pole Host musi zawierać samą nazwę albo adres serwera (bez https://): tunel jest zestawiany do tego, co stoi w polach Host i Port.",
            "Through an SSH tunnel the Host field has to hold the name or the address of the server alone (without https://): the tunnel is opened to what the Host and Port fields say.",
        ),
    ),
    (
        "elasticsearch_credentials_invalid",
        (
            "Danych logowania nie da się przesłać: nazwa użytkownika nie może być pusta ani zawierać dwukropka, a hasło, klucz API i token nie mogą zawierać znaków nowej linii.",
            "The sign-in data cannot be sent: the user name may not be empty or hold a colon, and a password, an API key or a token may not hold line breaks.",
        ),
    ),
    (
        "elasticsearch_credentials_required",
        (
            "Wybrany sposób logowania wymaga klucza API albo tokenu w polu hasła.",
            "The chosen sign-in method needs an API key or a token in the password field.",
        ),
    ),
    (
        "elasticsearch_not_search_api",
        (
            "Serwer pod tym adresem nie odpowiada jak Elasticsearch ani OpenSearch. Sprawdź host, port (zwykle 9200, w chmurze 443) i prefiks ścieżki.",
            "The server at this address does not answer like Elasticsearch or OpenSearch. Check the host, the port (usually 9200, 443 in a cloud) and the path prefix.",
        ),
    ),
    (
        "elasticsearch_answer_unreadable",
        (
            "Odpowiedzi serwera nie udało się odczytać. Spróbuj ponownie.",
            "The answer of the server could not be read. Try again.",
        ),
    ),
    (
        "elasticsearch_answer_too_large",
        (
            "Odpowiedź serwera przekracza 64 MB i nie została wczytana. Poproś o mniej dokumentów (size) albo o mniej pól (_source, filter_path).",
            "The answer of the server is larger than 64 MB and was not read. Ask for fewer documents (size) or fewer fields (_source, filter_path).",
        ),
    ),
    (
        "elasticsearch_path_invalid",
        (
            "Ścieżka żądania jest nieprawidłowa. Podaj metodę i ścieżkę interfejsu API, na przykład GET indeks/_search, bez adresu serwera, znaku # i przejść do katalogu nadrzędnego.",
            "The path of the request is not valid. Give a method and a path of the API, for example GET index/_search, without the address of a server, a # or steps to a parent folder.",
        ),
    ),
    (
        "elasticsearch_index_unknown",
        (
            "Nie ma takiego indeksu, aliasu ani strumienia danych. Sprawdź nazwę i odśwież listę w panelu.",
            "There is no such index, alias or data stream. Check the name and refresh the list in the side panel.",
        ),
    ),
    (
        "elasticsearch_index_closed",
        (
            "Ten indeks jest zamknięty: nie da się go czytać ani zmieniać. Otwórz go żądaniem POST indeks/_open w konsoli.",
            "This index is closed: it can be neither read nor changed. Open it with POST index/_open in the console.",
        ),
    ),
    (
        "elasticsearch_index_exists",
        (
            "Indeks, alias albo inny zasób o tej nazwie już istnieje.",
            "An index, an alias or another resource with this name already exists.",
        ),
    ),
    (
        "elasticsearch_name_invalid",
        (
            "Serwer nie przyjmuje tej nazwy indeksu: małe litery, bez spacji i znaków \\ / * ? \" < > | , #, nie od znaków - _ +.",
            "The server does not take this index name: lower-case letters, no spaces and none of \\ / * ? \" < > | , #, not starting with - _ +.",
        ),
    ),
    (
        "elasticsearch_not_found",
        (
            "Serwer nie znalazł tego, o co pyta żądanie: dokumentu, zadania, szablonu albo innego zasobu o tej nazwie.",
            "The server did not find what the request asks for: a document, a task, a template or another resource with this name.",
        ),
    ),
    (
        "elasticsearch_request_invalid",
        (
            "Serwer odrzucił żądanie jako nieprawidłowe. Sprawdź nazwy pól i parametrów, typy wartości oraz budowę zapytania w treści JSON.",
            "The server refused the request as invalid. Check the names of fields and parameters, the types of the values and the shape of the query in the JSON body.",
        ),
    ),
    (
        "elasticsearch_api_unknown",
        (
            "Serwer nie ma takiego interfejsu API albo nie przyjmuje go tą metodą. Sprawdź metodę (GET, POST, PUT, DELETE) i ścieżkę; Elasticsearch i OpenSearch różnią się częścią ścieżek, a starsze wersje nie mają nowszych interfejsów.",
            "The server has no such API, or not for this method. Check the method (GET, POST, PUT, DELETE) and the path; Elasticsearch and OpenSearch differ in some paths, and older versions lack newer APIs.",
        ),
    ),
    (
        "elasticsearch_forbidden",
        (
            "To konto albo ten klucz API nie ma na serwerze uprawnień do tej operacji.",
            "This account or API key is not allowed to do this on the server.",
        ),
    ),
    (
        "elasticsearch_blocked",
        (
            "Indeks albo klaster jest zablokowany do zapisu: indeks tylko do odczytu albo dysk węzła zapełniony ponad próg. Zwolnij miejsce albo zdejmij blokadę w ustawieniach indeksu (index.blocks).",
            "The index or the cluster is blocked for writes: a read-only index, or a node whose disk is filled past its watermark. Free space or lift the block in the index settings (index.blocks).",
        ),
    ),
    (
        "elasticsearch_overloaded",
        (
            "Klaster odrzucił żądanie, bo jest przeciążony albo zabrakło mu pamięci na tę operację. Spróbuj ponownie za chwilę albo zawęź zapytanie.",
            "The cluster refused the request because it is overloaded or short of memory for it. Try again in a moment, or narrow the query.",
        ),
    ),
    (
        "elasticsearch_unavailable",
        (
            "Klaster jest chwilowo niedostępny: nie ma wybranego węzła głównego albo część danych indeksu nie jest przydzielona. Spróbuj ponownie za chwilę.",
            "The cluster is unavailable for the moment: no elected master, or a part of the index is not allocated. Try again shortly.",
        ),
    ),
    (
        "elasticsearch_shards_failed",
        (
            "Część indeksu nie odpowiedziała, więc wynik byłby niepełny. Sprawdź stan klastra w sekcji Monitor i spróbuj ponownie.",
            "A part of the index did not answer, so the result would be incomplete. Check the state of the cluster in the Monitor section and try again.",
        ),
    ),
    (
        "elasticsearch_version_conflict",
        (
            "Dokument został w międzyczasie zmieniony albo dokument o tym identyfikatorze już istnieje, więc serwer odrzucił zapis.",
            "The document was changed in the meantime, or a document with this id already exists, so the server refused the write.",
        ),
    ),
    (
        "elasticsearch_value_rejected",
        (
            "Wartość nie pasuje do typu pola w mapowaniu indeksu (na przykład tekst w polu liczbowym albo data w innym formacie) albo mapowanie nie pozwala na nowe pola. Nic nie zostało zapisane.",
            "The value does not fit the type of its field in the mapping of the index (a text in a number field, a date in another format), or the mapping does not allow new fields. Nothing was written.",
        ),
    ),
    (
        "elasticsearch_value_invalid",
        (
            "Tej wartości nie da się zapisać w polu tego typu: w polu liczbowym wpisz liczbę, w polu logicznym true albo false, a obiekty i listy jako poprawny JSON.",
            "This value cannot be written into a field of this type: enter a number in a number field, true or false in a boolean field, and objects and lists as valid JSON.",
        ),
    ),
    (
        "elasticsearch_id_invalid",
        (
            "Identyfikator dokumentu (_id) musi być niepustym tekstem do 512 bajtów. Zostaw pole pominięte, a serwer nada identyfikator sam.",
            "The id of a document (_id) has to be a text of 1 to 512 bytes. Leave the field omitted and the server makes an id itself.",
        ),
    ),
    (
        "elasticsearch_id_read_only",
        (
            "Identyfikatora dokumentu (_id) nie da się zmienić. Dodaj dokument z nowym identyfikatorem i usuń stary.",
            "The id of a document (_id) cannot be changed. Add a document with the new id and delete the old one.",
        ),
    ),
    (
        "elasticsearch_id_not_unique",
        (
            "Ten identyfikator mają w indeksie dwa dokumenty (zapisane z różnym routingiem), więc siatka nie wskaże jednego z nich. Zmień albo usuń dokument żądaniem w konsoli, podając parametr routing.",
            "Two documents of the index have this id (written with different routing), so the grid cannot point at one of them. Change or delete the document with a request in the console, giving the routing parameter.",
        ),
    ),
    (
        "elasticsearch_document_exists",
        (
            "Dokument o tym identyfikatorze już istnieje. Zmień istniejący dokument albo podaj inny identyfikator.",
            "A document with this id already exists. Change the existing document or give another id.",
        ),
    ),
    (
        "elasticsearch_field_in_list",
        (
            "To pole leży wewnątrz listy obiektów, więc jedna komórka pokazuje wartości wielu elementów i nie da się jej zmienić osobno. Zmień całą listę w jej kolumnie albo użyj żądania _update w konsoli.",
            "This field lies inside a list of objects, so one cell shows the values of several elements and cannot be changed by itself. Change the whole list in its own column, or use an _update request in the console.",
        ),
    ),
    (
        "elasticsearch_read_only_object",
        (
            "Tego obiektu nie zmienia się z siatki: alias, strumień danych i zestawienia klastra są tylko do odczytu. Dokumenty zmienia się w indeksie, w którym leżą.",
            "This object is not changed from the grid: an alias, a data stream and the overviews of the cluster are read only. Documents are changed in the index they are in.",
        ),
    ),
    (
        "elasticsearch_filter_number",
        (
            "To pole przechowuje liczby: wpisz liczbę, zakres (10..20) albo porównanie (>= 10, < 20). Fragmentu liczby serwer nie wyszukuje.",
            "This field holds numbers: enter a number, an interval (10..20) or a comparison (>= 10, < 20). The server does not search for a part of a number.",
        ),
    ),
    (
        "elasticsearch_filter_date",
        (
            "To pole przechowuje daty: wpisz datę w zapisie ISO, pełną albo skróconą (2026, 2026-10, 2026-10-12 08:30), zakres (2026-10-01..2026-10-15) albo porównanie (>= 2026-10-01, > now-7d). Skrócona data oznacza cały swój okres.",
            "This field holds dates: enter an ISO date, whole or cut short (2026, 2026-10, 2026-10-12 08:30), an interval (2026-10-01..2026-10-15) or a comparison (>= 2026-10-01, > now-7d). A date cut short stands for its whole period.",
        ),
    ),
    (
        "elasticsearch_filter_boolean",
        (
            "To pole przechowuje wartości logiczne: wpisz true albo false.",
            "This field holds booleans: enter true or false.",
        ),
    ),
    (
        "elasticsearch_filter_ip",
        (
            "To pole przechowuje adresy IP: wpisz cały adres (10.0.0.7) albo blok adresów (10.0.0.0/8).",
            "This field holds IP addresses: enter a whole address (10.0.0.7) or a block of addresses (10.0.0.0/8).",
        ),
    ),
    (
        "elasticsearch_filter_unsupported",
        (
            "Po polu tego typu (obiekt, lista zagnieżdżona, położenie, dane binarne) siatka nie filtruje w ten sposób. Dla pola text bez podpola keyword działają „zawiera” i „równe”, ale nie porównania. Zapytanie można napisać w konsoli.",
            "The grid does not filter a field of this type (an object, a nested list, a location, binary data) this way. For a text field without a keyword sub-field “contains” and “equals” work, comparisons do not. The query can be written in the console.",
        ),
    ),
    (
        "elasticsearch_filter_unmapped",
        (
            "Tego pola nie ma w mapowaniu indeksu, więc serwer go nie przeszukuje: widać je tylko w dokumentach. Dodaj pole do mapowania, aby po nim filtrować.",
            "This field is not in the mapping of the index, so the server does not search it: it only shows in the documents. Add the field to the mapping to filter by it.",
        ),
    ),
    (
        "elasticsearch_sort_unsupported",
        (
            "Po tej kolumnie serwer nie porządkuje. Sortować można pola keyword, liczbowe, daty, logiczne i IP; pole text tylko wtedy, gdy ma podpole keyword. Po _id, obiektach i polach spoza mapowania sortowania nie ma.",
            "The server does not order by this column. Keyword, number, date, boolean and IP fields can be ordered; a text field only when it has a keyword sub-field. There is no order by _id, by objects or by fields outside the mapping.",
        ),
    ),
    (
        "elasticsearch_result_window",
        (
            "Ten indeks pozwala pobrać naraz mniej trafień, niż potrzebuje strona (ustawienie index.max_result_window). Zwiększ to ustawienie albo czytaj indeks zapytaniem w konsoli.",
            "This index lets fewer hits be fetched at once than a page needs (the setting index.max_result_window). Raise the setting, or read the index with a request in the console.",
        ),
    ),
    (
        "elasticsearch_page_too_deep",
        (
            "Ta strona leży zbyt głęboko: serwer nie przeskakuje do dowolnego miejsca, więc trzeba przeczytać wszystko przed nią (najwyżej 2 000 000 dokumentów). Wróć na początek i zawęź dane filtrem.",
            "This page is too deep: the server cannot jump to an arbitrary place, so everything before it has to be read (2,000,000 documents at most). Go back to the start and narrow the data with a filter.",
        ),
    ),
    (
        "elasticsearch_sql_unavailable",
        (
            "Ten serwer nie udostępnia SQL: Elasticsearch ma go pod ścieżką _sql, a OpenSearch we wtyczce SQL (_plugins/_sql). Użyj żądań interfejsu API, na przykład GET indeks/_search.",
            "This server offers no SQL: Elasticsearch has it under _sql and OpenSearch in its SQL plugin (_plugins/_sql). Use requests of the API instead, for example GET index/_search.",
        ),
    ),
    (
        "elasticsearch_sql_invalid",
        (
            "Serwer nie przyjął polecenia SQL: błąd składni, nieznany indeks albo kolumna, albo konstrukcja, której SQL tego serwera nie obsługuje. Nazwy z myślnikiem lub kropką pisz w cudzysłowie (Elasticsearch: \"nazwa\", OpenSearch: `nazwa`).",
            "The server did not take the SQL statement: a syntax error, an unknown index or column, or a construct the SQL of this server does not support. Write names with a hyphen or a dot in quotes (Elasticsearch: \"name\", OpenSearch: `name`).",
        ),
    ),
    (
        console::SYNTAX,
        (
            "Tego nie da się odczytać jako żądania. Żądanie to metoda i ścieżka w jednym wierszu (GET indeks/_search), a w kolejnych wierszach jego treść JSON; polecenie SQL zaczyna się od SELECT, SHOW, DESCRIBE albo EXPLAIN. Komentarz zaczyna się od # albo //.",
            "This cannot be read as a request. A request is a method and a path on one line (GET index/_search) and its JSON body on the lines after it; an SQL statement starts with SELECT, SHOW, DESCRIBE or EXPLAIN. A comment starts with # or //.",
        ),
    ),
    (
        console::BODY,
        (
            "Treść żądania nie jest poprawnym JSON-em: sprawdź nawiasy, przecinki i cudzysłowy (nazwy i teksty w \"podwójnych\"). Po treści w tym samym wierszu może stać tylko komentarz.",
            "The body of the request is not valid JSON: check the brackets, the commas and the quotes (names and texts in \"double\" ones). Only a comment may follow a body on its line.",
        ),
    ),
    (
        console::CONFIRMATION,
        (
            "To żądanie usuwa cały indeks, strumień danych albo migawkę, albo wszystkie dokumenty indeksu. Nic nie zostało wykonane. Jeśli tego chcesz, wpisz przed nim słowo confirm, na przykład: confirm DELETE nazwa-indeksu.",
            "This request removes a whole index, a data stream or a snapshot, or every document of an index. Nothing was run. If that is what you want, write the word confirm in front of it, for example: confirm DELETE index-name.",
        ),
    ),
    (
        "elasticsearch_import_duplicate",
        (
            "Dwa wiersze pliku mają ten sam identyfikator (_id). Nic nie zostało zapisane.",
            "Two rows of the file have the same id (_id). Nothing was written.",
        ),
    ),
    (
        "elasticsearch_import_id_exists",
        (
            "Co najmniej jeden dokument pliku ma identyfikator (_id), który już istnieje w indeksie. Import dodaje dokumenty i niczego nie nadpisuje, więc nic nie zostało zapisane. Usuń te wiersze z pliku albo dokumenty z indeksu.",
            "At least one document of the file has an id (_id) that already exists in the index. An import adds documents and overwrites nothing, so nothing was written. Remove those rows from the file or the documents from the index.",
        ),
    ),
    (
        "elasticsearch_import_value_rejected",
        (
            "Serwer odrzucił dokument importu: wartość nie pasuje do typu pola w mapowaniu indeksu. Dokumenty zapisane wcześniej w tym imporcie zostały usunięte, więc indeks jest taki jak przed importem.",
            "The server refused a document of the import: a value does not fit the type of its field in the mapping of the index. The documents this import had written before were removed again, so the index is as it was.",
        ),
    ),
    (
        "elasticsearch_import_refused",
        (
            "Serwer odrzucił dokument importu. Dokumenty zapisane wcześniej w tym imporcie zostały usunięte, więc indeks jest taki jak przed importem.",
            "The server refused a document of the import. The documents this import had written before were removed again, so the index is as it was.",
        ),
    ),
    (
        "elasticsearch_import_incomplete",
        (
            "Import został przerwany w trakcie zapisu i nie wszystkie zapisane już dokumenty udało się usunąć. Indeks nie ma transakcji: odśwież dane i sprawdź, co zostało.",
            "The import was interrupted while it was writing, and not every document it had written could be removed again. An index has no transactions: reload the data and check what is left.",
        ),
    ),
];

/// A sentence with the place it is about in front, kept for as long as the
/// process runs: `describe` returns text that lives for ever. At most a few
/// hundred short ones are made, then the plain sentence stands.
fn placed(
    code: &str,
    en: bool,
    make: impl FnOnce() -> String,
    plain: &'static str,
) -> &'static str {
    type Kept = Mutex<HashMap<(String, bool), &'static str>>;
    static KEPT: OnceLock<Kept> = OnceLock::new();
    let Ok(mut kept) = KEPT.get_or_init(Default::default).lock() else {
        return plain;
    };
    if let Some(found) = kept.get(&(code.to_owned(), en)) {
        return found;
    }
    if kept.len() >= 400 {
        return plain;
    }
    let made: &'static str = Box::leak(make().into_boxed_str());
    kept.insert((code.to_owned(), en), made);
    made
}

/// Sentences for the result codes of this driver. A code may end in the
/// line of the console (`…_at_line_3`) or the row of a file
/// (`…_at_row_12`) it is about.
fn describe(code: &str, en: bool) -> Option<&'static str> {
    let sentence = |code: &str| {
        SENTENCES
            .iter()
            .find(|(known, _)| *known == code)
            .map(|(_, (pl, english))| if en { *english } else { *pl })
    };
    for (mark, pl, english) in [
        ("_at_line_", "Wiersz", "Line"),
        ("_at_row_", "Wiersz pliku nr", "Row"),
    ] {
        if let Some((base, number)) = code.rsplit_once(mark)
            && let Ok(number) = number.parse::<u32>()
            && let Some(text) = sentence(base)
        {
            return Some(placed(
                code,
                en,
                || format!("{} {number}: {text}", if en { english } else { pl }),
                text,
            ));
        }
    }
    sentence(code)
}

/// What a refusal of the server is called.
fn code_of(refusal: &Refusal) -> String {
    let kind = refusal.kind.as_str();
    let code = match (refusal.status, kind) {
        (_, "index_not_found_exception" | "IndexNotFoundException") => {
            "elasticsearch_index_unknown"
        }
        (_, "index_closed_exception") => "elasticsearch_index_closed",
        (_, "resource_already_exists_exception") => "elasticsearch_index_exists",
        (_, "invalid_index_name_exception") => "elasticsearch_name_invalid",
        (_, "version_conflict_engine_exception") => "elasticsearch_version_conflict",
        (
            _,
            "mapper_parsing_exception"
            | "document_parsing_exception"
            | "strict_dynamic_mapping_exception",
        ) => "elasticsearch_value_rejected",
        (_, "cluster_block_exception") => "elasticsearch_blocked",
        (_, "task_cancelled_exception") => "query_cancelled",
        (401, _) => "database_login_rejected",
        (403, _) | (_, "security_exception") => "elasticsearch_forbidden",
        (429, _)
        | (
            _,
            "circuit_breaking_exception"
            | "es_rejected_execution_exception"
            | "rejected_execution_exception",
        ) => "elasticsearch_overloaded",
        _ if refusal.hint == Hint::NoHandler => "elasticsearch_api_unknown",
        _ if refusal.hint == Hint::ResultWindow => "elasticsearch_result_window",
        _ if refusal.hint == Hint::Fielddata => "elasticsearch_sort_unsupported",
        (405, _) => "elasticsearch_api_unknown",
        (
            _,
            "parsing_exception"
            | "x_content_parse_exception"
            | "x_content_e_o_f_exception"
            | "json_parse_exception"
            | "json_e_o_f_exception"
            | "not_x_content_exception"
            | "illegal_argument_exception"
            | "action_request_validation_exception"
            | "query_shard_exception"
            | "parse_exception"
            | "number_format_exception"
            | "input_coercion_exception"
            | "date_time_parse_exception"
            | "media_type_header_exception"
            | "mapper_exception",
        ) => "elasticsearch_request_invalid",
        (
            _,
            "verification_exception"
            | "sql_illegal_argument_exception"
            | "SyntaxCheckException"
            | "SemanticCheckException"
            | "SqlParseException"
            | "SQLFeatureNotSupportedException"
            | "SQLFeatureDisabledException"
            | "ExpressionEvaluationException"
            | "QueryEngineException"
            | "UnsupportedCursorRequestException",
        ) => "elasticsearch_sql_invalid",
        (503, _)
        | (
            _,
            "master_not_discovered_exception"
            | "cluster_manager_not_discovered_exception"
            | "no_shard_available_action_exception"
            | "unavailable_shards_exception"
            | "node_not_connected_exception",
        ) => "elasticsearch_unavailable",
        (404, _) => "elasticsearch_not_found",
        (status, "") => return format!("database_error [HTTP {status}]"),
        (_, other) => return format!("database_error [{other}]"),
    };
    code.to_owned()
}

/// What the server said about itself when the session was opened.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Flavor {
    opensearch: bool,
    /// Major, minor and patch; zeros when the server would not say.
    version: (u32, u32, u32),
    /// The version as text, for the monitor.
    described: String,
    cluster: String,
}

impl Flavor {
    /// Read the answer to `GET /`; nothing when it is not a search server.
    fn of(answer: &Value) -> Option<Flavor> {
        let version = answer["version"].as_object()?;
        let number = version.get("number")?.as_str()?;
        let mut parts = number
            .split(['.', '-'])
            .map(|part| part.parse::<u32>().unwrap_or(0));
        let tagline = answer["tagline"].as_str().unwrap_or_default();
        let opensearch = version
            .get("distribution")
            .and_then(Value::as_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("opensearch"))
            || tagline.contains("OpenSearch");
        // Something else that answers with a version in JSON is not one.
        if !(opensearch || tagline.contains("Search") || version.contains_key("lucene_version")) {
            return None;
        }
        let lucene = version
            .get("lucene_version")
            .and_then(Value::as_str)
            .map(|lucene| format!(", Lucene {lucene}"))
            .unwrap_or_default();
        let build = match version.get("build_flavor").and_then(Value::as_str) {
            Some(flavor) if flavor != "default" => format!(" ({flavor})"),
            _ => String::new(),
        };
        Some(Flavor {
            opensearch,
            version: (
                parts.next().unwrap_or(0),
                parts.next().unwrap_or(0),
                parts.next().unwrap_or(0),
            ),
            described: format!(
                "{} {number}{build}{lucene}",
                if opensearch {
                    "OpenSearch"
                } else {
                    "Elasticsearch"
                }
            ),
            cluster: answer["cluster_name"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        })
    }

    /// What is assumed of a server that would not say what it is: the
    /// product of the profile, in a current version.
    fn guessed(product: &str) -> Flavor {
        let opensearch = product.contains("opensearch");
        Flavor {
            opensearch,
            version: (0, 0, 0),
            described: if opensearch {
                "OpenSearch"
            } else {
                "Elasticsearch"
            }
            .to_owned(),
            cluster: String::new(),
        }
    }

    fn at_least(&self, major: u32, minor: u32) -> bool {
        self.version == (0, 0, 0) || (self.version.0, self.version.1) >= (major, minor)
    }

    /// The server has a point in time API.
    fn has_pit(&self) -> bool {
        if self.opensearch {
            self.at_least(2, 4)
        } else {
            self.at_least(7, 10)
        }
    }

    /// A point in time can be ordered by `_shard_doc`.
    fn has_shard_doc(&self) -> bool {
        !self.opensearch && self.at_least(7, 12)
    }
}

/// The fields of a container, and when they were read.
struct Known {
    layout: Arc<Layout>,
    at: Instant,
}

/// What reading on from a mark gave.
struct Read {
    hits: Vec<Hit>,
    next: Position,
    /// The read stopped at the bytes one page may hold.
    capped: bool,
}

/// A document as the index holds it now.
struct Located {
    index: String,
    routing: Option<String>,
    seq_no: u64,
    primary_term: u64,
    source: Value,
}

struct SearchSession {
    client: Client,
    flavor: Flavor,
    show_hidden: bool,
    known: Mutex<HashMap<String, Known>>,
    /// The kinds of the containers of the last listing, by name.
    kinds: Mutex<HashMap<String, String>>,
    cursors: Mutex<Cursors>,
    /// Whether the server opens a point in time: 0 not tried, 1 yes, 2 no.
    pit: AtomicU8,
    /// `_shard_doc` was refused: the server orders by `_doc` only.
    no_shard_doc: AtomicBool,
    /// Where the SQL of the server turned out to be.
    sql: Mutex<Option<sql::Api>>,
}

fn connect<'a>(request: &'a ConnectRequest<'a>) -> BoxFuture<'a, Result<Arc<dyn Session>>> {
    Box::pin(async move { Ok(Arc::new(open(request).await?) as Arc<dyn Session>) })
}

/// Open a session: reach the server, sign in and learn what it is.
async fn open(request: &ConnectRequest<'_>) -> Result<SearchSession> {
    let client = Client::new(request).await?;
    let reply = tokio::time::timeout(
        SHORT_DEADLINE,
        client.send("GET", "", None, None, 1024 * 1024),
    )
    .await
    .map_err(|_| anyhow!("database_connection_timeout"))??;
    let flavor = match reply.status {
        401 => bail!("database_login_rejected"),
        // An account that may not ask the root still works with what it
        // may see: the product of the profile says what the server is.
        403 => Flavor::guessed(request.product.id),
        200..=299 => reply
            .json()
            .as_ref()
            .and_then(Flavor::of)
            .ok_or_else(|| anyhow!("elasticsearch_not_search_api"))?,
        _ => match http::refusal(reply.status, &reply.body) {
            Some(refusal) if refusal.status == 429 || refusal.status >= 500 => {
                return Err(anyhow!(code_of(&refusal)));
            }
            _ => bail!("elasticsearch_not_search_api"),
        },
    };
    Ok(SearchSession {
        client,
        flavor,
        show_hidden: request.option("show_hidden") == "true",
        known: Mutex::default(),
        kinds: Mutex::default(),
        cursors: Mutex::default(),
        pit: AtomicU8::new(0),
        no_shard_doc: AtomicBool::new(false),
        sql: Mutex::new(None),
    })
}

/// How a run ended, before its futures are gone.
enum Ended<T> {
    Done(T),
    Cancelled,
    TimedOut,
}

/// The name the work of one action is listed under on the server.
fn opaque() -> String {
    format!("baza-{}", uuid::Uuid::new_v4().simple())
}

/// The connection test of the application sends this statement to every
/// engine; here it is answered by asking the root of the API.
fn is_probe(text: &str) -> bool {
    text.trim()
        .trim_end_matches(';')
        .trim_end()
        .eq_ignore_ascii_case("select 1")
}

/// A code with the line of the console it is about.
fn at_line(code: &str, line: usize) -> anyhow::Error {
    anyhow!("{code}_at_line_{line}")
}

impl SearchSession {
    /// One request of the driver itself, answered in JSON.
    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        opaque: &str,
        most: usize,
    ) -> Result<Value> {
        self.client
            .send(
                method,
                path,
                body.map(|body| Payload::Json(body.to_string().into_bytes())),
                Some(opaque),
                most,
            )
            .await?
            .accepted()
    }

    /// Run work that can be stopped: when it is cancelled or its time is
    /// up, the request is dropped, which closes its connection, and what
    /// the server still runs for it is stopped through the task API.
    async fn bounded<T>(
        &self,
        cancel: &CancellationToken,
        opaque: &str,
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
            Ended::Done(result) => result.map_err(http::plain),
            Ended::Cancelled => {
                self.stop(opaque).await;
                bail!("query_cancelled")
            }
            Ended::TimedOut => {
                self.stop(opaque).await;
                bail!("query_timeout")
            }
        }
    }

    async fn briefly<T>(&self, work: impl Future<Output = Result<T>>) -> Result<T> {
        match tokio::time::timeout(SHORT_DEADLINE, work).await {
            Ok(result) => result.map_err(http::plain),
            Err(_) => bail!("database_connection_timeout"),
        }
    }

    /// Stop what the server runs under an opaque id: the tasks that carry
    /// it are looked up and cancelled, parents first (their children go
    /// with them). Best effort, and short: the caller is waiting.
    async fn stop(&self, opaque: &str) {
        let _ = tokio::time::timeout(STOP_TIMEOUT, async {
            let Ok(reply) = self
                .client
                .send_now("GET", "_tasks?detailed=false&group_by=none", None)
                .await
            else {
                return;
            };
            let Some(answer) = reply.json() else {
                return;
            };
            for task in views::tasks(&answer) {
                if task.opaque.as_deref() == Some(opaque)
                    && task.cancellable
                    && task.parent.is_none()
                {
                    let path = format!("_tasks/{}/_cancel", http::segment(&task.id));
                    let _ = self.client.send_now("POST", &path, None).await;
                }
            }
        })
        .await;
    }

    /// The kind a container was listed with.
    fn kind_of(&self, name: &str) -> Option<String> {
        self.kinds.lock().ok()?.get(name).cloned()
    }

    /// The containers of the cluster: indices, aliases, data streams.
    async fn list(&self, opaque: &str) -> Result<Vec<TableRef>> {
        let wildcards = if self.show_hidden {
            "all"
        } else {
            "open,closed"
        };
        let shown = |name: &str| self.show_hidden || !name.starts_with('.');
        let entry = |name: &str, kind: &str| TableRef {
            schema: String::new(),
            name: name.to_owned(),
            kind: kind.to_owned(),
        };
        let mut containers = Vec::new();
        let resolved = self
            .call(
                "GET",
                &format!("_resolve/index/*?expand_wildcards={wildcards}"),
                None,
                opaque,
                ANSWER_BYTES,
            )
            .await;
        match resolved {
            Ok(answer) => {
                for index in answer["indices"].as_array().into_iter().flatten() {
                    let Some(name) = index["name"].as_str() else {
                        continue;
                    };
                    let attributes: Vec<&str> = index["attributes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    // The indices behind a data stream are part of it.
                    let backing = index["data_stream"].is_string();
                    if !self.show_hidden
                        && (!shown(name) || attributes.contains(&"hidden") || backing)
                    {
                        continue;
                    }
                    let kind = if attributes.contains(&"closed") {
                        "closed index"
                    } else {
                        "table"
                    };
                    containers.push(entry(name, kind));
                }
                for (list, kind) in [("aliases", "alias"), ("data_streams", "data stream")] {
                    for item in answer[list].as_array().into_iter().flatten() {
                        if let Some(name) = item["name"].as_str().filter(|name| shown(name)) {
                            containers.push(entry(name, kind));
                        }
                    }
                }
            }
            // A server before 7.9 has no `_resolve`: the two `_cat` lists.
            Err(error)
                if http::refused(&error).is_some_and(|refusal| {
                    refusal.hint == Hint::NoHandler || matches!(refusal.status, 400 | 404 | 405)
                }) =>
            {
                let indices = self
                    .call(
                        "GET",
                        &format!(
                            "_cat/indices?format=json&h=index,status&expand_wildcards={wildcards}"
                        ),
                        None,
                        opaque,
                        ANSWER_BYTES,
                    )
                    .await?;
                for index in indices.as_array().into_iter().flatten() {
                    if let Some(name) = index["index"].as_str().filter(|name| shown(name)) {
                        let closed = index["status"].as_str() == Some("close");
                        containers.push(entry(name, if closed { "closed index" } else { "table" }));
                    }
                }
                if let Ok(aliases) = self
                    .call(
                        "GET",
                        "_cat/aliases?format=json&h=alias",
                        None,
                        opaque,
                        ANSWER_BYTES,
                    )
                    .await
                {
                    let mut names: Vec<&str> = aliases
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|alias| alias["alias"].as_str())
                        .filter(|name| shown(name))
                        .collect();
                    names.sort_unstable();
                    names.dedup();
                    containers.extend(names.into_iter().map(|name| entry(name, "alias")));
                }
            }
            Err(error) => return Err(error),
        }
        containers.sort_by(|a, b| a.name.cmp(&b.name));
        containers.dedup_by(|a, b| a.name == b.name);
        containers.truncate(MOST_CONTAINERS);
        containers.extend(views::ALL.iter().map(|name| entry(name, "view")));
        Ok(containers)
    }

    /// The fields of a container: its mapping, and what a few documents
    /// hold beyond it. Asked again when it is older than a moment.
    async fn learned(&self, name: &str, opaque: &str) -> Result<Arc<Layout>> {
        if let Some(layout) = self.known.lock().ok().and_then(|known| {
            known
                .get(name)
                .filter(|known| known.at.elapsed() < LAYOUT_FRESH)
                .map(|known| known.layout.clone())
        }) {
            return Ok(layout);
        }
        let target = http::segment(name);
        let answer = self
            .call(
                "GET",
                &format!("{target}/_mapping"),
                None,
                opaque,
                ANSWER_BYTES,
            )
            .await?;
        let mapped = Layout::new(mapping::fields(&answer), []);
        // A field outside the mapping shows in documents only. A sample
        // that cannot be read (a closed index, no right to search) leaves
        // the mapping to stand alone.
        let sample = self
            .call(
                "POST",
                &format!("{target}/_search"),
                Some(json!({ "size": SAMPLE, "sort": ["_doc"], "track_total_hits": false })),
                opaque,
                ANSWER_BYTES,
            )
            .await
            .ok();
        let extra = sample
            .as_ref()
            .and_then(|sample| sample["hits"]["hits"].as_array())
            .map(|hits| mapped.missing_from(hits.iter().map(|hit| &hit["_source"])))
            .unwrap_or_default();
        let layout = Arc::new(Layout::new(mapped.fields, extra));
        if let Ok(mut known) = self.known.lock() {
            if known.len() >= 256 {
                known.clear();
            }
            known.insert(
                name.to_owned(),
                Known {
                    layout: layout.clone(),
                    at: Instant::now(),
                },
            );
        }
        Ok(layout)
    }

    /// The fields of a container as they were read last, however old.
    fn remembered(&self, name: &str) -> Option<Arc<Layout>> {
        self.known
            .lock()
            .ok()?
            .get(name)
            .map(|known| known.layout.clone())
    }

    /// Open a point in time on a container, where the server has them.
    async fn open_pit(&self, name: &str, opaque: &str) -> Result<Option<String>> {
        let state = self.pit.load(Ordering::Relaxed);
        if state == 2 || (state == 0 && !self.flavor.has_pit()) {
            return Ok(None);
        }
        let target = http::segment(name);
        let path = if self.flavor.opensearch {
            format!("{target}/_search/point_in_time?keep_alive={KEEP_ALIVE}")
        } else {
            format!("{target}/_pit?keep_alive={KEEP_ALIVE}")
        };
        let reply = self
            .client
            .send("POST", &path, None, Some(opaque), 1024 * 1024)
            .await?;
        match http::refusal(reply.status, &reply.body) {
            None => {
                let answer = reply.json().unwrap_or(Value::Null);
                let id = answer["id"]
                    .as_str()
                    .or_else(|| answer["pit_id"].as_str())
                    .map(str::to_owned);
                if id.is_some() {
                    self.pit.store(1, Ordering::Relaxed);
                }
                Ok(id)
            }
            // The server has no such API: pages are read without one.
            Some(refusal)
                if refusal.hint == Hint::NoHandler
                    || refusal.status == 405
                    || (refusal.status == 400 && state == 0 && refusal.kind.is_empty()) =>
            {
                self.pit.store(2, Ordering::Relaxed);
                Ok(None)
            }
            // The account may search but not open a point in time, or the
            // cluster keeps one for a shorter time than a page stays open:
            // this walk goes without.
            Some(refusal)
                if refusal.status == 403
                    || (refusal.status == 400
                        && !matches!(
                            refusal.kind.as_str(),
                            "index_closed_exception" | "index_not_found_exception"
                        )) =>
            {
                Ok(None)
            }
            Some(refusal) => Err(refusal.into()),
        }
    }

    /// Give up a cursor: close its point in time, so that the server does
    /// not keep the segments it reads for the rest of its life.
    async fn release(&self, cursor: Cursor) {
        let Some(pit) = cursor.pit else {
            return;
        };
        let (path, body) = if self.flavor.opensearch {
            ("_search/point_in_time", json!({ "pit_id": [pit] }))
        } else {
            ("_pit", json!({ "id": pit }))
        };
        let _ = tokio::time::timeout(
            STOP_TIMEOUT,
            self.client.send_now(
                "DELETE",
                path,
                Some(Payload::Json(body.to_string().into_bytes())),
            ),
        )
        .await;
    }

    /// Something was written: the pages that were read before show the
    /// index as it was. With `fields`, what is known of the fields goes too.
    async fn written(&self, fields: bool) {
        let cursors = match self.cursors.lock() {
            Ok(mut cursors) => cursors.drain(),
            Err(_) => Vec::new(),
        };
        for cursor in cursors {
            self.release(cursor).await;
        }
        if fields && let Ok(mut known) = self.known.lock() {
            known.clear();
        }
    }

    /// A cursor for the start of a walk.
    async fn open_cursor(&self, name: &str, opaque: &str) -> Result<Cursor> {
        let pit = self.open_pit(name, opaque).await?;
        let tiebreak = if pit.is_some()
            && self.flavor.has_shard_doc()
            && !self.no_shard_doc.load(Ordering::Relaxed)
        {
            Tiebreak::ShardDoc
        } else {
            Tiebreak::Doc
        };
        Ok(Cursor::new(pit, tiebreak))
    }

    /// Read on from a mark until `want` hits are taken, the documents end
    /// or, with their sources, the bytes of one page are full.
    #[allow(clippy::too_many_arguments)]
    async fn read(
        &self,
        cursor: &mut Cursor,
        name: &str,
        plan: &Plan,
        mut mark: Mark,
        want: usize,
        sources: bool,
        opaque: &str,
    ) -> Result<Read> {
        let mut hits: Vec<Hit> = Vec::new();
        let mut bytes = 0usize;
        // Every turn takes at least one hit or ends the read.
        loop {
            let need = want - hits.len();
            let size = need.min(cursor.batch.max(1)) + mark.overlap(cursor.tiebreak);
            let mut sort = plan.sort.clone();
            sort.push(cursor.tiebreak.sort());
            let mut body = json!({
                "query": plan.query,
                "sort": sort,
                "size": size,
                "track_total_hits": false,
                "_source": sources,
            });
            if let Some(after) = mark.after(cursor.tiebreak) {
                body["search_after"] = Value::Array(after);
            }
            let path = match &cursor.pit {
                Some(pit) => {
                    body["pit"] = json!({ "id": pit, "keep_alive": KEEP_ALIVE });
                    "_search".to_owned()
                }
                None => format!(
                    "{}/_search?preference={}",
                    http::segment(name),
                    cursor.preference
                ),
            };
            let sent = self
                .client
                .send(
                    "POST",
                    &path,
                    Some(Payload::Json(body.to_string().into_bytes())),
                    Some(opaque),
                    MAX_RESULT_BYTES,
                )
                .await;
            let reply = match sent {
                // Large documents: fewer of them at once.
                Err(error)
                    if error.to_string() == "elasticsearch_answer_too_large"
                        && cursor.batch > 1 =>
                {
                    cursor.batch = (cursor.batch / 8).max(1);
                    continue;
                }
                other => other?,
            };
            let size_of_answer = reply.body.len();
            let answer = match reply.accepted() {
                Ok(answer) => answer,
                Err(error) => {
                    let refusal = http::refused(&error);
                    // An index that allows fewer hits at once than usual.
                    if refusal.is_some_and(|refusal| refusal.hint == Hint::ResultWindow)
                        && cursor.batch > 10
                    {
                        cursor.batch = 10;
                        continue;
                    }
                    // A server that says it is Elasticsearch and still does
                    // not order by `_shard_doc`: nothing was read yet, so
                    // the walk starts over in the order every server knows.
                    if refusal.is_some_and(|refusal| refusal.hint == Hint::ShardDoc)
                        && cursor.tiebreak == Tiebreak::ShardDoc
                        && mark == Mark::default()
                        && hits.is_empty()
                    {
                        self.no_shard_doc.store(true, Ordering::Relaxed);
                        cursor.tiebreak = Tiebreak::Doc;
                        continue;
                    }
                    return Err(error);
                }
            };
            ensure!(
                answer["_shards"]["failed"].as_u64().unwrap_or(0) == 0
                    && answer["timed_out"].as_bool() != Some(true),
                "elasticsearch_shards_failed"
            );
            // The server may hand a point in time back under a new name.
            if cursor.pit.is_some()
                && let Some(id) = answer["pit_id"].as_str()
            {
                cursor.pit = Some(id.to_owned());
            }
            let found: Vec<Hit> = answer["hits"]["hits"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Hit::of)
                .collect();
            let full = found.len() >= size;
            let (taken, used_up) = mark.advance(cursor.tiebreak, found, need);
            let progressed = !taken.is_empty();
            hits.extend(taken);
            bytes = bytes.saturating_add(size_of_answer);
            if !full && used_up {
                return Ok(Read {
                    hits,
                    next: Position::End,
                    capped: false,
                });
            }
            if hits.len() >= want {
                return Ok(Read {
                    hits,
                    next: Position::At(mark),
                    capped: false,
                });
            }
            if (sources && bytes >= MAX_RESULT_BYTES) || !progressed {
                return Ok(Read {
                    hits,
                    next: Position::At(mark),
                    capped: true,
                });
            }
        }
    }

    /// One page with a cursor: the marks lead to its offset, and where no
    /// mark is near, the documents before it are passed without their
    /// sources.
    async fn page_with(
        &self,
        cursor: &mut Cursor,
        layout: &Layout,
        plan: &Plan,
        request: &PageRequest,
        opaque: &str,
    ) -> Result<QueryResult> {
        let name = request.container.name.as_str();
        let limit = request.limit as usize;
        let (mut at, mut position) = cursor.nearest(request.offset);
        let mut steps = 0;
        while at < request.offset {
            let mark = match &position {
                Position::At(mark) => mark.clone(),
                Position::End => break,
            };
            ensure!(steps < WALK_STEPS, "elasticsearch_page_too_deep");
            steps += 1;
            let step = (request.offset - at).min(WALK_HITS);
            let batch = cursor.batch;
            cursor.batch = cursor.batch.max(step as usize).min(WALK_HITS as usize);
            let passed = self
                .read(cursor, name, plan, mark, step as usize, false, opaque)
                .await;
            // An index with a small window keeps its small batch.
            cursor.batch = cursor.batch.min(batch.max(10));
            let passed = passed?;
            at += step;
            position = passed.next;
            cursor.mark(at, position.clone());
        }
        let read = match position {
            Position::End => Read {
                hits: Vec::new(),
                next: Position::End,
                capped: false,
            },
            Position::At(mark) => {
                self.read(cursor, name, plan, mark, limit, true, opaque)
                    .await?
            }
        };
        // A page that was cut at its bytes is shorter than it was asked
        // for; the page after it goes on where it ended, so that nothing
        // is left out.
        cursor.mark(request.offset + u64::from(request.limit), read.next);
        cursor.last = Some(request.offset);
        Ok(QueryResult {
            columns: layout.result_columns(),
            rows: read
                .hits
                .iter()
                .map(|hit| layout.row(&hit.id, &hit.source))
                .collect(),
            truncated: read.capped,
            ..QueryResult::default()
        })
    }

    async fn page(&self, request: &PageRequest, opaque: &str) -> Result<QueryResult> {
        let started = Instant::now();
        let name = request.container.name.as_str();
        if views::is_view(name) {
            let answer = self
                .call(
                    "GET",
                    &views::path(name, self.show_hidden),
                    None,
                    opaque,
                    ANSWER_BYTES,
                )
                .await?;
            let mut result =
                views::page(name, views::rows(name, &answer, self.show_hidden), request)?;
            result.duration_ms = started.elapsed().as_millis() as u64;
            return Ok(result);
        }
        let layout = self.learned(name, opaque).await?;
        let plan = Plan::new(&layout, request)?;
        let identity = browse::identity(request);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let kept = self
                .cursors
                .lock()
                .ok()
                .and_then(|mut cursors| cursors.take(&identity));
            let mut cursor = match kept {
                // The first page, and a page that is read again, look at
                // the index as it is now.
                Some(cursor) if request.offset != 0 && cursor.last != Some(request.offset) => {
                    cursor
                }
                stale => {
                    if let Some(stale) = stale {
                        self.release(stale).await;
                    }
                    self.open_cursor(name, opaque).await?
                }
            };
            let outcome = self
                .page_with(&mut cursor, &layout, &plan, request, opaque)
                .await;
            match outcome {
                Ok(mut result) => {
                    let gone = match self.cursors.lock() {
                        Ok(mut cursors) => cursors.put(identity, cursor),
                        Err(_) => vec![cursor],
                    };
                    for cursor in gone {
                        self.release(cursor).await;
                    }
                    result.duration_ms = started.elapsed().as_millis() as u64;
                    return Ok(result);
                }
                Err(error) => {
                    // The point in time ran out while the page was open:
                    // the walk starts over on a new one.
                    let expired = cursor.pit.is_some()
                        && http::refused(&error).is_some_and(|refusal| {
                            refusal.kind == "search_context_missing_exception"
                                || refusal.status == 404
                        });
                    self.release(cursor).await;
                    if expired && attempt == 1 {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }

    /// Find a document by its id: where it is, how it is routed and which
    /// change of the index it is from.
    async fn locate(&self, name: &str, id: &str, opaque: &str) -> Result<Option<Located>> {
        let answer = self
            .call(
                "POST",
                &format!("{}/_search", http::segment(name)),
                Some(json!({
                    "query": { "ids": { "values": [id] } },
                    "size": 2,
                    "seq_no_primary_term": true,
                    "track_total_hits": false,
                })),
                opaque,
                MAX_RESULT_BYTES,
            )
            .await?;
        let hits = answer["hits"]["hits"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        ensure!(hits.len() <= 1, "elasticsearch_id_not_unique");
        let Some(hit) = hits.first() else {
            return Ok(None);
        };
        Ok(Some(Located {
            index: hit["_index"].as_str().unwrap_or(name).to_owned(),
            routing: hit["_routing"].as_str().map(str::to_owned),
            seq_no: hit["_seq_no"]
                .as_u64()
                .ok_or_else(|| anyhow!("elasticsearch_answer_unreadable"))?,
            primary_term: hit["_primary_term"]
                .as_u64()
                .ok_or_else(|| anyhow!("elasticsearch_answer_unreadable"))?,
            source: hit.get("_source").cloned().unwrap_or(Value::Null),
        }))
    }

    /// Run one SQL statement and read its rows through the cursor of the
    /// server, up to what a result may hold.
    async fn run_sql(&self, statement: &str, opaque: &str) -> Result<QueryResult> {
        let remembered = self.sql.lock().ok().and_then(|api| *api);
        let candidates: Vec<sql::Api> = match remembered {
            Some(api) => vec![api],
            None => sql::Api::candidates(self.flavor.opensearch).to_vec(),
        };
        let mut first = None;
        for api in candidates {
            // Elasticsearch pages every statement; the plugin of OpenSearch
            // only the ones that read rows of documents.
            let paged = api == sql::Api::Elastic || sql::plain(statement);
            let reply = self
                .client
                .send(
                    "POST",
                    api.path(),
                    Some(Payload::Json(
                        sql::first_body(statement, paged).to_string().into_bytes(),
                    )),
                    Some(opaque),
                    MAX_RESULT_BYTES,
                )
                .await?;

            // Not here: the next place the SQL of a server can be.
            let absent = http::refusal(reply.status, &reply.body).is_some_and(|refusal| {
                refusal.hint == Hint::NoHandler
                    || (matches!(refusal.status, 400 | 404 | 405 | 501) && refusal.kind.is_empty())
            });
            if absent && remembered.is_none() {
                continue;
            }
            if let Ok(mut known) = self.sql.lock() {
                *known = Some(api);
            }
            first = Some((api, reply));
            break;
        }
        let (api, reply) = first.ok_or_else(|| anyhow!("elasticsearch_sql_unavailable"))?;
        if let Some(refusal) = http::refusal(reply.status, &reply.body) {
            // Whatever the two servers call a statement they do not take.
            let code = code_of(&refusal);
            if refusal.status == 400
                && (code.starts_with("database_error") || code == "elasticsearch_request_invalid")
            {
                bail!("elasticsearch_sql_invalid");
            }
            return Err(refusal.into());
        }
        let answer = reply
            .json()
            .ok_or_else(|| anyhow!("elasticsearch_answer_unreadable"))?;
        let Some(page) = sql::decode(&answer) else {
            // `EXPLAIN` and the like answer with a document of their own.
            return Ok(answer::shown(reply.status, &reply.body));
        };
        let mut result = QueryResult {
            columns: page.columns,
            rows: page.rows,
            ..QueryResult::default()
        };
        let mut bytes = reply.body.len();
        let mut cursor = page.cursor;
        while let Some(next) = cursor.take() {
            if result.rows.len() >= MAX_RESULT_ROWS || bytes >= MAX_RESULT_BYTES {
                result.truncated = true;
                // The rest is not read: the server may forget the cursor.
                let _ = self
                    .client
                    .send(
                        "POST",
                        api.close_path(),
                        Some(Payload::Json(
                            sql::next_body(&next).to_string().into_bytes(),
                        )),
                        Some(opaque),
                        1024 * 1024,
                    )
                    .await;
                break;
            }
            let reply = self
                .client
                .send(
                    "POST",
                    api.path(),
                    Some(Payload::Json(
                        sql::next_body(&next).to_string().into_bytes(),
                    )),
                    Some(opaque),
                    MAX_RESULT_BYTES,
                )
                .await?;
            bytes = bytes.saturating_add(reply.body.len());
            let answer = reply.accepted()?;
            let Some(more) = sql::decode(&answer) else {
                break;
            };
            if more.rows.is_empty() {
                break;
            }
            result.rows.extend(more.rows);
            cursor = more.cursor;
        }
        if result.rows.len() > MAX_RESULT_ROWS {
            result.rows.truncate(MAX_RESULT_ROWS);
            result.truncated = true;
        }
        // What the plugin answers whole it cuts without a word: rows at a
        // limit of the cluster, groups at 1000. A result of exactly that
        // many rows says so here.
        if api != sql::Api::Elastic
            && !result.truncated
            && result.rows.len() >= 200
            && !sql::plain(statement)
        {
            let settings = self
                .call(
                    "GET",
                    "_cluster/settings?include_defaults=true&filter_path=*.plugins.query.size_limit,*.plugins.query.buckets,*.opendistro.query.size_limit",
                    None,
                    opaque,
                    1024 * 1024,
                )
                .await
                .unwrap_or(Value::Null);
            let setting = |owner: &str, name: &str| {
                ["transient", "persistent", "defaults"]
                    .into_iter()
                    .find_map(|scope| {
                        settings[scope][owner]["query"][name]
                            .as_str()?
                            .parse::<usize>()
                            .ok()
                    })
            };
            let rows =
                setting("plugins", "size_limit").or_else(|| setting("opendistro", "size_limit"));
            let limits = [
                rows,
                // Its two defaults, where the limit could not be read.
                rows.is_none().then_some(200),
                rows.is_none().then_some(10_000),
                Some(setting("plugins", "buckets").unwrap_or(1000)),
            ];
            result.truncated = limits.contains(&Some(result.rows.len()));
        }
        Ok(result)
    }

    /// Run one request of the console.
    async fn run(&self, request: &console::Request, opaque: &str) -> Result<QueryResult> {
        match &request.step {
            console::Step::Sql(statement) => self.run_sql(statement, opaque).await,
            console::Step::Rest { method, path, body } => {
                let mut path = path.clone();
                // A `_cat` table is asked for as a list of rows, which is
                // what a grid shows; as text it is columns of spaces.
                let (plain, query) = path.split_once('?').unwrap_or((path.as_str(), ""));
                let formatted = query.split('&').any(|pair| {
                    pair.starts_with("format=") || pair == "help" || pair.starts_with("help=")
                });
                if plain.starts_with("_cat/") && !formatted {
                    path = if query.is_empty() {
                        format!("{plain}?format=json")
                    } else {
                        format!("{path}&format=json")
                    };
                }
                let payload = match body {
                    None => None,
                    Some(console::Body::One(text)) => {
                        Some(Payload::Json(text.clone().into_bytes()))
                    }
                    Some(console::Body::Lines(lines)) => {
                        let mut text = lines.join("\n");
                        text.push('\n');
                        Some(Payload::Lines(text.into_bytes()))
                    }
                };
                let reply = self
                    .client
                    .send(method, &path, payload, Some(opaque), MAX_RESULT_BYTES)
                    .await?;
                if let Some(refusal) = http::refusal(reply.status, &reply.body) {
                    return Err(refusal.into());
                }
                Ok(answer::shown(reply.status, &reply.body))
            }
        }
    }

    /// The figures of the monitor: what the cluster says of its health,
    /// its nodes, its indices and its tasks. Each part is asked for by
    /// itself, and a part the account may not see is left out.
    async fn stats(&self) -> Result<QueryResult> {
        let started = Instant::now();
        let id = opaque();
        let ask = |path: &'static str| {
            let id = id.clone();
            async move { self.call("GET", path, None, &id, ANSWER_BYTES).await.ok() }
        };
        let (health, nodes, cluster, listed, tasks) = tokio::join!(
            ask("_cluster/health"),
            ask(
                "_nodes/stats/http,jvm,os?filter_path=nodes.*.http.current_open,nodes.*.jvm.mem.heap_used_in_bytes,nodes.*.jvm.mem.heap_max_in_bytes,nodes.*.os.cpu.percent"
            ),
            ask(
                "_cluster/stats?filter_path=indices.count,indices.docs.count,indices.store.size_in_bytes,indices.shards.total"
            ),
            ask(
                "_cat/nodes?format=json&h=name,node.role,master,cluster_manager,heap.percent,cpu,load_1m"
            ),
            ask("_tasks?detailed=false&group_by=none&actions=indices:data/*"),
        );
        let duration = started.elapsed().as_millis() as u64;
        let mut columns: Vec<(&str, Cell)> = Vec::new();
        let whole = |value: &Value| {
            value
                .as_u64()
                .map(|number| Cell::Integer(number.to_string()))
        };
        // The two large figures: clients connected over HTTP, and the
        // searches and writes that are running.
        if let Some(nodes) = nodes.as_ref().and_then(|nodes| nodes["nodes"].as_object()) {
            let sum = |read: &dyn Fn(&Value) -> Option<u64>| -> u64 {
                nodes.values().filter_map(read).sum()
            };
            columns.push((
                "connections",
                Cell::Integer(sum(&|node| node["http"]["current_open"].as_u64()).to_string()),
            ));
            let used = sum(&|node| node["jvm"]["mem"]["heap_used_in_bytes"].as_u64());
            let most = sum(&|node| node["jvm"]["mem"]["heap_max_in_bytes"].as_u64());
            let running: Vec<Task> = tasks
                .as_ref()
                .map(views::tasks)
                .unwrap_or_default()
                .into_iter()
                .filter(|task| task.parent.is_none())
                .collect();
            if tasks.is_some() {
                columns.push(("active", Cell::Integer(running.len().to_string())));
            }
            if most > 0 {
                columns.push((
                    "heap",
                    Cell::Text(format!(
                        "{} of {} ({}%)",
                        views::size(used),
                        views::size(most),
                        used * 100 / most
                    )),
                ));
            }
            let cpu: Vec<u64> = nodes
                .values()
                .filter_map(|node| node["os"]["cpu"]["percent"].as_u64())
                .collect();
            if !cpu.is_empty() {
                columns.push((
                    "cpu",
                    Cell::Text(format!("{}%", cpu.iter().sum::<u64>() / cpu.len() as u64)),
                ));
            }
            if !running.is_empty() {
                let mut named: Vec<String> = running
                    .iter()
                    .take(MONITOR_TASKS)
                    .map(|task| {
                        format!(
                            "{} {:.1} s",
                            task.action.trim_start_matches("indices:data/"),
                            task.running_nanos as f64 / 1e9
                        )
                    })
                    .collect();
                if running.len() > MONITOR_TASKS {
                    named.push(format!("+{} more", running.len() - MONITOR_TASKS));
                }
                columns.push(("running tasks", Cell::Text(named.join("; "))));
            }
        }
        if let Some(health) = &health {
            if let Some(status) = health["status"].as_str() {
                columns.push(("status", Cell::Text(status.to_owned())));
            }
            if let (Some(nodes), Some(data)) = (
                health["number_of_nodes"].as_u64(),
                health["number_of_data_nodes"].as_u64(),
            ) {
                columns.push(("nodes", Cell::Text(format!("{nodes} ({data} data)"))));
            }
            let shard = |name: &str| health[name].as_u64().unwrap_or(0);
            let mut shards = format!("{} active", shard("active_shards"));
            for (name, label) in [
                ("relocating_shards", "relocating"),
                ("initializing_shards", "initializing"),
                ("unassigned_shards", "unassigned"),
            ] {
                if shard(name) > 0 {
                    shards.push_str(&format!(", {} {label}", shard(name)));
                }
            }
            columns.push(("shards", Cell::Text(shards)));
            if shard("number_of_pending_tasks") > 0 {
                columns.push((
                    "pending cluster tasks",
                    Cell::Integer(shard("number_of_pending_tasks").to_string()),
                ));
            }
        }
        if let Some(indices) = cluster.as_ref().map(|cluster| &cluster["indices"]) {
            if let Some(count) = whole(&indices["count"]) {
                columns.push(("indices", count));
            }
            if let Some(documents) = whole(&indices["docs"]["count"]) {
                columns.push(("documents", documents));
            }
            if let Some(bytes) = indices["store"]["size_in_bytes"].as_u64() {
                columns.push(("store size", Cell::Text(views::size(bytes))));
            }
        }
        if let Some(listed) = listed.as_ref().and_then(Value::as_array) {
            let mut named: Vec<String> = listed
                .iter()
                .take(MONITOR_NODES)
                .map(|node| {
                    let text = |name: &str| node[name].as_str().unwrap_or("?");
                    let elected = [&node["master"], &node["cluster_manager"]]
                        .into_iter()
                        .any(|mark| mark.as_str() == Some("*"));
                    format!(
                        "{}{} [{}] heap {}%, cpu {}%, load {}",
                        text("name"),
                        if elected { " *" } else { "" },
                        text("node.role"),
                        text("heap.percent"),
                        text("cpu"),
                        text("load_1m")
                    )
                })
                .collect();
            if listed.len() > MONITOR_NODES {
                named.push(format!("+{} more", listed.len() - MONITOR_NODES));
            }
            if !named.is_empty() {
                columns.push(("node list", Cell::Text(named.join("; "))));
            }
        }
        columns.push(("version", Cell::Text(self.flavor.described.clone())));
        let cluster_name = health
            .as_ref()
            .and_then(|health| health["cluster_name"].as_str())
            .unwrap_or(&self.flavor.cluster);
        if !cluster_name.is_empty() {
            columns.push(("database", Cell::Text(cluster_name.to_owned())));
        }
        Ok(QueryResult {
            columns: columns
                .iter()
                .map(|(name, cell)| ResultColumn {
                    name: (*name).into(),
                    data_type: match cell {
                        Cell::Integer(_) => "number",
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

impl Session for SearchSession {
    fn dialect(&self) -> &Dialect {
        &Dialect::ANSI
    }

    fn execute<'a>(
        &'a self,
        text: &'a str,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            ensure!(!cancel.is_cancelled(), "query_cancelled");
            let started = Instant::now();
            let id = opaque();
            if is_probe(text) {
                let answered = self
                    .bounded(&cancel, &id, async {
                        self.call("GET", "", None, &id, 1024 * 1024).await
                    })
                    .await;
                // An account that may not ask the root has still signed in.
                if let Err(error) = answered
                    && error.to_string() != "elasticsearch_forbidden"
                {
                    return Err(error);
                }
                return Ok(QueryResult {
                    columns: vec![ResultColumn {
                        name: "1".into(),
                        data_type: "number".into(),
                    }],
                    rows: vec![vec![Cell::Integer("1".into())]],
                    duration_ms: started.elapsed().as_millis() as u64,
                    ..QueryResult::default()
                });
            }
            // Everything is read before anything is sent: a script with a
            // mistake, or with a request that needs `confirm`, changes
            // nothing.
            let requests =
                console::parse(text).map_err(|mistake| at_line(mistake.code, mistake.line))?;
            if let Some(unconfirmed) = requests
                .iter()
                .find(|request| !request.confirmed && console::destructive(&request.step))
            {
                return Err(at_line(console::CONFIRMATION, unconfirmed.line));
            }
            let several = requests.len() > 1;
            let writes = requests
                .iter()
                .any(|request| !console::reads_only(&request.step));
            let outcome = self
                .bounded(&cancel, &id, async {
                    let mut last = QueryResult::default();
                    let mut affected = 0_u64;
                    for request in &requests {
                        last = self.run(request, &id).await.map_err(|error| {
                            // In a script, a refusal says which request it
                            // is about; the ones before it were run.
                            let code = http::plain(error).to_string();
                            if several && code.starts_with("elasticsearch_") {
                                at_line(&code, request.line)
                            } else {
                                anyhow!(code)
                            }
                        })?;
                        affected = affected.saturating_add(last.affected);
                    }
                    last.affected = affected;
                    Ok(last)
                })
                .await;
            if writes {
                self.written(true).await;
            }
            let mut result = outcome?;
            result.duration_ms = started.elapsed().as_millis() as u64;
            Ok(result)
        })
    }

    fn containers<'a>(&'a self) -> BoxFuture<'a, Result<Vec<TableRef>>> {
        Box::pin(self.briefly(async move {
            let id = opaque();
            // A refresh of the tree is also when the fields are read anew.
            self.written(true).await;
            let containers = self.list(&id).await?;
            if let Ok(mut kinds) = self.kinds.lock() {
                *kinds = containers
                    .iter()
                    .map(|container| (container.name.clone(), container.kind.clone()))
                    .collect();
            }
            Ok(containers)
        }))
    }

    fn columns<'a>(&'a self, container: &'a TableRef) -> BoxFuture<'a, Result<Vec<ColumnInfo>>> {
        Box::pin(self.briefly(async move {
            if views::is_view(&container.name) {
                return Ok(views::column_info(&container.name));
            }
            let id = opaque();
            let mut columns = self.learned(&container.name, &id).await?.column_info();
            // Without a key the grid offers no change: an alias, a data
            // stream and a closed index are read only.
            let kind = self
                .kind_of(&container.name)
                .unwrap_or_else(|| container.kind.clone());
            if kind != "table" {
                for column in &mut columns {
                    column.primary_key = false;
                }
            }
            Ok(columns)
        }))
    }

    fn server_stats<'a>(&'a self) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(self.briefly(self.stats()))
    }

    fn close<'a>(&'a self) -> BoxFuture<'a, ()> {
        // Nothing is held open but idle connections, which go with the
        // session, and the points in time of the pages, which are closed.
        Box::pin(self.written(false))
    }

    fn page_text(&self, page: &PageRequest) -> Result<String> {
        browse::page_text(self.remembered(&page.container.name).as_deref(), page)
    }

    fn read_page<'a>(
        &'a self,
        page: &'a PageRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            let id = opaque();
            self.bounded(&cancel, &id, self.page(page, &id)).await
        })
    }

    fn change_text(&self, change: &RowChange) -> Result<String> {
        self.change_as_text(change)
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
        Box::pin(async move {
            self.import(container, columns, rows, cancel)
                .await
                .map_err(http::plain)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_a_sentence_in_both_languages() {
        let mut seen = std::collections::HashSet::new();
        for (code, (pl, english)) in SENTENCES {
            assert!(seen.insert(*code), "{code} twice");
            assert!(code.starts_with("elasticsearch_"), "{code}");
            assert!(
                code.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            );
            for text in [pl, english] {
                assert!(text.contains(' ') && text.ends_with('.'), "{code}: {text}");
                assert!(!text.contains(code), "{code}");
            }
            assert_ne!(pl, english, "{code}");
            assert_eq!(describe(code, false), Some(*pl));
            assert_eq!(describe(code, true), Some(*english));
        }
        // The codes the other files of the driver answer with.
        for code in [
            "elasticsearch_endpoint_invalid",
            "elasticsearch_endpoint_tls_mismatch",
            "elasticsearch_endpoint_url_tunnel",
            "elasticsearch_credentials_invalid",
            "elasticsearch_credentials_required",
            "elasticsearch_path_invalid",
            "elasticsearch_answer_too_large",
            "elasticsearch_answer_unreadable",
            "elasticsearch_not_found",
            "elasticsearch_value_invalid",
            "elasticsearch_id_invalid",
            "elasticsearch_field_in_list",
            "elasticsearch_filter_number",
            "elasticsearch_filter_date",
            "elasticsearch_filter_boolean",
            "elasticsearch_filter_ip",
            "elasticsearch_filter_unsupported",
            "elasticsearch_filter_unmapped",
            "elasticsearch_sort_unsupported",
            console::SYNTAX,
            console::BODY,
            console::CONFIRMATION,
        ] {
            assert!(describe(code, true).is_some(), "{code}");
        }
        assert_eq!(describe("database_login_rejected", true), None);
        assert_eq!(describe("elasticsearch_made_up", true), None);
    }

    #[test]
    fn a_code_may_name_the_line_or_the_row_it_is_about() {
        let line = describe("elasticsearch_confirmation_required_at_line_3", true).unwrap();
        assert!(line.starts_with("Line 3: This request removes"), "{line}");
        let wiersz = describe("elasticsearch_console_json_at_line_12", false).unwrap();
        assert!(wiersz.starts_with("Wiersz 12: Treść żądania"), "{wiersz}");
        let row = describe("elasticsearch_import_value_rejected_at_row_7", true).unwrap();
        assert!(
            row.starts_with("Row 7: The server refused a document"),
            "{row}"
        );
        // The same text is handed out again, not made again.
        assert!(std::ptr::eq(
            line,
            describe("elasticsearch_confirmation_required_at_line_3", true).unwrap()
        ));
        assert_eq!(describe("elasticsearch_made_up_at_line_3", true), None);
        assert_eq!(describe("elasticsearch_console_json_at_line_x", true), None);
    }

    #[test]
    fn a_refusal_of_the_server_has_a_code() {
        let code = |status: u16, kind: &str, hint: Hint| {
            code_of(&Refusal {
                status,
                kind: kind.into(),
                hint,
            })
        };
        for (status, kind, expected) in [
            (
                404,
                "index_not_found_exception",
                "elasticsearch_index_unknown",
            ),
            (404, "IndexNotFoundException", "elasticsearch_index_unknown"),
            (400, "index_closed_exception", "elasticsearch_index_closed"),
            (
                400,
                "resource_already_exists_exception",
                "elasticsearch_index_exists",
            ),
            (
                400,
                "invalid_index_name_exception",
                "elasticsearch_name_invalid",
            ),
            (
                409,
                "version_conflict_engine_exception",
                "elasticsearch_version_conflict",
            ),
            (
                400,
                "mapper_parsing_exception",
                "elasticsearch_value_rejected",
            ),
            (
                400,
                "document_parsing_exception",
                "elasticsearch_value_rejected",
            ),
            (
                400,
                "strict_dynamic_mapping_exception",
                "elasticsearch_value_rejected",
            ),
            (403, "cluster_block_exception", "elasticsearch_blocked"),
            (400, "task_cancelled_exception", "query_cancelled"),
            (401, "security_exception", "database_login_rejected"),
            (403, "security_exception", "elasticsearch_forbidden"),
            (403, "", "elasticsearch_forbidden"),
            (
                429,
                "es_rejected_execution_exception",
                "elasticsearch_overloaded",
            ),
            (
                500,
                "circuit_breaking_exception",
                "elasticsearch_overloaded",
            ),
            (400, "parsing_exception", "elasticsearch_request_invalid"),
            (
                400,
                "x_content_parse_exception",
                "elasticsearch_request_invalid",
            ),
            (
                400,
                "illegal_argument_exception",
                "elasticsearch_request_invalid",
            ),
            (400, "verification_exception", "elasticsearch_sql_invalid"),
            (400, "SyntaxCheckException", "elasticsearch_sql_invalid"),
            (400, "SemanticCheckException", "elasticsearch_sql_invalid"),
            (503, "", "elasticsearch_unavailable"),
            (
                503,
                "master_not_discovered_exception",
                "elasticsearch_unavailable",
            ),
            (
                404,
                "resource_not_found_exception",
                "elasticsearch_not_found",
            ),
            (405, "", "elasticsearch_api_unknown"),
            // Anything else keeps the name the server gave it, and nothing
            // of what it said.
            (
                500,
                "null_pointer_exception",
                "database_error [null_pointer_exception]",
            ),
            (502, "", "database_error [HTTP 502]"),
        ] {
            assert_eq!(code(status, kind, Hint::None), expected, "{status} {kind}");
        }
        assert_eq!(code(400, "", Hint::NoHandler), "elasticsearch_api_unknown");
        assert_eq!(
            code(400, "illegal_argument_exception", Hint::ResultWindow),
            "elasticsearch_result_window"
        );
        assert_eq!(
            code(400, "illegal_argument_exception", Hint::Fielddata),
            "elasticsearch_sort_unsupported"
        );
        // Every code of this driver that a refusal maps to has a sentence.
        for status in [400, 401, 403, 404, 405, 409, 429, 500, 503] {
            let made = code(status, "", Hint::None);
            if made.starts_with("elasticsearch_") {
                assert!(describe(&made, true).is_some(), "{made}");
            }
        }
    }

    #[test]
    fn the_server_says_what_it_is() {
        let elastic: Value = serde_json::from_str(
            r#"{"name":"bb37fd677251","cluster_name":"docker-cluster","version":{"number":"9.5.5","build_flavor":"default","build_type":"docker","lucene_version":"10.5.1"},"tagline":"You Know, for Search"}"#,
        )
        .unwrap();
        let flavor = Flavor::of(&elastic).unwrap();
        assert!(!flavor.opensearch);
        assert_eq!(flavor.version, (9, 5, 5));
        assert_eq!(flavor.described, "Elasticsearch 9.5.5, Lucene 10.5.1");
        assert_eq!(flavor.cluster, "docker-cluster");
        assert!(flavor.has_pit() && flavor.has_shard_doc());
        let opensearch: Value = serde_json::from_str(
            r#"{"name":"31d59d4287dc","cluster_name":"docker-cluster","version":{"distribution":"opensearch","number":"2.19.6","lucene_version":"9.12.3"},"tagline":"The OpenSearch Project: https://opensearch.org/"}"#,
        )
        .unwrap();
        let flavor = Flavor::of(&opensearch).unwrap();
        assert!(flavor.opensearch);
        assert_eq!(flavor.version, (2, 19, 6));
        assert_eq!(flavor.described, "OpenSearch 2.19.6, Lucene 9.12.3");
        // A point in time, but no `_shard_doc` to order it by.
        assert!(flavor.has_pit() && !flavor.has_shard_doc());
        let version = |opensearch: bool, number: &str| {
            let mut answer = json!({ "version": { "number": number, "lucene_version": "8" }, "tagline": "You Know, for Search" });
            if opensearch {
                answer["version"]["distribution"] = json!("opensearch");
            }
            Flavor::of(&answer).unwrap()
        };
        // What came with which version.
        assert!(!version(false, "7.9.3").has_pit());
        assert!(version(false, "7.10.2").has_pit() && !version(false, "7.10.2").has_shard_doc());
        assert!(version(false, "7.17.29").has_shard_doc());
        assert!(version(false, "8.19.23-SNAPSHOT").has_shard_doc());
        assert!(!version(true, "1.3.20").has_pit());
        assert!(!version(true, "2.3.0").has_pit() && version(true, "2.4.0").has_pit());
        let serverless = Flavor::of(&json!({ "version": { "number": "8.11.0", "build_flavor": "serverless" }, "tagline": "You Know, for Search" })).unwrap();
        assert_eq!(serverless.described, "Elasticsearch 8.11.0 (serverless)");
        // Something else that answers in JSON is not a search server.
        for other in [
            json!({ "version": "1.0" }),
            json!({ "version": { "number": "1.0" } }),
            json!({ "status": "ok" }),
            json!([]),
        ] {
            assert_eq!(Flavor::of(&other), None, "{other}");
        }
        // A server that would not say is taken for a current one of the
        // product the profile names.
        let guessed = Flavor::guessed("amazon-opensearch-service");
        assert!(guessed.opensearch && guessed.has_pit() && !guessed.has_shard_doc());
        let guessed = Flavor::guessed("elastic-cloud");
        assert!(!guessed.opensearch && guessed.has_pit() && guessed.has_shard_doc());
    }

    #[test]
    fn the_products_are_presets_of_one_driver() {
        let spec = ENGINE.spec();
        assert_eq!(spec.generic().id, "elasticsearch");
        for alias in ["OpenSearch", "elastic", "ES"] {
            assert_eq!(Engine::from_id(alias), Some(ENGINE), "{alias}");
        }
        let ids: Vec<&str> = spec.products.iter().map(|product| product.id).collect();
        assert_eq!(
            ids,
            [
                "elasticsearch",
                "opensearch",
                "amazon-opensearch-service",
                "elastic-cloud"
            ]
        );
        for product in spec.products {
            let (pl, english) = product.note.unwrap_or_else(|| panic!("{}", product.id));
            assert!(pl.contains("Kibana Dev Tools") && english.contains("Kibana Dev Tools"));
            assert!(product.capabilities.is_none() && product.dialect.is_none());
            assert!(product.listed);
        }
        let own = |id: &str| {
            let product = ENGINE.product(id);
            (product.default_port, product.default_tls, product.group)
        };
        assert_eq!(
            own("elasticsearch"),
            (9200, TlsMode::Require, Group::Analytics)
        );
        assert_eq!(
            own("opensearch"),
            (9200, TlsMode::Require, Group::Analytics)
        );
        assert_eq!(
            own("amazon-opensearch-service"),
            (443, TlsMode::VerifyFull, Group::Cloud)
        );
        assert_eq!(
            own("elastic-cloud"),
            (443, TlsMode::VerifyFull, Group::Cloud)
        );
        assert!(
            ENGINE
                .product("amazon-opensearch-service")
                .matches("aws search")
        );
        assert!(ENGINE.product("elastic-cloud").matches("elastic cloud"));
        // What IAM sign-in would need is said, not left to be found out.
        let amazon = ENGINE.product("amazon-opensearch-service").note.unwrap();
        assert!(amazon.0.contains("SigV4") && amazon.1.contains("SigV4"));
        let caps = spec.capabilities;
        assert_eq!(caps.language, QueryLanguage::Command);
        assert_eq!(caps.browse, BrowseModel::Documents);
        assert!(!caps.schemas && !caps.table_designer && !caps.query_builder);
        assert!(caps.filter && caps.sort && caps.monitor && caps.import && caps.export);
        assert!(caps.edit_cells && caps.insert_rows && caps.delete_rows);
        assert_eq!(spec.form.database, FieldUse::Hidden);
        assert!(spec.form.tls && spec.form.ssh);
        // The key of an API goes into the password field; the option only
        // says how it is sent.
        let auth = spec.option("auth").unwrap();
        assert_eq!(auth.default, "auto");
        assert!(auth.accepts("api_key") && auth.accepts("bearer") && !auth.accepts("aws"));
        assert!(spec.option("path_prefix").is_some() && spec.option("show_hidden").is_some());
    }

    #[tokio::test]
    async fn a_profile_is_checked_before_anything_is_dialled() {
        let mut profile = crate::model::ConnectionProfile {
            name: "Search".into(),
            engine: ENGINE,
            host: "https://es.example.test:9243".into(),
            port: 9200,
            tls: TlsMode::Disable,
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
            "elasticsearch_endpoint_tls_mismatch"
        );
        profile.host = "es.example.test".into();
        profile.set_option("path_prefix", "../up");
        assert_eq!(
            failure(profile.clone()).await,
            "elasticsearch_endpoint_invalid"
        );
        profile.set_option("path_prefix", "");
        profile.set_option("auth", "bearer");
        assert_eq!(
            failure(profile.clone()).await,
            "elasticsearch_credentials_required"
        );
        profile.set_option("auth", "basic");
        profile.username = "a:b".into();
        profile.password = "x".into();
        assert_eq!(
            failure(profile.clone()).await,
            "elasticsearch_credentials_invalid"
        );
        profile.set_option("auth", "kerberos");
        assert_eq!(
            profile.validate().unwrap_err().to_string(),
            "connection_option_invalid"
        );
    }

    #[tokio::test]
    async fn a_typed_path_cannot_leave_the_server_or_its_prefix() {
        let mut profile = crate::model::ConnectionProfile {
            name: "Search".into(),
            engine: ENGINE,
            host: "es.example.test".into(),
            port: 9200,
            tls: TlsMode::Disable,
            ..crate::model::ConnectionProfile::default()
        };
        profile.set_option("path_prefix", "/es");
        let request = ConnectRequest {
            profile: &profile,
            product: profile.product_spec(),
            forwarded: None,
        };
        let client = Client::new(&request).await.unwrap();
        // Nothing listens there: what counts is what is refused before
        // anything is dialled.
        let code = |path: &'static str| {
            let client = &client;
            async move {
                client
                    .send("GET", path, None, None, 1024)
                    .await
                    .unwrap_err()
                    .to_string()
            }
        };
        for path in [
            "../_cat/indices",
            "a/../../b",
            "idx/_doc/a#b",
            "idx\\x",
            "a\nb",
        ] {
            assert_eq!(code(path).await, "elasticsearch_path_invalid", "{path}");
        }
        assert_eq!(
            client
                .send("G ET", "x", None, None, 1024)
                .await
                .unwrap_err()
                .to_string(),
            "elasticsearch_path_invalid"
        );
    }

    #[test]
    fn the_connection_test_is_answered_by_the_root() {
        for text in ["SELECT 1", " select 1 ; ", "Select 1;"] {
            assert!(is_probe(text), "{text}");
        }
        for text in ["SELECT 1 FROM x", "SELECT 2", "GET /"] {
            assert!(!is_probe(text), "{text}");
        }
        assert!(opaque().starts_with("baza-") && opaque() != opaque());
    }
}
