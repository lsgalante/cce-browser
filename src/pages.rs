//! Internal `cce:` pages and their backing stores.
//!
//! History and bookmarks live as TSV files under the XDG state dir
//! (`~/.local/state/cce/browser/`), with in-memory copies for rendering.
//! The `cce:` protocol handler serves them back as real pages —
//! `cce://history` and `cce://bookmarks` are fetched through Servo's
//! network stack and rendered like any other page, so entries are
//! ordinary links (including the mutating clear/remove actions).
//!
//! The handler runs on Servo's fetch threads, hence the `Arc<Mutex<_>>`
//! stores shared with the main thread.

use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

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

    fn html(&self) -> String {
        let entries = self.entries.lock().unwrap();
        let mut rows = String::new();
        for e in entries.iter().rev() {
            let title = if e.title.trim().is_empty() { &e.url } else { &e.title };
            let enc: String = url::form_urlencoded::byte_serialize(e.url.as_bytes()).collect();
            rows.push_str(&format!(
                "<div class=e><span class=w data-ts=\"{}\"></span>\
                 <a href=\"{}\">{}</a><span class=u>{}</span>\
                 <a class=rm href=\"cce://bookmarks/remove?url={}\">remove</a></div>\n",
                e.ts,
                html_escape(&e.url),
                html_escape(title),
                html_escape(&e.url),
                html_escape(&enc),
            ));
        }
        let meta = format!("{} bookmarks", entries.len());
        let body = if entries.is_empty() {
            "<p class=empty>No bookmarks yet. Star a page or press Ctrl+D.</p>".to_string()
        } else {
            rows
        };
        page("Bookmarks", &meta, &body, "")
    }
}

/// `cce:` scheme: internal pages served straight out of the app.
pub struct CceProtocol {
    pub history: Arc<History>,
    pub bookmarks: Arc<Bookmarks>,
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

impl ProtocolHandler for CceProtocol {
    fn load(
        &self,
        request: &mut Request,
        _done_chan: &mut DoneChannel,
        _context: &FetchContext,
    ) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        let url = request.current_url();
        let full = url.as_str().trim_start_matches("cce://");
        let (path, query) = full.split_once('?').unwrap_or((full, ""));
        let body = match path.trim_end_matches('/') {
            "history" => Some(self.history.html()),
            "history/clear" => {
                self.history.clear();
                Some(self.history.html())
            }
            "bookmarks" => Some(self.bookmarks.html()),
            "bookmarks/remove" => {
                if let Some((_, target)) =
                    url::form_urlencoded::parse(query.as_bytes()).find(|(k, _)| k == "url")
                {
                    self.bookmarks.remove(&target);
                }
                Some(self.bookmarks.html())
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
        };
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
                "no such cce: page: {path}"
            ))),
        };
        Box::pin(std::future::ready(response))
    }
}
