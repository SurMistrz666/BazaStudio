//! What the driver knows about a table: its columns with their types and
//! their part in the primary key, read from the schema tables of the node.
use super::types::{CqlType, Udts};
use crate::model::{Cell, ColumnInfo};
use std::sync::Arc;

/// The part a column plays in its table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    /// A column of the partition key, with its position in it.
    Partition(i64),
    /// A clustering column, with its position and whether it sorts
    /// descending.
    Clustering(i64, bool),
    /// One value per partition.
    Static,
    Regular,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Column {
    pub name: String,
    pub ty: CqlType,
    pub role: Role,
}

impl Column {
    pub(super) fn is_key(&self) -> bool {
        matches!(self.role, Role::Partition(_) | Role::Clustering(..))
    }

    /// The type as the grid names it: with the part the column plays, which
    /// decides what a query may ask of it.
    pub(super) fn label(&self) -> String {
        let name = self.ty.name();
        match self.role {
            Role::Partition(_) => format!("{name} · partition key"),
            Role::Clustering(_, false) => format!("{name} · clustering key"),
            Role::Clustering(_, true) => format!("{name} · clustering key desc"),
            Role::Static => format!("{name} · static"),
            Role::Regular => name,
        }
    }
}

/// A table, a materialized view or a virtual table.
#[derive(Debug)]
pub(super) struct Table {
    pub keyspace: String,
    pub name: String,
    /// In the order `SELECT *` returns them: partition key, clustering
    /// columns, static columns, the others by name.
    pub columns: Vec<Column>,
    /// The user-defined types of the keyspace.
    pub udts: Arc<Udts>,
    /// A base table: its rows can be written. A materialized view and a
    /// virtual table of the node cannot.
    pub writable: bool,
}

impl Table {
    /// Put the columns of the schema tables in the order of `SELECT *`.
    pub(super) fn new(
        keyspace: &str,
        name: &str,
        mut columns: Vec<Column>,
        udts: Arc<Udts>,
        writable: bool,
    ) -> Table {
        let rank = |column: &Column| match column.role {
            Role::Partition(position) => (0, position),
            Role::Clustering(position, _) => (1, position),
            Role::Static => (2, 0),
            Role::Regular => (3, 0),
        };
        columns.sort_by(|a, b| rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name)));
        Table {
            keyspace: keyspace.to_owned(),
            name: name.to_owned(),
            columns,
            udts,
            writable,
        }
    }

    pub(super) fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|column| column.name == name)
    }

    pub(super) fn index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    pub(super) fn partition_key(&self) -> impl Iterator<Item = &Column> {
        self.columns
            .iter()
            .filter(|column| matches!(column.role, Role::Partition(_)))
    }

    /// A table of counters: its values are only ever added to.
    pub(super) fn has_counters(&self) -> bool {
        self.columns.iter().any(|column| column.ty.is_counter())
    }

    /// The columns as the grid asks for them.
    pub(super) fn column_info(&self) -> Vec<ColumnInfo> {
        self.columns
            .iter()
            .map(|column| ColumnInfo {
                name: column.name.clone(),
                data_type: column.label(),
                nullable: !column.is_key(),
                primary_key: column.is_key(),
                default: None,
            })
            .collect()
    }
}

/// One row of `system_schema.columns` as a column: name, kind, position,
/// type and clustering order.
pub(super) fn column_of(row: &[Cell]) -> Option<Column> {
    let [name, kind, position, ty, order, ..] = row else {
        return None;
    };
    let position = position.display().parse::<i64>().unwrap_or(0);
    let role = match kind.display().as_str() {
        "partition_key" => Role::Partition(position),
        "clustering" => Role::Clustering(position, order.display().eq_ignore_ascii_case("desc")),
        "static" => Role::Static,
        _ => Role::Regular,
    };
    Some(Column {
        name: name.display(),
        ty: CqlType::parse(&ty.display()),
        role,
    })
}

/// The rows of `system_schema.types` of a keyspace as its user-defined
/// types: name, field names and field types, the two lists as JSON text.
pub(super) fn udts_of(rows: &[Vec<Cell>]) -> Udts {
    let list =
        |cell: &Cell| -> Vec<String> { serde_json::from_str(&cell.display()).unwrap_or_default() };
    rows.iter()
        .filter_map(|row| match row.as_slice() {
            [name, names, types, ..] => Some((
                name.display(),
                list(names)
                    .into_iter()
                    .zip(list(types))
                    .map(|(field, ty)| (field, CqlType::parse(&ty)))
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}

/// A keyspace of the server itself.
pub(super) fn is_system(keyspace: &str) -> bool {
    keyspace == "system" || keyspace.starts_with("system_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, kind: &str, position: i64, ty: &str, order: &str) -> Vec<Cell> {
        vec![
            Cell::Text(name.into()),
            Cell::Text(kind.into()),
            Cell::Integer(position.to_string()),
            Cell::Text(ty.into()),
            Cell::Text(order.into()),
        ]
    }

    #[test]
    fn columns_come_in_the_order_of_a_select_and_say_their_part() {
        let columns = [
            row("note", "regular", -1, "text", "none"),
            row("at", "clustering", 1, "timestamp", "desc"),
            row("region", "partition_key", 1, "text", "none"),
            row("amount", "regular", -1, "decimal", "none"),
            row("device", "clustering", 0, "uuid", "asc"),
            row("owner", "static", -1, "text", "none"),
            row("tenant", "partition_key", 0, "int", "none"),
        ]
        .iter()
        .filter_map(|row| column_of(row))
        .collect();
        let table = Table::new("ks", "readings", columns, Arc::new(Udts::new()), true);
        let names: Vec<&str> = table
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "tenant", "region", "device", "at", "owner", "amount", "note"
            ]
        );
        let info = table.column_info();
        assert_eq!(info[0].data_type, "int · partition key");
        assert_eq!(info[2].data_type, "uuid · clustering key");
        assert_eq!(info[3].data_type, "timestamp · clustering key desc");
        assert_eq!(info[4].data_type, "text · static");
        assert_eq!(info[5].data_type, "decimal");
        // The whole primary key addresses a row; nothing else does.
        assert_eq!(
            info.iter()
                .map(|column| column.primary_key)
                .collect::<Vec<_>>(),
            [true, true, true, true, false, false, false]
        );
        assert!(info[..4].iter().all(|column| !column.nullable));
        assert!(info[4..].iter().all(|column| column.nullable));
        assert_eq!(table.partition_key().count(), 2);
        assert_eq!(table.index("amount"), Some(5));
        assert!(table.column("nope").is_none());
        assert!(!table.has_counters());
        // The grid decides the kind of a typed value by these words; the
        // parts a column plays must not read as a number type.
        for column in &info {
            let upper = column.data_type.to_uppercase();
            let tail = upper.split_once(" · ").map_or("", |(_, tail)| tail);
            assert!(!tail.contains("INT") && !tail.contains("REAL") && !tail.contains("FLOAT"));
        }
    }

    #[test]
    fn user_defined_types_are_read_from_their_lists() {
        let udts = udts_of(&[vec![
            Cell::Text("address".into()),
            Cell::Json(r#"["street","zip"]"#.into()),
            Cell::Json(r#"["text","frozen<list<int>>"]"#.into()),
        ]]);
        assert_eq!(
            udts["address"],
            [
                ("street".to_owned(), CqlType::parse("text")),
                ("zip".to_owned(), CqlType::parse("frozen<list<int>>")),
            ]
        );
        assert!(is_system("system") && is_system("system_views") && !is_system("systems"));
    }
}
