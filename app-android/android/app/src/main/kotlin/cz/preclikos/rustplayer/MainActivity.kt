package cz.preclikos.rustplayer

import android.app.Activity
import android.content.Context
import android.graphics.Color
import android.graphics.PixelFormat
import android.graphics.drawable.ColorDrawable
import android.os.Build
import android.os.Bundle
import android.view.Display
import android.view.Gravity
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import android.widget.FrameLayout

/**
 * Host Activity for the embedded Rust player smoke test.
 *
 * Two stacked SurfaceViews (the layering real apps use):
 *
 *   - [videoView] (bottom): MediaCodec renders into it DIRECTLY in the
 *     "direct" playback mode — the decoded frames ride a hardware video
 *     plane, which is what lets the HWC pass HDR10/HDR10+/DV signals
 *     (incl. dynamic metadata) through to the display untouched.
 *   - [overlayView] (top, translucent): the wgpu/GLES surface. In direct
 *     mode it carries only subtitles/UI over transparent pixels; in the
 *     classic mode it carries the video itself (GL tonemap path).
 *
 * Both surfaces are forwarded to Rust over JNI once both exist.
 */
class MainActivity : Activity(), SurfaceHolder.Callback {

    // Opaque pointer to the Rust-side Handle (0 = not started).
    private var handle: Long = 0L

    private lateinit var videoView: SurfaceView
    private lateinit var overlayView: SurfaceView
    private lateinit var videoFrame: FrameLayout

    private var videoSurface: Surface? = null
    private var overlaySurface: Surface? = null
    private var overlayW = 0
    private var overlayH = 0

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        instance = this
        // Keep the screen on during playback without a WakeLock.
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        // Black window background: any region not covered by the video or
        // overlay (pre-first-frame, pillarbox gutters) must be black, not
        // the theme default (observed green on the Google TV Streamer).
        window.setBackgroundDrawable(ColorDrawable(Color.BLACK))

        val root = FrameLayout(this)

        // Video plane (bottom). Wrapped in its own FrameLayout whose size is
        // adjusted to the content aspect ratio from onVideoSize() — MediaCodec
        // stretches to fill the surface, so the surface itself must have the
        // video's aspect.
        videoFrame = FrameLayout(this)
        videoView = SurfaceView(this)
        videoView.holder.addCallback(VideoCallback())
        videoFrame.addView(
            videoView,
            FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT,
                FrameLayout.LayoutParams.MATCH_PARENT,
            ),
        )
        root.addView(
            videoFrame,
            FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT,
                FrameLayout.LayoutParams.MATCH_PARENT,
                Gravity.CENTER,
            ),
        )

        // GL overlay (top, translucent).
        overlayView = SurfaceView(this)
        overlayView.setZOrderMediaOverlay(true)
        overlayView.holder.setFormat(PixelFormat.TRANSLUCENT)
        overlayView.holder.addCallback(this)
        root.addView(
            overlayView,
            FrameLayout.LayoutParams(
                FrameLayout.LayoutParams.MATCH_PARENT,
                FrameLayout.LayoutParams.MATCH_PARENT,
            ),
        )

        setContentView(root)
    }

    /** Called from Rust (any thread) when the content size is known. */
    fun onVideoSize(width: Int, height: Int) {
        if (width <= 0 || height <= 0) return
        runOnUiThread {
            val parentW = (videoFrame.parent as FrameLayout).width
            val parentH = (videoFrame.parent as FrameLayout).height
            if (parentW == 0 || parentH == 0) return@runOnUiThread
            val videoAspect = width.toFloat() / height.toFloat()
            val parentAspect = parentW.toFloat() / parentH.toFloat()
            val lp = videoFrame.layoutParams as FrameLayout.LayoutParams
            if (videoAspect > parentAspect) {
                lp.width = parentW
                lp.height = (parentW / videoAspect).toInt()
            } else {
                lp.height = parentH
                lp.width = (parentH * videoAspect).toInt()
            }
            lp.gravity = Gravity.CENTER
            videoFrame.layoutParams = lp
        }
    }

    private inner class VideoCallback : SurfaceHolder.Callback {
        override fun surfaceCreated(holder: SurfaceHolder) {}

        override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
            videoSurface = holder.surface
            maybeStart()
        }

        override fun surfaceDestroyed(holder: SurfaceHolder) {
            videoSurface = null
            // The video surface dying invalidates the whole player (the
            // decoder renders into it) — tear down like the overlay path.
            teardown()
        }
    }

    // Overlay (this) callbacks ------------------------------------------------

    override fun surfaceCreated(holder: SurfaceHolder) {
        // Defer to surfaceChanged, which also gives us the size.
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        overlaySurface = holder.surface
        overlayW = width
        overlayH = height
        if (handle != 0L) {
            nativeSetSize(handle, width, height)
        } else {
            maybeStart()
        }
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        overlaySurface = null
        teardown()
    }

    private fun maybeStart() {
        val overlay = overlaySurface ?: return
        val video = videoSurface ?: return
        if (handle == 0L) {
            handle = nativeStart(
                applicationContext, this, overlay, video,
                overlayW, overlayH, displayHdrTypes(),
            )
        }
    }

    private fun teardown() {
        if (handle != 0L) {
            nativeDestroy(handle)
            handle = 0L
        }
    }

    /**
     * Bitmask of HDR formats the current display can render natively
     * (bit 0 = Dolby Vision, 1 = HDR10, 2 = HLG, 3 = HDR10+), passed to the
     * native player so it can choose PQ passthrough over shader tonemapping.
     * 0 = SDR-only display (or caps unavailable).
     */
    private fun displayHdrTypes(): Int {
        val display: Display? = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            display
        } else {
            @Suppress("DEPRECATION")
            windowManager.defaultDisplay
        }
        val caps = display?.hdrCapabilities ?: return 0
        var mask = 0
        for (t in caps.supportedHdrTypes) {
            when (t) {
                Display.HdrCapabilities.HDR_TYPE_DOLBY_VISION -> mask = mask or (1 shl 0)
                Display.HdrCapabilities.HDR_TYPE_HDR10 -> mask = mask or (1 shl 1)
                Display.HdrCapabilities.HDR_TYPE_HLG -> mask = mask or (1 shl 2)
                Display.HdrCapabilities.HDR_TYPE_HDR10_PLUS -> mask = mask or (1 shl 3)
            }
        }
        return mask
    }

    companion object {
        @JvmStatic
        var instance: MainActivity? = null
            private set

        init {
            System.loadLibrary("app_android")
        }

        // @JvmStatic on companion-object externs produces JNI symbol names on the
        // *outer* class (Java_cz_preclikos_rustplayer_MainActivity_…), which matches
        // what the Rust extern "system" fn names already expose.

        // The Context is forwarded so the Rust side can seed ndk_context with
        // (JavaVM, Activity); the Activity itself is kept for UI callbacks
        // (onVideoSize aspect updates).
        @JvmStatic
        private external fun nativeStart(
            context: Context,
            activity: MainActivity,
            overlaySurface: Surface,
            videoSurface: Surface,
            width: Int,
            height: Int,
            displayHdrTypes: Int,
        ): Long

        @JvmStatic
        private external fun nativeSetSize(handle: Long, width: Int, height: Int)

        @JvmStatic
        private external fun nativeDestroy(handle: Long)
    }
}
