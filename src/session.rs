//! Open-tab persistence: the tab set survives a restart.
//!
//! `~/.local/state/cce/browser/tabs.tsv` (the same state dir as history and
//! bookmarks), one line per tab in strip order: `<1|0>\t<url>`, the flag
//! marking the active tab. It is written eagerly on every tab-set change —
//! open, close, switch, navigation — rather than on exit, so a crash or a
//! compositor-side window close loses nothing; `save` skips the write when
//! the serialization is unchanged, which keeps the loading-time signal storm
//! from touching the disk more than once. Closing the last tab saves an
//! empty set, so a deliberately emptied browser starts fresh on the
//! homepage rather than resurrecting what was just closed.

use std::path::PathBuf;

use url::Url;

pub struct Session {
    path: PathBuf,
    /// Last serialization written (or loaded), to skip no-op writes.
    last: Option<String>,
}

impl Session {
    pub fn new() -> Self {
        Self::at(crate::pages::state_dir().join("tabs.tsv"))
    }

    fn at(path: PathBuf) -> Self {
        Self { path, last: None }
    }

    /// Tabs saved by the previous run, in strip order, plus the active
    /// index. Missing file or unparseable lines mean fewer tabs, never an
    /// error; an empty result is "nothing to restore".
    pub fn load(&mut self) -> (Vec<Url>, usize) {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        let mut tabs = Vec::new();
        let mut active = 0;
        for line in text.lines() {
            let mut parts = line.splitn(2, '\t');
            if let (Some(flag), Some(url)) = (parts.next(), parts.next()) {
                if let Ok(u) = Url::parse(url) {
                    if flag == "1" {
                        active = tabs.len();
                    }
                    tabs.push(u);
                }
            }
        }
        self.last = Some(text);
        (tabs, active)
    }

    /// Persist the open tabs; a no-op when nothing changed since the last
    /// write.
    pub fn save(&mut self, tabs: &[(String, bool)]) {
        let mut out = String::new();
        for (url, active) in tabs {
            out.push_str(if *active { "1\t" } else { "0\t" });
            out.push_str(url);
            out.push('\n');
        }
        if self.last.as_deref() == Some(&out) {
            return;
        }
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(&self.path, &out).is_ok() {
            self.last = Some(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_session(name: &str) -> Session {
        let dir = std::env::temp_dir().join("cce-browser-session-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        Session::at(path)
    }

    #[test]
    fn round_trips_tabs_and_active_index() {
        let mut s = temp_session("round-trip.tsv");
        s.save(&[
            ("https://example.com/".to_string(), false),
            ("https://example.org/".to_string(), true),
        ]);

        let mut fresh = Session::at(s.path.clone());
        let (tabs, active) = fresh.load();
        assert_eq!(
            tabs.iter().map(Url::as_str).collect::<Vec<_>>(),
            ["https://example.com/", "https://example.org/"]
        );
        assert_eq!(active, 1);
    }

    #[test]
    fn empty_save_clears_and_loads_as_nothing() {
        let mut s = temp_session("empty.tsv");
        s.save(&[("https://example.com/".to_string(), true)]);
        s.save(&[]);

        let (tabs, active) = Session::at(s.path.clone()).load();
        assert!(tabs.is_empty());
        assert_eq!(active, 0);
    }

    #[test]
    fn missing_file_and_junk_lines_load_as_fewer_tabs() {
        let mut s = temp_session("missing.tsv");
        assert!(s.load().0.is_empty());

        std::fs::write(
            &s.path,
            "no-tab-here\n1\tnot a url\n0\thttps://example.com/\n",
        )
        .unwrap();
        let (tabs, active) = Session::at(s.path.clone()).load();
        assert_eq!(tabs.len(), 1);
        assert_eq!(active, 0);
    }
}
