//! Browser settings from the per-app cce config
//! (`~/.config/cce/cce-browser/config.kdl`, section `browser`) — the file
//! cce-system-interface's Browser page edits. Loaded at startup and
//! re-read when the window regains focus, so settings changed in
//! system-interface apply on the next switch back to the browser.

use std::path::PathBuf;

pub const DEFAULT_HOMEPAGE: &str = "https://servo.org";

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub homepage: String,
    /// Query-URL prefix for the search fallback; escaped terms are appended.
    pub search_prefix: String,
    /// Override for the download directory (None = XDG default).
    pub download_dir: Option<PathBuf>,
    /// Record page visits to cce://history.
    pub history: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            homepage: DEFAULT_HOMEPAGE.to_string(),
            search_prefix: search_prefix("duckduckgo").to_string(),
            download_dir: None,
            history: true,
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
    }
}
