// geometry.hlsl -- 静态 VBO 通道（Ui / Textured）
#include "common.hlsl"

struct VsOut {
    float4 position : SV_Position;
    float2 world_px : TEXCOORD0;
    float2 uv : TEXCOORD1;
    float coverage : TEXCOORD2;   // 物理像素单位的带符号距离（解析 AA）
};

struct VsInUi {
    float2 position : POSITION;
    // 解析 AA 的覆盖率输入：到形状边界的**有符号距离**（逻辑像素，负 = 内部）。
    // 网格必须沿边界向外扩 ~1 物理像素，这一段就是抗锯齿带（Skia 的 AA 三角化同理）。
    float coverage : TEXCOORD0;
};

struct VsInTextured {
    float2 position : POSITION;
    float2 uv : TEXCOORD0;
};


VsOut geometry_vertex(float2 local, float2 uv, float coverage) {
    VsOut o;
    o.world_px = geometry_world(local);
    o.position = float4(to_ndc(o.world_px), 0.0, 1.0);
    o.uv = uv;
    // 逻辑像素 -> 物理像素：aa_coverage 的 d 必须是设备像素。
    o.coverage = coverage * frame.params.x;
    return o;
}

VsOut vs_ui(VsInUi input) {
    return geometry_vertex(input.position, float2(0.0, 0.0), input.coverage);
}

VsOut vs_textured(VsInTextured input) {
    return geometry_vertex(input.position, input.uv, KYTRIX_AA_INSIDE);
}

float4 ps_ui(VsOut input) : SV_Target {
    // 解析 AA：覆盖率由**顶点距离插值**给出，不再依赖 MSAA / 后处理（三后端一致）。
    float coverage = aa_coverage(input.coverage);
    return apply_gamma(draw.color0 * coverage *
                       clip_mask(input.world_px, (uint)draw.clip.x,
                                 (uint)KYTRIX_MAX_CLIP_ITEMS));
}

float4 ps_textured_image(VsOut input) : SV_Target {
    float4 texel = image_atlas.Sample(atlas_sampler, input.uv);
    float4 color = texel * draw.color0;
    return apply_gamma(color * clip_mask(input.world_px, (uint)draw.clip.x,
                                         (uint)KYTRIX_MAX_CLIP_ITEMS));
}

float4 ps_textured_glyph(VsOut input) : SV_Target {
    float coverage = glyph_atlas.Sample(atlas_sampler, input.uv).r;
    float4 color = draw.color0 * coverage;
    return apply_gamma(color * clip_mask(input.world_px, (uint)draw.clip.x,
                                         (uint)KYTRIX_MAX_CLIP_ITEMS));
}
