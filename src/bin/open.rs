//! `cce-browser-open` — the launch-latency half of single-instance.
//!
//! The desktop entry's `Exec` points here, not at `cce-browser`: forwarding a
//! link through the full browser binary costs ~20ms of dynamic-library loading
//! (libWPEWebKit and friends) before `main()` runs, all on the click-to-tab
//! path. This bin exists to link nothing, forward in a few ms, and only
//! `exec` the real browser when no instance answers.
//!
//! It therefore deliberately duplicates the client half of the socket
//! protocol instead of importing it: `src/instance.rs` (same crate) is the
//! server side and the fallback client, `cce_ui::ipc::socket_path` is the
//! path convention. All three must agree on `/tmp/cce-browser-<display>.sock`
//! and the `open <arg>` / `new-tab` lines. The protocol is small on purpose;
//! change it in both files or not at all.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;

fn socket_path() -> String {
    // Mirrors cce_ui::ipc::socket_path("cce-browser") — not imported, so this
    // bin stays free of cce-ui's native link flags.
    match std::env::var("WAYLAND_DISPLAY") {
        Ok(d) if !d.is_empty() => format!("/tmp/cce-browser-{d}.sock"),
        _ => "/tmp/cce-browser.sock".to_string(),
    }
}

/// One forwarding attempt; false on any failure. Mirrors
/// `instance::try_forward`, ack wait included — exiting on write alone races
/// the instance actually reading the line.
fn try_forward(arg: Option<&str>) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket_path()) else {
        return false;
    };
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
            format!("open {}\n", abs.as_deref().unwrap_or(a))
        }
        None => "new-tab\n".to_string(),
    };
    if stream.write_all(command.as_bytes()).is_err() {
        return false;
    }
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).is_ok()
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
