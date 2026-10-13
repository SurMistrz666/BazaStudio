//! Reader of the console: commands written the way the MongoDB shell takes
//! them, and the values inside them.
//!
//! Nothing here runs JavaScript. A statement is `show dbs`,
//! `show collections`, `use <name>` or a chain that starts at `db`:
//! `db.orders.find({status: "new"}).sort({total: -1}).limit(10)`. The
//! arguments are values in the relaxed notation of the shell: keys without
//! quotes, single quotes, comments, trailing commas, regular expressions
//! between slashes, and the constructors `ObjectId`, `ISODate`, `new Date`,
//! `NumberLong`, `NumberInt`, `NumberDecimal`, `Double`, `UUID`, `BinData`,
//! `HexData`, `Timestamp`, `RegExp`, `MinKey` and `MaxKey`. Extended JSON
//! (`{"$oid": "…"}`) is read as the value it stands for.
//!
//! A number follows extended JSON: without a fraction or an exponent it is
//! an int32, or an int64 when it does not fit; otherwise it is a double.
//!
//! What is not understood is refused with the place it was found at. The
//! reader never guesses.
use super::value::parse_date;
use ::mongodb::bson::spec::BinarySubtype;
use ::mongodb::bson::{
    Binary, Bson, DateTime, Decimal128, Document, JavaScriptCodeWithScope, Regex, Timestamp,
    oid::ObjectId,
};
use base64::Engine as _;

/// Text that is not a command or a value, and where it stops being one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SyntaxError {
    pub code: &'static str,
    /// Byte offset into the text.
    pub offset: usize,
}

/// The text does not follow the notation.
pub(super) const SYNTAX: &str = "mongodb_syntax_error";
/// The text is JavaScript beyond a command: a variable, a function, an
/// operator.
pub(super) const SCRIPT: &str = "mongodb_script_unsupported";
/// A constructor or an extended JSON value with content it cannot hold.
pub(super) const VALUE: &str = "mongodb_value_invalid";

/// One step of a chain after `db`.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Step {
    /// `.name` or `["name"]`.
    Member(String),
    /// `( … )` with its arguments.
    Call(Vec<Bson>),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Statement {
    ShowDatabases,
    ShowCollections,
    Use(String),
    /// `db` and what follows it. `confirmed` is the word `confirm` in
    /// front: the explicit yes to a command that removes or rewrites
    /// everything.
    Chain {
        steps: Vec<Step>,
        confirmed: bool,
    },
}

/// A statement and the byte offset it starts at.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Located {
    pub statement: Statement,
    pub offset: usize,
}

type Read<T> = Result<T, SyntaxError>;

/// Nesting of documents, arrays and calls a text may have.
const MAX_DEPTH: usize = 128;

struct Reader<'a> {
    text: &'a str,
    at: usize,
    depth: usize,
}

fn name_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}
fn name_part(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

impl<'a> Reader<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            at: 0,
            depth: 0,
        }
    }
    fn rest(&self) -> &'a str {
        &self.text[self.at..]
    }
    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.at += c.len_utf8();
        Some(c)
    }
    fn fail<T>(&self, code: &'static str) -> Read<T> {
        Err(SyntaxError {
            code,
            offset: self.at,
        })
    }
    fn fail_at<T>(&self, code: &'static str, offset: usize) -> Read<T> {
        Err(SyntaxError { code, offset })
    }
    /// Skip white space and comments.
    fn space(&mut self) -> Read<()> {
        loop {
            let rest = self.rest();
            if let Some(c) = rest.chars().next().filter(|c| c.is_whitespace()) {
                self.at += c.len_utf8();
            } else if rest.starts_with("//") {
                self.at += rest.find('\n').unwrap_or(rest.len());
            } else if rest.starts_with("/*") {
                match rest.find("*/") {
                    Some(end) => self.at += end + 2,
                    None => return self.fail(SYNTAX),
                }
            } else {
                return Ok(());
            }
        }
    }
    /// The next character after white space, if it is this one.
    fn eat(&mut self, wanted: char) -> Read<bool> {
        self.space()?;
        if self.peek() == Some(wanted) {
            self.at += wanted.len_utf8();
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn expect(&mut self, wanted: char) -> Read<()> {
        if self.eat(wanted)? {
            Ok(())
        } else {
            self.fail(SYNTAX)
        }
    }
    /// A name at the current position, without skipping anything first.
    fn name(&mut self) -> Option<&'a str> {
        let rest = self.rest();
        let first = rest.chars().next().filter(|c| name_start(*c))?;
        let length = rest
            .char_indices()
            .find(|(_, c)| !name_part(*c))
            .map_or(rest.len(), |(index, _)| index);
        self.at += length.max(first.len_utf8());
        Some(&rest[..length.max(first.len_utf8())])
    }

    fn string(&mut self) -> Read<String> {
        let Some(quote) = self.bump() else {
            return self.fail(SYNTAX);
        };
        let mut out = String::new();
        loop {
            let Some(c) = self.bump() else {
                return self.fail(SYNTAX);
            };
            match c {
                c if c == quote => return Ok(out),
                // A template with a placeholder would need JavaScript.
                '$' if quote == '`' && self.peek() == Some('{') => return self.fail(SCRIPT),
                '\n' if quote != '`' => return self.fail_at(SYNTAX, self.at - 1),
                '\\' => self.escape(&mut out)?,
                c => out.push(c),
            }
        }
    }
    fn hex_digits(&mut self, count: usize) -> Read<u32> {
        let digits = self.rest().get(..count).filter(|digits| {
            digits.len() == count && digits.bytes().all(|b| b.is_ascii_hexdigit())
        });
        match digits.and_then(|digits| u32::from_str_radix(digits, 16).ok()) {
            Some(number) => {
                self.at += count;
                Ok(number)
            }
            None => self.fail(SYNTAX),
        }
    }
    fn escape(&mut self, out: &mut String) -> Read<()> {
        let Some(c) = self.bump() else {
            return self.fail(SYNTAX);
        };
        match c {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0' if !self.peek().is_some_and(|next| next.is_ascii_digit()) => out.push('\0'),
            // A line continued on the next one.
            '\n' => {}
            'x' => {
                let code = self.hex_digits(2)?;
                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
            }
            'u' => {
                let start = self.at;
                let code = if self.peek() == Some('{') {
                    self.at += 1;
                    let rest = self.rest();
                    let Some(end) = rest.find('}').filter(|end| (1..=6).contains(end)) else {
                        return self.fail(SYNTAX);
                    };
                    let code = self.hex_digits(end)?;
                    self.at += 1;
                    code
                } else {
                    let high = self.hex_digits(4)?;
                    // Two halves of one character outside the basic plane.
                    if (0xd800..0xdc00).contains(&high) && self.rest().starts_with("\\u") {
                        let back = self.at;
                        self.at += 2;
                        match self.hex_digits(4) {
                            Ok(low) if (0xdc00..0xe000).contains(&low) => {
                                0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00)
                            }
                            _ => {
                                self.at = back;
                                high
                            }
                        }
                    } else {
                        high
                    }
                };
                match char::from_u32(code) {
                    Some(c) => out.push(c),
                    None => return self.fail_at(SYNTAX, start),
                }
            }
            other => out.push(other),
        }
        Ok(())
    }

    fn number(&mut self) -> Read<Bson> {
        let start = self.at;
        let negative = self.peek() == Some('-');
        if negative {
            self.at += 1;
        }
        if self.rest().starts_with("Infinity") {
            self.at += "Infinity".len();
            return Ok(Bson::Double(if negative {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }));
        }
        let digits = |reader: &mut Self, radix: u32| {
            let length = reader
                .rest()
                .find(|c: char| !c.is_digit(radix))
                .unwrap_or(reader.rest().len());
            reader.at += length;
            length
        };
        let rest = self.rest();
        let value = if rest.starts_with("0x") || rest.starts_with("0X") {
            self.at += 2;
            let from = self.at;
            if digits(self, 16) == 0 {
                return self.fail(SYNTAX);
            }
            let magnitude = i128::from_str_radix(&self.text[from..self.at], 16).ok();
            integer(magnitude.map(|number| if negative { -number } else { number }))
        } else {
            let whole = digits(self, 10);
            if whole > 1 && rest.starts_with('0') {
                // `007` is an octal number in old JavaScript and nothing in
                // JSON: neither is guessed.
                return self.fail_at(SYNTAX, start);
            }
            let mut fraction = false;
            if self.peek() == Some('.') {
                self.at += 1;
                fraction = true;
                if digits(self, 10) == 0 && whole == 0 {
                    return self.fail_at(SYNTAX, start);
                }
            } else if whole == 0 {
                return self.fail_at(SYNTAX, start);
            }
            if matches!(self.peek(), Some('e' | 'E')) {
                self.at += 1;
                fraction = true;
                if matches!(self.peek(), Some('+' | '-')) {
                    self.at += 1;
                }
                if digits(self, 10) == 0 {
                    return self.fail(SYNTAX);
                }
            }
            let text = &self.text[start..self.at];
            if fraction {
                text.parse::<f64>().ok().map(Bson::Double)
            } else {
                integer(text.parse::<i128>().ok())
            }
        };
        // `12n`, `12px`, `1.2.3`.
        if self.peek().is_some_and(|c| name_part(c) || c == '.') {
            return self.fail(SYNTAX);
        }
        match value {
            Some(value) => Ok(value),
            None => self.fail_at(VALUE, start),
        }
    }

    fn regex(&mut self) -> Read<Bson> {
        let start = self.at;
        self.at += 1;
        let mut pattern = String::new();
        let mut class = false;
        loop {
            let Some(c) = self.bump() else {
                return self.fail_at(SYNTAX, start);
            };
            match c {
                '\n' => return self.fail_at(SYNTAX, start),
                '\\' => {
                    pattern.push(c);
                    match self.bump() {
                        Some(next) if next != '\n' => pattern.push(next),
                        _ => return self.fail_at(SYNTAX, start),
                    }
                }
                '[' => {
                    class = true;
                    pattern.push(c);
                }
                ']' => {
                    class = false;
                    pattern.push(c);
                }
                '/' if !class => break,
                c => pattern.push(c),
            }
        }
        let flags = self.name().unwrap_or_default();
        regex_value(&pattern, flags).ok_or(SyntaxError {
            code: VALUE,
            offset: start,
        })
    }

    fn enter(&mut self) -> Read<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.fail(SYNTAX);
        }
        Ok(())
    }

    fn array(&mut self) -> Read<Vec<Bson>> {
        self.enter()?;
        self.at += 1;
        let mut items = Vec::new();
        loop {
            if self.eat(']')? {
                break;
            }
            items.push(self.value()?);
            if !self.eat(',')? {
                self.expect(']')?;
                break;
            }
        }
        self.depth -= 1;
        Ok(items)
    }

    fn object(&mut self) -> Read<Bson> {
        self.enter()?;
        let start = self.at;
        self.at += 1;
        let mut document = Document::new();
        loop {
            if self.eat('}')? {
                break;
            }
            let key = match self.peek() {
                Some('"' | '\'') => self.string()?,
                Some(c) if name_start(c) => self.name().unwrap_or_default().to_owned(),
                Some(c) if c.is_ascii_digit() => {
                    let rest = self.rest();
                    let length = rest
                        .find(|c: char| !c.is_ascii_digit())
                        .unwrap_or(rest.len());
                    self.at += length;
                    rest[..length].to_owned()
                }
                // `{...other}`, `{[name]: 1}`: JavaScript.
                Some('.' | '[') => return self.fail(SCRIPT),
                _ => return self.fail(SYNTAX),
            };
            self.space()?;
            if self.peek() != Some(':') {
                // `{name}` takes a variable; `a.b` needs quotes.
                return self.fail(if matches!(self.peek(), Some(',' | '}')) {
                    SCRIPT
                } else {
                    SYNTAX
                });
            }
            self.at += 1;
            let value = self.value()?;
            document.insert(key, value);
            if !self.eat(',')? {
                self.expect('}')?;
                break;
            }
        }
        self.depth -= 1;
        extended(document).map_err(|()| SyntaxError {
            code: VALUE,
            offset: start,
        })
    }

    /// The arguments of a call, after its `(`.
    fn arguments(&mut self) -> Read<Vec<Bson>> {
        self.enter()?;
        let mut items = Vec::new();
        loop {
            if self.eat(')')? {
                break;
            }
            items.push(self.value()?);
            if !self.eat(',')? {
                self.expect(')')?;
                break;
            }
        }
        self.depth -= 1;
        Ok(items)
    }

    fn value(&mut self) -> Read<Bson> {
        self.space()?;
        let start = self.at;
        match self.peek() {
            Some('{') => self.object(),
            Some('[') => self.array().map(Bson::Array),
            Some('"' | '\'' | '`') => self.string().map(Bson::String),
            Some('/') => self.regex(),
            Some(c) if c.is_ascii_digit() || c == '-' || c == '.' => self.number(),
            Some(c) if name_start(c) => {
                let mut word = self.name().unwrap_or_default();
                let fresh = word == "new";
                if fresh {
                    self.space()?;
                    let Some(name) = self.name() else {
                        return self.fail(SYNTAX);
                    };
                    word = name;
                }
                self.space()?;
                if self.peek() == Some('(') {
                    // Anything but a constructor of a value is a function
                    // of JavaScript: refused before its arguments are read.
                    if !CONSTRUCTORS.contains(&word) {
                        return self.fail_at(SCRIPT, start);
                    }
                    self.at += 1;
                    let arguments = self.arguments()?;
                    return construct(word, fresh, arguments).map_err(|code| SyntaxError {
                        code,
                        offset: start,
                    });
                }
                match word {
                    _ if fresh => self.fail(SYNTAX),
                    "true" => Ok(Bson::Boolean(true)),
                    "false" => Ok(Bson::Boolean(false)),
                    "null" => Ok(Bson::Null),
                    "undefined" => Ok(Bson::Undefined),
                    "NaN" => Ok(Bson::Double(f64::NAN)),
                    "Infinity" => Ok(Bson::Double(f64::INFINITY)),
                    "MinKey" => Ok(Bson::MinKey),
                    "MaxKey" => Ok(Bson::MaxKey),
                    // A variable or a function.
                    _ => self.fail_at(SCRIPT, start),
                }
            }
            _ => self.fail(SYNTAX),
        }
    }

    /// The rest of the line as one word: the argument of `use` and `show`.
    fn word(&mut self) -> &'a str {
        let rest = self.rest();
        let line = &rest[..rest.find(['\n', ';']).unwrap_or(rest.len())];
        self.at += line.len();
        line.trim()
    }

    fn chain(&mut self) -> Read<Vec<Step>> {
        let mut steps = Vec::new();
        loop {
            self.space()?;
            match self.peek() {
                Some('.') => {
                    self.at += 1;
                    self.space()?;
                    match self.name() {
                        Some(name) => steps.push(Step::Member(name.to_owned())),
                        None => return self.fail(SYNTAX),
                    }
                }
                Some('[') => {
                    self.at += 1;
                    self.space()?;
                    if !matches!(self.peek(), Some('"' | '\'' | '`')) {
                        return self.fail(SCRIPT);
                    }
                    steps.push(Step::Member(self.string()?));
                    self.expect(']')?;
                }
                Some('(') => {
                    self.at += 1;
                    steps.push(Step::Call(self.arguments()?));
                }
                _ => return Ok(steps),
            }
        }
    }

    fn statement(&mut self) -> Read<Statement> {
        let start = self.at;
        let Some(mut word) = self.name() else {
            return self.fail(SYNTAX);
        };
        let confirmed = word == "confirm";
        if confirmed {
            self.space()?;
            let after = self.at;
            match self.name() {
                Some("db") => word = "db",
                _ => return self.fail_at(SYNTAX, after),
            }
        }
        match word {
            "db" => Ok(Statement::Chain {
                steps: self.chain()?,
                confirmed,
            }),
            "show" => match self.word() {
                "dbs" | "databases" => Ok(Statement::ShowDatabases),
                "collections" | "tables" => Ok(Statement::ShowCollections),
                _ => self.fail_at(SCRIPT, start),
            },
            "use" => {
                let name = self.word();
                if database_name(name) {
                    Ok(Statement::Use(name.to_owned()))
                } else {
                    self.fail_at(VALUE, start)
                }
            }
            _ => self.fail_at(SCRIPT, start),
        }
    }
}

/// A name the server accepts for a database.
pub(super) fn database_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && !name.contains([
            '/', '\\', '.', ' ', '"', '$', '*', '<', '>', ':', '|', '?', '\0',
        ])
}

fn integer(number: Option<i128>) -> Option<Bson> {
    let number = i64::try_from(number?).ok()?;
    Some(match i32::try_from(number) {
        Ok(small) => Bson::Int32(small),
        Err(_) => Bson::Int64(number),
    })
}

/// A regular expression with the options the server knows, in the order
/// BSON stores them.
fn regex_value(pattern: &str, flags: &str) -> Option<Bson> {
    let mut options = flags.chars().collect::<Vec<_>>();
    options.sort_unstable();
    options.dedup();
    if options.len() != flags.chars().count() || !options.iter().all(|c| "imsxu".contains(*c)) {
        return None;
    }
    Some(Bson::RegularExpression(Regex {
        pattern: pattern.try_into().ok()?,
        options: options
            .into_iter()
            .collect::<String>()
            .as_str()
            .try_into()
            .ok()?,
    }))
}

/// A whole number an argument stands for, when it is one.
pub(super) fn whole(value: &Bson) -> Option<i64> {
    match value {
        Bson::Int32(number) => Some(i64::from(*number)),
        Bson::Int64(number) => Some(*number),
        // Up to 2^53 a double holds every whole number.
        Bson::Double(number)
            if number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_992.0 =>
        {
            Some(*number as i64)
        }
        _ => None,
    }
}

fn binary(subtype: i64, bytes: Option<Vec<u8>>) -> Option<Bson> {
    Some(Bson::Binary(Binary {
        subtype: BinarySubtype::from(u8::try_from(subtype).ok()?),
        bytes: bytes?,
    }))
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).ok())
        .collect()
}

fn uuid(text: &str) -> Option<Bson> {
    let id = uuid::Uuid::parse_str(text).ok()?;
    Some(Bson::Binary(Binary {
        subtype: BinarySubtype::Uuid,
        bytes: id.as_bytes().to_vec(),
    }))
}

fn date(value: &Bson) -> Option<Bson> {
    let millis = match value {
        Bson::String(text) => parse_date(text)?,
        other => whole(other)?,
    };
    Some(Bson::DateTime(DateTime::from_millis(millis)))
}

fn timestamp(time: &Bson, increment: &Bson) -> Option<Bson> {
    Some(Bson::Timestamp(Timestamp {
        time: u32::try_from(whole(time)?).ok()?,
        increment: u32::try_from(whole(increment)?).ok()?,
    }))
}

fn double_of(text: &str) -> Option<f64> {
    super::value::parse_double(text)
}

/// The constructors of values the reader knows.
const CONSTRUCTORS: &[&str] = &[
    "ObjectId",
    "ISODate",
    "Date",
    "NumberLong",
    "Long",
    "NumberInt",
    "Int32",
    "NumberDecimal",
    "Decimal128",
    "Double",
    "UUID",
    "BinData",
    "HexData",
    "Timestamp",
    "RegExp",
    "MinKey",
    "MaxKey",
    "Code",
];

/// The value a constructor of the shell makes.
fn construct(name: &str, fresh: bool, arguments: Vec<Bson>) -> Result<Bson, &'static str> {
    let made = match (name, arguments.as_slice()) {
        ("ObjectId", []) => Some(Bson::ObjectId(ObjectId::new())),
        ("ObjectId", [Bson::String(text)]) => ObjectId::parse_str(text).ok().map(Bson::ObjectId),
        ("ISODate", []) => Some(Bson::DateTime(DateTime::now())),
        ("ISODate", [Bson::String(_)]) => date(&arguments[0]),
        // Without `new`, JavaScript's `Date()` is a text, not a date.
        ("Date", _) if !fresh => return Err(SCRIPT),
        ("Date", []) => Some(Bson::DateTime(DateTime::now())),
        ("Date", [value]) => date(value),
        ("NumberLong" | "Long", [Bson::String(text)]) => text.trim().parse().ok().map(Bson::Int64),
        ("NumberLong" | "Long", [value]) => whole(value).map(Bson::Int64),
        ("NumberInt" | "Int32", [Bson::String(text)]) => text.trim().parse().ok().map(Bson::Int32),
        ("NumberInt" | "Int32", [value]) => whole(value)
            .and_then(|number| i32::try_from(number).ok())
            .map(Bson::Int32),
        ("NumberDecimal" | "Decimal128", [Bson::String(text)]) => {
            text.trim().parse::<Decimal128>().ok().map(Bson::Decimal128)
        }
        ("NumberDecimal" | "Decimal128", [Bson::Int32(number)]) => {
            number.to_string().parse().ok().map(Bson::Decimal128)
        }
        ("NumberDecimal" | "Decimal128", [Bson::Int64(number)]) => {
            number.to_string().parse().ok().map(Bson::Decimal128)
        }
        ("NumberDecimal" | "Decimal128", [Bson::Double(number)]) if number.is_finite() => {
            number.to_string().parse().ok().map(Bson::Decimal128)
        }
        ("Double", [Bson::String(text)]) => double_of(text).map(Bson::Double),
        ("Double", [Bson::Double(number)]) => Some(Bson::Double(*number)),
        ("Double", [value]) => whole(value).map(|number| Bson::Double(number as f64)),
        ("UUID", []) => Some(Bson::Binary(Binary {
            subtype: BinarySubtype::Uuid,
            bytes: uuid::Uuid::new_v4().as_bytes().to_vec(),
        })),
        ("UUID", [Bson::String(text)]) => uuid(text),
        ("BinData", [subtype, Bson::String(text)]) => whole(subtype).and_then(|subtype| {
            binary(
                subtype,
                base64::engine::general_purpose::STANDARD.decode(text).ok(),
            )
        }),
        ("HexData", [subtype, Bson::String(text)]) => {
            whole(subtype).and_then(|subtype| binary(subtype, from_hex(text)))
        }
        ("Timestamp", [time, increment]) => timestamp(time, increment),
        ("Timestamp", [Bson::Document(parts)]) if parts.len() == 2 => {
            match (parts.get("t"), parts.get("i")) {
                (Some(time), Some(increment)) => timestamp(time, increment),
                _ => None,
            }
        }
        ("RegExp", [Bson::String(pattern)]) => regex_value(pattern, ""),
        ("RegExp", [Bson::String(pattern), Bson::String(flags)]) => regex_value(pattern, flags),
        ("MinKey", []) => Some(Bson::MinKey),
        ("MaxKey", []) => Some(Bson::MaxKey),
        ("Code", [Bson::String(code)]) => Some(Bson::JavaScriptCode(code.clone())),
        // `new Date(2026, 0, 1)` and the like would need JavaScript.
        ("Date", _) => return Err(SCRIPT),
        _ => None,
    };
    made.ok_or(VALUE)
}

/// The value an extended JSON wrapper stands for; any other document as it
/// is. A document with a wrapper key and further keys is a document. A
/// wrapper whose content is not what it wraps is refused.
fn extended(document: Document) -> Result<Bson, ()> {
    let Some((key, value)) = document.iter().next() else {
        return Ok(Bson::Document(document));
    };
    if !key.starts_with('$') {
        return Ok(Bson::Document(document));
    }
    let text = |value: &Bson| match value {
        Bson::String(text) => Ok(text.clone()),
        _ => Err(()),
    };
    if document.len() == 2 {
        // The two wrappers with two keys.
        return match (key.as_str(), document.get("$scope"), document.get("$type")) {
            ("$code", Some(Bson::Document(scope)), _) => {
                Ok(Bson::JavaScriptCodeWithScope(JavaScriptCodeWithScope {
                    code: text(value)?,
                    scope: scope.clone(),
                }))
            }
            // Extended JSON of the first version.
            ("$binary", _, Some(Bson::String(subtype))) => binary(
                i64::from_str_radix(subtype, 16).map_err(|_| ())?,
                base64::engine::general_purpose::STANDARD
                    .decode(text(value)?)
                    .ok(),
            )
            .ok_or(()),
            _ => Ok(Bson::Document(document)),
        };
    }
    if document.len() != 1 {
        return Ok(Bson::Document(document));
    }
    let made = match key.as_str() {
        "$oid" => ObjectId::parse_str(text(value)?).ok().map(Bson::ObjectId),
        "$numberLong" => text(value)?.parse().ok().map(Bson::Int64),
        "$numberInt" => text(value)?.parse().ok().map(Bson::Int32),
        "$numberDouble" => double_of(&text(value)?).map(Bson::Double),
        "$numberDecimal" => text(value)?
            .parse::<Decimal128>()
            .ok()
            .map(Bson::Decimal128),
        "$date" => date(value),
        "$uuid" => uuid(&text(value)?),
        "$binary" => match value {
            Bson::Document(parts) if parts.len() == 2 => {
                match (parts.get("base64"), parts.get("subType")) {
                    (Some(Bson::String(data)), Some(Bson::String(subtype))) => binary(
                        i64::from_str_radix(subtype, 16).map_err(|_| ())?,
                        base64::engine::general_purpose::STANDARD.decode(data).ok(),
                    ),
                    _ => None,
                }
            }
            _ => None,
        },
        "$regularExpression" => match value {
            Bson::Document(parts) if parts.len() == 2 => {
                match (parts.get("pattern"), parts.get("options")) {
                    (Some(Bson::String(pattern)), Some(Bson::String(options))) => {
                        regex_value(pattern, options)
                    }
                    _ => None,
                }
            }
            _ => None,
        },
        "$timestamp" => match value {
            Bson::Document(parts) if parts.len() == 2 => match (parts.get("t"), parts.get("i")) {
                (Some(time), Some(increment)) => timestamp(time, increment),
                _ => None,
            },
            _ => None,
        },
        "$minKey" => (whole(value) == Some(1)).then_some(Bson::MinKey),
        "$maxKey" => (whole(value) == Some(1)).then_some(Bson::MaxKey),
        "$undefined" => (value == &Bson::Boolean(true)).then_some(Bson::Undefined),
        "$symbol" => Some(Bson::Symbol(text(value)?)),
        "$code" => Some(Bson::JavaScriptCode(text(value)?)),
        // An operator of a query or an update.
        _ => return Ok(Bson::Document(document)),
    };
    made.ok_or(())
}

/// One value that is the whole text.
pub(super) fn parse_value(text: &str) -> Read<Bson> {
    let mut reader = Reader::new(text);
    let value = reader.value()?;
    reader.space()?;
    if reader.at < text.len() {
        return reader.fail(SYNTAX);
    }
    Ok(value)
}

/// The statements of a script, separated by `;` or by line ends.
pub(super) fn parse_script(text: &str) -> Read<Vec<Located>> {
    let mut reader = Reader::new(text);
    let mut statements = Vec::new();
    loop {
        while reader.eat(';')? {}
        if reader.at >= text.len() {
            return Ok(statements);
        }
        let offset = reader.at;
        let statement = reader.statement()?;
        statements.push(Located { statement, offset });
        // What follows is the end, a separator or the next statement; an
        // operator or a property would be JavaScript.
        reader.space()?;
        if reader
            .peek()
            .is_some_and(|next| next != ';' && !name_start(next))
        {
            return reader.fail(SCRIPT);
        }
    }
}

/// Line and column of a byte offset, counted from one, in characters.
pub(super) fn position(text: &str, offset: usize) -> (usize, usize) {
    let mut end = offset.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let before = &text[..end];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .unwrap_or_default()
        .chars()
        .count()
        + 1;
    (line, column)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::mongodb::bson::doc;

    fn value(text: &str) -> Bson {
        parse_value(text).unwrap_or_else(|error| panic!("{text}: {error:?}"))
    }
    fn refused(text: &str) -> (&'static str, usize) {
        let error = parse_value(text).expect_err(text);
        (error.code, error.offset)
    }

    #[test]
    fn relaxed_notation_of_the_shell_is_read() {
        assert_eq!(
            value(
                r#"{ name: 'Zażółć "gęś"', "quoted key": 1, $gt: 2.5, nested: {a: [1, 2, {b: null}],}, 7: true, }"#
            ),
            Bson::Document(doc! {
                "name": "Zażółć \"gęś\"", "quoted key": 1, "$gt": 2.5,
                "nested": {"a": [1, 2, {"b": Bson::Null}]}, "7": true,
            })
        );
        // Comments and line ends are white space.
        assert_eq!(
            value("{ // the status\n  status: /* any */ \"new\",\n  qty: 3\n}"),
            Bson::Document(doc! {"status": "new", "qty": 3})
        );
        assert_eq!(value("[]"), Bson::Array(vec![]));
        assert_eq!(value("{}"), Bson::Document(doc! {}));
        assert_eq!(value(" `a\nb` "), Bson::String("a\nb".into()));
        assert_eq!(value("undefined"), Bson::Undefined);
        assert_eq!(value("MinKey"), Bson::MinKey);
    }

    #[test]
    fn numbers_follow_extended_json() {
        assert_eq!(value("0"), Bson::Int32(0));
        assert_eq!(value("-2147483648"), Bson::Int32(i32::MIN));
        assert_eq!(value("2147483648"), Bson::Int64(2_147_483_648));
        assert_eq!(value("-9223372036854775808"), Bson::Int64(i64::MIN));
        assert_eq!(value("1.0"), Bson::Double(1.0));
        assert_eq!(value("-.5"), Bson::Double(-0.5));
        assert_eq!(value("1e3"), Bson::Double(1000.0));
        assert_eq!(value("2.5E-3"), Bson::Double(0.0025));
        assert_eq!(value("0x1F"), Bson::Int32(31));
        assert_eq!(value("Infinity"), Bson::Double(f64::INFINITY));
        assert_eq!(value("-Infinity"), Bson::Double(f64::NEG_INFINITY));
        assert!(matches!(value("NaN"), Bson::Double(number) if number.is_nan()));
        // Too large for an int64: said, not rounded to a double.
        assert_eq!(refused("9223372036854775808"), (VALUE, 0));
        assert_eq!(refused("007").0, SYNTAX);
        assert_eq!(refused("12n").0, SYNTAX);
        assert_eq!(refused("1.2.3").0, SYNTAX);
        assert_eq!(refused("-").0, SYNTAX);
        assert_eq!(refused("1 + 2").0, SYNTAX);
    }

    #[test]
    fn texts_take_the_escapes_of_javascript() {
        assert_eq!(
            value(r#""a\tb\n\"c\" \\ \/ \x41 \u0105 \u{1F980} \uD83E\uDD80 \q""#),
            Bson::String("a\tb\n\"c\" \\ / A ą 🦀 🦀 q".into())
        );
        assert_eq!(value(r"'it\'s'"), Bson::String("it's".into()));
        assert_eq!(value("'a\\\nb'"), Bson::String("ab".into()));
        assert_eq!(refused("\"open").0, SYNTAX);
        assert_eq!(refused("\"line\nbreak\"").0, SYNTAX);
        assert_eq!(refused("`${name}`").0, SCRIPT);
        assert_eq!(refused(r#""\u12""#).0, SYNTAX);
    }

    #[test]
    fn constructors_of_the_shell_make_typed_values() {
        let id = ObjectId::parse_str("65f0c1e2a4b5c6d7e8f90123").unwrap();
        assert_eq!(
            value("ObjectId(\"65f0c1e2a4b5c6d7e8f90123\")"),
            Bson::ObjectId(id)
        );
        assert_eq!(
            value("ObjectId('65f0c1e2a4b5c6d7e8f90123')"),
            Bson::ObjectId(id)
        );
        assert!(matches!(value("ObjectId()"), Bson::ObjectId(_)));
        let noon = Bson::DateTime(DateTime::from_millis(1_714_557_600_000));
        assert_eq!(value("ISODate(\"2024-05-01T10:00:00Z\")"), noon);
        assert_eq!(value("ISODate('2024-05-01T12:00:00+02:00')"), noon);
        assert_eq!(value("new Date(\"2024-05-01T10:00:00.000Z\")"), noon);
        assert_eq!(value("new Date(1714557600000)"), noon);
        assert_eq!(
            value("ISODate(\"2024-05-01\")"),
            Bson::DateTime(DateTime::from_millis(1_714_521_600_000))
        );
        assert!(matches!(value("new Date()"), Bson::DateTime(_)));
        assert_eq!(value("NumberLong(5)"), Bson::Int64(5));
        assert_eq!(
            value("NumberLong(\"9007199254740993\")"),
            Bson::Int64(9_007_199_254_740_993)
        );
        assert_eq!(value("Long('-5')"), Bson::Int64(-5));
        assert_eq!(value("NumberInt(5)"), Bson::Int32(5));
        assert_eq!(value("Int32(\"5\")"), Bson::Int32(5));
        assert_eq!(
            value("NumberDecimal(\"12.50\")"),
            Bson::Decimal128("12.50".parse().unwrap())
        );
        assert_eq!(
            value("Decimal128('1E+3')"),
            Bson::Decimal128("1E+3".parse().unwrap())
        );
        assert_eq!(value("Double(5)"), Bson::Double(5.0));
        assert_eq!(
            value("UUID(\"73ffd264-44b3-4c69-90e8-e7d1dfc035d4\")"),
            Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes: uuid::Uuid::parse_str("73ffd264-44b3-4c69-90e8-e7d1dfc035d4")
                    .unwrap()
                    .as_bytes()
                    .to_vec(),
            })
        );
        assert_eq!(
            value("BinData(0, \"AAEC/w==\")"),
            Bson::Binary(Binary {
                subtype: BinarySubtype::Generic,
                bytes: vec![0, 1, 2, 255],
            })
        );
        assert_eq!(
            value("HexData(128, \"090807\")"),
            Bson::Binary(Binary {
                subtype: BinarySubtype::UserDefined(128),
                bytes: vec![9, 8, 7],
            })
        );
        assert_eq!(
            value("Timestamp(1714557600, 3)"),
            Bson::Timestamp(Timestamp {
                time: 1_714_557_600,
                increment: 3,
            })
        );
        assert_eq!(value("Timestamp({t: 1, i: 2})"), value("Timestamp(1, 2)"));
        assert_eq!(value("MaxKey()"), Bson::MaxKey);
        // What a constructor cannot hold is refused where it starts.
        for text in [
            "ObjectId(\"xyz\")",
            "ObjectId(5)",
            "ISODate(\"yesterday\")",
            "NumberInt(3000000000)",
            "NumberLong(1.5)",
            "NumberLong(\"abc\")",
            "NumberDecimal(\"abc\")",
            "UUID(\"nope\")",
            "BinData(300, \"AA==\")",
            "BinData(0, \"***\")",
            "HexData(0, \"abc\")",
            "Timestamp(-1, 0)",
        ] {
            assert_eq!(refused(&format!("{{a: {text}}}")), (VALUE, 4), "{text}");
        }
        // A function that is not a constructor of a value is JavaScript.
        assert_eq!(refused("{a: parseInt(\"5\")}"), (SCRIPT, 4));
        assert_eq!(refused("{a: Date()}"), (SCRIPT, 4));
        assert_eq!(refused("{a: someVariable}"), (SCRIPT, 4));
        assert_eq!(refused("{a}").0, SCRIPT);
        assert_eq!(refused("{...other}").0, SCRIPT);
        assert_eq!(refused("{a.b: 1}").0, SYNTAX);
        assert_eq!(refused("{a: 1 b: 2}").0, SYNTAX);
        assert_eq!(refused("{a: }").0, SYNTAX);
    }

    #[test]
    fn regular_expressions_are_read_between_slashes() {
        let regex = |pattern: &str, options: &str| {
            Bson::RegularExpression(Regex {
                pattern: pattern.try_into().unwrap(),
                options: options.try_into().unwrap(),
            })
        };
        assert_eq!(value("/^row \\d+$/i"), regex("^row \\d+$", "i"));
        assert_eq!(value("/a\\/b/"), regex("a\\/b", ""));
        // A slash inside a class does not end the expression.
        assert_eq!(value("/[/]x/mi"), regex("[/]x", "im"));
        assert_eq!(value("RegExp(\"^a\", \"i\")"), regex("^a", "i"));
        assert_eq!(
            value("{name: {$regex: \"^a\", $options: \"i\"}}"),
            Bson::Document(doc! {"name": {"$regex": "^a", "$options": "i"}})
        );
        assert_eq!(refused("/open").0, SYNTAX);
        assert_eq!(refused("/a/g").0, VALUE);
        assert_eq!(refused("/a/ii").0, VALUE);
    }

    #[test]
    fn extended_json_is_the_value_it_stands_for() {
        let id = ObjectId::parse_str("65f0c1e2a4b5c6d7e8f90123").unwrap();
        assert_eq!(
            value(r#"{"$oid":"65f0c1e2a4b5c6d7e8f90123"}"#),
            Bson::ObjectId(id)
        );
        assert_eq!(
            value("{$oid: '65f0c1e2a4b5c6d7e8f90123'}"),
            Bson::ObjectId(id)
        );
        assert_eq!(value(r#"{"$numberLong":"5"}"#), Bson::Int64(5));
        assert_eq!(value(r#"{"$numberInt":"5"}"#), Bson::Int32(5));
        assert_eq!(value(r#"{"$numberDouble":"5"}"#), Bson::Double(5.0));
        assert_eq!(
            value(r#"{"$numberDouble":"-Infinity"}"#),
            Bson::Double(f64::NEG_INFINITY)
        );
        assert_eq!(
            value(r#"{"$numberDecimal":"0.10"}"#),
            Bson::Decimal128("0.10".parse().unwrap())
        );
        let noon = Bson::DateTime(DateTime::from_millis(1_714_557_600_000));
        assert_eq!(value(r#"{"$date":"2024-05-01T10:00:00Z"}"#), noon);
        assert_eq!(value(r#"{"$date":{"$numberLong":"1714557600000"}}"#), noon);
        assert_eq!(value(r#"{"$date":1714557600000}"#), noon);
        assert_eq!(
            value(r#"{"$binary":{"base64":"AAEC/w==","subType":"00"}}"#),
            value("BinData(0, 'AAEC/w==')")
        );
        assert_eq!(
            value(r#"{"$binary":"AAEC/w==","$type":"80"}"#),
            value("BinData(128, 'AAEC/w==')")
        );
        assert_eq!(
            value(r#"{"$uuid":"73ffd264-44b3-4c69-90e8-e7d1dfc035d4"}"#),
            value("UUID('73ffd264-44b3-4c69-90e8-e7d1dfc035d4')")
        );
        assert_eq!(
            value(r#"{"$regularExpression":{"pattern":"^a","options":"i"}}"#),
            value("/^a/i")
        );
        assert_eq!(
            value(r#"{"$timestamp":{"t":1,"i":2}}"#),
            value("Timestamp(1, 2)")
        );
        assert_eq!(value(r#"{"$minKey":1}"#), Bson::MinKey);
        assert_eq!(value(r#"{"$maxKey":1}"#), Bson::MaxKey);
        assert_eq!(value(r#"{"$undefined":true}"#), Bson::Undefined);
        assert_eq!(value(r#"{"$symbol":"s"}"#), Bson::Symbol("s".into()));
        assert_eq!(value(r#"{"$code":"x"}"#), Bson::JavaScriptCode("x".into()));
        assert_eq!(
            value(r#"{"$code":"x","$scope":{"a":1}}"#),
            Bson::JavaScriptCodeWithScope(JavaScriptCodeWithScope {
                code: "x".into(),
                scope: doc! {"a": 1},
            })
        );
        // Operators and documents with further keys stay documents.
        assert_eq!(
            value("{$set: {a: 1}}"),
            Bson::Document(doc! {"$set": {"a": 1}})
        );
        assert_eq!(
            value(r#"{"$oid":"65f0c1e2a4b5c6d7e8f90123","more":1}"#),
            Bson::Document(doc! {"$oid": "65f0c1e2a4b5c6d7e8f90123", "more": 1})
        );
        // A wrapper with other content is refused, not passed on as a document.
        for text in [
            r#"{"$oid":"short"}"#,
            r#"{"$oid":5}"#,
            r#"{"$numberLong":5}"#,
            r#"{"$numberLong":"1.5"}"#,
            r#"{"$date":"soon"}"#,
            r#"{"$binary":{"base64":"AA==","subType":"zz"}}"#,
            r#"{"$uuid":"1234"}"#,
            r#"{"$minKey":2}"#,
        ] {
            assert_eq!(refused(text), (VALUE, 0), "{text}");
        }
    }

    fn script(text: &str) -> Vec<Statement> {
        parse_script(text)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .into_iter()
            .map(|located| located.statement)
            .collect()
    }
    fn member(name: &str) -> Step {
        Step::Member(name.into())
    }
    fn chain(steps: Vec<Step>) -> Statement {
        Statement::Chain {
            steps,
            confirmed: false,
        }
    }

    #[test]
    fn statements_are_chains_from_db_and_the_words_of_the_shell() {
        assert_eq!(
            script("db.orders.find({status: \"new\"}, {total: 1}).sort({total: -1}).limit(10)"),
            [chain(vec![
                member("orders"),
                member("find"),
                Step::Call(vec![
                    Bson::Document(doc! {"status": "new"}),
                    Bson::Document(doc! {"total": 1}),
                ]),
                member("sort"),
                Step::Call(vec![Bson::Document(doc! {"total": -1})]),
                member("limit"),
                Step::Call(vec![Bson::Int32(10)]),
            ])]
        );
        // A chain goes on over line ends; statements end at `;` or where
        // the next one starts.
        assert_eq!(
            script(
                "use bazaqa-shop\nshow collections;\n\ndb.getSiblingDB('other')[\"my coll\"]\n  .countDocuments()\n// done\ndb.getCollectionNames();;show dbs"
            ),
            [
                Statement::Use("bazaqa-shop".into()),
                Statement::ShowCollections,
                chain(vec![
                    member("getSiblingDB"),
                    Step::Call(vec![Bson::String("other".into())]),
                    member("my coll"),
                    member("countDocuments"),
                    Step::Call(vec![]),
                ]),
                chain(vec![member("getCollectionNames"), Step::Call(vec![])]),
                Statement::ShowDatabases,
            ]
        );
        assert_eq!(script("show databases"), [Statement::ShowDatabases]);
        assert_eq!(script("show tables"), [Statement::ShowCollections]);
        assert_eq!(script("  \n// nothing\n"), []);
        assert_eq!(
            script("confirm db.orders.drop()"),
            [Statement::Chain {
                steps: vec![member("orders"), member("drop"), Step::Call(vec![])],
                confirmed: true,
            }]
        );
        let offsets = parse_script("show dbs\n  db.a.find()")
            .unwrap()
            .into_iter()
            .map(|located| located.offset)
            .collect::<Vec<_>>();
        assert_eq!(offsets, [0, 11]);
    }

    #[test]
    fn javascript_is_refused_where_it_starts() {
        let refused = |text: &str| {
            let error = parse_script(text).expect_err(text);
            (error.code, position(text, error.offset))
        };
        assert_eq!(refused("var x = 5"), (SCRIPT, (1, 1)));
        assert_eq!(refused("SELECT * FROM orders"), (SCRIPT, (1, 1)));
        assert_eq!(refused("show users"), (SCRIPT, (1, 1)));
        assert_eq!(refused("use bad name"), (VALUE, (1, 1)));
        assert_eq!(refused("confirm show dbs"), (SYNTAX, (1, 9)));
        assert_eq!(
            refused("db.orders.find().forEach(function(d) { print(d) })"),
            (SCRIPT, (1, 26))
        );
        assert_eq!(refused("db.orders.find({a: x})"), (SCRIPT, (1, 20)));
        assert_eq!(refused("db.a.find()\ndb.b.find({a: })"), (SYNTAX, (2, 15)));
        assert_eq!(refused("db.a.count() + 1"), (SCRIPT, (1, 14)));
        assert_eq!(refused("db.a.find(}"), (SYNTAX, (1, 11)));
        assert_eq!(refused("db.a.find({a: 1}"), (SYNTAX, (1, 17)));
        assert_eq!(refused("db[name].find()"), (SCRIPT, (1, 4)));
        assert_eq!(refused("db.a.find() /* open"), (SYNTAX, (1, 13)));
        // Nesting has a limit instead of the stack.
        let deep = format!("db.a.find({}1{})", "[".repeat(500), "]".repeat(500));
        assert_eq!(parse_script(&deep).unwrap_err().code, SYNTAX);
    }

    #[test]
    fn positions_are_counted_in_characters() {
        assert_eq!(position("", 0), (1, 1));
        assert_eq!(position("ąę\nłó x", 9), (2, 3));
        assert_eq!(position("abc", 99), (1, 4));
    }
}
