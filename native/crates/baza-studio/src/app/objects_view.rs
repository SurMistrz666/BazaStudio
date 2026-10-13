//! "Obiekty": the tables and views of a schema, or of one folder of it, as a
//! list with their actions: open the data, show
//! the structure, start a new table and drop the selected ones. Several are
//! selected with ⌘ or Ctrl and Shift, and copied as names.
use super::shortcuts::{take, take_mod};
use super::*;
use crate::widgets::Btn;
use baza_core::engine::BrowseModel;
use egui_extras::{Column, TableBuilder};
use std::collections::BTreeSet;

/// Left padding of the text in a row of the list.
const CELL_PAD: f32 = 13.0;

/// The folder of the side panel the list shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ObjectsTarget {
    pub connection: Uuid,
    pub schema: String,
    /// One folder of the schema, or all of it.
    pub kind: Option<ObjectKind>,
}

#[derive(Default)]
pub(super) struct ObjectsState {
    pub target: Option<ObjectsTarget>,
    /// Schema and name of each selected object.
    pub selected: BTreeSet<(String, String)>,
    /// Place in the list a Shift-click selects from.
    pub anchor: Option<usize>,
    /// Place the arrow keys move from; with Shift the selection runs from
    /// the anchor to it.
    pub cursor: Option<usize>,
    /// A row to bring into view in the next frame.
    pub scroll_to: Option<usize>,
    pub search: String,
    /// The information panel on the right is hidden.
    pub hide_info: bool,
    /// Facts about the selected object for the information panel.
    pub details: Option<DetailsUi>,
}

/// What the information panel shows: facts the driver gave about one object.
pub(super) struct DetailsUi {
    pub connection: Uuid,
    pub table: TableRef,
    pub request: Uuid,
    pub result: Option<Result<QueryResult, String>>,
}

/// The columns of one table, shown read-only.
pub(super) struct StructureUi {
    pub request: Uuid,
    pub connection: Uuid,
    pub table: TableRef,
    pub columns: Option<Result<Vec<ColumnInfo>, String>>,
}

/// What the toolbar, a row's menu or the keyboard asked for in one frame.
#[derive(Default)]
struct Asked {
    open: bool,
    query: bool,
    structure: bool,
    builder: bool,
    designer: bool,
    drop: bool,
    refresh: bool,
    copy: bool,
    connect: bool,
}

fn key(table: &TableRef) -> (String, String) {
    (table.schema.clone(), table.name.clone())
}

impl StudioApp {
    /// Show a schema, or one folder of it, of the side panel in "Obiekty".
    pub(super) fn show_objects(
        &mut self,
        connection: Uuid,
        schema: &str,
        kind: Option<ObjectKind>,
    ) {
        let target = ObjectsTarget {
            connection,
            schema: schema.to_owned(),
            kind,
        };
        if self.objects.target.as_ref() != Some(&target) {
            self.objects.selected.clear();
            self.objects.anchor = None;
            self.objects.cursor = None;
            self.objects.target = Some(target);
        }
        self.section = Section::Objects;
    }

    /// "Tabela" or "Widok" of the ribbon: that folder of the schema on
    /// screen, or of the first schema of the connection in focus.
    pub(super) fn show_object_kind(&mut self, kind: ObjectKind) {
        let target = self
            .objects
            .target
            .clone()
            .or_else(|| self.default_objects_target());
        match target {
            Some(target) => {
                self.show_objects(target.connection, &target.schema.clone(), Some(kind))
            }
            None => self.section = Section::Objects,
        }
    }

    /// Without a chosen folder: the tables of the first schema of the
    /// connection in focus, when it is connected.
    fn default_objects_target(&self) -> Option<ObjectsTarget> {
        let connection = self.focus_connection()?;
        let tables = self.tables.get(&connection)?;
        let schema = tables
            .iter()
            .filter(|t| ObjectKind::from_metadata(&t.kind) == ObjectKind::Table)
            .map(|t| t.schema.clone())
            .min()?;
        Some(ObjectsTarget {
            connection,
            schema,
            kind: Some(ObjectKind::Table),
        })
    }

    /// The objects of the target after the search, as the list shows them:
    /// by kind, then by name.
    fn listed_objects(&self, target: &ObjectsTarget) -> Vec<TableRef> {
        let search = self.objects.search.trim().to_lowercase();
        let mut list: Vec<TableRef> = self
            .tables
            .get(&target.connection)
            .map(|tables| {
                tables
                    .iter()
                    .filter(|t| t.schema == target.schema)
                    .filter(|t| {
                        target
                            .kind
                            .is_none_or(|kind| ObjectKind::from_metadata(&t.kind) == kind)
                    })
                    .filter(|t| search.is_empty() || t.name.to_lowercase().contains(&search))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        list.sort_by(|a, b| {
            ObjectKind::from_metadata(&a.kind)
                .cmp(&ObjectKind::from_metadata(&b.kind))
                .then_with(|| a.name.cmp(&b.name))
        });
        list.dedup_by(|a, b| a.schema == b.schema && a.name == b.name);
        list
    }

    pub(super) fn objects_view(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        if self.objects.target.is_none() {
            self.objects.target = self.default_objects_target();
        }
        let Some(target) = self.objects.target.clone() else {
            let focus = self.focus_connection();
            let connect = focus.filter(|id| !self.databases.contains_key(id));
            egui::Frame::NONE
                .inner_margin(egui::Margin::same(20))
                .show(ui, |ui| {
                    if widgets::empty_state(
                        ui,
                        &p,
                        Icon::Folder,
                        l("Wybierz folder", "Choose a folder"),
                        l(
                            "Kliknij folder Tabele albo schemat w panelu Połączenia: tutaj pojawi się lista jego tabel i widoków z akcjami.",
                            "Click the Tables folder or a schema in the Connections panel: its tables and views are listed here with their actions.",
                        ),
                        connect.map(|_| l("Połącz", "Connect")),
                    ) && let Some(id) = connect
                    {
                        self.connect(id);
                    }
                });
            return;
        };
        let connection = target.connection;
        let connected = self.databases.contains_key(&connection);
        let caps = *self.capabilities(Some(connection));
        let list = self.listed_objects(&target);
        // A selection the list no longer shows (dropped, or hidden by the
        // search) is not acted on.
        self.objects
            .selected
            .retain(|(schema, name)| list.iter().any(|t| &t.schema == schema && &t.name == name));
        let can_drop = caps.language == QueryLanguage::Sql && caps.table_designer;
        let containers = localized(caps.browse.containers(), en);
        let folder = match target.kind {
            Some(ObjectKind::Table) | None => containers,
            Some(ObjectKind::View) => l("Widoki", "Views"),
            Some(ObjectKind::MaterializedView) => l("Widoki materializowane", "Materialized views"),
            Some(ObjectKind::Other) => l("Inne obiekty", "Other objects"),
        };
        let refresh_label = self.refresh_label(connection);
        let connection_name = self
            .connection_name(connection)
            .unwrap_or_default()
            .to_owned();
        let mut asked = Asked::default();
        let selected_count = self.objects.selected.len();

        widgets::toolbar(ui, &p, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            asked.open = Btn::quiet(l("Otwórz dane", "Open data"))
                .icon(Icon::Table)
                .small()
                .enabled(connected && selected_count > 0)
                .tip(l(
                    "Wiersze zaznaczonych obiektów, każdy na swojej karcie (Enter)",
                    "The rows of the selected objects, each on its own tab (Enter)",
                ))
                .show(ui, &p)
                .clicked();
            asked.structure = Btn::quiet(if caps.table_designer {
                l("Projektuj tabelę", "Design table")
            } else {
                l("Struktura", "Structure")
            })
            .icon(Icon::Schema)
            .small()
            .enabled(connected && selected_count == 1)
            .tip(l(
                "Kolumny zaznaczonej tabeli: typy, klucz, wartości domyślne",
                "The columns of the selected table: types, key, defaults",
            ))
            .show(ui, &p)
            .clicked();
            asked.designer = Btn::quiet(l("Nowa tabela", "New table"))
                .icon(Icon::Plus)
                .small()
                .enabled(connected && caps.table_designer)
                .show(ui, &p)
                .clicked();
            asked.drop = Btn::quiet(l("Usuń", "Delete"))
                .icon(Icon::Trash)
                .small()
                .enabled(connected && can_drop && selected_count > 0)
                .tip(l(
                    "Usuwa zaznaczone tabele i widoki z bazy, po potwierdzeniu (Delete)",
                    "Drops the selected tables and views from the database, after a confirmation (Delete)",
                ))
                .show(ui, &p)
                .clicked();
            widgets::right(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if widgets::icon_toggle(
                    ui,
                    &p,
                    Icon::Sidebar,
                    l("Panel informacji", "Information panel"),
                    !self.objects.hide_info,
                )
                .clicked()
                {
                    self.objects.hide_info = !self.objects.hide_info;
                }
                asked.refresh =
                    widgets::icon_button(ui, &p, Icon::Refresh, refresh_label).clicked();
                let width = (ui.available_width() - 12.0).clamp(90.0, 220.0);
                ui.add(
                    widgets::field(&p, &mut self.objects.search)
                        .id(egui::Id::new("objects_search"))
                        .desired_width(width - 18.0)
                        .hint_text(l("Szukaj po nazwie", "Search by name")),
                );
            });
        });

        // Where the list is and what is selected in it.
        let summary = {
            let count = list.len();
            let mut text = if en {
                format!("{count} {}", if count == 1 { "object" } else { "objects" })
            } else {
                format!("Obiekty: {count}")
            };
            if selected_count > 0 {
                text.push_str(&if en {
                    format!(" · {selected_count} selected")
                } else {
                    format!(" · zaznaczone: {selected_count}")
                });
            }
            text
        };
        egui::Frame::NONE
            .fill(p.side)
            .inner_margin(egui::Margin::symmetric(16, 0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_height(p.control_height() + 2.0);
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let mut path = vec![connection_name.as_str()];
                    if caps.schemas && !target.schema.is_empty() {
                        path.push(target.schema.as_str());
                    }
                    if target.kind.is_some() {
                        path.push(folder);
                    }
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(path.join("  ›  "))
                                .size(p.small() + 0.5)
                                .color(p.text),
                        )
                        .selectable(false)
                        .truncate(),
                    );
                    widgets::right(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(summary)
                                    .size(p.small() + 0.5)
                                    .color(p.muted),
                            )
                            .selectable(false),
                        );
                    });
                });
            });
        widgets::hline(ui, &p);

        // The panel of facts about the selected object.
        if connected && !self.objects.hide_info && !list.is_empty() {
            let single = (selected_count == 1)
                .then(|| {
                    list.iter()
                        .find(|t| self.objects.selected.contains(&key(t)))
                        .cloned()
                })
                .flatten();
            if let Some(table) = &single
                && self.objects.details.as_ref().is_none_or(|d| {
                    d.connection != connection
                        || d.table.schema != table.schema
                        || d.table.name != table.name
                })
            {
                self.load_details(connection, table.clone());
            }
            egui::Panel::right("object_info")
                .resizable(true)
                .default_size(270.0)
                .size_range(200.0..=460.0)
                .frame(
                    egui::Frame::NONE
                        .fill(p.side)
                        .inner_margin(egui::Margin::symmetric(16, 14)),
                )
                .show(ui, |ui| self.object_info(ui, single.as_ref(), caps.browse));
        }

        if !connected {
            let connecting = self.connect_requests.contains_key(&connection);
            egui::Frame::NONE
                .inner_margin(egui::Margin::same(20))
                .show(ui, |ui| {
                    if connecting {
                        ui.label(
                            egui::RichText::new(l("Łączenie…", "Connecting…"))
                                .size(p.body() - 0.5)
                                .color(p.muted),
                        );
                    } else {
                        asked.connect = widgets::empty_state(
                            ui,
                            &p,
                            Icon::Database,
                            l("Baza nie jest połączona", "The database is not connected"),
                            l(
                                "Połącz się, aby zobaczyć jej tabele i widoki.",
                                "Connect to see its tables and views.",
                            ),
                            Some(l("Połącz", "Connect")),
                        );
                    }
                });
        } else if list.is_empty() {
            let text = if self.objects.search.trim().is_empty() {
                l(
                    "Ten folder jest pusty. Nowa tabela pojawi się tutaj po odświeżeniu listy.",
                    "This folder is empty. A new table shows up here once the list is refreshed.",
                )
            } else {
                l(
                    "Żadna nazwa nie zawiera wpisanego tekstu.",
                    "No name contains the text you typed.",
                )
            };
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(20, 18))
                .show(ui, |ui| widgets::help(ui, &p, text));
        } else {
            self.objects_list(ui, &list, caps, can_drop, &mut asked);
        }

        // Keys of the list that work wherever the keyboard is, except in a
        // text field or behind a dialog: Mod+F, Mod+N, Mod+D and Ctrl+Q.
        let modal = ui.ctx().memory(|m| m.top_modal_layer()).is_some();
        if !modal && !ui.ctx().text_edit_focused() {
            if take_mod(ui, false, egui::Key::F) {
                ui.memory_mut(|m| m.request_focus(egui::Id::new("objects_search")));
            }
            asked.designer |= take_mod(ui, false, egui::Key::N) && caps.table_designer;
            asked.structure |= take_mod(ui, false, egui::Key::D) && selected_count == 1;
            asked.query |= take(ui, egui::Modifiers::CTRL, egui::Key::Q) && selected_count > 0;
        }
        let chosen: Vec<TableRef> = list
            .iter()
            .filter(|t| self.objects.selected.contains(&key(t)))
            .cloned()
            .collect();
        if asked.copy && !chosen.is_empty() {
            let names = chosen
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            self.copy_out(ui.ctx(), names);
            let text = if en {
                format!("Copied names: {}.", chosen.len())
            } else {
                format!("Skopiowane nazwy: {}.", chosen.len())
            };
            self.notify(text, false);
        }
        if asked.connect {
            self.connect(connection);
        }
        if asked.refresh {
            self.refresh_tables(Some(connection));
        }
        if asked.designer {
            self.open_designer(connection);
        }
        if asked.drop && can_drop && !chosen.is_empty() {
            self.drop_confirm = Some((connection, chosen.clone()));
        }
        if asked.structure
            && let Some(table) = chosen.first()
        {
            // The designer where it serves the
            // database, the read-only structure elsewhere.
            if caps.table_designer && ObjectKind::from_metadata(&table.kind) == ObjectKind::Table {
                self.open_design(connection, table.clone());
            } else {
                self.show_structure(connection, table.clone());
            }
        }
        if asked.builder
            && let Some(table) = chosen.first()
        {
            self.open_builder(connection, table.clone());
        }
        if asked.query
            && let Some(table) = chosen.first()
        {
            self.query_object(connection, table.clone());
        }
        if asked.open {
            // Each table on its own tab; the last one stays
            // on screen.
            for table in chosen.into_iter().take(12) {
                self.open_table(connection, table);
            }
        }
    }

    /// The rows of the list: one object each, with its kind.
    fn objects_list(
        &mut self,
        ui: &mut egui::Ui,
        list: &[TableRef],
        caps: Capabilities,
        can_drop: bool,
        asked: &mut Asked,
    ) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let striped = self.profiles.preferences.grid_striped;
        let row_height = p.row_height() + 2.0;
        let line = egui::Stroke::new(1.0, p.line);
        let list_id = egui::Id::new("objects_list");
        // The list takes the keyboard once a row is clicked, so Enter or
        // Delete never reach the side panel at the same time. The arrows and
        // Esc stay with it instead of moving the focus.
        let anchor = egui::Rect::from_min_size(ui.cursor().min, egui::vec2(1.0, 1.0));
        ui.interact(anchor, list_id, egui::Sense::focusable_noninteractive());
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                list_id,
                egui::EventFilter {
                    tab: false,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            )
        });
        let selected = self.objects.selected.clone();
        let (command, shift) = ui.input(|i| (i.modifiers.command, i.modifiers.shift));
        let mut clicked: Option<usize> = None;
        let mut menu_on: Option<usize> = None;
        let mut double: Option<usize> = None;
        let mut table = TableBuilder::new(ui)
            .id_salt("objects_table")
            .striped(striped)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .min_scrolled_height(60.0);
        if let Some(row) = self.objects.scroll_to.take() {
            table = table.scroll_to_row(row, None);
        }
        table
            .column(Column::remainder().at_least(180.0).clip(true))
            .column(Column::exact(200.0))
            .header(row_height + 2.0, |mut header| {
                for title in [l("Nazwa", "Name"), l("Rodzaj", "Kind")] {
                    header.col(|ui| {
                        let rect = ui.max_rect();
                        let painter = ui.painter();
                        painter.rect_filled(rect, 0.0, p.side);
                        painter.hline(rect.x_range(), rect.bottom() - 0.5, line);
                        painter.text(
                            egui::pos2(rect.left() + CELL_PAD, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            title,
                            theme::medium(p.body()),
                            p.muted,
                        );
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, list.len(), |mut row| {
                    let i = row.index();
                    let table = &list[i];
                    let is_selected = selected.contains(&key(table));
                    row.set_selected(is_selected);
                    let kind = ObjectKind::from_metadata(&table.kind);
                    let icon = match kind {
                        ObjectKind::Table => Icon::Table,
                        ObjectKind::View | ObjectKind::MaterializedView => Icon::View,
                        ObjectKind::Other => Icon::File,
                    };
                    let (_, name) = row.col(|ui| {
                        let rect = ui.max_rect();
                        let painter = ui.painter();
                        painter.hline(rect.x_range(), rect.bottom() - 0.5, line);
                        let side = p.icon();
                        widgets::paint(
                            painter,
                            egui::Rect::from_center_size(
                                egui::pos2(rect.left() + CELL_PAD + side / 2.0, rect.center().y),
                                egui::vec2(side, side),
                            ),
                            icon,
                            if is_selected { p.accent } else { p.muted },
                        );
                        let left = rect.left() + CELL_PAD + side + 8.0;
                        let galley = widgets::one_line(
                            painter,
                            &table.name,
                            egui::FontId::proportional(p.body()),
                            p.text,
                            (rect.right() - left - 6.0).max(8.0),
                        );
                        painter.galley(
                            egui::pos2(left, rect.center().y - galley.size().y / 2.0),
                            galley,
                            p.text,
                        );
                    });
                    let (_, kind_cell) = row.col(|ui| {
                        let rect = ui.max_rect();
                        let painter = ui.painter();
                        painter.hline(rect.x_range(), rect.bottom() - 0.5, line);
                        painter.text(
                            egui::pos2(rect.left() + CELL_PAD, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            kind_label(table, caps.browse, en),
                            egui::FontId::proportional(p.small() + 0.5),
                            p.muted,
                        );
                    });
                    let response = name | kind_cell;
                    if response.clicked() {
                        clicked = Some(i);
                    }
                    if response.double_clicked() {
                        double = Some(i);
                    }
                    if response.secondary_clicked() {
                        menu_on = Some(i);
                    }
                    response.context_menu(|ui| {
                        let many = selected.len() > 1 && is_selected;
                        if ui
                            .button(if many {
                                l("Otwórz dane zaznaczonych", "Open data of the selected")
                            } else {
                                l("Otwórz dane", "Open data")
                            })
                            .clicked()
                        {
                            asked.open = true;
                            ui.close();
                        }
                        if !many
                            && ui
                                .button(if caps.table_designer {
                                    l("Projektuj tabelę", "Design table")
                                } else {
                                    l("Struktura", "Structure")
                                })
                                .clicked()
                        {
                            asked.structure = true;
                            ui.close();
                        }
                        if !many
                            && caps.language != QueryLanguage::None
                            && ui
                                .add(
                                    egui::Button::new(l(
                                        "Zapytanie w edytorze",
                                        "Query in the editor",
                                    ))
                                    .shortcut_text("Ctrl+Q"),
                                )
                                .clicked()
                        {
                            asked.query = true;
                            ui.close();
                        }
                        if !many
                            && caps.query_builder
                            && ui.button(l("Kreator zapytań", "Query builder")).clicked()
                        {
                            asked.builder = true;
                            ui.close();
                        }
                        if ui
                            .button(if many {
                                l("Kopiuj nazwy", "Copy names")
                            } else {
                                l("Kopiuj nazwę", "Copy name")
                            })
                            .clicked()
                        {
                            asked.copy = true;
                            ui.close();
                        }
                        if can_drop {
                            ui.separator();
                            if ui
                                .button(if many {
                                    l("Usuń zaznaczone…", "Delete the selected…")
                                } else {
                                    l("Usuń…", "Delete…")
                                })
                                .clicked()
                            {
                                asked.drop = true;
                                ui.close();
                            }
                        }
                    });
                });
            });

        let objects = &mut self.objects;
        if let Some(i) = clicked.or(menu_on) {
            ui.memory_mut(|m| m.request_focus(list_id));
            let table = key(&list[i]);
            if clicked.is_some()
                && shift
                && let Some(anchor) = objects.anchor
            {
                // Shift selects everything between the anchor and this row.
                let (from, to) = (anchor.min(i), anchor.max(i).min(list.len() - 1));
                if !command {
                    objects.selected.clear();
                }
                objects.selected.extend(list[from..=to].iter().map(key));
                objects.cursor = Some(i);
            } else if clicked.is_some() && command {
                // ⌘ or Ctrl adds the row to the selection or takes it out.
                if !objects.selected.remove(&table) {
                    objects.selected.insert(table);
                }
                objects.anchor = Some(i);
                objects.cursor = Some(i);
            } else if clicked.is_some() || !objects.selected.contains(&table) {
                // A right click on a row outside the selection selects it.
                objects.selected = BTreeSet::from([table]);
                objects.anchor = Some(i);
                objects.cursor = Some(i);
            }
        }
        if let Some(i) = double {
            objects.selected = BTreeSet::from([key(&list[i])]);
            objects.anchor = Some(i);
            objects.cursor = Some(i);
            asked.open = true;
        }
        // Keys while the list has the keyboard: ⌘A, ⌘C, Enter, Delete,
        // arrows and Esc.
        if ui.memory(|m| m.has_focus(list_id)) && !ui.ctx().text_edit_focused() {
            let none = egui::Modifiers::NONE;
            if take_mod(ui, false, egui::Key::A) {
                objects.selected = list.iter().map(key).collect();
            }
            asked.copy |= ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
            asked.open |= take(ui, none, egui::Key::Enter) && !objects.selected.is_empty();
            let delete = take(ui, none, egui::Key::Delete)
                || take(ui, none, egui::Key::Backspace)
                || take_mod(ui, false, egui::Key::Delete)
                || take_mod(ui, false, egui::Key::Backspace);
            asked.drop |= delete && can_drop;
            if take(ui, none, egui::Key::Escape) {
                objects.selected.clear();
            }
            // Arrows, Home and End move; with Shift the selection runs from
            // the anchor to where they move.
            let last = list.len() - 1;
            let at = objects.cursor.or(objects.anchor).unwrap_or(0).min(last);
            let mut moved = None;
            for key in [
                egui::Key::ArrowUp,
                egui::Key::ArrowDown,
                egui::Key::Home,
                egui::Key::End,
            ] {
                for extend in [false, true] {
                    let modifiers = if extend { egui::Modifiers::SHIFT } else { none };
                    if take(ui, modifiers, key) {
                        let next = match key {
                            egui::Key::ArrowUp => at.saturating_sub(1),
                            egui::Key::ArrowDown => (at + 1).min(last),
                            egui::Key::Home => 0,
                            _ => last,
                        };
                        moved = Some((next, extend));
                    }
                }
            }
            if let Some((next, extend)) = moved {
                if extend {
                    let anchor = objects.anchor.unwrap_or(at).min(last);
                    objects.selected = list[anchor.min(next)..=anchor.max(next)]
                        .iter()
                        .map(key)
                        .collect();
                    objects.anchor = Some(anchor);
                } else {
                    objects.selected = BTreeSet::from([key(&list[next])]);
                    objects.anchor = Some(next);
                }
                objects.cursor = Some(next);
                objects.scroll_to = Some(next);
            }
        }
    }

    /// Ask the driver for the facts of the information panel.
    fn load_details(&mut self, connection: Uuid, table: TableRef) {
        let Some(db) = self.databases.get(&connection).cloned() else {
            return;
        };
        let request = Uuid::new_v4();
        self.objects.details = Some(DetailsUi {
            connection,
            table: table.clone(),
            request,
            result: None,
        });
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let r = db
                .container_details(&table)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Message::Details(request, r)).await;
            ctx.request_repaint();
        });
    }

    /// The information panel: the selected object and the facts about it.
    fn object_info(&mut self, ui: &mut egui::Ui, table: Option<&TableRef>, browse: BrowseModel) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let Some(table) = table else {
            widgets::help(
                ui,
                &p,
                l(
                    "Wybierz jeden obiekt, aby zobaczyć jego szczegóły.",
                    "Select one object to see its details.",
                ),
            );
            return;
        };
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let icon = match ObjectKind::from_metadata(&table.kind) {
                ObjectKind::View | ObjectKind::MaterializedView => Icon::View,
                ObjectKind::Other => Icon::File,
                ObjectKind::Table => Icon::Table,
            };
            widgets::icon(ui, icon, 34.0, p.accent);
            ui.vertical(|ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(&table.name)
                            .font(theme::strong(p.font + 1.0))
                            .color(p.text),
                    )
                    .truncate(),
                );
                ui.label(
                    egui::RichText::new(kind_label(table, browse, en))
                        .size(p.small())
                        .color(p.muted),
                );
            });
        });
        ui.add_space(10.0);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let fact = |ui: &mut egui::Ui, label: &str, value: &str| {
                    ui.label(egui::RichText::new(label).size(p.small()).color(p.muted));
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(value)
                                .size(p.body() - 0.5)
                                .color(p.text),
                        )
                        .wrap(),
                    );
                    ui.add_space(8.0);
                };
                if !table.schema.is_empty() {
                    fact(ui, l("Schemat", "Schema"), &table.schema);
                }
                match self
                    .objects
                    .details
                    .as_ref()
                    .and_then(|d| d.result.as_ref())
                {
                    None => {
                        ui.label(egui::RichText::new(l("Wczytywanie…", "Loading…")).color(p.muted));
                    }
                    Some(Err(error)) => {
                        let text = self.explain(error.clone());
                        widgets::help(ui, &p, &text);
                    }
                    Some(Ok(result)) => {
                        if let Some(row) = result.rows.first() {
                            for (column, cell) in result.columns.iter().zip(row) {
                                let value = detail_value(&column.name, cell, en);
                                if column.name == "definition" {
                                    ui.label(
                                        egui::RichText::new(detail_label(&column.name, en))
                                            .size(p.small())
                                            .color(p.muted),
                                    );
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(value)
                                                .monospace()
                                                .size(p.code() - 1.5)
                                                .color(p.text),
                                        )
                                        .wrap(),
                                    );
                                } else {
                                    fact(ui, &detail_label(&column.name, en), &value);
                                }
                            }
                        }
                    }
                }
            });
    }

    /// Read the columns of a table for the structure window.
    pub(super) fn show_structure(&mut self, connection: Uuid, table: TableRef) {
        let Some(db) = self.databases.get(&connection).cloned() else {
            self.connect(connection);
            return;
        };
        let request = Uuid::new_v4();
        self.structure = Some(StructureUi {
            request,
            connection,
            table: table.clone(),
            columns: None,
        });
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let r = db.columns(&table).await.map_err(|e| e.to_string());
            let _ = tx.send(Message::Structure(request, r)).await;
            ctx.request_repaint();
        });
    }

    /// The structure window and the confirmation before dropping objects.
    pub(super) fn objects_windows(&mut self, ctx: &egui::Context) {
        self.structure_window(ctx);
        self.drop_window(ctx);
    }

    fn structure_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.structure.take() else {
            return;
        };
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let (mut open, mut copy, mut close) = (false, false, false);
        let title = format!("{}: {}", l("Struktura", "Structure"), state.table.name);
        let dialog = widgets::dialog(
            ctx,
            &p,
            "table_structure",
            &title,
            640.0,
            l("Zamknij", "Close"),
            |ui| {
                widgets::dialog_body(ui, |ui| match &state.columns {
                    None => super::sql_view::hint(
                        ui,
                        &p,
                        l("Wczytywanie kolumn…", "Loading the columns…"),
                    ),
                    Some(Err(error)) => {
                        widgets::feedback(ui, &p, &self.explain(error.clone()), true);
                    }
                    Some(Ok(columns)) => {
                        egui::ScrollArea::vertical()
                            .max_height((ctx.content_rect().height() * 0.6).max(160.0))
                            .show(ui, |ui| {
                                egui::Grid::new("structure_grid")
                                    .num_columns(5)
                                    .striped(true)
                                    .spacing(egui::vec2(18.0, 6.0))
                                    .show(ui, |ui| {
                                        for heading in [
                                            l("Kolumna", "Column"),
                                            l("Typ", "Type"),
                                            "NULL",
                                            l("Klucz", "Key"),
                                            l("Domyślnie", "Default"),
                                        ] {
                                            ui.label(
                                                egui::RichText::new(heading)
                                                    .font(theme::medium(p.small() + 0.5))
                                                    .color(p.muted),
                                            );
                                        }
                                        ui.end_row();
                                        for column in columns {
                                            ui.label(
                                                egui::RichText::new(&column.name).color(p.text),
                                            );
                                            ui.label(
                                                egui::RichText::new(
                                                    column.data_type.to_lowercase(),
                                                )
                                                .color(p.muted),
                                            );
                                            ui.label(if column.nullable {
                                                l("tak", "yes")
                                            } else {
                                                l("nie", "no")
                                            });
                                            ui.label(if column.primary_key {
                                                l("główny", "primary")
                                            } else {
                                                ""
                                            });
                                            ui.label(
                                                egui::RichText::new(
                                                    column.default.as_deref().unwrap_or(""),
                                                )
                                                .color(p.muted),
                                            );
                                            ui.end_row();
                                        }
                                    });
                            });
                    }
                });
                widgets::dialog_footer(ui, &p, |ui| {
                    widgets::right(ui, |ui| {
                        open = Btn::primary(l("Otwórz dane", "Open data"))
                            .show(ui, &p)
                            .clicked();
                        copy = Btn::new(l("Kopiuj nazwy kolumn", "Copy column names"))
                            .enabled(matches!(state.columns, Some(Ok(_))))
                            .show(ui, &p)
                            .clicked();
                        close = Btn::new(l("Zamknij", "Close")).show(ui, &p).clicked();
                    });
                });
            },
        );
        if copy && let Some(Ok(columns)) = &state.columns {
            let names = columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            self.copy_out(ctx, names);
        }
        if open {
            self.open_table(state.connection, state.table);
            return;
        }
        if !close && !dialog.close {
            self.structure = Some(state);
        }
    }

    fn drop_window(&mut self, ctx: &egui::Context) {
        let Some((connection, tables)) = self.drop_confirm.take() else {
            return;
        };
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let (mut remove, mut cancel) = (false, false);
        let title = if tables.len() == 1 {
            l("Usunąć z bazy?", "Drop from the database?")
        } else if en {
            "Drop these objects from the database?"
        } else {
            "Usunąć te obiekty z bazy?"
        };
        let dialog = widgets::dialog(
            ctx,
            &p,
            "drop_objects",
            title,
            460.0,
            l("Zamknij", "Close"),
            |ui| {
                widgets::dialog_body(ui, |ui| {
                    for table in tables.iter().take(8) {
                        widgets::subheading(ui, &p, &table.name);
                    }
                    if tables.len() > 8 {
                        let more = tables.len() - 8;
                        widgets::help(
                            ui,
                            &p,
                            &if en {
                                format!("and {more} more")
                            } else {
                                format!("i jeszcze {more}")
                            },
                        );
                    }
                    widgets::help(
                        ui,
                        &p,
                        l(
                            "Tabele zostaną usunięte razem z danymi (DROP TABLE), widoki poleceniem DROP VIEW. Tej operacji nie można cofnąć.",
                            "Tables are dropped together with their data (DROP TABLE), views with DROP VIEW. This cannot be undone.",
                        ),
                    );
                });
                widgets::dialog_footer(ui, &p, |ui| {
                    widgets::right(ui, |ui| {
                        remove = Btn::danger(l("Usuń z bazy", "Drop")).show(ui, &p).clicked();
                        cancel = Btn::new(l("Anuluj", "Cancel")).show(ui, &p).clicked();
                    });
                });
            },
        );
        if remove {
            self.drop_objects(connection, tables);
        } else if !cancel && !dialog.close {
            self.drop_confirm = Some((connection, tables));
        }
    }

    /// Drop tables and views one after another; the first refusal stops the
    /// rest. Every statement is written to the history before it runs.
    fn drop_objects(&mut self, connection: Uuid, tables: Vec<TableRef>) {
        let Some(db) = self.databases.get(&connection).cloned() else {
            self.connect(connection);
            return;
        };
        let statements = match tables
            .iter()
            .map(|t| drop_statement(db.dialect(), t))
            .collect::<anyhow::Result<Vec<_>>>()
        {
            Ok(statements) => statements,
            Err(error) => {
                self.notify(error.to_string(), true);
                return;
            }
        };
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let vault = self.vault.clone();
        self.track(async move {
            let mut dropped = 0usize;
            let mut outcome = Ok(());
            for statement in statements {
                let run = async {
                    db.query(&statement, CancellationToken::new())
                        .await
                        .map(|_| ())
                };
                match recorded(vault.clone(), connection, &statement, run).await {
                    Ok(()) => dropped += 1,
                    Err(error) => {
                        outcome = Err(error);
                        break;
                    }
                }
            }
            let _ = tx
                .send(Message::Dropped(connection, outcome.map(|()| dropped)))
                .await;
            ctx.request_repaint();
        });
    }
}

/// The kind of an object as the list names it.
fn kind_label(table: &TableRef, browse: BrowseModel, en: bool) -> String {
    let l = |pl: &str, english: &str| if en { english } else { pl }.to_owned();
    match ObjectKind::from_metadata(&table.kind) {
        ObjectKind::Table => match browse {
            BrowseModel::Tables => l("Tabela", "Table"),
            BrowseModel::Documents => l("Kolekcja", "Collection"),
            BrowseModel::Keys => l("Klucze", "Keys"),
        },
        ObjectKind::View => l("Widok", "View"),
        ObjectKind::MaterializedView => l("Widok materializowany", "Materialized view"),
        ObjectKind::Other => table.kind.clone(),
    }
}

/// The statement that drops a table, a view or a materialized view.
fn drop_statement(dialect: &Dialect, table: &TableRef) -> anyhow::Result<String> {
    let what = match ObjectKind::from_metadata(&table.kind) {
        ObjectKind::Table => "TABLE",
        ObjectKind::View => "VIEW",
        ObjectKind::MaterializedView => "MATERIALIZED VIEW",
        ObjectKind::Other => anyhow::bail!("operation_unsupported"),
    };
    Ok(format!("DROP {what} {}", sql::qualified(dialect, table)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(name: &str, kind: &str) -> TableRef {
        TableRef {
            schema: "public".into(),
            name: name.into(),
            kind: kind.into(),
        }
    }

    #[test]
    fn tables_and_views_are_dropped_by_their_quoted_names() {
        let dialect = Dialect::ANSI;
        assert_eq!(
            drop_statement(&dialect, &object("order \"items\"", "BASE TABLE")).unwrap(),
            "DROP TABLE \"public\".\"order \"\"items\"\"\""
        );
        assert_eq!(
            drop_statement(&dialect, &object("active", "view")).unwrap(),
            "DROP VIEW \"public\".\"active\""
        );
        assert_eq!(
            drop_statement(&dialect, &object("totals", "materialized_view")).unwrap(),
            "DROP MATERIALIZED VIEW \"public\".\"totals\""
        );
        assert!(drop_statement(&dialect, &object("seq", "sequence")).is_err());
    }

    #[test]
    fn kinds_are_named_as_the_engine_stores_records() {
        let table = object("t", "table");
        assert_eq!(kind_label(&table, BrowseModel::Tables, false), "Tabela");
        assert_eq!(
            kind_label(&table, BrowseModel::Documents, true),
            "Collection"
        );
        assert_eq!(
            kind_label(&object("v", "VIEW"), BrowseModel::Tables, true),
            "View"
        );
        assert_eq!(
            kind_label(&object("s", "sequence"), BrowseModel::Tables, true),
            "sequence"
        );
    }
}

/// The name of a fact of the information panel.
fn detail_label(key: &str, en: bool) -> String {
    let (pl, english) = match key {
        "engine" => ("Silnik", "Engine"),
        "auto_increment" => ("Auto Increment", "Auto Increment"),
        "row_format" => ("Format wiersza", "Row format"),
        "row_estimate" => ("Wiersze (szacunkowo)", "Rows (estimate)"),
        "created" => ("Czas utworzenia", "Created"),
        "updated" => ("Czas aktualizacji", "Updated"),
        "checked" => ("Sprawdzano", "Checked"),
        "index_size" => ("Rozmiar indeksu", "Index size"),
        "data_size" => ("Rozmiar danych", "Data size"),
        "max_data_size" => ("Maks. rozmiar danych", "Max data size"),
        "free_size" => ("Wolnych", "Free"),
        "total_size" => ("Rozmiar całkowity", "Total size"),
        "collation" => ("Kolacja", "Collation"),
        "create_options" => ("Opcje tworzenia", "Create options"),
        "comment" => ("Komentarz", "Comment"),
        "owner" => ("Właściciel", "Owner"),
        "tablespace" => ("Przestrzeń tabel", "Tablespace"),
        "vacuumed" => ("Ostatni VACUUM", "Last VACUUM"),
        "analyzed" => ("Ostatni ANALYZE", "Last ANALYZE"),
        "definition" => ("Definicja", "Definition"),
        "kind" => ("Rodzaj", "Kind"),
        other => return other.to_owned(),
    };
    if en { english } else { pl }.to_owned()
}

/// A fact as the panel shows it: sizes as "3,17 MB (3 325 952)", nothing
/// as "--".
fn detail_value(key: &str, cell: &Cell, en: bool) -> String {
    let text = cell.display();
    if matches!(cell, Cell::Null) || text.trim().is_empty() {
        return "--".into();
    }
    if key.ends_with("_size")
        && let Ok(bytes) = text.trim().parse::<u64>()
    {
        let units = ["bytes", "KB", "MB", "GB", "TB"];
        let mut value = bytes as f64;
        let mut unit = 0;
        while value >= 1024.0 && unit < units.len() - 1 {
            value /= 1024.0;
            unit += 1;
        }
        let shown = if unit == 0 {
            format!("{bytes} bytes")
        } else {
            let number = format!("{value:.2}");
            let number = if en { number } else { number.replace('.', ",") };
            format!("{number} {}", units[unit])
        };
        return if unit == 0 {
            shown
        } else {
            format!("{shown} ({})", super::results::group_digits(bytes, en))
        };
    }
    text
}

#[cfg(test)]
mod detail_tests {
    use super::*;

    #[test]
    fn sizes_and_missing_facts_are_written_for_people() {
        assert_eq!(
            detail_value("data_size", &Cell::Integer("3325952".into()), false),
            "3,17 MB (3\u{a0}325\u{a0}952)"
        );
        assert_eq!(
            detail_value("index_size", &Cell::Integer("0".into()), true),
            "0 bytes"
        );
        assert_eq!(
            detail_value("comment", &Cell::Text(String::new()), true),
            "--"
        );
        assert_eq!(
            detail_value("engine", &Cell::Text("InnoDB".into()), true),
            "InnoDB"
        );
        assert_eq!(detail_label("engine", false), "Silnik");
        assert_eq!(detail_label("unknown_fact", true), "unknown_fact");
    }
}
