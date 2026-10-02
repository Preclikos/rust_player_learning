import Foundation
import CoreGraphics
import QuartzCore
import AVFoundation
import UIKit
import RustPlayerFFI

/// Provider policy supplied by the host: URL/auth rewriting and DRM key
/// resolution. `intercept` defaults to passthrough; `resolveKey` is required.
/// Mirrors the Rust `BridgeHost` provider hooks (and the Kotlin `PlayerBridge`).
public protocol RustPlayerProvider: AnyObject {
    func intercept(url: String, kind: RustPlayerRequestKind) async throws -> RustPreparedRequest
    func resolveKey(kid: Data) async throws -> Data
}

public extension RustPlayerProvider {
    func intercept(url: String, kind: RustPlayerRequestKind) async throws -> RustPreparedRequest {
        RustPreparedRequest(url: url) // passthrough
    }
}

/// What the interceptor wants fetched — the generic request-filter result
/// (cf. Shaka request filters / ExoPlayer `ResolvingDataSource` + header
/// setters). Rewrite the `url` and/or add `headers`, and optionally override
/// the `method`/`body`. `headers` are opaque key/value pairs the consumer
/// chose — the library applies no auth scheme of its own. All but `url`
/// default to empty, so `RustPreparedRequest(url:)` stays a plain passthrough.
public struct RustPreparedRequest {
    public let url: String
    /// Headers to ADD to the request (client defaults are kept).
    public let headers: [(String, String)]
    /// Method override ("GET"/"POST"/…); nil = default for the kind.
    public let method: String?
    /// Body substitution (e.g. a POST filter); nil = none.
    public let body: Data?
    public init(url: String,
                headers: [(String, String)] = [],
                method: String? = nil,
                body: Data? = nil) {
        self.url = url
        self.headers = headers
        self.method = method
        self.body = body
    }
}

/// Player events, delivered on the main thread. The protocol is `@MainActor`
/// (events are dispatched to main), so conformers — typically a `@MainActor`
/// view/engine — can touch main-actor state from these callbacks without
/// isolation warnings. All methods have default no-op implementations —
/// implement only what you need.
@MainActor
public protocol RustPlayerDelegate: AnyObject {
    func rustPlayerDidPrepare(_ player: RustPlayer)
    func rustPlayer(_ player: RustPlayer, didLoadTracks json: String)
    func rustPlayerDidStartPlaying(_ player: RustPlayer)
    func rustPlayerDidPause(_ player: RustPlayer)
    func rustPlayerDidBuffer(_ player: RustPlayer)
    func rustPlayer(_ player: RustPlayer, position positionMs: Int64, duration durationMs: Int64)
    func rustPlayer(_ player: RustPlayer, videoSize size: CGSize)
    func rustPlayerDidEnd(_ player: RustPlayer)
    func rustPlayer(_ player: RustPlayer, didError kind: String, detail: String)
}

public extension RustPlayerDelegate {
    func rustPlayerDidPrepare(_ player: RustPlayer) {}
    func rustPlayer(_ player: RustPlayer, didLoadTracks json: String) {}
    func rustPlayerDidStartPlaying(_ player: RustPlayer) {}
    func rustPlayerDidPause(_ player: RustPlayer) {}
    func rustPlayerDidBuffer(_ player: RustPlayer) {}
    func rustPlayer(_ player: RustPlayer, position positionMs: Int64, duration durationMs: Int64) {}
    func rustPlayer(_ player: RustPlayer, videoSize size: CGSize) {}
    func rustPlayerDidEnd(_ player: RustPlayer) {}
    func rustPlayer(_ player: RustPlayer, didError kind: String, detail: String) {}
}

/// Idiomatic Swift wrapper over the Rust player FFI. Create one per
/// `CAMetalLayer`, set a `delegate` + `provider`, then drive playback. Events
/// are decoded from the unified JSON and delivered to `delegate` on main.
public final class RustPlayer {
    public weak var delegate: RustPlayerDelegate?
    public weak var provider: RustPlayerProvider?

    private var handle: UnsafeMutableRawPointer?
    /// The callback `user` pointer: a +1 box holding `self` weakly, released
    /// only after `rustplayer_player_destroy` has returned (from then on Rust
    /// makes no further callback). A callback racing `deinit` reads nil
    /// instead of a freed `RustPlayer`.
    private var callbackBox: Unmanaged<CallbackBox>?
    private var lastSize: CGSize = .zero

    public init() {}

    deinit { destroy() }

    public var isStarted: Bool { handle != nil }

    /// Build the player on `layer` and play `manifestURL`. The `provider`
    /// resolves auth/CDN/DRM (all app-side). `startFraction` resumes at 0..1 of
    /// duration; `audioPassthrough` nil = library default; `autoSelectSubtitle`
    /// default-on.
    public func start(
        layer: CAMetalLayer,
        manifestURL: String,
        provider: RustPlayerProvider,
        startFraction: Float? = nil,
        audioPassthrough: Bool? = nil,
        autoSelectSubtitle: Bool = true
    ) {
        guard handle == nil else { return }
        self.provider = provider
        let scale = layer.contentsScale > 0 ? layer.contentsScale : 1
        let w = UInt32(layer.bounds.width * scale)
        let h = UInt32(layer.bounds.height * scale)
        let box = Unmanaged.passRetained(CallbackBox(self))
        let user = box.toOpaque()
        let ap: Int32 = audioPassthrough == nil ? -1 : (audioPassthrough! ? 1 : 0)
        installVideoLayer(under: layer)
        let videoPtr = videoLayer.map { Unmanaged.passUnretained($0).toOpaque() }
        handle = manifestURL.withCString { urlPtr in
            rustplayer_player_create_ex(
                Unmanaged.passUnretained(layer).toOpaque(),
                videoPtr,
                hdrMask(),
                max(w, 1), max(h, 1),
                urlPtr, startFraction ?? -1, ap, autoSelectSubtitle,
                interceptCallback, resolveKeyCallback, eventCallback,
                user
            )
        }
        if handle != nil { callbackBox = box } else { box.release(); removeVideoLayer() }
        // iOS fails the video layer in the background (-11847); the player
        // falls back to its own renderer. Re-installing the layer on return
        // re-arms direct mode and rebuilds the pipeline onto it.
        foregroundObserver = NotificationCenter.default.addObserver(
            forName: UIApplication.willEnterForegroundNotification, object: nil, queue: .main
        ) { [weak self] _ in self?.rearmVideoLayer() }
    }

    // --- direct mode ---

    /// Direct mode (default on, set before `start`): an
    /// `AVSampleBufferDisplayLayer` is placed under the render layer and,
    /// while HDR output applies (`hdrOutput`), HDR10 / HLG / Dolby Vision go
    /// through the system video pipeline straight to the display — dynamic
    /// metadata included — with the render layer carrying subtitles only.
    /// SDR screens, `.off`, or a layer failure fall back to the player's
    /// own rendering (tonemap). Needs the render layer to have a superlayer
    /// (be on screen) at `start`.
    public var directMode: Bool = true

    private var videoLayer: AVSampleBufferDisplayLayer?
    private weak var metalLayer: CAMetalLayer?
    private var foregroundObserver: NSObjectProtocol?

    private func rearmVideoLayer() {
        guard let handle, let v = videoLayer else { return }
        rustplayer_player_set_video_output_layer(handle, Unmanaged.passUnretained(v).toOpaque())
    }

    private func installVideoLayer(under layer: CAMetalLayer) {
        metalLayer = layer
        guard directMode, let parent = layer.superlayer else { return }
        let v = AVSampleBufferDisplayLayer()
        v.videoGravity = .resizeAspect
        v.backgroundColor = UIColor.black.cgColor
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        v.frame = layer.frame
        parent.insertSublayer(v, below: layer)
        // Transparent where there are no subtitles (the player configures
        // its surface non-opaque in direct mode too).
        layer.isOpaque = false
        CATransaction.commit()
        videoLayer = v
    }

    private func removeVideoLayer() {
        videoLayer?.removeFromSuperlayer()
        videoLayer = nil
    }

    // --- HDR output ---

    /// How HDR (PQ / HLG) video reaches the screen.
    public enum HdrOutput {
        /// Hand HDR to the display when it has EDR headroom
        /// (`displaySupportsHdr`), otherwise tonemap to SDR in the player.
        case auto
        /// Always tonemap to SDR in the player.
        case off
        /// Always hand HDR to the display — testing only. With
        /// `directMode` the system video pipeline plays HDR / Dolby Vision
        /// even on an SDR screen and tonemaps it itself; without, iOS 16+
        /// maps the PQ layer down (older systems stay on the tonemap).
        case forced
    }

    /// Default `.auto`. Takes effect from the next frame.
    public var hdrOutput: HdrOutput = .auto {
        didSet { applyHdrOutput() }
    }

    /// Can this device's screen show HDR video right now: the OS
    /// considers it HDR-eligible and the panel has EDR headroom
    /// (iOS 16+). False on SDR panels, e.g. iPhone SE / 8 class devices.
    public static var displaySupportsHdr: Bool {
        guard AVPlayer.eligibleForHDRPlayback else { return false }
        if #available(iOS 16.0, *) {
            return UIScreen.main.potentialEDRHeadroom > 1.0
        }
        return false
    }

    /// The display also takes Dolby Vision (`AVPlayer.availableHDRModes`).
    public static var displaySupportsDolbyVision: Bool {
        displaySupportsHdr && AVPlayer.availableHDRModes.contains(.dolbyVision)
    }

    /// Re-evaluate the screen (call after e.g. an external display was
    /// connected). `start()` and `hdrOutput` changes already do this.
    public func refreshHdrOutput() { applyHdrOutput() }

    private func applyHdrOutput() {
        guard let handle else { return }
        rustplayer_player_set_display_hdr_types(handle, hdrMask())
    }

    /// Display.HdrCapabilities-order mask: bit 0 = DV, 1 = HDR10, 2 = HLG.
    private func hdrMask() -> UInt32 {
        let hdr10Hlg: UInt32 = (1 << 1) | (1 << 2)
        switch hdrOutput {
        case .auto:
            guard Self.displaySupportsHdr else { return 0 }
            return hdr10Hlg | (Self.displaySupportsDolbyVision ? 1 : 0)
        case .off: return 0
        case .forced: return hdr10Hlg
        }
    }

    public func setSize(_ size: CGSize, scale: CGFloat) {
        guard let handle else { return }
        if let v = videoLayer, let m = metalLayer {
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            v.frame = m.frame
            CATransaction.commit()
        }
        rustplayer_player_set_size(handle, UInt32(size.width * scale), UInt32(size.height * scale), Float(scale))
    }

    public func play() { handle.map { rustplayer_player_play($0) } }
    public func pause() { handle.map { rustplayer_player_pause($0) } }
    public func togglePlayPause() {
        guard let handle else { return }
        if rustplayer_player_is_paused(handle) { rustplayer_player_play(handle) } else { rustplayer_player_pause(handle) }
    }
    public var isPaused: Bool { handle.map { rustplayer_player_is_paused($0) } ?? false }
    public func seek(toMs ms: Int64) { handle.map { rustplayer_player_seek_ms($0, ms) } }
    public var positionMs: Int64 { handle.map { rustplayer_player_position_ms($0) } ?? 0 }
    public var durationMs: Int64 { handle.map { rustplayer_player_duration_ms($0) } ?? 0 }
    public func setVolume(_ v: Float) { handle.map { rustplayer_player_set_volume($0, v) } }

    /// Fetch ClearKey content keys WRAPPED from `url` instead of through the
    /// provider's resolveKey (see docs/CLEARKEY_WRAPPED_LICENCE.md): per KID an
    /// ephemeral ECDH P-256 exchange, HKDF-SHA256 and AES-256-GCM, so no key
    /// crosses the wire in the clear. Authorization headers come from the
    /// provider's intercept for kind "license". Call right after `start`.
    /// `clientSecret`: this app version's secret (base64url, >= 16 bytes), the
    /// one the licence server holds for it; inject it at build time, never
    /// commit it. `secretId`: its id (scenario A); nil = the server finds the
    /// secret from the request proof (scenario B). False = malformed secret.
    @discardableResult
    public func setWrappedLicence(url: String, clientSecret: String, secretId: String? = nil) -> Bool {
        guard let handle else { return false }
        return url.withCString { u in
            clientSecret.withCString { s in
                if let id = secretId {
                    return id.withCString { i in rustplayer_player_set_wrapped_licence(handle, u, s, i) }
                }
                return rustplayer_player_set_wrapped_licence(handle, u, s, nil)
            }
        }
    }

    public func tracksJSON() -> String {
        guard let handle, let c = rustplayer_player_tracks_json(handle) else { return "{}" }
        defer { rustplayer_string_free(c) }
        return String(cString: c)
    }

    /// Buffer size and network-outage tolerance; 0 keeps a field's default
    /// (30 s, fill continuously, 96 MiB, 180 s). Call right after create;
    /// later calls apply from the next seek or track change.
    public func setBufferConfig(maxSecs: UInt32 = 0, minSecs: UInt32 = 0, maxMb: UInt32 = 0, outageSecs: UInt32 = 0) {
        handle.map { rustplayer_player_set_buffer_config($0, maxSecs, minSecs, maxMb, outageSecs) }
    }

    /// Debug HUD snapshot JSON: pipelines, segments, buffers, decoder and
    /// output paths, sync, network, ABR, recent event log. Built on call —
    /// poll at 1-2 Hz only while a HUD is visible.
    public func debugJSON() -> String {
        guard let handle, let c = rustplayer_player_debug_json(handle) else { return "{}" }
        defer { rustplayer_string_free(c) }
        return String(cString: c)
    }

    /// The same snapshot as ready-made HUD text lines, ending with the
    /// `events` newest event-log lines. Identical on every platform.
    public func debugText(events: UInt32 = 8) -> String {
        guard let handle, let c = rustplayer_player_debug_text(handle, events) else { return "" }
        defer { rustplayer_string_free(c) }
        return String(cString: c)
    }

    public func selectVideo(adapt: UInt32, repr: UInt32, soft: Bool = false) {
        handle.map { rustplayer_player_select_video($0, adapt, repr, soft) }
    }
    public func selectVideoAuto() { handle.map { rustplayer_player_select_video_auto($0) } }
    public func selectAudio(adapt: UInt32, repr: UInt32) { handle.map { rustplayer_player_select_audio($0, adapt, repr) } }
    public func selectSubtitle(adapt: UInt32, repr: UInt32) { handle.map { rustplayer_player_select_subtitle($0, adapt, repr) } }
    public func clearSubtitles() { handle.map { rustplayer_player_clear_subtitles($0) } }

    // --- generic knobs ---

    /// ARGB ints (like Android `Color` / ExoPlayer `CaptionStyleCompat`).
    public func setSubtitleStyle(textArgb: Int32, outlineArgb: Int32, sizeScale: Float) {
        handle.map { rustplayer_player_set_subtitle_style($0, textArgb, outlineArgb, sizeScale) }
    }
    public func setSubtitleSafeInsetBottom(_ px: UInt32) {
        handle.map { rustplayer_player_set_subtitle_safe_inset_bottom($0, px) }
    }
    /// Debug/compat: force HDR video to an 8-bit decode destination (the
    /// in-player tonemap still runs, at 8-bit precision). Applies from the
    /// next pipeline (re)build (play / retry / ABR swap).
    public func setHdrDecode8bit(_ enabled: Bool) {
        handle.map { rustplayer_player_set_hdr_decode_8bit($0, enabled) }
    }
    public func setVerboseLogging(_ enabled: Bool) {
        rustplayer_player_set_verbose_logging(enabled)
    }

    public func destroy() {
        if let o = foregroundObserver { NotificationCenter.default.removeObserver(o); foregroundObserver = nil }
        if let handle { rustplayer_player_destroy(handle); self.handle = nil }
        removeVideoLayer()
        callbackBox?.release()
        callbackBox = nil
    }

    // Decode one unified-JSON event and dispatch to the delegate (main thread).
    @MainActor
    fileprivate func handleEvent(_ json: String) {
        guard let data = json.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let type = obj["type"] as? String else { return }
        let d = delegate
        switch type {
        case "prepared": d?.rustPlayerDidPrepare(self)
        case "tracks_ready": d?.rustPlayer(self, didLoadTracks: tracksJSON())
        case "playing": d?.rustPlayerDidStartPlaying(self)
        case "paused": d?.rustPlayerDidPause(self)
        case "buffering": d?.rustPlayerDidBuffer(self)
        case "position":
            d?.rustPlayer(self,
                          position: (obj["position_ms"] as? NSNumber)?.int64Value ?? 0,
                          duration: (obj["duration_ms"] as? NSNumber)?.int64Value ?? 0)
        case "video_size", "stats":
            let w = (obj["width"] as? NSNumber)?.doubleValue ?? 0
            let h = (obj["height"] as? NSNumber)?.doubleValue ?? 0
            if w > 0 && h > 0 {
                let s = CGSize(width: w, height: h)
                if s != lastSize { lastSize = s; d?.rustPlayer(self, videoSize: s) }
            }
        case "end_of_stream": d?.rustPlayerDidEnd(self)
        case "error":
            d?.rustPlayer(self,
                          didError: obj["kind"] as? String ?? "",
                          detail: obj["detail"] as? String ?? "")
        default: break
        }
    }

    fileprivate var providerRef: RustPlayerProvider? { provider }
}

// MARK: - C callbacks (non-capturing → convertible to C function pointers).
// They recover the `RustPlayer` from the `user` pointer and bridge provider
// hooks to the async token completions.

/// What the `user` pointer points at. Weak, so a callback never extends or
/// resurrects a player that is being deinitialised.
private final class CallbackBox {
    weak var player: RustPlayer?
    init(_ player: RustPlayer) { self.player = player }
}

private func livePlayer(_ user: UnsafeMutableRawPointer) -> RustPlayer? {
    Unmanaged<CallbackBox>.fromOpaque(user).takeUnretainedValue().player
}

private let eventCallback: rustplayer_event_cb = { user, json in
    guard let user, let json, let player = livePlayer(user) else { return }
    let s = String(cString: json)
    // Hop to the main actor: handleEvent (and the @MainActor delegate it calls)
    // is main-isolated. `Task { @MainActor in }` keeps this iOS 15-compatible
    // (MainActor.assumeIsolated is iOS 17+).
    Task { @MainActor [weak player] in player?.handleEvent(s) }
}

private let interceptCallback: rustplayer_intercept_cb = { user, url, kind, token in
    guard let user, let url else { rustplayer_intercept_fail(token, "null intercept args"); return }
    guard let player = livePlayer(user) else { rustplayer_intercept_fail(token, "player destroyed"); return }
    let provider = player.providerRef
    let urlStr = String(cString: url)
    let reqKind = RustPlayerRequestKind(rawValue: UInt32(bitPattern: kind))
    Task {
        do {
            guard let provider else {
                completeIntercept(token, RustPreparedRequest(url: urlStr))
                return
            }
            let prepared = try await provider.intercept(url: urlStr, kind: reqKind)
            completeIntercept(token, prepared)
        } catch {
            rustplayer_intercept_fail(token, error.localizedDescription)
        }
    }
}

/// Marshal a `RustPreparedRequest` into the flat C `RustPlayerPreparedRequest` and
/// resolve the in-flight intercept. Every C string is `strdup`'d so the
/// pointers stay valid across the synchronous completion call, then freed.
private func completeIntercept(_ token: UInt64, _ prepared: RustPreparedRequest) {
    let urlC = strdup(prepared.url)
    let methodC: UnsafeMutablePointer<CChar>? = prepared.method.map { strdup($0) }
    // Flat [k0,v0,...] of owned C strings (NUL terminator appended below).
    var ownedHeaders: [UnsafeMutablePointer<CChar>?] = []
    for (k, v) in prepared.headers {
        ownedHeaders.append(strdup(k))
        ownedHeaders.append(strdup(v))
    }
    defer {
        free(urlC)
        if let methodC { free(methodC) }
        for p in ownedHeaders { free(p) }
    }
    // `UnsafePointer.init` is overloaded — spell out the Pointee so the
    // mutable→const conversion isn't ambiguous (it otherwise fails to compile).
    var headerPtrs: [UnsafePointer<CChar>?] = ownedHeaders.map { $0.map { UnsafePointer<CChar>($0) } }
    headerPtrs.append(nil) // NUL-terminate

    func send(body: UnsafePointer<UInt8>?, bodyLen: Int) {
        headerPtrs.withUnsafeBufferPointer { hb in
            var req = RustPlayerPreparedRequest(
                url: urlC.map { UnsafePointer<CChar>($0) },
                headers: prepared.headers.isEmpty ? nil : hb.baseAddress,
                method: methodC.map { UnsafePointer<CChar>($0) },
                body: body,
                body_len: bodyLen)
            rustplayer_intercept_complete(token, &req)
        }
    }

    if let body = prepared.body, !body.isEmpty {
        body.withUnsafeBytes { raw in
            send(body: raw.bindMemory(to: UInt8.self).baseAddress, bodyLen: body.count)
        }
    } else {
        send(body: nil, bodyLen: 0)
    }
}

private let resolveKeyCallback: rustplayer_resolve_key_cb = { user, kid, token in
    guard let user, let kid else { rustplayer_resolve_key_fail(token, "null kid"); return }
    guard let player = livePlayer(user) else { rustplayer_resolve_key_fail(token, "player destroyed"); return }
    let provider = player.providerRef
    let kidData = Data(bytes: kid, count: 16)
    Task {
        do {
            guard let provider else { throw RustPlayerError.noProvider }
            let key = try await provider.resolveKey(kid: kidData)
            guard key.count == 16 else { throw RustPlayerError.badKeyLength(key.count) }
            key.withUnsafeBytes { raw in
                rustplayer_resolve_key_complete(token, raw.bindMemory(to: UInt8.self).baseAddress!)
            }
        } catch {
            rustplayer_resolve_key_fail(token, error.localizedDescription)
        }
    }
}

public enum RustPlayerError: Error, CustomStringConvertible {
    case noProvider
    case badKeyLength(Int)
    public var description: String {
        switch self {
        case .noProvider: return "no RustPlayerProvider set"
        case .badKeyLength(let n): return "resolveKey returned \(n) bytes, expected 16"
        }
    }
}
