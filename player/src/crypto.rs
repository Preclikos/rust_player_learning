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
        let mut cipher = Aes128Ctr::new(&key.into(), iv.into());

        if subsamples.is_empty() {
            cipher.apply_keystream(data);
        } else {
            let mut offset = 0usize;
            for &(clear, encrypted) in subsamples {
                offset = offset.saturating_add(clear as usize);
                let end = offset.saturating_add(encrypted as usize);
                if end > data.len() {
                    return Err(format!(
                        "Subsample bounds ({}..{}) exceed sample length {}",
                        offset,
                        end,
                        data.len()
                    )
                    .into());
                }
                cipher.apply_keystream(&mut data[offset..end]);
                offset = end;
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

impl AacConfig {
    /// Build a 7-byte ADTS header that wraps a raw AAC frame of `frame_len` bytes.
    /// Used to make each MP4-muxed AAC sample self-describing for FFmpeg's AAC decoder.
    pub fn adts_header(&self, frame_len: usize) -> [u8; 7] {
        let total = frame_len + 7;
        let mut h = [0u8; 7];
        h[0] = 0xFF;
        h[1] = 0xF1; // syncword + MPEG-4 + layer 0 + no CRC
        h[2] = ((self.profile - 1) << 6)
            | (self.freq_index << 2)
            | ((self.chan_conf & 0x4) >> 2);
        h[3] = ((self.chan_conf & 0x3) << 6) | ((total >> 11) as u8 & 0x03);
        h[4] = ((total >> 3) & 0xFF) as u8;
        h[5] = (((total & 0x07) as u8) << 5) | 0x1F;
        h[6] = 0xFC;
        h
    }
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

pub fn parse_frma(init_data: &[u8]) -> Option<[u8; 4]> {
    let moov = find_top_box(init_data, b"moov")?;
    let frma = find_descendant(moov, b"frma")?;
    if frma.len() >= 4 {
        let mut r = [0u8; 4];
        r.copy_from_slice(&frma[..4]);
        Some(r)
    } else {
        None
    }
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

/// Extract VPS/SPS/PPS NALUs from the `hvcC` box (HEVC decoder configuration record).
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
