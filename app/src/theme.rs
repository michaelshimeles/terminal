//! Look and feel: a native macOS-style chrome (SF Pro, rounded cards, system accent,
//! light/dark following the system) around Bloomberg-dense data (SF Mono numbers, amber
//! tickers, green/red moves).
//!
//! Colours come from the current [`Palette`], selected each frame from the system
//! appearance and accent colour; accessors like [`amber`] read it.

use std::cell::Cell;

use crate::prefs::{MonoFont, UiFont};
use egui::{Color32, FontData, FontDefinitions, FontFamily, FontId, Shadow, Stroke, TextStyle, Theme, Visuals};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// Window background behind cards (shows vibrancy on macOS when translucent).
    pub window: Color32,
    /// Sidebar fill; translucent so the window's vibrancy shows through on macOS.
    pub sidebar: Color32,
    /// Card (pane) background — data lives here.
    pub card: Color32,
    pub card_header: Color32,
    pub border: Color32,
    pub grid: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub amber: Color32,
    pub amber_dim: Color32,
    pub up: Color32,
    pub down: Color32,
    pub candle_up: Color32,
    pub candle_down: Color32,
    pub cyan: Color32,
    pub control: Color32,
    pub control_hover: Color32,
    pub accent: Color32,
    pub series: [Color32; 6],
}

impl Palette {
    /// The built-in Terminal Dark palette.
    pub fn dark(accent: Color32) -> Self {
        crate::themes::terminal_dark().palette(accent)
    }
}

/// macOS default (blue) accent, used when the system does not report one.
pub const DEFAULT_ACCENT: Color32 = Color32::from_rgb(10, 132, 255);

thread_local! {
    static CURRENT: Cell<Palette> = Cell::new(Palette::dark(DEFAULT_ACCENT));
    static SCALE: Cell<f32> = const { Cell::new(1.0) };
    static DENSITY: Cell<f32> = const { Cell::new(1.0) };
    static UI_MONO: Cell<bool> = const { Cell::new(false) };
    static FONTS: Cell<Option<MonoFont>> = const { Cell::new(None) };
    /// Fonts fetched after start-up (the browser build's emoji fallbacks).
    #[cfg(target_arch = "wasm32")]
    static LATE_FONTS: std::cell::RefCell<Vec<(String, std::sync::Arc<FontData>)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Spacing multiplier from the density preference (compact < 1 < comfortable).
pub fn density() -> f32 {
    DENSITY.with(Cell::get)
}

/// A layout size in points scaled by density.
pub fn dp(points: f32) -> f32 {
    (points * density()).round()
}

pub fn palette() -> Palette {
    CURRENT.with(Cell::get)
}

pub fn set_palette(p: Palette) {
    CURRENT.with(|c| c.set(p));
}

pub fn window() -> Color32 {
    palette().window
}
pub fn sidebar() -> Color32 {
    palette().sidebar
}
pub fn card() -> Color32 {
    palette().card
}
pub fn header() -> Color32 {
    palette().card_header
}
pub fn border() -> Color32 {
    palette().border
}
pub fn grid() -> Color32 {
    palette().grid
}
pub fn text() -> Color32 {
    palette().text
}
pub fn muted() -> Color32 {
    palette().muted
}
pub fn amber() -> Color32 {
    palette().amber
}
pub fn amber_dim() -> Color32 {
    palette().amber_dim
}
pub fn up() -> Color32 {
    palette().up
}
pub fn candle_up() -> Color32 {
    palette().candle_up
}

pub fn candle_down() -> Color32 {
    palette().candle_down
}

pub fn down() -> Color32 {
    palette().down
}
pub fn cyan() -> Color32 {
    palette().cyan
}
pub fn accent() -> Color32 {
    palette().accent
}
pub fn control() -> Color32 {
    palette().control
}
pub fn control_hover() -> Color32 {
    palette().control_hover
}
pub fn series(i: usize) -> Color32 {
    palette().series[i % 6]
}
/// Selection/highlight wash derived from the accent colour.
pub fn accent_wash() -> Color32 {
    let a = accent();
    Color32::from_rgba_unmultiplied(a.r(), a.g(), a.b(), if palette().dark { 70 } else { 45 })
}

pub fn dem() -> Color32 {
    if palette().dark { Color32::from_rgb(64, 156, 255) } else { Color32::from_rgb(0, 102, 214) }
}
pub fn rep() -> Color32 {
    if palette().dark { Color32::from_rgb(255, 82, 82) } else { Color32::from_rgb(200, 30, 30) }
}
pub fn ind() -> Color32 {
    if palette().dark { Color32::from_rgb(191, 90, 242) } else { Color32::from_rgb(140, 50, 200) }
}

pub fn party_color(p: tr_congress::Party) -> Color32 {
    match p {
        tr_congress::Party::Democrat => dem(),
        tr_congress::Party::Republican => rep(),
        tr_congress::Party::Independent => ind(),
        tr_congress::Party::Unknown => muted(),
    }
}

pub fn rgba(c: Color32) -> [f32; 4] {
    [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0, c.a() as f32 / 255.0]
}

/// Numbers, tickers and tables (SF Mono on macOS).
pub fn mono(size: f32) -> FontId {
    FontId::new(size * SCALE.with(Cell::get), FontFamily::Monospace)
}

/// Chrome: titles, buttons, labels (SF Pro on macOS).
pub fn ui_font(size: f32) -> FontId {
    let family = if UI_MONO.with(Cell::get) { FontFamily::Monospace } else { FontFamily::Proportional };
    FontId::new(size * SCALE.with(Cell::get), family)
}

/// Corner radii for cards and controls.
pub const CARD_RADIUS: u8 = 10;
pub const CONTROL_RADIUS: u8 = 6;

/// egui's bundled fonts. The browser build starts with only the two text fonts, which keeps
/// the emoji fonts out of the download; they arrive later through [`add_late_font`].
pub fn base_fonts() -> FontDefinitions {
    #[cfg(not(target_arch = "wasm32"))]
    let fonts = FontDefinitions::default();
    #[cfg(target_arch = "wasm32")]
    let fonts = {
        use std::sync::Arc;
        let mut f = FontDefinitions::empty();
        f.font_data.insert("Hack".into(), Arc::new(FontData::from_static(epaint_default_fonts::HACK_REGULAR)));
        f.font_data.insert("Ubuntu-Light".into(), Arc::new(FontData::from_static(epaint_default_fonts::UBUNTU_LIGHT)));
        f.families.insert(FontFamily::Monospace, vec!["Hack".into(), "Ubuntu-Light".into()]);
        f.families.insert(FontFamily::Proportional, vec!["Ubuntu-Light".into()]);
        LATE_FONTS.with_borrow(|late| {
            for (name, data) in late {
                f.font_data.insert(name.clone(), data.clone());
                for family in [FontFamily::Proportional, FontFamily::Monospace] {
                    f.families.entry(family).or_default().push(name.clone());
                }
            }
        });
        f
    };
    fonts
}

/// Add a font fetched after start-up as the last fallback of every family.
#[cfg(target_arch = "wasm32")]
pub fn add_late_font(ctx: &egui::Context, name: &str, data: FontData) {
    LATE_FONTS.with_borrow_mut(|late| late.push((name.to_owned(), std::sync::Arc::new(data))));
    // Reinstall with the current monospace choice (or let the first configure pick it up).
    if let Some(mono) = FONTS.take() {
        install_fonts(ctx, mono);
    }
}

/// Use the system UI fonts when present (read from the OS at runtime, not bundled).
fn install_fonts(ctx: &egui::Context, mono: MonoFont) {
    if FONTS.with(Cell::get) == Some(mono) {
        return;
    }
    FONTS.with(|f| f.set(Some(mono)));
    let mut fonts = base_fonts();
    let ui_paths: &[&str] = &[
        "/System/Library/Fonts/SFNS.ttf",
        "/System/Library/Fonts/SFNSText.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
    ];
    let candidates: &[(&str, FontFamily, &[&str])] =
        &[("system-ui", FontFamily::Proportional, ui_paths), ("system-mono", FontFamily::Monospace, mono.paths())];
    for (name, family, paths) in candidates {
        if let Some(bytes) = paths.iter().find_map(|p| std::fs::read(p).ok()) {
            fonts.font_data.insert((*name).into(), std::sync::Arc::new(FontData::from_owned(bytes)));
            fonts.families.entry(family.clone()).or_default().insert(0, (*name).into());
        }
    }
    ctx.set_fonts(fonts);
}

fn visuals(p: &Palette) -> Visuals {
    let mut v = if p.dark { Visuals::dark() } else { Visuals::light() };
    v.panel_fill = Color32::TRANSPARENT;
    v.window_fill = p.card;
    v.extreme_bg_color = p.card;
    v.faint_bg_color = p.card_header;
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.accent;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_shadow =
        Shadow { offset: [0, 6], blur: 24, spread: 0, color: Color32::from_black_alpha(if p.dark { 110 } else { 40 }) };
    v.popup_shadow =
        Shadow { offset: [0, 4], blur: 16, spread: 0, color: Color32::from_black_alpha(if p.dark { 100 } else { 35 }) };
    v.selection.bg_fill = accent_wash_for(p);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.inactive.bg_fill = p.control;
    v.widgets.inactive.weak_bg_fill = p.control;
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.hovered.bg_fill = p.control_hover;
    v.widgets.hovered.weak_bg_fill = p.control_hover;
    v.widgets.hovered.bg_stroke = Stroke::NONE;
    v.widgets.active.bg_fill = accent_wash_for(p);
    v.widgets.active.weak_bg_fill = accent_wash_for(p);
    v.window_corner_radius = CARD_RADIUS.into();
    v.menu_corner_radius = 8.into();
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CONTROL_RADIUS.into();
    }
    v.text_cursor.stroke = Stroke::new(2.0, p.accent);
    v
}

fn accent_wash_for(p: &Palette) -> Color32 {
    Color32::from_rgba_unmultiplied(p.accent.r(), p.accent.g(), p.accent.b(), if p.dark { 70 } else { 45 })
}

/// One-time setup: fonts and text styles. Visuals are refreshed by [`refresh`].
/// Apply a theme's fonts, text scale and density, and its colours. `system_accent` is
/// used when the theme does not set an accent. Call when the theme changes.
pub fn configure(ctx: &egui::Context, t: &crate::themes::Theme, system_accent: Color32) {
    install_fonts(ctx, t.fonts.mono);
    SCALE.with(|c| c.set(t.fonts.scale.clamp(0.7, 1.6)));
    DENSITY.with(|c| c.set(t.density.factor()));
    UI_MONO.with(|c| c.set(t.fonts.ui == UiFont::Monospace));
    let d = density();
    ctx.all_styles_mut(|s| {
        s.text_styles = [
            (TextStyle::Small, ui_font(10.5)),
            (TextStyle::Body, ui_font(13.0)),
            (TextStyle::Button, ui_font(13.0)),
            (TextStyle::Heading, ui_font(16.0)),
            (TextStyle::Monospace, mono(12.0)),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(6.0 * d, 3.0 * d);
        s.spacing.button_padding = egui::vec2(8.0 * d, 3.0 * d);
        s.spacing.interact_size.y = 20.0 * d;
        s.spacing.menu_margin = egui::Margin::same((6.0 * d) as i8);
        s.animation_time = 0.18;
    });
    // egui's own widgets follow the theme's light/dark base.
    ctx.set_theme(if t.dark { egui::ThemePreference::Dark } else { egui::ThemePreference::Light });
    // Force visuals to be rebuilt with the new colours.
    set_palette(Palette { accent: Color32::TRANSPARENT, ..palette() });
    refresh(ctx, t, system_accent);
}

/// Keep the palette in step with the theme and system accent; cheap, call every frame.
pub fn refresh(ctx: &egui::Context, t: &crate::themes::Theme, system_accent: Color32) {
    let p = t.palette(system_accent);
    if p != palette() {
        let v = visuals(&p);
        ctx.set_visuals_of(Theme::Dark, v.clone());
        ctx.set_visuals_of(Theme::Light, v);
    }
    set_palette(p);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palettes_are_readable() {
        for p in [Palette::dark(DEFAULT_ACCENT), crate::themes::terminal_light().palette(DEFAULT_ACCENT)] {
            let lum = |c: Color32| 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
            // Body text contrasts strongly with the card it sits on.
            assert!((lum(p.text) - lum(p.card)).abs() > 150.0, "{}", p.dark);
            assert!((lum(p.muted) - lum(p.card)).abs() > 60.0);
        }
        set_palette(crate::themes::terminal_light().palette(Color32::RED));
        assert_eq!(accent(), Color32::RED);
        assert!(!palette().dark);
    }
}
