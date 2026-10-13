//! A window a page opens becomes a tab, end to end.
//!
//! Serves a page that opens a popup from a click, has a `target=_blank` link,
//! and tries `window.open` at load with no click behind it. Checks against
//! the real engine:
//!
//! * the load-time open is still blocked (no popup without a gesture);
//! * a click's `window.open` reaches `create`, and the window is adopted as
//!   a tab whose page loads;
//! * that page has a `window.opener`, and a message it posts there arrives —
//!   what a sign-in popup (Google's, on claude.ai) needs to hand back;
//! * the popup's `window.close()` is reported, and closing it returns to the
//!   opener rather than the tab beside it;
//! * a left-clicked `target=_blank` link opens a tab the same way;
//! * a page the person opened cannot close itself.
//!
//! `cce-shadow --instance <n> run ./target/release/examples/wpe_popup`

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
// The host's vi channel and scripts.
#[path = "../src/vi.rs"]
#[allow(dead_code)]
mod vi;

#[path = "../src/wpe/mod.rs"]
mod wpe;

#[cfg(feature = "wpe")]
const PAGE: &str = "<!doctype html><title>opener</title>\
<style>button,a{position:absolute;left:20px;width:200px;height:40px;display:block}</style>\
<button style=top:20px onclick=\"window.open('/popup')\">open</button>\
<a href=/blank target=_blank style=top:100px>blank</a>\
<button style=top:180px onclick=\"window.close()\">close</button>\
<script>\
addEventListener('message', e => { document.title = 'got:' + e.data });\
window.open('/auto');\
</script>";

#[cfg(feature = "wpe")]
const POPUP: &str = "<!doctype html><title>popup</title>\
<style>button{position:absolute;left:0;top:0;width:400px;height:400px}</style>\
<button onclick=\"window.close()\">close</button>\
<script>\
document.title = window.opener ? 'popup:opener' : 'popup:none';\
if (window.opener) window.opener.postMessage('hi', '*');\
</script>";

#[cfg(feature = "wpe")]
fn serve() -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
            let body = match path.as_str() {
                "/" => PAGE.to_string(),
                "/popup" => POPUP.to_string(),
                _ => format!("<!doctype html><title>{}</title>", &path[1..]),
            };
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

#[cfg(feature = "wpe")]
fn main() {
    use cce_ui::widget::MouseButton;
    let port = serve();
    let base = format!("http://127.0.0.1:{port}");
    let mut host = wpe::WebKitHost::new(url::Url::parse(&format!("{base}/")).unwrap(), (1200, 800));
    let settle = |h: &mut wpe::WebKitHost, n: u32| {
        for _ in 0..n {
            h.pump(); h.frame_drawn();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };
    let click = |h: &mut wpe::WebKitHost, x: f32, y: f32| {
        h.mouse_move(x, y);
        h.mouse_button_ui(MouseButton::Left, true, x, y);
        h.mouse_button_ui(MouseButton::Left, false, x, y);
    };
    let title = |h: &wpe::WebKitHost, i: usize| h.tab(i).and_then(|t| t.title.clone());
    settle(&mut host, 30);
    println!("loaded: {:?}", host.title());
    let mut ok = true;
    let mut check = |name: &str, pass: bool| {
        println!("  {name:<44} {}", if pass { "OK" } else { "FAIL" });
        ok &= pass;
    };

    check("window.open at load is blocked", host.adopt_popups().is_none());

    // A tab beside the opener, so returning to the opener is not the same
    // as falling to the neighbour.
    host.open_background_tab(url::Url::parse(&format!("{base}/other")).unwrap());
    settle(&mut host, 20);

    click(&mut host, 60.0, 40.0);
    settle(&mut host, 20);
    let popup = host.adopt_popups();
    println!("clicked open -> {popup:?}, {} tabs", host.tab_count());
    check("a click's window.open is adopted as a tab", popup == Some(2));
    let Some(popup) = popup else {
        println!("\nFAILED");
        std::process::exit(1);
    };
    host.activate(popup);
    settle(&mut host, 20);
    println!("popup: {:?}, opener: {:?}", title(&host, popup), title(&host, 0));
    check("  its page loads, with a window.opener", title(&host, popup).as_deref() == Some("popup:opener"));
    check("  and its message reaches the opener", title(&host, 0).as_deref() == Some("got:hi"));

    let id = host.tab(popup).map(|t| t.id);
    click(&mut host, 100.0, 100.0);
    settle(&mut host, 10);
    let closes = host.take_page_closes();
    println!("popup closed itself -> {closes:?}");
    check("the popup's window.close() is reported", id.is_some() && closes == id.into_iter().collect::<Vec<_>>());
    for id in closes {
        if let Some(i) = host.index_of(id) {
            host.close_tab(i);
        }
    }
    check("  and closing it returns to the opener", host.tab_count() == 2 && host.active_index() == 0);

    click(&mut host, 60.0, 120.0);
    settle(&mut host, 20);
    let blank = host.adopt_popups();
    println!("clicked target=_blank -> {blank:?}, title {:?}", blank.and_then(|i| title(&host, i)));
    check("a target=_blank link opens a tab", blank == Some(2));
    check("  that loads the link", blank.and_then(|i| title(&host, i)).as_deref() == Some("blank"));
    if let Some(i) = blank {
        host.activate(i);
        settle(&mut host, 5);
        host.close_tab(i);
        check("  and closing it returns to the opener", host.active_index() == 0);
    }

    click(&mut host, 60.0, 200.0);
    settle(&mut host, 10);
    let closes = host.take_page_closes();
    println!("opener tried window.close() -> {closes:?}");
    check("a page the person opened cannot close itself", closes.is_empty() && host.tab_count() == 2);

    println!("\n{}", if ok { "OK" } else { "FAILED" });
    std::process::exit(if ok { 0 } else { 1 });
}
