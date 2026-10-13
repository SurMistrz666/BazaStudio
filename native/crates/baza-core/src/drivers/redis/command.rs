//! The text of the console: lines of commands split into words the way
//! `redis-cli` does, and the commands a graphical client does not send
//! without being asked twice.
use anyhow::{Result, bail};

/// Word in front of a command that says "I mean it": the console's
/// confirmation for a command that removes data or stops the server.
pub(super) const CONFIRM: &str = "CONFIRM";

/// One command of the console text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Line {
    /// The line as it was typed, without the confirmation word.
    pub text: String,
    /// Its words: the command name first.
    pub words: Vec<Vec<u8>>,
    /// The line started with [`CONFIRM`].
    pub confirmed: bool,
}

impl Line {
    /// The command name in upper case.
    pub(super) fn name(&self) -> String {
        upper(self.words.first())
    }
    /// The second word in upper case: the subcommand of `CLIENT`, `CONFIG`…
    pub(super) fn sub(&self) -> String {
        upper(self.words.get(1))
    }
}

fn upper(word: Option<&Vec<u8>>) -> String {
    word.map(|word| String::from_utf8_lossy(word).to_ascii_uppercase())
        .unwrap_or_default()
}

/// Split one line into words.
///
/// * Words are separated by blanks.
/// * `"…"` is one word in which `\n`, `\r`, `\t`, `\b`, `\a`, `\\`, `\"` and
///   `\xHH` (one byte) are escapes; any other escaped character stands for
///   itself.
/// * `'…'` is one word taken literally, in which only `\'` is an escape.
/// * A closing quote is followed by a blank or the end of the line.
pub(super) fn words(line: &str) -> Result<Vec<Vec<u8>>> {
    let bytes = line.as_bytes();
    let mut words = Vec::new();
    let mut at = 0;
    loop {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at >= bytes.len() {
            return Ok(words);
        }
        let mut word = Vec::new();
        match bytes[at] {
            b'"' => {
                at += 1;
                loop {
                    let Some(&byte) = bytes.get(at) else {
                        bail!("redis_quote_unterminated");
                    };
                    at += 1;
                    match byte {
                        b'"' => break,
                        b'\\' => {
                            let Some(&escaped) = bytes.get(at) else {
                                bail!("redis_quote_unterminated");
                            };
                            at += 1;
                            let hex = |index: usize| {
                                bytes
                                    .get(index)
                                    .and_then(|digit| (*digit as char).to_digit(16))
                            };
                            match escaped {
                                b'x' => match (hex(at), hex(at + 1)) {
                                    (Some(high), Some(low)) => {
                                        word.push((high * 16 + low) as u8);
                                        at += 2;
                                    }
                                    _ => word.push(b'x'),
                                },
                                b'n' => word.push(b'\n'),
                                b'r' => word.push(b'\r'),
                                b't' => word.push(b'\t'),
                                b'b' => word.push(0x08),
                                b'a' => word.push(0x07),
                                other => word.push(other),
                            }
                        }
                        other => word.push(other),
                    }
                }
                if bytes
                    .get(at)
                    .is_some_and(|byte| !byte.is_ascii_whitespace())
                {
                    bail!("redis_quote_unterminated");
                }
            }
            b'\'' => {
                at += 1;
                loop {
                    let Some(&byte) = bytes.get(at) else {
                        bail!("redis_quote_unterminated");
                    };
                    at += 1;
                    match byte {
                        b'\'' => break,
                        b'\\' if bytes.get(at) == Some(&b'\'') => {
                            word.push(b'\'');
                            at += 1;
                        }
                        other => word.push(other),
                    }
                }
                if bytes
                    .get(at)
                    .is_some_and(|byte| !byte.is_ascii_whitespace())
                {
                    bail!("redis_quote_unterminated");
                }
            }
            _ => {
                while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
                    word.push(bytes[at]);
                    at += 1;
                }
            }
        }
        words.push(word);
    }
}

/// The commands of a console text: one per line. Empty lines and lines that
/// start with `#` are skipped.
pub(super) fn lines(text: &str) -> Result<Vec<Line>> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut words = words(trimmed)?;
        if words.is_empty() {
            continue;
        }
        // The confirmation word, bare, in front of a command.
        let confirmed = words.len() > 1
            && trimmed
                .get(..CONFIRM.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(CONFIRM))
            && trimmed[CONFIRM.len()..].starts_with(char::is_whitespace);
        let text = if confirmed {
            words.remove(0);
            trimmed[CONFIRM.len()..].trim_start().to_owned()
        } else {
            trimmed.to_owned()
        };
        lines.push(Line {
            text,
            words,
            confirmed,
        });
    }
    Ok(lines)
}

/// One word as the console reads it back: bare when it needs no quotes,
/// otherwise in double quotes with escapes.
pub(super) fn quote(word: &[u8]) -> String {
    let plain = std::str::from_utf8(word).is_ok_and(|text| {
        !text.is_empty()
            && text.chars().all(|c| {
                !c.is_whitespace() && !c.is_control() && !matches!(c, '"' | '\'' | '\\' | '#')
            })
    });
    if plain {
        return String::from_utf8_lossy(word).into_owned();
    }
    let mut out = String::with_capacity(word.len() + 2);
    out.push('"');
    let mut rest = word;
    while !rest.is_empty() {
        // Text is written as it is, whatever its language; a byte that is
        // not part of valid text is written as `\xHH`.
        let (valid, after) = match std::str::from_utf8(rest) {
            Ok(text) => (text, &rest[rest.len()..]),
            Err(error) => {
                let (valid, after) = rest.split_at(error.valid_up_to());
                (std::str::from_utf8(valid).unwrap_or_default(), after)
            }
        };
        for character in valid.chars() {
            match character {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c.is_control() => {
                    let mut buffer = [0u8; 4];
                    for byte in c.encode_utf8(&mut buffer).bytes() {
                        out.push_str(&format!("\\x{byte:02x}"));
                    }
                }
                c => out.push(c),
            }
        }
        rest = after;
        if let Some((&byte, after)) = rest.split_first() {
            out.push_str(&format!("\\x{byte:02x}"));
            rest = after;
        }
    }
    out.push('"');
    out
}

/// A command as the console reads it back.
pub(super) fn text<I, A>(words: I) -> String
where
    I: IntoIterator<Item = A>,
    A: AsRef<[u8]>,
{
    words
        .into_iter()
        .map(|word| quote(word.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// What the console does with a command before it reaches the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Guard {
    /// Sent as it is.
    Run,
    /// Never sent: the code says why.
    Refuse(&'static str),
    /// Sent only behind [`CONFIRM`]; the code says what it would do.
    Confirm(&'static str),
    /// `KEYS`: sent when the database is small, otherwise behind `CONFIRM`.
    Keys,
    /// `SELECT`: sent, and the console remembers the database.
    Select,
}

fn number(word: Option<&Vec<u8>>) -> Option<f64> {
    std::str::from_utf8(word?).ok()?.trim().parse().ok()
}

/// Position of an option word (`BLOCK`) among the words of a command.
fn option(words: &[Vec<u8>], name: &str) -> Option<usize> {
    words
        .iter()
        .position(|word| word.eq_ignore_ascii_case(name.as_bytes()))
}

/// Decide what to do with a command.
///
/// Refused, whatever the person says: commands that turn the connection
/// into a stream of messages (`MONITOR`, `SUBSCRIBE`…), which a grid of one
/// reply cannot show; commands that change who the connection is (`AUTH`,
/// `HELLO`, `RESET`, `QUIT`, `CLIENT REPLY`), which belong to the profile;
/// and commands that wait without an end (`BLPOP key 0`).
///
/// Behind `CONFIRM`: commands that remove every key, stop or reconfigure
/// the server, or hold it up for everybody.
pub(super) fn guard(line: &Line) -> Guard {
    let name = line.name();
    let sub = line.sub();
    let words = &line.words;
    let last = number(words.last());
    match name.as_str() {
        "MONITOR" | "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "UNSUBSCRIBE" | "PUNSUBSCRIBE"
        | "SUNSUBSCRIBE" | "SYNC" | "PSYNC" | "REPLCONF" => Guard::Refuse("redis_stream_refused"),
        "AUTH" | "HELLO" | "RESET" | "QUIT" => Guard::Refuse("redis_session_refused"),
        "CLIENT" if matches!(sub.as_str(), "REPLY" | "TRACKING" | "CACHING") => {
            Guard::Refuse("redis_session_refused")
        }
        "SELECT" => Guard::Select,
        "KEYS" => Guard::Keys,
        // The time to wait is the last word; zero waits for ever.
        "BLPOP" | "BRPOP" | "BRPOPLPUSH" | "BLMOVE" | "BZPOPMIN" | "BZPOPMAX"
            if last == Some(0.0) =>
        {
            Guard::Refuse("redis_wait_forever")
        }
        // … or the first one after the name.
        "BLMPOP" | "BZMPOP" if number(words.get(1)) == Some(0.0) => {
            Guard::Refuse("redis_wait_forever")
        }
        "XREAD" | "XREADGROUP"
            if option(words, "BLOCK").is_some_and(|at| number(words.get(at + 1)) == Some(0.0)) =>
        {
            Guard::Refuse("redis_wait_forever")
        }
        "WAIT" | "WAITAOF" if last == Some(0.0) => Guard::Refuse("redis_wait_forever"),
        "FLUSHALL" | "FLUSHDB" | "SWAPDB" | "MIGRATE" => Guard::Confirm("redis_confirm_data_loss"),
        "DEBUG" if matches!(sub.as_str(), "OBJECT" | "HELP" | "DIGEST" | "DIGEST-VALUE") => {
            Guard::Run
        }
        "SHUTDOWN" | "DEBUG" | "REPLICAOF" | "SLAVEOF" | "FAILOVER" | "SAVE" | "PFDEBUG" => {
            Guard::Confirm("redis_confirm_server")
        }
        "CLUSTER"
            if !matches!(
                sub.as_str(),
                "INFO"
                    | "NODES"
                    | "SLOTS"
                    | "SHARDS"
                    | "KEYSLOT"
                    | "COUNTKEYSINSLOT"
                    | "GETKEYSINSLOT"
                    | "MYID"
                    | "MYSHARDID"
                    | "LINKS"
                    | "REPLICAS"
                    | "SLAVES"
                    | "COUNT-FAILURE-REPORTS"
                    | "HELP"
            ) =>
        {
            Guard::Confirm("redis_confirm_server")
        }
        "CONFIG" if matches!(sub.as_str(), "SET" | "REWRITE" | "RESETSTAT") => {
            Guard::Confirm("redis_confirm_server")
        }
        "ACL" if matches!(sub.as_str(), "SETUSER" | "DELUSER" | "LOAD" | "SAVE") => {
            Guard::Confirm("redis_confirm_server")
        }
        "CLIENT" if matches!(sub.as_str(), "KILL" | "PAUSE" | "NO-EVICT" | "NO-TOUCH") => {
            Guard::Confirm("redis_confirm_server")
        }
        "MODULE" if matches!(sub.as_str(), "LOAD" | "LOADEX" | "UNLOAD") => {
            Guard::Confirm("redis_confirm_server")
        }
        "SCRIPT" if sub == "FLUSH" => Guard::Confirm("redis_confirm_server"),
        "FUNCTION" if matches!(sub.as_str(), "FLUSH" | "DELETE" | "RESTORE") => {
            Guard::Confirm("redis_confirm_server")
        }
        _ => Guard::Run,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_words(line: &str) -> Vec<String> {
        words(line)
            .unwrap()
            .into_iter()
            .map(|word| String::from_utf8(word).unwrap())
            .collect()
    }

    #[test]
    fn a_line_is_split_like_redis_cli_does() {
        assert_eq!(text_words("SET key value"), ["SET", "key", "value"]);
        assert_eq!(text_words("  GET\tkey  "), ["GET", "key"]);
        assert_eq!(
            text_words(r#"SET "my key" "value with spaces""#),
            ["SET", "my key", "value with spaces"]
        );
        assert_eq!(
            text_words(r#"SET k "say \"hi\"\n\ttab \\ back""#),
            ["SET", "k", "say \"hi\"\n\ttab \\ back"]
        );
        // Single quotes take everything literally but an escaped quote.
        assert_eq!(
            text_words(r#"SET k 'it\'s \n "raw"'"#),
            ["SET", "k", "it's \\n \"raw\""]
        );
        // Text in any language, an empty word, a quote inside a bare word.
        assert_eq!(
            text_words(r#"SET zażółć "gęślą jaźń 🦀" """#),
            ["SET", "zażółć", "gęślą jaźń 🦀", ""]
        );
        assert_eq!(text_words("SET a it's"), ["SET", "a", "it's"]);
        assert_eq!(text_words(""), Vec::<String>::new());
    }

    #[test]
    fn escapes_write_any_byte() {
        assert_eq!(
            words(r#"SET "\xff\x00\x7f" "\xZZ\q""#).unwrap(),
            vec![b"SET".to_vec(), vec![0xff, 0x00, 0x7f], b"xZZq".to_vec()]
        );
        assert_eq!(words(r#"ECHO "\a\b""#).unwrap()[1], vec![0x07, 0x08]);
    }

    #[test]
    fn quotes_that_do_not_close_are_an_error() {
        for line in [
            r#"SET k "open"#,
            "SET k 'open",
            r#"SET k "closed"tail"#,
            "SET k 'closed'tail",
            r#"SET k "ends in a backslash\"#,
        ] {
            assert_eq!(
                words(line).unwrap_err().to_string(),
                "redis_quote_unterminated",
                "{line}"
            );
        }
    }

    #[test]
    fn a_text_is_one_command_per_line() {
        let parsed = lines("# seed\nSET a 1\n\n  GET a  \r\nconfirm flushdb\n#done").unwrap();
        assert_eq!(
            parsed
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["SET a 1", "GET a", "flushdb"]
        );
        assert_eq!(parsed[0].name(), "SET");
        assert!(!parsed[1].confirmed && parsed[2].confirmed);
        assert_eq!(parsed[2].name(), "FLUSHDB");
        // The word alone is a command name like any other.
        assert!(!lines("CONFIRM").unwrap()[0].confirmed);
        assert!(lines("  \n# only a comment").unwrap().is_empty());
    }

    #[test]
    fn what_is_written_reads_back_as_the_same_bytes() {
        for word in [
            b"plain:key".to_vec(),
            b"".to_vec(),
            b"with space".to_vec(),
            "zażółć \"gęślą\" 🦀".as_bytes().to_vec(),
            b"back\\slash and 'quote'".to_vec(),
            b"line\nfeed\ttab\r".to_vec(),
            vec![0xff, 0x00, b'a', 0xc3, 0x28, 0x1b],
            b"#comment-like".to_vec(),
            (0..=255).collect(),
        ] {
            let written = text([b"SET".as_slice(), &word]);
            let read = words(&written).unwrap();
            assert_eq!(read, vec![b"SET".to_vec(), word.clone()], "{written}");
            // One line, whatever the word holds.
            assert!(
                !written.contains('\n') && !written.contains('\0'),
                "{written}"
            );
        }
        assert_eq!(text(["SET", "user:1", "Jan Kot"]), "SET user:1 \"Jan Kot\"");
        assert_eq!(quote(&[0xff, 0x00]), "\"\\xff\\x00\"");
    }

    fn decide(line: &str) -> Guard {
        guard(&lines(line).unwrap()[0])
    }

    #[test]
    fn commands_that_stream_or_change_the_session_are_refused() {
        for line in [
            "MONITOR",
            "subscribe news",
            "PSUBSCRIBE news.*",
            "SSUBSCRIBE shard",
            "UNSUBSCRIBE",
            "SYNC",
            "PSYNC ? -1",
        ] {
            assert_eq!(
                decide(line),
                Guard::Refuse("redis_stream_refused"),
                "{line}"
            );
        }
        for line in [
            "AUTH secret",
            "HELLO 3",
            "RESET",
            "QUIT",
            "CLIENT REPLY OFF",
            "CLIENT TRACKING ON",
        ] {
            assert_eq!(
                decide(line),
                Guard::Refuse("redis_session_refused"),
                "{line}"
            );
        }
        // Confirming does not open them.
        assert_eq!(
            decide("CONFIRM MONITOR"),
            Guard::Refuse("redis_stream_refused")
        );
    }

    #[test]
    fn waiting_without_an_end_is_refused_and_with_one_is_run() {
        for line in [
            "BLPOP queue 0",
            "BRPOP a b 0",
            "BLMOVE a b LEFT RIGHT 0",
            "BZPOPMIN z 0",
            "BLMPOP 0 2 a b LEFT",
            "BZMPOP 0 1 z MIN",
            "XREAD BLOCK 0 STREAMS s $",
            "XREADGROUP GROUP g c block 0 STREAMS s >",
            "WAIT 1 0",
        ] {
            assert_eq!(decide(line), Guard::Refuse("redis_wait_forever"), "{line}");
        }
        for line in [
            "BLPOP queue 5",
            "BLPOP queue 0.5",
            "BLMPOP 2 1 a LEFT",
            "XREAD BLOCK 1000 STREAMS s $",
            "XREAD COUNT 10 STREAMS s 0",
            "WAIT 1 100",
            "LPOP queue",
        ] {
            assert_eq!(decide(line), Guard::Run, "{line}");
        }
    }

    #[test]
    fn commands_that_remove_data_or_touch_the_server_need_the_word() {
        for line in [
            "FLUSHALL",
            "flushdb async",
            "SWAPDB 0 1",
            "MIGRATE host 6379 key 0 1000",
        ] {
            assert_eq!(
                decide(line),
                Guard::Confirm("redis_confirm_data_loss"),
                "{line}"
            );
        }
        for line in [
            "SHUTDOWN NOSAVE",
            "DEBUG SLEEP 30",
            "DEBUG SEGFAULT",
            "REPLICAOF NO ONE",
            "CONFIG SET maxmemory 1",
            "CONFIG REWRITE",
            "CLUSTER RESET HARD",
            "CLUSTER FAILOVER",
            "ACL SETUSER app on",
            "CLIENT KILL ID 3",
            "CLIENT PAUSE 60000",
            "SCRIPT FLUSH",
            "FUNCTION FLUSH",
            "MODULE UNLOAD x",
            "SAVE",
        ] {
            assert_eq!(
                decide(line),
                Guard::Confirm("redis_confirm_server"),
                "{line}"
            );
        }
        for line in [
            "CONFIG GET maxmemory",
            "CLUSTER NODES",
            "cluster keyslot user:1",
            "ACL LIST",
            "ACL WHOAMI",
            "CLIENT LIST",
            "CLIENT INFO",
            "SCRIPT EXISTS abc",
            "SCRIPT KILL",
            "DEBUG OBJECT key",
            "CLIENT SETNAME console",
            "BGSAVE",
            "DEL a b c",
            "INFO",
            "DBSIZE",
        ] {
            assert_eq!(decide(line), Guard::Run, "{line}");
        }
        assert_eq!(decide("KEYS *"), Guard::Keys);
        assert_eq!(decide("select 2"), Guard::Select);
        // The word in front is taken off: the guard still names the command.
        let line = &lines("CONFIRM FLUSHDB").unwrap()[0];
        assert!(line.confirmed);
        assert_eq!(guard(line), Guard::Confirm("redis_confirm_data_loss"));
    }
}
