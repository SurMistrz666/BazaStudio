//! The table designer, a tab of the work area: the fields of a
//! new or an existing table in a grid (name, type, length, decimals, NOT
//! NULL, key, comment), the default value and auto increment of the selected
//! one below, and the SQL that saving will run.
use super::shortcuts::take_mod;
use super::*;
use crate::widgets::Btn;
use baza_core::ddl::{
    self, DdlFlavor, FieldDraft,
    mysql::{Restore, TableExtras},
};
use egui_extras::{Column, TableBuilder};

/// What the designer shows under its toolbar.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DesignPage {
    Fields,
    // The parts of a MySQL or MariaDB table beyond its columns.
    Indexes,
    ForeignKeys,
    Checks,
    Triggers,
    Options,
    Partitions,
    Sql,
}

pub(super) struct DesignTab {
    pub id: Uuid,
    pub connection: Uuid,
    /// The table; its name is typed for a new one.
    pub table: TableRef,
    pub new: bool,
    pub fields: Vec<FieldDraft>,
    /// The columns as the database has them, for `ALTER TABLE`.
    pub original: Vec<FieldDraft>,
    pub selected: usize,
    pub page: DesignPage,
    /// Indexes, constraints, triggers, options and partitions, as edited
    /// and as the database has them (MySQL and MariaDB).
    pub extras: TableExtras,
    pub original_extras: TableExtras,
    /// What the server offers for the options, and columns of other tables.
    pub catalog: super::design_extras::Catalog,
    /// The item chosen on a page of the parts beyond the columns.
    pub item: usize,
    pub loading: bool,
    /// The parts beyond the columns are being read; saving waits for them,
    /// since what it drops is worked out from them.
    pub extras_loading: bool,
    /// A save stopped halfway: the next reading of the parts beyond the
    /// columns refreshes only what the database has, and what was edited
    /// stays to be saved again.
    pub keep_extras: bool,
    pub saving: bool,
    pub request: Uuid,
}

/// What saving a design runs: its statements in order, and what puts back
/// the parts they drop and add again when one in between fails.
pub(super) struct SavePlan {
    pub statements: Vec<String>,
    pub restores: Vec<Restore>,
}

/// What the confirmation before saving looks for in the statements: a
/// dropped column, index, key, constraint or trigger, another engine (one
/// such as BLACKHOLE keeps no rows), and partitions made or taken away,
/// which rebuild the table.
const DESTRUCTIVE: [&str; 9] = [
    " DROP COLUMN ",
    " DROP INDEX ",
    " DROP FOREIGN KEY ",
    " DROP CONSTRAINT ",
    " DROP CHECK ",
    " DROP PRIMARY KEY",
    " ENGINE=",
    " PARTITION BY ",
    " REMOVE PARTITIONING",
];

/// Saving asks first: it removes or rebuilds part of the table, or takes
/// values from an ENUM or a SET field (a server that is not strict empties
/// the rows that hold one).
fn needs_confirmation(statements: &[String], before: &[FieldDraft], after: &[FieldDraft]) -> bool {
    let removes = statements.iter().any(|statement| {
        statement.starts_with("DROP TRIGGER ")
            || statement.starts_with("ALTER TABLE ")
                && DESTRUCTIVE.iter().any(|action| statement.contains(action))
    });
    removes
        || after.iter().any(|field| {
            let Some(old) = field
                .original
                .as_deref()
                .and_then(|original| before.iter().find(|f| f.name == original))
            else {
                return false;
            };
            is_choice(&old.data_type)
                && is_choice(&field.data_type)
                && old.values.iter().any(|value| !field.values.contains(value))
        })
}

/// How the database of a connection writes tables.
pub(super) fn flavor_of(engine: baza_core::engine::Engine) -> DdlFlavor {
    use baza_core::drivers::{mssql, mysql, postgres, sqlite};
    match engine.id() {
        id if id == mysql::ENGINE.id() => DdlFlavor::MySql,
        id if id == postgres::ENGINE.id() => DdlFlavor::Postgres,
        id if id == sqlite::ENGINE.id() => DdlFlavor::Sqlite,
        id if id == mssql::ENGINE.id() => DdlFlavor::SqlServer,
        _ => DdlFlavor::Generic,
    }
}

/// Types offered in the list of the type column.
fn type_choices(flavor: DdlFlavor) -> &'static [&'static str] {
    match flavor {
        DdlFlavor::MySql => &[
            "int",
            "bigint",
            "tinyint",
            "smallint",
            "mediumint",
            "decimal",
            "float",
            "double",
            "varchar",
            "char",
            "text",
            "mediumtext",
            "longtext",
            "date",
            "datetime",
            "timestamp",
            "time",
            "year",
            "json",
            "blob",
            "longblob",
            "binary",
            "varbinary",
            "enum",
            "set",
        ],
        DdlFlavor::Postgres => &[
            "integer",
            "bigint",
            "smallint",
            "numeric",
            "real",
            "double precision",
            "boolean",
            "varchar",
            "char",
            "text",
            "date",
            "timestamp",
            "timestamptz",
            "time",
            "uuid",
            "jsonb",
            "json",
            "bytea",
        ],
        DdlFlavor::Sqlite => &["INTEGER", "TEXT", "REAL", "NUMERIC", "BLOB"],
        DdlFlavor::SqlServer => &[
            "int",
            "bigint",
            "smallint",
            "tinyint",
            "bit",
            "decimal",
            "float",
            "nvarchar",
            "varchar",
            "nchar",
            "char",
            "date",
            "datetime2",
            "datetimeoffset",
            "time",
            "uniqueidentifier",
            "varbinary",
        ],
        DdlFlavor::Generic => &[
            "integer",
            "bigint",
            "varchar",
            "text",
            "decimal",
            "date",
            "timestamp",
        ],
    }
}

/// An ENUM or a SET: its values are chosen from a list.
fn is_choice(data_type: &str) -> bool {
    let name = data_type.trim();
    name.eq_ignore_ascii_case("enum") || name.eq_ignore_ascii_case("set")
}

/// The first field of a new table: a numbered key.
fn key_field(flavor: DdlFlavor) -> FieldDraft {
    FieldDraft {
        name: "id".into(),
        data_type: match flavor {
            DdlFlavor::Postgres => "bigint",
            DdlFlavor::Sqlite => "INTEGER",
            _ => "int",
        }
        .into(),
        not_null: true,
        primary_key: true,
        auto_increment: true,
        ..Default::default()
    }
}

/// A MySQL default as information_schema gives it, written back as SQL:
/// MySQL 8 leaves text without its quotes, MariaDB keeps them.
fn mysql_default(value: &str, extra: &str, data_type: &str) -> String {
    let numeric = value.parse::<f64>().is_ok();
    let expression = extra.to_ascii_lowercase().contains("default_generated")
        || value.starts_with('\'')
        || value.eq_ignore_ascii_case("null")
        || value.to_ascii_uppercase().starts_with("CURRENT_TIMESTAMP");
    let number_type = ["int", "decimal", "float", "double", "bit", "year"]
        .iter()
        .any(|kind| data_type.to_ascii_lowercase().contains(kind));
    if expression || (numeric && number_type) {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "''"))
    }
}

impl StudioApp {
    /// "Nowa tabela": a new tab of the designer with one key field.
    pub(super) fn new_design(&mut self, connection: Uuid, schema: String) {
        let Some(db) = self.databases.get(&connection) else {
            self.connect(connection);
            return;
        };
        let flavor = flavor_of(db.engine());
        let request = Uuid::new_v4();
        self.designs.push(DesignTab {
            id: Uuid::new_v4(),
            connection,
            table: TableRef {
                schema,
                name: String::new(),
                kind: "table".into(),
            },
            new: true,
            fields: vec![key_field(flavor)],
            original: Vec::new(),
            selected: 0,
            page: DesignPage::Fields,
            extras: TableExtras::default(),
            original_extras: TableExtras::default(),
            catalog: Default::default(),
            item: 0,
            loading: false,
            extras_loading: false,
            keep_extras: false,
            saving: false,
            request,
        });
        self.design_active = self.designs.len() - 1;
        self.section = Section::Design;
        if flavor == DdlFlavor::MySql {
            self.load_design_extras(connection, None, request);
        }
    }

    /// "Projektuj tabelę": the fields of an existing table in the designer.
    pub(super) fn open_design(&mut self, connection: Uuid, table: TableRef) {
        if let Some(i) = self.designs.iter().position(|d| {
            d.connection == connection
                && !d.new
                && d.table.schema == table.schema
                && d.table.name == table.name
        }) {
            self.design_active = i;
            self.section = Section::Design;
            return;
        }
        let request = Uuid::new_v4();
        self.designs.push(DesignTab {
            id: Uuid::new_v4(),
            connection,
            table: table.clone(),
            new: false,
            fields: Vec::new(),
            original: Vec::new(),
            selected: 0,
            page: DesignPage::Fields,
            extras: TableExtras::default(),
            original_extras: TableExtras::default(),
            catalog: Default::default(),
            item: 0,
            loading: true,
            extras_loading: false,
            keep_extras: false,
            saving: false,
            request,
        });
        self.design_active = self.designs.len() - 1;
        self.section = Section::Design;
        self.load_design(connection, table, request);
    }

    /// Read the columns of the table of a design, and for MySQL the comments,
    /// defaults and auto increment that the column list does not carry.
    fn load_design(&mut self, connection: Uuid, table: TableRef, request: Uuid) {
        let Some(db) = self.databases.get(&connection).cloned() else {
            self.connect(connection);
            return;
        };
        let mysql = flavor_of(db.engine()) == DdlFlavor::MySql;
        if mysql {
            self.load_design_extras(connection, Some(table.clone()), request);
        }
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let r = async {
                let columns = db.columns(&table).await?;
                let extra = if mysql {
                    let dialect = db.dialect();
                    let statement = format!(
                        "SELECT column_name, column_comment, extra, column_default, column_type FROM information_schema.columns WHERE table_schema = {} AND table_name = {}",
                        sql::literal(dialect, &Cell::Text(table.schema.clone()))?,
                        sql::literal(dialect, &Cell::Text(table.name.clone()))?
                    );
                    Some(db.query(&statement, CancellationToken::new()).await?)
                } else {
                    None
                };
                Ok::<_, anyhow::Error>((columns, extra))
            }
            .await
            .map_err(|e| e.to_string());
            let _ = tx.send(Message::DesignColumns(request, r)).await;
            ctx.request_repaint();
        });
    }

    /// The columns of a design arrived.
    pub(super) fn design_loaded(
        &mut self,
        request: Uuid,
        result: Result<(Vec<ColumnInfo>, Option<QueryResult>), String>,
    ) {
        let Some(design) = self.designs.iter_mut().find(|d| d.request == request) else {
            return;
        };
        design.loading = false;
        match result {
            Ok((columns, extra)) => {
                let mut fields: Vec<FieldDraft> =
                    columns.iter().map(FieldDraft::from_column).collect();
                if let Some(extra) = extra {
                    for row in &extra.rows {
                        let text = |i: usize| {
                            row.get(i).map(|c| match c {
                                Cell::Null => None,
                                other => Some(other.display()),
                            })
                        };
                        let Some(Some(name)) = text(0) else { continue };
                        if let Some(field) = fields.iter_mut().find(|f| f.name == name) {
                            field.comment = text(1).flatten().unwrap_or_default();
                            let extra = text(2).flatten().unwrap_or_default();
                            field.auto_increment =
                                extra.to_ascii_lowercase().contains("auto_increment");
                            field.default = text(3)
                                .flatten()
                                .map(|value| mysql_default(&value, &extra, &field.data_type))
                                .unwrap_or_default();
                        }
                    }
                }
                design.original = fields.clone();
                design.fields = fields;
                design.selected = 0;
            }
            Err(error) => self.notify(error, true),
        }
    }

    /// The statements saving would run, or why there are none.
    fn design_statements(&self, design: &DesignTab) -> anyhow::Result<SavePlan> {
        let db = self
            .databases
            .get(&design.connection)
            .ok_or_else(|| anyhow::anyhow!("database_disconnected"))?;
        let flavor = flavor_of(db.engine());
        let mut table = design.table.clone();
        table.name = table.name.trim().to_owned();
        let plain = |statements| SavePlan {
            statements,
            restores: Vec::new(),
        };
        match (flavor, design.new) {
            (DdlFlavor::MySql, true) => {
                ddl::mysql::create(db.dialect(), &table, &design.fields, &design.extras).map(plain)
            }
            (DdlFlavor::MySql, false) => ddl::mysql::alter_plan(
                db.dialect(),
                &table,
                &design.original,
                &design.fields,
                &design.original_extras,
                &design.extras,
            )
            .map(|(statements, restores)| SavePlan {
                statements,
                restores,
            }),
            (_, true) => ddl::create_table(flavor, db.dialect(), &table, &design.fields).map(plain),
            (_, false) => ddl::alter_table(
                flavor,
                db.dialect(),
                &table,
                &design.original,
                &design.fields,
            )
            .map(plain),
        }
    }

    /// Save: ask first when part of the table would be removed or rebuilt,
    /// then run.
    fn save_design(&mut self, index: usize) {
        let design = &self.designs[index];
        match self.design_statements(design) {
            Ok(plan) if plan.statements.is_empty() => {
                let text = self.tr(
                    "Nie ma zmian do zapisania.",
                    "There are no changes to save.",
                );
                self.notify(text.into(), false);
            }
            Ok(plan) => {
                let id = design.id;
                if needs_confirmation(&plan.statements, &design.original, &design.fields) {
                    self.design_confirm = Some((id, plan));
                } else {
                    self.run_design(id, plan);
                }
            }
            Err(error) => self.notify(error.to_string(), true),
        }
    }

    /// Run the statements of a design one by one, each written to the
    /// history. When one fails, what puts back the parts it leaves dropped
    /// runs as well, as far as it goes, and is written to the history too.
    pub(super) fn run_design(&mut self, id: Uuid, plan: SavePlan) {
        let Some(design) = self.designs.iter_mut().find(|d| d.id == id) else {
            return;
        };
        let connection = design.connection;
        let Some(db) = self.databases.get(&connection).cloned() else {
            self.connect(connection);
            return;
        };
        design.saving = true;
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let vault = self.vault.clone();
        self.track(async move {
            let mut done = 0usize;
            let mut outcome = Ok(());
            for (index, statement) in plan.statements.iter().enumerate() {
                let run = async {
                    db.query(statement, CancellationToken::new())
                        .await
                        .map(|_| ())
                };
                match recorded(vault.clone(), connection, statement, run).await {
                    Ok(()) => done += 1,
                    Err(error) => {
                        for restore in plan.restores.iter().filter(|r| r.needed(index)) {
                            let run = async {
                                db.query(&restore.sql, CancellationToken::new())
                                    .await
                                    .map(|_| ())
                            };
                            let _ = recorded(vault.clone(), connection, &restore.sql, run).await;
                        }
                        outcome = Err(error);
                        break;
                    }
                }
            }
            let _ = tx
                .send(Message::DesignSaved(id, outcome.map(|()| done)))
                .await;
            ctx.request_repaint();
        });
    }

    /// Saving ended: read the table again, as the database now has it.
    pub(super) fn design_saved(&mut self, id: Uuid, result: Result<usize, String>) {
        let Some(design) = self.designs.iter_mut().find(|d| d.id == id) else {
            return;
        };
        design.saving = false;
        let connection = design.connection;
        match result {
            Ok(_) => {
                // Open data of this table read its columns again.
                let (schema, name) = (
                    design.table.schema.clone(),
                    design.table.name.trim().to_owned(),
                );
                for tab in self.tabs.iter_mut().filter(|t| {
                    t.connection == Some(connection)
                        && t.source
                            .as_ref()
                            .is_some_and(|s| s.schema == schema && s.name == name)
                }) {
                    tab.columns.clear();
                }
                let Some(design) = self.designs.iter_mut().find(|d| d.id == id) else {
                    return;
                };
                design.new = false;
                design.table.name = design.table.name.trim().to_owned();
                design.keep_extras = false;
                design.loading = true;
                design.request = Uuid::new_v4();
                let (table, request) = (design.table.clone(), design.request);
                let text = self.tr("Zapisano tabelę.", "The table was saved.");
                self.notify(text.into(), false);
                self.refresh_tables(Some(connection));
                self.load_design(connection, table, request);
            }
            Err(error) => {
                // Part may have been written: show what the table now is,
                // and keep the parts beyond the columns as edited.
                let reload = (!design.new).then(|| {
                    design.keep_extras = true;
                    design.loading = true;
                    design.request = Uuid::new_v4();
                    (design.table.clone(), design.request)
                });
                self.notify(error, true);
                if let Some((table, request)) = reload {
                    self.load_design(connection, table, request);
                }
            }
        }
    }

    /// Close a tab of the designer.
    pub(super) fn close_design(&mut self, id: Uuid) {
        self.designs.retain(|d| d.id != id);
        self.design_active = self.design_active.min(self.designs.len().saturating_sub(1));
        if self.section == Section::Design && self.designs.is_empty() {
            self.section = Section::Objects;
        }
    }

    pub(super) fn design_view(&mut self, ui: &mut egui::Ui) {
        if self.designs.is_empty() {
            self.section = Section::Objects;
            return;
        }
        let index = self.design_active.min(self.designs.len() - 1);
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let connected = self.databases.contains_key(&self.designs[index].connection);
        let flavor = self
            .databases
            .get(&self.designs[index].connection)
            .map_or(DdlFlavor::Generic, |db| flavor_of(db.engine()));
        let preview = self.design_statements(&self.designs[index]);
        let tables = self
            .tables
            .get(&self.designs[index].connection)
            .cloned()
            .unwrap_or_default();
        let mut extras_action = None;
        let mut open_values = None;
        let design = &mut self.designs[index];
        let busy = design.loading || design.saving || design.extras_loading;
        let (mut save, mut add, mut insert, mut remove, mut key) =
            (false, false, false, false, false);
        let (mut up, mut down, mut open_data) = (false, false, false);
        let count = design.fields.len();
        let at = design.selected.min(count.saturating_sub(1));

        widgets::toolbar(ui, &p, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            save = Btn::primary(l("Zapisz", "Save"))
                .icon(Icon::Save)
                .small()
                .enabled(connected && !busy)
                .tip(&format!(
                    "{} ({})",
                    l("Zapisz tabelę", "Save the table"),
                    super::shortcuts::keys_text("Mod+S", en)
                ))
                .show(ui, &p)
                .clicked();
            ui.add_space(8.0);
            add = Btn::quiet(l("Dodaj pole", "Add field"))
                .icon(Icon::Plus)
                .small()
                .enabled(!busy)
                .show(ui, &p)
                .clicked();
            insert = Btn::quiet(l("Wstaw pole", "Insert field"))
                .icon(Icon::ArrowDown)
                .small()
                .enabled(!busy && count > 0)
                .show(ui, &p)
                .clicked();
            remove = Btn::quiet(l("Usuń pole", "Delete field"))
                .icon(Icon::Minus)
                .small()
                .enabled(!busy && count > 1)
                .show(ui, &p)
                .clicked();
            ui.add_space(8.0);
            key = Btn::quiet(l("Klucz główny", "Primary key"))
                .icon(Icon::Lock)
                .small()
                .enabled(!busy && count > 0)
                .show(ui, &p)
                .clicked();
            ui.add_space(8.0);
            up = Btn::quiet(l("Przesuń w górę", "Move up"))
                .icon(Icon::ArrowUp)
                .small()
                .enabled(!busy && at > 0)
                .show(ui, &p)
                .clicked();
            down = Btn::quiet(l("Przesuń w dół", "Move down"))
                .icon(Icon::ArrowDown)
                .small()
                .enabled(!busy && at + 1 < count)
                .show(ui, &p)
                .clicked();
            widgets::right(ui, |ui| {
                if !design.new {
                    open_data = Btn::link(l("Otwórz dane", "Open data"))
                        .small()
                        .show(ui, &p)
                        .clicked();
                }
                if busy {
                    ui.add(egui::Spinner::new().size(14.0).color(p.muted));
                }
            });
        });

        // The name of a new table, and the pages of the designer.
        egui::Frame::NONE
            .fill(p.side)
            .inner_margin(egui::Margin::symmetric(14, 0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_height(p.control_height() + 6.0);
                    ui.spacing_mut().item_spacing.x = 0.0;
                    let mut pages = vec![(DesignPage::Fields, l("Pola", "Fields"))];
                    if flavor == DdlFlavor::MySql {
                        pages.extend([
                            (DesignPage::Indexes, l("Indeksy", "Indexes")),
                            (DesignPage::ForeignKeys, l("Klucze obce", "Foreign keys")),
                            (DesignPage::Checks, l("Sprawdzenia", "Checks")),
                            (DesignPage::Triggers, l("Wyzwalacze", "Triggers")),
                            (DesignPage::Options, l("Opcje", "Options")),
                            (DesignPage::Partitions, l("Partycje", "Partitions")),
                        ]);
                    }
                    pages.push((DesignPage::Sql, l("Podgląd SQL", "SQL preview")));
                    for (page, label) in pages {
                        if widgets::tab(
                            ui,
                            &p,
                            None,
                            label,
                            design.page == page,
                            12.0,
                            true,
                            p.control_height() + 6.0,
                        )
                        .clicked()
                            && design.page != page
                        {
                            design.page = page;
                            design.item = 0;
                        }
                    }
                    widgets::right(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if design.new {
                            ui.add(
                                widgets::field(&p, &mut design.table.name)
                                    .desired_width(220.0)
                                    .hint_text(l("nazwa_tabeli", "table_name")),
                            );
                            ui.label(
                                egui::RichText::new(l("Nazwa tabeli", "Table name"))
                                    .size(p.small())
                                    .color(p.muted),
                            );
                        }
                    });
                });
            });
        widgets::hline(ui, &p);

        if design.page == DesignPage::Sql {
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(16, 14))
                .show(ui, |ui| {
                    match preview.as_ref().map(|plan| &plan.statements) {
                        Ok(statements) if statements.is_empty() => {
                            widgets::help(
                                ui,
                                &p,
                                l(
                                    "Nie ma zmian do zapisania.",
                                    "There are no changes to save.",
                                ),
                            );
                        }
                        Ok(statements) => {
                            egui::ScrollArea::both()
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    let text = statements
                                        .iter()
                                        .map(|s| format!("{s};"))
                                        .collect::<Vec<_>>()
                                        .join("\n\n");
                                    let mut job = highlight(&text, p.code() - 0.5, p);
                                    job.wrap.max_width = f32::INFINITY;
                                    ui.add(egui::Label::new(job).selectable(true));
                                });
                        }
                        Err(error) => {
                            let text = crate::messages::describe(&error.to_string(), en);
                            widgets::feedback(ui, &p, &text, true);
                        }
                    }
                });
        } else if design.loading && design.fields.is_empty() {
            super::sql_view::hint(ui, &p, l("Wczytywanie pól…", "Loading the fields…"));
        } else if design.page != DesignPage::Fields {
            egui::ScrollArea::vertical()
                .id_salt(("design_extras", design.id))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    extras_action = super::design_extras::extras_page(ui, &p, en, design, &tables);
                });
        } else {
            // The default value and switches of the selected field, below.
            egui::Panel::bottom(egui::Id::new(("design_detail", design.id)))
                .exact_size(p.control_height() * 3.0 + 26.0)
                .resizable(false)
                .frame(
                    egui::Frame::NONE
                        .fill(p.side)
                        .inner_margin(egui::Margin::symmetric(16, 10)),
                )
                .show(ui, |ui| {
                    if let Some(field) = design.fields.get_mut(at) {
                        egui::Grid::new(("design_detail_grid", design.id))
                            .num_columns(2)
                            .spacing([18.0, 8.0])
                            .show(ui, |ui| {
                                ui.label(
                                    egui::RichText::new(l("Domyślnie", "Default")).color(p.muted),
                                );
                                ui.add(
                                    widgets::field(&p, &mut field.default)
                                        .desired_width(320.0)
                                        .hint_text(l(
                                            "np. 0, 'tekst', CURRENT_TIMESTAMP",
                                            "e.g. 0, 'text', CURRENT_TIMESTAMP",
                                        )),
                                );
                                ui.end_row();
                                ui.label("");
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 18.0;
                                    widgets::checkbox(
                                        ui,
                                        &p,
                                        &mut field.auto_increment,
                                        "Auto Increment",
                                    );
                                    if flavor == DdlFlavor::MySql {
                                        widgets::checkbox(
                                            ui,
                                            &p,
                                            &mut field.unsigned,
                                            l("Bez znaku (unsigned)", "Unsigned"),
                                        );
                                        if is_choice(&field.data_type)
                                            && Btn::new(&format!(
                                                "{} ({})…",
                                                l("Wartości", "Values"),
                                                field.values.len()
                                            ))
                                            .small()
                                            .show(ui, &p)
                                            .clicked()
                                        {
                                            open_values = Some(at);
                                        }
                                    }
                                });
                                ui.end_row();
                            });
                    }
                });
            let line = egui::Stroke::new(1.0, p.line);
            let row_height = p.row_height() + 4.0;
            let headers = [
                "",
                l("Nazwa", "Name"),
                l("Typ", "Type"),
                l("Długość", "Length"),
                l("Po przecinku", "Decimals"),
                "NOT NULL",
                l("Klucz", "Key"),
                l("Komentarz", "Comment"),
            ];
            let key_order: Vec<usize> = design
                .fields
                .iter()
                .enumerate()
                .filter(|(_, f)| f.primary_key)
                .map(|(i, _)| i)
                .collect();
            let choices = type_choices(flavor);
            let comments = flavor == DdlFlavor::MySql || flavor == DdlFlavor::Postgres;
            let mut select = None;
            TableBuilder::new(ui)
                .id_salt(("design_grid", design.id))
                .striped(self.profiles.preferences.grid_striped)
                .resizable(false)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(28.0))
                .column(Column::exact(210.0).clip(true))
                .column(Column::exact(180.0).clip(true))
                .column(Column::exact(80.0).clip(true))
                .column(Column::exact(96.0).clip(true))
                .column(Column::exact(84.0))
                .column(Column::exact(64.0))
                .column(Column::remainder().at_least(140.0).clip(true))
                .header(row_height, |mut header| {
                    for title in headers {
                        header.col(|ui| {
                            let rect = ui.max_rect();
                            ui.painter().rect_filled(rect, 0.0, p.side);
                            ui.painter()
                                .hline(rect.x_range(), rect.bottom() - 0.5, line);
                            ui.painter().text(
                                egui::pos2(rect.left() + 8.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                title,
                                theme::medium(p.small() + 0.5),
                                p.muted,
                            );
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_height, design.fields.len(), |mut row| {
                        let i = row.index();
                        row.set_selected(i == at);
                        let field = &mut design.fields[i];
                        let edit = |ui: &mut egui::Ui, text: &mut String, hint: &str| {
                            ui.add(
                                egui::TextEdit::singleline(text)
                                    .frame(egui::Frame::NONE)
                                    .margin(egui::Margin::symmetric(6, 2))
                                    .desired_width(ui.available_width() - 4.0)
                                    .hint_text(hint),
                            )
                        };
                        let mut touched = false;
                        row.col(|ui| {
                            if i == at {
                                widgets::paint(
                                    ui.painter(),
                                    egui::Rect::from_center_size(
                                        ui.max_rect().center(),
                                        egui::vec2(12.0, 12.0),
                                    ),
                                    Icon::ChevronRight,
                                    p.accent,
                                );
                            }
                        });
                        row.col(|ui| {
                            touched |= edit(ui, &mut field.name, l("nazwa", "name")).has_focus()
                        });
                        row.col(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            let width = ui.available_width();
                            ui.allocate_ui(egui::vec2(width - 26.0, ui.available_height()), |ui| {
                                ui.set_max_width(width - 26.0);
                                touched |= edit(ui, &mut field.data_type, "").has_focus();
                            });
                            let menu =
                                widgets::icon_button(ui, &p, Icon::ChevronDown, l("Typy", "Types"));
                            touched |= menu.clicked();
                            egui::Popup::menu(&menu).show(|ui| {
                                egui::ScrollArea::vertical()
                                    .max_height(280.0)
                                    .show(ui, |ui| {
                                        for choice in choices {
                                            if ui
                                                .selectable_label(
                                                    field.data_type.eq_ignore_ascii_case(choice),
                                                    *choice,
                                                )
                                                .clicked()
                                            {
                                                field.data_type = (*choice).to_owned();
                                                ui.close();
                                            }
                                        }
                                    });
                            });
                        });
                        row.col(|ui| {
                            // The values of an ENUM or a SET are edited in a
                            // window of their own.
                            if flavor == DdlFlavor::MySql && is_choice(&field.data_type) {
                                let text = if field.values.is_empty() {
                                    l("wartości…", "values…").to_owned()
                                } else {
                                    field.values.join(", ")
                                };
                                let label = ui.add(
                                    egui::Label::new(egui::RichText::new(text).color(p.keyword))
                                        .truncate()
                                        .sense(egui::Sense::click()),
                                );
                                if label.clicked() {
                                    open_values = Some(i);
                                    touched = true;
                                }
                            } else {
                                touched |= edit(ui, &mut field.length, "").has_focus();
                            }
                        });
                        row.col(|ui| touched |= edit(ui, &mut field.scale, "").has_focus());
                        row.col(|ui| {
                            ui.add_space(24.0);
                            touched |= ui.checkbox(&mut field.not_null, "").changed();
                        });
                        let (_, key_cell) = row.col(|ui| {
                            if field.primary_key {
                                let number =
                                    key_order.iter().position(|k| *k == i).map_or(1, |n| n + 1);
                                widgets::paint(
                                    ui.painter(),
                                    egui::Rect::from_center_size(
                                        ui.max_rect().center() - egui::vec2(6.0, 0.0),
                                        egui::vec2(13.0, 13.0),
                                    ),
                                    Icon::Lock,
                                    theme::WARNING,
                                );
                                ui.painter().text(
                                    ui.max_rect().center() + egui::vec2(8.0, 0.0),
                                    egui::Align2::LEFT_CENTER,
                                    number.to_string(),
                                    egui::FontId::proportional(p.small()),
                                    p.muted,
                                );
                            }
                        });
                        if key_cell.clicked() {
                            field.primary_key = !field.primary_key;
                            if field.primary_key {
                                field.not_null = true;
                            }
                            touched = true;
                        }
                        row.col(|ui| {
                            if comments {
                                touched |= edit(ui, &mut field.comment, "").has_focus();
                            }
                        });
                        if touched || row.response().clicked() {
                            select = Some(i);
                        }
                    });
                });
            if let Some(i) = select {
                design.selected = i;
            }
        }

        // The keyboard: Mod+S saves, as the button does when it is enabled.
        if !ui.ctx().memory(|m| m.top_modal_layer()).is_some()
            && take_mod(ui, false, egui::Key::S)
            && connected
            && !busy
        {
            save = true;
        }
        let design = &mut self.designs[index];
        let at = design.selected.min(design.fields.len().saturating_sub(1));
        if add {
            design.fields.push(FieldDraft {
                data_type: type_choices(flavor)[0].to_owned(),
                ..Default::default()
            });
            design.selected = design.fields.len() - 1;
        }
        if insert {
            design.fields.insert(
                at,
                FieldDraft {
                    data_type: type_choices(flavor)[0].to_owned(),
                    ..Default::default()
                },
            );
            design.selected = at;
        }
        if remove && design.fields.len() > 1 {
            design.fields.remove(at);
            design.selected = at.min(design.fields.len() - 1);
        }
        if key && let Some(field) = design.fields.get_mut(at) {
            field.primary_key = !field.primary_key;
            if field.primary_key {
                field.not_null = true;
            }
        }
        if up && at > 0 {
            design.fields.swap(at, at - 1);
            design.selected = at - 1;
        }
        if down && at + 1 < design.fields.len() {
            design.fields.swap(at, at + 1);
            design.selected = at + 1;
        }
        let (connection, table, design_id) = (design.connection, design.table.clone(), design.id);
        if let Some(field) = open_values
            && let Some(values) = design.fields.get(field).map(|f| f.values.clone())
        {
            self.design_values = Some(super::design_extras::ValuesEditor {
                design: design_id,
                field,
                values,
                new: String::new(),
            });
        }
        if let Some(super::design_extras::ExtrasAction::RefColumns(target)) = extras_action {
            self.load_ref_columns(design_id, connection, target);
        }
        if open_data {
            self.open_table(connection, table);
        }
        if save {
            self.save_design(index);
        }
    }

    /// The question before saving removes or rebuilds part of a table.
    pub(super) fn design_confirm_window(&mut self, ctx: &egui::Context) {
        let Some((id, plan)) = self.design_confirm.take() else {
            return;
        };
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let (mut run, mut cancel) = (false, false);
        let dialog = widgets::dialog(
            ctx,
            &p,
            "design_confirm",
            l("Zapisać zmiany tabeli?", "Save the changes of the table?"),
            560.0,
            l("Zamknij", "Close"),
            |ui| {
                widgets::dialog_body(ui, |ui| {
                    widgets::help(
                        ui,
                        &p,
                        l(
                            "Zapis usunie albo przebuduje część tabeli: pola, indeksy, klucze, sprawdzenia, wyzwalacze, wartości pól ENUM i SET, silnik albo partycje. Usuniętych danych nie da się przywrócić.",
                            "Saving removes or rebuilds parts of the table: fields, indexes, keys, checks, triggers, values of ENUM and SET fields, the engine or the partitions. Removed data cannot be brought back.",
                        ),
                    );
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .show(ui, |ui| {
                            for statement in &plan.statements {
                                ui.label(
                                    egui::RichText::new(statement)
                                        .monospace()
                                        .size(p.code() - 1.5)
                                        .color(p.text),
                                );
                            }
                        });
                });
                widgets::dialog_footer(ui, &p, |ui| {
                    widgets::right(ui, |ui| {
                        run = Btn::danger(l("Zapisz", "Save")).show(ui, &p).clicked();
                        cancel = Btn::new(l("Anuluj", "Cancel")).show(ui, &p).clicked();
                    });
                });
            },
        );
        if run {
            self.run_design(id, plan);
        } else if !cancel && !dialog.close {
            self.design_confirm = Some((id, plan));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_defaults_get_their_quotes_back() {
        assert_eq!(mysql_default("none", "", "varchar"), "'none'");
        assert_eq!(mysql_default("it's", "", "varchar"), "'it''s'");
        assert_eq!(mysql_default("0", "", "int"), "0");
        assert_eq!(mysql_default("12", "", "varchar"), "'12'");
        assert_eq!(mysql_default("'none'", "", "varchar"), "'none'");
        assert_eq!(
            mysql_default("CURRENT_TIMESTAMP", "DEFAULT_GENERATED", "datetime"),
            "CURRENT_TIMESTAMP"
        );
        assert_eq!(
            mysql_default("now()", "DEFAULT_GENERATED", "datetime"),
            "now()"
        );
    }

    #[test]
    fn saving_asks_first_when_part_of_the_table_goes() {
        let statements = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let status = |values: &[&str]| FieldDraft {
            name: "status".into(),
            data_type: "enum".into(),
            values: values.iter().map(|v| (*v).to_owned()).collect(),
            original: Some("status".into()),
            ..Default::default()
        };
        let before = vec![status(&["new", "paid"])];
        for statement in [
            "ALTER TABLE `s`.`t` DROP COLUMN `a`",
            "ALTER TABLE `s`.`t` DROP INDEX `by_a`",
            "ALTER TABLE `s`.`t` DROP FOREIGN KEY `fk_a`",
            "ALTER TABLE `s`.`t` DROP CONSTRAINT `positive`",
            "ALTER TABLE `s`.`t` DROP PRIMARY KEY, ADD PRIMARY KEY (`b`)",
            "DROP TRIGGER IF EXISTS `s`.`stamp`",
            "ALTER TABLE `s`.`t` ENGINE=BLACKHOLE",
            "ALTER TABLE `s`.`t` PARTITION BY HASH (id) PARTITIONS 4",
            "ALTER TABLE `s`.`t` REMOVE PARTITIONING",
        ] {
            assert!(
                needs_confirmation(&statements(&[statement]), &before, &before),
                "{statement}"
            );
        }
        // Adding asks nothing, nor does a new table with its engine.
        for statement in [
            "ALTER TABLE `s`.`t` ADD INDEX `by_a` (`a`)",
            "ALTER TABLE `s`.`t` COMMENT='x'",
            "CREATE TABLE `s`.`t` (\n  `id` int NULL\n)\nENGINE=InnoDB",
        ] {
            assert!(
                !needs_confirmation(&statements(&[statement]), &before, &before),
                "{statement}"
            );
        }
        // A value taken from an ENUM empties the rows that hold it.
        let change =
            statements(&["ALTER TABLE `s`.`t` CHANGE COLUMN `status` `status` enum('new') NULL"]);
        assert!(needs_confirmation(&change, &before, &[status(&["new"])]));
        assert!(!needs_confirmation(
            &change,
            &before,
            &[status(&["new", "paid", "sent"])]
        ));
    }
}
