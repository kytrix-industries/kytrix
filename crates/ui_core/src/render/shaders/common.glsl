// common.glsl -- 由 7 个 stage 文件 #include（无 #version）。
// 与 common.hlsl 逐行等价；契约：src/render/shaders/contract.md

// <kytrix-constants>
#define KYTRIX_KIND_QUAD 0u
#define KYTRIX_KIND_GLYPH 1u
#define KYTRIX_KIND_IMAGE 2u
#define KYTRIX_KIND_SHADOW 3u

#define KYTRIX_FLAG_OPAQUE 1u

#define KYTRIX_TEXTURE_GLYPH 0u
#define KYTRIX_TEXTURE_IMAGE 1u

#define KYTRIX_GRADIENT_NONE 0u
#define KYTRIX_GRADIENT_HORIZONTAL 1u
#define KYTRIX_GRADIENT_VERTICAL 2u
#define KYTRIX_GRADIENT_RADIAL 3u

#define KYTRIX_MAX_CLIP_DEPTH 8u
#define KYTRIX_CLIP_VERTEX_SLACK 2.0
#define KYTRIX_SHADOW_SIGMA_DIVISOR 3.0
#define KYTRIX_SHADOW_SIGMA_MIN 0.35
// </kytrix-constants>

struct FrameUniform {
    vec4 viewport;   // (w_px, h_px, 2 / w_px, 2 / h_px)
    vec4 params;     // (scale, 0, 0, 0)
};

struct ClipEntry {
    vec4  rect;           // (x, y, w, h) 物理像素
    vec4  corner_radius;  // (tl, tr, br, bl) 物理像素
    uvec4 meta;           // .x = parent（0 = 无裁剪）；.yzw 保留恒 0
};

struct GeometryDrawUniform {
    vec4  basis;        // (m11, m12, m21, m22)
    vec4  translation;  // (tx, ty, 0, 0)
    vec4  color0;       // premultiplied linear RGBA
    vec4  clip_rect;    // (x, y, w, h) 物理像素
    uvec4 clip;         // .x = clip_index（0 = 无裁剪）；.yzw 保留恒 0
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

float sd_rounded_rect(vec2 p, vec2 half_size, vec4 radii) {
    float r = (p.x > 0.0) ? ((p.y > 0.0) ? radii.z : radii.y)
                          : ((p.y > 0.0) ? radii.w : radii.x);
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

// sigma = blur / 3：3 sigma = blur 与 params0.w = blur + 1 的外扩一致。
float analytic_shadow(float d, float blur) {
    float sigma = max(blur / KYTRIX_SHADOW_SIGMA_DIVISOR, KYTRIX_SHADOW_SIGMA_MIN);
    return 0.5 * (1.0 - erf_approx(d / (sigma * 1.41421356)));
}

// ---- 渐变 ----------------------------------------------------------------

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
