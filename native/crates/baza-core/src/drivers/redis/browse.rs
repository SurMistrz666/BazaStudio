//! What the side panel lists for a Redis server and what "Dane tabeli" shows
//! for each entry.
//!
//! * `db0`, `db1`, …: the keys of a logical database, one row per key with
//!   its type, lifetime, size and value.
//! * `db0 · hashes`, `· lists`, `· sets`, `· sorted sets`, `· streams`: what
//!   the keys of one type hold, one row per field, element, member or entry,
//!   addressed by the key and the member. Filtered by `key`, such a list is
//!   one hash, list, set, sorted set or stream.
//! * `clients`, `databases`, `server info`, `slow log`: read-only reports of
//!   the server.
//!
//! Keys are found with `SCAN`, never with `KEYS`. A page is a slice of the
//! names found so far; the cursor is kept between pages, so the next page
//! goes on where the previous one stopped and a page that is read again
//! holds the same rows. The values are read anew every time.
use super::link::{Fail, Link, command, value_fail};
use super::reply::{self, Grid};
use super::value::{
    self, Collection, INLINE_MEMBERS_MAX, TEXT_CELL_MAX, name_cell, reply_bytes, reply_int,
    reply_items, reply_pairs, reply_text, value_cell,
};
use crate::builder::{Filter, FilterOp};
use crate::drivers::PageRequest;
use crate::model::{Cell, ColumnInfo, QueryResult, ResultColumn};
use anyhow::{Result, anyhow, ensure};
use redis::{Cmd, Value};
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};

/// Most rows a sorted list is made of: the whole scan is read and ordered
/// here, so it has to stay small.
pub(super) const SORT_LIMIT: usize = 20_000;
/// Most rows of one scan the pages reach.
pub(super) const DEPTH_LIMIT: usize = 100_000;
/// Members of a key read in one step.
const MEMBER_STEP: i64 = 500;
/// Members of a large collection shown in the preview of the key list.
const PREVIEW_MEMBERS: i64 = 12;
/// Separator between the database and what of it a container lists.
const SEPARATOR: &str = " · ";

/// What the rows of a container are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Kind {
    Keys,
    Hash,
    List,
    Set,
    Zset,
    Stream,
}

/// A read-only report of the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum View {
    Clients,
    Databases,
    Info,
    SlowLog,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Container {
    Data(i64, Kind),
    View(View),
}

/// A column of a container.
pub(super) struct Column {
    pub name: &'static str,
    pub data_type: &'static str,
    pub nullable: bool,
    /// Part of what addresses a row.
    pub key: bool,
    pub default: Option<&'static str>,
}

const fn column(name: &'static str, data_type: &'static str, key: bool) -> Column {
    Column {
        name,
        data_type,
        nullable: false,
        key,
        default: None,
    }
}

impl Kind {
    pub(super) const MEMBERS: [Kind; 5] =
        [Kind::Hash, Kind::List, Kind::Set, Kind::Zset, Kind::Stream];

    fn suffix(self) -> &'static str {
        match self {
            Kind::Keys => "",
            Kind::Hash => "hashes",
            Kind::List => "lists",
            Kind::Set => "sets",
            Kind::Zset => "sorted sets",
            Kind::Stream => "streams",
        }
    }

    /// The name Redis has for the type (`TYPE key`, `SCAN … TYPE`).
    pub(super) fn type_name(self) -> &'static str {
        match self {
            Kind::Keys => "",
            Kind::Hash => "hash",
            Kind::List => "list",
            Kind::Set => "set",
            Kind::Zset => "zset",
            Kind::Stream => "stream",
        }
    }

    pub(super) fn columns(self) -> &'static [Column] {
        match self {
            Kind::Keys => KEY_COLUMNS,
            Kind::Hash => HASH_COLUMNS,
            Kind::List => LIST_COLUMNS,
            Kind::Set => SET_COLUMNS,
            Kind::Zset => ZSET_COLUMNS,
            Kind::Stream => STREAM_COLUMNS,
        }
    }
}

const KEY_COLUMNS: &[Column] = &[
    column("key", "string", true),
    Column {
        name: "type",
        data_type: "read only",
        nullable: false,
        key: false,
        default: Some("string"),
    },
    Column {
        name: "ttl",
        data_type: "seconds",
        nullable: true,
        key: false,
        default: None,
    },
    Column {
        name: "size",
        data_type: "bytes or members",
        nullable: true,
        key: false,
        default: None,
    },
    column("value", "string or json", false),
];
const HASH_COLUMNS: &[Column] = &[
    column("key", "string", true),
    column("field", "string", true),
    column("value", "string", false),
];
// The value is part of what addresses an element: an index alone says
// nothing once somebody pushed to the list.
const LIST_COLUMNS: &[Column] = &[
    column("key", "string", true),
    column("index", "integer", true),
    column("value", "string", true),
];
const SET_COLUMNS: &[Column] = &[
    column("key", "string", true),
    column("member", "string", true),
];
const ZSET_COLUMNS: &[Column] = &[
    column("key", "string", true),
    column("member", "string", true),
    column("score", "double", false),
];
const STREAM_COLUMNS: &[Column] = &[
    column("key", "string", true),
    column("id", "string", true),
    column("entry", "json", false),
];

impl View {
    pub(super) const ALL: [View; 4] = [View::Clients, View::Databases, View::Info, View::SlowLog];

    pub(super) fn name(self) -> &'static str {
        match self {
            View::Clients => "clients",
            View::Databases => "databases",
            View::Info => "server info",
            View::SlowLog => "slow log",
        }
    }

    /// The command of the console that shows the same.
    fn command(self) -> &'static str {
        match self {
            View::Clients => "CLIENT LIST",
            View::Databases => "INFO keyspace",
            View::Info => "INFO",
            View::SlowLog => "SLOWLOG GET 128",
        }
    }
}

impl Container {
    pub(super) fn name(self) -> String {
        match self {
            Container::Data(db, Kind::Keys) => format!("db{db}"),
            Container::Data(db, kind) => format!("db{db}{SEPARATOR}{}", kind.suffix()),
            Container::View(view) => view.name().into(),
        }
    }

    pub(super) fn parse(name: &str) -> Result<Container> {
        if let Some(view) = View::ALL.into_iter().find(|view| view.name() == name) {
            return Ok(Container::View(view));
        }
        let (database, suffix) = match name.split_once(SEPARATOR) {
            Some((database, suffix)) => (database, suffix),
            None => (name, ""),
        };
        let db = database
            .strip_prefix("db")
            .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|digits| digits.parse::<i64>().ok())
            .ok_or_else(|| anyhow!("redis_container_unknown"))?;
        let kind = [Kind::Keys]
            .into_iter()
            .chain(Kind::MEMBERS)
            .find(|kind| kind.suffix() == suffix)
            .ok_or_else(|| anyhow!("redis_container_unknown"))?;
        Ok(Container::Data(db, kind))
    }

    /// The columns of the container as the grid asks for them.
    pub(super) fn column_info(self) -> Vec<ColumnInfo> {
        match self {
            Container::Data(_, kind) => kind
                .columns()
                .iter()
                .map(|column| ColumnInfo {
                    name: column.name.into(),
                    data_type: column.data_type.into(),
                    nullable: column.nullable,
                    primary_key: column.key,
                    default: column.default.map(str::to_owned),
                })
                .collect(),
            Container::View(_) => Vec::new(),
        }
    }
}

/// What addresses a row: the key and, in a list of members, the member.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct RowId {
    pub key: Vec<u8>,
    pub member: Member,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Member {
    /// A row of the key list.
    None,
    /// A field of a hash or a member of a set or a sorted set.
    Name(Vec<u8>),
    /// A position in a list.
    Index(i64),
    /// The id of a stream entry.
    Entry(String),
}

/// Write "contains this text" as a pattern of `MATCH`: every character that
/// means something in a pattern is escaped, and a letter matches in either
/// case. `None` for an empty text.
///
/// A pattern is matched byte by byte, so only ASCII letters can be given
/// both cases. A letter of another alphabet becomes "any bytes of this
/// length" (or "anything" when its two cases differ in length); the rows
/// are checked again here, precisely, after the server narrowed them down.
pub(super) fn contains_pattern(needle: &str) -> Option<Vec<u8>> {
    if needle.is_empty() {
        return None;
    }
    let mut pattern = vec![b'*'];
    // The pattern ends in a wildcard that stands for "anything".
    let mut open = true;
    for character in needle.chars() {
        if character.is_ascii_alphabetic() {
            pattern.extend([
                b'[',
                character.to_ascii_lowercase() as u8,
                character.to_ascii_uppercase() as u8,
                b']',
            ]);
            open = false;
        } else if character.is_ascii() {
            if matches!(character, '*' | '?' | '[' | ']' | '\\') {
                pattern.push(b'\\');
            }
            pattern.push(character as u8);
            open = false;
        } else {
            let lower: String = character.to_lowercase().collect();
            let upper: String = character.to_uppercase().collect();
            let mut buffer = [0u8; 4];
            let own = character.encode_utf8(&mut buffer);
            if lower == upper {
                pattern.extend(own.bytes());
                open = false;
            } else if lower.len() == upper.len() && lower.len() == own.len() {
                pattern.extend(std::iter::repeat_n(b'?', own.len()));
                open = false;
            } else if !open {
                pattern.push(b'*');
                open = true;
            }
        }
    }
    if !open {
        pattern.push(b'*');
    }
    Some(pattern)
}

/// Write "exactly this text" as a pattern.
fn exact_pattern(text: &[u8]) -> Vec<u8> {
    let mut pattern = Vec::with_capacity(text.len());
    for byte in text {
        if matches!(byte, b'*' | b'?' | b'[' | b']' | b'\\') {
            pattern.push(b'\\');
        }
        pattern.push(*byte);
    }
    pattern
}

/// One filter, checked on the cell of its column.
#[derive(Clone, Debug)]
pub(super) struct Test {
    pub column: usize,
    op: FilterOp,
    text: String,
    lower: String,
}

impl Test {
    fn new(column: usize, filter: &Filter) -> Test {
        let text = match &filter.value {
            Cell::Null => String::new(),
            other => other.display(),
        };
        Test {
            column,
            op: filter.op.clone(),
            lower: text.to_lowercase(),
            text,
        }
    }

    pub(super) fn passes(&self, cell: &Cell) -> bool {
        let null = matches!(cell, Cell::Null);
        let shown = cell.display();
        let ordered = |wanted: std::cmp::Ordering| {
            if null {
                return false;
            }
            match (shown.trim().parse::<f64>(), self.text.trim().parse::<f64>()) {
                (Ok(left), Ok(right)) => left.partial_cmp(&right) == Some(wanted),
                _ => shown.as_str().cmp(self.text.as_str()) == wanted,
            }
        };
        match self.op {
            // Bytes that are not text are shown as digits: what was typed
            // is looked for in the digits and in the bytes read as text.
            FilterOp::Contains => {
                !null
                    && (shown.to_lowercase().contains(&self.lower)
                        || match cell {
                            Cell::Unsupported(text) => value::unhex(text).is_some_and(|bytes| {
                                String::from_utf8_lossy(&bytes)
                                    .to_lowercase()
                                    .contains(&self.lower)
                            }),
                            _ => false,
                        })
            }
            FilterOp::Equal => !null && shown == self.text,
            FilterOp::NotEqual => null || shown != self.text,
            FilterOp::Greater => ordered(std::cmp::Ordering::Greater),
            FilterOp::Less => ordered(std::cmp::Ordering::Less),
            FilterOp::IsNull => null,
            FilterOp::IsNotNull => !null,
        }
    }
}

/// The types `TYPE` answers with for the keys this driver lists.
const TYPES: [&str; 6] = ["string", "list", "set", "zset", "hash", "stream"];

/// How a page of a container is read: what the server narrows down and what
/// is checked here.
#[derive(Clone, Debug)]
pub(super) struct Plan {
    pub db: i64,
    pub kind: Kind,
    /// Every filter of the request.
    pub tests: Vec<Test>,
    /// `MATCH` of the key scan.
    pub key_match: Option<Vec<u8>>,
    /// The one key a filter names exactly: nothing is scanned.
    pub key_exact: Option<Vec<u8>>,
    /// `MATCH` of the scan of fields or members.
    pub member_match: Option<Vec<u8>>,
    /// `TYPE` of the key scan.
    pub scan_type: Option<&'static str>,
    /// Column and whether the order is descending.
    pub order: Option<(usize, bool)>,
    /// A filter or the order needs more of a key than its name.
    pub needs_details: bool,
    /// A filter looks into the values of the key list.
    pub needs_values: bool,
    /// What tells one scan from another.
    pub signature: String,
}

impl Plan {
    pub(super) fn new(db: i64, kind: Kind, page: &PageRequest) -> Result<Plan> {
        ensure!(
            (1..=20_000).contains(&page.limit) && page.offset <= 1_000_000_000,
            "page_invalid"
        );
        let columns = kind.columns();
        let position = |name: &str| {
            columns
                .iter()
                .position(|column| column.name == name)
                .ok_or_else(|| anyhow!("column_not_found"))
        };
        let mut plan = Plan {
            db,
            kind,
            tests: Vec::new(),
            key_match: None,
            key_exact: None,
            member_match: None,
            scan_type: (kind != Kind::Keys).then(|| kind.type_name()),
            order: None,
            needs_details: false,
            needs_values: false,
            signature: String::new(),
        };
        let mut signature = format!("{db}/{kind:?}");
        for filter in &page.filters {
            let index = position(&filter.column)?;
            let test = Test::new(index, filter);
            signature.push_str(&format!("|{index}:{:?}:{}", filter.op, test.text));
            let name = columns[index].name;
            match (&filter.op, name) {
                (FilterOp::Contains, "key") if plan.key_match.is_none() => {
                    plan.key_match = contains_pattern(&test.text);
                }
                (FilterOp::Equal, "key") if !matches!(filter.value, Cell::Null) => {
                    plan.key_exact = value::bytes_of(&filter.value);
                }
                (FilterOp::Contains, "field" | "member")
                    if plan.member_match.is_none() && matches!(kind, Kind::Hash | Kind::Set) =>
                {
                    plan.member_match = contains_pattern(&test.text);
                }
                (FilterOp::Equal, "field" | "member") if matches!(kind, Kind::Hash | Kind::Set) => {
                    plan.member_match = Some(exact_pattern(test.text.as_bytes()));
                }
                (FilterOp::Contains | FilterOp::Equal, "type") if kind == Kind::Keys => {
                    let found: Vec<&str> = TYPES
                        .into_iter()
                        .filter(|name| match filter.op {
                            FilterOp::Equal => *name == test.text,
                            _ => name.contains(&test.lower),
                        })
                        .collect();
                    if let [only] = found[..] {
                        plan.scan_type = Some(only);
                    }
                    // A module type, or several of them: checked on the rows.
                    plan.needs_details = true;
                }
                _ => {}
            }
            if kind == Kind::Keys && index > 0 {
                plan.needs_details = true;
                plan.needs_values |= name == "value";
            }
            plan.tests.push(test);
        }
        if let Some((name, descending)) = &page.order_by {
            let index = position(name)?;
            if kind == Kind::Keys {
                // Ordering by value would read every value of the database.
                ensure!(columns[index].name != "value", "redis_sort_column");
                plan.needs_details |= index > 0;
            }
            plan.order = Some((index, *descending));
            signature.push_str(&format!("|order {index} {descending}"));
        }
        plan.signature = signature;
        Ok(plan)
    }

    fn accepts(&self, cells: &[Cell]) -> bool {
        self.tests
            .iter()
            .all(|test| cells.get(test.column).is_some_and(|cell| test.passes(cell)))
    }

    /// The command that reads the first step of the scan, for the tab and
    /// the history.
    pub(super) fn text(&self, limit: u32, home: i64) -> String {
        let mut words: Vec<Vec<u8>> = vec![b"SCAN".to_vec(), b"0".to_vec()];
        if let Some(pattern) = self
            .key_match
            .clone()
            .or_else(|| self.key_exact.as_deref().map(exact_pattern))
        {
            words.push(b"MATCH".to_vec());
            words.push(pattern);
        }
        words.push(b"COUNT".to_vec());
        words.push(limit.to_string().into_bytes());
        if let Some(name) = self.scan_type {
            words.push(b"TYPE".to_vec());
            words.push(name.as_bytes().to_vec());
        }
        let scan = super::command::text(words);
        if self.db == home {
            scan
        } else {
            format!("SELECT {}\n{scan}", self.db)
        }
    }
}

/// What a row is ordered by.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum SortKey {
    Number(f64),
    Text(Vec<u8>),
    /// No value: after everything else.
    Missing,
}

impl SortKey {
    fn of(cell: &Cell) -> SortKey {
        match cell {
            Cell::Null => SortKey::Missing,
            Cell::Integer(text) | Cell::Float(text) | Cell::Decimal(text) => {
                text.parse::<f64>().map_or_else(
                    |_| SortKey::Text(text.clone().into_bytes()),
                    SortKey::Number,
                )
            }
            other => {
                let shown = other.display();
                match shown.parse::<f64>() {
                    Ok(number) if matches!(other, Cell::Unsupported(_)) => SortKey::Number(number),
                    _ => SortKey::Text(shown.into_bytes()),
                }
            }
        }
    }

    fn compare(&self, other: &SortKey) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        match (self, other) {
            (SortKey::Number(a), SortKey::Number(b)) => a.partial_cmp(b).unwrap_or(Equal),
            (SortKey::Text(a), SortKey::Text(b)) => a.cmp(b),
            (SortKey::Missing, SortKey::Missing) => Equal,
            (SortKey::Number(_), _) => Less,
            (_, SortKey::Number(_)) => Greater,
            (SortKey::Text(_), SortKey::Missing) => Less,
            (SortKey::Missing, SortKey::Text(_)) => Greater,
        }
    }
}

/// The key whose members are being read, and how far that got.
#[derive(Debug)]
struct Open {
    key: Vec<u8>,
    /// Cursor of `HSCAN` / `SSCAN`; `None` before the first step.
    cursor: Option<Vec<u8>>,
    /// Next position of a list or a sorted set.
    index: i64,
    /// Last id of a stream that was read.
    after: Option<String>,
    /// Fields and members taken so far: a scan may return one twice.
    seen: HashSet<Vec<u8>>,
}

/// A scan in progress: the rows found so far and where to go on.
#[derive(Debug)]
pub(super) struct Scan {
    pub signature: String,
    pub ids: Vec<RowId>,
    sort_keys: Vec<SortKey>,
    seen: HashSet<Vec<u8>>,
    cursor: Vec<u8>,
    keys_done: bool,
    queue: VecDeque<Vec<u8>>,
    open: Option<Open>,
    /// `COUNT` of the next key scan: grows while a filter finds little.
    count: usize,
    pub finished: bool,
    sorted: bool,
    /// When the scan was last used, for the oldest one to make room.
    used: u64,
}

impl Scan {
    pub(super) fn new(signature: String) -> Scan {
        Scan {
            signature,
            ids: Vec::new(),
            sort_keys: Vec::new(),
            seen: HashSet::new(),
            cursor: b"0".to_vec(),
            keys_done: false,
            queue: VecDeque::new(),
            open: None,
            count: 1000,
            finished: false,
            sorted: false,
            used: 0,
        }
    }

    /// Take one step of a key scan: remember the cursor, drop the keys an
    /// earlier step already returned, and ask for more at once next time
    /// when a pattern found little.
    pub(super) fn take_keys(
        &mut self,
        keys: Vec<Vec<u8>>,
        next: &[u8],
        narrowed: bool,
    ) -> Vec<Vec<u8>> {
        self.cursor = next.to_vec();
        self.keys_done = next == b"0";
        let asked = self.count;
        let fresh: Vec<Vec<u8>> = keys
            .into_iter()
            .filter(|key| self.seen.insert(key.clone()))
            .collect();
        if narrowed && fresh.len() * 10 < asked {
            self.count = (self.count * 2).min(10_000);
        }
        fresh
    }

    /// Put keys in front of what the scan will find: the keys made through
    /// the grid a moment ago, which a scan would return anywhere. In a list
    /// of members their members are read first.
    pub(super) fn seed(&mut self, keys: Vec<Vec<u8>>, members: bool) {
        for key in keys {
            if !self.seen.insert(key.clone()) {
                continue;
            }
            if members {
                self.queue.push_back(key);
            } else {
                self.ids.push(RowId {
                    key,
                    member: Member::None,
                });
            }
        }
    }

    fn push(&mut self, id: RowId, sort: Option<SortKey>) {
        self.ids.push(id);
        if let Some(sort) = sort {
            self.sort_keys.push(sort);
        }
    }

    /// Order the rows once the whole scan is read.
    fn sort(&mut self, descending: bool) {
        let mut order: Vec<usize> = (0..self.ids.len()).collect();
        order.sort_by(|a, b| {
            let by_key = self.sort_keys[*a].compare(&self.sort_keys[*b]);
            let by_key = if descending { by_key.reverse() } else { by_key };
            by_key.then_with(|| self.ids[*a].cmp(&self.ids[*b]))
        });
        self.ids = order.iter().map(|index| self.ids[*index].clone()).collect();
        self.sort_keys.clear();
        self.sorted = true;
    }

    /// The rows of a page.
    pub(super) fn page(&self, offset: u64, limit: u32) -> &[RowId] {
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(self.ids.len());
        let end = start.saturating_add(limit as usize).min(self.ids.len());
        &self.ids[start..end]
    }

    /// A row was removed through the grid.
    pub(super) fn forget(&mut self, gone: &RowId) {
        if let Some(index) = self.ids.iter().position(|id| id == gone) {
            self.ids.remove(index);
        }
    }

    /// A key was removed: its row and the rows of its members go.
    pub(super) fn forget_key(&mut self, key: &[u8]) {
        self.ids.retain(|id| id.key != key);
    }

    /// A key or a member got another name: its row stays where it is.
    pub(super) fn rename(&mut self, from: &RowId, to: RowId) {
        if let Some(id) = self.ids.iter_mut().find(|id| *id == from) {
            *id = to;
        }
    }

    /// A key got another name: every row of it follows.
    pub(super) fn rename_key(&mut self, from: &[u8], to: &[u8]) {
        for id in self.ids.iter_mut().filter(|id| id.key == from) {
            id.key = to.to_vec();
        }
        if self.seen.remove(from) {
            self.seen.insert(to.to_vec());
        }
    }
}

/// The scans of a session: the last few, so that going back to a list
/// continues it.
#[derive(Default)]
pub(super) struct Scans {
    scans: Vec<Scan>,
    clock: u64,
}

impl Scans {
    /// The scan of a plan. `restart` forgets what was found: the first page
    /// always reads the keyspace as it is now.
    pub(super) fn of(&mut self, plan: &Plan, restart: bool) -> &mut Scan {
        self.clock += 1;
        let found = self
            .scans
            .iter()
            .position(|scan| scan.signature == plan.signature);
        let index = match found {
            Some(index) if !restart => index,
            Some(index) => {
                self.scans[index] = Scan::new(plan.signature.clone());
                index
            }
            None => {
                if self.scans.len() >= 8
                    && let Some(oldest) = (0..self.scans.len()).min_by_key(|i| self.scans[*i].used)
                {
                    self.scans.remove(oldest);
                }
                self.scans.push(Scan::new(plan.signature.clone()));
                self.scans.len() - 1
            }
        };
        self.scans[index].used = self.clock;
        &mut self.scans[index]
    }

    /// Every scan of a database, for a change made through the grid.
    pub(super) fn each(&mut self, db: i64, mut change: impl FnMut(&mut Scan)) {
        let prefix = format!("{db}/");
        for scan in &mut self.scans {
            if scan.signature.starts_with(&prefix) {
                change(scan);
            }
        }
    }
}

fn check(value: Value) -> Result<Value, Fail> {
    match value {
        Value::ServerError(error) => Err(value_fail(&error)),
        other => Ok(other),
    }
}

/// One step of the key scan of a plan.
async fn scan_keys(
    link: &Link,
    plan: &Plan,
    scan: &mut Scan,
    typed_scan: &AtomicBool,
) -> Result<Vec<Vec<u8>>, Fail> {
    if let Some(key) = &plan.key_exact {
        scan.keys_done = true;
        return Ok(if scan.seen.insert(key.clone()) {
            vec![key.clone()]
        } else {
            Vec::new()
        });
    }
    let words = |typed: bool| {
        let mut words: Vec<Vec<u8>> = vec![b"SCAN".to_vec(), scan.cursor.clone()];
        if let Some(pattern) = &plan.key_match {
            words.push(b"MATCH".to_vec());
            words.push(pattern.clone());
        }
        words.push(b"COUNT".to_vec());
        words.push(scan.count.to_string().into_bytes());
        if let (true, Some(name)) = (typed, plan.scan_type) {
            words.push(b"TYPE".to_vec());
            words.push(name.as_bytes().to_vec());
        }
        command(words)
    };
    let mut typed = plan.scan_type.is_some() && typed_scan.load(Ordering::Relaxed);
    let reply = match link.call(&words(typed)).await {
        // A server from before `SCAN … TYPE`: ask for the types instead.
        Err(fail) if typed && fail.is_unknown_or_syntax() => {
            typed_scan.store(false, Ordering::Relaxed);
            typed = false;
            link.call(&words(false)).await?
        }
        other => other?,
    };
    let mut parts = reply_items(reply).ok_or(Fail::Io)?.into_iter();
    let next = parts
        .next()
        .as_ref()
        .and_then(reply_bytes)
        .ok_or(Fail::Io)?;
    let keys: Vec<Vec<u8>> = parts
        .next()
        .and_then(reply_items)
        .ok_or(Fail::Io)?
        .iter()
        .filter_map(reply_bytes)
        .collect();
    let narrowed = plan.key_match.is_some() || plan.scan_type.is_some();
    let mut fresh = scan.take_keys(keys, &next, narrowed);
    if let (false, Some(name)) = (typed, plan.scan_type) {
        let types = link
            .pipe(
                fresh
                    .iter()
                    .map(|key| command([b"TYPE".as_slice(), key]))
                    .collect(),
            )
            .await?;
        let mut types = types.iter();
        fresh.retain(|_| types.next().and_then(reply_text).as_deref() == Some(name));
    }
    Ok(fresh)
}

/// The rows of the next members of the key that is open. Returns whether
/// the key has more.
async fn next_members(
    link: &Link,
    plan: &Plan,
    open: &mut Open,
) -> Result<(Vec<(RowId, Vec<Cell>)>, bool), Fail> {
    let key = open.key.clone();
    let key_cell = name_cell(&key);
    let row = |member: Member, cells: Vec<Cell>| {
        let mut all = Vec::with_capacity(cells.len() + 1);
        all.push(key_cell.clone());
        all.extend(cells);
        (
            RowId {
                key: key.clone(),
                member,
            },
            all,
        )
    };
    let mut rows = Vec::new();
    let reply = match plan.kind {
        Kind::Hash | Kind::Set => {
            let mut words: Vec<Vec<u8>> = vec![
                if plan.kind == Kind::Hash {
                    b"HSCAN".to_vec()
                } else {
                    b"SSCAN".to_vec()
                },
                key.clone(),
                open.cursor.clone().unwrap_or_else(|| b"0".to_vec()),
            ];
            if let Some(pattern) = &plan.member_match {
                words.push(b"MATCH".to_vec());
                words.push(pattern.clone());
            }
            words.push(b"COUNT".to_vec());
            words.push(MEMBER_STEP.to_string().into_bytes());
            link.call(&command(words)).await
        }
        Kind::Zset => {
            let stop = (open.index + MEMBER_STEP - 1).to_string();
            link.call(&command([
                b"ZRANGE".as_slice(),
                &key,
                open.index.to_string().as_bytes(),
                stop.as_bytes(),
                b"WITHSCORES",
            ]))
            .await
        }
        Kind::List => {
            let stop = (open.index + MEMBER_STEP - 1).to_string();
            link.call(&command([
                b"LRANGE".as_slice(),
                &key,
                open.index.to_string().as_bytes(),
                stop.as_bytes(),
            ]))
            .await
        }
        Kind::Stream => {
            let start = open.after.as_deref().map_or("-".to_owned(), next_entry_id);
            link.call(&command([
                b"XRANGE".as_slice(),
                &key,
                start.as_bytes(),
                b"+",
                b"COUNT",
                MEMBER_STEP.to_string().as_bytes(),
            ]))
            .await
        }
        Kind::Keys => return Ok((rows, false)),
    };
    let reply = match reply {
        Ok(reply) => reply,
        // The key changed its type or is gone since the scan saw it.
        Err(fail) if fail.is_server("WRONGTYPE") => return Ok((rows, false)),
        Err(fail) => return Err(fail),
    };
    let more = match plan.kind {
        Kind::Hash | Kind::Set => {
            let mut parts = reply_items(reply).ok_or(Fail::Io)?.into_iter();
            let next = parts
                .next()
                .as_ref()
                .and_then(reply_bytes)
                .ok_or(Fail::Io)?;
            let items = parts.next().ok_or(Fail::Io)?;
            if plan.kind == Kind::Hash {
                for (field, value) in reply_pairs(items).ok_or(Fail::Io)? {
                    let (Some(field), Some(value)) = (reply_bytes(&field), reply_bytes(&value))
                    else {
                        continue;
                    };
                    if open.seen.insert(field.clone()) {
                        let cells = vec![name_cell(&field), value_cell(&value)];
                        rows.push(row(Member::Name(field), cells));
                    }
                }
            } else {
                for member in reply_items(items)
                    .ok_or(Fail::Io)?
                    .iter()
                    .filter_map(reply_bytes)
                {
                    if open.seen.insert(member.clone()) {
                        let cells = vec![name_cell(&member)];
                        rows.push(row(Member::Name(member), cells));
                    }
                }
            }
            open.cursor = Some(next.clone());
            next != b"0"
        }
        Kind::Zset => {
            let pairs = reply_pairs(reply).ok_or(Fail::Io)?;
            let count = pairs.len() as i64;
            for (member, score) in pairs {
                let (Some(member), Some(score)) = (reply_bytes(&member), reply_text(&score)) else {
                    continue;
                };
                let cells = vec![name_cell(&member), Cell::Float(score)];
                rows.push(row(Member::Name(member), cells));
            }
            open.index += MEMBER_STEP;
            count == MEMBER_STEP
        }
        Kind::List => {
            let items = reply_items(reply).ok_or(Fail::Io)?;
            let count = items.len() as i64;
            for (offset, item) in items.iter().enumerate() {
                let Some(item) = reply_bytes(item) else {
                    continue;
                };
                let index = open.index + offset as i64;
                let cells = vec![Cell::Integer(index.to_string()), value_cell(&item)];
                rows.push(row(Member::Index(index), cells));
            }
            open.index += MEMBER_STEP;
            count == MEMBER_STEP
        }
        Kind::Stream => {
            let items = reply_items(reply).ok_or(Fail::Io)?;
            let count = items.len() as i64;
            for item in items {
                let Some((id, fields)) = reply::entry(item) else {
                    continue;
                };
                let cells = vec![
                    Cell::Text(id.clone()),
                    Cell::Unsupported(value::entry_json(&fields)),
                ];
                open.after = Some(id.clone());
                rows.push(row(Member::Entry(id), cells));
            }
            count == MEMBER_STEP
        }
        Kind::Keys => false,
    };
    Ok((rows, more))
}

/// The smallest id after this one: where the next step of `XRANGE` starts.
fn next_entry_id(id: &str) -> String {
    let (time, sequence) = id.split_once('-').unwrap_or((id, "0"));
    match (time.parse::<u64>(), sequence.parse::<u64>()) {
        (Ok(time), Ok(u64::MAX)) => format!("{}-0", time.saturating_add(1)),
        (Ok(time), Ok(sequence)) => format!("{time}-{}", sequence + 1),
        _ => format!("({id}"),
    }
}

/// Read on until `need` rows are known or the scan is at its end; a sorted
/// scan is read whole and ordered.
pub(super) async fn fill(
    link: &Link,
    plan: &Plan,
    scan: &mut Scan,
    need: usize,
    typed_scan: &AtomicBool,
) -> Result<()> {
    let limit = if plan.order.is_some() {
        SORT_LIMIT
    } else {
        DEPTH_LIMIT
    };
    let over = if plan.order.is_some() {
        "redis_sort_limit"
    } else {
        "redis_page_too_deep"
    };
    ensure!(plan.order.is_some() || need <= limit, over);
    let sort_column = plan.order.map(|(column, _)| column);
    while !scan.finished && (plan.order.is_some() || scan.ids.len() < need) {
        ensure!(scan.ids.len() <= limit, over);
        if plan.kind == Kind::Keys {
            if scan.keys_done {
                scan.finished = true;
                break;
            }
            let keys = scan_keys(link, plan, scan, typed_scan).await?;
            if plan.needs_details {
                let rows = key_rows(link, &keys, plan.needs_values).await?;
                for (key, cells) in keys.into_iter().zip(rows) {
                    let Some(cells) = cells else { continue };
                    if plan.accepts(&cells) {
                        let sort = sort_column.map(|column| SortKey::of(&cells[column]));
                        scan.push(
                            RowId {
                                key,
                                member: Member::None,
                            },
                            sort,
                        );
                    }
                }
            } else {
                for key in keys {
                    let cell = name_cell(&key);
                    if plan.accepts(std::slice::from_ref(&cell)) {
                        let sort = sort_column.map(|_| SortKey::Text(key.clone()));
                        scan.push(
                            RowId {
                                key,
                                member: Member::None,
                            },
                            sort,
                        );
                    }
                }
            }
            continue;
        }
        let Some(open) = scan.open.as_mut() else {
            match scan.queue.pop_front() {
                Some(key) => {
                    scan.open = Some(Open {
                        key,
                        cursor: None,
                        index: 0,
                        after: None,
                        seen: HashSet::new(),
                    });
                }
                None if scan.keys_done => scan.finished = true,
                None => {
                    let keys = scan_keys(link, plan, scan, typed_scan).await?;
                    // The name of the key is checked before its members are read.
                    let test_key = |key: &Vec<u8>| {
                        let cell = name_cell(key);
                        plan.tests
                            .iter()
                            .filter(|test| test.column == 0)
                            .all(|test| test.passes(&cell))
                    };
                    scan.queue.extend(keys.into_iter().filter(test_key));
                }
            }
            continue;
        };
        let (rows, more) = next_members(link, plan, open).await?;
        if !more {
            scan.open = None;
        }
        for (id, cells) in rows {
            if plan.accepts(&cells) {
                let sort = sort_column.map(|column| SortKey::of(&cells[column]));
                scan.push(id, sort);
            }
        }
    }
    ensure!(scan.ids.len() <= limit || plan.order.is_none(), over);
    if let (Some((_, descending)), false) = (plan.order, scan.sorted) {
        scan.sort(descending);
    }
    Ok(())
}

/// The first members of a collection, or all of them when it is small, as
/// the command that reads them.
fn collection_command(type_name: &str, key: &[u8], size: i64) -> Option<Cmd> {
    let whole = size <= INLINE_MEMBERS_MAX as i64;
    let last = if whole {
        "-1".to_owned()
    } else {
        (PREVIEW_MEMBERS - 1).to_string()
    };
    let count = PREVIEW_MEMBERS.to_string();
    Some(match (type_name, whole) {
        ("list", _) => command([b"LRANGE".as_slice(), key, b"0", last.as_bytes()]),
        ("zset", _) => command([
            b"ZRANGE".as_slice(),
            key,
            b"0",
            last.as_bytes(),
            b"WITHSCORES",
        ]),
        ("hash", true) => command([b"HGETALL".as_slice(), key]),
        ("hash", false) => command([b"HSCAN".as_slice(), key, b"0", b"COUNT", count.as_bytes()]),
        ("set", true) => command([b"SMEMBERS".as_slice(), key]),
        ("set", false) => command([b"SSCAN".as_slice(), key, b"0", b"COUNT", count.as_bytes()]),
        _ => return None,
    })
}

/// A collection out of the reply of [`collection_command`].
pub(super) fn collection_of(type_name: &str, whole: bool, reply: Value) -> Option<Collection> {
    let bytes = |values: Vec<Value>| values.iter().filter_map(reply_bytes).collect::<Vec<_>>();
    // A scan step answers with its cursor first.
    let body = |reply: Value| {
        if whole {
            Some(reply)
        } else {
            reply_items(reply)?.into_iter().nth(1)
        }
    };
    Some(match type_name {
        "list" => Collection::List(bytes(reply_items(reply)?)),
        "set" => Collection::Set(bytes(reply_items(body(reply)?)?)),
        "hash" => Collection::Hash(
            reply_pairs(body(reply)?)?
                .iter()
                .filter_map(|(name, value)| Some((reply_bytes(name)?, reply_bytes(value)?)))
                .collect(),
        ),
        "zset" => Collection::Zset(
            reply_pairs(reply)?
                .iter()
                .filter_map(|(member, score)| Some((reply_bytes(member)?, reply_text(score)?)))
                .collect(),
        ),
        _ => return None,
    })
}

/// Read a whole collection, for a change that replaces it.
pub(super) async fn whole_collection(
    link: &Link,
    type_name: &str,
    key: &[u8],
) -> Result<Option<Collection>, Fail> {
    let Some(cmd) = collection_command(type_name, key, 0) else {
        return Ok(None);
    };
    Ok(collection_of(type_name, true, link.call(&cmd).await?))
}

/// The rows of the key list for these keys: type, lifetime and size, and
/// the value when `values` is set. `None` for a key that is gone.
pub(super) async fn key_rows(
    link: &Link,
    keys: &[Vec<u8>],
    values: bool,
) -> Result<Vec<Option<Vec<Cell>>>, Fail> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let first = link
        .pipe(
            keys.iter()
                .flat_map(|key| {
                    [
                        command([b"TYPE".as_slice(), key]),
                        command([b"TTL".as_slice(), key]),
                    ]
                })
                .collect(),
        )
        .await?;
    let facts: Vec<(String, i64)> = first
        .chunks(2)
        .map(|pair| {
            (
                reply_text(&pair[0]).unwrap_or_else(|| "none".into()),
                pair.get(1).and_then(reply_int).unwrap_or(-2),
            )
        })
        .collect();
    let length = |type_name: &str| match type_name {
        "string" => Some("STRLEN"),
        "list" => Some("LLEN"),
        "hash" => Some("HLEN"),
        "set" => Some("SCARD"),
        "zset" => Some("ZCARD"),
        "stream" => Some("XLEN"),
        _ => None,
    };
    let sized: Vec<usize> = (0..keys.len())
        .filter(|index| length(&facts[*index].0).is_some())
        .collect();
    let lengths = link
        .pipe(
            sized
                .iter()
                .map(|index| {
                    command([
                        length(&facts[*index].0).unwrap_or("STRLEN").as_bytes(),
                        &keys[*index],
                    ])
                })
                .collect(),
        )
        .await?;
    let mut sizes: Vec<Option<i64>> = vec![None; keys.len()];
    for (index, reply) in sized.iter().zip(&lengths) {
        sizes[*index] = reply_int(reply);
    }
    // What reads the value, or the start of it, of each key.
    let mut readers: Vec<(usize, Cmd)> = Vec::new();
    if values {
        for (index, key) in keys.iter().enumerate() {
            let size = sizes[index].unwrap_or(0);
            let cmd = match facts[index].0.as_str() {
                "string" if size as usize <= TEXT_CELL_MAX => command([b"GET".as_slice(), key]),
                "string" => command([b"GETRANGE".as_slice(), key, b"0", b"719"]),
                "stream" => command([b"XREVRANGE".as_slice(), key, b"+", b"-", b"COUNT", b"3"]),
                "ReJSON-RL" => command([b"JSON.GET".as_slice(), key]),
                other => match collection_command(other, key, size) {
                    Some(cmd) => cmd,
                    None => continue,
                },
            };
            readers.push((index, cmd));
        }
    }
    let read = link
        .pipe(readers.iter().map(|(_, cmd)| cmd.clone()).collect())
        .await?;
    let mut shown: Vec<Option<Cell>> = vec![None; keys.len()];
    for ((index, _), reply) in readers.iter().zip(read) {
        let size = sizes[*index].unwrap_or(0);
        let type_name = facts[*index].0.as_str();
        // A key that changed between two steps reads as gone or as an
        // error: it is shown without a value this time.
        let Ok(reply) = check(reply) else { continue };
        shown[*index] = match type_name {
            "string" => match reply_bytes(&reply) {
                Some(bytes) if size as usize <= TEXT_CELL_MAX => Some(value_cell(&bytes)),
                Some(bytes) => Some(Cell::Unsupported(match value::text_of(&bytes) {
                    Some(text) => value::preview(text, size as usize),
                    None => value::preview(
                        &match std::str::from_utf8(&bytes) {
                            // Cut inside a character: the text before it.
                            Err(error) if error.valid_up_to() + 4 > bytes.len() => {
                                String::from_utf8_lossy(&bytes[..error.valid_up_to()]).into_owned()
                            }
                            _ => value::hex(&bytes[..bytes.len().min(120)]),
                        },
                        size as usize,
                    ),
                })),
                None => None,
            },
            "stream" => {
                let entries: Vec<String> = reply_items(reply)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(reply::entry)
                    .map(|(id, fields)| format!("{id} {}", value::entry_json(&fields)))
                    .collect();
                let text = entries.join(", ");
                Some(Cell::Unsupported(if size > entries.len() as i64 {
                    value::preview_of(&text)
                } else {
                    text
                }))
            }
            "ReJSON-RL" => reply_bytes(&reply).map(|bytes| match value::text_of(&bytes) {
                Some(text) if bytes.len() <= TEXT_CELL_MAX => Cell::Json(text.to_owned()),
                Some(text) => Cell::Unsupported(value::preview(text, bytes.len())),
                None => Cell::Unsupported(value::hex(&bytes[..bytes.len().min(120)])),
            }),
            other => {
                let whole = size <= INLINE_MEMBERS_MAX as i64;
                collection_of(other, whole, reply)
                    .map(|collection| collection.cell(size.max(0) as usize))
            }
        };
    }
    Ok(keys
        .iter()
        .enumerate()
        .map(|(index, key)| {
            let (type_name, ttl) = &facts[index];
            if type_name == "none" || *ttl == -2 {
                return None;
            }
            Some(vec![
                name_cell(key),
                // What follows from the value is shown, never typed over.
                Cell::Unsupported(type_name.clone()),
                if *ttl < 0 {
                    Cell::Null
                } else {
                    Cell::Integer(ttl.to_string())
                },
                sizes[index].map_or(Cell::Null, |size| Cell::Unsupported(size.to_string())),
                shown[index].take().unwrap_or_else(|| {
                    Cell::Unsupported(if values {
                        format!("<{type_name}>")
                    } else {
                        String::new()
                    })
                }),
            ])
        })
        .collect())
}

/// The rows of these ids as the server holds them now. `None` for a row
/// that is gone.
pub(super) async fn rows(
    link: &Link,
    kind: Kind,
    ids: &[RowId],
) -> Result<Vec<Option<Vec<Cell>>>, Fail> {
    if kind == Kind::Keys {
        let keys: Vec<Vec<u8>> = ids.iter().map(|id| id.key.clone()).collect();
        return key_rows(link, &keys, true).await;
    }
    let commands = ids
        .iter()
        .map(|id| match (kind, &id.member) {
            (Kind::Hash, Member::Name(field)) => command([b"HGET".as_slice(), &id.key, field]),
            (Kind::Set, Member::Name(member)) => {
                command([b"SISMEMBER".as_slice(), &id.key, member])
            }
            (Kind::Zset, Member::Name(member)) => command([b"ZSCORE".as_slice(), &id.key, member]),
            (Kind::List, Member::Index(index)) => {
                command([b"LINDEX".as_slice(), &id.key, index.to_string().as_bytes()])
            }
            (Kind::Stream, Member::Entry(entry)) => command([
                b"XRANGE".as_slice(),
                &id.key,
                entry.as_bytes(),
                entry.as_bytes(),
            ]),
            _ => command(["PING"]),
        })
        .collect();
    let replies = link.pipe(commands).await?;
    Ok(ids
        .iter()
        .zip(replies)
        .map(|(id, reply)| {
            let key = name_cell(&id.key);
            match (kind, &id.member, reply) {
                (_, _, Value::Nil | Value::ServerError(_)) => None,
                (Kind::Hash, Member::Name(field), reply) => Some(vec![
                    key,
                    name_cell(field),
                    value_cell(&reply_bytes(&reply)?),
                ]),
                (Kind::Set, Member::Name(member), reply) => {
                    (reply_int(&reply) == Some(1)).then(|| vec![key, name_cell(member)])
                }
                (Kind::Zset, Member::Name(member), reply) => Some(vec![
                    key,
                    name_cell(member),
                    Cell::Float(reply_text(&reply)?),
                ]),
                (Kind::List, Member::Index(index), reply) => Some(vec![
                    key,
                    Cell::Integer(index.to_string()),
                    value_cell(&reply_bytes(&reply)?),
                ]),
                (Kind::Stream, Member::Entry(_), reply) => {
                    let (id, fields) = reply_items(reply)?
                        .into_iter()
                        .next()
                        .and_then(reply::entry)?;
                    Some(vec![
                        key,
                        Cell::Text(id),
                        Cell::Unsupported(value::entry_json(&fields)),
                    ])
                }
                _ => None,
            }
        })
        .collect())
}

/// A page of a container of data as a result.
pub(super) fn result_of(kind: Kind, rows: Vec<Vec<Cell>>) -> QueryResult {
    QueryResult {
        columns: kind
            .columns()
            .iter()
            .map(|column| ResultColumn {
                name: column.name.into(),
                data_type: column.data_type.into(),
            })
            .collect(),
        rows,
        ..QueryResult::default()
    }
}

/// The logical databases that hold keys, from `INFO keyspace`: index, keys
/// and keys with a lifetime.
pub(super) fn databases(info: &[(String, String, String)]) -> Vec<(i64, i64, i64, i64)> {
    info.iter()
        .filter(|(section, _, _)| section.eq_ignore_ascii_case("keyspace"))
        .filter_map(|(_, name, value)| {
            let db = name.strip_prefix("db")?.parse().ok()?;
            let field = |wanted: &str| {
                value
                    .split(',')
                    .filter_map(|part| part.split_once('='))
                    .find(|(name, _)| *name == wanted)
                    .and_then(|(_, number)| number.parse::<i64>().ok())
                    .unwrap_or(0)
            };
            Some((db, field("keys"), field("expires"), field("avg_ttl")))
        })
        .collect()
}

/// A report of the server, read whole.
pub(super) async fn view(link: &Link, view: View, home: i64) -> Result<Grid, Fail> {
    let text = |reply: Value| reply_text(&reply).unwrap_or_default();
    Ok(match view {
        View::Clients => reply::clients(&text(link.call(&command(["CLIENT", "LIST"])).await?)),
        View::Info => reply::render("INFO", "", &[], link.call(&command(["INFO"])).await?),
        View::SlowLog => {
            let reply = link.call(&command(["SLOWLOG", "GET", "128"])).await?;
            reply::slow_log(&reply_items(reply).unwrap_or_default())
        }
        View::Databases => {
            let info = reply::info(&text(link.call(&command(["INFO", "keyspace"])).await?));
            let mut found = databases(&info);
            if !found.iter().any(|(db, ..)| *db == home) {
                found.push((home, 0, 0, 0));
            }
            found.sort();
            Grid {
                columns: ["database", "keys", "expiring keys", "average ttl ms"]
                    .map(String::from)
                    .to_vec(),
                rows: found
                    .into_iter()
                    .map(|(db, keys, expires, ttl)| {
                        vec![
                            Cell::Text(format!("db{db}")),
                            Cell::Integer(keys.to_string()),
                            Cell::Integer(expires.to_string()),
                            Cell::Integer(ttl.to_string()),
                        ]
                    })
                    .collect(),
            }
        }
    })
}

/// Filter, order and cut a report that was read whole.
pub(super) fn view_page(mut grid: Grid, page: &PageRequest) -> Result<QueryResult> {
    ensure!(
        (1..=20_000).contains(&page.limit) && page.offset <= 1_000_000_000,
        "page_invalid"
    );
    let position = |name: &str| {
        grid.columns
            .iter()
            .position(|column| column == name)
            .ok_or_else(|| anyhow!("column_not_found"))
    };
    let tests = page
        .filters
        .iter()
        .map(|filter| Ok(Test::new(position(&filter.column)?, filter)))
        .collect::<Result<Vec<_>>>()?;
    grid.rows.retain(|row| {
        tests
            .iter()
            .all(|test| row.get(test.column).is_some_and(|cell| test.passes(cell)))
    });
    if let Some((name, descending)) = &page.order_by {
        let column = position(name)?;
        grid.rows.sort_by(|a, b| {
            let order = SortKey::of(&a[column]).compare(&SortKey::of(&b[column]));
            if *descending { order.reverse() } else { order }
        });
    }
    let start = usize::try_from(page.offset)
        .unwrap_or(usize::MAX)
        .min(grid.rows.len());
    grid.rows.drain(..start);
    grid.rows.truncate(page.limit as usize);
    Ok(grid.finish())
}

/// What reading a page looks like as text.
pub(super) fn page_text(page: &PageRequest, home: i64) -> Result<String> {
    Ok(match Container::parse(&page.container.name)? {
        Container::View(view) => view.command().into(),
        Container::Data(db, kind) => Plan::new(db, kind, page)?.text(page.limit, home),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TableRef;

    fn request(name: &str) -> PageRequest {
        PageRequest::first(TableRef {
            schema: String::new(),
            name: name.into(),
            kind: "table".into(),
        })
    }
    fn contains(column: &str, text: &str) -> Filter {
        Filter {
            column: column.into(),
            op: FilterOp::Contains,
            value: Cell::Text(text.into()),
        }
    }

    #[test]
    fn containers_have_names_that_read_back() {
        for (container, name) in [
            (Container::Data(0, Kind::Keys), "db0"),
            (Container::Data(15, Kind::Keys), "db15"),
            (Container::Data(0, Kind::Hash), "db0 · hashes"),
            (Container::Data(2, Kind::List), "db2 · lists"),
            (Container::Data(0, Kind::Set), "db0 · sets"),
            (Container::Data(0, Kind::Zset), "db0 · sorted sets"),
            (Container::Data(3, Kind::Stream), "db3 · streams"),
            (Container::View(View::Clients), "clients"),
            (Container::View(View::SlowLog), "slow log"),
        ] {
            assert_eq!(container.name(), name);
            assert_eq!(Container::parse(name).unwrap(), container);
        }
        for bad in [
            "",
            "db",
            "dbx",
            "db-1",
            "db0 · tables",
            "users",
            "db0·hashes",
        ] {
            assert_eq!(
                Container::parse(bad).unwrap_err().to_string(),
                "redis_container_unknown",
                "{bad}"
            );
        }
        // Every list of data has a complete key, so the grid edits it; a
        // report has none.
        for kind in [Kind::Keys].into_iter().chain(Kind::MEMBERS) {
            let columns = Container::Data(0, kind).column_info();
            assert!(columns[0].primary_key && columns[0].name == "key");
            assert!(columns.iter().all(|column| !column.data_type.is_empty()));
        }
        assert!(Container::View(View::Info).column_info().is_empty());
    }

    #[test]
    fn a_contains_filter_becomes_a_match_pattern() {
        let pattern = |text: &str| String::from_utf8(contains_pattern(text).unwrap()).unwrap();
        assert_eq!(pattern("user"), "*[uU][sS][eE][rR]*");
        assert_eq!(pattern("Cart:42"), "*[cC][aA][rR][tT]:42*");
        // What a pattern would read as a wildcard is taken literally.
        assert_eq!(pattern("100%*?[x]\\"), "*100%\\*\\?\\[[xX]\\]\\\\*");
        // A star of the text at its end is still followed by "anything".
        assert_eq!(pattern("50*"), "*50\\**");
        // Letters outside ASCII: any bytes of that length, checked here.
        assert_eq!(pattern("żółw"), "*??????[wW]*");
        assert_eq!(pattern("🦀"), "*🦀*");
        assert_eq!(pattern("aßb"), "*[aA]??[bB]*");
        // A letter whose two cases differ in length: anything.
        assert_eq!(pattern("İ"), "*");
        assert_eq!(pattern("aİb"), "*[aA]*[bB]*");
        assert!(contains_pattern("").is_none());
        assert_eq!(exact_pattern(b"a*b?"), b"a\\*b\\?");
    }

    #[test]
    fn filters_are_checked_like_the_grid_shows_the_cells() {
        let test = |op: FilterOp, text: &str| {
            Test::new(
                0,
                &Filter {
                    column: "key".into(),
                    op,
                    value: Cell::Text(text.into()),
                },
            )
        };
        let text = |value: &str| Cell::Text(value.into());
        assert!(test(FilterOp::Contains, "GĘŚL").passes(&text("zażółć gęślą")));
        assert!(test(FilterOp::Contains, "19").passes(&Cell::Integer("119".into())));
        assert!(!test(FilterOp::Contains, "x").passes(&Cell::Null));
        assert!(test(FilterOp::Contains, "ff0").passes(&Cell::Unsupported("0xff00".into())));
        // Text around bytes that are not text is still found.
        assert!(test(FilterOp::Contains, "BIN").passes(&Cell::Unsupported("0x62696eff00".into())));
        assert!(!test(FilterOp::Contains, "key").passes(&Cell::Unsupported("0x62696eff00".into())));
        assert!(
            test(FilterOp::Equal, "a").passes(&text("a"))
                && !test(FilterOp::Equal, "a").passes(&text("A"))
        );
        assert!(test(FilterOp::NotEqual, "a").passes(&Cell::Null));
        assert!(test(FilterOp::Greater, "9").passes(&Cell::Integer("10".into())));
        assert!(test(FilterOp::Less, "b").passes(&text("a")));
        assert!(test(FilterOp::IsNull, "").passes(&Cell::Null));
        assert!(test(FilterOp::IsNotNull, "").passes(&text("")));
    }

    #[test]
    fn a_plan_lets_the_server_narrow_what_it_can() {
        let mut page = request("db0");
        page.filters = vec![contains("key", "user"), contains("type", "hash")];
        let plan = Plan::new(0, Kind::Keys, &page).unwrap();
        assert_eq!(
            plan.key_match.as_deref(),
            Some(b"*[uU][sS][eE][rR]*".as_slice())
        );
        assert_eq!(plan.scan_type, Some("hash"));
        assert!(plan.needs_details && !plan.needs_values);
        assert_eq!(
            plan.text(500, 0),
            "SCAN 0 MATCH *[uU][sS][eE][rR]* COUNT 500 TYPE hash"
        );
        // Another database than the one of the console is chosen first.
        assert!(plan.text(500, 1).starts_with("SELECT 0\nSCAN 0 "));
        // "set" is also in "zset": the rows are checked instead.
        page.filters = vec![contains("type", "set")];
        assert_eq!(Plan::new(0, Kind::Keys, &page).unwrap().scan_type, None);
        // A list of members always scans the keys of its type.
        let mut members = request("db0 · hashes");
        members.filters = vec![contains("field", "mail"), contains("value", "@")];
        let plan = Plan::new(0, Kind::Hash, &members).unwrap();
        assert_eq!(plan.scan_type, Some("hash"));
        assert_eq!(
            plan.member_match.as_deref(),
            Some(b"*[mM][aA][iI][lL]*".as_slice())
        );
        assert_eq!(plan.tests.len(), 2);
        // What is not a column, and what cannot be ordered.
        page.filters = vec![contains("nope", "x")];
        assert_eq!(
            Plan::new(0, Kind::Keys, &page).unwrap_err().to_string(),
            "column_not_found"
        );
        page.filters.clear();
        page.order_by = Some(("value".into(), false));
        assert_eq!(
            Plan::new(0, Kind::Keys, &page).unwrap_err().to_string(),
            "redis_sort_column"
        );
        page.order_by = Some(("size".into(), true));
        assert!(Plan::new(0, Kind::Keys, &page).unwrap().needs_details);
        page.limit = 0;
        assert_eq!(
            Plan::new(0, Kind::Keys, &page).unwrap_err().to_string(),
            "page_invalid"
        );
    }

    fn key(name: &str) -> RowId {
        RowId {
            key: name.as_bytes().to_vec(),
            member: Member::None,
        }
    }

    /// A keyspace walked the way `SCAN` walks it: a few keys per step, some
    /// of them twice, the cursor zero at the end.
    #[test]
    fn pages_go_on_where_the_scan_stopped_and_stay_the_same() {
        let all: Vec<Vec<u8>> = (0..1234)
            .map(|n| format!("key:{n:04}").into_bytes())
            .collect();
        let mut scan = Scan::new("0/Keys".into());
        let steps = std::cell::Cell::new(0);
        // What `fill` does for the key list, with the server played here.
        let read_until = |scan: &mut Scan, need: usize| {
            while scan.ids.len() < need && !scan.finished {
                if scan.keys_done {
                    scan.finished = true;
                    break;
                }
                let at: usize = String::from_utf8(scan.cursor.clone())
                    .unwrap()
                    .parse()
                    .unwrap();
                let end = (at + 100).min(all.len());
                // A step repeats the last keys of the one before.
                let mut batch = all[at.saturating_sub(7)..end].to_vec();
                batch.reverse();
                let next = if end == all.len() {
                    "0".to_owned()
                } else {
                    end.to_string()
                };
                steps.set(steps.get() + 1);
                for name in scan.take_keys(batch, next.as_bytes(), false) {
                    scan.push(
                        RowId {
                            key: name,
                            member: Member::None,
                        },
                        None,
                    );
                }
            }
        };
        read_until(&mut scan, 500);
        let first: Vec<RowId> = scan.page(0, 500).to_vec();
        assert_eq!(first.len(), 500);
        let steps_for_first = steps.get();
        assert_eq!(
            steps_for_first, 5,
            "no more of the keyspace is read than the page needs"
        );
        read_until(&mut scan, 1000);
        let second: Vec<RowId> = scan.page(500, 500).to_vec();
        read_until(&mut scan, 1500);
        let third: Vec<RowId> = scan.page(1000, 500).to_vec();
        assert!(scan.finished);
        assert_eq!((second.len(), third.len()), (500, 234));
        // Every key once, whatever the scan repeated.
        let mut seen: Vec<&RowId> = first.iter().chain(&second).chain(&third).collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 1234);
        // A page read again is the same page, and nothing is scanned for it.
        let before = steps.get();
        read_until(&mut scan, 500);
        assert_eq!(scan.page(0, 500), first.as_slice());
        assert_eq!(scan.page(500, 500), second.as_slice());
        assert_eq!(steps.get(), before);
        assert!(scan.page(5000, 500).is_empty());
        // What the grid changed is followed without a new scan.
        scan.forget(&first[1]);
        scan.rename(&first[0], key("renamed"));
        assert_eq!(scan.page(0, 2), [key("renamed"), first[2].clone()]);
    }

    #[test]
    fn a_scan_asks_for_more_at_once_while_a_pattern_finds_little() {
        let mut scan = Scan::new("0/Keys|filter".into());
        assert_eq!(scan.count, 1000);
        scan.take_keys(vec![b"one".to_vec()], b"17", true);
        assert_eq!(scan.count, 2000);
        for _ in 0..10 {
            scan.take_keys(Vec::new(), b"17", true);
        }
        assert_eq!(scan.count, 10_000);
        // Without a pattern a step returns what was asked for.
        let mut plain = Scan::new("0/Keys".into());
        plain.take_keys(Vec::new(), b"0", false);
        assert_eq!(plain.count, 1000);
        assert!(plain.keys_done);
    }

    #[test]
    fn rows_are_ordered_by_number_then_text_with_missing_values_last() {
        let mut scan = Scan::new("0/Keys|order".into());
        for (name, sort) in [
            ("b", SortKey::Number(10.0)),
            ("a", SortKey::Number(9.0)),
            ("d", SortKey::Missing),
            ("c", SortKey::Number(9.0)),
            ("e", SortKey::Text(b"x".to_vec())),
        ] {
            scan.push(key(name), Some(sort));
        }
        scan.sort(false);
        assert_eq!(scan.ids, [key("a"), key("c"), key("b"), key("e"), key("d")]);
        let mut scan = Scan::new("0/Keys|order".into());
        for (name, ttl) in [
            ("soon", Cell::Integer("5".into())),
            ("never", Cell::Null),
            ("later", Cell::Integer("60".into())),
        ] {
            scan.push(key(name), Some(SortKey::of(&ttl)));
        }
        scan.sort(true);
        assert_eq!(scan.ids, [key("never"), key("later"), key("soon")]);
        // A size is a number although its cell is read only.
        assert_eq!(
            SortKey::of(&Cell::Unsupported("12".into())),
            SortKey::Number(12.0)
        );
        assert_eq!(
            SortKey::of(&Cell::Unsupported("hash".into())),
            SortKey::Text(b"hash".to_vec())
        );
    }

    #[test]
    fn the_scans_of_a_session_are_kept_and_the_first_page_starts_again() {
        let mut scans = Scans::default();
        let plan = Plan::new(0, Kind::Keys, &request("db0")).unwrap();
        scans.of(&plan, true).push(key("a"), None);
        assert_eq!(scans.of(&plan, false).ids.len(), 1);
        assert!(scans.of(&plan, true).ids.is_empty());
        let mut filtered = request("db0");
        filtered.filters = vec![contains("key", "a")];
        let other = Plan::new(0, Kind::Keys, &filtered).unwrap();
        scans.of(&other, true).push(key("a"), None);
        scans.of(&plan, false).push(key("b"), None);
        // A change through the grid reaches every scan of that database.
        scans.each(0, |scan| scan.forget_key(b"a"));
        assert!(scans.of(&other, false).ids.is_empty());
        assert_eq!(scans.of(&plan, false).ids, [key("b")]);
        scans.each(1, |scan| scan.forget_key(b"b"));
        assert_eq!(scans.of(&plan, false).ids.len(), 1);
    }

    #[test]
    fn stream_ids_and_the_keyspace_report_are_read() {
        assert_eq!(next_entry_id("1700000000000-0"), "1700000000000-1");
        assert_eq!(next_entry_id(&format!("5-{}", u64::MAX)), "6-0");
        let info = reply::info(
            "# Keyspace\r\ndb0:keys=120,expires=14,avg_ttl=5000,subexpiry=0\r\ndb3:keys=2,expires=0,avg_ttl=0\r\n",
        );
        assert_eq!(databases(&info), [(0, 120, 14, 5000), (3, 2, 0, 0)]);
    }

    #[test]
    fn a_report_is_filtered_ordered_and_cut_here() {
        let grid = || Grid {
            columns: vec!["id".into(), "cmd".into()],
            rows: (1..=5)
                .map(|n| {
                    vec![
                        Cell::Integer(n.to_string()),
                        Cell::Text(if n % 2 == 0 { "get" } else { "SET" }.into()),
                    ]
                })
                .collect(),
        };
        let mut page = request("clients");
        page.filters = vec![contains("cmd", "set")];
        page.order_by = Some(("id".into(), true));
        page.limit = 2;
        let result = view_page(grid(), &page).unwrap();
        assert_eq!(
            result
                .rows
                .iter()
                .map(|row| row[0].display())
                .collect::<Vec<_>>(),
            ["5", "3"]
        );
        page.offset = 2;
        assert_eq!(view_page(grid(), &page).unwrap().rows.len(), 1);
        page.filters = vec![contains("nope", "x")];
        assert!(view_page(grid(), &page).is_err());
    }
}
