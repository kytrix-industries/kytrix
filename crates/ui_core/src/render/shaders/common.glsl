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
    vec4 viewport;
    vec4 params;
};

struct ClipEntry {
    vec4  rect;
    vec4  corner_radius;
    uvec4 meta;
};

struct GeometryDrawUniform {
    vec4  basis;
    vec4  translation;
    vec4  color0;
    vec4  clip_rect;
    vec4  clip;
};

struct GpuPrimitive {
    vec4 bounds;
    vec4 corner_radius;
    vec4 color0;
    vec4 color1;
    vec4 params0;
    vec4 params1;
    vec4 uv;
    vec4 clip_rect;
};

layout(std430, set = 0, binding = 0) readonly buffer Instances { GpuPrimitive instances[]; };
layout(std430, set = 0, binding = 1) readonly buffer Clips { ClipEntry clips[]; };
layout(push_constant) uniform RootPush { FrameUniform frame; GeometryDrawUniform draw; } root;
layout(set = 0, binding = 4) uniform sampler2DArray glyph_atlas;
layout(set = 0, binding = 5) uniform sampler2DArray image_atlas;
layout(set = 0, binding = 6) uniform sampler2DArray color_glyph_atlas;

float saturate(float x) { return clamp(x, 0.0, 1.0); }
float lerp(float a, float b, float t) { return mix(a, b, t); }
vec2 lerp(vec2 a, vec2 b, vec2 t) { return mix(a, b, t); }
vec4 lerp(vec4 a, vec4 b, float t) { return mix(a, b, t); }


vec2 to_ndc(vec2 world_px) {
    return vec2(world_px.x * root.frame.viewport.z - 1.0,
                world_px.y * root.frame.viewport.w - 1.0);
}



float sd_rounded_rect(vec2 p, vec2 half_size, vec4 radii) {
    float r = (p.x > 0.0) ? ((p.y > 0.0) ? radii.z : radii.y)
                          : ((p.y > 0.0) ? radii.w : radii.x);
    r = clamp(r, 0.0, min(half_size.x, half_size.y));
    vec2 q = abs(p) - half_size + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2(0.0))) - r;
}


float erf_approx(float x) {
    float ax = abs(x);
    float t = 1.0 / (1.0 + 0.3275911 * ax);
    float poly = t * (0.254829592 + t * (-0.284496736 + t * (1.421413741 +
                 t * (-1.453152027 + t * 1.061405429))));
    float e = 1.0 - poly * exp(-ax * ax);
    return (x < 0.0) ? -e : e;
}

float shadow_cdf(float x) {
    return 0.5 * (1.0 - erf_approx(x * 0.70710678));
}

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


vec2 geometry_world(vec2 local) {
    vec2 p = vec2(root.draw.basis.x * local.x + root.draw.basis.z * local.y + root.draw.translation.x,
                  root.draw.basis.y * local.x + root.draw.basis.w * local.y + root.draw.translation.y);
    return p * root.frame.params.x;
}


vec4 apply_gamma(vec4 premultiplied_linear) {
    return premultiplied_linear;
}
