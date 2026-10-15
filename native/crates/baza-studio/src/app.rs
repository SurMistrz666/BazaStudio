use crate::connection_edit::{self, EditorState, SecretDraft, SecretProfiles};
use crate::transfer::{self, Format};
use crate::{
    navigator::{self, NodeIdentity, ObjectKind, QueryNode},
    theme,
    widgets::{self, Icon},
};
use baza_core::drivers::{PageRequest, RowChange};
use baza_core::engine::{DefaultSchema, localized};
use baza_core::groups::{Groups, Snapshot as GroupSnapshot};
use baza_core::local_lock::{Enrollment, LocalLock, LockSettings};
use baza_core::profiles::{self, ImportPreview};
use baza_core::{database::Database, sql, *};
use baza_core::{
    ssh::HostKeyScan,
    updater::{self, UpdateCheck, UpdateOffer, Updater},
};
use eframe::egui::{self, Color32};
use std::sync::Mutex;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroize;

mod builder_view;
mod connection_dialog;
mod connections_view;
mod design_extras;
mod design_view;
mod dialogs;
mod history_view;
mod key_tree;
mod lock;
mod monitor_view;
mod objects_view;
mod preferences;
mod qa;
mod results;
mod shell;
mod shortcuts;
mod sidebar;
mod sql_view;
mod staged;
mod table_view;
mod transfer_view;
mod value_view;

pub use qa::{QaOptions, finished as qa_finished, watchdog as qa_watchdog};

/// The sections of the workspace, in the order of the section strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    /// The tables and views of a schema, as a list with their actions.
    Objects,
    /// The table designer, a tab of the work area; not in the strip.
    Design,
    Sql,
    Table,
    Builder,
    History,
    Transfer,
    Connections,
    Monitor,
}
impl Section {
    const ALL: [Section; 8] = [
        Section::Objects,
        Section::Sql,
        Section::Table,
        Section::Builder,
        Section::History,
        Section::Transfer,
        Section::Connections,
        Section::Monitor,
    ];
    fn label(self, en: bool) -> &'static str {
        let (pl, english) = match self {
            Section::Objects => ("Obiekty", "Objects"),
            Section::Design => ("Projekt tabeli", "Table design"),
            Section::Sql => ("Edytor SQL", "SQL editor"),
            Section::Table => ("Dane tabeli", "Table data"),
            Section::Builder => ("Kreator zapytań", "Query builder"),
            Section::History => ("Historia", "History"),
            Section::Transfer => ("Import i eksport", "Import and export"),
            Section::Connections => ("Połączenia", "Connections"),
            Section::Monitor => ("Monitor", "Monitor"),
        };
        if en { english } else { pl }
    }
}

/// What a tab holds, which decides the section that shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TabKind {
    /// A query written in the SQL editor.
    Query,
    /// The rows of one table or view, browsed in "Dane tabeli".
    Table,
    /// The result of the query builder. There is at most one such tab.
    Builder,
}

/// Colour of a new connection group; a group in this colour is drawn neutral.
const DEFAULT_GROUP_COLOR: [u8; 3] = [57, 118, 223];
/// Colour of a new connection; a connection in this colour is drawn neutral.
const DEFAULT_CONNECTION_COLOR: [u8; 3] = [71, 155, 230];

/// One reading of the monitor.
struct MonitorSample {
    at: chrono::DateTime<chrono::Local>,
    response_ms: f64,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SavedQuery {
    id: Uuid,
    name: String,
    connection: Option<Uuid>,
    sql: String,
}
#[derive(Clone)]
struct GroupEditor {
    id: Option<Uuid>,
    name: String,
    color: [u8; 3],
    parent: Option<Uuid>,
}
enum GroupOperation {
    Create(GroupEditor),
    Rename(GroupEditor),
    Remove(Uuid),
    Move(Uuid, Option<Uuid>),
    Membership(Uuid, Option<Uuid>),
    RemoveConnection(Uuid),
}
struct QueryTab {
    id: Uuid,
    kind: TabKind,
    /// Table or view browsed by a [`TabKind::Table`] tab. Unlike `table`, it
    /// stays set while a query runs and for results that cannot be edited.
    source: Option<TableRef>,
    request: Uuid,
    connection: Option<Uuid>,
    title: String,
    sql: String,
    result: Option<QueryResult>,
    error: Option<String>,
    busy: bool,
    cancel: CancellationToken,
    table: Option<TableRef>,
    /// What a [`TabKind::Table`] tab reads: its container with the filter,
    /// the order and the page. `sql` is the same request as text.
    page_request: Option<PageRequest>,
    columns: Vec<ColumnInfo>,
    filter_column: String,
    filter_value: String,
    /// The filter field holds a condition in SQL, not text to search for.
    /// A table opens with the search; the condition is a choice in the
    /// list of columns.
    filter_sql: bool,
    /// The names offered at the cursor while a condition is typed.
    filter_hints: table_view::FilterHints,
    /// Columns of the result the grid does not show, by name.
    hidden_columns: std::collections::BTreeSet<String>,
    /// What the list of columns to show is narrowed to.
    column_search: String,
    /// The tree of the keys of a key-value store is beside the grid.
    key_tree: bool,
    /// Categories of that tree that are folded, by their prefix.
    folded_keys: std::collections::BTreeSet<String>,
    order_column: String,
    descending: bool,
    /// Further columns the rows are ordered by, after `order_column`.
    then_by: Vec<(String, bool)>,
    page: u64,
    /// Records per page chosen for this tab; `None` follows the preferences.
    page_rows: Option<u32>,
    selection: Option<std::ops::Range<usize>>,
    selected_row: Option<usize>,
    selected_cell: Option<(usize, usize)>,
    /// Cells, rows or columns marked in the grid, for copying and deleting.
    range: Option<results::GridRange>,
    /// The pointer went down on a cell and is still held: moving it extends
    /// the range.
    dragging: bool,
    /// The grid shows one record as a form instead of rows.
    form_view: bool,
    /// Records the filter of the table lets through, once counted for the
    /// last page.
    total: Option<u64>,
    /// Values typed into the grid and not saved yet, by row and column.
    pending: std::collections::BTreeMap<(usize, usize), Cell>,
    /// Rows added at the end of the grid and not saved yet.
    added: std::collections::BTreeSet<usize>,
    /// The waiting changes in the order they were made, each with the values
    /// it replaced, for Mod+Z.
    pending_log: Vec<StagedStep>,
    /// A row to bring into view in the next frame.
    scroll_to: Option<usize>,
    /// A row to select once the page being loaded arrives (Mod+G).
    pending_row: Option<usize>,
}
/// What a new query starts with.
const SAMPLE_SQL: &str = "-- Ctrl+Enter\nSELECT 1;";

impl QueryTab {
    fn new(c: Option<Uuid>) -> Self {
        Self {
            id: Uuid::new_v4(),
            kind: TabKind::Query,
            source: None,
            request: Uuid::new_v4(),
            connection: c,
            title: "SQL".into(),
            sql: SAMPLE_SQL.into(),
            result: None,
            error: None,
            busy: false,
            cancel: CancellationToken::new(),
            table: None,
            page_request: None,
            columns: vec![],
            filter_column: String::new(),
            filter_value: String::new(),
            filter_sql: false,
            filter_hints: Default::default(),
            hidden_columns: Default::default(),
            column_search: String::new(),
            key_tree: true,
            folded_keys: Default::default(),
            order_column: String::new(),
            descending: false,
            then_by: Vec::new(),
            page: 0,
            page_rows: None,
            selection: None,
            selected_row: None,
            selected_cell: None,
            range: None,
            dragging: false,
            form_view: false,
            total: None,
            pending: Default::default(),
            added: Default::default(),
            pending_log: Vec::new(),
            scroll_to: None,
            pending_row: None,
        }
    }
}
#[derive(Default)]
struct PreferencesSaveState {
    request: Option<Uuid>,
    error: Option<String>,
    open_security: bool,
}
#[derive(Debug, PartialEq, Eq)]
enum PreferencesSaveCompletion {
    Ignored,
    Failed,
    Saved { open_security: bool },
}
impl PreferencesSaveState {
    fn busy(&self) -> bool {
        self.request.is_some()
    }
    fn begin(&mut self, open_security: bool) -> Option<Uuid> {
        if self.busy() {
            return None;
        }
        let request = Uuid::new_v4();
        self.request = Some(request);
        self.error = None;
        self.open_security = open_security;
        Some(request)
    }
    fn complete(
        &mut self,
        request: Uuid,
        result: Result<SecretProfiles, String>,
        current: &mut ProfileSet,
        original: &mut Option<Preferences>,
        visible: &mut bool,
    ) -> PreferencesSaveCompletion {
        if self.request != Some(request) {
            // SecretProfiles erases a stale successful response on drop.
            return PreferencesSaveCompletion::Ignored;
        }
        self.request = None;
        match result {
            Ok(mut saved) => {
                drop(connection_edit::replace_profiles(
                    current,
                    std::mem::take(&mut saved.0),
                ));
                *original = None;
                *visible = false;
                self.error = None;
                PreferencesSaveCompletion::Saved {
                    open_security: std::mem::take(&mut self.open_security),
                }
            }
            Err(error) => {
                self.error = Some(error);
                self.open_security = false;
                *visible = true;
                // The preview/draft and cancel snapshot are deliberately retained.
                PreferencesSaveCompletion::Failed
            }
        }
    }
}
fn persist_preferences(vault: &Vault, draft: Preferences) -> anyhow::Result<SecretProfiles> {
    // Read the current encrypted profile set before merging only preferences.
    // This refreshes the vault CAS baseline and preserves another instance's
    // connection changes; a write racing this read still fails its CAS.
    let mut latest = SecretProfiles(vault.load_profiles()?);
    latest.0.preferences = draft;
    vault.save_profiles(&latest.0)?;
    Ok(latest)
}
/// What the start of the application reads: the vault, the profiles, the
/// query history and the state of the local lock.
type LoadedVault = (
    Arc<Vault>,
    ProfileSet,
    Vec<HistoryRecord>,
    Arc<LocalLock>,
    LockSettings,
);
/// Headers, rows and file name of a file chosen for a data import.
type FilePreview = (Vec<String>, Vec<Vec<Cell>>, String);

enum Message {
    ConnectionSaved(Uuid, Result<ProfileSet, String>),
    ConnectionBaseReload(Uuid, Result<ProfileSet, String>),
    /// Result of testing a draft: the server key learned on first use, or the
    /// error code together with a changed server key when there is one.
    ConnectionTest(
        Uuid,
        [u8; 32],
        Result<Option<String>, (String, Option<HostKeyScan>)>,
    ),
    TablesLoaded(Uuid, Uuid, Result<Vec<TableRef>, String>),
    Loaded(Result<LoadedVault, String>),
    Connected(
        Uuid,
        Uuid,
        Result<(Database, Vec<TableRef>, Option<TunnelManager>), String>,
    ),
    Query(
        Uuid,
        Uuid,
        HistoryRecord,
        Option<TableRef>,
        Vec<ColumnInfo>,
        Result<QueryResult, String>,
    ),
    Saved(Result<(), String>),
    PreferencesSaved(Uuid, Result<SecretProfiles, String>),
    FileSql(Uuid, Result<String, String>),
    Notice(Result<String, String>),
    Mutation(Uuid, Result<(), String>),
    Exported(Result<String, String>),
    DataPreview(Uuid, Result<FilePreview, String>),
    ProfileFile(Uuid, Result<(Vec<u8>, String), String>),
    ProfilePreview(Uuid, Result<ImportPreview, String>),
    PrivateKey(Uuid, Uuid, Uuid, Result<zeroize::Zeroizing<String>, String>),
    Unlocked(Result<(ProfileSet, Vec<HistoryRecord>), String>),
    Enrollment(Result<Enrollment, String>),
    LockConfirmed(Result<(LockSettings, Vec<String>), String>),
    LockStopped(Result<(), String>),
    /// Server statistics of the connection with this id.
    Monitor(Uuid, Result<QueryResult, String>),
    /// Columns of a table on the canvas of the query builder of this
    /// connection, by `schema.table`.
    BuilderColumns(Uuid, String, Result<Vec<ColumnInfo>, String>),
    /// A path chosen in a file dialog of the connection editor with this id.
    PathPicked(Uuid, connection_dialog::PathTarget, PathBuf),
    Distribution(Result<Arc<Updater>, String>),
    Update(Result<UpdateCheck, String>),
    /// `None` when the file dialog was cancelled.
    UpdateFileChecked(Result<Option<bool>, String>),
    /// Host key fetched for the connection editor instance with this id.
    HostKey(Uuid, Result<HostKeyScan, String>),
    /// A connection without a saved fingerprint trusted this key on first use.
    HostKeyLearned(Uuid, String),
    /// The server of this connection presents a key other than the saved one.
    HostKeyChanged(Uuid, HostKeyScan),
    /// The query history was removed from the vault: the number of records.
    HistoryCleared(Result<usize, String>),
    /// Several rows of the tab with this id were changed in one go.
    Mutations(Uuid, usize, Option<String>),
    /// Tables and views of this connection were dropped (how many), or the
    /// first refusal.
    Dropped(Uuid, Result<usize, String>),
    /// Columns of the table whose structure is shown, for this request.
    Structure(Uuid, Result<Vec<ColumnInfo>, String>),
    /// Records the filter of the table of this tab lets through.
    RecordCount(Uuid, Result<u64, String>),
    /// Facts about an object for the information panel, for this request.
    Details(Uuid, Result<QueryResult, String>),
    /// Columns of a table opened in the designer, with MySQL's extras.
    DesignColumns(Uuid, Result<(Vec<ColumnInfo>, Option<QueryResult>), String>),
    /// The statements of a design ran (how many), or the first refusal.
    DesignSaved(Uuid, Result<usize, String>),
    /// The parts of a design beyond its columns, and what the server offers.
    DesignExtras(
        Uuid,
        Result<
            (
                Option<baza_core::ddl::mysql::TableExtras>,
                design_extras::Catalog,
            ),
            String,
        >,
    ),
    /// The columns of a table a foreign key of a design points at.
    DesignRefColumns(Uuid, String, Result<Vec<String>, String>),
    Library(u64, bool, Result<Vec<SavedQuery>, String>),
    GroupsLoaded(u64, Result<(Arc<Groups>, GroupSnapshot), String>),
    GroupSaved(u64, Result<GroupSnapshot, String>),
    /// The structures bound to columns, read or written.
    Layouts(Result<Vec<value_view::BlobLayout>, String>),
}
struct NewRowEditor {
    tab: Uuid,
    values: Vec<(String, String, bool, bool)>,
}
/// Why changes of data were made, for the undo history.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChangeKind {
    Do,
    Undo,
    Redo,
}
/// One step of waiting changes: each cell with the value it had waiting
/// before, if any.
type StagedStep = Vec<((usize, usize), Option<Cell>)>;
/// The waiting changes of a grid: values by cell, new rows, their order.
type StagedBackup = (
    std::collections::BTreeMap<(usize, usize), Cell>,
    std::collections::BTreeSet<usize>,
    Vec<StagedStep>,
);

/// Changes sent to the database, with how to take each one back.
struct PendingChanges {
    changes: Vec<RowChange>,
    inverses: Vec<Option<RowChange>>,
    kind: ChangeKind,
    /// The grid already shows the new values: no reload when all went in.
    shown: bool,
}

/// The change that takes `change` back. A removed row comes back from its
/// values; an added row is removed by its key; a cell gets its old value,
/// found by the key it now has.
fn inverse_change(
    change: &RowChange,
    row: Option<&[(String, Cell)]>,
    keys: &[String],
) -> Option<RowChange> {
    match change {
        RowChange::UpdateCell {
            table,
            column,
            old,
            new,
            key,
        } => Some(RowChange::UpdateCell {
            table: table.clone(),
            column: column.clone(),
            old: new.clone(),
            new: old.clone(),
            key: key
                .iter()
                .map(|(name, value)| {
                    if name == column {
                        (name.clone(), new.clone())
                    } else {
                        (name.clone(), value.clone())
                    }
                })
                .collect(),
        }),
        RowChange::DeleteRow { table, .. } => {
            let values = row?;
            values
                .iter()
                .all(|(_, cell)| cell.editable())
                .then(|| RowChange::InsertRow {
                    table: table.clone(),
                    values: values.to_vec(),
                })
        }
        RowChange::InsertRow { table, values } => {
            let key = keys
                .iter()
                .map(|name| {
                    values
                        .iter()
                        .find(|(n, cell)| n == name && !matches!(cell, Cell::Null))
                        .cloned()
                })
                .collect::<Option<Vec<_>>>()?;
            (!key.is_empty()).then(|| RowChange::DeleteRow {
                table: table.clone(),
                key,
            })
        }
    }
}

struct CellEditor {
    /// The value goes to every marked cell, not only this one.
    all_marked: bool,
    inline: bool,
    fresh: bool,
    tab: Uuid,
    row: usize,
    column: usize,
    value: String,
    null: bool,
}
struct ProfileExchange {
    id: Uuid,
    importing: bool,
    bytes: Vec<u8>,
    filename: String,
    passphrase: String,
    encrypted: bool,
    include_secrets: bool,
    preview: Option<ImportPreview>,
    selected: Vec<Uuid>,
    busy: bool,
}
struct ImportUi {
    tab: Uuid,
    headers: Vec<String>,
    rows: Vec<Vec<Cell>>,
    path: String,
}
pub struct StudioApp {
    runtime: tokio::runtime::Runtime,
    tx: mpsc::Sender<Message>,
    rx: mpsc::Receiver<Message>,
    ctx: egui::Context,
    vault: Option<Arc<Vault>>,
    profiles: ProfileSet,
    history: Vec<HistoryRecord>,
    databases: HashMap<Uuid, Database>,
    connect_requests: HashMap<Uuid, Uuid>,
    qa_connection: Option<Uuid>,
    tunnel_managers: HashMap<Uuid, TunnelManager>,
    tables: HashMap<Uuid, Vec<TableRef>>,
    tabs: Vec<QueryTab>,
    active: usize,
    connection_dialog: Option<ConnectionProfile>,
    connection_editor: EditorState,
    connection_probe_job: Option<tokio::task::AbortHandle>,
    show_preferences: bool,
    section: Section,
    cell_editor: Option<CellEditor>,
    builder: Option<builder_view::BuilderUi>,
    import: Option<ImportUi>,
    /// Rows of a tab to delete, waiting for confirmation.
    delete_confirm: Option<(Uuid, Vec<usize>)>,
    connection_delete: Option<Uuid>,
    /// What "Obiekty" lists, and what is selected in it.
    objects: objects_view::ObjectsState,
    /// Tables and views of a connection to drop, waiting for confirmation.
    drop_confirm: Option<(Uuid, Vec<TableRef>)>,
    /// The columns of one table, shown read-only.
    structure: Option<objects_view::StructureUi>,
    /// The list of keyboard shortcuts (F1) is open.
    show_shortcuts: bool,
    /// "Set a value" for the marked cells: the tab, the value, NULL.
    fill_cells: Option<(Uuid, String, bool)>,
    /// Changes of data that can be taken back, and taken back ones that can
    /// be made again: for each tab, pairs of (what to run, what undoes it).
    undo: Vec<(Uuid, Vec<(RowChange, RowChange)>)>,
    redo: Vec<(Uuid, Vec<(RowChange, RowChange)>)>,
    pending_changes: HashMap<Uuid, PendingChanges>,
    /// Waiting changes of a tab about to be written, shown for confirmation.
    save_confirm: Option<staged::SaveConfirm>,
    /// The value shown in "Podgląd wartości".
    value_view: Option<value_view::ValueView>,
    /// The values of an ENUM or SET field of a design being edited.
    design_values: Option<design_extras::ValuesEditor>,
    /// C and C++ structures bound to columns, from the vault.
    blob_layouts: Vec<value_view::BlobLayout>,
    /// Waiting changes of a tab sent to the database, kept until it answers.
    staged_backup: HashMap<Uuid, StagedBackup>,
    /// Open tabs of the table designer, and the one on screen.
    designs: Vec<design_view::DesignTab>,
    design_active: usize,
    /// What saving a design runs, waiting for confirmation: it removes or
    /// rebuilds part of the table.
    design_confirm: Option<(Uuid, design_view::SavePlan)>,
    /// "Go to record" (Mod+G): the tab and the number typed so far.
    goto_record: Option<(Uuid, String)>,
    /// The search bar of the SQL editor (Mod+F).
    find: Option<sql_view::FindBar>,
    export_format: Format,
    notice: Option<(String, bool)>,
    notice_time: Instant,
    history_search: String,
    /// The question "clear the history?" is on screen.
    history_clear_confirm: bool,
    /// The vault is removing the history.
    history_clearing: bool,
    group_draft: String,
    startup: Instant,
    qa: bool,
    qa_run: Option<Uuid>,
    qa_report: Option<PathBuf>,
    qa_screenshot_requested: bool,
    qa_completed: bool,
    qa_root: Option<PathBuf>,
    pending_save: Option<ProfileSet>,
    saving: bool,
    profile_exchange: Option<ProfileExchange>,
    connection_import_id: Option<Uuid>,
    new_row: Option<NewRowEditor>,
    local_lock: Option<Arc<LocalLock>>,
    lock_settings: LockSettings,
    locked: bool,
    lock_busy: bool,
    show_lock_settings: bool,
    unlock_password: String,
    unlock_code: String,
    enroll_password: String,
    enroll_confirm: String,
    enroll_totp: bool,
    enrollment: Option<Enrollment>,
    recovery_codes: Vec<String>,
    last_activity: Instant,
    jobs: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
    monitor_result: Option<QueryResult>,
    monitor_busy: bool,
    monitor_last: Instant,
    monitor_connection: Option<Uuid>,
    monitor_series: std::collections::VecDeque<MonitorSample>,
    qa_options: QaOptions,
    qa_view_applied: Option<Instant>,
    qa_rows: usize,
    /// QA only: what the last copy would have put on the clipboard. A QA
    /// run never touches the clipboard of the person at the computer.
    qa_clipboard: Option<String>,
    /// QA only: progress through the scripted pointer and key input.
    qa_script: qa::ScriptProgress,
    updater: Option<Arc<Updater>>,
    update_offer: Option<UpdateOffer>,
    update_message: Option<String>,
    show_distribution: bool,
    show_about: bool,
    distribution_busy: bool,
    /// Waiting for the user to trust or refuse a changed SSH server key.
    host_key_change: Option<(Uuid, HostKeyScan)>,
    qr_texture: Option<egui::TextureHandle>,
    qa_auto: bool,
    nav_search: String,
    navigation_selection: Option<NodeIdentity>,
    show_explorer: bool,
    focus_mode: bool,
    editor_ratio: f32,
    suggestions: Option<(Uuid, Vec<String>)>,
    library: Vec<SavedQuery>,
    save_query_name: Option<String>,
    preference_original: Option<Preferences>,
    preference_save: PreferencesSaveState,
    preference_section: u8,
    library_generation: u64,
    /// Result chosen in "Import i eksport", by tab id.
    transfer_choice: Option<Uuid>,
    profile_file_request: Uuid,
    group_generation: u64,
    group_store: Option<Arc<Groups>>,
    group_snapshot: GroupSnapshot,
    group_dialog: Option<GroupEditor>,
    group_remove: Option<Uuid>,
    group_busy: bool,
    group_loading: bool,
    connection_group_save: Option<(u64, Uuid)>,
    connection_form_group: Option<(Uuid, Option<Uuid>)>,
    /// A copy of a connection was opened without the SSH server key of the
    /// original: say so over the editor.
    copy_key_notice: bool,
    /// Code of the failure that kept the vault from opening at start.
    startup_error: Option<String>,
    /// QA only: show the "opening the vault" screen although the vault is open.
    qa_startup: bool,
}
impl StudioApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        qa: bool,
        qa_auto: bool,
        qa_report: Option<PathBuf>,
        qa_run: Option<Uuid>,
        qa_options: QaOptions,
    ) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("native runtime");
        let (tx, rx) = mpsc::channel(32);
        let ctx = cc.egui_ctx.clone();
        crate::theme::install_fonts(&ctx);
        crate::theme::apply_preferences(&ctx, &Preferences::default());
        let qa_root = if qa {
            Some(std::env::temp_dir().join(format!(
                "BazaStudio-native-render-QA-{}",
                qa_run.expect("owned QA run")
            )))
        } else {
            None
        };
        let app = Self {
            runtime,
            tx,
            rx,
            ctx,
            vault: None,
            profiles: ProfileSet::default(),
            history: vec![],
            databases: HashMap::new(),
            connect_requests: HashMap::new(),
            qa_connection: None,
            tunnel_managers: HashMap::new(),
            tables: HashMap::new(),
            tabs: vec![QueryTab::new(None)],
            active: 0,
            connection_dialog: None,
            connection_editor: EditorState::default(),
            connection_probe_job: None,
            show_preferences: false,
            section: Section::Sql,
            cell_editor: None,
            builder: None,
            import: None,
            delete_confirm: None,
            connection_delete: None,
            objects: Default::default(),
            drop_confirm: None,
            structure: None,
            show_shortcuts: false,
            fill_cells: None,
            undo: Vec::new(),
            redo: Vec::new(),
            pending_changes: HashMap::new(),
            save_confirm: None,
            value_view: None,
            design_values: None,
            blob_layouts: Vec::new(),
            staged_backup: HashMap::new(),
            designs: Vec::new(),
            design_active: 0,
            design_confirm: None,
            goto_record: None,
            find: None,
            export_format: Format::Csv,
            notice: None,
            notice_time: Instant::now(),
            history_search: String::new(),
            history_clear_confirm: false,
            history_clearing: false,
            group_draft: String::new(),
            startup: Instant::now(),
            qa,
            qa_run,
            qa_report,
            qa_screenshot_requested: false,
            qa_completed: false,
            qa_root,
            pending_save: None,
            saving: false,
            profile_exchange: None,
            connection_import_id: None,
            new_row: None,
            local_lock: None,
            lock_settings: LockSettings {
                enabled: false,
                two_factor: false,
                idle_minutes: 15,
            },
            locked: false,
            lock_busy: false,
            show_lock_settings: false,
            unlock_password: String::new(),
            unlock_code: String::new(),
            enroll_password: String::new(),
            enroll_confirm: String::new(),
            enroll_totp: false,
            enrollment: None,
            recovery_codes: vec![],
            last_activity: Instant::now(),
            jobs: Arc::new(Mutex::new(vec![])),
            monitor_result: None,
            monitor_busy: false,
            monitor_last: Instant::now() - Duration::from_secs(60),
            monitor_connection: None,
            monitor_series: std::collections::VecDeque::new(),
            qa_options,
            qa_view_applied: None,
            qa_rows: 0,
            qa_script: qa::ScriptProgress::default(),
            qa_clipboard: None,
            updater: None,
            update_offer: None,
            update_message: None,
            show_distribution: false,
            show_about: false,
            distribution_busy: false,
            host_key_change: None,
            qr_texture: None,
            qa_auto,
            nav_search: String::new(),
            navigation_selection: None,
            show_explorer: true,
            focus_mode: false,
            editor_ratio: 0.43,
            suggestions: None,
            library: vec![],
            save_query_name: None,
            preference_original: None,
            preference_save: PreferencesSaveState::default(),
            preference_section: 0,
            library_generation: 0,
            transfer_choice: None,
            profile_file_request: Uuid::new_v4(),
            group_generation: 0,
            group_store: None,
            group_snapshot: GroupSnapshot::default(),
            group_dialog: None,
            group_remove: None,
            group_busy: false,
            group_loading: false,
            connection_group_save: None,
            connection_form_group: None,
            copy_key_notice: false,
            startup_error: None,
            qa_startup: false,
        };
        app.load_vault();
        app
    }
    /// Open the vault and read the profiles in the background. The answer
    /// arrives as [`Message::Loaded`]; a failure can be retried.
    fn load_vault(&self) {
        let fixture_root = self.qa_root.clone();
        let qa_run = self.qa_run;
        let t = self.tx.clone();
        let c = self.ctx.clone();
        self.runtime.spawn(async move {
            let r = tokio::task::spawn_blocking(move || {
                let v = Arc::new(if let Some(root) = &fixture_root {
                    {
                        std::fs::create_dir_all(root)?;
                        std::fs::write(
                            root.join("owned-qa.marker"),
                            qa_run.expect("owned QA run").to_string(),
                        )?;
                        Vault::open_owned(
                            &root.join("vault"),
                            &format!("BazaStudio.QA.{}", Uuid::new_v4()),
                        )?
                    }
                } else {
                    Vault::open_default("BazaStudio")?
                });
                if let Some(root) = &fixture_root {
                    // QA only: the watchdog of a self-test removes these when
                    // the run cannot end by itself.
                    qa::owned(&v, root);
                }
                let local_lock = Arc::new(LocalLock::open(v.clone())?);
                let lock_settings = local_lock.status()?;
                let mut p = if lock_settings.enabled {
                    ProfileSet::default()
                } else {
                    v.load_profiles()?
                };
                if let Some(root) = fixture_root {
                    let path = root.join("demonstration.sqlite");
                    std::fs::File::create(&path)?;
                    p.connections.push(ConnectionProfile {
                        name: "Local workspace".into(),
                        engine: baza_core::drivers::sqlite::ENGINE,
                        sqlite_path: Some(path),
                        ..ConnectionProfile::default()
                    });
                    // QA only: connections to real servers named by the
                    // person who runs the self-test.
                    p.connections.extend(qa::extra_connections()?);
                    v.save_profiles(&p)?;
                }
                let h = if lock_settings.enabled {
                    vec![]
                } else {
                    v.load_history()?
                };
                Ok::<_, anyhow::Error>((v, p, h, local_lock, lock_settings))
            })
            .await
            .map_err(|_| "vault_task_failed".to_owned())
            .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = t.send(Message::Loaded(r)).await;
            c.request_repaint();
        });
    }
    /// Try to open the vault again after a failed start.
    fn retry_startup(&mut self) {
        self.startup_error = None;
        self.load_vault();
    }
    fn tr<'a>(&self, pl: &'a str, en: &'a str) -> &'a str {
        if self.profiles.preferences.language == "en" {
            en
        } else {
            pl
        }
    }
    fn en(&self) -> bool {
        self.profiles.preferences.language == "en"
    }
    fn palette(&self) -> theme::Palette {
        theme::palette(&self.profiles.preferences)
    }
    /// Put text on the clipboard; in a QA run keep it for the report instead.
    fn copy_out(&mut self, ctx: &egui::Context, text: String) {
        if self.qa {
            self.qa_clipboard = Some(text);
        } else {
            ctx.copy_text(text);
        }
    }
    fn notify(&mut self, msg: String, error: bool) {
        let msg = if error { self.explain(msg) } else { msg };
        self.notice = Some((msg, error));
        self.notice_time = Instant::now();
    }
    /// Sentence for an internal result code. Codes are never shown as they are.
    fn explain(&self, code: String) -> String {
        crate::messages::describe(&code, self.en())
    }
    /// Connection in focus: the one selected in the side panel, otherwise the
    /// one of the tab on screen.
    fn focus_connection(&self) -> Option<Uuid> {
        self.navigation_selection
            .as_ref()
            .and_then(NodeIdentity::connection)
            .or(self.tabs[self.active].connection)
            .filter(|id| self.profiles.connections.iter().any(|p| p.id == *id))
    }
    /// Connection the content on screen belongs to, for the title and the
    /// status bar. The side panel may have another connection selected (to
    /// connect it, edit it or look at its profile); that selection does not
    /// change whose rows a query, a table or the builder is showing.
    fn shown_connection(&self) -> Option<Uuid> {
        let of_kind = |kind: TabKind| {
            let active = &self.tabs[self.active];
            if active.kind == kind {
                return active.connection;
            }
            self.tabs
                .iter()
                .rfind(|tab| tab.kind == kind)
                .and_then(|tab| tab.connection)
        };
        let shown = match self.section {
            // These sections describe the selected connection itself.
            Section::Connections | Section::Monitor => return self.focus_connection(),
            Section::Objects => self.objects.target.as_ref().map(|t| t.connection),
            Section::Design => self.designs.get(self.design_active).map(|d| d.connection),
            Section::Table => of_kind(TabKind::Table),
            Section::Builder => self.builder.as_ref().map(|builder| builder.connection),
            Section::Transfer => self
                .transfer_choice
                .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
                .map_or(self.tabs[self.active].connection, |tab| tab.connection),
            Section::Sql | Section::History => self.tabs[self.active].connection,
        };
        shown
            .filter(|id| self.profiles.connections.iter().any(|p| p.id == *id))
            // Nothing is open in this section yet: the selection is all
            // there is to go by.
            .or_else(|| self.focus_connection())
    }
    fn connection_name(&self, id: Uuid) -> Option<&str> {
        self.profiles
            .connections
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.as_str())
    }
    /// What the interface may offer for a connection: what its open database
    /// says, otherwise what its saved profile says. Without a connection
    /// nothing is held back; the action then asks for one.
    fn capabilities(&self, connection: Option<Uuid>) -> &'static Capabilities {
        connection
            .and_then(|id| {
                self.databases
                    .get(&id)
                    .map(Database::capabilities)
                    .or_else(|| {
                        self.profiles
                            .connections
                            .iter()
                            .find(|p| p.id == id)
                            .map(|p| p.capabilities())
                    })
            })
            .unwrap_or(&Capabilities::SQL)
    }
    /// Name of the database product of a connection, for a sentence.
    fn product_name(&self, connection: Option<Uuid>) -> &'static str {
        connection
            .and_then(|id| self.profiles.connections.iter().find(|p| p.id == id))
            .map_or("", |p| p.product_label())
    }
    /// Sentence that says an action is not offered for the database product
    /// of a connection.
    fn unavailable(&self, connection: Option<Uuid>, pl: &str, en: &str) -> String {
        let product = self.product_name(connection);
        if self.en() {
            format!("{product}: {en}")
        } else {
            format!("{product}: {pl}")
        }
    }
    /// Why a section cannot be opened for the connection in focus; `None`
    /// when it can. The section strip, the menus and the shortcuts ask here.
    fn section_blocked(&self, section: Section) -> Option<String> {
        let connection = self.focus_connection();
        let caps = self.capabilities(connection);
        let (blocked, pl, en) = match section {
            Section::Sql => (
                caps.language == QueryLanguage::None,
                "ta baza nie przyjmuje zapytań, dane można tylko przeglądać.",
                "this database takes no queries; its data can only be browsed.",
            ),
            Section::Builder => (
                !caps.query_builder,
                "kreator zapytań nie jest dostępny dla tej bazy.",
                "the query builder is not available for this database.",
            ),
            Section::Transfer => (
                !caps.import && !caps.export,
                "import i eksport nie są dostępne dla tej bazy.",
                "import and export are not available for this database.",
            ),
            Section::Monitor => (
                !caps.monitor,
                "ta baza nie udostępnia statystyk serwera.",
                "this database offers no server statistics.",
            ),
            Section::Objects
            | Section::Design
            | Section::Table
            | Section::History
            | Section::Connections => {
                return None;
            }
        };
        blocked.then(|| self.unavailable(connection, pl, en))
    }
    /// What the list of a connection is refreshed as.
    fn refresh_label(&self, connection: Uuid) -> &'static str {
        match self.capabilities(Some(connection)).browse {
            BrowseModel::Tables => self.tr("Odśwież tabele i widoki", "Refresh tables and views"),
            BrowseModel::Documents => self.tr("Odśwież kolekcje", "Refresh collections"),
            BrowseModel::Keys => self.tr("Odśwież klucze", "Refresh keys"),
        }
    }
    /// Name of a section. The first one is a command console for a database
    /// whose language is not SQL.
    fn section_label(&self, section: Section) -> &'static str {
        let en = self.en();
        if section == Section::Sql
            && self.capabilities(self.focus_connection()).language == QueryLanguage::Command
        {
            return if en { "Console" } else { "Konsola" };
        }
        section.label(en)
    }
    fn save(&mut self) {
        if let Some(previous) = self.pending_save.replace(self.profiles.clone()) {
            drop(SecretProfiles(previous));
        }
        self.begin_save();
    }
    fn replace_profiles(&mut self, next: ProfileSet) {
        drop(connection_edit::replace_profiles(&mut self.profiles, next));
    }
    fn begin_save(&mut self) {
        if self.saving || self.preference_save.busy() {
            return;
        }
        let Some(v) = self.vault.clone() else {
            return;
        };
        let Some(p) = self.pending_save.take() else {
            return;
        };
        let p = SecretProfiles(p);
        self.saving = true;
        let t = self.tx.clone();
        let c = self.ctx.clone();
        self.track(async move {
            let r = tokio::task::spawn_blocking(move || v.save_profiles(&p.0))
                .await
                .map_err(|_| "vault_task_failed".to_owned())
                .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = t.send(Message::Saved(r)).await;
            c.request_repaint();
        });
    }

    fn save_preferences(&mut self, open_security: bool) {
        if self.preference_save.busy() {
            return;
        }
        if self.saving || self.pending_save.is_some() {
            self.preference_save.error = Some(self.tr(
                "Poczekaj na zakończenie bieżącego zapisu i ponów próbę. Twój formularz zachowano.",
                "Wait for the current save to finish and retry. Your draft is preserved.",
            ).into());
            return;
        }
        let Some(vault) = self.vault.clone() else {
            self.preference_save.error = Some(self.tr(
                "Sejf jest niedostępny. Twój formularz zachowano; ponów zapis po jego otwarciu.",
                "The vault is unavailable. Your draft is preserved; retry after opening it.",
            ).into());
            return;
        };
        let Some(request) = self.preference_save.begin(open_security) else {
            return;
        };
        self.saving = true;
        let draft = self.profiles.preferences.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let result = tokio::task::spawn_blocking(move || persist_preferences(&vault, draft))
                .await
                .map_err(|_| "vault_task_failed".to_owned())
                .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = tx.send(Message::PreferencesSaved(request, result)).await;
            ctx.request_repaint();
        });
    }

    fn connect(&mut self, id: Uuid) {
        if self.vault.is_none() {
            self.notify("vault_unavailable".into(), true);
            return;
        }
        if self.databases.contains_key(&id) || self.connect_requests.contains_key(&id) {
            return;
        }
        let Some(p) = self
            .profiles
            .connections
            .iter()
            .find(|p| p.id == id)
            .cloned()
        else {
            return;
        };
        let qa = self.qa
            && self.qa_connection == Some(p.id)
            && p.name == "Local workspace"
            && p.engine == baza_core::drivers::sqlite::ENGINE
            && p.ssh.is_none()
            && self.qa_root.as_ref().is_some_and(|root| {
                p.sqlite_path.as_ref() == Some(&root.join("demonstration.sqlite"))
            });
        let request = Uuid::new_v4();
        self.connect_requests.insert(id, request);
        let t = self.tx.clone();
        let c = self.ctx.clone();
        self.notify(
            format!("{}: {}", self.tr("Łączenie", "Connecting"), p.name),
            false,
        );
        self.track(async move {
            let r = async {
                let opened = connection_edit::open(&p).await?;
                if let Some(learned) = opened.learned_key.clone() {
                    let _ = t.send(Message::HostKeyLearned(p.id, learned)).await;
                }
                let prepared = async {
                    if qa {
                        opened.database.query("CREATE TABLE IF NOT EXISTS customers(id INTEGER PRIMARY KEY,name TEXT,email TEXT,status TEXT,revenue TEXT); WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<2000) INSERT OR IGNORE INTO customers SELECT n, 'Customer '||n, 'customer'||n||'@example.test', CASE WHEN n%3=0 THEN 'pending' ELSE 'active' END, printf('%.2f',n*19.99) FROM numbers; CREATE TABLE IF NOT EXISTS orders(id INTEGER PRIMARY KEY,customer_id INTEGER,total TEXT,placed TEXT); WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n<300) INSERT OR IGNORE INTO orders SELECT n, 1+(n*7)%2000, printf('%.2f',n*3.5), date('2026-01-01', '+'||(n%280)||' days') FROM numbers; CREATE TABLE IF NOT EXISTS products(id INTEGER PRIMARY KEY,name TEXT,price TEXT); INSERT OR IGNORE INTO products VALUES(1,'Notebook','12.50'),(2,'Pen','2.10'),(3,'Backpack','89.00'); CREATE VIEW IF NOT EXISTS active_customers AS SELECT id,name,email FROM customers WHERE status='active';",CancellationToken::new()).await?;
                    }
                    opened.database.tables().await
                }
                .await;
                match prepared {
                    Ok(tables) => Ok((opened.database, tables, opened.tunnel)),
                    Err(error) => {
                        // Nothing stays open behind a connection that failed.
                        let reason =
                            connection_edit::tunnel_problem(opened.tunnel.as_ref(), p.id);
                        opened.close().await;
                        Err(reason.unwrap_or(error))
                    }
                }
            }
            .await;
            if let Some(changed) = r.as_ref().err().and_then(connection_edit::changed_key) {
                let _ = t.send(Message::HostKeyChanged(id, changed)).await;
            }
            let r = r.map_err(|e: anyhow::Error| e.to_string());
            let _ = t.send(Message::Connected(id, request, r)).await;
            c.request_repaint();
        });
    }
    fn disconnect(&mut self, id: Uuid) {
        if self
            .navigation_selection
            .as_ref()
            .and_then(NodeIdentity::connection)
            == Some(id)
        {
            self.navigation_selection = None;
        }
        self.connect_requests.remove(&id);
        self.tables.remove(&id);
        let db = self.databases.remove(&id);
        let manager = self.tunnel_managers.remove(&id);
        for tab in &mut self.tabs {
            if tab.connection == Some(id) {
                tab.cancel.cancel();
                tab.request = Uuid::new_v4();
                tab.busy = false;
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
        }
        if self.cell_editor.as_ref().is_some_and(|editor| {
            self.tabs
                .iter()
                .any(|tab| tab.id == editor.tab && tab.connection == Some(id))
        }) {
            self.cell_editor = None;
        }
        self.track(async move {
            if let Some(db) = db {
                db.close().await;
            }
            if let Some(manager) = manager {
                manager.shutdown().await;
            }
        });
        self.notify(self.tr("Rozłączono.", "Disconnected.").into(), false);
    }
    /// Open a new query in the SQL editor.
    fn new_tab(&mut self, c: Option<Uuid>) {
        if self.capabilities(c).language == QueryLanguage::None {
            let text = self.unavailable(
                c,
                "ta baza nie przyjmuje zapytań, dane można tylko przeglądać.",
                "this database takes no queries; its data can only be browsed.",
            );
            self.notify(text, true);
            return;
        }
        let mut tab = QueryTab::new(c);
        // "SQL", "SQL 2", "SQL 3": the open queries stay apart in their list.
        let open = self
            .tabs
            .iter()
            .filter(|tab| tab.kind == TabKind::Query)
            .count();
        if open > 0 {
            tab.title = format!("SQL {}", open + 1);
        }
        if self.capabilities(c).language == QueryLanguage::Command {
            // A command console starts empty: the sample statement is SQL.
            tab.sql.clear();
            tab.title = self.tr("Konsola", "Console").to_owned();
            if open > 0 {
                tab.title = format!("{} {}", tab.title, open + 1);
            }
        }
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.section = Section::Sql;
    }
    /// Show the rows of a table or view in "Dane tabeli". A table that is
    /// already open is brought forward and reloaded instead of opened twice.
    fn open_table(&mut self, c: Uuid, table: TableRef) {
        let Some(db) = self.databases.get(&c).cloned() else {
            self.connect(c);
            return;
        };
        self.section = Section::Table;
        if let Some(index) = self.tabs.iter().position(|tab| {
            tab.kind == TabKind::Table
                && tab.connection == Some(c)
                && tab
                    .source
                    .as_ref()
                    .is_some_and(|open| open.schema == table.schema && open.name == table.name)
        }) {
            self.active = index;
            self.run_active(false);
            return;
        }
        let mut request = PageRequest::first(table.clone());
        request.limit = self.profiles.preferences.page_limit();
        let Ok(text) = db.page_text(&request) else {
            self.notify("identifier_invalid".into(), true);
            return;
        };
        let mut tab = QueryTab::new(Some(c));
        tab.kind = TabKind::Table;
        tab.title = table.name.clone();
        tab.table = Some(table.clone());
        tab.source = Some(table);
        tab.sql = text;
        tab.page_request = Some(request);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.run_active(false);
    }
    /// Bring a section forward and make the tab it shows the active one.
    fn set_section(&mut self, section: Section) {
        if let Some(reason) = self.section_blocked(section) {
            self.notify(reason, true);
            return;
        }
        self.section = section;
        let kind = match section {
            Section::Sql => TabKind::Query,
            Section::Table => TabKind::Table,
            Section::Builder => TabKind::Builder,
            _ => return,
        };
        if self.tabs[self.active].kind == kind {
            return;
        }
        // The most recently opened tab of that kind.
        if let Some(index) = self.tabs.iter().rposition(|tab| tab.kind == kind) {
            self.active = index;
        } else if kind == TabKind::Query {
            let connection = self.tabs[self.active].connection;
            self.new_tab(connection);
        }
    }
    /// Close a tab. The last query stays open, so the editor is never empty.
    fn close_tab(&mut self, id: Uuid) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let kind = self.tabs[index].kind;
        let same_kind = self.tabs.iter().filter(|tab| tab.kind == kind).count();
        if kind == TabKind::Query && same_kind <= 1 {
            return;
        }
        if self.cell_editor.as_ref().is_some_and(|e| e.tab == id) {
            self.cell_editor = None;
        }
        let active = self.tabs[self.active].id;
        let mut closed = self.tabs.remove(index);
        closed.cancel.cancel();
        closed.sql.zeroize();
        if let Some(result) = &mut closed.result {
            for row in &mut result.rows {
                for cell in row {
                    wipe_cell(cell);
                }
            }
        }
        self.active = self
            .tabs
            .iter()
            .position(|tab| tab.id == active)
            .or_else(|| self.tabs.iter().rposition(|tab| tab.kind == kind))
            .or_else(|| self.tabs.iter().rposition(|tab| tab.kind == TabKind::Query))
            .unwrap_or(0);
    }
    /// Run the SQL of the tab with this id, whichever tab is on screen.
    fn run_tab(&mut self, id: Uuid, selection: bool) {
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            let old = self.active;
            self.active = index;
            self.run_active(selection);
            self.active = old.min(self.tabs.len() - 1);
        }
    }
    fn run_active(&mut self, selection: bool) {
        self.cell_editor = None;
        if self.vault.is_none() {
            self.notify("vault_unavailable".into(), true);
            return;
        }
        let tab = &mut self.tabs[self.active];
        if tab.busy {
            return;
        }
        let Some(id) = tab.connection else {
            self.notify(
                self.tr(
                    "Wybierz połączenie w pasku zapytania.",
                    "Choose a connection in the query toolbar.",
                )
                .into(),
                true,
            );
            return;
        };
        let Some(db) = self.databases.get(&id).cloned() else {
            self.connect(id);
            return;
        };
        let statement = if selection {
            tab.selection
                .as_ref()
                .filter(|r| !r.is_empty())
                .map(|r| {
                    tab.sql
                        .chars()
                        .skip(r.start)
                        .take(r.end - r.start)
                        .collect::<String>()
                })
                .unwrap_or_else(|| tab.sql.clone())
        } else {
            tab.sql.clone()
        };
        if statement.trim().is_empty() {
            return;
        }
        let known = self.tables.get(&id).map(Vec::as_slice).unwrap_or(&[]);
        // A table tab reads the page it describes; its records can be changed
        // when the container is a table the connection still lists. Anything
        // typed is a statement, and SQL says by itself whether it is a plain
        // `SELECT *` of one table.
        let page = tab
            .page_request
            .clone()
            .filter(|_| tab.kind == TabKind::Table && !selection);
        let inferred = match &page {
            Some(request) => known
                .iter()
                .find(|table| {
                    table.schema == request.container.schema
                        && table.name == request.container.name
                        && table.kind.contains("table")
                })
                .cloned(),
            None if db.capabilities().language == QueryLanguage::Sql => {
                sql::editable_table(db.dialect(), &statement, known)
            }
            None => None,
        };
        // The columns of the same table are not asked for again with each
        // page; the designer clears them when it changes the table.
        let cached_columns = tab
            .table
            .as_ref()
            .zip(inferred.as_ref())
            .filter(|(before, now)| before.schema == now.schema && before.name == now.name)
            .filter(|_| !tab.columns.is_empty())
            .map(|_| tab.columns.clone());
        let reading_page = page.is_some();
        tab.table = None;
        tab.busy = true;
        tab.request = Uuid::new_v4();
        let request = tab.request;
        tab.error = None;
        tab.cancel = CancellationToken::new();
        let cancel = tab.cancel.clone();
        let tabid = tab.id;
        let t = self.tx.clone();
        let ctx = self.ctx.clone();
        let v = self.vault.clone();
        let prefs = self.profiles.preferences.clone();
        let record = HistoryRecord {
            id: Uuid::new_v4(),
            connection_id: id,
            sql: sql::redact_sql(&statement),
            status: "running".into(),
            started_at: chrono::Utc::now().to_rfc3339(),
            duration_ms: 0,
            rows: 0,
            error: None,
        };
        self.track(async move {
            let columns = match (&inferred, cached_columns) {
                (Some(_), Some(columns)) => columns,
                (Some(table), None) => db.columns(table).await.unwrap_or_default(),
                (None, _) => vec![],
            };
            let begin = record.clone();
            let r = async {
                // A statement is written down before it runs; reading a page
                // of a table changes nothing and is written down once, after.
                if let Some(v) = v.clone().filter(|_| !reading_page) {
                    tokio::task::spawn_blocking(move || v.append_history(&begin))
                        .await
                        .map_err(|_| anyhow::anyhow!("history_task_failed"))??;
                }
                match &page {
                    Some(request) => db.read_page(request, cancel).await,
                    None => db.query(&statement, cancel).await,
                }
            }
            .await
            .map_err(|e| e.to_string());
            let mut done = record;
            match &r {
                Ok(q) => {
                    done.status = "completed".into();
                    done.duration_ms = q.duration_ms;
                    done.rows = q.rows.len() as u64;
                }
                Err(e) => {
                    done.status = if e == "query_cancelled" {
                        "cancelled"
                    } else {
                        "failed"
                    }
                    .into();
                    done.error = Some(e.clone());
                }
            }
            // The result goes to the screen first, then into the history.
            let h = done.clone();
            let _ = prefs;
            let _ = t
                .send(Message::Query(tabid, request, done, inferred, columns, r))
                .await;
            ctx.request_repaint();
            if let Some(v) = v {
                let _ = tokio::task::spawn_blocking(move || v.append_history(&h)).await;
            }
        });
    }
    fn reload_tab(&mut self, id: Uuid) {
        self.run_tab(id, false);
    }
    /// Remove the whole query history from the vault. The list on screen is
    /// emptied when the vault has confirmed it.
    fn clear_history(&mut self) {
        if self.history_clearing {
            return;
        }
        let Some(vault) = self.vault.clone() else {
            self.notify("vault_unavailable".into(), true);
            return;
        };
        self.history_clearing = true;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let result = tokio::task::spawn_blocking(move || vault.clear_history())
                .await
                .map_err(|_| "vault_task_failed".to_owned())
                .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = tx.send(Message::HistoryCleared(result)).await;
            ctx.request_repaint();
        });
    }
    fn messages(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            if self.locked
                && !matches!(
                    &m,
                    Message::Loaded(_) | Message::Unlocked(_) | Message::LockStopped(_)
                )
            {
                self.discard_locked(m);
                continue;
            }
            match m {
                Message::ConnectionSaved(editor, result) => {
                    self.saving = false;
                    if !self.connection_editor.matches(editor) {
                        if let Ok(mut set) = result {
                            profiles::wipe_secrets(&mut set);
                        }
                        continue;
                    }
                    self.connection_editor.busy = false;
                    self.connection_editor.saving = false;
                    match result {
                        Ok(next) => {
                            let id = self.connection_editor.profile;
                            self.replace_profiles(next);
                            if let Some(id) = id {
                                self.disconnect(id);
                            }
                            if let Some((id, group)) =
                                self.connection_form_group.filter(|(id, group)| {
                                    self.group_snapshot.memberships.get(id).copied() != *group
                                })
                            {
                                self.group_operation(GroupOperation::Membership(id, group));
                                if self.group_busy {
                                    self.connection_group_save =
                                        Some((self.group_generation, editor));
                                    self.connection_editor.busy = true;
                                    self.connection_editor.saving = true;
                                } else {
                                    self.connection_editor.feedback = Some((self.tr("Profil zapisano, lecz nie przypisano grupy. Wybór został zachowany; ponów zapis po odświeżeniu grup.", "Profile saved, but the group was not assigned. Your choice is preserved; retry after refreshing groups.").into(), true));
                                }
                            } else {
                                self.finish_connection_save();
                            }
                        }
                        Err(e) => {
                            self.connection_editor.feedback = Some((
                                format!(
                                    "{} {}",
                                    self.tr(
                                        "Nie zapisano połączenia. Formularz zachował dane.",
                                        "Connection was not saved. Your draft is preserved."
                                    ),
                                    self.explain(e.clone())
                                ),
                                true,
                            ));
                            if e == "vault_conflict_reload" {
                                self.reload_connection_base(editor);
                            }
                        }
                    }
                }
                Message::ConnectionBaseReload(editor, result) => {
                    if !self.connection_editor.matches(editor) {
                        if let Ok(mut set) = result {
                            profiles::wipe_secrets(&mut set);
                        }
                        continue;
                    }
                    self.connection_editor.busy = false;
                    match result {
                        Ok(next) => {
                            self.replace_profiles(next);
                            self.connection_editor.feedback=Some((self.tr("Lista zapisanych profili została odświeżona. Sprawdź swój formularz i ponów zapis.","Saved profiles were refreshed. Review your preserved draft before saving again.").into(),true));
                        }
                        Err(e) => self.connection_editor.feedback = Some((self.explain(e), true)),
                    }
                }
                Message::ConnectionTest(editor, fingerprint, result) => {
                    if !self.connection_editor.matches(editor) {
                        continue;
                    }
                    self.connection_probe_job = None;
                    self.connection_editor.busy = false;
                    let current = self
                        .connection_dialog
                        .as_ref()
                        .and_then(|p| connection_edit::fingerprint(p).ok());
                    if current != Some(fingerprint) {
                        self.connection_editor.feedback = None;
                        continue;
                    }
                    self.connection_editor.changed_key = None;
                    self.connection_editor.feedback = Some(match result {
                        Ok(learned) => {
                            // Keep the key trusted during the test, so saving
                            // the connection pins it.
                            let mut remembered = false;
                            if let Some(draft) = self.connection_dialog.as_mut() {
                                // A key is learned only by a test that ran
                                // without one, whatever the field still shows
                                // (the key of a server the draft has left).
                                if let (Some(ssh), Some(learned)) = (draft.ssh.as_mut(), learned) {
                                    ssh.fingerprint = learned;
                                    remembered = true;
                                }
                                self.connection_editor.fingerprint =
                                    connection_edit::fingerprint(draft).ok();
                            }
                            let mut text = self
                                .tr(
                                    "Połączenie przetestowane — uwierzytelnienie i zapytanie działają.",
                                    "Connection tested — authentication and query succeeded.",
                                )
                                .to_owned();
                            if remembered {
                                // Say it: the field that holds the key is in
                                // the collapsed advanced part of the SSH tab.
                                text.push(' ');
                                text.push_str(self.tr(
                                    "Klucz serwera SSH został zapamiętany i będzie sprawdzany przy kolejnych połączeniach.",
                                    "The SSH server key was remembered and will be checked on later connections.",
                                ));
                            }
                            (text, false)
                        }
                        Err((e, changed)) => {
                            self.connection_editor.changed_key = changed;
                            (
                                format!(
                                    "{} {}",
                                    self.tr("Test nie powiódł się.", "Test failed."),
                                    self.explain(e)
                                ),
                                true,
                            )
                        }
                    });
                }

                Message::TablesLoaded(id, request, r) => {
                    if self.connect_requests.get(&id).copied() != Some(request) {
                        continue;
                    }
                    self.connect_requests.remove(&id);
                    match r {
                        Ok(tables) => {
                            self.tables.insert(id, tables);
                        }
                        Err(e) => self.notify(e, true),
                    }
                }
                Message::Loaded(r) => match r {
                    Ok((v, p, h, lock, settings)) => {
                        self.local_lock = Some(lock);
                        self.locked = settings.enabled;
                        self.lock_settings = settings;
                        self.vault = Some(v);
                        self.replace_profiles(p);
                        self.qa_connection = self.qa_chosen_connection();
                        self.history = h;
                        if !self.locked {
                            self.load_library();
                            self.load_layouts();
                            self.load_groups();
                            if !self.qa {
                                self.init_distribution();
                            }
                        }
                        self.qa_preferences();
                        crate::theme::apply_preferences(&self.ctx, &self.profiles.preferences);
                        if let Some(id) = self.qa_connection {
                            self.connect(id);
                        }
                    }
                    // Shown as its own screen, with a way to try again.
                    Err(e) => self.startup_error = Some(e),
                },
                Message::Connected(id, request, r) => {
                    if self.connect_requests.get(&id).copied() != Some(request) {
                        self.discard_locked(Message::Connected(id, request, r));
                        continue;
                    }
                    self.connect_requests.remove(&id);
                    match r {
                        Ok((d, t, manager)) => {
                            self.databases.insert(id, d);
                            if let Some(m) = manager {
                                self.tunnel_managers.insert(id, m);
                            }
                            self.tables.insert(id, t);
                            if self.tabs[self.active].connection.is_none() {
                                self.tabs[self.active].connection = Some(id);
                            }
                            self.notify(self.tr("Połączono.", "Connected.").into(), false);
                            // QA only: the fixture shows its table at once. Any
                            // other connection behaves as in a normal launch.
                            if self.qa_connection == Some(id)
                                && let Some(table) = self.qa_chosen_table(id)
                            {
                                self.open_table(id, table);
                            }
                        }
                        Err(e) => self.notify(e, true),
                    }
                }
                Message::Query(id, request, h, table, columns, r) => {
                    if !self.tabs.iter().any(|t| t.id == id && t.request == request) {
                        self.history.retain(|v| v.id != h.id);
                        self.history.insert(0, h);
                        self.history.truncate(500);
                        if let Ok(mut result) = r {
                            for row in &mut result.rows {
                                for cell in row {
                                    wipe_cell(cell);
                                }
                            }
                        }
                        continue;
                    }
                    if self.cell_editor.as_ref().is_some_and(|e| e.tab == id) {
                        self.cell_editor = None;
                    }
                    if self
                        .delete_confirm
                        .as_ref()
                        .is_some_and(|(tab, _)| *tab == id)
                    {
                        self.delete_confirm = None;
                    }
                    let history_connection = h.connection_id;
                    self.history.retain(|v| v.id != h.id);
                    self.history.insert(0, h);
                    self.history.truncate(500);
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                        tab.busy = false;
                        match r {
                            Ok(mut v) => {
                                if tab.connection == Some(history_connection) {
                                    sql::name_empty_result(table.as_ref(), &columns, &mut v);
                                    tab.table = sql::editable_result_table(table, &columns, &v);
                                    tab.columns = columns;
                                } else {
                                    tab.table = None;
                                    tab.columns.clear();
                                }
                                tab.selected_row = None;
                                tab.selected_cell = None;
                                tab.pending.clear();
                                tab.added.clear();
                                tab.pending_log.clear();
                                tab.result = Some(v);
                            }
                            Err(e) => {
                                // A page with a typed condition the server
                                // refuses was most likely refused for it.
                                let typed = tab
                                    .page_request
                                    .as_ref()
                                    .is_some_and(|page| page.condition.is_some());
                                tab.error = Some(match e.strip_prefix("database_error") {
                                    Some(state) if typed => {
                                        format!("filter_condition_rejected{state}")
                                    }
                                    _ => e,
                                });
                            }
                        }
                    }
                }
                Message::Saved(r) => {
                    self.saving = false;
                    if let Err(e) = r {
                        self.notify(e, true)
                    }
                    self.begin_save();
                }
                Message::PreferencesSaved(request, result) => {
                    let completion = self.preference_save.complete(
                        request,
                        result,
                        &mut self.profiles,
                        &mut self.preference_original,
                        &mut self.show_preferences,
                    );
                    if completion == PreferencesSaveCompletion::Ignored {
                        continue;
                    }
                    self.saving = false;
                    if let PreferencesSaveCompletion::Saved { open_security } = completion {
                        self.notify(
                            self.tr("Preferencje zapisano.", "Preferences saved.")
                                .into(),
                            false,
                        );
                        if open_security {
                            self.show_lock_settings = true;
                        }
                    }
                    theme::apply_preferences(&self.ctx, &self.profiles.preferences);
                    self.begin_save();
                }
                Message::FileSql(id, r) => {
                    if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == id) {
                        match r {
                            Ok(s) => {
                                tab.sql = s;
                                tab.table = None;
                            }
                            Err(e) => tab.error = Some(e),
                        }
                    }
                }
                Message::GroupsLoaded(generation, r) => {
                    if generation != self.group_generation {
                        continue;
                    }
                    self.group_loading = false;
                    match r {
                        Ok((store, snapshot)) => {
                            self.group_store = Some(store);
                            self.group_snapshot = snapshot;
                        }
                        Err(e) => self.notify(e, true),
                    }
                }
                Message::GroupSaved(generation, r) => {
                    if generation != self.group_generation {
                        continue;
                    }
                    self.group_busy = false;
                    let connection_editor = self
                        .connection_group_save
                        .take()
                        .filter(|(request, editor)| {
                            *request == generation && self.connection_editor.matches(*editor)
                        })
                        .map(|(_, editor)| editor);
                    match r {
                        Ok(snapshot) => {
                            self.group_snapshot = snapshot;
                            self.group_dialog = None;
                            self.group_remove = None;
                            if connection_editor.is_some() {
                                self.finish_connection_save();
                            }
                        }
                        Err(e) => {
                            if connection_editor.is_some() {
                                self.connection_editor.busy = false;
                                self.connection_editor.saving = false;
                                self.connection_editor.feedback = Some((
                                    format!(
                                        "{} {}",
                                        self.tr(
                                            "Profil zapisano, lecz nie przypisano grupy. Formularz zachował wybór; ponów zapis.",
                                            "Profile saved, but its group was not assigned. Your draft choice is preserved; retry saving."
                                        ),
                                        self.explain(e.clone())
                                    ),
                                    true,
                                ));
                            } else {
                                self.notify(e.clone(), true);
                            }
                            if e == "group_revision_changed" {
                                self.load_groups();
                            }
                        }
                    }
                }
                Message::Layouts(r) => match r {
                    Ok(rows) => self.blob_layouts = rows,
                    Err(e) => self.notify(e, true),
                },
                Message::Library(generation, mutation, r) => {
                    if generation == self.library_generation {
                        match r {
                            Ok(rows) => self.library = rows,
                            Err(e) => self.notify(e, true),
                        }
                    } else if mutation {
                        if let Err(e) = r {
                            self.notify(e, true);
                        }
                        self.load_library();
                    }
                }
                Message::UpdateFileChecked(r) => {
                    self.distribution_busy = false;
                    let en = self.profiles.preferences.language == "en";
                    match r {
                        Ok(Some(true)) => {
                            self.update_message = Some(
                                self.tr(
                                    "Plik zgadza się z podpisanym rozmiarem i sumą SHA256.",
                                    "The file matches the signed size and SHA256.",
                                )
                                .into(),
                            )
                        }
                        Ok(Some(false)) => {
                            self.update_message = Some(
                                self.tr(
                                    "Plik NIE zgadza się z podpisaną sumą SHA256. Nie instaluj go i pobierz pakiet ponownie.",
                                    "The file does NOT match the signed SHA256. Do not install it; download the package again.",
                                )
                                .into(),
                            )
                        }
                        Ok(None) => {}
                        Err(code) => {
                            self.update_message = Some(updater::describe(&code, en).into())
                        }
                    }
                }
                Message::HistoryCleared(r) => {
                    self.history_clearing = false;
                    match r {
                        Ok(_) => {
                            for record in &mut self.history {
                                record.sql.zeroize();
                                if let Some(error) = &mut record.error {
                                    error.zeroize();
                                }
                            }
                            self.history.clear();
                            self.history_search.clear();
                            self.notify(
                                self.tr("Historia wyczyszczona.", "History cleared.").into(),
                                false,
                            );
                        }
                        Err(e) => self.notify(e, true),
                    }
                }
                Message::HostKeyLearned(id, fingerprint) => {
                    let unpinned = self
                        .profiles
                        .connections
                        .iter_mut()
                        .find(|p| p.id == id)
                        .and_then(|p| p.ssh.as_mut())
                        .filter(|ssh| ssh.fingerprint.is_empty());
                    if let Some(ssh) = unpinned {
                        ssh.fingerprint = fingerprint.clone();
                        self.save();
                        // An editor of this connection that is open meanwhile
                        // shows the key as well; saving it keeps the key in
                        // any case (`connection_edit::server_key_to_save`).
                        let saved = self
                            .profiles
                            .connections
                            .iter()
                            .find(|p| p.id == id)
                            .and_then(|p| p.ssh.as_ref());
                        if let (Some(saved), Some(draft)) = (
                            saved,
                            self.connection_dialog.as_mut().filter(|d| d.id == id),
                        ) && self.connection_editor.profile == Some(id)
                        {
                            self.connection_editor
                                .key_learned(saved, draft, &fingerprint);
                        }
                    }
                }
                Message::HostKeyChanged(id, key) => {
                    self.host_key_change = Some((id, key));
                }
                Message::HostKey(editor, r) => {
                    if !self.connection_editor.matches(editor) {
                        continue;
                    }
                    let en = self.profiles.preferences.language == "en";
                    self.connection_editor.scanning = false;
                    if let Some(ssh) = self
                        .connection_dialog
                        .as_mut()
                        .and_then(|profile| profile.ssh.as_mut())
                    {
                        self.connection_editor.scan_note =
                            Some(connection_edit::apply_scan(ssh, r, en));
                    }
                }
                Message::Notice(r) | Message::Exported(r) => match r {
                    Ok(s) => self.notify(s, false),
                    Err(e) => self.notify(e, true),
                },
                Message::Mutation(id, r) => {
                    let shown = self.settle_changes(id, usize::from(r.is_ok()));
                    match r {
                        Ok(()) if shown => {}
                        Ok(()) => self.reload_tab(id),
                        Err(e) => {
                            self.notify(e, true);
                            self.reload_tab(id);
                        }
                    }
                }
                Message::Mutations(id, done, error) => {
                    let shown = self.settle_changes(id, done);
                    // Refused at once: nothing was written and what was typed
                    // stays in the grid to be put right.
                    if error.is_some() && done == 0 && self.restore_staged(id) {
                        if let Some(e) = error {
                            self.notify(e, true);
                        }
                        continue;
                    }
                    self.staged_backup.remove(&id);
                    match error {
                        Some(e) => self.notify(e, true),
                        None if done > 1 => {
                            let text = if self.en() {
                                format!("Changed records: {done}.")
                            } else {
                                format!("Zmienione rekordy: {done}.")
                            };
                            self.notify(text, false);
                        }
                        None => {}
                    }
                    // Even after a refusal some changes may have been written;
                    // without one the grid already shows the new values.
                    if !shown {
                        self.reload_tab(id);
                    }
                }
                Message::Dropped(connection, r) => {
                    match r {
                        Ok(count) => {
                            let text = if self.en() {
                                format!("Removed objects: {count}.")
                            } else {
                                format!("Usunięte obiekty: {count}.")
                            };
                            self.notify(text, false);
                        }
                        Err(e) => self.notify(e, true),
                    }
                    self.objects.selected.clear();
                    self.refresh_tables(Some(connection));
                }
                Message::RecordCount(id, r) => {
                    let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
                        continue;
                    };
                    match r {
                        Ok(count) => {
                            let limit = u64::from(self.page_limit(index));
                            let tab = &mut self.tabs[index];
                            tab.total = Some(count);
                            tab.page = count.saturating_sub(1) / limit.max(1);
                            self.browse(index);
                        }
                        Err(e) => self.notify(e, true),
                    }
                }
                Message::DesignColumns(request, r) => self.design_loaded(request, r),
                Message::DesignExtras(request, r) => self.design_extras_loaded(request, r),
                Message::DesignRefColumns(design, key, r) => {
                    self.ref_columns_loaded(design, key, r)
                }
                Message::DesignSaved(id, r) => self.design_saved(id, r),
                Message::Details(request, r) => {
                    if let Some(details) = self
                        .objects
                        .details
                        .as_mut()
                        .filter(|d| d.request == request)
                    {
                        details.result = Some(r);
                    }
                }
                Message::Structure(request, r) => {
                    if let Some(structure) =
                        self.structure.as_mut().filter(|s| s.request == request)
                    {
                        structure.columns = Some(r);
                    }
                }
                Message::DataPreview(id, r) => match r {
                    Ok((headers, rows, path)) => {
                        self.import = Some(ImportUi {
                            tab: id,
                            headers,
                            rows,
                            path,
                        });
                        // The preview is shown in place, in "Import i eksport".
                        self.section = Section::Transfer;
                    }
                    Err(e) => self.notify(e, true),
                },
                Message::ProfileFile(request, r) => {
                    if request != self.profile_file_request {
                        if let Ok((mut bytes, _)) = r {
                            bytes.zeroize();
                        }
                        continue;
                    }
                    match r {
                        Ok((bytes, filename)) => {
                            self.profile_exchange = Some(ProfileExchange {
                                id: Uuid::new_v4(),
                                importing: true,
                                bytes,
                                filename,
                                passphrase: String::new(),
                                encrypted: true,
                                include_secrets: false,
                                preview: None,
                                selected: vec![],
                                busy: false,
                            })
                        }
                        Err(e) => self.notify(e, true),
                    }
                }
                Message::ProfilePreview(request, r) => {
                    if let Some(state) = self.profile_exchange.as_mut().filter(|s| s.id == request)
                    {
                        state.busy = false;
                        match r {
                            Ok(p) => {
                                // Only database connections are offered; any
                                // other entry of the file is left out.
                                state.selected = p
                                    .rows
                                    .iter()
                                    .filter(|r| r.can_apply && r.kind == "database")
                                    .map(|r| r.id)
                                    .collect();
                                state.preview = Some(p);
                            }
                            Err(e) => self.notify(e, true),
                        }
                    }
                }
                Message::PrivateKey(id, editor, request, r) => {
                    if !self.connection_editor.accepts_key_result(editor, request) {
                        continue;
                    }
                    let en = self.en();
                    self.connection_editor.invalidate_key_load();
                    if let Some(p) = self.connection_dialog.as_mut().filter(|p| p.id == id)
                        && let Some(ssh) =
                            p.ssh.as_mut().filter(|ssh| ssh.auth == SshAuth::PrivateKey)
                    {
                        match r {
                            Ok(mut s) => {
                                if let Some(mut old) =
                                    ssh.private_key.replace(std::mem::take(&mut *s))
                                {
                                    old.zeroize();
                                }
                                self.connection_editor.feedback = None;
                                self.connection_editor.fingerprint = None;
                            }
                            Err(e) => {
                                self.connection_editor.feedback =
                                    Some((crate::messages::describe(&e, en), true))
                            }
                        }
                    }
                }
                Message::Unlocked(r) => {
                    self.lock_busy = false;
                    match r {
                        Ok((p, h)) => {
                            self.replace_profiles(p);
                            self.history = h;
                            self.locked = false;
                            self.last_activity = Instant::now();
                            self.unlock_password.zeroize();
                            self.unlock_code.zeroize();
                            self.load_library();
                            self.load_layouts();
                            self.load_groups();
                            if !self.qa {
                                self.init_distribution();
                            }
                            crate::theme::apply_preferences(&self.ctx, &self.profiles.preferences);
                        }
                        Err(e) => self.notify(e, true),
                    }
                }
                Message::Enrollment(r) => {
                    self.lock_busy = false;
                    match r {
                        Ok(e) => {
                            self.qr_texture = e
                                .otpauth_uri
                                .as_deref()
                                .and_then(|uri| qr_texture(&self.ctx, uri));
                            self.enrollment = Some(e)
                        }
                        Err(e) => self.notify(e, true),
                    }
                }
                Message::LockConfirmed(r) => {
                    self.lock_busy = false;
                    match r {
                        Ok((settings, codes)) => {
                            self.lock_settings = settings;
                            self.recovery_codes = codes;
                            self.enrollment = None;
                            self.qr_texture = None;
                            self.enroll_password.zeroize();
                            self.enroll_confirm.zeroize();
                            self.unlock_password.zeroize();
                            self.unlock_code.zeroize();
                        }
                        Err(e) => self.notify(e, true),
                    }
                }
                Message::LockStopped(r) => {
                    self.lock_busy = false;
                    if let Err(e) = r {
                        self.notify(e, true)
                    }
                }
                Message::Monitor(connection, r) => {
                    if self.monitor_connection != Some(connection) {
                        if let Ok(mut stale) = r {
                            for row in &mut stale.rows {
                                for cell in row {
                                    wipe_cell(cell);
                                }
                            }
                        }
                        continue;
                    }
                    self.monitor_busy = false;
                    match r {
                        Ok(v) => {
                            self.monitor_series.push_back(MonitorSample {
                                at: chrono::Local::now(),
                                response_ms: v.duration_ms as f64,
                            });
                            while self.monitor_series.len() > 120 {
                                self.monitor_series.pop_front();
                            }
                            self.monitor_result = Some(v);
                        }
                        Err(e) => self.notify(e, true),
                    }
                }
                Message::PathPicked(editor, target, path) => {
                    if !self.connection_editor.matches(editor) {
                        continue;
                    }
                    if let Some(draft) = self.connection_dialog.as_mut() {
                        match target {
                            connection_dialog::PathTarget::DatabaseFile => {
                                draft.sqlite_path = Some(path)
                            }
                            connection_dialog::PathTarget::CaCertificate => {
                                draft.ca_file = Some(path)
                            }
                        }
                    }
                }
                Message::BuilderColumns(connection, key, r) => {
                    self.builder_columns(connection, key, r)
                }
                Message::Distribution(r) => {
                    self.distribution_busy = false;
                    let en = self.profiles.preferences.language == "en";
                    match r {
                        Ok(updater) => self.updater = Some(updater),
                        // Not worth interrupting startup: the window explains it.
                        Err(code) => {
                            self.update_message = Some(updater::describe(&code, en).into())
                        }
                    }
                }
                Message::Update(r) => {
                    self.distribution_busy = false;
                    let en = self.profiles.preferences.language == "en";
                    self.update_offer = None;
                    self.update_message = Some(match r {
                        Ok(UpdateCheck::Available(offer)) => {
                            self.update_offer = Some(offer);
                            self.tr(
                                "Dostępna jest nowsza wersja.",
                                "A newer version is available.",
                            )
                            .into()
                        }
                        Ok(UpdateCheck::UpToDate) => self
                            .tr("Masz najnowszą wersję.", "You have the latest version.")
                            .into(),
                        Ok(UpdateCheck::Unreachable) => self
                            .tr(
                                "Nie udało się teraz sprawdzić aktualizacji. Spróbuj ponownie później.",
                                "Updates could not be checked right now. Try again later.",
                            )
                            .into(),
                        Err(code) => updater::describe(&code, en).into(),
                    });
                }
            }
        }
    }
    fn export_profiles_dialog(&mut self) {
        self.profile_file_request = Uuid::new_v4();
        self.profile_exchange = Some(ProfileExchange {
            id: Uuid::new_v4(),
            importing: false,
            bytes: vec![],
            filename: String::new(),
            passphrase: String::new(),
            encrypted: true,
            include_secrets: false,
            preview: None,
            selected: self.profiles.connections.iter().map(|p| p.id).collect(),
            busy: false,
        });
    }
    fn focus_editor(&self) {
        let id = egui::Id::new(("sql_editor", self.tabs[self.active].id));
        self.ctx.memory_mut(|m| m.request_focus(id));
    }
    fn editor_key(&self, key: egui::Key, shift: bool) {
        self.focus_editor();
        let mut modifiers = egui::Modifiers::COMMAND;
        modifiers.shift = shift;
        self.ctx.input_mut(|i| {
            i.events.push(egui::Event::Key {
                key,
                physical_key: Some(key),
                pressed: true,
                repeat: false,
                modifiers,
            })
        });
    }
    /// Open the table designer for a connected database.
    fn open_designer(&mut self, id: Uuid) {
        if !self.capabilities(Some(id)).table_designer {
            let text = self.unavailable(
                Some(id),
                "projektant tabeli nie jest dostępny dla tej bazy.",
                "the table designer is not available for this database.",
            );
            self.notify(text, true);
            return;
        }
        let Some(profile) = self.profiles.connections.iter().find(|p| p.id == id) else {
            return;
        };
        // The schema on screen in "Obiekty", else the one of the profile,
        // else the first one of the database.
        let listed = self
            .objects
            .target
            .as_ref()
            .filter(|t| t.connection == id)
            .map(|t| t.schema.clone());
        let configured = match profile.dialect().default_schema {
            DefaultSchema::Fixed(name) => name.to_owned(),
            DefaultSchema::Database => profile.database.clone(),
            DefaultSchema::None => String::new(),
        };
        let first = self
            .tables
            .get(&id)
            .and_then(|tables| tables.iter().map(|t| t.schema.clone()).min());
        let schema = listed
            .or_else(|| (!configured.is_empty()).then_some(configured))
            .or(first)
            .unwrap_or_default();
        self.new_design(id, schema);
    }
    /// Read the list of tables and views of a connected database again.
    fn refresh_tables(&mut self, id: Option<Uuid>) {
        if let Some(id) = id
            && let Some(db) = self.databases.get(&id).cloned()
        {
            let request = Uuid::new_v4();
            self.connect_requests.insert(id, request);
            let tx = self.tx.clone();
            let ctx = self.ctx.clone();
            self.track(async move {
                let result = db.tables().await.map_err(|e| e.to_string());
                let _ = tx.send(Message::TablesLoaded(id, request, result)).await;
                ctx.request_repaint();
            });
        }
    }
    fn navigator_queries(&self) -> Vec<QueryNode> {
        self.library
            .iter()
            .map(|query| QueryNode {
                id: query.id,
                name: query.name.clone(),
                connection: query.connection,
            })
            .collect()
    }
    fn validate_navigation_selection(&mut self) {
        let valid = self
            .navigation_selection
            .as_ref()
            .is_none_or(|node| match node {
                NodeIdentity::Group(id) => self
                    .group_snapshot
                    .groups
                    .iter()
                    .any(|group| group.id == *id),
                NodeIdentity::Connection(id) | NodeIdentity::Queries(id) => self
                    .profiles
                    .connections
                    .iter()
                    .any(|profile| profile.id == *id),
                NodeIdentity::Schema(id, schema) => self
                    .tables
                    .get(id)
                    .is_some_and(|tables| tables.iter().any(|table| table.schema == *schema)),
                NodeIdentity::Objects(id, schema, kind) => {
                    self.tables.get(id).is_some_and(|tables| {
                        tables.iter().any(|table| {
                            table.schema == *schema
                                && (matches!(kind, ObjectKind::Table | ObjectKind::View)
                                    || ObjectKind::from_metadata(&table.kind) == *kind)
                        })
                    })
                }
                NodeIdentity::Object(id, schema, name, kind) => {
                    self.tables.get(id).is_some_and(|tables| {
                        tables.iter().any(|table| {
                            table.schema == *schema
                                && table.name == *name
                                && ObjectKind::from_metadata(&table.kind) == *kind
                        })
                    })
                }
                NodeIdentity::Query(connection, id) => self
                    .library
                    .iter()
                    .any(|query| query.id == *id && query.connection == *connection),
            });
        if !valid {
            self.navigation_selection = None;
        }
    }
    fn open_saved_query(&mut self, id: Uuid) {
        let Some(mut query) = self.library.iter().find(|query| query.id == id).cloned() else {
            return;
        };
        self.new_tab(query.connection);
        self.tabs[self.active].sql = std::mem::take(&mut query.sql);
        self.tabs[self.active].title = std::mem::take(&mut query.name);
    }
    fn open_sql(&self, tab: Uuid) {
        let t = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            if let Some(f) = rfd::AsyncFileDialog::new()
                .add_filter("SQL", &["sql"])
                .pick_file()
                .await
            {
                let p = f.path();
                let r = async {
                    let md = tokio::fs::metadata(p).await?;
                    anyhow::ensure!(md.len() <= sql::MAX_SQL_BYTES as u64, "sql_file_too_large");
                    let b = read_bounded(p, sql::MAX_SQL_BYTES).await?;
                    let s =
                        String::from_utf8(b).map_err(|_| anyhow::anyhow!("sql_utf8_required"))?;
                    sql::check_script(&s)?;
                    Ok::<_, anyhow::Error>(s.trim_start_matches('\u{feff}').into())
                }
                .await
                .map_err(|e| e.to_string());
                let _ = t.send(Message::FileSql(tab, r)).await;
                ctx.request_repaint();
            }
        });
    }
    fn save_sql(&self, tab: Uuid) {
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab) else {
            return;
        };
        let sql = tab.sql.clone();
        let saved = self.tr("Zapisano plik SQL.", "SQL file saved.");
        let t = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            if let Some(f) = rfd::AsyncFileDialog::new()
                .add_filter("SQL", &["sql"])
                .set_file_name("query.sql")
                .save_file()
                .await
            {
                let r = tokio::fs::write(f.path(), sql.as_bytes())
                    .await
                    .map(|_| saved.into())
                    .map_err(|_| "sql_save_failed".into());
                let _ = t.send(Message::Notice(r)).await;
                ctx.request_repaint();
            }
        });
    }
    fn export_result(&self, id: Uuid) {
        let Some(tab) = self.tabs.iter().find(|t| t.id == id) else {
            return;
        };
        let Some(r) = tab.result.clone() else {
            return;
        };
        if !self.capabilities(tab.connection).export {
            return;
        }
        // Literals of an SQL export are written the way the database of the
        // result reads them; a result without a connection gets standard SQL.
        let dialect = tab
            .connection
            .and_then(|c| self.databases.get(&c).map(|db| *db.dialect()))
            .unwrap_or(Dialect::ANSI);
        let table = tab.table.clone();
        let format = match self.export_format {
            Format::Sql if self.capabilities(tab.connection).language != QueryLanguage::Sql => {
                Format::Csv
            }
            format => format,
        };
        let saved = self.tr("Zapisano eksport wyniku.", "Result export saved.");
        let t = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            if let Some(f) = rfd::AsyncFileDialog::new()
                .add_filter(format.label(), &[format.extension()])
                .set_file_name(format!("result.{}", format.extension()))
                .save_file()
                .await
            {
                let r = async {
                    let b = tokio::task::spawn_blocking(move || {
                        transfer::export(&r, format, &dialect, table.as_ref())
                    })
                    .await
                    .map_err(|_| anyhow::anyhow!("export_task_failed"))??;
                    tokio::fs::write(f.path(), b)
                        .await
                        .map_err(|_| anyhow::anyhow!("export_write_failed"))?;
                    Ok::<_, anyhow::Error>(saved.into())
                }
                .await
                .map_err(|e| e.to_string());
                let _ = t.send(Message::Exported(r)).await;
                ctx.request_repaint();
            }
        });
    }
    fn import_data(&mut self, id: Uuid) {
        let connection = self
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .and_then(|tab| tab.connection);
        if !self.capabilities(connection).import {
            let text = self.unavailable(
                connection,
                "import danych nie jest dostępny dla tej bazy.",
                "data import is not available for this database.",
            );
            self.notify(text, true);
            return;
        }
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            if let Some(f) = rfd::AsyncFileDialog::new()
                .add_filter("CSV / TSV / JSON", &["csv", "tsv", "json"])
                .pick_file()
                .await
            {
                let path = f.path().to_owned();
                let r = async {
                    let m = tokio::fs::metadata(&path).await?;
                    anyhow::ensure!(m.len() <= 5 * 1024 * 1024, "import_file_too_large");
                    let format = match path
                        .extension()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_lowercase()
                        .as_str()
                    {
                        "csv" => Format::Csv,
                        "tsv" => Format::Tsv,
                        "json" => Format::Json,
                        _ => anyhow::bail!("import_format_unsupported"),
                    };
                    let data = read_bounded(&path, 5 * 1024 * 1024).await?;
                    let (headers, rows) =
                        tokio::task::spawn_blocking(move || transfer::import_rows(&data, format))
                            .await
                            .map_err(|_| anyhow::anyhow!("import_task_failed"))??;
                    Ok::<_, anyhow::Error>((
                        headers,
                        rows,
                        path.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                    ))
                }
                .await
                .map_err(|e| e.to_string());
                let _ = tx.send(Message::DataPreview(id, r)).await;
                ctx.request_repaint();
            }
        });
    }
    fn save_connection_draft(&mut self, draft: &ConnectionProfile, editor: Uuid) {
        if self.group_busy || self.group_loading || self.group_store.is_none() {
            self.connection_editor.feedback = Some((
                self.tr(
                    "Poczekaj na odświeżenie lub zapis grup. Formularz zachował dane.",
                    "Wait for groups to refresh or finish saving. Your draft is preserved.",
                )
                .into(),
                true,
            ));
            return;
        }
        if let Err(e) = draft.validate() {
            self.connection_editor.feedback = Some((self.explain(e.to_string()), true));
            return;
        }
        let Some(vault) = self.vault.clone() else {
            return;
        };
        self.connection_editor.invalidate_key_load();
        self.connection_editor.busy = true;
        self.connection_editor.saving = true;
        self.saving = true;
        let before = SecretProfiles(self.profiles.clone());
        let draft = SecretDraft(draft.clone());
        let opened_key = self.connection_editor.opened_key().to_owned();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let result = tokio::task::spawn_blocking(move || {
                connection_edit::save_profile(&vault, &before.0, &draft.0, &opened_key)
            })
            .await
            .map_err(|_| "connection_save_task_failed".to_owned())
            .and_then(|r| r.map_err(|e| e.to_string()));
            if let Err(error) = tx.send(Message::ConnectionSaved(editor, result)).await
                && let Message::ConnectionSaved(_, Ok(mut previous)) = error.0
            {
                profiles::wipe_secrets(&mut previous);
            }
            ctx.request_repaint();
        });
    }
    fn finish_connection_save(&mut self) {
        if let Some(draft) = self.connection_dialog.take() {
            // Show where the connection is: a new one is easy to miss in a
            // long list.
            if self.profiles.connections.iter().any(|p| p.id == draft.id) {
                self.navigation_selection = Some(NodeIdentity::Connection(draft.id));
            }
            drop(SecretDraft(draft));
        }
        self.connection_editor.close();
        self.connection_form_group = None;
        self.connection_group_save = None;
        self.notify(
            self.tr(
                "Połączenie zapisane. Nowe dane zostaną użyte przy następnym połączeniu.",
                "Connection saved. The next connection uses the updated settings.",
            )
            .into(),
            false,
        );
        self.begin_save();
    }
    fn reload_connection_base(&mut self, editor: Uuid) {
        let Some(vault) = self.vault.clone() else {
            return;
        };
        self.connection_editor.busy = true;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let result = tokio::task::spawn_blocking(move || vault.load_profiles())
                .await
                .map_err(|_| "connection_reload_task_failed".to_owned())
                .and_then(|r| r.map_err(|e| e.to_string()));
            if let Err(error) = tx.send(Message::ConnectionBaseReload(editor, result)).await
                && let Message::ConnectionBaseReload(_, Ok(mut previous)) = error.0
            {
                profiles::wipe_secrets(&mut previous);
            }
            ctx.request_repaint();
        });
    }
    fn test_connection_draft(&mut self, draft: &ConnectionProfile, editor: Uuid) {
        if let Err(e) = draft.validate() {
            self.connection_editor.feedback = Some((self.explain(e.to_string()), true));
            return;
        }
        let fingerprint = match connection_edit::fingerprint(draft) {
            Ok(v) => v,
            Err(e) => {
                self.connection_editor.feedback = Some((self.explain(e.to_string()), true));
                return;
            }
        };
        self.connection_editor.invalidate_key_load();
        self.connection_editor.busy = true;
        self.connection_editor.saving = false;
        self.connection_editor.feedback = None;
        // Test with the server key the connection would be saved with.
        let mut draft = SecretDraft(draft.clone());
        connection_edit::settle_server_key(
            &self.profiles,
            self.connection_editor.opened_key(),
            &mut draft.0,
        );
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let handle = self.runtime.spawn(async move {
            let result = async {
                tokio::time::timeout(Duration::from_secs(35), connection_edit::probe(draft))
                    .await
                    .map_err(|_| anyhow::anyhow!("connection_test_timeout"))?
            }
            .await
            .map_err(|e| (e.to_string(), connection_edit::changed_key(&e)));
            let _ = tx
                .send(Message::ConnectionTest(editor, fingerprint, result))
                .await;
            ctx.request_repaint();
        });
        let abort = handle.abort_handle();
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.push(abort.clone());
        }
        self.connection_probe_job = Some(abort);
    }

    /// Apply several changes of one tab in order, as one action: the first
    /// refusal stops the rest. Every change is recorded in the history.
    fn mutate_many(&self, id: Uuid, db: Database, changes: Vec<RowChange>) -> anyhow::Result<()> {
        let texts = changes
            .iter()
            .map(|change| db.change_text(change))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let vault = self.vault.clone();
        let connection_id = self.tab_connection(id);
        self.track(async move {
            // One entry of the history for the whole batch: writing the
            // encrypted history once per cell made large edits slow.
            let text = texts.join(";\n");
            let mut done = 0usize;
            let run = async {
                for change in &changes {
                    db.apply(change).await?;
                    done += 1;
                }
                Ok(())
            };
            let outcome = recorded(vault, connection_id, &text, run).await;
            let _ = tx.send(Message::Mutations(id, done, outcome.err())).await;
            ctx.request_repaint();
        });
        Ok(())
    }

    /// Make one change of one record of the tab's table in the background
    /// and record it in the history. Fails at once when the change cannot be
    /// written down, for example without a primary key.
    fn mutate(&self, id: Uuid, db: Database, change: RowChange) -> anyhow::Result<()> {
        let sql = db.change_text(&change)?;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let vault = self.vault.clone();
        let connection_id = self.tab_connection(id);
        self.track(async move {
            let r = recorded(vault, connection_id, &sql, db.apply(&change)).await;
            let _ = tx.send(Message::Mutation(id, r)).await;
            ctx.request_repaint();
        });
        Ok(())
    }

    /// Remember how to take back the changes about to be made for a tab,
    /// until the database says how many were made.
    fn remember_changes(
        &mut self,
        id: Uuid,
        changes: &[RowChange],
        inverses: Vec<Option<RowChange>>,
        kind: ChangeKind,
    ) {
        self.pending_changes.insert(
            id,
            PendingChanges {
                changes: changes.to_vec(),
                inverses,
                kind,
                shown: false,
            },
        );
    }

    /// The first `done` changes of a tab were made: they can be taken back
    /// (Mod+Z) or, once taken back, made again (Mod+Shift+Z).
    /// Returns whether the grid already shows every change that was made.
    fn settle_changes(&mut self, id: Uuid, done: usize) -> bool {
        let Some(pending) = self.pending_changes.remove(&id) else {
            return false;
        };
        let shown = pending.shown && done == pending.changes.len();
        // Taking back runs the inverses newest first.
        let pairs: Vec<(RowChange, RowChange)> = (0..done.min(pending.changes.len()))
            .rev()
            .filter_map(|k| {
                pending.inverses[k]
                    .clone()
                    .map(|inverse| (inverse, pending.changes[k].clone()))
            })
            .collect();
        if pairs.is_empty() {
            return shown;
        }
        let stack = match pending.kind {
            ChangeKind::Do => {
                self.redo.retain(|(tab, _)| *tab != id);
                &mut self.undo
            }
            ChangeKind::Redo => &mut self.undo,
            ChangeKind::Undo => &mut self.redo,
        };
        stack.push((id, pairs));
        if stack.len() > 200 {
            stack.remove(0);
        }
        shown
    }

    /// Mod+Z and Mod+Shift+Z in a grid: take back the last change of the
    /// tab's table, or make again the last one taken back.
    fn undo_changes(&mut self, index: usize, again: bool) {
        let id = self.tabs[index].id;
        let stack = if again {
            &mut self.redo
        } else {
            &mut self.undo
        };
        let Some(position) = stack.iter().rposition(|(tab, _)| *tab == id) else {
            let text = if again {
                self.tr("Nie ma czego ponowić.", "There is nothing to redo.")
            } else {
                self.tr("Nie ma czego cofnąć.", "There is nothing to undo.")
            };
            self.notify(text.into(), false);
            return;
        };
        let (_, pairs) = stack.remove(position);
        let Some(db) = self.tabs[index]
            .connection
            .and_then(|c| self.databases.get(&c))
            .cloned()
        else {
            self.notify("connection_unavailable".into(), true);
            return;
        };
        let changes: Vec<RowChange> = pairs.iter().map(|(apply, _)| apply.clone()).collect();
        let inverses = pairs.into_iter().map(|(_, back)| Some(back)).collect();
        let kind = if again {
            ChangeKind::Redo
        } else {
            ChangeKind::Undo
        };
        self.remember_changes(id, &changes, inverses, kind);
        if let Err(error) = self.mutate_many(id, db, changes) {
            self.pending_changes.remove(&id);
            self.notify(error.to_string(), true);
        }
    }

    /// The connection of the tab with this id, for the history.
    fn tab_connection(&self, id: Uuid) -> Uuid {
        self.tabs
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.connection)
            .unwrap_or_default()
    }
}

/// Run one change the way every change is run: written to the history
/// first, then made, then its outcome written next to it. Without the vault
/// nothing is changed.
async fn recorded(
    vault: Option<Arc<Vault>>,
    connection_id: Uuid,
    text: &str,
    change: impl std::future::Future<Output = anyhow::Result<()>>,
) -> Result<(), String> {
    let started = Instant::now();
    let record = HistoryRecord {
        id: Uuid::new_v4(),
        connection_id,
        sql: sql::redact_sql(text),
        status: "running".into(),
        started_at: chrono::Utc::now().to_rfc3339(),
        duration_ms: 0,
        rows: 0,
        error: None,
    };
    let r = async {
        let vault = vault
            .clone()
            .ok_or_else(|| anyhow::anyhow!("vault_unavailable"))?;
        let begin = record.clone();
        tokio::task::spawn_blocking(move || vault.append_history(&begin))
            .await
            .map_err(|_| anyhow::anyhow!("history_task_failed"))??;
        change.await
    }
    .await
    .map_err(|e| e.to_string());
    let mut done = record;
    done.duration_ms = started.elapsed().as_millis() as u64;
    done.status = if r.is_ok() { "completed" } else { "failed" }.into();
    done.rows = u64::from(r.is_ok());
    done.error = r.as_ref().err().cloned();
    if let Some(v) = vault {
        let _ = tokio::task::spawn_blocking(move || v.append_history(&done)).await;
    }
    r
}
impl eframe::App for StudioApp {
    fn logic(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.messages();
        self.lock_idle(ctx);
        self.qa_tick(ctx);
    }
    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        self.shell(ui);
    }
    fn raw_input_hook(&mut self, _: &egui::Context, raw_input: &mut egui::RawInput) {
        // QA only; a normal launch has no script and returns at once.
        self.qa_input(raw_input);
    }
    fn on_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.saving && Instant::now() < deadline {
            self.messages();
            if self.saving {
                std::thread::sleep(Duration::from_millis(15));
            }
        }
        for tab in &self.tabs {
            tab.cancel.cancel();
        }
        profiles::wipe_secrets(&mut self.profiles);
        if let Some(previous) = self.pending_save.take() {
            drop(SecretProfiles(previous));
        }
        if let Some(draft) = self.connection_dialog.take() {
            drop(SecretDraft(draft));
        }
        self.connection_editor.close();
        if let Some(job) = self.connection_probe_job.take() {
            job.abort();
        }
        if let Some(root) = self.qa_root.take() {
            for db in self.databases.values() {
                self.runtime.block_on(db.close());
            }
            if let Some(v) = self.vault.take() {
                let _ = v.remove_owned_qa_key();
                drop(v);
            }
            if root.file_name().is_some_and(|n| {
                n.to_string_lossy()
                    .starts_with("BazaStudio-native-render-QA-")
            }) {
                let _ = std::fs::remove_dir_all(root);
            }
        }
    }
}
/// Line height of code in the SQL editor, shared with its line numbers.
fn code_line_height(size: f32) -> f32 {
    (size * 1.55).round()
}
fn highlight(text: &str, size: f32, p: theme::Palette) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let words = [
        "SELECT", "FROM", "WHERE", "JOIN", "LEFT", "INNER", "ON", "AND", "OR", "AS", "INSERT",
        "INTO", "UPDATE", "SET", "DELETE", "CREATE", "TABLE", "ALTER", "DROP", "NULL", "TRUE",
        "FALSE", "ORDER", "BY", "GROUP", "HAVING", "LIMIT", "OFFSET", "COUNT", "SUM", "AVG", "MIN",
        "MAX", "DISTINCT", "VALUES",
    ];
    let mut i = 0;
    let chars: Vec<char> = text.chars().collect();
    while i < chars.len() {
        let start = i;
        let color = if chars[i] == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            p.comment
        } else if chars[i] == '\'' {
            i += 1;
            while i < chars.len() {
                if chars[i] == '\'' {
                    i += 1;
                    if chars.get(i) == Some(&'\'') {
                        i += 1;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            p.string
        } else if chars[i].is_alphanumeric() || chars[i] == '_' {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let token: String = chars[start..i].iter().collect();
            if words.contains(&token.to_uppercase().as_str()) {
                p.keyword
            } else if token.chars().next().unwrap().is_ascii_digit() {
                p.number
            } else {
                p.text
            }
        } else {
            i += 1;
            p.text
        };
        let s: String = chars[start..i].iter().collect();
        job.append(
            &s,
            0.0,
            egui::TextFormat {
                font_id: egui::FontId::monospace(size),
                line_height: Some(code_line_height(size)),
                color,
                ..Default::default()
            },
        );
    }
    job
}
impl StudioApp {
    fn open_profile_file(&mut self) {
        self.profile_file_request = Uuid::new_v4();
        let request = self.profile_file_request;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            if let Some(f) = rfd::AsyncFileDialog::new()
                .add_filter("Baza Studio profiles", &["json", "bzx"])
                .pick_file()
                .await
            {
                let p = f.path().to_owned();
                let r = async {
                    let b = read_bounded(&p, profiles::MAX_BYTES).await?;
                    Ok::<_, anyhow::Error>((
                        b,
                        p.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                    ))
                }
                .await
                .map_err(|e| e.to_string());
                let _ = tx.send(Message::ProfileFile(request, r)).await;
                ctx.request_repaint();
            }
        });
    }
}
async fn read_bounded(path: &std::path::Path, limit: usize) -> anyhow::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let f = tokio::fs::File::open(path)
        .await
        .map_err(|_| anyhow::anyhow!("file_read_failed"))?;
    let mut data = zeroize::Zeroizing::new(Vec::new());
    f.take((limit + 1) as u64)
        .read_to_end(&mut data)
        .await
        .map_err(|_| anyhow::anyhow!("file_read_failed"))?;
    anyhow::ensure!(data.len() <= limit, "file_too_large");
    Ok(std::mem::take(&mut *data))
}

impl StudioApp {
    fn track(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        let h = self.runtime.spawn(future);
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.retain(|h| !h.is_finished());
            jobs.push(h.abort_handle());
        }
    }
    fn discard_locked(&self, message: Message) {
        match message {
            Message::Connected(_, _, Ok((db, _, manager))) => {
                self.runtime.spawn(async move {
                    db.close().await;
                    if let Some(m) = manager {
                        m.shutdown().await;
                    }
                });
            }
            Message::Query(_, _, mut history, _, _, result) => {
                history.sql.zeroize();
                if let Ok(mut result) = result {
                    for row in &mut result.rows {
                        for c in row {
                            wipe_cell(c);
                        }
                    }
                }
            }
            Message::FileSql(_, Ok(mut sql)) => sql.zeroize(),
            Message::ConnectionSaved(_, Ok(mut set))
            | Message::ConnectionBaseReload(_, Ok(mut set)) => profiles::wipe_secrets(&mut set),
            Message::ProfileFile(_, Ok((mut bytes, _))) => bytes.zeroize(),
            Message::Library(_, _, Ok(mut rows)) => {
                for q in &mut rows {
                    q.sql.zeroize();
                }
            }
            Message::DataPreview(_, Ok((_, mut rows, _))) => {
                for row in &mut rows {
                    for c in row {
                        wipe_cell(c);
                    }
                }
            }
            _ => {}
        }
    }

    fn lock_idle(&mut self, ctx: &egui::Context) {
        if self.locked || !self.lock_settings.enabled {
            return;
        }
        if ctx.input(|i| !i.events.is_empty()) {
            self.last_activity = Instant::now();
        }
        let remaining = Duration::from_secs(self.lock_settings.idle_minutes as u64 * 60)
            .saturating_sub(self.last_activity.elapsed());
        if remaining.is_zero() {
            self.lock_now();
        } else {
            ctx.request_repaint_after(remaining.min(Duration::from_secs(15)));
        }
    }
    fn lock_now(&mut self) {
        if !self.lock_settings.enabled || self.locked {
            return;
        }
        self.locked = true;
        self.lock_busy = true;
        self.connect_requests.clear();
        self.ctx.data_mut(|data| data.clear());
        for tab in &mut self.tabs {
            tab.cancel.cancel();
            tab.sql.zeroize();
            if let Some(result) = &mut tab.result {
                for row in &mut result.rows {
                    for cell in row {
                        wipe_cell(cell);
                    }
                }
            }
            tab.result = None;
        }
        for record in &mut self.history {
            record.sql.zeroize();
            if let Some(e) = &mut record.error {
                e.zeroize();
            }
        }
        self.history.clear();
        self.history_clear_confirm = false;
        self.history_clearing = false;
        self.library_generation = self.library_generation.wrapping_add(1);
        self.profile_file_request = Uuid::new_v4();
        self.group_generation = self.group_generation.wrapping_add(1);
        self.connection_form_group = None;
        self.connection_group_save = None;
        self.library.clear();
        self.group_store = None;
        self.group_snapshot = GroupSnapshot::default();
        if let Some(g) = &mut self.group_dialog {
            g.name.zeroize();
        }
        self.group_dialog = None;
        self.group_remove = None;
        self.group_busy = false;
        self.group_loading = false;
        self.suggestions = None;
        if let Some(name) = &mut self.save_query_name {
            name.zeroize();
        }
        self.save_query_name = None;
        self.nav_search.zeroize();
        self.navigation_selection = None;
        profiles::wipe_secrets(&mut self.profiles);
        let prefs = self.profiles.preferences.clone();
        self.replace_profiles(ProfileSet::default());
        self.profiles.preferences = prefs;
        self.tabs = vec![QueryTab::new(None)];
        self.active = 0;
        self.tables.clear();
        if let Some(p) = &mut self.connection_dialog {
            p.password.zeroize();
            if let Some(s) = &mut p.ssh {
                wipe_ssh_form(s);
            }
        }
        self.connection_dialog = None;
        self.connection_editor.close();
        if let Some(job) = self.connection_probe_job.take() {
            job.abort();
        }
        if let Some(e) = &mut self.cell_editor {
            e.value.zeroize();
        }
        self.cell_editor = None;
        if let Some(s) = &mut self.profile_exchange {
            s.bytes.zeroize();
            s.passphrase.zeroize();
        }
        self.profile_exchange = None;
        if let Some(s) = &mut self.import {
            for row in &mut s.rows {
                for c in row {
                    wipe_cell(c);
                }
            }
        }
        self.import = None;
        self.builder = None;
        if let Some(s) = &mut self.new_row {
            for (_, value, _, _) in &mut s.values {
                value.zeroize();
            }
        }
        self.new_row = None;
        if let Some(p) = &mut self.pending_save {
            profiles::wipe_secrets(p);
        }
        self.pending_save = None;
        self.saving = false;
        self.updater = None;
        self.update_offer = None;
        self.update_message = None;
        self.host_key_change = None;
        self.show_distribution = false;
        self.show_about = false;
        self.distribution_busy = false;
        self.qr_texture = None;
        self.notice = None;
        self.history_search.zeroize();
        self.group_draft.zeroize();
        if let Some(r) = &mut self.monitor_result {
            for row in &mut r.rows {
                for c in row {
                    wipe_cell(c);
                }
            }
        }
        self.monitor_result = None;
        self.monitor_busy = false;
        self.monitor_connection = None;
        self.monitor_series.clear();
        self.show_preferences = false;
        self.preference_original = None;
        self.preference_save = PreferencesSaveState::default();
        self.show_lock_settings = false;
        self.unlock_password.zeroize();
        self.unlock_code.zeroize();
        self.enroll_password.zeroize();
        self.enroll_confirm.zeroize();
        self.enrollment = None;
        for c in &mut self.recovery_codes {
            c.zeroize();
        }
        self.recovery_codes.clear();
        let databases = std::mem::take(&mut self.databases);
        let managers = std::mem::take(&mut self.tunnel_managers);
        let lock = self.local_lock.clone();
        let jobs = self.jobs.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn(async move {
            let result = async {
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    let pending = jobs
                        .lock()
                        .map(|j| j.iter().any(|h| !h.is_finished()))
                        .unwrap_or(false);
                    if !pending || Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                if let Ok(list) = jobs.lock() {
                    for h in list.iter() {
                        h.abort();
                    }
                }
                for d in databases.values() {
                    d.close().await;
                }
                for m in managers.values() {
                    m.shutdown().await;
                }
                if let Some(l) = lock {
                    tokio::task::spawn_blocking(move || l.forget_unlock())
                        .await
                        .map_err(|_| anyhow::anyhow!("local_lock_task_failed"))??;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await
            .map_err(|e| e.to_string());
            let _ = tx.send(Message::LockStopped(result)).await;
            ctx.request_repaint();
        });
    }
    fn unlock(&mut self) {
        let (Some(lock), Some(vault)) = (self.local_lock.clone(), self.vault.clone()) else {
            return;
        };
        self.lock_busy = true;
        let password = zeroize::Zeroizing::new(self.unlock_password.clone());
        let code = zeroize::Zeroizing::new(self.unlock_code.clone());
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let r = tokio::task::spawn_blocking(move || {
                lock.verify(&password, if code.is_empty() { None } else { Some(&code) })?;
                let p = vault.load_profiles()?;
                let h = vault.load_history()?;
                Ok::<_, anyhow::Error>((p, h))
            })
            .await
            .map_err(|_| "local_lock_task_failed".to_owned())
            .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = tx.send(Message::Unlocked(r)).await;
            ctx.request_repaint();
        });
    }
}
fn wipe_ssh_form(s: &mut SshProfile) {
    for v in [&mut s.password, &mut s.private_key, &mut s.passphrase]
        .into_iter()
        .flatten()
    {
        v.zeroize();
    }
}
fn wipe_cell(c: &mut Cell) {
    match c {
        Cell::Text(s)
        | Cell::Json(s)
        | Cell::Integer(s)
        | Cell::Decimal(s)
        | Cell::Float(s)
        | Cell::Unsupported(s) => s.zeroize(),
        Cell::Bytes(b) => b.zeroize(),
        _ => {}
    }
}

impl StudioApp {
    /// Read the server statistics of the connection in focus, when it is
    /// connected. Changing the connection starts a new series.
    fn refresh_monitor(&mut self) {
        let Some(id) = self.focus_connection() else {
            return;
        };
        if self.monitor_connection != Some(id) {
            self.monitor_connection = Some(id);
            self.monitor_busy = false;
            self.monitor_series.clear();
            if let Some(mut previous) = self.monitor_result.take() {
                for row in &mut previous.rows {
                    for cell in row {
                        wipe_cell(cell);
                    }
                }
            }
        }
        if self.monitor_busy {
            return;
        }
        let Some(db) = self.databases.get(&id).cloned() else {
            return;
        };
        if !db.capabilities().monitor {
            return;
        }
        self.monitor_busy = true;
        self.monitor_last = Instant::now();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let r = db.server_stats().await.map_err(|e| e.to_string());
            let _ = tx.send(Message::Monitor(id, r)).await;
            ctx.request_repaint();
        });
    }
}

impl StudioApp {
    /// Prepare the updater. This opens no connection; checking is manual.
    fn init_distribution(&mut self) {
        if self.distribution_busy {
            return;
        }
        self.distribution_busy = true;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let r = tokio::task::spawn_blocking(Updater::new)
                .await
                .map_err(|_| "update_task_failed".to_owned())
                .and_then(|r| r.map(Arc::new).map_err(|e| e.to_string()));
            let _ = tx.send(Message::Distribution(r)).await;
            ctx.request_repaint();
        });
    }
}
fn qr_texture(ctx: &egui::Context, uri: &str) -> Option<egui::TextureHandle> {
    let code = qrcode::QrCode::new(uri.as_bytes()).ok()?;
    let side = code.width() + 8;
    let mut image = egui::ColorImage::filled([side, side], Color32::WHITE);
    for y in 0..code.width() {
        for x in 0..code.width() {
            if code[(x, y)] == qrcode::Color::Dark {
                image.pixels[(y + 4) * side + x + 4] = Color32::BLACK;
            }
        }
    }
    Some(ctx.load_texture(
        "local_totp_enrollment",
        image,
        egui::TextureOptions::NEAREST,
    ))
}

impl Drop for SavedQuery {
    fn drop(&mut self) {
        self.sql.zeroize();
    }
}
fn decode_library(bytes: Option<&[u8]>) -> anyhow::Result<Vec<SavedQuery>> {
    let rows: Vec<SavedQuery> = match bytes {
        Some(bytes) => serde_json::from_slice(bytes)?,
        None => vec![],
    };
    anyhow::ensure!(rows.len() <= 500, "saved_query_limit");
    let mut ids = std::collections::HashSet::new();
    for row in &rows {
        anyhow::ensure!(
            ids.insert(row.id)
                && !row.name.trim().is_empty()
                && row.name.len() <= 120
                && !row.name.chars().any(char::is_control)
                && row.sql.len() <= sql::MAX_SQL_BYTES,
            "saved_query_record_invalid"
        );
    }
    Ok(rows)
}
impl StudioApp {
    fn load_library(&mut self) {
        self.library_generation = self.library_generation.wrapping_add(1);
        let generation = self.library_generation;
        let Some(vault) = self.vault.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let r = tokio::task::spawn_blocking(move || {
                let bytes = vault.load_named("saved-queries")?;
                decode_library(bytes.as_deref().map(Vec::as_slice))
            })
            .await
            .map_err(|_| "saved_query_task_failed".to_owned())
            .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = tx.send(Message::Library(generation, false, r)).await;
            ctx.request_repaint();
        });
    }
    fn update_library(&mut self, query: Option<SavedQuery>, remove: Option<Uuid>) {
        self.library_generation = self.library_generation.wrapping_add(1);
        let generation = self.library_generation;
        let Some(vault) = self.vault.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let r = tokio::task::spawn_blocking(move || {
                vault.update_named("saved-queries", move |bytes| {
                    let mut rows = decode_library(bytes)?;
                    if let Some(id) = remove {
                        rows.retain(|q| q.id != id);
                    }
                    if let Some(query) = query {
                        anyhow::ensure!(
                            !rows
                                .iter()
                                .any(|q| q.name.to_lowercase() == query.name.to_lowercase()
                                    && q.id != query.id),
                            "saved_query_name_exists"
                        );
                        rows.retain(|q| q.id != query.id);
                        rows.insert(0, query);
                    }
                    let bytes = serde_json::to_vec(&rows)?;
                    anyhow::ensure!(bytes.len() <= 32 * 1024 * 1024, "saved_query_storage_limit");
                    decode_library(Some(&bytes))?;
                    Ok((bytes, rows))
                })
            })
            .await
            .map_err(|_| "saved_query_task_failed".to_owned())
            .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = tx.send(Message::Library(generation, true, r)).await;
            ctx.request_repaint();
        });
    }
}

impl StudioApp {
    fn load_groups(&mut self) {
        if self.group_busy {
            return;
        }
        self.group_generation = self.group_generation.wrapping_add(1);
        let generation = self.group_generation;
        let Some(vault) = self.vault.clone() else {
            return;
        };
        self.group_loading = true;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let r = tokio::task::spawn_blocking(move || {
                let store = Arc::new(Groups::open(vault)?);
                let snapshot = store.load()?;
                Ok::<_, anyhow::Error>((store, snapshot))
            })
            .await
            .map_err(|_| "group_task_failed".to_owned())
            .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = tx.send(Message::GroupsLoaded(generation, r)).await;
            ctx.request_repaint();
        });
    }
    fn group_operation(&mut self, operation: GroupOperation) {
        if self.group_busy {
            self.notify(
                self.tr(
                    "Poczekaj na zapis grupy.",
                    "Wait for the current group save.",
                )
                .into(),
                true,
            );
            return;
        }
        if self.group_loading {
            self.notify(
                self.tr(
                    "Grupy są ładowane. Spróbuj ponownie.",
                    "Groups are loading. Try again.",
                )
                .into(),
                false,
            );
            return;
        }
        let Some(store) = self.group_store.clone() else {
            self.load_groups();
            self.notify(
                self.tr(
                    "Grupy są ładowane. Spróbuj ponownie.",
                    "Groups are loading. Try again.",
                )
                .into(),
                false,
            );
            return;
        };
        self.group_busy = true;
        self.group_generation = self.group_generation.wrapping_add(1);
        let generation = self.group_generation;
        let revision = self.group_snapshot.revision;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let r = tokio::task::spawn_blocking(move || match operation {
                GroupOperation::Create(g) => store.create(revision, &g.name, g.color, g.parent),
                GroupOperation::Rename(g) => {
                    let id =
                        g.id.ok_or_else(|| anyhow::anyhow!("group_identity_invalid"))?;
                    store.update(revision, id, &g.name, g.color, g.parent)
                }
                GroupOperation::Remove(id) => store.remove_group(revision, id),
                GroupOperation::Move(id, parent) => store.move_group(revision, id, parent),
                GroupOperation::Membership(id, group) => store.set_connection(revision, id, group),
                GroupOperation::RemoveConnection(id) => store.remove_connection(revision, id),
            })
            .await
            .map_err(|_| "group_task_failed".to_owned())
            .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = tx.send(Message::GroupSaved(generation, r)).await;
            ctx.request_repaint();
        });
    }
    fn group_path(&self, id: Uuid) -> String {
        let mut parts = Vec::new();
        let mut current = Some(id);
        for _ in 0..8 {
            let Some(g) =
                current.and_then(|id| self.group_snapshot.groups.iter().find(|g| g.id == id))
            else {
                break;
            };
            parts.push(g.name.as_str());
            current = g.parent;
        }
        parts.reverse();
        parts.join(" / ")
    }
    fn group_matches(
        &self,
        id: Uuid,
        profiles: &[ConnectionProfile],
        search: &str,
        depth: usize,
    ) -> bool {
        let matching_connections = profiles
            .iter()
            .filter(|profile| {
                navigator::connection_matches(profile, search)
                    || self.tables.get(&profile.id).is_some_and(|tables| {
                        tables.iter().any(|table| {
                            navigator::matches(&table.schema, search)
                                || navigator::matches(&table.name, search)
                        })
                    })
                    || self.library.iter().any(|query| {
                        query.connection == Some(profile.id)
                            && navigator::matches(&query.name, search)
                    })
            })
            .map(|profile| profile.id)
            .collect();
        navigator::group_visible(
            &self.group_snapshot,
            id,
            &matching_connections,
            search,
            depth,
        )
    }
}

impl Drop for ProfileExchange {
    fn drop(&mut self) {
        self.bytes.zeroize();
        self.passphrase.zeroize();
    }
}

impl Drop for CellEditor {
    fn drop(&mut self) {
        self.value.zeroize();
    }
}
impl StudioApp {
    /// The value of the cell editor waits in the grid until it is saved;
    /// typed for several marked cells, it goes into all of them.
    fn commit_cell(&mut self, editor: CellEditor) -> Result<(), String> {
        let index = self
            .tabs
            .iter()
            .position(|t| t.id == editor.tab)
            .ok_or_else(|| "result_changed_reload_required".to_owned())?;
        let value = (!editor.null).then(|| editor.value.clone());
        let cells = if editor.all_marked {
            self.marked_cells(index)
        } else {
            vec![(editor.row, editor.column)]
        };
        let edits = cells
            .into_iter()
            .map(|(r, c)| (r, c, value.clone()))
            .collect();
        self.edit_cells(index, edits);
        Ok(())
    }
}

fn row_word(count: usize, en: bool) -> &'static str {
    if en {
        if count == 1 { "row" } else { "rows" }
    } else if count == 1 {
        "wiersz"
    } else if (2..=4).contains(&(count % 10)) && !(12..=14).contains(&(count % 100)) {
        "wiersze"
    } else {
        "wierszy"
    }
}

// egui modal Areas are unconstrained vertically. A default Separator inside a
// horizontal Ui would consume that unbounded available height; all preferences
// body geometry must therefore be explicit and finite before allocating children.
fn bound_preferences_body(ui: &mut egui::Ui, width: f32, height: f32) {
    ui.set_width(width);
    ui.set_max_width(width);
    ui.set_height(height);
    ui.set_max_height(height);
    ui.spacing_mut().item_spacing.x = 0.;
}
// Frame children inherit their parent's layout. Both columns sit inside a
// horizontal body, so explicitly lay their controls out vertically before
// applying finite geometry. The production modal and regression use this helper.
fn preferences_column<R>(
    ui: &mut egui::Ui,
    width: f32,
    height: f32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.set_width(width);
        ui.set_max_width(width);
        ui.set_height(height);
        ui.set_max_height(height);
        add_contents(ui)
    })
}
fn preference_card_width(content_width: f32) -> f32 {
    (content_width - 8. * (theme::THEMES.len() - 1) as f32) / theme::THEMES.len() as f32
}
fn paint_preferences_divider(ui: &mut egui::Ui, height: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(1., height), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0., color);
}

#[cfg(test)]
mod preferences_save_tests {
    use super::*;

    fn preview() -> (ProfileSet, Option<Preferences>, bool) {
        let mut current = ProfileSet::default();
        let original = Some(current.preferences.clone());
        current.preferences.language = "en".into();
        current.preferences.font_size = 18.;
        current.preferences.theme = "midnight".into();
        (current, original, true)
    }

    #[tokio::test]
    async fn delayed_failure_keeps_modal_and_draft_then_retry_accepts_only_its_response() {
        let (mut current, mut original, mut visible) = preview();
        let mut state = PreferencesSaveState::default();
        let request = state.begin(true).unwrap();
        assert!(state.begin(false).is_none());
        let (tx, mut rx) = tokio::sync::oneshot::channel::<Result<SecretProfiles, String>>();
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(state.busy() && visible);
        assert_eq!(current.preferences.language, "en");
        assert_eq!(original.as_ref().unwrap().language, "pl");
        assert!(tx.send(Err("OWNED-SAVE-FAILURE".into())).is_ok());
        assert_eq!(
            state.complete(
                request,
                rx.await.unwrap(),
                &mut current,
                &mut original,
                &mut visible
            ),
            PreferencesSaveCompletion::Failed,
        );
        assert!(visible && !state.busy());
        assert_eq!(state.error.as_deref(), Some("OWNED-SAVE-FAILURE"));
        assert_eq!(current.preferences.font_size, 18.);
        assert_eq!(current.preferences.theme, "midnight");
        assert_eq!(original.as_ref().unwrap().font_size, 12.);
        let retry = state.begin(false).unwrap();
        assert_ne!(request, retry);
        assert!(state.error.is_none());
        let old_success = SecretProfiles(ProfileSet::default());
        assert_eq!(
            state.complete(
                request,
                Ok(old_success),
                &mut current,
                &mut original,
                &mut visible
            ),
            PreferencesSaveCompletion::Ignored,
        );
        assert!(state.busy() && visible);
        assert_eq!(current.preferences.language, "en");
        let committed = SecretProfiles(current.clone());
        assert_eq!(
            state.complete(
                retry,
                Ok(committed),
                &mut current,
                &mut original,
                &mut visible
            ),
            PreferencesSaveCompletion::Saved {
                open_security: false
            },
        );
        assert!(!visible && !state.busy() && original.is_none());
        assert_eq!(current.preferences.font_size, 18.);
        assert_eq!(
            state.complete(
                request,
                Err("OLD-FAILURE".into()),
                &mut current,
                &mut original,
                &mut visible
            ),
            PreferencesSaveCompletion::Ignored,
        );
        assert!(!visible && state.error.is_none());
    }

    #[test]
    fn invalidated_preferences_session_ignores_late_success_and_error() {
        let (mut current, _, _) = preview();
        let mut original = None;
        let mut visible = false;
        let mut state = PreferencesSaveState::default();
        let old = state.begin(true).unwrap();
        state = PreferencesSaveState::default();
        let mut stale = ProfileSet::default();
        stale.preferences.font_size = 22.;
        assert_eq!(
            state.complete(
                old,
                Ok(SecretProfiles(stale)),
                &mut current,
                &mut original,
                &mut visible
            ),
            PreferencesSaveCompletion::Ignored,
        );
        assert!(!visible && original.is_none());
        assert_eq!(current.preferences.font_size, 18.);
        original = Some(Preferences::default());
        visible = true;
        let new = state.begin(false).unwrap();
        assert_eq!(
            state.complete(
                old,
                Err("LATE-ERROR".into()),
                &mut current,
                &mut original,
                &mut visible
            ),
            PreferencesSaveCompletion::Ignored,
        );
        assert_eq!(state.request, Some(new));
        assert!(state.error.is_none() && visible);
    }

    #[test]
    fn security_redirect_is_released_only_after_matching_commit() {
        let (mut current, mut original, mut visible) = preview();
        let mut state = PreferencesSaveState::default();
        let request = state.begin(true).unwrap();
        assert!(state.busy() && visible && original.is_some());
        let committed = SecretProfiles(current.clone());
        assert_eq!(
            state.complete(
                request,
                Ok(committed),
                &mut current,
                &mut original,
                &mut visible
            ),
            PreferencesSaveCompletion::Saved {
                open_security: true
            },
        );
        assert!(!visible && original.is_none());
        assert!(!state.open_security);
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn actual_preferences_reopen_preserves_other_instances_connections_and_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let namespace = format!("BazaStudio.QA.{}", Uuid::new_v4());
        let path = directory.path().join("private");
        let vault = Vault::open_owned(&path, &namespace).unwrap();
        let mut base = ProfileSet::default();
        base.connections.push(ConnectionProfile {
            name: "Owned original".into(),
            host: "owned.example.test".into(),
            database: "owned_fixture".into(),
            username: "owned_user".into(),
            password: "OWNED-FAKE-ORIGINAL".into(),
            ..ConnectionProfile::default()
        });
        vault.save_profiles(&base).unwrap();
        let other = Vault::open_owned(&path, &namespace).unwrap();
        let mut latest = SecretProfiles(other.load_profiles().unwrap());
        latest.0.connections[0].name = "Owned other instance edit".into();
        latest.0.connections[0].password = "OWNED-FAKE-UPDATED".into();
        other.save_profiles(&latest.0).unwrap();
        let draft = Preferences {
            language: "en".into(),
            font_size: 22.,
            theme: "midnight".into(),
            ..Preferences::default()
        };
        let saved = persist_preferences(&vault, draft).unwrap();
        assert_eq!(saved.0.connections[0].id, base.connections[0].id);
        assert_eq!(saved.0.connections[0].name, "Owned other instance edit");
        assert_eq!(saved.0.connections[0].password, "OWNED-FAKE-UPDATED");
        let reopened = Vault::open_owned(&path, &namespace).unwrap();
        let read = SecretProfiles(reopened.load_profiles().unwrap());
        assert_eq!(read.0.preferences.language, "en");
        assert_eq!(read.0.preferences.font_size, 22.);
        assert_eq!(read.0.preferences.theme, "midnight");
        assert_eq!(read.0.connections[0].password, "OWNED-FAKE-UPDATED");
        vault.remove_owned_qa_key().unwrap();
    }

    #[cfg(any(windows, target_os = "macos"))]
    #[test]
    fn actual_failed_preferences_write_preserves_draft_and_disk_then_retry_reopens() {
        let directory = tempfile::tempdir().unwrap();
        let namespace = format!("BazaStudio.QA.{}", Uuid::new_v4());
        let path = directory.path().join("private");
        let vault = Vault::open_owned(&path, &namespace).unwrap();
        vault.save_profiles(&ProfileSet::default()).unwrap();
        let (mut current, mut original, mut visible) = preview();
        current.preferences.language = "invalid-language".into();
        let mut state = PreferencesSaveState::default();
        let request = state.begin(true).unwrap();
        let result =
            persist_preferences(&vault, current.preferences.clone()).map_err(|e| e.to_string());
        assert_eq!(
            state.complete(request, result, &mut current, &mut original, &mut visible),
            PreferencesSaveCompletion::Failed,
        );
        assert!(visible && original.is_some() && state.error.is_some());
        assert_eq!(current.preferences.language, "invalid-language");
        assert_eq!(current.preferences.font_size, 18.);
        assert_eq!(vault.load_profiles().unwrap().preferences.language, "pl");
        current.preferences.language = "en".into();
        let retry = state.begin(false).unwrap();
        let result =
            persist_preferences(&vault, current.preferences.clone()).map_err(|e| e.to_string());
        assert_eq!(
            state.complete(retry, result, &mut current, &mut original, &mut visible),
            PreferencesSaveCompletion::Saved {
                open_security: false
            },
        );
        let reopened = Vault::open_owned(&path, &namespace).unwrap();
        assert_eq!(reopened.load_profiles().unwrap().preferences.language, "en");
        assert_eq!(reopened.load_profiles().unwrap().preferences.font_size, 18.);
        assert!(!visible && original.is_none() && state.error.is_none());
        vault.remove_owned_qa_key().unwrap();
    }
}

#[cfg(test)]
mod preferences_layout_tests {
    use super::*;
    #[test]
    fn preferences_modal_body_and_theme_shapes_stay_finite_in_unconstrained_area() {
        for size in [
            egui::vec2(960., 640.),
            egui::vec2(1440., 900.),
            egui::vec2(1920., 1080.),
        ] {
            let ctx = egui::Context::default();
            let width = (size.x - 80.).clamp(720., 890.);
            let height = (size.y - 190.).clamp(340., 510.);
            let content_width = width - 201.;
            let card_width = preference_card_width(content_width);
            assert!(card_width.is_finite() && card_width >= 85.);
            let mut modal_rect = egui::Rect::NOTHING;
            // Run multiple real egui layout frames so Area sizing settles.
            for _ in 0..3 {
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        ..Default::default()
                    },
                    |root_ui| {
                        let ctx = root_ui.ctx();
                        let response = egui::Modal::new(egui::Id::new(
                            "preferences_layout_regression",
                        ))
                        .show(ctx, |ui| {
                            ui.set_width(width);
                            ui.label("Owned preferences layout regression");
                            ui.horizontal_top(|ui| {
                                bound_preferences_body(ui, width, height);
                                egui::Frame::NONE
                                    .inner_margin(egui::Margin::symmetric(10, 14))
                                    .show(ui, |ui| {
                                        preferences_column(ui, 140., height - 28., |ui| {
                                            for _ in 0..4 {
                                                let _ = ui.allocate_exact_size(
                                                    egui::vec2(140., 38.),
                                                    egui::Sense::hover(),
                                                );
                                            }
                                        });
                                    });
                                paint_preferences_divider(ui, height, Color32::GRAY);
                                egui::Frame::NONE
                                    .inner_margin(egui::Margin::symmetric(20, 20))
                                    .show(ui, |ui| {
                                        preferences_column(ui, content_width, height - 40., |ui| {
                                            egui::ScrollArea::vertical()
                                                .auto_shrink([false, false])
                                                .max_width(content_width)
                                                .max_height(height - 40.)
                                                .show(ui, |ui| {
                                                    ui.horizontal(|ui| {
                                                        ui.spacing_mut().item_spacing.x = 8.;
                                                        for (id, _) in theme::THEMES {
                                                            let (rect, _) = ui.allocate_exact_size(
                                                                egui::vec2(card_width, 135.),
                                                                egui::Sense::hover(),
                                                            );
                                                            ui.painter().rect_filled(
                                                                rect,
                                                                4.,
                                                                theme::base(id).side,
                                                            );
                                                        }
                                                    });
                                                    ui.label("Finite scroll body");
                                                });
                                        });
                                    });
                            });
                            ui.label("Owned bounded footer");
                            ui.min_rect()
                        });
                        // Modal's Area response also includes its viewport-sized
                        // backdrop child. The actual preference content bounds
                        // are the min_rect of the content Ui, returned as inner.
                        modal_rect = response.inner;
                    },
                );
                // This headless layout check has no renderer. Explicitly consume
                // texture deltas before assertions so a failure is not a second
                // panic from epaint's mandatory delta-application guard.
                output.textures_delta.clear();
                assert!(
                    modal_rect.min.x.is_finite()
                        && modal_rect.min.y.is_finite()
                        && modal_rect.max.x.is_finite()
                        && modal_rect.max.y.is_finite()
                );
                assert!(
                    modal_rect.width() < size.x && modal_rect.height() < size.y,
                    "viewport={size:?}, modal={modal_rect:?}, content={:?}",
                    ctx.content_rect()
                );
                for shape in output.shapes {
                    let rect = shape.shape.visual_bounding_rect();
                    if rect.is_positive() {
                        assert!(
                            rect.min.x.is_finite()
                                && rect.min.y.is_finite()
                                && rect.max.x.is_finite()
                                && rect.max.y.is_finite()
                        );
                        assert!(rect.width() <= size.x * 1.1 && rect.height() <= size.y * 1.1);
                    }
                }
            }
        }
    }
}
