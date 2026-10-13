//! Connections to an Oracle server: how one is opened (`Connector`), how the
//! blocking client library is driven from the runtime (`Conn`), the few a
//! session keeps (`Pool`) and what a failure is called.
//!
//! The client library does its work on the thread that calls it. Every call
//! therefore runs on a thread of the blocking pool, with the connection
//! moved there and back; what waits for it on the runtime can give up at any
//! time and tell the relay ([`super::net`]) to stop the statement. A
//! connection that was given up on is never used again.
use super::net::{Breaker, Endpoint, Link};
use super::values::SESSION_SETUP;
use crate::engine::ConnectRequest;
use anyhow::{Result, anyhow};
use oracledb::{Config, Connection, Error, ErrorKind};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use zeroize::Zeroizing;

/// Connections a session opens at most; the same number as the other drivers.
const MAX_CONNECTIONS: usize = 4;
/// How long the sign-in may take once the server was reached.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// An unused connection is closed after this long, so that one a firewall
/// has silently dropped is not handed out.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long signing off may take before the socket is simply closed.
const GOODBYE: Duration = Duration::from_secs(2);

/// Why a call of the library did not succeed.
pub(super) struct Failure {
    pub(super) error: anyhow::Error,
    /// The session is gone or in a state nobody knows: the connection is not
    /// used again.
    pub(super) broken: bool,
}
impl Failure {
    pub(super) fn code(code: &'static str) -> Self {
        Self {
            error: anyhow!(code),
            broken: false,
        }
    }
    fn lost(code: &'static str) -> Self {
        Self {
            error: anyhow!(code),
            broken: true,
        }
    }
}

/// Errors of the server after which the session no longer exists.
fn session_ended(code: usize) -> bool {
    matches!(
        code,
        28 | 1012 | 1033 | 1034 | 1089 | 1090 | 1092 | 3113 | 3114 | 3135 | 12537 | 12547 | 12570
    )
}

/// What a statement that failed is called: `database_error [ORA-00942]`,
/// never the text of the server, which can repeat a name or a value.
pub(super) fn statement_failure(error: &Error) -> Failure {
    match error.kind() {
        ErrorKind::DbError(found) => {
            let code = found.code();
            match code {
                // "User requested cancel of current operation".
                1013 => Failure::code("query_cancelled"),
                _ if session_ended(code) => Failure::lost("database_network_error"),
                _ => Failure {
                    error: anyhow!("database_error [ORA-{code:05}]"),
                    broken: false,
                },
            }
        }
        ErrorKind::ParseError(..) => Failure::code("sql_unterminated_literal"),
        ErrorKind::MissingBindValue(_) | ErrorKind::WrongNumPositionalBinds(..) => {
            Failure::code("oracle_placeholder_unsupported")
        }
        ErrorKind::UnsupportedDbType(_) => Failure::code("oracle_type_unsupported"),
        ErrorKind::CallTimeoutExceeded => Failure::lost("query_timeout"),
        ErrorKind::DeadConnection
        | ErrorKind::NotConnected
        | ErrorKind::UnableToRecover
        | ErrorKind::LockPoisoned
        | ErrorKind::StreamOperation => Failure::lost("database_network_error"),
        // Something of the protocol the library does not read: what was
        // left unread makes the next answer unreadable too.
        _ => Failure::lost("oracle_client_unsupported"),
    }
}

/// What the error number of a listener that refused a connection means.
fn refusal(number: u32) -> &'static str {
    match number {
        12514 | 12504 => "database_not_found",
        12505 => "oracle_sid_unknown",
        12516 | 12518 | 12519 | 12520 | 12523 => "oracle_listener_busy",
        12526..=12528 => "oracle_instance_restricted",
        _ => "oracle_listener_refused",
    }
}

/// What a sign-in that failed is called.
fn connect_failure(error: &Error, link: &Link) -> anyhow::Error {
    if let Some(code) = link.verdict() {
        return anyhow!(code);
    }
    if let Some(number) = link.refused() {
        return anyhow!(refusal(number));
    }
    anyhow!(match error.kind() {
        ErrorKind::DbError(found) => match found.code() {
            1017 => "database_login_rejected",
            1005 => "database_login_rejected",
            1045 | 1031 => "database_access_denied",
            28000 => "oracle_account_locked",
            28001 => "oracle_password_expired",
            28009 => "oracle_sys_needs_role",
            1033 | 1034 | 1089 | 1090 | 12528 => "oracle_instance_unavailable",
            12514 => "database_not_found",
            12505 => "oracle_sid_unknown",
            12660 => "oracle_native_encryption_required",
            code => return anyhow!("database_error [ORA-{code:05}]"),
        },
        ErrorKind::InvalidServiceName(..) => "database_not_found",
        ErrorKind::InvalidSid(..) => "oracle_sid_unknown",
        ErrorKind::ListenerRefusedConnection(..) | ErrorKind::UnexpectedRefuse(_) => {
            "oracle_listener_refused"
        }
        ErrorKind::ServerVersionNotSupported => "oracle_server_too_old",
        _ => "database_network_error",
    })
}

/// Which database of the server a profile names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Target {
    Service(String),
    Sid(String),
}

/// A name that goes into a connect descriptor as it is: nothing in it may
/// open or close a part of the descriptor.
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'$' | b'#')
        })
}

/// Everything needed to open one more connection for a profile.
pub(super) struct Connector {
    endpoint: Endpoint,
    target: Target,
    username: String,
    password: Zeroizing<String>,
    /// The role of the sign-in, as the library numbers it.
    role: u8,
}

impl Connector {
    pub(super) fn new(request: &ConnectRequest<'_>) -> Result<Self> {
        let profile = request.profile;
        anyhow::ensure!(!profile.username.trim().is_empty(), "oracle_login_required");
        let service = profile.database.trim();
        let sid = request.option("sid").trim();
        let target = match (service.is_empty(), sid.is_empty()) {
            (true, true) => anyhow::bail!("oracle_service_required"),
            (false, false) => anyhow::bail!("oracle_service_and_sid"),
            (false, true) => Target::Service(service.to_owned()),
            (true, false) => Target::Sid(sid.to_owned()),
        };
        let (Target::Service(name) | Target::Sid(name)) = &target;
        anyhow::ensure!(plain_name(name), "oracle_service_invalid");
        let role = match request.option("role") {
            "sysdba" => oracledb::AUTH_MODE_SYSDBA,
            "sysoper" => oracledb::AUTH_MODE_SYSOPER,
            _ => oracledb::AUTH_MODE_DEFAULT,
        };
        Ok(Self {
            endpoint: Endpoint::new(request)?,
            target,
            username: profile.username.trim().to_owned(),
            password: Zeroizing::new(profile.password.clone()),
            role,
        })
    }

    /// The descriptor the library is given: the address of the relay, and
    /// the service or the instance of the profile.
    fn descriptor(&self, relay: std::net::SocketAddr) -> String {
        let data = match &self.target {
            Target::Service(name) => format!("(SERVICE_NAME={name})"),
            Target::Sid(name) => format!("(SID={name})"),
        };
        format!(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=tcp)(HOST={})(PORT={}))(CONNECT_DATA={data}))",
            relay.ip(),
            relay.port()
        )
    }

    /// Reach the server, sign in and set the formats of the session.
    pub(super) async fn open(&self) -> Result<Conn> {
        let link = Link::open(&self.endpoint).await?;
        let breaker = link.breaker();
        let descriptor = self.descriptor(link.address());
        let username = self.username.clone();
        let password = self.password.clone();
        let role = self.role;
        let attempt = tokio::task::spawn_blocking(move || {
            catch_unwind(AssertUnwindSafe(|| -> Result<Connection, Error> {
                // What the server shows of this client is the name of the
                // program; the name of this computer and of its user are
                // nothing the server has to know.
                let config = Config::default()
                    .set_credentials(&username, &password)
                    .set_auth_mode(role)
                    .set_driver_name("Baza Studio")
                    .set_program("BazaStudio")?
                    .set_machine("BazaStudio")?
                    .set_osuser("BazaStudio")?
                    .set_terminal("BazaStudio")
                    .set_connect_string(&descriptor)?;
                let connection = oracledb::connect(config)?;
                connection.execute(SESSION_SETUP, &[])?;
                Ok(connection)
            }))
        });
        let outcome = match tokio::time::timeout(CONNECT_TIMEOUT, attempt).await {
            Err(_) => {
                breaker.sever();
                return Err(anyhow!("database_connection_timeout"));
            }
            Ok(Ok(Ok(outcome))) => outcome,
            // The library gave up in a way it has no error for.
            Ok(Ok(Err(_)) | Err(_)) => {
                return Err(anyhow!(
                    link.verdict().unwrap_or("oracle_client_unsupported")
                ));
            }
        };
        match outcome {
            Ok(client) => Ok(Conn {
                client: Some(client),
                link: Some(link),
                broken: false,
            }),
            Err(error) => Err(connect_failure(&error, &link)),
        }
    }
}

/// One connection: the session of the library and the relay it talks
/// through.
pub(super) struct Conn {
    /// Absent while a call runs, and for good when that call was given up.
    client: Option<Connection>,
    link: Option<Link>,
    broken: bool,
}

impl Conn {
    /// The part that stops a running statement.
    pub(super) fn breaker(&self) -> Option<Breaker> {
        self.link.as_ref().map(Link::breaker)
    }

    /// Whether the connection can take another statement.
    pub(super) fn usable(&self) -> bool {
        self.client.is_some() && self.link.is_some() && !self.broken
    }

    /// Run `job` with the session on a thread that may block. A panic of
    /// the library ends the connection and is reported like an error.
    pub(super) async fn work<T, F>(&mut self, job: F) -> Result<T, Failure>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T, Failure> + Send + 'static,
    {
        let Some(client) = self.client.take() else {
            return Err(Failure::lost("connection_unavailable"));
        };
        let handle = tokio::task::spawn_blocking(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| job(&client)));
            (client, outcome)
        });
        match handle.await {
            Ok((client, Ok(result))) => {
                self.client = Some(client);
                if let Err(failure) = &result {
                    self.broken |= failure.broken;
                }
                result
            }
            Ok((client, Err(_))) => {
                self.broken = true;
                self.client = Some(client);
                Err(Failure::lost("oracle_client_unsupported"))
            }
            Err(_) => {
                self.broken = true;
                Err(Failure::lost("oracle_client_unsupported"))
            }
        }
    }

    /// Sign off and close the socket. Never blocks the caller.
    fn dismiss(&mut self) {
        let client = self.client.take();
        let link = self.link.take();
        let broken = self.broken;
        if client.is_none() && link.is_none() {
            return;
        }
        let goodbye = move || {
            if let Some(client) = client {
                if broken {
                    // Nobody answers a sign-off there.
                    if let Some(link) = &link {
                        link.breaker().sever();
                    }
                } else {
                    let _ = client.set_call_timeout(Some(GOODBYE));
                }
                // Dropping the session signs it off, which is a round trip.
                let _ = catch_unwind(AssertUnwindSafe(move || drop(client)));
            }
            drop(link);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(goodbye);
            }
            Err(_) => {
                std::thread::spawn(goodbye);
            }
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.dismiss();
    }
}

/// A connection taken from the pool. It goes back with [`Pool::release`];
/// one that is dropped is closed, which also rolls back what it left open.
pub(super) struct Lease {
    conn: Conn,
    _permit: OwnedSemaphorePermit,
}
impl std::ops::Deref for Lease {
    type Target = Conn;
    fn deref(&self) -> &Conn {
        &self.conn
    }
}
impl std::ops::DerefMut for Lease {
    fn deref_mut(&mut self) -> &mut Conn {
        &mut self.conn
    }
}

struct Shared {
    connector: Connector,
    idle: Mutex<Vec<(Conn, Instant)>>,
    permits: Arc<Semaphore>,
    closed: AtomicBool,
}
impl Shared {
    /// Take out the connections that were unused for too long, or all of
    /// them.
    fn expired(&self, all: bool) -> Vec<Conn> {
        let Ok(mut idle) = self.idle.lock() else {
            return Vec::new();
        };
        let (old, fresh): (Vec<_>, Vec<_>) = std::mem::take(&mut *idle)
            .into_iter()
            .partition(|(_, since)| all || since.elapsed() >= IDLE_TIMEOUT);
        *idle = fresh;
        old.into_iter().map(|(conn, _)| conn).collect()
    }
}

/// The connections of one session: at most four, opened when they are
/// needed and closed after a minute without use.
pub(super) struct Pool {
    shared: Arc<Shared>,
}

impl Pool {
    /// A pool around a first connection that is already open.
    pub(super) fn new(connector: Connector, first: Conn) -> Self {
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
        let conn = match pooled {
            Some((conn, _)) => conn,
            None => self.shared.connector.open().await?,
        };
        Ok(Lease {
            conn,
            _permit: permit,
        })
    }

    /// Give back a connection. One that cannot take another statement is
    /// closed instead.
    pub(super) fn release(&self, lease: Lease) {
        if self.shared.closed.load(Ordering::Acquire) || !lease.conn.usable() {
            return;
        }
        if let Ok(mut idle) = self.shared.idle.lock() {
            idle.push((lease.conn, Instant::now()));
        }
    }

    /// Close every connection and refuse to open another.
    pub(super) fn close(&self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.permits.close();
        drop(self.shared.expired(true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ConnectionProfile, TlsMode};

    fn profile() -> ConnectionProfile {
        ConnectionProfile {
            name: "Owned fixture".into(),
            engine: super::super::ENGINE,
            host: "db.example.test".into(),
            port: 1521,
            username: "app".into(),
            tls: TlsMode::Disable,
            ..Default::default()
        }
    }

    fn connector(profile: &ConnectionProfile) -> Result<Connector> {
        profile.validate()?;
        Connector::new(&ConnectRequest {
            product: profile.product_spec(),
            profile,
            forwarded: None,
        })
    }

    fn refused(profile: &ConnectionProfile) -> String {
        connector(profile).err().expect("refused").to_string()
    }

    #[test]
    fn a_profile_names_a_service_or_an_instance() {
        let mut profile = profile();
        assert_eq!(refused(&profile), "oracle_service_required");
        profile.database = "FREEPDB1".into();
        let found = connector(&profile).unwrap();
        assert_eq!(found.target, Target::Service("FREEPDB1".into()));
        assert_eq!(
            found.descriptor("127.0.0.1:4001".parse().unwrap()),
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=tcp)(HOST=127.0.0.1)(PORT=4001))(CONNECT_DATA=(SERVICE_NAME=FREEPDB1)))"
        );
        assert_eq!(found.role, oracledb::AUTH_MODE_DEFAULT);
        profile.set_option("sid", "FREE");
        assert_eq!(refused(&profile), "oracle_service_and_sid");
        profile.database.clear();
        profile.set_option("role", "sysdba");
        let found = connector(&profile).unwrap();
        assert_eq!(found.target, Target::Sid("FREE".into()));
        assert!(
            found
                .descriptor("127.0.0.1:4001".parse().unwrap())
                .ends_with("(CONNECT_DATA=(SID=FREE)))")
        );
        assert_eq!(found.role, oracledb::AUTH_MODE_SYSDBA);
        // Nothing of a name may reach into the descriptor.
        for name in ["a)(b", "a b", "a=b", "x(", "ż"] {
            profile.set_option("sid", name);
            assert_eq!(refused(&profile), "oracle_service_invalid", "{name}");
        }
        profile.set_option("sid", "orcl.example.com");
        assert!(connector(&profile).is_ok());
        profile.username = "  ".into();
        assert_eq!(refused(&profile), "oracle_login_required");
        profile.username = "app".into();
        profile.set_option("role", "owner");
        assert_eq!(
            profile.validate().unwrap_err().to_string(),
            "connection_option_invalid"
        );
    }

    #[test]
    fn a_refusal_of_the_listener_has_a_name() {
        assert_eq!(refusal(12514), "database_not_found");
        assert_eq!(refusal(12505), "oracle_sid_unknown");
        assert_eq!(refusal(12516), "oracle_listener_busy");
        assert_eq!(refusal(12526), "oracle_instance_restricted");
        assert_eq!(refusal(12999), "oracle_listener_refused");
        assert!(session_ended(3113) && session_ended(28) && !session_ended(942));
    }
}
