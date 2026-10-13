//! "Historia": the queries that were run, newest first.
use super::*;
use crate::widgets::Btn;

/// Collapse a statement to one line of text for the list.
fn flatten(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len().min(400));
    for word in sql.split_whitespace() {
        if out.len() > 380 {
            out.push('…');
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn local_time(started_at: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(started_at)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| started_at.to_owned())
}

impl StudioApp {
    pub(super) fn history_view(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let mut open = None;
        let (mut to_editor, mut clear) = (false, false);
        egui::Frame::NONE
            .inner_margin(egui::Margin {
                left: 20,
                right: 20,
                top: 20,
                bottom: 0,
            })
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 6.0;
                let any = !self.history.is_empty();
                widgets::pane_heading(
                    ui,
                    &p,
                    l("Historia zapytań", "Query history"),
                    l(
                        "Ostatnie 500 uruchomień z tego komputera. Historia jest zaszyfrowana w lokalnym sejfie.",
                        "The last 500 runs on this computer. The history is encrypted in the local vault.",
                    ),
                    |ui| {
                        if any {
                            ui.add(
                                widgets::field(&p, &mut self.history_search)
                                    .desired_width(190.0)
                                    .hint_text(l("Szukaj w historii", "Search the history")),
                            );
                            clear = Btn::new(l("Wyczyść historię", "Clear history"))
                                .icon(Icon::Trash)
                                .enabled(!self.history_clearing)
                                .show(ui, &p)
                                .clicked();
                        }
                    },
                );
                if !any {
                    to_editor = widgets::empty_state(
                        ui,
                        &p,
                        Icon::History,
                        l("Jeszcze bez zapytań", "No queries yet"),
                        l(
                            "Uruchom zapytanie w edytorze lub kreatorze. Tutaj znajdziesz SQL, czas uruchomienia i liczbę wyników.",
                            "Run a query in the editor or the builder. You will find its SQL, the time of the run and the number of rows here.",
                        ),
                        Some(Section::Sql.label(en)),
                    );
                    return;
                }
                let search = self.history_search.trim().to_lowercase();
                let rows = self
                    .history
                    .iter()
                    .enumerate()
                    .filter(|(_, record)| {
                        search.is_empty()
                            || record.sql.to_lowercase().contains(&search)
                            || self
                                .connection_name(record.connection_id)
                                .is_some_and(|name| name.to_lowercase().contains(&search))
                    })
                    .map(|(i, _)| i)
                    .collect::<Vec<_>>();
                if rows.is_empty() {
                    widgets::help(
                        ui,
                        &p,
                        l(
                            "Żadne zapytanie nie pasuje do wyszukiwania.",
                            "No query matches the search.",
                        ),
                    );
                    return;
                }
                let button = l("Otwórz w edytorze", "Open in editor");
                let button_width = Btn::new(button).small().width(ui, &p);
                let code = egui::FontId::monospace(p.code() - 1.0);
                let small = egui::FontId::proportional(p.small());
                let line_height = code_line_height(p.code() - 1.0);
                let row_height = (30.0 + p.small() + 4.0 + 2.0 * line_height).round();
                ui.spacing_mut().item_spacing.y = 0.0;
                egui::ScrollArea::vertical()
                    .id_salt("history_list")
                    .auto_shrink([false, false])
                    .show_rows(ui, row_height, rows.len(), |ui, range| {
                        for i in range {
                            let record = &self.history[rows[i]];
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), row_height),
                                egui::Sense::hover(),
                            );
                            let painter = ui.painter();
                            painter.hline(
                                rect.x_range(),
                                rect.top() + 0.5,
                                egui::Stroke::new(1.0, p.line),
                            );
                            let text_width = rect.width() - button_width - 34.0;
                            let failed = record.status == "failed";
                            let status = match record.status.as_str() {
                                "completed" => l("ukończone", "completed"),
                                "failed" => l("nieudane", "failed"),
                                "cancelled" => l("zatrzymane", "stopped"),
                                "running" => l("w toku", "running"),
                                _ => l("nieznany stan", "unknown state"),
                            };
                            let mut meta = format!(
                                "{} · {} {} · {} ms · {status}",
                                local_time(&record.started_at),
                                record.rows,
                                row_word(record.rows as usize, en),
                                record.duration_ms,
                            );
                            if let Some(name) = self.connection_name(record.connection_id) {
                                meta.push_str(" · ");
                                meta.push_str(name);
                            }
                            let meta = widgets::one_line(
                                painter,
                                &meta,
                                small.clone(),
                                if failed { p.error } else { p.muted },
                                text_width,
                            );
                            painter.galley(
                                egui::pos2(rect.left(), rect.top() + 14.0),
                                meta,
                                p.muted,
                            );
                            let mut job = egui::text::LayoutJob::single_section(
                                flatten(&record.sql),
                                egui::TextFormat {
                                    font_id: code.clone(),
                                    line_height: Some(line_height),
                                    color: p.text,
                                    ..Default::default()
                                },
                            );
                            job.wrap = egui::text::TextWrapping {
                                max_width: text_width.max(40.0),
                                max_rows: 2,
                                break_anywhere: false,
                                overflow_character: Some('…'),
                            };
                            painter.galley(
                                egui::pos2(rect.left(), rect.top() + 14.0 + p.small() + 7.0),
                                painter.layout_job(job),
                                p.text,
                            );
                            let button_rect = egui::Rect::from_min_size(
                                egui::pos2(
                                    rect.right() - button_width - 6.0,
                                    rect.center().y - (p.control_height() - 4.0) / 2.0,
                                ),
                                egui::vec2(button_width, p.control_height() - 4.0),
                            );
                            let clicked = ui
                                .scope_builder(
                                    egui::UiBuilder::new()
                                        .id_salt(("history_open", record.id))
                                        .max_rect(button_rect),
                                    |ui| Btn::new(button).small().show(ui, &p).clicked(),
                                )
                                .inner;
                            if clicked {
                                open = Some((record.connection_id, record.sql.clone()));
                            }
                        }
                    });
            });
        if to_editor {
            self.set_section(Section::Sql);
        }
        if clear {
            // Nothing is removed before the question is answered.
            self.history_clear_confirm = true;
        }
        if let Some((connection, sql)) = open {
            self.new_tab(Some(connection));
            self.tabs[self.active].sql = sql;
        }
    }
}
