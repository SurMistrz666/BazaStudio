//! Changes of the grid as CQL: one cell, a new row, a removed row, and the
//! rows of an import.
//!
//! CQL writes are upserts: an `UPDATE` of a row that is gone creates it, and
//! an `INSERT` of a key that exists overwrites it. Every change of the grid
//! is therefore a lightweight transaction, which the server applies only
//! while its condition holds: `IF column = old value` for a cell,
//! `IF NOT EXISTS` for a new row and `IF EXISTS` for a removed one.
use super::catalog::{Column, Role, Table};
use super::value;
use crate::drivers::RowChange;
use crate::model::Cell;
use anyhow::{Result, anyhow, bail, ensure};
use std::collections::{BTreeMap, HashSet};

/// Most statements of one batch of an import. Every batch stays inside one
/// partition, where the server applies it as one write.
pub(super) const BATCH_STATEMENTS: usize = 30;
/// Most text of one batch. The server refuses a batch whose data passes its
/// `batch_size_fail_threshold` (50 KiB unless configured otherwise).
pub(super) const BATCH_BYTES: usize = 20 * 1024;

/// A table whose rows the grid may change.
fn writable(table: &Table) -> Result<()> {
    ensure!(table.writable, "cassandra_read_only_object");
    // Counters are only ever added to (`SET c = c + 1`), never set, and a
    // counter table takes no conditions.
    ensure!(!table.has_counters(), "cassandra_counter_read_only");
    Ok(())
}

/// `"column" = literal` for every column of the key that addresses a row,
/// from the cells the grid holds.
fn key_terms<'a>(
    table: &Table,
    columns: impl Iterator<Item = &'a Column>,
    key: &[(String, Cell)],
) -> Result<Vec<String>> {
    columns
        .map(|column| {
            let cell = key
                .iter()
                .find(|(name, _)| *name == column.name)
                .map(|(_, cell)| cell)
                .ok_or_else(|| anyhow!("primary_key_required"))?;
            // A row that only carries the static columns of its partition
            // has no clustering key: there is nothing to address.
            ensure!(!matches!(cell, Cell::Null), "cassandra_static_row");
            Ok(format!(
                "{} = {}",
                value::name(&column.name)?,
                value::literal(&column.ty, cell, &table.udts)?
            ))
        })
        .collect()
}

/// The statement that makes one change of one record, with the condition
/// under which the server applies it.
pub(super) fn statement(table: &Table, change: &RowChange) -> Result<String> {
    writable(table)?;
    let target = value::qualified(&table.keyspace, &table.name)?;
    match change {
        RowChange::UpdateCell {
            column,
            old,
            new,
            key,
            ..
        } => {
            let column = table
                .column(column)
                .ok_or_else(|| anyhow!("cassandra_column_unknown"))?;
            // A key column is where the row is; it cannot be given another
            // value.
            ensure!(!column.is_key(), "cassandra_key_read_only");
            let name = value::name(&column.name)?;
            // A static column belongs to the partition: it is addressed
            // without the clustering columns.
            let terms = if column.role == Role::Static {
                key_terms(table, table.partition_key(), key)?
            } else {
                key_terms(table, table.columns.iter().filter(|c| c.is_key()), key)?
            };
            ensure!(!terms.is_empty(), "primary_key_required");
            Ok(format!(
                "UPDATE {target} SET {name} = {} WHERE {} IF {name} = {}",
                value::literal(&column.ty, new, &table.udts)?,
                terms.join(" AND "),
                value::literal(&column.ty, old, &table.udts)?
            ))
        }
        RowChange::InsertRow { values, .. } => {
            let (names, literals) = insert_parts(table, values)?;
            Ok(format!(
                "INSERT INTO {target} ({}) VALUES ({}) IF NOT EXISTS",
                names.join(", "),
                literals.join(", ")
            ))
        }
        RowChange::DeleteRow { key, .. } => {
            let terms = key_terms(table, table.columns.iter().filter(|c| c.is_key()), key)?;
            ensure!(!terms.is_empty(), "primary_key_required");
            Ok(format!(
                "DELETE FROM {target} WHERE {} IF EXISTS",
                terms.join(" AND ")
            ))
        }
    }
}

/// The names and literals of an `INSERT`: the whole key and every other
/// column that has a value. A column without one is left out, which in CQL
/// is the same as nothing and writes no tombstone.
fn insert_parts(table: &Table, values: &[(String, Cell)]) -> Result<(Vec<String>, Vec<String>)> {
    for (name, _) in values {
        ensure!(table.column(name).is_some(), "cassandra_column_unknown");
    }
    let (mut names, mut literals) = (Vec::new(), Vec::new());
    for column in &table.columns {
        let cell = values
            .iter()
            .find(|(name, _)| *name == column.name)
            .map(|(_, cell)| cell);
        match cell {
            None | Some(Cell::Null) => ensure!(!column.is_key(), "cassandra_key_required"),
            Some(cell) => {
                names.push(value::name(&column.name)?);
                literals.push(value::literal(&column.ty, cell, &table.udts)?);
            }
        }
    }
    Ok((names, literals))
}

/// One row of an import, ready to be written.
#[derive(Debug, PartialEq)]
pub(super) struct ImportRow {
    /// `INSERT INTO … VALUES (…)`.
    pub insert: String,
    /// `SELECT … WHERE <key> LIMIT 1`: is there a row with this key?
    pub exists: String,
    /// The literals of the partition key: rows that share them share a batch.
    pub partition: String,
}

/// Turn the rows of an import into statements. Nothing is written when any
/// value cannot be written or two rows of the file have the same key.
pub(super) fn import_rows(
    table: &Table,
    columns: &[String],
    rows: &[Vec<Cell>],
) -> Result<Vec<ImportRow>> {
    writable(table)?;
    let target = value::qualified(&table.keyspace, &table.name)?;
    for column in table.columns.iter().filter(|column| column.is_key()) {
        ensure!(
            columns.contains(&column.name),
            "cassandra_import_key_missing"
        );
    }
    let first_key = table
        .columns
        .iter()
        .find(|column| column.is_key())
        .ok_or_else(|| anyhow!("primary_key_required"))?;
    let first_key = value::name(&first_key.name)?;
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        ensure!(row.len() == columns.len(), "import_shape_invalid");
        let values: Vec<(String, Cell)> =
            columns.iter().cloned().zip(row.iter().cloned()).collect();
        let (names, literals) = insert_parts(table, &values)?;
        let terms = key_terms(table, table.columns.iter().filter(|c| c.is_key()), &values)?;
        let partition = key_terms(table, table.partition_key(), &values)?.join(" AND ");
        let key = terms.join(" AND ");
        ensure!(seen.insert(key.clone()), "cassandra_import_duplicate");
        out.push(ImportRow {
            insert: format!(
                "INSERT INTO {target} ({}) VALUES ({})",
                names.join(", "),
                literals.join(", ")
            ),
            exists: format!("SELECT {first_key} FROM {target} WHERE {key} LIMIT 1"),
            partition,
        });
    }
    Ok(out)
}

/// Group the inserts of an import into the statements that are sent: the
/// rows of one partition together, as an unlogged batch the server applies
/// as one write, and a row alone as it is.
pub(super) fn batches(rows: &[ImportRow]) -> Vec<String> {
    let mut partitions: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut order = Vec::new();
    for row in rows {
        let group = partitions.entry(row.partition.as_str()).or_default();
        if group.is_empty() {
            order.push(row.partition.as_str());
        }
        group.push(row.insert.as_str());
    }
    let mut out = Vec::new();
    for partition in order {
        let mut chunk: Vec<&str> = Vec::new();
        let mut bytes = 0;
        let mut flush = |chunk: &mut Vec<&str>| {
            match chunk.as_slice() {
                [] => {}
                [single] => out.push((*single).to_owned()),
                many => out.push(format!(
                    "BEGIN UNLOGGED BATCH\n{};\nAPPLY BATCH",
                    many.join(";\n")
                )),
            }
            chunk.clear();
        };
        for insert in &partitions[partition] {
            if !chunk.is_empty()
                && (chunk.len() >= BATCH_STATEMENTS || bytes + insert.len() > BATCH_BYTES)
            {
                flush(&mut chunk);
                bytes = 0;
            }
            bytes += insert.len();
            chunk.push(insert);
        }
        flush(&mut chunk);
    }
    out
}

/// What kind of change it is, for the code a refused condition gets.
pub(super) fn refused(change: &RowChange) -> anyhow::Error {
    match change {
        // The key is taken.
        RowChange::InsertRow { .. } => anyhow!("cassandra_row_exists"),
        // The cell no longer holds the old value, or the row is gone.
        RowChange::UpdateCell { .. } | RowChange::DeleteRow { .. } => {
            anyhow!("row_changed_reload_required")
        }
    }
}

/// The table a change is made in.
pub(super) fn table_of(change: &RowChange) -> &crate::model::TableRef {
    match change {
        RowChange::UpdateCell { table, .. }
        | RowChange::InsertRow { table, .. }
        | RowChange::DeleteRow { table, .. } => table,
    }
}

/// Refuse a change of something that is not a table before asking the server.
pub(super) fn check_kind(change: &RowChange) -> Result<()> {
    if !table_of(change).kind.contains("table") {
        bail!("cassandra_read_only_object");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::cassandra::types::{CqlType, Udts};
    use crate::model::TableRef;
    use std::sync::Arc;

    fn column(name: &str, ty: &str, role: Role) -> Column {
        Column {
            name: name.into(),
            ty: CqlType::parse(ty),
            role,
        }
    }

    fn readings() -> Table {
        Table::new(
            "shop",
            "readings",
            vec![
                column("device", "uuid", Role::Partition(0)),
                column("at", "timestamp", Role::Clustering(0, true)),
                column("owner", "text", Role::Static),
                column("amount", "decimal", Role::Regular),
                column("tags", "set<text>", Role::Regular),
                column("label", "text", Role::Regular),
            ],
            Arc::new(Udts::new()),
            true,
        )
    }

    fn reference() -> TableRef {
        TableRef {
            schema: "shop".into(),
            name: "readings".into(),
            kind: "table".into(),
        }
    }

    fn text(value: &str) -> Cell {
        Cell::Text(value.into())
    }

    fn key() -> Vec<(String, Cell)> {
        vec![
            (
                "device".into(),
                text("8e8b1c2a-4f3d-4e7b-9c1a-2b3c4d5e6f70"),
            ),
            ("at".into(), text("2024-05-01T12:34:56.789Z")),
        ]
    }

    const KEY: &str =
        "\"device\" = 8e8b1c2a-4f3d-4e7b-9c1a-2b3c4d5e6f70 AND \"at\" = '2024-05-01T12:34:56.789Z'";

    #[test]
    fn a_cell_is_changed_only_while_it_holds_the_old_value() {
        let update = |column: &str, old: Cell, new: Cell| {
            statement(
                &readings(),
                &RowChange::UpdateCell {
                    table: reference(),
                    column: column.into(),
                    old,
                    new,
                    key: key(),
                },
            )
            .map_err(|error| error.to_string())
        };
        assert_eq!(
            update("label", text("old"), text("it's new")).unwrap(),
            format!(
                "UPDATE \"shop\".\"readings\" SET \"label\" = 'it''s new' WHERE {KEY} IF \"label\" = 'old'"
            )
        );
        // Nothing is a value of its own on both sides.
        assert_eq!(
            update(
                "amount",
                Cell::Null,
                Cell::Decimal("12345678901234567890.12".into())
            )
            .unwrap(),
            format!(
                "UPDATE \"shop\".\"readings\" SET \"amount\" = 12345678901234567890.12 WHERE {KEY} IF \"amount\" = null"
            )
        );
        assert!(
            update("amount", Cell::Decimal("1.50".into()), Cell::Null)
                .unwrap()
                .contains("SET \"amount\" = null WHERE")
        );
        assert!(
            update(
                "tags",
                Cell::Json(r#"["a"]"#.into()),
                Cell::Json(r#"["a","b"]"#.into())
            )
            .unwrap()
            .contains("SET \"tags\" = {'a', 'b'} WHERE")
        );
        // A static column is addressed by the partition alone.
        assert_eq!(
            update("owner", text("a"), text("b")).unwrap(),
            "UPDATE \"shop\".\"readings\" SET \"owner\" = 'b' WHERE \"device\" = 8e8b1c2a-4f3d-4e7b-9c1a-2b3c4d5e6f70 IF \"owner\" = 'a'"
        );
        assert_eq!(
            update("device", text("x"), text("y")).unwrap_err(),
            "cassandra_key_read_only"
        );
        assert_eq!(
            update("nope", text("x"), text("y")).unwrap_err(),
            "cassandra_column_unknown"
        );
        assert_eq!(
            update("amount", Cell::Null, text("abc")).unwrap_err(),
            "decimal_invalid"
        );
    }

    #[test]
    fn a_row_is_added_only_when_its_key_is_free_and_removed_only_when_it_exists() {
        let insert = |values: Vec<(String, Cell)>| {
            statement(
                &readings(),
                &RowChange::InsertRow {
                    table: reference(),
                    values,
                },
            )
            .map_err(|error| error.to_string())
        };
        let mut values = key();
        values.push(("label".into(), text("new")));
        values.push(("amount".into(), Cell::Null));
        // A column without a value is left out: it writes no tombstone.
        assert_eq!(
            insert(values).unwrap(),
            "INSERT INTO \"shop\".\"readings\" (\"device\", \"at\", \"label\") VALUES (8e8b1c2a-4f3d-4e7b-9c1a-2b3c4d5e6f70, '2024-05-01T12:34:56.789Z', 'new') IF NOT EXISTS"
        );
        assert_eq!(
            insert(vec![("label".into(), text("x"))]).unwrap_err(),
            "cassandra_key_required"
        );
        assert_eq!(insert(Vec::new()).unwrap_err(), "cassandra_key_required");
        assert_eq!(
            insert(vec![("nope".into(), text("x"))]).unwrap_err(),
            "cassandra_column_unknown"
        );
        let delete = |key: Vec<(String, Cell)>| {
            statement(
                &readings(),
                &RowChange::DeleteRow {
                    table: reference(),
                    key,
                },
            )
            .map_err(|error| error.to_string())
        };
        assert_eq!(
            delete(key()).unwrap(),
            format!("DELETE FROM \"shop\".\"readings\" WHERE {KEY} IF EXISTS")
        );
        assert_eq!(
            delete(vec![key().remove(0)]).unwrap_err(),
            "primary_key_required"
        );
        // The row of a partition that only has static values has no
        // clustering key to address it by.
        let mut partial = key();
        partial[1].1 = Cell::Null;
        assert_eq!(delete(partial).unwrap_err(), "cassandra_static_row");
    }

    #[test]
    fn counters_views_and_virtual_tables_are_not_written() {
        let counters = Table::new(
            "shop",
            "hits",
            vec![
                column("page", "text", Role::Partition(0)),
                column("count", "counter", Role::Regular),
            ],
            Arc::new(Udts::new()),
            true,
        );
        let change = RowChange::UpdateCell {
            table: reference(),
            column: "count".into(),
            old: Cell::Integer("1".into()),
            new: Cell::Integer("2".into()),
            key: vec![("page".into(), text("/"))],
        };
        assert_eq!(
            statement(&counters, &change).unwrap_err().to_string(),
            "cassandra_counter_read_only"
        );
        let mut view = readings();
        view.writable = false;
        assert_eq!(
            statement(
                &view,
                &RowChange::DeleteRow {
                    table: reference(),
                    key: key()
                }
            )
            .unwrap_err()
            .to_string(),
            "cassandra_read_only_object"
        );
        let mut other = reference();
        other.kind = "materialized view".into();
        assert!(
            check_kind(&RowChange::DeleteRow {
                table: other,
                key: key()
            })
            .is_err()
        );
        assert_eq!(
            refused(&RowChange::InsertRow {
                table: reference(),
                values: Vec::new()
            })
            .to_string(),
            "cassandra_row_exists"
        );
        assert_eq!(refused(&change).to_string(), "row_changed_reload_required");
    }

    #[test]
    fn an_import_is_checked_whole_and_written_partition_by_partition() {
        let table = readings();
        let columns = ["device", "at", "label"].map(str::to_owned);
        let row = |device: u8, minute: u8, label: &str| {
            vec![
                text(&format!("00000000-0000-0000-0000-0000000000{device:02x}")),
                text(&format!("2024-05-01T12:{minute:02}:00.000Z")),
                text(label),
            ]
        };
        let rows = vec![row(1, 0, "a"), row(2, 0, "b"), row(1, 1, "c")];
        let planned = import_rows(&table, &columns, &rows).unwrap();
        assert_eq!(planned.len(), 3);
        assert_eq!(
            planned[0].exists,
            "SELECT \"device\" FROM \"shop\".\"readings\" WHERE \"device\" = 00000000-0000-0000-0000-000000000001 AND \"at\" = '2024-05-01T12:00:00.000Z' LIMIT 1"
        );
        assert_eq!(planned[0].partition, planned[2].partition);
        // Two rows of one partition travel together; the third alone.
        let sent = batches(&planned);
        assert_eq!(sent.len(), 2);
        assert!(sent[0].starts_with("BEGIN UNLOGGED BATCH\nINSERT INTO"));
        assert!(sent[0].ends_with(";\nAPPLY BATCH") && sent[0].matches("INSERT INTO").count() == 2);
        assert!(sent[1].starts_with("INSERT INTO") && sent[1].contains("'b'"));
        // A large partition is cut into batches of a bounded size.
        let many: Vec<Vec<Cell>> = (0..70).map(|minute| row(1, minute % 60, "x")).collect();
        let many: Vec<Vec<Cell>> = many
            .into_iter()
            .enumerate()
            .map(|(index, mut cells)| {
                cells[1] = text(&format!(
                    "2024-05-01T{:02}:{:02}:00.000Z",
                    index / 60,
                    index % 60
                ));
                cells
            })
            .collect();
        let sent = batches(&import_rows(&table, &columns, &many).unwrap());
        assert_eq!(sent.len(), 3);
        assert!(sent.iter().all(|batch| batch.len() <= BATCH_BYTES + 400));
        let wide = vec![
            vec![
                text("00000000-0000-0000-0000-000000000001"),
                text("2024-05-01T12:00:00.000Z"),
                text(&"x".repeat(BATCH_BYTES)),
            ];
            1
        ];
        assert_eq!(
            batches(&import_rows(&table, &columns, &wide).unwrap()).len(),
            1
        );
        // What cannot be written stops the import before anything is sent.
        let failed = |columns: &[String], rows: &[Vec<Cell>]| {
            import_rows(&table, columns, rows).unwrap_err().to_string()
        };
        assert_eq!(
            failed(&columns, &[row(1, 0, "a"), row(1, 0, "again")]),
            "cassandra_import_duplicate"
        );
        assert_eq!(
            failed(
                &["device".into(), "label".into()],
                &[vec![text("x"), text("y")]]
            ),
            "cassandra_import_key_missing"
        );
        let mut broken = row(1, 0, "a");
        broken[0] = text("not a uuid");
        assert_eq!(failed(&columns, &[broken]), "cassandra_value_invalid");
        let mut empty_key = row(1, 0, "a");
        empty_key[1] = Cell::Null;
        assert_eq!(failed(&columns, &[empty_key]), "cassandra_key_required");
    }
}
