//! Demonstrates the calloop integration pattern: **block on GLib's fds**
//! rather than pumping on a timer.
//!
//! This is what `Application::register_sources` will do — register
//! `host.poll_fd()` as a calloop `Generic` and a timer for
//! `host.poll_timeout()`, both firing a `Message::Spin` that calls `pump`.
//! Here the same thing is done with a bare `poll(2)` so the behaviour can be
//! measured without a compositor.
//!
//! Run with `--features wpe`; pass `poll` as argv[1] to compare against the
//! old fixed-interval polling.

#[cfg(not(feature = "wpe"))]
fn main() {
    eprintln!("build with --features wpe");
}

#[cfg(feature = "wpe")]
#[derive(Debug, Clone, Copy)]
pub enum EditingCommand { Copy, Cut, Paste }

#[cfg(feature = "wpe")]
#[path = "../src/pages.rs"]
mod pages;
#[cfg(feature = "wpe")]
#[path = "../src/downloads.rs"]
mod downloads;
#[cfg(feature = "wpe")]
#[path = "../src/wpe/mod.rs"]
mod wpe;

#[cfg(feature = "wpe")]
fn main() {
    use rustix::event::{poll, PollFd, PollFlags};
    use std::time::{Duration, Instant};

    let blocking = std::env::args().nth(1).as_deref() != Some("poll");
    let url = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "https://example.com".into());
    let mut host = wpe::WebKitHost::new(url::Url::parse(&url).unwrap(), (1200, 800));

    let (mut wakeups, mut frames) = (0u32, 0u32);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(8) {
        if blocking {
            // Sleep until GLib has work, or until it asked to be woken.
            let ms = host
                .poll_timeout()
                .map(|d| d.as_millis() as i32)
                .unwrap_or(1000)
                .clamp(0, 1000);
            if let Some(fd) = host.poll_fd() {
                let mut fds = [PollFd::from_borrowed_fd(fd, PollFlags::IN)];
                let _ = poll(&mut fds, ms);
            }
        } else {
            std::thread::sleep(Duration::from_millis(16)); // the old way
        }
        wakeups += 1;
        if host.pump().0 {
            frames += 1;
        }
    }

    println!(
        "{:<9} {wakeups:>5} wakeups  {frames:>3} frames  over 8s   title={:?}",
        if blocking { "blocking" } else { "polling" },
        host.title()
    );
}
