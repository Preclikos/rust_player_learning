# Recommendation — generic embeddable player API for the `:rustplayer` AAR

> **Status: ✅ implemented (Android device-verified; iOS mirrored).** A = generic
> `RustPlayerProvider` (`onRequest(url,type)->PreparedRequest` + `resolveKey(kid)`);
> native `intercept` now upcalls Kotlin `onRequest` (URL rewrite + headers), no
> baked test logic in the library. B = `RustPlayer.start(…, manifestUrl, provider,
> startFraction, audioPassthrough, autoSelectSubtitle)` → threaded through
> `nativeStart`/`bridge::start`. C = `setVideoSurface`/`setSubtitleSafeInsetBottom`/
> `setAdaptiveFrameRate`/`setSubtitleStyle`/`setVerboseLogging`. The `:app` smoke
> test now supplies a `TestProvider` (baked keys via `resolveKey`) + the test URL.
> iOS mirrored (`RustPlayerProvider` intercept/resolveKey already; `rustplayer_player_create`
> takes the URL + config; subtitle-style/safe-inset/verbose knobs) — not built on a
> Mac yet. **Re-publish** a new version (was 0.0.1, test-shaped) after the planned
> project restructure.



**Principle (per integrator):** the AAR must be a **general-purpose player library —
like ExoPlayer or Shaka Player — with NO app-specific concepts baked in.** No
"bearer", no "storageId:slug", no license-endpoint URLs in the API. The library
only knows: *here is a request, give me back the (possibly rewritten) URL +
headers*, and *here is a key id, give me the key*. Whatever auth / CDN / DRM an
app needs is what the **app** puts inside those generic hooks — invisible to the
player. This is exactly the ExoPlayer / Shaka model.

Good news: `app_shared::bridge::BridgeHost` already has the right generic shape
(`intercept(url, kind) -> PreparedRequest`, `resolve_key(kid) -> [u8;16]`,
`on_event(json)`). Phase 1's Kotlin `PlayerBridge` is just a hardcoded test
provider; the work is to **surface those generic hooks to the consumer**, not to
add anything BlackZone-shaped.

Verified against `4a040d6` (`app-android/android/rustplayer/.../{RustPlayer,PlayerBridge,NativeBridge}.kt`).

---

## A — generic request interceptor (like Shaka request filters / ExoPlayer ResolvingDataSource + headers)

Today `PlayerBridge` only does `resolveKey` (baked keys) + `onEvent`; native
`intercept` is default passthrough. Expose ONE generic per-request hook:

```kotlin
enum class RequestType { MANIFEST, SEGMENT, INIT_SEGMENT, LICENSE }

data class PreparedRequest(
    val url: String,                       // may be rewritten (CDN resolution, signing, …)
    val headers: Map<String, String> = emptyMap(),
)

interface RustPlayerProvider {
    /** Called for every network request. Return the URL to actually fetch + headers.
     *  Default: identity (return the url unchanged, no headers). */
    fun onRequest(url: String, type: RequestType): PreparedRequest = PreparedRequest(url)

    /** ClearKey/DRM: map a CENC key id to its 16-byte key. Default: not supported. */
    fun resolveKey(kid: ByteArray): ByteArray? = null
}
```
This subsumes header injection AND URL rewriting into one filter (Shaka's
`networkingEngine.registerRequestFilter`; ExoPlayer's `ResolvingDataSource` +
`HttpDataSource` header setters). The native `AndroidHost::intercept` just forwards
`(url, kind)` to `onRequest` and uses the returned `PreparedRequest`; `resolve_key`
forwards to `resolveKey`. The player has zero app knowledge. The smoke-test `:app`
ships a provider whose `resolveKey` returns its baked keys and whose `onRequest` is
identity.

## B — `load`/`start` takes a URL + config (like ExoPlayer setMediaItem / Shaka load)

`RustPlayer.start(overlay, video, w, h, hdr)` plays a hardcoded stream. Make it
generic:

```kotlin
fun start(
    overlay: Surface, video: Surface, width: Int, height: Int, displayHdrTypes: Int,
    manifestUrl: String,
    provider: RustPlayerProvider = object : RustPlayerProvider {},
    startFraction: Float? = null,       // resume at 0..1 of duration (StartConfig.start_fraction)
    audioPassthrough: Boolean? = null,  // null = library default; true/false = caller's choice
    autoSelectSubtitle: Boolean = true, // ExoPlayer-like default-on; apps with their own
                                        // track logic pass false
)
```
Maps straight onto `bridge::start(player, manifestUrl, host, StartConfig{…})`.

## C — standard player knobs (parity with ExoPlayer's surface/track/format API)

`RustPlayer` has play/pause/seek/volume/select{Video,Audio,Subtitle}/clearSubtitles/
setSize/tracksJson/release. Add the rest (all already `pub fn` on the native
`Player`, via `BridgeHandle::player()`), all generic:
- `setVideoSurface(surface: Surface?)` — re-attach on a surface swap / detach on
  destroy (ExoPlayer `setVideoSurface(null)`); avoids rendering to an abandoned
  window after background→foreground.
- `setSubtitleSafeInsets(bottomPx: Int)` — caption safe area.
- `setAdaptiveFrameRate(enabled: Boolean)` — AFR / display-mode matching.
- `setSubtitleStyle(textColor: Int, outlineColor: Int, sizeScale: Float)` — ARGB,
  like ExoPlayer `CaptionStyleCompat`.
- a log-verbosity toggle (default quiet; gate the per-frame vsync/HEALTH spam).

## Events / tracks (already generic — keep)

`RustPlayer.Listener` (onPrepared/onTracks/onPlaying/onPaused/onBuffering/onPosition/
onVideoSize/onEnded/onError) decoding the unified JSON is the right ExoPlayer-style
`Player.Listener`. `tracksJson()` → keep the flat schema
(`{durationMs,video[],audio[],text[]}` with `forced` on text) as the documented
track-info contract for `selectAudio/selectSubtitle/selectVideo(adapt, repr)`.

---

## Notes
- Mirror the iOS `RustPlayerProvider` (intercept/resolveKey) so both platforms
  share one generic shape.
- **No app names in the library.** BlackZone is just one consumer: its
  `onRequest` adds its bearer + rewrites its CDN URIs; its `resolveKey` fetches
  from its license server — all in app code. The reference for that logic is our
  interim `BlackZoneAndroidRust/rust-bridge/src/lib.rs`, but it stays in the app,
  not the library.
- Keep the smoke-test `:app` working via the default provider + a test stream URL.
