#version 450
#extension GL_GOOGLE_include_directive : require

#include "common.glsl"
#include "common.frag.glsl"

layout(location = 0) in vec2 v_world_px;
layout(location = 1) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

void main() {
    // 字形图集恒 1 层：固定采样 layer 0。
    float coverage = texture(glyph_atlas, vec3(v_uv, 0.0)).r;
    vec4 color = root.draw.color0 * coverage;
    out_color = apply_gamma(color * clip_mask(v_world_px, uint(root.draw.clip.x),
                                              KYTRIX_MAX_CLIP_ITEMS));
}
