//! The network between the Oracle client library and the server.
//!
//! The library opens its own socket to one host, verifies a certificate only
//! against the bundled roots and only for that host, and cannot be told from
//! another thread to stop a statement. So it never reaches the server
//! directly. It is pointed at a listener on the loopback interface that this
//! module owns, one per connection, and the relay behind that listener
//!
//! * dials the address the driver was given (the server, or the loopback end
//!   of an SSH tunnel) while the host of the profile stays the server name;
//! * does TLS itself, in the three modes of the contract, and once more when
//!   the listener hands the connection over to a server process, which is
//!   when Oracle negotiates TLS a second time on the same socket;
//! * reads the packets of the handshake, to say why a connection was refused
//!   and to refuse what the library cannot do, instead of letting it fail in
//!   a way nobody can name (native network encryption);
//! * follows a redirect of the listener itself (the SCAN listener of a RAC
//!   cluster, a connection manager), with the connect packet the library
//!   would have sent there, so that the redirected connection is encrypted
//!   and stoppable like any other;
//! * slips an interrupt marker between two packets of the client when a
//!   statement has to stop, waits for the server to acknowledge it, and
//!   closes the connection.
//!
//! The plain bytes never leave the loopback socket of this process, which is
//! as exposed as the forwarded port of an SSH tunnel.
use crate::engine::ConnectRequest;
use crate::model::TlsMode;
use anyhow::{Result, anyhow, ensure};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// How long one step of opening a connection may take: the TCP connection,
/// and then each TLS handshake.
const STEP: Duration = Duration::from_secs(10);
/// How long the library gets to connect to the loopback listener.
const ARRIVAL: Duration = Duration::from_secs(15);
/// How long the server gets to acknowledge an interrupt before the
/// connection is closed anyway.
pub(super) const SETTLE: Duration = Duration::from_secs(3);
/// The packets of the handshake are small; anything longer is not one.
const HANDSHAKE_PACKET_LIMIT: usize = 64 * 1024;

// Packet types of Oracle Net (TNS), and the two header flags that matter.
const PACKET_CONNECT: u8 = 1;
const PACKET_ACCEPT: u8 = 2;
const PACKET_REFUSE: u8 = 4;
const PACKET_REDIRECT: u8 = 5;
const PACKET_DATA: u8 = 6;
const PACKET_RESEND: u8 = 11;
const PACKET_MARKER: u8 = 12;
const FLAG_REDIRECT: u8 = 0x04;
const FLAG_TLS_RENEGOTIATION: u8 = 0x08;
/// Where the connect data begins in a connect packet, and the most of it
/// that travels inside the packet: longer data follows in a packet of its
/// own.
const CONNECT_DATA_OFFSET: usize = 74;
const CONNECT_DATA_INLINE: usize = 230;
/// A listener redirects once; a few more are followed before giving up.
const REDIRECT_LIMIT: usize = 3;
/// The first protocol version with packets longer than 64 KiB (Oracle
/// Database 12.1), which is also the oldest one the library speaks.
const VERSION_LARGE_PACKETS: u16 = 315;
/// "Native network encryption is required" in the flags of an accept packet.
const ACCEPT_ENCRYPTION_REQUIRED: u8 = 0x10;
/// A marker packet that asks the server to stop what it is doing, as a
/// client without out-of-band data sends it: 11 bytes, type 12, marker 3.
const INTERRUPT: [u8; 11] = [0, 0, 0, 11, PACKET_MARKER, 0, 0, 0, 1, 0, 3];

/// "Encryption required": the connection is encrypted and the certificate is
/// not checked, as the mode says.
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

/// Why a TLS connection was not made, as a result code.
fn tls_failure(error: &std::io::Error) -> &'static str {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset, UnexpectedEof};
    match error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
    {
        Some(
            rustls::Error::InvalidCertificate(_)
            | rustls::Error::NoCertificatesPresented
            | rustls::Error::UnsupportedNameType,
        ) => "database_certificate_rejected",
        Some(rustls::Error::InvalidMessage(_) | rustls::Error::InappropriateMessage { .. }) => {
            "database_tls_unsupported"
        }
        Some(_) => "database_tls_error",
        // A listener without TLS answers a TLS greeting by hanging up, or
        // not at all.
        None if matches!(
            error.kind(),
            UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe
        ) =>
        {
            "database_tls_unsupported"
        }
        None => "database_network_error",
    }
}

/// How TLS is done for a profile: the verification its mode asks for and
/// the name the certificate has to carry.
struct Tls {
    connector: TlsConnector,
    name: ServerName<'static>,
}

fn tls(mode: TlsMode, ca_file: Option<&std::path::Path>, server_name: &str) -> Result<Option<Tls>> {
    if mode == TlsMode::Disable {
        return Ok(None);
    }
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
                        .map_err(|_| anyhow!("oracle_ca_file_unreadable"))?
                        .flatten();
                    let (added, _) = roots.add_parsable_certificates(certificates);
                    ensure!(added > 0, "oracle_ca_file_unreadable");
                }
                None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
            }
            builder.with_root_certificates(roots).with_no_client_auth()
        }
        _ => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth(),
    };
    let name = ServerName::try_from(server_name.to_owned())
        .map_err(|_| anyhow!("database_endpoint_invalid"))?;
    Ok(Some(Tls {
        connector: TlsConnector::from(Arc::new(config)),
        name,
    }))
}

/// Where the server of a profile is and how to talk to it.
pub(super) struct Endpoint {
    /// Host and port to open the TCP connection to.
    host: String,
    port: u16,
    /// The connection goes through an SSH tunnel: no other address of the
    /// server can be reached.
    tunnelled: bool,
    tls: Option<Arc<Tls>>,
}

impl Endpoint {
    pub(super) fn new(request: &ConnectRequest<'_>) -> Result<Self> {
        let profile = request.profile;
        Ok(Self {
            host: request.dial_host(),
            port: request.dial_port(),
            tunnelled: request.forwarded.is_some(),
            tls: tls(
                profile.tls,
                profile.ca_file.as_deref(),
                request.server_name(),
            )?
            .map(Arc::new),
        })
    }

    async fn dial(&self) -> Result<Wire> {
        dial(&self.host, self.port, self.tls.as_deref()).await
    }
}

async fn dial(host: &str, port: u16, tls: Option<&Tls>) -> Result<Wire> {
    let stream = tokio::time::timeout(STEP, TcpStream::connect((host, port)))
        .await
        .map_err(|_| anyhow!("database_connection_timeout"))?
        .map_err(|_| anyhow!("database_network_error"))?;
    let _ = stream.set_nodelay(true);
    match tls {
        None => Ok(Wire::Plain(stream)),
        Some(tls) => Ok(Wire::Tls(Box::new(handshake(tls, stream).await?))),
    }
}

async fn handshake(tls: &Tls, stream: TcpStream) -> Result<TlsStream<TcpStream>> {
    tokio::time::timeout(STEP, tls.connector.connect(tls.name.clone(), stream))
        .await
        .map_err(|_| anyhow!("database_connection_timeout"))?
        .map_err(|error| anyhow!(tls_failure(&error)))
}

/// The connection to the server, with or without TLS.
enum Wire {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}
impl Wire {
    async fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(stream) => stream.read(buffer).await,
            Wire::Tls(stream) => stream.read(buffer).await,
        }
    }
    async fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        match self {
            Wire::Plain(stream) => stream.write_all(data).await,
            Wire::Tls(stream) => {
                stream.write_all(data).await?;
                stream.flush().await
            }
        }
    }
    /// A new TLS session on the same socket: what the server expects after
    /// the listener handed the connection over.
    async fn renegotiate(self, tls: &Tls) -> Result<Self> {
        let stream = match self {
            Wire::Plain(stream) => stream,
            Wire::Tls(stream) => stream.into_inner().0,
        };
        Ok(Wire::Tls(Box::new(handshake(tls, stream).await?)))
    }
    async fn close(mut self) {
        if let Wire::Tls(stream) = &mut self {
            let _ = tokio::time::timeout(Duration::from_secs(1), stream.shutdown()).await;
        }
    }
}

/// Where a stream of packets is: inside a header, inside the rest of a
/// packet, or between two packets.
#[derive(Default)]
struct Position {
    header: [u8; 8],
    have: usize,
    remaining: usize,
}
impl Position {
    fn between_packets(&self) -> bool {
        self.have == 0 && self.remaining == 0
    }
    /// Follow `chunk` up to the end of a header or of a packet, whichever
    /// comes first. Returns how many bytes that were and, when a header was
    /// completed by them, the type of its packet.
    fn advance(&mut self, chunk: &[u8], large: bool) -> (usize, Option<u8>) {
        if self.remaining > 0 {
            let take = self.remaining.min(chunk.len());
            self.remaining -= take;
            return (take, None);
        }
        let take = (8 - self.have).min(chunk.len());
        self.header[self.have..self.have + take].copy_from_slice(&chunk[..take]);
        self.have += take;
        if self.have < 8 {
            return (take, None);
        }
        let length = if large {
            u32::from_be_bytes([
                self.header[0],
                self.header[1],
                self.header[2],
                self.header[3],
            ]) as usize
        } else {
            usize::from(u16::from_be_bytes([self.header[0], self.header[1]]))
        };
        self.have = 0;
        self.remaining = length.saturating_sub(8);
        (take, Some(self.header[4]))
    }
}

/// The error number a listener gives in a refuse packet: `(ERR=12514)`.
fn refusal_number(packet: &[u8]) -> Option<u32> {
    let start = packet.windows(5).position(|window| window == b"(ERR=")? + 5;
    let digits = packet[start..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    std::str::from_utf8(&packet[start..start + digits])
        .ok()?
        .parse()
        .ok()
}

/// Host and port a listener redirects to, from the address it names:
/// `(ADDRESS=(PROTOCOL=tcp)(HOST=10.0.0.5)(PORT=1521))`.
fn redirect_address(text: &str) -> Option<(String, u16)> {
    let upper = text.to_ascii_uppercase();
    let field = |name: &str| {
        let start = upper.find(&format!("({name}="))? + name.len() + 2;
        let end = start + text[start..].find(')')?;
        Some(text[start..end].trim().to_owned())
    };
    let host = field("HOST").filter(|host| !host.is_empty())?;
    Some((host, field("PORT")?.parse().ok()?))
}

/// The connect packet of the client with other connect data and the flag
/// of a redirect: what the client itself sends to the address a listener
/// redirected it to. Connect data that does not fit into the packet follows
/// in a data packet.
fn redirected_connect(original: &[u8], data: &[u8]) -> Option<Vec<u8>> {
    let mut message = original.get(..CONNECT_DATA_OFFSET)?.to_vec();
    message[5] |= FLAG_REDIRECT;
    message[24..26].copy_from_slice(&u16::try_from(data.len()).ok()?.to_be_bytes());
    if data.len() <= CONNECT_DATA_INLINE {
        let length = u16::try_from(CONNECT_DATA_OFFSET + data.len()).ok()?;
        message[..2].copy_from_slice(&length.to_be_bytes());
        message.extend_from_slice(data);
    } else {
        let length = u16::try_from(CONNECT_DATA_OFFSET).ok()?;
        message[..2].copy_from_slice(&length.to_be_bytes());
        // A data packet: its header, two bytes of flags, the data.
        let length = u16::try_from(10 + data.len()).ok()?;
        message.extend_from_slice(&length.to_be_bytes());
        message.extend_from_slice(&[0, 0, PACKET_DATA, 0, 0, 0, 0, 0]);
        message.extend_from_slice(data);
    }
    Some(message)
}

/// Whether the connect data of a connect packet follows in a packet of its
/// own.
fn connect_data_follows(packet: &[u8]) -> bool {
    packet.get(24..26).is_some_and(|bytes| {
        usize::from(u16::from_be_bytes([bytes[0], bytes[1]])) > CONNECT_DATA_INLINE
    }) && packet.len() <= CONNECT_DATA_OFFSET
}

/// Open the connection a listener redirected to and send it the connect
/// packet of the client with the connect data of the redirect. Returns the
/// connection, how it is encrypted from now on, and that connect data.
async fn follow(
    redirect: &[u8],
    connect: Option<&[u8]>,
    tls: Option<&Arc<Tls>>,
) -> std::result::Result<(Wire, Option<Arc<Tls>>, Vec<u8>), &'static str> {
    const REFUSED: &str = "oracle_redirect_unsupported";
    let split = redirect.iter().position(|byte| *byte == 0).ok_or(REFUSED)?;
    let address = std::str::from_utf8(&redirect[..split]).map_err(|_| REFUSED)?;
    let data = redirect[split + 1..].to_vec();
    let (host, port) = redirect_address(address).ok_or(REFUSED)?;
    let message = connect
        .and_then(|connect| redirected_connect(connect, &data))
        .ok_or(REFUSED)?;
    // The certificate of the other address carries the name of that address.
    let tls = match tls {
        None => None,
        Some(tls) => Some(Arc::new(Tls {
            connector: tls.connector.clone(),
            name: ServerName::try_from(host.clone()).map_err(|_| REFUSED)?,
        })),
    };
    let mut wire = dial(&host, port, tls.as_deref())
        .await
        .map_err(|error| code_of(&error))?;
    wire.write_all(&message)
        .await
        .map_err(|_| "database_network_error")?;
    Ok((wire, tls, data))
}

enum Control {
    /// Ask the server to stop the running statement, then close.
    Interrupt,
    /// Close now.
    Sever,
}

/// What the relay learned about a connection that did not come about.
#[derive(Default)]
struct Verdict {
    /// The relay ended the connection itself, for this reason.
    code: Mutex<Option<&'static str>>,
    /// The listener refused the connection with this error number.
    refused: AtomicU32,
}
impl Verdict {
    fn set(&self, code: &'static str) {
        if let Ok(mut slot) = self.code.lock() {
            slot.get_or_insert(code);
        }
    }
}

/// One connection to the server, as the library sees it: an address on the
/// loopback interface. Dropping it closes the connection, unless an
/// interrupt is under way, which is given [`SETTLE`] to be acknowledged.
pub(super) struct Link {
    address: SocketAddr,
    control: mpsc::UnboundedSender<Control>,
    verdict: Arc<Verdict>,
}

/// The part of a [`Link`] that stops a statement, for the side that waits.
#[derive(Clone)]
pub(super) struct Breaker(mpsc::UnboundedSender<Control>);
impl Breaker {
    /// Stop the statement on the server and close the connection: the
    /// library then fails whatever it was waiting for.
    pub(super) fn interrupt(&self) {
        let _ = self.0.send(Control::Interrupt);
    }
    /// Close the connection without asking the server anything.
    pub(super) fn sever(&self) {
        let _ = self.0.send(Control::Sever);
    }
}

impl Link {
    /// Connect to the server, with TLS when the profile asks for it, and
    /// start listening for the one connection of the library.
    pub(super) async fn open(endpoint: &Endpoint) -> Result<Self> {
        let wire = endpoint.dial().await?;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|_| anyhow!("database_network_error"))?;
        let address = listener
            .local_addr()
            .map_err(|_| anyhow!("database_network_error"))?;
        let (control, commands) = mpsc::unbounded_channel();
        let verdict = Arc::new(Verdict::default());
        tokio::spawn(relay(
            listener,
            wire,
            endpoint.tls.clone(),
            endpoint.tunnelled,
            commands,
            Arc::clone(&verdict),
        ));
        Ok(Self {
            address,
            control,
            verdict,
        })
    }

    /// Where the library connects, without TLS of its own.
    pub(super) fn address(&self) -> SocketAddr {
        self.address
    }

    pub(super) fn breaker(&self) -> Breaker {
        Breaker(self.control.clone())
    }

    /// Why the relay ended the connection, when it did.
    pub(super) fn verdict(&self) -> Option<&'static str> {
        self.verdict.code.lock().ok().and_then(|slot| *slot)
    }

    /// The error number of the listener that refused the connection.
    pub(super) fn refused(&self) -> Option<u32> {
        Some(self.verdict.refused.load(Ordering::Acquire)).filter(|number| *number > 0)
    }
}

/// Whether the server is being asked to stop.
enum Interrupt {
    No,
    /// As soon as the client is between two packets.
    Wanted,
    /// The marker went out; the connection is closed at this moment at the
    /// latest.
    Sent(Instant),
}

async fn relay(
    listener: TcpListener,
    mut wire: Wire,
    mut tls: Option<Arc<Tls>>,
    tunnelled: bool,
    mut commands: mpsc::UnboundedReceiver<Control>,
    verdict: Arc<Verdict>,
) {
    // One client, and soon: nobody else is meant to find this port.
    let Ok(Ok((mut client, _))) = tokio::time::timeout(ARRIVAL, listener.accept()).await else {
        wire.close().await;
        return;
    };
    drop(listener);
    let _ = client.set_nodelay(true);

    // Until the server accepted the connection, the packets of both sides
    // are read whole.
    let mut accepted = false;
    let mut greeting: Vec<u8> = Vec::new();
    let mut hello: Vec<u8> = Vec::new();
    // The connect packet of the client, as it last sent it.
    let mut connect: Option<Vec<u8>> = None;
    // After a redirect: the connect data the other address was given, which
    // stands for the one of the client whenever it sends its packet again.
    let mut redirected: Option<Vec<u8>> = None;
    let mut redirects = 0usize;
    // A redirect was announced and its data comes in the next packet.
    let mut awaited = false;
    // The data packet of the client that belonged to a replaced connect
    // packet is not passed on.
    let mut skip_data = false;
    let mut outgoing = Position::default();
    let mut incoming = Position::default();
    let mut interrupt = Interrupt::No;
    let mut listening = true;
    let mut from_client = vec![0u8; 64 * 1024];
    let mut from_server = vec![0u8; 64 * 1024];
    'pump: loop {
        let deadline = match interrupt {
            Interrupt::Sent(at) => Some(at),
            _ => None,
        };
        tokio::select! {
            command = commands.recv(), if listening => match command {
                Some(Control::Interrupt) if accepted => {
                    if matches!(interrupt, Interrupt::No) {
                        interrupt = Interrupt::Wanted;
                    }
                }
                // Nothing runs before the sign-in is over.
                Some(Control::Interrupt | Control::Sever) => break 'pump,
                // The driver let go of the connection.
                None => {
                    listening = false;
                    if matches!(interrupt, Interrupt::No) {
                        break 'pump;
                    }
                }
            },
            () = async { tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)).await }, if deadline.is_some() => {
                break 'pump;
            }
            read = client.read(&mut from_client) => {
                let Ok(count @ 1..) = read else { break 'pump };
                if !accepted {
                    hello.extend_from_slice(&from_client[..count]);
                    while hello.len() >= 8 {
                        let length = usize::from(u16::from_be_bytes([hello[0], hello[1]]));
                        if !(8..=HANDSHAKE_PACKET_LIMIT).contains(&length) {
                            break 'pump;
                        }
                        if hello.len() < length {
                            break;
                        }
                        let packet: Vec<u8> = hello.drain(..length).collect();
                        let sent = match packet[4] {
                            PACKET_CONNECT => {
                                let replaced = redirected
                                    .as_ref()
                                    .and_then(|data| redirected_connect(&packet, data));
                                skip_data = replaced.is_some() && connect_data_follows(&packet);
                                let sent = match &replaced {
                                    Some(message) => wire.write_all(message).await,
                                    None => wire.write_all(&packet).await,
                                };
                                connect = Some(packet);
                                sent
                            }
                            PACKET_DATA if std::mem::take(&mut skip_data) => Ok(()),
                            _ => wire.write_all(&packet).await,
                        };
                        if sent.is_err() {
                            break 'pump;
                        }
                    }
                    continue 'pump;
                }
                // The bytes go on as they came; they are only followed
                // closely enough to know where a packet ends, which is
                // where an interrupt may go.
                let (mut offset, mut sent) = (0, 0);
                while offset < count {
                    let (used, _) = outgoing.advance(&from_client[offset..count], accepted);
                    offset += used;
                    if matches!(interrupt, Interrupt::Wanted) && outgoing.between_packets() {
                        if wire.write_all(&from_client[sent..offset]).await.is_err()
                            || wire.write_all(&INTERRUPT).await.is_err()
                        {
                            break 'pump;
                        }
                        sent = offset;
                        interrupt = Interrupt::Sent(Instant::now() + SETTLE);
                    }
                }
                if sent < count && wire.write_all(&from_client[sent..count]).await.is_err() {
                    break 'pump;
                }
            }
            read = wire.read(&mut from_server) => {
                let Ok(count @ 1..) = read else { break 'pump };
                if accepted {
                    // The answer to an interrupt is a marker packet. The
                    // statement is over then, and so is this connection:
                    // the library is not told, it finds the socket closed.
                    let mut offset = 0;
                    let mut marker = None;
                    while offset < count {
                        let (used, started) = incoming.advance(&from_server[offset..count], true);
                        offset += used;
                        if started == Some(PACKET_MARKER) && matches!(interrupt, Interrupt::Sent(_)) {
                            marker = Some(offset.saturating_sub(8));
                            break;
                        }
                    }
                    let end = marker.unwrap_or(count);
                    if client.write_all(&from_server[..end]).await.is_err() || marker.is_some() {
                        break 'pump;
                    }
                    continue 'pump;
                }
                greeting.extend_from_slice(&from_server[..count]);
                while greeting.len() >= 8 {
                    let length = usize::from(u16::from_be_bytes([greeting[0], greeting[1]]));
                    if !(8..=HANDSHAKE_PACKET_LIMIT).contains(&length) {
                        break 'pump;
                    }
                    if greeting.len() < length {
                        break;
                    }
                    let mut packet: Vec<u8> = greeting.drain(..length).collect();
                    // A redirect names another address. The library would
                    // go there with a socket of its own: past the tunnel,
                    // without TLS and without a way to stop a statement. So
                    // the relay goes there itself, and the library never
                    // learns that it was sent elsewhere.
                    let inline = packet[4] == PACKET_REDIRECT && packet.len() > 10;
                    if packet[4] == PACKET_REDIRECT && !inline {
                        awaited = true;
                        continue;
                    }
                    if inline || (packet[4] == PACKET_DATA && std::mem::take(&mut awaited)) {
                        redirects += 1;
                        if tunnelled {
                            verdict.set("oracle_redirect_through_tunnel");
                            break 'pump;
                        }
                        if redirects > REDIRECT_LIMIT {
                            verdict.set("oracle_redirect_unsupported");
                            break 'pump;
                        }
                        match follow(&packet[10..], connect.as_deref(), tls.as_ref()).await {
                            Ok((other, encryption, data)) => {
                                std::mem::replace(&mut wire, other).close().await;
                                tls = encryption;
                                redirected = Some(data);
                                greeting.clear();
                            }
                            Err(code) => {
                                verdict.set(code);
                                break 'pump;
                            }
                        }
                        continue;
                    }
                    match packet[4] {
                        PACKET_ACCEPT => {
                            let version = packet
                                .get(8..10)
                                .map_or(0, |bytes| u16::from_be_bytes([bytes[0], bytes[1]]));
                            if version < VERSION_LARGE_PACKETS {
                                verdict.set("oracle_server_too_old");
                                break 'pump;
                            }
                            // The library does not encrypt in the way of
                            // Oracle Net itself and would not survive
                            // being asked to.
                            if packet.get(22).is_some_and(|flags| flags & ACCEPT_ENCRYPTION_REQUIRED != 0) {
                                verdict.set("oracle_native_encryption_required");
                                break 'pump;
                            }
                            accepted = true;
                        }
                        PACKET_REFUSE => {
                            if let Some(number) = refusal_number(&packet) {
                                verdict.refused.store(number, Ordering::Release);
                            }
                        }
                        PACKET_RESEND if packet[5] & FLAG_TLS_RENEGOTIATION != 0 => {
                            if let Some(tls) = &tls {
                                wire = match wire.renegotiate(tls).await {
                                    Ok(wire) => wire,
                                    Err(error) => {
                                        verdict.set(code_of(&error));
                                        return;
                                    }
                                };
                            }
                            // Not the business of the library, which speaks
                            // plain TCP to this relay.
                            packet[5] &= !FLAG_TLS_RENEGOTIATION;
                        }
                        _ => {}
                    }
                    if client.write_all(&packet).await.is_err() {
                        break 'pump;
                    }
                    if accepted {
                        // What follows an accept packet is no longer read.
                        let rest = std::mem::take(&mut greeting);
                        if !rest.is_empty() && client.write_all(&rest).await.is_err() {
                            break 'pump;
                        }
                        // The client answers an accept with a new packet.
                        outgoing = Position::default();
                    }
                }
            }
        }
        if matches!(interrupt, Interrupt::Wanted) && outgoing.between_packets() {
            if wire.write_all(&INTERRUPT).await.is_err() {
                break 'pump;
            }
            interrupt = Interrupt::Sent(Instant::now() + SETTLE);
        }
    }
    wire.close().await;
}

/// The stable code an error of this module carries.
fn code_of(error: &anyhow::Error) -> &'static str {
    match error.to_string().as_str() {
        "oracle_redirect_unsupported" => "oracle_redirect_unsupported",
        "database_certificate_rejected" => "database_certificate_rejected",
        "database_tls_unsupported" => "database_tls_unsupported",
        "database_connection_timeout" => "database_connection_timeout",
        "database_network_error" => "database_network_error",
        _ => "database_tls_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_are_followed_through_pieces_of_any_size() {
        // An 11-byte packet, then one of 8 bytes, then the start of a third.
        let stream = [
            &[0, 11, 0, 0, 6, 0, 0, 0, 1, 2, 3][..],
            &[0, 8, 0, 0, 12, 0, 0, 0],
            &[0, 20, 0, 0],
        ]
        .concat();
        for size in 1..=stream.len() {
            let mut position = Position::default();
            let mut types = Vec::new();
            let mut boundaries = Vec::new();
            let mut offset = 0;
            for piece in stream.chunks(size) {
                let mut piece = piece;
                while !piece.is_empty() {
                    let (used, started) = position.advance(piece, false);
                    assert!(used > 0);
                    types.extend(started);
                    offset += used;
                    piece = &piece[used..];
                    if position.between_packets() {
                        boundaries.push(offset);
                    }
                }
            }
            assert_eq!(types, [6, 12], "pieces of {size}");
            assert_eq!(boundaries, [11, 19], "pieces of {size}");
            assert!(!position.between_packets());
        }
        // After the accept packet the length takes four bytes.
        let mut position = Position::default();
        let large = [0, 1, 0, 0, 6, 0, 0, 0];
        assert_eq!(position.advance(&large, true), (8, Some(6)));
        assert_eq!(position.remaining, 65536 - 8);
    }

    #[test]
    fn the_error_number_of_a_listener_is_read_from_its_refusal() {
        let refusal = b"\0\x61\0\0\x04\0\0\0\"\0\0\x55(DESCRIPTION=(TMP=)(VSNNUM=0)(ERR=12514)(ERROR_STACK=(ERROR=(CODE=12514)(EMFI=4))))";
        assert_eq!(refusal_number(refusal), Some(12514));
        assert_eq!(refusal_number(b"(ERR=)"), None);
        assert_eq!(refusal_number(b"nothing"), None);
    }

    fn request_for(tls: TlsMode, ca_file: Option<std::path::PathBuf>) -> Result<Endpoint> {
        let profile = crate::model::ConnectionProfile {
            name: "Owned fixture".into(),
            engine: super::super::ENGINE,
            host: "db.example.test".into(),
            port: 1521,
            tls,
            ca_file,
            ..Default::default()
        };
        Endpoint::new(&ConnectRequest {
            product: profile.product_spec(),
            profile: &profile,
            forwarded: Some("127.0.0.1:4000".parse().unwrap()),
        })
    }

    #[test]
    fn the_address_to_dial_and_the_name_of_the_server_are_two_things() {
        let endpoint = request_for(TlsMode::VerifyFull, None).unwrap();
        assert_eq!((endpoint.host.as_str(), endpoint.port), ("127.0.0.1", 4000));
        assert!(endpoint.tunnelled);
        let tls = endpoint.tls.unwrap();
        assert_eq!(tls.name, ServerName::try_from("db.example.test").unwrap());
        assert!(request_for(TlsMode::Disable, None).unwrap().tls.is_none());
        assert!(request_for(TlsMode::Require, None).unwrap().tls.is_some());
        // A CA file that holds no certificate, and one that is not there.
        let directory = tempfile::tempdir().unwrap();
        let empty = directory.path().join("ca.pem");
        std::fs::write(&empty, "not a certificate").unwrap();
        for path in [empty, directory.path().join("missing.pem")] {
            let error = request_for(TlsMode::VerifyFull, Some(path)).err().unwrap();
            assert_eq!(error.to_string(), "oracle_ca_file_unreadable");
        }
    }

    /// A server that speaks just enough of the handshake: it reads one
    /// packet and answers with the bytes it was given.
    async fn answering(answer: Vec<u8>) -> SocketAddr {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let answer = answer.clone();
                tokio::spawn(async move {
                    let mut header = [0u8; 8];
                    if stream.read_exact(&mut header).await.is_err() {
                        return;
                    }
                    let length = usize::from(u16::from_be_bytes([header[0], header[1]]));
                    let mut rest = vec![0u8; length.saturating_sub(8)];
                    if stream.read_exact(&mut rest).await.is_err() {
                        return;
                    }
                    let _ = stream.write_all(&answer).await;
                    // Whatever the client says next is sent back, so that a
                    // test can see what reached the server.
                    let mut buffer = [0u8; 256];
                    while let Ok(count @ 1..) = stream.read(&mut buffer).await {
                        if stream.write_all(&buffer[..count]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        address
    }

    fn plain(address: SocketAddr, tunnelled: bool) -> Endpoint {
        Endpoint {
            host: address.ip().to_string(),
            port: address.port(),
            tunnelled,
            tls: None,
        }
    }

    fn packet(kind: u8, flags: u8, body: &[u8]) -> Vec<u8> {
        let length = u16::try_from(8 + body.len()).unwrap();
        let mut packet = length.to_be_bytes().to_vec();
        packet.extend([0, 0, kind, flags, 0, 0]);
        packet.extend(body);
        packet
    }

    fn accept(version: u16, flags: u8) -> Vec<u8> {
        let mut body = version.to_be_bytes().to_vec();
        body.extend([0u8; 12]);
        body.push(flags);
        body.extend([0u8; 30]);
        packet(PACKET_ACCEPT, 0, &body)
    }

    async fn exchange(link: &Link, request: &[u8]) -> Vec<u8> {
        let mut client = TcpStream::connect(link.address()).await.unwrap();
        client.write_all(request).await.unwrap();
        let mut answer = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut answer)).await;
        answer
    }

    #[tokio::test]
    async fn what_the_library_cannot_do_is_refused_with_a_reason() {
        let connect = packet(1, 0, b"(DESCRIPTION=...)");
        // A server older than Oracle Database 12.1.
        let link = Link::open(&plain(answering(accept(314, 0)).await, false))
            .await
            .unwrap();
        assert!(exchange(&link, &connect).await.is_empty());
        assert_eq!(link.verdict(), Some("oracle_server_too_old"));
        // Native network encryption that the server insists on.
        let link = Link::open(&plain(
            answering(accept(319, ACCEPT_ENCRYPTION_REQUIRED)).await,
            false,
        ))
        .await
        .unwrap();
        assert!(exchange(&link, &connect).await.is_empty());
        assert_eq!(link.verdict(), Some("oracle_native_encryption_required"));
        // A redirect cannot be followed through a tunnel, which leads to
        // one address only.
        let redirect = [
            packet(PACKET_REDIRECT, 0, &[0, 40]),
            packet(
                PACKET_DATA,
                0,
                b"\0\0(ADDRESS=(PROTOCOL=tcp)(HOST=127.0.0.1)(PORT=1))\0(DESCRIPTION=)",
            ),
        ]
        .concat();
        let link = Link::open(&plain(answering(redirect.clone()).await, true))
            .await
            .unwrap();
        assert!(exchange(&link, &connect).await.is_empty());
        assert_eq!(link.verdict(), Some("oracle_redirect_through_tunnel"));
        // One that names no address is no redirect to follow.
        let nowhere = [
            packet(PACKET_REDIRECT, 0, &[0, 9]),
            packet(PACKET_DATA, 0, b"\0\0(ADDRESS=)\0x"),
        ]
        .concat();
        let link = Link::open(&plain(answering(nowhere).await, false))
            .await
            .unwrap();
        assert!(exchange(&link, &connect).await.is_empty());
        assert_eq!(link.verdict(), Some("oracle_redirect_unsupported"));
        // A refusal is passed on, and its number kept.
        let refusal = packet(
            PACKET_REFUSE,
            0,
            b"\x22\0\0\x30(DESCRIPTION=(ERR=12514)(VSNNUM=0))",
        );
        let link = Link::open(&plain(answering(refusal.clone()).await, false))
            .await
            .unwrap();
        let mut client = TcpStream::connect(link.address()).await.unwrap();
        client.write_all(&connect).await.unwrap();
        let mut answer = vec![0u8; refusal.len()];
        client.read_exact(&mut answer).await.unwrap();
        assert_eq!(answer, refusal);
        assert_eq!((link.verdict(), link.refused()), (None, Some(12514)));
        // Nothing listens: the reason is known before the library is asked.
        let closed = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let gone = closed.local_addr().unwrap();
        drop(closed);
        let error = Link::open(&plain(gone, false)).await.err().unwrap();
        assert_eq!(error.to_string(), "database_network_error");
    }

    #[test]
    fn a_redirect_is_read_and_answered_like_the_client_would() {
        assert_eq!(
            redirect_address("(ADDRESS=(PROTOCOL=tcp)(HOST=10.0.0.5)(PORT=1521))"),
            Some(("10.0.0.5".into(), 1521))
        );
        assert_eq!(
            redirect_address("(address=(protocol=TCPS)(host= db-2.example.test )(port=2484))"),
            Some(("db-2.example.test".into(), 2484))
        );
        for bad in [
            "(ADDRESS=(HOST=x))",
            "(ADDRESS=(HOST=)(PORT=1))",
            "(HOST=x)(PORT=y)",
            "",
        ] {
            assert_eq!(redirect_address(bad), None, "{bad}");
        }
        // A connect packet: 66 bytes of fields after its header, then data.
        let mut original = vec![0u8; CONNECT_DATA_OFFSET];
        original[..2].copy_from_slice(&(74u16 + 5).to_be_bytes());
        original[4] = PACKET_CONNECT;
        original[8..10].copy_from_slice(&319u16.to_be_bytes());
        original[24..26].copy_from_slice(&5u16.to_be_bytes());
        original.extend_from_slice(b"first");
        let short = redirected_connect(&original, b"(DESCRIPTION=other)").unwrap();
        assert_eq!(short.len(), 74 + 19);
        assert_eq!(u16::from_be_bytes([short[0], short[1]]), 74 + 19);
        assert_eq!((short[4], short[5]), (PACKET_CONNECT, FLAG_REDIRECT));
        assert_eq!(u16::from_be_bytes([short[24], short[25]]), 19);
        assert_eq!(short[8..24], original[8..24]);
        assert_eq!(short[26..74], original[26..74]);
        assert_eq!(&short[74..], b"(DESCRIPTION=other)");
        assert!(!connect_data_follows(&short));
        // Data that does not fit travels in a packet of its own.
        let data = vec![b'x'; 231];
        let long = redirected_connect(&original, &data).unwrap();
        assert_eq!(u16::from_be_bytes([long[0], long[1]]), 74);
        assert_eq!(u16::from_be_bytes([long[24], long[25]]), 231);
        assert!(connect_data_follows(&long[..74]));
        assert_eq!(long[74..84], [0, 241, 0, 0, PACKET_DATA, 0, 0, 0, 0, 0]);
        assert_eq!(&long[84..], data.as_slice());
        assert!(redirected_connect(&original[..40], b"x").is_none());
    }

    /// Read what a client sends first: its connect packet, and the packet
    /// with the connect data when that did not fit.
    async fn first_message(stream: &mut TcpStream) -> Option<Vec<u8>> {
        let mut message = Vec::new();
        loop {
            let mut header = [0u8; 8];
            stream.read_exact(&mut header).await.ok()?;
            let length = usize::from(u16::from_be_bytes([header[0], header[1]]));
            let mut rest = vec![0u8; length.checked_sub(8)?];
            stream.read_exact(&mut rest).await.ok()?;
            let start = message.len();
            message.extend_from_slice(&header);
            message.extend_from_slice(&rest);
            if header[4] != PACKET_CONNECT || !connect_data_follows(&message[start..]) {
                return Some(message);
            }
        }
    }

    /// Two listeners: the first redirects every connection to the second,
    /// which reports the first message it is sent and then refuses.
    async fn redirecting(data: &'static str) -> (SocketAddr, mpsc::UnboundedReceiver<Vec<u8>>) {
        let second = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let target = second.local_addr().unwrap();
        let (seen, messages) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = second.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    if let Some(message) = first_message(&mut stream).await {
                        let _ = seen.send(message);
                    }
                    let refusal = packet(
                        PACKET_REFUSE,
                        0,
                        b"\x22\0\0\x23(DESCRIPTION=(ERR=12514)(VSNNUM=0))",
                    );
                    let _ = stream.write_all(&refusal).await;
                    let mut rest = [0u8; 64];
                    let _ = stream.read(&mut rest).await;
                });
            }
        });
        let first = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = first.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = first.accept().await {
                tokio::spawn(async move {
                    if first_message(&mut stream).await.is_none() {
                        return;
                    }
                    let text = format!(
                        "(ADDRESS=(PROTOCOL=tcp)(HOST=127.0.0.1)(PORT={}))\0{data}",
                        target.port()
                    );
                    let length = u16::try_from(text.len()).unwrap().to_be_bytes();
                    let mut body = vec![0u8, 0];
                    body.extend_from_slice(text.as_bytes());
                    let answer = [
                        packet(PACKET_REDIRECT, 0, &length),
                        packet(PACKET_DATA, 0, &body),
                    ]
                    .concat();
                    let _ = stream.write_all(&answer).await;
                    let mut rest = [0u8; 64];
                    let _ = stream.read(&mut rest).await;
                });
            }
        });
        (address, messages)
    }

    /// What the client library sends to `address` when it is told to
    /// connect there, as an error: the listeners of the test refuse.
    async fn library_connects(address: SocketAddr) -> String {
        tokio::task::spawn_blocking(move || {
            let config = oracledb::Config::default()
                .set_credentials("nobody", "nothing")
                .set_connect_string(&format!(
                    "(DESCRIPTION=(ADDRESS=(PROTOCOL=tcp)(HOST={})(PORT={}))(CONNECT_DATA=(SERVICE_NAME=first)))",
                    address.ip(),
                    address.port()
                ))
                .map_err(|error| error.to_string())?;
            oracledb::connect(config)
                .map(|_| ())
                .map_err(|error| error.to_string())
        })
        .await
        .unwrap()
        .expect_err("the listener of the test refuses")
    }

    /// The relay follows a redirect with exactly the bytes the client
    /// library sends when it follows one itself.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_redirect_is_followed_the_way_the_library_follows_it() {
        for data in [
            "(DESCRIPTION=(CONNECT_DATA=(SERVICE_NAME=second)(INSTANCE_NAME=node2)))",
            // Longer than a connect packet holds: it travels on its own.
            "(DESCRIPTION=(CONNECT_DATA=(SERVICE_NAME=second.a-long-domain-name.example.test)(INSTANCE_NAME=node2)(SERVER=DEDICATED)(CID=(PROGRAM=a-client-with-a-rather-long-name)(HOST=a-host-with-a-rather-long-name.example.test)(USER=somebody)))(CONNECTION_ID=abcdefghijklmnopqrstuv==)(EXTRA=0123456789012345678901234567890123456789))",
        ] {
            assert_eq!(data.len() > CONNECT_DATA_INLINE, data.contains("EXTRA"));
            // The library on its own.
            let (first, mut messages) = redirecting(data).await;
            library_connects(first).await;
            let direct = messages.recv().await.expect("a message of the library");
            // The library through the relay, which is told nothing.
            let (first, mut messages) = redirecting(data).await;
            let link = Link::open(&plain(first, false)).await.unwrap();
            library_connects(link.address()).await;
            let relayed = messages.recv().await.expect("a message of the relay");
            assert_eq!(relayed, direct, "{} bytes of connect data", data.len());
            assert_eq!((relayed[4], relayed[5]), (PACKET_CONNECT, FLAG_REDIRECT));
            assert!(relayed.ends_with(data.as_bytes()));
            // The refusal of the second listener reached the library.
            assert_eq!((link.verdict(), link.refused()), (None, Some(12514)));
        }
    }

    #[tokio::test]
    async fn an_interrupt_goes_out_between_two_packets_of_the_client() {
        let link = Link::open(&plain(answering(accept(319, 0)).await, false))
            .await
            .unwrap();
        let mut client = TcpStream::connect(link.address()).await.unwrap();
        client.write_all(&packet(1, 0, b"connect")).await.unwrap();
        let mut accepted = vec![0u8; accept(319, 0).len()];
        client.read_exact(&mut accepted).await.unwrap();
        // Half of a packet with a four-byte length, an interrupt, the rest:
        // the marker has to follow the whole packet. The test server sends
        // everything back.
        let request = [0, 0, 0, 14, 6, 0, 0, 0, 0, 0, 9, 9, 9, 9];
        client.write_all(&request[..5]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        link.breaker().interrupt();
        tokio::time::sleep(Duration::from_millis(50)).await;
        client.write_all(&request[5..]).await.unwrap();
        // The echo of the request arrives; the echo of the marker is the
        // acknowledgement, which ends the connection without being passed on.
        let mut seen = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut seen))
            .await
            .unwrap()
            .ok();
        assert_eq!(seen, request);
        assert_eq!(link.verdict(), None);
    }

    #[tokio::test]
    async fn a_server_without_tls_is_reported_as_such() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                drop(stream);
            }
        });
        for mode in [TlsMode::Require, TlsMode::VerifyFull] {
            let endpoint = Endpoint {
                tls: tls(mode, None, "db.example.test").unwrap().map(Arc::new),
                ..plain(address, false)
            };
            let error = Link::open(&endpoint).await.err().unwrap();
            assert_eq!(error.to_string(), "database_tls_unsupported");
        }
    }
}
