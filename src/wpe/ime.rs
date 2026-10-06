//! Which page text field is open for typing, as WebKit itself reports it.
//!
//! WebKit tells the embedder about an editable element through an input-method
//! context: `focus_in` when one takes focus in a focused view, `focus_out` when
//! it loses it, `set_cursor_area` as its caret moves. It asks the *display* for
//! that context (`create_input_method_context`), so supplying our own is all it
//! takes to hear about a field in any page, frame or shadow root — `<input>`,
//! `<textarea>` and `contenteditable` alike — with no script injected.
//!
//! The browser claims the field from `display_list` with it
//! (`cce_ui::text_input::claim`), which is what raises the compositor's
//! on-screen keyboard after a tap, exactly as it does for the chrome's own
//! fields.
//!
//! Only the announcements are taken. Every other vfunc is left to the base
//! class, so key events are not filtered and reach the page as they did before
//! this context existed.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;

use super::ffi::*;

/// A page field with focus: its caret in the view's logical px, once WebKit
/// has said where it is.
#[derive(Clone, Copy, Default)]
struct Field {
    view: usize,
    focused: bool,
    caret: Option<(i32, i32, i32, i32)>,
}

thread_local! {
    /// Per context, keyed by its pointer. A context is one view's for life.
    static FIELDS: RefCell<Vec<(usize, Field)>> = const { RefCell::new(Vec::new()) };
    /// A field opened, closed or moved since the browser last asked.
    static CHANGED: Cell<bool> = const { Cell::new(false) };
}

static mut TYPE: GType = 0;
static mut PARENT_FINALIZE: Option<unsafe extern "C" fn(*mut GObject)> = None;

unsafe fn context_type() -> GType {
    if TYPE == 0 {
        TYPE = super::subclass::register_subclass(
            wpe_input_method_context_get_type(),
            "CceWpeInputMethodContext",
            class_init,
        );
    }
    TYPE
}

fn update(ctx: *mut WPEInputMethodContext, f: impl FnOnce(&mut Field)) {
    let view = unsafe { wpe_input_method_context_get_view(ctx) } as usize;
    FIELDS.with(|m| {
        let mut m = m.borrow_mut();
        let at = match m.iter().position(|(c, _)| *c == ctx as usize) {
            Some(at) => at,
            None => {
                m.push((ctx as usize, Field { view, ..Field::default() }));
                m.len() - 1
            }
        };
        f(&mut m[at].1);
    });
    CHANGED.set(true);
}

unsafe extern "C" fn focus_in(ctx: *mut WPEInputMethodContext) {
    log::debug!("page field: focus in");
    update(ctx, |f| f.focused = true);
}

unsafe extern "C" fn focus_out(ctx: *mut WPEInputMethodContext) {
    log::debug!("page field: focus out");
    update(ctx, |f| {
        f.focused = false;
        f.caret = None;
    });
}

unsafe extern "C" fn set_cursor_area(
    ctx: *mut WPEInputMethodContext,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    update(ctx, |f| f.caret = Some((x, y, w, h)));
}

unsafe extern "C" fn finalize(obj: *mut GObject) {
    FIELDS.with(|m| m.borrow_mut().retain(|(c, _)| *c != obj as usize));
    CHANGED.set(true);
    if let Some(parent) = PARENT_FINALIZE {
        parent(obj);
    }
}

unsafe extern "C" fn class_init(class: *mut c_void, _data: *mut c_void) {
    let c = class as *mut WPEInputMethodContextClass;
    (*c).focus_in = Some(focus_in);
    (*c).focus_out = Some(focus_out);
    (*c).set_cursor_area = Some(set_cursor_area);
    PARENT_FINALIZE = (*c).parent_class.finalize;
    (*c).parent_class.finalize = Some(finalize);
}

/// `WPEDisplayClass::create_input_method_context`.
pub(super) unsafe extern "C" fn create_context(
    _d: *mut WPEDisplay,
    view: *mut WPEView,
) -> *mut WPEInputMethodContext {
    let prop = std::ffi::CString::new("view").unwrap();
    g_object_new(context_type(), prop.as_ptr(), view, std::ptr::null::<std::ffi::c_char>())
        as *mut WPEInputMethodContext
}

/// The open field in `view`, if one is: its caret's rectangle in the view's
/// logical px, or `None` inside when WebKit has not placed the caret yet.
///
/// A field WebKit marks `INHIBIT_OSK` (the page draws its own keyboard, or
/// wants none) is not reported. The hints are read now rather than at
/// `focus_in`, because moving from one field to the next says no focus in
/// or out, only a new caret. WebKit 2.52 does not set that hint yet:
/// `inputmode="none"` arrives as plain `SPELLCHECK` (measured), so such a
/// field is claimed like any other until it does.
pub(super) fn field(view: *mut WPEView) -> Option<Option<(i32, i32, i32, i32)>> {
    let (ctx, field) = FIELDS.with(|m| {
        m.borrow().iter().find(|(_, f)| f.view == view as usize && f.focused).copied()
    })?;
    let hints = unsafe { wpe_input_method_context_get_input_hints(ctx as *mut WPEInputMethodContext) };
    if hints & WPEInputHints::WPE_INPUT_HINT_INHIBIT_OSK != 0 {
        return None;
    }
    Some(field.caret)
}

/// Whether any field opened, closed or moved since the last call.
pub(super) fn take_changed() -> bool {
    CHANGED.replace(false)
}
