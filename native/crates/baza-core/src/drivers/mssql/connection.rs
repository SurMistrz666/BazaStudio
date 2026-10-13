//! Connections to SQL Server: how one is opened (`Connector`), the few that
//! a session keeps (`Pool`) and what a failure is called.
//!
//! The protocol library is handed a socket this module opened itself, so the
//! address that is dialled (the server, or the loopback end of an SSH tunnel)
//! and the name of the server (for TLS and for the login packet) are two
//! separate things without any change to the library.
use crate::engine::ConnectRequest;
use crate::model::TlsMode;
use anyhow::{Result, anyhow};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiberius::error::Error;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use zeroize::Zeroizing;

/// One connection, as the protocol library drives it.
pub(super) type Tds = Client<Compat<TcpStream>>;

/// Connections a session opens at most; the same number as the other drivers.
const MAX_CONNECTIONS: usize = 4;
/// How long opening the socket, and then the handshake, may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Bytes of one packet of the protocol.
const PACKET_SIZE: u32 = 8000;
/// An unused connection is closed after this long, so that one a firewall
/// has silently dropped is not handed out.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Everything needed to open one more connection for a profile.
pub(super) struct Connector {
    /// Name of the server: for TLS, for the login packet and for the address
    /// when there is no tunnel.
    server: String,
    port: u16,
    /// Loopback end of an SSH tunnel, when the profile has one.
    forwarded: Option<SocketAddr>,
    database: String,
    username: String,
    password: Zeroizing<String>,
    tls: TlsMode,
    /// Content of the CA file of the profile.
    authority: Option<Vec<u8>>,
    /// TDS 8.0: TLS before anything else.
    strict: bool,
}

impl Connector {
    pub(super) async fn new(request: &ConnectRequest<'_>) -> Result<Self> {
        let profile = request.profile;
        // Windows and Microsoft Entra ID sign-in are not implemented: without
        // a user name there is nothing to sign in with.
        anyhow::ensure!(!profile.username.trim().is_empty(), "mssql_login_required");
        let strict = request.option("strict_encryption") == "true";
        anyhow::ensure!(
            !(strict && profile.tls == TlsMode::Disable),
            "mssql_strict_needs_tls"
        );
        let authority = match (&profile.ca_file, profile.tls) {
            (Some(path), TlsMode::VerifyFull) => Some(
                tokio::fs::read(path)
                    .await
                    .map_err(|_| anyhow!("database_tls_error"))?,
            ),
            _ => None,
        };
        Ok(Self {
            server: request.server_name().to_owned(),
            port: profile.port,
            forwarded: request.forwarded,
            database: profile.database.clone(),
            username: profile.username.clone(),
            password: Zeroizing::new(profile.password.clone()),
            tls: profile.tls,
            authority,
            strict,
        })
    }

    /// What the protocol library is told about a connection to `server`.
    fn config(&self, server: &str, port: u16) -> Config {
        let mut config = Config::new();
        config.host(server);
        config.port(port);
        if !self.database.is_empty() {
            config.database(&self.database);
        }
        config.authentication(AuthMethod::sql_server(
            &self.username,
            self.password.as_str(),
        ));
        config.application_name("Baza Studio");
        // The packets of the other Microsoft drivers. The 4096 bytes the
        // server offers by default make a large result over TLS several
        // times slower, and more than 8000 costs the server larger buffers.
        config.packet_size(PACKET_SIZE);
        // The name of this computer is nothing the server has to know.
        config.client_name("BazaStudio");
        config.handshake_timeout(Some(CONNECT_TIMEOUT));
        // A statement may think for as long as the session lets it: the
        // session ends it after two minutes, not the library after thirty
        // seconds without a packet.
        config.command_timeout(None);
        let encrypted = if self.strict {
            EncryptionLevel::Strict
        } else {
            EncryptionLevel::Required
        };
        match self.tls {
            // The sign-in is still encrypted, as every client of SQL Server
            // does it; what follows is not. The certificate of that short
            // exchange is not verified.
            TlsMode::Disable => {
                config.encryption(EncryptionLevel::Off);
                config.trust_cert();
            }
            TlsMode::Require => {
                config.encryption(encrypted);
                config.trust_cert();
            }
            TlsMode::VerifyFull => {
                config.encryption(encrypted);
                // The bundled Mozilla roots, never the store of the system,
                // and the CA file of the profile on top of them.
                config.trust_webpki_roots();
                if let Some(authority) = &self.authority {
                    config.trust_cert_ca_bundle(authority.clone());
                }
            }
        }
        config
    }

    /// Open the socket and sign in.
    async fn dial(
        &self,
        server: &str,
        port: u16,
        forwarded: Option<SocketAddr>,
    ) -> Result<Tds, Refusal> {
        let socket = tokio::time::timeout(CONNECT_TIMEOUT, async {
            match forwarded {
                Some(address) => TcpStream::connect(address).await,
                None => TcpStream::connect((server, port)).await,
            }
        })
        .await
        .map_err(|_| Refusal::Code("database_connection_timeout"))?
        .map_err(|_| Refusal::Code("database_network_error"))?;
        let _ = socket.set_nodelay(true);
        Client::connect(self.config(server, port), socket.compat_write())
            .await
            .map_err(Refusal::Library)
    }

    /// Open one connection. A gateway of Azure SQL may answer the sign-in
    /// with the address of the node that holds the database; that address is
    /// then connected to, once.
    pub(super) async fn open(&self) -> Result<Tds> {
        match self.dial(&self.server, self.port, self.forwarded).await {
            Ok(client) => Ok(client),
            Err(Refusal::Library(Error::Routing { host, port })) => {
                // The tunnel leads to the gateway and nowhere else.
                anyhow::ensure!(self.forwarded.is_none(), "mssql_redirect_through_tunnel");
                match self.dial(&host, port, None).await {
                    Ok(client) => Ok(client),
                    Err(refusal) => Err(refusal.into_error(self.strict)),
                }
            }
            Err(refusal) => Err(refusal.into_error(self.strict)),
        }
    }
}

/// Why a connection was not opened.
enum Refusal {
    Code(&'static str),
    Library(Error),
}
impl Refusal {
    fn into_error(self, strict: bool) -> anyhow::Error {
        match self {
            Refusal::Code(code) => anyhow!(code),
            Refusal::Library(error) => connect_error(&error, strict),
        }
    }
}

/// Why a connection was not opened, as a code. The answers a person can act
/// on have codes of their own; the text of the server is never passed on,
/// because it repeats the login and the name of the database.
pub(super) fn connect_error(error: &Error, strict: bool) -> anyhow::Error {
    let code = match error {
        Error::Server(token) => match sign_in_code(token.code()) {
            Some(code) => code,
            None => return anyhow!("database_error [{}]", token.code()),
        },
        Error::Tls(text) if text.contains("certificate") => "database_certificate_rejected",
        // A server before SQL Server 2022 does not know what a TLS greeting
        // in place of its own protocol is, and says nothing at all.
        Error::Tls(_) | Error::Io { .. } if strict => "mssql_strict_unsupported",
        Error::Tls(_) => "database_tls_error",
        Error::Protocol(text) if text.contains("encryption level") => "database_tls_unsupported",
        Error::Io { kind, .. } if *kind == std::io::ErrorKind::TimedOut => {
            "database_connection_timeout"
        }
        Error::Io { .. } => "database_network_error",
        _ => "database_operation_failed",
    };
    anyhow!(code)
}

/// What an error number of SQL Server means while signing in.
fn sign_in_code(number: u32) -> Option<&'static str> {
    Some(match number {
        // Wrong name or password; a login that is disabled or may not sign
        // in reads the same on purpose.
        18456 | 18452 | 18470 => "database_login_rejected",
        18486 => "mssql_account_locked",
        18487 | 18488 => "mssql_password_expired",
        // "Cannot open database … requested by the login": there is no such
        // database, or this login may not open it. The server does not say
        // which.
        4060 => "database_not_found",
        916 | 4064 => "database_access_denied",
        // Azure SQL: the firewall of the server.
        40615 => "mssql_firewall_blocked",
        _ => return None,
    })
}

/// A failure of a statement as a code, with the error number of SQL Server
/// when there is one.
pub(super) fn statement_error(error: &Error) -> anyhow::Error {
    match error {
        Error::Server(token) => anyhow!("database_error [{}]", token.code()),
        Error::Io { .. } => anyhow!("database_network_error"),
        Error::Tls(_) => anyhow!("database_tls_error"),
        _ => anyhow!("database_operation_failed"),
    }
}

/// A connection on loan from the pool. Giving it back is explicit
/// ([`Pool::release`]); a lease that is dropped closes its connection, which
/// is what has to happen to one whose state is not known.
pub(super) struct Lease {
    client: Tds,
    _permit: OwnedSemaphorePermit,
}
impl std::ops::Deref for Lease {
    type Target = Tds;
    fn deref(&self) -> &Tds {
        &self.client
    }
}
impl std::ops::DerefMut for Lease {
    fn deref_mut(&mut self) -> &mut Tds {
        &mut self.client
    }
}

struct Shared {
    connector: Connector,
    idle: Mutex<Vec<(Tds, Instant)>>,
    permits: Arc<Semaphore>,
    closed: AtomicBool,
}
impl Shared {
    /// Take the connections that have been unused for too long (or all of
    /// them) out of the pool. They are closed when the caller drops them.
    fn expired(&self, all: bool) -> Vec<Tds> {
        let Ok(mut idle) = self.idle.lock() else {
            return Vec::new();
        };
        let (old, fresh) = std::mem::take(&mut *idle)
            .into_iter()
            .partition(|(_, since)| all || since.elapsed() >= IDLE_TIMEOUT);
        *idle = fresh;
        old.into_iter().map(|(client, _)| client).collect()
    }
}

/// The connections of one session: at most four, opened when they are
/// needed and closed after a minute without use.
pub(super) struct Pool {
    shared: Arc<Shared>,
}

impl Pool {
    /// A pool around a first connection that is already open.
    pub(super) fn new(connector: Connector, first: Tds) -> Self {
        let shared = Arc::new(Shared {
            connector,
            idle: Mutex::new(vec![(first, Instant::now())]),
            permits: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            closed: AtomicBool::new(false),
        });
        // Close what is no longer used, until the session is gone.
        let watched = Arc::downgrade(&shared);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(IDLE_TIMEOUT / 2).await;
                let Some(shared) = watched.upgrade() else {
                    break;
                };
                if shared.closed.load(Ordering::Acquire) {
                    break;
                }
                drop(shared.expired(false));
            }
        });
        Self { shared }
    }

    /// A connection of the pool, or a new one. Waits for a free place for as
    /// long as opening a connection may take.
    pub(super) async fn acquire(&self) -> Result<Lease> {
        anyhow::ensure!(
            !self.shared.closed.load(Ordering::Acquire),
            "connection_unavailable"
        );
        let permit = tokio::time::timeout(
            CONNECT_TIMEOUT,
            Arc::clone(&self.shared.permits).acquire_owned(),
        )
        .await
        .map_err(|_| anyhow!("database_connection_timeout"))?
        .map_err(|_| anyhow!("connection_unavailable"))?;
        drop(self.shared.expired(false));
        // The connection used last is the one most likely to be alive.
        let pooled = self.shared.idle.lock().ok().and_then(|mut idle| idle.pop());
        let client = match pooled {
            Some((client, _)) => client,
            None => self.shared.connector.open().await?,
        };
        Ok(Lease {
            client,
            _permit: permit,
        })
    }

    /// Give back a connection that is ready for the next statement.
    pub(super) fn release(&self, lease: Lease) {
        if self.shared.closed.load(Ordering::Acquire) {
            return;
        }
        if let Ok(mut idle) = self.shared.idle.lock() {
            idle.push((lease.client, Instant::now()));
        }
    }

    /// Close every connection and refuse to open another.
    pub(super) async fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.permits.close();
        for client in self.shared.expired(true) {
            let _ = tokio::time::timeout(Duration::from_secs(2), client.close()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn io(kind: std::io::ErrorKind) -> Error {
        Error::Io {
            kind,
            message: "the address of a server".into(),
        }
    }

    #[test]
    fn a_refused_connection_is_named_without_the_text_of_the_server() {
        let code = |error: Error, strict: bool| connect_error(&error, strict).to_string();
        assert_eq!(
            code(
                Error::Tls(
                    "the server's certificate was rejected during the TLS handshake: UnknownIssuer"
                        .into()
                ),
                false
            ),
            "database_certificate_rejected"
        );
        // A certificate problem is one in strict mode as well.
        assert_eq!(
            code(Error::Tls("invalid peer certificate: Expired".into()), true),
            "database_certificate_rejected"
        );
        assert_eq!(
            code(Error::Tls("unexpected message".into()), false),
            "database_tls_error"
        );
        assert_eq!(
            code(
                Error::Protocol("Server does not allow the requested encryption level.".into()),
                false
            ),
            "database_tls_unsupported"
        );
        assert_eq!(
            code(io(std::io::ErrorKind::TimedOut), false),
            "database_connection_timeout"
        );
        assert_eq!(
            code(io(std::io::ErrorKind::ConnectionReset), false),
            "database_network_error"
        );
        // An older server answers a strict greeting with silence or by
        // closing the connection.
        for kind in [
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::UnexpectedEof,
        ] {
            assert_eq!(code(io(kind), true), "mssql_strict_unsupported");
        }
        assert_eq!(
            code(Error::Protocol("unexpected token".into()), false),
            "database_operation_failed"
        );
        // Nothing of the library's text, which may hold an address, is kept.
        for error in [
            io(std::io::ErrorKind::ConnectionRefused),
            Error::Tls("db.example.test".into()),
            Error::Routing {
                host: "db.example.test".into(),
                port: 11000,
            },
        ] {
            for text in [
                connect_error(&error, false).to_string(),
                statement_error(&error).to_string(),
            ] {
                assert!(
                    text.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                    "{text}"
                );
            }
        }
        assert_eq!(sign_in_code(18456), Some("database_login_rejected"));
        assert_eq!(sign_in_code(4060), Some("database_not_found"));
        assert_eq!(sign_in_code(916), Some("database_access_denied"));
        assert_eq!(sign_in_code(18488), Some("mssql_password_expired"));
        assert_eq!(sign_in_code(18486), Some("mssql_account_locked"));
        assert_eq!(sign_in_code(40615), Some("mssql_firewall_blocked"));
        assert_eq!(sign_in_code(208), None);
        assert_eq!(
            statement_error(&io(std::io::ErrorKind::BrokenPipe)).to_string(),
            "database_network_error"
        );
        assert_eq!(
            statement_error(&Error::Conversion("x".into())).to_string(),
            "database_operation_failed"
        );
    }
}
