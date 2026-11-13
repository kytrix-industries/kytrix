// common.frag.glsl -- 只有 fragment 阶段可以 include 的部分。
// 与 common.hlsl 逐行等价（HLSL 的 fwidth 在两个 stage 都合法，故无需拆分）；
// 契约：src/render/shaders/contract.md
//
// 为什么单独成文件：GLSL 禁止在顶点阶段调用导数函数（`fwidth` / `dFdx` / `dFdy`），
// 而 `common.glsl` 被 3 个 vertex stage 包含。glslang 对**整份编译单元**报
// `'fwidth' : no matching overloaded function found`，即使该函数从未被顶点入口调用。
// 因此凡是用到导数的函数都必须离开顶点可见的范围。

// 1 px 解析 AA：d 为像素单位的带符号距离。只在「实例级一致」的分支内调用。
float aa_coverage(float d) {
    float w = max(fwidth(d), 1e-5);
    return saturate(0.5 - d / w);
}

// ---- clip ----------------------------------------------------------------

// clip_index == 0 = 无裁剪（表项 0 不是可用矩形，禁止对它做数学运算）。
float clip_mask(vec2 world_px, uint clip_index) {
    if (clip_index == 0u) {
        return 1.0;
    }
    float m = 1.0;
    uint idx = clip_index;
    for (uint i = 0u; i < KYTRIX_MAX_CLIP_DEPTH && idx != 0u; ++i) {
        ClipEntry e = clips[idx];
        vec2 half_size = max(e.rect.zw, vec2(0.0)) * 0.5;
        vec2 center = e.rect.xy + half_size;
        m *= aa_coverage(sd_rounded_rect(world_px - center, half_size, e.corner_radius));
        idx = e.meta.x;
    }
    return m;
}
