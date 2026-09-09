//! The page half of account autocomplete: what the chrome knows about a
//! login form, and how a picked account gets into it.
//!
//! Both directions run in a **private script world** (`WORLD`), not the
//! page's. Two things follow, and they are the reason for the whole
//! arrangement: the page cannot see or replace the helpers this installs, so
//! it cannot hook the moment a credential is filled; and the message channel
//! the chrome listens on cannot be spoofed by page script, so a page cannot
//! make the chrome believe a login field is focused when none is.
//!
//! The script is injected into the **top frame only**. A password field
//! inside a cross-origin iframe therefore gets no suggestions — the deliberate
//! trade: such a frame cannot report a position in the top document's
//! coordinates, and an embedded frame asking for the embedder's credentials
//! is exactly the shape of the attack this feature must not enable.

/// The isolated world everything here lives in.
pub const WORLD: &str = "cce-accounts";
/// The message channel the injected script posts on.
pub const CHANNEL: &str = "cceAccounts";

/// Watches the top frame for login fields and reports them to the chrome.
///
/// It reports *positions*, *field kinds* and *what is typed* — never page
/// content at large. Rects are CSS pixels relative to the viewport, which the
/// chrome converts with the same scale it sized the view at.
pub const WATCH_JS: &str = r#"
(() => {
  const post = (m) => {
    try { window.webkit.messageHandlers.cceAccounts.postMessage(JSON.stringify(m)); }
    catch (e) {}
  };
  const state = { user: null, pass: null, filling: false };
  window.__cceAccounts = state;

  const isPassword = (el) =>
    el && el.tagName === 'INPUT' && el.type === 'password' && !el.disabled && !el.readOnly;
  // A username field is a text-ish input that keeps company with a password
  // one: same form, or — for the many login pages that use no form element —
  // anywhere on a page that has one. Autocomplete hints and the usual names
  // are accepted on their own, since some pages ask for the username first
  // and only render the password field on the next step.
  const textish = (el) =>
    el && el.tagName === 'INPUT' &&
    ['text', 'email', 'tel', ''].includes((el.type || '').toLowerCase()) &&
    !el.disabled && !el.readOnly;
  const named = (el) => {
    const hint = ((el.autocomplete || '') + ' ' + (el.name || '') + ' ' +
                  (el.id || '') + ' ' + (el.getAttribute('aria-label') || '')).toLowerCase();
    return /user|email|login|account|ident/.test(hint);
  };
  const passwordsIn = (root) =>
    Array.from((root || document).querySelectorAll('input[type=password]'))
         .filter(isPassword);

  const kindOf = (el) => {
    if (isPassword(el)) return 'pass';
    if (!textish(el)) return null;
    const form = el.form;
    if (passwordsIn(form).length) return 'user';
    if (named(el) && passwordsIn(document).length) return 'user';
    if (named(el) && el.type.toLowerCase() === 'email') return 'user';
    return null;
  };

  const rectOf = (el) => {
    const r = el.getBoundingClientRect();
    return [r.left, r.top, r.width, r.height];
  };

  const report = (el, kind, type) => {
    // A fill is not something to report back: the input events it dispatches
    // would arrive as "the user typed", re-opening the list that was just
    // used and filtering it by the name it had just filled in.
    if (state.filling) return;
    if (kind === 'pass') { state.pass = el; } else { state.user = el; }
    // Remember the pair, so filling reaches both fields from either one.
    const form = el.form;
    const pass = passwordsIn(form).concat(passwordsIn(document))[0] || null;
    if (pass) state.pass = pass;
    if (kind === 'user') state.user = el;
    post({
      t: type,
      kind: kind,
      origin: location.origin,
      rect: rectOf(el),
      value: kind === 'pass' ? '' : (el.value || ''),
    });
  };

  document.addEventListener('focusin', (e) => {
    const kind = kindOf(e.target);
    if (kind) report(e.target, kind, 'focus');
  }, true);

  document.addEventListener('focusout', (e) => {
    if (kindOf(e.target)) post({ t: 'blur' });
  }, true);

  // Typing in the username field is the filter; the password field's own
  // text is never reported.
  document.addEventListener('input', (e) => {
    const kind = kindOf(e.target);
    if (kind === 'user' && document.activeElement === e.target) {
      report(e.target, kind, 'input');
    }
  }, true);

  // The page moving under an open list would leave it pointing at nothing.
  const moved = () => {
    const el = document.activeElement;
    const kind = kindOf(el);
    if (kind) report(el, kind, 'move'); else post({ t: 'blur' });
  };
  window.addEventListener('scroll', moved, true);
  window.addEventListener('resize', moved, true);
  // The chrome calls this when it has something new to offer — the account
  // index finishing its first read, after a field was already focused.
  state.rescan = moved;
})();
"#;

/// Fill the remembered pair. Evaluated in [`WORLD`], so it reads the elements
/// the watcher recorded rather than trusting anything the page exposes.
///
/// Values go in through the prototype's own `value` setter and are followed by
/// `input` and `change` events: frameworks that track their inputs (React's
/// value tracker above all) ignore a plain assignment, and a page whose state
/// never saw the credential appear will submit an empty form.
///
/// It does not submit. Filling is the chrome's business; pressing the button
/// is the person's.
pub fn fill_js(username: &str, password: &str) -> String {
    format!(
        r#"
(() => {{
  const s = window.__cceAccounts || {{}};
  // Suppress the watcher for the duration: dispatching `input` is the whole
  // point of filling, and it must not come back as typing. Synchronous, so
  // the flag is down again before anything else runs.
  s.filling = true;
  const set = (el, v) => {{
    if (!el) return false;
    const d = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), 'value');
    if (d && d.set) {{ d.set.call(el, v); }} else {{ el.value = v; }}
    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
    return true;
  }};
  const user = {user};
  const pass = {pass};
  const filledUser = user.length ? set(s.user, user) : false;
  const filledPass = set(s.pass, pass);
  if (filledUser && !filledPass && s.user) {{ s.user.focus(); }}
  s.filling = false;
}})();
"#,
        user = json_string(username),
        pass = json_string(password),
    )
}

/// A JSON string literal — the only escaping this file needs, and it has to be
/// exact: a credential is about to cross into a script source, where a stray
/// quote would end the string and the rest would be parsed as code.
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Line separators are literal newlines to a JS parser.
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Ask the watcher to re-report whatever login field is focused right now.
///
/// The chrome needs this exactly once per page in practice: a field can take
/// focus before the account index has finished its first read, and without a
/// nudge nothing would report it again until the person clicked away and back.
pub const RESCAN_JS: &str =
    "window.__cceAccounts && window.__cceAccounts.rescan && window.__cceAccounts.rescan();";

/// What the watcher saw, as the chrome consumes it.
#[derive(Debug, Clone)]
pub enum FormEvent {
    /// A login field took focus, or moved, or its text changed.
    Field {
        /// `location.origin` of the frame that reported it, checked against
        /// the tab's own URL before anything is offered.
        origin: String,
        /// A password field rather than a username one.
        password: bool,
        /// Viewport rect in CSS pixels: x, y, width, height.
        rect: (f32, f32, f32, f32),
        /// What the username field holds, for filtering. Always empty for a
        /// password field — the chrome has no business with what is typed
        /// into one.
        value: String,
        /// True when this is a re-report of a field that was already focused
        /// (scroll, resize, typing) rather than a fresh focus.
        moved: bool,
    },
    /// Focus left the login field.
    Blur,
}

/// Parse one message from the watcher. Anything unexpected is dropped: this
/// is a channel the chrome acts on, so it takes only what it recognizes.
pub fn parse_event(json: &str) -> Option<FormEvent> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    match value["t"].as_str()? {
        "blur" => Some(FormEvent::Blur),
        t @ ("focus" | "input" | "move") => {
            let rect = value["rect"].as_array()?;
            let num = |i: usize| rect.get(i).and_then(|v| v.as_f64()).map(|f| f as f32);
            Some(FormEvent::Field {
                origin: value["origin"].as_str().unwrap_or_default().to_string(),
                password: value["kind"].as_str() == Some("pass"),
                rect: (num(0)?, num(1)?, num(2)?, num(3)?),
                value: value["value"].as_str().unwrap_or_default().to_string(),
                moved: t != "focus",
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_cannot_break_out_of_the_fill_script() {
        // The whole hazard in one string: quotes, a backslash, a closing
        // script tag, a newline and a line separator.
        let nasty = "a\"b\\c</script>\nd\u{2028}e";
        let quoted = json_string(nasty);
        assert_eq!(quoted, "\"a\\\"b\\\\c</script>\\nd\\u2028e\"");
        let js = fill_js("user", nasty);
        assert!(js.contains(&quoted));
        // No raw newline from the credential ever reaches the source.
        assert!(!js.contains("d\u{2028}"));
    }

    #[test]
    fn events_parse_and_junk_is_dropped() {
        let focus = parse_event(
            r#"{"t":"focus","kind":"user","origin":"https://example.com","rect":[10,20,120,24],"value":"me"}"#,
        );
        match focus {
            Some(FormEvent::Field { origin, password, rect, value, moved }) => {
                assert_eq!(origin, "https://example.com");
                assert!(!password);
                assert_eq!(rect, (10.0, 20.0, 120.0, 24.0));
                assert_eq!(value, "me");
                assert!(!moved);
            }
            other => panic!("expected a field event, got {other:?}"),
        }
        assert!(matches!(parse_event(r#"{"t":"blur"}"#), Some(FormEvent::Blur)));
        assert!(matches!(
            parse_event(r#"{"t":"input","kind":"pass","origin":"x","rect":[0,0,1,1],"value":""}"#),
            Some(FormEvent::Field { password: true, moved: true, .. })
        ));
        // Nonsense, and a field event with no rect, are both ignored.
        assert!(parse_event("not json").is_none());
        assert!(parse_event(r#"{"t":"focus","kind":"user"}"#).is_none());
        assert!(parse_event(r#"{"t":"evil"}"#).is_none());
    }
}
