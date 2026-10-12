//! User preferences persisted in `prefs.json`: appearance (theme mode, colours, fonts,
//! density), the hotkey map and defaults for new panels.

use std::collections::BTreeMap;

use egui::{Key, Modifiers};
use serde::{Deserialize, Serialize};
use tr_chart::scene::ChartType;
use tr_core::Timeframe;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

/// Which window edge the sidebar sits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SidebarSide {
    #[default]
    Left,
    Right,
}

impl SidebarSide {
    pub fn other(self) -> Self {
        match self {
            SidebarSide::Left => SidebarSide::Right,
            SidebarSide::Right => SidebarSide::Left,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Density {
    Compact,
    #[default]
    Regular,
    Comfortable,
}

impl Density {
    pub const ALL: [Density; 3] = [Density::Compact, Density::Regular, Density::Comfortable];

    /// Multiplier for row heights, paddings and gaps.
    pub fn factor(self) -> f32 {
        match self {
            Density::Compact => 0.82,
            Density::Regular => 1.0,
            Density::Comfortable => 1.2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Density::Compact => "Compact",
            Density::Regular => "Regular",
            Density::Comfortable => "Comfortable",
        }
    }
}

/// Font for window chrome (labels, titles, menus).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum UiFont {
    /// SF Pro on macOS (the platform UI font).
    #[default]
    System,
    /// Monospace everywhere, classic terminal look.
    Monospace,
}

/// Font for numbers, tickers and the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MonoFont {
    /// SF Mono on macOS, else the bundled monospace font.
    #[default]
    System,
    Menlo,
    Monaco,
    Courier,
    /// egui's bundled Hack.
    Bundled,
}

impl MonoFont {
    pub const ALL: [MonoFont; 5] =
        [MonoFont::System, MonoFont::Menlo, MonoFont::Monaco, MonoFont::Courier, MonoFont::Bundled];

    pub fn label(self) -> &'static str {
        match self {
            MonoFont::System => "System (SF Mono)",
            MonoFont::Menlo => "Menlo",
            MonoFont::Monaco => "Monaco",
            MonoFont::Courier => "Courier New",
            MonoFont::Bundled => "Hack (bundled)",
        }
    }

    /// Candidate font files, first existing wins.
    pub fn paths(self) -> &'static [&'static str] {
        match self {
            MonoFont::System => &[
                "/System/Library/Fonts/SFNSMono.ttf",
                "/System/Library/Fonts/Menlo.ttc",
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            ],
            MonoFont::Menlo => &["/System/Library/Fonts/Menlo.ttc"],
            MonoFont::Monaco => &["/System/Library/Fonts/Monaco.ttf", "/System/Library/Fonts/Monaco.dfont"],
            MonoFont::Courier => &[
                "/System/Library/Fonts/Supplemental/Courier New.ttf",
                "/usr/share/fonts/truetype/msttcorefonts/Courier_New.ttf",
            ],
            MonoFont::Bundled => &[],
        }
    }
}

pub type Rgb = [u8; 3];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    /// Active theme name, or `System` to follow the OS with `dark_theme`/`light_theme`.
    pub theme: String,
    pub dark_theme: String,
    pub light_theme: String,
    pub vibrancy: bool,
    pub sidebar_side: SidebarSide,
    // Pre-theme (v2) appearance overrides, read only to migrate them into a theme.
    #[serde(skip_serializing)]
    pub mode: ThemeMode,
    #[serde(skip_serializing)]
    pub accent: Option<Rgb>,
    #[serde(skip_serializing)]
    pub up: Option<Rgb>,
    #[serde(skip_serializing)]
    pub down: Option<Rgb>,
    #[serde(skip_serializing)]
    pub highlight: Option<Rgb>,
    #[serde(skip_serializing)]
    pub ui_font: UiFont,
    #[serde(skip_serializing)]
    pub mono_font: MonoFont,
    #[serde(skip_serializing)]
    pub font_scale: f32,
    #[serde(skip_serializing)]
    pub density: Density,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: crate::themes::SYSTEM.into(),
            dark_theme: "Terminal Dark".into(),
            light_theme: "Terminal Light".into(),
            mode: ThemeMode::System,
            accent: None,
            up: None,
            down: None,
            highlight: None,
            ui_font: UiFont::System,
            mono_font: MonoFont::System,
            font_scale: 1.0,
            density: Density::Regular,
            vibrancy: true,
            sidebar_side: SidebarSide::Left,
        }
    }
}

/// Defaults applied to newly created panels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelDefaults {
    pub chart_type: ChartType,
    pub timeframe: Timeframe,
    pub show_volume: bool,
    pub congress_markers: bool,
    pub book_depth: usize,
}

impl Default for PanelDefaults {
    fn default() -> Self {
        Self {
            chart_type: ChartType::Candles,
            timeframe: Timeframe::D1,
            show_volume: true,
            congress_markers: true,
            book_depth: 20,
        }
    }
}

/// Every rebindable action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Action {
    Help,
    Congress,
    Politicians,
    Alerts,
    Watchlist,
    Refresh,
    FrameTiming,
    Portfolio,
    Positions,
    Orders,
    NextPane,
    PrevPane,
    ToggleSidebar,
    NextTheme,
    SwapSidebarSide,
    SplitRight,
    SplitDown,
    NewTab,
    ClosePane,
    PopOut,
    SaveLayout,
    Settings,
    NewChart,
    NewWatchlist,
    NewNews,
    NewOrderBook,
    NewWorldMarkets,
    NewYieldCurve,
    NewCongress,
    NewPoliticians,
    NewAlerts,
    NewSearch,
    NewOptions,
    DrawTrendline,
    DrawHorizontal,
    DrawVertical,
    DrawFib,
    DrawHorizontalRay,
    DrawCrossLine,
    DrawingToolbar,
    ResetView,
    LogScale,
    Magnet,
    Layout1,
    Layout2,
    Layout3,
    Layout4,
    Layout5,
    Layout6,
    Layout7,
    Layout8,
    Layout9,
}

impl Action {
    pub const ALL: [Action; 52] = [
        Action::Help,
        Action::Congress,
        Action::Politicians,
        Action::Alerts,
        Action::Watchlist,
        Action::Refresh,
        Action::FrameTiming,
        Action::Portfolio,
        Action::Positions,
        Action::Orders,
        Action::NextPane,
        Action::PrevPane,
        Action::ToggleSidebar,
        Action::NextTheme,
        Action::SwapSidebarSide,
        Action::SplitRight,
        Action::SplitDown,
        Action::NewTab,
        Action::ClosePane,
        Action::PopOut,
        Action::SaveLayout,
        Action::Settings,
        Action::NewChart,
        Action::NewWatchlist,
        Action::NewNews,
        Action::NewOrderBook,
        Action::NewWorldMarkets,
        Action::NewYieldCurve,
        Action::NewCongress,
        Action::NewPoliticians,
        Action::NewAlerts,
        Action::NewSearch,
        Action::NewOptions,
        Action::DrawTrendline,
        Action::DrawHorizontal,
        Action::DrawVertical,
        Action::DrawFib,
        Action::DrawHorizontalRay,
        Action::DrawCrossLine,
        Action::DrawingToolbar,
        Action::ResetView,
        Action::LogScale,
        Action::Magnet,
        Action::Layout1,
        Action::Layout2,
        Action::Layout3,
        Action::Layout4,
        Action::Layout5,
        Action::Layout6,
        Action::Layout7,
        Action::Layout8,
        Action::Layout9,
    ];

    pub fn label(self) -> String {
        match self {
            Action::Help => "Help".into(),
            Action::Congress => "Congress feed".into(),
            Action::Politicians => "Politicians".into(),
            Action::Alerts => "Alerts".into(),
            Action::Watchlist => "Watchlist".into(),
            Action::Refresh => "Refresh Congress filings".into(),
            Action::FrameTiming => "Frame timing readout".into(),
            Action::Portfolio => "Brokerage portfolio".into(),
            Action::Positions => "Brokerage positions".into(),
            Action::Orders => "Brokerage orders".into(),
            Action::NextPane => "Next pane".into(),
            Action::PrevPane => "Previous pane".into(),
            Action::ToggleSidebar => "Toggle sidebar".into(),
            Action::NextTheme => "Cycle to the next theme".into(),
            Action::SwapSidebarSide => "Move sidebar to the other side".into(),
            Action::SplitRight => "Split right".into(),
            Action::SplitDown => "Split down".into(),
            Action::NewTab => "New tab".into(),
            Action::ClosePane => "Close pane".into(),
            Action::PopOut => "Pop out pane".into(),
            Action::SaveLayout => "Save layout".into(),
            Action::Settings => "Settings".into(),
            Action::NewChart => "New Chart pane".into(),
            Action::NewWatchlist => "New Watchlist pane".into(),
            Action::NewNews => "New News pane".into(),
            Action::NewOrderBook => "New Order book pane".into(),
            Action::NewWorldMarkets => "New World markets pane".into(),
            Action::NewYieldCurve => "New Yield curve pane".into(),
            Action::NewCongress => "New Congress feed pane".into(),
            Action::NewPoliticians => "New Politicians pane".into(),
            Action::NewAlerts => "New Alerts pane".into(),
            Action::NewSearch => "New Security search pane".into(),
            Action::NewOptions => "New Options chain pane".into(),

            Action::DrawTrendline => "Draw trendline".into(),
            Action::DrawHorizontal => "Draw horizontal line".into(),
            Action::DrawVertical => "Draw vertical line".into(),
            Action::DrawFib => "Draw Fibonacci retracement".into(),
            Action::DrawHorizontalRay => "Draw horizontal ray".into(),
            Action::DrawCrossLine => "Draw cross line".into(),
            Action::DrawingToolbar => "Show or hide the drawing toolbar".into(),
            Action::ResetView => "Reset chart view".into(),
            Action::LogScale => "Toggle log scale".into(),
            Action::Magnet => "Toggle magnet".into(),
            a => format!("Switch to layout {}", a.layout_slot().unwrap_or(0) + 1),
        }
    }

    /// Group shown in the hotkey tables.
    pub fn category(self) -> &'static str {
        use Action as A;
        match self {
            a if a.panel_spec().is_some() => "New panels",
            A::NextPane
            | A::PrevPane
            | A::ToggleSidebar
            | A::NextTheme
            | A::SwapSidebarSide
            | A::SplitRight
            | A::SplitDown
            | A::NewTab
            | A::ClosePane
            | A::PopOut
            | A::Settings => "Workspace",
            a if a.layout_slot().is_some() || a == A::SaveLayout => "Layouts",
            A::DrawTrendline
            | A::DrawHorizontal
            | A::DrawVertical
            | A::DrawFib
            | A::DrawHorizontalRay
            | A::DrawCrossLine
            | A::DrawingToolbar
            | A::ResetView
            | A::LogScale
            | A::Magnet => "Chart",
            _ => "Panels and data",
        }
    }

    pub const CATEGORIES: [&'static str; 5] = ["New panels", "Panels and data", "Workspace", "Layouts", "Chart"];

    /// The panel command word for `New…` actions.
    pub fn panel_spec(self) -> Option<&'static str> {
        Some(match self {
            Action::NewChart => "CHART",
            Action::NewWatchlist => "WL",
            Action::NewNews => "NEWS",
            Action::NewOrderBook => "BOOK",
            Action::NewWorldMarkets => "WEI",
            Action::NewYieldCurve => "GC",
            Action::NewCongress => "CONG",
            Action::NewPoliticians => "POL",
            Action::NewAlerts => "ALRT",
            Action::NewSearch => "SECF",
            Action::NewOptions => "OMON",
            _ => return None,
        })
    }

    /// Index into the saved-layout list for `Layout1..Layout9`.
    pub fn layout_slot(self) -> Option<usize> {
        let i = Action::ALL.iter().position(|&a| a == self)?;
        let first = Action::ALL.iter().position(|&a| a == Action::Layout1)?;
        (i >= first).then(|| i - first)
    }
}

/// Modifier keys arrive as key presses of their own; they never form a binding alone.
pub fn is_modifier(key: Key) -> bool {
    matches!(
        key,
        Key::ShiftLeft
            | Key::ShiftRight
            | Key::ControlLeft
            | Key::ControlRight
            | Key::AltLeft
            | Key::AltRight
            | Key::SuperLeft
            | Key::SuperRight
    )
}

/// A key with exact modifiers. `command` is ⌘ on macOS and Ctrl elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub key: Key,
    pub alt: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub command: bool,
}

impl Chord {
    pub fn new(key: Key) -> Self {
        Self { key, alt: false, shift: false, ctrl: false, command: false }
    }

    pub fn from_event(key: Key, m: Modifiers) -> Self {
        // On non-mac platforms Ctrl is the command key; report it once.
        let mac = cfg!(target_os = "macos");
        Self { key, alt: m.alt, shift: m.shift, ctrl: if mac { m.ctrl } else { false }, command: m.command }
    }

    /// Parse `"Alt+T"`, `"Cmd+Shift+S"`, `"Ctrl+Cmd+S"`, `"F1"`, `"Shift+Tab"`.
    pub fn parse(s: &str) -> Option<Self> {
        let mut c = Chord::new(Key::Escape);
        let mut key = None;
        for part in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
            match part.to_ascii_lowercase().as_str() {
                "alt" | "option" | "opt" | "⌥" => c.alt = true,
                "shift" | "⇧" => c.shift = true,
                "ctrl" | "control" | "⌃" => {
                    if cfg!(target_os = "macos") {
                        c.ctrl = true
                    } else {
                        c.command = true
                    }
                }
                "cmd" | "command" | "⌘" | "mod" => c.command = true,
                _ => key = Some(Key::from_name(part).or_else(|| Key::from_name(&part.to_ascii_uppercase()))?),
            }
        }
        c.key = key.filter(|k| !is_modifier(*k))?;
        Some(c)
    }

    /// Canonical text form (round-trips through [`Chord::parse`]).
    pub fn format(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl+");
        }
        if self.command {
            s.push_str(if cfg!(target_os = "macos") { "Cmd+" } else { "Ctrl+" });
        }
        if self.alt {
            s.push_str("Alt+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        s.push_str(self.key.name());
        s
    }

    /// Compact display form, using macOS symbols on macOS.
    pub fn display(&self) -> String {
        if !cfg!(target_os = "macos") {
            let f = self.format();
            return match f.rsplit_once('+') {
                Some((mods, _)) => format!("{mods}+{}", self.key.symbol_or_name()),
                None => self.key.symbol_or_name().to_string(),
            };
        }
        let mut s = String::new();
        for (on, sym) in [(self.ctrl, "⌃"), (self.alt, "⌥"), (self.shift, "⇧"), (self.command, "⌘")] {
            if on {
                s.push_str(sym);
            }
        }
        s.push_str(self.key.symbol_or_name());
        s
    }

    /// Key-cap labels, modifiers first (⌃ ⌥ ⇧ ⌘ on macOS, words elsewhere).
    pub fn tokens(&self) -> Vec<String> {
        let mac = cfg!(target_os = "macos");
        let mut t = Vec::new();
        let mods = if mac {
            [(self.ctrl, "⌃"), (self.alt, "⌥"), (self.shift, "⇧"), (self.command, "⌘")]
        } else {
            [(self.ctrl || self.command, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift"), (false, "")]
        };
        for (on, label) in mods {
            if on {
                t.push(label.to_string());
            }
        }
        t.push(self.key.symbol_or_name().to_string());
        t
    }

    pub fn matches(&self, key: Key, m: Modifiers) -> bool {
        *self == Chord::from_event(key, m)
    }
}

/// Hotkey bindings; stored as text so the JSON is human-editable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Keymap(pub BTreeMap<Action, String>);

impl Default for Keymap {
    fn default() -> Self {
        let mac = cfg!(target_os = "macos");
        let mut m = BTreeMap::new();
        let mut bind = |a: Action, k: &str| {
            m.insert(a, k.to_string());
        };
        bind(Action::Help, "F1");
        bind(Action::Congress, "F2");
        bind(Action::Politicians, "F3");
        bind(Action::Alerts, "F4");
        bind(Action::Watchlist, "F5");
        bind(Action::Refresh, "F6");
        bind(Action::FrameTiming, "F12");
        bind(Action::Portfolio, "F7");
        bind(Action::Positions, "F8");
        bind(Action::Orders, "F9");
        bind(Action::NextPane, "Tab");
        bind(Action::PrevPane, "Shift+Tab");
        bind(Action::ToggleSidebar, if mac { "Ctrl+Cmd+S" } else { "Ctrl+Shift+S" });
        bind(Action::SplitRight, "Cmd+D");
        bind(Action::SplitDown, "Cmd+Shift+D");
        bind(Action::NewTab, "Cmd+T");
        bind(Action::ClosePane, "Cmd+W");
        bind(Action::PopOut, "Cmd+Shift+P");
        bind(Action::SaveLayout, "Cmd+S");
        bind(Action::Settings, "Cmd+Comma");
        bind(Action::DrawTrendline, "Alt+T");
        bind(Action::DrawHorizontal, "Alt+H");
        bind(Action::DrawVertical, "Alt+V");
        bind(Action::DrawFib, "Alt+F");
        bind(Action::DrawHorizontalRay, "Alt+J");
        bind(Action::DrawCrossLine, "Alt+C");
        bind(Action::DrawingToolbar, "Alt+D");
        bind(Action::ResetView, "Alt+R");
        bind(Action::LogScale, "Alt+L");
        bind(Action::Magnet, "Alt+M");
        // ⌘1…⌘9, ⌘0 open panels; ⌥1…⌥9 switch saved layouts.
        bind(Action::NewOptions, "Cmd+Shift+O");
        bind(Action::SwapSidebarSide, "Cmd+Alt+S");
        bind(Action::NextTheme, "Cmd+Alt+T");
        for (i, a) in Action::ALL.iter().filter(|a| a.panel_spec().is_some() && **a != Action::NewOptions).enumerate() {
            bind(*a, &format!("Cmd+{}", (i + 1) % 10));
        }
        for (i, a) in Action::ALL.iter().filter(|a| a.layout_slot().is_some()).enumerate() {
            bind(*a, &format!("Alt+{}", i + 1));
        }
        Keymap(m)
    }
}

impl Keymap {
    pub fn chord(&self, a: Action) -> Option<Chord> {
        self.0.get(&a).and_then(|s| Chord::parse(s))
    }

    /// Which action a key press triggers.
    pub fn action_for(&self, key: Key, m: Modifiers) -> Option<Action> {
        self.0.iter().find(|(_, s)| Chord::parse(s).is_some_and(|c| c.matches(key, m))).map(|(a, _)| *a)
    }

    /// Bind `a` to `chord`, unbinding any other action that used it. Returns the displaced action.
    pub fn bind(&mut self, a: Action, chord: Chord) -> Option<Action> {
        let text = chord.format();
        let displaced = self.0.iter().find(|(b, s)| **b != a && Chord::parse(s) == Some(chord)).map(|(b, _)| *b);
        if let Some(d) = displaced {
            self.0.remove(&d);
        }
        self.0.insert(a, text);
        displaced
    }

    /// Fill in defaults for actions missing from an older or hand-edited file.
    pub fn with_defaults(mut self) -> Self {
        for (a, k) in Keymap::default().0 {
            let taken = Chord::parse(&k).is_some_and(|c| self.0.values().any(|v| Chord::parse(v) == Some(c)));
            if !self.0.contains_key(&a) && !taken {
                self.0.insert(a, k);
            }
        }
        self
    }
}

pub const PREFS_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// File format revision, for migrating defaults (absent in v1 files, hence 0).
    #[serde(default)]
    pub version: u32,
    pub appearance: Appearance,
    pub hotkeys: Keymap,
    pub panels: PanelDefaults,
    /// A custom theme built from v2 appearance overrides, for the app to save.
    #[serde(skip)]
    pub migrated_theme: Option<crate::themes::Theme>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            version: PREFS_VERSION,
            appearance: Appearance::default(),
            hotkeys: Keymap::default(),
            panels: PanelDefaults::default(),
            migrated_theme: None,
        }
    }
}

impl Prefs {
    pub fn load(path: &std::path::Path) -> Prefs {
        let mut p: Prefs = std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        p.migrate();
        p.hotkeys = p.hotkeys.with_defaults();
        p.appearance.font_scale = p.appearance.font_scale.clamp(0.7, 1.6);
        p
    }

    /// Upgrade older files. v2 moved saved-layout switching from ⌘1…⌘9 to ⌥1…⌥9 so ⌘1…⌘0
    /// can open panels; bindings the user changed are left alone.
    pub fn migrate(&mut self) {
        if self.version < 2 {
            let layouts = Action::ALL.iter().filter(|a| a.layout_slot().is_some());
            for (i, a) in layouts.enumerate() {
                let old = Chord::parse(&format!("Cmd+{}", i + 1));
                if self.hotkeys.0.get(a).and_then(|s| Chord::parse(s)) == old {
                    self.hotkeys.0.remove(a);
                }
            }
        }
        if self.version < 3 {
            self.migrated_theme = self.migrate_appearance();
        }
        self.version = PREFS_VERSION;
    }

    /// v3 moved colours, fonts and density into themes. Plain light/dark choices map to
    /// the built-in themes; any overrides become a custom "My Theme" on that base.
    fn migrate_appearance(&mut self) -> Option<crate::themes::Theme> {
        use crate::themes::{Hex, terminal_dark, terminal_light};
        let a = &mut self.appearance;
        let overrides = a.accent.is_some()
            || a.up.is_some()
            || a.down.is_some()
            || a.highlight.is_some()
            || a.ui_font != UiFont::System
            || a.mono_font != MonoFont::System
            || (a.font_scale - 1.0).abs() > 1e-3
            || a.density != Density::Regular;
        let base = if a.mode == ThemeMode::Light { terminal_light() } else { terminal_dark() };
        if !overrides {
            a.theme = match a.mode {
                ThemeMode::System => crate::themes::SYSTEM.into(),
                _ => base.name,
            };
            return None;
        }
        let rgb = |c: Rgb| Hex::rgb(c[0], c[1], c[2]);
        let mut t = base;
        t.name = "My Theme".into();
        t.builtin = false;
        t.colors.accent = a.accent.map(rgb).or(t.colors.accent);
        if let Some(c) = a.up {
            t.colors.up = rgb(c);
            t.colors.candle_up = rgb(c);
        }
        if let Some(c) = a.down {
            t.colors.down = rgb(c);
            t.colors.candle_down = rgb(c);
        }
        if let Some(c) = a.highlight {
            t.colors.highlight = rgb(c);
            t.colors.highlight_dim = rgb(c.map(|x| (x as f32 * 0.66) as u8));
        }
        t.fonts.ui = a.ui_font;
        t.fonts.mono = a.mono_font;
        t.fonts.scale = a.font_scale.clamp(0.7, 1.6);
        t.density = a.density;
        a.theme = t.name.clone();
        Some(t)
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        crate::layouts::write_atomic(path, &serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_parse_and_round_trip() {
        let c = Chord::parse("Alt+T").unwrap();
        assert_eq!(c, Chord { key: Key::T, alt: true, shift: false, ctrl: false, command: false });
        for s in ["F1", "Shift+Tab", "Cmd+Shift+D", "Alt+R", "Cmd+1", "Cmd+Comma"] {
            let c = Chord::parse(s).unwrap();
            assert_eq!(Chord::parse(&c.format()), Some(c), "{s}");
        }
        assert!(Chord::parse("Cmd+").is_none());
        assert!(Chord::parse("Cmd+NotAKey").is_none());
        assert!(Chord::parse("AltLeft").is_none(), "a bare modifier is not a binding");
        assert!(is_modifier(Key::SuperRight) && !is_modifier(Key::F9));
        let t = Chord::parse("Cmd+Shift+D").unwrap().tokens();
        assert_eq!(t.len(), 3, "two modifiers and the key");
        assert_eq!(t.last().map(String::as_str), Some("D"));
        assert!(c.matches(Key::T, Modifiers::ALT));
        assert!(!c.matches(Key::T, Modifiers::ALT | Modifiers::SHIFT), "modifiers match exactly");
    }

    #[test]
    fn keymap_defaults_lookup_and_rebinding() {
        let mut k = Keymap::default();
        assert_eq!(k.action_for(Key::F1, Modifiers::NONE), Some(Action::Help));
        assert_eq!(k.action_for(Key::Tab, Modifiers::SHIFT), Some(Action::PrevPane));
        assert_eq!(k.action_for(Key::Tab, Modifiers::NONE), Some(Action::NextPane));
        assert_eq!(k.action_for(Key::Num1, Modifiers::COMMAND), Some(Action::NewChart));
        assert_eq!(k.action_for(Key::Num0, Modifiers::COMMAND), Some(Action::NewSearch));
        assert_eq!(k.action_for(Key::Num3, Modifiers::ALT), Some(Action::Layout3));
        // Every action has a default binding, and no two share one.
        let chords: Vec<Chord> = Action::ALL.iter().filter_map(|a| k.chord(*a)).collect();
        assert_eq!(chords.len(), Action::ALL.len());
        for (i, c) in chords.iter().enumerate() {
            assert!(!chords[i + 1..].contains(c), "duplicate default {}", c.format());
        }
        for a in Action::ALL {
            assert!(Action::CATEGORIES.contains(&a.category()), "{a:?}");
        }
        // Rebinding a used chord steals it from the previous action.
        let displaced = k.bind(Action::Settings, Chord::parse("F1").unwrap());
        assert_eq!(displaced, Some(Action::Help));
        assert_eq!(k.action_for(Key::F1, Modifiers::NONE), Some(Action::Settings));
        assert!(k.chord(Action::Help).is_none());
        // Missing actions get defaults back unless their chord is taken.
        let restored = k.clone().with_defaults();
        assert!(restored.chord(Action::Help).is_none());
        assert!(restored.chord(Action::Magnet).is_some());
        assert_eq!(Action::Layout9.layout_slot(), Some(8));
        assert_eq!(Action::Help.layout_slot(), None);
    }

    #[test]
    fn prefs_json_round_trip_and_partial_files() {
        let mut p = Prefs::default();
        p.appearance.theme = "Nord".into();
        p.appearance.light_theme = "Solarized Light".into();
        p.appearance.sidebar_side = SidebarSide::Right;
        p.panels.chart_type = ChartType::Line;
        p.hotkeys.bind(Action::Help, Chord::parse("Cmd+Shift+H").unwrap());
        let json = serde_json::to_string_pretty(&p).unwrap();
        assert!(json.contains("\"Help\": \"Cmd+Shift+H\"") || json.contains("\"Help\": \"Ctrl+Shift+H\""));
        let back: Prefs = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
        // Older or hand-written files with only some fields still load.
        let partial: Prefs = serde_json::from_str(r#"{"appearance":{"theme":"Monokai"}}"#).unwrap();
        assert_eq!(partial.appearance.theme, "Monokai");
        assert_eq!(partial.appearance.dark_theme, "Terminal Dark");
        assert_eq!(partial.panels, PanelDefaults::default());
        let dir = std::env::temp_dir().join(format!("tr-prefs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prefs.json");
        p.save(&path).unwrap();
        assert_eq!(Prefs::load(&path), p);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn v1_prefs_migrate_layout_bindings_to_alt() {
        // A v1 file: layouts on ⌘1…, one customised, no panel actions yet.
        let mut v1 = Keymap::default();
        // (Brokerage actions arrived later; a v1 file cannot contain them.)
        v1.0.retain(|a, _| {
            a.panel_spec().is_none() && !matches!(a, Action::Portfolio | Action::Positions | Action::Orders)
        });
        for (i, a) in Action::ALL.iter().filter(|a| a.layout_slot().is_some()).enumerate() {
            v1.0.insert(*a, format!("Cmd+{}", i + 1));
        }
        v1.0.insert(Action::Layout9, "F9".into());
        let parsed: Prefs = serde_json::from_str(r#"{"hotkeys":{"Layout1":"Cmd+1"}}"#).unwrap();
        assert_eq!(parsed.version, 0, "files without a version are v1");
        let mut p = Prefs { version: 0, hotkeys: v1, ..Default::default() };
        p.migrate();
        p.hotkeys = p.hotkeys.with_defaults();
        assert_eq!(p.version, PREFS_VERSION);
        assert_eq!(p.hotkeys.action_for(Key::Num1, Modifiers::COMMAND), Some(Action::NewChart));
        assert_eq!(p.hotkeys.action_for(Key::Num1, Modifiers::ALT), Some(Action::Layout1));
        assert_eq!(p.hotkeys.action_for(Key::F9, Modifiers::NONE), Some(Action::Layout9), "customised binding kept");
        assert_eq!(p.hotkeys.chord(Action::Orders), None, "a new default never steals a user's key");
        assert_eq!(p.hotkeys.action_for(Key::F7, Modifiers::NONE), Some(Action::Portfolio), "free defaults are added");
    }

    #[test]
    fn v2_appearance_migrates_into_themes() {
        // Defaults follow the system.
        let mut p: Prefs = serde_json::from_str(r#"{"version":2,"appearance":{"mode":"System"}}"#).unwrap();
        p.migrate();
        assert_eq!(p.appearance.theme, "System");
        assert!(p.migrated_theme.is_none());
        // A plain light choice maps to the built-in light theme.
        let mut p: Prefs = serde_json::from_str(r#"{"version":2,"appearance":{"mode":"Light"}}"#).unwrap();
        p.migrate();
        assert_eq!(p.appearance.theme, "Terminal Light");
        // Overrides become a custom theme on the chosen base.
        let mut p: Prefs = serde_json::from_str(
            r#"{"version":2,"appearance":{"mode":"Dark","up":[1,2,3],"highlight":[200,100,0],"density":"Compact","font_scale":1.2}}"#,
        )
        .unwrap();
        p.migrate();
        let t = p.migrated_theme.clone().unwrap();
        assert_eq!(p.appearance.theme, "My Theme");
        assert!(t.dark && !t.builtin);
        assert_eq!(t.colors.up, crate::themes::Hex::rgb(1, 2, 3));
        assert_eq!(t.colors.candle_up, t.colors.up);
        assert_eq!(t.colors.highlight, crate::themes::Hex::rgb(200, 100, 0));
        assert_eq!(t.density, Density::Compact);
        assert!((t.fonts.scale - 1.2).abs() < 1e-6);
        // Legacy fields are no longer written.
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("font_scale") && json.contains("\"theme\":\"My Theme\""), "{json}");
    }
}
