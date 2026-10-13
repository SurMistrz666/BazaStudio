//! "Połączenia": the profile of the connection in focus, the connection
//! groups, and the import and export of profiles.
use super::*;
use crate::widgets::Btn;
use eframe::egui::RichText;

/// One fact of a profile: a small muted label above its value. A file path
/// stays on one line: a long one is shortened in the middle, and the whole
/// path is shown when the pointer rests on it.
fn fact(ui: &mut egui::Ui, p: &theme::Palette, label: &str, value: &str, path: bool) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 5.0;
        widgets::label(ui, p, label);
        if path {
            let font = egui::FontId::proportional(p.body());
            let (shown, shortened) =
                widgets::middle_ellipsis(ui.painter(), value, &font, ui.available_width());
            let response =
                ui.add(egui::Label::new(RichText::new(shown).font(font).color(p.text)).extend());
            if shortened {
                response.on_hover_text(value);
            }
        } else {
            ui.add(egui::Label::new(RichText::new(value).size(p.body()).color(p.text)).wrap());
        }
    });
}

impl StudioApp {
    pub(super) fn connections_view(&mut self, ui: &mut egui::Ui) {
        if self.profile_exchange.is_some() {
            self.profile_exchange_view(ui);
            return;
        }
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let focus = self
            .focus_connection()
            .and_then(|id| self.profiles.connections.iter().find(|c| c.id == id))
            .cloned()
            .map(SecretDraft);
        egui::ScrollArea::vertical()
            .id_salt("connections_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::NONE
                    .inner_margin(egui::Margin::same(20))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 8.0;
                        match &focus {
                            Some(profile) => self.profile_panel(ui, &profile.0),
                            None => self.connections_overview(ui),
                        }
                        ui.add_space(16.0);
                        self.groups_panel(ui);
                        ui.add_space(6.0);
                        if widgets::link(
                            ui,
                            &p,
                            &format!(
                                "{} →",
                                l(
                                    "Jak skonfigurować TLS i SSH",
                                    "How to configure TLS and SSH"
                                )
                            ),
                        )
                        .clicked()
                        {
                            self.open_project(updater::GUIDE_URL);
                        }
                    });
            });
    }

    /// Nothing is in focus: an empty state, or the list of saved connections.
    fn connections_overview(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let mut add = false;
        widgets::pane_heading(
            ui,
            &p,
            l("Połączenia", "Connections"),
            l(
                "Zapisane profile baz danych. Hasła i klucze są zaszyfrowane w sejfie tego komputera.",
                "Saved database profiles. Passwords and keys are encrypted in this computer's vault.",
            ),
            |ui| {
                if !self.profiles.connections.is_empty() {
                    add = Btn::primary(l("Nowe połączenie", "New connection"))
                        .icon(Icon::Plus)
                        .show(ui, &p)
                        .clicked();
                }
            },
        );
        if self.profiles.connections.is_empty() {
            add = widgets::empty_state(
                ui,
                &p,
                Icon::Database,
                l("Jeszcze bez połączeń", "No connections yet"),
                l(
                    "Dodaj bazę PostgreSQL, MariaDB / MySQL albo plik SQLite. Do serwera za zaporą połączysz się przez tunel SSH.",
                    "Add a PostgreSQL or MariaDB / MySQL database, or an SQLite file. A server behind a firewall is reached through an SSH tunnel.",
                ),
                Some(l("Nowe połączenie", "New connection")),
            );
        } else {
            let rows = self
                .profiles
                .connections
                .iter()
                .map(|c| {
                    (
                        c.id,
                        c.name.clone(),
                        super::sidebar::connection_subtitle(c, en),
                    )
                })
                .collect::<Vec<_>>();
            widgets::hline(ui, &p);
            for (id, name, subtitle) in rows {
                let connected = self.databases.contains_key(&id);
                ui.horizontal(|ui| {
                    ui.set_min_height(p.control_height() + 14.0);
                    ui.spacing_mut().item_spacing.x = 10.0;
                    widgets::icon(ui, Icon::Database, 15.0, p.accent);
                    ui.label(RichText::new(name).size(p.body()).color(p.text));
                    ui.label(RichText::new(subtitle).size(p.small()).color(p.muted));
                    widgets::right(ui, |ui| {
                        if Btn::new(l("Pokaż profil", "Show profile"))
                            .small()
                            .show(ui, &p)
                            .clicked()
                        {
                            self.navigation_selection = Some(NodeIdentity::Connection(id));
                        }
                        ui.label(
                            RichText::new(if connected {
                                l("połączono", "connected")
                            } else {
                                l("rozłączono", "disconnected")
                            })
                            .size(p.small())
                            .color(if connected {
                                p.positive
                            } else {
                                p.muted
                            }),
                        );
                    });
                });
                widgets::hline(ui, &p);
            }
        }
        if add {
            self.new_connection();
        }
    }

    /// The saved profile of one connection: its route, its facts, its actions.
    fn profile_panel(&mut self, ui: &mut egui::Ui, profile: &ConnectionProfile) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let id = profile.id;
        let connected = self.databases.contains_key(&id);
        let connecting = self.connect_requests.contains_key(&id);
        widgets::pane_heading(
            ui,
            &p,
            l("Profil połączenia", "Connection profile"),
            &format!(
                "{}. {}",
                profile.name,
                l(
                    "Ustawienia zapisane na tym komputerze; hasła i klucze są zaszyfrowane w lokalnym sejfie.",
                    "Settings saved on this computer; passwords and keys are encrypted in the local vault.",
                )
            ),
            |ui| {
                if connected {
                    widgets::badge(ui, &p, l("Połączono", "Connected"), p.positive);
                } else if connecting {
                    widgets::badge(ui, &p, l("Łączenie", "Connecting"), p.accent);
                } else {
                    widgets::badge(ui, &p, l("Rozłączono", "Disconnected"), p.muted);
                }
            },
        );

        // Route from this computer to the database.
        egui::Frame::NONE
            .stroke(egui::Stroke::new(1.0, p.line))
            .inner_margin(egui::Margin::same(20))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 9.0;
                    let step = |ui: &mut egui::Ui, icon: Icon, text: &str| {
                        widgets::icon(ui, icon, 18.0, p.accent);
                        ui.label(RichText::new(text).size(p.body() - 0.5).color(p.text));
                    };
                    let arrow = |ui: &mut egui::Ui| {
                        ui.add_space(5.0);
                        widgets::icon(ui, Icon::ChevronRight, 14.0, p.muted);
                        ui.add_space(5.0);
                    };
                    step(ui, Icon::Terminal, l("Twój komputer", "Your computer"));
                    if profile.ssh.is_some() {
                        arrow(ui);
                        step(ui, Icon::Shield, "SSH");
                    }
                    arrow(ui);
                    if profile.is_file() {
                        let label = profile.product_label();
                        let text = if en {
                            format!("{label} file")
                        } else {
                            format!("Plik {label}")
                        };
                        step(ui, Icon::File, &text);
                    } else {
                        let tls = match profile.tls {
                            TlsMode::Disable => l("bez TLS", "no TLS"),
                            _ => "TLS",
                        };
                        widgets::product_badge(ui, profile.product_spec().badge, 18.0);
                        ui.label(
                            RichText::new(format!("{} · {tls}", profile.product_label()))
                                .size(p.body() - 0.5)
                                .color(p.text),
                        );
                    }
                });
            });
        ui.add_space(12.0);

        let database = if profile.is_file() {
            profile
                .sqlite_path
                .as_ref()
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            profile.database.clone()
        };
        let host = if profile.is_file() {
            profile
                .sqlite_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            format!("{}:{}", profile.host, profile.port)
        };
        let gateway = match &profile.ssh {
            Some(ssh) => format!("{}@{}:{}", ssh.username, ssh.host, ssh.port),
            None => l("bez tunelu SSH", "no SSH tunnel").to_owned(),
        };
        let mut checks = Vec::new();
        if profile.engine.spec().form.tls {
            checks.push(match profile.tls {
                TlsMode::VerifyFull => l("certyfikat i nazwa hosta", "certificate and hostname"),
                TlsMode::Require => l(
                    "szyfrowanie bez sprawdzania certyfikatu",
                    "encryption without certificate check",
                ),
                TlsMode::Disable => l("połączenie nieszyfrowane", "unencrypted connection"),
            });
        }
        if let Some(ssh) = &profile.ssh {
            checks.push(if ssh.fingerprint.is_empty() {
                l(
                    "klucz serwera SSH zostanie zapamiętany przy pierwszym połączeniu",
                    "the SSH server key is remembered on the first connection",
                )
            } else {
                l("zapisany odcisk klucza SSH", "saved SSH key fingerprint")
            });
        }
        if checks.is_empty() {
            checks.push(l("plik lokalny", "local file"));
        }
        let group = self
            .group_snapshot
            .memberships
            .get(&id)
            .map(|group| self.group_path(*group))
            .filter(|path| !path.is_empty())
            .unwrap_or_else(|| l("Bez grupy", "Ungrouped").to_owned());
        let user = if profile.username.is_empty() {
            "—".to_owned()
        } else {
            profile.username.clone()
        };
        let verification = checks.join(" + ");
        // The label, the value and whether the value is a file path.
        let facts = if profile.is_file() {
            vec![
                (l("Baza danych", "Database"), database, false),
                (l("Plik", "File"), host, true),
                (l("Weryfikacja", "Verification"), verification, false),
                (l("Grupa", "Group"), group, false),
            ]
        } else {
            vec![
                (l("Baza danych", "Database"), database, false),
                (l("Host bazy", "Database host"), host, false),
                (l("Brama SSH", "SSH gateway"), gateway, false),
                (l("Weryfikacja", "Verification"), verification, false),
                (l("Użytkownik bazy", "Database user"), user, false),
                (l("Grupa", "Group"), group, false),
            ]
        };
        let column = ((ui.available_width() - 24.0) / 2.0).max(160.0);
        for pair in facts.chunks(2) {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 24.0;
                for (label, value, path) in pair {
                    ui.allocate_ui_with_layout(
                        egui::vec2(column, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(column);
                            fact(ui, &p, label, value, *path);
                        },
                    );
                }
            });
            ui.add_space(10.0);
        }

        let (mut connect, mut disconnect, mut edit, mut query) = (false, false, false, false);
        let (mut delete, mut add) = (false, false);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if connected {
                disconnect = Btn::new(l("Rozłącz", "Disconnect")).show(ui, &p).clicked();
            } else {
                connect = Btn::primary(l("Połącz", "Connect"))
                    .enabled(!connecting)
                    .show(ui, &p)
                    .clicked();
            }
            edit = Btn::new(l("Edytuj…", "Edit…"))
                .icon(Icon::Pencil)
                .show(ui, &p)
                .clicked();
            query = Btn::new(l("Nowe zapytanie", "New query"))
                .icon(Icon::Terminal)
                .show(ui, &p)
                .clicked();
            delete = Btn::danger(l("Usuń połączenie", "Delete connection"))
                .icon(Icon::Trash)
                .show(ui, &p)
                .clicked();
            add = Btn::new(l("Nowe połączenie", "New connection"))
                .icon(Icon::Plus)
                .show(ui, &p)
                .clicked();
        });
        if connect {
            self.connect(id);
        }
        if disconnect {
            self.disconnect(id);
        }
        if edit {
            self.edit_connection(id);
        }
        if query {
            self.new_tab(Some(id));
        }
        if delete {
            self.connection_delete = Some(id);
        }
        if add {
            self.new_connection();
        }
    }

    /// Connection groups: add one by name, rename or remove the existing ones.
    fn groups_panel(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        widgets::subheading(ui, &p, l("Grupy połączeń", "Connection groups"));
        widgets::help(
            ui,
            &p,
            l(
                "Grupy porządkują połączenia w panelu po lewej. Usunięcie grupy nie usuwa jej połączeń.",
                "Groups organise the connections in the panel on the left. Removing a group does not remove its connections.",
            ),
        );
        ui.add_space(4.0);
        let mut create = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 9.0;
            let response = ui.add(
                widgets::field(&p, &mut self.group_draft)
                    .desired_width(190.0)
                    .char_limit(64)
                    .hint_text(l("Nazwa grupy", "Group name")),
            );
            create = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            create |= Btn::new(l("Dodaj grupę", "Add group"))
                .enabled(!self.group_busy && !self.group_draft.trim().is_empty())
                .show(ui, &p)
                .clicked();
        });
        if create && !self.group_draft.trim().is_empty() {
            self.group_operation(GroupOperation::Create(GroupEditor {
                id: None,
                name: self.group_draft.trim().to_owned(),
                color: DEFAULT_GROUP_COLOR,
                parent: None,
            }));
            if self.group_busy {
                self.group_draft.clear();
            }
        }
        ui.add_space(8.0);
        let groups = self.group_snapshot.groups.clone();
        if groups.is_empty() {
            return;
        }
        let alive = self
            .profiles
            .connections
            .iter()
            .map(|profile| profile.id)
            .collect();
        widgets::hline(ui, &p);
        for group in groups {
            let count =
                navigator::descendant_connection_count(&self.group_snapshot, group.id, &alive);
            let path = self.group_path(group.id);
            let (mut edit, mut remove) = (false, false);
            ui.horizontal(|ui| {
                ui.set_min_height(p.control_height() + 12.0);
                ui.spacing_mut().item_spacing.x = 10.0;
                widgets::icon(
                    ui,
                    Icon::Folder,
                    15.0,
                    if group.color == DEFAULT_GROUP_COLOR {
                        p.accent
                    } else {
                        Color32::from_rgb(group.color[0], group.color[1], group.color[2])
                    },
                );
                ui.add(
                    egui::Label::new(RichText::new(path).size(p.body()).color(p.text)).truncate(),
                );
                widgets::right(ui, |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    remove = Btn::new(l("Usuń grupę", "Remove group"))
                        .icon(Icon::Close)
                        .small()
                        .show(ui, &p)
                        .clicked();
                    edit = Btn::new(l("Zmień", "Edit")).small().show(ui, &p).clicked();
                    ui.label(
                        RichText::new(format!(
                            "{count} {}",
                            if en {
                                if count == 1 {
                                    "connection"
                                } else {
                                    "connections"
                                }
                            } else if count == 1 {
                                "połączenie"
                            } else if (2..=4).contains(&(count % 10))
                                && !(12..=14).contains(&(count % 100))
                            {
                                "połączenia"
                            } else {
                                "połączeń"
                            }
                        ))
                        .size(p.small())
                        .color(p.muted),
                    );
                });
            });
            widgets::hline(ui, &p);
            if edit {
                self.edit_group(&group);
            }
            if remove {
                self.group_remove = Some(group.id);
            }
        }
    }

    /// Import or export of connection profiles, shown in place of the profile.
    fn profile_exchange_view(&mut self, ui: &mut egui::Ui) {
        let Some(mut state) = self.profile_exchange.take() else {
            return;
        };
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let mut back = false;
        let mut preview_action = false;
        let mut import_action = false;
        let mut export_action = false;
        let mut correction = None;
        egui::ScrollArea::vertical()
            .id_salt("profile_exchange_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::NONE
                    .inner_margin(egui::Margin::same(20))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 9.0;
                        widgets::pane_heading(
                            ui,
                            &p,
                            if state.importing {
                                l("Import profili", "Import profiles")
                            } else {
                                l("Eksport profili", "Export profiles")
                            },
                            if state.importing {
                                l(
                                    "Połączenia z pliku otrzymają nowe identyfikatory. Wpisy z brakami wymagają uzupełnienia przed importem.",
                                    "Connections from the file receive new identifiers. Incomplete entries need to be completed before the import.",
                                )
                            } else {
                                l(
                                    "Wybierz profile, które chcesz przenieść. Zaszyfrowany plik BZX może zawierać hasła i klucze; JSON zawiera same ustawienia.",
                                    "Choose the profiles to move. An encrypted BZX file can carry passwords and keys; JSON carries settings only.",
                                )
                            },
                            |ui| {
                                back = Btn::new(l("Wróć", "Back"))
                                    .icon(Icon::ChevronLeft)
                                    .show(ui, &p)
                                    .clicked();
                            },
                        );
                        if state.importing {
                            widgets::label(ui, &p, l("Plik", "File"));
                            ui.label(RichText::new(&state.filename).size(p.body()).color(p.text));
                            ui.add_space(4.0);
                            if state.preview.is_none() {
                                widgets::label(
                                    ui,
                                    &p,
                                    l(
                                        "Hasło zaszyfrowanego pliku BZX (dla JSON zostaw puste)",
                                        "Passphrase of the encrypted BZX file (leave empty for JSON)",
                                    ),
                                );
                                if ui
                                    .add(
                                        widgets::field(&p, &mut state.passphrase)
                                            .password(true)
                                            .desired_width(340.0),
                                    )
                                    .changed()
                                {
                                    state.id = Uuid::new_v4();
                                    state.busy = false;
                                    state.preview = None;
                                    state.selected.clear();
                                }
                                ui.add_space(4.0);
                                ui.horizontal(|ui| {
                                    preview_action =
                                        Btn::primary(l("Przejrzyj profile", "Review profiles"))
                                            .enabled(!state.busy)
                                            .show(ui, &p)
                                            .clicked();
                                    if state.busy {
                                        ui.spinner();
                                    }
                                });
                            } else if let Some(preview) = &state.preview {
                                for warning in &preview.warnings {
                                    widgets::feedback(
                                        ui,
                                        &p,
                                        &crate::messages::describe(warning, en),
                                        true,
                                    );
                                }
                                widgets::hline(ui, &p);
                                if !preview.rows.iter().any(|r| r.kind == "database") {
                                    widgets::help(
                                        ui,
                                        &p,
                                        l(
                                            "Ten plik nie zawiera połączeń z bazami danych.",
                                            "This file contains no database connections.",
                                        ),
                                    );
                                }
                                // Only database connections are offered. Any
                                // other entry of the file is left out.
                                for row in preview.rows.iter().filter(|r| r.kind == "database") {
                                    ui.horizontal(|ui| {
                                        ui.set_min_height(p.control_height() + 8.0);
                                        ui.spacing_mut().item_spacing.x = 10.0;
                                        let mut selected = state.selected.contains(&row.id);
                                        if widgets::checkbox(ui, &p, &mut selected, &row.name)
                                            .changed()
                                        {
                                            if selected {
                                                state.selected.push(row.id);
                                            } else {
                                                state.selected.retain(|id| id != &row.id);
                                            }
                                        }
                                        ui.label(
                                            RichText::new(
                                                row.engine
                                                    .as_deref()
                                                    .and_then(Engine::from_id)
                                                    .map_or("", |engine| {
                                                        engine.product(&row.product).label
                                                    }),
                                            )
                                            .size(p.small())
                                            .color(p.muted),
                                        );
                                        if !row.can_apply {
                                            ui.label(
                                                RichText::new(
                                                    row.errors
                                                        .iter()
                                                        .map(|e| crate::messages::describe(e, en))
                                                        .collect::<Vec<_>>()
                                                        .join(" "),
                                                )
                                                .size(p.small())
                                                .color(p.error),
                                            );
                                        }
                                        widgets::right(ui, |ui| {
                                            if Btn::new(l("Uzupełnij", "Complete"))
                                                .small()
                                                .show(ui, &p)
                                                .clicked()
                                            {
                                                correction = Some(row.id);
                                            }
                                        });
                                    });
                                    widgets::hline(ui, &p);
                                }
                                ui.add_space(4.0);
                                widgets::checkbox(
                                    ui,
                                    &p,
                                    &mut state.include_secrets,
                                    l(
                                        "Zaimportuj także hasła i klucze z zaszyfrowanego pliku BZX",
                                        "Also import passwords and keys from the encrypted BZX file",
                                    ),
                                );
                                ui.add_space(4.0);
                                import_action =
                                    Btn::primary(l("Importuj zaznaczone", "Import selected"))
                                        .icon(Icon::Check)
                                        .enabled(!state.selected.is_empty() && !state.busy)
                                        .show(ui, &p)
                                        .clicked();
                            }
                        } else {
                            widgets::hline(ui, &p);
                            let rows = self
                                .profiles
                                .connections
                                .iter()
                                .map(|c| (c.id, c.name.clone(), c.product_label().to_owned()))
                                .collect::<Vec<_>>();
                            for (id, name, kind) in rows {
                                ui.horizontal(|ui| {
                                    ui.set_min_height(p.control_height() + 8.0);
                                    ui.spacing_mut().item_spacing.x = 10.0;
                                    let mut selected = state.selected.contains(&id);
                                    if widgets::checkbox(ui, &p, &mut selected, &name).changed() {
                                        if selected {
                                            state.selected.push(id);
                                        } else {
                                            state.selected.retain(|v| v != &id);
                                        }
                                    }
                                    ui.label(RichText::new(kind).size(p.small()).color(p.muted));
                                });
                                widgets::hline(ui, &p);
                            }
                            ui.add_space(4.0);
                            widgets::checkbox(
                                ui,
                                &p,
                                &mut state.encrypted,
                                l("Zaszyfrowany plik BZX (AES-256-GCM)", "Encrypted BZX file (AES-256-GCM)"),
                            );
                            if state.encrypted {
                                widgets::label(
                                    ui,
                                    &p,
                                    l(
                                        "Hasło pliku: co najmniej 12 znaków",
                                        "File passphrase: at least 12 characters",
                                    ),
                                );
                                ui.add(
                                    widgets::field(&p, &mut state.passphrase)
                                        .password(true)
                                        .desired_width(340.0),
                                );
                                widgets::checkbox(
                                    ui,
                                    &p,
                                    &mut state.include_secrets,
                                    l(
                                        "Dołącz hasła i klucze prywatne",
                                        "Include passwords and private keys",
                                    ),
                                );
                            } else {
                                state.include_secrets = false;
                                widgets::help(
                                    ui,
                                    &p,
                                    l(
                                        "JSON zawiera wyłącznie ustawienia, bez haseł i kluczy prywatnych.",
                                        "JSON contains settings only, without passwords or private keys.",
                                    ),
                                );
                            }
                            ui.add_space(4.0);
                            export_action = Btn::primary(l("Zapisz eksport…", "Save export…"))
                                .icon(Icon::Download)
                                .enabled(
                                    !state.selected.is_empty()
                                        && (!state.encrypted || state.passphrase.len() >= 12),
                                )
                                .show(ui, &p)
                                .clicked();
                        }
                    });
            });
        if back {
            // Dropping the state erases the file contents and the passphrase.
            return;
        }
        if preview_action {
            state.busy = true;
            let request = state.id;
            let bytes = state.bytes.clone();
            let phrase = state.passphrase.clone();
            let tx = self.tx.clone();
            let ctx = self.ctx.clone();
            self.track(async move {
                let r = tokio::task::spawn_blocking(move || {
                    profiles::preview_import(
                        &bytes,
                        if phrase.is_empty() {
                            None
                        } else {
                            Some(&phrase)
                        },
                    )
                })
                .await
                .map_err(|_| "profile_task_failed".to_owned())
                .and_then(|r| r.map_err(|e| e.to_string()));
                let _ = tx.send(Message::ProfilePreview(request, r)).await;
                ctx.request_repaint();
            });
        }
        if let Some(id) = correction
            && let Some(p) = state
                .preview
                .as_ref()
                .and_then(|p| p.template_connection(id))
        {
            self.connection_import_id = Some(id);
            self.connection_dialog = Some(p);
        }
        if import_action && let Some(p) = &state.preview {
            match profiles::apply_import(&self.profiles, p, &state.selected, state.include_secrets)
            {
                Ok(next) => {
                    self.replace_profiles(next);
                    self.save();
                    self.notify(
                        l("Zaimportowano profile.", "Profiles imported.").into(),
                        false,
                    );
                    return;
                }
                Err(e) => self.notify(e.to_string(), true),
            }
        }
        if export_action {
            let mut set = self.profiles.clone();
            set.connections.retain(|p| state.selected.contains(&p.id));
            // The export carries database connections only.
            set.tunnels.clear();
            let phrase = state.passphrase.clone();
            let encrypted = state.encrypted;
            let include = state.include_secrets;
            let exported = l("Wyeksportowano profile.", "Profiles exported.");
            let tx = self.tx.clone();
            let ctx = self.ctx.clone();
            self.track(async move {
                let ext = if encrypted { "bzx" } else { "json" };
                if let Some(f) = rfd::AsyncFileDialog::new()
                    .add_filter("Baza Studio", &[ext])
                    .set_file_name(format!("baza-profiles.{ext}"))
                    .save_file()
                    .await
                {
                    let r = async {
                        let bytes = tokio::task::spawn_blocking(move || {
                            if encrypted {
                                profiles::export_portable(&set, &phrase, include)
                            } else {
                                profiles::export_json(&set)
                            }
                        })
                        .await
                        .map_err(|_| anyhow::anyhow!("profile_task_failed"))??;
                        tokio::fs::write(f.path(), bytes)
                            .await
                            .map_err(|_| anyhow::anyhow!("profile_export_failed"))?;
                        Ok::<_, anyhow::Error>(exported.into())
                    }
                    .await
                    .map_err(|e| e.to_string());
                    let _ = tx.send(Message::Exported(r)).await;
                    ctx.request_repaint();
                }
            });
            return;
        }
        self.profile_exchange = Some(state);
    }
}
