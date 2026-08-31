# Porting cce-browser from Servo to WPE WebKit

Status as of 2026-08-30: **WPE is the default engine.** A plain
`cargo build --release -p cce-browser` produces the WebKit browser; the Servo backend
survives behind `--no-default-features --features servo`.

The flip was forced by an incident, not a ceremony: while WPE was opt-in, a routine
featureless rebuild by another session silently reverted the installed browser to
Servo — two days after the port landed, with the user's WebKit-stored logins invisible
and the interstitial memory leak live again. An opt-in engine cannot survive a
multi-session workspace; defaults are what other sessions build.

```sh
cargo build --release -p cce-browser
```

Written 2026-08-27 as a scoping document; kept as the record of what the port
involved, what it cost, and what is left.

## Why

Servo cannot render the web we actually browse. Two independent problems, neither
fixable in this crate:

- **Coverage.** CSS Grid shipped *disabled* (`2e700fe` counted 175 refusals on one
  GitHub page, 33 on an MDN reference); that commit turned it on, but it is
  representative rather than exceptional — much of the modern platform is missing or
  off, and pages collapse into a single column.
- **Identity.** Anti-bot classifiers model known engines. Servo presents as
  `Servo/… Firefox/…`, which matches nothing, so Cloudflare's managed challenge fails
  closed and re-runs forever. Measured cost of that loop: **54 GB RSS in ~6 minutes**
  (Servo leaks a document per load), which took the whole machine down. See
  `CLAUDE.md`.

WebKit fixes both by construction: a complete engine, presenting as
`… AppleWebKit/605.1.15 … Safari/605.1.15` — a profile the classifiers model.

## Why WPE and not WebKitGTK

Both are the same engine at `2.52.6` in `extra`. The difference is the embedding
contract:

| | WPE | WebKitGTK |
|---|---|---|
| render target | you get **buffers**; you composite | a `GtkWidget` in a GTK hierarchy |
| main loop | GLib, integratable | GLib **plus** GTK's assumptions |
| toolkit dep | none | GTK4 + all of it |

cce-ui is a custom Vulkan/Wayland toolkit with no GTK anywhere, and `cce-browser`
already consumes the engine as *"give me a finished frame, I draw it as one quad."*
That is precisely WPE's contract. WPE was built for set-top boxes and embedded devices
that own their display pipeline — structurally the same shape as this DE. WebKitGTK
would mean running a GTK main loop and snapshotting a widget to recover pixels.

Rejected outright: **CEF/Chromium.** No Arch package exists at all (checked: not
`cef`, `cef-minimal`, `cef-bin`, `libcef`), so it means vendoring a ~1 GB prebuilt
tarball or building Chromium; it re-execs *your own binary* for subprocesses, which
restructures `main()` around a Wayland client with a custom `calloop` loop; and it
ships a runtime data bundle you must locate at startup. `qt6-webengine` is the
cautionary tale — the Qt Company, full-time, still vendors 58 `.pak`/`.dat`/`.bin`
files and a helper binary, at 282 MB. `webkitgtk-6.0` ships **zero** such files.

## Packages

```sh
pacman -S wpewebkit          # extra, ~137 MB; pulls libwpe + wpebackend-fdo itself
```

Declared like `cce-compositor` declares wlroots: a system prerequisite, `pkg-config`
in `build.rs`, bindgen over the headers. This **deletes the Servo build** — the crate
stops being a 5-minute, 175 MB outlier and becomes a normal small crate.

## API generation: WPEPlatform — resolved

`wpewebkit 2.52.6-1` ships the **new WPEPlatform API**, confirmed from the package
file list. Its `pkg-config` modules:

```
wpe-webkit-2.0            wpe-platform-2.0            wpe-web-process-extension-2.0
wpe-platform-wayland-2.0  wpe-platform-drm-2.0        wpe-platform-headless-2.0
```

One library — `libWPEWebKit-2.0.so.1` — with the backends as separate modules. The
package still *depends* on `libwpe` / `wpebackend-fdo` (the legacy path is built too),
but the port targets `wpe-platform-2.0` and never touches them directly: no
embedder-supplied backend, which was the fiddliest part of the old generation.

The 46 WPEPlatform headers map onto `ServoHost` almost object for object:

| WPE | replaces |
| --- | --- |
| `WPEDisplayHeadless` | `SoftwareRenderingContext` — we own the display, no windowing assumptions |
| `WPEView` | Servo's `WebView`; one per tab |
| `WPEBufferSHM` / `WPEBufferDMABuf` | `read_to_image` — **both phases exist as first-class types** |
| `WPEEvent`, `WPEInputMethodContext` | `notify_input_event`; IME is a bonus we do not have today |
| `WPEToplevel`, `WPEScreen` | resize / scale plumbing |

`WPEDisplayHeadless` is the one to use: WPE composites nothing, hands us buffers, and
cce-ui draws them — exactly the current model. (`WPEDisplayWayland` exists but would
make WPE a Wayland client in its own right, which fights cce-ui compositing.) Headless
also means the throwaway spike and the shadow-session tests need no display at all.

### The frame contract, read off the installed headers

```
WPEDisplayHeadless  wpe_display_headless_new()
  └─ WPEView        wpe_view_new(display)      ← we subclass this
       └─ WebKitWebView   webkit_web_view_new(backend)
          webkit_web_view_get_wpe_view() / _get_display() tie the layers together
```

Frames arrive through the **`WPEViewClass.render_buffer` vfunc**:

```c
gboolean (*render_buffer)(WPEView *, WPEBuffer *, const WPERectangle *damage_rects,
                          guint n_damage_rects, GError **);
```

Cast to `WPEBufferSHM`, then `wpe_buffer_shm_get_data()` → `GBytes` → the pixels,
with `wpe_buffer_get_width/height` and `wpe_buffer_shm_get_stride/get_format`.

**Two things this buys us that the Servo path never had:**

- **Backpressure is built in.** You call `wpe_view_buffer_rendered(view, buffer)` when
  you are done with a buffer. The engine cannot outrun the compositor, because buffer
  lifetime is explicit and ours. That is structurally the opposite of the unbounded
  `PENDING` queue the Servo path pushes into (see `CLAUDE.md`) — the class of bug
  simply cannot arise.
- **`damage_rects`.** Partial updates are available whenever we want them; today every
  frame is a full-window repaint.

**The embedder subclasses two types, not one.** `WebKitWebView`'s `display` property
is construct-only and takes a **`WPEDisplay`**, not a view — WebKit makes its own view
by calling `WPEDisplayClass.create_view`. So we implement a `WPEDisplay` that vends our
`WPEView`, and the view overrides `render_buffer`. (`WPEDisplayHeadless` is
`G_DECLARE_FINAL_TYPE`, so it cannot be subclassed to shortcut this.)

## The embedding contract, learned the hard way

Established by a C spike (now `spike/wpe-spike.c`) and unchanged by everything built
on top of it. **The two traps below cost hours and neither produces an error message**,
so they are the part of this document most worth keeping.

Working from the start:

- `pkg-config` → compile → link against `wpe-webkit-2.0` + `wpe-platform-2.0`.
- Subclassing `WPEDisplay` *and* `WPEView`, overriding `connect`, `create_view` and
  `render_buffer`. Flagged as the main FFI risk; mechanical in both C and Rust, and
  ordinary GObject rather than a hack.
- `g_object_new(WEBKIT_TYPE_WEB_VIEW, "display", display, NULL)` — the WPEPlatform
  construction path. WebKit calls our `create_view`, and
  `webkit_web_view_get_wpe_view()` returns the instance we handed it.
- **The sandbox is a non-issue.** The full engine starts: `WPENetworkProcess` plus
  `WPEWebProcess` under `bwrap`, with no special setup. Retire that risk.
- View sizing/mapping: `wpe_view_resized` / `set_visible` / `map` all take.

**The spike renders.** `example.com` came out pixel-correct — right fonts, right link
colour, right layout. Two things were needed beyond the above, and both are
non-obvious:

- **A `WPEToplevel`.** WebKit asks the **toplevel** for buffer formats
  (`WPEToplevelClass.get_preferred_buffer_formats`), not the display. With
  `WPEDisplayClass.create_toplevel` left NULL, no formats are ever negotiated and
  `render_buffer` simply never fires — with no error. Subclass `WPEToplevel`
  (derivable; construct properties are `display` and `max-views`) and implement
  `get_preferred_buffer_formats` plus `resize`.
- **Both halves of the buffer handshake.** `wpe_view_buffer_rendered` means
  *displayed*; `wpe_view_buffer_released` means *the memory is yours again*. Calling
  only the first yields exactly one frame and then a permanent stall. Call both.

That second point is the backpressure mechanism, working as advertised: the engine
will not produce another buffer until the embedder hands one back. The unbounded-queue
failure mode is impossible here by construction.

### The staging holds: SHM is real

Buffers arrive as **`WPEBufferSHM`**, despite `wpe_display_headless_new()` advertising
54 DRM fourcc formats and inferring a DRM device — the reference display being
GPU-backed does not force the embedder to be:

```
render_buffer #2: 1200x800  type=SHM  bytes=3840000  stride=4800  format=0
```

`format=0` is `WPE_PIXEL_FORMAT_ARGB8888`; stride is `width * 4`; byte order in memory
is B,G,R,A. That is precisely the shape `cce_ui::vk::upload_rgba` already accepts.

**So Phase 1 needs no `cce-ui` change**, and the Vulkan `VK_EXT_external_memory_dma_buf`
import stays a Phase 2 optimisation rather than a day-one prerequisite in a shared
crate. (An earlier revision of this doc recorded the opposite as a live risk; the spike
settled it.)

A static page yields two frames and then quiets, which is correct — no animation, no
new frames.

## Impact map

| file | what happened |
| --- | --- |
| `src/wpe/` (new, ~900 lines) | `WebKitHost` plus the three GObject subclasses, the input mapping, and the GLib↔calloop bridge. |
| `src/webview.rs` | **kept.** `ServoHost` is still the default backend. It grew `key_ui` / `mouse_button_ui` / `editing_action_cmd` / `set_color_scheme_dark` taking cce-ui types, so both hosts present one surface. |
| `src/main.rs` | **kept**, with `Host` a compile-time alias for one backend or the other. `dom_key` and `dom_button` moved out; the chrome now names neither engine. `register_sources` is the only place they visibly differ. |
| `src/pages.rs` | **kept.** `CceProtocol::route` was extracted so both backends share one routing table — Servo through `ProtocolHandler`, WebKit through its URI-scheme callback. |
| `src/downloads.rs` | **kept**, plus `adopt` / `set_progress` / `set_finished` for engine-driven transfers. Under WPE the extension sniff is never reached. |
| `src/settings.rs` | **kept**, plus `is_dark()` replacing the `servo::Theme` conversion. |

Nothing was deleted. Both backends compile from the same source, which is why the
Servo path could stay green throughout.

Still available and **not yet taken**: JS dialogs (`script-dialog`), HTTP auth
(`authenticate`), permission requests (`permission-request`), find-in-page
(`WebKitFindController`), zoom (`webkit_web_view_set_zoom_level`). Each is a signal
away now that the host exists.

## The frame path

**Phase 1 shipped: SHM buffers straight into `upload_rgba`, no `cce-ui` change.**
Frames arrive as `WPEBufferSHM`, ARGB8888, stride `width * 4`, B,G,R,A in memory —
precisely what the registry already accepts.

Phase 2 — dmabuf imported as a Vulkan image via `VK_EXT_external_memory_dma_buf`,
skipping the CPU roundtrip — remains available and unstarted. It needs a **new
`cce-ui` API** (the registry only accepts `Vec<u8>`), and `cce-ui` is a **shared
crate**: check `git status` there and coordinate before touching it. It is an
optimisation, not a correctness fix; the port works without it.

## Bindings: hand-rolled, like wlroots

There are no usable Rust bindings.

- `wpe` — 0.0.19, last published **2023-03**. Abandoned.
- `cogcore-sys` / `cogcore` — FFI to Igalia's Cog launcher, recent (2026-08) but **92
  downloads**. Not a dependency; worth *reading* as prior art for the FFI shape.
- The `webkit` crate is macOS `WKWebView`. Irrelevant.

So: bindgen over the C headers, exactly the idiom `cce-compositor/build.rs` already
uses against wlroots. `build.rs` does this, gated on `CARGO_FEATURE_WPE`, so a default
build needs no WPE headers.

**This was predicted to be the main risk and was not.** Subclassing `WPEDisplay`,
`WPEView` and `WPEToplevel` from Rust is mechanical: `g_type_query` reports the
parent's instance and class sizes at runtime, `g_type_register_static_simple`
registers against those, and the class structs are public so installing a vfunc is a
field assignment. That is *more* robust than the C spike, which bakes the layout in at
compile time. Friction amounted to two things: `GClassInitFunc` is already an
`Option<fn>` and must not be wrapped again, and `gsize` is `u64`.

## Risks, as they actually landed

The ranking was wrong in an instructive way: the mechanical risks were cheap and the
undocumented-protocol ones were expensive.

1. ~~**FFI surface is hand-built.**~~ Retired. Mechanical, see above.
2. **ABI churn.** Unchanged and unavoidable. WebKit majors move, Arch is rolling;
   expect periodic build breaks against `wpe-webkit-2.0` / `wpe-platform-2.0`.
3. ~~**Multi-process and the sandbox.**~~ Retired. `WPENetworkProcess` and
   `WPEWebProcess` come up under `bwrap` with no special setup.
4. ~~**GLib main loop vs `calloop`.**~~ Done. `register_sources` registers the epoll fd
   carrying GLib's pollfd set, plus a timer from `poll_timeout`. Measured at **63
   wakeups per 8s against 495** for the fixed-interval version it replaced.
5. **Cloudflare remains unproven**, and is no longer on the critical path — see below.

**The real cost was none of these.** It was the object graph: that `WebKitWebView`
takes a `WPEDisplay` and makes its own view, that a `WPEToplevel` is required at all,
and that the buffer handshake has two halves. Every one of those fails *silently* —
no error, healthy web process, simply no frames. Better bindings would not have helped
with any of them.

## Real-world use — started 2026-08-28

The WPE build is installed and in daily use. What that has established, and what it
has cost, in the first hours:

**Cloudflare: signed in successfully**, dashboard and all. Servo could not get past the
interstitial at all — it re-ran the challenge until the machine died. This is not quite
proof that WebKit passes a *Managed Challenge* specifically (the session may never have
been served one), but it is a far higher bar than the marketing page that earlier test
used: a heavy JS application behind Cloudflare's own protection, reached through a
login. The engine-identity worry that motivated half this document has not materialised.

**Cookies persist.** The login survives in WebKit's own origin-keyed store under
`~/.local/state/cce/browser/profile/storage`. (`cookie_jar.json` beside it is Servo's
format, now dead weight.)

**Three bugs found by use, none by testing:**

| symptom | cause |
| --- | --- |
| pages rendered half size | `resize` took physical pixels and never told WPE the scale, so a 2x display laid out 2400x1600 *CSS* pixels |
| Ctrl+V did nothing in a page | `WPEDisplayClass.get_clipboard` left NULL — WebKit had no clipboard at all |
| …and still did nothing once added | the `WPEClipboard` subclass overrode `changed` without chaining up, so `set_content` stored nothing and WebKit never called `read` |

The scale bug is the instructive one: **every test up to that point ran at scale 1**,
where the physical/logical conversion is the identity, so nothing scale-dependent was
ever exercised. A whole class of bug was invisible to the entire test suite. The same
was true of input coordinates, which had the identical latent bug and were fixed in the
same pass before anyone hit them.

## Still unproven

- A live `"Just a moment..."` interstitial, specifically. Cheap to settle now that the
  WPE build is a real browser rather than a Python harness.
- Everything past the first hours of use.

## What remains

Roughly in order of what would decide whether WPE becomes the default:

1. **Soak it on real sites.** The gap named above. Everything else is speculation
   until someone browses on it.
2. **Settle Cloudflare**, next time a live challenge appears.
3. **Clean up the Servo-shaped seams in `main.rs`.** `focus()` is a no-op on the Servo
   side, and `set_force_dark` is only called under the feature. Both are honest
   scaffolding for running two backends at once, and both should go when one wins.
4. **Take the free WebKit features** — JS dialogs, HTTP auth, permissions,
   find-in-page, zoom. Each is a signal.
5. **Phase 2 dmabuf**, only once the rest is solid, and only after coordinating on
   `cce-ui`.

## Verifying it yourself

The examples are the test suite; all need `--features wpe`.

| example | what it demonstrates |
| --- | --- |
| `wpe_host` | boot, frames, page state, navigation, history |
| `wpe_input` | pointer / keyboard / wheel reaching the page, read back via `document.title` |
| `wpe_tabs` | several views on one display, and a **backgrounded** tab still updating |
| `wpe_loop` | blocking on GLib's fds vs polling, with the wakeup counts |
| `wpe_dark` | force-dark, asserted on rendered pixels rather than on the call |

`spike/wpe-spike.c` is the original C spike, kept because it is the shortest complete
statement of the embedding contract.
