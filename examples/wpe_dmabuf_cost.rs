//! Measures what WebKit's own side costs per frame when it renders into
//! mappable SHM — what the browser asks for today — against GPU DMABufs
//! (WPE-PORT.md, "Phase 2"). A 24px CSS spinner, the cheapest animation
//! there is, drawn at 60 Hz.
//!
//! The toplevel's buffer-format answer and the display's DRM device are
//! swapped in at runtime, so cce-wpe is untouched. DMABuf frames are held
//! here and acknowledged at 60 Hz, as a compositor would; SHM frames go to
//! the host, which reads only their damage. The host's own copy is
//! therefore in "this process" for SHM and absent for DMABuf, which nothing
//! here imports yet.
//!
//! `XP_FORMATS=shm|dmabuf XP_SIZE=2400x1600 XP_SECS=8 cargo run --release
//! -p cce-browser --example wpe_dmabuf_cost`
//!
//! Measured 2026-10-15 (Intel Iris Xe, WebKit 2.54.1), web processes' share
//! of one core:
//!
//! | size      | SHM   | DMABuf |
//! |-----------|-------|--------|
//! | 1200x800  | 7.6%  | 1.6%   |
//! | 2400x1600 | 19.4% | 1.8%   |
//! | 3600x2200 | 37.2% | 1.6%   |
#![allow(dead_code, unused_imports)]
#[derive(Debug, Clone, Copy)]
pub enum EditingCommand { Copy, Cut, Paste }
#[path = "../src/pages.rs"]
mod pages;
#[path = "../src/downloads.rs"]
mod downloads;
#[path = "../src/vi.rs"]
#[allow(dead_code)]
mod vi;
#[path = "../src/wpe/mod.rs"]
mod wpe;

use cce_wpe::ffi::*;
use cce_wpe::subclass::{fourcc, types, FRAME_SINK};

const SPIN: &str = "data:text/html,<!doctype html><title>spin</title><style>body{margin:0;background:%23fff}\
.s{width:24px;height:24px;margin:40px;border:3px solid %23ccc;border-top-color:%2336c;border-radius:50%25;animation:r 1s linear infinite}\
@keyframes r{to{transform:rotate(360deg)}}</style><div class=s></div>";

static mut KIND: [u64; 3] = [0; 3]; // shm, dmabuf, other
static mut FMT: (u32, u64) = (0, 0);

/// DMABuf frames held by the experiment until the next 60 Hz "draw", as a
/// compositor would hold them until its frame callback.
static mut HELD: Vec<(*mut WPEView, *mut WPEBuffer)> = Vec::new();

static mut DEVICE: *mut WPEDRMDevice = std::ptr::null_mut();

/// The Intel GPU (the one the compositor and cce-ui's Vulkan use). Without
/// a device on the display *and* the format group, WebKit ignores a DMABuf
/// offer and quietly stays on SHM.
unsafe fn device() -> *mut WPEDRMDevice {
    if DEVICE.is_null() {
        let (card, render) = (c"/dev/dri/card1", c"/dev/dri/renderD128");
        DEVICE = wpe_drm_device_new(card.as_ptr(), render.as_ptr());
    }
    DEVICE
}

unsafe extern "C" fn display_drm_device(_d: *mut WPEDisplay) -> *mut WPEDRMDevice {
    device()
}

unsafe extern "C" fn dmabuf_formats(_t: *mut WPEToplevel) -> *mut WPEBufferFormats {
    let b = wpe_buffer_formats_builder_new(device());
    wpe_buffer_formats_builder_append_group(b, device(), WPEBufferFormatUsage::WPE_BUFFER_FORMAT_USAGE_RENDERING);
    // Intel's tiled layouts first (what a GPU renders fastest into), then
    // linear. WebKit picked Y-tiled XR24 (0x0100000000000002).
    let mods: [u64; 4] = [0x0100000000000009, 0x0100000000000002, 0x0100000000000001, 0];
    for cc in [fourcc(b'A', b'R', b'2', b'4'), fourcc(b'X', b'R', b'2', b'4')] {
        for m in mods {
            wpe_buffer_formats_builder_append_format(b, cc, m);
        }
    }
    wpe_buffer_formats_builder_end(b)
}

fn ticks_of_web_processes() -> u64 {
    let me = std::process::id() as i32;
    let parent_of = |pid: i32| -> Option<i32> {
        let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        s.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
    };
    let mut total = 0;
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
        if std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default().trim() != "WPEWebProcess" {
            continue;
        }
        let mut p = pid;
        while let Some(pp) = parent_of(p) {
            if pp == me {
                // utime and stime, fields 14 and 15: 11 and 12 after the `)`.
                let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
                let fields: Vec<&str> = s.rsplit_once(')').map_or(vec![], |(_, r)| r.split_whitespace().collect());
                total += fields.get(11).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0)
                    + fields.get(12).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0);
                break;
            }
            if pp <= 1 {
                break;
            }
            p = pp;
        }
    }
    total
}

fn self_ticks() -> u64 {
    let s = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let f: Vec<&str> = s.rsplit_once(')').map_or(vec![], |(_, r)| r.split_whitespace().collect());
    f.get(11).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0) + f.get(12).and_then(|x| x.parse::<u64>().ok()).unwrap_or(0)
}

fn main() {
    let mode = std::env::var("XP_FORMATS").unwrap_or_else(|_| "shm".into());
    let size = std::env::var("XP_SIZE").unwrap_or_else(|_| "2400x1600".into());
    let (w, h) = size.split_once('x').map(|(a, b)| (a.parse().unwrap(), b.parse().unwrap())).unwrap();
    let secs: u64 = std::env::var("XP_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
    unsafe {
        let t = types();
        if mode == "dmabuf" {
            let class = g_type_class_ref(t.toplevel) as *mut WPEToplevelClass;
            (*class).get_preferred_buffer_formats = Some(dmabuf_formats);
            let dclass = g_type_class_ref(t.display) as *mut WPEDisplayClass;
            (*dclass).get_drm_device = Some(display_drm_device);
        }
    }
    let mut host = wpe::WebKitHost::new(url::Url::parse(SPIN).unwrap(), (w, h));
    // Count what kind of buffer each frame is, then hand it on to the host's own sink.
    unsafe {
        #[allow(static_mut_refs)]
        let mut inner = FRAME_SINK.take().unwrap();
        FRAME_SINK = Some(Box::new(move |view, buffer, damage| {
            let ty = (*(*(buffer as *mut GTypeInstance)).g_class).g_type;
            let i = if g_type_is_a(ty, wpe_buffer_shm_get_type()) != 0 {
                0
            } else if g_type_is_a(ty, wpe_buffer_dma_buf_get_type()) != 0 {
                let d = buffer as *mut WPEBufferDMABuf;
                FMT = (wpe_buffer_dma_buf_get_format(d), wpe_buffer_dma_buf_get_modifier(d));
                KIND[1] += 1;
                #[allow(static_mut_refs)]
                HELD.push((view, buffer));
                return true;
            } else {
                2
            };
            KIND[i] += 1;
            inner(view, buffer, damage)
        }));
    }
    let run = |h: &mut wpe::WebKitHost, ms: u64| {
        let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        // Drawn at 60 Hz, as a window would be: the engine is paced to it.
        let mut next_draw = std::time::Instant::now();
        while std::time::Instant::now() < end {
            h.pump();
            if std::time::Instant::now() >= next_draw {
                h.frame_drawn();
                unsafe {
                    #[allow(static_mut_refs)]
                    for (v, b) in HELD.drain(..) {
                        wpe_view_buffer_rendered(v, b);
                        wpe_view_buffer_released(v, b);
                    }
                }
                next_draw += std::time::Duration::from_micros(16_667);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    };
    run(&mut host, 2500);
    let (w0, s0) = (ticks_of_web_processes(), self_ticks());
    let k0 = unsafe { KIND };
    run(&mut host, secs * 1000);
    let (w1, s1) = (ticks_of_web_processes(), self_ticks());
    let k1 = unsafe { KIND };
    let fps = |i: usize| (k1[i] - k0[i]) as f64 / secs as f64;
    let (fmt, modifier) = unsafe { FMT };
    println!(
        "{mode:6} {w}x{h}: web processes {:5.1}% of a core, this process {:5.1}%; frames/s shm {:.0} dmabuf {:.0} other {:.0}{}",
        (w1 - w0) as f64 / secs as f64,
        (s1 - s0) as f64 / secs as f64,
        fps(0),
        fps(1),
        fps(2),
        if fmt != 0 { format!(" (fourcc {:?}, modifier {modifier:#x})", String::from_utf8_lossy(&fmt.to_le_bytes())) } else { String::new() }
    );
}
