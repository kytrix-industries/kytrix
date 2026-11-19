// quad.hlsl -- uber-quad（4 顶点 unit quad、实例化；无顶点缓冲）
#include "common.hlsl"

struct VsOut {
    float4 position : SV_Position;
    float2 local_px : TEXCOORD0;
    float2 uv : TEXCOORD1;
    nointerpolation uint instance : TEXCOORD2;
};

VsOut vs_main(uint vertex_id : SV_VertexID, uint instance_id : SV_InstanceID) {
    // 实例基址由 root constant `draw.clip.y` 显式给出，三平台一致（contract §6 / §7）。
    // 不依赖 DrawInstanced 的 StartInstanceLocation：D3D12 的 SV_InstanceID **不包含**它
    // （HLSL SM 6.8 因此新增 SV_StartInstanceLocation），而 Vulkan 的 firstInstance / Metal 的
    // baseInstance 却会加到索引上 —— 依赖硬件基址必然三平台分叉。
    uint base = (uint)draw.clip.y;
    GpuPrimitive p = instances[instance_id + base];

    float2 corner = float2((float)(vertex_id & 1u), (float)(vertex_id >> 1));
    float2 origin = p.bounds.xy;
    float2 size = max(p.bounds.zw, 0.0);
    float pad = p.params0.w;

    float2 lo = origin - pad;
    float2 hi = origin + size + pad;
    // 未夹紧的四角：UV 必须按**原始**（未夹紧）位置求比例。
    float2 full_lo = lo;
    float2 full_hi = hi;

    if ((uint)p.params1.x != 0u) {
        lo = max(lo, p.clip_rect.xy - KYTRIX_CLIP_VERTEX_SLACK);
        hi = min(hi, p.clip_rect.xy + max(p.clip_rect.zw, 0.0) + KYTRIX_CLIP_VERTEX_SLACK);
        hi = max(hi, lo);
    }

    float2 world_px = lerp(lo, hi, corner);
    float2 center = origin + size * 0.5;

    // UV 用**未夹紧**的四边形求比例（contract §7）：夹紧只是省片元，不能改变图元
    // 自身的纹理映射 —— 否则被裁剪的文本/图标会被拉伸压缩（滚动列表边缘的经典症状）。
    // 常量 alpha 图元（实色 quad）不看 UV，这条对它们没有影响。
    float2 uv_t = (world_px - full_lo) / max(full_hi - full_lo, float2(1e-6, 1e-6));

    VsOut o;
    // B6-2 合成端滚动偏移：偏移只作用在最终的屏幕映射上，local_px / uv / clip 判定全部留在
    // **烘焙坐标空间**（实例字节里与偏移无关），因此滚动帧不需要重编码任何实例。
    o.position = float4(to_ndc(world_px + frame.params.yz), 0.0, 1.0);
    o.local_px = world_px - center;
    o.uv = lerp(p.uv.xy, p.uv.zw, uv_t);
    o.instance = instance_id + base;
    return o;
}

float4 ps_main(VsOut input) : SV_Target {
    GpuPrimitive p = instances[input.instance];
    uint kind = (uint)p.params1.z;

    float2 half_size = max(p.bounds.zw, 0.0) * 0.5;
    float2 world_px = input.local_px + p.bounds.xy + half_size;

    float4 color;
    if (kind == KYTRIX_KIND_GLYPH) {
        // 字形图集恒 1 层（contract §8）：layer 硬编码 0。
        float coverage = glyph_atlas.Sample(atlas_sampler, float3(input.uv, 0.0)).r;
        color = p.color0 * coverage;
    } else if (kind == KYTRIX_KIND_IMAGE) {
        // 图片图集是 texture array：`params1.y` 是图集页（= 层号）索引（整数编码进 f32）。
        color = image_atlas.Sample(atlas_sampler, float3(input.uv, (float)(uint)p.params1.y))
                * p.color0;
    } else if (kind == KYTRIX_KIND_SHADOW) {
        // 可分离 2D 高斯（σ = blur / 2）：矩形部分是精确解，角部按圆角半径混合。
        color = p.color0 * analytic_shadow(input.local_px, half_size, p.corner_radius,
                                           p.params0.y);
    } else {
        float d_outer = sd_rounded_rect(input.local_px, half_size, p.corner_radius);
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

    color *= clip_mask(world_px, (uint)p.params1.x, (uint)p.params1.w);
    return apply_gamma(color);
}
