//! QA ONLY. Nothing here runs in a normal launch: every switch is honoured
//! solely together with `--native-ui-self-test` or `--qa-interactive`, which
//! work on an owned temporary vault and a sample SQLite file.
//!
//! The window of `--native-ui-self-test` opens above the other windows
//! without taking the keyboard from the application the person at the
//! computer is using. The system does not paint a window that is covered
//! completely or that sits behind a full-screen application, and a self-test
//! needs its frames. So windows of self-tests that run at the same time are
//! placed one step further down and to the right each (`QaWindowSlot` in
//! `main.rs`), and a run that is still not painted when its time is up is
//! ended from outside the frame loop with the failure
//! `native_window_not_painted` instead of waiting forever.
//! `--qa-foreground` makes the window take the keyboard and come forward like
//! a normal launch; use it when a full-screen application is in the way.
//!
//! The switches choose what is on screen when the self-test takes its
//! screenshot, so every section, dialog, theme and language can be looked at:
//!
//! * `--qa-view <name>`: a section (`sql`, `table`, `builder`, `history`,
//!   `transfer`, `connections`, `monitor`), a dialog (`connection-general`,
//!   `connection-ssh`, `connection-ssh-password`, `connection-ssl`,
//!   `preferences`, `preferences-editor`, `updates`, `about`, `lock`, `designer`,
//!   `insert`, `cell`, `group`, `delete`, `save-query`, `suggestions`,
//!   `host-key`) or a state
//!   (`empty`, `table-empty`, `history-empty`, `builder-empty`, `import`,
//!   `profiles-export`, `error`, `focus`, `locked`, `startup`,
//!   `startup-error`, `search`, `groups`, `table-filter`, `table-sort`,
//!   `table-page`, `table-edit`, `table-disconnected`).
//! * `--qa-theme <id>`: `vscode`, `graphite`, `midnight`, `oled`, `amethyst`.
//! * `--qa-lang <pl|en>`.
//! * `--qa-font-size <11..22>` and `--qa-density <compact|comfortable>`.
//! * `--qa-size <width>x<height>`: inner size of the window in points (see
//!   `main.rs`).
//! * `--qa-scale <1|2>`: pixel density of the rendering, whatever the display
//!   has. The window keeps the size of `--qa-size` in points, so
//!   `--qa-size 1920x1080 --qa-scale 2` gives a 3840x2160 screenshot of the
//!   same layout. The window has no title bar in this mode, because a titled
//!   window is shrunk to the screen and a 2x capture on a 1x display is larger.
//!   Without this switch the size and the density follow the display, so a
//!   script with fixed points should always be run with both switches.
//!   Both are kept for the whole run: when the display changes under the
//!   window (another monitor, a display going to sleep), the script pauses
//!   until they are restored, and a frame captured meanwhile is taken again.
//!   The report counts those events as `qa_density_restored`; `qa_size_refused`
//!   is true when the system would not give the window its size.
//! * `--qa-script "<step>;<step>…"`: pointer and key input played after the
//!   view is on screen and before the screenshot, to exercise real clicks.
//!   A step starts 450 ms after the previous one ended. Points are counted
//!   from the top-left corner, in points (not pixels) at any `--qa-scale`.
//!   While a script plays, the mouse and the keys of the person at the
//!   computer are ignored by this window, so that a run always plays the same.
//!   - `c:x,y` click, `d:x,y` double click, `r:x,y` right click;
//!   - `m:x,y` move the pointer without a click, for hover states;
//!   - `v:x,y,dy` turn the wheel over that point: a positive `dy` moves the
//!     view that many points further down a list or a form, a negative one up;
//!   - `t:text` typed text (everything after the colon, spaces included;
//!     `\x3b` stands for `;`, which otherwise ends the step);
//!   - `k:<key>` a key: `enter`, `esc`, `tab`, `f2`, `up`, `down`, `left`,
//!     `right`, `backspace`, `delete`, `space`, `home`, `end`, a letter or a
//!     digit, each optionally after `cmd+`, `ctrl+`, `shift+` or `alt+`
//!     (`k:cmd+a` selects the text of a field);
//!   - `w:<ms>` wait that long (at most 60000) before the next step;
//!   - `s:<name>` save a screenshot now as `<report stem>-<name>.png` (letters,
//!     digits, `-` and `_`) and go on, so one run yields a sequence. Each step
//!     waits for its own frame and writes exactly one file; the names are
//!     listed in the report as `qa_shots`, in the order of the script.
//!
//!   The clicks of a script do not depend on how long a frame takes: a double
//!   click is a double click also when a 4K frame takes longer than the time
//!   in which two clicks normally have to follow each other.
//! * `--qa-script-file <path>`: the same steps read from a file, separated by
//!   `;` or line ends; a line that starts with `#` is a comment. It is played
//!   after the steps of `--qa-script`.
//! * `BAZA_QA_SSH_KEY_FILE` (environment): path of a private key that the
//!   button "Wybierz plik…" of the SSH tab loads instead of opening the system
//!   file dialog, which a script cannot operate. Read in the QA modes only.
//! * `BAZA_QA_CONNECTIONS_FILE` (environment): path of a JSON file with an
//!   array of connection profiles in their stored form (the fields of
//!   `ConnectionProfile`, password included). They are added to the owned
//!   temporary vault next to the sample SQLite connection, so that a view can
//!   be looked at on a real server of any engine. The file belongs to the
//!   person who runs the test and stays outside the repository.
//! * `BAZA_QA_CONNECTION` (environment): name of the connection the views
//!   work on instead of the sample. It is connected at start and its first
//!   table is opened, or the one named by `BAZA_QA_TABLE`. The table needs
//!   at least one row: the screenshot waits for rows on screen.
//!   All three are read in the QA modes only.
use super::*;

/// QA only: the connection profiles of `BAZA_QA_CONNECTIONS_FILE`, for the
/// owned temporary vault of a self-test. Empty without the variable.
pub(super) fn extra_connections() -> anyhow::Result<Vec<ConnectionProfile>> {
    let Some(path) = std::env::var_os("BAZA_QA_CONNECTIONS_FILE").filter(|path| !path.is_empty())
    else {
        return Ok(Vec::new());
    };
    let data = std::fs::read(path).map_err(|_| anyhow::anyhow!("qa_connections_unreadable"))?;
    serde_json::from_slice(&data).map_err(|_| anyhow::anyhow!("qa_connections_invalid"))
}

/// QA-only choices read from the command line.
#[derive(Clone, Debug, Default)]
pub struct QaOptions {
    pub view: Option<String>,
    pub theme: Option<String>,
    pub language: Option<String>,
    pub font_size: Option<f32>,
    pub density: Option<String>,
    pub script: Vec<Step>,
    /// Pixel density asked for with `--qa-scale`.
    pub scale: Option<f32>,
    /// Inner size of the window in points, kept at any `--qa-scale`. Filled
    /// in by `main` together with the window it opens.
    pub size: Option<[f32; 2]>,
    /// `--qa-foreground`: the window takes the keyboard like a normal launch.
    pub foreground: bool,
}

/// One step of `--qa-script`.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Click(egui::Pos2),
    /// A click with Shift or Command held.
    ClickWith(egui::Pos2, egui::Modifiers),
    /// Press at the first point, move to the second and release there.
    Drag(egui::Pos2, egui::Pos2),
    /// What Command+C (Ctrl+C) gives the window: a copy request.
    Copy,
    /// What Command+V gives the window: the text on the clipboard. `\t` and
    /// `\n` in the script stand for a tab and a line break.
    Paste(String),
    DoubleClick(egui::Pos2),
    RightClick(egui::Pos2),
    /// Move the pointer without pressing a button.
    Move(egui::Pos2),
    /// Scroll the content under a point down by this many points.
    Scroll(egui::Pos2, f32),
    Text(String),
    Key(egui::Key, egui::Modifiers),
    /// Do nothing for this many milliseconds.
    Wait(u64),
    /// Save a screenshot under this name and go on.
    Shot(String),
}

/// Pause before every step, so that the previous one has taken effect.
const STEP_SETTLE: Duration = Duration::from_millis(450);

impl Step {
    fn parse(text: &str) -> Option<Self> {
        let (kind, value) = text.split_once(':')?;
        let point = || {
            let (x, y) = value.split_once(',')?;
            Some(egui::pos2(x.trim().parse().ok()?, y.trim().parse().ok()?))
        };
        Some(match kind.trim() {
            "c" => Self::Click(point()?),
            "cs" => Self::ClickWith(point()?, egui::Modifiers::SHIFT),
            "cc" => Self::ClickWith(point()?, egui::Modifiers::COMMAND),
            "g" => {
                let numbers = value
                    .split(',')
                    .map(|n| n.trim().parse::<f32>().ok())
                    .collect::<Option<Vec<_>>>()?;
                let [x1, y1, x2, y2] = numbers[..] else {
                    return None;
                };
                Self::Drag(egui::pos2(x1, y1), egui::pos2(x2, y2))
            }
            "k" if matches!(value.trim(), "cmd+c" | "ctrl+c") => Self::Copy,
            "d" => Self::DoubleClick(point()?),
            "r" => Self::RightClick(point()?),
            "m" => Self::Move(point()?),
            "v" => {
                let (at, distance) = value.rsplit_once(',')?;
                let (x, y) = at.split_once(',')?;
                Self::Scroll(
                    egui::pos2(x.trim().parse().ok()?, y.trim().parse().ok()?),
                    distance.trim().parse().ok()?,
                )
            }
            // `;` ends a step, so a text writes it as `\x3b`.
            "t" => Self::Text(value.replace("\\x3b", ";")),
            "p" => Self::Paste(
                value
                    .replace("\\t", "\t")
                    .replace("\\n", "\n")
                    .replace("\\x3b", ";"),
            ),
            "k" => {
                let mut modifiers = egui::Modifiers::NONE;
                let mut parts = value.trim().split('+').collect::<Vec<_>>();
                let name = parts.pop()?;
                for part in parts {
                    modifiers |= match part.trim() {
                        "cmd" => egui::Modifiers::COMMAND,
                        "ctrl" => egui::Modifiers::CTRL,
                        "shift" => egui::Modifiers::SHIFT,
                        "alt" => egui::Modifiers::ALT,
                        _ => return None,
                    };
                }
                let key = match name.trim() {
                    "enter" => egui::Key::Enter,
                    "esc" => egui::Key::Escape,
                    "tab" => egui::Key::Tab,
                    "f2" => egui::Key::F2,
                    "up" => egui::Key::ArrowUp,
                    "down" => egui::Key::ArrowDown,
                    "left" => egui::Key::ArrowLeft,
                    "right" => egui::Key::ArrowRight,
                    "backspace" => egui::Key::Backspace,
                    "delete" => egui::Key::Delete,
                    "space" => egui::Key::Space,
                    "home" => egui::Key::Home,
                    "end" => egui::Key::End,
                    "insert" => egui::Key::Insert,
                    "pageup" => egui::Key::PageUp,
                    "pagedown" => egui::Key::PageDown,
                    // A letter, a digit, a sign or a function key (f1 to f12).
                    other if other.len() == 1 => egui::Key::from_name(&other.to_uppercase())?,
                    other
                        if other.starts_with('f')
                            && other[1..]
                                .parse::<u8>()
                                .is_ok_and(|n| (1..=12).contains(&n)) =>
                    {
                        egui::Key::from_name(&other.to_uppercase())?
                    }
                    _ => return None,
                };
                Self::Key(key, modifiers)
            }
            "w" => Self::Wait(value.trim().parse().ok().filter(|ms| *ms <= 60_000)?),
            "s" => {
                let name = value.trim();
                let plain = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
                if name.is_empty() || name.len() > 64 || !name.chars().all(plain) {
                    return None;
                }
                Self::Shot(name.to_owned())
            }
            _ => return None,
        })
    }

    /// Longest time the step is given, for the limit of the whole run. It is
    /// generous: a window behind other windows is drawn less often.
    fn budget(&self) -> Duration {
        STEP_SETTLE
            + match self {
                Self::Wait(ms) => Duration::from_millis(*ms),
                // Encoding a large frame takes seconds in a debug build.
                Self::Shot(_) => Duration::from_secs(10),
                _ => Duration::from_millis(1500),
            }
    }
}

/// The QA vault and the folder of a self-test, for the watchdog: it has to
/// remove them when the run never reaches its own end.
static OWNED: Mutex<Option<(Arc<Vault>, PathBuf)>> = Mutex::new(None);
/// The self-test has ended by itself, with a report of success or failure.
static FINISHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Time a self-test has before its own check ends it with a failure report.
const RUN_LIMIT: Duration = Duration::from_secs(20);

/// QA only: note what a self-test has to remove when it ends.
pub(super) fn owned(vault: &Arc<Vault>, root: &std::path::Path) {
    if let Ok(mut owned) = OWNED.lock() {
        *owned = Some((vault.clone(), root.to_owned()));
    }
}

/// QA only: the self-test ended by itself; the watchdog has nothing to do.
pub fn finished() {
    FINISHED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// QA only: end a self-test that the system never painted.
///
/// Every check of a self-test, including the one that ends a stuck run, is
/// made while a frame is processed. A window that is covered completely, or
/// behind a full-screen application, gets no frame at all, so nothing would
/// ever end the run. This thread waits a little longer than the run is
/// allowed, then writes the failure report, removes the QA key and folder
/// and ends the process.
pub fn watchdog(report: PathBuf, options: &QaOptions) {
    let limit = RUN_LIMIT + options.script_budget() + Duration::from_secs(15);
    std::thread::spawn(move || {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if FINISHED.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        if FINISHED.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        give_up(&report);
        eprintln!("Error: native_window_not_painted");
        std::process::exit(1);
    });
}

/// What the watchdog leaves behind: a failure report, unless the run wrote a
/// report of its own, and neither the QA key nor the QA folder.
fn give_up(report: &std::path::Path) {
    if !report.exists() {
        let _ = std::fs::write(
            report,
            b"{\"renderer_screenshot\":false,\"failure\":\"native_window_not_painted\"}",
        );
    }
    if let Ok(mut owned) = OWNED.lock()
        && let Some((vault, root)) = owned.take()
    {
        let _ = vault.remove_owned_qa_key();
        if root.file_name().is_some_and(|name| {
            name.to_string_lossy()
                .starts_with("BazaStudio-native-render-QA-")
        }) {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

/// Keep the frames of one click or double click close together on the clock
/// of the frames, however long they really took: the press and the release
/// of a click, and the two clicks of a double click, are told by that clock.
/// The time never runs ahead of the real one, so it stays monotonic.
fn click_clock(progress: &mut ScriptProgress, time: &mut Option<f64>) {
    match progress.frame {
        0 => progress.pressed_at = None,
        1 => progress.pressed_at = *time,
        frame => {
            if let (Some(pressed), Some(now)) = (progress.pressed_at, time.as_mut()) {
                *now = now.min(pressed + CLICK_FRAME * f64::from(frame - 1));
            }
        }
    }
}

/// Steps of a script text: separated by `;` or line ends, `#` lines skipped.
fn parse_script(text: &str) -> Vec<Step> {
    text.lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .flat_map(|line| line.split(';'))
        .filter_map(Step::parse)
        .collect()
}

/// Mark of a screenshot asked for by a `s:<name>` step.
struct NamedShot(String);

/// Where the playback of the script is.
#[derive(Default)]
pub struct ScriptProgress {
    /// Index of the step being played.
    step: usize,
    /// Frame within the step: a click is a press in one frame and a release
    /// in the next.
    frame: u8,
    /// When the previous step ended.
    last: Option<Instant>,
    /// Screenshot the current `s:` step waits for, and whether the renderer
    /// was already asked for it.
    shot: Option<(String, bool)>,
    /// Names of the screenshots saved so far.
    shots: Vec<String>,
    /// The window has the pixel density and the size of `--qa-scale` in this
    /// frame. While it has not, no step is played and no screenshot is taken.
    settled: bool,
    /// Since when the window is being asked for its size without having it.
    resizing: Option<Instant>,
    /// The system would not give the window its size; the run goes on with
    /// the size it has, and the report states it.
    size_refused: bool,
    /// Time of the frame in which the pointer button of the current step
    /// went down, on the clock of the frames.
    pressed_at: Option<f64>,
    /// How often the pixel density had to be restored after the run began:
    /// each time the display changed under the window.
    density_restored: u32,
}

/// Longest time between the frames of one click or double click on the clock
/// that egui sees. A frame of a 4K capture can take longer than the time in
/// which two clicks count as a double click; the script's clicks must not
/// depend on that.
const CLICK_FRAME: f64 = 0.03;

impl QaOptions {
    /// Read the switches. They are ignored unless a QA mode is active.
    pub fn from_args(args: &[String], qa: bool) -> Self {
        let value = |flag: &str| {
            args.windows(2)
                .find(|pair| pair[0] == flag)
                .map(|pair| pair[1].clone())
                .filter(|_| qa)
        };
        Self {
            view: value("--qa-view"),
            theme: value("--qa-theme"),
            language: value("--qa-lang"),
            font_size: value("--qa-font-size")
                .and_then(|size| size.parse().ok())
                .filter(|size| (11.0..=22.0).contains(size)),
            density: value("--qa-density"),
            script: value("--qa-script")
                .map(|script| parse_script(&script))
                .into_iter()
                .chain(
                    value("--qa-script-file")
                        .and_then(|path| std::fs::read_to_string(path).ok())
                        .map(|script| parse_script(&script)),
                )
                .flatten()
                .collect(),
            scale: value("--qa-scale")
                .and_then(|scale| scale.parse().ok())
                .filter(|scale| *scale == 1.0 || *scale == 2.0),
            size: None,
            foreground: qa && args.iter().any(|arg| arg == "--qa-foreground"),
        }
    }

    /// Time the script may add to a run, for the limits that end a stuck
    /// self-test and refuse a slow one.
    pub fn script_budget(&self) -> Duration {
        self.script.iter().map(Step::budget).sum()
    }
}

impl StudioApp {
    /// QA only: start in the requested theme and language.
    pub(super) fn qa_preferences(&mut self) {
        if !self.qa {
            return;
        }
        if let Some(theme) = &self.qa_options.theme {
            self.profiles.preferences.theme = theme.clone();
        }
        if let Some(language) = &self.qa_options.language {
            self.profiles.preferences.language = language.clone();
        }
        if let Some(size) = self.qa_options.font_size {
            self.profiles.preferences.font_size = size;
        }
        if let Some(density) = &self.qa_options.density {
            self.profiles.preferences.density = density.clone();
        }
    }

    /// QA only: bring the requested section, dialog or state on screen. Runs
    /// once, after the rows of the fixture were shown.
    fn qa_view(&mut self) {
        let Some(view) = self.qa_options.view.clone() else {
            return;
        };
        let Some(connection) = self.qa_connection else {
            return;
        };
        let table = self.qa_chosen_table(connection);
        let table_tab = self.tabs.iter().position(|tab| tab.kind == TabKind::Table);
        // The sample content is in the language of the run, like the
        // interface around it.
        let en = self.en();
        let t = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let sample = |ssh: Option<SshAuth>| ConnectionProfile {
            name: t("Sklep (produkcja)", "Shop (production)").into(),
            engine: baza_core::drivers::postgres::ENGINE,
            host: "db.example.test".into(),
            database: t("sklep", "shop").into(),
            username: t("analityk", "analyst").into(),
            ssh: ssh.map(|auth| SshProfile {
                host: t("brama.example.test", "gateway.example.test").into(),
                port: 22,
                username: "deploy".into(),
                auth,
                password: None,
                private_key: None,
                passphrase: None,
                fingerprint: String::new(),
            }),
            ..ConnectionProfile::default()
        };
        let edit = |app: &mut Self, section: u8, ssh: Option<SshAuth>| {
            let draft = sample(ssh);
            app.connection_editor.open_draft(&draft);
            app.connection_editor.section = section;
            app.connection_dialog = Some(draft);
        };
        match view.as_str() {
            "sql" | "error" | "save-query" | "suggestions" => {
                self.set_section(Section::Sql);
                let tab = &mut self.tabs[self.active];
                tab.connection = Some(connection);
                tab.title = t("Aktywni klienci", "Active customers").into();
                tab.sql = "SELECT id, name, email, revenue\nFROM customers\nWHERE status = 'active'\nORDER BY id\nLIMIT 50;".into();
                self.run_active(false);
                if view == "error" {
                    self.notify("database_error [42601]".into(), true);
                    self.tabs[self.active].error = Some("query_timeout".into());
                }
                if view == "save-query" {
                    self.save_query_name = Some(t("Aktywni klienci", "Active customers").into());
                }
                if view == "suggestions" {
                    let tab = &mut self.tabs[self.active];
                    tab.sql = "SELECT * FROM c".into();
                    tab.selection = None;
                    self.suggestions = Some((
                        tab.id,
                        ["COUNT", "customers", "FROM", "SELECT", "WHERE"]
                            .map(str::to_owned)
                            .to_vec(),
                    ));
                }
            }
            "builder" => {
                if let Some(table) = table {
                    self.open_builder(connection, table);
                    if let Some(builder) = self.builder.as_mut() {
                        use baza_core::visual::{Aggregate, Field, Output};
                        let status = Field {
                            source: 0,
                            column: "status".into(),
                        };
                        builder.query.outputs = vec![
                            Output {
                                field: status.clone(),
                                aggregate: None,
                                alias: String::new(),
                            },
                            Output {
                                field: Field {
                                    source: 0,
                                    column: "*".into(),
                                },
                                aggregate: Some(Aggregate::Count),
                                alias: "orders".into(),
                            },
                        ];
                        builder.query.group_by = vec![status];
                        builder.page = super::builder_view::BuilderPage::Result;
                        let tab = builder.tab;
                        let dialect = self
                            .databases
                            .get(&connection)
                            .map_or(Dialect::ANSI, |db| *db.dialect());
                        if let Ok(sql) = builder.query.sql(&dialect) {
                            if let Some(target) = self.tabs.iter_mut().find(|t| t.id == tab) {
                                target.sql = sql;
                            }
                            self.run_tab(tab, false);
                        }
                    }
                }
            }
            "builder-empty" => {
                self.builder = None;
                self.section = Section::Builder;
            }
            "history" => self.set_section(Section::History),
            "history-empty" => {
                self.history.clear();
                self.set_section(Section::History);
            }
            "transfer" => self.set_section(Section::Transfer),
            "import" => {
                if let Some(index) = table_tab {
                    self.import = Some(ImportUi {
                        tab: self.tabs[index].id,
                        headers: vec![
                            "id".into(),
                            "name".into(),
                            "email".into(),
                            t("miasto", "city").into(),
                        ],
                        rows: (1..=4)
                            .map(|n| {
                                vec![
                                    Cell::Integer((9000 + n).to_string()),
                                    Cell::Text(format!("{} {n}", t("Klient", "Customer"))),
                                    Cell::Text(format!(
                                        "{}{n}@example.test",
                                        t("klient", "customer")
                                    )),
                                    Cell::Text(t("Łódź", "Leeds").into()),
                                ]
                            })
                            .collect(),
                        path: t("klienci.csv", "customers.csv").into(),
                    });
                }
                self.section = Section::Transfer;
            }
            "connections" => {
                self.navigation_selection = Some(NodeIdentity::Connection(connection));
                self.section = Section::Connections;
            }
            "profiles-export" => {
                self.section = Section::Connections;
                self.export_profiles_dialog();
            }
            "monitor" => {
                self.navigation_selection = Some(NodeIdentity::Connection(connection));
                self.section = Section::Monitor;
                // Earlier readings, so that the chart has a line to draw.
                self.monitor_connection = Some(connection);
                let now = chrono::Local::now();
                for step in (1..=24).rev() {
                    self.monitor_series.push_back(MonitorSample {
                        at: now - chrono::Duration::seconds(step * 5),
                        response_ms: 9.0
                            + 4.0 * (step as f64 * 0.55).sin()
                            + 2.0 * (step as f64 * 1.7).cos(),
                    });
                }
            }
            "connection-general" => edit(self, 0, None),
            "connection-ssh" => edit(self, 1, Some(SshAuth::PrivateKey)),
            "connection-ssh-password" => edit(self, 1, Some(SshAuth::Password)),
            "connection-ssl" => edit(self, 2, None),
            "preferences" => self.show_preferences = true,
            "preferences-editor" => {
                self.show_preferences = true;
                self.preference_section = 1;
            }
            "updates" => {
                self.show_distribution = true;
                self.update_message = Some(
                    self.tr("Masz najnowszą wersję.", "You have the latest version.")
                        .into(),
                );
            }
            "about" => self.show_about = true,
            "lock" => self.show_lock_settings = true,
            "locked" => {
                self.lock_settings.two_factor = true;
                self.locked = true;
            }
            "designer" => self.open_designer(connection),
            "design" => {
                if let Some(table) = table.clone() {
                    self.open_design(connection, table);
                }
            }
            "insert" => {
                if let Some(index) = table_tab {
                    self.begin_insert(index);
                }
            }
            "cell" => {
                if let Some(index) = table_tab {
                    let tab = &self.tabs[index];
                    self.cell_editor = Some(CellEditor {
                        all_marked: false,
                        inline: false,
                        fresh: true,
                        tab: tab.id,
                        row: 0,
                        column: 1,
                        value: "Customer 1".into(),
                        null: false,
                    });
                }
            }
            "group" => self.new_group(None),
            "delete" => self.connection_delete = Some(connection),
            "host-key" => {
                self.host_key_change = Some((
                    connection,
                    HostKeyScan {
                        fingerprint: "SHA256:7qJ3mVb0kq1cT5mW8yN2pLr4sXa6dZe9uHf0gIjKlMn".into(),
                        algorithm: "ssh-ed25519".into(),
                    },
                ));
            }
            "empty" => {
                self.disconnect(connection);
                self.profiles.connections.clear();
                self.navigation_selection = None;
                self.tabs = vec![QueryTab::new(None)];
                self.active = 0;
                self.notice = None;
                self.section = Section::Connections;
            }
            "table-empty" => {
                self.tabs.retain(|tab| tab.kind != TabKind::Table);
                self.active = 0;
                self.section = Section::Table;
            }
            "focus" => self.focus_mode = true,
            // The next views drive the same code as a click would, to see
            // that filtering, sorting, paging and editing still work.
            "table-filter" => {
                if let Some(index) = table_tab {
                    let tab = &mut self.tabs[index];
                    tab.filter_column = "status".into();
                    tab.filter_value = "pend".into();
                    self.browse(index);
                }
            }
            "table-sort" => {
                if let Some(index) = table_tab {
                    self.tabs[index].order_column = "name".into();
                    self.sort_result(index, 1);
                }
            }
            "table-page" => {
                if let Some(index) = table_tab {
                    self.tabs[index].page = 2;
                    self.browse(index);
                }
            }
            "table-edit" => {
                if let Some(index) = table_tab {
                    let tab = &mut self.tabs[index];
                    tab.selected_row = Some(2);
                    tab.selected_cell = Some((2, 3));
                    self.cell_editor = Some(CellEditor {
                        all_marked: false,
                        inline: true,
                        fresh: true,
                        tab: tab.id,
                        row: 1,
                        column: 1,
                        value: "Customer 2".into(),
                        null: false,
                    });
                }
            }
            "table-disconnected" => self.disconnect(connection),
            "table-marked" => {
                if let Some(index) = table_tab {
                    let tab = &mut self.tabs[index];
                    let mut range = super::results::GridRange::cell(1, 1);
                    range.focus = (4, 3);
                    tab.range = Some(range);
                    tab.selected_row = Some(4);
                    tab.selected_cell = Some((4, 3));
                }
            }
            "table-form" => {
                if let Some(index) = table_tab {
                    let tab = &mut self.tabs[index];
                    tab.form_view = true;
                    tab.selected_row = Some(2);
                }
            }
            "table-delete-many" => {
                if let Some(index) = table_tab {
                    self.tabs[index].range = Some(super::results::GridRange::rows(1));
                    if let Some(range) = self.tabs[index].range.as_mut() {
                        range.focus = (3, 0);
                    }
                    self.ask_delete(index);
                }
            }
            "objects" | "objects-drop" | "structure" => {
                if let Some(table) = table.clone() {
                    self.show_objects(connection, &table.schema, Some(ObjectKind::Table));
                    self.objects
                        .selected
                        .insert((table.schema.clone(), table.name.clone()));
                    if view == "structure" {
                        self.show_structure(connection, table.clone());
                    }
                    if view == "objects-drop" {
                        self.drop_confirm = Some((connection, vec![table]));
                    }
                }
            }
            "search" => self.nav_search = "cust".into(),
            "groups" => {
                self.group_operation(GroupOperation::Create(GroupEditor {
                    id: None,
                    name: t("Produkcja", "Production").into(),
                    color: DEFAULT_GROUP_COLOR,
                    parent: None,
                }));
                self.update_library(
                    Some(SavedQuery {
                        id: Uuid::new_v4(),
                        name: t("Aktywni klienci", "Active customers").into(),
                        connection: Some(connection),
                        sql: "SELECT * FROM customers WHERE status = 'active'".into(),
                    }),
                    None,
                );
                self.navigation_selection = Some(NodeIdentity::Connection(connection));
                self.section = Section::Connections;
            }
            "startup" => self.qa_startup = true,
            "startup-error" => {
                self.startup_error = Some("vault_keychain_access_denied".into());
            }
            // "table" and unknown names keep the default: the table data.
            _ => {}
        }
    }

    /// QA only: the connection the views work on: the one named by
    /// `BAZA_QA_CONNECTION`, otherwise the sample.
    pub(super) fn qa_chosen_connection(&self) -> Option<Uuid> {
        if !self.qa {
            return None;
        }
        let named = std::env::var("BAZA_QA_CONNECTION")
            .ok()
            .filter(|name| !name.is_empty());
        let connections = &self.profiles.connections;
        named
            .and_then(|name| connections.iter().find(|p| p.name == name))
            .or_else(|| connections.first())
            .map(|p| p.id)
    }

    /// QA only: the table a connected QA connection shows: the one named by
    /// `BAZA_QA_TABLE`, otherwise its first.
    pub(super) fn qa_chosen_table(&self, connection: Uuid) -> Option<TableRef> {
        let tables = self.tables.get(&connection)?;
        let named = std::env::var("BAZA_QA_TABLE")
            .ok()
            .filter(|name| !name.is_empty());
        named
            .and_then(|name| tables.iter().find(|table| table.name == name))
            .or_else(|| tables.iter().find(|table| table.name == "customers"))
            .or_else(|| tables.first())
            .cloned()
    }

    /// QA only: the private key file that stands in for the system file
    /// dialog of the SSH tab (`BAZA_QA_SSH_KEY_FILE`). Always `None` in a
    /// normal launch.
    pub(super) fn qa_key_file(&self) -> Option<PathBuf> {
        if !self.qa {
            return None;
        }
        std::env::var_os("BAZA_QA_SSH_KEY_FILE")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
    }

    /// QA only: feed the next piece of the script into the frame's input.
    pub(super) fn qa_input(&mut self, raw_input: &mut egui::RawInput) {
        if !self.qa_auto || self.qa_options.script.is_empty() {
            return;
        }
        // The script is the only pointer and keyboard of this window. The
        // person at the computer may be working meanwhile: what their mouse
        // and keys send here is dropped, so that a run always plays the same.
        raw_input.events.retain(|event| {
            !matches!(
                event,
                egui::Event::PointerMoved(_)
                    | egui::Event::PointerButton { .. }
                    | egui::Event::PointerGone
                    | egui::Event::MouseMoved(_)
                    | egui::Event::MouseWheel { .. }
                    | egui::Event::Zoom(_)
                    | egui::Event::Touch { .. }
                    | egui::Event::Key { .. }
                    | egui::Event::Text(_)
                    | egui::Event::Paste(_)
                    | egui::Event::Copy
                    | egui::Event::Cut
                    | egui::Event::Ime(_)
                    | egui::Event::ModifiersChanged(_)
                    | egui::Event::WindowFocused(_)
            )
        });
        raw_input.hovered_files.clear();
        raw_input.dropped_files.clear();
        raw_input.focused = true;
        let Some(applied) = self.qa_view_applied else {
            return;
        };
        let progress = &mut self.qa_script;
        let Some(step) = self.qa_options.script.get(progress.step) else {
            return;
        };
        // A screenshot step ends when its picture has been saved (`qa_tick`).
        // Nothing is played either while the window is being brought back to
        // its pixel density, for example after it moved to another display.
        if progress.shot.is_some() || !progress.settled {
            return;
        }
        // Let the view, and then each step, settle before the next one.
        let since = progress.last.unwrap_or(applied);
        if progress.frame == 0 && since.elapsed() < STEP_SETTLE {
            return;
        }
        let button = |pos: egui::Pos2, button: egui::PointerButton, pressed: bool| {
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }
        };
        let primary = egui::PointerButton::Primary;
        let secondary = egui::PointerButton::Secondary;
        let events = &mut raw_input.events;
        let frames = match step {
            Step::Click(pos) | Step::RightClick(pos) => {
                let which = if matches!(step, Step::Click(_)) {
                    primary
                } else {
                    secondary
                };
                match progress.frame {
                    0 => events.push(egui::Event::PointerMoved(*pos)),
                    1 => events.push(button(*pos, which, true)),
                    _ => events.push(button(*pos, which, false)),
                }
                click_clock(progress, &mut raw_input.time);
                3
            }
            Step::ClickWith(pos, modifiers) => {
                // The modifier is held for the whole click, as by a hand.
                let event = |pressed: bool| egui::Event::PointerButton {
                    pos: *pos,
                    button: primary,
                    pressed,
                    modifiers: *modifiers,
                };
                match progress.frame {
                    0 => {
                        events.push(egui::Event::ModifiersChanged(*modifiers));
                        events.push(egui::Event::PointerMoved(*pos));
                    }
                    1 => events.push(event(true)),
                    2 => events.push(event(false)),
                    _ => events.push(egui::Event::ModifiersChanged(egui::Modifiers::NONE)),
                }
                click_clock(progress, &mut raw_input.time);
                4
            }
            Step::Drag(from, to) => {
                // Press, five moves along the way, release.
                const MOVES: u8 = 5;
                match progress.frame {
                    0 => events.push(egui::Event::PointerMoved(*from)),
                    1 => events.push(button(*from, primary, true)),
                    frame if frame < 2 + MOVES => {
                        let t = f32::from(frame - 1) / f32::from(MOVES);
                        events.push(egui::Event::PointerMoved(from.lerp(*to, t)));
                    }
                    _ => events.push(button(*to, primary, false)),
                }
                3 + MOVES
            }
            Step::Copy => {
                events.push(egui::Event::Copy);
                1
            }
            Step::Paste(text) => {
                events.push(egui::Event::Paste(text.clone()));
                1
            }
            Step::DoubleClick(pos) => {
                match progress.frame {
                    0 => events.push(egui::Event::PointerMoved(*pos)),
                    1 | 3 => events.push(button(*pos, primary, true)),
                    _ => events.push(button(*pos, primary, false)),
                }
                click_clock(progress, &mut raw_input.time);
                5
            }
            Step::Move(pos) => {
                events.push(egui::Event::PointerMoved(*pos));
                1
            }
            Step::Scroll(pos, distance) => {
                if progress.frame == 0 {
                    events.push(egui::Event::PointerMoved(*pos));
                } else {
                    // The wheel moves the content: up, to see what is below.
                    events.push(egui::Event::MouseWheel {
                        unit: egui::MouseWheelUnit::Point,
                        delta: egui::vec2(0.0, -*distance),
                        phase: egui::TouchPhase::Move,
                        modifiers: egui::Modifiers::NONE,
                    });
                }
                2
            }
            Step::Text(text) => {
                events.push(egui::Event::Text(text.clone()));
                1
            }
            Step::Key(key, modifiers) => {
                let pressed = progress.frame == 0;
                if pressed && modifiers.any() {
                    events.push(egui::Event::ModifiersChanged(*modifiers));
                }
                events.push(egui::Event::Key {
                    key: *key,
                    physical_key: Some(*key),
                    pressed,
                    repeat: false,
                    modifiers: *modifiers,
                });
                if !pressed && modifiers.any() {
                    events.push(egui::Event::ModifiersChanged(egui::Modifiers::NONE));
                }
                2
            }
            Step::Wait(ms) => {
                if since.elapsed() < Duration::from_millis(*ms) {
                    return;
                }
                1
            }
            Step::Shot(name) => {
                progress.shot = Some((name.clone(), false));
                return;
            }
        };
        progress.frame += 1;
        if progress.frame >= frames {
            progress.frame = 0;
            progress.step += 1;
            progress.last = Some(Instant::now());
        }
    }

    /// QA only: the script was played to its end and had time to take effect.
    fn qa_script_done(&self) -> bool {
        self.qa_options.script.is_empty()
            || (self.qa_script.step >= self.qa_options.script.len()
                && self
                    .qa_script
                    .last
                    .is_some_and(|last| last.elapsed() >= Duration::from_millis(1200)))
    }

    /// QA only: render at the pixel density of `--qa-scale` and keep the
    /// window at its size in points, for the whole run. `false` while that is
    /// being restored: at the start, and again whenever the display changes
    /// under the window (it is moved to a screen of another density, a
    /// monitor is unplugged).
    fn qa_scale_ready(&mut self, ctx: &egui::Context) -> bool {
        let (Some(scale), Some(size)) = (self.qa_options.scale, self.qa_options.size) else {
            return true;
        };
        let started = self.qa_view_applied.is_some();
        let progress = &mut self.qa_script;
        if (ctx.pixels_per_point() - scale).abs() > 0.001 {
            // Takes effect with the next frame; the size is asked for anew.
            ctx.set_pixels_per_point(scale);
            progress.resizing = None;
            progress.size_refused = false;
            if started && progress.settled {
                progress.density_restored += 1;
            }
            return false;
        }
        let wanted = egui::vec2(size[0], size[1]);
        if (ctx.content_rect().size() - wanted).abs().max_elem() <= 1.0 {
            progress.resizing = None;
            return true;
        }
        if progress.size_refused {
            return true;
        }
        // A system that refuses the size is not asked forever; the report
        // states the size that was really rendered.
        if progress.resizing.get_or_insert_with(Instant::now).elapsed() > Duration::from_secs(4) {
            progress.size_refused = true;
            return true;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(wanted));
        false
    }

    /// QA only: a captured frame has the pixel density and the pixel size the
    /// run was asked for. A frame taken while the display or the zoom was
    /// changing is not saved; it is asked for again once the window settled.
    fn qa_frame_fits(&self, image: &egui::ColorImage) -> bool {
        let (Some(scale), Some(size)) = (self.qa_options.scale, self.qa_options.size) else {
            return true;
        };
        self.qa_script.settled
            && (self.qa_script.size_refused
                || (0..2).all(|axis| {
                    ((size[axis] * scale).round() - image.size[axis] as f32).abs() <= 1.0
                }))
    }

    /// Where a screenshot of this run is written: the report with the
    /// extension `png`, and `-<name>` before it for a `s:<name>` step.
    fn qa_shot_path(&self, name: Option<&str>) -> PathBuf {
        let report = self
            .qa_report
            .clone()
            .unwrap_or_else(|| PathBuf::from("native-ui-report.json"));
        match name {
            None => report.with_extension("png"),
            Some(name) => {
                let stem = report
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default();
                report.with_file_name(format!("{stem}-{name}.png"))
            }
        }
    }

    pub(super) fn qa_tick(&mut self, ctx: &egui::Context) {
        if !self.qa_auto || self.qa_completed {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(
            if self.qa_options.script.is_empty() {
                100
            } else {
                16
            },
        ));
        let rows = self
            .tabs
            .iter()
            .filter_map(|t| t.result.as_ref())
            .map(|r| r.rows.len())
            .max()
            .unwrap_or(0);
        self.qa_rows = self.qa_rows.max(rows);
        let settled = self.qa_scale_ready(ctx);
        self.qa_script.settled = settled;
        let ready = self.startup.elapsed() > Duration::from_secs(2) && self.qa_rows > 0 && settled;
        if ready && self.qa_view_applied.is_none() {
            self.qa_view();
            self.qa_view_applied = Some(Instant::now());
        }
        // A chosen view gets a moment for its own queries and layout to settle.
        let settle = if self.qa_options.view.is_some() {
            Duration::from_millis(1800)
        } else {
            Duration::ZERO
        };
        if settled
            && let Some((name, requested)) = self.qa_script.shot.as_mut()
            && !*requested
        {
            *requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
                NamedShot(name.clone()),
            )));
        }
        if self
            .qa_view_applied
            .is_some_and(|applied| applied.elapsed() >= settle)
            && self.qa_script_done()
            && settled
            && !self.qa_screenshot_requested
        {
            self.qa_screenshot_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        let screenshots = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| {
                    if let egui::Event::Screenshot {
                        image, user_data, ..
                    } = e
                    {
                        let name = user_data
                            .data
                            .as_ref()
                            .and_then(|data| data.downcast_ref::<NamedShot>())
                            .map(|shot| shot.0.clone());
                        Some((name, image.clone()))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        });
        for (name, image) in screenshots {
            // One file per request. The logic of a frame can run more than
            // once with the same events (egui repeats a pass it discarded),
            // and a frame can be captured while the display is changing.
            let fits = self.qa_frame_fits(&image);
            match &name {
                Some(name) => {
                    let awaited = matches!(
                        &self.qa_script.shot,
                        Some((pending, true)) if pending == name
                    );
                    if !awaited {
                        continue;
                    }
                    if !fits {
                        // Asked for again when the window has settled.
                        self.qa_script.shot = Some((name.clone(), false));
                        continue;
                    }
                }
                None => {
                    if self.qa_completed || !self.qa_screenshot_requested {
                        continue;
                    }
                    if !fits {
                        self.qa_screenshot_requested = false;
                        continue;
                    }
                }
            }
            let png = self.qa_shot_path(name.as_deref());
            let mut pixels = Vec::with_capacity(image.pixels.len() * 4);
            for c in &image.pixels {
                pixels.extend_from_slice(&c.to_array());
            }
            let encoded = image::save_buffer_with_format(
                &png,
                &pixels,
                image.size[0] as u32,
                image.size[1] as u32,
                image::ColorType::Rgba8,
                image::ImageFormat::Png,
            );
            if let Some(name) = name {
                // A step of the script: note it and play the next one.
                if encoded.is_ok() {
                    self.qa_script.shots.push(name);
                }
                self.qa_script.shot = None;
                self.qa_script.frame = 0;
                self.qa_script.step += 1;
                self.qa_script.last = Some(Instant::now());
                continue;
            }
            let path = self
                .qa_report
                .clone()
                .unwrap_or_else(|| PathBuf::from("native-ui-report.json"));
            let run = self.qa_run.map(|v| v.to_string());
            let owned = self
                .qa_root
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p.join("owned-qa.marker")).ok())
                == run;
            let report = serde_json::json!({
                "product": "Baza Studio",
                "implementation": "native_rust_egui_wgpu",
                "version": env!("CARGO_PKG_VERSION"),
                "qa_run": run,
                "qa_view": self.qa_options.view,
                "qa_connection": std::env::var("BAZA_QA_CONNECTION")
                    .ok()
                    .filter(|name| !name.is_empty()),
                "qa_shots": self.qa_script.shots,
                "qa_clipboard": self.qa_clipboard,
                "owned_fixture": owned,
                "renderer_screenshot": encoded.is_ok(),
                "viewport_width": image.size[0],
                "viewport_height": image.size[1],
                "pixels_per_point": ctx.pixels_per_point(),
                "qa_size_refused": self.qa_script.size_refused,
                "qa_density_restored": self.qa_script.density_restored,
                "frame_completed": true,
                "actual_sqlite_rows_rendered": self.qa_rows,
                "vault_opened": self.vault.is_some(),
                "elapsed_ms": self.startup.elapsed().as_millis(),
                "limitations": "Renderer acceptance only; does not claim human UI interaction or remote engine tests.",
            });
            let _ = std::fs::write(&path, serde_json::to_vec_pretty(&report).unwrap());
            self.qa_completed = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if !self.qa_completed
            && self.startup.elapsed() > RUN_LIMIT + self.qa_options.script_budget()
        {
            let p = self
                .qa_report
                .clone()
                .unwrap_or_else(|| "native-ui-report.json".into());
            let _ = std::fs::write(
                p,
                b"{\"renderer_screenshot\":false,\"failure\":\"native_renderer_timeout\"}",
            );
            self.qa_completed = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn switches_are_ignored_outside_the_qa_modes() {
        let args = args(&[
            "BazaStudio",
            "--qa-view",
            "sql",
            "--qa-theme",
            "oled",
            "--qa-lang",
            "en",
            "--qa-font-size",
            "16",
            "--qa-density",
            "comfortable",
            "--qa-script",
            "c:10,20",
        ]);
        let normal = QaOptions::from_args(&args, false);
        assert!(normal.view.is_none() && normal.theme.is_none() && normal.language.is_none());
        assert!(normal.font_size.is_none() && normal.density.is_none());
        assert!(normal.script.is_empty());
        let qa = QaOptions::from_args(&args, true);
        assert_eq!(qa.view.as_deref(), Some("sql"));
        assert_eq!(qa.theme.as_deref(), Some("oled"));
        assert_eq!(qa.language.as_deref(), Some("en"));
        assert_eq!(qa.font_size, Some(16.0));
        assert_eq!(qa.script, vec![Step::Click(egui::pos2(10.0, 20.0))]);
    }

    #[test]
    fn script_steps_are_parsed_and_unknown_ones_dropped() {
        let first = args(&[
            "BazaStudio",
            "--qa-script",
            "c:1,2;d:3.5,4;r:5,6;t:a:b;k:enter;k:nope;x:1,1;c:wide",
        ]);
        assert_eq!(
            QaOptions::from_args(&first, true).script,
            vec![
                Step::Click(egui::pos2(1.0, 2.0)),
                Step::DoubleClick(egui::pos2(3.5, 4.0)),
                Step::RightClick(egui::pos2(5.0, 6.0)),
                Step::Text("a:b".into()),
                Step::Key(egui::Key::Enter, egui::Modifiers::NONE),
            ]
        );
        let more = args(&[
            "BazaStudio",
            "--qa-script",
            "m:7,8;v:5,6,120;v:5,6;w:250;w:70000;s:after-test_1;s:bad name;s:;k:cmd+a;k:shift+tab;k:down;k:x;k:hyper+a;k:f13",
        ]);
        assert_eq!(
            QaOptions::from_args(&more, true).script,
            vec![
                Step::Move(egui::pos2(7.0, 8.0)),
                Step::Scroll(egui::pos2(5.0, 6.0), 120.0),
                Step::Wait(250),
                Step::Shot("after-test_1".into()),
                Step::Key(egui::Key::A, egui::Modifiers::COMMAND),
                Step::Key(egui::Key::Tab, egui::Modifiers::SHIFT),
                Step::Key(egui::Key::ArrowDown, egui::Modifiers::NONE),
                Step::Key(egui::Key::X, egui::Modifiers::NONE),
            ]
        );
        // A text size outside the range of the preferences is not applied.
        let odd = QaOptions::from_args(&["x", "--qa-font-size", "90"].map(String::from), true);
        assert!(odd.font_size.is_none());
    }

    #[test]
    fn clicks_of_a_script_do_not_depend_on_the_time_of_a_frame() {
        // Frames that take half a second and more: far beyond the 0.3 s in
        // which egui lets two clicks be a double click.
        let mut progress = ScriptProgress::default();
        let mut seen = Vec::new();
        for (frame, real) in [(0u8, 10.0), (1, 10.6), (2, 11.3), (3, 12.1), (4, 13.0)] {
            progress.frame = frame;
            let mut time = Some(real);
            click_clock(&mut progress, &mut time);
            seen.push(time.unwrap());
        }
        // The pointer moves and goes down on the real clock...
        assert_eq!(&seen[..2], [10.0, 10.6]);
        // ... and everything up to the second release follows closely.
        assert!(seen[4] - seen[1] <= 3.0 * CLICK_FRAME + 1e-9, "{seen:?}");
        assert!(seen.windows(2).all(|pair| pair[0] <= pair[1]), "{seen:?}");
        // Quick frames keep their own, earlier times: the clock never runs
        // ahead of the real one.
        let mut progress = ScriptProgress::default();
        let mut seen = Vec::new();
        for (frame, real) in [(0u8, 5.0), (1, 5.004), (2, 5.008), (3, 5.012), (4, 5.016)] {
            progress.frame = frame;
            let mut time = Some(real);
            click_clock(&mut progress, &mut time);
            seen.push(time.unwrap());
        }
        assert_eq!(seen, [5.0, 5.004, 5.008, 5.012, 5.016]);
        // A frame without a clock is left without one.
        let mut time = None;
        progress.frame = 2;
        click_clock(&mut progress, &mut time);
        assert!(time.is_none());
    }

    #[test]
    fn a_run_that_was_never_painted_leaves_a_failure_report_and_no_qa_data() {
        let directory = tempfile::tempdir().unwrap();
        // The QA vault and folder of a run, as `load_vault` registers them.
        let root = directory
            .path()
            .join(format!("BazaStudio-native-render-QA-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        #[cfg(any(windows, target_os = "macos"))]
        {
            let namespace = format!("BazaStudio.QA.{}", Uuid::new_v4());
            let vault = Arc::new(Vault::open_owned(&root.join("vault"), &namespace).unwrap());
            vault.save_profiles(&ProfileSet::default()).unwrap();
            owned(&vault, &root);
        }
        let report = directory.path().join("report.json");
        give_up(&report);
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        assert_eq!(written["failure"], "native_window_not_painted");
        assert_eq!(written["renderer_screenshot"], false);
        #[cfg(any(windows, target_os = "macos"))]
        assert!(!root.exists());
        // A report the run wrote itself is never replaced.
        std::fs::write(&report, b"{\"failure\":\"native_renderer_timeout\"}").unwrap();
        give_up(&report);
        assert!(
            std::fs::read_to_string(&report)
                .unwrap()
                .contains("native_renderer_timeout")
        );
    }

    #[test]
    fn foreground_is_a_qa_switch_too() {
        let list = args(&["BazaStudio", "--qa-foreground"]);
        assert!(QaOptions::from_args(&list, true).foreground);
        assert!(!QaOptions::from_args(&list, false).foreground);
        assert!(!QaOptions::from_args(&args(&["BazaStudio"]), true).foreground);
    }

    #[test]
    fn script_file_scale_and_time_budget() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("steps.txt");
        std::fs::write(&file, "# open the dialog\nc:10,20; w:1500\n\ns:first\n").unwrap();
        let path = file.to_string_lossy().into_owned();
        let list = args(&[
            "BazaStudio",
            "--qa-script",
            "t:x",
            "--qa-script-file",
            &path,
            "--qa-scale",
            "2",
        ]);
        let options = QaOptions::from_args(&list, true);
        assert_eq!(
            options.script,
            vec![
                Step::Text("x".into()),
                Step::Click(egui::pos2(10.0, 20.0)),
                Step::Wait(1500),
                Step::Shot("first".into()),
            ]
        );
        assert_eq!(options.scale, Some(2.0));
        // Settle time of four steps, the wait, one screenshot and two inputs.
        assert_eq!(
            options.script_budget(),
            Duration::from_millis(4 * 450 + 1500 + 10_000 + 2 * 1500)
        );
        let normal = QaOptions::from_args(&list, false);
        assert!(normal.script.is_empty() && normal.scale.is_none());
        assert_eq!(normal.script_budget(), Duration::ZERO);
        for scale in ["3", "0", "1.5", "big"] {
            let odd = QaOptions::from_args(&args(&["x", "--qa-scale", scale]), true);
            assert!(odd.scale.is_none(), "{scale}");
        }
    }
}
