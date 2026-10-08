//! `cce-browser-open` — the launch-latency half of single-instance.
//!
//! The desktop entry's `Exec` points here, not at `cce-browser`: forwarding a
//! link through the full browser binary costs ~20ms of dynamic-library loading
//! (libWPEWebKit and friends) before `main()` runs, all on the click-to-tab
//! path. This bin exists to link nothing, forward in a few ms, and only
//! `exec` the real browser when no instance answers.
//!
//! It takes the client half of the socket protocol from `cce_core::ipc`
//! (the GUI-free half of the toolkit, which has no native libraries): the
//! path convention and the one-line forward with its ack wait. `src/instance.rs`
//! (same crate, on `cce_ui::ipc::instance`, the same module) is the server side.
//! Both must agree on the `open <arg>` / `new-tab` lines. Until 2026-10-07 this
//! file copied both halves, to stay clear of cce-ui's native link flags.

use std::os::unix::process::CommandExt;

/// One forwarding attempt; false on any failure. A stuck instance does not
/// hang the click: `forward` bounds its wait for the ack.
fn try_forward(arg: Option<&str>) -> bool {
    let command = match arg {
        Some(a) => {
            // A relative file path is resolved against *this* process's cwd —
            // the instance's differs, so it must travel absolute.
            let p = std::path::Path::new(a);
            let abs = if p.exists() {
                std::fs::canonicalize(p)
                    .ok()
                    .and_then(|c| c.to_str().map(String::from))
            } else {
                None
            };
            format!("open {}", abs.as_deref().unwrap_or(a))
        }
        None => "new-tab".to_string(),
    };
    cce_core::ipc::instance::forward("cce-browser", &command).is_some()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if try_forward(args.first().map(String::as_str)) {
        return;
    }
    // No instance answered: become one. PATH resolution matches the desktop
    // entry convention (~/.local/bin first); the browser runs its own
    // forward_or_claim, which settles any launch race from here on.
    let err = std::process::Command::new("cce-browser").args(&args).exec();
    eprintln!("cce-browser-open: could not exec cce-browser: {err}");
    std::process::exit(1);
}
