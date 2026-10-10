//! The WPE WebKit engine backend (feature `wpe`, the default since
//! 2026-08-30).
//!
//! `WebKitHost` mirrors `webview.rs`'s `ServoHost` surface, so `main.rs`
//! drives either engine — see WPE-PORT.md.

// The embedding itself — bindings, WPEPlatform subclasses, the GLib
// source, input translation, frame damage, the input-method context — is
// the cce-wpe crate, shared with cce-mail. Re-exported under the module
// names `host.rs` has always used.
pub use cce_wpe::ffi;
use cce_wpe::{damage, glib_source, ime, input, subclass};
mod host;
/// The page half of account autocomplete: the watcher script and the events
/// it exchanges with the chrome.
pub mod formwatch;

// Not consumed yet — main.rs still drives ServoHost.
#[allow(unused_imports)]
pub use formwatch::FormEvent;
pub use host::{ContextMenuInfo, OptionItem, OptionMenuInfo, WebKitHost};
