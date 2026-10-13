//! Pages of an index: a filter as a query, an order as a sort, and where
//! the next page starts.
//!
//! A page is a `_search` in a fixed order, read on from the sort values of
//! the last hit of the page before (`search_after`). Nothing is addressed
//! by `from`, which the server only allows for the first 10 000 hits and
//! which shifts when documents arrive.
//!
//! The order has to tell every two documents apart, or `search_after`
//! skips the ones that share the values of the last hit. Elasticsearch has
//! `_shard_doc` for that, inside a point in time. Elsewhere the driver
//! orders by `_doc`, which is unique in a shard only, and takes care of the
//! ties itself ([`Mark`]). Sorting by `_id` would do as well, but it makes
//! the server load every id of the index into memory, and current
//! Elasticsearch refuses it.
use super::mapping::{Class, ID, Layout};
use crate::builder::{Filter, FilterOp};
use crate::drivers::PageRequest;
use crate::model::Cell;
use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::time::Instant;

/// Hits the server returns at `from + size` at most, unless an index says
/// otherwise (`index.max_result_window`).
pub(super) const WINDOW: u64 = 10_000;
/// Terms the last word of a "contains" on analysed text may stand for.
const EXPANSIONS: u32 = 1000;
/// How a date typed into a filter is read, whatever format the field has.
const DATE_FORMAT: &str = "strict_date_optional_time||epoch_millis";
/// Pages of one container that are open at the same time.
const OPEN_CURSORS: usize = 6;
/// Marks one cursor keeps.
const MARKS: usize = 4096;

/// A text as a JSON number, when it is written like one.
fn number(text: &str) -> Option<Value> {
    let text = text.trim();
    let first = text.strip_prefix('-').unwrap_or(text).bytes().next()?;
    if !first.is_ascii_digit() {
        return None;
    }
    serde_json::from_str::<serde_json::Number>(text)
        .ok()
        .map(Value::Number)
}

/// A text as a date the server reads: an ISO date, whole or cut short
/// (`2026`, `2026-10`, `2026-10-12 08:30`), milliseconds since 1970, or
/// date math (`now-1d`).
///
/// A date that is cut short stands for its whole period. The server would
/// read `2026-10` as the first day of the month, so the period is named in
/// its date math: `2026-10||/M` is the start of the month as a lower bound
/// and its end as an upper one.
fn date(text: &str) -> Option<Value> {
    let text = text.trim();
    if text.starts_with("now") {
        return Some(json!(text));
    }
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == text.len() {
        return match digits {
            4 => Some(json!(format!("{text}||/y"))),
            // Milliseconds, as the grid shows a date that was stored as a
            // number.
            5..=18 => Some(json!(text)),
            _ => None,
        };
    }
    let shaped = text
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b':' | b'.' | b'+' | b'T' | b'Z' | b' '));
    if digits != 4 || !shaped || text.as_bytes()[4] != b'-' {
        return None;
    }
    let stamp = text.replacen(' ', "T", 1);
    // The time zone is not part of how exact the date is.
    let zone = stamp
        .find('T')
        .and_then(|time| stamp[time..].find(['Z', '+', '-']).map(|at| time + at))
        .unwrap_or(stamp.len());
    let unit = match zone {
        7 => "M",
        10 => "d",
        13 => "h",
        16 => "m",
        19 => "s",
        // To the millisecond, or a shape the server has to judge.
        _ => return Some(json!(stamp)),
    };
    Some(json!(format!("{stamp}||/{unit}")))
}

fn boolean(text: &str) -> Option<Value> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" => Some(json!(true)),
        "false" => Some(json!(false)),
        _ => None,
    }
}

/// An address, or a block of addresses (`10.0.0.0/8`).
fn ip(text: &str) -> Option<Value> {
    let text = text.trim();
    let valid = match text.split_once('/') {
        Some((address, bits)) => match (address.parse::<IpAddr>(), bits.parse::<u8>()) {
            (Ok(IpAddr::V4(_)), Ok(bits)) => bits <= 32,
            (Ok(IpAddr::V6(_)), Ok(bits)) => bits <= 128,
            _ => false,
        },
        None => text.parse::<IpAddr>().is_ok(),
    };
    valid.then(|| json!(text))
}

/// The longest pattern an index takes by default (`index.max_regex_length`).
const PATTERN: usize = 1000;

/// The text anywhere in an exact value, whatever its letter case.
///
/// A regular expression in which every letter stands for both of its
/// cases. The `case_insensitive` option of the server would be shorter,
/// but it folds the letters of ASCII only: `ŻÓŁW` would not find `żółw`.
/// Everything else of the text is a character, not an operator.
fn fragment(path: &str, text: &str) -> Result<Value> {
    let mut pattern = String::from(".*");
    for c in text.chars() {
        let mut lower = c.to_lowercase();
        let mut upper = c.to_uppercase();
        match (lower.next(), lower.next(), upper.next(), upper.next()) {
            // A letter with one other case: either of the two.
            (Some(lower), None, Some(upper), None) if lower != upper => {
                pattern.push('[');
                pattern.push(lower);
                pattern.push(upper);
                pattern.push(']');
            }
            _ if c.is_alphanumeric() || c == ' ' || c == '_' => pattern.push(c),
            _ => {
                pattern.push('\\');
                pattern.push(c);
            }
        }
    }
    pattern.push_str(".*");
    ensure!(pattern.len() <= PATTERN, "elasticsearch_filter_unsupported");
    // No optional operators: `#`, `@`, `&`, `<`, `>` and `~` are characters.
    Ok(json!({ "regexp": { path: { "value": pattern, "flags": "NONE" } } }))
}

/// The bounds a typed text names for numbers and dates: one value, an
/// interval (`10..20`, `..20`, `10..`) or a comparison (`>= 10`, `< 20`).
fn bounds(text: &str, read: impl Fn(&str) -> Option<Value>) -> Option<Value> {
    let text = text.trim();
    let mut range = serde_json::Map::new();
    if let Some((low, high)) = text.split_once("..") {
        if !low.trim().is_empty() {
            range.insert("gte".into(), read(low)?);
        }
        if !high.trim().is_empty() {
            range.insert("lte".into(), read(high)?);
        }
        return (!range.is_empty()).then_some(Value::Object(range));
    }
    for (sign, bound) in [(">=", "gte"), ("<=", "lte"), (">", "gt"), ("<", "lt")] {
        if let Some(rest) = text.strip_prefix(sign) {
            range.insert(bound.into(), read(rest)?);
            return Some(Value::Object(range));
        }
    }
    let value = read(text)?;
    range.insert("gte".into(), value.clone());
    range.insert("lte".into(), value);
    Some(Value::Object(range))
}

fn range(path: &str, mut bounds: Value, class: Class) -> Value {
    if class == Class::Date {
        bounds["format"] = json!(DATE_FORMAT);
    }
    json!({ "range": { path: bounds } })
}

fn not(query: Value) -> Value {
    json!({ "bool": { "must_not": [query] } })
}

/// One filter of the grid as a query.
///
/// "Contains" means what the type of the field can answer: the words of an
/// analysed text with the last one as a beginning (and the text anywhere in
/// its exact sub-field, where it has one), the text anywhere in a keyword,
/// a value or an interval of numbers and dates, where a date cut short
/// stands for its whole period.
fn condition(layout: &Layout, filter: &Filter) -> Result<Value> {
    let path = filter.column.as_str();
    let class = layout
        .class(path)
        .ok_or_else(|| anyhow!("column_not_found"))?;
    let field = layout.field(path);
    let exact = field.and_then(|field| field.keyword.as_deref());
    let nested = field.is_some_and(|field| field.kind == "nested");
    let unsupported = || match class {
        Class::Unmapped => anyhow!("elasticsearch_filter_unmapped"),
        _ => anyhow!("elasticsearch_filter_unsupported"),
    };
    let present = || -> Result<Value> {
        Ok(match class {
            Class::Id => json!({ "match_all": {} }),
            Class::Unmapped => return Err(unsupported()),
            _ if nested => json!({ "nested": { "path": path, "query": { "match_all": {} } } }),
            _ => json!({ "exists": { "field": path } }),
        })
    };
    let text = match &filter.value {
        Cell::Null => None,
        other => Some(other.display()),
    };
    let numeric = |text: &str| number(text).ok_or_else(|| anyhow!("elasticsearch_filter_number"));
    let dated = |text: &str| date(text).ok_or_else(|| anyhow!("elasticsearch_filter_date"));
    let equal = |text: &str| -> Result<Value> {
        Ok(match class {
            Class::Id => json!({ "ids": { "values": [text] } }),
            Class::Text => match exact {
                Some(exact) => json!({ "term": { exact: { "value": text } } }),
                None => json!({ "match_phrase": { path: text } }),
            },
            Class::Keyword => json!({ "term": { path: { "value": text } } }),
            Class::Integer | Class::Float => {
                json!({ "term": { path: { "value": numeric(text)? } } })
            }
            Class::Date => {
                let value = dated(text)?;
                range(path, json!({ "gte": value, "lte": value }), class)
            }
            Class::Boolean => json!({ "term": { path: { "value": boolean(text)
                .ok_or_else(|| anyhow!("elasticsearch_filter_boolean"))? } } }),
            Class::Ip => json!({ "term": { path: { "value": ip(text)
                .ok_or_else(|| anyhow!("elasticsearch_filter_ip"))? } } }),
            Class::Binary | Class::Json | Class::Unmapped => return Err(unsupported()),
        })
    };
    let compared = |bound: &str, text: &str| -> Result<Value> {
        Ok(match class {
            Class::Integer | Class::Float => range(path, json!({ bound: numeric(text)? }), class),
            Class::Date => range(path, json!({ bound: dated(text)? }), class),
            Class::Keyword => range(path, json!({ bound: text }), class),
            Class::Text => match exact {
                Some(exact) => range(exact, json!({ bound: text }), class),
                None => return Err(unsupported()),
            },
            Class::Ip => {
                let address = text.trim().parse::<IpAddr>();
                ensure!(address.is_ok(), "elasticsearch_filter_ip");
                range(path, json!({ bound: text.trim() }), class)
            }
            _ => return Err(unsupported()),
        })
    };
    Ok(match (&filter.op, text.as_deref()) {
        (FilterOp::IsNull, _) | (FilterOp::Equal, None) => not(present()?),
        (FilterOp::IsNotNull, _) | (FilterOp::NotEqual, None) => present()?,
        (FilterOp::Equal, Some(text)) => equal(text)?,
        (FilterOp::NotEqual, Some(text)) => not(equal(text)?),
        (FilterOp::Greater, Some(text)) => compared("gt", text)?,
        (FilterOp::Less, Some(text)) => compared("lt", text)?,
        (FilterOp::Contains, Some(text)) => match class {
            Class::Text => {
                let words = json!({ "match_phrase_prefix": {
                    path: { "query": text, "max_expansions": EXPANSIONS }
                } });
                match exact {
                    Some(exact) => json!({ "bool": {
                        "should": [words, fragment(exact, text)?],
                        "minimum_should_match": 1,
                    } }),
                    None => words,
                }
            }
            Class::Keyword => fragment(path, text)?,
            Class::Integer | Class::Float => range(
                path,
                bounds(text, number).ok_or_else(|| anyhow!("elasticsearch_filter_number"))?,
                class,
            ),
            Class::Date => range(
                path,
                bounds(text, date).ok_or_else(|| anyhow!("elasticsearch_filter_date"))?,
                class,
            ),
            Class::Id | Class::Boolean | Class::Ip => equal(text)?,
            Class::Binary | Class::Json | Class::Unmapped => return Err(unsupported()),
        },
        // Nothing to compare with: nothing is found.
        (FilterOp::Contains | FilterOp::Greater | FilterOp::Less, None) => {
            json!({ "match_none": {} })
        }
    })
}

/// The order of a column, as the server sorts: by a field that keeps its
/// values for that. An analysed text does not; its exact sub-field does.
fn order(layout: &Layout, column: &str, descending: bool) -> Result<Value> {
    let field = layout
        .field(column)
        .ok_or_else(|| anyhow!("elasticsearch_sort_unsupported"))?;
    let (path, unmapped) = match field.class() {
        Class::Keyword => (field.path.as_str(), "keyword"),
        Class::Integer => (field.path.as_str(), "long"),
        Class::Float => (field.path.as_str(), "double"),
        Class::Date => (field.path.as_str(), field.kind.as_str()),
        Class::Boolean => (field.path.as_str(), "boolean"),
        Class::Ip => (field.path.as_str(), "ip"),
        Class::Text => match field.keyword.as_deref() {
            Some(exact) => (exact, "keyword"),
            None => bail!("elasticsearch_sort_unsupported"),
        },
        _ => bail!("elasticsearch_sort_unsupported"),
    };
    Ok(json!({ path: {
        "order": if descending { "desc" } else { "asc" },
        // Documents without the field come last in either direction, and
        // an index of an alias that lacks the field does not fail the page.
        "missing": "_last",
        "unmapped_type": unmapped,
    } }))
}

/// What a page asks the server: the query of its filters and the order
/// that was chosen.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Plan {
    pub query: Value,
    pub sort: Vec<Value>,
}

impl Plan {
    pub(super) fn new(layout: &Layout, request: &PageRequest) -> Result<Plan> {
        ensure!(
            (1..=20_000).contains(&request.limit) && request.offset <= 1_000_000_000,
            "page_invalid"
        );
        let filters = request
            .filters
            .iter()
            .map(|filter| condition(layout, filter))
            .collect::<Result<Vec<_>>>()?;
        let query = if filters.is_empty() {
            json!({ "match_all": {} })
        } else {
            json!({ "bool": { "filter": filters } })
        };
        let sort = match &request.order_by {
            Some((column, descending)) => vec![order(layout, column, *descending)?],
            None => Vec::new(),
        };
        Ok(Plan { query, sort })
    }
}

/// What tells two documents with the same sort values apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tiebreak {
    /// Shard and position in it: unique. Only inside a point in time.
    ShardDoc,
    /// Position in the shard: two shards may hold the same one.
    Doc,
}

impl Tiebreak {
    pub(super) fn sort(self) -> Value {
        match self {
            Tiebreak::ShardDoc => json!({ "_shard_doc": "asc" }),
            Tiebreak::Doc => json!("_doc"),
        }
    }
}

/// One hit of a search, as far as a page needs it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Hit {
    pub index: String,
    pub id: String,
    pub routing: Option<String>,
    pub sort: Vec<Value>,
    pub source: Value,
}

impl Hit {
    pub(super) fn of(hit: &Value) -> Option<Hit> {
        Some(Hit {
            index: hit["_index"].as_str().unwrap_or_default().to_owned(),
            id: hit["_id"].as_str()?.to_owned(),
            routing: hit["_routing"].as_str().map(str::to_owned),
            sort: hit["sort"].as_array().cloned().unwrap_or_default(),
            source: hit.get("_source").cloned().unwrap_or(Value::Null),
        })
    }

    fn identity(&self) -> (String, String, Option<String>) {
        (self.index.clone(), self.id.clone(), self.routing.clone())
    }
}

/// Where a read goes on: the sort values of the last hit that was taken,
/// and, where the order does not tell every two documents apart, the hits
/// already taken that share those values.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Mark {
    last: Option<Vec<Value>>,
    seen: Vec<(String, String, Option<String>)>,
}

impl Mark {
    /// What `search_after` is given to read on from here.
    ///
    /// The server returns what comes strictly after these values. With
    /// `_doc` as the last of them, a hit of another shard at the same
    /// position would be skipped, so the read starts one position earlier
    /// and [`Mark::advance`] drops the hits that were taken before.
    pub(super) fn after(&self, tiebreak: Tiebreak) -> Option<Vec<Value>> {
        let mut values = self.last.clone()?;
        if tiebreak == Tiebreak::Doc
            && let Some(position) = values.last_mut()
        {
            *position = json!(position.as_i64().unwrap_or(0) - 1);
        }
        Some(values)
    }

    /// Hits to ask for on top of the ones that are wanted.
    pub(super) fn overlap(&self, tiebreak: Tiebreak) -> usize {
        match tiebreak {
            Tiebreak::ShardDoc => 0,
            Tiebreak::Doc => self.seen.len(),
        }
    }

    /// Take up to `want` of the hits that follow this mark. Returns them,
    /// and whether every hit of the answer was used up.
    pub(super) fn advance(
        &mut self,
        tiebreak: Tiebreak,
        hits: Vec<Hit>,
        want: usize,
    ) -> (Vec<Hit>, bool) {
        let mut taken = Vec::new();
        let mut rest = hits.into_iter();
        while taken.len() < want {
            let Some(hit) = rest.next() else {
                return (taken, true);
            };
            let same = self.last.as_ref() == Some(&hit.sort);
            if tiebreak == Tiebreak::Doc {
                let identity = hit.identity();
                if same && self.seen.contains(&identity) {
                    continue;
                }
                if !same {
                    self.seen.clear();
                }
                self.seen.push(identity);
            }
            if !same {
                self.last = Some(hit.sort.clone());
            }
            taken.push(hit);
        }
        let used_up = rest.next().is_none();
        (taken, used_up)
    }
}

/// Where the rows from an offset start.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Position {
    At(Mark),
    /// Past the last document.
    End,
}

/// A walk through one container with one filter and one order: the point
/// in time it reads, and the marks of the offsets it has reached.
#[derive(Debug)]
pub(super) struct Cursor {
    /// The point in time the pages are read from, where the server has one.
    pub pit: Option<String>,
    pub tiebreak: Tiebreak,
    /// Names the shard copies that answer, where no point in time holds
    /// them: without it two pages could come from two replicas.
    pub preference: String,
    /// Hits asked for at once: lowered when the server or the size of an
    /// answer says so.
    pub batch: usize,
    marks: BTreeMap<u64, Position>,
    /// The offset of the page that was read last.
    pub last: Option<u64>,
    used: Instant,
}

impl Cursor {
    pub(super) fn new(pit: Option<String>, tiebreak: Tiebreak) -> Cursor {
        Cursor {
            pit,
            tiebreak,
            preference: format!("baza-{}", uuid::Uuid::new_v4().simple()),
            batch: 1000,
            marks: BTreeMap::from([(0, Position::At(Mark::default()))]),
            last: None,
            used: Instant::now(),
        }
    }

    /// The mark nearest below an offset, and the offset it stands for.
    pub(super) fn nearest(&self, offset: u64) -> (u64, Position) {
        self.marks
            .range(..=offset)
            .next_back()
            .map(|(at, position)| (*at, position.clone()))
            .unwrap_or((0, Position::At(Mark::default())))
    }

    pub(super) fn mark(&mut self, offset: u64, position: Position) {
        if self.marks.len() >= MARKS {
            // The start stays; the oldest marks of a long walk go.
            let keep: Vec<u64> = self.marks.keys().rev().take(MARKS / 2).copied().collect();
            self.marks.retain(|at, _| *at == 0 || keep.contains(at));
        }
        self.marks.insert(offset, position);
    }
}

/// The cursors of a session, by what they walk through.
#[derive(Default)]
pub(super) struct Cursors {
    open: Vec<(String, Cursor)>,
}

impl Cursors {
    /// Take the cursor of a page out, to read with it.
    pub(super) fn take(&mut self, identity: &str) -> Option<Cursor> {
        let at = self.open.iter().position(|(known, _)| known == identity)?;
        Some(self.open.remove(at).1)
    }

    /// Put a cursor back. Returns the cursors that had to go: another one
    /// of the same page, and the one that was not used for the longest.
    pub(super) fn put(&mut self, identity: String, mut cursor: Cursor) -> Vec<Cursor> {
        cursor.used = Instant::now();
        let mut gone = Vec::new();
        if let Some(other) = self.take(&identity) {
            gone.push(other);
        }
        self.open.push((identity, cursor));
        while self.open.len() > OPEN_CURSORS {
            let oldest = self
                .open
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, cursor))| cursor.used)
                .map(|(at, _)| at)
                .unwrap_or(0);
            gone.push(self.open.remove(oldest).1);
        }
        gone
    }

    pub(super) fn drain(&mut self) -> Vec<Cursor> {
        self.open.drain(..).map(|(_, cursor)| cursor).collect()
    }
}

/// What tells one walk from another: the container, the filters and the
/// order.
pub(super) fn identity(request: &PageRequest) -> String {
    let filters: Vec<String> = request
        .filters
        .iter()
        .map(|filter| format!("{:?} {:?} {:?}", filter.column, filter.op, filter.value))
        .collect();
    format!(
        "{}\u{1}{}\u{1}{:?}",
        request.container.name,
        filters.join("\u{1}"),
        request.order_by
    )
}

/// What reading a page looks like as text: a request of the console that
/// returns the same documents.
///
/// The filters are written as queries when the fields of the container are
/// known; before that the text names them in words.
pub(super) fn page_text(layout: Option<&Layout>, request: &PageRequest) -> Result<String> {
    ensure!(
        (1..=20_000).contains(&request.limit) && request.offset <= 1_000_000_000,
        "page_invalid"
    );
    let mut lines = Vec::new();
    let mut body = serde_json::Map::new();
    let planned = layout.and_then(|layout| Plan::new(layout, request).ok());
    let mut sort = Vec::new();
    match planned {
        Some(plan) => {
            body.insert("query".into(), plan.query);
            sort = plan.sort;
        }
        None => {
            for filter in &request.filters {
                lines.push(format!(
                    "# filter: {} {} {}",
                    filter.column,
                    match filter.op {
                        FilterOp::Equal => "=",
                        FilterOp::NotEqual => "<>",
                        FilterOp::Contains => "contains",
                        FilterOp::Greater => ">",
                        FilterOp::Less => "<",
                        FilterOp::IsNull => "is missing",
                        FilterOp::IsNotNull => "is present",
                    },
                    match filter.op {
                        FilterOp::IsNull | FilterOp::IsNotNull => String::new(),
                        _ => json!(filter.value.display()).to_string(),
                    }
                ));
            }
            if let Some((column, descending)) = &request.order_by {
                sort.push(json!({ column: if *descending { "desc" } else { "asc" } }));
            }
            body.insert("query".into(), json!({ "match_all": {} }));
        }
    }
    sort.push(json!("_doc"));
    body.insert("sort".into(), Value::Array(sort));
    body.insert("size".into(), json!(request.limit.min(WINDOW as u32)));
    let end = request.offset + u64::from(request.limit);
    if request.offset > 0 && end <= WINDOW {
        body.insert("from".into(), json!(request.offset));
    } else if request.offset > 0 {
        lines.push(format!(
            "# rows from {}: read on with search_after from the page before (an offset reaches {WINDOW} hits at most)",
            request.offset + 1
        ));
    }
    lines.push(format!(
        "GET {}/_search",
        super::http::segment(&request.container.name)
    ));
    lines.push(serde_json::to_string_pretty(&Value::Object(body))?);
    Ok(lines.join("\n"))
}

/// One filter, checked on what a cell shows: for the overviews, whose rows
/// are all on this side.
pub(super) struct Test {
    op: FilterOp,
    text: String,
    lower: String,
}

/// Compare two cells the way a person reads them: numbers by value, the
/// rest as text, nothing last.
pub(super) fn compare(left: &Cell, right: &Cell) -> Ordering {
    let number = |cell: &Cell| match cell {
        Cell::Integer(text) | Cell::Decimal(text) | Cell::Float(text) => text.parse::<f64>().ok(),
        _ => None,
    };
    match (left, right) {
        (Cell::Null, Cell::Null) => Ordering::Equal,
        (Cell::Null, _) => Ordering::Greater,
        (_, Cell::Null) => Ordering::Less,
        _ => match (number(left), number(right)) {
            (Some(left), Some(right)) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
            _ => left
                .display()
                .to_lowercase()
                .cmp(&right.display().to_lowercase()),
        },
    }
}

impl Test {
    pub(super) fn new(filter: &Filter) -> Test {
        let text = match &filter.value {
            Cell::Null => String::new(),
            other => other.display(),
        };
        Test {
            op: filter.op.clone(),
            lower: text.to_lowercase(),
            text,
        }
    }

    pub(super) fn passes(&self, cell: &Cell) -> bool {
        let null = matches!(cell, Cell::Null);
        let ordered = |wanted: Ordering| {
            let other = match cell {
                Cell::Integer(_) | Cell::Decimal(_) | Cell::Float(_) => {
                    Cell::Decimal(self.text.clone())
                }
                _ => Cell::Text(self.text.clone()),
            };
            !null && compare(cell, &other) == wanted
        };
        match self.op {
            FilterOp::Contains => !null && cell.display().to_lowercase().contains(&self.lower),
            FilterOp::Equal => !null && cell.display() == self.text,
            FilterOp::NotEqual => null || cell.display() != self.text,
            FilterOp::Greater => ordered(Ordering::Greater),
            FilterOp::Less => ordered(Ordering::Less),
            FilterOp::IsNull => null,
            FilterOp::IsNotNull => !null,
        }
    }
}

/// The id of a record, from the key the grid holds for it.
pub(super) fn id_of(key: &[(String, Cell)]) -> Result<String> {
    let cell = key
        .iter()
        .find(|(name, _)| name == ID)
        .map(|(_, cell)| cell)
        .ok_or_else(|| anyhow!("primary_key_required"))?;
    match cell {
        Cell::Text(id) | Cell::Integer(id) | Cell::Decimal(id) | Cell::Float(id)
            if !id.is_empty() =>
        {
            Ok(id.clone())
        }
        _ => bail!("primary_key_required"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::elasticsearch::mapping::{self, Field};
    use crate::model::TableRef;

    fn layout() -> Layout {
        let field = |path: &str, kind: &str, keyword: Option<&str>| Field {
            path: path.into(),
            kind: kind.into(),
            keyword: keyword.map(str::to_owned),
        };
        Layout::new(
            vec![
                field("title", "text", Some("title.keyword")),
                field("body", "text", None),
                field("tag", "keyword", None),
                field("count", "long", None),
                field("price", "scaled_float", None),
                field("seen", "date", None),
                field("active", "boolean", None),
                field("client", "ip", None),
                field("blob", "binary", None),
                field("place", "geo_point", None),
                field("items", "nested", None),
                field("address.city", "keyword", None),
            ],
            ["loose".to_owned()],
        )
    }

    fn filter(column: &str, op: FilterOp, value: &str) -> Filter {
        Filter {
            column: column.into(),
            op,
            value: Cell::Text(value.into()),
        }
    }

    fn query(column: &str, op: FilterOp, value: &str) -> Value {
        condition(&layout(), &filter(column, op, value)).unwrap()
    }

    fn refused(column: &str, op: FilterOp, value: &str) -> String {
        condition(&layout(), &filter(column, op, value))
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn contains_is_the_query_the_type_of_the_field_answers() {
        // Analysed text: its words, the last one as a beginning; and the
        // text anywhere in the exact sub-field, where there is one.
        assert_eq!(
            query("body", FilterOp::Contains, "quick bro"),
            json!({ "match_phrase_prefix": { "body": { "query": "quick bro", "max_expansions": 1000 } } })
        );
        assert_eq!(
            query("title", FilterOp::Contains, "ick BRO"),
            json!({ "bool": { "should": [
                { "match_phrase_prefix": { "title": { "query": "ick BRO", "max_expansions": 1000 } } },
                { "regexp": { "title.keyword": { "value": ".*[iI][cC][kK] [bB][rR][oO].*", "flags": "NONE" } } },
            ], "minimum_should_match": 1 } })
        );
        // A keyword: the text anywhere. Every letter stands for both of
        // its cases, in any alphabet, and everything else is a character.
        assert_eq!(
            query("tag", FilterOp::Contains, "Żół 1_"),
            json!({ "regexp": { "tag": { "value": ".*[żŻ][óÓ][łŁ] 1_.*", "flags": "NONE" } } })
        );
        assert_eq!(
            query("tag", FilterOp::Contains, "50% *o.f?\\[x]"),
            json!({ "regexp": { "tag": {
                "value": ".*50\\% \\*[oO]\\.[fF]\\?\\\\\\[[xX]\\].*",
                "flags": "NONE",
            } } })
        );
        // A letter without a single other case is itself.
        assert_eq!(
            query("tag", FilterOp::Contains, "ß日")["regexp"]["tag"]["value"],
            json!(".*ß日.*")
        );
        // A pattern the index would refuse for its length is not sent.
        assert_eq!(
            refused("tag", FilterOp::Contains, &"a".repeat(300)),
            "elasticsearch_filter_unsupported"
        );
        // Numbers: a value, an interval, a comparison.
        assert_eq!(
            query("count", FilterOp::Contains, " 42 "),
            json!({ "range": { "count": { "gte": 42, "lte": 42 } } })
        );
        assert_eq!(
            query("price", FilterOp::Contains, "9.99..20"),
            json!({ "range": { "price": { "gte": 9.99, "lte": 20 } } })
        );
        assert_eq!(
            query("count", FilterOp::Contains, "..-5"),
            json!({ "range": { "count": { "lte": -5 } } })
        );
        assert_eq!(
            query("count", FilterOp::Contains, ">= 10"),
            json!({ "range": { "count": { "gte": 10 } } })
        );
        assert_eq!(
            query("count", FilterOp::Contains, "<7"),
            json!({ "range": { "count": { "lt": 7 } } })
        );
        // Dates: one that is cut short stands for its whole period, which
        // the date math of the server rounds the two bounds to.
        assert_eq!(
            query("seen", FilterOp::Contains, "2026-10"),
            json!({ "range": { "seen": { "gte": "2026-10||/M", "lte": "2026-10||/M", "format": DATE_FORMAT } } })
        );
        assert_eq!(
            query(
                "seen",
                FilterOp::Contains,
                "2026-10-12 08..2026-10-12 17:30"
            ),
            json!({ "range": { "seen": { "gte": "2026-10-12T08||/h", "lte": "2026-10-12T17:30||/m", "format": DATE_FORMAT } } })
        );
        assert_eq!(
            query("seen", FilterOp::Contains, ">now-7d/d"),
            json!({ "range": { "seen": { "gt": "now-7d/d", "format": DATE_FORMAT } } })
        );
        // How exact a date is says which period it stands for.
        for (typed, read) in [
            ("2026", "2026||/y"),
            ("2026-10-12", "2026-10-12||/d"),
            ("2026-10-12T08:30+02:00", "2026-10-12T08:30+02:00||/m"),
            ("2026-10-12 08:30:15.250", "2026-10-12T08:30:15.250"),
            ("1760000000000", "1760000000000"),
            ("now/d", "now/d"),
        ] {
            assert_eq!(date(typed), Some(json!(read)), "{typed}");
        }
        assert_eq!(
            query("active", FilterOp::Contains, "True"),
            json!({ "term": { "active": { "value": true } } })
        );
        assert_eq!(
            query("client", FilterOp::Contains, "10.0.0.0/8"),
            json!({ "term": { "client": { "value": "10.0.0.0/8" } } })
        );
        assert_eq!(
            query(ID, FilterOp::Contains, "order/7"),
            json!({ "ids": { "values": ["order/7"] } })
        );
        // What a type cannot answer is said, not guessed.
        assert_eq!(
            refused("count", FilterOp::Contains, "4x"),
            "elasticsearch_filter_number"
        );
        assert_eq!(
            refused("count", FilterOp::Contains, ".."),
            "elasticsearch_filter_number"
        );
        assert_eq!(
            refused("seen", FilterOp::Contains, "yesterday"),
            "elasticsearch_filter_date"
        );
        assert_eq!(
            refused("seen", FilterOp::Contains, "12.10.2026"),
            "elasticsearch_filter_date"
        );
        assert_eq!(
            refused("active", FilterOp::Contains, "yes"),
            "elasticsearch_filter_boolean"
        );
        assert_eq!(
            refused("client", FilterOp::Contains, "10.0."),
            "elasticsearch_filter_ip"
        );
        assert_eq!(
            refused("client", FilterOp::Contains, "10.0.0.0/33"),
            "elasticsearch_filter_ip"
        );
        assert_eq!(
            refused("place", FilterOp::Contains, "52"),
            "elasticsearch_filter_unsupported"
        );
        assert_eq!(
            refused("blob", FilterOp::Contains, "AA"),
            "elasticsearch_filter_unsupported"
        );
        assert_eq!(
            refused("loose", FilterOp::Contains, "x"),
            "elasticsearch_filter_unmapped"
        );
        assert_eq!(
            refused("nothing", FilterOp::Contains, "x"),
            "column_not_found"
        );
    }

    #[test]
    fn the_other_operators_compare_exact_values() {
        assert_eq!(
            query("tag", FilterOp::Equal, "Nowe"),
            json!({ "term": { "tag": { "value": "Nowe" } } })
        );
        // The exact form of an analysed text, or its phrase without one.
        assert_eq!(
            query("title", FilterOp::Equal, "Quick brown"),
            json!({ "term": { "title.keyword": { "value": "Quick brown" } } })
        );
        assert_eq!(
            query("body", FilterOp::Equal, "Quick brown"),
            json!({ "match_phrase": { "body": "Quick brown" } })
        );
        assert_eq!(
            query("count", FilterOp::Equal, "18446744073709551615").to_string(),
            r#"{"term":{"count":{"value":18446744073709551615}}}"#
        );
        assert_eq!(
            query("seen", FilterOp::Equal, "2026-10-12T08:30:00Z"),
            json!({ "range": { "seen": {
                "gte": "2026-10-12T08:30:00Z||/s", "lte": "2026-10-12T08:30:00Z||/s", "format": DATE_FORMAT
            } } })
        );
        assert_eq!(
            query("tag", FilterOp::NotEqual, "x"),
            json!({ "bool": { "must_not": [{ "term": { "tag": { "value": "x" } } }] } })
        );
        assert_eq!(
            query("count", FilterOp::Greater, "10"),
            json!({ "range": { "count": { "gt": 10 } } })
        );
        assert_eq!(
            query("seen", FilterOp::Less, "2026"),
            json!({ "range": { "seen": { "lt": "2026||/y", "format": DATE_FORMAT } } })
        );
        assert_eq!(
            query("tag", FilterOp::Greater, "m"),
            json!({ "range": { "tag": { "gt": "m" } } })
        );
        assert_eq!(
            query("title", FilterOp::Less, "m"),
            json!({ "range": { "title.keyword": { "lt": "m" } } })
        );
        assert_eq!(
            query("client", FilterOp::Greater, "10.0.0.5"),
            json!({ "range": { "client": { "gt": "10.0.0.5" } } })
        );
        assert_eq!(
            refused("body", FilterOp::Greater, "m"),
            "elasticsearch_filter_unsupported"
        );
        assert_eq!(
            refused("active", FilterOp::Less, "true"),
            "elasticsearch_filter_unsupported"
        );
        assert_eq!(
            refused("count", FilterOp::Equal, "1..2"),
            "elasticsearch_filter_number"
        );
        assert_eq!(
            refused("client", FilterOp::Less, "10.0.0.0/8"),
            "elasticsearch_filter_ip"
        );
        // A field that is missing, and one that is there.
        assert_eq!(
            query("tag", FilterOp::IsNull, ""),
            json!({ "bool": { "must_not": [{ "exists": { "field": "tag" } }] } })
        );
        assert_eq!(
            query("place", FilterOp::IsNotNull, ""),
            json!({ "exists": { "field": "place" } })
        );
        assert_eq!(
            query("items", FilterOp::IsNotNull, ""),
            json!({ "nested": { "path": "items", "query": { "match_all": {} } } })
        );
        assert_eq!(
            query(ID, FilterOp::IsNotNull, ""),
            json!({ "match_all": {} })
        );
        assert_eq!(
            refused("loose", FilterOp::IsNull, ""),
            "elasticsearch_filter_unmapped"
        );
        // "Equals nothing" is "is missing".
        let null = Filter {
            column: "tag".into(),
            op: FilterOp::Equal,
            value: Cell::Null,
        };
        assert_eq!(
            condition(&layout(), &null).unwrap(),
            query("tag", FilterOp::IsNull, "")
        );
    }

    fn request(filters: Vec<Filter>, order: Option<(&str, bool)>) -> PageRequest {
        PageRequest {
            filters,
            order_by: order.map(|(column, descending)| (column.to_owned(), descending)),
            ..PageRequest::first(TableRef {
                schema: String::new(),
                name: "bazaqa-orders".into(),
                kind: "table".into(),
            })
        }
    }

    #[test]
    fn a_page_is_a_filter_context_and_an_order_the_server_can_keep() {
        let plain = Plan::new(&layout(), &request(Vec::new(), None)).unwrap();
        assert_eq!(plain.query, json!({ "match_all": {} }));
        assert!(plain.sort.is_empty());
        let plan = Plan::new(
            &layout(),
            &request(
                vec![
                    filter("tag", FilterOp::Equal, "a"),
                    filter("count", FilterOp::Greater, "1"),
                ],
                Some(("count", true)),
            ),
        )
        .unwrap();
        assert_eq!(
            plan.query,
            json!({ "bool": { "filter": [
                { "term": { "tag": { "value": "a" } } },
                { "range": { "count": { "gt": 1 } } },
            ] } })
        );
        assert_eq!(
            plan.sort,
            [json!({ "count": { "order": "desc", "missing": "_last", "unmapped_type": "long" } })]
        );
        let sort = |column: &str| {
            Plan::new(&layout(), &request(Vec::new(), Some((column, false))))
                .map(|plan| plan.sort[0].clone())
                .map_err(|error| error.to_string())
        };
        // An analysed text is ordered by its exact sub-field.
        assert_eq!(
            sort("title").unwrap(),
            json!({ "title.keyword": { "order": "asc", "missing": "_last", "unmapped_type": "keyword" } })
        );
        assert_eq!(
            sort("price").unwrap(),
            json!({ "price": { "order": "asc", "missing": "_last", "unmapped_type": "double" } })
        );
        assert_eq!(
            sort("seen").unwrap()["seen"]["unmapped_type"],
            json!("date")
        );
        for column in ["body", "place", "items", "blob", "loose", ID, "nothing"] {
            assert_eq!(
                sort(column).unwrap_err(),
                "elasticsearch_sort_unsupported",
                "{column}"
            );
        }
        let mut wrong = request(Vec::new(), None);
        wrong.limit = 0;
        assert_eq!(
            Plan::new(&layout(), &wrong).unwrap_err().to_string(),
            "page_invalid"
        );
    }

    fn hit(index: &str, id: &str, sort: Value) -> Hit {
        Hit {
            index: index.into(),
            id: id.into(),
            routing: None,
            sort: sort.as_array().cloned().unwrap(),
            source: Value::Null,
        }
    }

    fn ids(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|hit| hit.id.as_str()).collect()
    }

    /// What a server returns for `search_after` in an order by `_doc`: the
    /// hits strictly after the values, by position and then by shard.
    fn served(all: &[Hit], after: Option<Vec<Value>>, size: usize) -> Vec<Hit> {
        all.iter()
            .filter(|hit| match &after {
                None => true,
                Some(after) => {
                    let key = |values: &[Value]| -> Vec<i64> {
                        values.iter().map(|value| value.as_i64().unwrap()).collect()
                    };
                    key(&hit.sort) > key(after)
                }
            })
            .take(size)
            .cloned()
            .collect()
    }

    #[test]
    fn positions_shared_by_shards_are_neither_skipped_nor_read_twice() {
        // Three shards: the positions 0, 1 and 2 are each held by several.
        let all = vec![
            hit("i", "d2", json!([0])),
            hit("i", "d1", json!([0])),
            hit("i", "d3", json!([0])),
            hit("i", "d5", json!([1])),
            hit("i", "d6", json!([1])),
            hit("i", "d4", json!([1])),
            hit("i", "d10", json!([2])),
            hit("i", "d7", json!([2])),
            hit("i", "d8", json!([3])),
        ];
        for want in 1..=5 {
            let mut mark = Mark::default();
            let mut read = Vec::new();
            for _ in 0..20 {
                let size = want + mark.overlap(Tiebreak::Doc);
                let answer = served(&all, mark.after(Tiebreak::Doc), size);
                let full = answer.len() == size;
                let (taken, used_up) = mark.advance(Tiebreak::Doc, answer, want);
                read.extend(taken);
                if !full && used_up {
                    break;
                }
            }
            assert_eq!(ids(&read), ids(&all), "pages of {want}");
        }
        // A plain search_after from the last hit would have lost two.
        let mut naive = Mark::default();
        let (first, _) = naive.advance(Tiebreak::ShardDoc, served(&all, None, 1), 1);
        assert_eq!(ids(&first), ["d2"]);
        assert_eq!(
            ids(&served(&all, naive.after(Tiebreak::ShardDoc), 2)),
            ["d5", "d6"]
        );
    }

    #[test]
    fn a_unique_order_reads_on_from_the_last_hit() {
        let mut mark = Mark::default();
        assert_eq!(mark.after(Tiebreak::ShardDoc), None);
        assert_eq!(mark.overlap(Tiebreak::ShardDoc), 0);
        let hits = vec![
            hit("i", "a", json!(["t0", 0])),
            hit("i", "b", json!(["t0", 4294967296u64])),
            hit("i", "c", json!([null, 7])),
        ];
        let (taken, used_up) = mark.advance(Tiebreak::ShardDoc, hits.clone(), 2);
        assert_eq!((ids(&taken), used_up), (vec!["a", "b"], false));
        assert_eq!(
            mark.after(Tiebreak::ShardDoc),
            Some(vec![json!("t0"), json!(4294967296u64)])
        );
        let (taken, used_up) = mark.advance(Tiebreak::ShardDoc, hits[2..].to_vec(), 2);
        assert_eq!((ids(&taken), used_up), (vec!["c"], true));
        // A missing value is passed back as it came.
        assert_eq!(
            mark.after(Tiebreak::ShardDoc),
            Some(vec![Value::Null, json!(7)])
        );
        // In an order by `_doc` the position is the last value.
        let mut by_doc = Mark::default();
        by_doc.advance(Tiebreak::Doc, vec![hit("i", "a", json!(["t0", 0]))], 1);
        assert_eq!(
            by_doc.after(Tiebreak::Doc),
            Some(vec![json!("t0"), json!(-1)])
        );
        assert_eq!(by_doc.overlap(Tiebreak::Doc), 1);
    }

    #[test]
    fn a_cursor_remembers_where_the_offsets_start() {
        let mut cursor = Cursor::new(Some("pit".into()), Tiebreak::ShardDoc);
        assert_eq!(cursor.nearest(0), (0, Position::At(Mark::default())));
        assert_eq!(cursor.nearest(1500).0, 0);
        let mut mark = Mark::default();
        mark.advance(Tiebreak::ShardDoc, vec![hit("i", "a", json!([499]))], 1);
        cursor.mark(500, Position::At(mark.clone()));
        cursor.mark(1000, Position::End);
        assert_eq!(cursor.nearest(500), (500, Position::At(mark.clone())));
        assert_eq!(cursor.nearest(999), (500, Position::At(mark)));
        assert_eq!(cursor.nearest(5000), (1000, Position::End));
        // A long walk keeps its start and its latest marks.
        for step in 0..(MARKS as u64 + 10) {
            cursor.mark(2000 + step, Position::End);
        }
        assert!(cursor.marks.len() <= MARKS);
        assert_eq!(cursor.nearest(0).0, 0);
        assert_eq!(cursor.nearest(u64::MAX).0, 2000 + MARKS as u64 + 9);
    }

    #[test]
    fn cursors_are_kept_by_what_they_walk_through() {
        let first = request(vec![filter("tag", FilterOp::Contains, "a")], None);
        let mut other = first.clone();
        other.offset = 500;
        assert_eq!(
            identity(&first),
            identity(&other),
            "the offset is not part of it"
        );
        let mut sorted = first.clone();
        sorted.order_by = Some(("count".into(), false));
        assert_ne!(identity(&first), identity(&sorted));
        assert_ne!(identity(&first), identity(&request(Vec::new(), None)));
        let mut cursors = Cursors::default();
        assert!(cursors.take("a").is_none());
        assert!(
            cursors
                .put("a".into(), Cursor::new(Some("one".into()), Tiebreak::Doc))
                .is_empty()
        );
        // A second walk through the same page replaces the first.
        let gone = cursors.put("a".into(), Cursor::new(Some("two".into()), Tiebreak::Doc));
        assert_eq!(gone[0].pit.as_deref(), Some("one"));
        for name in ["b", "c", "d", "e", "f"] {
            assert!(
                cursors
                    .put(name.into(), Cursor::new(None, Tiebreak::Doc))
                    .is_empty()
            );
        }
        // One more than may be open: the one unused for the longest goes.
        let gone = cursors.put("g".into(), Cursor::new(None, Tiebreak::Doc));
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].pit.as_deref(), Some("two"));
        assert!(cursors.take("a").is_none());
        assert!(cursors.take("g").is_some());
        assert_eq!(cursors.drain().len(), OPEN_CURSORS - 1);
    }

    #[test]
    fn the_text_of_a_page_is_a_request_of_the_console() {
        let first = page_text(Some(&layout()), &request(Vec::new(), None)).unwrap();
        assert_eq!(
            first,
            "GET bazaqa-orders/_search\n{\n  \"query\": {\n    \"match_all\": {}\n  },\n  \"size\": 500,\n  \"sort\": [\n    \"_doc\"\n  ]\n}"
        );
        let mut filtered = request(
            vec![filter("tag", FilterOp::Contains, "nowe")],
            Some(("count", true)),
        );
        filtered.offset = 500;
        let text = page_text(Some(&layout()), &filtered).unwrap();
        assert!(text.starts_with("GET bazaqa-orders/_search\n{"), "{text}");
        let body: Value = serde_json::from_str(text.split_once('\n').unwrap().1).unwrap();
        assert_eq!(body["from"], json!(500));
        assert_eq!(
            body["query"]["bool"]["filter"][0]["regexp"]["tag"]["value"],
            json!(".*[nN][oO][wW][eE].*")
        );
        assert_eq!(body["sort"][0]["count"]["order"], json!("desc"));
        assert_eq!(body["sort"][1], json!("_doc"));
        // Past what "from" may address the text says how the page is read.
        filtered.offset = 10_000;
        let deep = page_text(Some(&layout()), &filtered).unwrap();
        assert!(
            deep.starts_with("# rows from 10001: read on with search_after"),
            "{deep}"
        );
        assert!(!deep.contains("\"from\""));
        // Before the fields are known the filter is named in words.
        let unknown = page_text(None, &filtered).unwrap();
        assert!(
            unknown.contains("# filter: tag contains \"nowe\""),
            "{unknown}"
        );
        assert!(unknown.contains("GET bazaqa-orders/_search"));
        let mut wrong = request(Vec::new(), None);
        wrong.limit = 20_001;
        assert_eq!(
            page_text(None, &wrong).unwrap_err().to_string(),
            "page_invalid"
        );
        // A filter the type cannot answer still has a text: the page says why.
        let odd = request(vec![filter("count", FilterOp::Contains, "x")], None);
        assert!(
            page_text(Some(&layout()), &odd)
                .unwrap()
                .contains("# filter: count contains \"x\"")
        );
    }

    #[test]
    fn the_overviews_are_filtered_and_ordered_on_this_side() {
        let test = |op: FilterOp, value: &str| Test::new(&filter("x", op, value));
        let text = |value: &str| Cell::Text(value.into());
        assert!(test(FilterOp::Contains, "GRE").passes(&text("green")));
        assert!(!test(FilterOp::Contains, "x").passes(&Cell::Null));
        assert!(test(FilterOp::Equal, "green").passes(&text("green")));
        assert!(test(FilterOp::NotEqual, "green").passes(&Cell::Null));
        assert!(test(FilterOp::Greater, "9").passes(&Cell::Integer("10".into())));
        assert!(!test(FilterOp::Greater, "9").passes(&text("10")));
        assert!(test(FilterOp::Less, "b").passes(&text("a")));
        assert!(test(FilterOp::IsNull, "").passes(&Cell::Null));
        assert!(test(FilterOp::IsNotNull, "").passes(&text("")));
        assert_eq!(
            compare(&Cell::Integer("9".into()), &Cell::Integer("10".into())),
            Ordering::Less
        );
        assert_eq!(compare(&Cell::Null, &text("a")), Ordering::Greater);
        assert_eq!(compare(&text("B"), &text("a")), Ordering::Greater);
        // The key of a record is its id.
        assert_eq!(
            id_of(&[(mapping::ID.into(), text("order/7"))]).unwrap(),
            "order/7"
        );
        assert_eq!(
            id_of(&[("other".into(), text("x"))])
                .unwrap_err()
                .to_string(),
            "primary_key_required"
        );
        assert_eq!(
            id_of(&[(mapping::ID.into(), Cell::Null)])
                .unwrap_err()
                .to_string(),
            "primary_key_required"
        );
    }
}
