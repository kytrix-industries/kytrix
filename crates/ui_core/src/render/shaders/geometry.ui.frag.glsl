#version 450
#extension GL_GOOGLE_include_directive : require

#include "common.glsl"
#include "common.frag.glsl"

layout(location = 0) in vec2 v_world_px;
layout(location = 2) in float v_coverage;
layout(location = 0) out vec4 out_color;

void main() {
    // 解析 AA：覆盖率由**顶点距离插值**给出，不依赖 MSAA / 后处理（三后端一致）。
    float coverage = aa_coverage(v_coverage);
    vec4 color = root.draw.color0 * coverage;
    out_color = apply_gamma(color * clip_mask(v_world_px, uint(root.draw.clip.x),
                                              KYTRIX_MAX_CLIP_DEPTH));
}
