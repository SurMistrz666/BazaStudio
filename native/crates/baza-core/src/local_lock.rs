//! Optional, local-only application unlock. No website account or network access.
use crate::Vault;
use anyhow::{Result, anyhow, ensure};
use argon2::{Algorithm, Argon2, Params, Version};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::{
    sync::{Arc, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

const RECORD: &str = "local-lock";
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LockSettings {
    pub enabled: bool,
    pub two_factor: bool,
    pub idle_minutes: u32,
}
pub struct Enrollment {
    pub secret: Option<String>,
    pub otpauth_uri: Option<String>,
    pub token: String,
}
impl Drop for Enrollment {
    fn drop(&mut self) {
        self.secret.zeroize();
        self.otpauth_uri.zeroize();
        self.token.zeroize();
    }
}
#[derive(Serialize, Deserialize, Zeroize)]
#[serde(deny_unknown_fields)]
struct Record {
    v: u8,
    enabled: bool,
    idle_minutes: u32,
    salt: [u8; 16],
    password: [u8; 32],
    totp: Option<[u8; 20]>,
    last_step: i64,
    recovery: Vec<[u8; 32]>,
    failures: u32,
    retry_at: u64,
    last_seen: u64,
    #[serde(default)]
    setup_failures: u32,
    #[serde(default)]
    setup_retry_at: u64,
}
impl Default for Record {
    fn default() -> Self {
        Self {
            v: 1,
            enabled: false,
            idle_minutes: 15,
            salt: [0; 16],
            password: [0; 32],
            totp: None,
            last_step: -1,
            recovery: vec![],
            failures: 0,
            retry_at: 0,
            last_seen: 0,
            setup_failures: 0,
            setup_retry_at: 0,
        }
    }
}
impl Drop for Record {
    fn drop(&mut self) {
        self.zeroize();
    }
}
struct Pending {
    record: Record,
    token: Zeroizing<String>,
    created: Instant,
    expected_revision: [u8; 32],
    attempts: u8,
}
#[derive(Clone, Copy)]
struct Verified {
    at: Instant,
    revision: [u8; 32],
}
struct State {
    pending: Option<Pending>,
    verified: Option<Verified>,
}
pub struct LocalLock {
    vault: Arc<Vault>,
    state: Mutex<State>,
}
fn now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow!("local_clock_invalid"))?
        .as_secs())
}
fn parse(raw: Option<&[u8]>) -> Result<Record> {
    let record: Record = match raw {
        Some(bytes) => serde_json::from_slice(bytes).map_err(|_| anyhow!("local_lock_invalid"))?,
        None => Record::default(),
    };
    ensure!(
        record.v == 1
            && matches!(record.idle_minutes, 5 | 15 | 30 | 60)
            && record.recovery.len() <= 10
            && record.failures <= 32
            && record.setup_failures <= 32,
        "local_lock_invalid"
    );
    Ok(record)
}
// Sorted field names, integer values and borrowed secret arrays give a canonical
// plaintext revision without retaining a second, unzeroized JSON value tree.
fn revision(record: &Record) -> Result<[u8; 32]> {
    use serde::ser::{SerializeMap, Serializer};
    let mut bytes = Zeroizing::new(Vec::new());
    let mut serializer = serde_json::Serializer::new(&mut *bytes);
    let mut map = serializer
        .serialize_map(Some(13))
        .map_err(|_| anyhow!("local_lock_invalid"))?;
    map.serialize_entry("enabled", &record.enabled)?;
    map.serialize_entry("failures", &record.failures)?;
    map.serialize_entry("idle_minutes", &record.idle_minutes)?;
    map.serialize_entry("last_seen", &record.last_seen)?;
    map.serialize_entry("last_step", &record.last_step)?;
    map.serialize_entry("password", &record.password)?;
    map.serialize_entry("recovery", &record.recovery)?;
    map.serialize_entry("retry_at", &record.retry_at)?;
    map.serialize_entry("salt", &record.salt)?;
    map.serialize_entry("setup_failures", &record.setup_failures)?;
    map.serialize_entry("setup_retry_at", &record.setup_retry_at)?;
    map.serialize_entry("totp", &record.totp)?;
    map.serialize_entry("v", &record.v)?;
    map.end()?;
    Ok(Sha256::digest(bytes.as_slice()).into())
}
fn same_revision(left: &[u8; 32], right: &[u8; 32]) -> bool {
    bool::from(left.ct_eq(right))
}
fn require_recent(state: &State, current: &[u8; 32]) -> Result<Verified> {
    let verified = state
        .verified
        .ok_or_else(|| anyhow!("local_reauthentication_required"))?;
    ensure!(
        verified.at.elapsed().as_secs() < 300,
        "local_reauthentication_required"
    );
    ensure!(
        same_revision(&verified.revision, current),
        "local_lock_changed"
    );
    Ok(verified)
}
fn valid_clock(record: &Record, stamp: u64) -> Result<()> {
    ensure!(
        stamp.saturating_add(120) >= record.last_seen,
        "local_clock_invalid"
    );
    Ok(())
}
fn password_hash(password: &str, salt: &[u8; 16]) -> Result<Zeroizing<[u8; 32]>> {
    ensure!(
        (12..=256).contains(&password.chars().count())
            && password.len() <= 1024
            && !password.contains('\0'),
        "local_password_length"
    );
    let params = Params::new(65536, 3, 1, Some(32)).map_err(|_| anyhow!("local_kdf_invalid"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut output = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(password.as_bytes(), salt, output.as_mut())
        .map_err(|_| anyhow!("local_kdf_failed"))?;
    Ok(output)
}
fn otp(secret: &[u8; 20], step: u64) -> Result<String> {
    let mut mac =
        Hmac::<Sha1>::new_from_slice(secret).map_err(|_| anyhow!("local_totp_invalid"))?;
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[19] & 15) as usize;
    let value = (u32::from_be_bytes(
        digest[offset..offset + 4]
            .try_into()
            .map_err(|_| anyhow!("local_totp_invalid"))?,
    ) & 0x7fff_ffff)
        % 1_000_000;
    Ok(format!("{value:06}"))
}
fn accepted(secret: &[u8; 20], code: &str, last: i64, stamp: u64) -> Result<Option<i64>> {
    if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(None);
    }
    let step = (stamp / 30) as i64;
    for candidate in [step, step - 1, step + 1] {
        if candidate >= 0
            && candidate > last
            && bool::from(
                otp(secret, candidate as u64)?
                    .as_bytes()
                    .ct_eq(code.as_bytes()),
            )
        {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}
fn base32(bytes: &[u8]) -> String {
    const CHARS: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut result = String::new();
    let mut buffer = 0u32;
    let mut bits = 0;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            result.push(CHARS[((buffer >> bits) & 31) as usize] as char)
        }
    }
    if bits > 0 {
        result.push(CHARS[((buffer << (5 - bits)) & 31) as usize] as char)
    }
    result
}
fn recovery_code() -> String {
    let raw: [u8; 16] = rand::random();
    raw.chunks(4)
        .map(|part| part.iter().map(|b| format!("{b:02X}")).collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}
fn recovery_hash(code: &str) -> [u8; 32] {
    Sha256::digest(code.trim().to_ascii_uppercase().as_bytes()).into()
}
impl LocalLock {
    pub fn open(vault: Arc<Vault>) -> Result<Self> {
        let lock = Self {
            vault,
            state: Mutex::new(State {
                pending: None,
                verified: None,
            }),
        };
        lock.status()?;
        Ok(lock)
    }
    pub fn status(&self) -> Result<LockSettings> {
        let raw = self.vault.load_named(RECORD)?;
        let record = parse(raw.as_deref().map(|v| v.as_slice()))?;
        Ok(LockSettings {
            enabled: record.enabled,
            two_factor: record.totp.is_some(),
            idle_minutes: record.idle_minutes,
        })
    }
    pub fn verify(&self, password: &str, code_or_recovery: Option<&str>) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| anyhow!("local_lock_busy"))?;
        self.verify_locked(&mut state, password, code_or_recovery)
    }
    fn verify_locked(
        &self,
        state: &mut State,
        password: &str,
        code_or_recovery: Option<&str>,
    ) -> Result<()> {
        // Clear before even reading the clock/vault: every failed attempt,
        // including a backoff or a corrupt record, revokes prior recent auth.
        state.verified = None;
        state.pending = None;
        let stamp = now()?;
        let (result, current) = self.vault.update_named(RECORD, |raw| {
            let mut record = parse(raw)?;
            let mut outcome = Ok(());
            if record.enabled {
                if stamp.saturating_add(120) < record.last_seen {
                    outcome = Err("local_clock_invalid")
                } else if stamp < record.retry_at {
                    outcome = Err("local_retry_later")
                } else {
                    let valid_password = password_hash(password, &record.salt)
                        .map(|candidate| bool::from(candidate.as_ref().ct_eq(&record.password)))
                        .unwrap_or(false);
                    let mut factor = true;
                    if let Some(secret) = record.totp {
                        factor = false;
                        if valid_password {
                            let code = code_or_recovery.unwrap_or("");
                            if let Some(step) = accepted(&secret, code, record.last_step, stamp)? {
                                record.last_step = step;
                                factor = true;
                            } else if code.len() <= 128 {
                                let hash = recovery_hash(code);
                                if let Some(index) = record
                                    .recovery
                                    .iter()
                                    .position(|item| bool::from(item.ct_eq(&hash)))
                                {
                                    record.recovery.remove(index);
                                    factor = true;
                                }
                            }
                        }
                    }
                    if valid_password && factor {
                        record.failures = 0;
                        record.retry_at = 0;
                    } else {
                        record.failures = (record.failures + 1).min(32);
                        record.retry_at =
                            stamp.saturating_add(2u64.saturating_pow(record.failures.min(8)));
                        outcome = Err("local_unlock_invalid");
                    }
                    record.last_seen = record.last_seen.max(stamp);
                }
            }
            let current = revision(&record)?;
            let bytes = serde_json::to_vec(&record).map_err(|_| anyhow!("local_lock_invalid"))?;
            Ok((bytes, (outcome, current)))
        })?;
        result.map_err(|code| anyhow!(code))?;
        state.verified = Some(Verified {
            at: Instant::now(),
            revision: current,
        });
        Ok(())
    }
    pub fn setup(&self, password: &str, enable_totp: bool) -> Result<Enrollment> {
        let mut state = self.state.lock().map_err(|_| anyhow!("local_lock_busy"))?;
        state.pending = None;
        let raw = self.vault.load_named(RECORD)?;
        let current = parse(raw.as_deref().map(|value| value.as_slice()))?;
        let expected = revision(&current)?;
        if current.enabled
            && let Err(error) = require_recent(&state, &expected)
        {
            state.verified = None;
            return Err(error);
        }
        let salt: [u8; 16] = rand::random();
        let hash = password_hash(password, &salt)?;
        let mut record = Record::default();
        record.enabled = true;
        record.salt = salt;
        record.password = *hash;
        record.totp = enable_totp.then(rand::random);
        let stamp = now()?;
        let result = self.vault.update_named(RECORD, |raw| {
            let mut current = parse(raw)?;
            let before = revision(&current)?;
            ensure!(same_revision(&before, &expected), "local_lock_changed");
            if current.enabled {
                require_recent(&state, &before)?;
            }
            valid_clock(&current, stamp)?;
            ensure!(stamp >= current.setup_retry_at, "local_retry_later");
            if current.setup_failures >= 10 {
                current.setup_failures = 0;
                current.setup_retry_at = 0;
            }
            record.idle_minutes = current.idle_minutes;
            let after = revision(&current)?;
            Ok((serde_json::to_vec(&current)?, after))
        });
        let expected_revision = match result {
            Ok(value) => value,
            Err(error) => {
                state.verified = None;
                return Err(error);
            }
        };
        if let Some(verified) = state.verified.as_mut() {
            verified.revision = expected_revision;
        }
        let secret = record.totp.as_ref().map(|value| base32(value));
        let uri=secret.as_ref().map(|value|format!("otpauth://totp/Baza%20Studio%3Alocal?secret={value}&issuer=Baza%20Studio&algorithm=SHA1&digits=6&period=30"));
        let token = uuid::Uuid::new_v4().to_string();
        state.pending = Some(Pending {
            record,
            token: Zeroizing::new(token.clone()),
            created: Instant::now(),
            expected_revision,
            attempts: 0,
        });
        Ok(Enrollment {
            secret,
            otpauth_uri: uri,
            token,
        })
    }
    pub fn confirm_setup(&self, token: &str, totp: Option<&str>) -> Result<Vec<String>> {
        let mut state = self.state.lock().map_err(|_| anyhow!("local_lock_busy"))?;
        let mut pending = state
            .pending
            .take()
            .ok_or_else(|| anyhow!("local_enrollment_missing"))?;
        if pending.created.elapsed().as_secs() >= 600 || pending.attempts >= 10 {
            state.verified = None;
            return Err(anyhow!("local_enrollment_expired"));
        }
        pending.attempts += 1;
        let stamp = match now() {
            Ok(stamp) => stamp,
            Err(error) => {
                state.verified = None;
                return Err(error);
            }
        };
        let result = self.vault.update_named(RECORD, |raw| {
            let mut current = parse(raw)?;
            let before = revision(&current)?;
            ensure!(
                same_revision(&before, &pending.expected_revision),
                "local_lock_changed"
            );
            if current.enabled {
                require_recent(&state, &before)?;
            }
            valid_clock(&current, stamp)?;
            ensure!(stamp >= current.setup_retry_at, "local_retry_later");
            let token_valid = bool::from(pending.token.as_bytes().ct_eq(token.as_bytes()));
            let step = match pending.record.totp.as_ref() {
                Some(secret) => accepted(secret, totp.unwrap_or(""), -1, stamp)?,
                None => Some(-1),
            };
            if !token_valid || step.is_none() {
                current.setup_failures = (current.setup_failures + 1).min(32);
                if current.setup_failures >= 10 {
                    current.setup_retry_at = stamp.saturating_add(60);
                }
                current.last_seen = current.last_seen.max(stamp);
                let after = revision(&current)?;
                let code = if token_valid {
                    "local_totp_invalid"
                } else {
                    "local_enrollment_invalid"
                };
                return Ok((serde_json::to_vec(&current)?, (Err(code), after)));
            }
            pending.record.last_step = step.unwrap_or(-1);
            pending.record.idle_minutes = current.idle_minutes;
            let codes: Vec<String> = if pending.record.totp.is_some() {
                (0..10).map(|_| recovery_code()).collect()
            } else {
                vec![]
            };
            pending.record.recovery = codes.iter().map(|value| recovery_hash(value)).collect();
            pending.record.last_seen = current.last_seen.max(stamp);
            let after = revision(&pending.record)?;
            Ok((serde_json::to_vec(&pending.record)?, (Ok(codes), after)))
        });
        match result {
            Ok((Ok(codes), current)) => {
                state.verified = Some(Verified {
                    at: Instant::now(),
                    revision: current,
                });
                Ok(codes)
            }
            Ok((Err(code), current)) => {
                state.verified = None;
                pending.expected_revision = current;
                if pending.attempts < 10 {
                    state.pending = Some(pending);
                }
                Err(anyhow!(code))
            }
            Err(error) => {
                state.verified = None;
                Err(error)
            }
        }
    }
    pub fn change_idle(&self, minutes: u32) -> Result<()> {
        ensure!(matches!(minutes, 5 | 15 | 30 | 60), "local_idle_invalid");
        let mut state = self.state.lock().map_err(|_| anyhow!("local_lock_busy"))?;
        let stamp = now()?;
        let result = self.vault.update_named(RECORD, |raw| {
            let mut record = parse(raw)?;
            let before = revision(&record)?;
            let verified = if record.enabled {
                Some(require_recent(&state, &before)?)
            } else {
                None
            };
            valid_clock(&record, stamp)?;
            record.idle_minutes = minutes;
            record.last_seen = record.last_seen.max(stamp);
            let current = revision(&record)?;
            Ok((
                serde_json::to_vec(&record)?,
                verified.map(|old| Verified {
                    at: old.at,
                    revision: current,
                }),
            ))
        });
        match result {
            Ok(verified) => {
                state.verified = verified;
                state.pending = None;
                Ok(())
            }
            Err(error) => {
                state.verified = None;
                state.pending = None;
                Err(error)
            }
        }
    }
    pub fn disable(&self, password: &str, code: Option<&str>) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| anyhow!("local_lock_busy"))?;
        self.verify_locked(&mut state, password, code)?;
        let expected = state
            .verified
            .ok_or_else(|| anyhow!("local_reauthentication_required"))?
            .revision;
        self.disable_verified_locked(&mut state, &expected)
    }
    fn disable_verified_locked(&self, state: &mut State, expected: &[u8; 32]) -> Result<()> {
        let stamp = now()?;
        let result = self.vault.update_named(RECORD, |raw| {
            let current = parse(raw)?;
            let before = revision(&current)?;
            ensure!(same_revision(&before, expected), "local_lock_changed");
            require_recent(state, &before)?;
            valid_clock(&current, stamp)?;
            let mut disabled = Record::default();
            disabled.last_seen = current.last_seen.max(stamp);
            // Keep the persisted enrollment budget: disabling cannot be used
            // to bypass the cross-instance factor-attempt cooldown.
            disabled.setup_failures = current.setup_failures;
            disabled.setup_retry_at = current.setup_retry_at;
            Ok((serde_json::to_vec(&disabled)?, ()))
        });
        state.verified = None;
        state.pending = None;
        result
    }
    pub fn forget_unlock(&self) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| anyhow!("local_lock_busy"))?;
        state.verified = None;
        state.pending = None;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rfc6238_vectors_and_monotonic_code_replay() {
        let secret = *b"12345678901234567890";
        for (stamp, expected) in [
            (59, "287082"),
            (1111111109, "081804"),
            (1111111111, "050471"),
            (1234567890, "005924"),
            (2000000000, "279037"),
            (20000000000, "353130"),
        ] {
            assert_eq!(otp(&secret, stamp / 30).unwrap(), expected)
        }
        assert_eq!(accepted(&secret, "287082", -1, 59).unwrap(), Some(1));
        assert_eq!(accepted(&secret, "287082", 1, 59).unwrap(), None);
        assert_eq!(base32(&secret), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
    }
    #[test]
    fn password_kdf_distinguishes_unicode_suffix() {
        let salt = [9u8; 16];
        let prefix = "ż".repeat(72);
        let a = password_hash(&(prefix.clone() + "a"), &salt).unwrap();
        let b = password_hash(&(prefix + "b"), &salt).unwrap();
        assert!(!bool::from(a.as_ref().ct_eq(b.as_ref())));
    }
    #[test]
    fn canonical_revision_covers_every_field_and_legacy_defaults() {
        let base = Record::default();
        let expected = revision(&base).unwrap();
        let bytes = serde_json::to_vec(&base).unwrap();
        let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        json.as_object_mut().unwrap().remove("setup_failures");
        json.as_object_mut().unwrap().remove("setup_retry_at");
        let legacy = serde_json::to_vec_pretty(&json).unwrap();
        assert_eq!(revision(&parse(Some(&legacy)).unwrap()).unwrap(), expected);
        for field in [
            "enabled",
            "failures",
            "idle_minutes",
            "last_seen",
            "last_step",
            "password",
            "recovery",
            "retry_at",
            "salt",
            "setup_failures",
            "setup_retry_at",
            "totp",
            "v",
        ] {
            let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            changed[field] = match field {
                "enabled" => serde_json::json!(true),
                "idle_minutes" => serde_json::json!(30),
                "last_step" => serde_json::json!(2),
                "password" => serde_json::to_value([1u8; 32]).unwrap(),
                "salt" => serde_json::to_value([1u8; 16]).unwrap(),
                "recovery" => serde_json::json!(vec![[1u8; 32]]),
                "totp" => serde_json::to_value([1u8; 20]).unwrap(),
                _ => serde_json::json!(1),
            };
            if field == "v" {
                changed[field] = serde_json::json!(2);
            }
            let changed: Record = serde_json::from_value(changed).unwrap();
            assert_ne!(revision(&changed).unwrap(), expected, "field {field}");
        }
    }
    #[cfg(any(windows, target_os = "macos"))]
    struct Owned {
        _directory: tempfile::TempDir,
        vault: Arc<Vault>,
        one: LocalLock,
        two: LocalLock,
    }
    #[cfg(any(windows, target_os = "macos"))]
    impl Owned {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let namespace = format!("BazaStudio.QA.{}", uuid::Uuid::new_v4());
            let root = directory.path().join("private");
            let vault = Arc::new(Vault::open_owned(&root, &namespace).unwrap());
            let second = Arc::new(Vault::open_owned(&root, &namespace).unwrap());
            let one = LocalLock::open(vault.clone()).unwrap();
            let two = LocalLock::open(second).unwrap();
            Self {
                _directory: directory,
                vault,
                one,
                two,
            }
        }
        fn enable(&self) {
            let enrollment = self.one.setup("Owned-Local-Password-2026", false).unwrap();
            self.one.confirm_setup(&enrollment.token, None).unwrap();
        }
    }
    #[cfg(any(windows, target_os = "macos"))]
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.vault.remove_owned_qa_key();
        }
    }
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn two_instances_cannot_confirm_an_obsolete_initial_enrollment() {
        let owned = Owned::new();
        let first = owned.one.setup("First-Local-Password-2026", false).unwrap();
        let second = owned
            .two
            .setup("Second-Local-Password-2026", false)
            .unwrap();
        owned.two.confirm_setup(&second.token, None).unwrap();
        assert_eq!(
            owned
                .one
                .confirm_setup(&first.token, None)
                .unwrap_err()
                .to_string(),
            "local_lock_changed"
        );
        assert!(owned.one.state.lock().unwrap().pending.is_none());
        owned
            .one
            .verify("Second-Local-Password-2026", None)
            .unwrap();
        assert!(owned.one.status().unwrap().enabled);
    }
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn pending_reenrollment_cannot_restore_a_disabled_lock() {
        let owned = Owned::new();
        owned.enable();
        let pending = owned.one.setup("Replacement-Password-2026", true).unwrap();
        owned
            .two
            .disable("Owned-Local-Password-2026", None)
            .unwrap();
        assert_eq!(
            owned
                .one
                .confirm_setup(&pending.token, Some("invalid"))
                .unwrap_err()
                .to_string(),
            "local_lock_changed"
        );
        assert!(!owned.one.status().unwrap().enabled);
    }
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn idle_change_requires_current_revision_and_keeps_original_auth_deadline() {
        let owned = Owned::new();
        owned.enable();
        let original = owned.one.state.lock().unwrap().verified.unwrap().at;
        owned.one.change_idle(30).unwrap();
        assert_eq!(
            owned.one.state.lock().unwrap().verified.unwrap().at,
            original
        );
        owned.two.verify("Owned-Local-Password-2026", None).unwrap();
        owned.two.change_idle(60).unwrap();
        assert_eq!(
            owned.one.change_idle(5).unwrap_err().to_string(),
            "local_lock_changed"
        );
        assert_eq!(owned.one.status().unwrap().idle_minutes, 60);
        assert!(owned.one.state.lock().unwrap().verified.is_none());
    }
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn disable_second_phase_cannot_overwrite_a_newer_enrollment() {
        let owned = Owned::new();
        owned.enable();
        // Deterministically pause at the same boundary used by public disable:
        // successful verification has committed, the final CAS has not.
        let expected = owned.one.state.lock().unwrap().verified.unwrap().revision;
        owned.two.verify("Owned-Local-Password-2026", None).unwrap();
        let enrollment = owned.two.setup("Newer-Local-Password-2026", false).unwrap();
        owned.two.confirm_setup(&enrollment.token, None).unwrap();
        let mut state = owned.one.state.lock().unwrap();
        assert_eq!(
            owned
                .one
                .disable_verified_locked(&mut state, &expected)
                .unwrap_err()
                .to_string(),
            "local_lock_changed"
        );
        assert!(state.verified.is_none());
        drop(state);
        assert!(owned.one.status().unwrap().enabled);
        owned.one.verify("Newer-Local-Password-2026", None).unwrap();
    }
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn failed_verification_and_backoff_revoke_previous_recent_auth() {
        let owned = Owned::new();
        owned.enable();
        assert!(owned.one.state.lock().unwrap().verified.is_some());
        assert_eq!(
            owned.one.verify("bad", None).unwrap_err().to_string(),
            "local_unlock_invalid"
        );
        assert!(owned.one.state.lock().unwrap().verified.is_none());
        assert_eq!(
            owned.one.change_idle(30).unwrap_err().to_string(),
            "local_reauthentication_required"
        );
        // A previous successful value cannot survive an early retry rejection.
        let raw = owned.vault.load_named(RECORD).unwrap().unwrap();
        let current = parse(Some(&raw)).unwrap();
        owned.one.state.lock().unwrap().verified = Some(Verified {
            at: Instant::now(),
            revision: revision(&current).unwrap(),
        });
        assert_eq!(
            owned
                .one
                .verify("Owned-Local-Password-2026", None)
                .unwrap_err()
                .to_string(),
            "local_retry_later"
        );
        assert!(owned.one.state.lock().unwrap().verified.is_none());
        assert!(
            owned
                .one
                .setup("Another-Local-Password-2026", false)
                .is_err()
        );
    }
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn enrollment_deadline_and_cross_instance_factor_budget_are_enforced() {
        let owned = Owned::new();
        let expired = owned.one.setup("Owned-Local-Password-2026", true).unwrap();
        owned
            .one
            .state
            .lock()
            .unwrap()
            .pending
            .as_mut()
            .unwrap()
            .created = Instant::now() - std::time::Duration::from_secs(601);
        assert_eq!(
            owned
                .one
                .confirm_setup(&expired.token, Some("invalid"))
                .unwrap_err()
                .to_string(),
            "local_enrollment_expired"
        );
        assert!(!owned.one.status().unwrap().enabled);
        let enrollment = owned.one.setup("Owned-Local-Password-2026", true).unwrap();
        for _ in 0..10 {
            assert_eq!(
                owned
                    .one
                    .confirm_setup(&enrollment.token, Some("invalid"))
                    .unwrap_err()
                    .to_string(),
                "local_totp_invalid"
            );
        }
        assert!(owned.one.state.lock().unwrap().pending.is_none());
        assert!(!owned.one.status().unwrap().enabled);
        let raw = owned.vault.load_named(RECORD).unwrap().unwrap();
        let record = parse(Some(&raw)).unwrap();
        assert_eq!(record.setup_failures, 10);
        assert!(record.setup_retry_at > now().unwrap());
        assert_eq!(
            owned
                .two
                .setup("Another-Local-Password-2026", true)
                .err()
                .unwrap()
                .to_string(),
            "local_retry_later"
        );
    }
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn owned_optional_enrollment_recovery_and_persistent_lock() {
        let directory = tempfile::tempdir().unwrap();
        let vault = Arc::new(
            Vault::open_owned(&directory.path().join("private"), "local-lock-owned-test").unwrap(),
        );
        let lock = LocalLock::open(vault.clone()).unwrap();
        assert!(!lock.status().unwrap().enabled);
        let enrollment = lock.setup("Owned-Test-Local-Password-2026", true).unwrap();
        assert!(!lock.status().unwrap().enabled);
        assert!(
            lock.confirm_setup(&enrollment.token, Some("invalid"))
                .is_err()
        );
        let state = lock.state.lock().unwrap();
        let secret = state.pending.as_ref().unwrap().record.totp.unwrap();
        drop(state);
        let code = otp(&secret, now().unwrap() / 30).unwrap();
        let recovery = lock.confirm_setup(&enrollment.token, Some(&code)).unwrap();
        assert_eq!(recovery.len(), 10);
        assert!(lock.status().unwrap().two_factor);
        lock.forget_unlock().unwrap();
        lock.verify("Owned-Test-Local-Password-2026", Some(&recovery[0]))
            .unwrap();
        let reopened = LocalLock::open(vault).unwrap();
        assert!(reopened.status().unwrap().enabled);
        assert!(
            reopened
                .verify("Owned-Test-Local-Password-2026", Some(&recovery[0]))
                .is_err()
        );
    }
}
