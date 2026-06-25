package cz.preclikos.rustplayer

import android.content.Context
import android.os.Handler
import android.os.Looper
import android.view.Surface
import org.json.JSONObject

/**
 * Idiomatic Kotlin wrapper over [NativeBridge] + the unified bridge core. This
 * is the API an app actually uses: typed control methods plus a [Listener] that
 * receives decoded player events on the **main thread**.
 *
 * It mirrors the shape a future generated binding (`player → ready-made Kotlin`)
 * would expose, so the test app exercises the real product ergonomics.
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

    // Native worker fires onEvent off-thread; hop to the main looper before
    // touching the listener / UI.
    private val bridge = PlayerBridge { json -> main.post { dispatch(json) } }

    val isStarted: Boolean get() = handle != 0L

    fun start(overlay: Surface, video: Surface, width: Int, height: Int, displayHdrTypes: Int) {
        if (handle != 0L) return
        handle = NativeBridge.nativeStart(
            context.applicationContext, bridge, overlay, video, width, height, displayHdrTypes,
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

    fun release() {
        if (handle != 0L) {
            NativeBridge.nativeDestroy(handle)
            handle = 0L
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
