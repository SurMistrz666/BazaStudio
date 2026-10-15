//! "Podgląd wartości": one value of a grid read as bytes: in hex with its
//! printable text beside it, or laid out by a C or C++ structure bound to its
//! column. The structures are kept in the vault, one per column.
use super::*;
use crate::cstruct;
use crate::widgets::Btn;
use eframe::egui::RichText;
use std::borrow::Cow;
use std::hash::{Hash, Hasher};

/// Bytes on one line of the hex dump.
const BYTES_PER_LINE: usize = 16;
/// The vault record of the structures bound to columns.
const RECORD: &str = "blob-layouts";
/// Most structures kept, and the longest source of one.
const MAX_LAYOUTS: usize = 500;
const MAX_SOURCE: usize = 256 * 1024;

/// A C or C++ structure bound to a column: its values are read with it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct BlobLayout {
    pub connection: Uuid,
    pub schema: String,
    pub table: String,
    pub column: String,
    /// The declarations, as pasted.
    pub source: String,
    /// The structure a value is; the last declared one when not found.
    pub root: String,
    /// Bytes of `long` in the program that wrote the values: 4 or 8.
    pub long_bytes: u8,
    pub big_endian: bool,
}
impl BlobLayout {
    fn options(&self) -> cstruct::Options {
        cstruct::Options {
            long_bytes: if self.long_bytes == 8 { 8 } else { 4 },
            big_endian: self.big_endian,
        }
    }
    fn same_column(&self, other: &BlobLayout) -> bool {
        self.connection == other.connection
            && self.schema == other.schema
            && self.table == other.table
            && self.column == other.column
    }
}

/// The structures of the vault record, checked.
fn decode_layouts(bytes: Option<&[u8]>) -> anyhow::Result<Vec<BlobLayout>> {
    let rows: Vec<BlobLayout> = match bytes {
        Some(bytes) => serde_json::from_slice(bytes)?,
        None => Vec::new(),
    };
    anyhow::ensure!(
        rows.len() <= MAX_LAYOUTS && rows.iter().all(|row| row.source.len() <= MAX_SOURCE),
        "blob_layout_limit"
    );
    Ok(rows)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ValueMode {
    Hex,
    Structure,
}

/// What the structure gives for a value: the structures to choose from and
/// the decoded lines, or why the declarations cannot be read.
type Reading = Result<(Vec<String>, cstruct::Decoded), cstruct::Error>;

/// The value of a grid on screen in "Podgląd wartości".
pub(super) struct ValueView {
    pub tab: Uuid,
    pub row: usize,
    pub column: usize,
    mode: ValueMode,
    /// The structure being written for the column of the value.
    draft: BlobLayout,
    /// The column can keep a structure: it belongs to a table.
    bindable: bool,
    /// The last reading, by what it was made from.
    cache: Option<(u64, Reading)>,
}

/// The bytes of a value: a binary value as it is, anything else as the
/// UTF-8 bytes of its text. NULL has none.
pub(super) fn bytes_of(cell: &Cell) -> Cow<'_, [u8]> {
    match cell {
        Cell::Null => Cow::Borrowed(&[]),
        Cell::Bytes(bytes) => Cow::Borrowed(bytes),
        other => Cow::Owned(other.display().into_bytes()),
    }
}

/// One line of a hex dump: the offset, sixteen bytes in two groups of eight,
/// and the printable ASCII of them, a dot for any other byte.
pub(super) fn hex_line(bytes: &[u8], offset: usize) -> String {
    let line = &bytes[offset..(offset + BYTES_PER_LINE).min(bytes.len())];
    let mut hex = String::with_capacity(BYTES_PER_LINE * 3 + 1);
    for i in 0..BYTES_PER_LINE {
        if i == 8 {
            hex.push(' ');
        }
        match line.get(i) {
            Some(byte) => hex.push_str(&format!("{byte:02x} ")),
            None => hex.push_str("   "),
        }
    }
    let text: String = line
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        })
        .collect();
    format!("{offset:08x}  {hex} {text}")
}

/// What a reading depends on. A long value is known by its length and its
/// ends, which is enough to see that another value is shown.
fn reading_key(draft: &BlobLayout, bytes: &[u8]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (
        &draft.source,
        &draft.root,
        draft.long_bytes,
        draft.big_endian,
    )
        .hash(&mut hasher);
    bytes.len().hash(&mut hasher);
    if bytes.len() <= 1 << 20 {
        bytes.hash(&mut hasher);
    } else {
        bytes[..4096].hash(&mut hasher);
        bytes[bytes.len() - 4096..].hash(&mut hasher);
    }
    hasher.finish()
}

fn read(draft: &BlobLayout, bytes: &[u8]) -> Reading {
    let options = draft.options();
    let program = cstruct::parse(&draft.source, &options)?;
    let roots = cstruct::roots(&program);
    let decoded = cstruct::decode(&program, &draft.root, bytes, &options)?;
    Ok((roots, decoded))
}

/// What the window asks for once it is drawn.
enum Action {
    Copy(String),
    Save(BlobLayout),
    Remove(BlobLayout),
}

impl StudioApp {
    /// Open "Podgląd wartości" on a cell of a tab: with the structure of its
    /// column, when one is kept, otherwise in hex.
    pub(super) fn view_value(&mut self, tab: Uuid, row: usize, column: usize) {
        let Some(open) = self.tabs.iter().find(|t| t.id == tab) else {
            return;
        };
        let name = open
            .result
            .as_ref()
            .and_then(|result| result.columns.get(column))
            .map(|c| c.name.clone())
            .unwrap_or_default();
        let table = open.source.clone().or_else(|| open.table.clone());
        let mut draft = BlobLayout {
            connection: open.connection.unwrap_or_default(),
            schema: table.as_ref().map(|t| t.schema.clone()).unwrap_or_default(),
            table: table.as_ref().map(|t| t.name.clone()).unwrap_or_default(),
            column: name,
            long_bytes: 4,
            ..BlobLayout::default()
        };
        let bindable = open.connection.is_some() && table.is_some();
        let saved = self
            .blob_layouts
            .iter()
            .find(|layout| layout.same_column(&draft))
            .cloned();
        let mode = if saved.is_some() {
            ValueMode::Structure
        } else {
            ValueMode::Hex
        };
        if let Some(saved) = saved {
            draft = saved;
        }
        self.value_view = Some(ValueView {
            tab,
            row,
            column,
            mode,
            draft,
            bindable,
            cache: None,
        });
    }

    /// Read the structures bound to columns from the vault.
    pub(super) fn load_layouts(&mut self) {
        let Some(vault) = self.vault.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let r = tokio::task::spawn_blocking(move || {
                let bytes = vault.load_named(RECORD)?;
                decode_layouts(bytes.as_deref().map(Vec::as_slice))
            })
            .await
            .map_err(|_| "blob_layout_task_failed".to_owned())
            .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = tx.send(Message::Layouts(r)).await;
            ctx.request_repaint();
        });
    }

    /// Keep the structure of a column, or take it away.
    fn store_layout(&mut self, layout: BlobLayout, remove: bool) {
        let Some(vault) = self.vault.clone() else {
            return;
        };
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.track(async move {
            let r = tokio::task::spawn_blocking(move || {
                vault.update_named(RECORD, move |bytes| {
                    let mut rows = decode_layouts(bytes)?;
                    rows.retain(|row| !row.same_column(&layout));
                    if !remove {
                        rows.insert(0, layout);
                    }
                    let bytes = serde_json::to_vec(&rows)?;
                    decode_layouts(Some(&bytes))?;
                    Ok((bytes, rows))
                })
            })
            .await
            .map_err(|_| "blob_layout_task_failed".to_owned())
            .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = tx.send(Message::Layouts(r)).await;
            ctx.request_repaint();
        });
    }

    pub(super) fn value_window(&mut self, ctx: &egui::Context) {
        let Some(mut view) = self.value_view.take() else {
            return;
        };
        let Some(tab) = self.tabs.iter().find(|t| t.id == view.tab) else {
            return;
        };
        let Some(result) = tab.result.as_ref() else {
            return;
        };
        let Some(cell) = result
            .rows
            .get(view.row)
            .and_then(|row| row.get(view.column))
        else {
            return;
        };
        // A value waiting to be saved is the one the grid shows.
        let cell = tab.pending.get(&(view.row, view.column)).unwrap_or(cell);
        let bytes = bytes_of(cell);
        let column = &result.columns[view.column];
        let saved = self
            .blob_layouts
            .iter()
            .find(|layout| layout.same_column(&view.draft))
            .cloned();
        let p = self.palette();
        let en = self.en();
        let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
        let title = format!("{}: {}", l("Podgląd wartości", "Value viewer"), column.name);
        let mut open = true;
        let mut action = None;
        let area = ctx.content_rect();
        egui::Window::new(&title)
            .id(egui::Id::new("value_view"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(egui::vec2(980.0, 560.0))
            .default_pos(egui::pos2(area.center().x - 490.0, area.top() + 90.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (mode, text) in [
                        (ValueMode::Hex, "Hex"),
                        (
                            ValueMode::Structure,
                            l("Struktura C/C++", "C/C++ structure"),
                        ),
                    ] {
                        if ui.selectable_label(view.mode == mode, text).clicked() {
                            view.mode = mode;
                        }
                    }
                    ui.add_space(8.0);
                    widgets::caption(
                        ui,
                        &p,
                        &format!(
                            "{} · {} B · {} {}",
                            if column.data_type.is_empty() {
                                "—"
                            } else {
                                column.data_type.as_str()
                            },
                            bytes.len(),
                            l("wiersz", "row"),
                            view.row + 1
                        ),
                    );
                    if view.mode == ValueMode::Hex {
                        widgets::right(ui, |ui| {
                            if Btn::new(l("Kopiuj zrzut", "Copy dump"))
                                .small()
                                .enabled(!bytes.is_empty())
                                .show(ui, &p)
                                .clicked()
                            {
                                action = Some(Action::Copy(
                                    (0..bytes.len())
                                        .step_by(BYTES_PER_LINE)
                                        .map(|offset| hex_line(&bytes, offset))
                                        .collect::<Vec<_>>()
                                        .join("\n"),
                                ));
                            }
                            if Btn::new(l("Kopiuj hex", "Copy hex"))
                                .small()
                                .enabled(!bytes.is_empty())
                                .show(ui, &p)
                                .clicked()
                            {
                                action = Some(Action::Copy(Cell::Bytes(bytes.to_vec()).display()));
                            }
                        });
                    }
                });
                ui.separator();
                match view.mode {
                    ValueMode::Hex => hex_dump(ui, &p, &bytes, cell, en),
                    ValueMode::Structure => {
                        if let Some(chosen) =
                            structure(ui, &p, &mut view, saved.as_ref(), &bytes, en)
                        {
                            action = Some(chosen);
                        }
                    }
                }
            });
        match action {
            Some(Action::Copy(text)) => self.copy_out(ctx, text),
            Some(Action::Save(layout)) => self.store_layout(layout, false),
            Some(Action::Remove(layout)) => self.store_layout(layout, true),
            None => {}
        }
        if open {
            self.value_view = Some(view);
        }
    }
}

fn hex_dump(ui: &mut egui::Ui, p: &theme::Palette, bytes: &[u8], cell: &Cell, en: bool) {
    if bytes.is_empty() {
        let text = match (cell, en) {
            (Cell::Null, _) => "NULL",
            (_, true) => "The value is empty.",
            (_, false) => "Wartość jest pusta.",
        };
        widgets::help(ui, p, text);
        return;
    }
    let font = egui::FontId::monospace(p.code() - 0.5);
    let line_height = ui.fonts_mut(|f| f.row_height(&font)) + 2.0;
    let lines = bytes.len().div_ceil(BYTES_PER_LINE);
    // Only the lines on screen are laid out: a value may be megabytes long.
    egui::ScrollArea::both()
        .id_salt("value_hex")
        .auto_shrink([false, false])
        .show_rows(ui, line_height, lines, |ui, range| {
            for line in range {
                ui.label(
                    RichText::new(hex_line(bytes, line * BYTES_PER_LINE))
                        .font(font.clone())
                        .color(p.text),
                );
            }
        });
}

/// The declarations on the left, the decoded value on the right.
fn structure(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    view: &mut ValueView,
    saved: Option<&BlobLayout>,
    bytes: &[u8],
    en: bool,
) -> Option<Action> {
    let l = |pl: &'static str, english: &'static str| if en { english } else { pl };
    let key = reading_key(&view.draft, bytes);
    if view.cache.as_ref().is_none_or(|(cached, _)| *cached != key) {
        view.cache = Some((key, read(&view.draft, bytes)));
    }
    let mut action = None;
    let height = ui.available_height();
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            ui.set_width(380.0);
            ui.spacing_mut().item_spacing.y = 6.0;
            widgets::caption(ui, p, l("Deklaracje C/C++", "C/C++ declarations"));
            egui::ScrollArea::vertical()
                .id_salt("value_source")
                .max_height((height - 170.0).max(120.0))
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut view.draft.source)
                            .code_editor()
                            .desired_rows(18)
                            .desired_width(f32::INFINITY)
                            .hint_text(
                                "#pragma pack(push, 1)\nstruct TItem {\n    DWORD id;\n    char name[24];\n};\n#pragma pack(pop)",
                            ),
                    );
                });
            let roots = match view.cache.as_ref() {
                Some((_, Ok((roots, _)))) => roots.clone(),
                _ => Vec::new(),
            };
            let chosen = if roots.contains(&view.draft.root) {
                view.draft.root.clone()
            } else {
                roots.last().cloned().unwrap_or_else(|| "—".into())
            };
            ui.horizontal(|ui| {
                widgets::caption(ui, p, l("Struktura", "Structure"));
                widgets::combo(ui, p, "value_root", chosen, 260.0, |ui| {
                    for root in &roots {
                        ui.selectable_value(&mut view.draft.root, root.clone(), root);
                    }
                });
            });
            let mut long_eight = view.draft.long_bytes == 8;
            widgets::checkbox(
                ui,
                p,
                &mut long_eight,
                l(
                    "long ma 8 bajtów (Linux, BSD 64-bit)",
                    "long is 8 bytes (64-bit Linux, BSD)",
                ),
            );
            view.draft.long_bytes = if long_eight { 8 } else { 4 };
            widgets::checkbox(ui, p, &mut view.draft.big_endian, "Big-endian");
            let readable = matches!(view.cache, Some((_, Ok(_))));
            ui.horizontal(|ui| {
                let changed = saved != Some(&view.draft);
                if Btn::primary(l("Zapisz dla kolumny", "Keep for the column"))
                    .small()
                    .enabled(view.bindable && readable && changed)
                    .show(ui, p)
                    .clicked()
                {
                    action = Some(Action::Save(view.draft.clone()));
                }
                if saved.is_some() && Btn::new(l("Odepnij", "Unbind")).small().show(ui, p).clicked()
                {
                    action = Some(Action::Remove(view.draft.clone()));
                }
            });
            let note = match (view.bindable, en) {
                (true, true) => format!(
                    "Kept in the vault for the column “{}” of “{}”.",
                    view.draft.column, view.draft.table
                ),
                (true, false) => format!(
                    "Zapisywana w sejfie dla kolumny „{}” tabeli „{}”.",
                    view.draft.column, view.draft.table
                ),
                (false, _) => l(
                    "Ten wynik nie pochodzi z tabeli: strukturę można tylko podejrzeć.",
                    "This result is not a table: the structure can only be looked at.",
                )
                .to_owned(),
            };
            widgets::help(ui, p, &note);
        });
        ui.separator();
        ui.vertical(|ui| match view.cache.as_ref().map(|(_, reading)| reading) {
            Some(Err(error)) => widgets::feedback(ui, p, &error.describe(en), true),
            Some(Ok((_, decoded))) => decoded_lines(ui, p, decoded, bytes.len(), en),
            None => {}
        });
    });
    action
}

fn decoded_lines(
    ui: &mut egui::Ui,
    p: &theme::Palette,
    decoded: &cstruct::Decoded,
    available: usize,
    en: bool,
) {
    let summary = if en {
        format!(
            "Structure: {} B · records: {} · bytes left over: {}",
            decoded.size, decoded.records, decoded.rest
        )
    } else {
        format!(
            "Struktura: {} B · rekordy: {} · pozostałe bajty: {}",
            decoded.size, decoded.records, decoded.rest
        )
    };
    widgets::caption(ui, p, &summary);
    if available < decoded.size {
        let text = if en {
            format!(
                "The value has {available} B, fewer than the structure: the missing fields show “—”."
            )
        } else {
            format!("Wartość ma {available} B, mniej niż struktura: brakujące pola mają „—”.")
        };
        widgets::feedback(ui, p, &text, true);
    }
    let row_height = p.row_height();
    let font = egui::FontId::monospace(p.code() - 1.0);
    egui_extras::TableBuilder::new(ui)
        .id_salt("value_fields")
        .striped(true)
        .resizable(true)
        .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
        .column(
            egui_extras::Column::initial(220.0)
                .at_least(80.0)
                .clip(true),
        )
        .column(
            egui_extras::Column::initial(110.0)
                .at_least(60.0)
                .clip(true),
        )
        .column(egui_extras::Column::initial(70.0).at_least(50.0))
        .column(egui_extras::Column::remainder().at_least(120.0).clip(true))
        .header(row_height, |mut header| {
            for title in [
                if en { "Field" } else { "Pole" },
                if en { "Type" } else { "Typ" },
                "Offset",
                if en { "Value" } else { "Wartość" },
            ] {
                header.col(|ui| {
                    ui.label(RichText::new(title).size(p.small()).color(p.muted));
                });
            }
        })
        .body(|body| {
            body.rows(row_height, decoded.lines.len(), |mut row| {
                let line = &decoded.lines[row.index()];
                row.col(|ui| {
                    ui.label(
                        RichText::new(format!("{}{}", "  ".repeat(line.depth), line.name))
                            .font(font.clone())
                            .color(p.string),
                    );
                });
                row.col(|ui| {
                    ui.label(
                        RichText::new(&line.type_name)
                            .font(font.clone())
                            .color(p.number),
                    );
                });
                row.col(|ui| {
                    ui.label(
                        RichText::new(format!("0x{:04x}", line.offset))
                            .font(font.clone())
                            .color(p.muted),
                    )
                    .on_hover_text(format!("{} B", line.offset));
                });
                row.col(|ui| {
                    ui.label(RichText::new(&line.value).font(font.clone()).color(p.text));
                });
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dump_line_shows_offset_bytes_and_text() {
        let bytes = b"\xe9\x03\x00\x00item1\x00ABCDEFGHIJ";
        assert_eq!(
            hex_line(bytes, 0),
            "00000000  e9 03 00 00 69 74 65 6d  31 00 41 42 43 44 45 46  ....item1.ABCDEF"
        );
        // The last line is padded so that its text stays in the column.
        assert_eq!(
            hex_line(bytes, 16),
            "00000010  47 48 49 4a                                       GHIJ"
        );
        assert_eq!(bytes_of(&Cell::Text("zó".into())).as_ref(), "zó".as_bytes());
        assert!(bytes_of(&Cell::Null).is_empty());
    }

    #[test]
    fn the_kept_structures_are_checked() {
        let layout = BlobLayout {
            column: "payload".into(),
            source: "struct A { int a; };".into(),
            long_bytes: 4,
            ..BlobLayout::default()
        };
        let bytes = serde_json::to_vec(&vec![layout.clone()]).unwrap();
        assert_eq!(
            decode_layouts(Some(&bytes)).unwrap(),
            std::slice::from_ref(&layout)
        );
        assert!(decode_layouts(None).unwrap().is_empty());
        let huge = BlobLayout {
            source: "x".repeat(MAX_SOURCE + 1),
            ..layout
        };
        let bytes = serde_json::to_vec(&vec![huge]).unwrap();
        assert!(decode_layouts(Some(&bytes)).is_err());
    }
}
