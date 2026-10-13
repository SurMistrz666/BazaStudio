//! What "Dane tabeli" needs of a store that only reads forward and only
//! finds items by their key: what is known of a table, how a page request
//! is answered, where the pages of a view start, and the columns of items
//! that each have their own attributes.
use super::value::{self, Hint};
use crate::builder::{Filter, FilterOp};
use crate::drivers::PageRequest;
use crate::model::{Cell, ColumnInfo, ResultColumn};
use anyhow::{Result, anyhow, ensure};
use serde_json::{Map, Value, json};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Instant;

/// Most calls one page makes. A call reads at most 1 MB, so a page reads
/// at most this many megabytes, whatever the filter.
pub(super) const PAGE_CALLS: usize = 16;
/// Most views (a table with its filter and order) whose pages are remembered.
const CURSORS: usize = 8;
/// Most pages of one view whose start is remembered.
const MARKS: usize = 4000;
/// The list of all tables with what `DescribeTable` says of each.
pub(super) const OVERVIEW: &str = "table overview";

/// An attribute of the primary key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Key {
    pub name: String,
    /// `S`, `N` or `B`.
    pub kind: String,
    /// The sort key; otherwise the partition key.
    pub sort: bool,
}

/// What `DescribeTable` says of a table.
#[derive(Clone, Debug)]
pub(super) struct TableInfo {
    pub name: String,
    /// The partition key, then the sort key when there is one.
    pub keys: Vec<Key>,
    pub status: String,
    pub billing: String,
    /// Counted by the service about every six hours.
    pub items: u64,
    pub bytes: u64,
    pub global_indexes: Vec<String>,
    pub local_indexes: Vec<String>,
    pub created: String,
    pub read_at: Instant,
}

impl TableInfo {
    /// Read the `Table` of a `DescribeTable` answer.
    pub(super) fn of(table: &Value) -> Result<TableInfo> {
        let name = table["TableName"]
            .as_str()
            .ok_or_else(|| anyhow!("dynamodb_endpoint_not_dynamodb"))?;
        let kind_of = |attribute: &str| {
            table["AttributeDefinitions"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|definition| definition["AttributeName"].as_str() == Some(attribute))
                .and_then(|definition| definition["AttributeType"].as_str())
                .unwrap_or("S")
                .to_owned()
        };
        let mut keys: Vec<Key> = table["KeySchema"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|element| {
                let name = element["AttributeName"].as_str()?;
                Some(Key {
                    name: name.to_owned(),
                    kind: kind_of(name),
                    sort: element["KeyType"].as_str() == Some("RANGE"),
                })
            })
            .collect();
        keys.sort_by_key(|key| key.sort);
        ensure!(!keys.is_empty(), "dynamodb_endpoint_not_dynamodb");
        let names = |field: &str| -> Vec<String> {
            table[field]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|index| index["IndexName"].as_str().map(str::to_owned))
                .collect()
        };
        // A table without a billing summary is a provisioned one.
        let billing = match table["BillingModeSummary"]["BillingMode"].as_str() {
            Some("PAY_PER_REQUEST") => "on-demand".to_owned(),
            _ => {
                let throughput = &table["ProvisionedThroughput"];
                match (
                    throughput["ReadCapacityUnits"].as_u64(),
                    throughput["WriteCapacityUnits"].as_u64(),
                ) {
                    (Some(read), Some(write)) if read + write > 0 => {
                        format!("provisioned {read} RCU / {write} WCU")
                    }
                    _ => "provisioned".to_owned(),
                }
            }
        };
        let whole = |value: &Value| {
            value
                .as_u64()
                .or_else(|| value.as_f64().map(|number| number.max(0.0) as u64))
                .unwrap_or(0)
        };
        let created = table["CreationDateTime"]
            .as_f64()
            .and_then(|seconds| chrono::DateTime::from_timestamp(seconds as i64, 0))
            .map(|moment| moment.format("%Y-%m-%d %H:%M:%SZ").to_string())
            .unwrap_or_default();
        Ok(TableInfo {
            name: name.to_owned(),
            keys,
            status: table["TableStatus"].as_str().unwrap_or("ACTIVE").to_owned(),
            billing,
            items: whole(&table["ItemCount"]),
            bytes: whole(&table["TableSizeBytes"]),
            global_indexes: names("GlobalSecondaryIndexes"),
            local_indexes: names("LocalSecondaryIndexes"),
            created,
            read_at: Instant::now(),
        })
    }

    pub(super) fn partition(&self) -> &Key {
        &self.keys[0]
    }

    pub(super) fn sort(&self) -> Option<&Key> {
        self.keys.iter().find(|key| key.sort)
    }

    pub(super) fn key(&self, name: &str) -> Option<&Key> {
        self.keys.iter().find(|key| key.name == name)
    }

    /// The key of an item: what addresses it, and where a read goes on
    /// after it.
    pub(super) fn key_of(&self, item: &Map<String, Value>) -> Option<Value> {
        let mut key = Map::new();
        for attribute in &self.keys {
            key.insert(attribute.name.clone(), item.get(&attribute.name)?.clone());
        }
        Some(Value::Object(key))
    }

    /// The key the grid holds for a record, as typed values.
    pub(super) fn key_from(&self, cells: &[(String, Cell)]) -> Result<Map<String, Value>> {
        let mut key = Map::new();
        for attribute in &self.keys {
            let cell = cells
                .iter()
                .find(|(name, _)| *name == attribute.name)
                .map(|(_, cell)| cell)
                .ok_or_else(|| anyhow!("primary_key_required"))?;
            let value = value::attribute(cell, Hint::Key(&attribute.kind))?
                .ok_or_else(|| anyhow!("dynamodb_key_required"))?;
            key.insert(attribute.name.clone(), value);
        }
        Ok(key)
    }
}

/// A size in the largest unit that keeps it readable.
pub(super) fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut amount = bytes as f64;
    let mut unit = 0;
    while amount >= 1000.0 && unit + 1 < UNITS.len() {
        amount /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{amount:.1} {}", UNITS[unit])
    }
}

/// The types items were seen to hold, by attribute.
pub(super) type Attributes = BTreeMap<String, BTreeSet<&'static str>>;

/// Note the attributes of items and the types of their values.
pub(super) fn note<'a>(
    attributes: &mut Attributes,
    items: impl IntoIterator<Item = &'a Map<String, Value>>,
) {
    for item in items {
        for (name, value) in item {
            let seen = attributes.entry(name.clone()).or_default();
            if let Some(kind) = value::type_of(value) {
                seen.insert(kind);
            }
        }
    }
}

/// The names of the columns of a table: its key, then every attribute that
/// was seen, by name.
pub(super) fn layout(table: &TableInfo, attributes: &Attributes) -> Vec<String> {
    let mut names: Vec<String> = table.keys.iter().map(|key| key.name.clone()).collect();
    names.extend(
        attributes
            .keys()
            .filter(|name| table.key(name).is_none())
            .cloned(),
    );
    names
}

/// A type of DynamoDB in a word. The grid writes the type of a column in
/// small letters next to its name, where `s`, `n` and `l` say nothing.
pub(super) fn word(kind: &str) -> &'static str {
    match kind {
        "S" => "string",
        "N" => "number",
        "B" => "binary",
        "BOOL" => "boolean",
        "NULL" => "null",
        "L" => "list",
        "M" => "map",
        "SS" => "string set",
        "NS" => "number set",
        "BS" => "binary set",
        _ => "",
    }
}

/// The types that were seen, in words.
pub(super) fn words(seen: Option<&BTreeSet<&'static str>>) -> String {
    value::TYPES
        .iter()
        .filter(|kind| seen.is_some_and(|seen| seen.contains(*kind)))
        .map(|kind| word(kind))
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The type of a column as the grid names it.
pub(super) fn label(table: &TableInfo, attributes: &Attributes, name: &str) -> String {
    if let Some(key) = table.key(name) {
        return format!(
            "{} · {}",
            word(&key.kind),
            if key.sort {
                "sort key"
            } else {
                "partition key"
            }
        );
    }
    words(attributes.get(name))
}

pub(super) fn column_info(table: &TableInfo, attributes: &Attributes) -> Vec<ColumnInfo> {
    layout(table, attributes)
        .into_iter()
        .map(|name| ColumnInfo {
            data_type: label(table, attributes, &name),
            nullable: table.key(&name).is_none(),
            primary_key: table.key(&name).is_some(),
            default: None,
            name,
        })
        .collect()
}

pub(super) fn result_columns(table: &TableInfo, attributes: &Attributes) -> Vec<ResultColumn> {
    layout(table, attributes)
        .into_iter()
        .map(|name| ResultColumn {
            data_type: label(table, attributes, &name),
            name,
        })
        .collect()
}

/// The cells of an item in the order of the columns: an attribute the item
/// does not have is nothing.
pub(super) fn row(columns: &[ResultColumn], item: &Map<String, Value>) -> Vec<Cell> {
    columns
        .iter()
        .map(|column| item.get(&column.name).map_or(Cell::Null, value::cell))
        .collect()
}

/// One filter, checked on what the cell of its attribute shows.
#[derive(Clone, Debug)]
pub(super) struct Test {
    pub attribute: String,
    op: FilterOp,
    text: String,
    lower: String,
}

impl Test {
    pub(super) fn new(filter: &Filter) -> Test {
        let text = match &filter.value {
            Cell::Null => String::new(),
            other => other.display(),
        };
        Test {
            attribute: filter.column.clone(),
            op: filter.op.clone(),
            lower: text.to_lowercase(),
            text,
        }
    }

    pub(super) fn passes_cell(&self, cell: &Cell) -> bool {
        let null = matches!(cell, Cell::Null);
        let ordered =
            |wanted: Ordering| !null && compare_text(&cell.display(), &self.text) == wanted;
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

    pub(super) fn passes(&self, item: &Map<String, Value>) -> bool {
        self.passes_cell(&item.get(&self.attribute).map_or(Cell::Null, value::cell))
    }

    /// What the service can check before it sends an item: a text without
    /// letters is found by `contains` whatever the letter case, so only
    /// texts that hold it (and values that are not texts, which are checked
    /// here) need to travel. `None` when everything has to be sent.
    pub(super) fn needle(&self) -> Option<&str> {
        let plain = !self.text.is_empty()
            && self.text.len() <= 1024
            && self
                .text
                .chars()
                .all(|c| c.to_lowercase().eq(c.to_uppercase()));
        (self.op == FilterOp::Contains && plain).then_some(self.text.as_str())
    }
}

/// The filter expression that narrows what a read sends for these tests,
/// with its names and values. Attributes named in `skip` are left out: a
/// `Query` may not filter on its own key.
pub(super) fn narrowing(tests: &[Test], skip: &[&str]) -> Option<(String, Value, Value)> {
    let mut parts = Vec::new();
    let mut names = Map::new();
    let mut values = Map::new();
    for (index, test) in tests.iter().enumerate() {
        let Some(needle) = test.needle() else {
            continue;
        };
        if skip.contains(&test.attribute.as_str()) {
            continue;
        }
        parts.push(format!(
            "attribute_exists(#f{index}) AND (contains(#f{index}, :f{index}) OR NOT attribute_type(#f{index}, :s))"
        ));
        names.insert(format!("#f{index}"), json!(test.attribute));
        values.insert(format!(":f{index}"), json!({ "S": needle }));
        values.insert(":s".into(), json!({ "S": "S" }));
    }
    (!parts.is_empty()).then(|| {
        (
            parts.join(" AND "),
            Value::Object(names),
            Value::Object(values),
        )
    })
}

/// Numbers by their value, anything else as text.
fn compare_text(left: &str, right: &str) -> Ordering {
    match (
        left.trim().parse::<bigdecimal::BigDecimal>(),
        right.trim().parse::<bigdecimal::BigDecimal>(),
    ) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        _ => left.cmp(right),
    }
}

/// The order of two cells of one column: nothing first, then by value.
pub(super) fn compare(left: &Cell, right: &Cell) -> Ordering {
    match (left, right) {
        (Cell::Null, Cell::Null) => Ordering::Equal,
        (Cell::Null, _) => Ordering::Less,
        (_, Cell::Null) => Ordering::Greater,
        (Cell::Bool(left), Cell::Bool(right)) => left.cmp(right),
        (Cell::Bytes(left), Cell::Bytes(right)) => left.cmp(right),
        (Cell::Decimal(left), Cell::Decimal(right)) => compare_text(left, right),
        // Numbers before texts, as the service orders types.
        (Cell::Decimal(_), _) => Ordering::Less,
        (_, Cell::Decimal(_)) => Ordering::Greater,
        (left, right) => left.display().cmp(&right.display()),
    }
}

/// How a page request is answered.
#[derive(Clone, Debug)]
pub(super) struct Plan {
    /// The value of the partition key the filter names: asked with `Query`
    /// first, which reads one partition instead of the table.
    pub exact: Option<Value>,
    /// The filter asks for exactly that key: nothing else is tried.
    pub exact_only: bool,
    /// Every filter, for a walk through the table.
    pub tests: Vec<Test>,
    /// The filters left to check on the items of that partition.
    pub exact_tests: Vec<Test>,
    /// Attribute to order by and whether descending.
    pub order: Option<(String, bool)>,
}

impl Plan {
    pub(super) fn new(table: &TableInfo, page: &PageRequest) -> Result<Plan> {
        ensure!(
            (1..=20_000).contains(&page.limit) && page.offset <= 1_000_000_000,
            "page_invalid"
        );
        let tests: Vec<Test> = page.filters.iter().map(Test::new).collect();
        let partition = table.partition();
        let position = page.filters.iter().position(|filter| {
            filter.column == partition.name
                && matches!(filter.op, FilterOp::Contains | FilterOp::Equal)
                && !matches!(filter.value, Cell::Null)
        });
        let mut exact = None;
        let mut exact_tests = tests.clone();
        let mut exact_only = false;
        if let Some(position) = position
            // Bytes are not typed into a filter.
            && partition.kind != "B"
            && let Ok(Some(value)) = value::attribute(
                &Cell::Text(page.filters[position].value.display()),
                Hint::Key(&partition.kind),
            )
        {
            exact = Some(value);
            exact_only = page.filters[position].op == FilterOp::Equal;
            exact_tests.remove(position);
        }
        Ok(Plan {
            exact,
            exact_only,
            tests,
            exact_tests,
            order: page.order_by.clone(),
        })
    }
}

/// What identifies a view: a table with its filter and its order.
pub(super) fn identity(page: &PageRequest) -> String {
    let mut text = page.container.name.clone();
    for filter in &page.filters {
        text.push_str(&format!(
            "\u{1f}{}\u{1e}{:?}\u{1e}{}",
            filter.column,
            filter.op,
            filter.value.display()
        ));
    }
    if let Some((column, descending)) = &page.order_by {
        text.push_str(&format!("\u{1f}order {column} {descending}"));
    }
    text
}

/// Where a page starts.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Mark {
    /// After the item with this key (`None`: at the beginning).
    At(Option<Value>),
    /// Past the last item.
    End,
}

/// Which operation a view reads with, settled on its first page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    /// `Query`: the partition of the key that was typed.
    Query,
    /// `Scan`: the whole table, in the order the service stores it.
    Scan,
}

/// What is remembered of a view between its pages.
#[derive(Debug)]
pub(super) struct Cursor {
    pub marks: BTreeMap<u64, Mark>,
    pub mode: Option<Mode>,
    /// All items of an ordered view, in order.
    pub sorted: Option<std::sync::Arc<Vec<Map<String, Value>>>>,
    used: Instant,
}

impl Cursor {
    fn new() -> Cursor {
        Cursor {
            marks: BTreeMap::from([(0, Mark::At(None))]),
            mode: None,
            sorted: None,
            used: Instant::now(),
        }
    }

    pub(super) fn mark(&mut self, offset: u64, mark: Mark) {
        if self.marks.len() >= MARKS {
            let middle = self.marks.keys().nth(MARKS / 2).copied();
            if let Some(middle) = middle {
                self.marks.remove(&middle);
            }
        }
        self.marks.insert(offset, mark);
    }

    /// The nearest remembered start at or before `offset`.
    pub(super) fn nearest(&self, offset: u64) -> (u64, Mark) {
        self.marks
            .range(..=offset)
            .next_back()
            .map_or((0, Mark::At(None)), |(offset, mark)| {
                (*offset, mark.clone())
            })
    }
}

/// The views whose pages are remembered.
#[derive(Debug, Default)]
pub(super) struct Cursors {
    views: HashMap<String, Cursor>,
}

impl Cursors {
    pub(super) fn of(&mut self, identity: &str, restart: bool) -> &mut Cursor {
        if restart {
            self.views.remove(identity);
        }
        if !self.views.contains_key(identity) && self.views.len() >= CURSORS {
            let oldest = self
                .views
                .iter()
                .min_by_key(|(_, cursor)| cursor.used)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.views.remove(&oldest);
            }
        }
        let cursor = self
            .views
            .entry(identity.to_owned())
            .or_insert_with(Cursor::new);
        cursor.used = Instant::now();
        cursor
    }

    pub(super) fn clear(&mut self) {
        self.views.clear();
    }
}

/// What reading a page looks like as text: the PartiQL statement that asks
/// for the same items, and in comments what the statement does not say.
pub(super) fn page_text(table: Option<&TableInfo>, page: &PageRequest) -> Result<String> {
    ensure!(
        (1..=20_000).contains(&page.limit) && page.offset <= 1_000_000_000,
        "page_invalid"
    );
    let mut text = format!("SELECT * FROM {}", value::name(&page.container.name));
    if page.container.name == OVERVIEW {
        return Ok(format!(
            "{text}\n-- every table of the account in this region, as DescribeTable describes it"
        ));
    }
    let plan = match table {
        Some(table) => Some(Plan::new(table, page)?),
        None => None,
    };
    let exact = plan.as_ref().and_then(|plan| {
        let table = table?;
        let value = plan.exact.as_ref()?;
        Some(format!(
            " WHERE {} = {}",
            value::name(&table.partition().name),
            value::partiql(value)
        ))
    });
    if let Some(condition) = &exact {
        text.push_str(condition);
    }
    text.push_str(&format!("\n-- {} items a page", page.limit));
    if page.offset > 0 {
        text.push_str(&format!(
            ", from item {}: read on from the last key of the page before (there is no OFFSET)",
            page.offset + 1
        ));
    }
    let described: Vec<String> = page
        .filters
        .iter()
        .map(|filter| {
            let word = match filter.op {
                FilterOp::Contains => "contains",
                FilterOp::Equal => "=",
                FilterOp::NotEqual => "<>",
                FilterOp::Greater => ">",
                FilterOp::Less => "<",
                FilterOp::IsNull => "is missing",
                FilterOp::IsNotNull => "exists",
            };
            let value = match filter.op {
                FilterOp::IsNull | FilterOp::IsNotNull => String::new(),
                _ => format!(" '{}'", filter.value.display().replace('\'', "''")),
            };
            format!("{} {word}{value}", value::name(&filter.column))
        })
        .collect();
    if !described.is_empty() {
        let what = described.join(" and ");
        let bound = format!(
            "checked on the items a Scan reads, at most {PAGE_CALLS} MB a page; a filter does not make a Scan read less"
        );
        match (&exact, plan.as_ref().is_some_and(|plan| plan.exact_only)) {
            // Nothing more to say: the statement is the filter.
            (Some(_), true) => {}
            (Some(_), false) => text.push_str(&format!(
                "\n-- a Query of that partition key; without an item of that key: {what}, {bound}"
            )),
            (None, _) => text.push_str(&format!("\n-- filter {what}: {bound}")),
        }
    }
    if let Some((column, descending)) = &page.order_by {
        text.push_str(&format!(
            "\n-- ordered by {} {}: by the service inside one partition by its sort key, otherwise by the application on what {PAGE_CALLS} MB hold",
            value::name(column),
            if *descending { "DESC" } else { "ASC" }
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TableRef;
    use crate::sql;

    fn music() -> TableInfo {
        TableInfo::of(&json!({
            "TableName": "Music",
            "TableStatus": "ACTIVE",
            "ItemCount": 120,
            "TableSizeBytes": 14_250,
            "CreationDateTime": 1_714_566_896.789,
            "AttributeDefinitions": [
                {"AttributeName": "Song", "AttributeType": "S"},
                {"AttributeName": "Artist", "AttributeType": "S"},
                {"AttributeName": "Year", "AttributeType": "N"},
            ],
            "KeySchema": [
                {"AttributeName": "Song", "KeyType": "RANGE"},
                {"AttributeName": "Artist", "KeyType": "HASH"},
            ],
            "BillingModeSummary": {"BillingMode": "PAY_PER_REQUEST"},
            "GlobalSecondaryIndexes": [{"IndexName": "by_year"}],
        }))
        .unwrap()
    }

    fn request(filters: &[(&str, FilterOp, &str)]) -> PageRequest {
        PageRequest {
            filters: filters
                .iter()
                .map(|(column, op, value)| Filter {
                    column: (*column).into(),
                    op: op.clone(),
                    value: Cell::Text((*value).into()),
                })
                .collect(),
            ..PageRequest::first(TableRef {
                schema: String::new(),
                name: "Music".into(),
                kind: "table".into(),
            })
        }
    }

    #[test]
    fn a_table_is_described_by_its_key_and_its_figures() {
        let table = music();
        // The partition key first, whatever order the service lists them in.
        assert_eq!(
            table
                .keys
                .iter()
                .map(|key| (key.name.as_str(), key.kind.as_str(), key.sort))
                .collect::<Vec<_>>(),
            [("Artist", "S", false), ("Song", "S", true)]
        );
        assert_eq!(table.partition().name, "Artist");
        assert_eq!(table.sort().map(|key| key.name.as_str()), Some("Song"));
        assert_eq!((table.items, table.bytes), (120, 14_250));
        assert_eq!(table.billing, "on-demand");
        assert_eq!(table.global_indexes, ["by_year"]);
        assert_eq!(table.created, "2024-05-01 12:34:56Z");
        let provisioned = TableInfo::of(&json!({
            "TableName": "Old",
            "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
            "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "N"}],
            "ProvisionedThroughput": {"ReadCapacityUnits": 5, "WriteCapacityUnits": 2},
        }))
        .unwrap();
        assert_eq!(provisioned.billing, "provisioned 5 RCU / 2 WCU");
        assert!(provisioned.sort().is_none());
        assert!(TableInfo::of(&json!({"TableName": "x"})).is_err());
        assert_eq!(size(999), "999 B");
        assert_eq!(size(14_250), "14.2 kB");
        assert_eq!(size(5_300_000_000), "5.3 GB");
    }

    #[test]
    fn columns_are_the_key_and_every_attribute_that_was_seen() {
        let table = music();
        let mut attributes = Attributes::new();
        let items = [
            json!({"Artist": {"S": "A"}, "Song": {"S": "x"}, "Year": {"N": "1999"}, "Tags": {"SS": ["a"]}}),
            json!({"Artist": {"S": "B"}, "Song": {"S": "y"}, "Year": {"S": "unknown"}, "Awards": {"M": {}}}),
        ];
        note(&mut attributes, items.iter().filter_map(Value::as_object));
        let info = column_info(&table, &attributes);
        assert_eq!(
            info.iter()
                .map(|column| (column.name.as_str(), column.data_type.as_str()))
                .collect::<Vec<_>>(),
            [
                ("Artist", "string · partition key"),
                ("Song", "string · sort key"),
                ("Awards", "map"),
                ("Tags", "string set"),
                ("Year", "string | number"),
            ]
        );
        assert!(info[0].primary_key && info[1].primary_key && !info[0].nullable);
        assert!(
            info[2..]
                .iter()
                .all(|column| !column.primary_key && column.nullable)
        );
        let columns = result_columns(&table, &attributes);
        assert_eq!(
            row(&columns, items[0].as_object().unwrap()),
            [
                Cell::Text("A".into()),
                Cell::Text("x".into()),
                Cell::Null,
                Cell::Json(r#"{"SS":["a"]}"#.into()),
                Cell::Decimal("1999".into()),
            ]
        );
        assert_eq!(
            table.key_of(items[1].as_object().unwrap()),
            Some(json!({"Artist": {"S": "B"}, "Song": {"S": "y"}}))
        );
        assert_eq!(table.key_of(&Map::new()), None);
        let key = table
            .key_from(&[
                ("Song".into(), Cell::Text("x".into())),
                ("Artist".into(), Cell::Text("A".into())),
            ])
            .unwrap();
        assert_eq!(
            Value::Object(key),
            json!({"Artist": {"S": "A"}, "Song": {"S": "x"}})
        );
        assert_eq!(
            table
                .key_from(&[("Artist".into(), Cell::Text("A".into()))])
                .unwrap_err()
                .to_string(),
            "primary_key_required"
        );
    }

    #[test]
    fn a_filter_on_the_partition_key_is_a_query() {
        let table = music();
        let plan = Plan::new(&table, &request(&[("Artist", FilterOp::Contains, "Queen")])).unwrap();
        assert_eq!(plan.exact, Some(json!({"S": "Queen"})));
        assert!(!plan.exact_only && plan.exact_tests.is_empty() && plan.tests.len() == 1);
        let plan = Plan::new(&table, &request(&[("Artist", FilterOp::Equal, "Queen")])).unwrap();
        assert!(plan.exact_only);
        // Any other attribute is searched for, the sort key included.
        for column in ["Song", "Year"] {
            let plan = Plan::new(&table, &request(&[(column, FilterOp::Contains, "x")])).unwrap();
            assert!(plan.exact.is_none() && plan.tests.len() == 1, "{column}");
        }
        let plan = Plan::new(
            &table,
            &request(&[
                ("Year", FilterOp::Contains, "19"),
                ("Artist", FilterOp::Contains, "Queen"),
            ]),
        )
        .unwrap();
        assert!(plan.exact.is_some());
        assert_eq!(plan.exact_tests.len(), 1);
        assert_eq!(plan.exact_tests[0].attribute, "Year");
        // A number key takes numbers only.
        let numbered = TableInfo::of(&json!({
            "TableName": "Orders",
            "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
            "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "N"}],
        }))
        .unwrap();
        let mut page = request(&[("id", FilterOp::Contains, "119")]);
        assert_eq!(
            Plan::new(&numbered, &page).unwrap().exact,
            Some(json!({"N": "119"}))
        );
        page.filters[0].value = Cell::Text("11x".into());
        assert!(Plan::new(&numbered, &page).unwrap().exact.is_none());
    }

    #[test]
    fn a_filter_is_checked_here_and_narrowed_by_the_service_where_it_can_be() {
        let test = |op: FilterOp, value: &str| {
            Test::new(&Filter {
                column: "a".into(),
                op,
                value: Cell::Text(value.into()),
            })
        };
        let item = |value: Value| json!({ "a": value }).as_object().unwrap().clone();
        let contains = test(FilterOp::Contains, "GĘŚLĄ");
        assert!(contains.passes(&item(json!({"S": "Zażółć gęślą jaźń"}))));
        assert!(!contains.passes(&item(json!({"S": "gesla"}))));
        assert!(!contains.passes(&Map::new()));
        // A number is searched as text, and so is what has parts.
        assert!(test(FilterOp::Contains, "19").passes(&item(json!({"N": "119"}))));
        assert!(test(FilterOp::Contains, "rock").passes(&item(json!({"SS": ["Rock", "pop"]}))));
        assert!(test(FilterOp::Equal, "7").passes(&item(json!({"N": "7"}))));
        assert!(test(FilterOp::Greater, "9").passes(&item(json!({"N": "10"}))));
        assert!(test(FilterOp::IsNull, "").passes(&Map::new()));
        assert!(test(FilterOp::IsNotNull, "").passes(&item(json!({"NULL": true}))));
        // The service compares letters exactly, so it is only asked for a
        // text that has none; texts that cannot hold it then stay there.
        assert!(contains.needle().is_none());
        assert!(test(FilterOp::Contains, "row 12").needle().is_none());
        assert!(test(FilterOp::Equal, "100%").needle().is_none());
        assert_eq!(test(FilterOp::Contains, "100%").needle(), Some("100%"));
        let tests = [
            test(FilterOp::Contains, "100%"),
            test(FilterOp::Contains, "abc"),
            test(FilterOp::Contains, "7.5"),
        ];
        let (expression, names, values) = narrowing(&tests, &[]).unwrap();
        assert_eq!(
            expression,
            "attribute_exists(#f0) AND (contains(#f0, :f0) OR NOT attribute_type(#f0, :s)) AND attribute_exists(#f2) AND (contains(#f2, :f2) OR NOT attribute_type(#f2, :s))"
        );
        assert_eq!(names, json!({"#f0": "a", "#f2": "a"}));
        assert_eq!(
            values,
            json!({":f0": {"S": "100%"}, ":f2": {"S": "7.5"}, ":s": {"S": "S"}})
        );
        assert!(narrowing(&tests[1..2], &[]).is_none());
        // A Query may not filter on its own key.
        assert!(narrowing(&tests, &["a"]).is_none());
        assert_eq!(
            compare(&Cell::Decimal("9".into()), &Cell::Decimal("10".into())),
            Ordering::Less
        );
        assert_eq!(
            compare(&Cell::Null, &Cell::Decimal("-1".into())),
            Ordering::Less
        );
        assert_eq!(
            compare(&Cell::Decimal("5".into()), &Cell::Text("4".into())),
            Ordering::Less
        );
        assert_eq!(
            compare(&Cell::Text("a".into()), &Cell::Text("b".into())),
            Ordering::Less
        );
    }

    #[test]
    fn the_text_of_a_page_is_one_statement_that_reads_back_as_its_table() {
        let table = music();
        let dialect = &crate::drivers::dynamodb::DIALECT;
        let known = [TableRef {
            schema: String::new(),
            name: "Music".into(),
            kind: "table".into(),
        }];
        let mut filtered = request(&[("Year", FilterOp::Contains, "it's; -- x")]);
        filtered.offset = 1000;
        filtered.order_by = Some(("Song".into(), true));
        let exact = request(&[("Artist", FilterOp::Contains, "Queen")]);
        for (page, table) in [
            (request(&[]), Some(&table)),
            (request(&[]), None),
            (filtered.clone(), Some(&table)),
            (exact.clone(), Some(&table)),
        ] {
            let text = page_text(table, &page).unwrap();
            assert_eq!(sql::statements(dialect, &text).unwrap().len(), 1, "{text}");
            assert_eq!(
                sql::editable_table(dialect, &text, &known).map(|found| found.name),
                Some("Music".to_owned()),
                "{text}"
            );
            // The history leaves out a statement with this word in it.
            assert!(!text.to_lowercase().contains("token"), "{text}");
        }
        assert_eq!(
            page_text(Some(&table), &request(&[])).unwrap(),
            "SELECT * FROM \"Music\"\n-- 500 items a page"
        );
        assert!(
            page_text(Some(&table), &exact)
                .unwrap()
                .starts_with("SELECT * FROM \"Music\" WHERE \"Artist\" = 'Queen'\n-- 500 items a page\n-- a Query of that partition key; without an item of that key:")
        );
        let text = page_text(Some(&table), &filtered).unwrap();
        assert!(text.contains(", from item 1001: read on from the last key"));
        assert!(text.contains("-- filter \"Year\" contains 'it''s; -- x': checked on the items a Scan reads, at most 16 MB a page"));
        assert!(text.contains("-- ordered by \"Song\" DESC"));
        let mut overview = request(&[]);
        overview.container.name = OVERVIEW.into();
        assert!(
            page_text(None, &overview)
                .unwrap()
                .contains("DescribeTable")
        );
    }

    #[test]
    fn the_start_of_every_page_is_remembered() {
        let mut cursors = Cursors::default();
        let key = identity(&request(&[("Year", FilterOp::Contains, "x")]));
        assert_ne!(key, identity(&request(&[])));
        let cursor = cursors.of(&key, true);
        assert_eq!(cursor.nearest(0), (0, Mark::At(None)));
        let second = Mark::At(Some(json!({"Artist": {"S": "A"}, "Song": {"S": "x"}})));
        cursor.mark(500, second.clone());
        cursor.mark(1000, Mark::End);
        assert_eq!(cursor.nearest(999), (500, second));
        assert_eq!(cursor.nearest(5000), (1000, Mark::End));
        assert_eq!(cursors.of(&key, false).marks.len(), 3);
        assert_eq!(cursors.of(&key, true).marks.len(), 1);
        for index in 0..CURSORS + 3 {
            cursors.of(&format!("view {index}"), false);
        }
        assert_eq!(cursors.views.len(), CURSORS);
        cursors.clear();
        assert!(cursors.views.is_empty());
    }
}
