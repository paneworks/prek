/* Keyboard smoke test client. Connect only to an explicitly supplied owned
 * Cage socket; no evdev, uinput or ambient WAYLAND_DISPLAY connection. */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include <wayland-client.h>
#include <xkbcommon/xkbcommon.h>
#include "virtual-keyboard-client-protocol.h"

struct client {
    struct wl_seat *seat;
    struct zwp_virtual_keyboard_manager_v1 *manager;
};
static void global(void *data, struct wl_registry *registry, uint32_t name,
                   const char *interface, uint32_t version) {
    (void)version;
    struct client *client = data;
    if (!strcmp(interface, "wl_seat"))
        client->seat = wl_registry_bind(registry, name, &wl_seat_interface, 1);
    else if (!strcmp(interface, "zwp_virtual_keyboard_manager_v1"))
        client->manager = wl_registry_bind(registry, name,
            &zwp_virtual_keyboard_manager_v1_interface, 1);
}
static void removed(void *data, struct wl_registry *registry, uint32_t name) {
    (void)data; (void)registry; (void)name;
}
static const struct wl_registry_listener registry_listener = {
    .global = global, .global_remove = removed,
};
int main(int argc, char **argv) {
    if ((argc != 2 && argc != 3 && argc != 4) || argv[1][0] != '/' ||
        (argc == 4 && strcmp(argv[3], "--persistent"))) {
        fprintf(stderr, "supply the absolute owned Cage socket path\n");
        return 2;
    }
    struct wl_display *display = wl_display_connect(argv[1]);
    if (!display) { perror("Cage socket"); return 1; }
    struct client client = {0};
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, &client);
    if (wl_display_roundtrip(display) < 0 || !client.seat || !client.manager) {
        fprintf(stderr, "Cage seat/virtual keyboard unavailable\n"); return 1;
    }
    struct xkb_context *context = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    struct xkb_rule_names names = {.layout = "us"};
    struct xkb_keymap *keymap = context ? xkb_keymap_new_from_names(context,
        &names, XKB_KEYMAP_COMPILE_NO_FLAGS) : NULL;
    char *text = keymap ? xkb_keymap_get_as_string(keymap,
        XKB_KEYMAP_FORMAT_TEXT_V1) : NULL;
    if (!text) { fprintf(stderr, "keymap failed\n"); return 1; }
    size_t length = strlen(text) + 1;
    int fd = memfd_create("prek-cage-keymap", MFD_CLOEXEC);
    if (fd < 0 || ftruncate(fd, (off_t)length) < 0) {
        perror("keymap fd"); return 1;
    }
    void *memory = mmap(NULL, length, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (memory == MAP_FAILED) { perror("keymap mmap"); return 1; }
    memcpy(memory, text, length);
    munmap(memory, length);
    struct zwp_virtual_keyboard_v1 *keyboard =
        zwp_virtual_keyboard_manager_v1_create_virtual_keyboard(client.manager,
            client.seat);
    zwp_virtual_keyboard_v1_keymap(keyboard, WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1,
        fd, (uint32_t)length);
    close(fd);
    if (wl_display_roundtrip(display) < 0) return 1;
    unsigned timestamp = 0;
    struct xkb_state *state = xkb_state_new(keymap);
    bool pressed[768] = {false};
    if (!state) return 1;
    puts("ready"); fflush(stdout);
    for (;;) {
    if (getchar() != '\n') {
        if (argc == 4 && feof(stdin)) break;
        fprintf(stderr, "keyboard test requires a start acknowledgement\n"); return 1;
    }
    if (argc >= 3) {
        FILE *input = fopen(argv[2], "r");
        if (!input) { perror("routed keys"); return 1; }
        unsigned code, value, count = 0;
        int fields;
        while ((fields = fscanf(input, "%u %u", &code, &value)) == 2) {
            if (code >= 768 || value > 1 || ++count > 64) {
                fprintf(stderr, "invalid routed key\n"); return 1;
            }
            zwp_virtual_keyboard_v1_key(keyboard, ++timestamp, code, value);
            if (pressed[code] != (value != 0)) {
                xkb_state_update_key(state, code + 8,
                    value ? XKB_KEY_DOWN : XKB_KEY_UP);
                pressed[code] = value != 0;
            }
            zwp_virtual_keyboard_v1_modifiers(keyboard,
                xkb_state_serialize_mods(state, XKB_STATE_MODS_DEPRESSED),
                xkb_state_serialize_mods(state, XKB_STATE_MODS_LATCHED),
                xkb_state_serialize_mods(state, XKB_STATE_MODS_LOCKED),
                xkb_state_serialize_layout(state, XKB_STATE_LAYOUT_EFFECTIVE));
        }
        if (fields != EOF || ferror(input)) return 1;
        fclose(input);
    } else {
    zwp_virtual_keyboard_v1_key(keyboard, 1, 48, WL_KEYBOARD_KEY_STATE_PRESSED);
    zwp_virtual_keyboard_v1_key(keyboard, 2, 48, WL_KEYBOARD_KEY_STATE_RELEASED);
    zwp_virtual_keyboard_v1_key(keyboard, 3, 42, WL_KEYBOARD_KEY_STATE_PRESSED);
    xkb_mod_index_t shift = xkb_keymap_mod_get_index(keymap, XKB_MOD_NAME_SHIFT);
    if (shift == XKB_MOD_INVALID || shift >= 32) return 1;
    zwp_virtual_keyboard_v1_modifiers(keyboard, 1u << shift, 0, 0, 0);
    zwp_virtual_keyboard_v1_key(keyboard, 4, 30, WL_KEYBOARD_KEY_STATE_PRESSED);
    zwp_virtual_keyboard_v1_key(keyboard, 5, 30, WL_KEYBOARD_KEY_STATE_RELEASED);
    zwp_virtual_keyboard_v1_key(keyboard, 6, 42, WL_KEYBOARD_KEY_STATE_RELEASED);
    zwp_virtual_keyboard_v1_modifiers(keyboard, 0, 0, 0, 0);
    }
    if (wl_display_roundtrip(display) < 0) return 1;
    puts("done"); fflush(stdout);
    if (argc != 4) break;
    }
    zwp_virtual_keyboard_v1_destroy(keyboard);
    int result = wl_display_roundtrip(display);
    wl_display_disconnect(display);
    xkb_state_unref(state);
    free(text); xkb_keymap_unref(keymap); xkb_context_unref(context);
    return result < 0;
}
