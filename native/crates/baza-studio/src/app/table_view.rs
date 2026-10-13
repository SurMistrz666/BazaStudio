//! "Dane tabeli": the rows of one table or view, with filter, order, pages and
//! single-cell editing.
use super::results::{FooterActions, page_rows_label};
use super::shortcuts::{take, take_mod};
use super::*;
use crate::widgets::Btn;

impl StudioApp {
    /// Index of the table on screen: the active tab, or the table opened last.
    fn table_tab(&mut self) -> Option<usize> {
        if self.tabs[self.active].kind != TabKind::Table {
            self.active = self
                .tabs
                .iter()
                .rposition(|tab| tab.kind == TabKind::Table)?;
        }
        Some(self.active)
    }

    pub(super) fn table_view(&mut self, ui: &mut egui::Ui) {
        let Some(index) = self.table_tab() else {
            self.table_empty(ui);
            return;
        };
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let (mut apply_filter, mut refresh, mut close) = (false, false, false);
        let caps = self.capabilities(self.tabs[index].connection);
        let tab = &mut self.tabs[index];
        let tabid = tab.id;
        let connection = tab.connection;
        let editable = tab.table.is_some();
        let tab_keyed = tab.columns.iter().any(|c| c.primary_key);
        let has_rows = tab.result.is_some();
        let columns = tab
            .result
            .as_ref()
            .map(|result| {
                result
                    .columns
                    .iter()
                    .map(|c| c.name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        widgets::toolbar(ui, &p, |ui| {
            let filter_button = Btn::new(l("Filtruj", "Filter")).icon(Icon::Filter);
            let icons = 2.0 * ((p.control_height() - 3.0).max(24.0) + 2.0);
            let fixed = filter_button.width(ui, &p) + icons + 44.0;
            let room = ui.available_width() - fixed;
            widgets::combo(
                ui,
                &p,
                ("filtercol", tab.id),
                if tab.filter_column.is_empty() {
                    l("Kolumna", "Column").to_owned()
                } else {
                    tab.filter_column.clone()
                },
                (room * 0.2).clamp(80.0, 150.0),
                |ui| {
                    for name in &columns {
                        ui.selectable_value(&mut tab.filter_column, name.clone(), name);
                    }
                },
            )
            .response
            .on_hover_text(l("Kolumna filtra", "Filter column"));
            let input =
                (ui.available_width() - filter_button.width(ui, &p) - icons - 30.0).max(60.0);
            let response = ui.add_enabled(
                caps.filter,
                widgets::field(&p, &mut tab.filter_value)
                    .id(egui::Id::new(("filter_value", tab.id)))
                    .desired_width(input - 18.0)
                    .hint_text(l("Filtruj: zawiera tekst", "Filter: contains text")),
            );
            apply_filter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            apply_filter |= filter_button.enabled(caps.filter).show(ui, &p).clicked();
            widgets::right(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                close =
                    widgets::icon_button(ui, &p, Icon::Close, l("Zamknij tabelę", "Close table"))
                        .clicked();
                refresh =
                    widgets::icon_button(ui, &p, Icon::Refresh, l("Odśwież dane", "Reload data"))
                        .clicked();
            });
        });

        // One sentence of help, and the actions that leave this view.
        let (mut import, mut builder, mut editor, mut designer) = (false, false, false, false);
        egui::Frame::NONE
            .inner_margin(egui::Margin {
                left: 16,
                right: 12,
                top: 7,
                bottom: 9,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 12.0;
                    let links = [
                        (l("Import danych", "Import data"), editable && caps.import),
                        (l("Kreator zapytań", "Query builder"), caps.query_builder),
                        (
                            match caps.language {
                                QueryLanguage::Command => l("Otwórz w konsoli", "Open in console"),
                                _ => l("Otwórz w edytorze SQL", "Open in SQL editor"),
                            },
                            caps.language != QueryLanguage::None,
                        ),
                        (l("Nowa tabela", "New table"), caps.table_designer),
                    ];
                    let links_width: f32 = links
                        .iter()
                        .map(|(text, _)| Btn::link(text).small().width(ui, &p) + 12.0)
                        .sum();
                    let keyed = tab_keyed;
                    let text = if editable && caps.edit_cells && keyed {
                        l(
                            "Dwuklik lub F2 zmienia jedną komórkę: Enter zapisuje, Esc anuluje. Nagłówek sortuje dane.",
                            "Double-click or F2 changes one cell: Enter saves, Esc cancels. A header sorts the data.",
                        )
                    } else if !caps.edit_cells {
                        l(
                            "Tylko do odczytu: ta baza nie pozwala zmieniać danych w siatce.",
                            "Read-only: this database does not let the grid change data.",
                        )
                    } else {
                        l(
                            "Tylko do odczytu: widok albo tabela bez klucza głównego. Nowy wiersz można dodać, ale istniejącego nie da się wskazać do zmiany ani usunięcia.",
                            "Read-only: a view, or a table without a primary key. New rows can be added, but an existing row cannot be singled out to change or delete.",
                        )
                    };
                    let room = ui.available_width() - links_width;
                    if room > 120.0 && has_rows {
                        let galley = widgets::one_line(
                            ui.painter(),
                            text,
                            egui::FontId::proportional(p.body() - 0.5),
                            p.muted,
                            room,
                        );
                        let (rect, response) =
                            ui.allocate_exact_size(galley.size(), egui::Sense::hover());
                        ui.painter().galley(rect.min, galley, p.muted);
                        response.on_hover_text(text);
                    }
                    widgets::right(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        let mut clicked = [false; 4];
                        for (i, (text, enabled)) in links.iter().enumerate().rev() {
                            clicked[i] = Btn::link(text)
                                .small()
                                .enabled(*enabled)
                                .show(ui, &p)
                                .clicked();
                        }
                        [import, builder, editor, designer] = clicked;
                    });
                });
            });
        let has_result = tab.result.is_some();
        let busy = tab.busy;
        let failed = tab.error.is_some();
        let source = tab.source.clone();
        let title = tab.title.clone();
        let sql = tab.sql.clone();
        let form_view = tab.form_view;
        let connected = connection.is_some_and(|id| self.databases.contains_key(&id));
        let connecting = connection.is_some_and(|id| self.connect_requests.contains_key(&id));

        // The bar under the rows stays at the bottom of the view.
        let mut footer = FooterActions::default();
        egui::Panel::bottom("table_footer")
            .exact_size(p.control_height() + 10.0)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(p.side)
                    .inner_margin(egui::Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| {
                let rect = ui.max_rect().expand2(egui::vec2(10.0, 0.0));
                ui.painter().hline(
                    rect.x_range(),
                    rect.top() + 0.5,
                    egui::Stroke::new(1.0, p.line),
                );
                footer = self.results_footer(ui, index);
            });

        self.tab_error(ui, index);
        let actions = self.results_heading(ui, index, true);
        let mut sort = None;
        let mut connect = false;
        if has_result && form_view {
            self.record_form(ui, index);
        } else if has_result {
            sort = self.results_grid(ui, index, false);
        } else if busy || connecting {
            super::sql_view::hint(
                ui,
                &p,
                l("Wczytywanie danych tabeli…", "Loading the table data…"),
            );
        } else if !connected {
            // The connection was closed: the rows are gone from memory.
            connect = widgets::empty_state(
                ui,
                &p,
                Icon::Database,
                l("Baza nie jest połączona", "The database is not connected"),
                l(
                    "Połącz się ponownie, aby wczytać wiersze tej tabeli.",
                    "Connect again to load the rows of this table.",
                ),
                Some(l("Połącz", "Connect")),
            );
        } else if !failed {
            // Connected again: read the rows without asking.
            refresh = true;
        }

        // Keys of the data editor, unless a dialog is open or a text
        // field other than the filter has the keyboard.
        let filter_id = egui::Id::new(("filter_value", tabid));
        let filter_focused = ui.memory(|m| m.has_focus(filter_id));
        let grid_focused = ui.memory(|m| m.has_focus(egui::Id::new(("grid_keys", tabid))));
        let nothing_focused = ui.memory(|m| m.focused().is_none());
        let modal = ui.ctx().memory(|m| m.top_modal_layer()).is_some();
        let typing = ui.ctx().text_edit_focused() && !filter_focused;
        let (mut add_key, mut goto_key, mut structure_key, mut query_key) =
            (false, false, false, false);
        if !modal && self.cell_editor.is_some() && take_mod(ui, false, egui::Key::S) {
            footer.apply = true;
        }
        if !modal && !typing {
            apply_filter |= take_mod(ui, false, egui::Key::R);
            if take_mod(ui, false, egui::Key::F) {
                ui.memory_mut(|m| m.request_focus(filter_id));
            }
            footer.stop |= take_mod(ui, false, egui::Key::T) && busy;
            if !filter_focused {
                add_key = take_mod(ui, false, egui::Key::N);
                goto_key = take_mod(ui, false, egui::Key::G) && has_result;
                structure_key = take_mod(ui, false, egui::Key::D);
                query_key = take(ui, egui::Modifiers::CTRL, egui::Key::Q);
                if (grid_focused || (form_view && nothing_focused))
                    && take(ui, egui::Modifiers::NONE, egui::Key::Space)
                {
                    footer.form_view = Some(!form_view);
                }
            }
        }
        if add_key && editable && caps.insert_rows {
            footer.insert = true;
        }
        if goto_key {
            self.ask_goto(index);
        }
        if let (true, Some(connection), Some(table)) = (structure_key, connection, source.clone()) {
            if caps.table_designer && editable {
                self.open_design(connection, table);
            } else {
                self.show_structure(connection, table);
            }
        }
        if let (true, Some(connection), Some(table)) = (query_key, connection, source.clone()) {
            self.query_object(connection, table);
        }
        if let Some(column) = sort {
            self.sort_result(index, column);
        }
        if let Some(rows) = footer.page_rows {
            let tab = &mut self.tabs[index];
            tab.page_rows = Some(rows);
            tab.page = 0;
            refresh = true;
        }
        if footer.default_rows {
            let rows = self.tabs[index]
                .page_rows
                .unwrap_or(self.profiles.preferences.page_rows);
            self.profiles.preferences.page_rows = rows;
            self.save();
            let text = if en {
                format!(
                    "Every table now opens with {} records per page.",
                    page_rows_label(rows, en).to_lowercase()
                )
            } else {
                format!(
                    "Każda tabela otworzy się teraz z liczbą rekordów na stronie: {}.",
                    page_rows_label(rows, en).to_lowercase()
                )
            };
            self.notify(text, false);
        }
        if let Some(form) = footer.form_view {
            self.tabs[index].form_view = form;
        }
        if footer.stop {
            self.tabs[index].cancel.cancel();
        }
        if footer.apply {
            if let Some(editor) = self.cell_editor.take()
                && let Err(error) = self.commit_cell(editor)
            {
                self.notify(error, true);
            }
        } else if footer.discard {
            self.cell_editor = None;
        }
        if apply_filter {
            let tab = &mut self.tabs[index];
            tab.page = 0;
            tab.total = None;
            tab.range = None;
            self.browse(index);
        } else if let Some(page) = footer.page {
            let tab = &mut self.tabs[index];
            tab.page = page;
            tab.range = None;
            tab.selected_row = None;
            tab.selected_cell = None;
            self.browse(index);
        } else if footer.last {
            self.count_records(index);
        } else if refresh || footer.refresh {
            self.tabs[index].total = None;
            self.browse(index);
        }
        if actions.insert || footer.insert {
            self.begin_insert(index);
        }
        if actions.delete || footer.delete {
            self.ask_delete(index);
        }
        if actions.export {
            self.export_result(tabid);
        }
        if import {
            self.import_data(tabid);
        }
        if let (true, Some(connection), Some(table)) = (builder, connection, source) {
            self.open_builder(connection, table);
        }
        if editor {
            self.open_in_editor(connection, &title, sql);
        }
        if let (true, Some(connection)) = (designer, connection) {
            self.open_designer(connection);
        }
        if let (true, Some(connection)) = (connect, connection) {
            self.connect(connection);
        }
        if close {
            self.close_tab(tabid);
        }
    }

    /// No table is open: say how to open one.
    fn table_empty(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let focus = self.focus_connection();
        let connected = focus.is_some_and(|id| self.databases.contains_key(&id));
        let (text, action) = if self.profiles.connections.is_empty() {
            (
                l(
                    "Dodaj połączenie z bazą, a jej tabele i widoki pojawią się w panelu Połączenia.",
                    "Add a database connection; its tables and views appear in the Connections panel.",
                ),
                Some(l("Nowe połączenie", "New connection")),
            )
        } else if connected {
            (
                l(
                    "Kliknij dwukrotnie tabelę lub widok w panelu Połączenia. Dane można filtrować, sortować i zmieniać po jednej komórce.",
                    "Double-click a table or view in the Connections panel. The data can be filtered, sorted and changed one cell at a time.",
                ),
                None,
            )
        } else {
            (
                l(
                    "Połącz się z bazą, a potem kliknij dwukrotnie tabelę lub widok w panelu Połączenia.",
                    "Connect to a database, then double-click a table or view in the Connections panel.",
                ),
                focus.map(|_| l("Połącz", "Connect")),
            )
        };
        egui::Frame::NONE
            .inner_margin(egui::Margin::same(20))
            .show(ui, |ui| {
                if widgets::empty_state(
                    ui,
                    &p,
                    Icon::Table,
                    l("Wybierz tabelę", "Choose a table"),
                    text,
                    action,
                ) {
                    match focus {
                        Some(id) if !self.profiles.connections.is_empty() => self.connect(id),
                        _ => self.new_connection(),
                    }
                }
            });
    }
}
