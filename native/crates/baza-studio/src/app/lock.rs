//! The optional local application lock: the unlock screen and its settings.
use super::*;
use crate::widgets::Btn;
use eframe::egui::RichText;

impl StudioApp {
    /// The window before the workspace: the vault is being opened, or it
    /// could not be opened and `error` holds the code of the failure.
    pub(super) fn startup_screen(&mut self, ui: &mut egui::Ui, error: Option<&str>) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let mut retry = false;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(p.bg))
            .show(ui, |ui| {
                let width = 460.0_f32.min(ui.available_width() - 40.0);
                ui.vertical_centered(|ui| {
                    ui.add_space((ui.available_height() * 0.26).max(24.0));
                    widgets::brand_mark(ui, 34.0);
                    ui.add_space(10.0);
                    widgets::heading(ui, &p, "Baza Studio");
                    ui.add_space(18.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(width, 0.0),
                        egui::Layout::top_down(egui::Align::Center),
                        |ui| {
                            ui.set_width(width);
                            ui.spacing_mut().item_spacing.y = 10.0;
                            match error {
                                None => {
                                    ui.add(egui::Spinner::new().size(16.0).color(p.muted));
                                    ui.label(
                                        RichText::new(l(
                                            "Otwieranie sejfu…",
                                            "Opening the secure vault…",
                                        ))
                                        .size(p.body())
                                        .color(p.text),
                                    );
                                    if cfg!(target_os = "macos") {
                                        ui.label(
                                            RichText::new(l(
                                                "Jeśli macOS zapyta o pęk kluczy, wybierz „Zawsze pozwalaj”.",
                                                "If macOS asks about the keychain, choose Always Allow.",
                                            ))
                                            .size(p.small() + 0.5)
                                            .color(p.muted),
                                        );
                                    }
                                }
                                Some(code) => {
                                    ui.label(
                                        RichText::new(l(
                                            "Nie udało się otworzyć sejfu",
                                            "The vault could not be opened",
                                        ))
                                        .font(theme::medium(p.font + 1.5))
                                        .color(p.text),
                                    );
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(crate::messages::describe(code, en))
                                                .size(p.body() - 0.5)
                                                .color(p.muted),
                                        )
                                        .wrap(),
                                    );
                                    ui.add_space(6.0);
                                    retry = Btn::primary(l("Spróbuj ponownie", "Try again"))
                                        .icon(Icon::Refresh)
                                        .show(ui, &p)
                                        .clicked();
                                }
                            }
                        },
                    );
                });
            });
        if retry {
            self.retry_startup();
        }
    }

    pub(super) fn unlock_screen(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let notice = self
            .notice
            .as_ref()
            .map(|(text, error)| (crate::messages::describe(text, en), *error));
        let mut unlock = false;
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(p.bg))
            .show(ui, |ui| {
                let width = 340.0_f32.min(ui.available_width() - 40.0);
                ui.vertical_centered(|ui| {
                    ui.add_space((ui.available_height() * 0.2).max(24.0));
                    widgets::brand_mark(ui, 34.0);
                    ui.add_space(10.0);
                    widgets::heading(ui, &p, "Baza Studio");
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(l(
                            "Aplikacja jest zablokowana na tym komputerze.",
                            "The application is locked on this computer.",
                        ))
                        .size(p.body())
                        .color(p.muted),
                    );
                    ui.add_space(22.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(width, 0.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(width);
                            ui.spacing_mut().item_spacing.y = 7.0;
                            widgets::label(ui, &p, l("Hasło lokalne", "Local password"));
                            let password = ui.add(
                                widgets::field(&p, &mut self.unlock_password)
                                    .password(true)
                                    .desired_width(width),
                            );
                            if ui.memory(|memory| memory.focused().is_none()) {
                                password.request_focus();
                            }
                            if self.lock_settings.two_factor {
                                ui.add_space(6.0);
                                widgets::label(
                                    ui,
                                    &p,
                                    l(
                                        "Kod z aplikacji uwierzytelniającej lub kod odzyskiwania",
                                        "Authenticator code or recovery code",
                                    ),
                                );
                                ui.add(
                                    widgets::field(&p, &mut self.unlock_code)
                                        .password(true)
                                        .desired_width(width),
                                );
                            }
                            ui.add_space(10.0);
                            let enter = ui.input_mut(|i| {
                                i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                            });
                            ui.horizontal(|ui| {
                                unlock = Btn::primary(l("Odblokuj", "Unlock"))
                                    .icon(Icon::Lock)
                                    .enabled(!self.lock_busy)
                                    .min_width(width)
                                    .show(ui, &p)
                                    .clicked()
                                    || enter;
                            });
                            if self.lock_busy {
                                ui.spinner();
                            }
                            if let Some((text, error)) = &notice {
                                ui.add_space(6.0);
                                widgets::feedback(ui, &p, text, *error);
                            }
                            ui.add_space(14.0);
                            widgets::help(
                                ui,
                                &p,
                                l(
                                    "Blokada działa lokalnie, bez konta i bez połączenia z portalem.",
                                    "The lock works locally, without an account or a connection to the portal.",
                                ),
                            );
                        },
                    );
                });
            });
        if unlock && !self.lock_busy {
            self.unlock();
        }
    }

    pub(super) fn lock_settings_window(&mut self, ctx: &egui::Context) {
        if !self.show_lock_settings {
            return;
        }
        let Some(lock) = self.local_lock.clone() else {
            return;
        };
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let mut setup = false;
        let mut confirm = false;
        let mut disable = false;
        let mut close = false;
        let mut idle = None;
        let dialog = widgets::dialog(
            ctx,
            &p,
            "local_lock_settings",
            l("Blokada aplikacji", "Application lock"),
            520.0,
            l("Zamknij", "Close"),
            |ui| {
                // Each state of the setup has its own body and its own main action.
                if !self.recovery_codes.is_empty() {
                    let mut copy = false;
                    let mut saved = false;
                    widgets::dialog_body(ui, |ui| {
                        widgets::feedback(
                            ui,
                            &p,
                            l(
                                "Zapisz kody odzyskiwania. To jedyny moment, w którym są widoczne.",
                                "Save your recovery codes. This is the only time they are shown.",
                            ),
                            true,
                        );
                        for code in &self.recovery_codes {
                            ui.label(RichText::new(code).monospace().size(p.code()));
                        }
                    });
                    widgets::dialog_footer(ui, &p, |ui| {
                        copy = Btn::new(l("Skopiuj kody", "Copy codes"))
                            .show(ui, &p)
                            .clicked();
                        widgets::right(ui, |ui| {
                            saved = Btn::primary(l("Kody są zapisane", "The codes are saved"))
                                .show(ui, &p)
                                .clicked();
                        });
                    });
                    if copy {
                        ui.ctx().copy_text(self.recovery_codes.join("\n"));
                    }
                    if saved {
                        for code in &mut self.recovery_codes {
                            code.zeroize();
                        }
                        self.recovery_codes.clear();
                    }
                } else if let Some(enrollment) = &self.enrollment {
                    widgets::dialog_body(ui, |ui| {
                        if let Some(secret) = &enrollment.secret {
                            widgets::help(
                                ui,
                                &p,
                                l(
                                    "Zeskanuj kod aplikacją uwierzytelniającą albo wpisz sekret ręcznie.",
                                    "Scan the code with your authenticator app, or enter the secret by hand.",
                                ),
                            );
                            if let Some(texture) = &self.qr_texture {
                                ui.image((texture.id(), egui::vec2(200.0, 200.0)));
                            }
                            ui.label(RichText::new(secret).monospace().size(p.code() - 1.0));
                            widgets::label(ui, &p, "TOTP · SHA1 · 6 · 30 s");
                            ui.add_space(4.0);
                            widgets::label(ui, &p, l("Kod z aplikacji", "Code from the app"));
                            ui.add(
                                widgets::field(&p, &mut self.unlock_code)
                                    .password(true)
                                    .desired_width(200.0),
                            );
                        } else {
                            widgets::help(
                                ui,
                                &p,
                                l(
                                    "Hasło jest gotowe. Potwierdź, aby włączyć blokadę.",
                                    "The password is ready. Confirm to enable the lock.",
                                ),
                            );
                        }
                    });
                    widgets::dialog_footer(ui, &p, |ui| {
                        if self.lock_busy {
                            ui.spinner();
                        }
                        widgets::right(ui, |ui| {
                            confirm = Btn::primary(l("Włącz blokadę", "Enable the lock"))
                                .enabled(!self.lock_busy)
                                .show(ui, &p)
                                .clicked();
                            close = Btn::new(l("Anuluj", "Cancel")).show(ui, &p).clicked();
                        });
                    });
                } else if !self.lock_settings.enabled {
                    widgets::dialog_body(ui, |ui| {
                        widgets::help(
                            ui,
                            &p,
                            l(
                                "Blokada jest wyłączona. Hasło i opcjonalny kod TOTP odblokowują wyłącznie ten program na tym komputerze; nie są wymagane do korzystania z aplikacji.",
                                "The lock is disabled. A password and an optional TOTP code unlock only this application on this computer; they are not required to use it.",
                            ),
                        );
                        ui.add_space(4.0);
                        widgets::label(
                            ui,
                            &p,
                            l(
                                "Nowe hasło lokalne (co najmniej 12 znaków)",
                                "New local password (at least 12 characters)",
                            ),
                        );
                        ui.add(
                            widgets::field(&p, &mut self.enroll_password)
                                .password(true)
                                .desired_width(320.0),
                        );
                        widgets::label(ui, &p, l("Powtórz hasło", "Repeat the password"));
                        ui.add(
                            widgets::field(&p, &mut self.enroll_confirm)
                                .password(true)
                                .desired_width(320.0),
                        );
                        widgets::checkbox(
                            ui,
                            &p,
                            &mut self.enroll_totp,
                            l(
                                "Dodaj też kod TOTP z aplikacji uwierzytelniającej",
                                "Also add a TOTP code from an authenticator app",
                            ),
                        );
                    });
                    widgets::dialog_footer(ui, &p, |ui| {
                        if self.lock_busy {
                            ui.spinner();
                        }
                        widgets::right(ui, |ui| {
                            setup = Btn::primary(l("Dalej", "Continue"))
                                .enabled(
                                    !self.lock_busy
                                        && self.enroll_password == self.enroll_confirm
                                        && self.enroll_password.chars().count() >= 12,
                                )
                                .min_width(84.0)
                                .show(ui, &p)
                                .clicked();
                            close = Btn::new(l("Anuluj", "Cancel"))
                                .min_width(84.0)
                                .show(ui, &p)
                                .clicked();
                        });
                    });
                } else {
                    widgets::dialog_body(ui, |ui| {
                        widgets::help(
                            ui,
                            &p,
                            l(
                                "Blokada jest włączona. Każdą zmianę potwierdź hasłem lokalnym.",
                                "The lock is enabled. Confirm every change with your local password.",
                            ),
                        );
                        ui.add_space(4.0);
                        widgets::label(ui, &p, l("Hasło lokalne", "Local password"));
                        ui.add(
                            widgets::field(&p, &mut self.unlock_password)
                                .password(true)
                                .desired_width(320.0),
                        );
                        if self.lock_settings.two_factor {
                            widgets::label(ui, &p, l("Kod z aplikacji", "Code from the app"));
                            ui.add(
                                widgets::field(&p, &mut self.unlock_code)
                                    .password(true)
                                    .desired_width(200.0),
                            );
                        }
                        ui.add_space(4.0);
                        widgets::label(
                            ui,
                            &p,
                            l("Zablokuj po bezczynności", "Lock after inactivity"),
                        );
                        ui.horizontal(|ui| {
                            for minutes in [5, 15, 30] {
                                let current = self.lock_settings.idle_minutes == minutes;
                                let label = format!("{minutes} min");
                                let button = if current {
                                    Btn::primary(&label)
                                } else {
                                    Btn::new(&label)
                                };
                                if button
                                    .small()
                                    .enabled(!self.lock_busy)
                                    .show(ui, &p)
                                    .clicked()
                                    && !current
                                {
                                    idle = Some(minutes);
                                }
                            }
                        });
                    });
                    widgets::dialog_footer(ui, &p, |ui| {
                        disable = Btn::danger(l("Wyłącz blokadę", "Disable the lock"))
                            .enabled(!self.lock_busy)
                            .show(ui, &p)
                            .clicked();
                        if self.lock_busy {
                            ui.spinner();
                        }
                        widgets::right(ui, |ui| {
                            close = Btn::new(l("Zamknij", "Close"))
                                .min_width(84.0)
                                .show(ui, &p)
                                .clicked();
                        });
                    });
                }
            },
        );
        self.show_lock_settings = !(close || dialog.close);
        if setup {
            self.lock_busy = true;
            let password = zeroize::Zeroizing::new(self.enroll_password.clone());
            let two_factor = self.enroll_totp;
            let tx = self.tx.clone();
            let ctx = self.ctx.clone();
            self.track(async move {
                let r = tokio::task::spawn_blocking(move || lock.setup(&password, two_factor))
                    .await
                    .map_err(|_| "local_lock_task_failed".to_owned())
                    .and_then(|r| r.map_err(|e| e.to_string()));
                let _ = tx.send(Message::Enrollment(r)).await;
                ctx.request_repaint();
            });
        } else if confirm {
            self.lock_busy = true;
            let token = self.enrollment.as_ref().unwrap().token.clone();
            let code = zeroize::Zeroizing::new(self.unlock_code.clone());
            let tx = self.tx.clone();
            let ctx = self.ctx.clone();
            self.track(async move {
                let r = tokio::task::spawn_blocking(move || {
                    let codes = lock
                        .confirm_setup(&token, if code.is_empty() { None } else { Some(&code) })?;
                    Ok::<_, anyhow::Error>((lock.status()?, codes))
                })
                .await
                .map_err(|_| "local_lock_task_failed".to_owned())
                .and_then(|r| r.map_err(|e| e.to_string()));
                let _ = tx.send(Message::LockConfirmed(r)).await;
                ctx.request_repaint();
            });
        } else if disable || idle.is_some() {
            self.lock_busy = true;
            let password = zeroize::Zeroizing::new(self.unlock_password.clone());
            let code = zeroize::Zeroizing::new(self.unlock_code.clone());
            let tx = self.tx.clone();
            let ctx = self.ctx.clone();
            self.track(async move {
                let r = tokio::task::spawn_blocking(move || {
                    if disable {
                        lock.disable(&password, if code.is_empty() { None } else { Some(&code) })?;
                    } else {
                        lock.verify(&password, if code.is_empty() { None } else { Some(&code) })?;
                        lock.change_idle(idle.unwrap())?;
                    }
                    Ok::<_, anyhow::Error>((lock.status()?, vec![]))
                })
                .await
                .map_err(|_| "local_lock_task_failed".to_owned())
                .and_then(|r| r.map_err(|e| e.to_string()));
                let _ = tx.send(Message::LockConfirmed(r)).await;
                ctx.request_repaint();
            });
        }
    }
}
