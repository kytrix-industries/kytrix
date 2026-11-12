// common.hlsl -- uber-quad + geometry 共享的函数与数据布局
// 规则：uniform / storage 结构体只用 16 B 成员（float4 / uint4）。

// <kytrix-constants>
#define KYTRIX_KIND_QUAD 0
#define KYTRIX_KIND_GLYPH 1
#define KYTRIX_KIND_IMAGE 2
#define KYTRIX_KIND_SHADOW 3

#define KYTRIX_FLAG_OPAQUE 1

#define KYTRIX_TEXTURE_GLYPH 0
#define KYTRIX_TEXTURE_IMAGE 1

#define KYTRIX_GRADIENT_NONE 0
#define KYTRIX_GRADIENT_HORIZONTAL 1
#define KYTRIX_GRADIENT_VERTICAL 2
#define KYTRIX_GRADIENT_RADIAL 3

#define KYTRIX_MAX_CLIP_DEPTH 8
#define KYTRIX_CLIP_VERTEX_SLACK 2.0
#define KYTRIX_SHADOW_SIGMA_DIVISOR 3.0
#define KYTRIX_SHADOW_SIGMA_MIN 0.35
// </kytrix-constants>

struct FrameUniform {
    float4 viewport;   // (w_px, h_px, 2 / w_px, 2 / h_px)
    float4 params;     // (scale, 0, 0, 0)
};

struct ClipEntry {
    float4 rect;           // (x, y, w, h) 物理像素
    float4 corner_radius;  // (tl, tr, br, bl) 物理像素
    uint4  meta;           // .x = parent（0 = 无裁剪）；.yzw 保留恒 0
};

struct GeometryDrawUniform {
    float4 basis;        // 0  (m11, m12, m21, m22)
    float4 translation;  // 16 (tx, ty, 0, 0)
    float4 color0;       // 32 premultiplied linear RGBA
    float4 clip_rect;    // 48 (x, y, w, h) 物理像素
    uint4  clip;         // 64 .x = clip_index（0 = 无裁剪）；.yzw 保留恒 0
};

// 单个 112 B 常量块：frame @ 0，draw @ 32
cbuffer Root : register(b0) { FrameUniform frame; GeometryDrawUniform draw; };

struct GpuPrimitive {
    float4 bounds;         // 0
    float4 corner_radius;  // 16
    float4 color0;         // 32
    float4 color1;         // 48
    float4 params0;        // 64
    float4 params1;        // 80
    float4 uv;             // 96
    float4 clip_rect;      // 112
};

StructuredBuffer<GpuPrimitive> instances : register(t0);
StructuredBuffer<ClipEntry> clips : register(t1);

Texture2D<float>  glyph_atlas : register(t2);
Texture2D<float4> image_atlas : register(t3);
SamplerState      atlas_sampler : register(s0);

// ---- 坐标 ----------------------------------------------------------------

// 物理像素（左上原点、Y 向下）-> D3D12 / Metal 的 NDC（Y 向上）。
float2 to_ndc(float2 world_px) {
    return float2(world_px.x * frame.viewport.z - 1.0,
                  1.0 - world_px.y * frame.viewport.w);
}

// ---- SDF 与 AA -----------------------------------------------------------

// 1 px 解析 AA：d 为像素单位的带符号距离。只在「实例级一致」的分支内调用。
float aa_coverage(float d) {
    float w = max(fwidth(d), 1e-5);
    return saturate(0.5 - d / w);
}

float sd_rounded_rect(float2 p, float2 half_size, float4 radii) {
    float r = (p.x > 0.0) ? ((p.y > 0.0) ? radii.z : radii.y)
                          : ((p.y > 0.0) ? radii.w : radii.x);
    float2 q = abs(p) - half_size + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - r;
}

// ---- 解析阴影 ------------------------------------------------------------

// Abramowitz-Stegun 7.1.26（|误差| <= 1.5e-7）。
float erf_approx(float x) {
    float ax = abs(x);
    float t = 1.0 / (1.0 + 0.3275911 * ax);
    float poly = t * (0.254829592 + t * (-0.284496736 + t * (1.421413741 +
                 t * (-1.453152027 + t * 1.061405429))));
    float e = 1.0 - poly * exp(-ax * ax);
    return (x < 0.0) ? -e : e;
}

// sigma = blur / 3：3 sigma = blur 与 params0.w = blur + 1 的外扩一致。
float analytic_shadow(float d, float blur) {
    float sigma = max(blur / KYTRIX_SHADOW_SIGMA_DIVISOR, KYTRIX_SHADOW_SIGMA_MIN);
    return 0.5 * (1.0 - erf_approx(d / (sigma * 1.41421356)));
}

// ---- 渐变 ----------------------------------------------------------------

float4 gradient_eval(uint kind, float2 p, float2 half_size, float4 c0, float4 c1) {
    float t;
    if (kind == KYTRIX_GRADIENT_HORIZONTAL) {
        t = (p.x + half_size.x) / max(2.0 * half_size.x, 1e-6);
    } else if (kind == KYTRIX_GRADIENT_VERTICAL) {
        t = (p.y + half_size.y) / max(2.0 * half_size.y, 1e-6);
    } else if (kind == KYTRIX_GRADIENT_RADIAL) {
        t = length(p) / max(min(half_size.x, half_size.y), 1e-6);
    } else {
        return c0;
    }
    return lerp(c0, c1, saturate(t));
}

// ---- clip ----------------------------------------------------------------

// clip_index == 0 = 无裁剪（表项 0 的 aabb 不是可用矩形，禁止对它做数学运算）。
float clip_mask(float2 world_px, uint clip_index) {
    if (clip_index == 0u) {
        return 1.0;
    }
    float m = 1.0;
    uint idx = clip_index;
    for (uint i = 0u; i < KYTRIX_MAX_CLIP_DEPTH && idx != 0u; ++i) {
        ClipEntry e = clips[idx];
        float2 half_size = max(e.rect.zw, 0.0) * 0.5;
        float2 center = e.rect.xy + half_size;
        m *= aa_coverage(sd_rounded_rect(world_px - center, half_size, e.corner_radius));
        idx = e.meta.x;
    }
    return m;
}

// ---- 输出 ----------------------------------------------------------------

// v1：渲染目标是 sRGB 格式，linear -> sRGB 由目标格式在写回时完成一次。
float4 apply_gamma(float4 premultiplied_linear) {
    return premultiplied_linear;
}
