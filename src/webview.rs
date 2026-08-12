//! Servo embedding host: boots an in-process Servo against a software
//! (CPU) rendering context and owns one WebView per tab, all sharing that
//! context — only the active tab is painted and read back (servoshell's
//! model). Finished frames upload into cce-ui's image registry; each tab
//! keeps its last frame so switching is instant.
//!
//! Everything here lives on the main thread. Servo wakes the calloop loop
//! through `Waker` (a channel sender); the app then calls [`ServoHost::pump`],
//! which spins Servo's event loop and, when the delegate has flagged a ready
//! frame on the active tab, paints and reads back pixels. `read_to_image`
//! happens *without* `present()` so the buffer is still there to read.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use dpi::PhysicalSize;
use euclid::Scale;
use servo::{
    CreateNewWebViewRequest, DeviceIntRect, DevicePoint, EventLoopWaker, InputEvent,
    Key as DomKey, KeyState, KeyboardEvent, LoadStatus, MouseButton as DomMouseButton,
    MouseButtonAction, MouseButtonEvent, MouseMoveEvent, NavigationRequest, RenderingContext,
    Servo, ServoBuilder, SoftwareRenderingContext, WebView, WebViewBuilder, WebViewDelegate,
    WebViewId, WheelDelta, WheelEvent, WheelMode,
};
use servo::protocol_handler::ProtocolRegistry;
use url::Url;

use crate::downloads::{is_download_url, Downloads};
use crate::pages::{Bookmarks, CceProtocol, History};
use crate::Message;

/// Delegate-observed signals for one webview, polled by the app after each
/// pump.
#[derive(Default)]
struct TabSignals {
    frame_ready: bool,
    title: Option<String>,
    url: Option<Url>,
    /// None until Servo reports a load status — the sync must not mistake
    /// the default for "finished loading" (that swallows the completion
    /// transition history recording depends on).
    loading: Option<bool>,
}

#[derive(Default)]
struct HostShared {
    dirty: Cell<bool>,
    per: RefCell<HashMap<WebViewId, TabSignals>>,
    /// WebViews created by pages (window.open / target=_blank), built in the
    /// delegate and adopted as tabs by the next `pump`.
    pending_new: RefCell<Vec<WebView>>,
    /// A navigation was diverted into a download; the app surfaces the
    /// downloads page.
    download_started: Cell<bool>,
}

struct Delegate {
    shared: Rc<HostShared>,
    wake: calloop::channel::Sender<Message>,
    context: Rc<SoftwareRenderingContext>,
    downloads: std::sync::Arc<Downloads>,
    /// Handle to this same Rc'd delegate, so page-opened webviews can be
    /// delegated back here; filled right after construction.
    self_rc: RefCell<std::rc::Weak<Delegate>>,
}

impl Delegate {
    fn with_tab(&self, webview: &WebView, f: impl FnOnce(&mut TabSignals)) {
        f(self.shared.per.borrow_mut().entry(webview.id()).or_default());
        self.shared.dirty.set(true);
        let _ = self.wake.send(Message::Spin);
    }
}

impl WebViewDelegate for Delegate {
    fn notify_new_frame_ready(&self, webview: WebView) {
        self.with_tab(&webview, |t| t.frame_ready = true);
    }

    fn notify_page_title_changed(&self, webview: WebView, title: Option<String>) {
        self.with_tab(&webview, |t| t.title = title);
    }

    fn notify_url_changed(&self, webview: WebView, url: Url) {
        self.with_tab(&webview, |t| t.url = Some(url));
    }

    fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus) {
        self.with_tab(&webview, |t| t.loading = Some(status != LoadStatus::Complete));
    }

    fn request_navigation(&self, _webview: WebView, request: NavigationRequest) {
        // Navigations to downloadable files become chrome downloads —
        // Servo has no download path of its own.
        if is_download_url(&request.url) {
            let url = request.url.clone();
            request.deny();
            self.downloads.start(url);
            self.shared.download_started.set(true);
            self.shared.dirty.set(true);
            let _ = self.wake.send(Message::Spin);
        } else {
            request.allow();
        }
    }

    fn request_create_new(&self, _parent_webview: WebView, request: CreateNewWebViewRequest) {
        let Some(delegate) = self.self_rc.borrow().upgrade() else {
            return; // dropping the request denies it
        };
        let webview = request
            .builder(self.context.clone())
            .delegate(delegate)
            .build();
        self.shared.pending_new.borrow_mut().push(webview);
        self.shared.dirty.set(true);
        let _ = self.wake.send(Message::Spin);
    }
}

/// Wakes the calloop event loop from Servo's internal threads.
#[derive(Clone)]
struct Waker(calloop::channel::Sender<Message>);

impl EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }

    fn wake(&self) {
        let _ = self.0.send(Message::Spin);
    }
}

/// One tab: its webview plus the app-visible page state and the last frame
/// uploaded to the image registry (id, w px, h px).
pub struct Tab {
    webview: WebView,
    pub title: Option<String>,
    pub url: Option<Url>,
    pub loading: bool,
    image: Option<(u32, u32, u32)>,
}

pub struct ServoHost {
    servo: Servo,
    context: Rc<SoftwareRenderingContext>,
    shared: Rc<HostShared>,
    delegate: Rc<Delegate>,
    history: std::sync::Arc<History>,
    bookmarks: std::sync::Arc<Bookmarks>,
    tabs: Vec<Tab>,
    active: usize,
    size_px: (u32, u32),
    scale: f32,
    /// Settings gate for cce://history recording.
    history_enabled: bool,
}

impl ServoHost {
    pub fn set_history_enabled(&mut self, on: bool) {
        self.history_enabled = on;
    }
}

impl ServoHost {
    pub fn new(wake: calloop::channel::Sender<Message>, url: Url, size_px: (u32, u32)) -> Self {
        // Servo's TLS stack looks up the process-wide rustls crypto provider.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let context = Rc::new(
            SoftwareRenderingContext::new(PhysicalSize::new(size_px.0.max(1), size_px.1.max(1)))
                .expect("create software rendering context"),
        );
        context
            .make_current()
            .expect("make software rendering context current");

        let history = std::sync::Arc::new(History::load());
        let bookmarks = std::sync::Arc::new(Bookmarks::load());
        let downloads = std::sync::Arc::new(Downloads::default());
        let mut protocols = ProtocolRegistry::default();
        let handler = CceProtocol {
            history: history.clone(),
            bookmarks: bookmarks.clone(),
            downloads: downloads.clone(),
        };
        if let Err(e) = protocols.register("cce", handler) {
            log::error!("failed to register cce: protocol: {e:?}");
        }

        let servo = ServoBuilder::default()
            .event_loop_waker(Box::new(Waker(wake.clone())))
            .protocol_registry(protocols)
            .build();

        let shared = Rc::new(HostShared::default());
        let delegate = Rc::new(Delegate {
            shared: shared.clone(),
            wake,
            context: context.clone(),
            downloads: downloads.clone(),
            self_rc: RefCell::new(std::rc::Weak::new()),
        });
        *delegate.self_rc.borrow_mut() = Rc::downgrade(&delegate);

        let mut host = Self {
            servo,
            context,
            shared,
            delegate,
            history,
            bookmarks,
            tabs: Vec::new(),
            // Sentinel so the first open_tab's activate() does the full
            // show/focus/resize dance instead of early-returning on 0 == 0.
            active: usize::MAX,
            size_px,
            scale: 1.0,
            history_enabled: true,
        };
        host.open_tab(url);
        host
    }

    fn build_webview(&self, url: Url) -> WebView {
        WebViewBuilder::new(&self.servo, self.context.clone())
            .url(url)
            .delegate(self.delegate.clone())
            .build()
    }

    /// Open a new tab and make it active.
    pub fn open_tab(&mut self, url: Url) {
        let webview = self.build_webview(url);
        self.tabs.push(Tab {
            webview,
            title: None,
            url: None,
            loading: true,
            image: None,
        });
        self.activate(self.tabs.len() - 1);
    }

    /// Close a tab. Returns false when that was the last tab (the app should
    /// exit; the tab is gone either way).
    pub fn close_tab(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() {
            return true;
        }
        let was_active = index == self.active;
        let old_active = self.active;
        let tab = self.tabs.remove(index);
        self.shared.per.borrow_mut().remove(&tab.webview.id());
        if let Some((id, ..)) = tab.image {
            cce_ui::vk::free_image(id);
        }
        drop(tab); // last WebView handle: servo tears the page down
        if self.tabs.is_empty() {
            return false;
        }
        // Closing the active tab moves to its neighbor; closing a background
        // tab keeps the current one (its index may have shifted down).
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

    /// Make tab `index` the visible, focused one.
    pub fn activate(&mut self, index: usize) {
        if index >= self.tabs.len() || index == self.active {
            return;
        }
        if let Some(old) = self.tabs.get(self.active) {
            old.webview.blur();
            old.webview.hide();
        }
        self.active = index;
        let tab = &self.tabs[index];
        tab.webview.show();
        tab.webview.focus();
        tab.webview.set_hidpi_scale_factor(Scale::new(self.scale));
        tab.webview
            .resize(PhysicalSize::new(self.size_px.0.max(1), self.size_px.1.max(1)));
        // Composite whatever frame the tab already has so the switch shows
        // content immediately; the resize above refreshes it right after.
        self.paint_active();
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

    /// Paint the active webview into the shared context and swap the read
    /// pixels into its registry image.
    fn paint_active(&mut self) {
        self.active_tab().webview.paint();
        let rect = DeviceIntRect::from_size(self.context.size2d().to_i32());
        if let Some(img) = self.context.read_to_image(rect) {
            let (w, h) = img.dimensions();
            let id = cce_ui::vk::upload_rgba(img.into_raw(), w, h);
            let tab = &mut self.tabs[self.active];
            if let Some((old, ..)) = tab.image.replace((id, w, h)) {
                cce_ui::vk::free_image(old);
            }
        }
    }

    /// Spin Servo, sync delegate signals into tabs, and repaint the active
    /// tab if it produced a frame. Returns (new frame, any state change).
    pub fn pump(&mut self) -> (bool, bool) {
        self.servo.spin_event_loop();
        // Adopt page-opened webviews as tabs; like a browser popup, the
        // newest one takes focus.
        let opened: Vec<WebView> = self.shared.pending_new.borrow_mut().drain(..).collect();
        for webview in opened {
            self.tabs.push(Tab {
                webview,
                title: None,
                url: None,
                loading: true,
                image: None,
            });
            self.activate(self.tabs.len() - 1);
        }
        let dirty = self.shared.dirty.take();
        let mut active_frame = false;
        if dirty {
            let mut per = self.shared.per.borrow_mut();
            for (i, tab) in self.tabs.iter_mut().enumerate() {
                if let Some(sig) = per.get_mut(&tab.webview.id()) {
                    tab.title = sig.title.clone();
                    tab.url = sig.url.clone();
                    if let Some(loading) = sig.loading.take() {
                        let was_loading = tab.loading;
                        tab.loading = loading;
                        // Load-complete transition: log the visit.
                        if was_loading && !loading && self.history_enabled {
                            if let Some(url) = &tab.url {
                                self.history
                                    .record(url.as_str(), tab.title.as_deref().unwrap_or(""));
                            }
                        }
                    }
                    if std::mem::take(&mut sig.frame_ready) && i == self.active {
                        active_frame = true;
                    }
                }
            }
        }
        if active_frame {
            self.paint_active();
        }
        (active_frame, dirty)
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

    /// A navigation became a download since the last check.
    pub fn take_download_started(&self) -> bool {
        self.shared.download_started.take()
    }

    /// Whether the active tab's page is bookmarked.
    pub fn active_bookmarked(&self) -> bool {
        self.active_tab()
            .url
            .as_ref()
            .is_some_and(|u| self.bookmarks.contains(u.as_str()))
    }

    /// Toggle the bookmark for the active tab's page.
    pub fn toggle_bookmark(&self) {
        let tab = self.active_tab();
        if let Some(url) = &tab.url {
            self.bookmarks
                .toggle(url.as_str(), tab.title.as_deref().unwrap_or(""));
        }
    }

    pub fn can_go_back(&self) -> bool {
        self.active_tab().webview.can_go_back()
    }

    pub fn can_go_forward(&self) -> bool {
        self.active_tab().webview.can_go_forward()
    }

    pub fn load(&self, url: Url) {
        self.active_tab().webview.load(url);
    }

    pub fn reload(&self) {
        self.active_tab().webview.reload();
    }

    pub fn back(&self) {
        let webview = &self.active_tab().webview;
        if webview.can_go_back() {
            let _ = webview.go_back(1);
        }
    }

    pub fn forward(&self) {
        let webview = &self.active_tab().webview;
        if webview.can_go_forward() {
            let _ = webview.go_forward(1);
        }
    }

    /// Resize the active webview (and the shared rendering context) to a
    /// physical size. Inactive tabs are brought up to size on activation.
    pub fn resize(&mut self, width_px: u32, height_px: u32, scale: f32) {
        self.size_px = (width_px, height_px);
        self.scale = scale;
        let webview = &self.active_tab().webview;
        webview.set_hidpi_scale_factor(Scale::new(scale));
        webview.resize(PhysicalSize::new(width_px.max(1), height_px.max(1)));
    }

    /// Pointer position in device pixels relative to the webview origin.
    pub fn mouse_move(&self, x_px: f32, y_px: f32) {
        let _ = self.active_tab().webview.notify_input_event(InputEvent::MouseMove(
            MouseMoveEvent::new(DevicePoint::new(x_px, y_px).into()),
        ));
    }

    pub fn mouse_button(&self, button: DomMouseButton, pressed: bool, x_px: f32, y_px: f32) {
        let action = if pressed { MouseButtonAction::Down } else { MouseButtonAction::Up };
        let _ = self.active_tab().webview.notify_input_event(InputEvent::MouseButton(
            MouseButtonEvent::new(action, button, DevicePoint::new(x_px, y_px).into()),
        ));
    }

    /// Wheel in device pixels, winit sign convention (positive y = scroll
    /// up). Servo hit-tests the wheel event, lets the page preventDefault,
    /// and applies the inverted delta as the scroll itself — no separate
    /// scroll event wanted.
    pub fn wheel(&self, dx_px: f64, dy_px: f64, x_px: f32, y_px: f32) {
        let _ = self.active_tab().webview.notify_input_event(InputEvent::Wheel(WheelEvent::new(
            WheelDelta { x: dx_px, y: dy_px, z: 0.0, mode: WheelMode::DeltaPixel },
            DevicePoint::new(x_px, y_px).into(),
        )));
    }

    pub fn key(&self, key: DomKey, pressed: bool) {
        let state = if pressed { KeyState::Down } else { KeyState::Up };
        let _ = self
            .active_tab()
            .webview
            .notify_input_event(InputEvent::Keyboard(KeyboardEvent::from_state_and_key(state, key)));
    }
}
