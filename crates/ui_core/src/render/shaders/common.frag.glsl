float aa_coverage(float d) {
    float w = max(length(vec2(dFdx(d), dFdy(d))), 1e-5);
    return saturate(0.5 - d / w);
}


float clip_mask(vec2 world_px, uint clip_index, uint clip_depth) {
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
            vec2 half_size = e.rect.zw * 0.5;
            vec2 center = e.rect.xy + half_size;
            m *= aa_coverage(sd_rounded_rect(world_px - center, half_size, e.corner_radius));
        } else {
            vec2 pixel = floor(world_px);
            vec2 lo = e.rect.xy;
            vec2 hi = e.rect.xy + e.rect.zw;
            if (pixel.x < lo.x || pixel.y < lo.y || pixel.x >= hi.x || pixel.y >= hi.y) {
                return 0.0;
            }
        }
        idx = e.meta.x;
    }
    return m;
}
