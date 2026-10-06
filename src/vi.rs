//! Vi-style modal keys, after qutebrowser.
//!
//! The chrome-independent half: the modes, the normal-mode bindings and the
//! parser that turns keystrokes into them, hint labels, `:` commands, and the
//! scripts the page side runs. `main.rs` owns what these do (the tab set, the
//! URL bar, the drawing), and the WPE host owns how they reach the engine.
//!
//! Points that are choices, not accidents:
//!
//! * **qutebrowser's bindings, where this browser has the feature.** `J`/`K`
//!   are next/previous tab and `d`/`u` close and reopen, as there — not
//!   Vimium's. What the browser does not have (zoom, quickmarks, windows)
//!   is not bound, rather than bound to nothing.
//! * **Unbound characters are swallowed, unbound named keys are not**
//!   (qutebrowser's `forward_unbound_keys = auto`): a stray letter must not
//!   type into a page that happens to have focus, but arrows, Page Up/Down,
//!   Space, Enter and Tab still reach it.
//! * **Insert mode is entered by a click, not by focus.** A field the page
//!   focuses on its own (a search box with `autofocus`) leaves normal mode
//!   alone, so `j` still scrolls; a field the person clicks into — or picks
//!   with a hint — takes the keyboard. `i` enters it explicitly.

use cce_ui::widget::{Key, KeyEvent};

/// What the keyboard means right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Keys are commands.
    Normal,
    /// Keys go to the page, except Escape, which comes back to normal.
    Insert,
    /// Every key goes to the page, the chrome's own Ctrl chords included;
    /// only Shift+Escape comes back.
    Passthrough,
    /// Labels are up over the page's clickable elements; typing one picks it.
    Hint,
    /// The `:` / `/` / `?` line has the keyboard.
    Command,
}

impl Mode {
    /// The status line's name for the mode; `None` for normal, which shows
    /// nothing.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Mode::Normal | Mode::Command => None,
            Mode::Insert => Some("-- INSERT --"),
            Mode::Passthrough => Some("-- PASSTHROUGH --"),
            Mode::Hint => Some("-- HINT --"),
        }
    }
}

/// What picking a hint does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintKind {
    /// Click it, as the pointer would.
    Follow,
    /// Middle-click it: a link opens in a background tab.
    Background,
    /// Copy its link.
    Yank,
}

/// Which line the command prompt is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prompt {
    Command,
    Search,
    SearchBack,
}

impl Prompt {
    pub fn sigil(self) -> char {
        match self {
            Prompt::Command => ':',
            Prompt::Search => '/',
            Prompt::SearchBack => '?',
        }
    }
}

/// A normal-mode command. A count, where one was typed, comes alongside.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    /// Scroll by this many lines (a line is half a wheel notch), eased.
    ScrollLines(f32, f32),
    /// Scroll by this fraction of the viewport: half a page, or a page.
    ScrollPage(f32),
    /// To the top / the bottom; with a count, to that percent.
    Top,
    Bottom,
    Back,
    Forward,
    Reload,
    /// Move this many tabs (a count multiplies).
    TabNext,
    TabPrev,
    /// `gt`: the next tab, or with a count, tab number `count`.
    TabGoto,
    TabFirst,
    TabLast,
    /// Tab number n, counting from 1 (`Alt+n`).
    TabFocus(usize),
    TabClose,
    TabOnly,
    UndoClose,
    /// Focus the URL bar: empty, or holding the current address (`edit`);
    /// submitting loads here or in a new tab.
    Open { tab: bool, edit: bool },
    Hint(HintKind),
    YankUrl,
    YankTitle,
    /// Open the clipboard's address, here or in a new tab.
    Paste { tab: bool },
    Insert,
    /// Focus the page's first text field and enter insert mode.
    FocusInput,
    Passthrough,
    Prompt(Prompt),
    SearchNext,
    SearchPrev,
    Bookmark,
    /// One of the `cce://` pages.
    Page(&'static str),
    /// Up one path segment; to the site root.
    Up,
    Root,
}

/// The normal-mode bindings: key sequence -> action. A sequence is a run of
/// tokens as [`token`] spells them: a plain character, or `<C-x>` / `<A-x>`.
pub const BINDINGS: &[(&str, Action)] = &[
    ("j", Action::ScrollLines(0.0, 1.0)),
    ("k", Action::ScrollLines(0.0, -1.0)),
    ("h", Action::ScrollLines(-1.0, 0.0)),
    ("l", Action::ScrollLines(1.0, 0.0)),
    ("<C-d>", Action::ScrollPage(0.5)),
    ("<C-u>", Action::ScrollPage(-0.5)),
    ("<C-f>", Action::ScrollPage(1.0)),
    ("<C-b>", Action::ScrollPage(-1.0)),
    ("gg", Action::Top),
    ("G", Action::Bottom),
    ("H", Action::Back),
    ("L", Action::Forward),
    ("r", Action::Reload),
    ("R", Action::Reload),
    ("J", Action::TabNext),
    ("K", Action::TabPrev),
    ("gt", Action::TabGoto),
    ("gT", Action::TabPrev),
    ("g0", Action::TabFirst),
    ("g^", Action::TabFirst),
    ("g$", Action::TabLast),
    ("<A-1>", Action::TabFocus(1)),
    ("<A-2>", Action::TabFocus(2)),
    ("<A-3>", Action::TabFocus(3)),
    ("<A-4>", Action::TabFocus(4)),
    ("<A-5>", Action::TabFocus(5)),
    ("<A-6>", Action::TabFocus(6)),
    ("<A-7>", Action::TabFocus(7)),
    ("<A-8>", Action::TabFocus(8)),
    ("<A-9>", Action::TabLast),
    ("d", Action::TabClose),
    ("co", Action::TabOnly),
    ("u", Action::UndoClose),
    ("o", Action::Open { tab: false, edit: false }),
    ("O", Action::Open { tab: true, edit: false }),
    ("go", Action::Open { tab: false, edit: true }),
    ("gO", Action::Open { tab: true, edit: true }),
    ("f", Action::Hint(HintKind::Follow)),
    ("F", Action::Hint(HintKind::Background)),
    (";b", Action::Hint(HintKind::Background)),
    (";y", Action::Hint(HintKind::Yank)),
    ("yy", Action::YankUrl),
    ("yt", Action::YankTitle),
    ("p", Action::Paste { tab: false }),
    ("P", Action::Paste { tab: true }),
    ("i", Action::Insert),
    ("gi", Action::FocusInput),
    ("<C-v>", Action::Passthrough),
    (":", Action::Prompt(Prompt::Command)),
    ("/", Action::Prompt(Prompt::Search)),
    ("?", Action::Prompt(Prompt::SearchBack)),
    ("n", Action::SearchNext),
    ("N", Action::SearchPrev),
    ("M", Action::Bookmark),
    ("Sb", Action::Page("cce://bookmarks")),
    ("Sh", Action::Page("cce://history")),
    ("gu", Action::Up),
    ("gU", Action::Root),
];

/// A keystroke as the bindings spell it. `None` for keys that are not
/// commands at all — named keys, which unbound go on to the page — and for
/// a lone modifier.
pub fn token(e: &KeyEvent) -> Option<String> {
    let Key::Character(c) = &e.logical_key else { return None };
    let ch = c.chars().next()?.to_ascii_lowercase();
    // Shift is spelled out on a chord, so Ctrl+Shift+D (the chrome's
    // favorite toggle) is not taken for Ctrl+D.
    let shift = if e.shift { "S-" } else { "" };
    if e.ctrl {
        Some(format!("<C-{shift}{ch}>"))
    } else if e.alt {
        Some(format!("<A-{shift}{ch}>"))
    } else {
        // The logical key already carries Shift: `G`, not `g`.
        Some(c.clone())
    }
}

/// Whether some binding begins with these exact keys.
fn is_prefix(keys: &str) -> bool {
    BINDINGS.iter().any(|(k, _)| k.starts_with(keys))
}

/// What one keystroke did to the pending sequence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fed {
    /// Part of a longer binding (or a count); wait for more.
    Pending,
    /// A whole binding, with its count if one was typed.
    Run(Action, Option<u32>),
    /// Bound to nothing; the sequence is dropped.
    Unbound,
}

/// The count and keys typed so far toward a binding.
#[derive(Debug, Default)]
pub struct Keys {
    count: String,
    keys: String,
}

impl Keys {
    pub fn feed(&mut self, token: &str) -> Fed {
        // A count leads, and a `0` only continues one.
        let digit = token.len() == 1 && token.as_bytes()[0].is_ascii_digit();
        if digit && self.keys.is_empty() && (token != "0" || !self.count.is_empty()) {
            self.count.push_str(token);
            return Fed::Pending;
        }
        let candidate = format!("{}{token}", self.keys);
        if let Some((_, action)) = BINDINGS.iter().find(|(k, _)| *k == candidate) {
            let count = self.count.parse().ok();
            self.clear();
            return Fed::Run(*action, count);
        }
        if is_prefix(&candidate) {
            self.keys = candidate;
            return Fed::Pending;
        }
        self.clear();
        Fed::Unbound
    }

    /// Whether `token` would be used by [`Self::feed`] rather than dropped —
    /// how a Ctrl chord the bindings do not know goes on to the chrome.
    pub fn takes(&self, token: &str) -> bool {
        is_prefix(&format!("{}{token}", self.keys))
    }

    pub fn clear(&mut self) {
        self.count.clear();
        self.keys.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.count.is_empty() && self.keys.is_empty()
    }

    /// What is pending, for the status line.
    pub fn shown(&self) -> String {
        format!("{}{}", self.count, self.keys)
    }
}

/// The characters hint labels are made of — the home row, qutebrowser's
/// default.
pub const HINT_CHARS: &str = "asdfghjkl";

/// Labels for `n` hints: as short as `n` allows, and prefix-free, so typing
/// a whole label is always unambiguous and never needs a confirming key.
///
/// Mixed lengths, the way qutebrowser does it: when `n` does not fill every
/// label of the longest length, the spare room goes to shorter ones. Each
/// short label takes one prefix away from the long ones, and with it
/// `chars - 1` long labels' worth of room.
pub fn hint_labels(n: usize, chars: &str) -> Vec<String> {
    let chars: Vec<char> = chars.chars().collect();
    let k = chars.len();
    if n == 0 || k < 2 {
        return Vec::new();
    }
    let mut needed = 1;
    let mut room = k;
    while room < n {
        needed += 1;
        room *= k;
    }
    let short = if needed > 1 { (room - n) / (k - 1) } else { 0 };
    let spell = |mut i: usize, len: usize| -> String {
        let mut s = vec![chars[0]; len];
        for slot in s.iter_mut().rev() {
            *slot = chars[i % k];
            i /= k;
        }
        s.into_iter().collect()
    };
    let mut out = Vec::with_capacity(n);
    for i in 0..short.min(n) {
        out.push(spell(i, needed - 1));
    }
    // The prefixes after the short labels, each extended by every char.
    let mut i = short * k;
    while out.len() < n {
        out.push(spell(i, needed));
        i += 1;
    }
    out
}

/// One clickable element on the page, as the hint script reports it, in the
/// chrome's logical pixels (the page's CSS pixels — see `field_rect`).
#[derive(Debug, Clone, PartialEq)]
pub struct Hint {
    /// The visible part of the element.
    pub rect: (f32, f32, f32, f32),
    /// Where a click on it lands: the visible part's middle.
    pub at: (f32, f32),
    pub href: Option<String>,
    pub label: String,
}

/// The hint script's answer, labelled. Anything malformed is dropped.
pub fn parse_hints(json: &str) -> Vec<Hint> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    let num = |v: &serde_json::Value, k: &str| v[k].as_f64().map(|f| f as f32);
    let mut hints: Vec<Hint> = items
        .iter()
        .filter_map(|v| {
            let (x, y, w, h) = (num(v, "x")?, num(v, "y")?, num(v, "w")?, num(v, "h")?);
            Some(Hint {
                rect: (x, y, w, h),
                at: (x + w / 2.0, y + h / 2.0),
                href: v["href"].as_str().filter(|s| !s.is_empty()).map(str::to_string),
                label: String::new(),
            })
        })
        .collect();
    let labels = hint_labels(hints.len(), HINT_CHARS);
    for (h, l) in hints.iter_mut().zip(labels) {
        h.label = l;
    }
    hints
}

/// A parsed `:` line.
#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Run(Action),
    /// Load what was typed: here, in a new tab, or in a background one.
    Open { target: String, tab: bool, background: bool },
    Quit,
    Empty,
}

/// Parse a `:` line — qutebrowser's names, plus the short forms vim hands
/// people's fingers (`:o`, `:t`, `:q`, `:b 3`).
pub fn parse_command(line: &str) -> Result<Cmd, String> {
    let line = line.trim();
    let (name, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    // `-t` / `-b` flags on `open`, as qutebrowser spells them.
    let mut tab = false;
    let mut background = false;
    let mut words: Vec<&str> = Vec::new();
    for w in rest.split_whitespace() {
        match w {
            "-t" | "--tab" => tab = true,
            "-b" | "--bg" => background = true,
            _ => words.push(w),
        }
    }
    let target = words.join(" ");
    let open = |tab: bool, background: bool| {
        if target.is_empty() {
            Ok(Cmd::Run(Action::Open { tab: tab || background, edit: false }))
        } else {
            Ok(Cmd::Open { target: target.clone(), tab, background })
        }
    };
    let number = || {
        rest.parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("{name}: needs a tab number"))
    };
    match name {
        "" => Ok(Cmd::Empty),
        "o" | "open" => open(tab, background),
        "t" | "to" | "tabopen" | "tabnew" => open(true, background),
        "bg" | "backgroundopen" => open(true, true),
        "q" | "quit" | "qa" | "qall" | "wq" | "wqa" | "x" => Ok(Cmd::Quit),
        "close" | "tab-close" | "tabclose" | "bd" | "bdelete" => Ok(Cmd::Run(Action::TabClose)),
        "tab-only" | "only" | "tabonly" => Ok(Cmd::Run(Action::TabOnly)),
        "undo" => Ok(Cmd::Run(Action::UndoClose)),
        "tab-next" | "tabnext" | "tabn" | "bn" => Ok(Cmd::Run(Action::TabNext)),
        "tab-prev" | "tabprev" | "tabp" | "tabN" | "bp" => Ok(Cmd::Run(Action::TabPrev)),
        "tab-focus" | "buffer" | "b" => number().map(|n| Cmd::Run(Action::TabFocus(n))),
        "back" => Ok(Cmd::Run(Action::Back)),
        "forward" => Ok(Cmd::Run(Action::Forward)),
        "reload" | "r" => Ok(Cmd::Run(Action::Reload)),
        "yank" if rest.is_empty() || rest == "url" => Ok(Cmd::Run(Action::YankUrl)),
        "yank" if rest == "title" => Ok(Cmd::Run(Action::YankTitle)),
        "bookmark-add" | "bookmark" => Ok(Cmd::Run(Action::Bookmark)),
        "bookmarks" => Ok(Cmd::Run(Action::Page("cce://bookmarks"))),
        "history" => Ok(Cmd::Run(Action::Page("cce://history"))),
        "downloads" => Ok(Cmd::Run(Action::Page("cce://downloads"))),
        "favorites" => Ok(Cmd::Run(Action::Page("cce://favorites"))),
        "mode-enter" if rest == "insert" => Ok(Cmd::Run(Action::Insert)),
        "mode-enter" if rest == "passthrough" => Ok(Cmd::Run(Action::Passthrough)),
        _ => Err(format!("Unknown command: {name}")),
    }
}

/// Up one path segment (`gu`): `/a/b/` and `/a/b` both go to `/a/`, and the
/// query and fragment go with the step. `None` when already at the root.
pub fn url_up(url: &url::Url) -> Option<url::Url> {
    let path = url.path().trim_end_matches('/');
    if path.is_empty() && url.query().is_none() && url.fragment().is_none() {
        return None;
    }
    let mut up = url.clone();
    up.set_query(None);
    up.set_fragment(None);
    let parent = match path.rfind('/') {
        Some(i) => &path[..=i],
        None => "/",
    };
    up.set_path(parent);
    Some(up)
}

/// The site root (`gU`).
pub fn url_root(url: &url::Url) -> Option<url::Url> {
    let mut root = url.clone();
    root.set_path("/");
    root.set_query(None);
    root.set_fragment(None);
    (root != *url).then_some(root)
}

// ---- the page side ----
//
// Engine-agnostic text, but only the WPE host runs it. Everything runs in a
// private script world (`WORLD`): the DOM is shared, the page's globals are
// not, so a page can neither see these helpers nor post on the channel the
// chrome listens to.

/// The world the scripts below run in.
pub const WORLD: &str = "cce-vi";
/// The channel the focus watcher reports on.
pub const CHANNEL: &str = "cceVi";

/// The shared test for "this element takes typing", as a JS prelude.
const EDITABLE_JS: &str = r#"
  const noText = ['button', 'submit', 'reset', 'checkbox', 'radio', 'image',
                  'file', 'hidden', 'range', 'color'];
  const editable = (el) => {
    if (!el) return false;
    if (el.isContentEditable) return true;
    if (el.disabled || el.readOnly) return false;
    if (el.tagName === 'TEXTAREA') return true;
    if (el.tagName === 'INPUT') return !noText.includes((el.type || 'text').toLowerCase());
    return false;
  };
"#;

/// Runs in every frame and reports whether the focused element takes text,
/// whenever focus moves. Only a document with focus speaks (every tab shares
/// the channel; only the shown one is focused), and a frame whose focus is
/// inside a child frame leaves it to the child.
pub fn focus_watch_js() -> String {
    format!(
        r#"(() => {{
  const h = window.webkit && window.webkit.messageHandlers;
  if (!h || !h.cceVi) return;
  {EDITABLE_JS}
  let timer = 0;
  // After the event, not in it: mid-move, activeElement is still the old one.
  const report = () => {{
    clearTimeout(timer);
    timer = setTimeout(() => {{
      if (!document.hasFocus()) return;
      let el = document.activeElement;
      while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement;
      if (el && (el.tagName === 'IFRAME' || el.tagName === 'FRAME')) return;
      try {{ h.cceVi.postMessage(JSON.stringify({{ t: 'focus', editable: editable(el) }})); }} catch (e) {{}}
    }}, 0);
  }};
  document.addEventListener('focusin', report, true);
  document.addEventListener('focusout', report, true);
}})();"#
    )
}

/// Whether what has focus now takes typing, following focus down through
/// same-origin frames: `"yes"` or `""`. Asked after a click in normal mode,
/// since clicking a field that already had focus moves no focus, and so
/// tells the watcher nothing.
pub fn active_editable_js() -> String {
    format!(
        r#"(() => {{
  {EDITABLE_JS}
  let el = document.activeElement;
  for (;;) {{
    while (el && el.shadowRoot && el.shadowRoot.activeElement) el = el.shadowRoot.activeElement;
    if (!el || (el.tagName !== 'IFRAME' && el.tagName !== 'FRAME')) break;
    try {{ const d = el.contentDocument; if (!d) return ''; el = d.activeElement; }} catch (e) {{ return ''; }}
  }}
  return editable(el) ? 'yes' : '';
}})()"#
    )
}

/// The clickable elements in view, as JSON: `[{x, y, w, h, href}]` in the
/// top viewport's CSS pixels, clipped to what is visible. Descends into
/// same-origin frames; a cross-origin frame's document is out of reach of a
/// script evaluated in the top frame. `links` keeps only real links (`F`,
/// `;y`), since only those open anything when middle-clicked or copied.
///
/// An element covered at its middle by something unrelated (a modal, a
/// cookie banner) is left out: a click there would land on the cover.
pub fn hints_js(links: bool) -> String {
    let sel = if links {
        "a[href], area[href]"
    } else {
        "a, area, button, select, textarea, summary, \
         input:not([type=\"hidden\"]), \
         [contenteditable]:not([contenteditable=\"false\"]), \
         [onclick], [onmousedown], [role=\"link\"], [role=\"button\"], \
         [role=\"tab\"], [role=\"checkbox\"], [role=\"switch\"], [role=\"option\"], \
         [role=\"menuitem\"], [role=\"menuitemcheckbox\"], [role=\"menuitemradio\"], \
         [role=\"treeitem\"], [aria-haspopup], [ng-click], [data-ng-click], \
         [tabindex]:not([tabindex=\"-1\"])"
    };
    format!(
        r#"(() => {{
  const SEL = {sel:?};
  const out = [];
  const seen = new Set();
  const walk = (win, ox, oy, clip) => {{
    const doc = win.document;
    let els;
    try {{ els = doc.querySelectorAll(SEL); }} catch (e) {{ return; }}
    for (const el of els) {{
      if (seen.has(el) || el.disabled) continue;
      const st = win.getComputedStyle(el);
      if (st.visibility !== 'visible' || st.display === 'none') continue;
      for (const r of el.getClientRects()) {{
        const x0 = Math.max(r.left + ox, clip[0]), y0 = Math.max(r.top + oy, clip[1]);
        const x1 = Math.min(r.right + ox, clip[2]), y1 = Math.min(r.bottom + oy, clip[3]);
        if (x1 - x0 < 2 || y1 - y0 < 2) continue;
        const hit = doc.elementFromPoint((x0 + x1) / 2 - ox, (y0 + y1) / 2 - oy);
        if (hit && hit !== el && !el.contains(hit) && !hit.contains(el)) continue;
        seen.add(el);
        out.push({{ x: x0, y: y0, w: x1 - x0, h: y1 - y0, href: el.href ? String(el.href) : '' }});
        break;
      }}
    }}
    for (const f of doc.querySelectorAll('iframe, frame')) {{
      let inner;
      try {{ inner = f.contentWindow; if (!inner || !inner.document) continue; }} catch (e) {{ continue; }}
      const r = f.getBoundingClientRect();
      const fx = r.left + ox + f.clientLeft, fy = r.top + oy + f.clientTop;
      const c = [Math.max(fx, clip[0]), Math.max(fy, clip[1]),
                 Math.min(fx + f.clientWidth, clip[2]), Math.min(fy + f.clientHeight, clip[3])];
      if (c[2] > c[0] && c[3] > c[1]) walk(inner, fx, fy, c);
    }}
  }};
  walk(window, 0, 0, [0, 0, window.innerWidth, window.innerHeight]);
  return JSON.stringify(out);
}})()"#
    )
}

/// Scroll the page's main scroller: by a fraction of the viewport, or to a
/// fraction of its range. The scroller is whatever scrolls under the middle
/// of the view — many pages scroll an inner element and leave the document
/// still — falling back to the document's own. `smooth` follows the DE's
/// animations switch.
pub fn scroll_js(by: Option<f32>, to: Option<f32>, smooth: bool) -> String {
    let behavior = if smooth { "smooth" } else { "instant" };
    let by = by.map_or("null".to_string(), |f| f.to_string());
    let to = to.map_or("null".to_string(), |f| f.to_string());
    format!(
        r#"(() => {{
  const by = {by}, to = {to};
  const scrolls = (el) => {{
    const s = getComputedStyle(el);
    return el.scrollHeight > el.clientHeight + 1 && /(auto|scroll|overlay)/.test(s.overflowY);
  }};
  const root = document.scrollingElement || document.documentElement;
  let el = document.elementFromPoint(innerWidth / 2, innerHeight / 2);
  while (el && el !== document.body && el !== document.documentElement && !scrolls(el)) el = el.parentElement;
  if (!el || el === document.body || el === document.documentElement) el = root;
  const view = el === root ? innerHeight : el.clientHeight;
  const top = by !== null ? el.scrollTop + by * view
                          : to * (el.scrollHeight - view);
  el.scrollTo({{ top, behavior: '{behavior}' }});
}})()"#
    )
}

/// Focus the first visible text field (`gi`): `"yes"`, or `""` for none.
pub fn focus_input_js() -> String {
    format!(
        r#"(() => {{
  {EDITABLE_JS}
  for (const el of document.querySelectorAll('input, textarea, [contenteditable]')) {{
    if (!editable(el)) continue;
    const r = el.getBoundingClientRect();
    if (r.width < 2 || r.height < 2 || r.bottom < 0 || r.top > innerHeight) continue;
    if (getComputedStyle(el).visibility !== 'visible') continue;
    el.focus();
    return 'yes';
  }}
  return '';
}})()"#
    )
}

/// A focus watcher report, or `None` for anything else on the channel.
pub fn parse_focus(json: &str) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    (v["t"] == "focus").then(|| v["editable"].as_bool()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(keys: &mut Keys, seq: &[&str]) -> Fed {
        let mut last = Fed::Unbound;
        for t in seq {
            last = keys.feed(t);
        }
        last
    }

    #[test]
    fn sequences_counts_and_dead_ends() {
        let mut k = Keys::default();
        assert_eq!(k.feed("j"), Fed::Run(Action::ScrollLines(0.0, 1.0), None));
        assert_eq!(k.feed("g"), Fed::Pending);
        assert_eq!(k.shown(), "g");
        assert_eq!(k.feed("g"), Fed::Run(Action::Top, None));
        assert_eq!(feed_all(&mut k, &["1", "0", "j"]), Fed::Run(Action::ScrollLines(0.0, 1.0), Some(10)));
        // A leading 0 is not a count, and binds to nothing.
        assert_eq!(k.feed("0"), Fed::Unbound);
        assert_eq!(feed_all(&mut k, &["3", "g", "t"]), Fed::Run(Action::TabGoto, Some(3)));
        assert_eq!(feed_all(&mut k, &["g", "z"]), Fed::Unbound);
        assert!(k.is_empty());
        assert_eq!(k.feed("<C-d>"), Fed::Run(Action::ScrollPage(0.5), None));
        assert!(!k.takes("<C-t>"));
        assert!(k.takes("<C-u>"));
    }

    #[test]
    fn every_binding_is_reachable() {
        // A binding that is a prefix of another would shadow it for good.
        for (a, _) in BINDINGS {
            for (b, _) in BINDINGS {
                assert!(a == b || !b.starts_with(a), "{a:?} shadows {b:?}");
            }
        }
    }

    #[test]
    fn hint_labels_are_short_unique_and_prefix_free() {
        for n in [0, 1, 5, 9, 10, 17, 80, 81, 82, 700] {
            let labels = hint_labels(n, HINT_CHARS);
            assert_eq!(labels.len(), n);
            for a in &labels {
                for b in &labels {
                    assert!(a == b || !b.starts_with(a.as_str()), "{a} prefixes {b} (n={n})");
                }
            }
            let max = labels.iter().map(String::len).max().unwrap_or(0);
            let need = (1..).find(|l| 9usize.pow(*l as u32) >= n.max(1)).unwrap();
            assert_eq!(max, if n == 0 { 0 } else { need }, "n={n}");
        }
        // Ten hints: eight single letters, the last letter's two-letter run.
        assert_eq!(hint_labels(10, HINT_CHARS)[..9], ["a", "s", "d", "f", "g", "h", "j", "k", "la"]);
    }

    #[test]
    fn hints_parse_and_junk_is_dropped() {
        let h = parse_hints(r#"[{"x":10,"y":20,"w":30,"h":10,"href":"https://a.example/"},{"x":"no"},{"x":0,"y":0,"w":4,"h":4,"href":""}]"#);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].at, (25.0, 25.0));
        assert_eq!(h[0].href.as_deref(), Some("https://a.example/"));
        assert_eq!(h[1].href, None);
        assert_eq!((h[0].label.as_str(), h[1].label.as_str()), ("a", "s"));
        assert!(parse_hints("not json").is_empty());
    }

    #[test]
    fn commands_parse() {
        assert_eq!(parse_command("o example.com"), Ok(Cmd::Open { target: "example.com".into(), tab: false, background: false }));
        assert_eq!(parse_command("open -t rust book"), Ok(Cmd::Open { target: "rust book".into(), tab: true, background: false }));
        assert_eq!(parse_command(" t "), Ok(Cmd::Run(Action::Open { tab: true, edit: false })));
        assert_eq!(parse_command("b 3"), Ok(Cmd::Run(Action::TabFocus(3))));
        assert!(parse_command("b x").is_err());
        assert_eq!(parse_command("q"), Ok(Cmd::Quit));
        assert_eq!(parse_command(""), Ok(Cmd::Empty));
        assert_eq!(parse_command("frobnicate"), Err("Unknown command: frobnicate".into()));
    }

    #[test]
    fn up_and_root() {
        let u = |s: &str| url::Url::parse(s).unwrap();
        assert_eq!(url_up(&u("https://a.example/x/y/?q=1")), Some(u("https://a.example/x/")));
        assert_eq!(url_up(&u("https://a.example/x/y")), Some(u("https://a.example/x/")));
        assert_eq!(url_up(&u("https://a.example/x")), Some(u("https://a.example/")));
        assert_eq!(url_up(&u("https://a.example/")), None);
        assert_eq!(url_root(&u("https://a.example/x/y#z")), Some(u("https://a.example/")));
        assert_eq!(url_root(&u("https://a.example/")), None);
    }

    #[test]
    fn focus_reports_parse() {
        assert_eq!(parse_focus(r#"{"t":"focus","editable":true}"#), Some(true));
        assert_eq!(parse_focus(r#"{"t":"focus","editable":false}"#), Some(false));
        assert_eq!(parse_focus(r#"{"t":"other"}"#), None);
    }
}
