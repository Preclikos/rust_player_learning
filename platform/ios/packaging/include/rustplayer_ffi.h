// C ABI of the Rust player FFI (exported by app-ios/src/lib.rs, compiled into
// the static lib bundled in RustPlayerFFI.xcframework). Imported as the Clang
// module `RustPlayerFFI`; the Swift `RustPlayer` wrapper calls these.
//
// Keep in lock-step with app-ios/src/lib.rs.

#ifndef RUSTPLAYER_FFI_H
#define RUSTPLAYER_FFI_H

#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

// Host callbacks (fired from Rust/Tokio worker threads).
//   intercept_cb   — rewrite/auth a request; complete via rustplayer_intercept_complete/fail
//   resolve_key_cb — CENC KID (16 bytes) → ClearKey; complete via rustplayer_resolve_key_complete/fail
//   event_cb       — one player event as unified JSON
typedef void (*rustplayer_intercept_cb)(void *user, const char *url, int kind, uint64_t token);
typedef void (*rustplayer_resolve_key_cb)(void *user, const uint8_t *kid, uint64_t token);
typedef void (*rustplayer_event_cb)(void *user, const char *json);

// RequestKind values passed to rustplayer_intercept_cb.
typedef enum {
    RUSTPLAYER_REQUEST_MANIFEST = 0,
    RUSTPLAYER_REQUEST_INIT_SEGMENT = 1,
    RUSTPLAYER_REQUEST_SEGMENT = 2,
    RUSTPLAYER_REQUEST_LICENSE = 3,
} RustPlayerRequestKind;

// Lifecycle. `metal_layer` is a CAMetalLayer*; `user` is an opaque host pointer
// passed back to every callback. Returns an opaque handle (NULL on failure).
void *rustplayer_player_create(void *metal_layer, uint32_t width, uint32_t height,
                       const char *manifest_url, float start_fraction,
                       int32_t audio_passthrough, bool auto_select_subtitle,
                       rustplayer_intercept_cb intercept_cb, rustplayer_resolve_key_cb resolve_key_cb,
                       rustplayer_event_cb event_cb, void *user);
void rustplayer_player_set_size(void *handle, uint32_t width, uint32_t height, float scale);
void rustplayer_player_destroy(void *handle);

// Playback control.
void rustplayer_player_play(void *handle);
void rustplayer_player_pause(void *handle);
bool rustplayer_player_is_paused(void *handle);
void rustplayer_player_seek_ms(void *handle, int64_t position_ms);
int64_t rustplayer_player_position_ms(void *handle);
int64_t rustplayer_player_duration_ms(void *handle);
void rustplayer_player_set_volume(void *handle, float volume);

// Tracks. Returns a heap C string the caller MUST free with rustplayer_string_free.
char *rustplayer_player_tracks_json(void *handle);
void rustplayer_string_free(char *s);
void rustplayer_player_select_video(void *handle, uint32_t adapt, uint32_t repr, bool soft);
void rustplayer_player_select_video_auto(void *handle);
void rustplayer_player_select_audio(void *handle, uint32_t adapt, uint32_t repr);
void rustplayer_player_select_subtitle(void *handle, uint32_t adapt, uint32_t repr);
void rustplayer_player_clear_subtitles(void *handle);

// Generic knobs.
void rustplayer_player_set_subtitle_style(void *handle, int32_t text_argb, int32_t outline_argb, float size_scale);
void rustplayer_player_set_subtitle_safe_inset_bottom(void *handle, uint32_t bottom_px);
void rustplayer_player_set_verbose_logging(bool enabled);

// What the host's request filter wants fetched — the generic request-filter
// result (mirrors player::net::PreparedRequest; cf. Shaka request filters /
// ExoPlayer ResolvingDataSource + header setters). All fields but `url` are
// optional, so a URL-only rewrite is `{ .url = url }` with everything else
// zeroed. `headers` is a flat, NUL-terminated [k0,v0,k1,v1,...,NULL] array
// (NULL = no headers). `method` NULL = default for the kind (GET, or POST for
// license). `body` optional (e.g. a POST filter); NULL/0 = none. Headers are
// opaque key/value pairs the consumer chose — the library adds no auth scheme,
// token names, or endpoint conventions of its own.
typedef struct {
    const char *url;
    const char *const *headers;   // [k0,v0,...,NULL] or NULL
    const char *method;           // "GET"/"POST"/... or NULL
    const uint8_t *body;          // optional; NULL = none
    size_t body_len;
} RustPlayerPreparedRequest;

// Provider-hook completions (called by the host to resolve an in-flight
// intercept_cb / resolve_key_cb, identified by its token). The `prepared`
// struct and everything it points at need only stay alive for the duration of
// the call.
void rustplayer_intercept_complete(uint64_t token, const RustPlayerPreparedRequest *prepared);
void rustplayer_intercept_fail(uint64_t token, const char *message);
void rustplayer_resolve_key_complete(uint64_t token, const uint8_t *key16);
void rustplayer_resolve_key_fail(uint64_t token, const char *message);

#ifdef __cplusplus
}
#endif

#endif // RUSTPLAYER_FFI_H
