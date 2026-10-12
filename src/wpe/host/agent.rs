//! The engine half of the agent tools (`crate::agent`): script that answers,
//! a snapshot of what a tab shows, and input aimed at **any** tab by index —
//! not only the shown one, which is all the chrome's own input ever needs.
//!
//! Everything here answers through a [`Done`], called on the main thread
//! from inside `pump` once WebKit finishes; it is called exactly once,
//! including when the work could not even start, so a caller waiting on it
//! is never left waiting.

use std::ffi::CString;

use url::Url;

use super::*;

/// What an engine request answers with.
pub type Done<T> = Box<dyn FnOnce(Result<T, String>)>;

/// A snapshot: tightly packed, premultiplied BGRA (WebKit's own format).
pub struct AgentImage {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerKind {
    Move,
    Down,
    Up,
}

/// A string for WebKit, refused rather than panicking on an interior nul —
/// what is passed here came from an agent, not from this crate.
fn c_string(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| "text contains a NUL character".to_string())
}

impl WebKitHost {
    /// Keep the script world `world` alive for the life of each document.
    ///
    /// WebKit drops a named world that nothing on the content manager holds
    /// once the script run in it returns, and with it every global that
    /// script left — so the page half's refs started again from `ref_1` on
    /// every call. A message channel registered in the world holds it; no
    /// one listens on it.
    pub fn agent_keep_world(&self, world: &str) {
        let (Ok(name), Ok(world)) = (c_string("cceAgentKeep"), c_string(world)) else { return };
        unsafe {
            if webkit_user_content_manager_register_script_message_handler(self.ucm, name.as_ptr(), world.as_ptr()) == 0 {
                log::warn!("could not hold the agent script world; refs will not outlive a call");
            }
        }
    }

    /// The viewport every tab lays out in, in CSS pixels.
    pub fn agent_viewport(&self) -> (f64, f64) {
        let (w, h) = self.logical_size();
        (w as f64, h as f64)
    }

    /// The index of the tab with this id, while it is open.
    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    fn webview_at(&self, index: usize) -> Result<*mut WebKitWebView, String> {
        self.tabs.get(index).map(|t| t.webview).ok_or_else(|| "no such tab".to_string())
    }

    /// Run `script` in tab `index`'s top frame and answer its completion
    /// value as JSON (`undefined` for none). `world` is a private script
    /// world, `None` the page's own. Not an async function: the completion
    /// value of the last statement is the answer, the way a console reads.
    pub fn agent_eval(&self, index: usize, script: &str, world: Option<&str>, done: Done<String>) {
        let (wv, script, world) = match (self.webview_at(index), c_string(script), world.map(c_string).transpose()) {
            (Ok(wv), Ok(s), Ok(w)) => (wv, s, w),
            (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => return done(Err(e)),
        };
        unsafe {
            webkit_web_view_evaluate_javascript(
                wv,
                script.as_ptr(),
                -1,
                world.as_ref().map_or(std::ptr::null(), |w| w.as_ptr()),
                std::ptr::null(),
                std::ptr::null_mut(),
                Some(on_agent_eval),
                Box::into_raw(Box::new(done)) as gpointer,
            );
        }
    }

    /// Call `body` as the body of an async function in tab `index`'s top
    /// frame — `return` gives the answer, and a promise it returns is
    /// awaited — answering as [`Self::agent_eval`] does.
    pub fn agent_call(&self, index: usize, body: &str, world: Option<&str>, done: Done<String>) {
        let (wv, body, world) = match (self.webview_at(index), c_string(body), world.map(c_string).transpose()) {
            (Ok(wv), Ok(s), Ok(w)) => (wv, s, w),
            (Err(e), ..) | (_, Err(e), _) | (.., Err(e)) => return done(Err(e)),
        };
        unsafe {
            webkit_web_view_call_async_javascript_function(
                wv,
                body.as_ptr(),
                -1,
                std::ptr::null_mut(),
                world.as_ref().map_or(std::ptr::null(), |w| w.as_ptr()),
                std::ptr::null(),
                std::ptr::null_mut(),
                Some(on_agent_call),
                Box::into_raw(Box::new(done)) as gpointer,
            );
        }
    }

    /// What tab `index` shows in its viewport, freshly painted by WebKit —
    /// a background tab included, whose frames the chrome never reads.
    pub fn agent_snapshot(&self, index: usize, done: Done<AgentImage>) {
        let wv = match self.webview_at(index) {
            Ok(wv) => wv,
            Err(e) => return done(Err(e)),
        };
        unsafe {
            webkit_web_view_get_snapshot(
                wv,
                WebKitSnapshotRegion::WEBKIT_SNAPSHOT_REGION_VISIBLE,
                WebKitSnapshotOptions::WEBKIT_SNAPSHOT_OPTIONS_NONE,
                std::ptr::null_mut(),
                Some(on_agent_snapshot),
                Box::into_raw(Box::new(done)) as gpointer,
            );
        }
    }

    /// Load `url` in tab `index`.
    pub fn agent_load(&self, index: usize, url: &Url) -> Result<(), String> {
        let wv = self.webview_at(index)?;
        let c = c_string(url.as_str())?;
        unsafe { webkit_web_view_load_uri(wv, c.as_ptr()) };
        Ok(())
    }

    /// Back (`-1`), forward (`1`) or reload (`0`) in tab `index`. Refused
    /// when there is no page that way.
    pub fn agent_history(&self, index: usize, step: i32) -> Result<(), String> {
        let wv = self.webview_at(index)?;
        unsafe {
            match step {
                -1 if webkit_web_view_can_go_back(wv) == 0 => return Err("no page to go back to".into()),
                1 if webkit_web_view_can_go_forward(wv) == 0 => return Err("no page to go forward to".into()),
                -1 => webkit_web_view_go_back(wv),
                1 => webkit_web_view_go_forward(wv),
                _ => webkit_web_view_reload(wv),
            }
        }
        Ok(())
    }

    /// A pointer event in tab `index`, at `(x, y)` in the page's CSS pixels
    /// (the chrome's logical pixels). `button` is the X11 number (1 left,
    /// 2 middle, 3 right) for a press or release. Double and triple clicks
    /// come from pressing again soon enough at the same place, as WPE counts
    /// them.
    pub fn agent_pointer(
        &self,
        index: usize,
        kind: PointerKind,
        button: u32,
        x: f64,
        y: f64,
        keys: WPEModifiers::Type,
    ) -> Result<(), String> {
        let view = self.tabs.get(index).map(|t| t.view).ok_or("no such tab")?;
        let time = input::now_ms();
        unsafe {
            let e = match kind {
                PointerKind::Move => wpe_event_pointer_move_new(
                    WPEEventType::WPE_EVENT_POINTER_MOVE,
                    view,
                    WPEInputSource::WPE_INPUT_SOURCE_MOUSE,
                    time,
                    self.held_buttons.get() | keys,
                    x,
                    y,
                    0.0,
                    0.0,
                ),
                PointerKind::Down | PointerKind::Up => {
                    let pressed = kind == PointerKind::Down;
                    let count = if pressed { wpe_view_compute_press_count(view, x, y, button, time) } else { 0 };
                    let bit = input::button_modifier(button);
                    let held = if pressed { self.held_buttons.get() | bit } else { self.held_buttons.get() & !bit };
                    self.held_buttons.set(held);
                    wpe_event_pointer_button_new(
                        if pressed { WPEEventType::WPE_EVENT_POINTER_DOWN } else { WPEEventType::WPE_EVENT_POINTER_UP },
                        view,
                        WPEInputSource::WPE_INPUT_SOURCE_MOUSE,
                        time,
                        held | keys,
                        button,
                        x,
                        y,
                        count,
                    )
                }
            };
            self.send(view, e);
        }
        Ok(())
    }

    /// A precise wheel scroll in tab `index`: `dx`/`dy` in CSS pixels in
    /// cce-ui's convention (positive = toward the top / left), aimed at
    /// `(x, y)` so whatever scrolls under that point is what moves.
    pub fn agent_wheel(&self, index: usize, dx: f64, dy: f64, x: f64, y: f64) -> Result<(), String> {
        let view = self.tabs.get(index).map(|t| t.view).ok_or("no such tab")?;
        unsafe {
            let e = wpe_event_scroll_new(
                view,
                WPEInputSource::WPE_INPUT_SOURCE_MOUSE,
                input::now_ms(),
                0,
                dx,
                dy,
                1,
                0,
                x,
                y,
            );
            self.send(view, e);
        }
        Ok(())
    }

    /// A key press or release in tab `index`, as an X11 keysym.
    pub fn agent_key(&self, index: usize, keyval: u32, pressed: bool, keys: WPEModifiers::Type) -> Result<(), String> {
        let view = self.tabs.get(index).map(|t| t.view).ok_or("no such tab")?;
        unsafe {
            let e = wpe_event_keyboard_new(
                if pressed { WPEEventType::WPE_EVENT_KEYBOARD_KEY_DOWN } else { WPEEventType::WPE_EVENT_KEYBOARD_KEY_UP },
                view,
                WPEInputSource::WPE_INPUT_SOURCE_KEYBOARD,
                input::now_ms(),
                keys,
                0,
                keyval,
            );
            self.send(view, e);
        }
        Ok(())
    }
}

/// A script's answer as JSON, or the exception it threw.
unsafe fn js_answer(v: *mut JSCValue, err: *mut GError) -> Result<String, String> {
    if !err.is_null() {
        let msg = from_cstr((*err).message).unwrap_or_else(|| "the script failed".into());
        g_error_free(err);
        if !v.is_null() {
            g_object_unref(v as *mut _);
        }
        return Err(msg);
    }
    if v.is_null() {
        return Ok("undefined".into());
    }
    let out = if jsc_value_is_undefined(v) != 0 {
        "undefined".to_string()
    } else {
        let raw = jsc_value_to_json(v, 0);
        if raw.is_null() {
            // Not serializable (a cycle, a function): its string form.
            let raw = jsc_value_to_string(v);
            let s = from_cstr(raw).unwrap_or_default();
            g_free(raw as *mut _);
            serde_json::Value::String(s).to_string()
        } else {
            let s = std::ffi::CStr::from_ptr(raw).to_string_lossy().into_owned();
            g_free(raw as *mut _);
            s
        }
    };
    g_object_unref(v as *mut _);
    Ok(out)
}

unsafe extern "C" fn on_agent_eval(source: *mut GObject, res: *mut GAsyncResult, data: gpointer) {
    let done = Box::from_raw(data as *mut Done<String>);
    let mut err: *mut GError = std::ptr::null_mut();
    let v = webkit_web_view_evaluate_javascript_finish(source as *mut WebKitWebView, res, &mut err);
    done(js_answer(v, err));
}

unsafe extern "C" fn on_agent_call(source: *mut GObject, res: *mut GAsyncResult, data: gpointer) {
    let done = Box::from_raw(data as *mut Done<String>);
    let mut err: *mut GError = std::ptr::null_mut();
    let v = webkit_web_view_call_async_javascript_function_finish(source as *mut WebKitWebView, res, &mut err);
    done(js_answer(v, err));
}

unsafe extern "C" fn on_agent_snapshot(source: *mut GObject, res: *mut GAsyncResult, data: gpointer) {
    let done = Box::from_raw(data as *mut Done<AgentImage>);
    let mut err: *mut GError = std::ptr::null_mut();
    let image = webkit_web_view_get_snapshot_finish(source as *mut WebKitWebView, res, &mut err);
    if image.is_null() {
        let msg = if err.is_null() {
            "no snapshot".to_string()
        } else {
            let m = from_cstr((*err).message).unwrap_or_else(|| "no snapshot".into());
            g_error_free(err);
            m
        };
        return done(Err(msg));
    }
    let (w, h) = (webkit_image_get_width(image).max(0) as u32, webkit_image_get_height(image).max(0) as u32);
    let stride = webkit_image_get_stride(image) as usize;
    let bytes = webkit_image_as_bytes(image);
    let mut len: u64 = 0;
    let data = if bytes.is_null() { std::ptr::null() } else { g_bytes_get_data(bytes, &mut len) as *const u8 };
    let row = w as usize * 4;
    let result = if data.is_null() || w == 0 || h == 0 || (len as usize) < stride * (h as usize - 1) + row {
        Err("the snapshot was empty".to_string())
    } else {
        let mut bgra = Vec::with_capacity(row * h as usize);
        for y in 0..h as usize {
            bgra.extend_from_slice(std::slice::from_raw_parts(data.add(y * stride), row));
        }
        Ok(AgentImage { width: w, height: h, bgra })
    };
    g_object_unref(image as *mut _);
    done(result);
}
