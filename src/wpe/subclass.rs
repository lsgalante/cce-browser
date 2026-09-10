//! The three GObject subclasses WPE requires of an embedder.
//!
//! WebKit does not hand us a view to render into; it *asks the display for
//! one*. So embedding means implementing all three of:
//!
//! * `WPEDisplay`  — vends the view and the toplevel (`create_view`,
//!   `create_toplevel`). `WebKitWebView`'s `display` property is
//!   construct-only and takes this.
//! * `WPEToplevel` — **owns buffer-format negotiation.** WebKit asks the
//!   toplevel, not the display. Leave `create_toplevel` NULL and
//!   `render_buffer` silently never fires, with a perfectly healthy web
//!   process and no error anywhere.
//! * `WPEView`     — receives finished frames via `render_buffer`.
//!
//! Registration goes through [`register_subclass`] rather than a Rust struct
//! embedding the parent, because WPE's instance structs are opaque
//! (`WPE_DECLARE_DERIVABLE_TYPE` typedefs `struct _WPEView` and never defines
//! it). `g_type_query` reports the parent's sizes at runtime instead, which is
//! ABI-safe and survives WPE growing a field. The *class* structs are public,
//! so bindgen lays them out correctly and installing a vfunc is a field set.

use std::ffi::{c_char, c_void, CString};

use super::ffi::*;

/// Register a GObject subclass of `parent`, sized from the runtime type query.
pub(super) unsafe fn register_subclass(
    parent: GType,
    name: &str,
    class_init: unsafe extern "C" fn(*mut c_void, *mut c_void),
) -> GType {
    let mut q: GTypeQuery = std::mem::zeroed();
    g_type_query(parent, &mut q);
    assert!(q.type_ != 0, "parent type {name} not registered");
    let cname = CString::new(name).expect("subclass name");
    g_type_register_static_simple(
        parent,
        cname.as_ptr(),
        q.class_size,
        std::mem::transmute::<_, GClassInitFunc>(class_init),
        q.instance_size,
        None,
        0,
    )
}

pub(super) const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// Registered once, on first host construction. GType registration is
/// process-wide and re-registering the same name aborts.
pub(super) struct Types {
    pub display: GType,
    pub view: GType,
    pub toplevel: GType,
    pub clipboard: GType,
}

static mut TYPES: Option<Types> = None;

pub(super) unsafe fn types() -> &'static Types {
    #[allow(static_mut_refs)]
    if TYPES.is_none() {
        TYPES = Some(Types {
            view: register_subclass(wpe_view_get_type(), "CceWpeView", view_class_init),
            toplevel: register_subclass(
                wpe_toplevel_get_type(),
                "CceWpeToplevel",
                toplevel_class_init,
            ),
            display: register_subclass(wpe_display_get_type(), "CceWpeDisplay", display_class_init),
            clipboard: register_subclass(
                wpe_clipboard_get_type(),
                "CceWpeClipboard",
                clipboard_class_init,
            ),
        });
    }
    #[allow(static_mut_refs)]
    TYPES.as_ref().unwrap()
}

// ---- view ----

/// Set by the host before it creates a webview; `render_buffer` hands frames
/// here. One host per process for now (see `WebKitHost::new`).
///
/// Returns whether the sink is **keeping** the buffer. If it is, releasing it
/// is the sink's job — it reads the pixels out at the next pump and hands the
/// memory back then.
pub(super) static mut FRAME_SINK: Option<Box<dyn FnMut(*mut WPEView, *mut WPEBuffer) -> bool>> =
    None;

unsafe extern "C" fn view_render_buffer(
    view: *mut WPEView,
    buffer: *mut WPEBuffer,
    _damage: *const WPERectangle,
    _n_damage: u32,
    _error: *mut *mut GError,
) -> gboolean {
    // The two halves mean different things and are no longer said together.
    // `rendered` means *displayed*: said at once, so the engine's own frame
    // pacing never waits on our readback. `released` means *the memory is
    // yours again*, and that has to wait until the pixels have been copied
    // out of it — so the sink says it, at the pump that reads the buffer.
    // (Saying neither is what stalls the engine after exactly one frame.)
    //
    // Holding the buffer until then is also the backpressure: the engine
    // cannot run arbitrarily far ahead of a browser that is not keeping up,
    // and a frame superseded before anyone read it is handed back unread
    // rather than copied.
    wpe_view_buffer_rendered(view, buffer);
    #[allow(static_mut_refs)]
    let held = FRAME_SINK.as_mut().is_some_and(|sink| sink(view, buffer));
    if !held {
        wpe_view_buffer_released(view, buffer);
    }
    1
}

unsafe extern "C" fn view_class_init(class: *mut c_void, _data: *mut c_void) {
    (*(class as *mut WPEViewClass)).render_buffer = Some(view_render_buffer);
}

// ---- toplevel ----

unsafe extern "C" fn toplevel_formats(_t: *mut WPEToplevel) -> *mut WPEBufferFormats {
    // Mappable ARGB/XRGB linear: what we can read back on the CPU and hand
    // straight to `cce_ui::vk::upload_rgba`. DMABuf comes later (phase 2).
    let b = wpe_buffer_formats_builder_new(std::ptr::null_mut());
    wpe_buffer_formats_builder_append_group(
        b,
        std::ptr::null_mut(),
        WPEBufferFormatUsage::WPE_BUFFER_FORMAT_USAGE_MAPPING,
    );
    for cc in [fourcc(b'A', b'R', b'2', b'4'), fourcc(b'X', b'R', b'2', b'4')] {
        wpe_buffer_formats_builder_append_format(b, cc, 0);
    }
    wpe_buffer_formats_builder_end(b)
}

unsafe extern "C" fn toplevel_resize(t: *mut WPEToplevel, w: i32, h: i32) -> gboolean {
    wpe_toplevel_resized(t, w, h);
    1
}

unsafe extern "C" fn toplevel_class_init(class: *mut c_void, _data: *mut c_void) {
    let c = class as *mut WPEToplevelClass;
    (*c).get_preferred_buffer_formats = Some(toplevel_formats);
    (*c).resize = Some(toplevel_resize);
}

// ---- display ----

unsafe extern "C" fn display_connect(_d: *mut WPEDisplay, _e: *mut *mut GError) -> gboolean {
    1
}

unsafe extern "C" fn display_create_view(d: *mut WPEDisplay) -> *mut WPEView {
    let prop = CString::new("display").unwrap();
    g_object_new(types().view, prop.as_ptr(), d, std::ptr::null::<c_char>()) as *mut WPEView
}

unsafe extern "C" fn display_create_toplevel(
    d: *mut WPEDisplay,
    max_views: u32,
) -> *mut WPEToplevel {
    let (p1, p2) = (
        CString::new("display").unwrap(),
        CString::new("max-views").unwrap(),
    );
    g_object_new(
        types().toplevel,
        p1.as_ptr(),
        d,
        p2.as_ptr(),
        max_views,
        std::ptr::null::<c_char>(),
    ) as *mut WPEToplevel
}

/// One clipboard per process, cached: `get_clipboard` is called repeatedly
/// and must return the same object, since WebKit tracks its change count.
static mut CLIPBOARD: *mut WPEClipboard = std::ptr::null_mut();

unsafe extern "C" fn display_get_clipboard(d: *mut WPEDisplay) -> *mut WPEClipboard {
    if CLIPBOARD.is_null() {
        let prop = CString::new("display").unwrap();
        CLIPBOARD = g_object_new(types().clipboard, prop.as_ptr(), d, std::ptr::null::<c_char>())
            as *mut WPEClipboard;
    }
    CLIPBOARD
}

unsafe extern "C" fn display_class_init(class: *mut c_void, _data: *mut c_void) {
    let c = class as *mut WPEDisplayClass;
    (*c).connect = Some(display_connect);
    (*c).create_view = Some(display_create_view);
    (*c).create_toplevel = Some(display_create_toplevel);
    // Without this, WebKit has no clipboard at all: Ctrl+V in a page reads
    // nothing and Ctrl+C writes nowhere, silently.
    (*c).get_clipboard = Some(display_get_clipboard);
}

// ---- clipboard ----
//
// Routed through `cce_ui`'s wl-copy/wl-paste helpers, which is what the Servo
// backend does too — it keeps the browser on the same clipboard path as the
// rest of the DE rather than opening a second connection of its own.

/// Formats we answer to. WebKit asks by MIME type; anything textual maps to
/// the one string the toolkit deals in.
fn is_text_format(f: &str) -> bool {
    f.starts_with("text/plain") || f == "UTF8_STRING" || f == "STRING"
}

unsafe extern "C" fn clipboard_read(
    _clipboard: *mut WPEClipboard,
    format: *const c_char,
) -> *mut GBytes {
    let format = if format.is_null() {
        String::new()
    } else {
        std::ffi::CStr::from_ptr(format).to_string_lossy().into_owned()
    };
    if !is_text_format(&format) {
        return std::ptr::null_mut();
    }
    let Some(text) = cce_ui::widget::clipboard::read_from_clipboard() else {
        return std::ptr::null_mut();
    };
    let bytes = text.into_bytes().into_boxed_slice();
    let len = bytes.len();
    // The GBytes owns the buffer and frees it through the notify below.
    g_bytes_new_with_free_func(
        Box::into_raw(bytes) as *const c_void,
        len as u64,
        Some(free_boxed_bytes),
        std::ptr::null_mut(),
    )
}

unsafe extern "C" fn free_boxed_bytes(p: gpointer) {
    drop(Box::from_raw(p as *mut u8));
}

/// Set while we push the system clipboard into WPE, so the `changed` that
/// results is not echoed straight back out again.
pub(super) static mut SYNCING: bool = false;

/// Make WPE aware of what the system clipboard holds.
///
/// WPE only knows about content it has been *given*: `read` is never called
/// for a clipboard it believes is empty, which is why paste silently did
/// nothing until this existed. A native Wayland backend would push this on
/// every selection change; we do it at the moment it matters — the paste —
/// rather than polling `wl-paste` in the background forever.
pub(super) unsafe fn sync_system_clipboard(display: *mut WPEDisplay) {
    let Some(text) = cce_ui::widget::clipboard::read_from_clipboard() else {
        return;
    };
    let clipboard = wpe_display_get_clipboard(display);
    if clipboard.is_null() {
        return;
    }
    let content = wpe_clipboard_content_new();
    let c = CString::new(text).unwrap_or_default();
    wpe_clipboard_content_set_text(content, c.as_ptr());
    SYNCING = true;
    wpe_clipboard_set_content(clipboard, content);
    SYNCING = false;
    wpe_clipboard_content_unref(content);

}

/// The page put something on the clipboard. `is_local` distinguishes that
/// from us being told about someone else's copy — without the check we would
/// echo a foreign clipboard straight back and clobber it.
/// The parent `changed`, kept because overriding it without chaining up is
/// what silently broke paste: `wpe_clipboard_set_content` routes through this
/// vfunc, and the **base implementation is what actually stores the content
/// and bumps the change count**. Without the chain-up, `set_content` appeared
/// to succeed while WPE still reported no formats and an empty clipboard, so
/// WebKit never even called `read`.
static mut PARENT_CHANGED: Option<
    unsafe extern "C" fn(*mut WPEClipboard, *mut GPtrArray, gboolean, *mut WPEClipboardContent),
> = None;

unsafe extern "C" fn clipboard_changed(
    clipboard: *mut WPEClipboard,
    formats: *mut GPtrArray,
    is_local: gboolean,
    content: *mut WPEClipboardContent,
) {
    if let Some(parent) = PARENT_CHANGED {
        parent(clipboard, formats, is_local, content);
    }
    // SYNCING guards the other direction: we just pushed the system
    // clipboard in, and copying it straight back out is a pointless round
    // trip through wl-copy.
    if is_local == 0 || content.is_null() || SYNCING {
        return;
    }
    // Borrowed from the content, not ours to free.
    let text = wpe_clipboard_content_get_text(content);
    if !text.is_null() {
        let s = std::ffi::CStr::from_ptr(text).to_string_lossy().into_owned();
        cce_ui::widget::clipboard::copy_to_clipboard(&s);
    }
}

unsafe extern "C" fn clipboard_class_init(class: *mut c_void, _data: *mut c_void) {
    let c = class as *mut WPEClipboardClass;
    let parent = g_type_class_peek_parent(class as gpointer) as *mut WPEClipboardClass;
    PARENT_CHANGED = (!parent.is_null()).then(|| (*parent).changed).flatten();
    (*c).read = Some(clipboard_read);
    (*c).changed = Some(clipboard_changed);
}
