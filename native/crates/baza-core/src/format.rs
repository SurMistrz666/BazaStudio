//! Beautify SQL: one clause per line, the items of a list one per line,
//! conditions under each other and subqueries indented, keywords in capital
//! letters. The text is read with the rules of the engine's quotes and
//! comments, so a literal, a quoted name or a comment is never changed;
//! only the white space between them and the case of keywords is. A
//! statement that is not a query (`CREATE TABLE`, …) is left as written.
use crate::engine::Dialect;
use crate::sql::{Lexeme, lex};

/// Spaces of one level of indentation.
const INDENT: &str = "  ";

/// Words written in capital letters, where they stand as keywords.
const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "IN",
    "IS",
    "NULL",
    "LIKE",
    "ILIKE",
    "BETWEEN",
    "EXISTS",
    "AS",
    "ON",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "OUTER",
    "CROSS",
    "NATURAL",
    "USING",
    "GROUP",
    "BY",
    "ORDER",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "FETCH",
    "NEXT",
    "ROWS",
    "ONLY",
    "DISTINCT",
    "ALL",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "ASC",
    "DESC",
    "WITH",
    "RECURSIVE",
    "RETURNING",
    "TRUE",
    "FALSE",
    "TOP",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "CAST",
    "COALESCE",
    "NULLIF",
    "OVER",
    "PARTITION",
    "DUPLICATE",
    "KEY",
    "ANY",
    "SOME",
    "ESCAPE",
    "INTERVAL",
    "STRAIGHT_JOIN",
];
/// Keywords that are called like functions: no space before their bracket.
const CALLS: &[&str] = &[
    "COUNT", "SUM", "AVG", "MIN", "MAX", "CAST", "COALESCE", "NULLIF", "LEFT", "RIGHT", "OVER",
    "VALUES",
];
/// Words that start a join, and the ones that may come before `JOIN`.
const JOINS: &[&str] = &[
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "NATURAL",
    "STRAIGHT_JOIN",
];
const JOIN_PREFIX: &[&str] = &[
    "INNER", "LEFT", "RIGHT", "FULL", "CROSS", "NATURAL", "OUTER",
];
/// Operators written as two or three characters.
const OPERATORS: &[&str] = &[
    "<=", ">=", "<>", "!=", "||", "::", "->", "->>", ":=", "=>", "<<", ">>", "**", "#>", "#>>",
    "@>", "<@",
];
/// Statements that are formatted; any other is copied as written.
const QUERIES: &[&str] = &["SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "VALUES"];

#[derive(Clone, Copy, Debug)]
struct Tok<'a> {
    kind: Lexeme,
    text: &'a str,
    start: usize,
    end: usize,
}
impl Tok<'_> {
    fn upper(&self) -> String {
        if self.kind == Lexeme::Word {
            self.text.to_ascii_uppercase()
        } else {
            String::new()
        }
    }
    fn is(&self, symbol: &str) -> bool {
        self.kind == Lexeme::Symbol && self.text == symbol
    }
    fn word(&self, word: &str) -> bool {
        self.kind == Lexeme::Word && self.text.eq_ignore_ascii_case(word)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Clause {
    None,
    Select,
    From,
    Where,
    Group,
    Having,
    Order,
    Set,
    Values,
    Limit,
    With,
    Other,
}

#[derive(Clone, Copy, Debug)]
struct Frame {
    /// A subquery is formatted; any other bracket is written inline.
    sub: bool,
    /// The level of the clause keywords inside it.
    base: usize,
    clause: Clause,
}

/// Format SQL text. Fails with `sql_unterminated_literal` when a quote or a
/// comment never ends.
pub fn beautify(dialect: &Dialect, sql: &str) -> anyhow::Result<String> {
    let mut tokens = Vec::new();
    lex(dialect, sql, |kind, range| {
        if kind != Lexeme::Space {
            tokens.push(Tok {
                kind,
                text: &sql[range.clone()],
                start: range.start,
                end: range.end,
            });
        }
    })?;
    let tokens = merge_operators(sql, tokens);
    let mut statements = Vec::new();
    let mut current: Vec<Tok> = Vec::new();
    for token in tokens {
        if token.is(";") {
            statements.push((std::mem::take(&mut current), true));
        } else {
            current.push(token);
        }
    }
    if !current.is_empty() {
        statements.push((current, false));
    }
    let mut out = Vec::new();
    for (statement, ended) in statements {
        let (Some(first), Some(last)) = (statement.first(), statement.last()) else {
            continue;
        };
        let opening = statement
            .iter()
            .find(|t| t.kind == Lexeme::Word)
            .map(Tok::upper)
            .unwrap_or_default();
        let mut text = if QUERIES.contains(&opening.as_str()) {
            Formatter::default().statement(&statement)
        } else {
            sql[first.start..last.end].to_owned()
        };
        if ended {
            text.push(';');
        }
        out.push(text);
    }
    Ok(out.join("\n\n"))
}

/// Join symbols that make one operator, as `<` and `=` make `<=`.
fn merge_operators<'a>(sql: &'a str, tokens: Vec<Tok<'a>>) -> Vec<Tok<'a>> {
    let mut out: Vec<Tok> = Vec::with_capacity(tokens.len());
    for token in tokens {
        if let Some(last) = out.last_mut()
            && last.kind == Lexeme::Symbol
            && token.kind == Lexeme::Symbol
            && last.end == token.start
            && OPERATORS.contains(&&sql[last.start..token.end])
        {
            last.end = token.end;
            last.text = &sql[last.start..token.end];
            continue;
        }
        out.push(token);
    }
    out
}

#[derive(Default)]
struct Formatter {
    out: String,
    line_start: bool,
    stack: Vec<Frame>,
    /// The next token starts the content line of a clause.
    pending: bool,
    /// The `AND` of a `BETWEEN` is coming.
    between: bool,
    /// The last token written, and whether it was a unary sign.
    previous: Option<(Lexeme, String, usize, bool)>,
    unary: bool,
    /// The statement starts with this word.
    kind: String,
}

impl Formatter {
    fn top(&self) -> Frame {
        *self.stack.last().expect("a frame")
    }
    fn top_mut(&mut self) -> &mut Frame {
        self.stack.last_mut().expect("a frame")
    }
    fn newline(&mut self, level: usize) {
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        if !self.out.is_empty() {
            self.out.push('\n');
        }
        self.out.push_str(&INDENT.repeat(level));
        self.line_start = true;
    }
    fn content(&self) -> usize {
        self.top().base + 1
    }

    fn statement(mut self, tokens: &[Tok]) -> String {
        self.stack.push(Frame {
            sub: true,
            base: 0,
            clause: Clause::None,
        });
        self.line_start = true;
        self.kind = tokens
            .iter()
            .find(|t| t.kind == Lexeme::Word)
            .map(Tok::upper)
            .unwrap_or_default();
        let mut i = 0;
        while i < tokens.len() {
            i += self.token(tokens, i);
        }
        self.out.trim_end().to_owned()
    }

    /// Write the token at `i`; returns how many tokens were used.
    fn token(&mut self, tokens: &[Tok], i: usize) -> usize {
        let t = tokens[i];
        let next = tokens.get(i + 1);
        let structured = self.top().sub;
        match t.kind {
            Lexeme::Comment => {
                let line = t.text.starts_with("--") || t.text.starts_with('#');
                if self.pending {
                    self.pending = false;
                    let level = self.content();
                    self.newline(level);
                }
                if !self.line_start {
                    self.out.push(' ');
                }
                self.out.push_str(t.text);
                self.line_start = false;
                if line {
                    // The rest of the line belongs to the comment.
                    let level = if structured {
                        self.content()
                    } else {
                        self.top().base + 1
                    };
                    self.newline(level);
                }
                return 1;
            }
            Lexeme::Symbol if t.text == "(" => {
                let sub = next.is_some_and(|n| n.word("SELECT") || n.word("WITH"));
                self.emit(t, "(");
                // A subquery is one level in from the line it opens on: a
                // query of `WITH` from the keyword, any other from the content.
                let base = match (sub, self.top().clause) {
                    (true, Clause::With) => self.top().base + 1,
                    (true, _) => self.content() + 1,
                    (false, _) => self.top().base,
                };
                self.stack.push(Frame {
                    sub,
                    base,
                    clause: Clause::None,
                });
                if sub {
                    self.newline(base);
                }
                return 1;
            }
            Lexeme::Symbol if t.text == ")" => {
                let frame = if self.stack.len() > 1 {
                    self.stack.pop().expect("a frame")
                } else {
                    self.top()
                };
                if frame.sub {
                    self.pending = false;
                    self.newline(frame.base.saturating_sub(1));
                }
                self.emit(t, ")");
                return 1;
            }
            Lexeme::Symbol if t.text == "," => {
                self.emit(t, ",");
                if structured {
                    match self.top().clause {
                        Clause::Select
                        | Clause::From
                        | Clause::Group
                        | Clause::Order
                        | Clause::Set
                        | Clause::Values => {
                            let level = self.content();
                            self.newline(level);
                        }
                        Clause::With => {
                            let level = self.top().base;
                            self.newline(level);
                        }
                        _ => {}
                    }
                }
                return 1;
            }
            _ => {}
        }
        // A word next to a dot is a name (`t.limit`), never a clause.
        let dotted = (i > 0 && tokens[i - 1].is(".")) || next.is_some_and(|n| n.is("."));
        if t.kind != Lexeme::Word || !structured || dotted {
            let text = self.cased(tokens, i);
            self.emit(t, &text);
            return 1;
        }
        let upper = t.upper();
        let previous_word = self.previous_word();
        // Two words that make one clause keyword.
        if matches!(upper.as_str(), "GROUP" | "ORDER") && next.is_some_and(|n| n.word("BY")) {
            self.block(
                t,
                &format!("{upper} BY"),
                if upper == "GROUP" {
                    Clause::Group
                } else {
                    Clause::Order
                },
            );
            return 2;
        }
        match upper.as_str() {
            "SELECT" => self.block(t, "SELECT", Clause::Select),
            "FROM" if previous_word == "DELETE" => {
                self.emit(t, "FROM");
                self.top_mut().clause = Clause::From;
            }
            "FROM" => self.block(t, "FROM", Clause::From),
            "WHERE" => self.block(t, "WHERE", Clause::Where),
            "HAVING" => self.block(t, "HAVING", Clause::Having),
            "SET" if self.kind == "UPDATE" => self.block(t, "SET", Clause::Set),
            // The rows of an `INSERT`, not `VALUES(a)` of `ON DUPLICATE KEY`.
            "VALUES"
                if (self.kind == "INSERT" && self.top().clause == Clause::Other)
                    || (self.kind == "VALUES" && self.top().clause == Clause::None) =>
            {
                self.block(t, "VALUES", Clause::Values)
            }
            "LIMIT" | "OFFSET" | "FETCH" | "RETURNING" => {
                self.line(t, &upper);
                self.top_mut().clause = Clause::Limit;
            }
            "UNION" | "INTERSECT" | "EXCEPT" => {
                self.line(t, &upper);
                self.top_mut().clause = Clause::None;
            }
            "WITH" => {
                self.emit(t, "WITH");
                self.top_mut().clause = Clause::With;
            }
            "INSERT" | "UPDATE" | "DELETE"
                if self.out.trim().is_empty() || self.top().clause == Clause::With =>
            {
                self.line(t, &upper);
                self.top_mut().clause = Clause::Other;
            }
            "DISTINCT" | "ALL" if self.pending && self.top().clause == Clause::Select => {
                // `SELECT DISTINCT` stays on the line of its keyword.
                self.pending = false;
                self.emit(t, &upper);
                self.pending = true;
            }
            word if JOINS.contains(&word)
                && self.top().clause == Clause::From
                && !JOIN_PREFIX.contains(&previous_word.as_str()) =>
            {
                let level = self.content();
                self.pending = false;
                self.newline(level);
                self.emit(t, &upper);
            }
            "AND" if self.between => {
                self.between = false;
                self.emit(t, "AND");
            }
            "AND" | "OR"
                if matches!(
                    self.top().clause,
                    Clause::Where | Clause::Having | Clause::From
                ) =>
            {
                let level = self.content() + usize::from(self.top().clause == Clause::From);
                self.pending = false;
                self.newline(level);
                self.emit(t, &upper);
            }
            "BETWEEN" => {
                self.between = true;
                self.emit(t, "BETWEEN");
            }
            _ => {
                let text = self.cased(tokens, i);
                self.emit(t, &text);
            }
        }
        1
    }

    /// The last word written, in capitals.
    fn previous_word(&self) -> String {
        match &self.previous {
            Some((Lexeme::Word, text, _, _)) => text.to_ascii_uppercase(),
            _ => String::new(),
        }
    }

    /// A clause keyword on a line of its own, its content on the next ones.
    fn block(&mut self, t: Tok, text: &str, clause: Clause) {
        self.pending = false;
        let base = self.top().base;
        if !self.line_start {
            self.newline(base);
        }
        self.emit(t, text);
        self.top_mut().clause = clause;
        self.pending = true;
    }

    /// A keyword that starts a line, its content after it.
    fn line(&mut self, t: Tok, text: &str) {
        self.pending = false;
        let base = self.top().base;
        if !self.line_start {
            self.newline(base);
        }
        self.emit(t, text);
    }

    /// A word as it is written: a keyword in capitals, unless it stands where
    /// a name does (after `FROM`, `AS`, a dot, …).
    fn cased(&self, tokens: &[Tok], i: usize) -> String {
        let t = tokens[i];
        if t.kind != Lexeme::Word {
            return t.text.to_owned();
        }
        let upper = t.upper();
        let dotted =
            (i > 0 && tokens[i - 1].is(".")) || tokens.get(i + 1).is_some_and(|n| n.is("."));
        let named = matches!(
            self.previous_word().as_str(),
            "FROM" | "JOIN" | "INTO" | "UPDATE" | "TABLE" | "AS"
        );
        if KEYWORDS.contains(&upper.as_str()) && !dotted && !named {
            upper
        } else {
            t.text.to_owned()
        }
    }

    /// Write a token with the space the tokens around it need.
    fn emit(&mut self, t: Tok, text: &str) {
        if self.pending {
            self.pending = false;
            let level = self.content();
            self.newline(level);
        }
        let space = !self.line_start && self.space_before(t, text);
        if space {
            self.out.push(' ');
        }
        self.out.push_str(text);
        self.line_start = false;
        // A sign after an operator, a bracket or a keyword is unary.
        let sign = matches!(text, "-" | "+")
            && match &self.previous {
                None => true,
                Some((Lexeme::Symbol, p, _, _)) => p != ")",
                Some((Lexeme::Word, p, _, _)) => {
                    KEYWORDS.contains(&p.to_ascii_uppercase().as_str())
                }
                _ => false,
            };
        self.previous = Some((t.kind, text.to_owned(), t.end, sign));
        self.unary = sign;
    }

    fn space_before(&self, t: Tok, text: &str) -> bool {
        let Some((kind, previous, end, unary)) = &self.previous else {
            return false;
        };
        if matches!(text, "," | ")" | "." | "::") || matches!(previous.as_str(), "(" | "." | "::") {
            return false;
        }
        if *unary {
            return false;
        }
        if text == "(" {
            // A function is called without a space; a keyword list has one.
            // After any other name the bracket stays as written: `count(`
            // is a call, `INTO t (a, b)` a list of columns.
            return match kind {
                Lexeme::Word => {
                    let upper = previous.to_ascii_uppercase();
                    if KEYWORDS.contains(&upper.as_str()) {
                        !CALLS.contains(&upper.as_str())
                    } else {
                        t.start != *end
                    }
                }
                Lexeme::Quoted => t.start != *end,
                _ => true,
            };
        }
        let operator = |s: &str| {
            !s.is_empty()
                && s.chars().all(|c| "=<>!+-*/%|&^~:@#".contains(c))
                && !matches!(s, "@" | ":" | "#")
        };
        if (t.kind == Lexeme::Symbol && operator(text))
            || (*kind == Lexeme::Symbol && operator(previous))
        {
            return true;
        }
        // Other neighbours stay as close as they were written: `@var`,
        // `$1`, `:name`, `N'text'`.
        t.start != *end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::{
        mysql::DIALECT as MYSQL, postgres::DIALECT as POSTGRES, sqlite::DIALECT as SQLITE,
    };

    /// The tokens of a text, keywords in capitals: what formatting may not change.
    fn tokens(dialect: &Dialect, sql: &str) -> Vec<String> {
        let mut out = Vec::new();
        lex(dialect, sql, |kind, range| match kind {
            Lexeme::Space => {}
            Lexeme::Word => out.push(sql[range].to_ascii_uppercase()),
            _ => out.push(sql[range].to_owned()),
        })
        .unwrap();
        out
    }

    #[test]
    fn a_query_gets_a_clause_per_line() {
        let sql = "select t.id, count(*) as n, sum(price*2) from shop.orders t inner join customers c on c.id = t.customer and c.active=1 left join notes n on n.id=t.note where t.total between 1 and 10 and (t.state='a' or t.state = 'b') group by t.id order by n desc, t.id limit 10 offset 20";
        assert_eq!(
            beautify(&MYSQL, sql).unwrap(),
            "SELECT
  t.id,
  COUNT(*) AS n,
  SUM(price * 2)
FROM
  shop.orders t
  INNER JOIN customers c ON c.id = t.customer
    AND c.active = 1
  LEFT JOIN notes n ON n.id = t.note
WHERE
  t.total BETWEEN 1 AND 10
  AND (t.state = 'a' OR t.state = 'b')
GROUP BY
  t.id
ORDER BY
  n DESC,
  t.id
LIMIT 10
OFFSET 20"
        );
    }

    #[test]
    fn subqueries_and_other_statements() {
        assert_eq!(
            beautify(&POSTGRES, "select * from a where id in (select id from b where x > -1);update a set x=1, y = 'it''s' where id=2;create table t (a int,\n  b text)").unwrap(),
            "SELECT
  *
FROM
  a
WHERE
  id IN (
    SELECT
      id
    FROM
      b
    WHERE
      x > -1
  );

UPDATE a
SET
  x = 1,
  y = 'it''s'
WHERE
  id = 2;

create table t (a int,\n  b text)"
        );
        assert_eq!(
            beautify(&SQLITE, "insert into t (a, b) values (1, 'x'), (2, 'y')").unwrap(),
            "INSERT INTO t (a, b)
VALUES
  (1, 'x'),
  (2, 'y')"
        );
    }

    #[test]
    fn literals_names_and_comments_are_kept_and_formatting_is_stable() {
        let samples: [(&Dialect, &str); 6] = [
            (
                &MYSQL,
                "SELECT `select`, 'a -- b', \"x\\\"y\" FROM `from` -- note\nWHERE a<>b AND c>=d # tail",
            ),
            (
                &POSTGRES,
                "select $tag$ ; select $tag$, E'\\'' ,x::int, @a, $1 from \"Order\" o /* keep ; me */ where o.\"from\"=:name",
            ),
            (
                &POSTGRES,
                "with a as (select 1 as x), b as (select 2) select * from a union all select * from b",
            ),
            (
                &SQLITE,
                "SELECT [order].[group] FROM [order] WHERE x = -1 OR y = +2",
            ),
            (
                &MYSQL,
                "SELECT a FROM t WHERE b LIKE _utf8mb4 0x41 ESCAPE '!' AND c IN (1,2, 3)",
            ),
            (&MYSQL, "DELETE FROM t WHERE id = 1;SELECT 1"),
        ];
        for (dialect, sql) in samples {
            let once = beautify(dialect, sql).unwrap();
            assert_eq!(tokens(dialect, sql), tokens(dialect, &once), "{once}");
            assert_eq!(
                beautify(dialect, &once).unwrap(),
                once,
                "not stable:\n{once}"
            );
        }
        // Names that are keywords keep their case where they stand as names.
        let named = beautify(&MYSQL, "select key from top as end where t.limit = 1").unwrap();
        assert!(named.contains("FROM\n  top AS end"), "{named}");
        assert!(named.contains("t.limit = 1"), "{named}");
        assert!(beautify(&MYSQL, "select 'open").is_err());
    }
}
