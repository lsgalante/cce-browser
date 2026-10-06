//! Proves the page half of account autocomplete across a frame boundary.
//!
//! Serves a page on `127.0.0.1` with a sign-in form in an iframe from
//! `localhost` — two origins, the shape of iCloud's sign-in — and checks,
//! against the real engine:
//!
//! * a focused field in the frame is reported with the **frame's** origin,
//!   and the frame's offset in the page arrives as a `Frame` event;
//! * a fill answered to that frame's token lands in the frame's fields,
//!   quotes and all;
//! * pressing the sign-in button reports the credential for saving;
//! * the relay's `postMessage` traffic never reaches the page's own script;
//! * a document without focus (a background tab, a window the person left)
//!   reports nothing.
//!
//! No keyring is involved: this is the engine side only.
//!
//! `cargo run --release -p cce-browser --example wpe_autofill`

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

/// Quotes, a backslash and a closing script tag: everything that would end a
/// string spliced into script source. (No newline — a password field strips
/// line breaks, so one could never round-trip.)
const PASSWORD: &str = "s3\"cr\\et</script>";

/// The embedding page: the frame sits at a known place, behind a border and
/// padding, so the reported offset can be checked to the pixel.
const TOP: &str = r#"<!doctype html><html><body style="margin:0">
<form onsubmit="event.preventDefault()">
  <input id="tu" type="email" style="position:absolute; left:10px; top:10px; width:200px; height:24px">
  <input id="tp" type="password" style="position:absolute; left:10px; top:50px; width:200px; height:24px">
</form>
<iframe id="f" src="http://localhost:PORT/frame"
  style="position:absolute; left:100px; top:150px; width:400px; height:200px;
         border:5px solid #888; padding:7px"></iframe>
<script>
  // The page's own listener: it must see the frame's probes, never the
  // watcher's relay.
  const tu = document.getElementById('tu'), tp = document.getElementById('tp');
  tp.addEventListener('input', () => { document.title = 'top:' + tu.value + '|' + tp.value; });
  window.addEventListener('message', (e) => {
    if (e.data && e.data.cceAccountsFrame) { document.title += ' LEAK'; return; }
    if (e.data && e.data.probe !== undefined) document.title = 'frame:' + e.data.probe;
  });
</script></body></html>"#;

/// The sign-in frame. Its own script reports what its fields hold, which is
/// how the test reads a fill back out of a cross-origin frame.
const FRAME: &str = r#"<!doctype html><html><body style="margin:0">
<form onsubmit="event.preventDefault()">
  <input id="u" name="username" style="position:absolute; left:10px; top:10px; width:200px; height:24px">
  <input id="p" type="password" style="position:absolute; left:10px; top:50px; width:200px; height:24px">
  <button id="go" type="submit" style="position:absolute; left:10px; top:90px; width:80px; height:24px">Sign in</button>
</form>
<script>
  const u = document.getElementById('u'), p = document.getElementById('p');
  const tell = () => parent.postMessage({ probe: u.value + '|' + p.value }, '*');
  u.addEventListener('input', tell); p.addEventListener('input', tell);
</script></body></html>"#;

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
            let req = String::from_utf8_lossy(&buf[..n]);
            let body = if req.starts_with("GET /frame") { FRAME.to_string() } else {
                TOP.replace("PORT", &port.to_string())
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
    use wpe::FormEvent;

    let port = serve();
    let mut host = wpe::WebKitHost::new(url::Url::parse("about:blank").unwrap(), (800, 600));
    host.set_accounts_enabled(true);
    host.focus(true);
    let settle = |h: &mut wpe::WebKitHost, n: u32| {
        for _ in 0..n {
            h.pump(); h.frame_drawn();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };
    let drain = |h: &wpe::WebKitHost| {
        let mut out = Vec::new();
        while let Some(e) = h.take_form_event() {
            out.push(e);
        }
        out
    };
    let click = |h: &mut wpe::WebKitHost, x: f32, y: f32| {
        h.mouse_move(x, y);
        h.mouse_button_ui(MouseButton::Left, true, x, y);
        h.mouse_button_ui(MouseButton::Left, false, x, y);
    };

    settle(&mut host, 10);
    host.load(url::Url::parse(&format!("http://127.0.0.1:{port}/top")).unwrap());
    settle(&mut host, 40);
    drain(&host);

    let mut ok = true;
    let mut check = |what: &str, pass: bool, detail: String| {
        ok &= pass;
        println!("{what:<44} {} {detail}", if pass { "OK   " } else { "WRONG" });
    };

    // The frame's content box starts at 100+5+7, 150+5+7; its username
    // field at 10,10 inside that.
    click(&mut host, 112.0 + 50.0, 162.0 + 20.0);
    settle(&mut host, 10);
    let events = drain(&host);
    let field = events.iter().find_map(|e| match e {
        FormEvent::Field { origin, frame, top, password, rect, moved: false, .. } => {
            Some((origin.clone(), frame.clone(), *top, *password, *rect))
        }
        _ => None,
    });
    let token = field.as_ref().map(|f| f.1.clone()).unwrap_or_default();
    check(
        "frame field reported with the frame's origin",
        field.as_ref().is_some_and(|f| {
            f.0 == format!("http://localhost:{port}") && !f.2 && !f.3 && f.4.0 == 10.0 && f.4.1 == 10.0
        }),
        format!("{field:?}"),
    );
    let offset = events.iter().find_map(|e| match e {
        FormEvent::Frame { frame, offset } if *frame == token => Some(*offset),
        _ => None,
    });
    check("frame offset relayed to the top", offset == Some((112.0, 162.0)), format!("{offset:?}"));

    let filled = host.fill_credentials(&token, "alice", PASSWORD);
    settle(&mut host, 10);
    let title = host.title().unwrap_or_default();
    check(
        "fill answered to the frame's token",
        filled && title == format!("frame:alice|{PASSWORD}"),
        format!("{title:?}"),
    );
    check("a token nobody asked with fills nothing", !host.fill_credentials("00", "x", "y"), String::new());

    click(&mut host, 112.0 + 40.0, 162.0 + 100.0);
    settle(&mut host, 10);
    let submit = drain(&host).into_iter().find_map(|e| match e {
        FormEvent::Submit { origin, username, password, top, .. } => {
            Some((origin, username, password.expose().to_string(), top))
        }
        _ => None,
    });
    check(
        "sign-in button reports the credential",
        submit.as_ref().is_some_and(|s| {
            s.0 == format!("http://localhost:{port}") && s.1 == "alice" && s.2 == PASSWORD && !s.3
        }),
        format!("{:?}", submit.as_ref().map(|s| (&s.0, &s.1, s.3))),
    );

    check("the page never saw the relay", !host.title().unwrap_or_default().contains("LEAK"), String::new());

    // The top frame takes the same path, with its own token.
    click(&mut host, 60.0, 20.0);
    settle(&mut host, 10);
    let top = drain(&host).into_iter().find_map(|e| match e {
        FormEvent::Field { frame, top: true, moved: false, origin, .. } => Some((frame, origin)),
        _ => None,
    });
    check(
        "top-frame field reported as the top",
        top.as_ref().is_some_and(|t| t.1 == format!("http://127.0.0.1:{port}") && t.0.len() == 24),
        format!("{top:?}"),
    );
    let top_token = top.map(|t| t.0).unwrap_or_default();
    check("an answered ask cannot be answered again", !host.fill_credentials(&token, "x", "y"), String::new());
    let filled = host.fill_credentials(&top_token, "bob@example.com", "pw");
    settle(&mut host, 10);
    let title = host.title().unwrap_or_default();
    check("fill answered to the top's token", filled && title == "top:bob@example.com|pw", format!("{title:?}"));

    // Without focus the document is not live: nothing is reported.
    host.focus(false);
    settle(&mut host, 4);
    drain(&host);
    click(&mut host, 112.0 + 50.0, 162.0 + 60.0);
    settle(&mut host, 10);
    let quiet = drain(&host);
    check(
        "an unfocused document reports no fields",
        !quiet.iter().any(|e| matches!(e, FormEvent::Field { .. })),
        format!("{} events", quiet.len()),
    );

    println!("\nautofill: {}", if ok { "OK" } else { "BROKEN" });
    std::process::exit(if ok { 0 } else { 1 });
}
