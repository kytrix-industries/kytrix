// quad.metal -- uber-quad（4 顶点 unit quad、实例化；无顶点缓冲）。
// 契约：src/render/shaders/contract.md；入口：vs_main / ps_main（产物 quad.metallib）。

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
    // 实例基址由 root constant `draw.clip.y` 显式给出，三平台一致（contract §6 / §7）。
    // 不依赖 baseInstance：Metal 的 instance_id 会包含它，而 D3D12 的 SV_InstanceID 不会。
    uint base = uint(root.draw.clip.y);
    uint instance = instance_id + base;
    GpuPrimitive p = instances[instance];

    float2 corner = float2(float(vertex_id & 1u), float(vertex_id >> 1u));
    float2 origin = p.bounds.xy;
    float2 size = max(p.bounds.zw, 0.0);
    float pad = p.params0.w;

    float2 lo = origin - pad;
    float2 hi = origin + size + pad;
    // 未夹紧的四角：UV 必须按**原始**（未夹紧）位置求比例（contract §7）。
    float2 full_lo = lo;
    float2 full_hi = hi;

    if (uint(p.params1.x) != 0u) {
        lo = max(lo, p.clip_rect.xy - KYTRIX_CLIP_VERTEX_SLACK);
        hi = min(hi, p.clip_rect.xy + max(p.clip_rect.zw, 0.0) + KYTRIX_CLIP_VERTEX_SLACK);
        hi = max(hi, lo);
    }

    float2 world_px = mix(lo, hi, corner);
    float2 center = origin + size * 0.5;
    // 夹紧只是省片元，不能改变图元自身的纹理映射 —— 否则被裁剪的文本/图标会被拉伸
    // 压缩（滚动列表边缘的经典症状）。
    float2 uv_t = (world_px - full_lo) / max(full_hi - full_lo, float2(1e-6));

    QuadVsOut o;
    // B6-2 合成端滚动偏移：只作用在最终的屏幕映射上，local_px / uv / clip 判定留在烘焙坐标空间。
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
        // 字形图集恒 1 层（contract §8）：slice 硬编码 0。
        float coverage = glyph_atlas.sample(atlas_sampler, in.uv, 0u).r;
        color = p.color0 * coverage;
    } else if (kind == KYTRIX_KIND_IMAGE) {
        // 图片图集是 texture array：`params1.y` 是图集页（= 层号）索引（整数编码进 f32）。
        color = image_atlas.sample(atlas_sampler, in.uv, uint(p.params1.y)) * p.color0;
    } else if (kind == KYTRIX_KIND_SHADOW) {
        // 可分离 2D 高斯（σ = blur / 2）：矩形部分是精确解，角部按圆角半径混合。
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
