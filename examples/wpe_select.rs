//! Proves a press-drag-release over text selects it.
//!
//! WebKit decides whether a pointer move is a drag from the button bits in
//! the move event's modifiers, not from the press it saw earlier; a move
//! that reports no held button is a hover, and nothing is selected. The page
//! mirrors `getSelection()` into `document.title`, which page-state sync
//! already reads back.
//!
//! `cargo run --release -p cce-browser --example wpe_select`

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
fn main() {
    use cce_ui::widget::MouseButton;

    let page = "data:text/html,<html><body style='margin:0;font:40px monospace'>\
        <p id=t style='margin:0'>alpha bravo charlie delta echo</p>\
        <script>document.title='sel:';\
        document.addEventListener('selectionchange',()=>\
        document.title='sel:'+getSelection().toString())</script></body></html>";
    let mut host = wpe::WebKitHost::new(url::Url::parse(page).unwrap(), (1200, 800));
    let settle = |h: &mut wpe::WebKitHost, n: u32| {
        for _ in 0..n {
            h.pump();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };
    settle(&mut host, 40);
    host.focus(true);

    // Press at the start of the line, drag right in steps, release.
    host.mouse_move(2.0, 20.0);
    settle(&mut host, 2);
    host.mouse_button_ui(MouseButton::Left, true, 2.0, 20.0);
    settle(&mut host, 2);
    for x in (20..=500).step_by(40) {
        host.mouse_move(x as f32, 20.0);
        settle(&mut host, 1);
    }
    host.mouse_button_ui(MouseButton::Left, false, 500.0, 20.0);
    settle(&mut host, 8);

    let title = host.title().unwrap_or_default();
    println!("title={title:?}");
    let selected = title.strip_prefix("sel:").unwrap_or("");
    println!("{}", if selected.trim().is_empty() { "FAIL: nothing selected" } else { "OK: drag selected text" });
}
