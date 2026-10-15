//! Frames as GPU buffers: WebKit renders the page into a dma-buf on the
//! window's own GPU, and the chrome draws that buffer where it is — no
//! readback into mappable memory on WebKit's side, no copy and upload on
//! ours (WPE-PORT.md, "Phase 2").
//!
//! Why: on the SHM path WebKit composites the whole frame and reads it back
//! *every frame*, whatever changed, so even the smallest animation on a
//! visible page costs in proportion to the window — a 24px spinner took the
//! web process 37% of a core at 3600x2200, against 1.6% rendering into
//! dma-bufs (`examples/wpe_dmabuf_cost.rs`).
//!
//! How, in the order a buffer lives:
//!
//! * **Offered at boot** ([`WebKitHost::offer_gpu_frames`]), before any
//!   webview: WebKit settles a toplevel's buffer type when it first uses
//!   it. cce-ui's probe names the device the window will render on and what
//!   it can sample; mappable memory stays offered behind it, so a refusal
//!   costs nothing. `CCE_BROWSER_SHM=1` keeps the old path.
//! * **Imported once per buffer.** WebKit renders into a few buffers in
//!   turn; each is imported the first time it arrives (`imported`), and a
//!   later frame in it is only *acquired* — ownership moved back to our
//!   queue, no copy.
//! * **Shown until replaced.** The buffer a tab shows is the tab's picture
//!   (`Tab::image`, with `image_gpu` saying the import owns the id). The
//!   one it replaces still sits in frames on the GPU, so it waits in
//!   `retiring` until cce-ui reports those frames finished, is released in
//!   cce-ui (ownership back to WebKit's side), and only then — `releasing`,
//!   `take_released` — handed back with `wpe_view_buffer_released`.
//!   Handing it back at once would let WebKit draw the next frame into a
//!   picture still on screen.
//! * **Gone when WebKit destroys it** (a resize, a process stopped by sleep,
//!   a tab closed): a weak reference reports it, and the image is freed —
//!   unless a tab still shows it, which keeps the picture (a sleeping tab's
//!   last frame) until something replaces it. The import holds the memory,
//!   so it stays valid that long.
//!
//! `rendered` — the engine's pacing — is said as on the SHM path: when the
//! frame is taken, so the page still runs at the rate the window is drawn.

use super::*;

use std::collections::HashMap;

/// How long a buffer whose rendering has not finished is waited for. WebKit
/// hands buffers over finished (measured: every one, under load), so this
/// is a guard, not a pace.
const RENDER_WAIT_MS: i32 = 50;

/// One imported buffer.
struct Import {
    id: u32,
    width: u32,
    height: u32,
    /// WebKit has not destroyed it.
    alive: bool,
}

#[derive(Default)]
pub(super) struct GpuFrames {
    /// Every buffer imported, by `WPEBuffer` address.
    imported: HashMap<usize, Import>,
    /// Per view, by address: the buffer its tab shows.
    shown: HashMap<usize, usize>,
    /// Buffers no longer shown: (view, buffer, the frame serial after which
    /// nothing draws them).
    retiring: Vec<(usize, usize, u64)>,
    /// Released in cce-ui, by image id: the view and buffer to hand back
    /// once it has run.
    releasing: HashMap<u32, (usize, usize)>,
    /// Buffers WebKit has destroyed since the last look, by address.
    dead: Rc<RefCell<Vec<usize>>>,
}

/// Whether WebKit handed this frame over as a dma-buf.
pub(super) unsafe fn is_dmabuf(buffer: *mut WPEBuffer) -> bool {
    g_type_check_instance_is_a(buffer as *mut GTypeInstance, wpe_buffer_dma_buf_get_type()) != 0
}

/// A buffer is gone: note it for the next look (the weak reference's
/// notify; it may not touch the buffer).
unsafe extern "C" fn on_buffer_gone(data: gpointer, buffer: *mut GObject) {
    let dead = Rc::from_raw(data as *const RefCell<Vec<usize>>);
    dead.borrow_mut().push(buffer as usize);
}

/// Wait, briefly, for whatever is still rendering into the buffer: a
/// dma-buf polls readable once its writes are done.
fn wait_rendered(fd: i32) {
    use rustix::event::{poll, PollFd, PollFlags};
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    if poll(&mut fds, 0).unwrap_or(0) == 1 {
        return;
    }
    let started = std::time::Instant::now();
    let done = poll(&mut fds, RENDER_WAIT_MS).unwrap_or(0) == 1;
    log::debug!("waited {:?} for a frame to finish rendering (done: {done})", started.elapsed());
}

impl WebKitHost {
    /// Have WebKit render into GPU buffers the window draws as they are.
    /// Call before the host is built (`restore` / `new`), from the app only:
    /// a host nothing draws (the examples) keeps reading frames from
    /// mappable memory. Returns whether the offer was made.
    pub fn offer_gpu_frames() -> bool {
        if std::env::var_os("CCE_BROWSER_SHM").is_some() {
            log::info!("CCE_BROWSER_SHM: page frames through mappable memory");
            return false;
        }
        let Some(support) = cce_ui::vk::probe_dmabuf() else {
            return false;
        };
        use std::os::unix::ffi::OsStrExt;
        let node = |p: &std::path::Path| CString::new(p.as_os_str().as_bytes()).ok();
        let Some(render_node) = node(&support.render_node) else { return false };
        let offer = super::super::subclass::DmaBufOffer {
            render_node,
            primary_node: support.primary_node.as_deref().and_then(node),
            formats: support.formats,
        };
        unsafe { super::super::subclass::offer_dmabuf(offer) };
        log::info!("page frames offered as GPU buffers on {}", support.render_node.display());
        true
    }

    /// Take a dma-buf frame for tab `index`: import or acquire it, make it
    /// the tab's picture, and retire the one it replaces. Returns false
    /// when it could not be taken (the caller hands it straight back).
    pub(super) unsafe fn take_gpu_frame(&mut self, index: usize, view: *mut WPEView, buffer: *mut WPEBuffer) -> bool {
        let d = buffer as *mut WPEBufferDMABuf;
        let (w, h) = (wpe_buffer_get_width(buffer).max(0) as u32, wpe_buffer_get_height(buffer).max(0) as u32);
        let planes = wpe_buffer_dma_buf_get_n_planes(d);
        if planes == 0 || w == 0 || h == 0 {
            return false;
        }
        wait_rendered(wpe_buffer_dma_buf_get_fd(d, 0));
        let key = buffer as usize;
        let mut gpu = self.gpu.borrow_mut();
        let id = match gpu.imported.get(&key) {
            Some(i) if (i.width, i.height) == (w, h) => {
                cce_ui::vk::dmabuf_acquire(i.id);
                i.id
            }
            _ => {
                let mut planes_out = Vec::with_capacity(planes as usize);
                for p in 0..planes {
                    let raw = wpe_buffer_dma_buf_get_fd(d, p);
                    let Ok(fd) = std::os::fd::BorrowedFd::borrow_raw(raw).try_clone_to_owned() else {
                        log::warn!("could not dup a page frame's dma-buf fd");
                        return false;
                    };
                    planes_out.push(cce_ui::vk::DmaBufPlane {
                        fd,
                        offset: wpe_buffer_dma_buf_get_offset(d, p),
                        stride: wpe_buffer_dma_buf_get_stride(d, p),
                    });
                }
                let id = cce_ui::vk::import_dmabuf(cce_ui::vk::DmaBuf {
                    width: w,
                    height: h,
                    fourcc: wpe_buffer_dma_buf_get_format(d),
                    modifier: wpe_buffer_dma_buf_get_modifier(d),
                    planes: planes_out,
                });
                if let Some(old) = gpu.imported.insert(key, Import { id, width: w, height: h, alive: true }) {
                    // Same buffer, new size: not seen from WebKit, which makes
                    // a new buffer for a new size, but the old image must not leak.
                    cce_ui::vk::free_image(old.id);
                } else {
                    g_object_weak_ref(
                        buffer as *mut GObject,
                        Some(on_buffer_gone),
                        Rc::into_raw(gpu.dead.clone()) as gpointer,
                    );
                }
                id
            }
        };
        let tab = &mut self.tabs[index];
        if !tab.image_gpu {
            if let Some((old, ..)) = tab.image.take() {
                cce_ui::vk::free_image(old);
            }
        }
        tab.image = Some((id, w, h));
        tab.image_gpu = true;
        if let Some(prev) = gpu.shown.insert(view as usize, key) {
            if prev != key {
                gpu.retiring.push((view as usize, prev, cce_ui::vk::frames_submitted()));
            }
        }
        true
    }

    /// A tab is giving up its GPU picture (its view is going, or its frames
    /// now come another way): what it showed retires like a replaced frame.
    pub(super) fn unshow_gpu_frame(&self, view: *mut WPEView) {
        let mut gpu = self.gpu.borrow_mut();
        if let Some(prev) = gpu.shown.remove(&(view as usize)) {
            gpu.retiring.push((view as usize, prev, cce_ui::vk::frames_submitted()));
        }
    }

    /// Move buffers along: hand back those whose last frames have finished
    /// and whose release has run, and free the images of buffers WebKit has
    /// destroyed. From every `pump` and every draw.
    pub(super) fn retire_gpu_frames(&self) {
        let mut gpu = self.gpu.borrow_mut();
        if gpu.imported.is_empty() {
            return;
        }
        let dead = std::mem::take(&mut *gpu.dead.borrow_mut());
        for b in dead {
            if let Some(i) = gpu.imported.get_mut(&b) {
                i.alive = false;
            }
        }
        // Released in cce-ui: back to WebKit, if it still has the buffer.
        for id in cce_ui::vk::take_released() {
            let Some((view, buffer)) = gpu.releasing.remove(&id) else { continue };
            if gpu.imported.get(&buffer).is_some_and(|i| i.alive) && self.view_alive(view) {
                unsafe { wpe_view_buffer_released(view as *mut WPEView, buffer as *mut WPEBuffer) };
            }
        }
        // No frame draws these any more: release them in cce-ui, or free
        // them if WebKit has let them go.
        let finished = cce_ui::vk::frames_finished();
        let (done, waiting): (Vec<_>, Vec<_>) = gpu.retiring.drain(..).partition(|&(.., serial)| finished >= serial);
        gpu.retiring = waiting;
        for (view, buffer, _) in done {
            let Some(i) = gpu.imported.get(&buffer) else { continue };
            if i.alive {
                let id = i.id;
                cce_ui::vk::dmabuf_release(id);
                gpu.releasing.insert(id, (view, buffer));
            } else {
                cce_ui::vk::free_image(i.id);
                gpu.imported.remove(&buffer);
            }
        }
        // Destroyed and nowhere in use: free now.
        let in_use: std::collections::HashSet<usize> = gpu
            .shown
            .values()
            .copied()
            .chain(gpu.retiring.iter().map(|&(_, b, _)| b))
            .chain(gpu.releasing.values().map(|&(_, b)| b))
            .collect();
        let gone: Vec<usize> =
            gpu.imported.iter().filter(|(b, i)| !i.alive && !in_use.contains(b)).map(|(b, _)| *b).collect();
        for b in gone {
            if let Some(i) = gpu.imported.remove(&b) {
                cce_ui::vk::free_image(i.id);
            }
        }
    }

    /// The renderer was replaced: every imported image went with it. Hand
    /// back every buffer still held and start over; the next frames import
    /// afresh.
    pub(super) fn reset_gpu_frames(&mut self) {
        let mut gpu = self.gpu.borrow_mut();
        let held: Vec<(usize, usize)> = gpu
            .shown
            .iter()
            .map(|(&v, &b)| (v, b))
            .chain(gpu.retiring.iter().map(|&(v, b, _)| (v, b)))
            .chain(gpu.releasing.values().copied())
            .collect();
        for (view, buffer) in held {
            if gpu.imported.get(&buffer).is_some_and(|i| i.alive) && self.view_alive(view) {
                unsafe { wpe_view_buffer_released(view as *mut WPEView, buffer as *mut WPEBuffer) };
            }
        }
        gpu.shown.clear();
        gpu.retiring.clear();
        gpu.releasing.clear();
        // Weak references stay on the buffers still alive; their notices
        // land in `dead` and find nothing to do.
        gpu.imported.clear();
        let _ = cce_ui::vk::take_released();
        drop(gpu);
        for tab in &mut self.tabs {
            if tab.image_gpu {
                tab.image = None;
                tab.image_gpu = false;
            }
        }
    }

    /// Whether a view still belongs to a webview here: a tab's, the spare's,
    /// or a closing tab's.
    fn view_alive(&self, view: usize) -> bool {
        self.tabs.iter().any(|t| t.view as usize == view)
            || self.spare.as_ref().is_some_and(|(_, v, _)| *v as usize == view)
            || self.closing.iter().any(|c| c.view() as usize == view)
    }
}
