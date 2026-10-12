//! Agent control: the browser as an MCP server, so an agent can drive it the
//! way Claude in Chrome drives Chrome — list and open tabs, navigate, read a
//! page as an accessibility-style tree with `ref_N` handles, find things,
//! click, type, scroll, fill forms, run script, and see screenshots.
//!
//! ```sh
//! claude mcp add --transport http cce-browser http://127.0.0.1:3004/mcp
//! ```
//!
//! cce-ui's tools-only server (`cce_ui::mcp`, as cce-notes and cce-designer
//! use it), loopback only and refusing web pages (Origin/Host checks). Only
//! the single instance serves it; `browser.agent false` turns it off and
//! `CCE_BROWSER_MCP_PORT` moves it (a second instance in a shadow). WPE only.
//!
//! **Each tool call runs on its own thread** (`serve`), as a plain sequence
//! of steps: a click by ref is "ask the page where the ref is, then press
//! there, then wait for any load it started". Each step is an [`Op`] sent to
//! the app's loop and answered there — immediately for the chrome's own
//! state, from inside `pump` for what WebKit answers asynchronously (script,
//! snapshots). The loop is never blocked waiting on the page.
//!
//! The page half is `agent/page.js`, run in a private script world
//! ([`WORLD`]): the page cannot see or replace it, and reads only the DOM it
//! shares. Input is real WPE input events, so the page sees a person's
//! clicks and keys (user activation, focus, `isTrusted`), and it works on a
//! background tab as on the shown one.

use std::collections::HashMap;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use cce_ui::mcp::{McpTool, McpToolCall};
use serde_json::{json, Value};

use crate::wpe::ffi::WPEModifiers;
use crate::wpe::{AgentImage, PointerKind};
use crate::{BrowserApp, Message};

/// The private script world the page half runs in.
pub const WORLD: &str = "cce-agent";
const PAGE_JS: &str = include_str!("agent/page.js");
const DEFAULT_PORT: u16 = 3004;

/// How long one step may wait on the loop. Under cce-ui's 30s call limit,
/// so a stuck step fails the call with its own reason, not a timeout.
const OP_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `navigate` waits for a load to finish before answering anyway.
const LOAD_WAIT: Duration = Duration::from_secs(15);
/// After input, how long a load it started is waited for.
const SETTLE_WAIT: Duration = Duration::from_secs(8);

/// The largest screenshot sent, in pixels on the long edge and in total —
/// what a vision model takes in without being scaled down again.
const SHOT_EDGE: f64 = 1568.0;
const SHOT_AREA: f64 = 1_150_000.0;

/// CSS pixels per screenshot pixel, per tab id, from that tab's last
/// screenshot: coordinates an agent reads off a screenshot are in its
/// pixels, and this turns them back into the page's.
static SHOT_SCALE: Mutex<Option<HashMap<u64, f64>>> = Mutex::new(None);

pub fn start(sender: calloop::channel::Sender<Message>) {
    let port = std::env::var("CCE_BROWSER_MCP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    cce_ui::mcp::start_mcp_server("cce-browser", port, tools(), sender, Message::Mcp);
}

/// One step of a tool call, done on the app's loop. `None` for a tab is the
/// shown one.
#[derive(Debug, Clone)]
pub enum Op {
    Tabs,
    Create { url: String, foreground: bool },
    Close(u64),
    Select(u64),
    /// A URL (with the URL bar's guessing), or `back` / `forward` / `reload`.
    Navigate(Option<u64>, String),
    Status(Option<u64>),
    /// Script in the agent world (`page_world` false) or the page's own;
    /// `call` runs it as an async function body rather than a script.
    Script { tab: Option<u64>, source: String, page_world: bool, call: bool },
    Snapshot(Option<u64>),
    /// Answer the page's open alert / confirm / prompt.
    Dialog { accept: bool, text: Option<String> },
    Pointer { tab: Option<u64>, kind: PointerKind, button: u32, x: f64, y: f64, keys: u32 },
    Wheel { tab: Option<u64>, dx: f64, dy: f64, x: f64, y: f64 },
    Key { tab: Option<u64>, keyval: u32, pressed: bool, keys: u32 },
}

pub enum Answer {
    Value(Value),
    /// A snapshot, with the viewport it shows in CSS pixels.
    Image(AgentImage, (f64, f64)),
}

pub type Reply = mpsc::Sender<Result<Answer, String>>;

// ---- the loop's half ----

impl BrowserApp {
    /// Do one step and answer it — now, or from `pump` for what WebKit
    /// answers later.
    pub(crate) fn agent_op(&mut self, op: Op, reply: Reply) {
        let answer = |r: Result<Value, String>| {
            let _ = reply.send(r.map(Answer::Value));
        };
        match op {
            Op::Tabs => {
                let tabs: Vec<Value> = (0..self.host.tab_count()).map(|i| self.tab_json(i)).collect();
                answer(Ok(Value::Array(tabs)))
            }
            Op::Create { url, foreground } => {
                let Some(url) = self.agent_url(&url) else { return answer(Err(format!("not a URL: {url}"))) };
                if foreground {
                    self.host.open_tab(url);
                    self.url_focused = false;
                    self.sync_page_state();
                    self.persist_session();
                } else {
                    self.open_background_tab(url);
                }
                answer(Ok(self.tab_json(self.host.tab_count() - 1)))
            }
            Op::Close(id) => answer(self.agent_index(Some(id)).and_then(|i| {
                if self.host.tab_count() == 1 {
                    return Err("that is the only tab; closing it would quit the browser".into());
                }
                self.close_tab(i);
                Ok(json!(format!("closed tab {id}")))
            })),
            Op::Select(id) => answer(self.agent_index(Some(id)).map(|i| {
                self.switch_tab(i);
                self.tab_json(i)
            })),
            Op::Navigate(tab, target) => answer(self.agent_index(tab).and_then(|i| {
                match target.as_str() {
                    "back" => self.host.agent_history(i, -1)?,
                    "forward" => self.host.agent_history(i, 1)?,
                    "reload" => self.host.agent_history(i, 0)?,
                    text => {
                        let url = self.agent_url(text).ok_or_else(|| format!("not a URL: {text}"))?;
                        self.host.agent_load(i, &url)?;
                    }
                }
                if i == self.host.active_index() {
                    self.loading = true;
                }
                Ok(self.tab_json(i))
            })),
            Op::Status(tab) => answer(self.agent_index(tab).map(|i| {
                let mut s = self.tab_json(i);
                // Not the tab's own: WebKit's dialogs and sign-ins are one at a
                // time for the whole browser, as the chrome shows them.
                if let Some(d) = self.host.pending_dialog() {
                    let kind = match (&d.prompt_default, d.has_cancel) {
                        (Some(_), _) => "prompt",
                        (None, true) => "confirm",
                        (None, false) => "alert",
                    };
                    s["dialog"] = json!({ "kind": kind, "message": d.message });
                } else if self.host.pending_auth().is_some() {
                    s["dialog"] = json!({ "kind": "sign-in", "message": "a site asked for a username and password; only the person can answer it" });
                }
                let notes = std::mem::take(&mut self.agent_notes);
                if !notes.is_empty() {
                    s["notes"] = json!(notes);
                }
                s
            })),
            Op::Dialog { accept, text } => {
                if self.host.pending_dialog().is_none() {
                    return answer(Err("no dialog is open".into()));
                }
                // The chrome's modal, if it has put it up yet, goes with it.
                match &mut self.modal {
                    Some(m) if matches!(m.kind, crate::ModalKind::Script) => {
                        if let (Some(t), Some((_, field))) = (&text, m.fields.first_mut()) {
                            *field = cce_ui::widget::LineEdit::with_text(t.clone());
                        }
                        self.close_modal(accept);
                    }
                    _ => self.host.respond_dialog(accept, text.as_deref()),
                }
                answer(Ok(json!(if accept { "accepted" } else { "dismissed" })))
            }
            Op::Script { tab, source, page_world, call } => {
                let i = match self.agent_index(tab) {
                    Ok(i) => i,
                    Err(e) => return answer(Err(e)),
                };
                let world = (!page_world).then_some(WORLD);
                let done = Box::new(move |r: Result<String, String>| {
                    let _ = reply.send(r.map(|json| Answer::Value(Value::String(json))));
                });
                if call {
                    self.host.agent_call(i, &source, world, done)
                } else {
                    self.host.agent_eval(i, &source, world, done)
                }
            }
            Op::Snapshot(tab) => {
                let i = match self.agent_index(tab) {
                    Ok(i) => i,
                    Err(e) => return answer(Err(e)),
                };
                let viewport = self.host.agent_viewport();
                self.host.agent_snapshot(
                    i,
                    Box::new(move |r| {
                        let _ = reply.send(r.map(|img| Answer::Image(img, viewport)));
                    }),
                );
            }
            Op::Pointer { tab, kind, button, x, y, keys } => answer(
                self.agent_index(tab)
                    .inspect(|_| {
                        if kind == PointerKind::Down {
                            self.agent_pressed = true;
                        }
                    })
                    .and_then(|i| self.host.agent_pointer(i, kind, button, x, y, keys as WPEModifiers::Type))
                    .map(|()| Value::Null),
            ),
            Op::Wheel { tab, dx, dy, x, y } => answer(
                self.agent_index(tab).and_then(|i| self.host.agent_wheel(i, dx, dy, x, y)).map(|()| Value::Null),
            ),
            Op::Key { tab, keyval, pressed, keys } => answer(
                self.agent_index(tab)
                    .and_then(|i| self.host.agent_key(i, keyval, pressed, keys as WPEModifiers::Type))
                    .map(|()| Value::Null),
            ),
        }
    }

    fn agent_index(&self, tab: Option<u64>) -> Result<usize, String> {
        match tab {
            None => Ok(self.host.active_index()),
            Some(id) => self.host.index_of(id).ok_or_else(|| format!("no tab {id}; tabs_context lists the open ones")),
        }
    }

    /// What the URL bar would make of it: a domain guess, a search.
    fn agent_url(&self, text: &str) -> Option<url::Url> {
        crate::parse_url_input(text, &self.settings.search_prefix)
    }

    fn tab_json(&self, i: usize) -> Value {
        let Some(t) = self.host.tab(i) else { return Value::Null };
        json!({
            "tabId": t.id,
            "url": t.url.as_ref().map(|u| u.to_string()).unwrap_or_default(),
            "title": t.title.clone().unwrap_or_default(),
            "loading": t.loading,
            "active": i == self.host.active_index(),
        })
    }
}

// ---- the call's half, on its own thread ----

/// Run a tool call on a thread of its own, answering it when done.
pub fn serve(call: McpToolCall, tx: calloop::channel::Sender<Message>) {
    std::thread::spawn(move || {
        let agent = Agent { tx };
        let result = agent.run(&call.name, &call.arguments);
        let _ = call.reply.send(result);
    });
}

struct Agent {
    tx: calloop::channel::Sender<Message>,
}

/// An open dialog, as the agent is told about it.
fn dialog_line(d: &Value) -> String {
    let kind = d["kind"].as_str().unwrap_or("dialog");
    let message = d["message"].as_str().unwrap_or("");
    if kind == "sign-in" {
        format!("A sign-in prompt is open: {message}.")
    } else {
        format!("A page {kind} is open: {message:?}. Answer it with handle_dialog.")
    }
}

/// A script error as WebKit words it, without the `<url>:<line>:<col>: `
/// it is prefixed with — the line is in the wrapper this module sent, not
/// in anything the agent wrote.
fn tidy_js_error(e: String) -> String {
    let located = e.find(": ").filter(|&i| {
        let last = e[..i].rsplit(':').next().unwrap_or("");
        e[..i].contains(':') && !last.is_empty() && last.bytes().all(|c| c.is_ascii_digit())
    });
    match located {
        Some(i) => e[i + 2..].to_string(),
        None => e,
    }
}

fn tab_arg(args: &Value) -> Option<u64> {
    args.get("tabId").and_then(Value::as_u64)
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| format!("`{key}` is required"))
}

fn coord_arg(args: &Value, key: &str) -> Option<(f64, f64)> {
    let a = args.get(key)?.as_array()?;
    Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))
}

impl Agent {
    fn op(&self, op: Op) -> Result<Answer, String> {
        let (tx, rx) = mpsc::channel();
        self.tx.send(Message::AgentOp(op, tx)).map_err(|_| "the browser is shutting down".to_string())?;
        rx.recv_timeout(OP_TIMEOUT).map_err(|_| "the page did not answer in time".to_string())?
    }

    fn value(&self, op: Op) -> Result<Value, String> {
        match self.op(op)? {
            Answer::Value(v) => Ok(v),
            Answer::Image(..) => Err("unexpected image".into()),
        }
    }

    fn status(&self, tab: Option<u64>) -> Result<Value, String> {
        self.value(Op::Status(tab))
    }

    /// The tab's id, so a call that starts on "the shown tab" keeps acting
    /// on that tab even if another is shown meanwhile.
    fn resolve(&self, tab: Option<u64>) -> Result<u64, String> {
        self.status(tab)?["tabId"].as_u64().ok_or_else(|| "no tab".into())
    }

    /// Script's answer, as the JSON value it was. Refused while a dialog is
    /// open: the page is blocked on it, and the script would only time out.
    fn script(&self, tab: Option<u64>, source: String, page_world: bool, call: bool) -> Result<Value, String> {
        let s = self.status(tab)?;
        if let Some(d) = s.get("dialog") {
            return Err(format!("{}\nThe page is blocked until it is answered.", dialog_line(d)));
        }
        let json = self.value(Op::Script { tab, source, page_world, call }).map_err(tidy_js_error)?;
        let text = json.as_str().unwrap_or("undefined");
        Ok(if text == "undefined" { Value::Null } else { serde_json::from_str(text).unwrap_or(Value::String(text.into())) })
    }

    /// Call the page half: `expr` is an expression over `__cceAgent`, which
    /// answers a JSON string.
    fn page(&self, tab: Option<u64>, expr: &str) -> Result<Value, String> {
        let v = self.script(tab, format!("{PAGE_JS}\n{expr}"), false, false)?;
        match v {
            Value::String(s) => serde_json::from_str(&s).map_err(|e| format!("bad answer from the page: {e}")),
            other => Ok(other),
        }
    }

    /// Wait for the load a navigation started to finish: until the tab has
    /// been seen loading and then not, or has not started at all after a
    /// moment (a same-document navigation never loads).
    fn wait_load(&self, tab: u64, limit: Duration) -> Result<Value, String> {
        let start = Instant::now();
        let mut seen = false;
        loop {
            let s = self.status(Some(tab))?;
            let loading = s["loading"].as_bool().unwrap_or(false);
            if loading {
                seen = true;
            } else if seen || start.elapsed() > Duration::from_millis(1200) {
                return Ok(s);
            }
            if start.elapsed() > limit {
                let mut s = s;
                s["note"] = json!("still loading; it may finish later");
                return Ok(s);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// After input: give a navigation it caused the chance to start, and if
    /// one did, wait for it. Answers a line to add to the result, if any.
    fn settle(&self, tab: u64) -> Result<Option<String>, String> {
        let before = self.status(Some(tab))?;
        std::thread::sleep(Duration::from_millis(150));
        let now = self.status(Some(tab))?;
        let mut lines = Vec::new();
        for s in [&before, &now] {
            for n in s["notes"].as_array().into_iter().flatten() {
                lines.push(n.as_str().unwrap_or_default().to_string());
            }
        }
        if let Some(d) = now.get("dialog") {
            lines.push(dialog_line(d));
        } else if now["loading"].as_bool().unwrap_or(false) || now["url"] != before["url"] {
            let done = self.wait_load(tab, SETTLE_WAIT)?;
            lines.push(match done.get("dialog") {
                Some(d) => dialog_line(d),
                None => format!("The page is now {} ({})", done["url"].as_str().unwrap_or(""), done["title"].as_str().unwrap_or("")),
            });
        }
        Ok((!lines.is_empty()).then(|| lines.join("\n")))
    }

    fn run(&self, name: &str, args: &Value) -> Result<Value, String> {
        let tab = tab_arg(args);
        match name {
            "tabs_context" => {
                let tabs = self.value(Op::Tabs)?;
                Ok(json!({ "tabs": tabs, "note": "Tools act on the shown tab (active: true) unless given a tabId." }))
            }
            "tabs_create" => {
                let url = args.get("url").and_then(Value::as_str).unwrap_or("about:blank").to_string();
                let foreground = args.get("foreground").and_then(Value::as_bool).unwrap_or(false);
                let t = self.value(Op::Create { url: url.clone(), foreground })?;
                let id = t["tabId"].as_u64().ok_or("no tab was opened")?;
                if url == "about:blank" {
                    Ok(t)
                } else {
                    self.wait_load(id, LOAD_WAIT)
                }
            }
            "tabs_close" => {
                let id = tab.ok_or("`tabId` is required")?;
                self.value(Op::Close(id))
            }
            "tabs_select" => {
                let id = tab.ok_or("`tabId` is required")?;
                self.value(Op::Select(id))
            }
            "navigate" => {
                let target = str_arg(args, "url")?.trim().to_string();
                let id = self.resolve(tab)?;
                self.value(Op::Navigate(Some(id), target))?;
                self.wait_load(id, LOAD_WAIT)
            }
            "read_page" => {
                let opts = json!({
                    "filter": args.get("filter").and_then(Value::as_str).unwrap_or("all"),
                    "ref": args.get("ref_id").and_then(Value::as_str),
                    "depth": args.get("depth").and_then(Value::as_u64),
                    "maxChars": args.get("max_chars").and_then(Value::as_u64).unwrap_or(50_000),
                });
                let v = self.page(tab, &format!("__cceAgent.tree({opts})"))?;
                Ok(Value::String(format!(
                    "{} — {}\nviewport {}x{} CSS px, scrolled to {},{} of {}x{}\n\n{}",
                    v["title"].as_str().unwrap_or(""),
                    v["url"].as_str().unwrap_or(""),
                    v["viewport"][0], v["viewport"][1], v["scroll"][0], v["scroll"][1], v["page"][0], v["page"][1],
                    v["tree"].as_str().unwrap_or(""),
                )))
            }
            "find" => {
                let query = str_arg(args, "query")?;
                let v = self.page(tab, &format!("__cceAgent.find({})", json!(query)))?;
                let hits: Vec<&str> = v.as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                Ok(Value::String(if hits.is_empty() {
                    format!("Nothing on the page matches {query:?}.")
                } else {
                    format!("{} match(es) (positions are CSS px in the viewport):\n{}", hits.len(), hits.join("\n"))
                }))
            }
            "get_page_text" => {
                let max = args.get("max_chars").and_then(Value::as_u64).unwrap_or(50_000);
                let v = self.page(tab, &format!("__cceAgent.text({max})"))?;
                Ok(Value::String(format!(
                    "{} — {}\n\n{}{}",
                    v["title"].as_str().unwrap_or(""),
                    v["url"].as_str().unwrap_or(""),
                    v["text"].as_str().unwrap_or(""),
                    if v["truncated"].as_bool().unwrap_or(false) { "\n[truncated]" } else { "" },
                )))
            }
            "form_input" => {
                let r = str_arg(args, "ref")?;
                let value = args.get("value").cloned().ok_or("`value` is required")?;
                let v = self.page(tab, &format!("__cceAgent.input({}, {})", json!(r), value))?;
                Ok(Value::String(format!("{r}: {}", v.as_str().unwrap_or("done"))))
            }
            "javascript_tool" => self.javascript(tab, str_arg(args, "text")?),
            "handle_dialog" => {
                let accept = args.get("accept").and_then(Value::as_bool).ok_or("`accept` is required")?;
                let text = args.get("text").and_then(Value::as_str).map(str::to_string);
                self.value(Op::Dialog { accept, text })
            }
            "computer" => self.computer(tab, args),
            _ => Err(format!("unknown tool {name}")),
        }
    }

    /// Script in the page's own world, read the way a console reads it: an
    /// expression's value (awaited if it is a promise), else the completion
    /// value of the statements, else — for statements that `await` — the
    /// body of an async function, answering what it `return`s.
    fn javascript(&self, tab: Option<u64>, code: &str) -> Result<Value, String> {
        let syntax = |e: &String| e.contains("SyntaxError");
        let expr = code.trim().trim_end_matches(';');
        let v = match self.script(tab, format!("return ({expr}\n);"), true, true) {
            Err(e) if syntax(&e) => match self.script(tab, code.to_string(), true, false) {
                // A function body's own syntax error is about the wrapping;
                // the script's is about the code as written.
                Err(e) if syntax(&e) => self.script(tab, code.to_string(), true, true).map_err(|f| if syntax(&f) { e } else { f }),
                other => other,
            },
            other => other,
        }?;
        Ok(match v {
            Value::Null => Value::String("undefined".into()),
            Value::String(s) => Value::String(s),
            other => Value::String(serde_json::to_string_pretty(&other).unwrap_or_default()),
        })
    }

    fn computer(&self, tab: Option<u64>, args: &Value) -> Result<Value, String> {
        let action = str_arg(args, "action")?;
        let id = self.resolve(tab)?;
        let tab = Some(id);
        let scale = SHOT_SCALE.lock().unwrap().as_ref().and_then(|m| m.get(&id).copied()).unwrap_or(1.0);
        let keys = match args.get("modifiers").and_then(Value::as_str) {
            Some(m) => modifiers(m)?,
            None => 0,
        };

        // A point to act on: a ref (brought into view), else a coordinate in
        // the last screenshot's pixels.
        let target = |what: &str| -> Result<(f64, f64, Option<String>), String> {
            if let Some(r) = args.get("ref").and_then(Value::as_str) {
                let p = self.page(tab, &format!("__cceAgent.point({})", json!(r)))?;
                let (x, y) = (p["x"].as_f64().unwrap_or(0.0), p["y"].as_f64().unwrap_or(0.0));
                let note = p["covered"].as_str().map(|c| format!("Note: {r} is covered at its middle by {c}; the click went to that."));
                return Ok((x, y, note));
            }
            let (x, y) = coord_arg(args, "coordinate").ok_or_else(|| format!("{what} needs `coordinate` or `ref`"))?;
            Ok((x * scale, y * scale, None))
        };
        let pointer = |kind, button, x, y| self.value(Op::Pointer { tab, kind, button, x, y, keys });
        let key = |keyval, pressed, keys| self.value(Op::Key { tab, keyval, pressed, keys });
        let shown = |x: f64, y: f64| format!("({}, {})", (x / scale).round(), (y / scale).round());
        let with_settle = |mut text: String, note: Option<String>| -> Result<Value, String> {
            if let Some(n) = note {
                text = format!("{text}\n{n}");
            }
            if let Some(s) = self.settle(id)? {
                text = format!("{text}\n{s}");
            }
            Ok(Value::String(text))
        };

        match action {
            "screenshot" => self.screenshot(id, None),
            "zoom" => {
                let r = args.get("region").and_then(Value::as_array).ok_or("zoom needs `region` [x0, y0, x1, y1]")?;
                let n: Vec<f64> = r.iter().filter_map(Value::as_f64).map(|v| v * scale).collect();
                if n.len() != 4 || n[2] <= n[0] || n[3] <= n[1] {
                    return Err("`region` is [x0, y0, x1, y1] with x1 > x0 and y1 > y0".into());
                }
                self.screenshot(id, Some((n[0], n[1], n[2], n[3])))
            }
            "left_click" | "right_click" | "middle_click" | "double_click" | "triple_click" => {
                let (x, y, note) = target(action)?;
                let (button, presses) = match action {
                    "right_click" => (3, 1),
                    "middle_click" => (2, 1),
                    "double_click" => (1, 2),
                    "triple_click" => (1, 3),
                    _ => (1, 1),
                };
                pointer(PointerKind::Move, 0, x, y)?;
                for _ in 0..presses {
                    pointer(PointerKind::Down, button, x, y)?;
                    pointer(PointerKind::Up, button, x, y)?;
                }
                with_settle(format!("{} at {}", action.replace('_', " "), shown(x, y)), note)
            }
            "hover" => {
                let (x, y, note) = target("hover")?;
                pointer(PointerKind::Move, 0, x, y)?;
                Ok(Value::String(format!("hovering at {}{}", shown(x, y), note.map(|n| format!("\n{n}")).unwrap_or_default())))
            }
            "left_click_drag" => {
                let (sx, sy) = coord_arg(args, "start_coordinate").ok_or("a drag needs `start_coordinate`")?;
                let (ex, ey) = coord_arg(args, "coordinate").ok_or("a drag needs `coordinate`")?;
                let (sx, sy, ex, ey) = (sx * scale, sy * scale, ex * scale, ey * scale);
                pointer(PointerKind::Move, 0, sx, sy)?;
                pointer(PointerKind::Down, 1, sx, sy)?;
                for i in 1..=12 {
                    let t = i as f64 / 12.0;
                    pointer(PointerKind::Move, 0, sx + (ex - sx) * t, sy + (ey - sy) * t)?;
                    std::thread::sleep(Duration::from_millis(8));
                }
                pointer(PointerKind::Up, 1, ex, ey)?;
                with_settle(format!("dragged from {} to {}", shown(sx, sy), shown(ex, ey)), None)
            }
            "type" => {
                let text = str_arg(args, "text")?;
                for c in text.chars() {
                    let sym = match c {
                        '\n' | '\r' => keysym::RETURN,
                        '\t' => keysym::TAB,
                        c => unicode_keysym(c),
                    };
                    key(sym, true, keys)?;
                    key(sym, false, keys)?;
                }
                with_settle(format!("typed {} character(s)", text.chars().count()), None)
            }
            "key" => {
                let text = str_arg(args, "text")?;
                let repeat = args.get("repeat").and_then(Value::as_u64).unwrap_or(1).clamp(1, 100);
                let chords = text.split_whitespace().map(chord).collect::<Result<Vec<_>, _>>()?;
                for _ in 0..repeat {
                    for &(sym, mods) in &chords {
                        key(sym, true, mods | keys)?;
                        key(sym, false, mods | keys)?;
                    }
                }
                with_settle(format!("pressed {text}{}", if repeat > 1 { format!(" x{repeat}") } else { String::new() }), None)
            }
            "scroll" => {
                let (x, y) = match (args.get("ref").and_then(Value::as_str), coord_arg(args, "coordinate")) {
                    (Some(_), _) => target("scroll").map(|(x, y, _)| (x, y))?,
                    (None, Some((x, y))) => (x * scale, y * scale),
                    (None, None) => {
                        let s = self.page(tab, "JSON.stringify([innerWidth / 2, innerHeight / 2])")?;
                        (s[0].as_f64().unwrap_or(400.0), s[1].as_f64().unwrap_or(300.0))
                    }
                };
                let dir = args.get("scroll_direction").and_then(Value::as_str).unwrap_or("down");
                let amount = args.get("scroll_amount").and_then(Value::as_f64).unwrap_or(3.0).clamp(0.0, 30.0);
                // cce-ui's convention, which WPE takes as is: positive is
                // toward the top or the left.
                let step = 100.0 / WHEEL_GAIN;
                let (dx, dy) = match dir {
                    "up" => (0.0, step),
                    "down" => (0.0, -step),
                    "left" => (step, 0.0),
                    "right" => (-step, 0.0),
                    other => return Err(format!("`scroll_direction` is up, down, left or right, not {other}")),
                };
                pointer(PointerKind::Move, 0, x, y)?;
                for _ in 0..amount.round() as u32 {
                    self.value(Op::Wheel { tab, dx, dy, x, y })?;
                }
                std::thread::sleep(Duration::from_millis(100));
                let s = self.page(tab, "JSON.stringify([Math.round(scrollX), Math.round(scrollY)])")?;
                Ok(Value::String(format!("scrolled {dir} by {amount} at {}; the page is at {},{}", shown(x, y), s[0], s[1])))
            }
            "scroll_to" => {
                let r = str_arg(args, "ref")?;
                self.page(tab, &format!("__cceAgent.scrollTo({})", json!(r)))?;
                Ok(Value::String(format!("scrolled {r} into view")))
            }
            "wait" => {
                let s = args.get("duration").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.0, 10.0);
                std::thread::sleep(Duration::from_secs_f64(s));
                Ok(Value::String(format!("waited {s}s")))
            }
            other => Err(format!("unknown action {other}")),
        }
    }

    /// A screenshot of what the tab shows, or of `region` (CSS px) of it at
    /// full resolution.
    fn screenshot(&self, id: u64, region: Option<(f64, f64, f64, f64)>) -> Result<Value, String> {
        let (img, (vw, vh)) = match self.op(Op::Snapshot(Some(id)))? {
            Answer::Image(img, viewport) => (img, viewport),
            Answer::Value(_) => return Err("no snapshot".into()),
        };
        // Device pixels per CSS pixel in this snapshot.
        let k = img.width as f64 / vw.max(1.0);
        let (crop, base) = match region {
            None => ((0.0, 0.0, img.width as f64, img.height as f64), (vw, vh)),
            Some((x0, y0, x1, y1)) => {
                let c = (x0 * k, y0 * k, (x1 * k).min(img.width as f64), (y1 * k).min(img.height as f64));
                if c.2 <= c.0 || c.3 <= c.1 {
                    return Err("the region is outside the viewport".into());
                }
                (c, (c.2 - c.0, c.3 - c.1))
            }
        };
        let (ow, oh) = fit(base.0, base.1);
        let png = encode_png(&img, crop, ow, oh)?;
        let data = base64::engine::general_purpose::STANDARD.encode(png);
        let caption = match region {
            None => {
                // What the agent's coordinates mean from now on.
                SHOT_SCALE.lock().unwrap().get_or_insert_with(HashMap::new).insert(id, vw / ow as f64);
                format!(
                    "Tab {id}: {ow}x{oh} screenshot of a {vw}x{vh} CSS px viewport. computer coordinates for this tab are in this screenshot's pixels."
                )
            }
            Some(_) => format!("Tab {id}: zoomed region at {ow}x{oh}."),
        };
        Ok(cce_ui::mcp::content(vec![
            cce_ui::mcp::image_block(data, "image/png"),
            cce_ui::mcp::text_block(caption),
        ]))
    }
}

/// How far WebKit moves a page per pixel of precise wheel delta — its own
/// multiplier, the 76 → 190 in CLAUDE.md's wheel notes. Measured with
/// `scroll`, which reports where the page got to: three steps of 100 sent
/// unscaled landed at 750.
const WHEEL_GAIN: f64 = 2.5;

/// The largest size within the screenshot budget, never larger than `w`x`h`.
fn fit(w: f64, h: f64) -> (u32, u32) {
    let s = (SHOT_EDGE / w.max(h)).min((SHOT_AREA / (w * h).max(1.0)).sqrt()).min(1.0);
    (((w * s).round() as u32).max(1), ((h * s).round() as u32).max(1))
}

/// `crop` of a premultiplied BGRA snapshot, box-filtered down to `ow`x`oh`
/// and composited over white, as an RGB PNG.
fn encode_png(img: &AgentImage, crop: (f64, f64, f64, f64), ow: u32, oh: u32) -> Result<Vec<u8>, String> {
    let (cx, cy, cw, ch) = (crop.0, crop.1, crop.2 - crop.0, crop.3 - crop.1);
    let (sw, sh) = (img.width as usize, img.height as usize);
    let mut rgb = Vec::with_capacity(ow as usize * oh as usize * 3);
    for oy in 0..oh as usize {
        let y0 = (cy + ch * oy as f64 / oh as f64).floor() as usize;
        let y1 = ((cy + ch * (oy + 1) as f64 / oh as f64).ceil() as usize).clamp(y0 + 1, sh);
        for ox in 0..ow as usize {
            let x0 = (cx + cw * ox as f64 / ow as f64).floor() as usize;
            let x1 = ((cx + cw * (ox + 1) as f64 / ow as f64).ceil() as usize).clamp(x0 + 1, sw);
            let (mut b, mut g, mut r, mut a, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for y in y0.min(sh - 1)..y1 {
                let row = &img.bgra[y * sw * 4..];
                for x in x0.min(sw - 1)..x1 {
                    let p = &row[x * 4..x * 4 + 4];
                    b += p[0] as u32;
                    g += p[1] as u32;
                    r += p[2] as u32;
                    a += p[3] as u32;
                    n += 1;
                }
            }
            let n = n.max(1);
            let white = 255 - a / n;
            rgb.extend_from_slice(&[(r / n + white).min(255) as u8, (g / n + white).min(255) as u8, (b / n + white).min(255) as u8]);
        }
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, ow, oh);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut w = enc.write_header().map_err(|e| e.to_string())?;
        w.write_image_data(&rgb).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

// ---- keys ----

/// X11 keysyms (`/usr/include/X11/keysymdef.h`).
mod keysym {
    pub const RETURN: u32 = 0xff0d;
    pub const TAB: u32 = 0xff09;
}

/// A character as an X11 keysym: Latin-1 is itself, the rest is the
/// codepoint in the 0x01000000 plane (as cce-wpe's `input` maps them).
fn unicode_keysym(c: char) -> u32 {
    match c as u32 {
        cp @ 0x20..=0xff => cp,
        cp => cp + 0x0100_0000,
    }
}

/// `ctrl+shift+t`, `Return`, `a` — xdotool's spelling, which is what the
/// computer tool's key action is written in — as a keysym and modifiers.
/// `cmd`/`meta`/`super` are Ctrl: the chords a Mac spells with Cmd are
/// Ctrl chords here.
fn chord(text: &str) -> Result<(u32, u32), String> {
    let parts: Vec<&str> = text.split('+').collect();
    let (name, mods) = match parts.split_last() {
        // A literal "+" (or a chord ending in it) splits to an empty name.
        Some((&"", rest)) if !rest.is_empty() => ("+", &rest[..rest.len() - 1]),
        Some((name, rest)) => (*name, rest),
        None => return Err("empty key".into()),
    };
    let keys = modifiers(&mods.join("+"))?;
    let lower = name.to_ascii_lowercase();
    let sym = match lower.as_str() {
        "return" | "enter" | "kp_enter" => keysym::RETURN,
        "tab" => keysym::TAB,
        "escape" | "esc" => 0xff1b,
        "backspace" => 0xff08,
        "delete" | "del" => 0xffff,
        "insert" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "left" | "arrowleft" => 0xff51,
        "up" | "arrowup" => 0xff52,
        "right" | "arrowright" => 0xff53,
        "down" | "arrowdown" => 0xff54,
        "page_up" | "pageup" | "prior" => 0xff55,
        "page_down" | "pagedown" | "next" => 0xff56,
        "space" => 0x20,
        "plus" => '+' as u32,
        "minus" => '-' as u32,
        f if f.len() >= 2 && f.starts_with('f') && f[1..].parse::<u32>().is_ok_and(|n| (1..=12).contains(&n)) => {
            0xffbe + f[1..].parse::<u32>().unwrap() - 1
        }
        _ => {
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => unicode_keysym(c),
                _ => return Err(format!("unknown key {name:?}")),
            }
        }
    };
    Ok((sym, keys))
}

/// `ctrl+shift` and the like, as WPE keyboard modifier bits.
fn modifiers(text: &str) -> Result<u32, String> {
    let mut keys = 0u32;
    for m in text.split('+').map(str::trim).filter(|m| !m.is_empty()) {
        keys |= match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "ctl" | "cmd" | "command" | "meta" | "super" | "win" => {
                WPEModifiers::WPE_MODIFIER_KEYBOARD_CONTROL
            }
            "shift" => WPEModifiers::WPE_MODIFIER_KEYBOARD_SHIFT,
            "alt" | "option" | "opt" => WPEModifiers::WPE_MODIFIER_KEYBOARD_ALT,
            other => return Err(format!("unknown modifier {other:?}")),
        };
    }
    Ok(keys)
}

// ---- the tools ----

fn tools() -> Vec<McpTool> {
    let tool = |name: &str, description: &str, input_schema: Value| McpTool {
        name: name.into(),
        description: description.into(),
        input_schema,
    };
    let tab_id = json!({ "type": "integer", "description": "The tab to act on (from tabs_context). Default: the tab the browser is showing." });
    vec![
        tool(
            "tabs_context",
            "List the browser's open tabs: tabId, url, title, whether it is loading, and which one is shown (active). Call this first.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "tabs_create",
            "Open a new tab, in the background unless `foreground`, and wait for its page to load. Returns the new tab, with its tabId — pass that to the other tools to work in it without disturbing the tab the person is looking at.",
            json!({ "type": "object", "properties": {
                "url": { "type": "string", "description": "URL, domain, or search words (as typed in the URL bar). Default about:blank." },
                "foreground": { "type": "boolean", "description": "Show the new tab. Default false." }
            } }),
        ),
        tool(
            "tabs_close",
            "Close a tab.",
            json!({ "type": "object", "properties": { "tabId": tab_id }, "required": ["tabId"] }),
        ),
        tool(
            "tabs_select",
            "Show a tab in the browser window.",
            json!({ "type": "object", "properties": { "tabId": tab_id }, "required": ["tabId"] }),
        ),
        tool(
            "navigate",
            "Load a URL in a tab (a bare domain or search words work as in the URL bar), or go \"back\" / \"forward\" / \"reload\". Waits for the page to load, then returns its url and title.",
            json!({ "type": "object", "properties": {
                "url": { "type": "string", "description": "URL, domain, search words, or back / forward / reload" },
                "tabId": tab_id
            }, "required": ["url"] }),
        ),
        tool(
            "read_page",
            "The page as an accessibility-style tree: one line per element, `role \"name\" [ref_N] state…`, indented by nesting. Refs name elements for computer (ref), form_input and scroll_to; they stay valid while the element is on the page. filter \"interactive\" lists only what can be clicked or typed into — much shorter on a big page. Hidden elements are left out; same-origin frames and open shadow roots are included.",
            json!({ "type": "object", "properties": {
                "filter": { "type": "string", "enum": ["all", "interactive"], "description": "Default all" },
                "ref_id": { "type": "string", "description": "Read only this element's subtree" },
                "depth": { "type": "integer", "description": "Deepest nesting shown (default 40)" },
                "max_chars": { "type": "integer", "description": "Default 50000" },
                "tabId": tab_id
            } }),
        ),
        tool(
            "find",
            "Find elements whose tree line (role, name, text, value, href) contains the query, ignoring case, or every word of it. Up to 20, each with its ref and its middle in viewport CSS px.",
            json!({ "type": "object", "properties": {
                "query": { "type": "string" },
                "tabId": tab_id
            }, "required": ["query"] }),
        ),
        tool(
            "get_page_text",
            "The page's visible text — its main/article content when it has some, else the whole body.",
            json!({ "type": "object", "properties": {
                "max_chars": { "type": "integer", "description": "Default 50000" },
                "tabId": tab_id
            } }),
        ),
        tool(
            "form_input",
            "Set a form control by ref: text for an input, textarea or contenteditable; an option's value or text for a select; true/false for a checkbox or radio. Fires the input/change events a page listens for. To type as keystrokes instead, click the field and use computer type.",
            json!({ "type": "object", "properties": {
                "ref": { "type": "string" },
                "value": { "description": "string, number or boolean" },
                "tabId": tab_id
            }, "required": ["ref", "value"] }),
        ),
        tool(
            "computer",
            "Mouse, keyboard and screen, as real input events (the page sees a person's clicks and keys).\n\
* screenshot: what the tab shows. After one, `coordinate` values for that tab are in that screenshot's pixels (before any, in CSS px).\n\
* left_click / right_click / middle_click / double_click / triple_click / hover: at `coordinate` [x, y], or at the middle of `ref` (scrolled into view first). A middle click on a link opens it in a background tab. Clicks wait for a page load they start.\n\
* left_click_drag: from `start_coordinate` to `coordinate`.\n\
* type: `text` as keystrokes into whatever has focus (click the field first).\n\
* key: `text` is one or more space-separated keys or chords in xdotool spelling: \"Return\", \"Tab\", \"Escape\", \"BackSpace\", \"ctrl+a\", \"shift+Tab\", \"Down\". cmd/meta mean ctrl. `repeat` presses it again.\n\
* scroll: `scroll_direction` up/down/left/right by `scroll_amount` steps of ~100 CSS px (default 3), at `coordinate` (whatever scrolls there moves), `ref`, or the viewport's middle.\n\
* scroll_to: bring `ref` into view.\n\
* zoom: `region` [x0, y0, x1, y1] (screenshot pixels) at full resolution, for small text.\n\
* wait: `duration` seconds (max 10).\n\
`modifiers` (\"ctrl\", \"shift\", \"alt\", combined with +) are held for a click or a key.",
            json!({ "type": "object", "properties": {
                "action": { "type": "string", "enum": [
                    "screenshot", "left_click", "right_click", "middle_click", "double_click", "triple_click",
                    "hover", "left_click_drag", "type", "key", "scroll", "scroll_to", "zoom", "wait"
                ] },
                "coordinate": { "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2 },
                "start_coordinate": { "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2 },
                "ref": { "type": "string", "description": "An element ref from read_page or find" },
                "text": { "type": "string" },
                "modifiers": { "type": "string" },
                "repeat": { "type": "integer" },
                "scroll_direction": { "type": "string", "enum": ["up", "down", "left", "right"] },
                "scroll_amount": { "type": "number" },
                "region": { "type": "array", "items": { "type": "number" }, "minItems": 4, "maxItems": 4 },
                "duration": { "type": "number" },
                "tabId": tab_id
            }, "required": ["action"] }),
        ),
        tool(
            "handle_dialog",
            "Answer the alert, confirm or prompt a page has open. While one is open the page is blocked: tools that read it refuse, and click results say a dialog appeared. The person sees the same dialog in the window.",
            json!({ "type": "object", "properties": {
                "accept": { "type": "boolean", "description": "OK (true) or Cancel (false)" },
                "text": { "type": "string", "description": "For a prompt: the text to answer with" }
            }, "required": ["accept"] }),
        ),
        tool(
            "javascript_tool",
            "Run JavaScript in the page's own context and return the result as JSON. Read like a console: an expression's value (a promise is awaited, so `await fetch(...)` works as an expression), else the last statement's value. Statements that use `await` run as an async function body: `return` the answer.",
            json!({ "type": "object", "properties": {
                "text": { "type": "string", "description": "The code" },
                "tabId": tab_id
            }, "required": ["text"] }),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_errors_lose_their_wrapper_location() {
        assert_eq!(tidy_js_error("http://127.0.0.1:8765/:27:105: Error: ref_1 is gone".into()), "Error: ref_1 is gone");
        assert_eq!(tidy_js_error("about:blank:1: SyntaxError: Unexpected end".into()), "SyntaxError: Unexpected end");
        assert_eq!(tidy_js_error("TypeError: x is not a function".into()), "TypeError: x is not a function");
    }

    #[test]
    fn chords_in_xdotool_spelling() {
        let ctrl = WPEModifiers::WPE_MODIFIER_KEYBOARD_CONTROL as u32;
        let shift = WPEModifiers::WPE_MODIFIER_KEYBOARD_SHIFT as u32;
        assert_eq!(chord("Return"), Ok((0xff0d, 0)));
        assert_eq!(chord("ctrl+a"), Ok(('a' as u32, ctrl)));
        assert_eq!(chord("cmd+shift+T"), Ok(('T' as u32, ctrl | shift)));
        assert_eq!(chord("shift+Tab"), Ok((0xff09, shift)));
        assert_eq!(chord("F5"), Ok((0xffc2, 0)));
        assert_eq!(chord("ctrl++"), Ok(('+' as u32, ctrl)));
        assert_eq!(chord("Page_Down"), Ok((0xff56, 0)));
        assert_eq!(chord("é"), Ok((0xe9, 0)));
        assert!(chord("hyper+x").is_err());
        assert!(chord("Frobnicate").is_err());
    }

    #[test]
    fn screenshots_fit_the_budget_and_never_grow() {
        assert_eq!(fit(800.0, 600.0), (800, 600));
        let (w, h) = fit(1920.0, 1200.0);
        assert!(w as f64 * h as f64 <= SHOT_AREA && w <= 1568, "{w}x{h}");
        assert!((w as f64 / h as f64 - 1.6).abs() < 0.01);
        let (w, h) = fit(3000.0, 400.0);
        assert_eq!(w, 1568);
        assert!(h < 400);
    }

    #[test]
    fn a_premultiplied_snapshot_lands_on_white() {
        // 2x1: opaque pure blue (BGRA), fully transparent.
        let img = AgentImage { width: 2, height: 1, bgra: vec![255, 0, 0, 255, 0, 0, 0, 0] };
        let png = encode_png(&img, (0.0, 0.0, 2.0, 1.0), 2, 1).unwrap();
        let dec = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        reader.next_frame(&mut buf).unwrap();
        assert_eq!(&buf[..6], &[0, 0, 255, 255, 255, 255]);
    }

    #[test]
    fn every_tool_is_described_with_an_object_schema() {
        for t in tools() {
            assert_eq!(t.input_schema["type"], "object", "{}", t.name);
            assert!(!t.description.is_empty());
        }
    }
}
