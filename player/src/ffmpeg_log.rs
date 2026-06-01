//! Routes FFmpeg's libav* diagnostics through Rust's `log` crate so
//! downstream binaries (Blackzone Console etc.) can consume them via the
//! usual env_logger / tracing-log subscriber without depending on
//! ffmpeg-sys-next directly.
//!
//! Public surface: `LogLevel` + `set_log_level(...)`. Re-exported at the
//! `player` crate root. The forwarder is idempotent — calling
//! `set_log_level` again only changes the verbosity threshold; the
//! av_log_set_callback registration happens once via `std::sync::Once`.
//!
//! Messages are emitted with `target: "ffmpeg"`, so consumers can route
//! them independently of the player's own logs.

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

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
mod imp {
    use super::LogLevel;
    use ffmpeg_sys_next as sys;
    use std::ffi::{c_char, c_int, c_void};
    use std::sync::Once;

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

    static INIT: Once = Once::new();

    pub fn set_log_level(level: LogLevel) {
        INIT.call_once(|| unsafe {
            sys::av_log_set_callback(Some(forward));
        });
        unsafe { sys::av_log_set_level(level.to_av()) };
    }

    // FFmpeg invokes this from arbitrary threads (decoder workers, I/O
    // callbacks, internal helpers). Everything inside must be Send-safe.
    // `log::log!` and `Once` are; we don't touch any shared mutable state.
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
        let rust_lvl = if level <= sys::AV_LOG_ERROR {
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
        log::log!(target: "ffmpeg", rust_lvl, "{}", msg);
    }
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub use imp::set_log_level;

// On platforms without ffmpeg-sys-next (iOS, Android — native decoders),
// expose a no-op so consumers can call `player::set_log_level(...)`
// unconditionally without cfg gates.
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
pub fn set_log_level(_level: LogLevel) {}
