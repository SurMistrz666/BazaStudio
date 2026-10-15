//! A SELECT composed of several tables, as the visual query builder draws
//! it: the tables with their aliases, the relations between their columns
//! that become joins, the output columns with their summaries, conditions,
//! grouping, order and a page. The SQL is written in the dialect of the
//! connection: names are quoted by it, and the page is written the way it
//! pages. Conditions and their values are SQL the person types for their
//! own query, as in the SQL editor.
use crate::engine::{Dialect, Paging};
use crate::model::TableRef;
use crate::sql::{qualified, quote_identifier};
use anyhow::{Result, ensure};

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
    /// Joined to the condition before it with `OR` instead of `AND`.
    pub or: bool,
    pub field: Field,
    /// A summary of the field: in `HAVING`.
    pub aggregate: Option<Aggregate>,
    pub operator: Operator,
    /// SQL as typed: `5`, `'text'`, `1, 2, 3` for `IN`, `1 AND 9` for
    /// `BETWEEN`, or another column.
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

    fn conditions(&self, dialect: &Dialect, conditions: &[Condition]) -> Result<String> {
        let mut text = String::new();
        for (index, condition) in conditions.iter().enumerate() {
            if index > 0 {
                text.push_str(if condition.or { " OR " } else { " AND " });
            }
            let subject = self.expression(dialect, &condition.field, condition.aggregate)?;
            let value = condition.value.trim();
            ensure!(
                !condition.operator.takes_value() || !value.is_empty(),
                "builder_condition_value"
            );
            text.push_str(&match condition.operator {
                Operator::IsNull | Operator::IsNotNull => {
                    format!("{subject} {}", condition.operator.sql())
                }
                Operator::In | Operator::NotIn => {
                    format!("{subject} {} ({value})", condition.operator.sql())
                }
                other => format!("{subject} {} {value}", other.sql()),
            });
        }
        Ok(text)
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
        Ok(lines.join("\n"))
    }
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
WHERE `conf_summon`.`prob` BETWEEN 1 AND 50 OR `conf_summon`.`mob` IN (101, 102) AND `conf_pet_exp`.`exp` IS NOT NULL
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
}
