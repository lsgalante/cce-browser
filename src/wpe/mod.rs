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

// Not consumed yet — main.rs still drives ServoHost.
#[allow(unused_imports)]
pub use host::{Tab, WebKitHost};
