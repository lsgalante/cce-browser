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
//! The order in [`forward_or_claim`] is what closes the startup race: try to
//! connect, and only bind after a connect has failed. A refused connection
//! means the socket file outlived a crashed instance and is removed before
//! binding; losing the bind to a simultaneous launch falls back to one more
//! connect. If that also fails the launch proceeds un-listened rather than
//! not at all.
//!
//! The claimed listener has to survive from `main()` (before the engine
//! starts) to `BrowserApp::new` (where the calloop sender first exists), so
//! it parks in a static until [`spawn_listener`] adopts it.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Mutex;

use crate::Message;

/// Socket prefix; `cce_ui::ipc::socket_path` appends `-<WAYLAND_DISPLAY>`.
const PREFIX: &str = "cce-browser";

/// The listener claimed by `forward_or_claim`, waiting for `spawn_listener`.
static CLAIMED: Mutex<Option<UnixListener>> = Mutex::new(None);
/// The socket path this process bound (and must unlink on exit), if any.
static OWNED_PATH: Mutex<Option<String>> = Mutex::new(None);

/// Hand `arg` to a running instance, or claim the instance socket.
///
/// Returns `true` when a running instance took the launch (the caller should
/// exit without starting the engine). Returns `false` when this process is
/// the instance — with the listener parked for [`spawn_listener`] — or when
/// single-instance handling failed entirely and the launch should proceed
/// standalone.
pub fn forward_or_claim(arg: Option<&str>) -> bool {
    let path = cce_ui::ipc::socket_path(PREFIX);

    if try_forward(&path, arg) {
        return true;
    }

    // Nothing answered. A socket file that still exists is a leftover from a
    // crashed instance; binding needs it gone.
    if std::path::Path::new(&path).exists() {
        let _ = std::fs::remove_file(&path);
    }
    match UnixListener::bind(&path) {
        Ok(listener) => {
            *CLAIMED.lock().unwrap() = Some(listener);
            *OWNED_PATH.lock().unwrap() = Some(path);
            false
        }
        // Lost the bind race to a simultaneous launch: it is the instance.
        Err(_) => try_forward(&path, arg),
    }
}

/// One forwarding attempt. False on any failure — there is no retry inside.
fn try_forward(path: &str, arg: Option<&str>) -> bool {
    let Ok(mut stream) = UnixStream::connect(path) else {
        return false;
    };
    // A relative file path is resolved against *this* process's cwd — the
    // instance's differs, so it must travel absolute.
    let command = match arg {
        Some(a) => {
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
    // Wait for the ack: returning (and exiting) on write alone races the
    // instance actually reading the line.
    // Bounded: an instance whose listener is stuck must not hang this
    // launch forever; unanswered, the launch is not forwarded.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).is_ok()
}

/// Adopt the listener claimed in `main()` and serve it on a thread, pushing
/// each received launch into the app's calloop channel. No-op when this
/// process runs standalone.
pub fn spawn_listener(sender: calloop::channel::Sender<Message>) {
    let Some(listener) = CLAIMED.lock().unwrap().take() else {
        return;
    };
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let Some(line) = read_request_line(&conn, 64 * 1024, std::time::Duration::from_secs(2)) else {
                continue;
            };
            let msg = match parse_command(line.trim()) {
                Some(m) => m,
                None => continue,
            };
            if sender.send(msg).is_err() {
                return; // channel gone: the app is shutting down
            }
            let _ = (&conn).write_all(b"ok\n");
        }
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
/// returns; a crash skips it, which is what the stale-socket removal in
/// [`forward_or_claim`] exists for.
pub fn cleanup() {
    if let Some(path) = OWNED_PATH.lock().unwrap().take() {
        let _ = std::fs::remove_file(path);
    }
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
}

/// One request line from a control-socket client, bounded in size and in
/// TOTAL time. Until 2026-10-02 this was `BufReader::read_line` on a socket
/// with no timeout at all, so a client that connected and said nothing (or trickled a
/// byte at a time) held the listener thread for good, and every later
/// launch forwarded to it (a link opened from another app) waited behind. None on EOF before any byte, timeout,
/// overflow or a read error.
fn read_request_line(conn: &std::os::unix::net::UnixStream, limit: usize, deadline: std::time::Duration) -> Option<String> {
    use std::io::Read;
    let until = std::time::Instant::now() + deadline;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut reader = conn;
    loop {
        let left = until.checked_duration_since(std::time::Instant::now()).filter(|d| !d.is_zero())?;
        conn.set_read_timeout(Some(left)).ok()?;
        let n = reader.read(&mut chunk).ok()?;
        if n == 0 {
            if buf.is_empty() {
                return None;
            }
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = buf.iter().position(|&b| b == b'\n') {
            buf.truncate(end + 1);
            break;
        }
        if buf.len() > limit {
            return None;
        }
    }
    let _ = conn.set_read_timeout(None);
    String::from_utf8(buf).ok()
}
