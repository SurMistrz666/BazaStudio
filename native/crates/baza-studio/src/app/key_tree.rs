//! "Drzewo kluczy": the keys of a page of a key-value store as a tree of
//! their parts, `klucz:data:cos` under `klucz` and `data`, each category
//! folded and unfolded on a click.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

/// What separates the parts of a key.
pub(super) const DELIMITER: char = ':';
/// Most levels of categories: the rest of a longer key stays in its name.
/// A key is the user's data, and one of thousands of parts would nest the
/// tree as deep.
const MAX_LEVELS: usize = 32;

/// The categories of a key and its own name: at most [`MAX_LEVELS`] parts,
/// then the rest.
fn key_parts(key: &str) -> Vec<&str> {
    key.splitn(MAX_LEVELS + 1, DELIMITER).collect()
}

/// One line of the tree on screen.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct TreeLine {
    pub depth: usize,
    pub label: String,
    pub kind: TreeKind,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum TreeKind {
    /// A category: the prefix that names it, with the delimiter, the keys
    /// under it and whether it is unfolded.
    Category {
        prefix: String,
        keys: usize,
        open: bool,
    },
    /// A key, by its row in the result.
    Key { row: usize },
}

#[derive(Default)]
struct Node {
    children: BTreeMap<String, Node>,
    /// Keys that end here: their rows.
    rows: Vec<usize>,
    keys: usize,
}

/// The lines of the tree of `keys` (row and key), without the lines under a
/// folded category. Categories come before the keys of their level.
pub(super) fn tree_lines<'a>(
    keys: impl Iterator<Item = (usize, &'a str)>,
    folded: &BTreeSet<String>,
) -> Vec<TreeLine> {
    let mut root = Node::default();
    for (row, key) in keys {
        let mut parts = key_parts(key);
        // The last part is the key itself; the ones before it are categories.
        parts.pop();
        let mut node = &mut root;
        node.keys += 1;
        for part in parts {
            node = node.children.entry(part.to_owned()).or_default();
            node.keys += 1;
        }
        node.rows.push(row);
    }
    let mut lines = Vec::new();
    walk(&root, "", 0, folded, &mut lines);
    lines
}

fn walk(
    node: &Node,
    prefix: &str,
    depth: usize,
    folded: &BTreeSet<String>,
    lines: &mut Vec<TreeLine>,
) {
    for (part, child) in &node.children {
        let path = format!("{prefix}{part}{DELIMITER}");
        let open = !folded.contains(&path);
        lines.push(TreeLine {
            depth,
            label: part.clone(),
            kind: TreeKind::Category {
                prefix: path.clone(),
                keys: child.keys,
                open,
            },
        });
        if open {
            walk(child, &path, depth + 1, folded, lines);
        }
    }
    for &row in &node.rows {
        lines.push(TreeLine {
            depth,
            label: String::new(),
            kind: TreeKind::Key { row },
        });
    }
}

impl StudioApp {
    /// The tree of the keys of the tab's page, beside its grid.
    pub(super) fn key_tree_panel(&mut self, ui: &mut egui::Ui, index: usize) {
        let p = self.palette();
        let en = self.en();
        let tab = &mut self.tabs[index];
        let Some(result) = tab.result.as_ref() else {
            return;
        };
        // The key is the first column of the key; a page without one has none.
        let key_column = tab
            .columns
            .iter()
            .find(|c| c.primary_key)
            .and_then(|key| result.columns.iter().position(|c| c.name == key.name))
            .unwrap_or(0);
        let keys: Vec<(usize, String)> = result
            .rows
            .iter()
            .enumerate()
            .filter_map(|(row, cells)| cells.get(key_column).map(|cell| (row, cell.display())))
            .collect();
        let lines = tree_lines(
            keys.iter().map(|(row, key)| (*row, key.as_str())),
            &tab.folded_keys,
        );
        let row_height = p.row_height();
        let mut toggle = None;
        let mut chosen = None;
        let mut filter = None;
        ui.horizontal(|ui| {
            widgets::caption(
                ui,
                &p,
                &if en {
                    format!("Key tree · {}", keys.len())
                } else {
                    format!("Drzewo kluczy · {}", keys.len())
                },
            );
            widgets::right(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                if widgets::icon_button(
                    ui,
                    &p,
                    Icon::ChevronRight,
                    if en { "Fold all" } else { "Zwiń wszystko" },
                )
                .clicked()
                {
                    // Every category of the page, also the ones not on screen.
                    let mut every = BTreeSet::new();
                    for (_, key) in &keys {
                        let parts = key_parts(key);
                        for end in 1..parts.len() {
                            every.insert(format!(
                                "{}{DELIMITER}",
                                parts[..end].join(&DELIMITER.to_string())
                            ));
                        }
                    }
                    tab.folded_keys = every;
                }
                if widgets::icon_button(
                    ui,
                    &p,
                    Icon::ChevronDown,
                    if en { "Unfold all" } else { "Rozwiń wszystko" },
                )
                .clicked()
                {
                    tab.folded_keys.clear();
                }
            });
        });
        let selected = tab.selected_row;
        let font = egui::FontId::monospace(p.code() - 1.0);
        egui::ScrollArea::both()
            .id_salt(("key_tree", tab.id))
            .auto_shrink([false, false])
            .show_rows(ui, row_height, lines.len(), |ui, range| {
                for line in &lines[range] {
                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width().max(200.0), row_height),
                        egui::Sense::click(),
                    );
                    let painter = ui.painter();
                    let x = rect.left() + 6.0 + line.depth as f32 * 14.0;
                    let center = rect.center().y;
                    match &line.kind {
                        TreeKind::Category { prefix, keys, open } => {
                            if response.hovered() {
                                painter.rect_filled(rect, 3, p.raised_hover);
                            }
                            widgets::paint(
                                painter,
                                egui::Rect::from_center_size(
                                    egui::pos2(x + 6.0, center),
                                    egui::vec2(11.0, 11.0),
                                ),
                                if *open {
                                    Icon::ChevronDown
                                } else {
                                    Icon::ChevronRight
                                },
                                p.muted,
                            );
                            painter.text(
                                egui::pos2(x + 16.0, center),
                                egui::Align2::LEFT_CENTER,
                                &line.label,
                                font.clone(),
                                p.keyword,
                            );
                            painter.text(
                                rect.right_center() - egui::vec2(8.0, 0.0),
                                egui::Align2::RIGHT_CENTER,
                                keys.to_string(),
                                egui::FontId::proportional(p.small()),
                                p.muted,
                            );
                            if response.double_clicked() {
                                filter = Some(prefix.clone());
                            } else if response.clicked() {
                                toggle = Some(prefix.clone());
                            }
                            response.on_hover_text(if en {
                                format!("{prefix}* · double-click filters the keys")
                            } else {
                                format!("{prefix}* · dwuklik filtruje klucze")
                            });
                        }
                        TreeKind::Key { row } => {
                            let Ok(found) = keys.binary_search_by_key(row, |(r, _)| *r) else {
                                continue;
                            };
                            let key = &keys[found].1;
                            let label = key_parts(key).pop().unwrap_or(key.as_str());
                            if selected == Some(*row) {
                                painter.rect_filled(rect, 3, p.accent.gamma_multiply(0.3));
                            } else if response.hovered() {
                                painter.rect_filled(rect, 3, p.raised_hover);
                            }
                            painter.text(
                                egui::pos2(x + 16.0, center),
                                egui::Align2::LEFT_CENTER,
                                label,
                                font.clone(),
                                p.text,
                            );
                            if response.clicked() {
                                chosen = Some(*row);
                            }
                            response.on_hover_text(key.as_str());
                        }
                    }
                }
            });
        if let Some(prefix) = toggle
            && !tab.folded_keys.remove(&prefix)
        {
            tab.folded_keys.insert(prefix);
        }
        if let Some(row) = chosen {
            tab.selected_row = Some(row);
            tab.selected_cell = Some((row, key_column));
            tab.range = Some(super::results::GridRange::cell(row, key_column));
            tab.scroll_to = Some(row);
        }
        if let Some(prefix) = filter {
            // The first click of the double click folded it.
            tab.folded_keys.remove(&prefix);
            // The filter of the page: keys that contain the prefix.
            tab.filter_sql = false;
            tab.filter_column = result.columns[key_column].name.clone();
            tab.filter_value = prefix;
            tab.page = 0;
            tab.total = None;
            self.browse(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_make_a_tree_of_their_parts() {
        let keys = [
            "klucz:data:cos",
            "klucz:data:inne",
            "klucz:x",
            "solo",
            "user:1",
        ];
        let lines = |folded: &BTreeSet<String>| {
            tree_lines(keys.iter().copied().enumerate(), folded)
                .into_iter()
                .map(|line| {
                    let what = match line.kind {
                        TreeKind::Category { keys, open, .. } => {
                            format!("{}{} ({keys})", if open { "-" } else { "+" }, line.label)
                        }
                        TreeKind::Key { row } => format!("#{row}"),
                    };
                    format!("{}{what}", "  ".repeat(line.depth))
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            lines(&BTreeSet::new()),
            [
                "-klucz (3)",
                "  -data (2)",
                "    #0",
                "    #1",
                "  #2",
                "-user (1)",
                "  #4",
                "#3"
            ]
        );
        // A folded category hides what is under it, at any depth.
        let folded = BTreeSet::from(["klucz:data:".to_owned(), "user:".to_owned()]);
        assert_eq!(
            lines(&folded),
            ["-klucz (3)", "  +data (2)", "  #2", "+user (1)", "#3"]
        );
    }

    #[test]
    fn a_key_of_many_parts_nests_only_so_deep() {
        let key = ":".repeat(50_000);
        let lines = tree_lines([(0, key.as_str())].into_iter(), &BTreeSet::new());
        assert_eq!(lines.len(), MAX_LEVELS + 1);
        assert_eq!(lines.last().unwrap().depth, MAX_LEVELS);
        assert_eq!(key_parts(&key).len(), MAX_LEVELS + 1);
    }
}
