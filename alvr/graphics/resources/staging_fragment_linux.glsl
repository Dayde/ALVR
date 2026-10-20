#version 300 es

// Linux variant: the decoded frame arrives as separate luma and chroma planes (NV12 or P010), which
// are converted to RGB here.
precision highp float;

uniform sampler2D y_tex;
uniform sampler2D uv_tex;
uniform mat3 yuv_matrix;
uniform vec3 yuv_offset;

in vec2 uv;
out vec4 out_color;

void main() {
    vec3 yuv = vec3(texture(y_tex, uv).r, texture(uv_tex, uv).rg) - yuv_offset;
    out_color = vec4(clamp(yuv_matrix * yuv, 0.0, 1.0), 1.0);
}
