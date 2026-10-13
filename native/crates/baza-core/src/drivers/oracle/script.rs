//! SQL text the way Oracle reads it: where a script is cut into the
//! statements that are sent one by one, and how values are written.
//!
//! A script is read like SQL*Plus and SQL Developer read one:
//!
//! * a `;` ends an SQL statement and is not sent, because Oracle refuses it;
//! * a PL/SQL block (`BEGIN`, `DECLARE`) contains `;` and ends with the `END`
//!   that closes it, or earlier at a line that holds only `/`;
//! * a stored unit (`CREATE PROCEDURE`, `FUNCTION`, `PACKAGE`, `TRIGGER`,
//!   `TYPE`, …) runs to a line that holds only `/`, or to the end of the text;
//! * `'…'` doubles its quote, `q'[…]'` ends at its own bracket, `"…"` is a
//!   name, `--` and `/* … */` are comments.
//!
//! Values are written for Oracle's rules: an empty text is `NULL`, a text
//! literal may not be longer than 4000 bytes, a `CLOB` is not compared with
//! `=`, and a binary floating point number needs its suffix.
use super::{PageRequest, RowChange};
use crate::builder::{Filter, FilterOp};
use crate::model::{Cell, TableRef};
use crate::sql::{LIKE_ESCAPE, MAX_SQL_BYTES, like_pattern, qualified, quote_identifier};
use anyhow::{Result, anyhow, ensure};

/// What a piece of a script is, which decides how it is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// One SQL statement, without its `;`.
    Sql,
    /// An anonymous PL/SQL block, with the `;` after its `END`.
    Block,
    /// `CREATE` of a stored PL/SQL unit, as it was written.
    Unit,
}

/// One statement of a script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Step {
    pub(super) text: String,
    pub(super) kind: Kind,
}

/// A lexical element of Oracle SQL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token {
    Space,
    Comment,
    Literal,
    Quoted,
    Word,
    Symbol,
}

fn word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'#') || byte >= 0x80
}

/// End of the `q'[…]'` literal whose quote is at `quote`: the index after
/// its closing quote. The bracket that opens it decides what closes it.
fn alternative_end(bytes: &[u8], quote: usize) -> Option<usize> {
    let open = *bytes.get(quote + 1)?;
    let close = match open {
        b'[' => b']',
        b'{' => b'}',
        b'<' => b'>',
        b'(' => b')',
        other => other,
    };
    let mut index = quote + 2;
    while index + 1 < bytes.len() {
        if bytes[index] == close && bytes[index + 1] == b'\'' {
            return Some(index + 2);
        }
        index += 1;
    }
    None
}

/// Read `text` element by element. Fails with `sql_unterminated_literal`
/// when a literal, a quoted name or a comment never ends.
fn lex(text: &str, mut each: impl FnMut(Token, std::ops::Range<usize>)) -> Result<()> {
    let bytes = text.as_bytes();
    let unterminated = || anyhow!("sql_unterminated_literal");
    let mut index = 0;
    while index < bytes.len() {
        let start = index;
        let byte = bytes[index];
        let token = if byte.is_ascii_whitespace() {
            while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                index += 1;
            }
            Token::Space
        } else if bytes[index..].starts_with(b"--") {
            while bytes.get(index).is_some_and(|byte| *byte != b'\n') {
                index += 1;
            }
            Token::Comment
        } else if bytes[index..].starts_with(b"/*") {
            let length = bytes[index + 2..]
                .windows(2)
                .position(|window| window == b"*/")
                .ok_or_else(unterminated)?;
            index += length + 4;
            Token::Comment
        } else if byte == b'\'' {
            index = plain_end(bytes, index, b'\'').ok_or_else(unterminated)?;
            Token::Literal
        } else if byte == b'"' {
            index = plain_end(bytes, index, b'"').ok_or_else(unterminated)?;
            Token::Quoted
        } else if word_byte(byte) {
            while bytes.get(index).is_some_and(|byte| word_byte(*byte)) {
                index += 1;
            }
            // `q'…'`, `Q'…'` and their national forms `nq'…'`.
            let word = &text[start..index];
            let alternative = word.eq_ignore_ascii_case("q") || word.eq_ignore_ascii_case("nq");
            let national = word.eq_ignore_ascii_case("n");
            if bytes.get(index) == Some(&b'\'') && alternative {
                index = alternative_end(bytes, index).ok_or_else(unterminated)?;
                Token::Literal
            } else if bytes.get(index) == Some(&b'\'') && national {
                index = plain_end(bytes, index, b'\'').ok_or_else(unterminated)?;
                Token::Literal
            } else {
                Token::Word
            }
        } else {
            index += 1;
            Token::Symbol
        };
        each(token, start..index);
    }
    Ok(())
}

/// End of the quoted text that opens at `start`: a doubled quote stays
/// inside.
fn plain_end(bytes: &[u8], start: usize, quote: u8) -> Option<usize> {
    let mut index = start + 1;
    loop {
        match bytes.get(index)? {
            byte if *byte == quote => {
                if bytes.get(index + 1) == Some(&quote) {
                    index += 2;
                } else {
                    return Some(index + 1);
                }
            }
            _ => index += 1,
        }
    }
}

/// What the first words of a statement make it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Opening {
    /// Not decided yet: nothing but white space and comments so far.
    Nothing,
    /// `CREATE` and what may stand between it and the kind of the object.
    Create,
    /// `WITH`, which may be followed by a function of its own.
    With,
    /// `CREATE TYPE`: a body is a unit, a specification one statement.
    Type,
    Sql,
    Block,
    Unit,
    /// `EXEC` of SQL*Plus: a call that ends with its line.
    Call,
}

/// How deep the reader is inside an anonymous block.
#[derive(Default)]
struct Nesting {
    /// `BEGIN` and `CASE` that were not closed yet.
    depth: u32,
    /// The block opened at least once.
    opened: bool,
    /// The word before was `END`: the next one says what it closes.
    after_end: bool,
    /// `PROCEDURE` or `FUNCTION` of a declaration part was read, and its
    /// `IS` or `AS` is still to come.
    declaring: bool,
    /// Bodies of subprograms of the declaration part that are still open or
    /// still to come: their `END` does not end the block.
    bodies: u32,
    /// The outermost `END` was read: the next `;` ends the block.
    closed: bool,
}
impl Nesting {
    fn word(&mut self, word: &str) {
        let upper = word.to_ascii_uppercase();
        if std::mem::take(&mut self.after_end) {
            match upper.as_str() {
                // `END IF` and `END LOOP` close what was never counted.
                "IF" | "LOOP" => {}
                // `END END`: a CASE expression right before the end of a
                // block.
                "END" => {
                    self.close();
                    self.after_end = true;
                }
                // `END CASE`, or `END name` with the name of the block.
                _ => self.close(),
            }
            return;
        }
        match upper.as_str() {
            "BEGIN" | "CASE" => {
                self.depth += 1;
                self.opened = true;
            }
            "END" => self.after_end = true,
            "PROCEDURE" | "FUNCTION" if self.depth == 0 => self.declaring = true,
            "IS" | "AS" if self.depth == 0 && std::mem::take(&mut self.declaring) => {
                self.bodies += 1;
            }
            _ => {}
        }
    }
    /// Anything but a word or a `;` was read: an `END` before it closed
    /// what was open, as in `CASE … END || 'x'`.
    fn other(&mut self) {
        if std::mem::take(&mut self.after_end) {
            self.close();
        }
    }
    fn close(&mut self) {
        self.depth = self.depth.saturating_sub(1);
        if self.depth == 0 && self.opened {
            if self.bodies > 0 {
                self.bodies -= 1;
            } else {
                self.closed = true;
            }
        }
    }
    /// A `;` was read. Returns whether it ends the block.
    fn semicolon(&mut self) -> bool {
        if std::mem::take(&mut self.after_end) {
            self.close();
        }
        // A declaration without a body: `PROCEDURE later;`.
        self.declaring = false;
        self.closed
    }
}

/// Cut a script into the statements that are sent one by one.
pub(super) fn steps(text: &str) -> Result<Vec<Step>> {
    ensure!(
        text.len() <= MAX_SQL_BYTES && !text.contains('\0'),
        "sql_invalid"
    );
    let bytes = text.as_bytes();
    let mut out: Vec<Step> = Vec::new();
    let mut start = 0;
    let mut opening = Opening::Nothing;
    let mut nesting = Nesting::default();
    // Whether anything but white space and comments was read.
    let mut content = false;
    let mut cuts: Vec<(usize, usize, Opening)> = Vec::new();
    // Where the statement itself begins. The comments before it are not
    // sent: the client library takes a statement that starts with a `--`
    // line for something the server does not know.
    let mut first = 0;
    lex(text, |token, range| {
        // A line that holds only `/` ends whatever was being read.
        if token == Token::Symbol && &text[range.clone()] == "/" {
            let before = text[..range.start]
                .rfind('\n')
                .map_or(0, |newline| newline + 1);
            let after = text[range.end..]
                .find('\n')
                .map_or(text.len(), |newline| range.end + newline);
            if text[before..range.start].trim().is_empty()
                && text[range.end..after].trim().is_empty()
            {
                if content {
                    cuts.push((first, range.start, opening));
                }
                start = range.end;
                opening = Opening::Nothing;
                nesting = Nesting::default();
                content = false;
                return;
            }
        }
        match token {
            Token::Space | Token::Comment => {
                // A call of SQL*Plus ends with its line.
                if opening == Opening::Call && text[range.clone()].contains('\n') && content {
                    let end = range.start;
                    cuts.push((first, end, opening));
                    start = range.end;
                    opening = Opening::Nothing;
                    content = false;
                }
                return;
            }
            Token::Word => {
                let word = &text[range.clone()];
                opening = match opening {
                    Opening::Nothing => {
                        if word.eq_ignore_ascii_case("BEGIN")
                            || word.eq_ignore_ascii_case("DECLARE")
                        {
                            Opening::Block
                        } else if word.eq_ignore_ascii_case("CREATE") {
                            Opening::Create
                        } else if word.eq_ignore_ascii_case("WITH") {
                            Opening::With
                        } else if word.eq_ignore_ascii_case("EXEC")
                            || word.eq_ignore_ascii_case("EXECUTE")
                        {
                            Opening::Call
                        } else {
                            Opening::Sql
                        }
                    }
                    Opening::Create => match word.to_ascii_uppercase().as_str() {
                        "OR" | "REPLACE" | "AND" | "RESOLVE" | "COMPILE" | "NO" | "FORCE"
                        | "EDITIONABLE" | "NONEDITIONABLE" | "IF" | "NOT" | "EXISTS" => {
                            Opening::Create
                        }
                        "PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "JAVA" => Opening::Unit,
                        "TYPE" => Opening::Type,
                        _ => Opening::Sql,
                    },
                    Opening::Type => {
                        if word.eq_ignore_ascii_case("BODY") {
                            Opening::Unit
                        } else {
                            Opening::Sql
                        }
                    }
                    Opening::With => match word.to_ascii_uppercase().as_str() {
                        "FUNCTION" | "PROCEDURE" => Opening::Unit,
                        _ => Opening::Sql,
                    },
                    decided => decided,
                };
                if opening == Opening::Block {
                    nesting.word(word);
                }
            }
            Token::Symbol => {
                let symbol = bytes[range.start];
                if opening == Opening::Nothing {
                    // `<<label>>` before a block.
                    opening = if symbol == b'<' {
                        Opening::Block
                    } else if symbol == b';' {
                        Opening::Nothing
                    } else {
                        Opening::Sql
                    };
                }
                if symbol == b';' {
                    let ends = match opening {
                        Opening::Nothing => {
                            start = range.end;
                            return;
                        }
                        Opening::Block => nesting.semicolon(),
                        Opening::Unit => false,
                        _ => true,
                    };
                    if ends {
                        // A block keeps the `;` after its `END`.
                        let end = if opening == Opening::Block {
                            range.end
                        } else {
                            range.start
                        };
                        cuts.push((first, end, opening));
                        start = range.end;
                        opening = Opening::Nothing;
                        nesting = Nesting::default();
                        content = false;
                    }
                    return;
                }
                if opening == Opening::Type {
                    opening = Opening::Sql;
                }
                if opening == Opening::Block {
                    nesting.other();
                }
            }
            Token::Literal | Token::Quoted => {
                if matches!(opening, Opening::Nothing | Opening::Type) {
                    opening = Opening::Sql;
                }
                if opening == Opening::Block {
                    nesting.other();
                }
            }
        }
        if !content {
            first = range.start;
        }
        content = true;
    })?;
    if content {
        cuts.push((first, text.len(), opening));
    }
    let _ = start;
    for (from, to, opening) in cuts {
        let piece = text[from..to].trim();
        if piece.is_empty() {
            continue;
        }
        out.push(match opening {
            // A block that was ended by a `/` line or by the end of the
            // text may lack the `;` after its END, which Oracle insists on.
            Opening::Block => Step {
                text: if piece.ends_with(';') {
                    piece.to_owned()
                } else {
                    format!("{piece};")
                },
                kind: Kind::Block,
            },
            Opening::Unit => Step {
                text: piece.to_owned(),
                kind: Kind::Unit,
            },
            // `EXEC name(arguments)` is a block around a call.
            Opening::Call => {
                let call = piece
                    .split_once(|c: char| c.is_whitespace())
                    .map_or("", |(_, call)| call.trim());
                ensure!(!call.is_empty(), "sql_invalid");
                Step {
                    text: format!("BEGIN {call}\n; END;"),
                    kind: Kind::Block,
                }
            }
            _ => Step {
                text: piece.to_owned(),
                kind: Kind::Sql,
            },
        });
    }
    ensure!(out.len() <= 10_000, "sql_too_many_statements");
    Ok(out)
}

/// A stored unit a statement creates: enough to ask the catalog whether it
/// compiled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Unit {
    /// The schema, when the statement names one.
    pub(super) owner: Option<String>,
    pub(super) name: String,
    /// The kind as the catalog names it: `PACKAGE BODY`.
    pub(super) kind: String,
}

/// The unit a `CREATE` statement names. Oracle creates a unit that does not
/// compile all the same, and answers with success.
pub(super) fn unit(text: &str) -> Option<Unit> {
    let mut tokens: Vec<(Token, &str)> = Vec::new();
    lex(text, |token, range| {
        // The head of the statement is enough.
        if !matches!(token, Token::Space | Token::Comment) && tokens.len() < 16 {
            tokens.push((token, &text[range]));
        }
    })
    .ok()?;
    let word = |index: usize, wanted: &str| {
        tokens
            .get(index)
            .is_some_and(|(token, text)| *token == Token::Word && text.eq_ignore_ascii_case(wanted))
    };
    if !word(0, "CREATE") {
        return None;
    }
    let mut index = 1;
    while [
        "OR",
        "REPLACE",
        "NO",
        "FORCE",
        "EDITIONABLE",
        "NONEDITIONABLE",
    ]
    .iter()
    .any(|skipped| word(index, skipped))
    {
        index += 1;
    }
    let mut kind = ["PROCEDURE", "FUNCTION", "PACKAGE", "TRIGGER", "TYPE"]
        .into_iter()
        .find(|kind| word(index, kind))?
        .to_owned();
    index += 1;
    if word(index, "BODY") {
        kind.push_str(" BODY");
        index += 1;
    }
    if word(index, "IF") && word(index + 1, "NOT") && word(index + 2, "EXISTS") {
        index += 3;
    }
    // A name that is not quoted is kept in upper case.
    let name = |index: usize| match tokens.get(index)? {
        (Token::Quoted, text) => Some(text[1..text.len() - 1].replace("\"\"", "\"")),
        (Token::Word, text) => Some(text.to_uppercase()),
        _ => None,
    };
    let first = name(index)?;
    let qualified = tokens
        .get(index + 1)
        .is_some_and(|(token, text)| *token == Token::Symbol && *text == ".");
    Some(if qualified {
        Unit {
            owner: Some(first),
            name: name(index + 2)?,
            kind,
        }
    } else {
        Unit {
            owner: None,
            name: first,
            kind,
        }
    })
}

/// Whether a statement has a placeholder (`:name`, `:1`) outside its
/// literals, names and comments. The client library reads them the same
/// way and then waits for values nobody has.
pub(super) fn has_placeholder(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut found = false;
    // After a literal a colon belongs to JSON syntax: `JSON {'a':1}`.
    let mut after_literal = false;
    let _ = lex(text, |token, range| {
        match token {
            Token::Space | Token::Comment => return,
            Token::Symbol if bytes[range.start] == b':' && !after_literal => {
                // White space may stand between the colon and the name.
                let next = bytes[range.end..]
                    .iter()
                    .copied()
                    .find(|byte| !byte.is_ascii_whitespace());
                if next.is_some_and(|byte| {
                    byte.is_ascii_alphanumeric() || byte == b'"' || byte >= 0x80
                }) {
                    found = true;
                }
            }
            _ => {}
        }
        after_literal = token == Token::Literal;
    });
    found
}

/// A stored unit as a statement that runs it without the library looking
/// inside: `:NEW` and `:OLD` of a trigger are not placeholders.
pub(super) fn dynamic(text: &str) -> String {
    format!("BEGIN EXECUTE IMMEDIATE {}; END;", long_text(text))
}

/// The statement the connection dialog tries a connection with. It is
/// `SELECT 1` on every engine, which Oracle before 23ai refuses for its
/// missing `FROM`.
pub(super) fn is_probe(text: &str) -> bool {
    text.trim()
        .trim_end_matches(';')
        .trim_end()
        .eq_ignore_ascii_case("select 1")
}

/// Characters of a text literal that is written in one piece. Four bytes
/// each at most, so the 4000 bytes of a literal are never passed.
const LITERAL_CHARS: usize = 1000;

/// `'it''s'`, for a text that fits one literal.
pub(super) fn short_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// A text of any length: one literal, or `TO_CLOB` pieces joined together.
fn long_text(text: &str) -> String {
    if text.chars().count() <= LITERAL_CHARS {
        return short_text(text);
    }
    let characters: Vec<char> = text.chars().collect();
    characters
        .chunks(LITERAL_CHARS)
        .map(|piece| format!("TO_CLOB({})", short_text(&piece.iter().collect::<String>())))
        .collect::<Vec<_>>()
        .join(" || ")
}

/// Bytes of a `RAW` literal: half of the 4000 characters of a literal.
const LITERAL_BYTES: usize = 2000;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

/// What is known about the column a value is written for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Target<'a> {
    /// The type of the column as the catalog names it, when it was read.
    pub(super) data_type: Option<&'a str>,
    /// The server has `TRUE` and `FALSE` (23ai).
    pub(super) booleans: bool,
}

/// Which family of types a column belongs to, as far as writing and
/// comparing values goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Family {
    /// `NUMBER` and `FLOAT`.
    Number,
    BinaryFloat,
    BinaryDouble,
    /// Compared with `DBMS_LOB.COMPARE`, like the two after it.
    Clob,
    NationalClob,
    Blob,
    Json,
    Vector,
    Other,
}

pub(super) fn family(data_type: &str) -> Family {
    let upper = data_type.trim().to_ascii_uppercase();
    match upper.as_str() {
        "BINARY_FLOAT" => Family::BinaryFloat,
        "BINARY_DOUBLE" => Family::BinaryDouble,
        "CLOB" => Family::Clob,
        "NCLOB" => Family::NationalClob,
        "BLOB" => Family::Blob,
        "JSON" => Family::Json,
        _ if upper.starts_with("VECTOR") => Family::Vector,
        _ if upper.starts_with("NUMBER") || upper.starts_with("FLOAT") => Family::Number,
        _ => Family::Other,
    }
}

/// A value as Oracle reads it.
pub(super) fn literal(cell: &Cell, target: Target<'_>) -> Result<String> {
    let family = target.data_type.map_or(Family::Other, family);
    // A number that is none, for a column that is known to hold no numbers:
    // the dialog of a new row takes an INTERVAL for an integer by its name.
    // Written as text, Oracle reads it the way it reads any text.
    let numeric = matches!(
        family,
        Family::Number | Family::BinaryFloat | Family::BinaryDouble
    );
    if let (Cell::Integer(text) | Cell::Decimal(text) | Cell::Float(text), Some(_), false) =
        (cell, target.data_type, numeric)
        && text.parse::<bigdecimal::BigDecimal>().is_err()
    {
        return literal(&Cell::Text(text.clone()), target);
    }
    Ok(match cell {
        Cell::Null => "NULL".into(),
        Cell::Bool(value) => match (target.booleans, value) {
            (true, true) => "TRUE".into(),
            (true, false) => "FALSE".into(),
            (false, true) => "1".into(),
            (false, false) => "0".into(),
        },
        Cell::Integer(text) => text
            .parse::<i128>()
            .map_err(|_| anyhow!("integer_invalid"))?
            .to_string(),
        Cell::Decimal(text) => {
            let value: bigdecimal::BigDecimal =
                text.parse().map_err(|_| anyhow!("decimal_invalid"))?;
            // No exponent: `1E+3` is read as a number, but never written so.
            value.to_plain_string()
        }
        Cell::Float(text) => {
            let value: f64 = text.parse().map_err(|_| anyhow!("float_invalid"))?;
            ensure!(value.is_finite(), "float_invalid");
            // A decimal literal is a NUMBER and overflows beyond 1E126; with
            // its suffix it is the binary number itself.
            match family {
                Family::BinaryFloat => format!("{:E}f", value as f32),
                Family::BinaryDouble => format!("{value:E}d"),
                _ if value.abs() >= 1e100 || (value != 0.0 && value.abs() < 1e-100) => {
                    format!("{value:E}d")
                }
                _ => value.to_string(),
            }
        }
        Cell::Text(text) | Cell::Json(text) => {
            ensure!(!text.contains('\0'), "text_nul_unsupported");
            // An empty text is NULL in Oracle; written so, nobody is misled.
            if text.is_empty() {
                "NULL".into()
            } else {
                long_text(text)
            }
        }
        Cell::Bytes(bytes) => {
            if bytes.is_empty() {
                "NULL".into()
            } else {
                ensure!(bytes.len() <= LITERAL_BYTES, "oracle_binary_too_long");
                format!("HEXTORAW('{}')", hex(bytes))
            }
        }
        Cell::Unsupported(_) => anyhow::bail!("unsupported_cell_type"),
    })
}

/// `column = value` in the way the type of the column allows.
fn equals(column: &str, value: &Cell, target: Target<'_>) -> Result<String> {
    let name = quote_identifier(&super::DIALECT, column)?;
    // An empty text is stored as NULL, so that is what it is compared with.
    let empty = match value {
        Cell::Null => true,
        Cell::Text(text) | Cell::Json(text) => text.is_empty(),
        Cell::Bytes(bytes) => bytes.is_empty(),
        _ => false,
    };
    if empty {
        return Ok(format!("{name} IS NULL"));
    }
    let written = literal(value, target)?;
    Ok(match target.data_type.map_or(Family::Other, family) {
        Family::Clob => format!("DBMS_LOB.COMPARE({name}, TO_CLOB({written})) = 0"),
        Family::NationalClob => format!("DBMS_LOB.COMPARE({name}, TO_NCLOB({written})) = 0"),
        Family::Blob => format!("DBMS_LOB.COMPARE({name}, TO_BLOB({written})) = 0"),
        Family::Json => format!("JSON_EQUAL({name}, {written})"),
        Family::Vector => {
            format!("VECTOR_DISTANCE({name}, TO_VECTOR({written}), EUCLIDEAN_SQUARED) = 0")
        }
        _ => format!("{name} = {written}"),
    })
}

/// What the driver knows about the columns of a table when it writes a
/// statement for it.
pub(super) struct Columns<'a> {
    /// Name and type of every column, when they were read.
    pub(super) types: Option<&'a [(String, String)]>,
    pub(super) booleans: bool,
}
impl<'a> Columns<'a> {
    fn target(&self, column: &str) -> Target<'a> {
        Target {
            data_type: self.types.and_then(|types| {
                types
                    .iter()
                    .find(|(name, _)| name == column)
                    .map(|(_, data_type)| data_type.as_str())
            }),
            booleans: self.booleans,
        }
    }
}

fn key_terms(key: &[(String, Cell)], columns: &Columns<'_>) -> Result<Vec<String>> {
    ensure!(
        !key.is_empty()
            && key.iter().all(|(_, value)| {
                !matches!(value, Cell::Null | Cell::Unsupported(_))
                    && !matches!(value, Cell::Text(text) if text.is_empty())
            }),
        "primary_key_required"
    );
    key.iter()
        .map(|(name, value)| equals(name, value, columns.target(name)))
        .collect()
}

/// The statement that makes one change of one record.
pub(super) fn change_statement(change: &RowChange, columns: &Columns<'_>) -> Result<String> {
    let dialect = &super::DIALECT;
    match change {
        RowChange::UpdateCell {
            table,
            column,
            old,
            new,
            key,
        } => {
            let mut terms = key_terms(key, columns)?;
            terms.push(equals(column, old, columns.target(column))?);
            Ok(format!(
                "UPDATE {} SET {} = {} WHERE {}",
                qualified(dialect, table)?,
                quote_identifier(dialect, column)?,
                literal(new, columns.target(column))?,
                terms.join(" AND ")
            ))
        }
        RowChange::InsertRow { table, values } => insert(table, values, columns),
        RowChange::DeleteRow { table, key } => Ok(format!(
            "DELETE FROM {} WHERE {}",
            qualified(dialect, table)?,
            key_terms(key, columns)?.join(" AND ")
        )),
    }
}

/// `INSERT` of one row. Oracle has no `DEFAULT VALUES`: a row of defaults
/// names one column and gives it `DEFAULT`.
fn insert(table: &TableRef, values: &[(String, Cell)], columns: &Columns<'_>) -> Result<String> {
    let dialect = &super::DIALECT;
    let target = qualified(dialect, table)?;
    if values.is_empty() {
        let first = columns
            .types
            .and_then(|types| types.first())
            .ok_or_else(|| anyhow!("oracle_columns_unknown"))?;
        return Ok(format!(
            "INSERT INTO {target} ({}) VALUES (DEFAULT)",
            quote_identifier(dialect, &first.0)?
        ));
    }
    let names = values
        .iter()
        .map(|(name, _)| quote_identifier(dialect, name))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    let literals = values
        .iter()
        .map(|(name, value)| literal(value, columns.target(name)))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    Ok(format!(
        "INSERT INTO {target} ({names}) VALUES ({literals})"
    ))
}

/// One `INSERT` of an import, as a statement of a PL/SQL block.
pub(super) fn import_statement(
    target: &str,
    names: &str,
    columns: &[String],
    row: &[Cell],
    known: &Columns<'_>,
) -> Result<String> {
    let literals = columns
        .iter()
        .zip(row)
        .map(|(column, cell)| literal(cell, known.target(column)))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    Ok(format!(
        "INSERT INTO {target} ({names}) VALUES ({literals});\n"
    ))
}

/// The text a column is searched as by the "contains" filter, in lower
/// case: what the grid shows for its type.
fn searched(name: &str, data_type: Option<&str>) -> String {
    let upper = data_type.map(|data_type| data_type.trim().to_ascii_uppercase());
    let upper = upper.as_deref().unwrap_or("");
    if upper.starts_with("NUMBER") || upper.starts_with("FLOAT") {
        // TO_CHAR writes `.5` and `-.5`; the grid shows `0.5` and `-0.5`.
        format!("LTRIM(REPLACE(REPLACE(' ' || TO_CHAR({name}), ' -.', ' -0.'), ' .', ' 0.'))")
    } else if upper == "BLOB" {
        // The first bytes, as the hexadecimal digits the grid shows.
        format!("LOWER(RAWTOHEX(DBMS_LOB.SUBSTR({name}, 2000, 1)))")
    } else {
        // Text as it is; dates, timestamps and intervals in the formats of
        // the session, which are the ones the grid shows; RAW as hex; JSON,
        // a boolean and a vector as Oracle writes them.
        format!("LOWER({name})")
    }
}

fn condition(filter: &Filter, columns: &Columns<'_>) -> Result<String> {
    let name = quote_identifier(&super::DIALECT, &filter.column)?;
    let target = columns.target(&filter.column);
    let empty = matches!(&filter.value, Cell::Text(text) if text.is_empty());
    Ok(match filter.op {
        FilterOp::IsNull => format!("{name} IS NULL"),
        FilterOp::IsNotNull => format!("{name} IS NOT NULL"),
        FilterOp::Contains => {
            let pattern = long_text(&format!("%{}%", like_pattern(&filter.value.display())));
            format!(
                "{} LIKE LOWER({pattern}) ESCAPE '{LIKE_ESCAPE}'",
                searched(&name, target.data_type)
            )
        }
        // An empty text is NULL: `= ''` would never be true.
        FilterOp::Equal if empty || matches!(filter.value, Cell::Null) => {
            format!("{name} IS NULL")
        }
        FilterOp::NotEqual if empty || matches!(filter.value, Cell::Null) => {
            format!("{name} IS NOT NULL")
        }
        FilterOp::Equal => equals(&filter.column, &filter.value, target)?,
        FilterOp::NotEqual => format!("NOT ({})", equals(&filter.column, &filter.value, target)?),
        FilterOp::Greater | FilterOp::Less => {
            let sign = if filter.op == FilterOp::Greater {
                ">"
            } else {
                "<"
            };
            format!("{name} {sign} {}", literal(&filter.value, target)?)
        }
    })
}

/// How the rows of a page are ordered when nobody chose an order, and what
/// breaks the ties of a chosen one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Order<'a> {
    /// Nothing is known about the container: the order of the scan.
    Unknown,
    /// The columns of the primary key.
    Key(&'a [String]),
    /// A table without a key: the address of the row.
    Rowid,
}

/// The statement that reads a page.
///
/// `OFFSET` and `FETCH` are pages of one list only when the order is total:
/// the primary key orders a page that has no order of its own and breaks the
/// ties of one that has; a table without a key is ordered by `ROWID`. A view
/// has neither and is read in the order the server produces it.
pub(super) fn page_statement(
    page: &PageRequest,
    columns: &Columns<'_>,
    order: Order<'_>,
) -> Result<String> {
    let dialect = &super::DIALECT;
    ensure!(
        page.limit > 0 && page.limit <= 20_000 && page.offset <= 1_000_000_000,
        "page_invalid"
    );
    let mut statement = format!("SELECT *\nFROM {}", qualified(dialect, &page.container)?);
    if !page.filters.is_empty() {
        let conditions = page
            .filters
            .iter()
            .map(|filter| condition(filter, columns))
            .collect::<Result<Vec<_>>>()?;
        statement.push_str(&format!("\nWHERE {}", conditions.join(" AND ")));
    }
    let mut terms = Vec::new();
    if let Some((column, descending)) = &page.order_by {
        terms.push(format!(
            "{} {}",
            quote_identifier(dialect, column)?,
            if *descending { "DESC" } else { "ASC" }
        ));
    }
    match order {
        Order::Unknown => {}
        Order::Key(key) => {
            for name in key {
                if page
                    .order_by
                    .as_ref()
                    .is_none_or(|(column, _)| column != name)
                {
                    terms.push(quote_identifier(dialect, name)?);
                }
            }
        }
        Order::Rowid => terms.push("ROWID".into()),
    }
    if !terms.is_empty() {
        statement.push_str(&format!("\nORDER BY {}", terms.join(", ")));
    }
    statement.push_str(&format!(
        "\nOFFSET {} ROWS FETCH NEXT {} ROWS ONLY",
        page.offset, page.limit
    ));
    Ok(statement)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(script: &str) -> Vec<(String, Kind)> {
        steps(script)
            .unwrap()
            .into_iter()
            .map(|step| (step.text, step.kind))
            .collect()
    }

    #[test]
    fn a_script_is_cut_where_oracle_ends_a_statement() {
        assert_eq!(
            texts(
                "SELECT 1 FROM dual; SELECT 'a;b', \"c;d\" FROM dual ;\n-- x;\n/* y; */ SELECT 3 FROM dual"
            ),
            [
                ("SELECT 1 FROM dual".to_owned(), Kind::Sql),
                ("SELECT 'a;b', \"c;d\" FROM dual".to_owned(), Kind::Sql),
                ("SELECT 3 FROM dual".to_owned(), Kind::Sql),
            ]
        );
        // Alternative quoting ends at its own bracket, whatever is inside.
        assert_eq!(
            texts("SELECT q'[it's; here]', Q'!a'b!', nq'{x;}' FROM dual; SELECT n'y;' FROM dual"),
            [
                (
                    "SELECT q'[it's; here]', Q'!a'b!', nq'{x;}' FROM dual".to_owned(),
                    Kind::Sql
                ),
                ("SELECT n'y;' FROM dual".to_owned(), Kind::Sql),
            ]
        );
        // A backslash is a character like any other.
        assert_eq!(
            texts("SELECT 'C:\\' FROM dual; SELECT 2 FROM dual").len(),
            2
        );
        for open in [
            "SELECT 'open",
            "SELECT q'[open]",
            "SELECT \"open",
            "SELECT 1 /* open",
        ] {
            assert_eq!(
                steps(open).unwrap_err().to_string(),
                "sql_unterminated_literal",
                "{open}"
            );
        }
        assert!(texts(" ; ;\n-- only a comment\n").is_empty());
        // Polish text keeps its bytes.
        assert_eq!(
            texts("SELECT 'Zażółć; gęślą' AS jaźń FROM dual; SELECT 2 FROM dual")[0].0,
            "SELECT 'Zażółć; gęślą' AS jaźń FROM dual"
        );
    }

    #[test]
    fn a_block_ends_with_its_own_end() {
        let script = "BEGIN\n  NULL;\nEND;\nSELECT 1 FROM dual;";
        assert_eq!(
            texts(script),
            [
                ("BEGIN\n  NULL;\nEND;".to_owned(), Kind::Block),
                ("SELECT 1 FROM dual".to_owned(), Kind::Sql),
            ]
        );
        // What looks like an end and is none.
        let nested = "DECLARE\n  n NUMBER := 0;\n  PROCEDURE later;\n  FUNCTION twice(x NUMBER) RETURN NUMBER IS\n  BEGIN\n    RETURN CASE WHEN x > 0 THEN x * 2 ELSE 0 END;\n  END twice;\n  PROCEDURE later IS BEGIN NULL; END;\nBEGIN\n  FOR i IN 1..3 LOOP\n    IF i > 1 THEN\n      n := n + twice(i);\n    END IF;\n    CASE i WHEN 1 THEN NULL; ELSE NULL; END CASE;\n  END LOOP;\n  BEGIN\n    n := n + 1; -- END;\n  EXCEPTION WHEN OTHERS THEN NULL;\n  END;\n  dbms_output.put_line('END; ' || n);\nEND;";
        let script = format!("{nested}\nSELECT 2 FROM dual");
        assert_eq!(
            texts(&script),
            [
                (nested.to_owned(), Kind::Block),
                ("SELECT 2 FROM dual".to_owned(), Kind::Sql),
            ]
        );
        // A label before the block and a name after its END.
        assert_eq!(
            texts("<<outer>> BEGIN NULL; END outer; SELECT 3 FROM dual"),
            [
                ("<<outer>> BEGIN NULL; END outer;".to_owned(), Kind::Block),
                ("SELECT 3 FROM dual".to_owned(), Kind::Sql),
            ]
        );
        // A block that is never closed runs to the end, for the server to
        // say what is missing.
        assert_eq!(
            texts("BEGIN\n  NULL;\nSELECT 1 FROM dual;"),
            [(
                "BEGIN\n  NULL;\nSELECT 1 FROM dual;".to_owned(),
                Kind::Block
            )]
        );
        // The `;` after the last END is added when a `/` line or the end of
        // the text came first.
        assert_eq!(
            texts("BEGIN\n  NULL;\nEND\n/\nSELECT 1 FROM dual\n/\nbegin null; end"),
            [
                ("BEGIN\n  NULL;\nEND;".to_owned(), Kind::Block),
                ("SELECT 1 FROM dual".to_owned(), Kind::Sql),
                ("begin null; end;".to_owned(), Kind::Block),
            ]
        );
    }

    #[test]
    fn a_slash_on_its_own_line_ends_a_unit() {
        let script = "CREATE OR REPLACE PROCEDURE baza_p(n IN NUMBER) AS\nBEGIN\n  UPDATE t SET a = a / 2 WHERE id = n;\nEND;\n/\nCREATE OR REPLACE TRIGGER baza_trg BEFORE INSERT ON t FOR EACH ROW\nBEGIN\n  :NEW.id := 1;\nEND;\n/\n\nSELECT 10 / 2 FROM dual\n/\nBEGIN baza_p(1); END;\n  /  \nCREATE TABLE plain (id NUMBER);\nCREATE EDITIONABLE PACKAGE baza_pkg AS\n  PROCEDURE p;\nEND baza_pkg;\n/";
        let found = texts(script);
        assert_eq!(
            found
                .iter()
                .map(|(text, kind)| (text.lines().next().unwrap(), *kind))
                .collect::<Vec<_>>(),
            [
                (
                    "CREATE OR REPLACE PROCEDURE baza_p(n IN NUMBER) AS",
                    Kind::Unit
                ),
                (
                    "CREATE OR REPLACE TRIGGER baza_trg BEFORE INSERT ON t FOR EACH ROW",
                    Kind::Unit
                ),
                ("SELECT 10 / 2 FROM dual", Kind::Sql),
                ("BEGIN baza_p(1); END;", Kind::Block),
                ("CREATE TABLE plain (id NUMBER)", Kind::Sql),
                ("CREATE EDITIONABLE PACKAGE baza_pkg AS", Kind::Unit),
            ]
        );
        // A unit keeps its last `;`, a statement loses it.
        assert!(found[0].0.ends_with("END;"));
        assert!(found[5].0.ends_with("END baza_pkg;"));
        // A function inside WITH is PL/SQL as well.
        assert_eq!(
            texts("WITH FUNCTION f RETURN NUMBER IS BEGIN RETURN 1; END;\nSELECT f FROM dual\n/\nWITH c AS (SELECT 1 n FROM dual) SELECT n FROM c;")
                .iter()
                .map(|(_, kind)| *kind)
                .collect::<Vec<_>>(),
            [Kind::Unit, Kind::Sql]
        );
        // The call of SQL*Plus.
        assert_eq!(
            texts(
                "EXEC dbms_stats.gather_table_stats(USER, 'T')\nexecute baza_p(2);\nSELECT 1 FROM dual"
            ),
            [
                (
                    "BEGIN dbms_stats.gather_table_stats(USER, 'T')\n; END;".to_owned(),
                    Kind::Block
                ),
                ("BEGIN baza_p(2)\n; END;".to_owned(), Kind::Block),
                ("SELECT 1 FROM dual".to_owned(), Kind::Sql),
            ]
        );
    }

    #[test]
    fn placeholders_are_found_where_the_library_finds_them() {
        for with in [
            "SELECT * FROM t WHERE id = :id",
            "SELECT :1 FROM dual",
            "BEGIN :NEW.id := 1; END;",
            "SELECT * FROM t WHERE a = :\"quoted\"",
        ] {
            assert!(has_placeholder(with), "{with}");
        }
        for without in [
            "SELECT ':a', q'[:b]', \":c\" FROM dual -- :d\n/* :e */",
            "BEGIN n := 1; END;",
            "SELECT JSON_OBJECT('a':1) FROM dual",
            "SELECT TO_CHAR(SYSDATE, 'HH24:MI') FROM dual",
        ] {
            assert!(!has_placeholder(without), "{without}");
        }
        assert_eq!(
            dynamic("CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN :NEW.a := 'b'; END;"),
            "BEGIN EXECUTE IMMEDIATE 'CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN :NEW.a := ''b''; END;'; END;"
        );
        assert!(!has_placeholder(&dynamic("BEGIN :NEW.a := 1; END;")));
        assert!(
            is_probe("SELECT 1") && is_probe(" select 1 ; ") && !is_probe("SELECT 1 FROM dual")
        );
    }

    #[test]
    fn a_stored_unit_is_named_as_the_catalog_names_it() {
        let named = |text: &str| unit(text).map(|unit| (unit.owner, unit.name, unit.kind));
        assert_eq!(
            named("CREATE OR REPLACE PROCEDURE baza_p(n NUMBER) AS BEGIN NULL; END;"),
            Some((None, "BAZA_P".into(), "PROCEDURE".into()))
        );
        assert_eq!(
            named("create or replace editionable package body \"App\".\"My Pkg\" as end;"),
            Some((Some("App".into()), "My Pkg".into(), "PACKAGE BODY".into()))
        );
        assert_eq!(
            named("CREATE /* x */ TRIGGER hr.trg -- y\nBEFORE INSERT ON t BEGIN NULL; END;"),
            Some((Some("HR".into()), "TRG".into(), "TRIGGER".into()))
        );
        assert_eq!(
            named("CREATE TYPE IF NOT EXISTS point AS OBJECT (x NUMBER)"),
            Some((None, "POINT".into(), "TYPE".into()))
        );
        assert_eq!(
            named("CREATE FUNCTION f RETURN NUMBER IS BEGIN RETURN 1; END;"),
            Some((None, "F".into(), "FUNCTION".into()))
        );
        for other in [
            "CREATE TABLE t (id NUMBER)",
            "CREATE OR REPLACE VIEW v AS SELECT 1 x FROM dual",
            "BEGIN NULL; END;",
            "CREATE PROCEDURE",
        ] {
            assert_eq!(named(other), None, "{other}");
        }
    }

    fn table() -> TableRef {
        TableRef {
            schema: "BAZA".into(),
            name: "Order Items".into(),
            kind: "table".into(),
        }
    }

    fn typed(types: &[(&str, &str)]) -> Vec<(String, String)> {
        types
            .iter()
            .map(|(name, data_type)| ((*name).to_owned(), (*data_type).to_owned()))
            .collect()
    }

    #[test]
    fn values_are_written_for_oracle() {
        let plain = Target::default();
        let write = |cell: Cell| literal(&cell, plain).unwrap();
        assert_eq!(
            write(Cell::Text("it's Zażółć 🦀".into())),
            "'it''s Zażółć 🦀'"
        );
        assert_eq!(
            write(Cell::Text("C:\\dane \"x\"".into())),
            "'C:\\dane \"x\"'"
        );
        // An empty text is NULL, and is written as what it is.
        assert_eq!(write(Cell::Text(String::new())), "NULL");
        assert_eq!(write(Cell::Bytes(Vec::new())), "NULL");
        assert_eq!(write(Cell::Null), "NULL");
        assert_eq!(
            write(Cell::Bytes(vec![0, 0xff, 0x10])),
            "HEXTORAW('00FF10')"
        );
        assert_eq!(
            write(Cell::Decimal("12345678901234567890.1200000000".into())),
            "12345678901234567890.1200000000"
        );
        assert_eq!(write(Cell::Decimal("1E+3".into())), "1000");
        assert_eq!(write(Cell::Integer("-42".into())), "-42");
        assert_eq!(write(Cell::Float("1.5".into())), "1.5");
        // No boolean before 23ai.
        assert_eq!(write(Cell::Bool(true)), "1");
        let modern = Target {
            booleans: true,
            ..plain
        };
        assert_eq!(literal(&Cell::Bool(false), modern).unwrap(), "FALSE");
        // A binary number is written as itself, not as a NUMBER.
        let double = Target {
            data_type: Some("BINARY_DOUBLE"),
            ..plain
        };
        let float = Target {
            data_type: Some("BINARY_FLOAT"),
            ..plain
        };
        assert_eq!(
            literal(&Cell::Float("1.1".into()), double).unwrap(),
            "1.1E0d"
        );
        assert_eq!(
            literal(&Cell::Float("1.1".into()), float).unwrap(),
            "1.1E0f"
        );
        assert_eq!(
            literal(&Cell::Float("1e300".into()), plain).unwrap(),
            "1E300d"
        );
        assert!(literal(&Cell::Float("inf".into()), double).is_err());
        // A long text is joined from pieces a literal may hold.
        let long = "ż".repeat(2500);
        let written = write(Cell::Text(long.clone()));
        assert_eq!(written.matches("TO_CLOB('").count(), 3);
        assert!(written.split(" || ").all(|piece| piece.len() <= 4000));
        assert_eq!(
            written
                .replace("TO_CLOB('", "")
                .replace("')", "")
                .replace(" || ", ""),
            long
        );
        assert_eq!(
            literal(&Cell::Bytes(vec![1; 2001]), plain)
                .unwrap_err()
                .to_string(),
            "oracle_binary_too_long"
        );
        assert!(literal(&Cell::Unsupported("<XMLTYPE>".into()), plain).is_err());
        // What the dialog of a new row calls an integer because the type has
        // "INT" in its name.
        let interval = Target {
            data_type: Some("INTERVAL YEAR(2) TO MONTH"),
            ..plain
        };
        assert_eq!(
            literal(&Cell::Integer("+01-06".into()), interval).unwrap(),
            "'+01-06'"
        );
        assert_eq!(literal(&Cell::Integer("7".into()), interval).unwrap(), "7");
        assert!(literal(&Cell::Integer("+01-06".into()), plain).is_err());
        let number = Target {
            data_type: Some("NUMBER(10)"),
            ..plain
        };
        assert!(literal(&Cell::Integer("x".into()), number).is_err());
    }

    #[test]
    fn a_change_is_one_statement_that_names_its_row() {
        let types = typed(&[
            ("id", "NUMBER(10)"),
            ("label", "VARCHAR2(50)"),
            ("note", "CLOB"),
            ("doc", "JSON"),
            ("ratio", "BINARY_DOUBLE"),
        ]);
        let columns = Columns {
            types: Some(&types),
            booleans: false,
        };
        let key = vec![("id".to_owned(), Cell::Integer("7".into()))];
        let update = |column: &str, old: Cell, new: Cell| {
            change_statement(
                &RowChange::UpdateCell {
                    table: table(),
                    column: column.into(),
                    old,
                    new,
                    key: key.clone(),
                },
                &columns,
            )
            .unwrap()
        };
        assert_eq!(
            update("label", Cell::Text("a".into()), Cell::Text("b'c".into())),
            "UPDATE \"BAZA\".\"Order Items\" SET \"label\" = 'b''c' WHERE \"id\" = 7 AND \"label\" = 'a'"
        );
        // NULL and the empty text are one value, on both sides.
        for old in [Cell::Null, Cell::Text(String::new())] {
            assert_eq!(
                update("label", old, Cell::Text(String::new())),
                "UPDATE \"BAZA\".\"Order Items\" SET \"label\" = NULL WHERE \"id\" = 7 AND \"label\" IS NULL"
            );
        }
        // Types that `=` does not compare.
        assert!(
            update("note", Cell::Text("old".into()), Cell::Text("new".into()))
                .ends_with("AND DBMS_LOB.COMPARE(\"note\", TO_CLOB('old')) = 0")
        );
        assert!(
            update(
                "doc",
                Cell::Json("{\"a\":1}".into()),
                Cell::Json("{}".into())
            )
            .ends_with("SET \"doc\" = '{}' WHERE \"id\" = 7 AND JSON_EQUAL(\"doc\", '{\"a\":1}')")
        );
        assert!(
            update("ratio", Cell::Float("0.1".into()), Cell::Float("2".into()))
                .ends_with("SET \"ratio\" = 2E0d WHERE \"id\" = 7 AND \"ratio\" = 1E-1d")
        );
        let delete = RowChange::DeleteRow {
            table: table(),
            key: key.clone(),
        };
        assert_eq!(
            change_statement(&delete, &columns).unwrap(),
            "DELETE FROM \"BAZA\".\"Order Items\" WHERE \"id\" = 7"
        );
        let insert = |values: Vec<(String, Cell)>, columns: &Columns<'_>| {
            change_statement(
                &RowChange::InsertRow {
                    table: table(),
                    values,
                },
                columns,
            )
        };
        assert_eq!(
            insert(
                vec![
                    ("id".into(), Cell::Integer("8".into())),
                    ("label".into(), Cell::Text(String::new()))
                ],
                &columns
            )
            .unwrap(),
            "INSERT INTO \"BAZA\".\"Order Items\" (\"id\", \"label\") VALUES (8, NULL)"
        );
        // A row of defaults names the first column.
        assert_eq!(
            insert(Vec::new(), &columns).unwrap(),
            "INSERT INTO \"BAZA\".\"Order Items\" (\"id\") VALUES (DEFAULT)"
        );
        let unknown = Columns {
            types: None,
            booleans: false,
        };
        assert_eq!(
            insert(Vec::new(), &unknown).unwrap_err().to_string(),
            "oracle_columns_unknown"
        );
        // A key that is NULL, or the empty text that is NULL, names no row.
        for missing in [Cell::Null, Cell::Text(String::new())] {
            let change = RowChange::DeleteRow {
                table: table(),
                key: vec![("id".to_owned(), missing)],
            };
            assert_eq!(
                change_statement(&change, &columns).unwrap_err().to_string(),
                "primary_key_required"
            );
        }
    }
}
