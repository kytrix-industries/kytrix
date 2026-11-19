// common.metal -- uber-quad + geometry 共享的数据布局与函数（与 common.hlsl 逐行等价）。
// 规则：uniform / storage 结构体只用 16 B 成员（float4 / uint4）。
// 契约：src/render/shaders/contract.md

#include <metal_stdlib>
using namespace metal;

// <kytrix-constants>
#define KYTRIX_KIND_QUAD 0u
#define KYTRIX_KIND_GLYPH 1u
#define KYTRIX_KIND_IMAGE 2u
#define KYTRIX_KIND_SHADOW 3u

#define KYTRIX_GRADIENT_NONE 0u
#define KYTRIX_GRADIENT_HORIZONTAL 1u
#define KYTRIX_GRADIENT_VERTICAL 2u
#define KYTRIX_GRADIENT_RADIAL 3u

// Canvas / 编码器的**圆角** clip 层数上限（轴对齐矩形 clip 在编码期求交合并，不占层数）。
#define KYTRIX_MAX_CLIP_DEPTH 8u
// clip 链的**总项数**上界 = MAX_CLIP_DEPTH（圆角）+ 1（矩形）。shader 的 clip 循环按它收口。
#define KYTRIX_MAX_CLIP_ITEMS 9u
// clip 链项的类型（写进 `ClipEntry.meta.y`）：矩形项 = 像素精确 scissor（覆盖率 0/1，
// 不参与 AA 相乘）；圆角项 = 解析 AA。见 `clip_mask` 与 contract.md §2。
#define KYTRIX_CLIP_KIND_RECT 1u
#define KYTRIX_CLIP_KIND_ROUNDED 2u
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
    float4 params;     // (scale, scroll_x, scroll_y, 0)：.x = DPI，.yz = 合成端滚动偏移（物理像素）
};

struct ClipEntry {
    float4 rect;           // (x, y, w, h) 物理像素
    float4 corner_radius;  // (tl, tr, br, bl) 物理像素
    uint4  meta;           // .x = parent（0 = 无裁剪）；.y = KYTRIX_CLIP_KIND_*；.zw 保留恒 0
};

struct GeometryDrawUniform {
    float4 basis;        // (m11, m12, m21, m22)
    float4 translation;  // (tx, ty, 0, 0)
    float4 color0;       // premultiplied linear RGBA
    float4 clip_rect;    // (x, y, w, h) 物理像素
    float4 clip;         // .x = clip_index（0 = 无裁剪），.y = instance_base；.zw 保留恒 0
};

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

// 单个 112 B 常量块：frame @ 0，draw @ 32。
struct RootUniform {
    FrameUniform frame;
    GeometryDrawUniform draw;
};

// MSL 没有 lerp：提供同名包装，保证函数体与 HLSL 逐行一致。
inline float lerp(float a, float b, float t) { return mix(a, b, t); }
inline float2 lerp(float2 a, float2 b, float2 t) { return mix(a, b, t); }
inline float4 lerp(float4 a, float4 b, float t) { return mix(a, b, t); }

// ---- 坐标 ----------------------------------------------------------------

// 物理像素（左上原点、Y 向下）-> Metal NDC（Y 向上）。
inline float2 to_ndc(float2 world_px, constant FrameUniform& frame) {
    return float2(world_px.x * frame.viewport.z - 1.0,
                  1.0 - world_px.y * frame.viewport.w);
}

// ---- SDF 与 AA -----------------------------------------------------------

// 1 px 解析 AA：d 为像素单位的带符号距离。只在「实例级一致」的分支内调用。
//
// 带宽取**梯度模长** `length(ddx, ddy)`（逐 fragment 按屏幕空间导数求得）：`|ddx| + |ddy|`
// 会把对角边的带宽放大到最多 1.41 px（比 1 px 软，且随边角度抖动），与 Skia 的
// `length(vec2(dFdx(d), dFdy(d)))` 一致。
inline float aa_coverage(float d) {
    float w = max(length(float2(dfdx(d), dfdy(d))), 1e-5);
    return saturate(0.5 - d / w);
}

// 圆角矩形 SDF。半径夹到 **SDF 合法域** `min(half_size)`：`r > min(half)` 会让
// `q = |p| - half + r` 恒正 ⇒ 等值线反过来「越裁越大」（内容溢出）。这不是 CSS 算法：
// CSS（Backgrounds §5.5）在「一条边上相邻两角半径之和 > 该边长」时按 f = min(side/Σr)
// 等比缩小**全部**半径；编码器先做那一步，再做本步的合法域 clamp（见
// `paint::instance::normalize_corner_radius`）。这里保留同一 clamp 作为第二道防线
// （数据异常也不出现反向错误），对已规范化的数据是幂等的。
inline float sd_rounded_rect(float2 p, float2 half_size, float4 radii) {
    float r = (p.x > 0.0) ? ((p.y > 0.0) ? radii.z : radii.y)
                          : ((p.y > 0.0) ? radii.w : radii.x);
    r = clamp(r, 0.0, min(half_size.x, half_size.y));
    float2 q = abs(p) - half_size + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - r;
}

// ---- 解析阴影 ------------------------------------------------------------

// Abramowitz-Stegun 7.1.26（|误差| <= 1.5e-7）。
inline float erf_approx(float x) {
    float ax = abs(x);
    float t = 1.0 / (1.0 + 0.3275911 * ax);
    float poly = t * (0.254829592 + t * (-0.284496736 + t * (1.421413741 +
                 t * (-1.453152027 + t * 1.061405429))));
    float e = 1.0 - poly * exp(-ax * ax);
    return (x < 0.0) ? -e : e;
}

// 标准正态的尾概率 Φ(-x)：半平面高斯模糊的**精确**覆盖率。
inline float shadow_cdf(float x) {
    return 0.5 * (1.0 - erf_approx(x * 0.70710678));
}

// 解析阴影覆盖率：可分离 2D 高斯。
//
// σ = blur × KYTRIX_SHADOW_SIGMA_SCALE（= blur / 2），与 CSS `box-shadow` /
// `filter: blur(r)` 的「模糊半径」定义一致（Figma 同）。Skia 的 `SkBlurMaskFilter`
// 用 `σ = radius × 0.57735 + 0.5`（其 SPECULAR/MASK 光栅器的 sigma 换算），因此同一
// 视觉参数下本式的 σ 比 Skia 的略小 —— 换算关系写在 contract.md §4。
//
// 形状：轴对齐矩形经二维高斯模糊后的响应是**可分离**的，一维因子是「两条对边的尾概率
// 之差」（box ⊗ gaussian 的精确解）：
//   c(h) = Φ((h - |x|)/σ) − Φ((-h - |x|)/σ) = shadow_cdf((|x|-h)/σ) − shadow_cdf((|x|+h)/σ)
// 只取近边项（旧实现 `Φ(-dx/σ)·Φ(-dy/σ)`）在小元素上会把阴影铺得过宽：元素尺寸
// `half ≳ 3σ` 时两边差不多，`half ≪ σ` 时少掉的对边项不可忽略（8×8 元素 + blur 24
// ⇒ σ=12 时中心被高估约 6 倍，且四周出现不存在的光晕）。
// 直角（r = 0）时它就是精确解。圆角处边界是圆弧，分离式会高估角部覆盖，因此按
// 「角部圆度 × r/(r+σ)」混合到 SDF 半平面解 Φ(-sdf/σ)：r = 0 退化为分离式，
// r >> σ 时角部走圆弧解。
inline float analytic_shadow(float2 p, float2 half_size, float4 radii, float blur) {
    float sigma = max(blur * KYTRIX_SHADOW_SIGMA_SCALE, KYTRIX_SHADOW_SIGMA_MIN);
    float ax = abs(p.x);
    float ay = abs(p.y);
    float dx = ax - half_size.x;
    float dy = ay - half_size.y;
    float axis_x = saturate(shadow_cdf(dx / sigma) - shadow_cdf((ax + half_size.x) / sigma));
    float axis_y = saturate(shadow_cdf(dy / sigma) - shadow_cdf((ay + half_size.y) / sigma));
    float rect_part = axis_x * axis_y;
    float corner_r = clamp(max(max(radii.x, radii.y), max(radii.z, radii.w)), 0.0,
                           min(half_size.x, half_size.y));
    float sdf_part = shadow_cdf(sd_rounded_rect(p, half_size, radii) / sigma);
    float corner = saturate(min(dx + corner_r, dy + corner_r) / max(corner_r, 1.0e-4));
    return lerp(rect_part, sdf_part, corner * (corner_r / (corner_r + sigma)));
}

// ---- 渐变 ----------------------------------------------------------------

// 双色渐变：线性预乘空间插值（与合成/AA 同一个空间；R05-7 的唯一插值空间）。
inline float4 gradient_eval(uint kind, float2 p, float2 half_size, float4 c0, float4 c1) {
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

inline float2 geometry_world(float2 local, constant RootUniform& root) {
    float2 p = float2(root.draw.basis.x * local.x + root.draw.basis.z * local.y +
                          root.draw.translation.x,
                      root.draw.basis.y * local.x + root.draw.basis.w * local.y +
                          root.draw.translation.y);
    return p * root.frame.params.x;
}

// ---- clip ----------------------------------------------------------------

// clip_index == 0 = 无裁剪（表项 0 不是可用矩形，禁止对它做数学运算）。
//
// 覆盖率语义（R05-10 / AR2 P1-A）：链上**至多一条矩形项**（编码期把全部轴对齐矩形 clip
// 求交合并成一条）+ k 条圆角项（k ≤ KYTRIX_MAX_CLIP_DEPTH）。
//
// - 矩形项 = **像素精确 scissor**（Skia `SkClipStack` 的 `kRect_ClipType` 语义）：覆盖率
//   只有 0 或 1，**不参与 AA 相乘**。判定用**像素索引**（`floor(world_px)`）与**编码期已吸附
//   到整数物理像素**的 `[lo, hi)` 比较（`lo = floor(x0)`、`hi = ceil(x1)`，即 Skia 的
//   `SkIRect::roundOut`）—— 与矩形边界重合的圆角 AA 不会被 scissor 提前切掉，因此
//   「同边界的矩形 clip + 圆角 clip」只乘一次 AA（不再有 0.5 × 0.5 = 0.25 的暗边）。
//   **为什么不能拿 `world_px` 直接和边界比**：`world_px` 是插值得到的顶点属性（顶点只夹到
//   `clip_rect ± KYTRIX_CLIP_VERTEX_SLACK`，边界像素确有片元），1 ulp 的插值误差就会让
//   「±0.5 px 像素单元」的相等判定翻面；因为覆盖率是 0/1，翻面的后果是**整列像素以满不透明
//   越出裁剪框**（lavapipe 上实测出过 1 px 实心出框条，AR2b P1-N1）。像素索引与整数边界之间
//   没有平局，1 ulp 也改不了 `floor` 的结果。
// - 圆角项 = 解析 AA（`aa_coverage(sd_rounded_rect)`），逐层相乘（嵌套圆角形状的交集不是
//   矩形，无法压成一条；完全相同的圆角项在编码期已去重，不会重复相乘）。
//
// 零尺寸项（含空交集）的覆盖率恒为 0（全裁），不是 0.5 的鬼线。`clip_depth` 是链长
// （编码器写入 params1.w）：循环按真实链长收口；链长 > KYTRIX_MAX_CLIP_ITEMS 在编码期
// 直接报错，不会静默少裁。
inline float clip_mask(float2 world_px, uint clip_index, uint clip_depth,
                       device const ClipEntry* clips) {
    if (clip_index == 0u) {
        return 1.0;
    }
    float m = 1.0;
    uint idx = clip_index;
    uint limit = min(clip_depth, KYTRIX_MAX_CLIP_ITEMS);
    for (uint i = 0u; i < limit && idx != 0u; ++i) {
        ClipEntry e = clips[idx];
        if (e.rect.z <= 0.0 || e.rect.w <= 0.0) {
            return 0.0;
        }
        if (e.meta.y == KYTRIX_CLIP_KIND_ROUNDED) {
            float2 half_size = e.rect.zw * 0.5;
            float2 center = e.rect.xy + half_size;
            m *= aa_coverage(sd_rounded_rect(world_px - center, half_size, e.corner_radius));
        } else {
            // 像素索引比较：`floor(world_px)` 与整数吸附后的 `[lo, hi)` 都是整数值，
            // 不存在「1 ulp 导致边界像素翻面」的平局（见函数头注释）。
            float2 pixel = floor(world_px);
            float2 lo = e.rect.xy;
            float2 hi = e.rect.xy + e.rect.zw;
            if (pixel.x < lo.x || pixel.y < lo.y || pixel.x >= hi.x || pixel.y >= hi.y) {
                return 0.0;
            }
        }
        idx = e.meta.x;
    }
    return m;
}

// ---- 输出 ----------------------------------------------------------------

// v1：渲染目标是 sRGB 格式，linear -> sRGB 由目标格式在写回时完成一次。
inline float4 apply_gamma(float4 premultiplied_linear) {
    return premultiplied_linear;
}
