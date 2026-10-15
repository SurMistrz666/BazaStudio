//! "Dane tabeli": the rows of one table or view, with filter, order, pages and
//! single-cell editing.
use super::results::{FooterActions, page_rows_label};
use super::shortcuts::{take, take_mod};
use super::*;
use crate::editor_text::{self, HintKind};
use crate::widgets::Btn;

/// Keywords offered while a condition is typed, next to the columns and the
/// functions it may use.
const KEYWORDS: &[&str] = &[
    "AND",
    "OR",
    "NOT",
    "IN",
    "IS NULL",
    "IS NOT NULL",
    "LIKE",
    "BETWEEN",
    "NULL",
];

/// Words that are not names when unquoted in at least one engine. A column
/// called one of them is quoted when a hint puts it into a condition.
const RESERVED: &[&str] = &[
    "ACCESS",
    "ALL",
    "ALTER",
    "AND",
    "ANY",
    "ARRAY",
    "AS",
    "ASC",
    "BETWEEN",
    "BOTH",
    "BY",
    "CASE",
    "CAST",
    "CHECK",
    "COLLATE",
    "COLUMN",
    "COMMENT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "CURRENT",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "DATE",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DO",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "FALSE",
    "FETCH",
    "FILE",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "GRANT",
    "GROUP",
    "HAVING",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEADING",
    "LEFT",
    "LEVEL",
    "LIKE",
    "LIMIT",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "MODE",
    "NATURAL",
    "NOT",
    "NULL",
    "NUMBER",
    "OF",
    "OFFSET",
    "ON",
    "ONLY",
    "OPTION",
    "OR",
    "ORDER",
    "OUTER",
    "PRIMARY",
    "RANGE",
    "REFERENCES",
    "RIGHT",
    "ROW",
    "ROWNUM",
    "ROWS",
    "SELECT",
    "SESSION",
    "SESSION_USER",
    "SET",
    "SIZE",
    "SOME",
    "START",
    "TABLE",
    "THEN",
    "TO",
    "TOP",
    "TRAILING",
    "TRUE",
    "UID",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USER",
    "USING",
    "VALUE",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
];

/// A column name as a hint writes it into a condition: as it is when it is
/// a plain lower-case word, otherwise quoted, because PostgreSQL reads an
/// unquoted `userId` as `userid` and no engine reads `order` as a name.
fn condition_name(dialect: &Dialect, name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !RESERVED.iter().any(|word| word.eq_ignore_ascii_case(name));
    if plain {
        name.to_owned()
    } else {
        sql::quote_identifier(dialect, name).unwrap_or_else(|_| name.to_owned())
    }
}

/// The list of names at the cursor of the filter field.
#[derive(Default)]
pub(super) struct FilterHints {
    /// The line the arrows chose.
    chosen: usize,
    /// The word Esc closed the list for; typing on opens it again.
    closed: Option<String>,
}

/// "Kolumny": which columns of the result the grid shows, found by name.
/// "Wszystkie" and "Żadne" act on the columns the search lists.
fn column_choice(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    tab: &mut QueryTab,
    columns: &[String],
    en: bool,
) {
    let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
    let hidden = columns
        .iter()
        .filter(|name| tab.hidden_columns.contains(*name))
        .count();
    let label = if hidden == 0 {
        l("Kolumny", "Columns").to_owned()
    } else {
        format!(
            "{} {}/{}",
            l("Kolumny", "Columns"),
            columns.len() - hidden,
            columns.len()
        )
    };
    // A width that does not change with the count keeps the list under it
    // in place while columns are ticked.
    let width = Btn::new(l("Kolumny 000/000", "Columns 000/000"))
        .icon(Icon::View)
        .width(ui, p);
    let button = Btn::new(&label)
        .icon(Icon::View)
        .min_width(width)
        .enabled(!columns.is_empty())
        .show(ui, p)
        .on_hover_text(l("Widoczne kolumny", "Visible columns"));
    let popup = egui::Id::new(("column_choice", tab.id));
    // The list closes on Esc or a press beside it and its button. egui's own
    // "click outside" took a click on "Żadne" for one beside the list once
    // the search had made the list shorter.
    let shown = egui::Popup::from_toggle_button_response(&button)
        .id(popup)
        .close_behavior(egui::PopupCloseBehavior::IgnoreClicks)
        .width(260.0)
        .show(|ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.add(
                widgets::field(p, &mut tab.column_search)
                    .hint_text(l("Szukaj kolumny", "Find a column"))
                    .desired_width(f32::INFINITY),
            );
            let needle = tab.column_search.to_lowercase();
            let listed = columns
                .iter()
                .filter(|name| name.to_lowercase().contains(&needle))
                .collect::<Vec<_>>();
            ui.horizontal(|ui| {
                if Btn::new(l("Wszystkie", "All"))
                    .small()
                    .show(ui, p)
                    .clicked()
                {
                    for name in &listed {
                        tab.hidden_columns.remove(*name);
                    }
                }
                if Btn::new(l("Żadne", "None")).small().show(ui, p).clicked() {
                    tab.hidden_columns
                        .extend(listed.iter().map(|name| (*name).clone()));
                }
            });
            egui::ScrollArea::vertical()
                .max_height(320.0)
                .show(ui, |ui| {
                    for name in &listed {
                        let mut shown = !tab.hidden_columns.contains(*name);
                        if widgets::checkbox(ui, p, &mut shown, name).changed() {
                            if shown {
                                tab.hidden_columns.remove(*name);
                            } else {
                                tab.hidden_columns.insert((*name).clone());
                            }
                        }
                    }
                    if listed.is_empty() {
                        widgets::help(ui, p, l("Żadna kolumna nie pasuje.", "No column matches."));
                    }
                });
        });
    if let Some(shown) = shown {
        let beside = ui.input(|i| {
            i.pointer.any_pressed()
                && i.pointer.interact_pos().is_some_and(|at| {
                    !shown.response.rect.contains(at) && !button.rect.contains(at)
                })
        });
        if beside || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            egui::Popup::close_id(ui.ctx(), popup);
        }
    }
}

/// The names offered at `at`, under the cursor of the filter field, in the
/// syntax colours of the theme. Returns the one that was clicked.
fn hint_list(
    ctx: &egui::Context,
    p: &theme::Palette,
    tab: Uuid,
    at: egui::Pos2,
    hints: &[(String, HintKind)],
    chosen: usize,
    en: bool,
) -> Option<usize> {
    let notes = hints
        .iter()
        .map(|(_, kind)| match kind {
            // A type as the header of the grid writes it.
            HintKind::Column(data_type) => data_type.to_lowercase(),
            HintKind::Function if en => "function".into(),
            HintKind::Function => "funkcja".into(),
            HintKind::Keyword if en => "keyword".into(),
            HintKind::Keyword => "słowo kluczowe".into(),
        })
        .collect::<Vec<String>>();
    let choices = hints
        .iter()
        .zip(&notes)
        .map(|((name, kind), note)| widgets::Choice {
            text: name.trim_end(),
            note,
            color: match kind {
                HintKind::Column(_) => p.string,
                HintKind::Function => p.number,
                HintKind::Keyword => p.keyword,
            },
            checked: None,
        })
        .collect::<Vec<_>>();
    widgets::choice_list(
        ctx,
        p,
        egui::Id::new(("filter_hints", tab)),
        at,
        &choices,
        Some(chosen),
        false,
    )
}

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
        let dialect = self.tabs[index]
            .connection
            .and_then(|id| self.databases.get(&id))
            .map(|db| *db.dialect());
        let tab = &mut self.tabs[index];
        let tabid = tab.id;
        let connection = tab.connection;
        let editable = tab.table.is_some();
        let tab_keyed = tab.columns.iter().any(|c| c.primary_key);
        let has_rows = tab.result.is_some();
        let typed_columns = tab
            .result
            .as_ref()
            .map(|result| {
                result
                    .columns
                    .iter()
                    .map(|c| (c.name.clone(), c.data_type.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let columns = typed_columns
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        widgets::toolbar(ui, &p, |ui| {
            let filter_button = Btn::new(l("Filtruj", "Filter")).icon(Icon::Filter);
            // The two icons on the right and "Kolumny" before them, with the
            // widest count it shows.
            let icons = 2.0 * ((p.control_height() - 3.0).max(24.0) + 2.0)
                + Btn::new(l("Kolumny 000/000", "Columns 000/000"))
                    .icon(Icon::View)
                    .width(ui, &p)
                + 6.0;
            let fixed = filter_button.width(ui, &p) + icons + 44.0;
            let room = ui.available_width() - fixed;
            let condition = l("Warunek SQL", "SQL condition");
            widgets::combo(
                ui,
                &p,
                ("filtercol", tab.id),
                if tab.filter_sql {
                    condition.to_owned()
                } else if tab.filter_column.is_empty() {
                    l("Kolumna", "Column").to_owned()
                } else {
                    tab.filter_column.clone()
                },
                (room * 0.2).clamp(80.0, 150.0),
                |ui| {
                    if caps.condition {
                        ui.selectable_value(&mut tab.filter_sql, true, condition);
                        ui.separator();
                    }
                    for name in &columns {
                        let chosen = !tab.filter_sql && tab.filter_column == *name;
                        if ui.selectable_label(chosen, name).clicked() {
                            tab.filter_column = name.clone();
                            tab.filter_sql = false;
                        }
                    }
                },
            )
            .response
            .on_hover_text(l(
                "Kolumna filtra albo warunek SQL",
                "Filter column or SQL condition",
            ));
            let input =
                (ui.available_width() - filter_button.width(ui, &p) - icons - 30.0).max(60.0);
            // While a condition is typed, the names that complete the word at
            // the cursor are offered there: the arrows choose, Enter or Tab
            // puts one in, Esc closes the list.
            let filter_id = egui::Id::new(("filter_value", tab.id));
            let word = egui::text_edit::TextEditState::load(ui.ctx(), filter_id)
                .and_then(|state| state.cursor.char_range())
                .map(|range| usize::from(range.primary.index))
                .filter(|_| tab.filter_sql && ui.memory(|m| m.has_focus(filter_id)))
                .and_then(|at| {
                    editor_text::word_before(&tab.filter_value, at)
                        .map(|(start, word)| (start..at, word))
                })
                .filter(|(_, word)| tab.filter_hints.closed.as_ref() != Some(word));
            let hints = word.as_ref().map_or_else(Vec::new, |(_, word)| {
                editor_text::condition_hints(
                    word,
                    &typed_columns,
                    sql::CONDITION_FUNCTIONS,
                    KEYWORDS,
                )
            });
            let mut chosen = tab.filter_hints.chosen.min(hints.len().saturating_sub(1));
            let mut accept = None;
            let mut shown = !hints.is_empty();
            if shown {
                let count = hints.len();
                let none = egui::Modifiers::NONE;
                if take(ui, none, egui::Key::ArrowDown) {
                    chosen = (chosen + 1) % count;
                }
                if take(ui, none, egui::Key::ArrowUp) {
                    chosen = (chosen + count - 1) % count;
                }
                if take(ui, none, egui::Key::Enter) || take(ui, none, egui::Key::Tab) {
                    accept = Some(chosen);
                }
                if take(ui, none, egui::Key::Escape) {
                    tab.filter_hints.closed = word.as_ref().map(|(_, word)| word.clone());
                    shown = false;
                }
            }
            let output = ui
                .add_enabled_ui(caps.filter, |ui| {
                    widgets::field(&p, &mut tab.filter_value)
                        .id(filter_id)
                        .desired_width(input - 18.0)
                        .hint_text(if tab.filter_sql {
                            l(
                                "Warunek, np. id = 19 OR secret IS NULL",
                                "Condition, e.g. id = 19 OR secret IS NULL",
                            )
                        } else {
                            l("Filtruj: zawiera tekst", "Filter: contains text")
                        })
                        .show(ui)
                })
                .inner;
            let response = &output.response;
            if let (true, Some((range, _))) = (shown, &word) {
                // Under the start of the word being typed.
                let x = output.galley_pos.x
                    + output
                        .galley
                        .pos_from_cursor(egui::text::CCursor::new(range.start))
                        .min
                        .x;
                let at = egui::pos2(
                    x.clamp(response.rect.left(), response.rect.right()),
                    response.rect.bottom() + 2.0,
                );
                accept = accept.or(hint_list(ui.ctx(), &p, tab.id, at, &hints, chosen, en));
                // Tab, the arrows and Esc stay with the field while the list is open.
                ui.memory_mut(|m| {
                    m.set_focus_lock_filter(
                        filter_id,
                        egui::EventFilter {
                            tab: true,
                            horizontal_arrows: false,
                            vertical_arrows: true,
                            escape: true,
                        },
                    )
                });
            }
            if let (Some(chosen_hint), Some((range, _))) = (accept, &word) {
                let (name, kind) = &hints[chosen_hint];
                let insert = match (kind, dialect) {
                    (HintKind::Column(_), Some(dialect)) => condition_name(&dialect, name),
                    _ => name.clone(),
                };
                let (text, at) =
                    editor_text::replace_chars(&tab.filter_value, range.clone(), &insert);
                tab.filter_value = text;
                let mut state =
                    egui::text_edit::TextEditState::load(ui.ctx(), filter_id).unwrap_or_default();
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(
                        egui::text::CCursor::new(at),
                    )));
                state.store(ui.ctx(), filter_id);
                ui.memory_mut(|m| m.request_focus(filter_id));
                chosen = 0;
            }
            tab.filter_hints.chosen = chosen;
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
                if caps.browse == baza_core::engine::BrowseModel::Keys
                    && widgets::icon_toggle(
                        ui,
                        &p,
                        Icon::Schema,
                        l("Drzewo kluczy", "Key tree"),
                        tab.key_tree,
                    )
                    .clicked()
                {
                    tab.key_tree = !tab.key_tree;
                }
                ui.add_space(6.0);
                column_choice(ui, &p, tab, &columns, en);
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
                    // The font has ✓ but no ✕: × stands for the painted
                    // icon of the button that discards.
                    let text = if editable && caps.edit_cells && tab_keyed {
                        l(
                            "Pisz w komórce albo kliknij ją dwukrotnie. Zmiany czekają w siatce: ✓ albo Ctrl+S zapisuje je w bazie, × porzuca. Nagłówek sortuje dane.",
                            "Type in a cell or double-click it. Changes wait in the grid: ✓ or Ctrl+S saves them to the database, × discards them. A header sorts the data.",
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

        // The keys of a key-value store as a tree of their parts.
        if caps.browse == baza_core::engine::BrowseModel::Keys
            && has_rows
            && self.tabs[index].key_tree
        {
            egui::Panel::left(egui::Id::new(("key_tree_panel", tabid)))
                .resizable(true)
                .default_size(280.0)
                .size_range(160.0..=600.0)
                .frame(
                    egui::Frame::NONE
                        .fill(p.side)
                        .inner_margin(egui::Margin::symmetric(8, 6)),
                )
                .show(ui, |ui| self.key_tree_panel(ui, index));
        }

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
        if !modal
            && (self.cell_editor.is_some() || self.has_staged(index))
            && take_mod(ui, false, egui::Key::S)
        {
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
            self.ask_save(index);
        } else if footer.discard {
            self.discard_staged(index);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hint_quotes_a_column_name_only_when_it_has_to() {
        let ansi = Dialect::ANSI;
        let backticks = Dialect {
            name_quotes: ('`', '`'),
            ..Dialect::ANSI
        };
        // Plain lower-case words stay as they are.
        assert_eq!(condition_name(&ansi, "status"), "status");
        assert_eq!(condition_name(&ansi, "created_at2"), "created_at2");
        assert_eq!(condition_name(&ansi, "_rev"), "_rev");
        // Mixed case would be folded, a reserved word is not a name.
        assert_eq!(condition_name(&ansi, "userId"), "\"userId\"");
        assert_eq!(condition_name(&ansi, "ID"), "\"ID\"");
        assert_eq!(condition_name(&ansi, "order"), "\"order\"");
        assert_eq!(condition_name(&ansi, "user"), "\"user\"");
        assert_eq!(condition_name(&backticks, "group"), "`group`");
        // Anything else than a plain word, in the quotes of the engine.
        assert_eq!(condition_name(&ansi, "first name"), "\"first name\"");
        assert_eq!(condition_name(&ansi, "2fa"), "\"2fa\"");
        assert_eq!(condition_name(&ansi, "zażółć"), "\"zażółć\"");
        assert_eq!(condition_name(&backticks, "a`b"), "`a``b`");
    }
}
