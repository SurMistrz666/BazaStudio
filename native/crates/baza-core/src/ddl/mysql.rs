//! The parts of a MySQL or MariaDB table beyond its columns, as the table
//! designer edits them: indexes, foreign keys, CHECK constraints, triggers,
//! the options of the table with the settings of its engine, and its
//! partitions. Each is read from `information_schema`, written into `CREATE
//! TABLE`, and turned into the statements that change a table from what it
//! has into what the designer shows.
use super::{DdlFlavor, FieldDraft, alter_table, create_table_with};
use crate::engine::Dialect;
use crate::model::{Cell, TableRef};
use crate::sql::{
    Lexeme, backslash_flipped, lex, literal, qualified, quote_identifier, sql_fragment,
};
use anyhow::{Result, bail, ensure};
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
    /// A whole number from the first to the second.
    Number(u64, u64),
}

/// A setting of a table that one engine has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineSetting {
    pub key: &'static str,
    /// Polish and English.
    pub label: (&'static str, &'static str),
    pub values: SettingValues,
    /// Only MariaDB has it; MySQL refuses it.
    pub mariadb_only: bool,
}

const fn setting(
    key: &'static str,
    label: (&'static str, &'static str),
    values: SettingValues,
) -> EngineSetting {
    EngineSetting {
        key,
        label,
        values,
        mariadb_only: false,
    }
}

const fn mariadb_setting(
    key: &'static str,
    label: (&'static str, &'static str),
    values: SettingValues,
) -> EngineSetting {
    EngineSetting {
        mariadb_only: true,
        ..setting(key, label, values)
    }
}

/// Numbers whose `0` asks for what the server chooses.
const ZERO_IS_DEFAULT: [&str; 4] = ["KEY_BLOCK_SIZE", "MAX_ROWS", "MIN_ROWS", "AVG_ROW_LENGTH"];

impl EngineSetting {
    /// The value that gives a setting back to the server's choice, when the
    /// table no longer sets it; `None` where the server has no such value,
    /// and the setting stays as it is.
    pub fn reset(&self) -> Option<&'static str> {
        match self.values {
            SettingValues::Choice(choices) if choices.contains(&"DEFAULT") => Some("DEFAULT"),
            _ if ZERO_IS_DEFAULT.contains(&self.key) => Some("0"),
            // The grammar of both servers takes `STATS_SAMPLE_PAGES=DEFAULT`.
            _ if self.key == "STATS_SAMPLE_PAGES" => Some("DEFAULT"),
            _ => None,
        }
    }
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
    mariadb_setting(
        "PAGE_COMPRESSED",
        ("Kompresja stron (MariaDB)", "Page compression (MariaDB)"),
        SWITCH,
    ),
    mariadb_setting(
        "PAGE_COMPRESSION_LEVEL",
        (
            "Poziom kompresji stron (MariaDB)",
            "Page compression level (MariaDB)",
        ),
        SettingValues::Number(1, 9),
    ),
    mariadb_setting(
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
        SettingValues::Number(1, 65535),
    ),
];
const ARIA: &[EngineSetting] = &[
    setting(
        "ROW_FORMAT",
        ("Format wierszy", "Row format"),
        SettingValues::Choice(&["DEFAULT", "PAGE", "FIXED", "DYNAMIC"]),
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
        SettingValues::Number(0, u32::MAX as u64),
    ),
    setting(
        "AVG_ROW_LENGTH",
        ("Średnia długość wiersza", "Average row length"),
        SettingValues::Number(0, u32::MAX as u64),
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
        SettingValues::Number(0, u32::MAX as u64),
    ),
    setting(
        "AVG_ROW_LENGTH",
        ("Średnia długość wiersza", "Average row length"),
        SettingValues::Number(0, u32::MAX as u64),
    ),
];
const MEMORY: &[EngineSetting] = &[
    setting(
        "MAX_ROWS",
        ("Najwięcej wierszy", "Most rows"),
        SettingValues::Number(0, u32::MAX as u64),
    ),
    setting(
        "MIN_ROWS",
        ("Najmniej wierszy", "Fewest rows"),
        SettingValues::Number(0, u32::MAX as u64),
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
    /// The partitions are divided again (`SUBPARTITION BY`), which the
    /// designer does not edit: it only takes partitioning away there.
    pub subpartitioned: bool,
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
/// comment after it, and no quote left open that would take in the SQL
/// written after it (see [`sql_fragment`]).
fn expression(dialect: &Dialect, text: &str, code: &'static str) -> Result<String> {
    let text = text.trim();
    ensure!(!text.is_empty() && text.len() <= 8000, code);
    sql_fragment(dialect, text).map_err(|_| anyhow::anyhow!(code))?;
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
            expression(dialect, &self.expression, "check_invalid")?
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
        ensure!(!self.name.trim().is_empty(), "trigger_invalid");
        let body = trigger_body(dialect, &self.body)?;
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

/// A block of a trigger body, as the server reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Block {
    Begin,
    If,
    /// `CASE` as a statement, closed by `END CASE`.
    CaseStatement,
    /// `CASE` in an expression, closed by `END` alone.
    CaseExpression,
    Loop,
    /// `WHILE`, and whether the `DO` after its condition has come.
    While(bool),
    /// MariaDB's `FOR … IN … DO`, and whether its `DO` has come.
    For(bool),
    Repeat,
}
impl Block {
    /// The word after `END` that closes the block; none for `BEGIN` and for
    /// a `CASE` expression.
    fn closer(self) -> Option<&'static str> {
        match self {
            Block::Begin | Block::CaseExpression => None,
            Block::If => Some("IF"),
            Block::CaseStatement => Some("CASE"),
            Block::Loop => Some("LOOP"),
            Block::While(_) => Some("WHILE"),
            Block::For(_) => Some("FOR"),
            Block::Repeat => Some("REPEAT"),
        }
    }
    /// Statements end with `;` inside it.
    fn holds_statements(self) -> bool {
        !matches!(
            self,
            Block::CaseExpression | Block::While(false) | Block::For(false)
        )
    }
}

/// The words that open, divide and close the blocks of a trigger body.
const BODY_WORDS: [&str; 12] = [
    "BEGIN", "END", "IF", "CASE", "LOOP", "WHILE", "FOR", "REPEAT", "THEN", "ELSE", "DO", "HANDLER",
];

/// Keywords an operand follows: an `END` right after one is a name and does
/// not end a `CASE` expression or a `REPEAT`.
#[rustfmt::skip]
const OPERAND_AFTER: &[&str] = &[
    "WHEN", "THEN", "ELSE", "ELSEIF", "IF", "CASE", "WHILE", "UNTIL", "DO", "AND", "OR", "XOR",
    "NOT", "IS", "LIKE", "RLIKE", "REGEXP", "SOUNDS", "IN", "BETWEEN", "DIV", "MOD", "COLLATE",
    "INTERVAL", "BINARY", "ESCAPE", "AS", "ON", "BY", "FOR", "OVER", "OF", "SET", "SELECT", "INTO",
    "FROM", "JOIN", "WHERE", "HAVING", "LIMIT", "OFFSET", "VALUES", "VALUE", "RETURN", "DECLARE",
    "DEFAULT", "DISTINCT", "ALL", "ANY", "SOME", "EXISTS", "USING", "UPDATE", "TABLE", "CALL",
    "SIGNAL", "RESIGNAL", "LEAVE", "ITERATE", "OPEN", "CLOSE", "FETCH", "WITH", "UNION", "EXCEPT",
    "INTERSECT", "PARTITION", "WINDOW", "ROWS", "RANGE", "AGAINST", "MATCH", "ROW", "CHARACTER",
    "CHARSET", "TO",
];

/// The body of a trigger as the server will read it: one statement, or one
/// compound statement such as `BEGIN … END`, and nothing after it. The body
/// goes in the same text as `CREATE TRIGGER`, and the server runs whatever
/// statement follows the end of the trigger at once. The body is read as the
/// engine reads SQL, with a backslash in strings taken as an escape and
/// literally, since the server may be set to either; its blocks are followed
/// as the server follows them, and a body this reading cannot follow for
/// certain is refused. Gives the body without a `;` or comments at its end.
fn trigger_body(dialect: &Dialect, body: &str) -> Result<String> {
    let body = body.trim();
    ensure!(
        !body.is_empty()
            && body.len() <= 65535
            && !body
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r')),
        "trigger_invalid"
    );
    let end = trigger_body_end(dialect, body)?;
    // Both readings have to find the same end.
    ensure!(
        trigger_body_end(&backslash_flipped(dialect), body)? == end,
        "trigger_body_invalid"
    );
    Ok(body[..end].to_owned())
}

/// Where a trigger body ends for one way of reading its quotes: after its
/// last word or symbol, before a `;` that only closes it.
fn trigger_body_end(dialect: &Dialect, body: &str) -> Result<usize> {
    const INVALID: &str = "trigger_body_invalid";
    let mut tokens: Vec<(Lexeme, std::ops::Range<usize>)> = Vec::new();
    let mut executable = false;
    lex(dialect, body, |kind, range| match kind {
        Lexeme::Space => {}
        // MySQL runs the text of `/*! … */`, MariaDB that of `/*M! … */` too.
        Lexeme::Comment => {
            let text = body[range].to_ascii_uppercase();
            executable |= text.starts_with("/*!") || text.starts_with("/*M!");
        }
        _ => tokens.push((kind, range)),
    })
    .map_err(|_| anyhow::anyhow!(INVALID))?;
    ensure!(!executable, INVALID);
    let symbol = |tokens: &[(Lexeme, std::ops::Range<usize>)], i: usize, text: &str| {
        tokens
            .get(i)
            .is_some_and(|(kind, range)| *kind == Lexeme::Symbol && &body[range.clone()] == text)
    };
    if symbol(&tokens, tokens.len().wrapping_sub(1), ";") {
        tokens.pop();
    }
    let Some(end) = tokens.last().map(|(_, range)| range.end) else {
        bail!(INVALID);
    };
    let tokens = &tokens[..];
    let symbol = |i: usize, text: &str| symbol(tokens, i, text);
    let word = |i: usize| match tokens.get(i) {
        Some((Lexeme::Word, range)) => Some(body[range.clone()].to_ascii_uppercase()),
        _ => None,
    };
    // A word after `@`, or after a `.` that follows a name, is a name and
    // never a keyword. After a number the `.` would be a decimal point. A
    // word joined to `$` is part of a name too (`x$end`), which the lexer
    // reads as three tokens.
    let named: Vec<bool> = (0..tokens.len())
        .map(|i| {
            let range = &tokens[i].1;
            body[..range.start].ends_with('$')
                || body[range.end..].starts_with('$')
                || i > 0
                    && (symbol(i - 1, "@")
                        || symbol(i - 1, ".")
                            && i > 1
                            && match &tokens[i - 2] {
                                (Lexeme::Quoted, _) => true,
                                (Lexeme::Word, range) => {
                                    !body[range.clone()].starts_with(|c: char| c.is_ascii_digit())
                                }
                                _ => false,
                            })
        })
        .collect();
    // The last token of an operand: what may stand before the `END` of a
    // `CASE` expression or of the condition of `REPEAT`.
    let ends_operand = |i: usize| match &tokens[i] {
        (Lexeme::Literal | Lexeme::Quoted, _) => true,
        (Lexeme::Symbol, range) => &body[range.clone()] == ")",
        (Lexeme::Word, range) => {
            named[i]
                || !OPERAND_AFTER
                    .iter()
                    .any(|k| k.eq_ignore_ascii_case(&body[range.clone()]))
        }
        _ => false,
    };
    // Each open block, the parentheses open around it, and where its
    // content starts.
    let mut blocks: Vec<(Block, usize, usize)> = Vec::new();
    let mut parens = 0usize;
    // Whether the token is the first of a statement.
    let mut start = true;
    let mut i = 0;
    while i < tokens.len() {
        let at_start = std::mem::replace(&mut start, false);
        let inner = blocks.last().map(|block| block.0);
        match tokens[i].0 {
            Lexeme::Word if !named[i] => {
                let keyword = word(i).unwrap_or_default();
                let structural = BODY_WORDS.contains(&keyword.as_str());
                ensure!(!(structural && symbol(i.wrapping_sub(1), ".")), INVALID);
                ensure!(keyword != "DELIMITER", "trigger_invalid");
                match keyword.as_str() {
                    // A label before a block: `name:`, not `:=`.
                    _ if at_start && !structural && symbol(i + 1, ":") && !symbol(i + 2, "=") => {
                        i += 2;
                        start = true;
                        continue;
                    }
                    "BEGIN" if at_start => {
                        // MariaDB: `BEGIN NOT ATOMIC`.
                        if word(i + 1).as_deref() == Some("NOT")
                            && word(i + 2).as_deref() == Some("ATOMIC")
                        {
                            i += 2;
                        }
                        blocks.push((Block::Begin, parens, i + 1));
                        start = true;
                    }
                    "IF" if at_start => blocks.push((Block::If, parens, i + 1)),
                    "LOOP" if at_start => {
                        blocks.push((Block::Loop, parens, i + 1));
                        start = true;
                    }
                    "WHILE" if at_start => blocks.push((Block::While(false), parens, i + 1)),
                    "FOR" if at_start => blocks.push((Block::For(false), parens, i + 1)),
                    "REPEAT" if at_start => {
                        blocks.push((Block::Repeat, parens, i + 1));
                        start = true;
                    }
                    "CASE" => {
                        let block = if at_start {
                            Block::CaseStatement
                        } else {
                            Block::CaseExpression
                        };
                        blocks.push((block, parens, i + 1));
                    }
                    // Statements follow these in an IF or a CASE statement;
                    // in a CASE expression a value does.
                    "THEN" | "ELSE" => {
                        start = matches!(inner, Some(Block::If | Block::CaseStatement));
                    }
                    "DO" if matches!(inner, Some(Block::While(false) | Block::For(false))) => {
                        if let Some(block) = blocks.last_mut() {
                            block.0 = match block.0 {
                                Block::For(_) => Block::For(true),
                                _ => Block::While(true),
                            };
                        }
                        start = true;
                    }
                    // `DECLARE … HANDLER FOR condition, … statement`.
                    "HANDLER" if word(i + 1).as_deref() == Some("FOR") => {
                        i += 1;
                        loop {
                            i += 1;
                            match word(i).as_deref() {
                                Some("SQLSTATE") => {
                                    if word(i + 1).as_deref() == Some("VALUE") {
                                        i += 1;
                                    }
                                    i += 1;
                                    ensure!(
                                        tokens.get(i).is_some_and(|t| t.0 == Lexeme::Literal),
                                        INVALID
                                    );
                                }
                                Some("NOT") => {
                                    i += 1;
                                    ensure!(word(i).as_deref() == Some("FOUND"), INVALID);
                                }
                                Some(name) if !BODY_WORDS.contains(&name) => {}
                                _ => bail!(INVALID),
                            }
                            if !symbol(i + 1, ",") {
                                break;
                            }
                            i += 1;
                        }
                        start = true;
                    }
                    "END" => {
                        let (block, open, content) =
                            blocks.pop().ok_or_else(|| anyhow::anyhow!(INVALID))?;
                        let closer = word(i + 1).filter(|w| {
                            ["IF", "CASE", "LOOP", "WHILE", "FOR", "REPEAT"].contains(&w.as_str())
                        });
                        // A statement block ends after its last `;`, an
                        // expression after its last operand.
                        let placed = match block {
                            Block::Begin => i == content || symbol(i - 1, ";"),
                            Block::If
                            | Block::CaseStatement
                            | Block::Loop
                            | Block::While(true)
                            | Block::For(true) => symbol(i - 1, ";"),
                            Block::While(false) | Block::For(false) => false,
                            Block::Repeat | Block::CaseExpression => ends_operand(i - 1),
                        };
                        ensure!(
                            placed && open == parens && closer.as_deref() == block.closer(),
                            INVALID
                        );
                        if closer.is_some() {
                            i += 1;
                        }
                    }
                    _ => {}
                }
            }
            Lexeme::Symbol => match &body[tokens[i].1.clone()] {
                ";" => {
                    ensure!(
                        parens == 0 && inner.is_some_and(Block::holds_statements),
                        INVALID
                    );
                    start = true;
                }
                "(" => parens += 1,
                ")" => {
                    ensure!(parens > blocks.last().map_or(0, |block| block.1), INVALID);
                    parens -= 1;
                }
                _ => {}
            },
            _ => {}
        }
        i += 1;
    }
    ensure!(blocks.is_empty() && parens == 0, INVALID);
    Ok(end)
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
            SettingValues::Number(min, max) => {
                value.parse::<u64>().is_ok_and(|n| (min..=max).contains(&n))
            }
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
                // A setting taken away goes back to what the server chooses,
                // where the server has a value that says so.
                if let Some(reset) = setting.reset() {
                    parts.push(format!("{}={reset}", setting.key));
                }
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
            _ => expression(dialect, &self.expression, "partition_invalid")?,
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
                            expression(dialect, &partition.values, "partition_invalid")?
                        ))
                    }
                    PartitionMethod::List | PartitionMethod::ListColumns => {
                        line.push_str(&format!(
                            " VALUES IN ({})",
                            expression(dialect, &partition.values, "partition_invalid")?
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
/// of `after` that are new or changed, a changed one with what it was: what
/// to drop and what to add. A part is matched by its `original` name; one
/// whose original name `before` does not have (gone since it was read) is
/// new.
#[allow(clippy::type_complexity)]
fn changes<'a, T: PartialEq>(
    before: &'a [T],
    after: &'a [T],
    name: impl Fn(&T) -> &str,
    original: impl Fn(&T) -> Option<&str>,
) -> (Vec<&'a T>, Vec<(&'a T, Option<&'a T>)>) {
    let mut dropped = Vec::new();
    let mut added = Vec::new();
    for old in before {
        match after.iter().find(|new| original(new) == Some(name(old))) {
            Some(new) if new == old => {}
            Some(new) => {
                dropped.push(old);
                added.push((new, Some(old)));
            }
            None => dropped.push(old),
        }
    }
    added.extend(
        after
            .iter()
            .filter(|new| original(new).is_none_or(|o| !before.iter().any(|old| name(old) == o)))
            .map(|new| (new, None)),
    );
    (dropped, added)
}

/// What puts a part of a table back as it was when saving stops halfway.
/// A changed index, foreign key, CHECK or trigger is dropped before the
/// columns change and added again after them; when a statement in between
/// (or the new part itself) fails, the old part would be gone. `sql`
/// creates it again; it is needed when the statement that failed comes
/// after `dropped` and no later than `added`, by their places in the list
/// [`alter_plan`] gives. A part whose columns are renamed in between has a
/// restore for each stretch between the renames, each naming the columns as
/// they are there: `dropped` is then the statement that renamed one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restore {
    pub dropped: usize,
    pub added: usize,
    pub sql: String,
}
impl Restore {
    /// Whether the statement at `failed` leaves the part gone.
    pub fn needed(&self, failed: usize) -> bool {
        self.dropped < failed && failed <= self.added
    }
}

/// The columns renamed by the change from `before` to `after`: their names
/// before and after.
fn renamed_columns(before: &[FieldDraft], after: &[FieldDraft]) -> Vec<(String, String)> {
    after
        .iter()
        .filter_map(|field| {
            let original = field.original.as_deref()?;
            let old = before.iter().find(|f| f.name == original)?;
            (field.name.trim() != old.name)
                .then(|| (old.name.clone(), field.name.trim().to_owned()))
        })
        .collect()
}

/// The new name of `column` among `renames`.
fn renamed<'a>(renames: &'a [(String, String)], column: &str) -> Option<&'a str> {
    renames
        .iter()
        .find(|(old, _)| old == column.trim())
        .map(|(_, new)| new.as_str())
}

impl ForeignKeyDraft {
    /// The key points at its own table.
    fn refers_to(&self, table: &TableRef) -> bool {
        self.ref_table.trim() == table.name
            && (self.ref_schema.trim().is_empty() || self.ref_schema.trim() == table.schema)
    }
}

impl TableExtras {
    /// The parts with the columns of the table named through `rename`, which
    /// gives the new name of a column or `None` to keep it: the columns of
    /// the indexes and of the foreign keys, and the columns a key points at
    /// in its own table. A key that points at a table of the same schema
    /// names that schema, as the server reads it back.
    fn with_columns(&self, table: &TableRef, rename: impl Fn(&str) -> Option<String>) -> Self {
        let mut extras = self.clone();
        let name = |column: &mut String| {
            if let Some(new) = rename(column) {
                *column = new;
            }
        };
        for index in &mut extras.indexes {
            index
                .parts
                .iter_mut()
                .for_each(|part| name(&mut part.column));
        }
        for key in &mut extras.foreign_keys {
            if key.refers_to(table) {
                key.ref_columns.iter_mut().for_each(name);
            }
            key.columns.iter_mut().for_each(name);
            if key.ref_schema.trim().is_empty() {
                key.ref_schema = table.schema.clone();
            }
        }
        extras
    }

    /// After a save that stopped halfway the designer keeps what was edited,
    /// while the database may already have part of it. A part shown as new,
    /// or under an original name the database no longer has, that the
    /// database now has under its name is taken as that one, so that the
    /// next save changes it instead of adding it a second time.
    pub fn adopt_existing(&mut self, database: &TableExtras) {
        fn adopt<T>(
            parts: &mut [T],
            existing: &[T],
            name: impl Fn(&T) -> &str,
            original: impl Fn(&mut T) -> &mut Option<String>,
        ) {
            for part in parts {
                let known = original(part)
                    .as_deref()
                    .is_some_and(|o| existing.iter().any(|e| name(e) == o));
                let current = name(part).trim().to_owned();
                if !known && existing.iter().any(|e| name(e) == current) {
                    *original(part) = Some(current);
                }
            }
        }
        adopt(
            &mut self.indexes,
            &database.indexes,
            |i| i.name.as_str(),
            |i| &mut i.original,
        );
        adopt(
            &mut self.foreign_keys,
            &database.foreign_keys,
            |f| f.name.as_str(),
            |f| &mut f.original,
        );
        adopt(
            &mut self.checks,
            &database.checks,
            |c| c.name.as_str(),
            |c| &mut c.original,
        );
        adopt(
            &mut self.triggers,
            &database.triggers,
            |t| t.name.as_str(),
            |t| &mut t.original,
        );
    }
}

/// The kinds of parts [`alter_plan`] drops and adds again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Index,
    ForeignKey,
    Check,
    Trigger,
}

/// The restores of one changed part, dropped by the statement `dropped` and
/// added again by `added`: one for each stretch between the statements that
/// rename a column of `columns` (the part's columns as they were), each with
/// the SQL `sql` writes for the renames done by then.
fn restores_of(
    dropped: usize,
    added: usize,
    columns: &[&str],
    renamed_at: &[(String, String, usize)],
    sql: impl Fn(&[(String, String)]) -> Option<String>,
) -> Vec<Restore> {
    let mut bounds = vec![dropped];
    bounds.extend(
        renamed_at
            .iter()
            .filter(|(old, _, at)| columns.contains(&old.as_str()) && dropped < *at && *at < added)
            .map(|(_, _, at)| *at),
    );
    bounds.sort_unstable();
    bounds.dedup();
    bounds.push(added);
    bounds
        .windows(2)
        .filter_map(|stretch| {
            let done: Vec<(String, String)> = renamed_at
                .iter()
                .filter(|(_, _, at)| *at <= stretch[0])
                .map(|(old, new, _)| (old.clone(), new.clone()))
                .collect();
            sql(&done).map(|sql| Restore {
                dropped: stretch[0],
                added: stretch[1],
                sql,
            })
        })
        .collect()
}

/// The statements that turn a table from `before` into `after`.
pub fn alter(
    dialect: &Dialect,
    table: &TableRef,
    fields_before: &[FieldDraft],
    fields_after: &[FieldDraft],
    before: &TableExtras,
    after: &TableExtras,
) -> Result<Vec<String>> {
    alter_plan(dialect, table, fields_before, fields_after, before, after)
        .map(|(statements, _)| statements)
}

/// The statements that turn a table from `before` into `after`, and what
/// puts back the parts they drop when one of them fails ([`Restore`]). What
/// goes comes first, then the columns, then what comes, so that a new index
/// may use a new column and a dropped one never holds a changed column back.
///
/// The server renames a column in the indexes and foreign keys that use it,
/// so a part that changes only by such a rename stays; a part that still
/// names a column by the name it is losing gets the new name.
pub fn alter_plan(
    dialect: &Dialect,
    table: &TableRef,
    fields_before: &[FieldDraft],
    fields_after: &[FieldDraft],
    before: &TableExtras,
    after: &TableExtras,
) -> Result<(Vec<String>, Vec<Restore>)> {
    after.check_names()?;
    let target = qualified(dialect, table)?;
    let q = |name: &str| quote_identifier(dialect, name.trim());
    let renames = renamed_columns(fields_before, fields_after);
    // The parts as the server has them once the columns are renamed, and as
    // the designer shows them with each column under the name it will have.
    let old = before.with_columns(table, |column| renamed(&renames, column).map(str::to_owned));
    let new = after.with_columns(table, |column| {
        if fields_after.iter().any(|f| f.name.trim() == column.trim()) {
            None
        } else {
            renamed(&renames, column).map(str::to_owned)
        }
    });
    let (old_triggers, new_triggers) = changes(
        &old.triggers,
        &new.triggers,
        |t| t.name.as_str(),
        |t| t.original.as_deref(),
    );
    let (old_keys, new_keys) = changes(
        &old.foreign_keys,
        &new.foreign_keys,
        |f| f.name.as_str(),
        |f| f.original.as_deref(),
    );
    let (old_checks, new_checks) = changes(
        &old.checks,
        &new.checks,
        |c| c.name.as_str(),
        |c| c.original.as_deref(),
    );
    let (old_indexes, new_indexes) = changes(
        &old.indexes,
        &new.indexes,
        |i| i.name.as_str(),
        |i| i.original.as_deref(),
    );
    let mut statements = Vec::new();
    // Where each part was dropped: its kind, its name and the statement.
    let mut drops: Vec<(Part, String, usize)> = Vec::new();
    for trigger in old_triggers {
        drops.push((Part::Trigger, trigger.name.clone(), statements.len()));
        statements.push(trigger.drop(dialect, table)?);
    }
    for key in old_keys {
        drops.push((Part::ForeignKey, key.name.clone(), statements.len()));
        statements.push(format!(
            "ALTER TABLE {target} DROP FOREIGN KEY {}",
            q(&key.name)?
        ));
    }
    for check in old_checks {
        drops.push((Part::Check, check.name.clone(), statements.len()));
        statements.push(format!(
            "ALTER TABLE {target} DROP CONSTRAINT {}",
            q(&check.name)?
        ));
    }
    for index in old_indexes {
        drops.push((Part::Index, index.name.clone(), statements.len()));
        statements.push(format!(
            "ALTER TABLE {target} DROP INDEX {}",
            q(&index.name)?
        ));
    }
    let columns = alter_table(
        DdlFlavor::MySql,
        dialect,
        table,
        fields_before,
        fields_after,
    )?;
    // The statement that renames each column.
    let mut renamed_at = Vec::new();
    for (old_name, new_name) in &renames {
        let prefix = format!(
            "ALTER TABLE {target} CHANGE COLUMN {} {} ",
            q(old_name)?,
            q(new_name)?
        );
        if let Some(at) = columns.iter().position(|s| s.starts_with(&prefix)) {
            renamed_at.push((old_name.clone(), new_name.clone(), statements.len() + at));
        }
    }
    statements.extend(columns);
    let dropped_at = |kind: Part, name: &str| {
        drops
            .iter()
            .find(|(k, n, _)| *k == kind && n == name)
            .map(|(_, _, at)| *at)
    };
    // Each changed part: its kind, its name before, and where it is added.
    let mut readded: Vec<(Part, String, usize)> = Vec::new();
    for (index, was) in new_indexes {
        if let Some(was) = was {
            readded.push((Part::Index, was.name.clone(), statements.len()));
        }
        statements.push(format!(
            "ALTER TABLE {target} ADD {}",
            index.definition(dialect)?
        ));
    }
    for (check, was) in new_checks {
        if let Some(was) = was {
            readded.push((Part::Check, was.name.clone(), statements.len()));
        }
        statements.push(format!(
            "ALTER TABLE {target} ADD {}",
            check.definition(dialect)?
        ));
    }
    for (key, was) in new_keys {
        if let Some(was) = was {
            readded.push((Part::ForeignKey, was.name.clone(), statements.len()));
        }
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
        ensure!(
            !before.partitioning.subpartitioned
                || after.partitioning.method == PartitionMethod::None,
            "partition_subpartitions_unsupported"
        );
        statements.push(match after.partitioning.clause(dialect)? {
            Some(clause) => format!("ALTER TABLE {target} {clause}"),
            None => format!("ALTER TABLE {target} REMOVE PARTITIONING"),
        });
    }
    for (trigger, was) in new_triggers {
        if let Some(was) = was {
            readded.push((Part::Trigger, was.name.clone(), statements.len()));
        }
        statements.push(trigger.create(dialect, table)?);
    }
    // The old parts, written from what the database had; one that does not
    // pass the checks of the designer is not written back.
    let mut restores = Vec::new();
    for (kind, name, added) in readded {
        let Some(dropped) = dropped_at(kind, &name) else {
            continue;
        };
        let add = |definition: Result<String>| {
            definition
                .ok()
                .map(|definition| format!("ALTER TABLE {target} ADD {definition}"))
        };
        restores.extend(match kind {
            Part::Index => {
                let Some(index) = before.indexes.iter().find(|i| i.name == name) else {
                    continue;
                };
                let columns: Vec<&str> = index.parts.iter().map(|p| p.column.trim()).collect();
                restores_of(dropped, added, &columns, &renamed_at, |done| {
                    add(index.clone().renamed_parts(done).definition(dialect))
                })
            }
            Part::ForeignKey => {
                let Some(key) = before.foreign_keys.iter().find(|k| k.name == name) else {
                    continue;
                };
                let mut columns: Vec<&str> = key.columns.iter().map(|c| c.trim()).collect();
                if key.refers_to(table) {
                    columns.extend(key.ref_columns.iter().map(|c| c.trim()));
                }
                restores_of(dropped, added, &columns, &renamed_at, |done| {
                    add(key
                        .clone()
                        .renamed_parts(done, table)
                        .definition(dialect, table))
                })
            }
            Part::Check => {
                let Some(check) = before.checks.iter().find(|c| c.name == name) else {
                    continue;
                };
                restores_of(dropped, added, &[], &renamed_at, |_| {
                    add(check.definition(dialect))
                })
            }
            Part::Trigger => {
                let Some(trigger) = before.triggers.iter().find(|t| t.name == name) else {
                    continue;
                };
                restores_of(dropped, added, &[], &renamed_at, |_| {
                    trigger.create(dialect, table).ok()
                })
            }
        });
    }
    Ok((statements, restores))
}

impl IndexDraft {
    /// The index with its columns renamed as `done` says.
    fn renamed_parts(mut self, done: &[(String, String)]) -> Self {
        for part in &mut self.parts {
            if let Some(new) = renamed(done, &part.column) {
                part.column = new.to_owned();
            }
        }
        self
    }
}

impl ForeignKeyDraft {
    /// The key with its columns, and those it points at in its own table,
    /// renamed as `done` says.
    fn renamed_parts(mut self, done: &[(String, String)], table: &TableRef) -> Self {
        let own = self.refers_to(table);
        let rename = |column: &mut String| {
            if let Some(new) = renamed(done, column) {
                *column = new.to_owned();
            }
        };
        self.columns.iter_mut().for_each(rename);
        if own {
            self.ref_columns.iter_mut().for_each(rename);
        }
        self
    }
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
            "SELECT partition_name, partition_method, partition_expression, partition_description, partition_comment, subpartition_method FROM information_schema.partitions WHERE table_schema = {schema} AND table_name = {name} AND partition_name IS NOT NULL ORDER BY partition_ordinal_position, subpartition_ordinal_position"
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

/// The version of the server; [`is_mariadb`] reads it.
pub const VERSION_QUERY: &str = "SELECT VERSION()";

/// The server is MariaDB, by the version it gives (`11.4.2-MariaDB-…`).
pub fn is_mariadb(version: &str) -> bool {
    version.to_ascii_lowercase().contains("mariadb")
}

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
    // A partition divided again has a row for each of its subpartitions.
    let mut partitions: Vec<PartitionDef> = Vec::new();
    for row in rows {
        let name = text(row, 0);
        if partitions.last().is_none_or(|last| last.name != name) {
            partitions.push(PartitionDef {
                name,
                values: text(row, 3),
                comment: text(row, 4),
            });
        }
    }
    let subpartitioned = rows.iter().any(|row| !text(row, 5).is_empty());
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
        subpartitioned,
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
            ..Default::default()
        };
        let (statements, restores) =
            alter_plan(&DIALECT, &table(), &fields, &fields, &before, &after).unwrap();
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
        // What was changed comes back as it was when a statement between
        // its drop and its new version fails; what was removed does not.
        assert_eq!(
            restores,
            [
                Restore {
                    dropped: 3,
                    added: 4,
                    sql: "ALTER TABLE `shop`.`orders` ADD UNIQUE INDEX `by_customer` (`customer`, `note`(20) DESC) COMMENT 'it''s'".into(),
                },
                Restore {
                    dropped: 0,
                    added: 7,
                    sql: "CREATE TRIGGER `shop`.`stamp` BEFORE INSERT ON `shop`.`orders` FOR EACH ROW BEGIN SET NEW.total = 0; END".into(),
                },
            ]
        );
        assert!(!restores[0].needed(3) && restores[0].needed(4) && !restores[0].needed(5));
        assert!(restores[1].needed(1) && restores[1].needed(7) && !restores[1].needed(0));
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
            subpartitioned: false,
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
        // A quote left open in the values would take in the comment, whose
        // own quote then closes it: the rest would be a second statement.
        let listed = Partitioning {
            method: PartitionMethod::List,
            expression: "id".into(),
            partitions: vec![PartitionDef {
                name: "p0".into(),
                values: "1) COMMENT = 'x".into(),
                comment: "); DROP TABLE users; SELECT '".into(),
            }],
            ..Default::default()
        };
        assert_eq!(
            listed.clause(&DIALECT).unwrap_err().to_string(),
            "partition_invalid"
        );
        let check = CheckDraft {
            name: "c".into(),
            expression: r"note <> 'a\'".into(),
            original: None,
        };
        assert_eq!(
            check.definition(&DIALECT).unwrap_err().to_string(),
            "check_invalid"
        );
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
        // Taken away: DEFAULT where the server takes it, 0 where 0 is the
        // server's choice, and nothing where neither is.
        let aria = |settings: &[(&str, &str)]| TableOptions {
            engine: "Aria".into(),
            settings: settings
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            ..Default::default()
        };
        let set = aria(&[
            ("TRANSACTIONAL", "1"),
            ("PAGE_CHECKSUM", "1"),
            ("CHECKSUM", "1"),
            ("MAX_ROWS", "100"),
            ("ROW_FORMAT", "PAGE"),
        ]);
        assert_eq!(
            aria(&[]).sql(Some(&set)).unwrap(),
            ["ROW_FORMAT=DEFAULT", "MAX_ROWS=0"]
        );
        let innodb = |settings: &[(&str, &str)]| TableOptions {
            engine: "InnoDB".into(),
            ..aria(settings)
        };
        let set = innodb(&[
            ("KEY_BLOCK_SIZE", "8"),
            ("STATS_SAMPLE_PAGES", "20"),
            ("STATS_PERSISTENT", "1"),
            ("PAGE_COMPRESSED", "1"),
        ]);
        assert_eq!(
            innodb(&[]).sql(Some(&set)).unwrap(),
            [
                "KEY_BLOCK_SIZE=0",
                "STATS_PERSISTENT=DEFAULT",
                "STATS_SAMPLE_PAGES=DEFAULT"
            ]
        );
        // The server takes at least one page and a level from 1 to 9.
        assert!(innodb(&[("STATS_SAMPLE_PAGES", "0")]).sql(None).is_err());
        assert!(
            innodb(&[("PAGE_COMPRESSION_LEVEL", "0")])
                .sql(None)
                .is_err()
        );
        assert_eq!(
            innodb(&[("STATS_SAMPLE_PAGES", "1")]).sql(None).unwrap(),
            ["ENGINE=InnoDB", "STATS_SAMPLE_PAGES=1"]
        );
        assert!(is_mariadb("11.4.2-MariaDB-ubu2404") && !is_mariadb("8.4.3"));
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

    #[test]
    fn a_trigger_body_is_one_statement_or_one_block() {
        let body = |text: &str| trigger_body(&DIALECT, text).map_err(|e| e.to_string());
        for (text, kept) in [
            ("SET NEW.total = 0", "SET NEW.total = 0"),
            ("SET NEW.total = 0;", "SET NEW.total = 0"),
            ("SET NEW.total = 0; -- reset\n", "SET NEW.total = 0"),
            (
                "BEGIN\n  IF NEW.total IS NULL THEN SET NEW.total = 0; END IF;\nEND;",
                "BEGIN\n  IF NEW.total IS NULL THEN SET NEW.total = 0; END IF;\nEND",
            ),
        ] {
            assert_eq!(body(text).unwrap(), kept, "{text}");
        }
        for text in [
            "SET NEW.kind = CASE WHEN NEW.total > 100 THEN 'big' ELSE 'small' END",
            "SET NEW.kind = CASE NEW.a WHEN 1 THEN IF(NEW.b, 'x', 'y') ELSE 'z' END",
            "BEGIN SET NEW.kind = (CASE WHEN NEW.a THEN 1 END); SET NEW.note = 'a; b END'; END",
            "outer_block: BEGIN\n  DECLARE n INT DEFAULT 0;\n  counting: LOOP\n    SET n = n + 1;\n    IF n > 3 THEN LEAVE counting; END IF;\n  END LOOP counting;\nEND outer_block",
            "BEGIN DECLARE i INT DEFAULT 0; WHILE i < 3 DO SET i = i + 1; END WHILE; END",
            "BEGIN DECLARE i INT DEFAULT 0; REPEAT SET i = i + 1; UNTIL i >= 3 END REPEAT; END",
            "CASE NEW.status WHEN 'new' THEN SET NEW.total = 0; ELSE BEGIN END; END CASE",
            "IF NEW.a < 0 THEN SET NEW.a = 0; ELSEIF NEW.a > 9 THEN SET NEW.a = 9; ELSE IF NEW.b THEN SET NEW.c = 1; END IF; END IF",
            "BEGIN DECLARE CONTINUE HANDLER FOR SQLSTATE '23000', NOT FOUND SET @failed = 1; INSERT INTO log VALUES (NEW.id); END",
            "BEGIN SET @end = NEW.`end`; SET NEW.begin = OLD.end; SET @a = x$end; END",
            "BEGIN NOT ATOMIC SET @a = 1; END",
            "BEGIN FOR i IN 1..3 DO SET @a = i; END FOR; END",
            "SET NEW.note = 'it''s \\'quoted\\''",
        ] {
            assert!(body(text).is_ok(), "{text}: {:?}", body(text));
        }
        for text in [
            // A second statement after a simple one or after the block.
            "SET NEW.updated_at = NOW(); UPDATE orders SET total = 0",
            "BEGIN SET NEW.a = 1; END; UPDATE orders SET total = 0",
            // The server runs the text of these comments.
            "SET NEW.a = 1 /*!50000 ; UPDATE orders SET total = 0 */",
            "SET NEW.a = 1 /*M!100000 ; UPDATE orders SET total = 0 */",
            // IF in an expression is a function, not a block.
            "BEGIN DO IF(1, 0, 0); END; UPDATE orders SET total = 0; END IF",
            "SET NEW.a = CASE WHEN 1 THEN IF(1, 2, 3) END; UPDATE orders SET total = 0; END IF",
            // BEGIN in an expression is a name.
            "SET @x = begin; UPDATE orders SET total = 0; END",
            // After a number the `.` is a decimal point, not a name's.
            "REPEAT SET @a = 1; UNTIL 1.END REPEAT; UPDATE orders SET total = 0; UNTIL 1 END REPEAT",
            // Blocks closed by the wrong word, or not at all.
            "BEGIN SET @a = 1; END IF",
            "BEGIN FOR i IN 1..3 DO SET @a = i; END; END FOR",
            "BEGIN IF NEW.a THEN SET @a = 1; END; END",
            "BEGIN SET @a = 1;",
            "END",
            "SET NEW.a = (1; UPDATE orders SET total = 0)",
            // A string whole only where a backslash escapes.
            "SET @a = 'x\\'; UPDATE orders SET total = 0; -- '",
        ] {
            assert_eq!(body(text).unwrap_err(), "trigger_body_invalid", "{text}");
        }
        assert_eq!(body("DELIMITER //").unwrap_err(), "trigger_invalid");
        assert_eq!(body("  ").unwrap_err(), "trigger_invalid");
    }

    #[test]
    fn a_renamed_column_stays_in_its_index_and_key() {
        let fields = vec![field("id", "int"), field("customer", "int")];
        let mut renamed = fields.clone();
        renamed[1].name = "client".into();
        let before = TableExtras {
            indexes: vec![IndexDraft {
                name: "by_customer".into(),
                parts: vec![IndexPart {
                    column: "customer".into(),
                    ..Default::default()
                }],
                original: Some("by_customer".into()),
                ..Default::default()
            }],
            foreign_keys: vec![ForeignKeyDraft {
                name: "fk_customer".into(),
                columns: vec!["customer".into()],
                ref_schema: "shop".into(),
                ref_table: "customers".into(),
                ref_columns: vec!["id".into()],
                original: Some("fk_customer".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        // The server renames the column in both: only the rename runs.
        let rename = "ALTER TABLE `shop`.`orders` CHANGE COLUMN `customer` `client` int NULL";
        assert_eq!(
            alter(&DIALECT, &table(), &fields, &renamed, &before, &before).unwrap(),
            [rename]
        );
        // The index changed too: it is added with the new name, and put back
        // with the name the column has when the save stops.
        let mut after = before.clone();
        after.indexes[0].parts[0].descending = true;
        let (statements, restores) =
            alter_plan(&DIALECT, &table(), &fields, &renamed, &before, &after).unwrap();
        assert_eq!(
            statements,
            [
                "ALTER TABLE `shop`.`orders` DROP INDEX `by_customer`",
                rename,
                "ALTER TABLE `shop`.`orders` ADD INDEX `by_customer` (`client` DESC)",
            ]
        );
        assert_eq!(
            restores,
            [
                Restore {
                    dropped: 0,
                    added: 1,
                    sql: "ALTER TABLE `shop`.`orders` ADD INDEX `by_customer` (`customer`)".into(),
                },
                Restore {
                    dropped: 1,
                    added: 2,
                    sql: "ALTER TABLE `shop`.`orders` ADD INDEX `by_customer` (`client`)".into(),
                },
            ]
        );
        // The key named by the designer with the new name is the same key.
        let mut after = before.clone();
        after.foreign_keys[0].columns = vec!["client".into()];
        assert_eq!(
            alter(&DIALECT, &table(), &fields, &renamed, &before, &after).unwrap(),
            [rename]
        );
    }

    #[test]
    fn a_key_into_the_same_schema_is_the_same_key() {
        let fields = vec![field("id", "int"), field("customer", "int")];
        let read = TableExtras {
            foreign_keys: vec![ForeignKeyDraft {
                name: "fk_customer".into(),
                columns: vec!["customer".into()],
                ref_schema: "shop".into(),
                ref_table: "customers".into(),
                ref_columns: vec!["id".into()],
                original: Some("fk_customer".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut picked = read.clone();
        picked.foreign_keys[0].ref_schema.clear();
        assert!(
            alter(&DIALECT, &table(), &fields, &fields, &read, &picked)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_save_that_stopped_is_saved_again_against_the_table() {
        let index = |name: &str, original: Option<&str>| IndexDraft {
            name: name.into(),
            parts: vec![IndexPart {
                column: "customer".into(),
                ..Default::default()
            }],
            original: original.map(Into::into),
            ..Default::default()
        };
        let database = TableExtras {
            indexes: vec![
                index("added", Some("added")),
                index("renamed", Some("renamed")),
            ],
            ..Default::default()
        };
        let mut edited = TableExtras {
            indexes: vec![
                index("added", None),
                index("renamed", Some("before_rename")),
                index("fresh", None),
            ],
            ..Default::default()
        };
        edited.adopt_existing(&database);
        let originals: Vec<Option<&str>> = edited
            .indexes
            .iter()
            .map(|i| i.original.as_deref())
            .collect();
        assert_eq!(originals, [Some("added"), Some("renamed"), None]);
        // A part whose original the table no longer has is added.
        let fields = vec![field("id", "int"), field("customer", "int")];
        let lost = TableExtras {
            indexes: vec![index("gone", Some("gone"))],
            ..Default::default()
        };
        assert_eq!(
            alter(
                &DIALECT,
                &table(),
                &fields,
                &fields,
                &TableExtras::default(),
                &lost
            )
            .unwrap(),
            ["ALTER TABLE `shop`.`orders` ADD INDEX `gone` (`customer`)"]
        );
    }

    #[test]
    fn subpartitions_are_read_once_and_not_changed() {
        let row = |cells: [&str; 6]| -> Vec<Cell> {
            cells.iter().map(|c| Cell::Text((*c).into())).collect()
        };
        let rows = vec![
            row(["p0", "RANGE", "id", "100", "", "HASH"]),
            row(["p0", "RANGE", "id", "100", "", "HASH"]),
            row(["p1", "RANGE", "id", "MAXVALUE", "", "HASH"]),
            row(["p1", "RANGE", "id", "MAXVALUE", "", "HASH"]),
        ];
        let read = read_partitions(&rows);
        assert!(read.subpartitioned);
        let names: Vec<&str> = read.partitions.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["p0", "p1"]);
        let fields = vec![field("id", "int")];
        let before = TableExtras {
            partitioning: read.clone(),
            ..Default::default()
        };
        let mut after = before.clone();
        after.partitioning.partitions[0].values = "200".into();
        assert_eq!(
            alter(&DIALECT, &table(), &fields, &fields, &before, &after)
                .unwrap_err()
                .to_string(),
            "partition_subpartitions_unsupported"
        );
        after.partitioning.method = PartitionMethod::None;
        assert_eq!(
            alter(&DIALECT, &table(), &fields, &fields, &before, &after).unwrap(),
            ["ALTER TABLE `shop`.`orders` REMOVE PARTITIONING"]
        );
    }
}
