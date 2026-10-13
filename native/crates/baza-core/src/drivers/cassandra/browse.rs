//! How "Dane tabeli" is served on a database without `OFFSET`, without a
//! text search and with `ORDER BY` only inside a partition.
//!
//! * A page is read forward with the paging state of the driver. The place
//!   where every page starts is remembered ([`Mark`]), so "next", "previous"
//!   and "reload" continue from there; a page nobody visited yet is reached
//!   by reading up to it.
//! * A filter on the single column of a partition key is first asked of the
//!   server as an exact key (`WHERE key = value`), which touches one
//!   partition. Every other filter is checked here, on rows read in the
//!   order the cluster stores them, at most [`SCAN_ROWS`] per page: a
//!   filter never turns into an unbounded scan of the cluster.
//! * An order is made here, on up to [`SORT_ROWS`] rows.
use super::catalog::Table;
use super::value;
use crate::builder::{Filter, FilterOp};
use crate::drivers::PageRequest;
use crate::model::{Cell, ResultColumn};
use anyhow::{Result, anyhow, ensure};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Instant;

/// Most rows one page of a filtered table reads and checks.
pub(super) const SCAN_ROWS: usize = 100_000;
/// Most rows that are read to be put in order.
pub(super) const SORT_ROWS: usize = 20_000;
/// Rows asked of the server at once while filtering.
pub(super) const FETCH_ROWS: usize = 1000;
/// Most views (a table with its filter and order) whose pages are remembered.
const CURSORS: usize = 8;
/// Most pages of one view whose start is remembered.
const MARKS: usize = 4000;

/// One filter, checked on the cell of its column.
#[derive(Clone, Debug)]
pub(super) struct Test {
    pub column: usize,
    op: FilterOp,
    text: String,
    lower: String,
}

impl Test {
    pub(super) fn new(column: usize, filter: &Filter) -> Test {
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
        (Cell::Float(left), Cell::Float(right)) => {
            match (left.parse::<f64>(), right.parse::<f64>()) {
                (Ok(left), Ok(right)) => left.total_cmp(&right),
                _ => left.cmp(right),
            }
        }
        (
            Cell::Integer(left) | Cell::Decimal(left),
            Cell::Integer(right) | Cell::Decimal(right),
        ) => compare_text(left, right),
        (left, right) => left.display().cmp(&right.display()),
    }
}

/// How a page request is answered.
#[derive(Clone, Debug)]
pub(super) struct Plan {
    /// `SELECT * FROM "keyspace"."table"`.
    pub base: String,
    /// The same with the partition key of the filter: tried first.
    pub exact: Option<String>,
    /// Every filter, for the rows that are checked here.
    pub tests: Vec<Test>,
    /// The filters that remain to be checked when the exact key was used.
    pub exact_tests: Vec<Test>,
    /// Column to order by and whether descending.
    pub order: Option<(usize, bool)>,
}

impl Plan {
    pub(super) fn new(table: &Table, page: &PageRequest) -> Result<Plan> {
        ensure!(
            (1..=20_000).contains(&page.limit) && page.offset <= 1_000_000_000,
            "page_invalid"
        );
        let base = format!(
            "SELECT * FROM {}",
            value::qualified(&table.keyspace, &table.name)?
        );
        let mut tests = Vec::new();
        for filter in &page.filters {
            let column = table
                .index(&filter.column)
                .ok_or_else(|| anyhow!("cassandra_column_unknown"))?;
            tests.push(Test::new(column, filter));
        }
        // The one column of a partition key, asked for a value of its type:
        // a read of one partition instead of a walk through the table.
        let mut partition = table.partition_key();
        let key = match (partition.next(), partition.next()) {
            (Some(column), None) => Some(column),
            _ => None,
        };
        let mut exact = None;
        let mut exact_tests = tests.clone();
        if let Some(key) = key
            && let Some(position) = page.filters.iter().position(|filter| {
                filter.column == key.name
                    && matches!(filter.op, FilterOp::Contains | FilterOp::Equal)
                    && !matches!(filter.value, Cell::Null)
            })
            && let Ok(literal) = value::literal(
                &key.ty,
                &Cell::Text(page.filters[position].value.display()),
                &table.udts,
            )
        {
            exact = Some(format!(
                "{base} WHERE {} = {literal}",
                value::name(&key.name)?
            ));
            exact_tests.remove(position);
        }
        let order = match &page.order_by {
            Some((column, descending)) => Some((
                table
                    .index(column)
                    .ok_or_else(|| anyhow!("cassandra_column_unknown"))?,
                *descending,
            )),
            None => None,
        };
        Ok(Plan {
            base,
            exact,
            tests,
            exact_tests,
            order,
        })
    }

    /// The exact key answers the question by itself: a filter that asks
    /// for equality, not for a text somewhere in the value.
    pub(super) fn exact_only(page: &PageRequest, table: &Table) -> bool {
        let mut partition = table.partition_key();
        match (partition.next(), partition.next()) {
            (Some(key), None) => page
                .filters
                .iter()
                .any(|filter| filter.column == key.name && filter.op == FilterOp::Equal),
            _ => false,
        }
    }
}

/// What identifies a view: a table with its filter and its order. Pages of
/// the same view continue each other.
pub(super) fn identity(page: &PageRequest) -> String {
    let mut text = format!("{}\u{1f}{}", page.container.schema, page.container.name);
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
    /// At this paging state (`None`: the beginning), after this many rows.
    At {
        state: Option<Arc<[u8]>>,
        skip: usize,
    },
    /// Past the last row.
    End,
}

impl Mark {
    pub(super) const START: Mark = Mark::At {
        state: None,
        skip: 0,
    };
}

/// Which statement a view reads with, settled on its first page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    /// The partition of the key that was typed.
    Exact,
    /// The table in the order the cluster stores it.
    Walk,
}

/// What is remembered of a view between its pages.
#[derive(Debug)]
pub(super) struct Cursor {
    pub marks: BTreeMap<u64, Mark>,
    pub mode: Option<Mode>,
    /// All rows of an ordered view, in order.
    pub sorted: Option<Arc<Vec<Vec<Cell>>>>,
    pub columns: Vec<ResultColumn>,
    used: Instant,
}

impl Cursor {
    fn new() -> Cursor {
        Cursor {
            marks: BTreeMap::from([(0, Mark::START)]),
            mode: None,
            sorted: None,
            columns: Vec::new(),
            used: Instant::now(),
        }
    }

    /// Remember where the page at `offset` starts.
    pub(super) fn mark(&mut self, offset: u64, mark: Mark) {
        if self.marks.len() >= MARKS {
            // Keep the beginning and the far end: drop the middle.
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
            .map_or((0, Mark::START), |(offset, mark)| (*offset, mark.clone()))
    }
}

/// The views whose pages are remembered.
#[derive(Debug, Default)]
pub(super) struct Cursors {
    views: HashMap<String, Cursor>,
}

impl Cursors {
    /// The cursor of a view; a new one when the view starts over.
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

/// The columns of a page of a table: the type of each with its part in the
/// key, as the catalog names it.
pub(super) fn labelled(table: &Table, columns: &mut [ResultColumn]) {
    for column in columns {
        if let Some(known) = table.column(&column.name) {
            column.data_type = known.label();
        }
    }
}

/// What reading a page looks like as text: the statement the server gets,
/// and in comments what CQL has no words for.
pub(super) fn page_text(table: Option<&Table>, page: &PageRequest) -> Result<String> {
    ensure!(
        (1..=20_000).contains(&page.limit) && page.offset <= 1_000_000_000,
        "page_invalid"
    );
    let base = format!(
        "SELECT * FROM {}",
        value::qualified(&page.container.schema, &page.container.name)?
    );
    let plan = match table {
        Some(table) => Some(Plan::new(table, page)?),
        None => None,
    };
    let exact = plan.as_ref().and_then(|plan| plan.exact.clone());
    let exact_only = table.is_some_and(|table| Plan::exact_only(page, table));
    let mut text = exact.clone().unwrap_or(base);
    text.push_str(&format!("\nLIMIT {}", page.limit));
    if page.offset > 0 {
        text.push_str(&format!(
            "\n-- rows from {}: read on from the paging state of the page before (CQL has no OFFSET)",
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
                FilterOp::IsNull => "is null",
                FilterOp::IsNotNull => "is not null",
            };
            let value = match filter.op {
                FilterOp::IsNull | FilterOp::IsNotNull => String::new(),
                _ => format!(" '{}'", filter.value.display().replace('\'', "''")),
            };
            Ok(format!("{} {word}{value}", value::name(&filter.column)?))
        })
        .collect::<Result<_>>()?;
    if !described.is_empty() {
        let what = described.join(" and ");
        if exact.is_some() && exact_only {
            // Nothing more to say: the statement is the filter.
        } else if exact.is_some() {
            text.push_str(&format!(
                "\n-- without a row of that key: {what}, checked by the application on rows in stored order, at most {SCAN_ROWS} a page"
            ));
        } else {
            text.push_str(&format!(
                "\n-- filter {what}: checked by the application on rows in stored order, at most {SCAN_ROWS} a page"
            ));
        }
    }
    if let Some((column, descending)) = &page.order_by {
        text.push_str(&format!(
            "\n-- ordered by {} {} by the application, up to {SORT_ROWS} rows",
            value::name(column)?,
            if *descending { "DESC" } else { "ASC" }
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::cassandra::catalog::{Column, Role};
    use crate::drivers::cassandra::types::{CqlType, Udts};
    use crate::model::TableRef;
    use crate::sql;

    fn table(partition: &[(&str, &str)]) -> Table {
        let mut columns: Vec<Column> = partition
            .iter()
            .enumerate()
            .map(|(position, (name, ty))| Column {
                name: (*name).into(),
                ty: CqlType::parse(ty),
                role: Role::Partition(position as i64),
            })
            .collect();
        columns.push(Column {
            name: "at".into(),
            ty: CqlType::parse("timestamp"),
            role: Role::Clustering(0, true),
        });
        columns.push(Column {
            name: "label".into(),
            ty: CqlType::parse("text"),
            role: Role::Regular,
        });
        Table::new("shop", "Order Items", columns, Arc::new(Udts::new()), true)
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
                schema: "shop".into(),
                name: "Order Items".into(),
                kind: "table".into(),
            })
        }
    }

    #[test]
    fn a_filter_on_the_partition_key_is_asked_as_an_exact_key_first() {
        let single = table(&[("id", "int")]);
        let plan = Plan::new(&single, &request(&[("id", FilterOp::Contains, "119")])).unwrap();
        assert_eq!(
            plan.exact.as_deref(),
            Some("SELECT * FROM \"shop\".\"Order Items\" WHERE \"id\" = 119")
        );
        // Without a row of that key the table is searched for the text.
        assert_eq!(plan.tests.len(), 1);
        assert!(plan.exact_tests.is_empty());
        assert_eq!(plan.base, "SELECT * FROM \"shop\".\"Order Items\"");
        // What is not a value of the key type is only searched for.
        let plan = Plan::new(&single, &request(&[("id", FilterOp::Contains, "11x")])).unwrap();
        assert!(plan.exact.is_none() && plan.tests.len() == 1);
        // A uuid stands without quotes; nothing else can be put there.
        let uuid = table(&[("id", "uuid")]);
        let plan = Plan::new(
            &uuid,
            &request(&[(
                "id",
                FilterOp::Contains,
                "8E8B1C2A-4F3D-4E7B-9C1A-2B3C4D5E6F70",
            )]),
        )
        .unwrap();
        assert!(
            plan.exact
                .unwrap()
                .ends_with("WHERE \"id\" = 8e8b1c2a-4f3d-4e7b-9c1a-2b3c4d5e6f70")
        );
        let plan = Plan::new(&uuid, &request(&[("id", FilterOp::Contains, "1 OR true")])).unwrap();
        assert!(plan.exact.is_none());
        let text = table(&[("id", "text")]);
        let plan = Plan::new(&text, &request(&[("id", FilterOp::Contains, "it's")])).unwrap();
        assert!(plan.exact.unwrap().ends_with("WHERE \"id\" = 'it''s'"));
        // A key of several columns cannot be asked by one of them, and no
        // other column can be asked at all without a scan.
        let composite = table(&[("tenant", "int"), ("region", "text")]);
        let plan = Plan::new(&composite, &request(&[("tenant", FilterOp::Contains, "1")])).unwrap();
        assert!(plan.exact.is_none());
        let plan = Plan::new(&single, &request(&[("label", FilterOp::Contains, "x")])).unwrap();
        assert!(plan.exact.is_none());
        // A second filter stays a check on the rows of that partition.
        let plan = Plan::new(
            &single,
            &request(&[
                ("label", FilterOp::Contains, "x"),
                ("id", FilterOp::Equal, "7"),
            ]),
        )
        .unwrap();
        assert!(plan.exact.is_some());
        assert_eq!(plan.exact_tests.len(), 1);
        assert_eq!(plan.exact_tests[0].column, 2);
        assert!(Plan::exact_only(
            &request(&[("id", FilterOp::Equal, "7")]),
            &single
        ));
        assert!(!Plan::exact_only(
            &request(&[("id", FilterOp::Contains, "7")]),
            &single
        ));
        assert_eq!(
            Plan::new(&single, &request(&[("nope", FilterOp::Contains, "x")]))
                .unwrap_err()
                .to_string(),
            "cassandra_column_unknown"
        );
    }

    #[test]
    fn the_text_of_a_page_is_one_statement_that_reads_back_as_its_table() {
        let single = table(&[("id", "int")]);
        let dialect = &crate::drivers::cassandra::DIALECT;
        let known = [TableRef {
            schema: "shop".into(),
            name: "Order Items".into(),
            kind: "table".into(),
        }];
        let mut filtered = request(&[("label", FilterOp::Contains, "it's; -- x")]);
        filtered.offset = 1000;
        filtered.order_by = Some(("at".into(), true));
        let exact = request(&[("id", FilterOp::Contains, "7")]);
        for (page, table) in [
            (request(&[]), Some(&single)),
            (request(&[]), None),
            (filtered.clone(), Some(&single)),
            (filtered.clone(), None),
            (exact.clone(), Some(&single)),
        ] {
            let text = page_text(table, &page).unwrap();
            assert_eq!(sql::statements(dialect, &text).unwrap().len(), 1, "{text}");
            assert_eq!(
                sql::editable_table(dialect, &text, &known).map(|found| found.name),
                Some("Order Items".to_owned()),
                "{text}"
            );
            // The history leaves out a statement with this word in it.
            assert!(!text.to_lowercase().contains("token"), "{text}");
        }
        assert_eq!(
            page_text(Some(&single), &request(&[])).unwrap(),
            "SELECT * FROM \"shop\".\"Order Items\"\nLIMIT 500"
        );
        assert_eq!(
            page_text(Some(&single), &exact).unwrap(),
            "SELECT * FROM \"shop\".\"Order Items\" WHERE \"id\" = 7\nLIMIT 500\n-- without a row of that key: \"id\" contains '7', checked by the application on rows in stored order, at most 100000 a page"
        );
        let text = page_text(Some(&single), &filtered).unwrap();
        assert!(text.contains("-- rows from 1001: read on from the paging state"));
        assert!(
            text.contains("-- filter \"label\" contains 'it''s; -- x': checked by the application")
        );
        assert!(text.ends_with("-- ordered by \"at\" DESC by the application, up to 20000 rows"));
        let mut bad = request(&[]);
        bad.limit = 0;
        assert_eq!(
            page_text(None, &bad).unwrap_err().to_string(),
            "page_invalid"
        );
    }

    #[test]
    fn filters_are_checked_on_what_a_cell_shows() {
        let test = |op: FilterOp, value: &str| {
            Test::new(
                0,
                &Filter {
                    column: "c".into(),
                    op,
                    value: Cell::Text(value.into()),
                },
            )
        };
        let text = |value: &str| Cell::Text(value.into());
        let contains = test(FilterOp::Contains, "GĘŚLĄ");
        assert!(contains.passes(&text("Zażółć gęślą jaźń")));
        assert!(!contains.passes(&text("Zazolc gesla")) && !contains.passes(&Cell::Null));
        // Wildcards of other engines are plain characters here.
        assert!(test(FilterOp::Contains, "100%").passes(&text("100% _under_")));
        assert!(!test(FilterOp::Contains, "100%").passes(&text("1000")));
        assert!(test(FilterOp::Contains, "19").passes(&Cell::Integer("119".into())));
        assert!(test(FilterOp::Contains, "\"k\":5").passes(&Cell::Json(r#"{"k":5}"#.into())));
        assert!(test(FilterOp::Contains, "ff").passes(&Cell::Bytes(vec![0, 255])));
        assert!(test(FilterOp::Equal, "7").passes(&Cell::Integer("7".into())));
        assert!(!test(FilterOp::Equal, "7").passes(&Cell::Integer("77".into())));
        assert!(test(FilterOp::NotEqual, "7").passes(&Cell::Null));
        assert!(test(FilterOp::Greater, "9").passes(&Cell::Integer("10".into())));
        assert!(test(FilterOp::Less, "9.5").passes(&Cell::Decimal("9.25".into())));
        assert!(test(FilterOp::Greater, "b").passes(&text("c")));
        assert!(!test(FilterOp::Greater, "1").passes(&Cell::Null));
        assert!(test(FilterOp::IsNull, "").passes(&Cell::Null));
        assert!(test(FilterOp::IsNotNull, "").passes(&text("")));
    }

    #[test]
    fn cells_are_ordered_by_value() {
        let integer = |value: &str| Cell::Integer(value.into());
        assert_eq!(compare(&integer("9"), &integer("10")), Ordering::Less);
        assert_eq!(
            compare(&integer("-5"), &Cell::Decimal("-5.5".into())),
            Ordering::Greater
        );
        assert_eq!(compare(&Cell::Null, &integer("-1")), Ordering::Less);
        assert_eq!(
            compare(&Cell::Float("1e3".into()), &Cell::Float("200".into())),
            Ordering::Greater
        );
        assert_eq!(
            compare(&Cell::Text("a".into()), &Cell::Text("b".into())),
            Ordering::Less
        );
        assert_eq!(
            compare(&Cell::Bool(false), &Cell::Bool(true)),
            Ordering::Less
        );
        // Timestamps are shown in a form that orders as text.
        assert_eq!(
            compare(
                &Cell::Text("2024-05-01T12:00:00.000Z".into()),
                &Cell::Text("2024-05-01T12:00:00.001Z".into())
            ),
            Ordering::Less
        );
    }

    #[test]
    fn the_start_of_every_page_is_remembered() {
        let mut cursors = Cursors::default();
        let page = request(&[("label", FilterOp::Contains, "x")]);
        let key = identity(&page);
        assert_ne!(key, identity(&request(&[])));
        let cursor = cursors.of(&key, true);
        assert_eq!(cursor.nearest(0), (0, Mark::START));
        let second = Mark::At {
            state: Some(Arc::from(&b"abc"[..])),
            skip: 7,
        };
        cursor.mark(500, second.clone());
        cursor.mark(1000, Mark::End);
        assert_eq!(cursor.nearest(500), (500, second.clone()));
        assert_eq!(cursor.nearest(999), (500, second));
        assert_eq!(cursor.nearest(5000), (1000, Mark::End));
        // The same view again continues; a filter applied again starts over.
        assert_eq!(cursors.of(&key, false).marks.len(), 3);
        assert_eq!(cursors.of(&key, true).marks.len(), 1);
        // Only a few views are kept: the one used longest ago goes.
        for index in 0..CURSORS + 3 {
            cursors.of(&format!("view {index}"), false);
        }
        assert_eq!(cursors.views.len(), CURSORS);
        assert!(!cursors.views.contains_key(&key));
        cursors.clear();
        assert!(cursors.views.is_empty());
        let mut cursor = Cursor::new();
        for offset in 0..(MARKS as u64 + 50) {
            cursor.mark(offset * 500, Mark::START);
        }
        assert!(cursor.marks.len() <= MARKS && cursor.marks.contains_key(&0));
    }
}
