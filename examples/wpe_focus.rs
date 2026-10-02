//! Proves the page believes it is focused — the condition WebKit gates the
//! text caret on.
//!
//! A caret is painted only in a frame that is both *focused* (the view's
//! focus) and *active* (the toplevel's `ACTIVE` state). Without either, a
//! field takes typing perfectly well and shows no caret, which is how it
//! shipped. `document.hasFocus()` reads the same two conditions, so the page
//! reports it into `document.title` and this reads it back, across the window
//! losing and regaining focus and across a tab switch.
//!
//! `cargo run --release -p cce-browser --example wpe_focus`

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
    let page = "data:text/html,<input autofocus><script>\
        setInterval(() => document.title = 'focus=' + document.hasFocus(), 50)\
        </script>";
    let mut host = wpe::WebKitHost::new(url::Url::parse(page).unwrap(), (400, 300));
    let settle = |h: &mut wpe::WebKitHost, n: u32| {
        for _ in 0..n {
            h.pump();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };

    let mut ok = true;
    let mut expect = |h: &mut wpe::WebKitHost, what: &str, want: bool| {
        settle(h, 10);
        let got = h.title();
        let pass = got.as_deref() == Some(if want { "focus=true" } else { "focus=false" });
        ok &= pass;
        println!("{what:<34} {got:?} {}", if pass { "OK" } else { "WRONG" });
    };

    settle(&mut host, 30);
    expect(&mut host, "loaded, window not focused", false);
    host.focus(true);
    expect(&mut host, "window focused", true);
    host.focus(false);
    expect(&mut host, "window unfocused", false);
    host.focus(true);
    expect(&mut host, "window refocused", true);

    // A tab opened while the window is focused must arrive focused too.
    host.open_tab(url::Url::parse(page).unwrap());
    settle(&mut host, 30);
    expect(&mut host, "new tab", true);
    host.activate(0);
    expect(&mut host, "back to the first tab", true);

    println!("\nfocus: {}", if ok { "OK" } else { "BROKEN" });
    std::process::exit(if ok { 0 } else { 1 });
}
