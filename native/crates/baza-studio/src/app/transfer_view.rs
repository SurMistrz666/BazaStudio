//! "Import i eksport": rows from a file into a table, a result into a file,
//! SQL files and connection profiles.
use super::*;
use crate::widgets::Btn;
use eframe::egui::RichText;

impl StudioApp {
    /// The result the section works on: the chosen one, else the table or
    /// query on screen, else the newest table, then the newest query. The
    /// result of the query builder comes last: it is a summary nobody opened
    /// to move data, and rows can be imported into a table only.
    fn transfer_tab(&self) -> Option<usize> {
        let usable = |tab: &QueryTab| tab.result.is_some();
        let newest = |kind: TabKind| {
            self.tabs
                .iter()
                .rposition(|tab| tab.kind == kind && usable(tab))
        };
        let active = &self.tabs[self.active];
        self.transfer_choice
            .and_then(|id| self.tabs.iter().position(|tab| tab.id == id && usable(tab)))
            .or_else(|| (usable(active) && active.kind != TabKind::Builder).then_some(self.active))
            .or_else(|| newest(TabKind::Table))
            .or_else(|| newest(TabKind::Query))
            .or_else(|| newest(TabKind::Builder))
    }

    pub(super) fn transfer_view(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let chosen = self.transfer_tab();
        let choices = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.result.is_some())
            .map(|(i, tab)| {
                let rows = tab.result.as_ref().map_or(0, |r| r.rows.len());
                // Say what each entry is; a bare title does not tell a table
                // from a query, and the builder has no title of its own.
                let name = match tab.kind {
                    TabKind::Table => format!("{}: {}", l("Tabela", "Table"), tab.title),
                    TabKind::Query => format!("{}: {}", l("Zapytanie", "Query"), tab.title),
                    TabKind::Builder => match &self.builder {
                        Some(builder) => format!(
                            "{}: {}",
                            l("Kreator zapytań", "Query builder"),
                            builder.table.name
                        ),
                        None => l("Kreator zapytań", "Query builder").to_owned(),
                    },
                };
                (i, tab.id, format!("{name} · {rows} {}", row_word(rows, en)))
            })
            .collect::<Vec<_>>();
        let (mut import, mut export, mut open_sql, mut save_sql) = (false, false, false, false);
        let (mut import_profiles, mut export_profiles, mut to_tables) = (false, false, false);
        egui::ScrollArea::vertical()
            .id_salt("transfer_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::NONE
                    .inner_margin(egui::Margin::same(20))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 8.0;
                        widgets::pane_heading(
                            ui,
                            &p,
                            l("Import i eksport", "Import and export"),
                            l(
                                "Wczytaj wiersze z pliku CSV, TSV lub JSON do tabeli albo zapisz wynik do pliku. Import zawsze pokazuje podgląd, zanim cokolwiek zapisze. Limit pliku: 5 MB.",
                                "Load rows from a CSV, TSV or JSON file into a table, or save a result to a file. An import always shows a preview before it writes anything. File limit: 5 MB.",
                            ),
                            |_| {},
                        );
                        if self.import.is_some() {
                            self.import_preview(ui);
                            ui.add_space(12.0);
                            widgets::hline(ui, &p);
                            ui.add_space(12.0);
                        }
                        widgets::label(ui, &p, l("Dane do importu lub eksportu", "Data to import into or export"));
                        if choices.is_empty() {
                            widgets::help(
                                ui,
                                &p,
                                l(
                                    "Żadna tabela ani zapytanie nie ma jeszcze wyniku. Otwórz tabelę w sekcji Dane tabeli albo uruchom zapytanie.",
                                    "No table or query has a result yet. Open a table in Table data or run a query.",
                                ),
                            );
                            to_tables = Btn::new(Section::Table.label(en))
                                .small()
                                .show(ui, &p)
                                .clicked();
                        } else {
                            widgets::combo(
                                ui,
                                &p,
                                "transfer_choice",
                                chosen
                                    .and_then(|index| {
                                        choices.iter().find(|(i, _, _)| *i == index)
                                    })
                                    .map_or_else(String::new, |(_, _, text)| text.clone()),
                                320.0_f32.min(ui.available_width() - 30.0),
                                |ui| {
                                    for (i, id, text) in &choices {
                                        if ui.selectable_label(chosen == Some(*i), text).clicked() {
                                            self.transfer_choice = Some(*id);
                                        }
                                    }
                                },
                            );
                        }
                        ui.add_space(14.0);

                        let target = chosen.filter(|index| self.tabs[*index].table.is_some());
                        widgets::subheading(ui, &p, l("Import danych do tabeli", "Import data into a table"));
                        match (chosen, target) {
                            (_, Some(index)) => {
                                widgets::help(
                                    ui,
                                    &p,
                                    &format!(
                                        "{} {}. {}",
                                        l("Wiersze trafią do tabeli", "The rows go into the table"),
                                        self.tabs[index].title,
                                        l(
                                            "Import wykonuje wyłącznie INSERT, a wszystkie wiersze są zapisywane razem w jednej transakcji.",
                                            "An import only runs INSERT, and all rows are written together in one transaction.",
                                        )
                                    ),
                                );
                            }
                            (Some(_), None) => {
                                widgets::help(
                                    ui,
                                    &p,
                                    l(
                                        "Wiersze można wczytać tylko do tabeli z kluczem głównym. Otwórz ją w sekcji Dane tabeli i wybierz powyżej; wybrany wynik można wyeksportować.",
                                        "Rows can be loaded only into a table with a primary key. Open it in Table data and choose it above; the chosen result can be exported.",
                                    ),
                                );
                            }
                            (None, None) => {
                                widgets::help(
                                    ui,
                                    &p,
                                    l(
                                        "Otwórz tabelę, do której chcesz wczytać wiersze.",
                                        "Open the table you want to load rows into.",
                                    ),
                                );
                            }
                        }
                        import = Btn::new(l("Sprawdź import", "Review import"))
                            .icon(Icon::Upload)
                            .enabled(target.is_some())
                            .tip(l(
                                "Wybierz plik CSV, TSV albo JSON i zobacz podgląd",
                                "Choose a CSV, TSV or JSON file and see a preview",
                            ))
                            .show(ui, &p)
                            .clicked();

                        ui.add_space(14.0);
                        widgets::hline(ui, &p);
                        ui.add_space(14.0);
                        widgets::subheading(ui, &p, l("Eksport wyniku", "Export a result"));
                        widgets::help(
                            ui,
                            &p,
                            l(
                                "Eksport CSV, TSV, JSON, XML lub SQL zawiera wiersze widoczne w wyniku. Plik zapisujesz na tym komputerze; nie jest wysyłany na serwer.",
                                "A CSV, TSV, JSON, XML or SQL export contains the rows shown in the result. You save the file on this computer; it is not sent to a server.",
                            ),
                        );
                        ui.horizontal(|ui| {
                            widgets::combo(
                                ui,
                                &p,
                                "transfer_format",
                                self.export_format.label(),
                                70.0,
                                |ui| {
                                    for f in [
                                        Format::Csv,
                                        Format::Tsv,
                                        Format::Json,
                                        Format::Xml,
                                        Format::Sql,
                                    ] {
                                        ui.selectable_value(&mut self.export_format, f, f.label());
                                    }
                                },
                            );
                            export = Btn::new(l("Eksportuj wynik", "Export results"))
                                .icon(Icon::Download)
                                .enabled(chosen.is_some())
                                .show(ui, &p)
                                .clicked();
                        });

                        ui.add_space(14.0);
                        widgets::hline(ui, &p);
                        ui.add_space(14.0);
                        widgets::subheading(ui, &p, l("Pliki SQL", "SQL files"));
                        widgets::help(
                            ui,
                            &p,
                            l(
                                "Otwórz plik .sql w edytorze albo zapisz do pliku zapytanie, nad którym pracujesz.",
                                "Open an .sql file in the editor, or save the query you are working on to a file.",
                            ),
                        );
                        ui.horizontal(|ui| {
                            open_sql = Btn::new(l("Otwórz plik SQL…", "Open SQL file…"))
                                .icon(Icon::File)
                                .show(ui, &p)
                                .clicked();
                            save_sql = Btn::new(l("Zapisz plik SQL…", "Save SQL file…"))
                                .icon(Icon::Save)
                                .show(ui, &p)
                                .clicked();
                        });

                        ui.add_space(14.0);
                        widgets::hline(ui, &p);
                        ui.add_space(14.0);
                        widgets::subheading(ui, &p, l("Profile połączeń", "Connection profiles"));
                        widgets::help(
                            ui,
                            &p,
                            l(
                                "Przenieś połączenia na inny komputer. Zaszyfrowany plik BZX może zawierać hasła i klucze; plik JSON zawiera same ustawienia.",
                                "Move connections to another computer. An encrypted BZX file can carry passwords and keys; a JSON file carries settings only.",
                            ),
                        );
                        ui.horizontal(|ui| {
                            import_profiles = Btn::new(l("Importuj profile…", "Import profiles…"))
                                .icon(Icon::Upload)
                                .show(ui, &p)
                                .clicked();
                            export_profiles = Btn::new(l("Eksportuj profile…", "Export profiles…"))
                                .icon(Icon::Download)
                                .enabled(!self.profiles.connections.is_empty())
                                .show(ui, &p)
                                .clicked();
                        });
                    });
            });
        let query_tab = |app: &mut Self| {
            app.set_section(Section::Sql);
            app.tabs[app.active].id
        };
        if let (true, Some(index)) = (import, chosen) {
            self.import_data(self.tabs[index].id);
        }
        if let (true, Some(index)) = (export, chosen) {
            self.export_result(self.tabs[index].id);
        }
        if open_sql {
            let id = query_tab(self);
            self.open_sql(id);
        }
        if save_sql {
            let id = self
                .tabs
                .iter()
                .rposition(|tab| tab.kind == TabKind::Query)
                .map_or(self.tabs[self.active].id, |index| self.tabs[index].id);
            self.save_sql(id);
        }
        if import_profiles {
            self.section = Section::Connections;
            self.open_profile_file();
        }
        if export_profiles {
            self.section = Section::Connections;
            self.export_profiles_dialog();
        }
        if to_tables {
            self.set_section(Section::Table);
        }
    }

    /// Preview of a file chosen for import: map its headers to the columns of
    /// the table, look at the first rows, then import or cancel.
    fn import_preview(&mut self, ui: &mut egui::Ui) {
        let Some(mut state) = self.import.take() else {
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
        let columns = tab.columns.clone();
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let (mut apply, mut cancel) = (false, false);
        let unknown = state
            .headers
            .iter()
            .filter(|h| !columns.iter().any(|c| &c.name == *h))
            .cloned()
            .collect::<Vec<_>>();
        egui::Frame::NONE
            .stroke(egui::Stroke::new(1.0, p.line))
            .inner_margin(egui::Margin::same(16))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 8.0;
                widgets::subheading(ui, &p, &format!("{} → {}", state.path, table.name));
                widgets::help(
                    ui,
                    &p,
                    &format!(
                        "{} {} · {} {}. {}",
                        state.rows.len(),
                        row_word(state.rows.len(), en),
                        state.headers.len(),
                        l("kolumn", "columns"),
                        l(
                            "Import wykona wyłącznie INSERT. Wszystkie wiersze zostaną zapisane razem w jednej transakcji. \\N oznacza NULL.",
                            "The import only runs INSERT. All rows are written together in one transaction. \\N means NULL.",
                        )
                    ),
                );
                widgets::label(ui, &p, l("Dopasuj nazwy kolumn", "Map column names"));
                ui.horizontal_wrapped(|ui| {
                    for (i, h) in state.headers.iter_mut().enumerate() {
                        widgets::combo(ui, &p, ("importmap", i), h.clone(), 130.0, |ui| {
                            for c in &columns {
                                ui.selectable_value(h, c.name.clone(), &c.name);
                            }
                        });
                    }
                });
                if !unknown.is_empty() {
                    widgets::feedback(
                        ui,
                        &p,
                        &format!(
                            "{}: {}",
                            l(
                                "Tych nagłówków nie ma w tabeli",
                                "These headers are not in the table"
                            ),
                            unknown.join(", ")
                        ),
                        true,
                    );
                }
                egui::ScrollArea::both()
                    .id_salt("import_preview_scroll")
                    .max_height(240.0)
                    .show(ui, |ui| {
                        egui::Grid::new("import_preview")
                            .striped(true)
                            .spacing([18.0, 7.0])
                            .show(ui, |ui| {
                                for h in &state.headers {
                                    ui.label(
                                        RichText::new(h)
                                            .font(theme::medium(p.body() - 0.5))
                                            .color(p.muted),
                                    );
                                }
                                ui.end_row();
                                for row in state.rows.iter().take(30) {
                                    for c in row {
                                        ui.label(
                                            RichText::new(
                                                c.display().chars().take(120).collect::<String>(),
                                            )
                                            .monospace()
                                            .size(p.code() - 1.5),
                                        );
                                    }
                                    ui.end_row();
                                }
                            });
                    });
                ui.horizontal(|ui| {
                    apply = Btn::primary(l("Importuj wiersze", "Import rows"))
                        .icon(Icon::Check)
                        .enabled(unknown.is_empty() && !state.rows.is_empty())
                        .show(ui, &p)
                        .clicked();
                    cancel = Btn::new(l("Anuluj", "Cancel")).show(ui, &p).clicked();
                });
            });
        if apply {
            let tx = self.tx.clone();
            let ctx = self.ctx.clone();
            let id = state.tab;
            let history_connection = tab.connection.unwrap();
            let vault = self.vault.clone();
            self.track(async move {
                let r = async {
                    let h = HistoryRecord {
                        id: Uuid::new_v4(),
                        connection_id: history_connection,
                        sql: format!(
                            "-- Data import: {} rows into {}",
                            state.rows.len(),
                            table.name
                        ),
                        status: "running".into(),
                        started_at: chrono::Utc::now().to_rfc3339(),
                        duration_ms: 0,
                        rows: 0,
                        error: None,
                    };
                    if let Some(v) = vault.clone() {
                        let h = h.clone();
                        tokio::task::spawn_blocking(move || v.append_history(&h))
                            .await
                            .map_err(|_| anyhow::anyhow!("history_task_failed"))??;
                    }
                    let result = db
                        .import_rows(
                            &table,
                            &state.headers,
                            &state.rows,
                            CancellationToken::new(),
                        )
                        .await;
                    let mut done = h;
                    done.status = if result.is_ok() {
                        "completed"
                    } else {
                        "failed"
                    }
                    .into();
                    done.rows = result.as_ref().map(|r| r.affected).unwrap_or(0);
                    done.error = result.as_ref().err().map(|e| e.to_string());
                    if let Some(v) = vault {
                        tokio::task::spawn_blocking(move || v.append_history(&done))
                            .await
                            .map_err(|_| anyhow::anyhow!("history_task_failed"))??;
                    }
                    result.map(|_| ())
                }
                .await
                .map_err(|e| e.to_string());
                let _ = tx.send(Message::Mutation(id, r)).await;
                ctx.request_repaint();
            });
            self.notify(
                self.tr("Importowanie wierszy…", "Importing rows…").into(),
                false,
            );
            return;
        }
        if !cancel {
            self.import = Some(state);
        }
    }
}
