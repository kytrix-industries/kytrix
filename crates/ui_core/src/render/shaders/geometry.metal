#include "common.metal"

struct GeomVsOut {
    float4 position [[position]];
    float2 world_px;
    float2 uv;
    float coverage;
};

struct GeomVsInUi {
    float2 position [[attribute(0)]];
    float coverage [[attribute(1)]];
};

struct GeomVsInTextured {
    float2 position [[attribute(0)]];
    float2 uv [[attribute(1)]];
};


inline GeomVsOut geometry_vertex(float2 local, float2 uv, float coverage,
                                 constant RootUniform& root) {
    GeomVsOut o;
    o.world_px = geometry_world(local, root);
    o.position = float4(to_ndc(o.world_px, root.frame), 0.0, 1.0);
    o.uv = uv;
    o.coverage = coverage * root.frame.params.x;
    return o;
}

vertex GeomVsOut vs_ui(GeomVsInUi in [[stage_in]],
                       constant RootUniform& root [[buffer(2)]]) {
    return geometry_vertex(in.position, float2(0.0, 0.0), in.coverage, root);
}

vertex GeomVsOut vs_textured(GeomVsInTextured in [[stage_in]],
                             constant RootUniform& root [[buffer(2)]]) {
    return geometry_vertex(in.position, in.uv, KYTRIX_AA_INSIDE, root);
}

fragment float4 ps_ui(GeomVsOut in [[stage_in]],
                      device const ClipEntry* clips [[buffer(1)]],
                      constant RootUniform& root [[buffer(2)]]) {
    float coverage = aa_coverage(in.coverage);
    return apply_gamma(root.draw.color0 * coverage *
                       clip_mask(in.world_px, uint(root.draw.clip.x),
                                 uint(KYTRIX_MAX_CLIP_ITEMS), clips));
}

fragment float4 ps_textured_image(GeomVsOut in [[stage_in]],
                                  device const ClipEntry* clips [[buffer(1)]],
                                  constant RootUniform& root [[buffer(2)]],
                                  texture2d_array<float> image_atlas [[texture(1)]],
                                  sampler atlas_sampler [[sampler(0)]]) {
    float4 texel = image_atlas.sample(atlas_sampler, in.uv, 0u);
    float4 color = texel * root.draw.color0;
    return apply_gamma(color * clip_mask(in.world_px, uint(root.draw.clip.x),
                                         uint(KYTRIX_MAX_CLIP_ITEMS), clips));
}

fragment float4 ps_textured_glyph(GeomVsOut in [[stage_in]],
                                  device const ClipEntry* clips [[buffer(1)]],
                                  constant RootUniform& root [[buffer(2)]],
                                  texture2d_array<float> glyph_atlas [[texture(0)]],
                                  sampler atlas_sampler [[sampler(0)]]) {
    float coverage = glyph_atlas.sample(atlas_sampler, in.uv, 0u).r;
    float4 color = root.draw.color0 * coverage;
    return apply_gamma(color * clip_mask(in.world_px, uint(root.draw.clip.x),
                                         uint(KYTRIX_MAX_CLIP_ITEMS), clips));
}
