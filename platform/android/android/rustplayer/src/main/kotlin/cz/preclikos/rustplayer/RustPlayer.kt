package cz.preclikos.rustplayer

import android.content.Context
import android.os.Handler
import android.os.Looper
import android.view.Surface
import org.json.JSONObject

/**
 * Idiomatic, GENERAL-PURPOSE player (the ExoPlayer/Shaka model): give it a
 * manifest URL + a [RustPlayerProvider] (auth / CDN / DRM, all app-side) and it
 * plays. Events arrive on the main thread via [Listener]. No app-specific
 * concepts live in this library.
 */
class RustPlayer(private val context: Context) {

    /** Player events, delivered on the main thread. All methods are optional. */
    interface Listener {
        fun onPrepared() {}
        fun onTracks(json: String) {}
        fun onPlaying() {}
        fun onPaused() {}
        fun onBuffering() {}
        fun onPosition(positionMs: Long, durationMs: Long) {}
        fun onVideoSize(width: Int, height: Int) {}
        fun onEnded() {}
        fun onError(kind: String, detail: String) {}
    }

    var listener: Listener? = null

    private var handle: Long = 0L
    private val main = Handler(Looper.getMainLooper())
    private var bridge: PlayerBridge? = null

    val isStarted: Boolean get() = handle != 0L

    /**
     * Build the player on the given surfaces and play [manifestUrl].
     *
     * @param provider auth/CDN/DRM hooks (default: identity requests, no keys).
     * @param startFraction resume at 0..1 of duration, or null to start at 0.
     * @param audioPassthrough true/false to force, null for the library default.
     * @param autoSelectSubtitle default-on (ExoPlayer-like); pass false if the
     *   app drives its own subtitle selection.
     * @param preferredAudioLang BCP-47 (e.g. "cs", "en") applied during default
     *   selection — i.e. BEFORE the first frame. Use this instead of a
     *   post-start [selectAudio]: selecting audio after playback starts triggers
     *   a seek-rebuild (and, on a resume, can stall direct-mode decode); a
     *   start-time preference is picked up with no rebuild. null = codec default.
     * @param preferredSubtitleLang BCP-47 applied during default selection;
     *   honoured even when [autoSelectSubtitle] is false. null = auto policy.
     */
    fun start(
        overlay: Surface,
        video: Surface,
        width: Int,
        height: Int,
        displayHdrTypes: Int,
        manifestUrl: String,
        provider: RustPlayerProvider = object : RustPlayerProvider {},
        startFraction: Float? = null,
        audioPassthrough: Boolean? = null,
        autoSelectSubtitle: Boolean = true,
        preferredAudioLang: String? = null,
        preferredSubtitleLang: String? = null,
    ) {
        if (handle != 0L) return
        val b = PlayerBridge(provider) { json -> main.post { dispatch(json) } }
        bridge = b
        handle = NativeBridge.nativeStart(
            context.applicationContext, b, overlay, video, width, height, displayHdrTypes,
            manifestUrl,
            startFraction ?: -1f,
            when (audioPassthrough) {
                null -> -1
                false -> 0
                true -> 1
            },
            autoSelectSubtitle,
            preferredAudioLang,
            preferredSubtitleLang,
        )
    }

    fun setSize(width: Int, height: Int) {
        if (handle != 0L) NativeBridge.nativeSetSize(handle, width, height)
    }

    fun play() {
        if (handle != 0L) NativeBridge.nativePlay(handle)
    }

    fun pause() {
        if (handle != 0L) NativeBridge.nativePause(handle)
    }

    fun togglePlayPause() {
        if (handle == 0L) return
        if (NativeBridge.nativeIsPaused(handle)) play() else pause()
    }

    val isPaused: Boolean get() = handle != 0L && NativeBridge.nativeIsPaused(handle)

    fun seekTo(positionMs: Long) {
        if (handle != 0L) NativeBridge.nativeSeekMs(handle, positionMs)
    }

    val positionMs: Long get() = if (handle != 0L) NativeBridge.nativePositionMs(handle) else 0L
    val durationMs: Long get() = if (handle != 0L) NativeBridge.nativeDurationMs(handle) else 0L

    fun setVolume(volume: Float) {
        if (handle != 0L) NativeBridge.nativeSetVolume(handle, volume)
    }

    fun tracksJson(): String = if (handle != 0L) NativeBridge.nativeGetTracksJson(handle) else "{}"

    fun selectVideo(adapt: Int, repr: Int) {
        if (handle != 0L) NativeBridge.nativeSetVideoTrack(handle, adapt, repr)
    }

    fun selectVideoAuto() {
        if (handle != 0L) NativeBridge.nativeSetVideoAuto(handle)
    }

    fun selectAudio(adapt: Int, repr: Int) {
        if (handle != 0L) NativeBridge.nativeSetAudioTrack(handle, adapt, repr)
    }

    fun selectSubtitle(adapt: Int, repr: Int) {
        if (handle != 0L) NativeBridge.nativeSetSubtitleTrack(handle, adapt, repr)
    }

    fun clearSubtitles() {
        if (handle != 0L) NativeBridge.nativeClearSubtitles(handle)
    }

    // --- generic knobs (ExoPlayer-style) ---

    /** Re-attach on a surface swap, or detach (null) on background. */
    fun setVideoSurface(surface: Surface?) {
        if (handle != 0L) NativeBridge.nativeSetVideoOutputWindow(handle, surface)
    }

    fun setSubtitleSafeInsetBottom(px: Int) {
        if (handle != 0L) NativeBridge.nativeSetSubtitleSafeInsetBottom(handle, px)
    }

    fun setAdaptiveFrameRate(enabled: Boolean) {
        if (handle != 0L) NativeBridge.nativeSetAdaptiveFrameRate(handle, enabled)
    }

    /** ARGB ints (Android `Color`), like ExoPlayer `CaptionStyleCompat`. */
    fun setSubtitleStyle(textArgb: Int, outlineArgb: Int, sizeScale: Float) {
        if (handle != 0L) NativeBridge.nativeSetSubtitleStyle(handle, textArgb, outlineArgb, sizeScale)
    }

    /** Verbose logging (default off; gates per-frame vsync/HEALTH spam). */
    fun setVerboseLogging(enabled: Boolean) {
        NativeBridge.nativeSetVerboseLogging(enabled)
    }

    fun release() {
        if (handle != 0L) {
            NativeBridge.nativeDestroy(handle)
            handle = 0L
            bridge = null
        }
    }

    private fun dispatch(json: String) {
        val l = listener ?: return
        val o = try {
            JSONObject(json)
        } catch (e: Exception) {
            return
        }
        when (o.optString("type")) {
            "prepared" -> l.onPrepared()
            "tracks_ready" -> l.onTracks(tracksJson())
            "playing" -> l.onPlaying()
            "paused" -> l.onPaused()
            "buffering" -> l.onBuffering()
            "position" -> l.onPosition(o.optLong("position_ms"), o.optLong("duration_ms"))
            "video_size", "stats" -> {
                val w = o.optInt("width")
                val h = o.optInt("height")
                if (w > 0 && h > 0) l.onVideoSize(w, h)
            }
            "end_of_stream" -> l.onEnded()
            "error" -> l.onError(o.optString("kind"), o.optString("detail"))
        }
    }
}
