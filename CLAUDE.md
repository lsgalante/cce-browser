# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`cce-browser` is a web browser for the CCE Wayland desktop environment, built on
**embedded WPE WebKit** (since 2026-08-30; the original Servo backend survives behind
a feature flag — see WPE-PORT.md for the whole port). It is one crate of the multi-repo `cce` workspace (its
own git repo side-by-side with its siblings; published read-only at
`https://git.lucas.co/cce-browser.git` via gitsite — the local repo is the source of
truth, there is no push remote). Read the workspace-level
`../cce-compositor/WORKSPACE.md` first: workspace layout, the `cce-ui` toolkit, config
conventions, and the multi-repo rules all live there.

Seven files, ~2.9k lines:

| file | what it owns |
| --- | --- |
| `src/main.rs` | `BrowserApp` — the `cce-ui` `Application`: chrome layout, hit-testing, the URL line editor, key/pointer routing |
| `src/instance.rs` | single-instance forwarding: a later launch hands its argument to the running instance's socket and exits |
| `src/bin/open.rs` | `cce-browser-open`, the desktop entry's `Exec` target: a ~500KB forwarder linking only libc (~4ms vs ~22ms through the full binary), exec'ing `cce-browser` when no instance answers |
| `src/webview.rs` | `ServoHost` — Servo boot, the delegate, one `WebView` per tab, the frame pipeline |
| `src/pages.rs` | the `cce:` protocol handler and its History / Bookmarks stores |
| `src/downloads.rs` | the chrome-side download pipeline (Servo has none) |
| `src/settings.rs` | the per-app KDL config |

## Build

The **default build is the WPE WebKit browser**: seconds to compile, ~14 MB linked
against the system `libWPEWebKit` (`pacman -S wpewebkit` is the one prerequisite).
That default is deliberate and load-bearing — while WPE was opt-in, a routine
featureless rebuild by another session silently reverted the installed browser to
Servo within two days. WORKSPACE.md's old "leave cce-browser out of `cce-ui` sweeps"
rule was about Servo's build cost and no longer applies to the default build.

```sh
cargo build --release -p cce-browser     # WPE WebKit (default); shared ../target/
cargo test --release -p cce-browser      # release, or it builds Servo-debug from scratch
ccebuild install --no-build cce-browser  # install binary + desktop entry
```

**`--no-default-features --features servo` builds the retired Servo backend**, and
*that* is the expensive one: it compiles Servo (more than the rest of the workspace
combined, ~175 MB binary). Only pay for it deliberately. A last-known-good Servo
binary sits at `~/.local/state/cce/browser/cce-browser-servo-fallback`.

There is **no `Makefile`** here (most siblings have one) — install goes
through `ccebuild` directly. `Cargo.lock` is gitignored in this crate. Running needs a
live Wayland session; it will not run headless.

The engine-specific sections below (frame pipeline, tabs, key routing) describe the
**Servo backend** (`src/webview.rs`, feature `servo`); the WPE equivalents live in
`src/wpe/` and are documented in WPE-PORT.md. `servo = "0.4"` comes from crates.io,
not a git pin. Servo's embedding API churns
hard between releases, so when a version bump breaks the build, expect the delegate
trait, the input-event constructors, and `Preferences`/`Opts` to be where it broke.

## Single instance

An external open (the desktop entry's `%u`) spawns a fresh process per link.
`src/instance.rs` turns that into a tab: `main()` tries
`/tmp/cce-browser-<WAYLAND_DISPLAY>.sock` (the standard `cce_ui::ipc`
convention; display keying isolates shadow sessions) before any engine or
Wayland work, forwards `open <arg>` / `new-tab` and exits on success, or binds
the socket and becomes the instance. The desktop entry's `Exec` is
**`cce-browser-open`** (`src/bin/open.rs`), a forwarder that links only libc —
the full binary spends ~20ms loading libWPEWebKit before `main()` runs, the
slim bin forwards in ~4ms — and execs `cce-browser` when nothing answers. It
deliberately duplicates the tiny client protocol rather than import anything;
keep it, `instance.rs`, and the socket-path convention in agreement. The listener thread pushes
`Message::OpenExternal` into calloop; `update()` parses the relayed argument
with `parse_startup_arg` — it *is* a launch argument, so the URL bar's
domain-guess parsing stays wrong for it — and a forwarded relative file path is
canonicalized on the *sending* side, whose cwd it is relative to. Beyond
tidiness this guards the profile dir: two engines must not share the plaintext
cookie jar. There is deliberately no `--new-window` yet; raising the existing
window on forward is also still open.

The other half of click-to-tab latency is inside WebKit: creating a webview
and spawning its WebProcess is ~200ms, so the WPE host keeps a hidden **spare
webview** prewarmed on about:blank and `open_tab` adopts it (see the `spare`
field in `src/wpe/host.rs`). Measured end to end: a link is a live tab in
~65ms (internal page) / ~120ms (example.com, warm) against ~250/~400ms
without. WPE's platform API has no
`webkit_web_context_prewarm_spare_web_process` (the GTK port's answer), which
is why the spare is hand-rolled.

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
`cce://bookmarks/remove?url=…`) — no chrome plumbing needed.

### Servo leaks a document per load — the biggest live hazard

Measured 2026-08-27: a page on a 1 s reload loop grows RSS ~0.9 GB per 90 s, linear,
never reclaimed. Isolated cleanly — the same page's JS churn *without* the reload is
flat, and an animation-heavy real page (cloudflare.com fully loaded) is flat. It is
navigation that leaks, not script. This is upstream in Servo and not fixable here.

In the wild it took the whole machine down: a **Cloudflare interstitial**
(`"Just a moment..."`) re-runs itself waiting on a browser-integrity check Servo can
never pass, and reached **54 GB RSS in ~6 minutes** — 83% of a 62 GB box, everything
stalling on reclaim. Ctrl+Shift+O (hand the page to another browser) is the escape
hatch, and the reason it exists.

**`cce://downloads` is the same hazard in our own code**: it carries
`<meta http-equiv="refresh" content="1">` while any transfer is active, so watching a
long download leaks at the rate above. Fixing it means live progress without a
navigation, and the obvious route is closed — **`fetch()` cannot reach a `cce:` URL**
(tried, including with `Access-Control-Allow-Origin: *`; the protocol registry appears
to serve top-level navigations only, and the fetch just rejects). A fix needs either a
real localhost HTTP endpoint the page can fetch, or progress moved into the chrome.
Until then, don't add a self-refreshing `cce:` page, and know this one is live.

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

Servo has no download pipeline at all, so a URL that looks downloadable is diverted to
a `reqwest` blocking worker that streams it into the download dir. The sniff is
**extension-only** (`DOWNLOAD_EXTENSIONS`) — no `Content-Disposition` or content-type
handling — so a download URL with no recognizable extension navigates instead.

It has to happen in **two places**, and that is not redundancy.
`WebViewDelegate::request_navigation` fires only for navigations the *content* starts
(a link, `location.href`). A URL the **embedder** supplies never reaches it — neither
the first tab's, which Servo loads straight from `WebViewBuilder::url`, nor one from
the URL bar — so those are sniffed in `ServoHost::take_as_download` instead. Until that
existed, `cce-browser https://…/thing.tar.gz` rendered Servo's "Unknown content type
(application/octet-stream)" page rather than downloading. A caller that takes a URL as
a download must return *without* navigating, which is what stops the two paths from
starting the same transfer twice.

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
