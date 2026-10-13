//! "Edytor SQL": the open queries, the editor and the result of the last run.
use super::shortcuts::{take, take_mod};
use super::*;
use crate::editor_text;
use crate::widgets::Btn;
use eframe::egui::{RichText, TextEdit};
use std::ops::Range;

/// The search bar of the SQL editor (Mod+F).
pub(super) struct FindBar {
    pub tab: Uuid,
    pub query: String,
    /// The field asks for the keyboard when the bar opens.
    pub fresh: bool,
    /// The line of the last match, to scroll the editor to.
    pub scroll_line: Option<usize>,
}

/// Shortcut shown on the run button. The return glyph comes from the system
/// font, which only macOS is known to carry.
const RUN_HINT: &str = if cfg!(target_os = "macos") {
    "Ctrl ↵"
} else {
    "Ctrl+Enter"
};
/// Keys that run a query, for the sentence under the editor.
const RUN_KEYS: &str = if cfg!(target_os = "macos") {
    "Ctrl+Enter / ⌘+Enter"
} else {
    "Ctrl+Enter"
};

impl StudioApp {
    pub(super) fn sql_view(&mut self, ui: &mut egui::Ui) {
        if self.tabs[self.active].kind != TabKind::Query {
            self.set_section(Section::Sql);
        }
        let index = self.active;
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let console =
            self.capabilities(self.tabs[index].connection).language == QueryLanguage::Command;
        // A query that still holds the SQL sample, on a database whose
        // language is not SQL: a console starts empty.
        if console {
            let tab = &mut self.tabs[index];
            if tab.sql == SAMPLE_SQL {
                tab.sql.clear();
                if tab.title == "SQL" {
                    tab.title = l("Konsola", "Console").to_owned();
                }
            }
        }
        let queries = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.kind == TabKind::Query)
            .map(|(i, tab)| {
                (
                    i,
                    format!("{}{}", tab.title, if tab.busy { " …" } else { "" }),
                )
            })
            .collect::<Vec<_>>();
        let mut switch_to = None;
        let (mut run, mut stop, mut load, mut save) = (false, false, false, false);
        let (mut library, mut new_tab, mut close) = (false, false, false);
        let tab = &mut self.tabs[index];
        let tabid = tab.id;
        let busy = tab.busy;
        widgets::toolbar(ui, &p, |ui| {
            let run_button = Btn::primary(l("Uruchom", "Run"))
                .icon(Icon::Play)
                .hint(RUN_HINT)
                .enabled(!busy);
            let stop_button = Btn::new(l("Zatrzymaj", "Stop")).icon(Icon::Stop);
            let icons = 5.0 * ((p.control_height() - 3.0).max(24.0) + 2.0);
            let mut fixed = run_button.width(ui, &p) + icons + 40.0;
            if busy {
                fixed += stop_button.width(ui, &p) + 8.0;
            }
            let select = ((ui.available_width() - fixed) / 2.0 - 30.0).clamp(90.0, 230.0);
            widgets::combo(
                ui,
                &p,
                "open_queries",
                queries
                    .iter()
                    .find(|(i, _)| *i == index)
                    .map_or_else(String::new, |(_, title)| title.clone()),
                select,
                |ui| {
                    for (i, title) in &queries {
                        if ui.selectable_label(*i == index, title).clicked() {
                            switch_to = Some(*i);
                        }
                    }
                },
            )
            .response
            .on_hover_text(l("Otwarte zapytania", "Open queries"));
            widgets::combo(
                ui,
                &p,
                ("query_connection", tab.id),
                tab.connection
                    .and_then(|id| {
                        self.profiles
                            .connections
                            .iter()
                            .find(|p| p.id == id)
                            .map(|p| p.name.clone())
                    })
                    .unwrap_or_else(|| l("Wybierz połączenie", "Choose connection").into()),
                select,
                |ui| {
                    for profile in &self.profiles.connections {
                        if ui
                            .selectable_value(&mut tab.connection, Some(profile.id), &profile.name)
                            .changed()
                        {
                            tab.table = None;
                            tab.columns.clear();
                            tab.result = None;
                        }
                    }
                },
            )
            .response
            .on_hover_text(l("Połączenie zapytania", "Connection of the query"));
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.add_space(4.0);
            new_tab = widgets::icon_button(
                ui,
                &p,
                Icon::Plus,
                &format!(
                    "{} (Ctrl+Q, {})",
                    l("Nowe zapytanie", "New query"),
                    super::shortcuts::keys_text("Mod+N", en)
                ),
            )
            .clicked();
            load = widgets::icon_button(
                ui,
                &p,
                Icon::File,
                &format!(
                    "{} ({})",
                    l("Otwórz plik SQL", "Open SQL file"),
                    super::shortcuts::keys_text("Mod+O", en)
                ),
            )
            .clicked();
            save = widgets::icon_button(ui, &p, Icon::Save, l("Zapisz plik SQL", "Save SQL file"))
                .clicked();
            library = widgets::icon_button(
                ui,
                &p,
                Icon::Bookmark,
                &format!(
                    "{} ({})",
                    l("Zapisz w bibliotece zapytań", "Save to the query library"),
                    super::shortcuts::keys_text("Mod+S", en)
                ),
            )
            .clicked();
            ui.add_enabled_ui(queries.len() > 1, |ui| {
                close = widgets::icon_button(
                    ui,
                    &p,
                    Icon::Close,
                    l("Zamknij zapytanie", "Close query"),
                )
                .clicked();
            });
            widgets::right(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                run = run_button.show(ui, &p).clicked();
                if busy {
                    stop = stop_button.show(ui, &p).clicked();
                }
            });
        });

        // The search bar, when Mod+F opened it for this query.
        let (mut find_next, mut find_previous, mut find_close) = (false, false, false);
        let mut find_count = None;
        if let Some(find) = self.find.as_mut().filter(|f| f.tab == tabid) {
            let matches = editor_text::find_all(&tab.sql, &find.query);
            let current = tab
                .selection
                .as_ref()
                .and_then(|s| matches.iter().position(|m| m == s));
            find_count = Some((current, matches.len()));
            egui::Frame::NONE
                .fill(p.side)
                .inner_margin(egui::Margin::symmetric(16, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let response = ui.add(
                            widgets::field(&p, &mut find.query)
                                .id(egui::Id::new("sql_find"))
                                .desired_width(260.0)
                                .hint_text(l("Szukaj w zapytaniu", "Find in the query")),
                        );
                        if find.fresh {
                            // The text it opens with is selected: typing
                            // replaces it.
                            let id = egui::Id::new("sql_find");
                            let mut state = egui::text_edit::TextEditState::load(ui.ctx(), id)
                                .unwrap_or_default();
                            state
                                .cursor
                                .set_char_range(Some(egui::text::CCursorRange::two(
                                    egui::text::CCursor::new(0),
                                    egui::text::CCursor::new(find.query.chars().count()),
                                )));
                            state.store(ui.ctx(), id);
                            response.request_focus();
                            find.fresh = false;
                        }
                        if response.has_focus() || response.lost_focus() {
                            let (enter, shift, escape) = ui.input(|i| {
                                (
                                    i.key_pressed(egui::Key::Enter),
                                    i.modifiers.shift,
                                    i.key_pressed(egui::Key::Escape),
                                )
                            });
                            find_next |= enter && !shift;
                            find_previous |= enter && shift;
                            find_close |= escape;
                        }
                        find_previous |= widgets::icon_button(
                            ui,
                            &p,
                            Icon::ArrowUp,
                            l("Poprzedni (Shift+F3)", "Previous (Shift+F3)"),
                        )
                        .clicked();
                        find_next |= widgets::icon_button(
                            ui,
                            &p,
                            Icon::ArrowDown,
                            l("Następny (F3)", "Next (F3)"),
                        )
                        .clicked();
                        let count = match (current, matches.len()) {
                            (_, 0) if !find.query.is_empty() => l("brak", "none").to_owned(),
                            (_, 0) => String::new(),
                            (Some(at), all) => format!("{} / {all}", at + 1),
                            (None, all) => all.to_string(),
                        };
                        ui.label(RichText::new(count).size(p.small()).color(p.muted));
                        widgets::right(ui, |ui| {
                            find_close |= widgets::icon_button(
                                ui,
                                &p,
                                Icon::Close,
                                l("Zamknij szukanie (Esc)", "Close the search (Esc)"),
                            )
                            .clicked();
                        });
                    });
                });
            widgets::hline(ui, &p);
        }
        let find_query = self
            .find
            .as_ref()
            .filter(|f| f.tab == tabid && !f.query.is_empty())
            .map(|f| f.query.clone());
        let scroll_line = self
            .find
            .as_mut()
            .filter(|f| f.tab == tabid)
            .and_then(|f| f.scroll_line.take());
        let current_match = tab.selection.clone();

        // Editor.
        let size = p.code();
        let wrap = self.profiles.preferences.editor_word_wrap;
        let numbered = self.profiles.preferences.editor_line_numbers;
        let found_fill = theme::blend(p.bg, theme::BRAND, 0.28);
        let current_fill = theme::blend(p.bg, theme::BRAND, 0.55);
        let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, width: f32| {
            let mut job = highlight(text.as_str(), size, p);
            if let Some(query) = &find_query {
                job = mark_matches(
                    job,
                    text.as_str(),
                    query,
                    current_match.as_ref(),
                    found_fill,
                    current_fill,
                );
            }
            job.wrap.max_width = if wrap { width } else { f32::INFINITY };
            ui.fonts_mut(|f| f.layout_job(job))
        };
        let height = (ui.available_height() * self.editor_ratio).clamp(110.0, 620.0);
        let lines = tab.sql.split('\n').count().max(1);
        let line_height = code_line_height(size);
        let out = egui::Frame::NONE
            .inner_margin(egui::Margin {
                left: 10,
                right: 4,
                top: 14,
                bottom: 4,
            })
            .show(ui, |ui| {
                let mut area = egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .max_height(height)
                    .min_scrolled_height(height)
                    .id_salt(("editor_scroll", tab.id));
                // A match found off screen: its line a third of the way down.
                if let Some(line) = scroll_line {
                    area = area.vertical_scroll_offset(
                        (line as f32 * line_height - height / 3.0).max(0.0),
                    );
                }
                area.show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        if numbered {
                            let digits = lines.to_string().len().max(2) as f32;
                            let width = digits * size * 0.62 + 6.0;
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(width, lines as f32 * line_height),
                                egui::Sense::hover(),
                            );
                            // Lay each number out like a line of code, so
                            // both sit on the same baseline.
                            let clip = ui.clip_rect();
                            let first =
                                ((clip.top() - rect.top()) / line_height).floor().max(0.0) as usize;
                            let last = (((clip.bottom() - rect.top()) / line_height).ceil()
                                as usize)
                                .min(lines);
                            let color = theme::blend(p.bg, p.muted, 0.75);
                            for line in first..last {
                                let galley =
                                    ui.painter()
                                        .layout_job(egui::text::LayoutJob::single_section(
                                            (line + 1).to_string(),
                                            egui::TextFormat {
                                                font_id: egui::FontId::monospace(size),
                                                line_height: Some(line_height),
                                                color,
                                                ..Default::default()
                                            },
                                        ));
                                ui.painter().galley(
                                    egui::pos2(
                                        rect.right() - galley.size().x,
                                        rect.top() + line as f32 * line_height,
                                    ),
                                    galley,
                                    color,
                                );
                            }
                            ui.add_space(15.0);
                        } else {
                            ui.add_space(8.0);
                        }
                        TextEdit::multiline(&mut tab.sql)
                            .id(egui::Id::new(("sql_editor", tab.id)))
                            .font(egui::TextStyle::Monospace)
                            .code_editor()
                            .frame(egui::Frame::NONE)
                            .margin(egui::Margin::ZERO)
                            .desired_width(ui.available_width())
                            .desired_rows(6)
                            .layouter(&mut layouter)
                            .show(ui)
                    })
                    .inner
                })
                .inner
            })
            .inner;
        egui::Frame::NONE
            .inner_margin(egui::Margin {
                left: 16,
                right: 16,
                top: 6,
                bottom: 10,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                widgets::help(
                    ui,
                    &p,
                    &format!(
                        "{RUN_KEYS} {}",
                        if console {
                            l(
                                "uruchamia zaznaczenie albo wszystkie polecenia, jedno po drugim. Wynik każdego polecenia trafia do siatki.",
                                "runs the selection or every command, one after another. Each result goes to the grid.",
                            )
                        } else {
                            l(
                                "uruchamia zaznaczenie albo całe zapytanie. Ctrl+Space podpowiada nazwy tabel i kolumn, F1 pokazuje wszystkie skróty.",
                                "runs the selection or the whole query. Ctrl+Space suggests table and column names, F1 shows every shortcut.",
                            )
                        }
                    ),
                );
            });
        tab.selection = out.cursor_range.map(|r| {
            let v = r.as_sorted_char_range();
            v.start.0..v.end.0
        });
        if out.response.changed() {
            // Arbitrary SQL changes cannot retain a trusted table-edit mapping.
            tab.table = None;
        }
        let editor_focused = out.response.has_focus();
        // Keys of the editor. A dialog or
        // another text field than the editor and its search keeps them.
        let find_focused = ui.memory(|m| m.has_focus(egui::Id::new("sql_find")));
        let modal = ui.ctx().memory(|m| m.top_modal_layer()).is_some();
        let free = !modal && (editor_focused || find_focused || !ui.ctx().text_edit_focused());
        let mut ctrl_enter = false;
        let (mut run_statement, mut select_statement) = (false, false);
        let mut text_change: Option<(String, Range<usize>)> = None;
        if free {
            ctrl_enter = take_mod(ui, false, egui::Key::Enter) || take_mod(ui, false, egui::Key::R);
            run_statement = take_mod(ui, true, egui::Key::R);
            select_statement = take_mod(ui, false, egui::Key::E);
            stop |= take_mod(ui, false, egui::Key::T) && busy;
            if take_mod(ui, false, egui::Key::F) {
                // The selected words, when short, become the search.
                let selected = tab
                    .selection
                    .clone()
                    .filter(|r| !r.is_empty() && r.len() <= 100)
                    .map(|r| {
                        tab.sql
                            .chars()
                            .skip(r.start)
                            .take(r.len())
                            .collect::<String>()
                    })
                    .filter(|text| !text.contains('\n'));
                let query = selected
                    .or_else(|| self.find.as_ref().map(|f| f.query.clone()))
                    .unwrap_or_default();
                self.find = Some(FindBar {
                    tab: tabid,
                    query,
                    fresh: true,
                    scroll_line: None,
                });
            }
            find_next |= take(ui, egui::Modifiers::NONE, egui::Key::F3);
            find_previous |= take(ui, egui::Modifiers::SHIFT, egui::Key::F3);
        }
        if editor_focused && !modal && !console {
            let selection = tab.selection.clone().unwrap_or(0..0);
            if take_mod(ui, false, egui::Key::Slash) {
                text_change = Some(editor_text::toggle_comment(&tab.sql, selection.clone()));
            } else if take_mod(ui, true, egui::Key::U) && !selection.is_empty() {
                text_change = Some(editor_text::change_case(&tab.sql, selection.clone(), true));
            } else if take_mod(ui, true, egui::Key::L) && !selection.is_empty() {
                text_change = Some(editor_text::change_case(&tab.sql, selection, false));
            }
        }
        if let Some((text, selection)) = text_change {
            tab.sql = text;
            // Arbitrary SQL changes cannot retain a trusted table-edit mapping.
            tab.table = None;
            select_in_editor(ui.ctx(), tabid, selection.clone(), true);
            tab.selection = Some(selection);
        }
        if find_close {
            self.find = None;
            ui.memory_mut(|m| m.request_focus(egui::Id::new(("sql_editor", tabid))));
        }
        if (find_next || find_previous)
            && let Some(query) = self
                .find
                .as_ref()
                .filter(|f| f.tab == tabid)
                .map(|f| f.query.clone())
        {
            let from = tab
                .selection
                .as_ref()
                .map_or(0, |r| if find_previous { r.start } else { r.end });
            match editor_text::find(&tab.sql, &query, from, find_previous) {
                Some(found) => {
                    select_in_editor(ui.ctx(), tabid, found.clone(), false);
                    if let Some(find) = self.find.as_mut() {
                        find.scroll_line = Some(editor_text::line_of(&tab.sql, found.start));
                    }
                    tab.selection = Some(found);
                }
                None if find_count.is_some_and(|(_, all)| all == 0) => {}
                None => {}
            }
        }
        if run_statement || select_statement {
            let dialect = tab
                .connection
                .and_then(|c| self.databases.get(&c).map(|db| *db.dialect()))
                .or_else(|| {
                    tab.connection.and_then(|c| {
                        self.profiles
                            .connections
                            .iter()
                            .find(|p| p.id == c)
                            .map(|p| *p.dialect())
                    })
                })
                .unwrap_or(Dialect::ANSI);
            let cursor = tab.selection.as_ref().map_or(0, |r| r.end);
            if let Some(statement) = statement_at_cursor(&tab.sql, cursor, &dialect, console) {
                select_in_editor(ui.ctx(), tabid, statement.clone(), true);
                tab.selection = Some(statement);
                if run_statement {
                    ctrl_enter = true;
                }
            }
        }
        let autocomplete = editor_focused
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::Space));
        if autocomplete {
            let mut names = tab
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect::<Vec<_>>();
            if let Some(c) = tab.connection {
                names.extend(
                    self.tables
                        .get(&c)
                        .into_iter()
                        .flatten()
                        .map(|t| t.name.clone()),
                );
            }
            names.extend(
                [
                    "SELECT", "FROM", "WHERE", "ORDER BY", "GROUP BY", "LIMIT", "JOIN", "COUNT",
                ]
                .map(str::to_owned),
            );
            names.sort();
            names.dedup();
            self.suggestions = Some((tab.id, names));
        }
        if stop {
            tab.cancel.cancel();
        }
        let has_result = tab.result.is_some();
        let connection = tab.connection;

        self.tab_error(ui, index);

        // Drag the line above the results to give the editor more or less room.
        let (splitter, response) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 5.0), egui::Sense::drag());
        if response.hovered() || response.dragged() {
            ui.set_cursor_icon(egui::CursorIcon::ResizeVertical);
            ui.painter().rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(splitter.left(), splitter.bottom() - 2.0),
                    splitter.right_bottom(),
                ),
                0.0,
                p.accent,
            );
        }
        if response.dragged() {
            self.editor_ratio = (self.editor_ratio
                + response.drag_delta().y / (ui.available_height() + height).max(1.0))
            .clamp(0.18, 0.72);
        }

        let actions = self.results_heading(ui, index, false);
        let mut sort = None;
        if has_result {
            sort = self.results_grid(ui, index, editor_focused);
        } else {
            egui::Frame::NONE
                .inner_margin(egui::Margin::same(20))
                .show(ui, |ui| {
                    widgets::help(
                        ui,
                        &p,
                        l(
                            "Uruchom zapytanie, aby zobaczyć wyniki.",
                            "Run a query to see results.",
                        ),
                    );
                });
        }

        if let Some(column) = sort
            && self.tabs[index].table.is_some()
        {
            self.sort_result(index, column);
        }
        if actions.insert {
            self.begin_insert(index);
        }
        if actions.delete {
            self.ask_delete(index);
        }
        if actions.export {
            self.export_result(tabid);
        }
        if run || ctrl_enter {
            self.run_active(true);
        }
        if load {
            self.open_sql(tabid);
        }
        if save {
            self.save_sql(tabid);
        }
        if library {
            self.save_query_name = Some(self.tabs[index].title.clone());
        }
        if let Some(i) = switch_to {
            self.active = i;
        }
        if new_tab {
            self.new_tab(connection);
        }
        if close {
            self.close_tab(tabid);
        }
    }

    /// Open SQL text as a new query in the editor.
    pub(super) fn open_in_editor(&mut self, connection: Option<Uuid>, title: &str, sql: String) {
        self.new_tab(connection);
        let tab = &mut self.tabs[self.active];
        tab.sql = sql;
        if !title.is_empty() {
            tab.title = title.to_owned();
        }
    }
}

/// One muted sentence in the place of a view's contents.
pub(super) fn hint(ui: &mut egui::Ui, p: &theme::Palette, text: &str) {
    egui::Frame::NONE
        .inner_margin(egui::Margin::same(20))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(p.body() - 0.5).color(p.muted));
        });
}

/// Select characters of the editor of a query, and give it the keyboard
/// when asked.
fn select_in_editor(ctx: &egui::Context, tab: Uuid, range: Range<usize>, focus: bool) {
    let id = egui::Id::new(("sql_editor", tab));
    let mut state = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(range.start),
            egui::text::CCursor::new(range.end),
        )));
    state.store(ctx, id);
    if focus {
        ctx.memory_mut(|m| m.request_focus(id));
    }
}

/// The statement around a cursor, in characters: between the `;` that end
/// statements, or the line in a console of commands.
fn statement_at_cursor(
    text: &str,
    cursor: usize,
    dialect: &Dialect,
    console: bool,
) -> Option<Range<usize>> {
    let at = editor_text::byte_of(text, cursor);
    let bytes = if console {
        let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
        let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
        let line = &text[start..end];
        let lead = line.len() - line.trim_start().len();
        let trail = line.len() - line.trim_end().len();
        (start + lead < end - trail).then(|| start + lead..end - trail)?
    } else {
        let ranges = sql::statement_ranges(dialect, text).ok()?;
        sql::statement_at(&ranges, at)?
    };
    Some(editor_text::chars_of(text, bytes))
}

/// The search in the editor: every match gets a background, the current one
/// a stronger one.
fn mark_matches(
    mut job: egui::text::LayoutJob,
    text: &str,
    query: &str,
    current: Option<&Range<usize>>,
    fill: Color32,
    current_fill: Color32,
) -> egui::text::LayoutJob {
    let offsets: Vec<usize> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain([text.len()])
        .collect();
    let marks: Vec<(Range<usize>, Color32)> = editor_text::find_all(text, query)
        .into_iter()
        .map(|m| {
            let color = if current == Some(&m) {
                current_fill
            } else {
                fill
            };
            (offsets[m.start]..offsets[m.end], color)
        })
        .collect();
    if marks.is_empty() {
        return job;
    }
    let mut sections = Vec::with_capacity(job.sections.len() + marks.len() * 2);
    for section in std::mem::take(&mut job.sections) {
        let range = section.byte_range.start.0..section.byte_range.end.0;
        // Marks are in order: only those that overlap this section matter.
        let first = marks.partition_point(|(m, _)| m.end <= range.start);
        let mut cuts = vec![range.start, range.end];
        for (m, _) in marks[first..]
            .iter()
            .take_while(|(m, _)| m.start < range.end)
        {
            cuts.extend([m.start, m.end].into_iter().filter(|e| range.contains(e)));
        }
        cuts.sort_unstable();
        cuts.dedup();
        for (i, pair) in cuts.windows(2).enumerate() {
            let piece = pair[0]..pair[1];
            let mut format = section.format.clone();
            if let Some((_, color)) = marks[first..]
                .iter()
                .find(|(m, _)| m.start <= piece.start && piece.end <= m.end)
            {
                format.background = *color;
            }
            sections.push(egui::text::LayoutSection {
                leading_space: if i == 0 { section.leading_space } else { 0.0 },
                byte_range: piece.start.into()..piece.end.into(),
                format,
            });
        }
    }
    job.sections = sections;
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_statement_at_the_cursor_is_found() {
        let text = "SELECT 1;\nSELECT 'a;b' FROM t;\n\nSELECT 3";
        let dialect = baza_core::drivers::postgres::ENGINE.spec().dialect;
        let second = statement_at_cursor(text, 14, &dialect, false).unwrap();
        assert_eq!(
            text.chars()
                .skip(second.start)
                .take(second.len())
                .collect::<String>(),
            "SELECT 'a;b' FROM t"
        );
        let console = "GET a\n  SET b 1  \nDEL c";
        let line = statement_at_cursor(console, 9, &dialect, true).unwrap();
        assert_eq!(&console[line], "SET b 1");
        assert_eq!(statement_at_cursor("", 0, &dialect, true), None);
    }

    #[test]
    fn matches_are_marked_without_losing_text() {
        let text = "select a; SELECT b";
        let job = highlight(text, 13.0, theme::palette(&Preferences::default()));
        let marked = mark_matches(
            job,
            text,
            "select",
            Some(&(10..16)),
            Color32::RED,
            Color32::BLUE,
        );
        let piece =
            |s: &egui::text::LayoutSection| &marked.text[s.byte_range.start.0..s.byte_range.end.0];
        let joined: String = marked.sections.iter().map(piece).collect();
        assert_eq!(joined, text);
        let colored: Vec<_> = marked
            .sections
            .iter()
            .filter(|s| s.format.background != Color32::TRANSPARENT)
            .map(|s| (piece(s), s.format.background))
            .collect();
        assert_eq!(
            colored,
            [("select", Color32::RED), ("SELECT", Color32::BLUE)]
        );
    }
}
