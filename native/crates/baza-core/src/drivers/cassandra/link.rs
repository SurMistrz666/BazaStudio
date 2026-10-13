//! Opening a session of the CQL driver the way this application needs it,
//! and turning its failures into result codes.
//!
//! The driver is made to talk to exactly one node: the one the profile
//! names. A CQL client normally learns the other nodes from `system.peers`
//! and dials every one of them at the address the node itself reports.
//! Behind an SSH tunnel, a port mapping or NAT those addresses cannot be
//! reached, so every address is translated to the one that was dialled
//! ([`Pin`] as an address translator) and only the node that answers there
//! is used ([`Pin`] as a host filter). That node coordinates the requests
//! for the whole cluster, as it does for `cqlsh`.
use crate::model::TlsMode;
use anyhow::{Result, anyhow, bail};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use scylla::client::PoolSize;
use scylla::client::execution_profile::ExecutionProfile;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::cluster::metadata::Peer;
use scylla::errors::{
    DbError, ExecutionError, NewSessionError, PrepareError, RequestAttemptError, TranslationError,
    UseKeyspaceError,
};
use scylla::policies::address_translator::{AddressTranslator, UntranslatedPeer};
use scylla::policies::host_filter::HostFilter;
use scylla::statement::{Consistency, SerialConsistency};
use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::pin::Pin as BoxPin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

/// Longest time to open a session: DNS, TCP, TLS, sign-in and the first
/// look at the node.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Longest one TCP connection may take to open.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest the driver waits for one request. The server ends a read or a
/// write earlier by its own limits; this is for a server that went quiet.
pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Where and how to connect, taken from the request once.
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
    /// The keyspace of the profile, when it names one.
    pub keyspace: String,
    pub consistency: Consistency,
}

impl Drop for Target {
    fn drop(&mut self) {
        use zeroize::Zeroize as _;
        self.password.zeroize();
    }
}

/// The consistency level of an option value.
pub(super) fn consistency(name: &str) -> Option<Consistency> {
    Some(match name {
        "local_quorum" => Consistency::LocalQuorum,
        "quorum" => Consistency::Quorum,
        "local_one" => Consistency::LocalOne,
        "one" => Consistency::One,
        "all" => Consistency::All,
        "each_quorum" => Consistency::EachQuorum,
        _ => return None,
    })
}

/// Keeps the driver on the one address that was dialled.
struct Pin {
    dial: SocketAddr,
    /// The node the connections belong to: the first one the driver asks
    /// about until the node itself has said who it is.
    node: Mutex<Option<Uuid>>,
}

impl AddressTranslator for Pin {
    // The trait is written with `async_trait`; this is what the macro
    // makes of `async fn translate_address`.
    fn translate_address<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _untranslated_peer: &'life1 UntranslatedPeer<'_>,
    ) -> BoxPin<Box<dyn Future<Output = Result<SocketAddr, TranslationError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        let dial = self.dial;
        Box::pin(async move { Ok(dial) })
    }
}

impl HostFilter for Pin {
    fn accept(&self, peer: &Peer) -> bool {
        let Ok(mut node) = self.node.lock() else {
            return false;
        };
        match *node {
            Some(chosen) => chosen == peer.host_id,
            None => {
                *node = Some(peer.host_id);
                true
            }
        }
    }
}

/// Verifies a certificate against the name of the profile. The driver asks
/// for the IP address it dialled, which behind a tunnel is the loopback
/// address and elsewhere is rarely what a certificate was issued for.
#[derive(Debug)]
struct NamedVerifier {
    inner: Arc<WebPkiServerVerifier>,
    name: ServerName<'static>,
}

impl ServerCertVerifier for NamedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _dialled: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.inner
            .verify_server_cert(end_entity, intermediates, &self.name, ocsp_response, now)
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
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
fn tls_config(target: &Target) -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|_| anyhow!("database_tls_error"))?;
    if target.tls == TlsMode::Require {
        return Ok(builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth());
    }
    let mut roots = rustls::RootCertStore::empty();
    match &target.ca_file {
        Some(path) => {
            let pem = std::fs::read(path).map_err(|_| anyhow!("database_tls_error"))?;
            for certificate in CertificateDer::pem_slice_iter(&pem) {
                let certificate = certificate.map_err(|_| anyhow!("database_tls_error"))?;
                roots
                    .add(certificate)
                    .map_err(|_| anyhow!("database_tls_error"))?;
            }
            if roots.is_empty() {
                bail!("database_tls_error");
            }
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    let name = ServerName::try_from(target.server_name.clone())
        .map_err(|_| anyhow!("database_certificate_rejected"))?;
    let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
        .build()
        .map_err(|_| anyhow!("database_tls_error"))?;
    Ok(builder
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NamedVerifier { inner, name }))
        .with_no_client_auth())
}

/// Connect, sign in and settle on the node that answers.
pub(super) async fn open(target: &Target) -> Result<Session> {
    match tokio::time::timeout(CONNECT_TIMEOUT, open_now(target)).await {
        Ok(result) => result,
        Err(_) => bail!("database_connection_timeout"),
    }
}

async fn open_now(target: &Target) -> Result<Session> {
    let dial = tokio::net::lookup_host((target.dial_host.as_str(), target.dial_port))
        .await
        .map_err(|_| anyhow!("database_network_error"))?
        .next()
        .ok_or_else(|| anyhow!("database_network_error"))?;
    let pin = Arc::new(Pin {
        dial,
        node: Mutex::new(None),
    });
    let profile = ExecutionProfile::builder()
        .consistency(target.consistency)
        .serial_consistency(Some(SerialConsistency::LocalSerial))
        .request_timeout(Some(REQUEST_TIMEOUT))
        .build();
    let mut builder = SessionBuilder::new()
        .known_node_addr(dial)
        .address_translator(pin.clone())
        .host_filter(pin.clone())
        .connection_timeout(DIAL_TIMEOUT)
        // The schema is read by this driver when it is asked for, so that a
        // service without some schema table still connects.
        .fetch_schema_metadata(false)
        // Two connections to the one node; a tunnel forwards a single port,
        // so the per-shard port of ScyllaDB is left alone.
        .pool_size(PoolSize::PerHost(NonZeroUsize::MIN.saturating_add(1)))
        .disallow_shard_aware_port(true)
        .tcp_nodelay(true)
        .keepalive_interval(Duration::from_secs(30))
        .keepalive_timeout(Duration::from_secs(30))
        .default_execution_profile_handle(profile.into_handle());
    if !target.username.is_empty() {
        builder = builder.user(target.username.clone(), target.password.clone());
    }
    if target.tls != TlsMode::Disable {
        builder = builder.tls_context(Some(Arc::new(tls_config(target)?)));
    }
    let session = builder
        .build()
        .await
        .map_err(|error| anyhow!(session_code(&error)))?;
    // Who answers at this address? Until now the driver took the first node
    // it heard of; from here on it is the node itself.
    let local = session
        .query_unpaged("SELECT host_id FROM system.local", &[])
        .await
        .map_err(|error| anyhow!(execution_code(&error)))?
        .into_rows_result()
        .ok()
        .and_then(|rows| rows.maybe_first_row::<(Uuid,)>().ok().flatten())
        .map(|(host_id,)| host_id);
    if let Some(host_id) = local {
        let changed = pin
            .node
            .lock()
            .map(|mut node| node.replace(host_id) != Some(host_id))
            .unwrap_or(false);
        if changed {
            session
                .refresh_metadata()
                .await
                .map_err(|_| anyhow!("database_network_error"))?;
        }
    }
    if !target.keyspace.is_empty() {
        session
            .use_keyspace(target.keyspace.clone(), true)
            .await
            .map_err(|error| anyhow!(use_keyspace_code(&error)))?;
    }
    Ok(session)
}

/// The result code for what a failed connection attempt was, read from the
/// names of the driver's nested error types. The text never leaves here.
fn connection_code(debug: &str) -> &'static str {
    let has = |needle: &str| debug.contains(needle);
    if has("AuthenticationError")
        || has("MissingAuthentication")
        || has("StartAuthSessionError")
        || has("AuthChallengeEvaluationError")
        || has("AuthFinishError")
    {
        "database_login_rejected"
    } else if has("Unauthorized") {
        "database_access_denied"
    } else if has("InvalidCertificate")
        || has("UnknownIssuer")
        || has("NotValidForName")
        || has("BadSignature")
        || has("CertExpired")
    {
        "database_certificate_rejected"
    } else if has("InvalidContentType")
        || has("InvalidMessage")
        || has("CorruptMessage")
        || has("AlertReceived")
        || has("PeerIncompatible")
    {
        // A TLS greeting was answered with something else.
        "database_tls_unsupported"
    } else if has("ConnectTimeout") || has("TimedOut") {
        "database_connection_timeout"
    } else if has("ProtocolError") || has("FrameHeaderParseError") || has("UnexpectedResponse") {
        "cassandra_handshake_failed"
    } else {
        "database_network_error"
    }
}

fn session_code(error: &NewSessionError) -> &'static str {
    match error {
        NewSessionError::UseKeyspaceError(error) => use_keyspace_code(error),
        NewSessionError::FailedToResolveAnyHostname(_) | NewSessionError::EmptyKnownNodesList => {
            "database_network_error"
        }
        other => connection_code(&format!("{other:?}")),
    }
}

fn use_keyspace_code(error: &UseKeyspaceError) -> &'static str {
    match error {
        UseKeyspaceError::BadKeyspaceName(_) | UseKeyspaceError::KeyspaceNameMismatch { .. } => {
            "database_not_found"
        }
        UseKeyspaceError::RequestError(RequestAttemptError::DbError(DbError::Invalid, _)) => {
            "database_not_found"
        }
        UseKeyspaceError::RequestError(attempt) => attempt_code(attempt),
        UseKeyspaceError::RequestTimeout(_) => "database_connection_timeout",
        _ => "database_network_error",
    }
}

/// The result code for an error the server answered a statement with. The
/// class of the error decides; within "invalid" the stable words of the
/// server's sentence tell the common cases apart. The sentence itself can
/// repeat names and values, so it never leaves the driver.
pub(super) fn db_code(error: &DbError, message: &str) -> &'static str {
    let message = message.to_ascii_lowercase();
    let has = |needle: &str| message.contains(needle);
    match error {
        DbError::SyntaxError => "cassandra_syntax_error",
        DbError::Invalid => {
            if has("disabled") || has("not enabled") {
                // "Materialized views are disabled. Enable in cassandra.yaml".
                "cassandra_disabled_by_configuration"
            } else if has("allow filtering") {
                "cassandra_filtering_required"
            } else if has("order by") {
                "cassandra_order_restricted"
            } else if has("no keyspace has been specified") {
                "cassandra_keyspace_required"
            } else if has("unconfigured table")
                || has("unconfigured columnfamily")
                || (has("table") && has("does not exist"))
            {
                "cassandra_table_unknown"
            } else if has("keyspace") && (has("does not exist") || has("doesn't exist")) {
                "database_not_found"
            } else if has("undefined column name")
                || has("unknown identifier")
                || has("unrecognized name")
                || has("undefined name")
            {
                "cassandra_column_unknown"
            } else if has("batch too large") {
                "cassandra_batch_too_large"
            } else if has("counter") {
                "cassandra_counter_rule"
            } else if has("primary key") || has("partition key") || has("clustering") {
                "cassandra_key_rule"
            } else if has("conditional") || has("lwt") || has("serial") {
                "cassandra_condition_unsupported"
            } else {
                "cassandra_invalid_query"
            }
        }
        DbError::AlreadyExists { .. } => "cassandra_already_exists",
        DbError::FunctionFailure { .. } => "cassandra_function_failed",
        DbError::AuthenticationError => "database_login_rejected",
        DbError::Unauthorized => "database_access_denied",
        DbError::ConfigError => "cassandra_disabled_by_configuration",
        DbError::Unavailable { .. } => "cassandra_unavailable",
        DbError::Overloaded | DbError::IsBootstrapping | DbError::RateLimitReached { .. } => {
            "cassandra_overloaded"
        }
        DbError::ReadTimeout { .. } | DbError::ReadFailure { .. } => "cassandra_read_timeout",
        DbError::WriteTimeout { .. } | DbError::WriteFailure { .. } => "cassandra_write_timeout",
        DbError::TruncateError => "cassandra_write_timeout",
        _ => "cassandra_server_error",
    }
}

fn attempt_code(error: &RequestAttemptError) -> &'static str {
    match error {
        RequestAttemptError::DbError(error, message) => db_code(error, message),
        RequestAttemptError::BrokenConnectionError(_)
        | RequestAttemptError::UnableToAllocStreamId => "database_network_error",
        RequestAttemptError::SerializationError(_)
        | RequestAttemptError::CqlRequestSerialization(_) => "cassandra_invalid_query",
        _ => "cassandra_server_error",
    }
}

/// The result code for a statement that failed.
pub(super) fn execution_code(error: &ExecutionError) -> &'static str {
    match error {
        ExecutionError::LastAttemptError(attempt) => attempt_code(attempt),
        ExecutionError::PrepareError(PrepareError::AllAttemptsFailed { first_attempt }) => {
            attempt_code(first_attempt)
        }
        ExecutionError::RequestTimeout(_) => "query_timeout",
        ExecutionError::BadQuery(_) => "cassandra_invalid_query",
        ExecutionError::UseKeyspaceError(error) => use_keyspace_code(error),
        ExecutionError::SchemaAgreementError(_) => "cassandra_schema_disagreement",
        ExecutionError::ConnectionPoolError(_) | ExecutionError::EmptyPlan => {
            "database_network_error"
        }
        other => connection_code(&format!("{other:?}")),
    }
}

/// A failed statement as the error the application shows.
pub(super) fn failed(error: ExecutionError) -> anyhow::Error {
    anyhow!(execution_code(&error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invalid_statement_is_told_apart_by_what_the_server_objects_to() {
        let invalid = |message: &str| db_code(&DbError::Invalid, message);
        assert_eq!(
            invalid(
                "Cannot execute this query as it might involve data filtering and thus may have unpredictable performance. If you want to execute this query despite the performance unpredictability, use ALLOW FILTERING"
            ),
            "cassandra_filtering_required"
        );
        assert_eq!(
            invalid(
                "ORDER BY is only supported when the partition key is restricted by an EQ or an IN."
            ),
            "cassandra_order_restricted"
        );
        assert_eq!(
            invalid(
                "No keyspace has been specified. USE a keyspace, or explicitly specify keyspace.tablename"
            ),
            "cassandra_keyspace_required"
        );
        assert_eq!(
            invalid("unconfigured table nope"),
            "cassandra_table_unknown"
        );
        assert_eq!(
            invalid("table ks.nope does not exist"),
            "cassandra_table_unknown"
        );
        assert_eq!(
            invalid("Keyspace 'nope' does not exist"),
            "database_not_found"
        );
        assert_eq!(
            invalid("Undefined column name x in table ks.t"),
            "cassandra_column_unknown"
        );
        assert_eq!(invalid("Batch too large"), "cassandra_batch_too_large");
        assert_eq!(
            invalid("Cannot mix counter and non counter columns in the same table"),
            "cassandra_counter_rule"
        );
        assert_eq!(
            invalid("PRIMARY KEY part id found in SET part"),
            "cassandra_key_rule"
        );
        assert_eq!(
            invalid("Materialized views are disabled. Enable in cassandra.yaml to use."),
            "cassandra_disabled_by_configuration"
        );
        assert_eq!(invalid("anything else"), "cassandra_invalid_query");
        assert_eq!(
            db_code(&DbError::SyntaxError, "line 1:7"),
            "cassandra_syntax_error"
        );
        assert_eq!(
            db_code(&DbError::Unauthorized, ""),
            "database_access_denied"
        );
        assert_eq!(
            db_code(&DbError::AuthenticationError, ""),
            "database_login_rejected"
        );
        assert_eq!(
            db_code(&DbError::ConfigError, "Materialized views are disabled"),
            "cassandra_disabled_by_configuration"
        );
        assert_eq!(db_code(&DbError::Overloaded, ""), "cassandra_overloaded");
        assert_eq!(db_code(&DbError::ServerError, ""), "cassandra_server_error");
    }

    #[test]
    fn a_failed_connection_is_named_by_its_cause() {
        assert_eq!(
            connection_code(
                "MetadataError(ConnectionPoolError(Broken { last_connection_error: ConnectionSetupRequestError(ConnectionSetupRequestError { request_kind: AuthResponse, error: DbError(AuthenticationError, \"x\") }) }))"
            ),
            "database_login_rejected"
        );
        assert_eq!(
            connection_code(
                "ConnectionSetupRequestError { request_kind: Startup, error: MissingAuthentication }"
            ),
            "database_login_rejected"
        );
        assert_eq!(
            connection_code(
                "IoError(Custom { kind: InvalidData, error: InvalidCertificate(UnknownIssuer) })"
            ),
            "database_certificate_rejected"
        );
        assert_eq!(
            connection_code(
                "IoError(Custom { kind: InvalidData, error: InvalidMessage(InvalidContentType) })"
            ),
            "database_tls_unsupported"
        );
        assert_eq!(
            connection_code("Broken { last_connection_error: ConnectTimeout }"),
            "database_connection_timeout"
        );
        assert_eq!(
            connection_code(
                "IoError(Os { code: 61, kind: ConnectionRefused, message: \"Connection refused\" })"
            ),
            "database_network_error"
        );
    }

    #[test]
    fn consistency_levels_are_read_from_their_option_names() {
        assert_eq!(consistency("local_quorum"), Some(Consistency::LocalQuorum));
        assert_eq!(consistency("one"), Some(Consistency::One));
        assert_eq!(consistency("serial"), None);
    }
}
