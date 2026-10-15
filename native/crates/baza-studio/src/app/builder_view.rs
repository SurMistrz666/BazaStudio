//! "Kreator zapytań": a SELECT drawn on a canvas. The tables of the
//! connection are put on it from the list beside it, their columns ticked
//! into the result, and a column dragged onto a column of another table
//! relates the two in a join. The pages under the canvas set the clauses;
//! the SQL beside it is formatted, and runs into the last page.
use super::shortcuts::take_mod;
use super::*;
use crate::widgets::Btn;
use baza_core::visual::{
    Aggregate, Condition, Field, JoinKind, Operator, OrderTerm, Output, Relation, VisualQuery,
    grouping,
};
use eframe::egui::RichText;
use std::collections::{HashMap, HashSet};

/// Width of a table on the canvas, the height of its title and of a column.
const NODE_WIDTH: f32 = 210.0;
const HEADER: f32 = 26.0;
const ROW: f32 = 20.0;
/// Columns shown at once; a longer table scrolls inside its box.
const MAX_ROWS: usize = 12;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BuilderPage {
    Select,
    From,
    Where,
    Group,
    Having,
    Order,
    Limit,
    Result,
}

/// The box of a table on the canvas.
#[derive(Clone, Copy)]
pub(super) struct Node {
    pub pos: egui::Pos2,
    /// Points the columns inside the box are scrolled by.
    pub scroll: f32,
}

pub(super) struct BuilderUi {
    /// Hidden tab that runs the built query and holds its result.
    pub tab: Uuid,
    pub connection: Uuid,
    pub query: VisualQuery,
    /// A box for each source of the query, in the same order.
    pub nodes: Vec<Node>,
    /// Columns of the tables, by `schema.table`, and the tables asked for.
    pub columns: HashMap<String, Vec<ColumnInfo>>,
    pub requested: HashSet<String>,
    /// Tables whose columns could not be read, with the error. They stay
    /// asked for, so they are not read again on every frame, until the
    /// person tries again on the canvas or puts the table on it again.
    failed: HashMap<String, String>,
    /// The builder's own text field that had the keyboard in the last frame:
    /// Mod+Enter typed in it runs the query.
    field_focus: Option<egui::Id>,
    pub page: BuilderPage,
    /// A relation being drawn from this column.
    dragging: Option<Field>,
    /// The relation whose menu is open.
    relation_menu: Option<usize>,
    table_search: String,
}

fn table_key(table: &TableRef) -> String {
    format!("{}.{}", table.schema, table.name)
}

impl BuilderUi {
    /// The names of the columns of a source, `*` first; only `*` while they
    /// are read, none for a source that is not there.
    fn column_names(&self, source: usize) -> impl Iterator<Item = &str> {
        let table = self.query.sources.get(source).map(|s| &s.table);
        table.map(|_| "*").into_iter().chain(
            table
                .and_then(|table| self.columns.get(&table_key(table)))
                .into_iter()
                .flatten()
                .map(|c| c.name.as_str()),
        )
    }
    /// The columns of a source, `*` first; only `*` while they are read.
    fn columns_of(&self, source: usize) -> Vec<String> {
        self.column_names(source).map(str::to_owned).collect()
    }
    /// The columns of the source have been read.
    fn has_columns(&self, source: usize) -> bool {
        self.query
            .sources
            .get(source)
            .is_some_and(|s| self.columns.contains_key(&table_key(&s.table)))
    }
    /// Why the columns of the source could not be read, if they could not.
    fn columns_failed(&self, source: usize) -> Option<&String> {
        let source = self.query.sources.get(source)?;
        self.failed.get(&table_key(&source.table))
    }
    /// Rows of the box of a source: its columns, or `*` with the line that
    /// says they are read or could not be read.
    fn box_rows(&self, source: usize) -> usize {
        if self.columns_failed(source).is_some() {
            3
        } else if self.has_columns(source) {
            self.column_names(source).count()
        } else {
            2
        }
    }
    /// Read the columns of the table again on the next frame, if reading
    /// them failed.
    fn retry_columns(&mut self, table: &TableRef) {
        let key = table_key(table);
        if self.failed.remove(&key).is_some() {
            self.requested.remove(&key);
        }
    }
    /// Note that one of the builder's own text fields has the keyboard.
    fn note_focus(&mut self, response: &egui::Response) {
        if response.has_focus() {
            self.field_focus = Some(response.id);
        }
    }
    fn label(&self, field: &Field) -> String {
        match self.query.sources.get(field.source) {
            Some(source) => format!("{}.{}", source.alias, field.column),
            None => "—".into(),
        }
    }
    /// Every column of every source, optionally without `*`.
    fn fields(&self, star: bool) -> Vec<Field> {
        (0..self.query.sources.len())
            .flat_map(|source| {
                self.columns_of(source)
                    .into_iter()
                    .filter(move |column| star || column != "*")
                    .map(move |column| Field { source, column })
            })
            .collect()
    }
    /// The first column that is not `*`, for a new item of a page.
    fn first_field(&self) -> Field {
        self.fields(false).into_iter().next().unwrap_or(Field {
            source: 0,
            column: "*".into(),
        })
    }
    /// Put a table on the canvas, at `at` or in the next free place.
    fn add_table(&mut self, table: TableRef, at: Option<egui::Pos2>) {
        let n = self.nodes.len();
        let pos = at.unwrap_or_else(|| {
            egui::pos2(
                24.0 + (n % 4) as f32 * (NODE_WIDTH + 70.0),
                24.0 + (n / 4) as f32 * (HEADER + ROW * MAX_ROWS as f32 + 60.0),
            )
        });
        // Putting a table on the canvas again also reads its columns again.
        self.retry_columns(&table);
        self.query.add_source(table);
        self.nodes.push(Node { pos, scroll: 0.0 });
    }
    fn remove_table(&mut self, index: usize) {
        self.query.remove_source(index);
        if index < self.nodes.len() {
            self.nodes.remove(index);
        }
        self.relation_menu = None;
        self.dragging = None;
    }
    /// The output of a column with no summary is ticked on the canvas.
    fn ticked(&self, field: &Field) -> bool {
        self.query
            .outputs
            .iter()
            .any(|o| o.field == *field && o.aggregate.is_none())
    }
    fn toggle(&mut self, field: Field) {
        if self.ticked(&field) {
            self.query
                .outputs
                .retain(|o| !(o.field == field && o.aggregate.is_none()));
        } else {
            self.query.outputs.push(Output {
                field,
                aggregate: None,
                alias: String::new(),
            });
        }
    }
}

/// What the canvas asks for once drawn.
enum CanvasAction {
    AddTable(TableRef, egui::Pos2),
    /// A column was dropped onto a column of its own table.
    SameTable,
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

    /// Put a table on the canvas of the builder and bring it forward. A
    /// builder of another connection starts over.
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
        if self
            .builder
            .as_ref()
            .is_none_or(|b| b.connection != connection)
        {
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
            self.builder = Some(BuilderUi {
                tab,
                connection,
                query: VisualQuery::default(),
                nodes: Vec::new(),
                columns: HashMap::new(),
                requested: HashSet::new(),
                failed: HashMap::new(),
                field_focus: None,
                page: BuilderPage::Select,
                dragging: None,
                relation_menu: None,
                table_search: String::new(),
            });
        }
        if let Some(b) = self.builder.as_mut() {
            if b.query
                .sources
                .iter()
                .any(|s| s.table.schema == table.schema && s.table.name == table.name)
            {
                // Chosen again: its columns are read again if they failed.
                b.retry_columns(&table);
            } else {
                b.add_table(table, None);
            }
        }
        self.section = Section::Builder;
        if let Some(index) = self.tabs.iter().position(|t| t.id == tab) {
            self.active = index;
        }
        if !self.databases.contains_key(&connection) {
            self.connect(connection);
        }
    }

    /// Ask for the columns of the tables on the canvas not read yet.
    fn request_builder_columns(&self, b: &mut BuilderUi) {
        let Some(db) = self.databases.get(&b.connection) else {
            return;
        };
        for source in &b.query.sources {
            let key = table_key(&source.table);
            if !b.requested.insert(key.clone()) {
                continue;
            }
            let (db, table, connection) = (db.clone(), source.table.clone(), b.connection);
            let tx = self.tx.clone();
            let ctx = self.ctx.clone();
            self.track(async move {
                let result = db.columns(&table).await.map_err(|e| e.to_string());
                let _ = tx
                    .send(Message::BuilderColumns(connection, key, result))
                    .await;
                ctx.request_repaint();
            });
        }
    }

    /// The columns of a table of the builder arrived.
    pub(super) fn builder_columns(
        &mut self,
        connection: Uuid,
        key: String,
        result: Result<Vec<ColumnInfo>, String>,
    ) {
        let Some(b) = self.builder.as_mut().filter(|b| b.connection == connection) else {
            return;
        };
        match result {
            Ok(columns) => {
                b.failed.remove(&key);
                b.columns.insert(key, columns);
            }
            Err(error) => {
                // The table stays asked for: it is read again only when the
                // person asks, so the error is shown once.
                b.failed.insert(key, error.clone());
                self.notify(error, true);
            }
        }
    }

    /// The SQL of the builder, formatted.
    fn builder_sql(b: &BuilderUi, dialect: &Dialect) -> anyhow::Result<String> {
        let text = b.query.sql(dialect)?;
        baza_core::format::beautify(dialect, &text)
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
        self.request_builder_columns(&mut b);
        let preview = Self::builder_sql(&b, &dialect);
        let busy = self.tabs[index].busy;
        let (mut run, mut open_editor, mut clear) = (false, false, false);
        widgets::toolbar(ui, &p, |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            run = Btn::primary(l("Uruchom", "Run"))
                .icon(Icon::Play)
                .enabled(preview.is_ok() && !busy)
                .tip(&format!(
                    "{} ({})",
                    l("Uruchom zapytanie", "Run the query"),
                    super::shortcuts::keys_text("Mod+Enter", en)
                ))
                .show(ui, &p)
                .clicked();
            open_editor = Btn::quiet(l("Otwórz w edytorze SQL", "Open in SQL editor"))
                .icon(Icon::Terminal)
                .enabled(preview.is_ok())
                .show(ui, &p)
                .clicked();
            clear = Btn::quiet(l("Wyczyść", "Clear"))
                .icon(Icon::Trash)
                .enabled(!b.query.sources.is_empty())
                .show(ui, &p)
                .clicked();
            widgets::right(ui, |ui| {
                ui.label(
                    RichText::new(l(
                        "Przeciągnij kolumnę na kolumnę innej tabeli, aby je połączyć.",
                        "Drag a column onto a column of another table to relate them.",
                    ))
                    .size(p.small())
                    .color(p.muted),
                );
            });
        });
        // Mod+Enter runs the query unless the result grid or its cell editor,
        // a dialog or a text field outside the builder has the keyboard: in
        // the grid it opens the larger cell editor.
        let grid_focused = ui.memory(|m| m.has_focus(egui::Id::new(("grid_keys", b.tab))));
        let own_field = b
            .field_focus
            .take()
            .is_some_and(|id| ui.memory(|m| m.has_focus(id)));
        let modal = ui.ctx().memory(|m| m.top_modal_layer()).is_some();
        let editing = self.cell_editor.as_ref().is_some_and(|e| e.tab == b.tab);
        let keys_free =
            !modal && !grid_focused && !editing && (own_field || !ui.ctx().text_edit_focused());
        if keys_free && !busy && preview.is_ok() && take_mod(ui, false, egui::Key::Enter) {
            run = true;
        }

        // The tables of the connection.
        let tables = self
            .tables
            .get(&b.connection)
            .map_or(&[][..], Vec::as_slice);
        let mut chosen_table = None;
        egui::Panel::left(egui::Id::new("builder_tables"))
            .resizable(true)
            .default_size(220.0)
            .size_range(150.0..=420.0)
            .frame(
                egui::Frame::NONE
                    .fill(p.side)
                    .inner_margin(egui::Margin::symmetric(8, 8)),
            )
            .show(ui, |ui| {
                chosen_table = table_list(ui, &p, en, &mut b, tables)
            });

        // The SQL, formatted.
        egui::Panel::right(egui::Id::new("builder_sql"))
            .resizable(true)
            .default_size(340.0)
            .size_range(220.0..=700.0)
            .frame(
                egui::Frame::NONE
                    .fill(p.side)
                    .inner_margin(egui::Margin::symmetric(12, 10)),
            )
            .show(ui, |ui| {
                widgets::caption(ui, &p, "SQL");
                ui.add_space(4.0);
                egui::ScrollArea::both()
                    .id_salt("builder_sql_text")
                    .auto_shrink([false, false])
                    .show(ui, |ui| match &preview {
                        Ok(sql) => {
                            let mut job = highlight(sql, p.code(), p);
                            job.wrap.max_width = f32::INFINITY;
                            ui.add(
                                egui::Label::new(job)
                                    .selectable(true)
                                    .wrap_mode(egui::TextWrapMode::Extend),
                            );
                        }
                        Err(error) => {
                            let text = crate::messages::describe(&error.to_string(), en);
                            widgets::feedback(ui, &p, &text, true);
                        }
                    });
            });

        // The clauses, and the result.
        let mut heading = None;
        egui::Panel::bottom(egui::Id::new("builder_clauses"))
            .resizable(true)
            .default_size(300.0)
            .size_range(220.0..=900.0)
            .frame(egui::Frame::NONE.fill(p.bg))
            .show(ui, |ui| {
                egui::Frame::NONE
                    .fill(p.side)
                    .inner_margin(egui::Margin::symmetric(12, 0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            for (page, label) in [
                                (BuilderPage::Select, "SELECT"),
                                (BuilderPage::From, "FROM"),
                                (BuilderPage::Where, "WHERE"),
                                (BuilderPage::Group, "GROUP BY"),
                                (BuilderPage::Having, "HAVING"),
                                (BuilderPage::Order, "ORDER BY"),
                                (BuilderPage::Limit, "LIMIT"),
                                (BuilderPage::Result, l("Wynik", "Result")),
                            ] {
                                if widgets::tab(
                                    ui,
                                    &p,
                                    None,
                                    label,
                                    b.page == page,
                                    12.0,
                                    true,
                                    p.control_height() + 4.0,
                                )
                                .clicked()
                                {
                                    b.page = page;
                                }
                            }
                        });
                    });
                widgets::hline(ui, &p);
                if b.page == BuilderPage::Result {
                    self.tab_error(ui, index);
                    heading = Some(self.results_heading(ui, index, false));
                    if self.tabs[index].result.is_some() {
                        // The pages decide the order; a header click does not.
                        self.results_grid(ui, index, false);
                    } else {
                        super::sql_view::hint(
                            ui,
                            &p,
                            l(
                                "Naciśnij „Uruchom”, aby zobaczyć wynik.",
                                "Press “Run” to see the result.",
                            ),
                        );
                    }
                } else {
                    egui::ScrollArea::vertical()
                        .id_salt("builder_page")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            egui::Frame::NONE
                                .inner_margin(egui::Margin::symmetric(14, 10))
                                .show(ui, |ui| clause_page(ui, &p, en, &mut b));
                        });
                }
            });

        // The canvas takes the rest.
        match canvas(ui, &p, en, &mut b) {
            Some(CanvasAction::AddTable(table, at)) => b.add_table(table, Some(at)),
            Some(CanvasAction::SameTable) => {
                self.notify("builder_relation_same_table".into(), true);
            }
            None => {}
        }
        if let Some(table) = chosen_table {
            b.add_table(table, None);
        }
        if clear {
            b.query = VisualQuery::default();
            b.nodes.clear();
            b.relation_menu = None;
        }
        let tabid = b.tab;
        let connection = b.connection;
        if run && preview.is_ok() {
            b.page = BuilderPage::Result;
        }
        self.builder = Some(b);
        if let Some(actions) = heading {
            if actions.insert {
                self.begin_insert(index);
            }
            if actions.delete {
                self.ask_delete(index);
            }
            if actions.export {
                self.export_result(tabid);
            }
        }
        if let Ok(sql) = preview {
            if run {
                let tab = &mut self.tabs[index];
                tab.connection = Some(connection);
                tab.sql = sql;
                self.run_tab(tabid, false);
            } else if open_editor {
                let title = self.tr("Kreator", "Builder").to_owned();
                self.open_in_editor(Some(connection), &title, sql);
            }
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
            .and_then(|id| self.tables.get(&id))
            .map_or(&[][..], Vec::as_slice);
        let mut chosen = None;
        let mut action = false;
        egui::Frame::NONE
            .inner_margin(egui::Margin::same(20))
            .show(ui, |ui| {
                let (text, label) = if !tables.is_empty() {
                    (
                        l(
                            "Kreator rysuje zapytanie SELECT z tabel połączonych relacjami: zaznaczasz kolumny, przeciągasz relacje i ustawiasz klauzule. Wybierz tabelę, od której zaczniesz.",
                            "The builder draws a SELECT from tables joined by relations: tick columns, drag relations and set the clauses. Choose the table to start from.",
                        ),
                        None,
                    )
                } else if self.profiles.connections.is_empty() {
                    (
                        l(
                            "Kreator rysuje zapytanie SELECT z tabel bazy. Najpierw dodaj połączenie z bazą.",
                            "The builder draws a SELECT from the tables of a database. Add a database connection first.",
                        ),
                        Some(l("Nowe połączenie", "New connection")),
                    )
                } else {
                    (
                        l(
                            "Kreator rysuje zapytanie SELECT z tabel bazy. Połącz się z bazą, aby wybrać tabelę.",
                            "The builder draws a SELECT from the tables of a database. Connect to a database to choose a table.",
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
                                    for table in tables {
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

/// The tables of the connection: a click puts one on the canvas, and so
/// does dragging it there.
fn table_list(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    en: bool,
    b: &mut BuilderUi,
    tables: &[TableRef],
) -> Option<TableRef> {
    let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
    let search = ui.add(
        widgets::field(p, &mut b.table_search)
            .hint_text(l("Szukaj tabeli", "Find a table"))
            .desired_width(f32::INFINITY),
    );
    b.note_focus(&search);
    ui.add_space(4.0);
    let needle = b.table_search.trim().to_lowercase();
    // The tables that match, by their place in the list; with nothing
    // typed, all of them, without a list of their own.
    let matching: Vec<usize> = if needle.is_empty() {
        Vec::new()
    } else {
        (0..tables.len())
            .filter(|&i| contains_folded(&tables[i].name, &needle))
            .collect()
    };
    let count = if needle.is_empty() {
        tables.len()
    } else {
        matching.len()
    };
    let mut chosen = None;
    // Only the rows on screen are laid out: a database may have thousands
    // of tables.
    ui.spacing_mut().item_spacing.y = 0.0;
    egui::ScrollArea::vertical()
        .id_salt("builder_table_list")
        .auto_shrink([false, false])
        .show_rows(ui, p.row_height(), count, |ui, rows| {
            for row in rows {
                let table = &tables[if needle.is_empty() {
                    row
                } else {
                    matching[row]
                }];
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), p.row_height()),
                    egui::Sense::click_and_drag(),
                );
                if response.hovered() {
                    ui.painter().rect_filled(rect, 4, p.raised_hover);
                }
                widgets::paint(
                    ui.painter(),
                    egui::Rect::from_center_size(
                        egui::pos2(rect.left() + 12.0, rect.center().y),
                        egui::vec2(13.0, 13.0),
                    ),
                    if table.kind.contains("view") {
                        Icon::View
                    } else {
                        Icon::Table
                    },
                    p.muted,
                );
                ui.painter().text(
                    egui::pos2(rect.left() + 26.0, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    &table.name,
                    egui::FontId::proportional(p.body()),
                    p.text,
                );
                response.dnd_set_drag_payload(table.clone());
                if response.double_clicked() {
                    chosen = Some(table.clone());
                }
                response.on_hover_text(l(
                    "Przeciągnij na płótno albo kliknij dwukrotnie",
                    "Drag onto the canvas or double-click",
                ));
            }
        });
    chosen
}

/// `name` contains `needle`, given in lower case, in any letter case.
fn contains_folded(name: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if name.is_ascii() && needle.is_ascii() {
        name.as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
    } else {
        name.to_lowercase().contains(needle)
    }
}

/// A short mark of a join, drawn in its circle.
fn join_mark(kind: JoinKind) -> &'static str {
    match kind {
        JoinKind::Inner => "I",
        JoinKind::Left => "L",
        JoinKind::Right => "R",
        JoinKind::Full => "F",
    }
}

/// The canvas: the tables as boxes, the relations as curves between their
/// columns.
fn canvas(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    en: bool,
    b: &mut BuilderUi,
) -> Option<CanvasAction> {
    let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
    let mut action = None;
    let rows_of = |b: &BuilderUi, i: usize| b.box_rows(i).max(1);
    let height_of = |rows: usize| HEADER + rows.min(MAX_ROWS) as f32 * ROW + 6.0;
    let extent = b
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| node.pos.to_vec2() + egui::vec2(NODE_WIDTH, height_of(rows_of(b, i))))
        .fold(egui::Vec2::ZERO, |a, c| a.max(c))
        + egui::vec2(300.0, 300.0);
    egui::ScrollArea::both()
        .id_salt("builder_canvas")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let size = extent.max(ui.available_size());
            let (area, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            let origin = area.min;
            let painter = ui.painter_at(area);
            // A dot grid under everything.
            let step = 24.0;
            let dot = theme::blend(p.bg, p.line, 0.6);
            let mut y = area.top() + step;
            while y < area.bottom() {
                let mut x = area.left() + step;
                while x < area.right() {
                    painter.circle_filled(egui::pos2(x, y), 1.0, dot);
                    x += step;
                }
                y += step;
            }
            if b.query.sources.is_empty() {
                painter.text(
                    area.min + egui::vec2(28.0, 28.0),
                    egui::Align2::LEFT_TOP,
                    l(
                        "Przeciągnij tabelę z listy po lewej albo kliknij ją dwukrotnie.",
                        "Drag a table from the list on the left, or double-click it.",
                    ),
                    egui::FontId::proportional(p.body()),
                    p.muted,
                );
            }
            // Geometry of the boxes and of their visible columns.
            let rect_of = |b: &BuilderUi, i: usize| {
                egui::Rect::from_min_size(
                    origin + b.nodes[i].pos.to_vec2(),
                    egui::vec2(NODE_WIDTH, height_of(rows_of(b, i))),
                )
            };
            // Where a column of a box is: its centre height, kept inside the box.
            let anchor_y = |b: &BuilderUi, field: &Field| {
                let rect = rect_of(b, field.source);
                let row = b
                    .column_names(field.source)
                    .position(|c| c == field.column)
                    .unwrap_or(0);
                let top = rect.top() + HEADER + 3.0;
                let bottom = rect.bottom() - 3.0;
                (top + row as f32 * ROW - b.nodes[field.source].scroll + ROW / 2.0)
                    .clamp(top + ROW / 2.0, bottom - ROW / 2.0)
            };
            let anchor = |b: &BuilderUi, field: &Field, toward: f32| {
                let rect = rect_of(b, field.source);
                let x = if toward > rect.center().x {
                    rect.right()
                } else {
                    rect.left()
                };
                egui::pos2(x, anchor_y(b, field))
            };
            let curve =
                |painter: &egui::Painter, a: egui::Pos2, z: egui::Pos2, stroke: egui::Stroke| {
                    let bend = ((z.x - a.x).abs() * 0.5).max(40.0);
                    let (da, dz) = if a.x <= z.x {
                        (bend, -bend)
                    } else {
                        (-bend, bend)
                    };
                    painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
                        [a, a + egui::vec2(da, 0.0), z + egui::vec2(dz, 0.0), z],
                        false,
                        egui::Color32::TRANSPARENT,
                        stroke,
                    ));
                };
            // The relations, under the boxes.
            let mut marks = Vec::new();
            for (k, relation) in b.query.relations.iter().enumerate() {
                if relation.left.source >= b.nodes.len() || relation.right.source >= b.nodes.len() {
                    continue;
                }
                let a = anchor(
                    b,
                    &relation.left,
                    rect_of(b, relation.right.source).center().x,
                );
                let z = anchor(
                    b,
                    &relation.right,
                    rect_of(b, relation.left.source).center().x,
                );
                curve(&painter, a, z, egui::Stroke::new(1.6, p.accent));
                marks.push((k, a + (z - a) / 2.0, relation.kind));
            }
            // The boxes.
            let mut rows_hit: Vec<(egui::Rect, Field)> = Vec::new();
            let mut remove = None;
            let mut retry = None;
            let pointer = ui.input(|i| i.pointer.hover_pos());
            for i in 0..b.nodes.len() {
                let rect = rect_of(b, i);
                let columns = b.columns_of(i);
                let header = egui::Rect::from_min_size(rect.min, egui::vec2(NODE_WIDTH, HEADER));
                painter.rect_filled(rect, 6, p.raised);
                painter.rect_filled(
                    header,
                    egui::CornerRadius {
                        nw: 6,
                        ne: 6,
                        sw: 0,
                        se: 0,
                    },
                    theme::blend(p.raised, p.accent, 0.45),
                );
                painter.rect_stroke(
                    rect,
                    6,
                    egui::Stroke::new(1.0, p.line),
                    egui::StrokeKind::Inside,
                );
                let source = &b.query.sources[i];
                let title = if source.alias == source.table.name {
                    source.alias.clone()
                } else {
                    format!("{} ({})", source.alias, source.table.name)
                };
                painter.text(
                    header.left_center() + egui::vec2(10.0, 0.0),
                    egui::Align2::LEFT_CENTER,
                    title,
                    theme::medium(p.body()),
                    p.text,
                );
                let close = egui::Rect::from_center_size(
                    header.right_center() - egui::vec2(14.0, 0.0),
                    egui::vec2(16.0, 16.0),
                );
                let close_response = ui
                    .interact(close, ui.id().with(("node_close", i)), egui::Sense::click())
                    .on_hover_text(l("Usuń z zapytania", "Remove from the query"));
                widgets::paint(
                    &painter,
                    close.shrink(2.0),
                    Icon::Close,
                    if close_response.hovered() {
                        p.text
                    } else {
                        p.muted
                    },
                );
                if close_response.clicked() {
                    remove = Some(i);
                }
                // The title moves the box.
                let drag = ui.interact(
                    header.with_max_x(close.left() - 2.0),
                    ui.id().with(("node_move", i)),
                    egui::Sense::drag(),
                );
                if drag.dragged() {
                    let node = &mut b.nodes[i];
                    node.pos = (node.pos + drag.drag_delta()).max(egui::Pos2::ZERO);
                }
                if drag.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                }
                // The columns, scrolled inside the box when there are many.
                let body = egui::Rect::from_min_max(
                    egui::pos2(rect.left(), header.bottom() + 3.0),
                    egui::pos2(rect.right(), rect.bottom() - 3.0),
                );
                let most = ((columns.len() as f32 - MAX_ROWS as f32) * ROW).max(0.0);
                if pointer.is_some_and(|at| body.contains(at)) && most > 0.0 {
                    let delta = ui.input(|i| i.smooth_scroll_delta.y);
                    if delta != 0.0 {
                        b.nodes[i].scroll = (b.nodes[i].scroll - delta).clamp(0.0, most);
                        // The canvas does not scroll with it.
                        ui.input_mut(|i| i.smooth_scroll_delta = egui::Vec2::ZERO);
                    }
                }
                let scroll = b.nodes[i].scroll.min(most);
                let clip = painter.with_clip_rect(body);
                // Under `*`: the columns being read, or why they could not
                // be, with a way to read them again.
                let note = |row: usize, text: &str, color| {
                    clip.text(
                        body.left_top() + egui::vec2(10.0, ROW * (row as f32 + 0.5)),
                        egui::Align2::LEFT_CENTER,
                        text,
                        egui::FontId::proportional(p.small()),
                        color,
                    );
                };
                if let Some(error) = b.columns_failed(i) {
                    let lines = egui::Rect::from_min_size(
                        body.left_top() + egui::vec2(0.0, ROW),
                        egui::vec2(NODE_WIDTH, ROW * 2.0),
                    )
                    .intersect(body);
                    let response = ui
                        .interact(lines, ui.id().with(("node_retry", i)), egui::Sense::click())
                        .on_hover_text(crate::messages::describe(error, en));
                    note(
                        1,
                        l(
                            "Nie udało się wczytać kolumn.",
                            "The columns could not be read.",
                        ),
                        p.error,
                    );
                    note(
                        2,
                        l("Kliknij, aby spróbować ponownie.", "Click to try again."),
                        if response.hovered() { p.text } else { p.accent },
                    );
                    if response.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if response.clicked() {
                        retry = Some(b.query.sources[i].table.clone());
                    }
                } else if !b.has_columns(i) {
                    note(1, l("wczytywanie kolumn…", "loading the columns…"), p.muted);
                }
                for (row, column) in columns.iter().enumerate() {
                    let top = body.top() + row as f32 * ROW - scroll;
                    if top + ROW < body.top() || top > body.bottom() {
                        continue;
                    }
                    let line = egui::Rect::from_min_size(
                        egui::pos2(body.left(), top),
                        egui::vec2(NODE_WIDTH, ROW),
                    );
                    let visible = line.intersect(body);
                    let field = Field {
                        source: i,
                        column: column.clone(),
                    };
                    let response = ui.interact(
                        visible,
                        ui.id().with(("node_row", i, row)),
                        egui::Sense::click_and_drag(),
                    );
                    if response.hovered() {
                        clip.rect_filled(line.shrink2(egui::vec2(3.0, 0.0)), 3, p.raised_hover);
                    }
                    let mark = egui::Rect::from_center_size(
                        egui::pos2(line.left() + 14.0, line.center().y),
                        egui::vec2(12.0, 12.0),
                    );
                    if b.ticked(&field) {
                        clip.rect_filled(mark, 3, p.accent);
                        widgets::paint(&clip, mark.shrink(2.0), Icon::Check, p.bg);
                    } else {
                        clip.rect_stroke(
                            mark,
                            3,
                            egui::Stroke::new(1.0, p.muted),
                            egui::StrokeKind::Inside,
                        );
                    }
                    clip.text(
                        egui::pos2(line.left() + 28.0, line.center().y),
                        egui::Align2::LEFT_CENTER,
                        column,
                        egui::FontId::monospace(p.code() - 1.5),
                        if column == "*" { p.muted } else { p.text },
                    );
                    if response.clicked() {
                        b.toggle(field.clone());
                    }
                    // A column dragged onto another table relates the two.
                    if column != "*" {
                        if response.drag_started() {
                            b.dragging = Some(field.clone());
                        }
                        rows_hit.push((visible, field));
                    }
                }
            }
            // The relation being drawn follows the pointer.
            if let Some(from) = b.dragging.clone() {
                if let Some(at) = pointer {
                    let start = anchor(b, &from, at.x);
                    curve(
                        &painter,
                        start,
                        at,
                        egui::Stroke::new(1.6, p.accent.gamma_multiply(0.7)),
                    );
                }
                if ui.input(|i| i.pointer.any_released()) {
                    let target = pointer.and_then(|at| {
                        rows_hit
                            .iter()
                            .find(|(rect, _)| rect.contains(at))
                            .map(|(_, field)| field.clone())
                    });
                    match target {
                        // A table is joined to itself through a second copy
                        // of it; dropping onto its own box says so.
                        Some(to) if to.source == from.source => {
                            if to != from {
                                action = Some(CanvasAction::SameTable);
                            }
                        }
                        Some(to) => b.query.relations.push(Relation {
                            kind: JoinKind::Inner,
                            left: from,
                            right: to,
                        }),
                        None => {}
                    }
                    b.dragging = None;
                }
            }
            // The marks of the relations, over the boxes: a click opens the menu.
            for (k, at, kind) in marks {
                let circle = egui::Rect::from_center_size(at, egui::vec2(20.0, 20.0));
                let response = ui
                    .interact(circle, ui.id().with(("relation", k)), egui::Sense::click())
                    .on_hover_text(kind.sql());
                painter.circle_filled(at, 10.0, p.bg);
                painter.circle_stroke(
                    at,
                    10.0,
                    egui::Stroke::new(1.6, if response.hovered() { p.text } else { p.accent }),
                );
                painter.text(
                    at,
                    egui::Align2::CENTER_CENTER,
                    join_mark(kind),
                    egui::FontId::proportional(p.small()),
                    p.text,
                );
                if response.clicked() {
                    b.relation_menu = Some(k);
                }
                if b.relation_menu == Some(k) {
                    relation_menu(ui, p, en, b, k, at);
                }
            }
            // A table dropped from the list.
            if let Some(at) = pointer
                && area.contains(at)
                && ui.input(|i| i.pointer.any_released())
                && let Some(table) = egui::DragAndDrop::take_payload::<TableRef>(ui.ctx())
            {
                let pos = (at - origin - egui::vec2(NODE_WIDTH / 2.0, HEADER / 2.0))
                    .max(egui::Vec2::ZERO);
                action = Some(CanvasAction::AddTable((*table).clone(), pos.to_pos2()));
            }
            if let Some(i) = remove {
                b.remove_table(i);
            }
            if let Some(table) = retry {
                b.retry_columns(&table);
            }
        });
    action
}

/// The menu of a relation: its kind of join, or removing it.
fn relation_menu(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    en: bool,
    b: &mut BuilderUi,
    k: usize,
    at: egui::Pos2,
) {
    let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
    let Some(relation) = b.query.relations.get(k).cloned() else {
        b.relation_menu = None;
        return;
    };
    let mut close = false;
    let shown = egui::Area::new(ui.id().with("relation_menu"))
        .order(egui::Order::Foreground)
        .fixed_pos(at + egui::vec2(14.0, 14.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::NONE
                .fill(p.bg)
                .stroke(egui::Stroke::new(1.0, p.line))
                .corner_radius(6)
                .inner_margin(egui::Margin::same(8))
                .shadow(ui.ctx().global_style().visuals.popup_shadow)
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} = {}",
                            b.label(&relation.left),
                            b.label(&relation.right)
                        ))
                        .monospace()
                        .size(p.code() - 1.5)
                        .color(p.muted),
                    );
                    for kind in JoinKind::ALL {
                        let text = match kind {
                            JoinKind::Inner => l(
                                "Tylko pasujące wiersze (INNER)",
                                "Matching rows only (INNER)",
                            ),
                            JoinKind::Left => l(
                                "Wszystkie wiersze lewej tabeli (LEFT)",
                                "All rows of the left table (LEFT)",
                            ),
                            JoinKind::Right => l(
                                "Wszystkie wiersze prawej tabeli (RIGHT)",
                                "All rows of the right table (RIGHT)",
                            ),
                            JoinKind::Full => l(
                                "Wszystkie wiersze obu tabel (FULL)",
                                "All rows of both tables (FULL)",
                            ),
                        };
                        if ui.selectable_label(relation.kind == kind, text).clicked() {
                            b.query.relations[k].kind = kind;
                            close = true;
                        }
                    }
                    ui.separator();
                    if ui
                        .button(l("Usuń relację", "Delete the relation"))
                        .clicked()
                    {
                        b.query.relations.remove(k);
                        close = true;
                    }
                });
        });
    let outside = ui.input(|i| i.pointer.any_pressed())
        && ui
            .input(|i| i.pointer.interact_pos())
            .is_some_and(|pos| !shown.response.rect.contains(pos) && (pos - at).length() > 12.0);
    if close || outside || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        b.relation_menu = None;
    }
}

/// A choice of a column of the query, optionally without `*` and without
/// the columns of one source. The choices are listed only while the list is
/// open, so a closed choice costs nothing per frame.
#[allow(clippy::too_many_arguments)]
fn field_combo(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    id: impl std::hash::Hash + std::fmt::Debug,
    b: &BuilderUi,
    field: &mut Field,
    star: bool,
    other_than: Option<usize>,
    width: f32,
) {
    widgets::combo(ui, p, egui::Id::new(id), b.label(field), width, |ui| {
        egui::ScrollArea::vertical()
            .max_height(300.0)
            .show(ui, |ui| {
                for source in (0..b.query.sources.len()).filter(|s| Some(*s) != other_than) {
                    let alias = &b.query.sources[source].alias;
                    for column in b.column_names(source).filter(|c| star || *c != "*") {
                        let selected = field.source == source && field.column == column;
                        if ui
                            .selectable_label(selected, format!("{alias}.{column}"))
                            .clicked()
                        {
                            *field = Field {
                                source,
                                column: column.to_owned(),
                            };
                        }
                    }
                }
            });
    });
}

/// A choice of a summary, or none.
fn aggregate_combo(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    id: impl std::hash::Hash + std::fmt::Debug,
    value: &mut Option<Aggregate>,
    none: &str,
) {
    let shown = value.map_or(none.to_owned(), |a| a.label().to_owned());
    widgets::combo(ui, p, egui::Id::new(id), shown, 140.0, |ui| {
        ui.selectable_value(value, None, none);
        for aggregate in Aggregate::ALL {
            ui.selectable_value(value, Some(aggregate), aggregate.label());
        }
    });
}

/// Buttons that move or remove the item at `index` of a list; returns what
/// was asked: -1 up, 1 down, 0 remove.
fn row_tools(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    en: bool,
    index: usize,
    len: usize,
) -> Option<i8> {
    let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
    let mut asked = None;
    ui.add_enabled_ui(index > 0, |ui| {
        if widgets::icon_button(ui, p, Icon::ArrowUp, l("W górę", "Up")).clicked() {
            asked = Some(-1);
        }
    });
    ui.add_enabled_ui(index + 1 < len, |ui| {
        if widgets::icon_button(ui, p, Icon::ArrowDown, l("W dół", "Down")).clicked() {
            asked = Some(1);
        }
    });
    if widgets::icon_button(ui, p, Icon::Minus, l("Usuń", "Delete")).clicked() {
        asked = Some(0);
    }
    asked
}

fn apply_tool<T>(items: &mut Vec<T>, index: usize, asked: Option<i8>) {
    match asked {
        Some(0) => {
            items.remove(index);
        }
        Some(-1) if index > 0 => items.swap(index, index - 1),
        Some(1) if index + 1 < items.len() => items.swap(index, index + 1),
        _ => {}
    }
}

fn add_button(ui: &mut egui::Ui, p: &theme::Palette, text: &str, enabled: bool) -> bool {
    Btn::quiet(text)
        .icon(Icon::Plus)
        .small()
        .enabled(enabled)
        .show(ui, p)
        .clicked()
}

/// One page of the clauses.
fn clause_page(ui: &mut egui::Ui, p: &theme::Palette, en: bool, b: &mut BuilderUi) {
    let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
    ui.spacing_mut().item_spacing.y = 6.0;
    let any = !b.query.sources.is_empty();
    if !any {
        widgets::help(
            ui,
            p,
            l(
                "Najpierw dodaj tabelę na płótno.",
                "Put a table on the canvas first.",
            ),
        );
        return;
    }
    match b.page {
        BuilderPage::Select => {
            widgets::checkbox(
                ui,
                p,
                &mut b.query.distinct,
                l("Bez powtórzeń (DISTINCT)", "Without duplicates (DISTINCT)"),
            );
            if b.query.outputs.is_empty() {
                widgets::help(
                    ui,
                    p,
                    l(
                        "Nic nie wybrano: wynik ma wszystkie kolumny (*).",
                        "Nothing chosen: the result has every column (*).",
                    ),
                );
            }
            let len = b.query.outputs.len();
            let mut tool = None;
            for index in 0..len {
                let mut output = b.query.outputs[index].clone();
                ui.horizontal(|ui| {
                    field_combo(
                        ui,
                        p,
                        ("out_field", index),
                        b,
                        &mut output.field,
                        true,
                        None,
                        260.0,
                    );
                    aggregate_combo(
                        ui,
                        p,
                        ("out_aggregate", index),
                        &mut output.aggregate,
                        l("bez podsumowania", "no summary"),
                    );
                    let alias = ui.add(
                        widgets::field(p, &mut output.alias)
                            .desired_width(140.0)
                            .hint_text(l("alias", "alias")),
                    );
                    b.note_focus(&alias);
                    if let Some(asked) = row_tools(ui, p, en, index, len) {
                        tool = Some((index, asked));
                    }
                });
                b.query.outputs[index] = output;
            }
            if let Some((index, asked)) = tool {
                apply_tool(&mut b.query.outputs, index, Some(asked));
            }
            if add_button(ui, p, l("Dodaj kolumnę", "Add a column"), true) {
                let field = b.first_field();
                b.query.outputs.push(Output {
                    field,
                    aggregate: None,
                    alias: String::new(),
                });
            }
        }
        BuilderPage::From => {
            widgets::caption(ui, p, l("Tabele", "Tables"));
            let mut remove = None;
            for index in 0..b.query.sources.len() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "{}.{}",
                            b.query.sources[index].table.schema, b.query.sources[index].table.name
                        ))
                        .color(p.muted),
                    );
                    ui.label(RichText::new("AS").color(p.keyword));
                    let alias = ui.add(
                        widgets::field(p, &mut b.query.sources[index].alias).desired_width(160.0),
                    );
                    b.note_focus(&alias);
                    if widgets::icon_button(
                        ui,
                        p,
                        Icon::Minus,
                        l("Usuń tabelę", "Remove the table"),
                    )
                    .clicked()
                    {
                        remove = Some(index);
                    }
                });
            }
            if let Some(index) = remove {
                b.remove_table(index);
            }
            ui.add_space(6.0);
            widgets::caption(ui, p, l("Relacje", "Relations"));
            let len = b.query.relations.len();
            let mut tool = None;
            for index in 0..len {
                let mut relation = b.query.relations[index].clone();
                // The two sides are columns of two tables: each list leaves
                // out the table of the other side.
                let (left_source, right_source) = (relation.left.source, relation.right.source);
                ui.horizontal(|ui| {
                    field_combo(
                        ui,
                        p,
                        ("rel_left", index),
                        b,
                        &mut relation.left,
                        false,
                        Some(right_source),
                        190.0,
                    );
                    let shown = relation.kind.sql();
                    widgets::combo(
                        ui,
                        p,
                        egui::Id::new(("rel_kind", index)),
                        shown,
                        130.0,
                        |ui| {
                            for kind in JoinKind::ALL {
                                ui.selectable_value(&mut relation.kind, kind, kind.sql());
                            }
                        },
                    );
                    field_combo(
                        ui,
                        p,
                        ("rel_right", index),
                        b,
                        &mut relation.right,
                        false,
                        Some(left_source),
                        190.0,
                    );
                    if let Some(asked) = row_tools(ui, p, en, index, len) {
                        tool = Some((index, asked));
                    }
                });
                if relation.left.source == relation.right.source {
                    widgets::feedback(
                        ui,
                        p,
                        &crate::messages::describe("builder_relation_same_table", en),
                        true,
                    );
                }
                b.query.relations[index] = relation;
            }
            if let Some((index, asked)) = tool {
                apply_tool(&mut b.query.relations, index, Some(asked));
            }
            // A new relation starts between the first columns of two tables,
            // so it waits until the columns of two of them are read.
            let mut firsts = (0..b.query.sources.len()).filter_map(|source| {
                b.column_names(source)
                    .find(|column| *column != "*")
                    .map(|column| Field {
                        source,
                        column: column.to_owned(),
                    })
            });
            let (first, second) = (firsts.next(), firsts.next());
            if add_button(
                ui,
                p,
                l("Dodaj relację", "Add a relation"),
                second.is_some(),
            ) && let (Some(left), Some(right)) = (first, second)
            {
                b.query.relations.push(Relation {
                    kind: JoinKind::Inner,
                    left,
                    right,
                });
            }
        }
        BuilderPage::Where | BuilderPage::Having => {
            let having = b.page == BuilderPage::Having;
            let mut list = if having {
                std::mem::take(&mut b.query.having)
            } else {
                std::mem::take(&mut b.query.conditions)
            };
            let len = list.len();
            let mut tool = None;
            for (index, condition) in list.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    // The number the grouping below refers to.
                    ui.add_sized(
                        [16.0, p.control_height()],
                        egui::Label::new(
                            RichText::new(format!("{}", index + 1))
                                .monospace()
                                .color(p.muted),
                        ),
                    );
                    if index == 0 {
                        ui.add_space(64.0);
                    } else {
                        let shown = if condition.or { "OR" } else { "AND" };
                        widgets::combo(
                            ui,
                            p,
                            egui::Id::new(("cond_join", having, index)),
                            shown,
                            60.0,
                            |ui| {
                                ui.selectable_value(&mut condition.or, false, "AND");
                                ui.selectable_value(&mut condition.or, true, "OR");
                            },
                        );
                    }
                    if having {
                        aggregate_combo(
                            ui,
                            p,
                            ("cond_aggregate", index),
                            &mut condition.aggregate,
                            "—",
                        );
                    }
                    field_combo(
                        ui,
                        p,
                        ("cond_field", having, index),
                        b,
                        &mut condition.field,
                        having,
                        None,
                        170.0,
                    );
                    widgets::combo(
                        ui,
                        p,
                        egui::Id::new(("cond_op", having, index)),
                        condition.operator.sql(),
                        100.0,
                        |ui| {
                            for operator in Operator::ALL {
                                ui.selectable_value(
                                    &mut condition.operator,
                                    operator,
                                    operator.sql(),
                                );
                            }
                        },
                    );
                    if condition.operator.takes_value() {
                        let hint = match condition.operator {
                            Operator::In | Operator::NotIn => "1, 2, 3",
                            Operator::Between => "1 AND 10",
                            Operator::Like | Operator::NotLike => "'%text%'",
                            _ => l(
                                "wartość albo kolumna, np. 5, 'tekst'",
                                "value or column, e.g. 5, 'text'",
                            ),
                        };
                        let value = ui.add(
                            widgets::field(p, &mut condition.value)
                                .desired_width(180.0)
                                .hint_text(hint),
                        );
                        b.note_focus(&value);
                    }
                    if let Some(asked) = row_tools(ui, p, en, index, len) {
                        tool = Some((index, asked));
                    }
                });
            }
            if let Some((index, asked)) = tool {
                apply_tool(&mut list, index, Some(asked));
            }
            if add_button(ui, p, l("Dodaj warunek", "Add a condition"), true) {
                let field = if having {
                    Field {
                        source: 0,
                        column: "*".into(),
                    }
                } else {
                    b.first_field()
                };
                list.push(Condition {
                    or: false,
                    field,
                    aggregate: having.then_some(Aggregate::Count),
                    operator: if having {
                        Operator::Greater
                    } else {
                        Operator::Equal
                    },
                    value: String::new(),
                });
            }
            // Where AND meets OR, the order in which they are joined is
            // spelled out, the way the SQL writes it.
            let joined_by = |or: bool| list.iter().skip(1).any(|c| c.or == or);
            if joined_by(true) && joined_by(false) {
                widgets::help(
                    ui,
                    p,
                    &format!(
                        "{} {}.",
                        l(
                            "Warunki łączą się po kolei, od góry:",
                            "The conditions are joined in turn, from the top:",
                        ),
                        grouping(&list)
                    ),
                );
            }
            widgets::help(
                ui,
                p,
                l(
                    "Wartość to SQL: liczba, tekst w apostrofach albo inna kolumna (alias.kolumna). Średniki, komentarze i podzapytania nie są dozwolone.",
                    "A value is SQL: a number, text in quotes or another column (alias.column). Semicolons, comments and subqueries are not allowed.",
                ),
            );
            if having {
                b.query.having = list;
            } else {
                b.query.conditions = list;
            }
        }
        BuilderPage::Group => {
            let len = b.query.group_by.len();
            let mut tool = None;
            for index in 0..len {
                let mut field = b.query.group_by[index].clone();
                ui.horizontal(|ui| {
                    field_combo(ui, p, ("group", index), b, &mut field, false, None, 260.0);
                    if let Some(asked) = row_tools(ui, p, en, index, len) {
                        tool = Some((index, asked));
                    }
                });
                b.query.group_by[index] = field;
            }
            if let Some((index, asked)) = tool {
                apply_tool(&mut b.query.group_by, index, Some(asked));
            }
            ui.horizontal(|ui| {
                if add_button(ui, p, l("Dodaj kolumnę", "Add a column"), true) {
                    let field = b.first_field();
                    b.query.group_by.push(field);
                }
                // What a summary needs: the other columns of the result.
                if Btn::quiet(l(
                    "Z kolumn wyniku bez podsumowania",
                    "From the result columns without a summary",
                ))
                .small()
                .show(ui, p)
                .clicked()
                {
                    b.query.group_by = b
                        .query
                        .outputs
                        .iter()
                        .filter(|o| o.aggregate.is_none() && o.field.column != "*")
                        .map(|o| o.field.clone())
                        .collect();
                }
            });
        }
        BuilderPage::Order => {
            let len = b.query.order_by.len();
            let mut tool = None;
            for index in 0..len {
                let mut term = b.query.order_by[index].clone();
                ui.horizontal(|ui| {
                    aggregate_combo(ui, p, ("order_aggregate", index), &mut term.aggregate, "—");
                    field_combo(
                        ui,
                        p,
                        ("order_field", index),
                        b,
                        &mut term.field,
                        term.aggregate.is_some(),
                        None,
                        260.0,
                    );
                    let shown = if term.descending { "DESC" } else { "ASC" };
                    widgets::combo(
                        ui,
                        p,
                        egui::Id::new(("order_way", index)),
                        shown,
                        80.0,
                        |ui| {
                            ui.selectable_value(&mut term.descending, false, "ASC");
                            ui.selectable_value(&mut term.descending, true, "DESC");
                        },
                    );
                    if let Some(asked) = row_tools(ui, p, en, index, len) {
                        tool = Some((index, asked));
                    }
                });
                b.query.order_by[index] = term;
            }
            if let Some((index, asked)) = tool {
                apply_tool(&mut b.query.order_by, index, Some(asked));
            }
            if add_button(ui, p, l("Dodaj sortowanie", "Add an order"), true) {
                let field = b.first_field();
                b.query.order_by.push(OrderTerm {
                    field,
                    aggregate: None,
                    descending: false,
                });
            }
        }
        BuilderPage::Limit => {
            widgets::form_row(ui, p, 160.0, l("Najwięcej wierszy", "Most rows"), |ui| {
                let limit = ui.add(
                    widgets::field(p, &mut b.query.limit)
                        .desired_width(120.0)
                        .hint_text(l("bez limitu", "no limit")),
                );
                b.note_focus(&limit);
            });
            widgets::form_row(ui, p, 160.0, l("Pomiń wiersze", "Skip rows"), |ui| {
                let offset = ui.add(
                    widgets::field(p, &mut b.query.offset)
                        .desired_width(120.0)
                        .hint_text("0"),
                );
                b.note_focus(&offset);
            });
        }
        BuilderPage::Result => {}
    }
}
