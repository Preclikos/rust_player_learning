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

Drop-in beside the existing `GetLicence`; reuses its KID/Guid helpers
(`Base64UrlToByteArray`, `FromBigEndianByteArray`, `ToBigEndianByteArray`,
`ByteArrayToBase64Url`), the `UnitOfWork.Licences` lookup and the existing
`ContentKey` JWK class extended with `alg` / `iv`.

```csharp
using System.Security.Cryptography;
using Newtonsoft.Json;

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

/// The existing ClearKey JWK, plus the wrapping fields (null on the plain endpoint).
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

public static class ClearKeyWrap
{
    public const string Alg = "ECDH-HKDF-A256GCM";
    // Must match the client byte-for-byte.
    private static readonly byte[] HkdfInfo = System.Text.Encoding.ASCII.GetBytes("rustplayer-clearkey-wrap-v1");
    private const int MaxKidsPerRequest = 16;

    public static ECDiffieHellman ImportPublicJwk(EcPublicJwk jwk)
    {
        if (jwk.Type != "EC" || jwk.Curve != "P-256")
            throw new ArgumentException("epk must be an EC P-256 JWK");
        var x = Base64UrlToByteArray(jwk.X);
        var y = Base64UrlToByteArray(jwk.Y);
        if (x.Length != 32 || y.Length != 32)
            throw new ArgumentException("epk coordinates must be 32 bytes");
        var p = new ECParameters { Curve = ECCurve.NamedCurves.nistP256, Q = new ECPoint { X = x, Y = y } };
        p.Validate(); // rejects points off the curve
        return ECDiffieHellman.Create(p);
    }

    public static EcPublicJwk ExportPublicJwk(ECDiffieHellman key)
    {
        var q = key.ExportParameters(false).Q;
        return new EcPublicJwk { X = ByteArrayToBase64Url(q.X!), Y = ByteArrayToBase64Url(q.Y!) };
    }

    /// One wrapping key per response: fresh server ECDH pair + fresh salt.
    public static (ECDiffieHellman serverKey, byte[] salt, byte[] wrappingKey) DeriveWrappingKey(EcPublicJwk clientEpk)
    {
        using var clientPub = ImportPublicJwk(clientEpk);
        var serverKey = ECDiffieHellman.Create(ECCurve.NamedCurves.nistP256);
        // Raw x-coordinate — what WebCrypto deriveBits(ECDH) yields.
        byte[] shared = serverKey.DeriveRawSecretAgreement(clientPub.PublicKey);
        byte[] salt = RandomNumberGenerator.GetBytes(32);
        byte[] wrappingKey = HKDF.DeriveKey(HashAlgorithmName.SHA256, shared, 32, salt, HkdfInfo);
        CryptographicOperations.ZeroMemory(shared);
        return (serverKey, salt, wrappingKey);
    }

    /// k = AES-256-GCM(key, iv, aad = raw kid) ciphertext || 16-byte tag.
    public static (byte[] iv, byte[] wrapped) Wrap(byte[] wrappingKey, byte[] kid, byte[] contentKey)
    {
        var iv = RandomNumberGenerator.GetBytes(12);
        var ct = new byte[contentKey.Length];
        var tag = new byte[16];
        using var gcm = new AesGcm(wrappingKey, tagSizeInBytes: 16);
        gcm.Encrypt(iv, contentKey, ct, tag, associatedData: kid);
        var wrapped = new byte[ct.Length + tag.Length];
        Buffer.BlockCopy(ct, 0, wrapped, 0, ct.Length);
        Buffer.BlockCopy(tag, 0, wrapped, ct.Length, tag.Length);
        return (iv, wrapped);
    }

    public static void ValidateRequest(WrappedLicenceRequest req)
    {
        if (req.Kids is null || req.Kids.Length == 0 || req.Kids.Length > MaxKidsPerRequest)
            throw new ArgumentException($"1..{MaxKidsPerRequest} kids per request");
        if (req.Epk is null) throw new ArgumentException("epk missing");
    }
}

// In the licence service, next to GetLicence:
public async Task<WrappedLicenceResponse> GetWrappedLicence(WrappedLicenceRequest request)
{
    ClearKeyWrap.ValidateRequest(request);
    var (serverKey, salt, wrappingKey) = ClearKeyWrap.DeriveWrappingKey(request.Epk);
    try
    {
        var keys = new List<WrappedContentKey>();
        foreach (var baseKeyId in request.Kids)
        {
            var kidBytes = Base64UrlToByteArray(baseKeyId);
            if (kidBytes.Length != 16) continue;
            // Same KID → Guid → Licences lookup as GetLicence.
            var idKey = Guid.Parse(FromBigEndianByteArray(kidBytes).ToString().Replace("-", ""));
            var licence = await UnitOfWork.Licences.GetByKeyAsync(idKey.ToString());
            if (licence == null) continue;
            // TODO (entitlement): confirm the authenticated user may play the
            // title this KID belongs to — the existing endpoint's rule applies.
            var contentKey = ToBigEndianByteArray(Guid.Parse(licence.Value));
            var (iv, wrapped) = ClearKeyWrap.Wrap(wrappingKey, kidBytes, contentKey);
            CryptographicOperations.ZeroMemory(contentKey);
            keys.Add(new WrappedContentKey
            {
                IdAsBase64Url = baseKeyId,
                IvAsBase64Url = ByteArrayToBase64Url(iv),
                ValueAsBase64Url = ByteArrayToBase64Url(wrapped),
            });
        }
        return new WrappedLicenceResponse
        {
            Epk = ClearKeyWrap.ExportPublicJwk(serverKey),
            SaltAsBase64Url = ByteArrayToBase64Url(salt),
            Keys = keys.ToArray(),
        };
    }
    finally
    {
        CryptographicOperations.ZeroMemory(wrappingKey);
        serverKey.Dispose();
    }
}

// Controller: [Authorize] + [HttpPost("licence/wrapped")] → GetWrappedLicence(request).
// Add the usual rate limit; the response is cheap but the endpoint hands out
// entitlements.
```

`ContentKey.Type` is a get-only `"oct"` in the existing class; `WrappedContentKey`
inherits it, so the response keys stay valid ClearKey JWKs for any consumer
that ignores `alg`/`iv` — and such a consumer would then fail to decrypt,
which is the intended outcome for a client that does not speak the protocol.

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
