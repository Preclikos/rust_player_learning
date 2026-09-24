//! Browser CENC decrypt on WebCrypto (`SubtleCrypto` AES-CTR).
//!
//! Software AES in wasm runs ~30 MiB/s on the main thread — a 5 MiB video
//! segment is ~170 ms of the one thread that also runs the audio callbacks,
//! WebCodecs delivery and rendering. `crypto.subtle.decrypt` runs on the
//! browser's crypto thread pool with hardware AES; the main thread only
//! gathers the protected bytes, fires one decrypt per sample (they run
//! concurrently) and scatters the results back.
//!
//! Counter convention matches the software path: the 16-byte IV (an 8-byte
//! CENC IV is zero-padded by the senc parser) is the initial counter block
//! and the whole 128-bit block increments (`Ctr128BE`) — WebCrypto
//! `length: 128`. Clear subsample runs do not advance the counter, so a
//! sample's protected spans are decrypted as ONE stream.

use std::cell::RefCell;
use std::collections::HashMap;

use js_sys::Uint8Array;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::crypto::{protected_spans, SencEntry};
use crate::net::BoxError;

thread_local! {
    /// Imported `CryptoKey` per KID: `importKey` is async and not free, the
    /// same key decrypts thousands of samples.
    static KEYS: RefCell<HashMap<[u8; 16], web_sys::CryptoKey>> = RefCell::new(HashMap::new());
}

fn describe(e: &JsValue) -> String {
    if let Some(ex) = e.dyn_ref::<web_sys::DomException>() {
        format!("{}: {}", ex.name(), ex.message())
    } else {
        e.as_string().unwrap_or_else(|| format!("{:?}", e))
    }
}

/// `crypto.subtle` of whatever global scope we run in. `None` when the page
/// is not a secure context (WebCrypto is HTTPS/localhost only) — the caller
/// falls back to software AES.
pub fn subtle() -> Option<web_sys::SubtleCrypto> {
    let global = js_sys::global();
    let crypto = if let Some(win) = global.dyn_ref::<web_sys::Window>() {
        win.crypto().ok()?
    } else if let Some(scope) = global.dyn_ref::<web_sys::WorkerGlobalScope>() {
        scope.crypto().ok()?
    } else {
        return None;
    };
    Some(crypto.subtle())
}

/// Register a key the platform holds for `kid` — a non-extractable
/// `CryptoKey` from a wrapped licence (`wrapped_licence`). From here on the
/// CENC path decrypts this KID through WebCrypto only.
pub fn install_key(kid: [u8; 16], key: web_sys::CryptoKey) {
    KEYS.with(|m| m.borrow_mut().insert(kid, key));
}

fn js_obj(pairs: &[(&str, JsValue)]) -> js_sys::Object {
    let o = js_sys::Object::new();
    for (k, v) in pairs {
        let _ = js_sys::Reflect::set(&o, &JsValue::from_str(k), v);
    }
    o
}

fn str_array(items: &[&str]) -> JsValue {
    let a = js_sys::Array::new();
    for s in items {
        a.push(&JsValue::from_str(s));
    }
    a.into()
}

async fn await_key(promise: Result<js_sys::Promise, JsValue>, what: &str) -> Result<web_sys::CryptoKey, BoxError> {
    let promise = promise.map_err(|e| -> BoxError { format!("{what}: {}", describe(&e)).into() })?;
    Ok(JsFuture::from(promise)
        .await
        .map_err(|e| -> BoxError { format!("{what}: {}", describe(&e)).into() })?
        .unchecked_into())
}

/// Client half of a wrapped-licence exchange: an ephemeral, non-extractable
/// ECDH P-256 key pair whose public coordinates go into the request `epk`.
pub struct KeyAgreement {
    private: web_sys::CryptoKey,
    pub x: Vec<u8>,
    pub y: Vec<u8>,
}

impl KeyAgreement {
    pub async fn begin() -> Result<Self, BoxError> {
        let subtle = subtle().ok_or("WebCrypto unavailable (not a secure context?)")?;
        let alg = js_obj(&[("name", "ECDH".into()), ("namedCurve", "P-256".into())]);
        let pair_promise = subtle
            .generate_key_with_object(&alg, false, &str_array(&["deriveBits"]))
            .map_err(|e| -> BoxError { format!("generateKey(ECDH): {}", describe(&e)).into() })?;
        let pair = JsFuture::from(pair_promise)
            .await
            .map_err(|e| -> BoxError { format!("generateKey(ECDH): {}", describe(&e)).into() })?;
        let private: web_sys::CryptoKey = js_sys::Reflect::get(&pair, &"privateKey".into())
            .map_err(|e| -> BoxError { describe(&e).into() })?
            .unchecked_into();
        let public: web_sys::CryptoKey = js_sys::Reflect::get(&pair, &"publicKey".into())
            .map_err(|e| -> BoxError { describe(&e).into() })?
            .unchecked_into();
        let jwk_promise = subtle
            .export_key("jwk", &public)
            .map_err(|e| -> BoxError { format!("exportKey(jwk): {}", describe(&e)).into() })?;
        let jwk = JsFuture::from(jwk_promise)
            .await
            .map_err(|e| -> BoxError { format!("exportKey(jwk): {}", describe(&e)).into() })?;
        let coord = |name: &str| -> Result<Vec<u8>, BoxError> {
            let v = js_sys::Reflect::get(&jwk, &JsValue::from_str(name)).map_err(|e| -> BoxError { describe(&e).into() })?;
            let s = v.as_string().ok_or_else(|| -> BoxError { format!("jwk.{name} missing").into() })?;
            Ok(crate::wrapped_licence::base64url_decode(&s)?)
        };
        Ok(Self {
            private,
            x: coord("x")?,
            y: coord("y")?,
        })
    }
}

/// Derive the wrapping key from the server's `epk` and `salt`, unwrap
/// `entry` into a non-extractable AES-CTR key and register it for its KID.
/// Byte-for-byte the server's recipe: ECDH x-coordinate → HKDF-SHA256(salt,
/// info) → AES-256-GCM with the KID as additional data.
pub async fn unwrap_and_install(
    agreement: &KeyAgreement,
    server_x: &[u8],
    server_y: &[u8],
    salt: &[u8],
    info: &[u8],
    entry: &crate::wrapped_licence::WrappedKey,
) -> Result<(), BoxError> {
    use crate::wrapped_licence::base64url_encode;
    let subtle = subtle().ok_or("WebCrypto unavailable (not a secure context?)")?;
    let ecdh = js_obj(&[("name", "ECDH".into()), ("namedCurve", "P-256".into())]);
    let server_jwk = js_obj(&[
        ("kty", "EC".into()),
        ("crv", "P-256".into()),
        ("x", base64url_encode(server_x).into()),
        ("y", base64url_encode(server_y).into()),
        ("ext", JsValue::TRUE),
    ]);
    let server_pub = await_key(
        subtle.import_key_with_object("jwk", &server_jwk, &ecdh, false, &js_sys::Array::new().into()),
        "importKey(server epk)",
    )
    .await?;
    let derive_alg = js_obj(&[("name", "ECDH".into()), ("public", server_pub.into())]);
    let bits_promise = subtle
        .derive_bits_with_object(&derive_alg, &agreement.private, 256)
        .map_err(|e| -> BoxError { format!("deriveBits(ECDH): {}", describe(&e)).into() })?;
    let secret: js_sys::ArrayBuffer = JsFuture::from(bits_promise)
        .await
        .map_err(|e| -> BoxError { format!("deriveBits(ECDH): {}", describe(&e)).into() })?
        .unchecked_into();
    let hkdf_key = await_key(
        subtle.import_key_with_str("raw", &Uint8Array::new(&secret), "HKDF", false, &str_array(&["deriveKey"])),
        "importKey(HKDF)",
    )
    .await?;
    let hkdf_alg = js_obj(&[
        ("name", "HKDF".into()),
        ("hash", "SHA-256".into()),
        ("salt", Uint8Array::from(salt).into()),
        ("info", Uint8Array::from(info).into()),
    ]);
    let gcm_type = js_obj(&[("name", "AES-GCM".into()), ("length", JsValue::from_f64(256.0))]);
    let wrap_key = await_key(
        subtle.derive_key_with_object_and_object(&hkdf_alg, &hkdf_key, &gcm_type, false, &str_array(&["unwrapKey"])),
        "deriveKey(HKDF→AES-GCM)",
    )
    .await?;
    let unwrap_alg = js_obj(&[
        ("name", "AES-GCM".into()),
        ("iv", Uint8Array::from(&entry.iv[..]).into()),
        ("additionalData", Uint8Array::from(&entry.kid[..]).into()),
    ]);
    let ctr_type = js_obj(&[("name", "AES-CTR".into()), ("length", JsValue::from_f64(128.0))]);
    let content_key = await_key(
        subtle.unwrap_key_with_buffer_source_and_object_and_object(
            "raw",
            &Uint8Array::from(&entry.wrapped[..]),
            &wrap_key,
            &unwrap_alg,
            &ctr_type,
            false,
            &str_array(&["decrypt"]),
        ),
        "unwrapKey(AES-GCM → AES-CTR)",
    )
    .await?;
    install_key(entry.kid, content_key);
    Ok(())
}

async fn crypto_key(subtle: &web_sys::SubtleCrypto, kid: &[u8; 16], key: Option<&[u8; 16]>) -> Result<web_sys::CryptoKey, BoxError> {
    if let Some(k) = KEYS.with(|m| m.borrow().get(kid).cloned()) {
        return Ok(k);
    }
    let key = key.ok_or("no WebCrypto key installed for this KID and no raw key to import")?;
    let usages = js_sys::Array::new();
    usages.push(&JsValue::from_str("decrypt"));
    let promise = subtle
        .import_key_with_str("raw", &Uint8Array::from(&key[..]), "AES-CTR", false, &usages)
        .map_err(|e| -> BoxError { format!("importKey: {}", describe(&e)).into() })?;
    let k: web_sys::CryptoKey = JsFuture::from(promise)
        .await
        .map_err(|e| -> BoxError { format!("importKey: {}", describe(&e)).into() })?
        .unchecked_into();
    KEYS.with(|m| m.borrow_mut().insert(*kid, k.clone()));
    Ok(k)
}

/// Decrypt every protected sample of a segment in place. `sample_ranges` and
/// `senc` are the segment's sample table and senc entries, index-aligned.
/// `key`: the raw ClearKey bytes to import when no `CryptoKey` is registered
/// for `kid` yet; `None` when the key is platform-held (installed by a
/// wrapped licence) — then a missing registration is an error.
pub async fn decrypt_samples(
    data: &mut [u8],
    kid: &[u8; 16],
    key: Option<&[u8; 16]>,
    sample_ranges: &[(usize, usize)],
    senc: &[SencEntry],
) -> Result<(), BoxError> {
    let subtle = subtle().ok_or("WebCrypto unavailable (not a secure context?)")?;
    let ckey = crypto_key(&subtle, kid, key).await?;

    // Gather + fire. Each entry: (sample spans, promise) — the promise runs
    // on the crypto pool while we keep gathering the next sample.
    let mut jobs: Vec<(Vec<(usize, usize)>, js_sys::Promise)> = Vec::with_capacity(sample_ranges.len());
    for ((offset, size), entry) in sample_ranges.iter().zip(senc.iter()) {
        let end = offset + size;
        if end > data.len() {
            continue;
        }
        let iv_is_zero = entry.iv.iter().all(|&b| b == 0);
        let no_encrypted_bytes =
            !entry.subsamples.is_empty() && entry.subsamples.iter().all(|&(_, enc)| enc == 0);
        if iv_is_zero || no_encrypted_bytes {
            continue;
        }
        let sample = &data[*offset..end];
        let spans: Vec<(usize, usize)> = if entry.subsamples.is_empty() {
            vec![(*offset, end)]
        } else {
            protected_spans(&entry.subsamples, sample.len())?
                .into_iter()
                .map(|(a, b)| (offset + a, offset + b))
                .collect()
        };
        let total: usize = spans.iter().map(|&(a, b)| b - a).sum();
        if total == 0 {
            continue;
        }
        let cipher = Uint8Array::new_with_length(total as u32);
        let mut at = 0u32;
        for &(a, b) in &spans {
            cipher.subarray(at, at + (b - a) as u32).copy_from(&data[a..b]);
            at += (b - a) as u32;
        }
        let params = web_sys::AesCtrParams::new("AES-CTR", &Uint8Array::from(&entry.iv[..]), 128);
        let promise = subtle
            .decrypt_with_object_and_buffer_source(&params, &ckey, &cipher)
            .map_err(|e| -> BoxError { format!("subtle.decrypt: {}", describe(&e)).into() })?;
        jobs.push((spans, promise));
    }

    // Await + scatter, in order (they complete concurrently regardless).
    for (spans, promise) in jobs {
        let plain: js_sys::ArrayBuffer = JsFuture::from(promise)
            .await
            .map_err(|e| -> BoxError { format!("subtle.decrypt: {}", describe(&e)).into() })?
            .unchecked_into();
        let plain = Uint8Array::new(&plain);
        let mut at = 0u32;
        for (a, b) in spans {
            let n = (b - a) as u32;
            plain.subarray(at, at + n).copy_to(&mut data[a..b]);
            at += n;
        }
    }
    Ok(())
}
