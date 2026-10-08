/* Reuse Weld's leased EGL import and state restoration with encoded-colour
 * output for Vello's unorm composition target. Called on the render thread. */
#include "android_video.c"

void *weld_probe_video_open(void) {
    const char *original = fragment_source;
    fragment_source =
        "#version 300 es\n"
        "#extension GL_OES_EGL_image_external_essl3 : require\n"
        "precision highp float;\n"
        "uniform samplerExternalOES source_image; uniform vec4 crop; in vec2 uv; out vec4 color;\n"
        "void main() { color = vec4(texture(source_image, crop.xy + uv * crop.zw).rgb, 1.0); }";
    void *converter = weld_mobile_video_open();
    fragment_source = original;
    return converter;
}

unsigned int weld_probe_video_texture(int width, int height) {
    if (!begin_operation()) return 0;
    GLint previous; glGetIntegerv(GL_TEXTURE_BINDING_2D, &previous);
    GLuint texture; glGenTextures(1, &texture); glBindTexture(GL_TEXTURE_2D, texture);
    glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, width, height);
    glBindTexture(GL_TEXTURE_2D, previous);
    if (glGetError() != GL_NO_ERROR) { glDeleteTextures(1, &texture); return 0; }
    return texture;
}
