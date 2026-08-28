//! Tabs: several views on one display, and — the point of the change —
//! **background tabs updating their own state**.
//!
//! The old polling read only the active webview, so a tab loading in the
//! background stayed titleless until you switched to it. Here tab 1 is opened
//! and immediately backgrounded; if signals work it still reports its title.
//!
//! `cargo run --release -p cce-browser --features wpe --example wpe_tabs`

#[cfg(not(feature = "wpe"))]
fn main() {
    eprintln!("build with --features wpe");
}

#[cfg(feature = "wpe")]
#[path = "../src/wpe/mod.rs"]
mod wpe;

#[cfg(feature = "wpe")]
fn main() {
    let mut host = wpe::WebKitHost::new(
        url::Url::parse("https://example.com").unwrap(),
        (1200, 800),
    );
    let settle = |h: &mut wpe::WebKitHost, n: u32| {
        for _ in 0..n {
            h.pump();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    };
    let dump = |h: &wpe::WebKitHost, label: &str| {
        println!("{label}  (active={} of {})", h.active_index(), h.tab_count());
        for i in 0..h.tab_count() {
            let t = h.tab(i).unwrap();
            println!(
                "   tab{i}: title={:?} url={:?} loading={}",
                t.title,
                t.url.as_ref().map(|u| u.as_str()),
                t.loading
            );
        }
    };

    settle(&mut host, 30);
    dump(&host, "-- one tab --");

    println!("\n-- open tab 1, then immediately background it --");
    host.open_tab(url::Url::parse("https://example.org").unwrap());
    host.activate(0); // switch away before it can finish loading
    settle(&mut host, 40);
    dump(&host, "   after settling (tab1 was never active again)");

    let bg_ok = host.tab(1).map(|t| t.title.is_some() && !t.loading).unwrap_or(false);
    println!("\n   background tab reported state: {}", if bg_ok { "OK" } else { "MISSING" });

    println!("\n-- switch to tab 1 --");
    host.activate(1);
    settle(&mut host, 10);
    println!("   active title={:?} image={:?}", host.title(), host.image());

    println!("\n-- close tab 1 --");
    assert!(host.close_tab(1), "should not be the last tab");
    settle(&mut host, 6);
    dump(&host, "   after close");

    println!("\n-- close the last tab --");
    let more = host.close_tab(0);
    println!("   close_tab returned {more} (false = was the last)");
    assert!(!more);
    println!("\nOK — no crash through open/background/switch/close");
}
