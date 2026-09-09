//! Internal `cce:` pages and their backing stores.
//!
//! History, bookmarks and favorites live as TSV files under the XDG state
//! dir (`~/.local/state/cce/browser/`), with in-memory copies for rendering.
//! The `cce:` protocol handler serves them back as real pages —
//! `cce://history` and `cce://bookmarks` are fetched through Servo's
//! network stack and rendered like any other page, so entries are
//! ordinary links (including the mutating clear/remove actions).
//!
//! The handler runs on Servo's fetch threads, hence the `Arc<Mutex<_>>`
//! stores shared with the main thread.

use std::fs::{self, OpenOptions};
#[cfg(feature = "servo")]
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
#[cfg(feature = "servo")]
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(feature = "servo")]
use servo::protocol_handler::{
    DoneChannel, FetchContext, HttpStatus, NetworkError, ProtocolHandler, Request, Response,
    ResponseBody, ResourceFetchTiming,
};

/// Render at most this many entries on the history page.
const RENDER_CAP: usize = 500;

#[derive(Clone)]
struct Entry {
    ts: u64,
    url: String,
    title: String,
}

/// `~/.local/state/cce/browser` — history and bookmarks live here directly,
/// Servo's own persisted state in a `profile` subdirectory under it.
pub(crate) fn state_dir() -> PathBuf {
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state"),
    };
    base.join("cce").join("browser")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One-line-safe field: the TSV logs separate with tabs and newlines.
fn sanitize(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn read_tsv(path: &PathBuf) -> Vec<Entry> {
    let mut entries = Vec::new();
    if let Ok(text) = fs::read_to_string(path) {
        for line in text.lines() {
            let mut parts = line.splitn(3, '\t');
            if let (Some(ts), Some(url), Some(title)) = (parts.next(), parts.next(), parts.next())
            {
                if let Ok(ts) = ts.parse() {
                    entries.push(Entry { ts, url: url.to_string(), title: title.to_string() });
                }
            }
        }
    }
    entries
}

fn write_tsv(path: &PathBuf, entries: &[Entry]) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let mut out = String::new();
    for e in entries {
        out.push_str(&format!("{}\t{}\t{}\n", e.ts, e.url, e.title));
    }
    let _ = fs::write(path, out);
}

/// Shared page skeleton for the internal pages (dark, DE-toned).
/// `head_extra` lands in <head> (e.g. a refresh tag for live pages).
pub(crate) fn page(title: &str, meta: &str, body: &str, head_extra: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>{title}</title>{head_extra}<style>\
         :root{{color-scheme:dark}}\
         body{{background:#1a1b1d;color:#dcdce1;font-family:sans-serif;margin:0;padding:28px 36px}}\
         h1{{font-size:20px;font-weight:600;margin:0 0 4px}}\
         .meta{{color:#8a8c92;font-size:13px;margin-bottom:20px}}\
         .meta a{{color:#7fa3d4;text-decoration:none;margin-left:12px}}\
         .e{{display:flex;gap:14px;padding:7px 10px;border-radius:8px;align-items:baseline}}\
         .e:hover{{background:#232427}}\
         .w{{color:#8a8c92;font-size:12px;min-width:11em}}\
         .e a{{color:#dcdce1;text-decoration:none;white-space:nowrap;overflow:hidden;\
               text-overflow:ellipsis;max-width:40%}}\
         .e a:hover{{color:#9fc1ea}}\
         .u{{color:#6f7177;font-size:12px;white-space:nowrap;overflow:hidden;\
             text-overflow:ellipsis;flex:1}}\
         .e a.rm{{color:#6f7177;font-size:12px;max-width:none}}\
         .e a.rm:hover{{color:#d49b9b}}\
         .e .tag{{color:#7fa3d4;font-size:12px}}\
         .empty{{color:#8a8c92}}\
         </style></head><body>\
         <h1>{title}</h1>\
         <div class=meta>{meta}</div>\
         {body}\
         <script>for(const el of document.querySelectorAll('[data-ts]')){{\
         const d=new Date(1000*+el.dataset.ts);\
         el.textContent=d.toLocaleDateString()+'  '+\
         d.toLocaleTimeString([],{{hour:'2-digit',minute:'2-digit'}});}}</script>\
         </body></html>"
    )
}

pub struct History {
    entries: Mutex<Vec<Entry>>,
    path: PathBuf,
}

impl History {
    /// Load the log from the state dir (missing file = empty history).
    pub fn load() -> Self {
        let path = state_dir().join("history.tsv");
        Self { entries: Mutex::new(read_tsv(&path)), path }
    }

    /// Record a completed page load. Internal pages and immediate
    /// duplicates (reload spam) are skipped.
    pub fn record(&self, url: &str, title: &str) {
        if url.starts_with("cce:") || url == "about:blank" {
            return;
        }
        let mut entries = self.entries.lock().unwrap();
        if entries.last().is_some_and(|last| last.url == url) {
            return;
        }
        let entry = Entry { ts: now(), url: sanitize(url), title: sanitize(title) };
        if let Some(dir) = self.path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{}\t{}\t{}", entry.ts, entry.url, entry.title);
        }
        entries.push(entry);
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
        let _ = fs::write(&self.path, "");
    }

    fn html(&self) -> String {
        let entries = self.entries.lock().unwrap();
        let mut rows = String::new();
        for e in entries.iter().rev().take(RENDER_CAP) {
            let title = if e.title.trim().is_empty() { &e.url } else { &e.title };
            rows.push_str(&format!(
                "<div class=e><span class=w data-ts=\"{}\"></span>\
                 <a href=\"{}\">{}</a><span class=u>{}</span></div>\n",
                e.ts,
                html_escape(&e.url),
                html_escape(title),
                html_escape(&e.url),
            ));
        }
        let meta = format!(
            "{} entries<a href=\"cce://history/clear\">clear</a>",
            entries.len()
        );
        let body = if entries.is_empty() {
            "<p class=empty>No history yet.</p>".to_string()
        } else {
            rows
        };
        page("History", &meta, &body, "")
    }
}

pub struct Bookmarks {
    entries: Mutex<Vec<Entry>>,
    path: PathBuf,
}

impl Bookmarks {
    pub fn load() -> Self {
        let path = state_dir().join("bookmarks.tsv");
        Self { entries: Mutex::new(read_tsv(&path)), path }
    }

    pub fn contains(&self, url: &str) -> bool {
        self.entries.lock().unwrap().iter().any(|e| e.url == url)
    }

    /// Add or remove a bookmark for `url`; returns true when it is now
    /// bookmarked.
    pub fn toggle(&self, url: &str, title: &str) -> bool {
        if url.starts_with("cce:") || url == "about:blank" {
            return false;
        }
        let mut entries = self.entries.lock().unwrap();
        let added = if let Some(i) = entries.iter().position(|e| e.url == url) {
            entries.remove(i);
            false
        } else {
            entries.push(Entry { ts: now(), url: sanitize(url), title: sanitize(title) });
            true
        };
        write_tsv(&self.path, &entries);
        added
    }

    pub fn remove(&self, url: &str) {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|e| e.url != url);
        write_tsv(&self.path, &entries);
    }

    /// The title a bookmark was saved with, for promoting it to a favorite
    /// from the bookmarks page without re-fetching anything.
    pub fn title_of(&self, url: &str) -> Option<String> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.url == url)
            .map(|e| e.title.clone())
    }

    fn html(&self, favorites: &Favorites) -> String {
        let entries = self.entries.lock().unwrap();
        let mut rows = String::new();
        for e in entries.iter().rev() {
            let title = if e.title.trim().is_empty() { &e.url } else { &e.title };
            let enc = url_encode(&e.url);
            // A bookmark that is already a favorite says so instead of
            // offering to add it twice.
            let fav = if favorites.contains(&e.url) {
                "<span class=tag>favorite</span>".to_string()
            } else {
                format!("<a class=rm href=\"cce://favorites/add?url={}\">favorite</a>", html_escape(&enc))
            };
            rows.push_str(&format!(
                "<div class=e><span class=w data-ts=\"{}\"></span>\
                 <a href=\"{}\">{}</a><span class=u>{}</span>{fav}\
                 <a class=rm href=\"cce://bookmarks/remove?url={}\">remove</a></div>\n",
                e.ts,
                html_escape(&e.url),
                html_escape(title),
                html_escape(&e.url),
                html_escape(&enc),
            ));
        }
        let meta = format!(
            "{} bookmarks<a href=\"cce://favorites\">favorites</a>",
            entries.len()
        );
        let body = if entries.is_empty() {
            "<p class=empty>No bookmarks yet. Star a page or press Ctrl+D.</p>".to_string()
        } else {
            rows
        };
        page("Bookmarks", &meta, &body, "")
    }
}

fn url_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// A favorite as the chrome shows it: the pill's label and where it goes.
#[derive(Clone, Debug, PartialEq)]
pub struct Favorite {
    pub url: String,
    pub label: String,
}

/// The label a favorite gets when it is added: the page title, or — for an
/// untitled page — the host with any `www.` shorn off (the file name, for
/// a `file:` URL, which has no host), so a pill never reads as a full URL.
fn default_label(url: &str, title: &str) -> String {
    let title = title.trim();
    if !title.is_empty() {
        return title.to_string();
    }
    let Ok(u) = url::Url::parse(url) else { return url.to_string() };
    let host = u
        .host_str()
        .map(|h| h.trim_start_matches("www.").to_string())
        .filter(|h| !h.is_empty());
    let file = u
        .path_segments()
        .and_then(|mut segs| segs.next_back().map(str::to_string))
        .filter(|f| !f.is_empty());
    host.or(file).unwrap_or_else(|| url.to_string())
}

/// The favorites: a short, ordered, hand-curated list of places, shown as a
/// row of pills in the utility bar. Deliberately not the bookmarks — the
/// star is an archive of everything worth finding again; this is the
/// handful of sites worth a permanent one-click spot. Insertion order is
/// strip order, and the `cce://favorites` page reorders, renames and
/// removes.
pub struct Favorites {
    entries: Mutex<Vec<Entry>>,
    path: PathBuf,
}

impl Favorites {
    pub fn load() -> Self {
        let path = state_dir().join("favorites.tsv");
        Self { entries: Mutex::new(read_tsv(&path)), path }
    }

    /// The strip, in order.
    pub fn snapshot(&self) -> Vec<Favorite> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .map(|e| Favorite { url: e.url.clone(), label: default_label(&e.url, &e.title) })
            .collect()
    }

    pub fn contains(&self, url: &str) -> bool {
        self.entries.lock().unwrap().iter().any(|e| e.url == url)
    }

    /// Add `url` to the end of the strip, or do nothing if it is there.
    /// Internal pages are refused — a favorite pointing at a blank tab
    /// helps nobody.
    pub fn add(&self, url: &str, title: &str) {
        if url.starts_with("cce:") || url == "about:blank" {
            return;
        }
        let mut entries = self.entries.lock().unwrap();
        if entries.iter().any(|e| e.url == url) {
            return;
        }
        entries.push(Entry {
            ts: now(),
            url: sanitize(url),
            title: sanitize(&default_label(url, title)),
        });
        write_tsv(&self.path, &entries);
    }

    /// Add or remove `url`; returns true when it is now a favorite.
    pub fn toggle(&self, url: &str, title: &str) -> bool {
        if self.contains(url) {
            self.remove(url);
            false
        } else {
            self.add(url, title);
            self.contains(url)
        }
    }

    pub fn remove(&self, url: &str) {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|e| e.url != url);
        write_tsv(&self.path, &entries);
    }

    pub fn rename(&self, url: &str, title: &str) {
        let mut entries = self.entries.lock().unwrap();
        if let Some(e) = entries.iter_mut().find(|e| e.url == url) {
            e.title = sanitize(&default_label(url, title));
            write_tsv(&self.path, &entries);
        }
    }

    /// Move `url` one place toward the front (`-1`) or the back (`1`).
    pub fn shift(&self, url: &str, delta: isize) {
        let mut entries = self.entries.lock().unwrap();
        let Some(i) = entries.iter().position(|e| e.url == url) else { return };
        let j = i as isize + delta;
        if j < 0 || j >= entries.len() as isize {
            return;
        }
        entries.swap(i, j as usize);
        write_tsv(&self.path, &entries);
    }

    fn html(&self) -> String {
        let entries = self.entries.lock().unwrap();
        let mut rows = String::new();
        let last = entries.len().saturating_sub(1);
        for (i, e) in entries.iter().enumerate() {
            let enc = url_encode(&e.url);
            let label = default_label(&e.url, &e.title);
            // Ordering links; the end pill has nowhere further to go.
            let up = if i > 0 {
                format!("<a class=rm href=\"cce://favorites/up?url={}\">&#9650;</a>", html_escape(&enc))
            } else {
                "<span class=rm>&#9650;</span>".to_string()
            };
            let down = if i < last {
                format!("<a class=rm href=\"cce://favorites/down?url={}\">&#9660;</a>", html_escape(&enc))
            } else {
                "<span class=rm>&#9660;</span>".to_string()
            };
            rows.push_str(&format!(
                "<div class=e><span class=w>{up} {down}</span>\
                 <a href=\"{url}\">{label}</a><span class=u>{url}</span>\
                 <form action=\"cce://favorites/rename\">\
                 <input type=hidden name=url value=\"{url}\">\
                 <input name=title value=\"{label}\" size=18>\
                 <button>rename</button></form>\
                 <a class=rm href=\"cce://favorites/remove?url={enc}\">remove</a></div>\n",
                url = html_escape(&e.url),
                label = html_escape(&label),
                enc = html_escape(&enc),
            ));
        }
        let meta = format!(
            "{} favorites<a href=\"cce://bookmarks\">bookmarks</a>",
            entries.len()
        );
        let body = if entries.is_empty() {
            "<p class=empty>No favorites yet. Press Ctrl+Shift+D on a page, pick \
             \"Add to Favorites\" from its right-click menu, or promote a bookmark.</p>"
                .to_string()
        } else {
            rows
        };
        page("Favorites", &meta, &body, FAVORITES_CSS)
    }
}

/// The rename form's styling, on top of the shared skeleton.
const FAVORITES_CSS: &str = "<style>\
    .e .w{min-width:3em}\
    .e form{display:flex;gap:6px;margin:0}\
    .e input{background:#111214;color:#dcdce1;border:1px solid #2c2d31;border-radius:5px;\
             padding:2px 6px;font-size:12px;width:9em}\
    .e button{background:#232427;color:#8a8c92;border:1px solid #2c2d31;border-radius:5px;\
              padding:2px 8px;font-size:12px;cursor:pointer}\
    .e button:hover{color:#dcdce1}\
    .w a{margin-right:4px}\
    </style>";

/// `cce:` scheme: internal pages served straight out of the app.
pub struct CceProtocol {
    pub history: Arc<History>,
    pub bookmarks: Arc<Bookmarks>,
    pub favorites: Arc<Favorites>,
    pub downloads: Arc<crate::downloads::Downloads>,
    /// Raised by cce://cookies/clear. The handler runs on fetch threads and
    /// cannot reach Servo, so it flags the request and the app's next pump
    /// performs the clear through the SiteDataManager.
    pub clear_cookies: Arc<std::sync::atomic::AtomicBool>,
}

/// Confirmation page for clearing cookies. Deliberately a page with a link
/// rather than a chord that acts immediately: logins persist now, so an
/// accidental keystroke would sign the user out of everything.
fn cookies_page() -> String {
    page(
        "Cookies",
        "Signed-in sessions live here",
        "<div class=e><span class=w></span><span class=u>Clearing cookies signs you out of          every site and cannot be undone. Bookmarks and history are untouched.</span></div>         <div class=e><span class=w></span>         <a class=rm href=\"cce://cookies/clear\">Clear all cookies</a></div>",
        "",
    )
}

fn cookies_cleared_page() -> String {
    page(
        "Cookies",
        "Cleared",
        "<div class=e><span class=w></span><span class=u>All cookies were cleared.          Sites you were signed in to will ask you to sign in again.</span></div>",
        "",
    )
}

impl CceProtocol {
    /// Route a `cce:` URL to its page. Shared by both engine backends —
    /// Servo reaches it through `ProtocolHandler` below, WebKit through its
    /// URI-scheme callback — so the table of pages exists once.
    ///
    /// `None` means no such page; the caller turns that into its engine's
    /// idea of a failed load.
    pub(crate) fn route(&self, url: &str) -> Option<String> {
        let full = url.trim_start_matches("cce://");
        let (path, query) = full.split_once('?').unwrap_or((full, ""));
        let param = |key: &str| -> Option<String> {
            url::form_urlencoded::parse(query.as_bytes())
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        };
        match path.trim_end_matches('/') {
            "history" => Some(self.history.html()),
            "history/clear" => {
                self.history.clear();
                Some(self.history.html())
            }
            "bookmarks" => Some(self.bookmarks.html(&self.favorites)),
            "bookmarks/remove" => {
                if let Some(target) = param("url") {
                    self.bookmarks.remove(&target);
                }
                Some(self.bookmarks.html(&self.favorites))
            }
            "favorites" => Some(self.favorites.html()),
            // Adding lands on the favorites page so the new pill's place in
            // the strip is visible right away. A bookmark promoted without a
            // title in the query keeps the title it was starred with.
            "favorites/add" => {
                if let Some(target) = param("url") {
                    let title = param("title")
                        .or_else(|| self.bookmarks.title_of(&target))
                        .unwrap_or_default();
                    self.favorites.add(&target, &title);
                }
                Some(self.favorites.html())
            }
            "favorites/remove" => {
                if let Some(target) = param("url") {
                    self.favorites.remove(&target);
                }
                Some(self.favorites.html())
            }
            "favorites/up" | "favorites/down" => {
                if let Some(target) = param("url") {
                    let delta = if path.ends_with("up") { -1 } else { 1 };
                    self.favorites.shift(&target, delta);
                }
                Some(self.favorites.html())
            }
            "favorites/rename" => {
                if let Some(target) = param("url") {
                    self.favorites.rename(&target, &param("title").unwrap_or_default());
                }
                Some(self.favorites.html())
            }
            "downloads" => Some(self.downloads.html()),
            "downloads/clear" => {
                self.downloads.clear_finished();
                Some(self.downloads.html())
            }
            "cookies" => Some(cookies_page()),
            "cookies/clear" => {
                self.clear_cookies.store(true, std::sync::atomic::Ordering::SeqCst);
                Some(cookies_cleared_page())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Favorites {
        let dir = std::env::temp_dir().join(format!("cce-browser-favs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Favorites { entries: Mutex::new(Vec::new()), path: dir.join("favorites.tsv") }
    }

    #[test]
    fn labels_fall_back_to_host_then_file_name() {
        assert_eq!(default_label("https://www.example.com/a", "Example"), "Example");
        assert_eq!(default_label("https://www.example.com/a", "  "), "example.com");
        assert_eq!(default_label("file:///home/me/page.html", ""), "page.html");
        assert_eq!(default_label("about:blank", ""), "about:blank");
    }

    #[test]
    fn strip_order_is_insertion_order_and_shifts_move_one_place() {
        let f = store();
        f.add("https://a.example/", "A");
        f.add("https://b.example/", "B");
        f.add("https://c.example/", "C");
        f.add("https://b.example/", "again"); // already there: no duplicate
        let labels = |f: &Favorites| f.snapshot().iter().map(|x| x.label.clone()).collect::<Vec<_>>();
        assert_eq!(labels(&f), ["A", "B", "C"]);
        f.shift("https://c.example/", -1);
        assert_eq!(labels(&f), ["A", "C", "B"]);
        f.shift("https://a.example/", -1); // already first: stays
        assert_eq!(labels(&f), ["A", "C", "B"]);
        f.rename("https://c.example/", "Sea");
        assert_eq!(labels(&f), ["A", "Sea", "B"]);
        assert!(!f.toggle("https://a.example/", "A"));
        assert!(f.toggle("https://d.example/", "D"));
        assert_eq!(labels(&f), ["Sea", "B", "D"]);

        // Round-trips through the file.
        let back = Favorites { entries: Mutex::new(read_tsv(&f.path)), path: f.path.clone() };
        assert_eq!(back.snapshot(), f.snapshot());
        let _ = fs::remove_dir_all(f.path.parent().unwrap());
    }

    #[test]
    fn internal_pages_are_refused() {
        let f = store();
        f.add("cce://history", "History");
        f.add("about:blank", "");
        assert!(f.snapshot().is_empty());
    }
}

#[cfg(feature = "servo")]
impl ProtocolHandler for CceProtocol {
    fn load(
        &self,
        request: &mut Request,
        _done_chan: &mut DoneChannel,
        _context: &FetchContext,
    ) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        let url = request.current_url();
        let body = self.route(url.as_str());
        let response = match body {
            Some(html) => {
                let mut response =
                    Response::new(url, ResourceFetchTiming::new(request.timing_type()));
                *response.body.lock() = ResponseBody::Done(html.into_bytes());
                response.headers.insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("text/html; charset=utf-8"),
                );
                response.status = HttpStatus::default();
                response
            }
            None => Response::network_error(NetworkError::ResourceLoadError(format!(
                "no such cce: page: {url}"
            ))),
        };
        Box::pin(std::future::ready(response))
    }
}
