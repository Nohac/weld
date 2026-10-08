/* The caller holds wgpu's GLES context lock. Convert an acquired Android image
 * into an ordinary sRGB texture, restoring every GL state touched here. */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <GLES2/gl2ext.h>
#include <stdlib.h>
#include <string.h>
#include <android/log.h>

typedef struct {
    GLuint program, vao, framebuffer, source;
    GLint crop;
    EGLDisplay display;
    EGLContext context;
    PFNEGLCREATEIMAGEKHRPROC create_image;
    PFNEGLDESTROYIMAGEKHRPROC destroy_image;
    PFNEGLGETNATIVECLIENTBUFFERANDROIDPROC client_buffer;
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind_image;
    PFNEGLCREATESYNCKHRPROC create_sync;
    PFNEGLDESTROYSYNCKHRPROC destroy_sync;
    PFNEGLDUPNATIVEFENCEFDANDROIDPROC dup_fence;
} Video;

static const char *vertex_source =
    "#version 300 es\n"
    "out vec2 uv;\n"
    "void main() { vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));"
    "uv = p; gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0); }";
static const char *fragment_source =
    "#version 300 es\n"
    "#extension GL_OES_EGL_image_external_essl3 : require\n"
    "precision highp float;\n"
    "uniform samplerExternalOES source_image; uniform vec4 crop; in vec2 uv; out vec4 color;\n"
    "void main() { vec3 c = texture(source_image, crop.xy + uv * crop.zw).rgb;"
    "vec3 linear = mix(c / 12.92, pow((c + 0.055) / 1.055, vec3(2.4)), step(vec3(0.04045), c));"
    "color = vec4(linear, 1.0); }";

/* Attribute an operation's GL errors to that operation. Preserve diagnostics
 * for an earlier context user without turning them into video failures. */
static int begin_operation(void) {
    for (int i = 0; i < 16; i++) {
        GLenum error = glGetError();
        if (error == GL_NO_ERROR) return 1;
        __android_log_print(ANDROID_LOG_WARN, "weld-mobile", "GL error before video operation: 0x%x", error);
    }
    return 0;
}

static GLuint shader(GLenum stage, const char *source) {
    GLuint s = glCreateShader(stage);
    glShaderSource(s, 1, &source, NULL);
    glCompileShader(s);
    GLint ok = 0;
    glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
    if (!ok) {
        char log[1024];
        glGetShaderInfoLog(s, sizeof(log), NULL, log);
        __android_log_print(ANDROID_LOG_ERROR, "weld-mobile", "video shader: %s", log);
        glDeleteShader(s); return 0;
    }
    return s;
}

void weld_mobile_video_close(Video *v) {
    if (!v) return;
    glDeleteProgram(v->program);
    glDeleteVertexArrays(1, &v->vao);
    glDeleteFramebuffers(1, &v->framebuffer);
    glDeleteTextures(1, &v->source);
    free(v);
}

void *weld_mobile_video_open(void) {
    Video *v = calloc(1, sizeof(*v));
    if (!v) return NULL;
    v->display = eglGetCurrentDisplay(); v->context = eglGetCurrentContext();
    if (v->display == EGL_NO_DISPLAY || v->context == EGL_NO_CONTEXT) { free(v); return NULL; }
    const char *extensions = eglQueryString(v->display, EGL_EXTENSIONS);
    if (!extensions || !strstr(extensions, "EGL_ANDROID_native_fence_sync") ||
        !strstr(extensions, "EGL_ANDROID_image_native_buffer")) { free(v); return NULL; }
    v->create_image = (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
    v->destroy_image = (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
    v->client_buffer = (PFNEGLGETNATIVECLIENTBUFFERANDROIDPROC)eglGetProcAddress("eglGetNativeClientBufferANDROID");
    v->bind_image = (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    v->create_sync = (PFNEGLCREATESYNCKHRPROC)eglGetProcAddress("eglCreateSyncKHR");
    v->destroy_sync = (PFNEGLDESTROYSYNCKHRPROC)eglGetProcAddress("eglDestroySyncKHR");
    v->dup_fence = (PFNEGLDUPNATIVEFENCEFDANDROIDPROC)eglGetProcAddress("eglDupNativeFenceFDANDROID");
    if (!v->create_image || !v->destroy_image || !v->client_buffer || !v->bind_image ||
        !v->create_sync || !v->destroy_sync || !v->dup_fence) { free(v); return NULL; }
    GLuint vertex = shader(GL_VERTEX_SHADER, vertex_source);
    GLuint fragment = shader(GL_FRAGMENT_SHADER, fragment_source);
    if (!vertex || !fragment) { glDeleteShader(vertex); glDeleteShader(fragment); free(v); return NULL; }
    v->program = glCreateProgram();
    glAttachShader(v->program, vertex); glAttachShader(v->program, fragment);
    glLinkProgram(v->program);
    glDeleteShader(vertex); glDeleteShader(fragment);
    GLint linked = 0;
    glGetProgramiv(v->program, GL_LINK_STATUS, &linked);
    if (!linked) { weld_mobile_video_close(v); return NULL; }
    v->crop = glGetUniformLocation(v->program, "crop");
    glGenVertexArrays(1, &v->vao);
    glGenFramebuffers(1, &v->framebuffer);
    glGenTextures(1, &v->source);
    return v;
}

unsigned int weld_mobile_video_texture(int width, int height) {
    if (!begin_operation()) return 0;
    GLint previous;
    glGetIntegerv(GL_TEXTURE_BINDING_2D, &previous);
    GLuint texture;
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_SRGB8_ALPHA8, width, height);
    glBindTexture(GL_TEXTURE_2D, previous);
    if (glGetError() != GL_NO_ERROR) { glDeleteTextures(1, &texture); return 0; }
    return texture;
}

/* >=0 transfers a native fence FD covering the source read. -2 means a valid
 * draw completed synchronously after fence export failed. On failure, any
 * submitted source reads have completed before -1 returns. */
int weld_mobile_video_draw(Video *v, void *buffer, unsigned int target,
                           int width, int height, const float *crop) {
    if (eglGetCurrentContext() != v->context || eglGetCurrentDisplay() != v->display) return -1;
    if (!begin_operation()) return -1;
    const EGLint attributes[] = { EGL_IMAGE_PRESERVED_KHR, EGL_TRUE, EGL_NONE };
    EGLClientBuffer client = v->client_buffer(buffer);
    if (!client) return -1;
    EGLImageKHR image = v->create_image(v->display, EGL_NO_CONTEXT, EGL_NATIVE_BUFFER_ANDROID, client, attributes);
    if (image == EGL_NO_IMAGE_KHR) return -1;

    GLint program, vao, draw, read, viewport[4], active, external, sampler;
    GLboolean mask[4];
    const GLenum capabilities[] = {GL_BLEND, GL_DEPTH_TEST, GL_STENCIL_TEST, GL_SCISSOR_TEST,
                                   GL_CULL_FACE, GL_RASTERIZER_DISCARD, GL_DITHER};
    GLboolean enabled[7];
    for (int i = 0; i < 7; i++) { enabled[i] = glIsEnabled(capabilities[i]); glDisable(capabilities[i]); }
    glGetIntegerv(GL_CURRENT_PROGRAM, &program);
    glGetIntegerv(GL_VERTEX_ARRAY_BINDING, &vao);
    glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &draw);
    glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &read);
    glGetIntegerv(GL_VIEWPORT, viewport);
    glGetBooleanv(GL_COLOR_WRITEMASK, mask);
    glGetIntegerv(GL_ACTIVE_TEXTURE, &active);
    glActiveTexture(GL_TEXTURE0);
    glGetIntegerv(GL_TEXTURE_BINDING_EXTERNAL_OES, &external);
    glGetIntegerv(GL_SAMPLER_BINDING, &sampler);

    glBindSampler(0, 0);
    glBindTexture(GL_TEXTURE_EXTERNAL_OES, v->source);
    v->bind_image(GL_TEXTURE_EXTERNAL_OES, image);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, v->framebuffer);
    glFramebufferTexture2D(GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, target, 0);
    int valid = glCheckFramebufferStatus(GL_DRAW_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE;
    glViewport(0, 0, width, height);
    glColorMask(GL_TRUE, GL_TRUE, GL_TRUE, GL_TRUE);
    glUseProgram(v->program);
    glUniform1i(glGetUniformLocation(v->program, "source_image"), 0);
    glUniform4fv(v->crop, 1, crop);
    glBindVertexArray(v->vao);
    if (valid) glDrawArrays(GL_TRIANGLES, 0, 3);
    valid = valid && glGetError() == GL_NO_ERROR;
    glFramebufferTexture2D(GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, 0, 0);

    int fd = -1;
    if (valid) {
        EGLSyncKHR fence = v->create_sync(v->display, EGL_SYNC_NATIVE_FENCE_ANDROID, NULL);
        if (fence != EGL_NO_SYNC_KHR) {
            glFlush(); fd = v->dup_fence(v->display, fence);
            v->destroy_sync(v->display, fence);
        }
    }
    if (fd < 0) {
        glFinish();
        if (valid && glGetError() == GL_NO_ERROR) fd = -2;
    }
    v->destroy_image(v->display, image);
    glBindVertexArray(vao);
    glUseProgram(program);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, draw);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, read);
    glViewport(viewport[0], viewport[1], viewport[2], viewport[3]);
    glColorMask(mask[0], mask[1], mask[2], mask[3]);
    glBindTexture(GL_TEXTURE_EXTERNAL_OES, external);
    glBindSampler(0, sampler);
    glActiveTexture(active);
    for (int i = 0; i < 7; i++) if (enabled[i]) glEnable(capabilities[i]);
    return fd;
}
