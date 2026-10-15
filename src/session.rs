//! Open-tab persistence: the tab set survives a restart.
//!
//! `~/.local/state/cce/browser/tabs.tsv` (the same state dir as history and
//! bookmarks), one line per tab in strip order: `<1|0>\t<url>\t<title>`,
//! the flag marking the active tab. The title is what a tab restored asleep
//! (`wpe/host/sleep.rs`) shows until it is loaded; a line without one — the
//! format before titles — restores with none. It is written eagerly on every tab-set change —
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

    /// Tabs saved by the previous run, in strip order, with their titles,
    /// plus the active index. Missing file or unparseable lines mean fewer
    /// tabs, never an error; an empty result is "nothing to restore".
    pub fn load(&mut self) -> (Vec<(Url, Option<String>)>, usize) {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        let mut tabs = Vec::new();
        let mut active = 0;
        for line in text.lines() {
            let mut parts = line.splitn(3, '\t');
            if let (Some(flag), Some(url)) = (parts.next(), parts.next()) {
                if let Ok(u) = Url::parse(url) {
                    if flag == "1" {
                        active = tabs.len();
                    }
                    let title = parts.next().filter(|t| !t.is_empty()).map(str::to_string);
                    tabs.push((u, title));
                }
            }
        }
        self.last = Some(text);
        (tabs, active)
    }

    /// Persist the open tabs — url, whether active, title; a no-op when
    /// nothing changed since the last write.
    pub fn save(&mut self, tabs: &[(String, bool, Option<String>)]) {
        let mut out = String::new();
        for (url, active, title) in tabs {
            out.push_str(if *active { "1\t" } else { "0\t" });
            out.push_str(url);
            out.push('\t');
            // The format has no escaping, as `pages::sanitize` says.
            let title = title.as_deref().unwrap_or_default();
            out.extend(title.chars().map(|c| if matches!(c, '\t' | '\n' | '\r') { ' ' } else { c }));
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

    /// A session file in a scratch directory of this PROCESS's own.
    ///
    /// The name was fixed — `/tmp/cce-browser-session-test` — where the
    /// sibling helpers in `pages.rs` already scope theirs by pid. /tmp is one
    /// namespace shared by every user of the machine, so a fixed name belongs
    /// to whoever ran first and the sticky bit denies it to everyone else;
    /// nearer to hand, two checkouts running their suites at once shared one
    /// directory.
    ///
    /// The directory is deliberately NOT removed wholesale, here or at the
    /// end of a test: the three tests run in parallel threads of one process
    /// and so share this one pid-scoped directory, and a `remove_dir_all`
    /// would take a sibling's file out from under it. Each test owns a
    /// distinct file name and clears just that.
    fn temp_session(name: &str) -> Session {
        let dir = std::env::temp_dir()
            .join(format!("cce-browser-session-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        Session::at(path)
    }

    #[test]
    fn round_trips_tabs_and_active_index() {
        let mut s = temp_session("round-trip.tsv");
        s.save(&[
            ("https://example.com/".to_string(), false, Some("Example\tDomain".to_string())),
            ("https://example.org/".to_string(), true, None),
        ]);

        let mut fresh = Session::at(s.path.clone());
        let (tabs, active) = fresh.load();
        assert_eq!(
            tabs.iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>(),
            ["https://example.com/", "https://example.org/"]
        );
        // A tab in a title would have split the line; it is a space instead.
        assert_eq!(tabs[0].1.as_deref(), Some("Example Domain"));
        assert_eq!(tabs[1].1, None);
        assert_eq!(active, 1);
    }

    #[test]
    fn empty_save_clears_and_loads_as_nothing() {
        let mut s = temp_session("empty.tsv");
        s.save(&[("https://example.com/".to_string(), true, None)]);
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

    #[test]
    fn a_file_from_before_titles_restores_untitled() {
        let s = temp_session("untitled.tsv");
        std::fs::write(&s.path, "0\thttps://example.com/\n1\thttps://example.org/\n").unwrap();
        let (tabs, active) = Session::at(s.path.clone()).load();
        assert_eq!(tabs.len(), 2);
        assert!(tabs.iter().all(|(_, t)| t.is_none()));
        assert_eq!(active, 1);
    }
}
