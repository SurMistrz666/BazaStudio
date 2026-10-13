//! The REST API over HTTP: where a request goes, how it is encrypted, how
//! an account signs in, how much of an answer is read and what a refusal
//! of the server is called.
//!
//! The address that is dialled (the server, or the loopback end of an SSH
//! tunnel) and the name of the server (for the `Host` line, for SNI and for
//! the certificate) are two separate things here: the URL keeps the name
//! and the client is told where that name lives.
use crate::engine::ConnectRequest;
use crate::model::TlsMode;
use anyhow::{Result, anyhow, bail, ensure};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HOST, HeaderMap, HeaderValue, USER_AGENT};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde_json::Value;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Requests a session sends at the same time at most.
const MAX_REQUESTS: usize = 6;
/// How long opening the socket and the TLS handshake may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A connection that was not used for this long is not used again: load
/// balancers in front of a cluster drop idle ones after a minute or less.
const IDLE_TIMEOUT: Duration = Duration::from_secs(20);

/// What the Host field of a profile means: a name or an address, or a whole
/// URL as a cloud console shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Endpoint {
    pub host: String,
    pub port: u16,
    pub encrypted: bool,
    /// Path in front of every request: empty, or `/prefix` without a slash
    /// at its end.
    pub prefix: String,
}

/// A path prefix as it is put in front of every request.
fn prefix(text: &str) -> Result<String> {
    let text = text.trim().trim_matches('/');
    ensure!(
        text.len() <= 200
            && text.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '/' | '%')
            })
            && !text
                .split('/')
                .any(|part| part.is_empty() || part == ".." || part == "."),
        "elasticsearch_endpoint_invalid"
    );
    Ok(format!("/{text}"))
}

impl Endpoint {
    /// Work out the endpoint of a profile before anything is dialled.
    pub(super) fn of(
        host: &str,
        port: u16,
        tls: TlsMode,
        prefix_option: &str,
        tunnelled: bool,
    ) -> Result<Endpoint> {
        let invalid = || anyhow!("elasticsearch_endpoint_invalid");
        let host = host.trim();
        let encrypted = tls != TlsMode::Disable;
        let mut endpoint = Endpoint {
            host: host.trim_end_matches('/').to_owned(),
            port,
            encrypted,
            prefix: String::new(),
        };
        if let Some((scheme, rest)) = host.split_once("://") {
            // The tunnel was opened to what the Host field holds, so only a
            // name or an address can be reached through one.
            ensure!(!tunnelled, "elasticsearch_endpoint_url_tunnel");
            let https = match scheme.to_ascii_lowercase().as_str() {
                "https" => true,
                "http" => false,
                _ => return Err(invalid()),
            };
            ensure!(https == encrypted, "elasticsearch_endpoint_tls_mismatch");
            let (authority, path) = match rest.find('/') {
                Some(at) => rest.split_at(at),
                None => (rest, ""),
            };
            // An account in an address would be a secret in a plain field.
            ensure!(
                !authority.is_empty()
                    && !authority.contains('@')
                    && !path.contains(['?', '#'])
                    && !rest.contains(char::is_whitespace),
                "elasticsearch_endpoint_invalid"
            );
            let (name, own_port) = match authority.rfind(':') {
                Some(at) if !authority[at..].contains(']') => {
                    let port = authority[at + 1..].parse::<u16>().map_err(|_| invalid())?;
                    ensure!(port > 0, "elasticsearch_endpoint_invalid");
                    (&authority[..at], Some(port))
                }
                _ => (authority, None),
            };
            endpoint.host = name
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned();
            if let Some(port) = own_port {
                endpoint.port = port;
            }
            if !path.trim_matches('/').is_empty() {
                endpoint.prefix = prefix(path)?;
            }
        }
        ensure!(
            !endpoint.host.is_empty()
                && !endpoint
                    .host
                    .contains(|c: char| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '@')),
            "elasticsearch_endpoint_invalid"
        );
        if !prefix_option.trim().trim_matches('/').is_empty() {
            endpoint.prefix = prefix(prefix_option)?;
        }
        Ok(endpoint)
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
        dss: &DigitallySignedStruct,
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
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Verifies the chain as usual, and the name against the host of the
/// profile whatever the URL says: behind an SSH tunnel a server that is
/// addressed by its IP address is dialled at 127.0.0.1, and its certificate
/// is still the one of the address in the profile.
#[derive(Debug)]
struct NameOfTheProfile {
    inner: Arc<WebPkiServerVerifier>,
    name: ServerName<'static>,
}
impl ServerCertVerifier for NameOfTheProfile {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _: &ServerName<'_>,
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
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// The TLS settings of a profile: no verification at all, or the chain
/// against the CA file of the profile, or against the bundled Mozilla roots.
fn tls_config(
    mode: TlsMode,
    authority: Option<&[u8]>,
    server: &str,
) -> Result<rustls::ClientConfig> {
    let failed = || anyhow!("database_tls_error");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|_| failed())?;
    let mut config = if mode == TlsMode::Require {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        match authority {
            Some(pem) => {
                for certificate in CertificateDer::pem_slice_iter(pem) {
                    roots
                        .add(certificate.map_err(|_| failed())?)
                        .map_err(|_| failed())?;
                }
                ensure!(!roots.is_empty(), "database_tls_error");
            }
            None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        }
        let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
            .build()
            .map_err(|_| failed())?;
        let name = ServerName::try_from(server.to_owned())
            .map_err(|_| anyhow!("database_certificate_rejected"))?;
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NameOfTheProfile { inner, name }))
            .with_no_client_auth()
    };
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// Where the requests of a profile go: the start of every URL, and the
/// address its host name stands for when that is not what DNS would say.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Target {
    /// `https://name:port/prefix`, without a slash at its end.
    pub base: String,
    /// The `Host` line: the server as the profile names it.
    pub host: String,
    /// Name in the URL and the address to dial for it.
    pub resolve: Option<(String, SocketAddr)>,
}

pub(super) fn target(endpoint: &Endpoint, forwarded: Option<SocketAddr>) -> Target {
    let scheme = if endpoint.encrypted { "https" } else { "http" };
    let (server, port) = (endpoint.host.as_str(), endpoint.port);
    let bracketed = |host: &str| match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(address)) => format!("[{address}]"),
        _ => host.to_owned(),
    };
    let default_port = if endpoint.encrypted { 443 } else { 80 };
    let host = if port == default_port {
        bracketed(server)
    } else {
        format!("{}:{port}", bracketed(server))
    };
    let prefix = &endpoint.prefix;
    match forwarded {
        None => Target {
            base: format!("{scheme}://{}:{port}{prefix}", bracketed(server)),
            host,
            resolve: None,
        },
        // An address has no name to resolve: the URL names the tunnel, and
        // the certificate is checked against the address of the profile.
        Some(address) if server.parse::<IpAddr>().is_ok() => Target {
            base: format!("{scheme}://{address}{prefix}"),
            host,
            resolve: None,
        },
        // A name stays in the URL, so it is what TLS sends as SNI; the
        // port is the one of the tunnel, because the client takes the port
        // from the URL.
        Some(address) => Target {
            base: format!("{scheme}://{server}:{}{prefix}", address.port()),
            host,
            resolve: Some((server.to_owned(), address)),
        },
    }
}

/// The `Authorization` line an account signs in with. It travels in a
/// header, never in the URL, which servers and proxies write into their
/// logs.
///
/// `scheme` is the "auth" option of the profile. Left on `auto`, a user
/// name means HTTP Basic and a password without a user name is an API key.
pub(super) fn authorization(
    scheme: &str,
    username: &str,
    password: &str,
) -> Result<Option<HeaderValue>> {
    use base64::Engine as _;
    let encode = |text: &str| base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let scheme = match scheme {
        "basic" | "api_key" | "bearer" => scheme,
        _ if !username.is_empty() => "basic",
        _ if !password.is_empty() => "api_key",
        _ => return Ok(None),
    };
    let line = zeroize::Zeroizing::new(match scheme {
        "basic" => {
            if username.is_empty() && password.is_empty() {
                return Ok(None);
            }
            // Basic authentication cannot carry a colon in the user name.
            ensure!(
                !username.is_empty() && !username.contains(':'),
                "elasticsearch_credentials_invalid"
            );
            let pair = zeroize::Zeroizing::new(format!("{username}:{password}"));
            format!("Basic {}", encode(&pair))
        }
        "api_key" => {
            ensure!(!password.is_empty(), "elasticsearch_credentials_required");
            if username.is_empty() {
                // The encoded form, as Kibana and the cloud console show it.
                format!("ApiKey {}", password.trim())
            } else {
                // The id of the key and its secret, given separately.
                let pair = zeroize::Zeroizing::new(format!("{username}:{password}"));
                format!("ApiKey {}", encode(&pair))
            }
        }
        _ => {
            ensure!(!password.is_empty(), "elasticsearch_credentials_required");
            format!("Bearer {}", password.trim())
        }
    });
    let mut value =
        HeaderValue::from_str(&line).map_err(|_| anyhow!("elasticsearch_credentials_invalid"))?;
    value.set_sensitive(true);
    Ok(Some(value))
}

/// What a request carries.
pub(super) enum Payload {
    /// One JSON document.
    Json(Vec<u8>),
    /// JSON documents, one per line: `_bulk`, `_msearch`.
    Lines(Vec<u8>),
}

/// What the server answered: the status and the bytes of the body.
#[derive(Debug)]
pub(super) struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Reply {
    /// The body as JSON; nothing when it is something else.
    pub(super) fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }

    /// The body of an answer that is not a refusal, as JSON.
    pub(super) fn accepted(self) -> Result<Value> {
        if let Some(refusal) = refusal(self.status, &self.body) {
            return Err(refusal.into());
        }
        ensure!(self.status < 400, "elasticsearch_not_found");
        self.json()
            .ok_or_else(|| anyhow!("elasticsearch_answer_unreadable"))
    }
}

/// Everything that is the same for every request of a session.
pub(super) struct Client {
    http: reqwest::Client,
    base: String,
    pub endpoint: Endpoint,
    permits: Semaphore,
}

impl Client {
    pub(super) async fn new(request: &ConnectRequest<'_>) -> Result<Self> {
        let profile = request.profile;
        let endpoint = Endpoint::of(
            &profile.host,
            profile.port,
            profile.tls,
            request.option("path_prefix"),
            request.forwarded.is_some(),
        )?;
        let target = target(&endpoint, request.forwarded);
        reqwest::Url::parse(&target.base).map_err(|_| anyhow!("elasticsearch_endpoint_invalid"))?;
        let mut headers = HeaderMap::new();
        headers.insert(
            HOST,
            HeaderValue::from_str(&target.host)
                .map_err(|_| anyhow!("elasticsearch_endpoint_invalid"))?,
        );
        headers.insert(USER_AGENT, HeaderValue::from_static("BazaStudio"));
        if let Some(value) =
            authorization(request.option("auth"), &profile.username, &profile.password)?
        {
            headers.insert(AUTHORIZATION, value);
        }
        let mut builder = reqwest::Client::builder()
            .default_headers(headers)
            .http1_only()
            // The profile says where the server is; a proxy of the system
            // would not know the loopback end of a tunnel.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            // A request is never sent twice behind the back of the caller.
            .retry(reqwest::retry::never())
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_idle_timeout(IDLE_TIMEOUT)
            .pool_max_idle_per_host(MAX_REQUESTS)
            .tcp_nodelay(true);
        if let Some((name, address)) = &target.resolve {
            builder = builder.resolve(name, *address);
        }
        if endpoint.encrypted {
            let authority = match (&profile.ca_file, profile.tls) {
                (Some(path), TlsMode::VerifyFull) => Some(
                    tokio::fs::read(path)
                        .await
                        .map_err(|_| anyhow!("database_tls_error"))?,
                ),
                _ => None,
            };
            builder = builder.tls_backend_preconfigured(tls_config(
                profile.tls,
                authority.as_deref(),
                &endpoint.host,
            )?);
        }
        Ok(Self {
            http: builder
                .build()
                .map_err(|_| anyhow!("database_operation_failed"))?,
            base: target.base,
            endpoint,
            permits: Semaphore::new(MAX_REQUESTS),
        })
    }

    /// The URL of a path of the API. A path that was typed cannot leave the
    /// server or the prefix of the profile.
    fn url(&self, path: &str) -> Result<reqwest::Url> {
        let refused = || anyhow!("elasticsearch_path_invalid");
        ensure!(
            !path.contains(|c: char| c.is_control() || c == '#' || c == '\\'),
            "elasticsearch_path_invalid"
        );
        let base = reqwest::Url::parse(&self.base).map_err(|_| refused())?;
        let url = reqwest::Url::parse(&format!("{}/{}", self.base, path.trim_start_matches('/')))
            .map_err(|_| refused())?;
        let inside = |path: &str| {
            let prefix = &self.endpoint.prefix;
            prefix.is_empty()
                || path
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        };
        ensure!(
            url.scheme() == base.scheme()
                && url.host_str() == base.host_str()
                && url.port_or_known_default() == base.port_or_known_default()
                && url.username().is_empty()
                && url.password().is_none()
                && inside(url.path()),
            "elasticsearch_path_invalid"
        );
        Ok(url)
    }

    /// Send one request and read its answer, at most `most` bytes of it.
    /// `opaque` is the `X-Opaque-Id` the server lists the work under, which
    /// is how a request that was given up is found and stopped there.
    pub(super) async fn send(
        &self,
        method: &str,
        path: &str,
        payload: Option<Payload>,
        opaque: Option<&str>,
        most: usize,
    ) -> Result<Reply> {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| anyhow!("database_operation_failed"))?;
        self.exchange(method, path, payload, opaque, most).await
    }

    /// Send a request at once, past the limit of requests: the one that
    /// stops another request must not wait for it.
    pub(super) async fn send_now(
        &self,
        method: &str,
        path: &str,
        payload: Option<Payload>,
    ) -> Result<Reply> {
        self.exchange(method, path, payload, None, 4 * 1024 * 1024)
            .await
    }

    async fn exchange(
        &self,
        method: &str,
        path: &str,
        payload: Option<Payload>,
        opaque: Option<&str>,
        most: usize,
    ) -> Result<Reply> {
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| anyhow!("elasticsearch_path_invalid"))?;
        let mut request = self.http.request(method, self.url(path)?);
        if let Some(id) = opaque {
            request = request.header("X-Opaque-Id", id);
        }
        request = match payload {
            Some(Payload::Json(body)) => {
                request.header(CONTENT_TYPE, "application/json").body(body)
            }
            Some(Payload::Lines(body)) => request
                .header(CONTENT_TYPE, "application/x-ndjson")
                .body(body),
            None => request,
        };
        let mut response = request
            .send()
            .await
            .map_err(|error| transport_error(&error))?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > most {
                        // Dropping the answer closes its connection: the
                        // rest is never read.
                        bail!("elasticsearch_answer_too_large");
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => bail!("database_network_error"),
            }
        }
        Ok(Reply { status, body })
    }
}

/// Something of a refusal the driver acts on, read from its text so that
/// the text itself goes no further.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hint {
    None,
    /// The server has no such API, or not for this method.
    NoHandler,
    /// More hits were asked for than `index.max_result_window` allows.
    ResultWindow,
    /// A sort or an aggregation on a field that keeps no values for it.
    Fielddata,
    /// The server does not order by `_shard_doc`.
    ShardDoc,
}

/// What the server said when it refused a request: the status and the kind
/// of the failure. Its text never leaves this module: it repeats names of
/// indices, of accounts and values of documents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Refusal {
    pub status: u16,
    /// The type of the root cause, in the spelling of the server
    /// (`index_not_found_exception`, `SyntaxCheckException`); empty when
    /// the server named none.
    pub kind: String,
    pub hint: Hint,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&super::code_of(self))
    }
}
impl std::error::Error for Refusal {}

/// A type name of the server, kept only when it looks like one.
fn kind_name(value: &Value) -> String {
    value
        .as_str()
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 64
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
        })
        .unwrap_or_default()
        .to_owned()
}

/// Read an answer as a refusal. An answer below 400 is none, and neither
/// is a 404 that carries a document of its own (`"found": false`).
pub(super) fn refusal(status: u16, body: &[u8]) -> Option<Refusal> {
    if status < 400 {
        return None;
    }
    let json: Option<Value> = serde_json::from_slice(body).ok();
    let error = json.as_ref().and_then(|json| json.get("error"));
    let (mut kind, mut reason) = (String::new(), String::new());
    match error {
        None if status == 404 => return None,
        None => {}
        Some(Value::String(text)) => reason = text.clone(),
        Some(error) => {
            let top = kind_name(&error["type"]);
            // What a search failed with is said by its shards.
            let root = &error["root_cause"][0];
            let shard = &error["failed_shards"][0]["reason"];
            let cause = [root, shard, &error["caused_by"]]
                .into_iter()
                .find(|cause| !kind_name(&cause["type"]).is_empty());
            kind = match cause {
                Some(cause)
                    if top.is_empty()
                        || matches!(
                            top.as_str(),
                            "search_phase_execution_exception"
                                | "remote_transport_exception"
                                | "exception"
                        ) =>
                {
                    kind_name(&cause["type"])
                }
                _ => top,
            };
            for part in [error, root, shard] {
                if let Some(text) = part["reason"].as_str() {
                    reason.push_str(text);
                    reason.push('\n');
                }
            }
        }
    }
    let hint = if reason.contains("no handler found") || reason.contains("Incorrect HTTP method") {
        Hint::NoHandler
    } else if reason.contains("Result window is too large")
        || reason.contains("Batch size is too large")
    {
        Hint::ResultWindow
    } else if reason.contains("_shard_doc") {
        Hint::ShardDoc
    } else if reason.contains("Fielddata") || reason.contains("not optimised for operations") {
        Hint::Fielddata
    } else {
        Hint::None
    };
    Some(Refusal { status, kind, hint })
}

/// The refusal behind a failure, for the callers that act on it.
pub(super) fn refused(error: &anyhow::Error) -> Option<&Refusal> {
    error.downcast_ref::<Refusal>()
}

/// A failure as its result code only, the way a caller of the driver gets it.
pub(super) fn plain(error: anyhow::Error) -> anyhow::Error {
    match error.downcast_ref::<Refusal>() {
        Some(refusal) => anyhow!(super::code_of(refusal)),
        None => error,
    }
}

/// Why a request got no answer, as a result code.
fn transport_error(error: &reqwest::Error) -> anyhow::Error {
    // The reason is somewhere down the chain of sources. The TLS library
    // hands a certificate it does not accept over inside I/O errors, and
    // an I/O error names the source of what it holds, not what it holds:
    // those are opened by hand.
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(link) = current {
        if let Some(tls) = link.downcast_ref::<rustls::Error>() {
            return anyhow!(tls_failure(tls));
        }
        current = match link.downcast_ref::<std::io::Error>() {
            Some(io) => io
                .get_ref()
                .map(|inner| inner as &(dyn std::error::Error + 'static)),
            None => link.source(),
        };
    }
    anyhow!(if error.is_timeout() && error.is_connect() {
        "database_connection_timeout"
    } else {
        "database_network_error"
    })
}

/// What a failed TLS handshake is called.
fn tls_failure(error: &rustls::Error) -> &'static str {
    match error {
        rustls::Error::InvalidCertificate(_)
        | rustls::Error::NoCertificatesPresented
        | rustls::Error::UnsupportedNameType => "database_certificate_rejected",
        // The server answered a TLS greeting with something else: it does
        // not speak TLS on this port.
        rustls::Error::InvalidMessage(_) | rustls::Error::InappropriateMessage { .. } => {
            "database_tls_unsupported"
        }
        _ => "database_tls_error",
    }
}

/// A name or an id as one segment of a path: everything but letters, digits
/// and `-._~` is escaped, so that a slash or a question mark in an id stays
/// part of the id.
pub(super) fn segment(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain_endpoint(host: &str, port: u16, encrypted: bool) -> Endpoint {
        Endpoint {
            host: host.into(),
            port,
            encrypted,
            prefix: String::new(),
        }
    }

    #[test]
    fn the_url_keeps_the_name_and_the_tunnel_gets_the_socket() {
        let tunnel = SocketAddr::from(([127, 0, 0, 1], 40123));
        assert_eq!(
            target(&plain_endpoint("search.example.test", 9200, false), None),
            Target {
                base: "http://search.example.test:9200".into(),
                host: "search.example.test:9200".into(),
                resolve: None,
            }
        );
        // Through a tunnel the name stays in the URL: it is the SNI of the
        // handshake and the name the certificate is checked against.
        assert_eq!(
            target(
                &plain_endpoint("abc.es.eu-central-1.aws.cloud.es.io", 443, true),
                Some(tunnel)
            ),
            Target {
                base: "https://abc.es.eu-central-1.aws.cloud.es.io:40123".into(),
                host: "abc.es.eu-central-1.aws.cloud.es.io".into(),
                resolve: Some(("abc.es.eu-central-1.aws.cloud.es.io".into(), tunnel)),
            }
        );
        // An address is dialled at the tunnel; the `Host` line still names it.
        assert_eq!(
            target(&plain_endpoint("10.0.0.5", 9200, false), Some(tunnel)),
            Target {
                base: "http://127.0.0.1:40123".into(),
                host: "10.0.0.5:9200".into(),
                resolve: None,
            }
        );
        let v6 = target(&plain_endpoint("::1", 9200, false), None);
        assert_eq!(
            (v6.base.as_str(), v6.host.as_str()),
            ("http://[::1]:9200", "[::1]:9200")
        );
        // A prefix follows the address wherever it is dialled.
        let behind = Endpoint {
            prefix: "/es".into(),
            ..plain_endpoint("proxy.example.test", 443, true)
        };
        assert_eq!(
            target(&behind, None).base,
            "https://proxy.example.test:443/es"
        );
        assert_eq!(
            target(&behind, Some(tunnel)).base,
            "https://proxy.example.test:40123/es"
        );
    }

    #[test]
    fn the_host_field_takes_a_name_or_a_whole_url() {
        let of = |host: &str, port: u16, tls: TlsMode, prefix: &str| {
            Endpoint::of(host, port, tls, prefix, false)
        };
        assert_eq!(
            of(" es.example.test ", 9200, TlsMode::Disable, "").unwrap(),
            plain_endpoint("es.example.test", 9200, false)
        );
        // A URL as a cloud console shows it: its port and its path count.
        assert_eq!(
            of(
                "https://abc.es.europe-west3.gcp.cloud.es.io:443/",
                9200,
                TlsMode::VerifyFull,
                ""
            )
            .unwrap(),
            plain_endpoint("abc.es.europe-west3.gcp.cloud.es.io", 443, true)
        );
        // Without a port in the URL the Port field stays in charge.
        let behind = of(
            "http://proxy.example.test/search/es/",
            8080,
            TlsMode::Disable,
            "",
        )
        .unwrap();
        assert_eq!((behind.port, behind.prefix.as_str()), (8080, "/search/es"));
        assert_eq!(
            of("http://[::1]:9201", 9200, TlsMode::Disable, "").unwrap(),
            plain_endpoint("::1", 9201, false)
        );
        // The option names a prefix as well, and wins over the URL.
        assert_eq!(
            of("es.example.test", 9200, TlsMode::Require, " elastic/ ")
                .unwrap()
                .prefix,
            "/elastic"
        );
        assert_eq!(
            of("http://es.example.test/a", 9200, TlsMode::Disable, "/b")
                .unwrap()
                .prefix,
            "/b"
        );
        let code = |host: &str, tls: TlsMode, prefix: &str| {
            of(host, 9200, tls, prefix).unwrap_err().to_string()
        };
        // The URL and the SSL/TLS tab have to agree.
        assert_eq!(
            code("https://es.example.test", TlsMode::Disable, ""),
            "elasticsearch_endpoint_tls_mismatch"
        );
        assert_eq!(
            code("http://es.example.test", TlsMode::Require, ""),
            "elasticsearch_endpoint_tls_mismatch"
        );
        for bad in [
            "ftp://es.example.test",
            "https://user:secret@es.example.test",
            "https://es.example.test/?pretty",
            "https://es.example.test:0",
            "https://es.example.test:port",
            "https://",
        ] {
            assert_eq!(
                code(bad, TlsMode::Require, ""),
                "elasticsearch_endpoint_invalid",
                "{bad}"
            );
        }
        assert_eq!(
            code("es.example.test", TlsMode::Disable, "../admin"),
            "elasticsearch_endpoint_invalid"
        );
        assert_eq!(
            code("es example", TlsMode::Disable, ""),
            "elasticsearch_endpoint_invalid"
        );
        // A tunnel is opened to the Host field as it stands.
        assert_eq!(
            Endpoint::of("https://es.example.test", 9200, TlsMode::Require, "", true)
                .unwrap_err()
                .to_string(),
            "elasticsearch_endpoint_url_tunnel"
        );
    }

    #[test]
    fn an_account_signs_in_through_the_authorization_header() {
        let line = |scheme: &str, user: &str, password: &str| {
            authorization(scheme, user, password).unwrap().map(|value| {
                assert!(value.is_sensitive());
                String::from_utf8(value.as_bytes().to_vec()).unwrap()
            })
        };
        assert_eq!(line("auto", "", ""), None);
        assert_eq!(line("basic", "", ""), None);
        assert_eq!(
            line("auto", "app", "zażółć:'\""),
            Some("Basic YXBwOnphxbzDs8WCxIc6JyI=".into()),
            "user:password in Base64, whatever the password holds"
        );
        // A password alone is an API key in its encoded form.
        assert_eq!(
            line(
                "auto",
                "",
                " VnVhQ2ZHY0JDZGJrUW0tZTVhT3g6dWkybHAyYXhUTm1zeWFrdzl0dk5udw== "
            ),
            Some("ApiKey VnVhQ2ZHY0JDZGJrUW0tZTVhT3g6dWkybHAyYXhUTm1zeWFrdzl0dk5udw==".into())
        );
        // The id and the secret of a key, given apart.
        assert_eq!(
            line("api_key", "VuaCfGcBCdbkQm-e5aOx", "ui2lp2axTNmsyakw9tvNnw"),
            Some("ApiKey VnVhQ2ZHY0JDZGJrUW0tZTVhT3g6dWkybHAyYXhUTm1zeWFrdzl0dk5udw==".into())
        );
        assert_eq!(
            line("bearer", "ignored", "token.value"),
            Some("Bearer token.value".into())
        );
        let code = |scheme: &str, user: &str, password: &str| {
            authorization(scheme, user, password)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            code("basic", "a:b", "x"),
            "elasticsearch_credentials_invalid"
        );
        assert_eq!(code("basic", "", "x"), "elasticsearch_credentials_invalid");
        assert_eq!(
            code("api_key", "", ""),
            "elasticsearch_credentials_required"
        );
        assert_eq!(code("bearer", "", ""), "elasticsearch_credentials_required");
        assert_eq!(
            code("api_key", "", "line\nbreak"),
            "elasticsearch_credentials_invalid"
        );
    }

    #[test]
    fn a_refusal_is_its_status_and_the_kind_of_its_cause() {
        let read = |status: u16, body: &str| refusal(status, body.as_bytes());
        assert_eq!(read(200, r#"{"error": "ignored"}"#), None);
        // Elasticsearch: the cause of a search is what its shards say.
        let search = read(
            400,
            r#"{"error":{"root_cause":[{"type":"illegal_argument_exception","reason":"Fielddata access on the _id field is disallowed"}],"type":"search_phase_execution_exception","reason":"all shards failed"},"status":400}"#,
        )
        .unwrap();
        assert_eq!(
            (search.status, search.kind.as_str(), search.hint),
            (400, "illegal_argument_exception", Hint::Fielddata)
        );
        let missing = read(
            404,
            r#"{"error":{"root_cause":[{"type":"index_not_found_exception","reason":"no such index [secret-name]"}],"type":"index_not_found_exception","reason":"no such index [secret-name]"},"status":404}"#,
        )
        .unwrap();
        assert_eq!(missing.kind, "index_not_found_exception");
        assert!(!missing.to_string().contains("secret-name"));
        // The SQL plugin of OpenSearch spells its types differently.
        assert_eq!(
            read(
                400,
                r#"{"error":{"reason":"Invalid SQL query","details":"…","type":"SyntaxCheckException"},"status":400}"#
            )
            .unwrap()
            .kind,
            "SyntaxCheckException"
        );
        // An API the server does not have is said in one line of text.
        for body in [
            r#"{"error":"no handler found for uri [/_plugins/_sql] and method [POST]"}"#,
            r#"{"error":"Incorrect HTTP method for uri [/_sql] and method [POST], allowed: [GET]","status":405}"#,
        ] {
            assert_eq!(read(405, body).unwrap().hint, Hint::NoHandler, "{body}");
        }
        assert_eq!(
            read(
                400,
                r#"{"error":{"type":"illegal_argument_exception","reason":"Result window is too large, from + size must be less than or equal to: [100]"}}"#
            )
            .unwrap()
            .hint,
            Hint::ResultWindow
        );
        // A document that is not there is an answer, not a refusal.
        assert_eq!(read(404, r#"{"_index":"a","_id":"1","found":false}"#), None);
        assert_eq!(read(404, ""), None);
        // Without a body, or with one that is not JSON, the status stands.
        let bare = read(503, "<html>upstream</html>").unwrap();
        assert_eq!((bare.status, bare.kind.as_str()), (503, ""));
        // A type that does not look like one is not passed on.
        assert_eq!(
            read(
                400,
                r#"{"error":{"type":"no such index [x] at host 10.0.0.1"}}"#
            )
            .unwrap()
            .kind,
            ""
        );
    }

    #[test]
    fn a_failed_handshake_is_told_from_a_lost_connection() {
        use rustls::{CertificateError, InvalidMessage};
        assert_eq!(
            tls_failure(&rustls::Error::InvalidCertificate(
                CertificateError::UnknownIssuer
            )),
            "database_certificate_rejected"
        );
        assert_eq!(
            tls_failure(&rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName
            )),
            "database_certificate_rejected"
        );
        assert_eq!(
            tls_failure(&rustls::Error::InvalidMessage(
                InvalidMessage::InvalidContentType
            )),
            "database_tls_unsupported"
        );
        assert_eq!(
            tls_failure(&rustls::Error::DecryptError),
            "database_tls_error"
        );
    }

    #[test]
    fn a_certificate_is_checked_against_the_name_of_the_profile() {
        // Every mode builds; a CA file without a certificate does not.
        tls_config(TlsMode::Require, None, "10.0.0.5").unwrap();
        tls_config(TlsMode::VerifyFull, None, "es.example.test").unwrap();
        tls_config(TlsMode::VerifyFull, None, "10.0.0.5").unwrap();
        assert_eq!(
            tls_config(
                TlsMode::VerifyFull,
                Some(b"not a certificate"),
                "es.example.test"
            )
            .unwrap_err()
            .to_string(),
            "database_tls_error"
        );
    }

    #[test]
    fn a_name_is_one_segment_of_a_path() {
        assert_eq!(segment("orders-2026.10"), "orders-2026.10");
        assert_eq!(segment("a/b?c#d e"), "a%2Fb%3Fc%23d%20e");
        assert_eq!(segment("zażółć"), "za%C5%BC%C3%B3%C5%82%C4%87");
        assert_eq!(segment("<logs-{now/d}>"), "%3Clogs-%7Bnow%2Fd%7D%3E");
    }
}
