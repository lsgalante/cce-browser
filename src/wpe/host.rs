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

use std::cell::{Cell, RefCell};
use std::ffi::{c_char, CString};
use std::rc::Rc;

use url::Url;

use cce_ui::widget::{KeyEvent, MouseButton};

use super::ffi::*;
use super::glib_source::GlibPoll;
use super::input;
use super::subclass::{types, FRAME_SINK};

/// Page state a tab's WebKit signals write into.
///
/// Held behind an `Rc` because each connected signal owns a reference: the
/// closure outlives any borrow we could hand it, and the webview may emit
/// after the `Tab` has moved within `tabs` (a `Vec` reallocates).
#[derive(Default)]
struct TabState {
    title: RefCell<Option<String>>,
    url: RefCell<Option<Url>>,
    loading: Cell<bool>,
    /// Set by any signal, cleared by `pump`. This is what lets a *background*
    /// tab report a title change — the old polling only ever looked at the
    /// active webview.
    dirty: Cell<bool>,
}

/// One tab: its webview plus the app-visible page state and the last frame
/// uploaded to the image registry (id, w px, h px). Same shape as
/// `webview::Tab` so the chrome reads it identically.
pub struct Tab {
    webview: *mut WebKitWebView,
    view: *mut WPEView,
    state: Rc<TabState>,
    pub title: Option<String>,
    pub url: Option<Url>,
    pub loading: bool,
    image: Option<(u32, u32, u32)>,
}

impl Drop for Tab {
    fn drop(&mut self) {
        // Unref the webview *first*: destroying it runs the closures'
        // destroy-notify, which releases their `Rc<TabState>` refs. Dropping
        // the state before the object that can still emit into it would be a
        // use-after-free.
        unsafe { g_object_unref(self.webview as *mut _) };
        if let Some((id, ..)) = self.image {
            cce_ui::vk::free_image(id);
        }
    }
}

/// `notify::` handler shared by title / uri / is-loading: read the property
/// straight back off the emitting webview and stash it.
unsafe extern "C" fn on_notify(
    obj: *mut GObject,
    _pspec: *mut GParamSpec,
    data: gpointer,
) {
    let st = &*(data as *const TabState);
    let wv = obj as *mut WebKitWebView;
    *st.title.borrow_mut() = from_cstr(webkit_web_view_get_title(wv));
    if let Some(u) = from_cstr(webkit_web_view_get_uri(wv)).and_then(|u| Url::parse(&u).ok()) {
        *st.url.borrow_mut() = Some(u);
    }
    st.loading.set(webkit_web_view_is_loading(wv) != 0);
    st.dirty.set(true);
}

/// Releases the `Rc` ref a connection owned, when the closure is destroyed.
unsafe extern "C" fn drop_state_ref(data: gpointer, _closure: *mut GClosure) {
    drop(Rc::from_raw(data as *const TabState));
}

unsafe fn connect_notify(wv: *mut WebKitWebView, signal: &str, state: &Rc<TabState>) {
    let name = cstr(signal);
    // Each connection owns its own ref, handed back by `drop_state_ref`.
    let raw = Rc::into_raw(state.clone()) as gpointer;
    g_signal_connect_data(
        wv as *mut _,
        name.as_ptr(),
        Some(std::mem::transmute::<_, unsafe extern "C" fn()>(
            on_notify as unsafe extern "C" fn(*mut GObject, *mut GParamSpec, gpointer),
        )),
        raw,
        Some(drop_state_ref),
        0,
    );
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
    /// Shared with the `cce:` pages, exactly as `ServoHost` holds them —
    /// bookmarks and history are app state, not engine state, so they cross
    /// the backend swap unchanged.
    history: std::sync::Arc<crate::pages::History>,
    bookmarks: std::sync::Arc<crate::pages::Bookmarks>,
    history_enabled: bool,
    force_dark: bool,
    /// Retained only so tests can assert on rendered output; the registry
    /// owns the copy that actually gets drawn.
    last_frame: Option<(Vec<u8>, u32, u32)>,
    /// Installed on every webview when force-dark is on.
    ucm: *mut WebKitUserContentManager,
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
                history: std::sync::Arc::new(crate::pages::History::load()),
                bookmarks: std::sync::Arc::new(crate::pages::Bookmarks::load()),
                history_enabled: true,
                force_dark: false,
                last_frame: None,
                ucm: webkit_user_content_manager_new(),
            };
            host.open_tab(url);
            host
        }
    }

    fn build_webview(&self, url: &Url, state: &Rc<TabState>) -> (*mut WebKitWebView, *mut WPEView) {
        unsafe {
            let (p_display, p_ucm) = (cstr("display"), cstr("user-content-manager"));
            let wv = g_object_new(
                webkit_web_view_get_type(),
                p_display.as_ptr(),
                self.display,
                p_ucm.as_ptr(),
                self.ucm,
                std::ptr::null::<c_char>(),
            ) as *mut WebKitWebView;
            let view = webkit_web_view_get_wpe_view(wv);
            wpe_view_set_toplevel(view, self.toplevel);
            // Signals, not polling: a background tab has to be able to report
            // its title without anyone asking the active webview.
            for sig in ["notify::title", "notify::uri", "notify::is-loading"] {
                connect_notify(wv, sig, state);
            }
            wpe_view_resized(view, self.size_px.0 as i32, self.size_px.1 as i32);
            wpe_view_set_visible(view, 1);
            wpe_view_map(view);
            let curl = cstr(url.as_str());
            webkit_web_view_load_uri(wv, curl.as_ptr());
            (wv, view)
        }
    }

    pub fn open_tab(&mut self, url: Url) {
        let state = Rc::new(TabState::default());
        state.loading.set(true);
        *state.url.borrow_mut() = Some(url.clone());
        let (webview, view) = self.build_webview(&url, &state);
        self.tabs.push(Tab {
            webview,
            view,
            state,
            title: None,
            url: Some(url),
            loading: true,
            image: None,
        });
        self.activate(self.tabs.len() - 1);
    }

    /// Close a tab. Returns false when that was the last one (the app should
    /// exit; the tab is gone either way). Mirrors `ServoHost::close_tab`,
    /// including how the next active index is chosen.
    pub fn close_tab(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() {
            return true;
        }
        let was_active = index == self.active;
        let old_active = self.active;
        // Dropping the Tab unrefs the webview and frees its registry image.
        drop(self.tabs.remove(index));
        if self.tabs.is_empty() {
            return false;
        }
        let next = if was_active {
            index.min(self.tabs.len() - 1)
        } else if old_active > index {
            old_active - 1
        } else {
            old_active
        };
        self.active = usize::MAX; // force activate() to do the work
        self.activate(next);
        true
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
        self.last_frame = Some((px.clone(), w, h));
        let id = cce_ui::vk::upload_rgba(px, w, h);
        let tab = &mut self.tabs[self.active];
        if let Some((old, ..)) = tab.image.replace((id, w, h)) {
            cce_ui::vk::free_image(old);
        }
        (true, true)
    }

    /// Fold each tab's signal-written state into the fields the chrome reads.
    ///
    /// Every tab, not just the active one — that is the whole point of moving
    /// off polling. The tab strip shows a title per tab, so a background tab
    /// finishing a load has to be visible without switching to it.
    fn sync_page_state(&mut self) -> bool {
        let mut changed = false;
        for tab in &mut self.tabs {
            if !tab.state.dirty.replace(false) {
                continue;
            }
            tab.title = tab.state.title.borrow().clone();
            if let Some(u) = tab.state.url.borrow().clone() {
                tab.url = Some(u);
            }
            tab.loading = tab.state.loading.get();
            changed = true;
        }
        changed
    }

    /// Top-left pixel of the last frame, for tests that need to assert on
    /// what was actually rendered rather than on what was configured.
    pub fn sample_pixel(&self) -> Option<(u8, u8, u8)> {
        let (px, ..) = self.last_frame.as_ref()?;
        Some((px[0], px[1], px[2]))
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

    // ---- settings and app-side state ----
    //
    // These exist so `WebKitHost` and `ServoHost` present the same surface;
    // bookmarks and history are app state either way, so they are identical.

    pub fn set_history_enabled(&mut self, on: bool) {
        self.history_enabled = on;
    }

    /// Install or remove the inverting user stylesheet.
    ///
    /// Simpler than the Servo path, which needed *two* timed reloads to let
    /// a user-content change and a scheme flip settle. WebKit applies user
    /// content to live pages, so a reload is enough — and only to re-run
    /// pages that already computed their colours.
    pub fn set_force_dark(&mut self, on: bool) {
        if on == self.force_dark {
            return;
        }
        self.force_dark = on;
        unsafe {
            if on {
                let css = cstr(FORCE_DARK_CSS);
                let sheet = webkit_user_style_sheet_new(
                    css.as_ptr(),
                    WebKitUserContentInjectedFrames::WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES,
                    WebKitUserStyleLevel::WEBKIT_USER_STYLE_LEVEL_USER,
                    std::ptr::null(),
                    std::ptr::null(),
                );
                webkit_user_content_manager_add_style_sheet(self.ucm, sheet);
                webkit_user_style_sheet_unref(sheet);
            } else {
                webkit_user_content_manager_remove_all_style_sheets(self.ucm);
            }
            for tab in &self.tabs {
                webkit_web_view_reload(tab.webview);
            }
        }
    }

    /// What pages see for `prefers-color-scheme`, via WPE's own setting.
    pub fn set_color_scheme(&self, dark: bool) {
        unsafe {
            let settings = wpe_display_get_settings(self.display);
            let key = cstr("/wpe-platform/dark-mode");
            let mut err: *mut GError = std::ptr::null_mut();
            wpe_settings_set_boolean(
                settings,
                key.as_ptr(),
                dark as gboolean,
                WPESettingsSource::WPE_SETTINGS_SOURCE_APPLICATION,
                &mut err,
            );
        }
    }

    /// A navigation became a download since the last check. Always false
    /// until downloads are ported to WebKit's own API.
    pub fn take_download_started(&self) -> bool {
        false
    }

    pub fn active_bookmarked(&self) -> bool {
        self.active_tab()
            .url
            .as_ref()
            .is_some_and(|u| self.bookmarks.contains(u.as_str()))
    }

    pub fn toggle_bookmark(&self) {
        let tab = self.active_tab();
        if let Some(url) = &tab.url {
            self.bookmarks
                .toggle(url.as_str(), tab.title.as_deref().unwrap_or(""));
        }
    }

    /// Clipboard on the page. WebKit takes these as named editing commands,
    /// so unlike the Servo backend there is no separate clipboard delegate to
    /// implement — it goes through the platform clipboard itself.
    pub fn editing_action(&self, command: EditingCommand) {
        unsafe {
            let c = cstr(command.as_str());
            webkit_web_view_execute_editing_command(self.active_tab().webview, c.as_ptr());
        }
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

/// Backend-neutral clipboard action, so `main.rs` names neither engine's.
#[derive(Debug, Clone, Copy)]
pub enum EditingCommand {
    Copy,
    Cut,
    Paste,
}

impl EditingCommand {
    fn as_str(self) -> &'static str {
        match self {
            Self::Copy => "Copy",
            Self::Cut => "Cut",
            Self::Paste => "Paste",
        }
    }
}

/// Same inverting stylesheet the Servo backend uses, and for the same reason:
/// it is the only thing that darkens a page shipping a hardcoded white with no
/// `prefers-color-scheme` rule to honour.
const FORCE_DARK_CSS: &str = "\
html { background-color: #ffffff !important; filter: invert(1) hue-rotate(180deg) !important; }
img, video, picture, canvas, svg, iframe, embed, object,
[style*=\"background-image\"], [style*=\"background:url\"] {
  filter: invert(1) hue-rotate(180deg) !important;
}
";
