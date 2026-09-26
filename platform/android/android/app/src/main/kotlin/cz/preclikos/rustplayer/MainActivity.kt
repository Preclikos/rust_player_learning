package cz.preclikos.rustplayer

import android.app.Activity
import android.app.AlertDialog
import android.graphics.Color
import android.graphics.PixelFormat
import android.graphics.drawable.ColorDrawable
import android.os.Build
import android.os.Bundle
import android.os.SystemClock
import android.view.Display
import android.view.Gravity
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.widget.Button
import android.widget.FrameLayout
import android.widget.LinearLayout
import android.widget.SeekBar
import android.widget.TextView
import org.json.JSONArray
import org.json.JSONObject

/** Bundled encrypted DASH test stream (smoke test only; has an AAC audio track
 * so the Android PCM sink is exercised too). */
private const val TEST_MANIFEST_URL = "https://preclikos.cz/examples/tearsofsteel_enc/manifest.mpd"

/** Logcat tag carrying the switch-quality trace (see [MainActivity.traceEvent]). */
private const val TRACE_TAG = "rustplayer_trace"

/**
 * Host Activity for the embedded Rust player.
 *
 * Two stacked SurfaceViews (the layering real apps use):
 *   - [videoView] (bottom): MediaCodec renders into it DIRECTLY in direct mode.
 *   - [overlayView] (top, translucent): the wgpu/GLES surface (subtitles/UI, or
 *     video itself in the GL path).
 *
 * On top of those is a small transport-controls bar driven through [RustPlayer]
 * — play/pause, a seek bar, and a track picker — exercising the unified bridge
 * control surface end to end.
 */
class MainActivity : Activity(), SurfaceHolder.Callback {

    private val player by lazy { RustPlayer(this) }

    private lateinit var videoView: SurfaceView
    private lateinit var overlayView: SurfaceView
    private lateinit var videoFrame: FrameLayout

    private lateinit var playPauseButton: Button
    private lateinit var seekBar: SeekBar
    private lateinit var timeLabel: TextView

    private var videoSurface: Surface? = null
    private var overlaySurface: Surface? = null
    private var overlayW = 0
    private var overlayH = 0
    private var userSeeking = false

    // ---- BlackZone startup-storm repro (diagnostic) -------------------------
    // Mimics BlackZone's PlaybackController start path to reproduce the 1-3fps
    // direct-mode collapse: resume-seek (startFraction) + arm ABR right after
    // start + change the audio track on the FIRST onPlaying (after first frame,
    // so change_audio_track does the destructive seek(position()) rebuild on top
    // of the resume). Toggle with:  adb shell am start -n .../MainActivity \
    //   --ez storm true   (and optionally --ef storm_fraction 0.5)
    // storm_fix:
    //   (unset)      = BUG path: selectAudio on onPlaying (after first frame).
    //   before_frame = apply audio pref on onTracks (before first frame).
    //   start_param  = pass preferredAudioLang to start() — no selectAudio at all.
    // storm_second_seek (default true): a 2nd resume seek 450ms after onPlaying,
    //   mimicking BlackZone's maybeApplyMovieResume — more rebuild churn.
    // storm_passthrough: force E-AC3 audio passthrough (slow AudioTrack start).
    private var stormMode = false
    private var stormFraction = 0.5f
    private var stormFix = ""            // "", "before_frame", "start_param"
    private var stormSecondSeek = true
    private var stormPassthrough = false
    private var stormAudioApplied = false
    private var stormSecondSeekDone = false

    // ---- switch-quality trace ------------------------------------------------
    // Every bridge event, verbatim, plus a MARK line at the instant we issue a
    // control call — that pairing is what lets the offline analyser open a window
    // around a switch and decide whether the user saw a spinner. Disable with
    //   adb shell am start -n .../MainActivity --ez trace false
    private var traceEnabled = true

    // Scripted scenario (see runScenarioStep) - empty = interactive app.
    private var scenario = ""
    private var manifestUrl = TEST_MANIFEST_URL
    private var startFraction: Float? = null
    // --ez keep_alive true: BlackZone-style background handling — the player
    // survives surfaceDestroyed (Home) paused, and the new surfaces are handed
    // back on return instead of restarting the stream.
    private var keepAlive = false
    private var scenarioIterations = 5
    private var scenarioSettleMs = 12_000L
    private var scenarioWarmupMs = 12_000L
    private var scenarioStarted = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        window.setBackgroundDrawable(ColorDrawable(Color.BLACK))

        stormMode = intent.getBooleanExtra("storm", false)
        stormFraction = intent.getFloatExtra("storm_fraction", 0.5f)
        stormFix = intent.getStringExtra("storm_fix") ?: ""
        stormSecondSeek = intent.getBooleanExtra("storm_second_seek", true)
        stormPassthrough = intent.getBooleanExtra("storm_passthrough", false)
        traceEnabled = intent.getBooleanExtra("trace", true)
        readScenarioExtras()
        // --ez verbose true: engine debug lines (audio clock internals etc.).
        if (intent.getBooleanExtra("verbose", false)) player.setVerboseLogging(true)
        keepAlive = intent.getBooleanExtra("keep_alive", false)
        if (stormMode) {
            android.util.Log.i(
                "rustplayer_repro",
                "STORM fraction=$stormFraction fix='$stormFix' secondSeek=$stormSecondSeek passthrough=$stormPassthrough",
            )
        }

        val root = FrameLayout(this)

        // Video plane (bottom), shaped to content aspect in onVideoSize().
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

        // GL overlay (translucent, above the video plane).
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

        // Transport controls (regular Views draw above both SurfaceViews).
        root.addView(buildControls(), FrameLayout.LayoutParams(
            FrameLayout.LayoutParams.MATCH_PARENT,
            FrameLayout.LayoutParams.WRAP_CONTENT,
            Gravity.BOTTOM,
        ))

        setContentView(root)

        player.listener = PlayerListener()
    }

    private fun buildControls(): View {
        val bar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            setBackgroundColor(Color.argb(160, 0, 0, 0))
            setPadding(24, 16, 24, 16)
            gravity = Gravity.CENTER_VERTICAL
        }
        playPauseButton = Button(this).apply {
            text = "▮▮"
            setOnClickListener {
                traceMark("transport", "action" to if (player.isPaused) "play" else "pause")
                player.togglePlayPause()
            }
        }
        seekBar = SeekBar(this).apply {
            max = 0
            setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
                override fun onProgressChanged(sb: SeekBar, progress: Int, fromUser: Boolean) {}
                override fun onStartTrackingTouch(sb: SeekBar) { userSeeking = true }
                override fun onStopTrackingTouch(sb: SeekBar) {
                    userSeeking = false
                    traceMark("seek", "target_ms" to sb.progress.toLong())
                    player.seekTo(sb.progress.toLong())
                }
            })
        }
        timeLabel = TextView(this).apply {
            setTextColor(Color.WHITE)
            text = "0:00 / 0:00"
        }
        val tracksButton = Button(this).apply {
            text = "Tracks"
            setOnClickListener { showTracksDialog() }
        }
        bar.addView(playPauseButton)
        bar.addView(seekBar, LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f))
        bar.addView(timeLabel)
        bar.addView(tracksButton)
        return bar
    }

    private fun showTracksDialog() {
        val root = try {
            JSONObject(player.tracksJson())
        } catch (e: Exception) {
            return
        }
        val labels = ArrayList<String>()
        val actions = ArrayList<() -> Unit>()

        fun addGroup(
            prefix: String,
            key: String,
            kind: String,
            mode: String = "manual",
            pick: (Int, Int) -> Unit,
        ) {
            val arr: JSONArray = root.optJSONArray(key) ?: return
            for (i in 0 until arr.length()) {
                val t = arr.getJSONObject(i)
                val label = t.optString("label")
                labels.add("$prefix: $label")
                val adapt = t.optInt("adapt")
                val repr = t.optInt("repr")
                actions.add {
                    traceMark(
                        "select", "kind" to kind, "mode" to mode,
                        "adapt" to adapt, "repr" to repr, "label" to label,
                    )
                    pick(adapt, repr)
                }
            }
        }

        labels.add("Video: Auto (ABR)")
        actions.add {
            traceMark("select", "kind" to "video", "mode" to "auto")
            player.selectVideoAuto()
        }
        addGroup("Video", "video", "video") { a, r -> player.selectVideo(a, r) }
        // Same rungs again through the SEAMLESS path, so the two can be
        // compared back to back on one device without touching the network.
        addGroup("Video (soft)", "video", "video", mode = "soft") { a, r ->
            player.selectVideoSoft(a, r)
        }
        addGroup("Audio", "audio", "audio") { a, r -> player.selectAudio(a, r) }
        labels.add("Subtitles: Off")
        actions.add {
            traceMark("select", "kind" to "text", "mode" to "off")
            player.clearSubtitles()
        }
        addGroup("Subtitle", "text", "text") { a, r -> player.selectSubtitle(a, r) }

        AlertDialog.Builder(this)
            .setTitle("Tracks")
            .setItems(labels.toTypedArray()) { _, which -> actions[which]() }
            .show()
    }

    private inner class PlayerListener : RustPlayer.Listener {
        override fun onRawEvent(json: String) = traceEvent(json)
        override fun onVideoSize(width: Int, height: Int) = applyVideoAspect(width, height)
        override fun onTracks(json: String) {
            // FIX (a): apply the audio pref BEFORE the first frame. pipeline_live
            // is still false, so change_audio_track stores the track and play()
            // picks it up at startup — no destructive seek/rebuild.
            if (stormMode && stormFix == "before_frame") applyStormAudio("onTracks")
        }
        override fun onPlaying() {
            playPauseButton.text = "▮▮"
            maybeStartScenario()
            // BUG path (stormFix==""): apply the audio pref AFTER the first frame,
            // exactly like BlackZone's onPlaying→applyLanguagePreference.
            // pipeline_live is true, so change_audio_track does seek(position()) —
            // a rebuild on the resume + freshly-armed ABR → 1-3fps collapse.
            if (stormMode && stormFix == "") {
                applyStormAudio("onPlaying")
                maybeStormSecondSeek()
            }
        }
        override fun onPaused() { playPauseButton.text = "▶" }
        override fun onPosition(positionMs: Long, durationMs: Long) {
            if (durationMs > 0 && seekBar.max != durationMs.toInt()) seekBar.max = durationMs.toInt()
            if (!userSeeking) seekBar.progress = positionMs.toInt()
            timeLabel.text = "${fmt(positionMs)} / ${fmt(durationMs)}"
        }
    }

    // ---- scripted switch-quality scenarios ----------------------------------
    //
    // Unattended, repeatable runs of the two switching contracts:
    //   abr_soft     - the SEAMLESS path. The viewer must see nothing: no
    //                  spinner, no frame hole, no jump. Driven through
    //                  selectVideoSoft so it needs no bandwidth games.
    //   manual_video - a user quality pick. Loading is EXPECTED, and playback
    //                  must be clean once it clears.
    //   manual_audio - same contract, audio side.
    //   manual_sub   - subtitle on/off, cheapest of the rebuilds.
    //
    //   adb shell am start -n cz.preclikos.rust_player/cz.preclikos.rustplayer.MainActivity \
    //     --es scenario abr_soft --ei iterations 5 --ei settle_ms 8000 --ei warmup_ms 12000
    //
    // Every step emits a MARK first, so the analyser windows on the cause.

    private fun readScenarioExtras() {
        // --es url <manifest>: play something other than the bundled test
        // stream (e.g. the E-AC-3 one for passthrough work).
        intent.getStringExtra("url")?.takeIf { it.isNotBlank() }?.let { manifestUrl = it }
        // --ef start_fraction 0.5: resume at that fraction of the duration
        // (plain path; storm mode has its own storm_fraction).
        startFraction = intent.getFloatExtra("start_fraction", -1f).takeIf { it in 0f..1f }
        scenario = intent.getStringExtra("scenario") ?: ""
        scenarioIterations = intent.getIntExtra("iterations", 5)
        scenarioSettleMs = intent.getIntExtra("settle_ms", 8_000).toLong()
        scenarioWarmupMs = intent.getIntExtra("warmup_ms", 12_000).toLong()
    }

    private fun maybeStartScenario() {
        if (scenario.isEmpty() || scenarioStarted) return
        scenarioStarted = true
        traceMark(
            "scenario_begin", "scenario" to scenario,
            "iterations" to scenarioIterations, "settle_ms" to scenarioSettleMs,
            "warmup_ms" to scenarioWarmupMs,
        )
        // Let the pipeline settle out of its start-up transient first, so the
        // first switch is measured against steady playback, not against the
        // tail of the initial buffering.
        playPauseButton.postDelayed({ runScenarioStep(0) }, scenarioWarmupMs)
    }

    private fun scenarioAbort(why: String) {
        traceMark("scenario_abort", "scenario" to scenario, "why" to why)
    }

    private fun runScenarioStep(step: Int) {
        if (step >= scenarioIterations) {
            traceMark("scenario_end", "scenario" to scenario, "steps" to step)
            return
        }
        val tracks = try {
            JSONObject(player.tracksJson())
        } catch (e: Exception) {
            scenarioAbort("tracks json unreadable: $e")
            return
        }

        fun pickAlternating(key: String): JSONObject? {
            val arr = tracks.optJSONArray(key) ?: return null
            if (arr.length() == 0) return null
            // Alternate between the two ends so consecutive steps always mean a
            // real change; with a single entry there is nothing to alternate.
            val idx = if (arr.length() < 2) 0 else if (step % 2 == 0) arr.length() - 1 else 0
            return arr.getJSONObject(idx)
        }

        when (scenario) {
            "abr_soft", "manual_video" -> {
                val t = pickAlternating("video")
                if (t == null) {
                    scenarioAbort("no video tracks")
                    return
                }
                val soft = scenario == "abr_soft"
                val adapt = t.optInt("adapt")
                val repr = t.optInt("repr")
                traceMark(
                    "select", "kind" to "video", "mode" to if (soft) "soft" else "manual",
                    "adapt" to adapt, "repr" to repr, "label" to t.optString("label"),
                    "src" to "scenario", "step" to step,
                )
                if (soft) player.selectVideoSoft(adapt, repr) else player.selectVideo(adapt, repr)
            }
            "manual_audio" -> {
                val t = pickAlternating("audio")
                if (t == null) {
                    scenarioAbort("no audio tracks")
                    return
                }
                val adapt = t.optInt("adapt")
                val repr = t.optInt("repr")
                traceMark(
                    "select", "kind" to "audio", "mode" to "manual",
                    "adapt" to adapt, "repr" to repr, "label" to t.optString("label"),
                    "src" to "scenario", "step" to step,
                )
                player.selectAudio(adapt, repr)
            }
            "manual_sub" -> {
                val arr = tracks.optJSONArray("text")
                if (arr == null || arr.length() == 0) {
                    scenarioAbort("no subtitle tracks")
                    return
                }
                if (step % 2 == 0) {
                    val t = arr.getJSONObject(0)
                    traceMark(
                        "select", "kind" to "text", "mode" to "manual",
                        "adapt" to t.optInt("adapt"), "repr" to t.optInt("repr"),
                        "label" to t.optString("label"), "src" to "scenario", "step" to step,
                    )
                    player.selectSubtitle(t.optInt("adapt"), t.optInt("repr"))
                } else {
                    traceMark(
                        "select", "kind" to "text", "mode" to "off",
                        "src" to "scenario", "step" to step,
                    )
                    player.clearSubtitles()
                }
            }
            else -> {
                scenarioAbort("unknown scenario")
                return
            }
        }
        playPauseButton.postDelayed({ runScenarioStep(step + 1) }, scenarioSettleMs)
    }

    // ---- trace emitters ------------------------------------------------------

    /**
     * One JSONL line per bridge event, tag [TRACE_TAG]:
     * `{"t":<elapsedRealtimeMs>,"e":{<event verbatim>}}`.
     *
     * `t` is [SystemClock.elapsedRealtime] so event lines and [traceMark] lines
     * share one monotonic axis that survives across processes and does not jump
     * with wall-clock or doze.
     */
    private fun traceEvent(json: String) {
        if (!traceEnabled) return
        android.util.Log.i(TRACE_TAG, """{"t":${SystemClock.elapsedRealtime()},"e":$json}""")
    }

    /**
     * `{"t":…,"mark":"<what>","pos":<positionMs>,<fields>}` — emitted at the
     * instant a control call is issued, BEFORE the call, so the analyser can
     * anchor its window on the cause rather than on the first visible effect.
     */
    private fun traceMark(mark: String, vararg fields: Pair<String, Any?>) {
        if (!traceEnabled) return
        val o = JSONObject()
        o.put("t", SystemClock.elapsedRealtime())
        o.put("mark", mark)
        o.put("pos", if (player.isStarted) player.positionMs else -1L)
        for ((k, v) in fields) o.put(k, v)
        android.util.Log.i(TRACE_TAG, o.toString())
    }

    private fun fmt(ms: Long): String {
        if (ms <= 0) return "0:00"
        val totalSec = ms / 1000
        return "%d:%02d".format(totalSec / 60, totalSec % 60)
    }

    /** Shape the video SurfaceView to the content aspect (MediaCodec stretches). */
    private fun applyVideoAspect(width: Int, height: Int) {
        if (width <= 0 || height <= 0) return
        val parentW = (videoFrame.parent as FrameLayout).width
        val parentH = (videoFrame.parent as FrameLayout).height
        if (parentW == 0 || parentH == 0) return
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

    private inner class VideoCallback : SurfaceHolder.Callback {
        override fun surfaceCreated(holder: SurfaceHolder) {}
        override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
            videoSurface = holder.surface
            if (keepAlive && player.isStarted) {
                player.setVideoSurface(holder.surface)
                reattached()
            } else {
                maybeStart()
            }
        }
        override fun surfaceDestroyed(holder: SurfaceHolder) {
            videoSurface = null
            if (keepAlive && player.isStarted) {
                // The player pauses itself while a surface is gone.
                player.setVideoSurface(null)
            } else {
                teardown()
            }
        }
    }

    // Overlay (this) callbacks.
    override fun surfaceCreated(holder: SurfaceHolder) {}

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        overlaySurface = holder.surface
        overlayW = width
        overlayH = height
        if (player.isStarted) {
            if (keepAlive && overlayDetached) {
                overlayDetached = false
                player.setOverlaySurface(holder.surface)
                reattached()
            }
            player.setSize(width, height)
        } else {
            maybeStart()
        }
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        overlaySurface = null
        if (keepAlive && player.isStarted) {
            player.setOverlaySurface(null)
            overlayDetached = true
        } else {
            teardown()
        }
    }

    private var overlayDetached = false

    /** Both planes back after a keep-alive background (the player resumes itself). */
    private fun reattached() {
        if (overlaySurface != null && videoSurface != null && !overlayDetached) {
            traceMark("reattach")
        }
    }

    private fun maybeStart() {
        val overlay = overlaySurface ?: return
        val video = videoSurface ?: return
        if (!player.isStarted) {
            traceMark("start", "url" to manifestUrl, "storm" to stormMode)
            if (stormMode) {
                val passthrough = if (stormPassthrough) true else null
                if (stormFix == "start_param") {
                    // FIX (b): hand the audio-language preference to start() so
                    // it's applied during default selection — no post-start
                    // selectAudio, hence no rebuild. (Our test stream is en-only,
                    // so this picks the same mp4a track; the point is the path.)
                    player.start(
                        overlay, video, overlayW, overlayH, displayHdrTypes(),
                        manifestUrl = manifestUrl,
                        provider = TestProvider,
                        startFraction = stormFraction,
                        audioPassthrough = passthrough,
                        autoSelectSubtitle = false,
                        preferredAudioLang = "en",
                    )
                } else {
                    // BlackZone-style: resume mid-content + own subtitle selection.
                    player.start(
                        overlay, video, overlayW, overlayH, displayHdrTypes(),
                        manifestUrl = manifestUrl,
                        provider = TestProvider,
                        startFraction = stormFraction,
                        audioPassthrough = passthrough,
                        autoSelectSubtitle = false,
                    )
                }
                // Arm ABR immediately after start (BlackZone's selectVideoAuto()).
                traceMark("select", "kind" to "video", "mode" to "auto", "src" to "storm")
                player.selectVideoAuto()
            } else {
                player.start(
                    overlay, video, overlayW, overlayH, displayHdrTypes(),
                    manifestUrl = manifestUrl,
                    provider = TestProvider,
                    startFraction = startFraction,
                )
            }
        }
    }

    /** Apply a non-default audio track, mimicking BlackZone's applyLanguagePreference. */
    private fun applyStormAudio(where: String) {
        if (stormAudioApplied) return
        val audio = try {
            JSONObject(player.tracksJson()).optJSONArray("audio")
        } catch (e: Exception) {
            null
        } ?: return
        if (audio.length() == 0) return
        stormAudioApplied = true
        // Pick the LAST audio rep so it differs from the default pick when
        // possible (the change itself is what triggers the rebuild either way).
        val t = audio.getJSONObject(audio.length() - 1)
        val adapt = t.optInt("adapt")
        val repr = t.optInt("repr")
        android.util.Log.i("rustplayer_repro", "applyStormAudio($where) adapt=$adapt repr=$repr")
        traceMark("select", "kind" to "audio", "mode" to "manual",
            "adapt" to adapt, "repr" to repr, "src" to "storm:$where")
        player.selectAudio(adapt, repr)
    }

    /** Second resume seek 450ms after first frame — BlackZone's maybeApplyMovieResume. */
    private fun maybeStormSecondSeek() {
        if (!stormSecondSeek || stormSecondSeekDone) return
        stormSecondSeekDone = true  // fire once, like BlackZone's movieResumeApplied
        playPauseButton.postDelayed({
            val dur = player.durationMs
            if (dur > 0) {
                val target = (dur * stormFraction).toLong()
                android.util.Log.i("rustplayer_repro", "stormSecondSeek to ${target}ms")
                traceMark("seek", "target_ms" to target, "src" to "storm")
                player.seekTo(target)
            }
        }, 450)
    }

    /**
     * The :app smoke test is just one consumer of the generic library: it plays
     * the bundled encrypted test stream and supplies the baked ClearKeys via the
     * standard provider hook (a real app would call its licence server here).
     */
    private object TestProvider : RustPlayerProvider {
        // KID(hex) → 16-byte key (matches app_shared::test_clearkeys()).
        private val keys: Map<String, ByteArray> = mapOf(
            "0fd37dac41c0e987e68d43b801b1210c" to hex("fd8d9f408c2bd702970afcd3b219e791"),
            "519af81ab2d284f52aa8257d96b5e4bd" to hex("627ef72b42d98770dec20ecab46cd1f4"),
            // tearsofsteel_enc (examples/tearsofsteel_enc/keys.json)
            "5fe47a2b5a43523cb79bb96e0a15d106" to hex("9355c4ddaedb22347380f4835b1f77e5"),
            "643819c17e42b72a9fa50b617fa7db2b" to hex("635f62d75077894b3c193e5f8de0c9c1"),
        )

        override fun resolveKey(kid: ByteArray): ByteArray? = keys[kid.toHex()]
        // onRequest defaults to identity — the test stream needs no auth/rewrite.

        private fun hex(s: String): ByteArray =
            ByteArray(s.length / 2) {
                ((s[it * 2].digitToInt(16) shl 4) or s[it * 2 + 1].digitToInt(16)).toByte()
            }

        private fun ByteArray.toHex(): String = joinToString("") { "%02x".format(it) }
    }

    private fun teardown() {
        if (player.isStarted) player.release()
    }

    /** HDR formats the display can render natively (bit 0 DV, 1 HDR10, 2 HLG, 3 HDR10+). */
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
}
