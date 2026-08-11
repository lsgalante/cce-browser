//! Visit history: an append-only TSV log under the XDG state dir
//! (`~/.local/state/cce/browser/history.tsv`), an in-memory copy for
//! rendering, and the `cce:` protocol handler that serves it back as a
//! real page — `cce://history` is fetched through Servo's network stack
//! and rendered like any other page, so entries are ordinary links.
//!
//! The handler runs on Servo's fetch threads, hence the `Arc<Mutex<_>>`
//! store shared with the main thread's recorder.

use std::fs::{self, OpenOptions};
use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use servo::protocol_handler::{
    DoneChannel, FetchContext, HttpStatus, NetworkError, ProtocolHandler, Request, Response,
    ResponseBody, ResourceFetchTiming,
};
use std::sync::Arc;

/// Render at most this many entries on the history page.
const RENDER_CAP: usize = 500;

#[derive(Clone)]
struct Entry {
    ts: u64,
    url: String,
    title: String,
}

pub struct History {
    entries: Mutex<Vec<Entry>>,
    path: PathBuf,
}

fn state_path() -> PathBuf {
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(x) if !x.is_empty() => PathBuf::from(x),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state"),
    };
    base.join("cce").join("browser").join("history.tsv")
}

/// One-line-safe field: the TSV log separates with tabs and newlines.
fn sanitize(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

impl History {
    /// Load the log from the state dir (missing file = empty history).
    pub fn load() -> Self {
        let path = state_path();
        let mut entries = Vec::new();
        if let Ok(text) = fs::read_to_string(&path) {
            for line in text.lines() {
                let mut parts = line.splitn(3, '\t');
                if let (Some(ts), Some(url), Some(title)) =
                    (parts.next(), parts.next(), parts.next())
                {
                    if let Ok(ts) = ts.parse() {
                        entries.push(Entry { ts, url: url.to_string(), title: title.to_string() });
                    }
                }
            }
        }
        Self { entries: Mutex::new(entries), path }
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
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entry = Entry { ts, url: sanitize(url), title: sanitize(title) };
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

    /// The history page markup: newest first, timestamps localized by a
    /// tiny script on the page itself.
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
        let count = entries.len();
        let body = if count == 0 {
            "<p class=empty>No history yet.</p>".to_string()
        } else {
            rows
        };
        format!(
            "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>History</title><style>\
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
             .empty{{color:#8a8c92}}\
             </style></head><body>\
             <h1>History</h1>\
             <div class=meta>{count} entries<a href=\"cce://history/clear\">clear</a></div>\
             {body}\
             <script>for(const el of document.querySelectorAll('[data-ts]')){{\
             const d=new Date(1000*+el.dataset.ts);\
             el.textContent=d.toLocaleDateString()+'  '+\
             d.toLocaleTimeString([],{{hour:'2-digit',minute:'2-digit'}});}}</script>\
             </body></html>"
        )
    }
}

/// `cce:` scheme: internal pages served straight out of the app.
pub struct CceProtocol {
    pub history: Arc<History>,
}

impl ProtocolHandler for CceProtocol {
    fn load(
        &self,
        request: &mut Request,
        _done_chan: &mut DoneChannel,
        _context: &FetchContext,
    ) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        let url = request.current_url();
        let page = url.as_str().trim_start_matches("cce://").trim_end_matches('/');
        let body = match page {
            "history" => Some(self.history.html()),
            "history/clear" => {
                self.history.clear();
                Some(self.history.html())
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
            None => Response::network_error(NetworkError::ResourceLoadError(
                format!("no such cce: page: {page}"),
            )),
        };
        Box::pin(std::future::ready(response))
    }
}
