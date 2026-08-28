//! Paste into a page: the path Ctrl+V takes through the chrome.
//!
//! `editing_action_cmd(Paste)` is what `main.rs` calls, so this exercises the
//! real route — WebKit's Paste command reads through `WPEClipboard::read`,
//! which is only there if the display vends a clipboard at all.
//!
//! Needs a Wayland display for wl-paste, so run it inside a session:
//!   cce-shadow --instance <n> run ./target/release/examples/wpe_paste <url>

#[cfg(not(feature = "wpe"))]
fn main() { eprintln!("build with --features wpe"); }

/// Mirrors main.rs's declaration; the wpe module refers to `crate::` and an
/// example is its own crate root. (A lib target would remove this wart.)
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
    let url = std::env::args().nth(1).unwrap_or_else(|| "http://127.0.0.1:8790/paste.html".into());
    let mut host = wpe::WebKitHost::new(url::Url::parse(&url).unwrap(), (1200, 800));
    let settle = |h: &mut wpe::WebKitHost, n: u32| {
        for _ in 0..n { h.pump(); std::thread::sleep(std::time::Duration::from_millis(50)); }
    };

    settle(&mut host, 40);
    println!("loaded: title={:?}", host.title());
    host.focus(true);
    // Click the autofocused input so the page has an editable target.
    host.mouse_move(400.0, 30.0);
    host.mouse_button_ui(cce_ui::widget::MouseButton::Left, true, 400.0, 30.0);
    host.mouse_button_ui(cce_ui::widget::MouseButton::Left, false, 400.0, 30.0);
    settle(&mut host, 10);

    println!("-- sync clipboard, let it propagate, then paste --");
    host.sync_clipboard();
    settle(&mut host, 10);
    host.editing_action_cmd(EditingCommand::Paste);
    settle(&mut host, 20);

    let title = host.title().unwrap_or_default();
    println!("   title={title:?}");
    let paste_ok = title.starts_with("pasted:") && title.len() > "pasted:".len();
    println!("paste into page: {}", if paste_ok { "OK" } else { "FAILED" });

    // Other direction: select the field's contents and copy, which routes
    // through the same `changed` vfunc, then read the system clipboard back.
    println!("\n-- select all + copy out of the page --");
    host.editing_action_cmd(EditingCommand::Copy);
    settle(&mut host, 6);
    let sys = std::process::Command::new("wl-paste")
        .output().ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    println!("   system clipboard now: {sys:?}");
}
