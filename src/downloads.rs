//! Chrome-side downloads. Servo has no download pipeline, so navigations
//! that target obviously-downloadable files are denied in the delegate and
//! fetched here instead: reqwest workers stream into the user's Downloads
//! directory, and `cce://downloads` renders the store — with a 1s
//! meta-refresh while anything is active, so progress needs no chrome
//! plumbing at all.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use url::Url;

use crate::pages::{html_escape, page};

/// Extensions that download instead of navigating. Servo renders none of
/// these; the common "click a release artifact" cases.
const DOWNLOAD_EXTENSIONS: &[&str] = &[
    "zip", "tar", "gz", "tgz", "xz", "bz2", "7z", "rar", "pdf", "iso", "img", "deb", "rpm",
    "exe", "msi", "dmg", "appimage", "bin", "apk", "jar", "flatpak",
];

pub fn is_download_url(url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let path = url.path().to_ascii_lowercase();
    DOWNLOAD_EXTENSIONS
        .iter()
        .any(|ext| path.ends_with(&format!(".{ext}")))
}

#[derive(Clone, PartialEq)]
pub enum State {
    Active,
    Done,
    Failed(String),
}

pub struct Download {
    /// Stable handle for worker updates — `clear_finished` shifts Vec
    /// positions, so indices must never cross a lock boundary.
    id: u64,
    pub ts: u64,
    pub url: String,
    pub filename: String,
    pub path: PathBuf,
    pub received: u64,
    pub total: Option<u64>,
    pub state: State,
}

#[derive(Default)]
pub struct Downloads {
    items: Mutex<Vec<Download>>,
    next_id: std::sync::atomic::AtomicU64,
}

/// Settings override for the download directory (None = XDG default).
/// A global because downloads run on worker threads.
static DIR_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn set_download_dir(dir: Option<PathBuf>) {
    *DIR_OVERRIDE.lock().unwrap() = dir;
}

/// The user's download directory: the settings override when set, else
/// XDG_DOWNLOAD_DIR from user-dirs.dirs, else ~/Downloads.
fn download_dir() -> PathBuf {
    if let Some(dir) = DIR_OVERRIDE.lock().unwrap().clone() {
        return dir;
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let conf = home.join(".config/user-dirs.dirs");
    if let Ok(text) = std::fs::read_to_string(conf) {
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("XDG_DOWNLOAD_DIR=") {
                let value = rest.trim_matches('"').replace("$HOME", &home.to_string_lossy());
                if !value.is_empty() {
                    return PathBuf::from(value);
                }
            }
        }
    }
    home.join("Downloads")
}

/// Minimal percent-decode for display filenames; anything path-hostile
/// falls back untouched.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn filename_for(url: &Url) -> String {
    let name = url
        .path_segments()
        .and_then(|mut s| s.next_back().map(str::to_string))
        .map(|s| percent_decode(&s))
        .unwrap_or_default();
    let name = name.replace(['/', '\0'], "_");
    if name.is_empty() { "download".to_string() } else { name }
}

/// `name.ext` → `name.1.ext` … until the path is free.
fn unique_path(dir: &PathBuf, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (filename.to_string(), String::new()),
    };
    for n in 1.. {
        let candidate = dir.join(format!("{stem}.{n}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[unit]) }
}

impl Downloads {
    /// Start fetching `url` on a worker thread.
    pub fn start(self: &Arc<Self>, url: Url) {
        let dir = download_dir();
        let _ = std::fs::create_dir_all(&dir);
        let filename = filename_for(&url);
        let path = unique_path(&dir, &filename);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.items.lock().unwrap().push(Download {
            id,
            ts,
            url: url.to_string(),
            filename: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(filename),
            path: path.clone(),
            received: 0,
            total: None,
            state: State::Active,
        });

        let store = self.clone();
        std::thread::spawn(move || {
            let result = store.fetch(id, url, path);
            store.with_item(id, |item| {
                item.state = match result {
                    Ok(()) => State::Done,
                    Err(e) => State::Failed(e),
                };
            });
        });
    }

    fn with_item(&self, id: u64, f: impl FnOnce(&mut Download)) {
        let mut items = self.items.lock().unwrap();
        if let Some(item) = items.iter_mut().find(|d| d.id == id) {
            f(item);
        }
    }

    fn fetch(&self, id: u64, url: Url, path: PathBuf) -> Result<(), String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("cce-browser/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| e.to_string())?;
        let mut resp = client.get(url).send().map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let total = resp.content_length();
        self.with_item(id, |item| item.total = total);
        let mut file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
        let mut buf = [0u8; 64 * 1024];
        let mut received: u64 = 0;
        loop {
            let n = resp.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            received += n as u64;
            self.with_item(id, |item| item.received = received);
        }
        Ok(())
    }

    /// Drop finished/failed entries (files stay on disk).
    pub fn clear_finished(&self) {
        self.items.lock().unwrap().retain(|d| d.state == State::Active);
    }

    pub fn html(&self) -> String {
        let items = self.items.lock().unwrap();
        let any_active = items.iter().any(|d| d.state == State::Active);
        let mut rows = String::new();
        for d in items.iter().rev() {
            let progress = match (&d.state, d.total) {
                (State::Active, Some(total)) if total > 0 => format!(
                    "{} / {} ({}%)",
                    human_size(d.received),
                    human_size(total),
                    d.received * 100 / total
                ),
                (State::Active, _) => format!("{}...", human_size(d.received)),
                (State::Done, _) => human_size(d.received),
                (State::Failed(e), _) => format!("failed: {}", html_escape(e)),
            };
            rows.push_str(&format!(
                "<div class=e><span class=w data-ts=\"{}\"></span>\
                 <a href=\"file://{}\">{}</a><span class=u>{}</span>\
                 <span class=w style=\"min-width:0\">{}</span></div>\n",
                d.ts,
                html_escape(&d.path.to_string_lossy()),
                html_escape(&d.filename),
                html_escape(&d.url),
                progress,
            ));
        }
        let meta = format!(
            "{} downloads<a href=\"cce://downloads/clear\">clear finished</a>",
            items.len()
        );
        let body = if items.is_empty() {
            "<p class=empty>No downloads yet. Links to archives and binaries download here.</p>"
                .to_string()
        } else {
            rows
        };
        // Self-refresh while transfers run; static once everything settled.
        let head = if any_active { "<meta http-equiv=\"refresh\" content=\"1\">" } else { "" };
        page("Downloads", &meta, &body, head)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The argv case: a release-artifact URL must be recognized before it is
    /// ever handed to Servo. `request_navigation` does not fire for a URL the
    /// embedder supplies, so `ServoHost::take_as_download` is the only thing
    /// standing between this and Servo's "Unknown content type" page.
    #[test]
    fn download_urls_are_recognized_by_extension() {
        for u in [
            "https://example.com/rel/app-1.2.3.tar.gz",
            "http://127.0.0.1:8740/big.bin",
            "https://example.com/Installer.EXE",
            "https://example.com/x.zip?token=abc",
        ] {
            assert!(is_download_url(&Url::parse(u).unwrap()), "should download: {u}");
        }
    }

    #[test]
    fn ordinary_pages_and_non_http_schemes_are_not_downloads() {
        for u in [
            "https://www.cloudflare.com/",
            "https://example.com/page.html",
            "https://example.com/binary",       // no extension: navigates
            "cce://downloads",
            "file:///home/me/x.zip",            // only http(s) is fetched here
        ] {
            assert!(!is_download_url(&Url::parse(u).unwrap()), "should not download: {u}");
        }
    }
}
