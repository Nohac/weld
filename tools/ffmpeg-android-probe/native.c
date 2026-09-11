/* Diagnostic-only glue for APIs absent from ffmpeg-sys-next bindings.
 * Compiled against the same headers as the Android FFmpeg libraries. */
#include <libavutil/log.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>

static pthread_mutex_t log_lock = PTHREAD_MUTEX_INITIALIZER;
static size_t logged_bytes;
static char codec_name[256];

static void capture_log(void *context, int level, const char *format, va_list args) {
    if (level > AV_LOG_INFO) return;
    char line[1024];
    int prefix = 1;
    av_log_format_line2(context, level, format, args, line, sizeof(line), &prefix);
    pthread_mutex_lock(&log_lock);
    const char *name = strstr(line, "MediaCodec started successfully: codec = ");
    if (name) {
        name += strlen("MediaCodec started successfully: codec = ");
        size_t length = strcspn(name, ",\r\n");
        if (length < sizeof(codec_name)) {
            memcpy(codec_name, name, length);
            codec_name[length] = '\0';
        }
    }
    size_t length = strnlen(line, sizeof(line));
    if (logged_bytes + length <= 262144) {
        fwrite(line, 1, length, stderr);
        logged_bytes += length;
    }
    pthread_mutex_unlock(&log_lock);
}

void weld_probe_log_start(void) {
    pthread_mutex_lock(&log_lock);
    logged_bytes = 0;
    codec_name[0] = '\0';
    pthread_mutex_unlock(&log_lock);
    av_log_set_callback(capture_log);
}

void weld_probe_log_stop(void) { av_log_set_callback(av_log_default_callback); }

void weld_probe_codec_name(char *output, size_t size) {
    pthread_mutex_lock(&log_lock);
    if (size) snprintf(output, size, "%s", codec_name);
    pthread_mutex_unlock(&log_lock);
}
