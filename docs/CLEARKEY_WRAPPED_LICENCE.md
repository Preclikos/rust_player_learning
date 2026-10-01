# ClearKey with wrapped keys: licence endpoint contract (v2)

ClearKey is not DRM. The client must hold the content key in the clear to
decrypt, and this repository is public. What can be hardened is everything
*around* the key. This endpoint closes the cheapest attacks and keeps the
licence server from acting as a public key-vending service:

- **No key on the wire.** The content key is wrapped for one client's
  ephemeral ECDH key, so a network capture or a proxy sees only ciphertext.
- **No key in JavaScript.** On the web, the wrapped key is unwrapped straight
  into a **non-extractable** WebCrypto `CryptoKey`.
- **Only our apps.** Each app version carries a **client secret** that the
  backend also holds and that never crosses the wire.
  - A client without it is refused before anything is wrapped, and could not
    unwrap a response anyway.
  - Deleting or revoking one row in the backend's table switches that app
    version off.
- **Only entitled users.** The endpoint is authenticated and checked per KID
  (the same `Licences` lookup as the existing endpoint), so a stolen request
  does not yield another user's keys.

What it does **not** do: stop someone who extracts the secret from an app
binary and writes their own client, or stop a modified client from decrypting
content it is entitled to. That needs real DRM (Widevine / PlayReady /
FairPlay via EME).

The secret raises the cost of that attack (reverse-engineer a binary instead
of reading this repository) and makes it revocable per version. The web
build's secret ships in a JavaScript bundle, so treat it as low-trust: give
the web its own row and revoke it independently.

The scheme is JWE ECDH-ES in spirit (RFC 7518 §4.6: ephemeral P-256 key
agreement, a key wrapped with AES-GCM, the ephemeral public key carried as a
JWK `epk`). It is simplified to what WebCrypto does natively:

- HKDF-SHA256 instead of Concat KDF;
- one salt per response;
- the KID as GCM additional data;
- the client secret mixed into the KDF input.

Protocol v1 (a constant HKDF info, no client secret) is gone: v2 clients send
`"v": 2` and accept only `alg: "ECDH-HKDF-A256GCM-v2"`.

## Wire format

`POST /licence/wrapped` (JSON, authenticated like the existing licence call)

```json
{
  "v": 2,
  "kids": ["UZr4GrLShPUqqCV9lrXkvQ"],
  "epk": { "kty": "EC", "crv": "P-256", "x": "…base64url…", "y": "…base64url…" },
  "proof": "…base64url, 32 bytes…",
  "sid": "android-2.4.0"
}
```

| Field | Meaning |
|---|---|
| `v` | Protocol version, `2`. Anything else is a 400. |
| `kids` | 1-16 KIDs, base64url of the 16 raw bytes, exactly as in the existing endpoint. |
| `epk` | The client's ephemeral P-256 public key (fresh per request). |
| `proof` | `HMAC-SHA256(client secret, proof message)`, see below. Always sent. |
| `sid` | Optional. The non-secret id the backend files the secret under. Present = scenario A, absent = scenario B. |

Response 200: the existing `keys` JWK array, with `k` wrapped:

```json
{
  "epk":  { "kty": "EC", "crv": "P-256", "x": "…", "y": "…" },
  "salt": "…(base64url, 32 random bytes)",
  "keys": [
    { "kty": "oct", "kid": "UZr4GrLShPUqqCV9lrXkvQ",
      "alg": "ECDH-HKDF-A256GCM-v2",
      "iv": "…(base64url, 12 bytes)",
      "k":  "…(base64url, 16-byte key ciphertext || 16-byte GCM tag)" }
  ],
  "notice": "deprecated"
}
```

`notice` is optional. `"deprecated"` means this app version is still served
but is due for an update; the player logs it. A KID the user may not play,
or that does not exist, is left out of `keys`. The player then fails that
track.

Errors (JSON `{"error": code, "detail": text}`):

| Status | `error` | When | Player reports |
|---|---|---|---|
| 400 | `bad_request` | malformed body, `v` not 2, bad `epk`, KID count | `http 400` |
| 401 | `unknown_client` | no `proof`, unknown `sid`, or no secret matches the proof | "client not recognised: wrong client secret or secret id" |
| 403 | `client_revoked` | the matching secret is revoked | "this app version's client secret is revoked; the app needs an update" |

The player surfaces each of these as `Error { LicenseResolver }`, the same as
any other failure to get a key. A host can match the 403 text to show
"please update the app".

## Cryptography (both sides must match byte for byte)

1. **Key agreement.** Both sides hold an ephemeral ECDH key pair on P-256
   (`nistP256`). The client's private key is non-extractable (WebCrypto); the
   server makes a fresh pair per response.
2. **Shared secret** = the raw ECDH x-coordinate (32 bytes). In .NET that is
   `DeriveRawSecretAgreement`; in WebCrypto, `deriveBits({name:"ECDH"}, priv, 256)`.
3. **Proof message** = `"rustplayer-clearkey-proof-v2"` (ASCII) ‖ `0x00` ‖
   epk x (32) ‖ epk y (32) ‖ each raw KID (16 each, request order). Every
   field has a fixed size, so the concatenation is unambiguous.
   **Proof** = `HMAC-SHA256(key = client secret, proof message)`, 32 bytes.
4. **Wrapping key** = `HKDF-SHA256(ikm = shared secret ‖ client secret,
   salt = response salt, info = "rustplayer-clearkey-wrap-v2", length = 32)`.
5. **Each key** = `AES-256-GCM(wrappingKey, iv = 12 random bytes, aad = raw
   KID bytes, plaintext = 16-byte content key)`, and `k = ciphertext || tag`
   (WebCrypto's GCM layout, 16-byte tag).

The proof binds the request to its `epk` and KIDs. A captured request can only
be replayed as itself, and the answer is wrapped for an `epk` whose private
key only the original client holds. That is why the proof needs no timestamp
or nonce, which also spares TVs with a wrong clock.

### Known-answer vectors

These are checked by `wrapped_licence::tests::known_answer_vectors` (Rust)
and by `scripts/wrapped_licence_mock.py --self-test` (python `cryptography`).
A backend should reproduce them in a unit test.

| Input | Value |
|---|---|
| client secret | bytes `00 01 02 … 1f` (32 bytes) |
| epk x / epk y | `11` × 32 / `22` × 32 |
| kids | one KID, `33` × 16 |
| **proof** | `e387a32563d9f7c997fd213c9852ba906da5f7e5ec215fe95ec49c461716f6a2` |
| ECDH shared secret / salt | `44` × 32 / `55` × 32 |
| **wrapping key** | `b5303419aaaa933b7a1816889f946502febc8c0b139177da79defe606954386b` |

### WebCrypto client sketch

The web shell implements this in Rust/web-sys:

```js
const kp = await crypto.subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, false, ["deriveBits"]);
const epk = await crypto.subtle.exportKey("jwk", kp.publicKey);          // {kty:"EC",crv:"P-256",x,y,...}
const hmacKey = await crypto.subtle.importKey("raw", clientSecret, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
const proof = await crypto.subtle.sign("HMAC", hmacKey, proofMessage(epk, kids));
// … POST { v: 2, kids, epk: {kty, crv, x, y}, proof, sid? } → response …
const serverPub = await crypto.subtle.importKey("jwk", response.epk, { name: "ECDH", namedCurve: "P-256" }, false, []);
const shared = await crypto.subtle.deriveBits({ name: "ECDH", public: serverPub }, kp.privateKey, 256);
const hkdf = await crypto.subtle.importKey("raw", concat(shared, clientSecret), "HKDF", false, ["deriveKey"]);
const wrapKey = await crypto.subtle.deriveKey(
  { name: "HKDF", hash: "SHA-256", salt, info: new TextEncoder().encode("rustplayer-clearkey-wrap-v2") },
  hkdf, { name: "AES-GCM", length: 256 }, false, ["unwrapKey"]);
const contentKey = await crypto.subtle.unwrapKey("raw", k, wrapKey,
  { name: "AES-GCM", iv, additionalData: kidBytes }, { name: "AES-CTR", length: 128 }, false, ["decrypt"]);
// contentKey is non-extractable: usable for AES-CTR decrypt, never readable.
```

## Client secrets: scenarios A and B

Every app build (per platform and version) gets its own random 32-byte
secret. The backend keeps all of them in one table:

| Column | Example | Notes |
|---|---|---|
| `Id` | `android-2.4.0` | Not secret. Sent as `sid` in scenario A. |
| `Secret` | 32 random bytes | Encrypted at rest (Data Protection / Key Vault), never logged. |
| `Status` | `Active` / `Deprecated` / `Revoked` | See the lifecycle below. |
| `Platform`, `CreatedAt`, `LastSeenAt` | | For operations and audit. |

The client always sends `proof`. How the server finds the secret:

- **Scenario A (`sid` present).** Look the row up by id and compare one HMAC
  in constant time. O(1). The id reveals which app version is asking, which
  is harmless and useful in the logs.
- **Scenario B (no `sid`).** Compute the HMAC under **every** row (all
  statuses) and compare each in constant time, without stopping at the first
  match, so the timing does not reveal which row matched. The request
  carries nothing that identifies the app version.
  - Cost: one HMAC-SHA256 over ~140 bytes is ~1 µs, so 160 rows are ~0.2 ms
    of CPU per licence request. That is noise next to the ECDH.
  - It stays cheap into the thousands. Should the table ever grow far past
    that, filter by `Platform` (an optional, non-secret hint), or move to
    scenario A.
  - Keep the rows in memory, refreshed every few minutes, rather than
    reading the database per request.

Both scenarios run against the same table and can be live at once: an app
build chooses one by setting or omitting the secret id. Authenticate the user
(bearer token) and rate-limit **before** the scan, so an anonymous caller
cannot make the server do 160 HMACs per request for free.

### Lifecycle

| Status | Served | Use |
|---|---|---|
| `Active` | yes | The current releases. |
| `Deprecated` | yes, with `"notice": "deprecated"` | Old releases being phased out; the app can nag about an update. |
| `Revoked` | no, 403 `client_revoked` | A blocked or leaked version. The app gets a clear "update" error. |
| (row deleted) | no, 401 `unknown_client` | Same effect, without the clear message. Prefer `Revoked` for a while, then delete. |

Rotation per release (CI):

1. The release job generates the secret:
   `py -3.12 scripts/wrapped_licence_mock.py --gen-secret`, or
   `openssl rand -base64 32 | tr '+/' '-_' | tr -d '='`.
2. It registers the row with the backend (an admin API or a migration) as
   `Active`, with the id `<platform>-<version>`.
3. It injects the secret into the build as a CI secret, never into the
   repository:
   - Android: a `BuildConfig` field from an environment variable, with R8 on.
   - iOS: a generated Swift file kept out of git.
   - Web: the bundle's config.
   Splitting it (for example XOR with a second constant) keeps it out of a
   plain `strings` dump. That is cheap obfuscation, not protection.
4. When a version drops out of support, mark it `Deprecated`, later
   `Revoked`, and finally delete it.

A leaked secret affects one version only: revoke it and ship a new build.

## Server (C#, .NET 10, Newtonsoft.Json): reference implementation

The layering mirrors the existing `Get` → `_licenceService.GetLicence(request.Kids)`:

- the controller decodes the JSON/base64url DTOs into internal byte models
  and encodes the result back;
- the service holds the domain (KID ↔ Guid, `Licences` lookup, client
  secrets, wrapping) over bytes only;
- `ClearKeyWrap` is pure cryptography.

### Internal models (`Services/Models`, no JSON, no base64)

```csharp
public sealed record EcPublicKey(byte[] X, byte[] Y);
public sealed record WrappedKey(byte[] Kid, byte[] Iv, byte[] Wrapped);
public sealed record WrappedLicence(EcPublicKey ServerKey, byte[] Salt, WrappedKey[] Keys, string? Notice);

public enum ClientSecretStatus { Active, Deprecated, Revoked }
public sealed record ClientSecret(string Id, byte[] Secret, ClientSecretStatus Status);

/// 401 / 403 from the licence flow; the controller maps them.
public sealed class LicenceClientException(int status, string code, string detail) : Exception(detail)
{
    public int Status { get; } = status;
    public string Code { get; } = code;
}
```

### API DTOs (`Models/Licence`)

```csharp
public class EcPublicJwk
{
    [JsonProperty("kty")] public string Type { get; set; } = "EC";
    [JsonProperty("crv")] public string Curve { get; set; } = "P-256";
    [JsonProperty("x")] public string X { get; set; } = "";
    [JsonProperty("y")] public string Y { get; set; } = "";
}

public class WrappedLicenceRequest
{
    [JsonProperty("v")] public int Version { get; set; }
    [JsonProperty("kids")] public string[] Kids { get; set; } = Array.Empty<string>();
    [JsonProperty("epk")] public EcPublicJwk Epk { get; set; } = new();
    [JsonProperty("proof")] public string Proof { get; set; } = "";
    [JsonProperty("sid")] public string? SecretId { get; set; }
}

/// The existing ClearKey JWK plus the wrapping fields.
public class WrappedContentKey : ContentKey
{
    [JsonProperty("alg")] public string Alg { get; } = ClearKeyWrap.Alg;
    [JsonProperty("iv")] public string IvAsBase64Url { get; set; } = "";
}

public class WrappedLicenceResponse
{
    [JsonProperty("epk")] public EcPublicJwk Epk { get; set; } = new();
    [JsonProperty("salt")] public string SaltAsBase64Url { get; set; } = "";
    [JsonProperty("keys")] public WrappedContentKey[] Keys { get; set; } = Array.Empty<WrappedContentKey>();
    [JsonProperty("notice", NullValueHandling = NullValueHandling.Ignore)] public string? Notice { get; set; }
}
```

### Shared helpers

`Base64Url` moves out of the service, because the controller needs it now.
The Guid endianness helpers stay in the service: they are the KID ↔ database
domain.

```csharp
public static class Base64Url
{
    public static string Encode(byte[] bytes) =>
        Convert.ToBase64String(bytes).Replace('/', '_').Replace('+', '-').TrimEnd('=');

    public static byte[] Decode(string base64url)
    {
        ArgumentNullException.ThrowIfNull(base64url);
        var padding = new string('=', (4 - base64url.Length % 4) % 4);
        return Convert.FromBase64String(base64url.Replace('_', '/').Replace('-', '+') + padding);
    }
}
```

The original `Base64UrlToByteArray` pads with `4 - len % 4` characters, which
adds four `=` when the length is already a multiple of four. `Convert`
tolerates that today; the `% 4` above makes it exact.

### `ClearKeyWrap` (`Services/Crypto`, pure functions over bytes)

```csharp
using System.Security.Cryptography;
using System.Text;

public static class ClearKeyWrap
{
    public const string Alg = "ECDH-HKDF-A256GCM-v2";
    // Must match the client byte-for-byte.
    private static readonly byte[] KdfInfo = Encoding.ASCII.GetBytes("rustplayer-clearkey-wrap-v2");
    private static readonly byte[] ProofLabel = Encoding.ASCII.GetBytes("rustplayer-clearkey-proof-v2");

    public static ECDiffieHellman ImportPublicKey(EcPublicKey key)
    {
        if (key.X.Length != 32 || key.Y.Length != 32)
            throw new ArgumentException("P-256 coordinates must be 32 bytes");
        var p = new ECParameters { Curve = ECCurve.NamedCurves.nistP256, Q = new ECPoint { X = key.X, Y = key.Y } };
        p.Validate(); // rejects points off the curve
        return ECDiffieHellman.Create(p);
    }

    public static EcPublicKey ExportPublicKey(ECDiffieHellman key)
    {
        var q = key.ExportParameters(false).Q;
        return new EcPublicKey(q.X!, q.Y!);
    }

    /// label ‖ 0x00 ‖ epk x ‖ epk y ‖ each kid.
    public static byte[] ProofMessage(byte[][] kids, EcPublicKey clientKey)
    {
        var m = new byte[ProofLabel.Length + 1 + 64 + 16 * kids.Length];
        var o = 0;
        ProofLabel.CopyTo(m, o); o += ProofLabel.Length;
        m[o++] = 0;
        clientKey.X.CopyTo(m, o); o += 32;
        clientKey.Y.CopyTo(m, o); o += 32;
        foreach (var kid in kids) { kid.CopyTo(m, o); o += 16; }
        return m;
    }

    public static bool ProofMatches(byte[] secret, byte[] message, byte[] proof) =>
        CryptographicOperations.FixedTimeEquals(HMACSHA256.HashData(secret, message), proof);

    /// One wrapping key per response: fresh server ECDH pair + fresh salt,
    /// with the client secret in the HKDF input.
    public static (ECDiffieHellman serverKey, byte[] salt, byte[] wrappingKey) DeriveWrappingKey(
        EcPublicKey clientKey, byte[] clientSecret)
    {
        using var clientPub = ImportPublicKey(clientKey);
        var serverKey = ECDiffieHellman.Create(ECCurve.NamedCurves.nistP256);
        // Raw x-coordinate — what WebCrypto deriveBits(ECDH) yields.
        byte[] shared = serverKey.DeriveRawSecretAgreement(clientPub.PublicKey);
        byte[] ikm = [.. shared, .. clientSecret];
        byte[] salt = RandomNumberGenerator.GetBytes(32);
        byte[] wrappingKey = HKDF.DeriveKey(HashAlgorithmName.SHA256, ikm, 32, salt, KdfInfo);
        CryptographicOperations.ZeroMemory(shared);
        CryptographicOperations.ZeroMemory(ikm);
        return (serverKey, salt, wrappingKey);
    }

    /// wrapped = AES-256-GCM(key, iv, aad = raw kid) ciphertext || 16-byte tag.
    public static (byte[] iv, byte[] wrapped) Wrap(byte[] wrappingKey, byte[] kid, byte[] contentKey)
    {
        var iv = RandomNumberGenerator.GetBytes(12);
        var wrapped = new byte[contentKey.Length + 16];
        using var gcm = new AesGcm(wrappingKey, tagSizeInBytes: 16);
        gcm.Encrypt(iv, contentKey, wrapped.AsSpan(0, contentKey.Length), wrapped.AsSpan(contentKey.Length, 16), kid);
        return (iv, wrapped);
    }
}
```

A unit test should feed the known-answer inputs to `ProofMessage`,
`HMACSHA256.HashData` and `HKDF.DeriveKey` (with `ikm = shared ‖ secret`)
and compare the results with the vectors above.

### `ClientSecretStore` (scenarios A and B)

```csharp
public interface IClientSecretStore
{
    /// The row the proof belongs to; throws LicenceClientException 401 / 403.
    ClientSecret Authenticate(string? secretId, byte[] proofMessage, byte[] proof);
}

/// In-memory snapshot of the ClientSecrets table, refreshed periodically
/// (rows are few and change only on release / revocation).
public sealed class ClientSecretStore(IReadOnlyList<ClientSecret> rows) : IClientSecretStore
{
    public ClientSecret Authenticate(string? secretId, byte[] proofMessage, byte[] proof)
    {
        if (proof.Length != 32)
            throw new LicenceClientException(401, "unknown_client", "missing or malformed proof");

        ClientSecret? found = null;
        if (!string.IsNullOrEmpty(secretId))
        {
            // Scenario A: one lookup, one HMAC.
            var row = rows.FirstOrDefault(r => r.Id == secretId);
            if (row != null && ClearKeyWrap.ProofMatches(row.Secret, proofMessage, proof))
                found = row;
        }
        else
        {
            // Scenario B: every row, constant-time compare, no early exit.
            foreach (var row in rows)
            {
                var match = ClearKeyWrap.ProofMatches(row.Secret, proofMessage, proof);
                if (match && found == null) found = row;
            }
        }

        if (found == null)
            throw new LicenceClientException(401, "unknown_client", "no client secret matches this proof");
        if (found.Status == ClientSecretStatus.Revoked)
            throw new LicenceClientException(403, "client_revoked", $"client {found.Id} is revoked");
        return found;
    }
}
```

### `LicenceService`

```csharp
public interface ILicenceService
{
    Task<ContentKey[]> GetLicence(string[] keys);                       // unchanged
    Task<WrappedLicence> GetWrappedLicence(byte[][] kids, EcPublicKey clientKey, byte[] proof, string? secretId);
    Task SaveLicence(int manifestId, LicenceKeyValue[] keyValuePairs);   // unchanged
}

public class LicenceService : ILicenceService
{
    private const int MaxKidsPerRequest = 16;
    private readonly IUnitOfWork UnitOfWork;
    private readonly IClientSecretStore Secrets;
    public LicenceService(IUnitOfWork unitOfWork, IClientSecretStore secrets) =>
        (UnitOfWork, Secrets) = (unitOfWork, secrets);

    // GetLicence: unchanged.

    public async Task<WrappedLicence> GetWrappedLicence(byte[][] kids, EcPublicKey clientKey, byte[] proof, string? secretId)
    {
        if (kids.Length is 0 or > MaxKidsPerRequest || kids.Any(k => k.Length != 16))
            throw new ArgumentException($"1..{MaxKidsPerRequest} kids of 16 bytes per request");

        // Before any ECDH or database work: an unknown or revoked client costs only HMACs.
        var client = Secrets.Authenticate(secretId, ClearKeyWrap.ProofMessage(kids, clientKey), proof);

        var (serverKey, salt, wrappingKey) = ClearKeyWrap.DeriveWrappingKey(clientKey, client.Secret);
        try
        {
            var keys = new List<WrappedKey>();
            foreach (var kid in kids)
            {
                var licence = await FindLicenceByKid(kid);
                if (licence == null) continue;
                // TODO (entitlement): confirm the authenticated user may play the
                // title this KID belongs to — the existing endpoint's rule applies.
                var contentKey = ContentKeyBytes(licence);
                var (iv, wrapped) = ClearKeyWrap.Wrap(wrappingKey, kid, contentKey);
                CryptographicOperations.ZeroMemory(contentKey);
                keys.Add(new WrappedKey(kid, iv, wrapped));
            }
            var notice = client.Status == ClientSecretStatus.Deprecated ? "deprecated" : null;
            return new WrappedLicence(ClearKeyWrap.ExportPublicKey(serverKey), salt, keys.ToArray(), notice);
        }
        finally
        {
            CryptographicOperations.ZeroMemory(wrappingKey);
            serverKey.Dispose();
        }
    }

    // --- domain: KID bytes ↔ the Guid the Licences table is keyed by ---------

    private Task<Licence?> FindLicenceByKid(byte[] kid)
    {
        var idKey = FromBigEndianByteArray(kid);
        return UnitOfWork.Licences.GetByKeyAsync(idKey.ToString());
    }

    private static byte[] ContentKeyBytes(Licence licence) =>
        ToBigEndianByteArray(Guid.Parse(licence.Value));

    // ToBigEndianByteArray / FromBigEndianByteArray / FlipSerializedGuidEndianness
    // stay exactly as they are today.
}
```

`GetByKeyAsync(idKey.ToString())` receives the same value as before. The old
code went Guid → string → strip dashes → `Guid.Parse` → `ToString()`, which is
the identity on a Guid.

### Controller

```csharp
[HttpPost("wrapped")]
[Produces("application/json")]
public async Task<ActionResult<WrappedLicenceResponse>> GetWrapped([FromBody] WrappedLicenceRequest request)
{
    if (request.Version != 2)
        return BadRequest(new { error = "bad_request", detail = "only protocol v2 is served" });
    if (request.Epk is not { Type: "EC", Curve: "P-256" })
        return BadRequest(new { error = "bad_request", detail = "epk must be an EC P-256 JWK" });

    var clientKey = new EcPublicKey(Base64Url.Decode(request.Epk.X), Base64Url.Decode(request.Epk.Y));
    var kids = request.Kids.Select(Base64Url.Decode).ToArray();
    var proof = string.IsNullOrEmpty(request.Proof) ? Array.Empty<byte>() : Base64Url.Decode(request.Proof);

    WrappedLicence licence;
    try
    {
        licence = await _licenceService.GetWrappedLicence(kids, clientKey, proof, request.SecretId);
    }
    catch (LicenceClientException e)
    {
        _logger.LogWarning("wrapped licence refused: {Code} sid={Sid}", e.Code, request.SecretId);
        return StatusCode(e.Status, new { error = e.Code, detail = e.Message });
    }

    return new WrappedLicenceResponse
    {
        Epk = new EcPublicJwk { X = Base64Url.Encode(licence.ServerKey.X), Y = Base64Url.Encode(licence.ServerKey.Y) },
        SaltAsBase64Url = Base64Url.Encode(licence.Salt),
        Keys = licence.Keys.Select(k => new WrappedContentKey
        {
            IdAsBase64Url = Base64Url.Encode(k.Kid),
            IvAsBase64Url = Base64Url.Encode(k.Iv),
            ValueAsBase64Url = Base64Url.Encode(k.Wrapped),
        }).ToArray(),
        Notice = licence.Notice,
    };
}
```

Put `[Authorize]` on the controller (or the action) and apply the usual rate
limit. An `ArgumentException` from the service maps to 400 through the
existing exception filter, or catch it here and `BadRequest` it.

## Operational hardening that matters more than the wrapping

- Authenticate the licence endpoint with a short-lived token bound to the
  user, session and title, and check entitlement per KID (the `TODO` above).
- Sign and expire segment URLs, or tokenise the CDN, so a key without the
  content (and content without the key) is worthless.
- Use one key per title, and rotate where the packager allows it
  (`default_KID` per period).
- Rate-limit and audit the endpoint. Log the `sid` (or the matched row's id)
  with every licence: an unknown id or a revoked version still in use is the
  first sign of a leaked secret.
- Never log key material or client secrets (the player logs KIDs and the
  secret's length only).

## Player side (implemented on every platform)

The player fetches the wrapped licence **itself**: one POST per KID, at the
moment the init segment reveals the KID. That is roughly at start, plus once
per new KID on a track switch. No host callback is involved; only the host's
`intercept(url, "license")` runs first, which is where an `Authorization`
header goes.

The configuration is the URL, the client secret (base64url, at least 16
bytes) and, for scenario A, the secret id. The player does not contain any
secret: the host app supplies it.

| Where | How |
|---|---|
| Engine (`player` crate) | `player.set_wrapped_licence(url, LicenceClient::new(secret_b64, Some(id))?)` installs a `WrappedLicenceResolver` as the `LicenseResolver`. `set_clearkey` still pre-populates the cache; the resolver only runs on a miss. |
| Bridge (`StartConfig`) | `wrapped_licence_url`, `wrapped_licence_secret`, `wrapped_licence_secret_id` replace the host `resolve_key` hook for that session. Or call `bridge.set_wrapped_licence(url, &secret, id)` right after `start()`. A missing or malformed secret fails every key request with the reason; the player never falls back to `resolve_key`. |
| Web (`RustPlayer.create` options) | `{ wrappedLicence: { url, secret, secretId? } }`; `resolveKey` may then be omitted. A malformed secret rejects `create()`. Demo: `index.html?licence=<endpoint>&secret=<b64url>[&sid=<id>]`. |
| Android (`RustPlayer.kt`) | `player.setWrappedLicence(url, clientSecret = BuildConfig.LICENCE_SECRET, secretId = "android-2.4.0")` right after `start(...)`. Returns false for a malformed secret. Test app: `--es licence_url … --es licence_secret … [--es licence_sid …]`. |
| iOS (`RustPlayer.swift`) | `player.setWrappedLicence(url: url, clientSecret: secret, secretId: "ios-2.4.0")` right after `start(...)`. In C: `rustplayer_player_set_wrapped_licence(handle, url, secret, secret_id_or_NULL)`. |

Leave the secret id out (`null` / `nil` / no `secretId`) for scenario B.

What each platform does with the response:

- **Web.** WebCrypto end to end. The HMAC proof comes from `sign(HMAC)`.
  The chain is `generateKey(ECDH P-256, non-extractable)` → `deriveBits` →
  `importKey(HKDF, shared ‖ secret)` → `deriveKey(AES-GCM-256)` →
  `unwrapKey`, ending in a **non-extractable AES-CTR `CryptoKey`**.
  - The content key never exists as bytes in JS or wasm memory, so it cannot
    be read from the inspector, a heap snapshot or a breakpoint.
  - Every CENC segment of such a track (video and audio) then decrypts
    through `crypto.subtle.decrypt`; the software AES-CTR fallback is skipped
    for it.
  - The client secret itself is in the page's bundle (low-trust, see the
    top of this document).
- **Native (Windows / Linux / macOS / Android / iOS).** `p256` ECDH, `hmac`,
  `hkdf`/`sha2` and `aes-gcm`. The unwrapped 16 bytes live in the process's
  key cache, as a `set_clearkey` key would.
  - They are never on the wire in the clear and never in a log, but a
    debugger attached to the process can still read them.
  - That is the same trust level as the raw ClearKey path. The win is that
    the public codebase no longer implies a key in every network trace, and
    that only builds carrying a live secret get keys at all.
  - The player wipes its own copy of the secret when it drops it (the
    host's strings are the host's business).

Dev/test: `scripts/wrapped_licence_mock.py` is an independent (python
`cryptography`) implementation of the endpoint.

- It serves the test-stream keys with open CORS, and holds a client-secret
  table: built-in **public** dev rows `dev` (active), `dev-deprecated` and
  `dev-revoked`, plus `--secret ID:B64[:STATUS]` / `--secrets FILE`.
- Start it with `py -3.12 scripts/wrapped_licence_mock.py --port 8090`; it
  prints the dev secrets.
- Then open
  `http://localhost:8080/?licence=http://localhost:8090/licence/wrapped&secret=<dev secret>&sid=dev`
  (drop `&sid=` for scenario B, use `sid=dev-revoked` to see the 403).
- The Rust side has self-contained tests (`wrapped_licence::tests`): known
  answers, scenario A/B lookup over a 163-row table, revocation, and the
  round trip.

Failure semantics: each of these surfaces as `Error { LicenseResolver }` for
that track, exactly as a failing `resolve_key` did:

- a non-2xx response (401 and 403 with the readable reasons above);
- a malformed response;
- a KID missing from `keys`;
- a wrong `alg` or length (a v1 server's `ECDH-HKDF-A256GCM` included);
- a GCM tag failure (wrong client secret or a tampered key).
