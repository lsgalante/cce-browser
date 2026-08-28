//! Waking on GLib activity instead of polling for it.
//!
//! WPE runs on a GLib `GMainContext`; cce-ui runs a calloop loop. The first
//! cut of [`super::WebKitHost::pump`] simply drained the context on a timer,
//! which works but burns wakeups when nothing is happening and adds latency
//! when something is.
//!
//! The bridge here is deliberately narrow: **calloop decides *when to look*,
//! GLib still does its own iteration.** We never reimplement GLib's
//! prepare/check/dispatch protocol — `g_main_context_iteration` does that,
//! correctly, and we only use `g_main_context_query` to learn what to wait on.
//!
//! GLib's fd set changes as WebKit opens sockets, and calloop wants stable
//! registrations, so the changing set lives in an **inner epoll fd** that is
//! itself the one stable thing calloop watches. Each pump re-syncs that set.
//! GLib also asks for a timeout, which a calloop timer carries.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::event::epoll;

use super::ffi::*;

/// The GLib fd set, mirrored into one epoll fd that calloop can watch.
pub(super) struct GlibPoll {
    epfd: OwnedFd,
    /// What is currently registered, so a re-sync can diff rather than
    /// teardown-and-rebuild every pump.
    registered: Vec<(i32, epoll::EventFlags)>,
    fds: Vec<GPollFD>,
    /// GLib's requested timeout in ms; `None` means "no timer needed".
    pub(super) timeout: Option<u32>,
}

fn flags_of(events: u16) -> epoll::EventFlags {
    let mut f = epoll::EventFlags::empty();
    // G_IO_IN / OUT / ERR / HUP, which are the poll(2) values.
    if events & 0x001 != 0 {
        f |= epoll::EventFlags::IN;
    }
    if events & 0x004 != 0 {
        f |= epoll::EventFlags::OUT;
    }
    if events & 0x008 != 0 {
        f |= epoll::EventFlags::ERR;
    }
    if events & 0x010 != 0 {
        f |= epoll::EventFlags::HUP;
    }
    f
}

impl GlibPoll {
    pub(super) fn new() -> std::io::Result<Self> {
        let epfd = epoll::create(epoll::CreateFlags::CLOEXEC)?;
        let mut this = Self {
            epfd,
            registered: Vec::new(),
            fds: Vec::new(),
            timeout: None,
        };
        this.sync();
        Ok(this)
    }

    pub(super) fn fd(&self) -> BorrowedFd<'_> {
        self.epfd.as_fd()
    }

    /// Ask GLib what it wants polled, and make the epoll set match.
    ///
    /// Called after every dispatch, because WebKit adds and drops fds as it
    /// opens connections — a set captured once goes stale within a page load.
    pub(super) fn sync(&mut self) {
        unsafe {
            let ctx = g_main_context_default();
            // `query` is only meaningful between prepare and check; we are not
            // running that protocol ourselves, but prepare also updates the
            // context's own idea of the timeout, so call it for that.
            let mut max_priority: i32 = 0;
            g_main_context_prepare(ctx, &mut max_priority);

            let mut timeout: i32 = -1;
            // Two-pass: ask for the count, then fill.
            let n = g_main_context_query(ctx, max_priority, &mut timeout, std::ptr::null_mut(), 0);
            self.fds.clear();
            self.fds.resize(n.max(0) as usize, std::mem::zeroed());
            let n = if self.fds.is_empty() {
                0
            } else {
                g_main_context_query(
                    ctx,
                    max_priority,
                    &mut timeout,
                    self.fds.as_mut_ptr(),
                    self.fds.len() as i32,
                )
            };
            self.fds.truncate(n.max(0) as usize);
            self.timeout = (timeout >= 0).then_some(timeout as u32);
        }

        let want: Vec<(i32, epoll::EventFlags)> = self
            .fds
            .iter()
            .map(|p| (p.fd, flags_of(p.events)))
            .collect();

        // Diff against what is registered. Same-fd-different-flags is a
        // modify, not a delete plus add, so a busy socket is not churned.
        for (fd, flags) in &want {
            let borrowed = unsafe { BorrowedFd::borrow_raw(*fd) };
            let data = epoll::EventData::new_u64(*fd as u64);
            match self.registered.iter().find(|(f, _)| f == fd) {
                Some((_, old)) if old == flags => {}
                Some(_) => {
                    let _ = epoll::modify(&self.epfd, borrowed, data, *flags);
                }
                None => {
                    let _ = epoll::add(&self.epfd, borrowed, data, *flags);
                }
            }
        }
        for (fd, _) in &self.registered {
            if !want.iter().any(|(f, _)| f == fd) {
                let _ = epoll::delete(&self.epfd, unsafe { BorrowedFd::borrow_raw(*fd) });
            }
        }
        self.registered = want;
    }

    /// Drain the inner epoll so it stops reporting readable. calloop is
    /// level-triggered on this fd; without this the loop would spin on a
    /// socket GLib has not consumed yet.
    pub(super) fn drain(&self) {
        let mut events = epoll::EventVec::with_capacity(16);
        let _ = epoll::wait(&self.epfd, &mut events, 0);
    }
}
