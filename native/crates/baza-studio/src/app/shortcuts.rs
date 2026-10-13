//! Keyboard shortcuts: the ones of the whole window, the list shown by F1,
//! and the small windows two of them open. "Mod" in
//! the list is Ctrl, and on a Mac Command; Ctrl alone works there as well.
use super::*;
use crate::widgets::Btn;
use eframe::egui::RichText;

/// One line of the list: the keys and what they do.
pub(super) struct Shortcut {
    pub keys: &'static str,
    pub pl: &'static str,
    pub en: &'static str,
}

const fn key(keys: &'static str, pl: &'static str, en: &'static str) -> Shortcut {
    Shortcut { keys, pl, en }
}

/// The list of F1, in groups: Polish name, English name, shortcuts.
pub(super) const GROUPS: &[(&str, &str, &[Shortcut])] = &[
    (
        "Ogólne",
        "General",
        &[
            key("F1", "Ta lista skrótów", "This list of shortcuts"),
            key(
                "Ctrl+Q, Mod+N",
                "Nowe zapytanie (w Danych tabeli i Obiektach mają swoje znaczenie, niżej)",
                "New query (Table data and Objects use them their own way, below)",
            ),
            key(
                "F6",
                "Konsola: nowe zapytanie do bazy z panelu",
                "Console: a new query on the database in focus",
            ),
            key("F8", "Obiekty", "Objects"),
            key(
                "Mod+1 … Mod+8",
                "Sekcje paska, po kolei",
                "The sections of the strip, in order",
            ),
            key(
                "Ctrl+Tab, Ctrl+Shift+Tab",
                "Następna, poprzednia karta",
                "Next, previous tab",
            ),
            key("Mod+W", "Zamknij kartę", "Close the tab"),
            key(
                "F5",
                "Odśwież: dane, listę obiektów albo monitor",
                "Refresh: the data, the list of objects or the monitor",
            ),
            key(
                "Mod+Shift+F",
                "Szukaj w panelu Połączenia",
                "Search the Connections panel",
            ),
            key("Mod+L", "Historia zapytań", "Query history"),
            key("Mod+O", "Otwórz plik SQL", "Open an SQL file"),
            key("Mod+,", "Preferencje", "Preferences"),
            key("F11", "Tryb skupienia", "Focus mode"),
            key(
                "Mod+=, Mod+-, Mod+0",
                "Powiększ, pomniejsz, zwykła wielkość",
                "Zoom in, zoom out, normal size",
            ),
        ],
    ),
    (
        "Edytor SQL",
        "SQL editor",
        &[
            key(
                "Mod+Enter, Mod+R",
                "Uruchom zaznaczenie albo całe zapytanie",
                "Run the selection or the whole query",
            ),
            key(
                "Mod+Shift+R",
                "Uruchom polecenie pod kursorem",
                "Run the statement at the cursor",
            ),
            key(
                "Mod+E",
                "Zaznacz polecenie pod kursorem",
                "Select the statement at the cursor",
            ),
            key("Mod+T", "Zatrzymaj", "Stop"),
            key(
                "Mod+/",
                "Dodaj albo usuń komentarz -- w liniach",
                "Comment or uncomment the lines with --",
            ),
            key(
                "Mod+Shift+U, Mod+Shift+L",
                "Wielkie, małe litery w zaznaczeniu",
                "Upper, lower case of the selection",
            ),
            key(
                "Mod+F, F3, Shift+F3",
                "Szukaj, następny, poprzedni",
                "Find, next, previous",
            ),
            key("Ctrl+Space", "Podpowiedzi nazw", "Suggest names"),
            key(
                "Mod+S",
                "Zapisz w bibliotece zapytań",
                "Save to the query library",
            ),
        ],
    ),
    (
        "Dane tabeli i wyniki",
        "Table data and results",
        &[
            key(
                "Strzałki, Tab, Shift+Tab",
                "Przejdź do sąsiedniej komórki",
                "Move to the next cell",
            ),
            key(
                "Shift+strzałki, Shift+klik",
                "Zaznacz komórki",
                "Mark cells",
            ),
            key(
                "Home, End, Mod+←, Mod+→",
                "Pierwsza, ostatnia kolumna rekordu",
                "First, last column of the record",
            ),
            key(
                "Mod+↑, Mod+↓, Mod+Home, Mod+End",
                "Pierwszy, ostatni rekord strony",
                "First, last record of the page",
            ),
            key(
                "PageUp, PageDown",
                "Ekran w górę, w dół",
                "A screen up, down",
            ),
            key("Enter, F2", "Zmień komórkę", "Edit the cell"),
            key(
                "Mod+Enter, Shift+Enter",
                "Edytor dużej wartości",
                "Large value editor",
            ),
            key(
                "Mod+S, Esc",
                "Zapisz albo porzuć zmianę komórki",
                "Save or discard the change of the cell",
            ),
            key(
                "Pisanie, Enter",
                "Nowa wartość komórki; przy kilku zaznaczonych trafia do wszystkich",
                "A new value of the cell; with several marked it goes into all of them",
            ),
            key(
                "Mod+V",
                "Wklej blok z arkusza; jedna wartość wypełnia zaznaczenie",
                "Paste a block from a spreadsheet; one value fills the mark",
            ),
            key(
                "Delete",
                "Wyczyść zaznaczone komórki",
                "Empty the marked cells",
            ),
            key(
                "Mod+Z, Mod+Shift+Z",
                "Cofnij, ponów zmianę danych",
                "Undo, redo a change of data",
            ),
            key("Insert, Mod+N", "Dodaj rekord", "Add a record"),
            key(
                "Mod+Delete, Mod+Backspace",
                "Usuń zaznaczone rekordy",
                "Delete the marked records",
            ),
            key(
                "Mod+A, Mod+C, Mod+Shift+C",
                "Zaznacz wszystko, kopiuj, kopiuj z nazwami kolumn",
                "Mark all, copy, copy with column names",
            ),
            key(
                "Mod+F, Mod+R",
                "Filtr, zastosuj filtr",
                "Filter, apply the filter",
            ),
            key("Mod+G", "Przejdź do rekordu", "Go to a record"),
            key(
                "Mod+D",
                "Projektuj tabelę (struktura)",
                "Design the table (structure)",
            ),
            key(
                "Ctrl+Q",
                "Zapytanie do tej tabeli w edytorze SQL",
                "A query of this table in the SQL editor",
            ),
            key("Mod+T", "Zatrzymaj wczytywanie", "Stop loading"),
            key("Spacja", "Formularz albo siatka", "Form or grid"),
        ],
    ),
    (
        "Obiekty",
        "Objects",
        &[
            key("Enter, dwuklik", "Otwórz dane", "Open the data"),
            key(
                "↑, ↓, Shift+↑, Shift+↓",
                "Wybierz, rozszerz wybór",
                "Select, extend the selection",
            ),
            key(
                "Mod+klik, Shift+klik",
                "Dodaj do wyboru, wybierz zakres",
                "Add to the selection, select a range",
            ),
            key(
                "Mod+A, Mod+C",
                "Wybierz wszystko, kopiuj nazwy",
                "Select all, copy the names",
            ),
            key("Mod+D", "Projektuj tabelę", "Design the table"),
            key("Ctrl+Q", "Zapytanie do tabeli", "A query of the table"),
            key("Mod+N", "Nowa tabela", "New table"),
            key(
                "Delete",
                "Usuń z bazy, po potwierdzeniu",
                "Drop from the database, after a confirmation",
            ),
            key("Mod+F", "Szukaj po nazwie", "Search by name"),
        ],
    ),
];

/// The keys of a shortcut as this computer names them: "Mod" is Command on a
/// Mac and Ctrl elsewhere; Polish key names get their English ones.
pub(super) fn keys_text(keys: &str, en: bool) -> String {
    let mut text = keys.replace(
        "Mod+",
        if cfg!(target_os = "macos") {
            "⌘+"
        } else {
            "Ctrl+"
        },
    );
    if en {
        for (pl, english) in [
            ("Shift+strzałki", "Shift+arrows"),
            ("Strzałki", "Arrows"),
            ("Spacja", "Space"),
            ("dwuklik", "double-click"),
            ("klik", "click"),
        ] {
            text = text.replace(pl, english);
        }
    }
    text
}

/// Take one press of `key` with exactly these modifiers out of this frame's
/// input, so nothing else acts on it.
pub(super) fn take(ui: &egui::Ui, modifiers: egui::Modifiers, key: egui::Key) -> bool {
    ui.input_mut(|i| {
        let mut found = false;
        i.events.retain(|event| {
            let hit = !found
                && matches!(
                    event,
                    egui::Event::Key { key: k, pressed: true, modifiers: m, .. }
                        if *k == key && m.matches_exact(modifiers)
                );
            found |= hit;
            !hit
        });
        found
    })
}

/// [`take`] with Command (on a Mac) or Ctrl, and Shift when asked.
pub(super) fn take_mod(ui: &egui::Ui, shift: bool, key: egui::Key) -> bool {
    let extra = if shift {
        egui::Modifiers::SHIFT
    } else {
        egui::Modifiers::NONE
    };
    take(ui, egui::Modifiers::COMMAND | extra, key) || take(ui, egui::Modifiers::CTRL | extra, key)
}

impl StudioApp {
    /// Shortcuts of the whole window, read before the section on screen.
    /// The sections that give a key a meaning of their own read it later.
    pub(super) fn global_shortcuts(&mut self, ui: &mut egui::Ui) {
        // Nothing reaches the window behind an open dialog.
        if ui.ctx().memory(|m| m.top_modal_layer()).is_some() {
            return;
        }
        let section = self.section;
        let own = matches!(section, Section::Table | Section::Objects);
        let none = egui::Modifiers::NONE;
        if take(ui, none, egui::Key::F1) {
            self.show_shortcuts = true;
        }
        if take(ui, none, egui::Key::F5) {
            self.refresh_section();
        }
        if take(ui, none, egui::Key::F6) {
            let connection = self.focus_connection();
            self.new_tab(connection);
        }
        if take(ui, none, egui::Key::F8) {
            self.set_section(Section::Objects);
        }
        if take(ui, none, egui::Key::F11) {
            self.focus_mode = !self.focus_mode;
        }
        let numbers = [
            egui::Key::Num1,
            egui::Key::Num2,
            egui::Key::Num3,
            egui::Key::Num4,
            egui::Key::Num5,
            egui::Key::Num6,
            egui::Key::Num7,
            egui::Key::Num8,
        ];
        for (number, section) in numbers.into_iter().zip(Section::ALL) {
            if take_mod(ui, false, number) {
                self.set_section(section);
            }
        }
        if take_mod(ui, false, egui::Key::Comma) {
            self.show_preferences = true;
        }
        if take_mod(ui, false, egui::Key::L) {
            self.set_section(Section::History);
        }
        if take_mod(ui, true, egui::Key::F) {
            self.show_explorer = true;
            self.focus_mode = false;
            ui.memory_mut(|m| m.request_focus(egui::Id::new("nav_search")));
        }
        let ctrl = egui::Modifiers::CTRL;
        if take(ui, ctrl | egui::Modifiers::SHIFT, egui::Key::Tab) {
            self.cycle_tab(false);
        }
        if take(ui, ctrl, egui::Key::Tab) {
            self.cycle_tab(true);
        }
        if take_mod(ui, false, egui::Key::W) {
            self.close_active_tab();
        }
        if take_mod(ui, false, egui::Key::O) {
            self.set_section(Section::Sql);
            self.open_sql(self.tabs[self.active].id);
        }
        // Ctrl+Q is "new query"; on a Mac Command+Q quits, so
        // only the Control key is read. Table data and Objects read Ctrl+Q
        // and Mod+N for their objects.
        if !own && (take(ui, ctrl, egui::Key::Q) || take_mod(ui, false, egui::Key::N)) {
            let connection = self.tabs[self.active]
                .connection
                .or(self.focus_connection());
            self.new_tab(connection);
        }
        if section == Section::Sql && take_mod(ui, false, egui::Key::S) {
            self.save_query_name = Some(self.tabs[self.active].title.clone());
        }
        // Mod+F belongs to the editor, the filter and the object search;
        // elsewhere it searches the side panel.
        if !own && section != Section::Sql && take_mod(ui, false, egui::Key::F) {
            self.show_explorer = true;
            self.focus_mode = false;
            ui.memory_mut(|m| m.request_focus(egui::Id::new("nav_search")));
        }
    }

    /// F5: read again what the section on screen shows.
    pub(super) fn refresh_section(&mut self) {
        match self.section {
            Section::Table => {
                let index = if self.tabs[self.active].kind == TabKind::Table {
                    Some(self.active)
                } else {
                    self.tabs.iter().rposition(|t| t.kind == TabKind::Table)
                };
                if let Some(index) = index {
                    self.tabs[index].total = None;
                    self.browse(index);
                }
            }
            Section::Objects => {
                let connection = self.objects.target.as_ref().map(|t| t.connection);
                self.refresh_tables(connection);
            }
            Section::Monitor => self.refresh_monitor(),
            _ => {
                let connection = self.focus_connection();
                self.refresh_tables(connection);
            }
        }
    }

    /// Ctrl+Tab: the next (or previous) open tab of the section on screen.
    fn cycle_tab(&mut self, forward: bool) {
        let kind = match self.section {
            Section::Sql => TabKind::Query,
            Section::Table => TabKind::Table,
            _ => return,
        };
        let same: Vec<usize> = (0..self.tabs.len())
            .filter(|i| self.tabs[*i].kind == kind)
            .collect();
        let Some(at) = same.iter().position(|i| *i == self.active) else {
            return;
        };
        let next = if forward {
            (at + 1) % same.len()
        } else {
            (at + same.len() - 1) % same.len()
        };
        self.active = same[next];
    }

    /// Mod+W: close the query or the table on screen.
    fn close_active_tab(&mut self) {
        let tab = &self.tabs[self.active];
        let open = match self.section {
            Section::Sql => tab.kind == TabKind::Query,
            Section::Table => tab.kind == TabKind::Table,
            _ => false,
        };
        if open {
            self.close_tab(tab.id);
        }
    }

    /// Ctrl+Q on a table: its first page as a statement in a new query, as
    /// a query of the object.
    pub(super) fn query_object(&mut self, connection: Uuid, table: TableRef) {
        let Some(db) = self.databases.get(&connection).cloned() else {
            self.connect(connection);
            return;
        };
        if self.capabilities(Some(connection)).language == QueryLanguage::None {
            return;
        }
        let mut request = PageRequest::first(table.clone());
        request.limit = self.profiles.preferences.page_limit();
        match db.page_text(&request) {
            Ok(text) => self.open_in_editor(Some(connection), &table.name, text),
            Err(error) => self.notify(error.to_string(), true),
        }
    }

    /// Mod+G: ask for the number of a record of the table on screen.
    pub(super) fn ask_goto(&mut self, index: usize) {
        self.goto_record = Some((self.tabs[index].id, String::new()));
    }

    /// Show a record by its number, counted from 1 over all the pages.
    fn goto(&mut self, id: Uuid, number: u64) {
        let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let limit = u64::from(self.page_limit(index));
        let target = number.saturating_sub(1);
        let page = target / limit;
        let row = (target % limit) as usize;
        let tab = &mut self.tabs[index];
        let rows = tab.result.as_ref().map_or(0, |r| r.rows.len());
        if page == tab.page && row < rows {
            let column = tab.selected_cell.map_or(0, |(_, c)| c);
            tab.selected_row = Some(row);
            tab.selected_cell = Some((row, column));
            tab.range = Some(super::results::GridRange::cell(row, column));
            tab.scroll_to = Some(row);
        } else {
            tab.page = page;
            tab.pending_row = Some(row);
            tab.range = None;
            self.browse(index);
        }
    }

    /// The list of shortcuts (F1) and the "go to record" question (Mod+G).
    pub(super) fn shortcut_windows(&mut self, ctx: &egui::Context) {
        self.shortcuts_window(ctx);
        self.goto_window(ctx);
    }

    fn shortcuts_window(&mut self, ctx: &egui::Context) {
        if !self.show_shortcuts {
            return;
        }
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let mut close = false;
        let dialog = widgets::dialog(
            ctx,
            &p,
            "keyboard_shortcuts",
            l("Skróty klawiaturowe", "Keyboard shortcuts"),
            700.0,
            l("Zamknij", "Close"),
            |ui| {
                widgets::dialog_body(ui, |ui| {
                    if cfg!(target_os = "macos") {
                        widgets::help(
                            ui,
                            &p,
                            l(
                                "⌘ to klawisz Command; zamiast niego działa też Control.",
                                "⌘ is the Command key; Control works in its place too.",
                            ),
                        );
                    }
                    egui::ScrollArea::vertical()
                        .max_height((ctx.content_rect().height() * 0.62).max(200.0))
                        .show(ui, |ui| {
                            for (pl, english, list) in GROUPS {
                                ui.add_space(6.0);
                                widgets::subheading(ui, &p, if en { english } else { pl });
                                egui::Grid::new(("shortcut_group", *pl))
                                    .num_columns(2)
                                    .spacing([26.0, 7.0])
                                    .min_col_width(190.0)
                                    .show(ui, |ui| {
                                        for shortcut in *list {
                                            ui.label(
                                                RichText::new(keys_text(shortcut.keys, en))
                                                    .monospace()
                                                    .size(p.code() - 1.5)
                                                    .color(p.text),
                                            );
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(if en {
                                                        shortcut.en
                                                    } else {
                                                        shortcut.pl
                                                    })
                                                    .size(p.body() - 0.5)
                                                    .color(p.muted),
                                                )
                                                .wrap(),
                                            );
                                            ui.end_row();
                                        }
                                    });
                            }
                        });
                });
                widgets::dialog_footer(ui, &p, |ui| {
                    widgets::right(ui, |ui| {
                        close = Btn::new(l("Zamknij", "Close")).show(ui, &p).clicked();
                    });
                });
            },
        );
        if close || dialog.close {
            self.show_shortcuts = false;
        }
    }

    fn goto_window(&mut self, ctx: &egui::Context) {
        let Some((id, mut text)) = self.goto_record.take() else {
            return;
        };
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let (mut go, mut cancel) = (false, false);
        let dialog = widgets::dialog(
            ctx,
            &p,
            "goto_record",
            l("Przejdź do rekordu", "Go to a record"),
            360.0,
            l("Zamknij", "Close"),
            |ui| {
                widgets::dialog_body(ui, |ui| {
                    widgets::label(ui, &p, l("Numer rekordu", "Record number"));
                    let field = egui::Id::new("goto_record_field");
                    let response = ui.add(
                        widgets::field(&p, &mut text)
                            .id(field)
                            .desired_width(ui.available_width() - 18.0)
                            .hint_text("1"),
                    );
                    if !response.has_focus() && !response.lost_focus() {
                        response.request_focus();
                    }
                    text.retain(|c| c.is_ascii_digit());
                    text.truncate(12);
                    go = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    widgets::help(
                        ui,
                        &p,
                        l(
                            "Liczony od 1 przez wszystkie strony; inna strona wczyta się sama.",
                            "Counted from 1 over all the pages; another page loads by itself.",
                        ),
                    );
                });
                widgets::dialog_footer(ui, &p, |ui| {
                    widgets::right(ui, |ui| {
                        go |= Btn::primary(l("Przejdź", "Go"))
                            .enabled(text.parse::<u64>().is_ok_and(|n| n > 0))
                            .show(ui, &p)
                            .clicked();
                        cancel = Btn::new(l("Anuluj", "Cancel")).show(ui, &p).clicked();
                    });
                });
            },
        );
        if go
            && let Ok(number) = text.parse::<u64>()
            && number > 0
        {
            self.goto(id, number);
        } else if !cancel && !dialog.close {
            self.goto_record = Some((id, text));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_group_has_shortcuts_with_both_languages() {
        for (pl, english, list) in GROUPS {
            assert!(!pl.is_empty() && !english.is_empty() && !list.is_empty());
            for shortcut in *list {
                assert!(
                    !shortcut.keys.is_empty() && !shortcut.pl.is_empty() && !shortcut.en.is_empty()
                );
            }
        }
    }

    #[test]
    fn keys_are_named_for_this_computer() {
        let modifier = if cfg!(target_os = "macos") {
            "⌘+"
        } else {
            "Ctrl+"
        };
        assert_eq!(
            keys_text("Mod+Shift+R", false),
            format!("{modifier}Shift+R")
        );
        assert_eq!(
            keys_text("Ctrl+Q, Mod+N", true),
            format!("Ctrl+Q, {modifier}N")
        );
        assert_eq!(
            keys_text("Shift+strzałki, Spacja", true),
            "Shift+arrows, Space"
        );
        assert_eq!(keys_text("Spacja", false), "Spacja");
    }
}
