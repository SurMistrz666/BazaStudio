pub use crate::engine::Engine;
use crate::engine::{
    Address, EngineOptions, FieldUse, MAX_OPTION_KEY, MAX_OPTION_VALUE, MAX_OPTIONS,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    Disable,
    Require,
    VerifyFull,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SshAuth {
    Password,
    PrivateKey,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshProfile {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
    pub password: Option<String>,
    pub private_key: Option<String>,
    pub passphrase: Option<String>,
    pub fingerprint: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionProfile {
    pub id: Uuid,
    pub name: String,
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub password: String,
    pub tls: TlsMode,
    pub ca_file: Option<PathBuf>,
    pub sqlite_path: Option<PathBuf>,
    pub color: [u8; 3],
    pub ssh: Option<SshProfile>,
    /// Which product of the engine family the connection was made for
    /// (`amazon-aurora-postgresql`); empty for the generic product of the
    /// family, which is also what profiles of earlier versions hold.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub product: String,
    /// Engine-specific settings, by the key of the engine's option fields.
    /// Never a secret: those stay in the password fields.
    #[serde(default, skip_serializing_if = "EngineOptions::is_empty")]
    pub options: EngineOptions,
}
impl Default for ConnectionProfile {
    /// A new connection: the generic product of the first engine of the
    /// registry, with the defaults of that product.
    fn default() -> Self {
        let engine = Engine::new(crate::drivers::REGISTRY[0]);
        let product = engine.spec().generic();
        Self {
            id: Uuid::new_v4(),
            name: String::new(),
            engine,
            host: "localhost".into(),
            port: product.default_port,
            database: product.default_database.into(),
            username: product.default_username.into(),
            password: String::new(),
            tls: product.default_tls,
            ca_file: None,
            sqlite_path: None,
            color: [71, 155, 230],
            ssh: None,
            product: String::new(),
            options: EngineOptions::new(),
        }
    }
}
impl ConnectionProfile {
    /// Check a profile against the form of its engine: what the connection
    /// dialog requires before it saves or tests.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.name.trim().is_empty() && self.name.len() <= 120,
            "connection_name_invalid"
        );
        anyhow::ensure!(self.password.len() <= 16384, "credential_too_large");
        anyhow::ensure!(
            self.product.len() <= 64
                && self
                    .product
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "connection_product_invalid"
        );
        anyhow::ensure!(
            self.options.len() <= MAX_OPTIONS
                && self.options.iter().all(|(key, value)| {
                    !key.is_empty()
                        && key.len() <= MAX_OPTION_KEY
                        && key
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                        && value.len() <= MAX_OPTION_VALUE
                        && !value.contains('\0')
                }),
            "connection_option_invalid"
        );
        let spec = self.engine.spec();
        if !spec.available {
            // Written by a build with more engines: kept as it is.
            return Ok(());
        }
        match spec.form.address {
            Address::File => {
                anyhow::ensure!(
                    self.sqlite_path
                        .as_ref()
                        .is_some_and(|p| !p.as_os_str().is_empty()),
                    "sqlite_path_required"
                );
                anyhow::ensure!(self.ssh.is_none(), "sqlite_ssh_unsupported");
            }
            Address::Server => {
                anyhow::ensure!(
                    !self.host.trim().is_empty()
                        && !self.host.contains('\0')
                        && self.host.len() <= 253
                        && self.port > 0,
                    "database_endpoint_invalid"
                );
                anyhow::ensure!(
                    (spec.form.database != FieldUse::Required || !self.database.is_empty())
                        && !self.database.contains('\0'),
                    "database_name_invalid"
                );
                anyhow::ensure!(
                    spec.form.ssh || self.ssh.is_none(),
                    "engine_ssh_unsupported"
                );
            }
        }
        for field in spec.options {
            anyhow::ensure!(
                field.accepts(self.option(field.key)),
                "connection_option_invalid"
            );
        }
        Ok(())
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelProfile {
    pub id: Uuid,
    pub name: String,
    pub ssh: SshProfile,
    pub target_host: String,
    pub target_port: u16,
    pub local_port: u16,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preferences {
    pub language: String,
    pub theme: String,
    pub font_size: f32,
    pub local_sql_history: bool,
    #[serde(default = "default_accent")]
    pub accent: [u8; 3],
    #[serde(default = "default_density")]
    pub density: String,
    #[serde(default = "true_value")]
    pub grid_striped: bool,
    #[serde(default = "true_value")]
    pub editor_line_numbers: bool,
    #[serde(default)]
    pub editor_word_wrap: bool,
    /// Records read per page in "Dane tabeli"; 0 reads all of them, up to
    /// the limit of one page ([`MAX_PAGE_ROWS`]).
    #[serde(default = "default_page_rows")]
    pub page_rows: u32,
}
/// Most records one page of "Dane tabeli" reads, also for "all of them".
pub const MAX_PAGE_ROWS: u32 = 20_000;
fn default_page_rows() -> u32 {
    1000
}
impl Preferences {
    /// Records to ask for in one page: the setting, with 0 meaning all.
    pub fn page_limit(&self) -> u32 {
        match self.page_rows {
            0 => MAX_PAGE_ROWS,
            rows => rows.min(MAX_PAGE_ROWS),
        }
    }
}
fn default_accent() -> [u8; 3] {
    [57, 118, 223]
}
fn default_density() -> String {
    "compact".into()
}
fn true_value() -> bool {
    true
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            language: "pl".into(),
            theme: "vscode".into(),
            font_size: 12.0,
            local_sql_history: false,
            accent: default_accent(),
            density: default_density(),
            grid_striped: true,
            editor_line_numbers: true,
            editor_word_wrap: false,
            page_rows: default_page_rows(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSet {
    pub version: u32,
    pub connections: Vec<ConnectionProfile>,
    pub tunnels: Vec<TunnelProfile>,
    pub preferences: Preferences,
}
impl Default for ProfileSet {
    fn default() -> Self {
        Self {
            version: 1,
            connections: Vec::new(),
            tunnels: Vec::new(),
            preferences: Preferences::default(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct HistoryRecord {
    pub id: Uuid,
    pub connection_id: Uuid,
    pub sql: String,
    pub status: String,
    pub started_at: String,
    pub duration_ms: u64,
    pub rows: u64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Cell {
    Null,
    Bool(bool),
    Integer(String),
    Decimal(String),
    Float(String),
    Text(String),
    Json(String),
    Bytes(Vec<u8>),
    Unsupported(String),
}
impl Cell {
    pub fn display(&self) -> String {
        match self {
            Self::Null => "NULL".into(),
            Self::Bool(v) => v.to_string(),
            Self::Integer(v)
            | Self::Decimal(v)
            | Self::Float(v)
            | Self::Text(v)
            | Self::Json(v)
            | Self::Unsupported(v) => v.clone(),
            Self::Bytes(v) => v.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }
    pub fn editable(&self) -> bool {
        !matches!(self, Self::Unsupported(_) | Self::Bytes(_))
    }
    pub fn from_edited(&self, s: &str, null: bool) -> anyhow::Result<Self> {
        if null {
            return Ok(Self::Null);
        };
        Ok(match self {
            Self::Integer(_) => {
                let v = s
                    .parse::<i128>()
                    .map_err(|_| anyhow::anyhow!("integer_invalid"))?;
                Self::Integer(v.to_string())
            }
            Self::Decimal(_) => {
                let _: bigdecimal::BigDecimal =
                    s.parse().map_err(|_| anyhow::anyhow!("decimal_invalid"))?;
                Self::Decimal(s.into())
            }
            Self::Float(_) => {
                let v: safe_float::Finite = s.parse()?;
                Self::Float(v.0.to_string())
            }
            Self::Bool(_) => Self::Bool(match s.to_lowercase().as_str() {
                "true" | "1" => true,
                "false" | "0" => false,
                _ => anyhow::bail!("boolean_invalid"),
            }),
            Self::Json(_) => {
                let _: serde_json::Value =
                    serde_json::from_str(s).map_err(|_| anyhow::anyhow!("json_invalid"))?;
                Self::Json(s.into())
            }
            Self::Unsupported(_) | Self::Bytes(_) => anyhow::bail!("cell_read_only"),
            _ => Self::Text(s.into()),
        })
    }
}
mod safe_float {
    pub struct Finite(pub f64);
    impl std::str::FromStr for Finite {
        type Err = anyhow::Error;
        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let v: f64 = s.parse()?;
            anyhow::ensure!(v.is_finite(), "float_invalid");
            Ok(Self(v))
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResultColumn {
    pub name: String,
    pub data_type: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QueryResult {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Cell>>,
    pub affected: u64,
    pub truncated: bool,
    pub duration_ms: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TableRef {
    pub schema: String,
    pub name: String,
    pub kind: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
    pub default: Option<String>,
}
