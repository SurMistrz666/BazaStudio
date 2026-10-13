//! The preferences dialog: appearance, SQL editor, data grid and desktop.
//! Changes preview at once and are written to the vault on "Zapisz".
use super::*;
use crate::widgets::Btn;
use eframe::egui::RichText;

/// Miniature of the workspace in one theme, for its card.
fn paint_theme_thumbnail(painter: &egui::Painter, rect: egui::Rect, id: &str) {
    let t = theme::base(id);
    painter.rect_filled(rect, 3.0, t.bg);
    let bar = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), 9.0));
    painter.rect_filled(
        bar,
        egui::CornerRadius {
            nw: 3,
            ne: 3,
            sw: 0,
            se: 0,
        },
        t.side,
    );
    let side = egui::Rect::from_min_max(
        rect.min + egui::vec2(0.0, 9.0),
        egui::pos2(rect.left() + rect.width() * 0.27, rect.bottom()),
    );
    painter.rect_filled(
        side,
        egui::CornerRadius {
            nw: 0,
            ne: 0,
            sw: 3,
            se: 0,
        },
        t.side,
    );
    painter.vline(side.right(), side.y_range(), egui::Stroke::new(1.0, t.line));
    painter.hline(rect.x_range(), bar.bottom(), egui::Stroke::new(1.0, t.line));
    // Active tab with its accent line.
    let tab = egui::Rect::from_min_size(
        egui::pos2(side.right() + 1.0, bar.bottom() + 1.0),
        egui::vec2(rect.width() * 0.2, 2.0),
    );
    painter.rect_filled(tab, 0.0, t.accent);
    for (row, color, length) in [
        (0, t.keyword, 0.42),
        (1, t.text, 0.3),
        (2, t.string, 0.36),
        (3, t.number, 0.22),
    ] {
        let start = egui::pos2(
            side.right() + rect.width() * 0.1,
            bar.bottom() + 14.0 + row as f32 * 9.0,
        );
        painter.line_segment(
            [start, start + egui::vec2(rect.width() * length, 0.0)],
            egui::Stroke::new(2.0, color),
        );
    }
    for row in 0..3 {
        let start = egui::pos2(rect.left() + 6.0, bar.bottom() + 12.0 + row as f32 * 9.0);
        painter.line_segment(
            [start, start + egui::vec2(side.width() - 14.0, 0.0)],
            egui::Stroke::new(2.0, theme::blend(t.side, t.muted, 0.5)),
        );
    }
    painter.rect_stroke(
        rect,
        3.0,
        egui::Stroke::new(1.0, t.line),
        egui::StrokeKind::Inside,
    );
}

impl StudioApp {
    pub(super) fn preferences_window(&mut self, ctx: &egui::Context) {
        if !self.show_preferences {
            return;
        }
        if self.preference_original.is_none() {
            self.preference_original = Some(self.profiles.preferences.clone());
            self.preference_save = PreferencesSaveState::default();
        }
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let width = (ctx.content_rect().width() - 80.0).clamp(720.0, 890.0);
        let body_height = (ctx.content_rect().height() - 190.0).clamp(340.0, 510.0);
        let content_width = width - 201.0;
        let card_width = preference_card_width(content_width);
        let mut apply = false;
        let mut cancel = false;
        let mut changed = false;
        let mut security = false;
        let busy = self.preference_save.busy();
        let error = self
            .preference_save
            .error
            .as_ref()
            .map(|error| crate::messages::describe(error, en));
        let response = egui::Modal::new(egui::Id::new("preferences_modal"))
            .backdrop_color(Color32::from_black_alpha(150))
            .frame(
                egui::Frame::NONE
                    .fill(p.bg)
                    .stroke(egui::Stroke::new(1.0, p.line))
                    .corner_radius(8),
            )
            .show(ctx, |ui| {
                ui.set_width(width);
                ui.add_enabled_ui(!busy, |ui| {
                    ui.set_width(width);
                    ui.spacing_mut().item_spacing.y = 0.0;
                    // Header.
                    egui::Frame::NONE
                        .fill(p.side)
                        .corner_radius(egui::CornerRadius {
                            nw: 7,
                            ne: 7,
                            sw: 0,
                            se: 0,
                        })
                        .inner_margin(egui::Margin {
                            left: 18,
                            right: 8,
                            top: 0,
                            bottom: 0,
                        })
                        .show(ui, |ui| {
                            ui.set_width(width - 26.0);
                            ui.horizontal(|ui| {
                                ui.set_height(42.0);
                                ui.label(
                                    RichText::new(l("Preferencje", "Preferences"))
                                        .font(theme::strong(p.font + 1.5))
                                        .color(p.text),
                                );
                                widgets::right(ui, |ui| {
                                    cancel = widgets::icon_button(
                                        ui,
                                        &p,
                                        Icon::Close,
                                        l("Zamknij", "Close"),
                                    )
                                    .clicked();
                                });
                            });
                        });
                    widgets::hline(ui, &p);
                    ui.horizontal_top(|ui| {
                        bound_preferences_body(ui, width, body_height);
                        // Sections.
                        egui::Frame::NONE
                            .fill(p.side)
                            .inner_margin(egui::Margin::symmetric(10, 14))
                            .show(ui, |ui| {
                                preferences_column(ui, 140.0, body_height - 28.0, |ui| {
                                    ui.spacing_mut().item_spacing.y = 3.0;
                                    for (section, icon, name) in [
                                        (0, Icon::Settings, l("Wygląd", "Appearance")),
                                        (1, Icon::Terminal, l("Edytor SQL", "SQL editor")),
                                        (2, Icon::Table, l("Siatka danych", "Data grid")),
                                        (3, Icon::Shield, l("Pulpit", "Desktop")),
                                    ] {
                                        let selected = self.preference_section == section;
                                        let (rect, r) = ui.allocate_exact_size(
                                            egui::vec2(140.0, 34.0),
                                            egui::Sense::click(),
                                        );
                                        if selected {
                                            ui.painter().rect_filled(rect, 3.0, p.raised);
                                        } else if r.hovered() {
                                            ui.painter().rect_filled(
                                                rect,
                                                3.0,
                                                theme::blend(p.side, p.raised, 0.6),
                                            );
                                        }
                                        let color = if selected { p.accent } else { p.muted };
                                        widgets::paint(
                                            ui.painter(),
                                            egui::Rect::from_min_size(
                                                rect.min + egui::vec2(11.0, 10.0),
                                                egui::vec2(14.0, 14.0),
                                            ),
                                            icon,
                                            color,
                                        );
                                        ui.painter().text(
                                            rect.min + egui::vec2(34.0, 17.0),
                                            egui::Align2::LEFT_CENTER,
                                            name,
                                            egui::FontId::proportional(p.body()),
                                            if selected { p.accent } else { p.text },
                                        );
                                        r.widget_info(|| {
                                            egui::WidgetInfo::labeled(
                                                egui::WidgetType::Button,
                                                true,
                                                name,
                                            )
                                        });
                                        if r.clicked() {
                                            self.preference_section = section;
                                        }
                                    }
                                });
                            });
                        paint_preferences_divider(ui, body_height, p.line);
                        egui::Frame::NONE
                            .inner_margin(egui::Margin::symmetric(20, 20))
                            .show(ui, |ui| {
                                preferences_column(ui, content_width, body_height - 40.0, |ui| {
                                    ui.spacing_mut().item_spacing = egui::vec2(12.0, 11.0);
                                    egui::ScrollArea::vertical()
                                        .auto_shrink([false, false])
                                        .max_width(content_width)
                                        .max_height(body_height - 40.0)
                                        .show(ui, |ui| {
                                            ui.set_max_width(content_width);
                                            match self.preference_section {
                                                0 => changed |= self.appearance_preferences(ui, card_width),
                                                1 => changed |= self.editor_preferences(ui),
                                                2 => changed |= self.grid_preferences(ui),
                                                _ => {
                                                    let (edited, lock) = self.desktop_preferences(ui);
                                                    changed |= edited;
                                                    if lock {
                                                        apply = true;
                                                        security = true;
                                                    }
                                                }
                                            }
                                        });
                                });
                            });
                    });
                    widgets::hline(ui, &p);
                    // Footer.
                    egui::Frame::NONE
                        .fill(p.side)
                        .corner_radius(egui::CornerRadius {
                            nw: 0,
                            ne: 0,
                            sw: 7,
                            se: 7,
                        })
                        .inner_margin(egui::Margin::symmetric(16, 12))
                        .show(ui, |ui| {
                            ui.set_width(width - 32.0);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                if Btn::quiet(l("Przywróć domyślne", "Restore defaults"))
                                    .show(ui, &p)
                                    .clicked()
                                {
                                    self.profiles.preferences = Preferences::default();
                                    changed = true;
                                }
                                if busy {
                                    ui.spinner();
                                }
                                ui.label(
                                    RichText::new(if busy {
                                        l(
                                            "Zapisywanie w lokalnym sejfie…",
                                            "Saving to the local vault…",
                                        )
                                    } else {
                                        l("Zmiany widać od razu", "Changes preview at once")
                                    })
                                    .size(p.small())
                                    .color(p.muted),
                                );
                                widgets::right(ui, |ui| {
                                    if Btn::primary(l("Zapisz preferencje", "Save preferences"))
                                        .show(ui, &p)
                                        .clicked()
                                    {
                                        apply = true;
                                    }
                                    if Btn::new(l("Anuluj", "Cancel"))
                                        .min_width(80.0)
                                        .show(ui, &p)
                                        .clicked()
                                    {
                                        cancel = true;
                                    }
                                });
                            });
                        });
                });
                if let Some(error) = &error {
                    egui::Frame::NONE
                        .inner_margin(egui::Margin::symmetric(16, 10))
                        .show(ui, |ui| {
                            ui.set_width(width - 32.0);
                            widgets::feedback(
                                ui,
                                &p,
                                &format!(
                                    "{} {error}",
                                    l(
                                        "Nie zapisano preferencji. Formularz zachowano; sprawdź ustawienia i ponów zapis.",
                                        "Preferences were not saved. Your draft is preserved; review it and retry.",
                                    )
                                ),
                                true,
                            );
                        });
                }
            });
        if changed {
            self.preference_save.error = None;
        }
        if apply {
            self.save_preferences(security);
        } else if !busy && (cancel || response.should_close()) {
            if let Some(original) = self.preference_original.take() {
                self.profiles.preferences = original;
            }
            self.show_preferences = false;
            self.preference_save = PreferencesSaveState::default();
            changed = true;
        }
        if changed || apply {
            theme::apply_preferences(ctx, &self.profiles.preferences);
            ctx.request_repaint();
        }
    }

    /// Themes and typography. Returns whether a preference was changed.
    fn appearance_preferences(&mut self, ui: &mut egui::Ui, card_width: f32) -> bool {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let mut changed = false;
        let prefs = &mut self.profiles.preferences;
        widgets::subheading(ui, &p, l("Motyw", "Theme"));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            for (id, name) in theme::THEMES {
                let selected = prefs.theme == id;
                let (rect, r) =
                    ui.allocate_exact_size(egui::vec2(card_width, 135.0), egui::Sense::click());
                ui.painter().rect_filled(rect, 5.0, p.side);
                ui.painter().rect_stroke(
                    rect,
                    5.0,
                    egui::Stroke::new(
                        if selected { 2.0 } else { 1.0 },
                        if selected { p.accent } else { p.line },
                    ),
                    egui::StrokeKind::Inside,
                );
                paint_theme_thumbnail(
                    ui.painter(),
                    egui::Rect::from_min_size(
                        rect.min + egui::vec2(8.0, 8.0),
                        egui::vec2(rect.width() - 16.0, 66.0),
                    ),
                    id,
                );
                let label = match id {
                    "graphite" => l("Grafit", "Graphite"),
                    "amethyst" => l("Ametyst", "Amethyst"),
                    _ => name,
                };
                ui.painter().text(
                    rect.min + egui::vec2(9.0, 91.0),
                    egui::Align2::LEFT_CENTER,
                    label,
                    theme::medium(p.small() + 0.5),
                    if selected { p.accent } else { p.text },
                );
                let description = match id {
                    "vscode" => l("Neutralne szarości", "Neutral greys"),
                    "graphite" => l("Chłodny grafit", "Cool graphite"),
                    "midnight" => l("Głęboki granat", "Deep navy"),
                    "oled" => l("Głęboka czerń", "Deep black"),
                    _ => l("Spokojny fiolet", "Soft purple"),
                };
                let description = widgets::one_line(
                    ui.painter(),
                    description,
                    egui::FontId::proportional(p.caption()),
                    p.muted,
                    rect.width() - 16.0,
                );
                ui.painter()
                    .galley(rect.min + egui::vec2(9.0, 104.0), description, p.muted);
                r.widget_info(|| {
                    egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, label)
                });
                if r.clicked() {
                    prefs.theme = id.into();
                    changed = true;
                }
            }
        });
        ui.add_space(4.0);
        let column = (ui.available_width() - 34.0) / 2.0;
        egui::Grid::new("appearance_fields")
            .num_columns(2)
            .min_col_width(column)
            .spacing([34.0, 14.0])
            .show(ui, |ui| {
                ui.vertical(|ui| {
                    widgets::label(ui, &p, l("Kolor akcentu", "Accent colour"));
                    ui.horizontal(|ui| {
                        let mut own = prefs.accent != theme::DEFAULT_ACCENT;
                        if widgets::checkbox(ui, &p, &mut own, l("Własny kolor", "Own colour"))
                            .changed()
                        {
                            prefs.accent = if own {
                                [p.accent.r(), p.accent.g(), p.accent.b()]
                            } else {
                                theme::DEFAULT_ACCENT
                            };
                            changed = true;
                        }
                        if own {
                            changed |= ui.color_edit_button_srgb(&mut prefs.accent).changed();
                            let [a, b, c] = prefs.accent;
                            ui.label(
                                RichText::new(format!("#{a:02X}{b:02X}{c:02X}"))
                                    .monospace()
                                    .size(p.small())
                                    .color(p.muted),
                            );
                        } else {
                            ui.label(
                                RichText::new(l("kolor motywu", "theme colour"))
                                    .size(p.small())
                                    .color(p.muted),
                            );
                        }
                    });
                });
                ui.vertical(|ui| {
                    widgets::label(
                        ui,
                        &p,
                        l("Rozmiar tekstu interfejsu", "Interface text size"),
                    );
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut prefs.font_size, 11.0..=22.0)
                                .suffix(" px")
                                .integer(),
                        )
                        .changed();
                });
                ui.end_row();
                ui.vertical(|ui| {
                    widgets::label(ui, &p, l("Język interfejsu", "Interface language"));
                    widgets::combo(
                        ui,
                        &p,
                        "pref_language",
                        if prefs.language == "pl" {
                            "Polski"
                        } else {
                            "English"
                        },
                        200.0,
                        |ui| {
                            changed |= ui
                                .selectable_value(&mut prefs.language, "pl".into(), "Polski")
                                .changed();
                            changed |= ui
                                .selectable_value(&mut prefs.language, "en".into(), "English")
                                .changed();
                        },
                    );
                });
                ui.vertical(|ui| {
                    widgets::label(ui, &p, l("Gęstość interfejsu", "Interface density"));
                    widgets::combo(
                        ui,
                        &p,
                        "pref_density",
                        if prefs.density == "compact" {
                            l("Kompaktowa", "Compact")
                        } else {
                            l("Wygodna", "Comfortable")
                        },
                        200.0,
                        |ui| {
                            changed |= ui
                                .selectable_value(
                                    &mut prefs.density,
                                    "compact".into(),
                                    l("Kompaktowa: więcej wierszy", "Compact: more rows"),
                                )
                                .changed();
                            changed |= ui
                                .selectable_value(
                                    &mut prefs.density,
                                    "comfortable".into(),
                                    l("Wygodna: więcej przestrzeni", "Comfortable: more space"),
                                )
                                .changed();
                        },
                    );
                });
                ui.end_row();
            });
        widgets::help(
            ui,
            &p,
            l(
                "Interfejs używa kroju Inter, tak jak portal; kod korzysta z systemowej czcionki o stałej szerokości.",
                "The interface uses the Inter typeface, like the portal; code uses the system monospace font.",
            ),
        );
        changed
    }

    fn editor_preferences(&mut self, ui: &mut egui::Ui) -> bool {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let prefs = &mut self.profiles.preferences;
        let mut changed = false;
        widgets::subheading(ui, &p, l("Edytor SQL", "SQL editor"));
        changed |= widgets::checkbox(
            ui,
            &p,
            &mut prefs.editor_line_numbers,
            l("Pokazuj numery wierszy", "Show line numbers"),
        )
        .changed();
        changed |= widgets::checkbox(
            ui,
            &p,
            &mut prefs.editor_word_wrap,
            l("Zawijaj długie wiersze", "Wrap long lines"),
        )
        .changed();
        ui.add_space(4.0);
        widgets::hline(ui, &p);
        ui.add_space(4.0);
        widgets::subheading(ui, &p, l("Skróty klawiaturowe", "Keyboard shortcuts"));
        egui::Grid::new("sql_shortcuts")
            .spacing([26.0, 10.0])
            .show(ui, |ui| {
                let editor = super::shortcuts::GROUPS
                    .iter()
                    .find(|(pl, _, _)| *pl == "Edytor SQL")
                    .map_or(&[][..], |(_, _, list)| *list);
                for shortcut in editor {
                    ui.label(
                        RichText::new(super::shortcuts::keys_text(shortcut.keys, en))
                            .monospace()
                            .size(p.code() - 1.5)
                            .color(p.text),
                    );
                    ui.label(
                        RichText::new(if en { shortcut.en } else { shortcut.pl })
                            .size(p.body() - 0.5)
                            .color(p.muted),
                    );
                    ui.end_row();
                }
            });
        if Btn::link(l("Wszystkie skróty (F1)", "Every shortcut (F1)"))
            .small()
            .show(ui, &p)
            .clicked()
        {
            self.show_shortcuts = true;
        }
        changed
    }

    fn grid_preferences(&mut self, ui: &mut egui::Ui) -> bool {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        widgets::subheading(ui, &p, l("Siatka danych", "Data grid"));
        let mut changed = widgets::checkbox(
            ui,
            &p,
            &mut self.profiles.preferences.grid_striped,
            l("Naprzemienne tło wierszy", "Alternate row background"),
        )
        .changed();
        ui.add_space(4.0);
        widgets::label(
            ui,
            &p,
            l(
                "Rekordy na stronie w Danych tabeli",
                "Records per page in Table data",
            ),
        );
        let current = self.profiles.preferences.page_rows;
        widgets::combo(
            ui,
            &p,
            "preference_page_rows",
            super::results::page_rows_label(current, en),
            200.0,
            |ui| {
                for rows in super::results::PAGE_ROWS_CHOICES {
                    if ui
                        .selectable_label(
                            current == rows,
                            super::results::page_rows_label(rows, en),
                        )
                        .clicked()
                    {
                        self.profiles.preferences.page_rows = rows;
                        changed = true;
                    }
                }
            },
        );
        widgets::help(
            ui,
            &p,
            l(
                "Kolejne strony otwiera pasek pod tabelą. Ikona zębatki na tym pasku zmienia liczbę rekordów dla jednej tabeli.",
                "The bar under a table opens the next pages. Its gear icon changes the number of records for one table.",
            ),
        );
        widgets::help(
            ui,
            &p,
            l(
                "Dwuklik lub F2 zmienia jedną komórkę. Enter zapisuje, Esc anuluje.",
                "Double-click or F2 changes one cell. Enter saves, Esc cancels.",
            ),
        );
        widgets::help(
            ui,
            &p,
            l(
                "Zaznaczanie jak w arkuszu: przeciągnij po komórkach, Shift rozszerza, numer wiersza zaznacza wiersz, Ctrl lub Cmd z nagłówkiem zaznacza kolumnę, Ctrl+A całą stronę. Ctrl+C kopiuje zaznaczenie, a menu pod prawym przyciskiem kopiuje je też jako CSV, JSON lub INSERT.",
                "Marking works as in a spreadsheet: drag over cells, Shift extends, a row number marks the row, Ctrl or Cmd with a header marks the column, Ctrl+A the whole page. Ctrl+C copies the marked cells, and the right-click menu copies them as CSV, JSON or INSERT too.",
            ),
        );
        widgets::help(
            ui,
            &p,
            l(
                "Klucz główny i poprzednia wartość są sprawdzane w transakcji. Wyniki złożonych zapytań, których nie da się jednoznacznie przypisać do tabeli, pozostają tylko do odczytu.",
                "The primary key and the previous value are checked in a transaction. Results of complex queries that cannot be mapped to one table stay read-only.",
            ),
        );
        changed
    }

    /// Returns whether a preference was changed, and whether the lock
    /// settings were asked for.
    fn desktop_preferences(&mut self, ui: &mut egui::Ui) -> (bool, bool) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        widgets::subheading(ui, &p, l("Pulpit i prywatność", "Desktop and privacy"));
        let changed = widgets::checkbox(
            ui,
            &p,
            &mut self.profiles.preferences.local_sql_history,
            l(
                "Zapisuj dodatkowe lokalne pliki historii .sql",
                "Save additional local .sql history files",
            ),
        )
        .changed();
        widgets::help(
            ui,
            &p,
            l(
                "Historia zapytań i biblioteka są zawsze zaszyfrowane w lokalnym sejfie.",
                "The query history and the library are always encrypted in the local vault.",
            ),
        );
        ui.add_space(4.0);
        widgets::hline(ui, &p);
        ui.add_space(4.0);
        widgets::subheading(ui, &p, l("Blokada aplikacji", "Application lock"));
        ui.horizontal(|ui| {
            widgets::icon(
                ui,
                Icon::Lock,
                15.0,
                if self.lock_settings.enabled {
                    p.positive
                } else {
                    p.muted
                },
            );
            ui.label(
                RichText::new(if self.lock_settings.enabled {
                    l("Blokada jest włączona", "The lock is enabled")
                } else {
                    l("Blokada jest wyłączona", "The lock is disabled")
                })
                .size(p.body())
                .color(p.text),
            );
        });
        widgets::help(
            ui,
            &p,
            l(
                "Lokalne hasło i dodatkowy kod TOTP są opcjonalne i niezależne od portalu.",
                "A local password and an additional TOTP code are optional and independent of the portal.",
            ),
        );
        let lock = Btn::new(l(
            "Zapisz i otwórz ustawienia blokady…",
            "Save and open the lock settings…",
        ))
        .show(ui, &p)
        .clicked();
        (changed, lock)
    }
}
