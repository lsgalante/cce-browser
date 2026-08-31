//! Right-click → context-menu signal → hit-test info, end to end.
//! `cce-shadow --instance <n> run ./target/release/examples/wpe_ctx`

#[cfg(not(feature = "wpe"))]
fn main() { eprintln!("build with --features wpe"); }

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
    use cce_ui::widget::MouseButton;
    let url = "http://127.0.0.1:8795/ctx.html";
    let mut host = wpe::WebKitHost::new(url::Url::parse(url).unwrap(), (1200, 800));
    let settle = |h: &mut wpe::WebKitHost, n: u32| {
        for _ in 0..n { h.pump(); std::thread::sleep(std::time::Duration::from_millis(50)); }
    };
    settle(&mut host, 30);
    println!("loaded: {:?}", host.title());

    let rclick = |h: &mut wpe::WebKitHost, x: f32, y: f32| {
        h.mouse_move(x, y);
        h.mouse_button_ui(MouseButton::Right, true, x, y);
        h.mouse_button_ui(MouseButton::Right, false, x, y);
    };

    println!("-- right-click the link (150,40) --");
    rclick(&mut host, 150.0, 40.0);
    settle(&mut host, 8);
    let on_link = host.take_context_menu();
    println!("   {on_link:?}");

    println!("-- right-click the input (150,175) --");
    rclick(&mut host, 150.0, 175.0);
    settle(&mut host, 8);
    let on_input = host.take_context_menu();
    println!("   {on_input:?}");

    println!("\n=== RESULT ===");
    let link_ok = on_link.as_ref().and_then(|i| i.link.as_ref())
        .is_some_and(|(u, _)| u == "https://example.org/target");
    println!("  link uri      {}", if link_ok { "OK" } else { "MISSING" });
    println!("  editable flag {}", if on_input.is_some_and(|i| i.is_editable) { "OK" } else { "MISSING" });
}
