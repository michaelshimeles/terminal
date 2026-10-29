//! Command palette (⌘K, Ctrl+K elsewhere): fuzzy search over every command, ticker,
//! panel, layout, theme, settings tab and workspace action.
//!
//! This module is the model: items, ranking and the persisted list of recent picks. The
//! overlay and the catalogue of live items (themes, saved layouts, watchlist) live in
//! `app/palette_view.rs`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::prefs::Action;

/// How many recent picks are remembered.
pub const MAX_RECENTS: usize = 10;
/// Rows shown for a query (the full list is still searchable).
pub const MAX_RESULTS: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Command,
    Ticker,
    Panel,
    Layout,
    Theme,
    Settings,
    Action,
    Indicator,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Command => "CMD",
            Kind::Ticker => "TICKER",
            Kind::Panel => "PANEL",
            Kind::Layout => "LAYOUT",
            Kind::Theme => "THEME",
            Kind::Settings => "SETTINGS",
            Kind::Action => "ACTION",
            Kind::Indicator => "STUDY",
        }
    }
}

/// What picking an item does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    /// Run a command line, exactly as if typed and sent with <GO>.
    Run(String),
    /// Run a rebindable action.
    Hotkey(Action),
    /// Open Settings on the tab with this label.
    Settings(String),
    /// Switch to a saved layout by name.
    SwitchLayout(String),
    /// Select a theme (or System) by exact name.
    Theme(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub kind: Kind,
    pub title: String,
    pub subtitle: String,
    /// Extra words that match but are not shown (aliases, full names).
    #[serde(default)]
    pub keywords: String,
    /// The action whose binding is shown as the shortcut hint.
    #[serde(default)]
    pub hotkey: Option<Action>,
    pub target: Target,
}

impl Item {
    pub fn new(kind: Kind, title: impl Into<String>, subtitle: impl Into<String>, target: Target) -> Self {
        Self { kind, title: title.into(), subtitle: subtitle.into(), keywords: String::new(), hotkey: None, target }
    }

    pub fn keywords(mut self, k: impl Into<String>) -> Self {
        self.keywords = k.into();
        self
    }

    pub fn hotkey(mut self, a: Action) -> Self {
        self.hotkey = Some(a);
        self
    }

    /// Two items are the same pick when they do the same thing.
    pub fn same(&self, other: &Item) -> bool {
        self.target == other.target
    }
}

/// Fuzzy subsequence score of `needle` in `hay` (case-insensitive), or `None` if some
/// character is missing. Rewards matches at word starts, runs of consecutive characters
/// and an exact or prefix match; penalises gaps.
pub fn fuzzy(needle: &str, hay: &str) -> Option<i32> {
    let n: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if n.is_empty() {
        return Some(0);
    }
    let h: Vec<char> = hay.chars().collect();
    let lower: Vec<char> = h.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    let mut score = 0;
    let mut ni = 0;
    let mut prev: Option<usize> = None;
    for (i, &c) in lower.iter().enumerate() {
        if ni == n.len() {
            break;
        }
        if c != n[ni] {
            continue;
        }
        let word_start = i == 0
            || !h[i - 1].is_alphanumeric()
            || (h[i].is_uppercase() && h[i - 1].is_lowercase())
            || (h[i].is_ascii_digit() && !h[i - 1].is_ascii_digit());
        score += 1;
        if word_start {
            score += 8;
        }
        match prev {
            Some(p) if p + 1 == i => score += 6,
            Some(p) => score -= ((i - p - 1) as i32).min(5),
            None => score -= (i as i32).min(8),
        }
        prev = Some(i);
        ni += 1;
    }
    if ni < n.len() {
        return None;
    }
    let hay_l: String = lower.iter().collect();
    let needle_l: String = n.iter().collect();
    if hay_l == needle_l {
        score += 60;
    } else if hay_l.starts_with(&needle_l) {
        score += 30;
    }
    Some(score)
}

/// Score an item for a query: every whitespace-separated word must match the title,
/// keywords or subtitle (title weighs most).
pub fn score(item: &Item, query: &str) -> Option<i32> {
    let mut total = 0;
    for word in query.split_whitespace() {
        let best = [
            fuzzy(word, &item.title).map(|s| s * 3),
            fuzzy(word, &item.keywords).map(|s| s * 2),
            fuzzy(word, &item.subtitle),
        ]
        .into_iter()
        .flatten()
        .max()?;
        total += best;
    }
    // A whole-query hit on the title (multi-word commands like `SMA 50`).
    if query.split_whitespace().nth(1).is_some()
        && let Some(s) = fuzzy(query.trim(), &item.title)
    {
        total = total.max(s * 3) + 20;
    }
    Some(total)
}

/// Palette state: open flag, query, selection and recent picks.
#[derive(Debug, Default)]
pub struct Palette {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    pub recents: Vec<Item>,
    /// Only items of this kind are offered (the indicator picker).
    pub scope: Option<Kind>,
    /// Set when opened, to focus the search field on the next frame.
    pub just_opened: bool,
    path: Option<PathBuf>,
}

impl Palette {
    /// Load recent picks from `path` (missing or unreadable files start empty).
    pub fn load(path: &Path) -> Self {
        let recents = crate::storage::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Self { recents, path: Some(path.to_path_buf()), ..Default::default() }
    }

    pub fn open(&mut self) {
        self.open = true;
        self.just_opened = true;
        self.query.clear();
        self.selected = 0;
        self.scope = None;
    }

    /// Open showing only `kind`, searching for `query`.
    pub fn open_scoped(&mut self, kind: Kind, query: &str) {
        self.open();
        self.scope = Some(kind);
        self.query = query.trim().to_string();
    }

    fn in_scope(&self, it: &Item) -> bool {
        self.scope.is_none_or(|k| it.kind == k)
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    pub fn toggle(&mut self) {
        if self.open { self.close() } else { self.open() }
    }

    /// Move the selection by `delta` rows, wrapping around `len`.
    pub fn step(&mut self, delta: i32, len: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected.min(len - 1) as i32 + delta).rem_euclid(len as i32) as usize;
    }

    /// Remember a pick (most recent first) and persist the list.
    pub fn record(&mut self, item: &Item) {
        self.recents.retain(|r| !r.same(item));
        self.recents.insert(0, item.clone());
        self.recents.truncate(MAX_RECENTS);
        if let Some(path) = &self.path
            && let Ok(json) = serde_json::to_vec_pretty(&self.recents)
            && let Err(e) = crate::storage::write_atomic(path, &json)
        {
            tracing::warn!("saving palette recents: {e}");
        }
    }

    /// Ranked results for the current query over `items`.
    pub fn results(&self, items: &[Item]) -> Vec<Item> {
        let (items, recents) = self.scoped(items);
        rank(&items, &recents, &self.query)
    }

    /// How many of [`Self::results`] are recent picks listed first (none while searching).
    pub fn recent_count(&self, items: &[Item]) -> usize {
        if !self.query.trim().is_empty() {
            return 0;
        }
        let (items, recents) = self.scoped(items);
        live_recents(&items, &recents).len()
    }

    fn scoped(&self, items: &[Item]) -> (Vec<Item>, Vec<Item>) {
        let keep = |v: &[Item]| -> Vec<Item> { v.iter().filter(|it| self.in_scope(it)).cloned().collect() };
        (keep(items), keep(&self.recents))
    }
}

/// Recent picks refreshed from the live catalogue: a pick still offered shows its current
/// row (markers, shortcut hints); saved layouts and themes that no longer exist are
/// dropped. Other picks (a searched ticker, a typed command) are kept as recorded.
pub fn live_recents(items: &[Item], recents: &[Item]) -> Vec<Item> {
    recents
        .iter()
        .filter_map(|r| match items.iter().find(|it| it.same(r)) {
            Some(live) => Some(live.clone()),
            None if matches!(r.target, Target::SwitchLayout(_) | Target::Theme(_)) => None,
            None => Some(r.clone()),
        })
        .collect()
}

/// Rank `items` for `query`. With no query: recent picks first, then everything in
/// catalogue order. With a query: best fuzzy matches, recent picks boosted, ties kept
/// in catalogue order.
pub fn rank(items: &[Item], recents: &[Item], query: &str) -> Vec<Item> {
    let recents = live_recents(items, recents);
    let recents = recents.as_slice();
    let recent_rank = |it: &Item| recents.iter().position(|r| r.same(it));
    if query.trim().is_empty() {
        let mut out: Vec<Item> = recents.to_vec();
        out.extend(items.iter().filter(|it| recent_rank(it).is_none()).cloned());
        return out;
    }
    // Recent picks that are no longer in the catalogue (a searched ticker) still match.
    let pool = items.iter().chain(recents.iter().filter(|r| !items.iter().any(|it| it.same(r))));
    let mut scored: Vec<(i32, usize, &Item)> = pool
        .enumerate()
        .filter_map(|(i, it)| {
            let s = score(it, query)?;
            let boost = recent_rank(it).map_or(0, |r| 40 - 3 * r as i32);
            Some((s + boost, i, it))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut out: Vec<Item> = Vec::new();
    for (_, _, it) in scored {
        if !out.iter().any(|o| o.same(it)) {
            out.push(it.clone());
        }
        if out.len() == MAX_RESULTS {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(code: &str, desc: &str) -> Item {
        Item::new(Kind::Command, code, desc, Target::Run(code.into()))
    }

    fn catalogue() -> Vec<Item> {
        vec![
            cmd("PORT", "Brokerage portfolio"),
            cmd("POS", "Brokerage positions"),
            cmd("ORD", "Brokerage orders"),
            cmd("OMON", "Option chain").keywords("options chain"),
            cmd("SMA 50", "Toggle SMA 50"),
            Item::new(Kind::Theme, "Nord", "Dark theme", Target::Run("THEME Nord".into())),
            Item::new(Kind::Settings, "Settings › Brokerage", "SnapTrade keys", Target::Settings("Brokerage".into())),
            Item::new(Kind::Action, "Move sidebar to the other side", "", Target::Hotkey(Action::SwapSidebarSide)),
            Item::new(Kind::Ticker, "AAPL US Equity", "Apple Inc", Target::Run("AAPL US Equity".into()))
                .keywords("Apple Inc"),
        ]
    }

    fn titles(v: &[Item]) -> Vec<&str> {
        v.iter().map(|i| i.title.as_str()).collect()
    }

    #[test]
    fn fuzzy_matches_subsequences_and_prefers_word_starts() {
        assert!(fuzzy("omn", "OMON").is_some());
        assert!(fuzzy("xyz", "OMON").is_none());
        assert_eq!(fuzzy("", "anything"), Some(0));
        // Exact beats prefix beats scattered.
        let exact = fuzzy("pos", "POS").unwrap();
        let prefix = fuzzy("pos", "POSITIONS").unwrap();
        let scattered = fuzzy("pos", "PORT LIST S").unwrap();
        assert!(exact > prefix && prefix > scattered, "{exact} {prefix} {scattered}");
        // Word-start initials match well: "mss" → "Move Sidebar to the other Side".
        assert!(fuzzy("mss", "Move sidebar to the other side").is_some());
        assert!(fuzzy("brk", "Settings › Brokerage").unwrap() > fuzzy("brk", "abcbrrrk").unwrap());
    }

    #[test]
    fn query_ranks_best_match_first() {
        let items = catalogue();
        assert_eq!(titles(&rank(&items, &[], "pos"))[0], "POS");
        assert_eq!(titles(&rank(&items, &[], "ord"))[0], "ORD");
        assert_eq!(titles(&rank(&items, &[], "omon"))[0], "OMON");
        assert_eq!(titles(&rank(&items, &[], "options"))[0], "OMON", "keywords match");
        assert_eq!(titles(&rank(&items, &[], "apple"))[0], "AAPL US Equity");
        assert_eq!(titles(&rank(&items, &[], "brokerage set"))[0], "Settings › Brokerage", "all words must match");
        assert_eq!(titles(&rank(&items, &[], "sma 50"))[0], "SMA 50");
        assert!(rank(&items, &[], "qqqzzz").is_empty());
    }

    #[test]
    fn recents_come_first_and_boost_matches() {
        let items = catalogue();
        let nord = items.iter().find(|i| i.title == "Nord").unwrap().clone();
        let ord = items.iter().find(|i| i.title == "ORD").unwrap().clone();
        let recents = vec![nord.clone(), ord.clone()];
        let empty = rank(&items, &recents, "");
        assert_eq!(titles(&empty)[..2], ["Nord", "ORD"]);
        assert_eq!(empty.len(), items.len(), "recents are not listed twice");
        // "or" matches ORD best; Nord is recent and also matches, so it moves up too.
        let r = rank(&items, &recents, "nor");
        assert_eq!(titles(&r)[0], "Nord");
        // A recent pick no longer in the catalogue (a searched ticker) is still found.
        let msft = Item::new(Kind::Ticker, "MSFT US Equity", "Microsoft", Target::Run("MSFT US Equity".into()));
        let r = rank(&items, std::slice::from_ref(&msft), "msft");
        assert_eq!(titles(&r), ["MSFT US Equity"]);
    }

    #[test]
    fn recents_follow_the_live_catalogue() {
        let theme = |name: &str, sub: &str| Item::new(Kind::Theme, name, sub, Target::Theme(name.into()));
        let layout = |name: &str| Item::new(Kind::Layout, name, "Saved layout", Target::SwitchLayout(name.into()));
        let ticker = Item::new(Kind::Ticker, "MSFT US Equity", "Microsoft", Target::Run("MSFT US Equity".into()));
        let recents =
            vec![theme("Nord", "Dark theme · current"), layout("Desk"), theme("Gone", "Dark theme"), ticker.clone()];
        let live = vec![theme("Nord", "Dark theme"), layout("Desk").hotkey(Action::Layout2)];
        let r = live_recents(&live, &recents);
        assert_eq!(titles(&r), ["Nord", "Desk", "MSFT US Equity"], "deleted themes and layouts drop out");
        assert_eq!(r[0].subtitle, "Dark theme", "the current marker follows the live row");
        assert_eq!(r[1].hotkey, Some(Action::Layout2), "shortcut hints follow the live row");
        assert_eq!(titles(&rank(&live, &recents, "")), ["Nord", "Desk", "MSFT US Equity"]);
        assert!(rank(&live, &recents, "gone").is_empty());
    }

    #[test]
    fn scoped_palette_offers_only_its_kind() {
        let rsi = Item::new(Kind::Indicator, "Relative Strength Index", "RSI 14", Target::Run("RSI".into()));
        let mut items = catalogue();
        items.push(rsi.clone());
        let mut p = Palette { recents: vec![items[0].clone(), rsi.clone()], ..Default::default() };
        p.open_scoped(Kind::Indicator, "  ");
        assert_eq!(p.query, "");
        assert_eq!(p.results(&items), std::slice::from_ref(&rsi));
        assert_eq!(p.recent_count(&items), 1, "recents outside the scope are hidden");
        p.query = "port".into();
        assert!(p.results(&items).is_empty());
        p.open();
        assert_eq!(p.scope, None, "a plain open drops the scope");
        assert_eq!(p.results(&items).len(), items.len());
        assert_eq!(p.recent_count(&items), 2);
    }

    #[test]
    fn selection_wraps_and_recents_persist() {
        let mut p = Palette::default();
        p.open();
        assert!(p.open && p.just_opened && p.query.is_empty());
        p.step(-1, 3);
        assert_eq!(p.selected, 2);
        p.step(1, 3);
        assert_eq!(p.selected, 0);
        p.step(1, 0);
        assert_eq!(p.selected, 0);
        p.toggle();
        assert!(!p.open);

        let dir = std::env::temp_dir().join(format!("tr-palette-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("palette.json");
        let mut p = Palette::load(&path);
        assert!(p.recents.is_empty());
        let items = catalogue();
        for it in items.iter().chain(items.iter()) {
            p.record(it);
        }
        p.record(&items[0]);
        assert_eq!(p.recents.len(), items.len().min(MAX_RECENTS));
        assert_eq!(p.recents[0].title, "PORT", "re-picking moves an item to the front");
        let back = Palette::load(&path);
        assert_eq!(back.recents, p.recents);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
