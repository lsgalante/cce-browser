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
        });
    }
    #[allow(static_mut_refs)]
    TYPES.as_ref().unwrap()
}

// ---- view ----

/// Set by the host before it creates a webview; `render_buffer` hands frames
/// here. One host per process for now (see `WebKitHost::new`).
pub(super) static mut FRAME_SINK: Option<Box<dyn FnMut(*mut WPEBuffer)>> = None;

unsafe extern "C" fn view_render_buffer(
    view: *mut WPEView,
    buffer: *mut WPEBuffer,
    _damage: *const WPERectangle,
    _n_damage: u32,
    _error: *mut *mut GError,
) -> gboolean {
    #[allow(static_mut_refs)]
    if let Some(sink) = FRAME_SINK.as_mut() {
        sink(buffer);
    }
    // BOTH halves. `rendered` means displayed, `released` means the memory is
    // yours again; with only the first the engine produces exactly one frame
    // and then stalls forever. This is also the backpressure that makes an
    // unbounded upload queue impossible here.
    wpe_view_buffer_rendered(view, buffer);
    wpe_view_buffer_released(view, buffer);
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

unsafe extern "C" fn display_class_init(class: *mut c_void, _data: *mut c_void) {
    let c = class as *mut WPEDisplayClass;
    (*c).connect = Some(display_connect);
    (*c).create_view = Some(display_create_view);
    (*c).create_toplevel = Some(display_create_toplevel);
}
