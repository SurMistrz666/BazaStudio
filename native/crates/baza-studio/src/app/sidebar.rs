//! The "POŁĄCZENIA" side panel: groups, connections, their schemas, tables and
//! views, and the saved queries.
use super::*;
use crate::widgets::{Btn, TreeRow};
use eframe::egui::RichText;

/// The click on a tree row was on its arrow: the first icon-wide strip.
fn on_disclosure(response: &egui::Response, p: &theme::Palette) -> bool {
    response
        .interact_pointer_pos()
        .is_some_and(|pos| pos.x < response.rect.left() + p.icon() + 9.0)
}

fn tint(color: [u8; 3], neutral: [u8; 3]) -> Option<Color32> {
    (color != neutral).then(|| Color32::from_rgb(color[0], color[1], color[2]))
}

impl StudioApp {
    pub(super) fn sidebar(&mut self, ui: &mut egui::Ui) {
        self.validate_navigation_selection();
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        ui.spacing_mut().item_spacing.y = 0.0;
        // The note at the bottom takes the height its text needs; the tree
        // gets the rest.
        egui::Panel::bottom("sidebar_note")
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::NONE.inner_margin(egui::Margin {
                left: 19,
                right: 14,
                top: 8,
                bottom: 12,
            }))
            .show(ui, |ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(l(
                            "Hasła i klucze są zaszyfrowane w sejfie tego komputera.",
                            "Passwords and keys are encrypted in this computer's vault.",
                        ))
                        .size(p.small())
                        .color(p.muted),
                    )
                    .wrap(),
                );
            });
        egui::Frame::NONE
            .inner_margin(egui::Margin {
                left: 19,
                right: 8,
                top: 12,
                bottom: 8,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.set_min_height(26.0);
                    widgets::caption(ui, &p, l("Połączenia", "Connections"));
                    widgets::right(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 1.0;
                        if widgets::icon_button(
                            ui,
                            &p,
                            Icon::Refresh,
                            l("Odśwież grupy i obiekty", "Refresh groups and objects"),
                        )
                        .clicked()
                        {
                            self.load_groups();
                            self.refresh_tables(self.focus_connection());
                        }
                        if widgets::icon_button(ui, &p, Icon::Folder, l("Nowa grupa", "New group"))
                            .clicked()
                        {
                            self.new_group(None);
                        }
                        if widgets::icon_button(
                            ui,
                            &p,
                            Icon::Plus,
                            l("Nowe połączenie", "New connection"),
                        )
                        .clicked()
                        {
                            self.new_connection();
                        }
                    });
                });
            });
        egui::Frame::NONE
            .inner_margin(egui::Margin {
                left: 11,
                right: 11,
                top: 0,
                bottom: 8,
            })
            .show(ui, |ui| {
                let response = ui.add(
                    widgets::field(&p, &mut self.nav_search)
                        .id(egui::Id::new("nav_search"))
                        .desired_width(f32::INFINITY)
                        .margin(egui::Margin {
                            left: 28,
                            right: 8,
                            top: 5,
                            bottom: 5,
                        })
                        .hint_text(l(
                            "Szukaj połączeń i tabel",
                            "Search connections and tables",
                        )),
                );
                widgets::paint(
                    ui.painter(),
                    egui::Rect::from_center_size(
                        egui::pos2(response.rect.left() + 15.0, response.rect.center().y),
                        egui::vec2(13.0, 13.0),
                    ),
                    Icon::Search,
                    p.muted,
                );
            });
        let search = self.nav_search.trim().to_lowercase();
        let profiles = SecretProfiles(self.profiles.clone());
        egui::ScrollArea::vertical()
            .id_salt("navigator_tree")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(11, 2))
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        if profiles.0.connections.is_empty() {
                            self.sidebar_empty(ui);
                            return;
                        }
                        let top = ui.cursor().top();
                        let groups = self
                            .group_snapshot
                            .groups
                            .iter()
                            .filter(|g| g.parent.is_none())
                            .map(|g| g.id)
                            .collect::<Vec<_>>();
                        for group in groups {
                            self.group_tree(ui, group, &profiles.0.connections, &search, 0);
                        }
                        for connection in &profiles.0.connections {
                            let grouped = self
                                .group_snapshot
                                .memberships
                                .get(&connection.id)
                                .is_some_and(|id| {
                                    self.group_snapshot
                                        .groups
                                        .iter()
                                        .any(|group| group.id == *id)
                                });
                            if !grouped {
                                self.connection_tree(ui, connection, &search);
                            }
                        }
                        self.loose_queries(ui, &search);
                        if !search.is_empty() && ui.cursor().top() == top {
                            // The search left nothing to show.
                            egui::Frame::NONE
                                .inner_margin(egui::Margin::symmetric(8, 8))
                                .show(ui, |ui| {
                                    widgets::help(
                                        ui,
                                        &p,
                                        l(
                                            "Żadne połączenie, tabela ani zapytanie nie pasuje do wyszukiwania.",
                                            "No connection, table or query matches the search.",
                                        ),
                                    );
                                });
                        }
                    });
            });
    }

    fn sidebar_empty(&mut self, ui: &mut egui::Ui) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(8, 10))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 9.0;
                ui.add(
                    egui::Label::new(
                        RichText::new(l("Jeszcze bez połączeń", "No connections yet"))
                            .font(theme::medium(p.body()))
                            .color(p.muted),
                    )
                    .selectable(false),
                );
                widgets::help(
                    ui,
                    &p,
                    l(
                        "Dodaj bazę PostgreSQL, MariaDB / MySQL albo plik SQLite.",
                        "Add a PostgreSQL or MariaDB / MySQL database, or an SQLite file.",
                    ),
                );
                if Btn::new(l("Nowe połączenie", "New connection"))
                    .small()
                    .show(ui, &p)
                    .clicked()
                {
                    self.new_connection();
                }
            });
    }

    /// Saved queries that belong to no existing connection.
    fn loose_queries(&mut self, ui: &mut egui::Ui, search: &str) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let queries = self
            .library
            .iter()
            .filter(|q| {
                q.connection.is_none_or(|id| {
                    !self
                        .profiles
                        .connections
                        .iter()
                        .any(|profile| profile.id == id)
                })
            })
            .filter(|q| search.is_empty() || navigator::matches(&q.name, search))
            .map(|q| (q.id, q.name.clone(), q.connection))
            .collect::<Vec<_>>();
        if queries.is_empty() {
            return;
        }
        let searching = !search.is_empty();
        let id = egui::Id::new(("loose_queries", searching));
        let mut state =
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true);
        if searching {
            state.set_open(true);
        }
        let response = widgets::tree_row(
            ui,
            &p,
            TreeRow {
                id: id.with("row"),
                label: l("Zapytania bez połączenia", "Queries without a connection"),
                icon: Icon::Bookmark,
                count: Some(queries.len()),
                open: Some(state.is_open()),
                selected: false,
                tint: None,
            },
        );
        widgets::update_tree_open(ui, &response, &mut state);
        state.show_body_indented(&response, ui, |ui| {
            for (id, name, connection) in queries {
                let node = NodeIdentity::Query(connection, id);
                let r = widgets::tree_row(
                    ui,
                    &p,
                    TreeRow {
                        id: egui::Id::new(("library_query", id)),
                        label: &name,
                        icon: Icon::Code,
                        count: None,
                        open: None,
                        selected: self.navigation_selection.as_ref() == Some(&node),
                        tint: None,
                    },
                );
                if r.clicked() || widgets::keyboard_activate(&r) {
                    self.navigation_selection = Some(node);
                    self.open_saved_query(id);
                }
                r.context_menu(|ui| {
                    if ui.button(l("Otwórz zapytanie", "Open query")).clicked() {
                        self.open_saved_query(id);
                        ui.close();
                    }
                    if ui.button(l("Usuń zapytanie", "Delete query")).clicked() {
                        self.update_library(None, Some(id));
                        ui.close();
                    }
                });
            }
        });
    }

    fn connection_tree(&mut self, ui: &mut egui::Ui, connection: &ConnectionProfile, search: &str) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let connected = self.databases.contains_key(&connection.id);
        let tables = self.tables.get(&connection.id).cloned().unwrap_or_default();
        let tree = navigator::ConnectionTree::build(
            connection.id,
            &tables,
            &self.navigator_queries(),
            navigator::connection_matches(connection, search),
            search,
        );
        if !tree.visible {
            return;
        }
        let searching = !self.nav_search.trim().is_empty();
        let node = NodeIdentity::Connection(connection.id);
        // In "Połączenia" the row "Profil połączenia" below reads as selected
        // instead, as in the portal demo.
        let in_focus = self.navigation_selection.as_ref() == Some(&node)
            || (self.navigation_selection.is_none()
                && self.section == Section::Monitor
                && self.tabs[self.active].connection == Some(connection.id));
        let profile_shown =
            self.section == Section::Connections && self.focus_connection() == Some(connection.id);
        let selected = in_focus && !profile_shown;
        let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            egui::Id::new(("connection_body", connection.id, connected, searching)),
            connected,
        );
        if searching {
            state.set_open(true);
        }
        let subtitle = connection_subtitle(connection, en);
        let row = widgets::connection_row(
            ui,
            &p,
            connection.id,
            &connection.name,
            &subtitle,
            connection.product_spec().badge,
            connected,
            selected,
            state.is_open(),
            tint(connection.color, DEFAULT_CONNECTION_COLOR),
            l("Menu połączenia", "Connection menu"),
        );
        if row.row.clicked() || widgets::keyboard_activate(&row.row) {
            row.row.request_focus();
            self.navigation_selection = Some(node);
        }
        if row.disclosure.clicked()
            || (row.row.clicked() && !row.more.clicked())
            || widgets::keyboard_activate(&row.disclosure)
            || widgets::keyboard_activate(&row.row)
        {
            state.toggle(ui);
        }
        if row.row.has_focus() {
            if ui.input(|input| input.key_pressed(egui::Key::ArrowRight)) {
                state.set_open(true);
            }
            if ui.input(|input| input.key_pressed(egui::Key::ArrowLeft)) {
                state.set_open(false);
            }
        }
        if row.row.double_clicked() && !row.more.clicked() {
            self.connect(connection.id);
        }
        row.row
            .clone()
            .on_hover_text(format!("{}\n{}", connection.name, subtitle));
        row.row
            .context_menu(|ui| self.connection_context(ui, connection, connected));
        egui::Popup::menu(&row.more).show(|ui| self.connection_context(ui, connection, connected));
        state.show_body_indented(&row.row, ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            if connected {
                let caps = self.capabilities(Some(connection.id));
                if tree.schemas.is_empty() && !searching {
                    let text = match caps.browse {
                        BrowseModel::Tables => l("Brak tabel i widoków.", "No tables or views."),
                        BrowseModel::Documents => l("Brak kolekcji.", "No collections."),
                        BrowseModel::Keys => l("Brak kluczy.", "No keys."),
                    };
                    ui.label(RichText::new(text).size(p.small()).color(p.muted));
                }
                let only = tree.schemas.len() == 1;
                for schema in tree.schemas {
                    if caps.schemas {
                        self.schema_tree(ui, connection.id, schema, searching, only);
                    } else {
                        // An engine without schemas: its containers hang
                        // directly under the connection. Only tables have
                        // views next to them.
                        for folder in schema.folders {
                            if folder.objects.is_empty()
                                && (folder.kind != ObjectKind::Table
                                    || caps.browse != BrowseModel::Tables)
                                && folder.kind != ObjectKind::Table
                            {
                                continue;
                            }
                            self.object_folder(ui, connection.id, &schema.name, folder, searching);
                        }
                    }
                }
            } else {
                ui.horizontal(|ui| {
                    ui.add_space(p.icon() + 9.0);
                    let connecting = self.connect_requests.contains_key(&connection.id);
                    if connecting {
                        ui.spinner();
                        ui.label(
                            RichText::new(l("Łączenie…", "Connecting…"))
                                .size(p.small())
                                .color(p.muted),
                        );
                    } else if Btn::link(l("Połącz", "Connect")).show(ui, &p).clicked() {
                        self.connect(connection.id);
                    }
                });
            }
            if !searching || !tree.queries.is_empty() {
                self.queries_tree(ui, connection.id, tree.queries, searching);
            }
            if !searching {
                // As in the portal demo: one row that opens the profile.
                let r = widgets::tree_row(
                    ui,
                    &p,
                    TreeRow {
                        id: egui::Id::new(("connection_profile", connection.id)),
                        label: l("Profil połączenia", "Connection profile"),
                        icon: Icon::Shield,
                        count: None,
                        open: None,
                        selected: profile_shown,
                        tint: None,
                    },
                );
                if r.clicked() || widgets::keyboard_activate(&r) {
                    self.navigation_selection = Some(NodeIdentity::Connection(connection.id));
                    self.section = Section::Connections;
                }
            }
        });
    }

    fn schema_tree(
        &mut self,
        ui: &mut egui::Ui,
        connection: Uuid,
        schema: navigator::SchemaNode,
        searching: bool,
        only: bool,
    ) {
        let p = self.palette();
        let node = NodeIdentity::Schema(connection, schema.name.clone());
        let id = egui::Id::new(("database_schema", connection, &schema.name, searching));
        // A single database opens to show its folders; several stay closed.
        let mut state =
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, only);
        if searching {
            state.set_open(true);
        }
        let count = schema
            .folders
            .iter()
            .map(|folder| folder.objects.len())
            .sum();
        let response = widgets::tree_row(
            ui,
            &p,
            TreeRow {
                id: id.with("row"),
                label: &schema.name,
                icon: Icon::Schema,
                count: Some(count),
                open: Some(state.is_open()),
                selected: self.navigation_selection.as_ref() == Some(&node),
                tint: None,
            },
        );
        if response.clicked() || widgets::keyboard_activate(&response) {
            self.navigation_selection = Some(node);
        }
        // A click on the name lists the schema in "Obiekty" and opens it; the
        // arrow only opens or closes it.
        if response.clicked() && !on_disclosure(&response, &p) {
            self.show_objects(connection, &schema.name, None);
            state.set_open(true);
        } else {
            widgets::update_tree_open(ui, &response, &mut state);
        }
        state.show_body_indented(&response, ui, |ui| {
            for folder in schema.folders {
                self.object_folder(ui, connection, &schema.name, folder, searching);
            }
        });
    }

    fn object_folder(
        &mut self,
        ui: &mut egui::Ui,
        connection: Uuid,
        schema: &str,
        folder: navigator::ObjectFolder,
        searching: bool,
    ) {
        // An empty folder of a kind the engine may not even have (views of a key-value
        // store, for example) only adds noise; containers are always shown.
        if folder.objects.is_empty() && folder.kind != ObjectKind::Table {
            return;
        }
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        // What the engine keeps its records in: tables, collections or keys.
        let containers = localized(self.capabilities(Some(connection)).browse.containers(), en);
        let (label, icon) = match folder.kind {
            ObjectKind::Table => (containers, Icon::Table),
            ObjectKind::View => (l("Widoki", "Views"), Icon::View),
            ObjectKind::MaterializedView => (
                l("Widoki materializowane", "Materialized views"),
                Icon::View,
            ),
            ObjectKind::Other => (l("Inne obiekty", "Other objects"), Icon::File),
        };
        let node = NodeIdentity::Objects(connection, schema.into(), folder.kind);
        let id = egui::Id::new((
            "database_objects",
            connection,
            schema,
            folder.kind,
            searching,
        ));
        // After connecting, folders start closed: the tables are listed
        // only when asked for.
        let mut state =
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
        if searching {
            state.set_open(true);
        }
        let response = widgets::tree_row(
            ui,
            &p,
            TreeRow {
                id: id.with("row"),
                label,
                icon,
                count: Some(folder.objects.len()),
                open: Some(state.is_open()),
                selected: self.navigation_selection.as_ref() == Some(&node),
                tint: None,
            },
        );
        if response.clicked() || widgets::keyboard_activate(&response) {
            self.navigation_selection = Some(node);
        }
        // A click on the name lists the folder in "Obiekty",
        // and opens it; the arrow only opens or closes it.
        if response.clicked() && !on_disclosure(&response, &p) {
            self.show_objects(connection, schema, Some(folder.kind));
            state.set_open(true);
        } else {
            widgets::update_tree_open(ui, &response, &mut state);
        }
        state.show_body_indented(&response, ui, |ui| {
            for table in folder.objects {
                let node = NodeIdentity::Object(
                    connection,
                    table.schema.clone(),
                    table.name.clone(),
                    folder.kind,
                );
                // The table on screen in "Dane tabeli", or one selected in
                // "Obiekty", reads as selected.
                let listed = self.section == Section::Objects
                    && self
                        .objects
                        .target
                        .as_ref()
                        .is_some_and(|t| t.connection == connection)
                    && self
                        .objects
                        .selected
                        .contains(&(table.schema.clone(), table.name.clone()));
                let shown = listed
                    || self.section == Section::Table && {
                        let tab = &self.tabs[self.active];
                        tab.kind == TabKind::Table
                            && tab.connection == Some(connection)
                            && tab.source.as_ref().is_some_and(|open| {
                                open.schema == table.schema && open.name == table.name
                            })
                    };
                let r = widgets::tree_row(
                    ui,
                    &p,
                    TreeRow {
                        id: egui::Id::new(("database_object", &node)),
                        label: &table.name,
                        icon,
                        count: None,
                        open: None,
                        selected: shown || self.navigation_selection.as_ref() == Some(&node),
                        tint: None,
                    },
                );
                if r.clicked() {
                    self.navigation_selection = Some(node.clone());
                }
                if r.double_clicked() || widgets::keyboard_activate(&r) {
                    self.navigation_selection = Some(node);
                    self.open_table(connection, table.clone());
                }
                r.context_menu(|ui| {
                    if ui.button(l("Otwórz dane", "Open data")).clicked() {
                        self.open_table(connection, table.clone());
                        ui.close();
                    }
                    if ui
                        .add_enabled(
                            self.capabilities(Some(connection)).query_builder,
                            egui::Button::new(l("Kreator zapytań", "Query builder")),
                        )
                        .clicked()
                    {
                        self.open_builder(connection, table.clone());
                        ui.close();
                    }
                });
            }
        });
    }

    fn queries_tree(
        &mut self,
        ui: &mut egui::Ui,
        connection: Uuid,
        queries: Vec<QueryNode>,
        searching: bool,
    ) {
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let node = NodeIdentity::Queries(connection);
        let id = egui::Id::new(("connection_queries", connection, searching));
        let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            id,
            !queries.is_empty(),
        );
        if searching {
            state.set_open(true);
        }
        let r = widgets::tree_row(
            ui,
            &p,
            TreeRow {
                id: id.with("row"),
                label: l("Zapisane zapytania", "Saved queries"),
                icon: Icon::Bookmark,
                count: Some(queries.len()),
                open: Some(state.is_open()),
                selected: self.navigation_selection.as_ref() == Some(&node),
                tint: None,
            },
        );
        if r.clicked() || widgets::keyboard_activate(&r) {
            self.navigation_selection = Some(node);
        }
        widgets::update_tree_open(ui, &r, &mut state);
        r.context_menu(|ui| {
            if ui.button(l("Nowe zapytanie", "New query")).clicked() {
                self.new_tab(Some(connection));
                ui.close();
            }
        });
        state.show_body_indented(&r, ui, |ui| {
            if queries.is_empty() {
                ui.horizontal(|ui| {
                    ui.add_space(p.icon() + 9.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(l(
                                "Zapisz zapytanie skrótem Ctrl+S.",
                                "Save a query with Ctrl+S.",
                            ))
                            .size(p.small())
                            .color(p.muted),
                        )
                        .wrap(),
                    );
                });
            }
            for query in queries {
                let node = NodeIdentity::Query(Some(connection), query.id);
                let r = widgets::tree_row(
                    ui,
                    &p,
                    TreeRow {
                        id: egui::Id::new(("saved_connection_query", query.id)),
                        label: &query.name,
                        icon: Icon::Code,
                        count: None,
                        open: None,
                        selected: self.navigation_selection.as_ref() == Some(&node),
                        tint: None,
                    },
                );
                if r.clicked() || widgets::keyboard_activate(&r) {
                    self.navigation_selection = Some(node);
                    self.open_saved_query(query.id);
                }
                r.context_menu(|ui| {
                    if ui.button(l("Otwórz zapytanie", "Open query")).clicked() {
                        self.open_saved_query(query.id);
                        ui.close();
                    }
                    if ui.button(l("Usuń zapytanie", "Delete query")).clicked() {
                        self.update_library(None, Some(query.id));
                        ui.close();
                    }
                });
            }
        });
    }

    fn connection_context(
        &mut self,
        ui: &mut egui::Ui,
        connection: &ConnectionProfile,
        connected: bool,
    ) {
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        if !connected && ui.button(l("Połącz", "Connect")).clicked() {
            self.connect(connection.id);
            ui.close();
        }
        if connected && ui.button(l("Rozłącz", "Disconnect")).clicked() {
            self.disconnect(connection.id);
            ui.close();
        }
        let caps = self.capabilities(Some(connection.id));
        if ui
            .add_enabled(
                caps.language != QueryLanguage::None,
                egui::Button::new(l("Nowe zapytanie", "New query")),
            )
            .clicked()
        {
            self.new_tab(Some(connection.id));
            ui.close();
        }
        if ui
            .button(l("Profil połączenia", "Connection profile"))
            .clicked()
        {
            self.navigation_selection = Some(NodeIdentity::Connection(connection.id));
            self.section = Section::Connections;
            ui.close();
        }
        if ui
            .button(l("Edytuj połączenie", "Edit connection"))
            .clicked()
        {
            self.edit_connection(connection.id);
            ui.close();
        }
        if connected {
            if ui
                .add_enabled(
                    caps.table_designer,
                    egui::Button::new(l("Nowa tabela", "New table")),
                )
                .clicked()
            {
                self.open_designer(connection.id);
                ui.close();
            }
            if ui.button(self.refresh_label(connection.id)).clicked() {
                self.refresh_tables(Some(connection.id));
                ui.close();
            }
        }
        ui.menu_button(l("Przenieś do grupy", "Move to group"), |ui| {
            if ui.button(l("Bez grupy", "Ungrouped")).clicked() {
                self.group_operation(GroupOperation::Membership(connection.id, None));
                ui.close();
            }
            for group in self.group_snapshot.groups.clone() {
                if ui.button(self.group_path(group.id)).clicked() {
                    self.group_operation(GroupOperation::Membership(connection.id, Some(group.id)));
                    ui.close();
                }
            }
        });
        ui.separator();
        if ui
            .button(l("Usuń połączenie", "Delete connection"))
            .clicked()
        {
            self.connection_delete = Some(connection.id);
            ui.close();
        }
    }

    fn group_tree(
        &mut self,
        ui: &mut egui::Ui,
        id: Uuid,
        profiles: &[ConnectionProfile],
        search: &str,
        depth: usize,
    ) {
        if depth >= 8 || !self.group_matches(id, profiles, search, depth) {
            return;
        }
        let Some(g) = self
            .group_snapshot
            .groups
            .iter()
            .find(|group| group.id == id)
            .cloned()
        else {
            return;
        };
        let p = self.palette();
        let searching = !self.nav_search.trim().is_empty();
        let child_search = if search.is_empty() || navigator::matches(&g.name, search) {
            ""
        } else {
            search
        };
        let children = self
            .group_snapshot
            .groups
            .iter()
            .filter(|child| child.parent == Some(id))
            .map(|child| child.id)
            .collect::<Vec<_>>();
        let node = NodeIdentity::Group(id);
        let state_id = egui::Id::new(("connection_group", id, searching));
        let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
            ui.ctx(),
            state_id,
            true,
        );
        if searching {
            state.set_open(true);
        }
        let alive = profiles.iter().map(|profile| profile.id).collect();
        let count = navigator::descendant_connection_count(&self.group_snapshot, id, &alive);
        let response = widgets::tree_row(
            ui,
            &p,
            TreeRow {
                id: state_id.with("row"),
                label: &g.name,
                icon: Icon::Folder,
                count: Some(count),
                open: Some(state.is_open()),
                selected: self.navigation_selection.as_ref() == Some(&node),
                tint: tint(g.color, DEFAULT_GROUP_COLOR),
            },
        );
        if response.clicked() || widgets::keyboard_activate(&response) {
            self.navigation_selection = Some(node);
        }
        widgets::update_tree_open(ui, &response, &mut state);
        state.show_body_indented(&response, ui, |ui| {
            for child in children {
                self.group_tree(ui, child, profiles, child_search, depth + 1);
            }
            for connection in profiles {
                if self.group_snapshot.memberships.get(&connection.id) == Some(&id) {
                    self.connection_tree(ui, connection, child_search);
                }
            }
        });
        response.context_menu(|ui| self.group_context(ui, &g));
    }

    /// Actions of one connection group, shared by the tree and the group list
    /// of the "Połączenia" section.
    pub(super) fn group_context(&mut self, ui: &mut egui::Ui, g: &baza_core::groups::Group) {
        let id = g.id;
        if ui
            .button(self.tr("Nowa podgrupa", "New subgroup"))
            .clicked()
        {
            self.group_dialog = Some(GroupEditor {
                id: None,
                name: String::new(),
                color: g.color,
                parent: Some(id),
            });
            ui.close();
        }
        if ui
            .button(self.tr("Zmień nazwę i wygląd", "Rename and customize"))
            .clicked()
        {
            self.edit_group(g);
            ui.close();
        }
        let label = self.tr("Przenieś grupę", "Move group");
        ui.menu_button(label, |ui| {
            if ui.button(self.tr("Poziom główny", "Root level")).clicked() {
                self.group_operation(GroupOperation::Move(id, None));
                ui.close();
            }
            for target in self.group_snapshot.groups.clone() {
                if target.id != id && ui.button(self.group_path(target.id)).clicked() {
                    self.group_operation(GroupOperation::Move(id, Some(target.id)));
                    ui.close();
                }
            }
        });
        ui.separator();
        if ui.button(self.tr("Usuń grupę…", "Delete group…")).clicked() {
            self.group_remove = Some(id);
            ui.close();
        }
    }

    pub(super) fn edit_group(&mut self, g: &baza_core::groups::Group) {
        self.group_dialog = Some(GroupEditor {
            id: Some(g.id),
            name: g.name.clone(),
            color: g.color,
            parent: g.parent,
        });
    }
}

/// Engine and address of a connection, as shown under its name.
pub(super) fn connection_subtitle(connection: &ConnectionProfile, en: bool) -> String {
    let endpoint = if connection.is_file() {
        connection
            .sqlite_path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| if en { "Local file" } else { "Plik lokalny" }.into())
    } else if connection.database.is_empty() {
        format!("{}:{}", connection.host, connection.port)
    } else {
        format!(
            "{} · {}:{}",
            connection.database, connection.host, connection.port
        )
    };
    format!("{} · {}", connection.product_label(), endpoint)
}
