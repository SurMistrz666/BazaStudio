//! The parts of a MySQL or MariaDB table beyond its columns in the table
//! designer: a page each for indexes, foreign keys, CHECK constraints,
//! triggers, the options of the table with the settings of its engine, and
//! partitions; and the editor of the values of an ENUM or a SET column.
use super::*;
use crate::widgets::Btn;
use baza_core::ddl::mysql::{self, *};
use eframe::egui::RichText;
use std::collections::{BTreeSet, HashMap};

/// Width of the label column of the editors.
const LABELS: f32 = 170.0;

/// What the server offers for the options of a table, and the columns of the
/// tables foreign keys point at.
#[derive(Clone, Debug, Default)]
pub(super) struct Catalog {
    pub engines: Vec<String>,
    pub charsets: Vec<String>,
    /// Collation and its character set.
    pub collations: Vec<(String, String)>,
    /// Columns of other tables, by `schema.table`.
    pub ref_columns: HashMap<String, Vec<String>>,
    /// Tables whose columns were asked for.
    pub requested: BTreeSet<String>,
}

/// The values of an ENUM or SET field being edited in their own window.
pub(super) struct ValuesEditor {
    pub design: Uuid,
    pub field: usize,
    pub values: Vec<String>,
    /// The value being typed; several lines are several values.
    pub new: String,
}

/// What a page asks for once drawn.
pub(super) enum ExtrasAction {
    /// The columns of a table a foreign key points at.
    RefColumns(TableRef),
}

fn ref_key(schema: &str, table: &str) -> String {
    format!("{schema}.{table}")
}

impl StudioApp {
    /// Read the parts of the table of a design beyond its columns, and what
    /// the server offers for its options. `table` is `None` for a new one.
    pub(super) fn load_design_extras(
        &mut self,
        connection: Uuid,
        table: Option<TableRef>,
        request: Uuid,
    ) {
        let Some(db) = self.databases.get(&connection).cloned() else {
            return;
        };
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let r = async {
                let mut catalog = Catalog::default();
                for (index, query) in mysql::CATALOG_QUERIES.iter().enumerate() {
                    // A server that hides a list leaves the field typed.
                    let rows = db
                        .query(query, CancellationToken::new())
                        .await
                        .map(|result| result.rows)
                        .unwrap_or_default();
                    let text = |row: &Vec<Cell>, i: usize| {
                        row.get(i).map(Cell::display).unwrap_or_default()
                    };
                    match index {
                        0 => catalog.engines = rows.iter().map(|row| text(row, 0)).collect(),
                        1 => catalog.charsets = rows.iter().map(|row| text(row, 0)).collect(),
                        // The full names of MariaDB first; MySQL's if none.
                        _ if catalog.collations.is_empty() => {
                            catalog.collations = rows
                                .iter()
                                .map(|row| (text(row, 0), text(row, 1)))
                                .collect()
                        }
                        _ => {}
                    }
                }
                let extras = match &table {
                    Some(table) => {
                        let names: Vec<String> = db
                            .columns(table)
                            .await?
                            .into_iter()
                            .map(|column| column.name)
                            .collect();
                        let mut rows = Vec::new();
                        for (index, query) in
                            mysql::queries(db.dialect(), table)?.iter().enumerate()
                        {
                            let result = match db.query(query, CancellationToken::new()).await {
                                Ok(result) => result.rows,
                                // MySQL: check_constraints has no table name.
                                Err(_) if index == 2 => db
                                    .query(
                                        &mysql::checks_query_mysql(db.dialect(), table)?,
                                        CancellationToken::new(),
                                    )
                                    .await
                                    .map(|result| result.rows)
                                    .unwrap_or_default(),
                                Err(error) => return Err(error),
                            };
                            rows.push(result);
                        }
                        Some(mysql::read(
                            [&rows[0], &rows[1], &rows[2], &rows[3], &rows[4], &rows[5]],
                            &names,
                        ))
                    }
                    None => None,
                };
                Ok::<_, anyhow::Error>((extras, catalog))
            }
            .await
            .map_err(|e| e.to_string());
            let _ = tx.send(Message::DesignExtras(request, r)).await;
            ctx.request_repaint();
        });
    }

    pub(super) fn design_extras_loaded(
        &mut self,
        request: Uuid,
        result: Result<(Option<TableExtras>, Catalog), String>,
    ) {
        let Some(design) = self.designs.iter_mut().find(|d| d.request == request) else {
            return;
        };
        match result {
            Ok((extras, catalog)) => {
                if let Some(extras) = extras {
                    design.original_extras = extras.clone();
                    design.extras = extras;
                }
                // Columns of other tables read before stay.
                let known = std::mem::take(&mut design.catalog.ref_columns);
                design.catalog = Catalog {
                    ref_columns: known,
                    ..catalog
                };
            }
            Err(error) => self.notify(error, true),
        }
    }

    /// Read the columns of the table a foreign key of a design points at.
    pub(super) fn load_ref_columns(&mut self, design: Uuid, connection: Uuid, table: TableRef) {
        let Some(db) = self.databases.get(&connection).cloned() else {
            return;
        };
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let key = ref_key(&table.schema, &table.name);
        self.track(async move {
            let r = db
                .columns(&table)
                .await
                .map(|columns| columns.into_iter().map(|c| c.name).collect())
                .map_err(|e| e.to_string());
            let _ = tx.send(Message::DesignRefColumns(design, key, r)).await;
            ctx.request_repaint();
        });
    }

    pub(super) fn ref_columns_loaded(
        &mut self,
        design: Uuid,
        key: String,
        result: Result<Vec<String>, String>,
    ) {
        if let (Some(design), Ok(columns)) =
            (self.designs.iter_mut().find(|d| d.id == design), result)
        {
            design.catalog.ref_columns.insert(key, columns);
        }
    }

    /// The window that edits the values of an ENUM or SET field.
    pub(super) fn values_window(&mut self, ctx: &egui::Context) {
        let Some(mut editor) = self.design_values.take() else {
            return;
        };
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let Some(design) = self.designs.iter().find(|d| d.id == editor.design) else {
            return;
        };
        let Some(field) = design.fields.get(editor.field) else {
            return;
        };
        let title = format!(
            "{}: {}",
            l("Wartości", "Values"),
            if field.name.trim().is_empty() {
                "—"
            } else {
                field.name.trim()
            }
        );
        let (mut save, mut cancel) = (false, false);
        let dialog = widgets::dialog(
            ctx,
            &p,
            "enum_values",
            &title,
            520.0,
            l("Zamknij", "Close"),
            |ui| {
                widgets::dialog_body(ui, |ui| {
                    widgets::help(
                        ui,
                        &p,
                        l(
                            "Kolejność ma znaczenie: ENUM porządkuje i porównuje wartości według niej. Wiele wartości naraz wklej po jednej w linii.",
                            "The order matters: ENUM sorts and compares values by it. Paste several values at once, one per line.",
                        ),
                    );
                    let mut action: Option<(usize, i8)> = None;
                    egui::ScrollArea::vertical()
                        .id_salt("enum_values_list")
                        .max_height(300.0)
                        .show(ui, |ui| {
                            for (index, value) in editor.values.iter_mut().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(format!("{}.", index + 1)).color(p.muted),
                                    );
                                    ui.add(widgets::field(&p, value).desired_width(300.0));
                                    if widgets::icon_button(
                                        ui,
                                        &p,
                                        Icon::ArrowUp,
                                        l("W górę", "Up"),
                                    )
                                    .clicked()
                                    {
                                        action = Some((index, -1));
                                    }
                                    if widgets::icon_button(
                                        ui,
                                        &p,
                                        Icon::ArrowDown,
                                        l("W dół", "Down"),
                                    )
                                    .clicked()
                                    {
                                        action = Some((index, 1));
                                    }
                                    if widgets::icon_button(
                                        ui,
                                        &p,
                                        Icon::Minus,
                                        l("Usuń", "Delete"),
                                    )
                                    .clicked()
                                    {
                                        action = Some((index, 0));
                                    }
                                });
                            }
                        });
                    match action {
                        Some((index, 0)) => {
                            editor.values.remove(index);
                        }
                        Some((index, -1)) if index > 0 => editor.values.swap(index, index - 1),
                        Some((index, 1)) if index + 1 < editor.values.len() => {
                            editor.values.swap(index, index + 1)
                        }
                        _ => {}
                    }
                    ui.horizontal(|ui| {
                        let response = ui.add(
                            egui::TextEdit::multiline(&mut editor.new)
                                .desired_rows(1)
                                .desired_width(330.0)
                                .hint_text(l("nowa wartość", "new value")),
                        );
                        let enter = response.has_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
                        if Btn::new(l("Dodaj", "Add")).small().show(ui, &p).clicked() || enter {
                            for line in editor.new.lines() {
                                let value = line.trim_end_matches('\r');
                                if !value.is_empty() && !editor.values.iter().any(|v| v == value) {
                                    editor.values.push(value.to_owned());
                                }
                            }
                            editor.new.clear();
                            response.request_focus();
                        }
                    });
                });
                widgets::dialog_footer(ui, &p, |ui| {
                    widgets::right(ui, |ui| {
                        save = Btn::primary(l("Zastosuj", "Apply"))
                            .min_width(84.0)
                            .show(ui, &p)
                            .clicked();
                        cancel = Btn::new(l("Anuluj", "Cancel"))
                            .min_width(84.0)
                            .show(ui, &p)
                            .clicked();
                    });
                });
            },
        );
        if save {
            if let Some(field) = self
                .designs
                .iter_mut()
                .find(|d| d.id == editor.design)
                .and_then(|d| d.fields.get_mut(editor.field))
            {
                field.values = editor.values;
            }
        } else if !cancel && !(dialog.close && !dialog.outside) {
            // A click beside the window does not lose the values typed.
            self.design_values = Some(editor);
        }
    }
}

/// A row of an editor: a label and what is edited beside it.
fn row<R>(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    label: &str,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    widgets::form_row(ui, p, LABELS, label, add)
}

fn text_row(ui: &mut egui::Ui, p: &theme::Palette, label: &str, text: &mut String, hint: &str) {
    row(ui, p, label, |ui| {
        ui.add(widgets::field(p, text).desired_width(360.0).hint_text(hint));
    });
}

fn code_row(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    label: &str,
    text: &mut String,
    rows: usize,
    hint: &str,
) {
    row(ui, p, label, |ui| {
        ui.add(
            egui::TextEdit::multiline(text)
                .code_editor()
                .desired_rows(rows)
                .desired_width(f32::INFINITY)
                .hint_text(hint),
        );
    });
}

/// A choice among names; an empty one is shown as `empty`.
fn name_combo(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    id: impl std::hash::Hash + std::fmt::Debug,
    value: &mut String,
    names: &[String],
    empty: Option<&str>,
    width: f32,
) -> bool {
    let before = value.clone();
    let shown = if value.is_empty() {
        empty.unwrap_or("—").to_owned()
    } else {
        value.clone()
    };
    widgets::combo(ui, p, egui::Id::new(id), shown, width, |ui| {
        egui::ScrollArea::vertical()
            .max_height(300.0)
            .show(ui, |ui| {
                if let Some(empty) = empty {
                    ui.selectable_value(value, String::new(), empty);
                }
                for name in names {
                    ui.selectable_value(value, name.clone(), name);
                }
            });
    });
    *value != before
}

/// The items of a page as a list with "Dodaj" and "Usuń"; a click on a line
/// chooses the item the editor below shows.
#[allow(clippy::too_many_arguments)]
fn item_list<T>(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    en: bool,
    id: &str,
    items: &mut Vec<T>,
    chosen: &mut usize,
    heads: &[&str],
    line: impl Fn(&T) -> Vec<String>,
    new: impl FnOnce(usize) -> T,
) {
    let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
    ui.horizontal(|ui| {
        if Btn::quiet(l("Dodaj", "Add"))
            .icon(Icon::Plus)
            .small()
            .show(ui, p)
            .clicked()
        {
            items.push(new(items.len()));
            *chosen = items.len() - 1;
        }
        if Btn::quiet(l("Usuń", "Delete"))
            .icon(Icon::Minus)
            .small()
            .enabled(!items.is_empty())
            .show(ui, p)
            .clicked()
            && *chosen < items.len()
        {
            items.remove(*chosen);
            *chosen = chosen.saturating_sub(1);
        }
    });
    if items.is_empty() {
        widgets::help(ui, p, l("Nie ma jeszcze żadnego.", "There is none yet."));
        return;
    }
    let row_height = p.row_height() + 2.0;
    let height = (row_height * (items.len() as f32 + 1.0))
        .min(ui.available_height() * 0.4)
        .max(row_height * 3.0);
    let mut picked = None;
    ui.push_id(id, |ui| {
        let mut table = egui_extras::TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .max_scroll_height(height)
            .min_scrolled_height(height);
        for (index, _) in heads.iter().enumerate() {
            table = table.column(if index + 1 == heads.len() {
                egui_extras::Column::remainder().clip(true)
            } else {
                egui_extras::Column::initial(200.0)
                    .at_least(80.0)
                    .clip(true)
                    .resizable(true)
            });
        }
        table
            .header(row_height, |mut header| {
                for head in heads {
                    header.col(|ui| {
                        ui.label(RichText::new(*head).size(p.small()).color(p.muted));
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, items.len(), |mut table_row| {
                    let index = table_row.index();
                    table_row.set_selected(index == *chosen);
                    for cell in line(&items[index]) {
                        table_row.col(|ui| {
                            ui.label(RichText::new(cell).color(p.text));
                        });
                    }
                    if table_row.response().clicked() {
                        picked = Some(index);
                    }
                });
            });
    });
    if let Some(index) = picked {
        *chosen = index;
    }
    *chosen = (*chosen).min(items.len() - 1);
    ui.add_space(6.0);
    widgets::hline(ui, p);
    ui.add_space(6.0);
}

/// One page of the parts beyond the columns.
pub(super) fn extras_page(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    en: bool,
    design: &mut design_view::DesignTab,
    tables: &[TableRef],
) -> Option<ExtrasAction> {
    let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
    let fields: Vec<String> = design
        .fields
        .iter()
        .map(|f| f.name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect();
    let mut action = None;
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            match design.page {
                design_view::DesignPage::Indexes => {
                    item_list(
                        ui,
                        p,
                        en,
                        "indexes",
                        &mut design.extras.indexes,
                        &mut design.item,
                        &[
                            l("Nazwa", "Name"),
                            l("Rodzaj", "Kind"),
                            l("Kolumny", "Columns"),
                        ],
                        |index| {
                            vec![
                                index.name.clone(),
                                match index.kind {
                                    IndexKind::Index => "INDEX",
                                    IndexKind::Unique => "UNIQUE",
                                    IndexKind::Fulltext => "FULLTEXT",
                                    IndexKind::Spatial => "SPATIAL",
                                }
                                .to_owned(),
                                index
                                    .parts
                                    .iter()
                                    .map(|part| part.column.clone())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                            ]
                        },
                        |count| IndexDraft {
                            name: format!("idx_{}", count + 1),
                            parts: fields
                                .first()
                                .map(|column| IndexPart {
                                    column: column.clone(),
                                    ..Default::default()
                                })
                                .into_iter()
                                .collect(),
                            ..Default::default()
                        },
                    );
                    if let Some(index) = design.extras.indexes.get_mut(design.item) {
                        index_editor(ui, p, en, index, &fields);
                    }
                }
                design_view::DesignPage::ForeignKeys => {
                    item_list(
                        ui,
                        p,
                        en,
                        "foreign_keys",
                        &mut design.extras.foreign_keys,
                        &mut design.item,
                        &[
                            l("Nazwa", "Name"),
                            l("Kolumny", "Columns"),
                            l("Odwołuje się do", "References"),
                            l("Przy usunięciu / zmianie", "On delete / update"),
                        ],
                        |key| {
                            vec![
                                key.name.clone(),
                                key.columns.join(", "),
                                format!("{} ({})", key.ref_table, key.ref_columns.join(", ")),
                                format!("{} / {}", key.on_delete.sql(), key.on_update.sql()),
                            ]
                        },
                        |count| ForeignKeyDraft {
                            name: format!("fk_{}", count + 1),
                            columns: fields.first().cloned().into_iter().collect(),
                            ref_columns: vec![String::new()],
                            ..Default::default()
                        },
                    );
                    let schema = design.table.schema.clone();
                    if let Some(key) = design.extras.foreign_keys.get_mut(design.item) {
                        action = foreign_key_editor(
                            ui,
                            p,
                            en,
                            key,
                            &fields,
                            tables,
                            &schema,
                            &mut design.catalog,
                        );
                    }
                }
                design_view::DesignPage::Checks => {
                    item_list(
                        ui,
                        p,
                        en,
                        "checks",
                        &mut design.extras.checks,
                        &mut design.item,
                        &[l("Nazwa", "Name"), l("Warunek", "Condition")],
                        |check| vec![check.name.clone(), check.expression.replace('\n', " ")],
                        |count| CheckDraft {
                            name: format!("chk_{}", count + 1),
                            ..Default::default()
                        },
                    );
                    if let Some(check) = design.extras.checks.get_mut(design.item) {
                        text_row(ui, p, l("Nazwa", "Name"), &mut check.name, "");
                        code_row(
                            ui,
                            p,
                            l("Warunek", "Condition"),
                            &mut check.expression,
                            4,
                            l(
                                "np. price >= 0 AND quantity > 0",
                                "e.g. price >= 0 AND quantity > 0",
                            ),
                        );
                    }
                }
                design_view::DesignPage::Triggers => {
                    let table = design.table.name.trim().to_owned();
                    item_list(
                        ui,
                        p,
                        en,
                        "triggers",
                        &mut design.extras.triggers,
                        &mut design.item,
                        &[
                            l("Nazwa", "Name"),
                            l("Kiedy", "When"),
                            l("Zdarzenie", "Event"),
                        ],
                        |trigger| {
                            vec![
                                trigger.name.clone(),
                                trigger.timing.sql().to_owned(),
                                trigger.event.sql().to_owned(),
                            ]
                        },
                        |count| TriggerDraft {
                            name: format!("{table}_trg_{}", count + 1),
                            body: "BEGIN\n  \nEND".into(),
                            ..Default::default()
                        },
                    );
                    if let Some(trigger) = design.extras.triggers.get_mut(design.item) {
                        trigger_editor(ui, p, en, trigger);
                    }
                }
                design_view::DesignPage::Options => {
                    options_editor(ui, p, en, &mut design.extras.options, &design.catalog)
                }
                design_view::DesignPage::Partitions => {
                    partitions_editor(ui, p, en, &mut design.extras.partitioning, &mut design.item)
                }
                design_view::DesignPage::Fields | design_view::DesignPage::Sql => {}
            }
        });
    action
}

fn index_editor(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    en: bool,
    index: &mut IndexDraft,
    fields: &[String],
) {
    let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
    text_row(ui, p, l("Nazwa", "Name"), &mut index.name, "");
    row(ui, p, l("Rodzaj", "Kind"), |ui| {
        widgets::combo(ui, p, "index_kind", index.kind.sql(), 200.0, |ui| {
            for kind in IndexKind::ALL {
                ui.selectable_value(&mut index.kind, kind, kind.sql());
            }
        });
    });
    text_row(ui, p, l("Komentarz", "Comment"), &mut index.comment, "");
    let mut remove = None;
    row(ui, p, l("Kolumny", "Columns"), |ui| {
        ui.vertical(|ui| {
            for (number, part) in index.parts.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    name_combo(
                        ui,
                        p,
                        ("index_part", number),
                        &mut part.column,
                        fields,
                        None,
                        200.0,
                    );
                    ui.add(
                        widgets::field(p, &mut part.length)
                            .desired_width(64.0)
                            .hint_text(l("długość", "length")),
                    );
                    widgets::checkbox(ui, p, &mut part.descending, "DESC");
                    if widgets::icon_button(
                        ui,
                        p,
                        Icon::Minus,
                        l("Usuń kolumnę", "Remove the column"),
                    )
                    .clicked()
                    {
                        remove = Some(number);
                    }
                });
            }
            if Btn::quiet(l("Dodaj kolumnę", "Add a column"))
                .icon(Icon::Plus)
                .small()
                .show(ui, p)
                .clicked()
            {
                let unused = fields
                    .iter()
                    .find(|f| !index.parts.iter().any(|part| &part.column == *f))
                    .cloned()
                    .unwrap_or_default();
                index.parts.push(IndexPart {
                    column: unused,
                    ..Default::default()
                });
            }
        });
    });
    if let Some(number) = remove {
        index.parts.remove(number);
    }
}

#[allow(clippy::too_many_arguments)]
fn foreign_key_editor(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    en: bool,
    key: &mut ForeignKeyDraft,
    fields: &[String],
    tables: &[TableRef],
    schema: &str,
    catalog: &mut Catalog,
) -> Option<ExtrasAction> {
    let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
    let mut action = None;
    text_row(ui, p, l("Nazwa", "Name"), &mut key.name, "");
    let target_schema = if key.ref_schema.trim().is_empty() {
        schema.to_owned()
    } else {
        key.ref_schema.trim().to_owned()
    };
    row(ui, p, l("Tabela docelowa", "Referenced table"), |ui| {
        let shown = if key.ref_table.is_empty() {
            "—".to_owned()
        } else if target_schema == schema {
            key.ref_table.clone()
        } else {
            format!("{target_schema}.{}", key.ref_table)
        };
        widgets::combo(ui, p, "fk_table", shown, 300.0, |ui| {
            egui::ScrollArea::vertical()
                .max_height(300.0)
                .show(ui, |ui| {
                    for table in tables.iter().filter(|t| t.kind.contains("table")) {
                        let label = if table.schema == schema {
                            table.name.clone()
                        } else {
                            format!("{}.{}", table.schema, table.name)
                        };
                        let chosen = key.ref_table == table.name && target_schema == table.schema;
                        if ui.selectable_label(chosen, label).clicked() {
                            key.ref_table = table.name.clone();
                            key.ref_schema = if table.schema == schema {
                                String::new()
                            } else {
                                table.schema.clone()
                            };
                        }
                    }
                });
        });
    });
    // The columns of the table pointed at, asked for once.
    let target = ref_key(&target_schema, &key.ref_table);
    if !key.ref_table.is_empty() && !catalog.requested.contains(&target) {
        catalog.requested.insert(target.clone());
        action = Some(ExtrasAction::RefColumns(TableRef {
            schema: target_schema.clone(),
            name: key.ref_table.clone(),
            kind: "table".into(),
        }));
    }
    let ref_columns = catalog.ref_columns.get(&target).cloned();
    let mut remove = None;
    row(ui, p, l("Kolumny", "Columns"), |ui| {
        ui.vertical(|ui| {
            key.ref_columns.resize(key.columns.len(), String::new());
            for (number, (column, target)) in key
                .columns
                .iter_mut()
                .zip(key.ref_columns.iter_mut())
                .enumerate()
            {
                ui.horizontal(|ui| {
                    name_combo(ui, p, ("fk_column", number), column, fields, None, 180.0);
                    ui.label(RichText::new("→").color(p.muted));
                    match &ref_columns {
                        Some(names) => {
                            name_combo(ui, p, ("fk_target", number), target, names, None, 180.0);
                        }
                        None => {
                            ui.add(widgets::field(p, target).desired_width(180.0));
                        }
                    }
                    if widgets::icon_button(ui, p, Icon::Minus, l("Usuń parę", "Remove the pair"))
                        .clicked()
                    {
                        remove = Some(number);
                    }
                });
            }
            if Btn::quiet(l("Dodaj parę kolumn", "Add a pair of columns"))
                .icon(Icon::Plus)
                .small()
                .show(ui, p)
                .clicked()
            {
                key.columns
                    .push(fields.first().cloned().unwrap_or_default());
                key.ref_columns.push(String::new());
            }
        });
    });
    if let Some(number) = remove {
        key.columns.remove(number);
        key.ref_columns.remove(number);
    }
    for (label, action_value) in [
        (
            l("Przy usunięciu (ON DELETE)", "On delete"),
            &mut key.on_delete,
        ),
        (
            l("Przy zmianie (ON UPDATE)", "On update"),
            &mut key.on_update,
        ),
    ] {
        row(ui, p, label, |ui| {
            widgets::combo(
                ui,
                p,
                ("fk_action", label),
                action_value.sql(),
                200.0,
                |ui| {
                    for choice in RefAction::ALL {
                        ui.selectable_value(action_value, choice, choice.sql());
                    }
                },
            );
        });
    }
    action
}

fn trigger_editor(ui: &mut egui::Ui, p: &theme::Palette, en: bool, trigger: &mut TriggerDraft) {
    let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
    text_row(ui, p, l("Nazwa", "Name"), &mut trigger.name, "");
    row(ui, p, l("Kiedy", "When"), |ui| {
        widgets::combo(ui, p, "trigger_timing", trigger.timing.sql(), 160.0, |ui| {
            for timing in Timing::ALL {
                ui.selectable_value(&mut trigger.timing, timing, timing.sql());
            }
        });
    });
    row(ui, p, l("Zdarzenie", "Event"), |ui| {
        widgets::combo(ui, p, "trigger_event", trigger.event.sql(), 160.0, |ui| {
            for event in Event::ALL {
                ui.selectable_value(&mut trigger.event, event, event.sql());
            }
        });
    });
    code_row(
        ui,
        p,
        l("Treść", "Body"),
        &mut trigger.body,
        12,
        l(
            "SET NEW.kolumna = …; kilka poleceń w BEGIN … END",
            "SET NEW.column = …; several statements in BEGIN … END",
        ),
    );
    widgets::help(
        ui,
        p,
        l(
            "NEW to wiersz po zmianie, OLD przed nią. Zmiana wyzwalacza usuwa go i tworzy na nowo.",
            "NEW is the row after the change, OLD before it. A changed trigger is dropped and created again.",
        ),
    );
}

fn options_editor(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    en: bool,
    options: &mut TableOptions,
    catalog: &Catalog,
) {
    let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
    let server_default = l("domyślny serwera", "server default");
    let mut engines = catalog.engines.clone();
    if !options.engine.is_empty()
        && !engines
            .iter()
            .any(|e| e.eq_ignore_ascii_case(&options.engine))
    {
        engines.push(options.engine.clone());
    }
    row(ui, p, l("Silnik", "Engine"), |ui| {
        name_combo(
            ui,
            p,
            "table_engine",
            &mut options.engine,
            &engines,
            Some(server_default),
            240.0,
        );
    });
    row(ui, p, l("Zestaw znaków", "Character set"), |ui| {
        if name_combo(
            ui,
            p,
            "table_charset",
            &mut options.charset,
            &catalog.charsets,
            Some(server_default),
            240.0,
        ) && !options.collation.is_empty()
            && !catalog
                .collations
                .iter()
                .any(|(c, set)| *c == options.collation && *set == options.charset)
        {
            // A collation of another character set does not fit.
            options.collation.clear();
        }
    });
    let collations: Vec<String> = catalog
        .collations
        .iter()
        .filter(|(_, set)| options.charset.is_empty() || *set == options.charset)
        .map(|(collation, _)| collation.clone())
        .collect();
    row(ui, p, l("Porównywanie", "Collation"), |ui| {
        name_combo(
            ui,
            p,
            "table_collation",
            &mut options.collation,
            &collations,
            Some(server_default),
            240.0,
        );
    });
    text_row(
        ui,
        p,
        "AUTO_INCREMENT",
        &mut options.auto_increment,
        l("następna wartość", "next value"),
    );
    text_row(ui, p, l("Komentarz", "Comment"), &mut options.comment, "");
    ui.add_space(4.0);
    widgets::hline(ui, p);
    ui.add_space(4.0);
    let settings = mysql::engine_options(&options.engine);
    if options.engine.is_empty() {
        widgets::help(
            ui,
            p,
            l(
                "Wybierz silnik, aby zobaczyć jego ustawienia.",
                "Choose an engine to see its settings.",
            ),
        );
        return;
    }
    widgets::caption(
        ui,
        p,
        &format!(
            "{} {}",
            l("Ustawienia silnika", "Settings of the engine"),
            options.engine
        ),
    );
    if settings.is_empty() {
        widgets::help(
            ui,
            p,
            l(
                "Ten silnik nie ma ustawień tabeli.",
                "This engine has no table settings.",
            ),
        );
    }
    for setting in settings {
        let label = if en { setting.label.1 } else { setting.label.0 };
        let mut value = options
            .settings
            .get(setting.key)
            .cloned()
            .unwrap_or_default();
        row(ui, p, label, |ui| match setting.values {
            SettingValues::Choice(choices) => {
                let names: Vec<String> = choices.iter().map(|c| (*c).to_owned()).collect();
                name_combo(
                    ui,
                    p,
                    ("engine_setting", setting.key),
                    &mut value,
                    &names,
                    Some("—"),
                    200.0,
                );
            }
            SettingValues::Number(max) => {
                ui.add(
                    widgets::field(p, &mut value)
                        .desired_width(120.0)
                        .hint_text(format!("0 … {max}")),
                );
            }
        });
        if value.is_empty() {
            options.settings.remove(setting.key);
        } else {
            options.settings.insert(setting.key.to_owned(), value);
        }
    }
}

fn partitions_editor(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    en: bool,
    partitioning: &mut Partitioning,
    chosen: &mut usize,
) {
    let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
    let none = l("Bez partycji", "No partitions");
    let label = |method: PartitionMethod| {
        if method == PartitionMethod::None {
            none.to_owned()
        } else {
            method.sql().to_owned()
        }
    };
    row(ui, p, l("Podział", "Partitioned by"), |ui| {
        widgets::combo(
            ui,
            p,
            "partition_method",
            label(partitioning.method),
            220.0,
            |ui| {
                for method in PartitionMethod::ALL {
                    ui.selectable_value(&mut partitioning.method, method, label(method));
                }
            },
        );
    });
    let method = partitioning.method;
    if method == PartitionMethod::None {
        return;
    }
    let columns = matches!(
        method,
        PartitionMethod::RangeColumns
            | PartitionMethod::ListColumns
            | PartitionMethod::Key
            | PartitionMethod::LinearKey
    );
    text_row(
        ui,
        p,
        if columns {
            l("Kolumny", "Columns")
        } else {
            l("Wyrażenie", "Expression")
        },
        &mut partitioning.expression,
        match method {
            PartitionMethod::Key | PartitionMethod::LinearKey => {
                l("puste: klucz główny", "empty: the primary key")
            }
            _ if columns => "created_at, region",
            _ => "YEAR(created_at)",
        },
    );
    if !method.listed() {
        text_row(
            ui,
            p,
            l("Liczba partycji", "Partitions"),
            &mut partitioning.count,
            l(
                "gdy nie są wypisane poniżej",
                "when they are not listed below",
            ),
        );
    }
    widgets::help(
        ui,
        p,
        l(
            "Zmiana partycji przebudowuje całą tabelę; przy dużej tabeli może to trwać.",
            "Changing the partitions rebuilds the whole table; a large one can take a while.",
        ),
    );
    item_list(
        ui,
        p,
        en,
        "partitions",
        &mut partitioning.partitions,
        chosen,
        &[
            l("Nazwa", "Name"),
            l("Wartości", "Values"),
            l("Komentarz", "Comment"),
        ],
        |partition| {
            vec![
                partition.name.clone(),
                partition.values.clone(),
                partition.comment.clone(),
            ]
        },
        |count| PartitionDef {
            name: format!("p{count}"),
            ..Default::default()
        },
    );
    if let Some(partition) = partitioning.partitions.get_mut(*chosen) {
        text_row(ui, p, l("Nazwa", "Name"), &mut partition.name, "");
        if method.listed() {
            text_row(
                ui,
                p,
                if matches!(
                    method,
                    PartitionMethod::Range | PartitionMethod::RangeColumns
                ) {
                    l("Mniejsze niż", "Less than")
                } else {
                    l("Wartości", "Values")
                },
                &mut partition.values,
                match method {
                    PartitionMethod::Range => "2025 / MAXVALUE",
                    PartitionMethod::RangeColumns => "2025, 'm'",
                    _ => "1, 2, 3",
                },
            );
        }
        text_row(ui, p, l("Komentarz", "Comment"), &mut partition.comment, "");
    }
}
