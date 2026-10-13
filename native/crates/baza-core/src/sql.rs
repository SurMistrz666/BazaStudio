use crate::drivers::RowChange;
#[cfg(test)]
use crate::drivers::{
    mysql::DIALECT as MYSQL, postgres::DIALECT as POSTGRES, sqlite::DIALECT as SQLITE,
};
use crate::engine::{Dialect, EmptyInsert, Engine, Paging, QueryLanguage, TextLiteral};
use crate::model::{Cell, ColumnInfo, QueryResult, TableRef};
pub const MAX_SQL_BYTES: usize = 5 * 1024 * 1024;
pub const MAX_RESULT_ROWS: usize = 20_000;
pub const MAX_RESULT_BYTES: usize = 64 * 1024 * 1024;
/// Escape character of the `LIKE` patterns this crate generates. It is not a
/// backslash on purpose: MySQL and MariaDB read a backslash inside a string
/// literal as an escape of their own, so `ESCAPE '\'` never ends there.
pub const LIKE_ESCAPE: char = '!';

/// Text for a `LIKE` pattern that matches `text` literally, to be used with
/// `ESCAPE '!'` ([`LIKE_ESCAPE`]).
pub fn like_pattern(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 4);
    for c in text.chars() {
        if matches!(c, '%' | '_') || c == LIKE_ESCAPE {
            out.push(LIKE_ESCAPE);
        }
        out.push(c);
    }
    out
}

/// One lexical element of SQL text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lexeme {
    Space,
    Comment,
    /// A string literal, including a PostgreSQL dollar-quoted body.
    Literal,
    /// A quoted identifier.
    Quoted,
    /// Letters, digits and `_`.
    Word,
    /// Any other single character.
    Symbol,
}

/// End of the quoted text that opens at `start`: the index after its closing
/// quote. A doubled quote stays inside; so does any character after a
/// backslash when the engine reads backslashes as escapes.
fn quoted_end(b: &[u8], start: usize, quote: u8, backslash: bool) -> anyhow::Result<usize> {
    let mut i = start + 1;
    loop {
        match b.get(i) {
            None => anyhow::bail!("sql_unterminated_literal"),
            Some(c) if *c == quote => {
                if b.get(i + 1) == Some(&quote) {
                    i += 2;
                } else {
                    return Ok(i + 1);
                }
            }
            Some(b'\\') if backslash => i += 2,
            Some(_) => i += 1,
        }
    }
}

/// Index of the `$` that closes a PostgreSQL dollar-quote tag opening at `i`.
/// `$1` is a parameter, not a tag.
fn dollar_tag(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if b.get(j).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    while b
        .get(j)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c >= 0x80)
    {
        j += 1;
    }
    (b.get(j) == Some(&b'$')).then_some(j)
}

/// Read SQL the way the engine of `dialect` does, as far as quoting and
/// comments go, and hand every element to `each`. The engines disagree exactly
/// where it matters for finding the end of a literal, and [`Lexer`] says how:
///
/// * MySQL and MariaDB read a backslash inside `'…'` and `"…"` as an escape,
///   and `"…"` is a string. `#` starts a comment; `--` only before white space.
/// * PostgreSQL reads a backslash literally unless the string is `E'…'`, has
///   `$tag$…$tag$` bodies and nests `/* … */` comments.
/// * SQLite reads a backslash literally and also quotes identifiers with
///   `` `…` `` and `[…]`.
///
/// Fails with `sql_unterminated_literal` when a quote, a comment or a dollar
/// body never ends.
///
/// [`Lexer`]: crate::engine::Lexer
fn lex(
    dialect: &Dialect,
    sql: &str,
    mut each: impl FnMut(Lexeme, std::ops::Range<usize>),
) -> anyhow::Result<()> {
    let b = sql.as_bytes();
    // Bytes of non-ASCII characters count as letters, so every boundary below
    // falls on an ASCII byte and is a valid place to slice the text.
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    let rules = dialect.lexer;
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let dashes = b[i..].starts_with(b"--")
            && (!rules.dashes_need_space
                || b.get(i + 2)
                    .is_none_or(|c| c.is_ascii_whitespace() || c.is_ascii_control()));
        let kind = if c.is_ascii_whitespace() {
            while b.get(i).is_some_and(u8::is_ascii_whitespace) {
                i += 1;
            }
            Lexeme::Space
        } else if dashes || (rules.hash_comments && c == b'#') {
            while b.get(i).is_some_and(|c| *c != b'\n') {
                i += 1;
            }
            Lexeme::Comment
        } else if b[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1usize;
            while depth > 0 {
                if i >= b.len() {
                    anyhow::bail!("sql_unterminated_literal");
                }
                if rules.nested_comments && b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Lexeme::Comment
        } else if c == b'\'' {
            i = quoted_end(b, i, b'\'', rules.backslash_escapes)?;
            Lexeme::Literal
        } else if c == b'"' {
            i = quoted_end(
                b,
                i,
                b'"',
                rules.backslash_escapes && rules.double_quote_strings,
            )?;
            if rules.double_quote_strings {
                Lexeme::Literal
            } else {
                Lexeme::Quoted
            }
        } else if c == b'`' && rules.backtick_names {
            i = quoted_end(b, i, b'`', false)?;
            Lexeme::Quoted
        } else if c == b'[' && rules.bracket_names {
            // Where brackets are the quotes of the dialect itself, `]]` is a
            // bracket inside the name; elsewhere the first `]` ends it.
            let doubled = dialect.name_quotes.1 == ']';
            loop {
                let Some(length) = b[i..].iter().position(|c| *c == b']') else {
                    anyhow::bail!("sql_unterminated_literal");
                };
                i += length + 1;
                if doubled && b.get(i) == Some(&b']') {
                    i += 1;
                } else {
                    break;
                }
            }
            Lexeme::Quoted
        } else if let Some(tag_end) = (rules.dollar_quotes && c == b'$')
            .then(|| dollar_tag(b, i))
            .flatten()
        {
            let tag = &b[i..=tag_end];
            let body = tag_end + 1;
            let Some(length) = b[body..]
                .windows(tag.len())
                .position(|window| window == tag)
            else {
                anyhow::bail!("sql_unterminated_literal");
            };
            i = body + length + tag.len();
            Lexeme::Literal
        } else if word(c) {
            // `$` may continue a PostgreSQL identifier; it never opens a
            // dollar quote there.
            while b
                .get(i)
                .is_some_and(|c| word(*c) || (rules.dollar_quotes && *c == b'$'))
            {
                i += 1;
            }
            if rules.escape_strings
                && i - start == 1
                && c.eq_ignore_ascii_case(&b'e')
                && b.get(i) == Some(&b'\'')
            {
                i = quoted_end(b, i, b'\'', true)?;
                Lexeme::Literal
            } else {
                Lexeme::Word
            }
        } else {
            i += 1;
            Lexeme::Symbol
        };
        each(kind, start..i);
    }
    Ok(())
}

/// Content of a quoted identifier: without its quotes, doubled quotes single.
fn unquote(dialect: &Dialect, text: &str) -> String {
    let mut chars = text.chars();
    let (Some(open), Some(_)) = (chars.next(), chars.next_back()) else {
        return String::new();
    };
    let inner = chars.as_str();
    if open == '[' {
        return if dialect.name_quotes.1 == ']' {
            inner.replace("]]", "]")
        } else {
            inner.into()
        };
    }
    inner.replace(&format!("{open}{open}"), &open.to_string())
}

/// Editing maps only a single SELECT * table whose identity comes from live metadata.
pub fn editable_table(dialect: &Dialect, sql: &str, tables: &[TableRef]) -> Option<TableRef> {
    if sql.len() > MAX_SQL_BYTES || sql.contains('\0') {
        return None;
    }
    let mut tokens = Vec::<String>::new();
    // Identifier quoting is semantic: an alias named "where" is never a
    // WHERE clause. Keep its kind through provenance inference.
    let mut quoted = Vec::<bool>::new();
    // Statements are counted like `statements` does: text between `;`.
    let (mut count, mut content) = (0usize, false);
    lex(dialect, sql, |kind, range| {
        let text = &sql[range];
        if kind == Lexeme::Space {
            return;
        }
        if kind == Lexeme::Symbol && text == ";" {
            count += usize::from(content);
            content = false;
        } else {
            content = true;
        }
        match kind {
            Lexeme::Space | Lexeme::Comment => {}
            Lexeme::Literal => {
                tokens.push("<literal>".into());
                quoted.push(false);
            }
            Lexeme::Quoted => {
                tokens.push(unquote(dialect, text));
                quoted.push(true);
            }
            Lexeme::Word | Lexeme::Symbol => {
                tokens.push(text.into());
                quoted.push(false);
            }
        }
    })
    .ok()?;
    if count + usize::from(content) != 1 {
        return None;
    }
    // `SELECT TOP 500 * FROM t` is the first page where paging is written so.
    if dialect.paging == Paging::Top
        && tokens.len() > 3
        && !quoted[1]
        && tokens[1].eq_ignore_ascii_case("TOP")
        && tokens[2].bytes().all(|b| b.is_ascii_digit())
    {
        tokens.drain(1..3);
        quoted.drain(1..3);
    }
    if tokens.len() < 4
        || quoted[..3].iter().any(|v| *v)
        || !tokens[0].eq_ignore_ascii_case("SELECT")
        || tokens[1] != "*"
        || !tokens[2].eq_ignore_ascii_case("FROM")
    {
        return None;
    }
    let (schema, name, end) = if tokens.get(4).is_some_and(|s| s == ".") {
        (Some(tokens[3].as_str()), tokens.get(5)?.as_str(), 6)
    } else {
        (None, tokens[3].as_str(), 4)
    };
    if tokens[end..].iter().enumerate().any(|(index, t)| {
        if quoted[end + index] {
            return false;
        }
        [
            "JOIN",
            "UNION",
            "INTERSECT",
            "EXCEPT",
            "SELECT",
            "FROM",
            "INTO",
            "RETURNING",
        ]
        .iter()
        .any(|k| t.eq_ignore_ascii_case(k))
    }) {
        return None;
    }
    // FROM may contain only the known table and one optional alias. Comma
    // joins after an alias are still joins, and must never expose cell editing.
    let tail = &tokens[end..];
    let clause = tail
        .iter()
        .enumerate()
        .position(|(index, token)| {
            if quoted[end + index] {
                return false;
            }
            [
                "WHERE", "ORDER", "GROUP", "HAVING", "LIMIT", "OFFSET", "FETCH", "FOR",
            ]
            .iter()
            .any(|keyword| token.eq_ignore_ascii_case(keyword))
        })
        .unwrap_or(tail.len());
    let from_tail = tail[..clause]
        .iter()
        .filter(|token| token.as_str() != ";")
        .collect::<Vec<_>>();
    let alias = |token: &&String| {
        !token.is_empty() && token.chars().all(|c| c.is_alphanumeric() || c == '_')
    };
    if !(from_tail.is_empty()
        || (from_tail.len() == 1 && alias(&from_tail[0]))
        || (from_tail.len() == 2
            && from_tail[0].eq_ignore_ascii_case("AS")
            && alias(&from_tail[1])))
    {
        return None;
    }

    let matches = tables
        .iter()
        .filter(|t| {
            t.name == name && schema.is_none_or(|s| s == t.schema) && t.kind.contains("table")
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return None;
    }
    Some(matches[0].clone())
}
pub fn quote_identifier(dialect: &Dialect, name: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        !name.is_empty() && name.len() <= 256 && !name.chars().any(|c| c == '\0' || c.is_control()),
        "identifier_invalid"
    );
    let (open, close) = dialect.name_quotes;
    Ok(format!(
        "{open}{}{close}",
        name.replace(close, &format!("{close}{close}"))
    ))
}
pub fn qualified(dialect: &Dialect, t: &TableRef) -> anyhow::Result<String> {
    if t.schema.is_empty() {
        quote_identifier(dialect, &t.name)
    } else {
        Ok(format!(
            "{}.{}",
            quote_identifier(dialect, &t.schema)?,
            quote_identifier(dialect, &t.name)?
        ))
    }
}
pub fn literal(dialect: &Dialect, cell: &Cell) -> anyhow::Result<String> {
    Ok(match cell {
        Cell::Null => "NULL".into(),
        Cell::Bool(v) => if *v {
            dialect.booleans.0
        } else {
            dialect.booleans.1
        }
        .into(),
        Cell::Integer(s) => s
            .parse::<i128>()
            .map_err(|_| anyhow::anyhow!("integer_invalid"))?
            .to_string(),
        Cell::Decimal(s) => {
            let v: bigdecimal::BigDecimal =
                s.parse().map_err(|_| anyhow::anyhow!("decimal_invalid"))?;
            v.to_string()
        }
        Cell::Float(s) => {
            let v: f64 = s.parse().map_err(|_| anyhow::anyhow!("float_invalid"))?;
            anyhow::ensure!(v.is_finite(), "float_invalid");
            v.to_string()
        }
        Cell::Text(s) | Cell::Json(s) => {
            anyhow::ensure!(!s.contains('\0'), "text_nul_unsupported");
            match dialect.text {
                TextLiteral::Standard => format!("'{}'", s.replace('\'', "''")),
                TextLiteral::National => format!("N'{}'", s.replace('\'', "''")),
                TextLiteral::PostgresEscape => {
                    format!("E'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
                }
                TextLiteral::Backslash => {
                    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
                }
                TextLiteral::MysqlHex => {
                    let h: String = s.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
                    if h.is_empty() {
                        "''".into()
                    } else {
                        format!("_utf8mb4 0x{h}")
                    }
                }
            }
        }
        Cell::Bytes(b) => {
            let h: String = b.iter().map(|b| format!("{b:02x}")).collect();
            let (before, after) = dialect.bytes;
            format!("{before}{h}{after}")
        }
        Cell::Unsupported(_) => anyhow::bail!("unsupported_cell_type"),
    })
}
pub fn predicate(dialect: &Dialect, column: &str, value: &Cell) -> anyhow::Result<String> {
    let c = quote_identifier(dialect, column)?;
    Ok(if matches!(value, Cell::Null) {
        format!("{c} IS NULL")
    } else {
        format!("{c} = {}", literal(dialect, value)?)
    })
}
pub fn update_cell(
    dialect: &Dialect,
    table: &TableRef,
    column: &str,
    old: &Cell,
    new: &Cell,
    pk: &[(String, Cell)],
) -> anyhow::Result<String> {
    anyhow::ensure!(
        !pk.is_empty()
            && pk
                .iter()
                .all(|(_, v)| !matches!(v, Cell::Null | Cell::Unsupported(_))),
        "primary_key_required"
    );
    let mut terms = pk
        .iter()
        .map(|(n, v)| predicate(dialect, n, v))
        .collect::<anyhow::Result<Vec<_>>>()?;
    terms.push(predicate(dialect, column, old)?);
    Ok(format!(
        "UPDATE {} SET {} = {} WHERE {}",
        qualified(dialect, table)?,
        quote_identifier(dialect, column)?,
        literal(dialect, new)?,
        terms.join(" AND ")
    ))
}
pub fn delete_row(
    dialect: &Dialect,
    table: &TableRef,
    pk: &[(String, Cell)],
) -> anyhow::Result<String> {
    anyhow::ensure!(
        !pk.is_empty() && pk.iter().all(|(_, v)| !matches!(v, Cell::Null)),
        "primary_key_required"
    );
    Ok(format!(
        "DELETE FROM {} WHERE {}",
        qualified(dialect, table)?,
        pk.iter()
            .map(|(n, v)| predicate(dialect, n, v))
            .collect::<anyhow::Result<Vec<_>>>()?
            .join(" AND ")
    ))
}
/// `INSERT` of one row. Columns that are not named take their defaults; a row
/// without any named column takes every default.
pub fn insert_row(
    dialect: &Dialect,
    table: &TableRef,
    values: &[(String, Cell)],
) -> anyhow::Result<String> {
    let target = qualified(dialect, table)?;
    if values.is_empty() {
        return Ok(match dialect.empty_insert {
            EmptyInsert::EmptyLists => format!("INSERT INTO {target} () VALUES ()"),
            EmptyInsert::DefaultValues => format!("INSERT INTO {target} DEFAULT VALUES"),
        });
    }
    let names = values
        .iter()
        .map(|(name, _)| quote_identifier(dialect, name))
        .collect::<anyhow::Result<Vec<_>>>()?
        .join(", ");
    let literals = values
        .iter()
        .map(|(_, value)| literal(dialect, value))
        .collect::<anyhow::Result<Vec<_>>>()?
        .join(", ");
    Ok(format!(
        "INSERT INTO {target} ({names}) VALUES ({literals})"
    ))
}
/// The statement that makes one change of one record.
pub fn change_statement(dialect: &Dialect, change: &RowChange) -> anyhow::Result<String> {
    match change {
        RowChange::UpdateCell {
            table,
            column,
            old,
            new,
            key,
        } => update_cell(dialect, table, column, old, new, key),
        RowChange::InsertRow { table, values } => insert_row(dialect, table, values),
        RowChange::DeleteRow { table, key } => delete_row(dialect, table, key),
    }
}

/// Split a script into statements at the `;` that stand outside comments,
/// quoted identifiers and string literals, read the way the engine of
/// `dialect` reads them.
pub fn statements(dialect: &Dialect, sql: &str) -> anyhow::Result<Vec<String>> {
    Ok(statement_ranges(dialect, sql)?
        .into_iter()
        .map(|range| sql[range].to_owned())
        .collect())
}
/// Where the statements of a script are: byte ranges without the `;` and the
/// white space around them, split as [`statements`] splits them.
pub fn statement_ranges(
    dialect: &Dialect,
    sql: &str,
) -> anyhow::Result<Vec<std::ops::Range<usize>>> {
    anyhow::ensure!(
        sql.len() <= MAX_SQL_BYTES && !sql.contains('\0'),
        "sql_file_invalid"
    );
    let mut out = Vec::new();
    let mut push = |range: std::ops::Range<usize>| {
        let text = &sql[range.clone()];
        let start = range.start + (text.len() - text.trim_start().len());
        let end = range.end - (text.len() - text.trim_end().len());
        if start < end {
            out.push(start..end);
        }
    };
    let mut start = 0;
    lex(dialect, sql, |kind, range| {
        if kind == Lexeme::Symbol && &sql[range.clone()] == ";" {
            push(start..range.start);
            start = range.end;
        }
    })?;
    push(start..sql.len());
    anyhow::ensure!(out.len() <= 10_000, "sql_too_many_statements");
    Ok(out)
}
/// The statement at a byte offset of a script: the one the offset falls in,
/// else the last one before it, else the first one.
pub fn statement_at(
    ranges: &[std::ops::Range<usize>],
    offset: usize,
) -> Option<std::ops::Range<usize>> {
    ranges
        .iter()
        .find(|r| r.start <= offset && offset <= r.end)
        .or_else(|| ranges.iter().rev().find(|r| r.end <= offset))
        .or_else(|| ranges.first())
        .cloned()
}
/// Accept a script that at least one registered SQL engine reads to its end.
/// A file is opened before anybody knows which database it will run on.
pub fn check_script(sql: &str) -> anyhow::Result<()> {
    let mut refusal = None;
    for engine in Engine::all() {
        let spec = engine.spec();
        if spec.capabilities.language != QueryLanguage::Sql {
            continue;
        }
        match statements(&spec.dialect, sql) {
            Ok(_) => return Ok(()),
            Err(error) => refusal = refusal.or(Some(error)),
        }
    }
    Err(refusal.unwrap_or_else(|| anyhow::anyhow!("sql_file_invalid")))
}
pub fn redact_sql(sql: &str) -> String {
    let mut out = sql.to_owned();
    for token in ["password", "passwd", "secret", "token"] {
        let lower = out.to_lowercase();
        if lower.contains(token) {
            return "-- SQL omitted: possible credential statement".into();
        }
    }
    let mut end = MAX_SQL_BYTES.min(out.len());
    while !out.is_char_boundary(end) {
        end -= 1;
    }
    out.truncate(end);
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn statements_are_found_by_where_the_cursor_is() {
        let sql = "  SELECT 1;\n\nSELECT ';' ; -- x;\nSELECT 3  ";
        let ranges = statement_ranges(&POSTGRES, sql).unwrap();
        let texts = ranges.iter().map(|r| &sql[r.clone()]).collect::<Vec<_>>();
        assert_eq!(texts, ["SELECT 1", "SELECT ';'", "-- x;\nSELECT 3"]);
        // Inside, at the end, between two statements, before the first.
        assert_eq!(statement_at(&ranges, 5), Some(ranges[0].clone()));
        assert_eq!(
            statement_at(&ranges, ranges[1].end),
            Some(ranges[1].clone())
        );
        assert_eq!(
            statement_at(&ranges, ranges[0].end + 2),
            Some(ranges[0].clone())
        );
        assert_eq!(statement_at(&ranges, 0), Some(ranges[0].clone()));
        assert_eq!(statement_at(&[], 3), None);
    }
    #[test]
    fn lexical_sql() {
        assert_eq!(
            statements(
                &POSTGRES,
                "SELECT ';';/* x;y /*nested*/ */ SELECT $$a;b$$; -- x;\n SELECT 3"
            )
            .unwrap()
            .len(),
            3
        );
        for engine in [&POSTGRES, &MYSQL, &SQLITE] {
            assert_eq!(
                statements(engine, "select 'broken")
                    .unwrap_err()
                    .to_string(),
                "sql_unterminated_literal"
            );
            assert!(statements(engine, "select 1 /* open").is_err());
            assert_eq!(
                statements(engine, "SELECT 'a;b', \"c;d\" ; SELECT 'it''s';")
                    .unwrap()
                    .len(),
                2
            );
            // Polish text keeps its bytes; the split happens on ASCII only.
            assert_eq!(
                statements(engine, "SELECT 'Zażółć; gęślą' AS jaźń; SELECT 2").unwrap(),
                ["SELECT 'Zażółć; gęślą' AS jaźń", "SELECT 2"]
            );
        }
    }
    #[test]
    fn a_backslash_ends_a_literal_only_where_the_engine_says_so() {
        // The exact text that made a filtered table read-only.
        let lone = "SELECT * FROM t WHERE a LIKE '%x%' ESCAPE '\\'";
        assert_eq!(statements(&SQLITE, lone).unwrap().len(), 1);
        assert_eq!(statements(&POSTGRES, lone).unwrap().len(), 1);
        // MySQL and MariaDB read `\'` as an escaped quote: the literal is open.
        assert!(statements(&MYSQL, lone).is_err());
        let path = "INSERT INTO t VALUES ('C:\\'); SELECT ';'";
        assert_eq!(statements(&SQLITE, path).unwrap().len(), 2);
        assert_eq!(statements(&POSTGRES, path).unwrap().len(), 2);
        // MySQL: the first literal runs up to the quote of the second one.
        assert!(statements(&MYSQL, path).is_err());
        assert_eq!(
            statements(&MYSQL, "INSERT INTO t VALUES ('C:\\\\'); SELECT ';'")
                .unwrap()
                .len(),
            2
        );
        // MySQL escapes inside both kinds of string quotes.
        let escaped = "SELECT 'it\\'s; fine', \"say \\\"hi;\\\"\"; SELECT 2";
        assert_eq!(statements(&MYSQL, escaped).unwrap().len(), 2);
        // PostgreSQL escapes only inside E'…', in either letter case.
        for text in [
            "SELECT E'it\\'s; fine'; SELECT 2",
            "SELECT e'a\\\\'; SELECT 2",
        ] {
            assert_eq!(statements(&POSTGRES, text).unwrap().len(), 2, "{text}");
        }
        // A word that merely ends in `e` does not make the string an E'…'.
        assert_eq!(
            statements(&POSTGRES, "SELECT name'\\'; SELECT 2")
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn comments_and_identifier_quotes_follow_the_engine() {
        // `#` is a comment in MySQL only.
        assert_eq!(
            statements(&MYSQL, "SELECT 1 # a;b\n; SELECT 2")
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            statements(&POSTGRES, "SELECT 1 # 2; SELECT 2")
                .unwrap()
                .len(),
            2
        );
        // `--` needs white space after it in MySQL: 5--1 is arithmetic.
        assert_eq!(
            statements(&MYSQL, "SELECT 5--1; SELECT 2").unwrap().len(),
            2
        );
        assert_eq!(
            statements(&SQLITE, "SELECT 5--1; SELECT 2").unwrap().len(),
            1
        );
        assert_eq!(
            statements(&MYSQL, "SELECT 5 -- 1; SELECT 2\n; SELECT 3")
                .unwrap()
                .len(),
            2
        );
        // Only PostgreSQL nests block comments.
        let nested = "/* a /* b */ ; */ SELECT 1";
        assert_eq!(statements(&POSTGRES, nested).unwrap().len(), 1);
        assert_eq!(statements(&MYSQL, nested).unwrap().len(), 2);
        // Dollar bodies exist in PostgreSQL only; `$1` is a parameter there.
        let body = "SELECT $fn$ a; b $fn$; SELECT 2";
        assert_eq!(statements(&POSTGRES, body).unwrap().len(), 2);
        assert_eq!(statements(&SQLITE, body).unwrap().len(), 3);
        assert_eq!(
            statements(&POSTGRES, "SELECT $1; SELECT $2").unwrap().len(),
            2
        );
        assert!(statements(&POSTGRES, "SELECT $a$ open").is_err());
        // Backticks and brackets quote identifiers where the engine has them.
        assert_eq!(
            statements(&MYSQL, "SELECT `a;b`; SELECT 2").unwrap().len(),
            2
        );
        assert_eq!(
            statements(&SQLITE, "SELECT [a;b], `c;d`; SELECT 2")
                .unwrap()
                .len(),
            2
        );
    }
    #[test]
    fn a_file_is_accepted_when_any_engine_reads_it() {
        check_script("INSERT INTO t VALUES ('C:\\'); SELECT 1").unwrap();
        check_script("SELECT 'it\\'s'; SELECT 1").unwrap();
        assert_eq!(
            check_script("SELECT 'open").unwrap_err().to_string(),
            "sql_unterminated_literal"
        );
        assert!(check_script("SELECT \0").is_err());
    }
    #[test]
    fn like_patterns_escape_their_wildcards_without_a_backslash() {
        assert_eq!(like_pattern("50%_off!"), "50!%!_off!!");
        assert_eq!(like_pattern("C:\\dane"), "C:\\dane");
        assert_eq!(like_pattern("zażółć"), "zażółć");
    }
    #[test]
    fn identifier_escape() {
        assert_eq!(quote_identifier(&POSTGRES, "a\"b").unwrap(), "\"a\"\"b\"");
        assert!(quote_identifier(&MYSQL, "bad\0").is_err());
    }
    #[test]
    fn exact_and_safe() {
        assert_eq!(
            literal(&SQLITE, &Cell::Integer("18446744073709551615".into())).unwrap(),
            "18446744073709551615"
        );
        assert_eq!(
            literal(&POSTGRES, &Cell::Text("x'\\; DROP".into())).unwrap(),
            "E'x''\\\\; DROP'"
        );
        assert!(literal(&SQLITE, &Cell::Integer("1;DROP".into())).is_err());
    }
    #[test]
    fn optimistic_null() {
        let s = update_cell(
            &SQLITE,
            &TableRef {
                schema: "main".into(),
                name: "a".into(),
                kind: "table".into(),
            },
            "value",
            &Cell::Null,
            &Cell::Text("new".into()),
            &[("id".into(), Cell::Integer("2".into()))],
        )
        .unwrap();
        assert!(s.ends_with("\"id\" = 2 AND \"value\" IS NULL"));
        assert!(
            update_cell(
                &SQLITE,
                &TableRef {
                    schema: "".into(),
                    name: "a".into(),
                    kind: "table".into()
                },
                "x",
                &Cell::Null,
                &Cell::Null,
                &[]
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod redaction_boundary_test {
    use super::*;
    #[test]
    fn redaction_handles_multibyte_size_boundary() {
        let value = "🦀".repeat(MAX_SQL_BYTES / 4 + 2);
        let result = redact_sql(&value);
        assert!(result.len() <= MAX_SQL_BYTES);
        assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    }
}

/// A `SELECT *` on one known table that returns no row carries no column
/// names: the drivers describe a result by its first row. Name the columns
/// after the table, in its order, so that an empty table and a filter without
/// a match still show their headers, can be filtered by another column and
/// accept a new row. A result with rows, or one without a known table, is
/// left as it is.
pub fn name_empty_result(
    table: Option<&TableRef>,
    metadata: &[ColumnInfo],
    result: &mut QueryResult,
) {
    if table.is_some() && result.columns.is_empty() && result.rows.is_empty() {
        result.columns = metadata
            .iter()
            .map(|column| crate::model::ResultColumn {
                name: column.name.clone(),
                data_type: column.data_type.clone(),
            })
            .collect();
    }
}
/// Result provenance is a separate guard: duplicate/unknown output names make
/// optimistic updates ambiguous even when the SQL parser recognized one table.
pub fn editable_result_table(
    table: Option<TableRef>,
    metadata: &[ColumnInfo],
    result: &QueryResult,
) -> Option<TableRef> {
    let table = table?;
    if metadata.len() != result.columns.len() || metadata.is_empty() {
        return None;
    }
    let mut names = std::collections::HashSet::new();
    for column in &result.columns {
        if !names.insert(&column.name)
            || metadata.iter().filter(|m| m.name == column.name).count() != 1
        {
            return None;
        }
    }
    if result
        .rows
        .iter()
        .any(|row| row.len() != result.columns.len())
    {
        return None;
    }
    Some(table)
}
#[cfg(test)]
mod provenance_guard_tests {
    use super::*;
    fn table() -> TableRef {
        TableRef {
            schema: "main".into(),
            name: "customers".into(),
            kind: "table".into(),
        }
    }
    #[test]
    fn alias_comma_join_is_never_editable() {
        for query in [
            "SELECT * FROM main.customers c,main.orders o WHERE c.id+1=o.id",
            "SELECT * FROM main.customers AS c,main.orders AS o WHERE c.name=o.name",
            "SELECT * FROM customers,orders",
            "SELECT * FROM customers c CROSS JOIN orders o",
            "SELECT * FROM main.customers \"where\",main.orders o",
            "SELECT * FROM main.customers AS \"order\",main.orders o",
            "SELECT * FROM main.customers \"limit\",main.orders o",
            "SELECT * FROM main.customers `where`,main.orders o",
        ] {
            for engine in [&POSTGRES, &MYSQL, &SQLITE] {
                assert!(
                    editable_table(engine, query, &[table()]).is_none(),
                    "{query}"
                );
            }
        }
        for query in [
            "SELECT * FROM main.customers c WHERE c.id IN (1,2)",
            "SELECT * FROM main.customers AS c WHERE c.id=2",
            "SELECT * FROM main.customers;",
            "SELECT * FROM main.customers ORDER BY id",
            "SELECT * FROM main.customers \"where\" WHERE \"where\".id=2",
            "SELECT * FROM main.customers AS \"order\" WHERE \"order\".id=2",
        ] {
            for engine in [&POSTGRES, &MYSQL, &SQLITE] {
                // `"…"` is a string in MySQL, so it cannot name an alias there.
                let expected = engine != &MYSQL || !query.contains('"');
                assert_eq!(
                    editable_table(engine, query, &[table()]).is_some(),
                    expected,
                    "{engine:?} {query}"
                );
            }
        }
    }
    #[test]
    fn literals_never_hide_a_second_table_or_statement() {
        let editable =
            |engine: &Dialect, query: &str| editable_table(engine, query, &[table()]).is_some();
        // A lone backslash closes the literal in SQLite and PostgreSQL.
        let lone = "SELECT * FROM customers WHERE name LIKE '%a%' ESCAPE '\\'";
        assert!(editable(&SQLITE, lone));
        assert!(editable(&POSTGRES, lone));
        assert!(!editable(&MYSQL, lone));
        // What follows such a literal is SQL again, so a join is seen.
        let join =
            "SELECT * FROM customers WHERE name = '\\' UNION SELECT * FROM orders WHERE 'x' = 'x'";
        assert!(!editable(&SQLITE, join));
        assert!(!editable(&POSTGRES, join));
        // In MySQL the same text keeps the keyword inside the literal; a real
        // one outside it is still found.
        assert!(!editable(
            &MYSQL,
            "SELECT * FROM customers WHERE name = 'a\\'b' UNION SELECT * FROM orders"
        ));
        assert!(editable(
            &MYSQL,
            "SELECT * FROM `customers` WHERE name = 'a\\' UNION b'"
        ));
        // Quoted names resolve per engine.
        assert!(editable(&MYSQL, "SELECT * FROM `main`.`customers`"));
        assert!(editable(&POSTGRES, "SELECT * FROM \"main\".\"customers\""));
        assert!(editable(&SQLITE, "SELECT * FROM [main].[customers]"));
        assert!(!editable(&MYSQL, "SELECT * FROM \"customers\""));
        // Two statements are never editable, comments or not.
        for engine in [&POSTGRES, &MYSQL, &SQLITE] {
            assert!(!editable(engine, "SELECT * FROM customers; SELECT 1"));
            assert!(editable(engine, "SELECT * FROM customers /* ; */ ;"));
            assert!(!editable(
                engine,
                "SELECT * FROM customers WHERE name = 'open"
            ));
        }
    }
    #[test]
    fn actual_result_metadata_must_be_unique_and_complete() {
        let metadata = vec![
            ColumnInfo {
                name: "id".into(),
                data_type: "INTEGER".into(),
                nullable: false,
                primary_key: true,
                default: None,
            },
            ColumnInfo {
                name: "name".into(),
                data_type: "TEXT".into(),
                nullable: true,
                primary_key: false,
                default: None,
            },
        ];
        let mut result = QueryResult {
            columns: vec![
                crate::model::ResultColumn {
                    name: "id".into(),
                    data_type: "INTEGER".into(),
                },
                crate::model::ResultColumn {
                    name: "name".into(),
                    data_type: "TEXT".into(),
                },
            ],
            ..QueryResult::default()
        };
        assert!(editable_result_table(Some(table()), &metadata, &result).is_some());
        result.columns[1].name = "id".into();
        assert!(editable_result_table(Some(table()), &metadata, &result).is_none());
        result.columns[1].name = "foreign.name".into();
        assert!(editable_result_table(Some(table()), &metadata, &result).is_none());
        // No row, no column names: the table lends its own, and only then.
        let mut empty = QueryResult::default();
        assert!(editable_result_table(Some(table()), &metadata, &empty).is_none());
        name_empty_result(None, &metadata, &mut empty);
        assert!(empty.columns.is_empty());
        name_empty_result(Some(&table()), &metadata, &mut empty);
        assert_eq!(
            empty
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["id", "name"]
        );
        assert_eq!(empty.columns[0].data_type, "INTEGER");
        assert!(editable_result_table(Some(table()), &metadata, &empty).is_some());
        let mut named = result.clone();
        name_empty_result(Some(&table()), &metadata[..1], &mut named);
        assert_eq!(named.columns.len(), 2);
    }
}
