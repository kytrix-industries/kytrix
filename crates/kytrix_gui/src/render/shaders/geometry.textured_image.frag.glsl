#version 450
#extension GL_GOOGLE_include_directive : require

#include "common.glsl"

layout(location = 0) in vec2 v_world_px;
layout(location = 1) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

void main() {
    vec4 texel = texture(image_atlas, v_uv);
    vec4 color = texel * root.draw.color0;
    out_color = apply_gamma(color * clip_mask(v_world_px, root.draw.clip.x));
}
