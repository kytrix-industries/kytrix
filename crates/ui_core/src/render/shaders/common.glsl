// common.glsl -- 由 7 个 stage 文件 #include（无 #version）。
// 与 common.hlsl / common.metal 逐行等价；契约：src/render/shaders/contract.md
// 任何数学函数体的改动必须三份同步（build.rs 的归一化逐行比对会拦住漂移）。

// <kytrix-constants>
#define KYTRIX_KIND_QUAD 0u
#define KYTRIX_KIND_GLYPH 1u
#define KYTRIX_KIND_IMAGE 2u
#define KYTRIX_KIND_SHADOW 3u

#define KYTRIX_TEXTURE_GLYPH 0u
#define KYTRIX_TEXTURE_IMAGE 1u

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
    vec4 viewport;   // (w_px, h_px, 2 / w_px, 2 / h_px)
    vec4 params;     // (scale, 0, 0, 0)
};

struct ClipEntry {
    vec4  rect;           // (x, y, w, h) 物理像素
    vec4  corner_radius;  // (tl, tr, br, bl) 物理像素
    uvec4 meta;           // .x = parent（0 = 无裁剪）；.y = KYTRIX_CLIP_KIND_*；.zw 保留恒 0
};

struct GeometryDrawUniform {
    vec4  basis;        // (m11, m12, m21, m22)
    vec4  translation;  // (tx, ty, 0, 0)
    vec4  color0;       // premultiplied linear RGBA
    vec4  clip_rect;    // (x, y, w, h) 物理像素
    vec4  clip;         // .x = clip_index（0 = 无裁剪），.y = instance_base；.zw 保留恒 0
};

struct GpuPrimitive {
    vec4 bounds;         // 0
    vec4 corner_radius;  // 16
    vec4 color0;         // 32
    vec4 color1;         // 48
    vec4 params0;        // 64
    vec4 params1;        // 80
    vec4 uv;             // 96
    vec4 clip_rect;      // 112
};

layout(std430, set = 0, binding = 0) readonly buffer Instances { GpuPrimitive instances[]; };
layout(std430, set = 0, binding = 1) readonly buffer Clips { ClipEntry clips[]; };
layout(push_constant) uniform RootPush { FrameUniform frame; GeometryDrawUniform draw; } root;
layout(set = 0, binding = 4) uniform sampler2D glyph_atlas;
layout(set = 0, binding = 5) uniform sampler2D image_atlas;

// GLSL 没有 lerp / saturate：提供同名包装，保证函数体与 HLSL 逐行一致。
float saturate(float x) { return clamp(x, 0.0, 1.0); }
float lerp(float a, float b, float t) { return mix(a, b, t); }
vec2 lerp(vec2 a, vec2 b, vec2 t) { return mix(a, b, t); }
vec4 lerp(vec4 a, vec4 b, float t) { return mix(a, b, t); }

// ---- 坐标 ----------------------------------------------------------------

// 物理像素（左上原点、Y 向下）-> Vulkan NDC（Y 向下）。
vec2 to_ndc(vec2 world_px) {
    return vec2(world_px.x * root.frame.viewport.z - 1.0,
                world_px.y * root.frame.viewport.w - 1.0);
}

// ---- SDF 与 AA -----------------------------------------------------------

// AA 与 clip_mask 在 `common.frag.glsl`：它们用 `fwidth`，而 GLSL 禁止顶点阶段求导，
// 本文件又被 3 个 vertex stage 包含（F-145）。

// 圆角矩形 SDF。半径夹到 **SDF 合法域** `min(half_size)`：`r > min(half)` 会让
// `q = |p| - half + r` 恒正 ⇒ 等值线反过来「越裁越大」（内容溢出）。这不是 CSS 算法：
// CSS（Backgrounds §5.5）在「一条边上相邻两角半径之和 > 该边长」时按 f = min(side/Σr)
// 等比缩小**全部**半径；编码器先做那一步，再做本步的合法域 clamp（见
// `paint::instance::normalize_corner_radius`）。这里保留同一 clamp 作为第二道防线
// （数据异常也不出现反向错误），对已规范化的数据是幂等的。
float sd_rounded_rect(vec2 p, vec2 half_size, vec4 radii) {
    float r = (p.x > 0.0) ? ((p.y > 0.0) ? radii.z : radii.y)
                          : ((p.y > 0.0) ? radii.w : radii.x);
    r = clamp(r, 0.0, min(half_size.x, half_size.y));
    vec2 q = abs(p) - half_size + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2(0.0))) - r;
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
// 形状：轴对齐矩形经二维高斯模糊后的响应是**可分离**的，一维因子是「两条对边的尾概率
// 之差」（box ⊗ gaussian 的精确解）：
//   c(h) = Φ((h - |x|)/σ) − Φ((-h - |x|)/σ) = shadow_cdf((|x|-h)/σ) − shadow_cdf((|x|+h)/σ)
// 只取近边项（旧实现 `Φ(-dx/σ)·Φ(-dy/σ)`）在小元素上会把阴影铺得过宽：元素尺寸
// `half ≳ 3σ` 时两边差不多，`half ≪ σ` 时少掉的对边项不可忽略（8×8 元素 + blur 24
// ⇒ σ=12 时中心被高估约 6 倍，且四周出现不存在的光晕）。
// 直角（r = 0）时它就是精确解。圆角处边界是圆弧，分离式会高估角部覆盖，因此按
// 「角部圆度 × r/(r+σ)」混合到 SDF 半平面解 Φ(-sdf/σ)：r = 0 退化为分离式，
// r >> σ 时角部走圆弧解。
float analytic_shadow(vec2 p, vec2 half_size, vec4 radii, float blur) {
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
vec4 gradient_eval(uint kind, vec2 p, vec2 half_size, vec4 c0, vec4 c1) {
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

vec2 geometry_world(vec2 local) {
    vec2 p = vec2(root.draw.basis.x * local.x + root.draw.basis.z * local.y + root.draw.translation.x,
                  root.draw.basis.y * local.x + root.draw.basis.w * local.y + root.draw.translation.y);
    return p * root.frame.params.x;
}

// ---- 输出 ----------------------------------------------------------------

// v1：渲染目标是 sRGB 格式，linear -> sRGB 由目标格式在写回时完成一次。
vec4 apply_gamma(vec4 premultiplied_linear) {
    return premultiplied_linear;
}
