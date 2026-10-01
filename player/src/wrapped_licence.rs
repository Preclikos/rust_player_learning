//! Wrapped ClearKey licences (protocol v2): the client half of
//! `docs/CLEARKEY_WRAPPED_LICENCE.md`.
//!
//! The licence server never sends a content key in the clear. The client
//! makes an ephemeral ECDH P-256 key and posts `{v, kids, epk, proof, sid?}`.
//! Each key comes back as `AES-256-GCM(HKDF-SHA256(ECDH ‖ client secret,
//! salt, "rustplayer-clearkey-wrap-v2"), iv, aad = kid)`.
//!
//! The **client secret** is a per-app-version value that the app and the
//! backend know and that never crosses the wire. It does two jobs:
//!
//! - **`proof`**, `HMAC-SHA256(secret, label ‖ epk ‖ kids)`: lets the server
//!   tell which app version is asking, and refuse a revoked one before it
//!   wraps anything.
//!   - With a `sid` (scenario A) the server looks the secret up by id.
//!   - Without one (scenario B) it tries every secret it holds.
//! - **KDF input**: a client without the secret cannot unwrap a response even
//!   if it can talk to the endpoint.
//!
//! This module speaks the protocol on every platform:
//!
//! - native: `p256` + `hkdf` + `hmac` + `aes-gcm` unwrap the key into the
//!   ClearKey decryptor's cache. The transport is protected; the key lives in
//!   process memory, as any ClearKey key must.
//! - browser: WebCrypto derives and unwraps straight into a
//!   **non-extractable** `CryptoKey` (`crypto_web`). The plaintext key never
//!   exists in JavaScript or on the wire, and the CENC path then decrypts
//!   through WebCrypto only.
//!
//! The HTTP call goes through the engine's `HttpClient` with
//! `RequestKind::License`, so the host's `RequestInterceptor` adds its
//! authorisation headers, exactly as for every other request. Install with
//! `Player::set_wrapped_licence(url, LicenceClient)`.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use serde::Deserialize;

use crate::net::{BoxError, HttpClient, LicenseResolver, RequestKind};

/// Protocol version carried in the request (`"v": 2`).
pub const PROTOCOL_VERSION: u32 = 2;
/// The only wrapping algorithm this client accepts.
pub const ALG: &str = "ECDH-HKDF-A256GCM-v2";
/// HKDF `info`; fixed by the protocol version, not configurable.
pub const KDF_INFO: &[u8] = b"rustplayer-clearkey-wrap-v2";
/// Domain label at the start of the HMAC proof message.
pub const PROOF_LABEL: &[u8] = b"rustplayer-clearkey-proof-v2";
/// Shortest client secret accepted. Generate 32 random bytes.
pub const MIN_SECRET_LEN: usize = 16;

// ---------------------------------------------------------------------------
// Client secret
// ---------------------------------------------------------------------------

/// The app version's licence credentials: the shared secret and, for
/// scenario A, its non-secret id. Cheap to clone (`Arc` inside the resolver).
#[derive(Clone)]
pub struct LicenceClient {
    secret: Vec<u8>,
    secret_id: Option<String>,
}

impl LicenceClient {
    /// `secret`: base64url (or standard base64), at least
    /// [`MIN_SECRET_LEN`] bytes decoded. `secret_id`: the id the backend files
    /// it under (scenario A); `None` or empty = the server finds the secret
    /// from the proof (scenario B).
    pub fn new(secret: &str, secret_id: Option<String>) -> Result<Self, String> {
        Self::from_bytes(base64url_decode(secret).map_err(|e| format!("licence client secret: {e}"))?, secret_id)
    }

    pub fn from_bytes(secret: Vec<u8>, secret_id: Option<String>) -> Result<Self, String> {
        if secret.len() < MIN_SECRET_LEN {
            return Err(format!(
                "licence client secret is {} bytes, needs at least {MIN_SECRET_LEN}",
                secret.len()
            ));
        }
        Ok(Self {
            secret,
            secret_id: secret_id.filter(|s| !s.is_empty()),
        })
    }

    pub fn secret_id(&self) -> Option<&str> {
        self.secret_id.as_deref()
    }

    pub(crate) fn secret(&self) -> &[u8] {
        &self.secret
    }
}

impl std::fmt::Debug for LicenceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LicenceClient")
            .field("secret", &format_args!("<{} bytes>", self.secret.len()))
            .field("secret_id", &self.secret_id)
            .finish()
    }
}

impl Drop for LicenceClient {
    fn drop(&mut self) {
        for b in self.secret.iter_mut() {
            // Volatile so the wipe is not optimised away as a dead store.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

/// The bytes the proof HMAC covers: label ‖ 0x00 ‖ epk x ‖ epk y ‖ each KID.
/// All fields are fixed-size, so the concatenation is unambiguous.
pub fn proof_message(kids: &[[u8; 16]], epk_x: &[u8], epk_y: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(PROOF_LABEL.len() + 1 + 64 + 16 * kids.len());
    m.extend_from_slice(PROOF_LABEL);
    m.push(0);
    m.extend_from_slice(epk_x);
    m.extend_from_slice(epk_y);
    for k in kids {
        m.extend_from_slice(k);
    }
    m
}

/// HKDF input keying material: ECDH x-coordinate ‖ client secret.
pub fn kdf_ikm(shared: &[u8], secret: &[u8]) -> Vec<u8> {
    let mut ikm = Vec::with_capacity(shared.len() + secret.len());
    ikm.extend_from_slice(shared);
    ikm.extend_from_slice(secret);
    ikm
}

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
    #[serde(default)]
    notice: Option<String>,
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
    /// Server note for this app version, e.g. `"deprecated"` (still served,
    /// but due for an update). Logged, never acted on.
    pub notice: Option<String>,
}

/// The request body: `kids`, the client's ephemeral public key, the proof
/// and, for scenario A, the secret id.
pub fn request_json(kids: &[[u8; 16]], epk_x: &[u8], epk_y: &[u8], proof: &[u8], secret_id: Option<&str>) -> String {
    let kids: Vec<String> = kids.iter().map(|k| base64url_encode(k)).collect();
    let mut v = serde_json::json!({
        "v": PROTOCOL_VERSION,
        "kids": kids,
        "epk": { "kty": "EC", "crv": "P-256", "x": base64url_encode(epk_x), "y": base64url_encode(epk_y) },
        "proof": base64url_encode(proof),
    });
    if let Some(id) = secret_id {
        v["sid"] = id.into();
    }
    v.to_string()
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
        notice: r.notice.filter(|n| !n.is_empty()),
    })
}

/// The status codes the protocol gives a meaning, made readable; anything
/// else passes through unchanged.
fn explain_http_error(e: BoxError) -> BoxError {
    let msg = e.to_string();
    if msg.starts_with("http 401") {
        format!("licence: client not recognised ({msg}): wrong client secret or secret id").into()
    } else if msg.starts_with("http 403") {
        format!("licence: this app version's client secret is revoked ({msg}); the app needs an update").into()
    } else {
        e
    }
}

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

/// [`LicenseResolver`] that fetches wrapped keys from `url` and unwraps them
/// with a per-request ephemeral ECDH key. One POST per KID (video and audio
/// resolve separately when their init segments are parsed).
pub struct WrappedLicenceResolver {
    url: String,
    client: Arc<LicenceClient>,
    http: Arc<HttpClient>,
}

impl WrappedLicenceResolver {
    pub fn new(url: String, client: LicenceClient, http: Arc<HttpClient>) -> Self {
        Self {
            url,
            client: Arc::new(client),
            http,
        }
    }

    async fn fetch(&self, kids: &[[u8; 16]], epk_x: &[u8], epk_y: &[u8], proof: &[u8]) -> Result<WrappedLicence, BoxError> {
        let body = request_json(kids, epk_x, epk_y, proof, self.client.secret_id());
        let bytes = self
            .http
            .post(self.url.clone(), RequestKind::License, Bytes::from(body), "application/json")
            .await
            .map_err(explain_http_error)?;
        let text = std::str::from_utf8(&bytes).map_err(|e| -> BoxError { format!("licence response: {e}").into() })?;
        let lic = parse_response(text)?;
        if let Some(n) = &lic.notice {
            log::warn!("[licence] server notice for this app version: {n}");
        }
        Ok(lic)
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
            // of this resolver (the fields are Arcs).
            let me = WrappedLicenceResolver {
                url: self.url.clone(),
                client: Arc::clone(&self.client),
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
    use hmac::{Hmac, Mac};
    use p256::elliptic_curve::sec1::ToSec1Point;
    use sha2::Sha256;

    /// A fresh P-256 secret; `from_slice` rejects the (negligibly rare)
    /// out-of-range scalars, so draw again.
    pub(super) fn ephemeral_secret() -> p256::SecretKey {
        loop {
            let bytes: [u8; 32] = rand::random();
            if let Ok(k) = p256::SecretKey::from_slice(&bytes) {
                return k;
            }
        }
    }

    /// `HMAC-SHA256(secret, message)`.
    pub(super) fn hmac_sha256(secret: &[u8], message: &[u8]) -> [u8; 32] {
        let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(secret).expect("HMAC takes any key length");
        mac.update(message);
        mac.finalize().into_bytes().into()
    }

    /// Wrapping key: HKDF-SHA256(ikm = ECDH ‖ client secret, salt, KDF_INFO),
    /// as the server does it.
    pub(super) fn wrapping_key(shared: &[u8], client_secret: &[u8], salt: &[u8]) -> Result<[u8; 32], BoxError> {
        let ikm = kdf_ikm(shared, client_secret);
        let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let mut out = [0u8; 32];
        hk.expand(KDF_INFO, &mut out)
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
            .map_err(|_| -> BoxError {
                "wrapped key failed authentication (wrong client secret, KID or tampered)".into()
            })?;
        plain
            .try_into()
            .map_err(|_| -> BoxError { "unwrapped key is not 16 bytes".into() })
    }

    pub(super) async fn resolve(r: &WrappedLicenceResolver, kid: [u8; 16]) -> Result<[u8; 16], BoxError> {
        let secret = ephemeral_secret();
        let point = secret.public_key().to_sec1_point(false);
        let (x, y) = (point.x().ok_or("public key x")?, point.y().ok_or("public key y")?);
        let proof = hmac_sha256(r.client.secret(), &proof_message(&[kid], x, y));
        let lic = r.fetch(&[kid], x, y, &proof).await?;
        let mut sec1 = Vec::with_capacity(65);
        sec1.push(0x04);
        sec1.extend_from_slice(&lic.server_x);
        sec1.extend_from_slice(&lic.server_y);
        let server = p256::PublicKey::from_sec1_bytes(&sec1)
            .map_err(|e| -> BoxError { format!("server epk is not a valid P-256 point: {e}").into() })?;
        let shared = p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), server.as_affine());
        let wk = wrapping_key(shared.raw_secret_bytes(), r.client.secret(), &lic.salt)?;
        let key = unwrap_key(&wk, find_key(&lic, &kid)?)?;
        log::info!("[licence] wrapped key for KID {} unwrapped", hex::encode(&kid[..4]));
        Ok(key)
    }

    #[cfg(test)]
    pub(super) mod server_sim {
        //! The server side, in the same crates, for the round-trip tests:
        //! the secret table, scenario A/B lookup and the wrap.
        use super::*;

        #[derive(Clone, Copy, PartialEq, Debug)]
        pub enum Status {
            Active,
            Deprecated,
            Revoked,
        }

        pub struct Row {
            pub id: &'static str,
            pub secret: Vec<u8>,
            pub status: Status,
        }

        /// What the endpoint answers before wrapping anything.
        #[derive(PartialEq, Debug)]
        pub enum Verdict {
            /// Index of the matching row.
            Serve(usize),
            /// 401: unknown sid or no secret produces this proof.
            Unauthorized,
            /// 403: the matching secret is revoked.
            Revoked,
        }

        fn ct_eq(a: &[u8], b: &[u8]) -> bool {
            a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
        }

        /// Scenario A (sid given): one lookup + one HMAC. Scenario B: an
        /// HMAC per row, every row, no early exit.
        pub fn authenticate(table: &[Row], sid: Option<&str>, proof: &[u8], message: &[u8]) -> Verdict {
            let found = match sid {
                Some(sid) => table
                    .iter()
                    .position(|r| r.id == sid)
                    .filter(|&i| ct_eq(&hmac_sha256(&table[i].secret, message), proof)),
                None => {
                    let mut found = None;
                    for (i, r) in table.iter().enumerate() {
                        if ct_eq(&hmac_sha256(&r.secret, message), proof) && found.is_none() {
                            found = Some(i);
                        }
                    }
                    found
                }
            };
            match found {
                None => Verdict::Unauthorized,
                Some(i) if table[i].status == Status::Revoked => Verdict::Revoked,
                Some(i) => Verdict::Serve(i),
            }
        }

        /// Wrap `content_key` for a client whose epk is (x, y), under
        /// `client_secret`.
        pub fn wrap(kid: [u8; 16], content_key: [u8; 16], client_x: &[u8], client_y: &[u8], client_secret: &[u8]) -> String {
            let server = ephemeral_secret();
            let mut sec1 = vec![0x04];
            sec1.extend_from_slice(client_x);
            sec1.extend_from_slice(client_y);
            let client = p256::PublicKey::from_sec1_bytes(&sec1).unwrap();
            let shared = p256::ecdh::diffie_hellman(server.to_nonzero_scalar(), client.as_affine());
            let salt: [u8; 32] = rand::random();
            let wk = wrapping_key(shared.raw_secret_bytes(), client_secret, &salt).unwrap();
            let iv: [u8; 12] = rand::random();
            let cipher = Aes256Gcm::new_from_slice(&wk).unwrap();
            let nonce = aes_gcm::Nonce::try_from(&iv[..]).unwrap();
            // `encrypt` returns ciphertext || tag: exactly the wire layout.
            let buf = cipher.encrypt(&nonce, Payload { msg: &content_key, aad: &kid }).unwrap();
            let point = server.public_key().to_sec1_point(false);
            serde_json::json!({
                "epk": { "kty": "EC", "crv": "P-256",
                         "x": base64url_encode(point.x().unwrap()), "y": base64url_encode(point.y().unwrap()) },
                "salt": base64url_encode(&salt),
                "keys": [ { "kty": "oct", "kid": base64url_encode(&kid), "alg": ALG,
                            "iv": base64url_encode(&iv), "k": base64url_encode(&buf) } ]
            })
            .to_string()
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

    /// A `!Send` local future presented as `Send`: one thread (see rt/web.rs).
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
        let message = proof_message(&[kid], &agreement.x, &agreement.y);
        let proof = crate::crypto_web::hmac_sha256(r.client.secret(), &message).await?;
        let lic = r.fetch(&[kid], &agreement.x, &agreement.y, &proof).await?;
        let entry = find_key(&lic, &kid)?;
        crate::crypto_web::unwrap_and_install(&agreement, &lic.server_x, &lic.server_y, &lic.salt, r.client.secret(), entry)
            .await?;
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
    fn client_secret_validation_and_redacted_debug() {
        assert!(LicenceClient::new(&base64url_encode(&[7u8; 15]), None).is_err());
        assert!(LicenceClient::new("not base64!", None).is_err());
        let c = LicenceClient::new(&base64url_encode(&[7u8; 32]), Some(String::new())).unwrap();
        assert_eq!(c.secret_id(), None, "empty sid = scenario B");
        let c = LicenceClient::new(&base64url_encode(&[7u8; 32]), Some("android-0.1.52".into())).unwrap();
        assert_eq!(c.secret_id(), Some("android-0.1.52"));
        let dbg = format!("{c:?}");
        assert!(dbg.contains("<32 bytes>") && !dbg.contains("7, 7"), "{dbg}");
    }

    #[test]
    fn request_json_carries_version_kids_epk_proof_and_optional_sid() {
        let kid = [0x51u8; 16];
        let j = request_json(&[kid], &[1u8; 32], &[2u8; 32], &[9u8; 32], None);
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["v"], 2);
        assert_eq!(v["kids"][0], base64url_encode(&kid));
        assert_eq!(v["epk"]["kty"], "EC");
        assert_eq!(v["epk"]["crv"], "P-256");
        assert_eq!(base64url_decode(v["epk"]["x"].as_str().unwrap()).unwrap(), vec![1u8; 32]);
        assert_eq!(base64url_decode(v["proof"].as_str().unwrap()).unwrap(), vec![9u8; 32]);
        assert!(v.get("sid").is_none());
        let j = request_json(&[kid], &[1u8; 32], &[2u8; 32], &[9u8; 32], Some("ios-0.1.52"));
        let v: serde_json::Value = serde_json::from_str(&j).unwrap();
        assert_eq!(v["sid"], "ios-0.1.52");
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
        assert!(lic.notice.is_none());

        let mut with_notice = good.clone();
        with_notice["notice"] = "deprecated".into();
        assert_eq!(parse_response(&with_notice.to_string()).unwrap().notice.as_deref(), Some("deprecated"));

        // A v1 server's alg is refused, not misread.
        let mut v1_alg = good.clone();
        v1_alg["keys"][0]["alg"] = "ECDH-HKDF-A256GCM".into();
        assert!(parse_response(&v1_alg.to_string()).is_err());
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

    #[test]
    fn http_status_errors_are_explained() {
        let e = explain_http_error("http 403".into()).to_string();
        assert!(e.contains("revoked"), "{e}");
        let e = explain_http_error("http 401".into()).to_string();
        assert!(e.contains("not recognised"), "{e}");
        assert_eq!(explain_http_error("http 500".into()).to_string(), "http 500");
    }

    /// Known-answer vectors, also in docs/CLEARKEY_WRAPPED_LICENCE.md and
    /// checked by scripts/wrapped_licence_mock.py --self-test (independent
    /// python `cryptography`), so a backend can test against fixed numbers.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn known_answer_vectors() {
        let secret: Vec<u8> = (0u8..32).collect();
        let x = [0x11u8; 32];
        let y = [0x22u8; 32];
        let kid = [0x33u8; 16];
        let proof = native::hmac_sha256(&secret, &proof_message(&[kid], &x, &y));
        assert_eq!(hex::encode(proof), KAT_PROOF);
        let shared = [0x44u8; 32];
        let salt = [0x55u8; 32];
        let wk = native::wrapping_key(&shared, &secret, &salt).unwrap();
        assert_eq!(hex::encode(wk), KAT_WRAPPING_KEY);
    }
    const KAT_PROOF: &str = "e387a32563d9f7c997fd213c9852ba906da5f7e5ec215fe95ec49c461716f6a2";
    const KAT_WRAPPING_KEY: &str = "b5303419aaaa933b7a1816889f946502febc8c0b139177da79defe606954386b";

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_round_trip_scenarios_a_and_b_and_revocation() {
        use super::native::server_sim::{self, Row, Status, Verdict};
        use p256::elliptic_curve::sec1::ToSec1Point;

        // A backend table: many app versions, one revoked, one deprecated.
        let mut table: Vec<Row> = (0..160)
            .map(|i| Row { id: "filler", secret: [vec![0xee; 31], vec![i as u8]].concat(), status: Status::Active })
            .collect();
        table.push(Row { id: "android-0.1.51", secret: [1u8; 32].to_vec(), status: Status::Revoked });
        table.push(Row { id: "android-0.1.52", secret: [2u8; 32].to_vec(), status: Status::Deprecated });
        table.push(Row { id: "android-0.1.53", secret: [3u8; 32].to_vec(), status: Status::Active });

        let kid: [u8; 16] = rand::random();
        let content_key: [u8; 16] = rand::random();
        let eph = native::ephemeral_secret();
        let point = eph.public_key().to_sec1_point(false);
        let (x, y) = (point.x().unwrap(), point.y().unwrap());
        let message = proof_message(&[kid], x, y);
        let proof_of = |secret: &[u8]| native::hmac_sha256(secret, &message);

        // Scenario A and B find the same row.
        let p53 = proof_of(&[3u8; 32]);
        let n = table.len();
        assert_eq!(server_sim::authenticate(&table, Some("android-0.1.53"), &p53, &message), Verdict::Serve(n - 1));
        assert_eq!(server_sim::authenticate(&table, None, &p53, &message), Verdict::Serve(n - 1));
        // Deprecated still served.
        let p52 = proof_of(&[2u8; 32]);
        assert_eq!(server_sim::authenticate(&table, None, &p52, &message), Verdict::Serve(n - 2));
        // Revoked → 403 in both scenarios.
        let p51 = proof_of(&[1u8; 32]);
        assert_eq!(server_sim::authenticate(&table, Some("android-0.1.51"), &p51, &message), Verdict::Revoked);
        assert_eq!(server_sim::authenticate(&table, None, &p51, &message), Verdict::Revoked);
        // Unknown secret, wrong sid, or a proof replayed for another epk → 401.
        let unknown = proof_of(&[9u8; 32]);
        assert_eq!(server_sim::authenticate(&table, None, &unknown, &message), Verdict::Unauthorized);
        assert_eq!(server_sim::authenticate(&table, Some("android-0.1.52"), &p53, &message), Verdict::Unauthorized);
        let other_epk = proof_message(&[kid], &[0u8; 32], y);
        assert_eq!(server_sim::authenticate(&table, None, &p53, &other_epk), Verdict::Unauthorized);

        // The served row's secret wraps; the client with that secret unwraps.
        let json = server_sim::wrap(kid, content_key, x, y, &[3u8; 32]);
        let lic = parse_response(&json).unwrap();
        let mut sec1 = vec![0x04];
        sec1.extend_from_slice(&lic.server_x);
        sec1.extend_from_slice(&lic.server_y);
        let server = p256::PublicKey::from_sec1_bytes(&sec1).unwrap();
        let shared = p256::ecdh::diffie_hellman(eph.to_nonzero_scalar(), server.as_affine());
        let entry = find_key(&lic, &kid).unwrap();
        let wk = native::wrapping_key(shared.raw_secret_bytes(), &[3u8; 32], &lic.salt).unwrap();
        assert_eq!(native::unwrap_key(&wk, entry).unwrap(), content_key);

        // Another app version's secret (or none at all) cannot unwrap it.
        let wk_other = native::wrapping_key(shared.raw_secret_bytes(), &[2u8; 32], &lic.salt).unwrap();
        assert!(native::unwrap_key(&wk_other, entry).is_err());
        // Tampered KID (AAD) → authentication fails.
        let tampered = WrappedKey { kid: [0u8; 16], iv: entry.iv.clone(), wrapped: entry.wrapped.clone() };
        assert!(native::unwrap_key(&wk, &tampered).is_err());
    }
}
