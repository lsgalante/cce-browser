//! The Raindrop.io REST client — phase 2. It fetches and it sends; deciding
//! what to send is the merge's job (`super::plan`), and nothing here has an
//! opinion about it.
//!
//! Checked against developer.raindrop.io on 2026-10-02: REST under
//! `api.raindrop.io/rest/v1`, `Authorization: Bearer <token>` (a personal
//! *test token* from the integration settings, which does not expire), 120
//! requests a minute with `429` past that, timestamps in ISO 8601, and:
//!
//! * `GET /raindrops/{collection}` — `-1` is Unsorted — 50 a page at most;
//! * `POST /raindrop` creates, `PUT /raindrop/{id}` is a **partial** update
//!   (only the fields sent change), `DELETE /raindrop/{id}` moves to Trash —
//!   and deletes **permanently** when the item is already in Trash, which is
//!   why only ids just fetched from a live collection are ever trashed.
//!
//! The token is held as an `accounts::Secret` and only ever goes into the
//! `Authorization` header; reqwest's errors carry the URL, never headers.

use super::{Local, Plan, RaindropId, Remote, Synced};
use crate::accounts::Secret;

const BASE: &str = "https://api.raindrop.io/rest/v1";
/// The Unsorted collection: the one this browser mirrors (RAINDROP-SYNC.md).
pub const UNSORTED: i64 = -1;
const PER_PAGE: usize = 50;
/// A fetch that runs past this many pages is not a bookmark collection.
const MAX_PAGES: usize = 400;

#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// 401/403: the token is wrong, revoked, or for nothing.
    Unauthorized,
    /// Still 429 after waiting once.
    RateLimited,
    /// The collection changed while it was being paged through, so the list
    /// may be missing an item — and a missing item reads as a deletion.
    Changed { fetched: usize, count: usize },
    Http(u16, String),
    Network(String),
    Parse(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => write!(f, "Raindrop refused the token"),
            ApiError::RateLimited => write!(f, "Raindrop's rate limit; try again in a minute"),
            ApiError::Changed { fetched, count } => write!(
                f,
                "the collection changed while it was read ({fetched} fetched, {count} reported); \
                 try again"
            ),
            ApiError::Http(code, body) => write!(f, "Raindrop answered {code}: {body}"),
            ApiError::Network(e) => write!(f, "could not reach Raindrop: {e}"),
            ApiError::Parse(e) => write!(f, "unexpected answer from Raindrop: {e}"),
        }
    }
}

pub struct Client {
    http: reqwest::blocking::Client,
    token: Secret,
    base: String,
}

impl Client {
    /// The real Raindrop — or, when `CCE_RAINDROP_API` is set, a stand-in at
    /// that base URL, which is how the whole browser is tested end to end
    /// without writing to an account.
    pub fn new(token: Secret) -> Self {
        match std::env::var("CCE_RAINDROP_API") {
            Ok(base) if !base.is_empty() => {
                log::warn!("raindrop: using the stand-in API at {base}");
                Self::with_base(token, &base)
            }
            _ => Self::with_base(token, BASE),
        }
    }

    /// Against another server — the tests' stand-in.
    pub fn with_base(token: Secret, base: &str) -> Self {
        let http = reqwest::blocking::Client::builder()
            .user_agent(concat!("cce-browser/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("an HTTP client with default TLS");
        Self { http, token, base: base.trim_end_matches('/').to_string() }
    }

    /// Send one request, waiting out a single `429` (Raindrop says when its
    /// window resets; never longer than a minute), and return the JSON body.
    fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value, ApiError> {
        for attempt in 0..2 {
            let mut req = self
                .http
                .request(method.clone(), format!("{}{path}", self.base))
                .bearer_auth(self.token.expose());
            if let Some(b) = body {
                // By hand rather than reqwest's `json` feature, to build the
                // same reqwest the sibling crates already compile.
                req = req
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(b.to_string());
            }
            let resp = req.send().map_err(|e| ApiError::Network(e.without_url().to_string()))?;
            let status = resp.status().as_u16();
            if status == 429 && attempt == 0 {
                let now = super::unix_now();
                let reset = resp
                    .headers()
                    .get("x-ratelimit-reset")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(now + 60);
                std::thread::sleep(std::time::Duration::from_secs(reset.saturating_sub(now).clamp(1, 60)));
                continue;
            }
            let text = resp.text().map_err(|e| ApiError::Network(e.without_url().to_string()))?;
            return match status {
                200..=299 => serde_json::from_str(&text).map_err(|e| ApiError::Parse(e.to_string())),
                401 | 403 => Err(ApiError::Unauthorized),
                429 => Err(ApiError::RateLimited),
                _ => Err(ApiError::Http(status, text.chars().take(200).collect())),
            };
        }
        Err(ApiError::RateLimited)
    }

    /// Every bookmark in `collection`, oldest first.
    ///
    /// Sorted by creation time ascending, so a bookmark added mid-fetch lands
    /// on the last page instead of shifting the others. A deletion still
    /// shifts them, which is why the total is checked against the `count`
    /// Raindrop reports and the whole fetch refused on a mismatch.
    pub fn fetch(&self, collection: i64) -> Result<Vec<Remote>, ApiError> {
        let mut out = Vec::new();
        let mut reported = None;
        for page in 0..MAX_PAGES {
            let v = self.call(
                reqwest::Method::GET,
                &format!("/raindrops/{collection}?perpage={PER_PAGE}&page={page}&sort=created"),
                None,
            )?;
            if let Some(c) = v["count"].as_u64() {
                reported = Some(c as usize);
            }
            let items = v["items"].as_array().ok_or_else(|| ApiError::Parse("no items".into()))?;
            for item in items {
                out.push(parse_item(item)?);
            }
            if items.len() < PER_PAGE {
                break;
            }
        }
        if let Some(count) = reported {
            if count != out.len() {
                return Err(ApiError::Changed { fetched: out.len(), count });
            }
        }
        Ok(out)
    }

    /// Create a bookmark; returns its new id. Link and title only — Raindrop
    /// fills in the rest itself.
    pub fn create(&self, collection: i64, link: &str, title: &str) -> Result<RaindropId, ApiError> {
        let body = serde_json::json!({
            "link": link,
            "title": title,
            "collection": { "$id": collection },
        });
        let v = self.call(reqwest::Method::POST, "/raindrop", Some(&body))?;
        v["item"]["_id"].as_u64().ok_or_else(|| ApiError::Parse("created item has no _id".into()))
    }

    /// Change a title. A partial update: nothing else on the item is touched.
    pub fn rename(&self, id: RaindropId, title: &str) -> Result<(), ApiError> {
        let body = serde_json::json!({ "title": title });
        self.call(reqwest::Method::PUT, &format!("/raindrop/{id}"), Some(&body)).map(|_| ())
    }

    /// Move to Trash. Only ever called with ids fetched from the live
    /// collection this pass — on an item already in Trash this is permanent.
    pub fn trash(&self, id: RaindropId) -> Result<(), ApiError> {
        self.call(reqwest::Method::DELETE, &format!("/raindrop/{id}"), None).map(|_| ())
    }
}

fn parse_item(v: &serde_json::Value) -> Result<Remote, ApiError> {
    let id = v["_id"].as_u64().ok_or_else(|| ApiError::Parse("an item has no _id".into()))?;
    let link = v["link"].as_str().ok_or_else(|| ApiError::Parse(format!("item {id} has no link")))?;
    // Tabs and line breaks become spaces here, at the source: the local
    // store has no escaping and flattens them, and a title that differed
    // only in that way would read as renamed in Raindrop on every pass.
    let flat = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    Ok(Remote {
        id,
        link: flat(link),
        title: flat(v["title"].as_str().unwrap_or_default()),
        created: v["created"].as_str().and_then(iso8601_secs).unwrap_or(0),
    })
}

/// `2026-10-02T16:04:05.123Z` → seconds since the epoch. Just the shape
/// Raindrop sends (UTC, `Z`, optional fraction); anything else is `None`.
pub fn iso8601_secs(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    if !s.ends_with('Z') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + mi * 60 + se).ok()
}

/// What applying a plan's remote half did — the input to `Plan::base_after`,
/// which keeps the base honest about anything that failed.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Applied {
    /// `(local URL, new id)` for each create Raindrop accepted.
    pub created: Vec<(String, RaindropId)>,
    pub failed_renames: Vec<RaindropId>,
    pub failed_trash: Vec<RaindropId>,
    /// One line per failure, for the status.
    pub errors: Vec<String>,
}

/// Apply a plan's remote half. A failure on one item does not stop the rest;
/// it is recorded, and the base built from the result leaves that item to be
/// retried. Unauthorized stops everything — every other call would fail too.
pub fn apply_remote(client: &Client, collection: i64, plan: &Plan) -> Result<Applied, ApiError> {
    let mut out = Applied::default();
    let mut note = |what: String, e: ApiError| -> Result<(), ApiError> {
        if e == ApiError::Unauthorized {
            return Err(e);
        }
        out.errors.push(format!("{what}: {e}"));
        Ok(())
    };
    let mut created = Vec::new();
    let mut failed_renames = Vec::new();
    let mut failed_trash = Vec::new();
    for l in &plan.create_remote {
        match client.create(collection, &l.url, &l.title) {
            Ok(id) => created.push((l.url.clone(), id)),
            Err(e) => note(format!("create {}", l.url), e)?,
        }
    }
    for (id, title) in &plan.rename_remote {
        if let Err(e) = client.rename(*id, title) {
            failed_renames.push(*id);
            note(format!("rename {id}"), e)?;
        }
    }
    for id in &plan.trash_remote {
        if let Err(e) = client.trash(*id) {
            failed_trash.push(*id);
            note(format!("trash {id}"), e)?;
        }
    }
    out.created = created;
    out.failed_renames = failed_renames;
    out.failed_trash = failed_trash;
    Ok(out)
}

/// The plan, readably — what the dry run prints.
pub fn describe(plan: &Plan, local: &[Local], remote: &[Remote], base: &[Synced]) -> String {
    let mut s = format!(
        "here {} · Raindrop {} · synced before {}\n",
        local.len(),
        remote.len(),
        base.len()
    );
    let title_of = |id: &RaindropId| {
        remote.iter().find(|r| r.id == *id).map(|r| r.link.as_str()).unwrap_or("?")
    };
    let mut line = |label: &str, items: Vec<String>| {
        if !items.is_empty() {
            s.push_str(&format!("\n{label} ({}):\n", items.len()));
            for i in items {
                s.push_str(&format!("  {i}\n"));
            }
        }
    };
    line("create in Raindrop", plan.create_remote.iter().map(|l| format!("{}  {}", l.url, l.title)).collect());
    line("rename in Raindrop", plan.rename_remote.iter().map(|(id, t)| format!("{} → {t}", title_of(id))).collect());
    line("move to Raindrop's trash", plan.trash_remote.iter().map(|id| title_of(id).to_string()).collect());
    line("relink here", plan.relink_local.iter().map(|(a, b)| format!("{a} → {b}")).collect());
    line("rename here", plan.rename_local.iter().map(|(u, t)| format!("{u} → {t}")).collect());
    line("delete here", plan.delete_local.clone());
    line("add here", plan.add_local.iter().map(|l| format!("{}  {}", l.url, l.title)).collect());
    if plan.is_noop() {
        s.push_str("\nnothing to do — in sync\n");
    }
    s
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Arc, Mutex};

    /// A stand-in Raindrop: answers each request with the next scripted
    /// `(status, extra headers, body)` and records `(request line, body)`.
    pub(in crate::raindrop) fn server(script: Vec<(u16, &'static str, String)>) -> (String, Arc<Mutex<Vec<(String, String)>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for (status, headers, body) in script {
                let Ok((stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream);
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut len = 0;
                let mut auth = String::new();
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h == "\r\n" || h.is_empty() {
                        break;
                    }
                    let lower = h.to_lowercase();
                    if let Some(v) = lower.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if lower.starts_with("authorization:") {
                        auth = h.trim().to_string();
                    }
                }
                let mut req_body = vec![0; len];
                reader.read_exact(&mut req_body).unwrap();
                log.lock().unwrap().push((
                    format!("{} | {auth}", first.trim()),
                    String::from_utf8_lossy(&req_body).into_owned(),
                ));
                let mut stream = reader.into_inner();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{headers}\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (base, seen)
    }

    pub(in crate::raindrop) fn items(range: std::ops::Range<u64>, count: usize) -> String {
        let items: Vec<_> = range
            .map(|i| serde_json::json!({
                "_id": i, "link": format!("https://{i}.test/"), "title": format!("t{i}"),
                "created": "2026-10-02T16:04:05.123Z", "collection": {"$id": -1},
            }))
            .collect();
        serde_json::json!({ "result": true, "items": items, "count": count }).to_string()
    }

    fn client(base: &str) -> Client {
        Client::with_base(Secret::from("tok-123".to_string()), base)
    }

    #[test]
    fn fetch_pages_until_a_short_page_and_checks_the_count() {
        let (base, seen) = server(vec![(200, "", items(1..51, 53)), (200, "", items(51..54, 53))]);
        let all = client(&base).fetch(UNSORTED).unwrap();
        assert_eq!(all.len(), 53);
        assert_eq!(all[0], Remote { id: 1, link: "https://1.test/".into(), title: "t1".into(), created: 1_790_957_045 });
        let seen = seen.lock().unwrap();
        assert!(seen[0].0.starts_with("GET /raindrops/-1?perpage=50&page=0&sort=created"));
        assert!(seen[1].0.contains("page=1"));
        assert!(seen[0].0.ends_with("authorization: Bearer tok-123") || seen[0].0.ends_with("Authorization: Bearer tok-123"));
    }

    #[test]
    fn a_collection_that_shifts_mid_fetch_is_refused() {
        // A deletion between pages: 50 + 2 fetched, but 53 reported.
        let (base, _) = server(vec![(200, "", items(1..51, 53)), (200, "", items(52..54, 53))]);
        assert_eq!(client(&base).fetch(UNSORTED), Err(ApiError::Changed { fetched: 52, count: 53 }));
    }

    #[test]
    fn create_sends_only_link_title_and_collection() {
        let (base, seen) = server(vec![(200, "", r#"{"result":true,"item":{"_id":77}}"#.into())]);
        assert_eq!(client(&base).create(UNSORTED, "https://a.test/", "A").unwrap(), 77);
        let body: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0].1).unwrap();
        assert_eq!(body, serde_json::json!({"link": "https://a.test/", "title": "A", "collection": {"$id": -1}}));
    }

    #[test]
    fn rename_is_a_title_only_partial_update() {
        let (base, seen) = server(vec![(200, "", r#"{"result":true,"item":{}}"#.into())]);
        client(&base).rename(5, "New").unwrap();
        let (line, body) = seen.lock().unwrap()[0].clone();
        assert!(line.starts_with("PUT /raindrop/5 "));
        assert_eq!(body, r#"{"title":"New"}"#);
    }

    #[test]
    fn a_rate_limit_is_waited_out_once() {
        let (base, seen) = server(vec![
            (429, "X-RateLimit-Reset: 0\r\n", "{}".into()),
            (200, "", r#"{"result":true}"#.into()),
        ]);
        client(&base).trash(9).unwrap();
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_bad_token_stops_the_pass() {
        let plan = Plan {
            create_remote: vec![Local { url: "https://a.test/".into(), title: "A".into(), ts: 1 }],
            trash_remote: vec![3],
            ..Plan::default()
        };
        let (base, seen) = server(vec![(401, "", "{}".into()), (200, "", "{}".into())]);
        assert_eq!(apply_remote(&client(&base), UNSORTED, &plan), Err(ApiError::Unauthorized));
        assert_eq!(seen.lock().unwrap().len(), 1, "nothing more is sent after a 401");
    }

    #[test]
    fn one_failure_does_not_stop_the_rest() {
        let plan = Plan {
            create_remote: vec![
                Local { url: "https://a.test/".into(), title: "A".into(), ts: 1 },
                Local { url: "https://b.test/".into(), title: "B".into(), ts: 2 },
            ],
            trash_remote: vec![3],
            ..Plan::default()
        };
        let (base, _) = server(vec![
            (500, "", "boom".into()),
            (200, "", r#"{"result":true,"item":{"_id":8}}"#.into()),
            (404, "", "gone".into()),
        ]);
        let applied = apply_remote(&client(&base), UNSORTED, &plan).unwrap();
        assert_eq!(applied.created, vec![("https://b.test/".to_string(), 8)]);
        assert_eq!(applied.failed_trash, vec![3]);
        assert_eq!(applied.errors.len(), 2);
    }

    #[test]
    fn titles_are_flattened_where_they_arrive() {
        let r = parse_item(&serde_json::json!({"_id": 1, "link": "https://a.test/", "title": "two\nlines\tand tab"})).unwrap();
        assert_eq!(r.title, "two lines and tab");
    }

    #[test]
    fn timestamps_parse() {
        assert_eq!(iso8601_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(iso8601_secs("2000-03-01T00:00:00.000Z"), Some(951_868_800));
        assert_eq!(iso8601_secs("2026-10-02T16:04:05Z"), Some(1_790_957_045));
        assert_eq!(iso8601_secs("2026-10-02 16:04:05"), None);
        assert_eq!(iso8601_secs("2026-13-02T16:04:05Z"), None);
    }

    #[test]
    fn errors_never_carry_the_token() {
        let e = client("http://127.0.0.1:1").fetch(UNSORTED).unwrap_err();
        assert!(matches!(e, ApiError::Network(_)));
        assert!(!e.to_string().contains("tok-123"));
    }
}
