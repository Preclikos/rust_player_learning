use std::collections::HashMap;
use std::error::Error;
use std::sync::{Arc, Mutex};

use aes::cipher::{KeyIvInit, StreamCipher};
use aes::Aes128;
use ctr::Ctr128BE;

use crate::net::{BoxError, LicenseResolver};

/// Render a key ID as a short identifier for logs. Returns the first 8
/// hex chars + ellipsis so engineers can correlate without leaking the
/// full KID. Use everywhere we'd otherwise hex::encode the full thing.
pub fn kid_short(kid: &[u8; 16]) -> String {
    let full = hex::encode(kid);
    format!("{}…", &full[..8])
}

type Aes128Ctr = Ctr128BE<Aes128>;

/// Whether CENC decryption should go through the aws-lc-rs (BoringSSL) cipher
/// instead of the RustCrypto one.
///
/// Only when RustCrypto has no hardware backend for this target, which in
/// practice means 32-bit ARM: it implements hardware AES for x86/x86_64 and
/// aarch64 only, so a desktop or a 64-bit phone is already on the silicon and
/// there is nothing to gain from a second code path.
#[cfg(target_os = "android")]
fn use_boringssl_aes() -> bool {
    use std::sync::OnceLock;
    static USE: OnceLock<bool> = OnceLock::new();
    *USE.get_or_init(|| {
        !aes::hardware_accelerated()
    })
}

#[cfg(not(target_os = "android"))]
fn use_boringssl_aes() -> bool {
    false
}

/// Byte ranges of a sample that are actually encrypted, in order.
///
/// A CENC subsample is a (clear, encrypted) pair; the clear run comes
/// first. Bounds are validated here once so neither cipher path has to.
fn protected_spans(
    subsamples: &[(u16, u32)],
    len: usize,
) -> Result<Vec<(usize, usize)>, Box<dyn Error + Send + Sync>> {
    let mut spans = Vec::with_capacity(subsamples.len());
    let mut offset = 0usize;
    for &(clear, encrypted) in subsamples {
        offset = offset.saturating_add(clear as usize);
        let end = offset.saturating_add(encrypted as usize);
        if end > len {
            return Err(format!(
                "Subsample bounds ({offset}..{end}) exceed sample length {len}"
            )
            .into());
        }
        if encrypted > 0 {
            spans.push((offset, end));
        }
        offset = end;
    }
    Ok(spans)
}

/// AES-128-CTR over `buf`, in place, starting from `iv`.
#[cfg(target_os = "android")]
fn boringssl_ctr(
    key: &[u8; 16],
    iv: &[u8; 16],
    buf: &mut [u8],
) -> Result<(), Box<dyn Error + Send + Sync>> {
    use aws_lc_rs::cipher::{DecryptingKey, DecryptionContext, UnboundCipherKey, AES_128};
    use aws_lc_rs::iv::{FixedLength, IV_LEN_128_BIT};

    let unbound = UnboundCipherKey::new(&AES_128, key)
        .map_err(|_| -> Box<dyn Error + Send + Sync> { "aws-lc: bad AES key".into() })?;
    let dk = DecryptingKey::ctr(unbound)
        .map_err(|_| -> Box<dyn Error + Send + Sync> { "aws-lc: ctr init".into() })?;
    let ctx = DecryptionContext::Iv128(FixedLength::<IV_LEN_128_BIT>::from(iv));
    dk.decrypt(buf, ctx)
        .map_err(|_| -> Box<dyn Error + Send + Sync> { "aws-lc: ctr decrypt".into() })?;
    Ok(())
}

#[cfg(not(target_os = "android"))]
fn boringssl_ctr(
    _key: &[u8; 16],
    _iv: &[u8; 16],
    _buf: &mut [u8],
) -> Result<(), Box<dyn Error + Send + Sync>> {
    unreachable!("boringssl_ctr is only reachable where use_boringssl_aes() is true")
}

/// One-shot AES capability + throughput report, logged the first time a sample
/// is decrypted.
///
/// CENC decrypt dominates segment preparation (a 14 Mbps segment measured
/// ~680 ms on a Google TV Streamer, ~16 MiB/s), which is far below what ARMv8
/// AES instructions should manage. This says, on the machine actually running,
/// whether the hardware backend was selected and what a contiguous keystream
/// really costs — so "is it the AES" stops being inferred from cycle counts.
pub fn log_aes_capability() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        if aes::hardware_accelerated() {
            log::debug!("[crypto] AES: hardware (RustCrypto, {})", std::env::consts::ARCH);
            return;
        }
        if use_boringssl_aes() {
            log::debug!(
                "[crypto] AES: hardware via aws-lc-rs on {} — the `aes` crate has no backend for this arch",
                std::env::consts::ARCH
            );
            return;
        }
        // Nothing reaches the silicon here. Worth a warning rather than a
        // debug line: software AES-CTR is ~40x slower and CENC decrypt sits on
        // the segment-boundary critical path, so it decides whether an ABR
        // switch is clean. Verbose logging adds the measured rate.
        let mut detail = String::new();
        if log::log_enabled!(log::Level::Debug) {
            let mut buf = vec![0u8; 4 * 1024 * 1024];
            let mut cipher = Aes128Ctr::new(&[0u8; 16].into(), &[0u8; 16].into());
            let t0 = crate::rt::Instant::now();
            cipher.apply_keystream(&mut buf);
            let ms = t0.elapsed().as_millis().max(1);
            detail = format!(" ({} MiB/s measured over 4 MiB)", 4 * 1000 / ms);
        }
        log::warn!(
            "[crypto] AES: SOFTWARE on {}{} — CENC decrypt will be the slowest step of              segment preparation",
            std::env::consts::ARCH,
            detail
        );
    });
}

/// Abstraction over CENC sample decryption.
///
/// Today implemented by [`ClearKeyDecryptor`] (software AES-CTR). Platform-backed
/// decryptors (Android `MediaDrm`, iOS FairPlay, Widevine CDM) can be added by
/// implementing this trait without touching the pipeline.
///
/// `decrypt_sample` is **synchronous** — the per-sample inner loop runs
/// on the hot path and must not await. Async key resolution happens
/// earlier via [`Decryptor::ensure_key_for`], with the result cached so
/// that `decrypt_sample` only ever does a hashmap lookup.
#[async_trait::async_trait]
pub trait Decryptor: Send + Sync {
    /// Ensure the key for `kid` is available locally (either pre‑seeded
    /// or resolved via the attached `LicenseResolver`). Called once per
    /// track when the decoder pipeline parses `tenc`; ClearKey-only
    /// decryptors with a resolver will await an HTTP round trip here.
    /// Implementations without an async resolver (pre-populated cache,
    /// platform-managed key store) may make this a no-op.
    async fn ensure_key_for(&self, kid: [u8; 16]) -> Result<(), BoxError>;

    fn decrypt_sample(
        &self,
        kid: &[u8; 16],
        iv: &[u8; 16],
        data: &mut [u8],
        subsamples: &[(u16, u32)],
    ) -> Result<(), Box<dyn Error + Send + Sync>>;
}

/// Software AES-128-CTR ClearKey decryptor. Holds a `(kid → key)` cache
/// that is populated either eagerly via [`ClearKeyDecryptor::from_hex`]
/// (the legacy `set_clearkey(HashMap)` path) or lazily on first use via
/// an attached [`LicenseResolver`].
///
/// Both the cache and the resolver use interior mutability so the
/// decryptor can be shared through `Arc<ClearKeyDecryptor>` while the
/// player still lets the consumer install / replace the resolver at any
/// point before playback starts.
pub struct ClearKeyDecryptor {
    keys: Mutex<HashMap<[u8; 16], [u8; 16]>>,
    resolver: Mutex<Option<Arc<dyn LicenseResolver>>>,
}

impl ClearKeyDecryptor {
    pub fn new(keys: HashMap<[u8; 16], [u8; 16]>) -> Self {
        Self {
            keys: Mutex::new(keys),
            resolver: Mutex::new(None),
        }
    }

    pub fn from_hex(map: HashMap<String, String>) -> Result<Self, Box<dyn Error>> {
        let mut keys = HashMap::new();
        for (kid_hex, key_hex) in map {
            let kid_bytes = hex::decode(kid_hex.trim())?;
            let key_bytes = hex::decode(key_hex.trim())?;
            if kid_bytes.len() != 16 || key_bytes.len() != 16 {
                return Err("ClearKey: KID and key must each be 16 bytes (32 hex chars)".into());
            }
            let mut kid = [0u8; 16];
            let mut key = [0u8; 16];
            kid.copy_from_slice(&kid_bytes);
            key.copy_from_slice(&key_bytes);
            keys.insert(kid, key);
        }
        Ok(Self::new(keys))
    }

    /// Attach a resolver consulted on cache misses. May be replaced
    /// later; the cache is preserved across replacements.
    pub fn set_resolver(&self, resolver: Arc<dyn LicenseResolver>) {
        *self.resolver.lock().unwrap() = Some(resolver);
    }

    /// Merge additional pre-seeded keys into the cache (e.g. legacy
    /// `set_clearkey(HashMap)` path).
    pub fn add_keys(&self, more: HashMap<[u8; 16], [u8; 16]>) {
        self.keys.lock().unwrap().extend(more);
    }

    /// Consume this decryptor and return its key cache. Used by
    /// `Player::set_clearkey` to merge keys from a freshly-built decryptor
    /// into the shared session-wide one.
    pub fn into_keys(self) -> HashMap<[u8; 16], [u8; 16]> {
        self.keys.into_inner().unwrap_or_default()
    }

    /// Look up a key for `kid`. Returns from cache if present; otherwise
    /// calls the attached `LicenseResolver` (await-able) and caches the
    /// result. If no key is cached AND no resolver is attached, returns
    /// `Err` — surfaces as `PlayerErrorKind::LicenseResolver`.
    pub async fn ensure_key(&self, kid: [u8; 16]) -> Result<[u8; 16], BoxError> {
        if let Some(k) = self.keys.lock().unwrap().get(&kid).copied() {
            return Ok(k);
        }
        let resolver = self.resolver.lock().unwrap().clone();
        let resolver = resolver.ok_or_else(|| -> BoxError {
            format!(
                "no key cached for KID {} and no LicenseResolver attached \
                 (call set_clearkey or set_license_resolver before play)",
                kid_short(&kid)
            )
            .into()
        })?;
        let key = resolver.resolve(kid).await?;
        self.keys.lock().unwrap().insert(kid, key);
        Ok(key)
    }
}

#[async_trait::async_trait]
impl Decryptor for ClearKeyDecryptor {
    async fn ensure_key_for(&self, kid: [u8; 16]) -> Result<(), BoxError> {
        self.ensure_key(kid).await.map(|_| ())
    }

    fn decrypt_sample(
        &self,
        kid: &[u8; 16],
        iv: &[u8; 16],
        data: &mut [u8],
        subsamples: &[(u16, u32)],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        // Synchronous fast path — caller MUST have ensured the key via
        // ensure_key() upstream (e.g. once per segment when parsing tenc).
        // We don't await here because this is called per-sample on the
        // decoder hot path.
        let key = {
            let keys = self.keys.lock().unwrap();
            *keys
                .get(kid)
                .ok_or_else(|| format!("ClearKey: no key for KID {} (ensure_key not called?)", kid_short(kid)))?
        };
        // CENC applies the counter as if every PROTECTED byte of the sample
        // were contiguous — clear subsample runs do not advance it. The
        // RustCrypto path gets that for free by keeping one cipher across the
        // spans; the one-shot BoringSSL call cannot, so it gathers the
        // protected bytes, decrypts them as one stream and scatters them back.
        // Most content has a single span per sample (measured: 144 spans for
        // 144 samples), which takes the copy-free branch below.
        if use_boringssl_aes() {
            if subsamples.is_empty() {
                return boringssl_ctr(&key, iv, data);
            }
            let spans = protected_spans(subsamples, data.len())?;
            if spans.len() == 1 {
                let (a, b) = spans[0];
                return boringssl_ctr(&key, iv, &mut data[a..b]);
            }
            let total: usize = spans.iter().map(|&(a, b)| b - a).sum();
            let mut scratch = Vec::with_capacity(total);
            for &(a, b) in &spans {
                scratch.extend_from_slice(&data[a..b]);
            }
            boringssl_ctr(&key, iv, &mut scratch)?;
            let mut at = 0usize;
            for &(a, b) in &spans {
                let n = b - a;
                data[a..b].copy_from_slice(&scratch[at..at + n]);
                at += n;
            }
            return Ok(());
        }

        let mut cipher = Aes128Ctr::new(&key.into(), iv.into());

        if subsamples.is_empty() {
            cipher.apply_keystream(data);
        } else {
            for (a, b) in protected_spans(subsamples, data.len())? {
                cipher.apply_keystream(&mut data[a..b]);
            }
        }
        Ok(())
    }
}

// =================== CENC / MP4 box parsing ===================

#[derive(Debug, Clone)]
pub struct SencEntry {
    pub iv: [u8; 16],
    pub subsamples: Vec<(u16, u32)>,
}

pub struct TencInfo {
    pub default_iv_size: u8,
    pub default_kid: [u8; 16],
}

#[derive(Clone, Copy)]
pub struct AacConfig {
    pub profile: u8,
    pub freq_index: u8,
    pub chan_conf: u8,
}

/// Iterate top-level boxes in `data`, returning the body of the first box matching `target`.
pub fn find_top_box<'a>(data: &'a [u8], target: &[u8; 4]) -> Option<&'a [u8]> {
    let mut i = 0;
    while i + 8 <= data.len() {
        let size = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        let bt = &data[i + 4..i + 8];
        if size < 8 || i + size > data.len() {
            return None;
        }
        if bt == target {
            return Some(&data[i + 8..i + size]);
        }
        i += size;
    }
    None
}

/// Brute-force descendant search. Scans byte by byte for the 4-byte type and validates the size.
/// Safe to use on metadata-only regions (moov, moof) where false positives are unlikely; do **not**
/// call this over `mdat` bytes.
pub fn find_descendant<'a>(data: &'a [u8], target: &[u8; 4]) -> Option<&'a [u8]> {
    let mut i = 0;
    while i + 8 <= data.len() {
        if &data[i + 4..i + 8] == target {
            let size = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
            if size >= 8 && i + size <= data.len() {
                return Some(&data[i + 8..i + size]);
            }
        }
        i += 1;
    }
    None
}

pub fn parse_tenc(init_data: &[u8]) -> Option<TencInfo> {
    let moov = find_top_box(init_data, b"moov")?;
    let tenc = find_descendant(moov, b"tenc")?;
    if tenc.len() < 4 {
        return None;
    }
    // Two layouts exist in the wild:
    //   ISO/IEC 23001-7 second edition (older), v0: 25 bytes after FullBox header
    //     FullBox(4) + reserved(3) + isProtected(1) + iv_size(1) + KID(16)  → offsets (8, 9)
    //   ISO/IEC 23001-7 third edition (newer), v0 or v1: 24 bytes
    //     FullBox(4) + reserved(1) + [reserved/crypt_skip](1) + isProtected(1) + iv_size(1) + KID(16)  → offsets (7, 8)
    // Distinguish by total content length rather than by version, since both v0 and v1
    // of the third edition use the same byte count.
    let (iv_size_off, kid_off) = if tenc.len() >= 4 + 3 + 1 + 1 + 16 {
        (8, 9)
    } else {
        (7, 8)
    };
    if tenc.len() < kid_off + 16 {
        return None;
    }
    let iv_size = tenc[iv_size_off];
    let mut kid = [0u8; 16];
    kid.copy_from_slice(&tenc[kid_off..kid_off + 16]);
    Some(TencInfo {
        default_iv_size: iv_size,
        default_kid: kid,
    })
}

pub fn parse_senc(segment_data: &[u8], iv_size: usize) -> Option<Vec<SencEntry>> {
    let moof = find_top_box(segment_data, b"moof")?;
    let senc = find_descendant(moof, b"senc")?;

    if senc.len() < 8 {
        return None;
    }
    let version_flags = u32::from_be_bytes([senc[0], senc[1], senc[2], senc[3]]);
    let flags = version_flags & 0x00FF_FFFF;
    let has_subsamples = flags & 0x0000_0002 != 0;
    let sample_count = u32::from_be_bytes([senc[4], senc[5], senc[6], senc[7]]) as usize;

    let mut d = &senc[8..];
    let mut entries = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        if d.len() < iv_size {
            return None;
        }
        let mut iv = [0u8; 16];
        iv[..iv_size].copy_from_slice(&d[..iv_size]);
        d = &d[iv_size..];

        let subsamples = if has_subsamples {
            if d.len() < 2 {
                return None;
            }
            let n = u16::from_be_bytes([d[0], d[1]]) as usize;
            d = &d[2..];
            let mut ss = Vec::with_capacity(n);
            for _ in 0..n {
                if d.len() < 6 {
                    return None;
                }
                let clear = u16::from_be_bytes([d[0], d[1]]);
                let encrypted = u32::from_be_bytes([d[2], d[3], d[4], d[5]]);
                d = &d[6..];
                ss.push((clear, encrypted));
            }
            ss
        } else {
            Vec::new()
        };

        entries.push(SencEntry { iv, subsamples });
    }
    Some(entries)
}

/// Extract the luma bit depth from the `hvcC` box's `bitDepthLumaMinus8`
/// field (byte 17, low 3 bits). 8 for SDR HEVC Main, 10 for HDR Main 10.
/// Returns None if the box is missing or too short.
pub fn parse_hvcc_bit_depth(init_data: &[u8]) -> Option<u8> {
    let moov = find_top_box(init_data, b"moov")?;
    let hvcc = find_descendant(moov, b"hvcC")?;
    if hvcc.len() < 18 {
        return None;
    }
    Some(8 + (hvcc[17] & 0x07))
}

/// Dolby Vision decoder configuration (`dvcC` / `dvvC` box, ETSI GS CCM 001).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DoviConfig {
    pub profile: u8,
    pub level: u8,
    pub rpu_present: bool,
    pub el_present: bool,
    pub bl_present: bool,
    /// 0 = none (profile 5), 1 = HDR10, 2 = SDR, 4 = HLG, 6 = Blu-ray HDR10.
    pub bl_signal_compatibility_id: u8,
}

/// Extract the Dolby Vision configuration from the init segment. `dvcC`
/// (profiles ≤ 7) and `dvvC` (profile 8+) share the same layout:
/// version_major(8) version_minor(8) profile(7) level(6) rpu(1) el(1)
/// bl(1) compatibility_id(4) ...
pub fn parse_dovi_config(init_data: &[u8]) -> Option<DoviConfig> {
    let moov = find_top_box(init_data, b"moov")?;
    let dv = find_descendant(moov, b"dvvC").or_else(|| find_descendant(moov, b"dvcC"))?;
    if dv.len() < 4 {
        return None;
    }
    // Bytes 2..4 after the two version bytes:
    //   dv[2]: profile(7) | level high bit
    //   dv[3]: level low 5 bits | rpu | el | bl
    //   dv[4]: compatibility_id(4) | reserved
    let profile = dv[2] >> 1;
    let level = ((dv[2] & 0x01) << 5) | (dv[3] >> 3);
    let rpu_present = dv[3] & 0x04 != 0;
    let el_present = dv[3] & 0x02 != 0;
    let bl_present = dv[3] & 0x01 != 0;
    let bl_signal_compatibility_id = if dv.len() > 4 { dv[4] >> 4 } else { 0 };
    Some(DoviConfig {
        profile,
        level,
        rpu_present,
        el_present,
        bl_present,
        bl_signal_compatibility_id,
    })
}

/// Extract VPS/SPS/PPS NALUs from the `hvcC` box (HEVC decoder configuration record).
/// The raw `hvcC` box payload (HEVCDecoderConfigurationRecord, ISO/IEC
/// 14496-15 §8.3.3.1) from the init segment. Decoders that take the record
/// whole — WebCodecs' `VideoDecoderConfig.description` — consume this; the
/// NAL-array walkers above take it apart.
pub fn parse_hvcc_record(init_data: &[u8]) -> Option<Vec<u8>> {
    let moov = find_top_box(init_data, b"moov")?;
    let hvcc = find_descendant(moov, b"hvcC")?;
    (hvcc.len() >= 23).then(|| hvcc.to_vec())
}

pub fn parse_hvcc_nalus(init_data: &[u8]) -> Option<Vec<Vec<u8>>> {
    let moov = find_top_box(init_data, b"moov")?;
    let hvcc = find_descendant(moov, b"hvcC")?;
    // Fixed header is 23 bytes, then numOfArrays.
    if hvcc.len() < 23 {
        return None;
    }
    let num_arrays = hvcc[22] as usize;
    let mut d = &hvcc[23..];
    let mut out = Vec::new();
    for _ in 0..num_arrays {
        if d.len() < 3 {
            return None;
        }
        // d[0]: array_completeness(1) + reserved(1) + NAL_unit_type(6) — we don't need it
        let num_nalus = u16::from_be_bytes([d[1], d[2]]) as usize;
        d = &d[3..];
        for _ in 0..num_nalus {
            if d.len() < 2 {
                return None;
            }
            let nlen = u16::from_be_bytes([d[0], d[1]]) as usize;
            d = &d[2..];
            if d.len() < nlen {
                return None;
            }
            out.push(d[..nlen].to_vec());
            d = &d[nlen..];
        }
    }
    Some(out)
}

/// Extract AAC AudioSpecificConfig (profile, freq_index, channels) from `esds`.
pub fn parse_aac_config(init_data: &[u8]) -> Option<AacConfig> {
    let moov = find_top_box(init_data, b"moov")?;
    let esds = find_descendant(moov, b"esds")?;
    // Skip FullBox version+flags (4 bytes), then walk descriptor chain.
    if esds.len() < 4 {
        return None;
    }
    let mut d = &esds[4..];
    let inside_es = read_descriptor(&mut d, 0x03)?;
    // ES_Descriptor body: ES_ID(2) + flags(1)
    if inside_es.len() < 3 {
        return None;
    }
    let mut d = &inside_es[3..];
    let inside_dcd = read_descriptor(&mut d, 0x04)?;
    // DecoderConfigDescriptor body: object_type(1)+stream_type(1)+buffer_size(3)+max_bitrate(4)+avg_bitrate(4) = 13
    if inside_dcd.len() < 13 {
        return None;
    }
    let mut d = &inside_dcd[13..];
    let dsi = read_descriptor(&mut d, 0x05)?;
    if dsi.len() < 2 {
        return None;
    }
    let b0 = dsi[0];
    let b1 = dsi[1];
    Some(AacConfig {
        profile: b0 >> 3,
        freq_index: ((b0 & 0x07) << 1) | (b1 >> 7),
        chan_conf: (b1 >> 3) & 0x0f,
    })
}

fn read_descriptor<'a>(d: &mut &'a [u8], expected_tag: u8) -> Option<&'a [u8]> {
    if d.is_empty() || d[0] != expected_tag {
        return None;
    }
    *d = &d[1..];
    // Variable-length size (up to 4 bytes, top bit indicates continuation)
    let mut size = 0usize;
    for _ in 0..4 {
        if d.is_empty() {
            return None;
        }
        let b = d[0];
        *d = &d[1..];
        size = (size << 7) | (b & 0x7f) as usize;
        if b & 0x80 == 0 {
            break;
        }
    }
    if d.len() < size {
        return None;
    }
    let content = &d[..size];
    *d = &d[size..];
    Some(content)
}

/// Crypto state carried alongside an encrypted track through the pipeline.
#[derive(Clone)]
pub struct TrackCrypto {
    pub decryptor: Arc<dyn Decryptor>,
    pub kid: [u8; 16],
    pub iv_size: usize,
}
