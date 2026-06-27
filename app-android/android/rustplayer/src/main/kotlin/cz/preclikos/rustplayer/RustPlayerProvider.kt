package cz.preclikos.rustplayer

/**
 * Generic request/key hooks — the ExoPlayer / Shaka model. The library knows
 * NOTHING app-specific (no auth, CDN, or DRM endpoints): for every network
 * request it asks [onRequest] for the URL to actually fetch + headers, and for
 * encrypted content it asks [resolveKey] for the key. Whatever an app needs
 * (bearer tokens, CDN URL resolution, a licence server) lives inside these
 * hooks, invisible to the player.
 */
interface RustPlayerProvider {
    enum class RequestType { MANIFEST, INIT_SEGMENT, SEGMENT, LICENSE }

    /** What to actually fetch: a (possibly rewritten) URL + headers to add. */
    data class PreparedRequest(
        val url: String,
        val headers: Map<String, String> = emptyMap(),
    )

    /**
     * Called for every network request. Return the URL to fetch + any headers.
     * Default: identity (URL unchanged, no headers).
     */
    fun onRequest(url: String, type: RequestType): PreparedRequest = PreparedRequest(url)

    /**
     * ClearKey/DRM: map a 16-byte CENC key id to its 16-byte key. Return `null`
     * if unsupported / unknown (the player then fails the stream). Default: null.
     */
    fun resolveKey(kid: ByteArray): ByteArray? = null
}
