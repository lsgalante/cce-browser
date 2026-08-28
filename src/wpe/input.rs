//! Translating cce-ui input into `WPEEvent`s.
//!
//! Unlike the Servo backend — where `main.rs` carried `dom_key`/`dom_button`
//! helpers and the host took engine types — the mapping lives *here* and
//! [`super::WebKitHost`] takes cce-ui's own `MouseButton` / `KeyEvent`. That
//! keeps engine vocabulary out of the chrome, so switching backends deletes
//! those helpers from `main.rs` rather than rewriting them.
//!
//! **Keyboard is the fiddly part.** `wpe_event_keyboard_new` wants an X11
//! *keysym* (`keyval`), not a character. Latin-1 codepoints are their own
//! keysym; anything above maps to `codepoint + 0x0100_0000`; named keys have
//! fixed `XK_*` values. Getting this wrong is silent — the page just receives
//! nothing useful.

use cce_ui::widget::{ElementState, Key, KeyEvent, MouseButton, NamedKey};

use super::ffi::*;

/// X11 keysyms for the named keys cce-ui reports (`/usr/include/X11/keysymdef.h`).
mod keysym {
    pub const BACKSPACE: u32 = 0xff08;
    pub const TAB: u32 = 0xff09;
    pub const RETURN: u32 = 0xff0d;
    pub const ESCAPE: u32 = 0xff1b;
    pub const SPACE: u32 = 0x0020;
    pub const HOME: u32 = 0xff50;
    pub const LEFT: u32 = 0xff51;
    pub const UP: u32 = 0xff52;
    pub const RIGHT: u32 = 0xff53;
    pub const DOWN: u32 = 0xff54;
    pub const PAGE_UP: u32 = 0xff55;
    pub const PAGE_DOWN: u32 = 0xff56;
    pub const END: u32 = 0xff57;
    pub const DELETE: u32 = 0xffff;
    pub const F5: u32 = 0xffc2;
    pub const SHIFT_L: u32 = 0xffe1;
    pub const CONTROL_L: u32 = 0xffe3;
    pub const ALT_L: u32 = 0xffe9;
    pub const SUPER_L: u32 = 0xffeb;
}

/// A Unicode scalar as an X11 keysym: Latin-1 is identity, the rest is the
/// codepoint in the 0x01000000 plane.
fn unicode_keysym(c: char) -> u32 {
    match c as u32 {
        cp @ 0x20..=0xff => cp,
        cp => cp + 0x0100_0000,
    }
}

/// cce-ui key -> X11 keysym. `None` for keys with no sensible mapping.
pub(super) fn keyval(key: &Key) -> Option<u32> {
    Some(match key {
        Key::Character(s) => unicode_keysym(s.chars().next()?),
        Key::Named(n) => match n {
            NamedKey::Backspace => keysym::BACKSPACE,
            NamedKey::Tab => keysym::TAB,
            NamedKey::Enter => keysym::RETURN,
            NamedKey::Escape => keysym::ESCAPE,
            NamedKey::Space => keysym::SPACE,
            NamedKey::ArrowDown => keysym::DOWN,
            NamedKey::ArrowLeft => keysym::LEFT,
            NamedKey::ArrowRight => keysym::RIGHT,
            NamedKey::ArrowUp => keysym::UP,
            NamedKey::End => keysym::END,
            NamedKey::Home => keysym::HOME,
            NamedKey::PageDown => keysym::PAGE_DOWN,
            NamedKey::PageUp => keysym::PAGE_UP,
            NamedKey::Delete => keysym::DELETE,
            NamedKey::Control => keysym::CONTROL_L,
            NamedKey::Shift => keysym::SHIFT_L,
            NamedKey::Alt => keysym::ALT_L,
            NamedKey::Super => keysym::SUPER_L,
            NamedKey::F5 => keysym::F5,
        },
    })
}

/// X11 button numbering, which is what WPE expects.
pub(super) fn button_number(b: MouseButton) -> Option<u32> {
    Some(match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        // Back/Forward are chrome navigation in `main.rs`, deliberately not
        // forwarded to the page.
        _ => return None,
    })
}

pub(super) fn modifiers(ctrl: bool, shift: bool, alt: bool) -> WPEModifiers::Type {
    let mut m: WPEModifiers::Type = 0;
    if ctrl {
        m |= WPEModifiers::WPE_MODIFIER_KEYBOARD_CONTROL;
    }
    if shift {
        m |= WPEModifiers::WPE_MODIFIER_KEYBOARD_SHIFT;
    }
    if alt {
        m |= WPEModifiers::WPE_MODIFIER_KEYBOARD_ALT;
    }
    m
}

pub(super) fn is_pressed(e: &KeyEvent) -> bool {
    e.state == ElementState::Pressed
}

/// WPE stamps events with a millisecond clock. It only has to be monotonic
/// and consistent — `wpe_view_compute_press_count` uses it for double-click
/// detection, so a frozen value would turn every click into a triple-click.
pub(super) fn now_ms() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0)
}
