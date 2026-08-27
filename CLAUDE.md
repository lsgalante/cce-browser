# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`cce-browser` is a web browser for the CCE Wayland desktop environment, built on an
**embedded, in-process Servo**. It is one crate of the multi-repo `cce` workspace (its
own git repo side-by-side with its siblings; published read-only at
`https://git.lucas.co/cce-browser.git` via gitsite — the local repo is the source of
truth, there is no push remote). Read the workspace-level
`../cce-compositor/WORKSPACE.md` first: workspace layout, the `cce-ui` toolkit, config
conventions, and the multi-repo rules all live there.

Five files, ~2.6k lines:

| file | what it owns |
| --- | --- |
| `src/main.rs` | `BrowserApp` — the `cce-ui` `Application`: chrome layout, hit-testing, the URL line editor, key/pointer routing |
| `src/webview.rs` | `ServoHost` — Servo boot, the delegate, one `WebView` per tab, the frame pipeline |
| `src/pages.rs` | the `cce:` protocol handler and its History / Bookmarks stores |
| `src/downloads.rs` | the chrome-side download pipeline (Servo has none) |
| `src/settings.rs` | the per-app KDL config |

## Build: this crate is the expensive one

**Building this crate builds Servo.** That costs more than the entire rest of the
workspace combined, and the linked binary is ~175 MB. WORKSPACE.md singles this crate
out for that reason: leave it out of `cce-ui` sweeps unless someone has decided the
rebuild is worth it. Before starting, check whether Servo artifacts are even present
(`ls ../target/release/deps | grep -c servo`) — if `target/` has been pruned, the next
build is from scratch, so kick it off early and in the background.

```sh
cargo build --release -p cce-browser     # scope to this crate (shared ../target/)
cargo test -p cce-browser                # the argv-parsing tests in main.rs
ccebuild install --no-build cce-browser  # install binary + desktop entry
```

There is **no `Makefile`** here (most siblings have one) — install goes
through `ccebuild` directly. `Cargo.lock` is gitignored in this crate. Running needs a
live Wayland session; it will not run headless.

`servo = "0.4"` comes from crates.io, not a git pin. Servo's embedding API churns
hard between releases, so when a version bump breaks the build, expect the delegate
trait, the input-event constructors, and `Preferences`/`Opts` to be where it broke.

## The frame pipeline

Servo renders into a **`SoftwareRenderingContext`** (CPU, no GPU handoff), one context
shared by every tab. `paint_active()` paints the active webview into it and calls
`read_to_image` — **without `present()`**, deliberately, because presenting would
release the buffer this needs to read. The pixels upload via `cce_ui::vk::upload_rgba`
and the page draws in `display_list` as a single full-bleed quad.

Everything is on the main thread. Servo's internal threads wake calloop through
`Waker` → `Message::Spin` → `ServoHost::pump()`, which spins Servo's loop, drains
delegate signals, and repaints only if the *active* tab flagged a frame.

Two consequences worth holding onto:

- **Registry images leak unless freed.** Every `image` replacement and every
  `close_tab` calls `cce_ui::vk::free_image` on the old id. New code that swaps a tab's
  frame must do the same.
- **An idle page produces no frames**, so nothing turns the loop on its own. Anything
  time-based (see the force-dark reload deadlines) has to spawn a thread that sends
  `Message::Spin` when the deadline passes, or it simply never fires.

## Tabs

One `WebView` per tab, all sharing the single rendering context; only the active one
is shown, focused, sized and painted (servoshell's model). Each `Tab` keeps its last
frame, so switching shows content instantly while the resize refreshes it.

- The delegate cannot touch `ServoHost` (it is held by Servo), so it records into
  `HostShared` — a `dirty` flag plus a per-`WebViewId` `TabSignals` map — which `pump`
  polls and folds into the `Tab` structs. New page state goes in `TabSignals`, not in
  a delegate callback that tries to mutate the host.
- `TabSignals::loading` is `Option<bool>` on purpose: a defaulted `false` would read as
  "finished loading" and swallow the true→false transition that history recording
  keys on.
- `active: usize::MAX` is a **sentinel**, set in `new()` and again in `close_tab`, so
  `activate()` does the full show/focus/resize dance instead of early-returning on
  `0 == 0`.
- Webviews created by pages (`window.open`, `target=_blank`) are built inside the
  delegate — which is why `Delegate` holds a `Weak` to itself and a clone of the
  `UserContentManager` — parked in `pending_new`, and adopted as tabs by the next
  `pump`. They are built before anyone told them the theme, so `pump` calls
  `notify_theme_change` on adoption.

## The chrome is hand-rolled

There are **no `cce-ui` widgets in this app**. The whole utility bar is emitted as
`PaintCtx` primitives in `display_list` (`display_list_text()` returns `true`), and
every hit test in `handle_mouse_input` re-derives the same rects from the same
`bar_rect`/`tab_rect`/`btn_rect`/`url_rect` helpers. **Draw and hit-test are two
readings of one geometry** — change a rect helper, not one call site.

- Everything bar-relative derives from `bar_rect`, never from `BAR_MARGIN` directly,
  or the bar-position setting silently stops moving things to the bottom edge.
- `BAR_FILL`'s **negative alpha is the frost sentinel** (`cce-ui/src/scene/paint.rs`):
  it marks the plate for the in-app blur pass, so the page shows through. Keep
  `|alpha|` low; a positive alpha just paints an opaque bar.
- **URL-bar caret and click mapping must use
  `cce_ui::engine::shaped_cluster_offsets`**, not `measure_text_width`. The latter
  returns inked extents, which drift off the glyph positions the renderer actually
  lays down — the caret ended up in the wrong place and clicks landed on the wrong
  character (commit `b280e6b`). Same shaped buffer (`font=None`) as `pc.text` draws
  with, or they disagree again.
- Clicking into an unfocused bar selects the whole URL, as do Ctrl+L and Ctrl+A, so
  typing replaces rather than appends. Arrows collapse a selection to the edge they
  move toward. `take_selection()` is the shared "replace the selection or act at the
  cursor" path for every edit.

### Key routing

`handle_key_input` is a three-stage funnel and the order is load-bearing: Ctrl chords
that belong to the chrome (tabs, internal pages, bookmark, external-open) fire
**regardless of URL-bar focus**; then a focused URL bar swallows everything into
`edit_url`; only then does the key reach the page.

Two page-directed cases are not plain key forwarding:

- **Ctrl+C/X/V go to the page as `EditingActionEvent`**, not as keystrokes. Servo has
  no built-in binding for the chords; sent raw they do nothing.
- **Modifiers must be passed explicitly.** `KeyboardEvent::from_state_and_key` defaults
  them to empty, which delivers every chord as a bare character — Ctrl+A typed a
  literal "a" into a focused textarea instead of selecting it.

Wheel events pass **winit-signed deltas** (positive = up) with no separate scroll
event: Servo hit-tests the wheel, gives the page its `preventDefault` chance, and
applies the inverted delta itself.

## `cce://` pages

`CceProtocol` registers the `cce` scheme with Servo's `ProtocolRegistry`, so
`cce://history`, `cce://bookmarks`, `cce://downloads` and `cce://cookies` are **real
pages fetched through Servo's network stack** and rendered like any other. That is why
every mutating action is an ordinary link (`cce://history/clear`,
`cce://bookmarks/remove?url=…`) — no chrome plumbing needed, and the live downloads
page just sets a 1 s `<meta refresh>` while transfers run.

The handler runs on **Servo's fetch threads**, hence the `Arc<Mutex<_>>` stores. It
therefore *cannot reach Servo itself*: `cce://cookies/clear` sets an `AtomicBool` that
the next `pump` acts on via `site_data_manager()`. Anything else needing engine access
from a page has to take the same route.

History and bookmarks are TSV under `~/.local/state/cce/browser/`; `sanitize()` strips
tabs and newlines because the format has no escaping. All four pages share the `page()`
skeleton — restyle there, not per page.

Clearing cookies is a **confirm-then-act page**, and Ctrl+Shift+Delete opens it rather
than clearing outright: sessions persist now, so an accidental chord would sign the
user out of everything.

## Downloads

Servo has no download pipeline at all. `request_navigation` sniffs the URL against
`DOWNLOAD_EXTENSIONS`, `deny()`s the navigation, and hands the URL to a `reqwest`
blocking worker that streams into the download dir. Note the sniff is **extension-only**
— there is no `Content-Disposition` or content-type handling, so a download URL with no
recognizable extension navigates instead.

`Download::id` exists because `clear_finished` shifts Vec positions; worker updates
must never carry an index across a lock boundary.

## Settings and profile state

`~/.config/cce/cce-browser/config.kdl`, section `browser`, read at startup and re-read
in `handle_focus_change` — so edits made in **cce-system-interface's Browser page**
(`../cce-system-interface/src/pages/browser.rs`, which owns the writing side) apply on
the next switch back. Keep the key names in `settings.rs` and that page in sync;
`external-browser` is currently read here with no UI writing it.

Servo persists per-profile state (cookie jar, auth cache, HSTS) only when given a
`config_dir` — without one every launch starts logged out of every site. It lives at
`~/.local/state/cce/browser/profile`, forced to `0700` because **the jar is plaintext
JSON holding live sessions**.

Two engine-level settings have sharp edges, both documented at length in `webview.rs`:

- **CSS Grid ships disabled in Servo** (`layout.grid.enabled`), so every
  `display: grid` was refused and fell back to block flow. It is turned on explicitly
  in `Preferences`; other modern-layout gaps are likely the same kind of default.
- **Force-dark schedules *two* reloads**, at 400 ms and 2.5 s. User content reaches the
  script thread as a separate message, and force-dark also flips the reported scheme
  (it reports *light*, so pages render the light theme the filter then inverts). Either
  in-flight change can land after a too-eager reload, leaving a page inverted the wrong
  way with nothing to reload it again. Both deadlines are needed; don't collapse them.

Servo's own arboard-backed clipboard delegate lands nothing on the clipboard in this
embedding (verified by reading the seat's clipboard back), so `CceClipboard` routes
through `cce_ui::widget::clipboard` — which also keeps the browser on the same
clipboard path as the rest of the DE.

## Not implemented yet

Worth knowing before assuming a bug: no find-in-page, no zoom, no context menu, no
favicons, no history/URL autocomplete, and no delegate hooks for JS dialogs
(`alert`/`confirm`), permission prompts, or HTTP auth. Ctrl+Shift+O ("hand this page to
another browser") is the deliberate escape hatch for pages Servo cannot follow, such as
a Cloudflare challenge that never completes.

Also note `parse_startup_arg` is **not** `parse_url_input` and the difference is
tested: the URL bar turns a dotted, space-free word into a domain guess, which would
mangle the local file path a launcher is allowed to pass for `%u` into
`https:///home/me/page.html`.
