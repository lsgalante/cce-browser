//! Single-instance forwarding over the CCE socket convention.
//!
//! Every external open (`xdg-open` via the desktop entry's `%u`) spawns a
//! fresh `cce-browser <url>` process. A second full instance is not just
//! clutter: the profile dir holds a plaintext cookie jar two engines must
//! not share. So the first instance listens on
//! `/tmp/cce-browser-<WAYLAND_DISPLAY>.sock` (keyed by display, which keeps
//! shadow sessions isolated for free), and every later launch hands its
//! argument to it and exits before any Wayland or engine work happens.
//!
//! The claim, the race it closes, the parked listener and the bounded reads
//! are `cce_ui::ipc::instance`'s; this module is the browser's protocol on
//! top: `open <arg>` / `new-tab`, each answered `ok`. `src/bin/open.rs`
//! speaks the same protocol without linking cce-ui — keep the two in step.

use crate::Message;

/// Socket prefix; `cce_ui::ipc::socket_path` appends `-<WAYLAND_DISPLAY>`.
const PREFIX: &str = "cce-browser";

/// Hand `arg` to a running instance, or claim the instance socket.
///
/// Returns `true` when a running instance took the launch (the caller should
/// exit without starting the engine). Returns `false` when this process is
/// the instance — with the listener parked for [`spawn_listener`] — or when
/// single-instance handling failed entirely and the launch should proceed
/// standalone.
pub fn forward_or_claim(arg: Option<&str>) -> bool {
    cce_ui::ipc::instance::forward_or_claim(PREFIX, &launch_line(arg))
}

/// The line a launch sends. A relative file path is resolved against *this*
/// process's cwd — the instance's differs, so it must travel absolute.
fn launch_line(arg: Option<&str>) -> String {
    let Some(a) = arg else {
        return "new-tab".to_string();
    };
    let p = std::path::Path::new(a);
    let abs = if p.exists() {
        std::fs::canonicalize(p).ok().and_then(|c| c.to_str().map(String::from))
    } else {
        None
    };
    format!("open {}", abs.as_deref().unwrap_or(a))
}

/// Serve the listener claimed in `main()`, pushing each received launch into
/// the app's calloop channel. No-op when this process runs standalone.
pub fn spawn_listener(sender: calloop::channel::Sender<Message>) {
    cce_ui::ipc::instance::serve(move |line| {
        let msg = parse_command(line)?;
        sender.send(msg).ok()?;
        Some("ok".into())
    });
}

/// `open <arg>` / `new-tab` → the message the app loop handles.
fn parse_command(line: &str) -> Option<Message> {
    if let Some(arg) = line.strip_prefix("open ") {
        let arg = arg.trim();
        (!arg.is_empty()).then(|| Message::OpenExternal(Some(arg.to_string())))
    } else if line == "new-tab" {
        Some(Message::OpenExternal(None))
    } else {
        None
    }
}

/// Unlink the socket if this process bound it. Called after the engine loop
/// returns.
pub fn cleanup() {
    cce_ui::ipc::instance::cleanup();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse() {
        assert!(matches!(
            parse_command("open https://example.com"),
            Some(Message::OpenExternal(Some(u))) if u == "https://example.com"
        ));
        assert!(matches!(
            parse_command("new-tab"),
            Some(Message::OpenExternal(None))
        ));
        assert!(parse_command("open ").is_none());
        assert!(parse_command("bogus").is_none());
    }

    #[test]
    fn launch_lines_round_trip() {
        assert_eq!(launch_line(None), "new-tab");
        assert_eq!(launch_line(Some("https://example.com")), "open https://example.com");
        assert!(matches!(
            parse_command(&launch_line(Some("https://example.com"))),
            Some(Message::OpenExternal(Some(u))) if u == "https://example.com"
        ));
        // A path that exists here travels absolute.
        let line = launch_line(Some("Cargo.toml"));
        assert!(line.starts_with("open /") && line.ends_with("/Cargo.toml"), "{line}");
    }
}
