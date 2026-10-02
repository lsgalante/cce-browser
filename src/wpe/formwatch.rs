//! The page half of account autocomplete: what the chrome knows about a
//! login form, how a picked account gets into it, and how a new sign-in is
//! offered for saving.
//!
//! Everything runs in a **private script world** (`WORLD`), not the page's.
//! Two things follow, and they are the reason for the whole arrangement: the
//! page cannot see or replace the helpers this installs, so it cannot hook the
//! moment a credential is filled; and the message channels the chrome listens
//! on cannot be spoofed by page script, so a page cannot make the chrome
//! believe a login field is focused when none is. Inside that world
//! `location.origin` is the frame's real origin, so what a watcher reports
//! about *where* it is can be believed.
//!
//! The watcher runs in **every frame**. Sign-in forms are often a frame of
//! their own — iCloud's is `idmsa.apple.com` inside `www.icloud.com` — and
//! that frame is where the credential actually goes, so it is the frame's
//! origin the chrome matches accounts against (see `on_form_event` in
//! `main.rs` for what else is offered there, and how it is labelled). Two
//! things a top-frame-only watcher got for free have to be rebuilt:
//!
//! * **Position.** A frame only knows its fields relative to its own
//!   viewport. Each frame names itself with a random token and announces it
//!   to its parent with `postMessage`; the parent's watcher finds which of its
//!   frames sent it (`event.source`), adds that frame's offset, and passes it
//!   up, until the top reports `{t: 'frame', offset}` to the chrome. Only
//!   geometry travels this way — the page could forge a relay, and the worst
//!   a forgery does is draw the list in the wrong place.
//! * **Filling.** The chrome can evaluate script only in the top frame. So a
//!   frame with a focused login field *asks* to be filled, on `FILL_CHANNEL`,
//!   a channel with a reply: the chrome holds the newest ask, and answers it
//!   with the picked credential or with nothing. The credential therefore
//!   goes to exactly the frame that asked, and never through the page.

/// The isolated world everything here lives in.
pub const WORLD: &str = "cce-accounts";
/// The message channel the injected script reports on.
pub const CHANNEL: &str = "cceAccounts";
/// The channel a frame asks to be filled on; its replies carry credentials.
pub const FILL_CHANNEL: &str = "cceAccountsFill";

/// Watches a frame for login fields and reports them to the chrome.
///
/// It reports *positions*, *field kinds* and *what is typed into a username
/// field* — never page content at large. The password crosses only on a
/// submit, as the thing being offered for saving. Rects are CSS pixels
/// relative to the frame's viewport; a frame's offset in the top document
/// arrives separately, as a `frame` event.
pub const WATCH_JS: &str = r#"
(() => {
  const handlers = window.webkit && window.webkit.messageHandlers;
  if (!handlers || !handlers.cceAccounts) return;
  const post = (m) => {
    try { handlers.cceAccounts.postMessage(JSON.stringify(m)); } catch (e) {}
  };
  const isTop = window === window.top;
  // The chrome's name for this document, random so that nothing can aim a
  // relay or a fill at a document it did not see announced. The top frame
  // gets one too: every tab's top frame shares these channels, and a fill
  // meant for this tab must not be answerable by another tab's page.
  const token = Array.from(crypto.getRandomValues(new Uint8Array(12)),
    (b) => b.toString(16).padStart(2, '0')).join('');
  // Every tab's watchers share the chrome's channels, and nothing on them says
  // which tab spoke. Focus does: only the shown tab's view is focused, so a
  // document without it is in a background tab (or a window the person left)
  // and has nothing to report or ask for. A page cannot fake this from its
  // own world.
  const live = () => document.hasFocus();
  const state = { user: null, pass: null, filling: false, typed: '', sent: '' };
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

  // ---- where this frame is, relayed up to the top ----

  // The child frame whose field is focused, as last announced through here:
  // a scroll in this document moves it, and only this document can say so.
  let child = null;
  const isFrame = (el) => el && (el.tagName === 'IFRAME' || el.tagName === 'FRAME');
  const relay = (el, frame, inner) => {
    const r = el.getBoundingClientRect();
    const cs = getComputedStyle(el);
    const dx = r.left + el.clientLeft + (parseFloat(cs.paddingLeft) || 0) + inner[0];
    const dy = r.top + el.clientTop + (parseFloat(cs.paddingTop) || 0) + inner[1];
    child = { el, frame, inner };
    if (isTop) {
      post({ t: 'frame', frame, offset: [dx, dy] });
    } else {
      window.parent.postMessage({ cceAccountsFrame: frame, dx, dy }, '*');
    }
  };
  const announce = () => {
    if (!isTop && live()) window.parent.postMessage({ cceAccountsFrame: token, dx: 0, dy: 0 }, '*');
  };
  window.addEventListener('message', (e) => {
    const d = e.data;
    if (!d || typeof d !== 'object' || typeof d.cceAccountsFrame !== 'string') return;
    // Ours: the page has no use for it, so it never sees it.
    e.stopImmediatePropagation();
    const el = Array.from(document.querySelectorAll('iframe, frame'))
      .find((f) => f.contentWindow === e.source);
    if (!el) return;
    relay(el, d.cceAccountsFrame, [Number(d.dx) || 0, Number(d.dy) || 0]);
  }, true);

  // ---- filling, on request ----

  const fill = (cred) => {
    // Suppress the watcher for the duration: dispatching `input` is the whole
    // point of filling, and it must not come back as typing. Synchronous, so
    // the flag is down again before anything else runs.
    state.filling = true;
    // Values go in through the prototype's own `value` setter and are
    // followed by `input` and `change`: frameworks that track their inputs
    // (React's value tracker above all) ignore a plain assignment, and a page
    // whose state never saw the credential appear submits an empty form.
    const set = (el, v) => {
      if (!el || !el.isConnected) return false;
      const d = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), 'value');
      if (d && d.set) { d.set.call(el, v); } else { el.value = v; }
      el.dispatchEvent(new Event('input', { bubbles: true }));
      el.dispatchEvent(new Event('change', { bubbles: true }));
      return true;
    };
    const user = String(cred.u || '');
    const filledUser = user.length ? set(state.user, user) : false;
    if (user.length) state.typed = user;
    const filledPass = set(state.pass, String(cred.p || ''));
    // A username-first page: the password field is not there yet.
    if (filledUser && !filledPass && state.user) state.user.focus();
    state.filling = false;
  };
  // One ask outstanding per document. The chrome keeps only the newest ask
  // from any frame and answers a superseded one with nothing, which is what
  // clears this flag for the next focus.
  let asking = false;
  const ask = () => {
    if (asking || !live()) return;
    asking = true;
    let pending;
    try { pending = handlers.cceAccountsFill.postMessage(token); } catch (e) { asking = false; return; }
    Promise.resolve(pending).then((r) => {
      asking = false;
      if (typeof r === 'string' && r) fill(JSON.parse(r));
    }, () => { asking = false; });
  };

  // ---- login fields ----

  const report = (el, kind, type) => {
    // A fill is not something to report back: the input events it dispatches
    // would arrive as "the user typed", re-opening the list that was just
    // used and filtering it by the name it had just filled in.
    if (state.filling || !live()) return;
    if (kind === 'pass') { state.pass = el; } else { state.user = el; }
    // Remember the pair, so filling reaches both fields from either one.
    const form = el.form;
    const pass = passwordsIn(form).concat(passwordsIn(document))[0] || null;
    if (pass) state.pass = pass;
    if (kind === 'user') { state.user = el; state.typed = el.value || state.typed; }
    post({
      t: type,
      kind: kind,
      frame: token,
      top: isTop,
      origin: location.origin,
      rect: rectOf(el),
      value: kind === 'pass' ? '' : (el.value || ''),
    });
    announce();
    if (type === 'focus') ask();
  };

  document.addEventListener('focusin', (e) => {
    const kind = kindOf(e.target);
    if (kind) report(e.target, kind, 'focus');
  }, true);

  document.addEventListener('focusout', (e) => {
    if (kindOf(e.target)) post({ t: 'blur', frame: token });
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
    // Focus is inside a child frame: what moved is that frame.
    if (isFrame(el)) {
      if (child && child.el === el) relay(el, child.frame, child.inner);
      return;
    }
    const kind = kindOf(el);
    if (kind) report(el, kind, 'move'); else post({ t: 'blur', frame: token });
  };
  window.addEventListener('scroll', moved, true);
  window.addEventListener('resize', moved, true);
  // Focus coming back to this document — the window, or this frame within
  // the page — while a login field is the focused element: report it, since
  // its `focusin` was ignored while the document was not live.
  window.addEventListener('focus', () => {
    const el = document.activeElement;
    const kind = kindOf(el);
    if (kind) report(el, kind, 'focus');
  });

  // ---- a sign-in going out, offered for saving ----

  // The username that goes with a password: the last username field before
  // it that holds something, else whatever was typed into one earlier in
  // this document — a username-first page has replaced that field by now.
  const userFor = (pass) => {
    const scope = pass.form || document;
    let found = '';
    for (const el of scope.querySelectorAll('input')) {
      if (el === pass) break;
      if (kindOf(el) === 'user' && el.value) found = el.value;
    }
    return found || state.typed || '';
  };
  const submitted = (scope) => {
    if (!live()) return;
    const pass = passwordsIn(scope).concat(passwordsIn(document)).find((p) => p.value);
    if (!pass) return;
    const user = userFor(pass);
    // A form can be submitted several ways at once (Enter, then the submit
    // event it causes); once per credential is enough.
    const key = user + '\n' + pass.value;
    if (key === state.sent) return;
    state.sent = key;
    post({ t: 'submit', frame: token, top: isTop, origin: location.origin,
           user: user, pass: pass.value });
  };
  // A real form says it went out with `submit`, which fires only once the
  // page's own validation has passed — an Enter or a button press inside a
  // form is left to it, or a sign-in the page refused would be offered.
  document.addEventListener('submit', (e) => submitted(e.target), true);
  // Most sign-in pages have no form at all: a script reads the fields when
  // Enter is pressed or the button is. Those count, and only those — a
  // button that says it signs in, never the show-password eye beside the
  // field.
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && kindOf(e.target) && !e.target.form) submitted(document);
  }, true);
  document.addEventListener('click', (e) => {
    const b = e.target && e.target.closest &&
      e.target.closest('button, input[type=submit], input[type=image], [role=button]');
    if (!b) return;
    const type = (b.getAttribute('type') || (b.tagName === 'BUTTON' ? 'submit' : '')).toLowerCase();
    if (b.form && (type === 'submit' || type === 'image')) return;
    const says = ((b.textContent || '') + ' ' + (b.value || '') + ' ' + (b.id || '') + ' ' +
                  (b.getAttribute('aria-label') || '')).toLowerCase();
    if (/sign|log ?in|continue|next|submit|enter|go\b/.test(says)) submitted(b.form);
  }, true);
})();
"#;

/// The answer to a frame's fill ask: the credential, as a JSON string the
/// watcher parses. It travels as data on the reply — never spliced into a
/// script source — so no character in a password can become code.
pub fn fill_reply(username: &str, password: &str) -> String {
    serde_json::json!({ "u": username, "p": password }).to_string()
}

/// A password typed into a page, on its way to being offered for saving.
///
/// Prints as `Password(…)`: `FormEvent` derives `Debug`, and so does the
/// chrome's message type, so a plain `String` here would put a live password
/// into any log line that ever formatted one. (`accounts::Secret` is the same
/// idea on the keyring side; this module stays free of the app's types so the
/// examples can build it on its own.)
#[derive(Clone, PartialEq)]
pub struct Password(String);

impl Password {
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for Password {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Password(…)")
    }
}

/// What the watcher saw, as the chrome consumes it.
#[derive(Debug, Clone)]
pub enum FormEvent {
    /// A login field took focus, or moved, or its text changed.
    Field {
        /// `location.origin` of the frame that reported it. For the top frame
        /// it is checked against the tab's own URL; for a child frame it is
        /// the site the credential would go to, and what accounts match.
        origin: String,
        /// The reporting document's token. A child frame's `Frame` offset
        /// events carry the same one, and a fill is answered only to the ask
        /// that carries the token the list was opened for.
        frame: String,
        /// The tab's top frame, rather than a frame inside it.
        top: bool,
        /// A password field rather than a username one.
        password: bool,
        /// Rect in CSS pixels relative to the reporting frame's viewport:
        /// x, y, width, height.
        rect: (f32, f32, f32, f32),
        /// What the username field holds, for filtering. Always empty for a
        /// password field — the chrome has no business with what is typed
        /// into one.
        value: String,
        /// True when this is a re-report of a field that was already focused
        /// (scroll, resize, typing) rather than a fresh focus.
        moved: bool,
    },
    /// Where a child frame's viewport sits in the top frame's, in CSS pixels.
    Frame { frame: String, offset: (f32, f32) },
    /// Focus left the login field in this frame.
    Blur { frame: String },
    /// A sign-in went out: a form submitted, Enter in a login field, or a
    /// sign-in button pressed with a password filled in.
    Submit {
        /// The frame's origin — the site these credentials belong to.
        origin: String,
        frame: String,
        top: bool,
        username: String,
        password: Password,
    },
}

/// Parse one message from the watcher. Anything unexpected is dropped: this
/// is a channel the chrome acts on, so it takes only what it recognizes.
pub fn parse_event(json: &str) -> Option<FormEvent> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let frame = || value["frame"].as_str().unwrap_or_default().to_string();
    match value["t"].as_str()? {
        "blur" => Some(FormEvent::Blur { frame: frame() }),
        "frame" => {
            let offset = value["offset"].as_array()?;
            let num = |i: usize| offset.get(i).and_then(|v| v.as_f64()).map(|f| f as f32);
            let frame = frame();
            (!frame.is_empty()).then_some(())?;
            Some(FormEvent::Frame { frame, offset: (num(0)?, num(1)?) })
        }
        "submit" => {
            let password = value["pass"].as_str()?;
            (!password.is_empty()).then_some(())?;
            Some(FormEvent::Submit {
                origin: value["origin"].as_str()?.to_string(),
                frame: frame(),
                top: value["top"].as_bool().unwrap_or(false),
                username: value["user"].as_str().unwrap_or_default().to_string(),
                password: Password(password.to_string()),
            })
        }
        t @ ("focus" | "input" | "move") => {
            let rect = value["rect"].as_array()?;
            let num = |i: usize| rect.get(i).and_then(|v| v.as_f64()).map(|f| f as f32);
            Some(FormEvent::Field {
                origin: value["origin"].as_str().unwrap_or_default().to_string(),
                frame: frame(),
                top: value["top"].as_bool().unwrap_or(false),
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
    fn a_credential_travels_as_data() {
        // The whole hazard in one string: quotes, a backslash, a closing
        // script tag, a newline and a line separator.
        let nasty = "a\"b\\c</script>\nd\u{2028}e";
        let reply = fill_reply("user", nasty);
        let back: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(back["u"], "user");
        assert_eq!(back["p"], nasty, "the password survives the trip exactly");
    }

    #[test]
    fn events_parse_and_junk_is_dropped() {
        let focus = parse_event(
            r#"{"t":"focus","kind":"user","frame":"ab12","origin":"https://example.com","rect":[10,20,120,24],"value":"me"}"#,
        );
        match focus {
            Some(FormEvent::Field { origin, frame, top, password, rect, value, moved }) => {
                assert!(!top, "a missing top flag is a child frame");
                assert_eq!(origin, "https://example.com");
                assert_eq!(frame, "ab12");
                assert!(!password);
                assert_eq!(rect, (10.0, 20.0, 120.0, 24.0));
                assert_eq!(value, "me");
                assert!(!moved);
            }
            other => panic!("expected a field event, got {other:?}"),
        }
        assert!(matches!(
            parse_event(r#"{"t":"blur","frame":""}"#),
            Some(FormEvent::Blur { frame }) if frame.is_empty()
        ));
        assert!(matches!(
            parse_event(r#"{"t":"input","kind":"pass","origin":"x","rect":[0,0,1,1],"value":""}"#),
            Some(FormEvent::Field { password: true, moved: true, .. })
        ));
        assert!(matches!(
            parse_event(r#"{"t":"frame","frame":"ab12","offset":[5,7.5]}"#),
            Some(FormEvent::Frame { offset: (5.0, 7.5), .. })
        ));
        // Nonsense, a field event with no rect, an offset with no frame, and a
        // submit with no password are all ignored.
        assert!(parse_event("not json").is_none());
        assert!(parse_event(r#"{"t":"focus","kind":"user"}"#).is_none());
        assert!(parse_event(r#"{"t":"evil"}"#).is_none());
        assert!(parse_event(r#"{"t":"frame","frame":"","offset":[1,1]}"#).is_none());
        assert!(parse_event(r#"{"t":"submit","origin":"https://x.test","user":"me","pass":""}"#).is_none());
    }

    #[test]
    fn a_submitted_password_never_prints_itself() {
        let e = parse_event(
            r#"{"t":"submit","frame":"","origin":"https://x.test","user":"me","pass":"hunter2"}"#,
        )
        .unwrap();
        let printed = format!("{e:?}");
        assert!(!printed.contains("hunter2"), "{printed}");
        match e {
            FormEvent::Submit { password, username, .. } => {
                assert_eq!(username, "me");
                assert_eq!(password.expose(), "hunter2");
            }
            other => panic!("expected a submit, got {other:?}"),
        }
    }
}
