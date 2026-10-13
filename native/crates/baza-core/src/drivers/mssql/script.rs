//! T-SQL text as the driver has to read it: where a batch of a script ends
//! (`GO`), what a batch leaves behind on its connection, and how a text
//! value is written so that the server reads back every character.
use crate::drivers::RowChange;
use crate::engine::Dialect;
use crate::model::Cell;
use crate::sql;
use anyhow::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Space,
    Comment,
    /// `'…'`
    Literal,
    /// `[…]` or `"…"`
    Quoted,
    /// Letters, digits, `_`, `@`, `#` and `$`: a keyword, a name, a variable
    /// or a number.
    Word,
    /// Any other single character.
    Symbol,
}

#[derive(Clone, Copy, Debug)]
struct Token<'a> {
    kind: Kind,
    text: &'a str,
    start: usize,
}

/// Split T-SQL into its lexical elements. It never fails: a literal or a
/// comment that does not end runs to the end of the text, and the server
/// says what is wrong with it.
fn tokens(sql: &str) -> Vec<Token<'_>> {
    let bytes = sql.as_bytes();
    // Bytes of non-ASCII characters count as letters, so every boundary
    // falls on an ASCII byte and is a valid place to slice the text.
    let word =
        |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'@' | b'#' | b'$') || c >= 0x80;
    // End of the quoted text that opens at `from`; a doubled closing quote
    // stays inside.
    let quoted = |from: usize, close: u8| {
        let mut at = from + 1;
        while at < bytes.len() {
            if bytes[at] == close {
                if bytes.get(at + 1) == Some(&close) {
                    at += 2;
                    continue;
                }
                return at + 1;
            }
            at += 1;
        }
        bytes.len()
    };
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        let c = bytes[at];
        let kind = if c.is_ascii_whitespace() {
            while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
                at += 1;
            }
            Kind::Space
        } else if bytes[at..].starts_with(b"--") {
            while bytes.get(at).is_some_and(|c| *c != b'\n') {
                at += 1;
            }
            Kind::Comment
        } else if bytes[at..].starts_with(b"/*") {
            // Block comments nest in T-SQL.
            let mut depth = 1usize;
            at += 2;
            while depth > 0 && at < bytes.len() {
                if bytes[at..].starts_with(b"/*") {
                    depth += 1;
                    at += 2;
                } else if bytes[at..].starts_with(b"*/") {
                    depth -= 1;
                    at += 2;
                } else {
                    at += 1;
                }
            }
            Kind::Comment
        } else if c == b'\'' {
            at = quoted(at, b'\'');
            Kind::Literal
        } else if c == b'"' {
            at = quoted(at, b'"');
            Kind::Quoted
        } else if c == b'[' {
            at = quoted(at, b']');
            Kind::Quoted
        } else if word(c) {
            while bytes.get(at).is_some_and(|c| word(*c)) {
                at += 1;
            }
            Kind::Word
        } else {
            at += 1;
            Kind::Symbol
        };
        out.push(Token {
            kind,
            text: &sql[start..at],
            start,
        });
    }
    out
}

/// One batch of a script: the text between two `GO` lines, and how many
/// times `GO n` asks for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Batch {
    pub text: String,
    pub repeat: u32,
}

/// Split a script at its `GO` lines, the way the tools of SQL Server do: `GO`
/// is not T-SQL, the server never sees it. It separates batches only as the
/// first word of a line of its own, optionally followed by a count and a
/// `--` comment; inside a literal, a comment or a quoted name it is text.
/// Batches without a statement are left out.
pub(super) fn batches(script: &str) -> Vec<Batch> {
    let tokens = tokens(script);
    let mut out = Vec::new();
    let mut start = 0usize;
    // The batch from `start` to `end`, when it holds anything to run.
    let mut push = |from: usize, to: usize, repeat: u32, first: usize, last: usize| {
        let runs = tokens[first..last]
            .iter()
            .any(|token| !matches!(token.kind, Kind::Space | Kind::Comment));
        if runs && repeat > 0 {
            out.push(Batch {
                text: script[from..to].trim().to_owned(),
                repeat,
            });
        }
    };
    let mut first = 0usize;
    let mut index = 0usize;
    while index < tokens.len() {
        let token = tokens[index];
        let opens_line = index == 0
            || (tokens[index - 1].kind == Kind::Space
                && (index == 1 || tokens[index - 1].text.contains('\n')));
        if !(token.kind == Kind::Word && token.text.eq_ignore_ascii_case("GO") && opens_line) {
            index += 1;
            continue;
        }
        // What else stands on the line of this GO.
        let mut next = index + 1;
        let mut repeat = 1u32;
        let mut counted = false;
        let mut separator = true;
        while let Some(follower) = tokens.get(next) {
            match follower.kind {
                Kind::Space if follower.text.contains('\n') => break,
                Kind::Space => {}
                Kind::Word if !counted && follower.text.bytes().all(|b| b.is_ascii_digit()) => {
                    counted = true;
                    repeat = follower.text.parse().unwrap_or(u32::MAX);
                }
                Kind::Comment if follower.text.starts_with("--") => {}
                _ => {
                    separator = false;
                    break;
                }
            }
            next += 1;
        }
        if separator {
            push(start, token.start, repeat, first, index);
            start = tokens.get(next).map_or(script.len(), |after| after.start);
            first = next;
            index = next;
        } else {
            index += 1;
        }
    }
    push(start, script.len(), 1, first, tokens.len());
    out
}

/// What a batch may leave behind on the connection that ran it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Effects {
    /// It may end inside a transaction: it names one, or calls code that
    /// may open one.
    pub transaction: bool,
    /// It changes what outlives even a scope of its own: the database in
    /// use, a cursor, or whatever a called procedure does.
    pub session: bool,
    /// It sets an option or makes a temporary table: state that ends with
    /// the batch when it runs in a scope of its own, and that the next
    /// batch of a script expects to find.
    pub scoped: bool,
    /// It uses local variables. The server reports a row count for every
    /// assignment, and nothing tells those apart from the rows a statement
    /// changed.
    pub variables: bool,
}

/// First words of the statements a batch can begin with. A batch that
/// begins with anything else calls a procedure by its bare name.
const STATEMENTS: &[&str] = &[
    "ADD",
    "ALTER",
    "BACKUP",
    "BEGIN",
    "BULK",
    "CHECKPOINT",
    "CLOSE",
    "COMMIT",
    "CREATE",
    "DBCC",
    "DEALLOCATE",
    "DECLARE",
    "DELETE",
    "DENY",
    "DISABLE",
    "DROP",
    "ENABLE",
    "END",
    "EXEC",
    "EXECUTE",
    "FETCH",
    "GOTO",
    "GRANT",
    "IF",
    "INSERT",
    "KILL",
    "MERGE",
    "OPEN",
    "PRINT",
    "RAISERROR",
    "RECONFIGURE",
    "RESTORE",
    "RETURN",
    "REVERT",
    "REVOKE",
    "ROLLBACK",
    "SAVE",
    "SELECT",
    "SET",
    "SETUSER",
    "SHUTDOWN",
    "THROW",
    "TRUNCATE",
    "UPDATE",
    "USE",
    "WAITFOR",
    "WHILE",
    "WITH",
];

/// Read what a batch does to its session. It errs on the careful side: a
/// word that only looks like one of these costs a new connection, nothing
/// else.
pub(super) fn effects(batch: &str) -> Effects {
    let tokens: Vec<Token<'_>> = tokens(batch)
        .into_iter()
        .filter(|token| !matches!(token.kind, Kind::Space | Kind::Comment))
        .collect();
    let is = |token: Option<&Token<'_>>, text: &str| {
        token.is_some_and(|token| token.kind == Kind::Symbol && token.text == text)
    };
    let mut effects = Effects::default();
    let first = tokens
        .iter()
        .find(|token| !(token.kind == Kind::Symbol && matches!(token.text, ";" | "(")));
    if let Some(first) = first
        && first.kind != Kind::Symbol
        && !STATEMENTS
            .iter()
            .any(|statement| first.text.eq_ignore_ascii_case(statement))
    {
        effects.session = true;
        effects.transaction = true;
    }
    for (index, token) in tokens.iter().enumerate() {
        if token.kind != Kind::Word {
            continue;
        }
        let text = token.text;
        if text.starts_with('@') {
            // `@@ROWCOUNT` and its kind are functions of the server.
            effects.variables |= !text.starts_with("@@");
            continue;
        }
        if text.starts_with('#') {
            // A `##global` temporary table is meant to be seen by others.
            effects.scoped |= !text.starts_with("##");
            continue;
        }
        match text.to_ascii_uppercase().as_str() {
            "TRAN" | "TRANSACTION" | "COMMIT" | "ROLLBACK" | "IMPLICIT_TRANSACTIONS" => {
                effects.transaction = true;
            }
            "EXEC" | "EXECUTE" => {
                effects.session = true;
                effects.transaction = true;
            }
            "USE" | "CURSOR" | "OPEN" | "DBCC" | "CONTEXT_INFO" | "REVERT" | "SETUSER" => {
                effects.session = true;
            }
            "SET" => {
                let target = tokens.get(index + 1);
                let option = target.is_some_and(|target| {
                    target.kind == Kind::Word && !target.text.starts_with('@')
                });
                // `SET column = …`, `SET t.column = …` and `SET column += …`
                // belong to an UPDATE; `SET NOCOUNT ON` is an option.
                let after = tokens.get(index + 2);
                let assigns = is(after, "=")
                    || is(after, ".")
                    || (after.is_some_and(|token| {
                        token.kind == Kind::Symbol && "+-*/%&|^".contains(token.text)
                    }) && is(tokens.get(index + 3), "="));
                effects.scoped |= option && !assigns;
            }
            _ => {}
        }
    }
    effects
}

/// Whether the server would drop characters of this text inside a literal:
/// a backslash at the end of a line continues the literal on the next line,
/// and both the backslash and the line break are gone.
fn continues_line(text: &str) -> bool {
    text.contains("\\\n") || text.contains("\\\r")
}

/// A text value as a literal. `N'…'` with doubled quotes is all T-SQL needs,
/// except for a backslash right before a line break: there the literal is
/// closed after the backslash and the rest is appended, as `NVARCHAR(MAX)`
/// so that nothing is cut at 4000 characters.
pub(super) fn text_literal(text: &str) -> String {
    let quote = |part: &str| format!("N'{}'", part.replace('\'', "''"));
    if !continues_line(text) {
        return quote(text);
    }
    let mut parts = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\\' && matches!(bytes.get(index + 1), Some(b'\n' | b'\r')) {
            parts.push(&text[start..=index]);
            start = index + 1;
        }
    }
    parts.push(&text[start..]);
    let mut out = format!("(CAST({} AS NVARCHAR(MAX))", quote(parts[0]));
    for part in &parts[1..] {
        out.push_str(" + ");
        out.push_str(&quote(part));
    }
    out.push(')');
    out
}

/// A cell as a literal of T-SQL.
pub(super) fn literal(dialect: &Dialect, cell: &Cell) -> Result<String> {
    match cell {
        Cell::Text(text) | Cell::Json(text) if continues_line(text) => {
            anyhow::ensure!(!text.contains('\0'), "text_nul_unsupported");
            Ok(text_literal(text))
        }
        _ => sql::literal(dialect, cell),
    }
}

/// The statement that makes one change of one record. It is the statement
/// every SQL engine gets, with the text values T-SQL would shorten written
/// the long way.
pub(super) fn change_statement(dialect: &Dialect, change: &RowChange) -> Result<String> {
    // The shared builder writes the statement around stand-ins for those
    // values, which are then exchanged for the real literals.
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let mut literals = Vec::new();
    let mut guard = |cell: &Cell| match cell {
        Cell::Text(text) | Cell::Json(text) if continues_line(text) && !text.contains('\0') => {
            literals.push(text_literal(text));
            Cell::Text(format!("baza-{nonce}-{}", literals.len() - 1))
        }
        other => other.clone(),
    };
    let pairs = |pairs: &[(String, Cell)], guard: &mut dyn FnMut(&Cell) -> Cell| {
        pairs
            .iter()
            .map(|(name, cell)| (name.clone(), guard(cell)))
            .collect::<Vec<_>>()
    };
    let guarded = match change {
        RowChange::UpdateCell {
            table,
            column,
            old,
            new,
            key,
        } => RowChange::UpdateCell {
            table: table.clone(),
            column: column.clone(),
            old: guard(old),
            new: guard(new),
            key: pairs(key, &mut guard),
        },
        RowChange::InsertRow { table, values } => RowChange::InsertRow {
            table: table.clone(),
            values: pairs(values, &mut guard),
        },
        RowChange::DeleteRow { table, key } => RowChange::DeleteRow {
            table: table.clone(),
            key: pairs(key, &mut guard),
        },
    };
    let mut statement = sql::change_statement(dialect, &guarded)?;
    for (index, literal) in literals.iter().enumerate() {
        statement = statement.replace(&format!("N'baza-{nonce}-{index}'"), literal);
    }
    Ok(statement)
}

/// Whether values of this type (as `columns` reports it) can stand on a side
/// of `=`. XML and the two old large text types cannot.
pub(super) fn comparable(data_type: &str) -> bool {
    !matches!(
        data_type.trim().to_ascii_lowercase().as_str(),
        "xml" | "text" | "ntext"
    )
}

/// The conflict check of a cell edit for a column whose type cannot be
/// compared: `statement` ends with `AND [column] = old`, and the column is
/// compared as the text it is shown as instead.
pub(super) fn compared_as_text(
    dialect: &Dialect,
    statement: String,
    column: &str,
    old: &Cell,
) -> Result<String> {
    let name = sql::quote_identifier(dialect, column)?;
    let old = literal(dialect, old)?;
    Ok(
        match statement.strip_suffix(&format!(" AND {name} = {old}")) {
            Some(head) => format!("{head} AND CAST({name} AS NVARCHAR(MAX)) = {old}"),
            None => statement,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::super::DIALECT;
    use super::*;
    use crate::model::TableRef;

    fn texts(script: &str) -> Vec<(String, u32)> {
        batches(script)
            .into_iter()
            .map(|batch| (batch.text, batch.repeat))
            .collect()
    }

    #[test]
    fn a_script_is_split_at_its_go_lines() {
        assert_eq!(texts("SELECT 1"), [("SELECT 1".to_owned(), 1)]);
        assert_eq!(
            texts("CREATE TABLE t (a INT)\nGO\nINSERT INTO t VALUES (1)\ngo\n"),
            [
                ("CREATE TABLE t (a INT)".to_owned(), 1),
                ("INSERT INTO t VALUES (1)".to_owned(), 1)
            ]
        );
        // Windows line ends, a count and a comment on the line of GO.
        assert_eq!(
            texts(
                "INSERT INTO t VALUES (1)\r\nGO 3 -- three times\r\nSELECT * FROM t\r\n  Go  \r\n"
            ),
            [
                ("INSERT INTO t VALUES (1)".to_owned(), 3),
                ("SELECT * FROM t".to_owned(), 1)
            ]
        );
        // What the editor's own splitting at `;` leaves in front of a batch.
        assert_eq!(texts("GO\nSELECT 2"), [("SELECT 2".to_owned(), 1)]);
        assert_eq!(texts("  GO"), []);
        assert_eq!(texts("GO\nGO\n-- nothing to run\nGO"), []);
        assert_eq!(
            texts("SELECT 1\nGO 0\nSELECT 2"),
            [("SELECT 2".to_owned(), 1)]
        );
        // A batch keeps its own comments.
        assert_eq!(
            texts("-- header\nSELECT 1\nGO"),
            [("-- header\nSELECT 1".to_owned(), 1)]
        );
    }

    #[test]
    fn go_is_only_a_separator_on_a_line_of_its_own() {
        for script in [
            "SELECT go FROM t",
            "SELECT 1 AS GO",
            "SELECT 'a\nGO\nb'",
            "SELECT 1 /* a\nGO\nb */",
            "SELECT [a\nGO\nb] FROM t",
            "SELECT \"a\nGO\nb\" FROM t",
            "SELECT 1 -- GO",
            "GOTO done",
            "SELECT 1\nGO;",
            "SELECT 1\nGO SELECT 2",
            "SELECT 1\nGO 2 3",
            "SELECT 1 /* x */ GO",
            "SELECT N'it''s\nGO\n' AS x",
            "SELECT 1 /* a /* nested */\nGO\n*/",
        ] {
            assert_eq!(batches(script).len(), 1, "{script}");
            assert_eq!(batches(script)[0].text, script.trim(), "{script}");
        }
        // Text that never ends is one batch: the server names the error.
        assert_eq!(batches("SELECT 'open\nGO\nSELECT 2").len(), 1);
        assert_eq!(batches("SELECT 1 /* open\nGO\nSELECT 2").len(), 1);
        // Polish text keeps its bytes; the split happens on ASCII only.
        assert_eq!(
            texts("SELECT N'Zażółć gęślą' AS jaźń\nGO\nSELECT 'ż'"),
            [
                ("SELECT N'Zażółć gęślą' AS jaźń".to_owned(), 1),
                ("SELECT 'ż'".to_owned(), 1)
            ]
        );
    }

    #[test]
    fn a_batch_is_read_for_what_it_leaves_on_its_connection() {
        let none = Effects::default();
        for plain in [
            "SELECT * FROM [dbo].[t] WHERE [a] = N'USE x; BEGIN TRAN' ORDER BY 1",
            "UPDATE t SET a = 1, b = N'SET NOCOUNT ON' WHERE id = 2",
            "UPDATE t SET t.a = 1 WHERE id = 2",
            "UPDATE dbo.t SET [nocount] = 1",
            "UPDATE t SET a += 1",
            "UPDATE t SET a -= 1",
            "INSERT INTO t (a) VALUES (1); DELETE FROM t WHERE a = 2",
            "WITH c AS (SELECT 1 AS n) SELECT * FROM c",
            ";WITH c AS (SELECT 1 AS n) SELECT * FROM c",
            "(SELECT 1) UNION (SELECT 2)",
            "CREATE TABLE [dbo].[t] (\n  [id] INT NOT NULL,\n  PRIMARY KEY ([id])\n)",
            "DROP TABLE dbo.t -- EXEC",
            "SELECT @@VERSION, @@SPID /* USE master */",
            "MERGE t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET t.a = s.a;",
            "SELECT * INTO ##shared FROM t",
            "WAITFOR DELAY '00:01:00'",
            "IF OBJECT_ID('dbo.t') IS NOT NULL DROP TABLE dbo.t",
        ] {
            assert_eq!(effects(plain), none, "{plain}");
        }
        let scoped = Effects {
            scoped: true,
            ..none
        };
        for text in [
            "SET NOCOUNT ON; SELECT 1",
            "set rowcount 5",
            "SET IDENTITY_INSERT dbo.t ON",
            "SET LANGUAGE N'polski'",
            "SET LOCK_TIMEOUT -1",
            "SET DATEFORMAT dmy",
            "SELECT 1 AS a INTO #t",
            "CREATE TABLE #t (a INT)",
            "SELECT * FROM #t",
        ] {
            assert_eq!(effects(text), scoped, "{text}");
        }
        let transaction = Effects {
            transaction: true,
            ..none
        };
        for text in [
            "BEGIN TRANSACTION; UPDATE t SET a = 1",
            "begin tran",
            "COMMIT",
            "ROLLBACK",
            "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION",
        ] {
            assert_eq!(effects(text), transaction, "{text}");
        }
        assert_eq!(
            effects("SET TRANSACTION ISOLATION LEVEL SNAPSHOT"),
            Effects {
                transaction: true,
                scoped: true,
                ..none
            }
        );
        assert_eq!(
            effects("SET IMPLICIT_TRANSACTIONS ON"),
            Effects {
                transaction: true,
                scoped: true,
                ..none
            }
        );
        let session = Effects {
            session: true,
            ..none
        };
        for text in ["USE master", "use [other]; SELECT 1", "DBCC TRACEON (3604)"] {
            assert_eq!(effects(text), session, "{text}");
        }
        assert_eq!(
            effects("SET CONTEXT_INFO 0x01"),
            Effects {
                session: true,
                scoped: true,
                ..none
            }
        );
        // Whatever a procedure does is not written in the batch.
        let called = Effects {
            session: true,
            transaction: true,
            ..none
        };
        for text in [
            "EXEC dbo.close_month",
            "execute sp_who2",
            "sp_who2",
            "dbo.close_month 2026, 10",
            "INSERT INTO t EXEC dbo.report",
        ] {
            assert_eq!(effects(text), called, "{text}");
        }
        // Variables: a row count of the batch says nothing.
        assert_eq!(
            effects("DECLARE @n INT = 5; UPDATE t SET a = @n"),
            Effects {
                variables: true,
                ..none
            }
        );
        assert_eq!(
            effects("DECLARE @n INT; SET @n = 5"),
            Effects {
                variables: true,
                ..none
            }
        );
        assert_eq!(
            effects("DECLARE c CURSOR FOR SELECT 1"),
            Effects {
                session: true,
                ..none
            }
        );
    }

    #[test]
    fn a_text_literal_keeps_a_backslash_at_the_end_of_a_line() {
        assert_eq!(text_literal("it's"), "N'it''s'");
        assert_eq!(text_literal(""), "N''");
        assert_eq!(text_literal("C:\\dane\\"), "N'C:\\dane\\'");
        assert_eq!(text_literal("a\nb\\c"), "N'a\nb\\c'");
        // The server would read `N'a\<line break>b'` as `ab`.
        assert_eq!(
            text_literal("a\\\nb"),
            "(CAST(N'a\\' AS NVARCHAR(MAX)) + N'\nb')"
        );
        assert_eq!(
            text_literal("x\\\r\ny'z\\\n"),
            "(CAST(N'x\\' AS NVARCHAR(MAX)) + N'\r\ny''z\\' + N'\n')"
        );
        assert_eq!(
            text_literal("\\\n\\\n"),
            "(CAST(N'\\' AS NVARCHAR(MAX)) + N'\n\\' + N'\n')"
        );
        assert_eq!(
            literal(&DIALECT, &Cell::Text("a\\\nb".into())).unwrap(),
            "(CAST(N'a\\' AS NVARCHAR(MAX)) + N'\nb')"
        );
        assert_eq!(
            literal(&DIALECT, &Cell::Text("plain".into())).unwrap(),
            "N'plain'"
        );
        assert_eq!(literal(&DIALECT, &Cell::Null).unwrap(), "NULL");
        assert!(literal(&DIALECT, &Cell::Text("a\\\n\0".into())).is_err());
    }

    #[test]
    fn a_change_is_the_shared_statement_with_safe_text() {
        let table = TableRef {
            schema: "dbo".into(),
            name: "notes".into(),
            kind: "table".into(),
        };
        let key = vec![("id".to_owned(), Cell::Integer("7".into()))];
        let plain = RowChange::UpdateCell {
            table: table.clone(),
            column: "body".into(),
            old: Cell::Text("old".into()),
            new: Cell::Text("it's new".into()),
            key: key.clone(),
        };
        assert_eq!(
            change_statement(&DIALECT, &plain).unwrap(),
            "UPDATE [dbo].[notes] SET [body] = N'it''s new' WHERE [id] = 7 AND [body] = N'old'"
        );
        assert_eq!(
            change_statement(&DIALECT, &plain).unwrap(),
            sql::change_statement(&DIALECT, &plain).unwrap()
        );
        let continued = RowChange::UpdateCell {
            table: table.clone(),
            column: "body".into(),
            old: Cell::Text("cd \\\nls".into()),
            new: Cell::Text("cd \\\npwd".into()),
            key: key.clone(),
        };
        assert_eq!(
            change_statement(&DIALECT, &continued).unwrap(),
            "UPDATE [dbo].[notes] SET [body] = (CAST(N'cd \\' AS NVARCHAR(MAX)) + N'\npwd') WHERE [id] = 7 AND [body] = (CAST(N'cd \\' AS NVARCHAR(MAX)) + N'\nls')"
        );
        let inserted = RowChange::InsertRow {
            table: table.clone(),
            values: vec![
                ("id".into(), Cell::Integer("8".into())),
                ("body".into(), Cell::Text("a\\\r\nb".into())),
            ],
        };
        assert_eq!(
            change_statement(&DIALECT, &inserted).unwrap(),
            "INSERT INTO [dbo].[notes] ([id], [body]) VALUES (8, (CAST(N'a\\' AS NVARCHAR(MAX)) + N'\r\nb'))"
        );
        // A type that cannot be compared is compared as its text; a NULL
        // and any other statement stay as they are.
        assert!(comparable("nvarchar(max)") && comparable("int") && comparable("sql_variant"));
        assert!(!comparable("xml") && !comparable("NTEXT") && !comparable("text"));
        let xml = RowChange::UpdateCell {
            table: table.clone(),
            column: "body".into(),
            old: Cell::Text("<a>it's</a>".into()),
            new: Cell::Text("<b/>".into()),
            key: key.clone(),
        };
        assert_eq!(
            compared_as_text(
                &DIALECT,
                change_statement(&DIALECT, &xml).unwrap(),
                "body",
                &Cell::Text("<a>it's</a>".into())
            )
            .unwrap(),
            "UPDATE [dbo].[notes] SET [body] = N'<b/>' WHERE [id] = 7 AND CAST([body] AS NVARCHAR(MAX)) = N'<a>it''s</a>'"
        );
        let unchanged =
            "UPDATE [dbo].[notes] SET [body] = N'<b/>' WHERE [id] = 7 AND [body] IS NULL";
        assert_eq!(
            compared_as_text(&DIALECT, unchanged.into(), "body", &Cell::Text("x".into())).unwrap(),
            unchanged
        );
        // The rules of the shared builder still hold.
        let keyless = RowChange::DeleteRow {
            table,
            key: Vec::new(),
        };
        assert_eq!(
            change_statement(&DIALECT, &keyless)
                .unwrap_err()
                .to_string(),
            "primary_key_required"
        );
    }
}
