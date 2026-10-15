//! C and C++ structures that lay out the bytes of a BLOB: the declarations
//! as a header writes them (comments, `#pragma pack`, `typedef`, `#define`
//! sizes, `enum` with an underlying type, the Windows integer names), the
//! layout a compiler gives them, and the values of their fields read from
//! bytes. Only what decides the bytes is read: functions, static members and
//! access specifiers are skipped.
use std::collections::HashMap;

/// How the program the bytes come from was built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Bytes of `long`: 4 on Windows (LLP64), 8 on 64-bit Unix (LP64).
    pub long_bytes: usize,
    pub big_endian: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            long_bytes: 4,
            big_endian: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prim {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    Bool,
    /// `char`: a byte read as a letter.
    Char,
    /// `wchar_t` of Windows: two bytes of UTF-16.
    WChar,
}
impl Prim {
    fn size(self) -> usize {
        match self {
            Prim::I8 | Prim::U8 | Prim::Bool | Prim::Char => 1,
            Prim::I16 | Prim::U16 | Prim::WChar => 2,
            Prim::I32 | Prim::U32 | Prim::F32 => 4,
            Prim::I64 | Prim::U64 | Prim::F64 => 8,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Prim::I8 => "int8",
            Prim::U8 => "uint8",
            Prim::I16 => "int16",
            Prim::U16 => "uint16",
            Prim::I32 => "int32",
            Prim::U32 => "uint32",
            Prim::I64 => "int64",
            Prim::U64 => "uint64",
            Prim::F32 => "float",
            Prim::F64 => "double",
            Prim::Bool => "bool",
            Prim::Char => "char",
            Prim::WChar => "wchar_t",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    Prim(Prim),
    /// A `struct`, `class` or `union`, by its place in [`Program::composites`].
    Composite(usize),
    /// An `enum`, by its place in [`Program::enums`].
    Enum(usize),
    Array(Box<Ty>, usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    /// Empty for an anonymous `struct` or `union` whose fields are the
    /// fields of the one around it.
    pub name: String,
    pub ty: Ty,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Composite {
    /// Empty for an anonymous one.
    pub name: String,
    pub union: bool,
    pub fields: Vec<Field>,
    /// `#pragma pack` in effect where it was defined, or 1 for `packed`.
    pub pack: Option<usize>,
    /// Declared at the top of the source, not inside another structure.
    pub top: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Enum {
    pub name: String,
    pub underlying: Prim,
    pub values: Vec<(i128, String)>,
}

/// The declarations of a source.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Program {
    pub composites: Vec<Composite>,
    pub enums: Vec<Enum>,
}

/// Why a source cannot be read, and on which line (from 1).
#[derive(Clone, Debug, PartialEq)]
pub struct Error {
    pub line: usize,
    pub kind: ErrorKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ErrorKind {
    /// Something else was expected here.
    Unexpected(String),
    UnknownType(String),
    /// A pointer holds an address of the program, not data of the BLOB.
    Pointer(String),
    BitField(String),
    /// An array size that is not a constant.
    ArraySize(String),
    /// A structure that contains itself.
    Cycle(String),
    /// Nothing that lays out bytes was declared.
    NoStructure,
    Unsupported(String),
    /// More than a header that lays out bytes needs: what went over.
    TooLarge(String),
}
impl Error {
    /// The reason as a sentence.
    pub fn describe(&self, en: bool) -> String {
        let (pl, english) = match &self.kind {
            ErrorKind::Unexpected(what) => (
                format!("nieoczekiwane „{what}”"),
                format!("unexpected “{what}”"),
            ),
            ErrorKind::UnknownType(name) => (
                format!("nieznany typ „{name}”"),
                format!("unknown type “{name}”"),
            ),
            ErrorKind::Pointer(name) => (
                format!("„{name}” jest wskaźnikiem: w BLOB-ie nie ma danych, na które wskazuje"),
                format!("“{name}” is a pointer: the BLOB does not hold what it points to"),
            ),
            ErrorKind::BitField(name) => (
                format!("pole bitowe „{name}” nie jest obsługiwane"),
                format!("the bit field “{name}” is not supported"),
            ),
            ErrorKind::ArraySize(what) => (
                format!("rozmiar tablicy „{what}” nie jest stałą"),
                format!("the array size “{what}” is not a constant"),
            ),
            ErrorKind::Cycle(name) => (
                format!("struktura „{name}” zawiera samą siebie"),
                format!("the structure “{name}” contains itself"),
            ),
            ErrorKind::NoStructure => (
                "nie zadeklarowano żadnej struktury".to_owned(),
                "no structure is declared".to_owned(),
            ),
            ErrorKind::Unsupported(what) => (
                format!("„{what}” nie jest obsługiwane"),
                format!("“{what}” is not supported"),
            ),
            ErrorKind::TooLarge(what) => (format!("za duże: {what}"), format!("too large: {what}")),
        };
        if en {
            format!("Line {}: {english}", self.line)
        } else {
            format!("Wiersz {}: {pl}", self.line)
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Num(i128),
    Punct(char),
    /// `#pragma pack(push[, n])`, `pack(pop)` and `pack([n])`.
    PackPush(Option<usize>),
    PackPop,
    PackSet(Option<usize>),
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    line: usize,
}

/// The text without its comments; line ends stay where they were.
fn strip_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('/', Some('/')) => {
                while chars.peek().is_some_and(|c| *c != '\n') {
                    chars.next();
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                    }
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

fn number(text: &str) -> Option<i128> {
    let digits = text.trim_end_matches(['u', 'U', 'l', 'L']);
    if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        i128::from_str_radix(hex, 16).ok()
    } else if digits.len() > 1 && digits.starts_with('0') {
        i128::from_str_radix(&digits[1..], 8).ok()
    } else {
        digits.parse().ok()
    }
}

fn tokenize(text: &str, line: usize, out: &mut Vec<Token>) -> Result<()> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            // `std::uint32_t` names `uint32_t`.
            if chars.get(i) == Some(&':') && chars.get(i + 1) == Some(&':') {
                i += 2;
                continue;
            }
            out.push(Token {
                tok: Tok::Ident(word),
                line,
            });
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            // A number that is not an integer (`1.5f`) is never a size; it
            // only stands in a default value, which is skipped.
            let tok = number(&word).map_or(Tok::Ident(word), Tok::Num);
            out.push(Token { tok, line });
        } else if c == '\'' {
            // A character constant, as in `enum { A = 'A' }`.
            let end = chars[i + 1..]
                .iter()
                .position(|c| *c == '\'')
                .map(|p| i + 1 + p);
            let Some(end) = end else {
                return Err(Error {
                    line,
                    kind: ErrorKind::Unexpected("'".into()),
                });
            };
            let value = chars[i + 1..end].first().map_or(0, |c| *c as i128);
            out.push(Token {
                tok: Tok::Num(value),
                line,
            });
            i = end + 1;
        } else {
            out.push(Token {
                tok: Tok::Punct(c),
                line,
            });
            i += 1;
        }
    }
    Ok(())
}

/// What a preprocessor line means for the layout: `#pragma pack` and
/// `#define` constants; the others (`#include`, `#ifndef`, …) mean nothing.
fn directive(
    text: &str,
    line: usize,
    defines: &mut HashMap<String, i128>,
    out: &mut Vec<Token>,
) -> Result<()> {
    let body = text.trim_start_matches('#').trim();
    if let Some(rest) = body.strip_prefix("pragma") {
        let rest = rest.trim();
        let Some(arguments) = rest
            .strip_prefix("pack")
            .map(str::trim)
            .and_then(|r| r.strip_prefix('('))
            .and_then(|r| r.trim_end().strip_suffix(')'))
        else {
            return Ok(());
        };
        let parts: Vec<&str> = arguments
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        let size = |text: &str| {
            number(text)
                .or_else(|| defines.get(text).copied())
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| n.is_power_of_two() && *n <= 16)
                .ok_or(Error {
                    line,
                    kind: ErrorKind::Unexpected(text.to_owned()),
                })
        };
        let tok = match parts.as_slice() {
            [] => Tok::PackSet(None),
            ["push"] => Tok::PackPush(None),
            ["push", n] => Tok::PackPush(Some(size(n)?)),
            ["pop", ..] => Tok::PackPop,
            [n] => Tok::PackSet(Some(size(n)?)),
            _ => return Ok(()),
        };
        out.push(Token { tok, line });
    } else if let Some(rest) = body.strip_prefix("define") {
        let mut words = rest.trim().splitn(2, char::is_whitespace);
        if let (Some(name), Some(value)) = (words.next(), words.next()) {
            // A constant only: `#define NAME (16 + 1)`; anything else is left.
            let mut tokens = Vec::new();
            if tokenize(value, line, &mut tokens).is_ok()
                && let Some(value) = Parser::constant_of(&tokens, defines)
            {
                defines.insert(name.to_owned(), value);
            }
        }
    }
    Ok(())
}

/// Longest source read.
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
/// Structures inside structures, parentheses inside a constant and levels of
/// a decoded value: a header nests a handful of them, and a source that
/// nests thousands would overflow the stack of the window's thread.
const MAX_NESTING: usize = 64;
/// Dimensions of an array, also through typedefs.
const MAX_DIMENSIONS: usize = 8;
/// Bytes of one type.
const MAX_TYPE_BYTES: usize = 1 << 32;

/// Read the declarations of a C or C++ source.
pub fn parse(source: &str, options: &Options) -> Result<Program> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(Error {
            line: 1,
            kind: ErrorKind::TooLarge(format!("{} KiB", source.len() / 1024)),
        });
    }
    let text = strip_comments(source);
    let mut tokens = Vec::new();
    let mut defines = HashMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim_start().starts_with('#') {
            directive(line.trim_start(), index + 1, &mut defines, &mut tokens)?;
        } else {
            tokenize(line, index + 1, &mut tokens)?;
        }
    }
    let mut parser = Parser {
        tokens,
        at: 0,
        pack: None,
        packs: Vec::new(),
        defines,
        types: HashMap::new(),
        program: Program::default(),
        options: *options,
        last_line: text.lines().count().max(1),
        nesting: 0,
    };
    parser.builtins();
    while parser.at < parser.tokens.len() {
        parser.top_level()?;
    }
    if !parser
        .program
        .composites
        .iter()
        .any(|c| c.top && !c.name.is_empty())
    {
        return Err(Error {
            line: 1,
            kind: ErrorKind::NoStructure,
        });
    }
    Ok(parser.program)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    pack: Option<usize>,
    packs: Vec<Option<usize>>,
    defines: HashMap<String, i128>,
    /// Type names: typedefs, structures, enums and the built-in names.
    types: HashMap<String, Ty>,
    program: Program,
    options: Options,
    last_line: usize,
    /// Bodies of structures open around the one being read.
    nesting: usize,
}

/// Words that make up a primitive type, such as `unsigned long long int`.
const PRIMITIVE_WORDS: &[&str] = &[
    "signed", "unsigned", "short", "long", "int", "char", "float", "double", "bool", "_Bool",
    "wchar_t", "const", "volatile",
];

impl Parser {
    fn builtins(&mut self) {
        use Prim::*;
        let names: &[(&str, Prim)] = &[
            ("int8_t", I8),
            ("uint8_t", U8),
            ("int16_t", I16),
            ("uint16_t", U16),
            ("int32_t", I32),
            ("uint32_t", U32),
            ("int64_t", I64),
            ("uint64_t", U64),
            ("size_t", U64),
            ("ssize_t", I64),
            ("char16_t", U16),
            ("char32_t", U32),
            ("__int8", I8),
            ("__int16", I16),
            ("__int32", I32),
            ("__int64", I64),
            ("BYTE", U8),
            ("UCHAR", U8),
            ("UINT8", U8),
            ("INT8", I8),
            ("CHAR", Char),
            ("TCHAR", Char),
            ("WORD", U16),
            ("USHORT", U16),
            ("UINT16", U16),
            ("SHORT", I16),
            ("INT16", I16),
            ("WCHAR", WChar),
            ("DWORD", U32),
            ("UINT", U32),
            ("UINT32", U32),
            ("ULONG", U32),
            ("INT", I32),
            ("INT32", I32),
            ("LONG", I32),
            ("BOOL", I32),
            ("QWORD", U64),
            ("DWORD64", U64),
            ("ULONGLONG", U64),
            ("UINT64", U64),
            ("LONGLONG", I64),
            ("INT64", I64),
            ("FLOAT", F32),
            ("DOUBLE", F64),
        ];
        for (name, prim) in names {
            self.types.insert((*name).to_owned(), Ty::Prim(*prim));
        }
    }

    fn line(&self) -> usize {
        self.tokens
            .get(self.at)
            .or_else(|| self.tokens.last())
            .map_or(self.last_line, |t| t.line)
    }
    fn error<T>(&self, kind: ErrorKind) -> Result<T> {
        Err(Error {
            line: self.line(),
            kind,
        })
    }
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.at).map(|t| &t.tok)
    }
    fn peek_at(&self, ahead: usize) -> Option<&Tok> {
        self.tokens.get(self.at + ahead).map(|t| &t.tok)
    }
    fn next(&mut self) -> Option<Tok> {
        let tok = self.tokens.get(self.at).map(|t| t.tok.clone());
        self.at += 1;
        tok
    }
    fn is_punct(&self, c: char) -> bool {
        self.peek() == Some(&Tok::Punct(c))
    }
    fn is_word(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(w)) if w == word)
    }
    fn expect(&mut self, c: char) -> Result<()> {
        if self.is_punct(c) {
            self.at += 1;
            Ok(())
        } else {
            let found = self.describe_next();
            self.error(ErrorKind::Unexpected(found))
        }
    }
    fn describe_next(&self) -> String {
        match self.peek() {
            Some(Tok::Ident(w)) => w.clone(),
            Some(Tok::Num(n)) => n.to_string(),
            Some(Tok::Punct(c)) => c.to_string(),
            Some(_) => "#pragma".into(),
            None => "EOF".into(),
        }
    }
    fn ident(&mut self) -> Result<String> {
        match self.peek() {
            Some(Tok::Ident(w)) => {
                let w = w.clone();
                self.at += 1;
                Ok(w)
            }
            _ => {
                let found = self.describe_next();
                self.error(ErrorKind::Unexpected(found))
            }
        }
    }

    /// Skip a bracketed part, `(…)`, `{…}` or `[…]`, with what it contains.
    fn skip_group(&mut self) -> Result<()> {
        let mut depth = 0usize;
        loop {
            match self.next() {
                Some(Tok::Punct('(' | '{' | '[')) => depth += 1,
                Some(Tok::Punct(')' | '}' | ']')) => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Ok(());
                    }
                }
                Some(_) => {}
                None => return self.error(ErrorKind::Unexpected("EOF".into())),
            }
        }
    }
    /// Skip to the end of a statement: its `;`, or a body in braces.
    fn skip_statement(&mut self) -> Result<()> {
        loop {
            match self.peek() {
                Some(Tok::Punct(';')) => {
                    self.at += 1;
                    return Ok(());
                }
                Some(Tok::Punct('{')) => {
                    self.skip_group()?;
                    if self.is_punct(';') {
                        self.at += 1;
                    }
                    return Ok(());
                }
                Some(Tok::Punct('(' | '[')) => self.skip_group()?,
                Some(_) => self.at += 1,
                None => return Ok(()),
            }
        }
    }

    /// `__attribute__((…))`, `__declspec(…)`, `alignas(…)` and `[[…]]`:
    /// whether one of them packs the structure.
    fn attributes(&mut self) -> Result<bool> {
        let mut packed = false;
        loop {
            if self.is_word("__attribute__")
                || self.is_word("__declspec")
                || self.is_word("alignas")
            {
                self.at += 1;
                let start = self.at;
                self.skip_group()?;
                packed |= self.tokens[start..self.at].iter().any(|t| {
                    t.tok == Tok::Ident("packed".into()) || t.tok == Tok::Ident("__packed__".into())
                });
            } else if self.is_punct('[') && self.peek_at(1) == Some(&Tok::Punct('[')) {
                self.skip_group()?;
            } else {
                return Ok(packed);
            }
        }
    }

    fn pack_directive(&mut self) -> bool {
        match self.peek().cloned() {
            Some(Tok::PackPush(n)) => {
                self.packs.push(self.pack);
                if n.is_some() {
                    self.pack = n;
                }
            }
            Some(Tok::PackPop) => self.pack = self.packs.pop().flatten(),
            Some(Tok::PackSet(n)) => self.pack = n,
            _ => return false,
        }
        self.at += 1;
        true
    }

    fn top_level(&mut self) -> Result<()> {
        if self.pack_directive() {
            return Ok(());
        }
        match self.peek() {
            Some(Tok::Punct(';' | '}')) => {
                // The end of a namespace, or an empty statement.
                self.at += 1;
                Ok(())
            }
            Some(Tok::Ident(word)) => match word.as_str() {
                "namespace" => {
                    // Its declarations are read as if they were outside it.
                    self.at += 1;
                    while !self.is_punct('{') && self.peek().is_some() {
                        self.at += 1;
                    }
                    self.at += 1;
                    Ok(())
                }
                "extern" if self.peek_at(1) == Some(&Tok::Punct('"')) => {
                    // `extern "C" {`: its declarations are read as top ones.
                    while !self.is_punct('{') && !self.is_punct(';') && self.peek().is_some() {
                        self.at += 1;
                    }
                    self.at += 1;
                    Ok(())
                }
                "extern" | "template" | "void" => self.skip_statement(),
                "using" => self.using(),
                "typedef" => {
                    self.at += 1;
                    self.declaration(true, true).map(|_| ())
                }
                "static" | "constexpr" | "const" => self.constant(),
                _ => self.declaration(false, true).map(|_| ()),
            },
            _ => self.skip_statement(),
        }
    }

    /// `using Name = type;` is a typedef.
    fn using(&mut self) -> Result<()> {
        self.at += 1;
        if self.is_word("namespace") {
            return self.skip_statement();
        }
        let name = self.ident()?;
        self.expect('=')?;
        let ty = self.type_spec(false)?;
        self.types.insert(name, ty);
        self.expect(';')
    }

    /// `static const int N = 5;` and the like: a constant for array sizes.
    fn constant(&mut self) -> Result<()> {
        while let Some(Tok::Ident(word)) = self.peek() {
            if !matches!(word.as_str(), "static" | "constexpr" | "const" | "inline") {
                break;
            }
            self.at += 1;
        }
        if self.is_word("struct") || self.is_word("class") || self.is_word("enum") {
            return self.declaration(false, true).map(|_| ());
        }
        // The type, then the name before `=`.
        let mut name = None;
        while let Some(tok) = self.peek().cloned() {
            match tok {
                Tok::Ident(word) => {
                    name = Some(word);
                    self.at += 1;
                }
                Tok::Punct('=') => {
                    self.at += 1;
                    let begin = self.at;
                    while !self.is_punct(';') && self.peek().is_some() {
                        self.at += 1;
                    }
                    if let (Some(name), Some(value)) = (
                        name,
                        Self::constant_of(&self.tokens[begin..self.at], &self.defines),
                    ) {
                        self.defines.insert(name, value);
                    }
                    self.at += 1;
                    return Ok(());
                }
                _ => return self.skip_statement(),
            }
        }
        Ok(())
    }

    /// The value of a constant expression: numbers, known names, `+ - * /`,
    /// `<<`, `>>`, `|`, `&` and parentheses. `None` for anything else.
    fn constant_of(tokens: &[Token], defines: &HashMap<String, i128>) -> Option<i128> {
        let mut at = 0;
        let value = expression(tokens, &mut at, defines, 0, 0)?;
        (at == tokens.len()).then_some(value)
    }

    /// A type, with the words before the name of a declaration.
    fn type_spec(&mut self, top: bool) -> Result<Ty> {
        // Qualifiers say nothing about the bytes.
        while matches!(self.peek(), Some(Tok::Ident(w)) if matches!(w.as_str(), "const" | "volatile" | "mutable" | "static" | "inline" | "typename"))
        {
            self.at += 1;
        }
        let Some(Tok::Ident(word)) = self.peek().cloned() else {
            let found = self.describe_next();
            return self.error(ErrorKind::Unexpected(found));
        };
        match word.as_str() {
            "struct" | "class" | "union" => self.composite(word == "union", top),
            "enum" => self.enumeration(),
            w if PRIMITIVE_WORDS.contains(&w) => self.primitive(),
            _ => {
                self.at += 1;
                match self.types.get(&word) {
                    Some(ty) => Ok(ty.clone()),
                    None => {
                        self.at -= 1;
                        self.error(ErrorKind::UnknownType(word))
                    }
                }
            }
        }
    }

    fn primitive(&mut self) -> Result<Ty> {
        let (mut unsigned, mut signed, mut shorts, mut longs) = (false, false, 0, 0);
        let mut base = None;
        while let Some(Tok::Ident(word)) = self.peek().cloned() {
            if !PRIMITIVE_WORDS.contains(&word.as_str()) {
                break;
            }
            self.at += 1;
            match word.as_str() {
                "unsigned" => unsigned = true,
                "signed" => signed = true,
                "short" => shorts += 1,
                "long" => longs += 1,
                "const" | "volatile" | "int" => {}
                other => base = Some(other.to_owned()),
            }
        }
        use Prim::*;
        let prim = match (base.as_deref(), shorts, longs) {
            (Some("char"), _, _) if unsigned => U8,
            (Some("char"), _, _) if signed => I8,
            (Some("char"), _, _) => Char,
            (Some("bool" | "_Bool"), _, _) => Bool,
            (Some("wchar_t"), _, _) => WChar,
            (Some("float"), _, _) => F32,
            (Some("double"), _, 0) => F64,
            (Some("double"), _, _) => {
                return self.error(ErrorKind::Unsupported("long double".into()));
            }
            (_, 1, _) if unsigned => U16,
            (_, 1, _) => I16,
            (_, _, 2) if unsigned => U64,
            (_, _, 2) => I64,
            (_, _, 1) => match (self.options.long_bytes, unsigned) {
                (8, true) => U64,
                (8, false) => I64,
                (_, true) => U32,
                (_, false) => I32,
            },
            _ if unsigned => U32,
            _ => I32,
        };
        Ok(Ty::Prim(prim))
    }

    /// `struct Name { … }`, or a reference to one.
    fn composite(&mut self, union: bool, top: bool) -> Result<Ty> {
        self.at += 1;
        let mut packed = self.attributes()?;
        let name = match self.peek() {
            Some(Tok::Ident(w)) if w != "final" => {
                let w = w.clone();
                self.at += 1;
                w
            }
            _ => String::new(),
        };
        packed |= self.attributes()?;
        if self.is_word("final") {
            self.at += 1;
        }
        // A base class: its fields come first.
        let mut base = None;
        if self.is_punct(':') {
            self.at += 1;
            while matches!(self.peek(), Some(Tok::Ident(w)) if matches!(w.as_str(), "public" | "private" | "protected" | "virtual"))
            {
                self.at += 1;
            }
            base = Some(self.type_spec(false)?);
            if self.is_punct(',') {
                return self.error(ErrorKind::Unsupported("multiple inheritance".into()));
            }
        }
        if !self.is_punct('{') {
            // A reference to a structure declared before.
            return match self.types.get(&name) {
                Some(ty) => Ok(ty.clone()),
                None => self.error(ErrorKind::UnknownType(name)),
            };
        }
        let pack = self.pack;
        let index = self.program.composites.len();
        self.program.composites.push(Composite {
            name: name.clone(),
            union,
            fields: Vec::new(),
            pack,
            top,
        });
        if !name.is_empty() {
            // Known inside its own body, for a member that refers to it.
            self.types.insert(name.clone(), Ty::Composite(index));
        }
        if self.nesting >= MAX_NESTING {
            return self.error(ErrorKind::TooLarge(format!(
                "{MAX_NESTING}+ struktur w strukturach / structures within structures"
            )));
        }
        self.nesting += 1;
        self.at += 1;
        let mut fields = Vec::new();
        if let Some(base) = base {
            fields.push(Field {
                name: String::new(),
                ty: base,
            });
        }
        while !self.is_punct('}') {
            if self.peek().is_none() {
                return self.error(ErrorKind::Unexpected("EOF".into()));
            }
            self.member(&name, &mut fields)?;
        }
        self.nesting -= 1;
        self.at += 1;
        packed |= self.attributes()?;
        let composite = &mut self.program.composites[index];
        composite.fields = fields;
        if packed {
            composite.pack = Some(1);
        }
        Ok(Ty::Composite(index))
    }

    /// One member of a structure: fields, or something without bytes.
    fn member(&mut self, owner: &str, fields: &mut Vec<Field>) -> Result<()> {
        if self.pack_directive() {
            return Ok(());
        }
        match self.peek().cloned() {
            Some(Tok::Punct(';')) => {
                self.at += 1;
                return Ok(());
            }
            Some(Tok::Ident(word)) => match word.as_str() {
                "public" | "private" | "protected" if self.peek_at(1) == Some(&Tok::Punct(':')) => {
                    self.at += 2;
                    return Ok(());
                }
                "static" | "constexpr" => return self.constant(),
                "void" => return self.function(),
                "friend" | "template" | "virtual" | "operator" | "explicit" => {
                    return self.skip_statement();
                }
                "typedef" => {
                    self.at += 1;
                    return self.declaration(true, false).map(|_| ());
                }
                "using" => return self.using(),
                // A constructor or a destructor.
                w if w == owner && self.peek_at(1) == Some(&Tok::Punct('(')) => {
                    return self.function();
                }
                _ => {}
            },
            Some(Tok::Punct('~')) => return self.function(),
            _ => {}
        }
        fields.extend(self.declaration(false, false)?);
        Ok(())
    }

    /// Skip a function: its parameters, qualifiers, an initializer list and
    /// its body or `;`.
    fn function(&mut self) -> Result<()> {
        while !self.is_punct('(') {
            if self.peek().is_none() {
                return Ok(());
            }
            self.at += 1;
        }
        self.skip_group()?;
        loop {
            match self.peek() {
                Some(Tok::Punct(';')) => {
                    self.at += 1;
                    return Ok(());
                }
                Some(Tok::Punct('{')) => {
                    self.skip_group()?;
                    return Ok(());
                }
                Some(Tok::Punct('(' | '[')) => self.skip_group()?,
                Some(_) => self.at += 1,
                None => return Ok(()),
            }
        }
    }

    /// `type a, b[4], c;` with an optional definition in the type. A typedef
    /// names the type; otherwise, inside a structure, the names are fields.
    fn declaration(&mut self, typedef: bool, top: bool) -> Result<Vec<Field>> {
        let ty = self.type_spec(top)?;
        let mut fields = Vec::new();
        // A definition alone: `struct A { … };` or an anonymous member.
        if self.is_punct(';') {
            self.at += 1;
            if !top
                && !typedef
                && let Ty::Composite(index) = ty
                && self.program.composites[index].name.is_empty()
            {
                fields.push(Field {
                    name: String::new(),
                    ty,
                });
            }
            return Ok(fields);
        }
        loop {
            let (name, field_ty) = self.declarator(ty.clone())?;
            if self.is_punct('(') {
                // A function returning this type.
                return self.function().map(|_| Vec::new());
            }
            if self.is_punct(':') {
                return self.error(ErrorKind::BitField(name));
            }
            // A default value of a member says nothing about the bytes read.
            if self.is_punct('=') {
                while !self.is_punct(',') && !self.is_punct(';') && self.peek().is_some() {
                    if self.is_punct('{') || self.is_punct('(') {
                        self.skip_group()?;
                    } else {
                        self.at += 1;
                    }
                }
            } else if self.is_punct('{') {
                self.skip_group()?;
            }
            if typedef {
                // `typedef struct { … } Name;` gives the structure its name.
                if let Ty::Composite(index) = field_ty
                    && self.program.composites[index].name.is_empty()
                {
                    self.program.composites[index].name = name.clone();
                }
                self.types.insert(name, field_ty);
            } else if !top {
                fields.push(Field { name, ty: field_ty });
            }
            if self.is_punct(',') {
                self.at += 1;
                continue;
            }
            self.expect(';')?;
            return Ok(fields);
        }
    }

    /// `name`, `name[4][2]`; a pointer is refused.
    fn declarator(&mut self, ty: Ty) -> Result<(String, Ty)> {
        self.attributes()?;
        if self.is_punct('*') || self.is_punct('&') {
            self.at += 1;
            let name = match self.peek() {
                Some(Tok::Ident(w)) => w.clone(),
                _ => "*".into(),
            };
            return self.error(ErrorKind::Pointer(name));
        }
        let name = self.ident()?;
        let mut dims = Vec::new();
        while self.is_punct('[') {
            self.at += 1;
            let begin = self.at;
            while !self.is_punct(']') {
                if self.peek().is_none() {
                    return self.error(ErrorKind::Unexpected("EOF".into()));
                }
                self.at += 1;
            }
            let tokens = &self.tokens[begin..self.at];
            let size = Self::constant_of(tokens, &self.defines)
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| *n <= 1 << 24);
            let Some(size) = size else {
                let text = tokens
                    .iter()
                    .map(|t| match &t.tok {
                        Tok::Ident(w) => w.clone(),
                        Tok::Num(n) => n.to_string(),
                        Tok::Punct(c) => c.to_string(),
                        _ => String::new(),
                    })
                    .collect::<Vec<_>>()
                    .join("");
                return self.error(ErrorKind::ArraySize(text));
            };
            dims.push(size);
            self.at += 1;
        }
        self.attributes()?;
        // The dimensions of a type named by a typedef count as well.
        let mut inner = &ty;
        let mut depth = dims.len();
        while let Ty::Array(element, _) = inner {
            depth += 1;
            inner = element;
        }
        if depth > MAX_DIMENSIONS {
            return self.error(ErrorKind::TooLarge(format!(
                "{name}: {depth} wymiarów / dimensions"
            )));
        }
        // `int a[2][3]` is two arrays of three.
        let ty = dims
            .into_iter()
            .rev()
            .fold(ty, |inner, size| Ty::Array(Box::new(inner), size));
        Ok((name, ty))
    }

    /// `enum [class] Name [: type] { A, B = 5 }`, or a reference to one.
    fn enumeration(&mut self) -> Result<Ty> {
        self.at += 1;
        if self.is_word("class") || self.is_word("struct") {
            self.at += 1;
        }
        self.attributes()?;
        let name = match self.peek() {
            Some(Tok::Ident(w)) => {
                let w = w.clone();
                self.at += 1;
                w
            }
            _ => String::new(),
        };
        let mut underlying = Prim::I32;
        if self.is_punct(':') {
            self.at += 1;
            underlying = match self.type_spec(false)? {
                Ty::Prim(prim) => prim,
                _ => return self.error(ErrorKind::Unsupported("enum base".into())),
            };
        }
        if !self.is_punct('{') {
            return match self.types.get(&name) {
                Some(ty) => Ok(ty.clone()),
                None => self.error(ErrorKind::UnknownType(name)),
            };
        }
        self.at += 1;
        let mut values = Vec::new();
        let mut next = 0i128;
        while !self.is_punct('}') {
            let variant = self.ident()?;
            if self.is_punct('=') {
                self.at += 1;
                let begin = self.at;
                let mut depth = 0usize;
                while let Some(tok) = self.peek() {
                    match tok {
                        Tok::Punct('(') => depth += 1,
                        Tok::Punct(')') => depth = depth.saturating_sub(1),
                        Tok::Punct(',' | '}') if depth == 0 => break,
                        _ => {}
                    }
                    self.at += 1;
                }
                next = Self::constant_of(&self.tokens[begin..self.at], &self.defines).ok_or(
                    Error {
                        line: self.line(),
                        kind: ErrorKind::ArraySize(variant.clone()),
                    },
                )?;
            }
            // Its constants may size the arrays that follow.
            self.defines.insert(variant.clone(), next);
            values.push((next, variant));
            next = next.saturating_add(1);
            if self.is_punct(',') {
                self.at += 1;
            }
        }
        self.at += 1;
        let index = self.program.enums.len();
        self.program.enums.push(Enum {
            name: name.clone(),
            underlying,
            values,
        });
        let ty = Ty::Enum(index);
        if !name.is_empty() {
            self.types.insert(name, ty.clone());
        }
        Ok(ty)
    }
}

/// A constant expression by precedence climbing; `depth` counts the
/// parentheses and signs around it.
fn expression(
    tokens: &[Token],
    at: &mut usize,
    defines: &HashMap<String, i128>,
    min: u8,
    depth: usize,
) -> Option<i128> {
    let mut left = operand(tokens, at, defines, depth)?;
    loop {
        let (op, width) = match (
            tokens.get(*at).map(|t| &t.tok),
            tokens.get(*at + 1).map(|t| &t.tok),
        ) {
            (Some(Tok::Punct('<')), Some(Tok::Punct('<'))) => ('l', 2),
            (Some(Tok::Punct('>')), Some(Tok::Punct('>'))) => ('r', 2),
            (Some(Tok::Punct(c @ ('+' | '-' | '*' | '/' | '%' | '|' | '&'))), _) => (*c, 1),
            _ => return Some(left),
        };
        let precedence = match op {
            '|' => 1,
            '&' => 2,
            'l' | 'r' => 3,
            '+' | '-' => 4,
            _ => 5,
        };
        if precedence < min {
            return Some(left);
        }
        *at += width;
        let right = expression(tokens, at, defines, precedence + 1, depth)?;
        left = match op {
            '+' => left.checked_add(right)?,
            '-' => left.checked_sub(right)?,
            '*' => left.checked_mul(right)?,
            '/' => left.checked_div(right)?,
            '%' => left.checked_rem(right)?,
            '|' => left | right,
            '&' => left & right,
            // A shift that pushes bits out of the value is no constant.
            'l' => {
                let shift = u32::try_from(right).ok()?;
                let shifted = left.checked_shl(shift)?;
                (shifted >> shift == left).then_some(shifted)?
            }
            _ => left.checked_shr(u32::try_from(right).ok()?)?,
        };
    }
}

fn operand(
    tokens: &[Token],
    at: &mut usize,
    defines: &HashMap<String, i128>,
    depth: usize,
) -> Option<i128> {
    if depth >= MAX_NESTING {
        return None;
    }
    let tok = &tokens.get(*at)?.tok;
    *at += 1;
    match tok {
        Tok::Num(n) => Some(*n),
        Tok::Ident(name) => defines.get(name).copied(),
        Tok::Punct('-') => operand(tokens, at, defines, depth + 1)?.checked_neg(),
        Tok::Punct('(') => {
            let value = expression(tokens, at, defines, 0, depth + 1)?;
            (tokens.get(*at)?.tok == Tok::Punct(')')).then(|| {
                *at += 1;
                value
            })
        }
        _ => None,
    }
}

/// Size and alignment of each type, as a compiler for x86 lays them out.
pub struct Layout<'p> {
    program: &'p Program,
    /// Size, alignment and the offsets of the fields, by composite.
    composites: Vec<Option<(usize, usize, Vec<usize>)>>,
}

impl<'p> Layout<'p> {
    pub fn new(program: &'p Program) -> std::result::Result<Self, Error> {
        let mut layout = Self {
            program,
            composites: vec![None; program.composites.len()],
        };
        for index in 0..program.composites.len() {
            layout.composite(index, &mut Vec::new())?;
        }
        Ok(layout)
    }

    fn composite(&mut self, index: usize, path: &mut Vec<usize>) -> Result<(usize, usize)> {
        if let Some((size, align, _)) = &self.composites[index] {
            return Ok((*size, *align));
        }
        let composite = &self.program.composites[index];
        if path.contains(&index) {
            return Err(Error {
                line: 1,
                kind: ErrorKind::Cycle(composite.name.clone()),
            });
        }
        path.push(index);
        let cap = composite.pack.unwrap_or(usize::MAX);
        let (mut end, mut align, mut offsets) = (0usize, 1usize, Vec::new());
        for field in &composite.fields.clone() {
            let (size, natural) = self.size_align(&field.ty, path)?;
            let field_align = natural.min(cap).max(1);
            align = align.max(field_align);
            if composite.union {
                offsets.push(0);
                end = end.max(size);
            } else {
                let offset = bounded(end.checked_next_multiple_of(field_align), &composite.name)?;
                offsets.push(offset);
                end = bounded(offset.checked_add(size), &composite.name)?;
            }
        }
        path.pop();
        let size = bounded(end.checked_next_multiple_of(align), &composite.name)?;
        self.composites[index] = Some((size, align, offsets));
        Ok((size, align))
    }

    fn size_align(&mut self, ty: &Ty, path: &mut Vec<usize>) -> Result<(usize, usize)> {
        Ok(match ty {
            Ty::Prim(prim) => (prim.size(), prim.size()),
            Ty::Enum(index) => {
                let size = self.program.enums[*index].underlying.size();
                (size, size)
            }
            Ty::Composite(index) => self.composite(*index, path)?,
            Ty::Array(inner, count) => {
                let (size, align) = self.size_align(inner, path)?;
                (bounded(size.checked_mul(*count), "[]")?, align)
            }
        })
    }

    /// Bytes of a type.
    pub fn size(&self, ty: &Ty) -> usize {
        match ty {
            Ty::Prim(prim) => prim.size(),
            Ty::Enum(index) => self.program.enums[*index].underlying.size(),
            Ty::Composite(index) => self.composites[*index].as_ref().map_or(0, |c| c.0),
            Ty::Array(inner, count) => self.size(inner).saturating_mul(*count),
        }
    }

    fn offsets(&self, index: usize) -> &[usize] {
        self.composites[index].as_ref().map_or(&[], |c| &c.2)
    }
}

/// A size or an offset of at most [`MAX_TYPE_BYTES`].
fn bounded(value: Option<usize>, name: &str) -> Result<usize> {
    value.filter(|v| *v <= MAX_TYPE_BYTES).ok_or_else(|| Error {
        line: 1,
        kind: ErrorKind::TooLarge(format!("{name} > 4 GiB")),
    })
}

/// One line of a decoded value: a field, an element or a structure.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub depth: usize,
    pub name: String,
    pub type_name: String,
    pub offset: usize,
    pub size: usize,
    /// Empty for a structure or an array whose lines follow.
    pub value: String,
}

/// The values of a structure read from bytes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Decoded {
    pub lines: Vec<Line>,
    /// Bytes of one structure.
    pub size: usize,
    /// Structures in the bytes, one after another.
    pub records: usize,
    /// Bytes after the last whole structure.
    pub rest: usize,
}

/// Most elements of one array listed line by line.
const ARRAY_LINES: usize = 256;
/// Most structures of one value decoded.
const MAX_RECORDS: usize = 1000;
/// Most lines of one value: a value is decoded again as its header is typed,
/// and structures of arrays of structures multiply.
const MAX_LINES: usize = 20_000;
/// Most bytes of a `char` or byte array shown as its value.
const MAX_SHOWN_BYTES: usize = 1024;

/// The structures declared at the top of a program, by name, for the choice
/// of the one a value is; the last declared comes last.
pub fn roots(program: &Program) -> Vec<String> {
    program
        .composites
        .iter()
        .filter(|c| c.top && !c.name.is_empty())
        .map(|c| c.name.clone())
        .collect()
}

/// Read `bytes` as the structure `root` (the last one declared when it is
/// not found), as many times as whole structures fit.
pub fn decode(
    program: &Program,
    root: &str,
    bytes: &[u8],
    options: &Options,
) -> std::result::Result<Decoded, Error> {
    let layout = Layout::new(program)?;
    let index = program
        .composites
        .iter()
        .rposition(|c| c.top && c.name == root)
        .or_else(|| {
            program
                .composites
                .iter()
                .rposition(|c| c.top && !c.name.is_empty())
        })
        .ok_or(Error {
            line: 1,
            kind: ErrorKind::NoStructure,
        })?;
    let ty = Ty::Composite(index);
    let size = layout.size(&ty);
    let records = bytes
        .len()
        .checked_div(size)
        .map_or(1, |count| count.clamp(1, MAX_RECORDS));
    let mut decoded = Decoded {
        lines: Vec::new(),
        size,
        records,
        rest: if size == 0 || bytes.len() < size {
            0
        } else {
            bytes.len() - records * size
        },
    };
    let reader = Reader {
        program,
        layout: &layout,
        bytes,
        options,
    };
    for record in 0..records {
        if decoded.lines.len() >= MAX_LINES {
            break;
        }
        let depth = usize::from(records > 1);
        if records > 1 {
            decoded.lines.push(Line {
                depth: 0,
                name: format!("[{record}]"),
                type_name: program.composites[index].name.clone(),
                offset: record * size,
                size,
                value: String::new(),
            });
        }
        reader.fields(index, record * size, depth, &mut decoded.lines);
    }
    if decoded.lines.len() >= MAX_LINES {
        decoded.lines.push(Line {
            depth: 0,
            name: "…".into(),
            type_name: String::new(),
            offset: 0,
            size: 0,
            value: format!("{MAX_LINES}+"),
        });
    }
    Ok(decoded)
}

struct Reader<'a> {
    program: &'a Program,
    layout: &'a Layout<'a>,
    bytes: &'a [u8],
    options: &'a Options,
}

impl Reader<'_> {
    fn type_name(&self, ty: &Ty) -> String {
        match ty {
            Ty::Prim(prim) => prim.name().to_owned(),
            Ty::Enum(index) => self.program.enums[*index].name.clone(),
            Ty::Composite(index) => {
                let composite = &self.program.composites[*index];
                match (composite.name.is_empty(), composite.union) {
                    (false, _) => composite.name.clone(),
                    (true, true) => "union".into(),
                    (true, false) => "struct".into(),
                }
            }
            Ty::Array(inner, count) => format!("{}[{count}]", self.type_name(inner)),
        }
    }

    fn fields(&self, index: usize, base: usize, depth: usize, lines: &mut Vec<Line>) {
        if depth > MAX_NESTING {
            return;
        }
        let composite = &self.program.composites[index];
        for (field, offset) in composite.fields.iter().zip(self.layout.offsets(index)) {
            let at = base + offset;
            match (&field.ty, field.name.is_empty()) {
                // An anonymous member or a base: its fields are these fields.
                (Ty::Composite(inner), true) => self.fields(*inner, at, depth, lines),
                (ty, _) => self.value(&field.name, ty, at, depth, lines),
            }
        }
    }

    fn value(&self, name: &str, ty: &Ty, at: usize, depth: usize, lines: &mut Vec<Line>) {
        if lines.len() >= MAX_LINES {
            return;
        }
        let size = self.layout.size(ty);
        let mut line = Line {
            depth,
            name: name.to_owned(),
            type_name: self.type_name(ty),
            offset: at,
            size,
            value: String::new(),
        };
        match ty {
            Ty::Prim(prim) => {
                line.value = self.prim(*prim, at);
                lines.push(line);
            }
            Ty::Enum(index) => {
                let declared = &self.program.enums[*index];
                line.value = match self.integer(declared.underlying, at) {
                    Some(n) => match declared.values.iter().find(|(v, _)| *v == n) {
                        Some((_, variant)) => format!("{variant} ({n})"),
                        None => n.to_string(),
                    },
                    None => "—".into(),
                };
                lines.push(line);
            }
            Ty::Composite(index) => {
                lines.push(line);
                self.fields(*index, at, depth + 1, lines);
            }
            Ty::Array(inner, count) => match inner.as_ref() {
                // Letters read as text, bytes as hex.
                Ty::Prim(Prim::Char) => {
                    line.value = self.text(at, *count);
                    lines.push(line);
                }
                Ty::Prim(Prim::U8 | Prim::I8) => {
                    line.value = self.slice(at, *count).map_or("—".into(), |bytes| {
                        let shown = bytes.len().min(MAX_SHOWN_BYTES / 2);
                        let mut text = bytes[..shown]
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<Vec<_>>()
                            .join(" ");
                        if shown < bytes.len() {
                            text.push_str(" …");
                        }
                        text
                    });
                    lines.push(line);
                }
                _ => {
                    lines.push(line);
                    let step = self.layout.size(inner);
                    // Elements past the end of the bytes hold nothing to read.
                    let mut listed = 0;
                    for element in 0..(*count).min(ARRAY_LINES) {
                        let offset = at + element * step;
                        if lines.len() >= MAX_LINES || element > 0 && offset >= self.bytes.len() {
                            break;
                        }
                        self.value(&format!("[{element}]"), inner, offset, depth + 1, lines);
                        listed += 1;
                    }
                    // Past the limit of lines, one line at the end says so.
                    if listed < *count && lines.len() < MAX_LINES {
                        lines.push(Line {
                            depth: depth + 1,
                            name: "…".into(),
                            type_name: String::new(),
                            offset: at + listed * step,
                            size: (count - listed) * step,
                            value: format!("+{}", count - listed),
                        });
                    }
                }
            },
        }
    }

    fn slice(&self, at: usize, size: usize) -> Option<&[u8]> {
        self.bytes.get(at..at.checked_add(size)?)
    }

    fn raw<const N: usize>(&self, at: usize) -> Option<[u8; N]> {
        let mut raw: [u8; N] = self.slice(at, N)?.try_into().ok()?;
        if self.options.big_endian {
            raw.reverse();
        }
        Some(raw)
    }

    fn integer(&self, prim: Prim, at: usize) -> Option<i128> {
        Some(match prim {
            Prim::I8 | Prim::Char => i8::from_le_bytes(self.raw(at)?) as i128,
            Prim::U8 | Prim::Bool => u8::from_le_bytes(self.raw(at)?) as i128,
            Prim::I16 => i16::from_le_bytes(self.raw(at)?) as i128,
            Prim::U16 | Prim::WChar => u16::from_le_bytes(self.raw(at)?) as i128,
            Prim::I32 => i32::from_le_bytes(self.raw(at)?) as i128,
            Prim::U32 => u32::from_le_bytes(self.raw(at)?) as i128,
            Prim::I64 => i64::from_le_bytes(self.raw(at)?) as i128,
            Prim::U64 => u64::from_le_bytes(self.raw(at)?) as i128,
            Prim::F32 | Prim::F64 => return None,
        })
    }

    fn prim(&self, prim: Prim, at: usize) -> String {
        let value = match prim {
            Prim::F32 => self.raw(at).map(|raw| f32::from_le_bytes(raw).to_string()),
            Prim::F64 => self.raw(at).map(|raw| f64::from_le_bytes(raw).to_string()),
            Prim::Bool => self.integer(prim, at).map(|n| (n != 0).to_string()),
            Prim::Char => self.integer(Prim::U8, at).map(|n| letter(n as u8)),
            Prim::WChar => self.integer(prim, at).map(|n| {
                char::from_u32(n as u32)
                    .filter(|c| !c.is_control())
                    .map_or(n.to_string(), |c| format!("'{c}' ({n})"))
            }),
            _ => self.integer(prim, at).map(|n| n.to_string()),
        };
        value.unwrap_or_else(|| "—".into())
    }

    /// A `char` array: the text up to its first NUL, with other bytes escaped.
    fn text(&self, at: usize, count: usize) -> String {
        let Some(bytes) = self.slice(at, count) else {
            return "—".into();
        };
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        let cut = end > MAX_SHOWN_BYTES;
        let text: String = bytes[..end.min(MAX_SHOWN_BYTES)]
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    (b as char).to_string()
                } else {
                    format!("\\x{b:02x}")
                }
            })
            .collect();
        if cut {
            format!("\"{text}\"…")
        } else {
            format!("\"{text}\"")
        }
    }
}

fn letter(byte: u8) -> String {
    if byte.is_ascii_graphic() || byte == b' ' {
        format!("'{}' ({byte})", byte as char)
    } else {
        byte.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(source: &str, root: &str, bytes: &[u8], options: Options) -> Vec<(String, String)> {
        let program = parse(source, &options).unwrap();
        decode(&program, root, bytes, &options)
            .unwrap()
            .lines
            .into_iter()
            .map(|line| (line.name, line.value))
            .collect()
    }

    #[test]
    fn a_packed_structure_of_a_header_reads_its_bytes() {
        let source = r#"
            #pragma once
            #include <stdint.h>
            #define NAME_LEN 8 // with a comment
            /* a block
               comment */
            #pragma pack(push, 1)
            typedef struct SItem {
                DWORD id;
                float x;
                WORD count;
                char name[NAME_LEN];
            } TItem;
            #pragma pack(pop)
        "#;
        let mut bytes = Vec::new();
        bytes.extend(1001u32.to_le_bytes());
        bytes.extend(1.5f32.to_le_bytes());
        bytes.extend(7u16.to_le_bytes());
        bytes.extend(b"item1\0\0\0");
        let program = parse(source, &Options::default()).unwrap();
        let decoded = decode(&program, "SItem", &bytes, &Options::default()).unwrap();
        assert_eq!(decoded.size, 18);
        assert_eq!(
            decoded
                .lines
                .iter()
                .map(|l| (l.name.as_str(), l.offset, l.value.as_str()))
                .collect::<Vec<_>>(),
            [
                ("id", 0, "1001"),
                ("x", 4, "1.5"),
                ("count", 8, "7"),
                ("name", 10, "\"item1\"")
            ]
        );
    }

    #[test]
    fn alignment_follows_the_compiler_and_pack() {
        let source = "struct A { char c; int i; short s; };";
        let program = parse(source, &Options::default()).unwrap();
        let layout = Layout::new(&program).unwrap();
        // 1 + 3 padding + 4 + 2 + 2 padding.
        assert_eq!(layout.size(&Ty::Composite(0)), 12);
        assert_eq!(layout.offsets(0), [0, 4, 8]);
        for packed in [
            "#pragma pack(1)\nstruct A { char c; int i; short s; };",
            "struct __attribute__((packed)) A { char c; int i; short s; };",
            "struct A { char c; int i; short s; } __attribute__((packed));",
        ] {
            let program = parse(packed, &Options::default()).unwrap();
            let layout = Layout::new(&program).unwrap();
            assert_eq!(layout.size(&Ty::Composite(0)), 7, "{packed}");
            assert_eq!(layout.offsets(0), [0, 1, 5], "{packed}");
        }
        // pack(2) caps the alignment of the int at 2.
        let program = parse("#pragma pack(push,2)\nstruct A { char c; int i; };\n#pragma pack(pop)\nstruct B { char c; int i; };", &Options::default()).unwrap();
        let layout = Layout::new(&program).unwrap();
        assert_eq!(layout.size(&Ty::Composite(0)), 6);
        assert_eq!(layout.size(&Ty::Composite(1)), 8);
    }

    #[test]
    fn primitives_nesting_arrays_enums_and_unions() {
        let source = r#"
            enum class EKind : unsigned char { NONE, SWORD = 5, BOW };
            struct Pos { long x; long y; };
            union Value { int i; float f; };
            #pragma pack(push, 1)
            struct Packet {
                EKind kind;
                unsigned long long big;
                Pos pos;
                short list[2][2];
                Value value;
                bool flag;
                signed char tiny;
                BYTE raw[3];
            };
            #pragma pack(pop)
        "#;
        let mut bytes = vec![6u8];
        bytes.extend(u64::MAX.to_le_bytes());
        bytes.extend(10i32.to_le_bytes());
        bytes.extend((-20i32).to_le_bytes());
        for n in [1i16, 2, 3, 4] {
            bytes.extend(n.to_le_bytes());
        }
        bytes.extend(2.0f32.to_le_bytes());
        bytes.extend([1, 0xff, 0xab, 0xcd, 0xef]);
        let lines = values(source, "Packet", &bytes, Options::default());
        let find = |name: &str| lines.iter().find(|(n, _)| n == name).unwrap().1.clone();
        assert_eq!(find("kind"), "BOW (6)");
        assert_eq!(find("big"), u64::MAX.to_string());
        assert_eq!(find("x"), "10");
        assert_eq!(find("y"), "-20");
        assert_eq!(find("f"), "2");
        assert_eq!(find("flag"), "true");
        assert_eq!(find("tiny"), "-1");
        assert_eq!(find("raw"), "ab cd ef");
        let elements: Vec<_> = lines
            .iter()
            .filter(|(n, _)| n.starts_with('['))
            .map(|(_, v)| v.as_str())
            .collect();
        // Two rows of two shorts, then their values.
        assert_eq!(elements, ["", "1", "2", "", "3", "4"]);
    }

    #[test]
    fn long_and_byte_order_follow_the_options() {
        let source = "struct A { long a; };";
        let unix = Options {
            long_bytes: 8,
            big_endian: false,
        };
        let program = parse(source, &unix).unwrap();
        assert_eq!(Layout::new(&program).unwrap().size(&Ty::Composite(0)), 8);
        let big = Options {
            long_bytes: 4,
            big_endian: true,
        };
        assert_eq!(
            values(source, "A", &[0, 0, 1, 0], big),
            [("a".into(), "256".into())]
        );
    }

    #[test]
    fn several_records_and_missing_bytes() {
        let source = "#pragma pack(1)\nstruct R { WORD a; BYTE b; };";
        let program = parse(source, &Options::default()).unwrap();
        let decoded = decode(&program, "R", &[1, 0, 2, 3, 0, 4, 9], &Options::default()).unwrap();
        assert_eq!((decoded.records, decoded.rest), (2, 1));
        assert_eq!(decoded.lines[0].name, "[0]");
        assert_eq!(decoded.lines[4].value, "3");
        // Fewer bytes than one structure: what is missing shows as such.
        let short = decode(&program, "R", &[1], &Options::default()).unwrap();
        assert_eq!(short.lines[0].value, "—");
    }

    #[test]
    fn cpp_members_without_bytes_are_skipped() {
        let source = r#"
            namespace net {
            constexpr int SIZE = 2 * 2;
            struct Base { int id; };
            class Packet : public Base {
            public:
                Packet() : value(0) {}
                ~Packet() {}
                static const int MAX = 10;
                void reset() { value = 0; }
                uint16_t value = 3;
                std::uint8_t data[SIZE];
                struct { BYTE a; BYTE b; };
            };
            }
        "#;
        let program = parse(source, &Options::default()).unwrap();
        let names: Vec<_> = decode(&program, "Packet", &[0; 16], &Options::default())
            .unwrap()
            .lines
            .into_iter()
            .map(|l| l.name)
            .collect();
        assert_eq!(names, ["id", "value", "data", "a", "b"]);
    }

    #[test]
    fn what_cannot_be_read_says_where() {
        let error = |source: &str| parse(source, &Options::default()).unwrap_err();
        assert_eq!(
            error("struct A {\n  Unknown x;\n};").kind,
            ErrorKind::UnknownType("Unknown".into())
        );
        assert_eq!(error("struct A {\n  Unknown x;\n};").line, 2);
        assert_eq!(
            error("struct A { char *name; };").kind,
            ErrorKind::Pointer("name".into())
        );
        assert_eq!(
            error("struct A { int flags : 3; };").kind,
            ErrorKind::BitField("flags".into())
        );
        assert_eq!(
            error("struct A { int a[n]; };").kind,
            ErrorKind::ArraySize("n".into())
        );
        assert_eq!(error("int x;").kind, ErrorKind::NoStructure);
        assert!(
            error("struct A { int a; ")
                .describe(false)
                .starts_with("Wiersz 1")
        );
    }

    #[test]
    fn hostile_headers_are_refused_or_bounded() {
        let options = Options::default();
        let too_large = |source: &str| {
            matches!(
                parse(source, &options).and_then(|p| decode(&p, "", &[], &options).map(|_| ())),
                Err(Error {
                    kind: ErrorKind::TooLarge(_) | ErrorKind::ArraySize(_),
                    ..
                })
            )
        };
        // Nesting that would overflow the stack.
        let nested = format!(
            "{}int a;{} s;",
            "struct A{".repeat(20_000),
            "} s;".repeat(20_000)
        );
        assert!(too_large(&nested));
        let parens = format!(
            "#define N {}1{}\nstruct S {{ char a[N]; }};",
            "(".repeat(100_000),
            ")".repeat(100_000)
        );
        assert!(too_large(&parens));
        assert!(too_large(&format!(
            "struct S {{ int a{}; }};",
            "[1]".repeat(100)
        )));
        // Sizes past the address space, and constants past i128.
        assert!(too_large(
            "struct A { char a[16777216][16777216][65536]; };"
        ));
        assert!(too_large("#define X -(1 << 127)\nstruct S { char a[X]; };"));
        assert!(
            parse(
                "enum E { A = 0x7fffffffffffffffffffffffffffffff, B };\nstruct S { E e; };",
                &options
            )
            .is_ok()
        );
        // Arrays of arrays of structures: the lines stop at the limit.
        let program = parse(
            "struct S { int v[256]; }; struct T { S s[256]; }; struct U { T t[256]; };",
            &options,
        )
        .unwrap();
        let lines = decode(&program, "U", &vec![0; 1 << 20], &options)
            .unwrap()
            .lines;
        assert!(lines.len() <= MAX_LINES + 1, "{}", lines.len());
        // Nothing past the end of the bytes is listed element by element.
        let lines = decode(&program, "U", &[], &options).unwrap().lines;
        assert!(lines.len() < 10, "{}", lines.len());
        // A long text is cut.
        let program = parse("struct S { char s[100000]; };", &options).unwrap();
        let lines = decode(&program, "S", &vec![b'a'; 100_000], &options)
            .unwrap()
            .lines;
        assert!(lines[0].value.len() < 2 * MAX_SHOWN_BYTES);
    }
}
