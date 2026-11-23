#include "common.metal"

struct QuadVsOut {
    float4 position [[position]];
    float2 local_px;
    float2 uv;
    uint   instance [[flat]];
};

vertex QuadVsOut vs_main(uint vertex_id [[vertex_id]],
                         uint instance_id [[instance_id]],
                         device const GpuPrimitive* instances [[buffer(0)]],
                         constant RootUniform& root [[buffer(2)]]) {
    uint base = uint(root.draw.clip.y);
    uint instance = instance_id + base;
    GpuPrimitive p = instances[instance];

    float2 corner = float2(float(vertex_id & 1u), float(vertex_id >> 1u));
    float2 origin = p.bounds.xy;
    float2 size = max(p.bounds.zw, 0.0);
    float pad = p.params0.w;

    float2 lo = origin - pad;
    float2 hi = origin + size + pad;
    float2 full_lo = lo;
    float2 full_hi = hi;

    if (uint(p.params1.x) != 0u) {
        lo = max(lo, p.clip_rect.xy - KYTRIX_CLIP_VERTEX_SLACK);
        hi = min(hi, p.clip_rect.xy + max(p.clip_rect.zw, 0.0) + KYTRIX_CLIP_VERTEX_SLACK);
        hi = max(hi, lo);
    }

    float2 world_px = mix(lo, hi, corner);
    float2 center = origin + size * 0.5;
    float2 uv_t = (world_px - full_lo) / max(full_hi - full_lo, float2(1e-6));

    QuadVsOut o;
    o.position = float4(to_ndc(world_px + root.frame.params.yz, root.frame), 0.0, 1.0);
    o.local_px = world_px - center;
    o.uv = mix(p.uv.xy, p.uv.zw, uv_t);
    o.instance = instance;
    return o;
}

fragment float4 ps_main(QuadVsOut in [[stage_in]],
                        device const GpuPrimitive* instances [[buffer(0)]],
                        device const ClipEntry* clips [[buffer(1)]],
                        texture2d_array<float> glyph_atlas [[texture(0)]],
                        texture2d_array<float> image_atlas [[texture(1)]],
                        sampler atlas_sampler [[sampler(0)]]) {
    GpuPrimitive p = instances[in.instance];
    uint kind = uint(p.params1.z);

    float2 half_size = max(p.bounds.zw, 0.0) * 0.5;
    float2 world_px = in.local_px + p.bounds.xy + half_size;

    float4 color;
    if (kind == KYTRIX_KIND_GLYPH) {
        float coverage = glyph_atlas.sample(atlas_sampler, in.uv, 0u).r;
        color = p.color0 * coverage;
    } else if (kind == KYTRIX_KIND_IMAGE) {
        color = image_atlas.sample(atlas_sampler, in.uv, uint(p.params1.y)) * p.color0;
    } else if (kind == KYTRIX_KIND_SHADOW) {
        color = p.color0 * analytic_shadow(in.local_px, half_size, p.corner_radius,
                                          p.params0.y);
    } else {
        float d_outer = sd_rounded_rect(in.local_px, half_size, p.corner_radius);
        float coverage = aa_coverage(d_outer);
        uint gradient = uint(p.params0.z);
        float4 fill = (gradient == KYTRIX_GRADIENT_NONE)
                          ? p.color0
                          : gradient_eval(gradient, in.local_px, half_size,
                                          p.color0, p.color1);
        color = fill * coverage;
        float border = p.params0.x;
        if (border > 0.0) {
            float4 inner_radii = max(p.corner_radius - border, 0.0);
            float2 inner_half = max(half_size - border, 0.0);
            float inner = aa_coverage(sd_rounded_rect(in.local_px, inner_half, inner_radii));
            float ring = max(coverage - inner, 0.0);
            color += (p.color1 - fill * p.color1.a) * ring;
        }
    }

    color *= clip_mask(world_px, uint(p.params1.x), uint(p.params1.w), clips);
    return apply_gamma(color);
}
