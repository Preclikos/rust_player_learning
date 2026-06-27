import Foundation
import CoreGraphics
import QuartzCore
import RustPlayerFFI

/// Provider policy supplied by the host: URL/auth rewriting and DRM key
/// resolution. `intercept` defaults to passthrough; `resolveKey` is required.
/// Mirrors the Rust `BridgeHost` provider hooks (and the Kotlin `PlayerBridge`).
public protocol RustPlayerProvider: AnyObject {
    func intercept(url: String, kind: BZRequestKind) async throws -> RustPreparedRequest
    func resolveKey(kid: Data) async throws -> Data
}

public extension RustPlayerProvider {
    func intercept(url: String, kind: BZRequestKind) async throws -> RustPreparedRequest {
        RustPreparedRequest(url: url) // passthrough
    }
}

/// What the interceptor wants fetched. (Headers/method/body are a future
/// extension — the FFI completion currently carries only the final URL.)
public struct RustPreparedRequest {
    public let url: String
    public init(url: String) { self.url = url }
}

/// Player events, delivered on the main thread. All methods have default
/// no-op implementations — implement only what you need.
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
        let user = Unmanaged.passUnretained(self).toOpaque()
        let ap: Int32 = audioPassthrough == nil ? -1 : (audioPassthrough! ? 1 : 0)
        handle = manifestURL.withCString { urlPtr in
            bz_player_create(
                Unmanaged.passUnretained(layer).toOpaque(),
                max(w, 1), max(h, 1),
                urlPtr, startFraction ?? -1, ap, autoSelectSubtitle,
                interceptCallback, resolveKeyCallback, eventCallback,
                user
            )
        }
    }

    public func setSize(_ size: CGSize, scale: CGFloat) {
        guard let handle else { return }
        bz_player_set_size(handle, UInt32(size.width * scale), UInt32(size.height * scale), Float(scale))
    }

    public func play() { handle.map { bz_player_play($0) } }
    public func pause() { handle.map { bz_player_pause($0) } }
    public func togglePlayPause() {
        guard let handle else { return }
        if bz_player_is_paused(handle) { bz_player_play(handle) } else { bz_player_pause(handle) }
    }
    public var isPaused: Bool { handle.map { bz_player_is_paused($0) } ?? false }
    public func seek(toMs ms: Int64) { handle.map { bz_player_seek_ms($0, ms) } }
    public var positionMs: Int64 { handle.map { bz_player_position_ms($0) } ?? 0 }
    public var durationMs: Int64 { handle.map { bz_player_duration_ms($0) } ?? 0 }
    public func setVolume(_ v: Float) { handle.map { bz_player_set_volume($0, v) } }

    public func tracksJSON() -> String {
        guard let handle, let c = bz_player_tracks_json(handle) else { return "{}" }
        defer { bz_string_free(c) }
        return String(cString: c)
    }

    public func selectVideo(adapt: UInt32, repr: UInt32, soft: Bool = false) {
        handle.map { bz_player_select_video($0, adapt, repr, soft) }
    }
    public func selectVideoAuto() { handle.map { bz_player_select_video_auto($0) } }
    public func selectAudio(adapt: UInt32, repr: UInt32) { handle.map { bz_player_select_audio($0, adapt, repr) } }
    public func selectSubtitle(adapt: UInt32, repr: UInt32) { handle.map { bz_player_select_subtitle($0, adapt, repr) } }
    public func clearSubtitles() { handle.map { bz_player_clear_subtitles($0) } }

    // --- generic knobs ---

    /// ARGB ints (like Android `Color` / ExoPlayer `CaptionStyleCompat`).
    public func setSubtitleStyle(textArgb: Int32, outlineArgb: Int32, sizeScale: Float) {
        handle.map { bz_player_set_subtitle_style($0, textArgb, outlineArgb, sizeScale) }
    }
    public func setSubtitleSafeInsetBottom(_ px: UInt32) {
        handle.map { bz_player_set_subtitle_safe_inset_bottom($0, px) }
    }
    public func setVerboseLogging(_ enabled: Bool) {
        bz_player_set_verbose_logging(enabled)
    }

    public func destroy() {
        if let handle { bz_player_destroy(handle); self.handle = nil }
    }

    // Decode one unified-JSON event and dispatch to the delegate (main thread).
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

private let eventCallback: bz_event_cb = { user, json in
    guard let user, let json else { return }
    let player = Unmanaged<RustPlayer>.fromOpaque(user).takeUnretainedValue()
    let s = String(cString: json)
    DispatchQueue.main.async { player.handleEvent(s) }
}

private let interceptCallback: bz_intercept_cb = { user, url, kind, token in
    guard let user, let url else { bz_intercept_fail(token, "null intercept args"); return }
    let player = Unmanaged<RustPlayer>.fromOpaque(user).takeUnretainedValue()
    let urlStr = String(cString: url)
    let reqKind = BZRequestKind(rawValue: UInt32(bitPattern: kind))
    Task {
        do {
            guard let provider = player.providerRef else {
                urlStr.withCString { bz_intercept_complete(token, $0) }
                return
            }
            let prepared = try await provider.intercept(url: urlStr, kind: reqKind)
            prepared.url.withCString { bz_intercept_complete(token, $0) }
        } catch {
            bz_intercept_fail(token, error.localizedDescription)
        }
    }
}

private let resolveKeyCallback: bz_resolve_key_cb = { user, kid, token in
    guard let user, let kid else { bz_resolve_key_fail(token, "null kid"); return }
    let player = Unmanaged<RustPlayer>.fromOpaque(user).takeUnretainedValue()
    let kidData = Data(bytes: kid, count: 16)
    Task {
        do {
            guard let provider = player.providerRef else { throw RustPlayerError.noProvider }
            let key = try await provider.resolveKey(kid: kidData)
            guard key.count == 16 else { throw RustPlayerError.badKeyLength(key.count) }
            key.withUnsafeBytes { raw in
                bz_resolve_key_complete(token, raw.bindMemory(to: UInt8.self).baseAddress!)
            }
        } catch {
            bz_resolve_key_fail(token, error.localizedDescription)
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
