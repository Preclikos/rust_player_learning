package cz.preclikos.rustplayer

import android.util.Log

/**
 * The object handed to the native layer. The native side calls back into it
 * from Tokio worker threads and it forwards to the app's [RustPlayerProvider]
 * (+ the event listener). Pure plumbing — no app-specific or test logic lives
 * here; that's the provider's job.
 *
 * Native call shapes (kept in lock-step with platform/android/src/lib.rs):
 *  - onEvent(json)                 — one player event, unified JSON
 *  - onRequest(url, kind) -> [..]  — flat [finalUrl, k1, v1, …]; identity = [url]
 *  - resolveKey(kid) -> ByteArray? — 16-byte key, or null
 */
class PlayerBridge(
    private val provider: RustPlayerProvider,
    private val onEventListener: (String) -> Unit,
) {
    fun onEvent(json: String) {
        try {
            onEventListener(json)
        } catch (e: Exception) {
            Log.e(TAG, "onEvent listener threw: ${e.message}")
        }
    }

    /** kind: 0=MANIFEST 1=INIT_SEGMENT 2=SEGMENT 3=LICENSE. */
    fun onRequest(url: String, kind: Int): Array<String> {
        val type = when (kind) {
            0 -> RustPlayerProvider.RequestType.MANIFEST
            1 -> RustPlayerProvider.RequestType.INIT_SEGMENT
            2 -> RustPlayerProvider.RequestType.SEGMENT
            else -> RustPlayerProvider.RequestType.LICENSE
        }
        val prepared = try {
            provider.onRequest(url, type)
        } catch (e: Exception) {
            Log.e(TAG, "onRequest threw: ${e.message}")
            return arrayOf(url)
        }
        val out = ArrayList<String>(1 + prepared.headers.size * 2)
        out.add(prepared.url)
        for ((k, v) in prepared.headers) {
            out.add(k); out.add(v)
        }
        return out.toTypedArray()
    }

    fun resolveKey(kid: ByteArray): ByteArray? =
        try {
            provider.resolveKey(kid)
        } catch (e: Exception) {
            Log.e(TAG, "resolveKey threw: ${e.message}")
            null
        }

    companion object {
        private const val TAG = "rustplayer"
    }
}
