#version 450
#extension GL_GOOGLE_include_directive : require

#include "common.glsl"

layout(location = 0) in vec2 position;
// 解析 AA 的覆盖率输入：到形状边界的有符号距离（逻辑像素，负 = 内部）。
// 网格必须沿边界向外扩 ~1 物理像素，这一段就是抗锯齿带（Skia 的 AA 三角化同理）。
layout(location = 1) in float coverage;
layout(location = 0) out vec2 v_world_px;
layout(location = 2) out float v_coverage;

void main() {
    v_world_px = geometry_world(position);
    // 逻辑像素 -> 物理像素：aa_coverage 的 d 必须是设备像素。
    v_coverage = coverage * root.frame.params.x;
    gl_Position = vec4(to_ndc(v_world_px), 0.0, 1.0);
}
