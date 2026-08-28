//! cce-browser — a web browser on the embedded Servo engine.
//!
//! Servo renders pages into a CPU (software) rendering context; each
//! finished frame is read back and uploaded to cce-ui's image registry,
//! then drawn as a single quad under a thin chrome bar (back / forward /
//! reload / URL field). Input over the page area is translated into Servo
//! input events; the URL bar is a small hand-rolled line editor.

mod downloads;
#[cfg(feature = "wpe")]
mod lineedit;
mod pages;
mod settings;
mod webview;
/// The in-progress WPE WebKit backend (see WPE-PORT.md). Compiled only under
/// `--features wpe`; the shipping browser is still Servo.
#[cfg(feature = "wpe")]
mod wpe;

use url::Url;
use wayland_client::QueueHandle;

use cce_ui::engine::{Application, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{DisplayList, PaintCtx};
use cce_ui::widget::display::measure_text_width;
use cce_ui::widget::{ElementState, Key, KeyEvent, MouseButton, MouseScrollDelta, NamedKey};

#[cfg(not(feature = "wpe"))]
use webview::ServoHost as Host;
#[cfg(feature = "wpe")]
use wpe::WebKitHost as Host;

/// Clipboard action, named by neither engine. Each backend maps it to its
/// own vocabulary — Servo needs an `EditingActionEvent`, WebKit a named
/// editing command — so the chrome never learns either.
#[derive(Debug, Clone, Copy)]
pub enum EditingCommand {
    Copy,
    Cut,
    Paste,
}

const BAR_MARGIN: f32 = 10.0;
/// Two rows: tab strip on top, nav controls + URL field below.
const BAR_H: f32 = BAR_PAD + TAB_H + ROW_GAP + BTN_H + BAR_PAD;
const BAR_RADIUS: f32 = 10.0;
const BAR_PAD: f32 = 7.0;
const TAB_H: f32 = 24.0;
const TAB_GAP: f32 = 4.0;
const TAB_MIN_W: f32 = 56.0;
const TAB_MAX_W: f32 = 200.0;
/// Tabs at least this wide get a close "x" region on their right edge.
const TAB_CLOSE_MIN_W: f32 = 72.0;
const TAB_CLOSE_W: f32 = 18.0;
const PLUS_W: f32 = 26.0;
const ROW_GAP: f32 = 6.0;
/// Utility-bar fill; the negative alpha marks the plate as blur-behind.
/// The blurred page is the base and this color tints it at |alpha|
/// opacity — keep |alpha| low so the frosted content shows through.
const BAR_FILL: [f32; 4] = [0.11, 0.12, 0.13, -0.28];
const BTN_W: f32 = 30.0;
const BTN_H: f32 = 26.0;
const BTN_GAP: f32 = 6.0;
const URL_FONT: f32 = 14.0;
const URL_PAD_X: f32 = 9.0;
/// Pixels per wheel notch when the DE reports discrete line deltas.
const LINE_PX: f64 = 76.0;


const PAGE_BG: [f32; 4] = [0.10, 0.10, 0.11, 1.0];
const FIELD_BG: [f32; 4] = [0.09, 0.09, 0.10, 0.40];
const BTN_BG: [f32; 4] = [0.20, 0.21, 0.23, 0.40];
const TAB_BG: [f32; 4] = [0.15, 0.16, 0.18, 0.30];
const TAB_ACTIVE_BG: [f32; 4] = [0.32, 0.34, 0.38, 0.55];
const RIM: [f32; 4] = [0.22, 0.23, 0.25, 1.0];
const RIM_FOCUS: [f32; 4] = [0.33, 0.48, 0.72, 1.0];
/// URL-bar selection highlight; the text is drawn over it.
const SEL_BG: [f32; 4] = [0.24, 0.38, 0.60, 0.95];
const ACCENT: [f32; 4] = [0.35, 0.55, 0.85, 1.0];
const TEXT: [u8; 3] = [220, 220, 225];
const TEXT_DIM: [u8; 3] = [120, 122, 128];

/// A page-blocking prompt drawn over the content.
///
/// Modal on purpose: the page is genuinely blocked inside WebKit until it is
/// answered, so letting the chrome carry on as if nothing were pending would
/// misrepresent what the engine is doing.
#[cfg(feature = "wpe")]
struct Modal {
    title: String,
    message: String,
    /// Editable fields, in tab order. Empty for a bare alert or confirm.
    fields: Vec<(&'static str, lineedit::LineEdit)>,
    focused: usize,
    has_cancel: bool,
    kind: ModalKind,
}

#[cfg(feature = "wpe")]
enum ModalKind {
    /// `alert` / `confirm` / `prompt`.
    Script,
    /// An HTTP auth challenge.
    Auth,
}

#[cfg(feature = "wpe")]
const MODAL_W: f32 = 420.0;
#[cfg(feature = "wpe")]
const MODAL_PAD: f32 = 18.0;
#[cfg(feature = "wpe")]
const MODAL_FIELD_H: f32 = 26.0;
#[cfg(feature = "wpe")]
const MODAL_BTN_W: f32 = 84.0;

#[cfg(feature = "wpe")]
impl Modal {
    fn height(&self) -> f32 {
        MODAL_PAD * 2.0
            + 20.0
            + 22.0
            + self.fields.len() as f32 * (MODAL_FIELD_H + 8.0)
            + 12.0
            + BTN_H
    }

    /// Centred, and clamped so it stays on screen on a small window.
    fn rect(&self, win: (f32, f32)) -> Rect {
        let w = MODAL_W.min(win.0 - 40.0).max(240.0);
        let h = self.height();
        Rect {
            x: ((win.0 - w) / 2.0).max(0.0),
            y: ((win.1 - h) / 2.0).max(0.0),
            width: w,
            height: h,
        }
    }

    fn field_rect(&self, r: &Rect, i: usize) -> Rect {
        Rect {
            x: r.x + MODAL_PAD,
            y: r.y + MODAL_PAD + 42.0 + i as f32 * (MODAL_FIELD_H + 8.0),
            width: r.width - MODAL_PAD * 2.0,
            height: MODAL_FIELD_H,
        }
    }

    /// (ok, cancel) — cancel is `None` for a bare alert.
    fn button_rects(&self, r: &Rect) -> (Rect, Option<Rect>) {
        let y = r.y + r.height - MODAL_PAD - BTN_H;
        let ok = Rect {
            x: r.x + r.width - MODAL_PAD - MODAL_BTN_W,
            y,
            width: MODAL_BTN_W,
            height: BTN_H,
        };
        let cancel = self.has_cancel.then(|| Rect {
            x: ok.x - MODAL_BTN_W - BTN_GAP,
            ..ok
        });
        (ok, cancel)
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    /// Servo requested an event-loop spin (waker or delegate signal).
    Spin,
    /// Last tab closed: exit the app.
    Quit,
}

struct BrowserApp {
    host: Host,
    /// Loaded from the app config; re-read when the window regains focus.
    settings: settings::Settings,
    win: (f32, f32),
    scale: f64,
    pointer: (f32, f32),
    /// URL bar contents; mirrors the page URL unless the bar is focused.
    url_input: String,
    url_focused: bool,
    /// Byte index of the URL-bar cursor.
    cursor: usize,
    /// Selected byte range in the URL bar, normalized (start < end). Set by
    /// clicking into an unfocused bar; any edit replaces or drops it.
    selection: Option<(usize, usize)>,
    loading: bool,
    /// Page title; drives the toplevel title (the engine re-applies
    /// `settings().title` whenever it changes).
    title: Option<String>,
    /// The page-blocking dialog or auth challenge currently on screen, if
    /// any. Only the WPE backend raises these — Servo has no delegate hooks
    /// for them, which is why they were listed as "not implemented".
    #[cfg(feature = "wpe")]
    modal: Option<Modal>,
    /// Kept so the WPE backend's calloop sources can fire `Spin`; Servo
    /// wakes the loop itself through its `EventLoopWaker`.
    #[cfg(feature = "wpe")]
    sender: calloop::channel::Sender<Message>,
    /// App-side bundled-fonts `FontSystem` (the same set the toolkit renders
    /// with) for URL-bar caret/click metrics via `shaped_cluster_offsets` —
    /// `measure_text_width`'s inked-extent numbers drift off the drawn glyphs.
    font_system: cce_ui::cosmic_text::FontSystem,
}

fn hit(r: &Rect, x: f32, y: f32) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

/// The floating utility bar, overlaid on the page content. Anchored to the
/// top or bottom window edge per the config; the page is full-bleed either
/// way, so nothing but the chrome geometry depends on this. Every other
/// bar-relative rect below is derived from this one — never from
/// `BAR_MARGIN` directly, or it would stay pinned to the top.
fn bar_rect(win: (f32, f32), position: settings::BarPosition) -> Rect {
    let y = match position {
        settings::BarPosition::Top => BAR_MARGIN,
        settings::BarPosition::Bottom => (win.1 - BAR_MARGIN - BAR_H).max(BAR_MARGIN),
    };
    Rect {
        x: BAR_MARGIN,
        y,
        width: (win.0 - 2.0 * BAR_MARGIN).max(120.0),
        height: BAR_H,
    }
}

/// Y of the tab-strip row.
fn tabs_y(bar: &Rect) -> f32 {
    bar.y + BAR_PAD
}

/// Y of the nav-controls row.
fn controls_y(bar: &Rect) -> f32 {
    bar.y + BAR_PAD + TAB_H + ROW_GAP
}

fn plus_rect(bar: &Rect) -> Rect {
    Rect {
        x: bar.x + bar.width - BAR_PAD - PLUS_W,
        y: tabs_y(bar),
        width: PLUS_W,
        height: TAB_H,
    }
}

fn tab_rect(bar: &Rect, count: usize, i: usize) -> Rect {
    let avail = bar.width - 2.0 * BAR_PAD - PLUS_W - TAB_GAP - (count.max(1) - 1) as f32 * TAB_GAP;
    let w = (avail / count.max(1) as f32).clamp(TAB_MIN_W, TAB_MAX_W);
    Rect {
        x: bar.x + BAR_PAD + i as f32 * (w + TAB_GAP),
        y: tabs_y(bar),
        width: w,
        height: TAB_H,
    }
}

/// The close "x" hit region on a tab pill, when the pill is wide enough.
fn tab_close_rect(pill: &Rect) -> Option<Rect> {
    (pill.width >= TAB_CLOSE_MIN_W).then(|| Rect {
        x: pill.x + pill.width - TAB_CLOSE_W,
        y: pill.y,
        width: TAB_CLOSE_W,
        height: pill.height,
    })
}

fn btn_rect(bar: &Rect, i: usize) -> Rect {
    Rect {
        x: bar.x + BAR_PAD + i as f32 * (BTN_W + BTN_GAP),
        y: controls_y(bar),
        width: BTN_W,
        height: BTN_H,
    }
}

/// The bookmark star, at the right end of the controls row.
fn star_rect(bar: &Rect) -> Rect {
    Rect {
        x: bar.x + bar.width - BAR_PAD - BTN_W,
        y: controls_y(bar),
        width: BTN_W,
        height: BTN_H,
    }
}

fn url_rect(bar: &Rect) -> Rect {
    let x = bar.x + BAR_PAD + 3.0 * (BTN_W + BTN_GAP) + 4.0;
    Rect {
        x,
        y: controls_y(bar),
        width: (bar.x + bar.width - BAR_PAD - BTN_W - BTN_GAP - x).max(60.0),
        height: BTN_H,
    }
}

/// Turn URL-bar input into something loadable: a real URL as-is, a bare
/// host gets https://, anything else becomes a search.
fn parse_url_input(input: &str, search_prefix: &str) -> Option<Url> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if s.eq_ignore_ascii_case("about:history") {
        return Url::parse("cce://history").ok();
    }
    if s.eq_ignore_ascii_case("about:bookmarks") {
        return Url::parse("cce://bookmarks").ok();
    }
    if s.eq_ignore_ascii_case("about:downloads") {
        return Url::parse("cce://downloads").ok();
    }
    if s.eq_ignore_ascii_case("about:cookies") {
        return Url::parse("cce://cookies").ok();
    }
    if let Ok(u) = Url::parse(s) {
        if matches!(u.scheme(), "http" | "https" | "file" | "data" | "about" | "cce") {
            return Some(u);
        }
    }
    if !s.contains(' ') && s.contains('.') {
        if let Ok(u) = Url::parse(&format!("https://{s}")) {
            return Some(u);
        }
    }
    let q: String = url::form_urlencoded::byte_serialize(s.as_bytes()).collect();
    Url::parse(&format!("{search_prefix}{q}")).ok()
}

/// Turn the startup argument into something loadable.
///
/// This is deliberately not [`parse_url_input`]: that one is the URL *bar*,
/// where a dotted word is meant to become a domain guess. Argv is different —
/// the desktop entry claims `text/html`, and the XDG spec lets a launcher pass
/// a local file for `%u` "either as a file: URL or as a file path". A plain
/// path takes the domain-guess branch and turns `/home/me/page.html` into
/// `https:///home/me/page.html`, so an existing path is resolved to a file:
/// URL first and only a non-path falls through to the bar's parsing.
fn parse_startup_arg(arg: &str, search_prefix: &str) -> Option<Url> {
    let path = std::path::Path::new(arg);
    if path.exists() {
        // Relative paths need the cwd joined on before file: URL conversion.
        if let Ok(abs) = std::fs::canonicalize(path) {
            if let Ok(u) = Url::from_file_path(&abs) {
                return Some(u);
            }
        }
    }
    parse_url_input(arg, search_prefix)
}

fn dom_button(button: MouseButton) -> Option<servo::MouseButton> {
    match button {
        MouseButton::Left => Some(servo::MouseButton::Left),
        MouseButton::Right => Some(servo::MouseButton::Right),
        MouseButton::Middle => Some(servo::MouseButton::Middle),
        _ => None,
    }
}

fn dom_key(key: &Key) -> Option<servo::Key> {
    Some(match key {
        Key::Character(s) => servo::Key::Character(s.clone()),
        Key::Named(NamedKey::Space) => servo::Key::Character(" ".into()),
        Key::Named(n) => servo::Key::Named(match n {
            NamedKey::Backspace => servo::NamedKey::Backspace,
            NamedKey::Tab => servo::NamedKey::Tab,
            NamedKey::Enter => servo::NamedKey::Enter,
            NamedKey::Escape => servo::NamedKey::Escape,
            NamedKey::ArrowDown => servo::NamedKey::ArrowDown,
            NamedKey::ArrowLeft => servo::NamedKey::ArrowLeft,
            NamedKey::ArrowRight => servo::NamedKey::ArrowRight,
            NamedKey::ArrowUp => servo::NamedKey::ArrowUp,
            NamedKey::End => servo::NamedKey::End,
            NamedKey::Home => servo::NamedKey::Home,
            NamedKey::PageDown => servo::NamedKey::PageDown,
            NamedKey::PageUp => servo::NamedKey::PageUp,
            NamedKey::Delete => servo::NamedKey::Delete,
            NamedKey::Control => servo::NamedKey::Control,
            NamedKey::Shift => servo::NamedKey::Shift,
            NamedKey::Alt => servo::NamedKey::Alt,
            NamedKey::Super => servo::NamedKey::Meta,
            NamedKey::F5 => servo::NamedKey::F5,
            NamedKey::Space => unreachable!(),
        }),
    })
}

fn prev_boundary(s: &str, i: usize) -> usize {
    let mut j = i;
    while j > 0 {
        j -= 1;
        if s.is_char_boundary(j) {
            return j;
        }
    }
    0
}

fn next_boundary(s: &str, i: usize) -> usize {
    let mut j = i;
    while j < s.len() {
        j += 1;
        if s.is_char_boundary(j) {
            return j;
        }
    }
    s.len()
}

impl BrowserApp {
    /// The utility bar's rect for the current window size and configured
    /// edge — the single source every chrome hit-test and draw reads.
    fn bar(&self) -> Rect {
        bar_rect(self.win, self.settings.bar_position)
    }

    /// The page fills the whole window; the utility bar floats above it.
    fn content_px(&self) -> (u32, u32) {
        (
            (self.win.0 as f64 * self.scale) as u32,
            (self.win.1 as f64 * self.scale) as u32,
        )
    }

    /// Pull delegate-observed page state into the chrome.
    fn sync_page_state(&mut self) {
        self.loading = self.host.loading();
        self.title = self.host.title().filter(|t| !t.is_empty());
        if !self.url_focused {
            if let Some(u) = self.host.url() {
                let s = u.to_string();
                self.url_input = if s == "about:blank" { String::new() } else { s };
                self.cursor = self.url_input.len();
                self.selection = None;
            }
        }
    }

    /// Adopt whatever the engine is blocked on. Returns whether the chrome
    /// needs redrawing.
    #[cfg(feature = "wpe")]
    fn sync_modal(&mut self) -> bool {
        if self.modal.is_some() {
            return false;
        }
        if let Some(d) = self.host.pending_dialog() {
            let mut fields = Vec::new();
            if let Some(default) = d.prompt_default.clone() {
                let mut e = lineedit::LineEdit::with_text(default);
                e.select_all();
                fields.push(("", e));
            }
            self.modal = Some(Modal {
                title: "This page says".to_string(),
                message: d.message,
                fields,
                focused: 0,
                has_cancel: d.has_cancel,
                kind: ModalKind::Script,
            });
            return true;
        }
        if let Some(a) = self.host.pending_auth() {
            let where_ = if a.realm.is_empty() {
                a.host.clone()
            } else {
                format!("{} — {}", a.host, a.realm)
            };
            self.modal = Some(Modal {
                title: if a.retry {
                    "Sign in failed — try again".to_string()
                } else {
                    "Sign in".to_string()
                },
                message: where_,
                fields: vec![
                    ("Username", lineedit::LineEdit::default()),
                    ("Password", lineedit::LineEdit::masked()),
                ],
                focused: 0,
                has_cancel: true,
                kind: ModalKind::Auth,
            });
            return true;
        }
        false
    }

    /// Answer the engine and dismiss. `ok` false is cancel.
    #[cfg(feature = "wpe")]
    fn close_modal(&mut self, ok: bool) {
        let Some(m) = self.modal.take() else { return };
        match m.kind {
            ModalKind::Script => {
                let text = m.fields.first().map(|(_, e)| e.text.clone());
                self.host.respond_dialog(ok, text.as_deref());
            }
            ModalKind::Auth => {
                if ok {
                    let user = m.fields[0].1.text.clone();
                    let password = m.fields[1].1.text.clone();
                    self.host.respond_auth(Some((&user, &password)));
                } else {
                    self.host.respond_auth(None);
                }
            }
        }
    }

    fn navigate(&mut self) {
        if let Some(url) = parse_url_input(&self.url_input, &self.settings.search_prefix) {
            self.host.load(url);
            self.url_focused = false;
            self.selection = None;
            self.loading = true;
        }
    }

    /// Pick up settings edits (system-interface, cce-data-editor) when the
    /// window regains focus. Returns whether anything changed.
    fn reload_settings(&mut self) -> bool {
        let new = settings::load();
        if new == self.settings {
            return false;
        }
        downloads::set_download_dir(new.download_dir.clone());
        self.host.set_history_enabled(new.history);
        self.host.set_color_scheme_dark(new.color_scheme.is_dark());
        self.host.set_force_dark(new.color_scheme.forces_dark());
        self.settings = new;
        true
    }

    /// Hand the current page to another browser — the escape hatch for the
    /// places Servo cannot follow, like a Cloudflare challenge that never
    /// completes.
    ///
    /// Prefers the configured command; otherwise asks XDG. The guard matters:
    /// cce-browser's own desktop entry claims http/https, so once it is the
    /// default handler, `xdg-open` would hand the page straight back to us.
    fn open_external(&mut self) {
        let Some(url) = self
            .host
            .url()
            .map(|u| u.to_string())
            .or_else(|| parse_url_input(&self.url_input, &self.settings.search_prefix).map(|u| u.to_string()))
        else {
            return;
        };
        let configured = self.settings.external_browser.clone();
        std::thread::spawn(move || {
            let command = match configured {
                Some(c) => c,
                None => {
                    let default = std::process::Command::new("xdg-mime")
                        .args(["query", "default", "x-scheme-handler/https"])
                        .output()
                        .ok()
                        .and_then(|o| String::from_utf8(o.stdout).ok())
                        .unwrap_or_default();
                    if default.trim_start().starts_with("cce-browser") {
                        log::warn!(
                            "cce-browser is the default https handler; set browser.external-browser                              to another command or this would just reopen here"
                        );
                        return;
                    }
                    "xdg-open".to_string()
                }
            };
            let mut parts = command.split_whitespace();
            let Some(program) = parts.next() else { return };
            let args: Vec<&str> = parts.collect();
            match std::process::Command::new(program).args(args).arg(&url).spawn() {
                Ok(_) => log::info!("handed {url} to {program}"),
                Err(e) => log::warn!("could not run {program}: {e}"),
            }
        });
    }

    /// New blank tab with the URL bar focused for typing.
    fn new_tab(&mut self) {
        let url = Url::parse("about:blank").expect("about:blank");
        self.host.open_tab(url);
        self.url_input.clear();
        self.cursor = 0;
        self.selection = None;
        self.url_focused = true;
        self.sync_page_state();
    }

    /// Close a tab; returns `Message::Quit` when it was the last one.
    fn close_tab(&mut self, index: usize) -> Option<Message> {
        if !self.host.close_tab(index) {
            return Some(Message::Quit);
        }
        self.url_focused = false;
        self.sync_page_state();
        None
    }

    fn switch_tab(&mut self, index: usize) {
        self.host.activate(index);
        self.url_focused = false;
        self.sync_page_state();
    }

    /// Show an internal page: reuse a tab already on it (reloading, so
    /// live pages like downloads refresh), otherwise open a new one.
    fn open_internal_page(&mut self, page: &str) {
        let Ok(url) = Url::parse(page) else { return };
        for i in 0..self.host.tab_count() {
            let on_page = self
                .host
                .tab(i)
                .and_then(|t| t.url.as_ref().map(|u| u.as_str().starts_with(page)))
                .unwrap_or(false);
            if on_page {
                self.switch_tab(i);
                self.host.reload();
                return;
            }
        }
        self.host.open_tab(url);
        self.url_focused = false;
        self.sync_page_state();
    }

    /// Widest prefix of `text` fitting `avail`, with a "…"-style tail cut.
    fn fit_text(text: &str, sans: &str, size: f32, avail: f32) -> String {
        if measure_text_width(text, sans, size) <= avail {
            return text.to_string();
        }
        let mut end = text.len();
        while end > 0 {
            end = prev_boundary(text, end);
            let cut = format!("{}...", &text[..end]);
            if measure_text_width(&cut, sans, size) <= avail {
                return cut;
            }
        }
        String::new()
    }

    /// Draw the page-blocking prompt, if one is up. Same primitives as the
    /// utility bar — there are no cce-ui widgets in this app — with a scrim
    /// over the page so it reads as blocked, which it genuinely is.
    #[cfg(feature = "wpe")]
    fn paint_modal(&mut self, pc: &mut PaintCtx, sans: &str) {
        let Some(m) = self.modal.as_ref() else { return };
        let r = m.rect(self.win);

        pc.quad(
            Rect { x: 0.0, y: 0.0, width: self.win.0, height: self.win.1 },
            [0.0, 0.0, 0.0, 0.45],
        );
        let radii = (BAR_RADIUS, BAR_RADIUS, BAR_RADIUS, BAR_RADIUS);
        pc.plate(r, radii, [0.13, 0.14, 0.16, 1.0], cce_ui::layout::bevel_width().min(4.0));

        pc.text(
            m.title.clone(),
            r.x + MODAL_PAD,
            r.y + MODAL_PAD,
            14.0,
            TEXT,
        );
        pc.text(
            Self::fit_text(&m.message, sans, 13.0, r.width - MODAL_PAD * 2.0),
            r.x + MODAL_PAD,
            r.y + MODAL_PAD + 22.0,
            13.0,
            TEXT_DIM,
        );

        for (i, (label, edit)) in m.fields.iter().enumerate() {
            let f = m.field_rect(&r, i);
            let focused = i == m.focused;
            pc.rounded_rect(
                Rect { x: f.x - 1.0, y: f.y - 1.0, width: f.width + 2.0, height: f.height + 2.0 },
                7.0,
                (true, true, true, true),
                if focused { RIM_FOCUS } else { RIM },
            );
            pc.rounded_rect(f, 6.0, (true, true, true, true), FIELD_BG);
            let ty = cce_ui::layout::align_text_y(f.y, f.height, URL_FONT, 0.0);
            // `display()` masks a password field; the text itself never
            // reaches the paint list.
            let shown = edit.display();
            if shown.is_empty() && !label.is_empty() {
                pc.text(*label, f.x + URL_PAD_X, ty, URL_FONT, TEXT_DIM);
            } else {
                pc.text(shown, f.x + URL_PAD_X, ty, URL_FONT, TEXT);
            }
        }

        let (ok, cancel) = m.button_rects(&r);
        for (rect, label, accent) in [(Some(ok), "OK", true), (cancel, "Cancel", false)]
            .into_iter()
            .filter_map(|(rc, l, a)| rc.map(|rc| (rc, l, a)))
        {
            pc.rounded_rect(
                rect,
                6.0,
                (true, true, true, true),
                if accent { ACCENT } else { BTN_BG },
            );
            let w = measure_text_width(label, sans, 13.0);
            pc.text(
                label,
                rect.x + (rect.width - w) / 2.0,
                cce_ui::layout::align_text_y(rect.y, rect.height, 13.0, 0.0),
                13.0,
                TEXT,
            );
        }
    }

    fn cursor_from_click(&mut self, click_x: f32, field: &Rect) -> usize {
        let rel = click_x - field.x - URL_PAD_X;
        // Boundary x offsets from the same shaped buffer the bar draws (font=None,
        // matching `pc.text`), then the closest boundary to the click.
        let offsets = cce_ui::engine::shaped_cluster_offsets(&mut self.font_system, &self.url_input, URL_FONT, None);
        offsets
            .iter()
            .min_by(|a, b| (a.1 - rel).abs().total_cmp(&(b.1 - rel).abs()))
            .map(|&(b, _)| b)
            .unwrap_or(self.url_input.len())
    }

    /// X offset (text-origin relative) of a byte index, off the same shaped
    /// buffer as `cursor_from_click`.
    fn x_offset(&mut self, byte: usize) -> f32 {
        let offsets = cce_ui::engine::shaped_cluster_offsets(&mut self.font_system, &self.url_input, URL_FONT, None);
        offsets
            .iter()
            .rev()
            .find(|&&(b, _)| b <= byte)
            .map(|&(_, x)| x)
            .unwrap_or(0.0)
    }

    /// Caret x offset for the current byte cursor.
    fn caret_offset(&mut self) -> f32 {
        self.x_offset(self.cursor)
    }

    /// Select the whole URL, caret at the end — what entering the bar does,
    /// whether from a click, Ctrl+L or Ctrl+A. No-op on an empty field.
    fn select_all_url(&mut self) {
        self.cursor = self.url_input.len();
        self.selection = (self.cursor > 0).then_some((0, self.cursor));
    }

    /// The selected substring, if a selection covers any text.
    fn selected_text(&self) -> Option<String> {
        self.selection
            .filter(|&(a, b)| a < b && b <= self.url_input.len())
            .map(|(a, b)| self.url_input[a..b].to_string())
    }

    /// Drop a selection, deleting its text first if it covers any. Returns
    /// whether text was removed, so edits can treat "replace the selection"
    /// and "act at the cursor" as one path.
    fn take_selection(&mut self) -> bool {
        match self.selection.take() {
            Some((a, b)) if a < b && b <= self.url_input.len() => {
                self.url_input.replace_range(a..b, "");
                self.cursor = a;
                true
            }
            _ => false,
        }
    }

    fn edit_url(&mut self, event: &KeyEvent) {
        match &event.logical_key {
            Key::Named(NamedKey::Enter) => self.navigate(),
            Key::Named(NamedKey::Escape) => {
                self.url_focused = false;
                self.selection = None;
                self.sync_page_state();
            }
            Key::Named(NamedKey::Backspace) => {
                if !self.take_selection() && self.cursor > 0 {
                    let prev = prev_boundary(&self.url_input, self.cursor);
                    self.url_input.replace_range(prev..self.cursor, "");
                    self.cursor = prev;
                }
            }
            Key::Named(NamedKey::Delete) => {
                if !self.take_selection() && self.cursor < self.url_input.len() {
                    let next = next_boundary(&self.url_input, self.cursor);
                    self.url_input.replace_range(self.cursor..next, "");
                }
            }
            // Arrows collapse a selection to the edge they move toward,
            // rather than stepping from the cursor.
            Key::Named(NamedKey::ArrowLeft) => {
                self.cursor = match self.selection.take() {
                    Some((a, _)) => a,
                    None => prev_boundary(&self.url_input, self.cursor),
                };
            }
            Key::Named(NamedKey::ArrowRight) => {
                self.cursor = match self.selection.take() {
                    Some((_, b)) => b,
                    None => next_boundary(&self.url_input, self.cursor),
                };
            }
            Key::Named(NamedKey::Home) => {
                self.selection = None;
                self.cursor = 0;
            }
            Key::Named(NamedKey::End) => {
                self.selection = None;
                self.cursor = self.url_input.len();
            }
            Key::Character(c) if event.ctrl => match c.as_str() {
                "u" => {
                    self.url_input.clear();
                    self.cursor = 0;
                    self.selection = None;
                }
                "a" => self.select_all_url(),
                "c" => {
                    if let Some(text) = self.selected_text() {
                        cce_ui::widget::clipboard::copy_to_clipboard(&text);
                    }
                }
                "x" => {
                    if let Some(text) = self.selected_text() {
                        cce_ui::widget::clipboard::copy_to_clipboard(&text);
                        self.take_selection();
                    }
                }
                "v" => {
                    if let Some(text) = cce_ui::widget::clipboard::read_from_clipboard() {
                        // The bar is one line: a multi-line paste would put
                        // text where the caret math cannot reach it.
                        let flat: String =
                            text.chars().filter(|c| !c.is_control()).collect();
                        if !flat.is_empty() {
                            self.take_selection();
                            self.url_input.insert_str(self.cursor, &flat);
                            self.cursor += flat.len();
                        }
                    }
                }
                _ => {}
            },
            _ => {
                let insert = match (&event.text, &event.logical_key) {
                    (Some(t), _) if !event.ctrl && !t.chars().any(char::is_control) => Some(t.clone()),
                    (None, Key::Named(NamedKey::Space)) => Some(" ".to_string()),
                    (None, Key::Character(c)) if !event.ctrl => Some(c.clone()),
                    _ => None,
                };
                if let Some(t) = insert {
                    self.take_selection();
                    self.url_input.insert_str(self.cursor, &t);
                    self.cursor += t.len();
                }
            }
        }
    }
}

impl Application for BrowserApp {
    type Message = Message;

    fn new(_qh: &QueueHandle<EngineState<Self>>, sender: calloop::channel::Sender<Self::Message>) -> Self {
        let settings = settings::load();
        downloads::set_download_dir(settings.download_dir.clone());
        // Optional CLI arg: the start URL (same parsing as the URL bar);
        // otherwise the configured homepage.
        let url = std::env::args()
            .nth(1)
            .and_then(|arg| parse_startup_arg(&arg, &settings.search_prefix))
            .or_else(|| parse_url_input(&settings.homepage, &settings.search_prefix))
            .unwrap_or_else(|| Url::parse(settings::DEFAULT_HOMEPAGE).expect("home url"));
        let url_input = url.to_string();
        let cursor = url_input.len();
        #[cfg(not(feature = "wpe"))]
        let mut host = Host::new(sender, url, (1200, 800), settings.color_scheme.forces_dark());
        #[cfg(feature = "wpe")]
        let mut host = {
            let _ = &sender; // WPE wakes through register_sources, not a waker
            Host::new(url, (1200, 800))
        };
        host.set_history_enabled(settings.history);
        host.set_color_scheme_dark(settings.color_scheme.is_dark());
        #[cfg(feature = "wpe")]
        host.set_force_dark(settings.color_scheme.forces_dark());
        Self {
            host,
            settings,
            win: (1200.0, 800.0),
            scale: 1.0,
            pointer: (0.0, 0.0),
            url_input,
            url_focused: false,
            cursor,
            selection: None,
            loading: true,
            title: None,
            #[cfg(feature = "wpe")]
            modal: None,
            #[cfg(feature = "wpe")]
            sender,
            font_system: cce_ui::create_font_system(),
        }
    }

    /// Wake on GLib activity rather than polling for it.
    ///
    /// Servo pushed `Message::Spin` into calloop from its own threads; WPE
    /// runs a GLib main context, so we register the epoll fd carrying its
    /// pollfd set plus a timer for the timeout GLib asks for. Both just fire
    /// `Spin`, which lands in `update` and calls `pump` — the same path the
    /// Servo waker used, so nothing downstream changes.
    #[cfg(feature = "wpe")]
    fn register_sources(&mut self, handle: &calloop::LoopHandle<'_, EngineState<Self>>) {
        use calloop::{generic::Generic, Interest, Mode, PostAction};

        if let Some(fd) = self.host.poll_fd_owned() {
            let tx = self.sender.clone();
            // Level-triggered: `pump` drains the epoll, so an un-consumed
            // socket re-arms rather than being missed.
            let source = Generic::new(fd, Interest::READ, Mode::Level);
            if let Err(e) = handle.insert_source(source, move |_, _, _| {
                let _ = tx.send(Message::Spin);
                Ok(PostAction::Continue)
            }) {
                log::warn!("could not watch the GLib fd ({e}); falling back to the timer alone");
            }
        }

        // GLib also asks to be woken on its own schedule (timeouts, animation
        // frames), which no fd reports. Re-armed from `poll_timeout` each
        // fire, so an idle page settles to long sleeps instead of a fixed tick.
        let tx = self.sender.clone();
        let timer = calloop::timer::Timer::from_duration(std::time::Duration::from_millis(16));
        if let Err(e) = handle.insert_source(timer, move |_, _, state| {
            let _ = tx.send(Message::Spin);
            let next = state
                .inner
                .as_ref()
                .and_then(|app| app.host.poll_timeout())
                .unwrap_or(std::time::Duration::from_millis(100))
                .clamp(
                    std::time::Duration::from_millis(4),
                    std::time::Duration::from_millis(250),
                );
            calloop::timer::TimeoutAction::ToDuration(next)
        }) {
            log::warn!("could not arm the GLib timer ({e})");
        }
    }

    fn settings(&self) -> WindowSettings {
        WindowSettings {
            title: self.title.clone().unwrap_or_else(|| "Browser".to_string()),
            app_id: "cce-browser".to_string(),
            width: 1200,
            height: 800,
            fullscreen: false,
            min_size: Some((480, 320)),
        }
    }

    fn update(&mut self, msg: Self::Message, needs_rebuild: &mut bool, exit: &mut bool) {
        match msg {
            Message::Spin => {
                let (new_frame, dirty) = self.host.pump();
                #[cfg(feature = "wpe")]
                if self.sync_modal() {
                    *needs_rebuild = true;
                }
                if self.host.take_download_started() {
                    self.open_internal_page("cce://downloads");
                }
                if dirty {
                    self.sync_page_state();
                }
                if new_frame || dirty {
                    *needs_rebuild = true;
                }
            }
            Message::Quit => *exit = true,
        }
    }

    fn tick(&mut self, _dt: f32, _needs_rebuild: &mut bool) {}

    fn handle_focus_change(&mut self, focused: bool, needs_rebuild: &mut bool) {
        // A settings change can move the bar to the other edge, so a reload
        // that changed anything has to redraw the chrome.
        if focused && self.reload_settings() {
            *needs_rebuild = true;
        }
    }

    fn handle_resize(&mut self, width: f32, height: f32, scale: f64) {
        self.win = (width, height);
        self.scale = scale;
        let (w, h) = self.content_px();
        self.host.resize(w, h, scale as f32);
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, _needs_rebuild: &mut bool) {
        self.pointer = (pos.x, pos.y);
        if !hit(&self.bar(), pos.x, pos.y) {
            let s = self.scale as f32;
            self.host.mouse_move(pos.x * s, pos.y * s);
        }
    }

    fn handle_mouse_input(
        &mut self,
        button: MouseButton,
        state: ElementState,
        pos: LogicalPosition,
        needs_rebuild: &mut bool,
    ) -> Option<Self::Message> {
        let pressed = state == ElementState::Pressed;

        #[cfg(feature = "wpe")]
        if self.modal.is_some() {
            if !pressed || button != MouseButton::Left {
                return None;
            }
            *needs_rebuild = true;
            let (hit_ok, hit_cancel, field) = {
                let m = self.modal.as_ref().unwrap();
                let r = m.rect(self.win);
                let (ok, cancel) = m.button_rects(&r);
                (
                    hit(&ok, pos.x, pos.y),
                    cancel.is_some_and(|c| hit(&c, pos.x, pos.y)),
                    (0..m.fields.len()).find(|&i| hit(&m.field_rect(&r, i), pos.x, pos.y)),
                )
            };
            if hit_ok {
                self.close_modal(true);
            } else if hit_cancel {
                self.close_modal(false);
            } else if let (Some(i), Some(m)) = (field, self.modal.as_mut()) {
                m.focused = i;
            }
            // Anything else is swallowed: the page must not receive clicks
            // while it is blocked waiting on this.
            return None;
        }

        let bar = self.bar();
        if hit(&bar, pos.x, pos.y) {
            if !pressed || !matches!(button, MouseButton::Left | MouseButton::Middle) {
                return None;
            }
            *needs_rebuild = true;
            // Tab strip: activate / close (x region or middle click) / new tab.
            let count = self.host.tab_count();
            for i in 0..count {
                let pill = tab_rect(&bar, count, i);
                if !hit(&pill, pos.x, pos.y) {
                    continue;
                }
                let on_close =
                    tab_close_rect(&pill).is_some_and(|r| hit(&r, pos.x, pos.y));
                if button == MouseButton::Middle || on_close {
                    return self.close_tab(i);
                }
                self.switch_tab(i);
                return None;
            }
            if button != MouseButton::Left {
                return None;
            }
            if hit(&plus_rect(&bar), pos.x, pos.y) {
                self.new_tab();
            } else if hit(&btn_rect(&bar, 0), pos.x, pos.y) {
                self.host.back();
            } else if hit(&btn_rect(&bar, 1), pos.x, pos.y) {
                self.host.forward();
            } else if hit(&btn_rect(&bar, 2), pos.x, pos.y) {
                self.host.reload();
            } else if hit(&star_rect(&bar), pos.x, pos.y) {
                self.host.toggle_bookmark();
            } else {
                let field = url_rect(&bar);
                if hit(&field, pos.x, pos.y) {
                    if self.url_focused {
                        self.cursor = self.cursor_from_click(pos.x, &field);
                        self.selection = None;
                    } else {
                        // Entering the bar selects the whole URL, so typing
                        // replaces it instead of appending to it.
                        self.url_focused = true;
                        self.select_all_url();
                    }
                } else {
                    self.url_focused = false;
                    self.selection = None;
                }
            }
            return None;
        }

        // Page area: a click dismisses URL-bar focus, then goes to the page.
        if self.url_focused && pressed {
            self.url_focused = false;
            self.selection = None;
            self.sync_page_state();
            *needs_rebuild = true;
        }
        match button {
            MouseButton::Back if pressed => self.host.back(),
            MouseButton::Forward if pressed => self.host.forward(),
            _ => {
                let s = self.scale as f32;
                self.host
                    .mouse_button_ui(button, pressed, pos.x * s, pos.y * s);
            }
        }
        None
    }

    fn handle_mouse_wheel(&mut self, delta: &MouseScrollDelta, pos: LogicalPosition, _needs_rebuild: &mut bool) {
        if hit(&self.bar(), pos.x, pos.y) {
            return;
        }
        // WheelDelta keeps cce-ui's winit sign convention (positive = scroll
        // up); Servo inverts it into the scroll offset internally, after the
        // page has had its preventDefault chance.
        let (dx, dy) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (*x as f64 * LINE_PX, *y as f64 * LINE_PX),
            MouseScrollDelta::PixelDelta(p) => (p.x, p.y),
        };
        let s = self.scale;
        self.host.wheel(dx * s, dy * s, pos.x * s as f32, pos.y * s as f32);
    }

    fn handle_key_input(&mut self, event: &KeyEvent, needs_rebuild: &mut bool) -> Option<Self::Message> {
        // A modal is exactly that: the page is blocked inside WebKit, so the
        // chrome's own chords must not fire behind it either.
        #[cfg(feature = "wpe")]
        if self.modal.is_some() {
            *needs_rebuild = true;
            if event.state == ElementState::Pressed
                && event.logical_key == Key::Named(NamedKey::Tab)
            {
                if let Some(m) = self.modal.as_mut() {
                    if !m.fields.is_empty() {
                        let n = m.fields.len();
                        m.focused = if event.shift {
                            (m.focused + n - 1) % n
                        } else {
                            (m.focused + 1) % n
                        };
                    }
                }
                return None;
            }
            let outcome = match self.modal.as_mut() {
                Some(m) if !m.fields.is_empty() => {
                    let i = m.focused;
                    m.fields[i].1.handle_key(event)
                }
                // No field: Enter accepts, Escape cancels, nothing else acts.
                Some(_) => match (&event.logical_key, event.state) {
                    (Key::Named(NamedKey::Enter), ElementState::Pressed) => {
                        lineedit::EditOutcome::Submit
                    }
                    (Key::Named(NamedKey::Escape), ElementState::Pressed) => {
                        lineedit::EditOutcome::Cancel
                    }
                    _ => lineedit::EditOutcome::Ignored,
                },
                None => lineedit::EditOutcome::Ignored,
            };
            match outcome {
                lineedit::EditOutcome::Submit => self.close_modal(true),
                lineedit::EditOutcome::Cancel => self.close_modal(false),
                _ => {}
            }
            return None;
        }

        // Tab shortcuts work regardless of URL-bar focus.
        if event.state == ElementState::Pressed && event.ctrl {
            let count = self.host.tab_count();
            match &event.logical_key {
                Key::Character(c) if c == "t" => {
                    self.new_tab();
                    *needs_rebuild = true;
                    return None;
                }
                Key::Character(c) if c == "w" => {
                    *needs_rebuild = true;
                    return self.close_tab(self.host.active_index());
                }
                // Ctrl+Shift+Delete opens the cookie page rather than
                // clearing outright; the page asks first.
                Key::Named(NamedKey::Delete) if event.shift => {
                    self.open_internal_page("cce://cookies");
                    *needs_rebuild = true;
                    return None;
                }
                Key::Character(c) if c == "h" || c == "b" || c == "j" => {
                    let page = match c.as_str() {
                        "h" => "cce://history",
                        "b" => "cce://bookmarks",
                        _ => "cce://downloads",
                    };
                    self.open_internal_page(page);
                    *needs_rebuild = true;
                    return None;
                }
                Key::Character(c) if c == "d" => {
                    self.host.toggle_bookmark();
                    *needs_rebuild = true;
                    return None;
                }
                // Ctrl+Shift+O: open the current page in another browser.
                Key::Character(c) if event.shift && c.eq_ignore_ascii_case("o") => {
                    self.open_external();
                    return None;
                }
                Key::Named(NamedKey::Tab) if count > 1 => {
                    let cur = self.host.active_index();
                    let next = if event.shift { (cur + count - 1) % count } else { (cur + 1) % count };
                    self.switch_tab(next);
                    *needs_rebuild = true;
                    return None;
                }
                _ => {}
            }
        }

        if self.url_focused {
            if event.state == ElementState::Pressed {
                self.edit_url(event);
                *needs_rebuild = true;
            }
            return None;
        }

        if event.state == ElementState::Pressed {
            if event.ctrl {
                if let Key::Character(c) = &event.logical_key {
                    match c.as_str() {
                        // Page clipboard: Servo needs the chord as an
                        // editing action, not as the raw keystroke.
                        "c" | "x" | "v" => {
                            self.host.editing_action_cmd(match c.as_str() {
                                "c" => EditingCommand::Copy,
                                "x" => EditingCommand::Cut,
                                _ => EditingCommand::Paste,
                            });
                            return None;
                        }
                        "l" => {
                            self.url_focused = true;
                            self.select_all_url();
                            *needs_rebuild = true;
                            return None;
                        }
                        "r" => {
                            self.host.reload();
                            return None;
                        }
                        _ => {}
                    }
                }
            }
            if event.logical_key == Key::Named(NamedKey::F5) {
                self.host.reload();
                return None;
            }
        }

        self.host.key_ui(event);
        None
    }

    fn display_list(&mut self, size: LogicalSize, _scale: f64) -> Option<DisplayList> {
        self.win = (size.width, size.height);
        let mut pc = PaintCtx::new();
        let w = size.width;

        let bar = self.bar();

        // Page: full-bleed under the floating bar.
        let content = Rect { x: 0.0, y: 0.0, width: w, height: size.height };
        pc.quad(content, PAGE_BG);
        if let Some((id, ..)) = self.host.image() {
            pc.image(id, content, 1.0);
        } else {
            // Just clear of the bar, whichever edge it is on.
            let y = match self.settings.bar_position {
                settings::BarPosition::Top => bar.y + bar.height + 22.0,
                settings::BarPosition::Bottom => BAR_MARGIN + 22.0,
            };
            pc.text("Loading...", BAR_MARGIN + 6.0, y, 13.0, TEXT_DIM);
        }

        // Floating utility bar: a blur-behind plate frosting the page under it.
        let radii = (BAR_RADIUS, BAR_RADIUS, BAR_RADIUS, BAR_RADIUS);
        pc.plate(bar, radii, BAR_FILL, cce_ui::layout::bevel_width().min(4.0));
        if self.loading {
            pc.clip_rounded(bar, BAR_RADIUS, |pc| {
                pc.quad(
                    Rect { x: bar.x, y: bar.y + bar.height - 2.0, width: bar.width, height: 2.0 },
                    ACCENT,
                );
            });
        }

        // Tab strip.
        let (sans, ..) = cce_ui::layout::read_preferred_fonts();
        let count = self.host.tab_count();
        let active = self.host.active_index();
        for i in 0..count {
            let pill = tab_rect(&bar, count, i);
            let is_active = i == active;
            pc.rounded_rect(
                pill,
                7.0,
                (true, true, true, true),
                if is_active { TAB_ACTIVE_BG } else { TAB_BG },
            );
            let tab = self.host.tab(i);
            let title = tab
                .and_then(|t| t.title.clone().filter(|s| !s.is_empty()))
                .or_else(|| tab.and_then(|t| t.url.clone()).map(|u| u.to_string()))
                .filter(|s| s != "about:blank")
                .unwrap_or_else(|| "New Tab".to_string());
            let close = tab_close_rect(&pill);
            let text_avail = pill.width - 16.0 - close.map_or(0.0, |_| TAB_CLOSE_W - 4.0);
            let label = Self::fit_text(&title, &sans, 12.0, text_avail);
            let color = if is_active { TEXT } else { TEXT_DIM };
            pc.text(
                label,
                pill.x + 8.0,
                cce_ui::layout::align_text_y(pill.y, pill.height, 12.0, 0.0),
                12.0,
                color,
            );
            if tab.is_some_and(|t| t.loading) {
                pc.quad(
                    Rect { x: pill.x, y: pill.y + pill.height - 2.0, width: pill.width, height: 2.0 },
                    ACCENT,
                );
            }
            if let Some(cr) = close {
                let xw = measure_text_width("x", &sans, 11.0);
                pc.text(
                    "x",
                    cr.x + (cr.width - xw) / 2.0 - 2.0,
                    cce_ui::layout::align_text_y(cr.y, cr.height, 11.0, 0.0),
                    11.0,
                    TEXT_DIM,
                );
            }
        }
        let plus = plus_rect(&bar);
        pc.rounded_rect(plus, 7.0, (true, true, true, true), BTN_BG);
        let pw = measure_text_width("+", &sans, 14.0);
        pc.text(
            "+",
            plus.x + (plus.width - pw) / 2.0,
            cce_ui::layout::align_text_y(plus.y, plus.height, 14.0, 0.0),
            14.0,
            TEXT,
        );

        let labels = ["<", ">", "R"];
        let enabled = [self.host.can_go_back(), self.host.can_go_forward(), true];
        for (i, label) in labels.iter().enumerate() {
            let r = btn_rect(&bar, i);
            pc.rounded_rect(r, 6.0, (true, true, true, true), BTN_BG);
            let color = if enabled[i] { TEXT } else { TEXT_DIM };
            let (sans, ..) = cce_ui::layout::read_preferred_fonts();
            let lw = measure_text_width(label, &sans, 14.0);
            pc.text(
                *label,
                r.x + (r.width - lw) / 2.0,
                cce_ui::layout::align_text_y(r.y, r.height, 14.0, 0.0),
                14.0,
                color,
            );
        }

        // Bookmark star: accent-lit when the page is bookmarked.
        let star = star_rect(&bar);
        pc.rounded_rect(star, 6.0, (true, true, true, true), BTN_BG);
        let starred = self.host.active_bookmarked();
        let star_color: [u8; 3] = if starred { [150, 190, 240] } else { TEXT_DIM };
        let sw = measure_text_width("*", &sans, 17.0);
        pc.text(
            "*",
            star.x + (star.width - sw) / 2.0,
            cce_ui::layout::align_text_y(star.y, star.height, 17.0, 0.0) + 3.0,
            17.0,
            star_color,
        );

        // URL field: rim + recess, brighter rim when focused.
        let f = url_rect(&bar);
        let rim = if self.url_focused { RIM_FOCUS } else { RIM };
        pc.rounded_rect(
            Rect { x: f.x - 1.0, y: f.y - 1.0, width: f.width + 2.0, height: f.height + 2.0 },
            7.0,
            (true, true, true, true),
            rim,
        );
        pc.rounded_rect(f, 6.0, (true, true, true, true), FIELD_BG);
        let ty = cce_ui::layout::align_text_y(f.y, f.height, URL_FONT, 0.0);
        let caret_x = if self.url_focused { Some(self.caret_offset()) } else { None };
        let sel_x = self
            .selection
            .filter(|&(a, b)| a < b)
            .map(|(a, b)| (self.x_offset(a), self.x_offset(b)));
        pc.clip(f, |pc| {
            if let Some((x0, x1)) = sel_x {
                pc.quad(
                    Rect {
                        x: f.x + URL_PAD_X + x0,
                        y: f.y + 4.0,
                        width: x1 - x0,
                        height: f.height - 8.0,
                    },
                    SEL_BG,
                );
            }
            pc.text(self.url_input.clone(), f.x + URL_PAD_X, ty, URL_FONT, TEXT);
            if let Some(offset) = caret_x {
                pc.quad(
                    Rect { x: f.x + URL_PAD_X + offset, y: f.y + 4.0, width: 1.0, height: f.height - 8.0 },
                    [0.85, 0.87, 0.92, 1.0],
                );
            }
        });

        #[cfg(feature = "wpe")]
        self.paint_modal(&mut pc, &sans);

        Some(pc.finish())
    }

    fn display_list_text(&self) -> bool {
        true
    }

    fn clear_color(&self) -> [f32; 4] {
        PAGE_BG
    }
}

fn main() {
    env_logger::init();
    cce_ui::engine::run::<BrowserApp>();
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH: &str = "https://duckduckgo.com/?q=";

    #[test]
    fn startup_arg_resolves_an_existing_path_to_a_file_url() {
        let dir = std::env::temp_dir().join("cce-browser-argv-test");
        std::fs::create_dir_all(&dir).unwrap();
        let page = dir.join("page.html");
        std::fs::write(&page, "<html></html>").unwrap();

        let u = parse_startup_arg(page.to_str().unwrap(), SEARCH).unwrap();
        assert_eq!(u.scheme(), "file");
        assert!(u.path().ends_with("page.html"), "got {u}");

        // The bar parser is what this guards against: a dotted, space-free
        // path takes its bare-host branch and becomes a bogus https URL.
        let bar = parse_url_input(page.to_str().unwrap(), SEARCH).unwrap();
        assert_eq!(bar.scheme(), "https");

        std::fs::remove_file(&page).unwrap();
    }

    #[test]
    fn startup_arg_still_takes_urls_and_searches() {
        let u = parse_startup_arg("https://example.com/x", SEARCH).unwrap();
        assert_eq!(u.as_str(), "https://example.com/x");

        // A bare host that is not a path still guesses https.
        assert_eq!(parse_startup_arg("example.com", SEARCH).unwrap().scheme(), "https");

        // A non-existent path is not a file: it falls through to the bar rules.
        let missing = parse_startup_arg("/nonexistent/nope.html", SEARCH).unwrap();
        assert_ne!(missing.scheme(), "file");
    }
}
