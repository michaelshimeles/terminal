//! Theme selection and editing: resolve the active theme (System follows the OS with the
//! configured dark/light pair), cycle, the THEME command, copy-on-write edits of built-ins,
//! and debounced saving of edited custom themes.

use std::time::Duration;

use super::TerminalApp;
use crate::prefs::{Density, UiFont};
use crate::themes::{self, SYSTEM, Theme};
use web_time::Instant;

impl TerminalApp {
    /// Index of the theme currently in effect.
    pub(super) fn active_theme_index(&self, ctx: &egui::Context) -> usize {
        let a = &self.prefs.appearance;
        let name = if a.theme.eq_ignore_ascii_case(SYSTEM) {
            match ctx.system_theme() {
                Some(egui::Theme::Light) => &a.light_theme,
                _ => &a.dark_theme,
            }
        } else {
            &a.theme
        };
        self.themes.iter().position(|t| t.name.eq_ignore_ascii_case(name)).unwrap_or(0)
    }

    /// Selection names in cycling order: System, then every theme.
    pub(super) fn theme_choices(&self) -> Vec<String> {
        std::iter::once(SYSTEM.to_string()).chain(self.themes.iter().map(|t| t.name.clone())).collect()
    }

    /// Select a theme by name (or System). Returns false if there is no such theme.
    pub(super) fn select_theme(&mut self, name: &str) -> bool {
        let resolved = if name.eq_ignore_ascii_case(SYSTEM) {
            SYSTEM.to_string()
        } else {
            match themes::find(&self.themes, name) {
                Some(t) => t.name.clone(),
                None => return false,
            }
        };
        self.prefs.appearance.theme = resolved.clone();
        self.save_prefs();
        self.status = format!("Theme: {resolved}");
        true
    }

    pub(super) fn cycle_theme(&mut self, step: i32) {
        let choices = self.theme_choices();
        let cur = choices.iter().position(|c| c.eq_ignore_ascii_case(&self.prefs.appearance.theme)).unwrap_or(0);
        let next = (cur as i32 + step).rem_euclid(choices.len() as i32) as usize;
        let name = choices[next].clone();
        self.select_theme(&name);
    }

    /// The index of an editable version of theme `idx`: custom themes edit in place; a
    /// built-in is duplicated into a new custom theme, which replaces it wherever it was
    /// selected.
    pub(super) fn editable_theme(&mut self, idx: usize) -> usize {
        if !self.themes[idx].builtin {
            return idx;
        }
        let original = self.themes[idx].name.clone();
        let mut copy = self.themes[idx].clone();
        copy.name = themes::unique_name(&self.themes, &format!("{original} Custom"));
        copy.builtin = false;
        copy.source = None;
        let name = copy.name.clone();
        self.themes.push(copy);
        let a = &mut self.prefs.appearance;
        for slot in [&mut a.theme, &mut a.dark_theme, &mut a.light_theme] {
            if slot.eq_ignore_ascii_case(&original) {
                *slot = name.clone();
            }
        }
        self.editing_theme = name.clone();
        self.save_prefs();
        self.mark_theme_dirty(&name);
        self.status = format!("{original} is built in; editing a copy named {name:?}");
        self.themes.len() - 1
    }

    pub(super) fn mark_theme_dirty(&mut self, name: &str) {
        if !self.dirty_themes.iter().any(|n| n == name) {
            self.dirty_themes.push(name.to_string());
        }
        self.themes_changed_at = Some(Instant::now());
    }

    /// Write edited custom themes once editing pauses (colour drags change every frame).
    pub(super) fn save_dirty_themes(&mut self, force: bool) {
        let idle = self.themes_changed_at.is_some_and(|t| t.elapsed() > Duration::from_millis(500));
        if self.dirty_themes.is_empty() || !(idle || force) {
            return;
        }
        for name in std::mem::take(&mut self.dirty_themes) {
            if let Some(t) = self.themes.iter_mut().find(|t| t.name == name && !t.builtin) {
                match self.theme_store.save(t) {
                    Ok(path) => t.source = Some(path),
                    Err(e) => self.status = format!("Could not save theme {name:?}: {e:#}"),
                }
            }
        }
        self.themes_changed_at = None;
    }

    /// Apply a change to the active theme (copying a built-in first), e.g. from THEME COMPACT.
    fn edit_active_theme(&mut self, ctx_theme: Option<egui::Theme>, f: impl FnOnce(&mut Theme)) {
        let a = &self.prefs.appearance;
        let name = if a.theme.eq_ignore_ascii_case(SYSTEM) {
            if ctx_theme == Some(egui::Theme::Light) { a.light_theme.clone() } else { a.dark_theme.clone() }
        } else {
            a.theme.clone()
        };
        let Some(idx) = self.themes.iter().position(|t| t.name.eq_ignore_ascii_case(&name)) else { return };
        let idx = self.editable_theme(idx);
        f(&mut self.themes[idx]);
        let n = self.themes[idx].name.clone();
        self.mark_theme_dirty(&n);
    }

    /// `THEME <name>|NEXT|PREV|SYSTEM|LIGHT|DARK|LIST|EDIT|COMPACT|REGULAR|COMFORTABLE|MONO|SF`.
    pub(super) fn theme_command(&mut self, arg: &str, system: Option<egui::Theme>) {
        let arg = arg.trim();
        match arg.to_ascii_uppercase().as_str() {
            "" | "LIST" => {
                self.status = format!("Themes: {}  (THEME <name>, THEME NEXT)", self.theme_choices().join(" · "));
            }
            "NEXT" => self.cycle_theme(1),
            "PREV" | "PREVIOUS" => self.cycle_theme(-1),
            "LIGHT" => {
                let n = self.prefs.appearance.light_theme.clone();
                self.select_theme(&n);
            }
            "DARK" => {
                let n = self.prefs.appearance.dark_theme.clone();
                self.select_theme(&n);
            }
            "EDIT" | "EDITOR" => {
                self.settings_tab = super::settings_view::SettingsTab::Themes;
                self.apply(crate::command::Function::Settings);
            }
            "COMPACT" => self.edit_active_theme(system, |t| t.density = Density::Compact),
            "REGULAR" => self.edit_active_theme(system, |t| t.density = Density::Regular),
            "COMFORTABLE" => self.edit_active_theme(system, |t| t.density = Density::Comfortable),
            "MONO" => self.edit_active_theme(system, |t| t.fonts.ui = UiFont::Monospace),
            "SF" | "SANS" => self.edit_active_theme(system, |t| t.fonts.ui = UiFont::System),
            _ => {
                if !self.select_theme(arg) {
                    self.status = format!("No theme {arg:?}. Themes: {}", self.theme_choices().join(" · "));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::themes::{builtins, find, unique_name};

    #[test]
    fn copies_of_builtins_get_unique_names() {
        let mut all = builtins();
        let a = unique_name(&all, "Nord Custom");
        assert_eq!(a, "Nord Custom");
        all.push(crate::themes::Theme { name: a, builtin: false, ..all[6].clone() });
        assert_eq!(unique_name(&all, "Nord Custom"), "Nord Custom 2");
        assert_eq!(find(&all, "nord c").unwrap().name, "Nord Custom");
    }
}
