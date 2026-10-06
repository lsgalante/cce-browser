//! Which page text field is open for typing, as WebKit itself reports it.
//!
//! WebKit tells the embedder about an editable element through an input-method
//! context: `focus_in` when one takes focus in a focused view, `focus_out` when
//! it loses it, `set_cursor_area` as its caret moves. It asks the *display* for
//! that context (`create_input_method_context`), so supplying our own is all it
//! takes to hear about a field in any page, frame or shadow root — `<input>`,
//! `<textarea>` and `contenteditable` alike — with no script injected.
//!
//! The browser claims the field from `display_list` with it
//! (`cce_ui::text_input::claim`), which is what raises the compositor's
//! on-screen keyboard after a tap, exactly as it does for the chrome's own
//! fields.
//!
//! Only the announcements are taken. Every other vfunc is left to the base
//! class, so key events are not filtered and reach the page as they did before
//! this context existed.
//!
//! The one thing the context does not say is `inputmode="none"` — the page
//! draws its own keyboard, or wants none. WebKit has a hint for it
//! (`INHIBIT_OSK`) but 2.52 never sets it (the field arrives as plain
//! `SPELLCHECK`, measured), so a small watcher in every frame reports it
//! instead ([`WATCH_JS`]), and a freshly opened field is not claimed until
//! that report is in or [`REPORT_WAIT`] has passed. Claimed at once, the
//! board would be up before the page could say it wants none.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::time::{Duration, Instant};

use super::ffi::*;

/// A page field with focus: its caret in the view's logical px, once WebKit
/// has said where it is.
#[derive(Clone, Copy, Default)]
struct Field {
    view: usize,
    focused: bool,
    caret: Option<(i32, i32, i32, i32)>,
    /// When WebKit said this field opened.
    since: Option<Instant>,
}

/// The script world the `inputmode` watcher runs in: page script cannot see
/// it, nor post on its channel.
const WORLD: &str = "cce-ime";
/// The channel it reports on.
const CHANNEL: &str = "cceIme";

/// Runs in every frame, and when focus moves reports whether the focused
/// element asks for no keyboard: `"none"` or `"text"`. Only the shown tab
/// speaks — every tab shares the channel, and a background tab is unmapped,
/// so hidden. Visibility rather than `document.hasFocus()`, which also
/// needs the window to hold the keyboard: WebKit opens a field and the board
/// can follow a tap without that (a seat with no keyboard device). A frame
/// whose focus is inside a child frame leaves it to the child, which runs
/// its own copy, so a field in a cross-origin frame is reported too.
const WATCH_JS: &str = r#"(() => {
  const h = window.webkit && window.webkit.messageHandlers;
  if (!h || !h.cceIme) return;
  // After the event, not in it: mid-move, activeElement is still the old one.
  document.addEventListener('focusin', () => setTimeout(() => {
    if (document.visibilityState !== 'visible') return;
    let el = document.activeElement;
    while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement;
    if (!el || el.tagName === 'IFRAME' || el.tagName === 'FRAME') return;
    const none = (el.inputMode || '').toLowerCase() === 'none';
    try { h.cceIme.postMessage(none ? 'none' : 'text'); } catch (e) {}
  }, 0), true);
})();"#;

/// How long a freshly opened field waits for the watcher before it is
/// claimed anyway. The report comes from the same focus change as WebKit's
/// `focus_in` and lands within a few ms of it, either side; this only
/// bounds a page the watcher cannot run in.
const REPORT_WAIT: Duration = Duration::from_millis(100);

/// A report this much older than WebKit's `focus_in` is about an earlier
/// field, not this one.
const REPORT_EARLY: Duration = Duration::from_millis(300);

thread_local! {
    /// Per context, keyed by its pointer. A context is one view's for life.
    static FIELDS: RefCell<Vec<(usize, Field)>> = const { RefCell::new(Vec::new()) };
    /// A field opened, closed or moved since the browser last asked.
    static CHANGED: Cell<bool> = const { Cell::new(false) };
    /// The watcher's last report: when, and whether it was `inputmode=none`.
    static REPORT: Cell<Option<(Instant, bool)>> = const { Cell::new(None) };
}

static mut TYPE: GType = 0;
static mut PARENT_FINALIZE: Option<unsafe extern "C" fn(*mut GObject)> = None;

unsafe fn context_type() -> GType {
    if TYPE == 0 {
        TYPE = super::subclass::register_subclass(
            wpe_input_method_context_get_type(),
            "CceWpeInputMethodContext",
            class_init,
        );
    }
    TYPE
}

fn update(ctx: *mut WPEInputMethodContext, f: impl FnOnce(&mut Field)) {
    let view = unsafe { wpe_input_method_context_get_view(ctx) } as usize;
    FIELDS.with(|m| {
        let mut m = m.borrow_mut();
        let at = match m.iter().position(|(c, _)| *c == ctx as usize) {
            Some(at) => at,
            None => {
                m.push((ctx as usize, Field { view, ..Field::default() }));
                m.len() - 1
            }
        };
        f(&mut m[at].1);
    });
    CHANGED.set(true);
}

unsafe extern "C" fn focus_in(ctx: *mut WPEInputMethodContext) {
    log::debug!("page field: focus in");
    update(ctx, |f| {
        f.focused = true;
        f.since = Some(Instant::now());
    });
    // Wake the loop when the wait for the watcher runs out: an idle page
    // sends nothing else that would.
    g_timeout_add_once(REPORT_WAIT.as_millis() as u32 + 10, Some(on_report_due), std::ptr::null_mut());
}

unsafe extern "C" fn on_report_due(_data: gpointer) {
    CHANGED.set(true);
}

unsafe extern "C" fn focus_out(ctx: *mut WPEInputMethodContext) {
    log::debug!("page field: focus out");
    update(ctx, |f| {
        f.focused = false;
        f.caret = None;
    });
}

unsafe extern "C" fn set_cursor_area(
    ctx: *mut WPEInputMethodContext,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    update(ctx, |f| f.caret = Some((x, y, w, h)));
}

unsafe extern "C" fn finalize(obj: *mut GObject) {
    FIELDS.with(|m| m.borrow_mut().retain(|(c, _)| *c != obj as usize));
    CHANGED.set(true);
    if let Some(parent) = PARENT_FINALIZE {
        parent(obj);
    }
}

unsafe extern "C" fn class_init(class: *mut c_void, _data: *mut c_void) {
    let c = class as *mut WPEInputMethodContextClass;
    (*c).focus_in = Some(focus_in);
    (*c).focus_out = Some(focus_out);
    (*c).set_cursor_area = Some(set_cursor_area);
    PARENT_FINALIZE = (*c).parent_class.finalize;
    (*c).parent_class.finalize = Some(finalize);
}

/// `WPEDisplayClass::create_input_method_context`.
pub(super) unsafe extern "C" fn create_context(
    _d: *mut WPEDisplay,
    view: *mut WPEView,
) -> *mut WPEInputMethodContext {
    let prop = std::ffi::CString::new("view").unwrap();
    g_object_new(context_type(), prop.as_ptr(), view, std::ptr::null::<std::ffi::c_char>())
        as *mut WPEInputMethodContext
}

/// The open field in `view`, if one is: its caret's rectangle in the view's
/// logical px, or `None` inside when WebKit has not placed the caret yet.
///
/// A field that asks for no keyboard is not reported: `inputmode="none"`,
/// by the watcher's last report or WebKit's `INHIBIT_OSK` hint, and a field
/// just opened is held back until the watcher has spoken (see the module
/// notes). Both are read now rather than at `focus_in`, because moving from
/// one field to the next says no focus in or out — only a new caret, and a
/// new report.
pub(super) fn field(view: *mut WPEView) -> Option<Option<(i32, i32, i32, i32)>> {
    let (ctx, field) = FIELDS.with(|m| {
        m.borrow().iter().find(|(_, f)| f.view == view as usize && f.focused).copied()
    })?;
    let hints = unsafe { wpe_input_method_context_get_input_hints(ctx as *mut WPEInputMethodContext) };
    if hints & WPEInputHints::WPE_INPUT_HINT_INHIBIT_OSK != 0 {
        return None;
    }
    let since = field.since.unwrap_or_else(Instant::now);
    match REPORT.get() {
        Some((at, none)) if at + REPORT_EARLY >= since => {
            if none {
                return None;
            }
        }
        _ if since.elapsed() < REPORT_WAIT => return None,
        _ => {}
    }
    Some(field.caret)
}

/// Install the `inputmode` watcher on the content manager every tab is
/// built against, with its channel. Once per host, for its whole life.
pub(super) unsafe fn install_watch(ucm: *mut WebKitUserContentManager) {
    let (name, world) = (cstr(CHANNEL), cstr(WORLD));
    if webkit_user_content_manager_register_script_message_handler(ucm, name.as_ptr(), world.as_ptr()) == 0 {
        log::warn!("could not register the inputmode channel");
        return;
    }
    let signal = cstr(&format!("script-message-received::{CHANNEL}"));
    g_signal_connect_data(
        ucm as *mut _,
        signal.as_ptr(),
        Some(std::mem::transmute::<usize, unsafe extern "C" fn()>(on_report as *const () as usize)),
        std::ptr::null_mut(),
        None,
        0,
    );
    let source = cstr(WATCH_JS);
    // Every frame, from document start: a field is often in a frame, and a
    // page can focus one before its own load finishes.
    let script = webkit_user_script_new_for_world(
        source.as_ptr(),
        WebKitUserContentInjectedFrames::WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES,
        WebKitUserScriptInjectionTime::WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START,
        world.as_ptr(),
        std::ptr::null(),
        std::ptr::null(),
    );
    webkit_user_content_manager_add_script(ucm, script);
    webkit_user_script_unref(script);
}

unsafe extern "C" fn on_report(_ucm: *mut WebKitUserContentManager, value: *mut JSCValue, _data: gpointer) {
    let raw = jsc_value_to_string(value);
    if raw.is_null() {
        return;
    }
    let none = std::ffi::CStr::from_ptr(raw).to_bytes() == b"none";
    g_free(raw as *mut _);
    log::debug!("page field: inputmode {}", if none { "none" } else { "text" });
    REPORT.set(Some((Instant::now(), none)));
    CHANGED.set(true);
}

fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).expect("no NUL")
}

/// Whether any field opened, closed or moved since the last call.
pub(super) fn take_changed() -> bool {
    CHANGED.replace(false)
}
