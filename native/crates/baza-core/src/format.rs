//! Beautify SQL: one clause per line, the items of a list one per line,
//! conditions under each other and subqueries indented, keywords in capital
//! letters. The text is read with the rules of the engine's quotes and
//! comments, so a literal, a quoted name or a comment is never changed;
//! only the white space between them and the case of keywords is. A
//! statement that is not a query (`CREATE TABLE`, …) is left as written.
//!
//! Formatting never changes what a script means or where it splits into
//! statements. Characters written together that the engine may read as one
//! token stay together (`<=>`, `1e-5`, `@id`), tokens written apart are never
//! joined into one (`- -1` does not become a comment), a `;` never ends up
//! inside a line comment, the separator lines of batch tools (`GO`, `/`) stay
//! on lines of their own, and a word that may be a name keeps its case.
use crate::engine::{Dialect, ParameterStyle};
use crate::sql::{Lexeme, lex};

/// Spaces of one level of indentation.
const INDENT: &str = "  ";

/// Keywords reserved by every engine in which the letter case of a name can
/// matter (MySQL for tables and their aliases, SQL Server with a
/// case-sensitive collation). Written bare, such a word can only be a
/// keyword, so it is written in capital letters unless it is part of a
/// qualified name or stands where a name does.
const RESERVED: &[&str] = &[
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
    "BETWEEN",
    "EXISTS",
    "AS",
    "ON",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "CROSS",
    "OUTER",
    "GROUP",
    "BY",
    "ORDER",
    "HAVING",
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
    "WITH",
    "ASC",
    "DESC",
    "STRAIGHT_JOIN",
];
/// Functions written in capital letters where they are called. Anywhere
/// else the word is a name (`SELECT min, max FROM t`) and keeps its case.
const FUNCTIONS: &[&str] = &[
    "COUNT", "SUM", "AVG", "MIN", "MAX", "CAST", "COALESCE", "NULLIF",
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
/// Characters that operators are made of. A run of them written together is
/// read as one operator, except where [`segments`] shows that every engine
/// reads it as several.
const OPERATOR_CHARS: &[u8] = b"=<>!+-*/%|&^~:@#?";
/// The operator characters that may come before a parameter character (see
/// [`markers`]) when the two are split: none of them is a parameter
/// character itself.
const PLAIN_OPERATOR_CHARS: &[u8] = b"=<>!+-*/%|&^~";
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
    /// Whether this token ends exactly where `next` starts.
    fn touches(&self, next: &Tok) -> bool {
        self.end == next.start
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
    let tokens = read(dialect, sql)?;
    let style = Style {
        keep_case: keeps_case(dialect),
        markers: markers(dialect),
    };
    let mut out = String::new();
    let mut current: Vec<Tok> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if let Some(next) = separator(sql, &tokens, i) {
            // The statement before a separator line ends there, and the
            // line follows it as it was written.
            push_statement(&mut out, sql, &current, false, style);
            current.clear();
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&sql[tokens[i].start..tokens[next - 1].end]);
            i = next;
            continue;
        }
        if tokens[i].is(";") {
            push_statement(&mut out, sql, &current, true, style);
            current.clear();
        } else {
            current.push(tokens[i]);
        }
        i += 1;
    }
    push_statement(&mut out, sql, &current, false, style);
    Ok(out)
}

/// The tokens of a text as the formatter reads them: without white space,
/// and with the tokens that must stay together joined (see [`merge`]).
fn read<'a>(dialect: &Dialect, sql: &'a str) -> anyhow::Result<Vec<Tok<'a>>> {
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
    Ok(merge(dialect, sql, tokens))
}

/// What the formatter needs to know about the engine.
#[derive(Clone, Copy, Default)]
struct Style {
    /// Words keep the case they were written in (see [`keeps_case`]).
    keep_case: bool,
    /// The characters that open a parameter (see [`markers`]).
    markers: &'static [u8],
}

/// Whether the engine tells the case of the words that look like keywords
/// apart. ClickHouse names its functions with letter case (`min` is not
/// `MIN` there) and keeps the case of every name, so no word may change it.
/// The dialect has no field that says so, and ClickHouse is the only engine
/// with this rule, so it is recognised by the dialect of its driver.
fn keeps_case(dialect: &Dialect) -> bool {
    *dialect == crate::drivers::clickhouse::DIALECT
}

/// Characters that open a parameter, a variable or a temporary table of the
/// engine and are never part of one of its operators: `:` everywhere (`:id`;
/// only `::` and `:=` hold it), `?` where it marks a parameter, `@` where a
/// variable starts with it and no operator holds it (SQL Server, and the
/// engines that quote names with backticks: MySQL, SQLite, ClickHouse), and
/// `#` of a temporary table in SQL Server. One written right after an
/// operator (`id=?`, `id=@id`) starts the parameter, and the token after it
/// stays where it was written (`@id`, `:id`, `?1`, `@@rowcount`).
fn markers(dialect: &Dialect) -> &'static [u8] {
    match (dialect.parameters, dialect.lexer.backtick_names) {
        (ParameterStyle::AtNamed, _) => b":@#",
        (ParameterStyle::Question, true) => b":?@",
        (ParameterStyle::Question, false) => b":?",
        (_, true) => b":@",
        (_, false) => b":",
    }
}

/// Add one statement to the formatted text: a query formatted, any other as
/// it was written, followed by its `;` when it had one.
fn push_statement(out: &mut String, sql: &str, statement: &[Tok], ended: bool, style: Style) {
    let (Some(first), Some(last)) = (statement.first(), statement.last()) else {
        return;
    };
    let opening = statement
        .iter()
        .find(|t| t.kind == Lexeme::Word)
        .map(Tok::upper)
        .unwrap_or_default();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    if QUERIES.contains(&opening.as_str()) {
        out.push_str(&Formatter::new(style).statement(statement));
    } else {
        out.push_str(&sql[first.start..last.end]);
    }
    if ended {
        // A `;` on the line of a line comment would be part of the comment.
        if last.kind == Lexeme::Comment && !last.text.starts_with("/*") {
            out.push('\n');
        }
        out.push(';');
    }
}

/// When the token at `i` opens a line that only separates the batches of a
/// script, the index of the first token after that line. Such a line holds
/// `GO`, optionally with a count and a `--` comment, as the tools of SQL
/// Server read it, or a lone `/`, as SQL*Plus reads it. The line is copied
/// as it was written and never joined with a statement: for SQL Server and
/// Oracle it separates the batches of the script, and for any other engine a
/// statement that continues across it reads the same, since only the white
/// space around it changes. A script opened without a connection is read
/// with the standard dialect, so every engine treats such lines alike.
fn separator(sql: &str, tokens: &[Tok], i: usize) -> Option<usize> {
    let t = tokens[i];
    let line_start = sql[..t.start].rfind('\n').map_or(0, |n| n + 1);
    if !sql[line_start..t.start].trim().is_empty() {
        return None;
    }
    let line_end = sql[t.end..].find('\n').map_or(sql.len(), |n| t.end + n);
    let mut next = i + 1;
    if t.word("GO") {
        let mut counted = false;
        while let Some(f) = tokens.get(next).filter(|f| f.start < line_end) {
            if !counted && f.kind == Lexeme::Word && f.text.bytes().all(|b| b.is_ascii_digit()) {
                counted = true;
            } else if !(f.kind == Lexeme::Comment && f.text.starts_with("--")) {
                return None;
            }
            next += 1;
        }
    } else if !t.is("/") {
        return None;
    }
    tokens
        .get(next)
        .is_none_or(|f| f.start >= line_end)
        .then_some(next)
}

/// Join the tokens that the formatter must keep together: a number written
/// across several tokens (`1e-5`, `1.5E+10`, `.5e-3`), a string or a name
/// with Unicode escapes (`U&'…'`), and a run of operator characters written
/// together, split only where [`segments`] allows.
fn merge<'a>(dialect: &Dialect, sql: &'a str, tokens: Vec<Tok<'a>>) -> Vec<Tok<'a>> {
    let markers = markers(dialect);
    let span = |from: &Tok, to: &Tok, kind| Tok {
        kind,
        text: &sql[from.start..to.end],
        start: from.start,
        end: to.end,
    };
    let operator_char = |t: &Tok| {
        t.kind == Lexeme::Symbol
            && t.text.len() == 1
            && OPERATOR_CHARS.contains(&t.text.as_bytes()[0])
    };
    let mut out = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i];
        if let Some(last) = number_end(sql, &tokens, i) {
            out.push(span(&t, &tokens[last], Lexeme::Word));
            i = last + 1;
            continue;
        }
        if t.word("U")
            && let (Some(amp), Some(quoted)) = (tokens.get(i + 1), tokens.get(i + 2))
            && amp.is("&")
            && t.touches(amp)
            && amp.touches(quoted)
            && matches!(quoted.kind, Lexeme::Literal | Lexeme::Quoted)
        {
            out.push(span(&t, quoted, quoted.kind));
            i += 3;
            continue;
        }
        if operator_char(&t) {
            let mut last = i;
            while tokens
                .get(last + 1)
                .is_some_and(|n| operator_char(n) && tokens[last].touches(n))
            {
                last += 1;
            }
            let run = &sql[t.start..tokens[last].end];
            for part in segments(run, markers) {
                out.push(Tok {
                    kind: Lexeme::Symbol,
                    text: &run[part.clone()],
                    start: t.start + part.start,
                    end: t.start + part.end,
                });
            }
            i = last + 1;
            continue;
        }
        out.push(t);
        i += 1;
    }
    out
}

/// The last token of a number that starts at `i` and that the lexer read as
/// several tokens: `1e-5` is `1e`, `-` and `5` there. A number starts with a
/// digit, or with a `.` before a digit that does not follow a name (`t.5`).
fn number_end(sql: &str, tokens: &[Tok], i: usize) -> Option<usize> {
    let t = tokens[i];
    let before = i
        .checked_sub(1)
        .map(|j| tokens[j])
        .filter(|b| b.touches(&t));
    let starts = match t.kind {
        Lexeme::Word => t.text.as_bytes()[0].is_ascii_digit() && !before.is_some_and(|b| b.is(".")),
        Lexeme::Symbol => {
            t.text == "."
                && !before.is_some_and(|b| {
                    matches!(b.kind, Lexeme::Word | Lexeme::Quoted) || b.is(")") || b.is("]")
                })
        }
        _ => false,
    };
    if !starts {
        return None;
    }
    let end = t.start + number_length(&sql.as_bytes()[t.start..]);
    let mut last = i;
    while tokens[last].end < end {
        last += 1;
        if tokens
            .get(last)
            .is_none_or(|n| !tokens[last - 1].touches(n))
        {
            return None;
        }
    }
    (last > i && tokens[last].end == end).then_some(last)
}

/// Length of the number at the start of `b`: digits with an optional
/// fraction and an optional exponent (`12`, `1.`, `.5`, `1.5E+10`), or 0.
fn number_length(b: &[u8]) -> usize {
    let digits = |from: usize| {
        b[from.min(b.len())..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count()
    };
    let whole = digits(0);
    let mut at = whole;
    if b.get(at) == Some(&b'.') {
        let fraction = digits(at + 1);
        if whole + fraction == 0 {
            return 0;
        }
        at += 1 + fraction;
    } else if whole == 0 {
        return 0;
    }
    if matches!(b.get(at), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(b.get(at + 1), Some(b'+' | b'-')));
        let exponent = digits(at + 1 + sign);
        if exponent > 0 {
            at += 1 + sign + exponent;
        }
    }
    at
}

/// Where a run of operator characters written together splits into
/// operators. It stays whole unless every engine reads the split the same:
/// a parameter character after an operator (`=?`, `<>@id`, `=:id`; see
/// [`markers`]), and the signs at the end (see [`signs`]). So `<=>`, `==`,
/// `!~`, `<->`, `?|`, `#-` and `+=` stay one operator each.
fn segments(run: &str, markers: &[u8]) -> Vec<std::ops::Range<usize>> {
    let b = run.as_bytes();
    let body = b
        .iter()
        .rposition(|c| !markers.contains(c))
        .map_or(0, |p| p + 1);
    if body > 0 && body < b.len() && b[..body].iter().all(|c| PLAIN_OPERATOR_CHARS.contains(c)) {
        let mut out = signs(&b[..body]);
        out.push(body..b.len());
        return out;
    }
    signs(b)
}

/// Split the `+` and `-` signs off the end of an operator the way
/// PostgreSQL does, whose rule is the strictest: only when the operator
/// holds none of `~ ! @ # % ^ & | ?`, since only then can it not be an
/// operator of its own. So `=-1` is `=` and `-1`, `*-1` is `*` and `-1`,
/// and `--` in MySQL (where it does not start a comment before a digit) is
/// two signs; `#-` and `@-@` stay whole.
fn signs(b: &[u8]) -> Vec<std::ops::Range<usize>> {
    let body = if b.iter().any(|c| b"~!@#%^&|?".contains(c)) {
        b.len()
    } else {
        b.iter()
            .rposition(|c| !matches!(c, b'+' | b'-'))
            .map_or(0, |p| p + 1)
    };
    let mut out: Vec<_> = (body > 0).then_some(0..body).into_iter().collect();
    out.extend((body..b.len()).map(|k| k..k + 1));
    out
}

/// Whether a symbol is an operator, written with a space on both sides. A
/// lone `@`, `:`, `#` or `?` opens a name or a parameter instead.
fn operator(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|c| OPERATOR_CHARS.contains(&c))
        && !matches!(s, "@" | ":" | "#" | "?")
}

/// The last token written.
#[derive(Clone, Debug)]
struct Written {
    kind: Lexeme,
    /// As written, in the case it was written in.
    text: String,
    /// Where it ends in the source.
    end: usize,
    /// A sign that belongs to the value after it.
    unary: bool,
    /// A keyword where it stands.
    keyword: bool,
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
    previous: Option<Written>,
    /// The statement starts with this word.
    kind: String,
    style: Style,
    /// `CASE` expressions open here, each waiting for its `END`.
    cases: usize,
}

impl Formatter {
    fn new(style: Style) -> Self {
        Formatter {
            style,
            ..Formatter::default()
        }
    }
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
                    // The rest of the line belongs to the comment. Before the
                    // first clause, the next line is where a clause starts.
                    let level = match (structured, self.top().clause) {
                        (true, Clause::None) => self.top().base,
                        (true, _) => self.content(),
                        (false, _) => self.top().base + 1,
                    };
                    self.newline(level);
                }
                return 1;
            }
            Lexeme::Symbol if t.text == "(" => {
                let sub = next.is_some_and(|n| n.word("SELECT") || n.word("WITH"));
                self.emit(t, "(", false);
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
                self.emit(t, ")", false);
                return 1;
            }
            Lexeme::Symbol if t.text == "," => {
                self.emit(t, ",", false);
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
            let (text, keyword) = self.spelling(tokens, i);
            self.emit(t, &text, keyword);
            return 1;
        }
        let upper = t.upper();
        let previous_word = self.previous_word();
        // Two words that make one clause keyword.
        if matches!(upper.as_str(), "GROUP" | "ORDER") && next.is_some_and(|n| n.word("BY")) {
            let (first, keyword) = self.spelling(tokens, i);
            let (by, _) = self.spelling(tokens, i + 1);
            let both = Tok {
                end: tokens[i + 1].end,
                ..t
            };
            let clause = if upper == "GROUP" {
                Clause::Group
            } else {
                Clause::Order
            };
            self.block(both, &format!("{first} {by}"), keyword, clause);
            return 2;
        }
        let (text, keyword) = self.spelling(tokens, i);
        match upper.as_str() {
            "SELECT" => self.block(t, &text, keyword, Clause::Select),
            "FROM" if previous_word == "DELETE" => {
                self.emit(t, &text, keyword);
                self.top_mut().clause = Clause::From;
            }
            "FROM" => self.block(t, &text, keyword, Clause::From),
            "WHERE" => self.block(t, &text, keyword, Clause::Where),
            "HAVING" => self.block(t, &text, keyword, Clause::Having),
            "SET" if self.kind == "UPDATE" => self.block(t, &text, keyword, Clause::Set),
            // The rows of an `INSERT`, not `VALUES(a)` of `ON DUPLICATE KEY`.
            "VALUES"
                if (self.kind == "INSERT" && self.top().clause == Clause::Other)
                    || (self.kind == "VALUES" && self.top().clause == Clause::None) =>
            {
                self.block(t, &text, keyword, Clause::Values)
            }
            "LIMIT" | "OFFSET" | "FETCH" | "RETURNING" if keyword => {
                self.line(t, &text, keyword);
                self.top_mut().clause = Clause::Limit;
            }
            "UNION" | "INTERSECT" | "EXCEPT" => {
                self.line(t, &text, keyword);
                self.top_mut().clause = Clause::None;
            }
            "WITH" => {
                self.emit(t, &text, keyword);
                self.top_mut().clause = Clause::With;
            }
            "INSERT" | "UPDATE" | "DELETE"
                if self.out.trim().is_empty() || self.top().clause == Clause::With =>
            {
                self.line(t, &text, keyword);
                self.top_mut().clause = Clause::Other;
            }
            "DISTINCT" | "ALL" if self.pending && self.top().clause == Clause::Select => {
                // `SELECT DISTINCT` stays on the line of its keyword.
                self.pending = false;
                self.emit(t, &text, keyword);
                self.pending = true;
            }
            word if JOINS.contains(&word)
                && keyword
                && self.top().clause == Clause::From
                && !JOIN_PREFIX.contains(&previous_word.as_str()) =>
            {
                let level = self.content();
                self.pending = false;
                self.newline(level);
                self.emit(t, &text, keyword);
            }
            "AND" if self.between => {
                self.between = false;
                self.emit(t, &text, keyword);
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
                self.emit(t, &text, keyword);
            }
            "BETWEEN" => {
                self.between = true;
                self.emit(t, &text, keyword);
            }
            _ => self.emit(t, &text, keyword),
        }
        1
    }

    /// The last word written, in capitals.
    fn previous_word(&self) -> String {
        match &self.previous {
            Some(p) if p.kind == Lexeme::Word => p.text.to_ascii_uppercase(),
            _ => String::new(),
        }
    }

    /// A clause keyword on a line of its own, its content on the next ones.
    fn block(&mut self, t: Tok, text: &str, keyword: bool, clause: Clause) {
        self.pending = false;
        let base = self.top().base;
        if !self.line_start {
            self.newline(base);
        }
        self.emit(t, text, keyword);
        self.top_mut().clause = clause;
        self.pending = true;
    }

    /// A keyword that starts a line, its content after it.
    fn line(&mut self, t: Tok, text: &str, keyword: bool) {
        self.pending = false;
        let base = self.top().base;
        if !self.line_start {
            self.newline(base);
        }
        self.emit(t, text, keyword);
    }

    /// The token at `i` as it is written, and whether it is a keyword there:
    /// a keyword in capital letters, unless the engine keeps the case of
    /// every word; anything else as it was typed.
    fn spelling(&mut self, tokens: &[Tok], i: usize) -> (String, bool) {
        let t = tokens[i];
        if !self.is_keyword(tokens, i) {
            return (t.text.to_owned(), false);
        }
        let upper = t.upper();
        match upper.as_str() {
            "CASE" => self.cases += 1,
            "END" => self.cases = self.cases.saturating_sub(1),
            _ => {}
        }
        let text = if self.style.keep_case {
            t.text.to_owned()
        } else {
            upper
        };
        (text, true)
    }

    /// Whether the word at `i` is a keyword where it stands. A word that
    /// every engine reserves is one unless it is part of a qualified name or
    /// stands where a name does; any other word is one only where the words
    /// around it show it (`max(`, `FETCH NEXT`, `ON DUPLICATE KEY`). Anything
    /// else may be a name, and a name keeps the case it was written in.
    fn is_keyword(&self, tokens: &[Tok], i: usize) -> bool {
        let t = tokens[i];
        if t.kind != Lexeme::Word {
            return false;
        }
        let before = i.checked_sub(1).map(|j| tokens[j]);
        let after = tokens.get(i + 1).copied();
        // A word next to a dot is part of a name: `t.limit`, `next.id`.
        if before.is_some_and(|b| b.is(".")) || after.is_some_and(|a| a.is(".")) {
            return false;
        }
        let word_before = |words: &[&str]| before.is_some_and(|b| words.iter().any(|w| b.word(w)));
        let word_after = |words: &[&str]| after.is_some_and(|a| words.iter().any(|w| a.word(w)));
        let call = after.is_some_and(|a| a.is("("));
        // A count or a parameter: `LIMIT 10`, `OFFSET ?`, `TOP @n`.
        let amount = |x: Option<Tok>| {
            x.is_some_and(|x| {
                (x.kind == Lexeme::Word && x.text.as_bytes()[0].is_ascii_digit())
                    || (x.kind == Lexeme::Symbol && matches!(x.text, "?" | "$" | ":" | "@"))
            })
        };
        let literal = after.is_some_and(|a| a.kind == Lexeme::Literal);
        let upper = t.upper();
        let upper = upper.as_str();
        if RESERVED.contains(&upper) {
            // Where a name stands, as in `SELECT … AS end` or `FROM top`.
            let named = word_before(&["FROM", "JOIN", "INTO", "TABLE", "AS"])
                || (upper != "SET" && word_before(&["UPDATE"]));
            return !named;
        }
        if FUNCTIONS.contains(&upper) {
            return call;
        }
        match upper {
            "OVER" | "USING" | "ANY" | "SOME" => call,
            "END" => self.cases > 0,
            "FULL" => word_after(&["JOIN", "OUTER"]),
            "NATURAL" => word_after(&["JOIN", "LEFT", "RIGHT", "FULL", "INNER"]),
            "PARTITION" => word_after(&["BY"]),
            "RECURSIVE" => word_before(&["WITH"]),
            "FETCH" => word_after(&["NEXT", "FIRST"]),
            "NEXT" | "FIRST" => word_before(&["FETCH"]),
            "ROW" | "ROWS" => {
                word_after(&["ONLY", "BETWEEN", "FETCH"])
                    || (amount(before)
                        && i >= 2
                        && ["OFFSET", "NEXT", "FIRST"]
                            .iter()
                            .any(|w| tokens[i - 2].word(w)))
            }
            "ONLY" => word_before(&["ROW", "ROWS"]),
            "LIMIT" | "OFFSET" => amount(after) || call || word_after(&["ALL"]),
            "TOP" => word_before(&["SELECT", "DISTINCT", "ALL"]) && (amount(after) || call),
            "DUPLICATE" => word_before(&["ON"]) && word_after(&["KEY"]),
            "KEY" => word_before(&["DUPLICATE"]) && word_after(&["UPDATE"]),
            "INTERVAL" => literal || amount(after),
            "ESCAPE" => literal,
            "ILIKE" => literal || amount(after),
            "TRUE" | "FALSE" => {
                before.is_some_and(|b| b.is("=") || b.is("<>") || b.is("!="))
                    || word_before(&["IS", "NOT", "THEN", "ELSE"])
            }
            "RETURNING" => matches!(self.kind.as_str(), "INSERT" | "UPDATE" | "DELETE"),
            _ => false,
        }
    }

    /// Write a token with the space the tokens around it need.
    fn emit(&mut self, t: Tok, text: &str, keyword: bool) {
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
        let unary = matches!(text, "-" | "+")
            && match &self.previous {
                None => true,
                Some(p) => match p.kind {
                    Lexeme::Symbol => !matches!(p.text.as_str(), ")" | "]" | "}"),
                    Lexeme::Word => p.keyword && !p.text.eq_ignore_ascii_case("END"),
                    _ => false,
                },
            };
        self.previous = Some(Written {
            kind: t.kind,
            text: text.to_owned(),
            end: t.end,
            unary,
            keyword,
        });
    }

    /// Whether a space goes before the next token: where the layout asks
    /// for one and splitting is safe, and wherever leaving it out would join
    /// two tokens into one.
    fn space_before(&self, t: Tok, text: &str) -> bool {
        let Some(previous) = &self.previous else {
            return false;
        };
        let touched = previous.end == t.start;
        let wanted = wanted(previous, t, text);
        (wanted && (!touched || self.splittable(previous, t, text))) || self.joins(text, touched)
    }

    /// Whether a space may go between two tokens written together: next to a
    /// bracket or a comma, or between an operator and what it works on, but
    /// never after a character that opens a parameter or a variable (`@id`,
    /// `:id`, `?1`, `@@rowcount`).
    fn splittable(&self, previous: &Written, t: Tok, text: &str) -> bool {
        if previous.kind == Lexeme::Symbol
            && previous
                .text
                .bytes()
                .last()
                .is_some_and(|c| self.style.markers.contains(&c))
        {
            return false;
        }
        let punctuation = |s: &str| matches!(s, "(" | ")" | "," | ";");
        let symbol_operator = |kind: Lexeme, s: &str| kind == Lexeme::Symbol && operator(s);
        (previous.kind == Lexeme::Symbol && punctuation(&previous.text))
            || (t.kind == Lexeme::Symbol && punctuation(text))
            || symbol_operator(previous.kind, &previous.text)
            || symbol_operator(t.kind, text)
    }

    /// Whether `text`, written right after what is already written, could
    /// be read together with it as something else: two dashes as a comment,
    /// `/` and `*` as the start of one, and, for tokens that stood apart in
    /// the source, two operators as one (`< =`), two words as one, or a
    /// number and a dot as a decimal number (`1 .5`).
    fn joins(&self, text: &str, touched: bool) -> bool {
        let (Some(a), Some(b)) = (self.out.bytes().last(), text.bytes().next()) else {
            return false;
        };
        if (a == b'-' && b == b'-') || (a == b'/' && b == b'*') {
            return true;
        }
        if touched {
            return false;
        }
        let op = |c: u8| OPERATOR_CHARS.contains(&c);
        let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
        (op(a) && op(b))
            || (word(a) && word(b))
            || (a.is_ascii_digit() && b == b'.')
            || (a == b'.' && b.is_ascii_digit())
    }
}

/// The space the layout asks for between the last token written and the
/// next one, before the source has its say.
fn wanted(previous: &Written, t: Tok, text: &str) -> bool {
    if matches!(text, "," | ")" | "." | "::") || matches!(previous.text.as_str(), "(" | "." | "::")
    {
        return false;
    }
    if previous.unary {
        return false;
    }
    if text == "(" {
        // A function is called without a space; a keyword list has one.
        // After any other name the bracket stays as written: `count(`
        // is a call, `INTO t (a, b)` a list of columns.
        return match previous.kind {
            Lexeme::Word if previous.keyword => {
                !CALLS.contains(&previous.text.to_ascii_uppercase().as_str())
            }
            Lexeme::Word | Lexeme::Quoted => t.start != previous.end,
            _ => true,
        };
    }
    if (t.kind == Lexeme::Symbol && operator(text))
        || (previous.kind == Lexeme::Symbol && operator(&previous.text))
    {
        return true;
    }
    // Other neighbours stay as close as they were written: `@var`,
    // `$1`, `:name`, `N'text'`.
    t.start != previous.end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::{
        clickhouse::DIALECT as CLICKHOUSE, mssql::DIALECT as MSSQL, mysql::DIALECT as MYSQL,
        oracle::DIALECT as ORACLE, postgres::DIALECT as POSTGRES, sqlite::DIALECT as SQLITE,
    };
    use crate::sql::statement_ranges;

    /// Words that are keywords only in some places.
    const CONTEXTUAL: &[&str] = &[
        "OVER",
        "USING",
        "ANY",
        "SOME",
        "END",
        "FULL",
        "NATURAL",
        "PARTITION",
        "RECURSIVE",
        "FETCH",
        "NEXT",
        "FIRST",
        "ROW",
        "ROWS",
        "ONLY",
        "LIMIT",
        "OFFSET",
        "TOP",
        "DUPLICATE",
        "KEY",
        "INTERVAL",
        "ESCAPE",
        "ILIKE",
        "TRUE",
        "FALSE",
        "RETURNING",
    ];

    /// The separator lines of a script, as written.
    fn separators(dialect: &Dialect, sql: &str) -> Vec<String> {
        let tokens = read(dialect, sql).unwrap();
        (0..tokens.len())
            .filter_map(|i| {
                separator(sql, &tokens, i)
                    .map(|next| sql[tokens[i].start..tokens[next - 1].end].to_owned())
            })
            .collect()
    }

    /// Format `sql` and check what formatting may never do; returns the
    /// formatted text.
    fn checked(dialect: &Dialect, sql: &str) -> String {
        let once = beautify(dialect, sql).unwrap();
        // The same tokens, operators and numbers whole: only a keyword may
        // change, to capital letters, and on ClickHouse not even that.
        let (before, after) = (read(dialect, sql).unwrap(), read(dialect, &once).unwrap());
        let texts = |tokens: &[Tok]| tokens.iter().map(|t| t.text.to_owned()).collect::<Vec<_>>();
        assert_eq!(
            before.len(),
            after.len(),
            "{:?}\n{:?}\n{once}",
            texts(&before),
            texts(&after)
        );
        for (a, b) in before.iter().zip(&after) {
            assert_eq!(a.kind, b.kind, "`{}` / `{}`:\n{once}", a.text, b.text);
            if a.text != b.text {
                let upper = a.text.to_ascii_uppercase();
                assert!(
                    a.kind == Lexeme::Word
                        && b.text == upper
                        && !keeps_case(dialect)
                        && [RESERVED, FUNCTIONS, CONTEXTUAL]
                            .iter()
                            .any(|list| list.contains(&upper.as_str())),
                    "`{}` became `{}`:\n{once}",
                    a.text,
                    b.text
                );
            }
        }
        // The same statements and the same batch separators.
        assert_eq!(
            statement_ranges(dialect, sql).unwrap().len(),
            statement_ranges(dialect, &once).unwrap().len(),
            "statements:\n{once}"
        );
        assert_eq!(
            separators(dialect, sql),
            separators(dialect, &once),
            "separators:\n{once}"
        );
        // Formatting again changes nothing.
        assert_eq!(
            beautify(dialect, &once).unwrap(),
            once,
            "not stable:\n{once}"
        );
        once
    }

    #[test]
    fn a_query_gets_a_clause_per_line() {
        let sql = "select t.id, count(*) as n, sum(price*2) from shop.orders t inner join customers c on c.id = t.customer and c.active=1 left join notes n on n.id=t.note where t.total between 1 and 10 and (t.state='a' or t.state = 'b') group by t.id order by n desc, t.id limit 10 offset 20";
        assert_eq!(
            checked(&MYSQL, sql),
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
            checked(
                &POSTGRES,
                "select * from a where id in (select id from b where x > -1);update a set x=1, y = 'it''s' where id=2;create table t (a int,\n  b text)"
            ),
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
            checked(&SQLITE, "insert into t (a, b) values (1, 'x'), (2, 'y')"),
            "INSERT INTO t (a, b)
VALUES
  (1, 'x'),
  (2, 'y')"
        );
    }

    #[test]
    fn literals_names_and_comments_are_kept_and_formatting_is_stable() {
        let samples: [(&Dialect, &str); 10] = [
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
            (
                &POSTGRES,
                "-- header\nselect a, -- first\n b from t where x in (select y from u -- inner\n) and z = U&'d\\0061t'",
            ),
            (
                &MSSQL,
                "select top (10) * from t where a = @a and b in (@b,@c) order by a offset @skip rows fetch next @take rows only",
            ),
            (
                &POSTGRES,
                "select count(/* all */*), sum(x)/*s*/-1, a.b -1 from t",
            ),
            (
                &ORACLE,
                "select case when a = 1 then -1 else 2 end as end from dual where b = :b",
            ),
        ];
        for (dialect, sql) in samples {
            checked(dialect, sql);
        }
        // Names that are keywords keep their case where they stand as names.
        let named = checked(&MYSQL, "select key from top as end where t.limit = 1");
        assert!(named.contains("FROM\n  top AS end"), "{named}");
        assert!(named.contains("t.limit = 1"), "{named}");
        assert!(beautify(&MYSQL, "select 'open").is_err());
    }

    #[test]
    fn operators_written_together_stay_together() {
        let out = checked(
            &MYSQL,
            "select a<=>b, c <=> d, e&&f, @n:=1 from t where x=-1 and y=? and z<>-2 and w=@v and q<@v",
        );
        for part in [
            "a <=> b,", "c <=> d,", "e && f", "@n := 1", "x = -1", "y = ?", "z <> -2", "w = @v",
            "q < @v",
        ] {
            assert!(out.contains(part), "{part}:\n{out}");
        }
        let out = checked(&SQLITE, "select * from t where a==1 and b=?1 and c=:c");
        for part in ["a == 1", "b = ?1", "c = :c"] {
            assert!(out.contains(part), "{part}:\n{out}");
        }
        let out = checked(
            &POSTGRES,
            "select a~*'x', b!~'y', p<->q, n<<=m, j?|array['a'], j#-'{a}', j@>k, k<@j, x!=-1, x::int from t",
        );
        for part in [
            "a ~* 'x'",
            "b !~ 'y'",
            "p <-> q",
            "n <<= m",
            "j ?| array['a']",
            "j #- '{a}'",
            "j @> k",
            "k <@ j",
            "x !=- 1",
            "x::int",
        ] {
            assert!(out.contains(part), "{part}:\n{out}");
        }
        let out = checked(
            &MSSQL,
            "update t set n+=1, @x=@@rowcount where a!<b and c<@p and d=#t.x",
        );
        for part in ["n += 1", "@x = @@rowcount", "a !< b", "c < @p"] {
            assert!(out.contains(part), "{part}:\n{out}");
        }
    }

    #[test]
    fn signs_never_make_a_comment() {
        assert_eq!(
            checked(&POSTGRES, "SELECT - -1 AS x, 2 AS y"),
            "SELECT\n  - -1 AS x,\n  2 AS y"
        );
        // In MySQL `--` before a digit is two minus signs, not a comment.
        let out = checked(&MYSQL, "SELECT 1--1 AS x, + +2, -+3 FROM t");
        assert!(out.contains("1 - -1 AS x"), "{out}");
        assert!(out.contains("+ +2") && out.contains("-+3"), "{out}");
        let out = checked(&POSTGRES, "select count(/* c */*), 2 / * from t");
        assert!(!out.contains("/**") && !out.contains("*/*"), "{out}");
    }

    #[test]
    fn numbers_stay_whole() {
        let out = checked(
            &MYSQL,
            "select 1e-5, 1.5E+10, .5e-3, 2e+3-1, 1.e-5, a .5, 0x1e-5 from t",
        );
        for part in [
            "1e-5,",
            "1.5E+10,",
            ".5e-3,",
            "2e+3 - 1,",
            "1.e-5,",
            "a .5,",
            "0x1e - 5",
        ] {
            assert!(out.contains(part), "{part}:\n{out}");
        }
        checked(&POSTGRES, "select x*1e-5, t.a-1.5e-3 from t");
    }

    #[test]
    fn a_semicolon_never_lands_in_a_line_comment() {
        let out = checked(&POSTGRES, "UPDATE t SET a=1 -- why\n;\nDELETE FROM t");
        assert!(out.contains("-- why\n;"), "{out}");
        assert_eq!(
            checked(&POSTGRES, "-- header\nselect 1;"),
            "-- header\nSELECT\n  1;"
        );
        let out = checked(
            &MYSQL,
            "update t set a=1 # why\n;create table x (a int) -- note\n;select 1",
        );
        assert!(
            out.contains("# why\n;") && out.contains("-- note\n;"),
            "{out}"
        );
    }

    #[test]
    fn batch_separators_stay_on_lines_of_their_own() {
        assert_eq!(
            checked(&MSSQL, "select 1\nGO\nselect 2;\ngo 3 -- again\nselect 3"),
            "SELECT\n  1\nGO\n\nSELECT\n  2;\ngo 3 -- again\n\nSELECT\n  3"
        );
        // `GO` with anything else on its line is a name.
        checked(&MSSQL, "select 1 go\nselect 2 GO, 3");
        assert_eq!(
            checked(
                &ORACLE,
                "select 1 from dual\n/\nbegin\n  null;\nend;\n/\nselect 2 from dual"
            ),
            "SELECT\n  1\nFROM\n  dual\n/\n\nbegin\n  null;\n\nend;\n/\n\nSELECT\n  2\nFROM\n  dual"
        );
        // A script opened without a connection may be for either.
        checked(&Dialect::ANSI, "select 1\nGO\nselect 4\n/\nselect 5 / 2");
    }

    #[test]
    fn names_keep_their_case() {
        // ClickHouse tells the case of functions and names apart.
        assert_eq!(
            checked(
                &CLICKHOUSE,
                "select name, min, max, count() from system.settings where name like 'max%'"
            ),
            "select\n  name,\n  min,\n  max,\n  count()\nfrom\n  system.settings\nwhere\n  name like 'max%'"
        );
        // MySQL tells the case of table aliases apart.
        let out = checked(
            &MYSQL,
            "select next.id, min, max(x) from orders next join items full on full.id = next.id",
        );
        assert!(
            out.contains("orders next") && out.contains("next.id"),
            "{out}"
        );
        assert!(out.contains("items full ON full.id"), "{out}");
        assert!(out.contains("  min,\n  MAX(x)"), "{out}");
        let out = checked(
            &POSTGRES,
            "select a from t order by a offset 5 rows fetch next 10 rows only",
        );
        assert!(
            out.contains("OFFSET 5 ROWS") && out.contains("FETCH NEXT 10 ROWS ONLY"),
            "{out}"
        );
        let out = checked(
            &MYSQL,
            "insert into t (a) values (1) on duplicate key update a = values(a), b = true",
        );
        assert!(
            out.contains("ON DUPLICATE KEY UPDATE a = VALUES(a),") && out.contains("b = TRUE"),
            "{out}"
        );
    }
}
