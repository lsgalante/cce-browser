//! Proves what a tab does when its WebProcess hangs or dies.
//!
//! Serves a page that spins its main thread forever on the first visit and
//! loads normally on the next, and checks, against the real engine:
//!
//! * a hung page is reported unresponsive once a click goes unanswered —
//!   including after a pointer move, which WebKit's own timer misses;
//! * a page that answers is not;
//! * "Wait" quiets the question, and stopping kills the process;
//! * the dead tab shows the error page **under its own URL**, so the URL
//!   bar and the saved session still mean the real page;
//! * a reload from there fetches the real page again;
//! * a WebProcess that dies outright (SIGKILL) gets the crash page.
//!
//! `cargo run --release -p cce-browser --example wpe_crash`

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

/// Spins once it has painted, so there is a frame to be stuck on.
#[cfg(feature = "wpe")]
const HANG: &str = "<!doctype html><title>hang</title><p>spinning\
<script>setTimeout(() => { for (;;) {} }, 300)</script>";
#[cfg(feature = "wpe")]
const FINE: &str = "<!doctype html><title>recovered</title><p>fine";

/// The first request gets the hanging page, every later one the fine one.
#[cfg(feature = "wpe")]
fn serve() -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut served = 0;
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            if !String::from_utf8_lossy(&buf[..n]).starts_with("GET /page") {
                let _ = write!(s, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                continue;
            }
            let body = if served == 0 { HANG } else { FINE };
            served += 1;
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
        }
    });
    port
}

/// Every `WPEWebProcess` under this process (they sit below bwrap).
#[cfg(feature = "wpe")]
fn web_processes() -> Vec<i32> {
    let me = std::process::id() as i32;
    let parent_of = |pid: i32| -> Option<i32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
    };
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if comm.trim() != "WPEWebProcess" {
            continue;
        }
        let mut p = pid;
        while let Some(pp) = parent_of(p) {
            if pp == me {
                out.push(pid);
                break;
            }
            if pp <= 1 {
                break;
            }
            p = pp;
        }
    }
    out
}

#[cfg(feature = "wpe")]
fn main() {
    let port = serve();
    let page = url::Url::parse(&format!("http://127.0.0.1:{port}/page")).unwrap();
    let mut host = wpe::WebKitHost::new(page.clone(), (800, 600));
    host.focus(true);
    let settle = |h: &mut wpe::WebKitHost, ms: u64| {
        let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        while std::time::Instant::now() < end {
            h.pump();
            // Nothing draws here; say so, or the readback waits forever.
            h.frame_drawn();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };

    use cce_ui::widget::MouseButton;
    let mut ok = true;
    let mut check = |what: &str, pass: bool, detail: String| {
        ok &= pass;
        println!("{what:<44} {} {detail}", if pass { "OK   " } else { "WRONG" });
    };

    settle(&mut host, 1500);
    check("the hanging page loaded", host.title().as_deref() == Some("hang"), format!("{:?}", host.title()));
    check("not unresponsive before any input", !host.active_unresponsive(), String::new());

    // Move, then click: the order a person does it in, and the one WebKit's
    // own timer misses — the click queues behind the unanswered move and is
    // never sent. The host's ping is what catches it.
    host.mouse_move(100.0, 100.0);
    host.mouse_button_ui(MouseButton::Left, true, 100.0, 100.0);
    host.mouse_button_ui(MouseButton::Left, false, 100.0, 100.0);
    settle(&mut host, 4000);
    check("unanswered input reports a hang", host.active_unresponsive(), String::new());

    host.wait_unresponsive();
    check("waiting quiets the question", !host.active_unresponsive(), String::new());

    host.stop_unresponsive();
    settle(&mut host, 1500);
    let title = host.title().unwrap_or_default();
    check("a stopped page shows the error page", title == "This page was stopped", format!("{title:?}"));
    check("under its own URL", host.url().as_ref() == Some(&page), format!("{:?}", host.url().map(|u| u.to_string())));
    check("and is no longer unresponsive", !host.active_unresponsive(), String::new());

    host.reload();
    settle(&mut host, 1500);
    let title = host.title().unwrap_or_default();
    check("reload fetches the real page", title == "recovered", format!("{title:?}"));

    // A page that answers is never asked about.
    host.mouse_move(120.0, 120.0);
    host.mouse_button_ui(MouseButton::Left, true, 120.0, 120.0);
    host.mouse_button_ui(MouseButton::Left, false, 120.0, 120.0);
    settle(&mut host, 4000);
    check("a live page is not reported", !host.active_unresponsive(), String::new());

    let victims = web_processes();
    // SIGKILL rather than SIGSEGV: JavaScriptCore installs its own SEGV
    // handler, and a sent one is not a fault it will die of.
    for &pid in &victims {
        unsafe { libc_kill(pid, 9) };
    }
    settle(&mut host, 1500);
    let title = host.title().unwrap_or_default();
    check(
        "a crashed process shows the crash page",
        title == "This page crashed",
        format!("{title:?} after SIGKILL to {victims:?}"),
    );
    check("still under its own URL", host.url().as_ref() == Some(&page), String::new());

    host.reload();
    settle(&mut host, 1500);
    let title = host.title().unwrap_or_default();
    check("and reloads from there", title == "recovered", format!("{title:?}"));

    println!("\ncrash: {}", if ok { "OK" } else { "BROKEN" });
    std::process::exit(if ok { 0 } else { 1 });
}

#[cfg(feature = "wpe")]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}
