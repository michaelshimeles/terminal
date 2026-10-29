//! Themes: named bundles of colours, fonts and density. Eight are built in; custom themes
//! are TOML (or JSON) files in `<data dir>/themes/`, so they can be shared, hand-edited,
//! imported and exported. A file only needs the fields it changes: anything missing is
//! filled from the built-in dark or light base, chosen by its `dark` flag.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use egui::Color32;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::prefs::{Density, MonoFont, UiFont};
use crate::storage;
use crate::theme::Palette;

/// Selection that follows the OS appearance using the configured dark/light pair.
pub const SYSTEM: &str = "System";

/// A colour written as `#rrggbb` or `#rrggbbaa` (alpha unpremultiplied).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hex(pub Color32);

impl Hex {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Hex(Color32::from_rgb(r, g, b))
    }

    pub fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Hex(Color32::from_rgba_unmultiplied(r, g, b, a))
    }

    pub fn parse(s: &str) -> Option<Self> {
        let h = s.trim().strip_prefix('#').unwrap_or(s.trim());
        if !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
        match h.len() {
            6 => Some(Hex::rgb(byte(0)?, byte(2)?, byte(4)?)),
            8 => Some(Hex::rgba(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
            3 => {
                let n = |i: usize| u8::from_str_radix(&h[i..=i], 16).ok().map(|v| v * 17);
                Some(Hex::rgb(n(0)?, n(1)?, n(2)?))
            }
            _ => None,
        }
    }

    pub fn format(&self) -> String {
        let [r, g, b, a] = self.0.to_srgba_unmultiplied();
        if a == 255 { format!("#{r:02x}{g:02x}{b:02x}") } else { format!("#{r:02x}{g:02x}{b:02x}{a:02x}") }
    }
}

impl Serialize for Hex {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.format())
    }
}

impl<'de> Deserialize<'de> for Hex {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Hex::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid colour {s:?} (use #rrggbb or #rrggbbaa)")))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThemeColors {
    /// Window background behind the panels.
    pub background: Hex,
    /// Sidebar fill (translucent on macOS so vibrancy shows through).
    pub sidebar: Hex,
    /// Panel (card) background, where data lives.
    pub panel: Hex,
    pub panel_header: Hex,
    pub border: Hex,
    pub grid: Hex,
    pub text: Hex,
    pub muted: Hex,
    /// Tickers, the command line and other "terminal amber" highlights.
    pub highlight: Hex,
    pub highlight_dim: Hex,
    /// Secondary data colour (IV, links, info).
    pub info: Hex,
    pub control: Hex,
    pub control_hover: Hex,
    /// Focus rings and selections; absent = the system accent colour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent: Option<Hex>,
    pub up: Hex,
    pub down: Hex,
    pub candle_up: Hex,
    pub candle_down: Hex,
    /// Indicator and comparison line colours.
    pub series: [Hex; 6],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThemeFonts {
    pub ui: UiFont,
    pub mono: MonoFont,
    /// Text size multiplier (0.7–1.6).
    pub scale: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Theme {
    pub name: String,
    pub dark: bool,
    pub colors: ThemeColors,
    pub fonts: ThemeFonts,
    pub density: Density,
    /// Shipped with the app (read-only; editing makes a custom copy).
    #[serde(skip)]
    pub builtin: bool,
    /// File the theme was loaded from (custom themes).
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

impl Theme {
    /// The UI palette, resolving the accent against the system accent colour.
    pub fn palette(&self, system_accent: Color32) -> Palette {
        let c = &self.colors;
        Palette {
            dark: self.dark,
            window: c.background.0,
            sidebar: c.sidebar.0,
            card: c.panel.0,
            card_header: c.panel_header.0,
            border: c.border.0,
            grid: c.grid.0,
            text: c.text.0,
            muted: c.muted.0,
            amber: c.highlight.0,
            amber_dim: c.highlight_dim.0,
            up: c.up.0,
            down: c.down.0,
            candle_up: c.candle_up.0,
            candle_down: c.candle_down.0,
            cyan: c.info.0,
            control: c.control.0,
            control_hover: c.control_hover.0,
            accent: c.accent.map_or(system_accent, |a| a.0),
            series: c.series.map(|s| s.0),
        }
    }

    /// Clamp values a hand-edited file might get wrong.
    pub fn sanitize(&mut self) {
        self.name = self.name.trim().to_string();
        if self.name.is_empty() {
            self.name = "Untitled Theme".into();
        }
        if !self.fonts.scale.is_finite() {
            self.fonts.scale = 1.0;
        }
        self.fonts.scale = self.fonts.scale.clamp(0.7, 1.6);
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("theme serializes")
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("theme serializes")
    }

    /// Parse a theme file, filling fields it omits from the matching built-in base.
    pub fn parse(text: &str, json: bool) -> Result<Theme> {
        let user: serde_json::Value = if json {
            serde_json::from_str(text).context("parsing theme JSON")?
        } else {
            let t: toml::Value = toml::from_str(text).context("parsing theme TOML")?;
            serde_json::to_value(t)?
        };
        if !user.is_object() {
            bail!("a theme file must be a table/object");
        }
        let dark = user.get("dark").and_then(|v| v.as_bool()).unwrap_or(true);
        let mut merged = serde_json::to_value(if dark { terminal_dark() } else { terminal_light() })?;
        merge(&mut merged, user);
        let mut t: Theme = serde_json::from_value(merged).context("invalid theme")?;
        t.builtin = false;
        t.source = None;
        t.sanitize();
        Ok(t)
    }
}

/// Deep-merge `over` into `base` (objects merge key by key; anything else replaces).
fn merge(base: &mut serde_json::Value, over: serde_json::Value) {
    match (base, over) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

fn fonts(ui: UiFont, mono: MonoFont, scale: f32) -> ThemeFonts {
    ThemeFonts { ui, mono, scale }
}

/// The original dark look: macOS-style chrome around near-black data panels.
pub fn terminal_dark() -> Theme {
    Theme {
        name: "Terminal Dark".into(),
        dark: true,
        colors: ThemeColors {
            background: Hex::rgb(30, 30, 32),
            sidebar: Hex::rgba(28, 28, 30, 150),
            panel: Hex::rgb(12, 12, 13),
            panel_header: Hex::rgb(28, 28, 30),
            border: Hex::rgba(255, 255, 255, 22),
            grid: Hex::rgb(34, 34, 36),
            text: Hex::rgb(236, 236, 238),
            muted: Hex::rgb(142, 142, 147),
            highlight: Hex::rgb(255, 176, 0),
            highlight_dim: Hex::rgb(170, 118, 0),
            info: Hex::rgb(100, 210, 255),
            control: Hex::rgba(255, 255, 255, 18),
            control_hover: Hex::rgba(255, 255, 255, 30),
            accent: None,
            up: Hex::rgb(48, 209, 88),
            down: Hex::rgb(255, 69, 58),
            candle_up: Hex::rgb(48, 209, 88),
            candle_down: Hex::rgb(255, 69, 58),
            series: [
                Hex::rgb(10, 132, 255),
                Hex::rgb(255, 55, 95),
                Hex::rgb(255, 214, 10),
                Hex::rgb(100, 210, 255),
                Hex::rgb(200, 200, 205),
                Hex::rgb(255, 159, 10),
            ],
        },
        fonts: fonts(UiFont::System, MonoFont::System, 1.0),
        density: Density::Regular,
        builtin: true,
        source: None,
    }
}

pub fn terminal_light() -> Theme {
    Theme {
        name: "Terminal Light".into(),
        dark: false,
        colors: ThemeColors {
            background: Hex::rgb(236, 236, 238),
            sidebar: Hex::rgba(246, 246, 248, 150),
            panel: Hex::rgb(255, 255, 255),
            panel_header: Hex::rgb(244, 244, 246),
            border: Hex::rgba(0, 0, 0, 26),
            grid: Hex::rgb(234, 234, 237),
            text: Hex::rgb(29, 29, 31),
            muted: Hex::rgb(110, 110, 115),
            highlight: Hex::rgb(178, 104, 0),
            highlight_dim: Hex::rgb(196, 150, 70),
            info: Hex::rgb(0, 122, 204),
            control: Hex::rgba(0, 0, 0, 14),
            control_hover: Hex::rgba(0, 0, 0, 24),
            accent: None,
            up: Hex::rgb(28, 150, 64),
            down: Hex::rgb(215, 40, 40),
            candle_up: Hex::rgb(28, 150, 64),
            candle_down: Hex::rgb(215, 40, 40),
            series: [
                Hex::rgb(0, 122, 255),
                Hex::rgb(214, 0, 120),
                Hex::rgb(196, 140, 0),
                Hex::rgb(0, 150, 170),
                Hex::rgb(90, 90, 96),
                Hex::rgb(220, 110, 0),
            ],
        },
        fonts: fonts(UiFont::System, MonoFont::System, 1.0),
        density: Density::Regular,
        builtin: true,
        source: None,
    }
}

/// Classic terminal: amber monospace on black, compact.
fn bloomberg() -> Theme {
    Theme {
        name: "Bloomberg".into(),
        dark: true,
        colors: ThemeColors {
            background: Hex::rgb(0, 0, 0),
            sidebar: Hex::rgba(10, 8, 2, 220),
            panel: Hex::rgb(0, 0, 0),
            panel_header: Hex::rgb(28, 20, 0),
            border: Hex::rgba(255, 176, 0, 46),
            grid: Hex::rgb(26, 20, 6),
            text: Hex::rgb(255, 176, 0),
            muted: Hex::rgb(160, 112, 24),
            highlight: Hex::rgb(255, 214, 102),
            highlight_dim: Hex::rgb(170, 130, 40),
            info: Hex::rgb(96, 200, 255),
            control: Hex::rgba(255, 176, 0, 22),
            control_hover: Hex::rgba(255, 176, 0, 40),
            accent: Some(Hex::rgb(255, 140, 0)),
            up: Hex::rgb(0, 230, 118),
            down: Hex::rgb(255, 59, 48),
            candle_up: Hex::rgb(0, 230, 118),
            candle_down: Hex::rgb(255, 59, 48),
            series: [
                Hex::rgb(255, 255, 255),
                Hex::rgb(96, 200, 255),
                Hex::rgb(255, 92, 168),
                Hex::rgb(0, 230, 118),
                Hex::rgb(255, 214, 102),
                Hex::rgb(180, 140, 255),
            ],
        },
        fonts: fonts(UiFont::Monospace, MonoFont::System, 1.0),
        density: Density::Compact,
        builtin: true,
        source: None,
    }
}

fn high_contrast() -> Theme {
    Theme {
        name: "High Contrast".into(),
        dark: true,
        colors: ThemeColors {
            background: Hex::rgb(0, 0, 0),
            sidebar: Hex::rgb(0, 0, 0),
            panel: Hex::rgb(0, 0, 0),
            panel_header: Hex::rgb(26, 26, 26),
            border: Hex::rgba(255, 255, 255, 150),
            grid: Hex::rgb(64, 64, 64),
            text: Hex::rgb(255, 255, 255),
            muted: Hex::rgb(200, 200, 200),
            highlight: Hex::rgb(255, 235, 0),
            highlight_dim: Hex::rgb(210, 190, 0),
            info: Hex::rgb(0, 230, 255),
            control: Hex::rgba(255, 255, 255, 40),
            control_hover: Hex::rgba(255, 255, 255, 80),
            accent: Some(Hex::rgb(0, 200, 255)),
            up: Hex::rgb(0, 255, 102),
            down: Hex::rgb(255, 64, 64),
            candle_up: Hex::rgb(0, 255, 102),
            candle_down: Hex::rgb(255, 64, 64),
            series: [
                Hex::rgb(0, 200, 255),
                Hex::rgb(255, 0, 255),
                Hex::rgb(255, 235, 0),
                Hex::rgb(0, 255, 200),
                Hex::rgb(255, 255, 255),
                Hex::rgb(255, 150, 0),
            ],
        },
        fonts: fonts(UiFont::System, MonoFont::System, 1.1),
        density: Density::Comfortable,
        builtin: true,
        source: None,
    }
}

// Solarized by Ethan Schoonover: base and accent tones.
const S_BASE03: Hex = Hex::rgb(0x00, 0x2b, 0x36);
const S_BASE02: Hex = Hex::rgb(0x07, 0x36, 0x42);
const S_BASE00: Hex = Hex::rgb(0x65, 0x7b, 0x83);
const S_BASE0: Hex = Hex::rgb(0x83, 0x94, 0x96);
const S_BASE1: Hex = Hex::rgb(0x93, 0xa1, 0xa1);
const S_BASE2: Hex = Hex::rgb(0xee, 0xe8, 0xd5);
const S_BASE3: Hex = Hex::rgb(0xfd, 0xf6, 0xe3);
const S_YELLOW: Hex = Hex::rgb(0xb5, 0x89, 0x00);
const S_ORANGE: Hex = Hex::rgb(0xcb, 0x4b, 0x16);
const S_RED: Hex = Hex::rgb(0xdc, 0x32, 0x2f);
const S_MAGENTA: Hex = Hex::rgb(0xd3, 0x36, 0x82);
const S_VIOLET: Hex = Hex::rgb(0x6c, 0x71, 0xc4);
const S_BLUE: Hex = Hex::rgb(0x26, 0x8b, 0xd2);
const S_CYAN: Hex = Hex::rgb(0x2a, 0xa1, 0x98);
const S_GREEN: Hex = Hex::rgb(0x85, 0x99, 0x00);

fn solarized_dark() -> Theme {
    Theme {
        name: "Solarized Dark".into(),
        dark: true,
        colors: ThemeColors {
            background: S_BASE02,
            sidebar: Hex::rgba(0x07, 0x36, 0x42, 200),
            panel: S_BASE03,
            panel_header: S_BASE02,
            border: Hex::rgba(0x93, 0xa1, 0xa1, 40),
            grid: Hex::rgb(0x0a, 0x3a, 0x46),
            text: Hex::rgb(0xd0, 0xd8, 0xd8),
            muted: S_BASE0,
            highlight: S_YELLOW,
            highlight_dim: Hex::rgb(0x7d, 0x5f, 0x00),
            info: S_CYAN,
            control: Hex::rgba(0x93, 0xa1, 0xa1, 26),
            control_hover: Hex::rgba(0x93, 0xa1, 0xa1, 44),
            accent: Some(S_BLUE),
            up: S_GREEN,
            down: S_RED,
            candle_up: S_GREEN,
            candle_down: S_RED,
            series: [S_BLUE, S_MAGENTA, S_YELLOW, S_CYAN, S_VIOLET, S_ORANGE],
        },
        fonts: fonts(UiFont::System, MonoFont::System, 1.0),
        density: Density::Regular,
        builtin: true,
        source: None,
    }
}

fn solarized_light() -> Theme {
    Theme {
        name: "Solarized Light".into(),
        dark: false,
        colors: ThemeColors {
            background: S_BASE2,
            sidebar: Hex::rgba(0xee, 0xe8, 0xd5, 200),
            panel: S_BASE3,
            panel_header: S_BASE2,
            border: Hex::rgba(0x58, 0x6e, 0x75, 46),
            grid: Hex::rgb(0xf0, 0xea, 0xd6),
            text: Hex::rgb(0x07, 0x36, 0x42),
            muted: S_BASE00,
            highlight: Hex::rgb(0x9a, 0x6f, 0x00),
            highlight_dim: S_BASE1,
            info: S_CYAN,
            control: Hex::rgba(0x58, 0x6e, 0x75, 22),
            control_hover: Hex::rgba(0x58, 0x6e, 0x75, 38),
            accent: Some(S_BLUE),
            up: S_GREEN,
            down: S_RED,
            candle_up: S_GREEN,
            candle_down: S_RED,
            series: [S_BLUE, S_MAGENTA, S_YELLOW, S_CYAN, S_VIOLET, S_ORANGE],
        },
        fonts: fonts(UiFont::System, MonoFont::System, 1.0),
        density: Density::Regular,
        builtin: true,
        source: None,
    }
}

/// Nord (arcticicestudio): polar night, snow storm, frost and aurora.
fn nord() -> Theme {
    Theme {
        name: "Nord".into(),
        dark: true,
        colors: ThemeColors {
            background: Hex::rgb(0x2e, 0x34, 0x40),
            sidebar: Hex::rgba(0x3b, 0x42, 0x52, 200),
            panel: Hex::rgb(0x24, 0x29, 0x33),
            panel_header: Hex::rgb(0x3b, 0x42, 0x52),
            border: Hex::rgba(0xd8, 0xde, 0xe9, 30),
            grid: Hex::rgb(0x31, 0x37, 0x44),
            text: Hex::rgb(0xec, 0xef, 0xf4),
            muted: Hex::rgb(0x9a, 0xa5, 0xb8),
            highlight: Hex::rgb(0xeb, 0xcb, 0x8b),
            highlight_dim: Hex::rgb(0xa8, 0x91, 0x63),
            info: Hex::rgb(0x88, 0xc0, 0xd0),
            control: Hex::rgba(0xd8, 0xde, 0xe9, 22),
            control_hover: Hex::rgba(0xd8, 0xde, 0xe9, 40),
            accent: Some(Hex::rgb(0x88, 0xc0, 0xd0)),
            up: Hex::rgb(0xa3, 0xbe, 0x8c),
            down: Hex::rgb(0xbf, 0x61, 0x6a),
            candle_up: Hex::rgb(0xa3, 0xbe, 0x8c),
            candle_down: Hex::rgb(0xbf, 0x61, 0x6a),
            series: [
                Hex::rgb(0x81, 0xa1, 0xc1),
                Hex::rgb(0xb4, 0x8e, 0xad),
                Hex::rgb(0xeb, 0xcb, 0x8b),
                Hex::rgb(0x8f, 0xbc, 0xbb),
                Hex::rgb(0xd8, 0xde, 0xe9),
                Hex::rgb(0xd0, 0x87, 0x70),
            ],
        },
        fonts: fonts(UiFont::System, MonoFont::System, 1.0),
        density: Density::Regular,
        builtin: true,
        source: None,
    }
}

/// Monokai (Wimer Hazenberg).
fn monokai() -> Theme {
    Theme {
        name: "Monokai".into(),
        dark: true,
        colors: ThemeColors {
            background: Hex::rgb(0x27, 0x28, 0x22),
            sidebar: Hex::rgba(0x2d, 0x2e, 0x27, 210),
            panel: Hex::rgb(0x1e, 0x1f, 0x1c),
            panel_header: Hex::rgb(0x3e, 0x3d, 0x32),
            border: Hex::rgba(0xf8, 0xf8, 0xf2, 28),
            grid: Hex::rgb(0x2c, 0x2d, 0x27),
            text: Hex::rgb(0xf8, 0xf8, 0xf2),
            muted: Hex::rgb(0x90, 0x8c, 0x76),
            highlight: Hex::rgb(0xe6, 0xdb, 0x74),
            highlight_dim: Hex::rgb(0xa0, 0x98, 0x50),
            info: Hex::rgb(0x66, 0xd9, 0xef),
            control: Hex::rgba(0xf8, 0xf8, 0xf2, 20),
            control_hover: Hex::rgba(0xf8, 0xf8, 0xf2, 36),
            accent: Some(Hex::rgb(0xa6, 0xe2, 0x2e)),
            up: Hex::rgb(0xa6, 0xe2, 0x2e),
            down: Hex::rgb(0xf9, 0x26, 0x72),
            candle_up: Hex::rgb(0xa6, 0xe2, 0x2e),
            candle_down: Hex::rgb(0xf9, 0x26, 0x72),
            series: [
                Hex::rgb(0x66, 0xd9, 0xef),
                Hex::rgb(0xf9, 0x26, 0x72),
                Hex::rgb(0xe6, 0xdb, 0x74),
                Hex::rgb(0xae, 0x81, 0xff),
                Hex::rgb(0xf8, 0xf8, 0xf2),
                Hex::rgb(0xfd, 0x97, 0x1f),
            ],
        },
        fonts: fonts(UiFont::System, MonoFont::System, 1.0),
        density: Density::Regular,
        builtin: true,
        source: None,
    }
}

/// The themes that ship with the app, in display order.
pub fn builtins() -> Vec<Theme> {
    vec![
        terminal_dark(),
        terminal_light(),
        bloomberg(),
        high_contrast(),
        solarized_dark(),
        solarized_light(),
        nord(),
        monokai(),
    ]
}

/// Custom theme files.
pub struct ThemeStore {
    pub dir: PathBuf,
}

impl ThemeStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{}.toml", crate::layouts::Store::file_stem(name)))
    }

    /// Built-ins followed by custom themes (sorted by name). A custom file that reuses a
    /// built-in or earlier name is listed with a suffix rather than shadowing it.
    pub fn load_all(&self) -> Vec<Theme> {
        let mut out = builtins();
        let mut files: Vec<PathBuf> = storage::list(&self.dir)
            .into_iter()
            .filter(|p| p.extension().is_some_and(|x| x == "toml" || x == "json"))
            .collect();
        files.sort();
        let mut custom: Vec<Theme> = Vec::new();
        for p in files {
            match load_file(&p) {
                Ok(mut t) => {
                    let taken = |n: &str, list: &[Theme], more: &[Theme]| {
                        list.iter().chain(more).any(|x| x.name.eq_ignore_ascii_case(n))
                    };
                    if taken(&t.name, &out, &custom) {
                        let base = format!("{} (Custom)", t.name);
                        let mut name = base.clone();
                        let mut k = 2;
                        while taken(&name, &out, &custom) {
                            name = format!("{base} {k}");
                            k += 1;
                        }
                        t.name = name;
                    }
                    custom.push(t);
                }
                Err(e) => tracing::warn!("theme {}: {e:#}", p.display()),
            }
        }
        custom.sort_by_key(|t| t.name.to_lowercase());
        out.extend(custom);
        out
    }

    /// Write a custom theme (to the file it came from, else `<name>.toml`).
    pub fn save(&self, theme: &Theme) -> Result<PathBuf> {
        if theme.builtin {
            bail!("built-in themes are read-only; duplicate it to edit");
        }
        let path = theme
            .source
            .clone()
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .unwrap_or_else(|| self.path(&theme.name));
        storage::write_atomic(&path, theme.to_toml().as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    /// Remove a custom theme's file.
    pub fn delete(&self, theme: &Theme) -> Result<()> {
        if theme.builtin {
            bail!("built-in themes cannot be deleted");
        }
        let path = theme.source.clone().unwrap_or_else(|| self.path(&theme.name));
        if !storage::exists(&path) {
            bail!("no file for theme {:?}", theme.name);
        }
        Ok(storage::remove(&path)?)
    }

    /// Write a theme to `to` as JSON (`.json`) or TOML (anything else).
    pub fn export(theme: &Theme, to: &Path) -> Result<()> {
        let json = to.extension().is_some_and(|x| x.eq_ignore_ascii_case("json"));
        let text = if json { theme.to_json() } else { theme.to_toml() };
        storage::write_atomic(to, text.as_bytes()).with_context(|| format!("writing {}", to.display()))
    }
}

/// Read a TOML or JSON theme file.
pub fn load_file(path: &Path) -> Result<Theme> {
    let text = String::from_utf8(storage::read(path).with_context(|| format!("reading {}", path.display()))?)
        .with_context(|| format!("{} is not UTF-8 text", path.display()))?;
    let json = path.extension().is_some_and(|x| x.eq_ignore_ascii_case("json"));
    let mut t = Theme::parse(&text, json)?;
    t.source = Some(path.to_path_buf());
    if t.name == "Untitled Theme" {
        t.name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    }
    Ok(t)
}

/// `base`, or `base 2`, `base 3`… avoiding names in `themes`.
pub fn unique_name(themes: &[Theme], base: &str) -> String {
    let taken = |n: &str| themes.iter().any(|t| t.name.eq_ignore_ascii_case(n)) || n.eq_ignore_ascii_case(SYSTEM);
    if !taken(base) {
        return base.to_string();
    }
    (2..).map(|i| format!("{base} {i}")).find(|n| !taken(n)).expect("unbounded")
}

/// Find a theme by exact name, then by unique prefix (case-insensitive).
pub fn find<'a>(themes: &'a [Theme], query: &str) -> Option<&'a Theme> {
    let q = query.trim();
    themes.iter().find(|t| t.name.eq_ignore_ascii_case(q)).or_else(|| {
        let ql = q.to_lowercase();
        let mut hits = themes.iter().filter(|t| t.name.to_lowercase().starts_with(&ql));
        let first = hits.next()?;
        hits.next().is_none().then_some(first)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lum(c: Color32) -> f32 {
        0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32
    }

    #[test]
    fn hex_parse_and_format() {
        assert_eq!(Hex::parse("#ff8000"), Some(Hex::rgb(255, 128, 0)));
        assert_eq!(Hex::parse("ff8000"), Some(Hex::rgb(255, 128, 0)));
        assert_eq!(Hex::parse("#f80"), Some(Hex::rgb(255, 136, 0)));
        assert_eq!(Hex::parse("#ffffff1a"), Some(Hex::rgba(255, 255, 255, 26)));
        for bad in ["", "#12", "#gggggg", "#1234567", "red"] {
            assert_eq!(Hex::parse(bad), None, "{bad}");
        }
        assert_eq!(Hex::rgb(1, 2, 3).format(), "#010203");
        let a = Hex::rgba(255, 255, 255, 22);
        assert_eq!(Hex::parse(&a.format()), Some(a));
    }

    #[test]
    fn builtins_are_distinct_and_readable() {
        let all = builtins();
        assert_eq!(all.len(), 8);
        for (i, t) in all.iter().enumerate() {
            assert!(t.builtin);
            assert!(all[i + 1..].iter().all(|o| !o.name.eq_ignore_ascii_case(&t.name)), "duplicate {}", t.name);
            assert!(all[i + 1..].iter().all(|o| o.colors != t.colors), "{} duplicates colours", t.name);
            let c = &t.colors;
            // Body text and highlights stand out from the data panels.
            assert!((lum(c.text.0) - lum(c.panel.0)).abs() > 110.0, "{} text contrast", t.name);
            assert!((lum(c.highlight.0) - lum(c.panel.0)).abs() > 60.0, "{} highlight contrast", t.name);
            assert_ne!(c.up, c.down, "{}", t.name);
            // The dark flag matches the panel brightness (egui widgets follow it).
            assert_eq!(t.dark, lum(c.panel.0) < 128.0, "{} dark flag", t.name);
            let p = t.palette(Color32::RED);
            assert_eq!(p.accent, c.accent.map_or(Color32::RED, |a| a.0));
            assert_eq!(p.candle_up, c.candle_up.0);
        }
        assert!(find(&all, "solarized").is_none(), "ambiguous prefix");
        assert_eq!(find(&all, "solarized l").unwrap().name, "Solarized Light");
        assert_eq!(find(&all, "NORD").unwrap().name, "Nord");
    }

    #[test]
    fn every_builtin_round_trips_through_toml_and_json() {
        for t in builtins() {
            let from_toml = Theme::parse(&t.to_toml(), false).unwrap();
            assert_eq!(from_toml, Theme { builtin: false, ..t.clone() }, "{} toml", t.name);
            let from_json = Theme::parse(&t.to_json(), true).unwrap();
            assert_eq!(from_json, from_toml, "{} json", t.name);
        }
        let toml = terminal_dark().to_toml();
        assert!(toml.contains("[colors]") && toml.contains("background = \"#1e1e20\""), "{toml}");
    }

    #[test]
    fn partial_files_inherit_from_their_base() {
        let t = Theme::parse("name = \"Mint\"\ndark = false\n[colors]\nup = \"#00aa66\"\n", false).unwrap();
        assert_eq!(t.colors.up, Hex::rgb(0, 0xaa, 0x66));
        assert_eq!(t.colors.panel, terminal_light().colors.panel, "light base");
        assert_eq!(t.density, Density::Regular);
        let j = Theme::parse(r#"{"name":"Night","fonts":{"scale":9.0},"density":"Compact"}"#, true).unwrap();
        assert_eq!(j.colors.panel, terminal_dark().colors.panel, "dark base by default");
        assert_eq!(j.fonts.scale, 1.6, "clamped");
        assert_eq!(j.density, Density::Compact);
        assert!(Theme::parse("[colors]\nup = \"green\"", false).is_err(), "bad colour is an error");
        assert!(Theme::parse("[1, 2]", true).is_err());
        assert!(Theme::parse("not = [toml", false).is_err());
    }

    #[test]
    fn store_saves_lists_deletes_and_resolves_collisions() {
        let dir = std::env::temp_dir().join(format!("tr-themes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ThemeStore::new(dir.clone());
        assert_eq!(store.load_all().len(), 8);
        let mut mine = Theme { name: "Desk".into(), builtin: false, source: None, ..nord() };
        mine.colors.up = Hex::rgb(1, 2, 3);
        store.save(&mine).unwrap();
        assert!(store.save(&nord()).is_err(), "built-ins are read-only");
        // A hand-dropped JSON file reusing a built-in name gets a suffix.
        std::fs::write(dir.join("x.json"), r##"{"name":"Nord","colors":{"down":"#ff00ff"}}"##).unwrap();
        let all = store.load_all();
        assert_eq!(all.len(), 10);
        assert_eq!(find(&all, "Desk").unwrap().colors.up, Hex::rgb(1, 2, 3));
        let copy = find(&all, "Nord (Custom)").unwrap();
        assert_eq!(copy.colors.down, Hex::rgb(255, 0, 255));
        assert!(!copy.builtin);
        assert_eq!(unique_name(&all, "Desk"), "Desk 2");
        assert_eq!(unique_name(&all, "System"), "System 2");
        // Export as JSON and TOML, then import back.
        for ext in ["json", "toml"] {
            let out = dir.join(format!("export/desk.{ext}"));
            ThemeStore::export(&mine, &out).unwrap();
            assert_eq!(load_file(&out).unwrap().colors, mine.colors, "{ext}");
        }
        let all = store.load_all();
        // Saving an edited theme goes back to its own file.
        let mut desk = find(&all, "desk").unwrap().clone();
        desk.colors.down = Hex::rgb(9, 9, 9);
        assert_eq!(store.save(&desk).unwrap(), store.path("Desk"));
        store.delete(&desk).unwrap();
        store.delete(find(&all, "Nord (Custom)").unwrap()).unwrap();
        assert!(store.delete(&desk).is_err());
        assert!(store.delete(&nord()).is_err());
        assert_eq!(store.load_all().len(), 8);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
