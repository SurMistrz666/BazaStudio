//! End-to-end check against a real SSH server (for example OpenSSH `sshd`).
//! Ignored by default; describe the server with environment variables and run
//!
//! ```text
//! cargo test -p baza-core --test real_sshd -- --ignored --nocapture
//! ```
//!
//! Required: `BAZA_SSH_TEST_PORT`, `BAZA_SSH_TEST_USER`, `BAZA_SSH_TEST_KEY`
//! (path to a private key the server accepts) and `BAZA_SSH_TEST_TARGET_PORT`
//! (an HTTP server reachable from the SSH server).
//! Optional: `BAZA_SSH_TEST_HOST` and `BAZA_SSH_TEST_TARGET_HOST` (default
//! `127.0.0.1`), `BAZA_SSH_TEST_KEY_PASSPHRASE`, `BAZA_SSH_TEST_FINGERPRINT`
//! (expected `SHA256:…` value), `BAZA_SSH_TEST_HTTP_PATH`,
//! `BAZA_SSH_TEST_EXPECT` (text the HTTP answer must contain),
//! `BAZA_SSH_TEST_SHA256` (hexadecimal SHA256 of the HTTP body),
//! `BAZA_SSH_TEST_ALTERNATE_FINGERPRINT` (another host key of the same server),
//! `BAZA_SSH_TEST_CLOSED_PORT` (a port on the target host with no listener) and
//! `BAZA_SSH_TEST_PASSWORD_DISABLED=1` (the server offers key sign-in only).
//! The HTTP server must accept 24 connections at once; `python3 -m http.server`
//! has a backlog of 5 and resets a burst even without a tunnel.
use baza_core::{
    TunnelManager,
    model::{SshAuth, SshProfile, TunnelProfile},
    ssh::{self, scan_host_key},
};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

fn variable(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}
fn required(name: &str) -> String {
    variable(name).unwrap_or_else(|| panic!("{name} is required"))
}

/// Body of an HTTP/1.0 answer fetched through the forwarded port.
async fn http_get(local: u16, path: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(("127.0.0.1", local)).await.unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes())
        .await
        .unwrap();
    // Finish sending first, like `printf … | nc`; the answer must still arrive.
    stream.shutdown().await.unwrap();
    let mut body = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), stream.read_to_end(&mut body))
        .await
        .expect("no answer through the tunnel")
        .unwrap();
    assert!(body.starts_with(b"HTTP/1."), "not an HTTP answer");
    // Headers carry a date, so only the body is comparable between requests.
    let start = body
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP header end")
        + 4;
    body.split_off(start)
}

async fn start_error(manager: &TunnelManager, profile: &TunnelProfile) -> String {
    let code = manager.start(profile).await.unwrap_err().to_string();
    println!("  {code}: {}", ssh::describe(&code, true));
    assert!(ssh::describe_known(&code, false).is_some(), "{code}");
    code
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a real SSH server described by BAZA_SSH_TEST_* variables"]
async fn scan_start_forward_and_stop_against_a_real_ssh_server() {
    let host = variable("BAZA_SSH_TEST_HOST").unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = required("BAZA_SSH_TEST_PORT").parse().unwrap();
    let key_path = required("BAZA_SSH_TEST_KEY");
    let key = std::fs::read_to_string(&key_path).unwrap();
    let target_host = variable("BAZA_SSH_TEST_TARGET_HOST").unwrap_or_else(|| "127.0.0.1".into());
    let target_port: u16 = required("BAZA_SSH_TEST_TARGET_PORT").parse().unwrap();
    let path = variable("BAZA_SSH_TEST_HTTP_PATH").unwrap_or_else(|| "/".into());

    let scan = scan_host_key(&host, port).await.unwrap();
    println!("scanned {} {}", scan.algorithm, scan.fingerprint);
    if let Some(expected) = variable("BAZA_SSH_TEST_FINGERPRINT") {
        assert_eq!(scan.fingerprint, expected);
    }
    let mut profile = TunnelProfile {
        id: uuid::Uuid::new_v4(),
        name: "Real SSH server".into(),
        ssh: SshProfile {
            host: host.clone(),
            port,
            username: required("BAZA_SSH_TEST_USER"),
            auth: SshAuth::PrivateKey,
            password: None,
            private_key: Some(key),
            passphrase: variable("BAZA_SSH_TEST_KEY_PASSPHRASE"),
            // Like a new profile: nobody typed a fingerprint.
            fingerprint: String::new(),
        },
        target_host,
        target_port,
        local_port: 0,
    };
    ssh::validate_tunnel(&profile, true).unwrap();

    let manager = TunnelManager::new().await;
    // First connection: the key is trusted on first use and reported back.
    let first = manager.start(&profile).await.unwrap();
    assert_eq!(first.state, "active");
    let learned = first
        .host_key
        .expect("the first connection reports the host key");
    assert_eq!(learned, scan);
    println!("first use learned {}", learned.fingerprint);
    manager.stop(profile.id).await.unwrap();
    // What the application saves afterwards is enforced from then on.
    profile.ssh.fingerprint = learned.fingerprint.clone();
    ssh::validate_tunnel(&profile, true).unwrap();
    let started = manager.start(&profile).await.unwrap();
    assert_eq!(started.state, "active");
    assert_eq!(started.host_key.as_ref(), Some(&learned));
    let local = started.local_port.unwrap();
    println!("key {key_path}: forwarding 127.0.0.1:{local}");

    let answer = http_get(local, &path).await;
    if let Some(expected) = variable("BAZA_SSH_TEST_EXPECT") {
        assert!(String::from_utf8_lossy(&answer).contains(&expected));
    }
    if let Some(expected) = variable("BAZA_SSH_TEST_SHA256") {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(&answer)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(digest, expected);
        println!("body of {} B matches the expected SHA256", answer.len());
    }
    // A pool of simultaneous connections, as a database tool would open.
    let pool = (0..24)
        .map(|_| {
            let path = path.clone();
            tokio::spawn(async move { http_get(local, &path).await })
        })
        .collect::<Vec<_>>();
    for request in pool {
        assert_eq!(request.await.unwrap(), answer);
    }
    let status = manager.status(profile.id).unwrap();
    println!(
        "sent {} B, received {} B, error {:?}",
        status.bytes_sent, status.bytes_received, status.error
    );
    assert_eq!(status.state, "active");
    assert!(status.bytes_sent > 0 && status.bytes_received as usize >= answer.len() * 25);
    assert!(status.error.is_none());

    if let Some(alternate) = variable("BAZA_SSH_TEST_ALTERNATE_FINGERPRINT") {
        // The administrator may hand out the fingerprint of any of the server's keys.
        let mut other_key = profile.clone();
        other_key.id = uuid::Uuid::new_v4();
        other_key.ssh.fingerprint = alternate;
        let local = manager.start(&other_key).await.unwrap().local_port.unwrap();
        assert_eq!(http_get(local, &path).await, answer);
        println!("alternate host key fingerprint accepted");
        manager.stop(other_key.id).await.unwrap();
    }

    if let Some(closed) = variable("BAZA_SSH_TEST_CLOSED_PORT") {
        let mut refused = profile.clone();
        refused.id = uuid::Uuid::new_v4();
        refused.target_port = closed.parse().unwrap();
        let local = manager.start(&refused).await.unwrap().local_port.unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", local)).await.unwrap();
        let mut end = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut end)).await;
        assert!(end.is_empty());
        let status = manager.status(refused.id).unwrap();
        println!("closed target: {:?}", status.error);
        assert_eq!(status.state, "active");
        assert_eq!(status.error.as_deref(), Some("tunnel_target_refused"));
        manager.stop(refused.id).await.unwrap();
    }

    println!("expected failures:");
    let mut wrong_pin = profile.clone();
    wrong_pin.id = uuid::Uuid::new_v4();
    wrong_pin.ssh.fingerprint = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into();
    let refused = manager.start(&wrong_pin).await.unwrap_err();
    assert_eq!(refused.to_string(), "ssh_host_key_changed");
    println!(
        "  ssh_host_key_changed: {}",
        ssh::describe("ssh_host_key_changed", true)
    );
    // The key the server really presents is reported for an explicit decision.
    let changed = refused.downcast_ref::<ssh::HostKeyChanged>().unwrap();
    assert_eq!(changed.presented, learned);
    let failed = manager.status(wrong_pin.id).unwrap();
    assert_eq!(failed.state, "failed");
    assert_eq!(failed.host_key.as_ref(), Some(&learned));
    println!("  server presents {}", changed.presented.fingerprint);
    let mut wrong_user = profile.clone();
    wrong_user.id = uuid::Uuid::new_v4();
    wrong_user.ssh.username = "baza-no-such-user".into();
    let code = start_error(&manager, &wrong_user).await;
    assert!(
        matches!(
            code.as_str(),
            "ssh_key_rejected" | "ssh_authentication_failed"
        ),
        "{code}"
    );
    if variable("BAZA_SSH_TEST_PASSWORD_DISABLED").is_some() {
        let mut password = profile.clone();
        password.id = uuid::Uuid::new_v4();
        password.ssh.auth = SshAuth::Password;
        password.ssh.private_key = None;
        password.ssh.password = Some("not-a-real-password".into());
        assert_eq!(
            start_error(&manager, &password).await,
            "ssh_password_auth_disabled"
        );
    }
    let mut unresolved = profile.clone();
    unresolved.id = uuid::Uuid::new_v4();
    unresolved.ssh.host = "baza-no-such-host.invalid".into();
    assert_eq!(start_error(&manager, &unresolved).await, "ssh_dns_failed");

    let stopped = manager.stop(profile.id).await.unwrap();
    assert_eq!(stopped.state, "stopped");
    assert!(TcpStream::connect(("127.0.0.1", local)).await.is_err());
    manager.shutdown().await;
}
