//! The console: requests written the way Kibana Dev Tools takes them.
//! (Their answers become a grid in [`answer`](super::answer).)
//!
//! ```text
//! GET bazaqa-orders/_search
//! { "query": { "match": { "title": "nowe" } } }
//!
//! POST _bulk
//! { "index": { "_index": "bazaqa-orders", "_id": "1" } }
//! { "title": "Nowe" }
//!
//! SELECT status, COUNT(*) FROM "bazaqa-orders" GROUP BY status;
//! ```
//!
//! A request is a method and a path on one line, and, on the lines after
//! it, its JSON body; several bodies are sent one per line, as `_bulk`
//! wants them. A line that starts like SQL is sent to the SQL API of the
//! server. `#` and `//` start a comment. A request that would remove an
//! index, or every document of one, runs only with the word `confirm` in
//! front of it; everything is read before anything is sent, so a script
//! with a mistake changes nothing.
use serde_json::Value;

/// What cannot be read as a request.
pub(super) const SYNTAX: &str = "elasticsearch_console_syntax";
/// A body that is not JSON.
pub(super) const BODY: &str = "elasticsearch_console_json";
/// A request that removes everything, without `confirm`.
pub(super) const CONFIRMATION: &str = "elasticsearch_confirmation_required";

const METHODS: [&str; 6] = ["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];
/// The words an SQL statement of either server starts with.
const SQL_WORDS: [&str; 6] = ["SELECT", "SHOW", "DESCRIBE", "DESC", "EXPLAIN", "WITH"];
/// Paths whose body is a list of documents, one per line.
const LINE_BODIES: [&str; 2] = ["_bulk", "_msearch"];

/// The body of a request.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Body {
    /// One JSON document, on one line.
    One(String),
    /// Several, sent one per line.
    Lines(Vec<String>),
}

/// What a request asks for.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Step {
    Rest {
        method: &'static str,
        /// As it was typed, without the slash in front.
        path: String,
        body: Option<Body>,
    },
    Sql(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Request {
    /// The line the request starts on, from 1.
    pub line: usize,
    /// The word `confirm` stood in front of it.
    pub confirmed: bool,
    pub step: Step,
}

/// What in a script is not understood, and on which line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Mistake {
    pub code: &'static str,
    pub line: usize,
}

struct Reader<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Reader<'a> {
    fn rest(&self) -> &'a str {
        &self.text[self.at..]
    }

    fn line(&self) -> usize {
        self.text[..self.at].matches('\n').count() + 1
    }

    fn mistake(&self, code: &'static str) -> Mistake {
        Mistake {
            code,
            line: self.line(),
        }
    }

    /// The rest of the current line, without its end.
    fn take_line(&mut self) -> &'a str {
        let rest = self.rest();
        let end = rest.find('\n').unwrap_or(rest.len());
        self.at += (end + 1).min(rest.len());
        rest[..end].trim_end_matches('\r')
    }

    /// Skip blank lines and comments between requests and before a body.
    fn skip_blank(&mut self) {
        loop {
            let rest = self.rest();
            let trimmed = rest.trim_start();
            self.at += rest.len() - trimmed.len();
            if trimmed.starts_with('#') || trimmed.starts_with("//") {
                self.take_line();
            } else if let Some(inside) = trimmed.strip_prefix("/*") {
                self.at += 2 + inside.find("*/").map_or(inside.len(), |end| end + 2);
            } else {
                return;
            }
        }
    }

    /// Read one JSON value that starts here, and return it on one line:
    /// comments are dropped and a `"""` text of Dev Tools becomes a string.
    fn json(&mut self) -> Result<String, Mistake> {
        let line = self.line();
        let failed = || Mistake { code: BODY, line };
        let bytes = self.text.as_bytes();
        let mut out = String::new();
        let mut depth = 0usize;
        while self.at < bytes.len() {
            let rest = &self.text[self.at..];
            match bytes[self.at] {
                b'"' if rest.starts_with("\"\"\"") => {
                    let inside = &rest[3..];
                    let end = inside.find("\"\"\"").ok_or_else(failed)?;
                    out.push_str(&Value::String(inside[..end].to_owned()).to_string());
                    self.at += end + 6;
                }
                b'"' => {
                    let mut end = 1;
                    let inner = rest.as_bytes();
                    loop {
                        match inner.get(end) {
                            Some(b'\\') => end += 2,
                            Some(b'"') => break,
                            // A string of JSON does not span lines.
                            Some(b'\n') | None => return Err(failed()),
                            Some(_) => end += 1,
                        }
                    }
                    out.push_str(&rest[..=end]);
                    self.at += end + 1;
                }
                b'#' => {
                    self.take_line();
                }
                b'/' if rest.starts_with("//") => {
                    self.take_line();
                }
                b'/' if rest.starts_with("/*") => {
                    let end = rest[2..].find("*/").ok_or_else(failed)?;
                    self.at += end + 4;
                }
                b'\n' | b'\r' => self.at += 1,
                open @ (b'{' | b'[') => {
                    depth += 1;
                    out.push(open as char);
                    self.at += 1;
                }
                close @ (b'}' | b']') => {
                    depth = depth.checked_sub(1).ok_or_else(failed)?;
                    out.push(close as char);
                    self.at += 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {
                    let c = rest.chars().next().unwrap_or(' ');
                    out.push(c);
                    self.at += c.len_utf8();
                }
            }
        }
        if depth != 0 || serde_json::from_str::<serde::de::IgnoredAny>(&out).is_err() {
            return Err(failed());
        }
        Ok(out)
    }

    /// An SQL statement: up to its `;`, or to the line the next request
    /// starts on.
    fn sql(&mut self) -> String {
        let start = self.at;
        let bytes = self.text.as_bytes();
        let mut line_start = false;
        while self.at < bytes.len() {
            if line_start {
                let rest = self.rest();
                let trimmed = rest.trim_start_matches([' ', '\t']);
                if starts_rest(trimmed) {
                    break;
                }
                line_start = false;
            }
            match bytes[self.at] {
                quote @ (b'\'' | b'"' | b'`') => {
                    self.at += 1;
                    while self.at < bytes.len() && bytes[self.at] != quote {
                        self.at += 1;
                    }
                    self.at += 1;
                }
                b';' => {
                    let statement = self.text[start..self.at].trim().to_owned();
                    self.at += 1;
                    return statement;
                }
                b'\n' => {
                    line_start = true;
                    self.at += 1;
                }
                _ => self.at += 1,
            }
        }
        self.at = self.at.min(bytes.len());
        self.text[start..self.at].trim().to_owned()
    }
}

/// The first word of a text, in capitals, and what follows it.
fn first_word(text: &str) -> (String, &str) {
    let end = text
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    (text[..end].to_ascii_uppercase(), &text[end..])
}

/// `DELETE FROM index …` is SQL (OpenSearch has it); `DELETE index` is not.
fn sql_delete(word: &str, rest: &str) -> bool {
    let mut words = rest.split_whitespace();
    word == "DELETE"
        && words
            .next()
            .is_some_and(|next| next.eq_ignore_ascii_case("FROM"))
        && words.next().is_some()
}

/// Whether a line starts a request of the API, or the word `confirm`: what
/// ends an SQL statement that has no `;`. A word of SQL does not: a
/// statement may go on with `SELECT` or `DESC` on a line of its own.
fn starts_rest(line: &str) -> bool {
    let (word, rest) = first_word(line);
    let spaced = rest.starts_with([' ', '\t']);
    let head = rest.split('\n').next().unwrap_or_default();
    (METHODS.contains(&word.as_str())
        && spaced
        && !head.trim().is_empty()
        && !sql_delete(&word, head))
        || (word == "CONFIRM" && spaced)
}

/// Read a script into its requests, or name the first thing in it that is
/// not one.
pub(super) fn parse(text: &str) -> Result<Vec<Request>, Mistake> {
    let mut reader = Reader { text, at: 0 };
    let mut requests = Vec::new();
    loop {
        reader.skip_blank();
        if reader.rest().is_empty() {
            return Ok(requests);
        }
        let line = reader.line();
        let (mut word, mut rest) = first_word(reader.rest());
        let confirmed = word == "CONFIRM" && rest.starts_with([' ', '\t']);
        if confirmed {
            let after = rest.trim_start_matches([' ', '\t']);
            reader.at = text.len() - after.len();
            (word, rest) = first_word(after);
        }
        let head = rest.split('\n').next().unwrap_or_default();
        if SQL_WORDS.contains(&word.as_str()) || sql_delete(&word, head) {
            let statement = reader.sql();
            if statement.is_empty() {
                return Err(Mistake { code: SYNTAX, line });
            }
            requests.push(Request {
                line,
                confirmed,
                step: Step::Sql(statement),
            });
            continue;
        }
        let Some(method) = METHODS.iter().copied().find(|method| *method == word) else {
            return Err(Mistake { code: SYNTAX, line });
        };
        if !rest.starts_with([' ', '\t']) {
            return Err(Mistake { code: SYNTAX, line });
        }
        reader.at = text.len() - rest.len();
        let typed = reader.take_line();
        // What follows the path on its line can only be a comment.
        let mut parts = typed.split_whitespace();
        let Some(token) = parts.next() else {
            return Err(Mistake { code: SYNTAX, line });
        };
        // `GET /` asks the root of the API.
        let path = token.trim_start_matches('/');
        let trailing = parts.next().unwrap_or_default();
        if !(trailing.is_empty() || trailing.starts_with('#') || trailing.starts_with("//")) {
            return Err(Mistake { code: SYNTAX, line });
        }
        let mut bodies = Vec::new();
        loop {
            reader.skip_blank();
            if !reader.rest().starts_with(['{', '[']) {
                break;
            }
            bodies.push(reader.json()?);
            // What follows a body on its line can only be a comment.
            let after = reader.rest();
            let line_end = after.find('\n').unwrap_or(after.len());
            let tail = after[..line_end].trim();
            if !(tail.is_empty()
                || tail.starts_with('#')
                || tail.starts_with("//")
                || tail.starts_with(['{', '[']))
            {
                return Err(reader.mistake(BODY));
            }
        }
        let endpoint = path.split('?').next().unwrap_or_default();
        let lines = LINE_BODIES
            .iter()
            .any(|name| endpoint.split('/').any(|part| part == *name));
        let body = match bodies.len() {
            0 => None,
            1 if !lines => Some(Body::One(bodies.remove(0))),
            _ => Some(Body::Lines(bodies)),
        };
        requests.push(Request {
            line,
            confirmed,
            step: Step::Rest {
                method,
                path: path.to_owned(),
                body,
            },
        });
    }
}

/// The segments of a path, without its query.
fn segments(path: &str) -> Vec<&str> {
    path.split('?')
        .next()
        .unwrap_or_default()
        .split('/')
        .filter(|part| !part.is_empty())
        .collect()
}

/// A query that leaves no document out.
fn matches_everything(query: &Value) -> bool {
    match query {
        Value::Null => true,
        Value::Object(object) if object.is_empty() => true,
        Value::Object(object) => object.len() == 1 && object.contains_key("match_all"),
        _ => false,
    }
}

/// Whether an SQL statement has a `WHERE` outside its quotes and comments.
fn has_where(statement: &str) -> bool {
    let mut plain = String::new();
    let mut quote = None;
    let mut rest = statement.chars().peekable();
    while let Some(c) = rest.next() {
        match quote {
            Some(open) if c == open => quote = None,
            Some(_) => {}
            None if matches!(c, '\'' | '"' | '`') => quote = Some(c),
            // A comment runs to the end of its line.
            None if c == '-' && rest.peek() == Some(&'-') => {
                for skipped in rest.by_ref() {
                    if skipped == '\n' {
                        break;
                    }
                }
                plain.push(' ');
            }
            None => plain.push(c.to_ascii_uppercase()),
        }
    }
    plain
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|word| word == "WHERE")
}

/// A request that removes an index, a data stream or a snapshot, or the
/// documents of an index without saying which: it runs only with `confirm`.
pub(super) fn destructive(step: &Step) -> bool {
    let (method, path, body) = match step {
        Step::Sql(statement) => {
            return first_word(statement).0 == "DELETE" && !has_where(statement);
        }
        Step::Rest { method, path, body } => (*method, path.as_str(), body),
    };
    let parts = segments(path);
    let Some(first) = parts.first().copied() else {
        return false;
    };
    let wide = |target: &str| target == "_all" || target.contains('*');
    let parsed = || match body {
        Some(Body::One(text)) => serde_json::from_str::<Value>(text).unwrap_or(Value::Null),
        _ => Value::Null,
    };
    let filtered = path.contains("?q=") || path.contains("&q=");
    match (method, parts.last().copied().unwrap_or_default()) {
        ("DELETE", _) => {
            // One segment that is not an API: indices, by name or pattern.
            (parts.len() == 1 && (!first.starts_with('_') || first == "_all"))
                || matches!(first, "_data_stream" | "_snapshot")
        }
        (_, "_delete_by_query") => {
            wide(first) || (!filtered && matches_everything(&parsed()["query"]))
        }
        // Every document rewritten by a script.
        (_, "_update_by_query") => {
            let body = parsed();
            !body["script"].is_null()
                && (wide(first) || (!filtered && matches_everything(&body["query"])))
        }
        (_, "_aliases") => parsed()["actions"].as_array().is_some_and(|actions| {
            actions
                .iter()
                .any(|action| !action["remove_index"].is_null())
        }),
        // The SQL API by its path: OpenSearch deletes through it.
        (_, "_sql") => parsed()["query"].as_str().is_some_and(|statement| {
            first_word(statement.trim_start()).0 == "DELETE" && !has_where(statement)
        }),
        _ => false,
    }
}

/// Whether a request only reads: after anything else the pages and the
/// fields that were read before no longer stand.
pub(super) fn reads_only(step: &Step) -> bool {
    match step {
        Step::Sql(statement) => first_word(statement).0 != "DELETE",
        Step::Rest { method, path, .. } => {
            const READS: [&str; 22] = [
                "_search",
                "_msearch",
                "_count",
                "_explain",
                "_validate",
                "_analyze",
                "_field_caps",
                "_mget",
                "_mtermvectors",
                "_termvectors",
                "_sql",
                "_eql",
                "_esql",
                "_query",
                "_ppl",
                "_knn_search",
                "_search_shards",
                "_rank_eval",
                "_terms_enum",
                "_render",
                "_simulate",
                "_async_search",
            ];
            let parts = segments(path);
            matches!(*method, "GET" | "HEAD")
                || (*method == "POST"
                    && parts
                        .iter()
                        .any(|part| READS.contains(part) || *part == "template"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rest(method: &'static str, path: &str, body: Option<Body>) -> Step {
        Step::Rest {
            method,
            path: path.into(),
            body,
        }
    }

    fn steps(text: &str) -> Vec<Step> {
        parse(text)
            .unwrap_or_else(|mistake| panic!("{mistake:?}: {text}"))
            .into_iter()
            .map(|request| request.step)
            .collect()
    }

    #[test]
    fn a_script_is_read_the_way_dev_tools_reads_it() {
        let text = "# seed\nGET /bazaqa-orders/_search?size=1\n{\n  \"query\": { \"match\": { \"title\": \"a # b // c\" } } // trailing\n}\n\npost _bulk\n{ \"index\": { \"_index\": \"bazaqa-orders\", \"_id\": \"1\" } }\n{ \"title\": \"x\",\n  \"n\": 1 }\n{ \"delete\": { \"_index\": \"bazaqa-orders\", \"_id\": \"2\" } }\n// done\nHEAD bazaqa-orders\nPUT bazaqa-orders/_doc/7   # the seventh\n{\"title\": \"y\"}\n";
        let parsed = parse(text).unwrap();
        assert_eq!(
            parsed
                .iter()
                .map(|request| request.line)
                .collect::<Vec<_>>(),
            [2, 7, 13, 14]
        );
        assert!(parsed.iter().all(|request| !request.confirmed));
        assert_eq!(
            parsed[0].step,
            rest(
                "GET",
                "bazaqa-orders/_search?size=1",
                Some(Body::One(
                    "{  \"query\": { \"match\": { \"title\": \"a # b // c\" } } }".into()
                ))
            )
        );
        // A body of several documents is sent one per line.
        assert_eq!(
            parsed[1].step,
            rest(
                "POST",
                "_bulk",
                Some(Body::Lines(vec![
                    "{ \"index\": { \"_index\": \"bazaqa-orders\", \"_id\": \"1\" } }".into(),
                    "{ \"title\": \"x\",  \"n\": 1 }".into(),
                    "{ \"delete\": { \"_index\": \"bazaqa-orders\", \"_id\": \"2\" } }".into(),
                ]))
            )
        );
        assert_eq!(parsed[2].step, rest("HEAD", "bazaqa-orders", None));
        assert_eq!(
            parsed[3].step,
            rest(
                "PUT",
                "bazaqa-orders/_doc/7",
                Some(Body::One("{\"title\": \"y\"}".into()))
            )
        );
        // One action of `_bulk` is still a body of lines.
        assert_eq!(
            steps("POST idx/_bulk\n{\"delete\":{\"_id\":\"1\"}}")[0],
            rest(
                "POST",
                "idx/_bulk",
                Some(Body::Lines(vec!["{\"delete\":{\"_id\":\"1\"}}".into()]))
            )
        );
        assert!(parse("").unwrap().is_empty());
        assert!(
            parse("  \n# nothing\n// at all\n/* really */\n")
                .unwrap()
                .is_empty()
        );
        // Windows line ends, and the root of the API.
        assert_eq!(
            steps("GET _cat/indices\r\n\r\nGET /\r\nGET /_cluster/health\r\n"),
            [
                rest("GET", "_cat/indices", None),
                rest("GET", "", None),
                rest("GET", "_cluster/health", None),
            ]
        );
    }

    #[test]
    fn a_triple_quoted_text_becomes_a_string() {
        let text = "POST bazaqa-orders/_update/1\n{\n  \"script\": {\n    \"source\": \"\"\"\n      if (ctx._source.n > 1) { ctx._source.tag = \"big\" }\n    \"\"\"\n  }\n}";
        let parsed = steps(text);
        let Step::Rest {
            body: Some(Body::One(body)),
            ..
        } = &parsed[0]
        else {
            panic!("{parsed:?}");
        };
        let value: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            value["script"]["source"],
            json!("\n      if (ctx._source.n > 1) { ctx._source.tag = \"big\" }\n    ")
        );
        assert!(!body.contains('\n'));
    }

    #[test]
    fn sql_is_told_from_a_request_by_its_first_word() {
        let text = "select status, count(*)\n  from \"bazaqa-orders\"\n  where title = 'a;b'\n  group by status;\nSHOW TABLES\nGET _cat/health\nDESCRIBE `bazaqa-orders`;\ndelete from bazaqa-orders where n > 5";
        assert_eq!(
            steps(text),
            [
                Step::Sql(
                    "select status, count(*)\n  from \"bazaqa-orders\"\n  where title = 'a;b'\n  group by status"
                        .into()
                ),
                Step::Sql("SHOW TABLES".into()),
                rest("GET", "_cat/health", None),
                Step::Sql("DESCRIBE `bazaqa-orders`".into()),
                Step::Sql("delete from bazaqa-orders where n > 5".into()),
            ]
        );
        // `DELETE` with a path is a request of the API.
        assert_eq!(
            steps("DELETE bazaqa-orders/_doc/1")[0],
            rest("DELETE", "bazaqa-orders/_doc/1", None)
        );
        assert_eq!(steps("delete from"), [rest("DELETE", "from", None)]);
    }

    #[test]
    fn what_is_not_a_request_is_named_with_its_line() {
        let mistake = |text: &str| parse(text).unwrap_err();
        assert_eq!(
            mistake("GET _cat/health\n\nFETCH everything"),
            Mistake {
                code: SYNTAX,
                line: 3
            }
        );
        assert_eq!(
            mistake("{\"query\": {}}"),
            Mistake {
                code: SYNTAX,
                line: 1
            }
        );
        assert_eq!(
            mistake("GET"),
            Mistake {
                code: SYNTAX,
                line: 1
            }
        );
        assert_eq!(
            mistake("GET   "),
            Mistake {
                code: SYNTAX,
                line: 1
            }
        );
        assert_eq!(
            mistake("GETX _search"),
            Mistake {
                code: SYNTAX,
                line: 1
            }
        );
        assert_eq!(
            mistake("GET a/_search and more"),
            Mistake {
                code: SYNTAX,
                line: 1
            }
        );
        assert_eq!(
            mistake("confirm"),
            Mistake {
                code: SYNTAX,
                line: 1
            }
        );
        // A body that is not JSON, with the line it starts on.
        for body in [
            "{\"query\": }",
            "{\"query\": {\"match_all\": {}}",
            "{\"a\": \"open",
            "{\"a\": \"\"\"never closed}",
            "{'single': 1}",
            "[1, 2",
        ] {
            assert_eq!(
                mistake(&format!("GET _cat/health\nPOST a/_search\n{body}")),
                Mistake {
                    code: BODY,
                    line: 3
                },
                "{body}"
            );
        }
        assert_eq!(
            mistake("POST a/_search\n{\"size\": 1} trailing"),
            Mistake {
                code: BODY,
                line: 2
            }
        );
    }

    fn needs_confirmation(text: &str) -> bool {
        let parsed = parse(text).unwrap();
        assert_eq!(parsed.len(), 1, "{text}");
        destructive(&parsed[0].step)
    }

    #[test]
    fn removing_everything_needs_the_word_confirm() {
        for text in [
            "DELETE bazaqa-orders",
            "DELETE /bazaqa-orders,bazaqa-clients",
            "DELETE bazaqa-*",
            "DELETE _all",
            "DELETE *",
            "DELETE _data_stream/bazaqa-logs",
            "DELETE _snapshot/backups/snap-1",
            "POST bazaqa-orders/_delete_by_query",
            "POST bazaqa-orders/_delete_by_query\n{}",
            "POST bazaqa-orders/_delete_by_query?conflicts=proceed\n{\"query\": {\"match_all\": {}}}",
            "POST _all/_delete_by_query\n{\"query\": {\"term\": {\"tag\": \"x\"}}}",
            "POST bazaqa-*/_delete_by_query\n{\"query\": {\"term\": {\"tag\": \"x\"}}}",
            "POST bazaqa-orders/_update_by_query\n{\"script\": {\"source\": \"ctx._source.n = 0\"}}",
            "POST _aliases\n{\"actions\": [{\"remove_index\": {\"index\": \"bazaqa-orders\"}}]}",
            "POST _plugins/_sql\n{\"query\": \"DELETE FROM bazaqa-orders\"}",
            "DELETE FROM bazaqa-orders",
            "delete from `bazaqa-orders`;",
            "DELETE FROM orders -- where n > 1",
        ] {
            assert!(needs_confirmation(text), "{text}");
            let confirmed = parse(&format!("confirm {text}")).unwrap();
            assert!(confirmed[0].confirmed, "{text}");
            assert!(destructive(&confirmed[0].step), "{text}");
        }
        for text in [
            "DELETE bazaqa-orders/_doc/1",
            "DELETE bazaqa-orders/_alias/current",
            "DELETE _index_template/bazaqa",
            "DELETE _pit\n{\"id\": \"x\"}",
            "DELETE _search/scroll/_all",
            "POST bazaqa-orders/_delete_by_query\n{\"query\": {\"term\": {\"tag\": \"x\"}}}",
            "POST bazaqa-orders/_delete_by_query?q=tag:x",
            "POST bazaqa-orders/_update_by_query",
            "POST bazaqa-orders/_update_by_query\n{\"script\": {\"source\": \"ctx._source.n = 0\"}, \"query\": {\"term\": {\"tag\": \"x\"}}}",
            "POST _aliases\n{\"actions\": [{\"add\": {\"index\": \"a\", \"alias\": \"b\"}}]}",
            "POST bazaqa-orders/_search\n{\"query\": {\"match_all\": {}}}",
            "PUT bazaqa-orders",
            "GET bazaqa-orders",
            "DELETE FROM bazaqa-orders WHERE n > 5",
            "SELECT * FROM bazaqa-orders",
            "POST _sql\n{\"query\": \"SELECT 'DELETE' FROM x\"}",
        ] {
            assert!(!needs_confirmation(text), "{text}");
        }
        // The word is taken only in front of a request.
        assert!(parse("CONFIRM GET _cat/health").unwrap()[0].confirmed);
        assert_eq!(
            parse("confirm show tables").unwrap()[0].step,
            Step::Sql("show tables".into())
        );
        assert!(has_where("DELETE FROM a WHERE x = 'y'"));
        assert!(!has_where("DELETE FROM a_where 'WHERE'"));
    }

    #[test]
    fn a_request_that_changes_nothing_is_told_from_one_that_may() {
        for text in [
            "GET bazaqa-orders/_search",
            "POST bazaqa-orders/_search\n{}",
            "POST _msearch\n{}\n{}",
            "POST bazaqa-orders/_count",
            "POST _sql?format=json\n{\"query\": \"SELECT 1\"}",
            "POST _plugins/_sql\n{\"query\": \"SELECT 1\"}",
            "POST _search/template\n{}",
            "HEAD bazaqa-orders",
            "SELECT 1",
        ] {
            assert!(reads_only(&parse(text).unwrap()[0].step), "{text}");
        }
        for text in [
            "POST bazaqa-orders/_doc\n{}",
            "PUT bazaqa-orders",
            "DELETE bazaqa-orders/_doc/1",
            "POST _bulk\n{}",
            "POST bazaqa-orders/_refresh",
            "POST bazaqa-orders/_delete_by_query\n{}",
            "DELETE FROM a WHERE b = 1",
        ] {
            assert!(!reads_only(&parse(text).unwrap()[0].step), "{text}");
        }
    }
}
