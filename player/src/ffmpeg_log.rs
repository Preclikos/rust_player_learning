//! Sets FFmpeg's libav* verbosity threshold and — on Linux — routes
//! the messages through Rust's `log` crate so downstream binaries
//! (Blackzone Console etc.) can subscribe without depending on
//! ffmpeg-sys-next directly.
//!
//! Public surface: `LogLevel` + `set_log_level(...)`. Re-exported at
//! the `player` crate root. The forwarding callback is idempotent —
//! re-calling `set_log_level` only changes the verbosity threshold;
//! `av_log_set_callback` is wired up once via `std::sync::Once`.
//!
//! Messages are emitted with `target: "ffmpeg"`, so consumers can
//! route them independently of the player's own logs.
//!
//! ## Platform support
//!
//! `av_log_set_level` is cross-platform — it takes a plain `c_int` and
//! works on every target where ffmpeg-sys-next links.
//!
//! `av_log_set_callback`, however, exposes a `va_list` parameter whose
//! bindgen-generated Rust type varies per platform:
//!
//!   - Linux: `*mut sys::__va_list_tag` (struct emitted by bindgen)
//!   - Windows MSVC/MinGW: `va_list` ≈ `*mut c_char`, no struct exists
//!   - macOS bindgen output: no `__va_list_tag` symbol at all
//!
//! Bridging that portably requires either a small C shim (build.rs +
//! cc crate) or `core::ffi::VaList`, which is still nightly-only.
//! Until one of those lands, we install the forwarding callback **on
//! Linux only**. Windows + macOS still get level control and silent
//! categories — their FFmpeg output goes to the libav* default
//! callback (stderr), not through Rust's `log` facade.

#[derive(Copy, Clone, Debug)]
pub enum LogLevel {
    Quiet,
    Fatal,
    Error,
    Warning,
    Info,
    Verbose,
    Debug,
    Trace,
}

/// FFmpeg at AV_LOG_VERBOSE dumps the entire list of D3D11VA decoder GUIDs
/// the GPU driver advertises (~70 lines per decoder open, plus a few profile
/// numbers under each). It's noise for normal triage — we keep VERBOSE
/// otherwise so D3D11VA HRESULTs surface — so downgrade just these lines.
///
/// Returns true if the line is a GUID-dump artifact and should be hidden
/// (we still emit it at TRACE so a sufficiently verbose run can recover it).
#[cfg(any(target_os = "windows", target_os = "linux"))]
fn is_dxva_guid_dump(msg: &str) -> bool {
    let body = msg.split_once("] ").map(|(_, b)| b).unwrap_or(msg);
    let body = body.trim();
    if body.is_empty() {
        return false;
    }
    if body == "Decoder GUIDs reported as supported:" {
        return true;
    }
    // GUID lines: "{8-4-4-4-12 hex}" (length 38 with braces).
    if body.len() == 38
        && body.starts_with('{')
        && body.ends_with('}')
        && body[1..37]
            .bytes()
            .all(|c| c.is_ascii_hexdigit() || c == b'-')
    {
        return true;
    }
    // Profile-number lines under each GUID — short whitespace-padded digits
    // like " 103" / " 106 107".
    if body.len() <= 32 && body.bytes().all(|c| c.is_ascii_digit() || c == b' ') {
        return true;
    }
    false
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
mod imp {
    use super::LogLevel;
    use ffmpeg_sys_next as sys;
    use std::ffi::c_int;

    impl LogLevel {
        fn to_av(self) -> c_int {
            match self {
                LogLevel::Quiet => sys::AV_LOG_QUIET,
                LogLevel::Fatal => sys::AV_LOG_FATAL,
                LogLevel::Error => sys::AV_LOG_ERROR,
                LogLevel::Warning => sys::AV_LOG_WARNING,
                LogLevel::Info => sys::AV_LOG_INFO,
                LogLevel::Verbose => sys::AV_LOG_VERBOSE,
                LogLevel::Debug => sys::AV_LOG_DEBUG,
                LogLevel::Trace => sys::AV_LOG_TRACE,
            }
        }
    }

    pub fn set_log_level(level: LogLevel) {
        unsafe { sys::av_log_set_level(level.to_av()) };
        #[cfg(target_os = "linux")]
        super::linux_forwarder::install_once();
        #[cfg(target_os = "windows")]
        super::windows_forwarder::install_once();
    }
}

#[cfg(target_os = "linux")]
mod linux_forwarder {
    use ffmpeg_sys_next as sys;
    use std::ffi::{c_char, c_int, c_void};
    use std::sync::Once;

    pub fn install_once() {
        static INIT: Once = Once::new();
        INIT.call_once(|| unsafe { sys::av_log_set_callback(Some(forward)) });
    }

    // FFmpeg invokes this from arbitrary threads (decoder workers, I/O
    // callbacks, internal helpers). Everything inside must be Send-safe;
    // `log::log!` and `Once` are, and we touch no shared mutable state.
    unsafe extern "C" fn forward(
        avcl: *mut c_void,
        level: c_int,
        fmt: *const c_char,
        args: *mut sys::__va_list_tag,
    ) {
        let mut buf = [0u8; 2048];
        let mut prefix: c_int = 1;
        let n = unsafe {
            sys::av_log_format_line2(
                avcl,
                level,
                fmt,
                args,
                buf.as_mut_ptr() as *mut c_char,
                buf.len() as c_int,
                &mut prefix,
            )
        };
        if n <= 0 {
            return;
        }
        let len = (n as usize).min(buf.len() - 1);
        let msg = match std::str::from_utf8(&buf[..len]) {
            Ok(s) => s.trim_end(),
            Err(_) => return,
        };
        if msg.is_empty() {
            return;
        }
        let mut rust_lvl = if level <= sys::AV_LOG_ERROR {
            log::Level::Error
        } else if level <= sys::AV_LOG_WARNING {
            log::Level::Warn
        } else if level <= sys::AV_LOG_INFO {
            log::Level::Info
        } else if level <= sys::AV_LOG_VERBOSE {
            log::Level::Debug
        } else {
            log::Level::Trace
        };
        if super::is_dxva_guid_dump(msg) {
            rust_lvl = log::Level::Trace;
        }
        log::log!(target: "ffmpeg", rust_lvl, "{}", msg);
    }
}

#[cfg(target_os = "windows")]
mod windows_forwarder {
    use ffmpeg_sys_next as sys;
    use std::ffi::{c_char, c_int};
    use std::sync::Once;

    extern "C" {
        fn ffmpeg_log_install(cb: unsafe extern "C" fn(c_int, *const c_char, c_int));
    }

    pub fn install_once() {
        static INIT: Once = Once::new();
        INIT.call_once(|| unsafe { ffmpeg_log_install(forward) });
    }

    unsafe extern "C" fn forward(level: c_int, msg: *const c_char, len: c_int) {
        let bytes = unsafe { std::slice::from_raw_parts(msg as *const u8, len as usize) };
        let s = match std::str::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut rust_lvl = if level <= sys::AV_LOG_ERROR {
            log::Level::Error
        } else if level <= sys::AV_LOG_WARNING {
            log::Level::Warn
        } else if level <= sys::AV_LOG_INFO {
            log::Level::Info
        } else if level <= sys::AV_LOG_VERBOSE {
            log::Level::Debug
        } else {
            log::Level::Trace
        };
        if super::is_dxva_guid_dump(s) {
            rust_lvl = log::Level::Trace;
        }
        log::log!(target: "ffmpeg", rust_lvl, "{}", s);
    }
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub use imp::set_log_level;

// On platforms without ffmpeg-sys-next (iOS, Android — native decoders),
// expose a no-op so consumers can call `player::set_log_level(...)`
// unconditionally without cfg gates.
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn set_log_level(_level: LogLevel) {}
