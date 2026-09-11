/* Render-thread EGL interop shared by Linux GLES and Android. Godot owns the
 * context and GL texture; only native-buffer import differs between providers. */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <GLES2/gl2ext.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef EGLClientBuffer (*NativeClientBuffer)(const void *);
typedef struct {
    EGLDisplay display;
    EGLContext context;
    GLuint texture;
    GLenum target;
    PFNEGLCREATEIMAGEKHRPROC create_image;
    PFNEGLDESTROYIMAGEKHRPROC destroy_image;
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind_image;
    PFNEGLCREATESYNCKHRPROC create_sync;
    PFNEGLDESTROYSYNCKHRPROC destroy_sync;
    PFNEGLDUPNATIVEFENCEFDANDROIDPROC dup_fence;
    int modifiers;
} WeldEgl;

static _Thread_local const char *last_error = "EGL interop failed";
const char *weld_egl_error(void) { return last_error; }
static int has_extension(const char *list, const char *name) {
    if (!list) return 0;
    size_t size = strlen(name);
    for (const char *p = list; (p = strstr(p, name)); p += size) {
        if ((p == list || p[-1] == ' ') && (p[size] == ' ' || p[size] == '\0')) return 1;
    }
    return 0;
}

void *weld_egl_open(unsigned int texture, unsigned int target) {
    EGLDisplay display = eglGetCurrentDisplay();
    EGLContext context = eglGetCurrentContext();
    last_error = "no current EGL context; select Compatibility/opengl3_es (Linux Wayland)";
    if (display == EGL_NO_DISPLAY || context == EGL_NO_CONTEXT) return NULL;
    last_error = "Godot external texture is not a GL texture";
    if (!glIsTexture(texture)) return NULL;
    const char *version = (const char *)glGetString(GL_VERSION);
    last_error = "native video requires an OpenGL ES context";
    if (!version || !strstr(version, "OpenGL ES")) return NULL;
    const char *extensions = eglQueryString(display, EGL_EXTENSIONS);
    const char *gl_extensions = (const char *)glGetString(GL_EXTENSIONS);
    const char *required[] = { "EGL_KHR_image_base", "EGL_ANDROID_native_fence_sync",
        target == GL_TEXTURE_EXTERNAL_OES ? "EGL_ANDROID_image_native_buffer" : "EGL_EXT_image_dma_buf_import" };
    for (unsigned int i = 0; i < sizeof(required) / sizeof(required[0]); ++i) {
        if (!has_extension(extensions, required[i])) { last_error = required[i]; return NULL; }
    }
    const char *gl_required = target == GL_TEXTURE_EXTERNAL_OES ? "GL_OES_EGL_image_external_essl3" : "GL_OES_EGL_image";
    if (!has_extension(gl_extensions, gl_required)) { last_error = gl_required; return NULL; }
    if (target != GL_TEXTURE_EXTERNAL_OES && target != GL_TEXTURE_2D) return NULL;
    WeldEgl *egl = calloc(1, sizeof(*egl));
    if (!egl) { last_error = "could not allocate EGL adapter"; return NULL; }
    egl->display = display; egl->context = context; egl->texture = texture; egl->target = target;
    egl->modifiers = has_extension(extensions, "EGL_EXT_image_dma_buf_import_modifiers");
    egl->create_image = (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
    egl->destroy_image = (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
    egl->bind_image = (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    egl->create_sync = (PFNEGLCREATESYNCKHRPROC)eglGetProcAddress("eglCreateSyncKHR");
    egl->destroy_sync = (PFNEGLDESTROYSYNCKHRPROC)eglGetProcAddress("eglDestroySyncKHR");
    egl->dup_fence = (PFNEGLDUPNATIVEFENCEFDANDROIDPROC)eglGetProcAddress("eglDupNativeFenceFDANDROID");
    if (!egl->create_image || !egl->destroy_image || !egl->bind_image ||
        !egl->create_sync || !egl->destroy_sync || !egl->dup_fence) {
        last_error = "required EGL entry point missing"; free(egl); return NULL;
    }
    return egl;
}
int weld_egl_current(WeldEgl *egl) {
    return egl && eglGetCurrentDisplay() == egl->display && eglGetCurrentContext() == egl->context;
}
void *weld_egl_import_android(WeldEgl *egl, void *buffer) {
    if (!weld_egl_current(egl)) return NULL;
    NativeClientBuffer client_buffer = (NativeClientBuffer)eglGetProcAddress("eglGetNativeClientBufferANDROID");
    if (!client_buffer) return NULL;
    EGLClientBuffer client = client_buffer(buffer);
    if (!client) return NULL;
    const EGLint attributes[] = { EGL_IMAGE_PRESERVED_KHR, EGL_TRUE, EGL_NONE };
    return egl->create_image(egl->display, EGL_NO_CONTEXT, EGL_NATIVE_BUFFER_ANDROID, client, attributes);
}
int weld_egl_xrgb_modifiers(WeldEgl *egl, uint64_t *output, int capacity) {
    if (!weld_egl_current(egl) || capacity < 0 || capacity > 64) return -1;
    if (!egl->modifiers) return 0;
    PFNEGLQUERYDMABUFMODIFIERSEXTPROC query = (PFNEGLQUERYDMABUFMODIFIERSEXTPROC)eglGetProcAddress("eglQueryDmaBufModifiersEXT");
    if (!query) return 0;
    EGLuint64KHR modifiers[64];
    EGLBoolean external_only[64];
    EGLint count = 0;
    if (!query(egl->display, 0x34325258, capacity, modifiers, external_only, &count)) return 0;
    int written = 0;
    for (int i = 0; i < count && i < capacity; i++) {
        if (!external_only[i]) output[written++] = modifiers[i];
    }
    return written;
}
void *weld_egl_import_dmabuf(WeldEgl *egl, int width, int height, int fd, int offset, int stride, uint64_t modifier) {
    if (!weld_egl_current(egl) || (!egl->modifiers && modifier != 0)) return NULL;
    EGLint attributes[20] = {
        EGL_WIDTH, width, EGL_HEIGHT, height,
        EGL_LINUX_DRM_FOURCC_EXT, 0x34325258,
        EGL_DMA_BUF_PLANE0_FD_EXT, fd, EGL_DMA_BUF_PLANE0_OFFSET_EXT, offset,
        EGL_DMA_BUF_PLANE0_PITCH_EXT, stride
    };
    int count = 12;
    if (egl->modifiers) {
        attributes[count++] = EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT;
        attributes[count++] = (EGLint)(uint32_t)modifier;
        attributes[count++] = EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT;
        attributes[count++] = (EGLint)(uint32_t)(modifier >> 32);
    }
    attributes[count] = EGL_NONE;
    return egl->create_image(egl->display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attributes);
}
int weld_egl_bind(WeldEgl *egl, void *image) {
    if (!weld_egl_current(egl) || !image || glGetError() != GL_NO_ERROR) return 0;
    GLint active, previous;
    glGetIntegerv(GL_ACTIVE_TEXTURE, &active);
    glActiveTexture(GL_TEXTURE0);
    glGetIntegerv(egl->target == GL_TEXTURE_EXTERNAL_OES ? GL_TEXTURE_BINDING_EXTERNAL_OES : GL_TEXTURE_BINDING_2D, &previous);
    glBindTexture(egl->target, egl->texture);
    egl->bind_image(egl->target, image);
    glTexParameteri(egl->target, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    glTexParameteri(egl->target, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    glTexParameteri(egl->target, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(egl->target, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    GLenum error = glGetError();
    glBindTexture(egl->target, (GLuint)previous);
    glActiveTexture((GLenum)active);
    return error == GL_NO_ERROR && glGetError() == GL_NO_ERROR;
}
/* Fresh owned fd or -1 FAILURE, never an implicit-completion sentinel. */
int weld_egl_release_fence(WeldEgl *egl) {
    if (!weld_egl_current(egl)) return -1;
    const EGLint attributes[] = { EGL_NONE };
    EGLSyncKHR sync = egl->create_sync(egl->display, EGL_SYNC_NATIVE_FENCE_ANDROID, attributes);
    if (sync == EGL_NO_SYNC_KHR) return -1;
    glFlush();
    if (glGetError() != GL_NO_ERROR) { egl->destroy_sync(egl->display, sync); return -1; }
    int fd = egl->dup_fence(egl->display, sync);
    egl->destroy_sync(egl->display, sync);
    return fd;
}
int weld_egl_destroy_image(WeldEgl *egl, void *image) {
    return weld_egl_current(egl) && egl->destroy_image(egl->display, image) == EGL_TRUE;
}
void weld_egl_close(WeldEgl *egl) { free(egl); }
