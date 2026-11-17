// common.frag.glsl -- 只有 fragment 阶段可以 include 的部分。
// 与 common.hlsl / common.metal 逐行等价（HLSL 的 fwidth 在两个 stage 都合法，故无需拆分）；
// 契约：src/render/shaders/contract.md
//
// 为什么单独成文件：GLSL 禁止在顶点阶段调用导数函数（`fwidth` / `dFdx` / `dFdy`），
// 而 `common.glsl` 被 3 个 vertex stage 包含。glslang 对**整份编译单元**报
// `'fwidth' : no matching overloaded function found`，即使该函数从未被顶点入口调用。
// 因此凡是用到导数的函数都必须离开顶点可见的范围。

// 1 px 解析 AA：d 为像素单位的带符号距离。只在「实例级一致」的分支内调用。
//
// 带宽取**梯度模长** `length(ddx, ddy)`（逐 fragment 按屏幕空间导数求得）：`|ddx| + |ddy|`
// 会把对角边的带宽放大到最多 1.41 px（比 1 px 软，且随边角度抖动），与 Skia 的
// `length(vec2(dFdx(d), dFdy(d)))` 一致。
float aa_coverage(float d) {
    float w = max(length(vec2(dFdx(d), dFdy(d))), 1e-5);
    return saturate(0.5 - d / w);
}

// ---- clip ----------------------------------------------------------------

// clip_index == 0 = 无裁剪（表项 0 不是可用矩形，禁止对它做数学运算）。
//
// 覆盖率语义（R05-10 / AR2 P1-A）：链上**至多一条矩形项**（编码期把全部轴对齐矩形 clip
// 求交合并成一条）+ k 条圆角项（k ≤ KYTRIX_MAX_CLIP_DEPTH）。
//
// - 矩形项 = **像素精确 scissor**（Skia `SkClipStack` 的 `kRect_ClipType` 语义）：覆盖率
//   只有 0 或 1，**不参与 AA 相乘**。判定用**像素索引**（`floor(world_px)`）与**编码期已吸附
//   到整数物理像素**的 `[lo, hi)` 比较（`lo = floor(x0)`、`hi = ceil(x1)`，即 Skia 的
//   `SkIRect::roundOut`）—— 与矩形边界重合的圆角 AA 不会被 scissor 提前切掉，因此
//   「同边界的矩形 clip + 圆角 clip」只乘一次 AA（不再有 0.5 × 0.5 = 0.25 的暗边）。
//   **为什么不能拿 `world_px` 直接和边界比**：`world_px` 是插值得到的顶点属性（顶点只夹到
//   `clip_rect ± KYTRIX_CLIP_VERTEX_SLACK`，边界像素确有片元），1 ulp 的插值误差就会让
//   「±0.5 px 像素单元」的相等判定翻面；因为覆盖率是 0/1，翻面的后果是**整列像素以满不透明
//   越出裁剪框**（lavapipe 上实测出过 1 px 实心出框条，AR2b P1-N1）。像素索引与整数边界之间
//   没有平局，1 ulp 也改不了 `floor` 的结果。
// - 圆角项 = 解析 AA（`aa_coverage(sd_rounded_rect)`），逐层相乘（嵌套圆角形状的交集不是
//   矩形，无法压成一条；完全相同的圆角项在编码期已去重，不会重复相乘）。
//
// 零尺寸项（含空交集）的覆盖率恒为 0（全裁），不是 0.5 的鬼线。`clip_depth` 是链长
// （编码器写入 params1.w）：循环按真实链长收口；链长 > KYTRIX_MAX_CLIP_ITEMS 在编码期
// 直接报错，不会静默少裁。
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
            // 像素索引比较：`floor(world_px)` 与整数吸附后的 `[lo, hi)` 都是整数值，
            // 不存在「1 ulp 导致边界像素翻面」的平局（见函数头注释）。
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
