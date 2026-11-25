#include <metal_stdlib>
using namespace metal;

#define KYTRIX_KIND_QUAD 0u
#define KYTRIX_KIND_GLYPH 1u
#define KYTRIX_KIND_IMAGE 2u
#define KYTRIX_KIND_SHADOW 3u
#define KYTRIX_KIND_COLOR_GLYPH 8u

#define KYTRIX_GRADIENT_NONE 0u
#define KYTRIX_GRADIENT_HORIZONTAL 1u
#define KYTRIX_GRADIENT_VERTICAL 2u
#define KYTRIX_GRADIENT_RADIAL 3u

#define KYTRIX_MAX_CLIP_DEPTH 8u
#define KYTRIX_MAX_CLIP_ITEMS 9u
#define KYTRIX_CLIP_KIND_RECT 1u
#define KYTRIX_CLIP_KIND_ROUNDED 2u
#define KYTRIX_CLIP_VERTEX_SLACK 2.0
#define KYTRIX_SHADOW_SIGMA_SCALE 0.5
#define KYTRIX_SHADOW_SIGMA_MIN 0.35
#define KYTRIX_AA_INSIDE -65536.0

struct FrameUniform {
    float4 viewport;
    float4 params;
};

struct ClipEntry {
    float4 rect;
    float4 corner_radius;
    uint4  meta;
};

struct GeometryDrawUniform {
    float4 basis;
    float4 translation;
    float4 color0;
    float4 clip_rect;
    float4 clip;
};

struct GpuPrimitive {
    float4 bounds;
    float4 corner_radius;
    float4 color0;
    float4 color1;
    float4 params0;
    float4 params1;
    float4 uv;
    float4 clip_rect;
};

struct RootUniform {
    FrameUniform frame;
    GeometryDrawUniform draw;
};

inline float lerp(float a, float b, float t) { return mix(a, b, t); }
inline float2 lerp(float2 a, float2 b, float2 t) { return mix(a, b, t); }
inline float4 lerp(float4 a, float4 b, float t) { return mix(a, b, t); }


inline float2 to_ndc(float2 world_px, constant FrameUniform& frame) {
    return float2(world_px.x * frame.viewport.z - 1.0,
                  1.0 - world_px.y * frame.viewport.w);
}


inline float aa_coverage(float d) {
    float w = max(length(float2(dfdx(d), dfdy(d))), 1e-5);
    return saturate(0.5 - d / w);
}

inline float sd_rounded_rect(float2 p, float2 half_size, float4 radii) {
    float r = (p.x > 0.0) ? ((p.y > 0.0) ? radii.z : radii.y)
                          : ((p.y > 0.0) ? radii.w : radii.x);
    r = clamp(r, 0.0, min(half_size.x, half_size.y));
    float2 q = abs(p) - half_size + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - r;
}


inline float erf_approx(float x) {
    float ax = abs(x);
    float t = 1.0 / (1.0 + 0.3275911 * ax);
    float poly = t * (0.254829592 + t * (-0.284496736 + t * (1.421413741 +
                 t * (-1.453152027 + t * 1.061405429))));
    float e = 1.0 - poly * exp(-ax * ax);
    return (x < 0.0) ? -e : e;
}

inline float shadow_cdf(float x) {
    return 0.5 * (1.0 - erf_approx(x * 0.70710678));
}

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


inline float2 geometry_world(float2 local, constant RootUniform& root) {
    float2 p = float2(root.draw.basis.x * local.x + root.draw.basis.z * local.y +
                          root.draw.translation.x,
                      root.draw.basis.y * local.x + root.draw.basis.w * local.y +
                          root.draw.translation.y);
    return p * root.frame.params.x;
}


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


inline float4 apply_gamma(float4 premultiplied_linear) {
    return premultiplied_linear;
}
