#version 450
#extension GL_GOOGLE_include_directive : require

#include "common.glsl"
#include "common.frag.glsl"

layout(location = 0) in vec2 v_world_px;
layout(location = 1) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

void main() {
    float coverage = texture(glyph_atlas, v_uv).r;
    vec4 color = root.draw.color0 * coverage;
    out_color = apply_gamma(color * clip_mask(v_world_px, root.draw.clip.x));
}
