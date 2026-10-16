//! Browser settings from the per-app cce config
//! (`~/.config/cce/cce-browser/config.kdl`, section `browser`) — the file
//! cce-system-interface's Browser page edits. Loaded at startup and
//! re-read when the window regains focus, so settings changed in
//! system-interface apply on the next switch back to the browser.

use std::path::PathBuf;

pub const DEFAULT_HOMEPAGE: &str = "https://servo.org";

/// Which edge the floating utility bar is anchored to. The page is
/// full-bleed under the bar either way, so this is chrome geometry only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BarPosition {
    #[default]
    Top,
    Bottom,
}

impl BarPosition {
    /// Config keys as written by the system-interface Browser page.
    fn from_key(key: &str) -> Self {
        match key {
            "bottom" => Self::Bottom,
            _ => Self::Top,
        }
    }
}

/// When the tab line is up: qutebrowser's tab bar, here a strip beside the
/// corner control while the bar is folded (the open bar has its own tab
/// row). The values and their meanings are qutebrowser's `tabs.show`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TabLine {
    Never,
    /// For a moment after the tab in front changes.
    #[default]
    Switching,
    /// Whenever more than one tab is open.
    Multiple,
    Always,
}

impl TabLine {
    /// Config keys as written by the system-interface Browser page.
    fn from_key(key: &str) -> Self {
        match key {
            "never" => Self::Never,
            "multiple" => Self::Multiple,
            "always" => Self::Always,
            _ => Self::Switching,
        }
    }
}

/// The color scheme reported to pages as `prefers-color-scheme`. Sites that
/// ship a dark stylesheet honor it; sites that don't are unaffected — this is
/// a signal, not a filter over their colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorScheme {
    /// Whatever the desktop prefers: the Settings portal's `color-scheme`
    /// (`portal_scheme.rs`), which the system-interface Appearance page sets
    /// for every app at once.
    #[default]
    System,
    Dark,
    Light,
    /// Dark by force: a user stylesheet inverts the page, for sites that
    /// ship no dark theme at all (google.com serves a hardcoded white).
    ForceDark,
}

impl ColorScheme {
    /// Config keys as written by the system-interface Browser page. An
    /// absent or unknown key follows the system.
    fn from_key(key: &str) -> Self {
        match key {
            "dark" => Self::Dark,
            "light" => Self::Light,
            "force-dark" => Self::ForceDark,
            _ => Self::System,
        }
    }

    /// What to report for `prefers-color-scheme`, backend-neutrally, given
    /// what the desktop currently prefers.
    ///
    /// Force-dark reports **light** on purpose: the filter inverts
    /// unconditionally, so a site with a real dark theme would be handed an
    /// already-dark page and inverted back into a light one.
    pub fn is_dark(self, system_dark: bool) -> bool {
        match self {
            Self::System => system_dark,
            Self::Dark => true,
            Self::Light | Self::ForceDark => false,
        }
    }

    /// Whether the inverting user stylesheet is installed.
    pub fn forces_dark(self) -> bool {
        matches!(self, Self::ForceDark)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub homepage: String,
    /// Query-URL prefix for the search fallback; escaped terms are appended.
    pub search_prefix: String,
    /// Override for the download directory (None = XDG default).
    pub download_dir: Option<PathBuf>,
    /// Record page visits to cce://history.
    pub history: bool,
    /// Offer accounts from cce-secrets on login forms. On by default, and a
    /// single switch for the whole feature: with it off the browser injects
    /// no watcher script and never opens the keyring.
    pub accounts: bool,
    /// Sync bookmarks with Raindrop.io's Unsorted collection
    /// (RAINDROP-SYNC.md). Off by default: it needs a token in the keyring,
    /// and it writes to an account elsewhere.
    pub raindrop: bool,
    /// Vi-style modal keys, after qutebrowser (`src/vi.rs`). Off by default:
    /// it changes what every letter typed at a page means.
    pub vi_mode: bool,
    /// Serve the agent tools (`src/agent.rs`) over MCP on loopback, so an
    /// agent can drive the browser. Read at startup only.
    pub agent: bool,
    /// Window edge the utility bar floats against.
    pub bar_position: BarPosition,
    /// When the folded bar shows the tabs anyway (`main.rs`, "the tab line").
    pub tab_line: TabLine,
    /// What pages are told to prefer.
    pub color_scheme: ColorScheme,
    /// Minutes a tab stays out of sight before it is put to sleep — its web
    /// process stopped, the page loaded again when it is shown
    /// (`wpe/host/sleep.rs`). 0 never does.
    pub sleep_tabs_after: u32,
    /// Command used to hand the current page to another browser. Empty means
    /// "ask XDG", which is right until cce-browser is itself the default.
    pub external_browser: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            homepage: DEFAULT_HOMEPAGE.to_string(),
            search_prefix: search_prefix("duckduckgo").to_string(),
            download_dir: None,
            history: true,
            accounts: true,
            raindrop: false,
            vi_mode: false,
            agent: true,
            bar_position: BarPosition::Top,
            tab_line: TabLine::Switching,
            color_scheme: ColorScheme::System,
            sleep_tabs_after: DEFAULT_SLEEP_TABS_AFTER,
            external_browser: None,
        }
    }
}

/// Engine keys as written by the system-interface Browser page.
fn search_prefix(key: &str) -> &'static str {
    match key {
        "google" => "https://www.google.com/search?q=",
        "bing" => "https://www.bing.com/search?q=",
        "wikipedia" => "https://en.wikipedia.org/wiki/Special:Search?search=",
        _ => "https://duckduckgo.com/html/?q=",
    }
}

/// Half an hour out of sight: long enough that a tab being worked between
/// never sleeps, short enough that a day's leftovers stop costing a process
/// each. Edge's default is two hours, Chrome's Memory Saver about the same.
pub const DEFAULT_SLEEP_TABS_AFTER: u32 = 30;

impl Settings {
    /// [`Self::sleep_tabs_after`] as the host takes it.
    pub fn sleep_after(&self) -> Option<std::time::Duration> {
        (self.sleep_tabs_after > 0).then(|| std::time::Duration::from_secs(self.sleep_tabs_after as u64 * 60))
    }
}

pub fn load() -> Settings {
    let path = cce_ui::config::get_app_config_path("cce-browser");
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let val = cce_ui::config::parse_kdl_to_json(&content);
    let b = &val["browser"];

    let homepage = match b["homepage"].as_str().map(str::trim) {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => DEFAULT_HOMEPAGE.to_string(),
    };
    let download_dir = match b["download-dir"].as_str().map(str::trim) {
        Some(d) if !d.is_empty() => {
            let home = std::env::var("HOME").unwrap_or_default();
            Some(PathBuf::from(match d.strip_prefix("~/") {
                Some(rest) => format!("{home}/{rest}"),
                None => d.to_string(),
            }))
        }
        _ => None,
    };
    Settings {
        homepage,
        search_prefix: search_prefix(b["search"].as_str().unwrap_or("duckduckgo")).to_string(),
        download_dir,
        history: b["history"].as_bool().unwrap_or(true),
        accounts: b["accounts"].as_bool().unwrap_or(true),
        raindrop: b["raindrop"].as_bool().unwrap_or(false),
        vi_mode: b["vi-mode"].as_bool().unwrap_or(false),
        agent: b["agent"].as_bool().unwrap_or(true),
        bar_position: BarPosition::from_key(b["bar-position"].as_str().unwrap_or("top")),
        tab_line: TabLine::from_key(b["tab-line"].as_str().unwrap_or("switching")),
        color_scheme: ColorScheme::from_key(b["color-scheme"].as_str().unwrap_or("system")),
        sleep_tabs_after: b["sleep-tabs-after"]
            .as_u64()
            .map_or(DEFAULT_SLEEP_TABS_AFTER, |m| m.min(u32::MAX as u64) as u32),
        external_browser: b["external-browser"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config that never chose a scheme follows the desktop; the explicit
    /// keys the system-interface Browser page writes still override it.
    #[test]
    fn color_scheme_keys_and_the_system_default() {
        assert_eq!(ColorScheme::from_key("system"), ColorScheme::System);
        assert_eq!(ColorScheme::from_key(""), ColorScheme::System);
        assert_eq!(ColorScheme::from_key("dark"), ColorScheme::Dark);
        assert_eq!(ColorScheme::from_key("light"), ColorScheme::Light);
        assert_eq!(ColorScheme::from_key("force-dark"), ColorScheme::ForceDark);
        assert_eq!(Settings::default().color_scheme, ColorScheme::System);
    }

    #[test]
    fn only_system_listens_to_the_desktop() {
        for system_dark in [true, false] {
            assert_eq!(ColorScheme::System.is_dark(system_dark), system_dark);
            assert!(ColorScheme::Dark.is_dark(system_dark));
            assert!(!ColorScheme::Light.is_dark(system_dark));
            assert!(!ColorScheme::ForceDark.is_dark(system_dark), "force-dark inverts a light page");
        }
    }
}
