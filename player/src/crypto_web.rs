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

async fn crypto_key(subtle: &web_sys::SubtleCrypto, kid: &[u8; 16], key: &[u8; 16]) -> Result<web_sys::CryptoKey, BoxError> {
    if let Some(k) = KEYS.with(|m| m.borrow().get(kid).cloned()) {
        return Ok(k);
    }
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
pub async fn decrypt_samples(
    data: &mut [u8],
    kid: &[u8; 16],
    key: &[u8; 16],
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
