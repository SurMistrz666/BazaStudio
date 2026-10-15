//! MongoDB and the servers and services that speak its protocol, through
//! the official driver.
//!
//! Databases are the schema level of the side panel and collections its
//! containers. A document is a row: its top-level fields are the columns,
//! found by looking at documents (`value.rs` says how a BSON value becomes
//! a cell and back without losing its type). Records are addressed by
//! `_id`. The query language is the command console (`shell.rs` reads the
//! notation of the MongoDB shell, `console.rs` runs it); nothing is
//! JavaScript.
//!
//! The products are presets over this one driver. What a server is, where
//! the driver has to know, is taken from its answers and from its address
//! ([`Flavor`]), not only from the product the user picked.
use super::{PageRequest, RowChange, Session};
use crate::builder::FilterOp;
use crate::engine::{
    Badge, BrowseModel, Capabilities, ConnectRequest, ConnectionForm, Dialect, Engine, EngineSpec,
    FieldUse, Group, OptionField, OptionKind, ProductSpec, QueryLanguage,
};
use crate::model::{Cell, ColumnInfo, QueryResult, ResultColumn, TableRef, TlsMode};
use crate::sql::{MAX_RESULT_BYTES, MAX_RESULT_ROWS};
use ::mongodb::bson::{Bson, Document, doc};
use ::mongodb::error::{Error as DriverError, ErrorKind, WriteFailure};
use ::mongodb::event::EventHandler;
use ::mongodb::event::sdam::SdamEvent;
use ::mongodb::options::{ClientOptions, Credential, ServerAddress, Tls, TlsOptions};
use ::mongodb::{Client, ClientSession, Database};
use anyhow::{Result, anyhow, bail, ensure};
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

mod console;
mod relay;
mod shell;
mod value;

use value::{Fields, Shape, Source};

pub const ENGINE: Engine = Engine::new(&SPEC);

pub static SPEC: EngineSpec = EngineSpec {
    id: "mongodb",
    aliases: &["mongo"],
    portable_name: "mongodb",
    label: "MongoDB",
    available: true,
    form: ConnectionForm {
        database: FieldUse::Optional,
        database_label: ("Domyślna baza", "Default database"),
        ..ConnectionForm::SERVER
    },
    capabilities: Capabilities {
        language: QueryLanguage::Command,
        browse: BrowseModel::Documents,
        // Databases are the level above collections.
        schemas: true,
        filter: true,
        condition: false,
        sort: true,
        sort_many: false,
        edit_cells: true,
        insert_rows: true,
        delete_rows: true,
        // A collection has no declared columns, and a query is not SQL.
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
        key: "srv",
        label: ("Lista DNS (mongodb+srv)", "DNS seed list (mongodb+srv)"),
        kind: OptionKind::Flag,
        default: "false",
        placeholder: "",
        help: Some((
            "Serwery są odczytywane z DNS, jak w adresie mongodb+srv:// (Atlas). Port nie jest wtedy używany. Nie działa przez tunel SSH.",
            "The servers are read from DNS, as in a mongodb+srv:// address (Atlas). The port is not used then. It does not work through an SSH tunnel.",
        )),
        required: false,
    },
    OptionField {
        key: "auth_source",
        label: ("Baza uwierzytelniania", "Authentication database"),
        kind: OptionKind::Text,
        default: "admin",
        placeholder: "admin",
        help: Some((
            "Baza, w której założono konto (authSource).",
            "The database the account was created in (authSource).",
        )),
        required: false,
    },
    OptionField {
        key: "replica_set",
        label: ("Zestaw replik", "Replica set"),
        kind: OptionKind::Text,
        default: "",
        placeholder: "rs0",
        help: None,
        required: false,
    },
    OptionField {
        key: "direct_connection",
        label: ("Połączenie bezpośrednie", "Direct connection"),
        kind: OptionKind::Flag,
        default: "false",
        placeholder: "",
        help: Some((
            "Tylko z podanym serwerem, bez szukania reszty zestawu replik. Przez tunel SSH jest tak zawsze: tunel przekazuje jeden port jednego serwera.",
            "With the given server only, without looking for the rest of the replica set. It is always so through an SSH tunnel: a tunnel forwards one port of one server.",
        )),
        required: false,
    },
    OptionField {
        key: "extra_hosts",
        label: ("Dodatkowe hosty", "Additional hosts"),
        kind: OptionKind::Text,
        default: "",
        placeholder: "host2:27017, host3:27017",
        help: Some((
            "Pozostałe serwery zestawu replik jako host:port po przecinku. Zwykle wystarcza jeden: resztę podaje zestaw.",
            "The other servers of the replica set as host:port, separated by commas. One is usually enough: the set names the rest.",
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
        default_port: 27017,
        ..ProductSpec::GENERIC
    }
}

static PRODUCTS: &[ProductSpec] = &[
    ProductSpec {
        keywords: &["mongo", "document", "nosql", "json", "bson"],
        ..product("mongodb", "MongoDB", Group::KeyValue, "Mo", [0, 138, 78])
    },
    ProductSpec {
        default_tls: TlsMode::VerifyFull,
        keywords: &["mongo", "atlas", "cloud", "srv", "document", "nosql"],
        note: Some((
            "Atlas podaje adres jako mongodb+srv://…: wpisz jego nazwę hosta i zaznacz „Lista DNS (mongodb+srv)”. Szyfrowanie jest wymagane, a adres IP tego komputera musi być na liście dostępu klastra.",
            "Atlas gives its address as mongodb+srv://…: enter its host name and turn on “DNS seed list (mongodb+srv)”. Encryption is required, and the IP address of this computer has to be on the access list of the cluster.",
        )),
        ..product(
            "mongodb-atlas",
            "MongoDB Atlas",
            Group::Cloud,
            "At",
            [0, 104, 74],
        )
    },
    ProductSpec {
        default_tls: TlsMode::Require,
        keywords: &["aws", "amazon", "docdb", "documentdb", "mongo"],
        note: Some((
            "Klaster jest osiągalny tylko z jego sieci VPC, zwykle przez tunel SSH (karta SSH). Szyfrowanie jest wymagane; pełna weryfikacja wymaga pliku CA Amazona (global-bundle.pem) wskazanego na karcie SSL/TLS. Ponawianie zapisów (retryWrites) jest tu wyłączone, a filtr „zawiera” przeszukuje tylko pola tekstowe.",
            "The cluster can be reached from its VPC only, usually through an SSH tunnel (the SSH tab). Encryption is required; full verification needs Amazon's CA file (global-bundle.pem) chosen on the SSL/TLS tab. Retryable writes (retryWrites) are off here, and the “contains” filter searches text fields only.",
        )),
        ..product(
            "amazon-documentdb",
            "Amazon DocumentDB",
            Group::Cloud,
            "Dd",
            [82, 127, 255],
        )
    },
    ProductSpec {
        default_tls: TlsMode::VerifyFull,
        default_port: 10255,
        keywords: &["azure", "microsoft", "cosmos", "cosmosdb", "mongo"],
        note: Some((
            "Konto z jednostkami RU: port 10255, szyfrowanie wymagane, nazwa użytkownika to nazwa konta, a hasło to jego klucz. Klaster vCore podaje adres mongodb+srv://: zaznacz „Lista DNS (mongodb+srv)”. Ponawianie zapisów (retryWrites) jest tu wyłączone, a filtr „zawiera” przeszukuje tylko pola tekstowe.",
            "An account with request units: port 10255, encryption required, the user name is the account name and the password its key. A vCore cluster gives a mongodb+srv:// address: turn on “DNS seed list (mongodb+srv)”. Retryable writes (retryWrites) are off here, and the “contains” filter searches text fields only.",
        )),
        ..product(
            "azure-cosmosdb-mongodb",
            "Azure Cosmos DB for MongoDB",
            Group::Cloud,
            "Co",
            [0, 120, 212],
        )
    },
    ProductSpec {
        keywords: &["ferret", "ferretdb", "postgres", "mongo", "document"],
        note: Some((
            "FerretDB mówi protokołem MongoDB nad PostgreSQL. Część poleceń i operatorów nie jest w nim dostępna, a filtr „zawiera” przeszukuje tylko pola tekstowe.",
            "FerretDB speaks the MongoDB protocol on top of PostgreSQL. Some commands and operators are not available in it, and the “contains” filter searches text fields only.",
        )),
        ..product(
            "ferretdb",
            "FerretDB",
            Group::KeyValue,
            "Fe",
            [246, 142, 31],
        )
    },
];

/// What the server turned out to be, where the driver has to know.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flavor {
    /// MongoDB itself, Atlas included, with its version.
    Mongo {
        major: u32,
        minor: u32,
    },
    Ferret,
    DocumentDb,
    Cosmos,
}
impl Flavor {
    /// Tell the server apart by what `buildInfo` says. The two hosted
    /// emulations report a plain MongoDB version, so their address and the
    /// product of the profile are the hint.
    fn detect(build: &Document, product: &str, host: &str) -> Self {
        let host = host.to_ascii_lowercase();
        if build
            .keys()
            .any(|key| key.to_ascii_lowercase().starts_with("ferretdb"))
        {
            return Flavor::Ferret;
        }
        if product == "amazon-documentdb" || host.ends_with(".docdb.amazonaws.com") {
            return Flavor::DocumentDb;
        }
        if product == "azure-cosmosdb-mongodb" || host.ends_with(".cosmos.azure.com") {
            return Flavor::Cosmos;
        }
        if product == "ferretdb" {
            return Flavor::Ferret;
        }
        let mut parts = build
            .get_str("version")
            .unwrap_or_default()
            .split('.')
            .map(|part| part.parse::<u32>().unwrap_or(0));
        Flavor::Mongo {
            major: parts.next().unwrap_or(0),
            minor: parts.next().unwrap_or(0),
        }
    }
    /// `$expr`, `$regexMatch`, `$convert` and `$getField`: a "contains"
    /// filter that also searches numbers, dates and ObjectIds as text.
    fn searches_any_type(self) -> bool {
        matches!(self, Flavor::Mongo { major, .. } if major >= 5)
    }
}

/// A service that refuses the transaction number of a retryable write.
fn refuses_retryable_writes(product: &str, host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    matches!(product, "amazon-documentdb" | "azure-cosmosdb-mongodb")
        || host.ends_with(".docdb.amazonaws.com")
        || host.ends_with(".cosmos.azure.com")
}

/// How a profile is connected, worked out before anything is dialled.
#[derive(Debug, PartialEq)]
struct Settings {
    /// Host and port of the servers the client starts from.
    seeds: Vec<(String, u16)>,
    /// The host is a DNS seed list (`mongodb+srv`).
    srv: bool,
    direct: bool,
    replica_set: Option<String>,
    auth_source: String,
    /// TLS as the client is told: off when the relay does it.
    tls: TlsMode,
    /// The connection goes through a tunnel with TLS: a relay in this
    /// process encrypts in front of the forwarded port.
    relay: bool,
    retry_writes: bool,
}

/// A host name as a person types it: no scheme, account, path or list.
fn plain_host(host: &str) -> bool {
    !host.is_empty() && !host.contains("//") && !host.contains(['/', '?', '@', ',', ' ', '#', '%'])
}

impl Settings {
    fn of(request: &ConnectRequest<'_>) -> Result<Self> {
        let profile = request.profile;
        let host = profile.host.trim();
        ensure!(plain_host(host), "mongodb_host_invalid");
        let srv = request.option("srv") == "true";
        let direct = request.option("direct_connection") == "true";
        let extra = request.option("extra_hosts").trim();
        let replica_set = Some(request.option("replica_set").trim())
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        let auth_source = match request.option("auth_source").trim() {
            "" => "admin",
            source => source,
        }
        .to_owned();
        let retry_writes = !refuses_retryable_writes(request.product.id, host);
        if let Some(forwarded) = request.forwarded {
            // A tunnel forwards one port of one server: a seed list has no
            // server to forward to, and the other members of a set would be
            // dialled past the tunnel.
            ensure!(!srv, "mongodb_srv_through_tunnel");
            return Ok(Self {
                seeds: vec![(forwarded.ip().to_string(), forwarded.port())],
                srv: false,
                direct: true,
                replica_set: None,
                auth_source,
                tls: profile.tls,
                relay: profile.tls != TlsMode::Disable,
                retry_writes,
            });
        }
        let mut seeds = vec![(host.to_owned(), profile.port)];
        if srv {
            ensure!(!direct && extra.is_empty(), "mongodb_options_conflict");
            // Looked up in DNS: a name, never an address.
            ensure!(
                host.contains('.')
                    && host.parse::<std::net::IpAddr>().is_err()
                    && host
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-'),
                "mongodb_host_invalid"
            );
        } else {
            for entry in extra
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
            {
                let (name, port) = match entry.rsplit_once(':') {
                    Some((name, port)) if !name.contains(':') => (
                        name,
                        port.parse::<u16>()
                            .ok()
                            .filter(|port| *port > 0)
                            .ok_or_else(|| anyhow!("mongodb_extra_hosts_invalid"))?,
                    ),
                    _ => (entry, 27017),
                };
                ensure!(plain_host(name), "mongodb_extra_hosts_invalid");
                seeds.push((name.to_owned(), port));
            }
            ensure!(!direct || seeds.len() == 1, "mongodb_options_conflict");
        }
        Ok(Self {
            seeds,
            srv,
            direct,
            replica_set,
            auth_source,
            tls: profile.tls,
            relay: false,
            retry_writes,
        })
    }
}

/// The parts of a `mongodb://` or `mongodb+srv://` address, as the fields
/// of a connection profile.
///
/// The host field does not take such an address: it holds an account, and
/// the host of a profile is also the target of its SSH tunnel and the name
/// in its certificate. A dialog that wants to accept a pasted address takes
/// it apart with [`parse_address`] and fills the fields; nothing in the
/// application does that yet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AddressParts {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub database: String,
    /// The TLS mode the address asks for; `None` when it does not say.
    pub tls: Option<TlsMode>,
    /// Engine options by their key: `srv`, `auth_source`, `replica_set`,
    /// `direct_connection`, `extra_hosts`.
    pub options: Vec<(&'static str, String)>,
}

/// `%41` as `A`; `None` for an escape that is not one or text that is not
/// UTF-8.
fn percent_decoded(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut rest = text.as_bytes();
    while let Some((first, tail)) = rest.split_first() {
        if *first == b'%' {
            let digits = std::str::from_utf8(tail.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(digits, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(*first);
            rest = tail;
        }
    }
    String::from_utf8(bytes).ok()
}

/// A host of an address with its port: `db.example.test:27018`, `[::1]`.
fn host_and_port(text: &str) -> Option<(String, u16)> {
    let (host, port) = match text.strip_prefix('[') {
        Some(rest) => {
            let (host, after) = rest.split_once(']')?;
            (host, after.strip_prefix(':'))
        }
        None => match text.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (text, None),
        },
    };
    let port = match port {
        Some(port) => port.parse::<u16>().ok().filter(|port| *port > 0)?,
        None => 27017,
    };
    let host = percent_decoded(host)?;
    plain_host(&host).then_some((host, port))
}

/// Take a `mongodb://` or `mongodb+srv://` address apart. What is not a
/// field of a profile (`retryWrites`, `w`, `appName`, …) is left out;
/// `None` for text that is not such an address.
pub fn parse_address(text: &str) -> Option<AddressParts> {
    let text = text.trim();
    let (scheme, rest) = text.split_once("://")?;
    let srv = match scheme.to_ascii_lowercase().as_str() {
        "mongodb" => false,
        "mongodb+srv" => true,
        _ => return None,
    };
    let (location, query) = match rest.split_once('?') {
        Some((location, query)) => (location, query),
        None => (rest, ""),
    };
    let (authority, database) = match location.split_once('/') {
        Some((authority, database)) => (authority, database),
        None => (location, ""),
    };
    let (account, hosts) = match authority.rsplit_once('@') {
        Some((account, hosts)) => (Some(account), hosts),
        None => (None, authority),
    };
    let mut parts = AddressParts {
        database: percent_decoded(database)?,
        ..AddressParts::default()
    };
    if let Some(account) = account {
        let (username, password) = account.split_once(':').unwrap_or((account, ""));
        parts.username = percent_decoded(username)?;
        parts.password = percent_decoded(password)?;
    }
    let mut hosts = hosts.split(',');
    if srv {
        // A name for DNS: one, and without a port.
        let name = hosts.next().filter(|name| !name.contains(':'))?;
        (parts.host, parts.port) = host_and_port(name)?;
        if hosts.next().is_some() {
            return None;
        }
        parts.options.push(("srv", "true".into()));
        // Such an address is encrypted unless it says otherwise.
        parts.tls = Some(TlsMode::VerifyFull);
    } else {
        (parts.host, parts.port) = host_and_port(hosts.next()?)?;
        let extra = hosts
            .map(|host| host_and_port(host).map(|(host, port)| format!("{host}:{port}")))
            .collect::<Option<Vec<_>>>()?;
        if !extra.is_empty() {
            parts.options.push(("extra_hosts", extra.join(", ")));
        }
    }
    let mut unverified = false;
    for pair in query.split(['&', ';']).filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=')?;
        let value = percent_decoded(value)?;
        let on = value.eq_ignore_ascii_case("true");
        match key.to_ascii_lowercase().as_str() {
            "authsource" => parts.options.push(("auth_source", value)),
            "replicaset" => parts.options.push(("replica_set", value)),
            "directconnection" if on => parts.options.push(("direct_connection", "true".into())),
            "tls" | "ssl" => {
                parts.tls = Some(if on {
                    TlsMode::VerifyFull
                } else {
                    TlsMode::Disable
                });
            }
            "tlsinsecure" | "tlsallowinvalidcertificates" | "tlsallowinvalidhostnames" => {
                unverified |= on;
            }
            _ => {}
        }
    }
    // Encrypted without the checks: the mode that says so.
    if unverified && parts.tls != Some(TlsMode::Disable) {
        parts.tls = Some(TlsMode::Require);
    }
    Some(parts)
}

/// Time the client waits for a server it can use.
const SELECTION: Duration = Duration::from_secs(10);
/// Time a statement, a page or a change may take.
const STATEMENT: Duration = Duration::from_secs(120);
/// Time a piece of metadata may take: the list of collections, a sample.
const METADATA: Duration = Duration::from_secs(30);

async fn client_options(
    request: &ConnectRequest<'_>,
    settings: &Settings,
    relay: Option<SocketAddr>,
) -> Result<ClientOptions> {
    let profile = request.profile;
    let mut options = if settings.srv {
        // The driver reads the SRV and TXT records. Nothing secret is in
        // this address: the account is set below.
        ClientOptions::parse(format!("mongodb+srv://{}/", settings.seeds[0].0))
            .await
            .map_err(|error| match error.kind.as_ref() {
                ErrorKind::DnsResolve { .. } => anyhow!("mongodb_srv_lookup_failed"),
                _ => anyhow!("mongodb_host_invalid"),
            })?
    } else {
        let hosts = match relay {
            Some(address) => vec![ServerAddress::from(address)],
            None => settings
                .seeds
                .iter()
                .map(|(host, port)| ServerAddress::Tcp {
                    host: host.clone(),
                    port: Some(*port),
                })
                .collect(),
        };
        ClientOptions::builder().hosts(hosts).build()
    };
    options.app_name = Some("Baza Studio".into());
    options.connect_timeout = Some(Duration::from_secs(10));
    options.server_selection_timeout = Some(SELECTION);
    options.max_pool_size = Some(8);
    if settings.direct {
        options.direct_connection = Some(true);
    }
    if settings.replica_set.is_some() {
        options.repl_set_name.clone_from(&settings.replica_set);
    }
    if !settings.retry_writes {
        options.retry_writes = Some(false);
    }
    if !profile.database.is_empty() {
        options.default_database = Some(profile.database.clone());
    }
    options.credential = (!profile.username.is_empty()).then(|| {
        Credential::builder()
            .username(profile.username.clone())
            .password(profile.password.clone())
            .source(settings.auth_source.clone())
            .build()
    });
    let mode = if settings.relay {
        TlsMode::Disable
    } else {
        settings.tls
    };
    options.tls = Some(match mode {
        TlsMode::Disable => Tls::Disabled,
        TlsMode::Require => {
            let mut tls = TlsOptions::default();
            tls.allow_invalid_certificates = Some(true);
            Tls::Enabled(tls)
        }
        TlsMode::VerifyFull => {
            let mut tls = TlsOptions::default();
            tls.ca_file_path.clone_from(&profile.ca_file);
            Tls::Enabled(tls)
        }
    });
    Ok(options)
}

/// The number the server gave an error, when the error is the server's.
fn server_code(error: &DriverError) -> Option<i64> {
    match error.kind.as_ref() {
        ErrorKind::Command(command) => Some(command.code.into()),
        ErrorKind::Write(WriteFailure::WriteError(write)) => Some(write.code.into()),
        ErrorKind::Write(WriteFailure::WriteConcernError(concern)) => Some(concern.code.into()),
        _ => None,
    }
}

/// The server said no to the account, not to the command.
fn unauthorized(error: &DriverError) -> bool {
    matches!(server_code(error), Some(13 | 8000 | 303))
}

/// The result code for an error number of the server. The server's text
/// never leaves this driver: it can repeat a name or an address.
fn server_failure(code: i64) -> anyhow::Error {
    match code {
        18 => anyhow!("database_login_rejected"),
        13 => anyhow!("mongodb_not_authorized"),
        26 => anyhow!("mongodb_namespace_missing"),
        48 => anyhow!("mongodb_namespace_exists"),
        50 => anyhow!("query_timeout"),
        59 => anyhow!("mongodb_command_unknown"),
        121 => anyhow!("mongodb_validation_failed"),
        10107 | 13435 => anyhow!("mongodb_not_primary"),
        11000 | 11001 | 12582 => anyhow!("mongodb_duplicate_key"),
        11601 => anyhow!("query_cancelled"),
        other => anyhow!("mongodb_server_error_{}", other.max(0)),
    }
}

/// A write that the server refused part of the way through a command.
fn write_failure(code: i64, written: usize) -> anyhow::Error {
    anyhow!("mongodb_write_failed_{}_after_{written}", code.max(0))
}

/// The result code for a failure of a command on an open connection.
fn failure(error: DriverError) -> anyhow::Error {
    if let Some(code) = server_code(&error) {
        return server_failure(code);
    }
    match error.kind.as_ref() {
        ErrorKind::Authentication { .. } => anyhow!("database_login_rejected"),
        ErrorKind::Io(io) => anyhow!(relay::tls_failure(io).unwrap_or("database_network_error")),
        ErrorKind::ServerSelection { .. } => anyhow!("mongodb_no_server_selected"),
        ErrorKind::ConnectionPoolCleared { .. } | ErrorKind::DnsResolve { .. } => {
            anyhow!("database_network_error")
        }
        ErrorKind::InvalidTlsConfig { .. } => anyhow!("mongodb_ca_file_unreadable"),
        ErrorKind::InvalidArgument { .. } => anyhow!("mongodb_arguments_invalid"),
        _ => anyhow!("database_operation_failed"),
    }
}

/// Why a server could not be reached, from the failure of a connection to
/// it. `encrypted` says whether this side asked for TLS.
fn unreachable(error: &DriverError, encrypted: bool) -> &'static str {
    match error.kind.as_ref() {
        ErrorKind::Authentication { .. } => "database_login_rejected",
        ErrorKind::InvalidTlsConfig { .. } => "mongodb_ca_file_unreadable",
        ErrorKind::Io(io) => match relay::tls_failure(io) {
            Some(code) => code,
            // A server without TLS hangs up on a TLS greeting, and a server
            // that requires TLS hangs up on a plain one.
            None if relay::closed_early(io) && encrypted => "database_tls_unsupported",
            None if relay::closed_early(io) => "mongodb_connection_closed",
            None if io.kind() == std::io::ErrorKind::TimedOut => "database_connection_timeout",
            None => "database_network_error",
        },
        _ => "database_network_error",
    }
}

/// The session id the server knows a running statement by, shared with the
/// code that stops it.
type SessionId = Mutex<Option<Document>>;

/// The connection a piece of work runs on: with a session of its own when
/// the server has sessions, so that the work can be stopped on the server.
struct Scope {
    session: Option<ClientSession>,
}

/// Documents read from a cursor, up to the limits of a result.
#[derive(Default)]
struct Batch {
    documents: Vec<Document>,
    truncated: bool,
}

impl Scope {
    /// Without a session: for metadata, which is short and is not stopped.
    fn plain() -> Self {
        Self { session: None }
    }
    async fn open(client: &Client, id: &SessionId) -> Self {
        // A server without sessions is used without one.
        let session = client.start_session().await.ok();
        if let Ok(mut slot) = id.lock() {
            *slot = session.as_ref().map(|session| session.id().clone());
        }
        Self { session }
    }
    async fn command(
        &mut self,
        database: &Database,
        command: Document,
    ) -> ::mongodb::error::Result<Document> {
        match &mut self.session {
            Some(session) => database.run_command(command).session(session).await,
            None => database.run_command(command).await,
        }
    }
    /// Run a command that answers with a cursor and read it to its end or
    /// to the limits of a result. A cursor that is left early is closed on
    /// the server by the driver.
    async fn documents(
        &mut self,
        database: &Database,
        command: Document,
    ) -> ::mongodb::error::Result<Batch> {
        let mut batch = Batch::default();
        let mut bytes = 0usize;
        macro_rules! take {
            ($cursor:expr) => {{
                let size = $cursor.current().as_bytes().len();
                if batch.documents.len() >= MAX_RESULT_ROWS
                    || bytes.saturating_add(size) > MAX_RESULT_BYTES
                {
                    batch.truncated = true;
                    break;
                }
                bytes += size;
                batch.documents.push($cursor.deserialize_current()?);
            }};
        }
        match &mut self.session {
            Some(session) => {
                let mut cursor = database
                    .run_cursor_command(command)
                    .session(&mut *session)
                    .await?;
                while cursor.advance(session).await? {
                    take!(cursor);
                }
            }
            None => {
                let mut cursor = database.run_cursor_command(command).await?;
                while cursor.advance().await? {
                    take!(cursor);
                }
            }
        }
        Ok(batch)
    }
}

/// What is known about the fields of a collection.
struct Known {
    /// The fields of the last sample, and those that pages showed since.
    fields: Fields,
    /// Fields no sample has shown, only a page: kept as columns, so that
    /// the grid and the columns of the collection keep matching.
    extras: Vec<String>,
}

type Namespace = (String, String);

struct MongoSession {
    client: Client,
    flavor: Flavor,
    /// Version as the server reports it.
    version: String,
    /// `standalone`, `replica set rs0 (primary)`, `sharded cluster (mongos)`.
    topology: String,
    /// The database of the profile, or `test` when it names none.
    home: String,
    /// The database `db` stands for in the console; `use` changes it.
    current: Mutex<String>,
    known: Mutex<HashMap<Namespace, Known>>,
    /// Operation counters of the previous reading of the monitor.
    counters: Mutex<Option<(Instant, [i64; 6])>>,
    /// Encrypts in front of an SSH tunnel; lives as long as the session.
    _relay: Option<relay::Relay>,
}

fn connect<'a>(request: &'a ConnectRequest<'a>) -> BoxFuture<'a, Result<Arc<dyn Session>>> {
    Box::pin(async move {
        let profile = request.profile;
        let settings = Settings::of(request)?;
        let relay = match (request.forwarded, settings.relay) {
            (Some(target), true) => Some(
                relay::Relay::start(
                    target,
                    request.server_name(),
                    profile.tls,
                    profile.ca_file.as_deref(),
                )
                .await?,
            ),
            _ => None,
        };
        let mut options = client_options(
            request,
            &settings,
            relay.as_ref().map(relay::Relay::address),
        )
        .await?;
        // The first server that cannot be reached says why. Without this
        // the client would only report, after its whole waiting time, that
        // it found no server.
        let (sender, mut failures) = tokio::sync::mpsc::unbounded_channel();
        options.sdam_event_handler = Some(EventHandler::callback(move |event: SdamEvent| {
            if let SdamEvent::ServerHeartbeatFailed(failed) = event {
                let _ = sender.send((failed.server_address.to_string(), failed.failure));
            }
        }));
        let encrypted = !settings.relay && settings.tls != TlsMode::Disable;
        let seed = options.hosts.first().map(ToString::to_string);
        let single = options.hosts.len() == 1 && !settings.srv;
        let client = Client::with_options(options)
            .map_err(|error| anyhow!(unreachable(&error, encrypted)))?;
        let opened = async {
            let admin = client.database("admin");
            // A member the set names that cannot be reached, while the
            // server that was given can.
            let mut lost_member = false;
            let greeting = hello(&admin);
            tokio::pin!(greeting);
            let hello = loop {
                tokio::select! {
                    reply = &mut greeting => break reply,
                    Some((address, error)) = failures.recv(), if single => {
                        // The one server that was given does not answer:
                        // nothing else can be found through it.
                        if Some(&address) == seed.as_ref() {
                            bail!(unreachable(&error, encrypted));
                        }
                        // Another member of its set: the rest may do.
                        lost_member = true;
                    }
                }
            };
            let hello = match hello {
                Ok(reply) => reply,
                Err(error) => {
                    // No server in time: the last failed connection says why.
                    let mut last = None;
                    while let Ok((_, failure)) = failures.try_recv() {
                        last = Some(failure);
                    }
                    bail!(match (error.kind.as_ref(), last) {
                        (ErrorKind::ServerSelection { .. }, _) if lost_member => {
                            "mongodb_member_unreachable"
                        }
                        (ErrorKind::ServerSelection { .. }, Some(failure)) => {
                            unreachable(&failure, encrypted)
                        }
                        (ErrorKind::ServerSelection { .. }, None) => "mongodb_no_server_selected",
                        _ => unreachable(&error, encrypted),
                    });
                }
            };
            // A server that will not say what it is is treated as MongoDB
            // of an unknown version.
            let build = admin
                .run_command(doc! {"buildInfo": 1})
                .await
                .unwrap_or_default();
            let current = match profile.database.as_str() {
                "" => "test",
                name => name,
            };
            // The account is checked by a command that needs one: a server
            // answers `hello` and `buildInfo` to anybody.
            let listed = admin
                .run_command(doc! {"listDatabases": 1, "nameOnly": true, "authorizedDatabases": true})
                .await;
            if let Err(error) = listed {
                ensure!(unauthorized(&error), failure(error));
                let own = client
                    .database(current)
                    .run_command(doc! {"listCollections": 1, "nameOnly": true, "authorizedCollections": true})
                    .await;
                if let Err(error) = own {
                    ensure!(unauthorized(&error), failure(error));
                    bail!(if profile.username.is_empty() {
                        "database_login_rejected"
                    } else {
                        "database_access_denied"
                    });
                }
            }
            Ok((hello, build, current.to_owned()))
        }
        .await;
        let (hello, build, current) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                client.shutdown().immediate(true).await;
                return Err(error);
            }
        };
        let topology = if hello.get_str("msg").is_ok_and(|kind| kind == "isdbgrid") {
            "sharded cluster (mongos)".to_owned()
        } else if let Ok(name) = hello.get_str("setName") {
            let primary = hello.get_bool("isWritablePrimary").unwrap_or(false)
                || hello.get_bool("ismaster").unwrap_or(false);
            let role = if primary {
                "primary"
            } else if hello.get_bool("secondary").unwrap_or(false) {
                "secondary"
            } else {
                "member"
            };
            format!("replica set {name} ({role})")
        } else {
            "standalone".to_owned()
        };
        Ok(Arc::new(MongoSession {
            flavor: Flavor::detect(&build, request.product.id, &profile.host),
            version: build.get_str("version").unwrap_or_default().to_owned(),
            topology,
            home: current.clone(),
            current: Mutex::new(current),
            known: Mutex::default(),
            counters: Mutex::default(),
            client,
            _relay: relay,
        }) as Arc<dyn Session>)
    })
}

/// What the server says about itself. Servers older than 4.4.2 know the
/// command under its earlier name only.
async fn hello(admin: &Database) -> ::mongodb::error::Result<Document> {
    match admin.run_command(doc! {"hello": 1}).await {
        Err(error) if server_code(&error) == Some(59) => {
            admin.run_command(doc! {"isMaster": 1}).await
        }
        reply => reply,
    }
}

/// A document with one field.
fn one(key: &str, value: impl Into<Bson>) -> Document {
    let mut document = Document::new();
    document.insert(key, value);
    document
}

/// Characters with a meaning in a regular expression, made plain.
fn regex_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A field name that can be used as a path in a filter, an order and an
/// update: one top-level field, not a path into a document or an operator.
fn settable(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('$') && !name.contains(['.', '\0'])
}

/// The "contains" filter of "Dane tabeli": letter case is ignored, and
/// what is typed is plain text, never an expression.
fn contains(field: &str, text: &str, any_type: bool) -> Result<Document> {
    let pattern = regex_escape(text);
    let in_text = doc! {"$regex": &pattern, "$options": "i"};
    if !any_type {
        ensure!(settable(field), "mongodb_field_name_unsupported");
        return Ok(one(field, in_text));
    }
    // Numbers, dates, ObjectIds and booleans are searched as the text the
    // server writes them as. What has no text (a document, an array) is
    // empty here; an array of texts is found by the plain match.
    let input = if value::plain_name(field) {
        Bson::String(format!("${field}"))
    } else {
        Bson::Document(doc! {"$getField": {"field": {"$literal": field}, "input": "$$ROOT"}})
    };
    let as_text = doc! {"$expr": {"$regexMatch": {
        "input": {"$convert": {"input": input, "to": "string", "onError": "", "onNull": ""}},
        "regex": &pattern,
        "options": "i",
    }}};
    Ok(if settable(field) {
        doc! {"$or": [one(field, in_text), as_text]}
    } else {
        as_text
    })
}

/// A page of a collection as the command that reads it.
#[derive(Debug, PartialEq)]
struct PageQuery {
    filter: Document,
    sort: Option<Document>,
    skip: u64,
    limit: u32,
}

impl PageQuery {
    fn of(page: &PageRequest, any_type: bool) -> Result<Self> {
        let mut conditions = Vec::new();
        for filter in &page.filters {
            let field = filter.column.as_str();
            if filter.op == FilterOp::Contains {
                conditions.push(contains(field, &filter.value.display(), any_type)?);
                continue;
            }
            ensure!(settable(field), "mongodb_field_name_unsupported");
            let operand = value::exact(&filter.value)?;
            let condition = match filter.op {
                FilterOp::Equal => operand,
                FilterOp::NotEqual => Bson::Document(doc! {"$ne": operand}),
                FilterOp::Greater => Bson::Document(doc! {"$gt": operand}),
                FilterOp::Less => Bson::Document(doc! {"$lt": operand}),
                // A field without a value is null or is not there.
                FilterOp::IsNull => Bson::Null,
                FilterOp::IsNotNull => Bson::Document(doc! {"$ne": Bson::Null}),
                FilterOp::Contains => unreachable!("handled above"),
            };
            conditions.push(one(field, condition));
        }
        let filter = match conditions.len() {
            0 => Document::new(),
            1 => conditions.remove(0),
            _ => doc! {"$and": conditions},
        };
        let sort = match &page.order_by {
            Some((field, descending)) => {
                ensure!(settable(field), "mongodb_field_name_unsupported");
                Some(one(field, if *descending { -1 } else { 1 }))
            }
            None => None,
        };
        Ok(Self {
            filter,
            sort,
            skip: page.offset,
            limit: page.limit.clamp(1, 20_000),
        })
    }
    fn command(&self, collection: &str) -> Document {
        let mut command = doc! {"find": collection, "filter": self.filter.clone()};
        if let Some(sort) = &self.sort {
            command.insert("sort", sort.clone());
        }
        if self.skip > 0 {
            command.insert("skip", i64::try_from(self.skip).unwrap_or(i64::MAX));
        }
        command.insert("limit", i64::from(self.limit));
        command.insert("maxTimeMS", console::MAX_TIME_MS);
        command
    }
    /// The same page as a command of the console.
    fn text(&self, container: &TableRef) -> String {
        let mut text = format!(
            "{}.find({})",
            target(container),
            value::shell(&Bson::Document(self.filter.clone()))
        );
        if let Some(sort) = &self.sort {
            text.push_str(&format!(
                ".sort({})",
                value::shell(&Bson::Document(sort.clone()))
            ));
        }
        if self.skip > 0 {
            text.push_str(&format!(".skip({})", self.skip));
        }
        text.push_str(&format!(".limit({})", self.limit));
        text
    }
}

/// A collection as the console names it, always with its database:
/// `db.getSiblingDB("shop").orders`.
fn target(container: &TableRef) -> String {
    let database = value::shell(&Bson::String(container.schema.clone()));
    if value::plain_name(&container.name) && !container.name.starts_with('$') {
        format!("db.getSiblingDB({database}).{}", container.name)
    } else {
        format!(
            "db.getSiblingDB({database}).getCollection({})",
            value::shell(&Bson::String(container.name.clone()))
        )
    }
}

/// The `_id` a change addresses its document by: every value the cell of
/// the grid may stand for (`value::key_values`).
fn key_of(key: &[(String, Cell)]) -> Result<Vec<Bson>> {
    match key {
        [(name, cell)] if name == "_id" => value::key_values(cell),
        _ => Err(anyhow!("primary_key_required")),
    }
}

/// The condition on `_id` that finds a key: the value, or one of several.
fn id_condition(mut values: Vec<Bson>) -> Bson {
    if values.len() == 1 {
        values.remove(0)
    } else {
        Bson::Document(doc! {"$in": values})
    }
}

/// The key as the text of a change shows it: where the collection is known
/// to have keys of one type, the value of that type.
fn shown_key(values: Vec<Bson>, shape: Shape) -> Bson {
    let typed = match shape {
        Shape::Uniform(kind) => values.iter().find(|value| value::type_name(value) == kind),
        Shape::Open => None,
    };
    match typed {
        Some(value) => value.clone(),
        None => id_condition(values),
    }
}

/// The databases the server keeps for itself.
fn system_database(name: &str) -> bool {
    matches!(name, "admin" | "config" | "local")
}

/// A number of seconds for people, in units both languages write alike.
fn duration_text(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (days, hours, minutes) = (seconds / 86_400, seconds / 3600 % 24, seconds / 60 % 60);
    if days > 0 {
        format!("{days} d {hours} h")
    } else if hours > 0 {
        format!("{hours} h {minutes} min")
    } else if minutes > 0 {
        format!("{minutes} min")
    } else {
        format!("{seconds} s")
    }
}

const COUNTERS: [&str; 6] = ["insert", "query", "update", "delete", "getmore", "command"];

impl MongoSession {
    fn database_in_use(&self) -> String {
        self.current
            .lock()
            .map(|name| name.clone())
            .unwrap_or_default()
    }

    /// Stop on the server whatever the session with this id is running.
    async fn stop(&self, id: Option<Document>) {
        let Some(id) = id else {
            return;
        };
        let admin = self.client.database("admin");
        let kill = async { admin.run_command(doc! {"killSessions": [id]}).await };
        let _ = tokio::time::timeout(Duration::from_secs(3), kill).await;
    }

    /// Run `work` until it ends, is cancelled or takes longer than `limit`.
    /// Work that is cut short is stopped on the server as well: `id` is the
    /// session it runs in, which `work` fills in when it opens its scope.
    async fn race<T>(
        &self,
        cancel: &CancellationToken,
        limit: Duration,
        id: &SessionId,
        work: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let stopped = tokio::select! {
            biased;
            _ = cancel.cancelled() => "query_cancelled",
            _ = tokio::time::sleep(limit) => "query_timeout",
            result = work => return result,
        };
        let running = id.lock().ok().and_then(|mut slot| slot.take());
        self.stop(running).await;
        Err(anyhow!(stopped))
    }

    /// Collections and views of a database, with their kind, by name.
    async fn collections(
        &self,
        scope: &mut Scope,
        database: &str,
    ) -> Result<Vec<(String, String)>> {
        let batch = scope
            .documents(
                &self.client.database(database),
                doc! {"listCollections": 1, "nameOnly": true, "authorizedCollections": true},
            )
            .await
            .map_err(failure)?;
        let mut found = batch
            .documents
            .iter()
            .filter_map(|entry| {
                let name = entry.get_str("name").ok()?;
                // The storage of a time series collection, not a collection
                // of the user's.
                if name.starts_with("system.buckets.") {
                    return None;
                }
                Some((
                    name.to_owned(),
                    entry.get_str("type").unwrap_or("collection").to_owned(),
                ))
            })
            .collect::<Vec<_>>();
        found.sort();
        Ok(found)
    }

    /// Operations in progress, without the one that asks.
    async fn operations(&self, scope: &mut Scope) -> Result<Vec<Document>> {
        let admin = self.client.database("admin");
        let reply = match scope.command(&admin, doc! {"currentOp": 1}).await {
            // An account that may not see everybody's sees its own.
            Err(error) if unauthorized(&error) => {
                scope
                    .command(&admin, doc! {"currentOp": 1, "$ownOps": true})
                    .await
            }
            reply => reply,
        }
        .map_err(failure)?;
        Ok(reply
            .get_array("inprog")
            .map(|operations| {
                operations
                    .iter()
                    .filter_map(Bson::as_document)
                    .filter(|operation| {
                        !operation
                            .get_document("command")
                            .is_ok_and(|command| command.contains_key("currentOp"))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Forget what is known about the fields of a collection.
    fn forget(&self, database: &str, collection: &str) {
        if let Ok(mut known) = self.known.lock() {
            known.remove(&(database.to_owned(), collection.to_owned()));
        }
    }

    /// Look at documents of a collection to learn its fields: the first
    /// ones and, where there are more, the newest ones, so that a field
    /// added lately is seen as well.
    async fn sample(&self, container: &TableRef) -> Result<Fields> {
        const FIRST: i64 = 300;
        const NEWEST: i64 = 200;
        let database = self.client.database(&container.schema);
        let mut scope = Scope::plain();
        let limit = i64::try_from(METADATA.as_millis()).unwrap_or(30_000);
        let first = scope
            .documents(
                &database,
                doc! {"find": container.name.as_str(), "limit": FIRST, "maxTimeMS": limit},
            )
            .await
            .map_err(failure)?;
        let mut fields = Fields::default();
        for document in &first.documents {
            fields.observe(document);
        }
        // The `_id` index of a collection makes this cheap; a view has none.
        if first.documents.len() as i64 == FIRST && container.kind.contains("table") {
            let newest = scope
                .documents(
                    &database,
                    doc! {"find": container.name.as_str(), "sort": {"_id": -1}, "limit": NEWEST, "maxTimeMS": limit},
                )
                .await
                .map_err(failure)?;
            for document in &newest.documents {
                let seen = first
                    .documents
                    .iter()
                    .any(|earlier| earlier.get("_id") == document.get("_id"));
                if !seen {
                    fields.observe(document);
                }
            }
        }
        Ok(fields)
    }

    /// Sample a collection and remember its fields, with those that pages
    /// showed before and no sample has.
    async fn learn(&self, container: &TableRef) -> Result<Fields> {
        let mut fields = self.sample(container).await?;
        let namespace = (container.schema.clone(), container.name.clone());
        let mut known = self
            .known
            .lock()
            .map_err(|_| anyhow!("database_operation_failed"))?;
        let mut extras = Vec::new();
        if let Some(before) = known.get(&namespace) {
            for name in &before.extras {
                if !fields.knows(name) {
                    fields.adopt(&before.fields, name);
                    extras.push(name.clone());
                }
            }
        }
        known.insert(
            namespace,
            Known {
                fields: fields.clone(),
                extras,
            },
        );
        Ok(fields)
    }

    /// The fields of a collection as they are known, looked up when they
    /// are not.
    async fn fields(&self, container: &TableRef) -> Result<Fields> {
        let namespace = (container.schema.clone(), container.name.clone());
        let known = self
            .known
            .lock()
            .ok()
            .and_then(|known| known.get(&namespace).map(|known| known.fields.clone()));
        match known {
            Some(fields) => Ok(fields),
            None => self.learn(container).await,
        }
    }

    /// The type a field has in the collection, as far as it is known now.
    fn shape(&self, container: &TableRef, field: &str) -> Shape {
        let namespace = (container.schema.clone(), container.name.clone());
        self.known
            .lock()
            .ok()
            .and_then(|known| known.get(&namespace).map(|known| known.fields.shape(field)))
            .unwrap_or(Shape::Open)
    }

    /// The document a new row stands for. A field that was left empty is
    /// left out of the document, except where the field holds texts: an
    /// empty text is a value, an empty number or date is not.
    fn new_document(&self, table: &TableRef, values: &[(String, Cell)]) -> Result<Document> {
        let mut document = Document::new();
        for (name, cell) in values {
            ensure!(
                !name.is_empty() && !name.contains('\0'),
                "mongodb_field_name_unsupported"
            );
            let shape = self.shape(table, name);
            let empty = matches!(
                cell,
                Cell::Integer(text) | Cell::Decimal(text) | Cell::Float(text) | Cell::Text(text)
                    if text.trim().is_empty()
            );
            if empty && !matches!(shape, Shape::Uniform("string") | Shape::Open) {
                continue;
            }
            document.insert(
                name.clone(),
                value::field_value(cell, shape, Source::Typed)?,
            );
        }
        Ok(document)
    }

    /// One change as the command of the console that makes it. The types
    /// follow what is known about the fields; `apply` reads the document
    /// itself and may know more.
    fn change(&self, change: &RowChange) -> Result<String> {
        Ok(match change {
            RowChange::UpdateCell {
                table,
                column,
                old,
                new,
                key,
            } => {
                ensure!(column != "_id", "mongodb_id_immutable");
                ensure!(settable(column), "mongodb_field_name_unsupported");
                let id = shown_key(key_of(key)?, self.shape(table, "_id"));
                let mut filter = doc! {"_id": id};
                filter.insert(column.clone(), value::exact(old)?);
                let mut set = Document::new();
                set.insert(
                    column.clone(),
                    value::edited_value(new, None, self.shape(table, column))?,
                );
                format!(
                    "{}.updateOne({}, {})",
                    target(table),
                    value::shell(&Bson::Document(filter)),
                    value::shell(&Bson::Document(doc! {"$set": set}))
                )
            }
            RowChange::InsertRow { table, values } => format!(
                "{}.insertOne({})",
                target(table),
                value::shell(&Bson::Document(self.new_document(table, values)?))
            ),
            RowChange::DeleteRow { table, key } => format!(
                "{}.deleteOne({})",
                target(table),
                value::shell(&Bson::Document(
                    doc! {"_id": shown_key(key_of(key)?, self.shape(table, "_id"))}
                ))
            ),
        })
    }

    /// The document a key of the grid addresses. A text may be the key of
    /// two documents, as a text and as an ObjectId or a date with the same
    /// characters: such a key is refused, not guessed.
    async fn addressed(
        &self,
        scope: &mut Scope,
        table: &TableRef,
        key: &[(String, Cell)],
    ) -> Result<Document> {
        let found = scope
            .documents(
                &self.client.database(&table.schema),
                doc! {"find": table.name.as_str(), "filter": {"_id": id_condition(key_of(key)?)}, "limit": 2},
            )
            .await
            .map_err(failure)?;
        let mut documents = found.documents.into_iter();
        match (documents.next(), documents.next()) {
            (Some(document), None) => Ok(document),
            (None, _) => Err(anyhow!("row_changed_reload_required")),
            (Some(_), Some(_)) => Err(anyhow!("mongodb_key_ambiguous")),
        }
    }

    async fn change_now(&self, scope: &mut Scope, change: &RowChange) -> Result<()> {
        match change {
            RowChange::UpdateCell {
                table,
                column,
                old,
                new,
                key,
            } => {
                ensure!(column != "_id", "mongodb_id_immutable");
                ensure!(settable(column), "mongodb_field_name_unsupported");
                key_of(key)?;
                let fields = self.fields(table).await?;
                let database = self.client.database(&table.schema);
                // The document as it is now says what the field holds and
                // of which type; the grid only has the text of it.
                let document = self.addressed(scope, table, key).await?;
                let current = document.get(column);
                // The conflict check: the field still shows what was edited.
                ensure!(value::cell(current) == *old, "row_changed_reload_required");
                let next = value::edited_value(new, current, fields.shape(column))?;
                // And it must still hold it when the change is made: the
                // same value of the same type, or still no field at all.
                let unchanged = match current {
                    None => doc! {"$exists": false},
                    Some(Bson::Undefined) => doc! {"$type": "undefined"},
                    Some(current) => {
                        doc! {"$eq": current.clone(), "$type": value::type_alias(current)}
                    }
                };
                let mut filter = doc! {"_id": document.get("_id").cloned().unwrap_or(Bson::Null)};
                filter.insert(column.clone(), unchanged);
                let mut set = Document::new();
                set.insert(column.clone(), next);
                let reply = scope
                    .command(
                        &database,
                        doc! {"update": table.name.as_str(), "updates": [{"q": filter, "u": {"$set": set}, "multi": false}]},
                    )
                    .await
                    .map_err(failure)?;
                if let Some((code, _)) = console::write_error(&reply) {
                    return Err(server_failure(code));
                }
                ensure!(
                    reply.get("n").and_then(shell::whole) == Some(1),
                    "row_changed_reload_required"
                );
            }
            RowChange::InsertRow { table, values } => {
                self.fields(table).await?;
                let (_, document) = console::with_id(self.new_document(table, values)?);
                let reply = scope
                    .command(
                        &self.client.database(&table.schema),
                        doc! {"insert": table.name.as_str(), "documents": [document]},
                    )
                    .await
                    .map_err(failure)?;
                if let Some((code, _)) = console::write_error(&reply) {
                    return Err(server_failure(code));
                }
            }
            RowChange::DeleteRow { table, key } => {
                let mut values = key_of(key)?;
                let id = if values.len() == 1 {
                    values.remove(0)
                } else {
                    // The key may be one of several values: the document
                    // says which, and only that one is removed.
                    let document = self.addressed(scope, table, key).await?;
                    document.get("_id").cloned().unwrap_or(Bson::Null)
                };
                let reply = scope
                    .command(
                        &self.client.database(&table.schema),
                        doc! {"delete": table.name.as_str(), "deletes": [{"q": {"_id": id}, "limit": 1}]},
                    )
                    .await
                    .map_err(failure)?;
                if let Some((code, _)) = console::write_error(&reply) {
                    return Err(server_failure(code));
                }
                ensure!(
                    reply.get("n").and_then(shell::whole) == Some(1),
                    "row_changed_reload_required"
                );
            }
        }
        Ok(())
    }

    /// Remove documents an import wrote before it failed.
    async fn undo_import(&self, table: &TableRef, written: &[Bson]) -> Result<()> {
        let database = self.client.database(&table.schema);
        for ids in written.chunks(console::INSERT_BATCH) {
            let reply = database
                .run_command(
                    doc! {"delete": table.name.as_str(), "deletes": [{"q": {"_id": {"$in": ids.to_vec()}}, "limit": 0}]},
                )
                .await
                .map_err(|_| anyhow!("mongodb_import_incomplete"))?;
            ensure!(
                console::write_error(&reply).is_none(),
                "mongodb_import_incomplete"
            );
        }
        Ok(())
    }

    /// The row of the "Monitor" section.
    async fn statistics(&self) -> Result<QueryResult> {
        let started = Instant::now();
        let admin = self.client.database("admin");
        let mut columns = Vec::new();
        let mut cells = Vec::new();
        let mut add = |name: &str, kind: &str, cell: Cell| {
            columns.push(ResultColumn {
                name: name.into(),
                data_type: kind.into(),
            });
            cells.push(cell);
        };
        // The largest sections of the answer are left out: this is asked
        // every five seconds. `metrics` stays: MongoDB 8 leaves the memory
        // figures out together with it.
        let status = admin
            .run_command(
                doc! {"serverStatus": 1, "locks": 0, "wiredTiger": 0, "tcmalloc": 0, "repl": 0},
            )
            .await;
        let status = match status {
            Ok(status) => Some(status),
            // A service or an account without `serverStatus` still has a
            // version and a response time.
            Err(error) if server_code(&error).is_some() => {
                admin.run_command(doc! {"ping": 1}).await.map_err(failure)?;
                None
            }
            Err(error) => return Err(failure(error)),
        };
        let number = |document: &Document, key: &str| document.get(key).and_then(shell::whole);
        let mut scope = Scope::plain();
        // The work of clients only: not the threads of the server itself
        // (checkpoints, the journal), and not what a client keeps open to
        // watch the server (a `hello` that waits for a change).
        let operations = self.operations(&mut scope).await.ok().map(|operations| {
            operations
                .into_iter()
                .filter(|operation| {
                    operation.get_bool("active").unwrap_or(false)
                        && operation.contains_key("client")
                        && !operation.get_document("command").is_ok_and(|command| {
                            command.keys().next().is_some_and(|name| {
                                matches!(name.as_str(), "hello" | "isMaster" | "ismaster")
                            })
                        })
                })
                .collect::<Vec<_>>()
        });
        if let Some(connections) = status
            .as_ref()
            .and_then(|status| status.get_document("connections").ok())
        {
            let current = number(connections, "current").unwrap_or(0);
            add("connections", "int64", Cell::Integer(current.to_string()));
            // Operations in progress; a server that will not list them
            // still counts the connections that are busy.
            let active = operations
                .as_ref()
                .map(|operations| operations.len() as i64)
                .or(number(connections, "active"));
            if let Some(active) = active {
                add("active", "int64", Cell::Integer(active.to_string()));
            }
            if let Some(available) = number(connections, "available") {
                add(
                    "max_connections",
                    "int64",
                    Cell::Integer((current + available).to_string()),
                );
            }
        }
        let version = match self.version.as_str() {
            "" => self.topology.clone(),
            version => format!("{version}, {}", self.topology),
        };
        add("version", "string", Cell::Text(version));
        add("database", "string", Cell::Text(self.database_in_use()));
        if let Some(status) = &status {
            if let Ok(counters) = status.get_document("opcounters") {
                let now = COUNTERS.map(|name| number(counters, name).unwrap_or(0));
                let text = |values: [i64; 6], unit: &str| {
                    COUNTERS
                        .iter()
                        .zip(values)
                        .map(|(name, value)| format!("{name} {value}{unit}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                add("opcounters", "string", Cell::Text(text(now, "")));
                if let Ok(mut previous) = self.counters.lock() {
                    // Operations per second since the reading before this one.
                    if let Some((at, before)) = *previous {
                        let seconds = at.elapsed().as_secs_f64();
                        if (1.0..=60.0).contains(&seconds) {
                            let mut rates = [0_i64; 6];
                            for (index, rate) in rates.iter_mut().enumerate() {
                                *rate = ((now[index] - before[index]).max(0) as f64 / seconds)
                                    .round() as i64;
                            }
                            add("ops/s", "string", Cell::Text(text(rates, "")));
                        }
                    }
                    *previous = Some((Instant::now(), now));
                }
            }
            if let Ok(memory) = status.get_document("mem") {
                let megabytes =
                    |key: &str| number(memory, key).map(|size| format!("{key} {size} MB"));
                let parts = [megabytes("resident"), megabytes("virtual")]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                if !parts.is_empty() {
                    add("memory", "string", Cell::Text(parts.join(", ")));
                }
            }
        }
        if let Some(operations) = &operations {
            let mut text = format!("{} active", operations.len());
            let longest = operations
                .iter()
                .filter_map(|operation| Some((number(operation, "secs_running")?, operation)))
                .max_by_key(|(seconds, _)| *seconds);
            if let Some((seconds, operation)) = longest.filter(|(seconds, _)| *seconds > 0) {
                text.push_str(&format!(
                    ", longest {}: {} {}",
                    duration_text(seconds),
                    operation.get_str("op").unwrap_or("?"),
                    operation.get_str("ns").unwrap_or_default()
                ));
            }
            add(
                "currentOp",
                "string",
                Cell::Text(text.trim_end().to_owned()),
            );
        }
        if let Some(uptime) = status.as_ref().and_then(|status| number(status, "uptime")) {
            add("uptime", "string", Cell::Text(duration_text(uptime)));
        }
        Ok(QueryResult {
            columns,
            rows: vec![cells],
            duration_ms: started.elapsed().as_millis() as u64,
            ..QueryResult::default()
        })
    }
}

/// The statements of a script as planned commands, or the first thing in
/// it that is not understood, with its place.
fn planned(text: &str) -> Result<Vec<console::Op>> {
    let located = |code: &str, offset: usize| {
        let (line, column) = shell::position(text, offset);
        anyhow!("{code}_{line}_{column}")
    };
    shell::parse_script(text)
        .map_err(|error| located(error.code, error.offset))?
        .iter()
        .map(|statement| {
            console::plan(&statement.statement).map_err(|code| located(code, statement.offset))
        })
        .collect()
}

/// The connection test of the application sends this SQL statement to
/// every engine; here it is answered by a ping of the server.
fn is_probe(text: &str) -> bool {
    text.trim()
        .trim_end_matches(';')
        .trim_end()
        .eq_ignore_ascii_case("select 1")
}

impl Session for MongoSession {
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
            let id = SessionId::default();
            let mut result = if is_probe(text) {
                self.race(&cancel, METADATA, &id, async {
                    self.client
                        .database("admin")
                        .run_command(doc! {"ping": 1})
                        .await
                        .map_err(failure)?;
                    Ok(QueryResult {
                        columns: vec![ResultColumn {
                            name: "ok".into(),
                            data_type: "int32".into(),
                        }],
                        rows: vec![vec![Cell::Integer("1".into())]],
                        ..QueryResult::default()
                    })
                })
                .await?
            } else {
                // Everything is read before anything runs: a script with a
                // mistake, or with a command that needs `confirm`, changes
                // nothing.
                let commands = planned(text)?;
                self.race(&cancel, STATEMENT, &id, async {
                    let mut scope = Scope::open(&self.client, &id).await;
                    let mut last = QueryResult::default();
                    let mut affected = 0_u64;
                    for command in &commands {
                        last = self.run(&mut scope, command).await?;
                        affected = affected.saturating_add(last.affected);
                    }
                    if last.columns.is_empty() {
                        last.affected = affected;
                    }
                    Ok(last)
                })
                .await?
            };
            result.duration_ms = started.elapsed().as_millis() as u64;
            Ok(result)
        })
    }

    fn containers<'a>(&'a self) -> BoxFuture<'a, Result<Vec<TableRef>>> {
        Box::pin(async move {
            let id = SessionId::default();
            self.race(&CancellationToken::new(), METADATA, &id, async {
                let listed = self
                    .client
                    .database("admin")
                    .run_command(
                        doc! {"listDatabases": 1, "nameOnly": true, "authorizedDatabases": true},
                    )
                    .await;
                let mut databases = match listed {
                    Ok(reply) => reply
                        .get_array("databases")
                        .map(|entries| {
                            entries
                                .iter()
                                .filter_map(Bson::as_document)
                                .filter_map(|entry| entry.get_str("name").ok())
                                .map(str::to_owned)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                    // An account that may not list databases works in its own.
                    Err(error) if unauthorized(&error) => vec![self.home.clone()],
                    Err(error) => return Err(failure(error)),
                };
                // The server's own databases are not the user's data: they
                // are listed only when the profile names one of them.
                databases.retain(|name| !system_database(name) || *name == self.home);
                databases.sort();
                let read = futures_util::stream::iter(databases)
                    .map(|database| async move {
                        let found = self.collections(&mut Scope::plain(), &database).await;
                        (database, found)
                    })
                    .buffered(6)
                    .collect::<Vec<_>>()
                    .await;
                let mut containers = Vec::new();
                let mut refused = None;
                let total = read.len();
                for (database, found) in read {
                    match found {
                        Ok(mut collections) => {
                            // Definitions of views, the profiler: reached
                            // from the console.
                            if !system_database(&database) {
                                collections.retain(|(name, _)| !name.starts_with("system."));
                            }
                            containers.extend(collections.into_iter().map(|(name, kind)| {
                                TableRef {
                                    schema: database.clone(),
                                    // What can be changed by `_id` is a table for
                                    // the interface. A time series is not, and
                                    // the server's own collections are left alone.
                                    kind: match kind.as_str() {
                                        "view" => "view",
                                        "timeseries" => "time series",
                                        _ if name.starts_with("system.") => "system collection",
                                        _ => "table",
                                    }
                                    .into(),
                                    name,
                                }
                            }));
                        }
                        Err(error) => refused = Some(error),
                    }
                }
                // One database that cannot be read does not hide the others.
                match refused {
                    Some(error) if total == 1 => Err(error),
                    _ => Ok(containers),
                }
            })
            .await
        })
    }

    fn columns<'a>(&'a self, container: &'a TableRef) -> BoxFuture<'a, Result<Vec<ColumnInfo>>> {
        Box::pin(async move {
            let id = SessionId::default();
            self.race(&CancellationToken::new(), METADATA, &id, async {
                Ok(self.learn(container).await?.columns())
            })
            .await
        })
    }

    fn server_stats<'a>(&'a self) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            let id = SessionId::default();
            self.race(&CancellationToken::new(), METADATA, &id, self.statistics())
                .await
        })
    }

    fn close<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let client = self.client.clone();
            let _ = tokio::time::timeout(Duration::from_secs(5), async { client.shutdown().await })
                .await;
        })
    }

    fn page_text(&self, page: &PageRequest) -> Result<String> {
        Ok(PageQuery::of(page, self.flavor.searches_any_type())?.text(&page.container))
    }

    fn read_page<'a>(
        &'a self,
        page: &'a PageRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<QueryResult>> {
        Box::pin(async move {
            ensure!(!cancel.is_cancelled(), "query_cancelled");
            let started = Instant::now();
            let query = PageQuery::of(page, self.flavor.searches_any_type())?;
            let id = SessionId::default();
            let mut result = self
                .race(&cancel, STATEMENT, &id, async {
                    let container = &page.container;
                    let mut scope = Scope::open(&self.client, &id).await;
                    let batch = scope
                        .documents(
                            &self.client.database(&container.schema),
                            query.command(&container.name),
                        )
                        .await
                        .map_err(failure)?;
                    // The columns are those of the collection, so that every
                    // page has the same ones and an empty page has them too.
                    let mut fields = self.fields(container).await?;
                    let before = fields.names().len();
                    for document in &batch.documents {
                        fields.observe(document);
                    }
                    if fields.names().len() > before
                        && let Ok(mut known) = self.known.lock()
                        && let Some(known) =
                            known.get_mut(&(container.schema.clone(), container.name.clone()))
                    {
                        // A field no sample has shown: it is a column of
                        // the collection from now on.
                        for name in &fields.names()[before..] {
                            known.fields.adopt(&fields, name);
                            known.extras.push(name.clone());
                        }
                    }
                    let mut columns = fields.result_columns();
                    if columns.is_empty() {
                        // An empty collection still has its key.
                        columns.push(ResultColumn {
                            name: "_id".into(),
                            data_type: "objectId".into(),
                        });
                    }
                    Ok(QueryResult {
                        columns,
                        rows: batch
                            .documents
                            .iter()
                            .map(|document| fields.row(document))
                            .collect(),
                        truncated: batch.truncated,
                        ..QueryResult::default()
                    })
                })
                .await?;
            result.duration_ms = started.elapsed().as_millis() as u64;
            Ok(result)
        })
    }

    fn change_text(&self, change: &RowChange) -> Result<String> {
        self.change(change)
    }

    fn apply<'a>(&'a self, change: &'a RowChange) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let id = SessionId::default();
            self.race(&CancellationToken::new(), STATEMENT, &id, async {
                let mut scope = Scope::open(&self.client, &id).await;
                self.change_now(&mut scope, change).await
            })
            .await
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
            let fields = self.fields(container).await?;
            let mut ids = Vec::with_capacity(rows.len());
            // Keys made here cannot belong to a document that was there
            // before: they are safe to remove again after a failure.
            let mut own = Vec::with_capacity(rows.len());
            let mut documents = Vec::with_capacity(rows.len());
            for row in rows {
                let mut document = Document::new();
                for (name, cell) in columns.iter().zip(row) {
                    // A field without a value is left out of the document.
                    if !matches!(cell, Cell::Null) {
                        document.insert(
                            name.clone(),
                            value::field_value(cell, fields.shape(name), Source::Imported)?,
                        );
                    }
                }
                own.push(!document.contains_key("_id"));
                let (id, document) = console::with_id(document);
                ids.push(id);
                documents.push(document);
            }
            let database = self.client.database(&container.schema);
            // A stand-alone server has no transactions, so "all or none" is
            // kept by removing what was written when a later document fails.
            let mut written: Vec<Bson> = Vec::new();
            let mut outcome = Ok(());
            for (index, batch) in documents.chunks(console::INSERT_BATCH).enumerate() {
                let from = index * console::INSERT_BATCH;
                if cancel.is_cancelled() {
                    outcome = Err(anyhow!("query_cancelled"));
                    break;
                }
                let sent = database
                    .run_command(doc! {"insert": container.name.as_str(), "documents": batch.to_vec(), "ordered": true})
                    .await;
                match sent {
                    Ok(reply) => {
                        let inserted = reply
                            .get("n")
                            .and_then(shell::whole)
                            .unwrap_or(0)
                            .clamp(0, batch.len() as i64)
                            as usize;
                        written.extend_from_slice(&ids[from..from + inserted]);
                        if let Some((code, _)) = console::write_error(&reply) {
                            outcome = Err(server_failure(code));
                            break;
                        }
                    }
                    Err(error) => {
                        // Nobody knows how far this batch got. The documents
                        // with keys made here are ours wherever they are.
                        written.extend(
                            (from..from + batch.len())
                                .filter(|at| own[*at])
                                .map(|at| ids[at].clone()),
                        );
                        outcome = Err(failure(error));
                        break;
                    }
                }
            }
            if let Err(error) = outcome {
                self.undo_import(container, &written).await?;
                return Err(error);
            }
            Ok(QueryResult {
                affected: written.len() as u64,
                duration_ms: started.elapsed().as_millis() as u64,
                ..QueryResult::default()
            })
        })
    }
}

/// Names of the server's error numbers that are met most often, for the
/// sentence of an error this driver has no sentence of its own for.
fn error_name(code: i64) -> Option<&'static str> {
    Some(match code {
        2 => "BadValue",
        9 => "FailedToParse",
        14 => "TypeMismatch",
        16 => "InvalidLength",
        20 => "IllegalOperation",
        27 => "IndexNotFound",
        28 => "PathNotViable",
        40 => "ConflictingUpdateOperators",
        43 => "CursorNotFound",
        52 => "DollarPrefixedFieldName",
        54 => "NotSingleValueField",
        56 => "EmptyFieldName",
        57 => "DottedFieldName",
        66 => "ImmutableField",
        67 => "CannotCreateIndex",
        72 => "InvalidOptions",
        73 => "InvalidNamespace",
        85 => "IndexOptionsConflict",
        86 => "IndexKeySpecsConflict",
        96 => "OperationFailed",
        112 => "WriteConflict",
        115 => "CommandNotSupported",
        166 => "CommandNotSupportedOnView",
        168 => "InvalidPipelineOperator",
        197 => "InvalidIndexSpecificationOption",
        241 => "ConversionFailure",
        262 => "ExceededTimeLimit",
        292 => "QueryExceededMemoryLimitNoDiskUseAllowed",
        11000 => "DuplicateKey",
        10334 => "BSONObjectTooLarge",
        17280 => "KeyTooLong",
        _ => return None,
    })
}

/// Keep a sentence that was put together for a code, so that it can be
/// handed out as a `&'static str`. The codes carry numbers (a place in a
/// script, an error number), so the set is bounded: beyond the bound the
/// sentence without the numbers is used.
fn remembered(code: &str, en: bool, make: impl FnOnce() -> String) -> Option<&'static str> {
    static SENTENCES: Mutex<Option<HashMap<(String, bool), &'static str>>> = Mutex::new(None);
    let mut sentences = SENTENCES.lock().ok()?;
    let sentences = sentences.get_or_insert_with(HashMap::new);
    if let Some(sentence) = sentences.get(&(code.to_owned(), en)) {
        return Some(sentence);
    }
    if sentences.len() >= 512 {
        return None;
    }
    let sentence: &'static str = Box::leak(make().into_boxed_str());
    sentences.insert((code.to_owned(), en), sentence);
    Some(sentence)
}

/// The codes of the console, which end in the line and the column of the
/// place they are about.
const PLACED: &[(&str, (&str, &str))] = &[
    (
        shell::SYNTAX,
        (
            "Polecenia nie da się odczytać: w tym miejscu brakuje nawiasu, przecinka, cudzysłowu albo wartości.",
            "The command cannot be read: a bracket, a comma, a quote or a value is missing or misplaced here.",
        ),
    ),
    (
        shell::SCRIPT,
        (
            "Konsola nie wykonuje JavaScriptu: zmienne, funkcje i działania nie są obsługiwane. Polecenie zaczyna się od db, show albo use, a jego argumenty są wartościami.",
            "The console does not run JavaScript: variables, functions and operators are not supported. A command starts with db, show or use, and its arguments are values.",
        ),
    ),
    (
        shell::VALUE,
        (
            "Ta wartość nie pasuje do swojego typu: sprawdź ObjectId, datę, liczbę, dane binarne albo nazwę bazy.",
            "This value does not fit its type: check the ObjectId, the date, the number, the binary data or the database name.",
        ),
    ),
    (
        console::METHOD,
        (
            "Konsola nie zna tej metody. Na kolekcji: find, findOne, countDocuments, estimatedDocumentCount, aggregate, distinct, insertOne, insertMany, updateOne, updateMany, replaceOne, deleteOne, deleteMany, createIndex, getIndexes, dropIndex, drop, stats. Po find: sort, skip, limit, projection, hint, collation, maxTimeMS, count. Na db: runCommand, adminCommand, getSiblingDB, getCollection, getCollectionNames, getCollectionInfos, createCollection, dropDatabase, stats, serverStatus, currentOp, killOp, version, getName.",
            "The console does not know this method. On a collection: find, findOne, countDocuments, estimatedDocumentCount, aggregate, distinct, insertOne, insertMany, updateOne, updateMany, replaceOne, deleteOne, deleteMany, createIndex, getIndexes, dropIndex, drop, stats. After find: sort, skip, limit, projection, hint, collation, maxTimeMS, count. On db: runCommand, adminCommand, getSiblingDB, getCollection, getCollectionNames, getCollectionInfos, createCollection, dropDatabase, stats, serverStatus, currentOp, killOp, version, getName.",
        ),
    ),
    (
        console::ARGUMENTS,
        (
            "Ta metoda nie przyjmuje takich argumentów: sprawdź ich liczbę i rodzaj. updateOne i updateMany wymagają operatorów ($set, $inc…), replaceOne całego dokumentu bez operatorów.",
            "This method does not take these arguments: check their number and kind. updateOne and updateMany need operators ($set, $inc…), replaceOne a whole document without operators.",
        ),
    ),
    (
        console::CONFIRMATION,
        (
            "To polecenie usuwa albo zmienia wszystkie dokumenty kolekcji, całą kolekcję albo całą bazę. Nic nie zostało wykonane. Jeśli tego chcesz, wpisz przed nim słowo confirm, na przykład: confirm db.orders.drop().",
            "This command removes or rewrites every document of a collection, a whole collection or a whole database. Nothing was run. If that is what you want, write the word confirm in front of it, for example: confirm db.orders.drop().",
        ),
    ),
];

fn describe(code: &str, en: bool) -> Option<&'static str> {
    let pick = |text: (&'static str, &'static str)| if en { text.1 } else { text.0 };
    let fixed = match code {
        "mongodb_host_invalid" => (
            "W polu Host wpisz samą nazwę serwera, bez mongodb://, konta, portu i parametrów: mają one własne pola. Dla adresu mongodb+srv:// wpisz jego nazwę hosta i zaznacz „Lista DNS (mongodb+srv)”.",
            "Enter the name of the server alone in the Host field, without mongodb://, the account, the port and parameters: they have fields of their own. For a mongodb+srv:// address enter its host name and turn on “DNS seed list (mongodb+srv)”.",
        ),
        "mongodb_srv_through_tunnel" => (
            "Lista DNS (mongodb+srv) nie działa przez tunel SSH: tunel przekazuje jeden port jednego serwera. Wyłącz ją i wpisz nazwę jednego serwera klastra.",
            "A DNS seed list (mongodb+srv) does not work through an SSH tunnel: a tunnel forwards one port of one server. Turn it off and enter the name of one server of the cluster.",
        ),
        "mongodb_options_conflict" => (
            "Te ustawienia się wykluczają: lista DNS nie łączy się z połączeniem bezpośrednim ani z dodatkowymi hostami, a połączenie bezpośrednie dotyczy jednego serwera.",
            "These settings exclude each other: a DNS seed list goes neither with a direct connection nor with additional hosts, and a direct connection is to one server.",
        ),
        "mongodb_extra_hosts_invalid" => (
            "Dodatkowe hosty wpisz jako host:port, oddzielone przecinkami.",
            "Enter the additional hosts as host:port, separated by commas.",
        ),
        "mongodb_srv_lookup_failed" => (
            "Nie udało się odczytać z DNS listy serwerów tego hosta. Sprawdź nazwę z adresu mongodb+srv:// albo wyłącz „Lista DNS (mongodb+srv)”.",
            "The list of servers of this host could not be read from DNS. Check the name from the mongodb+srv:// address, or turn off “DNS seed list (mongodb+srv)”.",
        ),
        "mongodb_no_server_selected" => (
            "W wyznaczonym czasie nie znaleziono serwera, który przyjąłby polecenie: serwer nie odpowiada albo zestaw replik nie ma serwera głównego. Sprawdź połączenie i nazwę zestawu replik albo zaznacz „Połączenie bezpośrednie”.",
            "No server that would take the command was found in time: the server does not answer, or the replica set has no primary. Check the connection and the name of the replica set, or turn on “Direct connection”.",
        ),
        "mongodb_member_unreachable" => (
            "Zestaw replik podaje adres serwera, którego stąd nie da się osiągnąć. Zaznacz „Połączenie bezpośrednie”, aby pracować tylko z podanym serwerem.",
            "The replica set names a server that cannot be reached from here. Turn on “Direct connection” to work with the given server only.",
        ),
        "mongodb_connection_closed" => (
            "Serwer od razu zamknął połączenie. Tak odpowiada serwer, który wymaga szyfrowania, gdy SSL/TLS jest wyłączony: sprawdź kartę SSL/TLS.",
            "The server closed the connection at once. A server that requires encryption answers like this when SSL/TLS is off: check the SSL/TLS tab.",
        ),
        "mongodb_ca_file_unreadable" => (
            "Nie udało się odczytać certyfikatu CA. Wskaż na karcie SSL/TLS plik w formacie PEM.",
            "The CA certificate could not be read. Choose a file in PEM format on the SSL/TLS tab.",
        ),
        "mongodb_not_authorized" => (
            "To konto nie ma uprawnień do tej operacji.",
            "This account is not allowed to do this.",
        ),
        "mongodb_namespace_missing" => (
            "Nie ma takiej kolekcji albo bazy.",
            "There is no such collection or database.",
        ),
        "mongodb_namespace_exists" => (
            "Kolekcja o tej nazwie już istnieje.",
            "A collection with this name exists already.",
        ),
        "mongodb_command_unknown" => (
            "Serwer nie zna tego polecenia.",
            "The server does not know this command.",
        ),
        "mongodb_not_primary" => (
            "Ten serwer nie jest serwerem głównym zestawu replik i nie przyjmuje zapisów. Połącz się z serwerem głównym albo wyłącz „Połączenie bezpośrednie”, aby został znaleziony.",
            "This server is not the primary of its replica set and takes no writes. Connect to the primary, or turn off “Direct connection” so that it is found.",
        ),
        "mongodb_duplicate_key" => (
            "Dokument z takim _id albo z taką wartością pola o unikalnym indeksie już istnieje.",
            "A document with this _id, or with this value of a field that has a unique index, exists already.",
        ),
        "mongodb_validation_failed" => (
            "Dokument nie spełnia reguł walidacji kolekcji.",
            "The document does not pass the validation rules of the collection.",
        ),
        "mongodb_id_immutable" => (
            "Pola _id nie można zmienić. Dodaj dokument z nowym _id i usuń stary.",
            "The _id of a document cannot be changed. Add a document with the new _id and remove the old one.",
        ),
        "mongodb_field_name_unsupported" => (
            "Pola o pustej nazwie, z kropką w nazwie albo z nazwą od znaku $ nie da się tu zmieniać, filtrować ani sortować. Użyj konsoli.",
            "A field with an empty name, with a dot in its name or with a name that starts with $ cannot be changed, filtered or sorted here. Use the console.",
        ),
        "mongodb_date_invalid" => (
            "Wpisz datę jako 2026-05-01 albo 2026-05-01T10:30:00Z.",
            "Enter the date as 2026-05-01 or 2026-05-01T10:30:00Z.",
        ),
        "mongodb_objectid_invalid" => (
            "ObjectId to 24 cyfry szesnastkowe.",
            "An ObjectId is 24 hexadecimal digits.",
        ),
        "mongodb_key_ambiguous" => (
            "Dwa dokumenty mają klucze, które wyglądają tu tak samo: tekst oraz ObjectId albo data o tych samych znakach. Zmień albo usuń taki dokument w konsoli.",
            "Two documents have keys that look the same here: a text, and an ObjectId or a date with the same characters. Change or remove such a document in the console.",
        ),
        "mongodb_import_incomplete" => (
            "Import się nie powiódł, a dokumentów zapisanych przed błędem nie udało się usunąć: część pliku została w kolekcji.",
            "The import failed, and the documents written before the failure could not be removed: a part of the file stays in the collection.",
        ),
        _ => return describe_numbered(code, en),
    };
    Some(pick(fixed))
}

/// Sentences for the codes that carry numbers.
fn describe_numbered(code: &str, en: bool) -> Option<&'static str> {
    let named = |number: i64| match error_name(number) {
        Some(name) => format!("{number} ({name})"),
        None => number.to_string(),
    };
    if let Some(number) = code.strip_prefix("mongodb_server_error_") {
        let number: i64 = number.parse().ok()?;
        const PLAIN: (&str, &str) = (
            "Serwer odrzucił polecenie.",
            "The server rejected the command.",
        );
        return remembered(code, en, || {
            if en {
                format!(
                    "The server rejected the command: MongoDB error {}.",
                    named(number)
                )
            } else {
                format!("Serwer odrzucił polecenie: błąd MongoDB {}.", named(number))
            }
        })
        .or(Some(if en { PLAIN.1 } else { PLAIN.0 }));
    }
    if let Some(rest) = code.strip_prefix("mongodb_write_failed_") {
        let (number, written) = rest.split_once("_after_")?;
        let (number, written): (i64, u64) = (number.parse().ok()?, written.parse().ok()?);
        const PLAIN: (&str, &str) = (
            "Serwer odrzucił jeden z dokumentów; część dokumentów tego polecenia została zapisana.",
            "The server rejected one of the documents; a part of the documents of this command was written.",
        );
        return remembered(code, en, || {
            if en {
                format!(
                    "The server rejected a document: MongoDB error {}. Documents of this command written before it: {written}.",
                    named(number)
                )
            } else {
                format!(
                    "Serwer odrzucił dokument: błąd MongoDB {}. Dokumenty tego polecenia zapisane wcześniej: {written}.",
                    named(number)
                )
            }
        })
        .or(Some(if en { PLAIN.1 } else { PLAIN.0 }));
    }
    // `<code>_<line>_<column>` of the console.
    let (rest, column) = code.rsplit_once('_')?;
    let (kind, line) = rest.rsplit_once('_')?;
    let (line, column): (u32, u32) = (line.parse().ok()?, column.parse().ok()?);
    let (_, sentence) = PLACED.iter().find(|(name, _)| *name == kind)?;
    let sentence = if en { sentence.1 } else { sentence.0 };
    remembered(code, en, || {
        if en {
            format!("{sentence} Place: line {line}, column {column}.")
        } else {
            format!("{sentence} Miejsce: wiersz {line}, kolumna {column}.")
        }
    })
    .or(Some(sentence))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::Filter;
    use crate::model::ConnectionProfile;

    fn profile() -> ConnectionProfile {
        ConnectionProfile {
            name: "Documents".into(),
            engine: ENGINE,
            host: "db.example.test".into(),
            port: 27017,
            tls: TlsMode::Disable,
            ..ConnectionProfile::default()
        }
    }
    fn settings(profile: &ConnectionProfile, forwarded: Option<u16>) -> Result<Settings> {
        profile.validate().unwrap();
        Settings::of(&ConnectRequest {
            profile,
            product: profile.product_spec(),
            forwarded: forwarded.map(|port| SocketAddr::from(([127, 0, 0, 1], port))),
        })
    }
    fn code<T: std::fmt::Debug>(result: Result<T>) -> String {
        result.unwrap_err().to_string()
    }

    #[test]
    fn the_descriptor_offers_what_documents_allow() {
        let caps = SPEC.capabilities;
        assert_eq!(caps.language, QueryLanguage::Command);
        assert_eq!(caps.browse, BrowseModel::Documents);
        assert!(caps.schemas && caps.filter && caps.sort && caps.monitor);
        assert!(caps.edit_cells && caps.insert_rows && caps.delete_rows);
        assert!(caps.import && caps.export);
        assert!(!caps.table_designer && !caps.query_builder);
        assert_eq!(Engine::from_id("mongo"), Some(ENGINE));
        let ports = PRODUCTS
            .iter()
            .map(|product| (product.id, product.default_port, product.default_tls))
            .collect::<Vec<_>>();
        assert_eq!(
            ports,
            [
                ("mongodb", 27017, TlsMode::Disable),
                ("mongodb-atlas", 27017, TlsMode::VerifyFull),
                ("amazon-documentdb", 27017, TlsMode::Require),
                ("azure-cosmosdb-mongodb", 10255, TlsMode::VerifyFull),
                ("ferretdb", 27017, TlsMode::Disable),
            ]
        );
        // The database is optional: a profile without one is valid.
        profile().validate().unwrap();
    }

    #[test]
    fn a_profile_becomes_the_settings_of_a_connection() {
        let mut profile = profile();
        let plain = settings(&profile, None).unwrap();
        assert_eq!(plain.seeds, [("db.example.test".to_owned(), 27017)]);
        assert!(!plain.srv && !plain.direct && !plain.relay && plain.retry_writes);
        assert_eq!(plain.auth_source, "admin");
        assert_eq!(plain.replica_set, None);
        profile.set_option("auth_source", "shop");
        profile.set_option("replica_set", "rs0");
        profile.set_option("extra_hosts", "b.example.test:27018, c.example.test");
        let set = settings(&profile, None).unwrap();
        assert_eq!(
            set.seeds,
            [
                ("db.example.test".to_owned(), 27017),
                ("b.example.test".to_owned(), 27018),
                ("c.example.test".to_owned(), 27017),
            ]
        );
        assert_eq!(set.auth_source, "shop");
        assert_eq!(set.replica_set.as_deref(), Some("rs0"));
        // Through a tunnel: the forwarded port, directly, whatever the set.
        profile.tls = TlsMode::VerifyFull;
        let tunnelled = settings(&profile, Some(40123)).unwrap();
        assert_eq!(tunnelled.seeds, [("127.0.0.1".to_owned(), 40123)]);
        assert!(tunnelled.direct && tunnelled.relay);
        assert_eq!(tunnelled.replica_set, None);
        profile.tls = TlsMode::Disable;
        assert!(!settings(&profile, Some(40123)).unwrap().relay);
        // A direct connection is to one server.
        profile.set_option("direct_connection", "true");
        assert_eq!(code(settings(&profile, None)), "mongodb_options_conflict");
        profile.set_option("extra_hosts", "");
        assert!(settings(&profile, None).unwrap().direct);
        profile.set_option("extra_hosts", "b.example.test:port");
        assert_eq!(
            code(settings(&profile, None)),
            "mongodb_extra_hosts_invalid"
        );
    }

    #[test]
    fn a_dns_seed_list_is_a_name_and_never_goes_through_a_tunnel() {
        let mut profile = profile();
        profile.product = "mongodb-atlas".into();
        profile.host = "cluster0.ab1cd.mongodb.net".into();
        profile.set_option("srv", "true");
        let srv = settings(&profile, None).unwrap();
        assert!(srv.srv && !srv.direct);
        assert_eq!(
            code(settings(&profile, Some(40123))),
            "mongodb_srv_through_tunnel"
        );
        profile.set_option("direct_connection", "true");
        assert_eq!(code(settings(&profile, None)), "mongodb_options_conflict");
        profile.set_option("direct_connection", "false");
        profile.host = "127.0.0.1".into();
        assert_eq!(code(settings(&profile, None)), "mongodb_host_invalid");
    }

    /// What a dialog needs to accept a pasted address: its parts as the
    /// fields of a profile.
    #[test]
    fn an_address_is_taken_apart_into_the_fields_of_a_profile() {
        assert_eq!(
            parse_address(
                " mongodb+srv://app:p%40ss%3Aw%2Ford@cluster0.ab1cd.mongodb.net/shop?retryWrites=true&w=majority&authSource=admin "
            ),
            Some(AddressParts {
                host: "cluster0.ab1cd.mongodb.net".into(),
                port: 27017,
                username: "app".into(),
                password: "p@ss:w/ord".into(),
                database: "shop".into(),
                tls: Some(TlsMode::VerifyFull),
                options: vec![("srv", "true".into()), ("auth_source", "admin".into())],
            })
        );
        assert_eq!(
            parse_address(
                "mongodb://a.example.test:27018,b.example.test,[::1]:27020/?replicaSet=rs0&tls=true&tlsAllowInvalidCertificates=true"
            ),
            Some(AddressParts {
                host: "a.example.test".into(),
                port: 27018,
                tls: Some(TlsMode::Require),
                options: vec![
                    ("extra_hosts", "b.example.test:27017, ::1:27020".into()),
                    ("replica_set", "rs0".into()),
                ],
                ..AddressParts::default()
            })
        );
        let local = parse_address("mongodb://localhost").unwrap();
        assert_eq!(
            (local.host.as_str(), local.port, local.tls),
            ("localhost", 27017, None)
        );
        let direct =
            parse_address("MongoDB://root@[::1]:27017/admin?directConnection=true&ssl=false")
                .unwrap();
        assert_eq!(
            (direct.host.as_str(), direct.username.as_str()),
            ("::1", "root")
        );
        assert_eq!(direct.tls, Some(TlsMode::Disable));
        assert_eq!(direct.options, [("direct_connection", "true".to_owned())]);
        // The parts make a profile the driver accepts.
        let parts = parse_address("mongodb+srv://app@cluster0.ab1cd.mongodb.net/shop").unwrap();
        let mut profile = profile();
        profile.host = parts.host;
        profile.port = parts.port;
        for (key, value) in &parts.options {
            profile.set_option(key, value);
        }
        assert!(settings(&profile, None).unwrap().srv);
        for text in [
            "db.example.test",
            "postgres://db.example.test/shop",
            "mongodb://",
            "mongodb://db.example.test:port",
            "mongodb://db.example.test:0",
            "mongodb://us%ZZer@db.example.test",
            "mongodb+srv://a.example.test,b.example.test",
            "mongodb+srv://a.example.test:27017",
            "mongodb://db.example.test/?replicaSet",
        ] {
            assert_eq!(parse_address(text), None, "{text}");
        }
    }

    /// A connection string holds an account; the host field is not the
    /// place for one.
    #[test]
    fn an_address_pasted_into_the_host_field_is_refused() {
        for host in [
            "mongodb://db.example.test",
            "mongodb+srv://user:secret@cluster0.ab1cd.mongodb.net/shop?retryWrites=true",
            "user:secret@db.example.test",
            "db.example.test/shop",
            "a.example.test,b.example.test",
        ] {
            let mut profile = profile();
            profile.host = host.into();
            assert_eq!(
                code(settings(&profile, None)),
                "mongodb_host_invalid",
                "{host}"
            );
        }
        let mut profile = profile();
        profile.host = "::1".into();
        assert_eq!(settings(&profile, None).unwrap().seeds[0].0, "::1");
    }

    #[test]
    fn hosted_emulations_are_told_apart_and_get_no_retryable_writes() {
        let mongo = doc! {"version": "8.0.4"};
        assert_eq!(
            Flavor::detect(&mongo, "mongodb", "db.example.test"),
            Flavor::Mongo { major: 8, minor: 0 }
        );
        assert!(Flavor::detect(&mongo, "", "x").searches_any_type());
        assert!(!Flavor::detect(&doc! {"version": "4.4.29"}, "", "x").searches_any_type());
        assert!(!Flavor::detect(&doc! {}, "", "x").searches_any_type());
        assert_eq!(
            Flavor::detect(
                &doc! {"version": "7.0.77", "ferretdb": {"version": "v2.1.0"}},
                "mongodb",
                "x"
            ),
            Flavor::Ferret
        );
        assert_eq!(
            Flavor::detect(
                &doc! {"version": "5.0.0"},
                "mongodb",
                "shop.cluster-abc.eu-central-1.docdb.amazonaws.com"
            ),
            Flavor::DocumentDb
        );
        assert_eq!(
            Flavor::detect(&doc! {"version": "4.2.0"}, "azure-cosmosdb-mongodb", "x"),
            Flavor::Cosmos
        );
        assert!(!Flavor::DocumentDb.searches_any_type());
        let mut profile = profile();
        assert!(settings(&profile, None).unwrap().retry_writes);
        profile.host = "shop.mongo.cosmos.azure.com".into();
        assert!(!settings(&profile, None).unwrap().retry_writes);
        profile.host = "db.example.test".into();
        profile.product = "amazon-documentdb".into();
        assert!(!settings(&profile, None).unwrap().retry_writes);
    }

    fn orders() -> TableRef {
        TableRef {
            schema: "shop".into(),
            name: "orders".into(),
            kind: "table".into(),
        }
    }
    fn filtered(column: &str, op: FilterOp, value: Cell) -> PageRequest {
        PageRequest {
            filters: vec![Filter {
                column: column.into(),
                op,
                value,
            }],
            ..PageRequest::first(orders())
        }
    }

    /// What is typed into the filter is text, whatever it looks like.
    #[test]
    fn the_contains_filter_is_escaped_and_ignores_case() {
        assert_eq!(regex_escape("100% _under_"), "100% _under_");
        assert_eq!(
            regex_escape(r"a.b*c+d?e^f$g|h(i)j[k]l{m}n\o"),
            r"a\.b\*c\+d\?e\^f\$g\|h\(i\)j\[k\]l\{m\}n\\o"
        );
        let page = filtered("label", FilterOp::Contains, Cell::Text(".* (x)".into()));
        let plain = PageQuery::of(&page, false).unwrap();
        assert_eq!(
            plain.filter,
            doc! {"label": {"$regex": r"\.\* \(x\)", "$options": "i"}}
        );
        assert_eq!(
            plain.text(&orders()),
            r#"db.getSiblingDB("shop").orders.find({label: {$regex: "\\.\\* \\(x\\)", $options: "i"}}).limit(500)"#
        );
        // Where the server can, numbers, dates and ObjectIds are searched
        // as text too.
        let rich = PageQuery::of(&page, true).unwrap();
        assert_eq!(
            rich.filter,
            doc! {"$or": [
                {"label": {"$regex": r"\.\* \(x\)", "$options": "i"}},
                {"$expr": {"$regexMatch": {
                    "input": {"$convert": {"input": "$label", "to": "string", "onError": "", "onNull": ""}},
                    "regex": r"\.\* \(x\)",
                    "options": "i",
                }}},
            ]}
        );
        // A name that is not a plain path is read as a field, not as a path.
        let odd = filtered("a.b", FilterOp::Contains, Cell::Text("x".into()));
        assert_eq!(
            code(PageQuery::of(&odd, false)),
            "mongodb_field_name_unsupported"
        );
        let field = PageQuery::of(&odd, true).unwrap().filter;
        assert_eq!(
            field
                .get_document("$expr")
                .and_then(|expression| expression.get_document("$regexMatch"))
                .and_then(|matching| matching.get_document("input"))
                .and_then(|input| input.get_document("$convert"))
                .and_then(|convert| convert.get_document("input"))
                .unwrap(),
            &doc! {"$getField": {"field": {"$literal": "a.b"}, "input": "$$ROOT"}}
        );
    }

    #[test]
    fn a_page_is_a_find_with_its_filter_order_and_place() {
        let mut page = filtered("status", FilterOp::Equal, Cell::Text("new".into()));
        page.filters.push(Filter {
            column: "total".into(),
            op: FilterOp::Greater,
            value: Cell::Integer("5".into()),
        });
        page.order_by = Some(("total".into(), true));
        page.offset = 1000;
        page.limit = 50;
        let query = PageQuery::of(&page, true).unwrap();
        assert_eq!(
            query.command("orders"),
            doc! {
                "find": "orders",
                "filter": {"$and": [{"status": "new"}, {"total": {"$gt": 5}}]},
                "sort": {"total": -1}, "skip": 1000_i64, "limit": 50_i64, "maxTimeMS": 120_000_i64,
            }
        );
        let text = query.text(&orders());
        assert_eq!(
            text,
            r#"db.getSiblingDB("shop").orders.find({$and: [{status: "new"}, {total: {$gt: 5}}]}).sort({total: -1}).skip(1000).limit(50)"#
        );
        // The text is a command the console reads as the same page.
        let commands = planned(&text).unwrap();
        assert_eq!(
            commands,
            [console::Op::Collection {
                database: Some("shop".into()),
                collection: "orders".into(),
                action: Box::new(console::Action::Find(Box::new(console::Find {
                    filter: query.filter.clone(),
                    sort: query.sort.clone(),
                    skip: Some(1000),
                    limit: Some(50),
                    ..console::Find::default()
                }))),
            }]
        );
        let first = PageQuery::of(&PageRequest::first(orders()), true).unwrap();
        assert_eq!(
            first.text(&TableRef {
                schema: "my shop".into(),
                name: "order items".into(),
                kind: "table".into(),
            }),
            r#"db.getSiblingDB("my shop").getCollection("order items").find({}).limit(500)"#
        );
        for (op, condition) in [
            (FilterOp::NotEqual, doc! {"n": {"$ne": 1}}),
            (FilterOp::Less, doc! {"n": {"$lt": 1}}),
            (FilterOp::IsNull, doc! {"n": Bson::Null}),
            (FilterOp::IsNotNull, doc! {"n": {"$ne": Bson::Null}}),
        ] {
            let page = filtered("n", op, Cell::Integer("1".into()));
            assert_eq!(PageQuery::of(&page, true).unwrap().filter, condition);
        }
        let mut odd = PageRequest::first(orders());
        odd.order_by = Some(("$where".into(), false));
        assert_eq!(
            code(PageQuery::of(&odd, true)),
            "mongodb_field_name_unsupported"
        );
    }

    #[test]
    fn a_script_is_planned_whole_and_its_mistakes_have_a_place() {
        assert_eq!(
            planned("show dbs; use shop\ndb.orders.find()")
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            code(planned("db.orders.find({a: })")),
            "mongodb_syntax_error_1_20"
        );
        assert_eq!(
            code(planned("db.orders.find()\nvar x = 1")),
            "mongodb_script_unsupported_2_1"
        );
        assert_eq!(
            code(planned("show dbs\n  db.orders.bulkWrite([])")),
            "mongodb_method_unsupported_2_3"
        );
        assert_eq!(
            code(planned("db.orders.find();\ndb.orders.deleteMany({})")),
            "mongodb_confirmation_required_2_1"
        );
        assert_eq!(
            code(planned("db.orders.find({_id: ObjectId('x')})")),
            "mongodb_value_invalid_1_22"
        );
        assert!(is_probe("SELECT 1") && is_probe(" select 1 ; ") && !is_probe("select 2"));
    }

    #[test]
    fn a_key_is_the_id_of_a_document() {
        assert_eq!(
            key_of(&[("_id".into(), Cell::Integer("7".into()))]).unwrap(),
            [Bson::Int32(7)]
        );
        assert_eq!(code(key_of(&[])), "primary_key_required");
        assert_eq!(
            code(key_of(&[("id".into(), Cell::Integer("7".into()))])),
            "primary_key_required"
        );
        // An ObjectId is shown as its digits: the key may be the ObjectId
        // or a text of the same characters, and the document says which.
        let id = Bson::ObjectId("65f0c1e2a4b5c6d7e8f90123".parse().unwrap());
        let text = Bson::String("65f0c1e2a4b5c6d7e8f90123".into());
        let values =
            key_of(&[("_id".into(), Cell::Text("65f0c1e2a4b5c6d7e8f90123".into()))]).unwrap();
        assert_eq!(values, [text.clone(), id.clone()]);
        assert_eq!(
            id_condition(values.clone()),
            Bson::Document(doc! {"$in": [text.clone(), id.clone()]})
        );
        assert_eq!(id_condition(vec![Bson::Int32(7)]), Bson::Int32(7));
        // The text of a change shows the type the keys of the collection have.
        assert_eq!(shown_key(values.clone(), Shape::Uniform("objectId")), id);
        assert_eq!(shown_key(values.clone(), Shape::Uniform("string")), text);
        assert_eq!(shown_key(values.clone(), Shape::Open), id_condition(values));
        assert!(system_database("admin") && system_database("local") && !system_database("shop"));
    }

    /// Every code of this driver reaches the interface as a sentence, in
    /// both languages.
    #[test]
    fn codes_have_sentences() {
        for code in [
            "mongodb_host_invalid",
            "mongodb_srv_through_tunnel",
            "mongodb_options_conflict",
            "mongodb_extra_hosts_invalid",
            "mongodb_srv_lookup_failed",
            "mongodb_no_server_selected",
            "mongodb_member_unreachable",
            "mongodb_connection_closed",
            "mongodb_ca_file_unreadable",
            "mongodb_not_authorized",
            "mongodb_namespace_missing",
            "mongodb_namespace_exists",
            "mongodb_command_unknown",
            "mongodb_not_primary",
            "mongodb_duplicate_key",
            "mongodb_validation_failed",
            "mongodb_id_immutable",
            "mongodb_field_name_unsupported",
            "mongodb_date_invalid",
            "mongodb_objectid_invalid",
            "mongodb_import_incomplete",
            "mongodb_key_ambiguous",
            "mongodb_server_error_2",
            "mongodb_server_error_999999",
            "mongodb_write_failed_11000_after_3",
            "mongodb_syntax_error_1_20",
            "mongodb_script_unsupported_2_1",
            "mongodb_value_invalid_1_22",
            "mongodb_method_unsupported_2_3",
            "mongodb_arguments_invalid_1_1",
            "mongodb_confirmation_required_2_1",
        ] {
            for en in [false, true] {
                let text = describe(code, en).unwrap_or_else(|| panic!("{code} has no sentence"));
                assert!(text.ends_with('.') && text.contains(' '), "{code}: {text}");
                assert!(!text.contains("mongodb_"), "{code}: {text}");
            }
            assert_ne!(describe(code, false), describe(code, true), "{code}");
        }
        assert_eq!(
            describe("mongodb_server_error_2", true),
            Some("The server rejected the command: MongoDB error 2 (BadValue).")
        );
        assert_eq!(
            describe("mongodb_write_failed_11000_after_3", true),
            Some(
                "The server rejected a document: MongoDB error 11000 (DuplicateKey). Documents of this command written before it: 3."
            )
        );
        assert!(
            describe("mongodb_syntax_error_3_14", false)
                .unwrap()
                .ends_with("Miejsce: wiersz 3, kolumna 14.")
        );
        // Codes of other modules are not answered here.
        assert_eq!(describe("database_login_rejected", true), None);
        assert_eq!(describe("mongodb_made_up", true), None);
        assert_eq!(describe("mongodb_made_up_1_2", true), None);
        assert_eq!(server_failure(11000).to_string(), "mongodb_duplicate_key");
        assert_eq!(server_failure(8).to_string(), "mongodb_server_error_8");
        assert_eq!(
            write_failure(11000, 2).to_string(),
            "mongodb_write_failed_11000_after_2"
        );
        assert_eq!(duration_text(42), "42 s");
        assert_eq!(duration_text(3 * 3600 + 12 * 60), "3 h 12 min");
        assert_eq!(duration_text(2 * 86_400 + 3 * 3600), "2 d 3 h");
    }
}
