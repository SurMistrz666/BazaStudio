//! The HTTP interface of ClickHouse: where a request goes, how it is
//! encrypted, who it signs in as and what a refusal is called.
//!
//! One request carries one statement. The address that is dialled (the
//! server, or the loopback end of an SSH tunnel) and the name of the server
//! (for the `Host` line, for SNI and for the certificate) are two separate
//! things here: the URL keeps the name and the client is told where that
//! name lives.
use crate::engine::ConnectRequest;
use crate::model::TlsMode;
use anyhow::{Result, anyhow};
use reqwest::header::{AUTHORIZATION, HOST, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Semaphore, SemaphorePermit};

/// Requests a session sends at the same time at most; the same number as
/// the connections of the other drivers.
const MAX_REQUESTS: usize = 4;
/// How long opening the socket and the TLS handshake may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A connection that was not used for this long is not used again. Servers
/// before 24.x close an idle one after three seconds, and a statement sent
/// into a connection that is being closed is lost without an answer.
const IDLE_TIMEOUT: Duration = Duration::from_secs(2);

/// Code of the server for a failure, from the header of its answer or from
/// the first words of its text.
pub(super) type ServerCode = u32;

/// Everything that is the same for every request of a session.
pub(super) struct Client {
    http: reqwest::Client,
    /// `http://name:port/`
    url: reqwest::Url,
    database: String,
    permits: Semaphore,
}

/// A request that is on its way: the permit goes back when it is dropped.
pub(super) struct Reply<'a> {
    pub response: reqwest::Response,
    _permit: SemaphorePermit<'a>,
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
                anyhow::ensure!(!roots.is_empty(), "database_tls_error");
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

/// Where the requests of a profile go: the URL, and the address its host
/// name stands for when that is not what DNS would say.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Target {
    pub url: String,
    /// The `Host` line: the server as the profile names it.
    pub host: String,
    /// Name in the URL and the address to dial for it.
    pub resolve: Option<(String, SocketAddr)>,
}

pub(super) fn target(
    server: &str,
    port: u16,
    encrypted: bool,
    forwarded: Option<SocketAddr>,
) -> Target {
    let scheme = if encrypted { "https" } else { "http" };
    let bracketed = |host: &str| match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(address)) => format!("[{address}]"),
        _ => host.to_owned(),
    };
    let default_port = if encrypted { 443 } else { 80 };
    let host = if port == default_port {
        bracketed(server)
    } else {
        format!("{}:{port}", bracketed(server))
    };
    match forwarded {
        None => Target {
            url: format!("{scheme}://{}:{port}/", bracketed(server)),
            host,
            resolve: None,
        },
        // An address has no name to resolve: the URL names the tunnel, and
        // the certificate is checked against the address of the profile.
        Some(address) if server.parse::<IpAddr>().is_ok() => Target {
            url: format!("{scheme}://{address}/"),
            host,
            resolve: None,
        },
        // A name stays in the URL, so it is what TLS sends as SNI; the
        // port is the one of the tunnel, because the client takes the port
        // from the URL.
        Some(address) => Target {
            url: format!("{scheme}://{server}:{}/", address.port()),
            host,
            resolve: Some((server.to_owned(), address)),
        },
    }
}

/// The headers an account signs in with. They travel in headers, never in
/// the URL, which servers and proxies write into their logs.
///
/// Basic authentication carries any password, and proxies in front of
/// ClickHouse understand it. Only a user name with a colon cannot be
/// written that way: it goes into the headers of ClickHouse itself.
fn credentials(username: &str, password: &str) -> Result<Vec<(HeaderName, HeaderValue)>> {
    use base64::Engine as _;
    let refused = || anyhow!("clickhouse_credentials_invalid");
    let sensitive = |mut value: HeaderValue| {
        value.set_sensitive(true);
        value
    };
    if username.is_empty() && password.is_empty() {
        // The server signs such a request in as its `default` user.
        return Ok(Vec::new());
    }
    let username = if username.is_empty() {
        "default"
    } else {
        username
    };
    if !username.contains(':') {
        let pair = zeroize::Zeroizing::new(format!("{username}:{password}"));
        let encoded = zeroize::Zeroizing::new(format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(pair.as_bytes())
        ));
        let value = HeaderValue::from_str(&encoded).map_err(|_| refused())?;
        return Ok(vec![(AUTHORIZATION, sensitive(value))]);
    }
    Ok(vec![
        (
            HeaderName::from_static("x-clickhouse-user"),
            HeaderValue::from_bytes(username.as_bytes()).map_err(|_| refused())?,
        ),
        (
            HeaderName::from_static("x-clickhouse-key"),
            sensitive(HeaderValue::from_bytes(password.as_bytes()).map_err(|_| refused())?),
        ),
    ])
}

impl Client {
    pub(super) async fn new(request: &ConnectRequest<'_>) -> Result<Self> {
        let profile = request.profile;
        let encrypted = profile.tls != TlsMode::Disable;
        let server = request.server_name().trim();
        let target = target(server, profile.port, encrypted, request.forwarded);
        let url =
            reqwest::Url::parse(&target.url).map_err(|_| anyhow!("database_endpoint_invalid"))?;
        let mut headers = HeaderMap::new();
        headers.insert(
            HOST,
            HeaderValue::from_str(&target.host)
                .map_err(|_| anyhow!("database_endpoint_invalid"))?,
        );
        headers.insert(USER_AGENT, HeaderValue::from_static("BazaStudio"));
        for (name, value) in credentials(&profile.username, &profile.password)? {
            headers.insert(name, value);
        }
        let mut builder = reqwest::Client::builder()
            .default_headers(headers)
            .http1_only()
            // The profile says where the server is; a proxy of the system
            // would not know the loopback end of a tunnel.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            // A statement is never sent twice behind the back of the caller.
            .retry(reqwest::retry::never())
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_idle_timeout(IDLE_TIMEOUT)
            .pool_max_idle_per_host(MAX_REQUESTS)
            .tcp_nodelay(true);
        if let Some((name, address)) = &target.resolve {
            builder = builder.resolve(name, *address);
        }
        if encrypted {
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
                server,
            )?);
        }
        Ok(Self {
            http: builder
                .build()
                .map_err(|_| anyhow!("database_operation_failed"))?,
            url,
            database: profile.database.clone(),
            permits: Semaphore::new(MAX_REQUESTS),
        })
    }

    /// Send one statement. `parameters` are settings and parameters of the
    /// interface; `body` is the statement, or the statement and the data
    /// that follows it. The database of the profile is named unless the
    /// statement continues a session that may have chosen another one.
    pub(super) async fn send(
        &self,
        parameters: &[(&str, &str)],
        body: Vec<u8>,
        with_database: bool,
    ) -> Result<Reply<'_>, reqwest::Error> {
        let permit = self
            .permits
            .acquire()
            .await
            .expect("the semaphore of a session is never closed");
        let response = self.post(parameters, body, with_database).await?;
        Ok(Reply {
            response,
            _permit: permit,
        })
    }

    /// Send a statement at once, past the limit of requests: the one that
    /// stops another statement must not wait for it.
    pub(super) async fn send_now(
        &self,
        parameters: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.post(parameters, body, false).await
    }

    async fn post(
        &self,
        parameters: &[(&str, &str)],
        body: Vec<u8>,
        with_database: bool,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut url = self.url.clone();
        {
            let mut query = url.query_pairs_mut();
            if with_database && !self.database.is_empty() {
                query.append_pair("database", &self.database);
            }
            for (name, value) in parameters {
                query.append_pair(name, value);
            }
        }
        self.http.post(url).body(body).send().await
    }
}

/// The code of the server in an answer that is a refusal: from its header,
/// otherwise from the `Code: 60.` its text starts with.
pub(super) fn server_code(headers: &HeaderMap, text: &[u8]) -> Option<ServerCode> {
    headers
        .get("x-clickhouse-exception-code")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
        .or_else(|| code_in_text(text))
}

/// The code in `Code: 60. DB::Exception: …`, wherever in the text it stands.
pub(super) fn code_in_text(text: &[u8]) -> Option<ServerCode> {
    let text = String::from_utf8_lossy(text);
    let rest = &text[text.find("Code: ")? + 6..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if !rest[digits.len()..].starts_with('.') {
        return None;
    }
    digits.parse().ok()
}

/// Why a request got no answer, as a result code.
pub(super) fn transport_error(error: &reqwest::Error) -> anyhow::Error {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_url_keeps_the_name_and_the_tunnel_gets_the_socket() {
        let tunnel = SocketAddr::from(([127, 0, 0, 1], 40123));
        assert_eq!(
            target("db.example.test", 8123, false, None),
            Target {
                url: "http://db.example.test:8123/".into(),
                host: "db.example.test:8123".into(),
                resolve: None,
            }
        );
        // Through a tunnel the name stays in the URL: it is the SNI of the
        // handshake and the name the certificate is checked against.
        assert_eq!(
            target(
                "abc.eu-central-1.aws.clickhouse.cloud",
                8443,
                true,
                Some(tunnel)
            ),
            Target {
                url: "https://abc.eu-central-1.aws.clickhouse.cloud:40123/".into(),
                host: "abc.eu-central-1.aws.clickhouse.cloud:8443".into(),
                resolve: Some(("abc.eu-central-1.aws.clickhouse.cloud".into(), tunnel)),
            }
        );
        // An address is dialled at the tunnel; the `Host` line still names it.
        assert_eq!(
            target("10.0.0.5", 8123, false, Some(tunnel)),
            Target {
                url: "http://127.0.0.1:40123/".into(),
                host: "10.0.0.5:8123".into(),
                resolve: None,
            }
        );
        assert_eq!(target("::1", 8123, false, None).url, "http://[::1]:8123/");
        assert_eq!(target("::1", 8123, false, None).host, "[::1]:8123");
        // The usual port of the scheme is not repeated in the `Host` line.
        assert_eq!(
            target("ch.example.test", 443, true, None).host,
            "ch.example.test"
        );
    }

    #[test]
    fn an_account_signs_in_through_headers() {
        assert!(credentials("", "").unwrap().is_empty());
        let basic = credentials("app", "zażółć:'\"").unwrap();
        assert_eq!(basic.len(), 1);
        assert_eq!(basic[0].0, AUTHORIZATION);
        assert!(basic[0].1.is_sensitive());
        assert_eq!(
            basic[0].1.as_bytes(),
            b"Basic YXBwOnphxbzDs8WCxIc6JyI=",
            "user:password in Base64, whatever the password holds"
        );
        // A password without a user name belongs to the default user.
        assert_eq!(
            credentials("", "x").unwrap()[0].1.as_bytes(),
            b"Basic ZGVmYXVsdDp4"
        );
        // A colon in the user name does not fit Basic authentication.
        let own = credentials("a:b", "x").unwrap();
        assert_eq!(own[0].0.as_str(), "x-clickhouse-user");
        assert!(own[1].1.is_sensitive());
        assert_eq!(
            credentials("a:b", "line\nbreak").unwrap_err().to_string(),
            "clickhouse_credentials_invalid"
        );
    }

    #[test]
    fn the_code_of_the_server_is_found_in_the_header_or_in_the_text() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            server_code(&headers, b"Code: 60. DB::Exception: Unknown table"),
            Some(60)
        );
        assert_eq!(
            code_in_text(b"\r\n__exception__\r\ntag\r\nCode: 395. DB::Exception: boom"),
            Some(395)
        );
        assert_eq!(code_in_text(b"Port 9000 is for clickhouse-client"), None);
        assert_eq!(code_in_text(b"Code: abc"), None);
        headers.insert(
            "x-clickhouse-exception-code",
            HeaderValue::from_static("516"),
        );
        assert_eq!(server_code(&headers, b"anything"), Some(516));
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
        tls_config(TlsMode::VerifyFull, None, "db.example.test").unwrap();
        tls_config(TlsMode::VerifyFull, None, "10.0.0.5").unwrap();
        assert_eq!(
            tls_config(
                TlsMode::VerifyFull,
                Some(b"not a certificate"),
                "db.example.test"
            )
            .unwrap_err()
            .to_string(),
            "database_tls_error"
        );
    }
}
