/* GPU-backed Wayland workload. Mesa owns the bounded EGL swapchain and releases. */
#define _POSIX_C_SOURCE 200809L
#include <EGL/egl.h>
#include <GLES2/gl2.h>
#include <wayland-client.h>
#include <wayland-egl.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include "xdg-shell-client-protocol.h"
#include "xdg-decoration-client-protocol.h"

struct window {
    struct wl_surface *surface;
    struct xdg_surface *xdg;
    struct xdg_toplevel *top;
    struct wl_egl_window *native;
    EGLSurface egl;
    int configured, width, height;
};
static struct wl_compositor *compositor;
static struct xdg_wm_base *shell;
static struct zxdg_decoration_manager_v1 *decorations;
static struct wl_pointer *pointer;
static struct wl_keyboard *keyboard;
static int stopped;
static unsigned long inputs;
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec * 1e-9; }
static void configure(void *data, struct xdg_surface *surface, uint32_t serial) {
    struct window *w = data;
    xdg_surface_ack_configure(surface, serial);
    w->configured = 1;
    if (w->native) wl_egl_window_resize(w->native, w->width, w->height, 0, 0);
}
static const struct xdg_surface_listener surface_listener = { .configure = configure };
static void top_configure(void *data, struct xdg_toplevel *top, int32_t width, int32_t height, struct wl_array *states) {
    (void)top; (void)states;
    struct window *w = data;
    if (width > 0) w->width = width;
    if (height > 0) w->height = height;
}
static void close_window(void *data, struct xdg_toplevel *top) { (void)data; (void)top; stopped = 1; }
static const struct xdg_toplevel_listener top_listener = { .configure = top_configure, .close = close_window };
static void decoration_configure(void *data, struct zxdg_toplevel_decoration_v1 *decoration, uint32_t mode) { (void)data; (void)decoration; (void)mode; }
static const struct zxdg_toplevel_decoration_v1_listener decoration_listener = { .configure=decoration_configure };
static void ping(void *data, struct xdg_wm_base *wm, uint32_t serial) { (void)data; xdg_wm_base_pong(wm, serial); }
static const struct xdg_wm_base_listener wm_listener = { .ping = ping };
static void enter(void *d, struct wl_pointer *p, uint32_t s, struct wl_surface *w, wl_fixed_t x, wl_fixed_t y) { (void)d;(void)p;(void)s;(void)w;(void)x;(void)y; }
static void leave(void *d, struct wl_pointer *p, uint32_t s, struct wl_surface *w) { (void)d;(void)p;(void)s;(void)w; }
static void motion(void *d, struct wl_pointer *p, uint32_t t, wl_fixed_t x, wl_fixed_t y) { (void)d;(void)p;(void)t;(void)x;(void)y; inputs++; }
static void button(void *d, struct wl_pointer *p, uint32_t s, uint32_t t, uint32_t b, uint32_t state) { (void)d;(void)p;(void)s;(void)t;(void)b;(void)state; inputs++; }
static void axis(void *d, struct wl_pointer *p, uint32_t t, uint32_t a, wl_fixed_t v) { (void)d;(void)p;(void)t;(void)a;(void)v; inputs++; }
static const struct wl_pointer_listener pointer_listener = { .enter=enter, .leave=leave, .motion=motion, .button=button, .axis=axis };
static void keymap(void *d, struct wl_keyboard *k, uint32_t f, int32_t fd, uint32_t size) { (void)d;(void)k;(void)f;(void)size; close(fd); }
static void key_enter(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *w, struct wl_array *keys) { (void)d;(void)k;(void)s;(void)w;(void)keys; }
static void key_leave(void *d, struct wl_keyboard *k, uint32_t s, struct wl_surface *w) { (void)d;(void)k;(void)s;(void)w; }
static void key(void *d, struct wl_keyboard *k, uint32_t s, uint32_t t, uint32_t code, uint32_t state) { (void)d;(void)k;(void)s;(void)t;(void)code;(void)state; inputs++; }
static void mods(void *d, struct wl_keyboard *k, uint32_t s, uint32_t a, uint32_t b, uint32_t c, uint32_t group) { (void)d;(void)k;(void)s;(void)a;(void)b;(void)c;(void)group; }
static const struct wl_keyboard_listener keyboard_listener = { .keymap=keymap,.enter=key_enter,.leave=key_leave,.key=key,.modifiers=mods };
static void caps(void *d, struct wl_seat *seat, uint32_t caps) {
    (void)d;
    if ((caps & WL_SEAT_CAPABILITY_POINTER) && !pointer) { pointer = wl_seat_get_pointer(seat); wl_pointer_add_listener(pointer, &pointer_listener, NULL); }
    if ((caps & WL_SEAT_CAPABILITY_KEYBOARD) && !keyboard) { keyboard = wl_seat_get_keyboard(seat); wl_keyboard_add_listener(keyboard, &keyboard_listener, NULL); }
}
static const struct wl_seat_listener seat_listener = { .capabilities=caps };
static void global(void *data, struct wl_registry *registry, uint32_t name, const char *interface, uint32_t version) {
    (void)data; (void)version;
    if (!strcmp(interface, "wl_compositor")) compositor = wl_registry_bind(registry, name, &wl_compositor_interface, 4);
    else if (!strcmp(interface, "xdg_wm_base")) { shell = wl_registry_bind(registry, name, &xdg_wm_base_interface, 1); xdg_wm_base_add_listener(shell, &wm_listener, NULL); }
    else if (!strcmp(interface, "zxdg_decoration_manager_v1")) decorations = wl_registry_bind(registry, name, &zxdg_decoration_manager_v1_interface, 1);
    else if (!strcmp(interface, "wl_seat")) { struct wl_seat *seat = wl_registry_bind(registry, name, &wl_seat_interface, 1); wl_seat_add_listener(seat, &seat_listener, NULL); }
}
static void removed(void *d, struct wl_registry *r, uint32_t n) { (void)d;(void)r;(void)n; }
static const struct wl_registry_listener registry_listener = { .global=global,.global_remove=removed };

int main(int argc, char **argv) {
    if (argc != 5) return 2;
    int server_decoration = !strcmp(argv[4], "server");
    int hz = atoi(argv[1]), count = atoi(argv[2]), seconds = atoi(argv[3]);
    if (hz < 1 || hz > 1000 || count < 1 || count > 8 || seconds < 1 || seconds > 180) return 2;
    struct wl_display *display = wl_display_connect(NULL);
    if (!display) return 3;
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    if (wl_display_roundtrip(display) < 0 || !compositor || !shell) return 3;
    EGLDisplay egl = eglGetDisplay((EGLNativeDisplayType)display);
    if (!eglInitialize(egl, NULL, NULL) || !eglBindAPI(EGL_OPENGL_ES_API)) return 4;
    EGLint attributes[] = { EGL_SURFACE_TYPE,EGL_WINDOW_BIT,EGL_RENDERABLE_TYPE,EGL_OPENGL_ES2_BIT,EGL_RED_SIZE,8,EGL_GREEN_SIZE,8,EGL_BLUE_SIZE,8,EGL_ALPHA_SIZE,0,EGL_NONE };
    EGLConfig config;
    EGLint configs;
    if (!eglChooseConfig(egl, attributes, &config, 1, &configs) || configs != 1) return 4;
    EGLint context_attributes[] = { EGL_CONTEXT_CLIENT_VERSION,2,EGL_NONE };
    EGLContext context = eglCreateContext(egl, config, EGL_NO_CONTEXT, context_attributes);
    if (context == EGL_NO_CONTEXT) return 4;
    struct window windows[8] = {0};
    for (int i=0; i<count; i++) {
        struct window *w = &windows[i];
        w->width = 640; w->height = 480;
        w->surface = wl_compositor_create_surface(compositor);
        w->xdg = xdg_wm_base_get_xdg_surface(shell, w->surface);
        xdg_surface_add_listener(w->xdg, &surface_listener, w);
        w->top = xdg_surface_get_toplevel(w->xdg);
        xdg_toplevel_add_listener(w->top, &top_listener, w);
        xdg_toplevel_set_title(w->top, "Weld GPU benchmark");
        xdg_toplevel_set_app_id(w->top, "weld-benchmark");
        if (server_decoration) {
            if (!decorations) return 3;
            struct zxdg_toplevel_decoration_v1 *decoration = zxdg_decoration_manager_v1_get_toplevel_decoration(decorations, w->top);
            zxdg_toplevel_decoration_v1_add_listener(decoration, &decoration_listener, NULL);
            zxdg_toplevel_decoration_v1_set_mode(decoration, ZXDG_TOPLEVEL_DECORATION_V1_MODE_SERVER_SIDE);
        }
        wl_surface_commit(w->surface);
        while (!w->configured) if (wl_display_dispatch(display) < 0) return 3;
        w->native = wl_egl_window_create(w->surface, w->width, w->height);
        w->egl = eglCreateWindowSurface(egl, config, (EGLNativeWindowType)w->native, NULL);
        if (!w->native || w->egl == EGL_NO_SURFACE || !eglMakeCurrent(egl, w->egl, w->egl, context)) return 4;
        if (!eglSwapInterval(egl, 0)) return 4;
    }
    printf("PRODUCER ready windows=%d requested_hz=%d renderer=%s\n", count, hz, glGetString(GL_RENDERER)); fflush(stdout);
    double start = now(), deadline = start, report_at = start + 1.0;
    unsigned long frames = 0, last_frames = 0;
    while (!stopped && now() - start < seconds) {
        while (wl_display_prepare_read(display) != 0) if (wl_display_dispatch_pending(display) < 0) return 3;
        wl_display_flush(display);
        struct pollfd fd = { .fd=wl_display_get_fd(display), .events=POLLIN };
        int wait_ms = (int)((deadline - now()) * 1000.0);
        if (wait_ms < 0) wait_ms = 0;
        if (poll(&fd, 1, wait_ms) > 0 && (fd.revents & POLLIN)) {
            if (wl_display_read_events(display) < 0) break;
            if (wl_display_dispatch_pending(display) < 0) break;
        } else wl_display_cancel_read(display);
        if (fd.revents & (POLLERR|POLLHUP|POLLNVAL)) break;
        if (now() < deadline) {
            struct timespec remaining = { .tv_sec=0, .tv_nsec=(long)((deadline-now())*1e9) };
            if (remaining.tv_nsec > 0) nanosleep(&remaining, NULL);
        }
        for (int i=0; i<count; i++) {
            struct window *w=&windows[i];
            if (!eglMakeCurrent(egl, w->egl, w->egl, context)) return 4;
            glViewport(0,0,w->width,w->height);
            glDisable(GL_SCISSOR_TEST);
            glClearColor(0.04f,0.08f+0.03f*i,0.13f,1.0f); glClear(GL_COLOR_BUFFER_BIT);
            glEnable(GL_SCISSOR_TEST);
            glScissor((int)(frames*7 % (unsigned)w->width),0,40,w->height);
            glClearColor(0.1f,0.8f,0.5f,1.0f); glClear(GL_COLOR_BUFFER_BIT);
            if (!eglSwapBuffers(egl,w->egl)) { stopped=1; break; }
        }
        frames++;
        double current=now();
        if (current >= report_at) { printf("PRODUCER elapsed=%.3f frames=%lu recent=%lu inputs=%lu\n",current-start,frames,frames-last_frames,inputs); fflush(stdout); last_frames=frames; report_at=current+1.0; }
        deadline += 1.0/hz;
        if (deadline < current) deadline=current+1.0/hz;
    }
    printf("PRODUCER end frames=%lu inputs=%lu\n",frames,inputs);
    eglMakeCurrent(egl,EGL_NO_SURFACE,EGL_NO_SURFACE,EGL_NO_CONTEXT);
    for (int i=0;i<count;i++) { eglDestroySurface(egl,windows[i].egl); wl_egl_window_destroy(windows[i].native); }
    eglDestroyContext(egl,context); eglTerminate(egl); wl_display_disconnect(display);
    return 0;
}
