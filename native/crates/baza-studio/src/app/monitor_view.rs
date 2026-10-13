//! "Monitor": live figures of the connected database, read every few seconds
//! while the section is on screen.
use super::*;
use eframe::egui::RichText;

/// Seconds between two readings.
const INTERVAL: u64 = 5;

impl StudioApp {
    pub(super) fn monitor_view(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let focus = self.focus_connection();
        let connected = focus.is_some_and(|id| self.databases.contains_key(&id));
        let Some(id) = focus.filter(|_| connected) else {
            let name = focus
                .and_then(|id| self.connection_name(id))
                .map(str::to_owned);
            let mut action = false;
            egui::Frame::NONE
                .inner_margin(egui::Margin::same(20))
                .show(ui, |ui| {
                    action = match &name {
                        Some(name) => widgets::empty_state(
                            ui,
                            &p,
                            Icon::Activity,
                            l("Baza nie jest połączona", "The database is not connected"),
                            &format!(
                                "{} {name}{}",
                                l("Połącz się z", "Connect to"),
                                l(
                                    ", aby zobaczyć liczbę połączeń i czas odpowiedzi serwera.",
                                    " to see the number of connections and the response time of the server.",
                                )
                            ),
                            Some(l("Połącz", "Connect")),
                        ),
                        None => widgets::empty_state(
                            ui,
                            &p,
                            Icon::Activity,
                            l("Brak połączenia", "No connection"),
                            l(
                                "Monitor pokazuje liczbę połączeń i czas odpowiedzi połączonej bazy. Wybierz połączenie w panelu po lewej.",
                                "The monitor shows the number of connections and the response time of a connected database. Choose a connection in the panel on the left.",
                            ),
                            Some(Section::Connections.label(en)),
                        ),
                    };
                });
            if action {
                match focus {
                    Some(id) => self.connect(id),
                    None => self.section = Section::Connections,
                }
            }
            return;
        };
        if self.monitor_connection != Some(id)
            || self.monitor_last.elapsed() > Duration::from_secs(INTERVAL)
        {
            self.refresh_monitor();
        }
        ui.ctx()
            .request_repaint_after(Duration::from_secs(INTERVAL));
        let name = self.connection_name(id).unwrap_or_default().to_owned();
        let engine = self.databases.get(&id).map_or("", |db| db.product().label);
        let mut refresh = false;
        egui::ScrollArea::vertical()
            .id_salt("monitor_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(24, 22))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.horizontal(|ui| {
                            ui.set_min_height(p.control_height());
                            widgets::heading(ui, &p, "Monitor");
                            widgets::right(ui, |ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                refresh = widgets::icon_button(
                                    ui,
                                    &p,
                                    Icon::Refresh,
                                    l("Odśwież teraz", "Refresh now"),
                                )
                                .clicked();
                                if self.monitor_busy {
                                    ui.add(egui::Spinner::new().size(12.0).color(p.muted));
                                }
                                ui.label(
                                    RichText::new(format!(
                                        "{name} · {}",
                                        l("odświeżanie co 5 s", "refreshed every 5 s")
                                    ))
                                    .size(p.small() + 0.5)
                                    .color(p.muted),
                                );
                            });
                        });
                        ui.add_space(12.0);
                        widgets::hline(ui, &p);
                        let Some(result) = &self.monitor_result else {
                            ui.add_space(16.0);
                            widgets::help(ui, &p, l("Oczekiwanie na dane…", "Waiting for data…"));
                            return;
                        };
                        let value = |column: &str| {
                            result
                                .columns
                                .iter()
                                .position(|c| c.name.eq_ignore_ascii_case(column))
                                .and_then(|i| result.rows.first().and_then(|row| row.get(i)))
                                .map(Cell::display)
                        };
                        // Two figures of the engine, then the response time.
                        // The columns every engine may report have a name
                        // here; a driver's own column is shown under the
                        // name the driver gave it.
                        const KNOWN: [(&str, &str, &str); 4] = [
                            ("connections", "Połączenia", "Connections"),
                            ("active", "Aktywne zapytania", "Active queries"),
                            ("max_connections", "Limit połączeń", "Connection limit"),
                            ("tables", "Tabele", "Tables"),
                        ];
                        let mut figures: Vec<(String, String)> = KNOWN
                            .into_iter()
                            .filter_map(|(column, pl, english)| {
                                let text = value(column)?;
                                // A limit of zero is how a server says it has none.
                                let text = if column == "max_connections" && text == "0" {
                                    l("bez limitu", "no limit").to_owned()
                                } else {
                                    text
                                };
                                Some((l(pl, english).to_owned(), text))
                            })
                            .collect();
                        figures.extend(
                            result
                                .columns
                                .iter()
                                .filter(|column| {
                                    let name = column.name.to_ascii_lowercase();
                                    name != "version"
                                        && name != "database"
                                        && KNOWN.iter().all(|(known, _, _)| *known != name)
                                })
                                .filter_map(|column| {
                                    Some((column.name.clone(), value(&column.name)?))
                                }),
                        );
                        // What does not fit as a figure is listed below.
                        let more = figures.split_off(figures.len().min(2));
                        let mut metrics: Vec<(&str, String, &str)> = figures
                            .iter()
                            .map(|(label, text)| (label.as_str(), text.clone(), ""))
                            .collect();
                        // An engine with a single figure also shows its short
                        // version number.
                        if metrics.len() < 2
                            && let Some(version) = value("version").filter(|v| v.len() <= 14) {
                                metrics.push((l("Wersja", "Version"), version, ""));
                            }
                        metrics.push((
                            l("Czas odpowiedzi", "Response time"),
                            result.duration_ms.to_string(),
                            "ms",
                        ));
                        ui.add_space(16.0);
                        let column = ui.available_width() / metrics.len() as f32;
                        ui.horizontal_top(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            for (label, value, unit) in &metrics {
                                ui.allocate_ui_with_layout(
                                    egui::vec2(column, 0.0),
                                    egui::Layout::top_down(egui::Align::Min),
                                    |ui| {
                                        ui.set_width(column);
                                        ui.spacing_mut().item_spacing.y = 6.0;
                                        widgets::label(ui, &p, label);
                                        ui.horizontal(|ui| {
                                            ui.spacing_mut().item_spacing.x = 6.0;
                                            ui.label(
                                                RichText::new(value)
                                                    .size(p.font + 14.0)
                                                    .color(p.text),
                                            );
                                            if !unit.is_empty() {
                                                ui.with_layout(
                                                    egui::Layout::bottom_up(egui::Align::Min),
                                                    |ui| {
                                                        ui.add_space(5.0);
                                                        ui.label(
                                                            RichText::new(*unit)
                                                                .size(p.body())
                                                                .color(p.muted),
                                                        );
                                                    },
                                                );
                                            }
                                        });
                                    },
                                );
                            }
                        });
                        ui.add_space(16.0);
                        widgets::hline(ui, &p);
                        ui.add_space(18.0);
                        self.monitor_chart(ui);
                        ui.add_space(18.0);
                        widgets::hline(ui, &p);
                        ui.add_space(16.0);
                        ui.horizontal_top(|ui| {
                            ui.spacing_mut().item_spacing.x = 12.0;
                            ui.add_space(2.0);
                            ui.vertical(|ui| {
                                ui.add_space(6.0);
                                widgets::status_dot(ui, theme::BRAND);
                            });
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 7.0;
                                // Engine and database, then the full version
                                // text unless a figure above already shows it.
                                let short = |text: &String| text.len() <= 14;
                                let mut server = vec![engine.to_owned()];
                                server.extend(
                                    [
                                        value("database"),
                                        value("version").filter(|v| !short(v)),
                                    ]
                                    .into_iter()
                                    .flatten()
                                    .filter(|text| !text.is_empty() && text != "NULL")
                                    .map(|text| text.chars().take(90).collect::<String>()),
                                );
                                server.extend(
                                    more.iter().map(|(label, text)| format!("{label}: {text}")),
                                );
                                ui.label(
                                    RichText::new(server.join(" · "))
                                        .font(theme::medium(p.body()))
                                        .color(p.text),
                                );
                                widgets::help(
                                    ui,
                                    &p,
                                    l(
                                        "Wartości pochodzą bezpośrednio z połączonej bazy. Monitor wysyła jedno lekkie zapytanie co 5 sekund, tylko gdy ta sekcja jest otwarta.",
                                        "The values come directly from the connected database. The monitor sends one light query every 5 seconds, only while this section is open.",
                                    ),
                                );
                            });
                        });
                    });
            });
        if refresh {
            self.refresh_monitor();
        }
    }

    /// Response time of the last readings as a line over a faint area.
    fn monitor_chart(&self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        widgets::label(
            ui,
            &p,
            l("Czas odpowiedzi serwera (ms)", "Server response time (ms)"),
        );
        ui.add_space(8.0);
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), 138.0),
            egui::Sense::hover(),
        );
        let painter = ui.painter_at(rect);
        let grid = egui::Stroke::new(1.0, theme::blend(p.bg, p.line, 0.6));
        for step in 0..=2 {
            let y = rect.top() + rect.height() * step as f32 / 2.0;
            painter.hline(
                rect.x_range(),
                y.clamp(rect.top() + 0.5, rect.bottom() - 0.5),
                grid,
            );
        }
        let samples = &self.monitor_series;
        let top = samples
            .iter()
            .map(|sample| sample.response_ms)
            .fold(1.0_f64, f64::max)
            * 1.3;
        let x = |i: usize| {
            if samples.len() < 2 {
                rect.center().x
            } else {
                rect.left() + rect.width() * i as f32 / (samples.len() - 1) as f32
            }
        };
        let y = |value: f64| rect.bottom() - 4.0 - (rect.height() - 12.0) * (value / top) as f32;
        let points = samples
            .iter()
            .enumerate()
            .map(|(i, sample)| egui::pos2(x(i), y(sample.response_ms)))
            .collect::<Vec<_>>();
        if points.len() >= 2 {
            let area = p.accent.gamma_multiply(0.07);
            for pair in points.windows(2) {
                painter.add(egui::Shape::convex_polygon(
                    vec![
                        pair[0],
                        pair[1],
                        egui::pos2(pair[1].x, rect.bottom()),
                        egui::pos2(pair[0].x, rect.bottom()),
                    ],
                    area,
                    egui::Stroke::NONE,
                ));
            }
            painter.add(egui::Shape::line(points, egui::Stroke::new(1.7, p.accent)));
        } else if let Some(point) = points.first() {
            painter.circle_filled(*point, 2.5, p.accent);
        }
        ui.add_space(6.0);
        let time = |sample: Option<&MonitorSample>| {
            sample.map_or_else(String::new, |s| s.at.format("%H:%M:%S").to_string())
        };
        let small = egui::FontId::proportional(p.small());
        let (labels, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), p.small() + 4.0),
            egui::Sense::hover(),
        );
        for (text, anchor, at) in [
            (
                time(samples.front()),
                egui::Align2::LEFT_CENTER,
                labels.left(),
            ),
            (
                if samples.len() >= 3 {
                    time(samples.get(samples.len() / 2))
                } else {
                    String::new()
                },
                egui::Align2::CENTER_CENTER,
                labels.center().x,
            ),
            (
                if samples.len() >= 2 {
                    time(samples.back())
                } else {
                    String::new()
                },
                egui::Align2::RIGHT_CENTER,
                labels.right(),
            ),
        ] {
            ui.painter().text(
                egui::pos2(at, labels.center().y),
                anchor,
                text,
                small.clone(),
                p.muted,
            );
        }
    }
}
