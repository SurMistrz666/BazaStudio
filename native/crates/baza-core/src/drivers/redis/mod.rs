//! Redis and the servers and services that speak its protocol (RESP2 and
//! RESP3): Valkey, KeyDB, Dragonfly and the hosted ones.
//!
//! The first engine of this application without SQL. What the interface
//! gets from it:
//!
//! * **Console** ([`command`], [`reply`]): commands, one per line, split
//!   into words like `redis-cli` does; a reply becomes a grid. Commands that
//!   would hang a graphical client are refused, and the ones that remove
//!   everything or touch the server run only behind the word `CONFIRM`.
//! * **Keys** ([`browse`]): the side panel lists the logical databases
//!   (`db0`…) and, for each, the members of its hashes, lists, sets, sorted
//!   sets and streams as lists of their own. Keys are found with `SCAN`.
//! * **Changes** ([`edit`]): a cell, a new row and a removed row become the
//!   commands of the type, checked against what the grid showed.
//! * **Bytes** ([`value`]): keys and values that are not text are shown as
//!   `0x…` and are never typed over.
//!
//! One connection talks to one server. Cluster mode and Sentinel are not
//! followed: a node of a cluster works for the keys it holds and answers the
//! others with `MOVED`, which is explained in words.
use super::{PageRequest, RowChange, Session};
use crate::engine::{
    Badge, BrowseModel, Capabilities, ConnectRequest, ConnectionForm, Dialect, Engine, EngineSpec,
    FieldUse, Group, OptionField, OptionKind, ProductSpec, QueryLanguage,
};
use crate::model::{Cell, ColumnInfo, QueryResult, ResultColumn, TableRef, TlsMode};
use anyhow::{Result, anyhow, bail, ensure};
use futures_util::future::BoxFuture;
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

mod browse;
mod command;
mod edit;
mod link;
mod reply;
mod value;

use browse::{Container, Kind, Plan, Scans, View};
use command::{Guard, Line};
use link::{Fail, Link, Protocol, Target, command as words};
use value::{reply_int, reply_text};

pub const ENGINE: Engine = Engine::new(&SPEC);

pub static SPEC: EngineSpec = EngineSpec {
    id: "redis",
    aliases: &["valkey", "rediss", "keydb", "dragonfly"],
    portable_name: "redis",
    label: "Redis",
    available: true,
    form: ConnectionForm {
        // The logical database is a number: the option below.
        database: FieldUse::Hidden,
        ..ConnectionForm::SERVER
    },
    capabilities: Capabilities {
        language: QueryLanguage::Command,
        browse: BrowseModel::Keys,
        schemas: false,
        filter: true,
        // Within limits: see `browse::SORT_LIMIT`.
        sort: true,
        edit_cells: true,
        insert_rows: true,
        delete_rows: true,
        table_designer: false,
        query_builder: false,
        import: true,
        export: true,
        monitor: true,
    },
    dialect: Dialect::ANSI,
    options: &[
        OptionField {
            key: "database_index",
            label: ("Numer bazy", "Database index"),
            kind: OptionKind::Number { min: 0, max: 9999 },
            default: "0",
            placeholder: "",
            help: Some((
                "Baza logiczna, w której pracuje konsola. Samodzielny serwer ma ich zwykle 16 (0–15); węzeł klastra i część usług w chmurze tylko bazę 0.",
                "The logical database the console works in. A standalone server usually has 16 of them (0–15); a cluster node and some cloud services only have database 0.",
            )),
            required: false,
        },
        OptionField {
            key: "protocol",
            label: ("Protokół", "Protocol"),
            kind: OptionKind::Choice(&["auto", "resp3", "resp2"]),
            default: "auto",
            placeholder: "",
            help: Some((
                "„auto” wybiera RESP3, a gdy serwer go nie zna (Redis starszy niż 6), RESP2.",
                "“auto” chooses RESP3, and RESP2 when the server does not know it (Redis older than 6).",
            )),
            required: false,
        },
    ],
    products: PRODUCTS,
    connect,
    describe: Some(describe),
};

/// What every product of the family shares: one server per connection.
const ONE_SERVER: (&str, &str) = (
    "Połączenie z jednym serwerem. Tryb klastra i Sentinel nie są jeszcze obsługiwane: przy klastrze wskaż jeden węzeł (klucze pozostałych węzłów odpowiedzą komunikatem MOVED), a przy Sentinelu podaj adres aktualnego serwera głównego.",
    "A connection to one server. Cluster mode and Sentinel are not supported yet: for a cluster name one node (keys of the other nodes answer with MOVED), and with Sentinel give the address of the current primary.",
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
        default_port: 6379,
        default_tls: TlsMode::Disable,
        note: Some(ONE_SERVER),
        ..ProductSpec::GENERIC
    }
}

static PRODUCTS: &[ProductSpec] = &[
    ProductSpec {
        keywords: &["key", "value", "cache", "resp", "rediss"],
        ..product("redis", "Redis", Group::KeyValue, "Re", [216, 44, 32])
    },
    ProductSpec {
        keywords: &["key", "value", "cache", "redis", "linux foundation"],
        ..product("valkey", "Valkey", Group::KeyValue, "Va", [91, 110, 225])
    },
    ProductSpec {
        keywords: &["key", "value", "cache", "redis", "snap"],
        note: Some((
            "Zgodny z protokołem Redis (poziom wersji 6). Połączenie z jednym serwerem; tryb klastra nie jest jeszcze obsługiwany.",
            "Speaks the Redis protocol (at the level of version 6). A connection to one server; cluster mode is not supported yet.",
        )),
        ..product("keydb", "KeyDB", Group::KeyValue, "Kd", [196, 62, 86])
    },
    ProductSpec {
        keywords: &["key", "value", "cache", "redis", "dragonflydb"],
        note: Some((
            "Zgodny z protokołem Redis, ale nie zna każdego polecenia: nieznane odrzuci z komunikatem. Połączenie z jednym serwerem.",
            "Speaks the Redis protocol but does not know every command: an unknown one is refused with a message. A connection to one server.",
        )),
        ..product(
            "dragonfly",
            "Dragonfly",
            Group::KeyValue,
            "Df",
            [124, 77, 214],
        )
    },
    ProductSpec {
        default_tls: TlsMode::VerifyFull,
        keywords: &["aws", "amazon", "elasticache", "redis", "valkey", "cache"],
        note: Some((
            "Przy włączonym szyfrowaniu w tranzycie zostaw TLS; bez niego wybierz tryb „Wyłączony”. Serwer jest dostępny tylko z sieci VPC: użyj tunelu SSH przez host pośredni. Hasłem jest token AUTH albo hasło użytkownika RBAC. Tryb klastra nie jest jeszcze obsługiwany: wskaż punkt końcowy jednego węzła albo grupę bez trybu klastra.",
            "With in-transit encryption keep TLS on; without it choose the mode “Disabled”. The server is reachable from its VPC only: use an SSH tunnel through a bastion host. The password is the AUTH token or the password of an RBAC user. Cluster mode is not supported yet: name the endpoint of one node, or use a group with cluster mode disabled.",
        )),
        ..product(
            "amazon-elasticache",
            "Amazon ElastiCache",
            Group::Cloud,
            "Ec",
            [82, 127, 255],
        )
    },
    ProductSpec {
        default_tls: TlsMode::VerifyFull,
        keywords: &["aws", "amazon", "memorydb", "redis", "valkey"],
        note: Some((
            "Zawsze szyfrowany i zawsze w trybie klastra: przy jednym shardzie wszystkie klucze obsługuje jeden węzeł i wszystko działa; przy wielu shardach klucze innych węzłów odpowiedzą komunikatem MOVED. Dostępny tylko z sieci VPC (tunel SSH). Podaj użytkownika i hasło z listy ACL.",
            "Always encrypted and always in cluster mode: with one shard a single node serves every key and everything works; with several shards the keys of other nodes answer with MOVED. Reachable from its VPC only (SSH tunnel). Give a user and a password of its ACL.",
        )),
        ..product(
            "amazon-memorydb",
            "Amazon MemoryDB",
            Group::Cloud,
            "Md",
            [61, 105, 235],
        )
    },
    ProductSpec {
        default_port: 6380,
        default_tls: TlsMode::VerifyFull,
        keywords: &["azure", "microsoft", "cache", "redis", "managed redis"],
        note: Some((
            "Port 6380 z szyfrowaniem (Azure Managed Redis: 10000). Pole użytkownika zostaw puste, hasłem jest klucz dostępu. Logowanie przez Microsoft Entra ID i warstwy w trybie klastra nie są jeszcze obsługiwane.",
            "Port 6380 with encryption (Azure Managed Redis: 10000). Leave the user empty; the password is the access key. Sign-in with Microsoft Entra ID and the clustered tiers are not supported yet.",
        )),
        ..product(
            "azure-cache-for-redis",
            "Azure Cache for Redis",
            Group::Cloud,
            "Ac",
            [0, 120, 212],
        )
    },
    ProductSpec {
        keywords: &["google", "gcp", "memorystore", "redis", "valkey", "cache"],
        note: Some((
            "Instancja ma tylko adres prywatny: użyj tunelu SSH przez maszynę w tej samej sieci VPC. Przy włączonym szyfrowaniu w tranzycie port to 6378, a pełna weryfikacja wymaga pliku CA instancji na karcie SSL/TLS. Hasłem jest ciąg AUTH instancji. Memorystore for Redis Cluster nie jest jeszcze obsługiwany.",
            "The instance has a private address only: use an SSH tunnel through a machine in the same VPC. With in-transit encryption the port is 6378 and full verification needs the CA file of the instance on the SSL/TLS tab. The password is the AUTH string of the instance. Memorystore for Redis Cluster is not supported yet.",
        )),
        ..product(
            "google-memorystore",
            "Google Memorystore",
            Group::Cloud,
            "Gm",
            [66, 133, 244],
        )
    },
    ProductSpec {
        default_tls: TlsMode::VerifyFull,
        keywords: &["upstash", "serverless", "redis", "cache"],
        note: Some((
            "Szyfrowanie jest wymagane. Podaj samo hasło z panelu Upstash, pole użytkownika zostaw puste. Baza ma tylko numer 0.",
            "Encryption is required. Give only the password from the Upstash console and leave the user empty. There is only database 0.",
        )),
        ..product("upstash", "Upstash", Group::Cloud, "Up", [0, 150, 110])
    },
];

/// Longest a command of the console, a page or an import may take.
const DEADLINE: Duration = Duration::from_secs(120);
/// Longest a change of the grid or a look at the server may take: these
/// are a few commands, so more than this is a connection that went quiet.
const SHORT_DEADLINE: Duration = Duration::from_secs(30);
/// Most keys of a database `KEYS` walks without being asked twice: as many
/// rows as a result holds.
const KEYS_LIMIT: i64 = crate::sql::MAX_RESULT_ROWS as i64;
/// Most clients the monitor lists one by one.
const MONITOR_CLIENTS: usize = 6;

/// A sentence for a result code of this driver. A code that ends in
/// `_at_command_N` is the code of the N-th command of a console text.
fn describe(code: &str, en: bool) -> Option<&'static str> {
    if let Some((base, number)) = code.rsplit_once("_at_command_")
        && let Ok(number) = number.parse::<u32>()
        && let Some(text) = sentence(base, en)
    {
        return Some(numbered(base, number, text, en));
    }
    sentence(code, en)
}

/// The sentence of a code with the number of the command in front. The
/// signature of `describe` returns text that lives for ever, so a text made
/// here is kept: at most a few hundred short ones, then the plain sentence.
fn numbered(base: &str, number: u32, text: &'static str, en: bool) -> &'static str {
    type Kept = Mutex<HashMap<(String, u32, bool), &'static str>>;
    static KEPT: OnceLock<Kept> = OnceLock::new();
    let Ok(mut kept) = KEPT.get_or_init(Default::default).lock() else {
        return text;
    };
    let key = (base.to_owned(), number, en);
    if let Some(found) = kept.get(&key) {
        return found;
    }
    if kept.len() >= 400 {
        return text;
    }
    let made = if en {
        format!(
            "Command {number}: {text} The commands before it were run, the ones after it were not."
        )
    } else {
        format!(
            "Polecenie nr {number}: {text} Wcześniejsze polecenia zostały wykonane, kolejne nie."
        )
    };
    let made: &'static str = Box::leak(made.into_boxed_str());
    kept.insert(key, made);
    made
}

fn sentence(code: &str, en: bool) -> Option<&'static str> {
    let (pl, english) = match code {
        "redis_handshake_failed" => (
            "Serwer nie odpowiedział jak serwer Redis. Jeśli przyjmuje tylko połączenia szyfrowane, włącz szyfrowanie na karcie SSL/TLS; sprawdź też adres i port.",
            "The server did not answer like a Redis server. If it accepts encrypted connections only, switch encryption on on the SSL/TLS tab; check the address and the port as well.",
        ),
        "redis_protocol_unsupported" => (
            "Ten serwer nie obsługuje protokołu RESP3. W ustawieniach połączenia wybierz protokół „auto” albo „resp2”.",
            "This server does not speak RESP3. Choose the protocol “auto” or “resp2” in the connection settings.",
        ),
        "redis_database_index_invalid" => (
            "Serwer nie ma bazy o tym numerze. Samodzielny serwer ma zwykle bazy 0–15, a węzeł klastra i część usług w chmurze tylko bazę 0. Popraw pole „Numer bazy” albo polecenie SELECT.",
            "The server has no database with this index. A standalone server usually has databases 0–15; a cluster node and some cloud services only have database 0. Correct the “Database index” field or the SELECT command.",
        ),
        "redis_cluster_moved" => (
            "Ten klucz należy do innego węzła klastra (odpowiedź MOVED albo ASK). Ta wersja łączy się z jednym serwerem naraz i nie podąża za przekierowaniami: połącz się z węzłem, który obsługuje ten slot. CLUSTER NODES w konsoli pokaże węzły, a CLUSTER KEYSLOT klucz poda numer slotu.",
            "This key belongs to another node of the cluster (a MOVED or ASK reply). This version talks to one server at a time and does not follow redirections: connect to the node that serves the slot. CLUSTER NODES in the console lists the nodes, and CLUSTER KEYSLOT key gives the slot number.",
        ),
        "redis_cluster_down" => (
            "Klaster nie obsługuje teraz poleceń (CLUSTERDOWN): część slotów nie ma swojego węzła.",
            "The cluster is not serving commands now (CLUSTERDOWN): some slots have no node.",
        ),
        "redis_cluster_cross_slot" => (
            "Klucze tego polecenia leżą w różnych slotach klastra (CROSSSLOT). W klastrze jedno polecenie może dotyczyć tylko kluczy z jednego slotu.",
            "The keys of this command live in different slots of the cluster (CROSSSLOT). In a cluster one command can only touch keys of one slot.",
        ),
        "redis_server_not_ready" => (
            "Serwer nie jest jeszcze gotowy: wczytuje dane albo trwa przełączanie węzłów. Spróbuj ponownie za chwilę.",
            "The server is not ready yet: it is loading its data or its nodes are being switched. Try again in a moment.",
        ),
        "redis_read_only_replica" => (
            "Ten serwer jest repliką tylko do odczytu. Zmiany wykonuj na serwerze głównym.",
            "This server is a read-only replica. Make changes on the primary server.",
        ),
        "redis_no_permission" => (
            "To konto nie ma uprawnień do tego polecenia albo do tego klucza (NOPERM). Uprawnienia nadaje administrator serwera poleceniem ACL SETUSER.",
            "This account has no permission for this command or this key (NOPERM). The administrator of the server grants permissions with ACL SETUSER.",
        ),
        "redis_busy_script" => (
            "Serwer jest zajęty wykonywaniem skryptu (BUSY). Poczekaj albo zatrzymaj skrypt poleceniem SCRIPT KILL.",
            "The server is busy running a script (BUSY). Wait, or stop the script with SCRIPT KILL.",
        ),
        "redis_no_script" => (
            "Serwer nie zna skryptu o tym skrócie SHA1 (NOSCRIPT). Wczytaj go poleceniem SCRIPT LOAD albo użyj EVAL.",
            "The server knows no script with this SHA1 digest (NOSCRIPT). Load it with SCRIPT LOAD or use EVAL.",
        ),
        "redis_out_of_memory" => (
            "Serwer osiągnął limit pamięci (OOM) i odrzuca polecenia, które zapisują dane.",
            "The server reached its memory limit (OOM) and refuses commands that write data.",
        ),
        "redis_misconfigured" => (
            "Serwer odrzuca zapisy, bo nie może utrwalić danych na dysku (MISCONF). Sprawdź dziennik serwera.",
            "The server refuses writes because it cannot save its data to disk (MISCONF). Check the server log.",
        ),
        "redis_exec_aborted" => (
            "Transakcja została odrzucona, bo jedno z poleceń po MULTI było błędne (EXECABORT). Nic nie zostało wykonane.",
            "The transaction was discarded because a command after MULTI was wrong (EXECABORT). Nothing was run.",
        ),
        "redis_no_group" => (
            "Strumień albo grupa odbiorców o tej nazwie nie istnieje (NOGROUP).",
            "The stream or the consumer group with this name does not exist (NOGROUP).",
        ),
        "redis_group_exists" => (
            "Grupa odbiorców o tej nazwie już istnieje (BUSYGROUP).",
            "A consumer group with this name already exists (BUSYGROUP).",
        ),
        "redis_no_replicas" => (
            "Zbyt mało replik potwierdza zapisy (NOREPLICAS), więc serwer ich nie przyjmuje.",
            "Too few replicas acknowledge writes (NOREPLICAS), so the server does not accept them.",
        ),
        "redis_script_not_stopped" => (
            "Skryptu nie da się teraz zatrzymać: żaden nie działa albo działający zdążył już zapisać dane.",
            "The script cannot be stopped now: none is running, or the running one has already written data.",
        ),
        "redis_wrong_type" => (
            "Ten klucz przechowuje wartość innego typu niż ten, na którym działa polecenie (WRONGTYPE). Typ klucza pokazuje kolumna „type” na liście kluczy albo polecenie TYPE.",
            "This key holds a value of another type than the one the command works on (WRONGTYPE). The type of a key is in the “type” column of the key list, or ask with TYPE.",
        ),
        "redis_unknown_command" => (
            "Serwer nie zna tego polecenia albo podpolecenia. Sprawdź pisownię; polecenia modułów działają tylko tam, gdzie moduł jest wczytany.",
            "The server does not know this command or subcommand. Check the spelling; commands of a module only work where the module is loaded.",
        ),
        "redis_wrong_arguments" => (
            "Polecenie ma złą liczbę argumentów. Wartość ze spacjami ujmij w cudzysłów: SET \"mój klucz\" \"wartość ze spacjami\".",
            "The command has the wrong number of arguments. Put a value with spaces in quotes: SET \"my key\" \"value with spaces\".",
        ),
        "redis_syntax_error" => (
            "Serwer nie zrozumiał składni polecenia. Sprawdź kolejność argumentów i nazwy opcji.",
            "The server did not understand the syntax of the command. Check the order of the arguments and the names of the options.",
        ),
        "redis_not_an_integer" => (
            "Wartość nie jest liczbą całkowitą albo wychodzi poza zakres.",
            "The value is not an integer or is out of range.",
        ),
        "redis_not_a_float" => ("Wartość nie jest liczbą.", "The value is not a number."),
        "redis_no_such_key" => ("Nie ma takiego klucza.", "There is no such key."),
        "redis_index_out_of_range" => (
            "Lista nie ma elementu o tym indeksie.",
            "The list has no element at this index.",
        ),
        "redis_exec_without_multi" => (
            "EXEC albo DISCARD bez wcześniejszego MULTI: nie ma otwartej transakcji.",
            "EXEC or DISCARD without MULTI before it: no transaction is open.",
        ),
        "redis_multi_nested" => (
            "Transakcja jest już otwarta: MULTI nie można zagnieżdżać. Zakończ ją poleceniem EXEC albo DISCARD.",
            "A transaction is already open: MULTI cannot be nested. End it with EXEC or DISCARD.",
        ),
        "redis_stream_id_invalid" => (
            "Identyfikator wpisu strumienia jest nieprawidłowy albo nie jest większy od ostatniego. Użyj * (serwer nada kolejny) albo postaci milisekundy-numer, większej od ostatniej; w jednym imporcie identyfikatory jednego strumienia są albo wszystkie nadawane, albo wszystkie podane.",
            "The id of the stream entry is invalid or not greater than the last one. Use * (the server assigns the next one) or the form milliseconds-sequence, greater than the last; within one import the ids of one stream are either all assigned or all given.",
        ),
        "redis_ttl_invalid" => (
            "Czas życia podaj w pełnych sekundach, co najmniej 1. Pusta wartość (NULL) oznacza klucz, który nie wygasa.",
            "Give the lifetime in whole seconds, at least 1. An empty value (NULL) means a key that does not expire.",
        ),
        "redis_command_failed" => (
            "Serwer odrzucił polecenie. Sprawdź jego nazwę, argumenty i typ klucza.",
            "The server rejected the command. Check its name, its arguments and the type of the key.",
        ),
        "redis_quote_unterminated" => (
            "Cudzysłów nie jest zamknięty albo tuż za zamykającym stoi znak. Wartość ze spacjami zapisz tak: SET \"mój klucz\" \"wartość\". W cudzysłowie \\\" oznacza cudzysłów, a \\xHH jeden bajt.",
            "A quote is not closed, or a character follows a closing quote directly. Write a value with spaces like this: SET \"my key\" \"value\". Inside quotes \\\" is a quote and \\xHH one byte.",
        ),
        "redis_stream_refused" => (
            "To polecenie zamienia połączenie w niekończący się strumień komunikatów (MONITOR, SUBSCRIBE…), którego siatka wyników nie pokaże, więc konsola go nie wysyła. Do podglądu na żywo użyj redis-cli.",
            "This command turns the connection into an endless stream of messages (MONITOR, SUBSCRIBE…), which a result grid cannot show, so the console does not send it. Use redis-cli to watch a server live.",
        ),
        "redis_session_refused" => (
            "To polecenie zmienia samo połączenie (logowanie, protokół, sposób odpowiadania), którym zarządza aplikacja. Konto, hasło i protokół ustawia się w profilu połączenia.",
            "This command changes the connection itself (sign-in, protocol, how replies are sent), which the application manages. The account, the password and the protocol are set in the connection profile.",
        ),
        "redis_wait_forever" => (
            "To polecenie czekałoby bez końca (limit czasu 0). Podaj limit w sekundach, na przykład BLPOP kolejka 5; czekające polecenie zatrzymuje przycisk Zatrzymaj.",
            "This command would wait for ever (a timeout of 0). Give a timeout in seconds, for example BLPOP queue 5; the Stop button ends a command that waits.",
        ),
        "redis_confirm_data_loss" => (
            "To polecenie usuwa albo przenosi wszystkie klucze i nie da się go cofnąć, więc konsola nie wysyła go od razu. Jeśli naprawdę tego chcesz, poprzedź je słowem CONFIRM, na przykład: CONFIRM FLUSHDB.",
            "This command removes or moves every key and cannot be undone, so the console does not send it straight away. If that is what you want, write CONFIRM before it, for example: CONFIRM FLUSHDB.",
        ),
        "redis_confirm_server" => (
            "To polecenie zatrzymuje, blokuje albo przestawia serwer dla wszystkich jego klientów, więc konsola nie wysyła go od razu. Jeśli naprawdę tego chcesz, poprzedź je słowem CONFIRM, na przykład: CONFIRM CONFIG SET maxmemory 1gb.",
            "This command stops, blocks or reconfigures the server for all of its clients, so the console does not send it straight away. If that is what you want, write CONFIRM before it, for example: CONFIRM CONFIG SET maxmemory 1gb.",
        ),
        "redis_confirm_keys" => (
            "Ta baza ma ponad 20 000 kluczy: KEYS przejrzałoby je wszystkie naraz i zablokowało serwer na ten czas. Użyj SCAN albo listy kluczy z filtrem; jeśli mimo to chcesz, napisz CONFIRM KEYS wzorzec.",
            "This database holds more than 20,000 keys: KEYS would walk all of them at once and block the server meanwhile. Use SCAN or the key list with a filter; if you want it anyway, write CONFIRM KEYS pattern.",
        ),
        "redis_container_unknown" => (
            "Nie ma takiej listy. Odśwież drzewo połączenia.",
            "There is no such list. Refresh the tree of the connection.",
        ),
        "redis_sort_limit" => (
            "Sortowanie działa do 20 000 wierszy, bo cała lista musi zostać wczytana i ułożona. Zawęź ją filtrem i posortuj ponownie.",
            "Sorting works on up to 20,000 rows, because the whole list has to be read and ordered. Narrow it with a filter and sort again.",
        ),
        "redis_sort_column" => (
            "Lista kluczy sortuje się według kolumn key, type, ttl i size. Sortowanie według wartości wymagałoby wczytania wartości każdego klucza.",
            "The key list is sorted by the columns key, type, ttl and size. Sorting by value would need the value of every key.",
        ),
        "redis_page_too_deep" => (
            "Lista sięga do 100 000. wiersza jednego przeglądania. Zawęź ją filtrem, na przykład fragmentem nazwy klucza.",
            "A list reaches as far as row 100,000 of one scan. Narrow it with a filter, for example a part of the key name.",
        ),
        "redis_key_required" => (
            "Podaj nazwę klucza. W oknie nowego wiersza odznacz „Pomiń” przy kolumnie key i wpisz nazwę.",
            "Give the key a name. In the new row dialog untick “Omit” next to the key column and type the name.",
        ),
        "redis_key_exists" => (
            "Klucz o tej nazwie już istnieje. Wybierz inną nazwę albo zmień istniejący klucz.",
            "A key with this name already exists. Choose another name or change the existing key.",
        ),
        "redis_member_exists" => (
            "Ten klucz ma już pole albo element o tej nazwie. Zmień istniejący wiersz, zamiast dodawać nowy.",
            "This key already has a field or a member with this name. Change the existing row instead of adding one.",
        ),
        "redis_member_required" => (
            "Podaj nazwę pola albo elementu. W oknie nowego wiersza odznacz „Pomiń” przy tej kolumnie.",
            "Give the field or the member a name. In the new row dialog untick “Omit” next to that column.",
        ),
        "redis_value_required" => (
            "Podaj wartość. Dla typów hash, list, set, zset i stream jest nią tekst JSON; w oknie nowego wiersza odznacz przy niej „Pomiń” i NULL.",
            "Give a value. For the types hash, list, set, zset and stream it is a JSON text; in the new row dialog untick “Omit” and NULL next to it.",
        ),
        "redis_type_unknown" => (
            "Nieznany typ klucza. Dozwolone: string, hash, list, set, zset, stream, json.",
            "Unknown key type. Allowed: string, hash, list, set, zset, stream, json.",
        ),
        "redis_score_invalid" => (
            "Wynik (score) musi być liczbą, na przykład 1.5, albo inf / -inf.",
            "A score must be a number, for example 1.5, or inf / -inf.",
        ),
        "redis_column_read_only" => (
            "Tej kolumny nie zmienia się wprost: typ i rozmiar wynikają z wartości klucza, a indeks z miejsca na liście.",
            "This column is not changed directly: the type and the size follow from the value of the key, and the index from the place in the list.",
        ),
        "redis_member_key_fixed" => (
            "Wiersz elementu należy do swojego klucza i nie da się go przenieść. Nazwę klucza zmienia się na liście kluczy (db0, db1…).",
            "A member row belongs to its key and cannot be moved. A key is renamed in the key list (db0, db1…).",
        ),
        "redis_value_read_only" => (
            "Ta wartość jest pokazana w skrócie albo nie jest tekstem, więc nie da się jej tu bezpiecznie zmienić ani wczytać z pliku. Elementy dużych kolekcji i wpisy strumieni zmienisz, wyeksportujesz i zaimportujesz na listach „hashes”, „lists”, „streams” i kolejnych, a dane binarne w konsoli (\"\\xHH\").",
            "This value is shown shortened or is not text, so it cannot be changed safely here or read from a file. Change, export and import members of large collections and entries of streams in the lists “hashes”, “lists”, “streams” and so on, and binary data in the console (\"\\xHH\").",
        ),
        "redis_json_shape_invalid" => (
            "Wartość nie jest tekstem JSON o kształcie tego typu. hash: {\"pole\": \"wartość\"}; list i set: [\"a\", \"b\"]; zset: {\"element\": 1.5}. Elementami są teksty i liczby, bez zagnieżdżania.",
            "The value is not a JSON text of the shape of this type. hash: {\"field\": \"value\"}; list and set: [\"a\", \"b\"]; zset: {\"member\": 1.5}. Members are texts and numbers, nothing nested.",
        ),
        "redis_collection_empty" => (
            "Kolekcja nie może być pusta: Redis usuwa klucz bez elementów. Usuń zamiast tego klucz.",
            "A collection cannot be empty: Redis removes a key without members. Delete the key instead.",
        ),
        "redis_list_index_invalid" => (
            "Nowy element trafia na koniec listy (zostaw indeks pusty) albo na jej początek (indeks 0).",
            "A new element goes to the end of the list (leave the index empty) or to its start (index 0).",
        ),
        "redis_stream_entry_fixed" => (
            "Wpisów strumienia nie da się zmieniać. Dodaj nowy wpis albo usuń ten.",
            "Entries of a stream cannot be changed. Add a new entry or delete this one.",
        ),
        "redis_entry_invalid" => (
            "Wpis strumienia podaj jako obiekt JSON z co najmniej jednym polem, na przykład {\"czujnik\": \"a\", \"temperatura\": 21.5}.",
            "Give a stream entry as a JSON object with at least one field, for example {\"sensor\": \"a\", \"temperature\": 21.5}.",
        ),
        "redis_import_incomplete" => (
            "Serwer odrzucił część importu już w trakcie zapisu (na przykład z braku pamięci). Część wierszy została zapisana: odśwież listę i sprawdź dane.",
            "The server refused a part of the import while it was being written (for example for lack of memory). Some rows were written: reload the list and check the data.",
        ),
        _ => return None,
    };
    Some(if en { english } else { pl })
}

/// Every code this driver can answer with that has a sentence here.
#[cfg(test)]
const CODES: &[&str] = &[
    "redis_handshake_failed",
    "redis_protocol_unsupported",
    "redis_database_index_invalid",
    "redis_cluster_moved",
    "redis_cluster_down",
    "redis_cluster_cross_slot",
    "redis_server_not_ready",
    "redis_read_only_replica",
    "redis_no_permission",
    "redis_busy_script",
    "redis_no_script",
    "redis_out_of_memory",
    "redis_misconfigured",
    "redis_exec_aborted",
    "redis_no_group",
    "redis_group_exists",
    "redis_no_replicas",
    "redis_script_not_stopped",
    "redis_wrong_type",
    "redis_unknown_command",
    "redis_wrong_arguments",
    "redis_syntax_error",
    "redis_not_an_integer",
    "redis_not_a_float",
    "redis_no_such_key",
    "redis_index_out_of_range",
    "redis_exec_without_multi",
    "redis_multi_nested",
    "redis_stream_id_invalid",
    "redis_ttl_invalid",
    "redis_command_failed",
    "redis_quote_unterminated",
    "redis_stream_refused",
    "redis_session_refused",
    "redis_wait_forever",
    "redis_confirm_data_loss",
    "redis_confirm_server",
    "redis_confirm_keys",
    "redis_container_unknown",
    "redis_sort_limit",
    "redis_sort_column",
    "redis_page_too_deep",
    "redis_key_required",
    "redis_key_exists",
    "redis_member_exists",
    "redis_member_required",
    "redis_value_required",
    "redis_type_unknown",
    "redis_score_invalid",
    "redis_column_read_only",
    "redis_member_key_fixed",
    "redis_value_read_only",
    "redis_json_shape_invalid",
    "redis_collection_empty",
    "redis_list_index_invalid",
    "redis_stream_entry_fixed",
    "redis_entry_invalid",
    "redis_import_incomplete",
];

/// A logical database of the session: its connection, and the lock that
/// lets one change at a time use `WATCH` on it.
struct Db {
    link: Link,
    edit: tokio::sync::Mutex<()>,
}

/// The connection of the console. It is used by one command at a time, so
/// `SELECT`, `MULTI` and `WATCH` behave as in `redis-cli`.
struct Console {
    link: Option<Link>,
    /// The database the console is in: the one of the profile until a
    /// `SELECT` changes it.
    db: i64,
    /// A `MULTI` of the console is open.
    queueing: bool,
}

struct RedisSession {
    target: Target,
    protocol: Protocol,
    /// The database index of the profile.
    home: i64,
    /// What the server said it is: `Redis 8.2.1, standalone, Linux …`.
    server: String,
    databases: tokio::sync::Mutex<HashMap<i64, Arc<Db>>>,
    console: tokio::sync::Mutex<Console>,
    scans: tokio::sync::Mutex<Scans>,
    /// `SCAN … TYPE` works on this server.
    typed_scan: AtomicBool,
    /// The side panel asked for the containers: a session of the
    /// application, not the one-statement probe of the connection dialog.
    listed: AtomicBool,
    /// The types of the keys the key list last showed, by database.
    shown: Mutex<HashMap<i64, HashMap<Vec<u8>, String>>>,
    /// Keys made through the grid, newest first: shown first on a first
    /// page, where a scan would put them anywhere.
    fresh: Mutex<HashMap<i64, Vec<Vec<u8>>>>,
    /// The key a list of members last showed, when its page held the
    /// members of one key only: a new row without a key belongs to it.
    single: Mutex<HashMap<(i64, Kind), Vec<u8>>>,
}

fn connect<'a>(request: &'a ConnectRequest<'a>) -> BoxFuture<'a, Result<Arc<dyn Session>>> {
    Box::pin(async move {
        let profile = request.profile;
        let target = Target {
            dial_host: request.dial_host(),
            dial_port: request.dial_port(),
            server_name: request.server_name().to_owned(),
            tls: profile.tls,
            ca_file: profile.ca_file.clone(),
            username: profile.username.clone(),
            password: profile.password.clone(),
        };
        let home: i64 = request
            .option("database_index")
            .parse()
            .map_err(|_| anyhow!("connection_option_invalid"))?;
        let (protocol, link) = match request.option("protocol") {
            "resp2" => (
                Protocol::Resp2,
                Link::open(&target, home, Protocol::Resp2).await?,
            ),
            "resp3" => (
                Protocol::Resp3,
                Link::open(&target, home, Protocol::Resp3).await?,
            ),
            _ => match Link::open(&target, home, Protocol::Resp3).await {
                Ok(link) => (Protocol::Resp3, link),
                // A server from before `HELLO`: the older protocol, on a
                // connection of its own.
                Err(fail)
                    if matches!(
                        fail.code(),
                        "redis_protocol_unsupported"
                            | "redis_unknown_command"
                            | "redis_syntax_error"
                    ) =>
                {
                    (
                        Protocol::Resp2,
                        Link::open(&target, home, Protocol::Resp2).await?,
                    )
                }
                Err(fail) => return Err(fail.into()),
            },
        };
        let info = match link.call(&words(["INFO", "server"])).await {
            Ok(reply) => reply::info(&reply_text(&reply).unwrap_or_default()),
            // An account that may not ask: the product of the profile names
            // the server.
            Err(_) => Vec::new(),
        };
        let server = server_label(&info, request.product.label);
        let mut databases = HashMap::new();
        databases.insert(
            home,
            Arc::new(Db {
                link,
                edit: tokio::sync::Mutex::new(()),
            }),
        );
        Ok(Arc::new(RedisSession {
            target,
            protocol,
            home,
            server,
            databases: tokio::sync::Mutex::new(databases),
            console: tokio::sync::Mutex::new(Console {
                link: None,
                db: home,
                queueing: false,
            }),
            scans: tokio::sync::Mutex::new(Scans::default()),
            typed_scan: AtomicBool::new(true),
            listed: AtomicBool::new(false),
            shown: Mutex::new(HashMap::new()),
            fresh: Mutex::new(HashMap::new()),
            single: Mutex::new(HashMap::new()),
        }) as Arc<dyn Session>)
    })
}

/// What the server is, from `INFO server`: the product that answers, its
/// version, its mode and what it runs on.
fn server_label(info: &[(String, String, String)], product: &str) -> String {
    let get = |name: &str| reply::info_value(info, name).filter(|value| !value.is_empty());
    let (actual, version) = if let Some(version) = get("valkey_version") {
        ("Valkey", version)
    } else if let Some(version) = get("dragonfly_version") {
        ("Dragonfly", version.trim_start_matches("df-"))
    } else if let Some(version) = get("redis_version") {
        ("Redis", version)
    } else {
        return product.to_owned();
    };
    // A profile of a server one runs oneself says what answered, whichever
    // of them the profile was saved for. KeyDB and the hosted services
    // report the version they are compatible with: it follows their name.
    let mut label = if matches!(product, "Redis" | "Valkey" | "Dragonfly") {
        format!("{actual} {version}")
    } else {
        format!("{product} ({actual} {version})")
    };
    for extra in [get("redis_mode").or(get("server_mode")), get("os")]
        .into_iter()
        .flatten()
    {
        label.push_str(", ");
        label.push_str(extra);
    }
    // The interface lists a version text under the chart only when it is
    // longer than a figure would be.
    if label.len() <= 14 {
        label.push_str(" server");
    }
    label
}

/// Uptime in the largest two units.
fn uptime(seconds: i64) -> String {
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
    );
    if days > 0 {
        format!("{days} d {hours} h")
    } else if hours > 0 {
        format!("{hours} h {minutes} min")
    } else if minutes > 0 {
        format!("{minutes} min {} s", seconds % 60)
    } else {
        format!("{seconds} s")
    }
}

/// The row of the "Monitor" section out of `INFO` and `CLIENT LIST`.
fn stats(
    info: &[(String, String, String)],
    clients: Option<&reply::Grid>,
    server: &str,
    home: i64,
) -> QueryResult {
    let get = |name: &str| reply::info_value(info, name).unwrap_or_default().to_owned();
    let number = |name: &str| get(name).parse::<i64>().unwrap_or(0);
    let keyspace = browse::databases(info);
    let total: i64 = keyspace.iter().map(|(_, keys, ..)| keys).sum();
    let expiring: i64 = keyspace.iter().map(|(_, _, expires, _)| expires).sum();
    let (hits, misses) = (number("keyspace_hits"), number("keyspace_misses"));
    let mut columns: Vec<(&str, Cell)> = vec![
        // The two large figures.
        (
            "connections",
            Cell::Integer(number("connected_clients").to_string()),
        ),
        ("RAM", Cell::Text(get("used_memory_human"))),
        ("version", Cell::Text(server.to_owned())),
        ("database", Cell::Text(format!("db{home}"))),
        ("keys", Cell::Integer(total.to_string())),
        ("expiring keys", Cell::Integer(expiring.to_string())),
        (
            "ops/s",
            Cell::Integer(number("instantaneous_ops_per_sec").to_string()),
        ),
        ("peak memory", Cell::Text(get("used_memory_peak_human"))),
        (
            "memory limit",
            Cell::Text(match get("maxmemory_human").as_str() {
                "" | "0B" => "none".into(),
                limit => limit.to_owned(),
            }),
        ),
        ("uptime", Cell::Text(uptime(number("uptime_in_seconds")))),
        ("role", Cell::Text(get("role"))),
        (
            "blocked clients",
            Cell::Integer(number("blocked_clients").to_string()),
        ),
        (
            "client limit",
            Cell::Integer(number("maxclients").to_string()),
        ),
        (
            "commands",
            Cell::Integer(number("total_commands_processed").to_string()),
        ),
        (
            "hit rate",
            if hits + misses > 0 {
                Cell::Text(format!(
                    "{:.1}%",
                    hits as f64 * 100.0 / (hits + misses) as f64
                ))
            } else {
                Cell::Null
            },
        ),
        ("expired", Cell::Integer(number("expired_keys").to_string())),
        ("evicted", Cell::Integer(number("evicted_keys").to_string())),
        (
            "rejected connections",
            Cell::Integer(number("rejected_connections").to_string()),
        ),
        (
            "keyspace",
            Cell::Text(if keyspace.is_empty() {
                "empty".into()
            } else {
                keyspace
                    .iter()
                    .map(|(db, keys, expires, _)| {
                        let word = if *keys == 1 { "key" } else { "keys" };
                        if *expires > 0 {
                            format!("db{db}: {keys} {word}, {expires} expiring")
                        } else {
                            format!("db{db}: {keys} {word}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            }),
        ),
    ];
    if let Some(grid) = clients {
        let at = |name: &str| grid.columns.iter().position(|column| column == name);
        let field = |row: &[Cell], name: &str| {
            at(name)
                .and_then(|index| row.get(index))
                .map(Cell::display)
                .filter(|text| !text.is_empty() && text != "NULL")
        };
        let mut listed: Vec<String> = grid
            .rows
            .iter()
            .take(MONITOR_CLIENTS)
            .map(|row| {
                let mut text = format!("#{}", field(row, "id").unwrap_or_default());
                for part in [field(row, "name"), field(row, "addr")]
                    .into_iter()
                    .flatten()
                {
                    text.push(' ');
                    text.push_str(&part);
                }
                if let Some(db) = field(row, "db") {
                    text.push_str(&format!(" db{db}"));
                }
                if let Some(command) = field(row, "cmd") {
                    text.push_str(&format!(" {command}"));
                }
                if let Some(idle) = field(row, "idle") {
                    text.push_str(&format!(" idle {idle} s"));
                }
                text
            })
            .collect();
        if grid.rows.len() > MONITOR_CLIENTS {
            listed.push(format!("+{} more", grid.rows.len() - MONITOR_CLIENTS));
        }
        columns.push(("clients", Cell::Text(listed.join("; "))));
    }
    // A property the server does not report is left out, not shown empty.
    columns.retain(|(_, cell)| !matches!(cell, Cell::Text(text) if text.is_empty()));
    QueryResult {
        columns: columns
            .iter()
            .map(|(name, cell)| ResultColumn {
                name: (*name).into(),
                data_type: match cell {
                    Cell::Integer(_) => "integer",
                    _ => "string",
                }
                .into(),
            })
            .collect(),
        rows: vec![columns.into_iter().map(|(_, cell)| cell).collect()],
        ..QueryResult::default()
    }
}

/// How a run of the console ended, before the futures of the run are gone.
enum Ended<T> {
    Done(T),
    Cancelled,
    TimedOut,
}

impl RedisSession {
    /// The connection of a logical database, opened when first needed and
    /// again after it was lost.
    async fn database(&self, db: i64) -> Result<Arc<Db>> {
        let mut databases = self.databases.lock().await;
        if let Some(found) = databases.get(&db)
            && found.link.alive()
        {
            return Ok(found.clone());
        }
        let link = Link::open(&self.target, db, self.protocol).await?;
        let opened = Arc::new(Db {
            link,
            edit: tokio::sync::Mutex::new(()),
        });
        databases.insert(db, opened.clone());
        Ok(opened)
    }

    /// Close the connections of the databases, so that the next use opens
    /// them anew: after one of them went quiet without closing.
    async fn reconnect_later(&self) {
        for (_, database) in self.databases.lock().await.drain() {
            database.link.close();
        }
    }

    /// Run something of a few commands; when the server does not answer in
    /// time the connection is given up.
    async fn briefly<T>(&self, work: impl Future<Output = Result<T>>) -> Result<T> {
        match tokio::time::timeout(SHORT_DEADLINE, work).await {
            Ok(result) => result,
            Err(_) => {
                self.reconnect_later().await;
                bail!("database_connection_timeout")
            }
        }
    }

    /// Run something longer until it is done, stopped or late.
    async fn bounded<T>(
        &self,
        cancel: CancellationToken,
        work: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("query_cancelled"),
            _ = tokio::time::sleep(DEADLINE) => {}
            result = work => return result,
        }
        self.reconnect_later().await;
        bail!("query_timeout")
    }

    /// The type of a key as the key list last showed it.
    fn shown_type(&self, db: i64, key: &[u8]) -> Option<String> {
        self.shown.lock().ok()?.get(&db)?.get(key).cloned()
    }

    /// The one key the list of members of this kind shows, if it shows one.
    fn single_key(&self, db: i64, kind: Kind) -> Option<Vec<u8>> {
        self.single.lock().ok()?.get(&(db, kind)).cloned()
    }

    /// A key was made through the grid.
    fn remember_new(&self, db: i64, key: &[u8]) {
        if let Ok(mut fresh) = self.fresh.lock() {
            let keys = fresh.entry(db).or_default();
            keys.retain(|known| known != key);
            keys.insert(0, key.to_vec());
            keys.truncate(50);
        }
    }

    async fn console_link<'a>(&self, console: &'a mut Console) -> Result<&'a Link> {
        if !console.link.as_ref().is_some_and(Link::alive) {
            console.queueing = false;
            console.link = Some(Link::open(&self.target, console.db, self.protocol).await?);
        }
        console
            .link
            .as_ref()
            .ok_or_else(|| anyhow!("database_network_error"))
    }

    /// Run one command of the console.
    async fn run_line(&self, console: &mut Console, line: &Line) -> Result<redis::Value> {
        let guard = command::guard(line);
        match guard {
            Guard::Refuse(code) => bail!(code),
            Guard::Confirm(code) if !line.confirmed => bail!(code),
            Guard::Keys if !line.confirmed => {
                let link = self.console_link(console).await?;
                let size = reply_int(&link.call(&words(["DBSIZE"])).await?).unwrap_or(0);
                ensure!(size <= KEYS_LIMIT, "redis_confirm_keys");
            }
            _ => {}
        }
        let link = self.console_link(console).await?;
        let reply = match link.call(&words(&line.words)).await {
            Ok(reply) => reply,
            Err(fail) => {
                if matches!(fail, Fail::Io | Fail::Timeout) {
                    console.link = None;
                }
                return Err(fail.into());
            }
        };
        match line.name().as_str() {
            "SELECT" => {
                if let Some(db) = line
                    .words
                    .get(1)
                    .and_then(|word| std::str::from_utf8(word).ok())
                    .and_then(|word| word.parse().ok())
                {
                    console.db = db;
                }
            }
            "MULTI" => console.queueing = true,
            "EXEC" | "DISCARD" => console.queueing = false,
            _ => {}
        }
        Ok(reply)
    }

    /// Run the commands of a console text in order. One command answers
    /// with its reply as a grid; several answer with one row each.
    async fn run_lines(&self, console: &mut Console, lines: &[Line]) -> Result<QueryResult> {
        if let [line] = lines {
            let reply = self.run_line(console, line).await?;
            return Ok(reply::render(&line.name(), &line.sub(), &line.words, reply).finish());
        }
        let queued_before = console.queueing;
        let mut rows = Vec::with_capacity(lines.len());
        for (index, line) in lines.iter().enumerate() {
            match self.run_line(console, line).await {
                Ok(reply) => rows.push(vec![Cell::Text(line.text.clone()), reply::cell(&reply)]),
                Err(error) => {
                    // A transaction this text opened does not stay open
                    // behind its failed command.
                    if console.queueing && !queued_before {
                        if let Some(link) = console.link.as_ref() {
                            let _ = link.call(&words(["DISCARD"])).await;
                        }
                        console.queueing = false;
                    }
                    let code = error.to_string();
                    // Which command it was is said for the codes of this
                    // driver; a lost connection needs no number.
                    if code.starts_with("redis_") {
                        bail!("{code}_at_command_{}", index + 1);
                    }
                    return Err(error);
                }
            }
        }
        Ok(reply::Grid {
            columns: vec!["command".into(), "reply".into()],
            rows,
        }
        .finish())
    }

    /// End what the console is doing: close its connection, and tell the
    /// server from another one, in case the closed socket is not noticed at
    /// once (a tunnel in between).
    async fn stop_console(&self, console: &mut Console) {
        let Some(link) = console.link.take() else {
            return;
        };
        console.queueing = false;
        link.close();
        if let Some(id) = link.id
            && let Ok(home) = self.database(self.home).await
        {
            let kill = words([
                "CLIENT".to_owned(),
                "KILL".to_owned(),
                "ID".to_owned(),
                id.to_string(),
            ]);
            let _ = tokio::time::timeout(Duration::from_secs(2), home.link.call(&kill)).await;
        }
    }

    async fn console_run(&self, text: &str, cancel: CancellationToken) -> Result<QueryResult> {
        let started = Instant::now();
        let lines = command::lines(text)?;
        if lines.is_empty() {
            // Only comments: nothing to send.
            return Ok(QueryResult::default());
        }
        // The connection dialog tests a connection with the statement
        // `SELECT 1` on every engine. Here that is "switch to database 1",
        // which a server with a single database refuses although the
        // connection is fine. A session that never listed its containers is
        // that test: it is answered by a `PING`.
        if !self.listed.load(Ordering::Relaxed)
            && let [line] = lines.as_slice()
            && line.text.eq_ignore_ascii_case("SELECT 1")
        {
            let home = self.database(self.home).await?;
            let reply = home.link.call(&words(["PING"])).await?;
            let mut result = reply::generic(reply).finish();
            result.duration_ms = started.elapsed().as_millis() as u64;
            return Ok(result);
        }
        if cancel.is_cancelled() {
            bail!("query_cancelled");
        }
        let deadline = tokio::time::sleep(DEADLINE);
        tokio::pin!(deadline);
        let mut console = tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("query_cancelled"),
            _ = &mut deadline => bail!("query_timeout"),
            console = self.console.lock() => console,
        };
        let ended = tokio::select! {
            biased;
            _ = cancel.cancelled() => Ended::Cancelled,
            _ = &mut deadline => Ended::TimedOut,
            result = self.run_lines(&mut console, &lines) => Ended::Done(result),
        };
        match ended {
            Ended::Done(result) => {
                let mut result = result?;
                result.duration_ms = started.elapsed().as_millis() as u64;
                Ok(result)
            }
            Ended::Cancelled => {
                self.stop_console(&mut console).await;
                bail!("query_cancelled")
            }
            Ended::TimedOut => {
                self.stop_console(&mut console).await;
                bail!("query_timeout")
            }
        }
    }

    async fn page(&self, page: &PageRequest) -> Result<QueryResult> {
        let started = Instant::now();
        let mut result = match Container::parse(&page.container.name)? {
            Container::View(view) => {
                let home = self.database(self.home).await?;
                browse::view_page(browse::view(&home.link, view, self.home).await?, page)?
            }
            Container::Data(db, kind) => {
                let plan = Plan::new(db, kind, page)?;
                let database = self.database(db).await?;
                let need = usize::try_from(page.offset)
                    .unwrap_or(usize::MAX)
                    .saturating_add(page.limit as usize);
                let ids = {
                    let mut scans = self.scans.lock().await;
                    let restart = page.offset == 0;
                    let scan = scans.of(&plan, restart);
                    if restart && plan.tests.is_empty() && plan.order.is_none() {
                        let fresh = self
                            .fresh
                            .lock()
                            .ok()
                            .and_then(|fresh| fresh.get(&db).cloned())
                            .unwrap_or_default();
                        scan.seed(fresh, kind != Kind::Keys);
                    }
                    browse::fill(&database.link, &plan, scan, need, &self.typed_scan).await?;
                    scan.page(page.offset, page.limit).to_vec()
                };
                let rows: Vec<Vec<Cell>> = browse::rows(&database.link, kind, &ids)
                    .await?
                    .into_iter()
                    .flatten()
                    .collect();
                if kind == Kind::Keys
                    && let Ok(mut shown) = self.shown.lock()
                {
                    // What the next change of a value cell is checked against.
                    let types = shown.entry(db).or_default();
                    if types.len() > 20_000 {
                        types.clear();
                    }
                    for row in &rows {
                        if let (Some(key), Some(kind)) = (value::bytes_of(&row[0]), row.get(1)) {
                            types.insert(key, kind.display());
                        }
                    }
                }
                if kind != Kind::Keys
                    && let Ok(mut single) = self.single.lock()
                {
                    match ids.split_first() {
                        Some((first, rest)) if rest.iter().all(|id| id.key == first.key) => {
                            single.insert((db, kind), first.key.clone());
                        }
                        _ => {
                            single.remove(&(db, kind));
                        }
                    }
                }
                browse::result_of(kind, rows)
            }
        };
        result.duration_ms = started.elapsed().as_millis() as u64;
        Ok(result)
    }
}

impl Session for RedisSession {
    fn dialect(&self) -> &Dialect {
        &Dialect::ANSI
    }

    fn execute<'a>(
        &'a self,
        text: &'a str,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(self.console_run(text, cancel))
    }

    fn containers<'a>(&'a self) -> BoxFuture<'a, Result<Vec<TableRef>>> {
        Box::pin(self.briefly(async move {
            self.listed.store(true, Ordering::Relaxed);
            let home = self.database(self.home).await?;
            // The databases that hold keys, and the one of the profile.
            let mut databases = BTreeSet::from([self.home]);
            if let Ok(reply) = home.link.call(&words(["INFO", "keyspace"])).await {
                let info = reply::info(&reply_text(&reply).unwrap_or_default());
                databases.extend(browse::databases(&info).into_iter().map(|(db, ..)| db));
            }
            let mut containers = Vec::new();
            for db in databases {
                for kind in [Kind::Keys].into_iter().chain(Kind::MEMBERS) {
                    containers.push(TableRef {
                        schema: String::new(),
                        name: Container::Data(db, kind).name(),
                        kind: "table".into(),
                    });
                }
            }
            containers.extend(View::ALL.into_iter().map(|view| TableRef {
                schema: String::new(),
                name: view.name().into(),
                kind: "view".into(),
            }));
            Ok(containers)
        }))
    }

    fn columns<'a>(&'a self, container: &'a TableRef) -> BoxFuture<'a, Result<Vec<ColumnInfo>>> {
        Box::pin(async move { Ok(Container::parse(&container.name)?.column_info()) })
    }

    fn server_stats<'a>(&'a self) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(self.briefly(async move {
            let home = self.database(self.home).await?;
            let started = Instant::now();
            let reply = home.link.call(&words(["INFO"])).await?;
            let duration = started.elapsed().as_millis() as u64;
            let info = reply::info(&reply_text(&reply).unwrap_or_default());
            // One line per client is read only while they are few.
            let few = reply::info_value(&info, "connected_clients")
                .and_then(|count| count.parse::<usize>().ok())
                .is_some_and(|count| count <= 200);
            let clients = if few {
                match home.link.call(&words(["CLIENT", "LIST"])).await {
                    Ok(reply) => Some(reply::clients(&reply_text(&reply).unwrap_or_default())),
                    Err(_) => None,
                }
            } else {
                None
            };
            let mut result = stats(&info, clients.as_ref(), &self.server, self.home);
            result.duration_ms = duration;
            Ok(result)
        }))
    }

    fn close<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(link) = self.console.lock().await.link.take() {
                link.close();
            }
            for (_, database) in self.databases.lock().await.drain() {
                database.link.close();
            }
        })
    }

    fn page_text(&self, page: &PageRequest) -> Result<String> {
        browse::page_text(page, self.home)
    }

    fn read_page<'a>(
        &'a self,
        page: &'a PageRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(self.bounded(cancel, self.page(page)))
    }

    fn change_text(&self, change: &RowChange) -> Result<String> {
        edit::change_text(self, change)
    }

    fn apply<'a>(&'a self, change: &'a RowChange) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.briefly(edit::apply(self, change)))
    }

    fn import_rows<'a>(
        &'a self,
        container: &'a TableRef,
        columns: &'a [String],
        rows: &'a [Vec<Cell>],
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(self.bounded(
            cancel.clone(),
            edit::import(self, container, columns, rows, cancel),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_a_sentence_in_both_languages() {
        for code in CODES {
            for en in [false, true] {
                let text = describe(code, en).unwrap_or_else(|| panic!("{code} has no sentence"));
                assert!(text.contains(' ') && text.ends_with('.'), "{code}: {text}");
                // The sentence explains; it does not repeat the code.
                assert!(!text.contains(code), "{code}");
            }
            assert_ne!(describe(code, false), describe(code, true), "{code}");
        }
        assert_eq!(describe("database_login_rejected", true), None);
        assert_eq!(describe("redis_made_up", true), None);
    }

    #[test]
    fn the_guards_and_the_server_codes_all_have_sentences() {
        for line in ["MONITOR", "AUTH x", "BLPOP q 0", "FLUSHALL", "SHUTDOWN"] {
            let parsed = &command::lines(line).unwrap()[0];
            let code = match command::guard(parsed) {
                Guard::Refuse(code) | Guard::Confirm(code) => code,
                other => panic!("{line}: {other:?}"),
            };
            assert!(CODES.contains(&code), "{code}");
        }
        for (word, detail) in [
            ("WRONGTYPE", ""),
            ("MOVED", "1 a:1"),
            ("ERR", "unknown command"),
            ("ERR", "anything else"),
            ("BUSY", ""),
            ("OOM", ""),
            ("NOPERM", ""),
            ("READONLY", ""),
            ("CLUSTERDOWN", ""),
        ] {
            let code = link::server_code(word, detail);
            assert!(CODES.contains(&code), "{word}: {code}");
        }
    }

    #[test]
    fn a_failed_command_of_a_text_is_named_by_its_number() {
        let text = describe("redis_wrong_type_at_command_3", true).unwrap();
        assert!(text.starts_with("Command 3: This key holds"), "{text}");
        assert!(text.ends_with("the ones after it were not."), "{text}");
        let polish = describe("redis_wrong_type_at_command_3", false).unwrap();
        assert!(polish.starts_with("Polecenie nr 3: Ten klucz"), "{polish}");
        // The same text is handed out again, not made again.
        assert!(std::ptr::eq(
            text,
            describe("redis_wrong_type_at_command_3", true).unwrap()
        ));
        assert_eq!(describe("redis_made_up_at_command_3", true), None);
        assert_eq!(describe("redis_wrong_type_at_command_x", true), None);
        // The interface takes it for a code and asks the driver.
        let code = "redis_wrong_type_at_command_3";
        assert!(
            code.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        );
    }

    #[test]
    fn the_products_are_presets_of_one_driver() {
        let spec = ENGINE.spec();
        assert_eq!(spec.generic().id, "redis");
        assert_eq!(Engine::from_id("Valkey"), Some(ENGINE));
        assert_eq!(Engine::from_id("rediss"), Some(ENGINE));
        let ids: Vec<&str> = spec.products.iter().map(|product| product.id).collect();
        assert_eq!(
            ids,
            [
                "redis",
                "valkey",
                "keydb",
                "dragonfly",
                "amazon-elasticache",
                "amazon-memorydb",
                "azure-cache-for-redis",
                "google-memorystore",
                "upstash",
            ]
        );
        for product in spec.products {
            // Every product says what one connection can and cannot reach.
            assert!(product.note.is_some(), "{}", product.id);
            assert!(product.capabilities.is_none() && product.dialect.is_none());
        }
        let tls = |id: &str| ENGINE.product(id).default_tls;
        assert_eq!(tls("redis"), TlsMode::Disable);
        assert_eq!(tls("valkey"), TlsMode::Disable);
        for hosted in [
            "amazon-elasticache",
            "amazon-memorydb",
            "azure-cache-for-redis",
            "upstash",
        ] {
            assert_eq!(tls(hosted), TlsMode::VerifyFull, "{hosted}");
        }
        assert_eq!(ENGINE.product("azure-cache-for-redis").default_port, 6380);
        assert!(ENGINE.product("upstash").matches("serverless redis"));
        assert!(ENGINE.product("amazon-memorydb").matches("aws"));
        // The form: no database name, an optional account, TLS and SSH.
        assert_eq!(spec.form.database, FieldUse::Hidden);
        assert!(spec.form.tls && spec.form.ssh);
        assert_eq!(spec.capabilities.language, QueryLanguage::Command);
        assert_eq!(spec.capabilities.browse, BrowseModel::Keys);
        assert!(!spec.capabilities.schemas && !spec.capabilities.table_designer);
        assert!(!spec.capabilities.query_builder);
    }

    #[test]
    fn a_profile_takes_its_database_index_and_protocol_from_the_options() {
        let mut profile = crate::model::ConnectionProfile {
            name: "Cache".into(),
            engine: ENGINE,
            host: "cache.example.test".into(),
            port: 6379,
            database: String::new(),
            tls: TlsMode::Disable,
            ..crate::model::ConnectionProfile::default()
        };
        profile.validate().unwrap();
        assert_eq!(profile.option("database_index"), "0");
        assert_eq!(profile.option("protocol"), "auto");
        profile.set_option("database_index", "3");
        profile.set_option("protocol", "resp2");
        profile.validate().unwrap();
        profile.set_option("database_index", "-1");
        assert_eq!(
            profile.validate().unwrap_err().to_string(),
            "connection_option_invalid"
        );
        profile.set_option("database_index", "3");
        profile.set_option("protocol", "resp9");
        assert_eq!(
            profile.validate().unwrap_err().to_string(),
            "connection_option_invalid"
        );
    }

    fn info(text: &str) -> Vec<(String, String, String)> {
        reply::info(text)
    }

    #[test]
    fn the_server_says_what_it_is() {
        let redis =
            info("# Server\nredis_version:8.2.1\nredis_mode:standalone\nos:Linux 6.8.0 aarch64\n");
        assert_eq!(
            server_label(&redis, "Redis"),
            "Redis 8.2.1, standalone, Linux 6.8.0 aarch64"
        );
        // Valkey also reports the Redis version it is compatible with.
        let valkey = info(
            "# Server\nredis_version:7.2.4\nserver_name:valkey\nvalkey_version:8.0.1\nredis_mode:standalone\nos:Linux 6.8.0 aarch64\n",
        );
        assert_eq!(
            server_label(&valkey, "Valkey"),
            "Valkey 8.0.1, standalone, Linux 6.8.0 aarch64"
        );
        // A profile saved as "Redis" that points at Valkey is told so.
        assert!(server_label(&valkey, "Redis").starts_with("Valkey 8.0.1"));
        assert!(server_label(&redis, "Valkey").starts_with("Redis 8.2.1"));
        assert!(
            server_label(&valkey, "Amazon ElastiCache")
                .starts_with("Amazon ElastiCache (Valkey 8.0.1)")
        );
        assert!(server_label(&redis, "KeyDB").starts_with("KeyDB (Redis 8.2.1)"));
        let dragonfly = info(
            "# Server\nredis_version:7.4.0\ndragonfly_version:df-v1.25.0\nredis_mode:standalone\n",
        );
        assert_eq!(
            server_label(&dragonfly, "Dragonfly"),
            "Dragonfly v1.25.0, standalone"
        );
        // Always longer than a figure, so that it is listed under the chart.
        assert_eq!(
            server_label(&info("redis_version:8.2.1\n"), "Redis"),
            "Redis 8.2.1 server"
        );
        assert_eq!(server_label(&[], "Upstash"), "Upstash");
    }

    #[test]
    fn the_monitor_row_is_read_from_info_and_the_client_list() {
        let report = info(
            "# Clients\nconnected_clients:3\nblocked_clients:1\nmaxclients:10000\n# Memory\nused_memory_human:1.05M\nused_memory_peak_human:1.20M\nmaxmemory_human:0B\n# Stats\ntotal_commands_processed:5120\ninstantaneous_ops_per_sec:12\nkeyspace_hits:90\nkeyspace_misses:10\nexpired_keys:4\nevicted_keys:0\nrejected_connections:0\n# Server\nuptime_in_seconds:93784\n# Replication\nrole:master\n# Keyspace\ndb0:keys=120,expires=14,avg_ttl=0\ndb3:keys=2,expires=0,avg_ttl=0\n",
        );
        let clients = reply::clients(
            "id=7 addr=172.18.0.1:40112 name=baza-studio db=0 idle=0 cmd=client|list\nid=9 addr=172.18.0.1:40200 name= db=3 idle=41 cmd=blpop\n",
        );
        let result = stats(&report, Some(&clients), "Redis 8.2.1, standalone", 0);
        assert_eq!(result.rows.len(), 1);
        let value = |name: &str| {
            let index = result
                .columns
                .iter()
                .position(|column| column.name == name)
                .unwrap_or_else(|| panic!("no column {name}"));
            result.rows[0][index].display()
        };
        // The two large figures come first.
        assert_eq!(result.columns[0].name, "connections");
        assert_eq!(result.columns[1].name, "RAM");
        assert_eq!(value("connections"), "3");
        assert_eq!(value("RAM"), "1.05M");
        assert_eq!(value("version"), "Redis 8.2.1, standalone");
        assert_eq!(value("database"), "db0");
        assert_eq!(value("keys"), "122");
        assert_eq!(value("expiring keys"), "14");
        assert_eq!(value("ops/s"), "12");
        assert_eq!(value("memory limit"), "none");
        assert_eq!(value("uptime"), "1 d 2 h");
        assert_eq!(value("role"), "master");
        assert_eq!(value("hit rate"), "90.0%");
        assert_eq!(value("keyspace"), "db0: 120 keys, 14 expiring; db3: 2 keys");
        assert_eq!(
            value("clients"),
            "#7 baza-studio 172.18.0.1:40112 db0 client|list idle 0 s; #9 172.18.0.1:40200 db3 blpop idle 41 s"
        );
        // A server that keeps its clients to itself still has a row.
        let bare = stats(
            &info("# Clients\nconnected_clients:1\n"),
            None,
            "Upstash",
            0,
        );
        assert!(bare.columns.iter().all(|column| column.name != "clients"));
        assert_eq!(uptime(59), "59 s");
        assert_eq!(uptime(3700), "1 h 1 min");
        assert_eq!(uptime(125), "2 min 5 s");
    }
}
