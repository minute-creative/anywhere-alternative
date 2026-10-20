// Anywhere Alternative on a phone: the C interface Swift calls.
//
// One connection at a time. Strings returned as `char*` are UTF-8 JSON
// owned by Rust: free them with `aa_mobile_free`. Every function may be
// called from any thread.
//
// Kept by hand in step with crates/aa-mobile/src/lib.rs (same names, same
// order), so the Rust file is the reference.

#ifndef AA_MOBILE_H
#define AA_MOBILE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/// One decoded picture. `data` points at Rust memory that stays valid until
/// the next `aa_mobile_frame` call. `format`: 0 = BGRA, 1 = RGBA.
typedef struct AAFrame {
    const uint8_t *data;
    size_t len;
    uint32_t width;
    uint32_t height;
    uint32_t format;
    uint32_t frame_id;
} AAFrame;

/// Call once. `data_dir`: a writable folder for keys, pairings and logs.
void aa_mobile_init(const char *data_dir);

/// Everything logged so far that the app has not read yet, newest last
/// (one line per entry). Also written to `viewer.log` in `data_dir`.
char *aa_mobile_take_log(void);

/// Computers sharing right now: on this Wi-Fi, and every remembered
/// address of paired ones (so a paired Mac on Tailscale is found from
/// anywhere). JSON array of {name, addr, key, paired, version}.
/// Blocks for about `timeout_ms` (1000 is plenty).
char *aa_mobile_discover(uint32_t timeout_ms);

/// Remembered computers: JSON array of {name, key, addrs, last_used}.
char *aa_mobile_paired(void);

/// Forget a paired computer (its key, hex).
void aa_mobile_forget(const char *key_hex);

/// Pair with the computer at `addr` ("ip:port" or "ip") using the six-digit
/// code shown on its Share page. Returns 0 on success; otherwise an error
/// message in `*err` (free it).
int32_t aa_mobile_pair(const char *addr, const char *code, char **err);

/// Connect. `host`: "ip:port", "ip", a computer name, "key:<hex>" (find a
/// paired computer wherever it is now) or "" (whatever is found first).
/// `resolution`: "1080" or "720" ("full" allowed). `fps`: 60 or 75 (0 =
/// automatic). `screen_hz`: the phone's refresh rate.
/// Returns 0 when the session thread has started; the outcome comes
/// through `aa_mobile_status`.
int32_t aa_mobile_connect(const char *host, const char *resolution, uint16_t fps, uint16_t screen_hz,
                          float max_mbps, bool mute_host, bool mock);

/// Phone mode for the next connect: ask the Mac for a screen of this shape
/// (logical points, width x height). 0, 0 = off (show the normal screen).
void aa_mobile_set_phone_display(uint16_t width, uint16_t height);

/// End the session (sends Bye) and free everything.
void aa_mobile_disconnect(void);

/// JSON: {state: "idle"|"connecting"|"connected"|"reconnecting"|"ended",
/// reason, paused, fps, mbps, rtt_ms, loss_pct, codec, target_fps,
/// target_kbps, sharing, notes, warning, host_mac, apps_version,
/// full_access, apps_pending}.
char *aa_mobile_status(void);

/// Newest decoded frame, if there is one since the last call. Returns 1
/// and fills `out`, or 0.
int32_t aa_mobile_frame(AAFrame *out);

/// Sound: fill `out` with up to `samples` 16-bit samples (stereo
/// interleaved, 48 kHz); the rest is silence. Returns how many were real.
size_t aa_mobile_audio_pull(int16_t *out, size_t samples);

// Input to the other computer.
void aa_mobile_mouse_abs(uint16_t x, uint16_t y);      // 0..65535 across the picture
void aa_mobile_mouse_rel(int16_t dx, int16_t dy);      // game mouse
void aa_mobile_mouse_button(uint8_t button, bool pressed); // 0 left, 1 right, 2 middle, 3 back, 4 forward
void aa_mobile_mouse_scroll(int16_t dx, int16_t dy);   // 1/120ths of a notch
void aa_mobile_key(uint16_t hid_usage, bool pressed);  // USB HID usage, page 7
void aa_mobile_gamepad_attach(uint8_t slot, bool playstation);
void aa_mobile_gamepad_detach(uint8_t slot);
void aa_mobile_gamepad(uint8_t slot, uint32_t buttons, int16_t lx, int16_t ly, int16_t rx, int16_t ry,
                       uint8_t lt, uint8_t rt);

// Settings while connected.
void aa_mobile_set_max_mbps(float mbps);
void aa_mobile_set_mute_host(bool muted);

// The launcher.
void aa_mobile_request_apps(void);
/// JSON array of {id, name, kind, running, last_used}; empty until the
/// host has answered. `apps_version` in the status changes when it does.
char *aa_mobile_apps(void);
/// The PNG icon of one app: copies up to `cap` bytes into `buf` and returns
/// the icon's full size (0 = none yet).
size_t aa_mobile_icon(const char *id, uint8_t *buf, size_t cap);
/// `action`: "open", "quit" or "hide". `show`: after opening, the host
/// shares only that app (one app at a time; needs full access there).
void aa_mobile_app_action(const char *id, const char *action, bool show);

/// App mode: show only the app with this id (and name, for the Mac's
/// status line), or the whole screen when `id` is "". Needs Full access.
void aa_mobile_share(const char *id, const char *name);

// Clipboard.
/// Text copied on the phone, for pasting on the other computer.
void aa_mobile_copy_text(const char *text);
/// Text the other computer copied since the last call ("" if none).
char *aa_mobile_take_clip_text(void);

/// Back to the whole screen after an app was shown alone.
void aa_mobile_show_desktop(void);

/// The version of the Rust side ("0.6.0").
const char *aa_mobile_version(void);

void aa_mobile_free(char *s);

#ifdef __cplusplus
}
#endif

#endif
