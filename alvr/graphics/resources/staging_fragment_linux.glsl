#version 300 es

// Desktop Linux variant of staging_fragment.glsl. The decoded frame arrives as separate luma and
// chroma planes (NV12 or P010 dma-bufs) which are converted to RGB here, with the matrix and
// offsets of the stream's colour space and range.

precision highp float;

uniform sampler2D luma_tex;
uniform sampler2D chroma_tex;
uniform mat3 yuv_matrix;
uniform vec3 yuv_offset;
// Moves the chroma coordinates to the chroma sample location of the stream (left sited for H.264
// and HEVC), instead of the centered location assumed by sampling with the luma coordinates
uniform vec2 chroma_offset;

in vec2 uv;
out vec4 out_color;

void main() {
    vec3 yuv = vec3(texture(luma_tex, uv).r, texture(chroma_tex, uv + chroma_offset).rg) -
        yuv_offset;
    out_color = vec4(clamp(yuv_matrix * yuv, 0.0, 1.0), 1.0);
}
