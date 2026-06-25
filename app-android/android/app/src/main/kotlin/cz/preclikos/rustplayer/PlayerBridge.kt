package cz.preclikos.rustplayer

import android.util.Log

/**
 * Host callback object handed to the native bridge core. The native side calls
 * back into it from Tokio worker threads:
 *
 *  - [onEvent]    — one player event as unified JSON (see `app_shared::bridge`).
 *  - [resolveKey] — provider hook: a CENC Key ID (16 bytes) → its ClearKey.
 *
 * This is the **test provider**: [resolveKey] just returns the baked ClearKeys
 * for the bundled preclikos test stream (mirrors `app_shared::test_clearkeys`).
 * A product app implements its real licence-server call here — the bridge shape
 * is identical, only the policy differs.
 */
class PlayerBridge(
    private val onEventListener: (String) -> Unit,
) {
    // KID(hex) -> 16-byte key, matching app_shared::test_clearkeys().
    private val keys: Map<String, ByteArray> = mapOf(
        "0fd37dac41c0e987e68d43b801b1210c" to hex("fd8d9f408c2bd702970afcd3b219e791"),
        "519af81ab2d284f52aa8257d96b5e4bd" to hex("627ef72b42d98770dec20ecab46cd1f4"),
    )

    /** Called from a native worker thread; [onEventListener] re-dispatches. */
    fun onEvent(json: String) {
        try {
            onEventListener(json)
        } catch (e: Exception) {
            Log.e(TAG, "onEvent listener threw: ${e.message}")
        }
    }

    /**
     * Called from a native worker thread. Returns 16 bytes for a known KID, or
     * an empty array on miss (the native side treats that as a licence error).
     */
    fun resolveKey(kid: ByteArray): ByteArray {
        val key = keys[kid.toHex()]
        if (key == null) {
            Log.e(TAG, "resolveKey: no key for KID ${kid.toHex()}")
            return ByteArray(0)
        }
        return key
    }

    companion object {
        private const val TAG = "app_android"

        private fun hex(s: String): ByteArray =
            ByteArray(s.length / 2) {
                ((s[it * 2].digitToInt(16) shl 4) or s[it * 2 + 1].digitToInt(16)).toByte()
            }

        private fun ByteArray.toHex(): String = joinToString("") { "%02x".format(it) }
    }
}
