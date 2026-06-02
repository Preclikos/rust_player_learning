/* Bridges FFmpeg's va_list log callback to a plain C function pointer that
 * Rust can call without needing stable VaList support.
 *
 * We forward-declare the two libavutil symbols we need instead of including
 * <libavutil/log.h>, so the cc crate does not require the FFmpeg include path.
 * The symbols are resolved at link time against the libavutil already linked
 * by ffmpeg-sys-next.
 */
#include <stdarg.h>
#include <string.h>

/* Forward declarations — satisfied by libavutil at link time. */
extern int  av_log_format_line2(void *ptr, int level, const char *fmt,
                                va_list vl, char *line, int line_size,
                                int *print_prefix);
extern void av_log_set_callback(void (*cb)(void *, int, const char *, va_list));

typedef void (*rust_ffmpeg_log_fn)(int level, const char *msg, int msg_len);
static rust_ffmpeg_log_fn g_callback = NULL;

static void shim_callback(void *avcl, int level, const char *fmt, va_list args) {
    if (!g_callback) return;
    char buf[2048];
    int print_prefix = 1;
    int n = av_log_format_line2(avcl, level, fmt, args,
                                buf, (int)sizeof(buf), &print_prefix);
    if (n <= 0) return;
    int len = n < (int)sizeof(buf) ? n : (int)(sizeof(buf) - 1);
    buf[len] = '\0';
    while (len > 0 && (buf[len - 1] == '\n' || buf[len - 1] == '\r'))
        buf[--len] = '\0';
    if (len > 0) g_callback(level, buf, len);
}

void ffmpeg_log_install(rust_ffmpeg_log_fn cb) {
    g_callback = cb;
    av_log_set_callback(shim_callback);
}
