// common.hlsl -- uber-quad + geometry 共享的函数与数据布局
// 规则：uniform / storage 结构体只用 16 B 成员（float4 / uint4）。
// 与 common.metal / common.glsl 逐行等价；契约：src/render/shaders/contract.md

// <kytrix-constants>
#define KYTRIX_KIND_QUAD 0
#define KYTRIX_KIND_GLYPH 1
#define KYTRIX_KIND_IMAGE 2
#define KYTRIX_KIND_SHADOW 3

#define KYTRIX_TEXTURE_GLYPH 0
#define KYTRIX_TEXTURE_IMAGE 1

#define KYTRIX_GRADIENT_NONE 0
#define KYTRIX_GRADIENT_HORIZONTAL 1
#define KYTRIX_GRADIENT_VERTICAL 2
#define KYTRIX_GRADIENT_RADIAL 3

#define KYTRIX_MAX_CLIP_DEPTH 8
#define KYTRIX_CLIP_VERTEX_SLACK 2.0
// σ = blur × 0.5（CSS `box-shadow` 的「模糊半径」定义；与 Skia `SkBlurMaskFilter` 的
// σ 同义，Skia 的 radius 换算见 contract.md §4）。
#define KYTRIX_SHADOW_SIGMA_SCALE 0.5
#define KYTRIX_SHADOW_SIGMA_MIN 0.35
// 解析 AA 的「完全覆盖」距离（逻辑像素）：几何通道的纹理管线没有覆盖率属性，
// 用它把 aa_coverage 推到 1.0。
#define KYTRIX_AA_INSIDE -65536.0
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
    float4 clip;         // 64 .x = clip_index（0 = 无裁剪），.y = instance_base；.zw 保留恒 0
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

// 圆角矩形 SDF。半径按 CSS 规则夹到 `min(half_size)`：`r > min(half)` 会让
// `q = |p| - half + r` 恒正 ⇒ 等值线反过来「越裁越大」（内容溢出）。编码器已按
// 同一规则规范化，这里是第二道防线（数据异常也不出现反向错误）。
float sd_rounded_rect(float2 p, float2 half_size, float4 radii) {
    float r = (p.x > 0.0) ? ((p.y > 0.0) ? radii.z : radii.y)
                          : ((p.y > 0.0) ? radii.w : radii.x);
    r = clamp(r, 0.0, min(half_size.x, half_size.y));
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

// 标准正态的尾概率 Φ(-x)：半平面高斯模糊的**精确**覆盖率。
float shadow_cdf(float x) {
    return 0.5 * (1.0 - erf_approx(x * 0.70710678));
}

// 解析阴影覆盖率：可分离 2D 高斯。
//
// σ = blur × KYTRIX_SHADOW_SIGMA_SCALE（= blur / 2），与 CSS `box-shadow` /
// `filter: blur(r)` 的「模糊半径」定义一致（Figma 同）。Skia 的 `SkBlurMaskFilter`
// 用 `σ = radius × 0.57735 + 0.5`（其 SPECULAR/MASK 光栅器的 sigma 换算），因此同一
// 视觉参数下本式的 σ 比 Skia 的略小 —— 换算关系写在 contract.md §4。
//
// 形状：轴对齐矩形的二维高斯响应是**可分离**的
//   Φ(-dx/σ) × Φ(-dy/σ)   （dx/dy = 到左右/上下边的带符号距离，正 = 外侧）
// 直角（r = 0）时它就是精确解（旧实现的半平面近似在角部把阴影「填实」约 58%）。
// 圆角处边界是圆弧，分离式会高估角部覆盖，因此按「角部圆度 × r/(r+σ)」混合到
// SDF 半平面解 Φ(-sdf/σ)：r = 0 退化为分离式，r >> σ 时角部走圆弧解。
float analytic_shadow(float2 p, float2 half_size, float4 radii, float blur) {
    float sigma = max(blur * KYTRIX_SHADOW_SIGMA_SCALE, KYTRIX_SHADOW_SIGMA_MIN);
    float dx = abs(p.x) - half_size.x;
    float dy = abs(p.y) - half_size.y;
    float rect_part = shadow_cdf(dx / sigma) * shadow_cdf(dy / sigma);
    float corner_r = clamp(max(max(radii.x, radii.y), max(radii.z, radii.w)), 0.0,
                         min(half_size.x, half_size.y));
    float sdf_part = shadow_cdf(sd_rounded_rect(p, half_size, radii) / sigma);
    float corner = saturate(min(dx + corner_r, dy + corner_r) / max(corner_r, 1.0e-4));
    return lerp(rect_part, sdf_part, corner * (corner_r / (corner_r + sigma)));
}

// ---- 渐变 ----------------------------------------------------------------

// 双色渐变：线性预乘空间插值（与合成/AA 同一个空间；R05-7 的唯一插值空间）。
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

// ---- geometry ------------------------------------------------------------

float2 geometry_world(float2 local) {
    float2 p = float2(draw.basis.x * local.x + draw.basis.z * local.y + draw.translation.x,
                      draw.basis.y * local.x + draw.basis.w * local.y + draw.translation.y);
    return p * frame.params.x;
}

// ---- clip ----------------------------------------------------------------

// clip_index == 0 = 无裁剪（表项 0 的 aabb 不是可用矩形，禁止对它做数学运算）。
//
// 覆盖率语义（R05-10）：CPU 侧已把**全部轴对齐矩形 clip 求交合并为链上的一条**，
// 因此逐层相乘只发生在「矩形交集 × 圆角形状」之间 —— 两层边界重合不再得到
// 0.5 × 0.5 = 0.25（Skia `SkClipStack` 的矩形求交语义）。零尺寸 clip 的覆盖率恒为 0
//（全裁）：它不是 0.5 的鬼线。`clip_depth` 是链长（编码器写入 params1.w）：循环按
// 真实链长收口，链长 > KYTRIX_MAX_CLIP_DEPTH 时编码期直接报错，不会静默少裁。
float clip_mask(float2 world_px, uint clip_index, uint clip_depth) {
    if (clip_index == 0u) {
        return 1.0;
    }
    float m = 1.0;
    uint idx = clip_index;
    uint limit = min(clip_depth, KYTRIX_MAX_CLIP_DEPTH);
    for (uint i = 0u; i < limit && idx != 0u; ++i) {
        ClipEntry e = clips[idx];
        if (e.rect.z <= 0.0 || e.rect.w <= 0.0) {
            return 0.0;
        }
        float2 half_size = e.rect.zw * 0.5;
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
