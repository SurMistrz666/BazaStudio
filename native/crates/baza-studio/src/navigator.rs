//! Database navigation is derived from real metadata, never fabricated folders.
//! These identities remain distinct across connections, schemas and object kinds.
use baza_core::{ConnectionProfile, TableRef, groups::Snapshot};
use std::collections::{BTreeMap, HashSet};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    Other,
}
impl ObjectKind {
    pub fn from_metadata(kind: &str) -> Self {
        match kind.trim().to_ascii_lowercase().replace('_', " ").as_str() {
            "table" | "base table" | "system versioned" => Self::Table,
            "view" => Self::View,
            "materialized view" => Self::MaterializedView,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum NodeIdentity {
    Group(Uuid),
    Connection(Uuid),
    Schema(Uuid, String),
    Objects(Uuid, String, ObjectKind),
    Queries(Uuid),
    Object(Uuid, String, String, ObjectKind),
    Query(Option<Uuid>, Uuid),
}
impl NodeIdentity {
    pub fn connection(&self) -> Option<Uuid> {
        match self {
            Self::Group(_) => None,
            Self::Connection(id)
            | Self::Schema(id, _)
            | Self::Objects(id, _, _)
            | Self::Queries(id)
            | Self::Object(id, _, _, _) => Some(*id),
            Self::Query(connection, _) => *connection,
        }
    }
}

#[derive(Clone, Debug)]
pub struct QueryNode {
    pub id: Uuid,
    pub name: String,
    pub connection: Option<Uuid>,
}

pub struct ObjectFolder {
    pub kind: ObjectKind,
    pub objects: Vec<TableRef>,
}
pub struct SchemaNode {
    pub name: String,
    pub folders: Vec<ObjectFolder>,
}
pub struct ConnectionTree {
    pub visible: bool,
    pub schemas: Vec<SchemaNode>,
    pub queries: Vec<QueryNode>,
}

pub fn matches(text: &str, search: &str) -> bool {
    text.to_lowercase().contains(search)
}

pub fn connection_matches(profile: &ConnectionProfile, search: &str) -> bool {
    search.is_empty()
        || matches(&profile.name, search)
        || matches(profile.product_label(), search)
        || matches(profile.engine.label(), search)
        || matches(&profile.host, search)
        || matches(&profile.database, search)
        || profile
            .sqlite_path
            .as_ref()
            .and_then(|path| path.file_name())
            .is_some_and(|name| matches(&name.to_string_lossy(), search))
}

impl ConnectionTree {
    pub fn build(
        connection: Uuid,
        metadata: &[TableRef],
        queries: &[QueryNode],
        connection_match: bool,
        search: &str,
    ) -> Self {
        let show_all = connection_match || search.is_empty();
        let mut schemas: BTreeMap<String, BTreeMap<ObjectKind, Vec<TableRef>>> = BTreeMap::new();
        let mut seen = HashSet::new();
        for object in metadata {
            let kind = ObjectKind::from_metadata(&object.kind);
            let identity = (object.schema.clone(), object.name.clone(), kind);
            if !seen.insert(identity)
                || (!show_all && !matches(&object.schema, search) && !matches(&object.name, search))
            {
                continue;
            }
            schemas
                .entry(object.schema.clone())
                .or_default()
                .entry(kind)
                .or_default()
                .push(object.clone());
        }
        let schemas = schemas
            .into_iter()
            .map(|(name, mut folders)| {
                // Both supported object kinds are useful even when this real
                // schema contains no objects of one kind; zero is a real count.
                if show_all || matches(&name, search) {
                    folders.entry(ObjectKind::Table).or_default();
                    folders.entry(ObjectKind::View).or_default();
                }
                SchemaNode {
                    name,
                    folders: folders
                        .into_iter()
                        .map(|(kind, mut objects)| {
                            objects.sort_by(|a, b| a.name.cmp(&b.name));
                            ObjectFolder { kind, objects }
                        })
                        .collect(),
                }
            })
            .collect::<Vec<_>>();
        let mut queries = queries
            .iter()
            .filter(|query| {
                query.connection == Some(connection) && (show_all || matches(&query.name, search))
            })
            .cloned()
            .collect::<Vec<_>>();
        queries.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        Self {
            visible: show_all || !schemas.is_empty() || !queries.is_empty(),
            schemas,
            queries,
        }
    }
}

/// Searching a descendant keeps every real ancestor discoverable. The render
/// layer passes an empty search to descendants of a directly matching group.
pub fn group_visible(
    snapshot: &Snapshot,
    group_id: Uuid,
    matching_connections: &HashSet<Uuid>,
    search: &str,
    depth: usize,
) -> bool {
    if depth >= 8 {
        return false;
    }
    let Some(group) = snapshot.groups.iter().find(|group| group.id == group_id) else {
        return false;
    };
    search.is_empty()
        || matches(&group.name, search)
        || matching_connections
            .iter()
            .any(|id| snapshot.memberships.get(id) == Some(&group_id))
        || snapshot
            .groups
            .iter()
            .filter(|child| child.parent == Some(group_id))
            .any(|child| group_visible(snapshot, child.id, matching_connections, search, depth + 1))
}

pub fn descendant_connection_count(snapshot: &Snapshot, id: Uuid, alive: &HashSet<Uuid>) -> usize {
    let mut count = 0;
    for connection in alive {
        let mut group = snapshot.memberships.get(connection).copied();
        let mut visited = HashSet::new();
        for _ in 0..8 {
            let Some(current) = group else { break };
            if !visited.insert(current) {
                break;
            }
            if current == id {
                count += 1;
                break;
            }
            group = snapshot
                .groups
                .iter()
                .find(|group| group.id == current)
                .and_then(|group| group.parent);
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use baza_core::groups::Group;
    use std::collections::HashMap;
    fn table(schema: &str, name: &str, kind: &str) -> TableRef {
        TableRef {
            schema: schema.into(),
            name: name.into(),
            kind: kind.into(),
        }
    }
    fn group(id: Uuid, name: &str, parent: Option<Uuid>) -> Group {
        Group {
            id,
            name: name.into(),
            color: [35, 95, 180],
            parent,
        }
    }
    #[test]
    fn unsorted_metadata_groups_unique_schemas_objects_and_real_kinds() {
        let id = Uuid::new_v4();
        let metadata = vec![
            table("z", "b", "VIEW"),
            table("a", "b", "base table"),
            table("z", "a", "table"),
            table("a", "b", "BASE TABLE"),
            table("z", "cache", "materialized_view"),
        ];
        let tree = ConnectionTree::build(id, &metadata, &[], true, "");
        assert_eq!(
            tree.schemas
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "z"]
        );
        assert_eq!(tree.schemas[0].folders[0].objects.len(), 1);
        assert_eq!(
            tree.schemas[1]
                .folders
                .iter()
                .map(|f| f.kind)
                .collect::<Vec<_>>(),
            vec![
                ObjectKind::Table,
                ObjectKind::View,
                ObjectKind::MaterializedView
            ]
        );
    }
    #[test]
    fn schema_identity_preserves_case_and_qualified_object_identity() {
        let id = Uuid::new_v4();
        let tree = ConnectionTree::build(
            id,
            &[
                table("Sales", "orders", "table"),
                table("sales", "orders", "table"),
            ],
            &[],
            true,
            "",
        );
        assert_eq!(tree.schemas.len(), 2);
        assert_ne!(
            NodeIdentity::Object(id, "Sales".into(), "orders".into(), ObjectKind::Table),
            NodeIdentity::Object(id, "sales".into(), "orders".into(), ObjectKind::Table)
        );
    }
    #[test]
    fn table_search_keeps_only_matching_objects_and_their_schema() {
        let id = Uuid::new_v4();
        let tree = ConnectionTree::build(
            id,
            &[
                table("main", "customers", "table"),
                table("main", "orders", "view"),
                table("audit", "events", "table"),
            ],
            &[],
            false,
            "order",
        );
        assert!(tree.visible);
        assert_eq!(tree.schemas.len(), 1);
        assert_eq!(tree.schemas[0].folders[0].kind, ObjectKind::View);
        assert_eq!(tree.schemas[0].folders[0].objects[0].name, "orders");
    }
    #[test]
    fn schema_or_connection_search_reveals_all_actual_descendants() {
        let id = Uuid::new_v4();
        let metadata = [table("Sales", "a", "table"), table("Sales", "b", "view")];
        for (direct, search) in [(false, "sales"), (true, "endpoint")] {
            let tree = ConnectionTree::build(id, &metadata, &[], direct, search);
            assert_eq!(tree.schemas[0].folders.len(), 2);
        }
    }
    #[test]
    fn saved_query_search_reveals_disconnected_connection_without_cross_connection_leak() {
        let id = Uuid::new_v4();
        let other = Uuid::new_v4();
        let queries = [
            QueryNode {
                id: Uuid::new_v4(),
                name: "Monthly report".into(),
                connection: Some(id),
            },
            QueryNode {
                id: Uuid::new_v4(),
                name: "Monthly report".into(),
                connection: Some(other),
            },
            QueryNode {
                id: Uuid::new_v4(),
                name: "Monthly report".into(),
                connection: None,
            },
        ];
        let tree = ConnectionTree::build(id, &[], &queries, false, "monthly");
        assert!(tree.visible);
        assert!(tree.schemas.is_empty());
        assert_eq!(tree.queries.len(), 1);
        assert_eq!(tree.queries[0].connection, Some(id));
        assert!(!ConnectionTree::build(id, &[], &queries, false, "absent").visible);
    }
    #[test]
    fn empty_metadata_does_not_fabricate_schemas_or_function_folders() {
        let tree = ConnectionTree::build(Uuid::new_v4(), &[], &[], true, "");
        assert!(tree.visible);
        assert!(tree.schemas.is_empty());
        assert!(tree.queries.is_empty());
    }
    #[test]
    fn real_schema_has_supported_tables_and_views_with_honest_zero_count() {
        let tree = ConnectionTree::build(
            Uuid::new_v4(),
            &[table("main", "customers", "table")],
            &[],
            true,
            "",
        );
        assert_eq!(tree.schemas[0].folders.len(), 2);
        assert_eq!(tree.schemas[0].folders[0].kind, ObjectKind::Table);
        assert_eq!(tree.schemas[0].folders[0].objects.len(), 1);
        assert_eq!(tree.schemas[0].folders[1].kind, ObjectKind::View);
        assert!(tree.schemas[0].folders[1].objects.is_empty());
    }
    #[test]
    fn unknown_metadata_has_own_category_without_becoming_a_table() {
        assert_eq!(ObjectKind::from_metadata("sequence"), ObjectKind::Other);
        assert_eq!(
            ObjectKind::from_metadata("system versioned"),
            ObjectKind::Table
        );
    }
    #[test]
    fn descendant_query_match_keeps_group_ancestors_visible() {
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        let connection = Uuid::new_v4();
        let snapshot = Snapshot {
            revision: 1,
            groups: vec![group(root, "Root", None), group(child, "Child", Some(root))],
            memberships: HashMap::from([(connection, child)]),
        };
        assert!(group_visible(
            &snapshot,
            root,
            &HashSet::from([connection]),
            "monthly",
            0
        ));
        assert!(!group_visible(
            &snapshot,
            root,
            &HashSet::new(),
            "monthly",
            0
        ));
        assert!(group_visible(&snapshot, root, &HashSet::new(), "child", 0));
        // A matching ancestor's render path uses an empty search for every child.
        assert!(group_visible(&snapshot, child, &HashSet::new(), "", 1));
    }
    #[test]
    fn group_counts_only_alive_descendants_and_ignores_orphan_membership() {
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        let a = Uuid::new_v4();
        let stale = Uuid::new_v4();
        let orphan = Uuid::new_v4();
        let snapshot = Snapshot {
            revision: 1,
            groups: vec![group(root, "Root", None), group(child, "Child", Some(root))],
            memberships: HashMap::from([(a, child), (stale, root), (orphan, Uuid::new_v4())]),
        };
        assert_eq!(
            descendant_connection_count(&snapshot, root, &HashSet::from([a, orphan])),
            1
        );
        assert_eq!(
            descendant_connection_count(&snapshot, child, &HashSet::from([a])),
            1
        );
    }
    #[test]
    fn corrupt_cycle_is_bounded_and_missing_group_never_appears() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let connection = Uuid::new_v4();
        let snapshot = Snapshot {
            revision: 1,
            groups: vec![group(a, "A", Some(b)), group(b, "B", Some(a))],
            memberships: HashMap::from([(connection, a)]),
        };
        assert!(!group_visible(&snapshot, a, &HashSet::new(), "missing", 0));
        assert!(!group_visible(
            &snapshot,
            Uuid::new_v4(),
            &HashSet::new(),
            "",
            0
        ));
        assert_eq!(
            descendant_connection_count(&snapshot, b, &HashSet::from([connection])),
            1
        );
    }
}
