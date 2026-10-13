//! Current-user OS vault, authenticated records and atomic native publication.
use crate::model::{HistoryRecord, ProfileSet};
use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{Aead, Payload},
};
use anyhow::{Result, anyhow, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

const MAGIC: &[u8; 8] = b"BZV1\r\n\x1a\n";
const LIMIT: usize = 5 * 1024 * 1024;
const HISTORY_LIMIT: usize = 32 * 1024 * 1024;
const PROFILES: &str = "native-profiles.bzvault";
const HISTORY: &str = "native-history.bzvault";
#[cfg(windows)]
const KEY: &str = "native-master.dpapi";
const NAMED: &[(&str, &str)] = &[
    ("update-state", "native-update-state.bzvault"),
    ("local-lock", "native-local-lock.bzvault"),
    ("saved-queries", "native-saved-queries.bzvault"),
    ("connection-groups", "native-connection-groups.bzvault"),
];
/// Records written by releases that still used online activation. Nothing
/// reads them any more, so they are removed when the vault is opened and they
/// never count as evidence of an initialised vault.
const RETIRED: &[&str] = &[
    "native-license-identity.bzvault",
    "native-license-lease.bzvault",
];
/// The updater's rollback floor. It is the one live record that carries
/// nothing the user created, so a vault holding only this record is an install
/// without user data.
#[cfg(target_os = "macos")]
const REBUILDABLE: &str = "update-state";
/// Whether opening may wait for the user. Only macOS ever asks: once, for the
/// key an older build stored in the keychain for its own binary alone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Approval {
    Ask,
    Never,
}
pub struct Vault {
    root: PathBuf,
    namespace: String,
    key: Zeroizing<[u8; 32]>,
    state: Mutex<Option<[u8; 32]>>,
}

fn safe<T>(value: std::io::Result<T>, code: &'static str) -> Result<T> {
    value.map_err(|_| anyhow!(code))
}
fn digest(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}
fn context_limit(context: &str) -> usize {
    if matches!(context, "history" | "record:saved-queries") {
        HISTORY_LIMIT
    } else {
        LIMIT
    }
}
fn read_bounded(path: &Path) -> Result<Vec<u8>> {
    read_limit(path, LIMIT)
}
fn read_limit(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let metadata = safe(fs::symlink_metadata(path), "vault_read_failed")?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.len() as usize <= limit + 64,
        "vault_file_invalid"
    );
    let mut file = platform::open_read(path)?;
    let mut data = Vec::new();
    safe(
        Read::by_ref(&mut file)
            .take((limit + 65) as u64)
            .read_to_end(&mut data),
        "vault_read_failed",
    )?;
    ensure!(data.len() <= limit + 64, "vault_file_limit");
    Ok(data)
}
impl Vault {
    /// Opens the application's own vault. On macOS this can wait, once, for
    /// the user to approve reading the key a build before the
    /// `native-master-v2` keychain item stored; see `keychain_key`.
    pub fn open_default(application: &str) -> Result<Self> {
        ensure!(application == "BazaStudio", "vault_application_invalid");
        let base = dirs::data_local_dir().ok_or_else(|| anyhow!("vault_data_directory_missing"))?;
        // A frozen identifier of the vault format, not an address: it is part of
        // the authenticated data of every record and names the key in the
        // system key store, so changing it would make every existing vault
        // unreadable.
        Self::open_at(
            &base.join(application),
            &format!("pl.h81.{application}.native.v1"),
            Approval::Ask,
        )
    }
    /// Owned QA fixtures never raise a system prompt. On macOS a fixture whose
    /// key an older build stored for itself, and which holds user data, is
    /// refused with `vault_keychain_approval_required` instead.
    pub fn open_owned(path: &Path, namespace: &str) -> Result<Self> {
        Self::open_at(path, namespace, Approval::Never)
    }
    fn scope(root: &Path, namespace: &str) -> String {
        format!(
            "{}.{}",
            namespace,
            &hex(&digest(root.to_string_lossy().as_bytes()))[..24]
        )
    }
    fn open_at(path: &Path, namespace: &str, approval: Approval) -> Result<Self> {
        ensure!(
            !namespace.is_empty()
                && namespace.len() <= 128
                && namespace
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b)),
            "vault_namespace_invalid"
        );
        let root = safe(std::path::absolute(path), "vault_path_invalid")?;
        let _initial_boundary = platform::private_directory(&root)?;
        let scope = Self::scope(&root, namespace);
        let _lock = platform::lock(&scope)?;
        let _boundary = platform::private_directory(&root)?;
        #[cfg(not(target_os = "macos"))]
        let key = {
            let _ = approval;
            let existing = root.join(PROFILES).exists()
                || root.join(HISTORY).exists()
                || NAMED.iter().any(|(_, file)| root.join(file).exists());
            Zeroizing::new(platform::master_key(&root, &scope, existing)?)
        };
        #[cfg(target_os = "macos")]
        let key = Self::keychain_key(&root, &scope, approval)?;
        Self::remove_retired(&root);
        let state = if root.join(PROFILES).exists() {
            Some(digest(&read_bounded(&root.join(PROFILES))?))
        } else {
            None
        };
        Ok(Self {
            root,
            namespace: scope,
            key,
            state: Mutex::new(state),
        })
    }
    /// Sealed records only the user can recreate: everything except the
    /// updater's own record and the retired ones.
    #[cfg(target_os = "macos")]
    fn user_records(root: &Path) -> Vec<(String, PathBuf)> {
        let mut records = vec![
            ("profiles".to_owned(), root.join(PROFILES)),
            ("history".to_owned(), root.join(HISTORY)),
        ];
        records.extend(
            NAMED
                .iter()
                .filter(|(name, _)| *name != REBUILDABLE)
                .map(|(name, file)| (format!("record:{name}"), root.join(file))),
        );
        records.retain(|(_, path)| path.exists());
        records
    }
    /// Runs under the vault lock and the private directory boundary.
    ///
    /// The key lives in the item every build can read through the system
    /// keychain tool (see the macOS `platform` module). Installs written by
    /// older builds only have an item bound to the binary that created it:
    ///
    /// * no user data on disk: that item is never read. The records sealed
    ///   with its key are dropped and the vault starts with a new key, exactly
    ///   like a first run, without any prompt;
    /// * user data on disk: the old key is needed once. macOS asks the user,
    ///   the key is accepted only when it opens that data, and it is then
    ///   stored in the current format so no later build asks again.
    ///
    /// Nothing on disk changes unless the key was obtained. The codes a user
    /// can cause: `vault_keychain_access_denied` (the prompt was refused),
    /// `vault_keychain_approval_timeout` (no answer within two minutes; the
    /// prompt stays on screen), `vault_keychain_approval_pending` (opened again
    /// while that prompt is still waiting) and, for owned fixtures only,
    /// `vault_keychain_approval_required`.
    #[cfg(target_os = "macos")]
    fn keychain_key(root: &Path, scope: &str, approval: Approval) -> Result<Zeroizing<[u8; 32]>> {
        if let Some(key) = platform::stored_key(scope)? {
            return Ok(key);
        }
        let user_records = Self::user_records(root);
        let rebuildable = NAMED
            .iter()
            .find(|(name, _)| *name == REBUILDABLE)
            .map(|(_, file)| root.join(file))
            .ok_or_else(|| anyhow!("vault_record_name_invalid"))?;
        if !platform::legacy_key_present(scope)? {
            ensure!(
                user_records.is_empty() && !rebuildable.exists(),
                "vault_original_key_missing"
            );
            return platform::create_key(scope);
        }
        if user_records.is_empty() {
            // Removed before the new key exists: an interrupted run repeats
            // this branch instead of leaving a record no key can open.
            if fs::symlink_metadata(&rebuildable).is_ok_and(|metadata| metadata.is_file()) {
                safe(
                    fs::remove_file(&rebuildable),
                    "vault_stale_record_cleanup_failed",
                )?;
            }
            return platform::create_key(scope);
        }
        ensure!(
            approval == Approval::Ask,
            "vault_keychain_approval_required"
        );
        let key = platform::legacy_key(scope)?;
        // An orphaned old item must never be adopted for data it cannot open.
        let mut opens = false;
        for (context, path) in &user_records {
            let sealed = read_limit(path, context_limit(context))?;
            if unseal_with(&key, scope, context, &sealed).is_ok() {
                opens = true;
                break;
            }
        }
        ensure!(opens, "vault_original_key_missing");
        // The key is correct and already approved, so a keychain that cannot
        // take the new item costs another question next time, not the data.
        let _ = platform::adopt_key(scope, &key);
        Ok(key)
    }
    /// Runs under the vault lock, after `private_directory` has refused links,
    /// foreign owners and shared files anywhere below the vault root. A file
    /// that cannot be removed now is simply retried on the next open.
    fn remove_retired(root: &Path) {
        for name in RETIRED {
            let path = root.join(name);
            if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                let _ = fs::remove_file(&path);
            }
        }
    }
    fn aad(&self, context: &str) -> Vec<u8> {
        aad(&self.namespace, context)
    }
    fn named_path(&self, name: &str) -> Result<PathBuf> {
        let file = NAMED
            .iter()
            .find(|(allowed, _)| *allowed == name)
            .ok_or_else(|| anyhow!("vault_record_name_invalid"))?
            .1;
        Ok(self.root.join(file))
    }
    fn named_locked(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let path = self.named_path(name)?;
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(self.unseal(
            &format!("record:{name}"),
            &read_limit(&path, context_limit(&format!("record:{name}")))?,
        )?))
    }
    /// Names are fixed application records, never user-controlled paths. The
    /// callback runs under both instance and OS locks; it must not call Vault.
    pub fn update_named<T>(
        &self,
        name: &str,
        update: impl FnOnce(Option<&[u8]>) -> Result<(Vec<u8>, T)>,
    ) -> Result<T> {
        let path = self.named_path(name)?;
        let _guard = self.state.lock().map_err(|_| anyhow!("vault_busy"))?;
        let _lock = platform::lock(&self.namespace)?;
        let _boundary = platform::private_directory(&self.root)?;
        let current = self.named_locked(name)?;
        let (next, result) = update(current.as_deref().map(Vec::as_slice))?;
        let next = Zeroizing::new(next);
        ensure!(
            next.len() <= context_limit(&format!("record:{name}")),
            "vault_file_limit"
        );
        platform::atomic_write(&path, &self.seal(&format!("record:{name}"), &next)?, false)?;
        Ok(result)
    }
    pub fn load_named(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        self.named_path(name)?;
        let _guard = self.state.lock().map_err(|_| anyhow!("vault_busy"))?;
        let _lock = platform::lock(&self.namespace)?;
        let _boundary = platform::private_directory(&self.root)?;
        self.named_locked(name)
    }
    pub fn save_named(&self, name: &str, plain: &[u8]) -> Result<()> {
        ensure!(
            plain.len() <= context_limit(&format!("record:{name}")),
            "vault_file_limit"
        );
        self.update_named(name, |_| Ok((plain.to_vec(), ())))
    }
    pub fn load_record<T: serde::de::DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        self.load_named(name)?
            .map(|plain| {
                serde_json::from_slice(&plain).map_err(|_| anyhow!("vault_named_record_invalid"))
            })
            .transpose()
    }
    pub fn save_record<T: serde::Serialize>(&self, name: &str, value: &T) -> Result<()> {
        let plain = Zeroizing::new(
            serde_json::to_vec(value).map_err(|_| anyhow!("vault_named_record_invalid"))?,
        );
        self.save_named(name, &plain)
    }
    fn seal(&self, context: &str, plain: &[u8]) -> Result<Vec<u8>> {
        ensure!(plain.len() <= context_limit(context), "vault_file_limit");
        let nonce: [u8; 12] = rand::random();
        let cipher = Aes256Gcm::new_from_slice(self.key.as_ref())
            .map_err(|_| anyhow!("vault_cipher_invalid"))?;
        let encrypted = cipher
            .encrypt(
                &nonce.into(),
                Payload {
                    msg: plain,
                    aad: &self.aad(context),
                },
            )
            .map_err(|_| anyhow!("vault_encrypt_failed"))?;
        let mut output = MAGIC.to_vec();
        output.extend_from_slice(&nonce);
        output.extend_from_slice(&encrypted);
        Ok(output)
    }
    fn unseal(&self, context: &str, data: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        unseal_with(&self.key, &self.namespace, context, data)
    }
    fn profiles_locked(&self) -> Result<ProfileSet> {
        let path = self.root.join(PROFILES);
        if !path.exists() {
            return Ok(ProfileSet::default());
        }
        let plain = self.unseal("profiles", &read_bounded(&path)?)?;
        let set: ProfileSet =
            serde_json::from_slice(&plain).map_err(|_| anyhow!("vault_profiles_invalid"))?;
        crate::profiles::validate_set(&set)?;
        Ok(set)
    }
    pub fn load_profiles(&self) -> Result<ProfileSet> {
        let mut state = self.state.lock().map_err(|_| anyhow!("vault_busy"))?;
        let _lock = platform::lock(&self.namespace)?;
        let _boundary = platform::private_directory(&self.root)?;
        let set = self.profiles_locked()?;
        *state = if self.root.join(PROFILES).exists() {
            Some(digest(&read_bounded(&self.root.join(PROFILES))?))
        } else {
            None
        };
        Ok(set)
    }
    pub fn save_profiles(&self, set: &ProfileSet) -> Result<()> {
        crate::profiles::validate_set(set)?;
        let mut state = self.state.lock().map_err(|_| anyhow!("vault_busy"))?;
        let _lock = platform::lock(&self.namespace)?;
        let _boundary = platform::private_directory(&self.root)?;
        let path = self.root.join(PROFILES);
        let current = if path.exists() {
            Some(digest(&read_bounded(&path)?))
        } else {
            None
        };
        ensure!(*state == current, "vault_conflict_reload");
        let plain =
            Zeroizing::new(serde_json::to_vec(set).map_err(|_| anyhow!("vault_profiles_invalid"))?);
        let sealed = self.seal("profiles", &plain)?;
        platform::atomic_write(&path, &sealed, false)?;
        *state = Some(digest(&sealed));
        Ok(())
    }
    fn history_locked(&self) -> Result<Vec<HistoryRecord>> {
        let path = self.root.join(HISTORY);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let plain = self.unseal("history", &read_limit(&path, HISTORY_LIMIT)?)?;
        let history: Vec<HistoryRecord> =
            serde_json::from_slice(&plain).map_err(|_| anyhow!("vault_history_invalid"))?;
        ensure!(history.len() <= 500, "vault_history_limit");
        Ok(history)
    }
    pub fn load_history(&self) -> Result<Vec<HistoryRecord>> {
        let _guard = self.state.lock().map_err(|_| anyhow!("vault_busy"))?;
        let _lock = platform::lock(&self.namespace)?;
        let _boundary = platform::private_directory(&self.root)?;
        self.history_locked()
    }
    pub fn append_history(&self, record: &HistoryRecord) -> Result<()> {
        ensure!(
            record.sql.len() <= crate::sql::MAX_SQL_BYTES
                && record.status.len() <= 64
                && record.error.as_ref().is_none_or(|e| e.len() <= 8192),
            "vault_history_record_limit"
        );
        let _guard = self.state.lock().map_err(|_| anyhow!("vault_busy"))?;
        let _lock = platform::lock(&self.namespace)?;
        let _boundary = platform::private_directory(&self.root)?;
        let mut profiles = self.profiles_locked()?;
        let copy_sql = profiles.preferences.local_sql_history;
        let mut item = record.clone();
        let mut secrets = Vec::new();
        for profile in &profiles.connections {
            secrets.push(profile.password.as_str());
            if let Some(ssh) = &profile.ssh {
                secrets.extend(
                    [
                        ssh.password.as_deref(),
                        ssh.private_key.as_deref(),
                        ssh.passphrase.as_deref(),
                    ]
                    .into_iter()
                    .flatten(),
                );
            }
        }
        for profile in &profiles.tunnels {
            let ssh = &profile.ssh;
            secrets.extend(
                [
                    ssh.password.as_deref(),
                    ssh.private_key.as_deref(),
                    ssh.passphrase.as_deref(),
                ]
                .into_iter()
                .flatten(),
            );
        }
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        for secret in secrets {
            if !secret.is_empty() {
                item.sql = item.sql.replace(secret, "[REDACTED]");
                if let Some(error) = &mut item.error {
                    *error = error.replace(secret, "[REDACTED]");
                }
            }
        }
        let upper = item.sql.to_ascii_uppercase();
        if (upper.contains("PASSWORD") || upper.contains("IDENTIFIED BY"))
            && [
                "CREATE USER",
                "ALTER USER",
                "CREATE ROLE",
                "ALTER ROLE",
                "SET PASSWORD",
            ]
            .iter()
            .any(|prefix| upper.contains(prefix))
        {
            item.sql = "[Credential-management SQL omitted from local history]".into();
        }
        crate::profiles::wipe_secrets(&mut profiles);
        let copy = copy_sql.then(|| (item.id, Zeroizing::new(item.sql.clone())));
        let mut history = self.history_locked()?;
        history.retain(|value| value.id != item.id);
        history.push(item);
        if history.len() > 500 {
            history.drain(..history.len() - 500);
        }
        let mut plain = Zeroizing::new(
            serde_json::to_vec(&history).map_err(|_| anyhow!("vault_history_invalid"))?,
        );
        while plain.len() > HISTORY_LIMIT && history.len() > 1 {
            history.remove(0);
            plain = Zeroizing::new(
                serde_json::to_vec(&history).map_err(|_| anyhow!("vault_history_invalid"))?,
            );
        }
        platform::atomic_write(
            &self.root.join(HISTORY),
            &self.seal("history", &plain)?,
            false,
        )?;
        if let Some((id, sql)) = copy {
            let directory = self.root.join("native-sql-history");
            let _directory = platform::private_directory(&directory)?;
            platform::atomic_write(&directory.join(format!("{id}.sql")), sql.as_bytes(), false)?;
            let retained = history
                .iter()
                .map(|item| format!("{}.sql", item.id))
                .collect::<std::collections::HashSet<_>>();
            for entry in safe(fs::read_dir(&directory), "vault_read_failed")? {
                let entry = safe(entry, "vault_read_failed")?;
                let filename = entry.file_name();
                let name = filename.to_string_lossy();
                if let Some(stem) = name.strip_suffix(".sql")
                    && uuid::Uuid::parse_str(stem).is_ok()
                    && !retained.contains(name.as_ref())
                {
                    safe(
                        fs::remove_file(entry.path()),
                        "vault_history_cleanup_failed",
                    )?;
                }
            }
        }
        Ok(())
    }
    /// Remove every record of the query history and return how many there
    /// were. The plain SQL copies that the opt-in preference keeps next to the
    /// history are records of the same runs and are removed with it.
    ///
    /// The same rules as for the other record writes apply: the current
    /// history must open with this vault's key, the empty history replaces it
    /// in one atomic publication, and only files named `<uuid>.sql` are
    /// removed from the copy folder. A query that is still running adds its
    /// record again when it ends.
    pub fn clear_history(&self) -> Result<usize> {
        let _guard = self.state.lock().map_err(|_| anyhow!("vault_busy"))?;
        let _lock = platform::lock(&self.namespace)?;
        let _boundary = platform::private_directory(&self.root)?;
        let removed = self.history_locked()?.len();
        let plain = serde_json::to_vec(&Vec::<HistoryRecord>::new())
            .map_err(|_| anyhow!("vault_history_invalid"))?;
        platform::atomic_write(
            &self.root.join(HISTORY),
            &self.seal("history", &plain)?,
            false,
        )?;
        let directory = self.root.join("native-sql-history");
        if directory.exists() {
            let _directory = platform::private_directory(&directory)?;
            for entry in safe(fs::read_dir(&directory), "vault_read_failed")? {
                let entry = safe(entry, "vault_read_failed")?;
                let filename = entry.file_name();
                if filename
                    .to_string_lossy()
                    .strip_suffix(".sql")
                    .is_some_and(|stem| uuid::Uuid::parse_str(stem).is_ok())
                {
                    safe(
                        fs::remove_file(entry.path()),
                        "vault_history_cleanup_failed",
                    )?;
                }
            }
        }
        Ok(removed)
    }
    pub fn remove_owned_qa_key(&self) -> Result<()> {
        ensure!(
            self.namespace.starts_with("BazaStudio.QA."),
            "vault_qa_namespace_required"
        );
        platform::remove_qa_key(&self.namespace)
    }
}
fn hex(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn aad(scope: &str, context: &str) -> Vec<u8> {
    format!("baza-native-v1:{scope}:{context}").into_bytes()
}
fn unseal_with(
    key: &[u8; 32],
    scope: &str,
    context: &str,
    data: &[u8],
) -> Result<Zeroizing<Vec<u8>>> {
    ensure!(
        data.len() >= 36 && data.len() <= context_limit(context) + 64 && data.starts_with(MAGIC),
        "vault_record_invalid"
    );
    let nonce: [u8; 12] = data[8..20]
        .try_into()
        .map_err(|_| anyhow!("vault_record_invalid"))?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| anyhow!("vault_cipher_invalid"))?;
    Ok(Zeroizing::new(
        cipher
            .decrypt(
                &nonce.into(),
                Payload {
                    msg: &data[20..],
                    aad: &aad(scope, context),
                },
            )
            .map_err(|_| anyhow!("vault_authentication_failed"))?,
    ))
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::{
        ffi::c_void,
        mem::{size_of, zeroed},
        os::windows::{
            ffi::OsStrExt,
            io::{FromRawHandle, RawHandle},
        },
        ptr::{null, null_mut},
    };
    use windows_sys::Win32::{
        Foundation::*,
        Security::{Authorization::*, Cryptography::*, *},
        Storage::FileSystem::*,
        System::Threading::*,
    };
    use zeroize::Zeroize;
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    struct Local(*mut c_void);
    impl Drop for Local {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    LocalFree(self.0);
                }
            }
        }
    }
    fn wide(path: &std::ffi::OsStr) -> Result<Vec<u16>> {
        let mut value: Vec<u16> = path.encode_wide().collect();
        ensure!(!value.contains(&0), "vault_path_invalid");
        value.push(0);
        Ok(value)
    }
    fn last() -> anyhow::Error {
        let code = unsafe { GetLastError() };
        anyhow!("vault_windows_error_{code}")
    }
    fn sid() -> Result<String> {
        unsafe {
            let mut token = null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(last());
            }
            let _token = Handle(token);
            let mut length = 0;
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut length);
            ensure!(length > 0 && length < 65536, "vault_token_invalid");
            let mut data = vec![0u8; length as usize];
            if GetTokenInformation(
                token,
                TokenUser,
                data.as_mut_ptr().cast(),
                length,
                &mut length,
            ) == 0
            {
                return Err(last());
            }
            let user = &*(data.as_ptr().cast::<TOKEN_USER>());
            let mut text = null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
                return Err(last());
            }
            let _text = Local(text.cast());
            let mut count = 0;
            while *text.add(count) != 0 && count < 1024 {
                count += 1;
            }
            ensure!(count < 1024, "vault_sid_invalid");
            Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
                text, count,
            )))
        }
    }
    struct Security {
        descriptor: Local,
        sid: PSID,
    }
    impl Security {
        fn new() -> Result<Self> {
            unsafe {
                let owner = sid()?;
                let text = wide(std::ffi::OsStr::new(&format!(
                    "O:{owner}D:P(A;OICI;FA;;;{owner})"
                )))?;
                let mut descriptor = null_mut();
                if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    text.as_ptr(),
                    1,
                    &mut descriptor,
                    null_mut(),
                ) == 0
                {
                    return Err(last());
                }
                let descriptor = Local(descriptor);
                let mut present = 0;
                let mut defaulted = 0;
                let mut dacl = null_mut();
                if GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut dacl, &mut defaulted)
                    == 0
                    || present == 0
                    || dacl.is_null()
                {
                    bail!("vault_private_acl_invalid");
                }
                let mut ace = null_mut();
                if GetAce(dacl, 0, &mut ace) == 0 {
                    return Err(last());
                }
                let user = (ace as *mut u8).add(8).cast();
                Ok(Self {
                    descriptor,
                    sid: user,
                })
            }
        }
        fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.descriptor.0,
                bInheritHandle: 0,
            }
        }
    }
    fn open(path: &Path, access: u32) -> Result<Handle> {
        let name = wide(path.as_os_str())?;
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };
        ensure!(handle != INVALID_HANDLE_VALUE, "vault_snapshot_changed");
        Ok(Handle(handle))
    }
    fn verify(handle: HANDLE, path: &Path, directory: bool) -> Result<()> {
        unsafe {
            let mut info: BY_HANDLE_FILE_INFORMATION = zeroed();
            if GetFileInformationByHandle(handle, &mut info) == 0 {
                return Err(last());
            }
            ensure!(
                info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
                    && (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) == directory
                    && (directory || info.nNumberOfLinks == 1),
                "vault_unsafe_link"
            );
            let mut buffer = vec![0u16; 32768];
            let count =
                GetFinalPathNameByHandleW(handle, buffer.as_mut_ptr(), buffer.len() as u32, 0);
            ensure!(
                count > 0 && count < (buffer.len() as u32),
                "vault_path_invalid"
            );
            let actual = String::from_utf16_lossy(&buffer[..count as usize]);
            let actual = actual.strip_prefix("\\\\?\\").unwrap_or(&actual);
            let expected = path.to_string_lossy();
            if !actual.eq_ignore_ascii_case(&expected) {
                let mut expanded = vec![0u16; 32768];
                let input = wide(path.as_os_str())?;
                let count = GetLongPathNameW(input.as_ptr(), expanded.as_mut_ptr(), 32768);
                ensure!(
                    count > 0
                        && count < 32768
                        && actual.eq_ignore_ascii_case(&String::from_utf16_lossy(
                            &expanded[..count as usize]
                        )),
                    "vault_path_mismatch"
                );
            }
            Ok(())
        }
    }
    fn validate_acl(handle: HANDLE, directory: bool, expected: PSID) -> Result<()> {
        unsafe {
            let mut owner = null_mut();
            let mut acl = null_mut();
            let mut sd = null_mut();
            let code = GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut sd,
            );
            ensure!(code == 0, "vault_acl_read_failed");
            let _sd = Local(sd);
            ensure!(
                !owner.is_null() && EqualSid(owner, expected) != 0,
                "vault_owner_mismatch"
            );
            let mut info: ACL_SIZE_INFORMATION = zeroed();
            let mut ace = null_mut();
            ensure!(
                !acl.is_null()
                    && GetAclInformation(
                        acl,
                        (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                        size_of::<ACL_SIZE_INFORMATION>() as u32,
                        AclSizeInformation
                    ) != 0
                    && info.AceCount == 1
                    && GetAce(acl, 0, &mut ace) != 0,
                if directory {
                    "vault_nonprivate_directory"
                } else {
                    "vault_nonprivate_file"
                }
            );
            let value = &*(ace.cast::<ACCESS_ALLOWED_ACE>());
            ensure!(
                value.Header.AceType == 0
                    && value.Header.AceFlags & INHERIT_ONLY_ACE as u8 == 0
                    && value.Mask & 0x1f01ff == 0x1f01ff
                    && EqualSid((&value.SidStart as *const u32).cast_mut().cast(), expected) != 0,
                if directory {
                    "vault_nonprivate_directory"
                } else {
                    "vault_nonprivate_file"
                }
            );
            Ok(())
        }
    }
    pub struct Protection {
        _handles: Vec<Handle>,
    }
    pub fn open_read(path: &Path) -> Result<File> {
        let handle = open(path, 0x00020081)?;
        verify(handle.0, path, false)?;
        let security = Security::new()?;
        validate_acl(handle.0, false, security.sid)?;
        let file = unsafe { File::from_raw_handle(handle.0 as RawHandle) };
        std::mem::forget(handle);
        Ok(file)
    }
    pub fn private_directory(root: &Path) -> Result<Protection> {
        ensure!(
            root.is_absolute()
                && !root
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
            "vault_path_invalid"
        );
        let security = Security::new()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let result = (|| -> Result<Protection> {
                let mut held: Vec<(PathBuf, Handle, bool)> = Vec::new();
                let mut ancestors: Vec<_> = root.ancestors().collect();
                ancestors.reverse();
                for folder in ancestors {
                    if !folder.exists() {
                        let name = wide(folder.as_os_str())?;
                        let attributes = security.attributes();
                        let success = unsafe { CreateDirectoryW(name.as_ptr(), &attributes) };
                        if success == 0 && unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
                            return Err(last());
                        }
                    }
                    let handle = open(folder, if folder == root { 0x00020081 } else { 0x81 })?;
                    verify(handle.0, folder, true)?;
                    if folder == root {
                        validate_acl(handle.0, true, security.sid)?;
                    }
                    held.push((folder.to_owned(), handle, true));
                }
                let mut pending = vec![root.to_owned()];
                let mut snapshots = Vec::new();
                let boundary = held.len() - 1;
                while let Some(folder) = pending.pop() {
                    let entries = safe(fs::read_dir(&folder), "vault_snapshot_changed")?;
                    let mut names = Vec::new();
                    for entry in entries {
                        let entry = safe(entry, "vault_snapshot_changed")?;
                        let path = entry.path();
                        let directory = safe(entry.file_type(), "vault_snapshot_changed")?.is_dir();
                        names.push(entry.file_name());
                        ensure!(held.len() + names.len() <= 50000, "vault_tree_limit");
                        let handle = open(&path, 0x00020081)?;
                        verify(handle.0, &path, directory)?;
                        validate_acl(handle.0, directory, security.sid)?;
                        held.push((path.clone(), handle, directory));
                        if directory {
                            pending.push(path);
                        }
                    }
                    names.sort();
                    snapshots.push((folder, names));
                }
                for (folder, expected) in snapshots {
                    let mut names = Vec::new();
                    for entry in safe(fs::read_dir(folder), "vault_snapshot_changed")? {
                        names.push(safe(entry, "vault_snapshot_changed")?.file_name());
                        ensure!(names.len() <= 50000, "vault_tree_limit");
                    }
                    names.sort();
                    ensure!(names == expected, "vault_snapshot_changed");
                }
                for (path, handle, directory) in held.iter().skip(boundary) {
                    verify(handle.0, path, *directory)?;
                    validate_acl(handle.0, *directory, security.sid)?;
                }
                // New objects already have an explicit SID owner and private
                // DACL. Existing objects are validated without ANY ACL writes.
                // This prevents propagation or modifying external hardlinks.
                Ok(Protection {
                    _handles: held
                        .into_iter()
                        .filter_map(|(_, handle, directory)| directory.then_some(handle))
                        .collect(),
                })
            })();
            match result {
                Ok(boundary) => return Ok(boundary),
                Err(error)
                    if error.to_string() == "vault_snapshot_changed"
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => return Err(error),
            }
        }
    }
    pub struct Lock(Handle);
    impl Drop for Lock {
        fn drop(&mut self) {
            unsafe {
                ReleaseMutex(self.0.0);
            }
        }
    }
    pub fn lock(scope: &str) -> Result<Lock> {
        let security = Security::new()?;
        let attributes = security.attributes();
        let name = wide(std::ffi::OsStr::new(&format!(
            "Local\\BazaVault-{}",
            hex(&digest(scope.as_bytes()))
        )))?;
        let handle = unsafe { CreateMutexW(&attributes, 0, name.as_ptr()) };
        ensure!(!handle.is_null(), "vault_lock_failed");
        let handle = Handle(handle);
        let status = unsafe { WaitForSingleObject(handle.0, 5000) };
        ensure!(
            status == WAIT_OBJECT_0 || status == WAIT_ABANDONED,
            "vault_busy"
        );
        Ok(Lock(handle))
    }
    fn create_file(path: &Path) -> Result<File> {
        let security = Security::new()?;
        let attributes = security.attributes();
        let name = wide(path.as_os_str())?;
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        ensure!(handle != INVALID_HANDLE_VALUE, "vault_new_file_failed");
        Ok(unsafe { File::from_raw_handle(handle as RawHandle) })
    }
    fn cleanup(path: &Path) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match fs::remove_file(path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e)
                    if matches!(e.raw_os_error(), Some(5 | 32 | 33))
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(_) => bail!("vault_temporary_cleanup_failed"),
            }
        }
    }
    pub fn atomic_write(path: &Path, data: &[u8], first: bool) -> Result<()> {
        let parent = path.parent().ok_or_else(|| anyhow!("vault_path_invalid"))?;
        let _boundary = private_directory(parent)?;
        let temporary = parent.join(format!(".baza-native-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = create_file(&temporary)?;
            safe(file.write_all(data), "vault_write_failed")?;
            safe(file.sync_all(), "vault_write_failed")?;
            drop(file);
            let source = wide(temporary.as_os_str())?;
            let target = wide(path.as_os_str())?;
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let success = unsafe {
                    MoveFileExW(
                        source.as_ptr(),
                        target.as_ptr(),
                        MOVEFILE_WRITE_THROUGH | if first { 0 } else { MOVEFILE_REPLACE_EXISTING },
                    )
                };
                if success != 0 {
                    break;
                }
                let code = unsafe { GetLastError() };
                if first && matches!(code, 80 | 183) {
                    break;
                }
                if matches!(code, 5 | 32 | 33) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(25));
                } else {
                    bail!("vault_atomic_publish_failed");
                }
            }
            Ok(())
        })();
        let cleanup = cleanup(&temporary);
        result?;
        cleanup
    }
    fn crypt(value: &[u8], scope: &str, inverse: bool) -> Result<Zeroizing<Vec<u8>>> {
        ensure!(
            !value.is_empty() && value.len() <= 8192,
            "vault_key_invalid"
        );
        let mut input = value.to_vec();
        let mut entropy = digest(scope.as_bytes());
        let source = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_mut_ptr(),
        };
        let extra = CRYPT_INTEGER_BLOB {
            cbData: 32,
            pbData: entropy.as_mut_ptr(),
        };
        let mut output: CRYPT_INTEGER_BLOB = unsafe { zeroed() };
        let success = unsafe {
            if inverse {
                CryptUnprotectData(
                    &source,
                    null_mut(),
                    &extra,
                    null(),
                    null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            } else {
                CryptProtectData(
                    &source,
                    null(),
                    &extra,
                    null(),
                    null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            }
        };
        input.zeroize();
        ensure!(success != 0, "vault_dpapi_authentication_failed");
        let native = Local(output.pbData.cast());
        let result = Zeroizing::new(
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec(),
        );
        unsafe {
            std::ptr::write_bytes(output.pbData, 0, output.cbData as usize);
        }
        drop(native);
        Ok(result)
    }
    pub fn master_key(root: &Path, scope: &str, existing: bool) -> Result<[u8; 32]> {
        let path = root.join(KEY);
        if !path.exists() {
            ensure!(!existing, "vault_original_key_missing");
            let key: Zeroizing<[u8; 32]> = Zeroizing::new(rand::random());
            let sealed = crypt(key.as_ref(), scope, false)?;
            atomic_write(&path, &sealed, true)?;
        }
        let plain = crypt(&read_bounded(&path)?, scope, true)?;
        plain
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("vault_key_invalid"))
    }
    pub fn remove_qa_key(_scope: &str) -> Result<()> {
        Ok(())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::*;
    pub fn open_read(_path: &Path) -> Result<File> {
        bail!("native_os_vault_unsupported")
    }
    pub fn private_directory(_root: &Path) -> Result<()> {
        bail!("native_os_vault_unsupported")
    }
    pub fn lock(_scope: &str) -> Result<()> {
        bail!("native_os_vault_unsupported")
    }
    pub fn master_key(_root: &Path, _scope: &str, _existing: bool) -> Result<[u8; 32]> {
        bail!("native_os_vault_unsupported")
    }
    pub fn atomic_write(_path: &Path, _data: &[u8], _first: bool) -> Result<()> {
        bail!("native_os_vault_unsupported")
    }
    pub fn remove_qa_key(_scope: &str) -> Result<()> {
        bail!("native_os_vault_unsupported")
    }
}
#[cfg(all(test, any(windows, target_os = "macos")))]
mod tests {
    use super::*;
    use std::sync::Arc;
    fn owned() -> (tempfile::TempDir, PathBuf, String) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("owned-native-vault");
        let ns = format!("BazaStudio.QA.{}", uuid::Uuid::new_v4());
        (temp, root, ns)
    }
    #[test]
    fn encrypted_records_bound_to_name_no_replacement_key_and_tamper_refusal() {
        let (_temp, root, ns) = owned();
        let v = Vault::open_owned(&root, &ns).unwrap();
        v.save_record(
            "local-lock",
            &serde_json::json!({"synthetic_secret":"OWNED-SECRET"}),
        )
        .unwrap();
        let bytes = read_bounded(&root.join("native-local-lock.bzvault")).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("OWNED-SECRET"));
        assert!(v.load_named("../outside").is_err());
        platform::atomic_write(&root.join("native-update-state.bzvault"), &bytes, false).unwrap();
        assert!(v.load_named("update-state").is_err());
        let mut bad = bytes;
        *bad.last_mut().unwrap() ^= 1;
        platform::atomic_write(&root.join("native-local-lock.bzvault"), &bad, false).unwrap();
        assert!(v.load_named("local-lock").is_err());
        let _ = v.remove_owned_qa_key();
        drop(v);
        #[cfg(windows)]
        {
            fs::remove_file(root.join(KEY)).unwrap();
            assert!(Vault::open_owned(&root, &ns).is_err());
            assert!(!root.join(KEY).exists());
        }
    }
    #[test]
    fn all_native_themes_roundtrip_through_the_encrypted_vault() {
        let (_temp, root, namespace) = owned();
        let vault = Vault::open_owned(&root, &namespace).unwrap();
        for theme in [
            "vscode", "graphite", "midnight", "oled", "amethyst", "light", "nord", "amoled",
        ] {
            let mut profiles = vault.load_profiles().unwrap();
            profiles.preferences.theme = theme.into();
            vault.save_profiles(&profiles).unwrap();
            let reopened = Vault::open_owned(&root, &namespace).unwrap();
            assert_eq!(reopened.load_profiles().unwrap().preferences.theme, theme);
        }
        let mut invalid = vault.load_profiles().unwrap();
        invalid.preferences.theme = "unrecognized-theme".into();
        assert!(vault.save_profiles(&invalid).is_err());
        assert_eq!(vault.load_profiles().unwrap().preferences.theme, "amoled");
        let _ = vault.remove_owned_qa_key();
    }
    #[test]
    fn profile_cas_and_history_completion_are_durable() {
        let (_temp, root, ns) = owned();
        let one = Vault::open_owned(&root, &ns).unwrap();
        let two = Vault::open_owned(&root, &ns).unwrap();
        let mut set = one.load_profiles().unwrap();
        set.preferences.local_sql_history = true;
        one.save_profiles(&set).unwrap();
        assert!(two.save_profiles(&ProfileSet::default()).is_err());
        let id = uuid::Uuid::new_v4();
        let mut h = HistoryRecord {
            id,
            connection_id: uuid::Uuid::new_v4(),
            sql: "SELECT 42".into(),
            status: "running".into(),
            started_at: "2026-10-05T00:00:00Z".into(),
            duration_ms: 0,
            rows: 0,
            error: None,
        };
        one.append_history(&h).unwrap();
        h.status = "completed".into();
        h.rows = 1;
        one.append_history(&h).unwrap();
        let history = two.load_history().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, id);
        assert_eq!(history[0].status, "completed");
        let _ = one.remove_owned_qa_key();
    }
    #[test]
    fn parallel_native_initializers_and_atomic_record_updates_share_one_key() {
        let (_temp, root, ns) = owned();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let handles = (0..8)
            .map(|_| {
                let root = root.clone();
                let ns = ns.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let v = Vault::open_owned(&root, &ns).unwrap();
                    v.update_named("update-state", |old| {
                        let count = old
                            .map(|b| serde_json::from_slice::<u64>(b).unwrap())
                            .unwrap_or(0)
                            + 1;
                        Ok((serde_json::to_vec(&count).unwrap(), ()))
                    })
                    .unwrap();
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap();
        }
        let v = Vault::open_owned(&root, &ns).unwrap();
        assert_eq!(v.load_record::<u64>("update-state").unwrap(), Some(8));
        assert!(
            fs::read_dir(&root).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
        let _ = v.remove_owned_qa_key();
    }
    fn tunnel_with_password(password: &str) -> crate::model::TunnelProfile {
        crate::model::TunnelProfile {
            id: uuid::Uuid::new_v4(),
            name: "Owned tunnel fixture".into(),
            ssh: crate::model::SshProfile {
                host: "127.0.0.1".into(),
                port: 22,
                username: "owned-qa".into(),
                auth: crate::model::SshAuth::Password,
                password: Some(password.into()),
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
    fn saved_tunnel_secret_reopens_and_never_reaches_the_profile_file_in_plaintext() {
        let (_temp, root, ns) = owned();
        let vault = Vault::open_owned(&root, &ns).unwrap();
        let mut set = vault.load_profiles().unwrap();
        set.tunnels
            .push(tunnel_with_password("OWNED-TUNNEL-SECRET"));
        vault.save_profiles(&set).unwrap();
        let reopened = Vault::open_owned(&root, &ns).unwrap();
        let restored = reopened.load_profiles().unwrap();
        assert_eq!(restored.tunnels.len(), 1);
        assert_eq!(restored.tunnels[0].id, set.tunnels[0].id);
        assert_eq!(
            restored.tunnels[0].ssh.password.as_deref(),
            Some("OWNED-TUNNEL-SECRET")
        );
        let bytes = read_bounded(&root.join(PROFILES)).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("OWNED-TUNNEL-SECRET"));
        let _ = vault.remove_owned_qa_key();
    }
    #[test]
    fn history_redacts_saved_profile_secrets_in_sql_and_errors() {
        let (_temp, root, ns) = owned();
        let vault = Vault::open_owned(&root, &ns).unwrap();
        let mut set = vault.load_profiles().unwrap();
        set.tunnels
            .push(tunnel_with_password("OWNED-TUNNEL-SECRET"));
        vault.save_profiles(&set).unwrap();
        let mut record = HistoryRecord {
            id: uuid::Uuid::new_v4(),
            connection_id: set.tunnels[0].id,
            sql: "SELECT 'OWNED-TUNNEL-SECRET';".into(),
            status: "running".into(),
            started_at: "2026-10-05T00:00:00Z".into(),
            duration_ms: 0,
            rows: 0,
            error: None,
        };
        vault.append_history(&record).unwrap();
        record.status = "failed".into();
        record.error = Some("syntax error near OWNED-TUNNEL-SECRET".into());
        vault.append_history(&record).unwrap();
        let history = vault.load_history().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].status, "failed");
        assert_eq!(history[0].sql, "SELECT '[REDACTED]';");
        assert_eq!(
            history[0].error.as_deref(),
            Some("syntax error near [REDACTED]")
        );
        assert!(!root.join("native-sql-history").exists());
        let _ = vault.remove_owned_qa_key();
    }
    fn plant_retired(vault: &Vault, root: &Path) {
        // Sealed under the record name the removed activation client used.
        let sealed = vault
            .seal("record:license-identity", b"owned-retired-identity")
            .unwrap();
        for name in RETIRED {
            platform::atomic_write(&root.join(name), &sealed, false).unwrap();
        }
    }
    fn retired_present(root: &Path) -> bool {
        RETIRED.iter().any(|name| root.join(name).exists())
    }
    #[test]
    fn install_left_by_an_activation_release_opens_and_loses_only_retired_records() {
        // The shape of a real existing install: retired identity record, the
        // update record and the OS-held master key, but no profiles.
        let (_temp, root, ns) = owned();
        let first = Vault::open_owned(&root, &ns).unwrap();
        first
            .save_named("update-state", b"owned-update-state")
            .unwrap();
        plant_retired(&first, &root);
        drop(first);
        assert!(retired_present(&root));
        let reopened = Vault::open_owned(&root, &ns).unwrap();
        assert!(!retired_present(&root));
        assert_eq!(
            reopened
                .load_named("update-state")
                .unwrap()
                .unwrap()
                .as_slice(),
            b"owned-update-state"
        );
        assert_eq!(
            reopened
                .load_named("license-identity")
                .unwrap_err()
                .to_string(),
            "vault_record_name_invalid"
        );
        let mut profiles = reopened.load_profiles().unwrap();
        assert!(profiles.connections.is_empty() && profiles.tunnels.is_empty());
        profiles.preferences.language = "en".into();
        reopened.save_profiles(&profiles).unwrap();
        drop(reopened);
        let again = Vault::open_owned(&root, &ns).unwrap();
        assert_eq!(again.load_profiles().unwrap().preferences.language, "en");
        let _ = again.remove_owned_qa_key();
    }
    #[test]
    fn retired_records_alone_never_make_a_vault_look_initialised() {
        let (_temp, root, ns) = owned();
        let first = Vault::open_owned(&root, &ns).unwrap();
        plant_retired(&first, &root);
        drop(first);
        // Same master key, nothing but retired records: opens and keeps the key.
        let second = Vault::open_owned(&root, &ns).unwrap();
        assert!(!retired_present(&root));
        second.save_named("update-state", b"kept").unwrap();
        plant_retired(&second, &root);
        let sealed_update = read_bounded(&root.join("native-update-state.bzvault")).unwrap();
        // Lose the master key while only retired records remain on disk.
        fs::remove_file(root.join("native-update-state.bzvault")).unwrap();
        second.remove_owned_qa_key().unwrap();
        drop(second);
        #[cfg(windows)]
        fs::remove_file(root.join(KEY)).unwrap();
        // Before the records were retired this was `vault_original_key_missing`.
        let third = Vault::open_owned(&root, &ns).unwrap();
        assert!(!retired_present(&root));
        assert!(third.load_named("update-state").unwrap().is_none());
        // A live record without its key is still refused, never re-keyed.
        platform::atomic_write(
            &root.join("native-update-state.bzvault"),
            &sealed_update,
            false,
        )
        .unwrap();
        assert!(third.load_named("update-state").is_err());
        third.remove_owned_qa_key().unwrap();
        drop(third);
        #[cfg(windows)]
        fs::remove_file(root.join(KEY)).unwrap();
        assert_eq!(
            Vault::open_owned(&root, &ns).err().unwrap().to_string(),
            "vault_original_key_missing"
        );
    }
    #[cfg(windows)]
    #[test]
    fn hardlinked_existing_file_refused_without_changing_inode_or_content() {
        let (temp, root, ns) = owned();
        let v = Vault::open_owned(&root, &ns).unwrap();
        v.save_named("update-state", b"owned-state").unwrap();
        let path = root.join("native-update-state.bzvault");
        let before = fs::read(&path).unwrap();
        let outside = temp.path().join("outside-alias");
        fs::hard_link(&path, &outside).unwrap();
        assert_eq!(
            v.load_named("update-state").unwrap_err().to_string(),
            "vault_unsafe_link"
        );
        assert_eq!(fs::read(&outside).unwrap(), before);
    }
}
#[cfg(all(test, any(windows, target_os = "macos")))]
mod history_policy_tests {
    use super::*;
    #[test]
    fn encrypted_history_always_enabled_plain_sql_only_after_explicit_optin() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("native-private");
        let vault =
            Vault::open_owned(&root, &format!("BazaStudio.QA.{}", uuid::Uuid::new_v4())).unwrap();
        let mut profiles = vault.load_profiles().unwrap();
        assert!(!profiles.preferences.local_sql_history);
        let mut h = HistoryRecord {
            id: uuid::Uuid::new_v4(),
            connection_id: uuid::Uuid::new_v4(),
            sql: "SELECT 'OWNED-SQL-MARKER';".into(),
            status: "running".into(),
            started_at: "2026-10-05T00:00:00Z".into(),
            duration_ms: 0,
            rows: 0,
            error: None,
        };
        vault.append_history(&h).unwrap();
        assert_eq!(vault.load_history().unwrap().len(), 1);
        assert!(!root.join("native-sql-history").exists());
        assert!(
            !String::from_utf8_lossy(&fs::read(root.join(HISTORY)).unwrap())
                .contains("OWNED-SQL-MARKER")
        );
        profiles.preferences.local_sql_history = true;
        vault.save_profiles(&profiles).unwrap();
        h.status = "completed".into();
        vault.append_history(&h).unwrap();
        assert_eq!(
            fs::read_to_string(
                root.join("native-sql-history")
                    .join(format!("{}.sql", h.id))
            )
            .unwrap(),
            h.sql
        );
        assert_eq!(vault.load_history().unwrap().len(), 1);
        let _ = vault.remove_owned_qa_key();
    }
    #[test]
    fn clearing_history_removes_records_and_sql_copies_and_nothing_else() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("native-private");
        let namespace = format!("BazaStudio.QA.{}", uuid::Uuid::new_v4());
        let vault = Vault::open_owned(&root, &namespace).unwrap();
        // Nothing to clear yet: an empty history is still a valid record.
        assert_eq!(vault.clear_history().unwrap(), 0);
        assert!(vault.load_history().unwrap().is_empty());
        let mut profiles = vault.load_profiles().unwrap();
        profiles.connections.push(crate::model::ConnectionProfile {
            name: "Owned clear".into(),
            host: "owned-db.example.test".into(),
            database: "owned_fixture".into(),
            username: "owned-user".into(),
            password: "OWNED-DB-PASSWORD".into(),
            ..Default::default()
        });
        profiles.preferences.local_sql_history = true;
        vault.save_profiles(&profiles).unwrap();
        vault.save_named("saved-queries", b"[]").unwrap();
        let record = |sql: &str| HistoryRecord {
            id: uuid::Uuid::new_v4(),
            connection_id: profiles.connections[0].id,
            sql: sql.into(),
            status: "completed".into(),
            started_at: "2026-10-12T00:00:00Z".into(),
            duration_ms: 3,
            rows: 1,
            error: None,
        };
        let records = [
            record("SELECT 'OWNED-CLEAR-ONE'"),
            record("SELECT 'OWNED-CLEAR-TWO'"),
            record("SELECT 'OWNED-CLEAR-THREE'"),
        ];
        for item in &records {
            vault.append_history(item).unwrap();
        }
        let copies = root.join("native-sql-history");
        assert_eq!(fs::read_dir(&copies).unwrap().count(), 3);
        // A file that is not a copy made by the vault is left alone. It is
        // written like the vault's own files, private to the user, because
        // the vault refuses to work next to anything else.
        platform::atomic_write(&copies.join("notes.txt"), b"kept", false).unwrap();
        let profiles_before = fs::read(root.join(PROFILES)).unwrap();
        let other = Vault::open_owned(&root, &namespace).unwrap();
        assert_eq!(other.load_history().unwrap().len(), 3);

        assert_eq!(vault.clear_history().unwrap(), 3);

        // Every instance sees the empty history, and so does a reopened vault.
        assert!(vault.load_history().unwrap().is_empty());
        assert!(other.load_history().unwrap().is_empty());
        assert!(
            Vault::open_owned(&root, &namespace)
                .unwrap()
                .load_history()
                .unwrap()
                .is_empty()
        );
        // The record on disk is still sealed with the vault key and holds no
        // trace of the statements; no temporary file is left behind.
        let sealed = fs::read(root.join(HISTORY)).unwrap();
        assert!(sealed.starts_with(MAGIC));
        assert!(!String::from_utf8_lossy(&sealed).contains("OWNED-CLEAR"));
        let names = |directory: &Path| {
            let mut names = fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        assert_eq!(names(&copies), ["notes.txt"]);
        assert!(names(&root).iter().all(|name| !name.ends_with(".tmp")));
        // Profiles and the other records are untouched.
        assert_eq!(fs::read(root.join(PROFILES)).unwrap(), profiles_before);
        let reloaded = vault.load_profiles().unwrap();
        assert_eq!(reloaded.connections[0].password, "OWNED-DB-PASSWORD");
        assert_eq!(
            vault
                .load_named("saved-queries")
                .unwrap()
                .unwrap()
                .as_slice(),
            b"[]"
        );
        // History keeps working afterwards, with its copy.
        let next = record("SELECT 'OWNED-AFTER-CLEAR'");
        vault.append_history(&next).unwrap();
        let history = other.load_history().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, next.id);
        assert!(copies.join(format!("{}.sql", next.id)).exists());
        assert_eq!(vault.clear_history().unwrap(), 1);
        assert_eq!(names(&copies), ["notes.txt"]);
        let _ = vault.remove_owned_qa_key();
    }
    #[test]
    fn history_that_does_not_open_is_never_replaced_by_a_clear() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("native-private");
        let namespace = format!("BazaStudio.QA.{}", uuid::Uuid::new_v4());
        let vault = Vault::open_owned(&root, &namespace).unwrap();
        vault
            .append_history(&HistoryRecord {
                id: uuid::Uuid::new_v4(),
                connection_id: uuid::Uuid::new_v4(),
                sql: "SELECT 1".into(),
                status: "completed".into(),
                started_at: "2026-10-12T00:00:00Z".into(),
                duration_ms: 1,
                rows: 1,
                error: None,
            })
            .unwrap();
        // A record sealed for another purpose is not this vault's history.
        vault.save_named("saved-queries", b"[]").unwrap();
        let foreign = fs::read(root.join("native-saved-queries.bzvault")).unwrap();
        platform::atomic_write(&root.join(HISTORY), &foreign, false).unwrap();
        assert!(vault.clear_history().is_err());
        assert_eq!(fs::read(root.join(HISTORY)).unwrap(), foreign);
        let _ = vault.remove_owned_qa_key();
    }
}
#[cfg(all(test, target_os = "macos"))]
mod keychain_tests {
    use super::*;
    use std::process::{Command, Stdio};

    fn files(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut files = fs::read_dir(root)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        files.sort();
        files
    }
    /// A vault in the state a build before the `native-master-v2` item left
    /// it: the only key is the in-process item that trusts its creator.
    fn old_install(root: &Path, namespace: &str) -> (Vault, Zeroizing<[u8; 32]>) {
        let root = std::path::absolute(root).unwrap();
        drop(platform::private_directory(&root).unwrap());
        let scope = Vault::scope(&root, namespace);
        let key = Zeroizing::new(rand::random::<[u8; 32]>());
        platform::seed_legacy_key(&scope, &key).unwrap();
        let vault = Vault {
            root,
            namespace: scope,
            key: key.clone(),
            state: Mutex::new(None),
        };
        (vault, key)
    }
    fn write_user_data(vault: &Vault) {
        let mut profiles = vault.load_profiles().unwrap();
        profiles.preferences.language = "en".into();
        vault.save_profiles(&profiles).unwrap();
        vault.save_named(REBUILDABLE, b"kept").unwrap();
    }
    fn describe(opened: Result<Vault>) -> String {
        match opened {
            Ok(vault) => format!(
                "open language={} update-state={}",
                vault.load_profiles().unwrap().preferences.language,
                vault
                    .load_named(REBUILDABLE)
                    .unwrap()
                    .map_or("none".into(), |value| String::from_utf8_lossy(&value)
                        .into_owned())
            ),
            Err(error) => format!("error {error}"),
        }
    }
    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        namespace: String,
        scope: String,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = std::path::absolute(temp.path().join("owned-native-vault")).unwrap();
            let namespace = format!("BazaStudio.QA.{}", uuid::Uuid::new_v4());
            let scope = Vault::scope(&root, &namespace);
            Self {
                _temp: temp,
                root,
                namespace,
                scope,
            }
        }
        fn open(&self, approval: Approval) -> Result<Vault> {
            Vault::open_at(&self.root, &self.namespace, approval)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = platform::remove_qa_key(&self.scope);
        }
    }

    #[test]
    fn first_key_is_stored_once_in_the_item_every_build_reads() {
        let fixture = Fixture::new();
        let vault = fixture.open(Approval::Never).unwrap();
        assert!(platform::key_present(&fixture.scope, false).unwrap());
        assert!(!platform::key_present(&fixture.scope, true).unwrap());
        assert_eq!(
            *platform::stored_key(&fixture.scope).unwrap().unwrap(),
            *vault.key
        );
        // A second first run finds the item and never replaces its key.
        assert_eq!(*platform::create_key(&fixture.scope).unwrap(), *vault.key);
        assert_eq!(*fixture.open(Approval::Never).unwrap().key, *vault.key);
    }

    #[test]
    fn old_item_without_user_data_is_never_read_and_the_vault_starts_clean() {
        let fixture = Fixture::new();
        let (old, old_key) = old_install(&fixture.root, &fixture.namespace);
        old.save_named(REBUILDABLE, b"sealed with the old key")
            .unwrap();
        let retired = old.seal("record:license-identity", b"retired").unwrap();
        for name in RETIRED {
            platform::atomic_write(&fixture.root.join(name), &retired, false).unwrap();
        }
        drop(old);
        // No approval is available here, so success proves none was needed.
        let vault = fixture.open(Approval::Never).unwrap();
        assert_ne!(*vault.key, *old_key);
        assert!(files(&fixture.root).is_empty());
        assert!(vault.load_named(REBUILDABLE).unwrap().is_none());
        vault.save_named(REBUILDABLE, b"new").unwrap();
        drop(vault);
        let reopened = fixture.open(Approval::Never).unwrap();
        assert_eq!(
            reopened
                .load_named(REBUILDABLE)
                .unwrap()
                .unwrap()
                .as_slice(),
            b"new"
        );
        // The old item is left exactly as it was.
        assert_eq!(*platform::legacy_key(&fixture.scope).unwrap(), *old_key);
    }

    #[test]
    fn old_item_with_any_user_record_is_refused_without_approval_and_nothing_changes() {
        for record in [
            "profiles",
            "history",
            "local-lock",
            "saved-queries",
            "connection-groups",
        ] {
            let fixture = Fixture::new();
            let (old, old_key) = old_install(&fixture.root, &fixture.namespace);
            match record {
                "profiles" => {
                    let profiles = old.load_profiles().unwrap();
                    old.save_profiles(&profiles).unwrap();
                }
                "history" => old
                    .append_history(&HistoryRecord {
                        id: uuid::Uuid::new_v4(),
                        connection_id: uuid::Uuid::new_v4(),
                        sql: "SELECT 1".into(),
                        status: "completed".into(),
                        started_at: "2026-10-05T00:00:00Z".into(),
                        duration_ms: 0,
                        rows: 1,
                        error: None,
                    })
                    .unwrap(),
                name => old.save_named(name, b"owned user record").unwrap(),
            }
            old.save_named(REBUILDABLE, b"kept").unwrap();
            drop(old);
            let before = files(&fixture.root);
            assert_eq!(before.len(), 2, "{record}");
            assert_eq!(
                describe(fixture.open(Approval::Never)),
                "error vault_keychain_approval_required",
                "{record}"
            );
            assert_eq!(files(&fixture.root), before, "{record}");
            assert!(!platform::key_present(&fixture.scope, false).unwrap());
            assert_eq!(*platform::legacy_key(&fixture.scope).unwrap(), *old_key);
        }
    }

    #[test]
    fn approved_old_key_opens_the_data_and_is_stored_for_every_later_build() {
        let fixture = Fixture::new();
        let (old, old_key) = old_install(&fixture.root, &fixture.namespace);
        write_user_data(&old);
        drop(old);
        let before = files(&fixture.root);
        // macOS answers the binary that created the old item without asking,
        // which is what it does for a build the user has approved.
        let vault = fixture.open(Approval::Ask).unwrap();
        assert_eq!(*vault.key, *old_key);
        assert_eq!(files(&fixture.root), before);
        assert_eq!(
            *platform::stored_key(&fixture.scope).unwrap().unwrap(),
            *old_key
        );
        drop(vault);
        // The old item is not consulted again: without it, and without
        // approval, the data still opens.
        platform::remove_legacy_qa_key(&fixture.scope).unwrap();
        assert_eq!(
            describe(fixture.open(Approval::Never)),
            "open language=en update-state=kept"
        );
    }

    #[test]
    fn old_key_that_does_not_open_the_data_is_never_adopted() {
        let fixture = Fixture::new();
        let (old, _) = old_install(&fixture.root, &fixture.namespace);
        write_user_data(&old);
        drop(old);
        platform::remove_legacy_qa_key(&fixture.scope).unwrap();
        let unrelated = Zeroizing::new(rand::random::<[u8; 32]>());
        platform::seed_legacy_key(&fixture.scope, &unrelated).unwrap();
        let before = files(&fixture.root);
        assert_eq!(
            describe(fixture.open(Approval::Ask)),
            "error vault_original_key_missing"
        );
        assert_eq!(files(&fixture.root), before);
        assert!(!platform::key_present(&fixture.scope, false).unwrap());
    }

    #[test]
    fn stored_item_that_is_not_a_vault_key_is_refused_and_kept() {
        let fixture = Fixture::new();
        let added = Command::new("/usr/bin/security")
            .args(["add-generic-password", "-A", "-a", "native-master-v2", "-s"])
            .arg(&fixture.scope)
            .args(["-w", "not-a-vault-key"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(added.success());
        for _ in 0..2 {
            assert_eq!(
                describe(fixture.open(Approval::Never)),
                "error vault_key_invalid"
            );
        }
        assert!(!fixture.root.join(PROFILES).exists());
    }

    const ROLE: &str = "BAZA_VAULT_QA_ROLE";
    const ROOT: &str = "BAZA_VAULT_QA_ROOT";
    const NAMESPACE: &str = "BAZA_VAULT_QA_NAMESPACE";

    /// The other build of the test below; an ordinary run passes at once.
    #[test]
    fn another_build() {
        let Ok(role) = std::env::var(ROLE) else {
            return;
        };
        let root = PathBuf::from(std::env::var(ROOT).unwrap());
        let namespace = std::env::var(NAMESPACE).unwrap();
        let result = match role.as_str() {
            "create" => {
                write_user_data(&Vault::open_owned(&root, &namespace).unwrap());
                "created".into()
            }
            "leave-old-install" => {
                let (old, _) = old_install(&root, &namespace);
                old.save_named(REBUILDABLE, b"sealed with the old key")
                    .unwrap();
                let retired = old.seal("record:license-identity", b"retired").unwrap();
                platform::atomic_write(&root.join(RETIRED[0]), &retired, false).unwrap();
                "left".into()
            }
            "leave-old-data" => {
                write_user_data(&old_install(&root, &namespace).0);
                "left".into()
            }
            "open" => describe(Vault::open_owned(&root, &namespace)),
            "open-asking" => describe(Vault::open_at(&root, &namespace, Approval::Ask)),
            "open-asking-denied" => {
                platform::refuse_keychain_prompts().unwrap();
                describe(Vault::open_at(&root, &namespace, Approval::Ask))
            }
            "remove-keys" => {
                let scope = Vault::scope(&std::path::absolute(&root).unwrap(), &namespace);
                platform::remove_qa_key(&scope).unwrap();
                "removed".into()
            }
            other => panic!("unknown role {other}"),
        };
        println!("QA-RESULT {result}");
    }
    /// Runs one role in a separately signed copy of this test binary. A copy
    /// that does not finish is waiting for a keychain prompt: it is killed,
    /// which withdraws the prompt, and the test fails.
    fn other_build(build: &Path, role: &str, root: &Path, namespace: &str) -> String {
        let test = format!(
            "{}::another_build",
            module_path!().split_once("::").unwrap().1
        );
        let mut child = Command::new(build)
            .args(["--exact", &test, "--nocapture", "--test-threads=1"])
            .env(ROLE, role)
            .env(ROOT, root)
            .env(NAMESPACE, namespace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{role}: the other build was still waiting after 10 s");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut output = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        output
            .lines()
            .find_map(|line| Some(line.split_once("QA-RESULT ")?.1))
            .unwrap_or_else(|| panic!("{role}: {output}"))
            .to_owned()
    }
    fn designated_requirement(build: &Path) -> String {
        let output = Command::new("/usr/bin/codesign")
            .args(["--display", "--requirements", "-"])
            .arg(build)
            .stderr(Stdio::null())
            .output()
            .unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("cdhash"), "{text}");
        text
    }

    #[test]
    fn differently_signed_builds_share_a_vault_and_never_wait_for_the_keychain() {
        let temp = tempfile::tempdir().unwrap();
        // What packaging does to every release: an ad-hoc signature, hence a
        // code identity of its own for each build.
        let [first, second] = ["build-a", "build-b"].map(|name| {
            let path = temp.path().join(name);
            fs::copy(std::env::current_exe().unwrap(), &path).unwrap();
            let signed = Command::new("/usr/bin/codesign")
                .args(["--force", "--sign", "-", "--identifier"])
                .arg(format!("pl.bazastudio.qa.{name}"))
                .arg(&path)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(signed.success());
            path
        });
        assert_ne!(
            designated_requirement(&first),
            designated_requirement(&second)
        );
        struct Keys<'a>(&'a Path, Vec<(PathBuf, String)>);
        impl Drop for Keys<'_> {
            fn drop(&mut self) {
                // Only the build that stored an old item may delete it.
                for (root, namespace) in &self.1 {
                    let _ = std::panic::catch_unwind(|| {
                        other_build(self.0, "remove-keys", root, namespace)
                    });
                }
            }
        }
        let mut keys = Keys(&first, Vec::new());
        let mut vault = |name: &str| {
            let owned = (
                temp.path().join(name),
                format!("BazaStudio.QA.{}", uuid::Uuid::new_v4()),
            );
            keys.1.push(owned.clone());
            owned
        };
        let (current, empty, with_data) =
            (vault("current"), vault("old-install"), vault("old-data"));
        let run = |build: &Path, role: &str, vault: &(PathBuf, String)| -> String {
            other_build(build, role, &vault.0, &vault.1)
        };
        let opened = "open language=en update-state=kept";

        // Stored by one build, read by the next.
        assert_eq!(run(&first, "create", &current), "created");
        assert_eq!(run(&second, "open", &current), opened);

        // An older build's item and no user data: a first run, no approval.
        assert_eq!(run(&first, "leave-old-install", &empty), "left");
        let clean = format!(
            "open language={} update-state=none",
            ProfileSet::default().preferences.language
        );
        assert_eq!(run(&second, "open", &empty), clean);
        assert!(files(&empty.0).is_empty());
        assert_eq!(run(&first, "open", &empty), clean);

        // An older build's item and user data: refused at once, untouched.
        assert_eq!(run(&first, "leave-old-data", &with_data), "left");
        let before = files(&with_data.0);
        assert_eq!(
            run(&second, "open", &with_data),
            "error vault_keychain_approval_required"
        );
        // The app does ask. A user who denies gets a code of its own at once.
        assert_eq!(
            run(&second, "open-asking-denied", &with_data),
            "error vault_keychain_access_denied"
        );
        assert_eq!(files(&with_data.0), before);
        // The storing build is the one macOS answers without asking, as it
        // does for a build the user approved. After that nobody is asked.
        assert_eq!(run(&first, "open-asking", &with_data), opened);
        assert_eq!(run(&second, "open", &with_data), opened);
        assert_eq!(files(&with_data.0), before);
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::{
        ffi::{CString, c_void},
        os::unix::{
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
            io::{AsRawFd, FromRawFd},
        },
        process::{Command, Stdio},
        ptr::{null, null_mut},
        sync::atomic::{AtomicBool, Ordering},
    };
    const NOFOLLOW: i32 = 0x100;
    const CLOEXEC: i32 = 0x1000000;
    const DIRECTORY: i32 = 0x100000;
    unsafe extern "C" {
        fn geteuid() -> u32;
        fn flock(fd: i32, op: i32) -> i32;
        fn openat(fd: i32, name: *const std::ffi::c_char, flags: i32, ...) -> i32;
        fn renameat(
            oldfd: i32,
            old: *const std::ffi::c_char,
            newfd: i32,
            new: *const std::ffi::c_char,
        ) -> i32;
        fn unlinkat(fd: i32, name: *const std::ffi::c_char, flags: i32) -> i32;
        fn acl_get_fd_np(fd: i32, kind: i32) -> *mut c_void;
        fn acl_get_entry(acl: *mut c_void, index: i32, entry: *mut *mut c_void) -> i32;
        fn acl_valid(acl: *mut c_void) -> i32;
        fn acl_free(acl: *mut c_void) -> i32;
        fn __error() -> *mut i32;
    }
    fn owner() -> Result<u32> {
        let uid = unsafe { geteuid() };
        ensure!(uid != 0, "vault_root_context_forbidden");
        Ok(uid)
    }
    fn pin(path: &Path, directory: bool) -> Result<File> {
        let metadata = safe(fs::symlink_metadata(path), "vault_snapshot_changed")?;
        ensure!(
            !metadata.file_type().is_symlink() && metadata.is_dir() == directory,
            "vault_unsafe_link"
        );
        let file = safe(
            fs::OpenOptions::new()
                .read(true)
                .custom_flags(NOFOLLOW | CLOEXEC | if directory { DIRECTORY } else { 0 })
                .open(path),
            "vault_snapshot_changed",
        )?;
        let opened = safe(file.metadata(), "vault_read_failed")?;
        ensure!(
            metadata.dev() == opened.dev() && metadata.ino() == opened.ino(),
            "vault_snapshot_changed"
        );
        Ok(file)
    }
    fn validate(file: &File, path: &Path, directory: bool, private: bool) -> Result<()> {
        let m = safe(file.metadata(), "vault_read_failed")?;
        let current = safe(fs::symlink_metadata(path), "vault_snapshot_changed")?;
        ensure!(
            !current.file_type().is_symlink()
                && m.dev() == current.dev()
                && m.ino() == current.ino()
                && m.is_dir() == directory,
            "vault_unsafe_link"
        );
        if private {
            ensure!(m.uid() == owner()?, "vault_owner_mismatch");
            ensure!(
                m.mode() & 0o7777 == if directory { 0o700 } else { 0o600 },
                if directory {
                    "vault_nonprivate_directory"
                } else {
                    "vault_nonprivate_file"
                }
            );
            // POSIX mode alone does not exclude extended macOS ACL grants.
            // Refuse any extended ACL; existing ACLs are never changed.
            unsafe { *__error() = 0 };
            let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), 0x100) };
            let acl_error = if acl.is_null() {
                std::io::Error::last_os_error().raw_os_error()
            } else {
                None
            };
            if acl.is_null() {
                // Darwin returns NULL/ENOENT when the already-pinned file has
                // no FILESEC_ACL property. This is absence, not an unreadable
                // ACL. Every other error still fails closed. Apple Libc:
                // posix1e/acl_file.c and gen/filesec.c (FILESEC_ACL case).
                ensure!(acl_error == Some(2), "vault_acl_read_failed");
            } else {
                struct Acl(*mut c_void);
                impl Drop for Acl {
                    fn drop(&mut self) {
                        unsafe {
                            acl_free(self.0);
                        }
                    }
                }
                let acl = Acl(acl);
                ensure!(unsafe { acl_valid(acl.0) } == 0, "vault_acl_read_failed");
                let mut entry = null_mut();
                let found = unsafe { acl_get_entry(acl.0, 0, &mut entry) };
                let missing =
                    found == -1 && std::io::Error::last_os_error().raw_os_error() == Some(22);
                ensure!(
                    missing,
                    if directory {
                        "vault_nonprivate_directory"
                    } else {
                        "vault_nonprivate_file"
                    }
                );
            }
        }
        if !directory {
            ensure!(m.nlink() == 1, "vault_unsafe_link");
        }
        Ok(())
    }
    pub struct Protection {
        _handles: Vec<File>,
    }
    pub fn open_read(path: &Path) -> Result<File> {
        let file = pin(path, false)?;
        validate(&file, path, false, true)?;
        Ok(file)
    }
    pub fn private_directory(root: &Path) -> Result<Protection> {
        guard(root, true)
    }
    /// Check that `root` and the folders above it are what they should be and
    /// hold them open. With `entries`, everything below `root` is checked
    /// too, which opens every file there for the time of the check.
    ///
    /// The folder of the lock files is guarded without its entries: a lock
    /// only depends on its own file, which `lock` checks, and on the folder
    /// being private. That folder collects one file per vault that was ever
    /// opened by this user. Opening them all for every lock made a process
    /// with several vaults run out of descriptors (at 256 for an application
    /// started from the Finder) once enough of them had piled up.
    fn guard(root: &Path, entries: bool) -> Result<Protection> {
        owner()?;
        ensure!(
            root.is_absolute()
                && !root
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
            "vault_path_invalid"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let result = (|| {
                if !root.exists() {
                    safe(
                        fs::DirBuilder::new()
                            .recursive(true)
                            .mode(0o700)
                            .create(root),
                        "vault_directory_create_failed",
                    )?;
                }
                ensure!(
                    !safe(fs::symlink_metadata(root), "vault_snapshot_changed")?
                        .file_type()
                        .is_symlink(),
                    "vault_unsafe_link"
                );
                // Canonical ancestor aliases such as Apple's /var -> /private/var are
                // allowed; every actual traversed directory is opened without following links.
                let physical = safe(fs::canonicalize(root), "vault_path_invalid")?;
                let mut held = Vec::new();
                let mut ancestors: Vec<_> = physical.ancestors().collect();
                ancestors.reverse();
                for folder in ancestors {
                    let file = pin(folder, true)?;
                    validate(&file, folder, true, folder == physical)?;
                    held.push((folder.to_owned(), file, true));
                }
                let mut pending = if entries {
                    vec![physical.clone()]
                } else {
                    Vec::new()
                };
                let mut snapshots = Vec::new();
                while let Some(folder) = pending.pop() {
                    let mut names = Vec::new();
                    for entry in safe(fs::read_dir(&folder), "vault_snapshot_changed")? {
                        let entry = safe(entry, "vault_snapshot_changed")?;
                        let path = entry.path();
                        let directory = safe(entry.file_type(), "vault_snapshot_changed")?.is_dir();
                        let file = pin(&path, directory)?;
                        validate(&file, &path, directory, true)?;
                        names.push(entry.file_name());
                        ensure!(held.len() < 50000, "vault_tree_limit");
                        held.push((path.clone(), file, directory));
                        if directory {
                            pending.push(path);
                        }
                    }
                    names.sort();
                    snapshots.push((folder, names));
                }
                for (folder, names) in snapshots {
                    let mut now = Vec::new();
                    for item in safe(fs::read_dir(folder), "vault_snapshot_changed")? {
                        now.push(safe(item, "vault_snapshot_changed")?.file_name());
                        ensure!(now.len() <= 50000, "vault_tree_limit");
                    }
                    now.sort();
                    ensure!(now == names, "vault_snapshot_changed");
                }
                for (path, file, directory) in &held {
                    validate(file, path, *directory, path.starts_with(&physical))?;
                }
                Ok(Protection {
                    _handles: held
                        .into_iter()
                        .filter_map(|(_, f, d)| d.then_some(f))
                        .collect(),
                })
            })();
            match result {
                Ok(value) => return Ok(value),
                Err(e)
                    if e.to_string() == "vault_snapshot_changed" && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(e) => return Err(e),
            }
        }
    }
    pub struct Lock {
        file: File,
        _boundary: Protection,
    }
    impl Drop for Lock {
        fn drop(&mut self) {
            unsafe {
                flock(self.file.as_raw_fd(), 8);
            }
        }
    }
    /// Folder of the lock files and the lock file of a scope in it.
    fn lock_file(scope: &str) -> Result<(PathBuf, PathBuf)> {
        let base = dirs::data_local_dir()
            .ok_or_else(|| anyhow!("vault_data_directory_missing"))?
            .join("BazaNativeLocks");
        let path = base.join(format!("{}.lock", hex(&digest(scope.as_bytes()))));
        Ok((base, path))
    }
    /// Remove the lock file of a QA scope, unless somebody holds the lock.
    /// Every test and self-test opens a vault of its own; without this the
    /// folder of the lock files would keep one more file for each of them.
    pub fn remove_qa_lock(scope: &str) {
        let Ok((_, path)) = lock_file(scope) else {
            return;
        };
        if let Ok(file) = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(NOFOLLOW | CLOEXEC)
            .open(&path)
            && unsafe { flock(file.as_raw_fd(), 2 | 4) } == 0
        {
            let _ = fs::remove_file(&path);
        }
    }
    pub fn lock(scope: &str) -> Result<Lock> {
        let (base, path) = lock_file(scope)?;
        let boundary = guard(&base, false)?;
        let file = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(NOFOLLOW | CLOEXEC)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => safe(
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(NOFOLLOW | CLOEXEC)
                    .open(&path),
                "vault_lock_failed",
            )?,
            Err(_) => bail!("vault_lock_failed"),
        };
        validate(&file, &path, false, true)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while unsafe { flock(file.as_raw_fd(), 2 | 4) } != 0 {
            ensure!(Instant::now() < deadline, "vault_busy");
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(Lock {
            file,
            _boundary: boundary,
        })
    }
    pub fn atomic_write(path: &Path, data: &[u8], first: bool) -> Result<()> {
        let parent = path.parent().ok_or_else(|| anyhow!("vault_path_invalid"))?;
        let _boundary = private_directory(parent)?;
        let parent_path = parent.to_owned();
        let parent = pin(parent, true)?;
        validate(&parent, &parent_path, true, true)?;
        let target = CString::new(
            path.file_name()
                .ok_or_else(|| anyhow!("vault_path_invalid"))?
                .as_encoded_bytes(),
        )
        .map_err(|_| anyhow!("vault_path_invalid"))?;
        let temp = CString::new(format!(".baza-native-{}.tmp", uuid::Uuid::new_v4())).unwrap();
        let fd = unsafe {
            openat(
                parent.as_raw_fd(),
                temp.as_ptr(),
                1 | 0x200 | 0x800 | NOFOLLOW | CLOEXEC,
                0o600u32,
            )
        };
        ensure!(fd >= 0, "vault_new_file_failed");
        let mut file = unsafe { File::from_raw_fd(fd) };
        let result = (|| {
            safe(file.write_all(data), "vault_write_failed")?;
            safe(file.sync_all(), "vault_write_failed")?;
            drop(file);
            ensure!(!first || !path.exists(), "vault_first_publish_exists");
            ensure!(
                unsafe {
                    renameat(
                        parent.as_raw_fd(),
                        temp.as_ptr(),
                        parent.as_raw_fd(),
                        target.as_ptr(),
                    )
                } == 0,
                "vault_atomic_publish_failed"
            );
            safe(parent.sync_all(), "vault_write_failed")?;
            Ok(())
        })();
        unsafe {
            unlinkat(parent.as_raw_fd(), temp.as_ptr(), 0);
        }
        result
    }
    // Keychain storage of the master key.
    //
    // The key is a generic password in the user's login keychain whose
    // service is the per-vault scope. securityd binds every item of a file
    // keychain to the code that stored it twice: through the trusted
    // application list of its ACL and through a partition list it fills in
    // by itself (securityd/src/acls.cpp, validatePartition). Without an
    // Apple-issued certificate a build is identified in both by its code
    // directory hash, so an item this process stored is unreadable to the
    // next build until the user types the keychain password. An ACL open to
    // any application does not change that: the partition list still names
    // one hash and only the keychain password may edit it.
    //
    // /usr/bin/security is the one reader whose identity is the same for
    // every build ("apple-tool:" partition). The current item, account
    // `native-master-v2`, is therefore stored and read through it, open to any
    // application, holding the key as 64 hexadecimal digits. This is the
    // Windows DPAPI current-user model: every process of this user can obtain
    // the key and no other user can.
    //
    // `native-master-v1` is the item older builds stored in-process for their
    // own binary. It is read at most once, with the user's approval, and never
    // changed: a different build may not delete it (errSecInvalidOwnerEdit)
    // and rewriting its data leaves it unreadable to every build.
    const ACCOUNT: &str = "native-master-v2";
    const LEGACY_ACCOUNT: &str = "native-master-v1";
    const LABEL: &str = "Baza native current-user vault";
    const COMMENT: &str = "Vault key of this macOS user. Kept for the system keychain tool so that updated builds can read it.";
    const TOOL: &str = "/usr/bin/security";
    const NOT_FOUND: i32 = -25300;
    /// Creating an item never prompts; reading one prompts only when somebody
    /// restricted it by hand, and then the user needs time to answer.
    const WRITE_LIMIT: Duration = Duration::from_secs(20);
    const READ_LIMIT: Duration = Duration::from_secs(60);
    /// How long opening waits for the user to answer the one-time approval.
    const APPROVAL_LIMIT: Duration = Duration::from_secs(if cfg!(test) { 3 } else { 120 });
    type Ref = *const c_void;
    #[repr(C)]
    struct DictCallbacks {
        version: isize,
        retain: Ref,
        release: Ref,
        description: Ref,
        equal: Ref,
        hash: Ref,
    }
    #[repr(C)]
    struct ArrayCallbacks {
        version: isize,
        retain: Ref,
        release: Ref,
        description: Ref,
        equal: Ref,
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(a: Ref, s: *const std::ffi::c_char, encoding: u32) -> Ref;
        fn CFDictionaryCreate(
            a: Ref,
            keys: *const Ref,
            values: *const Ref,
            n: isize,
            k: *const DictCallbacks,
            v: *const ArrayCallbacks,
        ) -> Ref;
        fn CFArrayCreate(
            a: Ref,
            values: *const Ref,
            n: isize,
            callbacks: *const ArrayCallbacks,
        ) -> Ref;
        #[cfg(test)]
        fn CFDataCreate(a: Ref, data: *const u8, n: isize) -> Ref;
        fn CFDataGetLength(data: Ref) -> isize;
        fn CFDataGetBytePtr(data: Ref) -> *const u8;
        fn CFDataGetTypeID() -> usize;
        fn CFGetTypeID(data: Ref) -> usize;
        fn CFRelease(data: Ref);
        static kCFTypeDictionaryKeyCallBacks: DictCallbacks;
        static kCFTypeDictionaryValueCallBacks: ArrayCallbacks;
        static kCFTypeArrayCallBacks: ArrayCallbacks;
        static kCFBooleanTrue: Ref;
        static kCFBooleanFalse: Ref;
    }
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecKeychainCopyDefault(out: *mut Ref) -> i32;
        fn SecKeychainGetStatus(chain: Ref, out: *mut u32) -> i32;
        fn SecKeychainGetPath(chain: Ref, length: *mut u32, out: *mut std::ffi::c_char) -> i32;
        fn SecItemCopyMatching(query: Ref, out: *mut Ref) -> i32;
        fn SecItemDelete(query: Ref) -> i32;
        #[cfg(test)]
        fn SecItemAdd(query: Ref, out: *mut Ref) -> i32;
        #[cfg(test)]
        fn SecAccessCreate(label: Ref, trusted: Ref, out: *mut Ref) -> i32;
        #[cfg(test)]
        fn SecKeychainSetUserInteractionAllowed(state: u8) -> i32;
        static kSecClass: Ref;
        static kSecClassGenericPassword: Ref;
        static kSecAttrService: Ref;
        static kSecAttrAccount: Ref;
        static kSecAttrSynchronizable: Ref;
        static kSecMatchSearchList: Ref;
        static kSecMatchLimit: Ref;
        static kSecMatchLimitOne: Ref;
        static kSecReturnData: Ref;
        static kSecReturnAttributes: Ref;
        #[cfg(test)]
        static kSecUseKeychain: Ref;
        #[cfg(test)]
        static kSecValueData: Ref;
        #[cfg(test)]
        static kSecAttrAccess: Ref;
        #[cfg(test)]
        static kSecAttrLabel: Ref;
    }
    struct Owned(Ref);
    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe {
                CFRelease(self.0);
            }
        }
    }
    fn own(value: Ref) -> Result<Owned> {
        ensure!(!value.is_null(), "vault_keychain_allocation_failed");
        Ok(Owned(value))
    }
    fn string(text: &str) -> Result<Owned> {
        let text = CString::new(text).map_err(|_| anyhow!("vault_keychain_scope_invalid"))?;
        own(unsafe { CFStringCreateWithCString(null(), text.as_ptr(), 0x08000100) })
    }
    fn dictionary(pairs: &[(Ref, Ref)]) -> Result<Owned> {
        let keys = pairs.iter().map(|(k, _)| *k).collect::<Vec<_>>();
        let values = pairs.iter().map(|(_, v)| *v).collect::<Vec<_>>();
        own(unsafe {
            CFDictionaryCreate(
                null(),
                keys.as_ptr(),
                values.as_ptr(),
                pairs.len() as isize,
                &raw const kCFTypeDictionaryKeyCallBacks,
                &raw const kCFTypeDictionaryValueCallBacks,
            )
        })
    }
    fn status(code: i32) -> Result<()> {
        ensure!(code == 0, "vault_keychain_error_{code}");
        Ok(())
    }
    /// The user's default keychain, accepted only when it is an unlocked,
    /// private file of this user.
    struct Chain {
        handle: Owned,
        path: String,
    }
    fn default_chain() -> Result<Chain> {
        owner()?;
        let mut value = null();
        status(unsafe { SecKeychainCopyDefault(&mut value) })?;
        let chain = own(value)?;
        let mut bytes = [0u8; 4096];
        let mut size = bytes.len() as u32;
        status(unsafe { SecKeychainGetPath(chain.0, &mut size, bytes.as_mut_ptr().cast()) })?;
        let end = bytes
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| anyhow!("vault_keychain_path_invalid"))?;
        let raw = std::str::from_utf8(&bytes[..end])
            .map_err(|_| anyhow!("vault_keychain_path_invalid"))?;
        let path = PathBuf::from(raw);
        ensure!(
            path.is_absolute() && !raw.bytes().any(|b| b.is_ascii_control()),
            "vault_keychain_path_invalid"
        );
        let physical = if path.exists() {
            path
        } else {
            PathBuf::from(format!("{raw}-db"))
        };
        let m = safe(
            fs::symlink_metadata(physical),
            "vault_keychain_path_invalid",
        )?;
        ensure!(
            m.is_file()
                && !m.file_type().is_symlink()
                && m.uid() == owner()?
                && m.mode() & 0o022 == 0,
            "vault_keychain_owner_permissions_invalid"
        );
        let mut flags = 0;
        status(unsafe { SecKeychainGetStatus(chain.0, &mut flags) })?;
        ensure!(flags & 1 != 0, "vault_keychain_locked");
        Ok(Chain {
            handle: chain,
            path: raw.to_owned(),
        })
    }
    struct Query {
        // Keep the CoreFoundation objects referenced by `pairs` alive.
        _service: Owned,
        _account: Owned,
        _search: Owned,
        pairs: Vec<(Ref, Ref)>,
    }
    /// Matches one account of one vault scope in the default keychain only.
    fn query(chain: &Chain, scope: &str, account: &str) -> Result<Query> {
        let service = string(scope)?;
        let account = string(account)?;
        let search = own(unsafe {
            CFArrayCreate(null(), &chain.handle.0, 1, &raw const kCFTypeArrayCallBacks)
        })?;
        let pairs = unsafe {
            vec![
                (kSecClass, kSecClassGenericPassword),
                (kSecAttrService, service.0),
                (kSecAttrAccount, account.0),
                (kSecAttrSynchronizable, kCFBooleanFalse),
                (kSecMatchSearchList, search.0),
            ]
        };
        Ok(Query {
            _service: service,
            _account: account,
            _search: search,
            pairs,
        })
    }
    /// Attributes are not protected by the item's access list, so this never
    /// prompts, whichever build stored the item.
    fn present(chain: &Chain, scope: &str, account: &str) -> Result<bool> {
        let mut query = query(chain, scope, account)?;
        query.pairs.extend(unsafe {
            [
                (kSecReturnAttributes, kCFBooleanTrue),
                (kSecMatchLimit, kSecMatchLimitOne),
            ]
        });
        let dict = dictionary(&query.pairs)?;
        let mut result = null();
        let code = unsafe { SecItemCopyMatching(dict.0, &mut result) };
        if !result.is_null() {
            unsafe { CFRelease(result) };
        }
        if code == NOT_FOUND {
            return Ok(false);
        }
        status(code)?;
        Ok(true)
    }
    struct Tool(std::process::Child);
    impl Drop for Tool {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    /// Runs the system keychain tool with an empty environment and returns its
    /// exit code with at most 95 bytes of its output. A run that does not end
    /// in time is killed, which also withdraws a prompt it may have raised.
    fn tool(
        arguments: &[&str],
        input: Option<&[u8]>,
        limit: Duration,
    ) -> Result<(i32, Zeroizing<[u8; 96]>, usize)> {
        let m = safe(
            fs::symlink_metadata(TOOL),
            "vault_keychain_tool_unavailable",
        )?;
        ensure!(
            m.is_file() && m.uid() == 0 && m.mode() & 0o022 == 0,
            "vault_keychain_tool_invalid"
        );
        let mut child = Tool(safe(
            Command::new(TOOL)
                .args(arguments)
                .env_clear()
                .current_dir("/")
                .stdin(if input.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn(),
            "vault_keychain_tool_unavailable",
        )?);
        if let Some(input) = input {
            let mut stdin = child
                .0
                .stdin
                .take()
                .ok_or_else(|| anyhow!("vault_keychain_tool_failed"))?;
            safe(stdin.write_all(input), "vault_keychain_tool_failed")?;
        }
        let deadline = Instant::now() + limit;
        let status = loop {
            if let Some(status) = safe(child.0.try_wait(), "vault_keychain_tool_failed")? {
                break status;
            }
            ensure!(Instant::now() < deadline, "vault_keychain_tool_timeout");
            std::thread::sleep(Duration::from_millis(5));
        };
        let mut output = Zeroizing::new([0u8; 96]);
        let mut length = 0;
        if let Some(mut stdout) = child.0.stdout.take() {
            loop {
                let count = safe(
                    stdout.read(&mut output[length..]),
                    "vault_keychain_tool_failed",
                )?;
                if count == 0 {
                    break;
                }
                length += count;
                ensure!(length < output.len(), "vault_key_invalid");
            }
        }
        Ok((status.code().unwrap_or(-1), output, length))
    }
    fn nibble(digit: u8) -> Result<u8> {
        match digit {
            b'0'..=b'9' => Ok(digit - b'0'),
            b'a'..=b'f' => Ok(digit - b'a' + 10),
            _ => bail!("vault_key_invalid"),
        }
    }
    fn read_stored(chain: &Chain, scope: &str) -> Result<Option<Zeroizing<[u8; 32]>>> {
        if !present(chain, scope, ACCOUNT)? {
            return Ok(None);
        }
        let (code, output, length) = tool(
            &[
                "find-generic-password",
                "-s",
                scope,
                "-a",
                ACCOUNT,
                "-w",
                &chain.path,
            ],
            None,
            READ_LIMIT,
        )?;
        // The tool exits with the low byte of the OSStatus.
        match code {
            0 => {}
            44 => return Ok(None),
            36 | 51 | 128 => bail!("vault_keychain_access_denied"),
            _ => bail!("vault_keychain_tool_error_{code}"),
        }
        ensure!(length == 65 && output[64] == b'\n', "vault_key_invalid");
        let mut key = Zeroizing::new([0u8; 32]);
        for (byte, [high, low]) in key.iter_mut().zip(output[..64].as_chunks::<2>().0) {
            *byte = (nibble(*high)? << 4) | nibble(*low)?;
        }
        Ok(Some(key))
    }
    /// Adds the current item and never replaces one: the tool refuses a
    /// duplicate and the caller reads back whichever key is stored. The key
    /// travels on the tool's standard input, never on a command line.
    fn store(chain: &Chain, scope: &str, key: &[u8; 32]) -> Result<()> {
        ensure!(
            !scope.is_empty()
                && scope
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b)),
            "vault_keychain_scope_invalid"
        );
        let mut line = Zeroizing::new(Vec::with_capacity(512 + scope.len() + 2 * chain.path.len()));
        for part in [
            "add-generic-password -A -a ",
            ACCOUNT,
            " -s ",
            scope,
            " -l \"",
            LABEL,
            "\" -j \"",
            COMMENT,
            "\" -w ",
        ] {
            line.extend_from_slice(part.as_bytes());
        }
        for byte in key {
            line.push(b"0123456789abcdef"[usize::from(byte >> 4)]);
            line.push(b"0123456789abcdef"[usize::from(byte & 15)]);
        }
        line.extend_from_slice(b" \"");
        for byte in chain.path.bytes() {
            if matches!(byte, b'"' | b'\\') {
                line.push(b'\\');
            }
            line.push(byte);
        }
        line.extend_from_slice(b"\"\n");
        // The tool reads one command of at most 4095 bytes per line.
        ensure!(line.len() < 4000, "vault_keychain_path_invalid");
        let (code, _, _) = tool(&["-i"], Some(&line), WRITE_LIMIT)?;
        ensure!(matches!(code, 0 | 45), "vault_keychain_tool_error_{code}");
        Ok(())
    }
    pub fn stored_key(scope: &str) -> Result<Option<Zeroizing<[u8; 32]>>> {
        read_stored(&default_chain()?, scope)
    }
    pub fn create_key(scope: &str) -> Result<Zeroizing<[u8; 32]>> {
        let chain = default_chain()?;
        let key = Zeroizing::new(rand::random::<[u8; 32]>());
        store(&chain, scope, &key)?;
        read_stored(&chain, scope)?.ok_or_else(|| anyhow!("vault_keychain_publication_failed"))
    }
    pub fn adopt_key(scope: &str, key: &[u8; 32]) -> Result<()> {
        store(&default_chain()?, scope, key)
    }
    pub fn legacy_key_present(scope: &str) -> Result<bool> {
        present(&default_chain()?, scope, LEGACY_ACCOUNT)
    }
    /// macOS answers this for the creating binary at once and asks the user
    /// about every other build; the call returns when the user has answered.
    fn read_legacy(scope: &str) -> Result<Zeroizing<[u8; 32]>> {
        let chain = default_chain()?;
        let mut query = query(&chain, scope, LEGACY_ACCOUNT)?;
        query.pairs.extend(unsafe {
            [
                (kSecReturnData, kCFBooleanTrue),
                (kSecMatchLimit, kSecMatchLimitOne),
            ]
        });
        let dict = dictionary(&query.pairs)?;
        let mut result = null();
        let code = unsafe { SecItemCopyMatching(dict.0, &mut result) };
        // errSecAuthFailed, errSecInteractionNotAllowed, userCanceledErr.
        ensure!(
            !matches!(code, -25293 | -25308 | -128),
            "vault_keychain_access_denied"
        );
        ensure!(code != NOT_FOUND, "vault_original_key_missing");
        status(code)?;
        let result = own(result)?;
        ensure!(
            unsafe {
                CFGetTypeID(result.0) == CFDataGetTypeID() && CFDataGetLength(result.0) == 32
            },
            "vault_key_invalid"
        );
        let mut key = Zeroizing::new([0u8; 32]);
        key.copy_from_slice(unsafe { std::slice::from_raw_parts(CFDataGetBytePtr(result.0), 32) });
        Ok(key)
    }
    /// Reads the item of an older build, waiting a bounded time for the user.
    /// When the limit passes the system prompt stays on screen, so a later
    /// call returns `vault_keychain_approval_pending` instead of stacking a
    /// second prompt behind it.
    pub fn legacy_key(scope: &str) -> Result<Zeroizing<[u8; 32]>> {
        static ASKING: AtomicBool = AtomicBool::new(false);
        struct Asking;
        impl Drop for Asking {
            fn drop(&mut self) {
                ASKING.store(false, Ordering::SeqCst);
            }
        }
        // A read the system answers at once is over in milliseconds.
        let patience = Instant::now() + Duration::from_secs(2);
        while ASKING.swap(true, Ordering::SeqCst) {
            ensure!(Instant::now() < patience, "vault_keychain_approval_pending");
            std::thread::sleep(Duration::from_millis(20));
        }
        let asking = Asking;
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let scope = scope.to_owned();
        let worker = std::thread::Builder::new()
            .name("baza-keychain-approval".into())
            .spawn(move || {
                let asking = asking;
                let result = read_legacy(&scope);
                drop(asking);
                // Nobody is waiting after the limit: the key is dropped and wiped.
                let _ = sender.send(result);
            });
        ensure!(worker.is_ok(), "vault_keychain_approval_unavailable");
        match receiver.recv_timeout(APPROVAL_LIMIT) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                bail!("vault_keychain_approval_timeout")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("vault_keychain_approval_unavailable")
            }
        }
    }
    fn remove_qa_item(scope: &str, account: &str) -> Result<()> {
        ensure!(
            scope.starts_with("BazaStudio.QA."),
            "vault_qa_namespace_required"
        );
        let query = query(&default_chain()?, scope, account)?;
        let dict = dictionary(&query.pairs)?;
        let code = unsafe { SecItemDelete(dict.0) };
        ensure!(
            code == 0 || code == NOT_FOUND,
            "vault_keychain_error_{code}"
        );
        Ok(())
    }
    pub fn remove_qa_key(scope: &str) -> Result<()> {
        remove_qa_lock(scope);
        remove_qa_item(scope, ACCOUNT)?;
        remove_qa_item(scope, LEGACY_ACCOUNT)
    }
    #[cfg(test)]
    pub fn remove_legacy_qa_key(scope: &str) -> Result<()> {
        remove_qa_item(scope, LEGACY_ACCOUNT)
    }
    /// What builds before the `native-master-v2` item did on first run: an
    /// item added in-process whose access object trusts the calling binary.
    #[cfg(test)]
    pub fn seed_legacy_key(scope: &str, key: &[u8; 32]) -> Result<()> {
        ensure!(
            scope.starts_with("BazaStudio.QA."),
            "vault_qa_namespace_required"
        );
        let chain = default_chain()?;
        let service = string(scope)?;
        let account = string(LEGACY_ACCOUNT)?;
        let label = string(LABEL)?;
        let mut access = null();
        status(unsafe { SecAccessCreate(label.0, null(), &mut access) })?;
        let access = own(access)?;
        let data = own(unsafe { CFDataCreate(null(), key.as_ptr(), 32) })?;
        let dict = dictionary(&unsafe {
            [
                (kSecClass, kSecClassGenericPassword),
                (kSecAttrService, service.0),
                (kSecAttrAccount, account.0),
                (kSecAttrSynchronizable, kCFBooleanFalse),
                (kSecUseKeychain, chain.handle.0),
                (kSecValueData, data.0),
                (kSecAttrAccess, access.0),
                (kSecAttrLabel, label.0),
            ]
        })?;
        status(unsafe { SecItemAdd(dict.0, null_mut()) })
    }
    /// Makes the system refuse, for the rest of this process, every keychain
    /// read it would otherwise ask the user about: a denial without a prompt.
    #[cfg(test)]
    pub fn refuse_keychain_prompts() -> Result<()> {
        status(unsafe { SecKeychainSetUserInteractionAllowed(0) })
    }
    #[cfg(test)]
    pub fn key_present(scope: &str, legacy: bool) -> Result<bool> {
        present(
            &default_chain()?,
            scope,
            if legacy { LEGACY_ACCOUNT } else { ACCOUNT },
        )
    }

    #[cfg(test)]
    mod mac_filesystem_tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        #[test]
        fn absent_acl_private_owned_file_is_accepted_but_hardlink_still_refused() {
            let fixture = tempfile::Builder::new()
                .permissions(fs::Permissions::from_mode(0o700))
                .tempdir()
                .unwrap();
            let root = fs::canonicalize(fixture.path()).unwrap();
            let directory = pin(&root, true).unwrap();
            validate(&directory, &root, true, true).unwrap();
            let path = root.join("owned.bin");
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            validate(&file, &path, false, true).unwrap();
            fs::hard_link(&path, root.join("owned-link.bin")).unwrap();
            assert_eq!(
                validate(&file, &path, false, true).unwrap_err().to_string(),
                "vault_unsafe_link"
            );
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }

        #[test]
        fn lock_folder_is_guarded_without_opening_the_lock_files_of_other_vaults() {
            let fixture = tempfile::Builder::new()
                .permissions(fs::Permissions::from_mode(0o700))
                .tempdir()
                .unwrap();
            let root = fs::canonicalize(fixture.path()).unwrap();
            // Lock files of other vaults, and one file the full check refuses.
            for index in 0..40 {
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(root.join(format!("{index}.lock")))
                    .unwrap();
            }
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .open(root.join("public.lock"))
                .unwrap();
            assert_eq!(
                private_directory(&root).err().unwrap().to_string(),
                "vault_nonprivate_file"
            );
            // The guard of the lock folder looks at the folder, not at them.
            let held = guard(&root, false).unwrap();
            assert!(held._handles.len() < 40);
            drop(held);
            // The folder itself must still be private.
            fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(guard(&root, false).is_err());
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        }

        #[test]
        fn qa_cleanup_removes_the_lock_file_of_its_scope_unless_it_is_held() {
            let scope = format!("BazaStudio.QA.{}", uuid::Uuid::new_v4());
            let (_, path) = lock_file(&scope).unwrap();
            assert!(!path.exists());
            let lock = lock(&scope).unwrap();
            assert!(path.exists());
            // Held by somebody: left alone.
            remove_qa_lock(&scope);
            assert!(path.exists());
            drop(lock);
            remove_qa_lock(&scope);
            assert!(!path.exists());
            // Nothing to remove is fine, and the scope can be locked again.
            remove_qa_lock(&scope);
            drop(super::lock(&scope).unwrap());
            remove_qa_lock(&scope);
            assert!(!path.exists());
        }

        #[test]
        fn explicit_acl_and_public_mode_remain_refused_without_permission_mutation() {
            let fixture = tempfile::Builder::new()
                .permissions(fs::Permissions::from_mode(0o700))
                .tempdir()
                .unwrap();
            let root = fs::canonicalize(fixture.path()).unwrap();
            let path = root.join("owned.bin");
            let file = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            validate(&file, &path, false, true).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            assert_eq!(
                validate(&file, &path, false, true).unwrap_err().to_string(),
                "vault_nonprivate_file"
            );
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o644);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let user = std::process::Command::new("/usr/bin/id")
                .arg("-un")
                .output()
                .unwrap();
            assert!(user.status.success());
            let user = String::from_utf8(user.stdout).unwrap();
            let entry = format!("user:{} allow read", user.trim());
            assert!(
                std::process::Command::new("/bin/chmod")
                    .arg("+a")
                    .arg(&entry)
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            assert_eq!(
                validate(&file, &path, false, true).unwrap_err().to_string(),
                "vault_nonprivate_file"
            );
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
