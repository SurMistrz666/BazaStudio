//! Bounded, reviewable portable definitions and authenticated BZX1 interop.
use crate::model::*;
use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{Aead, Payload},
};
use anyhow::{Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{
    Serialize,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    fmt,
    sync::atomic::{AtomicUsize, Ordering},
};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};
pub const MAX_BYTES: usize = 5 * 1024 * 1024;
pub const MAGIC: &[u8; 8] = b"BZX1\r\n\x1a\n";
const CONTENT: &str = "baza-studio-profiles-v1";
static KDF_ACTIVE: AtomicUsize = AtomicUsize::new(0);
pub fn wipe_secrets(set: &mut ProfileSet) {
    for p in &mut set.connections {
        p.password.zeroize();
        if let Some(ssh) = &mut p.ssh {
            wipe_ssh(ssh);
        }
    }
    for p in &mut set.tunnels {
        wipe_ssh(&mut p.ssh);
    }
}
fn wipe_ssh(ssh: &mut SshProfile) {
    for value in [&mut ssh.password, &mut ssh.private_key, &mut ssh.passphrase]
        .into_iter()
        .flatten()
    {
        value.zeroize();
    }
}
pub fn validate_set(set: &ProfileSet) -> Result<()> {
    ensure!(
        set.version == 1 && set.connections.len() + set.tunnels.len() <= 100,
        "profile_limit"
    );
    let mut ids = HashSet::new();
    let mut connections = HashSet::new();
    let mut tunnels = HashSet::new();
    for p in &set.connections {
        p.validate()?;
        if let Some(ssh) = &p.ssh {
            crate::ssh::validate_ssh(ssh, false)?;
        }
        ensure!(
            ids.insert(p.id) && connections.insert(p.name.to_lowercase()),
            "profile_duplicate"
        );
    }
    for p in &set.tunnels {
        crate::ssh::validate_tunnel(p, false)?;
        ensure!(
            ids.insert(p.id) && tunnels.insert(p.name.to_lowercase()),
            "profile_duplicate"
        );
    }
    ensure!(
        matches!(set.preferences.language.as_str(), "pl" | "en")
            && matches!(
                set.preferences.theme.as_str(),
                "vscode"
                    | "graphite"
                    | "midnight"
                    | "nord"
                    | "amoled"
                    | "oled"
                    | "amethyst"
                    | "light"
            )
            && set.preferences.font_size.is_finite()
            && matches!(set.preferences.density.as_str(), "compact" | "comfortable")
            && (10.0..=28.0).contains(&set.preferences.font_size)
            && set.preferences.page_rows <= crate::model::MAX_PAGE_ROWS,
        "preferences_invalid"
    );
    ensure!(
        serde_json::to_vec(set)
            .map_err(|_| anyhow!("profiles_invalid"))?
            .len()
            <= MAX_BYTES - 2048,
        "file_limit"
    );
    Ok(())
}
#[derive(Clone, Serialize)]
pub struct ImportRow {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub engine: Option<String>,
    /// Product id named by the file; empty for the generic product.
    pub product: String,
    pub can_apply: bool,
    pub needs_password: bool,
    pub needs_private_key: bool,
    pub needs_pin: bool,
    pub has_archived_credentials: bool,
    pub errors: Vec<String>,
}
pub struct ImportPreview {
    pub rows: Vec<ImportRow>,
    pub warnings: Vec<String>,
    staged: ProfileSet,
    manual: HashSet<Uuid>,
}
impl Drop for ImportPreview {
    fn drop(&mut self) {
        wipe_secrets(&mut self.staged);
    }
}
impl ImportPreview {
    pub fn template_connection(&self, id: Uuid) -> Option<ConnectionProfile> {
        self.staged
            .connections
            .iter()
            .find(|p| p.id == id)
            .map(|p| {
                let mut p = p.clone();
                p.password.zeroize();
                if let Some(ssh) = &mut p.ssh {
                    wipe_ssh(ssh);
                    ssh.password = None;
                    ssh.private_key = None;
                    ssh.passphrase = None;
                }
                p
            })
    }
    pub fn template_tunnel(&self, id: Uuid) -> Option<TunnelProfile> {
        self.staged.tunnels.iter().find(|p| p.id == id).map(|p| {
            let mut p = p.clone();
            wipe_ssh(&mut p.ssh);
            p.ssh.password = None;
            p.ssh.private_key = None;
            p.ssh.passphrase = None;
            p
        })
    }
    pub fn correct_connection(&mut self, id: Uuid, profile: &ConnectionProfile) -> Result<()> {
        let slot = self
            .staged
            .connections
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| anyhow!("selection_invalid"))?;
        profile.validate()?;
        *slot = profile.clone();
        slot.id = id;
        self.manual.insert(id);
        if let Some(row) = self.rows.iter_mut().find(|p| p.id == id) {
            row.name = profile.name.clone();
            row.errors.clear();
            row.can_apply = true;
        }
        Ok(())
    }
    pub fn correct_tunnel(&mut self, id: Uuid, profile: &TunnelProfile) -> Result<()> {
        crate::ssh::validate_tunnel(profile, true)?;
        let slot = self
            .staged
            .tunnels
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| anyhow!("selection_invalid"))?;
        *slot = profile.clone();
        slot.id = id;
        self.manual.insert(id);
        if let Some(row) = self.rows.iter_mut().find(|p| p.id == id) {
            row.name = profile.name.clone();
            row.errors.clear();
            row.can_apply = true;
            row.needs_password = false;
            row.needs_private_key = false;
            row.needs_pin = false;
        }
        Ok(())
    }
}
pub fn apply_import(
    existing: &ProfileSet,
    preview: &ImportPreview,
    selected: &[Uuid],
    include_secrets: bool,
) -> Result<ProfileSet> {
    ensure!(
        !selected.is_empty()
            && selected.len() <= 100
            && selected.iter().copied().collect::<HashSet<_>>().len() == selected.len(),
        "selection_invalid"
    );
    let mut output = existing.clone();
    for id in selected {
        let row = preview
            .rows
            .iter()
            .find(|r| r.id == *id)
            .ok_or_else(|| anyhow!("selection_invalid"))?;
        ensure!(row.can_apply, "profile_correction_required");
        if let Some(source) = preview.staged.connections.iter().find(|p| p.id == *id) {
            let mut p = source.clone();
            p.id = Uuid::new_v4();
            if !include_secrets && !preview.manual.contains(id) {
                p.password.zeroize();
                if let Some(ssh) = &mut p.ssh {
                    wipe_ssh(ssh);
                    ssh.password = None;
                    ssh.private_key = None;
                    ssh.passphrase = None;
                }
            }
            output.connections.push(p);
        } else if let Some(source) = preview.staged.tunnels.iter().find(|p| p.id == *id) {
            let mut p = source.clone();
            p.id = Uuid::new_v4();
            if !include_secrets && !preview.manual.contains(id) {
                wipe_ssh(&mut p.ssh);
                p.ssh.password = None;
                p.ssh.private_key = None;
                p.ssh.passphrase = None;
            }
            crate::ssh::validate_tunnel(&p, true)?;
            output.tunnels.push(p);
        } else {
            bail!("selection_invalid");
        }
    }
    validate_set(&output)?;
    Ok(output)
}

struct JsonSeed {
    depth: usize,
    nodes: std::rc::Rc<std::cell::Cell<usize>>,
}
impl<'de> DeserializeSeed<'de> for JsonSeed {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> std::result::Result<Value, D::Error> {
        if self.depth > 16 || self.nodes.get() >= 20000 {
            return Err(de::Error::custom("json_limit"));
        }
        self.nodes.set(self.nodes.get() + 1);
        d.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for JsonSeed {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("bounded JSON")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("number_invalid"))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_none<E: de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_unit<E: de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> std::result::Result<Value, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = a.next_element_seed(JsonSeed {
            depth: self.depth + 1,
            nodes: self.nodes.clone(),
        })? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> std::result::Result<Value, A::Error> {
        let mut out = serde_json::Map::new();
        while let Some(k) = a.next_key::<String>()? {
            if out.contains_key(&k)
                || matches!(k.as_str(), "__proto__" | "prototype" | "constructor")
            {
                return Err(de::Error::custom("unsafe_json"));
            }
            let v = a.next_value_seed(JsonSeed {
                depth: self.depth + 1,
                nodes: self.nodes.clone(),
            })?;
            out.insert(k, v);
        }
        Ok(Value::Object(out))
    }
}
fn parse_json(data: &[u8]) -> Result<Value> {
    ensure!(!data.is_empty() && data.len() <= MAX_BYTES, "file_limit");
    let mut decoder = serde_json::Deserializer::from_slice(data);
    let value = JsonSeed {
        depth: 0,
        nodes: Default::default(),
    }
    .deserialize(&mut decoder)
    .map_err(|_| anyhow!("invalid_json"))?;
    decoder.end().map_err(|_| anyhow!("invalid_json"))?;
    Ok(value)
}
fn text(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or("").into()
}
fn port(value: &Value, key: &str) -> Result<u16> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u16::try_from(v).ok())
        .ok_or_else(|| anyhow!("port_invalid"))
}
fn ssh_from(value: &Value, secrets: bool) -> Result<SshProfile> {
    Ok(SshProfile {
        host: text(value, "host"),
        port: port(value, "port")?,
        username: text(value, "username"),
        auth: match text(value, "auth").as_str() {
            "password" => SshAuth::Password,
            "private_key" => SshAuth::PrivateKey,
            _ => bail!("ssh_auth_invalid"),
        },
        password: if secrets {
            value
                .get("password")
                .and_then(Value::as_str)
                .map(String::from)
        } else {
            None
        },
        private_key: if secrets {
            value
                .get("private_key")
                .and_then(Value::as_str)
                .map(String::from)
        } else {
            None
        },
        passphrase: if secrets {
            value
                .get("passphrase")
                .and_then(Value::as_str)
                .map(String::from)
        } else {
            None
        },
        fingerprint: text(value, "host_key_fingerprint"),
    })
}
pub fn preview_import(data: &[u8], passphrase: Option<&str>) -> Result<ImportPreview> {
    let encrypted = data.starts_with(MAGIC);
    let plain = if encrypted {
        decrypt_portable(
            data,
            passphrase.ok_or_else(|| anyhow!("passphrase_required"))?,
        )?
    } else {
        Zeroizing::new(data.to_vec())
    };
    let value = parse_json(&plain)?;
    ensure!(
        value.get("format") == Some(&json!(CONTENT)) && value.get("version") == Some(&json!(1)),
        "format_unsupported"
    );
    let records = value
        .get("profiles")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("profiles_invalid"))?;
    ensure!(!records.is_empty() && records.len() <= 100, "profile_limit");
    let secrets = encrypted && value.get("includes_secrets") == Some(&Value::Bool(true));
    let mut preview = ImportPreview {
        rows: Vec::new(),
        warnings: Vec::new(),
        staged: ProfileSet::default(),
        manual: HashSet::new(),
    };
    if !encrypted {
        preview
            .warnings
            .push("plaintext_credentials_ignored".into());
    }
    for value in records {
        let id = Uuid::new_v4();
        let kind = text(value, "kind");
        let mut row = ImportRow {
            id,
            kind: if kind == "tunnel" {
                "tunnel"
            } else {
                "database"
            }
            .into(),
            name: text(value, "name"),
            engine: None,
            product: String::new(),
            can_apply: false,
            needs_password: false,
            needs_private_key: false,
            needs_pin: false,
            has_archived_credentials: secrets,
            errors: Vec::new(),
        };
        let parsed = (|| -> Result<()> {
            ensure!(
                kind == "tunnel" || kind == "database",
                "profile_kind_unsupported"
            );
            if kind == "tunnel" {
                let ssh = ssh_from(
                    value.get("ssh").ok_or_else(|| anyhow!("ssh_required"))?,
                    secrets,
                )?;
                row.needs_password = ssh.auth == SshAuth::Password
                    && ssh.password.as_ref().is_none_or(|v| v.is_empty());
                row.needs_private_key = ssh.auth == SshAuth::PrivateKey
                    && ssh.private_key.as_ref().is_none_or(|v| v.is_empty());
                let profile = TunnelProfile {
                    id,
                    name: row.name.clone(),
                    ssh,
                    target_host: text(value, "target_host"),
                    target_port: port(value, "target_port")?,
                    local_port: port(value, "local_port")?,
                };
                preview.staged.tunnels.push(profile.clone());
                crate::ssh::validate_tunnel(&profile, true)?;
            } else {
                let engine = text(value, "engine");
                row.engine = Some(engine.clone());
                row.product = text(value, "product");
                let engine = Engine::from_id(&engine)
                    .filter(|engine| engine.spec().available)
                    .ok_or_else(|| anyhow!("engine_unsupported"))?;
                let mut profile = ConnectionProfile {
                    id,
                    name: row.name.clone(),
                    engine,
                    host: text(value, "host"),
                    port: port(value, "port")?,
                    database: text(value, "database"),
                    username: text(value, "username"),
                    password: if secrets {
                        text(value, "password")
                    } else {
                        String::new()
                    },
                    tls: match text(value, "ssl_mode").as_str() {
                        "disable" => TlsMode::Disable,
                        "require" => TlsMode::Require,
                        "verify_full" => TlsMode::VerifyFull,
                        _ => bail!("tls_mode_invalid"),
                    },
                    ca_file: None,
                    sqlite_path: None,
                    color: [57, 118, 223],
                    ssh: match value.get("ssh") {
                        Some(v)
                            if v.is_object() && v.get("enabled") != Some(&Value::Bool(false)) =>
                        {
                            Some(ssh_from(v, secrets)?)
                        }
                        _ => None,
                    },
                    // Files of earlier versions name neither: the generic
                    // product of the family, no options.
                    product: text(value, "product"),
                    options: match value.get("options") {
                        None | Some(Value::Null) => Default::default(),
                        Some(Value::Object(map)) => map
                            .iter()
                            .map(|(key, value)| {
                                value
                                    .as_str()
                                    .map(|value| (key.clone(), value.to_owned()))
                                    .ok_or_else(|| anyhow!("connection_option_invalid"))
                            })
                            .collect::<Result<_>>()?,
                        Some(_) => bail!("connection_option_invalid"),
                    },
                };
                if let Some(color) = value.get("color").and_then(Value::as_str) {
                    ensure!(
                        color.len() == 7 && color.is_ascii() && color.starts_with('#'),
                        "color_invalid"
                    );
                    {
                        for (index, slot) in profile.color.iter_mut().enumerate() {
                            *slot = u8::from_str_radix(&color[1 + index * 2..3 + index * 2], 16)
                                .map_err(|_| anyhow!("color_invalid"))?;
                        }
                    }
                }
                preview.staged.connections.push(profile.clone());
                profile.validate()?;
                if let Some(ssh) = &profile.ssh {
                    crate::ssh::validate_ssh(ssh, false)?;
                }
            }
            Ok(())
        })();
        match parsed {
            Ok(()) => row.can_apply = true,
            Err(error) => row.errors.push(error.to_string()),
        };
        preview.rows.push(row);
    }
    Ok(preview)
}
fn ssh_json(profile: &SshProfile, secrets: bool) -> Value {
    let mut value = json!({"enabled":true,"host":profile.host,"port":profile.port,"username":profile.username,"auth":match profile.auth{SshAuth::Password=>"password",SshAuth::PrivateKey=>"private_key"},"host_key_fingerprint":profile.fingerprint});
    if secrets {
        for (name, secret) in [
            ("password", &profile.password),
            ("private_key", &profile.private_key),
            ("passphrase", &profile.passphrase),
        ] {
            if let Some(secret) = secret {
                value[name] = json!(secret);
            }
        }
    }
    value
}
fn serialize(set: &ProfileSet, secrets: bool) -> Result<Zeroizing<Vec<u8>>> {
    validate_set(set)?;
    let mut rows = Vec::new();
    for p in &set.connections {
        let mut value = json!({
            "kind": "database",
            "name": p.name,
            "engine": p.engine.spec().portable_name,
            "host": p.host,
            "port": p.port,
            "database": p.database,
            "username": p.username,
            "ssl_mode": match p.tls {
                TlsMode::Disable => "disable",
                TlsMode::Require => "require",
                TlsMode::VerifyFull => "verify_full",
            },
            "ssl_ca": null,
            "color": format!("#{:02x}{:02x}{:02x}", p.color[0], p.color[1], p.color[2]),
            "ssh": p.ssh.as_ref().map(|s| ssh_json(s, secrets)),
        });
        // Written only when set, so that a profile of the generic product
        // exports exactly as it did before products existed.
        if !p.product.is_empty() {
            value["product"] = json!(p.product);
        }
        if !p.options.is_empty() {
            value["options"] = json!(p.options);
        }
        if secrets {
            value["password"] = json!(p.password);
        }
        rows.push(value);
    }
    for p in &set.tunnels {
        rows.push(json!({"kind":"tunnel","name":p.name,"ssh":ssh_json(&p.ssh,secrets),"target_host":p.target_host,"target_port":p.target_port,"local_port":p.local_port}));
    }
    let output = serde_json::to_vec(
        &json!({"format":CONTENT,"version":1,"includes_secrets":secrets,"profiles":rows}),
    )
    .map_err(|_| anyhow!("profiles_invalid"))?;
    ensure!(output.len() <= MAX_BYTES - 2048, "file_limit");
    Ok(Zeroizing::new(output))
}
pub fn export_json(set: &ProfileSet) -> Result<Vec<u8>> {
    Ok(serialize(set, false)?.to_vec())
}
fn derive(phrase: &str, salt: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    ensure!((12..=1024).contains(&phrase.len()), "passphrase_invalid");
    let previous = KDF_ACTIVE.fetch_add(1, Ordering::AcqRel);
    struct Slot;
    impl Drop for Slot {
        fn drop(&mut self) {
            KDF_ACTIVE.fetch_sub(1, Ordering::AcqRel);
        }
    }
    let _slot = Slot;
    ensure!(previous < 2, "kdf_busy");
    let params = scrypt::Params::new(15, 8, 1).map_err(|_| anyhow!("kdf_invalid"))?;
    let mut key = Zeroizing::new([0u8; 32]);
    scrypt::scrypt(phrase.as_bytes(), salt, &params, key.as_mut())
        .map_err(|_| anyhow!("kdf_failed"))?;
    Ok(key)
}
pub fn export_portable(set: &ProfileSet, phrase: &str, include_secrets: bool) -> Result<Vec<u8>> {
    let payload = serialize(set, include_secrets)?;
    let salt: [u8; 16] = rand::random();
    let nonce: [u8; 12] = rand::random();
    let header=serde_json::to_vec(&json!({"v":1,"content":CONTENT,"cipher":"AES-256-GCM","kdf":"scrypt","n":32768,"r":8,"p":1,"salt":URL_SAFE_NO_PAD.encode(salt),"nonce":URL_SAFE_NO_PAD.encode(nonce)})).map_err(|_|anyhow!("header_invalid"))?;
    let mut prefix = MAGIC.to_vec();
    prefix.extend_from_slice(&(header.len() as u16).to_be_bytes());
    prefix.extend_from_slice(&header);
    let key = derive(phrase, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref()).map_err(|_| anyhow!("cipher_invalid"))?;
    let encrypted = cipher
        .encrypt(
            &nonce.into(),
            Payload {
                msg: &payload,
                aad: &prefix,
            },
        )
        .map_err(|_| anyhow!("bundle_encrypt_failed"))?;
    prefix.extend_from_slice(&encrypted);
    Ok(prefix)
}
pub fn decrypt_portable(data: &[u8], phrase: &str) -> Result<Zeroizing<Vec<u8>>> {
    ensure!(
        data.starts_with(MAGIC) && data.len() >= 26 && data.len() <= MAX_BYTES,
        "bundle_header_invalid"
    );
    let length = u16::from_be_bytes(data[8..10].try_into().unwrap()) as usize;
    ensure!(
        (1..=1024).contains(&length) && data.len() >= 10 + length + 16,
        "bundle_header_invalid"
    );
    let header = parse_json(&data[10..10 + length])?;
    let expected = json!({"v":1,"content":CONTENT,"cipher":"AES-256-GCM","kdf":"scrypt","n":32768,"r":8,"p":1});
    let map = header
        .as_object()
        .ok_or_else(|| anyhow!("bundle_header_invalid"))?;
    ensure!(
        map.len() == 9
            && expected
                .as_object()
                .unwrap()
                .iter()
                .all(|(key, value)| map.get(key) == Some(value)),
        "bundle_header_invalid"
    );
    let decode = |name: &str, len: usize| -> Result<Vec<u8>> {
        let text = text(&header, name);
        let bytes = URL_SAFE_NO_PAD
            .decode(&text)
            .map_err(|_| anyhow!("bundle_header_invalid"))?;
        ensure!(
            bytes.len() == len && URL_SAFE_NO_PAD.encode(&bytes) == text,
            "bundle_header_invalid"
        );
        Ok(bytes)
    };
    let salt = decode("salt", 16)?;
    let nonce: [u8; 12] = decode("nonce", 12)?.try_into().unwrap();
    let key = derive(phrase, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref()).map_err(|_| anyhow!("cipher_invalid"))?;
    Ok(Zeroizing::new(
        cipher
            .decrypt(
                &nonce.into(),
                Payload {
                    msg: &data[10 + length..],
                    aad: &data[..10 + length],
                },
            )
            .map_err(|_| anyhow!("bundle_authentication_failed"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn set() -> ProfileSet {
        let mut set = ProfileSet::default();
        set.tunnels.push(TunnelProfile {
            id: Uuid::new_v4(),
            name: "Owned fixture".into(),
            ssh: SshProfile {
                host: "127.0.0.1".into(),
                port: 22,
                username: "owned".into(),
                auth: SshAuth::Password,
                password: Some("SYNTHETIC-ONLY-SECRET".into()),
                private_key: None,
                passphrase: None,
                fingerprint: "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            },
            target_host: "127.0.0.1".into(),
            target_port: 5432,
            local_port: 0,
        });
        set
    }
    #[test]
    fn customization_defaults_preserve_existing_profile_sets_and_validate_density() {
        let old = br#"{"version":1,"connections":[],"tunnels":[],"preferences":{"language":"pl","theme":"vscode","font_size":12.0,"local_sql_history":false}}"#;
        let mut profiles: ProfileSet = serde_json::from_slice(old).unwrap();
        assert_eq!(profiles.preferences.accent, [57, 118, 223]);
        assert!(profiles.preferences.grid_striped && profiles.preferences.editor_line_numbers);
        assert!(!profiles.preferences.editor_word_wrap);
        for density in ["compact", "comfortable"] {
            profiles.preferences.density = density.into();
            validate_set(&profiles).unwrap();
            let restored: ProfileSet =
                serde_json::from_slice(&serde_json::to_vec(&profiles).unwrap()).unwrap();
            assert_eq!(restored.preferences.density, density);
        }
        profiles.preferences.density = "unbounded-custom-layout".into();
        assert!(validate_set(&profiles).is_err());
        // Rows per page: older sets read 1000, 0 means all (one full page).
        profiles.preferences.density = "compact".into();
        assert_eq!(profiles.preferences.page_rows, 1000);
        profiles.preferences.page_rows = 0;
        validate_set(&profiles).unwrap();
        assert_eq!(
            profiles.preferences.page_limit(),
            crate::model::MAX_PAGE_ROWS
        );
        profiles.preferences.page_rows = crate::model::MAX_PAGE_ROWS + 1;
        assert!(validate_set(&profiles).is_err());
    }
    #[test]
    fn plaintext_preview_masks_and_requires_new_password() {
        let plain = export_json(&set()).unwrap();
        assert!(!String::from_utf8_lossy(&plain).contains("SYNTHETIC-ONLY-SECRET"));
        let mut preview = preview_import(&plain, None).unwrap();
        assert!(preview.rows[0].needs_password && !preview.rows[0].can_apply);
        let id = preview.rows[0].id;
        let mut p = preview.template_tunnel(id).unwrap();
        p.ssh.password = Some("NEW-EXPLICIT-SECRET".into());
        preview.correct_tunnel(id, &p).unwrap();
        let applied = apply_import(&ProfileSet::default(), &preview, &[id], false).unwrap();
        assert_ne!(applied.tunnels[0].id, id);
        assert_eq!(
            applied.tunnels[0].ssh.password.as_deref(),
            Some("NEW-EXPLICIT-SECRET")
        );
    }
    #[test]
    fn bzx_authenticated_secret_optin_tamper_wrongphrase_and_fresh_nonce() {
        let set = set();
        let one = export_portable(&set, "owned fixture phrase", true).unwrap();
        let two = export_portable(&set, "owned fixture phrase", true).unwrap();
        assert_ne!(one, two);
        let preview = preview_import(&one, Some("owned fixture phrase")).unwrap();
        let id = preview.rows[0].id;
        assert!(apply_import(&ProfileSet::default(), &preview, &[id], false).is_err());
        let imported = apply_import(&ProfileSet::default(), &preview, &[id], true).unwrap();
        assert_eq!(
            imported.tunnels[0].ssh.password,
            set.tunnels[0].ssh.password
        );
        // An import never reuses the identity stored in the bundle.
        assert_ne!(imported.tunnels[0].id, set.tunnels[0].id);
        assert!(decrypt_portable(&one, "wrong owned phrase").is_err());
        let mut bad = one.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(decrypt_portable(&bad, "owned fixture phrase").is_err());
        // The product and the engine options of a connection travel in the
        // encrypted bundle as well. Checked here, one after the other: the
        // key derivation refuses more than two at a time.
        let bundle = export_portable(&product_set(), "owned fixture phrase", true).unwrap();
        product_set_returns(&bundle, Some("owned fixture phrase"), true);
    }
    /// A database profile as the earlier versions exported it: no product,
    /// no options, and `mariadb` for the whole MySQL family.
    const OLD_EXPORT: &str = r##"{"format":"baza-studio-profiles-v1","includes_secrets":false,"profiles":[{"color":"#479be6","database":"shop","engine":"mariadb","host":"db.example.test","kind":"database","name":"Shop","port":3306,"ssh":null,"ssl_ca":null,"ssl_mode":"require","username":"app"}],"version":1}"##;
    #[test]
    fn exports_of_earlier_versions_import_and_export_unchanged() {
        let preview = preview_import(OLD_EXPORT.as_bytes(), None).unwrap();
        assert!(preview.rows[0].can_apply && preview.rows[0].product.is_empty());
        let id = preview.rows[0].id;
        let set = apply_import(&ProfileSet::default(), &preview, &[id], false).unwrap();
        let profile = &set.connections[0];
        assert_eq!(profile.engine, crate::drivers::mysql::ENGINE);
        assert!(profile.product.is_empty() && profile.options.is_empty());
        // The same profile leaves exactly as it came in.
        assert_eq!(
            String::from_utf8(export_json(&set).unwrap()).unwrap(),
            OLD_EXPORT
        );
    }
    /// A profile of a product with options, as later engines will have.
    fn product_set() -> ProfileSet {
        let mut set = ProfileSet::default();
        let mut profile = ConnectionProfile {
            name: "Warehouse".into(),
            host: "cluster.example.test".into(),
            port: 5439,
            database: "dev".into(),
            username: "analyst".into(),
            password: "SYNTHETIC-ONLY-SECRET".into(),
            tls: TlsMode::Require,
            product: "amazon-redshift".into(),
            ..ConnectionProfile::default()
        };
        profile
            .options
            .insert("region".into(), "eu-central-1".into());
        profile.options.insert("endpoint".into(), "".into());
        set.connections.push(profile);
        set
    }
    /// An export of [`product_set`] comes back with its product and options.
    fn product_set_returns(data: &[u8], phrase: Option<&str>, secrets: bool) {
        let profile = &product_set().connections[0];
        let preview = preview_import(data, phrase).unwrap();
        assert_eq!(preview.rows[0].product, "amazon-redshift");
        assert_eq!(preview.rows[0].engine.as_deref(), Some("postgres"));
        let id = preview.rows[0].id;
        let imported = apply_import(&ProfileSet::default(), &preview, &[id], secrets).unwrap();
        let back = &imported.connections[0];
        assert_eq!(back.product, profile.product);
        assert_eq!(back.options, profile.options);
        assert_eq!(back.product_label(), "Amazon Redshift");
        assert_eq!(back.port, 5439);
        assert_eq!(
            back.password,
            if secrets {
                profile.password.as_str()
            } else {
                ""
            }
        );
    }
    #[test]
    fn product_and_options_round_trip_through_a_plain_export() {
        let plain = export_json(&product_set()).unwrap();
        assert!(!String::from_utf8_lossy(&plain).contains("SYNTHETIC-ONLY-SECRET"));
        product_set_returns(&plain, None, false);
        // Options must be texts, and an engine nobody has is refused.
        for (from, to, code) in [
            (
                r#""options":{"endpoint":"","region":"eu-central-1"}"#,
                r#""options":{"region":7}"#,
                "connection_option_invalid",
            ),
            (
                r#""engine":"postgres""#,
                r#""engine":"engine_of_a_later_build""#,
                "engine_unsupported",
            ),
        ] {
            let text = String::from_utf8(plain.clone()).unwrap();
            assert!(text.contains(from), "{text}");
            let preview = preview_import(text.replace(from, to).as_bytes(), None).unwrap();
            assert!(!preview.rows[0].can_apply);
            assert_eq!(preview.rows[0].errors, [code]);
        }
    }
    #[test]
    fn json_duplicate_prototype_depth_archive_and_limits_fail() {
        for data in [
            br#"{"profiles":[],"profiles":[]}"#.as_slice(),
            br#"{"__proto__":{}}"#.as_slice(),
            b"PK\x03\x04zip",
            b"<!DOCTYPE x><x/>",
        ] {
            assert!(preview_import(data, None).is_err());
        }
        let deep = format!("{}0{}", "[".repeat(20), "]".repeat(20));
        assert!(parse_json(deep.as_bytes()).is_err());
        assert!(preview_import(&vec![b' '; MAX_BYTES + 1], None).is_err());
    }
}
#[cfg(test)]
mod cross_implementation_tests {
    use super::*;
    /// The fixture is a BZX1 bundle exported by the earlier Python implementation of the
    /// same wire format: one database and one tunnel profile, with a synthetic host,
    /// credentials and SSH pin.
    #[test]
    fn existing_python_bzx1_fixture_authenticates_and_imports_both_kinds() {
        let bytes = include_bytes!("../tests/fixtures/owned-mixed-profiles.bzx");
        let preview = preview_import(bytes, Some("owned portable format fixture 2026")).unwrap();
        assert_eq!(preview.rows.len(), 2);
        assert!(preview.rows.iter().all(|r| r.can_apply));
        let ids = preview.rows.iter().map(|r| r.id).collect::<Vec<_>>();
        let set = apply_import(&ProfileSet::default(), &preview, &ids, true).unwrap();
        assert_eq!(set.connections.len(), 1);
        assert_eq!(set.tunnels.len(), 1);
        assert!(decrypt_portable(bytes, "another owned passphrase").is_err());
    }
}
