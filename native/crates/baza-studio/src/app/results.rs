//! The result of a query: the "Wyniki" heading and the grid below it, shared
//! by the SQL editor, the table data and the query builder.
use super::shortcuts::{take, take_mod};
use super::*;
use crate::widgets::Btn;
use baza_core::builder::{Filter, FilterOp, Projection, QueryBuilder};
use eframe::egui::{RichText, TextEdit};
use egui_extras::{Column, TableBuilder};

/// Left padding of the text in a grid cell.
const CELL_PAD: f32 = 13.0;

/// What the user asked for in the heading of a result.
#[derive(Default)]
pub(super) struct HeadingActions {
    pub export: bool,
    pub insert: bool,
    pub delete: bool,
}

/// What the user asked for in the bar under the rows of a table.
#[derive(Default)]
pub(super) struct FooterActions {
    pub insert: bool,
    pub delete: bool,
    /// Save the cell being edited.
    pub apply: bool,
    /// Leave the cell being edited unchanged.
    pub discard: bool,
    pub refresh: bool,
    pub stop: bool,
    /// Another page, counted from 0.
    pub page: Option<u64>,
    /// The last page: the records are counted first.
    pub last: bool,
    /// Records per page for this table; 0 means all of them.
    pub page_rows: Option<u32>,
    /// The records per page of this table become the default.
    pub default_rows: bool,
    pub form_view: Option<bool>,
}

/// The choices of records per page: 0 stands for all of them.
pub(super) const PAGE_ROWS_CHOICES: [u32; 6] = [100, 500, 1000, 5000, 10_000, 0];

/// A choice of records per page as the menus name it.
pub(super) fn page_rows_label(rows: u32, en: bool) -> String {
    if rows == 0 {
        let most = group_digits(u64::from(baza_core::model::MAX_PAGE_ROWS), en);
        return if en {
            format!("All (up to {most})")
        } else {
            format!("Wszystkie (do {most})")
        };
    }
    group_digits(u64::from(rows), en)
}

/// Digits in groups of three: "10 000" in Polish, "10,000" in English.
pub(super) fn group_digits(value: u64, en: bool) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(if en { ',' } else { '\u{a0}' });
        }
        out.push(ch);
    }
    out
}

impl StudioApp {
    /// Heading row of a result: the title, the row count, the export format
    /// and the export action. Without a footer (see [`Self::results_footer`])
    /// it also offers adding and removing a record.
    pub(super) fn results_heading(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        footer: bool,
    ) -> HeadingActions {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let mut actions = HeadingActions::default();
        let tab = &self.tabs[index];
        let summary = tab.result.as_ref().map(|r| {
            let mut text = format!(
                "{} {} · {} ms",
                r.rows.len(),
                row_word(r.rows.len(), en),
                r.duration_ms
            );
            // Only a statement without a result set reports changed rows.
            if r.affected > 0 && r.columns.is_empty() {
                text.push_str(&format!(
                    " · {} {}",
                    r.affected,
                    l("zmienionych", "affected")
                ));
            }
            if r.truncated {
                text.push_str(l(" · limit wierszy", " · row limit"));
            }
            text
        });
        let has_result = tab.result.is_some();
        let busy = tab.busy;
        let caps = self.capabilities(tab.connection);
        let editable = tab.table.is_some() && has_result;
        // A record can only be found again by its key: without one, the grid
        // offers insert but not delete.
        let keyed = tab.columns.iter().any(|c| c.primary_key);
        let row_selected = !self.marked_rows(index).is_empty();
        widgets::hline(ui, &p);
        egui::Frame::NONE
            .fill(p.side)
            .inner_margin(egui::Margin::symmetric(14, 0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_height(p.control_height() + 4.0);
                    ui.spacing_mut().item_spacing.x = 10.0;
                    ui.add(
                        egui::Label::new(
                            RichText::new(l("Wyniki", "Results"))
                                .size(p.small() + 0.5)
                                .color(p.text),
                        )
                        .selectable(false),
                    );
                    if busy {
                        ui.add(egui::Spinner::new().size(12.0).color(p.muted));
                    }
                    if !footer && editable && (caps.insert_rows || caps.delete_rows) {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        ui.add_space(8.0);
                        let record = record_words(caps.browse, en);
                        if caps.insert_rows {
                            actions.insert = Btn::quiet(&record.noun)
                                .icon(Icon::Plus)
                                .small()
                                .tip(&record.insert)
                                .show(ui, &p)
                                .clicked();
                        }
                        if caps.delete_rows && keyed {
                            actions.delete = Btn::quiet(&record.delete)
                                .icon(Icon::Trash)
                                .small()
                                .enabled(row_selected)
                                .show(ui, &p)
                                .clicked();
                        }
                    }
                    widgets::right(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        actions.export = Btn::link(l("Eksportuj wynik", "Export results"))
                            .icon(Icon::ArrowDown)
                            .small()
                            .enabled(has_result && caps.export)
                            .show(ui, &p)
                            .clicked();
                        widgets::quiet_combo(
                            ui,
                            &p,
                            "export_format",
                            RichText::new(self.export_format.label())
                                .size(p.small() + 0.5)
                                .color(p.text),
                            38.0,
                            |ui| {
                                for f in [
                                    Format::Csv,
                                    Format::Tsv,
                                    Format::Json,
                                    Format::Xml,
                                    Format::Sql,
                                ] {
                                    // INSERT statements are for a database
                                    // that reads SQL.
                                    if f == Format::Sql && caps.language != QueryLanguage::Sql {
                                        continue;
                                    }
                                    ui.selectable_value(&mut self.export_format, f, f.label());
                                }
                            },
                        )
                        .response
                        .on_hover_text(l("Format eksportu", "Export format"));
                        if let Some(summary) = summary {
                            ui.add_space(4.0);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(summary).size(p.small() + 0.5).color(p.muted),
                                )
                                .selectable(false)
                                .truncate(),
                            );
                        }
                    });
                });
            });
        widgets::hline(ui, &p);
        actions
    }

    /// The grid of a result. Returns the index of a column whose header was
    /// clicked. `editor_focused` keeps F2 for the SQL editor while it has the
    /// keyboard.
    pub(super) fn results_grid(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        editor_focused: bool,
    ) -> Option<usize> {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let striped = self.profiles.preferences.grid_striped;
        let mut inline_save = false;
        let mut inline_cancel = false;
        let mut clicked_edit = None;
        let mut sort = None;
        let caps = self.capabilities(self.tabs[index].connection);
        let dialect = self.tabs[index]
            .connection
            .and_then(|c| self.databases.get(&c))
            .map(|db| *db.dialect());
        let tab = &mut self.tabs[index];
        let Some(result) = &tab.result else {
            return None;
        };
        if result.columns.is_empty() {
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(20, 18))
                .show(ui, |ui| {
                    widgets::help(
                        ui,
                        &p,
                        l(
                            "Polecenie wykonano. Nie zwróciło wierszy.",
                            "The statement ran. It returned no rows.",
                        ),
                    );
                });
            return None;
        }
        // Hidden columns stay in the result; the grid skips them, and the
        // keyboard moves between the shown ones.
        let visible = shown_columns(result, &tab.hidden_columns);
        if visible.is_empty() {
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(20, 18))
                .show(ui, |ui| {
                    widgets::help(
                        ui,
                        &p,
                        l(
                            "Wszystkie kolumny są ukryte. Wybierz widoczne przyciskiem „Kolumny” nad wynikami.",
                            "Every column is hidden. Choose the visible ones with “Columns” above the results.",
                        ),
                    );
                });
            return None;
        }
        // Position of a column among the shown ones: a hidden one counts as
        // the next shown one.
        let position = |c: usize| {
            visible
                .iter()
                .position(|&v| v >= c)
                .unwrap_or(visible.len() - 1)
        };
        let editable =
            tab.table.is_some() && caps.edit_cells && tab.columns.iter().any(|c| c.primary_key);
        // The columns the rows are ordered by, in turn, and whether descending.
        let sorts = (!tab.order_column.is_empty())
            .then(|| (tab.order_column.clone(), tab.descending))
            .into_iter()
            .chain(tab.then_by.iter().cloned())
            .collect::<Vec<_>>();
        let line = egui::Stroke::new(1.0, p.line);
        let body_font = egui::FontId::proportional(p.body());
        let row_height = p.row_height() + 2.0;
        // The grid takes the keyboard once a cell is clicked. The arrows, Tab
        // and Esc then stay with it instead of moving the focus elsewhere.
        let grid_id = egui::Id::new(("grid_keys", tab.id));
        let anchor = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(1.0, 1.0));
        ui.interact(anchor, grid_id, egui::Sense::focusable_noninteractive());
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                grid_id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            )
        });
        let keyboard = ui.memory(|m| m.has_focus(grid_id))
            && !editor_focused
            && !ui.ctx().text_edit_focused()
            && self.cell_editor.is_none();
        let mut focus_grid = false;
        let mut scroll_row = tab.scroll_to.take();
        let mut scroll_cell = None;
        let (mut insert_key, mut delete_key, mut clear_key) = (false, false, false);
        let mut paste: Option<String> = None;
        let mut set_value = false;
        let mut set_null = false;
        let (mut undo_key, mut redo_key) = (false, false);
        let last = (
            result.rows.len().saturating_sub(1),
            result.columns.len().saturating_sub(1),
        );
        // A record asked for with Mod+G, once its page is here.
        if !tab.busy
            && let Some(row) = tab.pending_row.take()
            && row < result.rows.len()
        {
            let column = tab.selected_cell.map_or(0, |(_, c)| c).min(last.1);
            tab.selected_row = Some(row);
            tab.selected_cell = Some((row, column));
            tab.range = Some(GridRange::cell(row, column));
            scroll_row = Some(row);
            focus_grid = true;
        }
        let (total_rows_now, total_columns_now) = (result.rows.len(), result.columns.len());
        if keyboard && !result.rows.is_empty() {
            let at = tab
                .selected_cell
                .unwrap_or((tab.selected_row.unwrap_or(0), 0));
            let at = (at.0.min(last.0), at.1.min(last.1));
            let moving = tab
                .range
                .filter(|g| !g.whole_rows && !g.whole_columns)
                .map_or(at, |g| (g.focus.0.min(last.0), g.focus.1.min(last.1)));
            let screen = ((ui.available_height() / row_height) as usize)
                .saturating_sub(3)
                .max(1);
            // The keys count shown columns only.
            let last_shown = (last.0, visible.len() - 1);
            let shown = |(r, c): (usize, usize)| (r, position(c));
            let actual = |(r, at): (usize, usize)| (r, visible[at]);
            let mut target = None;
            for key in [
                egui::Key::ArrowUp,
                egui::Key::ArrowDown,
                egui::Key::ArrowLeft,
                egui::Key::ArrowRight,
                egui::Key::Home,
                egui::Key::End,
                egui::Key::PageUp,
                egui::Key::PageDown,
            ] {
                for command in [false, true] {
                    for extend in [false, true] {
                        let pressed = if command {
                            take_mod(ui, extend, key)
                        } else if extend {
                            take(ui, egui::Modifiers::SHIFT, key)
                        } else {
                            take(ui, egui::Modifiers::NONE, key)
                        };
                        let from = if extend { moving } else { at };
                        if pressed
                            && let Some(cell) =
                                grid_target(key, command, shown(from), last_shown, screen)
                        {
                            target = Some((actual(cell), extend));
                        }
                    }
                }
            }
            let (row, column) = shown(at);
            if take(ui, egui::Modifiers::NONE, egui::Key::Tab) {
                target = Some((
                    actual(if column < last_shown.1 {
                        (row, column + 1)
                    } else {
                        ((row + 1).min(last.0), 0)
                    }),
                    false,
                ));
            }
            if take(ui, egui::Modifiers::SHIFT, egui::Key::Tab) {
                target = Some((
                    actual(if column > 0 {
                        (row, column - 1)
                    } else {
                        (row.saturating_sub(1), last_shown.1)
                    }),
                    false,
                ));
            }
            if let Some((cell, extend)) = target {
                tab.range = Some(if extend {
                    let mut range = tab
                        .range
                        .filter(|g| !g.whole_rows && !g.whole_columns)
                        .unwrap_or(GridRange::cell(at.0, at.1));
                    range.focus = cell;
                    range
                } else {
                    GridRange::cell(cell.0, cell.1)
                });
                tab.selected_row = Some(cell.0);
                tab.selected_cell = Some(cell);
                scroll_row = Some(cell.0);
                scroll_cell = Some(cell);
            }
            let (r, c) = tab.selected_cell.unwrap_or(at);
            let (r, c) = (r.min(last.0), c.min(last.1));
            let cell = tab.pending.get(&(r, c)).unwrap_or(&result.rows[r][c]);
            let can_edit =
                (editable && cell.editable()) || (tab.added.contains(&r) && tab.table.is_some());
            if take(ui, egui::Modifiers::NONE, egui::Key::Enter) && can_edit {
                clicked_edit = Some(start_cell_edit(tab.id, r, c, cell));
            }
            if (take_mod(ui, false, egui::Key::Enter)
                || take(ui, egui::Modifiers::SHIFT, egui::Key::Enter))
                && can_edit
            {
                let mut editor = start_cell_edit(tab.id, r, c, cell);
                editor.inline = false;
                clicked_edit = Some(editor);
            }
            insert_key = take(ui, egui::Modifiers::NONE, egui::Key::Insert);
            delete_key =
                take_mod(ui, false, egui::Key::Delete) || take_mod(ui, false, egui::Key::Backspace);
            // Delete or Backspace alone empties the marked cells, as in a
            // spreadsheet.
            clear_key = take(ui, egui::Modifiers::NONE, egui::Key::Delete)
                || take(ui, egui::Modifiers::NONE, egui::Key::Backspace);
            // Mod+Z takes the last change back, Mod+Shift+Z (or Ctrl+Y)
            // makes it again.
            redo_key =
                take_mod(ui, true, egui::Key::Z) || take(ui, egui::Modifiers::CTRL, egui::Key::Y);
            undo_key = take_mod(ui, false, egui::Key::Z);
            // Typing starts editing the current cell; with several cells
            // marked, Enter writes the value into all of them.
            if can_edit
                && let Some(typed) = ui.input_mut(|i| {
                    let found = i.events.iter().position(|e| {
                        matches!(e, egui::Event::Text(t) if !t.is_empty() && t != " " && !t.chars().any(char::is_control))
                    })?;
                    match i.events.remove(found) {
                        egui::Event::Text(t) => Some(t),
                        _ => None,
                    }
                })
            {
                let marked = tab
                    .range
                    .and_then(|g| Some((g.row_span(total_rows_now)?, g.column_span(total_columns_now)?)))
                    .is_some_and(|((r0, r1), (c0, c1))| (r1 - r0 + 1) * (c1 - c0 + 1) > 1);
                let mut editor = start_cell_edit(tab.id, r, c, cell);
                editor.value = typed;
                editor.null = false;
                editor.inline = true;
                editor.all_marked = marked;
                // The cursor after what was typed, not over it.
                let id = egui::Id::new(("inline_edit", tab.id, r, c));
                let mut state = egui::text_edit::TextEditState::default();
                let end = editor.value.chars().count();
                state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(end))));
                state.store(ui.ctx(), id);
                clicked_edit = Some(editor);
            }
        }
        let (shift, command, primary_down, pointer) = ui.input(|i| {
            (
                i.modifiers.shift,
                i.modifiers.command,
                i.pointer.primary_down(),
                i.pointer.latest_pos(),
            )
        });
        if !primary_down {
            tab.dragging = false;
        }
        let total_rows = result.rows.len();
        let total_columns = result.columns.len();
        // The values an ENUM or a SET column takes, offered while it is edited.
        let column_choices = result
            .columns
            .iter()
            .map(|shown| {
                tab.columns
                    .iter()
                    .find(|column| column.name == shown.name)
                    .and_then(|column| sql::choices(&column.data_type))
            })
            .collect::<Vec<_>>();
        let range = tab.range;
        let marked = |r: usize, c: usize| {
            range.is_some_and(|range| range.contains(r, c, total_rows, total_columns))
        };
        let row_marked = |r: usize| {
            range.is_some_and(|range| {
                range.whole_rows
                    && range
                        .row_span(total_rows)
                        .is_some_and(|(first, last)| (first..=last).contains(&r))
            })
        };
        // Numbers count from the first record of the page on screen.
        let first_number = tab
            .page_request
            .as_ref()
            .filter(|_| tab.kind == TabKind::Table)
            .map_or(0, |request| request.offset);
        let live: Option<String> = self
            .cell_editor
            .as_ref()
            .filter(|e| e.all_marked && e.inline && e.tab == tab.id)
            .map(|e| {
                if e.null {
                    "NULL".to_owned()
                } else {
                    e.value.clone()
                }
            });
        let mut copy_as: Option<CopyAs> = None;
        let mut view_value: Option<(usize, usize)> = None;
        let mut select_all = false;
        let marked_fill = theme::blend(p.bg, p.accent, 0.20);
        // Changes waiting to be saved are tinted; their rows carry a pencil,
        // new rows a plus.
        let staged_fill = theme::blend(p.bg, theme::WARNING, 0.18);
        let staged_rows: std::collections::BTreeSet<usize> =
            tab.pending.keys().map(|(r, _)| *r).collect();
        let mut move_next = false;
        egui::ScrollArea::horizontal()
            .id_salt(("result_scroll", tab.id))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                let visuals = ui.visuals_mut();
                visuals.faint_bg_color = p.stripe;
                visuals.widgets.hovered.bg_fill = p.raised;
                visuals.selection.bg_fill = theme::blend(p.bg, p.accent, 0.16);
                let grid_height = (ui.available_height() - row_height - 6.0).max(60.0);
                let number_width = 46.0;
                let count = visible.len();
                let column_width =
                    ((ui.available_width() - number_width) / count as f32).max(120.0);
                let mut table = TableBuilder::new(ui)
                    .id_salt(("result", tab.id))
                    .striped(striped)
                    .resizable(true)
                    .sense(egui::Sense::click())
                    .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                    .min_scrolled_height(grid_height)
                    .max_scroll_height(grid_height)
                    .column(Column::exact(number_width));
                if count > 1 {
                    table = table.columns(
                        Column::initial(column_width).at_least(70.0).clip(true),
                        count - 1,
                    );
                }
                if let Some(row) = scroll_row {
                    table = table.scroll_to_row(row, None);
                }
                table
                    .column(Column::remainder().at_least(120.0).clip(true))
                    .header(row_height + 2.0, |mut header| {
                        header.col(|ui| {
                            let rect = ui.max_rect();
                            ui.painter().rect_filled(rect, 0.0, p.side);
                            ui.painter().vline(rect.right() - 0.5, rect.y_range(), line);
                            ui.painter()
                                .hline(rect.x_range(), rect.bottom() - 0.5, line);
                            ui.painter().text(
                                egui::pos2(rect.left() + CELL_PAD, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                "#",
                                egui::FontId::proportional(p.small()),
                                p.muted,
                            );
                        });
                        for &i in &visible {
                            let c = &result.columns[i];
                            let (_, response) = header.col(|ui| {
                                let rect = ui.max_rect();
                                let painter = ui.painter();
                                painter.rect_filled(rect, 0.0, p.side);
                                painter.hline(rect.x_range(), rect.bottom() - 0.5, line);
                                let place = sorts.iter().position(|(name, _)| *name == c.name);
                                let sorted = place.is_some();
                                let name = widgets::one_line(
                                    painter,
                                    &c.name,
                                    theme::medium(p.body()),
                                    if sorted { p.text } else { p.muted },
                                    rect.width() - CELL_PAD - 22.0,
                                );
                                let mut x = rect.left() + CELL_PAD;
                                let name_width = name.size().x;
                                painter.galley(
                                    egui::pos2(x, rect.center().y - name.size().y / 2.0),
                                    name,
                                    p.muted,
                                );
                                x += name_width + 5.0;
                                if let Some(place) = place {
                                    widgets::paint(
                                        painter,
                                        egui::Rect::from_center_size(
                                            egui::pos2(x + 6.0, rect.center().y),
                                            egui::vec2(12.0, 12.0),
                                        ),
                                        if sorts[place].1 {
                                            Icon::ArrowDown
                                        } else {
                                            Icon::ArrowUp
                                        },
                                        p.muted,
                                    );
                                    x += 17.0;
                                    // Which of several orders this column is.
                                    if sorts.len() > 1 {
                                        painter.text(
                                            egui::pos2(x - 3.0, rect.center().y),
                                            egui::Align2::LEFT_CENTER,
                                            (place + 1).to_string(),
                                            egui::FontId::proportional(p.caption()),
                                            p.muted,
                                        );
                                        x += 9.0;
                                    }
                                }
                                let room = rect.right() - 8.0 - x;
                                if room > 34.0 && !c.data_type.is_empty() {
                                    let kind = widgets::one_line(
                                        painter,
                                        &c.data_type.to_lowercase(),
                                        egui::FontId::proportional(p.caption()),
                                        theme::blend(p.side, p.muted, 0.62),
                                        room,
                                    );
                                    painter.galley(
                                        egui::pos2(
                                            x + 2.0,
                                            rect.center().y - kind.size().y / 2.0 + 0.5,
                                        ),
                                        kind,
                                        p.muted,
                                    );
                                }
                            });
                            if response.clicked() {
                                focus_grid = true;
                                if command {
                                    // ⌘ or Ctrl marks the whole column; with
                                    // Shift as well the mark grows to it.
                                    tab.range = Some(
                                        match tab.range.filter(|r| shift && r.whole_columns) {
                                            Some(mut range) => {
                                                range.focus = (0, i);
                                                range
                                            }
                                            None => GridRange::columns(i),
                                        },
                                    );
                                } else {
                                    sort = Some(i);
                                }
                            }
                            let mut tip = if c.data_type.is_empty() {
                                c.name.clone()
                            } else {
                                format!("{} · {}", c.name, c.data_type)
                            };
                            if caps.sort_many && !sorts.is_empty() {
                                tip.push_str(l(
                                    "\nShift+klik: sortuj też po tej kolumnie",
                                    "\nShift+click: also order by this column",
                                ));
                            }
                            response.on_hover_text(tip);
                        }
                    })
                    .body(|body| {
                        body.rows(row_height, result.rows.len(), |mut row| {
                            let r = row.index();
                            row.set_selected(tab.selected_row == Some(r) || row_marked(r));
                            let (_, number) = row.col(|ui| {
                                let rect = ui.max_rect();
                                ui.painter().vline(rect.right() - 0.5, rect.y_range(), line);
                                ui.painter()
                                    .hline(rect.x_range(), rect.bottom() - 0.5, line);
                                if tab.added.contains(&r) {
                                    widgets::paint(
                                        ui.painter(),
                                        egui::Rect::from_center_size(
                                            egui::pos2(
                                                rect.left() + CELL_PAD + 5.0,
                                                rect.center().y,
                                            ),
                                            egui::vec2(12.0, 12.0),
                                        ),
                                        Icon::Plus,
                                        theme::WARNING,
                                    );
                                } else {
                                    ui.painter().text(
                                        egui::pos2(rect.left() + CELL_PAD, rect.center().y),
                                        egui::Align2::LEFT_CENTER,
                                        (first_number + r as u64 + 1).to_string(),
                                        egui::FontId::proportional(p.small()),
                                        p.muted,
                                    );
                                }
                                if staged_rows.contains(&r) && !tab.added.contains(&r) {
                                    widgets::paint(
                                        ui.painter(),
                                        egui::Rect::from_center_size(
                                            egui::pos2(rect.right() - 9.0, rect.center().y),
                                            egui::vec2(10.0, 10.0),
                                        ),
                                        Icon::Pencil,
                                        theme::WARNING,
                                    );
                                }
                            });
                            if number.clicked() {
                                focus_grid = true;
                                // A row number marks the row; Shift marks
                                // every row up to it.
                                tab.range =
                                    Some(match tab.range.filter(|g| shift && g.whole_rows) {
                                        Some(mut range) => {
                                            range.focus = (r, 0);
                                            range
                                        }
                                        None => GridRange::rows(r),
                                    });
                                tab.selected_row = Some(r);
                                tab.selected_cell = None;
                            }
                            for &c in &visible {
                                let cell = &result.rows[r][c];
                                let waiting = tab.pending.contains_key(&(r, c));
                                let cell = tab.pending.get(&(r, c)).unwrap_or(cell);
                                let new_row = tab.added.contains(&r);
                                let editing = self.cell_editor.as_ref().is_some_and(|e| {
                                    e.inline && e.tab == tab.id && e.row == r && e.column == c
                                });
                                let selected = tab.selected_cell == Some((r, c));
                                let in_mark = marked(r, c);
                                let (_, response) = row.col(|ui| {
                                    let rect = ui.max_rect();
                                    ui.painter()
                                        .hline(rect.x_range(), rect.bottom() - 0.5, line);
                                    if let Some(editor) =
                                        self.cell_editor.as_mut().filter(|_| editing)
                                    {
                                        ui.add_space(4.0);
                                        ui.spacing_mut().item_spacing.x = 2.0;
                                        ui.spacing_mut().interact_size.y = row_height - 6.0;
                                        // A selected row tints its text; the
                                        // value being typed stays plain.
                                        ui.visuals_mut().override_text_color = Some(p.text);
                                        let field = egui::Id::new(("inline_edit", tab.id, r, c));
                                        // An ENUM or a SET column lists its values
                                        // under the cell: the arrows choose one,
                                        // Space ticks a value of a SET, Enter saves.
                                        let choices = column_choices[c].as_ref();
                                        let choice_id = field.with("choice");
                                        let mut chosen = choices.map(|(_, values)| {
                                            ui.data(|d| d.get_temp::<usize>(choice_id))
                                                .unwrap_or_else(|| {
                                                    values
                                                        .iter()
                                                        .position(|v| *v == editor.value)
                                                        .unwrap_or(0)
                                                })
                                                .min(values.len().saturating_sub(1))
                                        });
                                        let mut follow = false;
                                        if let (Some((multiple, values)), Some(at)) =
                                            (choices, chosen.as_mut())
                                            && !values.is_empty()
                                            && ui.memory(|m| m.has_focus(field))
                                        {
                                            let count = values.len();
                                            let none = egui::Modifiers::NONE;
                                            let step = if take(ui, none, egui::Key::ArrowDown) {
                                                1
                                            } else if take(ui, none, egui::Key::ArrowUp) {
                                                count - 1
                                            } else {
                                                0
                                            };
                                            if step > 0 {
                                                *at = (*at + step) % count;
                                                follow = true;
                                                if !multiple {
                                                    editor.value = values[*at].clone();
                                                }
                                            }
                                            if *multiple && take(ui, none, egui::Key::Space) {
                                                editor.value =
                                                    toggle_choice(values, &editor.value, *at);
                                            }
                                        }
                                        if let Some(at) = chosen {
                                            ui.data_mut(|d| d.insert_temp(choice_id, at));
                                        }
                                        let response = ui.add_enabled(
                                            !editor.null,
                                            TextEdit::singleline(&mut editor.value)
                                                .id(field)
                                                .desired_width(
                                                    (ui.available_width() - 34.0).max(30.0),
                                                )
                                                .margin(egui::Margin::symmetric(6, 2))
                                                .font(body_font.clone())
                                                .char_limit(sql::MAX_SQL_BYTES),
                                        );
                                        // The editor opens with the keyboard
                                        // in it, ready for typing: the focus is
                                        // asked for until the field has it. A
                                        // NULL value has no field to type in.
                                        if editor.fresh {
                                            if response.has_focus() || editor.null {
                                                editor.fresh = false;
                                            } else {
                                                response.request_focus();
                                            }
                                        }
                                        // A single-line field gives up the focus
                                        // on Enter and Escape, so both are read
                                        // in the frame the focus was lost too.
                                        if response.has_focus() || response.lost_focus() {
                                            let (enter, escape) = ui.input(|i| {
                                                (
                                                    i.key_pressed(egui::Key::Enter),
                                                    i.key_pressed(egui::Key::Escape),
                                                )
                                            });
                                            // Tab saves the cell and moves to
                                            // the next one, as Enter saves.
                                            let tab_key = ui.input(|i| {
                                                i.key_pressed(egui::Key::Tab) && !i.modifiers.shift
                                            });
                                            inline_cancel |= escape;
                                            inline_save |= (enter || tab_key) && !escape;
                                            move_next |= tab_key && !escape;
                                        }
                                        inline_save |= widgets::icon_toggle(
                                            ui,
                                            &p,
                                            Icon::Check,
                                            l(
                                                "Zapisz tylko tę komórkę (Enter)",
                                                "Save this cell only (Enter)",
                                            ),
                                            true,
                                        )
                                        .clicked();
                                        if let Some((multiple, values)) = choices {
                                            let ticked =
                                                editor.value.split(',').collect::<Vec<_>>();
                                            let list = values
                                                .iter()
                                                .map(|value| widgets::Choice {
                                                    text: value,
                                                    note: "",
                                                    color: p.text,
                                                    checked: multiple
                                                        .then(|| ticked.contains(&value.as_str())),
                                                })
                                                .collect::<Vec<_>>();
                                            if let Some(index) = widgets::choice_list(
                                                ui.ctx(),
                                                &p,
                                                choice_id.with("list"),
                                                rect.left_bottom(),
                                                &list,
                                                chosen,
                                                follow,
                                            ) {
                                                ui.data_mut(|d| d.insert_temp(choice_id, index));
                                                editor.null = false;
                                                if *multiple {
                                                    editor.value =
                                                        toggle_choice(values, &editor.value, index);
                                                    // The keyboard goes back to the cell.
                                                    response.request_focus();
                                                } else {
                                                    editor.value = values[index].clone();
                                                    inline_save = true;
                                                }
                                            }
                                        }
                                        response.context_menu(|ui| {
                                            ui.checkbox(&mut editor.null, "NULL");
                                            if ui
                                                .button(l(
                                                    "Edytor dużej wartości…",
                                                    "Large value editor…",
                                                ))
                                                .clicked()
                                            {
                                                editor.inline = false;
                                                ui.close();
                                            }
                                            if ui
                                                .button(l("Anuluj edycję", "Cancel edit"))
                                                .clicked()
                                            {
                                                inline_cancel = true;
                                                ui.close();
                                            }
                                        });
                                        return;
                                    }
                                    let painter = ui.painter();
                                    if waiting && !selected {
                                        painter.rect_filled(rect, 0.0, staged_fill);
                                    }
                                    if in_mark && !selected {
                                        painter.rect_filled(rect, 0.0, marked_fill);
                                    }
                                    if selected {
                                        painter.rect_filled(rect, 0.0, p.raised);
                                        painter.rect_stroke(
                                            rect.shrink2(egui::vec2(0.5, 1.0)),
                                            2.0,
                                            egui::Stroke::new(1.0, p.accent),
                                            egui::StrokeKind::Inside,
                                        );
                                    }
                                    // A value being typed for several marked cells
                                    // shows in all of them while it is typed.
                                    let typed = live.as_deref().filter(|_| in_mark);
                                    let null = typed.is_none() && matches!(cell, Cell::Null);
                                    let text: String = match typed {
                                        Some(value) => value.chars().take(512).collect(),
                                        // A cell of a new row nobody typed into
                                        // takes the database's default.
                                        None if new_row && !waiting => String::new(),
                                        None => cell.display().chars().take(512).collect(),
                                    };
                                    let mut job = egui::text::LayoutJob::simple_singleline(
                                        text,
                                        body_font.clone(),
                                        if null { p.muted } else { p.text },
                                    );
                                    if null {
                                        job.sections[0].format.italics = true;
                                    }
                                    job.wrap = egui::text::TextWrapping {
                                        max_width: (rect.width() - CELL_PAD - 6.0).max(8.0),
                                        max_rows: 1,
                                        break_anywhere: true,
                                        overflow_character: Some('…'),
                                    };
                                    let galley = painter.layout_job(job);
                                    painter.galley(
                                        egui::pos2(
                                            rect.left() + CELL_PAD,
                                            rect.center().y - galley.size().y / 2.0,
                                        ),
                                        galley,
                                        p.text,
                                    );
                                });
                                if editing {
                                    continue;
                                }
                                if scroll_cell == Some((r, c)) {
                                    response.scroll_to_me(None);
                                }
                                if response.clicked() {
                                    focus_grid = true;
                                    // Shift-click grows the mark to this cell.
                                    tab.range = Some(match tab.range.filter(|_| shift) {
                                        Some(mut range) => {
                                            range.focus = (r, c);
                                            range.whole_rows = false;
                                            range.whole_columns = false;
                                            range
                                        }
                                        None => GridRange::cell(r, c),
                                    });
                                    tab.selected_row = Some(r);
                                    tab.selected_cell = Some((r, c));
                                }
                                // Pressing on a cell and moving over others
                                // marks the rectangle between them.
                                if primary_down
                                    && response.is_pointer_button_down_on()
                                    && !tab.dragging
                                {
                                    tab.dragging = true;
                                    focus_grid = true;
                                    if !shift {
                                        tab.range = Some(GridRange::cell(r, c));
                                    }
                                }
                                if tab.dragging
                                    && primary_down
                                    && pointer.is_some_and(|pos| response.rect.contains(pos))
                                    && let Some(range) = tab.range.as_mut()
                                    && range.focus != (r, c)
                                    && !range.whole_rows
                                    && !range.whole_columns
                                {
                                    range.focus = (r, c);
                                    tab.selected_row = Some(r);
                                    tab.selected_cell = Some((r, c));
                                }
                                let can_edit = (editable && cell.editable())
                                    || (new_row && tab.table.is_some());
                                if response.double_clicked() && can_edit {
                                    clicked_edit = Some(start_cell_edit(tab.id, r, c, cell));
                                } else if response.double_clicked()
                                    && matches!(cell, Cell::Bytes(_))
                                {
                                    // Bytes that cannot be changed are read in hex.
                                    view_value = Some((r, c));
                                }
                                response.context_menu(|ui| {
                                    // The menu acts on the mark; a cell outside
                                    // it becomes the mark.
                                    if !in_mark {
                                        tab.range = Some(GridRange::cell(r, c));
                                        tab.selected_row = Some(r);
                                        tab.selected_cell = Some((r, c));
                                    }
                                    if ui.button(l("Kopiuj", "Copy")).clicked() {
                                        copy_as = Some(CopyAs::Text);
                                        ui.close();
                                    }
                                    if ui
                                        .button(l(
                                            "Kopiuj z nazwami kolumn",
                                            "Copy with column names",
                                        ))
                                        .clicked()
                                    {
                                        copy_as = Some(CopyAs::TextWithHeaders);
                                        ui.close();
                                    }
                                    ui.menu_button(l("Kopiuj jako", "Copy as"), |ui| {
                                        if ui.button("CSV").clicked() {
                                            copy_as = Some(CopyAs::Csv);
                                            ui.close();
                                        }
                                        if ui.button("JSON").clicked() {
                                            copy_as = Some(CopyAs::Json);
                                            ui.close();
                                        }
                                        if caps.language == QueryLanguage::Sql
                                            && (tab.table.is_some() || tab.source.is_some())
                                            && ui
                                                .button(l("Polecenia INSERT", "INSERT statements"))
                                                .clicked()
                                        {
                                            copy_as = Some(CopyAs::Insert);
                                            ui.close();
                                        }
                                    });
                                    if ui
                                        .button(l("Podgląd wartości (hex)…", "View value (hex)…"))
                                        .clicked()
                                    {
                                        view_value = Some((r, c));
                                        ui.close();
                                    }
                                    if ui.button(l("Zaznacz wszystko", "Select all")).clicked() {
                                        select_all = true;
                                        ui.close();
                                    }
                                    if editable {
                                        ui.separator();
                                        if ui
                                            .add(
                                                egui::Button::new(l("Wklej", "Paste"))
                                                    .shortcut_text(super::shortcuts::keys_text(
                                                        "Mod+V", en,
                                                    )),
                                            )
                                            .clicked()
                                        {
                                            ui.ctx().send_viewport_cmd(
                                                egui::ViewportCommand::RequestPaste,
                                            );
                                            ui.close();
                                        }
                                        if ui
                                            .button(l(
                                                "Ustaw wartość w zaznaczonych…",
                                                "Set a value in the marked cells…",
                                            ))
                                            .clicked()
                                        {
                                            set_value = true;
                                            ui.close();
                                        }
                                        if ui.button(l("Ustaw NULL", "Set to NULL")).clicked() {
                                            set_null = true;
                                            ui.close();
                                        }
                                    }
                                    ui.separator();
                                    if can_edit
                                        && ui.button(l("Edytuj komórkę", "Edit cell")).clicked()
                                    {
                                        clicked_edit = Some(start_cell_edit(tab.id, r, c, cell));
                                        ui.close();
                                    }
                                });
                                if response.hovered() && cell.display().chars().count() > 24 {
                                    response.on_hover_text(
                                        cell.display().chars().take(4096).collect::<String>(),
                                    );
                                }
                            }
                        });
                    });
            });
        if !editor_focused
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F2))
            && let Some((r, c)) = tab.selected_cell
            && editable
            && result
                .rows
                .get(r)
                .and_then(|row| row.get(c))
                .is_some_and(Cell::editable)
        {
            clicked_edit = Some(start_cell_edit(tab.id, r, c, &result.rows[r][c]));
        }
        // ⌘/Ctrl+A marks the page, ⌘/Ctrl+C copies the mark and Esc clears
        // it, while no text field has the keyboard.
        let typing = editor_focused || ui.ctx().text_edit_focused();
        if !typing {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::A)) {
                select_all = true;
            }
            // Mod+V arrives as the text on the clipboard.
            if editable {
                paste = ui.input(|i| {
                    i.events.iter().find_map(|e| match e {
                        egui::Event::Paste(text) => Some(text.clone()),
                        _ => None,
                    })
                });
            }
            // Mod+C arrives as a copy request; with Shift held it copies
            // the column names too.
            if tab.range.is_some()
                && let Some(with_names) = ui.input(|i| {
                    i.events
                        .iter()
                        .any(|e| matches!(e, egui::Event::Copy))
                        .then_some(i.modifiers.shift)
                })
            {
                copy_as = Some(if with_names {
                    CopyAs::TextWithHeaders
                } else {
                    CopyAs::Text
                });
            }
            if tab.range.is_some()
                && self.cell_editor.is_none()
                && ui.input(|i| i.key_pressed(egui::Key::Escape))
            {
                tab.range = None;
            }
        }
        if select_all {
            tab.range = Some(GridRange::all());
        }
        let copied = copy_as.and_then(|how| {
            let range = tab.range?;
            let part = marked_part(result, &range, &tab.hidden_columns)?;
            let cells = part.rows.len() * part.columns.len();
            let table = tab.table.clone().or_else(|| tab.source.clone());
            Some(
                clipboard_text(&part, how, dialect.as_ref(), table.as_ref())
                    .map(|text| (text, cells)),
            )
        });
        if let Some(copied) = copied {
            match copied {
                Ok((text, cells)) => {
                    self.copy_out(ui.ctx(), text);
                    let message = if en {
                        format!("Copied cells: {cells}.")
                    } else {
                        format!("Skopiowane komórki: {cells}.")
                    };
                    self.notify(message, false);
                }
                Err(error) => self.notify(error.to_string(), true),
            }
        }
        if let Some(edit) = clicked_edit {
            self.cell_editor = Some(edit);
        }
        if inline_save {
            if let Some(editor) = self.cell_editor.take() {
                let (r, c) = (editor.row, editor.column);
                if let Err(error) = self.commit_cell(editor) {
                    self.notify(error, true);
                }
                // Tab: on to the next cell of the row, then the next row.
                if move_next {
                    let tab = &mut self.tabs[index];
                    let (rows, visible) = tab.result.as_ref().map_or((0, Vec::new()), |result| {
                        (
                            result.rows.len(),
                            shown_columns(result, &tab.hidden_columns),
                        )
                    });
                    if rows > 0 && !visible.is_empty() {
                        let next = match visible.iter().find(|&&shown| shown > c) {
                            Some(&shown) => (r, shown),
                            None => ((r + 1).min(rows - 1), visible[0]),
                        };
                        tab.selected_cell = Some(next);
                        tab.selected_row = Some(next.0);
                        tab.range = Some(GridRange::cell(next.0, next.1));
                        tab.scroll_to = Some(next.0);
                    }
                }
            }
        } else if inline_cancel {
            self.cell_editor = None;
        }
        // After a click, and after a cell was saved or left, the keys go to
        // the grid again.
        if focus_grid || inline_save || inline_cancel {
            ui.memory_mut(|m| m.request_focus(grid_id));
        }
        let tab = &self.tabs[index];
        let has_table = tab.table.is_some() && tab.result.is_some();
        let keyed = tab.columns.iter().any(|c| c.primary_key);
        if insert_key && has_table && caps.insert_rows {
            self.begin_insert(index);
        }
        if delete_key && has_table && caps.delete_rows && keyed {
            self.ask_delete(index);
        }
        // Several cells at once, as in a spreadsheet.
        if let Some(text) = paste {
            self.paste_cells(index, &text);
        }
        if clear_key {
            self.clear_cells(index);
        }
        if set_null {
            let cells = self.marked_cells(index);
            let edits = cells.into_iter().map(|(r, c)| (r, c, None)).collect();
            self.edit_cells(index, edits);
        }
        if set_value {
            self.fill_cells = Some((self.tabs[index].id, String::new(), false));
        }
        if undo_key && self.undo_staged(index) {
            // A waiting change was taken back; nothing went to the database.
        } else if undo_key || redo_key {
            self.undo_changes(index, redo_key);
        }
        if let Some((r, c)) = view_value {
            self.view_value(self.tabs[index].id, r, c);
        }
        sort
    }

    /// Records per page of a tab: its own choice or the preferences.
    pub(super) fn page_limit(&self, index: usize) -> u32 {
        match self.tabs[index].page_rows {
            Some(0) => baza_core::model::MAX_PAGE_ROWS,
            Some(rows) => rows.min(baza_core::model::MAX_PAGE_ROWS),
            None => self.profiles.preferences.page_limit(),
        }
    }

    /// Rows of the result a delete acts on: the marked rows, or the rows of
    /// the marked cells, or the selected row. A marked column alone does not
    /// mean its rows.
    pub(super) fn marked_rows(&self, index: usize) -> Vec<usize> {
        let tab = &self.tabs[index];
        let rows = tab.result.as_ref().map_or(0, |r| r.rows.len());
        match tab.range {
            Some(range) if !(range.whole_columns && !range.whole_rows) => range
                .row_span(rows)
                .map(|(first, last)| (first..=last).collect())
                .unwrap_or_default(),
            _ => tab.selected_row.filter(|r| *r < rows).into_iter().collect(),
        }
    }

    /// The marked cells of a tab, row by row, or the selected cell.
    pub(super) fn marked_cells(&self, index: usize) -> Vec<(usize, usize)> {
        let tab = &self.tabs[index];
        let Some(result) = &tab.result else {
            return Vec::new();
        };
        let (rows, columns) = (result.rows.len(), result.columns.len());
        let shown = |c: &usize| !tab.hidden_columns.contains(&result.columns[*c].name);
        match tab.range {
            Some(range) => match (range.row_span(rows), range.column_span(columns)) {
                (Some((r0, r1)), Some((c0, c1))) => (r0..=r1)
                    .flat_map(|r| (c0..=c1).filter(shown).map(move |c| (r, c)))
                    .collect(),
                _ => Vec::new(),
            },
            None => tab
                .selected_cell
                .filter(|(r, c)| *r < rows && *c < columns)
                .into_iter()
                .collect(),
        }
    }

    /// Mod+V in the grid: a block copied from a spreadsheet goes in from the
    /// first marked cell; one value fills every marked cell.
    pub(super) fn paste_cells(&mut self, index: usize, text: &str) {
        let block = spreadsheet_rows(text);
        let cells = self.marked_cells(index);
        let Some(&(top, left)) = cells.iter().min() else {
            return;
        };
        // A block fills the shown columns from the first marked one.
        let tab = &self.tabs[index];
        let shown = tab
            .result
            .as_ref()
            .map(|result| shown_columns(result, &tab.hidden_columns))
            .unwrap_or_default();
        let start = shown.iter().position(|&c| c >= left).unwrap_or(shown.len());
        let edits = if block.len() == 1 && block[0].len() == 1 && cells.len() > 1 {
            cells
                .into_iter()
                .map(|(r, c)| (r, c, Some(block[0][0].clone())))
                .collect()
        } else {
            block
                .into_iter()
                .enumerate()
                .flat_map(|(dr, row)| {
                    let shown = &shown;
                    row.into_iter().enumerate().filter_map(move |(dc, value)| {
                        Some((top + dr, *shown.get(start + dc)?, Some(value)))
                    })
                })
                .collect()
        };
        self.edit_cells(index, edits);
    }

    /// Delete in the grid: NULL where the column allows it, else empty text.
    pub(super) fn clear_cells(&mut self, index: usize) {
        let tab = &self.tabs[index];
        let Some(result) = &tab.result else {
            return;
        };
        let nullable = |c: usize| {
            result.columns.get(c).is_some_and(|column| {
                tab.columns
                    .iter()
                    .find(|i| i.name == column.name)
                    .is_none_or(|i| i.nullable)
            })
        };
        let edits = self
            .marked_cells(index)
            .into_iter()
            .map(|(r, c)| {
                (
                    r,
                    c,
                    if nullable(c) {
                        None
                    } else {
                        Some(String::new())
                    },
                )
            })
            .collect();
        self.edit_cells(index, edits);
    }

    /// "Ustaw wartość w zaznaczonych": one value, or NULL, for every marked cell.
    pub(super) fn fill_window(&mut self, ctx: &egui::Context) {
        let Some((id, mut value, mut null)) = self.fill_cells.take() else {
            return;
        };
        let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let count = self.marked_cells(index).len();
        let (mut apply, mut cancel) = (false, false);
        let dialog = widgets::dialog(
            ctx,
            &p,
            "fill_cells",
            l("Ustaw wartość", "Set a value"),
            420.0,
            l("Zamknij", "Close"),
            |ui| {
                widgets::dialog_body(ui, |ui| {
                    widgets::help(
                        ui,
                        &p,
                        &if en {
                            format!("The same value goes into the {count} marked cells.")
                        } else {
                            format!("Ta sama wartość trafi do zaznaczonych komórek: {count}.")
                        },
                    );
                    let response = ui.add_enabled(
                        !null,
                        widgets::field(&p, &mut value)
                            .id(egui::Id::new("fill_cells_value"))
                            .desired_width(ui.available_width() - 18.0),
                    );
                    if !response.has_focus() && !response.lost_focus() && !null {
                        response.request_focus();
                    }
                    apply = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    widgets::checkbox(ui, &p, &mut null, "NULL");
                });
                widgets::dialog_footer(ui, &p, |ui| {
                    widgets::right(ui, |ui| {
                        apply |= Btn::primary(l("Ustaw", "Set"))
                            .enabled(count > 0)
                            .show(ui, &p)
                            .clicked();
                        cancel = Btn::new(l("Anuluj", "Cancel")).show(ui, &p).clicked();
                    });
                });
            },
        );
        if apply {
            let edits = self
                .marked_cells(index)
                .into_iter()
                .map(|(r, c)| (r, c, (!null).then(|| value.clone())))
                .collect();
            self.edit_cells(index, edits);
        } else if !cancel && !dialog.close {
            self.fill_cells = Some((id, value, null));
        }
    }

    /// Ask before deleting the marked rows of a tab.
    pub(super) fn ask_delete(&mut self, index: usize) {
        let rows = self.marked_rows(index);
        // New rows that were never saved simply go.
        let added: Vec<usize> = rows
            .iter()
            .copied()
            .filter(|r| self.tabs[index].added.contains(r))
            .collect();
        if !added.is_empty() {
            self.drop_added_rows(index, &added);
            return;
        }
        if !rows.is_empty() {
            self.delete_confirm = Some((self.tabs[index].id, rows));
        }
    }

    /// Count the records the filter of a table lets through, to show its
    /// last page. Only for databases the query builder serves.
    pub(super) fn count_records(&mut self, index: usize) {
        let tab = &self.tabs[index];
        let (Some(request), Some(connection)) = (tab.page_request.clone(), tab.connection) else {
            return;
        };
        let Some(db) = self.databases.get(&connection).cloned() else {
            self.connect(connection);
            return;
        };
        let mut count = QueryBuilder::page(&request);
        count.projections = vec![Projection {
            column: "*".into(),
            aggregate: Some("COUNT".into()),
            alias: None,
        }];
        count.order_by = None;
        count.then_by.clear();
        count.limit = 1;
        count.offset = 0;
        let text = match count.sql(db.dialect()) {
            Ok(text) => text,
            Err(error) => {
                self.notify(error.to_string(), true);
                return;
            }
        };
        let id = tab.id;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let r = db
                .query(&text, CancellationToken::new())
                .await
                .map_err(|e| e.to_string())
                .and_then(|result| {
                    result
                        .rows
                        .first()
                        .and_then(|row| row.first())
                        .and_then(|cell| cell.display().trim().parse::<u64>().ok())
                        .ok_or_else(|| "operation_unsupported".to_owned())
                });
            let _ = tx.send(Message::RecordCount(id, r)).await;
            ctx.request_repaint();
        });
    }

    /// The bar under the rows of a table: add, delete, save or
    /// leave the cell being edited, reload and stop on the left; the
    /// statement of the page in the middle; the record, the pages, the
    /// records per page and the grid or form view on the right.
    pub(super) fn results_footer(&mut self, ui: &mut egui::Ui, index: usize) -> FooterActions {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let mut actions = FooterActions::default();
        let caps = *self.capabilities(self.tabs[index].connection);
        let limit = self.page_limit(index);
        let preferred = self.profiles.preferences.page_rows;
        let marked = !self.marked_rows(index).is_empty();
        let tab = &self.tabs[index];
        let editing = self.cell_editor.as_ref().is_some_and(|e| e.tab == tab.id);
        let waiting = tab.pending.len()
            + tab
                .added
                .iter()
                .filter(|r| !tab.pending.keys().any(|(row, _)| row == *r))
                .count();
        let rows = tab.result.as_ref().map_or(0, |r| r.rows.len());
        let offset = tab
            .page_request
            .as_ref()
            .map_or(tab.page * u64::from(limit), |r| r.offset);
        let page = tab.page;
        // A page with fewer records than asked for is the last one.
        let full = tab
            .page_request
            .as_ref()
            .is_some_and(|r| rows as u64 >= u64::from(r.limit));
        let editable = tab.table.is_some() && tab.result.is_some();
        let keyed = tab.columns.iter().any(|c| c.primary_key);
        let busy = tab.busy;
        let tab_id = tab.id;
        let form_view = tab.form_view;
        let chosen_rows = tab.page_rows.unwrap_or(preferred);
        let statement = tab.sql.split_whitespace().collect::<Vec<_>>().join(" ");
        let record = tab
            .selected_row
            .filter(|r| *r < rows)
            .map(|r| offset + r as u64 + 1);
        let total = tab.total;
        let words = record_words(caps.browse, en);
        let status = {
            let page_text = match total {
                Some(total) if limit > 0 => {
                    let pages = total.div_ceil(u64::from(limit)).max(1);
                    if en {
                        format!("page {} of {pages}", page + 1)
                    } else {
                        format!("strona {} z {pages}", page + 1)
                    }
                }
                _ if en => format!("page {}", page + 1),
                _ => format!("strona {}", page + 1),
            };
            let last_on_page = offset + rows as u64;
            match record {
                Some(n) if en => format!("Record {n} of {last_on_page} · {page_text}"),
                Some(n) => format!("Rekord {n} z {last_on_page} · {page_text}"),
                None => format!(
                    "{} {} · {page_text}",
                    group_digits(rows as u64, en),
                    row_word(rows, en)
                ),
            }
        };
        let can_count = caps.query_builder && caps.language == QueryLanguage::Sql;
        let disabled = |ui: &mut egui::Ui, enabled: bool, icon: Icon, tip: &str| {
            ui.add_enabled_ui(enabled, |ui| widgets::icon_button(ui, &p, icon, tip))
                .inner
                .clicked()
        };
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 1.0;
            actions.insert = disabled(
                ui,
                editable && caps.insert_rows && !busy,
                Icon::Plus,
                &words.insert,
            );
            actions.delete = disabled(
                ui,
                editable && caps.delete_rows && keyed && marked && !busy,
                Icon::Minus,
                &words.delete,
            );
            actions.apply = disabled(
                ui,
                editing || waiting > 0,
                Icon::Check,
                &format!(
                    "{} ({})",
                    l("Zapisz zmiany w bazie", "Save the changes to the database"),
                    super::shortcuts::keys_text("Mod+S", en)
                ),
            );
            actions.discard = disabled(
                ui,
                editing || waiting > 0,
                Icon::Close,
                l("Porzuć zmiany", "Discard the changes"),
            );
            actions.refresh = disabled(ui, !busy, Icon::Refresh, l("Odśwież dane", "Reload data"));
            actions.stop = disabled(
                ui,
                busy,
                Icon::Stop,
                l("Zatrzymaj wczytywanie", "Stop loading"),
            );
            ui.add_space(10.0);
            widgets::right(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 1.0;
                if widgets::icon_toggle(
                    ui,
                    &p,
                    Icon::Form,
                    l("Widok formularza: jeden rekord", "Form view: one record"),
                    form_view,
                )
                .clicked()
                {
                    actions.form_view = Some(true);
                }
                if widgets::icon_toggle(
                    ui,
                    &p,
                    Icon::Table,
                    l("Widok siatki", "Grid view"),
                    !form_view,
                )
                .clicked()
                {
                    actions.form_view = Some(false);
                }
                ui.add_space(8.0);
                let settings = widgets::icon_button(
                    ui,
                    &p,
                    Icon::Settings,
                    l("Rekordy na stronie", "Records per page"),
                );
                egui::Popup::menu(&settings).show(|ui| {
                    ui.label(
                        RichText::new(l("Rekordy na stronie", "Records per page"))
                            .size(p.small())
                            .color(p.muted),
                    );
                    for choice in PAGE_ROWS_CHOICES {
                        if ui
                            .selectable_label(chosen_rows == choice, page_rows_label(choice, en))
                            .clicked()
                        {
                            actions.page_rows = Some(choice);
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui
                        .add_enabled(
                            chosen_rows != preferred,
                            egui::Button::new(l(
                                "Ustaw jako domyślne dla wszystkich tabel",
                                "Make it the default for every table",
                            )),
                        )
                        .clicked()
                    {
                        actions.default_rows = true;
                        ui.close();
                    }
                });
                ui.add_space(4.0);
                let more = full && !busy;
                if disabled(
                    ui,
                    more && can_count,
                    Icon::Last,
                    if can_count {
                        l("Ostatnia strona", "Last page")
                    } else {
                        l(
                            "Ostatnia strona: ta baza nie liczy rekordów",
                            "Last page: this database does not count records",
                        )
                    },
                ) {
                    actions.last = true;
                }
                if disabled(
                    ui,
                    more,
                    Icon::ChevronRight,
                    l("Następna strona", "Next page"),
                ) {
                    actions.page = Some(page + 1);
                }
                // The page number can be typed; Enter goes there.
                let field = egui::Id::new(("page_field", tab_id));
                let mut text = ui
                    .data(|d| d.get_temp::<String>(field))
                    .unwrap_or_else(|| (page + 1).to_string());
                let response = ui.add_enabled(
                    !busy,
                    egui::TextEdit::singleline(&mut text)
                        .id(field.with("edit"))
                        .desired_width(38.0)
                        .horizontal_align(egui::Align::Center)
                        .margin(egui::Margin::symmetric(4, 3))
                        .font(egui::FontId::proportional(p.small() + 0.5)),
                );
                if response.has_focus() {
                    text.retain(|c| c.is_ascii_digit());
                    text.truncate(9);
                    ui.data_mut(|d| d.insert_temp(field, text));
                } else {
                    if response.lost_focus()
                        && let Ok(n) = text.trim().parse::<u64>()
                        && n >= 1
                        && n - 1 != page
                    {
                        actions.page = Some(n - 1);
                    }
                    ui.data_mut(|d| d.remove::<String>(field));
                }
                response.on_hover_text(l("Numer strony", "Page number"));
                if disabled(
                    ui,
                    page > 0 && !busy,
                    Icon::ChevronLeft,
                    l("Poprzednia strona", "Previous page"),
                ) {
                    actions.page = Some(page - 1);
                }
                if disabled(
                    ui,
                    page > 0 && !busy,
                    Icon::First,
                    l("Pierwsza strona", "First page"),
                ) {
                    actions.page = Some(0);
                }
                ui.add_space(10.0);
                if waiting > 0 {
                    let text = if en {
                        format!("Changes to save: {waiting}")
                    } else {
                        format!("Zmiany do zapisania: {waiting}")
                    };
                    ui.add(
                        egui::Label::new(RichText::new(text).size(p.small()).color(theme::WARNING))
                            .selectable(false),
                    );
                    ui.add_space(10.0);
                }
                ui.add(
                    egui::Label::new(RichText::new(status).size(p.small()).color(p.muted))
                        .selectable(false),
                );
                ui.add_space(10.0);
                // The statement of the page fills what is left, from the left.
                let room = ui.available_width();
                if room > 40.0 {
                    ui.allocate_ui_with_layout(
                        egui::vec2(room, ui.available_height()),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&statement)
                                        .monospace()
                                        .size(p.small() - 0.5)
                                        .color(p.muted),
                                )
                                .truncate(),
                            )
                            .on_hover_text(&statement);
                        },
                    );
                }
            });
        });
        actions
    }

    /// One record as a form: the name of each column and its value. The
    /// record is the selected row, or the first one.
    pub(super) fn record_form(&mut self, ui: &mut egui::Ui, index: usize) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let caps = *self.capabilities(self.tabs[index].connection);
        let tab = &mut self.tabs[index];
        let Some(result) = &tab.result else {
            return;
        };
        let rows = result.rows.len();
        if rows == 0 || result.columns.is_empty() {
            super::sql_view::hint(
                ui,
                &p,
                l(
                    "Na tej stronie nie ma rekordów.",
                    "There are no records on this page.",
                ),
            );
            return;
        }
        let editable =
            tab.table.is_some() && caps.edit_cells && tab.columns.iter().any(|c| c.primary_key);
        let r = tab.selected_row.unwrap_or(0).min(rows - 1);
        let offset = tab
            .page_request
            .as_ref()
            .map_or(0, |request| request.offset);
        let mut go_to = None;
        let mut edit = None;
        egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(16, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    if Btn::quiet(l("Poprzedni", "Previous"))
                        .icon(Icon::ChevronLeft)
                        .small()
                        .enabled(r > 0)
                        .show(ui, &p)
                        .clicked()
                    {
                        go_to = Some(r - 1);
                    }
                    if Btn::quiet(l("Następny", "Next"))
                        .icon(Icon::ChevronRight)
                        .small()
                        .enabled(r + 1 < rows)
                        .show(ui, &p)
                        .clicked()
                    {
                        go_to = Some(r + 1);
                    }
                    ui.add_space(8.0);
                    let text = if en {
                        format!("Record {}", offset + r as u64 + 1)
                    } else {
                        format!("Rekord {}", offset + r as u64 + 1)
                    };
                    ui.label(RichText::new(text).size(p.small() + 0.5).color(p.muted));
                });
            });
        widgets::hline(ui, &p);
        egui::ScrollArea::vertical()
            .id_salt(("record_form", tab.id))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(16, 12))
                    .show(ui, |ui| {
                        egui::Grid::new(("record_form_grid", tab.id))
                            .num_columns(2)
                            .spacing(egui::vec2(18.0, 8.0))
                            .min_col_width(120.0)
                            .show(ui, |ui| {
                                for (c, column) in result.columns.iter().enumerate() {
                                    if tab.hidden_columns.contains(&column.name) {
                                        continue;
                                    }
                                    let cell = &result.rows[r][c];
                                    ui.label(
                                        RichText::new(&column.name)
                                            .font(theme::medium(p.body()))
                                            .color(p.text),
                                    )
                                    .on_hover_text(&column.data_type);
                                    let null = matches!(cell, Cell::Null);
                                    let mut text = RichText::new(
                                        cell.display().chars().take(4096).collect::<String>(),
                                    )
                                    .color(if null { p.muted } else { p.text });
                                    if null {
                                        text = text.italics();
                                    }
                                    let response = ui.add(
                                        egui::Label::new(text).wrap().sense(egui::Sense::click()),
                                    );
                                    let can_edit = editable && cell.editable();
                                    if response.double_clicked() && can_edit {
                                        edit = Some(c);
                                    }
                                    response.context_menu(|ui| {
                                        if ui.button(l("Kopiuj wartość", "Copy value")).clicked()
                                        {
                                            ui.ctx().copy_text(cell.display());
                                            ui.close();
                                        }
                                        if can_edit
                                            && ui
                                                .button(l("Edytuj wartość…", "Edit value…"))
                                                .clicked()
                                        {
                                            edit = Some(c);
                                            ui.close();
                                        }
                                    });
                                    ui.end_row();
                                }
                            });
                    });
            });
        if let Some(row) = go_to {
            tab.selected_row = Some(row);
            tab.selected_cell = None;
            tab.range = Some(GridRange::rows(row));
        }
        if let Some(c) = edit {
            // The form edits in the window editor: the grid is not on screen.
            let mut editor = start_cell_edit(tab.id, r, c, &result.rows[r][c]);
            editor.inline = false;
            tab.selected_row = Some(r);
            tab.selected_cell = Some((r, c));
            self.cell_editor = Some(editor);
        }
    }

    /// Sort a browsable result by the column with this index: the same column
    /// again reverses the order. With Shift the column is one more to order
    /// by: again the other way, and a third time it is left out.
    pub(super) fn sort_result(&mut self, index: usize, column: usize) {
        let caps = *self.capabilities(self.tabs[index].connection);
        if !caps.sort {
            return;
        }
        let shift = self.ctx.input(|i| i.modifiers.shift);
        let tab = &mut self.tabs[index];
        let Some(name) = tab
            .result
            .as_ref()
            .and_then(|result| result.columns.get(column))
            .map(|c| c.name.clone())
        else {
            return;
        };
        if tab.source.is_none() && tab.table.is_none() {
            return;
        }
        if shift && caps.sort_many && !tab.order_column.is_empty() && tab.order_column != name {
            match tab.then_by.iter().position(|(sorted, _)| *sorted == name) {
                Some(at) if !tab.then_by[at].1 => tab.then_by[at].1 = true,
                Some(at) => {
                    tab.then_by.remove(at);
                }
                None => tab.then_by.push((name, false)),
            }
        } else {
            if tab.order_column != name {
                tab.then_by.clear();
            }
            tab.descending = tab.order_column == name && !tab.descending;
            tab.order_column = name;
        }
        self.browse(index);
    }

    /// Load the rows of the tab's table again with its filter, order and page.
    pub(super) fn browse(&mut self, index: usize) {
        if self.has_staged(index) {
            let text = self.tr(
                // The font has no ✓ and ✕: the buttons are painted icons.
                "Najpierw zapisz zmiany (Ctrl+S) albo je porzuć.",
                "Save the changes first (Ctrl+S) or discard them.",
            );
            self.notify(text.into(), true);
            return;
        }
        let limit = self.page_limit(index);
        let tab = &mut self.tabs[index];
        let Some(table) = tab.source.clone().or_else(|| tab.table.clone()) else {
            return;
        };
        let Some(connection) = tab.connection else {
            return;
        };
        let Some(db) = self.databases.get(&connection).cloned() else {
            self.connect(connection);
            return;
        };
        let mut b = PageRequest::first(table);
        b.limit = limit;
        b.offset = tab.page * u64::from(limit);
        if tab.filter_sql {
            // Checked where the driver writes it (`sql::typed_condition`).
            b.condition = Some(tab.filter_value.trim().to_owned()).filter(|c| !c.is_empty());
        } else if !tab.filter_value.is_empty() {
            if tab.filter_column.is_empty() {
                // Without a chosen column, search the first one.
                if let Some(first) = tab.result.as_ref().and_then(|r| r.columns.first()) {
                    tab.filter_column = first.name.clone();
                }
            }
            b.filters.push(Filter {
                column: tab.filter_column.clone(),
                op: FilterOp::Contains,
                value: Cell::Text(tab.filter_value.clone()),
            });
        }
        if !tab.order_column.is_empty() {
            b.order_by = Some((tab.order_column.clone(), tab.descending));
            b.then_by = tab.then_by.clone();
        }
        match db.page_text(&b) {
            Ok(s) => {
                tab.sql = s;
                tab.page_request = Some(b);
                let id = tab.id;
                self.run_tab(id, false);
            }
            Err(e) => tab.error = Some(e.to_string()),
        }
    }

    /// Error of the tab's last query, as a sentence.
    pub(super) fn tab_error(&mut self, ui: &mut egui::Ui, index: usize) {
        let Some(error) = self.tabs[index].error.clone() else {
            return;
        };
        let p = self.palette();
        let text = self.explain(error);
        egui::Frame::NONE
            .inner_margin(egui::Margin {
                left: 16,
                right: 16,
                top: 0,
                bottom: 10,
            })
            .show(ui, |ui| widgets::feedback(ui, &p, &text, true));
    }

    /// Start the "insert row" dialog for the editable table of a tab.
    pub(super) fn begin_insert(&mut self, index: usize) {
        let tables = self.capabilities(self.tabs[index].connection).browse
            == baza_core::engine::BrowseModel::Tables;
        // A table gets the new row in the grid; documents and keys keep the
        // window, which asks for the key.
        if tables && self.tabs[index].table.is_some() && self.tabs[index].result.is_some() {
            self.add_row(index);
            return;
        }
        let tab = &self.tabs[index];
        self.new_row = Some(NewRowEditor {
            tab: tab.id,
            values: tab
                .columns
                .iter()
                .map(|c| {
                    (
                        c.name.clone(),
                        String::new(),
                        c.nullable,
                        c.default.is_some() || (tables && generated_key(c)),
                    )
                })
                .collect(),
        });
    }
}

/// A key the database most likely fills in itself: an integer primary key of a
/// relational table (auto-increment, identity, rowid). The name of a key of a
/// document or key-value store is always typed by the user.
pub(super) fn generated_key(column: &baza_core::ColumnInfo) -> bool {
    let kind = column.data_type.to_ascii_lowercase();
    column.primary_key
        && (kind.contains("serial")
            || kind == "int"
            || kind.starts_with("int")
            || kind.ends_with("int")
            || kind.contains("integer")
            || kind.contains("identity"))
}

/// Words for one record of the engine on screen: a row, a document or a key.
pub(super) struct RecordWords {
    pub noun: String,
    pub insert: String,
    pub delete: String,
    pub delete_question: String,
    pub delete_help: String,
}

pub(super) fn record_words(browse: baza_core::engine::BrowseModel, en: bool) -> RecordWords {
    let (pl, english) = browse.record();
    let capital = |word: &str| {
        let mut chars = word.chars();
        chars
            .next()
            .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
            .unwrap_or_default()
    };
    if en {
        RecordWords {
            noun: capital(english),
            insert: format!("Insert {english}"),
            delete: format!("Delete {english}"),
            delete_question: format!("Delete {english}?"),
            delete_help: format!(
                "One {english}, identified by its key, is removed. This cannot be undone."
            ),
        }
    } else {
        let (accusative, help) = match pl {
            "dokument" => (
                "dokument",
                "Zostanie usunięty jeden dokument, wskazany kluczem.",
            ),
            "klucz" => ("klucz", "Zostanie usunięty jeden klucz razem z wartością."),
            _ => (
                "wiersz",
                "Z tabeli zostanie usunięty jeden wiersz, wskazany kluczem głównym.",
            ),
        };
        RecordWords {
            noun: capital(pl),
            insert: format!("Dodaj {accusative}"),
            delete: format!("Usuń {accusative}"),
            delete_question: format!("Usunąć {accusative}?"),
            delete_help: format!("{help} Tej operacji nie można cofnąć."),
        }
    }
}

/// The question and the help before deleting several records at once.
pub(super) fn delete_many_words(
    browse: baza_core::engine::BrowseModel,
    count: usize,
    en: bool,
) -> (String, String) {
    use baza_core::engine::BrowseModel;
    if en {
        let noun = match browse {
            BrowseModel::Tables => "rows",
            BrowseModel::Documents => "documents",
            BrowseModel::Keys => "keys",
        };
        return (
            format!("Delete {count} {noun}?"),
            format!(
                "The {count} marked {noun} are removed one by one, each identified by its key. This cannot be undone."
            ),
        );
    }
    let few = (2..=4).contains(&(count % 10)) && !(12..=14).contains(&(count % 100));
    let noun = match (browse, few) {
        (BrowseModel::Tables, true) => "wiersze",
        (BrowseModel::Tables, false) => "wierszy",
        (BrowseModel::Documents, true) => "dokumenty",
        (BrowseModel::Documents, false) => "dokumentów",
        (BrowseModel::Keys, true) => "klucze",
        (BrowseModel::Keys, false) => "kluczy",
    };
    (
        format!("Usunąć {count} {noun}?"),
        format!(
            "Zaznaczone {noun} ({count}) zostaną usunięte jeden po drugim, każdy wskazany kluczem. Tej operacji nie można cofnąć."
        ),
    )
}

/// The editor for one cell. Short single-line values are edited in the grid;
/// JSON documents, multi-line text and long values open the larger editor,
/// with JSON laid out over several lines.
/// Indexes of the columns of a result the grid shows: all but the hidden.
pub(super) fn shown_columns(
    result: &QueryResult,
    hidden: &std::collections::BTreeSet<String>,
) -> Vec<usize> {
    (0..result.columns.len())
        .filter(|&c| !hidden.contains(&result.columns[c].name))
        .collect()
}

/// The value of a SET column with `values[index]` ticked or no longer
/// ticked, its values in the order of the type.
pub(super) fn toggle_choice(values: &[String], current: &str, index: usize) -> String {
    let ticked = current.split(',').collect::<Vec<_>>();
    values
        .iter()
        .enumerate()
        .filter(|(i, value)| ticked.contains(&value.as_str()) != (*i == index))
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

pub(super) fn start_cell_edit(tab: Uuid, row: usize, column: usize, cell: &Cell) -> CellEditor {
    let value = cell.display();
    let json = matches!(cell, Cell::Json(_));
    let inline = !json && !value.contains('\n') && value.chars().count() <= 80;
    CellEditor {
        all_marked: false,
        inline,
        fresh: true,
        tab,
        row,
        column,
        value: if json { pretty_json(&value) } else { value },
        null: matches!(cell, Cell::Null),
    }
}

/// Indent JSON text without parsing it into a map, so the order of fields and
/// the exact digits of numbers stay as the database sent them. Text that is
/// not JSON is returned unchanged.
pub(super) fn pretty_json(text: &str) -> String {
    let trimmed = text.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(trimmed.len() * 2);
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = trimmed.chars().peekable();
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
    };
    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            '{' | '[' => {
                out.push(ch);
                // An empty object or array stays on one line.
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
                if matches!(chars.peek(), Some('}') | Some(']')) {
                    out.push(chars.next().unwrap_or(' '));
                } else {
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(ch);
            }
            ',' => {
                out.push(ch);
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    if in_string || depth != 0 {
        return text.to_owned();
    }
    out
}

#[cfg(test)]
mod choice_tests {
    use super::toggle_choice;

    #[test]
    fn a_set_value_keeps_the_order_of_the_type() {
        let values = ["read", "write", "admin"].map(String::from);
        assert_eq!(toggle_choice(&values, "", 1), "write");
        assert_eq!(toggle_choice(&values, "write", 0), "read,write");
        assert_eq!(toggle_choice(&values, "admin,read", 1), "read,write,admin");
        assert_eq!(toggle_choice(&values, "read,write", 0), "write");
    }
}

#[cfg(test)]
mod pretty_json_tests {
    use super::pretty_json;

    #[test]
    fn keeps_order_digits_and_strings() {
        let text = r#"{"z":1,"a":[1.50,{"b":"x, y: {z}"}],"e":{},"n":12345678901234567890}"#;
        let pretty = pretty_json(text);
        assert_eq!(
            pretty,
            "{\n  \"z\": 1,\n  \"a\": [\n    1.50,\n    {\n      \"b\": \"x, y: {z}\"\n    }\n  ],\n  \"e\": {},\n  \"n\": 12345678901234567890\n}"
        );
        let compact: String = pretty.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(compact, text.replace("x, y: {z}", "x,y:{z}"));
    }

    #[test]
    fn leaves_other_text_alone() {
        assert_eq!(pretty_json("plain text"), "plain text");
        assert_eq!(pretty_json("{unterminated"), "{unterminated");
        assert_eq!(pretty_json(r#"{"a":"\"}"#), r#"{"a":"\"}"#);
    }
}

/// Cells marked in the grid: a rectangle from the cell where marking started
/// to the cell it reaches now, or whole rows, or whole columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct GridRange {
    pub anchor: (usize, usize),
    pub focus: (usize, usize),
    pub whole_rows: bool,
    pub whole_columns: bool,
}
impl GridRange {
    pub fn cell(row: usize, column: usize) -> Self {
        Self {
            anchor: (row, column),
            focus: (row, column),
            whole_rows: false,
            whole_columns: false,
        }
    }
    pub fn rows(row: usize) -> Self {
        Self {
            whole_rows: true,
            ..Self::cell(row, 0)
        }
    }
    pub fn columns(column: usize) -> Self {
        Self {
            whole_columns: true,
            ..Self::cell(0, column)
        }
    }
    pub fn all() -> Self {
        Self {
            whole_rows: true,
            whole_columns: true,
            ..Self::cell(0, 0)
        }
    }
    /// First and last marked row, inclusive, of a result with `rows` rows.
    pub fn row_span(&self, rows: usize) -> Option<(usize, usize)> {
        let last = rows.checked_sub(1)?;
        if self.whole_columns {
            return Some((0, last));
        }
        let (a, b) = (self.anchor.0, self.focus.0);
        Some((a.min(b).min(last), a.max(b).min(last)))
    }
    /// First and last marked column, inclusive, of a result with `columns` columns.
    pub fn column_span(&self, columns: usize) -> Option<(usize, usize)> {
        let last = columns.checked_sub(1)?;
        if self.whole_rows {
            return Some((0, last));
        }
        let (a, b) = (self.anchor.1, self.focus.1);
        Some((a.min(b).min(last), a.max(b).min(last)))
    }
    pub fn contains(&self, row: usize, column: usize, rows: usize, columns: usize) -> bool {
        matches!(
            (self.row_span(rows), self.column_span(columns)),
            (Some((r0, r1)), Some((c0, c1)))
                if (r0..=r1).contains(&row) && (c0..=c1).contains(&column)
        )
    }
}

/// How marked cells are copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CopyAs {
    /// Values separated by tabs and lines, ready to paste into a spreadsheet.
    Text,
    TextWithHeaders,
    Csv,
    Json,
    Insert,
}

/// The marked part of a result, as a result of its own.
pub(super) fn marked_part(
    result: &QueryResult,
    range: &GridRange,
    hidden: &std::collections::BTreeSet<String>,
) -> Option<QueryResult> {
    let (r0, r1) = range.row_span(result.rows.len())?;
    let (c0, c1) = range.column_span(result.columns.len())?;
    // A hidden column is not copied with the mark around it.
    let columns = (c0..=c1)
        .filter(|&c| !hidden.contains(&result.columns[c].name))
        .collect::<Vec<_>>();
    Some(QueryResult {
        columns: columns.iter().map(|&c| result.columns[c].clone()).collect(),
        rows: result.rows[r0..=r1]
            .iter()
            .map(|row| columns.iter().map(|&c| row[c].clone()).collect())
            .collect(),
        affected: 0,
        truncated: false,
        duration_ms: 0,
    })
}

/// Clipboard text of a marked part. CSV, JSON and INSERT statements use the
/// export writers; INSERT needs the table and the dialect of its database.
pub(super) fn clipboard_text(
    part: &QueryResult,
    how: CopyAs,
    dialect: Option<&Dialect>,
    table: Option<&TableRef>,
) -> anyhow::Result<String> {
    let format = match how {
        CopyAs::Text | CopyAs::TextWithHeaders => {
            let mut lines = Vec::with_capacity(part.rows.len() + 1);
            if how == CopyAs::TextWithHeaders {
                lines.push(
                    part.columns
                        .iter()
                        .map(|c| spreadsheet_field(&c.name))
                        .collect::<Vec<_>>()
                        .join("\t"),
                );
            }
            for row in &part.rows {
                lines.push(
                    row.iter()
                        .map(|cell| match cell {
                            Cell::Null => String::new(),
                            other => spreadsheet_field(&other.display()),
                        })
                        .collect::<Vec<_>>()
                        .join("\t"),
                );
            }
            return Ok(lines.join("\n"));
        }
        CopyAs::Csv => Format::Csv,
        CopyAs::Json => Format::Json,
        CopyAs::Insert => Format::Sql,
    };
    let dialect = dialect.copied().unwrap_or(Dialect::ANSI);
    let bytes = crate::transfer::export(part, format, &dialect, table)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// A value as a spreadsheet reads it from pasted text: quoted when it holds a
/// tab, a line break or a quote.
fn spreadsheet_field(value: &str) -> String {
    if value.contains(['\t', '\n', '\r', '"']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod grid_range_tests {
    use super::*;

    fn result(rows: usize, columns: usize) -> QueryResult {
        QueryResult {
            columns: (0..columns)
                .map(|c| baza_core::ResultColumn {
                    name: format!("c{c}"),
                    data_type: "text".into(),
                })
                .collect(),
            rows: (0..rows)
                .map(|r| {
                    (0..columns)
                        .map(|c| {
                            if (r, c) == (1, 1) {
                                Cell::Null
                            } else {
                                Cell::Text(format!("{r}:{c}"))
                            }
                        })
                        .collect()
                })
                .collect(),
            affected: 0,
            truncated: false,
            duration_ms: 0,
        }
    }

    #[test]
    fn a_rectangle_rows_and_columns_are_marked_either_way_round() {
        let mut range = GridRange::cell(3, 2);
        range.focus = (1, 0);
        assert_eq!(range.row_span(10), Some((1, 3)));
        assert_eq!(range.column_span(4), Some((0, 2)));
        assert!(range.contains(2, 1, 10, 4) && !range.contains(4, 1, 10, 4));
        assert_eq!(GridRange::rows(5).column_span(4), Some((0, 3)));
        assert_eq!(GridRange::columns(2).row_span(10), Some((0, 9)));
        assert!(GridRange::all().contains(9, 3, 10, 4));
        assert_eq!(GridRange::all().row_span(0), None);
    }

    #[test]
    fn marked_cells_are_copied_as_spreadsheet_text() {
        let data = result(3, 3);
        let mut range = GridRange::cell(0, 0);
        range.focus = (1, 1);
        let part = marked_part(&data, &range, &Default::default()).unwrap();
        assert_eq!(
            clipboard_text(&part, CopyAs::Text, None, None).unwrap(),
            "0:0\t0:1\n1:0\t"
        );
        assert_eq!(
            clipboard_text(&part, CopyAs::TextWithHeaders, None, None).unwrap(),
            "c0\tc1\n0:0\t0:1\n1:0\t"
        );
        assert_eq!(spreadsheet_field("a\tb \"c\""), "\"a\tb \"\"c\"\"\"");
    }

    #[test]
    fn marked_rows_become_insert_statements() {
        let data = result(2, 2);
        let part = marked_part(&data, &GridRange::rows(0), &Default::default()).unwrap();
        let table = TableRef {
            schema: "main".into(),
            name: "t".into(),
            kind: "table".into(),
        };
        let text = clipboard_text(&part, CopyAs::Insert, None, Some(&table)).unwrap();
        assert!(text.contains("INSERT INTO"), "{text}");
        assert!(text.contains("'0:0'") && !text.contains("'1:0'"), "{text}");
    }
}

/// Where a key moves the current cell of the grid: `command` jumps to the
/// edge; `screen` is the number of rows a page key moves.
fn grid_target(
    key: egui::Key,
    command: bool,
    (r, c): (usize, usize),
    (last_r, last_c): (usize, usize),
    screen: usize,
) -> Option<(usize, usize)> {
    use egui::Key::*;
    Some(match (key, command) {
        (ArrowUp, false) => (r.saturating_sub(1), c),
        (ArrowDown, false) => ((r + 1).min(last_r), c),
        (ArrowLeft, false) => (r, c.saturating_sub(1)),
        (ArrowRight, false) => (r, (c + 1).min(last_c)),
        (ArrowLeft, true) | (Home, false) => (r, 0),
        (ArrowRight, true) | (End, false) => (r, last_c),
        (ArrowUp, true) | (PageUp, true) | (Home, true) => (0, c),
        (ArrowDown, true) | (PageDown, true) | (End, true) => (last_r, c),
        (PageUp, false) => (r.saturating_sub(screen), c),
        (PageDown, false) => ((r + screen).min(last_r), c),
        _ => return None,
    })
}

#[cfg(test)]
mod grid_key_tests {
    use super::grid_target;
    use eframe::egui::Key;

    #[test]
    fn keys_move_the_current_cell_and_stop_at_the_edges() {
        let last = (9, 4);
        assert_eq!(
            grid_target(Key::ArrowDown, false, (2, 1), last, 5),
            Some((3, 1))
        );
        assert_eq!(
            grid_target(Key::ArrowUp, false, (0, 1), last, 5),
            Some((0, 1))
        );
        assert_eq!(
            grid_target(Key::ArrowRight, false, (2, 4), last, 5),
            Some((2, 4))
        );
        assert_eq!(
            grid_target(Key::ArrowLeft, true, (2, 3), last, 5),
            Some((2, 0))
        );
        assert_eq!(grid_target(Key::End, false, (2, 1), last, 5), Some((2, 4)));
        assert_eq!(
            grid_target(Key::ArrowDown, true, (2, 1), last, 5),
            Some((9, 1))
        );
        assert_eq!(grid_target(Key::Home, true, (7, 2), last, 5), Some((0, 2)));
        assert_eq!(
            grid_target(Key::PageDown, false, (7, 2), last, 5),
            Some((9, 2))
        );
        assert_eq!(
            grid_target(Key::PageUp, false, (7, 2), last, 5),
            Some((2, 2))
        );
        assert_eq!(grid_target(Key::Enter, false, (7, 2), last, 5), None);
    }
}

/// Text pasted from a spreadsheet as rows of values: tabs between values,
/// line breaks between rows, quotes around a value that holds either.
pub(super) fn spreadsheet_rows(text: &str) -> Vec<Vec<String>> {
    let text = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(text);
    let mut rows = vec![Vec::new()];
    let mut value = String::new();
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    let mut at_start = true;
    while let Some(ch) = chars.next() {
        if quoted {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    value.push('"');
                } else {
                    quoted = false;
                }
            } else {
                value.push(ch);
            }
            continue;
        }
        match ch {
            '"' if at_start => quoted = true,
            '\t' => {
                rows.last_mut().unwrap().push(std::mem::take(&mut value));
                at_start = true;
                continue;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                rows.last_mut().unwrap().push(std::mem::take(&mut value));
                rows.push(Vec::new());
                at_start = true;
                continue;
            }
            other => value.push(other),
        }
        at_start = false;
    }
    rows.last_mut().unwrap().push(value);
    rows
}

#[cfg(test)]
mod paste_tests {
    use super::spreadsheet_rows;

    #[test]
    fn spreadsheet_text_becomes_rows_and_values() {
        assert_eq!(spreadsheet_rows("5"), vec![vec!["5".to_owned()]]);
        assert_eq!(
            spreadsheet_rows("a\tb\r\nc\td\r\n"),
            vec![
                vec!["a".to_owned(), "b".into()],
                vec!["c".into(), "d".into()]
            ]
        );
        assert_eq!(
            spreadsheet_rows("\"two\nlines\"\t\"say \"\"hi\"\"\"\t"),
            vec![vec![
                "two\nlines".to_owned(),
                "say \"hi\"".into(),
                String::new()
            ]]
        );
    }
}
