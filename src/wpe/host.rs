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
use std::ffi::{c_char, c_void, CString};
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
    /// The tab's WebProcess died, and why (`WebKitWebProcessTerminationReason`).
    /// Set by the signal, taken by `pump`, which puts the error page up —
    /// outside the signal, so the load is not started from inside WebKit's
    /// own teardown of the process.
    terminated: Cell<Option<WebKitWebProcessTerminationReason::Type>>,
    /// WebKit's responsiveness timer gave up on the WebProcess: a message
    /// has gone ~3s without an answer. Cleared when it answers again.
    unresponsive: Cell<bool>,
    /// The person chose to wait on this hang, so it is not asked about again
    /// until the page recovers and hangs anew.
    hang_waived: Cell<bool>,
    /// When the outstanding [`WebKitHost::ping`] went out, if one has not
    /// been answered yet.
    ping_since: Cell<Option<std::time::Instant>>,
    /// The process is being stopped as a deadlock, so its termination should
    /// reload the page rather than show the error page.
    auto_reload: Cell<bool>,
    /// When this tab was last recovered that way.
    last_auto: Cell<Option<std::time::Instant>>,
}

/// How long every page process must sit idle while the active tab is
/// unresponsive before the hang is taken for a deadlock and recovered
/// without asking.
const DEADLOCK_WATCH: std::time::Duration = std::time::Duration::from_secs(5);

/// A tab is recovered automatically at most this often. A page that
/// deadlocks on every load is left to the prompt instead of reloading in a
/// loop.
const AUTO_RECOVER_GAP: std::time::Duration = std::time::Duration::from_secs(120);

/// A deadlock in progress: when the watch began, on which tab, and every
/// page process's CPU time at that moment.
struct DeadlockWatch {
    tab: Rc<TabState>,
    since: std::time::Instant,
    ticks: std::collections::HashMap<i32, u64>,
}

/// CPU time (user + system, in `/proc` clock ticks) of every WPEWebProcess
/// below this one — they sit under bubblewrap, so not as direct children.
///
/// WebKit has no API that says which process serves which tab, so the
/// watch reads all of them. That is what makes it conservative: one busy
/// process anywhere is enough to leave the hang to the prompt.
fn web_process_ticks() -> std::collections::HashMap<i32, u64> {
    let me = std::process::id() as i32;
    let stat = |pid: i32| std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
    // The fields after the parenthesized command name, which may hold spaces.
    let fields = |s: &str| -> Vec<String> {
        s.rsplit_once(')').map_or_else(Vec::new, |(_, rest)| {
            rest.split_whitespace().map(str::to_string).collect()
        })
    };
    let mut out = std::collections::HashMap::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if comm.trim() != "WPEWebProcess" {
            continue;
        }
        let Some(s) = stat(pid) else { continue };
        let f = fields(&s);
        let mut parent = f.get(1).and_then(|p| p.parse::<i32>().ok());
        let mut ours = false;
        for _ in 0..4 {
            match parent {
                Some(p) if p == me => {
                    ours = true;
                    break;
                }
                Some(p) if p > 1 => {
                    parent = stat(p).and_then(|s| fields(&s).get(1).and_then(|p| p.parse().ok()));
                }
                _ => break,
            }
        }
        // utime and stime are fields 14 and 15; `f` starts at field 3.
        let ticks = |i: usize| f.get(i).and_then(|t| t.parse::<u64>().ok()).unwrap_or(0);
        if ours {
            out.insert(pid, ticks(11) + ticks(12));
        }
    }
    out
}

/// How long a page may leave a ping unanswered before it counts as hung —
/// WebKit's own responsiveness timeout.
const HANG_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// The world the ping runs in, so the page never sees it.
const PING_WORLD: &str = "cce-ping";

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
    /// Under `CCE_BROWSER_DAMAGE_CHECK` only: the picture `image` should
    /// hold, patched region by region alongside it.
    mirror: Option<Vec<u8>>,
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
    connect_state(wv, signal, on_notify as *const () as usize, state);
}

/// Connect `cb` with a `TabState` as its data.
unsafe fn connect_state(wv: *mut WebKitWebView, signal: &str, cb: usize, state: &Rc<TabState>) {
    let name = cstr(signal);
    // Each connection owns its own ref, handed back by `drop_state_ref`.
    let raw = Rc::into_raw(state.clone()) as gpointer;
    g_signal_connect_data(
        wv as *mut _,
        name.as_ptr(),
        Some(std::mem::transmute::<usize, unsafe extern "C" fn()>(cb)),
        raw,
        Some(drop_state_ref),
        0,
    );
}

/// The tab's WebProcess is gone — crashed, killed for memory, or stopped by
/// [`WebKitHost::stop_unresponsive`]. Without this the tab just froze on its
/// last frame, with nothing saying the page behind it no longer existed.
unsafe extern "C" fn on_terminated(
    _wv: *mut WebKitWebView,
    reason: WebKitWebProcessTerminationReason::Type,
    data: gpointer,
) {
    let st = &*(data as *const TabState);
    st.terminated.set(Some(reason));
    st.unresponsive.set(false);
    st.hang_waived.set(false);
    st.ping_since.set(None);
    st.dirty.set(true);
}

/// The ping came back — or failed because the process is gone, which the
/// termination signal reports on its own.
unsafe extern "C" fn on_ping(source: *mut GObject, res: *mut GAsyncResult, data: gpointer) {
    let st = Rc::from_raw(data as *const TabState);
    let mut err: *mut GError = std::ptr::null_mut();
    let v = webkit_web_view_evaluate_javascript_finish(source as *mut WebKitWebView, res, &mut err);
    if !v.is_null() {
        g_object_unref(v as *mut _);
    }
    if !err.is_null() {
        g_error_free(err);
    }
    st.ping_since.set(None);
    if !st.unresponsive.get() {
        st.hang_waived.set(false);
    }
}

/// Fires once the grace has run out. It does nothing itself: being a GLib
/// source is what wakes the loop, and the `pump` that follows is where an
/// unanswered ping is noticed. Without it, a hung page — which sends nothing
/// — would leave the loop asleep and the question unasked.
unsafe extern "C" fn on_ping_due(_data: gpointer) {}

unsafe extern "C" fn on_responsive(obj: *mut GObject, _pspec: *mut GParamSpec, data: gpointer) {
    let st = &*(data as *const TabState);
    let responsive = webkit_web_view_get_is_web_process_responsive(obj as *mut WebKitWebView) != 0;
    st.unresponsive.set(!responsive);
    if responsive {
        st.hang_waived.set(false);
    }
}

/// The page shown in place of one whose WebProcess died. Loaded as
/// *alternate* HTML for the dead page's own URL, so the URL bar, the saved
/// session and Reload all still mean the real page.
fn terminated_page(url: Option<&Url>, reason: WebKitWebProcessTerminationReason::Type) -> String {
    use crate::pages::{html_escape, page};
    use WebKitWebProcessTerminationReason::*;
    let (title, why) = match reason {
        WEBKIT_WEB_PROCESS_EXCEEDED_MEMORY_LIMIT => (
            "This page ran out of memory",
            "Its renderer used more memory than it is allowed and was stopped.",
        ),
        WEBKIT_WEB_PROCESS_TERMINATED_BY_API => (
            "This page was stopped",
            "Its renderer had stopped responding, and was shut down.",
        ),
        _ => ("This page crashed", "Its renderer exited unexpectedly."),
    };
    let meta = match url {
        Some(u) => {
            let u = html_escape(u.as_str());
            format!("{u}<a href=\"{u}\">Reload</a>")
        }
        None => String::new(),
    };
    page(title, &meta, &format!("<p class=empty>{why}</p>"), "")
}

/// The frame handed over by `render_buffer`, drained by `pump`. A slot, not a
/// queue: only the newest frame is ever shown, and the engine will not run far
/// ahead of a browser that has not released the one it is holding.
/// Counters behind `CCE_BROWSER_FRAME_DEBUG=1`: how many frames the engine
/// finished against how many were actually read back. The gap between them is
/// what pacing saves, and it is invisible from the outside — a browser that
/// skips nine frames in ten looks exactly like one that copies all ten.
#[derive(Default)]
struct FrameCounts {
    produced: u64,
    read: u64,
    /// Of `read`, how many copied only their damage.
    partial: u64,
    /// Bytes copied out of the engine's buffers.
    bytes: u64,
}

/// Read whole frames only, ignoring damage. The escape hatch if a page is
/// ever drawn stale; `CCE_BROWSER_DAMAGE_CHECK` is how to find out.
fn full_frames() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("CCE_BROWSER_FULL_FRAMES").is_some())
}

/// Check every region read against the whole frame: each tab keeps a CPU
/// copy of its picture, patched exactly as its image is, and any pixel that
/// disagrees with the engine's buffer is logged. Costs a full copy and a
/// compare per frame, so it is a test switch, not a mode.
fn damage_check() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("CCE_BROWSER_DAMAGE_CHECK").is_some())
}

fn frame_debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("CCE_BROWSER_FRAME_DEBUG").is_some())
}

#[derive(Default)]
struct Pending {
    /// The newest finished buffer the engine has handed over, still unread.
    /// Read and released at the next `pump`; superseded by a newer one, which
    /// hands this one back **unread** — that skipped copy is the whole point
    /// of holding it rather than copying in the callback.
    held: Option<(*mut WPEView, *mut WPEBuffer)>,
    /// Per view, what its frames changed since the last one that was read —
    /// including every frame handed back unread in between, whose changes
    /// the next readback still has to carry. No entry: nothing changed.
    damage: std::collections::HashMap<usize, super::damage::Damage>,
    counts: FrameCounts,
    /// When the counters were last reported.
    reported: Option<std::time::Instant>,
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
    favorites: std::sync::Arc<crate::pages::Favorites>,
    history_enabled: bool,
    force_dark: bool,
    /// Serves the `cce:` pages. Boxed and leaked into the scheme callback,
    /// so it must outlive every webview.
    protocol: Rc<crate::pages::CceProtocol>,
    downloads: std::sync::Arc<crate::downloads::Downloads>,
    clear_cookies: std::sync::Arc<std::sync::atomic::AtomicBool>,
    session: *mut WebKitNetworkSession,
    download_started: Rc<Cell<bool>>,
    /// A page asked something and is blocked until we answer.
    prompts: Rc<RefCell<Prompts>>,
    /// A frame has been uploaded that nothing has drawn yet.
    ///
    /// The readback is paced by this: while it is set, a finished buffer is
    /// left *held* instead of being copied over a picture nobody saw — and
    /// since a held buffer is not yet acknowledged, the engine waits on it
    /// too (see `frame_drawn`).
    pending_draw: Cell<bool>,
    /// The injected account watcher, kept so the setting can take it away
    /// again. `None` when account autocomplete is off, which is also when no
    /// page carries the script at all.
    watcher: Option<*mut WebKitUserScript>,
    /// Retained only so tests can assert on rendered output; the registry
    /// owns the copy that actually gets drawn.
    /// Top-left pixel of the last frame — three bytes, not the frame.
    last_pixel: Option<(u8, u8, u8)>,
    /// Installed on every webview when force-dark is on.
    ucm: *mut WebKitUserContentManager,
    /// A pre-built hidden webview parked on about:blank, WebProcess already
    /// spawned. `open_tab` adopts it and pays only the navigation — measured
    /// at ~65ms to a live internal page against ~250ms building from scratch
    /// (~200ms of which is webview creation + process spawn). The price is
    /// one idle WebProcess held per window. Theme changes reach it anyway:
    /// the colour scheme is display-level and force-dark lives in the shared
    /// user-content-manager it was built with.
    spare: Option<(*mut WebKitWebView, *mut WPEView, Rc<TabState>)>,
    /// Whether the window holds keyboard focus, as last told by [`Self::focus`].
    /// Kept so a tab made active later inherits it: focus belongs to the view,
    /// and only the active tab's view should have it.
    window_focused: bool,
    /// The pointer buttons the page is holding, as `WPE_MODIFIER_POINTER_*`
    /// bits, stamped on every pointer event.
    ///
    /// WebKit reads a drag off the *move* event's own modifiers, not off the
    /// press it saw earlier: a move reporting no held button is a hover, so
    /// press-drag-release over text selected nothing (and dragged nothing)
    /// while every move went out with an empty mask.
    held_buttons: Cell<WPEModifiers::Type>,
    /// A hang being watched to see whether it is a deadlock.
    deadlock_watch: Option<DeadlockWatch>,
    /// The injected vi focus watcher; `None` while vi mode is off, which is
    /// also when no page carries it.
    vi_watcher: Option<*mut WebKitUserScript>,
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

            // Persisted profile. The data directory persists website data
            // (localStorage, IndexedDB, service workers) on its own, but the
            // cookie store stays memory-only until it is explicitly given a
            // file — the set_persistent_storage call below, without which
            // every launch starts logged out of every site even though the
            // rest of the profile survives. Same location and the same 0700
            // reasoning as the Servo backend — the jar holds live sessions.
            let profile = crate::pages::state_dir().join("profile");
            let _ = std::fs::create_dir_all(&profile);
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700));
            }
            let (data_dir, cache_dir) = (
                cstr(&profile.to_string_lossy()),
                cstr(&profile.join("cache").to_string_lossy()),
            );
            let session = webkit_network_session_new(data_dir.as_ptr(), cache_dir.as_ptr());
            let cookie_db = cstr(&profile.join("cookies.sqlite").to_string_lossy());
            webkit_cookie_manager_set_persistent_storage(
                webkit_network_session_get_cookie_manager(session),
                cookie_db.as_ptr(),
                WebKitCookiePersistentStorage::WEBKIT_COOKIE_PERSISTENT_STORAGE_SQLITE,
            );

            let history = std::sync::Arc::new(crate::pages::History::load());
            let bookmarks = std::sync::Arc::new(crate::pages::Bookmarks::load());
            let favorites = std::sync::Arc::new(crate::pages::Favorites::load());
            let downloads = std::sync::Arc::new(crate::downloads::Downloads::default());
            let clear_cookies =
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let protocol = Rc::new(crate::pages::CceProtocol {
                history: history.clone(),
                bookmarks: bookmarks.clone(),
                favorites: favorites.clone(),
                downloads: downloads.clone(),
                clear_cookies: clear_cookies.clone(),
            });

            // The `cce:` scheme, served straight out of the app exactly as the
            // Servo backend serves it — same routing table, so the pages and
            // their mutating links behave identically on both engines.
            let ctx = webkit_web_context_get_default();
            let scheme = cstr("cce");
            webkit_web_context_register_uri_scheme(
                ctx,
                scheme.as_ptr(),
                Some(on_cce_request),
                Rc::into_raw(protocol.clone()) as gpointer,
                None,
            );

            let download_started = Rc::new(Cell::new(false));

            // WebKit fetches downloads itself, and decides what *is* one by
            // content type — so the extension sniff `is_download_url` exists
            // for is simply not needed here, and neither is the argv/URL-bar
            // blind spot it created.
            let ctxs = Rc::new(DownloadCtx {
                downloads: downloads.clone(),
                started: download_started.clone(),
            });
            let sig = cstr("download-started");
            g_signal_connect_data(
                session as *mut _,
                sig.as_ptr(),
                Some(std::mem::transmute::<_, unsafe extern "C" fn()>(
                    on_download_started
                        as unsafe extern "C" fn(*mut GObject, *mut WebKitDownload, gpointer),
                )),
                Rc::into_raw(ctxs) as gpointer,
                None,
                0,
            );

            let prompts = Rc::new(RefCell::new(Prompts::default()));
            let pending = Rc::new(std::cell::RefCell::new(Pending::default()));
            let sink = pending.clone();
            FRAME_SINK = Some(Box::new(move |view: *mut WPEView, buffer: *mut WPEBuffer, damage: &[(i32, i32, i32, i32)]| {
                let mut slot = sink.borrow_mut();
                slot.damage
                    .entry(view as usize)
                    .or_insert_with(|| super::damage::Damage::Rects(Vec::new()))
                    .add(damage);
                // Replace, never accumulate: the newest frame wins. The one it
                // supersedes goes back to the engine **without being read** —
                // several frames can be dispatched inside a single pump's
                // drain, and only the last of them will ever be shown, so the
                // rest are not worth 35 MB of copying each.
                // It will never be shown, so it is as done as it will get:
                // say both halves, or that view composites nothing again.
                if let Some((old_view, old_buffer)) = slot.held.replace((view, buffer)) {
                    wpe_view_buffer_rendered(old_view, old_buffer);
                    wpe_view_buffer_released(old_view, old_buffer);
                }
                if frame_debug() {
                    slot.counts.produced += 1;
                }
                true
            }));

            // `0` is no view limit, and every tab's view must fit: a full
            // toplevel refuses `wpe_view_set_toplevel` *silently*. At `1`
            // (the spike's value) only the first tab ever attached, and every
            // later one ran without the window's scale (rendering at half
            // resolution on a 2x output) or its ACTIVE state (no text caret).
            let toplevel = wpe_display_create_toplevel(display, 0);
            // Scale is 1 until the first `resize` from a real window; the
            // constructor's size is already logical.
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
                history: history.clone(),
                bookmarks: bookmarks.clone(),
                favorites,
                history_enabled: true,
                force_dark: false,
                protocol,
                downloads,
                clear_cookies,
                session,
                download_started,
                pending_draw: Cell::new(false),
                prompts,
                last_pixel: None,
                ucm: webkit_user_content_manager_new(),
                watcher: None,
                spare: None,
                window_focused: false,
                held_buttons: Cell::new(0),
                deadlock_watch: None,
                vi_watcher: None,
            };
            // The account watcher's channel, in its own script world. Both
            // halves are registered here, once, on the shared content
            // manager every tab is built against.
            host.register_account_channel();
            host.register_vi_channel();
            // Which page fields want no on-screen keyboard (`ime.rs`).
            super::ime::install_watch(host.ucm);
            host.open_tab(url);
            host
        }
    }

    /// Listen for the account watcher's messages, in its private world.
    ///
    /// The world is the security boundary: page script cannot post on a
    /// channel registered for another world, so a message arriving here came
    /// from the injected watcher and not from the page pretending to be one.
    fn register_account_channel(&self) {
        use super::formwatch;
        unsafe {
            let name = cstr(formwatch::CHANNEL);
            let world = cstr(formwatch::WORLD);
            if webkit_user_content_manager_register_script_message_handler(
                self.ucm,
                name.as_ptr(),
                world.as_ptr(),
            ) == 0
            {
                log::warn!("could not register the account message channel");
                return;
            }
            let signal = cstr(&format!("script-message-received::{}", formwatch::CHANNEL));
            g_signal_connect_data(
                self.ucm as *mut _,
                signal.as_ptr(),
                Some(std::mem::transmute::<usize, unsafe extern "C" fn()>(
                    on_account_message as *const () as usize,
                )),
                Rc::into_raw(self.prompts.clone()) as gpointer,
                Some(drop_prompts_ref),
                0,
            );

            // The fill asks: a channel with a reply, so a credential goes back
            // to exactly the frame that asked. Same world, same guarantee —
            // page script cannot ask on it.
            let name = cstr(formwatch::FILL_CHANNEL);
            if webkit_user_content_manager_register_script_message_handler_with_reply(
                self.ucm,
                name.as_ptr(),
                world.as_ptr(),
            ) == 0
            {
                log::warn!("could not register the account fill channel");
                return;
            }
            let signal = cstr(&format!(
                "script-message-with-reply-received::{}",
                formwatch::FILL_CHANNEL
            ));
            g_signal_connect_data(
                self.ucm as *mut _,
                signal.as_ptr(),
                Some(std::mem::transmute::<usize, unsafe extern "C" fn()>(
                    on_fill_ask as *const () as usize,
                )),
                Rc::into_raw(self.prompts.clone()) as gpointer,
                Some(drop_prompts_ref),
                0,
            );
        }
    }

    /// Install or remove the login-field watcher — the whole page-side
    /// footprint of the feature, so a browser with accounts turned off
    /// injects nothing at all.
    pub fn set_accounts_enabled(&mut self, on: bool) {
        use super::formwatch;
        unsafe {
            match (on, self.watcher.take()) {
                (true, None) => {
                    let source = cstr(formwatch::WATCH_JS);
                    let world = cstr(formwatch::WORLD);
                    // Every frame — sign-in forms are often a frame of their
                    // own — and at document start, so the listeners are in
                    // place before a login page's own script runs.
                    let script = webkit_user_script_new_for_world(
                        source.as_ptr(),
                        WebKitUserContentInjectedFrames::WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES,
                        WebKitUserScriptInjectionTime::WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START,
                        world.as_ptr(),
                        std::ptr::null(),
                        std::ptr::null(),
                    );
                    webkit_user_content_manager_add_script(self.ucm, script);
                    self.watcher = Some(script);
                }
                (false, Some(script)) => {
                    webkit_user_content_manager_remove_script(self.ucm, script);
                    webkit_user_script_unref(script);
                    self.drop_fill_asks();
                }
                // Already in the asked-for state; `take` above is why the
                // enabled case has to put its handle back.
                (true, Some(script)) => self.watcher = Some(script),
                (false, None) => {}
            }
        }
    }

    /// The next login-field event the watcher reported.
    pub fn take_form_event(&self) -> Option<super::formwatch::FormEvent> {
        self.prompts.borrow_mut().form_events.pop_front()
    }

    /// Drop anything the watcher reported for a page that is going away, so a
    /// stale focus cannot open a list over the next one.
    pub fn clear_form_events(&self) {
        self.prompts.borrow_mut().form_events.clear();
    }

    /// Put a picked account into the login fields of the document `frame`.
    ///
    /// Answers that document's fill ask with the credential, as data on the
    /// reply — the watcher fills its own recorded fields. Returns false, and
    /// sends nothing, when that document has no ask open: it navigated, a
    /// newer ask displaced it, or the tab was switched away from.
    pub fn fill_credentials(&self, frame: &str, username: &str, password: &str) -> bool {
        let reply = {
            let mut p = self.prompts.borrow_mut();
            let Some(at) = p.fill_asks.iter().position(|(t, _)| t == frame) else {
                return false;
            };
            p.fill_asks.remove(at).map(|(_, r)| r)
        };
        let Some(reply) = reply else { return false };
        let answer = super::formwatch::fill_reply(username, password);
        unsafe { answer_fill(reply, Some(&answer)) };
        true
    }

    /// Answer every open fill ask with nothing. On a tab switch and on a
    /// navigation: whatever field asked is no longer the one on screen.
    pub fn drop_fill_asks(&self) {
        let asks: Vec<_> = self.prompts.borrow_mut().fill_asks.drain(..).collect();
        for (_, reply) in asks {
            unsafe { answer_fill(reply, None) };
        }
    }

    // ---- vi mode ----

    /// Listen for the vi focus watcher, in its own private world — the same
    /// guarantee as the account channel: page script cannot post on it.
    fn register_vi_channel(&self) {
        unsafe {
            let name = cstr(crate::vi::CHANNEL);
            let world = cstr(crate::vi::WORLD);
            if webkit_user_content_manager_register_script_message_handler(
                self.ucm,
                name.as_ptr(),
                world.as_ptr(),
            ) == 0
            {
                log::warn!("could not register the vi message channel");
                return;
            }
            let signal = cstr(&format!("script-message-received::{}", crate::vi::CHANNEL));
            g_signal_connect_data(
                self.ucm as *mut _,
                signal.as_ptr(),
                Some(std::mem::transmute::<usize, unsafe extern "C" fn()>(
                    on_vi_message as *const () as usize,
                )),
                Rc::into_raw(self.prompts.clone()) as gpointer,
                Some(drop_prompts_ref),
                0,
            );
        }
    }

    /// Install or remove the vi focus watcher. Off, pages carry nothing.
    pub fn set_vi_enabled(&mut self, on: bool) {
        unsafe {
            match (on, self.vi_watcher.take()) {
                (true, None) => {
                    let source = cstr(&crate::vi::focus_watch_js());
                    let world = cstr(crate::vi::WORLD);
                    // Every frame: the field being clicked into is often in one.
                    let script = webkit_user_script_new_for_world(
                        source.as_ptr(),
                        WebKitUserContentInjectedFrames::WEBKIT_USER_CONTENT_INJECT_ALL_FRAMES,
                        WebKitUserScriptInjectionTime::WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START,
                        world.as_ptr(),
                        std::ptr::null(),
                        std::ptr::null(),
                    );
                    webkit_user_content_manager_add_script(self.ucm, script);
                    self.vi_watcher = Some(script);
                }
                (false, Some(script)) => {
                    webkit_user_content_manager_remove_script(self.ucm, script);
                    webkit_user_script_unref(script);
                }
                (true, Some(script)) => self.vi_watcher = Some(script),
                (false, None) => {}
            }
        }
        self.prompts.borrow_mut().vi_focus = None;
    }

    /// Whether the focused element takes text, if focus moved since last asked.
    pub fn take_vi_focus(&self) -> Option<bool> {
        self.prompts.borrow_mut().vi_focus.take()
    }

    /// The page text field open for typing in the active tab: its caret in
    /// window logical px, or the whole page until WebKit has placed the
    /// caret. What `display_list` claims, so a tap on a page field raises
    /// the on-screen keyboard.
    ///
    /// Not gated on `window_focused`: the toolkit enables text input only
    /// while the compositor has given this surface the text-input focus,
    /// which already says the same thing — and `window_focused` follows
    /// `wl_keyboard`, which a seat with no keyboard device never enters.
    pub fn page_text_field(&self) -> Option<(f32, f32, f32, f32)> {
        let tab = self.tabs.get(self.active)?;
        let field = super::ime::field(tab.view)?;
        let (w, h) = self.logical_size();
        let (x, y, cw, ch) = field.unwrap_or((0, 0, w, h));
        Some((x as f32, y as f32, cw.max(1) as f32, ch.max(1) as f32))
    }

    /// Whether a page field opened, closed or moved since the last call:
    /// the frame that claims it has to be built.
    pub fn take_page_text_field_changed(&self) -> bool {
        super::ime::take_changed()
    }

    /// Run `script` in the active tab's top frame, in the vi world, and queue
    /// its result as a string under `tag` for [`Self::take_vi_result`]. A
    /// failed script answers with an empty string, so a caller waiting on
    /// it is never left waiting.
    pub fn vi_eval(&self, script: &str, tag: u32) {
        let Some(t) = self.tabs.get(self.active) else { return };
        let ctx = Box::new((self.prompts.clone(), tag));
        unsafe {
            let (script, world) = (cstr(script), cstr(crate::vi::WORLD));
            webkit_web_view_evaluate_javascript(
                t.webview,
                script.as_ptr(),
                -1,
                world.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                Some(on_vi_eval),
                Box::into_raw(ctx) as gpointer,
            );
        }
    }

    pub fn take_vi_result(&self) -> Option<(u32, String)> {
        self.prompts.borrow_mut().vi_results.pop_front()
    }

    fn find_controller(&self) -> Option<*mut WebKitFindController> {
        let t = self.tabs.get(self.active)?;
        Some(unsafe { webkit_web_view_get_find_controller(t.webview) })
    }

    /// Find `text` in the active page, highlighting every match and
    /// scrolling to the first. Smart case, as qutebrowser does it: case
    /// matters only when the text has a capital in it.
    pub fn find(&self, text: &str, backwards: bool) {
        let Some(fc) = self.find_controller() else { return };
        use WebKitFindOptions::*;
        let mut opts = WEBKIT_FIND_OPTIONS_WRAP_AROUND;
        if !text.chars().any(char::is_uppercase) {
            opts |= WEBKIT_FIND_OPTIONS_CASE_INSENSITIVE;
        }
        if backwards {
            opts |= WEBKIT_FIND_OPTIONS_BACKWARDS;
        }
        let c = unsafe { cstr(text) };
        unsafe { webkit_find_controller_search(fc, c.as_ptr(), opts, 1000) };
    }

    /// The next match, in the search's own direction.
    pub fn find_next(&self) {
        if let Some(fc) = self.find_controller() {
            unsafe { webkit_find_controller_search_next(fc) }
        }
    }

    pub fn find_prev(&self) {
        if let Some(fc) = self.find_controller() {
            unsafe { webkit_find_controller_search_previous(fc) }
        }
    }

    /// Drop the search and its highlights.
    pub fn find_finish(&self) {
        if let Some(fc) = self.find_controller() {
            unsafe { webkit_find_controller_search_finish(fc) }
        }
        self.prompts.borrow_mut().find_result = None;
    }

    /// How the last search went: the match count, 0 for none.
    pub fn take_find_result(&self) -> Option<u32> {
        self.prompts.borrow_mut().find_result.take()
    }

    fn build_webview(&self, url: &Url, state: &Rc<TabState>) -> (*mut WebKitWebView, *mut WPEView) {
        unsafe {
            let (p_display, p_ucm, p_session) = (
                cstr("display"),
                cstr("user-content-manager"),
                cstr("network-session"),
            );
            let wv = g_object_new(
                webkit_web_view_get_type(),
                p_display.as_ptr(),
                self.display,
                p_ucm.as_ptr(),
                self.ucm,
                p_session.as_ptr(),
                self.session,
                std::ptr::null::<c_char>(),
            ) as *mut WebKitWebView;
            set_features(wv);
            let view = webkit_web_view_get_wpe_view(wv);
            wpe_view_set_toplevel(view, self.toplevel);
            // Signals, not polling: a background tab has to be able to report
            // its title without anyone asking the active webview.
            for sig in ["notify::title", "notify::uri", "notify::is-loading"] {
                connect_notify(wv, sig, state);
            }
            // A dead or hung WebProcess. The first gets an error page in
            // `pump`; the second is offered to the chrome to ask about.
            connect_state(wv, "web-process-terminated", on_terminated as *const () as usize, state);
            connect_state(
                wv,
                "notify::is-web-process-responsive",
                on_responsive as *const () as usize,
                state,
            );
            // A page's alert/confirm/prompt, and HTTP auth challenges. Both
            // are held open and answered later, so the chrome can draw a real
            // dialog rather than the handler having to decide inline.
            connect_raw(
                wv,
                "script-dialog",
                on_script_dialog as *const () as usize,
                &self.prompts,
            );
            connect_raw(
                wv,
                "authenticate",
                on_authenticate as *const () as usize,
                &self.prompts,
            );
            // Right-click reaches the page as button 3; if the page does not
            // preventDefault, WebKit asks for a menu here. Returning TRUE
            // claims presentation, so the chrome draws it.
            connect_raw(
                wv,
                "context-menu",
                on_context_menu as *const () as usize,
                &self.prompts,
            );
            // A `<select>` asking to open. WPE draws no popup of its own: a
            // select nobody answers here simply never opens.
            connect_raw(
                wv,
                "show-option-menu",
                on_show_option_menu as *const () as usize,
                &self.prompts,
            );
            // A middle-clicked link is a background tab, not a navigation.
            connect_raw(
                wv,
                "decide-policy",
                on_decide_policy as *const () as usize,
                &self.prompts,
            );
            // Find-in-page answers on the controller, not the view.
            let fc = webkit_web_view_get_find_controller(wv);
            for (signal, cb) in [
                ("found-text", on_found_text as *const () as usize),
                ("failed-to-find-text", on_failed_to_find as *const () as usize),
            ] {
                let name = cstr(signal);
                g_signal_connect_data(
                    fc as *mut _,
                    name.as_ptr(),
                    Some(std::mem::transmute::<usize, unsafe extern "C" fn()>(cb)),
                    Rc::into_raw(self.prompts.clone()) as gpointer,
                    Some(drop_prompts_ref),
                    0,
                );
            }
            let (lw, lh) = self.logical_size();
            wpe_view_resized(view, lw, lh);
            wpe_view_set_visible(view, 1);
            wpe_view_map(view);
            let curl = cstr(url.as_str());
            webkit_web_view_load_uri(wv, curl.as_ptr());
            (wv, view)
        }
    }

    /// Build the hidden spare webview so its WebProcess is up before the
    /// next `open_tab` needs it.
    fn prewarm_spare(&mut self) {
        if self.spare.is_some() {
            return;
        }
        let state = Rc::new(TabState::default());
        let url = Url::parse("about:blank").expect("about:blank");
        let (wv, view) = self.build_webview(&url, &state);
        unsafe {
            wpe_view_unmap(view);
            wpe_view_set_visible(view, 0);
        }
        self.spare = Some((wv, view, state));
    }

    pub fn open_tab(&mut self, url: Url) {
        self.add_tab(url, true);
    }

    /// Open `url` in a new tab behind the active one: it loads, and shows
    /// up in the strip, but the page on screen and its focus stay put.
    pub fn open_background_tab(&mut self, url: Url) {
        self.add_tab(url, false);
    }

    fn add_tab(&mut self, url: Url, show: bool) {
        let (webview, view, state) = match self.spare.take() {
            // Adopt the prewarmed webview; only the navigation is paid.
            Some((wv, view, state)) => {
                unsafe {
                    let curl = cstr(url.as_str());
                    webkit_web_view_load_uri(wv, curl.as_ptr());
                }
                (wv, view, state)
            }
            None => {
                let state = Rc::new(TabState::default());
                let (wv, view) = self.build_webview(&url, &state);
                if !show {
                    // Built mapped, like every webview; a background one
                    // starts the way a switched-away tab is left.
                    unsafe {
                        wpe_view_unmap(view);
                        wpe_view_set_visible(view, 0);
                    }
                }
                (wv, view, state)
            }
        };
        state.loading.set(true);
        *state.url.borrow_mut() = Some(url.clone());
        self.tabs.push(Tab {
            webview,
            view,
            state,
            title: None,
            url: Some(url),
            loading: true,
            image: None,
            mirror: None,
        });
        if show {
            self.activate(self.tabs.len() - 1);
        }
        // Replace the spare right away, but after the load started, so the
        // page fetch runs while this builds — measured, it does not show up
        // in the click-to-tab time.
        self.prewarm_spare();
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
        self.close_option_menu();
        // Anything still held belongs to a view that may be the one about to
        // be destroyed; hand it back while it is still safe to. Losing that
        // frame costs a repaint, which the tab change causes anyway.
        self.release_held();
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

    /// Give back an unread buffer, if one is being held.
    fn release_held(&self) {
        if let Some((view, buffer)) = self.pending.borrow_mut().held.take() {
            unsafe {
                wpe_view_buffer_rendered(view, buffer);
                wpe_view_buffer_released(view, buffer);
            }
        }
    }

    /// Make tab `index` visible and focused. Mirrors `ServoHost::activate`,
    /// including the `usize::MAX` sentinel so the first call is not a no-op.
    pub fn activate(&mut self, index: usize) {
        if index >= self.tabs.len() || index == self.active {
            return;
        }
        unsafe {
            if let Some(old) = self.tabs.get(self.active) {
                if self.window_focused {
                    wpe_view_focus_out(old.view);
                }
                wpe_view_unmap(old.view);
                wpe_view_set_visible(old.view, 0);
            }
            self.active = index;
            // A select's list belongs to the page going away.
            self.close_option_menu();
            // Login fields reported by, and fill asks from, the tab going
            // away: a list must not open over the next one, and a pick made
            // there must have nowhere to land.
            self.clear_form_events();
            self.drop_fill_asks();
            let tab = &self.tabs[index];
            wpe_view_set_toplevel(tab.view, self.toplevel);
            wpe_view_set_visible(tab.view, 1);
            wpe_view_map(tab.view);
            let (lw, lh) = self.logical_size();
            wpe_view_resized(tab.view, lw, lh);
            if self.window_focused {
                wpe_view_focus_in(tab.view);
            }
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

    /// An owned duplicate of [`Self::poll_fd`], for handing to calloop.
    ///
    /// calloop wants to own what it polls, and the borrow above is tied to
    /// `&self`. A dup refers to the same epoll instance, so registrations
    /// made through the original are still what this observes.
    pub fn poll_fd_owned(&self) -> Option<std::os::fd::OwnedFd> {
        let fd = self.poll.as_ref()?.fd();
        rustix::io::dup(fd).ok()
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
        // `cce://cookies/clear` runs on WebKit's fetch path and cannot reach
        // the session from there, so it sets the flag and this acts on it —
        // the same relay `ServoHost::pump` uses. Timespan 0 clears them all.
        if self
            .clear_cookies
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            unsafe {
                webkit_website_data_manager_clear(
                    webkit_network_session_get_website_data_manager(self.session),
                    WebKitWebsiteDataTypes::WEBKIT_WEBSITE_DATA_COOKIES,
                    0,
                    std::ptr::null_mut(),
                    None,
                    std::ptr::null_mut(),
                );
            }
        }
        unsafe {
            while g_main_context_iteration(std::ptr::null_mut(), 0) != 0 {}
        }
        // WebKit opens and drops sockets as it loads, so the set that matters
        // is the one *after* dispatch, not before.
        if let Some(p) = &mut self.poll {
            p.sync();
        }
        self.watch_deadlock();
        // Nothing has drawn the last frame yet, so reading another would be
        // copying over a picture that was never shown. Leave the buffer held
        // until the draw. (Its view is waiting on `rendered` meanwhile, so it
        // is not followed by another; one from a different view supersedes
        // it and hands it back unread.)
        if self.pending_draw.get() {
            return (false, self.sync_page_state());
        }
        // One readback per pump, of the newest buffer only: everything the
        // engine rendered in between was handed back unread.
        let held = self.pending.borrow_mut().held.take();
        let dirty = self.sync_page_state();
        let Some((view, buffer)) = held else {
            return (false, dirty);
        };
        let damage = self.pending.borrow_mut().damage.remove(&(view as usize));
        // The frame belongs to the tab that drew it, which is not always the
        // one on screen. A view no tab owns is the prewarmed spare, or a tab
        // closed since: nothing shows it.
        let Some(index) = self.tabs.iter().position(|t| t.view == view) else {
            unsafe {
                wpe_view_buffer_rendered(view, buffer);
                wpe_view_buffer_released(view, buffer);
            }
            return (false, dirty);
        };
        let read = unsafe {
            let read = self.read_frame(index, buffer, damage);
            // Read now and drawn at the next frame: as good as on screen, so
            // the engine may start on the next one while this one waits for
            // the draw. That next one is then held, unacknowledged, until
            // the draw has happened — which is what keeps the engine to the
            // chrome's rate. (Said at the draw instead, the two never
            // overlapped, and a Muji banner animating at 60 fps in a visible
            // window dropped to 30.)
            wpe_view_buffer_rendered(view, buffer);
            // The pixels are ours now; the memory can go back.
            wpe_view_buffer_released(view, buffer);
            read
        };
        if frame_debug() {
            let mut p = self.pending.borrow_mut();
            p.counts.read += 1;
            if let Some((bytes, partial)) = read {
                p.counts.bytes += bytes as u64;
                p.counts.partial += partial as u64;
            }
            let now = std::time::Instant::now();
            let due = p.reported.is_none_or(|t| now.duration_since(t).as_secs_f32() >= 1.0);
            if due {
                p.reported = Some(now);
                let c = std::mem::take(&mut p.counts);
                log::info!(
                    "frames: engine produced {}, read back {} ({} handed back unread), \
                     {} of them only their damage; {:.1} MB copied",
                    c.produced,
                    c.read,
                    c.produced.saturating_sub(c.read),
                    c.partial,
                    c.bytes as f64 / 1e6
                );
            }
        }
        if read.is_none() || index != self.active {
            return (false, true);
        }
        self.pending_draw.set(true);
        (true, true)
    }

    /// Copy a finished frame into tab `index`'s image: only the damaged
    /// regions when the image already holds the frame before them, the
    /// whole frame otherwise. Returns the bytes copied and whether it was
    /// regions, or `None` for a buffer that could not be read.
    unsafe fn read_frame(
        &mut self,
        index: usize,
        buffer: *mut WPEBuffer,
        damage: Option<super::damage::Damage>,
    ) -> Option<(usize, bool)> {
        let shm = ShmFrame::of(buffer)?;
        let (w, h) = (shm.width, shm.height);
        // The one pixel anything actually reads back (see `sample_pixel`),
        // kept instead of a copy of the whole frame. Cloning 35 MB per frame
        // to serve a three-byte question cost 7 ms of every frame.
        let p0 = std::slice::from_raw_parts(shm.data, 4);
        self.last_pixel = Some((p0[2], p0[1], p0[0]));
        let tab = &mut self.tabs[index];
        // Regions only make sense against the picture they change: this
        // tab's image, at this size. No damage at all means nothing changed.
        let current = tab.image.is_some_and(|(_, iw, ih)| (iw, ih) == (w, h));
        let regions = match damage {
            _ if full_frames() || !current => None,
            None => Some(Vec::new()),
            Some(d) => d.regions(w, h),
        };
        if let (Some(regions), Some((id, ..))) = (regions, tab.image) {
            let len = super::damage::packed_len(&regions);
            let mut px = cce_ui::vk::recycle_buffer(len);
            super::damage::pack(shm.data, shm.stride, &regions, &mut px);
            if let Some(mirror) = tab.mirror.as_mut() {
                check_regions(mirror, &shm, &px, &regions, index);
            }
            cce_ui::vk::update_pixel_regions(id, px, w, h, cce_ui::vk::PixelFormat::Bgra, regions);
            return Some((len, true));
        }
        let px = shm.copy_all();
        let len = px.len();
        if damage_check() {
            tab.mirror = Some(px.clone());
        }
        match tab.image {
            // Same tab, same size: replace the contents of the image that is
            // already there. No allocation, no descriptor, and above all no
            // image freed — freeing one waits for the whole device to go idle,
            // which on this path meant once per frame.
            Some((id, iw, ih)) if (iw, ih) == (w, h) => {
                cce_ui::vk::update_pixels(id, px, w, h, cce_ui::vk::PixelFormat::Bgra);
            }
            _ => {
                let id = cce_ui::vk::upload_pixels(px, w, h, cce_ui::vk::PixelFormat::Bgra);
                if let Some((old, ..)) = tab.image.replace((id, w, h)) {
                    cce_ui::vk::free_image(old);
                }
            }
        }
        Some((len, false))
    }

    /// Re-paint the page into a renderer that has just replaced the one the
    /// tab images were uploaded to.
    ///
    /// An image id belongs to a **renderer**, not to the process: `cce-ui`'s
    /// `window_runner` repairs a lost Wayland transport by opening a new
    /// session around the same `Application`, which rebuilds the renderer and
    /// with it the image table. A draw for an unknown id is skipped rather
    /// than reported, so the chrome came back over an empty page.
    ///
    /// Two halves. Dropping the ids is the easy one. The hard one is that
    /// nothing would otherwise provoke a new frame: a page that has finished
    /// loading renders once and then only on damage, so `pump` would find no
    /// buffer held and the window would sit blank until the user scrolled or
    /// navigated. Remapping the active view is the nudge — it is what
    /// `activate` already relies on to get a frame out of a tab being
    /// switched to.
    ///
    /// A buffer still held from the old session is deliberately kept: its
    /// pixels are fine, and the next `pump` uploads them under a fresh id.
    pub fn renderer_replaced(&mut self) {
        for tab in &mut self.tabs {
            if let Some((id, ..)) = tab.image.take() {
                // A free for an id the new renderer never had is a no-op, and
                // ids are process-unique, so this cannot reach a live image.
                cce_ui::vk::free_image(id);
            }
        }
        unsafe {
            let view = self.active_tab().view;
            wpe_view_unmap(view);
            wpe_view_set_visible(view, 1);
            wpe_view_map(view);
            let (lw, lh) = self.logical_size();
            wpe_view_resized(view, lw, lh);
        }
    }

    /// The chrome drew: whatever was uploaded is on screen, so the next
    /// engine frame is worth reading. Called from `display_list`.
    ///
    /// Returns whether a frame is already waiting to be read. Its view is
    /// held up until it is (`pump` says `rendered` as it reads), and having
    /// been held up it makes no noise that would turn the loop — so the
    /// caller must wake a pump, or the page sits on that frame until GLib's
    /// next timeout.
    ///
    /// This is what paces the engine to the chrome: one frame being drawn,
    /// one more finished and waiting, and nothing composited beyond that. So
    /// the page runs at the output's refresh while the window is up, at the
    /// runner's starvation fallback (~4 a second) with the display off, and
    /// not at all where nothing draws. Anything that reads frames without a
    /// chrome (the examples) must call this too, or the page stops after its
    /// second frame.
    pub fn frame_drawn(&self) -> bool {
        self.pending_draw.set(false);
        self.pending.borrow().held.is_some()
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
            if let Some(reason) = tab.state.terminated.take() {
                // Stopped as a deadlock: just load the page again. A plain
                // load, not a reload, so a page that came from a form post
                // is not posted twice.
                let reload = tab.state.auto_reload.take().then_some(tab.url.as_ref()).flatten();
                if let Some(u) = reload {
                    log::warn!("reloading {u} after stopping its deadlocked web process");
                    unsafe {
                        let c = cstr(u.as_str());
                        webkit_web_view_load_uri(tab.webview, c.as_ptr());
                    }
                } else {
                    // Loading anything respawns a WebProcess; this loads the
                    // error page under the dead page's URL. Reload — the chrome's
                    // or the page's link — then fetches the real one.
                    log::warn!(
                        "web process for {} terminated (reason {reason})",
                        tab.url.as_ref().map_or("<no url>", |u| u.as_str())
                    );
                    unsafe {
                        let html = cstr(&terminated_page(tab.url.as_ref(), reason));
                        let uri = tab.url.as_ref().map(|u| cstr(u.as_str()));
                        webkit_web_view_load_alternate_html(
                            tab.webview,
                            html.as_ptr(),
                            uri.as_ref().map_or(std::ptr::null(), |u| u.as_ptr()),
                            // The page's own URL as the base too: without one the
                            // error page is `about:blank` to itself, so its
                            // Reload link — refused outright when the dead page
                            // was a `file:` — had nowhere real to go.
                            uri.as_ref().map_or(std::ptr::null(), |u| u.as_ptr()),
                        );
                    }
                }
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
        self.last_pixel
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

    /// The active tab's WebProcess has stopped answering, and the person has
    /// not already chosen to wait on it.
    pub fn active_unresponsive(&self) -> bool {
        self.tabs.get(self.active).is_some_and(|t| self.hung(t) && !t.state.hang_waived.get())
    }

    /// The tab's process has stopped answering — by WebKit's timer or an
    /// overdue ping. Never while a page dialog or auth challenge is up: the
    /// process is blocked on *us* then, and a ping sent just before it
    /// opened goes unanswered for as long as it stays open.
    fn hung(&self, t: &Tab) -> bool {
        let p = self.prompts.borrow();
        if p.dialog.is_some() || p.auth.is_some() {
            return false;
        }
        let ping_overdue = t.state.ping_since.get().is_some_and(|at| at.elapsed() >= HANG_GRACE);
        t.state.unresponsive.get() || ping_overdue
    }

    /// Recover a deadlocked page without asking.
    ///
    /// A deadlock and a busy page look alike from here — both stop
    /// answering — but a deadlocked process burns no CPU (the 2026-10-05
    /// one sat at zero, every thread parked on a lock) and a spinning script
    /// burns a whole core. So while the active tab is hung, every page
    /// process's CPU time is sampled; if none of them has used more than a
    /// sliver of it across `DEADLOCK_WATCH`, the process is stopped and the
    /// page loaded again. Anything busier is left to the prompt, as is a
    /// hang the person chose to wait on, and a tab recovered within
    /// `AUTO_RECOVER_GAP`.
    fn watch_deadlock(&mut self) {
        let state = self
            .tabs
            .get(self.active)
            .filter(|t| self.hung(t) && !t.state.hang_waived.get())
            .map(|t| t.state.clone());
        let Some(state) = state else {
            self.deadlock_watch = None;
            return;
        };
        if state.last_auto.get().is_some_and(|at| at.elapsed() < AUTO_RECOVER_GAP) {
            return;
        }
        let watching = self.deadlock_watch.as_ref().filter(|w| Rc::ptr_eq(&w.tab, &state));
        let Some(watch) = watching else {
            self.deadlock_watch = Some(DeadlockWatch {
                tab: state,
                since: std::time::Instant::now(),
                ticks: web_process_ticks(),
            });
            return;
        };
        let elapsed = watch.since.elapsed();
        if elapsed < DEADLOCK_WATCH {
            return;
        }
        let now = web_process_ticks();
        // 5% of one core, at /proc's 100 ticks a second. A process that
        // appeared mid-watch is measured from zero, which counts its whole
        // startup against it — on the side of not stopping anything.
        let budget = (elapsed.as_secs_f64() * 100.0 * 0.05) as u64;
        let idle = !now.is_empty()
            && now.iter().all(|(pid, t)| {
                t.saturating_sub(watch.ticks.get(pid).copied().unwrap_or(0)) <= budget
            });
        if !idle {
            // Busy: a script, most likely. Watch again from here, so a page
            // that stops spinning and then deadlocks is still caught.
            self.deadlock_watch = Some(DeadlockWatch {
                tab: state,
                since: std::time::Instant::now(),
                ticks: now,
            });
            return;
        }
        self.deadlock_watch = None;
        log::warn!(
            "{} has not answered in {:.0}s and no page process is running; \
             stopping it as deadlocked",
            self.tabs[self.active].url.as_ref().map_or("<no url>", |u| u.as_str()),
            (elapsed + HANG_GRACE).as_secs_f64(),
        );
        state.auto_reload.set(true);
        state.last_auto.set(Some(std::time::Instant::now()));
        self.stop_unresponsive();
    }

    /// A dialog or auth challenge was answered: whatever ping was waiting
    /// behind it was waiting on the person, not on a hung page.
    fn forget_ping(&self) {
        if let Some(t) = self.tabs.get(self.active) {
            t.state.ping_since.set(None);
        }
    }

    /// Ask the active page's main thread for an answer, to learn whether it
    /// is still there.
    ///
    /// WebKit's own responsiveness timer misses the commonest hang: pointer
    /// events are queued behind an unacknowledged one, and a *move* — which
    /// always comes before a click — does not start the timer. So the
    /// clicks behind it are never even sent, and the page that ignores them
    /// is never reported. A no-op script, in a world the page cannot see,
    /// is answered by the same main thread, so its silence is the hang.
    fn ping(&self) {
        let Some(t) = self.tabs.get(self.active) else { return };
        if t.state.ping_since.get().is_some() {
            return;
        }
        t.state.ping_since.set(Some(std::time::Instant::now()));
        unsafe {
            let (script, world) = (cstr("0"), cstr(PING_WORLD));
            webkit_web_view_evaluate_javascript(
                t.webview,
                script.as_ptr(),
                -1,
                world.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                Some(on_ping),
                Rc::into_raw(t.state.clone()) as gpointer,
            );
            g_timeout_add_once(HANG_GRACE.as_millis() as u32 + 50, Some(on_ping_due), std::ptr::null_mut());
        }
    }

    /// Leave the active tab's hang alone until it recovers.
    pub fn wait_unresponsive(&self) {
        if let Some(t) = self.tabs.get(self.active) {
            t.state.hang_waived.set(true);
        }
    }

    /// Kill the active tab's hung WebProcess. The termination signal follows,
    /// and with it the error page offering a reload.
    pub fn stop_unresponsive(&self) {
        if let Some(t) = self.tabs.get(self.active) {
            unsafe { webkit_web_view_terminate_web_process(t.webview) }
        }
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
    pub fn set_color_scheme_dark(&self, dark: bool) {
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

    /// A navigation became a download since the last check.
    pub fn take_download_started(&self) -> bool {
        self.download_started.replace(false)
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

    /// The bookmarks store, shared with the `cce://bookmarks` page; the
    /// chrome's bookmarks menu lists and edits it directly.
    pub fn bookmarks(&self) -> std::sync::Arc<crate::pages::Bookmarks> {
        self.bookmarks.clone()
    }

    /// The favorites store, shared with the `cce://favorites` page; the
    /// chrome reads the strip from it.
    pub fn favorites(&self) -> std::sync::Arc<crate::pages::Favorites> {
        self.favorites.clone()
    }

    pub fn active_favorited(&self) -> bool {
        self.active_tab()
            .url
            .as_ref()
            .is_some_and(|u| self.favorites.contains(u.as_str()))
    }

    pub fn toggle_favorite(&self) {
        let tab = self.active_tab();
        if let Some(url) = &tab.url {
            self.favorites
                .toggle(url.as_str(), tab.title.as_deref().unwrap_or(""));
        }
    }

    /// Clipboard on the page. WebKit takes these as named editing commands,
    /// so unlike the Servo backend there is no separate clipboard delegate to
    /// implement — it goes through the platform clipboard itself.
    /// Push the system selection into WPE. Separated so it can be done
    /// ahead of a paste rather than in the same breath — the web process is
    /// a different process, and the content has to reach it.
    pub fn sync_clipboard(&self) {
        unsafe { super::subclass::sync_system_clipboard(self.display) }
    }

    pub fn editing_action_cmd(&self, command: crate::EditingCommand) {
        unsafe {
            // WebKit will not read a clipboard it thinks is empty, so the
            // system selection has to be pushed in before Paste runs.
            if matches!(command, crate::EditingCommand::Paste) {
                super::subclass::sync_system_clipboard(self.display);
            }
            let c = cstr(match command {
                crate::EditingCommand::Copy => "Copy",
                crate::EditingCommand::Cut => "Cut",
                crate::EditingCommand::Paste => "Paste",
            });
            webkit_web_view_execute_editing_command(self.active_tab().webview, c.as_ptr());
        }
    }

    // ---- pending prompts ----

    /// The dialog a page is currently blocked on, if any. Cloned rather than
    /// taken: the chrome redraws from this every frame, and the page stays
    /// blocked until [`Self::respond_dialog`].
    pub fn pending_dialog(&self) -> Option<PendingDialog> {
        self.prompts.borrow().dialog.as_ref().map(|(_, d)| d.clone())
    }

    /// One-shot: the context menu the page just requested, if any. Taken
    /// rather than cloned — the chrome opens it once, at the pointer.
    pub fn take_context_menu(&self) -> Option<ContextMenuInfo> {
        self.prompts.borrow_mut().context_menu.take()
    }

    /// One-shot: the `<select>` list the page just asked to show, if any.
    /// The menu itself stays held until [`Self::pick_option`] or
    /// [`Self::close_option_menu`] answers it, or the page closes it.
    pub fn take_option_menu(&self) -> Option<OptionMenuInfo> {
        self.prompts.borrow_mut().option_menu_new.take()
    }

    /// Whether a select's list is still waiting on an answer. False once the
    /// page has closed it itself — the select was removed, the page
    /// navigated — which is how the chrome learns to take its list down.
    pub fn option_menu_open(&self) -> bool {
        self.prompts.borrow().option_menu.is_some()
    }

    /// Choose option `index` and close the list. The select changes value
    /// and fires its `input`/`change` as for any pick.
    pub fn pick_option(&self, index: usize) {
        let Some(menu) = self.prompts.borrow_mut().option_menu.take() else { return };
        unsafe {
            webkit_option_menu_activate_item(menu, index as u32);
            webkit_option_menu_close(menu);
            g_object_unref(menu as *mut _);
        }
    }

    /// Close the list without choosing. Nothing is selected on the way
    /// (`select_item` is never called), so closing leaves the value as it was.
    pub fn close_option_menu(&self) {
        let menu = {
            let mut p = self.prompts.borrow_mut();
            p.option_menu_new = None;
            p.option_menu.take()
        };
        // Taken out first: `close` emits the menu's own close signal, whose
        // handler borrows the prompts too.
        if let Some(menu) = menu {
            unsafe {
                webkit_option_menu_close(menu);
                g_object_unref(menu as *mut _);
            }
        }
    }

    /// Links the pages asked to open in background tabs since the last call.
    pub fn take_background_opens(&self) -> Vec<Url> {
        std::mem::take(&mut self.prompts.borrow_mut().background_opens)
    }

    /// Fetch `uri` through WebKit's download pipeline — same signals, same
    /// store, same `cce://downloads` page as a navigated download. This is
    /// what "Download Link/Image" in the context menu dispatches to.
    pub fn download_uri(&self, uri: &str) {
        unsafe {
            let c = cstr(uri);
            webkit_web_view_download_uri(self.active_tab().webview, c.as_ptr());
        }
    }

    pub fn pending_auth(&self) -> Option<PendingAuth> {
        self.prompts.borrow().auth.as_ref().map(|(_, a)| a.clone())
    }

    /// Answer the page. `text` carries a `prompt`'s reply; it is ignored for
    /// alert and confirm.
    pub fn respond_dialog(&self, ok: bool, text: Option<&str>) {
        let Some((dialog, pending)) = self.prompts.borrow_mut().dialog.take() else {
            return;
        };
        self.forget_ping();
        unsafe {
            if pending.prompt_default.is_some() {
                // A cancelled prompt must return null, not "" — a page
                // distinguishes the two.
                if ok {
                    let t = cstr(text.unwrap_or(""));
                    webkit_script_dialog_prompt_set_text(dialog, t.as_ptr());
                } else {
                    webkit_script_dialog_prompt_set_text(dialog, std::ptr::null());
                }
            } else if pending.has_cancel {
                webkit_script_dialog_confirm_set_confirmed(dialog, ok as gboolean);
            }
            webkit_script_dialog_close(dialog);
            webkit_script_dialog_unref(dialog);
        }
    }

    /// Answer an auth challenge, or cancel it. Credentials are used for this
    /// session only — `WEBKIT_CREDENTIAL_PERSISTENCE_FOR_SESSION` — rather
    /// than written to the profile, which would need a deliberate decision
    /// about storing passwords on disk.
    pub fn respond_auth(&self, credentials: Option<(&str, &str)>) {
        let Some((request, _)) = self.prompts.borrow_mut().auth.take() else {
            return;
        };
        self.forget_ping();
        unsafe {
            match credentials {
                Some((user, password)) => {
                    let (u, p) = (cstr(user), cstr(password));
                    let cred = webkit_credential_new(
                        u.as_ptr(),
                        p.as_ptr(),
                        WebKitCredentialPersistence::WEBKIT_CREDENTIAL_PERSISTENCE_FOR_SESSION,
                    );
                    webkit_authentication_request_authenticate(request, cred);
                    webkit_credential_free(cred);
                }
                None => webkit_authentication_request_cancel(request),
            }
            g_object_unref(request as *mut _);
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
            let (x, y) = self.to_logical(x_px, y_px);
            let e = wpe_event_pointer_move_new(
                WPEEventType::WPE_EVENT_POINTER_MOVE,
                view,
                WPEInputSource::WPE_INPUT_SOURCE_MOUSE,
                input::now_ms(),
                self.held_buttons.get(),
                x,
                y,
                0.0,
                0.0,
            );
            self.send(view, e);
        }
    }

    pub fn mouse_button_ui(&self, button: MouseButton, pressed: bool, x_px: f32, y_px: f32) {
        let Some(n) = input::button_number(button) else {
            return;
        };
        if pressed {
            self.ping();
        }
        unsafe {
            let view = self.active_tab().view;
            let time = input::now_ms();
            // WPE tracks double/triple clicks for us; a frozen clock here
            // would make every click read as a repeat.
            let (x, y) = self.to_logical(x_px, y_px);
            let press_count = if pressed {
                wpe_view_compute_press_count(view, x, y, n, time)
            } else {
                0
            };
            // The mask describes the state *after* this event, which is
            // what a DOM `buttons` reads on mousedown and mouseup.
            let bit = input::button_modifier(n);
            let held = if pressed {
                self.held_buttons.get() | bit
            } else {
                self.held_buttons.get() & !bit
            };
            self.held_buttons.set(held);
            let e = wpe_event_pointer_button_new(
                if pressed {
                    WPEEventType::WPE_EVENT_POINTER_DOWN
                } else {
                    WPEEventType::WPE_EVENT_POINTER_UP
                },
                view,
                WPEInputSource::WPE_INPUT_SOURCE_MOUSE,
                time,
                held,
                n,
                x,
                y,
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
        // cce-ui publishes the gesture phase of the wheel event being
        // dispatched: a trackpad's finger lift arrives as a zero delta in
        // FingerEnd, which is WebKit's scroll-stop — the signal its own
        // kinetic scrolling keys off. Finger phases report the touchpad
        // source so the engine treats the deltas as a gesture, not clicks.
        let phase = cce_ui::widget::scroll_motion::current_scroll_phase();
        let (source, is_stop) = match phase {
            cce_ui::widget::ScrollPhase::Wheel => (WPEInputSource::WPE_INPUT_SOURCE_MOUSE, 0),
            cce_ui::widget::ScrollPhase::Finger => (WPEInputSource::WPE_INPUT_SOURCE_TOUCHPAD, 0),
            cce_ui::widget::ScrollPhase::FingerEnd => (WPEInputSource::WPE_INPUT_SOURCE_TOUCHPAD, 1),
        };
        unsafe {
            let view = self.active_tab().view;
            let (x, y) = self.to_logical(x_px, y_px);
            let e = wpe_event_scroll_new(
                view,
                source,
                input::now_ms(),
                0,
                dx_px / self.scale as f64,
                dy_px / self.scale as f64,
                1, // precise deltas: these are pixels, not notches
                is_stop,
                x,
                y,
            );
            self.send(view, e);
        }
    }

    /// Takes cce-ui's `KeyEvent` directly — the keysym mapping lives in
    /// `input`, so the chrome never learns engine vocabulary.
    pub fn key_ui(&self, event: &KeyEvent) {
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

    /// Window focus, from the runner's keyboard enter/leave.
    ///
    /// WebKit needs **two** things before it paints a text caret: the view
    /// focused and the toplevel `ACTIVE`. Keystrokes reach a focused field
    /// with neither, so the only symptom of missing this is a field you can
    /// type into with no caret in it — which shipped, unnoticed, because
    /// nothing called this at all. `document.hasFocus()` reads the same pair;
    /// `examples/wpe_focus.rs` checks it.
    pub fn focus(&mut self, focused: bool) {
        self.window_focused = focused;
        unsafe {
            let state = wpe_toplevel_get_state(self.toplevel);
            let state = if focused {
                state | WPEToplevelState::WPE_TOPLEVEL_STATE_ACTIVE
            } else {
                state & !WPEToplevelState::WPE_TOPLEVEL_STATE_ACTIVE
            };
            wpe_toplevel_state_changed(self.toplevel, state);
            if let Some(tab) = self.tabs.get(self.active) {
                if focused {
                    wpe_view_focus_in(tab.view)
                } else {
                    wpe_view_focus_out(tab.view)
                }
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

    /// Resize, in **physical** pixels — `ServoHost`'s convention, so
    /// `main.rs` passes `content_px()` to either backend unchanged.
    ///
    /// WPE wants the opposite split: a **logical** size plus a scale, and it
    /// produces a buffer of `size * scale`. Handing it physical pixels while
    /// leaving the scale at 1 makes it lay out 2400x1600 *CSS* pixels on a 2x
    /// display — the viewport reads as twice as wide as it is and the whole
    /// page renders at half size. That is the bug this converts away.
    pub fn resize(&mut self, width_px: u32, height_px: u32, scale: f32) {
        self.size_px = (width_px.max(1), height_px.max(1));
        self.scale = scale.max(0.01);
        let (lw, lh) = self.logical_size();
        unsafe {
            wpe_toplevel_scale_changed(self.toplevel, self.scale as f64);
            wpe_toplevel_resized(self.toplevel, lw, lh);
            let view = self.active_tab().view;
            wpe_view_resized(view, lw, lh);
        }
    }

    /// The view size WPE works in: physical divided back out by the scale.
    fn logical_size(&self) -> (i32, i32) {
        (
            ((self.size_px.0 as f32 / self.scale).round() as i32).max(1),
            ((self.size_px.1 as f32 / self.scale).round() as i32).max(1),
        )
    }

    /// Physical pointer coordinates into the view's logical space, for the
    /// same reason as `resize` — a click at the bottom of a 2x window would
    /// otherwise land twice as far down the page as the cursor.
    fn to_logical(&self, x_px: f32, y_px: f32) -> (f64, f64) {
        ((x_px / self.scale) as f64, (y_px / self.scale) as f64)
    }
}

/// Copy an SHM buffer's pixels out for the image registry.
///
/// `WPE_PIXEL_FORMAT_ARGB8888` is B,G,R,A in memory on little-endian, which
/// is handed over **as BGRA** rather than swizzled: the sampler reads either
/// channel order at no cost, and rearranging 35 MB of bytes per frame on the
/// CPU cost 7.4 ms at this display's fullscreen size — most of a frame budget,
/// spent on nothing.
///
/// The destination comes from `cce_ui::vk::recycle_buffer`, so in the steady
/// state this allocates nothing: a fresh frame-sized `Vec` per frame was
/// another 4.5 ms, almost all of it zeroing and page faults rather than
/// copying. What remains is one memcpy per row, and only when the stride
/// forces it — a tight stride is copied whole.
///
/// Called from `pump`, never from the frame callback: a buffer superseded
/// before the next pump is never read at all.
///
/// The stride is not assumed to equal `width * 4`.
/// WebKit features every webview runs with, off by default in this build.
///
/// * `PropagateDamagingInformation` — each frame reports what it repainted,
///   so `pump` copies only that (see `damage.rs`). Without it a page that
///   can scroll costs a whole-window copy sixty times a second while it sits
///   still, for an overlay scrollbar WebKit never stops repainting.
/// * `HiddenPageCSSAnimationSuspension` — a background tab's CSS animations
///   stop. WebKit already stops its `requestAnimationFrame` (checked: 0 a
///   second hidden), but not this, and not its timers. A hidden Muji page
///   measured 67% of a core with it off and 43% on.
const FEATURES: &[(&str, bool)] =
    &[("PropagateDamagingInformation", true), ("HiddenPageCSSAnimationSuspension", true)];

unsafe fn set_features(wv: *mut WebKitWebView) {
    let settings = webkit_web_view_get_settings(wv);
    let list = webkit_settings_get_all_features();
    for &(name, on) in FEATURES {
        let found = (0..webkit_feature_list_get_length(list))
            .map(|i| webkit_feature_list_get(list, i))
            .find(|&f| from_cstr(webkit_feature_get_identifier(f)).as_deref() == Some(name));
        match found {
            Some(f) => webkit_settings_set_feature_enabled(settings, f, on as gboolean),
            // A WebKit upgrade renamed or dropped it: the browser still
            // works, it just loses what the feature bought.
            None => log::warn!("WebKit has no feature {name}; leaving it as it is"),
        }
    }
    webkit_feature_list_unref(list);
}

/// A mapped SHM frame: where its pixels are, and how they are laid out.
/// Borrowed from the buffer, so it must not outlive the buffer's release.
struct ShmFrame {
    data: *const u8,
    stride: usize,
    width: u32,
    height: u32,
}

impl ShmFrame {
    /// The buffer's pixels, if it is an SHM buffer with all its rows there.
    unsafe fn of(buffer: *mut WPEBuffer) -> Option<Self> {
        if g_type_check_instance_is_a(buffer as *mut GTypeInstance, wpe_buffer_shm_get_type()) == 0 {
            return None;
        }
        let shm = buffer as *mut WPEBufferSHM;
        let (width, height) = (
            wpe_buffer_get_width(buffer) as u32,
            wpe_buffer_get_height(buffer) as u32,
        );
        let mut len: u64 = 0;
        let data = g_bytes_get_data(wpe_buffer_shm_get_data(shm), &mut len as *mut u64) as *const u8;
        if data.is_null() || width == 0 || height == 0 {
            return None;
        }
        let stride = wpe_buffer_shm_get_stride(shm) as usize;
        if (len as usize) < stride * (height as usize - 1) + width as usize * 4 {
            return None;
        }
        Some(Self { data, stride, width, height })
    }

    /// The whole frame, tightly packed, in a recycled buffer.
    unsafe fn copy_all(&self) -> Vec<u8> {
        let row = self.width as usize * 4;
        let need = row * self.height as usize;
        let mut out = cce_ui::vk::recycle_buffer(need);
        if self.stride == row {
            std::ptr::copy_nonoverlapping(self.data, out.as_mut_ptr(), need);
        } else {
            for y in 0..self.height as usize {
                std::ptr::copy_nonoverlapping(
                    self.data.add(y * self.stride),
                    out.as_mut_ptr().add(y * row),
                    row,
                );
            }
        }
        out
    }
}

/// `CCE_BROWSER_DAMAGE_CHECK`: patch the tab's CPU copy with the regions just
/// read, as its image is being patched, and compare the result with the
/// engine's whole frame. A mismatch means the damage left something out and
/// the page on screen is stale there; the copy is then resynced so one miss
/// is not reported on every frame after it.
unsafe fn check_regions(
    mirror: &mut [u8],
    shm: &ShmFrame,
    packed: &[u8],
    regions: &[super::damage::Rect],
    tab: usize,
) {
    let row = shm.width as usize * 4;
    let mut at = 0usize;
    for &(x, y, w, h) in regions {
        let len = w as usize * 4;
        for r in 0..h as usize {
            let dst = (y as usize + r) * row + x as usize * 4;
            mirror[dst..dst + len].copy_from_slice(&packed[at..at + len]);
            at += len;
        }
    }
    let truth = shm.copy_all();
    let wrong = mirror
        .chunks_exact(4)
        .zip(truth.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    if wrong > 0 {
        log::warn!(
            "damage check: tab {tab} is stale in {wrong} pixels after reading {} region(s) {regions:?}",
            regions.len()
        );
        mirror.copy_from_slice(&truth);
    } else {
        log::info!("damage check: tab {tab} exact after {} region(s)", regions.len());
    }
}

unsafe fn from_cstr(p: *const c_char) -> Option<String> {
    (!p.is_null())
        .then(|| std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
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

/// Serves a `cce:` page. Runs on the main thread, unlike the Servo handler
/// which runs on fetch threads — the `Arc<Mutex<_>>` stores are shared with
/// that backend and stay as they are.
unsafe extern "C" fn on_cce_request(request: *mut WebKitURISchemeRequest, data: gpointer) {
    let protocol = &*(data as *const crate::pages::CceProtocol);
    let uri = from_cstr(webkit_uri_scheme_request_get_uri(request)).unwrap_or_default();
    match protocol.route(&uri) {
        Some(html) => {
            let len = html.len() as i64;
            let bytes = html.into_bytes().into_boxed_slice();
            let ptr = Box::into_raw(bytes) as *mut c_void;
            // The stream owns the buffer and frees it with g_free, so the box
            // is deliberately leaked into it rather than dropped here.
            let stream = g_memory_input_stream_new_from_data(ptr, len, Some(free_boxed));
            let ctype = cstr("text/html; charset=utf-8");
            webkit_uri_scheme_request_finish(request, stream, len, ctype.as_ptr());
            g_object_unref(stream as *mut _);
        }
        None => {
            let msg = cstr(&format!("no such cce: page: {uri}"));
            let err = g_error_new_literal(1, 0, msg.as_ptr());
            webkit_uri_scheme_request_finish_error(request, err);
            g_error_free(err);
        }
    }
}

unsafe extern "C" fn free_boxed(p: gpointer) {
    drop(Box::from_raw(p as *mut u8));
}

/// Shared with WebKit's download signals for the life of the process.
struct DownloadCtx {
    downloads: std::sync::Arc<crate::downloads::Downloads>,
    started: Rc<Cell<bool>>,
}

/// Per-download state, owned by that download's own signal closures.
struct OneDownload {
    ctx: Rc<DownloadCtx>,
    id: Cell<u64>,
}

unsafe extern "C" fn on_download_started(
    _session: *mut GObject,
    download: *mut WebKitDownload,
    data: gpointer,
) {
    let ctx = &*(data as *const DownloadCtx);
    let one = Rc::new(OneDownload {
        ctx: Rc::new(DownloadCtx {
            downloads: ctx.downloads.clone(),
            started: ctx.started.clone(),
        }),
        id: Cell::new(u64::MAX),
    });
    ctx.started.set(true);

    for (sig, cb) in [
        (
            "decide-destination",
            on_decide_destination as *const () as usize,
        ),
        ("received-data", on_received_data as *const () as usize),
        ("finished", on_finished as *const () as usize),
        ("failed", on_failed as *const () as usize),
    ] {
        let name = cstr(sig);
        g_signal_connect_data(
            download as *mut _,
            name.as_ptr(),
            Some(std::mem::transmute::<usize, unsafe extern "C" fn()>(cb)),
            Rc::into_raw(one.clone()) as gpointer,
            Some(drop_one_download),
            0,
        );
    }
}

unsafe extern "C" fn drop_one_download(data: gpointer, _c: *mut GClosure) {
    drop(Rc::from_raw(data as *const OneDownload));
}

/// WebKit asks where to put it, passing the name the *server* suggested —
/// `Content-Disposition` when present, which the extension sniff could never
/// see. Returning TRUE means we handled it.
unsafe extern "C" fn on_decide_destination(
    download: *mut WebKitDownload,
    suggested: *const c_char,
    data: gpointer,
) -> gboolean {
    let one = &*(data as *const OneDownload);
    let name = from_cstr(suggested).unwrap_or_else(|| "download".into());
    let path = crate::downloads::Downloads::destination_for(&name);

    let total = {
        let response = webkit_download_get_response(download);
        (!response.is_null())
            .then(|| webkit_uri_response_get_content_length(response))
            .filter(|n| *n > 0)
    };
    let uri = from_cstr(webkit_download_get_destination(download)).unwrap_or_default();
    one.id
        .set(one.ctx.downloads.adopt(uri, path.clone(), total));

    let dest = cstr(&path.to_string_lossy());
    webkit_download_set_destination(download, dest.as_ptr());
    1
}

unsafe extern "C" fn on_received_data(
    download: *mut WebKitDownload,
    _len: u64,
    data: gpointer,
) {
    let one = &*(data as *const OneDownload);
    if one.id.get() != u64::MAX {
        one.ctx.downloads.set_progress(
            one.id.get(),
            webkit_download_get_received_data_length(download),
            None,
        );
    }
}

unsafe extern "C" fn on_finished(_d: *mut WebKitDownload, data: gpointer) {
    let one = &*(data as *const OneDownload);
    if one.id.get() != u64::MAX {
        one.ctx.downloads.set_finished(one.id.get(), Ok(()));
    }
}

unsafe extern "C" fn on_failed(_d: *mut WebKitDownload, error: *mut GError, data: gpointer) {
    let one = &*(data as *const OneDownload);
    let msg = (!error.is_null())
        .then(|| from_cstr((*error).message))
        .flatten()
        .unwrap_or_else(|| "download failed".into());
    if one.id.get() != u64::MAX {
        one.ctx.downloads.set_finished(one.id.get(), Err(msg));
    }
}

/// What a page is currently blocked on. At most one of each: WebKit will not
/// raise a second dialog on the same view until the first is answered.
#[derive(Default)]
pub(super) struct Prompts {
    dialog: Option<(*mut WebKitScriptDialog, PendingDialog)>,
    auth: Option<(*mut WebKitAuthenticationRequest, PendingAuth)>,
    /// The page asked for a context menu; the chrome draws its own.
    context_menu: Option<ContextMenuInfo>,
    /// The `<select>` list waiting on an answer, reffed until it gets one or
    /// the page closes it. Never more than one: a newer one closes the last.
    option_menu: Option<*mut WebKitOptionMenu>,
    /// What that list holds, until the chrome takes it to draw.
    option_menu_new: Option<OptionMenuInfo>,
    /// Links middle-clicked in a page, oldest first, for the chrome to open
    /// as background tabs.
    background_opens: Vec<Url>,
    /// Login fields the account watcher reported, oldest first. A queue and
    /// not a slot: a blur followed by a focus is two different states, and
    /// collapsing them would leave the list open over the wrong field.
    form_events: std::collections::VecDeque<crate::wpe::formwatch::FormEvent>,
    /// Open fill asks, oldest first, by the asking document's token. Each
    /// is a reply a watcher's promise is waiting on, held with a ref, and
    /// every one is answered exactly once: with a credential, or with
    /// nothing when it is displaced or dropped.
    fill_asks: std::collections::VecDeque<(String, *mut WebKitScriptMessageReply)>,
    /// The vi focus watcher's latest word: whether the focused element takes
    /// text. Only the newest matters, so a slot, not a queue.
    vi_focus: Option<bool>,
    /// Answers to [`WebKitHost::vi_eval`], by the caller's tag.
    vi_results: std::collections::VecDeque<(u32, String)>,
    /// The last find-in-page outcome: the match count, 0 for none.
    find_result: Option<u32>,
}

/// What was under the pointer when the page asked for a context menu, read
/// off WebKit's hit test. The chrome builds its menu from this.
#[derive(Debug, Clone, Default)]
pub struct ContextMenuInfo {
    /// `(uri, label)` when the hit was a link.
    pub link: Option<(String, Option<String>)>,
    pub image_uri: Option<String>,
    pub is_selection: bool,
    pub is_editable: bool,
}

/// A `<select>`'s option list, for the chrome to draw at the select.
#[derive(Debug, Clone)]
pub struct OptionMenuInfo {
    pub items: Vec<OptionItem>,
    /// The select's box — `(x, y, width, height)` in the view's logical
    /// pixels, which are the chrome's.
    pub anchor: (f32, f32, f32, f32),
}

/// One row of a select's list: an `<option>`, or an `<optgroup>`'s label.
#[derive(Debug, Clone)]
pub struct OptionItem {
    pub label: String,
    /// An `<optgroup>` heading: drawn, never picked.
    pub group_label: bool,
    /// An option inside an `<optgroup>`, drawn indented under its heading.
    pub group_child: bool,
    pub enabled: bool,
    /// The select's current value.
    pub selected: bool,
}

/// A page's `alert` / `confirm` / `prompt`, waiting on the chrome.
#[derive(Debug, Clone)]
pub struct PendingDialog {
    pub message: String,
    /// `Some` for `prompt`, carrying its default text; `None` otherwise.
    pub prompt_default: Option<String>,
    /// `confirm` and `beforeunload` offer a choice; `alert` only acknowledges.
    pub has_cancel: bool,
}

/// An HTTP auth challenge, waiting on the chrome.
#[derive(Debug, Clone)]
pub struct PendingAuth {
    pub host: String,
    pub realm: String,
    /// Set when the previous credentials were rejected — worth telling the
    /// user, since the field otherwise looks identical to the first attempt.
    pub retry: bool,
}

unsafe fn connect_raw(
    wv: *mut WebKitWebView,
    signal: &str,
    cb: usize,
    prompts: &Rc<RefCell<Prompts>>,
) {
    let name = cstr(signal);
    g_signal_connect_data(
        wv as *mut _,
        name.as_ptr(),
        Some(std::mem::transmute::<usize, unsafe extern "C" fn()>(cb)),
        Rc::into_raw(prompts.clone()) as gpointer,
        Some(drop_prompts_ref),
        0,
    );
}

/// A report from the vi focus watcher. Anything else is dropped.
unsafe extern "C" fn on_vi_message(
    _ucm: *mut WebKitUserContentManager,
    value: *mut JSCValue,
    data: gpointer,
) {
    let prompts = &*(data as *const RefCell<Prompts>);
    let raw = jsc_value_to_string(value);
    let Some(json) = from_cstr(raw) else { return };
    g_free(raw as *mut _);
    if let Some(editable) = crate::vi::parse_focus(&json) {
        prompts.borrow_mut().vi_focus = Some(editable);
    }
}

/// A [`WebKitHost::vi_eval`] script finished: queue what it returned.
unsafe extern "C" fn on_vi_eval(source: *mut GObject, res: *mut GAsyncResult, data: gpointer) {
    let ctx = Box::from_raw(data as *mut (Rc<RefCell<Prompts>>, u32));
    let mut err: *mut GError = std::ptr::null_mut();
    let v = webkit_web_view_evaluate_javascript_finish(source as *mut WebKitWebView, res, &mut err);
    let mut text = String::new();
    if !v.is_null() {
        if jsc_value_is_string(v) != 0 {
            let raw = jsc_value_to_string(v);
            text = from_cstr(raw).unwrap_or_default();
            g_free(raw as *mut _);
        }
        g_object_unref(v as *mut _);
    }
    if !err.is_null() {
        g_error_free(err);
    }
    ctx.0.borrow_mut().vi_results.push_back((ctx.1, text));
}

unsafe extern "C" fn on_found_text(_fc: *mut WebKitFindController, count: guint, data: gpointer) {
    let prompts = &*(data as *const RefCell<Prompts>);
    prompts.borrow_mut().find_result = Some(count);
}

unsafe extern "C" fn on_failed_to_find(_fc: *mut WebKitFindController, data: gpointer) {
    let prompts = &*(data as *const RefCell<Prompts>);
    prompts.borrow_mut().find_result = Some(0);
}

/// A message from the account watcher. Anything that does not parse as one of
/// its events is dropped without comment — this is a channel the chrome acts
/// on, so it accepts only what it recognizes.
unsafe extern "C" fn on_account_message(
    _ucm: *mut WebKitUserContentManager,
    value: *mut JSCValue,
    data: gpointer,
) {
    let prompts = &*(data as *const RefCell<Prompts>);
    let raw = jsc_value_to_string(value);
    let Some(json) = from_cstr(raw) else { return };
    g_free(raw as *mut _);
    if let Some(event) = super::formwatch::parse_event(&json) {
        let mut p = prompts.borrow_mut();
        // A page that spins on scroll must not grow this without bound; the
        // chrome only ever cares about the last few.
        if p.form_events.len() > 8 {
            p.form_events.pop_front();
        }
        p.form_events.push_back(event);
    }
}

/// A login field asking to be filled. The reply is held until a pick
/// answers it, or a newer ask displaces it. Returning TRUE says it will be
/// answered — later, which is the point.
unsafe extern "C" fn on_fill_ask(
    _ucm: *mut WebKitUserContentManager,
    value: *mut JSCValue,
    reply: *mut WebKitScriptMessageReply,
    data: gpointer,
) -> gboolean {
    let prompts = &*(data as *const RefCell<Prompts>);
    let raw = jsc_value_to_string(value);
    let token = from_cstr(raw);
    g_free(raw as *mut _);
    // The watcher's tokens are 24 hex digits; anything else is not one.
    let Some(token) =
        token.filter(|t| t.len() == 24 && t.bytes().all(|b| b.is_ascii_hexdigit()))
    else {
        webkit_script_message_reply_ref(reply);
        answer_fill(reply, None);
        return 1;
    };
    webkit_script_message_reply_ref(reply);
    let displaced: Vec<_> = {
        let mut p = prompts.borrow_mut();
        let mut out = Vec::new();
        p.fill_asks.retain(|(t, r)| {
            let same = *t == token;
            if same {
                out.push(*r);
            }
            !same
        });
        p.fill_asks.push_back((token, reply));
        // Only the focused field's ask matters; a few spare cover a list
        // still open while focus wanders between frames.
        while p.fill_asks.len() > 4 {
            if let Some((_, r)) = p.fill_asks.pop_front() {
                out.push(r);
            }
        }
        out
    };
    for r in displaced {
        answer_fill(r, None);
    }
    1
}

/// Answer a fill ask — with the credential's JSON, or with null — and let
/// go of it.
unsafe fn answer_fill(reply: *mut WebKitScriptMessageReply, value: Option<&str>) {
    thread_local! {
        /// A context to build reply values in. Any will do: the value is
        /// serialized across to the web process, not run here.
        static JSC: *mut JSCContext = unsafe { jsc_context_new() };
    }
    JSC.with(|ctx| {
        let v = match value {
            // JSON has no raw NUL — serde escapes it — so this cannot fail on
            // a credential.
            Some(s) => {
                let c = cstr(s);
                jsc_value_new_string(*ctx, c.as_ptr())
            }
            None => jsc_value_new_null(*ctx),
        };
        webkit_script_message_reply_return_value(reply, v);
        g_object_unref(v as *mut _);
    });
    webkit_script_message_reply_unref(reply);
}

unsafe extern "C" fn drop_prompts_ref(data: gpointer, _c: *mut GClosure) {
    drop(Rc::from_raw(data as *const RefCell<Prompts>));
}

/// Returning TRUE means *we* will answer. The dialog is reffed and held; the
/// page stays blocked until `respond_dialog` closes it.
unsafe extern "C" fn on_script_dialog(
    _wv: *mut WebKitWebView,
    dialog: *mut WebKitScriptDialog,
    data: gpointer,
) -> gboolean {
    let prompts = &*(data as *const RefCell<Prompts>);
    let kind = webkit_script_dialog_get_dialog_type(dialog);
    let message = from_cstr(webkit_script_dialog_get_message(dialog)).unwrap_or_default();
    let is_prompt = kind == WebKitScriptDialogType::WEBKIT_SCRIPT_DIALOG_PROMPT;
    let pending = PendingDialog {
        message,
        prompt_default: is_prompt
            .then(|| from_cstr(webkit_script_dialog_prompt_get_default_text(dialog)))
            .flatten()
            .or_else(|| is_prompt.then(String::new)),
        has_cancel: kind != WebKitScriptDialogType::WEBKIT_SCRIPT_DIALOG_ALERT,
    };
    webkit_script_dialog_ref(dialog);
    prompts.borrow_mut().dialog = Some((dialog, pending));
    1
}

/// Same contract: TRUE means we answer, and the request is reffed until we do.
unsafe extern "C" fn on_authenticate(
    _wv: *mut WebKitWebView,
    request: *mut WebKitAuthenticationRequest,
    data: gpointer,
) -> gboolean {
    let prompts = &*(data as *const RefCell<Prompts>);
    let pending = PendingAuth {
        host: from_cstr(webkit_authentication_request_get_host(request)).unwrap_or_default(),
        realm: from_cstr(webkit_authentication_request_get_realm(request)).unwrap_or_default(),
        retry: webkit_authentication_request_is_retry(request) != 0,
    };
    g_object_ref(request as *mut _);
    prompts.borrow_mut().auth = Some((request, pending));
    1
}

/// A navigation is about to happen. A middle-click on a link — which WebKit
/// reports as an ordinary navigation (or a new-window one, for a
/// `target=_blank` link) carrying the button — is diverted to a background
/// tab: the decision is ignored here and the URL queued for the chrome.
/// Everything else gets WebKit's default.
unsafe extern "C" fn on_decide_policy(
    _wv: *mut WebKitWebView,
    decision: *mut WebKitPolicyDecision,
    kind: WebKitPolicyDecisionType::Type,
    data: gpointer,
) -> gboolean {
    if kind != WebKitPolicyDecisionType::WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION
        && kind != WebKitPolicyDecisionType::WEBKIT_POLICY_DECISION_TYPE_NEW_WINDOW_ACTION
    {
        return 0;
    }
    let action = webkit_navigation_policy_decision_get_navigation_action(
        decision as *mut WebKitNavigationPolicyDecision,
    );
    if action.is_null()
        || webkit_navigation_action_get_mouse_button(action) != 2
        || webkit_navigation_action_get_navigation_type(action)
            != WebKitNavigationType::WEBKIT_NAVIGATION_TYPE_LINK_CLICKED
    {
        return 0;
    }
    let request = webkit_navigation_action_get_request(action);
    let Some(url) = (!request.is_null())
        .then(|| from_cstr(webkit_uri_request_get_uri(request)))
        .flatten()
        .and_then(|u| Url::parse(&u).ok())
    else {
        return 0;
    };
    webkit_policy_decision_ignore(decision);
    let prompts = &*(data as *const RefCell<Prompts>);
    prompts.borrow_mut().background_opens.push(url);
    1
}

/// The page asked for a context menu. Stash what the hit test says was under
/// the pointer and claim presentation; the chrome draws the menu at the
/// pointer position it already tracks (the hit test carries no coordinates).
unsafe extern "C" fn on_context_menu(
    _wv: *mut WebKitWebView,
    _menu: *mut WebKitContextMenu,
    hit: *mut WebKitHitTestResult,
    data: gpointer,
) -> gboolean {
    let prompts = &*(data as *const RefCell<Prompts>);
    let mut info = ContextMenuInfo::default();
    if !hit.is_null() {
        if webkit_hit_test_result_context_is_link(hit) != 0 {
            if let Some(uri) = from_cstr(webkit_hit_test_result_get_link_uri(hit)) {
                info.link = Some((uri, from_cstr(webkit_hit_test_result_get_link_label(hit))));
            }
        }
        if webkit_hit_test_result_context_is_image(hit) != 0 {
            info.image_uri = from_cstr(webkit_hit_test_result_get_image_uri(hit));
        }
        info.is_selection = webkit_hit_test_result_context_is_selection(hit) != 0;
        info.is_editable = webkit_hit_test_result_context_is_editable(hit) != 0;
    }
    prompts.borrow_mut().context_menu = Some(info);
    1
}

/// A `<select>` asked to open. Read its options and where it is, hold the
/// menu, and claim it: the chrome draws the list and answers with
/// `pick_option` or `close_option_menu`. Returning FALSE would leave it to
/// WebKit's default, which on WPE is nothing at all.
unsafe extern "C" fn on_show_option_menu(
    _wv: *mut WebKitWebView,
    menu: *mut WebKitOptionMenu,
    rect: *mut WebKitRectangle,
    data: gpointer,
) -> gboolean {
    let prompts = &*(data as *const RefCell<Prompts>);
    let items = (0..webkit_option_menu_get_n_items(menu))
        .map(|i| {
            let item = webkit_option_menu_get_item(menu, i);
            OptionItem {
                label: from_cstr(webkit_option_menu_item_get_label(item)).unwrap_or_default(),
                group_label: webkit_option_menu_item_is_group_label(item) != 0,
                group_child: webkit_option_menu_item_is_group_child(item) != 0,
                enabled: webkit_option_menu_item_is_enabled(item) != 0,
                selected: webkit_option_menu_item_is_selected(item) != 0,
            }
        })
        .collect();
    let anchor = if rect.is_null() {
        (0.0, 0.0, 0.0, 0.0)
    } else {
        let r = &*rect;
        (r.x as f32, r.y as f32, r.width as f32, r.height as f32)
    };
    g_object_ref(menu as *mut _);
    // The page can close the list itself (the select goes away, the page
    // navigates); hearing that is how the chrome's copy comes down too.
    let name = cstr("close");
    g_signal_connect_data(
        menu as *mut _,
        name.as_ptr(),
        Some(std::mem::transmute::<usize, unsafe extern "C" fn()>(
            on_option_menu_close as *const () as usize,
        )),
        // The close handler's own reference, dropped with the connection.
        {
            Rc::increment_strong_count(data as *const RefCell<Prompts>);
            data
        },
        Some(drop_prompts_ref),
        0,
    );
    let previous = {
        let mut p = prompts.borrow_mut();
        p.option_menu_new = Some(OptionMenuInfo { items, anchor });
        p.option_menu.replace(menu)
    };
    // Closed outside the borrow: its close handler borrows the prompts.
    if let Some(old) = previous {
        webkit_option_menu_close(old);
        g_object_unref(old as *mut _);
    }
    1
}

/// A held list was closed — by the page, or by our own `close`. Only the
/// first case finds it still held; ours took it out before closing. WebKit
/// keeps its own reference across the emission, so dropping ours here is
/// safe.
unsafe extern "C" fn on_option_menu_close(menu: *mut WebKitOptionMenu, data: gpointer) {
    let prompts = &*(data as *const RefCell<Prompts>);
    let mut p = prompts.borrow_mut();
    if p.option_menu == Some(menu) {
        p.option_menu = None;
        p.option_menu_new = None;
        drop(p);
        g_object_unref(menu as *mut _);
    }
}
