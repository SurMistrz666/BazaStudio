//! Design tokens of the workspace. The values mirror the interactive demo on
//! the product portal, so the application and the demo read as one design.
use eframe::egui::{self, Color32};

/// Theme identifiers stored in the preferences, with the names shown in the
/// "Wygląd" selector.
pub const THEMES: [(&str, &str); 6] = [
    ("vscode", "VS Code"),
    ("graphite", "Graphite"),
    ("midnight", "Midnight"),
    ("oled", "OLED"),
    ("amethyst", "Amethyst"),
    ("light", "Light"),
];

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// Brand cyan: the logo mark and the status dot.
pub const BRAND: Color32 = hex(0x6cd5ef);
/// Primary action ("Uruchom", "Zapisz"): the same blue in every theme.
pub const PRIMARY: Color32 = hex(0x1d6ca1);
pub const PRIMARY_HOVER: Color32 = hex(0x2d91c3);
pub const PRIMARY_BORDER: Color32 = hex(0x369acb);
/// Destructive action.
pub const DANGER: Color32 = hex(0x492c32);
pub const DANGER_HOVER: Color32 = hex(0x5c343b);
pub const DANGER_BORDER: Color32 = hex(0x97595b);
/// Text of an error message.
pub const ERROR_TEXT: Color32 = hex(0xffc4ba);
pub const POSITIVE: Color32 = hex(0x8ad0a6);
pub const WARNING: Color32 = hex(0xe8ba78);

/// Accent stored by default in the preferences. It means "use the accent of
/// the selected theme"; any other value is the user's own colour.
pub const DEFAULT_ACCENT: [u8; 3] = [57, 118, 223];

/// Font family of medium-weight interface text (buttons, grid headers).
pub const MEDIUM: &str = "ui-medium";
/// Font family of headings.
pub const STRONG: &str = "ui-strong";

pub fn medium(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name(MEDIUM.into()))
}
pub fn strong(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name(STRONG.into()))
}

/// Resolved colours and text sizes of one theme.
#[derive(Clone, Copy)]
pub struct Palette {
    /// Content background, also the background of inputs.
    pub bg: Color32,
    /// Bars, the side panel, grid headers and the status bar.
    pub side: Color32,
    /// Raised surfaces: selected rows, hover, secondary buttons.
    pub raised: Color32,
    /// One pixel lines and borders.
    pub line: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    /// Hover of a secondary button.
    pub raised_hover: Color32,
    /// Subtle zebra of the result grid.
    pub stripe: Color32,
    pub keyword: Color32,
    pub string: Color32,
    pub comment: Color32,
    pub number: Color32,
    /// Base interface text size in points.
    pub font: f32,
    /// More room in rows and controls.
    pub comfortable: bool,
    /// A light background: dark text, light widgets.
    pub light: bool,
    /// Text of an error message, readable on this background.
    pub error: Color32,
    /// "Connected", "saved": readable on this background.
    pub positive: Color32,
}

impl Palette {
    /// Regular interface text: tree rows, grid cells, form fields.
    pub fn body(&self) -> f32 {
        self.font + 0.5
    }
    /// Section tabs, buttons and the top bar.
    pub fn control(&self) -> f32 {
        self.font
    }
    /// Sub-labels, counters and the status bar.
    pub fn small(&self) -> f32 {
        (self.font - 1.5).max(9.5)
    }
    /// Upper-case captions.
    pub fn caption(&self) -> f32 {
        (self.font - 2.0).max(9.0)
    }
    /// Heading of a section view or a dialog.
    pub fn heading(&self) -> f32 {
        self.font + 4.0
    }
    /// SQL and other code.
    pub fn code(&self) -> f32 {
        self.font + 1.0
    }
    /// Side of the icon next to a line of interface text.
    pub fn icon(&self) -> f32 {
        (self.font * 1.17).round()
    }
    /// Height of buttons, inputs and selects.
    pub fn control_height(&self) -> f32 {
        (self.font * 2.4).round().max(26.0) + if self.comfortable { 4.0 } else { 0.0 }
    }
    /// Height of a tree row or a grid row.
    pub fn row_height(&self) -> f32 {
        (self.font * 2.3).round().max(24.0) + if self.comfortable { 4.0 } else { 0.0 }
    }
}

/// Base tokens of a theme: background, side, raised, line, text, muted, accent
/// and the four syntax colours of the SQL editor.
fn tokens(name: &str) -> [u32; 11] {
    match name {
        "graphite" => [
            0x22262c, 0x1d2127, 0x2c3139, 0x414954, 0xe4e9ef, 0xa9b3c1, 0x95bddb, 0x9cc4e4,
            0xb6d992, 0x8894a3, 0xe8b180,
        ],
        "midnight" => [
            0x111b29, 0x152235, 0x1c2c42, 0x344864, 0xd8e6f8, 0x93abc7, 0x70b9f8, 0x7fc0f9,
            0xa6e3a1, 0x7f96b3, 0xfab387,
        ],
        "oled" | "amoled" => [
            0x080909, 0x111313, 0x191c1c, 0x343a3a, 0xe8f0ec, 0xa5b7b0, 0x86d5b6, 0x86d5b6,
            0xd8c690, 0x7d8f88, 0xffc092,
        ],
        // White, as the light theme of VS Code, for those who like it.
        "light" => [
            0xffffff, 0xf3f3f3, 0xe8e8e8, 0xd4d4d4, 0x1f1f1f, 0x616161, 0x005fb8, 0x0000ff,
            0xa31515, 0x008000, 0x098658,
        ],
        "amethyst" | "nord" => [
            0x211b2b, 0x292233, 0x352b43, 0x4b3e5d, 0xeee3fc, 0xb4a4c9, 0xc6a4f8, 0xc6a4f8,
            0xb7d6ae, 0x9486aa, 0xf0bd92,
        ],
        _ => [
            0x1e1e1e, 0x252526, 0x2d2d30, 0x3b3b3e, 0xd9d9dd, 0x9a9aab, 0x4daafc, 0x569cd6,
            0xce9178, 0x7f9d6c, 0xb5cea8,
        ],
    }
}

/// Palette of a theme with the default text size and the theme's own accent.
pub fn base(name: &str) -> Palette {
    let c = tokens(name).map(hex);
    derive(c, c[6], 12.0, false)
}

fn derive(c: [Color32; 11], accent: Color32, font: f32, comfortable: bool) -> Palette {
    let light = c[0].r() as u32 + c[0].g() as u32 + c[0].b() as u32 > 3 * 160;
    Palette {
        light,
        error: if light { hex(0xb3261e) } else { ERROR_TEXT },
        positive: if light { hex(0x1e7b45) } else { POSITIVE },
        bg: c[0],
        side: c[1],
        raised: c[2],
        line: c[3],
        text: c[4],
        muted: c[5],
        accent,
        raised_hover: blend(c[2], c[4], 0.08),
        stripe: blend(c[0], accent, 0.07),
        keyword: c[7],
        string: c[8],
        comment: c[9],
        number: c[10],
        font,
        comfortable,
    }
}

/// Palette for the saved preferences: theme, optional own accent, text size.
pub fn palette(prefs: &baza_core::Preferences) -> Palette {
    let c = tokens(&prefs.theme).map(hex);
    let accent = if prefs.accent == DEFAULT_ACCENT {
        c[6]
    } else {
        Color32::from_rgb(prefs.accent[0], prefs.accent[1], prefs.accent[2])
    };
    derive(
        c,
        accent,
        prefs.font_size.clamp(10.0, 24.0),
        prefs.density == "comfortable",
    )
}

pub fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let c = |a: u8, b: u8| ((a as f32) * (1.0 - t) + (b as f32) * t).round() as u8;
    Color32::from_rgb(c(a.r(), b.r()), c(a.g(), b.g()), c(a.b(), b.b()))
}

/// Apply the palette to every built-in egui widget.
fn apply(ctx: &egui::Context, p: &Palette) {
    let mut v = if p.light {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };
    let line = egui::Stroke::new(1.0, p.line);
    let radius = egui::CornerRadius::same(5);
    v.panel_fill = p.bg;
    v.window_fill = p.bg;
    v.window_stroke = line;
    v.window_corner_radius = egui::CornerRadius::same(8);
    v.menu_corner_radius = egui::CornerRadius::same(6);
    v.window_shadow = egui::Shadow {
        offset: [0, 14],
        blur: 40,
        spread: 0,
        color: Color32::from_black_alpha(if p.light { 40 } else { 110 }),
    };
    v.popup_shadow = egui::Shadow {
        offset: [0, 8],
        blur: 22,
        spread: 0,
        color: Color32::from_black_alpha(if p.light { 30 } else { 90 }),
    };
    v.extreme_bg_color = p.bg;
    v.text_edit_bg_color = Some(p.bg);
    v.code_bg_color = p.side;
    v.faint_bg_color = p.stripe;
    v.striped = false;
    v.hyperlink_color = p.accent;
    v.warn_fg_color = if p.light { hex(0x8a5a00) } else { WARNING };
    v.error_fg_color = p.error;
    v.override_text_color = None;
    v.weak_text_color = Some(p.muted);
    v.selection.bg_fill = blend(p.bg, p.accent, 0.30);
    v.selection.stroke = egui::Stroke::new(1.0, p.accent);
    v.indent_has_left_vline = false;
    v.collapsing_header_frame = false;
    v.slider_trailing_fill = true;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = p.bg;
    w.noninteractive.weak_bg_fill = p.bg;
    w.noninteractive.bg_stroke = line;
    w.noninteractive.fg_stroke = egui::Stroke::new(1.0, p.text);
    w.noninteractive.corner_radius = radius;

    w.inactive.bg_fill = p.raised;
    w.inactive.weak_bg_fill = p.raised;
    w.inactive.bg_stroke = line;
    w.inactive.fg_stroke = egui::Stroke::new(1.0, p.text);
    w.inactive.corner_radius = radius;
    w.inactive.expansion = 0.0;

    w.hovered.bg_fill = p.raised_hover;
    w.hovered.weak_bg_fill = p.raised_hover;
    w.hovered.bg_stroke = egui::Stroke::new(1.0, blend(p.line, p.text, 0.28));
    w.hovered.fg_stroke = egui::Stroke::new(1.0, p.text);
    w.hovered.corner_radius = radius;
    w.hovered.expansion = 0.0;

    w.active.bg_fill = blend(p.raised, p.accent, 0.28);
    w.active.weak_bg_fill = blend(p.raised, p.accent, 0.28);
    w.active.bg_stroke = egui::Stroke::new(1.0, p.accent);
    w.active.fg_stroke = egui::Stroke::new(1.0, p.text);
    w.active.corner_radius = radius;
    w.active.expansion = 0.0;

    w.open.bg_fill = p.raised;
    w.open.weak_bg_fill = p.raised;
    w.open.bg_stroke = egui::Stroke::new(1.0, p.accent);
    w.open.fg_stroke = egui::Stroke::new(1.0, p.text);
    w.open.corner_radius = radius;
    w.open.expansion = 0.0;

    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals(v);
    let mut style = (*ctx.style_of(egui::Theme::Dark)).clone();
    style.spacing.item_spacing = egui::vec2(8.0, if p.comfortable { 8.0 } else { 6.0 });
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.interact_size = egui::vec2(28.0, p.control_height());
    style.spacing.menu_margin = egui::Margin::same(6);
    style.spacing.window_margin = egui::Margin::same(0);
    style.spacing.combo_width = 120.0;
    style.spacing.icon_width = 15.0;
    style.spacing.icon_width_inner = 9.0;
    style.spacing.icon_spacing = 7.0;
    style.spacing.indent = 14.0;
    style.spacing.scroll = egui::style::ScrollStyle::thin();
    for (text_style, font) in [
        (egui::TextStyle::Body, egui::FontId::proportional(p.body())),
        (
            egui::TextStyle::Button,
            egui::FontId::proportional(p.control()),
        ),
        (
            egui::TextStyle::Small,
            egui::FontId::proportional(p.small()),
        ),
        (egui::TextStyle::Heading, strong(p.heading())),
        (
            egui::TextStyle::Monospace,
            egui::FontId::monospace(p.code()),
        ),
    ] {
        style.text_styles.insert(text_style, font);
    }
    ctx.set_style_of(egui::Theme::Dark, style);
}

/// The two subset files of one Inter weight: Latin, then Latin Extended, which
/// carries the Polish letters.
macro_rules! inter {
    ($weight:literal) => {
        [
            include_bytes!(concat!(
                "../../../assets/fonts/Inter-",
                $weight,
                "-latin.ttf"
            ))
            .as_slice(),
            include_bytes!(concat!(
                "../../../assets/fonts/Inter-",
                $weight,
                "-latin-ext.ttf"
            ))
            .as_slice(),
        ]
    };
}

/// Install the interface typeface.
///
/// The interface uses Inter, the typeface of the product portal, embedded in
/// three weights (see `native/assets/fonts`). The system interface font and
/// the fonts bundled with egui stay behind it as fallbacks for glyphs Inter
/// lacks, such as arrows, symbols and other scripts. Code keeps the system
/// monospace font, as on the portal.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let ui_paths: &[&str] = if cfg!(windows) {
        &["C:/Windows/Fonts/segoeui.ttf"]
    } else {
        &[
            "/System/Library/Fonts/SFNS.ttf",
            "/System/Library/Fonts/Supplemental/Arial.ttf",
        ]
    };
    let mono_paths: &[&str] = if cfg!(windows) {
        &["C:/Windows/Fonts/consola.ttf"]
    } else {
        &[
            "/System/Library/Fonts/Menlo.ttc",
            "/System/Library/Fonts/Supplemental/Courier New.ttf",
        ]
    };
    let read = |paths: &[&str]| {
        paths.iter().find_map(|path| {
            std::fs::read(path)
                .ok()
                .filter(|bytes| bytes.len() <= 10 * 1024 * 1024)
        })
    };
    for (name, family, bytes) in [
        ("system-ui", egui::FontFamily::Proportional, read(ui_paths)),
        ("system-code", egui::FontFamily::Monospace, read(mono_paths)),
    ] {
        if let Some(bytes) = bytes {
            fonts.font_data.insert(
                name.into(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            fonts
                .families
                .entry(family)
                .or_default()
                .insert(0, name.into());
        }
    }
    // System font first, then the fonts bundled with egui.
    let fallback = fonts
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    for (family, weight, files) in [
        (egui::FontFamily::Proportional, "regular", inter!("Regular")),
        (
            egui::FontFamily::Name(MEDIUM.into()),
            "medium",
            inter!("Medium"),
        ),
        (
            egui::FontFamily::Name(STRONG.into()),
            "semibold",
            inter!("SemiBold"),
        ),
    ] {
        let mut list = Vec::with_capacity(fallback.len() + 2);
        for (subset, bytes) in ["latin", "latin-ext"].into_iter().zip(files) {
            let name = format!("inter-{weight}-{subset}");
            fonts.font_data.insert(
                name.clone(),
                std::sync::Arc::new(egui::FontData::from_static(bytes)),
            );
            list.push(name);
        }
        list.extend(fallback.iter().cloned());
        fonts.families.insert(family, list);
    }
    ctx.set_fonts(fonts);
}

pub fn apply_preferences(ctx: &egui::Context, prefs: &baza_core::Preferences) {
    apply(ctx, &palette(prefs));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seven tokens of every theme are exactly the ones of the portal demo.
    #[test]
    fn themes_use_the_portal_demo_tokens() {
        let expected: [(&str, [u32; 7]); 6] = [
            (
                "vscode",
                [
                    0x1e1e1e, 0x252526, 0x2d2d30, 0x3b3b3e, 0xd9d9dd, 0x9a9aab, 0x4daafc,
                ],
            ),
            (
                "graphite",
                [
                    0x22262c, 0x1d2127, 0x2c3139, 0x414954, 0xe4e9ef, 0xa9b3c1, 0x95bddb,
                ],
            ),
            (
                "midnight",
                [
                    0x111b29, 0x152235, 0x1c2c42, 0x344864, 0xd8e6f8, 0x93abc7, 0x70b9f8,
                ],
            ),
            (
                "oled",
                [
                    0x080909, 0x111313, 0x191c1c, 0x343a3a, 0xe8f0ec, 0xa5b7b0, 0x86d5b6,
                ],
            ),
            (
                "amethyst",
                [
                    0x211b2b, 0x292233, 0x352b43, 0x4b3e5d, 0xeee3fc, 0xb4a4c9, 0xc6a4f8,
                ],
            ),
            (
                "light",
                [
                    0xffffff, 0xf3f3f3, 0xe8e8e8, 0xd4d4d4, 0x1f1f1f, 0x616161, 0x005fb8,
                ],
            ),
        ];
        assert_eq!(THEMES.map(|(id, _)| id), expected.map(|(id, _)| id));
        for (id, values) in expected {
            let p = base(id);
            assert_eq!(
                [p.bg, p.side, p.raised, p.line, p.text, p.muted, p.accent],
                values.map(hex),
                "{id}"
            );
        }
    }

    #[test]
    fn default_accent_follows_the_theme_and_an_own_accent_wins() {
        let mut prefs = baza_core::Preferences {
            theme: "amethyst".into(),
            ..Default::default()
        };
        assert_eq!(prefs.accent, DEFAULT_ACCENT);
        assert_eq!(palette(&prefs).accent, hex(0xc6a4f8));
        prefs.accent = [10, 200, 30];
        assert_eq!(palette(&prefs).accent, Color32::from_rgb(10, 200, 30));
        // Unknown names fall back to the default theme instead of failing.
        prefs.theme = "unknown".into();
        assert_eq!(palette(&prefs).bg, hex(0x1e1e1e));
    }
}
