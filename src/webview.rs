//! Servo embedding host: boots an in-process Servo against a software
//! (CPU) rendering context, owns the single WebView, and pumps finished
//! frames into cce-ui's image registry as RGBA uploads.
//!
//! Everything here lives on the main thread. Servo wakes the calloop loop
//! through `Waker` (a channel sender); the app then calls [`ServoHost::pump`],
//! which spins Servo's event loop and, when the delegate has flagged a ready
//! frame, paints and reads back pixels. `read_to_image` happens *without*
//! `present()` so the buffer is still there to read.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use dpi::PhysicalSize;
use euclid::Scale;
use servo::{
    DeviceIntRect, DevicePoint, EventLoopWaker, InputEvent, Key as DomKey, KeyState,
    KeyboardEvent, LoadStatus, MouseButton as DomMouseButton, MouseButtonAction, MouseButtonEvent,
    MouseMoveEvent, RenderingContext, Servo, ServoBuilder, SoftwareRenderingContext, WebView,
    WebViewBuilder, WebViewDelegate, WheelDelta, WheelEvent, WheelMode,
};
use url::Url;

use crate::Message;

/// Page state observed by the delegate, polled by the app after each pump.
#[derive(Default)]
pub struct PageState {
    frame_ready: Cell<bool>,
    dirty: Cell<bool>,
    title: RefCell<Option<String>>,
    url: RefCell<Option<Url>>,
    loading: Cell<bool>,
}

struct Delegate {
    state: Rc<PageState>,
    wake: calloop::channel::Sender<Message>,
}

impl Delegate {
    fn touch(&self) {
        self.state.dirty.set(true);
        let _ = self.wake.send(Message::Spin);
    }
}

impl WebViewDelegate for Delegate {
    fn notify_new_frame_ready(&self, _webview: WebView) {
        self.state.frame_ready.set(true);
        self.touch();
    }

    fn notify_page_title_changed(&self, _webview: WebView, title: Option<String>) {
        *self.state.title.borrow_mut() = title;
        self.touch();
    }

    fn notify_url_changed(&self, _webview: WebView, url: Url) {
        *self.state.url.borrow_mut() = Some(url);
        self.touch();
    }

    fn notify_load_status_changed(&self, _webview: WebView, status: LoadStatus) {
        self.state.loading.set(status != LoadStatus::Complete);
        self.touch();
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

pub struct ServoHost {
    servo: Servo,
    webview: WebView,
    context: Rc<SoftwareRenderingContext>,
    state: Rc<PageState>,
    /// Current page frame in the cce-ui image registry: (id, w px, h px).
    image: Option<(u32, u32, u32)>,
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

        let servo = ServoBuilder::default()
            .event_loop_waker(Box::new(Waker(wake.clone())))
            .build();

        let state = Rc::new(PageState::default());
        let webview = WebViewBuilder::new(&servo, context.clone())
            .url(url)
            .delegate(Rc::new(Delegate { state: state.clone(), wake }))
            .build();
        webview.show();
        webview.focus();

        Self { servo, webview, context, state, image: None }
    }

    /// Spin Servo and swap any finished frame into the image registry.
    /// Returns (new frame uploaded, page state changed).
    pub fn pump(&mut self) -> (bool, bool) {
        self.servo.spin_event_loop();
        let dirty = self.state.dirty.take();
        let mut new_frame = false;
        if self.state.frame_ready.take() {
            self.webview.paint();
            let rect = DeviceIntRect::from_size(self.context.size2d().to_i32());
            if let Some(img) = self.context.read_to_image(rect) {
                let (w, h) = img.dimensions();
                let id = cce_ui::vk::upload_rgba(img.into_raw(), w, h);
                if let Some((old, ..)) = self.image.replace((id, w, h)) {
                    cce_ui::vk::free_image(old);
                }
                new_frame = true;
            }
        }
        (new_frame, dirty)
    }

    pub fn image(&self) -> Option<(u32, u32, u32)> {
        self.image
    }

    pub fn title(&self) -> Option<String> {
        self.state.title.borrow().clone()
    }

    pub fn url(&self) -> Option<Url> {
        self.state.url.borrow().clone()
    }

    pub fn loading(&self) -> bool {
        self.state.loading.get()
    }

    pub fn can_go_back(&self) -> bool {
        self.webview.can_go_back()
    }

    pub fn can_go_forward(&self) -> bool {
        self.webview.can_go_forward()
    }

    pub fn load(&self, url: Url) {
        self.webview.load(url);
    }

    pub fn reload(&self) {
        self.webview.reload();
    }

    pub fn back(&self) {
        if self.webview.can_go_back() {
            let _ = self.webview.go_back(1);
        }
    }

    pub fn forward(&self) {
        if self.webview.can_go_forward() {
            let _ = self.webview.go_forward(1);
        }
    }

    /// Resize the webview (and its rendering context) to a physical size.
    pub fn resize(&self, width_px: u32, height_px: u32, scale: f32) {
        self.webview.set_hidpi_scale_factor(Scale::new(scale));
        self.webview
            .resize(PhysicalSize::new(width_px.max(1), height_px.max(1)));
    }

    /// Pointer position in device pixels relative to the webview origin.
    pub fn mouse_move(&self, x_px: f32, y_px: f32) {
        let _ = self.webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(
            DevicePoint::new(x_px, y_px).into(),
        )));
    }

    pub fn mouse_button(&self, button: DomMouseButton, pressed: bool, x_px: f32, y_px: f32) {
        let action = if pressed { MouseButtonAction::Down } else { MouseButtonAction::Up };
        let _ = self.webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
            action,
            button,
            DevicePoint::new(x_px, y_px).into(),
        )));
    }

    /// Wheel in device pixels, winit sign convention (positive y = scroll
    /// up). Servo hit-tests the wheel event, lets the page preventDefault,
    /// and applies the inverted delta as the scroll itself — no separate
    /// scroll event wanted.
    pub fn wheel(&self, dx_px: f64, dy_px: f64, x_px: f32, y_px: f32) {
        let _ = self.webview.notify_input_event(InputEvent::Wheel(WheelEvent::new(
            WheelDelta { x: dx_px, y: dy_px, z: 0.0, mode: WheelMode::DeltaPixel },
            DevicePoint::new(x_px, y_px).into(),
        )));
    }

    pub fn key(&self, key: DomKey, pressed: bool) {
        let state = if pressed { KeyState::Down } else { KeyState::Up };
        let _ = self
            .webview
            .notify_input_event(InputEvent::Keyboard(KeyboardEvent::from_state_and_key(state, key)));
    }
}
