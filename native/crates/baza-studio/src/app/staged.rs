//! Changes typed into a table's grid wait there until they are saved, as in
//! the database tools people know: an edited cell is marked, a new row is a
//! row at the end of the grid, and ✓ (Mod+S) shows what is about to be
//! written and, once confirmed, writes everything in one go, while ✕ leaves
//! it. Mod+Z takes the waiting changes back one by one.
use super::*;

/// The tab, its database, the changes to make and how to take each back.
type PreparedSave = (Uuid, Database, Vec<RowChange>, Vec<Option<RowChange>>);

/// What ✓ or Mod+S is about to write, waiting for "Zapisz".
pub(super) struct SaveConfirm {
    pub tab: Uuid,
    /// New rows.
    pub inserts: usize,
    /// Changed cells, and the rows they are in.
    pub cells: usize,
    pub rows: usize,
    /// The statements, as the history will show them.
    pub statements: Vec<String>,
    /// The widget that had the keyboard before the dialog took it.
    pub focus_back: Option<egui::Id>,
}

impl StudioApp {
    /// Put new values into cells of a tab's grid, to be saved later: `None`
    /// is NULL. Cells that cannot be changed are left.
    pub(super) fn edit_cells(&mut self, index: usize, edits: Vec<(usize, usize, Option<String>)>) {
        let caps = *self.capabilities(self.tabs[index].connection);
        let tab = &mut self.tabs[index];
        if tab.table.is_none() || !caps.edit_cells && tab.added.is_empty() {
            self.notify("result_read_only".into(), true);
            return;
        }
        let keyed = tab.columns.iter().any(|c| c.primary_key);
        let Some(result) = &tab.result else {
            return;
        };
        let mut batch = Vec::new();
        let mut refused = None;
        for (r, c, value) in edits {
            let (Some(row), Some(column)) = (result.rows.get(r), result.columns.get(c)) else {
                continue;
            };
            let added = tab.added.contains(&r);
            // An existing record is found again by its key.
            if !added && !keyed {
                refused = Some("primary_key_required".to_owned());
                continue;
            }
            let original = &row[c];
            let new = if added {
                match &value {
                    None => Cell::Null,
                    Some(text) => super::dialogs::typed_cell(&column.data_type, text),
                }
            } else {
                if !original.editable() {
                    continue;
                }
                match original.from_edited(value.as_deref().unwrap_or(""), value.is_none()) {
                    Ok(cell) => cell,
                    Err(error) => {
                        refused = Some(error.to_string());
                        continue;
                    }
                }
            };
            let previous = tab.pending.get(&(r, c)).cloned();
            if !added && &new == original {
                if previous.is_some() {
                    tab.pending.remove(&(r, c));
                    batch.push(((r, c), previous));
                }
                continue;
            }
            if previous.as_ref() == Some(&new) {
                continue;
            }
            tab.pending.insert((r, c), new);
            batch.push(((r, c), previous));
        }
        if !batch.is_empty() {
            tab.pending_log.push(batch);
        }
        if let Some(error) = refused {
            self.notify(error, true);
        }
    }

    /// Mod+Z while changes wait: the last one goes back.
    pub(super) fn undo_staged(&mut self, index: usize) -> bool {
        let tab = &mut self.tabs[index];
        let Some(batch) = tab.pending_log.pop() else {
            return false;
        };
        for (cell, previous) in batch.into_iter().rev() {
            match previous {
                Some(value) => {
                    tab.pending.insert(cell, value);
                }
                None => {
                    tab.pending.remove(&cell);
                }
            }
        }
        true
    }

    /// "+": a new row at the end of the grid, typed into in place.
    pub(super) fn add_row(&mut self, index: usize) {
        let tab = &mut self.tabs[index];
        let Some(result) = tab.result.as_mut() else {
            return;
        };
        let row = result.rows.len();
        result.rows.push(vec![Cell::Null; result.columns.len()]);
        tab.added.insert(row);
        // The first column the database does not fill in itself.
        let column = result
            .columns
            .iter()
            .position(|c| {
                tab.columns
                    .iter()
                    .find(|i| i.name == c.name)
                    .is_none_or(|i| !(i.default.is_some() || super::results::generated_key(i)))
            })
            .unwrap_or(0);
        tab.selected_row = Some(row);
        tab.selected_cell = Some((row, column));
        tab.range = Some(super::results::GridRange::cell(row, column));
        tab.scroll_to = Some(row);
    }

    /// ✓ or Mod+S: show what is about to be written and ask first.
    pub(super) fn ask_save(&mut self, index: usize) {
        if !self.take_cell_editor(index) {
            return;
        }
        match self.prepare_staged(index) {
            Ok((_, _, changes, _)) if changes.is_empty() => self.discard_staged(index),
            Ok((id, db, changes, _)) => {
                let tab = &self.tabs[index];
                let rows = tab
                    .pending
                    .keys()
                    .filter(|(r, _)| !tab.added.contains(r))
                    .map(|(r, _)| *r)
                    .collect::<std::collections::BTreeSet<_>>()
                    .len();
                let inserts = changes
                    .iter()
                    .filter(|c| matches!(c, RowChange::InsertRow { .. }))
                    .count();
                self.save_confirm = Some(SaveConfirm {
                    tab: id,
                    inserts,
                    cells: changes.len() - inserts,
                    rows,
                    statements: changes
                        .iter()
                        .map(|change| db.change_text(change).unwrap_or_default())
                        .collect(),
                    focus_back: None,
                });
            }
            Err(error) => self.notify(error.to_string(), true),
        }
    }

    /// Put the value of an open cell editor of this tab among its waiting
    /// changes. Says why and returns `false` when the value is refused.
    fn take_cell_editor(&mut self, index: usize) -> bool {
        if let Some(editor) = self.cell_editor.take() {
            if editor.tab == self.tabs[index].id {
                if let Err(error) = self.commit_cell(editor) {
                    self.notify(error, true);
                    return false;
                }
            } else {
                self.cell_editor = Some(editor);
            }
        }
        true
    }

    /// Write every waiting change of a tab in one go, once "Zapisz" said so.
    pub(super) fn apply_staged(&mut self, index: usize) {
        if !self.take_cell_editor(index) {
            return;
        }
        match self.prepare_staged(index) {
            Ok((_, _, changes, _)) if changes.is_empty() => {
                self.discard_staged(index);
            }
            Ok((id, db, changes, inverses)) => {
                self.remember_changes(id, &changes, inverses, ChangeKind::Do);
                if let Err(error) = self.mutate_many(id, db, changes) {
                    self.pending_changes.remove(&id);
                    self.notify(error.to_string(), true);
                    return;
                }
                // The page is read again when the database has answered; if
                // it refused the first change, the waiting changes come back.
                let tab = &mut self.tabs[index];
                let backup = (
                    std::mem::take(&mut tab.pending),
                    std::mem::take(&mut tab.added),
                    std::mem::take(&mut tab.pending_log),
                );
                self.staged_backup.insert(id, backup);
            }
            Err(error) => self.notify(error.to_string(), true),
        }
    }

    /// The waiting changes of a tab as changes of records: new rows first,
    /// then the changed cells, row by row.
    fn prepare_staged(&self, index: usize) -> anyhow::Result<PreparedSave> {
        let tab = &self.tabs[index];
        let result = tab
            .result
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("result_changed_reload_required"))?;
        let table = tab
            .table
            .clone()
            .ok_or_else(|| anyhow::anyhow!("result_read_only"))?;
        let db = tab
            .connection
            .and_then(|id| self.databases.get(&id))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("connection_unavailable"))?;
        let key_names: Vec<String> = tab
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| c.name.clone())
            .collect();
        let mut changes = Vec::new();
        let mut inverses = Vec::new();
        // New rows first, in their order, with the values typed into them.
        for &r in &tab.added {
            let values: Vec<(String, Cell)> = result
                .columns
                .iter()
                .enumerate()
                .filter_map(|(c, column)| {
                    tab.pending
                        .get(&(r, c))
                        .map(|v| (column.name.clone(), v.clone()))
                })
                .collect();
            if values.is_empty() {
                continue;
            }
            let change = RowChange::InsertRow {
                table: table.clone(),
                values,
            };
            inverses.push(inverse_change(&change, None, &key_names));
            changes.push(change);
        }
        // Changed records, row by row: the other columns first, found by
        // the key the row has; then the key columns, each found by the
        // key as the ones before it left it.
        let rows: std::collections::BTreeSet<usize> = tab
            .pending
            .keys()
            .map(|(r, _)| *r)
            .filter(|r| !tab.added.contains(r))
            .collect();
        for r in rows {
            let row = &result.rows[r];
            let mut key = key_names
                .iter()
                .map(|name| {
                    let i = result
                        .columns
                        .iter()
                        .position(|v| &v.name == name)
                        .ok_or_else(|| anyhow::anyhow!("primary_key_required"))?;
                    Ok((name.clone(), row[i].clone()))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            anyhow::ensure!(!key.is_empty(), "primary_key_required");
            let mut cells: Vec<(usize, &Cell)> = tab
                .pending
                .range((r, 0)..=(r, usize::MAX))
                .map(|(&(_, c), v)| (c, v))
                .collect();
            cells.sort_by_key(|(c, _)| key_names.contains(&result.columns[*c].name));
            for (c, new) in cells {
                let column = result.columns[c].name.clone();
                let change = RowChange::UpdateCell {
                    table: table.clone(),
                    column: column.clone(),
                    old: row[c].clone(),
                    new: new.clone(),
                    key: key.clone(),
                };
                inverses.push(inverse_change(&change, None, &[]));
                changes.push(change);
                if let Some(part) = key.iter_mut().find(|(name, _)| *name == column) {
                    part.1 = new.clone();
                }
            }
        }
        Ok((tab.id, db, changes, inverses))
    }

    /// ✕: leave every waiting change and remove the new rows.
    pub(super) fn discard_staged(&mut self, index: usize) {
        if self
            .cell_editor
            .as_ref()
            .is_some_and(|e| e.tab == self.tabs[index].id)
        {
            self.cell_editor = None;
        }
        let tab = &mut self.tabs[index];
        if let Some(result) = tab.result.as_mut() {
            for &r in tab.added.iter().rev() {
                if r < result.rows.len() {
                    result.rows.remove(r);
                }
            }
        }
        tab.added.clear();
        tab.pending.clear();
        tab.pending_log.clear();
    }

    /// Remove rows added in the grid and not saved, keeping the waiting
    /// values of the others in place.
    pub(super) fn drop_added_rows(&mut self, index: usize, rows: &[usize]) {
        let tab = &mut self.tabs[index];
        let Some(result) = tab.result.as_mut() else {
            return;
        };
        let mut gone: Vec<usize> = rows
            .iter()
            .copied()
            .filter(|r| tab.added.contains(r))
            .collect();
        gone.sort_unstable();
        for &r in gone.iter().rev() {
            result.rows.remove(r);
            tab.added.remove(&r);
            let shift = |row: usize| if row > r { row - 1 } else { row };
            tab.added = tab.added.iter().map(|&a| shift(a)).collect();
            tab.pending = std::mem::take(&mut tab.pending)
                .into_iter()
                .filter(|((row, _), _)| *row != r)
                .map(|((row, c), v)| ((shift(row), c), v))
                .collect();
        }
        tab.pending_log.clear();
        tab.range = None;
        tab.selected_cell = None;
        tab.selected_row = None;
    }

    /// The database refused the first waiting change: put them all back.
    /// Returns whether there was anything to put back.
    pub(super) fn restore_staged(&mut self, id: Uuid) -> bool {
        let Some((pending, added, log)) = self.staged_backup.remove(&id) else {
            return false;
        };
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return false;
        };
        tab.pending = pending;
        tab.added = added;
        tab.pending_log = log;
        true
    }

    /// Changes wait in this tab: what may not happen before they are saved
    /// or left.
    pub(super) fn has_staged(&self, index: usize) -> bool {
        let tab = &self.tabs[index];
        !tab.pending.is_empty() || !tab.added.is_empty()
    }
}
