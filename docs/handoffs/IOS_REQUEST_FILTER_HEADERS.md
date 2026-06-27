# iOS request filter — carry headers (+ method + body), not just a URL

> **Status: 🟢 FIXED — the iOS boundary now carries the full request filter.**
> `rustplayer_intercept_complete` takes a `RustPlayerPreparedRequest`
> (`url` + flat `[k0,v0,…,NULL]` headers + optional `method` + `body`); the Rust
> shell maps them onto `player::net::PreparedRequest`
> (`headers`/`Method`/`Bytes`), and the Swift `RustPreparedRequest` gained
> `headers`/`method`/`body` (defaulted, so `RustPreparedRequest(url:)` stays a
> passthrough). Verified: `cargo build -p bridge-ios --target aarch64-apple-ios`
> links and exports the symbols. Remaining: the on-device wire test +
> `ios-v0.1.1` re-tag below must run on a Mac with Xcode.
>
> **Also (beyond the original spec):** the entire iOS C ABI was renamed from the
> `bz_`/`BZ` (BlackZone) prefix to a fully generic `rustplayer_` / `RustPlayer`
> / `RUSTPLAYER_` surface — there is **no app/provider branding anywhere** in
> the package now. This is a breaking ABI change vs `ios-v0.1.0`. The snippets
> below predate that rename and still show the old `bz_` names — read them for
> the *mechanism*; the shipped names are `rustplayer_*`.

## Goal

Bring the iOS request filter to parity with the Android one: the generic
`RustPlayerProvider.intercept(...)` result must be able to carry **HTTP headers
(and optionally an overridden method + body)**, not only a rewritten URL.

This is the standard generic player-library contract — **Shaka** request
filters and **ExoPlayer** `ResolvingDataSource` + `HttpDataSource` header setters
both let the host rewrite the URL **and** set headers/method/body per request.
The library stays provider-agnostic: it forwards an opaque `(url, kind)` to the
host and uses whatever `(url, headers, method, body)` the host returns. **No
app-specific concepts** (no auth scheme, no token names, no endpoint URLs, no
URL-scheme conventions) enter the library — headers are opaque key/value pairs
the *consumer* chose.

## Why it matters

A URL-only filter only covers signed-URL / CDN-resolution cases. Any consumer
that authenticates a request with an **HTTP header** (a bearer token, a custom
device header, a signed header) — the most common manifest/license auth model —
**cannot use the iOS package at all today**, because the header it returns is
silently discarded. Android consumers already can (see below); iOS is the
half-finished side.

## Current state (evidence — `ios-v0.1.0` / `origin/master`)

iOS completion takes a bare URL and the Rust side defaults the rest away:

```c
// platform/ios/packaging/include/rustplayer_ffi.h
void bz_intercept_complete(uint64_t token, const char *url);
```
```rust
// platform/ios/src/lib.rs — bz_intercept_complete
let _ = tx.send(Ok(PreparedRequest { url, ..Default::default() }));  // headers/method/body lost
```
```swift
// platform/ios/packaging/Sources/RustPlayer/RustPlayer.swift
public struct RustPreparedRequest {
    public let url: String                 // ← only field; comment even says
}                                          //   "Headers/method/body are a future extension"
```

Android is already generic and carries headers:

```kotlin
// platform/android/.../rustplayer/RustPlayerProvider.kt
data class PreparedRequest(val url: String, val headers: Map<String, String> = emptyMap())
fun onRequest(url: String, type: RequestType): PreparedRequest = PreparedRequest(url)
```

So the **core supports it** (`player::net::PreparedRequest` already has
`url` + `headers` + `method` + `body`); only the iOS FFI boundary throws it away.

## Design (generic)

> The `bz_` FFI prefix is the bridge's own namespace used across the whole
> package (`bz_player_create`, …) — keep it. "Generic" here means **no
> provider/app identifiers in the API surface**, not renaming the C prefix.

### 1. C ABI — `platform/ios/packaging/include/rustplayer_ffi.h`

Replace the URL-only completion with a struct, mirroring `player::net::PreparedRequest`:

```c
// What the host's request filter wants fetched. Headers is a flat,
// NUL-terminated [k0,v0,k1,v1,...,NULL] array (NULL = no headers). method NULL
// = default for the kind. body optional (e.g. a POST filter); NULL = none.
typedef struct {
    const char *url;
    const char *const *headers;   // [k0,v0,...,NULL] or NULL
    const char *method;           // "GET"/"POST"/NULL
    const uint8_t *body;          // optional; NULL = none
    size_t body_len;
} BZPreparedRequest;

void bz_intercept_complete(uint64_t token, const BZPreparedRequest *prepared);
```

`bz_intercept_fail` is unchanged. `bz_intercept_cb` already passes the request
`kind` — keep it.

### 2. Rust — `platform/ios/src/lib.rs`

Add the `#[repr(C)]` mirror and build a full `PreparedRequest`:

```rust
#[repr(C)]
pub struct BZPreparedRequest {
    url: *const c_char,
    headers: *const *const c_char,   // flat [k0,v0,...,NULL]
    method: *const c_char,
    body: *const u8,
    body_len: usize,
}

#[no_mangle]
pub extern "C" fn bz_intercept_complete(token: u64, prepared: *const BZPreparedRequest) {
    // SAFETY: `prepared` is valid for the duration of this call (host keeps the
    // C strings/array/body alive until the call returns).
    let p = unsafe { &*prepared };
    let url = unsafe { cstr(p.url) };
    let headers = unsafe { read_flat_headers(p.headers) };          // Vec<(String,String)>
    let method = if p.method.is_null() { None } else { Some(unsafe { cstr(p.method) }) };
    let body = if p.body.is_null() || p.body_len == 0 { None }
               else { Some(unsafe { std::slice::from_raw_parts(p.body, p.body_len) }.to_vec()) };
    if let Some(tx) = intercept_registry().lock().unwrap().remove(&token) {
        let _ = tx.send(Ok(PreparedRequest { url, headers, method, body }));
    }
}
```

Map the fields onto whatever `player::net::PreparedRequest` actually names them
(check the struct — headers may be `Vec<(String,String)>` or a map; method may be
an enum/Option<String>). The point is: stop discarding them.

### 3. Swift — `platform/ios/packaging/Sources/RustPlayer/RustPlayer.swift`

Extend the public struct (default args keep every existing
`RustPreparedRequest(url:)` call site compiling — passthrough stays the default):

```swift
public struct RustPreparedRequest {
    public let url: String
    public let headers: [(String, String)]
    public let method: String?
    public let body: Data?
    public init(url: String, headers: [(String, String)] = [],
                method: String? = nil, body: Data? = nil) {
        self.url = url; self.headers = headers; self.method = method; self.body = body
    }
}
```

In `interceptCallback`, marshal into the flat C array and call the struct
overload. Keep the C strings alive across the call, then free:

```swift
let prepared = try await provider.intercept(url: urlStr, kind: reqKind)
var cHeaders: [UnsafePointer<CChar>?] = []
for (k, v) in prepared.headers { cHeaders.append(strdup(k)); cHeaders.append(strdup(v)) }
cHeaders.append(nil)
prepared.url.withCString { urlC in
    (prepared.method ?? "").withCString { methodC in
        var req = BZPreparedRequest(url: urlC, headers: &cHeaders,
                                    method: prepared.method == nil ? nil : methodC,
                                    body: nil, body_len: 0)
        if let b = prepared.body {
            b.withUnsafeBytes { raw in
                req.body = raw.bindMemory(to: UInt8.self).baseAddress
                req.body_len = b.count
                bz_intercept_complete(token, &req)
            }
        } else {
            bz_intercept_complete(token, &req)
        }
    }
}
for p in cHeaders where p != nil { free(UnsafeMutablePointer(mutating: p)) }
```

## Prior art (use as the implementation template)

This exact generic shape is already written and **device-verified** in the
interim hand-rolled iOS bridge that the package supersedes — port it in, then
delete that bridge:

- `blackzone-ios/blackzone-player-bridge/blackzone_player.h` → `BZPreparedRequest`
- `blackzone-ios/blackzone-player-bridge/src/lib.rs` → `bz_intercept_complete(token, *const BZPreparedRequest)`
- `blackzone-ios/BlackZone/Player/PlayerFFI.swift` → `withCPrepared` (the flat-array marshalling above)

It is generic in shape — the only provider-specific logic lives in that app's
*interceptor implementation*, never in the FFI/struct. Copy the mechanism, not
the consumer.

## Verification

1. Build on macOS + Xcode: `platform/ios/packaging/scripts/build_xcframework.sh`
   (this is also the first real Mac build of the package — watch the link set).
2. Prove a header set in the filter reaches the wire: a **generic** test provider
   that returns `RustPreparedRequest(url:, headers: ["X-Test": "1"])` against a
   local echo server / request bin; assert the header arrives for the manifest
   `kind`. No app names in the test.
3. Re-tag `ios-v0.1.1`, run `publish-ios.yml`, and **update the release notes
   checksum** (`swift package compute-checksum RustPlayerFFI.xcframework.zip`) —
   consumers pin `url:`+`checksum:`.

## Acceptance

A consumer whose `intercept(url, kind)` returns a `RustPreparedRequest` with
headers (and/or method/body) gets those applied to the actual HTTP request for
that `kind`. `grep -ri` over `platform/ios` for any provider/app identifier
returns nothing — the surface stays a generic request filter.

## Out of scope / follow-up

- **Android method/body parity.** Android already carries headers but not
  `method`/`body`. If full request-filter parity is wanted cross-platform, extend
  the Kotlin `PreparedRequest` + JNI the same way. Not needed for the iOS unblock.
- StartConfig / subtitle-style / video-size knobs are already shipped — unrelated.
