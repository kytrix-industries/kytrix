// quad.hlsl -- uber-quad（4 顶点 unit quad、实例化；无顶点缓冲）
#include "common.hlsl"

struct VsOut {
    float4 position : SV_Position;
    float2 local_px : TEXCOORD0;
    float2 uv : TEXCOORD1;
    nointerpolation uint instance : TEXCOORD2;
};

VsOut vs_main(uint vertex_id : SV_VertexID, uint instance_id : SV_InstanceID) {
    GpuPrimitive p = instances[instance_id];

    float2 corner = float2((float)(vertex_id & 1u), (float)(vertex_id >> 1));
    float2 origin = p.bounds.xy;
    float2 size = max(p.bounds.zw, 0.0);
    float pad = p.params0.w;

    float2 lo = origin - pad;
    float2 hi = origin + size + pad;

    if ((uint)p.params1.x != 0u) {
        lo = max(lo, p.clip_rect.xy - KYTRIX_CLIP_VERTEX_SLACK);
        hi = min(hi, p.clip_rect.xy + max(p.clip_rect.zw, 0.0) + KYTRIX_CLIP_VERTEX_SLACK);
        hi = max(hi, lo);
    }

    float2 world_px = lerp(lo, hi, corner);
    float2 center = origin + size * 0.5;

    VsOut o;
    o.position = float4(to_ndc(world_px), 0.0, 1.0);
    o.local_px = world_px - center;
    o.uv = lerp(p.uv.xy, p.uv.zw, corner);
    o.instance = instance_id;
    return o;
}

float4 ps_main(VsOut input) : SV_Target {
    GpuPrimitive p = instances[input.instance];
    uint kind = (uint)p.params1.z;

    float2 half_size = max(p.bounds.zw, 0.0) * 0.5;
    float2 world_px = input.local_px + p.bounds.xy + half_size;

    float4 color;
    if (kind == KYTRIX_KIND_GLYPH) {
        float coverage = glyph_atlas.Sample(atlas_sampler, input.uv).r;
        color = p.color0 * coverage;
    } else if (kind == KYTRIX_KIND_IMAGE) {
        color = image_atlas.Sample(atlas_sampler, input.uv) * p.color0;
    } else {
        float d_outer = sd_rounded_rect(input.local_px, half_size, p.corner_radius);
        if (kind == KYTRIX_KIND_SHADOW) {
            color = p.color0 * analytic_shadow(d_outer, p.params0.y);
        } else {
            float coverage = aa_coverage(d_outer);
            uint gradient = (uint)p.params0.z;
            float4 fill = (gradient == KYTRIX_GRADIENT_NONE)
                              ? p.color0
                              : gradient_eval(gradient, input.local_px, half_size,
                                              p.color0, p.color1);
            color = fill * coverage;
            float border = p.params0.x;
            if (border > 0.0) {
                float4 inner_radii = max(p.corner_radius - border, 0.0);
                float2 inner_half = max(half_size - border, 0.0);
                float inner = aa_coverage(sd_rounded_rect(input.local_px, inner_half, inner_radii));
                float ring = max(coverage - inner, 0.0);
                color += (p.color1 - fill * p.color1.a) * ring;
            }
        }
    }

    color *= clip_mask(world_px, (uint)p.params1.x);
    return apply_gamma(color);
}
