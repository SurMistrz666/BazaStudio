//! TLS for a connection that goes through an SSH tunnel.
//!
//! The MongoDB client names a server by the address it dials, so it cannot
//! dial the loopback end of a tunnel and still verify the certificate of
//! the real host. Here the encryption is done on this side instead: a
//! listener on the loopback interface takes the plain connections of the
//! client, opens a TLS connection through the tunnel for each of them, with
//! the host of the profile as the server name, and passes the bytes on.
//! The plain part never leaves this process's loopback socket, which is as
//! exposed as the forwarded port of the tunnel itself.
use crate::model::TlsMode;
use anyhow::{Result, anyhow, ensure};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// "Encryption required": the connection is encrypted and the certificate
/// is not checked, as the mode says.
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
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Why a TLS connection was not made, as a result code: the certificate was
/// refused, or the other side does not speak TLS.
pub(super) fn tls_failure(error: &std::io::Error) -> Option<&'static str> {
    let inner = error.get_ref()?.downcast_ref::<rustls::Error>()?;
    Some(match inner {
        rustls::Error::InvalidCertificate(_)
        | rustls::Error::NoCertificatesPresented
        | rustls::Error::UnsupportedNameType => "database_certificate_rejected",
        rustls::Error::InvalidMessage(_) | rustls::Error::InappropriateMessage { .. } => {
            "database_tls_unsupported"
        }
        _ => "database_tls_error",
    })
}

/// The connection ended before the other side said anything.
pub(super) fn closed_early(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset, UnexpectedEof};
    matches!(
        error.kind(),
        UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe
    )
}

fn connector(mode: TlsMode, ca_file: Option<&Path>) -> Result<TlsConnector> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|_| anyhow!("database_tls_error"))?;
    let config = match mode {
        TlsMode::VerifyFull => {
            let mut roots = rustls::RootCertStore::empty();
            match ca_file {
                Some(path) => {
                    let certificates = CertificateDer::pem_file_iter(path)
                        .map_err(|_| anyhow!("mongodb_ca_file_unreadable"))?
                        .flatten();
                    let (added, _) = roots.add_parsable_certificates(certificates);
                    ensure!(added > 0, "mongodb_ca_file_unreadable");
                }
                None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        TlsMode::Require => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth(),
        TlsMode::Disable => return Err(anyhow!("database_tls_error")),
    };
    Ok(TlsConnector::from(Arc::new(config)))
}

const STEP: Duration = Duration::from_secs(10);

async fn handshake(
    connector: &TlsConnector,
    name: &ServerName<'static>,
    target: SocketAddr,
) -> Result<TlsStream<TcpStream>> {
    let stream = tokio::time::timeout(STEP, TcpStream::connect(target))
        .await
        .map_err(|_| anyhow!("database_connection_timeout"))?
        .map_err(|_| anyhow!("database_network_error"))?;
    let _ = stream.set_nodelay(true);
    tokio::time::timeout(STEP, connector.connect(name.clone(), stream))
        .await
        .map_err(|_| anyhow!("database_connection_timeout"))?
        .map_err(|error| {
            anyhow!(match tls_failure(&error) {
                Some(code) => code,
                // A server without TLS answers a TLS greeting by hanging up.
                None if closed_early(&error) => "database_tls_unsupported",
                None => "database_network_error",
            })
        })
}

/// The loopback listener that encrypts for the client. Dropping it stops
/// the listener; connections that are open end with their client.
pub(super) struct Relay {
    address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}
impl Relay {
    /// Check that `target` (the loopback end of the tunnel) answers with
    /// TLS that passes `mode` for `server_name`, then start listening.
    pub(super) async fn start(
        target: SocketAddr,
        server_name: &str,
        mode: TlsMode,
        ca_file: Option<&Path>,
    ) -> Result<Self> {
        let connector = connector(mode, ca_file)?;
        let name = ServerName::try_from(server_name.to_owned())
            .map_err(|_| anyhow!("database_endpoint_invalid"))?;
        // One handshake first, so that a refused certificate is reported as
        // that, and not as a connection the client lost.
        drop(handshake(&connector, &name, target).await?);
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|_| anyhow!("database_network_error"))?;
        let address = listener
            .local_addr()
            .map_err(|_| anyhow!("database_network_error"))?;
        let task = tokio::spawn(async move {
            while let Ok((mut plain, _)) = listener.accept().await {
                let connector = connector.clone();
                let name = name.clone();
                tokio::spawn(async move {
                    if let Ok(mut encrypted) = handshake(&connector, &name, target).await {
                        let _ = tokio::io::copy_bidirectional(&mut plain, &mut encrypted).await;
                    }
                });
            }
        });
        Ok(Self { address, task })
    }
    /// Where the client connects, without TLS of its own.
    pub(super) fn address(&self) -> SocketAddr {
        self.address
    }
}
impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_server_without_tls_is_reported_as_such_and_nothing_listens() {
        // A plain server that hangs up on anything it is sent.
        let plain = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let target = plain.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = plain.accept().await {
                drop(stream);
            }
        });
        for mode in [TlsMode::Require, TlsMode::VerifyFull] {
            let error = Relay::start(target, "db.example.test", mode, None)
                .await
                .err()
                .unwrap();
            assert_eq!(error.to_string(), "database_tls_unsupported");
        }
        // Nothing answers at all.
        let closed = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let gone = closed.local_addr().unwrap();
        drop(closed);
        let error = Relay::start(gone, "db.example.test", TlsMode::Require, None)
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "database_network_error");
        // A CA file that holds no certificate.
        let directory = tempfile::tempdir().unwrap();
        let empty = directory.path().join("ca.pem");
        std::fs::write(&empty, "not a certificate").unwrap();
        let error = Relay::start(target, "db.example.test", TlsMode::VerifyFull, Some(&empty))
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "mongodb_ca_file_unreadable");
    }
}
