// geometry.hlsl -- 静态 VBO 通道（Ui / Textured）
#include "common.hlsl"

struct VsOut {
    float4 position : SV_Position;
    float2 world_px : TEXCOORD0;
    float2 uv : TEXCOORD1;
};

struct VsInUi {
    float2 position : POSITION;
};

struct VsInTextured {
    float2 position : POSITION;
    float2 uv : TEXCOORD0;
};

float2 geometry_world(float2 local) {
    float2 p = float2(draw.basis.x * local.x + draw.basis.z * local.y + draw.translation.x,
                      draw.basis.y * local.x + draw.basis.w * local.y + draw.translation.y);
    return p * frame.params.x;
}

VsOut geometry_vertex(float2 local, float2 uv) {
    VsOut o;
    o.world_px = geometry_world(local);
    o.position = float4(to_ndc(o.world_px), 0.0, 1.0);
    o.uv = uv;
    return o;
}

VsOut vs_ui(VsInUi input) {
    return geometry_vertex(input.position, float2(0.0, 0.0));
}

VsOut vs_textured(VsInTextured input) {
    return geometry_vertex(input.position, input.uv);
}

float4 ps_ui(VsOut input) : SV_Target {
    return apply_gamma(draw.color0 * clip_mask(input.world_px, draw.clip.x));
}

float4 ps_textured_image(VsOut input) : SV_Target {
    float4 texel = image_atlas.Sample(atlas_sampler, input.uv);
    float4 color = texel * draw.color0;
    return apply_gamma(color * clip_mask(input.world_px, draw.clip.x));
}

float4 ps_textured_glyph(VsOut input) : SV_Target {
    float coverage = glyph_atlas.Sample(atlas_sampler, input.uv).r;
    float4 color = draw.color0 * coverage;
    return apply_gamma(color * clip_mask(input.world_px, draw.clip.x));
}
