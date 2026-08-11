//! cce-browser — a web browser on the embedded Servo engine.
//!
//! Servo renders pages into a CPU (software) rendering context; each
//! finished frame is read back and uploaded to cce-ui's image registry,
//! then drawn as a single quad under a thin chrome bar (back / forward /
//! reload / URL field). Input over the page area is translated into Servo
//! input events; the URL bar is a small hand-rolled line editor.

mod webview;

use url::Url;
use wayland_client::QueueHandle;

use cce_ui::engine::{Application, EngineState, LogicalPosition, LogicalSize, WindowSettings};
use cce_ui::scene::layout::Rect;
use cce_ui::scene::paint::{DisplayList, PaintCtx};
use cce_ui::widget::display::measure_text_width;
use cce_ui::widget::{ElementState, Key, KeyEvent, MouseButton, MouseScrollDelta, NamedKey};

use webview::ServoHost;

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

const HOME_URL: &str = "https://servo.org";

const PAGE_BG: [f32; 4] = [0.10, 0.10, 0.11, 1.0];
const FIELD_BG: [f32; 4] = [0.09, 0.09, 0.10, 0.40];
const BTN_BG: [f32; 4] = [0.20, 0.21, 0.23, 0.40];
const TAB_BG: [f32; 4] = [0.15, 0.16, 0.18, 0.30];
const TAB_ACTIVE_BG: [f32; 4] = [0.32, 0.34, 0.38, 0.55];
const RIM: [f32; 4] = [0.22, 0.23, 0.25, 1.0];
const RIM_FOCUS: [f32; 4] = [0.33, 0.48, 0.72, 1.0];
const ACCENT: [f32; 4] = [0.35, 0.55, 0.85, 1.0];
const TEXT: [u8; 3] = [220, 220, 225];
const TEXT_DIM: [u8; 3] = [120, 122, 128];

#[derive(Debug, Clone)]
pub enum Message {
    /// Servo requested an event-loop spin (waker or delegate signal).
    Spin,
    /// Last tab closed: exit the app.
    Quit,
}

struct BrowserApp {
    host: ServoHost,
    win: (f32, f32),
    scale: f64,
    pointer: (f32, f32),
    /// URL bar contents; mirrors the page URL unless the bar is focused.
    url_input: String,
    url_focused: bool,
    /// Byte index of the URL-bar cursor.
    cursor: usize,
    loading: bool,
    /// Page title; drives the toplevel title (the engine re-applies
    /// `settings().title` whenever it changes).
    title: Option<String>,
}

fn hit(r: &Rect, x: f32, y: f32) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

/// The floating utility bar, overlaid on the page content.
fn bar_rect(win_w: f32) -> Rect {
    Rect {
        x: BAR_MARGIN,
        y: BAR_MARGIN,
        width: (win_w - 2.0 * BAR_MARGIN).max(120.0),
        height: BAR_H,
    }
}

/// Y of the tab-strip row.
fn tabs_y() -> f32 {
    BAR_MARGIN + BAR_PAD
}

/// Y of the nav-controls row.
fn controls_y() -> f32 {
    BAR_MARGIN + BAR_PAD + TAB_H + ROW_GAP
}

fn plus_rect(win_w: f32) -> Rect {
    let bar = bar_rect(win_w);
    Rect {
        x: bar.x + bar.width - BAR_PAD - PLUS_W,
        y: tabs_y(),
        width: PLUS_W,
        height: TAB_H,
    }
}

fn tab_rect(win_w: f32, count: usize, i: usize) -> Rect {
    let bar = bar_rect(win_w);
    let avail = bar.width - 2.0 * BAR_PAD - PLUS_W - TAB_GAP - (count.max(1) - 1) as f32 * TAB_GAP;
    let w = (avail / count.max(1) as f32).clamp(TAB_MIN_W, TAB_MAX_W);
    Rect {
        x: bar.x + BAR_PAD + i as f32 * (w + TAB_GAP),
        y: tabs_y(),
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

fn btn_rect(i: usize) -> Rect {
    Rect {
        x: BAR_MARGIN + BAR_PAD + i as f32 * (BTN_W + BTN_GAP),
        y: controls_y(),
        width: BTN_W,
        height: BTN_H,
    }
}

fn url_rect(win_w: f32) -> Rect {
    let bar = bar_rect(win_w);
    let x = BAR_MARGIN + BAR_PAD + 3.0 * (BTN_W + BTN_GAP) + 4.0;
    Rect {
        x,
        y: controls_y(),
        width: (bar.x + bar.width - BAR_PAD - x).max(60.0),
        height: BTN_H,
    }
}

/// Turn URL-bar input into something loadable: a real URL as-is, a bare
/// host gets https://, anything else becomes a search.
fn parse_url_input(input: &str) -> Option<Url> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(u) = Url::parse(s) {
        if matches!(u.scheme(), "http" | "https" | "file" | "data" | "about") {
            return Some(u);
        }
    }
    if !s.contains(' ') && s.contains('.') {
        if let Ok(u) = Url::parse(&format!("https://{s}")) {
            return Some(u);
        }
    }
    let q: String = url::form_urlencoded::byte_serialize(s.as_bytes()).collect();
    Url::parse(&format!("https://duckduckgo.com/html/?q={q}")).ok()
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
            }
        }
    }

    fn navigate(&mut self) {
        if let Some(url) = parse_url_input(&self.url_input) {
            self.host.load(url);
            self.url_focused = false;
            self.loading = true;
        }
    }

    /// New blank tab with the URL bar focused for typing.
    fn new_tab(&mut self) {
        let url = Url::parse("about:blank").expect("about:blank");
        self.host.open_tab(url);
        self.url_input.clear();
        self.cursor = 0;
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

    fn cursor_from_click(&self, click_x: f32, field: &Rect) -> usize {
        let rel = click_x - field.x - URL_PAD_X;
        let (sans, ..) = cce_ui::layout::read_preferred_fonts();
        let mut i = 0;
        while i < self.url_input.len() {
            let next = next_boundary(&self.url_input, i);
            if measure_text_width(&self.url_input[..next], &sans, URL_FONT) > rel {
                return i;
            }
            i = next;
        }
        self.url_input.len()
    }

    fn edit_url(&mut self, event: &KeyEvent) {
        match &event.logical_key {
            Key::Named(NamedKey::Enter) => self.navigate(),
            Key::Named(NamedKey::Escape) => {
                self.url_focused = false;
                self.sync_page_state();
            }
            Key::Named(NamedKey::Backspace) => {
                if self.cursor > 0 {
                    let prev = prev_boundary(&self.url_input, self.cursor);
                    self.url_input.replace_range(prev..self.cursor, "");
                    self.cursor = prev;
                }
            }
            Key::Named(NamedKey::Delete) => {
                if self.cursor < self.url_input.len() {
                    let next = next_boundary(&self.url_input, self.cursor);
                    self.url_input.replace_range(self.cursor..next, "");
                }
            }
            Key::Named(NamedKey::ArrowLeft) => self.cursor = prev_boundary(&self.url_input, self.cursor),
            Key::Named(NamedKey::ArrowRight) => self.cursor = next_boundary(&self.url_input, self.cursor),
            Key::Named(NamedKey::Home) => self.cursor = 0,
            Key::Named(NamedKey::End) => self.cursor = self.url_input.len(),
            Key::Character(c) if event.ctrl => {
                if c == "u" {
                    self.url_input.clear();
                    self.cursor = 0;
                }
            }
            _ => {
                let insert = match (&event.text, &event.logical_key) {
                    (Some(t), _) if !event.ctrl && !t.chars().any(char::is_control) => Some(t.clone()),
                    (None, Key::Named(NamedKey::Space)) => Some(" ".to_string()),
                    (None, Key::Character(c)) if !event.ctrl => Some(c.clone()),
                    _ => None,
                };
                if let Some(t) = insert {
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
        // Optional CLI arg: the start URL (same parsing as the URL bar).
        let url = std::env::args()
            .nth(1)
            .and_then(|arg| parse_url_input(&arg))
            .unwrap_or_else(|| Url::parse(HOME_URL).expect("home url"));
        let url_input = url.to_string();
        let cursor = url_input.len();
        let host = ServoHost::new(sender, url, (1200, 800));
        Self {
            host,
            win: (1200.0, 800.0),
            scale: 1.0,
            pointer: (0.0, 0.0),
            url_input,
            url_focused: false,
            cursor,
            loading: true,
            title: None,
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

    fn handle_resize(&mut self, width: f32, height: f32, scale: f64) {
        self.win = (width, height);
        self.scale = scale;
        let (w, h) = self.content_px();
        self.host.resize(w, h, scale as f32);
    }

    fn handle_pointer_move(&mut self, pos: LogicalPosition, _needs_rebuild: &mut bool) {
        self.pointer = (pos.x, pos.y);
        if !hit(&bar_rect(self.win.0), pos.x, pos.y) {
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

        if hit(&bar_rect(self.win.0), pos.x, pos.y) {
            if !pressed || !matches!(button, MouseButton::Left | MouseButton::Middle) {
                return None;
            }
            *needs_rebuild = true;
            // Tab strip: activate / close (x region or middle click) / new tab.
            let count = self.host.tab_count();
            for i in 0..count {
                let pill = tab_rect(self.win.0, count, i);
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
            if hit(&plus_rect(self.win.0), pos.x, pos.y) {
                self.new_tab();
            } else if hit(&btn_rect(0), pos.x, pos.y) {
                self.host.back();
            } else if hit(&btn_rect(1), pos.x, pos.y) {
                self.host.forward();
            } else if hit(&btn_rect(2), pos.x, pos.y) {
                self.host.reload();
            } else {
                let field = url_rect(self.win.0);
                if hit(&field, pos.x, pos.y) {
                    self.cursor = self.cursor_from_click(pos.x, &field);
                    self.url_focused = true;
                } else {
                    self.url_focused = false;
                }
            }
            return None;
        }

        // Page area: a click dismisses URL-bar focus, then goes to the page.
        if self.url_focused && pressed {
            self.url_focused = false;
            self.sync_page_state();
            *needs_rebuild = true;
        }
        match button {
            MouseButton::Back if pressed => self.host.back(),
            MouseButton::Forward if pressed => self.host.forward(),
            _ => {
                if let Some(b) = dom_button(button) {
                    let s = self.scale as f32;
                    self.host.mouse_button(b, pressed, pos.x * s, pos.y * s);
                }
            }
        }
        None
    }

    fn handle_mouse_wheel(&mut self, delta: &MouseScrollDelta, pos: LogicalPosition, _needs_rebuild: &mut bool) {
        if hit(&bar_rect(self.win.0), pos.x, pos.y) {
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
                        "l" => {
                            self.url_focused = true;
                            self.cursor = self.url_input.len();
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

        if let Some(k) = dom_key(&event.logical_key) {
            self.host.key(k, event.state == ElementState::Pressed);
        }
        None
    }

    fn display_list(&mut self, size: LogicalSize, _scale: f64) -> Option<DisplayList> {
        self.win = (size.width, size.height);
        let mut pc = PaintCtx::new();
        let w = size.width;

        // Page: full-bleed under the floating bar.
        let content = Rect { x: 0.0, y: 0.0, width: w, height: size.height };
        pc.quad(content, PAGE_BG);
        if let Some((id, ..)) = self.host.image() {
            pc.image(id, content, 1.0);
        } else {
            pc.text("Loading...", BAR_MARGIN + 6.0, BAR_MARGIN + BAR_H + 22.0, 13.0, TEXT_DIM);
        }

        // Floating utility bar: a blur-behind plate frosting the page under it.
        let bar = bar_rect(w);
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
            let pill = tab_rect(w, count, i);
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
        let plus = plus_rect(w);
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
            let r = btn_rect(i);
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

        // URL field: rim + recess, brighter rim when focused.
        let f = url_rect(w);
        let rim = if self.url_focused { RIM_FOCUS } else { RIM };
        pc.rounded_rect(
            Rect { x: f.x - 1.0, y: f.y - 1.0, width: f.width + 2.0, height: f.height + 2.0 },
            7.0,
            (true, true, true, true),
            rim,
        );
        pc.rounded_rect(f, 6.0, (true, true, true, true), FIELD_BG);
        let ty = cce_ui::layout::align_text_y(f.y, f.height, URL_FONT, 0.0);
        pc.clip(f, |pc| {
            pc.text(self.url_input.clone(), f.x + URL_PAD_X, ty, URL_FONT, TEXT);
            if self.url_focused {
                let (sans, ..) = cce_ui::layout::read_preferred_fonts();
                let cx = f.x + URL_PAD_X + measure_text_width(&self.url_input[..self.cursor], &sans, URL_FONT);
                pc.quad(
                    Rect { x: cx, y: f.y + 4.0, width: 1.0, height: f.height - 8.0 },
                    [0.85, 0.87, 0.92, 1.0],
                );
            }
        });

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
