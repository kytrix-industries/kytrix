#version 450
#extension GL_GOOGLE_include_directive : require

#include "common.glsl"
#include "common.frag.glsl"

layout(location = 0) in vec2 v_world_px;
layout(location = 1) in vec2 v_uv;
layout(location = 0) out vec4 out_color;

void main() {
    // 应用注册的静态纹理几何没有页概念：固定采样 layer 0（contract §8）。
    vec4 texel = texture(image_atlas, vec3(v_uv, 0.0));
    vec4 color = texel * root.draw.color0;
    out_color = apply_gamma(color * clip_mask(v_world_px, uint(root.draw.clip.x),
                                              KYTRIX_MAX_CLIP_ITEMS));
}
