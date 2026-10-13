//! Hand-painted icons and the small set of controls shared by every view.
//! Their proportions follow the workspace demo on the product portal.
use crate::theme::{self, Palette};
use eframe::egui::{self, Color32, Rect, Response, Stroke, pos2, vec2};

/// Line icons drawn on a 24 point grid, in the style of the portal's icon set.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Terminal,
    Table,
    Workflow,
    History,
    Upload,
    Download,
    Database,
    Activity,
    Folder,
    Shield,
    ChevronDown,
    ChevronRight,
    ChevronLeft,
    Play,
    Stop,
    Refresh,
    ArrowDown,
    ArrowUp,
    Check,
    Close,
    Plus,
    Search,
    Settings,
    Lock,
    File,
    Save,
    Filter,
    Code,
    View,
    Schema,
    Pencil,
    Trash,
    External,
    Sidebar,
    Focus,
    Bookmark,
    Minus,
    First,
    Last,
    Form,
}

/// Points of an elliptic arc. Angles are in degrees, 0 pointing right and 90
/// pointing down, as on the screen.
fn arc(cx: f32, cy: f32, rx: f32, ry: f32, from: f32, to: f32) -> Vec<(f32, f32)> {
    let steps = ((to - from).abs() / 15.0).ceil().max(2.0) as usize;
    (0..=steps)
        .map(|i| {
            let angle = (from + (to - from) * i as f32 / steps as f32).to_radians();
            (cx + rx * angle.cos(), cy + ry * angle.sin())
        })
        .collect()
}

pub fn paint(p: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let scale = rect.width().min(rect.height()) / 24.0;
    let origin = rect.center() - vec2(12.0, 12.0) * scale;
    let point = |x: f32, y: f32| origin + vec2(x, y) * scale;
    let stroke = Stroke::new((2.0 * scale).clamp(1.0, 1.7), color);
    let line = |coords: &[(f32, f32)]| {
        p.add(egui::Shape::line(
            coords.iter().map(|&(x, y)| point(x, y)).collect(),
            stroke,
        ));
    };
    let closed = |coords: &[(f32, f32)]| {
        p.add(egui::Shape::closed_line(
            coords.iter().map(|&(x, y)| point(x, y)).collect(),
            stroke,
        ));
    };
    let rounded = |x: f32, y: f32, w: f32, h: f32, r: f32| {
        p.rect_stroke(
            Rect::from_min_size(point(x, y), vec2(w, h) * scale),
            r * scale,
            stroke,
            egui::StrokeKind::Middle,
        );
    };
    let circle = |x: f32, y: f32, r: f32| {
        p.circle_stroke(point(x, y), r * scale, stroke);
    };
    match icon {
        Icon::Terminal => {
            line(&[(4., 17.), (10., 11.), (4., 5.)]);
            line(&[(12., 19.), (20., 19.)]);
        }
        Icon::Table => {
            rounded(3., 3., 18., 18., 2.);
            line(&[(3., 9.), (21., 9.)]);
            line(&[(9., 3.), (9., 21.)]);
        }
        Icon::Workflow => {
            rounded(3., 3., 8., 8., 2.);
            rounded(13., 13., 8., 8., 2.);
            let mut path = vec![(7., 11.), (7., 15.)];
            path.extend(arc(9., 15., 2., 2., 180., 90.));
            path.push((13., 17.));
            line(&path);
        }
        Icon::History | Icon::Refresh => {
            let mut path = arc(12., 12., 9., 9., 180., -137.);
            path.push((3., 8.));
            line(&path);
            line(&[(3., 3.), (3., 8.), (8., 8.)]);
            if icon == Icon::History {
                line(&[(12., 7.), (12., 12.), (16., 14.)]);
            }
        }
        Icon::Upload | Icon::Download => {
            line(&[
                (3., 15.),
                (3., 19.5),
                (4.5, 21.),
                (19.5, 21.),
                (21., 19.5),
                (21., 15.),
            ]);
            line(&[(12., 3.), (12., 15.)]);
            if icon == Icon::Upload {
                line(&[(7., 8.), (12., 3.), (17., 8.)]);
            } else {
                line(&[(7., 10.), (12., 15.), (17., 10.)]);
            }
        }
        Icon::Database => {
            closed(&arc(12., 5., 9., 3., 0., 345.));
            let mut body = vec![(3., 5.)];
            body.extend(arc(12., 19., 9., 3., 180., 0.));
            body.push((21., 5.));
            line(&body);
            line(&arc(12., 12., 9., 3., 180., 0.));
        }
        Icon::Activity => {
            line(&[
                (2., 12.),
                (5.6, 12.),
                (6.7, 10.6),
                (9., 2.6),
                (15., 21.4),
                (17.3, 13.4),
                (18.4, 12.),
                (22., 12.),
            ]);
        }
        Icon::Folder => {
            closed(&[
                (2., 18.),
                (2., 5.),
                (4., 3.),
                (7.9, 3.),
                (9.6, 3.9),
                (10.4, 5.1),
                (12.1, 6.),
                (20., 6.),
                (22., 8.),
                (22., 18.),
                (20., 20.),
                (4., 20.),
            ]);
        }
        Icon::Shield => {
            closed(&[
                (12., 2.2),
                (14.5, 3.9),
                (17., 4.8),
                (20., 5.),
                (20., 13.),
                (19.2, 16.2),
                (17., 18.9),
                (14., 20.9),
                (12., 21.9),
                (10., 20.9),
                (7., 18.9),
                (4.8, 16.2),
                (4., 13.),
                (4., 5.),
                (7., 4.8),
                (9.5, 3.9),
            ]);
            line(&[(9., 12.), (11., 14.), (15., 10.)]);
        }
        Icon::ChevronDown => line(&[(6., 9.), (12., 15.), (18., 9.)]),
        Icon::ChevronRight => line(&[(9., 6.), (15., 12.), (9., 18.)]),
        Icon::ChevronLeft => line(&[(15., 6.), (9., 12.), (15., 18.)]),
        Icon::Play => closed(&[(6., 3.5), (19.5, 12.), (6., 20.5)]),
        Icon::Stop => rounded(5.5, 5.5, 13., 13., 2.),
        Icon::ArrowDown => {
            line(&[(12., 5.), (12., 19.)]);
            line(&[(19., 12.), (12., 19.), (5., 12.)]);
        }
        Icon::ArrowUp => {
            line(&[(12., 19.), (12., 5.)]);
            line(&[(5., 12.), (12., 5.), (19., 12.)]);
        }
        Icon::Check => line(&[(20., 6.), (9., 17.), (4., 12.)]),
        Icon::Close => {
            line(&[(18., 6.), (6., 18.)]);
            line(&[(6., 6.), (18., 18.)]);
        }
        Icon::Plus => {
            line(&[(5., 12.), (19., 12.)]);
            line(&[(12., 5.), (12., 19.)]);
        }
        Icon::Search => {
            circle(11., 11., 7.5);
            line(&[(16.5, 16.5), (21., 21.)]);
        }
        Icon::Settings => {
            line(&[(11., 7.), (20., 7.)]);
            line(&[(4., 17.), (13., 17.)]);
            circle(7., 7., 3.);
            circle(17., 17., 3.);
        }
        Icon::Lock => {
            rounded(4., 11., 16., 10., 2.);
            let mut shackle = vec![(7.5, 11.)];
            shackle.extend(arc(12., 7.5, 4.5, 4.5, 180., 360.));
            shackle.push((16.5, 11.));
            line(&shackle);
        }
        Icon::File => {
            closed(&[
                (14., 2.),
                (6., 2.),
                (4., 4.),
                (4., 20.),
                (6., 22.),
                (18., 22.),
                (20., 20.),
                (20., 8.),
            ]);
            line(&[(14., 2.), (14., 8.), (20., 8.)]);
            line(&[(8., 13.), (16., 13.)]);
            line(&[(8., 17.), (16., 17.)]);
        }
        Icon::Save => {
            closed(&[
                (15.2, 3.),
                (5., 3.),
                (3., 5.),
                (3., 19.),
                (5., 21.),
                (19., 21.),
                (21., 19.),
                (21., 8.8),
            ]);
            line(&[(17., 21.), (17., 13.), (7., 13.), (7., 21.)]);
            line(&[(7., 3.), (7., 8.), (15., 8.)]);
        }
        Icon::Filter => {
            closed(&[
                (3., 4.),
                (21., 4.),
                (14., 12.5),
                (14., 19.),
                (10., 21.),
                (10., 12.5),
            ]);
        }
        Icon::Code => {
            line(&[(16., 18.), (22., 12.), (16., 6.)]);
            line(&[(8., 6.), (2., 12.), (8., 18.)]);
        }
        Icon::View => {
            let mut lens = arc(12., 17.5, 11.2, 11.2, 207., 333.);
            lens.extend(arc(12., 6.5, 11.2, 11.2, 27., 153.));
            closed(&lens);
            circle(12., 12., 3.);
        }
        Icon::Schema => {
            closed(&[(12., 2.5), (22., 7.5), (12., 12.5), (2., 7.5)]);
            line(&[(2., 12.), (12., 17.), (22., 12.)]);
            line(&[(2., 16.5), (12., 21.5), (22., 16.5)]);
        }
        Icon::Pencil => {
            closed(&[(17.5, 3.), (21., 6.5), (7.5, 20.), (2.5, 21.5), (4., 16.5)]);
            line(&[(15., 5.5), (18.5, 9.)]);
        }
        Icon::Trash => {
            line(&[(3., 6.), (21., 6.)]);
            line(&[
                (19., 6.),
                (19., 20.),
                (17., 22.),
                (7., 22.),
                (5., 20.),
                (5., 6.),
            ]);
            line(&[
                (8., 6.),
                (8., 4.),
                (10., 2.),
                (14., 2.),
                (16., 4.),
                (16., 6.),
            ]);
            line(&[(10., 11.), (10., 17.)]);
            line(&[(14., 11.), (14., 17.)]);
        }
        Icon::External => {
            line(&[(8., 7.), (17., 7.), (17., 16.)]);
            line(&[(7., 17.), (17., 7.)]);
        }
        Icon::Sidebar => {
            rounded(3., 3., 18., 18., 2.);
            line(&[(9., 3.), (9., 21.)]);
        }
        Icon::Focus => {
            line(&[(8., 3.), (5., 3.), (3., 5.), (3., 8.)]);
            line(&[(16., 3.), (19., 3.), (21., 5.), (21., 8.)]);
            line(&[(3., 16.), (3., 19.), (5., 21.), (8., 21.)]);
            line(&[(21., 16.), (21., 19.), (19., 21.), (16., 21.)]);
        }
        Icon::Bookmark => {
            closed(&[
                (19., 21.),
                (12., 17.),
                (5., 21.),
                (5., 5.),
                (7., 3.),
                (17., 3.),
                (19., 5.),
            ]);
        }
        Icon::Minus => line(&[(5., 12.), (19., 12.)]),
        Icon::First => {
            line(&[(7., 6.), (7., 18.)]);
            line(&[(17., 6.), (11., 12.), (17., 18.)]);
        }
        Icon::Last => {
            line(&[(17., 6.), (17., 18.)]);
            line(&[(7., 6.), (13., 12.), (7., 18.)]);
        }
        Icon::Form => {
            rounded(4., 4., 16., 16., 2.);
            line(&[(8., 9.), (11., 9.)]);
            line(&[(13., 9.), (16., 9.)]);
            line(&[(8., 13.), (11., 13.)]);
            line(&[(13., 13.), (16., 13.)]);
            line(&[(8., 17.), (11., 17.)]);
        }
    }
}

pub fn icon(ui: &mut egui::Ui, icon: Icon, size: f32, color: Color32) -> Response {
    let (rect, r) = ui.allocate_exact_size(vec2(size, size), egui::Sense::hover());
    paint(ui.painter(), rect, icon, color);
    r
}

/// The application mark: the database glyph in brand cyan.
pub fn brand_mark(ui: &mut egui::Ui, size: f32) -> Response {
    icon(ui, Icon::Database, size, theme::BRAND)
}

fn dimmed(color: Color32, enabled: bool) -> Color32 {
    if enabled {
        color
    } else {
        color.gamma_multiply(0.5)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The one main action of a view or dialog.
    Primary,
    /// A regular action.
    Secondary,
    /// A destructive action.
    Danger,
    /// A frameless action that stays out of the way until hovered.
    Quiet,
    /// A frameless action in the accent colour.
    Link,
}

/// A button painted in the look of the portal: `Btn::primary("Uruchom").icon(Icon::Play).show(ui, &p)`.
pub struct Btn<'a> {
    label: &'a str,
    icon: Option<Icon>,
    kind: Kind,
    enabled: bool,
    hint: Option<&'a str>,
    small: bool,
    min_width: f32,
    tip: Option<&'a str>,
}

impl<'a> Btn<'a> {
    fn with(label: &'a str, kind: Kind) -> Self {
        Self {
            label,
            icon: None,
            kind,
            enabled: true,
            hint: None,
            small: false,
            min_width: 0.0,
            tip: None,
        }
    }
    pub fn new(label: &'a str) -> Self {
        Self::with(label, Kind::Secondary)
    }
    pub fn primary(label: &'a str) -> Self {
        Self::with(label, Kind::Primary)
    }
    pub fn danger(label: &'a str) -> Self {
        Self::with(label, Kind::Danger)
    }
    pub fn quiet(label: &'a str) -> Self {
        Self::with(label, Kind::Quiet)
    }
    pub fn link(label: &'a str) -> Self {
        Self::with(label, Kind::Link)
    }
    pub fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    /// Keyboard shortcut shown after the label.
    pub fn hint(mut self, hint: &'a str) -> Self {
        self.hint = Some(hint);
        self
    }
    pub fn small(mut self) -> Self {
        self.small = true;
        self
    }
    pub fn min_width(mut self, width: f32) -> Self {
        self.min_width = width;
        self
    }
    pub fn tip(mut self, tip: &'a str) -> Self {
        self.tip = Some(tip);
        self
    }

    /// Width this button takes, to lay out a row before painting it.
    pub fn width(&self, ui: &egui::Ui, p: &Palette) -> f32 {
        self.measure(ui, p).0
    }

    fn measure(
        &self,
        ui: &egui::Ui,
        p: &Palette,
    ) -> (
        f32,
        std::sync::Arc<egui::Galley>,
        Option<std::sync::Arc<egui::Galley>>,
    ) {
        let framed = !matches!(self.kind, Kind::Link);
        let pad = match (framed, self.small) {
            (false, _) => 2.0,
            (true, true) => 9.0,
            (true, false) => 13.0,
        };
        let size = if self.small {
            p.control() - 0.5
        } else {
            p.control()
        };
        let font = match self.kind {
            Kind::Quiet | Kind::Link => egui::FontId::proportional(size),
            _ => theme::medium(size),
        };
        let label = ui
            .painter()
            .layout_no_wrap(self.label.to_owned(), font, Color32::PLACEHOLDER);
        let hint = self.hint.map(|hint| {
            ui.painter().layout_no_wrap(
                hint.to_owned(),
                egui::FontId::monospace(p.small()),
                Color32::PLACEHOLDER,
            )
        });
        let mut width = pad * 2.0 + label.size().x;
        if self.icon.is_some() {
            width += p.icon() + if self.label.is_empty() { 0.0 } else { 7.0 };
        }
        if let Some(hint) = &hint {
            width += 10.0 + hint.size().x;
        }
        (width.max(self.min_width), label, hint)
    }

    pub fn show(self, ui: &mut egui::Ui, p: &Palette) -> Response {
        let (width, label, hint) = self.measure(ui, p);
        let height = if self.small {
            p.control_height() - 4.0
        } else {
            p.control_height()
        };
        let sense = if self.enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        };
        let (rect, response) = ui.allocate_exact_size(vec2(width, height), sense);
        let enabled = self.enabled && ui.is_enabled();
        response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, self.label)
        });
        if ui.is_rect_visible(rect) {
            let hot = enabled && (response.hovered() || response.has_focus());
            let down = enabled && response.is_pointer_button_down_on();
            let (fill, border, text) = match self.kind {
                Kind::Primary => (
                    Some(if down {
                        theme::blend(theme::PRIMARY, Color32::BLACK, 0.12)
                    } else if hot {
                        theme::PRIMARY_HOVER
                    } else {
                        theme::PRIMARY
                    }),
                    Some(theme::PRIMARY_BORDER),
                    Color32::WHITE,
                ),
                Kind::Secondary => (
                    Some(if hot { p.raised_hover } else { p.raised }),
                    Some(if hot {
                        theme::blend(p.line, p.text, 0.28)
                    } else {
                        p.line
                    }),
                    p.text,
                ),
                Kind::Danger => (
                    Some(if hot {
                        theme::DANGER_HOVER
                    } else {
                        theme::DANGER
                    }),
                    Some(theme::DANGER_BORDER),
                    theme::blend(p.error, Color32::WHITE, 0.4),
                ),
                Kind::Quiet => (
                    hot.then_some(p.raised),
                    None,
                    if hot { p.text } else { p.muted },
                ),
                Kind::Link => (None, None, p.accent),
            };
            let painter = ui.painter();
            if let Some(fill) = fill {
                painter.rect_filled(rect, 5.0, dimmed(fill, enabled));
            }
            if let Some(border) = border {
                painter.rect_stroke(
                    rect,
                    5.0,
                    Stroke::new(1.0, dimmed(border, enabled)),
                    egui::StrokeKind::Inside,
                );
            }
            if response.has_focus() {
                painter.rect_stroke(
                    rect.expand(2.0),
                    7.0,
                    Stroke::new(1.0, p.accent),
                    egui::StrokeKind::Outside,
                );
            }
            let text = dimmed(text, enabled);
            let side = p.icon();
            let gap = if self.label.is_empty() { 0.0 } else { 7.0 };
            let mut content = label.size().x;
            if self.icon.is_some() {
                content += side + gap;
            }
            if let Some(hint) = &hint {
                content += 10.0 + hint.size().x;
            }
            let mut x = rect.center().x - content / 2.0;
            if let Some(icon) = self.icon {
                paint(
                    painter,
                    Rect::from_center_size(pos2(x + side / 2.0, rect.center().y), vec2(side, side)),
                    icon,
                    text,
                );
                x += side + gap;
            }
            let label_width = label.size().x;
            painter.galley(pos2(x, rect.center().y - label.size().y / 2.0), label, text);
            if self.kind == Kind::Link && hot {
                painter.hline(
                    x..=x + label_width,
                    rect.center().y + p.control() * 0.62,
                    Stroke::new(1.0, text),
                );
            }
            x += label_width;
            if let Some(hint) = hint {
                let color = dimmed(theme::blend(text, theme::PRIMARY, 0.25), enabled);
                painter.galley(
                    pos2(x + 10.0, rect.center().y - hint.size().y / 2.0),
                    hint,
                    color,
                );
            }
        }
        let response = if enabled {
            response.on_hover_cursor(egui::CursorIcon::PointingHand)
        } else {
            response
        };
        match self.tip {
            Some(tip) => response.on_hover_text(tip),
            None => response,
        }
    }
}

/// Square frameless icon action with a tooltip.
pub fn icon_button(ui: &mut egui::Ui, p: &Palette, icon: Icon, tip: &str) -> Response {
    icon_toggle(ui, p, icon, tip, false)
}

/// Like [`icon_button`], drawn in the accent colour while `active`.
pub fn icon_toggle(
    ui: &mut egui::Ui,
    p: &Palette,
    icon: Icon,
    tip: &str,
    active: bool,
) -> Response {
    let side = (p.control_height() - 3.0).max(24.0);
    let (rect, r) = ui.allocate_exact_size(vec2(side, side), egui::Sense::click());
    let enabled = ui.is_enabled();
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, tip));
    let hot = enabled && (r.hovered() || r.has_focus());
    if hot {
        ui.painter().rect_filled(rect, 5.0, p.raised);
    }
    let color = if active {
        p.accent
    } else if hot {
        p.text
    } else {
        p.muted
    };
    paint(
        ui.painter(),
        Rect::from_center_size(rect.center(), vec2(p.icon() + 1.0, p.icon() + 1.0)),
        icon,
        dimmed(color, enabled),
    );
    r.on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(tip)
}

/// One tab of the section strip, or of a dialog's tab row.
#[allow(clippy::too_many_arguments)]
pub fn tab(
    ui: &mut egui::Ui,
    p: &Palette,
    icon: Option<Icon>,
    label: &str,
    active: bool,
    pad: f32,
    show_label: bool,
    height: f32,
) -> Response {
    let font = egui::FontId::proportional(p.control());
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER);
    let side = p.icon() + 1.0;
    let mut width = pad * 2.0;
    if icon.is_some() {
        width += side;
    }
    if show_label {
        width += galley.size().x + if icon.is_some() { 8.0 } else { 0.0 };
    }
    let (rect, r) = ui.allocate_exact_size(vec2(width.round(), height), egui::Sense::click());
    r.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, ui.is_enabled(), active, label)
    });
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 0.0, p.bg);
        painter.rect_filled(
            Rect::from_min_size(rect.min, vec2(rect.width(), 2.0)),
            0.0,
            p.accent,
        );
    } else if r.hovered() {
        painter.rect_filled(rect, 0.0, theme::blend(p.side, p.raised, 0.7));
    }
    painter.vline(rect.right() - 0.5, rect.y_range(), Stroke::new(1.0, p.line));
    if r.has_focus() {
        painter.rect_stroke(
            rect.shrink(2.0),
            3.0,
            Stroke::new(1.0, p.accent),
            egui::StrokeKind::Inside,
        );
    }
    let color = if active || r.hovered() {
        p.text
    } else {
        p.muted
    };
    let mut x = rect.left() + pad;
    let middle = rect.center().y + if active { 1.0 } else { 0.0 };
    if let Some(icon) = icon {
        paint(
            painter,
            Rect::from_center_size(pos2(x + side / 2.0, middle), vec2(side, side)),
            icon,
            color,
        );
        x += side + 8.0;
    }
    if show_label {
        painter.galley(pos2(x, middle - galley.size().y / 2.0), galley, color);
    }
    let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
    if show_label {
        r
    } else {
        r.on_hover_text(label)
    }
}

/// Text cut to one line with an ellipsis.
pub fn one_line(
    painter: &egui::Painter,
    text: &str,
    font: egui::FontId,
    color: Color32,
    width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = egui::text::TextWrapping {
        max_width: width.max(8.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    painter.layout_job(job)
}

/// Shorten `text` in its middle until it is at most `width` wide: the start
/// and, a little longer, the end stay, with `…` between them. A file path
/// keeps its drive or root and its file name that way. Returns the text to
/// show and whether anything was left out.
pub fn middle_ellipsis(
    painter: &egui::Painter,
    text: &str,
    font: &egui::FontId,
    width: f32,
) -> (String, bool) {
    let fits = |candidate: &str| {
        painter
            .layout_no_wrap(candidate.to_owned(), font.clone(), Color32::PLACEHOLDER)
            .size()
            .x
            <= width
    };
    if fits(text) {
        return (text.to_owned(), false);
    }
    let chars = text.chars().collect::<Vec<_>>();
    let shortened = |kept: usize| {
        let head = kept * 2 / 5;
        let tail = kept - head;
        chars[..head]
            .iter()
            .chain(std::iter::once(&'…'))
            .chain(&chars[chars.len() - tail..])
            .collect::<String>()
    };
    // The largest number of characters that still fits, by halving.
    let (mut low, mut high) = (0, chars.len().saturating_sub(1));
    while low < high {
        let middle = (low + high).div_ceil(2);
        if fits(&shortened(middle)) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    (shortened(low), true)
}

fn chevron(painter: &egui::Painter, center: egui::Pos2, side: f32, open: bool, color: Color32) {
    paint(
        painter,
        Rect::from_center_size(center, vec2(side, side)),
        if open {
            Icon::ChevronDown
        } else {
            Icon::ChevronRight
        },
        color,
    );
}

pub struct TreeRow<'a> {
    pub id: egui::Id,
    pub label: &'a str,
    pub icon: Icon,
    /// Counter at the right edge.
    pub count: Option<usize>,
    /// `Some` for a branch: whether it is expanded.
    pub open: Option<bool>,
    pub selected: bool,
    /// Own colour of the icon, e.g. the colour of a connection group.
    pub tint: Option<Color32>,
}

/// Focusable tree row with one full-width hit target. The label is supplied to
/// AccessKit, and keyboard activation uses the same action as a click.
pub fn tree_row(ui: &mut egui::Ui, p: &Palette, row: TreeRow<'_>) -> Response {
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width().max(40.0), p.row_height()),
        egui::Sense::hover(),
    );
    let response = ui.interact(rect, row.id, egui::Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            ui.is_enabled(),
            if let Some(count) = row.count {
                format!("{} ({count})", row.label)
            } else {
                row.label.into()
            },
        )
    });
    if response.clicked() {
        response.request_focus();
    }
    let painter = ui.painter();
    if row.selected {
        painter.rect_filled(rect, 3.0, p.raised);
    } else if response.hovered() {
        painter.rect_filled(rect, 3.0, theme::blend(p.side, p.raised, 0.6));
    }
    if response.has_focus() {
        painter.rect_stroke(
            rect.shrink(0.5),
            3.0,
            Stroke::new(1.0, theme::blend(p.line, p.accent, 0.6)),
            egui::StrokeKind::Inside,
        );
    }
    let color = if row.selected { p.accent } else { p.text };
    let side = p.icon();
    let mut left = rect.left() + 5.0;
    if let Some(open) = row.open {
        chevron(
            painter,
            pos2(left + side / 2.0 - 1.0, rect.center().y),
            side - 2.0,
            open,
            p.muted,
        );
    }
    left += side + 4.0;
    paint(
        painter,
        Rect::from_center_size(pos2(left + side / 2.0, rect.center().y), vec2(side, side)),
        row.icon,
        if row.selected {
            p.accent
        } else {
            row.tint.unwrap_or(p.muted)
        },
    );
    left += side + 7.0;
    let count = row.count.map(|count| {
        painter.layout_no_wrap(
            count.to_string(),
            egui::FontId::proportional(p.small()),
            p.muted,
        )
    });
    let right = rect.right() - 8.0 - count.as_ref().map_or(0.0, |g| g.size().x + 8.0);
    let galley = one_line(
        painter,
        row.label,
        egui::FontId::proportional(p.body()),
        color,
        right - left,
    );
    painter.galley(
        pos2(left, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    if let Some(count) = count {
        painter.galley(
            pos2(
                rect.right() - 8.0 - count.size().x,
                rect.center().y - count.size().y / 2.0,
            ),
            count,
            p.muted,
        );
    }
    response
}

pub fn keyboard_activate(response: &Response) -> bool {
    response.has_focus()
        && response.ctx.input(|input| {
            !input.modifiers.ctrl
                && !input.modifiers.command
                && !input.modifiers.alt
                && (input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Space))
        })
}

pub fn update_tree_open(
    ui: &egui::Ui,
    response: &Response,
    state: &mut egui::collapsing_header::CollapsingState,
) {
    if response.clicked() || keyboard_activate(response) {
        state.toggle(ui);
    }
    if response.has_focus() {
        if ui.input(|input| input.key_pressed(egui::Key::ArrowRight)) {
            state.set_open(true);
        }
        if ui.input(|input| input.key_pressed(egui::Key::ArrowLeft)) {
            state.set_open(false);
        }
    }
}

pub struct ConnectionRow {
    pub row: Response,
    pub disclosure: Response,
    pub more: Response,
}

/// Two-line connection identity: the mark of the database product and the
/// name, and the product and address below it. The colour chosen for the
/// connection rings the mark. The row remains a native accessible widget with
/// separate disclosure and menu targets.
#[allow(clippy::too_many_arguments)]
pub fn connection_row(
    ui: &mut egui::Ui,
    p: &Palette,
    id: uuid::Uuid,
    name: &str,
    subtitle: &str,
    mark: baza_core::Badge,
    connected: bool,
    selected: bool,
    open: bool,
    tint: Option<Color32>,
    menu_label: &str,
) -> ConnectionRow {
    let height = p.row_height() + p.small() + 7.0;
    let (rect, row) =
        ui.allocate_exact_size(vec2(ui.available_width(), height), egui::Sense::click());
    row.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), name));
    let first_line = rect.top() + p.row_height() / 2.0 + 1.0;
    let disclosure_rect = Rect::from_center_size(
        pos2(rect.left() + 5.0 + p.icon() / 2.0 - 1.0, first_line),
        vec2(p.icon() + 6.0, p.row_height() - 2.0),
    );
    let disclosure = ui.interact(
        disclosure_rect,
        egui::Id::new(("connection_disclosure", id)),
        egui::Sense::click(),
    );
    disclosure
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), name));
    let more_rect = Rect::from_center_size(
        pos2(rect.right() - 14.0, first_line),
        vec2(22.0, p.row_height() - 4.0),
    );
    let more = ui
        .interact(
            more_rect,
            egui::Id::new(("connection_more", id)),
            egui::Sense::click(),
        )
        .on_hover_text(menu_label);
    more.widget_info(|| {
        egui::WidgetInfo::labeled(
            egui::WidgetType::Button,
            ui.is_enabled(),
            format!("{menu_label}: {name}"),
        )
    });
    let painter = ui.painter();
    let hot = row.hovered() || more.hovered() || disclosure.hovered();
    if selected {
        painter.rect_filled(rect, 3.0, p.raised);
    } else if hot {
        painter.rect_filled(rect, 3.0, theme::blend(p.side, p.raised, 0.6));
    }
    if row.has_focus() {
        painter.rect_stroke(
            rect.shrink(0.5),
            3.0,
            Stroke::new(1.0, theme::blend(p.line, p.accent, 0.6)),
            egui::StrokeKind::Inside,
        );
    }
    let side = p.icon();
    chevron(painter, disclosure_rect.center(), side - 2.0, open, p.muted);
    let color = if selected { p.accent } else { p.text };
    let icon_left = rect.left() + 5.0 + side + 4.0;
    let mark_rect = Rect::from_center_size(
        pos2(icon_left + side / 2.0 + 1.0, first_line),
        vec2(side + 3.0, side + 3.0),
    );
    paint_badge(painter, mark_rect, mark, true);
    if let Some(tint) = tint {
        painter.rect_stroke(
            mark_rect.expand(2.0),
            (side + 3.0) * 0.24 + 2.0,
            Stroke::new(1.2, tint),
            egui::StrokeKind::Inside,
        );
    }
    let text_left = icon_left + side + 10.0;
    let text_right = more_rect.left() - 14.0;
    let title = one_line(
        painter,
        name,
        egui::FontId::proportional(p.body()),
        color,
        text_right - text_left,
    );
    painter.galley(
        pos2(text_left, first_line - title.size().y / 2.0),
        title,
        color,
    );
    let sub = one_line(
        painter,
        subtitle,
        egui::FontId::proportional(p.small()),
        p.muted,
        rect.right() - 8.0 - text_left,
    );
    painter.galley(
        pos2(text_left, rect.top() + p.row_height() + 1.0),
        sub,
        p.muted,
    );
    painter.circle_filled(
        pos2(more_rect.left() - 6.0, first_line),
        3.0,
        if connected {
            p.positive
        } else {
            theme::blend(p.side, p.muted, 0.45)
        },
    );
    if more.hovered() {
        painter.rect_filled(more_rect, 4.0, theme::blend(p.raised, p.text, 0.1));
    }
    if hot || selected {
        for offset in [-4.0, 0.0, 4.0] {
            painter.circle_filled(
                more_rect.center() + vec2(0.0, offset),
                1.2,
                if more.hovered() { p.text } else { p.muted },
            );
        }
    }
    ConnectionRow {
        row,
        disclosure,
        more,
    }
}

/// Small upper-case, letter-spaced caption such as "POŁĄCZENIA".
pub fn caption(ui: &mut egui::Ui, p: &Palette, text: &str) -> Response {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &text.to_uppercase(),
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::proportional(p.caption()),
            extra_letter_spacing: p.caption() * 0.11,
            color: p.muted,
            ..Default::default()
        },
    );
    ui.add(egui::Label::new(job).selectable(false))
}

/// Outlined upper-case state, such as "POŁĄCZONO".
pub fn badge(ui: &mut egui::Ui, p: &Palette, text: &str, color: Color32) -> Response {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &text.to_uppercase(),
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::proportional(p.caption()),
            extra_letter_spacing: p.caption() * 0.08,
            color,
            ..Default::default()
        },
    );
    egui::Frame::NONE
        .stroke(Stroke::new(1.0, theme::blend(p.line, color, 0.35)))
        .corner_radius(4)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| ui.add(egui::Label::new(job).selectable(false)))
        .response
}

/// Text in the heading weight.
pub fn heading(ui: &mut egui::Ui, p: &Palette, text: &str) -> Response {
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .font(theme::strong(p.heading()))
                .color(p.text),
        )
        .selectable(false),
    )
}

/// Second level heading inside a view.
pub fn subheading(ui: &mut egui::Ui, p: &Palette, text: &str) -> Response {
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .font(theme::medium(p.font + 1.5))
                .color(p.text),
        )
        .selectable(false),
    )
}

/// Muted explanatory sentence.
pub fn help(ui: &mut egui::Ui, p: &Palette, text: &str) -> Response {
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .size(p.body() - 0.5)
                .color(p.muted),
        )
        .wrap(),
    )
}

/// Label above a value or a form field.
pub fn label(ui: &mut egui::Ui, p: &Palette, text: &str) -> Response {
    ui.add(
        egui::Label::new(egui::RichText::new(text).size(p.small()).color(p.muted))
            .selectable(false),
    )
}

/// Heading of a section view: title, one muted sentence, and optional actions
/// aligned to the right edge.
pub fn pane_heading(
    ui: &mut egui::Ui,
    p: &Palette,
    title: &str,
    text: &str,
    actions: impl FnOnce(&mut egui::Ui),
) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
        actions(ui);
        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.spacing_mut().item_spacing.y = 7.0;
            heading(ui, p, title);
            if !text.is_empty() {
                help(ui, p, text);
            }
        });
    });
    ui.add_space(20.0);
}

/// Empty state of a view: an icon, a short title, one muted sentence and one
/// action. Returns `true` when the action was chosen.
pub fn empty_state(
    ui: &mut egui::Ui,
    p: &Palette,
    icon_: Icon,
    title: &str,
    text: &str,
    action: Option<&str>,
) -> bool {
    let mut clicked = false;
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(18, 30))
        .show(ui, |ui| {
            ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.spacing_mut().item_spacing.y = 12.0;
                ui.set_max_width(500.0_f32.min(ui.available_width()));
                icon(ui, icon_, 23.0, p.muted);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(title)
                            .font(theme::medium(p.font + 1.5))
                            .color(p.muted),
                    )
                    .selectable(false),
                );
                help(ui, p, text);
                if let Some(action) = action {
                    clicked = Btn::new(action).small().show(ui, p).clicked();
                }
            });
        });
    clicked
}

/// Full-width one pixel line.
pub fn hline(ui: &mut egui::Ui, p: &Palette) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, p.line);
}

/// Toolbar of a section view: one padded row with a line below.
pub fn toolbar<R>(ui: &mut egui::Ui, p: &Palette, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let inner = egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(16, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.set_min_height(p.control_height());
                ui.spacing_mut().item_spacing.x = 8.0;
                add(ui)
            })
            .inner
        })
        .inner;
    hline(ui, p);
    inner
}

/// Lay the rest of a horizontal row out from the right edge.
pub fn right<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add)
        .inner
}

/// Single-line input in the look of the portal.
pub fn field<'t>(p: &Palette, text: &'t mut dyn egui::TextBuffer) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text)
        .margin(egui::Margin::symmetric(9, 5))
        .min_size(vec2(0.0, p.control_height()))
        .vertical_align(egui::Align::Center)
        .font(egui::FontId::proportional(p.body()))
}

/// Add a single-line field of a form. When the Tab key brings the keyboard
/// to it, its text is selected, so typing replaces a default such as
/// `localhost` instead of being appended to it. A click places the cursor.
pub fn form_field(ui: &mut egui::Ui, field: egui::TextEdit<'_>) -> Response {
    let mut output = field.show(ui);
    let by_pointer =
        ui.input(|i| i.pointer.any_down() || i.pointer.any_pressed() || i.pointer.any_released());
    if output.response.gained_focus() && !by_pointer {
        let end = output.galley.job.text.chars().count();
        output
            .state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(end),
            )));
        output.state.store(ui.ctx(), output.response.id);
        ui.ctx().request_repaint();
    }
    output.response.response
}

fn chevron_icon(ui: &egui::Ui, rect: Rect, visuals: &egui::style::WidgetVisuals, _open: bool) {
    paint(
        ui.painter(),
        Rect::from_center_size(rect.center(), vec2(12.0, 12.0)),
        Icon::ChevronDown,
        visuals.fg_stroke.color.gamma_multiply(0.8),
    );
}

fn combo_scope<R>(
    ui: &mut egui::Ui,
    p: &Palette,
    framed: bool,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.scope(|ui| {
        let text_height = ui.text_style_height(&egui::TextStyle::Button);
        ui.spacing_mut().button_padding = vec2(
            if framed { 9.0 } else { 6.0 },
            ((p.control_height() - text_height) / 2.0 - if framed { 0.0 } else { 2.0 }).max(2.0),
        );
        let widgets = &mut ui.visuals_mut().widgets;
        if framed {
            for visuals in [
                &mut widgets.inactive,
                &mut widgets.hovered,
                &mut widgets.active,
                &mut widgets.open,
            ] {
                visuals.weak_bg_fill = p.bg;
                visuals.bg_fill = p.bg;
            }
        } else {
            for visuals in [&mut widgets.inactive, &mut widgets.open] {
                visuals.weak_bg_fill = Color32::TRANSPARENT;
                visuals.bg_fill = Color32::TRANSPARENT;
                visuals.bg_stroke = Stroke::NONE;
            }
            for visuals in [&mut widgets.hovered, &mut widgets.active] {
                visuals.weak_bg_fill = p.raised;
                visuals.bg_fill = p.raised;
                visuals.bg_stroke = Stroke::NONE;
            }
        }
        add(ui)
    })
    .inner
}

/// Select in the look of the portal's inputs: background fill, a line border
/// and a chevron.
pub fn combo<R>(
    ui: &mut egui::Ui,
    p: &Palette,
    id: impl egui::AsIdSalt,
    selected: impl Into<egui::WidgetText>,
    width: f32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<Option<R>> {
    combo_scope(ui, p, true, |ui| {
        egui::ComboBox::from_id_salt(id)
            .width(width)
            .selected_text(selected)
            .icon(chevron_icon)
            .truncate()
            .show_ui(ui, add)
    })
}

/// Frameless select used in bars, such as the "Wygląd" selector.
pub fn quiet_combo<R>(
    ui: &mut egui::Ui,
    p: &Palette,
    id: impl egui::AsIdSalt,
    selected: impl Into<egui::WidgetText>,
    width: f32,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<Option<R>> {
    combo_scope(ui, p, false, |ui| {
        egui::ComboBox::from_id_salt(id)
            .width(width)
            .selected_text(selected)
            .icon(chevron_icon)
            .show_ui(ui, add)
    })
}

/// Check box with the accent fill of the portal.
pub fn checkbox(ui: &mut egui::Ui, p: &Palette, value: &mut bool, text: &str) -> Response {
    let galley = ui.painter().layout_no_wrap(
        text.to_owned(),
        egui::FontId::proportional(p.body()),
        Color32::PLACEHOLDER,
    );
    let gap = if text.is_empty() { 0.0 } else { 8.0 };
    let size = vec2(
        16.0 + gap + galley.size().x,
        galley.size().y.max(16.0).max(p.control_height() - 6.0),
    );
    let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click());
    if response.clicked() {
        *value = !*value;
        response.mark_changed();
    }
    let enabled = ui.is_enabled();
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, enabled, *value, text)
    });
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let mark =
            Rect::from_center_size(pos2(rect.left() + 8.0, rect.center().y), vec2(15.0, 15.0));
        if *value {
            painter.rect_filled(mark, 3.0, dimmed(p.accent, enabled));
            paint(
                painter,
                mark.shrink(2.5),
                Icon::Check,
                dimmed(theme::blend(p.bg, Color32::BLACK, 0.35), enabled),
            );
        } else {
            painter.rect_filled(mark, 3.0, p.bg);
            painter.rect_stroke(
                mark,
                3.0,
                Stroke::new(
                    1.0,
                    dimmed(
                        if response.hovered() {
                            theme::blend(p.line, p.text, 0.4)
                        } else {
                            theme::blend(p.line, p.text, 0.15)
                        },
                        enabled,
                    ),
                ),
                egui::StrokeKind::Inside,
            );
        }
        if response.has_focus() {
            painter.rect_stroke(
                mark.expand(2.0),
                5.0,
                Stroke::new(1.0, p.accent),
                egui::StrokeKind::Outside,
            );
        }
        painter.galley(
            pos2(
                rect.left() + 16.0 + gap,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            dimmed(p.text, enabled),
        );
    }
    response
}

/// Inline underlined link. Opening pages is done by the caller.
pub fn link(ui: &mut egui::Ui, p: &Palette, text: &str) -> Response {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: egui::FontId::proportional(p.body() - 0.5),
            color: p.accent,
            underline: Stroke::new(1.0, p.accent),
            ..Default::default()
        },
    );
    ui.add(
        egui::Label::new(job)
            .selectable(false)
            .sense(egui::Sense::click()),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Result line of an action: the accent colour for a notice, the error colour
/// for a failure.
pub fn feedback(ui: &mut egui::Ui, p: &Palette, text: &str, error: bool) {
    egui::Frame::NONE
        .fill(p.raised)
        .corner_radius(5)
        .inner_margin(egui::Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(
                egui::Label::new(
                    egui::RichText::new(text)
                        .size(p.body() - 0.5)
                        .color(if error { p.error } else { p.accent }),
                )
                .wrap(),
            );
        });
}

/// Small filled dot, as in the status bar.
pub fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(6.0, 6.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 2.5, color);
}

pub struct DialogOutput {
    /// The close button, Escape, or a click outside the dialog.
    pub close: bool,
    /// `close` came from a click outside the dialog. A form that holds
    /// unsaved input may choose to stay open then.
    pub outside: bool,
}

/// Modal dialog with a titled header. The contents add a [`dialog_body`] and a
/// [`dialog_footer`].
pub fn dialog(
    ctx: &egui::Context,
    p: &Palette,
    id: &str,
    title: &str,
    width: f32,
    close_tip: &str,
    add: impl FnOnce(&mut egui::Ui),
) -> DialogOutput {
    let width = width.min(ctx.content_rect().width() - 32.0).max(280.0);
    let mut close = false;
    let modal = egui::Modal::new(egui::Id::new(id))
        .backdrop_color(Color32::from_black_alpha(150))
        .frame(
            egui::Frame::NONE
                .fill(p.bg)
                .stroke(Stroke::new(1.0, p.line))
                .corner_radius(8)
                .shadow(egui::Shadow {
                    offset: [0, 16],
                    blur: 44,
                    spread: 0,
                    color: Color32::from_black_alpha(120),
                }),
        )
        .show(ctx, |ui| {
            ui.set_width(width);
            ui.set_max_width(width);
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::Frame::NONE
                .fill(p.side)
                .corner_radius(egui::CornerRadius {
                    nw: 7,
                    ne: 7,
                    sw: 0,
                    se: 0,
                })
                .inner_margin(egui::Margin {
                    left: 18,
                    right: 8,
                    top: 0,
                    bottom: 0,
                })
                .show(ui, |ui| {
                    ui.set_width(width - 26.0);
                    ui.horizontal(|ui| {
                        ui.set_height(42.0);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(title)
                                    .font(theme::strong(p.font + 1.5))
                                    .color(p.text),
                            )
                            .selectable(false)
                            .truncate(),
                        );
                        right(ui, |ui| {
                            close = icon_button(ui, p, Icon::Close, close_tip).clicked();
                        });
                    });
                });
            hline(ui, p);
            add(ui)
        });
    DialogOutput {
        close: close || modal.should_close(),
        outside: !close && modal.backdrop_response.clicked(),
    }
}

/// Padded content of a [`dialog`].
pub fn dialog_body<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let width = ui.available_width();
    egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(20, 18))
        .show(ui, |ui| {
            ui.set_width(width - 40.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            add(ui)
        })
        .inner
}

/// Action row of a [`dialog`]. Put secondary content first and the buttons in
/// [`right`], primary action first so that it ends up at the right edge.
pub fn dialog_footer<R>(ui: &mut egui::Ui, p: &Palette, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    hline(ui, p);
    let width = ui.available_width();
    egui::Frame::NONE
        .fill(p.side)
        .corner_radius(egui::CornerRadius {
            nw: 0,
            ne: 0,
            sw: 7,
            se: 7,
        })
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(width - 32.0);
            ui.horizontal(|ui| {
                ui.set_min_height(p.control_height());
                ui.spacing_mut().item_spacing.x = 8.0;
                add(ui)
            })
            .inner
        })
        .inner
}

/// One row of a form: a label column of fixed width and the control.
pub fn form_row<R>(
    ui: &mut egui::Ui,
    p: &Palette,
    label_width: f32,
    text: &str,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    ui.horizontal(|ui| {
        ui.set_min_height(p.control_height());
        ui.spacing_mut().item_spacing.x = 12.0;
        let (rect, _) =
            ui.allocate_exact_size(vec2(label_width, p.control_height()), egui::Sense::hover());
        let galley = one_line(
            ui.painter(),
            text,
            egui::FontId::proportional(p.body()),
            p.muted,
            label_width,
        );
        ui.painter().galley(
            pos2(rect.left(), rect.center().y - galley.size().y / 2.0),
            galley,
            p.muted,
        );
        add(ui)
    })
    .inner
}

/// Paint the mark of a database product: its letters on its colour. The
/// letters are dark on a light colour and light on a dark one.
pub fn paint_badge(painter: &egui::Painter, rect: Rect, badge: baza_core::Badge, enabled: bool) {
    let [r, g, b] = badge.color;
    let fill = dimmed(Color32::from_rgb(r, g, b), enabled);
    let luminance = 0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b);
    let ink = if luminance > 150.0 {
        Color32::from_rgb(20, 24, 31)
    } else {
        Color32::WHITE
    };
    let side = rect.width().min(rect.height());
    let rect = Rect::from_center_size(rect.center(), vec2(side, side));
    painter.rect_filled(rect, side * 0.24, fill);
    let size = if badge.text.chars().count() > 1 {
        side * 0.52
    } else {
        side * 0.62
    };
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        badge.text,
        theme::strong(size),
        dimmed(ink, enabled),
    );
}

/// The mark of a database product as a widget of `size` points.
pub fn product_badge(ui: &mut egui::Ui, badge: baza_core::Badge, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(vec2(size, size), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        paint_badge(ui.painter(), rect, badge, ui.is_enabled());
    }
    response
}

/// Choose a database product: a field that shows the current one and opens a
/// list of every product of the registry, grouped, with a search field that
/// has the keyboard. Arrow keys move through the list and Enter chooses.
/// Returns the product that was chosen in this frame.
pub fn product_picker(
    ui: &mut egui::Ui,
    p: &Palette,
    en: bool,
    width: f32,
    current: (baza_core::Engine, &'static baza_core::ProductSpec),
) -> Option<(baza_core::Engine, &'static baza_core::ProductSpec)> {
    use baza_core::{Engine, Group, engine::localized};
    let id = ui.make_persistent_id("product_picker");
    let popup_id = id.with("popup");
    let search_id = id.with("search");
    let cursor_id = id.with("cursor");
    let focus_id = id.with("focus");
    let height = p.control_height();
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), egui::Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), popup_id);
    if ui.is_rect_visible(rect) {
        let enabled = ui.is_enabled();
        let border = if open || response.hovered() || response.has_focus() {
            p.accent
        } else {
            p.line
        };
        ui.painter().rect(
            rect,
            4.0,
            p.bg,
            Stroke::new(1.0, dimmed(border, enabled)),
            egui::StrokeKind::Inside,
        );
        let mark = Rect::from_center_size(
            pos2(rect.left() + 9.0 + 9.0, rect.center().y),
            vec2(18.0, 18.0),
        );
        paint_badge(ui.painter(), mark, current.1.badge, enabled);
        let galley = one_line(
            ui.painter(),
            current.1.label,
            egui::FontId::proportional(p.body()),
            dimmed(p.text, enabled),
            width - 9.0 - 18.0 - 8.0 - 30.0,
        );
        ui.painter().galley(
            pos2(mark.right() + 8.0, rect.center().y - galley.size().y / 2.0),
            galley,
            p.text,
        );
        paint(
            ui.painter(),
            Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), vec2(12.0, 12.0)),
            Icon::ChevronDown,
            dimmed(p.muted, enabled),
        );
    }
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::ComboBox, ui.is_enabled(), current.1.label)
    });
    if response.clicked() && !open {
        // A fresh list every time: no search text, the current product marked.
        ui.data_mut(|data| {
            data.insert_temp(search_id, String::new());
            data.insert_temp(cursor_id, usize::MAX);
            data.insert_temp(focus_id, true);
        });
    }
    let mut chosen = None;
    egui::Popup::from_toggle_button_response(&response)
        .id(popup_id)
        .width(width)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .frame(
            egui::Frame::NONE
                .fill(p.side)
                .stroke(Stroke::new(1.0, p.line))
                .corner_radius(6)
                .inner_margin(egui::Margin::same(6))
                .shadow(egui::Shadow {
                    offset: [0, 8],
                    blur: 24,
                    spread: 0,
                    color: Color32::from_black_alpha(110),
                }),
        )
        .show(|ui| {
            ui.set_width(width - 12.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            let mut search: String = ui.data(|data| data.get_temp(search_id)).unwrap_or_default();
            let field = ui.add(
                field(p, &mut search)
                    .desired_width(f32::INFINITY)
                    .hint_text(if en {
                        "Search: name, vendor, cloud…"
                    } else {
                        "Szukaj: nazwa, dostawca, chmura…"
                    }),
            );
            // The keyboard goes to the search field once, when the list opens.
            if ui.data(|data| data.get_temp::<bool>(focus_id)) == Some(true) {
                field.request_focus();
                ui.data_mut(|data| data.insert_temp(focus_id, false));
            }
            let typed = field.changed();
            ui.data_mut(|data| data.insert_temp(search_id, search.clone()));
            // What the list holds, in the order it is shown.
            let listed: Vec<(Group, Engine, &'static baza_core::ProductSpec)> = Group::ALL
                .into_iter()
                .flat_map(|group| {
                    let search = search.clone();
                    Engine::all().flat_map(move |engine| {
                        let search = search.clone();
                        engine
                            .spec()
                            .products
                            .iter()
                            .filter(move |product| {
                                product.group == group
                                    && product.listed
                                    && (product.matches(&search)
                                        || localized(group.label(), en)
                                            .to_lowercase()
                                            .contains(search.trim().to_lowercase().as_str()))
                            })
                            .map(move |product| (group, engine, product))
                    })
                })
                .collect();
            let is_current = |engine: Engine, product: &baza_core::ProductSpec| {
                engine == current.0 && std::ptr::eq(product, current.1)
            };
            let mut cursor: usize = ui
                .data(|data| data.get_temp(cursor_id))
                .unwrap_or(usize::MAX);
            if typed {
                cursor = 0;
            } else if cursor == usize::MAX {
                cursor = listed
                    .iter()
                    .position(|(_, engine, product)| is_current(*engine, product))
                    .unwrap_or(0);
            }
            let mut moved = typed;
            if !listed.is_empty() {
                let (down, up, enter) = ui.input(|input| {
                    (
                        input.key_pressed(egui::Key::ArrowDown),
                        input.key_pressed(egui::Key::ArrowUp),
                        input.key_pressed(egui::Key::Enter),
                    )
                });
                if down {
                    cursor = (cursor + 1).min(listed.len() - 1);
                    moved = true;
                }
                if up {
                    cursor = cursor.saturating_sub(1);
                    moved = true;
                }
                cursor = cursor.min(listed.len() - 1);
                if enter {
                    let (_, engine, product) = listed[cursor];
                    chosen = Some((engine, product));
                }
            }
            ui.data_mut(|data| data.insert_temp(cursor_id, cursor));
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .max_height(286.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    if listed.is_empty() {
                        ui.add_space(6.0);
                        help(
                            ui,
                            p,
                            if en {
                                "No database product matches this text."
                            } else {
                                "Żaden produkt bazodanowy nie pasuje do tego tekstu."
                            },
                        );
                        ui.add_space(6.0);
                    }
                    let mut group_shown = None;
                    for (index, (group, engine, product)) in listed.iter().enumerate() {
                        if group_shown != Some(*group) {
                            group_shown = Some(*group);
                            ui.add_space(if index == 0 { 2.0 } else { 8.0 });
                            ui.horizontal(|ui| {
                                ui.add_space(6.0);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(
                                            localized(group.label(), en).to_uppercase(),
                                        )
                                        .font(theme::medium(p.caption()))
                                        .color(p.muted),
                                    )
                                    .selectable(false),
                                );
                            });
                            ui.add_space(2.0);
                        }
                        let (row, row_response) = ui.allocate_exact_size(
                            vec2(ui.available_width(), (height - 2.0).max(26.0)),
                            egui::Sense::click(),
                        );
                        let selected = is_current(*engine, product);
                        if ui.is_rect_visible(row) {
                            if index == cursor || row_response.hovered() {
                                ui.painter().rect_filled(row, 4.0, p.raised);
                            }
                            let mark = Rect::from_center_size(
                                pos2(row.left() + 6.0 + 9.0, row.center().y),
                                vec2(18.0, 18.0),
                            );
                            paint_badge(ui.painter(), mark, product.badge, true);
                            let galley = one_line(
                                ui.painter(),
                                product.label,
                                egui::FontId::proportional(p.body()),
                                p.text,
                                row.width() - 6.0 - 18.0 - 8.0 - 30.0,
                            );
                            ui.painter().galley(
                                pos2(mark.right() + 8.0, row.center().y - galley.size().y / 2.0),
                                galley,
                                p.text,
                            );
                            if selected {
                                paint(
                                    ui.painter(),
                                    Rect::from_center_size(
                                        pos2(row.right() - 14.0, row.center().y),
                                        vec2(13.0, 13.0),
                                    ),
                                    Icon::Check,
                                    p.accent,
                                );
                            }
                        }
                        row_response.widget_info(|| {
                            egui::WidgetInfo::selected(
                                egui::WidgetType::SelectableLabel,
                                true,
                                selected,
                                product.label,
                            )
                        });
                        if index == cursor && moved {
                            ui.scroll_to_rect(row, None);
                        }
                        if row_response.clicked() {
                            chosen = Some((*engine, *product));
                        }
                    }
                });
        });
    if chosen.is_some() {
        egui::Popup::close_id(ui.ctx(), popup_id);
    }
    chosen
}

/// A button of the ribbon at the top of the window: an icon above its
/// label. `compact` leaves the label out.
pub fn ribbon_button(
    ui: &mut egui::Ui,
    p: &Palette,
    icon: Icon,
    label: &str,
    active: bool,
    compact: bool,
    height: f32,
) -> Response {
    let font = egui::FontId::proportional(p.small());
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font, Color32::PLACEHOLDER);
    let side = (p.icon() * 1.45).round();
    let width = if compact {
        side + 18.0
    } else {
        (galley.size().x + 16.0).max(side + 26.0)
    };
    let (rect, r) = ui.allocate_exact_size(vec2(width.round(), height), egui::Sense::click());
    r.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, ui.is_enabled(), active, label)
    });
    let enabled = ui.is_enabled();
    let painter = ui.painter();
    let body = rect.shrink2(vec2(2.0, 4.0));
    if active {
        painter.rect_filled(body, 5.0, theme::blend(p.side, p.accent, 0.22));
        painter.rect_stroke(
            body,
            5.0,
            Stroke::new(1.0, theme::blend(p.side, p.accent, 0.55)),
            egui::StrokeKind::Inside,
        );
    } else if enabled && r.hovered() {
        painter.rect_filled(body, 5.0, p.raised);
    }
    if r.has_focus() {
        painter.rect_stroke(
            body,
            5.0,
            Stroke::new(1.0, p.accent),
            egui::StrokeKind::Inside,
        );
    }
    let color = dimmed(
        if active {
            p.accent
        } else if r.hovered() {
            p.text
        } else {
            theme::blend(p.muted, p.text, 0.45)
        },
        enabled,
    );
    let icon_center = if compact {
        rect.center()
    } else {
        pos2(rect.center().x, rect.top() + 6.0 + side / 2.0)
    };
    paint(
        painter,
        Rect::from_center_size(icon_center, vec2(side, side)),
        icon,
        color,
    );
    if !compact {
        let text_color = dimmed(if active { p.text } else { p.muted }, enabled);
        painter.galley(
            pos2(
                rect.center().x - galley.size().x / 2.0,
                rect.bottom() - 7.0 - galley.size().y,
            ),
            galley,
            text_color,
        );
    }
    let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
    if compact { r.on_hover_text(label) } else { r }
}

/// A tab of the work area that can be closed: an icon, its title and a
/// cross. Returns the tab and whether the cross was clicked.
pub fn closable_tab(
    ui: &mut egui::Ui,
    p: &Palette,
    icon: Icon,
    title: &str,
    active: bool,
    closable: bool,
    height: f32,
) -> (Response, bool) {
    let font = egui::FontId::proportional(p.control() - 0.5);
    let galley = ui
        .painter()
        .layout_no_wrap(title.to_owned(), font, Color32::PLACEHOLDER);
    let side = p.icon();
    let pad = 12.0;
    let close_side = 16.0;
    let width =
        pad + side + 7.0 + galley.size().x + if closable { 8.0 + close_side } else { 0.0 } + pad;
    let (rect, r) = ui.allocate_exact_size(vec2(width.round(), height), egui::Sense::click());
    r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, true, active, title));
    let painter = ui.painter();
    if active {
        painter.rect_filled(rect, 0.0, p.bg);
        painter.rect_filled(
            Rect::from_min_size(rect.min, vec2(rect.width(), 2.0)),
            0.0,
            p.accent,
        );
    } else if r.hovered() {
        painter.rect_filled(rect, 0.0, theme::blend(p.side, p.raised, 0.7));
    }
    painter.vline(rect.right() - 0.5, rect.y_range(), Stroke::new(1.0, p.line));
    let color = if active || r.hovered() {
        p.text
    } else {
        p.muted
    };
    let middle = rect.center().y + if active { 1.0 } else { 0.0 };
    let mut x = rect.left() + pad;
    paint(
        painter,
        Rect::from_center_size(pos2(x + side / 2.0, middle), vec2(side, side)),
        icon,
        if active { p.accent } else { color },
    );
    x += side + 7.0;
    painter.galley(pos2(x, middle - galley.size().y / 2.0), galley, color);
    let mut closed = false;
    if closable {
        let close_rect = Rect::from_center_size(
            pos2(rect.right() - pad - close_side / 2.0, middle),
            vec2(close_side, close_side),
        );
        let close = ui.interact(close_rect, r.id.with("close"), egui::Sense::click());
        if close.hovered() {
            ui.painter().rect_filled(close_rect, 3.0, p.raised);
        }
        paint(
            ui.painter(),
            close_rect.shrink(3.0),
            Icon::Close,
            if close.hovered() { p.text } else { p.muted },
        );
        closed = close.clicked();
    }
    (r.on_hover_cursor(egui::CursorIcon::PointingHand), closed)
}
