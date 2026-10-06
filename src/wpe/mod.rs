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
/// What a frame changed, so only that much is read back and uploaded.
mod damage;
/// Which page text field is open, for the on-screen keyboard.
mod ime;
mod input;
mod glib_source;
mod host;
/// The page half of account autocomplete: the watcher script and the events
/// it exchanges with the chrome.
pub mod formwatch;

// Not consumed yet — main.rs still drives ServoHost.
#[allow(unused_imports)]
pub use formwatch::FormEvent;
pub use host::{
    ContextMenuInfo, OptionItem, OptionMenuInfo, PendingAuth, PendingDialog, Tab, WebKitHost,
};
