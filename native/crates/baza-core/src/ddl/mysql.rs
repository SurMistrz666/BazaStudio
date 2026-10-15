//! The parts of a MySQL or MariaDB table beyond its columns, as the table
//! designer edits them: indexes, foreign keys, CHECK constraints, triggers,
//! the options of the table with the settings of its engine, and its
//! partitions. Each is read from `information_schema`, written into `CREATE
//! TABLE`, and turned into the statements that change a table from what it
//! has into what the designer shows.
use super::{DdlFlavor, FieldDraft, alter_table, create_table_with};
use crate::engine::Dialect;
use crate::model::{Cell, TableRef};
use crate::sql::{literal, qualified, quote_identifier};
use anyhow::{Result, ensure};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IndexKind {
    #[default]
    Index,
    Unique,
    Fulltext,
    Spatial,
}
impl IndexKind {
    pub const ALL: [IndexKind; 4] = [
        IndexKind::Index,
        IndexKind::Unique,
        IndexKind::Fulltext,
        IndexKind::Spatial,
    ];
    pub fn sql(self) -> &'static str {
        match self {
            IndexKind::Index => "INDEX",
            IndexKind::Unique => "UNIQUE INDEX",
            IndexKind::Fulltext => "FULLTEXT INDEX",
            IndexKind::Spatial => "SPATIAL INDEX",
        }
    }
}

/// One column of an index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexPart {
    pub column: String,
    /// Characters of a text column the index takes; empty for all of them.
    pub length: String,
    pub descending: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexDraft {
    pub name: String,
    pub kind: IndexKind,
    pub parts: Vec<IndexPart>,
    pub comment: String,
    /// The name the index has in the database; `None` for a new one.
    pub original: Option<String>,
}

/// What a foreign key does with the rows that refer to a changed one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RefAction {
    #[default]
    Restrict,
    Cascade,
    SetNull,
    NoAction,
    SetDefault,
}
impl RefAction {
    pub const ALL: [RefAction; 5] = [
        RefAction::Restrict,
        RefAction::Cascade,
        RefAction::SetNull,
        RefAction::NoAction,
        RefAction::SetDefault,
    ];
    pub fn sql(self) -> &'static str {
        match self {
            RefAction::Restrict => "RESTRICT",
            RefAction::Cascade => "CASCADE",
            RefAction::SetNull => "SET NULL",
            RefAction::NoAction => "NO ACTION",
            RefAction::SetDefault => "SET DEFAULT",
        }
    }
    fn parse(text: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|action| action.sql().eq_ignore_ascii_case(text.trim()))
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForeignKeyDraft {
    pub name: String,
    pub columns: Vec<String>,
    /// Empty for the schema of the table itself.
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_delete: RefAction,
    pub on_update: RefAction,
    pub original: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckDraft {
    pub name: String,
    /// The condition, as SQL.
    pub expression: String,
    pub original: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Timing {
    #[default]
    Before,
    After,
}
impl Timing {
    pub const ALL: [Timing; 2] = [Timing::Before, Timing::After];
    pub fn sql(self) -> &'static str {
        match self {
            Timing::Before => "BEFORE",
            Timing::After => "AFTER",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Event {
    #[default]
    Insert,
    Update,
    Delete,
}
impl Event {
    pub const ALL: [Event; 3] = [Event::Insert, Event::Update, Event::Delete];
    pub fn sql(self) -> &'static str {
        match self {
            Event::Insert => "INSERT",
            Event::Update => "UPDATE",
            Event::Delete => "DELETE",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TriggerDraft {
    pub name: String,
    pub timing: Timing,
    pub event: Event,
    /// What runs for each row: one statement, or `BEGIN … END`.
    pub body: String,
    pub original: Option<String>,
}

/// The options of a table. Empty means "as the server chooses".
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableOptions {
    pub engine: String,
    pub charset: String,
    pub collation: String,
    pub auto_increment: String,
    pub comment: String,
    /// The settings of the engine by their key, as [`engine_options`] lists
    /// them: `ROW_FORMAT`, `PAGE_COMPRESSED`, `TRANSACTIONAL`, …
    pub settings: BTreeMap<String, String>,
}

/// What a setting of an engine takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingValues {
    Choice(&'static [&'static str]),
    /// A whole number up to this one.
    Number(u64),
}

/// A setting of a table that one engine has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineSetting {
    pub key: &'static str,
    /// Polish and English.
    pub label: (&'static str, &'static str),
    pub values: SettingValues,
}

const fn setting(
    key: &'static str,
    label: (&'static str, &'static str),
    values: SettingValues,
) -> EngineSetting {
    EngineSetting { key, label, values }
}

const SWITCH: SettingValues = SettingValues::Choice(&["0", "1"]);
const DEFAULT_SWITCH: SettingValues = SettingValues::Choice(&["DEFAULT", "0", "1"]);

const INNODB: &[EngineSetting] = &[
    setting(
        "ROW_FORMAT",
        ("Format wierszy", "Row format"),
        SettingValues::Choice(&["DEFAULT", "DYNAMIC", "COMPACT", "REDUNDANT", "COMPRESSED"]),
    ),
    setting(
        "KEY_BLOCK_SIZE",
        (
            "Rozmiar bloku przy kompresji (KB)",
            "Block size when compressed (KB)",
        ),
        SettingValues::Choice(&["0", "1", "2", "4", "8", "16"]),
    ),
    setting(
        "PAGE_COMPRESSED",
        ("Kompresja stron (MariaDB)", "Page compression (MariaDB)"),
        SWITCH,
    ),
    setting(
        "PAGE_COMPRESSION_LEVEL",
        (
            "Poziom kompresji stron (MariaDB)",
            "Page compression level (MariaDB)",
        ),
        SettingValues::Number(9),
    ),
    setting(
        "ENCRYPTED",
        ("Szyfrowanie (MariaDB)", "Encryption (MariaDB)"),
        SettingValues::Choice(&["DEFAULT", "YES", "NO"]),
    ),
    setting(
        "STATS_PERSISTENT",
        ("Trwałe statystyki", "Persistent statistics"),
        DEFAULT_SWITCH,
    ),
    setting(
        "STATS_AUTO_RECALC",
        ("Samo przeliczaj statystyki", "Recalculate statistics"),
        DEFAULT_SWITCH,
    ),
    setting(
        "STATS_SAMPLE_PAGES",
        ("Stron próbki statystyk", "Statistics sample pages"),
        SettingValues::Number(65535),
    ),
];
const ARIA: &[EngineSetting] = &[
    setting(
        "ROW_FORMAT",
        ("Format wierszy", "Row format"),
        SettingValues::Choice(&["PAGE", "FIXED", "DYNAMIC"]),
    ),
    setting("TRANSACTIONAL", ("Odporna na awarie", "Crash-safe"), SWITCH),
    setting(
        "PAGE_CHECKSUM",
        ("Suma kontrolna stron", "Page checksum"),
        SWITCH,
    ),
    setting(
        "CHECKSUM",
        ("Suma kontrolna tabeli", "Table checksum"),
        SWITCH,
    ),
    setting(
        "MAX_ROWS",
        ("Najwięcej wierszy", "Most rows"),
        SettingValues::Number(u32::MAX as u64),
    ),
    setting(
        "AVG_ROW_LENGTH",
        ("Średnia długość wiersza", "Average row length"),
        SettingValues::Number(u32::MAX as u64),
    ),
];
const MYISAM: &[EngineSetting] = &[
    setting(
        "ROW_FORMAT",
        ("Format wierszy", "Row format"),
        SettingValues::Choice(&["DEFAULT", "FIXED", "DYNAMIC"]),
    ),
    setting(
        "CHECKSUM",
        ("Suma kontrolna tabeli", "Table checksum"),
        SWITCH,
    ),
    setting(
        "DELAY_KEY_WRITE",
        ("Opóźniony zapis indeksów", "Delayed key write"),
        SWITCH,
    ),
    setting(
        "PACK_KEYS",
        ("Pakowanie indeksów", "Pack keys"),
        DEFAULT_SWITCH,
    ),
    setting(
        "MAX_ROWS",
        ("Najwięcej wierszy", "Most rows"),
        SettingValues::Number(u32::MAX as u64),
    ),
    setting(
        "AVG_ROW_LENGTH",
        ("Średnia długość wiersza", "Average row length"),
        SettingValues::Number(u32::MAX as u64),
    ),
];
const MEMORY: &[EngineSetting] = &[
    setting(
        "MAX_ROWS",
        ("Najwięcej wierszy", "Most rows"),
        SettingValues::Number(u32::MAX as u64),
    ),
    setting(
        "MIN_ROWS",
        ("Najmniej wierszy", "Fewest rows"),
        SettingValues::Number(u32::MAX as u64),
    ),
];

/// The settings an engine has; none for one this list does not know.
pub fn engine_options(engine: &str) -> &'static [EngineSetting] {
    match engine.to_ascii_uppercase().as_str() {
        "INNODB" => INNODB,
        "ARIA" => ARIA,
        "MYISAM" => MYISAM,
        "MEMORY" | "HEAP" => MEMORY,
        _ => &[],
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PartitionMethod {
    #[default]
    None,
    Range,
    RangeColumns,
    List,
    ListColumns,
    Hash,
    LinearHash,
    Key,
    LinearKey,
}
impl PartitionMethod {
    pub const ALL: [PartitionMethod; 9] = [
        PartitionMethod::None,
        PartitionMethod::Range,
        PartitionMethod::RangeColumns,
        PartitionMethod::List,
        PartitionMethod::ListColumns,
        PartitionMethod::Hash,
        PartitionMethod::LinearHash,
        PartitionMethod::Key,
        PartitionMethod::LinearKey,
    ];
    pub fn sql(self) -> &'static str {
        match self {
            PartitionMethod::None => "",
            PartitionMethod::Range => "RANGE",
            PartitionMethod::RangeColumns => "RANGE COLUMNS",
            PartitionMethod::List => "LIST",
            PartitionMethod::ListColumns => "LIST COLUMNS",
            PartitionMethod::Hash => "HASH",
            PartitionMethod::LinearHash => "LINEAR HASH",
            PartitionMethod::Key => "KEY",
            PartitionMethod::LinearKey => "LINEAR KEY",
        }
    }
    /// Each partition names the values it holds.
    pub fn listed(self) -> bool {
        matches!(
            self,
            PartitionMethod::Range
                | PartitionMethod::RangeColumns
                | PartitionMethod::List
                | PartitionMethod::ListColumns
        )
    }
    fn parse(text: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|method| method.sql().eq_ignore_ascii_case(text.trim()))
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartitionDef {
    pub name: String,
    /// What follows `VALUES LESS THAN` or `VALUES IN`, without the brackets:
    /// `100`, `'a', 'b'`, or `MAXVALUE` for the last range.
    pub values: String,
    pub comment: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Partitioning {
    pub method: PartitionMethod,
    /// The expression (`RANGE`, `LIST`, `HASH`) or the columns (`… COLUMNS`,
    /// `KEY`, empty for the primary key).
    pub expression: String,
    /// Partitions of `HASH` and `KEY` that are not listed one by one.
    pub count: String,
    pub partitions: Vec<PartitionDef>,
}

/// Everything of a table but its columns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableExtras {
    pub indexes: Vec<IndexDraft>,
    pub foreign_keys: Vec<ForeignKeyDraft>,
    pub checks: Vec<CheckDraft>,
    pub triggers: Vec<TriggerDraft>,
    pub options: TableOptions,
    pub partitioning: Partitioning,
}

/// SQL typed in the designer: one expression or value list, no statement or
/// comment after it.
fn expression(text: &str, code: &'static str) -> Result<String> {
    let text = text.trim();
    ensure!(
        !text.is_empty()
            && text.len() <= 8000
            && !text.contains(';')
            && !text.contains("--")
            && !text.contains("/*")
            && !text.contains('#'),
        code
    );
    Ok(text.to_owned())
}

/// The name of an engine, a character set or a collation.
fn word(text: &str, code: &'static str) -> Result<String> {
    let text = text.trim();
    ensure!(
        !text.is_empty()
            && text.len() <= 64
            && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        code
    );
    Ok(text.to_owned())
}

fn text_literal(dialect: &Dialect, text: &str) -> Result<String> {
    literal(dialect, &Cell::Text(text.to_owned()))
}

impl IndexDraft {
    /// `UNIQUE INDEX name (a(10), b DESC) COMMENT '…'`.
    fn definition(&self, dialect: &Dialect) -> Result<String> {
        ensure!(
            !self.name.trim().is_empty() && !self.parts.is_empty() && self.parts.len() <= 64,
            "index_invalid"
        );
        let parts = self
            .parts
            .iter()
            .map(|part| {
                let mut text = quote_identifier(dialect, part.column.trim())?;
                let length = part.length.trim();
                if !length.is_empty() {
                    ensure!(
                        length.len() <= 6 && length.chars().all(|c| c.is_ascii_digit()),
                        "index_invalid"
                    );
                    text.push_str(&format!("({length})"));
                }
                if part.descending {
                    text.push_str(" DESC");
                }
                Ok(text)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut text = format!(
            "{} {} ({})",
            self.kind.sql(),
            quote_identifier(dialect, self.name.trim())?,
            parts.join(", ")
        );
        if !self.comment.trim().is_empty() {
            text.push_str(&format!(
                " COMMENT {}",
                super::mysql_comment(self.comment.trim())?
            ));
        }
        Ok(text)
    }
}

impl ForeignKeyDraft {
    /// `CONSTRAINT name FOREIGN KEY (a) REFERENCES t (b) ON DELETE … ON UPDATE …`.
    fn definition(&self, dialect: &Dialect, table: &TableRef) -> Result<String> {
        ensure!(
            !self.name.trim().is_empty()
                && !self.columns.is_empty()
                && self.columns.len() == self.ref_columns.len()
                && !self.ref_table.trim().is_empty(),
            "foreign_key_invalid"
        );
        let names = |columns: &[String]| {
            columns
                .iter()
                .map(|c| quote_identifier(dialect, c.trim()))
                .collect::<Result<Vec<_>>>()
                .map(|names| names.join(", "))
        };
        let target = TableRef {
            schema: if self.ref_schema.trim().is_empty() {
                table.schema.clone()
            } else {
                self.ref_schema.trim().to_owned()
            },
            name: self.ref_table.trim().to_owned(),
            kind: "table".into(),
        };
        Ok(format!(
            "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) ON DELETE {} ON UPDATE {}",
            quote_identifier(dialect, self.name.trim())?,
            names(&self.columns)?,
            qualified(dialect, &target)?,
            names(&self.ref_columns)?,
            self.on_delete.sql(),
            self.on_update.sql()
        ))
    }
}

impl CheckDraft {
    /// `CONSTRAINT name CHECK (expression)`.
    fn definition(&self, dialect: &Dialect) -> Result<String> {
        ensure!(!self.name.trim().is_empty(), "check_invalid");
        Ok(format!(
            "CONSTRAINT {} CHECK ({})",
            quote_identifier(dialect, self.name.trim())?,
            expression(&self.expression, "check_invalid")?
        ))
    }
}

impl TriggerDraft {
    fn qualified_name(&self, dialect: &Dialect, table: &TableRef) -> Result<String> {
        qualified(
            dialect,
            &TableRef {
                schema: table.schema.clone(),
                name: self.name.trim().to_owned(),
                kind: "trigger".into(),
            },
        )
    }
    fn create(&self, dialect: &Dialect, table: &TableRef) -> Result<String> {
        let body = self.body.trim();
        ensure!(
            !self.name.trim().is_empty()
                && !body.is_empty()
                && body.len() <= 65535
                && !body.to_ascii_uppercase().contains("DELIMITER"),
            "trigger_invalid"
        );
        Ok(format!(
            "CREATE TRIGGER {} {} {} ON {} FOR EACH ROW {body}",
            self.qualified_name(dialect, table)?,
            self.timing.sql(),
            self.event.sql(),
            qualified(dialect, table)?
        ))
    }
    fn drop(&self, dialect: &Dialect, table: &TableRef) -> Result<String> {
        Ok(format!(
            "DROP TRIGGER IF EXISTS {}",
            self.qualified_name(dialect, table)?
        ))
    }
}

impl TableOptions {
    /// One option as SQL, checked.
    fn setting_sql(engine: &str, key: &str, value: &str) -> Result<Option<String>> {
        let Some(setting) = engine_options(engine).iter().find(|s| s.key == key) else {
            // A setting of another engine is not written.
            return Ok(None);
        };
        let value = value.trim();
        let valid = match setting.values {
            SettingValues::Choice(choices) => choices.iter().any(|c| c.eq_ignore_ascii_case(value)),
            SettingValues::Number(max) => value.parse::<u64>().is_ok_and(|n| n <= max),
        };
        ensure!(valid, "table_option_invalid");
        Ok(Some(format!("{key}={value}")))
    }

    /// The options that differ from `before`, or all that are set when there
    /// is nothing before.
    fn sql(&self, before: Option<&TableOptions>) -> Result<Vec<String>> {
        let empty = TableOptions::default();
        let old = before.unwrap_or(&empty);
        let mut parts = Vec::new();
        if self.engine.trim() != old.engine.trim() && !self.engine.trim().is_empty() {
            parts.push(format!(
                "ENGINE={}",
                word(&self.engine, "table_option_invalid")?
            ));
        }
        if (self.charset.trim() != old.charset.trim()
            || self.collation.trim() != old.collation.trim())
            && (!self.charset.trim().is_empty() || !self.collation.trim().is_empty())
        {
            if !self.charset.trim().is_empty() {
                parts.push(format!(
                    "DEFAULT CHARSET={}",
                    word(&self.charset, "table_option_invalid")?
                ));
            }
            if !self.collation.trim().is_empty() {
                parts.push(format!(
                    "COLLATE={}",
                    word(&self.collation, "table_option_invalid")?
                ));
            }
        }
        let increment = self.auto_increment.trim();
        if increment != old.auto_increment.trim() && !increment.is_empty() {
            ensure!(increment.parse::<u64>().is_ok(), "table_option_invalid");
            parts.push(format!("AUTO_INCREMENT={increment}"));
        }
        if self.comment.trim() != old.comment.trim() {
            parts.push(format!(
                "COMMENT={}",
                super::mysql_comment(self.comment.trim())?
            ));
        }
        for setting in engine_options(&self.engine) {
            let now = self.settings.get(setting.key).map_or("", |v| v.trim());
            let was = old.settings.get(setting.key).map_or("", |v| v.trim());
            if now == was {
                continue;
            }
            if now.is_empty() {
                // A setting taken away goes back to what the server chooses.
                parts.push(format!("{}=DEFAULT", setting.key));
            } else if let Some(text) = Self::setting_sql(&self.engine, setting.key, now)? {
                parts.push(text);
            }
        }
        Ok(parts)
    }
}

impl Partitioning {
    /// `PARTITION BY …`, or `None` without partitions.
    fn clause(&self, dialect: &Dialect) -> Result<Option<String>> {
        let method = self.method;
        if method == PartitionMethod::None {
            return Ok(None);
        }
        let subject = match method {
            // KEY() partitions by the primary key.
            PartitionMethod::Key | PartitionMethod::LinearKey
                if self.expression.trim().is_empty() =>
            {
                String::new()
            }
            _ => expression(&self.expression, "partition_invalid")?,
        };
        let mut text = format!("PARTITION BY {} ({subject})", method.sql());
        if method.listed() {
            ensure!(!self.partitions.is_empty(), "partition_invalid");
        }
        if self.partitions.is_empty() {
            let count = self.count.trim();
            ensure!(
                count.parse::<u32>().is_ok_and(|n| (1..=8192).contains(&n)),
                "partition_invalid"
            );
            text.push_str(&format!(" PARTITIONS {count}"));
            return Ok(Some(text));
        }
        let definitions = self
            .partitions
            .iter()
            .map(|partition| {
                let mut line = format!(
                    "PARTITION {}",
                    quote_identifier(dialect, partition.name.trim())?
                );
                match method {
                    PartitionMethod::Range
                        if partition.values.trim().eq_ignore_ascii_case("MAXVALUE") =>
                    {
                        line.push_str(" VALUES LESS THAN MAXVALUE");
                    }
                    PartitionMethod::Range | PartitionMethod::RangeColumns => {
                        line.push_str(&format!(
                            " VALUES LESS THAN ({})",
                            expression(&partition.values, "partition_invalid")?
                        ))
                    }
                    PartitionMethod::List | PartitionMethod::ListColumns => {
                        line.push_str(&format!(
                            " VALUES IN ({})",
                            expression(&partition.values, "partition_invalid")?
                        ))
                    }
                    _ => {}
                }
                if !partition.comment.trim().is_empty() {
                    line.push_str(&format!(
                        " COMMENT = {}",
                        super::mysql_comment(partition.comment.trim())?
                    ));
                }
                Ok(line)
            })
            .collect::<Result<Vec<_>>>()?;
        text.push_str(&format!(" (\n  {}\n)", definitions.join(",\n  ")));
        Ok(Some(text))
    }
}

/// Names must not repeat within one kind of thing.
fn unique<'a>(names: impl Iterator<Item = &'a str>, code: &'static str) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for name in names {
        ensure!(seen.insert(name.trim().to_lowercase()), code);
    }
    Ok(())
}

impl TableExtras {
    fn check_names(&self) -> Result<()> {
        // Only InnoDB (and NDB of MySQL Cluster) keeps foreign keys; the
        // server refuses the change of engine with an error of its own.
        let engine = self.options.engine.trim();
        ensure!(
            self.foreign_keys.is_empty()
                || engine.is_empty()
                || ["innodb", "ndb", "ndbcluster"].contains(&engine.to_ascii_lowercase().as_str()),
            "engine_foreign_keys_unsupported"
        );
        unique(
            self.indexes.iter().map(|i| i.name.as_str()),
            "index_name_duplicate",
        )?;
        unique(
            self.foreign_keys
                .iter()
                .map(|f| f.name.as_str())
                .chain(self.checks.iter().map(|c| c.name.as_str())),
            "constraint_name_duplicate",
        )?;
        unique(
            self.triggers.iter().map(|t| t.name.as_str()),
            "trigger_name_duplicate",
        )
    }
}

/// The statements that create a new table with its columns and the rest.
pub fn create(
    dialect: &Dialect,
    table: &TableRef,
    fields: &[FieldDraft],
    extras: &TableExtras,
) -> Result<Vec<String>> {
    extras.check_names()?;
    let mut more = Vec::new();
    for index in &extras.indexes {
        more.push(index.definition(dialect)?);
    }
    for check in &extras.checks {
        more.push(check.definition(dialect)?);
    }
    for key in &extras.foreign_keys {
        more.push(key.definition(dialect, table)?);
    }
    let mut after = String::new();
    let options = extras.options.sql(None)?;
    if !options.is_empty() {
        after.push_str(&format!("\n{}", options.join(" ")));
    }
    if let Some(partitions) = extras.partitioning.clause(dialect)? {
        after.push_str(&format!("\n{partitions}"));
    }
    let mut statements =
        create_table_with(DdlFlavor::MySql, dialect, table, fields, &more, &after)?;
    for trigger in &extras.triggers {
        statements.push(trigger.create(dialect, table)?);
    }
    Ok(statements)
}

/// The parts of `before` that are gone or changed in `after`, and the parts
/// of `after` that are new or changed: what to drop and what to add. A part
/// is matched by its `original` name.
fn changes<'a, T: PartialEq>(
    before: &'a [T],
    after: &'a [T],
    name: impl Fn(&T) -> &str,
    original: impl Fn(&T) -> Option<&str>,
) -> (Vec<&'a T>, Vec<&'a T>) {
    let mut dropped = Vec::new();
    let mut added = Vec::new();
    for old in before {
        match after.iter().find(|new| original(new) == Some(name(old))) {
            Some(new) if new == old => {}
            Some(new) => {
                dropped.push(old);
                added.push(new);
            }
            None => dropped.push(old),
        }
    }
    added.extend(after.iter().filter(|new| original(new).is_none()));
    (dropped, added)
}

/// The statements that turn a table from `before` into `after`: what goes
/// first, then the columns, then what comes, so that a new index may use a
/// new column and a dropped one never holds a changed column back.
pub fn alter(
    dialect: &Dialect,
    table: &TableRef,
    fields_before: &[FieldDraft],
    fields_after: &[FieldDraft],
    before: &TableExtras,
    after: &TableExtras,
) -> Result<Vec<String>> {
    after.check_names()?;
    let target = qualified(dialect, table)?;
    let q = |name: &str| quote_identifier(dialect, name.trim());
    let (old_triggers, new_triggers) = changes(
        &before.triggers,
        &after.triggers,
        |t| t.name.as_str(),
        |t| t.original.as_deref(),
    );
    let (old_keys, new_keys) = changes(
        &before.foreign_keys,
        &after.foreign_keys,
        |f| f.name.as_str(),
        |f| f.original.as_deref(),
    );
    let (old_checks, new_checks) = changes(
        &before.checks,
        &after.checks,
        |c| c.name.as_str(),
        |c| c.original.as_deref(),
    );
    let (old_indexes, new_indexes) = changes(
        &before.indexes,
        &after.indexes,
        |i| i.name.as_str(),
        |i| i.original.as_deref(),
    );
    let mut statements = Vec::new();
    for trigger in old_triggers {
        statements.push(trigger.drop(dialect, table)?);
    }
    for key in old_keys {
        statements.push(format!(
            "ALTER TABLE {target} DROP FOREIGN KEY {}",
            q(&key.name)?
        ));
    }
    for check in old_checks {
        statements.push(format!(
            "ALTER TABLE {target} DROP CONSTRAINT {}",
            q(&check.name)?
        ));
    }
    for index in old_indexes {
        statements.push(format!(
            "ALTER TABLE {target} DROP INDEX {}",
            q(&index.name)?
        ));
    }
    statements.extend(alter_table(
        DdlFlavor::MySql,
        dialect,
        table,
        fields_before,
        fields_after,
    )?);
    for index in new_indexes {
        statements.push(format!(
            "ALTER TABLE {target} ADD {}",
            index.definition(dialect)?
        ));
    }
    for check in new_checks {
        statements.push(format!(
            "ALTER TABLE {target} ADD {}",
            check.definition(dialect)?
        ));
    }
    for key in new_keys {
        statements.push(format!(
            "ALTER TABLE {target} ADD {}",
            key.definition(dialect, table)?
        ));
    }
    let options = after.options.sql(Some(&before.options))?;
    if !options.is_empty() {
        statements.push(format!("ALTER TABLE {target} {}", options.join(", ")));
    }
    if after.partitioning != before.partitioning {
        statements.push(match after.partitioning.clause(dialect)? {
            Some(clause) => format!("ALTER TABLE {target} {clause}"),
            None => format!("ALTER TABLE {target} REMOVE PARTITIONING"),
        });
    }
    for trigger in new_triggers {
        statements.push(trigger.create(dialect, table)?);
    }
    Ok(statements)
}

/// The queries that read the parts of a table, in the order [`read`] takes
/// their rows: indexes, foreign keys, CHECK constraints, triggers, options,
/// partitions. The CHECK query of MariaDB comes first; MySQL, whose
/// `check_constraints` has no table name, takes [`checks_query_mysql`].
pub fn queries(dialect: &Dialect, table: &TableRef) -> Result<[String; 6]> {
    let schema = text_literal(dialect, &table.schema)?;
    let name = text_literal(dialect, &table.name)?;
    Ok([
        format!(
            "SELECT index_name, non_unique, column_name, sub_part, collation, index_type, index_comment FROM information_schema.statistics WHERE table_schema = {schema} AND table_name = {name} AND index_name <> 'PRIMARY' ORDER BY index_name, seq_in_index"
        ),
        format!(
            "SELECT k.constraint_name, k.column_name, k.referenced_table_schema, k.referenced_table_name, k.referenced_column_name, r.update_rule, r.delete_rule FROM information_schema.key_column_usage k JOIN information_schema.referential_constraints r ON r.constraint_schema = k.constraint_schema AND r.constraint_name = k.constraint_name AND r.table_name = k.table_name WHERE k.table_schema = {schema} AND k.table_name = {name} AND k.referenced_table_name IS NOT NULL ORDER BY k.constraint_name, k.ordinal_position"
        ),
        format!(
            "SELECT tc.constraint_name, cc.check_clause FROM information_schema.table_constraints tc JOIN information_schema.check_constraints cc ON cc.constraint_schema = tc.constraint_schema AND cc.constraint_name = tc.constraint_name AND cc.table_name = tc.table_name WHERE tc.table_schema = {schema} AND tc.table_name = {name} AND tc.constraint_type = 'CHECK' ORDER BY tc.constraint_name"
        ),
        format!(
            "SELECT trigger_name, action_timing, event_manipulation, action_statement FROM information_schema.triggers WHERE event_object_schema = {schema} AND event_object_table = {name} ORDER BY action_order"
        ),
        format!(
            "SELECT t.engine, c.character_set_name, t.table_collation, t.auto_increment, t.table_comment, t.create_options FROM information_schema.tables t LEFT JOIN information_schema.collations c ON c.collation_name = t.table_collation WHERE t.table_schema = {schema} AND t.table_name = {name}"
        ),
        format!(
            "SELECT partition_name, partition_method, partition_expression, partition_description, partition_comment FROM information_schema.partitions WHERE table_schema = {schema} AND table_name = {name} AND partition_name IS NOT NULL ORDER BY partition_ordinal_position"
        ),
    ])
}

/// The CHECK query of MySQL 8.
pub fn checks_query_mysql(dialect: &Dialect, table: &TableRef) -> Result<String> {
    Ok(format!(
        "SELECT tc.constraint_name, cc.check_clause FROM information_schema.table_constraints tc JOIN information_schema.check_constraints cc ON cc.constraint_schema = tc.constraint_schema AND cc.constraint_name = tc.constraint_name WHERE tc.table_schema = {} AND tc.table_name = {} AND tc.constraint_type = 'CHECK' ORDER BY tc.constraint_name",
        text_literal(dialect, &table.schema)?,
        text_literal(dialect, &table.name)?
    ))
}

/// The engines, character sets and collations a server offers.
/// The last two both list collations: MariaDB 10.10 and later name them in
/// full (`utf8mb4_uca1400_ai_ci`) only in `full_collation_name`; MySQL, which
/// has no such column, takes the other.
pub const CATALOG_QUERIES: [&str; 4] = [
    "SELECT engine FROM information_schema.engines WHERE support IN ('YES', 'DEFAULT') ORDER BY engine",
    "SELECT character_set_name FROM information_schema.character_sets ORDER BY character_set_name",
    "SELECT full_collation_name, character_set_name FROM information_schema.collation_character_set_applicability ORDER BY full_collation_name",
    "SELECT collation_name, character_set_name FROM information_schema.collations ORDER BY collation_name",
];

fn text(row: &[Cell], index: usize) -> String {
    match row.get(index) {
        None | Some(Cell::Null) => String::new(),
        Some(cell) => cell.display(),
    }
}

/// The parts of a table from the rows of [`queries`]. `columns` are the
/// names of its columns: a CHECK named after one belongs to that column
/// (MariaDB's `JSON`) and is not the designer's.
pub fn read(rows: [&[Vec<Cell>]; 6], columns: &[String]) -> TableExtras {
    let [indexes, keys, checks, triggers, options, partitions] = rows;
    TableExtras {
        indexes: read_indexes(indexes),
        foreign_keys: read_foreign_keys(keys),
        checks: checks
            .iter()
            .map(|row| (text(row, 0), text(row, 1)))
            .filter(|(name, _)| !columns.iter().any(|c| c.eq_ignore_ascii_case(name)))
            .map(|(name, expression)| CheckDraft {
                original: Some(name.clone()),
                name,
                expression,
            })
            .collect(),
        triggers: triggers
            .iter()
            .map(|row| {
                let name = text(row, 0);
                TriggerDraft {
                    original: Some(name.clone()),
                    name,
                    timing: if text(row, 1).eq_ignore_ascii_case("AFTER") {
                        Timing::After
                    } else {
                        Timing::Before
                    },
                    event: Event::ALL
                        .into_iter()
                        .find(|e| e.sql().eq_ignore_ascii_case(&text(row, 2)))
                        .unwrap_or_default(),
                    body: text(row, 3),
                }
            })
            .collect(),
        options: options
            .first()
            .map(|row| read_options(row))
            .unwrap_or_default(),
        partitioning: read_partitions(partitions),
    }
}

fn read_indexes(rows: &[Vec<Cell>]) -> Vec<IndexDraft> {
    let mut indexes: Vec<IndexDraft> = Vec::new();
    let mut expressions = std::collections::HashSet::new();
    for row in rows {
        let name = text(row, 0);
        // An index on an expression is left as it is.
        if matches!(row.get(2), None | Some(Cell::Null)) {
            expressions.insert(name);
            continue;
        }
        let part = IndexPart {
            column: text(row, 2),
            length: text(row, 3),
            descending: text(row, 4).eq_ignore_ascii_case("D"),
        };
        match indexes.last_mut().filter(|index| index.name == name) {
            Some(index) => index.parts.push(part),
            None => indexes.push(IndexDraft {
                original: Some(name.clone()),
                kind: match text(row, 5).to_ascii_uppercase().as_str() {
                    "FULLTEXT" => IndexKind::Fulltext,
                    "SPATIAL" => IndexKind::Spatial,
                    _ if text(row, 1) == "0" => IndexKind::Unique,
                    _ => IndexKind::Index,
                },
                name,
                parts: vec![part],
                comment: text(row, 6),
            }),
        }
    }
    indexes.retain(|index| !expressions.contains(&index.name));
    indexes
}

fn read_foreign_keys(rows: &[Vec<Cell>]) -> Vec<ForeignKeyDraft> {
    let mut keys: Vec<ForeignKeyDraft> = Vec::new();
    for row in rows {
        let name = text(row, 0);
        match keys.last_mut().filter(|key| key.name == name) {
            Some(key) => {
                key.columns.push(text(row, 1));
                key.ref_columns.push(text(row, 4));
            }
            None => keys.push(ForeignKeyDraft {
                original: Some(name.clone()),
                name,
                columns: vec![text(row, 1)],
                ref_schema: text(row, 2),
                ref_table: text(row, 3),
                ref_columns: vec![text(row, 4)],
                on_update: RefAction::parse(&text(row, 5)),
                on_delete: RefAction::parse(&text(row, 6)),
            }),
        }
    }
    keys
}

/// `row_format=DYNAMIC \`PAGE_COMPRESSED\`='ON' partitioned` as settings.
pub fn read_create_options(text: &str, engine: &str) -> BTreeMap<String, String> {
    let known = engine_options(engine);
    text.split_whitespace()
        .filter_map(|part| part.split_once('='))
        .filter_map(|(key, value)| {
            let key = key.trim_matches('`').to_ascii_uppercase();
            let setting = known.iter().find(|s| s.key == key)?;
            let mut value = value.trim_matches(['\'', '"']).to_owned();
            if let SettingValues::Choice(choices) = setting.values
                && choices.contains(&"1")
            {
                // MariaDB writes the switches of an engine as ON and OFF.
                match value.to_ascii_uppercase().as_str() {
                    "ON" | "YES" if !choices.contains(&"YES") => value = "1".into(),
                    "OFF" | "NO" if !choices.contains(&"NO") => value = "0".into(),
                    _ => {}
                }
            }
            Some((key, value.to_ascii_uppercase()))
        })
        .collect()
}

fn read_options(row: &[Cell]) -> TableOptions {
    let engine = text(row, 0);
    let collation = text(row, 2);
    // MariaDB 10.10 and later list a collation by a name without its
    // character set, so the join may find none: the set starts the name.
    let charset = match text(row, 1) {
        set if set.is_empty() => collation.split('_').next().unwrap_or_default().to_owned(),
        set => set,
    };
    TableOptions {
        settings: read_create_options(&text(row, 5), &engine),
        engine,
        charset,
        collation,
        auto_increment: text(row, 3),
        comment: text(row, 4),
    }
}

fn read_partitions(rows: &[Vec<Cell>]) -> Partitioning {
    let Some(first) = rows.first() else {
        return Partitioning::default();
    };
    let method = PartitionMethod::parse(&text(first, 1));
    let partitions: Vec<PartitionDef> = rows
        .iter()
        .map(|row| PartitionDef {
            name: text(row, 0),
            values: text(row, 3),
            comment: text(row, 4),
        })
        .collect();
    // Partitions of HASH and KEY named by the server are a count.
    let numbered = !method.listed()
        && partitions
            .iter()
            .enumerate()
            .all(|(i, p)| p.name == format!("p{i}") && p.comment.is_empty());
    Partitioning {
        method,
        expression: text(first, 2),
        count: if numbered {
            partitions.len().to_string()
        } else {
            String::new()
        },
        partitions: if numbered { Vec::new() } else { partitions },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::mysql::DIALECT;

    fn table() -> TableRef {
        TableRef {
            schema: "shop".into(),
            name: "orders".into(),
            kind: "table".into(),
        }
    }

    fn field(name: &str, data_type: &str) -> FieldDraft {
        FieldDraft {
            name: name.into(),
            data_type: data_type.into(),
            original: Some(name.into()),
            ..Default::default()
        }
    }

    fn extras() -> TableExtras {
        TableExtras {
            indexes: vec![IndexDraft {
                name: "by_customer".into(),
                kind: IndexKind::Unique,
                parts: vec![
                    IndexPart {
                        column: "customer".into(),
                        ..Default::default()
                    },
                    IndexPart {
                        column: "note".into(),
                        length: "20".into(),
                        descending: true,
                    },
                ],
                comment: "it's".into(),
                original: None,
            }],
            foreign_keys: vec![ForeignKeyDraft {
                name: "fk_customer".into(),
                columns: vec!["customer".into()],
                ref_table: "customers".into(),
                ref_columns: vec!["id".into()],
                on_delete: RefAction::Cascade,
                ..Default::default()
            }],
            checks: vec![CheckDraft {
                name: "positive".into(),
                expression: "total >= 0".into(),
                original: None,
            }],
            triggers: vec![TriggerDraft {
                name: "stamp".into(),
                timing: Timing::Before,
                event: Event::Insert,
                body: "BEGIN SET NEW.total = 0; END".into(),
                original: None,
            }],
            options: TableOptions {
                engine: "InnoDB".into(),
                charset: "utf8mb4".into(),
                collation: "utf8mb4_unicode_ci".into(),
                comment: "orders".into(),
                settings: BTreeMap::from([("ROW_FORMAT".into(), "DYNAMIC".into())]),
                ..Default::default()
            },
            partitioning: Partitioning::default(),
        }
    }

    #[test]
    fn a_new_table_has_everything_in_its_create() {
        let fields = vec![field("id", "int"), field("customer", "int")];
        let statements = create(&DIALECT, &table(), &fields, &extras()).unwrap();
        let create = &statements[0];
        for part in [
            "  UNIQUE INDEX `by_customer` (`customer`, `note`(20) DESC) COMMENT 'it''s'",
            "  CONSTRAINT `positive` CHECK (total >= 0)",
            "  CONSTRAINT `fk_customer` FOREIGN KEY (`customer`) REFERENCES `shop`.`customers` (`id`) ON DELETE CASCADE ON UPDATE RESTRICT",
            "\nENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci COMMENT=",
            " ROW_FORMAT=DYNAMIC",
        ] {
            assert!(create.contains(part), "{part}\n{create}");
        }
        assert_eq!(
            statements[1],
            "CREATE TRIGGER `shop`.`stamp` BEFORE INSERT ON `shop`.`orders` FOR EACH ROW BEGIN SET NEW.total = 0; END"
        );
    }

    #[test]
    fn a_changed_table_drops_before_it_adds() {
        let fields = vec![field("id", "int"), field("customer", "int")];
        let mut before = extras();
        before.indexes[0].original = Some("by_customer".into());
        before.foreign_keys[0].original = Some("fk_customer".into());
        before.checks[0].original = Some("positive".into());
        before.triggers[0].original = Some("stamp".into());
        let mut after = before.clone();
        // Unchanged: nothing to do.
        assert!(
            alter(&DIALECT, &table(), &fields, &fields, &before, &after)
                .unwrap()
                .is_empty()
        );
        // Aria keeps no foreign keys: the change is refused before it runs.
        let mut aria = after.clone();
        aria.options.engine = "Aria".into();
        assert_eq!(
            alter(&DIALECT, &table(), &fields, &fields, &before, &aria)
                .unwrap_err()
                .to_string(),
            "engine_foreign_keys_unsupported"
        );
        after.foreign_keys.clear();
        after.indexes[0].kind = IndexKind::Index;
        after.checks.clear();
        after.triggers[0].body = "SET NEW.total = 1".into();
        after.options.engine = "Aria".into();
        after.options.settings = BTreeMap::from([("TRANSACTIONAL".into(), "1".into())]);
        after.partitioning = Partitioning {
            method: PartitionMethod::Hash,
            expression: "id".into(),
            count: "4".into(),
            partitions: Vec::new(),
        };
        let statements = alter(&DIALECT, &table(), &fields, &fields, &before, &after).unwrap();
        let expected = [
            "DROP TRIGGER IF EXISTS `shop`.`stamp`",
            "ALTER TABLE `shop`.`orders` DROP FOREIGN KEY `fk_customer`",
            "ALTER TABLE `shop`.`orders` DROP CONSTRAINT `positive`",
            "ALTER TABLE `shop`.`orders` DROP INDEX `by_customer`",
            "ALTER TABLE `shop`.`orders` ADD INDEX `by_customer` (`customer`, `note`(20) DESC) COMMENT 'it''s'",
            // A setting the new engine has but the table no longer sets.
            "ALTER TABLE `shop`.`orders` ENGINE=Aria, ROW_FORMAT=DEFAULT, TRANSACTIONAL=1",
            "ALTER TABLE `shop`.`orders` PARTITION BY HASH (id) PARTITIONS 4",
            "CREATE TRIGGER `shop`.`stamp` BEFORE INSERT ON `shop`.`orders` FOR EACH ROW SET NEW.total = 1",
        ];
        assert_eq!(statements.len(), expected.len(), "{statements:#?}");
        for (got, want) in statements.iter().zip(expected) {
            assert!(got.starts_with(want), "{got}\n{want}");
        }
        // Partitions taken away.
        let mut plain = after.clone();
        plain.partitioning = Partitioning::default();
        assert_eq!(
            alter(&DIALECT, &table(), &fields, &fields, &after, &plain).unwrap(),
            ["ALTER TABLE `shop`.`orders` REMOVE PARTITIONING"]
        );
    }

    #[test]
    fn partitions_are_written_by_their_method() {
        let range = Partitioning {
            method: PartitionMethod::Range,
            expression: "YEAR(created)".into(),
            count: String::new(),
            partitions: vec![
                PartitionDef {
                    name: "p2024".into(),
                    values: "2025".into(),
                    comment: String::new(),
                },
                PartitionDef {
                    name: "rest".into(),
                    values: "maxvalue".into(),
                    comment: String::new(),
                },
            ],
        };
        assert_eq!(
            range.clause(&DIALECT).unwrap().unwrap(),
            "PARTITION BY RANGE (YEAR(created)) (\n  PARTITION `p2024` VALUES LESS THAN (2025),\n  PARTITION `rest` VALUES LESS THAN MAXVALUE\n)"
        );
        let key = Partitioning {
            method: PartitionMethod::Key,
            count: "8".into(),
            ..Default::default()
        };
        assert_eq!(
            key.clause(&DIALECT).unwrap().unwrap(),
            "PARTITION BY KEY () PARTITIONS 8"
        );
        let injected = Partitioning {
            method: PartitionMethod::Hash,
            expression: "id); DROP TABLE x".into(),
            count: "2".into(),
            ..Default::default()
        };
        assert!(injected.clause(&DIALECT).is_err());
    }

    #[test]
    fn engine_settings_are_checked_and_read_back() {
        let mut options = TableOptions {
            engine: "InnoDB".into(),
            settings: BTreeMap::from([("ROW_FORMAT".into(), "DYNAMIC; DROP".into())]),
            ..Default::default()
        };
        assert!(options.sql(None).is_err());
        options.settings = BTreeMap::from([("ROW_FORMAT".into(), "COMPRESSED".into())]);
        let before = options.clone();
        options.settings.clear();
        assert_eq!(options.sql(Some(&before)).unwrap(), ["ROW_FORMAT=DEFAULT"]);
        assert_eq!(
            read_create_options(
                "row_format=DYNAMIC `PAGE_COMPRESSED`='ON' partitioned",
                "InnoDB"
            ),
            BTreeMap::from([
                ("PAGE_COMPRESSED".into(), "1".into()),
                ("ROW_FORMAT".into(), "DYNAMIC".into())
            ])
        );
    }

    #[test]
    fn the_parts_of_a_table_are_read_from_their_rows() {
        let row = |cells: &[&str]| -> Vec<Cell> {
            cells
                .iter()
                .map(|c| {
                    if *c == "NULL" {
                        Cell::Null
                    } else {
                        Cell::Text((*c).into())
                    }
                })
                .collect()
        };
        let indexes = vec![
            row(&["by_name", "1", "name", "10", "A", "BTREE", ""]),
            row(&["by_name", "1", "kind", "NULL", "D", "BTREE", ""]),
            row(&["code", "0", "code", "NULL", "A", "BTREE", "unique code"]),
            row(&["search", "1", "body", "NULL", "NULL", "FULLTEXT", ""]),
        ];
        let keys = vec![
            row(&["fk_pair", "a", "shop", "pairs", "x", "CASCADE", "SET NULL"]),
            row(&["fk_pair", "b", "shop", "pairs", "y", "CASCADE", "SET NULL"]),
        ];
        let checks = vec![
            row(&["doc", "json_valid(`doc`)"]),
            row(&["positive", "`total` > 0"]),
        ];
        let triggers = vec![row(&["stamp", "AFTER", "UPDATE", "SET @x = 1"])];
        let options = vec![row(&[
            "Aria",
            "utf8mb4",
            "utf8mb4_general_ci",
            "21",
            "note",
            "transactional=1 row_format=PAGE",
        ])];
        let partitions = vec![
            row(&["p0", "HASH", "`id`", "NULL", ""]),
            row(&["p1", "HASH", "`id`", "NULL", ""]),
        ];
        let extras = read(
            [&indexes, &keys, &checks, &triggers, &options, &partitions],
            &["doc".into()],
        );
        assert_eq!(extras.indexes.len(), 3);
        assert!(extras.indexes[0].parts[1].descending);
        assert_eq!(extras.indexes[0].parts[0].length, "10");
        assert_eq!(extras.indexes[1].kind, IndexKind::Unique);
        assert_eq!(extras.indexes[2].kind, IndexKind::Fulltext);
        assert_eq!(extras.foreign_keys[0].columns, ["a", "b"]);
        assert_eq!(extras.foreign_keys[0].on_delete, RefAction::SetNull);
        // The CHECK of the JSON column belongs to the column.
        assert_eq!(extras.checks.len(), 1);
        assert_eq!(extras.triggers[0].timing, Timing::After);
        assert_eq!(extras.triggers[0].event, Event::Update);
        assert_eq!(extras.options.settings["TRANSACTIONAL"], "1");
        assert_eq!(extras.partitioning.count, "2");
        assert!(extras.partitioning.partitions.is_empty());
    }
}
