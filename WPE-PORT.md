# Scoping: porting cce-browser from Servo to WPE WebKit

Status: **scoping only.** Nothing here is implemented. Written 2026-08-27.

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

**The fiddly bit, and the real FFI risk:** `render_buffer` is a GObject vfunc, so the
embedder has to **subclass `WPEView` from Rust** — register a GType, set the vfunc
pointer in `class_init`. Tractable over bindgen, and `cogcore-sys` is worth reading for
exactly this, but it is the one piece that is genuinely awkward rather than mechanical.
Prove it in the spike before anything else.

## Impact map

| file | fate |
| --- | --- |
| `src/webview.rs` (708 lines) | **full rewrite.** It *is* the engine boundary. |
| `src/main.rs` (1119) | **mostly survives.** Chrome, hit-testing, URL editor, key routing are engine-agnostic. The input translation (`dom_key`, `dom_button`, wheel) is retargeted; `display_list` keeps drawing one quad. |
| `src/pages.rs` (342) | **survives**, minus the protocol plumbing — WebKit has a URI-scheme registration API (`webkit_web_context_register_uri_scheme`) that maps cleanly onto `CceProtocol`. |
| `src/downloads.rs` (266) | **shrinks a lot.** WebKit has a real download API, so the extension sniff, `is_download_url`, the `request_navigation` divert, *and* the argv blind spot (`5a85c97`) all disappear as a category. Progress arrives as signals, killing the `<meta refresh>` leak too. |
| `src/settings.rs` (147) | **survives** unchanged; keys are ours. |

Things currently listed under "Not implemented yet" that WebKit simply provides:
JS dialogs (`script-dialog`), HTTP auth (`authenticate`), permission requests
(`permission-request`), find-in-page (`WebKitFindController`), and zoom
(`webkit_web_view_set_zoom_level`). Force-dark stops needing an inverting user
stylesheet and its two-reload settling dance.

## The frame path — where the real design work is

Servo today: CPU render → `read_to_image` → `cce_ui::vk::upload_rgba` (a full-window
RGBA `Vec<u8>` per frame, through a global queue). WPE can do better, but staging
matters:

- **Phase 1 — SHM/CPU buffers.** Match the existing pipeline exactly: take WPE's
  buffer, hand the bytes to `upload_rgba`. **No `cce-ui` change at all.** Lowest risk,
  proves the port end to end.
- **Phase 2 — dmabuf, zero copy.** WPE exports dmabuf; import it as a Vulkan image via
  `VK_EXT_external_memory_dma_buf` and skip the CPU roundtrip entirely. This is
  strictly better than anything the Servo path could do — but it needs a **new
  `cce-ui` API** (the registry only accepts `Vec<u8>` today), and `cce-ui` is a
  **shared crate**: check `git status` there and coordinate before touching it.

Do not attempt Phase 2 first. Phase 1 is the thing that tells us the port works.

## Bindings: hand-rolled, like wlroots

There are no usable Rust bindings.

- `wpe` — 0.0.19, last published **2023-03**. Abandoned.
- `cogcore-sys` / `cogcore` — FFI to Igalia's Cog launcher, recent (2026-08) but **92
  downloads**. Not a dependency; worth *reading* as prior art for the FFI shape.
- The `webkit` crate is macOS `WKWebView`. Irrelevant.

So: bindgen over the C headers, exactly the idiom `cce-compositor/build.rs` already
uses against wlroots. This is the single largest chunk of work and the main risk.

## Risks, ranked

1. **FFI surface is hand-built.** Biggest cost. Mitigate by binding only what
   `ServoHost`'s public surface needs — look at its 30 methods, not at all of WebKit.
2. **ABI churn.** WebKit majors move and Arch is rolling; expect periodic build
   breaks. Pin the `pkg-config` name, accept the maintenance.
3. **Multi-process and the sandbox.** WebKit spawns its own helper binaries, shipped
   by the package, so `main()` is untouched — unlike CEF. But `wpewebkit` depends on
   **`bubblewrap`**: the sandbox wants user namespaces, which is worth verifying early
   inside the cce session rather than discovering late.
4. **GLib main loop vs `calloop`.** WebKit needs a `GMainContext` turning. Either
   integrate its fd into `calloop` or run it stepped from `pump`. Solvable, needs
   design.
5. **Cloudflare is likely-but-unproven.** WebKit *should* pass; not yet demonstrated
   against a live challenge (see below).

## Still unproven

WebKit rendered `cloudflare.com` correctly and instantly in a shadow session — but
Servo did too, in the same session minutes later. **Neither was served a challenge**,
so that comparison establishes nothing about surviving one. The decisive test is to
catch a live `"Just a moment..."` and run both against that exact URL at that moment.
The harness is ready: `scratchpad/wktest.py` drives WebKitGTK via PyGObject (no
install needed — `WebKit-6.0.typelib` is already present).

The port's case does **not** rest on this. Coverage alone justifies it.

## Suggested order

1. ~~Confirm the WPE API generation~~ — done, WPEPlatform (above). Install
   `wpewebkit`; get `build.rs` + bindgen over `wpe-webkit-2.0` and `wpe-platform-2.0`
   producing symbols.
2. A throwaway binary: `WPEDisplayHeadless` + one `WPEView`, load a URL, pull one
   `WPEBufferSHM` out and write it to a PNG. No cce-ui, no Wayland, no chrome.
3. `ServoHost` → `WebKitHost` behind the same method surface, Phase-1 SHM buffers,
   single tab, no chrome changes.
4. Tabs, then `cce:` schemes, then downloads-via-real-API.
5. Phase 2 dmabuf, only once the rest is solid, and only after coordinating on
   `cce-ui`.
