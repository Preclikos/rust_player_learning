package cz.preclikos.rustplayer

import android.app.Activity
import android.content.Context
import android.os.Bundle
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager

/**
 * Host Activity for the embedded Rust player smoke test.
 *
 * Owns a [SurfaceView] and forwards its [Surface] lifecycle to the Rust side
 * over JNI. The Rust player (libapp_android.so) renders straight into the
 * Surface via an ANativeWindow — this is the embed model real apps use, not
 * winit's NativeActivity.
 */
class MainActivity : Activity(), SurfaceHolder.Callback {

    // Opaque pointer to the Rust-side Handle (0 = not started).
    private var handle: Long = 0L

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Keep the screen on during playback without a WakeLock.
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        val view = SurfaceView(this)
        view.holder.addCallback(this)
        setContentView(view)
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        // Defer to surfaceChanged, which also gives us the size.
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        if (handle == 0L) {
            handle = nativeStart(applicationContext, holder.surface, width, height)
        } else {
            nativeSetSize(handle, width, height)
        }
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        // The Surface is about to become invalid — tear the player down before
        // returning (contract of surfaceDestroyed).
        if (handle != 0L) {
            nativeDestroy(handle)
            handle = 0L
        }
    }

    companion object {
        init {
            System.loadLibrary("app_android")
        }

        // @JvmStatic on companion-object externs produces JNI symbol names on the
        // *outer* class (Java_cz_preclikos_rustplayer_MainActivity_…), which matches
        // what the Rust extern "system" fn names already expose.

        // The Context is forwarded so the Rust side can seed ndk_context with
        // (JavaVM, Activity) — cpal and other transitive deps look it up there.
        @JvmStatic
        private external fun nativeStart(
            context: Context,
            surface: Surface,
            width: Int,
            height: Int,
        ): Long

        @JvmStatic
        private external fun nativeSetSize(handle: Long, width: Int, height: Int)

        @JvmStatic
        private external fun nativeDestroy(handle: Long)
    }
}
