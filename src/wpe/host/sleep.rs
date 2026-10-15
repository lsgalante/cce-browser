//! Sleeping tabs: a tab left out of sight long enough gives its web process
//! back, and showing it again loads it again — Chrome's tab discarding,
//! Edge's sleeping tabs. A tab restored at launch starts out asleep too, so a
//! saved session costs one page load rather than one per tab.
//!
//! **Asleep means the web process is terminated**, not the webview dropped.
//! WebKit keeps a tab's back/forward list in the UI process, so the webview
//! that survives its process still knows its history, and `reload` on it
//! launches a fresh process for the current entry: Back still works after a
//! wake. Dropping the webview would have lost that, and freed far less —
//! WebKit parks the process of a closed page in its WebProcessCache (capacity
//! 32 here, for up to half an hour), and a closed Muji tab's parked process
//! still held 479 of its 814 MB. A terminated process holds nothing.
//!
//! What is never put to sleep: the shown tab; one loading, hung, playing
//! audio or capturing camera, microphone or screen; one whose page came from
//! a form post (a reload would post again); one with unsaved input (the
//! probe below); and one tied to another tab by `window.open` — a popup
//! shares its opener's process, so terminating one would kill the other.
//! Such a group sleeps only together, once every tab in it may.
//!
//! The sweep runs on a GLib timer, since an idle browser turns no loop.
//!
//! A tab being **closed** has its process stopped the same way
//! (`end_on_close`), once its page has unloaded. What that cannot reach is
//! a process holding one of the tab's earlier pages for Back: WebKit parks
//! those in its cache when the webview goes, and the API has no handle on
//! them.

use super::*;

/// How often the tabs are looked over, at most.
const SLEEP_CHECK: std::time::Duration = std::time::Duration::from_secs(60);

/// How long a probe may go unanswered before it is asked again. A page that
/// does not answer is busy or hung, and is left awake meanwhile.
const PROBE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

// Not among the generated bindings; GLib itself is linked already.
extern "C" {
    fn g_source_remove(tag: u32) -> gboolean;
}

/// The script world the probe runs in, so the page never sees it.
const PROBE_WORLD: &str = "cce-sleep";

/// Whether the page holds anything a reload would lose: a field whose value
/// differs from the one the page was served with, a changed choice, or an
/// editable element with the focus (a half-written message). Same-origin
/// frames are looked into; a cross-origin one cannot be, and is taken as
/// clean. Erring towards "edited" only costs a tab its sleep.
const PROBE_JS: &str = r#"(() => {
  const edited = (doc) => {
    for (const e of doc.querySelectorAll('input, textarea, select')) {
      if (e.disabled || e.type === 'hidden') continue;
      if (e.tagName === 'SELECT') {
        // With no option marked `selected`, a single select starts on its
        // first enabled option — selected, but not defaultSelected.
        const opts = [...e.options];
        if (e.multiple || opts.some(o => o.defaultSelected)) {
          if (opts.some(o => o.selected !== o.defaultSelected)) return true;
        } else if (e.selectedIndex !== opts.findIndex(o => !o.disabled)) {
          return true;
        }
      } else if (e.type === 'checkbox' || e.type === 'radio') {
        if (e.checked !== e.defaultChecked) return true;
      } else if (e.value !== e.defaultValue) {
        return true;
      }
    }
    const a = doc.activeElement;
    if (a && a.isContentEditable) return true;
    for (const f of doc.querySelectorAll('iframe, frame')) {
      try { if (f.contentDocument && edited(f.contentDocument)) return true; } catch (_) {}
    }
    return false;
  };
  return edited(document);
})()"#;

/// Where a tab's check for unsaved input stands.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Probe {
    #[default]
    None,
    Asked(std::time::Instant),
    Clean,
    Edited,
}

/// A closed tab's webview, held until its page has unloaded: `about:blank`
/// was loaded into it, so the page had its `pagehide` and `unload`.
pub struct Closing {
    webview: *mut WebKitWebView,
    since: std::time::Instant,
}

impl Closing {
    /// The closing webview's view.
    pub(super) fn view(&self) -> *mut WPEView {
        unsafe { webkit_web_view_get_wpe_view(self.webview) }
    }
}

/// How long a closing page may take over its unload before its process is
/// stopped anyway.
const CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

impl Tab {
    /// Out of sight with no web process; showing it loads it again.
    pub fn asleep(&self) -> bool {
        self.state.asleep.get()
    }
}

impl WebKitHost {
    /// Put tabs to sleep once they have been out of sight for `after`;
    /// `None` never does. Tabs already asleep stay so until shown.
    pub fn set_sleep_after(&mut self, after: Option<std::time::Duration>) {
        self.sleep_after = after;
        unsafe {
            match (after, self.sleep_timer) {
                (Some(after), None) => {
                    let every = after.min(SLEEP_CHECK).max(std::time::Duration::from_millis(100));
                    self.sleep_timer =
                        Some(g_timeout_add(every.as_millis() as u32, Some(on_sleep_tick), std::ptr::null_mut()));
                }
                (None, Some(id)) => {
                    g_source_remove(id);
                    self.sleep_timer = None;
                }
                _ => {}
            }
        }
    }

    /// A tab that comes back asleep from a saved session: built, in the
    /// strip, but loading nothing. WebKit launches a web process only for a
    /// webview's first load, so until it is shown it costs none.
    pub(super) fn add_asleep_tab(&mut self, url: Url, title: Option<String>) {
        let state = Rc::new(TabState::default());
        let (webview, view) = unsafe { new_webview(&self.builder, std::ptr::null_mut(), &state) };
        unsafe {
            wpe_view_unmap(view);
            wpe_view_set_visible(view, 0);
        }
        state.asleep.set(true);
        state.hidden_since.set(Some(std::time::Instant::now()));
        *state.url.borrow_mut() = Some(url.clone());
        *state.title.borrow_mut() = title.clone();
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.tabs.push(Tab {
            id,
            webview,
            view,
            state,
            title,
            url: Some(url),
            loading: false,
            image: None,
            image_gpu: false,
            mirror: None,
            opener: None,
        });
    }

    /// Wake tab `index` if it is asleep: reload its current history entry
    /// in a new process, or — restored, never loaded — load its URL.
    /// Returns whether it was asleep.
    pub(super) fn wake(&mut self, index: usize) -> bool {
        let Some(t) = self.tabs.get_mut(index) else { return false };
        if !t.state.asleep.replace(false) {
            return false;
        }
        t.state.probe.set(Probe::None);
        unsafe {
            let list = webkit_web_view_get_back_forward_list(t.webview);
            let current = if list.is_null() {
                std::ptr::null_mut()
            } else {
                webkit_back_forward_list_get_current_item(list)
            };
            if !current.is_null() {
                webkit_web_view_reload(t.webview);
            } else if let Some(u) = &t.url {
                let c = cstr(u.as_str());
                webkit_web_view_load_uri(t.webview, c.as_ptr());
            }
        }
        log::info!("waking {}", t.url.as_ref().map_or("<no url>", |u| u.as_str()));
        t.state.loading.set(true);
        t.state.dirty.set(true);
        t.loading = true;
        true
    }

    /// An agent wants tab `index`: it counts as used, so its sleep clock
    /// starts over, and it is woken if asleep — by loading it again, or with
    /// `load` false only marked awake, for a caller about to load something
    /// itself (`navigate`). Returns whether it was asleep.
    pub fn agent_wake(&mut self, index: usize, load: bool) -> bool {
        let Some(t) = self.tabs.get(index) else { return false };
        if index != self.active {
            t.state.hidden_since.set(Some(std::time::Instant::now()));
        }
        if load {
            return self.wake(index);
        }
        t.state.probe.set(Probe::None);
        t.state.asleep.replace(false)
    }

    /// Tab `index` is closing: unless it shares its process with another
    /// tab (`opener_groups`), take its process down with it — after its page
    /// has unloaded. Dropped as it was, the webview's process went to
    /// WebKit's cache holding most of what the page used; stopped at once,
    /// the page never heard it was closing (its `pagehide` beacon was lost).
    /// So `about:blank` goes into it first — the same site, no process swap
    /// — and `end_closed` stops the process once that has loaded.
    pub(super) fn end_on_close(&mut self, index: usize) {
        let alone = self.opener_groups().iter().any(|g| g.len() == 1 && g[0] == index);
        let t = &self.tabs[index];
        // Asleep already has no process to stop.
        if !alone || t.state.asleep.get() {
            return;
        }
        // Its termination is not a crash, should anything still look.
        t.state.asleep.set(true);
        unsafe {
            wpe_view_unmap(t.view);
            wpe_view_set_visible(t.view, 0);
            g_object_ref(t.webview as *mut _);
            let blank = cstr("about:blank");
            webkit_web_view_load_uri(t.webview, blank.as_ptr());
        }
        self.closing.push(Closing { webview: t.webview, since: std::time::Instant::now() });
    }

    /// Stop the processes of closed tabs whose pages have unloaded, and let
    /// their webviews go.
    pub(super) fn end_closed(&mut self) {
        if self.closing.is_empty() {
            return;
        }
        let (ending, waiting): (Vec<Closing>, Vec<Closing>) = std::mem::take(&mut self.closing)
            .into_iter()
            .partition(|c| unsafe {
                // Unloaded once `about:blank` has committed and finished;
                // "not loading" alone would also be true before it had begun.
                let blank = from_cstr(webkit_web_view_get_uri(c.webview)).as_deref() == Some("about:blank");
                let unloaded = blank && webkit_web_view_is_loading(c.webview) == 0;
                unloaded || c.since.elapsed() >= CLOSE_GRACE
            });
        self.closing = waiting;
        for c in ending {
            let view = c.view();
            // A closing view can still hand over a frame (one rendered before
            // it was unmapped), which then sits held, unread. Give it back
            // while the view exists: held past the view, it was handed back
            // to a destroyed one when the next frame superseded it — a
            // crash in `wpe_view_buffer_released`, 2026-10-15.
            if self.pending.borrow().held.is_some_and(|(v, _)| v == view) {
                self.release_held();
            }
            self.pending.borrow_mut().damage.remove(&(view as usize));
            unsafe {
                webkit_web_view_terminate_web_process(c.webview);
                g_object_unref(c.webview as *mut _);
            }
        }
    }

    /// Look the tabs over and put to sleep what may sleep. Called from every
    /// `pump`; does the work once per sweep interval, or when a probe has
    /// answered.
    pub(super) fn sleep_idle_tabs(&mut self) {
        let Some(after) = self.sleep_after else { return };
        let now = std::time::Instant::now();
        let every = after.min(SLEEP_CHECK);
        let sweep = self.last_sleep_sweep.is_none_or(|t| now.duration_since(t) >= every);
        let answered = self
            .tabs
            .iter()
            .any(|t| matches!(t.state.probe.get(), Probe::Clean | Probe::Edited));
        if !sweep && !answered {
            return;
        }
        if sweep {
            self.last_sleep_sweep = Some(now);
        }
        let ready: Vec<bool> = (0..self.tabs.len()).map(|i| self.may_sleep(i, after, now)).collect();
        for group in self.opener_groups() {
            let states: Vec<Rc<TabState>> = group.iter().map(|&i| self.tabs[i].state.clone()).collect();
            if !group.iter().all(|&i| ready[i]) {
                for s in &states {
                    s.probe.set(Probe::None);
                }
                continue;
            }
            if states.iter().any(|s| s.probe.get() == Probe::Edited) {
                // Something would be lost: give it another full stretch
                // before asking again.
                for s in &states {
                    s.probe.set(Probe::None);
                    s.hidden_since.set(Some(now));
                }
                continue;
            }
            if states.iter().all(|s| s.probe.get() == Probe::Clean) {
                for &i in &group {
                    self.put_to_sleep(i);
                }
                continue;
            }
            if sweep {
                for &i in &group {
                    match self.tabs[i].state.probe.get() {
                        Probe::None => self.probe(i),
                        Probe::Asked(at) if now.duration_since(at) >= PROBE_WAIT => self.probe(i),
                        _ => {}
                    }
                }
            }
        }
    }

    /// Everything about tab `i` alone that allows sleep (the group rule and
    /// the probe come on top).
    fn may_sleep(&self, i: usize, after: std::time::Duration, now: std::time::Instant) -> bool {
        let t = &self.tabs[i];
        let s = &t.state;
        {
            // A dialog or sign-in is up: whichever page it belongs to is
            // blocked on the person.
            let p = self.prompts.borrow();
            if p.dialog.is_some() || p.auth.is_some() {
                return false;
            }
        }
        if i == self.active
            || s.asleep.get()
            || s.loading.get()
            || s.posted.get()
            || s.unresponsive.get()
            || s.ping_since.get().is_some()
            || !s.hidden_since.get().is_some_and(|h| now.duration_since(h) >= after)
        {
            return false;
        }
        use WebKitMediaCaptureState::WEBKIT_MEDIA_CAPTURE_STATE_NONE as NONE;
        unsafe {
            webkit_web_view_is_playing_audio(t.webview) == 0
                && webkit_web_view_get_camera_capture_state(t.webview) == NONE
                && webkit_web_view_get_microphone_capture_state(t.webview) == NONE
                && webkit_web_view_get_display_capture_state(t.webview) == NONE
        }
    }

    /// The tabs as groups tied by `window.open` (`Tab::opener`, either way
    /// round), by index. Most tabs are a group of one.
    fn opener_groups(&self) -> Vec<Vec<usize>> {
        let n = self.tabs.len();
        let mut root: Vec<usize> = (0..n).collect();
        fn find(root: &mut [usize], mut i: usize) -> usize {
            while root[i] != i {
                root[i] = root[root[i]];
                i = root[i];
            }
            i
        }
        for i in 0..n {
            if let Some(j) = self.tabs[i].opener.and_then(|id| self.index_of(id)) {
                let (a, b) = (find(&mut root, i), find(&mut root, j));
                root[a] = b;
            }
        }
        let mut groups: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
        for i in 0..n {
            let r = find(&mut root, i);
            groups.entry(r).or_default().push(i);
        }
        groups.into_values().collect()
    }

    /// Ask tab `i`'s page whether it holds unsaved input.
    fn probe(&self, i: usize) {
        let t = &self.tabs[i];
        t.state.probe.set(Probe::Asked(std::time::Instant::now()));
        unsafe {
            let (script, world) = (cstr(PROBE_JS), cstr(PROBE_WORLD));
            webkit_web_view_evaluate_javascript(
                t.webview,
                script.as_ptr(),
                -1,
                world.as_ptr(),
                std::ptr::null(),
                std::ptr::null_mut(),
                Some(on_probe),
                Rc::into_raw(t.state.clone()) as gpointer,
            );
        }
    }

    fn put_to_sleep(&mut self, i: usize) {
        // A frame of this view may be held unread; hand it back before the
        // process that drew it goes.
        self.release_held();
        let t = &self.tabs[i];
        let idle = t.state.hidden_since.get().map_or(0, |h| h.elapsed().as_secs() / 60);
        log::info!(
            "putting {} to sleep (out of sight {idle} min)",
            t.url.as_ref().map_or("<no url>", |u| u.as_str())
        );
        t.state.asleep.set(true);
        t.state.probe.set(Probe::None);
        // The tab keeps its last frame (`Tab::image`), so showing it again
        // shows the page at once while the reload replaces it.
        unsafe { webkit_web_view_terminate_web_process(t.webview) };
    }
}

/// The probe's answer: `true` is unsaved input. An error — the page went
/// away, the process died — is taken as "edited", which only means not now.
unsafe extern "C" fn on_probe(source: *mut GObject, res: *mut GAsyncResult, data: gpointer) {
    let st = Rc::from_raw(data as *const TabState);
    let mut err: *mut GError = std::ptr::null_mut();
    let v = webkit_web_view_evaluate_javascript_finish(source as *mut WebKitWebView, res, &mut err);
    let edited = if v.is_null() {
        true
    } else {
        let b = jsc_value_to_boolean(v) != 0;
        g_object_unref(v as *mut _);
        b
    };
    if !err.is_null() {
        g_error_free(err);
    }
    // Asked again since, or the tab woke meanwhile: this answer is stale.
    if matches!(st.probe.get(), Probe::Asked(_)) {
        st.probe.set(if edited { Probe::Edited } else { Probe::Clean });
    }
}

/// The sweep's clock. It does nothing itself: being a GLib source is what
/// turns the loop, and the `pump` that follows does the sweep.
unsafe extern "C" fn on_sleep_tick(_data: gpointer) -> gboolean {
    1 // G_SOURCE_CONTINUE
}

/// Note a form post on its way. Never decides anything: WebKit's own
/// handling (and the middle-click handler) still runs.
pub(super) unsafe extern "C" fn on_decide_post(
    _wv: *mut WebKitWebView,
    decision: *mut WebKitPolicyDecision,
    kind: WebKitPolicyDecisionType::Type,
    data: gpointer,
) -> gboolean {
    if kind != WebKitPolicyDecisionType::WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION {
        return 0;
    }
    let action = webkit_navigation_policy_decision_get_navigation_action(
        decision as *mut WebKitNavigationPolicyDecision,
    );
    if action.is_null() {
        return 0;
    }
    let request = webkit_navigation_action_get_request(action);
    if !request.is_null()
        && from_cstr(webkit_uri_request_get_http_method(request)).is_some_and(|m| m.eq_ignore_ascii_case("POST"))
    {
        (*(data as *const TabState)).post_pending.set(true);
    }
    0
}

/// A page committed: it came from a form post if one was decided since the
/// last commit. (A frame's post counts too, which only errs towards keeping
/// the tab awake.)
pub(super) unsafe extern "C" fn on_load_changed(
    _wv: *mut WebKitWebView,
    event: WebKitLoadEvent::Type,
    data: gpointer,
) {
    if event == WebKitLoadEvent::WEBKIT_LOAD_COMMITTED {
        let st = &*(data as *const TabState);
        st.posted.set(st.post_pending.replace(false));
    }
}
