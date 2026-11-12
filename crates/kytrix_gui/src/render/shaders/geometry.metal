// geometry.metal -- 静态 VBO 通道（Ui / Textured）。
// 契约：src/render/shaders/contract.md；入口：vs_ui / vs_textured / ps_ui /
// ps_textured_image / ps_textured_glyph（产物 geometry.metallib）。

#include "common.metal"

struct GeomVsOut {
    float4 position [[position]];
    float2 world_px;
    float2 uv;
};

struct GeomVsInUi {
    float2 position [[attribute(0)]];
};

struct GeomVsInTextured {
    float2 position [[attribute(0)]];
    float2 uv [[attribute(1)]];
};

inline float2 geometry_world(float2 local, constant RootUniform& root) {
    float2 p = float2(root.draw.basis.x * local.x + root.draw.basis.z * local.y +
                          root.draw.translation.x,
                      root.draw.basis.y * local.x + root.draw.basis.w * local.y +
                          root.draw.translation.y);
    return p * root.frame.params.x;
}

inline GeomVsOut geometry_vertex(float2 local, float2 uv, constant RootUniform& root) {
    GeomVsOut o;
    o.world_px = geometry_world(local, root);
    o.position = float4(to_ndc(o.world_px, root.frame), 0.0, 1.0);
    o.uv = uv;
    return o;
}

vertex GeomVsOut vs_ui(GeomVsInUi in [[stage_in]],
                       constant RootUniform& root [[buffer(2)]]) {
    return geometry_vertex(in.position, float2(0.0, 0.0), root);
}

vertex GeomVsOut vs_textured(GeomVsInTextured in [[stage_in]],
                             constant RootUniform& root [[buffer(2)]]) {
    return geometry_vertex(in.position, in.uv, root);
}

fragment float4 ps_ui(GeomVsOut in [[stage_in]],
                      device const ClipEntry* clips [[buffer(1)]],
                      constant RootUniform& root [[buffer(2)]]) {
    return apply_gamma(root.draw.color0 * clip_mask(in.world_px, root.draw.clip.x, clips));
}

fragment float4 ps_textured_image(GeomVsOut in [[stage_in]],
                                  device const ClipEntry* clips [[buffer(1)]],
                                  constant RootUniform& root [[buffer(2)]],
                                  texture2d<float> image_atlas [[texture(1)]],
                                  sampler atlas_sampler [[sampler(0)]]) {
    float4 texel = image_atlas.sample(atlas_sampler, in.uv);
    float4 color = texel * root.draw.color0;
    return apply_gamma(color * clip_mask(in.world_px, root.draw.clip.x, clips));
}

fragment float4 ps_textured_glyph(GeomVsOut in [[stage_in]],
                                  device const ClipEntry* clips [[buffer(1)]],
                                  constant RootUniform& root [[buffer(2)]],
                                  texture2d<float> glyph_atlas [[texture(0)]],
                                  sampler atlas_sampler [[sampler(0)]]) {
    float coverage = glyph_atlas.sample(atlas_sampler, in.uv).r;
    float4 color = root.draw.color0 * coverage;
    return apply_gamma(color * clip_mask(in.world_px, root.draw.clip.x, clips));
}
