package cz.preclikos.rustplayer

import android.content.Context
import android.view.Surface

/**
 * Thin JNI binding for the `bridge` core. One `external` per exported native
 * function; symbol names are `Java_cz_preclikos_rustplayer_NativeBridge_<name>`
 * (see platform/android/src/lib.rs). Loads librustplayer.so.
 *
 * Apps don't use this directly — [RustPlayer] wraps it in an idiomatic API.
 */
object NativeBridge {
    init {
        System.loadLibrary("rustplayer")
    }

    external fun nativeStart(
        context: Context,
        bridge: PlayerBridge,
        overlaySurface: Surface,
        videoSurface: Surface,
        width: Int,
        height: Int,
        displayHdrTypes: Int,
        manifestUrl: String,
        startFraction: Float,        // < 0 = no resume
        audioPassthrough: Int,       // -1 = default, 0 = off, 1 = on
        autoSelectSubtitle: Boolean,
        preferredAudioLang: String?,    // BCP-47, null = codec default
        preferredSubtitleLang: String?, // BCP-47, null = auto-select policy
    ): Long

    external fun nativeSetSize(handle: Long, width: Int, height: Int)
    external fun nativePlay(handle: Long)
    external fun nativePause(handle: Long)
    external fun nativeIsPaused(handle: Long): Boolean
    external fun nativeSeekMs(handle: Long, positionMs: Long)
    external fun nativePositionMs(handle: Long): Long
    external fun nativeDurationMs(handle: Long): Long
    external fun nativeSetVolume(handle: Long, volume: Float)
    external fun nativeGetTracksJson(handle: Long): String
    external fun nativeSetVideoTrack(handle: Long, adapt: Int, repr: Int)
    external fun nativeSetVideoAuto(handle: Long)
    external fun nativeSetAudioTrack(handle: Long, adapt: Int, repr: Int)
    external fun nativeSetSubtitleTrack(handle: Long, adapt: Int, repr: Int)
    external fun nativeClearSubtitles(handle: Long)
    external fun nativeDestroy(handle: Long)

    // Generic player knobs.
    external fun nativeSetVideoOutputWindow(handle: Long, surface: Surface?)
    external fun nativeSetSubtitleSafeInsetBottom(handle: Long, bottomPx: Int)
    external fun nativeSetAdaptiveFrameRate(handle: Long, enabled: Boolean)
    external fun nativeSetSubtitleStyle(handle: Long, textArgb: Int, outlineArgb: Int, sizeScale: Float)
    external fun nativeSetVerboseLogging(enabled: Boolean)
}
