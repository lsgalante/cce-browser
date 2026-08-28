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

/// The color scheme reported to pages as `prefers-color-scheme`. Sites that
/// ship a dark stylesheet honor it; sites that don't are unaffected — this is
/// a signal, not a filter over their colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorScheme {
    #[default]
    Dark,
    Light,
    /// Dark by force: a user stylesheet inverts the page, for sites that
    /// ship no dark theme at all (google.com serves a hardcoded white).
    ForceDark,
}

impl ColorScheme {
    /// Config keys as written by the system-interface Browser page.
    fn from_key(key: &str) -> Self {
        match key {
            "light" => Self::Light,
            "force-dark" => Self::ForceDark,
            _ => Self::Dark,
        }
    }

    /// What to report for `prefers-color-scheme`, backend-neutrally.
    ///
    /// Force-dark reports **light** on purpose: the filter inverts
    /// unconditionally, so a site with a real dark theme would be handed an
    /// already-dark page and inverted back into a light one.
    pub fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }

    /// Whether the inverting user stylesheet is installed.
    pub fn forces_dark(self) -> bool {
        matches!(self, Self::ForceDark)
    }
}

impl From<ColorScheme> for servo::Theme {
    fn from(scheme: ColorScheme) -> Self {
        match scheme {
            ColorScheme::Dark => servo::Theme::Dark,
            // Force-dark inverts unconditionally, so pages have to render
            // their LIGHT theme underneath: reporting dark to a site that
            // has one would hand the filter an already-dark page and invert
            // it back into a light one.
            ColorScheme::Light | ColorScheme::ForceDark => servo::Theme::Light,
        }
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
    /// Window edge the utility bar floats against.
    pub bar_position: BarPosition,
    /// What pages are told to prefer.
    pub color_scheme: ColorScheme,
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
            bar_position: BarPosition::Top,
            color_scheme: ColorScheme::Dark,
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
        bar_position: BarPosition::from_key(b["bar-position"].as_str().unwrap_or("top")),
        color_scheme: ColorScheme::from_key(b["color-scheme"].as_str().unwrap_or("dark")),
        external_browser: b["external-browser"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    }
}
