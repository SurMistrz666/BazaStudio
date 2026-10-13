//! Changes of documents: one field, a new document, a removed one, and an
//! import.
//!
//! An index has no transactions and no locks. What it has is a number for
//! every change of a document (`_seq_no`, with `_primary_term`), and writes
//! that are refused unless the document still carries the number that was
//! read. A change of the grid is made that way:
//!
//! 1. the document is found by its `_id` (which also says how it is
//!    routed), and the field is compared with what the grid showed;
//! 2. the write names the `_seq_no` that was read. If anything changed the
//!    document in between, the server refuses and nothing is written.
//!
//! Every write asks for a refresh, so that the page read after it shows it.
//!
//! An import is a `_bulk` of `create` actions, which never overwrite. It is
//! as close to "all or nothing" as an index allows: every row is turned
//! into a document before anything is sent, ids that exist stop the import
//! before it starts, and when the server refuses a document the documents
//! written so far are deleted again, each only if nobody changed it since.
//! What that cannot undo: a search between the write and the removal may
//! have seen the documents, a field the documents added to the mapping
//! stays in the mapping, and a connection that breaks in the middle of a
//! request leaves the outcome of that request unknown (said as
//! `elasticsearch_import_incomplete`).
use super::http::{self, Payload};
use super::mapping::{self, ID, Layout};
use super::{SearchSession, opaque, views};
use crate::drivers::RowChange;
use crate::model::{Cell, QueryResult, TableRef};
use anyhow::{Result, anyhow, bail, ensure};
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

const CONFLICT: &str = "row_changed_reload_required";
/// Documents of one `_bulk` request, and its bytes.
const BULK_DOCUMENTS: usize = 1000;
const BULK_BYTES: usize = 8 * 1024 * 1024;
/// Ids looked up at once before an import.
const LOOKUP: usize = 1000;
/// What an answer of `_bulk` is cut down to.
const BULK_ANSWER: &str = "filter_path=errors,items.*._id,items.*.status,items.*._seq_no,items.*._primary_term,items.*.error.type";

/// The container a change is made in.
fn table_of(change: &RowChange) -> &TableRef {
    match change {
        RowChange::UpdateCell { table, .. }
        | RowChange::InsertRow { table, .. }
        | RowChange::DeleteRow { table, .. } => table,
    }
}

/// Whether a field still holds what the grid showed.
fn same(current: &Cell, shown: &Cell) -> bool {
    if current == shown {
        return true;
    }
    let number = |cell: &Cell| match cell {
        Cell::Integer(text) | Cell::Decimal(text) | Cell::Float(text) => {
            text.parse::<bigdecimal::BigDecimal>().ok()
        }
        _ => None,
    };
    let document = |cell: &Cell| match cell {
        Cell::Json(text) => serde_json::from_str::<Value>(text).ok(),
        _ => None,
    };
    match (number(current), number(shown)) {
        (Some(current), Some(shown)) => current == shown,
        _ => matches!((document(current), document(shown)), (Some(a), Some(b)) if a == b),
    }
}

/// The parameters of a write to a document that was found: it is made only
/// while the document is the one that was read, where it is routed, and it
/// is visible to the next search.
fn guard(located: &super::Located) -> String {
    let mut parameters = format!(
        "if_seq_no={}&if_primary_term={}&refresh=true",
        located.seq_no, located.primary_term
    );
    if let Some(routing) = &located.routing {
        parameters.push_str(&format!("&routing={}", http::segment(routing)));
    }
    parameters
}

/// One line of an action of `_bulk`, or of a document.
fn line(out: &mut Vec<u8>, value: &Value) {
    out.extend_from_slice(value.to_string().as_bytes());
    out.push(b'\n');
}

/// One document an import wrote: enough to remove it again, and only it.
struct Written {
    id: String,
    seq_no: u64,
    primary_term: u64,
}

/// What became of one `_bulk` request of an import.
struct Outcome {
    written: Vec<Written>,
    /// The first document the server refused: its place in the request and
    /// the kind of the refusal.
    refused: Option<(usize, String)>,
}

fn outcome(answer: &Value) -> Outcome {
    let mut out = Outcome {
        written: Vec::new(),
        refused: None,
    };
    for (place, item) in answer["items"].as_array().into_iter().flatten().enumerate() {
        let Some(result) = item.as_object().and_then(|item| item.values().next()) else {
            continue;
        };
        let status = result["status"].as_u64().unwrap_or(0);
        if (200..300).contains(&status) {
            if let (Some(id), Some(seq_no), Some(primary_term)) = (
                result["_id"].as_str(),
                result["_seq_no"].as_u64(),
                result["_primary_term"].as_u64(),
            ) {
                out.written.push(Written {
                    id: id.to_owned(),
                    seq_no,
                    primary_term,
                });
            }
        } else if out.refused.is_none() {
            out.refused = Some((
                place,
                result["error"]["type"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            ));
        }
    }
    out
}

/// What a refused document of an import is called.
fn import_code(kind: &str, row: usize) -> String {
    match kind {
        "version_conflict_engine_exception" => "elasticsearch_import_id_exists".into(),
        "cluster_block_exception" => "elasticsearch_blocked".into(),
        "mapper_parsing_exception"
        | "document_parsing_exception"
        | "strict_dynamic_mapping_exception"
        | "illegal_argument_exception" => {
            format!("elasticsearch_import_value_rejected_at_row_{row}")
        }
        _ => format!("elasticsearch_import_refused_at_row_{row}"),
    }
}

impl SearchSession {
    /// Only an index takes changes from the grid.
    fn writable(&self, table: &TableRef) -> Result<()> {
        let kind = self
            .kind_of(&table.name)
            .unwrap_or_else(|| table.kind.clone());
        ensure!(
            kind == "table" && !views::is_view(&table.name),
            "elasticsearch_read_only_object"
        );
        Ok(())
    }

    /// What a change looks like as text: the request of the console that
    /// makes it.
    pub(super) fn change_as_text(&self, change: &RowChange) -> Result<String> {
        let table = table_of(change);
        self.writable(table)?;
        let index = http::segment(&table.name);
        Ok(match change {
            RowChange::UpdateCell {
                column, new, key, ..
            } => {
                ensure!(column != ID, "elasticsearch_id_read_only");
                // The grid read the fields before it showed the documents.
                let layout = self
                    .remembered(&table.name)
                    .ok_or_else(|| anyhow!("result_changed_reload_required"))?;
                let class = layout
                    .class(column)
                    .ok_or_else(|| anyhow!("column_not_found"))?;
                let document = mapping::partial(&json!({}), column, mapping::json(class, new)?)?;
                format!(
                    "# only while the document is the one that was read (if_seq_no, if_primary_term)\nPOST {index}/_update/{}?refresh=true\n{}",
                    http::segment(&browse_id(key)?),
                    serde_json::to_string_pretty(&json!({ "doc": document }))?
                )
            }
            RowChange::InsertRow { values, .. } => {
                let layout = self
                    .remembered(&table.name)
                    .ok_or_else(|| anyhow!("result_changed_reload_required"))?;
                let (id, document) = new_document(&layout, values)?;
                let body = serde_json::to_string_pretty(&Value::Object(document))?;
                match id {
                    Some(id) => format!(
                        "PUT {index}/_doc/{}?op_type=create&refresh=true\n{body}",
                        http::segment(&id)
                    ),
                    None => format!("POST {index}/_doc?refresh=true\n{body}"),
                }
            }
            RowChange::DeleteRow { key, .. } => format!(
                "# only while the document is the one that was read (if_seq_no, if_primary_term)\nDELETE {index}/_doc/{}?refresh=true",
                http::segment(&browse_id(key)?)
            ),
        })
    }

    /// Make one change of one document, or none at all.
    pub(super) async fn change(&self, change: &RowChange) -> Result<()> {
        let table = table_of(change);
        self.writable(table)?;
        let id = opaque();
        let name = table.name.as_str();
        let outcome = match change {
            RowChange::UpdateCell {
                column,
                old,
                new,
                key,
                ..
            } => {
                ensure!(column != ID, "elasticsearch_id_read_only");
                let layout = self.learned(name, &id).await?;
                let class = layout
                    .class(column)
                    .ok_or_else(|| anyhow!("column_not_found"))?;
                let document_id = browse_id(key)?;
                // The document is gone, or it is not what the grid showed.
                let located = self
                    .locate(name, &document_id, &id)
                    .await?
                    .ok_or_else(|| anyhow!(CONFLICT))?;
                let current = mapping::cell(class, &mapping::lookup(&located.source, column));
                ensure!(same(&current, old), CONFLICT);
                let value = mapping::json(class, new)?;
                let document = mapping::partial(&located.source, column, value)?;
                self.call(
                    "POST",
                    &format!(
                        "{}/_update/{}?{}",
                        http::segment(&located.index),
                        http::segment(&document_id),
                        guard(&located)
                    ),
                    Some(json!({ "doc": document })),
                    &id,
                    1024 * 1024,
                )
                .await
            }
            RowChange::InsertRow { values, .. } => {
                let layout = self.learned(name, &id).await?;
                let (document_id, document) = new_document(&layout, values)?;
                let index = http::segment(name);
                let (method, path) = match &document_id {
                    // `create`: an id that is taken is refused, not overwritten.
                    Some(document_id) => (
                        "PUT",
                        format!(
                            "{index}/_doc/{}?op_type=create&refresh=true",
                            http::segment(document_id)
                        ),
                    ),
                    None => ("POST", format!("{index}/_doc?refresh=true")),
                };
                let made = self
                    .call(
                        method,
                        &path,
                        Some(Value::Object(document)),
                        &id,
                        1024 * 1024,
                    )
                    .await;
                self.written(false).await;
                return match made {
                    Ok(_) => Ok(()),
                    Err(error)
                        if http::refused(&error).is_some_and(|refusal| refusal.status == 409) =>
                    {
                        bail!("elasticsearch_document_exists")
                    }
                    Err(error) => Err(error),
                };
            }
            RowChange::DeleteRow { key, .. } => {
                let document_id = browse_id(key)?;
                let located = self
                    .locate(name, &document_id, &id)
                    .await?
                    .ok_or_else(|| anyhow!(CONFLICT))?;
                self.call(
                    "DELETE",
                    &format!(
                        "{}/_doc/{}?{}",
                        http::segment(&located.index),
                        http::segment(&document_id),
                        guard(&located)
                    ),
                    None,
                    &id,
                    1024 * 1024,
                )
                .await
            }
        };
        self.written(false).await;
        match outcome {
            Ok(_) => Ok(()),
            // Changed or removed between the look and the write.
            Err(error)
                if http::refused(&error)
                    .is_some_and(|refusal| matches!(refusal.status, 404 | 409))
                    || error.to_string() == "elasticsearch_not_found" =>
            {
                bail!(CONFLICT)
            }
            Err(error) => Err(error),
        }
    }

    /// Which of these ids are the id of a document.
    async fn any_exists(&self, name: &str, ids: &[&str], opaque: &str) -> Result<bool> {
        for chunk in ids.chunks(LOOKUP) {
            let answer = self
                .call(
                    "POST",
                    &format!("{}/_search", http::segment(name)),
                    Some(json!({
                        "query": { "ids": { "values": chunk } },
                        "size": 1,
                        "_source": false,
                        "track_total_hits": false,
                    })),
                    opaque,
                    1024 * 1024,
                )
                .await?;
            if answer["hits"]["hits"]
                .as_array()
                .is_some_and(|hits| !hits.is_empty())
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Remove what an import wrote, each document only if it is still the
    /// one the import made. Returns whether all of them are gone.
    async fn take_back(&self, name: &str, written: &[Written], opaque: &str) -> bool {
        let index = http::segment(name);
        let mut clean = true;
        for chunk in written.chunks(BULK_DOCUMENTS) {
            let mut body = Vec::new();
            for document in chunk {
                line(
                    &mut body,
                    &json!({ "delete": {
                        "_id": document.id,
                        "if_seq_no": document.seq_no,
                        "if_primary_term": document.primary_term,
                    } }),
                );
            }
            let sent = self
                .client
                .send(
                    "POST",
                    &format!("{index}/_bulk?{BULK_ANSWER}"),
                    Some(Payload::Lines(body)),
                    Some(opaque),
                    BULK_BYTES,
                )
                .await
                .and_then(http::Reply::accepted);
            match sent {
                Ok(answer) => clean &= outcome(&answer).refused.is_none(),
                Err(_) => clean = false,
            }
        }
        let _ = self
            .call(
                "POST",
                &format!("{index}/_refresh"),
                None,
                opaque,
                1024 * 1024,
            )
            .await;
        clean
    }

    /// Add the rows of a file as documents: all of them, or none.
    pub(super) async fn import(
        &self,
        container: &TableRef,
        columns: &[String],
        rows: &[Vec<Cell>],
        cancel: CancellationToken,
    ) -> Result<QueryResult> {
        let started = Instant::now();
        self.writable(container)?;
        ensure!(!cancel.is_cancelled(), "query_cancelled");
        if rows.is_empty() {
            return Ok(QueryResult::default());
        }
        let id = opaque();
        let name = container.name.as_str();
        let layout = self.learned(name, &id).await?;
        // Every row becomes a document first: a value that cannot be
        // written stops the import before anything is sent.
        let mut documents: Vec<(Option<String>, Value)> = Vec::with_capacity(rows.len());
        let mut seen = HashSet::new();
        for row in rows {
            ensure!(row.len() == columns.len(), "import_shape_invalid");
            let (document_id, document) =
                mapping::document(&layout, columns.iter().map(String::as_str).zip(row.iter()))?;
            if let Some(document_id) = &document_id {
                ensure!(
                    seen.insert(document_id.clone()),
                    "elasticsearch_import_duplicate"
                );
            }
            documents.push((document_id, Value::Object(document)));
        }
        // `create` would refuse an id that exists one by one, after others
        // were written: the ids are looked up before anything is.
        let given: Vec<&str> = documents
            .iter()
            .filter_map(|(document_id, _)| document_id.as_deref())
            .collect();
        ensure!(
            !self.any_exists(name, &given, &id).await?,
            "elasticsearch_import_id_exists"
        );
        let index = http::segment(name);
        let mut written: Vec<Written> = Vec::new();
        let mut next = 0;
        let mut failure: Option<anyhow::Error> = None;
        // The outcome of a request that got no answer is not known.
        let mut unknown = false;
        while next < documents.len() && failure.is_none() {
            if cancel.is_cancelled() {
                failure = Some(anyhow!("query_cancelled"));
                break;
            }
            let first = next;
            let mut body = Vec::new();
            while next < documents.len()
                && next - first < BULK_DOCUMENTS
                && (body.len() < BULK_BYTES || next == first)
            {
                let (document_id, document) = &documents[next];
                match document_id {
                    Some(document_id) => {
                        line(&mut body, &json!({ "create": { "_id": document_id } }))
                    }
                    // Without an id the server makes one.
                    None => line(&mut body, &json!({ "index": {} })),
                }
                line(&mut body, document);
                next += 1;
            }
            let sent = tokio::time::timeout(
                super::DEADLINE,
                self.client.send(
                    "POST",
                    &format!("{index}/_bulk?{BULK_ANSWER}"),
                    Some(Payload::Lines(body)),
                    Some(&id),
                    BULK_BYTES,
                ),
            )
            .await;
            let reply = match sent {
                Ok(Ok(reply)) => reply,
                Ok(Err(error)) => {
                    unknown = true;
                    failure = Some(error);
                    break;
                }
                Err(_) => {
                    unknown = true;
                    failure = Some(anyhow!("query_timeout"));
                    break;
                }
            };
            match reply.accepted() {
                Ok(answer) => {
                    let done = outcome(&answer);
                    written.extend(done.written);
                    if let Some((place, kind)) = done.refused {
                        failure = Some(anyhow!(import_code(&kind, first + place + 1)));
                    }
                }
                // The whole request was refused: nothing of it was written.
                Err(error) => failure = Some(error),
            }
        }
        self.written(false).await;
        if let Some(error) = failure {
            let clean = self.take_back(name, &written, &id).await;
            if unknown || !clean {
                bail!("elasticsearch_import_incomplete");
            }
            return Err(error);
        }
        self.call("POST", &format!("{index}/_refresh"), None, &id, 1024 * 1024)
            .await?;
        Ok(QueryResult {
            affected: rows.len() as u64,
            duration_ms: started.elapsed().as_millis() as u64,
            ..QueryResult::default()
        })
    }
}

/// The id of the document a key of the grid points at.
fn browse_id(key: &[(String, Cell)]) -> Result<String> {
    super::browse::id_of(key)
}

/// The document a new row stands for, and the id it was given.
fn new_document(
    layout: &Layout,
    values: &[(String, Cell)],
) -> Result<(Option<String>, Map<String, Value>)> {
    mapping::document(
        layout,
        values.iter().map(|(name, cell)| (name.as_str(), cell)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_is_compared_with_what_the_grid_showed() {
        let text = |value: &str| Cell::Text(value.into());
        assert!(same(&text("a"), &text("a")));
        assert!(!same(&text("a"), &text("A")));
        assert!(same(&Cell::Null, &Cell::Null));
        assert!(!same(&Cell::Null, &text("")));
        // A number is the same however it is written.
        assert!(same(
            &Cell::Decimal("12.50".into()),
            &Cell::Float("12.5".into())
        ));
        assert!(same(
            &Cell::Integer("7".into()),
            &Cell::Decimal("7.0".into())
        ));
        assert!(!same(
            &Cell::Integer("7".into()),
            &Cell::Integer("8".into())
        ));
        assert!(!same(&Cell::Integer("7".into()), &text("7")));
        // JSON by what it says, not by how it is spaced.
        assert!(same(
            &Cell::Json(r#"{"a":1,"b":[1,2]}"#.into()),
            &Cell::Json(r#"{ "b": [1, 2], "a": 1 }"#.into())
        ));
        assert!(!same(
            &Cell::Json(r#"[1,2]"#.into()),
            &Cell::Json(r#"[2,1]"#.into())
        ));
        assert!(same(&Cell::Bytes(vec![1, 2]), &Cell::Bytes(vec![1, 2])));
    }

    #[test]
    fn a_write_names_the_change_it_was_read_at() {
        let located = |routing: Option<&str>| super::super::Located {
            index: "bazaqa-orders".into(),
            routing: routing.map(str::to_owned),
            seq_no: 41,
            primary_term: 3,
            source: Value::Null,
        };
        assert_eq!(
            guard(&located(None)),
            "if_seq_no=41&if_primary_term=3&refresh=true"
        );
        assert_eq!(
            guard(&located(Some("client/7&x"))),
            "if_seq_no=41&if_primary_term=3&refresh=true&routing=client%2F7%26x"
        );
    }

    #[test]
    fn the_answer_of_a_bulk_says_what_was_written_and_what_was_refused() {
        let answer = json!({ "errors": true, "items": [
            { "create": { "_id": "a", "status": 201, "_seq_no": 5, "_primary_term": 1 } },
            { "index": { "_id": "generated", "status": 201, "_seq_no": 6, "_primary_term": 1 } },
            { "create": { "_id": "b", "status": 400, "error": { "type": "mapper_parsing_exception" } } },
            { "create": { "_id": "c", "status": 409, "error": { "type": "version_conflict_engine_exception" } } },
            { "create": { "_id": "d", "status": 201, "_seq_no": 7, "_primary_term": 1 } },
        ] });
        let done = outcome(&answer);
        assert_eq!(
            done.written
                .iter()
                .map(|document| (document.id.as_str(), document.seq_no, document.primary_term))
                .collect::<Vec<_>>(),
            [("a", 5, 1), ("generated", 6, 1), ("d", 7, 1)]
        );
        assert_eq!(
            done.refused,
            Some((2, "mapper_parsing_exception".to_owned()))
        );
        let clean = outcome(&json!({ "errors": false, "items": [
            { "delete": { "_id": "a", "status": 200, "_seq_no": 9, "_primary_term": 1 } },
        ] }));
        assert!(clean.refused.is_none());
        assert!(outcome(&json!({})).written.is_empty());
        // What a refused row is called, with the row of the file it is.
        assert_eq!(
            import_code("mapper_parsing_exception", 12),
            "elasticsearch_import_value_rejected_at_row_12"
        );
        assert_eq!(
            import_code("document_parsing_exception", 1),
            "elasticsearch_import_value_rejected_at_row_1"
        );
        assert_eq!(
            import_code("version_conflict_engine_exception", 3),
            "elasticsearch_import_id_exists"
        );
        assert_eq!(
            import_code("cluster_block_exception", 3),
            "elasticsearch_blocked"
        );
        assert_eq!(
            import_code("something_else", 4),
            "elasticsearch_import_refused_at_row_4"
        );
    }

    #[test]
    fn a_body_of_lines_is_one_document_per_line() {
        let mut body = Vec::new();
        line(&mut body, &json!({ "create": { "_id": "a\nb" } }));
        line(&mut body, &json!({ "title": "x", "n": 1 }));
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "{\"create\":{\"_id\":\"a\\nb\"}}\n{\"n\":1,\"title\":\"x\"}\n"
        );
    }
}
