//! A middle-clicked link becomes a background tab, end to end.
//!
//! Serves a page with an ordinary link and a `target=_blank` one, and checks
//! against the real engine:
//!
//! * a middle-click on either is diverted — queued for a background tab —
//!   and the page it was clicked on stays where it was;
//! * a left-click on the ordinary link still navigates, undiverted;
//! * `open_background_tab` adds a tab that loads while the active one
//!   stays active.
//!
//! `cce-shadow --instance <n> run ./target/release/examples/wpe_middle`

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
const PAGE: &str = "<!doctype html><title>links</title>\
<style>a{position:absolute;left:20px;width:200px;height:40px;display:block}</style>\
<a id=plain href=/plain style=top:20px>plain</a>\
<a id=blank href=/blank target=_blank style=top:100px>blank</a>";

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
            let body = if path == "/" {
                PAGE.to_string()
            } else {
                format!("<!doctype html><title>{}</title>", &path[1..])
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
    let click = |h: &mut wpe::WebKitHost, b: MouseButton, x: f32, y: f32| {
        h.mouse_move(x, y);
        h.mouse_button_ui(b, true, x, y);
        h.mouse_button_ui(b, false, x, y);
    };
    settle(&mut host, 30);
    println!("loaded: {:?}", host.title());
    let mut ok = true;
    let mut check = |name: &str, pass: bool| {
        println!("  {name:<36} {}", if pass { "OK" } else { "FAIL" });
        ok &= pass;
    };

    click(&mut host, MouseButton::Middle, 60.0, 40.0);
    settle(&mut host, 10);
    let plain = host.take_background_opens();
    println!("middle plain -> {plain:?}, title {:?}", host.title());
    check("middle-click plain link is queued", plain.iter().any(|u| u.path() == "/plain"));
    check("  and the page stays", host.title().as_deref() == Some("links"));

    click(&mut host, MouseButton::Middle, 60.0, 120.0);
    settle(&mut host, 10);
    let blank = host.take_background_opens();
    println!("middle blank -> {blank:?}, title {:?}", host.title());
    check("middle-click target=_blank is queued", blank.iter().any(|u| u.path() == "/blank"));
    check("  and the page stays", host.title().as_deref() == Some("links"));

    let count = host.tab_count();
    host.open_background_tab(url::Url::parse(&format!("{base}/bg")).unwrap());
    settle(&mut host, 20);
    check("background tab added", host.tab_count() == count + 1);
    check("  active tab unchanged", host.active_index() == 0);
    check(
        "  and it loaded behind the page",
        host.tab(count).and_then(|t| t.title.clone()).as_deref() == Some("bg"),
    );

    click(&mut host, MouseButton::Left, 60.0, 40.0);
    settle(&mut host, 20);
    let left = host.take_background_opens();
    println!("left plain -> {left:?}, title {:?}", host.title());
    check("left-click is not diverted", left.is_empty());
    check("  and navigates", host.title().as_deref() == Some("plain"));

    println!("\n{}", if ok { "OK" } else { "FAILED" });
    std::process::exit(if ok { 0 } else { 1 });
}
