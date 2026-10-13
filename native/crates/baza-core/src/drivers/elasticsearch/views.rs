//! The overviews next to the indices: what the cluster says about its
//! indices, its nodes and the tasks it is running. They are read-only
//! containers; their few rows are filtered, ordered and paged on this side.
use super::browse::{Test, compare};
use crate::drivers::PageRequest;
use crate::model::{Cell, ColumnInfo, QueryResult, ResultColumn};
use anyhow::{Result, anyhow};
use serde_json::Value;

/// Names no index can have: an index name holds no space.
pub(super) const INDICES: &str = "index overview";
pub(super) const NODES: &str = "cluster nodes";
pub(super) const TASKS: &str = "running tasks";
pub(super) const ALL: [&str; 3] = [INDICES, NODES, TASKS];

pub(super) fn is_view(name: &str) -> bool {
    ALL.contains(&name)
}

const INDEX_COLUMNS: &[(&str, &str)] = &[
    ("index", "text"),
    ("health", "text"),
    ("status", "text"),
    ("documents", "number"),
    ("deleted documents", "number"),
    ("store size", "text"),
    ("size in bytes", "number"),
    ("primary shards", "number"),
    ("replicas", "number"),
    ("created", "text"),
];
const NODE_COLUMNS: &[(&str, &str)] = &[
    ("node", "text"),
    ("address", "text"),
    ("version", "text"),
    ("roles", "text"),
    ("elected master", "boolean"),
    ("heap %", "number"),
    ("memory %", "number"),
    ("cpu %", "number"),
    ("load 1m", "number"),
    ("disk used %", "number"),
    ("uptime", "text"),
];
const TASK_COLUMNS: &[(&str, &str)] = &[
    ("task", "text"),
    ("action", "text"),
    ("description", "text"),
    ("running (s)", "number"),
    ("started", "text"),
    ("cancellable", "boolean"),
    ("parent", "text"),
    ("node", "text"),
    ("opaque id", "text"),
];

pub(super) fn columns(name: &str) -> &'static [(&'static str, &'static str)] {
    match name {
        INDICES => INDEX_COLUMNS,
        NODES => NODE_COLUMNS,
        _ => TASK_COLUMNS,
    }
}

pub(super) fn column_info(name: &str) -> Vec<ColumnInfo> {
    columns(name)
        .iter()
        .map(|(column, kind)| ColumnInfo {
            name: (*column).into(),
            data_type: (*kind).into(),
            nullable: true,
            primary_key: false,
            default: None,
        })
        .collect()
}

/// The request an overview is read with.
pub(super) fn path(name: &str, hidden: bool) -> String {
    match name {
        INDICES => format!(
            "_cat/indices?format=json&bytes=b&s=index&expand_wildcards={}&h=index,health,status,docs.count,docs.deleted,store.size,pri,rep,creation.date.string",
            if hidden { "all" } else { "open,closed" }
        ),
        // Elasticsearch names the elected node `master`, OpenSearch
        // `cluster_manager`; a column a server does not know is left out.
        NODES => "_cat/nodes?format=json&h=name,ip,version,node.role,master,cluster_manager,heap.percent,ram.percent,cpu,load_1m,disk.used_percent,uptime".into(),
        _ => "_tasks?detailed=true&group_by=none".into(),
    }
}

/// A size in the largest unit that keeps it readable.
pub(super) fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut amount = bytes as f64;
    let mut unit = 0;
    while amount >= 1000.0 && unit + 1 < UNITS.len() {
        amount /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{amount:.1} {}", UNITS[unit])
    }
}

/// A figure of a `_cat` table, which writes every value as text.
fn figure(value: &Value) -> Cell {
    let text = match value {
        Value::String(text) => text.trim().to_owned(),
        Value::Number(number) => number.to_string(),
        _ => return Cell::Null,
    };
    if text.is_empty() {
        Cell::Null
    } else if text.parse::<i64>().is_ok() {
        Cell::Integer(text)
    } else if text.parse::<f64>().is_ok() {
        Cell::Decimal(text)
    } else {
        Cell::Text(text)
    }
}

fn text(value: &Value) -> Cell {
    match value.as_str() {
        Some(text) if !text.is_empty() => Cell::Text(text.to_owned()),
        _ => Cell::Null,
    }
}

/// The rows of an overview, from what the server answered.
pub(super) fn rows(name: &str, answer: &Value, hidden: bool) -> Vec<Vec<Cell>> {
    match name {
        INDICES => answer
            .as_array()
            .into_iter()
            .flatten()
            .filter(|index| {
                hidden
                    || !index["index"]
                        .as_str()
                        .is_some_and(|name| name.starts_with('.'))
            })
            .map(|index| {
                let bytes = index["store.size"]
                    .as_str()
                    .and_then(|bytes| bytes.parse::<u64>().ok());
                vec![
                    text(&index["index"]),
                    text(&index["health"]),
                    text(&index["status"]),
                    figure(&index["docs.count"]),
                    figure(&index["docs.deleted"]),
                    bytes.map_or(Cell::Null, |bytes| Cell::Text(size(bytes))),
                    bytes.map_or(Cell::Null, |bytes| Cell::Integer(bytes.to_string())),
                    figure(&index["pri"]),
                    figure(&index["rep"]),
                    text(&index["creation.date.string"]),
                ]
            })
            .collect(),
        NODES => answer
            .as_array()
            .into_iter()
            .flatten()
            .map(|node| {
                let elected = [&node["master"], &node["cluster_manager"]]
                    .into_iter()
                    .any(|mark| mark.as_str() == Some("*"));
                vec![
                    text(&node["name"]),
                    text(&node["ip"]),
                    text(&node["version"]),
                    text(&node["node.role"]),
                    Cell::Bool(elected),
                    figure(&node["heap.percent"]),
                    figure(&node["ram.percent"]),
                    figure(&node["cpu"]),
                    figure(&node["load_1m"]),
                    figure(&node["disk.used_percent"]),
                    text(&node["uptime"]),
                ]
            })
            .collect(),
        _ => tasks(answer)
            .into_iter()
            // The request that lists the tasks is not something to watch.
            .filter(|task| !task.action.starts_with("cluster:monitor/tasks/lists"))
            .map(|task| {
                vec![
                    Cell::Text(task.id),
                    Cell::Text(task.action),
                    task.description.map_or(Cell::Null, Cell::Text),
                    Cell::Decimal(format!("{:.1}", task.running_nanos as f64 / 1e9)),
                    chrono::DateTime::from_timestamp_millis(task.started_millis)
                        .map_or(Cell::Null, |moment| {
                            Cell::Text(moment.format("%Y-%m-%d %H:%M:%SZ").to_string())
                        }),
                    Cell::Bool(task.cancellable),
                    task.parent.map_or(Cell::Null, Cell::Text),
                    Cell::Text(task.node),
                    task.opaque.map_or(Cell::Null, Cell::Text),
                ]
            })
            .collect(),
    }
}

/// One task the cluster is running.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Task {
    /// `node:number`, the name the task API takes.
    pub id: String,
    pub node: String,
    pub action: String,
    pub description: Option<String>,
    pub running_nanos: u64,
    pub started_millis: i64,
    pub cancellable: bool,
    pub parent: Option<String>,
    /// The `X-Opaque-Id` of the request the task belongs to.
    pub opaque: Option<String>,
}

/// The tasks of an answer of `_tasks`, grouped by node or not at all.
pub(super) fn tasks(answer: &Value) -> Vec<Task> {
    fn read(task: &Value) -> Option<Task> {
        let node = task["node"].as_str()?.to_owned();
        Some(Task {
            id: format!("{node}:{}", task["id"].as_u64()?),
            node,
            action: task["action"].as_str().unwrap_or_default().to_owned(),
            description: task["description"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(str::to_owned),
            running_nanos: task["running_time_in_nanos"].as_u64().unwrap_or(0),
            started_millis: task["start_time_in_millis"].as_i64().unwrap_or(0),
            cancellable: task["cancellable"].as_bool().unwrap_or(false),
            parent: task["parent_task_id"].as_str().map(str::to_owned),
            opaque: task["headers"]["X-Opaque-Id"].as_str().map(str::to_owned),
        })
    }
    let mut found: Vec<Task> = match &answer["tasks"] {
        Value::Array(list) => list.iter().filter_map(read).collect(),
        Value::Object(named) => named.values().filter_map(read).collect(),
        _ => answer["nodes"]
            .as_object()
            .into_iter()
            .flat_map(|nodes| nodes.values())
            .flat_map(|node| {
                node["tasks"]
                    .as_object()
                    .into_iter()
                    .flat_map(|tasks| tasks.values())
            })
            .filter_map(read)
            .collect(),
    };
    found.sort_by_key(|task| std::cmp::Reverse(task.running_nanos));
    found
}

/// A page of an overview: its rows filtered, ordered and cut on this side.
pub(super) fn page(
    name: &str,
    mut rows: Vec<Vec<Cell>>,
    request: &PageRequest,
) -> Result<QueryResult> {
    let known = columns(name);
    let at = |column: &str| {
        known
            .iter()
            .position(|(known, _)| *known == column)
            .ok_or_else(|| anyhow!("column_not_found"))
    };
    for filter in &request.filters {
        let (column, test) = (at(&filter.column)?, Test::new(filter));
        rows.retain(|row| test.passes(&row[column]));
    }
    if let Some((column, descending)) = &request.order_by {
        let column = at(column)?;
        rows.sort_by(|left, right| {
            // Whatever the direction, a row without the value comes last.
            match (&left[column], &right[column]) {
                (Cell::Null, _) | (_, Cell::Null) => compare(&left[column], &right[column]),
                _ if *descending => compare(&right[column], &left[column]),
                _ => compare(&left[column], &right[column]),
            }
        });
    }
    Ok(QueryResult {
        columns: known
            .iter()
            .map(|(column, kind)| ResultColumn {
                name: (*column).into(),
                data_type: (*kind).into(),
            })
            .collect(),
        rows: rows
            .into_iter()
            .skip(usize::try_from(request.offset).unwrap_or(usize::MAX))
            .take(request.limit as usize)
            .collect(),
        ..QueryResult::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{Filter, FilterOp};
    use crate::model::TableRef;
    use serde_json::json;

    fn request(name: &str) -> PageRequest {
        PageRequest::first(TableRef {
            schema: String::new(),
            name: name.into(),
            kind: "view".into(),
        })
    }

    #[test]
    fn the_indices_are_listed_with_their_documents_and_their_size() {
        let answer = json!([
            { "index": "bazaqa-orders", "health": "green", "status": "open", "docs.count": "1200",
              "docs.deleted": "3", "store.size": "1536000", "pri": "3", "rep": "1",
              "creation.date.string": "2026-10-12T08:00:00.000Z" },
            { "index": ".kibana_1", "health": "green", "status": "open", "docs.count": "7",
              "docs.deleted": "0", "store.size": "208", "pri": "1", "rep": "0" },
            // A closed index has no figures.
            { "index": "bazaqa-old", "health": null, "status": "close", "docs.count": null,
              "store.size": null, "pri": "1", "rep": "1" },
        ]);
        let listed = rows(INDICES, &answer, false);
        assert_eq!(listed.len(), 2, "an index of the system is left out");
        assert_eq!(
            listed[0],
            [
                Cell::Text("bazaqa-orders".into()),
                Cell::Text("green".into()),
                Cell::Text("open".into()),
                Cell::Integer("1200".into()),
                Cell::Integer("3".into()),
                Cell::Text("1.5 MB".into()),
                Cell::Integer("1536000".into()),
                Cell::Integer("3".into()),
                Cell::Integer("1".into()),
                Cell::Text("2026-10-12T08:00:00.000Z".into()),
            ]
        );
        assert_eq!(listed[1][3], Cell::Null);
        assert_eq!(listed[1][5], Cell::Null);
        assert_eq!(rows(INDICES, &answer, true).len(), 3);
        assert_eq!(listed[0].len(), columns(INDICES).len());
        assert!(path(INDICES, false).contains("expand_wildcards=open,closed&"));
        assert!(path(INDICES, true).contains("expand_wildcards=all&"));
        // Ordered by size, the largest first, the one without a size last.
        let mut by_size = request(INDICES);
        by_size.order_by = Some(("size in bytes".into(), true));
        let page_of = page(INDICES, rows(INDICES, &answer, true), &by_size).unwrap();
        assert_eq!(
            page_of
                .rows
                .iter()
                .map(|row| row[0].display())
                .collect::<Vec<_>>(),
            ["bazaqa-orders", ".kibana_1", "bazaqa-old"]
        );
        let mut filtered = request(INDICES);
        filtered.filters = vec![Filter {
            column: "status".into(),
            op: FilterOp::Contains,
            value: Cell::Text("CLOSE".into()),
        }];
        assert_eq!(
            page(INDICES, rows(INDICES, &answer, true), &filtered)
                .unwrap()
                .rows
                .len(),
            1
        );
        filtered.filters[0].column = "nothing".into();
        assert_eq!(
            page(INDICES, Vec::new(), &filtered)
                .unwrap_err()
                .to_string(),
            "column_not_found"
        );
        let mut second = request(INDICES);
        (second.limit, second.offset) = (1, 1);
        assert_eq!(
            page(INDICES, rows(INDICES, &answer, true), &second)
                .unwrap()
                .rows[0][0],
            Cell::Text(".kibana_1".into())
        );
    }

    #[test]
    fn the_nodes_of_both_products_are_read() {
        let elastic = json!([{ "name": "es01", "ip": "172.17.0.2", "version": "9.5.5",
            "node.role": "cdfhilmrstw", "master": "*", "heap.percent": "35", "ram.percent": "95",
            "cpu": "3", "load_1m": "0.23", "disk.used_percent": "49.26", "uptime": "6.5m" }]);
        let opensearch = json!([
            { "name": "os01", "ip": "172.18.0.7", "version": "2.19.6", "node.role": "dimr",
              "master": "*", "cluster_manager": "*", "heap.percent": "47", "cpu": "4", "load_1m": "0.23" },
            { "name": "os02", "ip": "172.18.0.8", "version": "2.19.6", "node.role": "d",
              "cluster_manager": "-", "heap.percent": "12" },
        ]);
        let node = &rows(NODES, &elastic, false)[0];
        assert_eq!(node.len(), columns(NODES).len());
        assert_eq!(node[0], Cell::Text("es01".into()));
        assert_eq!(node[4], Cell::Bool(true));
        assert_eq!(node[5], Cell::Integer("35".into()));
        assert_eq!(node[8], Cell::Decimal("0.23".into()));
        let nodes = rows(NODES, &opensearch, false);
        assert_eq!(
            (nodes[0][4].clone(), nodes[1][4].clone()),
            (Cell::Bool(true), Cell::Bool(false))
        );
        assert_eq!(nodes[1][6], Cell::Null, "a figure the server left out");
        assert!(path(NODES, false).contains("master,cluster_manager"));
    }

    #[test]
    fn the_tasks_are_read_grouped_or_not() {
        let flat = json!({ "tasks": [
            { "node": "n1", "id": 12, "type": "transport", "action": "indices:data/read/search",
              "description": "indices[bazaqa-orders], search_type[QUERY_THEN_FETCH]",
              "start_time_in_millis": 1760000000000u64, "running_time_in_nanos": 2500000000u64,
              "cancellable": true, "headers": { "X-Opaque-Id": "baza-1" } },
            { "node": "n1", "id": 13, "action": "indices:data/read/search[phase/query]",
              "start_time_in_millis": 1760000000100u64, "running_time_in_nanos": 2400000000u64,
              "cancellable": true, "parent_task_id": "n1:12", "headers": { "X-Opaque-Id": "baza-1" } },
            { "node": "n1", "id": 14, "action": "cluster:monitor/tasks/lists",
              "running_time_in_nanos": 100, "cancellable": false, "headers": {} },
        ] });
        let found = tasks(&flat);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].id, "n1:12");
        assert_eq!(found[0].opaque.as_deref(), Some("baza-1"));
        assert_eq!(found[1].parent.as_deref(), Some("n1:12"));
        let grouped = json!({ "nodes": { "n1": { "name": "es01", "tasks": {
            "n1:12": { "node": "n1", "id": 12, "action": "indices:data/write/update/byquery",
                "running_time_in_nanos": 9, "cancellable": true, "headers": {} },
        } } } });
        assert_eq!(
            tasks(&grouped)[0].action,
            "indices:data/write/update/byquery"
        );
        assert!(tasks(&json!({})).is_empty());
        let listed = rows(TASKS, &flat, false);
        assert_eq!(listed.len(), 2, "the listing itself is left out");
        assert_eq!(listed[0].len(), columns(TASKS).len());
        assert_eq!(listed[0][0], Cell::Text("n1:12".into()));
        assert_eq!(listed[0][3], Cell::Decimal("2.5".into()));
        assert_eq!(listed[0][4], Cell::Text("2025-10-09 08:53:20Z".into()));
        assert_eq!(listed[0][5], Cell::Bool(true));
        assert_eq!(listed[1][6], Cell::Text("n1:12".into()));
        assert_eq!(path(TASKS, false), "_tasks?detailed=true&group_by=none");
    }

    #[test]
    fn a_size_is_written_in_a_readable_unit() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(999), "999 B");
        assert_eq!(size(1536), "1.5 kB");
        assert_eq!(size(2_500_000_000), "2.5 GB");
        assert!(is_view(INDICES) && is_view(NODES) && is_view(TASKS));
        assert!(!is_view("bazaqa-orders"));
        assert!(column_info(NODES).iter().all(|column| !column.primary_key));
    }
}
