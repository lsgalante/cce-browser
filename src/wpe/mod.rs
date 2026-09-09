//! The WPE WebKit engine backend (feature `wpe`, off by default).
//!
//! Mirrors `webview.rs`'s `ServoHost` surface so `main.rs` can swap engines
//! with minimal churn — see WPE-PORT.md. Nothing here is wired into the app
//! yet; the shipping browser is still Servo.

pub mod ffi {
    #![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code)]
    include!(concat!(env!("OUT_DIR"), "/wpe_bindings.rs"));
}

mod subclass;
mod input;
mod glib_source;
mod host;
/// The page half of account autocomplete: the watcher script, the fill
/// script, and the events they exchange with the chrome.
pub mod formwatch;

// Not consumed yet — main.rs still drives ServoHost.
#[allow(unused_imports)]
pub use formwatch::FormEvent;
pub use host::{ContextMenuInfo, PendingAuth, PendingDialog, Tab, WebKitHost};
