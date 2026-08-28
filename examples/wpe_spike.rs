//! Rust port of `spike/wpe-spike.c` — the WPE embedding contract, from Rust.
//!
//! Run with:  `cargo run --release -p cce-browser --features wpe --example wpe_spike`
//! Writes the second rendered frame to `/tmp/wpe-spike-rs.ppm`.
//!
//! The point of this example is the **subclassing**, which is the only part
//! of the port that is not ordinary FFI. WPE's instance structs are opaque
//! (`WPE_DECLARE_DERIVABLE_TYPE` typedefs `struct _WPEView` and never defines
//! it), so we cannot write `struct SpikeView { parent: WPEView }` the way the
//! C spike does. Instead `g_type_query` reports the parent's instance and
//! class sizes at runtime and we register against those — ABI-safe, and it
//! keeps working if WPE grows a field. The *class* structs are public, so
//! bindgen lays them out correctly and vfunc assignment is a plain field set.
//!
//! See WPE-PORT.md for the two traps this encodes: the toplevel owns format
//! negotiation, and the buffer handshake has two halves.

#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]

use std::ffi::{c_char, c_void, CString};
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};

mod ffi {
    #![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code)]
    include!(concat!(env!("OUT_DIR"), "/wpe_bindings.rs"));
}
use ffi::*;

static FRAMES: AtomicU32 = AtomicU32::new(0);
static mut LOOP_PTR: *mut GMainLoop = std::ptr::null_mut();

/// Register a GObject subclass of `parent`, sizing it from the runtime type
/// query rather than from a struct layout we cannot see.
unsafe fn register_subclass(
    parent: GType,
    name: &str,
    class_init: unsafe extern "C" fn(*mut c_void, *mut c_void),
) -> GType {
    let mut q: GTypeQuery = std::mem::zeroed();
    g_type_query(parent, &mut q);
    assert!(q.type_ != 0, "parent type not registered");
    let cname = CString::new(name).unwrap();
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

// ---- the view: receives rendered buffers ----

unsafe extern "C" fn view_render_buffer(
    view: *mut WPEView,
    buffer: *mut WPEBuffer,
    _damage: *const WPERectangle,
    n_damage: u32,
    _error: *mut *mut GError,
) -> gboolean {
    let n = FRAMES.fetch_add(1, Ordering::SeqCst) + 1;
    let (w, h) = (wpe_buffer_get_width(buffer), wpe_buffer_get_height(buffer));
    let is_shm = g_type_check_instance_is_a(buffer as *mut GTypeInstance, wpe_buffer_shm_get_type()) != 0;
    println!("render_buffer #{n}: {w}x{h} shm={is_shm} damage_rects={n_damage}");

    if is_shm && n == 2 {
        let shm = buffer as *mut WPEBufferSHM;
        let bytes = wpe_buffer_shm_get_data(shm);
        let mut len: u64 = 0;
        let px = g_bytes_get_data(bytes, &mut len as *mut u64) as *const u8;
        let stride = wpe_buffer_shm_get_stride(shm) as usize;
        println!("  bytes={len} stride={stride} format={}", wpe_buffer_shm_get_format(shm));

        // ARGB8888 is B,G,R,A in memory on little-endian.
        let mut out = Vec::with_capacity(w as usize * h as usize * 3 + 32);
        out.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
        for y in 0..h as usize {
            for x in 0..w as usize {
                let p = px.add(y * stride + x * 4);
                out.push(*p.add(2));
                out.push(*p.add(1));
                out.push(*p);
            }
        }
        let _ = std::fs::File::create("/tmp/wpe-spike-rs.ppm").map(|mut f| f.write_all(&out));
        println!("  WROTE /tmp/wpe-spike-rs.ppm");
        g_main_loop_quit(LOOP_PTR);
    }

    // Both halves, or the engine produces one frame and stalls forever.
    wpe_view_buffer_rendered(view, buffer);
    wpe_view_buffer_released(view, buffer);
    1
}

unsafe extern "C" fn view_class_init(class: *mut c_void, _data: *mut c_void) {
    (*(class as *mut WPEViewClass)).render_buffer = Some(view_render_buffer);
}

// ---- the toplevel: WebKit asks IT for buffer formats ----

unsafe extern "C" fn toplevel_formats(_t: *mut WPEToplevel) -> *mut WPEBufferFormats {
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
    println!("toplevel resize -> {w}x{h}");
    wpe_toplevel_resized(t, w, h);
    1
}

unsafe extern "C" fn toplevel_class_init(class: *mut c_void, _data: *mut c_void) {
    let c = class as *mut WPEToplevelClass;
    (*c).get_preferred_buffer_formats = Some(toplevel_formats);
    (*c).resize = Some(toplevel_resize);
}

// ---- the display: vends the view and the toplevel ----

static mut VIEW_TYPE: GType = 0;
static mut TOPLEVEL_TYPE: GType = 0;

unsafe extern "C" fn display_connect(_d: *mut WPEDisplay, _e: *mut *mut GError) -> gboolean {
    1
}

unsafe extern "C" fn display_create_view(d: *mut WPEDisplay) -> *mut WPEView {
    let prop = CString::new("display").unwrap();
    g_object_new(VIEW_TYPE, prop.as_ptr(), d, std::ptr::null::<c_char>()) as *mut WPEView
}

unsafe extern "C" fn display_create_toplevel(d: *mut WPEDisplay, max_views: u32) -> *mut WPEToplevel {
    println!("create_toplevel(max_views={max_views})");
    let (p1, p2) = (CString::new("display").unwrap(), CString::new("max-views").unwrap());
    g_object_new(TOPLEVEL_TYPE, p1.as_ptr(), d, p2.as_ptr(), max_views, std::ptr::null::<c_char>())
        as *mut WPEToplevel
}

unsafe extern "C" fn display_class_init(class: *mut c_void, _data: *mut c_void) {
    let c = class as *mut WPEDisplayClass;
    (*c).connect = Some(display_connect);
    (*c).create_view = Some(display_create_view);
    (*c).create_toplevel = Some(display_create_toplevel);
}

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

fn main() {
    let url = std::env::args().nth(1).unwrap_or_else(|| "https://example.com".into());
    let curl = CString::new(url.clone()).unwrap();

    unsafe {
        VIEW_TYPE = register_subclass(wpe_view_get_type(), "CceSpikeView", view_class_init);
        TOPLEVEL_TYPE = register_subclass(wpe_toplevel_get_type(), "CceSpikeToplevel", toplevel_class_init);
        let display_type = register_subclass(wpe_display_get_type(), "CceSpikeDisplay", display_class_init);

        let display = g_object_new(display_type, std::ptr::null::<c_char>()) as *mut WPEDisplay;
        let mut err: *mut GError = std::ptr::null_mut();
        if wpe_display_connect(display, &mut err) == 0 {
            eprintln!("connect failed");
            std::process::exit(1);
        }

        let prop = CString::new("display").unwrap();
        let wv = g_object_new(webkit_web_view_get_type(), prop.as_ptr(), display, std::ptr::null::<c_char>())
            as *mut WebKitWebView;
        let view = webkit_web_view_get_wpe_view(wv);
        println!("web view={wv:?} wpe_view={view:?}");

        let top = wpe_display_create_toplevel(display, 1);
        if !top.is_null() {
            wpe_toplevel_resized(top, 1200, 800);
            wpe_view_set_toplevel(view, top);
        }
        wpe_view_resized(view, 1200, 800);
        wpe_view_set_visible(view, 1);
        wpe_view_map(view);

        webkit_web_view_load_uri(wv, curl.as_ptr());
        LOOP_PTR = g_main_loop_new(std::ptr::null_mut(), 0);
        g_timeout_add_seconds(30, Some(std::mem::transmute(g_main_loop_quit as *const ())), LOOP_PTR as *mut c_void);
        g_main_loop_run(LOOP_PTR);
    }
    println!("done, {} frames", FRAMES.load(Ordering::SeqCst));
}
