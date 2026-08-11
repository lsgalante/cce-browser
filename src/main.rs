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
const BAR_H: f32 = 38.0;
const BAR_RADIUS: f32 = 10.0;
const BAR_PAD: f32 = 7.0;
/// Utility-bar fill; the negative alpha marks the plate as blur-behind, so
/// it frosts the page content drawn beneath it (|alpha| = blur strength).
const BAR_FILL: [f32; 4] = [0.11, 0.12, 0.13, -0.58];
const BTN_W: f32 = 30.0;
const BTN_H: f32 = 26.0;
const BTN_GAP: f32 = 6.0;
const URL_FONT: f32 = 14.0;
const URL_PAD_X: f32 = 9.0;
/// Pixels per wheel notch when the DE reports discrete line deltas.
const LINE_PX: f64 = 76.0;

const HOME_URL: &str = "https://servo.org";

const PAGE_BG: [f32; 4] = [0.10, 0.10, 0.11, 1.0];
const FIELD_BG: [f32; 4] = [0.09, 0.09, 0.10, 0.85];
const BTN_BG: [f32; 4] = [0.20, 0.21, 0.23, 0.85];
const RIM: [f32; 4] = [0.22, 0.23, 0.25, 1.0];
const RIM_FOCUS: [f32; 4] = [0.33, 0.48, 0.72, 1.0];
const ACCENT: [f32; 4] = [0.35, 0.55, 0.85, 1.0];
const TEXT: [u8; 3] = [220, 220, 225];
const TEXT_DIM: [u8; 3] = [120, 122, 128];

#[derive(Debug, Clone)]
pub enum Message {
    /// Servo requested an event-loop spin (waker or delegate signal).
    Spin,
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

fn btn_rect(i: usize) -> Rect {
    Rect {
        x: BAR_MARGIN + BAR_PAD + i as f32 * (BTN_W + BTN_GAP),
        y: BAR_MARGIN + (BAR_H - BTN_H) / 2.0,
        width: BTN_W,
        height: BTN_H,
    }
}

fn url_rect(win_w: f32) -> Rect {
    let bar = bar_rect(win_w);
    let x = BAR_MARGIN + BAR_PAD + 3.0 * (BTN_W + BTN_GAP) + 4.0;
    Rect {
        x,
        y: BAR_MARGIN + (BAR_H - BTN_H) / 2.0,
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
                self.url_input = u.to_string();
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
        let url = Url::parse(HOME_URL).expect("home url");
        let host = ServoHost::new(sender, url, (1200, 800));
        Self {
            host,
            win: (1200.0, 800.0),
            scale: 1.0,
            pointer: (0.0, 0.0),
            url_input: HOME_URL.to_string(),
            url_focused: false,
            cursor: HOME_URL.len(),
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

    fn update(&mut self, msg: Self::Message, needs_rebuild: &mut bool, _exit: &mut bool) {
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
            if !pressed || button != MouseButton::Left {
                return None;
            }
            *needs_rebuild = true;
            if hit(&btn_rect(0), pos.x, pos.y) {
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
