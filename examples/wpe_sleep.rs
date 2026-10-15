//! Proves sleeping tabs (`src/wpe/host/sleep.rs`) and the background timer
//! throttle, against the real engine:
//!
//! * a tab restored asleep shows its saved title and has no web process;
//!   showing it loads it;
//! * a tab out of sight past the limit is put to sleep: its process is gone,
//!   its title and URL stay;
//! * showing it again reloads the page *with its history*: Back still works;
//! * showing it right after it fell asleep loads the page, not the crash
//!   page;
//! * a tab with edited input, or whose page came from a form post, stays
//!   awake — and untouched choices are not taken for edits;
//! * a popup and its opener sleep only together: not while the opener is
//!   shown, both once neither is;
//! * a hidden page's 10 ms timer runs about once a second;
//! * closing a tab stops its process once its page has unloaded — the
//!   page's `pagehide` beacon still goes out — but closing a popup leaves
//!   the opener it shares a process with alone.
//!
//! `cargo run --release -p cce-browser --example wpe_sleep`

// Builds the app's own modules by path and uses only part of each.
#![allow(dead_code, unused_imports)]

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
#[path = "../src/vi.rs"]
#[allow(dead_code)]
mod vi;

#[path = "../src/wpe/mod.rs"]
mod wpe;

#[cfg(feature = "wpe")]
fn page(path: &str) -> String {
    match path {
        // Edited by its own script: what a half-filled form looks like.
        "/form" => "<!doctype html><title>form</title><input value=served>\
                    <script>document.querySelector('input').value = 'typed'</script>"
            .into(),
        // Untouched choices, one with no option marked `selected`: its
        // first option is selected without being the default one.
        "/select" => "<!doctype html><title>select</title><select><option>a<option>b</select>\
                      <select><option>a<option selected>b</select><input type=checkbox checked>"
            .into(),
        // Posts itself on load, landing on /done by POST.
        "/post" => "<!doctype html><title>posting</title>\
                    <form method=post action=/done><input name=q value=1></form>\
                    <script>document.forms[0].submit()</script>"
            .into(),
        // Opens a popup on the first press.
        "/opener" => "<!doctype html><title>opener</title><p style='height:2000px'>press\
                      <script>addEventListener('mousedown', () => window.open('/c'), {once: true})</script>"
            .into(),
        // Says goodbye on its way out.
        "/leaving" => "<!doctype html><title>leaving</title>\
                       <script>addEventListener('pagehide', () => navigator.sendBeacon('/beacon'))</script>"
            .into(),
        // Counts its timer's runs in the title.
        "/timer" => "<!doctype html><title>n0</title>\
                     <script>let n = 0; setInterval(() => { document.title = 'n' + ++n }, 10)</script>"
            .into(),
        p => format!("<!doctype html><title>{}</title><p>{p}", p.trim_start_matches('/')),
    }
}

/// The `/leaving` page's beacon arrived.
#[cfg(feature = "wpe")]
static BEACON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(feature = "wpe")]
fn serve() -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 8192];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
            if path == "/beacon" {
                BEACON.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let body = page(&path);
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
    use cce_ui::widget::MouseButton;
    use std::time::Duration;
    let port = serve();
    let url = |p: &str| url::Url::parse(&format!("http://127.0.0.1:{port}{p}")).unwrap();
    let settle = |h: &mut wpe::WebKitHost, ms: u64| {
        let end = std::time::Instant::now() + Duration::from_millis(ms);
        while std::time::Instant::now() < end {
            h.pump();
            // Nothing draws here; say so, or the readback waits forever.
            h.frame_drawn();
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let title = |h: &wpe::WebKitHost, i: usize| h.tab(i).and_then(|t| t.title.clone()).unwrap_or_default();
    let asleep = |h: &wpe::WebKitHost, i: usize| h.tab(i).is_some_and(|t| t.asleep());
    let mut ok = true;
    let mut check = |what: &str, pass: bool, detail: String| {
        ok &= pass;
        println!("{what:<56} {} {detail}", if pass { "OK   " } else { "WRONG" });
    };

    // Restored: the second tab in front, the first asleep under its title.
    let mut host = wpe::WebKitHost::restore(
        vec![(url("/a"), Some("Saved A".into())), (url("/b"), Some("Saved B".into()))],
        1,
        (800, 600),
    );
    host.focus(true);
    settle(&mut host, 1500);
    let procs = web_processes().len();
    check("restored: the tab in front loads", title(&host, 1) == "b", title(&host, 1));
    check("the other is asleep under its saved title", asleep(&host, 0) && title(&host, 0) == "Saved A", title(&host, 0));
    // The one in front and the prewarmed spare; none for the sleeper.
    check("and has no web process", procs == 2, format!("{procs} processes"));

    host.activate(0);
    settle(&mut host, 1500);
    check("showing it loads it", !asleep(&host, 0) && title(&host, 0) == "a", title(&host, 0));
    check("with a process of its own", web_processes().len() == 3, format!("{} processes", web_processes().len()));

    // History for tab 0: /a, then /c.
    host.load(url("/c"));
    settle(&mut host, 1000);
    host.activate(1);
    host.set_sleep_after(Some(Duration::from_millis(500)));
    settle(&mut host, 2500);
    check("out of sight past the limit, it sleeps", asleep(&host, 0), String::new());
    check("keeping its title", title(&host, 0) == "c", title(&host, 0));
    check(
        "and its URL",
        host.tab(0).and_then(|t| t.url.clone()) == Some(url("/c")),
        format!("{:?}", host.tab(0).and_then(|t| t.url.as_ref().map(|u| u.to_string()))),
    );
    // Process swap leaves /a's process in WebKit's cache; what matters is
    // that the sleeper's own one is gone, which a count cannot single out
    // — so compare against waking it: one process comes back.
    let sleeping = web_processes().len();

    host.set_sleep_after(None);
    host.activate(0);
    settle(&mut host, 1500);
    let awake = web_processes().len();
    check("showing it again reloads the page", !asleep(&host, 0) && title(&host, 0) == "c", title(&host, 0));
    check("in a process of its own again", awake == sleeping + 1, format!("{sleeping} -> {awake} processes"));
    check("with its history: Back is there", host.can_go_back(), String::new());
    host.back();
    settle(&mut host, 1500);
    check("and goes where it went before", title(&host, 0) == "a", title(&host, 0));

    // Asleep and straight back: the termination must not land as a crash.
    // Woken in the very turn it fell asleep in, before the end of its
    // process has even been reported.
    host.activate(1);
    host.set_sleep_after(Some(Duration::from_millis(300)));
    let end = std::time::Instant::now() + Duration::from_secs(3);
    while !asleep(&host, 0) && std::time::Instant::now() < end {
        host.pump();
        host.frame_drawn();
        std::thread::sleep(Duration::from_millis(10));
    }
    let slept = asleep(&host, 0);
    host.set_sleep_after(None);
    host.activate(0);
    settle(&mut host, 1500);
    check("woken at once after sleeping, it loads the page", slept && title(&host, 0) == "a", title(&host, 0));

    // What stays awake: edited input, a page that came from a post.
    host.open_background_tab(url("/form"));
    host.open_background_tab(url("/post"));
    host.open_background_tab(url("/select"));
    let (form, post, select) = (2, 3, 4);
    // Until the post has landed — a tab still loading stays awake anyway,
    // which would prove nothing.
    for _ in 0..50 {
        settle(&mut host, 100);
        if title(&host, post) == "done" && !host.tab(post).is_some_and(|t| t.loading) {
            break;
        }
    }
    check("(the post landed)", title(&host, post) == "done", title(&host, post));
    host.set_sleep_after(Some(Duration::from_millis(300)));
    settle(&mut host, 2500);
    check("a tab with edited input stays awake", !asleep(&host, form), String::new());
    check("a page that came from a form post stays awake", !asleep(&host, post), String::new());
    check("(while plain ones sleep)", asleep(&host, 1), String::new());
    check("untouched choices are not edits: it sleeps", asleep(&host, select), String::new());
    host.set_sleep_after(None);

    // A popup and its opener.
    host.open_tab(url("/opener"));
    settle(&mut host, 1500);
    let opener = host.tab_count() - 1;
    host.mouse_move(100.0, 100.0);
    host.mouse_button_ui(MouseButton::Left, true, 100.0, 100.0);
    host.mouse_button_ui(MouseButton::Left, false, 100.0, 100.0);
    settle(&mut host, 1000);
    let popup = host.adopt_popups();
    settle(&mut host, 1000);
    check("(the opener opened a popup)", popup.is_some(), format!("{popup:?}"));
    let popup = popup.unwrap_or(opener);
    host.set_sleep_after(Some(Duration::from_millis(300)));
    settle(&mut host, 2500);
    check("a popup does not sleep while its opener is shown", !asleep(&host, popup), title(&host, popup));
    host.activate(0);
    settle(&mut host, 2500);
    check("both sleep once neither is shown", asleep(&host, popup) && asleep(&host, opener), String::new());
    host.set_sleep_after(None);
    host.activate(opener);
    settle(&mut host, 1500);
    check("the opener wakes on its own", title(&host, opener) == "opener" && asleep(&host, popup), title(&host, opener));

    // The timer throttle.
    host.open_background_tab(url("/timer"));
    settle(&mut host, 1000);
    let t = host.tab_count() - 1;
    let count = |h: &wpe::WebKitHost| title(h, t).trim_start_matches('n').parse::<u32>().unwrap_or(0);
    let before = count(&host);
    settle(&mut host, 3000);
    let runs = count(&host) - before;
    check("a hidden page's 10 ms timer runs about once a second", runs <= 6, format!("{runs} runs in 3 s"));
    host.activate(t);
    settle(&mut host, 300);
    let before = count(&host);
    settle(&mut host, 1000);
    let runs = count(&host) - before;
    check("and freely once shown", runs >= 30, format!("{runs} runs in 1 s"));

    // Closing. A tab of its own: the prewarmed spare becomes it and a new
    // spare is built, so closing it should bring the count back to before.
    let before = web_processes().len();
    host.open_tab(url("/leaving"));
    settle(&mut host, 1500);
    host.close_tab(host.tab_count() - 1);
    settle(&mut host, 2500);
    let after = web_processes().len();
    check("closing a tab stops its process", after == before, format!("{before} -> {after} processes"));
    check("after its page has said goodbye", BEACON.load(std::sync::atomic::Ordering::SeqCst), String::new());
    // A popup shares its opener's process: closing it must not take the
    // opener down.
    host.open_tab(url("/opener"));
    settle(&mut host, 1500);
    let opener = host.tab_count() - 1;
    host.mouse_move(100.0, 100.0);
    host.mouse_button_ui(MouseButton::Left, true, 100.0, 100.0);
    host.mouse_button_ui(MouseButton::Left, false, 100.0, 100.0);
    settle(&mut host, 1000);
    let popup = host.adopt_popups();
    settle(&mut host, 1000);
    if let Some(p) = popup {
        host.close_tab(p);
    }
    settle(&mut host, 2500);
    check(
        "closing a popup leaves its opener running",
        popup.is_some() && title(&host, opener) == "opener" && !asleep(&host, opener),
        title(&host, opener),
    );

    println!("\n{}", if ok { "all OK" } else { "FAILED" });
    std::process::exit(if ok { 0 } else { 1 });
}
