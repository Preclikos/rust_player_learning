//! Wrapped ClearKey licences — the client half of
//! `docs/CLEARKEY_WRAPPED_LICENCE.md`.
//!
//! The licence server never sends a content key in the clear. The client
//! makes an ephemeral ECDH P-256 key, posts `{kids, epk}` (the JWK public
//! key), and gets each key back as `AES-256-GCM(HKDF-SHA256(ECDH shared
//! secret, salt, info), iv, aad = kid)`. This module speaks that protocol
//! for every platform:
//!
//! - native: `p256` + `hkdf` + `aes-gcm` unwrap the key into the ClearKey
//!   decryptor's cache — the transport is protected, the key lives in
//!   process memory as any ClearKey key must;
//! - browser: WebCrypto derives and unwraps straight into a
//!   **non-extractable** `CryptoKey` (`crypto_web`), so the plaintext key
//!   never exists in JavaScript or on the wire; the CENC path then decrypts
//!   through WebCrypto exclusively.
//!
//! The HTTP call goes through the engine's `HttpClient` with
//! `RequestKind::License`, so the host's `RequestInterceptor` adds its
//! authorisation headers exactly as for every other request. Install with
//! `Player::set_wrapped_licence(url, hkdf_info)`.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use serde::Deserialize;

use crate::net::{BoxError, HttpClient, LicenseResolver, RequestKind};

/// HKDF `info` both sides use unless the deployment configures its own.
pub const DEFAULT_HKDF_INFO: &str = "rustplayer-clearkey-wrap-v1";
/// The only wrapping algorithm this client accepts.
pub const ALG: &str = "ECDH-HKDF-A256GCM";

// ---------------------------------------------------------------------------
// base64url (RFC 4648 §5, no padding) — what JWK and the endpoint use
// ---------------------------------------------------------------------------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(B64[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(B64[n as usize & 63] as char);
        }
    }
    out
}

pub fn base64url_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim().trim_end_matches('=');
    let val = |c: u8| -> Result<u32, String> {
        Ok(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return Err(format!("base64url: invalid character {:?}", c as char)),
        } as u32)
    };
    let bytes = s.as_bytes();
    if bytes.len() % 4 == 1 {
        return Err("base64url: invalid length".into());
    }
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct EpkJwk {
    kty: String,
    crv: String,
    x: String,
    y: String,
}

#[derive(Deserialize)]
struct KeyJwk {
    kid: String,
    k: String,
    iv: Option<String>,
    alg: Option<String>,
}

#[derive(Deserialize)]
struct ResponseJson {
    epk: EpkJwk,
    salt: String,
    keys: Vec<KeyJwk>,
}

/// One wrapped content key as the server returned it (decoded bytes).
pub struct WrappedKey {
    pub kid: [u8; 16],
    /// AES-GCM nonce, 12 bytes.
    pub iv: Vec<u8>,
    /// Ciphertext (16 bytes) || GCM tag (16 bytes).
    pub wrapped: Vec<u8>,
}

/// A decoded licence response.
pub struct WrappedLicence {
    /// Server ephemeral P-256 public key coordinates, 32 bytes each.
    pub server_x: Vec<u8>,
    pub server_y: Vec<u8>,
    pub salt: Vec<u8>,
    pub keys: Vec<WrappedKey>,
}

/// The request body for `kids` with the client's ephemeral public key.
pub fn request_json(kids: &[[u8; 16]], epk_x: &[u8], epk_y: &[u8]) -> String {
    let kids: Vec<String> = kids.iter().map(|k| base64url_encode(k)).collect();
    serde_json::json!({
        "kids": kids,
        "epk": { "kty": "EC", "crv": "P-256", "x": base64url_encode(epk_x), "y": base64url_encode(epk_y) }
    })
    .to_string()
}

/// Parse and validate a licence response. Keys with an unexpected `alg` or
/// malformed fields are rejected, not skipped: a client that misreads a key
/// would decrypt garbage.
pub fn parse_response(json: &str) -> Result<WrappedLicence, String> {
    let r: ResponseJson = serde_json::from_str(json).map_err(|e| format!("licence response: {e}"))?;
    if r.epk.kty != "EC" || r.epk.crv != "P-256" {
        return Err(format!("licence response: epk is {}/{}, expected EC/P-256", r.epk.kty, r.epk.crv));
    }
    let server_x = base64url_decode(&r.epk.x)?;
    let server_y = base64url_decode(&r.epk.y)?;
    if server_x.len() != 32 || server_y.len() != 32 {
        return Err("licence response: epk coordinates must be 32 bytes".into());
    }
    let salt = base64url_decode(&r.salt)?;
    if salt.len() < 16 {
        return Err("licence response: salt shorter than 16 bytes".into());
    }
    let mut keys = Vec::with_capacity(r.keys.len());
    for k in r.keys {
        match k.alg.as_deref() {
            Some(ALG) => {}
            other => return Err(format!("licence response: key alg {other:?}, expected {ALG:?}")),
        }
        let kid: [u8; 16] = base64url_decode(&k.kid)?
            .try_into()
            .map_err(|_| "licence response: kid must be 16 bytes".to_string())?;
        let iv = base64url_decode(k.iv.as_deref().ok_or("licence response: key without iv")?)?;
        if iv.len() != 12 {
            return Err("licence response: iv must be 12 bytes".into());
        }
        let wrapped = base64url_decode(&k.k)?;
        if wrapped.len() != 32 {
            return Err("licence response: k must be 16-byte key + 16-byte tag".into());
        }
        keys.push(WrappedKey { kid, iv, wrapped });
    }
    Ok(WrappedLicence {
        server_x,
        server_y,
        salt,
        keys,
    })
}

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

/// [`LicenseResolver`] that fetches wrapped keys from `url` and unwraps them
/// with a per-request ephemeral ECDH key. One POST per KID (video and audio
/// resolve separately when their init segments are parsed).
pub struct WrappedLicenceResolver {
    url: String,
    info: Vec<u8>,
    http: Arc<HttpClient>,
}

impl WrappedLicenceResolver {
    pub fn new(url: String, hkdf_info: Option<String>, http: Arc<HttpClient>) -> Self {
        Self {
            url,
            info: hkdf_info.unwrap_or_else(|| DEFAULT_HKDF_INFO.to_string()).into_bytes(),
            http,
        }
    }

    async fn fetch(&self, kids: &[[u8; 16]], epk_x: &[u8], epk_y: &[u8]) -> Result<WrappedLicence, BoxError> {
        let body = request_json(kids, epk_x, epk_y);
        let bytes = self
            .http
            .post(self.url.clone(), RequestKind::License, Bytes::from(body), "application/json")
            .await?;
        let text = std::str::from_utf8(&bytes).map_err(|e| -> BoxError { format!("licence response: {e}").into() })?;
        Ok(parse_response(text)?)
    }
}

#[async_trait]
impl LicenseResolver for WrappedLicenceResolver {
    async fn resolve(&self, kid: [u8; 16]) -> Result<[u8; 16], BoxError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            native::resolve(self, kid).await
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = kid;
            Err("wrapped licence keys are platform-held in the browser (see resolve_platform_key)".into())
        }
    }

    async fn resolve_platform_key(&self, kid: [u8; 16]) -> Result<bool, BoxError> {
        #[cfg(target_arch = "wasm32")]
        {
            // The boxed local future must be 'static: hand it an owned copy
            // of this resolver (the client is an Arc).
            let me = WrappedLicenceResolver {
                url: self.url.clone(),
                info: self.info.clone(),
                http: Arc::clone(&self.http),
            };
            web::SendFut(Box::pin(async move { web::resolve(&me, kid).await })).await?;
            Ok(true)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = kid;
            Ok(false)
        }
    }
}

fn find_key<'a>(lic: &'a WrappedLicence, kid: &[u8; 16]) -> Result<&'a WrappedKey, BoxError> {
    lic.keys
        .iter()
        .find(|k| &k.kid == kid)
        .ok_or_else(|| -> BoxError { "licence response has no key for the requested KID".into() })
}

// ---------------------------------------------------------------------------
// Native: RustCrypto
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use aes_gcm::aead::{Aead, Payload};
    use aes_gcm::{Aes256Gcm, KeyInit};
    use hkdf::Hkdf;
    use p256::elliptic_curve::sec1::ToSec1Point;
    use sha2::Sha256;

    /// A fresh P-256 secret; `from_slice` rejects the (negligibly rare)
    /// out-of-range scalars, so draw again.
    fn ephemeral_secret() -> p256::SecretKey {
        loop {
            let bytes: [u8; 32] = rand::random();
            if let Ok(k) = p256::SecretKey::from_slice(&bytes) {
                return k;
            }
        }
    }

    /// Wrapping key for `shared` — HKDF-SHA256 as the server does it.
    pub(super) fn wrapping_key(shared: &[u8], salt: &[u8], info: &[u8]) -> Result<[u8; 32], BoxError> {
        let hk = Hkdf::<Sha256>::new(Some(salt), shared);
        let mut out = [0u8; 32];
        hk.expand(info, &mut out)
            .map_err(|e| -> BoxError { format!("HKDF expand: {e}").into() })?;
        Ok(out)
    }

    /// AES-256-GCM unwrap with the KID as additional data.
    pub(super) fn unwrap_key(wrapping_key: &[u8; 32], entry: &WrappedKey) -> Result<[u8; 16], BoxError> {
        let cipher = Aes256Gcm::new_from_slice(wrapping_key)
            .map_err(|e| -> BoxError { format!("AES-GCM key: {e}").into() })?;
        let nonce = aes_gcm::Nonce::try_from(&entry.iv[..])
            .map_err(|_| -> BoxError { "AES-GCM nonce must be 12 bytes".into() })?;
        let plain = cipher
            .decrypt(&nonce, Payload { msg: &entry.wrapped, aad: &entry.kid })
            .map_err(|_| -> BoxError { "wrapped key failed authentication (wrong key, KID or tampered)".into() })?;
        plain
            .try_into()
            .map_err(|_| -> BoxError { "unwrapped key is not 16 bytes".into() })
    }

    pub(super) async fn resolve(r: &WrappedLicenceResolver, kid: [u8; 16]) -> Result<[u8; 16], BoxError> {
        let secret = ephemeral_secret();
        let point = secret.public_key().to_sec1_point(false);
        let (x, y) = (point.x().ok_or("public key x")?, point.y().ok_or("public key y")?);
        let lic = r.fetch(&[kid], x, y).await?;
        let mut sec1 = Vec::with_capacity(65);
        sec1.push(0x04);
        sec1.extend_from_slice(&lic.server_x);
        sec1.extend_from_slice(&lic.server_y);
        let server = p256::PublicKey::from_sec1_bytes(&sec1)
            .map_err(|e| -> BoxError { format!("server epk is not a valid P-256 point: {e}").into() })?;
        let shared = p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), server.as_affine());
        let wk = wrapping_key(shared.raw_secret_bytes(), &lic.salt, &r.info)?;
        let key = unwrap_key(&wk, find_key(&lic, &kid)?)?;
        log::info!("[licence] wrapped key for KID {} unwrapped", hex::encode(&kid[..4]));
        Ok(key)
    }

    #[cfg(test)]
    pub(super) mod server_sim {
        //! The server side, in the same crates, for the round-trip test.
        use super::*;

        pub struct Response {
            pub json: String,
        }

        /// Wrap `content_key` for a client whose epk is (x, y).
        pub fn wrap(kid: [u8; 16], content_key: [u8; 16], client_x: &[u8], client_y: &[u8], info: &[u8]) -> Response {
            let server = ephemeral_secret();
            let mut sec1 = vec![0x04];
            sec1.extend_from_slice(client_x);
            sec1.extend_from_slice(client_y);
            let client = p256::PublicKey::from_sec1_bytes(&sec1).unwrap();
            let shared = p256::ecdh::diffie_hellman(server.to_nonzero_scalar(), client.as_affine());
            let salt: [u8; 32] = rand::random();
            let wk = wrapping_key(shared.raw_secret_bytes(), &salt, info).unwrap();
            let iv: [u8; 12] = rand::random();
            let cipher = Aes256Gcm::new_from_slice(&wk).unwrap();
            let nonce = aes_gcm::Nonce::try_from(&iv[..]).unwrap();
            // `encrypt` returns ciphertext || tag — exactly the wire layout.
            let buf = cipher.encrypt(&nonce, Payload { msg: &content_key, aad: &kid }).unwrap();
            let point = server.public_key().to_sec1_point(false);
            let json = serde_json::json!({
                "epk": { "kty": "EC", "crv": "P-256",
                         "x": base64url_encode(point.x().unwrap()), "y": base64url_encode(point.y().unwrap()) },
                "salt": base64url_encode(&salt),
                "keys": [ { "kty": "oct", "kid": base64url_encode(&kid), "alg": ALG,
                            "iv": base64url_encode(&iv), "k": base64url_encode(&buf) } ]
            })
            .to_string();
            Response { json }
        }
    }
}

// ---------------------------------------------------------------------------
// Browser: WebCrypto (non-extractable keys)
// ---------------------------------------------------------------------------

#[cfg(target_arch = "wasm32")]
mod web {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// A `!Send` local future presented as `Send` — one thread (see rt/web.rs).
    pub(super) struct SendFut<T>(pub Pin<Box<dyn Future<Output = T>>>);
    unsafe impl<T> Send for SendFut<T> {}
    impl<T> Future for SendFut<T> {
        type Output = T;
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
            self.0.as_mut().poll(cx)
        }
    }

    pub(super) async fn resolve(r: &WrappedLicenceResolver, kid: [u8; 16]) -> Result<(), BoxError> {
        let agreement = crate::crypto_web::KeyAgreement::begin().await?;
        let lic = r.fetch(&[kid], &agreement.x, &agreement.y).await?;
        let entry = find_key(&lic, &kid)?;
        crate::crypto_web::unwrap_and_install(&agreement, &lic.server_x, &lic.server_y, &lic.salt, &r.info, entry).await?;
        log::info!(
            "[licence] wrapped key for KID {} unwrapped into a non-extractable WebCrypto key",
            hex::encode(&kid[..4])
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_round_trips_and_matches_known_vectors() {
        // RFC 4648 test vectors, url-safe without padding.
        assert_eq!(base64url_encode(b""), "");
        assert_eq!(base64url_encode(b"f"), "Zg");
        assert_eq!(base64url_encode(b"fo"), "Zm8");
        assert_eq!(base64url_encode(b"foo"), "Zm9v");
        assert_eq!(base64url_encode(b"foob"), "Zm9vYg");
        assert_eq!(base64url_encode(&[0xfb, 0xff, 0xbf]), "-_-_");
        for n in 0..70usize {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 37 % 256) as u8).collect();
            assert_eq!(base64url_decode(&base64url_encode(&bytes)).unwrap(), bytes, "len {n}");
        }
        // Padded input (the old server helper emits it) and standard alphabet decode too.
        assert_eq!(base64url_decode("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(base64url_decode("+/8=").unwrap(), vec![0xfb, 0xff]);
        assert!(base64url_decode("Zm9vY").is_err());
        assert!(base64url_decode("Zm9v!g").is_err());
    }

    #[test]
    fn request_json_carries_kids_and_epk() {
        let kid = [0x51u8; 16];
        let j = request_json(&[kid], &[1u8; 32], &[2u8; 32]);
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["kids"][0], base64url_encode(&kid));
        assert_eq!(v["epk"]["kty"], "EC");
        assert_eq!(v["epk"]["crv"], "P-256");
        assert_eq!(base64url_decode(v["epk"]["x"].as_str().unwrap()).unwrap(), vec![1u8; 32]);
    }

    #[test]
    fn parse_response_rejects_wrong_alg_and_sizes() {
        let good = serde_json::json!({
            "epk": {"kty":"EC","crv":"P-256","x": base64url_encode(&[1u8;32]), "y": base64url_encode(&[2u8;32])},
            "salt": base64url_encode(&[3u8;32]),
            "keys": [{"kty":"oct","kid": base64url_encode(&[4u8;16]), "alg": ALG,
                      "iv": base64url_encode(&[5u8;12]), "k": base64url_encode(&[6u8;32])}]
        });
        let lic = parse_response(&good.to_string()).unwrap();
        assert_eq!(lic.keys.len(), 1);
        assert_eq!(lic.keys[0].kid, [4u8; 16]);

        let mut bad_alg = good.clone();
        bad_alg["keys"][0]["alg"] = "A128KW".into();
        assert!(parse_response(&bad_alg.to_string()).is_err());
        let mut no_alg = good.clone();
        no_alg["keys"][0].as_object_mut().unwrap().remove("alg");
        assert!(parse_response(&no_alg.to_string()).is_err());
        let mut short_iv = good.clone();
        short_iv["keys"][0]["iv"] = base64url_encode(&[5u8; 8]).into();
        assert!(parse_response(&short_iv.to_string()).is_err());
        let mut bad_curve = good.clone();
        bad_curve["epk"]["crv"] = "P-384".into();
        assert!(parse_response(&bad_curve.to_string()).is_err());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_round_trip_unwraps_what_the_server_wrapped() {
        use super::native::server_sim;
        use p256::elliptic_curve::sec1::ToSec1Point;
        let kid: [u8; 16] = rand::random();
        let content_key: [u8; 16] = rand::random();
        let secret = {
            let bytes: [u8; 32] = rand::random();
            p256::SecretKey::from_slice(&bytes).unwrap()
        };
        let point = secret.public_key().to_sec1_point(false);
        let resp = server_sim::wrap(kid, content_key, point.x().unwrap(), point.y().unwrap(), DEFAULT_HKDF_INFO.as_bytes());
        let lic = parse_response(&resp.json).unwrap();
        let mut sec1 = vec![0x04];
        sec1.extend_from_slice(&lic.server_x);
        sec1.extend_from_slice(&lic.server_y);
        let server = p256::PublicKey::from_sec1_bytes(&sec1).unwrap();
        let shared = p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), server.as_affine());
        let wk = super::native::wrapping_key(shared.raw_secret_bytes(), &lic.salt, DEFAULT_HKDF_INFO.as_bytes()).unwrap();
        let entry = find_key(&lic, &kid).unwrap();
        assert_eq!(super::native::unwrap_key(&wk, entry).unwrap(), content_key);

        // Wrong info → different wrapping key → authentication fails.
        let wk_bad = super::native::wrapping_key(shared.raw_secret_bytes(), &lic.salt, b"other-info").unwrap();
        assert!(super::native::unwrap_key(&wk_bad, entry).is_err());
        // Tampered KID (AAD) → authentication fails.
        let mut tampered = WrappedKey { kid: [0u8; 16], iv: entry.iv.clone(), wrapped: entry.wrapped.clone() };
        tampered.kid[0] ^= 1;
        assert!(super::native::unwrap_key(&wk, &tampered).is_err());
    }
}
