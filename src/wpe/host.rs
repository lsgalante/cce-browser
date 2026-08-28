//! `WebKitHost` — the WPE-backed twin of `webview.rs`'s `ServoHost`.
//!
//! Deliberately mirrors that type's method surface so `main.rs` can switch
//! engines by changing a type name rather than its logic. Frames land in
//! cce-ui's image registry exactly as before, so `display_list` is unchanged:
//! the page is still one full-bleed quad.
//!
//! **Loop integration is the one real difference.** Servo had an
//! `EventLoopWaker` that pushed `Message::Spin` into calloop from its own
//! threads; WPE runs on a GLib `GMainContext`. [`WebKitHost::pump`] therefore
//! drains that context non-blockingly, which keeps the same shape as
//! `ServoHost::pump` but means *something has to call it*. Today that is the
//! app's `tick`. The correct fix is to put the context's pollfds into calloop
//! so the app wakes only when GLib has work — see WPE-PORT.md; doing it by
//! polling first keeps this milestone about the engine, not the event loop.

use std::ffi::{c_char, CString};
use std::rc::Rc;

use url::Url;

use cce_ui::widget::{KeyEvent, MouseButton};

use super::ffi::*;
use super::glib_source::GlibPoll;
use super::input;
use super::subclass::{types, FRAME_SINK};

/// One tab: its webview plus the app-visible page state and the last frame
/// uploaded to the image registry (id, w px, h px). Same shape as
/// `webview::Tab` so the chrome reads it identically.
pub struct Tab {
    webview: *mut WebKitWebView,
    view: *mut WPEView,
    pub title: Option<String>,
    pub url: Option<Url>,
    pub loading: bool,
    image: Option<(u32, u32, u32)>,
}

/// Frames handed over by `render_buffer`, drained by `pump`. A slot, not a
/// queue: only the newest frame is worth uploading, and WPE will not produce
/// another until we release the current one anyway.
#[derive(Default)]
struct Pending {
    frame: Option<(Vec<u8>, u32, u32)>,
}

pub struct WebKitHost {
    display: *mut WPEDisplay,
    toplevel: *mut WPEToplevel,
    tabs: Vec<Tab>,
    active: usize,
    size_px: (u32, u32),
    scale: f32,
    pending: Rc<std::cell::RefCell<Pending>>,
    /// GLib's pollfd set, mirrored into one epoll fd for calloop.
    poll: Option<GlibPoll>,
}

unsafe fn cstr(s: &str) -> CString {
    CString::new(s).expect("no interior nul")
}

impl WebKitHost {
    /// Boot WPE and open the first tab.
    ///
    /// One host per process: the frame sink and the GType registrations are
    /// process-wide. That matches the app (one browser window per process)
    /// but is worth knowing before writing a test that builds two.
    pub fn new(url: Url, size_px: (u32, u32)) -> Self {
        unsafe {
            let t = types();
            let display = g_object_new(t.display, std::ptr::null::<c_char>()) as *mut WPEDisplay;
            let mut err: *mut GError = std::ptr::null_mut();
            assert!(
                wpe_display_connect(display, &mut err) != 0,
                "wpe_display_connect failed"
            );

            let pending = Rc::new(std::cell::RefCell::new(Pending::default()));
            let sink = pending.clone();
            FRAME_SINK = Some(Box::new(move |buffer: *mut WPEBuffer| {
                if let Some(f) = read_shm(buffer) {
                    // Replace, never accumulate: the newest frame wins.
                    sink.borrow_mut().frame = Some(f);
                }
            }));

            let toplevel = wpe_display_create_toplevel(display, 1);
            wpe_toplevel_resized(toplevel, size_px.0 as i32, size_px.1 as i32);

            let mut host = Self {
                display,
                toplevel,
                tabs: Vec::new(),
                active: usize::MAX, // sentinel: force activate() to do the work
                size_px,
                scale: 1.0,
                pending,
                poll: GlibPoll::new()
                    .map_err(|e| log::warn!("no GLib epoll bridge ({e}); pump will poll"))
                    .ok(),
            };
            host.open_tab(url);
            host
        }
    }

    fn build_webview(&self, url: &Url) -> (*mut WebKitWebView, *mut WPEView) {
        unsafe {
            let prop = cstr("display");
            let wv = g_object_new(
                webkit_web_view_get_type(),
                prop.as_ptr(),
                self.display,
                std::ptr::null::<c_char>(),
            ) as *mut WebKitWebView;
            let view = webkit_web_view_get_wpe_view(wv);
            wpe_view_set_toplevel(view, self.toplevel);
            wpe_view_resized(view, self.size_px.0 as i32, self.size_px.1 as i32);
            wpe_view_set_visible(view, 1);
            wpe_view_map(view);
            let curl = cstr(url.as_str());
            webkit_web_view_load_uri(wv, curl.as_ptr());
            (wv, view)
        }
    }

    pub fn open_tab(&mut self, url: Url) {
        let (webview, view) = self.build_webview(&url);
        self.tabs.push(Tab {
            webview,
            view,
            title: None,
            url: Some(url),
            loading: true,
            image: None,
        });
        self.activate(self.tabs.len() - 1);
    }

    /// Make tab `index` visible and focused. Mirrors `ServoHost::activate`,
    /// including the `usize::MAX` sentinel so the first call is not a no-op.
    pub fn activate(&mut self, index: usize) {
        if index >= self.tabs.len() || index == self.active {
            return;
        }
        unsafe {
            if let Some(old) = self.tabs.get(self.active) {
                wpe_view_unmap(old.view);
                wpe_view_set_visible(old.view, 0);
            }
            self.active = index;
            let tab = &self.tabs[index];
            wpe_view_set_toplevel(tab.view, self.toplevel);
            wpe_view_set_visible(tab.view, 1);
            wpe_view_map(tab.view);
            wpe_view_resized(tab.view, self.size_px.0 as i32, self.size_px.1 as i32);
        }
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }
    pub fn active_index(&self) -> usize {
        self.active
    }
    pub fn tab(&self, index: usize) -> Option<&Tab> {
        self.tabs.get(index)
    }
    fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    /// The epoll fd carrying GLib's pollfd set, for `register_sources`.
    /// `None` if the bridge could not be created, in which case the app must
    /// fall back to calling [`Self::pump`] on a timer.
    pub fn poll_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.poll.as_ref().map(|p| p.fd())
    }

    /// How long calloop may sleep before pumping anyway, per GLib.
    pub fn poll_timeout(&self) -> Option<std::time::Duration> {
        self.poll
            .as_ref()
            .and_then(|p| p.timeout)
            .map(|ms| std::time::Duration::from_millis(ms as u64))
    }

    /// Drain GLib's pending work, then upload any frame it produced.
    /// Returns (new frame, any state change) like `ServoHost::pump`.
    pub fn pump(&mut self) -> (bool, bool) {
        // Clear the inner epoll first: calloop is level-triggered on that fd,
        // so leaving it readable across a pump that does not consume the
        // underlying socket would spin the loop.
        if let Some(p) = &self.poll {
            p.drain();
        }
        unsafe {
            while g_main_context_iteration(std::ptr::null_mut(), 0) != 0 {}
        }
        // WebKit opens and drops sockets as it loads, so the set that matters
        // is the one *after* dispatch, not before.
        if let Some(p) = &mut self.poll {
            p.sync();
        }
        let frame = self.pending.borrow_mut().frame.take();
        let dirty = self.sync_page_state();
        let Some((px, w, h)) = frame else {
            return (false, dirty);
        };
        let id = cce_ui::vk::upload_rgba(px, w, h);
        let tab = &mut self.tabs[self.active];
        if let Some((old, ..)) = tab.image.replace((id, w, h)) {
            cce_ui::vk::free_image(old);
        }
        (true, true)
    }

    /// Pull title/url/loading off the active webview. WebKit exposes these as
    /// properties; polling them here keeps the delegate-free shape of this
    /// first cut. Signals (`notify::title`, `load-changed`) are the better
    /// answer once tabs land, so background tabs update too.
    fn sync_page_state(&mut self) -> bool {
        unsafe {
            let tab = &mut self.tabs[self.active];
            let title = from_cstr(webkit_web_view_get_title(tab.webview));
            let uri = from_cstr(webkit_web_view_get_uri(tab.webview));
            let loading = webkit_web_view_is_loading(tab.webview) != 0;
            let url = uri.and_then(|u| Url::parse(&u).ok());
            let changed = title != tab.title || url != tab.url || loading != tab.loading;
            tab.title = title;
            if url.is_some() {
                tab.url = url;
            }
            tab.loading = loading;
            changed
        }
    }

    pub fn image(&self) -> Option<(u32, u32, u32)> {
        self.active_tab().image
    }
    pub fn title(&self) -> Option<String> {
        self.active_tab().title.clone()
    }
    pub fn url(&self) -> Option<Url> {
        self.active_tab().url.clone()
    }
    pub fn loading(&self) -> bool {
        self.active_tab().loading
    }

    pub fn load(&self, url: Url) {
        unsafe {
            let c = cstr(url.as_str());
            webkit_web_view_load_uri(self.active_tab().webview, c.as_ptr());
        }
    }
    pub fn reload(&self) {
        unsafe { webkit_web_view_reload(self.active_tab().webview) }
    }
    pub fn back(&self) {
        unsafe { webkit_web_view_go_back(self.active_tab().webview) }
    }
    pub fn forward(&self) {
        unsafe { webkit_web_view_go_forward(self.active_tab().webview) }
    }
    pub fn can_go_back(&self) -> bool {
        unsafe { webkit_web_view_can_go_back(self.active_tab().webview) != 0 }
    }
    pub fn can_go_forward(&self) -> bool {
        unsafe { webkit_web_view_can_go_forward(self.active_tab().webview) != 0 }
    }

    // ---- input ----
    //
    // Coordinates are device pixels relative to the view origin, matching
    // `ServoHost`'s convention so `main.rs` scales them the same way. Events
    // are refcounted; `wpe_view_event` takes its own reference, so each one is
    // unreffed here after delivery.

    pub fn mouse_move(&self, x_px: f32, y_px: f32) {
        unsafe {
            let view = self.active_tab().view;
            let e = wpe_event_pointer_move_new(
                WPEEventType::WPE_EVENT_POINTER_MOVE,
                view,
                WPEInputSource::WPE_INPUT_SOURCE_MOUSE,
                input::now_ms(),
                0,
                x_px as f64,
                y_px as f64,
                0.0,
                0.0,
            );
            self.send(view, e);
        }
    }

    pub fn mouse_button(&self, button: MouseButton, pressed: bool, x_px: f32, y_px: f32) {
        let Some(n) = input::button_number(button) else {
            return;
        };
        unsafe {
            let view = self.active_tab().view;
            let time = input::now_ms();
            // WPE tracks double/triple clicks for us; a frozen clock here
            // would make every click read as a repeat.
            let press_count = if pressed {
                wpe_view_compute_press_count(view, x_px as f64, y_px as f64, n, time)
            } else {
                0
            };
            let e = wpe_event_pointer_button_new(
                if pressed {
                    WPEEventType::WPE_EVENT_POINTER_DOWN
                } else {
                    WPEEventType::WPE_EVENT_POINTER_UP
                },
                view,
                WPEInputSource::WPE_INPUT_SOURCE_MOUSE,
                time,
                0,
                n,
                x_px as f64,
                y_px as f64,
                press_count,
            );
            self.send(view, e);
        }
    }

    /// Wheel deltas in device pixels, in cce-ui's winit convention (positive
    /// = scroll up), passed through **unchanged**.
    ///
    /// Measured, not assumed: WPE already inverts on the way to the DOM, so a
    /// negation here double-inverts and the page scrolls backwards. An
    /// earlier cut negated these and `examples/wpe_input` caught it — the
    /// page reported `deltaY` of the wrong sign.
    pub fn wheel(&self, dx_px: f64, dy_px: f64, x_px: f32, y_px: f32) {
        unsafe {
            let view = self.active_tab().view;
            let e = wpe_event_scroll_new(
                view,
                WPEInputSource::WPE_INPUT_SOURCE_MOUSE,
                input::now_ms(),
                0,
                dx_px,
                dy_px,
                1, // precise deltas: these are pixels, not notches
                0, // not a scroll-stop event
                x_px as f64,
                y_px as f64,
            );
            self.send(view, e);
        }
    }

    /// Takes cce-ui's `KeyEvent` directly — the keysym mapping lives in
    /// `input`, so the chrome never learns engine vocabulary.
    pub fn key(&self, event: &KeyEvent) {
        let Some(keyval) = input::keyval(&event.logical_key) else {
            return;
        };
        let pressed = input::is_pressed(event);
        unsafe {
            let view = self.active_tab().view;
            let e = wpe_event_keyboard_new(
                if pressed {
                    WPEEventType::WPE_EVENT_KEYBOARD_KEY_DOWN
                } else {
                    WPEEventType::WPE_EVENT_KEYBOARD_KEY_UP
                },
                view,
                WPEInputSource::WPE_INPUT_SOURCE_KEYBOARD,
                input::now_ms(),
                input::modifiers(event.ctrl, event.shift, event.alt),
                0, // hardware keycode: unknown to us, and WebKit works off keyval
                keyval,
            );
            self.send(view, e);
        }
    }

    /// Page focus. Without this the page has no focused frame and keyboard
    /// input is dropped, which looks exactly like a broken key mapping.
    pub fn focus(&self, focused: bool) {
        unsafe {
            let view = self.active_tab().view;
            if focused {
                wpe_view_focus_in(view)
            } else {
                wpe_view_focus_out(view)
            }
        }
    }

    unsafe fn send(&self, view: *mut WPEView, event: *mut WPEEvent) {
        if event.is_null() {
            return;
        }
        wpe_view_event(view, event);
        wpe_event_unref(event);
    }

    pub fn resize(&mut self, width_px: u32, height_px: u32, scale: f32) {
        self.size_px = (width_px.max(1), height_px.max(1));
        self.scale = scale;
        unsafe {
            wpe_toplevel_resized(self.toplevel, self.size_px.0 as i32, self.size_px.1 as i32);
            let view = self.active_tab().view;
            wpe_view_resized(view, self.size_px.0 as i32, self.size_px.1 as i32);
        }
    }
}

/// Copy an SHM buffer's pixels out as RGBA for `upload_rgba`.
///
/// `WPE_PIXEL_FORMAT_ARGB8888` is B,G,R,A in memory on little-endian, and the
/// stride is not assumed to equal `width * 4`.
unsafe fn read_shm(buffer: *mut WPEBuffer) -> Option<(Vec<u8>, u32, u32)> {
    if g_type_check_instance_is_a(buffer as *mut GTypeInstance, wpe_buffer_shm_get_type()) == 0 {
        return None;
    }
    let shm = buffer as *mut WPEBufferSHM;
    let (w, h) = (
        wpe_buffer_get_width(buffer) as u32,
        wpe_buffer_get_height(buffer) as u32,
    );
    let mut len: u64 = 0;
    let src = g_bytes_get_data(wpe_buffer_shm_get_data(shm), &mut len as *mut u64) as *const u8;
    if src.is_null() || w == 0 || h == 0 {
        return None;
    }
    let stride = wpe_buffer_shm_get_stride(shm) as usize;
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let s = src.add(y * stride + x * 4);
            let d = (y * w as usize + x) * 4;
            out[d] = *s.add(2);
            out[d + 1] = *s.add(1);
            out[d + 2] = *s;
            out[d + 3] = *s.add(3);
        }
    }
    Some((out, w, h))
}

unsafe fn from_cstr(p: *const c_char) -> Option<String> {
    (!p.is_null())
        .then(|| std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
}
