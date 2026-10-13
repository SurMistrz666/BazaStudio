//! "Kreator zapytań": build a SELECT from the columns, a summary and a group
//! of one table, see its SQL and run it.
use super::*;
use crate::widgets::Btn;
use baza_core::builder::{Projection, QueryBuilder};
use eframe::egui::RichText;

/// The SELECT the choices of the builder stand for.
pub(super) fn builder_sql(b: &BuilderUi, dialect: &Dialect) -> anyhow::Result<String> {
    let mut model = QueryBuilder::table(b.table.clone());
    for name in &b.selected {
        model.projections.push(Projection {
            column: name.clone(),
            aggregate: None,
            alias: None,
        });
    }
    if !b.aggregate.is_empty() {
        model.projections.push(Projection {
            column: b.aggregate_column.clone(),
            aggregate: Some(b.aggregate.clone()),
            alias: Some("result".into()),
        });
    }
    if !b.group_column.is_empty() {
        model.group_by.push(b.group_column.clone());
    }
    model.sql(dialect)
}

impl StudioApp {
    /// The hidden tab that runs the built query, created on first use.
    fn builder_tab(&mut self) -> Uuid {
        if let Some(tab) = self.tabs.iter().find(|tab| tab.kind == TabKind::Builder) {
            return tab.id;
        }
        let mut tab = QueryTab::new(None);
        tab.kind = TabKind::Builder;
        tab.title = self.tr("Kreator", "Builder").into();
        tab.sql.clear();
        let id = tab.id;
        self.tabs.push(tab);
        id
    }

    /// Start the builder on a table and bring its section forward.
    pub(super) fn open_builder(&mut self, connection: Uuid, table: TableRef) {
        if !self.capabilities(Some(connection)).query_builder {
            let text = self.unavailable(
                Some(connection),
                "kreator zapytań nie jest dostępny dla tej bazy.",
                "the query builder is not available for this database.",
            );
            self.notify(text, true);
            return;
        }
        let tab = self.builder_tab();
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab) {
            tab.cancel.cancel();
            tab.request = Uuid::new_v4();
            tab.busy = false;
            tab.connection = Some(connection);
            tab.sql.clear();
            tab.error = None;
            tab.table = None;
            tab.columns.clear();
            if let Some(mut result) = tab.result.take() {
                for row in &mut result.rows {
                    for cell in row {
                        wipe_cell(cell);
                    }
                }
            }
        }
        let request = Uuid::new_v4();
        let db = self.databases.get(&connection).cloned();
        self.builder = Some(BuilderUi {
            tab,
            connection,
            table: table.clone(),
            columns: vec![],
            request,
            loading: db.is_some(),
            loaded: false,
            selected: vec![],
            aggregate: String::new(),
            aggregate_column: "*".into(),
            group_column: String::new(),
        });
        self.section = Section::Builder;
        if let Some(index) = self.tabs.iter().position(|t| t.id == tab) {
            self.active = index;
        }
        let Some(db) = db else {
            self.connect(connection);
            return;
        };
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let result = db.columns(&table).await.map_err(|e| e.to_string());
            let _ = tx.send(Message::BuilderColumns(request, result)).await;
            ctx.request_repaint();
        });
    }

    pub(super) fn builder_view(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let Some(mut b) = self.builder.take() else {
            self.builder_empty(ui);
            return;
        };
        let Some(index) = self.tabs.iter().position(|tab| tab.id == b.tab) else {
            // The application was locked in between; start over.
            self.builder_empty(ui);
            return;
        };
        self.active = index;
        // The dialect the connection settled on, or the one of its profile
        // while it is not connected.
        let dialect = self
            .databases
            .get(&b.connection)
            .map(|db| *db.dialect())
            .or_else(|| {
                self.profiles
                    .connections
                    .iter()
                    .find(|profile| profile.id == b.connection)
                    .map(|profile| *profile.dialect())
            });
        let Some(dialect) = dialect else {
            // The connection was deleted.
            self.builder_empty(ui);
            return;
        };
        let connected = self.databases.contains_key(&b.connection);
        if connected && !b.loaded && !b.loading {
            // Connected after the builder was opened: read the columns now.
            let (connection, table) = (b.connection, b.table.clone());
            self.open_builder(connection, table);
            self.builder_view(ui);
            return;
        }
        let tables = self.tables.get(&b.connection).cloned().unwrap_or_default();
        let preview = builder_sql(&b, &dialect);
        let busy = self.tabs[index].busy;
        let (mut run, mut open_editor) = (false, false);
        let mut switch_table = None;
        widgets::toolbar(ui, &p, |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let run_button = Btn::primary(l("Pokaż wynik", "Show result"))
                .icon(Icon::Play)
                .enabled(preview.is_ok() && !busy);
            let room = ui.available_width() - run_button.width(ui, &p) - 40.0;
            let wide = room > 660.0;
            let select = ((room - if wide { 250.0 } else { 0.0 }) / 4.0).clamp(74.0, 170.0);
            let caption = |ui: &mut egui::Ui, text: &str| {
                if wide {
                    ui.label(RichText::new(text).size(p.small() + 0.5).color(p.muted));
                }
            };
            caption(ui, l("Tabela", "Table"));
            widgets::combo(
                ui,
                &p,
                "builder_table",
                b.table.name.clone(),
                select,
                |ui| {
                    for table in &tables {
                        let current = table.schema == b.table.schema && table.name == b.table.name;
                        if ui.selectable_label(current, &table.name).clicked() && !current {
                            switch_table = Some(table.clone());
                        }
                    }
                },
            )
            .response
            .on_hover_text(l("Tabela", "Table"));
            caption(ui, l("Podsumowanie", "Summary"));
            widgets::combo(
                ui,
                &p,
                "aggregate",
                if b.aggregate.is_empty() {
                    l("Brak", "None").to_owned()
                } else {
                    b.aggregate.clone()
                },
                select * 0.8,
                |ui| {
                    ui.selectable_value(&mut b.aggregate, String::new(), l("Brak", "None"));
                    for a in ["COUNT", "SUM", "AVG", "MIN", "MAX"] {
                        ui.selectable_value(&mut b.aggregate, a.into(), a);
                    }
                },
            )
            .response
            .on_hover_text(l("Podsumowanie", "Summary"));
            ui.add_enabled_ui(!b.aggregate.is_empty(), |ui| {
                widgets::combo(
                    ui,
                    &p,
                    "aggregate_column",
                    b.aggregate_column.clone(),
                    select * 0.8,
                    |ui| {
                        ui.selectable_value(&mut b.aggregate_column, "*".into(), "*");
                        for c in &b.columns {
                            ui.selectable_value(&mut b.aggregate_column, c.name.clone(), &c.name);
                        }
                    },
                )
                .response
                .on_hover_text(l("Kolumna podsumowania", "Summary column"));
            });
            caption(ui, l("Grupuj według", "Group by"));
            widgets::combo(
                ui,
                &p,
                "group_column",
                if b.group_column.is_empty() {
                    l("Brak", "None").to_owned()
                } else {
                    b.group_column.clone()
                },
                select,
                |ui| {
                    ui.selectable_value(&mut b.group_column, String::new(), l("Brak", "None"));
                    for c in &b.columns {
                        ui.selectable_value(&mut b.group_column, c.name.clone(), &c.name);
                    }
                },
            )
            .response
            .on_hover_text(l("Grupuj według", "Group by"));
            widgets::right(ui, |ui| {
                run = run_button.show(ui, &p).clicked();
            });
        });

        // Columns of the table.
        egui::Frame::NONE
            .inner_margin(egui::Margin {
                left: 16,
                right: 16,
                top: 10,
                bottom: 6,
            })
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(16.0, 4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(l("Kolumny", "Columns"))
                            .size(p.small() + 0.5)
                            .color(p.muted),
                    );
                    if b.loading {
                        ui.spinner();
                    } else if b.columns.is_empty() {
                        ui.label(
                            RichText::new(if connected {
                                l("Ta tabela nie ma kolumn.", "This table has no columns.")
                            } else {
                                l(
                                    "Połącz się z bazą, aby wybrać kolumny.",
                                    "Connect to the database to choose columns.",
                                )
                            })
                            .size(p.body() - 0.5)
                            .color(p.muted),
                        );
                    }
                    for c in &b.columns {
                        let mut selected = b.selected.contains(&c.name);
                        if widgets::checkbox(ui, &p, &mut selected, &c.name).changed() {
                            if selected {
                                b.selected.push(c.name.clone());
                            } else {
                                b.selected.retain(|v| v != &c.name);
                            }
                        }
                    }
                    if !b.columns.is_empty() && b.selected.is_empty() {
                        ui.label(
                            RichText::new(l(
                                "nic nie zaznaczono, więc wszystkie",
                                "nothing selected means all",
                            ))
                            .size(p.small())
                            .color(theme::blend(p.bg, p.muted, 0.8)),
                        );
                    }
                });
            });

        // The SQL the choices above produce.
        egui::Frame::NONE
            .inner_margin(egui::Margin {
                left: 16,
                right: 16,
                top: 8,
                bottom: 12,
            })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                match &preview {
                    Ok(sql) => {
                        let mut job = highlight(sql, p.code(), p);
                        job.wrap.max_width = ui.available_width();
                        ui.add(egui::Label::new(job).wrap());
                        ui.add_space(4.0);
                        open_editor = Btn::link(l("Otwórz w edytorze SQL", "Open in SQL editor"))
                            .small()
                            .show(ui, &p)
                            .clicked();
                    }
                    Err(e) => {
                        widgets::feedback(
                            ui,
                            &p,
                            &crate::messages::describe(&e.to_string(), en),
                            true,
                        );
                    }
                }
            });

        self.tab_error(ui, index);
        let has_result = self.tabs[index].result.is_some();
        let actions = self.results_heading(ui, index, false);
        if has_result {
            // The builder's own controls decide the order; a header click does not.
            self.results_grid(ui, index, false);
        } else {
            super::sql_view::hint(
                ui,
                &p,
                l(
                    "Wybierz kolumny i naciśnij „Pokaż wynik”.",
                    "Choose columns and press “Show result”.",
                ),
            );
        }
        let tabid = b.tab;
        let connection = b.connection;
        let title = b.table.name.clone();
        self.builder = Some(b);
        if actions.insert {
            self.begin_insert(index);
        }
        if actions.delete {
            self.ask_delete(index);
        }
        if actions.export {
            self.export_result(tabid);
        }
        if let Ok(sql) = preview {
            if run {
                let tab = &mut self.tabs[index];
                tab.connection = Some(connection);
                tab.sql = sql;
                self.run_tab(tabid, false);
            } else if open_editor {
                self.open_in_editor(Some(connection), &title, sql);
            }
        }
        if let Some(table) = switch_table {
            self.open_builder(connection, table);
        }
    }

    /// No table chosen yet: offer the tables of the connection in focus.
    fn builder_empty(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let focus = self.focus_connection();
        let tables = focus
            .filter(|id| self.databases.contains_key(id))
            .and_then(|id| self.tables.get(&id).cloned())
            .unwrap_or_default();
        let mut chosen = None;
        let mut action = false;
        egui::Frame::NONE
            .inner_margin(egui::Margin::same(20))
            .show(ui, |ui| {
                let (text, label) = if !tables.is_empty() {
                    (
                        l(
                            "Kreator układa zapytanie SELECT z kolumn, podsumowania i grupowania jednej tabeli. Wybierz tabelę, od której zaczniesz.",
                            "The builder composes a SELECT from the columns, a summary and a grouping of one table. Choose the table to start from.",
                        ),
                        None,
                    )
                } else if self.profiles.connections.is_empty() {
                    (
                        l(
                            "Kreator układa zapytanie SELECT z kolumn jednej tabeli. Najpierw dodaj połączenie z bazą.",
                            "The builder composes a SELECT from the columns of one table. Add a database connection first.",
                        ),
                        Some(l("Nowe połączenie", "New connection")),
                    )
                } else {
                    (
                        l(
                            "Kreator układa zapytanie SELECT z kolumn jednej tabeli. Połącz się z bazą, aby wybrać tabelę.",
                            "The builder composes a SELECT from the columns of one table. Connect to a database to choose a table.",
                        ),
                        Some(if focus.is_some() {
                            l("Połącz", "Connect")
                        } else {
                            l("Połączenia", "Connections")
                        }),
                    )
                };
                action = widgets::empty_state(
                    ui,
                    &p,
                    Icon::Workflow,
                    l("Wybierz tabelę", "Choose a table"),
                    text,
                    label,
                );
                if !tables.is_empty() {
                    egui::Frame::NONE
                        .inner_margin(egui::Margin {
                            left: 18,
                            right: 18,
                            top: 0,
                            bottom: 0,
                        })
                        .show(ui, |ui| {
                            widgets::combo(
                                ui,
                                &p,
                                "builder_start",
                                l("Tabela…", "Table…"),
                                240.0,
                                |ui| {
                                    for table in &tables {
                                        if ui.selectable_label(false, &table.name).clicked() {
                                            chosen = Some(table.clone());
                                        }
                                    }
                                },
                            );
                        });
                }
            });
        if let (Some(connection), Some(table)) = (focus, chosen) {
            self.open_builder(connection, table);
        } else if action {
            if self.profiles.connections.is_empty() {
                self.new_connection();
            } else if let Some(id) = focus {
                self.connect(id);
            } else {
                self.section = Section::Connections;
            }
        }
    }
}
