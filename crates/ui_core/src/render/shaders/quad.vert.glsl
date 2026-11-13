#version 450
#extension GL_GOOGLE_include_directive : require

#include "common.glsl"

layout(location = 0) out vec2 v_local_px;
layout(location = 1) out vec2 v_uv;
layout(location = 2) flat out uint v_instance;

void main() {
    GpuPrimitive p = instances[gl_InstanceIndex];

    uint vertex_id = uint(gl_VertexIndex);
    vec2 corner = vec2(float(vertex_id & 1u), float(vertex_id >> 1u));
    vec2 origin = p.bounds.xy;
    vec2 size = max(p.bounds.zw, vec2(0.0));
    float pad = p.params0.w;

    vec2 lo = origin - pad;
    vec2 hi = origin + size + pad;

    if (uint(p.params1.x) != 0u) {
        lo = max(lo, p.clip_rect.xy - KYTRIX_CLIP_VERTEX_SLACK);
        hi = min(hi, p.clip_rect.xy + max(p.clip_rect.zw, vec2(0.0)) + KYTRIX_CLIP_VERTEX_SLACK);
        hi = max(hi, lo);
    }

    vec2 world_px = lerp(lo, hi, corner);
    vec2 center = origin + size * 0.5;

    gl_Position = vec4(to_ndc(world_px), 0.0, 1.0);
    v_local_px = world_px - center;
    v_uv = lerp(p.uv.xy, p.uv.zw, corner);
    v_instance = uint(gl_InstanceIndex);
}
