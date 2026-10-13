//! Changes the grid makes: one cell, one new row, one removed row, and the
//! rows of an imported file.
//!
//! A change is first resolved into the commands it stands for ([`Op`]): that
//! is the text the history shows. It is then made under `WATCH`: the value
//! the grid showed is compared with what the server holds, and the commands
//! run as one transaction, so somebody else's change in between ends in
//! `row_changed_reload_required` and nothing is written.
use super::browse::{Container, Kind, Member, RowId, View, whole_collection};
use super::link::{Link, command, value_fail};
use super::value::{
    self, Collection, bytes_of, name_of, parse_score, reply_bytes, reply_int, reply_text,
};
use super::{Db, RedisSession, command as console};
use crate::drivers::RowChange;
use crate::model::{Cell, QueryResult, TableRef};
use anyhow::{Result, anyhow, bail, ensure};
use redis::{Cmd, Value};
use std::collections::HashSet;
use tokio_util::sync::CancellationToken;

/// Stands in for a list element between the two commands that remove it.
const REMOVED: &str = "__baza_studio_removed_element__";
/// Fields of a hash or members written by one command.
const WRITE_STEP: usize = 500;

type Bytes = Vec<u8>;

/// What a new key holds.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Content {
    Text(Bytes),
    Members(Collection),
    Json(String),
    Entry(Vec<(Bytes, Bytes)>),
}

/// A change of the grid, resolved.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Op {
    Rename {
        key: Bytes,
        to: Bytes,
    },
    /// `was` is the lifetime the grid showed, `seconds` the new one.
    Expire {
        key: Bytes,
        was: Option<i64>,
        seconds: Option<i64>,
    },
    SetText {
        key: Bytes,
        old: Bytes,
        new: Bytes,
    },
    /// Replace a whole small collection by the one written as JSON.
    Replace {
        key: Bytes,
        kind: &'static str,
        old: Collection,
        new: Collection,
    },
    SetJson {
        key: Bytes,
        old: String,
        new: String,
    },
    Create {
        key: Bytes,
        content: Content,
        ttl: Option<i64>,
    },
    Delete {
        key: Bytes,
    },
    HashSet {
        key: Bytes,
        field: Bytes,
        old: Bytes,
        new: Bytes,
    },
    HashRename {
        key: Bytes,
        field: Bytes,
        to: Bytes,
    },
    HashAdd {
        key: Bytes,
        field: Bytes,
        value: Bytes,
    },
    HashDelete {
        key: Bytes,
        field: Bytes,
    },
    ListSet {
        key: Bytes,
        index: i64,
        old: Bytes,
        new: Bytes,
    },
    ListPush {
        key: Bytes,
        value: Bytes,
        front: bool,
    },
    ListDelete {
        key: Bytes,
        index: i64,
        value: Bytes,
    },
    SetRename {
        key: Bytes,
        member: Bytes,
        to: Bytes,
    },
    SetAdd {
        key: Bytes,
        member: Bytes,
    },
    SetDelete {
        key: Bytes,
        member: Bytes,
    },
    ZsetScore {
        key: Bytes,
        member: Bytes,
        old: String,
        new: String,
    },
    ZsetRename {
        key: Bytes,
        member: Bytes,
        to: Bytes,
    },
    ZsetAdd {
        key: Bytes,
        member: Bytes,
        score: String,
    },
    ZsetDelete {
        key: Bytes,
        member: Bytes,
    },
    StreamAdd {
        key: Bytes,
        id: String,
        fields: Vec<(Bytes, Bytes)>,
    },
    StreamDelete {
        key: Bytes,
        id: String,
    },
}

fn words<const N: usize>(parts: [&[u8]; N]) -> Vec<Bytes> {
    parts.iter().map(|part| part.to_vec()).collect()
}

/// The commands that write a collection into a key that does not hold one.
fn write_collection(key: &[u8], collection: &Collection) -> Vec<Vec<Bytes>> {
    let mut commands = Vec::new();
    let mut add = |name: &[u8], items: Vec<Bytes>, width: usize| {
        for chunk in items.chunks(WRITE_STEP * width) {
            let mut command = vec![name.to_vec(), key.to_vec()];
            command.extend(chunk.iter().cloned());
            commands.push(command);
        }
    };
    match collection {
        Collection::Hash(pairs) => add(
            b"HSET",
            pairs
                .iter()
                .flat_map(|(name, value)| [name.clone(), value.clone()])
                .collect(),
            2,
        ),
        Collection::List(items) => add(b"RPUSH", items.clone(), 1),
        Collection::Set(items) => add(b"SADD", items.clone(), 1),
        Collection::Zset(pairs) => add(
            b"ZADD",
            pairs
                .iter()
                .flat_map(|(member, score)| [score.clone().into_bytes(), member.clone()])
                .collect(),
            2,
        ),
    }
    commands
}

fn entry_command(key: &[u8], id: &str, fields: &[(Bytes, Bytes)]) -> Vec<Bytes> {
    let mut command = words([b"XADD", key, id.as_bytes()]);
    for (name, value) in fields {
        command.push(name.clone());
        command.push(value.clone());
    }
    command
}

impl Op {
    /// The key the change is about.
    fn key(&self) -> &Bytes {
        match self {
            Op::Rename { key, .. }
            | Op::Expire { key, .. }
            | Op::SetText { key, .. }
            | Op::Replace { key, .. }
            | Op::SetJson { key, .. }
            | Op::Create { key, .. }
            | Op::Delete { key }
            | Op::HashSet { key, .. }
            | Op::HashRename { key, .. }
            | Op::HashAdd { key, .. }
            | Op::HashDelete { key, .. }
            | Op::ListSet { key, .. }
            | Op::ListPush { key, .. }
            | Op::ListDelete { key, .. }
            | Op::SetRename { key, .. }
            | Op::SetAdd { key, .. }
            | Op::SetDelete { key, .. }
            | Op::ZsetScore { key, .. }
            | Op::ZsetRename { key, .. }
            | Op::ZsetAdd { key, .. }
            | Op::ZsetDelete { key, .. }
            | Op::StreamAdd { key, .. }
            | Op::StreamDelete { key, .. } => key,
        }
    }

    /// The commands of the change, as the console would be given them.
    pub(super) fn commands(&self) -> Vec<Vec<Bytes>> {
        match self {
            Op::Rename { key, to } => vec![words([b"RENAMENX", key, to])],
            Op::Expire {
                key,
                seconds: Some(seconds),
                ..
            } => {
                vec![words([b"EXPIRE", key, seconds.to_string().as_bytes()])]
            }
            Op::Expire {
                key, seconds: None, ..
            } => vec![words([b"PERSIST", key])],
            Op::SetText { key, new, .. } => vec![words([b"SET", key, new, b"KEEPTTL"])],
            Op::Replace { key, new, .. } => {
                let mut commands = vec![words([b"DEL", key])];
                commands.extend(write_collection(key, new));
                commands
            }
            Op::SetJson { key, new, .. } => vec![words([b"JSON.SET", key, b"$", new.as_bytes()])],
            Op::Create { key, content, ttl } => {
                let mut commands = match content {
                    Content::Text(text) => vec![words([b"SET", key, text, b"NX"])],
                    Content::Members(collection) => write_collection(key, collection),
                    Content::Json(json) => {
                        vec![words([b"JSON.SET", key, b"$", json.as_bytes(), b"NX"])]
                    }
                    Content::Entry(fields) => vec![entry_command(key, "*", fields)],
                };
                if let Some(seconds) = ttl {
                    commands.push(words([b"EXPIRE", key, seconds.to_string().as_bytes()]));
                }
                commands
            }
            Op::Delete { key } => vec![words([b"DEL", key])],
            Op::HashSet {
                key, field, new, ..
            } => vec![words([b"HSET", key, field, new])],
            Op::HashRename { key, field, to } => vec![
                words([b"HSET", key, to, b"<value>"]),
                words([b"HDEL", key, field]),
            ],
            Op::HashAdd { key, field, value } => vec![words([b"HSETNX", key, field, value])],
            Op::HashDelete { key, field } => vec![words([b"HDEL", key, field])],
            Op::ListSet {
                key, index, new, ..
            } => {
                vec![words([b"LSET", key, index.to_string().as_bytes(), new])]
            }
            Op::ListPush {
                key,
                value,
                front: true,
            } => vec![words([b"LPUSH", key, value])],
            Op::ListPush {
                key,
                value,
                front: false,
            } => vec![words([b"RPUSH", key, value])],
            Op::ListDelete { key, index, .. } => vec![
                words([
                    b"LSET",
                    key,
                    index.to_string().as_bytes(),
                    REMOVED.as_bytes(),
                ]),
                words([b"LREM", key, b"1", REMOVED.as_bytes()]),
            ],
            Op::SetRename { key, member, to } => {
                vec![words([b"SREM", key, member]), words([b"SADD", key, to])]
            }
            Op::SetAdd { key, member } => vec![words([b"SADD", key, member])],
            Op::SetDelete { key, member } => vec![words([b"SREM", key, member])],
            Op::ZsetScore {
                key, member, new, ..
            } => {
                vec![words([b"ZADD", key, b"XX", new.as_bytes(), member])]
            }
            Op::ZsetRename { key, member, to } => vec![
                words([b"ZADD", key, b"<score>", to]),
                words([b"ZREM", key, member]),
            ],
            Op::ZsetAdd { key, member, score } => {
                vec![words([b"ZADD", key, b"NX", score.as_bytes(), member])]
            }
            Op::ZsetDelete { key, member } => vec![words([b"ZREM", key, member])],
            Op::StreamAdd { key, id, fields } => vec![entry_command(key, id, fields)],
            Op::StreamDelete { key, id } => vec![words([b"XDEL", key, id.as_bytes()])],
        }
    }

    /// The change as text, one command per line.
    pub(super) fn text(&self, db: i64, home: i64) -> String {
        let mut lines = Vec::new();
        if db != home {
            lines.push(format!("SELECT {db}"));
        }
        lines.extend(self.commands().into_iter().map(console::text));
        lines.join("\n")
    }
}

fn required(values: &[(String, Cell)], column: &str, code: &'static str) -> Result<Bytes> {
    values
        .iter()
        .find(|(name, _)| name == column)
        .and_then(|(_, cell)| bytes_of(cell))
        .ok_or_else(|| anyhow!(code))
}

fn optional(values: &[(String, Cell)], column: &str) -> Option<Bytes> {
    values
        .iter()
        .find(|(name, _)| name == column)
        .and_then(|(_, cell)| bytes_of(cell))
}

fn text_of_bytes(bytes: &[u8], code: &'static str) -> Result<String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| anyhow!(code))
}

/// A lifetime in seconds from a cell: nothing for NULL or an empty text.
fn lifetime(cell: Option<&Cell>) -> Result<Option<i64>> {
    let text = match cell {
        None | Some(Cell::Null) => return Ok(None),
        Some(cell) => cell.display(),
    };
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let seconds: i64 = text.parse().map_err(|_| anyhow!("redis_ttl_invalid"))?;
    ensure!(
        (1..=i64::MAX / 1000).contains(&seconds),
        "redis_ttl_invalid"
    );
    Ok(Some(seconds))
}

/// The content of a new key of this type from the text of its value.
fn content(type_name: &str, value: Option<Bytes>) -> Result<Content> {
    let json = |value: Option<Bytes>| -> Result<String> {
        text_of_bytes(
            &value.ok_or_else(|| anyhow!("redis_value_required"))?,
            "redis_json_shape_invalid",
        )
    };
    Ok(match type_name.trim().to_ascii_lowercase().as_str() {
        "" | "string" => Content::Text(value.unwrap_or_default()),
        kind @ ("hash" | "list" | "set" | "zset") => {
            let kind = ["hash", "list", "set", "zset"]
                .into_iter()
                .find(|name| *name == kind)
                .unwrap_or("hash");
            Content::Members(Collection::parse(kind, &json(value)?)?)
        }
        "stream" => Content::Entry(value::parse_entry(&json(value)?)?),
        "json" | "rejson-rl" => {
            let text = json(value)?;
            serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|_| anyhow!("redis_json_shape_invalid"))?;
            Content::Json(text)
        }
        _ => bail!("redis_type_unknown"),
    })
}

/// The type a key of the key list has, as the grid last showed it, or as
/// the shape of its JSON value says.
fn shown_type(session: &RedisSession, db: i64, key: &[u8], old: &Cell) -> Result<&'static str> {
    let known = session.shown_type(db, key);
    Ok(match (old, known.as_deref()) {
        (Cell::Json(_), Some("hash")) => "hash",
        (Cell::Json(_), Some("list")) => "list",
        (Cell::Json(_), Some("set")) => "set",
        (Cell::Json(_), Some("zset")) => "zset",
        (Cell::Json(_), Some("ReJSON-RL")) => "ReJSON-RL",
        (Cell::Json(text), _) if text.trim_start().starts_with('{') => "hash",
        (Cell::Json(_), _) => "list",
        (Cell::Null, _) => bail!("row_changed_reload_required"),
        // A preview is not the value: it cannot be the old side of a change.
        (other, _) if bytes_of(other).is_none() => bail!("redis_value_read_only"),
        _ => "string",
    })
}

/// Resolve a change of the grid into what it does.
pub(super) fn resolve(session: &RedisSession, change: &RowChange) -> Result<(i64, Op)> {
    let table: &TableRef = match change {
        RowChange::UpdateCell { table, .. }
        | RowChange::InsertRow { table, .. }
        | RowChange::DeleteRow { table, .. } => table,
    };
    let (db, kind) = match Container::parse(&table.name)? {
        Container::Data(db, kind) => (db, kind),
        Container::View(View::Clients | View::Databases | View::Info | View::SlowLog) => {
            bail!("result_read_only")
        }
    };
    let new_bytes = |cell: &Cell, code: &'static str| bytes_of(cell).ok_or_else(|| anyhow!(code));
    let op = match change {
        RowChange::UpdateCell {
            column,
            old,
            new,
            key,
            ..
        } => {
            let name = name_of(key, "key")?;
            match (kind, column.as_str()) {
                (Kind::Keys, "key") => Op::Rename {
                    key: name,
                    to: new_bytes(new, "redis_key_required")?,
                },
                (Kind::Keys, "ttl") => Op::Expire {
                    key: name,
                    was: lifetime(Some(old)).unwrap_or(None),
                    seconds: lifetime(Some(new))?,
                },
                (Kind::Keys, "value") => match shown_type(session, db, &name, old)? {
                    "string" => Op::SetText {
                        key: name,
                        old: new_bytes(old, "redis_value_read_only")?,
                        new: new_bytes(new, "redis_value_required")?,
                    },
                    "ReJSON-RL" => Op::SetJson {
                        key: name,
                        old: old.display(),
                        new: text_of_bytes(
                            &new_bytes(new, "redis_value_required")?,
                            "redis_json_shape_invalid",
                        )?,
                    },
                    collection => Op::Replace {
                        key: name,
                        kind: collection,
                        old: Collection::parse(collection, &old.display())
                            .map_err(|_| anyhow!("row_changed_reload_required"))?,
                        new: Collection::parse(
                            collection,
                            &text_of_bytes(
                                &new_bytes(new, "redis_value_required")?,
                                "redis_json_shape_invalid",
                            )?,
                        )?,
                    },
                },
                (Kind::Keys, _) => bail!("redis_column_read_only"),
                // A member stays with its key.
                (_, "key") => bail!("redis_member_key_fixed"),
                (Kind::Hash, "value") => Op::HashSet {
                    field: name_of(key, "field")?,
                    key: name,
                    old: new_bytes(old, "redis_value_read_only")?,
                    new: new_bytes(new, "redis_value_required")?,
                },
                (Kind::Hash, "field") => Op::HashRename {
                    field: name_of(key, "field")?,
                    key: name,
                    to: new_bytes(new, "redis_member_required")?,
                },
                (Kind::List, "value") => Op::ListSet {
                    index: index_of(key)?,
                    key: name,
                    old: new_bytes(old, "redis_value_read_only")?,
                    new: new_bytes(new, "redis_value_required")?,
                },
                (Kind::List, _) => bail!("redis_column_read_only"),
                (Kind::Set, "member") => Op::SetRename {
                    member: name_of(key, "member")?,
                    key: name,
                    to: new_bytes(new, "redis_member_required")?,
                },
                (Kind::Zset, "score") => Op::ZsetScore {
                    member: name_of(key, "member")?,
                    key: name,
                    old: old.display(),
                    new: parse_score(&new.display())?,
                },
                (Kind::Zset, "member") => Op::ZsetRename {
                    member: name_of(key, "member")?,
                    key: name,
                    to: new_bytes(new, "redis_member_required")?,
                },
                (Kind::Stream, _) => bail!("redis_stream_entry_fixed"),
                _ => bail!("redis_column_read_only"),
            }
        }
        RowChange::InsertRow { values, .. } => insert(kind, values, session.single_key(db, kind))?,
        RowChange::DeleteRow { key, .. } => {
            let name = name_of(key, "key")?;
            match kind {
                Kind::Keys => Op::Delete { key: name },
                Kind::Hash => Op::HashDelete {
                    field: name_of(key, "field")?,
                    key: name,
                },
                Kind::List => Op::ListDelete {
                    index: index_of(key)?,
                    value: name_of(key, "value").map_err(|_| anyhow!("redis_value_read_only"))?,
                    key: name,
                },
                Kind::Set => Op::SetDelete {
                    member: name_of(key, "member")?,
                    key: name,
                },
                Kind::Zset => Op::ZsetDelete {
                    member: name_of(key, "member")?,
                    key: name,
                },
                Kind::Stream => Op::StreamDelete {
                    id: text_of_bytes(&name_of(key, "id")?, "primary_key_required")?,
                    key: name,
                },
            }
        }
    };
    if let Op::Rename { to, .. } = &op {
        ensure!(!to.is_empty(), "redis_key_required");
    }
    Ok((db, op))
}

fn index_of(key: &[(String, Cell)]) -> Result<i64> {
    String::from_utf8_lossy(&name_of(key, "index")?)
        .parse()
        .map_err(|_| anyhow!("primary_key_required"))
}

/// A new row of a container as a change.
///
/// `shown` is the key whose members the list shows, when it shows the
/// members of one key: a new member without a key is a member of that one.
fn insert(kind: Kind, values: &[(String, Cell)], shown: Option<Bytes>) -> Result<Op> {
    let key = match optional(values, "key") {
        Some(key) => key,
        None => shown
            .filter(|_| kind != Kind::Keys)
            .ok_or_else(|| anyhow!("redis_key_required"))?,
    };
    ensure!(!key.is_empty(), "redis_key_required");
    let cell = |column: &str| {
        values
            .iter()
            .find(|(name, _)| name == column)
            .map(|(_, cell)| cell)
    };
    Ok(match kind {
        Kind::Keys => {
            // The type as it is shown: a file of a page holds it in the
            // read-only cell of the key list.
            let type_name = match cell("type") {
                None | Some(Cell::Null) => String::new(),
                Some(cell) => cell.display(),
            };
            Op::Create {
                content: content(&type_name, optional(values, "value"))?,
                ttl: lifetime(cell("ttl"))?,
                key,
            }
        }
        Kind::Hash => Op::HashAdd {
            key,
            field: required(values, "field", "redis_member_required")?,
            value: optional(values, "value").unwrap_or_default(),
        },
        Kind::List => {
            let front = match cell("index").map(Cell::display).as_deref().map(str::trim) {
                None | Some("" | "NULL") => false,
                Some("0") => true,
                Some(_) => bail!("redis_list_index_invalid"),
            };
            Op::ListPush {
                key,
                value: required(values, "value", "redis_value_required")?,
                front,
            }
        }
        Kind::Set => Op::SetAdd {
            key,
            member: required(values, "member", "redis_member_required")?,
        },
        Kind::Zset => Op::ZsetAdd {
            key,
            member: required(values, "member", "redis_member_required")?,
            score: parse_score(&cell("score").map(Cell::display).unwrap_or_default())?,
        },
        Kind::Stream => {
            let id = optional(values, "id")
                .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| "*".into());
            let entry = required(values, "entry", "redis_entry_invalid")?;
            Op::StreamAdd {
                key,
                id,
                fields: value::parse_entry(&text_of_bytes(&entry, "redis_entry_invalid")?)?,
            }
        }
    })
}

fn build(words: &[Vec<Bytes>]) -> Vec<Cmd> {
    words.iter().map(command).collect()
}

/// A reply that must not be an error.
fn accepted(reply: Value) -> Result<Value> {
    match reply {
        Value::ServerError(error) => Err(anyhow!(value_fail(&error).code())),
        other => Ok(other),
    }
}

/// Run commands as a transaction over what is being watched.
async fn commit(link: &Link, commands: Vec<Cmd>) -> Result<Vec<Value>> {
    match link.atomic(commands).await? {
        None => bail!("row_changed_reload_required"),
        Some(replies) => replies.into_iter().map(accepted).collect(),
    }
}

async fn unwatch(link: &Link) {
    let _ = link.call(&command(["UNWATCH"])).await;
}

const CONFLICT: &str = "row_changed_reload_required";

/// The remaining lifetime of a key as the command that gives it back after
/// the key was written anew.
async fn keep_lifetime(link: &Link, key: &[u8]) -> Result<Option<Cmd>> {
    let left = reply_int(&link.call(&command([b"PTTL".as_slice(), key])).await?).unwrap_or(-1);
    Ok((left > 0).then(|| command([b"PEXPIRE".as_slice(), key, left.to_string().as_bytes()])))
}

/// What has to hold for a change, checked while the key is watched. Returns
/// the commands to run.
async fn checked(link: &Link, op: &Op) -> Result<Vec<Cmd>> {
    let call = async |parts: &[&[u8]]| -> Result<Value> { Ok(link.call(&command(parts)).await?) };
    let type_of = async |key: &[u8]| -> Result<String> {
        Ok(reply_text(&link.call(&command([b"TYPE".as_slice(), key])).await?).unwrap_or_default())
    };
    let mut commands = build(&op.commands());
    match op {
        Op::Expire { key, was, .. } => {
            let now = reply_int(&call(&[b"TTL", key]).await?).unwrap_or(-2);
            // A lifetime runs down between two readings: it only has to be
            // the same kind of lifetime, and not a longer one.
            let same = match was {
                None => now == -1,
                Some(shown) => now >= 0 && now <= shown.saturating_add(2),
            };
            ensure!(same, CONFLICT);
        }
        Op::SetText { key, old, new } => {
            ensure!(type_of(key).await? == "string", CONFLICT);
            let held = reply_bytes(&call(&[b"GET", key]).await?);
            ensure!(held.as_ref() == Some(old), CONFLICT);
            // `KEEPTTL` is what the history shows; servers older than it
            // get the remaining lifetime spelled out.
            let left = reply_int(&call(&[b"PTTL", key]).await?).unwrap_or(-1);
            commands = vec![if left > 0 {
                command([
                    b"SET".as_slice(),
                    key,
                    new,
                    b"PX",
                    left.to_string().as_bytes(),
                ])
            } else {
                command([b"SET".as_slice(), key, new])
            }];
        }
        Op::Replace { key, kind, old, .. } => {
            ensure!(type_of(key).await? == *kind, CONFLICT);
            let held = whole_collection(link, kind, key).await?;
            ensure!(held.is_some_and(|held| held.same(old)), CONFLICT);
            commands.extend(keep_lifetime(link, key).await?);
        }
        Op::SetJson { key, old, .. } => {
            ensure!(type_of(key).await? == "ReJSON-RL", CONFLICT);
            let held = reply_text(&call(&[b"JSON.GET", key]).await?).unwrap_or_default();
            let parse = |text: &str| serde_json::from_str::<serde_json::Value>(text).ok();
            ensure!(
                parse(&held).is_some() && parse(&held) == parse(old),
                CONFLICT
            );
        }
        Op::Create { key, .. } => {
            ensure!(
                reply_int(&call(&[b"EXISTS", key]).await?) == Some(0),
                "redis_key_exists"
            );
            // The key is new: nothing of it is there to refuse `NX`.
            commands = build(
                &op.commands()
                    .into_iter()
                    .map(|mut words| {
                        if words.last().is_some_and(|word| word == b"NX") {
                            words.pop();
                        }
                        words
                    })
                    .collect::<Vec<_>>(),
            );
        }
        Op::HashSet {
            key, field, old, ..
        } => {
            let held = reply_bytes(&call(&[b"HGET", key, field]).await?);
            ensure!(held.as_ref() == Some(old), CONFLICT);
        }
        Op::HashRename { key, field, to } => {
            let held = reply_bytes(&call(&[b"HGET", key, field]).await?)
                .ok_or_else(|| anyhow!(CONFLICT))?;
            ensure!(
                reply_int(&call(&[b"HEXISTS", key, to]).await?) == Some(0),
                "redis_member_exists"
            );
            commands = vec![
                command([b"HSET".as_slice(), key, to, &held]),
                command([b"HDEL".as_slice(), key, field]),
            ];
        }
        Op::ListSet {
            key, index, old, ..
        } => {
            let held = reply_bytes(&call(&[b"LINDEX", key, index.to_string().as_bytes()]).await?);
            ensure!(held.as_ref() == Some(old), CONFLICT);
        }
        Op::ListDelete { key, index, value } => {
            let held = reply_bytes(&call(&[b"LINDEX", key, index.to_string().as_bytes()]).await?);
            ensure!(held.as_ref() == Some(value), CONFLICT);
        }
        Op::SetRename { key, member, to } => {
            ensure!(
                reply_int(&call(&[b"SISMEMBER", key, member]).await?) == Some(1),
                CONFLICT
            );
            ensure!(
                reply_int(&call(&[b"SISMEMBER", key, to]).await?) == Some(0),
                "redis_member_exists"
            );
        }
        Op::ZsetScore {
            key, member, old, ..
        } => {
            let held = reply_text(&call(&[b"ZSCORE", key, member]).await?)
                .ok_or_else(|| anyhow!(CONFLICT))?;
            let number = |text: &str| text.trim().parse::<f64>().ok();
            ensure!(
                number(&held).is_some() && number(&held) == number(old),
                CONFLICT
            );
        }
        Op::ZsetRename { key, member, to } => {
            let score = reply_text(&call(&[b"ZSCORE", key, member]).await?)
                .ok_or_else(|| anyhow!(CONFLICT))?;
            ensure!(
                matches!(call(&[b"ZSCORE", key, to]).await?, Value::Nil),
                "redis_member_exists"
            );
            commands = vec![
                command([b"ZADD".as_slice(), key, score.as_bytes(), to]),
                command([b"ZREM".as_slice(), key, member]),
            ];
        }
        _ => {}
    }
    Ok(commands)
}

/// Make a resolved change, or none of it.
async fn run(db: &Db, op: &Op) -> Result<()> {
    let link = &db.link;
    let one = async |expected_gone: &'static str| -> Result<()> {
        let commands = op.commands();
        let reply = link.call(&command(&commands[0])).await?;
        // These commands answer with the number of things they changed.
        ensure!(reply_int(&reply) != Some(0), expected_gone);
        Ok(())
    };
    match op {
        Op::Rename { key, to } => {
            if key == to {
                return Ok(());
            }
            match link.call(&command([b"RENAMENX".as_slice(), key, to])).await {
                Ok(reply) => ensure!(reply_int(&reply) == Some(1), "redis_key_exists"),
                Err(fail) if fail.code() == "redis_no_such_key" => bail!(CONFLICT),
                Err(fail) => return Err(fail.into()),
            }
            Ok(())
        }
        // One command that says by itself whether it found its row.
        Op::Delete { .. }
        | Op::HashDelete { .. }
        | Op::SetDelete { .. }
        | Op::ZsetDelete { .. }
        | Op::StreamDelete { .. } => one(CONFLICT).await,
        Op::HashAdd { .. } | Op::SetAdd { .. } | Op::ZsetAdd { .. } => {
            one("redis_member_exists").await
        }
        Op::ListPush { .. } | Op::StreamAdd { .. } => {
            link.call(&command(&op.commands()[0])).await?;
            Ok(())
        }
        Op::Create {
            key,
            content: Content::Text(text),
            ttl,
        } => {
            let mut words: Vec<Bytes> = words([b"SET", key, text, b"NX"]);
            if let Some(seconds) = ttl {
                words.push(b"EX".to_vec());
                words.push(seconds.to_string().into_bytes());
            }
            let reply = link.call(&command(words)).await?;
            ensure!(!matches!(reply, Value::Nil), "redis_key_exists");
            Ok(())
        }
        _ => {
            let _editing = db.edit.lock().await;
            link.call(&command([b"WATCH".as_slice(), op.key()])).await?;
            let commands = match checked(link, op).await {
                Ok(commands) => commands,
                Err(error) => {
                    unwatch(link).await;
                    return Err(error);
                }
            };
            commit(link, commands).await?;
            Ok(())
        }
    }
}

/// Make one change of one record, or none at all.
pub(super) async fn apply(session: &RedisSession, change: &RowChange) -> Result<()> {
    let (db, op) = resolve(session, change)?;
    let database = session.database(db).await?;
    run(&database, &op).await?;
    // The lists that are open follow the change without a new scan.
    let mut scans = session.scans.lock().await;
    let id = |key: &Bytes, member: Member| RowId {
        key: key.clone(),
        member,
    };
    match &op {
        Op::Rename { key, to } => scans.each(db, |scan| scan.rename_key(key, to)),
        Op::Delete { key } => scans.each(db, |scan| scan.forget_key(key)),
        Op::HashRename { key, field, to }
        | Op::SetRename {
            key,
            member: field,
            to,
        }
        | Op::ZsetRename {
            key,
            member: field,
            to,
        } => scans.each(db, |scan| {
            scan.rename(
                &id(key, Member::Name(field.clone())),
                id(key, Member::Name(to.clone())),
            )
        }),
        Op::HashDelete { key, field }
        | Op::SetDelete { key, member: field }
        | Op::ZsetDelete { key, member: field } => scans.each(db, |scan| {
            scan.forget(&id(key, Member::Name(field.clone())))
        }),
        Op::StreamDelete { key, id: entry } => scans.each(db, |scan| {
            scan.forget(&id(key, Member::Entry(entry.clone())))
        }),
        Op::Create { key, .. }
        | Op::HashAdd { key, .. }
        | Op::ListPush { key, .. }
        | Op::SetAdd { key, .. }
        | Op::ZsetAdd { key, .. }
        | Op::StreamAdd { key, .. } => session.remember_new(db, key),
        _ => {}
    }
    Ok(())
}

/// What a change looks like as text, for the history.
pub(super) fn change_text(session: &RedisSession, change: &RowChange) -> Result<String> {
    let (db, op) = resolve(session, change)?;
    Ok(op.text(db, session.home))
}

/// The cells of an imported row by the name of their column.
fn named<'a>(columns: &'a [String], row: &'a [Cell]) -> Vec<(String, Cell)> {
    columns.iter().cloned().zip(row.iter().cloned()).collect()
}

/// A stream id as two numbers.
fn entry_id(id: &str) -> Option<(u64, u64)> {
    let (time, sequence) = id.split_once('-')?;
    Some((time.parse().ok()?, sequence.parse().ok()?))
}

/// Add the rows of a file to a container, all of them or none: everything
/// that could refuse a row is checked first, while the keys are watched,
/// and the rows are then written as one transaction.
pub(super) async fn import(
    session: &RedisSession,
    container: &TableRef,
    columns: &[String],
    rows: &[Vec<Cell>],
    cancel: CancellationToken,
) -> Result<QueryResult> {
    let started = std::time::Instant::now();
    let (db, kind) = match Container::parse(&container.name)? {
        Container::Data(db, kind) => (db, kind),
        Container::View(_) => bail!("result_read_only"),
    };
    let ops: Vec<Op> = rows
        .iter()
        .map(|row| {
            let values = named(columns, row);
            // A file made from a page of the key list holds a preview where
            // a value did not fit its cell (a long text, a large collection,
            // a stream): writing that back would replace the value by its
            // beginning.
            if kind == Kind::Keys
                && let Some((_, cell)) = values.iter().find(|(name, _)| name == "value")
            {
                let cut = match cell {
                    Cell::Null => false,
                    Cell::Unsupported(_) => bytes_of(cell).is_none(),
                    other => value::looks_cut(&other.display()),
                };
                let stream = values.iter().any(|(name, cell)| {
                    name == "type" && cell.display().eq_ignore_ascii_case("stream")
                }) && !cell.display().trim_start().starts_with('{');
                ensure!(!cut && !stream, "redis_value_read_only");
            }
            insert(kind, &values, None)
        })
        .collect::<Result<_>>()?;
    if ops.is_empty() {
        return Ok(QueryResult::default());
    }
    // A row that the file holds twice is refused like one the server holds.
    let mut rows_seen = HashSet::new();
    for op in &ops {
        let (identity, code) = match op {
            Op::Create { key, .. } => ((key.clone(), Bytes::new()), "redis_key_exists"),
            Op::HashAdd { key, field, .. } => ((key.clone(), field.clone()), "redis_member_exists"),
            Op::SetAdd { key, member } | Op::ZsetAdd { key, member, .. } => {
                ((key.clone(), member.clone()), "redis_member_exists")
            }
            _ => continue,
        };
        ensure!(rows_seen.insert(identity), code);
    }
    let mut keys: Vec<Bytes> = Vec::new();
    let mut keys_seen = HashSet::new();
    for op in &ops {
        if keys_seen.insert(op.key().clone()) {
            keys.push(op.key().clone());
        }
    }
    let database = session.database(db).await?;
    let link = &database.link;
    let _editing = database.edit.lock().await;
    for chunk in keys.chunks(1000) {
        let mut watch = vec![b"WATCH".to_vec()];
        watch.extend(chunk.iter().cloned());
        link.call(&command(watch)).await?;
    }
    let verdict: Result<Vec<Cmd>> = async {
        // The keys must be free (key list) or of the type of the list.
        let types = link
            .pipe(
                keys.iter()
                    .map(|key| command([b"TYPE".as_slice(), key]))
                    .collect(),
            )
            .await?;
        for reply in &types {
            let found = reply_text(reply).unwrap_or_default();
            match kind {
                Kind::Keys => ensure!(found == "none", "redis_key_exists"),
                _ => ensure!(
                    found == "none" || found == kind.type_name(),
                    "redis_wrong_type"
                ),
            }
        }
        // No member of the file may be there already.
        let probes: Vec<Cmd> = ops
            .iter()
            .filter_map(|op| match op {
                Op::HashAdd { key, field, .. } => {
                    Some(command([b"HEXISTS".as_slice(), key, field]))
                }
                Op::SetAdd { key, member } => Some(command([b"SISMEMBER".as_slice(), key, member])),
                Op::ZsetAdd { key, member, .. } => {
                    Some(command([b"ZSCORE".as_slice(), key, member]))
                }
                _ => None,
            })
            .collect();
        for reply in link.pipe(probes).await? {
            let free = matches!(reply, Value::Nil) || reply_int(&reply) == Some(0);
            ensure!(
                free || matches!(reply, Value::ServerError(_)),
                "redis_member_exists"
            );
        }
        // Ids of stream entries: all generated, or all given and rising
        // above what the stream already holds.
        if kind == Kind::Stream {
            for key in &keys {
                let ids: Vec<&str> = ops
                    .iter()
                    .filter_map(|op| match op {
                        Op::StreamAdd { key: owner, id, .. } if owner == key => Some(id.as_str()),
                        _ => None,
                    })
                    .collect();
                if ids.iter().all(|id| *id == "*") {
                    continue;
                }
                let mut last = match link
                    .call(&command([b"XINFO".as_slice(), b"STREAM", key]))
                    .await
                {
                    Ok(info) => value::reply_pairs(info)
                        .unwrap_or_default()
                        .iter()
                        .find(|(name, _)| reply_text(name).as_deref() == Some("last-generated-id"))
                        .and_then(|(_, id)| entry_id(&reply_text(id)?)),
                    Err(_) => None,
                };
                for id in ids {
                    let next = entry_id(id).ok_or_else(|| anyhow!("redis_stream_id_invalid"))?;
                    ensure!(
                        last.is_none_or(|last| next > last),
                        "redis_stream_id_invalid"
                    );
                    last = Some(next);
                }
            }
        }
        ensure!(!cancel.is_cancelled(), "query_cancelled");
        Ok(ops
            .iter()
            .flat_map(|op| {
                let mut commands = op.commands();
                // Inside the transaction nothing has to refuse a key twice.
                for words in &mut commands {
                    if words.first().is_some_and(|name| name == b"SET")
                        && words.last().is_some_and(|word| word == b"NX")
                    {
                        words.pop();
                    }
                }
                build(&commands)
            })
            .collect())
    }
    .await;
    let commands = match verdict {
        Ok(commands) => commands,
        Err(error) => {
            unwatch(link).await;
            return Err(error);
        }
    };
    match link.atomic(commands).await? {
        None => bail!(CONFLICT),
        Some(replies) => {
            // Everything that could fail was checked; what the server still
            // refused (memory, a module) leaves the rest written.
            ensure!(
                replies
                    .iter()
                    .all(|reply| !matches!(reply, Value::ServerError(_))),
                "redis_import_incomplete"
            );
        }
    }
    for key in keys.iter().take(50) {
        session.remember_new(db, key);
    }
    Ok(QueryResult {
        affected: ops.len() as u64,
        duration_ms: started.elapsed().as_millis() as u64,
        ..QueryResult::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Cell {
        Cell::Text(value.into())
    }
    fn key(pairs: &[(&str, Cell)]) -> Vec<(String, Cell)> {
        pairs
            .iter()
            .map(|(name, cell)| ((*name).to_owned(), cell.clone()))
            .collect()
    }
    fn lines(op: &Op) -> String {
        op.text(0, 0)
    }

    #[test]
    fn a_new_row_of_the_key_list_is_a_key_of_the_chosen_type() {
        let row = |pairs: &[(&str, Cell)]| insert(Kind::Keys, &key(pairs), None);
        // Only the key and the value: a string without a lifetime.
        let op = row(&[
            ("key", text("greeting")),
            ("ttl", Cell::Null),
            ("value", text("cześć")),
        ])
        .unwrap();
        assert_eq!(lines(&op), "SET greeting cześć NX");
        let op = row(&[
            ("key", text("session 1")),
            ("ttl", text("60")),
            ("value", text("a b")),
        ])
        .unwrap();
        assert_eq!(
            lines(&op),
            "SET \"session 1\" \"a b\" NX\nEXPIRE \"session 1\" 60"
        );
        let op = row(&[
            ("key", text("user:1")),
            ("type", text("hash")),
            ("value", text(r#"{"name": "Jan", "age": 41}"#)),
        ])
        .unwrap();
        assert_eq!(lines(&op), "HSET user:1 age 41 name Jan");
        let op = row(&[
            ("key", text("q")),
            ("type", text("List")),
            ("value", text(r#"["a", "b"]"#)),
        ])
        .unwrap();
        assert_eq!(lines(&op), "RPUSH q a b");
        let op = row(&[
            ("key", text("z")),
            ("type", text("zset")),
            ("value", text(r#"{"a": 1.5}"#)),
        ])
        .unwrap();
        assert_eq!(lines(&op), "ZADD z 1.5 a");
        let op = row(&[
            ("key", text("s")),
            ("type", text("stream")),
            ("value", text(r#"{"t": "21"}"#)),
        ])
        .unwrap();
        assert_eq!(lines(&op), "XADD s * t 21");
        let op = row(&[
            ("key", text("doc")),
            ("type", text("json")),
            ("value", text(r#"{"a": [1]}"#)),
        ])
        .unwrap();
        assert_eq!(lines(&op), "JSON.SET doc $ \"{\\\"a\\\": [1]}\" NX");
        // What is missing or wrong is said in a code of its own.
        for (pairs, code) in [
            (vec![("value", text("x"))], "redis_key_required"),
            (
                vec![("key", Cell::Null), ("value", text("x"))],
                "redis_key_required",
            ),
            (
                vec![("key", text("")), ("value", text("x"))],
                "redis_key_required",
            ),
            (
                vec![("key", text("k")), ("type", text("table"))],
                "redis_type_unknown",
            ),
            (
                vec![("key", text("k")), ("type", text("hash"))],
                "redis_value_required",
            ),
            (
                vec![
                    ("key", text("k")),
                    ("type", text("hash")),
                    ("value", text("[1]")),
                ],
                "redis_json_shape_invalid",
            ),
            (
                vec![("key", text("k")), ("ttl", text("soon"))],
                "redis_ttl_invalid",
            ),
            (
                vec![("key", text("k")), ("ttl", text("0"))],
                "redis_ttl_invalid",
            ),
        ] {
            assert_eq!(row(&pairs).unwrap_err().to_string(), code, "{pairs:?}");
        }
        // The type as a page of the key list shows it.
        let op = row(&[
            ("key", text("l")),
            ("type", Cell::Unsupported("list".into())),
            ("size", Cell::Unsupported("2".into())),
            ("value", Cell::Json(r#"["x", "y"]"#.into())),
        ])
        .unwrap();
        assert_eq!(lines(&op), "RPUSH l x y");
        // The size is told by the server, whatever a file says.
        assert!(
            row(&[
                ("key", text("k")),
                ("size", text("99")),
                ("value", text("v"))
            ])
            .is_ok()
        );
    }

    #[test]
    fn new_members_are_written_with_the_command_of_their_type() {
        let op = insert(
            Kind::Hash,
            &key(&[
                ("key", text("h")),
                ("field", text("f")),
                ("value", text("v")),
            ]),
            None,
        )
        .unwrap();
        assert_eq!(lines(&op), "HSETNX h f v");
        // The end of a list unless the index says its start.
        let op = insert(
            Kind::List,
            &key(&[("key", text("l")), ("value", text("v"))]),
            None,
        )
        .unwrap();
        assert_eq!(lines(&op), "RPUSH l v");
        let op = insert(
            Kind::List,
            &key(&[
                ("key", text("l")),
                ("index", Cell::Integer("0".into())),
                ("value", text("v")),
            ]),
            None,
        )
        .unwrap();
        assert_eq!(lines(&op), "LPUSH l v");
        assert_eq!(
            insert(
                Kind::List,
                &key(&[
                    ("key", text("l")),
                    ("index", text("3")),
                    ("value", text("v"))
                ]),
                None
            )
            .unwrap_err()
            .to_string(),
            "redis_list_index_invalid"
        );
        let op = insert(
            Kind::Set,
            &key(&[("key", text("s")), ("member", text("m"))]),
            None,
        )
        .unwrap();
        assert_eq!(lines(&op), "SADD s m");
        let op = insert(
            Kind::Zset,
            &key(&[
                ("key", text("z")),
                ("member", text("m")),
                ("score", Cell::Float("2.5".into())),
            ]),
            None,
        )
        .unwrap();
        assert_eq!(lines(&op), "ZADD z NX 2.5 m");
        assert_eq!(
            insert(
                Kind::Zset,
                &key(&[("key", text("z")), ("member", text("m"))]),
                None
            )
            .unwrap_err()
            .to_string(),
            "redis_score_invalid"
        );
        let op = insert(
            Kind::Stream,
            &key(&[("key", text("s")), ("entry", text(r#"{"a": "1"}"#))]),
            None,
        )
        .unwrap();
        assert_eq!(lines(&op), "XADD s * a 1");
        let op = insert(
            Kind::Stream,
            &key(&[
                ("key", text("s")),
                ("id", text("5-1")),
                ("entry", text(r#"{"a": "1"}"#)),
            ]),
            None,
        )
        .unwrap();
        assert_eq!(lines(&op), "XADD s 5-1 a 1");
        assert_eq!(
            insert(
                Kind::Stream,
                &key(&[("key", text("s")), ("entry", text("{}"))]),
                None
            )
            .unwrap_err()
            .to_string(),
            "redis_entry_invalid"
        );
        assert_eq!(
            insert(Kind::Set, &key(&[("key", text("s"))]), None)
                .unwrap_err()
                .to_string(),
            "redis_member_required"
        );
    }

    #[test]
    fn a_new_member_without_a_key_belongs_to_the_key_the_list_shows() {
        let shown = Some(b"user:1".to_vec());
        let op = insert(
            Kind::Hash,
            &key(&[("field", text("phone")), ("value", text("600"))]),
            shown.clone(),
        )
        .unwrap();
        assert_eq!(lines(&op), "HSETNX user:1 phone 600");
        // A key that is given wins.
        let op = insert(
            Kind::Set,
            &key(&[("key", text("other")), ("member", text("m"))]),
            shown.clone(),
        )
        .unwrap();
        assert_eq!(lines(&op), "SADD other m");
        // A list that shows several keys, and the key list, have no such key.
        assert_eq!(
            insert(Kind::Hash, &key(&[("field", text("f"))]), None)
                .unwrap_err()
                .to_string(),
            "redis_key_required"
        );
        assert_eq!(
            insert(Kind::Keys, &key(&[("value", text("v"))]), shown)
                .unwrap_err()
                .to_string(),
            "redis_key_required"
        );
    }

    #[test]
    fn a_change_is_written_for_another_database_than_the_one_of_the_console() {
        let op = Op::Delete { key: b"k".to_vec() };
        assert_eq!(op.text(2, 0), "SELECT 2\nDEL k");
        assert_eq!(op.text(2, 2), "DEL k");
        // Bytes that are not text are written so that the console reads
        // them back as the same bytes.
        let op = Op::SetText {
            key: vec![0xff, 0x00],
            old: Vec::new(),
            new: b"line\nfeed".to_vec(),
        };
        assert_eq!(op.text(0, 0), "SET \"\\xff\\x00\" \"line\\nfeed\" KEEPTTL");
        let op = Op::ListDelete {
            key: b"l".to_vec(),
            index: 3,
            value: b"v".to_vec(),
        };
        assert_eq!(
            op.text(0, 0),
            format!("LSET l 3 {REMOVED}\nLREM l 1 {REMOVED}")
        );
        let op = Op::Replace {
            key: b"h".to_vec(),
            kind: "hash",
            old: Collection::Hash(vec![]),
            new: Collection::Hash(vec![(b"a".to_vec(), b"1".to_vec())]),
        };
        assert_eq!(op.text(0, 0), "DEL h\nHSET h a 1");
    }

    #[test]
    fn a_large_collection_is_written_in_steps() {
        let members: Vec<Bytes> = (0..1200).map(|n| n.to_string().into_bytes()).collect();
        let commands = write_collection(b"s", &Collection::Set(members));
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0].len(), 2 + 500);
        assert_eq!(commands[2].len(), 2 + 200);
        let pairs: Vec<(Bytes, Bytes)> = (0..501)
            .map(|n| (n.to_string().into_bytes(), b"v".to_vec()))
            .collect();
        let commands = write_collection(b"h", &Collection::Hash(pairs));
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].len(), 2 + 1000);
    }

    #[test]
    fn stream_ids_are_compared_as_numbers() {
        assert!(entry_id("10-0") > entry_id("9-99"));
        assert!(entry_id("5-2") > entry_id("5-1"));
        assert_eq!(entry_id("*"), None);
        assert_eq!(entry_id("5"), None);
    }

    #[test]
    fn rows_of_a_file_take_their_cells_by_column_name() {
        let columns = ["value".to_owned(), "key".to_owned()];
        let row = [text("v"), text("k")];
        let op = insert(Kind::Keys, &named(&columns, &row), None).unwrap();
        assert_eq!(lines(&op), "SET k v NX");
    }
}
