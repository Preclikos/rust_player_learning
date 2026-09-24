# ClearKey with wrapped keys — licence endpoint contract

ClearKey is not DRM: the client must hold the content key in the clear to
decrypt, and this repository is public. What can be hardened is everything
*around* the key. This endpoint closes the cheapest attack — reading the key
off the wire or out of the page in devtools — and keeps the licence server
from acting as a public key-vending service:

- the content key never travels in the clear; it is wrapped for one client's
  ephemeral ECDH key, so a network capture or a proxy sees ciphertext only;
- on the web the wrapped key is unwrapped straight into a **non-extractable**
  WebCrypto `CryptoKey`, so the plaintext key never exists in JavaScript;
- the endpoint is authenticated and entitlement-checked per KID (the same
  `Licences` lookup as the existing endpoint), so a stolen request does not
  yield another user's keys.

What it does **not** do: stop a modified client from decrypting content it is
entitled to. That needs real DRM (Widevine / PlayReady / FairPlay via EME).

The scheme is JWE ECDH-ES in spirit (RFC 7518 §4.6: ephemeral P-256 key
agreement, key wrapped with AES-GCM, ephemeral public key carried as a JWK
`epk`), simplified to what WebCrypto does natively: HKDF-SHA256 instead of
Concat KDF, one salt per response, the KID as GCM additional data.

## Wire format

`POST /licence/wrapped` (JSON, authenticated like the existing licence call)

```json
{
  "kids": ["UZr4GrLShPUqqCV9lrXkvQ"],
  "epk": { "kty": "EC", "crv": "P-256", "x": "…base64url…", "y": "…base64url…" }
}
```

Response — the existing `keys` JWK array, with `k` wrapped:

```json
{
  "epk":  { "kty": "EC", "crv": "P-256", "x": "…", "y": "…" },
  "salt": "…(base64url, 32 random bytes)",
  "keys": [
    { "kty": "oct", "kid": "UZr4GrLShPUqqCV9lrXkvQ",
      "alg": "ECDH-HKDF-A256GCM",
      "iv": "…(base64url, 12 bytes)",
      "k":  "…(base64url, 16-byte key ciphertext || 16-byte GCM tag)" }
  ]
}
```

`kid` is base64url of the 16 raw KID bytes, exactly as in the existing
endpoint. A client that receives `alg` other than `ECDH-HKDF-A256GCM` must
reject the key.

## Cryptography (both sides must match exactly)

1. Both sides hold an ephemeral ECDH key pair on P-256 (`nistP256`). The
   client's private key is non-extractable (WebCrypto); the server makes a
   fresh pair per response.
2. Shared secret = raw ECDH x-coordinate (32 bytes): .NET
   `DeriveRawSecretAgreement`, WebCrypto `deriveBits({name:"ECDH"}, priv, 256)`.
3. Wrapping key = `HKDF-SHA256(ikm = shared secret, salt = response salt,
   info = "rustplayer-clearkey-wrap-v1", length = 32)`.
4. Each key: `AES-256-GCM(wrappingKey, iv = 12 random bytes, aad = raw KID
   bytes, plaintext = 16-byte content key)`; `k = ciphertext || tag`
   (WebCrypto's GCM layout, 16-byte tag).

WebCrypto client sketch (the web shell implements this in Rust/web-sys):

```js
const kp = await crypto.subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, false, ["deriveBits"]);
const epk = await crypto.subtle.exportKey("jwk", kp.publicKey);          // {kty:"EC",crv:"P-256",x,y,...}
// … POST { kids, epk: {kty, crv, x, y} } → response …
const serverPub = await crypto.subtle.importKey("jwk", response.epk, { name: "ECDH", namedCurve: "P-256" }, false, []);
const secret = await crypto.subtle.deriveBits({ name: "ECDH", public: serverPub }, kp.privateKey, 256);
const hkdf = await crypto.subtle.importKey("raw", secret, "HKDF", false, ["deriveKey"]);
const wrapKey = await crypto.subtle.deriveKey(
  { name: "HKDF", hash: "SHA-256", salt, info: new TextEncoder().encode("rustplayer-clearkey-wrap-v1") },
  hkdf, { name: "AES-GCM", length: 256 }, false, ["unwrapKey"]);
const contentKey = await crypto.subtle.unwrapKey("raw", k, wrapKey,
  { name: "AES-GCM", iv, additionalData: kidBytes }, { name: "AES-CTR", length: 128 }, false, ["decrypt"]);
// contentKey is non-extractable: usable for AES-CTR decrypt, never readable.
```

## Server (C#, .NET 10, Newtonsoft.Json) — reference implementation

Layering mirrors the existing `Get` → `_licenceService.GetLicence(request.Kids)`:
the controller decodes the JSON/base64url DTOs into internal byte models and
encodes the result back; the service holds the domain (KID ↔ Guid, `Licences`
lookup, wrapping) over bytes only; `ClearKeyWrap` is pure cryptography.

### Internal models (`Services/Models`, no JSON, no base64)

```csharp
public sealed record EcPublicKey(byte[] X, byte[] Y);
public sealed record WrappedKey(byte[] Kid, byte[] Iv, byte[] Wrapped);
public sealed record WrappedLicence(EcPublicKey ServerKey, byte[] Salt, WrappedKey[] Keys);
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
    [JsonProperty("kids")] public string[] Kids { get; set; } = Array.Empty<string>();
    [JsonProperty("epk")] public EcPublicJwk Epk { get; set; } = new();
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
}
```

### Shared helpers

`Base64Url` moves out of the service (the controller needs it now); the Guid
endianness helpers stay in the service — they are the KID ↔ database domain.

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

(The original `Base64UrlToByteArray` pads with `4 - len % 4` characters, which
adds four `=` when the length is already a multiple of four; `Convert`
tolerates it today, the `% 4` above makes it exact.)

### `ClearKeyWrap` (`Services/Crypto`, pure functions over bytes)

```csharp
using System.Security.Cryptography;
using System.Text;

public static class ClearKeyWrap
{
    public const string Alg = "ECDH-HKDF-A256GCM";
    // Must match the client byte-for-byte.
    private static readonly byte[] HkdfInfo = Encoding.ASCII.GetBytes("rustplayer-clearkey-wrap-v1");

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

    /// One wrapping key per response: fresh server ECDH pair + fresh salt.
    public static (ECDiffieHellman serverKey, byte[] salt, byte[] wrappingKey) DeriveWrappingKey(EcPublicKey clientKey)
    {
        using var clientPub = ImportPublicKey(clientKey);
        var serverKey = ECDiffieHellman.Create(ECCurve.NamedCurves.nistP256);
        // Raw x-coordinate — what WebCrypto deriveBits(ECDH) yields.
        byte[] shared = serverKey.DeriveRawSecretAgreement(clientPub.PublicKey);
        byte[] salt = RandomNumberGenerator.GetBytes(32);
        byte[] wrappingKey = HKDF.DeriveKey(HashAlgorithmName.SHA256, shared, 32, salt, HkdfInfo);
        CryptographicOperations.ZeroMemory(shared);
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

### `LicenceService`

```csharp
public interface ILicenceService
{
    Task<ContentKey[]> GetLicence(string[] keys);                       // unchanged
    Task<WrappedLicence> GetWrappedLicence(byte[][] kids, EcPublicKey clientKey);
    Task SaveLicence(int manifestId, LicenceKeyValue[] keyValuePairs);   // unchanged
}

public class LicenceService : ILicenceService
{
    private const int MaxKidsPerRequest = 16;
    private readonly IUnitOfWork UnitOfWork;
    public LicenceService(IUnitOfWork unitOfWork) => UnitOfWork = unitOfWork;

    public async Task<ContentKey[]> GetLicence(string[] keys)
    {
        var contentKeys = new List<ContentKey>();
        foreach (var baseKeyId in keys)
        {
            var licence = await FindLicenceByKid(Base64Url.Decode(baseKeyId));
            if (licence == null) continue;
            contentKeys.Add(new ContentKey
            {
                IdAsBase64Url = baseKeyId,
                ValueAsBase64Url = Base64Url.Encode(ContentKeyBytes(licence)),
            });
        }
        return contentKeys.ToArray();
    }

    public async Task<WrappedLicence> GetWrappedLicence(byte[][] kids, EcPublicKey clientKey)
    {
        if (kids.Length is 0 or > MaxKidsPerRequest)
            throw new ArgumentException($"1..{MaxKidsPerRequest} kids per request");
        var (serverKey, salt, wrappingKey) = ClearKeyWrap.DeriveWrappingKey(clientKey);
        try
        {
            var keys = new List<WrappedKey>();
            foreach (var kid in kids)
            {
                if (kid.Length != 16) continue;
                var licence = await FindLicenceByKid(kid);
                if (licence == null) continue;
                // TODO (entitlement): confirm the authenticated user may play the
                // title this KID belongs to — the existing endpoint's rule applies.
                var contentKey = ContentKeyBytes(licence);
                var (iv, wrapped) = ClearKeyWrap.Wrap(wrappingKey, kid, contentKey);
                CryptographicOperations.ZeroMemory(contentKey);
                keys.Add(new WrappedKey(kid, iv, wrapped));
            }
            return new WrappedLicence(ClearKeyWrap.ExportPublicKey(serverKey), salt, keys.ToArray());
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

`GetByKeyAsync(idKey.ToString())` receives the same value as before: the old
code went Guid → string → strip dashes → `Guid.Parse` → `ToString()`, which is
the identity on a Guid.

### Controller

```csharp
[HttpPost]
[Produces("application/json")]
public async Task<LicenseResponse> Get([FromBody] LicenceRequest request) =>
    new LicenseResponse { SessionType = request.Type, Keys = await _licenceService.GetLicence(request.Kids) };

[HttpPost("wrapped")]
[Produces("application/json")]
public async Task<ActionResult<WrappedLicenceResponse>> GetWrapped([FromBody] WrappedLicenceRequest request)
{
    if (request.Epk is not { Type: "EC", Curve: "P-256" })
        return BadRequest("epk must be an EC P-256 JWK");

    var clientKey = new EcPublicKey(Base64Url.Decode(request.Epk.X), Base64Url.Decode(request.Epk.Y));
    var kids = request.Kids.Select(Base64Url.Decode).ToArray();

    var licence = await _licenceService.GetWrappedLicence(kids, clientKey);

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
    };
}
```

`[Authorize]` on the controller (or the action) and the usual rate limit; an
`ArgumentException` from the service maps to 400 through the existing
exception filter, or catch it here and `BadRequest` it.

## Operational hardening that matters more than the wrapping

- Authenticate the licence endpoint with a short-lived token bound to the
  user, session and title; check entitlement per KID (the `TODO` above).
- Signed / expiring segment URLs or CDN tokenisation, so a key without the
  content — and content without the key — is worthless.
- One key per title, rotation where the packager allows it
  (`default_KID` per period); rate limiting and audit on the endpoint.
- Never log key material (the player logs KIDs only — checked).

## Player side (next step)

Web shell: `RustPlayer.create(..., { licenceUrl, authToken })` (or a host
`resolveWrappedKeys(kids)` hook returning the response JSON) generates the
ECDH pair, calls the endpoint, derives the wrapping key and `unwrapKey`s each
content key into a non-extractable AES-CTR `CryptoKey`; the CENC path then
always decrypts through WebCrypto (the software AES fallback needs raw key
bytes and is skipped for wrapped keys). Native shells keep raw keys for now;
the same protocol can follow with `p256` + `hkdf` + `aes-gcm` crates.
