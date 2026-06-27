// C ABI of the Rust player FFI (exported by app-ios/src/lib.rs, compiled into
// the static lib bundled in RustPlayerFFI.xcframework). Imported as the Clang
// module `RustPlayerFFI`; the Swift `RustPlayer` wrapper calls these.
//
// Keep in lock-step with app-ios/src/lib.rs.

#ifndef RUSTPLAYER_FFI_H
#define RUSTPLAYER_FFI_H

#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

// Host callbacks (fired from Rust/Tokio worker threads).
//   intercept_cb   — rewrite/auth a request; complete via bz_intercept_complete/fail
//   resolve_key_cb — CENC KID (16 bytes) → ClearKey; complete via bz_resolve_key_complete/fail
//   event_cb       — one player event as unified JSON
typedef void (*bz_intercept_cb)(void *user, const char *url, int kind, uint64_t token);
typedef void (*bz_resolve_key_cb)(void *user, const uint8_t *kid, uint64_t token);
typedef void (*bz_event_cb)(void *user, const char *json);

// RequestKind values passed to bz_intercept_cb.
typedef enum {
    BZ_REQUEST_MANIFEST = 0,
    BZ_REQUEST_INIT_SEGMENT = 1,
    BZ_REQUEST_SEGMENT = 2,
    BZ_REQUEST_LICENSE = 3,
} BZRequestKind;

// Lifecycle. `metal_layer` is a CAMetalLayer*; `user` is an opaque host pointer
// passed back to every callback. Returns an opaque handle (NULL on failure).
void *bz_player_create(void *metal_layer, uint32_t width, uint32_t height,
                       const char *manifest_url, float start_fraction,
                       int32_t audio_passthrough, bool auto_select_subtitle,
                       bz_intercept_cb intercept_cb, bz_resolve_key_cb resolve_key_cb,
                       bz_event_cb event_cb, void *user);
void bz_player_set_size(void *handle, uint32_t width, uint32_t height, float scale);
void bz_player_destroy(void *handle);

// Playback control.
void bz_player_play(void *handle);
void bz_player_pause(void *handle);
bool bz_player_is_paused(void *handle);
void bz_player_seek_ms(void *handle, int64_t position_ms);
int64_t bz_player_position_ms(void *handle);
int64_t bz_player_duration_ms(void *handle);
void bz_player_set_volume(void *handle, float volume);

// Tracks. Returns a heap C string the caller MUST free with bz_string_free.
char *bz_player_tracks_json(void *handle);
void bz_string_free(char *s);
void bz_player_select_video(void *handle, uint32_t adapt, uint32_t repr, bool soft);
void bz_player_select_video_auto(void *handle);
void bz_player_select_audio(void *handle, uint32_t adapt, uint32_t repr);
void bz_player_select_subtitle(void *handle, uint32_t adapt, uint32_t repr);
void bz_player_clear_subtitles(void *handle);

// Generic knobs.
void bz_player_set_subtitle_style(void *handle, int32_t text_argb, int32_t outline_argb, float size_scale);
void bz_player_set_subtitle_safe_inset_bottom(void *handle, uint32_t bottom_px);
void bz_player_set_verbose_logging(bool enabled);

// Provider-hook completions (called by the host to resolve an in-flight
// intercept_cb / resolve_key_cb, identified by its token).
void bz_intercept_complete(uint64_t token, const char *url);
void bz_intercept_fail(uint64_t token, const char *message);
void bz_resolve_key_complete(uint64_t token, const uint8_t *key16);
void bz_resolve_key_fail(uint64_t token, const char *message);

#ifdef __cplusplus
}
#endif

#endif // RUSTPLAYER_FFI_H
