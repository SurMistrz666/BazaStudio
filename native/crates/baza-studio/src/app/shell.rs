//! Frame of the main window: the top bar, the section strip, the notice line
//! and the status bar around the section views.
use super::*;
use eframe::egui::RichText;

/// Height of the top bar and of the section strip: 38 points at the default
/// text size, more for larger text.
fn bar_height(p: &theme::Palette) -> f32 {
    (p.font * 3.17).round().max(38.0)
}

impl StudioApp {
    pub(super) fn shell(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if let Some(code) = self.startup_error.clone() {
            self.startup_screen(ui, Some(&code));
            return;
        }
        if self.vault.is_none() || self.qa_startup {
            self.startup_screen(ui, None);
            return;
        }
        if self.locked {
            self.unlock_screen(ui);
            return;
        }
        let p = self.palette();
        self.global_shortcuts(ui);
        if !self.focus_mode {
            egui::Panel::top("app_header")
                .exact_size(bar_height(&p))
                .resizable(false)
                .frame(
                    egui::Frame::NONE
                        .fill(p.side)
                        .inner_margin(egui::Margin::symmetric(14, 0)),
                )
                .show(ui, |ui| self.top_bar(ui));
        }
        if self.locked {
            // The lock button of the top bar was used in this frame.
            self.unlock_screen(ui);
            return;
        }
        egui::Panel::bottom("status")
            .exact_size((p.small() * 2.5).round().max(26.0))
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(p.side)
                    .inner_margin(egui::Margin::symmetric(13, 0)),
            )
            .show(ui, |ui| self.status_bar(ui));
        if self.show_explorer && !self.focus_mode {
            egui::Panel::left("explorer")
                .resizable(true)
                .default_size(248.0)
                .size_range(200.0..=420.0)
                .frame(egui::Frame::NONE.fill(p.side))
                .show(ui, |ui| self.sidebar(ui));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(p.bg))
            .show(ui, |ui| {
                egui::Panel::top("section_strip")
                    .exact_size((p.font * 4.7).round().max(58.0))
                    .resizable(false)
                    .frame(egui::Frame::NONE.fill(p.side))
                    .show(ui, |ui| self.section_strip(ui));
                if matches!(
                    self.section,
                    Section::Objects | Section::Table | Section::Design
                ) {
                    egui::Panel::top("workspace_tabs")
                        .exact_size((bar_height(&p) - 4.0).max(34.0))
                        .resizable(false)
                        .frame(egui::Frame::NONE.fill(p.side))
                        .show(ui, |ui| self.workspace_tabs(ui));
                }
                self.notice_bar(ui);
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE.fill(p.bg))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        match self.section {
                            Section::Objects => self.objects_view(ui),
                            Section::Design => self.design_view(ui),
                            Section::Sql => self.sql_view(ui),
                            Section::Table => self.table_view(ui),
                            Section::Builder => self.builder_view(ui),
                            Section::History => self.history_view(ui),
                            Section::Transfer => self.transfer_view(ui),
                            Section::Connections => self.connections_view(ui),
                            Section::Monitor => self.monitor_view(ui),
                        }
                    });
            });
        self.group_window(&ctx);
        self.saved_query_window(&ctx);
        self.suggestions_window(&ctx);
        self.connection_window(&ctx);
        self.preferences_window(&ctx);
        self.cell_window(&ctx);
        self.value_window(&ctx);
        self.confirmations(&ctx);
        self.objects_windows(&ctx);
        self.shortcut_windows(&ctx);
        self.fill_window(&ctx);
        self.design_confirm_window(&ctx);
        self.values_window(&ctx);
        self.insert_window(&ctx);
        self.lock_settings_window(&ctx);
        self.distribution_window(&ctx);
        self.about_window(&ctx);
        self.host_key_change_window(&ctx);
    }

    /// Open a page of the project on GitHub in the browser.
    pub(super) fn open_project(&mut self, url: &str) {
        if let Err(error) = updater::open_project_url(url) {
            self.notify(error.to_string(), true);
        }
    }

    pub(super) fn new_connection(&mut self) {
        self.connection_dialog = Some(ConnectionProfile::default());
    }

    pub(super) fn edit_connection(&mut self, id: Uuid) {
        self.connection_dialog = self
            .profiles
            .connections
            .iter()
            .find(|p| p.id == id)
            .cloned();
    }

    /// Open the editor on a copy of a connection: everything but the id, the
    /// name and the SSH server key, in the same group. Nothing is stored
    /// until the copy is saved.
    pub(super) fn duplicate_connection(&mut self, id: Uuid) {
        let Some(source) = self.profiles.connections.iter().find(|p| p.id == id) else {
            return;
        };
        let mut copy = source.clone();
        copy.id = Uuid::new_v4();
        copy.name =
            connection_edit::copy_name(&self.profiles, &source.name, self.tr("kopia", "copy"));
        // The copy learns the key on its first connection: once edited, it
        // may point at another SSH server.
        self.copy_key_notice = copy
            .ssh
            .as_mut()
            .is_some_and(|ssh| !std::mem::take(&mut ssh.fingerprint).is_empty());
        self.connection_form_group =
            Some((copy.id, self.group_snapshot.memberships.get(&id).copied()));
        self.connection_dialog = Some(copy);
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let focus = self.focus_connection();
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 7.0;
            widgets::brand_mark(ui, 17.0);
            ui.add(
                egui::Label::new(
                    RichText::new("Baza Studio")
                        .font(theme::strong(p.control() + 0.5))
                        .color(p.text),
                )
                .selectable(false),
            );
            // Menus and the controls on the right keep their room; the name of
            // the workspace takes what is left.
            let room = ui.available_width() - 690.0 * p.font / 12.0;
            if room > 50.0 {
                // The connection of what is on screen, not of the row
                // selected in the side panel.
                let name = self
                    .shown_connection()
                    .and_then(|id| self.connection_name(id))
                    .unwrap_or(l("bez połączenia", "no connection"));
                let text = format!("— {name}");
                let galley = widgets::one_line(
                    ui.painter(),
                    &text,
                    egui::FontId::proportional(p.control()),
                    p.muted,
                    room.min(260.0),
                );
                let (rect, _) = ui.allocate_exact_size(galley.size(), egui::Sense::hover());
                ui.painter().galley(rect.min, galley, p.muted);
            }
            ui.add_space(8.0);
            self.menus(ui, focus);
            widgets::right(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                if widgets::icon_button(ui, &p, Icon::Settings, l("Preferencje", "Preferences"))
                    .clicked()
                {
                    self.show_preferences = true;
                }
                if self.lock_settings.enabled
                    && widgets::icon_button(
                        ui,
                        &p,
                        Icon::Lock,
                        l("Zablokuj aplikację", "Lock application"),
                    )
                    .clicked()
                {
                    self.lock_now();
                }
                let before = self.profiles.preferences.language.clone();
                widgets::quiet_combo(
                    ui,
                    &p,
                    "header_language",
                    RichText::new(before.to_uppercase()).color(p.text),
                    34.0,
                    |ui| {
                        for (id, name) in [("pl", "Polski"), ("en", "English")] {
                            ui.selectable_value(
                                &mut self.profiles.preferences.language,
                                id.into(),
                                name,
                            );
                        }
                    },
                )
                .response
                .on_hover_text(l("Język interfejsu", "Interface language"));
                ui.add_space(6.0);
                let current = self.profiles.preferences.theme.clone();
                let current_name = theme::THEMES
                    .iter()
                    .find(|(id, _)| *id == current)
                    .map_or("VS Code", |(_, name)| name);
                widgets::quiet_combo(
                    ui,
                    &p,
                    "header_theme",
                    RichText::new(current_name).color(p.text),
                    76.0,
                    |ui| {
                        for (id, name) in theme::THEMES {
                            ui.selectable_value(
                                &mut self.profiles.preferences.theme,
                                id.into(),
                                name,
                            );
                        }
                    },
                );
                ui.label(
                    RichText::new(l("Wygląd", "Appearance"))
                        .size(p.control())
                        .color(p.muted),
                );
                if before != self.profiles.preferences.language
                    || current != self.profiles.preferences.theme
                {
                    theme::apply_preferences(ui.ctx(), &self.profiles.preferences);
                    self.save();
                }
            });
        });
    }

    /// The application menus, drawn as quiet labels in the top bar.
    fn menus(&mut self, ui: &mut egui::Ui, focus: Option<Uuid>) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let title = |text: &'static str| RichText::new(text).size(p.control()).color(p.muted);
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.spacing_mut().button_padding = egui::vec2(7.0, 4.0);
            let widgets = &mut ui.visuals_mut().widgets;
            for visuals in [
                &mut widgets.inactive,
                &mut widgets.hovered,
                &mut widgets.open,
            ] {
                visuals.bg_stroke = egui::Stroke::NONE;
            }
            widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
            widgets.hovered.weak_bg_fill = p.raised;
            widgets.open.weak_bg_fill = p.raised;
            ui.menu_button(title(l("Plik", "File")), |ui| {
                if ui
                    .add(
                        egui::Button::new(l("Nowe zapytanie", "New query")).shortcut_text("Ctrl+Q"),
                    )
                    .clicked()
                {
                    self.new_tab(self.tabs[self.active].connection);
                    ui.close();
                }
                if ui
                    .add(
                        egui::Button::new(l("Otwórz plik SQL…", "Open SQL file…"))
                            .shortcut_text(shortcuts::keys_text("Mod+O", en)),
                    )
                    .clicked()
                {
                    self.set_section(Section::Sql);
                    self.open_sql(self.tabs[self.active].id);
                    ui.close();
                }
                if ui.button(l("Zapisz plik SQL…", "Save SQL file…")).clicked() {
                    self.save_sql(self.tabs[self.active].id);
                    ui.close();
                }
                if ui
                    .button(l(
                        "Zapisz zapytanie w bibliotece…",
                        "Save query to library…",
                    ))
                    .clicked()
                {
                    self.save_query_name = Some(self.tabs[self.active].title.clone());
                    ui.close();
                }
                ui.separator();
                if ui
                    .button(l(
                        "Importuj profile połączeń…",
                        "Import connection profiles…",
                    ))
                    .clicked()
                {
                    self.section = Section::Connections;
                    self.open_profile_file();
                    ui.close();
                }
                if ui
                    .button(l(
                        "Eksportuj profile połączeń…",
                        "Export connection profiles…",
                    ))
                    .clicked()
                {
                    self.section = Section::Connections;
                    self.export_profiles_dialog();
                    ui.close();
                }
            });
            ui.menu_button(title(l("Edycja", "Edit")), |ui| {
                for (pl, english, key, shift) in [
                    ("Cofnij   Ctrl+Z", "Undo   Ctrl+Z", egui::Key::Z, false),
                    (
                        "Ponów   Ctrl+Shift+Z",
                        "Redo   Ctrl+Shift+Z",
                        egui::Key::Z,
                        true,
                    ),
                    (
                        "Zaznacz wszystko   Ctrl+A",
                        "Select all   Ctrl+A",
                        egui::Key::A,
                        false,
                    ),
                ] {
                    if ui.button(l(pl, english)).clicked() {
                        self.set_section(Section::Sql);
                        self.editor_key(key, shift);
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button(l("Kopiuj   Ctrl+C", "Copy   Ctrl+C")).clicked() {
                    self.focus_editor();
                    self.ctx.input_mut(|i| i.events.push(egui::Event::Copy));
                    ui.close();
                }
                if ui.button(l("Wytnij   Ctrl+X", "Cut   Ctrl+X")).clicked() {
                    self.focus_editor();
                    self.ctx.input_mut(|i| i.events.push(egui::Event::Cut));
                    ui.close();
                }
            });
            ui.menu_button(title(l("Widok", "View")), |ui| {
                for (number, section) in Section::ALL.into_iter().enumerate() {
                    let available = self.section_blocked(section).is_none();
                    let keys = shortcuts::keys_text(&format!("Mod+{}", number + 1), en);
                    if ui
                        .add_enabled(
                            available,
                            egui::Button::new(self.section_label(section)).shortcut_text(keys),
                        )
                        .clicked()
                    {
                        self.set_section(section);
                        ui.close();
                    }
                }
                ui.separator();
                ui.checkbox(
                    &mut self.show_explorer,
                    l("Panel połączeń", "Connections panel"),
                );
                ui.checkbox(&mut self.focus_mode, l("Tryb skupienia", "Focus mode"));
                if ui
                    .add(
                        egui::Button::new(l("Preferencje…", "Preferences…"))
                            .shortcut_text(shortcuts::keys_text("Mod+,", en)),
                    )
                    .clicked()
                {
                    self.show_preferences = true;
                    ui.close();
                }
            });
            ui.menu_button(title(l("Połączenie", "Connection")), |ui| {
                if ui
                    .button(l("Nowe połączenie…", "New connection…"))
                    .clicked()
                {
                    self.new_connection();
                    ui.close();
                }
                if ui.button(l("Nowa grupa…", "New group…")).clicked() {
                    self.new_group(None);
                    ui.close();
                }
                if let Some(id) = focus {
                    let connected = self.databases.contains_key(&id);
                    ui.separator();
                    if !connected && ui.button(l("Połącz", "Connect")).clicked() {
                        self.connect(id);
                        ui.close();
                    }
                    if connected && ui.button(l("Rozłącz", "Disconnect")).clicked() {
                        self.disconnect(id);
                        ui.close();
                    }
                    if ui
                        .button(l("Edytuj połączenie…", "Edit connection…"))
                        .clicked()
                    {
                        self.edit_connection(id);
                        ui.close();
                    }
                    if connected {
                        if ui
                            .add_enabled(
                                self.capabilities(Some(id)).table_designer,
                                egui::Button::new(l("Nowa tabela…", "New table…")),
                            )
                            .clicked()
                        {
                            self.open_designer(id);
                            ui.close();
                        }
                        if ui.button(self.refresh_label(id)).clicked() {
                            self.refresh_tables(Some(id));
                            ui.close();
                        }
                    }
                }
            });
            ui.menu_button(title(l("Narzędzia", "Tools")), |ui| {
                if ui.button(l("Historia zapytań", "Query history")).clicked() {
                    self.set_section(Section::History);
                    ui.close();
                }
                let offered = |app: &Self, section| app.section_blocked(section).is_none();
                if ui
                    .add_enabled(
                        offered(self, Section::Builder),
                        egui::Button::new(l("Kreator zapytań", "Query builder")),
                    )
                    .clicked()
                {
                    self.set_section(Section::Builder);
                    ui.close();
                }
                if ui
                    .add_enabled(
                        offered(self, Section::Transfer),
                        egui::Button::new(l("Import i eksport", "Import and export")),
                    )
                    .clicked()
                {
                    self.set_section(Section::Transfer);
                    ui.close();
                }
                if ui
                    .add_enabled(
                        offered(self, Section::Monitor),
                        egui::Button::new("Monitor"),
                    )
                    .clicked()
                {
                    self.set_section(Section::Monitor);
                    ui.close();
                }
                ui.separator();
                if ui
                    .button(l("Blokada aplikacji…", "Application lock…"))
                    .clicked()
                {
                    self.show_lock_settings = true;
                    ui.close();
                }
            });
            ui.menu_button(title(l("Pomoc", "Help")), |ui| {
                if ui
                    .add(
                        egui::Button::new(l("Skróty klawiaturowe", "Keyboard shortcuts"))
                            .shortcut_text("F1"),
                    )
                    .clicked()
                {
                    self.show_shortcuts = true;
                    ui.close();
                }
                if ui.button(l("Przewodnik", "Guide")).clicked() {
                    self.open_project(updater::GUIDE_URL);
                    ui.close();
                }
                if ui
                    .button(l("Kod źródłowy na GitHubie", "Source code on GitHub"))
                    .clicked()
                {
                    self.open_project(updater::SOURCE_URL);
                    ui.close();
                }
                if ui
                    .button(l("Wydania i pobieranie", "Releases and downloads"))
                    .clicked()
                {
                    self.open_project(updater::RELEASES_URL);
                    ui.close();
                }
                ui.separator();
                if ui.button(l("Aktualizacje…", "Updates…")).clicked() {
                    self.show_distribution = true;
                    if self.updater.is_none() {
                        self.init_distribution();
                    }
                    ui.close();
                }
                if ui.button(l("O programie", "About")).clicked() {
                    self.show_about = true;
                    ui.close();
                }
            });
        });
    }

    /// The ribbon at the top of the work area:
    /// new connection and new query, the tables and views of the schema in
    /// focus, then the tools. A narrow window keeps only the icons.
    fn section_strip(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let workspace = matches!(
            self.section,
            Section::Objects | Section::Table | Section::Design
        );
        let views = self.section == Section::Objects
            && self
                .objects
                .target
                .as_ref()
                .is_some_and(|t| t.kind == Some(ObjectKind::View));
        enum Act {
            Connection,
            Query,
            Objects(ObjectKind),
            Go(Section),
        }
        let items: [(Icon, &str, bool, Act); 10] = [
            (
                Icon::Database,
                l("Połączenie", "Connection"),
                false,
                Act::Connection,
            ),
            (
                Icon::Plus,
                l("Nowe zapytanie", "New query"),
                false,
                Act::Query,
            ),
            (
                Icon::Table,
                l("Tabela", "Table"),
                workspace && !views,
                Act::Objects(ObjectKind::Table),
            ),
            (
                Icon::View,
                l("Widok", "View"),
                views,
                Act::Objects(ObjectKind::View),
            ),
            (
                Icon::Terminal,
                self.section_label(Section::Sql),
                self.section == Section::Sql,
                Act::Go(Section::Sql),
            ),
            (
                Icon::Workflow,
                l("Kreator", "Builder"),
                self.section == Section::Builder,
                Act::Go(Section::Builder),
            ),
            (
                Icon::History,
                l("Historia", "History"),
                self.section == Section::History,
                Act::Go(Section::History),
            ),
            (
                Icon::Upload,
                l("Import i eksport", "Import and export"),
                self.section == Section::Transfer,
                Act::Go(Section::Transfer),
            ),
            (
                Icon::Shield,
                l("Połączenia", "Connections"),
                self.section == Section::Connections,
                Act::Go(Section::Connections),
            ),
            (
                Icon::Activity,
                "Monitor",
                self.section == Section::Monitor,
                Act::Go(Section::Monitor),
            ),
        ];
        let mut chosen = None;
        let height = ui.available_height();
        ui.horizontal(|ui| {
            ui.set_height(height);
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.add_space(6.0);
            let font = egui::FontId::proportional(p.small());
            let labels: f32 = items
                .iter()
                .map(|(_, label, _, _)| {
                    ui.painter()
                        .layout_no_wrap((*label).to_owned(), font.clone(), p.text)
                        .size()
                        .x
                        + 16.0
                })
                .sum();
            let buttons = 2.0 * ((p.control_height() - 3.0).max(24.0) + 2.0) + 30.0;
            let compact = labels + 16.0 > ui.available_width() - buttons;
            for (i, (icon, label, active, act)) in items.iter().enumerate() {
                // Separators after "new query" and after "view": the groups
                // of the ribbon.
                if i == 2 || i == 4 {
                    let x = ui.cursor().left() + 4.0;
                    let rect = ui.max_rect();
                    ui.painter().vline(
                        x,
                        (rect.top() + 12.0)..=(rect.bottom() - 12.0),
                        egui::Stroke::new(1.0, p.line),
                    );
                    ui.add_space(9.0);
                }
                let blocked = match act {
                    Act::Go(section) => self.section_blocked(*section).filter(|_| !active),
                    _ => None,
                };
                let response = ui
                    .add_enabled_ui(blocked.is_none(), |ui| {
                        widgets::ribbon_button(ui, &p, *icon, label, *active, compact, height)
                    })
                    .inner;
                match blocked {
                    Some(reason) => {
                        response.on_disabled_hover_text(reason);
                    }
                    None if response.clicked() => chosen = Some(i),
                    None => {}
                }
            }
            widgets::right(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                ui.add_space(6.0);
                if widgets::icon_toggle(
                    ui,
                    &p,
                    Icon::Focus,
                    l("Tryb skupienia", "Focus mode"),
                    self.focus_mode,
                )
                .clicked()
                {
                    self.focus_mode = !self.focus_mode;
                }
                if !self.focus_mode
                    && widgets::icon_toggle(
                        ui,
                        &p,
                        Icon::Sidebar,
                        l("Panel połączeń", "Connections panel"),
                        self.show_explorer,
                    )
                    .clicked()
                {
                    self.show_explorer = !self.show_explorer;
                }
            });
        });
        if let Some(i) = chosen {
            match items[i].3 {
                Act::Connection => self.new_connection(),
                Act::Query => {
                    let connection = self.focus_connection();
                    self.new_tab(connection);
                }
                Act::Objects(kind) => self.show_object_kind(kind),
                Act::Go(section) => self.set_section(section),
            }
        }
    }

    /// The tabs of the work area under the ribbon: "Obiekty"
    /// and one tab for each open table, closed with its cross.
    fn workspace_tabs(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let kind_word = if en { "Table" } else { "Tabela" };
        let tabs: Vec<(usize, Uuid, String, String)> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab.kind == TabKind::Table)
            .map(|(i, tab)| {
                let connection = tab
                    .connection
                    .and_then(|id| self.connection_name(id))
                    .unwrap_or_default();
                let schema = tab.source.as_ref().map(|t| t.schema.as_str()).unwrap_or("");
                let full = if schema.is_empty() {
                    format!("{} ({connection}) - {kind_word}", tab.title)
                } else {
                    format!("{} @{schema} ({connection}) - {kind_word}", tab.title)
                };
                let short = if full.chars().count() > 46 {
                    format!("{}…", full.chars().take(45).collect::<String>())
                } else {
                    full.clone()
                };
                (i, tab.id, short, full)
            })
            .collect();
        let design_word = if en { "Design" } else { "Projekt" };
        let unnamed = if en { "Untitled" } else { "Bez nazwy" };
        let designs: Vec<(usize, Uuid, String)> = self
            .designs
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let connection = self.connection_name(d.connection).unwrap_or_default();
                let name = if d.table.name.trim().is_empty() {
                    unnamed
                } else {
                    d.table.name.trim()
                };
                let title = if d.table.schema.is_empty() {
                    format!("{name} ({connection}) - {design_word}")
                } else {
                    format!("{name} @{} ({connection}) - {design_word}", d.table.schema)
                };
                (i, d.id, title)
            })
            .collect();
        let (mut open_design, mut close_design) = (None, None);
        let (mut open_objects, mut open, mut close) = (false, None, None);
        let height = ui.available_height();
        egui::ScrollArea::horizontal()
            .id_salt("workspace_tabs")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_height(height);
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let (r, _) = widgets::closable_tab(
                        ui,
                        &p,
                        Icon::Folder,
                        if en { "Objects" } else { "Obiekty" },
                        self.section == Section::Objects,
                        false,
                        height,
                    );
                    open_objects = r.clicked();
                    for (i, id, short, full) in &tabs {
                        let active = self.section == Section::Table && self.active == *i;
                        let (r, closed) =
                            widgets::closable_tab(ui, &p, Icon::Table, short, active, true, height);
                        if closed {
                            close = Some(*id);
                        } else if r.clicked() {
                            open = Some(*i);
                        }
                        r.on_hover_text(full);
                    }
                    for (i, id, title) in &designs {
                        let active = self.section == Section::Design && self.design_active == *i;
                        let short = if title.chars().count() > 46 {
                            format!("{}…", title.chars().take(45).collect::<String>())
                        } else {
                            title.clone()
                        };
                        let (r, closed) = widgets::closable_tab(
                            ui,
                            &p,
                            Icon::Pencil,
                            &short,
                            active,
                            true,
                            height,
                        );
                        if closed {
                            close_design = Some(*id);
                        } else if r.clicked() {
                            open_design = Some(*i);
                        }
                        r.on_hover_text(title);
                    }
                });
            });
        if open_objects {
            self.section = Section::Objects;
        }
        if let Some(i) = open_design {
            self.design_active = i;
            self.section = Section::Design;
        }
        if let Some(id) = close_design {
            self.close_design(id);
        }
        if let Some(i) = open {
            self.active = i;
            self.section = Section::Table;
        }
        if let Some(id) = close {
            let was_active = self.tabs.get(self.active).is_some_and(|t| t.id == id);
            self.close_tab(id);
            // The last table closed: back to the list of objects.
            if was_active && !self.tabs.iter().any(|t| t.kind == TabKind::Table) {
                self.section = Section::Objects;
            }
        }
    }

    /// Latest notice, above the status bar. A notice fades after a few
    /// seconds; an error stays until it is dismissed or replaced.
    fn notice_bar(&mut self, ui: &mut egui::Ui) {
        let Some((text, error)) = self.notice.clone() else {
            return;
        };
        if !error {
            let left = Duration::from_secs(6).saturating_sub(self.notice_time.elapsed());
            if left.is_zero() {
                self.notice = None;
                return;
            }
            ui.ctx().request_repaint_after(left);
        }
        let p = self.palette();
        let close = self.tr("Zamknij komunikat", "Dismiss message");
        let mut dismiss = false;
        egui::Panel::bottom("notice")
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(p.raised)
                    .inner_margin(egui::Margin::symmetric(16, 6)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_min_height(p.control_height() - 3.0);
                    widgets::right(ui, |ui| {
                        dismiss = widgets::icon_button(ui, &p, Icon::Close, close).clicked();
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.add(
                                egui::Label::new(
                                    RichText::new(text.chars().take(400).collect::<String>())
                                        .size(p.body() - 0.5)
                                        .color(if error { p.error } else { p.accent }),
                                )
                                .truncate(),
                            )
                            .on_hover_text(&text);
                        });
                    });
                });
            });
        if dismiss {
            self.notice = None;
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let left = match self.shown_connection() {
            Some(id) => {
                let profile = self.profiles.connections.iter().find(|c| c.id == id);
                let state = if self.databases.contains_key(&id) {
                    l("połączono", "connected")
                } else if self.connect_requests.contains_key(&id) {
                    l("łączenie…", "connecting…")
                } else {
                    l("rozłączono", "disconnected")
                };
                profile.map_or_else(String::new, |profile| {
                    format!("{} · {} · {state}", profile.name, profile.product_label())
                })
            }
            None => l("Gotowy", "Ready").to_owned(),
        };
        let rows = self.tabs[self.active]
            .result
            .as_ref()
            .filter(|_| {
                matches!(
                    self.section,
                    Section::Sql | Section::Table | Section::Builder
                )
            })
            .map(|result| {
                format!(
                    "{} {} · ",
                    result.rows.len(),
                    row_word(result.rows.len(), en)
                )
            })
            .unwrap_or_default();
        let left = format!(
            "{left} · {}",
            l(
                "profile i hasła w zaszyfrowanym sejfie lokalnym",
                "profiles and passwords in the encrypted local vault"
            )
        );
        let font = egui::FontId::proportional(p.small());
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 7.0;
            widgets::status_dot(ui, theme::BRAND);
            let right = ui.painter().layout_no_wrap(
                format!("{rows}Baza Studio {}", env!("CARGO_PKG_VERSION")),
                font.clone(),
                p.muted,
            );
            let room = (ui.available_width() - right.size().x - 24.0).max(40.0);
            let galley = widgets::one_line(ui.painter(), &left, font.clone(), p.muted, room);
            let (rect, _) = ui.allocate_exact_size(galley.size(), egui::Sense::hover());
            ui.painter().galley(rect.min, galley, p.muted);
            widgets::right(ui, |ui| {
                let (rect, _) = ui.allocate_exact_size(right.size(), egui::Sense::hover());
                ui.painter().galley(rect.min, right, p.muted);
            });
        });
    }

    /// Start a new connection group below `parent`.
    pub(super) fn new_group(&mut self, parent: Option<Uuid>) {
        self.group_dialog = Some(GroupEditor {
            id: None,
            name: String::new(),
            color: DEFAULT_GROUP_COLOR,
            parent,
        });
    }
}
