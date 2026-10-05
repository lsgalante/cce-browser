# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`cce-browser` is a web browser for the CCE Wayland desktop environment, built on
**embedded WPE WebKit** (since 2026-08-30; the original Servo backend survives behind
a feature flag — see WPE-PORT.md for the whole port). It is one crate of the multi-repo `cce` workspace (its
own git repo side-by-side with its siblings; `origin` is GitHub,
`github.com/lsgalante/cce-browser`, and a post-commit hook pushes each commit there —
`https://git.lucas.co/cce-browser.git` is only an hourly mirror of GitHub. **Committing
is not publishing**: a commit whose push failed (hook missing, network down) is on no
remote at all, so check `origin/master` after committing). Read the workspace-level
`../cce-compositor/WORKSPACE.md` first: workspace layout, the `cce-ui` toolkit, config
conventions, and the multi-repo rules all live there.

Eighteen files, ~11.4k lines. The eleven that carry the design:

| file | what it owns |
| --- | --- |
| `src/main.rs` | `BrowserApp` — the `cce-ui` `Application`: chrome layout, hit-testing, the URL line editor, key/pointer routing |
| `src/instance.rs` | single-instance forwarding: a later launch hands its argument to the running instance's socket and exits |
| `src/bin/open.rs` | `cce-browser-open`, the desktop entry's `Exec` target: a ~500KB forwarder linking only libc (~4ms vs ~22ms through the full binary), exec'ing `cce-browser` when no instance answers |
| `src/webview.rs` | **retired backend, behind the non-default `servo` feature** — `ServoHost`: Servo boot, the delegate, one `WebView` per tab, the frame pipeline. Not built by `cargo build`; see WPE-PORT.md |
| `src/pages.rs` | the `cce:` protocol handler and its History / Bookmarks / Favorites stores |
| `src/downloads.rs` | the chrome-side download pipeline (Servo has none) |
| `src/session.rs` | open-tab persistence: the tab set survives a restart |
| `src/settings.rs` | the per-app KDL config |
| `src/accounts.rs` | accounts from cce-secrets: the Secret Service worker, which entries a host earns, saving a new login, and the never-save list |
| `src/wpe/formwatch.rs` | the page half of account autocomplete: the watcher every frame runs (fields, frame-offset relay, fill asks, sign-in capture) and the events it sends |
| `src/raindrop/` | bookmark sync with Raindrop.io's Unsorted: the three-way merge (`mod.rs`), the REST client (`api.rs`), the worker and status line (`sync.rs`) — design in RAINDROP-SYNC.md |

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
cookie jar. There is deliberately no `--new-window` yet. On every forwarded
open the app asks the compositor to `focus-window cce-browser` over the
control socket — focus pans the camera to the window, which is what makes a
forwarded link *visible*; without it the tab opens in a window parked
off-camera and the click looks inert (that shipped for half a day). The
compositor's xdg-activation is not the route: it deliberately answers with an
attention notification, not focus.

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

## The frame pipeline on WPE

WebKit renders into a **mappable SHM buffer** (`toplevel_formats` asks for
one; DMABuf import is still the Phase 2 in WPE-PORT.md), `read_shm` copies it
out, and the pixels are drawn as one full-bleed quad. What that path costs is
worth knowing, because it is paid on **every frame of every scroll** and this
display is 3840x2400 — 35 MB a frame fullscreen.

Three things it deliberately does *not* do any more, each measured at that
size before it went:

- **No CPU swizzle.** `WPE_PIXEL_FORMAT_ARGB8888` is BGRA in memory and is
  handed over as `PixelFormat::Bgra`; the sampler reads either channel order
  at no cost. Rearranging the bytes cost **7.4 ms a frame**.
- **No per-frame allocation.** The destination comes from
  `cce_ui::vk::recycle_buffer`, and the sink refills the *superseded* frame's
  buffer rather than dropping it — when the engine outruns `pump`, which is
  exactly when frames are being thrown away, allocating a new buffer each time
  would be the most expensive possible way to discard work. A fresh Vec per
  frame was **4.5 ms**, nearly all zeroing and page faults.
- **No copy for `sample_pixel`.** It used to clone the whole frame to answer a
  three-byte question (only `examples/wpe_dark.rs` asks); `last_pixel` keeps
  the three bytes instead. That clone was **7 ms a frame**.

And on the GPU side `pump` calls `update_pixels` when the tab already has an
image of the same size, so the frame replaces the contents of one texture
instead of creating an image and freeing last frame's — that free took
`device_wait_idle`, once per frame. Only a resize (or a tab's first frame)
takes the create path.

What is left per frame: one memcpy out of SHM (whole-buffer when the stride is
tight, per row otherwise), one memcpy into the shared staging buffer, and the
transfer. The remaining `queue_wait_idle` inside the toolkit's update is
explained there. **Do not reintroduce a `Vec` allocation, a swizzle, or a
second copy on this path without measuring** — the numbers above are what each
one costs.

### The readback is paced to draws

`render_buffer` says the two halves of the buffer protocol at different times,
and that is the pacing. `wpe_view_buffer_rendered` — *displayed* — is said at
once, so the engine's own frame pacing never waits on us.
`wpe_view_buffer_released` — *the memory is yours again* — waits until the
pixels have been copied out, which happens in `pump`, not in the callback.
(Saying neither is what stalls the engine after exactly one frame; that is what
the old comment here warned about.)

Holding the buffer buys two things. A frame superseded before anyone read it is
handed back **unread**, so several frames dispatched inside one pump's drain
cost one copy rather than N. And `pending_draw` gates the readback on the
chrome having actually drawn (`frame_drawn`, called from `display_list`): while
nothing has drawn the last frame, the next one is left held rather than copied
over a picture nobody saw.

That second half is where the win is, and it is not the one first expected.
Measured in a shadow against a page animating at 63 fps: **visible and drawing,
62 of 63 frames are read — the pacing changes nothing**, because `pump` is what
dispatches the engine's frames and it dispatches them promptly, so the engine
never gets ahead. **Minimized, 4 of 64 are read** — 60 handed back unread, a
page that used to cost its full window size sixty times a second while nobody
was looking. Restoring recovers the full rate within a second, with live
content.

`CCE_BROWSER_FRAME_DEBUG=1` logs the two counts once a second; the gap between
them is invisible from the outside, since a browser that skips nine frames in
ten looks exactly like one that copies all ten.

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

### Session restore

The open-tab set persists across restarts: `src/session.rs` writes
`~/.local/state/cce/browser/tabs.tsv` (one `<active-flag>\t<url>` line per tab)
and startup restores it, engine-agnostically — the chrome reads tabs back
through the shared host surface, so both backends get it for free. Points that
are choices, not accidents:

- **Saves are eager, not on-exit** — `persist_session()` fires on every tab
  open/close/switch and on navigation (via the Spin-dirty path), so a crash or
  a compositor-side window close loses nothing. `Session::save` compares
  against the last serialization and skips no-op writes, which is what keeps
  the loading-time signal storm off the disk.
- **Closing the last tab saves the empty set** before `Message::Quit`, so a
  deliberately emptied browser starts fresh on the homepage instead of
  resurrecting what was just closed. Quitting via the window close keeps the
  tabs (they were never closed).
- **A launch argument opens as an extra tab on top of the restored set**; only
  when there is nothing to restore does it become the single starting tab
  (then falling back to the homepage, as before).
- **`about:blank` tabs are skipped on save** — a "New Tab" is not worth
  resurrecting.
- Restore is **eager**: every saved tab starts loading at launch (one
  WebProcess each on WPE). Fine at normal tab counts; lazy restore is the
  upgrade path if someone lives with dozens.

### A dead or hung page

A tab's WebProcess can die (crash, memory limit) or hang (a deadlock inside
WebKit/Mesa froze one for good on 2026-10-05: main thread on a driver lock,
zero CPU, the UI process perfectly idle — it looked like the browser was
stuck). `src/wpe/host.rs` handles both; `examples/wpe_crash.rs` proves them
against the real engine. Points that are choices:

- **A dead process gets an error page under the dead page's URL**
  (`web-process-terminated` → `terminated_page`, loaded as alternate HTML in
  `pump`, not inside the signal). The URL bar, the saved session and Reload
  all still mean the real page. The real URL is passed as the *base* URI too:
  without it the error page is `about:blank` to itself and its Reload link
  goes nowhere (refused outright for a `file:` page).
- **A hang raises a modal** ("This page isn't responding": Stop page / Wait).
  Stop kills the process with `webkit_web_view_terminate_web_process`, which
  lands in the error page above. Wait (or Escape) is remembered per tab until
  the page answers again. **Enter does not stop**: the question can appear
  mid-typing, and an Enter aimed at the page must not destroy what was typed.
- **WebKit's own responsiveness timer misses the commonest hang.** Pointer
  events are queued behind an unacknowledged one, and a *move* — which always
  precedes a click — does not start the timer, so the clicks behind it are
  never even sent. Keys and the wheel are caught; move-then-click was not.
  Every page press therefore also sends a no-op script in a private world
  (`ping`), and a ping unanswered for `HANG_GRACE` (3s) is a hang. A one-shot
  GLib timeout wakes the loop when the grace runs out, since a hung page
  sends nothing that would. A pending `alert()` or auth challenge is never a
  hang — the page is blocked on *us* — and answering one forgets the ping
  that was waiting behind it.
- **A deadlock is recovered without asking** (`watch_deadlock`). A hung page
  and a spinning script look alike from here; the difference is CPU: the
  2026-10-05 deadlock sat at zero, a script burns a core. WebKit says nothing
  about which process serves which tab, so every WPEWebProcess under the
  browser is sampled from `/proc`, and only if *all* of them stay under 5% of
  a core for `DEADLOCK_WATCH` (5s) is the process stopped and the page loaded
  again — a plain load, so a form post is not resubmitted. Busy, waited on
  ("Wait"), or already recovered within `AUTO_RECOVER_GAP` (2 min) is left to
  the prompt, so a page that deadlocks on every load cannot reload-loop.
- **The deadlock itself is a Mesa iris bug, still on Mesa `main`**: a lock-order
  inversion between the aux-map mutex and `bufmgr->lock`, hit by WebKit's two
  Skia GPU painting threads allocating textures at once (one adds an aux
  mapping and needs a new table page; the other reuses a cached BO and unmaps
  its old aux range). `main()` therefore sets `INTEL_DEBUG=noccs` before
  anything starts (`disable_intel_ccs`): without CCS the aux map is never
  used. Page processes inherit it through bubblewrap. Measured in a scale-2
  shadow scrolling 24 large images: no difference in frame rate or CPU.
  `CCE_BROWSER_CCS=1` turns compression back on.

## The chrome is hand-rolled

There are **no `cce-ui` widgets in this app**. The whole utility bar is emitted as
`PaintCtx` primitives in `display_list` (`display_list_text()` returns `true`), and
every hit test in `handle_mouse_input` re-derives the same rects from the same
`bar_rect`/`tab_rect`/`btn_rect`/`url_rect`/`fav_rects` helpers. **Draw and hit-test are two
readings of one geometry** — change a rect helper, not one call site.

### Favorites are not bookmarks

Two stores, two meanings. The **star** (`Ctrl+D`, `cce://bookmarks`) is the
archive: everything worth finding again, newest first. **Favorites**
(`Ctrl+Shift+D`, the right-click menu's "Add to Favorites", or the
`favorite` link on a bookmark row; managed at `cce://favorites` /
`about:favorites`, `Ctrl+Shift+B`) are the handful of places worth a
permanent one-click spot: a **strip of label pills inside the bar**, between
the tab row and the controls row. Click loads the favorite in the active tab
and folds the bar (a menu pick); middle-click opens it in a background tab
and leaves the bar out. Insertion order is strip order; the page reorders
(▲/▼), renames (a GET form per row — form submissions reach the `cce:`
handler like any other navigation) and removes.

### The bookmarks menu

The controls row's **"B" button** (immediately left of the star) drops the
bookmarks menu: the star is *this* page's bookmark, the button beside it is
all of them. A **search field** on top, then three sections — add/remove
this page, the saved pages themselves (newest first, the `cce://bookmarks`
order), and `Manage Bookmarks (n)` which hands the collection to that page. A row visits
in the active tab and folds everything away, middle-click opens it in a
background tab and leaves the menu up, and the **remove "x"** on the hovered row prunes
in place. It closes on Escape (ahead of the URL bar and the page), on a
click anywhere off its plate, and with the bar it hangs from.

Points that are choices, not accidents:

- **It snapshots the store when it opens.** A list a pointer is travelling
  down must not reorder underneath it, so the two edits it offers re-read
  explicitly (`refresh_bm_menu`) rather than the paint path reading the
  store every frame.
- **It is not gated on an engine backend**, unlike the right-click menu:
  bookmarks are app state, so both hosts expose `bookmarks()` and the menu
  works on either.
- **`bm_layout()` is the one geometry** draw and hit-test both read — plate,
  toggle row, visible entry rows, manage row. It hangs off the button
  (**below** a top bar, **above** a bottom one), right-aligned to it and
  clamped on screen, and never grows past the space it has: `cap` is how
  many rows fit and the list **scrolls** past that, wheel included, rather
  than the plate running off the window.
- **An open menu owns the pointer**: clicks, moves and the wheel all stop at
  it, exactly as the right-click menu already did, so the page behind never
  sees a click that was meant to dismiss a menu.
- Rows are drawn at **full brightness**. Dim means *unavailable* everywhere
  else in this chrome (the disabled toggle on an internal page says so that
  way), and hover is the highlight rect's job.
- Opening drops URL-bar focus, for the same reason folding does: a field
  behind a menu must not keep eating keystrokes.
- **The search field has the keyboard while the menu is open** (a
  `LineEdit`, so caret, selection, clipboard and undo come with it). Every
  word typed must appear in a bookmark's title or address, ignoring case;
  Up/Down move a selected row through the matches (`BmMenu::step`: it wraps
  at the ends and scrolls to stay in view, as the account list does, and is
  highlighted like a hovered row); Enter visits the selected row; Escape
  still closes the menu; the chrome's Ctrl chords still fire first, as they
  do over a focused URL bar. A query edit — a key, a paste, an undo —
  re-filters (`BmMenu::filter`) and takes the list and the selection back to
  the first match. `all` is the snapshot, `items` what the query
  lets through, and an `Entry` index points into `items`.
- **The plate is sized for the whole collection, not the matches.** Above a
  bottom bar a plate that shrank as the query narrowed would slide its
  search field out from under the pointer mid-typing (it did, in the first
  cut); unmatched slots are left empty instead. `Manage Bookmarks (n)`
  counts the whole collection for the same reason it exists.

`Ctrl+B` still opens the `cce://bookmarks` page rather than this menu — the
page is the fuller tool, and the menu is a pointer affordance.

### Favorites geometry

Geometry points that are choices: the bar has **no empty row** — with no
favorites it is the two-row bar it always was (`bar_h(favorites)`), so
`controls_y` is measured from the bar's *bottom* edge rather than counted
down from the top. The strip does not scroll or wrap: pills take their
label's width up to `FAV_MAX_W`, and `fav_rects` simply stops at the bar's
edge, so a too-long strip loses its tail. The chrome keeps a snapshot
(`favs`) refreshed with the rest of the page state, which is also how edits
made on the `cce://favorites` page — on the way into a navigation — reach
the strip. A label defaults to the page title, else the host (`www.`
stripped), else the file name; internal pages are refused.

### The bar is a circle menu

The chrome's persistent element is a **corner control** — in the place of the
DE's dot (`cce_ui::widget::plate_dock::draw_corner_dot`, on a designer pane or
the terminal window), but drawn as a **circular plate**: the bar's own material
(`BAR_FILL`) and rolled bevel, `plate_shaped` with corner exponent 2 — sitting
in the **bar's corner nearest
the window corner it is anchored to** (`dot_center()`: top-right for a top bar,
bottom-right for a bottom one). It is drawn here at **1.5x the DE's size**
(`DOT_R`, 12px radius against `plate_dock::CORNER_R`'s 8, with `DOT_INSET`
keeping the DE's margin to the plate edge): folded, it is the whole chrome, and
the pane-corner size was too small to find. The size lives in this crate, not
in `cce-ui` — the shared constant is every other app's dot too — so the drawing
and the hit circle are the browser's own (`dot_hit`), same disc. Being a plate
rather than a flat plate-border disc (it was one until 2026-10-05) is what makes
it read as the bar in miniature: folded, it is exactly the seed shape the bar
unfolds from; open, it is a raised plate on the bar's corner. A
circle menu is the corner of the thing it expands into, so the dot sits where
the bar's corner will be, the bar grows out of the dot's own disc, and open,
the dot is the bar's corner. It is always drawn and always live: clicking it
unfolds the two-row bar, clicking it again folds the bar back. `chrome_open` names the state, `chrome_t` the unfold
progress (animated in `tick` over `CHROME_ANIM_S`, or landed in one frame
when the DE's animations switch, `cce_ui::motion::enabled`, is off), and
`dot_hover` its hover emphasis, which is a repaint. **Do not decorate the dot** — no glyph, no
lines, no ring; it is a plate, not a browser icon.

`chrome_plate()` is the one shape draw and hit-test both read — the bar, or
the lerp from the dot's disc up to the bar — and
`chrome_hit()` is the chrome's pointer gate (the dot always, the plate while
any of it shows). The bar's contents are laid out at their *final* rects and
clipped to the growing plate, so the unfold is a reveal, not a re-layout. The
row the dot sits on reserves `DOT_COL` at its right end (`dot_col`: the tab
row's "+" for a top bar, the controls row's star for a bottom one);
`bar_rect` and the rest of the helpers are otherwise unchanged. The plate is
drawn through `plate_shaped`, a per-plate corner exponent added to `cce-ui`,
easing from circular at the seed to the DE's own squircle as it becomes the
bar.

Menu semantics, all in `handle_mouse_input` / `handle_key_input`:

- **Open**: click the dot; `Ctrl+L` (then focuses the URL); `Ctrl+T` (a new
  tab focuses the URL field, which must be on screen). The bookmarks menu
  is a second layer inside the open bar, and folding takes it with it.
- **Fold**: click the dot again; click the page; `Escape` with the URL
  unfocused (the first Escape in a focused field only drops focus, as
  before); submitting a URL; picking a tab. Closing a tab does *not* fold —
  several often go in a row.
- **Peek**: a tab opening in the background (`open_background_tab`) unfolds a
  folded bar for `CHROME_PEEK` (1.5s), so the new tab is seen landing in the
  strip, then folds it again. `chrome_peek` is the deadline; a thread sends
  `Spin` when it passes (an idle page turns no loop) and `settle_peek` folds.
  Any press on the bar, or any explicit open, makes it the person's and it
  stays; a pointer *brought onto* it holds the fold off; a page click does
  not fold a peek, so a run of middle-clicked links keeps it out instead of
  flapping it. A bar the person already had open is left alone.
- Folding drops URL-bar focus (`close_chrome`), so an off-screen field never
  keeps eating keystrokes. Wheel and pointer moves over the chrome stay off the
  page, gated by `chrome_hit`, not `bar_rect`.

### Select lists

**WPE draws no `<select>` popup of its own.** A click on a select raises
`show-option-menu` with the options and the select's box, and a select
nobody answers simply never opens — which is how every dropdown on every
page was dead until 2026-10-05. `on_show_option_menu` (`src/wpe/host.rs`)
holds the `WebKitOptionMenu` and hands the chrome an `OptionMenuInfo`;
`OptMenu` in `main.rs` draws the list at the select and answers with
`pick_option` (activate + close: the value changes and `change` fires) or
`close_option_menu`. Points that are choices:

- **`select_item` is never called.** WebKit's `close` activates whatever is
  selected, so moving WebKit's selection with the highlight would make
  Escape commit the row it was on. The highlight is the chrome's alone.
- **The page can close it too** (the select is removed, the page
  navigates): the menu's own `close` signal drops the held menu, and the
  next `Spin` sees `option_menu_open()` false and takes the list down.
- **The anchor is logical pixels** — the chrome's — at any output scale
  (verified at 2), like the account list's field rects.
- **`opt_layout()` is the one geometry**: under the select, or over it when
  there is more room above, never past the window; a longer list scrolls
  (wheel, arrows, paging), with a thumb. Wheel travel accumulates in
  `wheel_rest` — a trackpad's few pixels an event, rounded one at a time,
  never moved it.
- It **owns the pointer and the keyboard** while up, like the right-click
  menu: a click off it closes it and goes no further (so clicking the
  select again folds it), and no key reaches the page or the chrome's
  chords. Up/Down/PageUp/PageDown/Home/End skip disabled options and
  optgroup headings; Enter (or Space, outside type-to-find) picks; Escape
  and Tab close; letters find (`find_typed`: "1","0" finds 10, a repeated
  letter cycles).
- A tab switch, a tab close, losing window focus and a resize all close it.

`examples/wpe_options.rs` proves the engine side against the real engine.

### Middle-click is a background tab

Every "open this in a new tab" that is not the person asking for a fresh tab
opens **behind** the page: a middle-clicked link in a page, a middle-clicked
favorite or bookmark-menu row, and the context menu's "Open Link in New Tab".
The active tab, its focus and the URL bar stay as they were, and the bar
peeks (above). `Ctrl+T`, the "+" and a forwarded external open still open in
front — those are asks to *go* somewhere.

A page link reaches the chrome through WebKit's `decide-policy`
(`on_decide_policy`): a link-click navigation — or new-window one, for
`target=_blank` — carrying mouse button 2 is ignored and its URL queued
(`take_background_opens`, drained in the `Spin` handler). Everything else gets
WebKit's default. A background webview is left unmapped and invisible, the
state a switched-away tab is in. `examples/wpe_middle.rs` proves the engine
side (both link kinds diverted, left-click undisturbed, the tab loading
behind the page). On the retired Servo backend `open_background_tab` is an
open followed by switching back, and page middle-clicks are not caught.

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
event: the engine hit-tests the wheel, gives the page its `preventDefault` chance, and
applies the inverted delta itself.

### The wheel eases; the trackpad does not

A notch used to move the page `LINE_PX` in one step, which is the browser
feeling unlike every other cce app. It now goes through
`cce_ui::widget::scroll_motion` — the DE's shared model, tuned by
`smooth_scroll` / `scroll_ease` in `input.kdl` (this app's domain, then
`cce-ui`'s) — so notches glide and several in a row accumulate into one
movement instead of a staircase.

The browser does not own the page's offset, so the model runs as a **virtual**
one: `apply` moves its target, `tick` walks the eased position, and
`advance_scroll` hands the engine the *difference* since the last frame. WebKit
keeps the real position and clamps it at the page's ends, which is why the
bounds here are `UNBOUNDED`. The accumulator is rebased to zero whenever the
glide settles, and dropped on a tab switch or a real navigation, so deltas
aimed at one page never land on the next.

Points that are choices:

- **Only a notch eases.** A trackpad's pixel deltas already follow the finger,
  and the engine runs its own kinetic scrolling off the gesture phases
  `host.wheel` passes it. Two coast models fighting over one page would be
  worse than either, so `Finger` and `FingerEnd` keep the direct path.
- **The phase is set explicitly before each glide frame** rather than
  inherited: a stale `FingerEnd` would tell the engine every frame that a
  gesture had just ended.
- **`tick` asks for a rebuild while the glide is in flight**, the same way the
  chrome's unfold does — that is what keeps the runner's loop turning.
- Settings are read once per process (`scroll_settings` is a `OnceLock`), so a
  change to `input.kdl` needs a restart.

Measured in a headless shadow at scale 2, one notch: eased it walks
92 → 123 → 145 → 159 → … → 190; direct it lands on 190 immediately. Three
notches in quick succession land exactly three notches on (190 → 760), so the
distance a notch travels is unchanged — only its timing. (The 190 is WebKit's
own multiplier on a precise delta; the direct path has always moved that far.)

## Account autocomplete (cce-secrets)

A login field on a page gets a list of the accounts the keyring holds for that
site; picking one fills the username and password. A sign-in the keyring does
not know yet is offered for saving (Save / Never / Not now), and a saved entry
is offered from then on. There is no cce-secrets
*protocol* — that app fronts the freedesktop **Secret Service** (gnome-keyring
here) and so does this, reading the same entries: item label as the title,
`UserName` and `URL` as attributes. `browser.accounts` (default true) is the
one switch; with it off nothing is injected and the keyring is never opened.

Three files meet: `accounts.rs` (which entries a host earns, and the worker
that reads them), `wpe/formwatch.rs` (the page half), and `AcMenu` in
`main.rs` (the list itself, drawn at the field like every other menu here).
WPE only — the retired Servo backend has no user-script hooks — so the chrome
side is `#[cfg(feature = "wpe")]`, while `accounts.rs` is not.

The security shape is the design, not decoration:

- **Everything runs in a private script world** (`formwatch::WORLD`). The page
  cannot see or replace the watcher's helpers, so it cannot hook the moment a
  credential is filled, and it cannot post on the chrome's message channel to
  fake a focused field.
- **Every frame, matched by the frame's own origin.** Sign-in forms are often
  an iframe from another site (iCloud's is `idmsa.apple.com` inside
  `icloud.com`), and that frame is where the password goes, so accounts are
  matched against the *frame's* host. Inside the private world
  `location.origin` is the frame's real origin, so it can be believed; a top
  frame must still be on the tab's own host. For a frame from another site the
  page's own accounts are offered too, after the frame's, under a "sign-in form
  from <host>" line — a deliberate widening (it would hand the page's password
  to the embedded site if picked) that is what makes iCloud work with an
  `icloud.com` entry.
- **A frame places itself by relay.** It knows its field only in its own
  viewport, so each frame announces a random token to its parent with
  `postMessage`; each parent's watcher finds the sending frame by
  `event.source`, adds its offset and passes it up, and the top reports a
  `Frame` offset the chrome keeps in `frame_offsets`. Only geometry travels
  that way (a forgery misplaces the list, nothing more), and the watcher
  swallows those messages so the page never sees them.
- **A fill is an answer, not a script.** The chrome can evaluate script only
  in the top frame, so a focused field *asks* to be filled on a reply channel
  (`FILL_CHANNEL`); the host holds the newest asks by token and a pick answers
  exactly the one the list was opened for, with the credential as data. No
  password is ever spliced into script source.
- **Only a focused document speaks.** The channels are shared by every tab and
  say nothing about which one spoke, so watchers report and ask only while
  `document.hasFocus()` — true only in the shown tab, now that the window's
  focus reaches the page (`WebKitHost::focus`) — and a tab switch drops every
  queued event and open ask.
- **Matching is narrow** (`Account::matches`): exact host, or a *parent* domain
  covering its subdomains — never upward, never sideways. An entry with no URL
  falls back to its title against the site name (`GitHub` → `github.com`), the
  one guess in here, made only when there is nothing better.
- **No password is fetched to build a list.** Listing reads labels, usernames
  and URLs; the pick is what asks the keyring for one secret, by object path.
  `accounts::Secret` prints as `Secret(…)` so a derived `Debug` on `Message`
  cannot spill it into a log.
- **The fill is re-checked when it lands.** An unlock prompt can put seconds
  between the pick and the answer, so `fill_account` drops the credential
  unless the list is still open, still holds that account, the tab is still
  on the host it was opened for, and the asking document's token still has
  an ask open.
- **Never automatic.** Nothing fills without a pick, nothing submits the form,
  and a locked collection is skipped rather than unlocked — the browser asking
  for the keyring password because a page happened to show a login field would
  be its own phishing lesson. cce-secrets is where unlocking belongs.
- The list says so when the page is not https and not loopback
  (`insecure_origin`): the password would cross the network in the clear, and
  only the person can decide that is fine.

Things that were learned the hard way and are easy to undo:

- **The keyring is read on the first login field, never at launch.** A browser
  that never sees one never opens the store, which is what keeps this from
  costing an unlock prompt at login.
- **A field can be focused before the index has finished loading** — it always
  is, on a page that autofocuses. The chrome keeps its own copy of the last
  field report (`last_field`) and replays it when the index arrives; without
  that the first login form of a session silently gets nothing. (A replay, not
  a rescan script, because the field may be in a frame the chrome cannot run
  script in.)
- **The engine's dirty flag is not a navigation.** Clearing the list on
  `dirty` closed it in the same pump that opened it (title and loading
  transitions set it too). It is keyed on the tab's URL actually changing
  (`nav_url`), and form events are drained *after* that check so an event
  arriving with the load survives it.
- **A fill must not report itself.** The `input` and `change` events the fill
  dispatches — which are the point, since frameworks ignore a plain assignment
  — came back as "the user typed" and re-opened the list, filtered by the name
  just filled in. The watcher holds a `filling` flag across the fill.
- **CSS pixels are the chrome's logical pixels.** `resize` hands WPE the
  *logical* size and sets the scale separately, so a viewport rect from the
  page needs no conversion at any output scale (verified at scale 2).

### Saving a new login

The watcher reports a sign-in going out — a form's `submit` (which fires only
once the page's own validation passed), or, on the many pages with no form,
Enter in a login field or a button that says it signs in — with the username
and password. The chrome holds it as a `SaveOffer` and asks only when the
index has no entry for that site with that username (the same set the field
would have been offered, so filling from the page's entry into a sign-in frame
is not "new"), and the site is not on the never list. Points that are choices:

- **It writes what cce-secrets writes**: default collection, the page's host as
  the title, `UserName`, `URL` = the *form's* origin (where it will be offered
  next), `text/plain`, never replacing an item. cce-keyring-sync adopts it like
  any keyring-born entry.
- **A locked collection is refused, not unlocked** — same rule as listing.
- **The offer outlives the page.** It sits in the dot's corner, survives the
  navigation a sign-in usually causes, and waits for an answer; it is not
  modal, and only a press on its own plate is its.
- **"Never" is per form host**, in `~/.local/state/cce/browser/never-save.txt`.
  There is no UI to undo it yet; delete the line.
- The typed password crosses the watcher's channel as `formwatch::Password` and
  lives as `accounts::Secret` — both print redacted — and only until answered.
- An existing entry is never updated (a changed password is not detected);
  that needs comparing secrets, which this deliberately never fetches unasked.

Testing it needs an isolated keyring, never the real one: `dbus-run-session`
plus `gnome-keyring-daemon --unlock --components=secrets`, seeded with
`secret-tool`, and the browser launched into that bus with
`DBUS_SESSION_BUS_ADDRESS`. A `file:` page will not do — its origin is `null`,
so serve the fixture over http on localhost (`127.0.0.1` and `localhost` are
two origins, which is a cross-site sign-in frame for free). Two traps: give
`gnome-keyring-daemon` a **short** `-C` control directory (a long scratch path
overflows the 108-byte socket path and fails as "Address already in use"), and
run it `--foreground` in the background so one process owns the bus name.
`examples/wpe_autofill.rs` covers the engine side (frames, relay, fill, submit,
focus gating) with no keyring at all.

## Raindrop bookmark sync

With `browser.raindrop` on, bookmarks sync with Raindrop.io's **Unsorted**
collection: a worker thread polls the in-memory bookmarks, runs a three-way
merge against `raindrop-sync.tsv` (the pairs as of the last pass), and shows
its status on `cce://bookmarks`. **Read `RAINDROP-SYNC.md` before touching
`src/raindrop/`** — every rule in the merge (identity by Raindrop id, only link
and title ever sent, the deletion guard, the base recording what *happened*)
exists because the alternative silently deletes or duplicates bookmarks. The
token is a keyring entry `service=raindrop.io` with no `UserName`;
`cce-browser --raindrop-plan` is a read-only dry run against the real account,
and `CCE_RAINDROP_API` points everything at a stand-in for testing.

## `cce://` pages

`CceProtocol` registers the `cce` scheme with Servo's `ProtocolRegistry`, so
`cce://history`, `cce://bookmarks`, `cce://favorites`, `cce://downloads` and
`cce://cookies` are **real pages fetched through Servo's network stack** and rendered
like any other. That is why every mutating action is an ordinary link
(`cce://history/clear`, `cce://bookmarks/remove?url=…`,
`cce://favorites/up?url=…`) — no chrome plumbing needed.

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

History, bookmarks and favorites are TSV under `~/.local/state/cce/browser/`;
`sanitize()` strips tabs and newlines because the format has no escaping. All the
pages share the `page()` skeleton — restyle there, not per page. A page's own
`<style>` goes in through `head_extra`, which lands *before* the skeleton's, so
an override has to out-specify it (`.e .w`, not `.w`).

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

Worth knowing before assuming a bug: no find-in-page, no zoom, no favicons, and no
history/URL autocomplete. (The context menu, JS dialogs and HTTP auth landed with the
WPE backend and are Servo-only gaps now.) Account autocomplete saves new logins but
never updates a changed password, and "never save" has no undo UI. Ctrl+Shift+O ("hand this page to
another browser") is the deliberate escape hatch for pages Servo cannot follow, such as
a Cloudflare challenge that never completes.

Also note `parse_startup_arg` is **not** `parse_url_input` and the difference is
tested: the URL bar turns a dotted, space-free word into a domain guess, which would
mangle the local file path a launcher is allowed to pass for `%u` into
`https:///home/me/page.html`.
