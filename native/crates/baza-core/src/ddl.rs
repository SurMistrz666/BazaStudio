//! The statements of the table designer: `CREATE TABLE` for a new table and
//! the `ALTER TABLE` statements that turn the columns a table has into the
//! ones the designer shows. Each SQL family writes them its own way. The
//! parts of a MySQL or MariaDB table beyond its columns are in [`mysql`].
pub mod mysql;

use crate::engine::Dialect;
use crate::model::{Cell, ColumnInfo, TableRef};
use crate::sql::{literal, qualified, quote_identifier};
use anyhow::{Result, ensure};

/// How a database family writes a column and changes a table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DdlFlavor {
    MySql,
    Postgres,
    Sqlite,
    SqlServer,
    /// Standard SQL: create only, changes column by column where possible.
    Generic,
}

/// One column as the designer edits it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FieldDraft {
    pub name: String,
    /// The type without its length, as `varchar` or `decimal`.
    pub data_type: String,
    pub length: String,
    pub scale: String,
    pub not_null: bool,
    pub primary_key: bool,
    /// An SQL expression, written as typed: `0`, `'none'`, `CURRENT_TIMESTAMP`.
    pub default: String,
    pub comment: String,
    pub auto_increment: bool,
    pub unsigned: bool,
    /// The values of an `enum` or a `set`, in their order.
    pub values: Vec<String>,
    /// The name the column has in the database; `None` for a new column.
    pub original: Option<String>,
}

impl FieldDraft {
    /// A column the database already has, as its metadata describes it.
    pub fn from_column(column: &ColumnInfo) -> Self {
        let (data_type, length, scale, unsigned) = split_type(&column.data_type);
        // The values of an `enum` or a `set` are not its length.
        let values = crate::sql::choices(&column.data_type)
            .map(|(_, values)| values)
            .unwrap_or_default();
        let (data_type, length, scale) = if values.is_empty() {
            (data_type, length, scale)
        } else {
            (data_type.to_ascii_lowercase(), String::new(), String::new())
        };
        Self {
            name: column.name.clone(),
            data_type,
            length,
            scale,
            not_null: !column.nullable,
            primary_key: column.primary_key,
            default: column.default.clone().unwrap_or_default(),
            comment: String::new(),
            auto_increment: false,
            unsigned,
            values,
            original: Some(column.name.clone()),
        }
    }
}

/// `varchar(64)` as `("varchar", "64", "", false)`, `decimal(10,2) unsigned`
/// as `("decimal", "10", "2", true)`.
pub fn split_type(text: &str) -> (String, String, String, bool) {
    let lower = text.trim();
    let unsigned = lower.to_ascii_lowercase().contains(" unsigned");
    let base = lower
        .split_whitespace()
        .filter(|word| {
            !word.eq_ignore_ascii_case("unsigned") && !word.eq_ignore_ascii_case("zerofill")
        })
        .collect::<Vec<_>>()
        .join(" ");
    match (base.find('('), base.rfind(')')) {
        (Some(open), Some(close)) if close > open => {
            let inner = &base[open + 1..close];
            let mut parts = inner.splitn(2, ',');
            let length = parts.next().unwrap_or("").trim().to_owned();
            let scale = parts.next().unwrap_or("").trim().to_owned();
            let name = format!("{}{}", base[..open].trim(), &base[close + 1..]);
            (name.trim().to_owned(), length, scale, unsigned)
        }
        _ => (base, String::new(), String::new(), unsigned),
    }
}

/// The type of a column as SQL: its name, the length and scale in brackets,
/// `UNSIGNED` where the family has it. Only letters, digits, spaces and
/// underscores make a type name, digits (or `MAX`) a length.
pub fn type_text(flavor: DdlFlavor, field: &FieldDraft) -> Result<String> {
    let name = field.data_type.trim();
    // `enum('a','b')` and `set(…)`: the values quoted as MySQL reads them,
    // where a backslash escapes.
    if flavor == DdlFlavor::MySql
        && (name.eq_ignore_ascii_case("enum") || name.eq_ignore_ascii_case("set"))
    {
        ensure!(
            !field.values.is_empty()
                && field.values.len() <= 65535
                && field.values.iter().all(|value| !value.contains('\0')),
            "column_values_invalid"
        );
        let values = field
            .values
            .iter()
            .map(|value| format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''")))
            .collect::<Vec<_>>();
        return Ok(format!(
            "{}({})",
            name.to_ascii_lowercase(),
            values.join(",")
        ));
    }
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '_'),
        "column_type_invalid"
    );
    let number = |text: &str| {
        text.chars().all(|c| c.is_ascii_digit()) && text.len() <= 6
            || text.eq_ignore_ascii_case("max")
    };
    let length = field.length.trim();
    let scale = field.scale.trim();
    ensure!(number(length) && number(scale), "column_length_invalid");
    ensure!(
        scale.is_empty() || !length.is_empty(),
        "column_length_invalid"
    );
    let mut text = name.to_owned();
    if !length.is_empty() {
        text.push('(');
        text.push_str(length);
        if !scale.is_empty() {
            text.push(',');
            text.push_str(scale);
        }
        text.push(')');
    }
    if field.unsigned && flavor == DdlFlavor::MySql {
        text.push_str(" UNSIGNED");
    }
    Ok(text)
}

/// A default value as typed: one expression, no statement after it. It is
/// read as the engine reads SQL, so that a quote it leaves open cannot take
/// in what the statement says after it (the comment of the column).
fn default_text(dialect: &Dialect, field: &FieldDraft) -> Result<Option<String>> {
    let text = field.default.trim();
    if text.is_empty() {
        return Ok(None);
    }
    ensure!(text.len() <= 1000, "column_default_invalid");
    crate::sql::sql_fragment(dialect, text)
        .map_err(|_| anyhow::anyhow!("column_default_invalid"))?;
    Ok(Some(text.to_owned()))
}

/// The definition of a column after its name.
fn definition(
    flavor: DdlFlavor,
    dialect: &Dialect,
    field: &FieldDraft,
    inline_key: bool,
) -> Result<String> {
    let mut text = type_text(flavor, field)?;
    if inline_key {
        // SQLite numbers rows itself only for `INTEGER PRIMARY KEY`.
        text.push_str(" PRIMARY KEY AUTOINCREMENT");
        return Ok(text);
    }
    if field.auto_increment {
        match flavor {
            DdlFlavor::Postgres => text.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
            DdlFlavor::SqlServer => text.push_str(" IDENTITY(1,1)"),
            _ => {}
        }
    }
    if field.not_null || field.primary_key {
        text.push_str(" NOT NULL");
    } else if flavor != DdlFlavor::Postgres && flavor != DdlFlavor::Sqlite {
        text.push_str(" NULL");
    }
    if let Some(default) = default_text(dialect, field)? {
        text.push_str(" DEFAULT ");
        text.push_str(&default);
    }
    if flavor == DdlFlavor::MySql {
        if field.auto_increment {
            text.push_str(" AUTO_INCREMENT");
        }
        if !field.comment.trim().is_empty() {
            text.push_str(" COMMENT ");
            text.push_str(&mysql_comment(field.comment.trim())?);
        }
    }
    Ok(text)
}

/// A comment of MySQL and MariaDB. Their grammar takes only a quoted string
/// there, not the `_utf8mb4 0x…` literal the dialect writes elsewhere; a
/// backslash escapes in it, as the servers read strings by default.
pub(crate) fn mysql_comment(text: &str) -> Result<String> {
    ensure!(
        text.len() <= 2048 && !text.contains('\0'),
        "comment_invalid"
    );
    Ok(format!(
        "'{}'",
        text.replace('\\', "\\\\").replace('\'', "''")
    ))
}

fn check_names(fields: &[FieldDraft]) -> Result<()> {
    ensure!(
        !fields.is_empty() && fields.len() <= 1000,
        "table_columns_invalid"
    );
    let mut seen = std::collections::HashSet::new();
    for field in fields {
        ensure!(!field.name.trim().is_empty(), "table_columns_invalid");
        ensure!(
            seen.insert(field.name.trim().to_lowercase()),
            "column_name_duplicate"
        );
    }
    Ok(())
}

/// `COMMENT ON COLUMN` of PostgreSQL for each column with a comment.
fn postgres_comments(
    dialect: &Dialect,
    table: &TableRef,
    fields: &[&FieldDraft],
) -> Result<Vec<String>> {
    fields
        .iter()
        .map(|field| {
            Ok(format!(
                "COMMENT ON COLUMN {}.{} IS {}",
                qualified(dialect, table)?,
                quote_identifier(dialect, field.name.trim())?,
                if field.comment.trim().is_empty() {
                    "NULL".to_owned()
                } else {
                    literal(dialect, &Cell::Text(field.comment.trim().to_owned()))?
                }
            ))
        })
        .collect()
}

/// The statements that create a new table with these columns.
pub fn create_table(
    flavor: DdlFlavor,
    dialect: &Dialect,
    table: &TableRef,
    fields: &[FieldDraft],
) -> Result<Vec<String>> {
    create_table_with(flavor, dialect, table, fields, &[], "")
}

/// [`create_table`] with more lines in the body after the columns and the
/// key (indexes, constraints), and text after the body (table options).
pub fn create_table_with(
    flavor: DdlFlavor,
    dialect: &Dialect,
    table: &TableRef,
    fields: &[FieldDraft],
    more: &[String],
    after: &str,
) -> Result<Vec<String>> {
    ensure!(!table.name.trim().is_empty(), "table_columns_invalid");
    check_names(fields)?;
    let keys: Vec<&FieldDraft> = fields.iter().filter(|f| f.primary_key).collect();
    let sqlite_identity = flavor == DdlFlavor::Sqlite
        && keys.len() == 1
        && keys[0].auto_increment
        && keys[0].data_type.trim().eq_ignore_ascii_case("integer");
    let mut lines = Vec::new();
    for field in fields {
        let inline = sqlite_identity && field.primary_key;
        lines.push(format!(
            "  {} {}",
            quote_identifier(dialect, field.name.trim())?,
            definition(flavor, dialect, field, inline)?
        ));
    }
    if !keys.is_empty() && !sqlite_identity {
        lines.push(format!(
            "  PRIMARY KEY ({})",
            keys.iter()
                .map(|f| quote_identifier(dialect, f.name.trim()))
                .collect::<Result<Vec<_>>>()?
                .join(", ")
        ));
    }
    lines.extend(more.iter().map(|line| format!("  {line}")));
    let mut statements = vec![format!(
        "CREATE TABLE {} (\n{}\n){after}",
        qualified(dialect, table)?,
        lines.join(",\n")
    )];
    if flavor == DdlFlavor::Postgres {
        let commented: Vec<&FieldDraft> = fields
            .iter()
            .filter(|f| !f.comment.trim().is_empty())
            .collect();
        statements.extend(postgres_comments(dialect, table, &commented)?);
    }
    Ok(statements)
}

/// The statements that turn the columns `before` into `after`. Columns are
/// matched by [`FieldDraft::original`]. What a family cannot change in place
/// is refused with a code, never done halfway.
pub fn alter_table(
    flavor: DdlFlavor,
    dialect: &Dialect,
    table: &TableRef,
    before: &[FieldDraft],
    after: &[FieldDraft],
) -> Result<Vec<String>> {
    check_names(after)?;
    let target = qualified(dialect, table)?;
    let q = |name: &str| quote_identifier(dialect, name.trim());
    let mut statements = Vec::new();
    // Removed columns first: a new column may take a removed one's name.
    for old in before {
        if !after
            .iter()
            .any(|f| f.original.as_deref() == Some(old.name.as_str()))
        {
            statements.push(format!(
                "ALTER TABLE {target} DROP COLUMN {}",
                q(&old.name)?
            ));
        }
    }
    let keys_before: Vec<&str> = before
        .iter()
        .filter(|f| f.primary_key)
        .map(|f| f.name.as_str())
        .collect();
    let keys_after: Vec<String> = after
        .iter()
        .filter(|f| f.primary_key)
        .map(|f| f.original.clone().unwrap_or_else(|| f.name.clone()))
        .collect();
    let keys_changed = keys_before.len() != keys_after.len()
        || keys_before
            .iter()
            .zip(&keys_after)
            .any(|(a, b)| *a != b.as_str());
    let mut comments = Vec::new();
    for (index, field) in after.iter().enumerate() {
        // A column keeps its place: MySQL puts a new one after the one above it.
        let place = match (flavor, index) {
            (DdlFlavor::MySql, 0) => " FIRST".to_owned(),
            (DdlFlavor::MySql, _) => format!(" AFTER {}", q(&after[index - 1].name)?),
            _ => String::new(),
        };
        let Some(original) = &field.original else {
            let mut added = field.clone();
            // A key column joins the key below, not in its definition.
            added.primary_key = false;
            added.not_null = field.not_null || field.primary_key;
            statements.push(format!(
                "ALTER TABLE {target} ADD {}{} {}{place}",
                if flavor == DdlFlavor::SqlServer {
                    ""
                } else {
                    "COLUMN "
                },
                q(&field.name)?,
                definition(flavor, dialect, &added, false)?
            ));
            if flavor == DdlFlavor::Postgres && !field.comment.trim().is_empty() {
                comments.push(field);
            }
            continue;
        };
        let Some(old) = before.iter().find(|f| &f.name == original) else {
            continue;
        };
        let renamed = field.name.trim() != old.name;
        let mut same_key = field.clone();
        same_key.primary_key = old.primary_key;
        same_key.name = old.name.clone();
        same_key.original = old.original.clone();
        let changed = same_key != *old;
        if !renamed && !changed {
            continue;
        }
        match flavor {
            DdlFlavor::MySql => {
                let mut kept = field.clone();
                kept.primary_key = false;
                kept.not_null = field.not_null || field.primary_key;
                statements.push(format!(
                    "ALTER TABLE {target} CHANGE COLUMN {} {} {}",
                    q(&old.name)?,
                    q(&field.name)?,
                    definition(flavor, dialect, &kept, false)?
                ));
            }
            DdlFlavor::Postgres => {
                if renamed {
                    statements.push(format!(
                        "ALTER TABLE {target} RENAME COLUMN {} TO {}",
                        q(&old.name)?,
                        q(&field.name)?
                    ));
                }
                let column = q(&field.name)?;
                if type_text(flavor, field)? != type_text(flavor, old)? {
                    statements.push(format!(
                        "ALTER TABLE {target} ALTER COLUMN {column} TYPE {}",
                        type_text(flavor, field)?
                    ));
                }
                if field.not_null != old.not_null {
                    statements.push(format!(
                        "ALTER TABLE {target} ALTER COLUMN {column} {} NOT NULL",
                        if field.not_null { "SET" } else { "DROP" }
                    ));
                }
                if field.default.trim() != old.default.trim() {
                    statements.push(match default_text(dialect, field)? {
                        Some(value) => format!(
                            "ALTER TABLE {target} ALTER COLUMN {column} SET DEFAULT {value}"
                        ),
                        None => format!("ALTER TABLE {target} ALTER COLUMN {column} DROP DEFAULT"),
                    });
                }
                ensure!(
                    field.auto_increment == old.auto_increment,
                    "column_change_unsupported"
                );
                if field.comment.trim() != old.comment.trim() {
                    comments.push(field);
                }
            }
            DdlFlavor::Sqlite => {
                ensure!(!changed, "column_change_unsupported");
                statements.push(format!(
                    "ALTER TABLE {target} RENAME COLUMN {} TO {}",
                    q(&old.name)?,
                    q(&field.name)?
                ));
            }
            DdlFlavor::SqlServer => {
                if renamed {
                    let path = format!("{}.{}", table.schema, table.name);
                    statements.push(format!(
                        "EXEC sp_rename {}, {}, 'COLUMN'",
                        literal(dialect, &Cell::Text(format!("{path}.{}", old.name)))?,
                        literal(dialect, &Cell::Text(field.name.trim().to_owned()))?
                    ));
                }
                if changed {
                    ensure!(
                        field.default.trim() == old.default.trim()
                            && field.auto_increment == old.auto_increment,
                        "column_change_unsupported"
                    );
                    let mut plain = field.clone();
                    plain.default.clear();
                    plain.primary_key = false;
                    statements.push(format!(
                        "ALTER TABLE {target} ALTER COLUMN {} {}",
                        q(&field.name)?,
                        definition(flavor, dialect, &plain, false)?
                    ));
                }
            }
            DdlFlavor::Generic => anyhow::bail!("column_change_unsupported"),
        }
    }
    if keys_changed {
        let names = after
            .iter()
            .filter(|f| f.primary_key)
            .map(|f| q(&f.name))
            .collect::<Result<Vec<_>>>()?
            .join(", ");
        match flavor {
            DdlFlavor::MySql => {
                let mut parts = Vec::new();
                if !keys_before.is_empty() {
                    parts.push("DROP PRIMARY KEY".to_owned());
                }
                if !names.is_empty() {
                    parts.push(format!("ADD PRIMARY KEY ({names})"));
                }
                statements.push(format!("ALTER TABLE {target} {}", parts.join(", ")));
            }
            // Elsewhere the key is a named constraint: a table without one
            // can get one, an existing one is changed in SQL by hand.
            DdlFlavor::Postgres | DdlFlavor::SqlServer
                if keys_before.is_empty() && !names.is_empty() =>
            {
                statements.push(format!("ALTER TABLE {target} ADD PRIMARY KEY ({names})"));
            }
            _ => anyhow::bail!("primary_key_change_unsupported"),
        }
    }
    if flavor == DdlFlavor::Postgres {
        statements.extend(postgres_comments(dialect, table, &comments)?);
    }
    Ok(statements)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, data_type: &str, length: &str) -> FieldDraft {
        FieldDraft {
            name: name.into(),
            data_type: data_type.into(),
            length: length.into(),
            original: None,
            ..Default::default()
        }
    }

    fn table() -> TableRef {
        TableRef {
            schema: "shop".into(),
            name: "orders".into(),
            kind: "table".into(),
        }
    }

    const MYSQL: Dialect = Dialect {
        name_quotes: ('`', '`'),
        ..Dialect::ANSI
    };

    #[test]
    fn types_are_split_and_written_back() {
        assert_eq!(
            split_type("varchar(64)"),
            ("varchar".into(), "64".into(), String::new(), false)
        );
        assert_eq!(
            split_type("decimal(10,2) unsigned"),
            ("decimal".into(), "10".into(), "2".into(), true)
        );
        assert_eq!(
            split_type("character varying"),
            (
                "character varying".into(),
                String::new(),
                String::new(),
                false
            )
        );
        let mut price = field("price", "decimal", "10");
        price.scale = "2".into();
        price.unsigned = true;
        assert_eq!(
            type_text(DdlFlavor::MySql, &price).unwrap(),
            "decimal(10,2) UNSIGNED"
        );
        assert_eq!(
            type_text(DdlFlavor::Postgres, &price).unwrap(),
            "decimal(10,2)"
        );
        assert!(type_text(DdlFlavor::MySql, &field("x", "int); DROP TABLE t", "")).is_err());
        assert!(type_text(DdlFlavor::MySql, &field("x", "varchar", "1x")).is_err());
        let mut bad = field("x", "int", "");
        bad.default = "1; DROP TABLE t".into();
        assert!(definition(DdlFlavor::MySql, &MYSQL, &bad, false).is_err());
        // An ENUM keeps its values, not a length and a scale.
        let status = FieldDraft::from_column(&ColumnInfo {
            name: "status".into(),
            data_type: r"enum('new','it''s','a\\b')".into(),
            nullable: false,
            primary_key: false,
            default: None,
        });
        assert_eq!(
            (
                status.data_type.as_str(),
                status.length.as_str(),
                status.scale.as_str()
            ),
            ("enum", "", "")
        );
        assert_eq!(status.values, ["new", "it's", r"a\b"]);
        assert_eq!(
            type_text(DdlFlavor::MySql, &status).unwrap(),
            r"enum('new','it''s','a\\b')"
        );
        // A comment is a plain quoted string, the only form the grammar takes.
        let mut noted = field("x", "int", "");
        noted.comment = r"it's a\b".into();
        assert!(
            definition(DdlFlavor::MySql, &MYSQL, &noted, false)
                .unwrap()
                .ends_with(r" COMMENT 'it''s a\\b'")
        );
    }

    #[test]
    fn a_default_cannot_leave_a_quote_open_for_the_comment() {
        use crate::drivers::mysql::DIALECT as SERVER;
        let mut note = field("note", "varchar", "20");
        // The comment's own quote would close the default's, and the rest
        // of the comment would be a second statement.
        note.default = "0 COMMENT 'x".into();
        note.comment = "'; DROP TABLE users; SELECT '".into();
        let refused = |field: &FieldDraft| {
            create_table(
                DdlFlavor::MySql,
                &SERVER,
                &table(),
                std::slice::from_ref(field),
            )
            .unwrap_err()
            .to_string()
        };
        assert_eq!(refused(&note), "column_default_invalid");
        // Whole only where a backslash escapes, or only where it does not.
        for default in [r"'a\'", r"'it\'s'"] {
            note.default = default.into();
            assert_eq!(refused(&note), "column_default_invalid", "{default}");
        }
        note.default = r"'a\\b', 'c'".replace(", 'c'", "");
        note.comment = "it's".into();
        assert!(create_table(DdlFlavor::MySql, &SERVER, &table(), &[note]).is_ok());
    }

    #[test]
    fn a_new_table_is_created_with_its_key_and_comments() {
        let mut id = field("id", "bigint", "20");
        id.primary_key = true;
        id.auto_increment = true;
        let mut label = field("label", "varchar", "40");
        label.comment = "name given by the player".into();
        label.default = "'none'".into();
        let sql = create_table(
            DdlFlavor::MySql,
            &MYSQL,
            &table(),
            &[id.clone(), label.clone()],
        )
        .unwrap();
        assert_eq!(
            sql,
            [
                "CREATE TABLE `shop`.`orders` (\n  `id` bigint(20) NOT NULL AUTO_INCREMENT,\n  `label` varchar(40) NULL DEFAULT 'none' COMMENT 'name given by the player',\n  PRIMARY KEY (`id`)\n)"
            ]
        );
        let pg = create_table(
            DdlFlavor::Postgres,
            &Dialect::ANSI,
            &table(),
            &[id.clone(), label],
        )
        .unwrap();
        assert!(
            pg[0].contains("\"id\" bigint(20) GENERATED BY DEFAULT AS IDENTITY NOT NULL"),
            "{}",
            pg[0]
        );
        assert_eq!(
            pg[1],
            "COMMENT ON COLUMN \"shop\".\"orders\".\"label\" IS 'name given by the player'"
        );
        let mut rowid = field("id", "INTEGER", "");
        rowid.primary_key = true;
        rowid.auto_increment = true;
        let lite = create_table(DdlFlavor::Sqlite, &Dialect::ANSI, &table(), &[rowid]).unwrap();
        assert_eq!(
            lite,
            ["CREATE TABLE \"shop\".\"orders\" (\n  \"id\" INTEGER PRIMARY KEY AUTOINCREMENT\n)"]
        );
        assert!(
            create_table(
                DdlFlavor::MySql,
                &MYSQL,
                &table(),
                &[field("a", "int", ""), field("A", "int", "")]
            )
            .is_err()
        );
    }

    #[test]
    fn changes_become_the_alter_statements_of_each_family() {
        let mut id = field("id", "int", "");
        id.primary_key = true;
        id.not_null = true;
        id.original = Some("id".into());
        let mut name = field("name", "varchar", "40");
        name.original = Some("name".into());
        let mut old_note = field("note", "text", "");
        old_note.original = Some("note".into());
        let before = vec![id.clone(), name.clone(), old_note];

        // Rename and widen `name`, drop `note`, add `email` at the end.
        let mut title = name.clone();
        title.name = "title".into();
        title.length = "80".into();
        let email = field("email", "varchar", "120");
        let after = vec![id.clone(), title.clone(), email.clone()];
        assert_eq!(
            alter_table(DdlFlavor::MySql, &MYSQL, &table(), &before, &after).unwrap(),
            [
                "ALTER TABLE `shop`.`orders` DROP COLUMN `note`",
                "ALTER TABLE `shop`.`orders` CHANGE COLUMN `name` `title` varchar(80) NULL",
                "ALTER TABLE `shop`.`orders` ADD COLUMN `email` varchar(120) NULL AFTER `title`",
            ]
        );
        assert_eq!(
            alter_table(
                DdlFlavor::Postgres,
                &Dialect::ANSI,
                &table(),
                &before,
                &after
            )
            .unwrap(),
            [
                "ALTER TABLE \"shop\".\"orders\" DROP COLUMN \"note\"",
                "ALTER TABLE \"shop\".\"orders\" RENAME COLUMN \"name\" TO \"title\"",
                "ALTER TABLE \"shop\".\"orders\" ALTER COLUMN \"title\" TYPE varchar(80)",
                "ALTER TABLE \"shop\".\"orders\" ADD COLUMN \"email\" varchar(120)",
            ]
        );
        // SQLite renames in place but cannot change a type.
        assert!(alter_table(DdlFlavor::Sqlite, &Dialect::ANSI, &table(), &before, &after).is_err());
        let mut renamed = name.clone();
        renamed.name = "title".into();
        assert_eq!(
            alter_table(
                DdlFlavor::Sqlite,
                &Dialect::ANSI,
                &table(),
                &before[..2],
                &[id.clone(), renamed]
            )
            .unwrap(),
            ["ALTER TABLE \"shop\".\"orders\" RENAME COLUMN \"name\" TO \"title\""]
        );
        // Nothing changed: nothing to run.
        assert!(
            alter_table(DdlFlavor::MySql, &MYSQL, &table(), &before, &before)
                .unwrap()
                .is_empty()
        );
        // A key moved to another column: MySQL drops and adds it.
        let mut keyed_name = name.clone();
        keyed_name.primary_key = true;
        let mut plain_id = id.clone();
        plain_id.primary_key = false;
        let moved = alter_table(
            DdlFlavor::MySql,
            &MYSQL,
            &table(),
            &before[..2],
            &[plain_id.clone(), keyed_name.clone()],
        )
        .unwrap();
        assert_eq!(
            moved.last().unwrap(),
            "ALTER TABLE `shop`.`orders` DROP PRIMARY KEY, ADD PRIMARY KEY (`name`)"
        );
        assert!(
            alter_table(
                DdlFlavor::Postgres,
                &Dialect::ANSI,
                &table(),
                &before[..2],
                &[plain_id, keyed_name]
            )
            .is_err()
        );
    }
}
