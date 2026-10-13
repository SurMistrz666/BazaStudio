//! The DynamoDB JSON API over HTTP: where a profile's endpoint is, how the
//! connection is encrypted, one signed call, and the mapping of what can go
//! wrong to result codes.
//!
//! The API is one `POST` per operation with a JSON body and the operation
//! named in the `X-Amz-Target` header, signed with Signature Version 4
//! ([`sigv4`](super::sigv4)). Nothing else of an AWS SDK is needed.
use super::sigv4::{self, Credentials};
use crate::engine::ConnectRequest;
use crate::model::TlsMode;
use anyhow::{Result, anyhow, bail, ensure};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use serde_json::Value;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

/// Longest one TCP connection may take to open.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest one call may take. The service answers a call within seconds
/// (it reads at most 1 MB); more than this is a connection that went quiet.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Times a call is sent again when the service throttles it or fails.
const RETRIES: u32 = 4;
const SERVICE: &str = "dynamodb";
const API: &str = "DynamoDB_20120810";

/// Where the API of a profile is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Endpoint {
    pub https: bool,
    pub host: String,
    pub port: u16,
    /// The path of the URL: `/` unless a custom endpoint has another.
    pub path: String,
    pub region: String,
}

impl Endpoint {
    /// The endpoint of a profile, worked out before anything is dialled:
    ///
    /// * the "endpoint" option, when it holds a URL;
    /// * the Host and Port fields, for a product that runs somewhere of its
    ///   own (DynamoDB Local, Alternator) and for any profile with an SSH
    ///   tunnel, whose target those fields are;
    /// * otherwise the endpoint of the AWS region.
    pub(super) fn of(request: &ConnectRequest<'_>) -> Result<Endpoint> {
        let profile = request.profile;
        let region = profile.database.trim().to_ascii_lowercase();
        ensure!(
            !region.is_empty()
                && region.len() <= 40
                && region
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "dynamodb_region_invalid"
        );
        let encrypted = profile.tls != TlsMode::Disable;
        let custom = request.option("endpoint").trim();
        let own_address = matches!(request.product.id, "dynamodb-local" | "scylladb-alternator");
        let endpoint = if !custom.is_empty() {
            let parsed = Self::parse(custom, &region)?;
            // The SSL/TLS tab and the URL have to say the same: encryption
            // is never switched off, or on, behind the user's back.
            ensure!(parsed.https == encrypted, "dynamodb_endpoint_tls_mismatch");
            parsed
        } else if own_address || request.forwarded.is_some() {
            let host = profile.host.trim();
            ensure!(plain_host(host), "dynamodb_endpoint_invalid");
            Endpoint {
                https: encrypted,
                host: host.to_ascii_lowercase(),
                port: profile.port,
                path: "/".into(),
                region,
            }
        } else {
            // China has a domain of its own.
            let domain = if region.starts_with("cn-") {
                "amazonaws.com.cn"
            } else {
                "amazonaws.com"
            };
            Endpoint {
                https: encrypted,
                host: format!("dynamodb.{region}.{domain}"),
                port: if encrypted { 443 } else { 80 },
                path: "/".into(),
                region,
            }
        };
        Ok(endpoint)
    }

    /// Read `http://host:8000` or `https://host/path`.
    fn parse(url: &str, region: &str) -> Result<Endpoint> {
        let (https, rest) = if let Some(rest) = strip_prefix_ignore_case(url, "https://") {
            (true, rest)
        } else if let Some(rest) = strip_prefix_ignore_case(url, "http://") {
            (false, rest)
        } else {
            bail!("dynamodb_endpoint_invalid");
        };
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, "/"),
        };
        // No account, query or fragment, and a path of plain characters:
        // it is signed as it is sent.
        ensure!(
            !authority.contains('@')
                && path
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric()
                        || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~')),
            "dynamodb_endpoint_invalid"
        );
        let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
            // An IPv6 address in brackets.
            let (address, after) = inner
                .split_once(']')
                .ok_or_else(|| anyhow!("dynamodb_endpoint_invalid"))?;
            (address, after.strip_prefix(':'))
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            }
        };
        let port = match port {
            Some(port) => port
                .parse::<u16>()
                .ok()
                .filter(|port| *port > 0)
                .ok_or_else(|| anyhow!("dynamodb_endpoint_invalid"))?,
            None if https => 443,
            None => 80,
        };
        ensure!(plain_host(host), "dynamodb_endpoint_invalid");
        Ok(Endpoint {
            https,
            host: host.to_ascii_lowercase(),
            port,
            path: path.to_owned(),
            region: region.to_owned(),
        })
    }

    fn scheme(&self) -> &'static str {
        if self.https { "https" } else { "http" }
    }

    fn default_port(&self) -> bool {
        self.port == if self.https { 443 } else { 80 }
    }

    /// The host as a URL and the `Host` header write it.
    fn bracketed(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }

    /// The value of the `Host` header: the port only when it is not the
    /// one of the scheme.
    pub(super) fn authority(&self) -> String {
        if self.default_port() {
            self.bracketed()
        } else {
            format!("{}:{}", self.bracketed(), self.port)
        }
    }

    /// The endpoint for people.
    pub(super) fn url(&self) -> String {
        let path = if self.path == "/" { "" } else { &self.path };
        format!("{}://{}{path}", self.scheme(), self.authority())
    }

    /// The AWS service itself, not something that speaks its API elsewhere.
    pub(super) fn is_aws(&self) -> bool {
        self.host.ends_with(".amazonaws.com") || self.host.ends_with(".amazonaws.com.cn")
    }
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .filter(|start| start.eq_ignore_ascii_case(prefix))
        .map(|_| &text[prefix.len()..])
}

/// A host name or an address as a person types it.
fn plain_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
}

/// Verifies a certificate against the name of the endpoint when the URL
/// that is dialled names the loopback end of a tunnel instead.
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

/// The TLS settings of a profile: the bundled Mozilla roots, or its CA
/// file, or no verification at all. `pinned` is the name to verify when
/// the URL that is dialled does not carry it.
fn tls_config(
    mode: TlsMode,
    ca_file: Option<&std::path::Path>,
    pinned: Option<&str>,
) -> Result<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|_| anyhow!("database_tls_error"))?;
    if mode == TlsMode::Require {
        return Ok(builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth());
    }
    let mut roots = rustls::RootCertStore::empty();
    match ca_file {
        Some(path) => {
            let pem = std::fs::read(path).map_err(|_| anyhow!("database_tls_error"))?;
            for certificate in CertificateDer::pem_slice_iter(&pem) {
                let certificate = certificate.map_err(|_| anyhow!("database_tls_error"))?;
                roots
                    .add(certificate)
                    .map_err(|_| anyhow!("database_tls_error"))?;
            }
            ensure!(!roots.is_empty(), "database_tls_error");
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    let Some(name) = pinned else {
        return Ok(builder.with_root_certificates(roots).with_no_client_auth());
    };
    let name = ServerName::try_from(name.to_owned())
        .map_err(|_| anyhow!("database_certificate_rejected"))?;
    let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
        .build()
        .map_err(|_| anyhow!("database_tls_error"))?;
    Ok(builder
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NamedVerifier { inner, name }))
        .with_no_client_auth())
}

/// Why a call failed, before it becomes a result code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Fail {
    /// The service answered with an error: the name of its exception and
    /// its sentence. The sentence never leaves the driver.
    Service { name: String, message: String },
    /// A result code that is already decided.
    Code(&'static str),
}

impl Fail {
    /// The condition of a write did not hold.
    pub(super) fn is_condition(&self) -> bool {
        matches!(self, Fail::Service { name, .. } if name == "ConditionalCheckFailedException")
    }

    /// The server does not know the operation at all.
    pub(super) fn is_unknown_operation(&self) -> bool {
        matches!(self, Fail::Service { name, .. } if name == "UnknownOperationException")
    }

    /// The reasons a transaction was cancelled with held a failed condition.
    pub(super) fn is_cancelled_by_condition(&self) -> bool {
        matches!(self, Fail::Service { name, message }
            if name == "TransactionCanceledException" && message.contains("ConditionalCheckFailed"))
    }

    pub(super) fn code(&self) -> &'static str {
        match self {
            Fail::Code(code) => code,
            Fail::Service { name, message } => service_code(name, message),
        }
    }
}

impl From<Fail> for anyhow::Error {
    fn from(fail: Fail) -> Self {
        anyhow!(fail.code())
    }
}

/// The result code for an exception of the service. Its name decides;
/// within a validation error the stable words of its sentence tell the
/// common cases apart.
pub(super) fn service_code(name: &str, message: &str) -> &'static str {
    let message = message.to_ascii_lowercase();
    let has = |needle: &str| message.contains(needle);
    match name {
        "ResourceNotFoundException" | "TableNotFoundException" => "dynamodb_table_unknown",
        "ConditionalCheckFailedException" => "dynamodb_condition_failed",
        // DynamoDB Local names it without the last word.
        "DuplicateItemException" | "DuplicateItem" => "dynamodb_item_exists",
        // A signature carries the time it was made at, and the service
        // takes one for five minutes.
        "InvalidSignatureException"
            if has("signature expired") || has("not yet current") || has("too skewed") =>
        {
            "dynamodb_clock_skew"
        }
        "RequestTimeTooSkewed" | "RequestExpired" => "dynamodb_clock_skew",
        "UnrecognizedClientException"
        | "InvalidSignatureException"
        | "IncompleteSignatureException"
        | "MissingAuthenticationTokenException"
        | "InvalidClientTokenId"
        | "SignatureDoesNotMatch"
        | "InvalidAccessKeyId" => "database_login_rejected",
        "ExpiredTokenException" | "ExpiredToken" => "dynamodb_credentials_expired",
        "AccessDeniedException" | "AccessDenied" => "database_access_denied",
        "ProvisionedThroughputExceededException"
        | "ThrottlingException"
        | "RequestLimitExceeded"
        | "LimitExceededException" => "dynamodb_throttled",
        "ResourceInUseException" => "dynamodb_table_busy",
        "TransactionCanceledException" | "TransactionConflictException" => {
            "dynamodb_transaction_cancelled"
        }
        "ItemCollectionSizeLimitExceededException" => "dynamodb_item_too_large",
        "UnknownOperationException" => "dynamodb_operation_unknown",
        "ValidationException" | "SerializationException" => {
            if has("statement wasn't well formed")
                || has("unexpected")
                || has("syntax")
                || has("unsupported token")
            {
                "dynamodb_statement_invalid"
            } else if has("key schema") || has("key element") || has("missing the key") {
                "dynamodb_key_mismatch"
            } else if has("where clause") || has("must have a where") {
                "dynamodb_statement_needs_key"
            } else if has("item size") || has("exceeded the maximum allowed size") {
                "dynamodb_item_too_large"
            } else if has("empty string") || has("empty set") || has("empty binary") {
                "dynamodb_key_required"
            } else if has("reserved keyword") {
                "dynamodb_statement_invalid"
            } else {
                "dynamodb_request_invalid"
            }
        }
        "InternalServerError" | "InternalFailure" | "ServiceUnavailable" => {
            "dynamodb_service_error"
        }
        // A refusal this driver has no words of its own for.
        _ => "dynamodb_request_invalid",
    }
}

/// Signed calls to one endpoint.
pub(super) struct Client {
    http: reqwest::Client,
    pub endpoint: Endpoint,
    credentials: Credentials,
    /// The URL that is requested: the endpoint itself, or its name without
    /// the port (the address is overridden), or the loopback end of a
    /// tunnel.
    url: String,
}

impl Client {
    pub(super) fn new(request: &ConnectRequest<'_>) -> Result<Client> {
        let profile = request.profile;
        let endpoint = Endpoint::of(request)?;
        ensure!(
            !profile.username.trim().is_empty() && !profile.password.is_empty(),
            "dynamodb_credentials_required"
        );
        let session_token = Some(request.option("session_token").trim())
            .filter(|token| !token.is_empty())
            .map(str::to_owned);
        let credentials = Credentials {
            access_key: profile.username.trim().to_owned(),
            secret_key: profile.password.clone(),
            session_token,
        };
        let literal = endpoint.host.parse::<IpAddr>().is_ok();
        let mut builder = reqwest::Client::builder()
            // The endpoint is dialled directly, like every other database.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(DIAL_TIMEOUT)
            .timeout(CALL_TIMEOUT)
            .pool_idle_timeout(Duration::from_secs(50))
            .tcp_nodelay(true);
        let path = &endpoint.path;
        let url = match request.forwarded {
            // The loopback end of an SSH tunnel. A name keeps standing in
            // the URL, for TLS, and is resolved to the tunnel; an address
            // cannot be resolved to another one, so the tunnel is the URL.
            Some(forwarded) if literal => {
                format!("{}://{forwarded}{path}", endpoint.scheme())
            }
            Some(forwarded) => {
                builder = builder.resolve(&endpoint.host, forwarded);
                format!("{}://{}{path}", endpoint.scheme(), endpoint.host)
            }
            None => format!(
                "{}://{}:{}{path}",
                endpoint.scheme(),
                endpoint.bracketed(),
                endpoint.port
            ),
        };
        if endpoint.https {
            let pinned = (request.forwarded.is_some() && literal).then_some(endpoint.host.as_str());
            builder = builder.tls_backend_preconfigured(tls_config(
                profile.tls,
                profile.ca_file.as_deref(),
                pinned,
            )?);
        }
        Ok(Client {
            http: builder.build().map_err(|_| anyhow!("database_tls_error"))?,
            endpoint,
            credentials,
            url,
        })
    }

    /// One attempt of one call.
    async fn attempt(&self, operation: &str, body: &[u8]) -> Result<Value, Fail> {
        let target = format!("{API}.{operation}");
        let host = self.endpoint.authority();
        let stamp = sigv4::timestamp(chrono::Utc::now());
        let mut headers = vec![
            ("content-type", "application/x-amz-json-1.0"),
            ("host", host.as_str()),
            ("x-amz-date", stamp.as_str()),
            ("x-amz-target", target.as_str()),
        ];
        if let Some(token) = &self.credentials.session_token {
            headers.push(("x-amz-security-token", token.as_str()));
        }
        let authorization = sigv4::authorization(
            &sigv4::Request {
                method: "POST",
                path: &self.endpoint.path,
                query: "",
                headers: &headers,
                body,
            },
            &self.credentials,
            &stamp,
            &self.endpoint.region,
            SERVICE,
        );
        let mut request = self.http.post(&self.url).body(body.to_vec());
        for (name, value) in &headers {
            request = request.header(*name, *value);
        }
        let response = request
            .header("authorization", authorization)
            .header("accept-encoding", "identity")
            .send()
            .await
            .map_err(|error| Fail::Code(transport_code(&error)))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|_| Fail::Code("database_network_error"))?;
        if status.is_success() {
            return serde_json::from_slice(&bytes)
                .map_err(|_| Fail::Code("dynamodb_endpoint_not_dynamodb"));
        }
        // The service failed to serve the call: worth another try.
        if status.is_server_error() {
            return Err(Fail::Code("dynamodb_service_error"));
        }
        // An error of the service is JSON with the name of its exception.
        let Ok(error) = serde_json::from_slice::<Value>(&bytes) else {
            return Err(Fail::Code("dynamodb_endpoint_not_dynamodb"));
        };
        let name = error["__type"]
            .as_str()
            .or_else(|| error["code"].as_str())
            .unwrap_or_default()
            .rsplit('#')
            .next()
            .unwrap_or_default()
            .to_owned();
        if name.is_empty() {
            return Err(Fail::Code("dynamodb_endpoint_not_dynamodb"));
        }
        let mut message = error["message"]
            .as_str()
            .or_else(|| error["Message"].as_str())
            .unwrap_or_default()
            .to_owned();
        // Why each write of a cancelled transaction was refused.
        if let Some(reasons) = error["CancellationReasons"].as_array() {
            for reason in reasons {
                message.push(' ');
                message.push_str(reason["Code"].as_str().unwrap_or_default());
            }
        }
        Err(Fail::Service { name, message })
    }

    /// Call an operation of the API. A call the service throttled, or
    /// failed to serve, is sent again a few times, each time later.
    pub(super) async fn call(&self, operation: &str, body: &Value) -> Result<Value, Fail> {
        let bytes = serde_json::to_vec(body).map_err(|_| Fail::Code("dynamodb_request_invalid"))?;
        let mut wait = Duration::from_millis(120);
        for attempt in 0..=RETRIES {
            match self.attempt(operation, &bytes).await {
                Err(fail)
                    if attempt < RETRIES
                        && matches!(
                            fail.code(),
                            "dynamodb_throttled" | "dynamodb_service_error"
                        ) =>
                {
                    tokio::time::sleep(wait).await;
                    wait *= 2;
                }
                result => return result,
            }
        }
        Err(Fail::Code("dynamodb_throttled"))
    }
}

/// The result code for a request that did not get an answer, read from the
/// names of the nested errors. The text never leaves here.
fn transport_code(error: &reqwest::Error) -> &'static str {
    let debug = format!("{error:?}");
    let has = |needle: &str| debug.contains(needle);
    if has("InvalidCertificate")
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
    {
        // A TLS greeting was answered with something else.
        "database_tls_unsupported"
    } else if error.is_timeout() {
        "database_connection_timeout"
    } else {
        "database_network_error"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::dynamodb::ENGINE;
    use crate::model::ConnectionProfile;
    use std::net::SocketAddr;

    fn profile(product: &str) -> ConnectionProfile {
        let spec = ENGINE.product(product);
        ConnectionProfile {
            name: "Test".into(),
            engine: ENGINE,
            product: ENGINE.spec().stored_product(spec).into(),
            host: "localhost".into(),
            port: spec.default_port,
            database: "eu-central-1".into(),
            username: "AKIDEXAMPLE".into(),
            password: "secret".into(),
            tls: spec.default_tls,
            ..ConnectionProfile::default()
        }
    }

    fn endpoint(profile: &ConnectionProfile, forwarded: Option<u16>) -> Result<Endpoint, String> {
        Endpoint::of(&ConnectRequest {
            profile,
            product: profile.product_spec(),
            forwarded: forwarded.map(|port| SocketAddr::from(([127, 0, 0, 1], port))),
        })
        .map_err(|error| error.to_string())
    }

    #[test]
    fn the_service_is_found_by_its_region() {
        let aws = profile("amazon-dynamodb");
        let found = endpoint(&aws, None).unwrap();
        // Host and Port of the profile are not used.
        assert_eq!(found.url(), "https://dynamodb.eu-central-1.amazonaws.com");
        assert_eq!(found.authority(), "dynamodb.eu-central-1.amazonaws.com");
        assert_eq!((found.port, found.region.as_str()), (443, "eu-central-1"));
        assert!(found.is_aws());
        let mut china = aws.clone();
        china.database = "CN-North-1".into();
        assert_eq!(
            endpoint(&china, None).unwrap().host,
            "dynamodb.cn-north-1.amazonaws.com.cn"
        );
        // Without encryption the service is asked on its plain port.
        let mut plain = aws.clone();
        plain.tls = TlsMode::Disable;
        assert_eq!(
            endpoint(&plain, None).unwrap().url(),
            "http://dynamodb.eu-central-1.amazonaws.com"
        );
        for bad in ["", "eu central", "eu_central_1", "x/../y"] {
            let mut broken = aws.clone();
            broken.database = bad.into();
            assert_eq!(
                endpoint(&broken, None).unwrap_err(),
                "dynamodb_region_invalid",
                "{bad}"
            );
        }
    }

    #[test]
    fn a_server_of_its_own_is_at_the_host_and_port_of_the_profile() {
        let local = profile("dynamodb-local");
        let found = endpoint(&local, None).unwrap();
        assert_eq!(found.url(), "http://localhost:8000");
        assert_eq!(found.authority(), "localhost:8000");
        assert!(!found.is_aws());
        let mut encrypted = profile("scylladb-alternator");
        encrypted.host = "Scylla.Example.Test".into();
        encrypted.port = 8043;
        encrypted.tls = TlsMode::VerifyFull;
        assert_eq!(
            endpoint(&encrypted, None).unwrap().url(),
            "https://scylla.example.test:8043"
        );
        // Through a tunnel the Host and Port are what the SSH server reaches.
        let mut tunnelled = profile("amazon-dynamodb");
        tunnelled.host = "vpce-0abc.dynamodb.eu-central-1.vpce.amazonaws.com".into();
        tunnelled.port = 443;
        let found = endpoint(&tunnelled, Some(50123)).unwrap();
        assert_eq!(
            found.url(),
            "https://vpce-0abc.dynamodb.eu-central-1.vpce.amazonaws.com"
        );
        assert!(found.is_aws());
        let mut odd = local.clone();
        odd.host = "local host".into();
        assert_eq!(
            endpoint(&odd, None).unwrap_err(),
            "dynamodb_endpoint_invalid"
        );
    }

    #[test]
    fn an_endpoint_url_is_read_and_has_to_agree_with_the_tls_mode() {
        let mut custom = profile("amazon-dynamodb");
        custom.set_option("endpoint", " HTTPS://VPCE-1.example.test:8443/dynamo/v1 ");
        let found = endpoint(&custom, None).unwrap();
        assert_eq!(
            (
                found.https,
                found.host.as_str(),
                found.port,
                found.path.as_str()
            ),
            (true, "vpce-1.example.test", 8443, "/dynamo/v1")
        );
        assert_eq!(found.url(), "https://vpce-1.example.test:8443/dynamo/v1");
        custom.set_option("endpoint", "http://127.0.0.1:8000");
        // The profile still asks for verified encryption: said, not ignored.
        assert_eq!(
            endpoint(&custom, None).unwrap_err(),
            "dynamodb_endpoint_tls_mismatch"
        );
        custom.tls = TlsMode::Disable;
        let found = endpoint(&custom, None).unwrap();
        assert_eq!(found.url(), "http://127.0.0.1:8000");
        custom.set_option("endpoint", "http://[::1]:8000");
        let found = endpoint(&custom, None).unwrap();
        assert_eq!(
            (found.host.as_str(), found.authority()),
            ("::1", "[::1]:8000".into())
        );
        custom.set_option("endpoint", "http://localhost");
        assert_eq!(endpoint(&custom, None).unwrap().port, 80);
        for bad in [
            "localhost:8000",
            "ftp://localhost",
            "http://user:pw@localhost",
            "http://localhost:0",
            "http://localhost:x",
            "http://localhost/a?b=1",
            "http://",
        ] {
            custom.set_option("endpoint", bad);
            assert_eq!(
                endpoint(&custom, None).unwrap_err(),
                "dynamodb_endpoint_invalid",
                "{bad}"
            );
        }
    }

    #[test]
    fn a_client_needs_both_keys() {
        let mut local = profile("dynamodb-local");
        let request = |profile: &ConnectionProfile| {
            Client::new(&ConnectRequest {
                profile,
                product: profile.product_spec(),
                forwarded: None,
            })
            .map(|client| client.url)
            .map_err(|error| error.to_string())
        };
        assert_eq!(request(&local).unwrap(), "http://localhost:8000/");
        local.password.clear();
        assert_eq!(
            request(&local).unwrap_err(),
            "dynamodb_credentials_required"
        );
        // A tunnel is dialled at its loopback end; a name stays in the URL.
        let mut named = profile("dynamodb-local");
        named.host = "dynamo.internal".into();
        let tunnelled = Client::new(&ConnectRequest {
            profile: &named,
            product: named.product_spec(),
            forwarded: Some(SocketAddr::from(([127, 0, 0, 1], 50123))),
        })
        .unwrap();
        assert_eq!(tunnelled.url, "http://dynamo.internal/");
        assert_eq!(tunnelled.endpoint.authority(), "dynamo.internal:8000");
        let mut address = profile("dynamodb-local");
        address.host = "10.0.0.5".into();
        let tunnelled = Client::new(&ConnectRequest {
            profile: &address,
            product: address.product_spec(),
            forwarded: Some(SocketAddr::from(([127, 0, 0, 1], 50123))),
        })
        .unwrap();
        assert_eq!(tunnelled.url, "http://127.0.0.1:50123/");
        assert_eq!(tunnelled.endpoint.authority(), "10.0.0.5:8000");
    }

    #[test]
    fn exceptions_of_the_service_have_codes() {
        for (name, message, code) in [
            (
                "ResourceNotFoundException",
                "Cannot do operations on a non-existent table",
                "dynamodb_table_unknown",
            ),
            (
                "ConditionalCheckFailedException",
                "The conditional request failed",
                "dynamodb_condition_failed",
            ),
            (
                "DuplicateItemException",
                "Duplicate primary key exists in table",
                "dynamodb_item_exists",
            ),
            (
                "UnrecognizedClientException",
                "The security token included in the request is invalid.",
                "database_login_rejected",
            ),
            (
                "InvalidSignatureException",
                "The request signature we calculated does not match",
                "database_login_rejected",
            ),
            (
                "ExpiredTokenException",
                "The security token included in the request is expired",
                "dynamodb_credentials_expired",
            ),
            (
                "AccessDeniedException",
                "User is not authorized to perform: dynamodb:Scan",
                "database_access_denied",
            ),
            (
                "ProvisionedThroughputExceededException",
                "",
                "dynamodb_throttled",
            ),
            ("ThrottlingException", "Rate exceeded", "dynamodb_throttled"),
            (
                "InvalidSignatureException",
                "Signature expired: 20260101T000000Z is now earlier than 20260101T001500Z (20260101T002000Z - 5 min.)",
                "dynamodb_clock_skew",
            ),
            (
                "ResourceInUseException",
                "Table already exists",
                "dynamodb_table_busy",
            ),
            (
                "ValidationException",
                "Statement wasn't well formed, can't be processed: Unexpected keyword",
                "dynamodb_statement_invalid",
            ),
            (
                "ValidationException",
                "The provided key element does not match the schema",
                "dynamodb_key_mismatch",
            ),
            (
                "ValidationException",
                "Where clause does not contain a mandatory equality on all key attributes",
                "dynamodb_statement_needs_key",
            ),
            (
                "ValidationException",
                "Item size has exceeded the maximum allowed size",
                "dynamodb_item_too_large",
            ),
            (
                "ValidationException",
                "One or more parameter values are not valid. The AttributeValue for a key attribute cannot contain an empty string value. Key: id",
                "dynamodb_key_required",
            ),
            (
                "ValidationException",
                "anything else",
                "dynamodb_request_invalid",
            ),
            ("InternalServerError", "", "dynamodb_service_error"),
            (
                "DuplicateItem",
                "Duplicate primary key exists in table",
                "dynamodb_item_exists",
            ),
            ("SomethingNewException", "", "dynamodb_request_invalid"),
        ] {
            assert_eq!(service_code(name, message), code, "{name}: {message}");
        }
        let condition = Fail::Service {
            name: "ConditionalCheckFailedException".into(),
            message: String::new(),
        };
        assert!(condition.is_condition() && !condition.is_unknown_operation());
        let cancelled = Fail::Service {
            name: "TransactionCanceledException".into(),
            message: "Transaction cancelled None ConditionalCheckFailed".into(),
        };
        assert!(cancelled.is_cancelled_by_condition());
        assert_eq!(
            anyhow::Error::from(cancelled).to_string(),
            "dynamodb_transaction_cancelled"
        );
        assert_eq!(
            Fail::Code("database_network_error").code(),
            "database_network_error"
        );
    }

    #[test]
    fn the_tls_settings_follow_the_mode_of_the_profile() {
        assert!(tls_config(TlsMode::Require, None, None).is_ok());
        assert!(tls_config(TlsMode::VerifyFull, None, None).is_ok());
        assert!(tls_config(TlsMode::VerifyFull, None, Some("10.0.0.5")).is_ok());
        assert_eq!(
            tls_config(
                TlsMode::VerifyFull,
                Some(std::path::Path::new("/nonexistent/ca.pem")),
                None
            )
            .unwrap_err()
            .to_string(),
            "database_tls_error"
        );
    }
}
