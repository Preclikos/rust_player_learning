package cz.preclikos.rustplayer

import android.content.Context
import android.view.Surface

/**
 * Thin JNI binding for the unified bridge core (`app_shared::bridge`). One
 * `external` per exported native function; symbol names are
 * `Java_cz_preclikos_rustplayer_NativeBridge_<name>` (see app-android/src/lib.rs).
 *
 * Apps don't use this directly — [RustPlayer] wraps it in an idiomatic API.
 */
object NativeBridge {
    init {
        System.loadLibrary("app_android")
    }

    external fun nativeStart(
        context: Context,
        bridge: PlayerBridge,
        overlaySurface: Surface,
        videoSurface: Surface,
        width: Int,
        height: Int,
        displayHdrTypes: Int,
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
}
