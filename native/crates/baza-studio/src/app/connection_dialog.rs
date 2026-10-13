//! The connection editor: a dialog with the tabs "Ogólne", "SSH" and
//! "SSL/TLS", a connection test on the left and the save action on the right.
use super::*;
use crate::widgets::Btn;
use eframe::egui::RichText;

/// Width of the dialog and of the label column of its forms.
const WIDTH: f32 = 640.0;
const LABELS: f32 = 190.0;
/// Width of an input next to the label column. It leaves room for the scroll
/// bar of a long tab.
const INPUT: f32 = WIDTH - 40.0 - LABELS - 12.0 - 8.0;

/// Height of the form of a tab together with the result of a test under it.
const FORM_HEIGHT: f32 = 372.0;
/// The form keeps at least this much, however long a result is.
const FORM_HEIGHT_MIN: f32 = 120.0;

/// Result of a connection test: the message and, when the server presented a
/// key other than the saved one, that key with the action that trusts it.
/// Returns whether the action was chosen.
fn test_result(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    feedback: Option<&(String, bool)>,
    presented: Option<&HostKeyScan>,
    trust: &str,
) -> bool {
    let mut trusted = false;
    if let Some((message, error)) = feedback {
        widgets::feedback(ui, p, message, *error);
    }
    if let Some(key) = presented {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!("{} {}", key.algorithm, key.fingerprint))
                    .monospace()
                    .size(p.code() - 1.5),
            );
            trusted = Btn::new(trust).small().show(ui, p).clicked();
        });
    }
    trusted
}

/// Which path of the draft a file dialog was opened for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PathTarget {
    /// The database file of an engine that is a local file.
    DatabaseFile,
    CaCertificate,
}

/// Number input in the look of a text field.
fn port_field(ui: &mut egui::Ui, p: &theme::Palette, value: &mut u16) {
    ui.scope(|ui| {
        let widgets = &mut ui.visuals_mut().widgets;
        for visuals in [
            &mut widgets.inactive,
            &mut widgets.hovered,
            &mut widgets.active,
        ] {
            visuals.weak_bg_fill = p.bg;
            visuals.bg_fill = p.bg;
        }
        ui.spacing_mut().button_padding = egui::vec2(9.0, 5.0);
        ui.add_sized(
            [92.0, p.control_height()],
            egui::DragValue::new(value).range(1..=65535).speed(0.0),
        );
    });
}

impl StudioApp {
    pub(super) fn connection_window(&mut self, ctx: &egui::Context) {
        let Some(mut draft) = self.connection_dialog.take() else {
            return;
        };
        if self.connection_editor.profile != Some(draft.id) {
            self.connection_editor.open_draft(&draft);
        }
        let editor = self.connection_editor.id;
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let mut section = self.connection_editor.section;
        let mut save = false;
        let mut test = false;
        let mut cancel = false;
        let busy = self.connection_editor.busy;
        let saving = self.connection_editor.saving;
        let title = if self.profiles.connections.iter().any(|c| c.id == draft.id) {
            l("Edytuj połączenie", "Edit connection")
        } else {
            l("Nowe połączenie", "New connection")
        };
        let can_save = !busy
            && !self.saving
            && self.pending_save.is_none()
            && (self.connection_import_id.is_some()
                || (!self.group_busy && !self.group_loading && self.group_store.is_some()));
        let dialog = widgets::dialog(
            ctx,
            &p,
            "connection_editor",
            title,
            WIDTH,
            l("Zamknij", "Close"),
            |ui| {
                // Tab row.
                egui::Frame::NONE.fill(p.side).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        for (id, text) in [(0, l("Ogólne", "General")), (1, "SSH"), (2, "SSL/TLS")]
                        {
                            ui.add_enabled_ui(!busy, |ui| {
                                if widgets::tab(ui, &p, None, text, section == id, 18.0, true, 34.0)
                                    .clicked()
                                {
                                    section = id;
                                }
                            });
                        }
                    });
                });
                widgets::hline(ui, &p);
                widgets::dialog_body(ui, |ui| {
                    // The form and the result of a test share one area of
                    // constant height: a result takes its room from the form
                    // above it, so the dialog neither grows nor moves when a
                    // result appears or goes, on any tab. Its height is
                    // measured first, without painting it.
                    let feedback = self.connection_editor.feedback.clone();
                    let presented = self.connection_editor.changed_key.clone();
                    let trust = l("Zaufaj nowemu kluczowi", "Trust the new key");
                    let has_result = feedback.is_some() || presented.is_some();
                    let result_height = if has_result {
                        let mut measure = ui.new_child(
                            egui::UiBuilder::new()
                                .id_salt("connection_result_size")
                                .max_rect(ui.available_rect_before_wrap())
                                .invisible(),
                        );
                        test_result(
                            &mut measure,
                            &p,
                            feedback.as_ref(),
                            presented.as_ref(),
                            trust,
                        );
                        measure.min_rect().height()
                    } else {
                        0.0
                    };
                    let form_height = if has_result {
                        (FORM_HEIGHT - result_height - ui.spacing().item_spacing.y)
                            .max(FORM_HEIGHT_MIN)
                    } else {
                        FORM_HEIGHT
                    };
                    egui::ScrollArea::vertical()
                        .id_salt(("connection_editor_body", section))
                        .auto_shrink([false, false])
                        .max_height(form_height)
                        .min_scrolled_height(form_height)
                        .show(ui, |ui| {
                            ui.add_enabled_ui(!busy, |ui| {
                                ui.spacing_mut().item_spacing.y = 10.0;
                                match section {
                                    1 => self.ssh_fields(ui, &mut draft),
                                    2 => self.connection_tls_fields(ui, &mut draft),
                                    _ => self.connection_general_fields(ui, &mut draft),
                                }
                                ui.add_space(6.0);
                            });
                        });
                    if has_result {
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), result_height),
                            egui::Sense::hover(),
                        );
                        let mut result = ui.new_child(
                            egui::UiBuilder::new()
                                .id_salt("connection_result")
                                .max_rect(rect),
                        );
                        if test_result(
                            &mut result,
                            &p,
                            feedback.as_ref(),
                            presented.as_ref(),
                            trust,
                        ) && let Some(key) = &presented
                        {
                            if let Some(ssh) = draft.ssh.as_mut() {
                                ssh.fingerprint = key.fingerprint.clone();
                            }
                            self.connection_editor.changed_key = None;
                        }
                    }
                    ui.label(
                        RichText::new(l(
                            "Hasła i klucze są zapisywane w zaszyfrowanym sejfie tego użytkownika systemu.",
                            "Passwords and keys are stored in the encrypted vault of this system user.",
                        ))
                        .size(p.small())
                        .color(p.muted),
                    );
                });
                widgets::dialog_footer(ui, &p, |ui| {
                    test = Btn::new(l("Test połączenia", "Test connection"))
                        .enabled(!busy)
                        .show(ui, &p)
                        .clicked();
                    if busy {
                        ui.spinner();
                        ui.label(
                            RichText::new(if saving {
                                l("Zapisywanie profilu…", "Saving the profile…")
                            } else {
                                l("Sprawdzanie połączenia…", "Checking the connection…")
                            })
                            .size(p.small() + 0.5)
                            .color(p.muted),
                        );
                    }
                    widgets::right(ui, |ui| {
                        save = Btn::primary(l("Zapisz", "Save"))
                            .enabled(can_save)
                            .min_width(84.0)
                            .show(ui, &p)
                            .clicked();
                        cancel = Btn::new(l("Anuluj", "Cancel"))
                            .enabled(!saving)
                            .min_width(84.0)
                            .show(ui, &p)
                            .clicked();
                    });
                });
            },
        );
        self.connection_editor.section = section;
        let fingerprint = connection_edit::fingerprint(&draft).ok();
        if self.connection_editor.fingerprint.is_some()
            && self.connection_editor.fingerprint != fingerprint
            && !busy
        {
            self.connection_editor.feedback = None;
        }
        self.connection_editor.fingerprint = fingerprint;
        // A click beside the dialog does not close it: the form may hold a
        // password and a loaded key, and such a click is common right after
        // the system file dialog. The close button, Escape and "Anuluj" do.
        if cancel || (!saving && dialog.close && !dialog.outside) {
            if let Some(job) = self.connection_probe_job.take() {
                job.abort();
            }
            self.connection_editor.close();
            self.connection_form_group = None;
            self.connection_import_id = None;
            drop(SecretDraft(draft));
            return;
        }
        if save {
            if let Some(id) = self.connection_import_id {
                if let Some(preview) = self
                    .profile_exchange
                    .as_mut()
                    .and_then(|s| s.preview.as_mut())
                {
                    match preview.correct_connection(id, &draft) {
                        Ok(()) => {
                            self.connection_import_id = None;
                            self.connection_editor.close();
                            drop(SecretDraft(draft));
                            return;
                        }
                        Err(e) => {
                            self.connection_editor.feedback =
                                Some((self.explain(e.to_string()), true))
                        }
                    }
                }
            } else {
                self.save_connection_draft(&draft, editor);
            }
        } else if test {
            self.test_connection_draft(&draft, editor);
        }
        self.connection_dialog = Some(draft);
    }

    /// Open a file dialog and put the chosen path into the draft of this
    /// editor instance.
    fn pick_path(&self, target: PathTarget, engine: Engine) {
        let editor = self.connection_editor.id;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        // The file name endings of the engine in the draft.
        let (label, extensions) = (engine.label(), engine.spec().form.file_extensions);
        self.track(async move {
            let dialog = rfd::AsyncFileDialog::new();
            let dialog = match target {
                PathTarget::DatabaseFile if !extensions.is_empty() => {
                    dialog.add_filter(label, extensions)
                }
                PathTarget::DatabaseFile => dialog,
                PathTarget::CaCertificate => {
                    dialog.add_filter("PEM / CRT", &["pem", "crt", "cer", "ca-bundle"])
                }
            };
            if let Some(file) = dialog.pick_file().await {
                let _ = tx
                    .send(Message::PathPicked(editor, target, file.path().to_owned()))
                    .await;
                ctx.request_repaint();
            }
        });
    }

    fn connection_general_fields(&mut self, ui: &mut egui::Ui, draft: &mut ConnectionProfile) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        widgets::form_row(
            ui,
            &p,
            LABELS,
            l("Nazwa połączenia", "Connection name"),
            |ui| {
                widgets::form_field(
                    ui,
                    widgets::field(&p, &mut draft.name)
                        .char_limit(120)
                        .desired_width(INPUT),
                );
            },
        );
        let spec = draft.engine.spec();
        let product = draft.product_spec();
        let mut picked = None;
        widgets::form_row(ui, &p, LABELS, l("Rodzaj bazy", "Database type"), |ui| {
            picked = widgets::product_picker(ui, &p, en, INPUT, (draft.engine, product));
        });
        if let Some((engine, product)) = picked {
            self.connection_editor.set_product(draft, engine, product);
        }
        let spec = if picked.is_some() {
            draft.engine.spec()
        } else {
            spec
        };
        let form = spec.form;
        if let Some(note) = draft.product_spec().note {
            widgets::form_row(ui, &p, LABELS, "", |ui| {
                ui.allocate_ui(egui::vec2(INPUT, 0.0), |ui| {
                    widgets::help(ui, &p, baza_core::engine::localized(note, en));
                });
            });
        }
        if form.address == baza_core::engine::Address::File {
            let mut pick = false;
            widgets::form_row(ui, &p, LABELS, l("Plik bazy", "Database file"), |ui| {
                let button = Btn::new(l("Wybierz…", "Choose…"));
                let width = INPUT - button.width(ui, &p) - 8.0;
                let mut path = draft
                    .sqlite_path
                    .as_ref()
                    .map(|v| v.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if ui
                    .add(widgets::field(&p, &mut path).desired_width(width))
                    .changed()
                {
                    draft.sqlite_path = if path.is_empty() {
                        None
                    } else {
                        Some(path.into())
                    };
                }
                pick = button.show(ui, &p).clicked();
            });
            if pick {
                self.pick_path(PathTarget::DatabaseFile, draft.engine);
            }
        } else {
            widgets::form_row(ui, &p, LABELS, "Host", |ui| {
                widgets::form_field(
                    ui,
                    widgets::field(&p, &mut draft.host)
                        .desired_width(INPUT)
                        .char_limit(253),
                );
            });
            widgets::form_row(ui, &p, LABELS, "Port", |ui| {
                port_field(ui, &p, &mut draft.port);
            });
        }
        use baza_core::engine::FieldUse;
        if form.username != FieldUse::Hidden {
            widgets::form_row(ui, &p, LABELS, l("Nazwa użytkownika", "User name"), |ui| {
                widgets::form_field(
                    ui,
                    widgets::field(&p, &mut draft.username).desired_width(INPUT),
                );
            });
        }
        if form.password != FieldUse::Hidden {
            widgets::form_row(ui, &p, LABELS, l("Hasło", "Password"), |ui| {
                widgets::form_field(
                    ui,
                    widgets::field(&p, &mut draft.password)
                        .password(true)
                        .desired_width(INPUT),
                );
            });
        }
        if form.database != FieldUse::Hidden {
            let label = baza_core::engine::localized(form.database_label, en);
            widgets::form_row(ui, &p, LABELS, label, |ui| {
                widgets::form_field(
                    ui,
                    widgets::field(&p, &mut draft.database)
                        .desired_width(INPUT)
                        .hint_text(if form.database == FieldUse::Optional {
                            l("opcjonalnie", "optional")
                        } else {
                            ""
                        }),
                );
            });
        }
        self.engine_option_fields(ui, draft);
        self.connection_group_selector(ui, draft.id);
        widgets::form_row(ui, &p, LABELS, l("Kolor", "Colour"), |ui| {
            ui.color_edit_button_srgb(&mut draft.color);
        });
        if draft.ssh.is_some() {
            widgets::help(
                ui,
                &p,
                l(
                    "Tunel SSH jest włączony: host bazy musi być osiągalny z serwera SSH.",
                    "The SSH tunnel is enabled: the database host must be reachable from the SSH server.",
                ),
            );
        }
    }

    /// The fields an engine declares for itself (`EngineSpec::options`):
    /// a text, a number, a switch or a choice, stored in the profile's
    /// options under the key of the field.
    fn engine_option_fields(&mut self, ui: &mut egui::Ui, draft: &mut ConnectionProfile) {
        use baza_core::OptionKind;
        use baza_core::engine::localized;
        let p = self.palette();
        let en = self.en();
        for option in draft.engine.spec().options {
            let label = localized(option.label, en);
            let current = draft.option(option.key).to_owned();
            let mut next = current.clone();
            widgets::form_row(ui, &p, LABELS, label, |ui| match option.kind {
                OptionKind::Text => {
                    widgets::form_field(
                        ui,
                        widgets::field(&p, &mut next)
                            .desired_width(INPUT)
                            .char_limit(baza_core::engine::MAX_OPTION_VALUE)
                            .hint_text(option.placeholder),
                    );
                }
                OptionKind::Number { min, max } => {
                    // Typed as text: an empty field is the default.
                    let mut text = next.clone();
                    widgets::form_field(
                        ui,
                        widgets::field(&p, &mut text)
                            .desired_width(92.0)
                            .char_limit(20)
                            .hint_text(option.default),
                    );
                    let digits = text.trim();
                    if digits.is_empty() {
                        next = option.default.to_owned();
                    } else if let Ok(number) = digits.parse::<i64>() {
                        next = number.clamp(min, max).to_string();
                    }
                }
                OptionKind::Flag => {
                    let mut on = next == "true";
                    if widgets::checkbox(ui, &p, &mut on, "").changed() {
                        next = on.to_string();
                    }
                }
                OptionKind::Choice(values) => {
                    widgets::combo(
                        ui,
                        &p,
                        ("engine_option", option.key),
                        next.clone(),
                        INPUT,
                        |ui| {
                            for value in values {
                                ui.selectable_value(&mut next, (*value).to_owned(), *value);
                            }
                        },
                    );
                }
            });
            if next != current {
                draft.set_option(option.key, &next);
            }
            if let Some(help) = option.help {
                widgets::form_row(ui, &p, LABELS, "", |ui| {
                    ui.allocate_ui(egui::vec2(INPUT, 0.0), |ui| {
                        widgets::help(ui, &p, localized(help, en));
                    });
                });
            }
        }
    }

    fn connection_tls_fields(&mut self, ui: &mut egui::Ui, draft: &mut ConnectionProfile) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        if !draft.engine.spec().form.tls {
            let label = draft.product_label();
            let text = match (draft.is_file(), en) {
                (true, false) => format!("Lokalny plik {label} nie korzysta z SSL/TLS."),
                (true, true) => format!("A local {label} file does not use SSL/TLS."),
                (false, false) => format!("{label} nie korzysta z ustawień SSL/TLS."),
                (false, true) => format!("{label} does not use the SSL/TLS settings."),
            };
            widgets::help(ui, &p, &text);
            return;
        }
        let name = |mode: TlsMode| match mode {
            TlsMode::Disable => l("Wyłączony", "Disabled"),
            TlsMode::Require => l("Szyfrowanie wymagane", "Encryption required"),
            TlsMode::VerifyFull => l(
                "Sprawdzaj certyfikat i nazwę hosta",
                "Verify certificate and hostname",
            ),
        };
        widgets::form_row(ui, &p, LABELS, l("Tryb SSL/TLS", "SSL/TLS mode"), |ui| {
            widgets::combo(ui, &p, "connection_tls", name(draft.tls), INPUT, |ui| {
                for mode in [TlsMode::VerifyFull, TlsMode::Require, TlsMode::Disable] {
                    ui.selectable_value(&mut draft.tls, mode, name(mode));
                }
            });
        });
        let mut pick = false;
        widgets::form_row(ui, &p, LABELS, l("Certyfikat CA", "CA certificate"), |ui| {
            let button = Btn::new(l("Wybierz…", "Choose…"));
            let width = INPUT - button.width(ui, &p) - 8.0;
            let mut path = draft
                .ca_file
                .as_ref()
                .map(|v| v.to_string_lossy().into_owned())
                .unwrap_or_default();
            if ui
                .add(
                    widgets::field(&p, &mut path)
                        .desired_width(width)
                        .hint_text(l(
                            "puste: certyfikaty systemu",
                            "empty: system certificates",
                        )),
                )
                .changed()
            {
                draft.ca_file = if path.is_empty() {
                    None
                } else {
                    Some(path.into())
                };
            }
            pick = button.show(ui, &p).clicked();
        });
        if pick {
            self.pick_path(PathTarget::CaCertificate, draft.engine);
        }
        ui.add_space(4.0);
        // What the chosen mode does, because the right one depends on the
        // certificate the server has.
        widgets::help(
            ui,
            &p,
            match draft.tls {
                TlsMode::VerifyFull => l(
                    "Pełna weryfikacja sprawdza zaufany urząd certyfikacji i nazwę hosta z zakładki Ogólne. Działa również przez SSH: tunel nie zmienia nazwy używanej do sprawdzenia certyfikatu. Serwer z certyfikatem samopodpisanym wymaga wskazania jego certyfikatu CA.",
                    "Full verification checks the trusted certificate authority and the hostname from the General tab. It also works through SSH: the tunnel keeps the name used to check the certificate. A server with a self-signed certificate needs its CA certificate chosen here.",
                ),
                TlsMode::Require => l(
                    "Połączenie z bazą jest szyfrowane, ale certyfikat serwera nie jest sprawdzany. Wystarcza dla serwera z certyfikatem samopodpisanym, gdy ufasz sieci albo łączysz się przez tunel SSH.",
                    "The connection to the database is encrypted, but the certificate of the server is not checked. Enough for a server with a self-signed certificate when you trust the network or connect through an SSH tunnel.",
                ),
                TlsMode::Disable => l(
                    "Połączenie z bazą nie jest szyfrowane. Przy włączonym tunelu SSH dane są chronione na odcinku do serwera SSH.",
                    "The connection to the database is not encrypted. With the SSH tunnel enabled the data is protected on the way to the SSH server.",
                ),
            },
        );
    }

    fn connection_group_selector(&mut self, ui: &mut egui::Ui, id: Uuid) {
        if self.connection_import_id.is_some() {
            return;
        }
        let p = self.palette();
        let label = self.tr("Grupa", "Group");
        if self.group_loading || self.group_store.is_none() {
            let mut reload = false;
            widgets::form_row(ui, &p, LABELS, label, |ui| {
                ui.label(
                    RichText::new(self.tr("Ładowanie grup…", "Loading groups…"))
                        .size(p.body() - 0.5)
                        .color(p.muted),
                );
                reload = Btn::new(self.tr("Odśwież grupy", "Refresh groups"))
                    .small()
                    .enabled(!self.group_loading)
                    .show(ui, &p)
                    .clicked();
            });
            if reload {
                self.load_groups();
            }
            return;
        }
        if self
            .connection_form_group
            .as_ref()
            .is_none_or(|v| v.0 != id)
        {
            self.connection_form_group =
                Some((id, self.group_snapshot.memberships.get(&id).copied()));
        }
        let mut group = self.connection_form_group.as_ref().and_then(|v| v.1);
        let ungrouped = self.tr("Bez grupy", "Ungrouped");
        let selected = group
            .map(|id| self.group_path(id))
            .unwrap_or_else(|| ungrouped.into());
        widgets::form_row(ui, &p, LABELS, label, |ui| {
            widgets::combo(ui, &p, "connection_group", selected, INPUT, |ui| {
                ui.selectable_value(&mut group, None, ungrouped);
                for g in &self.group_snapshot.groups {
                    ui.selectable_value(&mut group, Some(g.id), self.group_path(g.id));
                }
            });
        });
        self.connection_form_group = Some((id, group));
    }

    fn ssh_fields(&mut self, ui: &mut egui::Ui, draft: &mut ConnectionProfile) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        if !draft.engine.spec().form.ssh {
            let label = draft.product_label();
            let text = match (draft.is_file(), en) {
                (true, false) => format!(
                    "{label} to plik na tym komputerze. Tunel SSH jest dostępny dla baz działających jako serwer."
                ),
                (true, true) => format!(
                    "{label} is a file on this computer. SSH tunnels are available for databases that run as a server."
                ),
                (false, false) => format!("{label} nie łączy się przez tunel SSH."),
                (false, true) => format!("{label} is not reached through an SSH tunnel."),
            };
            widgets::help(ui, &p, &text);
            return;
        }
        let mut enabled = draft.ssh.is_some();
        if widgets::checkbox(ui, &p, &mut enabled, l("Użyj tunelu SSH", "Use SSH tunnel")).changed()
        {
            self.connection_editor.set_ssh_enabled(draft, enabled);
        }
        if draft.ssh.is_none() {
            widgets::help(
                ui,
                &p,
                l(
                    "Tunel łączy się z serwerem SSH i dopiero z niego z bazą. Otwiera się sam przy każdym połączeniu z tą bazą. Przydaje się, gdy baza nie jest dostępna z internetu.",
                    "A tunnel connects to an SSH server and reaches the database from there. It opens by itself whenever this connection connects. Use it when the database is not reachable from the internet.",
                ),
            );
            return;
        }
        let id = draft.id;
        let verify_full = draft.tls == TlsMode::VerifyFull;
        let Some(ssh) = &mut draft.ssh else {
            return;
        };
        widgets::form_row(ui, &p, LABELS, "Host", |ui| {
            widgets::form_field(ui, widgets::field(&p, &mut ssh.host).desired_width(INPUT));
        });
        widgets::form_row(ui, &p, LABELS, "Port", |ui| {
            port_field(ui, &p, &mut ssh.port);
        });
        widgets::form_row(ui, &p, LABELS, l("Nazwa użytkownika", "User name"), |ui| {
            widgets::form_field(
                ui,
                widgets::field(&p, &mut ssh.username).desired_width(INPUT),
            );
        });
        let previous_auth = ssh.auth;
        let method = |auth: SshAuth| match auth {
            SshAuth::Password => l("Hasło", "Password"),
            SshAuth::PrivateKey => l("Klucz prywatny", "Private key"),
        };
        widgets::form_row(
            ui,
            &p,
            LABELS,
            l("Metoda uwierzytelniania", "Authentication method"),
            |ui| {
                widgets::combo(ui, &p, "ssh_auth", method(ssh.auth), INPUT, |ui| {
                    for auth in [SshAuth::Password, SshAuth::PrivateKey] {
                        ui.selectable_value(&mut ssh.auth, auth, method(auth));
                    }
                });
            },
        );
        if previous_auth != ssh.auth {
            self.connection_editor.invalidate_key_load();
        }
        if ssh.auth == SshAuth::Password {
            widgets::form_row(ui, &p, LABELS, l("Hasło", "Password"), |ui| {
                widgets::form_field(
                    ui,
                    widgets::field(&p, ssh.password.get_or_insert_with(String::new))
                        .password(true)
                        .desired_width(INPUT),
                );
            });
        } else {
            let mut load = false;
            widgets::form_row(ui, &p, LABELS, l("Klucz prywatny", "Private key"), |ui| {
                load = Btn::new(l("Wybierz plik…", "Choose file…"))
                    .show(ui, &p)
                    .clicked();
                let loaded = ssh.private_key.as_ref().is_some_and(|s| !s.is_empty());
                if loaded {
                    widgets::icon(ui, Icon::Check, 13.0, p.positive);
                }
                ui.label(
                    RichText::new(if loaded {
                        l("Klucz wczytany", "Key loaded")
                    } else {
                        l("Nie wybrano klucza", "No key chosen")
                    })
                    .size(p.body() - 0.5)
                    .color(p.muted),
                );
            });
            if load {
                let tx = self.tx.clone();
                let ctx = self.ctx.clone();
                let editor = self.connection_editor.id;
                let request = self.connection_editor.begin_key_load();
                // QA ONLY: a self-test script cannot operate the system file
                // dialog, so `BAZA_QA_SSH_KEY_FILE` names the file instead.
                // Always `None` in a normal launch.
                let qa_key_file = self.qa_key_file();
                self.track(async move {
                    let chosen = match qa_key_file {
                        Some(path) => Some(path),
                        None => rfd::AsyncFileDialog::new()
                            .pick_file()
                            .await
                            .map(|file| file.path().to_owned()),
                    };
                    if let Some(path) = chosen {
                        let r = async {
                            let data = zeroize::Zeroizing::new(read_bounded(&path, 65536).await?);
                            let text = std::str::from_utf8(&data)
                                .map_err(|_| anyhow::anyhow!("ssh_private_key_utf8_required"))?;
                            anyhow::ensure!(
                                text.contains("PRIVATE KEY"),
                                "ssh_private_key_invalid"
                            );
                            Ok::<_, anyhow::Error>(zeroize::Zeroizing::new(text.to_owned()))
                        }
                        .await
                        .map_err(|e| e.to_string());
                        let _ = tx.send(Message::PrivateKey(id, editor, request, r)).await;
                        ctx.request_repaint();
                    }
                });
            }
            widgets::form_row(ui, &p, LABELS, l("Hasło klucza", "Key passphrase"), |ui| {
                widgets::form_field(
                    ui,
                    widgets::field(&p, ssh.passphrase.get_or_insert_with(String::new))
                        .password(true)
                        .desired_width(INPUT)
                        .hint_text(l(
                            "puste, jeśli klucz go nie ma",
                            "empty if the key has none",
                        )),
                );
            });
        }
        ui.add_space(2.0);
        widgets::help(
            ui,
            &p,
            l(
                "Host i port bazy na karcie Ogólne są widziane z serwera SSH (np. 127.0.0.1, gdy baza działa na tym samym serwerze).",
                "The database host and port on the General tab are resolved from the SSH server (for example 127.0.0.1 when the database runs on the same machine).",
            ),
        );
        widgets::help(
            ui,
            &p,
            l(
                "Klucz serwera jest zapamiętywany przy pierwszym połączeniu i sprawdzany przy każdym następnym, zanim zostanie wysłane hasło lub klucz.",
                "The server key is remembered on the first connection and checked on every later one, before a password or key is sent.",
            ),
        );
        // The key shown belongs to the saved server. When the draft points at
        // another one, saving drops it (`connection_edit::server_key_to_save`).
        let saved = self
            .profiles
            .connections
            .iter()
            .find(|profile| profile.id == id)
            .and_then(|profile| profile.ssh.as_ref());
        let key_of_another_server = !ssh.fingerprint.is_empty()
            && connection_edit::server_key_to_save(saved, self.connection_editor.opened_key(), ssh)
                .is_empty();
        egui::CollapsingHeader::new(
            RichText::new(l("Zaawansowane: klucz serwera", "Advanced: server key"))
                .size(p.body() - 0.5)
                .color(p.muted),
        )
        .id_salt("ssh_host_key")
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            widgets::help(
                ui,
                &p,
                l(
                    "Nic tu nie trzeba wpisywać. Zostaw pole puste albo wpisz otrzymany odcisk, aby był sprawdzany od pierwszego połączenia.",
                    "Nothing is needed here. Leave the field empty, or enter the fingerprint you were given to have it checked from the first connection.",
                ),
            );
            // The whole width: a fingerprint is 50 characters long.
            ui.add(
                widgets::field(&p, &mut ssh.fingerprint)
                    .hint_text("SHA256:…")
                    .desired_width(f32::INFINITY),
            );
            if key_of_another_server {
                widgets::help(
                    ui,
                    &p,
                    l(
                        "Zmieniono host lub port SSH: ten odcisk należał do poprzedniego serwera i nie zostanie zapisany. Klucz nowego serwera zostanie zapamiętany przy pierwszym połączeniu.",
                        "The SSH host or port was changed: this fingerprint belonged to the previous server and will not be saved. The key of the new server is remembered on the first connection.",
                    ),
                );
            }
            let scanning = self.connection_editor.scanning;
            let mut fetch = false;
            ui.horizontal(|ui| {
                fetch = Btn::new(l("Pobierz odcisk z serwera", "Fetch from server"))
                    .small()
                    .enabled(!scanning && ssh::validate_host(&ssh.host).is_ok())
                    .show(ui, &p)
                    .clicked();
                if scanning {
                    ui.spinner();
                }
            });
            if fetch {
                self.connection_editor.scanning = true;
                self.connection_editor.scan_note = None;
                let (host, port) = (ssh.host.clone(), ssh.port);
                let editor = self.connection_editor.id;
                let tx = self.tx.clone();
                let ctx = self.ctx.clone();
                self.track(async move {
                    let r = ssh::scan_host_key(&host, port)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(Message::HostKey(editor, r)).await;
                    ctx.request_repaint();
                });
            }
            if let Some((text, warning)) = &self.connection_editor.scan_note {
                if *warning {
                    widgets::feedback(ui, &p, text, true);
                } else {
                    widgets::help(ui, &p, text);
                }
            }
        });
        if verify_full {
            widgets::help(
                ui,
                &p,
                l(
                    "Certyfikat SSL/TLS jest sprawdzany dla pierwotnego hosta bazy również wewnątrz tunelu.",
                    "The SSL/TLS certificate is checked against the original database host inside the tunnel as well.",
                ),
            );
        }
    }

    /// A saved connection met a different SSH server key. Nothing is replaced
    /// until the user decides.
    pub(super) fn host_key_change_window(&mut self, ctx: &egui::Context) {
        let Some((id, key)) = self.host_key_change.clone() else {
            return;
        };
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let name = self
            .connection_name(id)
            .map(str::to_owned)
            .unwrap_or_default();
        let (mut trust, mut cancel) = (false, false);
        let dialog = widgets::dialog(
            ctx,
            &p,
            "host_key_change",
            l(
                "Klucz serwera SSH się zmienił",
                "The SSH server key changed",
            ),
            560.0,
            l("Zamknij", "Close"),
            |ui| {
                widgets::dialog_body(ui, |ui| {
                    widgets::subheading(ui, &p, &name);
                    widgets::feedback(ui, &p, ssh::describe("ssh_host_key_changed", en), true);
                    ui.add_space(4.0);
                    widgets::label(ui, &p, l("Nowy klucz serwera", "New server key"));
                    ui.label(
                        RichText::new(format!("{} {}", key.algorithm, key.fingerprint))
                            .monospace()
                            .size(p.code() - 1.0),
                    );
                });
                widgets::dialog_footer(ui, &p, |ui| {
                    widgets::right(ui, |ui| {
                        trust = Btn::danger(l(
                            "Zaufaj nowemu kluczowi i połącz",
                            "Trust the new key and connect",
                        ))
                        .show(ui, &p)
                        .clicked();
                        cancel = Btn::new(l("Anuluj", "Cancel")).show(ui, &p).clicked();
                    });
                });
            },
        );
        if trust {
            self.host_key_change = None;
            let saved = self
                .profiles
                .connections
                .iter_mut()
                .find(|p| p.id == id)
                .and_then(|p| p.ssh.as_mut());
            if let Some(ssh) = saved {
                ssh.fingerprint = key.fingerprint;
                self.save();
                self.connect(id);
            }
        } else if cancel || dialog.close {
            self.host_key_change = None;
        }
    }
}
