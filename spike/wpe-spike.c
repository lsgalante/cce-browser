/*
 * WPE embedding spike — reference, NOT part of the build.
 *
 * Proves the whole embedding contract WPE-PORT.md plans against: own the
 * display, own the view, own the toplevel, and get rendered pixels out as a
 * CPU-mappable buffer. Verified 2026-08-27 against wpewebkit 2.52.6-1 —
 * renders example.com pixel-correct at 1200x800.
 *
 *   gcc -Wno-deprecated-declarations spike/wpe-spike.c -o /tmp/wpe-spike \
 *       $(pkg-config --cflags --libs wpe-webkit-2.0 wpe-platform-2.0)
 *   /tmp/wpe-spike https://example.com     # writes /tmp/wpe-spike.ppm
 *
 * It is C because the point was to settle the object graph fast, before
 * paying for bindgen and GObject subclassing from Rust. Keep it as the
 * thing to read while writing that; the two traps below cost hours and
 * neither produces an error message.
 *
 * TRAP 1 — the toplevel owns format negotiation. WebKit asks
 * WPEToplevelClass.get_preferred_buffer_formats, NOT the display's. Leave
 * WPEDisplayClass.create_toplevel NULL and render_buffer never fires, with
 * no warning and a perfectly healthy web process.
 *
 * TRAP 2 — the buffer handshake has two halves. wpe_view_buffer_rendered
 * means "displayed"; wpe_view_buffer_released means "the memory is yours
 * again". Call only the first and you get exactly one frame, then a
 * permanent stall. This is also the backpressure that makes the unbounded
 * upload queue of the Servo path impossible here.
 */
#include <wpe/webkit.h>
#include <wpe/wpe-platform.h>
#include <stdio.h>

static GMainLoop *loop;
static int frames = 0;

/* ---- our WPEView: receives rendered buffers ---- */
#define SPIKE_TYPE_VIEW (spike_view_get_type())
G_DECLARE_FINAL_TYPE(SpikeView, spike_view, SPIKE, VIEW, WPEView)
struct _SpikeView { WPEView parent; };
G_DEFINE_TYPE(SpikeView, spike_view, WPE_TYPE_VIEW)

static gboolean spike_view_render_buffer(WPEView *view, WPEBuffer *buffer,
                                         const WPERectangle *damage, guint n_damage,
                                         GError **error) {
    int w = wpe_buffer_get_width(buffer), h = wpe_buffer_get_height(buffer);
    const char *kind = WPE_IS_BUFFER_SHM(buffer) ? "SHM"
                     : (WPE_IS_BUFFER_DMA_BUF(buffer) ? "DMABuf" : "other");
    printf("render_buffer #%d: %dx%d  type=%s  damage_rects=%u\n",
           ++frames, w, h, kind, n_damage);

    if (WPE_IS_BUFFER_SHM(buffer)) {
        WPEBufferSHM *shm = WPE_BUFFER_SHM(buffer);
        GBytes *bytes = wpe_buffer_shm_get_data(shm);
        gsize len = 0; const guchar *px = g_bytes_get_data(bytes, &len);
        guint stride = wpe_buffer_shm_get_stride(shm);
        printf("  bytes=%zu stride=%u format=%d\n", len, stride,
               (int)wpe_buffer_shm_get_format(shm));
        if (frames == 2) {                 // let the page settle a little
            FILE *f = fopen("/tmp/wpe-spike.ppm", "wb");
            fprintf(f, "P6\n%d %d\n255\n", w, h);
            for (int y = 0; y < h; y++)
                for (int x = 0; x < w; x++) {
                    const guchar *p = px + y * stride + x * 4;   // BGRA
                    fputc(p[2], f); fputc(p[1], f); fputc(p[0], f);
                }
            fclose(f);
            printf("  WROTE /tmp/wpe-spike.ppm\n");
            g_main_loop_quit(loop);
        }
    }
    // Both halves of the handshake: displayed, then memory returned.
    wpe_view_buffer_rendered(view, buffer);
    wpe_view_buffer_released(view, buffer);
    return TRUE;
}
static void spike_view_init(SpikeView *v) {}
static void spike_view_class_init(SpikeViewClass *k) {
    WPE_VIEW_CLASS(k)->render_buffer = spike_view_render_buffer;
}

/* ---- our WPEToplevel: WebKit asks IT for buffer formats ---- */
#define SPIKE_TYPE_TOPLEVEL (spike_toplevel_get_type())
G_DECLARE_FINAL_TYPE(SpikeToplevel, spike_toplevel, SPIKE, TOPLEVEL, WPEToplevel)
struct _SpikeToplevel { WPEToplevel parent; };
G_DEFINE_TYPE(SpikeToplevel, spike_toplevel, WPE_TYPE_TOPLEVEL)

#define FOURCC(a,b,c,d) ((guint32)(a)|((guint32)(b)<<8)|((guint32)(c)<<16)|((guint32)(d)<<24))
static WPEBufferFormats *spike_toplevel_formats(WPEToplevel *t) {
    WPEBufferFormatsBuilder *b = wpe_buffer_formats_builder_new(NULL);
    wpe_buffer_formats_builder_append_group(b, NULL, WPE_BUFFER_FORMAT_USAGE_MAPPING);
    wpe_buffer_formats_builder_append_format(b, FOURCC('A','R','2','4'), 0);
    wpe_buffer_formats_builder_append_format(b, FOURCC('X','R','2','4'), 0);
    return wpe_buffer_formats_builder_end(b);
}
static gboolean spike_toplevel_resize(WPEToplevel *t, int w, int h) {
    printf("toplevel resize -> %dx%d\n", w, h);
    wpe_toplevel_resized(t, w, h);
    return TRUE;
}
static void spike_toplevel_init(SpikeToplevel *t) {}
static void spike_toplevel_class_init(SpikeToplevelClass *k) {
    WPEToplevelClass *tc = WPE_TOPLEVEL_CLASS(k);
    tc->get_preferred_buffer_formats = spike_toplevel_formats;
    tc->resize = spike_toplevel_resize;
}

/* ---- our WPEDisplay: vends the view above ---- */
#define SPIKE_TYPE_DISPLAY (spike_display_get_type())
G_DECLARE_FINAL_TYPE(SpikeDisplay, spike_display, SPIKE, DISPLAY, WPEDisplay)
struct _SpikeDisplay { WPEDisplay parent; };
G_DEFINE_TYPE(SpikeDisplay, spike_display, WPE_TYPE_DISPLAY)

static gboolean spike_display_connect(WPEDisplay *d, GError **e) { return TRUE; }

#define FOURCC(a,b,c,d) ((guint32)(a)|((guint32)(b)<<8)|((guint32)(c)<<16)|((guint32)(d)<<24))
/* No EGL display and no DRM device -> WebKit should fall back to mappable SHM.
   Advertise ARGB/XRGB with LINEAR so it has something to pick. */
static WPEBufferFormats *spike_display_formats(WPEDisplay *d) {
    WPEBufferFormatsBuilder *b = wpe_buffer_formats_builder_new(NULL);
    wpe_buffer_formats_builder_append_group(b, NULL, WPE_BUFFER_FORMAT_USAGE_RENDERING);
    wpe_buffer_formats_builder_append_format(b, FOURCC('A','B','2','4'), 0 /*LINEAR*/);
    wpe_buffer_formats_builder_append_format(b, FOURCC('X','B','2','4'), 0);
    return wpe_buffer_formats_builder_end(b);
}
static WPEView *spike_display_create_view(WPEDisplay *d) {
    return g_object_new(SPIKE_TYPE_VIEW, "display", d, NULL);
}
static WPEToplevel *spike_display_create_toplevel(WPEDisplay *d, guint max_views) {
    printf("create_toplevel(max_views=%u)\n", max_views);
    return g_object_new(SPIKE_TYPE_TOPLEVEL, "display", d, "max-views", max_views, NULL);
}
static void spike_display_init(SpikeDisplay *d) {}
static void spike_display_class_init(SpikeDisplayClass *k) {
    WPEDisplayClass *dc = WPE_DISPLAY_CLASS(k);
    dc->connect = spike_display_connect;
    dc->create_view = spike_display_create_view;
    dc->get_preferred_buffer_formats = spike_display_formats;
    dc->create_toplevel = spike_display_create_toplevel;
}

int main(int argc, char **argv) {
    const char *url = argc > 1 ? argv[1] : "https://example.com";
    WPEDisplay *display = g_object_new(SPIKE_TYPE_DISPLAY, NULL);
    GError *err = NULL;
    if (!wpe_display_connect(display, &err)) {
        printf("connect failed: %s\n", err ? err->message : "?"); return 1;
    }
    WebKitWebView *wv = g_object_new(WEBKIT_TYPE_WEB_VIEW, "display", display, NULL);
    WPEView *v = webkit_web_view_get_wpe_view(wv);
    printf("web view=%p  its wpe_view=%p\n", (void*)wv, (void*)v);
    WPEToplevel *top = wpe_display_create_toplevel(display, 1);
    printf("toplevel=%p\n", (void*)top);
    if (top) { wpe_toplevel_resized(top, 1200, 800); wpe_view_set_toplevel(v, top); }
    wpe_view_resized(v, 1200, 800);
    wpe_view_set_visible(v, TRUE);
    wpe_view_map(v);
    printf("view now %dx%d visible=%d mapped=%d\n", wpe_view_get_width(v),
           wpe_view_get_height(v), wpe_view_get_visible(v), wpe_view_get_mapped(v));
    webkit_web_view_load_uri(wv, url);
    loop = g_main_loop_new(NULL, FALSE);
    g_timeout_add_seconds(25, (GSourceFunc)g_main_loop_quit, loop);
    g_main_loop_run(loop);
    printf("done, %d frames\n", frames);
    return frames ? 0 : 2;
}
