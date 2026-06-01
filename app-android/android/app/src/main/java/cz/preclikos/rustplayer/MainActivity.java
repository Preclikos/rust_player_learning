package cz.preclikos.rustplayer;

import android.app.Activity;
import android.os.Bundle;
import android.view.Surface;
import android.view.SurfaceHolder;
import android.view.SurfaceView;
import android.view.WindowManager;

/**
 * Host Activity for the embedded Rust player smoke test.
 *
 * Owns a {@link SurfaceView} and forwards its {@link Surface} lifecycle to the
 * Rust side over JNI. The Rust player (libapp_android.so) renders straight into
 * the Surface via an ANativeWindow — this is the embed model real apps use,
 * not winit's NativeActivity.
 */
public class MainActivity extends Activity implements SurfaceHolder.Callback {

    static {
        System.loadLibrary("app_android");
    }

    // Opaque pointer to the Rust-side Handle (0 = not started).
    private long handle = 0;

    private static native long nativeStart(Surface surface, int width, int height);
    private static native void nativeSetSize(long handle, int width, int height);
    private static native void nativeDestroy(long handle);

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        // Keep the screen on during playback without a WakeLock.
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);

        SurfaceView view = new SurfaceView(this);
        view.getHolder().addCallback(this);
        setContentView(view);
    }

    @Override
    public void surfaceCreated(SurfaceHolder holder) {
        // Defer to surfaceChanged, which also gives us the size.
    }

    @Override
    public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
        if (handle == 0) {
            handle = nativeStart(holder.getSurface(), width, height);
        } else {
            nativeSetSize(handle, width, height);
        }
    }

    @Override
    public void surfaceDestroyed(SurfaceHolder holder) {
        // The Surface is about to become invalid — tear the player down before
        // returning (contract of surfaceDestroyed).
        if (handle != 0) {
            nativeDestroy(handle);
            handle = 0;
        }
    }
}
