//! What the application knows about a database engine without connecting to
//! it: the descriptor of an engine family ([`EngineSpec`]), the products that
//! speak its protocol ([`ProductSpec`]), what the interface may offer
//! ([`Capabilities`]) and how its SQL is written ([`Dialect`]).
//!
//! An engine family is one driver. It is described by one `static EngineSpec`
//! in its module under `drivers/` and listed once in `drivers::REGISTRY`;
//! nothing else in the workspace names an engine. The contract for a new
//! driver is `drivers/README.md`.
use crate::model::{ConnectionProfile, TlsMode};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

/// A text in the two languages of the interface: Polish, then English.
pub type Bilingual = (&'static str, &'static str);

/// Pick the language of a [`Bilingual`] text.
pub fn localized(text: Bilingual, en: bool) -> &'static str {
    if en { text.1 } else { text.0 }
}

/// An engine family, as stored in a connection profile.
///
/// It is a handle to the family's [`EngineSpec`]. Two engines are equal when
/// their ids are; the id is also what a profile stores (`"postgres"`,
/// `"mysql"`, `"sqlite"`, …), so the stored names never change when engines
/// are added.
///
/// A profile written by a build with more engines may name one this build
/// does not have. Such a profile still loads, is listed, is saved back
/// unchanged and cannot be connected ([`EngineSpec::available`] is false).
#[derive(Clone, Copy)]
pub struct Engine(&'static EngineSpec);

impl Engine {
    pub const fn new(spec: &'static EngineSpec) -> Self {
        Self(spec)
    }
    /// The descriptor of this family.
    pub fn spec(self) -> &'static EngineSpec {
        self.0
    }
    /// The stored name of this family.
    pub fn id(self) -> &'static str {
        self.0.id
    }
    /// Name of the family for people, for example `PostgreSQL`.
    pub fn label(self) -> &'static str {
        self.0.label
    }
    /// Default port of the family's generic product; 0 for a file.
    pub fn port(self) -> u16 {
        self.0.generic().default_port
    }
    /// Every engine family of this build, in the order of the registry.
    pub fn all() -> impl Iterator<Item = Engine> {
        crate::drivers::REGISTRY.iter().map(|spec| Engine(spec))
    }
    /// The family with this stored name or one of its aliases, compared
    /// without regard to letter case.
    pub fn from_id(id: &str) -> Option<Engine> {
        Self::all().find(|engine| {
            engine.0.id.eq_ignore_ascii_case(id)
                || engine
                    .0
                    .aliases
                    .iter()
                    .any(|alias| alias.eq_ignore_ascii_case(id))
        })
    }
    /// The product with this id, or the generic product of the family when
    /// the id is empty or unknown to this build.
    pub fn product(self, id: &str) -> &'static ProductSpec {
        self.0.product(id)
    }
    /// Family and product for a product id that is unique in the registry,
    /// as used by the product picker.
    pub fn of_product(id: &str) -> Option<(Engine, &'static ProductSpec)> {
        Self::all().find_map(|engine| {
            engine
                .0
                .products
                .iter()
                .find(|product| product.id == id)
                .map(|product| (engine, product))
        })
    }
}
impl PartialEq for Engine {
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id
    }
}
impl Eq for Engine {}
impl std::hash::Hash for Engine {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.id.hash(state);
    }
}
impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Engine({})", self.0.id)
    }
}
impl Serialize for Engine {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.id)
    }
}
impl<'de> Deserialize<'de> for Engine {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = String::deserialize(deserializer)?;
        if let Some(engine) = Engine::all().find(|engine| engine.0.id == id) {
            return Ok(engine);
        }
        unavailable(&id)
            .map(Engine)
            .ok_or_else(|| serde::de::Error::custom("engine_invalid"))
    }
}

/// Descriptor of an engine this build has no driver for, kept so that its
/// profiles survive a round trip. One is made per distinct name and lives for
/// the rest of the process; the number of names is bounded.
fn unavailable(id: &str) -> Option<&'static EngineSpec> {
    static KNOWN: OnceLock<Mutex<Vec<&'static EngineSpec>>> = OnceLock::new();
    let valid = !id.is_empty()
        && id.len() <= 40
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if !valid {
        return None;
    }
    let mut known = KNOWN.get_or_init(Default::default).lock().ok()?;
    if let Some(spec) = known.iter().find(|spec| spec.id == id) {
        return Some(spec);
    }
    if known.len() >= 64 {
        return None;
    }
    let name: &'static str = Box::leak(id.to_owned().into_boxed_str());
    let products: &'static [ProductSpec] = Box::leak(Box::new([ProductSpec {
        id: name,
        label: name,
        ..ProductSpec::GENERIC
    }]));
    let spec: &'static EngineSpec = Box::leak(Box::new(EngineSpec {
        id: name,
        aliases: &[],
        portable_name: name,
        label: name,
        available: false,
        form: ConnectionForm::SERVER,
        capabilities: Capabilities::NONE,
        dialect: Dialect::ANSI,
        options: &[],
        products,
        connect: connect_unavailable,
        describe: None,
    }));
    known.push(spec);
    Some(spec)
}
fn connect_unavailable<'a>(
    _: &'a ConnectRequest<'a>,
) -> BoxFuture<'a, anyhow::Result<Arc<dyn crate::drivers::Session>>> {
    Box::pin(async { Err(anyhow::anyhow!("engine_unsupported")) })
}

/// What a driver is given to open a session.
pub struct ConnectRequest<'a> {
    /// The validated profile: account, database, TLS mode, CA file, file
    /// path and the engine options.
    pub profile: &'a ConnectionProfile,
    /// The product the profile was saved for.
    pub product: &'static ProductSpec,
    /// Loopback end of an SSH tunnel to the server, when the profile has an
    /// SSH tab. The TCP connection goes there; `profile.host` and
    /// `profile.port` stay the identity of the server for TLS (SNI and the
    /// certificate name) and for anything the protocol itself sends.
    pub forwarded: Option<SocketAddr>,
}
impl ConnectRequest<'_> {
    /// Name of the server for TLS and for the protocol: always the host of
    /// the profile, never the loopback end of a tunnel.
    pub fn server_name(&self) -> &str {
        &self.profile.host
    }
    /// Host to open the TCP connection to.
    pub fn dial_host(&self) -> String {
        match self.forwarded {
            Some(address) => address.ip().to_string(),
            None => self.profile.host.clone(),
        }
    }
    /// Port to open the TCP connection to.
    pub fn dial_port(&self) -> u16 {
        self.forwarded
            .map_or(self.profile.port, |address| address.port())
    }
    /// Value of an engine option of the profile, or the default the spec
    /// declares for it, or an empty text.
    pub fn option(&self, key: &str) -> &str {
        self.profile.option(key)
    }
}

/// Opens a session. See `drivers::Session` for what a session must do.
pub type ConnectFn = for<'a> fn(
    &'a ConnectRequest<'a>,
) -> BoxFuture<'a, anyhow::Result<Arc<dyn crate::drivers::Session>>>;

/// Sentence for a result code that belongs to one driver, or `None` for a
/// code of another module.
pub type DescribeFn = fn(code: &str, en: bool) -> Option<&'static str>;

/// Descriptor of an engine family: one driver and everything the rest of the
/// application needs to know about it.
pub struct EngineSpec {
    /// Stored name: lower-case ASCII, digits, `_` and `-`. Never changes.
    pub id: &'static str,
    /// Other names accepted in imported profile files.
    pub aliases: &'static [&'static str],
    /// Name written into exported profile files. For the engines of the
    /// first format version it is the name older readers expect.
    pub portable_name: &'static str,
    /// Name of the family for people.
    pub label: &'static str,
    /// False only for the placeholder of an engine without a driver here.
    pub available: bool,
    /// Which parts of the connection dialog apply.
    pub form: ConnectionForm,
    pub capabilities: Capabilities,
    pub dialect: Dialect,
    /// Engine-specific fields of the connection dialog.
    pub options: &'static [OptionField],
    /// Products that speak this protocol. The first one is the generic
    /// product: the one a profile without a product id belongs to.
    pub products: &'static [ProductSpec],
    pub connect: ConnectFn,
    /// Sentences for the driver's own result codes.
    pub describe: Option<DescribeFn>,
}
impl EngineSpec {
    /// The product of profiles that name none.
    pub fn generic(&'static self) -> &'static ProductSpec {
        &self.products[0]
    }
    /// The product with this id; the generic one for an empty or unknown id.
    pub fn product(&'static self, id: &str) -> &'static ProductSpec {
        self.products
            .iter()
            .find(|product| !id.is_empty() && product.id == id)
            .unwrap_or(self.generic())
    }
    /// What to store in a profile for this product: nothing for the generic
    /// one, so that profiles of earlier versions and new ones look the same.
    pub fn stored_product(&'static self, product: &ProductSpec) -> &'static str {
        if std::ptr::eq(product, self.generic()) {
            ""
        } else {
            product.id
        }
    }
    /// Capabilities of a product: its own when it narrows the family's.
    pub fn capabilities_of(&'static self, product: &'static ProductSpec) -> &'static Capabilities {
        product.capabilities.as_ref().unwrap_or(&self.capabilities)
    }
    /// Dialect of a product: its own when it differs from the family's.
    pub fn dialect_of(&'static self, product: &'static ProductSpec) -> &'static Dialect {
        product.dialect.as_ref().unwrap_or(&self.dialect)
    }
    /// The declared option with this key.
    pub fn option(&'static self, key: &str) -> Option<&'static OptionField> {
        self.options.iter().find(|field| field.key == key)
    }
}

/// Section of the product picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Relational,
    Cloud,
    Analytics,
    KeyValue,
}
impl Group {
    pub const ALL: [Group; 4] = [
        Group::Relational,
        Group::Cloud,
        Group::Analytics,
        Group::KeyValue,
    ];
    pub fn label(self) -> Bilingual {
        match self {
            Group::Relational => ("Relacyjne", "Relational"),
            Group::Cloud => ("Usługi chmurowe", "Cloud services"),
            Group::Analytics => ("Analityka i wyszukiwanie", "Analytics and search"),
            Group::KeyValue => ("Klucz-wartość i dokumenty", "Key-value and documents"),
        }
    }
}

/// The mark of a product: one or two letters on a colour. The interface
/// paints it; no image is needed to add a product.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Badge {
    pub text: &'static str,
    pub color: [u8; 3],
}

/// A product that speaks the protocol of an engine family: the engine itself,
/// a fork or a hosted service. It is a preset: a label, a mark, defaults for
/// the connection dialog and, where the product differs, its own
/// capabilities and dialect.
#[derive(Clone, Copy)]
pub struct ProductSpec {
    /// Stored in `ConnectionProfile::product`; unique in the whole registry.
    pub id: &'static str,
    pub label: &'static str,
    pub group: Group,
    /// Offered in the product picker. The generic product of a family whose
    /// members all have a product of their own is not.
    pub listed: bool,
    pub badge: Badge,
    /// Port put into the dialog when the product is chosen; 0 for a file.
    pub default_port: u16,
    /// TLS mode put into the dialog when the product is chosen.
    pub default_tls: TlsMode,
    /// Database name put into an empty field when the product is chosen.
    pub default_database: &'static str,
    /// User name put into an empty field when the product is chosen.
    pub default_username: &'static str,
    /// Further words the picker finds the product by (`aws`, `rds`, …).
    pub keywords: &'static [&'static str],
    /// Shown under the picker: what to know before connecting.
    pub note: Option<Bilingual>,
    /// Set when the product offers less than the family.
    pub capabilities: Option<Capabilities>,
    /// Set when the product writes SQL differently from the family.
    pub dialect: Option<Dialect>,
}
impl ProductSpec {
    /// Starting point for a product: fill in the id, the label and the badge.
    pub const GENERIC: ProductSpec = ProductSpec {
        id: "",
        label: "",
        group: Group::Relational,
        listed: true,
        badge: Badge {
            text: "?",
            color: [110, 118, 129],
        },
        default_port: 0,
        // A new connection starts without TLS: most databases are reached on
        // a private network or through an SSH tunnel, and a self-signed
        // certificate would fail full verification. Cloud services that only
        // accept encrypted connections set their own mode.
        default_tls: TlsMode::Disable,
        default_database: "",
        default_username: "",
        keywords: &[],
        note: None,
        capabilities: None,
        dialect: None,
    };
    /// Whether the picker finds this product for a search text.
    pub fn matches(&self, search: &str) -> bool {
        let search = search.trim().to_lowercase();
        search.is_empty()
            || search.split_whitespace().all(|word| {
                self.label.to_lowercase().contains(word)
                    || self.id.contains(word)
                    || self.keywords.iter().any(|keyword| keyword.contains(word))
            })
    }
}

/// Whether a standard field of the connection dialog applies to an engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldUse {
    Required,
    Optional,
    Hidden,
}

/// How a connection of an engine is addressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Address {
    /// Host and port of a server.
    Server,
    /// A local file (`ConnectionProfile::sqlite_path`, whatever the engine).
    File,
}

/// Which standard parts of the connection dialog an engine uses. Fields of
/// its own are [`OptionField`]s.
#[derive(Clone, Copy, Debug)]
pub struct ConnectionForm {
    pub address: Address,
    pub database: FieldUse,
    /// Label of the database field, for engines that call it otherwise.
    pub database_label: Bilingual,
    pub username: FieldUse,
    pub password: FieldUse,
    /// The SSL/TLS tab applies.
    pub tls: bool,
    /// The SSH tab applies: the server can be reached through a tunnel.
    pub ssh: bool,
    /// File name endings offered by the file dialog of a file engine.
    pub file_extensions: &'static [&'static str],
}
impl ConnectionForm {
    /// A server with an account and a database name.
    pub const SERVER: ConnectionForm = ConnectionForm {
        address: Address::Server,
        database: FieldUse::Required,
        database_label: ("Baza danych", "Database"),
        username: FieldUse::Optional,
        password: FieldUse::Optional,
        tls: true,
        ssh: true,
        file_extensions: &[],
    };
    /// A local database file.
    pub const FILE: ConnectionForm = ConnectionForm {
        address: Address::File,
        database: FieldUse::Hidden,
        database_label: ("Baza danych", "Database"),
        username: FieldUse::Hidden,
        password: FieldUse::Hidden,
        tls: false,
        ssh: false,
        file_extensions: &[],
    };
}

/// Kind of an engine-specific connection field.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OptionKind {
    Text,
    /// A whole number within these bounds.
    Number {
        min: i64,
        max: i64,
    },
    /// Stored as `true` or `false`.
    Flag,
    /// One of these values.
    Choice(&'static [&'static str]),
}

/// An engine-specific field of the connection dialog. Its value is stored as
/// text in `ConnectionProfile::options` under `key`; a value equal to
/// `default` is not stored. Secrets do not belong here: they stay in the
/// password fields of the profile.
#[derive(Clone, Copy, Debug)]
pub struct OptionField {
    /// Stored name: lower-case ASCII, digits and `_`.
    pub key: &'static str,
    pub label: Bilingual,
    pub kind: OptionKind,
    pub default: &'static str,
    /// Greyed text of an empty text field.
    pub placeholder: &'static str,
    /// One sentence under the field.
    pub help: Option<Bilingual>,
    /// A text option that may not stay empty.
    pub required: bool,
}
impl OptionField {
    /// Check a stored value against the kind of the field.
    pub fn accepts(&self, value: &str) -> bool {
        match self.kind {
            OptionKind::Text => !value.contains('\0') && !(self.required && value.is_empty()),
            OptionKind::Number { min, max } => value
                .parse::<i64>()
                .is_ok_and(|number| (min..=max).contains(&number)),
            OptionKind::Flag => matches!(value, "true" | "false"),
            OptionKind::Choice(values) => values.contains(&value),
        }
    }
}

/// What a query is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryLanguage {
    /// SQL statements: the SQL editor with its highlighting, statement
    /// splitting and the inference of an editable table from `SELECT *`.
    Sql,
    /// Commands of the engine, one per line: a command console.
    Command,
    /// Nothing can be typed; the data is only browsed.
    None,
}

/// What the side panel and "Dane tabeli" browse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowseModel {
    /// Tables and views with typed columns.
    Tables,
    /// Collections of documents; the columns are the fields that were found.
    Documents,
    /// Groups of keys; the rows are keys with their values.
    Keys,
}
impl BrowseModel {
    /// Name of the containers in the side panel.
    pub fn containers(self) -> Bilingual {
        match self {
            BrowseModel::Tables => ("Tabele", "Tables"),
            BrowseModel::Documents => ("Kolekcje", "Collections"),
            BrowseModel::Keys => ("Klucze", "Keys"),
        }
    }
    /// Name of one record in the grid.
    pub fn record(self) -> Bilingual {
        match self {
            BrowseModel::Tables => ("wiersz", "row"),
            BrowseModel::Documents => ("dokument", "document"),
            BrowseModel::Keys => ("klucz", "key"),
        }
    }
}

/// What the interface may offer for a connection. A section, a button or a
/// menu entry that needs a missing capability is disabled and says why;
/// nothing in the interface asks which engine it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// SQL editor, command console or neither.
    pub language: QueryLanguage,
    pub browse: BrowseModel,
    /// Containers are grouped in schemas (namespaces, keyspaces); the side
    /// panel shows that level and the table designer asks for it.
    pub schemas: bool,
    /// "Dane tabeli" can filter by a column.
    pub filter: bool,
    /// "Dane tabeli" can order by a column.
    pub sort: bool,
    /// One cell of a record with a key can be changed in the grid.
    pub edit_cells: bool,
    /// A record can be added in the grid.
    pub insert_rows: bool,
    /// A record with a key can be removed in the grid.
    pub delete_rows: bool,
    /// "Projektant tabeli": a `CREATE TABLE` built from the dialect's types.
    pub table_designer: bool,
    /// "Kreator zapytań": columns, an aggregate and grouping as SQL.
    pub query_builder: bool,
    /// Rows of a CSV, TSV or JSON file can be added to a container.
    pub import: bool,
    /// A result can be saved as a file.
    pub export: bool,
    /// The "Monitor" section can read server statistics.
    pub monitor: bool,
}
impl Capabilities {
    /// Everything: a relational engine with SQL.
    pub const SQL: Capabilities = Capabilities {
        language: QueryLanguage::Sql,
        browse: BrowseModel::Tables,
        schemas: true,
        filter: true,
        sort: true,
        edit_cells: true,
        insert_rows: true,
        delete_rows: true,
        table_designer: true,
        query_builder: true,
        import: true,
        export: true,
        monitor: true,
    };
    /// Nothing but looking at tables: the starting point of a new driver.
    pub const NONE: Capabilities = Capabilities {
        language: QueryLanguage::None,
        browse: BrowseModel::Tables,
        schemas: false,
        filter: false,
        sort: false,
        edit_cells: false,
        insert_rows: false,
        delete_rows: false,
        table_designer: false,
        query_builder: false,
        import: false,
        export: true,
        monitor: false,
    };
}

/// How a text value is written as a literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextLiteral {
    /// `'it''s'`: a quote is doubled, nothing else is special.
    Standard,
    /// `N'it''s'`: the same with a national character prefix.
    National,
    /// `E'it''s \\'`: PostgreSQL escape string, a backslash is doubled.
    PostgresEscape,
    /// `'it''s \\'`: an ordinary string in which a backslash escapes, so it
    /// is doubled as well.
    Backslash,
    /// `_utf8mb4 0x…`: the bytes of the text, so that neither a quote nor a
    /// backslash of the value reaches the MySQL parser. It is a literal like
    /// `'…'`, so it takes the collation of the column it is compared with;
    /// `CONVERT(0x… USING utf8mb4)` does not, and MySQL 8 refuses to compare
    /// it with a column of any collation but the default one.
    MysqlHex,
}

/// How a page of rows is asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paging {
    /// `… LIMIT n OFFSET m`
    LimitOffset,
    /// `… OFFSET m ROWS FETCH NEXT n ROWS ONLY`. Engines that accept it only
    /// after `ORDER BY` name the order to use when the query has none.
    OffsetFetch { default_order: Option<&'static str> },
    /// `SELECT TOP n …`: a first page only, no offset.
    Top,
    /// No paging clause: the whole result, cut at the row limit of a result.
    None,
}

/// How a "contains" filter is written. The pattern is always escaped with
/// `sql::LIKE_ESCAPE` and followed by `ESCAPE '!'`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContainsFilter {
    /// Text around the quoted column that turns it into text, for engines
    /// whose `LIKE` takes text only: `("CAST(", " AS TEXT)")`.
    pub cast: Option<(&'static str, &'static str)>,
    /// `LIKE`, or the operator that ignores letter case where `LIKE` does not.
    pub operator: &'static str,
    /// Text around the pattern literal, for engines that ignore letter case
    /// only when both sides are folded: `("LOWER(", ")")` here and in `cast`.
    pub pattern: Option<(&'static str, &'static str)>,
}
impl ContainsFilter {
    /// `column LIKE pattern`: numbers are searched as text and letter case
    /// is ignored by the collation.
    pub const LIKE: ContainsFilter = ContainsFilter {
        cast: None,
        operator: "LIKE",
        pattern: None,
    };
}

/// Placeholders of a prepared statement. The application sends literals; a
/// driver that binds values, and the editor's reading of `$1`, use this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParameterStyle {
    /// `$1`, `$2`
    Dollar,
    /// `?`
    Question,
    /// `@p1`, `@p2`
    AtNamed,
    /// `:1`, `:2`
    Colon,
}

/// `INSERT` of a row that takes every default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmptyInsert {
    /// `INSERT INTO t DEFAULT VALUES`
    DefaultValues,
    /// `INSERT INTO t () VALUES ()`
    EmptyLists,
}

/// Schema the table designer proposes for a new table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultSchema {
    /// A fixed name such as `public`, `main` or `dbo`.
    Fixed(&'static str),
    /// The database of the profile.
    Database,
    /// The engine has no schemas.
    None,
}

/// Where a string literal, a quoted name or a comment ends: what the reader
/// of SQL text needs to split a script into statements and to recognise a
/// plain `SELECT * FROM table`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lexer {
    /// A backslash inside `'…'` and `"…"` escapes the next character.
    pub backslash_escapes: bool,
    /// `"…"` is a string, not a quoted name.
    pub double_quote_strings: bool,
    /// `` `…` `` quotes a name.
    pub backtick_names: bool,
    /// `[…]` quotes a name.
    pub bracket_names: bool,
    /// `$tag$ … $tag$` is a string body and `$` may continue a name.
    pub dollar_quotes: bool,
    /// `E'…'` is a string in which a backslash escapes.
    pub escape_strings: bool,
    /// `/* … /* … */ … */` nests.
    pub nested_comments: bool,
    /// `#` starts a comment that runs to the end of the line.
    pub hash_comments: bool,
    /// `--` starts a comment only before white space.
    pub dashes_need_space: bool,
}
impl Lexer {
    /// Standard SQL: `'…'` strings, `"…"` names, `--` and `/* */` comments.
    pub const STANDARD: Lexer = Lexer {
        backslash_escapes: false,
        double_quote_strings: false,
        backtick_names: false,
        bracket_names: false,
        dollar_quotes: false,
        escape_strings: false,
        nested_comments: false,
        hash_comments: false,
        dashes_need_space: false,
    };
}

/// How an engine writes SQL: everything the generated statements and the
/// reader of SQL text depend on. A driver describes its dialect as data; no
/// code outside the driver asks which engine it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dialect {
    /// Opening and closing quote of a name; a closing quote inside a name is
    /// doubled.
    pub name_quotes: (char, char),
    pub text: TextLiteral,
    /// Text before and after the hex digits of a binary value: `("X'", "'")`.
    pub bytes: (&'static str, &'static str),
    /// Literals for true and false.
    pub booleans: (&'static str, &'static str),
    pub paging: Paging,
    pub contains: ContainsFilter,
    pub parameters: ParameterStyle,
    pub lexer: Lexer,
    pub empty_insert: EmptyInsert,
    pub default_schema: DefaultSchema,
    /// Column types the table designer offers; the first is the type of a
    /// new key column.
    pub column_types: &'static [&'static str],
}
impl Dialect {
    /// Standard SQL, for an engine that needs nothing special and as the
    /// base of a dialect written with `..Dialect::ANSI`.
    pub const ANSI: Dialect = Dialect {
        name_quotes: ('"', '"'),
        text: TextLiteral::Standard,
        bytes: ("X'", "'"),
        booleans: ("TRUE", "FALSE"),
        paging: Paging::LimitOffset,
        contains: ContainsFilter::LIKE,
        parameters: ParameterStyle::Question,
        lexer: Lexer::STANDARD,
        empty_insert: EmptyInsert::DefaultValues,
        default_schema: DefaultSchema::None,
        column_types: &["INTEGER", "TEXT"],
    };
}

/// Engine options of a profile: a small map from the key of an
/// [`OptionField`] to its value, kept in the order of the keys so that a
/// profile always serialises the same way.
pub type EngineOptions = std::collections::BTreeMap<String, String>;

/// Most options a profile may hold, and the longest key and value.
pub const MAX_OPTIONS: usize = 32;
pub const MAX_OPTION_KEY: usize = 48;
pub const MAX_OPTION_VALUE: usize = 2048;

impl ConnectionProfile {
    /// The database is a local file, not a server.
    pub fn is_file(&self) -> bool {
        self.engine.spec().form.address == Address::File
    }
    /// The product this profile was saved for.
    pub fn product_spec(&self) -> &'static ProductSpec {
        self.engine.product(&self.product)
    }
    /// Name of the product for people: `Amazon Aurora PostgreSQL`.
    pub fn product_label(&self) -> &'static str {
        self.product_spec().label
    }
    /// What the interface may offer for this profile before it is connected.
    pub fn capabilities(&self) -> &'static Capabilities {
        self.engine.spec().capabilities_of(self.product_spec())
    }
    /// The SQL dialect of this profile before it is connected.
    pub fn dialect(&self) -> &'static Dialect {
        self.engine.spec().dialect_of(self.product_spec())
    }
    /// Value of an engine option: the stored one, otherwise the default the
    /// spec declares, otherwise an empty text.
    pub fn option(&self, key: &str) -> &str {
        match self.options.get(key) {
            Some(value) => value,
            None => self
                .engine
                .spec()
                .option(key)
                .map_or("", |field| field.default),
        }
    }
    /// Store an engine option. A value equal to the declared default is
    /// removed instead, so that an untouched dialog stores nothing.
    pub fn set_option(&mut self, key: &str, value: &str) {
        let default = self
            .engine
            .spec()
            .option(key)
            .map_or("", |field| field.default);
        if value == default {
            self.options.remove(key);
        } else {
            self.options.insert(key.to_owned(), value.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::{mysql, postgres, sqlite};
    use crate::model::ProfileSet;

    #[test]
    fn the_registry_is_consistent() {
        let mut engines = std::collections::HashSet::new();
        let mut products = std::collections::HashSet::new();
        for engine in Engine::all() {
            let spec = engine.spec();
            assert!(engines.insert(spec.id), "engine {} twice", spec.id);
            assert!(spec.available && !spec.label.is_empty());
            assert!(!spec.products.is_empty(), "{} has no product", spec.id);
            assert_eq!(Engine::from_id(spec.id), Some(engine));
            assert_eq!(Engine::from_id(spec.portable_name), Some(engine));
            for alias in spec.aliases {
                assert_eq!(Engine::from_id(&alias.to_uppercase()), Some(engine));
            }
            for product in spec.products {
                assert!(products.insert(product.id), "product {} twice", product.id);
                assert!(
                    !product.id.is_empty()
                        && product
                            .id
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                    "{}",
                    product.id
                );
                assert!(!product.label.is_empty());
                assert!((1..=2).contains(&product.badge.text.chars().count()));
                assert_eq!(
                    product.default_port == 0,
                    spec.form.address == Address::File,
                    "{}",
                    product.id
                );
                assert_eq!(
                    Engine::of_product(product.id).map(|found| found.0),
                    Some(engine)
                );
            }
            // Something of every family can be picked for a new connection.
            assert!(spec.products.iter().any(|product| product.listed));
            for option in spec.options {
                assert!(
                    option
                        .key
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                );
                assert!(option.accepts(option.default) || option.required);
            }
        }
        // Every SQL dialect reads back what it writes: the first page of a
        // table with an awkward name is one statement on that table, so its
        // rows can be edited. A lexer that does not know the quotes of its
        // own dialect fails here.
        let awkward = crate::model::TableRef {
            schema: "s".into(),
            name: "Order Items; -- x".into(),
            kind: "table".into(),
        };
        for engine in Engine::all() {
            let spec = engine.spec();
            for product in spec.products {
                if spec.capabilities_of(product).language != QueryLanguage::Sql {
                    continue;
                }
                let dialect = spec.dialect_of(product);
                let page = crate::drivers::PageRequest::first(awkward.clone());
                let sql = crate::builder::QueryBuilder::page(&page)
                    .sql(dialect)
                    .unwrap();
                assert_eq!(
                    crate::sql::statements(dialect, &sql).unwrap().len(),
                    1,
                    "{}: {sql}",
                    product.id
                );
                assert!(
                    crate::sql::editable_table(dialect, &sql, std::slice::from_ref(&awkward))
                        .is_some(),
                    "{}: {sql}",
                    product.id
                );
                assert!(!dialect.column_types.is_empty(), "{}", product.id);
            }
        }
        // A new connection starts as PostgreSQL with its defaults, without TLS
        // (the owner's choice of 2026-10-13: most servers are reached on a
        // private network or through SSH, often with a self-signed certificate).
        let fresh = ConnectionProfile::default();
        assert_eq!(fresh.engine, postgres::ENGINE);
        assert_eq!((fresh.port, fresh.tls), (5432, TlsMode::Disable));
        // Every installed database starts without TLS; a cloud service that only
        // accepts encrypted connections keeps an encrypted mode by default.
        for (id, expected) in [
            ("mysql", TlsMode::Disable),
            ("mariadb", TlsMode::Disable),
            ("sql-server", TlsMode::Disable),
            ("mongodb", TlsMode::Disable),
            ("amazon-rds-postgresql", TlsMode::Require),
            ("google-cloud-sql-mysql", TlsMode::Require),
            ("azure-sql-database", TlsMode::VerifyFull),
            ("mongodb-atlas", TlsMode::VerifyFull),
            ("upstash", TlsMode::VerifyFull),
        ] {
            let product = crate::drivers::REGISTRY
                .iter()
                .flat_map(|spec| spec.products.iter())
                .find(|product| product.id == id)
                .unwrap_or_else(|| panic!("{id} is registered"));
            assert_eq!(product.default_tls, expected, "{id}");
        }
        assert!(fresh.database.is_empty() && fresh.username.is_empty());
        assert!(fresh.product.is_empty() && fresh.options.is_empty());
        // The stored names of the first three engines never change.
        assert_eq!(postgres::ENGINE.id(), "postgres");
        assert_eq!(mysql::ENGINE.id(), "mysql");
        assert_eq!(sqlite::ENGINE.id(), "sqlite");
        assert_eq!(
            serde_json::to_string(&[postgres::ENGINE, mysql::ENGINE, sqlite::ENGINE]).unwrap(),
            r#"["postgres","mysql","sqlite"]"#
        );
    }

    #[test]
    fn products_fall_back_to_the_generic_one_and_are_found_by_search() {
        let spec = postgres::ENGINE.spec();
        assert_eq!(postgres::ENGINE.product("").id, "postgresql");
        assert_eq!(postgres::ENGINE.product("made-up").id, "postgresql");
        let aurora = postgres::ENGINE.product("amazon-aurora-postgresql");
        assert_eq!(aurora.label, "Amazon Aurora PostgreSQL");
        assert_eq!(spec.stored_product(aurora), "amazon-aurora-postgresql");
        assert_eq!(spec.stored_product(spec.generic()), "");
        assert!(aurora.matches("aurora") && aurora.matches("AWS post") && !aurora.matches("mysql"));
        // The product of earlier MySQL and MariaDB profiles keeps its name
        // and is not offered for new ones.
        let legacy = mysql::ENGINE.spec().generic();
        assert_eq!(legacy.label, "MariaDB / MySQL");
        assert!(!legacy.listed);
        // Redshift writes SQL differently from the family.
        let redshift = postgres::ENGINE.product("amazon-redshift");
        assert_eq!(redshift.default_port, 5439);
        assert_ne!(spec.dialect_of(redshift), &spec.dialect);
        assert_eq!(spec.dialect_of(aurora), &spec.dialect);
        assert_eq!(spec.capabilities_of(aurora), &Capabilities::SQL);
    }

    /// A profile as every version before products wrote it.
    const OLD_PROFILE: &str = r#"{"id":"6f2c1a3e-8c1f-4c7e-9a55-0d3f2b6a9c10","name":"Shop","engine":"mysql","host":"db.example.test","port":3306,"database":"shop","username":"app","password":"OWNED-FAKE","tls":"verify_full","ca_file":null,"sqlite_path":null,"color":[71,155,230],"ssh":null}"#;

    #[test]
    fn profiles_of_earlier_versions_load_and_are_written_back_byte_for_byte() {
        let profile: ConnectionProfile = serde_json::from_str(OLD_PROFILE).unwrap();
        assert_eq!(profile.engine, mysql::ENGINE);
        assert!(profile.product.is_empty() && profile.options.is_empty());
        assert_eq!(profile.product_label(), "MariaDB / MySQL");
        profile.validate().unwrap();
        assert_eq!(serde_json::to_string(&profile).unwrap(), OLD_PROFILE);
        let set = format!(
            r#"{{"version":1,"connections":[{OLD_PROFILE}],"tunnels":[],"preferences":{{"language":"pl","theme":"vscode","font_size":12.0,"local_sql_history":false}}}}"#
        );
        let loaded: ProfileSet = serde_json::from_str(&set).unwrap();
        assert_eq!(loaded.connections[0].engine, mysql::ENGINE);
    }

    #[test]
    fn product_and_options_are_stored_only_when_set() {
        let mut profile: ConnectionProfile = serde_json::from_str(OLD_PROFILE).unwrap();
        profile.product = "tidb".into();
        profile.options.insert("zone".into(), "b".into());
        profile.options.insert("account".into(), "a".into());
        let text = serde_json::to_string(&profile).unwrap();
        // Appended after the fields of earlier versions, keys in order.
        assert!(
            text.ends_with(r#""ssh":null,"product":"tidb","options":{"account":"a","zone":"b"}}"#)
        );
        let back: ConnectionProfile = serde_json::from_str(&text).unwrap();
        assert_eq!(back.product_label(), "TiDB");
        assert_eq!(back.options, profile.options);
        back.validate().unwrap();
        // What a profile may not hold.
        profile.product = "Not An Id".into();
        assert_eq!(
            profile.validate().unwrap_err().to_string(),
            "connection_product_invalid"
        );
        profile.product.clear();
        profile.options.insert("Bad Key".into(), String::new());
        assert_eq!(
            profile.validate().unwrap_err().to_string(),
            "connection_option_invalid"
        );
    }

    #[test]
    fn a_profile_of_an_engine_without_a_driver_survives_a_round_trip() {
        let text = OLD_PROFILE.replace(
            r#""engine":"mysql""#,
            r#""engine":"engine_of_a_later_build""#,
        );
        let profile: ConnectionProfile = serde_json::from_str(&text).unwrap();
        assert!(!profile.engine.spec().available);
        assert_eq!(profile.engine.label(), "engine_of_a_later_build");
        assert_eq!(profile.capabilities(), &Capabilities::NONE);
        // It stays in the vault as it is and is saved back unchanged.
        profile.validate().unwrap();
        assert_eq!(serde_json::to_string(&profile).unwrap(), text);
        assert_eq!(Engine::from_id("engine_of_a_later_build"), None);
        // Anything that is not a plain name is refused.
        for bad in ["", "Has Space", "ąę", &"x".repeat(41)] {
            let text = OLD_PROFILE.replace(r#""engine":"mysql""#, &format!(r#""engine":"{bad}""#));
            assert!(serde_json::from_str::<ConnectionProfile>(&text).is_err());
        }
    }

    #[test]
    fn options_read_their_defaults_and_store_only_what_differs() {
        static FIELDS: &[OptionField] = &[
            OptionField {
                key: "database_index",
                label: ("Numer bazy", "Database index"),
                kind: OptionKind::Number { min: 0, max: 15 },
                default: "0",
                placeholder: "",
                help: None,
                required: false,
            },
            OptionField {
                key: "mode",
                label: ("Tryb", "Mode"),
                kind: OptionKind::Choice(&["single", "cluster"]),
                default: "single",
                placeholder: "",
                help: None,
                required: false,
            },
        ];
        assert!(FIELDS[0].accepts("15") && !FIELDS[0].accepts("16") && !FIELDS[0].accepts("x"));
        assert!(FIELDS[1].accepts("cluster") && !FIELDS[1].accepts("other"));
        let flag = OptionField {
            kind: OptionKind::Flag,
            default: "false",
            ..FIELDS[0]
        };
        assert!(flag.accepts("true") && !flag.accepts("yes"));
        let text = OptionField {
            kind: OptionKind::Text,
            default: "",
            required: true,
            ..FIELDS[0]
        };
        assert!(text.accepts("FREEPDB1") && !text.accepts("") && !text.accepts("a\0b"));
        // An engine without declared options: nothing has a default.
        let mut profile = ConnectionProfile::default();
        assert_eq!(profile.option("anything"), "");
        profile.set_option("anything", "x");
        assert_eq!(profile.option("anything"), "x");
        profile.set_option("anything", "");
        assert!(profile.options.is_empty());
    }
}
