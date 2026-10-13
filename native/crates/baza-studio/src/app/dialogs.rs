//! The smaller dialogs: confirmations, the cell and row editors, the table
//! designer, saving a query, groups, SQL suggestions and updates.
use super::*;
use crate::widgets::Btn;
use eframe::egui::{RichText, TextEdit};

/// Where the source code is published, as shown in "O programie": the address
/// the button opens, without its scheme.
fn source_address() -> &'static str {
    updater::SOURCE_URL
        .strip_prefix("https://")
        .unwrap_or(updater::SOURCE_URL)
}

impl StudioApp {
    /// "O programie": the version, the copyright and the licence.
    pub(super) fn about_window(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let (mut source, mut website, mut close) = (false, false, false);
        let dialog = widgets::dialog(
            ctx,
            &p,
            "about",
            l("O programie", "About"),
            520.0,
            l("Zamknij", "Close"),
            |ui| {
                widgets::dialog_body(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        widgets::brand_mark(ui, 34.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 3.0;
                            widgets::heading(ui, &p, "Baza Studio");
                            ui.label(
                                RichText::new(format!(
                                    "{} {}",
                                    l("Wersja", "Version"),
                                    env!("CARGO_PKG_VERSION")
                                ))
                                .size(p.body() - 0.5)
                                .color(p.muted),
                            );
                        });
                    });
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Copyright © 2026 SurMistrz666")
                            .size(p.body())
                            .color(p.text),
                    );
                    widgets::help(
                        ui,
                        &p,
                        l(
                            "Wolne oprogramowanie na licencji GNU General Public License w wersji 3 lub nowszej. Program jest rozpowszechniany BEZ JAKIEJKOLWIEK GWARANCJI. Możesz go używać, zmieniać i rozpowszechniać na warunkach tej licencji.",
                            "Free software under the GNU General Public License, version 3 or later. This program comes with ABSOLUTELY NO WARRANTY. You may use, modify and redistribute it under the terms of that license.",
                        ),
                    );
                    widgets::help(
                        ui,
                        &p,
                        l(
                            "Pełny tekst licencji i informacje o składnikach innych autorów są dołączone do programu: pliki LICENSE i THIRD-PARTY-NOTICES.txt obok aplikacji albo w folderze Resources pakietu.",
                            "The full license text and the third-party notices ship with the application: the files LICENSE and THIRD-PARTY-NOTICES.txt next to the app, or in the Resources folder of the bundle.",
                        ),
                    );
                    ui.add_space(4.0);
                    widgets::label(ui, &p, l("Kod źródłowy", "Source code"));
                    ui.add(
                        egui::Label::new(
                            RichText::new(source_address())
                                .monospace()
                                .size(p.code() - 1.0)
                                .color(p.text),
                        )
                        .selectable(true),
                    );
                });
                widgets::dialog_footer(ui, &p, |ui| {
                    source = Btn::new(l("Kod źródłowy", "Source code"))
                        .icon(Icon::Code)
                        .tip(l(
                            "Otwórz repozytorium w przeglądarce",
                            "Open the repository in the browser",
                        ))
                        .show(ui, &p)
                        .clicked();
                    website = Btn::new(l("Wydania", "Releases"))
                        .icon(Icon::External)
                        .show(ui, &p)
                        .clicked();
                    widgets::right(ui, |ui| {
                        close = Btn::primary(l("Zamknij", "Close"))
                            .min_width(84.0)
                            .show(ui, &p)
                            .clicked();
                    });
                });
            },
        );
        if source {
            self.open_project(updater::SOURCE_URL);
        }
        if website {
            self.open_project(updater::RELEASES_URL);
        }
        if close || dialog.close {
            self.show_about = false;
        }
    }

    pub(super) fn confirmations(&mut self, ctx: &egui::Context) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        if let Some(id) = self.connection_delete {
            let name = self
                .connection_name(id)
                .map(str::to_owned)
                .unwrap_or_default();
            let (mut remove, mut cancel) = (false, false);
            let dialog = widgets::dialog(
                ctx,
                &p,
                "delete_connection",
                l("Usunąć połączenie?", "Delete connection?"),
                440.0,
                l("Zamknij", "Close"),
                |ui| {
                    widgets::dialog_body(ui, |ui| {
                        widgets::subheading(ui, &p, &name);
                        widgets::help(
                            ui,
                            &p,
                            l(
                                "Profil i zapisane w nim hasła zostaną usunięte z tego komputera. Baza danych pozostaje nietknięta.",
                                "The profile and the passwords saved in it are removed from this computer. The database itself is not touched.",
                            ),
                        );
                    });
                    widgets::dialog_footer(ui, &p, |ui| {
                        widgets::right(ui, |ui| {
                            remove = Btn::danger(l("Usuń połączenie", "Delete connection"))
                                .show(ui, &p)
                                .clicked();
                            cancel = Btn::new(l("Anuluj", "Cancel")).show(ui, &p).clicked();
                        });
                    });
                },
            );
            if remove {
                // Close it first: the pool of the database and the SSH tunnel
                // of the connection end with it.
                if self.databases.contains_key(&id)
                    || self.connect_requests.contains_key(&id)
                    || self.tunnel_managers.contains_key(&id)
                {
                    self.disconnect(id);
                }
                self.profiles.connections.retain(|p| p.id != id);
                self.group_operation(GroupOperation::RemoveConnection(id));
                self.save();
                self.tables.remove(&id);
                self.connection_delete = None;
                if self
                    .navigation_selection
                    .as_ref()
                    .and_then(NodeIdentity::connection)
                    == Some(id)
                {
                    self.navigation_selection = None;
                }
            } else if cancel || dialog.close {
                self.connection_delete = None;
            }
        }
        if self.history_clear_confirm {
            let count = self.history.len();
            let copies = self.profiles.preferences.local_sql_history;
            let (mut clear, mut cancel) = (false, false);
            let dialog = widgets::dialog(
                ctx,
                &p,
                "clear_history",
                l("Wyczyścić historię?", "Clear the history?"),
                440.0,
                l("Zamknij", "Close"),
                |ui| {
                    widgets::dialog_body(ui, |ui| {
                        widgets::subheading(
                            ui,
                            &p,
                            &if en {
                                format!(
                                    "{count} {}",
                                    if count == 1 {
                                        "query run"
                                    } else {
                                        "query runs"
                                    }
                                )
                            } else {
                                format!("{}: {count}", "Zapisane uruchomienia zapytań")
                            },
                        );
                        widgets::help(
                            ui,
                            &p,
                            l(
                                "Cała historia zapytań zostanie usunięta z tego komputera. Bazy danych i zapisane zapytania pozostają nietknięte. Tej operacji nie można cofnąć.",
                                "The whole query history is removed from this computer. Databases and saved queries are not touched. This cannot be undone.",
                            ),
                        );
                        if copies {
                            widgets::help(
                                ui,
                                &p,
                                l(
                                    "Lokalne kopie SQL tych zapytań również zostaną usunięte.",
                                    "The local SQL copies of these queries are removed as well.",
                                ),
                            );
                        }
                    });
                    widgets::dialog_footer(ui, &p, |ui| {
                        widgets::right(ui, |ui| {
                            clear = Btn::danger(l("Wyczyść historię", "Clear history"))
                                .show(ui, &p)
                                .clicked();
                            cancel = Btn::new(l("Anuluj", "Cancel")).show(ui, &p).clicked();
                        });
                    });
                },
            );
            if clear {
                self.history_clear_confirm = false;
                self.clear_history();
            } else if cancel || dialog.close {
                self.history_clear_confirm = false;
            }
        }
        if let Some((id, rows)) = self.delete_confirm.clone() {
            let (mut remove, mut cancel) = (false, false);
            let browse = self
                .tabs
                .iter()
                .find(|t| t.id == id)
                .map(|t| self.capabilities(t.connection).browse)
                .unwrap_or(baza_core::engine::BrowseModel::Tables);
            let record = super::results::record_words(browse, self.en());
            let (question, help) = if rows.len() == 1 {
                (record.delete_question.clone(), record.delete_help.clone())
            } else {
                super::results::delete_many_words(browse, rows.len(), en)
            };
            let dialog = widgets::dialog(
                ctx,
                &p,
                "delete_row",
                &question,
                420.0,
                l("Zamknij", "Close"),
                |ui| {
                    widgets::dialog_body(ui, |ui| {
                        widgets::help(ui, &p, &help);
                    });
                    widgets::dialog_footer(ui, &p, |ui| {
                        widgets::right(ui, |ui| {
                            remove = Btn::danger(&record.delete).show(ui, &p).clicked();
                            cancel = Btn::new(l("Anuluj", "Cancel")).show(ui, &p).clicked();
                        });
                    });
                },
            );
            if cancel || dialog.close {
                self.delete_confirm = None;
            }
            if remove {
                self.delete_confirm = None;
                if let Some(tab) = self.tabs.iter().find(|t| t.id == id)
                    && let (Some(result), Some(table), Some(db)) = (
                        &tab.result,
                        &tab.table,
                        tab.connection.and_then(|c| self.databases.get(&c)).cloned(),
                    )
                {
                    let keys = tab
                        .columns
                        .iter()
                        .filter(|c| c.primary_key)
                        .collect::<Vec<_>>();
                    // Each row by its whole key; a row whose key is not in
                    // the result cannot be singled out and is left alone.
                    let (changes, inverses): (Vec<_>, Vec<_>) = rows
                        .iter()
                        .filter_map(|&row| {
                            let values = result.rows.get(row)?;
                            let key = keys
                                .iter()
                                .map(|c| {
                                    let i = result.columns.iter().position(|v| v.name == c.name)?;
                                    Some((c.name.clone(), values[i].clone()))
                                })
                                .collect::<Option<Vec<_>>>()?;
                            let change = (!key.is_empty()).then(|| RowChange::DeleteRow {
                                table: table.clone(),
                                key,
                            })?;
                            // The whole row, to put it back with Mod+Z.
                            let whole: Vec<(String, Cell)> = result
                                .columns
                                .iter()
                                .map(|c| c.name.clone())
                                .zip(values.iter().cloned())
                                .collect();
                            let inverse = super::inverse_change(&change, Some(&whole), &[]);
                            Some((change, inverse))
                        })
                        .unzip();
                    let outcome = if changes.is_empty() {
                        Err(anyhow::anyhow!("primary_key_required"))
                    } else {
                        self.remember_changes(id, &changes, inverses, super::ChangeKind::Do);
                        self.mutate_many(id, db, changes)
                    };
                    if let Err(e) = outcome {
                        self.pending_changes.remove(&id);
                        self.notify(e.to_string(), true);
                    }
                }
            }
        }
    }

    /// Editor of one large cell value. The inline editor of the grid opens it
    /// from its context menu.
    pub(super) fn cell_window(&mut self, ctx: &egui::Context) {
        let Some(mut e) = self.cell_editor.take() else {
            return;
        };
        if e.inline {
            self.cell_editor = Some(e);
            return;
        }
        let Some(tab) = self.tabs.iter().find(|t| t.id == e.tab) else {
            return;
        };
        let Some(result) = tab.result.as_ref() else {
            return;
        };
        let Some(table) = tab.table.clone() else {
            return;
        };
        if result
            .rows
            .get(e.row)
            .and_then(|row| row.get(e.column))
            .is_none()
        {
            return;
        }
        let column = result.columns[e.column].name.clone();
        let pk: Vec<_> = tab
            .columns
            .iter()
            .filter(|c| c.primary_key)
            .filter_map(|c| {
                result
                    .columns
                    .iter()
                    .position(|v| v.name == c.name)
                    .map(|i| (c.name.clone(), result.rows[e.row][i].clone()))
            })
            .collect();
        let db = tab
            .connection
            .and_then(|id| self.databases.get(&id))
            .cloned();
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let (mut save, mut cancel) = (false, false);
        let dialog = widgets::dialog(
            ctx,
            &p,
            "cell_editor",
            &format!("{}: {}", l("Edytuj komórkę", "Edit cell"), column),
            560.0,
            l("Zamknij", "Close"),
            |ui| {
                widgets::dialog_body(ui, |ui| {
                    let (record_pl, record_en) = db
                        .as_ref()
                        .map(|db| db.capabilities().browse.record())
                        .unwrap_or(("wiersz", "row"));
                    widgets::label(
                        ui,
                        &p,
                        &format!(
                            "{} · {} {}",
                            table.name,
                            if en { record_en } else { record_pl },
                            e.row + 1
                        ),
                    );
                    ui.add_enabled(
                        !e.null,
                        TextEdit::multiline(&mut e.value)
                            .font(egui::TextStyle::Monospace)
                            .margin(egui::Margin::symmetric(9, 7))
                            .desired_rows(14)
                            .desired_width(f32::INFINITY),
                    );
                    widgets::checkbox(ui, &p, &mut e.null, "NULL");
                    widgets::help(
                        ui,
                        &p,
                        l(
                            "Zmiana dotyczy wyłącznie tej kolumny. Poprzednia wartość i klucz główny są sprawdzane przed zapisem.",
                            "Only this column changes. Its previous value and the primary key are checked before saving.",
                        ),
                    );
                    if pk.is_empty() {
                        widgets::feedback(
                            ui,
                            &p,
                            l(
                                "Wynik nie zawiera pełnego klucza głównego, więc edycja jest zablokowana.",
                                "The result does not contain the complete primary key, so editing is blocked.",
                            ),
                            true,
                        );
                    }
                });
                widgets::dialog_footer(ui, &p, |ui| {
                    widgets::right(ui, |ui| {
                        save = Btn::primary(l("Zapisz komórkę", "Save cell"))
                            .enabled(!pk.is_empty())
                            .show(ui, &p)
                            .clicked();
                        cancel = Btn::new(l("Anuluj", "Cancel")).show(ui, &p).clicked();
                    });
                });
            },
        );
        if save {
            match self.commit_cell(e) {
                Ok(()) => return,
                Err(err) => {
                    self.notify(err, true);
                    return;
                }
            }
        }
        if !cancel && !dialog.close {
            self.cell_editor = Some(e);
        }
    }

    pub(super) fn insert_window(&mut self, ctx: &egui::Context) {
        let Some(mut state) = self.new_row.take() else {
            return;
        };
        let Some(tab) = self.tabs.iter().find(|t| t.id == state.tab) else {
            return;
        };
        let Some(table) = tab.table.clone() else {
            return;
        };
        let Some(db) = tab.connection.and_then(|c| self.databases.get(&c)).cloned() else {
            return;
        };
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let browse = db.capabilities().browse;
        let record = super::results::record_words(browse, en);
        let (mut save, mut cancel) = (false, false);
        let dialog = widgets::dialog(
            ctx,
            &p,
            "row_insert",
            &format!("{}: {}", record.insert, table.name),
            620.0,
            l("Zamknij", "Close"),
            |ui| {
                widgets::dialog_body(ui, |ui| {
                    widgets::help(
                        ui,
                        &p,
                        if browse == baza_core::engine::BrowseModel::Tables {
                            l(
                                "Pominięte kolumny otrzymają wartości domyślne bazy. NULL jest osobną wartością, inną niż pusty tekst.",
                                "Omitted columns receive the database defaults. NULL is a value of its own, different from empty text.",
                            )
                        } else {
                            l(
                                "Pominięte pola nie zostaną zapisane. NULL jest osobną wartością, inną niż pusty tekst.",
                                "Omitted fields are not written. NULL is a value of its own, different from empty text.",
                            )
                        },
                    );
                    egui::ScrollArea::vertical()
                        .id_salt("insert_fields_scroll")
                        .max_height(360.0)
                        .show(ui, |ui| {
                            egui::Grid::new("insert_fields")
                                .num_columns(4)
                                .spacing([16.0, 8.0])
                                .show(ui, |ui| {
                                    for text in [
                                        l("Kolumna", "Column"),
                                        l("Wartość", "Value"),
                                        "NULL",
                                        l("Pomiń", "Omit"),
                                    ] {
                                        widgets::label(ui, &p, text);
                                    }
                                    ui.end_row();
                                    for (name, value, null, omit) in &mut state.values {
                                        ui.label(
                                            RichText::new(name.as_str())
                                                .size(p.body())
                                                .color(p.text),
                                        );
                                        ui.add_enabled(
                                            !*null && !*omit,
                                            // A grid cell offers only the width its column had
                                            // before, so the width is also the minimum.
                                            widgets::field(&p, value)
                                                .desired_width(300.0)
                                                .min_size(egui::vec2(300.0, p.control_height())),
                                        );
                                        ui.add_enabled_ui(!*omit, |ui| {
                                            widgets::checkbox(ui, &p, null, "");
                                        });
                                        widgets::checkbox(ui, &p, omit, "");
                                        ui.end_row();
                                    }
                                });
                        });
                });
                widgets::dialog_footer(ui, &p, |ui| {
                    widgets::right(ui, |ui| {
                        save = Btn::primary(&record.insert).show(ui, &p).clicked();
                        cancel = Btn::new(l("Anuluj", "Cancel")).show(ui, &p).clicked();
                    });
                });
            },
        );
        if save {
            let result = (|| -> anyhow::Result<(RowChange, Vec<String>)> {
                let values = state
                    .values
                    .iter()
                    .filter(|(_, _, _, omit)| !*omit)
                    .map(|(name, value, null, _)| {
                        let c = tab
                            .columns
                            .iter()
                            .find(|c| c.name == *name)
                            .ok_or_else(|| anyhow::anyhow!("column_not_found"))?;
                        if *null {
                            anyhow::ensure!(c.nullable && !c.primary_key, "column_null_forbidden");
                            return Ok((name.clone(), Cell::Null));
                        }
                        let cell = typed_cell(&c.data_type, value);
                        Ok((name.clone(), cell))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let change = RowChange::InsertRow {
                    table: table.clone(),
                    values,
                };
                let keys: Vec<String> = tab
                    .columns
                    .iter()
                    .filter(|c| c.primary_key)
                    .map(|c| c.name.clone())
                    .collect();
                Ok((change, keys))
            })();
            let result = result.and_then(|(change, keys)| {
                let inverse = super::inverse_change(&change, None, &keys);
                self.remember_changes(
                    state.tab,
                    std::slice::from_ref(&change),
                    vec![inverse],
                    super::ChangeKind::Do,
                );
                self.mutate(state.tab, db, change).inspect_err(|_| {
                    self.pending_changes.remove(&state.tab);
                })
            });
            match result {
                Ok(()) => return,
                Err(e) => self.notify(e.to_string(), true),
            }
        }
        if !cancel && !dialog.close {
            self.new_row = Some(state);
        }
    }

    pub(super) fn saved_query_window(&mut self, ctx: &egui::Context) {
        let Some(mut name) = self.save_query_name.take() else {
            return;
        };
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let (mut save, mut cancel) = (false, false);
        let dialog = widgets::dialog(
            ctx,
            &p,
            "save_query_library",
            l("Zapisz zapytanie", "Save query"),
            440.0,
            l("Zamknij", "Close"),
            |ui| {
                widgets::dialog_body(ui, |ui| {
                    widgets::label(ui, &p, l("Nazwa", "Name"));
                    let r = ui.add(
                        widgets::field(&p, &mut name)
                            .char_limit(120)
                            .desired_width(f32::INFINITY),
                    );
                    if !r.has_focus() {
                        r.request_focus();
                    }
                    widgets::help(
                        ui,
                        &p,
                        l(
                            "Zapytanie trafi do zaszyfrowanej biblioteki i pojawi się w panelu Połączenia.",
                            "The query goes into the encrypted library and appears in the Connections panel.",
                        ),
                    );
                    save = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                });
                widgets::dialog_footer(ui, &p, |ui| {
                    widgets::right(ui, |ui| {
                        save |= Btn::primary(l("Zapisz", "Save"))
                            .enabled(!name.trim().is_empty())
                            .min_width(84.0)
                            .show(ui, &p)
                            .clicked();
                        cancel = Btn::new(l("Anuluj", "Cancel"))
                            .min_width(84.0)
                            .show(ui, &p)
                            .clicked();
                    });
                });
            },
        );
        if save && !name.trim().is_empty() {
            let tab = &self.tabs[self.active];
            self.update_library(
                Some(SavedQuery {
                    id: Uuid::new_v4(),
                    name: name.trim().to_owned(),
                    connection: tab.connection,
                    sql: tab.sql.clone(),
                }),
                None,
            );
        } else if !cancel && !dialog.close {
            self.save_query_name = Some(name);
        }
    }

    /// Names that complete the word before the cursor in the SQL editor.
    pub(super) fn suggestions_window(&mut self, ctx: &egui::Context) {
        let Some((id, names)) = self.suggestions.take() else {
            return;
        };
        let p = self.palette();
        let title = self.tr("Podpowiedzi SQL", "SQL suggestions");
        let close_tip = self.tr("Zamknij", "Close");
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) else {
            return;
        };
        let chars = tab.sql.chars().collect::<Vec<_>>();
        let end = tab
            .selection
            .as_ref()
            .map(|r| r.end)
            .unwrap_or(chars.len())
            .min(chars.len());
        let mut start = end;
        while start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '_') {
            start -= 1;
        }
        let prefix = chars[start..end].iter().collect::<String>().to_lowercase();
        let mut open = true;
        let mut chosen = None;
        let area = ctx.content_rect();
        // A small panel without a title bar, below the toolbar of the editor.
        egui::Window::new(title)
            .id(egui::Id::new("sql_suggestions"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .default_width(260.0)
            .default_pos(egui::pos2(area.center().x - 130.0, area.top() + 150.0))
            .frame(
                egui::Frame::NONE
                    .fill(p.bg)
                    .stroke(egui::Stroke::new(1.0, p.line))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::same(10))
                    .shadow(ctx.global_style().visuals.popup_shadow),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.horizontal(|ui| {
                    widgets::caption(ui, &p, title);
                    widgets::right(ui, |ui| {
                        if widgets::icon_button(ui, &p, Icon::Close, close_tip).clicked() {
                            open = false;
                        }
                    });
                });
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .max_height(320.0)
                    .show(ui, |ui| {
                        ui.set_min_width(230.0);
                        let mut any = false;
                        for name in names
                            .iter()
                            .filter(|n| n.to_lowercase().starts_with(&prefix))
                            .take(100)
                        {
                            any = true;
                            if ui
                                .selectable_label(
                                    false,
                                    RichText::new(name).monospace().size(p.code() - 1.0),
                                )
                                .clicked()
                            {
                                chosen = Some(name.clone());
                            }
                        }
                        if !any {
                            ui.label(
                                RichText::new(if self.profiles.preferences.language == "en" {
                                    "No name starts with this text."
                                } else {
                                    "Żadna nazwa nie zaczyna się od tego tekstu."
                                })
                                .size(p.small() + 0.5)
                                .color(p.muted),
                            );
                        }
                    });
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            open = false;
        }
        if let Some(name) = chosen {
            let mut result = chars[..start].iter().collect::<String>();
            result.push_str(&name);
            result.extend(chars[end..].iter());
            tab.sql = result;
            tab.table = None;
            tab.selection = None;
        } else if open {
            self.suggestions = Some((id, names));
        }
    }

    pub(super) fn group_window(&mut self, ctx: &egui::Context) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        if let Some(id) = self.group_remove {
            let name = self.group_path(id);
            let (mut confirm, mut cancel) = (false, false);
            let dialog = widgets::dialog(
                ctx,
                &p,
                "delete_group",
                l("Usunąć grupę?", "Delete group?"),
                440.0,
                l("Zamknij", "Close"),
                |ui| {
                    widgets::dialog_body(ui, |ui| {
                        widgets::subheading(ui, &p, &name);
                        widgets::help(
                            ui,
                            &p,
                            l(
                                "Połączenia pozostaną zapisane. Podgrupy przejdą poziom wyżej, a połączenia tej grupy będą dostępne bez grupy.",
                                "Connections stay saved. Subgroups move up one level, and the connections of this group become ungrouped.",
                            ),
                        );
                    });
                    widgets::dialog_footer(ui, &p, |ui| {
                        widgets::right(ui, |ui| {
                            confirm = Btn::danger(l("Usuń grupę", "Delete group"))
                                .enabled(!self.group_busy)
                                .show(ui, &p)
                                .clicked();
                            cancel = Btn::new(l("Anuluj", "Cancel")).show(ui, &p).clicked();
                        });
                    });
                },
            );
            if confirm {
                self.group_operation(GroupOperation::Remove(id));
            }
            if cancel || dialog.close {
                self.group_remove = None;
            }
        }
        let Some(mut draft) = self.group_dialog.take() else {
            return;
        };
        let (mut save, mut cancel) = (false, false);
        let dialog = widgets::dialog(
            ctx,
            &p,
            "group_editor",
            if draft.id.is_some() {
                l("Edytuj grupę", "Edit group")
            } else {
                l("Nowa grupa", "New group")
            },
            440.0,
            l("Zamknij", "Close"),
            |ui| {
                widgets::dialog_body(ui, |ui| {
                    widgets::label(ui, &p, l("Nazwa", "Name"));
                    ui.add(
                        widgets::field(&p, &mut draft.name)
                            .desired_width(f32::INFINITY)
                            .char_limit(64),
                    );
                    ui.add_space(4.0);
                    widgets::label(ui, &p, l("Grupa nadrzędna", "Parent group"));
                    let selected = draft
                        .parent
                        .map(|id| self.group_path(id))
                        .unwrap_or_else(|| l("Poziom główny", "Root level").into());
                    widgets::combo(ui, &p, "group_parent", selected, 380.0, |ui| {
                        ui.selectable_value(
                            &mut draft.parent,
                            None,
                            l("Poziom główny", "Root level"),
                        );
                        for g in &self.group_snapshot.groups {
                            if Some(g.id) != draft.id {
                                ui.selectable_value(
                                    &mut draft.parent,
                                    Some(g.id),
                                    self.group_path(g.id),
                                );
                            }
                        }
                    });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(l("Kolor", "Colour"))
                                .size(p.small())
                                .color(p.muted),
                        );
                        ui.color_edit_button_srgb(&mut draft.color);
                    });
                });
                widgets::dialog_footer(ui, &p, |ui| {
                    widgets::right(ui, |ui| {
                        save = Btn::primary(l("Zapisz", "Save"))
                            .enabled(!self.group_busy && !draft.name.trim().is_empty())
                            .min_width(84.0)
                            .show(ui, &p)
                            .clicked();
                        cancel = Btn::new(l("Anuluj", "Cancel"))
                            .min_width(84.0)
                            .show(ui, &p)
                            .clicked();
                    });
                });
            },
        );
        if save {
            self.group_operation(if draft.id.is_some() {
                GroupOperation::Rename(draft.clone())
            } else {
                GroupOperation::Create(draft.clone())
            });
        }
        if !cancel && !dialog.close {
            self.group_dialog = Some(draft);
        }
    }

    pub(super) fn distribution_window(&mut self, ctx: &egui::Context) {
        if !self.show_distribution {
            return;
        }
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let (mut check, mut browser, mut verify, mut close) = (false, false, false, false);
        let dialog = widgets::dialog(
            ctx,
            &p,
            "distribution",
            l("Aktualizacje", "Updates"),
            600.0,
            l("Zamknij", "Close"),
            |ui| {
                widgets::dialog_body(ui, |ui| {
                    ui.horizontal(|ui| {
                        widgets::brand_mark(ui, 20.0);
                        widgets::subheading(
                            ui,
                            &p,
                            &format!("Baza Studio {}", env!("CARGO_PKG_VERSION")),
                        );
                    });
                    widgets::help(
                        ui,
                        &p,
                        l(
                            "Sprawdzenie pyta GitHuba o listę wydań, bez konta i bez identyfikatora instalacji. Pakiet pobierasz samodzielnie w przeglądarce ze strony wydania.",
                            "Checking asks GitHub for the list of releases, with no account and no installation identifier. You download the package yourself in the browser from the release page.",
                        ),
                    );
                    if let Some(message) = &self.update_message {
                        widgets::feedback(ui, &p, message, false);
                    }
                    if let Some(offer) = &self.update_offer {
                        ui.add_space(4.0);
                        widgets::hline(ui, &p);
                        ui.add_space(4.0);
                        let heading = if offer.size > 0 {
                            format!(
                                "{} {} · {:.1} MiB",
                                l("Wersja", "Version"),
                                offer.version,
                                offer.size as f64 / 1024.0 / 1024.0
                            )
                        } else {
                            format!("{} {}", l("Wersja", "Version"), offer.version)
                        };
                        widgets::subheading(ui, &p, &heading);
                        if !offer.notes.is_empty() {
                            widgets::help(ui, &p, &offer.notes);
                        }
                        if !offer.sha256.is_empty() {
                            widgets::label(
                                ui,
                                &p,
                                l(
                                    "Suma SHA256 pakietu podana przez GitHub",
                                    "SHA256 of the package as GitHub reports it",
                                ),
                            );
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&offer.sha256)
                                        .monospace()
                                        .size(p.code() - 2.0)
                                        .color(p.text),
                                )
                                .wrap(),
                            );
                        }
                        ui.horizontal(|ui| {
                            browser = Btn::new(l("Otwórz stronę wydania", "Open the release page"))
                                .icon(Icon::External)
                                .show(ui, &p)
                                .clicked();
                            verify =
                                Btn::new(l("Sprawdź pobrany plik…", "Verify a downloaded file…"))
                                    .enabled(!self.distribution_busy && !offer.sha256.is_empty())
                                    .show(ui, &p)
                                    .clicked();
                        });
                    }
                });
                widgets::dialog_footer(ui, &p, |ui| {
                    if self.distribution_busy {
                        ui.spinner();
                    }
                    widgets::right(ui, |ui| {
                        check = Btn::primary(l("Sprawdź aktualizacje", "Check for updates"))
                            .enabled(!self.distribution_busy && self.updater.is_some())
                            .show(ui, &p)
                            .clicked();
                        close = Btn::new(l("Zamknij", "Close")).show(ui, &p).clicked();
                    });
                });
            },
        );
        self.show_distribution = !(close || dialog.close);
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        if check {
            if let Some(updater) = self.updater.clone() {
                self.distribution_busy = true;
                self.update_message = None;
                self.track(async move {
                    let r = updater.check().await.map_err(|e| e.to_string());
                    let _ = tx.send(Message::Update(r)).await;
                    ctx.request_repaint();
                });
            }
        } else if browser {
            let opened = match &self.update_offer {
                Some(offer) => offer.open_page(),
                None => updater::open_downloads_page(),
            };
            if let Err(error) = opened {
                self.update_message = Some(updater::describe(&error.to_string(), en).into());
            }
        } else if verify && let Some(offer) = self.update_offer.clone() {
            self.distribution_busy = true;
            self.track(async move {
                let r = match rfd::AsyncFileDialog::new().pick_file().await {
                    Some(file) => {
                        let path = file.path().to_owned();
                        tokio::task::spawn_blocking(move || offer.matches_file(&path))
                            .await
                            .map_err(|_| "update_file_unreadable".to_owned())
                            .and_then(|r| r.map_err(|e| e.to_string()))
                            .map(Some)
                    }
                    None => Ok(None),
                };
                let _ = tx.send(Message::UpdateFileChecked(r)).await;
                ctx.request_repaint();
            });
        }
    }
}

/// The typed value of a field of the insert dialog. Only a plain numeric type
/// (with an optional size, unsigned or nullable wrapper) is sent as a number;
/// anything else (intervals, arrays, maps, points, mixed types) goes as text
/// and the driver converts it to the column's type.
pub(super) fn typed_cell(data_type: &str, value: &str) -> Cell {
    let mut ty = data_type.trim().to_ascii_lowercase();
    for wrapper in ["nullable(", "lowcardinality("] {
        if let Some(inner) = ty.strip_prefix(wrapper).and_then(|t| t.strip_suffix(')')) {
            ty = inner.to_owned();
        }
    }
    let base = ty
        .split(['(', ' '])
        .next()
        .unwrap_or_default()
        .trim_end_matches("unsigned")
        .to_owned();
    const INTEGERS: [&str; 22] = [
        "int",
        "integer",
        "smallint",
        "tinyint",
        "mediumint",
        "bigint",
        "int2",
        "int4",
        "int8",
        "serial",
        "bigserial",
        "smallserial",
        "int16",
        "int32",
        "int64",
        "int128",
        "int256",
        "uint8",
        "uint16",
        "uint32",
        "uint64",
        "hugeint",
    ];
    const DECIMALS: [&str; 4] = ["decimal", "numeric", "number", "money"];
    const FLOATS: [&str; 7] = [
        "real", "float", "double", "float4", "float8", "float32", "float64",
    ];
    if INTEGERS.contains(&base.as_str()) {
        Cell::Integer(value.to_owned())
    } else if DECIMALS.contains(&base.as_str()) {
        Cell::Decimal(value.to_owned())
    } else if FLOATS.contains(&base.as_str()) || ty == "double precision" {
        Cell::Float(value.to_owned())
    } else {
        Cell::Text(value.to_owned())
    }
}

#[cfg(test)]
mod typed_cell_tests {
    use super::*;

    #[test]
    fn only_plain_numeric_types_become_numbers() {
        assert!(matches!(typed_cell("INTEGER", "1"), Cell::Integer(_)));
        assert!(matches!(
            typed_cell("int(11) unsigned", "1"),
            Cell::Integer(_)
        ));
        assert!(matches!(
            typed_cell("Nullable(Int32)", "1"),
            Cell::Integer(_)
        ));
        assert!(matches!(
            typed_cell("numeric(10,2)", "1.5"),
            Cell::Decimal(_)
        ));
        assert!(matches!(
            typed_cell("double precision", "1.5"),
            Cell::Float(_)
        ));
        for other in [
            "INTERVAL",
            "INTEGER[]",
            "Point",
            "Map(String, Int32)",
            "interval day to second",
            "text",
        ] {
            assert!(matches!(typed_cell(other, "x"), Cell::Text(_)), "{other}");
        }
    }
}
