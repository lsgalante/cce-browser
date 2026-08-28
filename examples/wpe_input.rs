//! Proves input actually reaches the page.
//!
//! Loads a page that appends every event it receives to `document.title`,
//! then drives `WebKitHost`'s input methods and reads the title back — which
//! works because page-state sync is already wired. No JS-evaluation API needed.
//!
//! `cargo run --release -p cce-browser --features wpe --example wpe_input -- <url>`

#[cfg(not(feature = "wpe"))]
fn main() {
    eprintln!("build with --features wpe");
}

#[cfg(feature = "wpe")]
#[path = "../src/wpe/mod.rs"]
mod wpe;

#[cfg(feature = "wpe")]
fn main() {
    use cce_ui::widget::{ElementState, Key, KeyEvent, MouseButton};

    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:8750/input.html".into());
    let mut host = wpe::WebKitHost::new(url::Url::parse(&url).unwrap(), (1200, 800));

    let settle = |h: &mut wpe::WebKitHost, n: u32| {
        for _ in 0..n {
            h.pump();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };

    settle(&mut host, 40);
    println!("loaded: title={:?}", host.title());
    host.focus(true);

    println!("-- pointer move + left click at (300,220) --");
    host.mouse_move(300.0, 220.0);
    settle(&mut host, 4);
    host.mouse_button(MouseButton::Left, true, 300.0, 220.0);
    host.mouse_button(MouseButton::Left, false, 300.0, 220.0);
    settle(&mut host, 8);
    println!("   title={:?}", host.title());

    println!("-- key 'a' --");
    let key = |c: &str, pressed: bool| KeyEvent {
        state: if pressed { ElementState::Pressed } else { ElementState::Released },
        logical_key: Key::Character(c.into()),
        text: Some(c.into()),
        repeat: false,
        ctrl: false,
        shift: false,
        alt: false,
    };
    host.key(&key("a", true));
    host.key(&key("a", false));
    settle(&mut host, 8);
    println!("   title={:?}", host.title());

    println!("-- wheel down --");
    host.wheel(0.0, -120.0, 300.0, 220.0);
    settle(&mut host, 8);
    let title = host.title().unwrap_or_default();
    println!("   title={title:?}");

    println!("\n=== RESULT ===");
    for (label, needle) in [
        ("pointer move", "move"),
        ("button down", "down0"),
        ("button up", "up0"),
        ("click", "click@300,220"),
        ("keydown 'a'", "key:a"),
        ("wheel", "wheel:"),
    ] {
        println!("  {:<14} {}", label, if title.contains(needle) { "OK" } else { "MISSING" });
    }
}
