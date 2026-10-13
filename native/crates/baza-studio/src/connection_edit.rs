//! Connection editor persistence: a failed write never consumes the user's draft.
use anyhow::Result;
use baza_core::{ConnectionProfile, ProfileSet, Vault, profiles};
use uuid::Uuid;
use zeroize::Zeroize;

pub struct SecretDraft(pub ConnectionProfile);
impl Drop for SecretDraft {
    fn drop(&mut self) {
        self.0.password.zeroize();
        if let Some(ssh) = &mut self.0.ssh {
            if let Some(p) = &mut ssh.password {
                p.zeroize();
            }
            if let Some(p) = &mut ssh.private_key {
                p.zeroize();
            }
            if let Some(p) = &mut ssh.passphrase {
                p.zeroize();
            }
        }
    }
}
pub struct SecretProfiles(pub ProfileSet);
impl Drop for SecretProfiles {
    fn drop(&mut self) {
        profiles::wipe_secrets(&mut self.0);
    }
}

#[derive(Default)]
pub struct EditorState {
    pub profile: Option<Uuid>,
    pub id: Uuid,
    pub section: u8,
    pub busy: bool,
    pub saving: bool,
    pub feedback: Option<(String, bool)>,
    pub disabled_ssh: Option<baza_core::SshProfile>,
    ssh_paused_by_engine: bool,
    pub fingerprint: Option<[u8; 32]>,
    key_load_request: Uuid,
    /// A host key is being fetched from the SSH server.
    pub scanning: bool,
    /// Result line under the fingerprint field; `true` marks a warning.
    pub scan_note: Option<(String, bool)>,
    /// The last test met a server key other than the fingerprint in the draft.
    pub changed_key: Option<baza_core::ssh::HostKeyScan>,
    /// The server key fingerprint the draft had when the editor was opened.
    /// A field that still holds it was not touched by the user.
    opened_key: String,
}
impl EditorState {
    pub fn open(&mut self, profile: Uuid) {
        self.close();
        self.profile = Some(profile);
        self.id = Uuid::new_v4();
    }
    /// Open the editor on a draft and note the server key it starts with.
    pub fn open_draft(&mut self, draft: &ConnectionProfile) {
        self.open(draft.id);
        self.opened_key = draft
            .ssh
            .as_ref()
            .map(|ssh| ssh.fingerprint.clone())
            .unwrap_or_default();
    }
    /// The server key fingerprint the draft had when the editor was opened.
    pub fn opened_key(&self) -> &str {
        &self.opened_key
    }
    /// A connection of the edited profile trusted `key` on first use while
    /// the editor was open. Show it in the draft, unless the user has
    /// already changed the field or points the draft at another server.
    pub fn key_learned(
        &mut self,
        saved: &baza_core::SshProfile,
        draft: &mut ConnectionProfile,
        key: &str,
    ) {
        if let Some(ssh) = draft.ssh.as_mut()
            && ssh.fingerprint == self.opened_key
            && same_server(saved, ssh)
        {
            ssh.fingerprint = key.to_owned();
            self.opened_key = key.to_owned();
        }
    }
    pub fn matches(&self, id: Uuid) -> bool {
        self.profile.is_some() && self.id == id
    }
    pub fn begin_key_load(&mut self) -> Uuid {
        self.key_load_request = Uuid::new_v4();
        self.key_load_request
    }
    pub fn invalidate_key_load(&mut self) {
        self.key_load_request = Uuid::nil();
    }
    pub fn accepts_key_result(&self, editor: Uuid, request: Uuid) -> bool {
        self.matches(editor) && !self.busy && !request.is_nil() && self.key_load_request == request
    }
    fn cache_ssh(&mut self, value: baza_core::SshProfile) {
        if let Some(mut previous) = self.disabled_ssh.replace(value) {
            wipe_ssh(&mut previous);
        }
    }
    /// Make the draft a connection of the generic product of `engine`.
    #[cfg(test)]
    pub fn set_engine(&mut self, profile: &mut ConnectionProfile, engine: baza_core::Engine) {
        self.set_product(profile, engine, engine.spec().generic());
    }
    /// Make the draft a connection of `product` of `engine`: what the product
    /// picker does. The port, the TLS mode, the database and the user name
    /// take the defaults of the product where the draft still holds the
    /// defaults of the previous one; what the user typed stays. Options of
    /// another engine family are dropped, and an SSH tab is put aside while
    /// the engine cannot use one.
    pub fn set_product(
        &mut self,
        profile: &mut ConnectionProfile,
        engine: baza_core::Engine,
        product: &'static baza_core::ProductSpec,
    ) {
        let stored = engine.spec().stored_product(product);
        if profile.engine == engine && profile.product == stored {
            return;
        }
        self.invalidate_key_load();
        let previous = profile.product_spec();
        let family_changed = profile.engine != engine;
        if family_changed {
            profile.options.clear();
        }
        profile.engine = engine;
        profile.product = stored.to_owned();
        // A port of another protocol is never right; within a family a port
        // the user chose is kept.
        if family_changed || profile.port == previous.default_port || profile.port == 0 {
            profile.port = product.default_port;
        }
        if profile.tls == previous.default_tls {
            profile.tls = product.default_tls;
        }
        if profile.database == previous.default_database {
            profile.database = product.default_database.to_owned();
        }
        if profile.username == previous.default_username {
            profile.username = product.default_username.to_owned();
        }
        if !engine.spec().form.ssh {
            if let Some(ssh) = profile.ssh.take() {
                self.cache_ssh(ssh);
                self.ssh_paused_by_engine = true;
            }
        } else if self.ssh_paused_by_engine {
            profile.ssh = self.disabled_ssh.take();
            self.ssh_paused_by_engine = false;
        }
    }
    pub fn set_ssh_enabled(&mut self, profile: &mut ConnectionProfile, enabled: bool) {
        self.invalidate_key_load();
        self.ssh_paused_by_engine = false;
        if enabled {
            if profile.ssh.is_none() {
                profile.ssh = Some(self.disabled_ssh.take().unwrap_or(baza_core::SshProfile {
                    host: String::new(),
                    port: 22,
                    username: String::new(),
                    auth: baza_core::SshAuth::PrivateKey,
                    password: None,
                    private_key: None,
                    passphrase: None,
                    fingerprint: String::new(),
                }));
            }
        } else if let Some(ssh) = profile.ssh.take() {
            self.cache_ssh(ssh);
        }
    }
    pub fn close(&mut self) {
        *self = Self::default();
    }
}
impl Drop for EditorState {
    fn drop(&mut self) {
        if let Some(ssh) = &mut self.disabled_ssh {
            wipe_ssh(ssh);
        }
    }
}
fn wipe_ssh(ssh: &mut baza_core::SshProfile) {
    if let Some(p) = &mut ssh.password {
        p.zeroize();
    }
    if let Some(p) = &mut ssh.private_key {
        p.zeroize();
    }
    if let Some(p) = &mut ssh.passphrase {
        p.zeroize();
    }
}
/// Replace the active snapshot after clearing its credential fields. The returned
/// previous snapshot contains metadata only, allowing safe disposal or inspection.
pub fn replace_profiles(current: &mut ProfileSet, next: ProfileSet) -> ProfileSet {
    profiles::wipe_secrets(current);
    std::mem::replace(current, next)
}
pub fn fingerprint(draft: &ConnectionProfile) -> Result<[u8; 32]> {
    use sha2::Digest;
    let bytes = zeroize::Zeroizing::new(serde_json::to_vec(draft)?);
    Ok(sha2::Sha256::digest(bytes.as_slice()).into())
}
/// The same SSH server: its key is the same key.
fn same_server(a: &baza_core::SshProfile, b: &baza_core::SshProfile) -> bool {
    a.port == b.port && a.host.trim().eq_ignore_ascii_case(b.host.trim())
}
/// The SSH server key to store with a connection that is being saved.
///
/// `saved` is the SSH tab the vault holds for the connection now, `opened`
/// the fingerprint the editor showed when it was opened and `draft` the SSH
/// tab being saved. A key learned while the editor was open (a first
/// connection that finished meanwhile) must not be lost to a draft that never
/// knew it, so a fingerprint field the user did not touch never replaces the
/// saved key. Only two things remove a saved key:
///
/// * the user changed the field, which includes clearing it in the advanced
///   part of the SSH tab: what the field holds is saved as it is;
/// * the SSH host or port is no longer the saved one: the key belonged to
///   another server and is dropped, so the next connection learns the key of
///   the new server.
pub fn server_key_to_save(
    saved: Option<&baza_core::SshProfile>,
    opened: &str,
    draft: &baza_core::SshProfile,
) -> String {
    if draft.fingerprint != opened {
        return draft.fingerprint.clone();
    }
    match saved {
        Some(saved) if !same_server(saved, draft) => String::new(),
        Some(saved) if !saved.fingerprint.is_empty() => saved.fingerprint.clone(),
        _ => draft.fingerprint.clone(),
    }
}
/// Give a draft the server key it is saved and tested with: see
/// [`server_key_to_save`]. `before` is the set of saved profiles.
pub fn settle_server_key(before: &ProfileSet, opened: &str, draft: &mut ConnectionProfile) {
    let id = draft.id;
    if let Some(ssh) = draft.ssh.as_mut() {
        let saved = before
            .connections
            .iter()
            .find(|profile| profile.id == id)
            .and_then(|profile| profile.ssh.as_ref());
        ssh.fingerprint = server_key_to_save(saved, opened, ssh);
    }
}
/// Write a draft over its saved profile, or add it. `opened_key` is the
/// server key fingerprint the editor was opened with ([`EditorState::opened_key`]);
/// it decides which key is stored, see [`server_key_to_save`].
pub fn save_profile(
    vault: &Vault,
    before: &ProfileSet,
    draft: &ConnectionProfile,
    opened_key: &str,
) -> Result<ProfileSet> {
    draft.validate()?;
    let mut draft = SecretDraft(draft.clone());
    settle_server_key(before, opened_key, &mut draft.0);
    let draft = &draft.0;
    let mut next = SecretProfiles(before.clone());
    if let Some(old) = next.0.connections.iter_mut().find(|p| p.id == draft.id) {
        drop(SecretDraft(std::mem::replace(old, draft.clone())));
    } else {
        next.0.connections.push(draft.clone());
    }
    profiles::validate_set(&next.0)?;
    vault.save_profiles(&next.0)?;
    Ok(std::mem::take(&mut next.0))
}

/// Put a fetched host key into the draft and say what the user should do next.
pub fn apply_scan(
    ssh: &mut baza_core::SshProfile,
    result: Result<baza_core::ssh::HostKeyScan, String>,
    en: bool,
) -> (String, bool) {
    let scan = match result {
        Ok(scan) => scan,
        Err(code) => return (baza_core::ssh::describe(&code, en).into(), true),
    };
    let replaced = !ssh.fingerprint.trim().is_empty() && ssh.fingerprint != scan.fingerprint;
    ssh.fingerprint = scan.fingerprint;
    if replaced {
        let text = if en {
            "The server presents a DIFFERENT key than the fingerprint entered before. Confirm the new value with the server administrator before saving."
        } else {
            "Serwer przedstawia INNY klucz niż wpisany wcześniej odcisk. Potwierdź nową wartość u administratora serwera, zanim zapiszesz."
        };
        (text.into(), true)
    } else {
        let text = if en {
            "Fetched. Compare it with the value from the server administrator before saving."
        } else {
            "Pobrano. Porównaj z wartością od administratora serwera, zanim zapiszesz."
        };
        (format!("{} · {text}", scan.algorithm), false)
    }
}
/// Fingerprint to save after a tunnel start: the key that was trusted on first
/// use because the profile had none.
fn learned_key_of(ssh: &baza_core::SshProfile, status: &baza_core::TunnelStatus) -> Option<String> {
    if !ssh.fingerprint.is_empty() {
        return None;
    }
    status.host_key.as_ref().map(|key| key.fingerprint.clone())
}
/// The new server key carried by a `ssh_host_key_changed` error.
pub fn changed_key(error: &anyhow::Error) -> Option<baza_core::ssh::HostKeyScan> {
    error
        .downcast_ref::<baza_core::ssh::HostKeyChanged>()
        .map(|changed| changed.presented.clone())
}
/// When a database connection through a tunnel fails, the tunnel usually
/// knows the real reason (for example a refused service port).
pub fn tunnel_problem(
    manager: Option<&baza_core::TunnelManager>,
    id: Uuid,
) -> Option<anyhow::Error> {
    manager?.status(id)?.error.map(|code| anyhow::anyhow!(code))
}

struct TemporaryTunnelProfile(baza_core::TunnelProfile);
impl Drop for TemporaryTunnelProfile {
    fn drop(&mut self) {
        wipe_ssh(&mut self.0.ssh);
    }
}
/// A database opened for a profile, with the SSH tunnel it goes through when
/// the profile has one.
pub struct Opened {
    pub database: baza_core::database::Database,
    /// Owner of the tunnel of this connection. It is shut down with the
    /// connection; dropping it closes the tunnel as well.
    pub tunnel: Option<baza_core::TunnelManager>,
    /// The SSH server key that was trusted on first use because the profile
    /// had no fingerprint yet. The caller saves it into the profile.
    pub learned_key: Option<String>,
}
impl Opened {
    /// Close the database first and the tunnel under it afterwards.
    pub async fn close(self) {
        self.database.close().await;
        if let Some(tunnel) = self.tunnel {
            tunnel.shutdown().await;
        }
    }
}
/// Open the database of a profile the way every part of the application does.
///
/// A profile with an SSH tab gets its own in-process tunnel to the database
/// host and port as seen from the SSH server, and the driver dials the
/// loopback end of that tunnel; the tunnel has the id of the profile. On any
/// failure the tunnel is closed again. When the driver fails behind a tunnel,
/// the reason the tunnel knows (for example a refused database port) is
/// reported instead of the driver's network error.
pub async fn open(profile: &ConnectionProfile) -> Result<Opened> {
    profile.validate()?;
    let mut learned_key = None;
    let (port, tunnel) = if let Some(ssh) = &profile.ssh {
        let manager = baza_core::TunnelManager::new().await;
        let tunnel = TemporaryTunnelProfile(baza_core::TunnelProfile {
            id: profile.id,
            name: profile.name.clone(),
            ssh: ssh.clone(),
            target_host: profile.host.clone(),
            target_port: profile.port,
            local_port: 0,
        });
        // A failed start leaves nothing behind: the manager ends here.
        let status = manager.start(&tunnel.0).await?;
        learned_key = learned_key_of(ssh, &status);
        (status.local_port, Some(manager))
    } else {
        (None, None)
    };
    match baza_core::database::Database::connect(profile, port).await {
        Ok(database) => Ok(Opened {
            database,
            tunnel,
            learned_key,
        }),
        Err(error) => {
            let reason = tunnel_problem(tunnel.as_ref(), profile.id);
            if let Some(tunnel) = &tunnel {
                tunnel.shutdown().await;
            }
            Err(reason.unwrap_or(error))
        }
    }
}
/// Connect, run `SELECT 1` and disconnect. Returns the SSH server key that was
/// trusted on first use when the draft had no fingerprint yet.
pub async fn probe(draft: SecretDraft) -> Result<Option<String>> {
    let opened = open(&draft.0).await?;
    let learned = opened.learned_key.clone();
    let query = opened
        .database
        .query("SELECT 1", tokio_util::sync::CancellationToken::new())
        .await;
    let reason = tunnel_problem(opened.tunnel.as_ref(), draft.0.id);
    opened.close().await;
    match query {
        Ok(_) => Ok(learned),
        Err(error) => Err(reason.unwrap_or(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn owned_ssh() -> baza_core::SshProfile {
        baza_core::SshProfile {
            host: "owned-ssh.example.test".into(),
            port: 2222,
            username: "owned-user".into(),
            auth: baza_core::SshAuth::PrivateKey,
            password: Some("OWNED-FAKE-PASSWORD".into()),
            private_key: Some("OWNED-FAKE-PRIVATE-KEY".into()),
            passphrase: Some("OWNED-FAKE-PASSPHRASE".into()),
            fingerprint: "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        }
    }
    #[test]
    fn engine_cycles_restore_ssh_and_preserve_explicit_off() {
        use baza_core::drivers::{mysql, postgres, sqlite};
        let mut draft = SecretDraft(ConnectionProfile {
            name: "Owned edit".into(),
            ssh: Some(owned_ssh()),
            ..ConnectionProfile::default()
        });
        let mut editor = EditorState::default();
        editor.open(draft.0.id);
        for _ in 0..3 {
            editor.set_engine(&mut draft.0, sqlite::ENGINE);
            assert!(draft.0.ssh.is_none());
            assert_eq!(editor.disabled_ssh.as_ref().unwrap().port, 2222);
            editor.set_engine(&mut draft.0, postgres::ENGINE);
            assert_eq!(
                draft.0.ssh.as_ref().unwrap().private_key.as_deref(),
                Some("OWNED-FAKE-PRIVATE-KEY")
            );
            assert!(editor.disabled_ssh.is_none());
        }
        editor.set_ssh_enabled(&mut draft.0, false);
        for _ in 0..3 {
            editor.set_engine(&mut draft.0, sqlite::ENGINE);
            editor.set_engine(&mut draft.0, mysql::ENGINE);
            assert!(draft.0.ssh.is_none());
            assert_eq!(
                editor.disabled_ssh.as_ref().unwrap().password.as_deref(),
                Some("OWNED-FAKE-PASSWORD")
            );
        }
        editor.set_ssh_enabled(&mut draft.0, true);
        assert_eq!(
            draft.0.ssh.as_ref().unwrap().passphrase.as_deref(),
            Some("OWNED-FAKE-PASSPHRASE")
        );
        assert!(editor.disabled_ssh.is_none());
        editor.set_engine(&mut draft.0, sqlite::ENGINE);
        editor.set_engine(&mut draft.0, postgres::ENGINE);
        assert_eq!(draft.0.ssh.as_ref().unwrap().port, 2222);
    }
    #[test]
    fn replaced_profile_snapshot_erases_all_old_credentials_and_keeps_new_data() {
        let mut current = ProfileSet::default();
        current.connections.push(ConnectionProfile {
            name: "Old snapshot".into(),
            password: "OWNED-OLD-DB-PASSWORD".into(),
            ssh: Some(owned_ssh()),
            ..ConnectionProfile::default()
        });
        current.tunnels.push(baza_core::TunnelProfile {
            id: Uuid::new_v4(),
            name: "Old tunnel".into(),
            ssh: owned_ssh(),
            target_host: "owned-db.example.test".into(),
            target_port: 5432,
            local_port: 0,
        });
        let mut next = ProfileSet::default();
        next.connections.push(ConnectionProfile {
            name: "New snapshot".into(),
            password: "OWNED-NEW-DB-PASSWORD".into(),
            ssh: Some(owned_ssh()),
            ..ConnectionProfile::default()
        });
        let previous = replace_profiles(&mut current, next);
        assert!(previous.connections[0].password.is_empty());
        for ssh in [
            previous.connections[0].ssh.as_ref().unwrap(),
            &previous.tunnels[0].ssh,
        ] {
            for value in [&ssh.password, &ssh.private_key, &ssh.passphrase] {
                assert!(value.as_ref().is_none_or(|value| value.is_empty()));
            }
        }
        assert_eq!(current.connections[0].name, "New snapshot");
        assert_eq!(current.connections[0].password, "OWNED-NEW-DB-PASSWORD");
        assert!(
            current.connections[0]
                .ssh
                .as_ref()
                .unwrap()
                .private_key
                .as_ref()
                .is_some_and(|value| !value.is_empty())
        );
        drop(SecretProfiles(current));
        drop(SecretProfiles(previous));
    }
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn actual_disabled_ssh_save_removes_only_committed_ssh_and_keeps_other_profile_data() {
        let directory = tempfile::tempdir().unwrap();
        let namespace = format!("BazaStudio.QA.{}", Uuid::new_v4());
        let vault = Vault::open_owned(&directory.path().join("private"), &namespace).unwrap();
        let mut before = SecretProfiles(ProfileSet::default());
        before.0.connections.push(ConnectionProfile {
            name: "Owned off edit".into(),
            host: "owned-db.example.test".into(),
            database: "owned_fixture".into(),
            username: "owned-user".into(),
            password: "OWNED-DB-PASSWORD".into(),
            ssh: Some(owned_ssh()),
            ..ConnectionProfile::default()
        });
        vault.save_profiles(&before.0).unwrap();
        let mut draft = SecretDraft(before.0.connections[0].clone());
        let mut editor = EditorState::default();
        editor.open(draft.0.id);
        editor.set_ssh_enabled(&mut draft.0, false);
        editor.set_engine(&mut draft.0, baza_core::drivers::sqlite::ENGINE);
        editor.set_engine(&mut draft.0, baza_core::drivers::postgres::ENGINE);
        assert!(draft.0.ssh.is_none());
        assert!(editor.disabled_ssh.as_ref().unwrap().private_key.is_some());
        let stored_before = SecretProfiles(vault.load_profiles().unwrap());
        assert!(
            stored_before.0.connections[0]
                .ssh
                .as_ref()
                .unwrap()
                .private_key
                .is_some()
        );
        let saved = SecretProfiles(
            save_profile(&vault, &stored_before.0, &draft.0, editor.opened_key()).unwrap(),
        );
        assert!(saved.0.connections[0].ssh.is_none());
        let reopened = SecretProfiles(vault.load_profiles().unwrap());
        assert!(reopened.0.connections[0].ssh.is_none());
        assert_eq!(reopened.0.connections[0].password, "OWNED-DB-PASSWORD");
        assert_eq!(reopened.0.connections[0].username, "owned-user");
        assert_eq!(reopened.0.connections[0].host, "owned-db.example.test");
        assert_eq!(reopened.0.connections[0].database, "owned_fixture");
        editor.close();
        assert!(editor.disabled_ssh.is_none());
        vault.remove_owned_qa_key().unwrap();
    }
    #[test]
    fn latest_picker_wins_and_cancelled_disabled_busy_reopened_editors_reject_old_results() {
        let mut state = EditorState::default();
        let profile = Uuid::new_v4();
        state.open(profile);
        let editor = state.id;
        let first = state.begin_key_load();
        let second = state.begin_key_load();
        assert!(state.accepts_key_result(editor, second));
        assert!(!state.accepts_key_result(editor, first));
        state.invalidate_key_load();
        assert!(!state.accepts_key_result(editor, second));
        let busy_request = state.begin_key_load();
        state.busy = true;
        assert!(!state.accepts_key_result(editor, busy_request));
        state.open(profile);
        assert!(!state.accepts_key_result(editor, busy_request));
        assert!(!state.accepts_key_result(state.id, Uuid::nil()));
    }
    #[test]
    fn closed_or_reopened_editor_rejects_old_async_results() {
        let profile = Uuid::new_v4();
        let mut state = EditorState::default();
        state.open(profile);
        let old = state.id;
        assert!(state.matches(old));
        state.close();
        assert!(!state.matches(old));
        state.open(profile);
        assert!(!state.matches(old));
        assert!(state.matches(state.id));
    }
    // Well-formed SHA256 fingerprints: 43 characters of unpadded base64.
    const LEARNED: &str = "SHA256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBA";
    const ENTERED: &str = "SHA256:CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCA";
    fn ssh_with_key(key: &str) -> baza_core::SshProfile {
        baza_core::SshProfile {
            fingerprint: key.into(),
            ..owned_ssh()
        }
    }
    #[test]
    fn untouched_fingerprint_field_never_replaces_a_saved_server_key() {
        let key =
            |saved: Option<&baza_core::SshProfile>, opened: &str, draft: &baza_core::SshProfile| {
                server_key_to_save(saved, opened, draft)
            };
        // A stale draft: opened before the first connection learned the key.
        assert_eq!(
            key(Some(&ssh_with_key(LEARNED)), "", &ssh_with_key("")),
            LEARNED
        );
        // Opened with one key while another one was trusted meanwhile.
        assert_eq!(
            key(
                Some(&ssh_with_key(LEARNED)),
                ENTERED,
                &ssh_with_key(ENTERED)
            ),
            LEARNED
        );
        // The user cleared the field: the key goes.
        assert_eq!(
            key(Some(&ssh_with_key(LEARNED)), LEARNED, &ssh_with_key("")),
            ""
        );
        // The user entered another fingerprint: it is saved as entered.
        assert_eq!(
            key(
                Some(&ssh_with_key(LEARNED)),
                LEARNED,
                &ssh_with_key(ENTERED)
            ),
            ENTERED
        );
        // Another server: the old key does not apply, whichever part moved.
        let mut moved = ssh_with_key(LEARNED);
        moved.host = "other-ssh.example.test".into();
        assert_eq!(key(Some(&ssh_with_key(LEARNED)), LEARNED, &moved), "");
        let mut moved = ssh_with_key(LEARNED);
        moved.port = 2200;
        assert_eq!(key(Some(&ssh_with_key(LEARNED)), LEARNED, &moved), "");
        // ... also for a stale draft that points somewhere else.
        let mut moved = ssh_with_key("");
        moved.port = 2200;
        assert_eq!(key(Some(&ssh_with_key(LEARNED)), "", &moved), "");
        // ... unless a fingerprint was entered for the new server.
        let mut moved = ssh_with_key(ENTERED);
        moved.host = "other-ssh.example.test".into();
        assert_eq!(key(Some(&ssh_with_key(LEARNED)), LEARNED, &moved), ENTERED);
        // The same host in another letter case or with spaces is the same server.
        let mut same = ssh_with_key(LEARNED);
        same.host = " OWNED-SSH.example.test ".into();
        assert_eq!(key(Some(&ssh_with_key(LEARNED)), LEARNED, &same), LEARNED);
        // Nothing saved yet: the draft decides.
        assert_eq!(key(None, "", &ssh_with_key("")), "");
        assert_eq!(key(None, "", &ssh_with_key(ENTERED)), ENTERED);
        assert_eq!(key(Some(&ssh_with_key("")), "", &ssh_with_key("")), "");
    }
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn actual_save_keeps_a_learned_key_unless_it_was_cleared_or_the_server_changed() {
        let directory = tempfile::tempdir().unwrap();
        let namespace = format!("BazaStudio.QA.{}", Uuid::new_v4());
        let vault = Vault::open_owned(&directory.path().join("private"), &namespace).unwrap();
        // Everything below must run before the QA key is removed, so the
        // assertions are collected first.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut stored = SecretProfiles(ProfileSet::default());
            stored.0.connections.push(ConnectionProfile {
                name: "Owned key".into(),
                host: "owned-db.example.test".into(),
                database: "owned_fixture".into(),
                username: "owned-user".into(),
                password: "OWNED-DB-PASSWORD".into(),
                ssh: Some(ssh_with_key("")),
                ..ConnectionProfile::default()
            });
            vault.save_profiles(&stored.0).unwrap();
            let fingerprint =
                |set: &ProfileSet| set.connections[0].ssh.as_ref().unwrap().fingerprint.clone();

            // 1. The editor is opened before the first connection...
            let mut stale = SecretDraft(stored.0.connections[0].clone());
            let mut editor = EditorState::default();
            editor.open_draft(&stale.0);
            assert_eq!(editor.opened_key(), "");
            // ... the connection learns the key and the application saves it...
            stored.0.connections[0].ssh.as_mut().unwrap().fingerprint = LEARNED.into();
            vault.save_profiles(&stored.0).unwrap();
            // ... and the draft that never knew the key is saved afterwards.
            stale.0.name = "Owned key, renamed".into();
            let saved = SecretProfiles(
                save_profile(&vault, &stored.0, &stale.0, editor.opened_key()).unwrap(),
            );
            assert_eq!(fingerprint(&saved.0), LEARNED);
            assert_eq!(saved.0.connections[0].name, "Owned key, renamed");
            let reopened = SecretProfiles(vault.load_profiles().unwrap());
            assert_eq!(fingerprint(&reopened.0), LEARNED);
            assert_eq!(reopened.0.connections[0].password, "OWNED-DB-PASSWORD");

            // 2. Saving again without touching the field keeps it as well.
            let mut draft = SecretDraft(reopened.0.connections[0].clone());
            editor.open_draft(&draft.0);
            assert_eq!(editor.opened_key(), LEARNED);
            draft.0.database = "owned_other".into();
            let saved = SecretProfiles(
                save_profile(&vault, &reopened.0, &draft.0, editor.opened_key()).unwrap(),
            );
            assert_eq!(fingerprint(&saved.0), LEARNED);

            // 3. Another SSH port, then another host: the key is dropped,
            // and it does not come back with the old address.
            let mut draft = SecretDraft(saved.0.connections[0].clone());
            editor.open_draft(&draft.0);
            draft.0.ssh.as_mut().unwrap().port = 2200;
            let moved = SecretProfiles(
                save_profile(&vault, &saved.0, &draft.0, editor.opened_key()).unwrap(),
            );
            assert_eq!(fingerprint(&moved.0), "");
            assert_eq!(moved.0.connections[0].ssh.as_ref().unwrap().port, 2200);
            assert_eq!(fingerprint(&vault.load_profiles().unwrap()), "");
            let mut relearned = SecretProfiles(moved.0.clone());
            relearned.0.connections[0].ssh.as_mut().unwrap().fingerprint = LEARNED.into();
            vault.save_profiles(&relearned.0).unwrap();
            let mut draft = SecretDraft(relearned.0.connections[0].clone());
            editor.open_draft(&draft.0);
            draft.0.ssh.as_mut().unwrap().host = "other-ssh.example.test".into();
            let moved = SecretProfiles(
                save_profile(&vault, &relearned.0, &draft.0, editor.opened_key()).unwrap(),
            );
            assert_eq!(fingerprint(&moved.0), "");
            assert_eq!(
                moved.0.connections[0].ssh.as_ref().unwrap().host,
                "other-ssh.example.test"
            );

            // 4. The user clears the field in the advanced part: it is removed.
            let mut pinned = SecretProfiles(moved.0.clone());
            pinned.0.connections[0].ssh.as_mut().unwrap().fingerprint = LEARNED.into();
            vault.save_profiles(&pinned.0).unwrap();
            let mut draft = SecretDraft(pinned.0.connections[0].clone());
            editor.open_draft(&draft.0);
            draft.0.ssh.as_mut().unwrap().fingerprint.clear();
            let cleared = SecretProfiles(
                save_profile(&vault, &pinned.0, &draft.0, editor.opened_key()).unwrap(),
            );
            assert_eq!(fingerprint(&cleared.0), "");
            assert_eq!(fingerprint(&vault.load_profiles().unwrap()), "");
        }));
        vault.remove_owned_qa_key().unwrap();
        if let Err(failure) = outcome {
            std::panic::resume_unwind(failure);
        }
    }
    #[test]
    fn a_key_learned_while_the_editor_is_open_shows_in_an_untouched_draft_only() {
        let saved = ssh_with_key(LEARNED);
        let draft_with = |key: &str| ConnectionProfile {
            name: "Owned live".into(),
            ssh: Some(ssh_with_key(key)),
            ..ConnectionProfile::default()
        };
        let key_of = |draft: &ConnectionProfile| draft.ssh.as_ref().unwrap().fingerprint.clone();
        // Untouched and the same server: the field shows the key, and still
        // counts as untouched afterwards.
        let mut draft = draft_with("");
        let mut editor = EditorState::default();
        editor.open_draft(&draft);
        editor.key_learned(&saved, &mut draft, LEARNED);
        assert_eq!(key_of(&draft), LEARNED);
        assert_eq!(editor.opened_key(), LEARNED);
        assert_eq!(
            server_key_to_save(
                Some(&saved),
                editor.opened_key(),
                draft.ssh.as_ref().unwrap()
            ),
            LEARNED
        );
        // A fingerprint the user is typing is left alone.
        let mut draft = draft_with("");
        editor.open_draft(&draft);
        draft.ssh.as_mut().unwrap().fingerprint = ENTERED.into();
        editor.key_learned(&saved, &mut draft, LEARNED);
        assert_eq!(key_of(&draft), ENTERED);
        assert_eq!(editor.opened_key(), "");
        // So is a draft that already points at another server.
        let mut draft = draft_with("");
        editor.open_draft(&draft);
        draft.ssh.as_mut().unwrap().port = 2200;
        editor.key_learned(&saved, &mut draft, LEARNED);
        assert_eq!(key_of(&draft), "");
        // A draft without an SSH tab has nothing to show.
        let mut draft = ConnectionProfile::default();
        editor.open_draft(&draft);
        editor.key_learned(&saved, &mut draft, LEARNED);
        assert!(draft.ssh.is_none());
    }
    #[tokio::test]
    async fn actual_sqlite_probe_connects_queries_closes_and_preserves_existing_data() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owned-probe.sqlite");
        // The production connector intentionally refuses nonexistent files.
        std::fs::File::create(&path).unwrap();
        let p = ConnectionProfile {
            name: "Owned probe".into(),
            engine: baza_core::drivers::sqlite::ENGINE,
            sqlite_path: Some(path),
            ..ConnectionProfile::default()
        };
        let db = baza_core::database::Database::connect(&p, None)
            .await
            .unwrap();
        db.query("CREATE TABLE preserved(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO preserved VALUES(1,'OWNED-PROBE-SENTINEL');",tokio_util::sync::CancellationToken::new()).await.unwrap();
        db.close().await;
        probe(SecretDraft(p.clone())).await.unwrap();
        let db = baza_core::database::Database::connect(&p, None)
            .await
            .unwrap();
        let result = db
            .query(
                "SELECT value FROM preserved WHERE id=1",
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            result.rows[0][0],
            baza_core::Cell::Text("OWNED-PROBE-SENTINEL".into())
        );
        db.close().await;
    }
    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn actual_vault_conflict_keeps_complete_edit_and_can_retry_after_explicit_reload() {
        let directory = tempfile::tempdir().unwrap();
        let namespace = format!("BazaStudio.QA.{}", Uuid::new_v4());
        let path = directory.path().join("private");
        let vault = Vault::open_owned(&path, &namespace).unwrap();
        let other = Vault::open_owned(&path, &namespace).unwrap();
        let mut before = ProfileSet::default();
        let original = ConnectionProfile {
            name: "Original".into(),
            host: "original.example.test".into(),
            database: "owned_fixture".into(),
            username: "owned_user".into(),
            password: "OWNED-FAKE-PASSWORD".into(),
            ..ConnectionProfile::default()
        };
        before.connections.push(original.clone());
        vault.save_profiles(&before).unwrap();
        let mut changed = other.load_profiles().unwrap();
        changed.preferences.theme = "midnight".into();
        other.save_profiles(&changed).unwrap();
        let mut draft = SecretDraft(original);
        draft.0.name = "Edited draft".into();
        draft.0.host = "edited.example.test".into();
        draft.0.database = "edited_fixture".into();
        draft.0.password = "OWNED-EDITED-FAKE-PASSWORD".into();
        draft.0.ssh = Some(baza_core::SshProfile {
            host: "bastion.example.test".into(),
            port: 2222,
            username: "owned_ssh_user".into(),
            auth: baza_core::SshAuth::Password,
            password: Some("OWNED-FAKE-SSH-PASSWORD".into()),
            private_key: None,
            passphrase: None,
            fingerprint: "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        });
        assert_eq!(
            save_profile(&vault, &before, &draft.0, "")
                .err()
                .unwrap()
                .to_string(),
            "vault_conflict_reload"
        );
        assert_eq!(draft.0.name, "Edited draft");
        assert_eq!(draft.0.host, "edited.example.test");
        assert_eq!(draft.0.database, "edited_fixture");
        assert_eq!(draft.0.password, "OWNED-EDITED-FAKE-PASSWORD");
        assert_eq!(draft.0.ssh.as_ref().unwrap().port, 2222);
        assert!(draft.0.ssh.as_ref().unwrap().password.is_some());
        let latest = vault.load_profiles().unwrap();
        assert_eq!(latest.connections[0].name, "Original");
        let saved = save_profile(&vault, &latest, &draft.0, "").unwrap();
        assert_eq!(saved.preferences.theme, "midnight");
        assert_eq!(saved.connections[0].id, draft.0.id);
        assert_eq!(saved.connections[0].host, "edited.example.test");
        assert_eq!(
            other.load_profiles().unwrap().connections[0]
                .ssh
                .as_ref()
                .unwrap()
                .port,
            2222
        );
        vault.remove_owned_qa_key().unwrap();
    }
}

/// End-to-end proof that a database is reached through the SSH tab of its own
/// connection, on the code path of the application (`open` for "connect",
/// `probe` for "Test połączenia"). Ignored by default: it needs a real SSH
/// server and a real database behind it.
///
/// ```text
/// cargo test -p baza-studio over_ssh -- --ignored --nocapture
/// ```
///
/// SSH server: `BAZA_SSH_TEST_PORT`, `BAZA_SSH_TEST_USER`, `BAZA_SSH_TEST_KEY`
/// (path of a private key the server accepts); optional `BAZA_SSH_TEST_HOST`
/// (default `127.0.0.1`) and `BAZA_SSH_TEST_KEY_PASSPHRASE`.
/// Database, as seen from the SSH server: `BAZA_DB_TEST_ENGINE` (the stored
/// name of an engine family, such as `mysql` or `postgres`),
/// `BAZA_DB_TEST_PORT`, `BAZA_DB_TEST_USER`, `BAZA_DB_TEST_PASSWORD`,
/// `BAZA_DB_TEST_DATABASE`, `BAZA_DB_TEST_TABLE` (a table with at least one
/// row); optional `BAZA_DB_TEST_PRODUCT` (a product id of that family),
/// `BAZA_DB_TEST_HOST` (default `127.0.0.1`) and `BAZA_DB_TEST_CLOSED_PORT`
/// (a port of that host with no listener, default 1). The account needs a
/// password the server checks: one step signs in with a wrong one.
///
/// The failing sign-ins are few on purpose: OpenSSH slows down a source
/// address after repeated unauthenticated connections.
#[cfg(test)]
mod over_ssh {
    use super::*;
    use baza_core::{Cell, SshAuth, SshProfile, TlsMode};
    use tokio_util::sync::CancellationToken;

    fn variable(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|value| !value.is_empty())
    }
    fn required(name: &str) -> String {
        variable(name).unwrap_or_else(|| panic!("{name} is required"))
    }
    /// The connection as the dialog would save it, before any connection:
    /// nobody typed a fingerprint.
    fn profile() -> ConnectionProfile {
        let engine = required("BAZA_DB_TEST_ENGINE");
        let engine = baza_core::Engine::from_id(&engine)
            .unwrap_or_else(|| panic!("BAZA_DB_TEST_ENGINE {engine} is not a registered engine"));
        ConnectionProfile {
            name: "Database over SSH".into(),
            engine,
            product: variable("BAZA_DB_TEST_PRODUCT").unwrap_or_default(),
            host: variable("BAZA_DB_TEST_HOST").unwrap_or_else(|| "127.0.0.1".into()),
            port: required("BAZA_DB_TEST_PORT").parse().unwrap(),
            database: required("BAZA_DB_TEST_DATABASE"),
            username: required("BAZA_DB_TEST_USER"),
            password: required("BAZA_DB_TEST_PASSWORD"),
            tls: TlsMode::Disable,
            ssh: Some(SshProfile {
                host: variable("BAZA_SSH_TEST_HOST").unwrap_or_else(|| "127.0.0.1".into()),
                port: required("BAZA_SSH_TEST_PORT").parse().unwrap(),
                username: required("BAZA_SSH_TEST_USER"),
                auth: SshAuth::PrivateKey,
                password: None,
                private_key: Some(std::fs::read_to_string(required("BAZA_SSH_TEST_KEY")).unwrap()),
                passphrase: variable("BAZA_SSH_TEST_KEY_PASSPHRASE"),
                fingerprint: String::new(),
            }),
            ..ConnectionProfile::default()
        }
    }
    /// What the interface shows for a failure: a sentence, never the code.
    fn sentence(code: &str) -> String {
        let text = crate::messages::describe(code, false);
        println!("  {code}: {text}");
        assert!(text.contains(' ') && text != code, "{code} has no sentence");
        text
    }
    async fn listening(port: u16) -> bool {
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs a real SSH server and database: BAZA_SSH_TEST_* and BAZA_DB_TEST_*"]
    async fn database_through_the_ssh_tab_of_its_connection() {
        let mut profile = profile();
        let table = baza_core::TableRef {
            schema: String::new(),
            name: required("BAZA_DB_TEST_TABLE"),
            kind: "table".into(),
        };
        let select = format!(
            "SELECT * FROM {}",
            baza_core::sql::qualified(profile.dialect(), &table).unwrap()
        );

        // 1. First connection: key sign-in, no fingerprint saved yet. The
        // server key is trusted on first use and handed back to be saved.
        let opened = open(&profile).await.unwrap();
        let learned = opened
            .learned_key
            .clone()
            .expect("the first connection reports the server key");
        assert!(learned.starts_with("SHA256:"), "{learned}");
        println!("first use learned {learned}");
        let tunnel = opened
            .tunnel
            .clone()
            .expect("a tunnel owned by the connection");
        let before = tunnel.status(profile.id).unwrap();
        assert_eq!(before.state, "active");
        let local = before.local_port.unwrap();
        assert_ne!(
            local, profile.port,
            "the driver dials the tunnel, not the database"
        );
        // Rows of a real table arrive through the tunnel.
        let rows = opened
            .database
            .query(&select, CancellationToken::new())
            .await
            .unwrap();
        assert!(!rows.rows.is_empty() && !rows.columns.is_empty());
        assert!(rows.rows.iter().all(|row| row.len() == rows.columns.len()));
        let tables = opened.database.tables().await.unwrap();
        assert!(tables.iter().any(|found| found.name == table.name));
        let count = opened
            .database
            .query(
                &select.replace("SELECT *", "SELECT COUNT(*)"),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(count.rows[0][0], Cell::Integer(rows.rows.len().to_string()));
        let after = tunnel.status(profile.id).unwrap();
        println!(
            "{:?} over 127.0.0.1:{local}: {} rows, {} columns; tunnel sent {} B, received {} B, {} client(s)",
            profile.engine,
            rows.rows.len(),
            rows.columns.len(),
            after.bytes_sent,
            after.bytes_received,
            after.active_clients
        );
        // The traffic of the queries is the traffic of the tunnel.
        assert!(after.bytes_sent > before.bytes_sent);
        assert!(after.bytes_received > before.bytes_received);
        assert!(after.active_clients >= 1);
        assert!(after.error.is_none(), "{:?}", after.error);
        // Disconnecting closes the tunnel: nothing listens any more.
        assert!(listening(local).await);
        opened.close().await;
        assert!(
            !listening(local).await,
            "the tunnel outlived its connection"
        );
        assert_eq!(tunnel.status(profile.id).unwrap().state, "stopped");

        // 2. The application saves the learned key; later connections are
        // checked against it and learn nothing new.
        profile.ssh.as_mut().unwrap().fingerprint = learned.clone();
        let again = open(&profile).await.unwrap();
        assert!(again.learned_key.is_none());
        let status = again.tunnel.as_ref().unwrap().status(profile.id).unwrap();
        assert_eq!(status.host_key.unwrap().fingerprint, learned);
        let one = again
            .database
            .query("SELECT 1", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(one.rows[0][0], Cell::Integer("1".into()));
        again.close().await;
        println!("reconnected with the saved key");
        // "Test połączenia" on the same draft: nothing to learn either.
        assert_eq!(probe(SecretDraft(profile.clone())).await.unwrap(), None);
        // ... and on a draft without a fingerprint it returns the key.
        let mut fresh = profile.clone();
        fresh.ssh.as_mut().unwrap().fingerprint.clear();
        assert_eq!(
            probe(SecretDraft(fresh)).await.unwrap().as_deref(),
            Some(learned.as_str())
        );
        println!("connection test passes and returns the key for a new draft");

        println!("expected failures:");
        // 3. Wrong database password: the tunnel is fine, so the answer is
        // the database's, in words, and not a tunnel or network error.
        let mut wrong_password = profile.clone();
        wrong_password.password = "not-the-password".into();
        for attempt in ["connect", "test"] {
            let error = if attempt == "connect" {
                open(&wrong_password).await.err().unwrap()
            } else {
                probe(SecretDraft(wrong_password.clone()))
                    .await
                    .unwrap_err()
            };
            let code = error.to_string();
            assert_eq!(code, "database_login_rejected", "{attempt}");
            assert!(baza_core::ssh::describe_known(&code, false).is_none());
            assert!(changed_key(&error).is_none());
            let text = sentence(&code);
            assert!(text.contains("hasło"), "{text}");
        }
        // A new connection verifies the server certificate unless the SSL/TLS
        // tab says otherwise. Whatever this server offers, the answer is a
        // success or a sentence about TLS, never "the network was lost".
        let mut verified = profile.clone();
        verified.tls = TlsMode::VerifyFull;
        match open(&verified).await {
            Ok(opened) => {
                opened.close().await;
                println!("  full certificate verification passes inside the tunnel");
            }
            Err(error) => {
                let code = error.to_string();
                assert!(
                    matches!(
                        code.as_str(),
                        "database_certificate_rejected" | "database_tls_unsupported"
                    ),
                    "{code}"
                );
                assert!(sentence(&code).contains("SSL/TLS"));
                if code == "database_certificate_rejected" {
                    // The mode that sentence names works: TLS without
                    // verification, inside the SSH tunnel.
                    let mut encrypted = profile.clone();
                    encrypted.tls = TlsMode::Require;
                    let opened = open(&encrypted).await.unwrap();
                    opened
                        .database
                        .query("SELECT 1", CancellationToken::new())
                        .await
                        .unwrap();
                    opened.close().await;
                    println!("  \"encryption required\" connects inside the tunnel");
                }
            }
        }
        // A database that does not exist is named as such, too.
        let mut wrong_database = profile.clone();
        wrong_database.database = "baza_no_such_database".into();
        let code = open(&wrong_database).await.err().unwrap().to_string();
        assert!(
            matches!(
                code.as_str(),
                "database_not_found" | "database_access_denied"
            ),
            "{code}"
        );
        sentence(&code);

        // 4. Nothing listens on the database port: the SSH server refuses
        // the forward, and that is what the user is told.
        let mut closed = profile.clone();
        closed.port = variable("BAZA_DB_TEST_CLOSED_PORT")
            .map(|port| port.parse().unwrap())
            .unwrap_or(1);
        for attempt in ["connect", "test"] {
            let started = std::time::Instant::now();
            let error = if attempt == "connect" {
                open(&closed).await.err().unwrap()
            } else {
                probe(SecretDraft(closed.clone())).await.unwrap_err()
            };
            let code = error.to_string();
            assert_eq!(code, "tunnel_target_refused", "{attempt}");
            assert!(baza_core::ssh::describe_known(&code, false).is_some());
            // The answer comes at once, not after a pool timeout.
            assert!(started.elapsed() < std::time::Duration::from_secs(10));
            sentence(&code);
        }

        // 5. A saved key that is not the server's: refused before any
        // credential is sent, with the key the server really presents.
        let mut wrong_pin = profile.clone();
        wrong_pin.ssh.as_mut().unwrap().fingerprint =
            "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into();
        let error = open(&wrong_pin).await.err().unwrap();
        assert_eq!(error.to_string(), "ssh_host_key_changed");
        let presented = changed_key(&error).expect("the key the server presents");
        assert_eq!(presented.fingerprint, learned);
        sentence("ssh_host_key_changed");
        println!(
            "  server presents {} {}",
            presented.algorithm, presented.fingerprint
        );
        let error = probe(SecretDraft(wrong_pin)).await.unwrap_err();
        assert_eq!(error.to_string(), "ssh_host_key_changed");
        assert_eq!(changed_key(&error).unwrap().fingerprint, learned);

        // After all of that the good profile still connects.
        let last = open(&profile).await.unwrap();
        last.database
            .query("SELECT 1", CancellationToken::new())
            .await
            .unwrap();
        last.close().await;
    }
}
