//! The commands of the console: what a statement asks for ([`plan`]) and
//! how it is run on the server.
//!
//! Everything is sent as a database command built here, so that a command
//! does exactly what its text says and nothing is retried or rewritten on
//! the way. What the console does not know is refused; see
//! `mongodb_method_unsupported` for the list.
use super::shell::{Statement, Step, database_name, whole};
use super::value::{self, cell, common_type, grid, type_name};
use super::{MongoSession, Scope, failure};
use crate::model::{Cell, QueryResult, ResultColumn};
use ::mongodb::bson::{Bson, Document, doc, oid::ObjectId};
use anyhow::{Result, anyhow, bail};

/// A method the console does not run.
pub(super) const METHOD: &str = "mongodb_method_unsupported";
/// A method with arguments it does not take.
pub(super) const ARGUMENTS: &str = "mongodb_arguments_invalid";
/// A command that removes or rewrites everything, without `confirm`.
pub(super) const CONFIRMATION: &str = "mongodb_confirmation_required";

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Op {
    ShowDatabases,
    ShowCollections,
    Use(String),
    Database {
        /// `getSiblingDB`; otherwise the database in use.
        database: Option<String>,
        action: DbAction,
    },
    Collection {
        database: Option<String>,
        collection: String,
        action: Box<Action>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum DbAction {
    Name,
    Version,
    CollectionNames,
    CollectionInfos,
    Command { command: Document, admin: bool },
    Drop,
    CreateCollection { name: String, options: Document },
    Stats,
    ServerStatus,
    CurrentOp,
    KillOp(Bson),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Find {
    pub filter: Document,
    pub projection: Option<Document>,
    pub sort: Option<Document>,
    pub skip: Option<i64>,
    pub limit: Option<i64>,
    pub hint: Option<Bson>,
    pub collation: Option<Document>,
    pub max_time_ms: Option<i64>,
    /// `findOne`: at most one document.
    pub one: bool,
    /// `.count()` after the filter: the number of documents, not the documents.
    pub count: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Action {
    Find(Box<Find>),
    Count {
        filter: Document,
        options: Document,
    },
    EstimatedCount,
    Aggregate {
        pipeline: Vec<Bson>,
        options: Document,
    },
    Distinct {
        field: String,
        filter: Document,
        options: Document,
    },
    Insert {
        documents: Vec<Document>,
        ordered: bool,
        many: bool,
    },
    Update {
        filter: Document,
        /// A document of operators, or a pipeline.
        update: Bson,
        options: Document,
        many: bool,
    },
    Replace {
        filter: Document,
        replacement: Document,
        options: Document,
    },
    Delete {
        filter: Document,
        options: Document,
        many: bool,
    },
    CreateIndex {
        keys: Document,
        options: Document,
    },
    Indexes,
    DropIndex(Bson),
    Drop,
    Stats,
}

type Planned<T> = Result<T, &'static str>;

fn document(value: Option<&Bson>) -> Planned<Document> {
    match value {
        Some(Bson::Document(document)) => Ok(document.clone()),
        _ => Err(ARGUMENTS),
    }
}
/// A filter: a document, or nothing at all for "every document".
fn filter(value: Option<&Bson>) -> Planned<Document> {
    match value {
        None | Some(Bson::Null | Bson::Undefined) => Ok(Document::new()),
        other => document(other),
    }
}
fn optional(value: Option<&Bson>) -> Planned<Option<Document>> {
    match value {
        None | Some(Bson::Null | Bson::Undefined) => Ok(None),
        other => document(other).map(Some),
    }
}
/// An options document that holds only what the method is known to take.
fn options(value: Option<&Bson>, allowed: &[&str]) -> Planned<Document> {
    let options = optional(value)?.unwrap_or_default();
    if options.keys().all(|key| allowed.contains(&key.as_str())) {
        Ok(options)
    } else {
        Err(ARGUMENTS)
    }
}
fn text(value: Option<&Bson>) -> Planned<String> {
    match value {
        Some(Bson::String(text)) if !text.is_empty() => Ok(text.clone()),
        _ => Err(ARGUMENTS),
    }
}
fn count(value: Option<&Bson>) -> Planned<i64> {
    value
        .and_then(whole)
        .filter(|number| *number >= 0)
        .ok_or(ARGUMENTS)
}
fn at_most(arguments: &[Bson], most: usize) -> Planned<()> {
    if arguments.len() <= most {
        Ok(())
    } else {
        Err(ARGUMENTS)
    }
}
fn exactly(arguments: &[Bson], wanted: std::ops::RangeInclusive<usize>) -> Planned<()> {
    if wanted.contains(&arguments.len()) {
        Ok(())
    } else {
        Err(ARGUMENTS)
    }
}
fn flag(options: &Document, key: &str) -> Planned<bool> {
    match options.get(key) {
        None => Ok(false),
        Some(Bson::Boolean(flag)) => Ok(*flag),
        Some(_) => Err(ARGUMENTS),
    }
}
/// Documents of an array argument.
fn documents(value: Option<&Bson>) -> Planned<Vec<Document>> {
    match value {
        Some(Bson::Array(items)) => items.iter().map(|item| document(Some(item))).collect(),
        _ => Err(ARGUMENTS),
    }
}
fn operators(document: &Document) -> bool {
    !document.is_empty() && document.keys().all(|key| key.starts_with('$'))
}

/// The calls that may follow a call: `(name, arguments)` pairs.
fn calls(steps: &[Step]) -> Planned<Vec<(&str, &[Bson])>> {
    steps
        .chunks(2)
        .map(|pair| match pair {
            [Step::Member(name), Step::Call(arguments)] => {
                Ok((name.as_str(), arguments.as_slice()))
            }
            // A property of a result (`.insertedId`) is JavaScript.
            _ => Err(METHOD),
        })
        .collect()
}

fn find(arguments: &[Bson], one: bool, rest: &[Step]) -> Planned<Find> {
    at_most(arguments, 2)?;
    let mut spec = Find {
        filter: filter(arguments.first())?,
        projection: optional(arguments.get(1))?,
        one,
        ..Find::default()
    };
    let modifiers = calls(rest)?;
    if one && !modifiers.is_empty() {
        return Err(METHOD);
    }
    for (index, (name, arguments)) in modifiers.iter().enumerate() {
        let argument = arguments.first();
        match *name {
            "toArray" | "pretty" => exactly(arguments, 0..=0)?,
            "count" => {
                // A number: nothing can follow it.
                if index + 1 != modifiers.len() {
                    return Err(METHOD);
                }
                exactly(arguments, 0..=0)?;
                spec.count = true;
            }
            "sort" | "projection" | "project" | "collation" | "skip" | "limit" | "maxTimeMS"
            | "hint" => {
                exactly(arguments, 1..=1)?;
                match *name {
                    "sort" => spec.sort = Some(document(argument)?),
                    "projection" | "project" => spec.projection = Some(document(argument)?),
                    "collation" => spec.collation = Some(document(argument)?),
                    "skip" => spec.skip = Some(count(argument)?),
                    "limit" => spec.limit = Some(count(argument)?),
                    "maxTimeMS" => spec.max_time_ms = Some(count(argument)?),
                    _ => match argument {
                        Some(Bson::String(_) | Bson::Document(_)) => spec.hint = argument.cloned(),
                        _ => return Err(ARGUMENTS),
                    },
                }
            }
            _ => return Err(METHOD),
        }
    }
    Ok(spec)
}

fn collection_action(name: &str, arguments: &[Bson], rest: &[Step]) -> Planned<Action> {
    let first = arguments.first();
    let second = arguments.get(1);
    let third = arguments.get(2);
    let action = match name {
        "find" => return find(arguments, false, rest).map(|spec| Action::Find(Box::new(spec))),
        "findOne" => return find(arguments, true, rest).map(|spec| Action::Find(Box::new(spec))),
        "aggregate" => {
            at_most(arguments, 2)?;
            // What may follow a pipeline changes nothing about its result.
            for (name, arguments) in calls(rest)? {
                if !matches!(name, "toArray" | "pretty") || !arguments.is_empty() {
                    return Err(METHOD);
                }
            }
            let pipeline = match first {
                None => Vec::new(),
                Some(Bson::Array(stages))
                    if stages
                        .iter()
                        .all(|stage| matches!(stage, Bson::Document(_))) =>
                {
                    stages.clone()
                }
                _ => return Err(ARGUMENTS),
            };
            return Ok(Action::Aggregate {
                pipeline,
                options: options(
                    second,
                    &["allowDiskUse", "collation", "hint", "maxTimeMS", "let"],
                )?,
            });
        }
        "countDocuments" | "count" => {
            at_most(arguments, 2)?;
            Action::Count {
                filter: filter(first)?,
                options: options(second, &["skip", "limit", "collation", "hint", "maxTimeMS"])?,
            }
        }
        "estimatedDocumentCount" => {
            exactly(arguments, 0..=0)?;
            Action::EstimatedCount
        }
        "distinct" => {
            exactly(arguments, 1..=3)?;
            Action::Distinct {
                field: text(first)?,
                filter: filter(second)?,
                options: options(third, &["collation", "maxTimeMS"])?,
            }
        }
        "insertOne" => {
            exactly(arguments, 1..=1)?;
            Action::Insert {
                documents: vec![document(first)?],
                ordered: true,
                many: false,
            }
        }
        "insertMany" => {
            exactly(arguments, 1..=2)?;
            let documents = documents(first)?;
            if documents.is_empty() {
                return Err(ARGUMENTS);
            }
            let options = options(second, &["ordered"])?;
            Action::Insert {
                documents,
                ordered: match options.get("ordered") {
                    None => true,
                    Some(Bson::Boolean(ordered)) => *ordered,
                    Some(_) => return Err(ARGUMENTS),
                },
                many: true,
            }
        }
        "updateOne" | "updateMany" => {
            exactly(arguments, 2..=3)?;
            let update = match second {
                Some(Bson::Document(update)) if operators(update) => second.cloned(),
                Some(Bson::Array(stages))
                    if !stages.is_empty()
                        && stages
                            .iter()
                            .all(|stage| matches!(stage, Bson::Document(_))) =>
                {
                    second.cloned()
                }
                // A plain document would replace the whole document.
                _ => None,
            };
            let options = options(third, &["upsert", "arrayFilters", "collation", "hint"])?;
            flag(&options, "upsert")?;
            Action::Update {
                filter: document(first)?,
                update: update.ok_or(ARGUMENTS)?,
                options,
                many: name == "updateMany",
            }
        }
        "replaceOne" => {
            exactly(arguments, 2..=3)?;
            let replacement = document(second)?;
            if replacement.keys().any(|key| key.starts_with('$')) {
                return Err(ARGUMENTS);
            }
            let options = options(third, &["upsert", "collation", "hint"])?;
            flag(&options, "upsert")?;
            Action::Replace {
                filter: document(first)?,
                replacement,
                options,
            }
        }
        "deleteOne" | "deleteMany" => {
            exactly(arguments, 1..=2)?;
            Action::Delete {
                filter: document(first)?,
                options: options(second, &["collation", "hint"])?,
                many: name == "deleteMany",
            }
        }
        "createIndex" => {
            exactly(arguments, 1..=2)?;
            let keys = document(first)?;
            if keys.is_empty() {
                return Err(ARGUMENTS);
            }
            Action::CreateIndex {
                keys,
                options: optional(second)?.unwrap_or_default(),
            }
        }
        "getIndexes" => {
            exactly(arguments, 0..=0)?;
            Action::Indexes
        }
        "dropIndex" => {
            exactly(arguments, 1..=1)?;
            match first {
                Some(Bson::String(_) | Bson::Document(_)) => {
                    Action::DropIndex(first.cloned().unwrap_or_default())
                }
                _ => return Err(ARGUMENTS),
            }
        }
        "drop" => {
            exactly(arguments, 0..=0)?;
            Action::Drop
        }
        "stats" => {
            exactly(arguments, 0..=0)?;
            Action::Stats
        }
        _ => return Err(METHOD),
    };
    // The result of these is not a cursor: nothing can be called on it.
    if rest.is_empty() {
        Ok(action)
    } else {
        Err(METHOD)
    }
}

fn database_action(name: &str, arguments: &[Bson]) -> Planned<DbAction> {
    let first = arguments.first();
    Ok(match name {
        "runCommand" | "adminCommand" => {
            exactly(arguments, 1..=1)?;
            let command = match first {
                // `db.runCommand("ping")`.
                Some(Bson::String(name)) if !name.is_empty() => {
                    let mut command = Document::new();
                    command.insert(name.clone(), 1);
                    command
                }
                other => document(other)?,
            };
            if command.is_empty() {
                return Err(ARGUMENTS);
            }
            DbAction::Command {
                command,
                admin: name == "adminCommand",
            }
        }
        "getCollectionNames" => {
            exactly(arguments, 0..=0)?;
            DbAction::CollectionNames
        }
        "getCollectionInfos" => {
            exactly(arguments, 0..=0)?;
            DbAction::CollectionInfos
        }
        "getName" => {
            exactly(arguments, 0..=0)?;
            DbAction::Name
        }
        "version" => {
            exactly(arguments, 0..=0)?;
            DbAction::Version
        }
        "dropDatabase" => {
            exactly(arguments, 0..=0)?;
            DbAction::Drop
        }
        "createCollection" => {
            exactly(arguments, 1..=2)?;
            DbAction::CreateCollection {
                name: text(first)?,
                options: optional(arguments.get(1))?.unwrap_or_default(),
            }
        }
        "stats" => {
            exactly(arguments, 0..=0)?;
            DbAction::Stats
        }
        "serverStatus" => {
            exactly(arguments, 0..=0)?;
            DbAction::ServerStatus
        }
        "currentOp" => {
            exactly(arguments, 0..=0)?;
            DbAction::CurrentOp
        }
        "killOp" => {
            exactly(arguments, 1..=1)?;
            match first {
                Some(Bson::Int32(_) | Bson::Int64(_) | Bson::String(_)) => {
                    DbAction::KillOp(first.cloned().unwrap_or_default())
                }
                _ => return Err(ARGUMENTS),
            }
        }
        _ => return Err(METHOD),
    })
}

/// What the chain after `db` asks for.
fn chain(steps: &[Step]) -> Planned<Op> {
    let mut database = None;
    let mut collection: Option<String> = None;
    let mut index = 0;
    loop {
        let Some(Step::Member(name)) = steps.get(index) else {
            // `db` alone, or `db(…)`.
            return Err(METHOD);
        };
        let Some(Step::Call(arguments)) = steps.get(index + 1) else {
            // A name that is not called is a collection; a further one is
            // the rest of a name with dots, as in `db.system.views`.
            collection = Some(match collection {
                Some(prefix) => format!("{prefix}.{name}"),
                None => name.clone(),
            });
            index += 1;
            continue;
        };
        index += 2;
        if let Some(collection) = collection {
            let action = collection_action(name, arguments, &steps[index..])?;
            return Ok(Op::Collection {
                database,
                collection,
                action: Box::new(action),
            });
        }
        match name.as_str() {
            "getSiblingDB" => {
                exactly(arguments, 1..=1)?;
                let name = text(arguments.first())?;
                if !database_name(&name) {
                    return Err(ARGUMENTS);
                }
                database = Some(name);
            }
            "getCollection" => {
                exactly(arguments, 1..=1)?;
                collection = Some(text(arguments.first())?);
            }
            other => {
                let action = database_action(other, arguments)?;
                if index < steps.len() {
                    return Err(METHOD);
                }
                return Ok(Op::Database { database, action });
            }
        }
    }
}

/// A raw command that removes or rewrites everything it is pointed at.
fn command_destructive(command: &Document) -> bool {
    let Some((name, _)) = command.iter().next() else {
        return false;
    };
    let any = |list: &str, test: &dyn Fn(&Document) -> bool| {
        command.get_array(list).is_ok_and(|items| {
            items
                .iter()
                .any(|item| item.as_document().is_some_and(test))
        })
    };
    let unfiltered = |statement: &Document| {
        !statement
            .get_document("q")
            .is_ok_and(|filter| !filter.is_empty())
    };
    match name.as_str() {
        "drop" | "dropDatabase" | "dropAllUsersFromDatabase" | "dropAllRolesFromDatabase" => true,
        "delete" => any("deletes", &|statement| {
            unfiltered(statement) && statement.get("limit").and_then(whole) != Some(1)
        }),
        "update" => any("updates", &|statement| {
            unfiltered(statement) && statement.get_bool("multi").unwrap_or(false)
        }),
        "dropIndexes" => command.get_str("index").is_ok_and(|index| index == "*"),
        "aggregate" => command
            .get_array("pipeline")
            .is_ok_and(|stages| replaces_a_collection(stages)),
        "renameCollection" => command.get_bool("dropTarget").unwrap_or(false),
        _ => false,
    }
}

/// `$out` replaces the whole collection it writes to.
fn replaces_a_collection(pipeline: &[Bson]) -> bool {
    pipeline.iter().any(|stage| {
        stage
            .as_document()
            .is_some_and(|stage| stage.contains_key("$out"))
    })
}

/// A command that removes or rewrites every document of a collection, a
/// collection or a database: it runs only with `confirm` in front of it.
pub(super) fn destructive(op: &Op) -> bool {
    match op {
        Op::Database { action, .. } => match action {
            DbAction::Drop => true,
            DbAction::Command { command, .. } => command_destructive(command),
            _ => false,
        },
        Op::Collection { action, .. } => match action.as_ref() {
            Action::Drop => true,
            Action::Delete {
                filter, many: true, ..
            }
            | Action::Update {
                filter, many: true, ..
            } => filter.is_empty(),
            Action::DropIndex(Bson::String(name)) => name == "*",
            Action::Aggregate { pipeline, .. } => replaces_a_collection(pipeline),
            _ => false,
        },
        _ => false,
    }
}

/// What a statement asks for. A command that needs `confirm` and does not
/// have it is refused here, before anything of a script runs.
pub(super) fn plan(statement: &Statement) -> Planned<Op> {
    match statement {
        Statement::ShowDatabases => Ok(Op::ShowDatabases),
        Statement::ShowCollections => Ok(Op::ShowCollections),
        Statement::Use(name) => Ok(Op::Use(name.clone())),
        Statement::Chain { steps, confirmed } => {
            let op = chain(steps)?;
            if destructive(&op) && !confirmed {
                return Err(CONFIRMATION);
            }
            Ok(op)
        }
    }
}

/// The name the shell gives an index without one: `status_1_total_-1`.
fn index_name(keys: &Document) -> String {
    keys.iter()
        .map(|(field, kind)| {
            let kind = match kind {
                Bson::String(kind) => kind.clone(),
                other => {
                    whole(other).map_or_else(|| value::shell(other), |number| number.to_string())
                }
            };
            format!("{field}_{kind}")
        })
        .collect::<Vec<_>>()
        .join("_")
}

fn columns_of(names: &[(&str, &Bson)]) -> Vec<ResultColumn> {
    names
        .iter()
        .map(|(name, value)| ResultColumn {
            name: (*name).into(),
            data_type: match type_name(value) {
                "null" => String::new(),
                kind => kind.into(),
            },
        })
        .collect()
}

/// One row of named values.
fn row(values: &[(&str, Bson)]) -> QueryResult {
    let named = values
        .iter()
        .map(|(name, value)| (*name, value))
        .collect::<Vec<_>>();
    QueryResult {
        columns: columns_of(&named),
        rows: vec![values.iter().map(|(_, value)| cell(Some(value))).collect()],
        ..QueryResult::default()
    }
}

/// Documents as rows: the union of their top-level fields.
pub(super) fn rows_of(documents: &[Document], truncated: bool) -> QueryResult {
    let (columns, rows) = grid(documents);
    QueryResult {
        columns,
        rows,
        truncated,
        ..QueryResult::default()
    }
}

/// Values that are not documents, one per row.
fn values_of(name: &str, values: &[Bson]) -> QueryResult {
    QueryResult {
        columns: vec![ResultColumn {
            name: name.into(),
            data_type: common_type(values),
        }],
        rows: values.iter().map(|value| vec![cell(Some(value))]).collect(),
        ..QueryResult::default()
    }
}

fn number(reply: &Document, key: &str) -> i64 {
    reply.get(key).and_then(whole).unwrap_or(0)
}

/// The first error a write command reports inside a reply that is `ok`.
pub(super) fn write_error(reply: &Document) -> Option<(i64, usize)> {
    if let Ok(errors) = reply.get_array("writeErrors")
        && let Some(Bson::Document(first)) = errors.first()
    {
        return Some((
            number(first, "code"),
            number(first, "index").max(0) as usize,
        ));
    }
    reply
        .get_document("writeConcernError")
        .ok()
        .map(|error| (number(error, "code"), usize::MAX))
}

/// Put `_id` in front, with a new ObjectId when the document has none: the
/// server would do the same, and this side then knows the key.
pub(super) fn with_id(document: Document) -> (Bson, Document) {
    let id = document
        .get("_id")
        .cloned()
        .unwrap_or_else(|| Bson::ObjectId(ObjectId::new()));
    let mut ordered = doc! { "_id": id.clone() };
    ordered.extend(document.into_iter().filter(|(key, _)| key != "_id"));
    (id, ordered)
}

/// Documents sent to the server in one `insert`.
pub(super) const INSERT_BATCH: usize = 1000;

/// Server limit of a read, in milliseconds: the time a statement may take.
pub(super) const MAX_TIME_MS: i64 = 120_000;

fn extend(command: &mut Document, options: &Document, keys: &[&str]) {
    for key in keys {
        if let Some(value) = options.get(*key) {
            command.insert(*key, value.clone());
        }
    }
}

impl MongoSession {
    /// Run one planned command and shape its answer for the grid.
    pub(super) async fn run(&self, scope: &mut Scope, op: &Op) -> Result<QueryResult> {
        match op {
            Op::Use(name) => {
                *self
                    .current
                    .lock()
                    .map_err(|_| anyhow!("database_operation_failed"))? = name.clone();
                Ok(row(&[(
                    "result",
                    Bson::String(format!("switched to db {name}")),
                )]))
            }
            Op::ShowDatabases => {
                let reply = scope
                    .command(&self.client.database("admin"), doc! {"listDatabases": 1})
                    .await
                    .map_err(failure)?;
                let databases = reply
                    .get_array("databases")
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Bson::as_document)
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                Ok(rows_of(&databases, false))
            }
            Op::ShowCollections => {
                let database = self.database_in_use();
                let collections = self.collections(scope, &database).await?;
                let mut rows = Vec::new();
                for (index, (name, kind)) in collections.iter().enumerate() {
                    // An estimate from the metadata of the collection; a
                    // view has none. Asked for a bounded number of them.
                    let estimate = if kind == "view" || index >= 500 {
                        Cell::Null
                    } else {
                        scope
                            .command(&self.client.database(&database), doc! {"count": name})
                            .await
                            .ok()
                            .map_or(Cell::Null, |reply| {
                                Cell::Integer(number(&reply, "n").to_string())
                            })
                    };
                    rows.push(vec![
                        Cell::Text(name.clone()),
                        Cell::Text(kind.clone()),
                        estimate,
                    ]);
                }
                Ok(QueryResult {
                    columns: [
                        ("name", "string"),
                        ("type", "string"),
                        ("documents", "int64"),
                    ]
                    .into_iter()
                    .map(|(name, kind)| ResultColumn {
                        name: name.into(),
                        data_type: kind.into(),
                    })
                    .collect(),
                    rows,
                    ..QueryResult::default()
                })
            }
            Op::Database { database, action } => {
                let name = database.clone().unwrap_or_else(|| self.database_in_use());
                self.run_database(scope, &name, action).await
            }
            Op::Collection {
                database,
                collection,
                action,
            } => {
                let name = database.clone().unwrap_or_else(|| self.database_in_use());
                self.run_collection(scope, &name, collection, action).await
            }
        }
    }

    async fn run_database(
        &self,
        scope: &mut Scope,
        name: &str,
        action: &DbAction,
    ) -> Result<QueryResult> {
        let database = self.client.database(name);
        let admin = self.client.database("admin");
        let reply = match action {
            DbAction::Name => return Ok(row(&[("name", Bson::String(name.into()))])),
            DbAction::Version => {
                return Ok(row(&[("version", Bson::String(self.version.clone()))]));
            }
            DbAction::CollectionNames => {
                let names = self
                    .collections(scope, name)
                    .await?
                    .into_iter()
                    .map(|(name, _)| Bson::String(name))
                    .collect::<Vec<_>>();
                return Ok(values_of("name", &names));
            }
            DbAction::CollectionInfos => {
                let batch = scope
                    .documents(&database, doc! {"listCollections": 1})
                    .await
                    .map_err(failure)?;
                return Ok(rows_of(&batch.documents, batch.truncated));
            }
            DbAction::CurrentOp => {
                let operations = self.operations(scope).await?;
                return Ok(rows_of(&operations, false));
            }
            DbAction::Command {
                command,
                admin: on_admin,
            } => {
                let target = if *on_admin { &admin } else { &database };
                scope.command(target, command.clone()).await
            }
            DbAction::Drop => scope.command(&database, doc! {"dropDatabase": 1}).await,
            DbAction::CreateCollection { name, options } => {
                let mut command = doc! {"create": name};
                command.extend(options.clone());
                scope.command(&database, command).await
            }
            DbAction::Stats => scope.command(&database, doc! {"dbStats": 1}).await,
            DbAction::ServerStatus => scope.command(&admin, doc! {"serverStatus": 1}).await,
            DbAction::KillOp(operation) => {
                scope
                    .command(&admin, doc! {"killOp": 1, "op": operation.clone()})
                    .await
            }
        }
        .map_err(failure)?;
        Ok(rows_of(&[reply], false))
    }

    async fn run_collection(
        &self,
        scope: &mut Scope,
        database_name: &str,
        collection: &str,
        action: &Action,
    ) -> Result<QueryResult> {
        let database = self.client.database(database_name);
        match action {
            Action::Find(find) if find.count => {
                let options = Document::new();
                self.count(scope, &database, collection, &find.filter, &options)
                    .await
            }
            Action::Find(find) => {
                let mut command = doc! {"find": collection, "filter": find.filter.clone()};
                if let Some(projection) = &find.projection {
                    command.insert("projection", projection.clone());
                }
                if let Some(sort) = &find.sort {
                    command.insert("sort", sort.clone());
                }
                if let Some(skip) = find.skip.filter(|skip| *skip > 0) {
                    command.insert("skip", skip);
                }
                match (find.one, find.limit) {
                    (true, _) => {
                        command.insert("limit", 1_i64);
                        command.insert("singleBatch", true);
                    }
                    // A limit of nothing is no limit, as in the shell.
                    (false, Some(limit)) if limit > 0 => {
                        command.insert("limit", limit);
                    }
                    _ => {}
                }
                if let Some(hint) = &find.hint {
                    command.insert("hint", hint.clone());
                }
                if let Some(collation) = &find.collation {
                    command.insert("collation", collation.clone());
                }
                command.insert(
                    "maxTimeMS",
                    find.max_time_ms
                        .filter(|limit| (1..MAX_TIME_MS).contains(limit))
                        .unwrap_or(MAX_TIME_MS),
                );
                let batch = scope.documents(&database, command).await.map_err(failure)?;
                Ok(rows_of(&batch.documents, batch.truncated))
            }
            Action::Count { filter, options } => {
                self.count(scope, &database, collection, filter, options)
                    .await
            }
            Action::EstimatedCount => {
                let reply = scope
                    .command(&database, doc! {"count": collection})
                    .await
                    .map_err(failure)?;
                Ok(row(&[("count", Bson::Int64(number(&reply, "n")))]))
            }
            Action::Aggregate { pipeline, options } => {
                let mut command = doc! {
                    "aggregate": collection, "pipeline": pipeline.clone(), "cursor": {},
                    "maxTimeMS": MAX_TIME_MS,
                };
                extend(
                    &mut command,
                    options,
                    &["allowDiskUse", "collation", "hint", "let", "maxTimeMS"],
                );
                let batch = scope.documents(&database, command).await.map_err(failure)?;
                Ok(rows_of(&batch.documents, batch.truncated))
            }
            Action::Distinct {
                field,
                filter,
                options,
            } => {
                let mut command = doc! {
                    "distinct": collection, "key": field, "query": filter.clone(),
                    "maxTimeMS": MAX_TIME_MS,
                };
                extend(&mut command, options, &["collation", "maxTimeMS"]);
                let reply = scope.command(&database, command).await.map_err(failure)?;
                let values = reply.get_array("values").cloned().unwrap_or_default();
                Ok(values_of("value", &values))
            }
            Action::Insert {
                documents,
                ordered,
                many,
            } => {
                let (ids, documents): (Vec<_>, Vec<_>) =
                    documents.iter().cloned().map(with_id).unzip();
                let mut written = 0;
                for batch in documents.chunks(INSERT_BATCH) {
                    let reply = scope
                        .command(
                            &database,
                            doc! {"insert": collection, "documents": batch.to_vec(), "ordered": *ordered},
                        )
                        .await
                        .map_err(failure)?;
                    let inserted = number(&reply, "n").max(0) as usize;
                    if let Some((code, _)) = write_error(&reply) {
                        return Err(super::write_failure(code, written + inserted));
                    }
                    written += inserted;
                }
                let mut result = if *many {
                    row(&[
                        ("acknowledged", Bson::Boolean(true)),
                        ("insertedCount", Bson::Int64(written as i64)),
                        ("insertedIds", Bson::Array(ids)),
                    ])
                } else {
                    row(&[
                        ("acknowledged", Bson::Boolean(true)),
                        ("insertedId", ids.into_iter().next().unwrap_or_default()),
                    ])
                };
                result.affected = written as u64;
                Ok(result)
            }
            Action::Update {
                filter,
                update,
                options,
                many,
            } => {
                let mut statement = doc! {"q": filter.clone(), "u": update.clone(), "multi": *many};
                extend(
                    &mut statement,
                    options,
                    &["upsert", "arrayFilters", "collation", "hint"],
                );
                self.update(scope, &database, collection, statement).await
            }
            Action::Replace {
                filter,
                replacement,
                options,
            } => {
                let mut statement =
                    doc! {"q": filter.clone(), "u": replacement.clone(), "multi": false};
                extend(&mut statement, options, &["upsert", "collation", "hint"]);
                self.update(scope, &database, collection, statement).await
            }
            Action::Delete {
                filter,
                options,
                many,
            } => {
                let mut statement = doc! {"q": filter.clone(), "limit": if *many { 0 } else { 1 }};
                extend(&mut statement, options, &["collation", "hint"]);
                let reply = scope
                    .command(
                        &database,
                        doc! {"delete": collection, "deletes": [statement]},
                    )
                    .await
                    .map_err(failure)?;
                if let Some((code, _)) = write_error(&reply) {
                    return Err(super::server_failure(code));
                }
                let deleted = number(&reply, "n");
                let mut result = row(&[
                    ("acknowledged", Bson::Boolean(true)),
                    ("deletedCount", Bson::Int64(deleted)),
                ]);
                result.affected = deleted.max(0) as u64;
                Ok(result)
            }
            Action::CreateIndex { keys, options } => {
                let name = match options.get("name") {
                    None => index_name(keys),
                    Some(Bson::String(name)) if !name.is_empty() => name.clone(),
                    Some(_) => bail!(ARGUMENTS),
                };
                let mut index = doc! {"key": keys.clone(), "name": &name};
                index.extend(options.clone().into_iter().filter(|(key, _)| key != "name"));
                scope
                    .command(
                        &database,
                        doc! {"createIndexes": collection, "indexes": [index]},
                    )
                    .await
                    .map_err(failure)?;
                Ok(row(&[("name", Bson::String(name))]))
            }
            Action::Indexes => {
                let batch = scope
                    .documents(&database, doc! {"listIndexes": collection})
                    .await
                    .map_err(failure)?;
                Ok(rows_of(&batch.documents, batch.truncated))
            }
            Action::DropIndex(index) => {
                let reply = scope
                    .command(
                        &database,
                        doc! {"dropIndexes": collection, "index": index.clone()},
                    )
                    .await
                    .map_err(failure)?;
                Ok(rows_of(&[reply], false))
            }
            Action::Drop => {
                let dropped = match scope.command(&database, doc! {"drop": collection}).await {
                    Ok(_) => true,
                    // No such collection: the shell answers `false`.
                    Err(error) if super::server_code(&error) == Some(26) => false,
                    Err(error) => return Err(failure(error)),
                };
                self.forget(database_name, collection);
                Ok(row(&[("dropped", Bson::Boolean(dropped))]))
            }
            Action::Stats => {
                let reply = scope
                    .command(&database, doc! {"collStats": collection})
                    .await
                    .map_err(failure)?;
                Ok(rows_of(&[reply], false))
            }
        }
    }

    /// `countDocuments`: the documents a filter matches, counted by the
    /// server one by one, as the drivers do it.
    async fn count(
        &self,
        scope: &mut Scope,
        database: &::mongodb::Database,
        collection: &str,
        filter: &Document,
        options: &Document,
    ) -> Result<QueryResult> {
        let mut pipeline = vec![doc! {"$match": filter.clone()}];
        for (key, stage) in [("skip", "$skip"), ("limit", "$limit")] {
            if let Some(value) = options.get(key) {
                let amount = count(Some(value)).map_err(|code| anyhow!(code))?;
                if amount > 0 {
                    let mut step = Document::new();
                    step.insert(stage, amount);
                    pipeline.push(step);
                }
            }
        }
        pipeline.push(doc! {"$group": {"_id": 1, "n": {"$sum": 1}}});
        let mut command = doc! {
            "aggregate": collection, "pipeline": pipeline, "cursor": {}, "maxTimeMS": MAX_TIME_MS,
        };
        extend(&mut command, options, &["collation", "hint", "maxTimeMS"]);
        let batch = scope.documents(database, command).await.map_err(failure)?;
        let found = batch
            .documents
            .first()
            .map_or(0, |group| number(group, "n"));
        Ok(row(&[("count", Bson::Int64(found))]))
    }

    async fn update(
        &self,
        scope: &mut Scope,
        database: &::mongodb::Database,
        collection: &str,
        statement: Document,
    ) -> Result<QueryResult> {
        let reply = scope
            .command(
                database,
                doc! {"update": collection, "updates": [statement]},
            )
            .await
            .map_err(failure)?;
        if let Some((code, _)) = write_error(&reply) {
            return Err(super::server_failure(code));
        }
        let upserted = reply
            .get_array("upserted")
            .ok()
            .and_then(|items| items.first())
            .and_then(Bson::as_document)
            .and_then(|first| first.get("_id"))
            .cloned();
        let created = i64::from(upserted.is_some());
        let modified = number(&reply, "nModified");
        let mut result = row(&[
            ("acknowledged", Bson::Boolean(true)),
            ("matchedCount", Bson::Int64(number(&reply, "n") - created)),
            ("modifiedCount", Bson::Int64(modified)),
            ("upsertedCount", Bson::Int64(created)),
            ("upsertedId", upserted.unwrap_or(Bson::Null)),
        ]);
        result.affected = (modified + created).max(0) as u64;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::mongodb::shell::parse_script;

    fn planned(text: &str) -> Planned<Op> {
        let statements = parse_script(text).unwrap_or_else(|error| panic!("{text}: {error:?}"));
        assert_eq!(statements.len(), 1, "{text}");
        plan(&statements[0].statement)
    }
    fn action(text: &str) -> Action {
        match planned(text) {
            Ok(Op::Collection {
                database: None,
                collection,
                action,
            }) => {
                assert_eq!(collection, "orders", "{text}");
                *action
            }
            other => panic!("{text}: {other:?}"),
        }
    }
    fn on_database(text: &str) -> DbAction {
        match planned(text) {
            Ok(Op::Database {
                database: None,
                action,
            }) => action,
            other => panic!("{text}: {other:?}"),
        }
    }

    #[test]
    fn reads_are_planned_with_their_modifiers() {
        let found = |spec: Find| Action::Find(Box::new(spec));
        assert_eq!(action("db.orders.find()"), found(Find::default()));
        assert_eq!(
            action(
                "db.orders.find({status: 'new'}, {total: 1}).sort({total: -1}).skip(20).limit(10).hint('status_1').collation({locale: 'pl'}).maxTimeMS(500).toArray()"
            ),
            found(Find {
                filter: doc! {"status": "new"},
                projection: Some(doc! {"total": 1}),
                sort: Some(doc! {"total": -1}),
                skip: Some(20),
                limit: Some(10),
                hint: Some(Bson::String("status_1".into())),
                collation: Some(doc! {"locale": "pl"}),
                max_time_ms: Some(500),
                one: false,
                count: false,
            })
        );
        assert_eq!(
            action("db.orders.findOne({_id: 5})"),
            found(Find {
                filter: doc! {"_id": 5},
                one: true,
                ..Find::default()
            })
        );
        assert_eq!(
            action("db.orders.find({a: 1}).count()"),
            found(Find {
                filter: doc! {"a": 1},
                count: true,
                ..Find::default()
            })
        );
        assert_eq!(
            action("db.orders.countDocuments({a: 1}, {limit: 5})"),
            Action::Count {
                filter: doc! {"a": 1},
                options: doc! {"limit": 5},
            }
        );
        assert_eq!(
            action("db.orders.countDocuments()"),
            Action::Count {
                filter: doc! {},
                options: doc! {},
            }
        );
        assert_eq!(
            action("db.orders.estimatedDocumentCount()"),
            Action::EstimatedCount
        );
        assert_eq!(
            action(
                "db.orders.aggregate([{$match: {a: 1}}, {$group: {_id: '$k', n: {$sum: 1}}}], {allowDiskUse: true})"
            ),
            Action::Aggregate {
                pipeline: vec![
                    Bson::Document(doc! {"$match": {"a": 1}}),
                    Bson::Document(doc! {"$group": {"_id": "$k", "n": {"$sum": 1}}}),
                ],
                options: doc! {"allowDiskUse": true},
            }
        );
        assert_eq!(
            action("db.orders.distinct('status', {total: {$gt: 5}})"),
            Action::Distinct {
                field: "status".into(),
                filter: doc! {"total": {"$gt": 5}},
                options: doc! {},
            }
        );
        assert_eq!(action("db.orders.getIndexes()"), Action::Indexes);
        assert_eq!(action("db.orders.stats()"), Action::Stats);
    }

    #[test]
    fn writes_are_planned_as_the_shell_takes_them() {
        assert_eq!(
            action("db.orders.insertOne({a: 1})"),
            Action::Insert {
                documents: vec![doc! {"a": 1}],
                ordered: true,
                many: false,
            }
        );
        assert_eq!(
            action("db.orders.insertMany([{a: 1}, {a: 2}], {ordered: false})"),
            Action::Insert {
                documents: vec![doc! {"a": 1}, doc! {"a": 2}],
                ordered: false,
                many: true,
            }
        );
        assert_eq!(
            action("db.orders.updateOne({_id: 1}, {$set: {a: 2}}, {upsert: true})"),
            Action::Update {
                filter: doc! {"_id": 1},
                update: Bson::Document(doc! {"$set": {"a": 2}}),
                options: doc! {"upsert": true},
                many: false,
            }
        );
        assert_eq!(
            action("db.orders.updateMany({a: 1}, [{$set: {b: '$a'}}])"),
            Action::Update {
                filter: doc! {"a": 1},
                update: Bson::Array(vec![Bson::Document(doc! {"$set": {"b": "$a"}})]),
                options: doc! {},
                many: true,
            }
        );
        assert_eq!(
            action("db.orders.replaceOne({_id: 1}, {a: 2})"),
            Action::Replace {
                filter: doc! {"_id": 1},
                replacement: doc! {"a": 2},
                options: doc! {},
            }
        );
        assert_eq!(
            action("db.orders.deleteOne({_id: 1})"),
            Action::Delete {
                filter: doc! {"_id": 1},
                options: doc! {},
                many: false,
            }
        );
        assert_eq!(
            action("db.orders.createIndex({status: 1, total: -1}, {unique: true})"),
            Action::CreateIndex {
                keys: doc! {"status": 1, "total": -1},
                options: doc! {"unique": true},
            }
        );
        assert_eq!(
            action("db.orders.dropIndex('status_1')"),
            Action::DropIndex(Bson::String("status_1".into()))
        );
        assert_eq!(
            index_name(&doc! {"status": 1, "total": -1}),
            "status_1_total_-1"
        );
        assert_eq!(
            index_name(&doc! {"body": "text", "at": 1.0}),
            "body_text_at_1"
        );
    }

    #[test]
    fn databases_and_collections_are_named_as_in_the_shell() {
        let named = |text: &str| match planned(text) {
            Ok(Op::Collection {
                database,
                collection,
                ..
            }) => (database, collection),
            other => panic!("{text}: {other:?}"),
        };
        assert_eq!(named("db.orders.find()"), (None, "orders".into()));
        assert_eq!(
            named("db.system.views.find()"),
            (None, "system.views".into())
        );
        assert_eq!(named("db['my coll'].find()"), (None, "my coll".into()));
        assert_eq!(
            named("db.getCollection('a.b').find()"),
            (None, "a.b".into())
        );
        assert_eq!(
            named("db.getSiblingDB('shop').getCollection('orders').find()"),
            (Some("shop".into()), "orders".into())
        );
        assert_eq!(
            named("db.getSiblingDB(\"shop\").orders.items.find()"),
            (Some("shop".into()), "orders.items".into())
        );
        // A collection may be named like a method of the database.
        assert_eq!(named("db.stats.find()"), (None, "stats".into()));
        assert_eq!(on_database("db.stats()"), DbAction::Stats);
        assert_eq!(
            on_database("db.getCollectionNames()"),
            DbAction::CollectionNames
        );
        assert_eq!(on_database("db.getName()"), DbAction::Name);
        assert_eq!(on_database("db.currentOp()"), DbAction::CurrentOp);
        assert_eq!(
            on_database("db.killOp(1234)"),
            DbAction::KillOp(Bson::Int32(1234))
        );
        assert_eq!(
            on_database("db.runCommand({ping: 1})"),
            DbAction::Command {
                command: doc! {"ping": 1},
                admin: false,
            }
        );
        assert_eq!(
            on_database("db.adminCommand('listDatabases')"),
            DbAction::Command {
                command: doc! {"listDatabases": 1},
                admin: true,
            }
        );
        assert_eq!(
            on_database("db.createCollection('log', {capped: true, size: 4096})"),
            DbAction::CreateCollection {
                name: "log".into(),
                options: doc! {"capped": true, "size": 4096},
            }
        );
        assert_eq!(
            planned("db.getSiblingDB('other').getCollectionNames()"),
            Ok(Op::Database {
                database: Some("other".into()),
                action: DbAction::CollectionNames,
            })
        );
        assert_eq!(planned("show dbs"), Ok(Op::ShowDatabases));
        assert_eq!(planned("use shop"), Ok(Op::Use("shop".into())));
    }

    #[test]
    fn what_the_console_does_not_run_is_refused() {
        for text in [
            "db",
            "db.orders",
            "db.orders.findAndModify({})",
            "db.orders.find().forEach()",
            "db.orders.find().explain()",
            "db.orders.find().count().toArray()",
            "db.orders.findOne().sort({a: 1})",
            "db.orders.insertOne({a: 1}).insertedId",
            "db.orders.insertOne({a: 1}).toArray()",
            "db.orders.mapReduce()",
            "db.orders.renameCollection('x')",
            "db.getUsers()",
            "db.getCollectionNames().length",
            "db.runCommand({ping: 1}).ok",
        ] {
            assert_eq!(planned(text), Err(METHOD), "{text}");
        }
        for text in [
            "db.orders.find(5)",
            "db.orders.find({}, {}, {})",
            "db.orders.find().limit(-1)",
            "db.orders.find().limit('ten')",
            "db.orders.find().sort()",
            "db.orders.find().toArray(1)",
            "db.orders.find().hint(5)",
            "db.orders.countDocuments({}, {foo: 1})",
            "db.orders.aggregate({$match: {}})",
            "db.orders.aggregate([1])",
            "db.orders.aggregate([], {explain: true})",
            "db.orders.distinct()",
            "db.orders.distinct({a: 1})",
            "db.orders.insertOne()",
            "db.orders.insertOne([{a: 1}])",
            "db.orders.insertMany({a: 1})",
            "db.orders.insertMany([])",
            "db.orders.insertMany([{a: 1}], {ordered: 1})",
            // Without operators the document would be replaced.
            "db.orders.updateOne({_id: 1}, {a: 2})",
            "db.orders.updateOne({_id: 1})",
            "db.orders.updateOne({_id: 1}, {$set: {a: 1}}, {upsert: 'yes'})",
            "db.orders.updateMany({}, [])",
            "db.orders.replaceOne({_id: 1}, {$set: {a: 2}})",
            "db.orders.deleteMany()",
            "db.orders.deleteOne('x')",
            "db.orders.createIndex({})",
            "db.orders.createIndex('status')",
            "db.orders.dropIndex()",
            "db.orders.drop({})",
            "db.runCommand()",
            "db.runCommand({})",
            "db.getSiblingDB('bad name').orders.find()",
            "db.getCollection('').find()",
            "db.killOp({})",
        ] {
            assert_eq!(planned(text), Err(ARGUMENTS), "{text}");
        }
    }

    /// Removing or rewriting everything needs the word `confirm`.
    #[test]
    fn commands_without_a_filter_need_a_confirmation() {
        for text in [
            "db.orders.deleteMany({})",
            "db.orders.updateMany({}, {$set: {a: 1}})",
            "db.orders.drop()",
            "db.dropDatabase()",
            "db.getSiblingDB('shop').dropDatabase()",
            "db.orders.dropIndex('*')",
            "db.orders.aggregate([{$match: {}}, {$out: 'orders'}])",
            "db.runCommand({drop: 'orders'})",
            "db.runCommand({dropDatabase: 1})",
            "db.runCommand({delete: 'orders', deletes: [{q: {}, limit: 0}]})",
            "db.runCommand({delete: 'orders', deletes: [{q: {a: 1}, limit: 0}, {q: {}, limit: 0}]})",
            "db.runCommand({update: 'orders', updates: [{q: {}, u: {$set: {a: 1}}, multi: true}]})",
            "db.runCommand({dropIndexes: 'orders', index: '*'})",
            "db.runCommand({aggregate: 'orders', pipeline: [{$out: 'x'}], cursor: {}})",
            "db.adminCommand({renameCollection: 'a.b', to: 'a.c', dropTarget: true})",
        ] {
            assert_eq!(planned(text), Err(CONFIRMATION), "{text}");
            let confirmed = planned(&format!("confirm {text}")).unwrap();
            assert!(destructive(&confirmed), "{text}");
        }
        // With a filter, or on one document, nothing is asked.
        for text in [
            "db.orders.deleteMany({status: 'old'})",
            "db.orders.deleteOne({})",
            "db.orders.updateMany({a: 1}, {$set: {a: 2}})",
            "db.orders.updateOne({}, {$set: {a: 2}})",
            "db.orders.dropIndex('status_1')",
            "db.orders.aggregate([{$merge: 'copy'}])",
            "db.runCommand({delete: 'orders', deletes: [{q: {}, limit: 1}]})",
            "db.runCommand({delete: 'orders', deletes: [{q: {a: 1}, limit: 0}]})",
            "db.runCommand({update: 'orders', updates: [{q: {}, u: {$set: {a: 1}}}]})",
            "db.runCommand({ping: 1})",
            // The word does no harm where it is not needed.
            "confirm db.orders.find()",
        ] {
            let op = planned(text).unwrap_or_else(|code| panic!("{text}: {code}"));
            assert!(!destructive(&op), "{text}");
        }
    }

    #[test]
    fn a_new_document_gets_its_key_in_front() {
        let (id, document) = with_id(doc! {"a": 1, "_id": 7, "b": 2});
        assert_eq!(id, Bson::Int32(7));
        assert_eq!(document, doc! {"_id": 7, "a": 1, "b": 2});
        assert_eq!(
            document.keys().map(String::as_str).collect::<Vec<_>>(),
            ["_id", "a", "b"]
        );
        let (id, document) = with_id(doc! {"a": 1});
        assert!(matches!(id, Bson::ObjectId(_)));
        assert_eq!(document.get("_id"), Some(&id));
        assert_eq!(document.keys().next().map(String::as_str), Some("_id"));
    }

    #[test]
    fn errors_inside_a_reply_are_found() {
        assert_eq!(write_error(&doc! {"ok": 1, "n": 1}), None);
        assert_eq!(
            write_error(
                &doc! {"ok": 1, "n": 2, "writeErrors": [{"index": 2, "code": 11000, "errmsg": "x"}]}
            ),
            Some((11000, 2))
        );
        assert_eq!(
            write_error(&doc! {"ok": 1, "writeConcernError": {"code": 64, "errmsg": "x"}}),
            Some((64, usize::MAX))
        );
    }

    #[test]
    fn results_are_shaped_for_the_grid() {
        let counted = row(&[("count", Bson::Int64(3))]);
        assert_eq!(counted.columns[0].name, "count");
        assert_eq!(counted.columns[0].data_type, "int64");
        assert_eq!(counted.rows, [[Cell::Integer("3".into())]]);
        let values = values_of(
            "value",
            &[Bson::String("a".into()), Bson::Int32(1), Bson::Null],
        );
        assert_eq!(values.columns[0].data_type, "mixed: string, int32");
        assert_eq!(
            values.rows,
            [
                [Cell::Text("a".into())],
                [Cell::Integer("1".into())],
                [Cell::Null]
            ]
        );
        let reply = rows_of(&[doc! {"ok": 1.0, "n": 2}], false);
        assert_eq!(
            reply
                .columns
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["ok", "n"]
        );
        assert_eq!(reply.rows[0][0], Cell::Float("1.0".into()));
        // No documents: no columns, and the grid says nothing was returned.
        assert!(rows_of(&[], false).columns.is_empty());
    }
}
