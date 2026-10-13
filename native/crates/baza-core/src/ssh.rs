//! Pinned native SSH forwarding. No shell, agent or known-host auto-add.
use crate::model::{SshAuth, SshProfile, TunnelProfile};
use anyhow::{Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
use russh::{
    AlgorithmKind, ChannelOpenFailure, MethodKind,
    client::{self, AuthResult},
    keys::{Algorithm, HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    future::Future,
    io::ErrorKind,
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};
static KEY_ACTIVE: AtomicU32 = AtomicU32::new(0);
struct KeySlot;
impl Drop for KeySlot {
    fn drop(&mut self) {
        KEY_ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// One TCP connection attempt to the gateway, per resolved address.
const CONNECT: Duration = Duration::from_secs(10);
/// Key exchange and authentication together, including host key retries.
const HANDSHAKE: Duration = Duration::from_secs(30);
const SCAN: Duration = Duration::from_secs(15);
const CHANNEL: Duration = Duration::from_secs(15);
/// Keepalives detect a dead gateway; an idle tunnel is never closed by us.
const KEEPALIVE: Duration = Duration::from_secs(30);
/// Silence allowed in the direction that is still open after the other one
/// ended. Connections that are open in both directions have no idle limit.
const HALF_CLOSED_IDLE: Duration = Duration::from_secs(120);
pub const MAX_TUNNELS: usize = 8;
/// Forwarded connections of one manager, across all of its tunnels.
pub const MAX_CLIENTS: usize = 256;
/// Forwarded connections of one tunnel. Database tools open pools of them.
pub const MAX_TUNNEL_CLIENTS: u32 = 32;

type Code = &'static str;
const CHANGED: Code = "ssh_host_key_changed";
const NO_SUCH_HOST_KEY: Code = "ssh_host_key_algorithms_unsupported";
/// Internal marker: the scan stopped the handshake on purpose.
const SCANNED: Code = "ssh_host_key_scanned";

// Own the asynchronous clone before spawning, so cancellation of even an
// unpolled startup future wipes first-party credential copies.
struct PrivateTunnel(TunnelProfile);
impl std::ops::Deref for PrivateTunnel {
    type Target = TunnelProfile;
    fn deref(&self) -> &TunnelProfile {
        &self.0
    }
}
impl PrivateTunnel {
    fn clear_auth(&mut self) {
        self.0.ssh.password.zeroize();
        self.0.ssh.private_key.zeroize();
        self.0.ssh.passphrase.zeroize();
    }
}
impl Drop for PrivateTunnel {
    fn drop(&mut self) {
        self.clear_auth();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TunnelStatus {
    pub id: Uuid,
    /// `starting`, `active`, `stopping`, `stopped`, `failed` or `disconnected`.
    pub state: String,
    pub local_port: Option<u16>,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub active_clients: u32,
    /// Stable code, see [`describe`]. While the tunnel is `active` this is the
    /// reason the most recent forwarded connection could not be opened.
    pub error: Option<String>,
    /// Host key the server proved in the latest attempt. For an `active` tunnel
    /// whose profile had no fingerprint yet, this is the key that was trusted
    /// on first use and should now be saved into the profile. With the error
    /// `ssh_host_key_changed` it is the new key the server presents instead of
    /// the saved one.
    #[serde(default)]
    pub host_key: Option<HostKeyScan>,
}
struct Entry {
    status: Mutex<TunnelStatus>,
    cancel: CancellationToken,
    sent: AtomicU64,
    received: AtomicU64,
    clients: AtomicU32,
    done: tokio::sync::Notify,
}
impl Entry {
    fn view(&self) -> TunnelStatus {
        let mut value = self.status.lock().unwrap().clone();
        value.bytes_sent = self.sent.load(Ordering::Relaxed);
        value.bytes_received = self.received.load(Ordering::Relaxed);
        value.active_clients = self.clients.load(Ordering::Relaxed);
        value
    }
    fn state(&self, state: &str, error: Option<String>, port: Option<u16>) {
        let mut status = self.status.lock().unwrap();
        status.state = state.into();
        status.error = error;
        status.local_port = port;
    }
    fn host_key(&self, key: Option<HostKeyScan>) {
        self.status.lock().unwrap().host_key = key;
    }
    /// Record or clear a per-connection problem without changing the state.
    fn note(&self, problem: Option<Code>) {
        let mut status = self.status.lock().unwrap();
        if status.state == "active" {
            status.error = problem.map(str::to_owned);
        }
    }
}
struct Inner {
    entries: Mutex<HashMap<Uuid, Arc<Entry>>>,
    clients: Arc<Semaphore>,
    shutdown: CancellationToken,
}
impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown.cancel();
        for entry in self.entries.get_mut().unwrap().values() {
            entry.cancel.cancel();
        }
    }
}
#[derive(Clone)]
pub struct TunnelManager {
    inner: Arc<Inner>,
}

pub fn validate_host(host: &str) -> Result<()> {
    ensure!(
        !host.is_empty() && host.len() <= 253 && host == host.trim(),
        "host_invalid"
    );
    ensure!(
        !host
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || "/\\@?#%,;".contains(c)),
        "host_invalid"
    );
    if let Ok(ip) = host.parse::<IpAddr>() {
        ensure!(!ip.is_unspecified() && !ip.is_multicast(), "host_invalid");
    } else {
        ensure!(
            host.split('.').all(|part| !part.is_empty()
                && part.len() <= 63
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')),
            "host_invalid"
        );
    }
    Ok(())
}
pub fn validate_ssh(profile: &SshProfile, credentials: bool) -> Result<()> {
    validate_host(&profile.host)?;
    ensure!(profile.port > 0, "ssh_port_invalid");
    ensure!(
        !profile.username.is_empty()
            && profile.username.len() <= 128
            && !profile
                .username
                .chars()
                .any(|c| c.is_control() || c.is_whitespace()),
        "ssh_username_invalid"
    );
    // An empty fingerprint means the server key is trusted on first use.
    if !profile.fingerprint.is_empty() {
        let encoded = profile
            .fingerprint
            .strip_prefix("SHA256:")
            .ok_or_else(|| anyhow!("ssh_pin_invalid"))?;
        let raw = STANDARD_NO_PAD
            .decode(encoded)
            .map_err(|_| anyhow!("ssh_pin_invalid"))?;
        ensure!(
            raw.len() == 32 && STANDARD_NO_PAD.encode(&raw) == encoded,
            "ssh_pin_invalid"
        );
    }
    for (value, limit) in [
        (&profile.password, 8192),
        (&profile.private_key, 65536),
        (&profile.passphrase, 8192),
    ] {
        ensure!(
            value
                .as_ref()
                .is_none_or(|v| v.len() <= limit && !v.contains('\0')),
            "ssh_credential_invalid"
        );
    }
    if credentials {
        match profile.auth {
            SshAuth::Password => ensure!(
                profile.password.as_ref().is_some_and(|v| !v.is_empty()),
                "ssh_password_required"
            ),
            SshAuth::PrivateKey => ensure!(
                profile.private_key.as_ref().is_some_and(|v| !v.is_empty()),
                "ssh_private_key_required"
            ),
        }
    }
    Ok(())
}
pub fn validate_tunnel(profile: &TunnelProfile, credentials: bool) -> Result<()> {
    ensure!(
        !profile.name.trim().is_empty()
            && profile.name.len() <= 128
            && !profile.name.chars().any(char::is_control),
        "tunnel_name_invalid"
    );
    validate_host(&profile.target_host)?;
    ensure!(
        profile.target_port > 0 && (profile.local_port == 0 || profile.local_port >= 1024),
        "tunnel_port_invalid"
    );
    validate_ssh(&profile.ssh, credentials)
}
/// Host key presented by a server during key exchange.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostKeyScan {
    /// `SHA256:…` exactly as [`validate_ssh`] and the profile field expect it.
    pub fingerprint: String,
    /// For example `ssh-ed25519`, `ecdsa-sha2-nistp256` or `ssh-rsa`.
    pub algorithm: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyFamily {
    Ed25519,
    Ecdsa,
    Rsa,
}
impl KeyFamily {
    const ALL: [Self; 3] = [Self::Ed25519, Self::Ecdsa, Self::Rsa];
    fn of(algorithm: &Algorithm) -> Option<Self> {
        match algorithm {
            Algorithm::Ed25519 => Some(Self::Ed25519),
            Algorithm::Ecdsa { .. } => Some(Self::Ecdsa),
            Algorithm::Rsa { .. } => Some(Self::Rsa),
            _ => None,
        }
    }
}
#[derive(Clone)]
struct Presented {
    scan: HostKeyScan,
    family: Option<KeyFamily>,
}

/// Error of [`TunnelManager::start`] when the server presents a key other than
/// the saved fingerprint. Its text is the code `ssh_host_key_changed`; the new
/// key is carried so a caller can offer an explicit "trust the new key" action.
/// A saved fingerprint is never replaced automatically.
#[derive(Clone, Debug)]
pub struct HostKeyChanged {
    pub presented: HostKeyScan,
}
impl std::fmt::Display for HostKeyChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(CHANGED)
    }
}
impl std::error::Error for HostKeyChanged {}

/// What a connection does with the key the server proves.
#[derive(Clone)]
enum Trust {
    /// Record the key and stop before authentication.
    Scan,
    /// Accept only the saved fingerprint.
    Pinned(String),
    /// No fingerprint saved yet: accept the key and report it for saving.
    FirstUse,
}
impl Trust {
    fn of(profile: &SshProfile) -> Self {
        if profile.fingerprint.is_empty() {
            Self::FirstUse
        } else {
            Self::Pinned(profile.fingerprint.clone())
        }
    }
}

/// Why our own host key policy stopped the handshake.
#[derive(Debug)]
enum Stop {
    Ssh(russh::Error),
    Scanned,
    Mismatch,
    Certificate,
    WeakKey,
}
impl From<russh::Error> for Stop {
    fn from(error: russh::Error) -> Self {
        Self::Ssh(error)
    }
}

struct Gate {
    trust: Trust,
    presented: Arc<Mutex<Option<Presented>>>,
}
impl client::Handler for Gate {
    type Error = Stop;
    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        // Host certificates require a separate explicit CA trust policy. A raw
        // host-key pin never silently authorizes a certificate identity.
        if key.certificate().is_some() {
            return Err(Stop::Certificate);
        }
        let public = key.public_key();
        if public
            .key_data()
            .rsa()
            .is_some_and(|rsa| rsa.key_size() < 2048)
        {
            return Err(Stop::WeakKey);
        }
        let algorithm = public.algorithm();
        let fingerprint = public.fingerprint(HashAlg::Sha256).to_string();
        let verdict = match &self.trust {
            Trust::Scan => Err(Stop::Scanned),
            Trust::FirstUse => Ok(true),
            Trust::Pinned(saved) if *saved == fingerprint => Ok(true),
            Trust::Pinned(_) => Err(Stop::Mismatch),
        };
        *self.presented.lock().unwrap() = Some(Presented {
            scan: HostKeyScan {
                fingerprint,
                algorithm: algorithm.as_str().to_owned(),
            },
            family: KeyFamily::of(&algorithm),
        });
        verdict
    }
}

fn stop_code(stop: &Stop) -> Code {
    let error = match stop {
        Stop::Scanned => return SCANNED,
        Stop::Mismatch => return CHANGED,
        Stop::Certificate => return "ssh_host_certificate_unsupported",
        Stop::WeakKey => return "ssh_host_key_weak",
        Stop::Ssh(error) => error,
    };
    match error {
        russh::Error::NoCommonAlgo {
            kind: AlgorithmKind::Key,
            ..
        } => NO_SUCH_HOST_KEY,
        russh::Error::NoCommonAlgo { .. } | russh::Error::UnknownAlgo => {
            "ssh_algorithms_unsupported"
        }
        russh::Error::Version => "ssh_not_an_ssh_server",
        russh::Error::HUP | russh::Error::Disconnect => "ssh_gateway_closed_connection",
        russh::Error::ConnectionTimeout
        | russh::Error::KeepaliveTimeout
        | russh::Error::InactivityTimeout
        | russh::Error::Elapsed(_) => "ssh_handshake_timeout",
        russh::Error::IO(io) => match io.kind() {
            ErrorKind::UnexpectedEof
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::BrokenPipe => "ssh_gateway_closed_connection",
            ErrorKind::TimedOut => "ssh_handshake_timeout",
            _ => "ssh_handshake_failed",
        },
        _ => "ssh_handshake_failed",
    }
}

/// The same algorithm policy for scanning and for connecting, so a scanned
/// fingerprint is the one a later connection is checked against.
fn client_config(only: Option<KeyFamily>) -> client::Config {
    let mut config = client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(KEEPALIVE),
        keepalive_max: 3,
        channel_buffer_size: 16,
        ..Default::default()
    };
    config.preferred.key.to_mut().retain(|key| {
        !matches!(key, Algorithm::Rsa { hash: None } | Algorithm::Dsa)
            && only.is_none_or(|family| KeyFamily::of(key) == Some(family))
    });
    config
}

async fn dial(host: &str, port: u16) -> std::result::Result<TcpStream, Code> {
    let resolved = tokio::time::timeout(CONNECT, tokio::net::lookup_host((host, port))).await;
    let Ok(Ok(addresses)) = resolved else {
        return Err("ssh_dns_failed");
    };
    let mut failure = "ssh_dns_failed";
    for address in addresses {
        if address.ip().is_unspecified() || address.ip().is_multicast() {
            failure = "ssh_gateway_invalid";
            continue;
        }
        match tokio::time::timeout(CONNECT, TcpStream::connect(address)).await {
            Ok(Ok(socket)) => {
                // Interactive protocols must not wait for Nagle's algorithm.
                let _ = socket.set_nodelay(true);
                return Ok(socket);
            }
            Ok(Err(error)) => {
                failure = match error.kind() {
                    ErrorKind::ConnectionRefused => "ssh_connection_refused",
                    ErrorKind::TimedOut => "ssh_connect_timeout",
                    _ => "ssh_gateway_unreachable",
                }
            }
            Err(_) => failure = "ssh_connect_timeout",
        }
    }
    Err(failure)
}

async fn open_session(
    host: &str,
    port: u16,
    trust: &Trust,
    only: Option<KeyFamily>,
    presented: &Arc<Mutex<Option<Presented>>>,
) -> std::result::Result<client::Handle<Gate>, Code> {
    let socket = dial(host, port).await?;
    let gate = Gate {
        trust: trust.clone(),
        presented: presented.clone(),
    };
    client::connect_stream(Arc::new(client_config(only)), socket, gate)
        .await
        .map_err(|stop| stop_code(&stop))
}

/// Connect, read the host key the server proves during key exchange and
/// disconnect before any authentication. The result still has to be compared
/// with a value obtained from the server administrator: a scan cannot tell a
/// genuine server from one that intercepts the connection.
pub async fn scan_host_key(host: &str, port: u16) -> Result<HostKeyScan> {
    validate_host(host)?;
    ensure!(port > 0, "ssh_port_invalid");
    let presented = Arc::new(Mutex::new(None));
    let attempt = tokio::time::timeout(
        SCAN,
        open_session(host, port, &Trust::Scan, None, &presented),
    )
    .await;
    let failure = match attempt {
        Err(_) => "ssh_handshake_timeout",
        Ok(Err(code)) => code,
        Ok(Ok(session)) => {
            // Unreachable: a scanning gate never accepts a key.
            drop(session);
            "ssh_handshake_failed"
        }
    };
    let found = presented.lock().unwrap().take();
    match found {
        Some(key) if failure == SCANNED => Ok(key.scan),
        _ if failure == SCANNED => bail!("ssh_handshake_failed"),
        _ => bail!(failure),
    }
}

/// A connection attempt that failed, with the host key if one was seen.
struct Failure {
    code: Code,
    presented: Option<HostKeyScan>,
}
impl From<Code> for Failure {
    fn from(code: Code) -> Self {
        Self {
            code,
            presented: None,
        }
    }
}

/// Key exchange under the profile's trust: a saved fingerprint is enforced, a
/// profile without one accepts the key and reports it. A server usually owns
/// several host keys and presents only one per connection, so when the first
/// one does not match a saved fingerprint, each remaining key type is
/// requested before the key is reported as changed.
async fn handshake(
    profile: &SshProfile,
) -> std::result::Result<(client::Handle<Gate>, HostKeyScan), Failure> {
    let presented: Arc<Mutex<Option<Presented>>> = Arc::new(Mutex::new(None));
    let trust = Trust::of(profile);
    let seen = || presented.lock().unwrap().clone();
    let accepted = |session| match seen() {
        Some(key) => Ok((session, key.scan)),
        None => Err(Failure::from("ssh_handshake_failed")),
    };
    match open_session(&profile.host, profile.port, &trust, None, &presented).await {
        Ok(session) => return accepted(session),
        Err(CHANGED) => {}
        Err(code) => return Err(code.into()),
    }
    // The key a first connection would have learned is the one to report.
    let first = seen();
    for family in KeyFamily::ALL {
        if Some(family) == first.as_ref().and_then(|key| key.family) {
            continue;
        }
        let retry = open_session(
            &profile.host,
            profile.port,
            &trust,
            Some(family),
            &presented,
        )
        .await;
        if let Ok(session) = retry {
            return accepted(session);
        }
    }
    Err(Failure {
        code: CHANGED,
        presented: first.map(|key| key.scan),
    })
}

fn load_bounded_key(
    text: &str,
    phrase: Option<&str>,
) -> std::result::Result<russh::keys::PrivateKey, Code> {
    if text.len() > 65536 {
        return Err("ssh_private_key_invalid");
    }
    let phrase = phrase.filter(|value| !value.is_empty());
    let key = if text
        .trim_start()
        .starts_with("-----BEGIN OPENSSH PRIVATE KEY-----")
    {
        let key =
            russh::keys::PrivateKey::from_openssh(text).map_err(|_| "ssh_private_key_invalid")?;
        if let russh::keys::ssh_key::Kdf::Bcrypt { salt, rounds } = key.kdf()
            && (!(1..=128).contains(rounds) || salt.len() > 64)
        {
            return Err("ssh_key_kdf_limit");
        }
        if key.is_encrypted() {
            key.decrypt(phrase.ok_or("ssh_key_passphrase_required")?)
                .map_err(|_| "ssh_key_passphrase_invalid")?
        } else {
            key
        }
    } else {
        if text.contains("ENCRYPTED") || text.trim_start().starts_with("PuTTY-User-Key-File-") {
            return Err("ssh_key_convert_to_openssh");
        }
        russh::keys::decode_secret_key(text, None).map_err(|_| "ssh_private_key_invalid")?
    };
    if key
        .public_key()
        .key_data()
        .rsa()
        .is_some_and(|rsa| !(2048..=8192).contains(&rsa.key_size()))
    {
        return Err("ssh_rsa_key_size_invalid");
    }
    Ok(key)
}

struct Method {
    kind: MethodKind,
    rejected: Code,
    disabled: Code,
}
fn method(auth: SshAuth) -> Method {
    match auth {
        SshAuth::Password => Method {
            kind: MethodKind::Password,
            rejected: "ssh_password_rejected",
            disabled: "ssh_password_auth_disabled",
        },
        SshAuth::PrivateKey => Method {
            kind: MethodKind::PublicKey,
            rejected: "ssh_key_rejected",
            disabled: "ssh_publickey_auth_disabled",
        },
    }
}

async fn authenticate(
    client: &mut client::Handle<Gate>,
    profile: &SshProfile,
) -> std::result::Result<(), Code> {
    let method = method(profile.auth);
    // The standard "none" request lists what this account may use, which tells
    // a disabled method apart from wrong credentials.
    match client.authenticate_none(profile.username.clone()).await {
        Ok(AuthResult::Success) => return Ok(()),
        Ok(AuthResult::Failure {
            remaining_methods, ..
        }) => {
            if !remaining_methods.is_empty() && !remaining_methods.contains(&method.kind) {
                return Err(method.disabled);
            }
        }
        // The server dropped the connection instead of answering.
        Err(_) => return Err("ssh_authentication_failed"),
    }
    let outcome = match profile.auth {
        SshAuth::Password => {
            client
                .authenticate_password(
                    profile.username.clone(),
                    profile.password.as_deref().unwrap_or(""),
                )
                .await
        }
        SshAuth::PrivateKey => {
            let encoded = Zeroizing::new(profile.private_key.clone().unwrap_or_default());
            let phrase = profile.passphrase.clone().map(Zeroizing::new);
            let count = KEY_ACTIVE.fetch_add(1, Ordering::AcqRel);
            let slot = KeySlot;
            if count >= 4 {
                return Err("ssh_key_crypto_busy");
            }
            let key = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                load_bounded_key(&encoded, phrase.as_deref().map(|v| v.as_str()))
            })
            .await
            .map_err(|_| "ssh_private_key_invalid")??;
            if matches!(key.algorithm(), Algorithm::Dsa) {
                return Err("ssh_private_key_algorithm_unsupported");
            }
            let hash = if key.algorithm().is_rsa() {
                match client
                    .best_supported_rsa_hash()
                    .await
                    .map_err(|_| "ssh_rsa_hash_failed")?
                {
                    Some(Some(HashAlg::Sha512)) => Some(HashAlg::Sha512),
                    Some(Some(HashAlg::Sha256)) => Some(HashAlg::Sha256),
                    _ => return Err("ssh_rsa_sha2_required"),
                }
            } else {
                None
            };
            client
                .authenticate_publickey(
                    profile.username.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                )
                .await
        }
    };
    match outcome {
        Ok(AuthResult::Success) => Ok(()),
        Ok(AuthResult::Failure { .. }) => Err(method.rejected),
        Err(_) => Err("ssh_authentication_failed"),
    }
}

async fn connect(
    profile: &SshProfile,
    cancel: &CancellationToken,
) -> std::result::Result<(client::Handle<Gate>, HostKeyScan), Failure> {
    let operation = async {
        let (mut client, key) = handshake(profile).await?;
        authenticate(&mut client, profile).await?;
        Ok((client, key))
    };
    tokio::select! {
        _ = cancel.cancelled() => Err("tunnel_start_cancelled".into()),
        value = tokio::time::timeout(HANDSHAKE, operation) => {
            value.unwrap_or(Err("ssh_handshake_timeout".into()))
        }
    }
}

/// Sentence for a code this module reports, or `None` for a code that belongs
/// to another part of the application.
pub fn describe_known(code: &str, en: bool) -> Option<&'static str> {
    let (pl, english) = match code {
        // Reaching the SSH server.
        "ssh_dns_failed" => (
            "Nie znaleziono serwera SSH o tej nazwie. Sprawdź adres i połączenie z internetem.",
            "No SSH server with this name was found. Check the address and your internet connection.",
        ),
        "ssh_connection_refused" => (
            "Serwer odrzucił połączenie na tym porcie. Sprawdź port SSH i czy usługa SSH działa.",
            "The server refused the connection on this port. Check the SSH port and that SSH is running.",
        ),
        "ssh_connect_timeout" => (
            "Serwer SSH nie odpowiedział w wyznaczonym czasie. Sprawdź adres, port, zaporę lub VPN.",
            "The SSH server did not answer in time. Check the address, port, firewall or VPN.",
        ),
        "ssh_gateway_unreachable" => (
            "Nie udało się połączyć z serwerem SSH. Sprawdź sieć, adres i port.",
            "The SSH server could not be reached. Check the network, address and port.",
        ),
        "ssh_gateway_invalid" | "host_invalid" => (
            "Adres serwera jest nieprawidłowy. Wpisz nazwę hosta lub adres IP, bez protokołu i ścieżki.",
            "The server address is not valid. Enter a host name or IP address, without a protocol or path.",
        ),
        // Host key.
        "ssh_host_key_changed" => (
            "UWAGA: klucz tego serwera SSH jest inny niż zapamiętany. Nie wysłano hasła ani klucza. Jeśli administrator nie wymienił klucza serwera, ktoś może przechwytywać połączenie. Zaufaj nowemu kluczowi tylko wtedy, gdy wiesz, że serwer został zmieniony.",
            "WARNING: this SSH server's key is different from the remembered one. No password or key was sent. Unless the administrator replaced the server key, someone may be intercepting the connection. Trust the new key only if you know the server was changed.",
        ),
        "ssh_host_certificate_unsupported" => (
            "Serwer przedstawił certyfikat hosta, którego ta wersja nie obsługuje. Poproś administratora o zwykły klucz hosta.",
            "The server presented a host certificate, which this version does not support. Ask the administrator for a plain host key.",
        ),
        "ssh_host_key_weak" => (
            "Klucz RSA serwera jest krótszy niż 2048 bitów i został odrzucony jako niebezpieczny.",
            "The server's RSA key is shorter than 2048 bits and was refused as unsafe.",
        ),
        "ssh_host_key_algorithms_unsupported" | "ssh_algorithms_unsupported" => (
            "Serwer SSH oferuje tylko przestarzałe algorytmy, których ta aplikacja nie używa. Poproś administratora o aktualizację konfiguracji SSH.",
            "The SSH server only offers outdated algorithms that this application does not use. Ask the administrator to update the SSH configuration.",
        ),
        "ssh_pin_invalid" => (
            "Odcisk klucza ma zły format. Zostaw pole puste albo wpisz SHA256: i 43 znaki, bez znaków = na końcu.",
            "The fingerprint has the wrong format. Leave the field empty or enter SHA256: followed by 43 characters, without trailing = signs.",
        ),
        // Handshake.
        "ssh_not_an_ssh_server" => (
            "Pod tym adresem i portem nie odpowiada serwer SSH. Sprawdź port.",
            "The service at this address and port is not an SSH server. Check the port.",
        ),
        "ssh_gateway_closed_connection" => (
            "Serwer zamknął połączenie przed zalogowaniem. Sprawdź, czy to port SSH; serwer może też blokować ten adres albo ograniczać liczbę połączeń.",
            "The server closed the connection before sign-in. Check that this is the SSH port; the server may also be blocking this address or limiting connections.",
        ),
        "ssh_handshake_timeout" => (
            "Serwer SSH odpowiada zbyt wolno i połączenie zostało przerwane. Spróbuj ponownie.",
            "The SSH server answered too slowly and the connection was abandoned. Try again.",
        ),
        "ssh_handshake_failed" => (
            "Nie udało się uzgodnić bezpiecznego połączenia z serwerem SSH.",
            "A secure connection with the SSH server could not be negotiated.",
        ),
        // Signing in.
        "ssh_password_rejected" => (
            "Serwer SSH odrzucił nazwę użytkownika lub hasło.",
            "The SSH server rejected the user name or password.",
        ),
        "ssh_key_rejected" => (
            "Serwer SSH nie przyjął tego klucza dla tego użytkownika. Sprawdź nazwę użytkownika i czy klucz publiczny jest dodany na serwerze.",
            "The SSH server did not accept this key for this user. Check the user name and that the public key is installed on the server.",
        ),
        "ssh_password_auth_disabled" => (
            "Ten serwer SSH nie pozwala logować się hasłem. Użyj klucza prywatnego.",
            "This SSH server does not allow password sign-in. Use a private key.",
        ),
        "ssh_publickey_auth_disabled" => (
            "Ten serwer SSH nie pozwala logować się kluczem. Użyj hasła.",
            "This SSH server does not allow key sign-in. Use a password.",
        ),
        "ssh_authentication_failed" => (
            "Serwer SSH przerwał logowanie. Sprawdź dane logowania i spróbuj ponownie za chwilę.",
            "The SSH server ended the sign-in. Check the credentials and try again in a moment.",
        ),
        "ssh_username_invalid" => (
            "Podaj nazwę użytkownika SSH, bez spacji.",
            "Enter the SSH user name, without spaces.",
        ),
        "ssh_port_invalid" => (
            "Port SSH musi być liczbą od 1 do 65535.",
            "The SSH port must be a number from 1 to 65535.",
        ),
        "ssh_password_required" => ("Podaj hasło SSH.", "Enter the SSH password."),
        "ssh_private_key_required" => (
            "Wczytaj plik klucza prywatnego.",
            "Load a private key file.",
        ),
        "ssh_credential_invalid" => (
            "Hasło lub klucz zawiera niedozwolone znaki albo jest zbyt długie.",
            "The password or key contains invalid characters or is too long.",
        ),
        // Private key files.
        "ssh_private_key_invalid" | "ssh_private_key_utf8_required" => (
            "Nie udało się odczytać klucza prywatnego. Wybierz plik klucza prywatnego (nie .pub) w formacie OpenSSH.",
            "The private key could not be read. Choose the private key file (not .pub) in OpenSSH format.",
        ),
        "ssh_key_passphrase_required" => (
            "Ten klucz jest zaszyfrowany. Podaj hasło klucza.",
            "This key is encrypted. Enter the key passphrase.",
        ),
        "ssh_key_passphrase_invalid" => (
            "Hasło klucza jest nieprawidłowe.",
            "The key passphrase is wrong.",
        ),
        "ssh_key_convert_to_openssh" => (
            "Ten format klucza nie jest obsługiwany. Przekonwertuj klucz do formatu OpenSSH (np. ssh-keygen -p -f plik albo eksport OpenSSH w PuTTYgen).",
            "This key format is not supported. Convert the key to OpenSSH format (for example ssh-keygen -p -f file, or the OpenSSH export in PuTTYgen).",
        ),
        "ssh_key_kdf_limit" => (
            "Klucz ma zbyt kosztowne zabezpieczenie hasłem. Zapisz go ponownie poleceniem ssh-keygen -p -a 16 -f plik.",
            "The key uses passphrase protection that is too expensive. Save it again with ssh-keygen -p -a 16 -f file.",
        ),
        "ssh_rsa_key_size_invalid" => (
            "Klucz RSA musi mieć od 2048 do 8192 bitów.",
            "The RSA key must have between 2048 and 8192 bits.",
        ),
        "ssh_private_key_algorithm_unsupported" => (
            "Klucze DSA nie są obsługiwane. Użyj klucza Ed25519, ECDSA lub RSA.",
            "DSA keys are not supported. Use an Ed25519, ECDSA or RSA key.",
        ),
        "ssh_rsa_sha2_required" | "ssh_rsa_hash_failed" => (
            "Serwer nie obsługuje bezpiecznych podpisów RSA (SHA-2). Użyj klucza Ed25519 albo poproś o aktualizację serwera SSH.",
            "The server does not support secure RSA signatures (SHA-2). Use an Ed25519 key or ask for the SSH server to be updated.",
        ),
        "ssh_key_crypto_busy" => (
            "Trwa zbyt wiele logowań kluczem naraz. Spróbuj ponownie za chwilę.",
            "Too many key sign-ins are in progress. Try again in a moment.",
        ),
        "ssh_gateway_disconnected" => (
            "Połączenie z serwerem SSH zostało przerwane. Uruchom tunel ponownie.",
            "The connection to the SSH server was lost. Start the tunnel again.",
        ),
        // Forwarding.
        "tunnel_target_refused" => (
            "Serwer SSH nie może połączyć się z adresem usługi. Sprawdź adres i port usługi widziane z serwera SSH oraz czy usługa działa.",
            "The SSH server cannot connect to the service address. Check the service address and port as seen from the SSH server, and that the service is running.",
        ),
        "tunnel_forwarding_prohibited" => (
            "Serwer SSH nie zezwala temu kontu na przekierowanie do tego adresu. Poproś administratora o włączenie przekierowań TCP.",
            "The SSH server does not allow this account to forward to this address. Ask the administrator to enable TCP forwarding.",
        ),
        "tunnel_target_unreachable" | "tunnel_channel_failed" => (
            "Serwer SSH nie otworzył połączenia z usługą docelową.",
            "The SSH server did not open a connection to the target service.",
        ),
        "tunnel_target_timeout" => (
            "Usługa docelowa nie odpowiedziała serwerowi SSH w wyznaczonym czasie. Sprawdź adres i port usługi.",
            "The target service did not answer the SSH server in time. Check the service address and port.",
        ),
        "tunnel_client_limit" => (
            "Osiągnięto limit jednoczesnych połączeń przez tunel. Zamknij nieużywane połączenia w programie klienckim.",
            "The limit of simultaneous connections through the tunnel was reached. Close unused connections in the client program.",
        ),
        "local_port_in_use" => (
            "Wybrany port lokalny jest zajęty przez inny program. Wybierz inny port albo tryb automatyczny.",
            "The chosen local port is in use by another program. Choose another port or automatic mode.",
        ),
        "local_port_unavailable" => (
            "Nie udało się otworzyć portu lokalnego. Wybierz inny port albo tryb automatyczny.",
            "The local port could not be opened. Choose another port or automatic mode.",
        ),
        "local_listener_failed" => (
            "Port lokalny przestał przyjmować połączenia. Uruchom tunel ponownie.",
            "The local port stopped accepting connections. Start the tunnel again.",
        ),
        // Tunnel lifecycle and profile.
        "tunnel_name_invalid" => ("Podaj nazwę tunelu.", "Enter a name for the tunnel."),
        "tunnel_port_invalid" => (
            "Port usługi musi być liczbą od 1 do 65535, a port lokalny 1024 lub wyższy.",
            "The service port must be from 1 to 65535 and the local port 1024 or higher.",
        ),
        "tunnel_busy" => (
            "Ten tunel właśnie się uruchamia lub zatrzymuje. Poczekaj chwilę.",
            "This tunnel is starting or stopping. Wait a moment.",
        ),
        "tunnel_limit" => (
            "Działa już maksymalna liczba tuneli. Zatrzymaj jeden z nich.",
            "The maximum number of tunnels is already running. Stop one of them.",
        ),
        "tunnel_start_cancelled" => (
            "Uruchamianie tunelu zostało przerwane.",
            "Starting the tunnel was cancelled.",
        ),
        "tunnel_stop_pending" => (
            "Tunel wciąż się zatrzymuje. Spróbuj ponownie za chwilę.",
            "The tunnel is still stopping. Try again in a moment.",
        ),
        "tunnel_not_found" => (
            "Ten tunel nie jest uruchomiony.",
            "This tunnel is not running.",
        ),
        "tunnel_manager_closed" => (
            "Tunele są zamykane. Odblokuj aplikację lub uruchom ją ponownie.",
            "Tunnels are shutting down. Unlock the application or start it again.",
        ),
        _ => return None,
    };
    Some(if en { english } else { pl })
}

/// Short, actionable sentence for any `ssh_*`, `tunnel_*` or `local_port_*`
/// code. Unknown codes get a generic sentence, never the code itself.
pub fn describe(code: &str, en: bool) -> &'static str {
    describe_known(code, en).unwrap_or(if en {
        "The SSH tunnel could not be set up. Check the settings and try again."
    } else {
        "Nie udało się zestawić tunelu SSH. Sprawdź ustawienia i spróbuj ponownie."
    })
}

struct StartupGuard {
    entry: Arc<Entry>,
    armed: bool,
}
impl Drop for StartupGuard {
    fn drop(&mut self) {
        if self.armed {
            self.entry.cancel.cancel();
        }
    }
}
struct TaskGuard(Arc<Entry>);
impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.cancel.cancel();
        let active = matches!(
            self.0.view().state.as_str(),
            "active" | "starting" | "stopping"
        );
        if active {
            self.0.state("stopped", None, None);
        }
        self.0.done.notify_waiters();
    }
}
struct ClientGuard(Arc<Entry>);
impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::Relaxed);
    }
}

/// macOS starts GUI applications with 256 file descriptors, fewer than the
/// forwarded connections a manager admits. Raise the soft limit once.
#[cfg(target_os = "macos")]
fn raise_descriptor_limit() {
    #[repr(C)]
    struct Limit {
        current: u64,
        maximum: u64,
    }
    unsafe extern "C" {
        fn getrlimit(resource: i32, limit: *mut Limit) -> i32;
        fn setrlimit(resource: i32, limit: *const Limit) -> i32;
    }
    const NOFILE: i32 = 8;
    const WANTED: u64 = 4096;
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let mut limit = Limit {
            current: 0,
            maximum: 0,
        };
        // SAFETY: both calls only read or write the local, correctly laid out struct.
        unsafe {
            if getrlimit(NOFILE, &mut limit) == 0 && limit.current < WANTED {
                limit.current = WANTED.min(limit.maximum);
                setrlimit(NOFILE, &limit);
            }
        }
    });
}
#[cfg(not(target_os = "macos"))]
fn raise_descriptor_limit() {}

impl TunnelManager {
    pub async fn new() -> Self {
        raise_descriptor_limit();
        Self {
            inner: Arc::new(Inner {
                entries: Mutex::new(HashMap::new()),
                clients: Arc::new(Semaphore::new(MAX_CLIENTS)),
                shutdown: CancellationToken::new(),
            }),
        }
    }
    pub async fn start(&self, profile: &TunnelProfile) -> Result<TunnelStatus> {
        validate_tunnel(profile, true)?;
        ensure!(!self.inner.shutdown.is_cancelled(), "tunnel_manager_closed");
        let entry = {
            let mut entries = self.inner.entries.lock().unwrap();
            if let Some(previous) = entries.get(&profile.id) {
                match previous.view().state.as_str() {
                    "active" => return Ok(previous.view()),
                    "starting" | "stopping" => bail!("tunnel_busy"),
                    _ => {}
                }
            }
            ensure!(
                entries
                    .values()
                    .filter(|v| matches!(
                        v.view().state.as_str(),
                        "active" | "starting" | "stopping"
                    ))
                    .count()
                    < MAX_TUNNELS,
                "tunnel_limit"
            );
            if entries.len() >= 256 {
                entries.retain(|_, entry| {
                    matches!(
                        entry.view().state.as_str(),
                        "active" | "starting" | "stopping"
                    )
                });
            }
            let entry = Arc::new(Entry {
                status: Mutex::new(TunnelStatus {
                    id: profile.id,
                    state: "starting".into(),
                    local_port: None,
                    bytes_sent: 0,
                    bytes_received: 0,
                    active_clients: 0,
                    error: None,
                    host_key: None,
                }),
                cancel: self.inner.shutdown.child_token(),
                sent: AtomicU64::new(0),
                received: AtomicU64::new(0),
                clients: AtomicU32::new(0),
                done: tokio::sync::Notify::new(),
            });
            entries.insert(profile.id, entry.clone());
            entry
        };
        let (reply, receive) = oneshot::channel();
        let owned = PrivateTunnel(profile.clone());
        let limits = self.inner.clients.clone();
        let task_entry = entry.clone();
        tokio::spawn(async move {
            let _cleanup = TaskGuard(task_entry.clone());
            run_tunnel(owned, task_entry, limits, reply).await;
        });
        let mut guard = StartupGuard { entry, armed: true };
        let result = receive
            .await
            .map_err(|_| anyhow!("tunnel_start_cancelled"))?;
        if result.is_ok() {
            guard.armed = false;
        }
        result
    }
    pub async fn stop(&self, id: Uuid) -> Result<TunnelStatus> {
        let entry = self
            .inner
            .entries
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow!("tunnel_not_found"))?;
        let notified = entry.done.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if matches!(
            entry.view().state.as_str(),
            "starting" | "active" | "stopping"
        ) {
            entry.state("stopping", None, None);
            entry.cancel.cancel();
            tokio::time::timeout(Duration::from_secs(4), notified)
                .await
                .map_err(|_| anyhow!("tunnel_stop_pending"))?;
        }
        Ok(entry.view())
    }
    pub fn status(&self, id: Uuid) -> Option<TunnelStatus> {
        self.inner
            .entries
            .lock()
            .unwrap()
            .get(&id)
            .map(|value| value.view())
    }
    pub fn statuses(&self) -> Vec<TunnelStatus> {
        self.inner
            .entries
            .lock()
            .unwrap()
            .values()
            .map(|value| value.view())
            .collect()
    }
    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        let ids: Vec<_> = self.inner.entries.lock().unwrap().keys().copied().collect();
        for id in ids {
            let _ = self.stop(id).await;
        }
    }
}

async fn run_tunnel(
    mut profile: PrivateTunnel,
    entry: Arc<Entry>,
    limits: Arc<Semaphore>,
    reply: oneshot::Sender<Result<TunnelStatus>>,
) {
    let client = match connect(&profile.ssh, &entry.cancel).await {
        Ok((client, key)) => {
            entry.host_key(Some(key));
            Arc::new(client)
        }
        Err(failure) => {
            entry.host_key(failure.presented.clone());
            entry.state(
                if entry.cancel.is_cancelled() {
                    "stopped"
                } else {
                    "failed"
                },
                Some(failure.code.into()),
                None,
            );
            let error = match failure.presented {
                Some(presented) if failure.code == CHANGED => {
                    anyhow::Error::new(HostKeyChanged { presented })
                }
                _ => anyhow!(failure.code),
            };
            let _ = reply.send(Err(error));
            return;
        }
    };
    profile.clear_auth();
    let listener =
        match TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, profile.local_port)).await {
            Ok(value) => value,
            Err(error) => {
                let code = if error.kind() == ErrorKind::AddrInUse {
                    "local_port_in_use"
                } else {
                    "local_port_unavailable"
                };
                entry.state("failed", Some(code.into()), None);
                let _ = reply.send(Err(anyhow!(code)));
                return;
            }
        };
    let port = match listener.local_addr() {
        Ok(value) => value.port(),
        Err(_) => return,
    };
    if entry.cancel.is_cancelled() {
        let _ = reply.send(Err(anyhow!("tunnel_start_cancelled")));
        return;
    }
    entry.state("active", None, Some(port));
    let _ = reply.send(Ok(entry.view()));
    let mut workers = JoinSet::new();
    let mut pulse = tokio::time::interval(Duration::from_secs(1));
    let mut accept_failures = 0u32;
    loop {
        tokio::select! {
            _ = entry.cancel.cancelled() => break,
            _ = pulse.tick() => {
                if client.is_closed() {
                    entry.state("disconnected", Some("ssh_gateway_disconnected".into()), None);
                    break;
                }
            }
            Some(_) = workers.join_next(), if !workers.is_empty() => {}
            accepted = listener.accept() => {
                let (socket, address) = match accepted {
                    Ok(value) => value,
                    Err(_) => {
                        // Running out of descriptors is temporary; a listener
                        // that keeps failing for seconds is not.
                        accept_failures += 1;
                        if accept_failures > 40 {
                            entry.state("failed", Some("local_listener_failed".into()), None);
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        continue;
                    }
                };
                accept_failures = 0;
                let Ok(permit) = limits.clone().try_acquire_owned() else {
                    entry.note(Some("tunnel_client_limit"));
                    continue;
                };
                if entry.clients.load(Ordering::Relaxed) >= MAX_TUNNEL_CLIENTS {
                    entry.note(Some("tunnel_client_limit"));
                    continue;
                }
                entry.clients.fetch_add(1, Ordering::Relaxed);
                let child = entry.clone();
                let ssh = client.clone();
                let host = profile.target_host.clone();
                let target = profile.target_port;
                workers.spawn(async move {
                    let _permit = permit;
                    let _client = ClientGuard(child.clone());
                    forward(socket, address, ssh, host, target, child).await;
                });
            }
        }
    }
    drop(listener);
    entry.cancel.cancel();
    if tokio::time::timeout(Duration::from_secs(2), async {
        while workers.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        workers.abort_all();
        while workers.join_next().await.is_some() {}
    }
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        client.disconnect(russh::Disconnect::ByApplication, "", ""),
    )
    .await;
}

fn channel_code(error: &russh::Error) -> Code {
    match error {
        russh::Error::ChannelOpenFailure(ChannelOpenFailure::ConnectFailed) => {
            "tunnel_target_refused"
        }
        russh::Error::ChannelOpenFailure(ChannelOpenFailure::AdministrativelyProhibited) => {
            "tunnel_forwarding_prohibited"
        }
        russh::Error::ChannelOpenFailure(_) => "tunnel_target_unreachable",
        _ => "tunnel_channel_failed",
    }
}

/// Copy one direction until its source ends, then pass the end-of-stream on.
/// There is no idle or stall timer: a quiet pooled connection is healthy.
async fn pump(
    mut source: impl AsyncRead + Unpin,
    mut sink: impl AsyncWrite + Unpin,
    total: &AtomicU64,
    moved: &AtomicU64,
) {
    let mut buffer = vec![0u8; 32 * 1024];
    loop {
        let size = match source.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(size) => size,
        };
        if sink.write_all(&buffer[..size]).await.is_err() {
            break;
        }
        total.fetch_add(size as u64, Ordering::Relaxed);
        moved.fetch_add(size as u64, Ordering::Relaxed);
    }
    let _ = sink.shutdown().await;
}

/// Let the other direction of a half-closed connection finish. Only a
/// direction that stays silent for [`HALF_CLOSED_IDLE`] is given up, so a peer
/// that never answers the end-of-stream cannot hold a connection slot forever.
async fn finish(remaining: impl Future<Output = ()>, moved: &AtomicU64) {
    tokio::pin!(remaining);
    let mut seen = moved.load(Ordering::Relaxed);
    loop {
        tokio::select! {
            _ = &mut remaining => return,
            _ = tokio::time::sleep(HALF_CLOSED_IDLE) => {
                let now = moved.load(Ordering::Relaxed);
                if now == seen {
                    return;
                }
                seen = now;
            }
        }
    }
}

async fn forward(
    socket: TcpStream,
    address: std::net::SocketAddr,
    client: Arc<client::Handle<Gate>>,
    host: String,
    port: u16,
    entry: Arc<Entry>,
) {
    let _ = socket.set_nodelay(true);
    let opening = client.channel_open_direct_tcpip(
        host,
        port as u32,
        address.ip().to_string(),
        address.port() as u32,
    );
    let channel = tokio::select! {
        _ = entry.cancel.cancelled() => return,
        opened = tokio::time::timeout(CHANNEL, opening) => match opened {
            Ok(Ok(channel)) => channel,
            Ok(Err(error)) => {
                entry.note(Some(channel_code(&error)));
                return;
            }
            Err(_) => {
                entry.note(Some("tunnel_target_timeout"));
                return;
            }
        },
    };
    entry.note(None);
    let (local_read, local_write) = socket.into_split();
    let (remote_read, remote_write) = tokio::io::split(channel.into_stream());
    // Two independent directions: one side may stop sending while it still
    // waits for the other side's answer.
    let (sent, received) = (AtomicU64::new(0), AtomicU64::new(0));
    let upstream = pump(local_read, remote_write, &entry.sent, &sent);
    let downstream = pump(remote_read, local_write, &entry.received, &received);
    tokio::pin!(upstream, downstream);
    let relay = async {
        tokio::select! {
            _ = &mut upstream => finish(downstream, &received).await,
            _ = &mut downstream => finish(upstream, &sent).await,
        }
    };
    tokio::select! {
        _ = entry.cancel.cancelled() => {}
        _ = relay => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile() -> TunnelProfile {
        TunnelProfile {
            id: Uuid::new_v4(),
            name: "Owned test".into(),
            ssh: SshProfile {
                host: "127.0.0.1".into(),
                port: 22,
                username: "owned".into(),
                auth: SshAuth::Password,
                password: Some("synthetic-only".into()),
                private_key: None,
                passphrase: None,
                fingerprint: "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            },
            target_host: "127.0.0.1".into(),
            target_port: 5432,
            local_port: 0,
        }
    }
    #[test]
    fn validation_refuses_url_public_bind_privileged_ports_and_noncanonical_pin() {
        let base = profile();
        assert!(validate_tunnel(&base, true).is_ok());
        for host in [
            "https://host",
            "host/path",
            "host@other",
            "0.0.0.0",
            "ff02::1",
            "host\n",
        ] {
            let mut p = base.clone();
            p.ssh.host = host.into();
            assert!(validate_tunnel(&p, true).is_err());
        }
        let mut p = base.clone();
        p.local_port = 1;
        assert!(validate_tunnel(&p, true).is_err());
        for pin in [
            "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "SHA256:short",
            " ",
        ] {
            p = base.clone();
            p.ssh.fingerprint = pin.into();
            assert_eq!(
                validate_tunnel(&p, true).unwrap_err().to_string(),
                "ssh_pin_invalid"
            );
        }
        // No fingerprint yet: the key is trusted on first use.
        p = base.clone();
        p.ssh.fingerprint.clear();
        assert!(validate_tunnel(&p, true).is_ok());
    }
    #[tokio::test]
    async fn failed_start_has_no_listener_and_manager_shutdown_rejects_new_start() {
        let manager = TunnelManager::new().await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut p = profile();
        p.ssh.port = listener.local_addr().unwrap().port();
        drop(listener);
        assert_eq!(
            manager.start(&p).await.unwrap_err().to_string(),
            "ssh_connection_refused"
        );
        let state = manager.status(p.id).unwrap();
        assert_eq!(state.state, "failed");
        assert_eq!(state.error.as_deref(), Some("ssh_connection_refused"));
        assert!(state.local_port.is_none());
        assert_eq!(state.active_clients, 0);
        manager.shutdown().await;
        assert_eq!(
            manager.start(&p).await.unwrap_err().to_string(),
            "tunnel_manager_closed"
        );
    }
    #[tokio::test]
    async fn scan_reports_refused_ports_services_that_are_not_ssh_and_bad_input() {
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        assert_eq!(
            scan_host_key("127.0.0.1", port)
                .await
                .unwrap_err()
                .to_string(),
            "ssh_connection_refused"
        );
        let web = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = web.local_addr().unwrap().port();
        let web_task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = web.accept().await {
                let _ = socket
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
                    .await;
            }
        });
        // A service that answers something else and hangs up.
        assert_eq!(
            scan_host_key("127.0.0.1", port)
                .await
                .unwrap_err()
                .to_string(),
            "ssh_gateway_closed_connection"
        );
        web_task.abort();
        for (host, port) in [("https://host", 22), ("0.0.0.0", 22), ("127.0.0.1", 0)] {
            assert!(scan_host_key(host, port).await.is_err());
        }
    }
    #[test]
    fn every_reported_code_has_a_plain_sentence_in_both_languages() {
        // Every code literal in this file that reaches a caller.
        let source = include_str!("ssh.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        let mut codes = std::collections::BTreeSet::new();
        for piece in production.split('"').skip(1).step_by(2) {
            let is_code = [
                "ssh_",
                "tunnel_",
                "local_port_",
                "local_listener_",
                "host_invalid",
            ]
            .iter()
            .any(|prefix| piece.starts_with(prefix))
                && piece
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            if is_code && piece != SCANNED {
                codes.insert(piece);
            }
        }
        assert!(codes.len() > 40, "{}", codes.len());
        for code in codes {
            for en in [false, true] {
                let text = describe_known(code, en).unwrap_or_else(|| panic!("{code}"));
                assert!(text.len() > 12 && !text.contains(code), "{code}");
            }
            assert_ne!(describe(code, false), describe(code, true), "{code}");
        }
        assert!(describe_known("vault_conflict_reload", true).is_none());
        for en in [false, true] {
            let fallback = describe("ssh_some_future_code", en);
            assert!(!fallback.contains("ssh_some_future_code") && fallback.ends_with('.'));
        }
        assert!(describe("ssh_host_key_changed", true).starts_with("WARNING"));
        assert!(describe("ssh_host_key_changed", false).starts_with("UWAGA"));
    }
    #[test]
    fn private_key_problems_are_told_apart() {
        let plain =
            russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                .unwrap();
        let text = plain
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .unwrap();
        assert!(load_bounded_key(&text, None).is_ok());
        // Editors keep an empty passphrase field as `Some("")`.
        assert!(load_bounded_key(&text, Some("")).is_ok());
        let locked = plain
            .encrypt(&mut rand::rng(), "owned-key-passphrase")
            .unwrap()
            .to_openssh(russh::keys::ssh_key::LineEnding::LF)
            .unwrap();
        assert_eq!(
            load_bounded_key(&locked, None).unwrap_err(),
            "ssh_key_passphrase_required"
        );
        assert_eq!(
            load_bounded_key(&locked, Some("")).unwrap_err(),
            "ssh_key_passphrase_required"
        );
        assert_eq!(
            load_bounded_key(&locked, Some("wrong")).unwrap_err(),
            "ssh_key_passphrase_invalid"
        );
        assert!(load_bounded_key(&locked, Some("owned-key-passphrase")).is_ok());
        assert_eq!(
            load_bounded_key("PuTTY-User-Key-File-3: ssh-ed25519", None).unwrap_err(),
            "ssh_key_convert_to_openssh"
        );
        assert_eq!(
            load_bounded_key("ssh-ed25519 AAAA public-part", None).unwrap_err(),
            "ssh_private_key_invalid"
        );
    }
}

#[cfg(test)]
mod real_forward_tests {
    use super::*;
    use russh::server::{self, Server as _};
    #[derive(Clone)]
    struct OwnedServer {
        auth_calls: Arc<AtomicU32>,
    }
    impl server::Server for OwnedServer {
        type Handler = Self;
        fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
            self.clone()
        }
    }
    impl server::Handler for OwnedServer {
        type Error = russh::Error;
        async fn auth_none(&mut self, _: &str) -> std::result::Result<server::Auth, Self::Error> {
            self.auth_calls.fetch_add(1, Ordering::Relaxed);
            Ok(server::Auth::reject())
        }
        async fn auth_password(
            &mut self,
            user: &str,
            password: &str,
        ) -> std::result::Result<server::Auth, Self::Error> {
            self.auth_calls.fetch_add(1, Ordering::Relaxed);
            Ok(if user == "owned" && password == "owned-only-password" {
                server::Auth::Accept
            } else {
                server::Auth::reject()
            })
        }
        async fn channel_open_direct_tcpip(
            &mut self,
            channel: russh::Channel<server::Msg>,
            host: &str,
            port: u32,
            _: &str,
            _: u32,
            reply: server::ChannelOpenHandle,
            _: &mut server::Session,
        ) -> std::result::Result<(), Self::Error> {
            if host != "127.0.0.1" {
                // Dropping the handle is an administrative refusal.
                return Ok(());
            }
            let Ok(mut target) =
                TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port as u16)).await
            else {
                reply.reject(ChannelOpenFailure::ConnectFailed).await;
                return Ok(());
            };
            reply.accept().await;
            tokio::spawn(async move {
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut target).await;
            });
            Ok(())
        }
    }
    struct Fixture {
        gateway: u16,
        target: u16,
        fingerprint: String,
        auth_calls: Arc<AtomicU32>,
        tasks: Vec<tokio::task::JoinHandle<()>>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for task in &self.tasks {
                task.abort();
            }
        }
    }
    impl Fixture {
        /// An echo target behind an in-process SSH gateway with one Ed25519 host key.
        async fn start() -> Self {
            Self::offering(&[MethodKind::Password, MethodKind::PublicKey]).await
        }
        async fn offering(methods: &[MethodKind]) -> Self {
            let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let target = echo.local_addr().unwrap().port();
            let echo_task = tokio::spawn(async move {
                while let Ok((mut s, _)) = echo.accept().await {
                    tokio::spawn(async move {
                        let (mut read, mut write) = s.split();
                        let _ = tokio::io::copy(&mut read, &mut write).await;
                        // Echo ends when the client stops sending.
                        let _ = write.shutdown().await;
                    });
                }
            });
            let hostkey =
                russh::keys::PrivateKey::random(&mut rand::rng(), russh::keys::Algorithm::Ed25519)
                    .unwrap();
            let fingerprint = hostkey
                .public_key()
                .fingerprint(HashAlg::Sha256)
                .to_string();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let gateway = listener.local_addr().unwrap().port();
            let auth_calls = Arc::new(AtomicU32::new(0));
            let mut server = OwnedServer {
                auth_calls: auth_calls.clone(),
            };
            let config = Arc::new(server::Config {
                methods: methods.into(),
                keys: vec![hostkey],
                auth_rejection_time: Duration::from_millis(1),
                ..Default::default()
            });
            let server_task = tokio::spawn(async move {
                server.run_on_socket(config, &listener).await.unwrap();
            });
            Self {
                gateway,
                target,
                fingerprint,
                auth_calls,
                tasks: vec![echo_task, server_task],
            }
        }
        fn profile(&self) -> TunnelProfile {
            TunnelProfile {
                id: Uuid::new_v4(),
                name: "Owned loopback SSH".into(),
                ssh: SshProfile {
                    host: "127.0.0.1".into(),
                    port: self.gateway,
                    username: "owned".into(),
                    auth: SshAuth::Password,
                    password: Some("owned-only-password".into()),
                    private_key: None,
                    passphrase: None,
                    fingerprint: self.fingerprint.clone(),
                },
                target_host: "127.0.0.1".into(),
                target_port: self.target,
                local_port: 0,
            }
        }
    }
    #[tokio::test]
    async fn actual_pinned_ssh_binary_forward_metrics_collision_and_pin_before_auth() {
        let fixture = Fixture::start().await;
        let manager = TunnelManager::new().await;
        let mut profile = fixture.profile();
        let mut bad = profile.clone();
        bad.id = Uuid::new_v4();
        bad.ssh.fingerprint = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into();
        let refused = manager.start(&bad).await.unwrap_err();
        assert_eq!(refused.to_string(), "ssh_host_key_changed");
        // The key the server really presents is reported, never adopted.
        let changed = refused.downcast_ref::<HostKeyChanged>().unwrap();
        assert_eq!(changed.presented.fingerprint, fixture.fingerprint);
        assert_eq!(fixture.auth_calls.load(Ordering::Relaxed), 0);
        let failed = manager.status(bad.id).unwrap();
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.error.as_deref(), Some("ssh_host_key_changed"));
        assert_eq!(failed.host_key.unwrap().fingerprint, fixture.fingerprint);
        assert!(failed.local_port.is_none());
        let started = manager.start(&profile).await.unwrap();
        assert_eq!(started.state, "active");
        let port = started.local_port.unwrap();
        let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let payload = (0..131072).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        stream.write_all(&payload).await.unwrap();
        let mut received = vec![0; payload.len()];
        tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut received))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(payload, received);
        let state = manager.status(profile.id).unwrap();
        assert_eq!(state.bytes_sent, payload.len() as u64);
        assert_eq!(state.bytes_received, payload.len() as u64);
        assert!(state.error.is_none());
        profile.id = Uuid::new_v4();
        profile.local_port = port;
        assert_eq!(
            manager.start(&profile).await.unwrap_err().to_string(),
            "local_port_in_use"
        );
        let stopped = manager.stop(started.id).await.unwrap();
        assert_eq!(stopped.state, "stopped");
        assert_eq!(stopped.active_clients, 0);
        assert!(
            TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
                .await
                .is_err()
        );
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn first_connection_without_a_fingerprint_learns_it_and_later_ones_enforce_it() {
        let fixture = Fixture::start().await;
        let manager = TunnelManager::new().await;
        let mut profile = fixture.profile();
        profile.ssh.fingerprint.clear();
        let started = manager.start(&profile).await.unwrap();
        assert_eq!(started.state, "active");
        let learned = started.host_key.unwrap();
        assert_eq!(learned.fingerprint, fixture.fingerprint);
        assert_eq!(learned.algorithm, "ssh-ed25519");
        manager.stop(profile.id).await.unwrap();
        // What the application saves after the first connection is a normal pin.
        profile.ssh.fingerprint = learned.fingerprint;
        let again = manager.start(&profile).await.unwrap();
        assert_eq!(again.host_key.unwrap().fingerprint, fixture.fingerprint);
        manager.stop(profile.id).await.unwrap();
        // Another server answering for the same profile is refused.
        let other = Fixture::start().await;
        profile.ssh.port = other.gateway;
        let refused = manager.start(&profile).await.unwrap_err();
        assert_eq!(refused.to_string(), "ssh_host_key_changed");
        assert_eq!(
            refused
                .downcast_ref::<HostKeyChanged>()
                .unwrap()
                .presented
                .fingerprint,
            other.fingerprint
        );
        assert_eq!(other.auth_calls.load(Ordering::Relaxed), 0);
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn scan_returns_the_pin_without_any_authentication_attempt() {
        let fixture = Fixture::start().await;
        let scan = scan_host_key("127.0.0.1", fixture.gateway).await.unwrap();
        assert_eq!(scan.fingerprint, fixture.fingerprint);
        assert_eq!(scan.algorithm, "ssh-ed25519");
        assert_eq!(fixture.auth_calls.load(Ordering::Relaxed), 0);
        // The scanned value is accepted as a profile pin exactly as returned.
        let mut profile = fixture.profile();
        profile.ssh.fingerprint = scan.fingerprint;
        validate_tunnel(&profile, true).unwrap();
        let manager = TunnelManager::new().await;
        assert_eq!(manager.start(&profile).await.unwrap().state, "active");
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn wrong_password_is_a_rejection_not_a_handshake_failure() {
        let fixture = Fixture::start().await;
        let manager = TunnelManager::new().await;
        let mut profile = fixture.profile();
        profile.ssh.password = Some("not-the-owned-password".into());
        assert_eq!(
            manager.start(&profile).await.unwrap_err().to_string(),
            "ssh_password_rejected"
        );
        assert_eq!(manager.status(profile.id).unwrap().state, "failed");
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn a_method_the_server_does_not_offer_is_named_before_any_credential_is_sent() {
        let fixture = Fixture::offering(&[MethodKind::PublicKey]).await;
        let manager = TunnelManager::new().await;
        let profile = fixture.profile();
        assert_eq!(
            manager.start(&profile).await.unwrap_err().to_string(),
            "ssh_password_auth_disabled"
        );
        // Only the credential-free probe reached the server.
        assert_eq!(fixture.auth_calls.load(Ordering::Relaxed), 1);
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn half_closed_client_still_receives_the_whole_answer() {
        let fixture = Fixture::start().await;
        let manager = TunnelManager::new().await;
        let started = manager.start(&fixture.profile()).await.unwrap();
        let mut stream =
            TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, started.local_port.unwrap()))
                .await
                .unwrap();
        let payload = (0..300_000).map(|i| (i % 239) as u8).collect::<Vec<_>>();
        stream.write_all(&payload).await.unwrap();
        // Like `printf request | nc`: finish sending, then wait for the reply.
        stream.shutdown().await.unwrap();
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut received))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(received, payload);
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn unreachable_target_is_reported_while_the_tunnel_stays_up() {
        let fixture = Fixture::start().await;
        let manager = TunnelManager::new().await;
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut profile = fixture.profile();
        profile.target_port = closed.local_addr().unwrap().port();
        drop(closed);
        let started = manager.start(&profile).await.unwrap();
        let local = started.local_port.unwrap();
        let attempt = |manager: TunnelManager, id: Uuid| async move {
            let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, local))
                .await
                .unwrap();
            let mut end = Vec::new();
            tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut end))
                .await
                .unwrap()
                .ok();
            assert!(end.is_empty());
            manager.status(id).unwrap()
        };
        let status = attempt(manager.clone(), profile.id).await;
        assert_eq!(status.state, "active");
        assert_eq!(status.error.as_deref(), Some("tunnel_target_refused"));
        manager.stop(profile.id).await.unwrap();
        // A gateway that refuses the destination itself.
        let mut prohibited = fixture.profile();
        prohibited.target_host = "192.0.2.1".into();
        let started = manager.start(&prohibited).await.unwrap();
        let mut stream =
            TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, started.local_port.unwrap()))
                .await
                .unwrap();
        let mut end = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut end)).await;
        assert_eq!(
            manager.status(prohibited.id).unwrap().error.as_deref(),
            Some("tunnel_forwarding_prohibited")
        );
        manager.shutdown().await;
    }
    #[tokio::test]
    async fn a_pool_of_connections_fits_and_the_cap_is_reported() {
        let fixture = Fixture::start().await;
        let manager = TunnelManager::new().await;
        let profile = fixture.profile();
        let local = manager.start(&profile).await.unwrap().local_port.unwrap();
        let mut pool = Vec::new();
        for index in 0..MAX_TUNNEL_CLIENTS {
            let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, local))
                .await
                .unwrap();
            let message = [index as u8; 64];
            stream.write_all(&message).await.unwrap();
            let mut echoed = [0u8; 64];
            tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut echoed))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(echoed, message);
            pool.push(stream);
        }
        let status = manager.status(profile.id).unwrap();
        assert_eq!(status.active_clients, MAX_TUNNEL_CLIENTS);
        assert!(status.error.is_none());
        let mut extra = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, local))
            .await
            .unwrap();
        let mut end = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), extra.read_to_end(&mut end)).await;
        assert_eq!(
            manager.status(profile.id).unwrap().error.as_deref(),
            Some("tunnel_client_limit")
        );
        // Earlier connections are untouched by the refused one.
        pool[0].write_all(b"still-open").await.unwrap();
        let mut echoed = [0u8; 10];
        pool[0].read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"still-open");
        drop(pool);
        manager.shutdown().await;
    }
}
