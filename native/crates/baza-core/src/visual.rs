//! A SELECT composed of several tables, as the visual query builder draws
//! it: the tables with their aliases, the relations between their columns
//! that become joins, the output columns with their summaries, conditions,
//! grouping, order and a page. The SQL is written in the dialect of the
//! connection: names are quoted by it, and the page is written the way it
//! pages.
//!
//! The value of a condition is SQL the person types: a number, a text in
//! quotes, another column or the list of `IN`. Each value is checked the way
//! the condition typed into "Dane tabeli" is checked
//! ([`typed_condition`]): no second statement, no comment, no subquery and no
//! function that could change data or the server. A value that is more than
//! one literal or one name stays in parentheses, so it never reaches past
//! its own condition. Several conditions are each put in parentheses and
//! joined from the top down: where `AND` meets `OR`, the conditions above
//! are grouped first, so the query runs in the order the list shows.
use crate::engine::{Dialect, Paging};
use crate::model::TableRef;
use crate::sql::{Lexeme, lex, qualified, quote_identifier, statement_ranges, typed_condition};
use anyhow::{Result, bail, ensure};

/// A table of the query and the name it goes by.
#[derive(Clone, Debug)]
pub struct Source {
    pub table: TableRef,
    pub alias: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JoinKind {
    #[default]
    Inner,
    Left,
    Right,
    Full,
}
impl JoinKind {
    pub const ALL: [JoinKind; 4] = [
        JoinKind::Inner,
        JoinKind::Left,
        JoinKind::Right,
        JoinKind::Full,
    ];
    pub fn sql(self) -> &'static str {
        match self {
            JoinKind::Inner => "INNER JOIN",
            JoinKind::Left => "LEFT JOIN",
            JoinKind::Right => "RIGHT JOIN",
            JoinKind::Full => "FULL JOIN",
        }
    }
    /// The same join seen from the other table.
    fn flipped(self) -> Self {
        match self {
            JoinKind::Left => JoinKind::Right,
            JoinKind::Right => JoinKind::Left,
            other => other,
        }
    }
}

/// A column of one of the sources; `*` is all of them.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Field {
    pub source: usize,
    pub column: String,
}

/// Two columns that are equal in the rows of the result. `Left` keeps every
/// row of the table of `left`, `Right` every row of `right`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relation {
    pub kind: JoinKind,
    pub left: Field,
    pub right: Field,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aggregate {
    Count,
    CountDistinct,
    Sum,
    Avg,
    Min,
    Max,
}
impl Aggregate {
    pub const ALL: [Aggregate; 6] = [
        Aggregate::Count,
        Aggregate::CountDistinct,
        Aggregate::Sum,
        Aggregate::Avg,
        Aggregate::Min,
        Aggregate::Max,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Aggregate::Count => "COUNT",
            Aggregate::CountDistinct => "COUNT DISTINCT",
            Aggregate::Sum => "SUM",
            Aggregate::Avg => "AVG",
            Aggregate::Min => "MIN",
            Aggregate::Max => "MAX",
        }
    }
    fn apply(self, inner: &str) -> String {
        match self {
            Aggregate::CountDistinct => format!("COUNT(DISTINCT {inner})"),
            other => format!("{}({inner})", other.label()),
        }
    }
}

/// A column of the result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub field: Field,
    pub aggregate: Option<Aggregate>,
    /// Empty for none.
    pub alias: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Operator {
    #[default]
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    Like,
    NotLike,
    In,
    NotIn,
    Between,
    IsNull,
    IsNotNull,
}
impl Operator {
    pub const ALL: [Operator; 13] = [
        Operator::Equal,
        Operator::NotEqual,
        Operator::Less,
        Operator::LessOrEqual,
        Operator::Greater,
        Operator::GreaterOrEqual,
        Operator::Like,
        Operator::NotLike,
        Operator::In,
        Operator::NotIn,
        Operator::Between,
        Operator::IsNull,
        Operator::IsNotNull,
    ];
    pub fn sql(self) -> &'static str {
        match self {
            Operator::Equal => "=",
            Operator::NotEqual => "<>",
            Operator::Less => "<",
            Operator::LessOrEqual => "<=",
            Operator::Greater => ">",
            Operator::GreaterOrEqual => ">=",
            Operator::Like => "LIKE",
            Operator::NotLike => "NOT LIKE",
            Operator::In => "IN",
            Operator::NotIn => "NOT IN",
            Operator::Between => "BETWEEN",
            Operator::IsNull => "IS NULL",
            Operator::IsNotNull => "IS NOT NULL",
        }
    }
    /// The operator compares with a value typed after it.
    pub fn takes_value(self) -> bool {
        !matches!(self, Operator::IsNull | Operator::IsNotNull)
    }
}

/// One condition of `WHERE` or `HAVING`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Condition {
    /// Joined to the conditions above it with `OR` instead of `AND`. The
    /// list is read from the top down: `a OR b AND c` is `(a OR b) AND c`
    /// (see [`grouping`]).
    pub or: bool,
    pub field: Field,
    /// A summary of the field: in `HAVING`.
    pub aggregate: Option<Aggregate>,
    pub operator: Operator,
    /// SQL as typed: `5`, `'text'`, `1, 2, 3` for `IN`, `1 AND 9` for
    /// `BETWEEN`, or another column. It is checked before it is written
    /// (see the module documentation).
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderTerm {
    pub field: Field,
    pub aggregate: Option<Aggregate>,
    pub descending: bool,
}

#[derive(Clone, Debug, Default)]
pub struct VisualQuery {
    pub sources: Vec<Source>,
    pub relations: Vec<Relation>,
    pub outputs: Vec<Output>,
    pub distinct: bool,
    pub conditions: Vec<Condition>,
    pub group_by: Vec<Field>,
    pub having: Vec<Condition>,
    pub order_by: Vec<OrderTerm>,
    /// Typed numbers; empty for none.
    pub limit: String,
    pub offset: String,
}

impl VisualQuery {
    /// Add a table under a name no other source has; returns its index.
    pub fn add_source(&mut self, table: TableRef) -> usize {
        let taken = |name: &str| {
            self.sources
                .iter()
                .any(|s| s.alias.eq_ignore_ascii_case(name))
        };
        let mut alias = table.name.clone();
        let mut n = 2;
        while taken(&alias) {
            alias = format!("{}_{n}", table.name);
            n += 1;
        }
        self.sources.push(Source { table, alias });
        self.sources.len() - 1
    }

    /// Remove a source with everything that refers to it.
    pub fn remove_source(&mut self, index: usize) {
        if index >= self.sources.len() {
            return;
        }
        self.sources.remove(index);
        let keep = |field: &Field| field.source != index;
        let shift = |field: &mut Field| {
            if field.source > index {
                field.source -= 1;
            }
        };
        self.relations.retain(|r| keep(&r.left) && keep(&r.right));
        self.outputs.retain(|o| keep(&o.field));
        self.conditions.retain(|c| keep(&c.field));
        self.group_by.retain(keep);
        self.having.retain(|c| keep(&c.field));
        self.order_by.retain(|o| keep(&o.field));
        for relation in &mut self.relations {
            shift(&mut relation.left);
            shift(&mut relation.right);
        }
        for output in &mut self.outputs {
            shift(&mut output.field);
        }
        for condition in self.conditions.iter_mut().chain(self.having.iter_mut()) {
            shift(&mut condition.field);
        }
        for field in &mut self.group_by {
            shift(field);
        }
        for term in &mut self.order_by {
            shift(&mut term.field);
        }
    }

    /// The field as SQL: qualified by its source when there are several.
    fn field(&self, dialect: &Dialect, field: &Field) -> Result<String> {
        let source = self
            .sources
            .get(field.source)
            .ok_or_else(|| anyhow::anyhow!("builder_field_invalid"))?;
        let column = if field.column == "*" {
            "*".to_owned()
        } else {
            quote_identifier(dialect, &field.column)?
        };
        Ok(if self.sources.len() > 1 {
            format!("{}.{column}", quote_identifier(dialect, &source.alias)?)
        } else {
            column
        })
    }

    fn expression(
        &self,
        dialect: &Dialect,
        field: &Field,
        aggregate: Option<Aggregate>,
    ) -> Result<String> {
        Ok(match aggregate {
            // Rows are counted as `COUNT(*)`; `t.*` is not a value elsewhere.
            Some(aggregate) if field.column == "*" => {
                ensure!(aggregate == Aggregate::Count, "aggregate_star_invalid");
                ensure!(field.source < self.sources.len(), "builder_field_invalid");
                "COUNT(*)".to_owned()
            }
            Some(aggregate) => aggregate.apply(&self.field(dialect, field)?),
            None => self.field(dialect, field)?,
        })
    }

    fn source(&self, dialect: &Dialect, index: usize) -> Result<String> {
        let source = &self.sources[index];
        let mut text = qualified(dialect, &source.table)?;
        if source.alias != source.table.name {
            text.push_str(&format!(
                " AS {}",
                quote_identifier(dialect, &source.alias)?
            ));
        }
        Ok(text)
    }

    /// One condition: its subject, its operator and its checked value.
    fn condition(&self, dialect: &Dialect, condition: &Condition) -> Result<String> {
        let subject = self.expression(dialect, &condition.field, condition.aggregate)?;
        let operator = condition.operator;
        let value = condition.value.trim();
        ensure!(
            !operator.takes_value() || !value.is_empty(),
            "builder_condition_value"
        );
        Ok(match operator {
            Operator::IsNull | Operator::IsNotNull => format!("{subject} {}", operator.sql()),
            // The list is checked whole and is written in its own parentheses.
            Operator::In | Operator::NotIn => format!(
                "{subject} {} {}",
                operator.sql(),
                checked_value(dialect, value)?
            ),
            Operator::Between => {
                let (low, high) = between_bounds(dialect, value)?;
                format!(
                    "{subject} BETWEEN {} AND {}",
                    operand(dialect, low)?,
                    operand(dialect, high)?
                )
            }
            other => format!("{subject} {} {}", other.sql(), operand(dialect, value)?),
        })
    }

    /// The conditions of `WHERE` or `HAVING`: each in parentheses when there
    /// are several, joined from the top down as [`grouping`] shows them.
    fn conditions(&self, dialect: &Dialect, conditions: &[Condition]) -> Result<String> {
        let several = conditions.len() > 1;
        let parts = conditions
            .iter()
            .map(|condition| {
                let text = self.condition(dialect, condition)?;
                Ok(if several { format!("({text})") } else { text })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(join_top_down(parts, conditions))
    }

    /// The order the sources are joined in: the first, then each one related
    /// to those already joined; one related to none comes last, crossed.
    fn join_order(&self) -> Vec<usize> {
        let mut order = vec![0];
        let mut left: Vec<usize> = (1..self.sources.len()).collect();
        while !left.is_empty() {
            let related = left.iter().position(|&candidate| {
                self.relations.iter().any(|r| {
                    (r.left.source == candidate && order.contains(&r.right.source))
                        || (r.right.source == candidate && order.contains(&r.left.source))
                })
            });
            order.push(left.remove(related.unwrap_or(0)));
        }
        order
    }

    /// The SELECT, one clause per line.
    pub fn sql(&self, dialect: &Dialect) -> Result<String> {
        ensure!(!self.sources.is_empty(), "builder_no_table");
        // A table is joined to itself through a second copy with its own
        // alias; a relation inside one copy would join nothing.
        ensure!(
            self.relations
                .iter()
                .all(|relation| relation.left.source != relation.right.source),
            "builder_relation_same_table"
        );
        let mut lines = Vec::new();
        let outputs = if self.outputs.is_empty() {
            "*".to_owned()
        } else {
            self.outputs
                .iter()
                .map(|output| {
                    let mut text = self.expression(dialect, &output.field, output.aggregate)?;
                    if !output.alias.trim().is_empty() {
                        text.push_str(&format!(
                            " AS {}",
                            quote_identifier(dialect, output.alias.trim())?
                        ));
                    }
                    Ok(text)
                })
                .collect::<Result<Vec<_>>>()?
                .join(", ")
        };
        let limit = number(&self.limit)?;
        let offset = number(&self.offset)?;
        let top = match (dialect.paging, limit) {
            (Paging::Top, Some(limit)) => {
                ensure!(offset.is_none(), "page_offset_unsupported");
                format!("TOP {limit} ")
            }
            _ => String::new(),
        };
        lines.push(format!(
            "SELECT {}{top}{outputs}",
            if self.distinct { "DISTINCT " } else { "" }
        ));
        let order = self.join_order();
        lines.push(format!("FROM {}", self.source(dialect, order[0])?));
        for (position, &index) in order.iter().enumerate().skip(1) {
            let joined = &order[..position];
            let mut kind = None;
            let mut on = Vec::new();
            for relation in &self.relations {
                let (this, other, flipped) = if relation.right.source == index {
                    (&relation.right, &relation.left, false)
                } else if relation.left.source == index {
                    (&relation.left, &relation.right, true)
                } else {
                    continue;
                };
                if !joined.contains(&other.source) {
                    continue;
                }
                // The table joined now is the right one of `JOIN`.
                kind.get_or_insert(if flipped {
                    relation.kind.flipped()
                } else {
                    relation.kind
                });
                on.push(format!(
                    "{} = {}",
                    self.field(dialect, other)?,
                    self.field(dialect, this)?
                ));
            }
            lines.push(match kind {
                Some(kind) => format!(
                    "{} {} ON {}",
                    kind.sql(),
                    self.source(dialect, index)?,
                    on.join(" AND ")
                ),
                None => format!("CROSS JOIN {}", self.source(dialect, index)?),
            });
        }
        if !self.conditions.is_empty() {
            lines.push(format!(
                "WHERE {}",
                self.conditions(dialect, &self.conditions)?
            ));
        }
        if !self.group_by.is_empty() {
            let fields = self
                .group_by
                .iter()
                .map(|field| self.field(dialect, field))
                .collect::<Result<Vec<_>>>()?;
            lines.push(format!("GROUP BY {}", fields.join(", ")));
        }
        if !self.having.is_empty() {
            lines.push(format!(
                "HAVING {}",
                self.conditions(dialect, &self.having)?
            ));
        }
        let mut terms = self
            .order_by
            .iter()
            .map(|term| {
                Ok(format!(
                    "{} {}",
                    self.expression(dialect, &term.field, term.aggregate)?,
                    if term.descending { "DESC" } else { "ASC" }
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        match dialect.paging {
            Paging::LimitOffset => {
                if !terms.is_empty() {
                    lines.push(format!("ORDER BY {}", terms.join(", ")));
                }
                if let Some(limit) = limit {
                    lines.push(format!("LIMIT {limit}"));
                }
                if let Some(offset) = offset {
                    ensure!(limit.is_some(), "builder_offset_needs_limit");
                    lines.push(format!("OFFSET {offset}"));
                }
            }
            Paging::OffsetFetch { default_order } => {
                if terms.is_empty()
                    && (limit.is_some() || offset.is_some())
                    && let Some(order) = default_order
                {
                    terms.push(order.to_owned());
                }
                if !terms.is_empty() {
                    lines.push(format!("ORDER BY {}", terms.join(", ")));
                }
                if limit.is_some() || offset.is_some() {
                    lines.push(format!("OFFSET {} ROWS", offset.unwrap_or(0)));
                }
                if let Some(limit) = limit {
                    lines.push(format!("FETCH NEXT {limit} ROWS ONLY"));
                }
            }
            Paging::Top | Paging::None => {
                if !terms.is_empty() {
                    lines.push(format!("ORDER BY {}", terms.join(", ")));
                }
                ensure!(
                    dialect.paging == Paging::Top || (limit.is_none() && offset.is_none()),
                    "page_offset_unsupported"
                );
            }
        }
        let text = lines.join("\n");
        // Whatever was typed into the parts, the whole is one statement.
        ensure!(
            statement_ranges(dialect, &text)?.len() == 1,
            "builder_value_statements"
        );
        Ok(text)
    }
}

/// How the conditions are joined, by their numbers from 1 as the list shows
/// them: `(1 OR 2) AND 3`. The SQL groups them the same way.
pub fn grouping(conditions: &[Condition]) -> String {
    join_top_down(
        (1..=conditions.len()).map(|number| number.to_string()),
        conditions,
    )
}

/// The parts joined by the `AND` or `OR` of their conditions, from the top
/// down: where the joining word changes, everything above is put in
/// parentheses first. The word of the first condition is not used.
fn join_top_down(parts: impl IntoIterator<Item = String>, conditions: &[Condition]) -> String {
    let mut text = String::new();
    let mut last = None;
    for (index, (part, condition)) in parts.into_iter().zip(conditions).enumerate() {
        if index > 0 {
            if last.is_some_and(|or| or != condition.or) {
                text = format!("({text})");
            }
            text.push_str(if condition.or { " OR " } else { " AND " });
            last = Some(condition.or);
        }
        text.push_str(&part);
    }
    text
}

/// A typed value checked as [`typed_condition`] checks a condition, and in
/// parentheses. Its refusals are reported as the builder's own codes.
fn checked_value(dialect: &Dialect, text: &str) -> Result<String> {
    ensure!(!text.trim().is_empty(), "builder_condition_value");
    typed_condition(dialect, text).map_err(|error| {
        let code = error.to_string();
        match code.strip_prefix("filter_condition_") {
            Some(rest) => anyhow::anyhow!("builder_value_{rest}"),
            None => anyhow::anyhow!(code),
        }
    })
}

/// A value after an operator: checked, and in parentheses unless it is one
/// literal or one name (`5`, `'text'`, `o.id`), which cannot bind to
/// anything around it.
fn operand(dialect: &Dialect, text: &str) -> Result<String> {
    let checked = checked_value(dialect, text)?;
    let text = text.trim();
    Ok(if single_term(dialect, text) {
        text.to_owned()
    } else {
        checked
    })
}

/// The text is one literal, or one name with the names it is qualified by.
fn single_term(dialect: &Dialect, text: &str) -> bool {
    let mut tokens = Vec::new();
    let read = lex(dialect, text, |kind, range| {
        if kind != Lexeme::Space {
            tokens.push((kind, &text[range]));
        }
    });
    if read.is_err() {
        return false;
    }
    match tokens.as_slice() {
        [(Lexeme::Literal, _)] => true,
        names => {
            names.len() % 2 == 1
                && names.iter().enumerate().all(|(index, &(kind, token))| {
                    if index % 2 == 0 {
                        matches!(kind, Lexeme::Word | Lexeme::Quoted)
                    } else {
                        kind == Lexeme::Symbol && token == "."
                    }
                })
        }
    }
}

/// The two values of `BETWEEN`, typed as `1 AND 10`: the text on each side of
/// the one `AND` outside parentheses and quotes.
fn between_bounds<'a>(dialect: &Dialect, text: &'a str) -> Result<(&'a str, &'a str)> {
    let mut depth = 0i64;
    let mut ands = Vec::new();
    lex(dialect, text, |kind, range| {
        let token = &text[range.clone()];
        match kind {
            Lexeme::Symbol if token == "(" => depth += 1,
            Lexeme::Symbol if token == ")" => depth -= 1,
            Lexeme::Word if depth == 0 && token.eq_ignore_ascii_case("AND") => ands.push(range),
            _ => {}
        }
    })
    .map_err(|_| anyhow::anyhow!("builder_value_unterminated"))?;
    let [and] = ands.as_slice() else {
        bail!("builder_between_value");
    };
    let (low, high) = (text[..and.start].trim(), text[and.end..].trim());
    ensure!(!low.is_empty() && !high.is_empty(), "builder_between_value");
    Ok((low, high))
}

/// A typed count of rows: empty for none.
fn number(text: &str) -> Result<Option<u64>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<u64>()
        .map(Some)
        .map_err(|_| anyhow::anyhow!("builder_limit_invalid"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::{
        mssql::DIALECT as MSSQL, mysql::DIALECT as MYSQL, postgres::DIALECT as POSTGRES,
    };

    fn table(name: &str) -> TableRef {
        TableRef {
            schema: "game".into(),
            name: name.into(),
            kind: "table".into(),
        }
    }
    fn field(source: usize, column: &str) -> Field {
        Field {
            source,
            column: column.into(),
        }
    }

    #[test]
    fn related_tables_become_joins() {
        let mut query = VisualQuery::default();
        let summon = query.add_source(table("conf_summon"));
        let exp = query.add_source(table("conf_pet_exp"));
        query.relations.push(Relation {
            kind: JoinKind::Inner,
            left: field(summon, "vnum"),
            right: field(exp, "exp"),
        });
        assert_eq!(
            query.sql(&MYSQL).unwrap(),
            "SELECT *\nFROM `game`.`conf_summon`\nINNER JOIN `game`.`conf_pet_exp` ON `conf_summon`.`vnum` = `conf_pet_exp`.`exp`"
        );
        query.outputs = vec![
            Output {
                field: field(summon, "mob"),
                aggregate: None,
                alias: String::new(),
            },
            Output {
                field: field(exp, "*"),
                aggregate: Some(Aggregate::Count),
                alias: "n".into(),
            },
        ];
        query.distinct = true;
        query.conditions = vec![
            Condition {
                or: false,
                field: field(summon, "prob"),
                aggregate: None,
                operator: Operator::Between,
                value: "1 AND 50".into(),
            },
            Condition {
                or: true,
                field: field(summon, "mob"),
                aggregate: None,
                operator: Operator::In,
                value: "101, 102".into(),
            },
            Condition {
                or: false,
                field: field(exp, "exp"),
                aggregate: None,
                operator: Operator::IsNotNull,
                value: String::new(),
            },
        ];
        query.group_by = vec![field(summon, "mob")];
        query.having = vec![Condition {
            or: false,
            field: field(exp, "*"),
            aggregate: Some(Aggregate::Count),
            operator: Operator::Greater,
            value: "1".into(),
        }];
        query.order_by = vec![OrderTerm {
            field: field(exp, "*"),
            aggregate: Some(Aggregate::Count),
            descending: true,
        }];
        query.limit = "10".into();
        query.offset = "20".into();
        assert_eq!(
            query.sql(&MYSQL).unwrap(),
            "SELECT DISTINCT `conf_summon`.`mob`, COUNT(*) AS `n`
FROM `game`.`conf_summon`
INNER JOIN `game`.`conf_pet_exp` ON `conf_summon`.`vnum` = `conf_pet_exp`.`exp`
WHERE ((`conf_summon`.`prob` BETWEEN 1 AND 50) OR (`conf_summon`.`mob` IN (101, 102))) AND (`conf_pet_exp`.`exp` IS NOT NULL)
GROUP BY `conf_summon`.`mob`
HAVING COUNT(*) > 1
ORDER BY COUNT(*) DESC
LIMIT 10
OFFSET 20"
        );
    }

    #[test]
    fn join_order_follows_the_relations_and_keeps_left_joins() {
        let mut query = VisualQuery::default();
        let a = query.add_source(table("a"));
        let b = query.add_source(table("b"));
        let c = query.add_source(table("c"));
        // `b` is related to `c` only, `c` to `a`: `c` is joined before `b`.
        query.relations.push(Relation {
            kind: JoinKind::Left,
            left: field(b, "id"),
            right: field(c, "b_id"),
        });
        query.relations.push(Relation {
            kind: JoinKind::Inner,
            left: field(a, "id"),
            right: field(c, "a_id"),
        });
        let sql = query.sql(&POSTGRES).unwrap();
        assert_eq!(
            sql,
            "SELECT *\nFROM \"game\".\"a\"\nINNER JOIN \"game\".\"c\" ON \"a\".\"id\" = \"c\".\"a_id\"\nRIGHT JOIN \"game\".\"b\" ON \"c\".\"b_id\" = \"b\".\"id\""
        );
        // A table related to none is crossed; one used twice has its own name.
        let d = query.add_source(table("a"));
        assert_eq!(query.sources[d].alias, "a_2");
        assert!(
            query
                .sql(&POSTGRES)
                .unwrap()
                .ends_with("\nCROSS JOIN \"game\".\"a\" AS \"a_2\"")
        );
        // Removing a source takes what refers to it and renumbers the rest.
        query.remove_source(b);
        assert_eq!(query.relations.len(), 1);
        assert_eq!(query.relations[0].right.source, 1);
    }

    #[test]
    fn a_page_is_written_the_way_the_dialect_pages() {
        let mut query = VisualQuery::default();
        query.add_source(table("orders"));
        query.limit = "5".into();
        assert_eq!(
            query.sql(&MSSQL).unwrap(),
            "SELECT *\nFROM [game].[orders]\nORDER BY 1\nOFFSET 0 ROWS\nFETCH NEXT 5 ROWS ONLY"
        );
        query.limit = "five".into();
        assert_eq!(
            query.sql(&MYSQL).unwrap_err().to_string(),
            "builder_limit_invalid"
        );
        query.limit.clear();
        query.conditions.push(Condition {
            or: false,
            field: field(0, "id"),
            aggregate: None,
            operator: Operator::Equal,
            value: " ".into(),
        });
        assert_eq!(
            query.sql(&MYSQL).unwrap_err().to_string(),
            "builder_condition_value"
        );
        assert_eq!(
            VisualQuery::default().sql(&MYSQL).unwrap_err().to_string(),
            "builder_no_table"
        );
    }

    fn condition(or: bool, column: &str, operator: Operator, value: &str) -> Condition {
        Condition {
            or,
            field: field(0, column),
            aggregate: None,
            operator,
            value: value.into(),
        }
    }

    /// The `WHERE` of a query on one table with these conditions.
    fn where_of(conditions: Vec<Condition>) -> Result<String> {
        let mut query = VisualQuery::default();
        query.add_source(table("orders"));
        query.conditions = conditions;
        let sql = query.sql(&MYSQL)?;
        Ok(sql
            .lines()
            .find_map(|line| line.strip_prefix("WHERE "))
            .unwrap_or_default()
            .to_owned())
    }

    fn refusal(operator: Operator, value: &str) -> String {
        where_of(vec![condition(false, "id", operator, value)])
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn a_value_cannot_reach_past_its_condition() {
        // A second statement, in any operator.
        for operator in [Operator::Equal, Operator::Like, Operator::In] {
            assert_eq!(
                refusal(operator, "1; DELETE FROM orders"),
                "builder_value_statements"
            );
        }
        assert_eq!(
            refusal(Operator::Between, "1 AND 5; DROP TABLE orders"),
            "builder_value_statements"
        );
        // A list that closes its parentheses to add a condition of its own.
        assert_eq!(
            refusal(Operator::In, "1) OR (1=1"),
            "builder_value_parentheses"
        );
        assert_eq!(
            refusal(Operator::NotIn, "1)) OR ((1=1"),
            "builder_value_parentheses"
        );
        assert_eq!(
            refusal(Operator::Equal, "1) OR (1=1"),
            "builder_value_parentheses"
        );
        // Comments, which could hide the rest of the query.
        assert_eq!(refusal(Operator::Equal, "1 -- x"), "builder_value_comment");
        assert_eq!(refusal(Operator::Equal, "1 # x"), "builder_value_comment");
        assert_eq!(refusal(Operator::In, "1 /* x */"), "builder_value_comment");
        assert_eq!(refusal(Operator::Equal, "'a"), "builder_value_unterminated");
        // A subquery and a function that does more than compute a value.
        assert_eq!(
            refusal(Operator::In, "SELECT id FROM users"),
            "builder_value_word [SELECT]"
        );
        assert_eq!(
            refusal(Operator::Equal, "SLEEP(5)"),
            "builder_value_function [SLEEP]"
        );
        // A semicolon in a text is text.
        assert_eq!(
            where_of(vec![condition(false, "note", Operator::Equal, "'a;b'")]).unwrap(),
            "`note` = 'a;b'"
        );
    }

    #[test]
    fn a_value_of_more_than_one_term_stays_in_parentheses() {
        let one = |operator, value: &str| {
            where_of(vec![condition(false, "id", operator, value)]).unwrap()
        };
        assert_eq!(one(Operator::Equal, "5"), "`id` = 5");
        assert_eq!(one(Operator::Equal, "orders.id"), "`id` = orders.id");
        assert_eq!(one(Operator::Equal, "`o`.`id`"), "`id` = `o`.`id`");
        assert_eq!(one(Operator::Less, "-5"), "`id` < (-5)");
        assert_eq!(one(Operator::Equal, "5 OR 1=1"), "`id` = (5 OR 1=1)");
        assert_eq!(
            one(Operator::Like, "LOWER('%A%')"),
            "`id` LIKE (LOWER('%A%'))"
        );
        assert_eq!(one(Operator::In, "1, 2, 3"), "`id` IN (1, 2, 3)");
        assert_eq!(
            one(Operator::Between, "1 AND (2 + 3)"),
            "`id` BETWEEN 1 AND ((2 + 3))"
        );
        assert_eq!(
            one(Operator::Between, "'a AND b' AND 'c'"),
            "`id` BETWEEN 'a AND b' AND 'c'"
        );
        for value in ["1", "1 AND 2 AND 3", "AND 2", "1 AND"] {
            assert_eq!(
                refusal(Operator::Between, value),
                "builder_between_value",
                "{value}"
            );
        }
    }

    #[test]
    fn mixed_and_or_are_grouped_from_the_top_down() {
        let conditions = vec![
            condition(false, "a", Operator::Equal, "1"),
            condition(true, "b", Operator::Equal, "2"),
            condition(false, "c", Operator::IsNull, ""),
            condition(true, "d", Operator::Equal, "4"),
        ];
        assert_eq!(grouping(&conditions), "((1 OR 2) AND 3) OR 4");
        assert_eq!(
            where_of(conditions).unwrap(),
            "(((`a` = 1) OR (`b` = 2)) AND (`c` IS NULL)) OR (`d` = 4)"
        );
        // The same word all the way needs no grouping.
        let conditions = vec![
            condition(false, "a", Operator::Equal, "1"),
            condition(false, "b", Operator::Equal, "2"),
            condition(false, "c", Operator::Equal, "3"),
        ];
        assert_eq!(grouping(&conditions), "1 AND 2 AND 3");
        assert_eq!(
            where_of(conditions).unwrap(),
            "(`a` = 1) AND (`b` = 2) AND (`c` = 3)"
        );
        // The word of the first condition joins it to nothing.
        let conditions = vec![condition(true, "a", Operator::Equal, "1")];
        assert_eq!(grouping(&conditions), "1");
        assert_eq!(where_of(conditions).unwrap(), "`a` = 1");
    }

    #[test]
    fn a_relation_inside_one_table_is_refused() {
        let mut query = VisualQuery::default();
        let a = query.add_source(table("a"));
        query.add_source(table("b"));
        query.relations.push(Relation {
            kind: JoinKind::Inner,
            left: field(a, "id"),
            right: field(a, "parent_id"),
        });
        assert_eq!(
            query.sql(&MYSQL).unwrap_err().to_string(),
            "builder_relation_same_table"
        );
        // A second copy of the table joins it to itself.
        let copy = query.add_source(table("a"));
        query.relations[0].right = field(copy, "parent_id");
        assert!(
            query
                .sql(&MYSQL)
                .unwrap()
                .contains("INNER JOIN `game`.`a` AS `a_2` ON `a`.`id` = `a_2`.`parent_id`")
        );
    }
}
