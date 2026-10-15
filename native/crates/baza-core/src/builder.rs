use crate::drivers::PageRequest;
#[cfg(test)]
use crate::drivers::{
    mysql::DIALECT as MYSQL, postgres::DIALECT as POSTGRES, sqlite::DIALECT as SQLITE,
};
use crate::engine::{Dialect, Paging};
use crate::{model::*, sql::*};
#[derive(Clone, Debug)]
pub struct CreateColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
}
pub fn create_table(
    dialect: &Dialect,
    table: &TableRef,
    columns: &[CreateColumn],
) -> anyhow::Result<String> {
    anyhow::ensure!(
        !columns.is_empty() && columns.len() <= 128,
        "table_columns_invalid"
    );
    let mut names = std::collections::HashSet::new();
    let allowed = dialect.column_types;
    let mut definitions = Vec::new();
    for c in columns {
        anyhow::ensure!(names.insert(c.name.clone()), "column_name_duplicate");
        anyhow::ensure!(
            allowed.contains(&c.data_type.as_str()),
            "column_type_invalid"
        );
        definitions.push(format!(
            "  {} {}{}",
            quote_identifier(dialect, &c.name)?,
            c.data_type,
            if c.nullable && !c.primary_key {
                ""
            } else {
                " NOT NULL"
            }
        ));
    }
    let keys = columns
        .iter()
        .filter(|c| c.primary_key)
        .map(|c| quote_identifier(dialect, &c.name))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if !keys.is_empty() {
        definitions.push(format!("  PRIMARY KEY ({})", keys.join(", ")));
    }
    Ok(format!(
        "CREATE TABLE {} (\n{}\n)",
        qualified(dialect, table)?,
        definitions.join(",\n")
    ))
}
#[derive(Clone, Debug, PartialEq)]
pub enum FilterOp {
    Equal,
    NotEqual,
    Contains,
    Greater,
    Less,
    IsNull,
    IsNotNull,
}
#[derive(Clone, Debug)]
pub struct Filter {
    pub column: String,
    pub op: FilterOp,
    pub value: Cell,
}
#[derive(Clone, Debug)]
pub struct Projection {
    pub column: String,
    pub aggregate: Option<String>,
    pub alias: Option<String>,
}
#[derive(Clone, Debug)]
pub struct ColumnRef {
    pub table: TableRef,
    pub name: String,
}
#[derive(Clone, Debug)]
pub struct Join {
    pub table: TableRef,
    pub left: ColumnRef,
    pub right: ColumnRef,
    pub left_join: bool,
}
#[derive(Clone, Debug)]
pub struct QueryBuilder {
    pub table: TableRef,
    pub projections: Vec<Projection>,
    pub filters: Vec<Filter>,
    /// A condition typed by the user: see [`PageRequest::condition`].
    pub condition: Option<String>,
    pub joins: Vec<Join>,
    pub group_by: Vec<String>,
    pub order_by: Option<(String, bool)>,
    /// Further columns to order by: see [`PageRequest::then_by`].
    pub then_by: Vec<(String, bool)>,
    pub limit: u32,
    pub offset: u64,
}
impl QueryBuilder {
    pub fn table(t: TableRef) -> Self {
        Self {
            table: t,
            projections: vec![],
            filters: vec![],
            condition: None,
            joins: vec![],
            group_by: vec![],
            order_by: None,
            then_by: vec![],
            limit: 500,
            offset: 0,
        }
    }
    /// The first page of a table or view with a filter and an order: what
    /// "Dane tabeli" shows.
    pub fn page(request: &PageRequest) -> Self {
        Self {
            filters: request.filters.clone(),
            condition: request.condition.clone(),
            order_by: request.order_by.clone(),
            then_by: request.then_by.clone(),
            limit: request.limit,
            offset: request.offset,
            ..Self::table(request.container.clone())
        }
    }
    pub fn sql(&self, dialect: &Dialect) -> anyhow::Result<String> {
        anyhow::ensure!(
            self.limit > 0 && self.limit <= 20_000 && self.offset <= 1_000_000_000,
            "page_invalid"
        );
        let qcol = |s: &str| quote_identifier(dialect, s);
        let qref = |c: &ColumnRef| -> anyhow::Result<String> {
            Ok(format!(
                "{}.{}",
                qualified(dialect, &c.table)?,
                quote_identifier(dialect, &c.name)?
            ))
        };
        let select = if self.projections.is_empty() {
            "*".into()
        } else {
            self.projections
                .iter()
                .map(|p| {
                    let col = if p.column == "*" {
                        "*".into()
                    } else {
                        qcol(&p.column)?
                    };
                    let mut expr = if let Some(a) = &p.aggregate {
                        let a = a.to_uppercase();
                        anyhow::ensure!(
                            ["COUNT", "SUM", "AVG", "MIN", "MAX"].contains(&a.as_str()),
                            "aggregate_invalid"
                        );
                        anyhow::ensure!(p.column != "*" || a == "COUNT", "aggregate_star_invalid");
                        format!("{a}({col})")
                    } else {
                        col
                    };
                    if let Some(a) = &p.alias {
                        expr.push_str(&format!(" AS {}", quote_identifier(dialect, a)?));
                    }
                    Ok(expr)
                })
                .collect::<anyhow::Result<Vec<String>>>()?
                .join(", ")
        };
        let mut s = format!("SELECT {select}\nFROM {}", qualified(dialect, &self.table)?);
        for j in &self.joins {
            s.push_str(&format!(
                "\n{} JOIN {} ON {} = {}",
                if j.left_join { "LEFT" } else { "INNER" },
                qualified(dialect, &j.table)?,
                qref(&j.left)?,
                qref(&j.right)?
            ));
        }
        if !self.filters.is_empty() {
            let f = self
                .filters
                .iter()
                .map(|f| {
                    let col = qcol(&f.column)?;
                    Ok(match f.op {
                        FilterOp::IsNull => format!("{col} IS NULL"),
                        FilterOp::IsNotNull => format!("{col} IS NOT NULL"),
                        FilterOp::Contains => {
                            let pattern = literal(
                                dialect,
                                &Cell::Text(format!("%{}%", like_pattern(&f.value.display()))),
                            )?;
                            // Where LIKE takes text only, or tells letter
                            // case apart, the dialect names a cast and an
                            // operator; elsewhere numbers are searched as
                            // text and case is ignored by the collation.
                            let subject = match dialect.contains.cast {
                                Some((before, after)) => format!("{before}{col}{after}"),
                                None => col,
                            };
                            let pattern = match dialect.contains.pattern {
                                Some((before, after)) => format!("{before}{pattern}{after}"),
                                None => pattern,
                            };
                            format!(
                                "{subject} {} {pattern} ESCAPE '{LIKE_ESCAPE}'",
                                dialect.contains.operator
                            )
                        }
                        FilterOp::Equal if matches!(f.value, Cell::Null) => {
                            format!("{col} IS NULL")
                        }
                        FilterOp::NotEqual if matches!(f.value, Cell::Null) => {
                            format!("{col} IS NOT NULL")
                        }
                        _ => format!(
                            "{col} {} {}",
                            match f.op {
                                FilterOp::Equal => "=",
                                FilterOp::NotEqual => "<>",
                                FilterOp::Greater => ">",
                                FilterOp::Less => "<",
                                _ => unreachable!(),
                            },
                            literal(dialect, &f.value)?
                        ),
                    })
                })
                .collect::<anyhow::Result<Vec<String>>>()?;
            s.push_str(&format!("\nWHERE {}", f.join(" AND ")));
        }
        if let Some(typed) = &self.condition {
            let typed = typed_condition(dialect, typed)?;
            s.push_str(&if self.filters.is_empty() {
                format!("\nWHERE {typed}")
            } else {
                format!(" AND {typed}")
            });
        }
        if !self.group_by.is_empty() {
            s.push_str(&format!(
                "\nGROUP BY {}",
                self.group_by
                    .iter()
                    .map(|v| qcol(v))
                    .collect::<anyhow::Result<Vec<_>>>()?
                    .join(", ")
            ));
        }
        if let Some(first) = &self.order_by {
            let terms = std::iter::once(first)
                .chain(&self.then_by)
                .map(|(c, desc)| {
                    Ok(format!(
                        "{} {}",
                        qcol(c)?,
                        if *desc { "DESC" } else { "ASC" }
                    ))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            s.push_str(&format!("\nORDER BY {}", terms.join(", ")));
        }
        match dialect.paging {
            Paging::LimitOffset => {
                s.push_str(&format!("\nLIMIT {} OFFSET {}", self.limit, self.offset));
            }
            Paging::OffsetFetch { default_order } => {
                if let (None, Some(order)) = (&self.order_by, default_order) {
                    s.push_str(&format!("\nORDER BY {order}"));
                }
                s.push_str(&format!(
                    "\nOFFSET {} ROWS FETCH NEXT {} ROWS ONLY",
                    self.offset, self.limit
                ));
            }
            Paging::Top => {
                anyhow::ensure!(self.offset == 0, "page_offset_unsupported");
                s = s.replacen("SELECT ", &format!("SELECT TOP {} ", self.limit), 1);
            }
            Paging::None => {
                anyhow::ensure!(self.offset == 0, "page_offset_unsupported");
            }
        }
        Ok(s)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escapes_filters() {
        let mut b = QueryBuilder::table(TableRef {
            schema: "main".into(),
            name: "a".into(),
            kind: "table".into(),
        });
        b.filters.push(Filter {
            column: "name".into(),
            op: FilterOp::Equal,
            value: Cell::Text("x' OR 1=1--".into()),
        });
        let s = b.sql(&SQLITE).unwrap();
        assert!(s.contains("'x'' OR 1=1--'"));
    }
    #[test]
    fn contains_filter_escapes_wildcards_and_stays_one_editable_statement() {
        let table = TableRef {
            schema: String::new(),
            name: "a".into(),
            kind: "table".into(),
        };
        let filtered = |engine: &Dialect, value: &str| {
            let mut b = QueryBuilder::table(table.clone());
            b.filters.push(Filter {
                column: "name".into(),
                op: FilterOp::Contains,
                value: Cell::Text(value.into()),
            });
            b.sql(engine).unwrap()
        };
        assert_eq!(
            filtered(&SQLITE, "50%_a!"),
            "SELECT *\nFROM \"a\"\nWHERE \"name\" LIKE '%50!%!_a!!%' ESCAPE '!'\nLIMIT 500 OFFSET 0"
        );
        assert!(
            filtered(&POSTGRES, "a\\b")
                .contains("WHERE CAST(\"name\" AS TEXT) ILIKE E'%a\\\\b%' ESCAPE '!'")
        );
        // "%a_%" as bytes: MySQL never sees a quote or a backslash of the value.
        assert!(
            filtered(&MYSQL, "a_").contains("WHERE `name` LIKE _utf8mb4 0x2561215f25 ESCAPE '!'")
        );
        // Whatever is typed, the filtered query is still one statement on one
        // table, so its rows stay editable.
        for engine in [&POSTGRES, &MYSQL, &SQLITE] {
            for value in [
                "pend",
                "\\",
                "C:\\",
                "'",
                "\\'",
                "'; DROP TABLE a; --",
                "100%",
                "a_b",
                "!",
                "$$",
                "E'",
                "/* x",
                "-- x",
                "zażółć",
            ] {
                let sql = filtered(engine, value);
                assert!(!sql.contains("ESCAPE '\\'"), "{sql}");
                assert_eq!(statements(engine, &sql).unwrap().len(), 1, "{sql}");
                assert!(
                    editable_table(engine, &sql, std::slice::from_ref(&table))
                        .is_some_and(|found| found.name == table.name),
                    "{engine:?} {sql}"
                );
            }
        }
    }
    #[test]
    fn a_typed_condition_goes_in_parentheses_next_to_the_other_filters() {
        let table = TableRef {
            schema: String::new(),
            name: "a".into(),
            kind: "table".into(),
        };
        let mut b = QueryBuilder::table(table.clone());
        b.condition = Some("id = 19 or secret is null".into());
        assert_eq!(
            b.sql(&SQLITE).unwrap(),
            "SELECT *\nFROM \"a\"\nWHERE (id = 19 or secret is null)\nLIMIT 500 OFFSET 0"
        );
        b.filters.push(Filter {
            column: "name".into(),
            op: FilterOp::Equal,
            value: Cell::Text("x".into()),
        });
        let sql = b.sql(&SQLITE).unwrap();
        assert!(
            sql.contains("WHERE \"name\" = 'x' AND (id = 19 or secret is null)\n"),
            "{sql}"
        );
        assert!(editable_table(&SQLITE, &sql, std::slice::from_ref(&table)).is_some());
        // A refused condition is never written.
        b.condition = Some("1 = 1; DROP TABLE a".into());
        assert!(b.sql(&SQLITE).is_err());
    }
    #[test]
    fn a_page_is_ordered_by_several_columns_in_turn() {
        let mut b = QueryBuilder::table(TableRef {
            schema: String::new(),
            name: "a".into(),
            kind: "table".into(),
        });
        b.order_by = Some(("status".into(), false));
        b.then_by = vec![("created".into(), true), ("id".into(), false)];
        assert!(
            b.sql(&SQLITE)
                .unwrap()
                .contains("\nORDER BY \"status\" ASC, \"created\" DESC, \"id\" ASC\n")
        );
    }
    #[test]
    fn aggregates_and_pages() {
        let mut b = QueryBuilder::table(TableRef {
            schema: "main".into(),
            name: "a".into(),
            kind: "table".into(),
        });
        b.projections.push(Projection {
            column: "*".into(),
            aggregate: Some("count".into()),
            alias: Some("total".into()),
        });
        assert!(
            b.sql(&POSTGRES)
                .unwrap()
                .starts_with("SELECT COUNT(*) AS \"total\"")
        );
        b.limit = 0;
        assert!(b.sql(&SQLITE).is_err());
    }
}

#[cfg(test)]
mod dialect_tests {
    use super::*;
    use crate::engine::{ContainsFilter, Lexer, TextLiteral};
    fn page(dialect: &Dialect, order: bool, offset: u64) -> anyhow::Result<String> {
        let mut b = QueryBuilder::table(TableRef {
            schema: "dbo".into(),
            name: "Orders".into(),
            kind: "table".into(),
        });
        b.limit = 50;
        b.offset = offset;
        if order {
            b.order_by = Some(("id".into(), true));
        }
        b.sql(dialect)
    }
    /// The ways of paging the dialects of later engines need.
    #[test]
    fn a_page_is_written_the_way_the_dialect_says() {
        let brackets = Dialect {
            name_quotes: ('[', ']'),
            // The reader of SQL has to know the quotes the dialect writes.
            lexer: Lexer {
                bracket_names: true,
                ..Lexer::STANDARD
            },
            text: TextLiteral::National,
            paging: Paging::OffsetFetch {
                default_order: Some("1"),
            },
            ..Dialect::ANSI
        };
        // An engine that takes OFFSET only after ORDER BY gets one. It must
        // not be a subquery: a `SELECT` inside would no longer read as a
        // plain page of one table, and the rows would not be editable.
        assert_eq!(
            page(&brackets, false, 100).unwrap(),
            "SELECT *\nFROM [dbo].[Orders]\nORDER BY 1\nOFFSET 100 ROWS FETCH NEXT 50 ROWS ONLY"
        );
        assert_eq!(
            page(&brackets, true, 0).unwrap(),
            "SELECT *\nFROM [dbo].[Orders]\nORDER BY [id] DESC\nOFFSET 0 ROWS FETCH NEXT 50 ROWS ONLY"
        );
        let plain_fetch = Dialect {
            paging: Paging::OffsetFetch {
                default_order: None,
            },
            ..Dialect::ANSI
        };
        assert!(
            page(&plain_fetch, false, 0)
                .unwrap()
                .ends_with("FROM \"dbo\".\"Orders\"\nOFFSET 0 ROWS FETCH NEXT 50 ROWS ONLY")
        );
        let top = Dialect {
            paging: Paging::Top,
            ..brackets
        };
        assert_eq!(
            page(&top, false, 0).unwrap(),
            "SELECT TOP 50 *\nFROM [dbo].[Orders]"
        );
        assert_eq!(
            page(&top, false, 50).unwrap_err().to_string(),
            "page_offset_unsupported"
        );
        let unpaged = Dialect {
            paging: Paging::None,
            ..Dialect::ANSI
        };
        assert_eq!(
            page(&unpaged, true, 0).unwrap(),
            "SELECT *\nFROM \"dbo\".\"Orders\"\nORDER BY \"id\" DESC"
        );
        // Every first page is still one plain SELECT of one table: editable.
        let table = TableRef {
            schema: "dbo".into(),
            name: "Orders".into(),
            kind: "table".into(),
        };
        for dialect in [&brackets, &plain_fetch, &top, &unpaged] {
            for order in [false, true] {
                let sql = page(dialect, order, 0).unwrap();
                assert!(
                    editable_table(dialect, &sql, std::slice::from_ref(&table)).is_some(),
                    "{sql}"
                );
            }
        }
        // A bracket inside a bracketed name is doubled, and read back.
        let odd = TableRef {
            schema: "dbo".into(),
            name: "a]b; c".into(),
            kind: "table".into(),
        };
        let sql = QueryBuilder::table(odd.clone()).sql(&brackets).unwrap();
        assert!(sql.contains("FROM [dbo].[a]]b; c]"));
        assert_eq!(statements(&brackets, &sql).unwrap().len(), 1);
        assert_eq!(
            editable_table(&brackets, &sql, std::slice::from_ref(&odd)).map(|found| found.name),
            Some(odd.name)
        );
    }
    #[test]
    fn literals_and_filters_follow_the_dialect() {
        let national = Dialect {
            name_quotes: ('[', ']'),
            text: TextLiteral::National,
            bytes: ("0x", ""),
            booleans: ("1", "0"),
            ..Dialect::ANSI
        };
        assert_eq!(
            literal(&national, &Cell::Text("it's".into())).unwrap(),
            "N'it''s'"
        );
        assert_eq!(
            literal(&national, &Cell::Bytes(vec![0, 255])).unwrap(),
            "0x00ff"
        );
        assert_eq!(literal(&national, &Cell::Bool(true)).unwrap(), "1");
        assert_eq!(quote_identifier(&national, "a]b").unwrap(), "[a]]b]");
        let backslash = Dialect {
            text: TextLiteral::Backslash,
            contains: ContainsFilter {
                cast: Some(("CAST(", " AS VARCHAR(65535))")),
                operator: "ILIKE",
                pattern: None,
            },
            ..Dialect::ANSI
        };
        assert_eq!(
            literal(&backslash, &Cell::Text("C:\\dane 'x'".into())).unwrap(),
            "'C:\\\\dane ''x'''"
        );
        let mut b = QueryBuilder::table(TableRef {
            schema: String::new(),
            name: "t".into(),
            kind: "table".into(),
        });
        b.filters.push(Filter {
            column: "name".into(),
            op: FilterOp::Contains,
            value: Cell::Text("a%".into()),
        });
        assert!(
            b.sql(&backslash)
                .unwrap()
                .contains("WHERE CAST(\"name\" AS VARCHAR(65535)) ILIKE '%a!%%' ESCAPE '!'")
        );
        // An empty row takes every default, in the words of the dialect.
        let table = TableRef {
            schema: String::new(),
            name: "t".into(),
            kind: "table".into(),
        };
        assert_eq!(
            insert_row(&SQLITE, &table, &[]).unwrap(),
            "INSERT INTO \"t\" DEFAULT VALUES"
        );
        assert_eq!(
            insert_row(&MYSQL, &table, &[]).unwrap(),
            "INSERT INTO `t` () VALUES ()"
        );
        assert_eq!(
            insert_row(
                &POSTGRES,
                &table,
                &[
                    ("id".into(), Cell::Integer("7".into())),
                    ("note".into(), Cell::Null)
                ]
            )
            .unwrap(),
            "INSERT INTO \"t\" (\"id\", \"note\") VALUES (7, NULL)"
        );
    }
}

#[cfg(test)]
mod exact_identifier_tests {
    use super::*;
    #[test]
    fn dotted_column_is_one_identifier() {
        let mut b = QueryBuilder::table(TableRef {
            schema: "main".into(),
            name: "Orders".into(),
            kind: "table".into(),
        });
        b.projections.push(Projection {
            column: "Order.Id".into(),
            aggregate: None,
            alias: None,
        });
        b.filters.push(Filter {
            column: "Order.Id".into(),
            op: FilterOp::Equal,
            value: Cell::Integer("2".into()),
        });
        b.group_by.push("Order.Id".into());
        b.order_by = Some(("Order.Id".into(), false));
        let s = b.sql(&SQLITE).unwrap();
        assert!(s.starts_with("SELECT \"Order.Id\"\n"));
        assert!(s.contains("WHERE \"Order.Id\" = 2"));
        assert!(s.contains("GROUP BY \"Order.Id\""));
        assert!(!s.contains("\"Order\".\"Id\""));
    }
    #[test]
    fn joins_use_explicit_table_and_column_identity() {
        let t = TableRef {
            schema: "public".into(),
            name: "Order.Items".into(),
            kind: "table".into(),
        };
        let other = TableRef {
            schema: "public".into(),
            name: "Customers".into(),
            kind: "table".into(),
        };
        let mut b = QueryBuilder::table(t.clone());
        b.joins.push(Join {
            table: other.clone(),
            left: ColumnRef {
                table: t,
                name: "Customer.Id".into(),
            },
            right: ColumnRef {
                table: other,
                name: "Id".into(),
            },
            left_join: true,
        });
        assert!(b.sql(&POSTGRES).unwrap().contains(
            "ON \"public\".\"Order.Items\".\"Customer.Id\" = \"public\".\"Customers\".\"Id\""
        ));
    }
}
