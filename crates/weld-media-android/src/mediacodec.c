/* The release API is not included in ffmpeg-sys-next's generated bindings. */
#include <libavcodec/mediacodec.h>
#include <libavutil/frame.h>
#include <libavutil/pixfmt.h>
#include <errno.h>

int weld_mediacodec_render_frame(AVFrame *frame) {
    if (!frame || frame->format != AV_PIX_FMT_MEDIACODEC || !frame->data[3])
        return AVERROR(EINVAL);
    return av_mediacodec_release_buffer((AVMediaCodecBuffer *)frame->data[3], 1);
}
