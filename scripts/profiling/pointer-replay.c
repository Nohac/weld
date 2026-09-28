/* Persistent, unprivileged virtual pointer for the orbit diagnostic.
 * Build with wayland-scanner output from wlr-virtual-pointer-unstable-v1.xml.
 * Stdin: m x y width height (absolute motion), r dx dy, b 0|1 (middle button).
 * EOF releases the held button before destroying the device. */
#include <stdint.h>
#include <math.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <wayland-client.h>
#include "virtual-pointer.h"

static struct zwlr_virtual_pointer_manager_v1 *manager;

static void global(void *data, struct wl_registry *registry, uint32_t id,
                   const char *interface, uint32_t version) {
    (void)data;
    (void)version;
    if (strcmp(interface, "zwlr_virtual_pointer_manager_v1") == 0)
        manager = wl_registry_bind(registry, id, &zwlr_virtual_pointer_manager_v1_interface, 1);
}

static void removed(void *data, struct wl_registry *registry, uint32_t id) {
    (void)data;
    (void)registry;
    (void)id;
}

static uint32_t milliseconds(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return 0;
    return (uint32_t)((uint64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000);
}

int main(void) {
    struct wl_display *display = wl_display_connect(NULL);
    if (!display) return 1;
    struct wl_registry *registry = wl_display_get_registry(display);
    const struct wl_registry_listener listener = {global, removed};
    wl_registry_add_listener(registry, &listener, NULL);
    if (wl_display_roundtrip(display) < 0 || !manager) return 2;
    struct zwlr_virtual_pointer_v1 *pointer = zwlr_virtual_pointer_manager_v1_create_virtual_pointer(manager, NULL);
    if (!pointer || wl_display_roundtrip(display) < 0) return 3;
    char line[160];
    unsigned held = 0;
    int status = 0;
    while (fgets(line, sizeof line, stdin)) {
        unsigned x, y, width, height, button;
        double dx, dy;
        if (sscanf(line, "m %u %u %u %u", &x, &y, &width, &height) == 4 && width && height && x <= width && y <= height) {
            zwlr_virtual_pointer_v1_motion_absolute(pointer, milliseconds(), x, y, width, height);
        } else if (sscanf(line, "r %lf %lf", &dx, &dy) == 2 && isfinite(dx) && isfinite(dy) && fabs(dx) <= 10000 && fabs(dy) <= 10000) {
            zwlr_virtual_pointer_v1_motion(pointer, milliseconds(), wl_fixed_from_double(dx), wl_fixed_from_double(dy));
        } else if (sscanf(line, "b %u", &button) == 1 && button <= 1) {
            zwlr_virtual_pointer_v1_button(pointer, milliseconds(), 274, button);
            held = button;
        } else { status = 4; break; }
        zwlr_virtual_pointer_v1_frame(pointer);
        if (wl_display_flush(display) < 0) { status = 5; break; }
    }
    if (held) {
        zwlr_virtual_pointer_v1_button(pointer, milliseconds(), 274, WL_POINTER_BUTTON_STATE_RELEASED);
        zwlr_virtual_pointer_v1_frame(pointer);
    }
    wl_display_roundtrip(display);
    zwlr_virtual_pointer_v1_destroy(pointer);
    zwlr_virtual_pointer_manager_v1_destroy(manager);
    wl_registry_destroy(registry);
    wl_display_flush(display);
    wl_display_disconnect(display);
    return status;
}
