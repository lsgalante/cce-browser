//! Exercises `WebKitHost` — the real API surface `main.rs` will consume,
//! not the raw FFI. Proves boot → pump → frames → page state.
//!
//! `cargo run --release -p cce-browser --features wpe --example wpe_host`

#[cfg(not(feature = "wpe"))]
fn main() {
    eprintln!("build with --features wpe");
}

#[cfg(feature = "wpe")]
#[path = "../src/wpe/mod.rs"]
mod wpe;

#[cfg(feature = "wpe")]
fn main() {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "https://example.com".into());
    let mut host = wpe::WebKitHost::new(url::Url::parse(&url).unwrap(), (1200, 800));

    let mut frames = 0;
    let mut navigated = false;
    for i in 0..60 {
        // Once the first page settles, navigate and come back — exercises
        // load / can_go_back / back on a live history.
        if !navigated && !host.loading() && frames > 1 && i > 10 {
            navigated = true;
            println!("-- navigating to example.org --");
            host.load(url::Url::parse("https://example.org").unwrap());
        }
        if navigated && i == 40 {
            println!("-- back (can_go_back={}) --", host.can_go_back());
            host.back();
        }
        let (new_frame, dirty) = host.pump();
        if new_frame {
            frames += 1;
            println!(
                "t={:>4}ms frame#{frames} image={:?}",
                i * 100,
                host.image()
            );
        }
        if dirty {
            println!(
                "  state: title={:?} url={:?} loading={} back={} fwd={}",
                host.title(),
                host.url().map(|u| u.to_string()),
                host.loading(),
                host.can_go_back(),
                host.can_go_forward()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    println!(
        "done: {frames} frames, tabs={} active={}",
        host.tab_count(),
        host.active_index()
    );
    assert!(frames > 0, "no frames produced");
}
