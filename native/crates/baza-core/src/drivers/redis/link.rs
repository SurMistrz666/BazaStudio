//! One connection to a Redis server: the TCP or TLS stream the driver opens
//! itself (so that an SSH tunnel is dialled while the name of the server
//! stays the one of the profile), the RESP handshake of the `redis` crate on
//! top of it, and the mapping of its failures to result codes.
use crate::model::TlsMode;
use redis::aio::MultiplexedConnection;
use redis::{AsyncConnectionConfig, Cmd, ErrorKind, ProtocolVersion, RedisConnectionInfo, Value};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;

/// Longest time to open a connection: TCP, TLS and the sign-in together.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Longest a TLS handshake may take once the TCP connection is open. A
/// Redis server without TLS reads the greeting as the start of a command
/// and waits for the rest of it, so it never answers at all.
const TLS_TIMEOUT: Duration = Duration::from_secs(6);

/// Where and how to connect, taken from the request once.
#[derive(Clone)]
pub(super) struct Target {
    /// Host and port the TCP connection goes to: the server, or the loopback
    /// end of an SSH tunnel.
    pub dial_host: String,
    pub dial_port: u16,
    /// Name of the server for TLS: always the host of the profile.
    pub server_name: String,
    pub tls: TlsMode,
    pub ca_file: Option<PathBuf>,
    pub username: String,
    pub password: String,
}

impl Drop for Target {
    fn drop(&mut self) {
        use zeroize::Zeroize as _;
        self.password.zeroize();
    }
}

/// Version of the protocol a connection speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Protocol {
    Resp2,
    Resp3,
}

/// Why a command or a connection failed, before it becomes a result code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Fail {
    /// The server answered with an error: its code word (`WRONGTYPE`) and
    /// the rest of its sentence. The sentence never leaves the driver.
    Server { code: String, detail: String },
    /// The connection is gone.
    Io,
    /// The server did not answer in time.
    Timeout,
    /// A result code that is already decided.
    Code(&'static str),
}

impl Fail {
    /// The result code of the failure.
    pub(super) fn code(&self) -> &'static str {
        match self {
            Fail::Io => "database_network_error",
            Fail::Timeout => "database_connection_timeout",
            Fail::Code(code) => code,
            Fail::Server { code, detail } => server_code(code, detail),
        }
    }
    pub(super) fn is_server(&self, word: &str) -> bool {
        matches!(self, Fail::Server { code, .. } if code == word)
    }
    /// The server does not know the command, or not with these words.
    pub(super) fn is_unknown_or_syntax(&self) -> bool {
        matches!(
            self.code(),
            "redis_unknown_command" | "redis_syntax_error" | "redis_wrong_arguments"
        )
    }
}

impl From<Fail> for anyhow::Error {
    fn from(fail: Fail) -> Self {
        anyhow::anyhow!(fail.code())
    }
}

/// The result code for an error reply. The code word decides; a plain `ERR`
/// is told apart by the stable start of its sentence.
pub(super) fn server_code(code: &str, detail: &str) -> &'static str {
    let detail = detail.to_ascii_lowercase();
    let has = |needle: &str| detail.contains(needle);
    match code {
        "WRONGTYPE" => "redis_wrong_type",
        "MOVED" | "ASK" => "redis_cluster_moved",
        "CLUSTERDOWN" => "redis_cluster_down",
        "CROSSSLOT" => "redis_cluster_cross_slot",
        "TRYAGAIN" | "MASTERDOWN" | "LOADING" => "redis_server_not_ready",
        "READONLY" => "redis_read_only_replica",
        "NOAUTH" | "WRONGPASS" => "database_login_rejected",
        "NOPERM" => "redis_no_permission",
        "BUSY" => "redis_busy_script",
        "NOSCRIPT" => "redis_no_script",
        "OOM" => "redis_out_of_memory",
        "MISCONF" => "redis_misconfigured",
        "EXECABORT" => "redis_exec_aborted",
        "NOGROUP" => "redis_no_group",
        "BUSYGROUP" => "redis_group_exists",
        "NOREPLICAS" => "redis_no_replicas",
        "NOPROTO" => "redis_protocol_unsupported",
        "UNKILLABLE" | "NOTBUSY" => "redis_script_not_stopped",
        _ if has("unknown command") || has("unknown subcommand") => "redis_unknown_command",
        _ if has("wrong number of arguments") => "redis_wrong_arguments",
        _ if has("db index is out of range") || has("invalid db index") => {
            "redis_database_index_invalid"
        }
        _ if has("select is not allowed in cluster mode") => "redis_database_index_invalid",
        _ if has("not an integer") || has("out of range") && has("integer") => {
            "redis_not_an_integer"
        }
        _ if has("not a valid float") || has("not a float") => "redis_not_a_float",
        _ if has("no such key") => "redis_no_such_key",
        _ if has("index out of range") => "redis_index_out_of_range",
        _ if has("exec without multi") || has("discard without multi") => {
            "redis_exec_without_multi"
        }
        _ if has("multi calls can not be nested") => "redis_multi_nested",
        _ if has("equal or smaller than the target stream top item")
            || has("invalid stream id") =>
        {
            "redis_stream_id_invalid"
        }
        _ if has("invalid password")
            || has("invalid username-password")
            || has("without any password configured")
            || has("no password is set") =>
        {
            "database_login_rejected"
        }
        _ if has("syntax error") => "redis_syntax_error",
        _ if has("invalid expire time") || has("invalid ttl") => "redis_ttl_invalid",
        _ if has("no such file") || has("permission denied") => "redis_command_failed",
        _ => "redis_command_failed",
    }
}

fn fail_of(error: redis::RedisError) -> Fail {
    if let Some(code) = error.code() {
        // An error reply of the server.
        if !matches!(
            error.kind(),
            ErrorKind::Io | ErrorKind::Client | ErrorKind::Parse
        ) {
            return Fail::Server {
                code: code.to_owned(),
                detail: error.detail().unwrap_or_default().to_owned(),
            };
        }
    }
    match error.kind() {
        ErrorKind::AuthenticationFailed => Fail::Code("database_login_rejected"),
        ErrorKind::RESP3NotSupported => Fail::Code("redis_protocol_unsupported"),
        ErrorKind::Server(_) | ErrorKind::Extension => Fail::Server {
            code: error.code().unwrap_or("ERR").to_owned(),
            detail: error.detail().unwrap_or_default().to_owned(),
        },
        _ if error.is_timeout() => Fail::Timeout,
        _ => Fail::Io,
    }
}

/// An error value inside a pipeline or a transaction as a failure.
pub(super) fn value_fail(error: &redis::ServerError) -> Fail {
    Fail::Server {
        code: error.code().to_owned(),
        detail: error.details().unwrap_or_default().to_owned(),
    }
}

/// A command from its words, each of them bytes.
pub(super) fn command<I, A>(words: I) -> Cmd
where
    I: IntoIterator<Item = A>,
    A: AsRef<[u8]>,
{
    let mut cmd = Cmd::new();
    for word in words {
        cmd.arg(word.as_ref());
    }
    cmd
}

/// Accepts whatever certificate the server shows: TLS mode "encryption
/// required", which encrypts without verifying who answers.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);
impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// The TLS settings of a target: the bundled Mozilla roots, or the CA file
/// of the profile, or no verification at all.
fn tls_config(target: &Target) -> Result<rustls::ClientConfig, Fail> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|_| Fail::Code("database_tls_error"))?;
    if target.tls == TlsMode::Require {
        return Ok(builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth());
    }
    let mut roots = rustls::RootCertStore::empty();
    match &target.ca_file {
        Some(path) => {
            let pem = std::fs::read(path).map_err(|_| Fail::Code("database_tls_error"))?;
            for certificate in CertificateDer::pem_slice_iter(&pem) {
                let certificate = certificate.map_err(|_| Fail::Code("database_tls_error"))?;
                roots
                    .add(certificate)
                    .map_err(|_| Fail::Code("database_tls_error"))?;
            }
            if roots.is_empty() {
                return Err(Fail::Code("database_tls_error"));
            }
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    Ok(builder.with_root_certificates(roots).with_no_client_auth())
}

/// Why a TLS handshake failed, as a result code.
fn tls_fail(error: &std::io::Error) -> Fail {
    let text = error.to_string().to_ascii_lowercase();
    if text.contains("certificate") || text.contains("unknownissuer") || text.contains("notvalid") {
        Fail::Code("database_certificate_rejected")
    } else if text.contains("invalidcontenttype")
        || text.contains("corrupt message")
        || text.contains("invalid content type")
        || error.kind() == std::io::ErrorKind::UnexpectedEof
    {
        // The server answered a TLS greeting with something else: it does
        // not speak TLS on this port.
        Fail::Code("database_tls_unsupported")
    } else {
        Fail::Code("database_tls_error")
    }
}

/// An open connection. Dropping it closes the socket.
pub(super) struct Link {
    connection: MultiplexedConnection,
    /// The task that moves bytes between the socket and the connection.
    /// Ending it closes the socket at once, whatever the server is doing.
    driver: tokio::task::JoinHandle<()>,
    /// What `CLIENT ID` answered: how another connection names this one.
    pub id: Option<i64>,
}

impl Drop for Link {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

impl Link {
    /// Connect, sign in, choose the database and name the client.
    pub(super) async fn open(target: &Target, db: i64, protocol: Protocol) -> Result<Link, Fail> {
        match tokio::time::timeout(CONNECT_TIMEOUT, Self::open_now(target, db, protocol)).await {
            Ok(result) => result,
            Err(_) => Err(Fail::Timeout),
        }
    }

    async fn open_now(target: &Target, db: i64, protocol: Protocol) -> Result<Link, Fail> {
        let tcp = TcpStream::connect((target.dial_host.as_str(), target.dial_port))
            .await
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::TimedOut => Fail::Timeout,
                _ => Fail::Io,
            })?;
        let _ = tcp.set_nodelay(true);
        let mut info = RedisConnectionInfo::default()
            .set_db(db)
            .set_protocol(match protocol {
                Protocol::Resp2 => ProtocolVersion::RESP2,
                Protocol::Resp3 => ProtocolVersion::RESP3,
            })
            .set_lib_name("baza-studio", env!("CARGO_PKG_VERSION"));
        if !target.username.is_empty() {
            info = info.set_username(&target.username);
        }
        if !target.password.is_empty() || !target.username.is_empty() {
            info = info.set_password(&target.password);
        }
        // The driver keeps its own deadlines: a command of the console may
        // wait for as long as the person lets it.
        let config = AsyncConnectionConfig::new()
            .set_response_timeout(None)
            .set_connection_timeout(None);
        let (connection, driver) = if target.tls == TlsMode::Disable {
            let (connection, driver) = MultiplexedConnection::new_with_config(&info, tcp, config)
                .await
                .map_err(|error| match fail_of(error) {
                    // A server that only accepts TLS closes a plain
                    // connection or answers with something that is not RESP.
                    Fail::Io => Fail::Code("redis_handshake_failed"),
                    other => other,
                })?;
            (connection, tokio::spawn(driver))
        } else {
            let config_tls = tls_config(target)?;
            let name = ServerName::try_from(target.server_name.clone())
                .map_err(|_| Fail::Code("database_tls_error"))?;
            let handshake =
                tokio_rustls::TlsConnector::from(Arc::new(config_tls)).connect(name, tcp);
            let stream = match tokio::time::timeout(TLS_TIMEOUT, handshake).await {
                Ok(stream) => stream.map_err(|error| tls_fail(&error))?,
                Err(_) => return Err(Fail::Code("database_tls_unsupported")),
            };
            let (connection, driver) =
                MultiplexedConnection::new_with_config(&info, stream, config)
                    .await
                    .map_err(fail_of)?;
            (connection, tokio::spawn(driver))
        };
        let mut link = Link {
            connection,
            driver,
            id: None,
        };
        // A server with a password answers the first command of a
        // connection that sent none with NOAUTH: say so now, not later.
        match link.call(&command(["PING"])).await {
            Ok(_) => {}
            // PING itself may be forbidden to this account: the sign-in
            // worked all the same.
            Err(fail) if fail.code() == "redis_no_permission" => {}
            Err(fail) => return Err(fail),
        }
        if let Ok(Value::Int(id)) = link.call(&command(["CLIENT", "ID"])).await {
            link.id = Some(id);
        }
        let _ = link
            .call(&command(["CLIENT", "SETNAME", "baza-studio"]))
            .await;
        Ok(link)
    }

    /// Whether the socket is still open.
    pub(super) fn alive(&self) -> bool {
        !self.driver.is_finished()
    }

    /// Close the socket now. Commands that wait for an answer fail.
    pub(super) fn close(&self) {
        self.driver.abort();
    }

    /// Send one command and wait for its reply.
    pub(super) async fn call(&self, cmd: &Cmd) -> Result<Value, Fail> {
        let mut connection = self.connection.clone();
        match connection.send_packed_command(cmd).await {
            Ok(Value::ServerError(error)) => Err(value_fail(&error)),
            Ok(value) => Ok(value),
            Err(error) => Err(fail_of(error)),
        }
    }

    /// Send commands in one write and read all their replies. A command the
    /// server refused has a `Value::ServerError` in its place.
    pub(super) async fn pipe(&self, commands: Vec<Cmd>) -> Result<Vec<Value>, Fail> {
        if commands.is_empty() {
            return Ok(Vec::new());
        }
        let mut replies = Vec::with_capacity(commands.len());
        // A bounded batch keeps the request and the answer of one write small.
        for batch in commands.chunks(2000) {
            let mut pipeline = redis::Pipeline::with_capacity(batch.len());
            pipeline.ignore_errors();
            for cmd in batch {
                pipeline.add_command(cmd.clone());
            }
            let mut connection = self.connection.clone();
            let mut part: Vec<Value> = pipeline
                .query_async(&mut connection)
                .await
                .map_err(fail_of)?;
            if part.len() != batch.len() {
                return Err(Fail::Io);
            }
            replies.append(&mut part);
        }
        Ok(replies)
    }

    /// Run commands as one transaction (`MULTI` … `EXEC`). `None` when a
    /// watched key was changed by somebody else and nothing was done.
    pub(super) async fn atomic(&self, commands: Vec<Cmd>) -> Result<Option<Vec<Value>>, Fail> {
        let mut pipeline = redis::Pipeline::with_capacity(commands.len());
        pipeline.atomic().ignore_errors();
        for cmd in commands {
            pipeline.add_command(cmd);
        }
        let mut connection = self.connection.clone();
        match pipeline.query_async::<Value>(&mut connection).await {
            Ok(Value::Nil) => Ok(None),
            Ok(Value::Array(replies)) => Ok(Some(replies)),
            Ok(_) => Err(Fail::Io),
            Err(error) => Err(fail_of(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_replies_become_stable_codes() {
        for (code, detail, expected) in [
            (
                "WRONGTYPE",
                "Operation against a key holding the wrong kind of value",
                "redis_wrong_type",
            ),
            ("MOVED", "3999 10.0.0.5:6381", "redis_cluster_moved"),
            ("ASK", "3999 10.0.0.5:6381", "redis_cluster_moved"),
            (
                "NOAUTH",
                "Authentication required.",
                "database_login_rejected",
            ),
            (
                "WRONGPASS",
                "invalid username-password pair or user is disabled.",
                "database_login_rejected",
            ),
            (
                "NOPERM",
                "User app has no permissions to run the 'flushall' command",
                "redis_no_permission",
            ),
            (
                "READONLY",
                "You can't write against a read only replica.",
                "redis_read_only_replica",
            ),
            (
                "ERR",
                "unknown command 'FOO', with args beginning with: 'secret'",
                "redis_unknown_command",
            ),
            (
                "ERR",
                "wrong number of arguments for 'set' command",
                "redis_wrong_arguments",
            ),
            ("ERR", "syntax error", "redis_syntax_error"),
            (
                "ERR",
                "value is not an integer or out of range",
                "redis_not_an_integer",
            ),
            ("ERR", "value is not a valid float", "redis_not_a_float"),
            ("ERR", "no such key", "redis_no_such_key"),
            ("ERR", "index out of range", "redis_index_out_of_range"),
            (
                "ERR",
                "DB index is out of range",
                "redis_database_index_invalid",
            ),
            (
                "ERR",
                "SELECT is not allowed in cluster mode",
                "redis_database_index_invalid",
            ),
            ("ERR", "EXEC without MULTI", "redis_exec_without_multi"),
            ("ERR", "MULTI calls can not be nested", "redis_multi_nested"),
            (
                "ERR",
                "The ID specified in XADD is equal or smaller than the target stream top item",
                "redis_stream_id_invalid",
            ),
            (
                "ERR",
                "AUTH <password> called without any password configured for the default user. Are you sure your configuration is correct?",
                "database_login_rejected",
            ),
            ("ERR", "something a module said", "redis_command_failed"),
            (
                "EXECABORT",
                "Transaction discarded because of previous errors.",
                "redis_exec_aborted",
            ),
            ("SOMEMODULE", "its own words", "redis_command_failed"),
        ] {
            assert_eq!(server_code(code, detail), expected, "{code} {detail}");
        }
        // The sentence of the server never becomes part of the code.
        let fail = Fail::Server {
            code: "MOVED".into(),
            detail: "3999 10.0.0.5:6381".into(),
        };
        assert_eq!(anyhow::Error::from(fail).to_string(), "redis_cluster_moved");
    }

    #[test]
    fn a_command_keeps_the_bytes_of_its_words() {
        let cmd = command([b"SET".as_slice(), &[0xff, 0x00, b' '], b"v"]);
        assert_eq!(
            cmd.get_packed_command(),
            b"*3\r\n$3\r\nSET\r\n$3\r\n\xff\x00 \r\n$1\r\nv\r\n"
        );
    }

    #[test]
    fn the_ca_file_of_the_profile_replaces_the_bundled_roots() {
        let target = |tls, ca_file| Target {
            dial_host: "127.0.0.1".into(),
            dial_port: 6379,
            server_name: "cache.example.test".into(),
            tls,
            ca_file,
            username: String::new(),
            password: String::new(),
        };
        assert!(tls_config(&target(TlsMode::VerifyFull, None)).is_ok());
        assert!(tls_config(&target(TlsMode::Require, None)).is_ok());
        // A file that holds no certificate is an error, not "trust nothing".
        let directory = tempfile::tempdir().unwrap();
        let empty = directory.path().join("empty.pem");
        std::fs::write(&empty, "not a certificate").unwrap();
        assert_eq!(
            tls_config(&target(TlsMode::VerifyFull, Some(empty))).unwrap_err(),
            Fail::Code("database_tls_error")
        );
        let missing = directory.path().join("missing.pem");
        assert!(tls_config(&target(TlsMode::VerifyFull, Some(missing))).is_err());
    }
}
