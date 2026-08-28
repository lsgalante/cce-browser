//! Verifies force-dark actually inverts, by sampling the rendered frame.
//! `cargo run --release -p cce-browser --features wpe --example wpe_dark`

#[cfg(not(feature = "wpe"))]
fn main() { eprintln!("build with --features wpe"); }

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
    // A deliberately hardcoded-white page: the case force-dark exists for.
    let url = "data:text/html,<body style='background:%23ffffff'><h1 style='color:%23000'>hello</h1></body>";
    let mut host = wpe::WebKitHost::new(url::Url::parse(url).unwrap(), (400, 300));
    let settle = |h: &mut wpe::WebKitHost, n: u32| {
        for _ in 0..n { h.pump(); std::thread::sleep(std::time::Duration::from_millis(50)); }
    };

    settle(&mut host, 30);
    let before = host.sample_pixel();
    println!("light: top-left pixel = {before:?}");

    println!("-- enabling force-dark --");
    host.set_force_dark(true);
    settle(&mut host, 40);
    let after = host.sample_pixel();
    println!("dark:  top-left pixel = {after:?}");

    match (before, after) {
        (Some(b), Some(a)) => {
            let lum = |p: (u8, u8, u8)| p.0 as u32 + p.1 as u32 + p.2 as u32;
            println!("\nluminance {} -> {}", lum(b), lum(a));
            println!("force-dark: {}", if lum(a) < lum(b) / 2 { "OK (page darkened)" } else { "NO EFFECT" });
        }
        _ => println!("no frame sampled"),
    }
}
