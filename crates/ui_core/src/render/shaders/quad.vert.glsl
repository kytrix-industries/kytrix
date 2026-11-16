#version 450
#extension GL_GOOGLE_include_directive : require

#include "common.glsl"

layout(location = 0) out vec2 v_local_px;
layout(location = 1) out vec2 v_uv;
layout(location = 2) flat out uint v_instance;

void main() {
    // 实例基址由 push constant `root.draw.clip.y` 显式给出，三平台一致（contract §6 / §7）。
    // 不依赖 firstInstance：gl_InstanceIndex 会包含它，而 D3D12 的 SV_InstanceID 不会。
    uint base = uint(root.draw.clip.y);
    uint instance = uint(gl_InstanceIndex) + base;
    GpuPrimitive p = instances[instance];

    uint vertex_id = uint(gl_VertexIndex);
    vec2 corner = vec2(float(vertex_id & 1u), float(vertex_id >> 1u));
    vec2 origin = p.bounds.xy;
    vec2 size = max(p.bounds.zw, vec2(0.0));
    float pad = p.params0.w;

    vec2 lo = origin - pad;
    vec2 hi = origin + size + pad;
    // 未夹紧的四角：UV 必须按**原始**（未夹紧）位置求比例（contract §7）。
    vec2 full_lo = lo;
    vec2 full_hi = hi;

    if (uint(p.params1.x) != 0u) {
        lo = max(lo, p.clip_rect.xy - KYTRIX_CLIP_VERTEX_SLACK);
        hi = min(hi, p.clip_rect.xy + max(p.clip_rect.zw, vec2(0.0)) + KYTRIX_CLIP_VERTEX_SLACK);
        hi = max(hi, lo);
    }

    vec2 world_px = lerp(lo, hi, corner);
    vec2 center = origin + size * 0.5;
    // 夹紧只是省片元，不能改变图元自身的纹理映射 —— 否则被裁剪的文本/图标会被拉伸
    // 压缩（滚动列表边缘的经典症状）。
    vec2 uv_t = (world_px - full_lo) / max(full_hi - full_lo, vec2(1e-6));

    gl_Position = vec4(to_ndc(world_px), 0.0, 1.0);
    v_local_px = world_px - center;
    v_uv = lerp(p.uv.xy, p.uv.zw, uv_t);
    v_instance = instance;
}
