#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <GLES2/gl2ext.h>
#include <stdint.h>
#include <stddef.h>

/* The caller holds WGPU's current EGL context and owns the DMA-BUF lease. */
unsigned int weld_probe_import(int width, int height, int fd, int offset, int stride, uint64_t modifier) {
    for (int i = 0; i < 16; i++) {
        if (glGetError() == GL_NO_ERROR) break;
        if (i == 15) return 0;
    }
    EGLDisplay display = eglGetCurrentDisplay();
    PFNEGLCREATEIMAGEKHRPROC create = (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
    PFNEGLDESTROYIMAGEKHRPROC destroy = (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC bind = (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress("glEGLImageTargetTexture2DOES");
    if (display == EGL_NO_DISPLAY || !create || !destroy || !bind || modifier != 0) return 0;
    const EGLint attributes[] = { EGL_WIDTH, width, EGL_HEIGHT, height,
        EGL_LINUX_DRM_FOURCC_EXT, 0x34325258,
        EGL_DMA_BUF_PLANE0_FD_EXT, fd, EGL_DMA_BUF_PLANE0_OFFSET_EXT, offset,
        EGL_DMA_BUF_PLANE0_PITCH_EXT, stride, EGL_NONE };
    EGLImageKHR image = create(display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attributes);
    if (image == EGL_NO_IMAGE_KHR) return 0;
    GLint previous; glGetIntegerv(GL_TEXTURE_BINDING_2D, &previous);
    GLuint texture; glGenTextures(1, &texture); glBindTexture(GL_TEXTURE_2D, texture);
    bind(GL_TEXTURE_2D, image);
    GLenum error = glGetError();
    glBindTexture(GL_TEXTURE_2D, previous); destroy(display, image);
    if (error != GL_NO_ERROR) { glDeleteTextures(1, &texture); return 0; }
    return texture;
}
