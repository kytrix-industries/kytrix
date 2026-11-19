#version 450
#extension GL_GOOGLE_include_directive : require

#include "common.glsl"
#include "common.frag.glsl"

layout(location = 0) in vec2 v_local_px;
layout(location = 1) in vec2 v_uv;
layout(location = 2) flat in uint v_instance;
layout(location = 0) out vec4 out_color;

void main() {
    GpuPrimitive p = instances[v_instance];
    uint kind = uint(p.params1.z);

    vec2 half_size = max(p.bounds.zw, vec2(0.0)) * 0.5;
    vec2 world_px = v_local_px + p.bounds.xy + half_size;

    vec4 color;
    if (kind == KYTRIX_KIND_GLYPH) {
        // 字形图集恒 1 层（contract §8）：layer 硬编码 0。
        float coverage = texture(glyph_atlas, vec3(v_uv, 0.0)).r;
        color = p.color0 * coverage;
    } else if (kind == KYTRIX_KIND_IMAGE) {
        // 图片图集是 texture array：`params1.y` 是图集页（= 层号）索引（整数编码进 f32）。
        color = texture(image_atlas, vec3(v_uv, float(uint(p.params1.y)))) * p.color0;
    } else if (kind == KYTRIX_KIND_SHADOW) {
        // 可分离 2D 高斯（σ = blur / 2）：矩形部分是精确解，角部按圆角半径混合。
        color = p.color0 * analytic_shadow(v_local_px, half_size, p.corner_radius, p.params0.y);
    } else {
        float d_outer = sd_rounded_rect(v_local_px, half_size, p.corner_radius);
        float coverage = aa_coverage(d_outer);
        uint gradient = uint(p.params0.z);
        vec4 fill = (gradient == KYTRIX_GRADIENT_NONE)
                        ? p.color0
                        : gradient_eval(gradient, v_local_px, half_size, p.color0, p.color1);
        color = fill * coverage;
        float border = p.params0.x;
        if (border > 0.0) {
            vec4 inner_radii = max(p.corner_radius - border, vec4(0.0));
            vec2 inner_half = max(half_size - border, vec2(0.0));
            float inner = aa_coverage(sd_rounded_rect(v_local_px, inner_half, inner_radii));
            float ring = max(coverage - inner, 0.0);
            color += (p.color1 - fill * p.color1.a) * ring;
        }
    }

    color *= clip_mask(world_px, uint(p.params1.x), uint(p.params1.w));
    out_color = apply_gamma(color);
}
