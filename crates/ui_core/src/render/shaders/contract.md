# 渲染契约（`render/shaders/contract.md`）— 唯一权威

本文件是 CPU 侧 `GpuPrimitive`、三套 shader（`*.hlsl` / `*.metal` / `*.glsl`）与三套 renderer 之间的**唯一契约**。
字段顺序、offset、取值、绑定、坐标与颜色空间的任何变更，必须**同时**改：

1. `paint/instance.rs`（`GpuPrimitive` 与 `KIND_*`/`FLAG_*`/`GRADIENT_*` 常量）；
2. 本目录下 15 个源文件（`common.*` / `common.frag.glsl` / `quad.*` / `geometry.*`）；
3. 本文件；
4. 触发像素快照回归（主设计 I8）。

`build.rs` 会在**每次构建**时比对本文件的锚点与三套源码的结构体/常量，漂移即构建失败。
设计依据：主设计 §2.3/§2.11/§5.4/§5.8、P01 §7（AA）、P02 §8/§9/§10、P06 §7。

---

## 1. `GpuPrimitive`：128 B / 16 B 对齐

| offset  | 字节 | 字段            | 类型                          | 语义                                                                                     |
|---------|------|-----------------|-------------------------------|------------------------------------------------------------------------------------------|
| **0**   | 16   | `bounds`        | `[f32;4]` / `float4` / `vec4` | `x, y, w, h`：SDF 图元为**物理像素、未取整浮点**；图集图元为**整数矩形的浮点表示**       |
| 16      | 16   | `corner_radius` | 同上                          | `tl, tr, br, bl`（物理像素）。规范化是**两步**：① CSS Backgrounds §5.5 的比例缩放（相邻两角半径之和超过边长 ⇒ 全部半径乘 `f = min(side/Σr)`）；② SDF 合法域 clamp `r <= min(w,h)/2`。shader 里保留 ② 作为防御性 clamp |
| 32      | 16   | `color0`        | 同上                          | **linear premultiplied** sRGB RGBA：实色 / 渐变起色 / 阴影色 / 字形色 / 图片调制色       |
| 48      | 16   | `color1`        | 同上                          | 渐变终色 **或** 边框色（互斥）；无则全 0                                                 |
| 64      | 16   | `params0`       | 同上                          | `border_width, shadow_blur, gradient_kind, aa_padding`（物理像素）                       |
| 80      | 16   | `params1`       | 同上                          | `clip_index, atlas_page, kind, clip_depth`（**整数编码进 f32**）                          |
| 96      | 16   | `uv`            | 同上                          | 图集 tile `u0, v0, u1, v1`（归一化，**用边**，无半纹素偏移）                             |
| 112     | 16   | `clip_rect`     | 同上                          | clip 链**交集 AABB**（物理浮点，`x, y, w, h`）                                           |
| **128** | —    | **总计**        | —                             | `size_of == 128`、`align_of == 16`、8 个 `offset_of!` 断言（已落地 `paint/instance.rs`） |

硬约束：

1. 实例/常量结构体**只允许 16 B 成员**（`float4` / `uint4` / `[f32;4]`）：HLSL cbuffer 的 16 B 打包、MSL 自然对齐、
   std140/std430 在「全 vec4」时逐字节一致；出现 `float3`/标量就会三平台分叉。
2. 实例缓冲**基址 16 B 对齐**（F-23），buffer 内按 128 B 步进。若某后端用「大常量缓冲 + 动态偏移」承载 `RootUniform`，
   偏移起点按 256 B 向上取整（只影响上传布局，不影响本节字段偏移）。
3. `params1` 的四个分量是**整数编码进 f32**（`value as f32`，取值 `[0, 2^24)`）。shader 一律用**数值转换**取回：
   `(uint)x` / `uint(x)`。**禁止位重解释**：`asuint` / `asfloat` / `as_type<>` / `floatBitsToUint` / `floatBitsToInt` /
   `bitcast` 一律不得出现（f32 存的是数值，不是整数的位模式）。
4. `params1.w` = **GPU 裁剪链的真实链长**（`clip_index` 指向的链上表项数，`0` = 无裁剪）：shader 用它
   收口 `clip_mask` 的循环。链上恒为「至多一条矩形项 + k 条圆角项（k ≤ `KYTRIX_MAX_CLIP_DEPTH`）」，
   因此链长上界是 `KYTRIX_MAX_CLIP_ITEMS = KYTRIX_MAX_CLIP_DEPTH + 1`；**圆角层数**超过
   `KYTRIX_MAX_CLIP_DEPTH` 时编码期直接返回 `EngineError::ClipDepthExceeded`（矩形层不占该预算：
   轴对齐矩形 clip 求交合并成一条，无损），shader 不会静默少裁。`params1.y` 对非图集 kind 写 0 并忽略；
   `uv` 对非图集 kind 写 0 并忽略；`color1` 无用途时写 0。
5. `bounds.zw` 必须 `>= 0` 且有限（编码器保证）；shader 仍取 `max(bounds.zw, 0)` 作为防御性一致。

## 2. `kind` / `atlas_page` / `gradient_kind` / `flags`

| `kind`        | 值    | 产生者                                 | FS 分支                                    |
|---------------|-------|----------------------------------------|--------------------------------------------|
| `KIND_QUAD`   | **0** | `Primitive::Quad`                      | SDF + 实色/渐变/边框                       |
| `KIND_GLYPH`  | **1** | `Primitive::GlyphRun`（每字形 1 实例） | `color0 × texel.r`                         |
| `KIND_IMAGE`  | **2** | `Primitive::Image`                     | `texel × color0`                           |
| `KIND_SHADOW` | **3** | `Primitive::Shadow`                    | SDF + erf 解析阴影                         |
| 保留          | 4     | —                                      | Path 两阶段 coverage（v1 不用，F-26）      |
| 保留          | 5     | —                                      | Geometry 实例化表达（v1 走 geometry 通道） |
| 保留          | 6–7   | —                                      | 应用自定义（v1 不做，F-29）                |

`atlas_page`（`params1.y`）：**采样哪张图集由 `params1.z`（`kind`）决定，`.y` 只是该图集
texture array 的层号（图集页索引）**。字形图集恒 1 层、页索引恒 `0`；图片图集的层数是**运行时
值**：等于 App 当前打开的资源图集页数（`render::atlas::ImageAtlasPager::page_count`，上限
`MAX_IMAGE_PAGES = 4`），页索引 `0..page_count` 由资源层的多页分配器给出（D-9 多页图集 + LRU）。
数组按当前页数懒创建、**只增不减**（页数增长时按新层数重建数组 + App 全量重传每个已打开层；
没有图片时是 1 层的 1×1 占位数组，不占 16 MiB）。`MAX_IMAGE_PAGES` 只是页号 / 待上传槽的上限，
shader 不读任何上限（层号直接来自 `params1.y`）；对非图集 kind 写 `0` 并忽略。
`gradient_kind`：`NONE = 0`、`HORIZONTAL = 1`、`VERTICAL = 2`、`RADIAL = 3`。
`clip_depth`（`params1.w`）：链长，见 §1 硬约束 4。

> **已删除** `FLAG_OPAQUE`（R05-11）：它原本只被编码器写、没有任何读者（shader 不读、后端也没有
> 无混合管线变体），而判据本身对 AA 边缘/边框也不成立（圆角边与边框环在任何像素上都不是
> 完全不透明）。`params1.w` 现在承载 `clip_depth` —— 有读者的语义，并且让「链长与 shader 循环
> 不一致」变成编码期可观测的错误，而不是静默少裁。

**裁剪链的构造规则（R05-10，对齐 Skia `SkClipStack` / WebRender 的 clip rect 合并）**：

1. 全部**轴对齐矩形** clip（四角半径全 0）在编码期求交，链上只有**一条**表项；该表项的
   `rect` 被**吸附到整数物理像素**（`lo = floor(x0)`、`hi = ceil(x1)`，Skia `SkIRect::roundOut`
   语义：向外取整，保证不漏裁）。吸附后 shader 才能用像素索引比较而不出现 1 ulp 的平局
   （AR2b P1-N1）；方向选「向外」是因为它与旧的「像素单元与矩形有交」判定**逐像素等价**，
   不改变任何像素的取舍；
2. **圆角** clip 各占一层（圆角形状的交集不是矩形，无法压成一条）；圆角项的 `rect` 按**覆盖规则**
   取整到物理像素（`lo = floor(x0)`、`hi = ceil(x1)`，与矩形项同一外扩语义），半径保持连续
   `PhysicalPx` —— 解析 AA 的几何只需要半径连续，矩形边界取整到像素不会改变像素取舍；
3. 完全相同的圆角 clip 按 `(parent, rect 位模式, radii 位模式)` **去重**；
4. 交集为空 / 零宽高的矩形项 ⇒ 吸附后写成 `w = h = 0` ⇒ shader 直接返回覆盖率 0（全裁）。
   不能对空交集做 `floor/ceil`：那会把反向区间变成 1 px 的保留区（`floor(10.6) = 10`、
   `ceil(10.4) = 11`）。

这样「滚动容器 + 画布区是同一个矩形」时覆盖率不再相乘（旧实现 0.5 × 0.5 = 0.25 的暗边），
矩形嵌套多少层都只占一项；`clip_depth` 只有在圆角层数真正超限时才可能触顶，且超限是**编码期
错误**（不是 release 下静默丢最外层）。

## 3. 颜色空间与混合（唯一转换点）

| 项       | 约定                                                                                                                                     |
|----------|------------------------------------------------------------------------------------------------------------------------------------------|
| 输入     | `color0`/`color1` 是 **linear sRGB、alpha 已预乘**（`LinearPremulRgba`）；shader **不做** sRGB→linear、**不做**预乘                      |
| 计算     | 全程 linear；SDF AA、边框、渐变、阴影、clip mask 都在 linear premultiplied 空间相乘/相加                                                 |
| 输出     | 片元输出 **linear premultiplied RGBA**，写进 **sRGB 格式**的渲染目标：D3D12 `*_UNORM_SRGB` / Metal `*.bgra8Unorm_srgb` / Vulkan `*_SRGB` |
| 编码     | linear→sRGB 由目标格式在写回时完成**一次**；混合发生在编码前的 linear 值上                                                               |
| 禁止     | shader 内 `pow(x, 2.2)` / `pow(x, 1/2.2)` / 手写 sRGB 曲线；对 R8 字形图集声明 sRGB 格式；对 sRGB 纹理再手动解码                         |
| 插值空间 | **唯一**：线性预乘。合成、AA、`gradient_eval`、CPU 侧 `ColorResolver::lerp` 全部同一套数学；引擎不提供第二种插值空间（旧的 `Interpolation::Oklab/Oklch` 是只影响死代码的伪配置，已删除，R05-7） |
| 函数锚点 | `apply_gamma(linear)` 在 v1 是**恒等函数**（`return c;`）：编码点只有目标格式一处                                                        |

blend state（三平台逐字相同）：

```text
src_color = ONE,  dst_color = ONE_MINUS_SRC_ALPHA,  color_op = ADD
src_alpha = ONE,  dst_alpha = ONE_MINUS_SRC_ALPHA,  alpha_op = ADD
write_mask = R | G | B | A
```

即 `dst' = src + dst × (1 − src.a)`。

## 4. 函数集（三套源码同名同语义）

| 函数                                        | 语义（逐字实现）                                                                                                    |
|---------------------------------------------|---------------------------------------------------------------------------------------------------------------------|
| `sd_rounded_rect(p, half_size, radii)`      | 按象限取半径后 `r = clamp(r, 0, min(half_size.x, half_size.y))`（**SDF 合法域**，防 `r > min/2` 时 SDF 反向「越裁越大」；CSS 的比例缩放在编码期做，见 §1）；`q = abs(p) - half_size + r`；`min(max(q.x,q.y),0) + length(max(q,0)) - r` |
| `aa_coverage(d)`                            | `w = max(length(ddx(d), ddy(d)), 1e-5); saturate(0.5 - d / w)`（1 px 解析 AA，`d` 单位 = 物理像素）。带宽取**梯度模长**（不是 `|ddx| + |ddy|`：后者让对角边的带宽变成 1.41 px） |
| `shadow_cdf(x)`                             | `0.5 × (1 − erf(x × 0.70710678))` = 标准正态尾概率 Φ(−x)；`erf` 用 Abramowitz–Stegun 7.1.26                        |
| `analytic_shadow(p, half_size, radii, blur)` | `σ = max(blur × 0.5, 0.35)`（CSS `box-shadow` 的模糊半径语义）；轴对齐矩形部分是**完整的可分离二维高斯**：一维因子 = 两条对边的尾概率之差 `shadow_cdf((|x|-h)/σ) − shadow_cdf((|x|+h)/σ)`（box ⊗ gaussian 的精确解，**不能**只取近边项），角部按「角部圆度 × r/(r+σ)」混合到 `Φ(−sdf/σ)`；`r = 0` 时退化为精确的分离式 |
| `gradient_eval(kind, p, half_size, c0, c1)` | 线性 `(p.x+hx)/(2hx)`；垂直 `(p.y+hy)/(2hy)`；径向 `length(p)/min(hx,hy)`；`saturate(t)` 后 `lerp`（premultiplied） |
| `clip_mask(world_px, clip_index, clip_depth)` | `clip_index == 0 ⇒ 1`；否则沿 `parent` 链跑 `min(clip_depth, KYTRIX_MAX_CLIP_ITEMS=9)` 层；**任一层 `w <= 0 或 h <= 0` ⇒ 直接返回 0**（零尺寸 clip 全裁，不是 0.5 的鬼线）。按 `meta.y` 分两种语义：**矩形项**（`KYTRIX_CLIP_KIND_RECT`）= 像素精确 scissor：`pixel = floor(world_px)` 与**编码期已吸附到整数物理像素**的 `[lo, hi)` 比较（`lo = floor(x0)`、`hi = ceil(x1)`，Skia `SkIRect::roundOut`），落在框外 ⇒ 返回 0，**不参与 AA 相乘**；**不得**用 `world_px ± 0.5` 之类的浮点单元比较（`world_px` 是插值属性，1 ulp 就会翻面，覆盖率 0/1 ⇒ 整列像素越界 = AR2b P1-N1）。**圆角项**（`KYTRIX_CLIP_KIND_ROUNDED`）= `m *= aa_coverage(sd_rounded_rect(...))`（圆角项的 `rect` 按覆盖规则取整到物理像素，半径保持连续；AA 只需要半径精确） |
| `apply_gamma(c)`                            | v1 恒等（见 §3）                                                                                                    |

`fwidth` 纪律：只在**实例级一致**的分支里调用（`kind` / `border_width` / `clip_index` 都是逐实例常量）。因此
`clip_mask` **不做**逐片元 AABB 早退；被完全裁掉的实例由顶点着色器的 `clip_rect` 夹紧退化成零面积四边形解决。

**clip 链的形状（R05-10 / AR2 P1-A）**：链上恒为「**至多一条矩形项**（链根）+ k 条圆角项
（k ≤ `KYTRIX_MAX_CLIP_DEPTH`）」。全部轴对齐矩形 clip 在编码期**求交合并**成一条（无损），
因此矩形层的覆盖率只有 0/1；圆角层才做解析 AA。完全相同的圆角层在编码期**去重**（交集幂等：
`A ∩ A = A`，重复层再乘一次覆盖率恒为错）。链长上界 = `KYTRIX_MAX_CLIP_ITEMS = MAX_CLIP_DEPTH + 1`
（矩形项不占圆角预算：`Canvas::push_clip` 与编码器都按 `MAX_CLIP_DEPTH` 计圆角层数，所以
「8 层圆角 + 祖先矩形 clip」合法）；圆角层数超限时编码期返回 `EngineError::ClipDepthExceeded`。

## 5. 顶点外扩、坐标与 NDC

```text
full_lo = bounds.xy - params0.w
full_hi = bounds.xy + bounds.zw + params0.w
lo = full_lo
hi = full_hi
if (clip_index != 0) {
    lo = max(lo, clip_rect.xy - 2.0)
    hi = min(hi, clip_rect.xy + max(clip_rect.zw, 0) + 2.0)
    hi = max(hi, lo)
}
world_px = lerp(lo, hi, corner01)        // corner01 ∈ {0,1}²
uv_t     = (world_px - full_lo) / max(full_hi - full_lo, 1e-6)
uv       = lerp(uv.xy, uv.zw, uv_t)      // 注意：**不是** corner01
```

`params0.w`（aa_padding，编码器唯一来源）：Quad **1.0**、Shadow **3σ + 1.0**（`σ = blur/2`）、图集
（Glyph/Image）**0.0**。
片元用**真实** `bounds` 求 `local_px = world_px − (bounds.xy + bounds.zw/2)`；扩出去的边缘覆盖率自然为 0。

**UV 必须按未夹紧的四边形求比例**：`clip_rect` 夹紧只为省片元，它改变的是几何覆盖范围，
不是图元自身的纹理映射。用 `corner01` 直接把 `uv.xy..uv.zw` 映射到夹紧后的四边形，会让
被裁剪的文本/图标整体拉伸压缩（滚动列表边缘的经典症状）。实色图元不看 UV，因此这条对
它们无影响；Glyph / Image 必须遵守。

坐标：用户空间恒为**左上原点、Y 向下、物理像素**；`z = 0, w = 1`。

| API           | NDC                            |
|---------------|--------------------------------|
| D3D12 / Metal | `(x·2/w_px − 1, 1 − y·2/h_px)` |
| Vulkan        | `(x·2/w_px − 1, y·2/h_px − 1)` |

FS **不依赖** `SV_Position` 的像素中心约定：片元只用插值出来的 `local_px` / `world_px`。

uber-quad 无顶点缓冲：`SV_VertexID` / `vertex_id` / `gl_VertexIndex` 取 4 顶点 unit quad（`(0,0),(1,0),(0,1),(1,1)`），
`SV_InstanceID` / `instance_id` / `gl_InstanceIndex` 取实例；实例下标以 `nointerpolation uint` / `[[flat]] uint` /
`flat in uint` 传给片元。**索引前必须先加 `draw.clip.y`**（见 §6 的实例基址表）。

## 6. `RootUniform`（112 B）与 `ClipEntry`（48 B）

`RootUniform`：`frame` @ 0（32 B）+ `draw` @ 32（80 B）。三平台各一个常量槽位（D3D12 root constants `b0` /
Metal `buffer(2)` / Vulkan push constants `[0, 112)`）。

| offset | 字段               | 语义                                                                         |
|--------|--------------------|------------------------------------------------------------------------------|
| 0      | `frame.viewport`   | `(w_px, h_px, 2/w_px, 2/h_px)`                                               |
| 16     | `frame.params`     | `(scale, scroll_x, scroll_y, 0)`；`.x` 只用于 geometry 的逻辑→物理换算，`.yz` 是**合成端滚动偏移**（物理像素；无滚动时恒 0） |
| 32     | `draw.basis`       | `(m11, m12, m21, m22)`；`x' = m11·x + m21·y + tx`、`y' = m12·x + m22·y + ty` |
| 48     | `draw.translation` | `(tx, ty, 0, 0)`                                                             |
| 64     | `draw.color0`      | linear premultiplied（`Ui` 的填充色 / `Textured` 的调制色）                  |
| 80     | `draw.clip_rect`   | 该 draw 的 clip 交集 AABB（物理浮点，`x,y,w,h`）                             |
| 96     | `draw.clip`        | `.x = clip_index`（0 = 无裁剪）、`.y = instance_base`、`.zw` 保留恒 0        |

**`frame.params.yz` = 合成端滚动偏移（B6-2）**

滚动**不重编码任何实例**：实例里的 `bounds` / `clip_rect` / `corner_radius` 全部烘焙在「滚动
容器内容空间」，滚动时 CPU 只更新 `FramePayload::scroll` 表（槽位 → 物理偏移），GPU 侧把它写进
本 op 的 `frame.params.yz`，由实例顶点 shader 在**最后一步**（`to_ndc` 之前）加到世界坐标上：

```hlsl
o.position = float4(to_ndc(world_px + frame.params.yz), 0.0, 1.0);  // quad.hlsl
o.position = float4(to_ndc(world_px + root.frame.params.yz, root.frame), 0.0, 1.0);  // quad.metal
gl_Position = vec4(to_ndc(world_px + root.frame.params.yz), 0.0, 1.0);  // quad.vert.glsl
```

三条硬约束：

1. **只加在屏幕映射上**：`local_px`、`uv`、`clip_rect` 与片元侧 `clip_mask` 的判定全部留在烘焙
   坐标空间 —— 因此裁剪链（`ClipEntry` 是绝对坐标）与偏移天然一致，不需要平移裁剪表；
2. **一条 op 只有一个偏移**：`FrameOp::Instances.scroll` 相同的相邻 lane 段才允许合并
   （`append_segment_ops` 在槽位变化时切断合并），否则一个 draw call 会把两组偏移加到同一批顶点；
3. **hardware scissor 不参与**：`damage` 的 scissor 是屏幕空间的脏矩形（不是 op 的 clip），
   平移顶点后它仍然正确；滚动帧的 damage 取滚动容器的视口（`scroll_damage`）。


**`draw.clip.y` = `instance_base`（实例基址的唯一通道）**

uber-quad 的每个 `FrameOp::Instances { start, len }` 段都要从 `instances[start]` 起读。三个图形
API 的硬件实例基址语义**互不相同**：

| API | 参数 | 是否加到 shader 里的实例索引 |
|---|---|---|
| D3D12 | `DrawInstanced(_, _, _, StartInstanceLocation)` | **否**（`SV_InstanceID` 不含它；这正是 HLSL SM 6.8 新增 `SV_StartInstanceLocation` 的动机，见 [HLSL 提案 0015](https://microsoft.github.io/hlsl-specs/proposals/0015-extended-command-info/)） |
| Vulkan | `vkCmdDraw(_, _, firstInstance, _)` | **是**（`gl_InstanceIndex` 含它） |
| Metal | `drawPrimitives(..., baseInstance:)` | **是**（`instance_id` 含它） |

因此**三平台一律把硬件基址传 0**，段起点只经 `draw.clip.y` 进入 shader：
`instances[instance_id + uint(draw.clip.y)]`。任何依赖硬件基址的写法都会在至少一个平台上错位，
且是静默错误像素、不报 API 错误。

`ClipEntry`（P09 从 CPU `ClipChain` 转换上传；**表项 0 恒为哨兵**）：

| offset | 字段            | 语义                                         |
|--------|-----------------|----------------------------------------------|
| 0      | `rect`          | 本层 clip `x, y, w, h`（物理像素）。**恒为整数物理像素**（编码期 `roundOut` 吸附，空交集 / 零宽高写成 `w = h = 0`）；圆角项的半径按 `radius × dpr` 保持连续 `PhysicalPx` |
| 16     | `corner_radius` | `tl, tr, br, bl`（物理像素）。矩形项恒为 0，圆角项至少一角 > 0（`meta.y` 与它一一对应，debug 自检交叉校验） |
| 32     | `meta`          | `.x = parent`（0 = 无裁剪），`.y = KYTRIX_CLIP_KIND_RECT / KYTRIX_CLIP_KIND_ROUNDED`，`.zw` 保留恒 0 |

**哨兵陷阱**：CPU 表项 0 的 `aabb = [f32::MIN, f32::MIN, f32::MAX, f32::MAX]` 按其 `(x,y,w,h)` 语义算出的右/下边界是
`MIN+MAX = 0`，**不是**无穷大矩形。任何对 `clip_rect` 的算术都必须在 `clip_index != 0` 时进行；表项 0 不得被读取。

## 7. 绑定表与管线状态

| 逻辑资源                  | D3D12                                 | Metal                                      | Vulkan                                    |
|---------------------------|---------------------------------------|--------------------------------------------|-------------------------------------------|
| `instances`（128 B 步进） | SRV `t0`（`StructureByteStride=128`） | `device const GpuPrimitive* [[buffer(0)]]` | set 0 binding 0，std430 `readonly buffer` |
| `clips`（48 B 步进）      | SRV `t1`                              | `[[buffer(1)]]`                            | binding 1，std430                         |
| `root`（112 B）           | root constants `b0`（28 DWORD）       | `[[buffer(2)]]`                            | push constants `[0, 112)`                 |
| glyph 图集（R8，1 层 array） | SRV `t2`（`ViewDimension=TEXTURE2D_ARRAY`） | `[[texture(0)]]`（`texture2d_array<float>`） | binding 4（combined `sampler2DArray`） |
| image 图集（BGRA8 sRGB，`page_count` 层 array） | SRV `t3`（`ViewDimension=TEXTURE2D_ARRAY`） | `[[texture(1)]]`（`texture2d_array<float>`） | binding 5（combined `sampler2DArray`） |
| sampler                   | static `s0`                           | `[[sampler(0)]]`                           | 同上                                      |

图集是 **texture array**（D-9）：字形固定 1 层（恒采样 layer 0）；图片的层数 = App 当前打开的
图集页数（`1..=MAX_IMAGE_PAGES`；没有图片时是 1 层的 1×1 占位数组），layer = 图集页索引。
层数**只增不减**：页数增长时数组重建为更多层且 App 重传每个已打开层；缩容会让所有窗口已有
`AtlasTile.page` 指向不存在的层，因此永不缩容。层号是**采样坐标**（`Texture2DArray.Sample` 的 z /
`texture2d_array.sample` 的 slice / GLSL `texture(sampler2DArray, vec3(uv, layer))`），不是
sampler/描述符索引 —— 因此三端都**不需要** bindless / descriptor indexing；描述符数组（bindless）
与伪数组不参与本契约。

采样器：`FILTER = LINEAR`、`ADDRESS = CLAMP_TO_EDGE`、无 mip（`LOD = 0`）、无比较、无各向异性。

管线状态：

* **uber-quad**：拓扑 **`TriangleStrip`**、**4 个顶点**、**`cull = NONE`**、无 index 缓冲、无深度/模板、MSAA 关闭。
  `TriangleList` 只会画出 4 个顶点中的一半（缺 index 缓冲补齐 6 顶点），**不得**使用。
* **geometry**：拓扑由 `GeometryDesc.topology`（`TriangleList` / `TriangleStrip`）决定，`cull = NONE`，无深度/模板，
  MSAA 关闭；顶点布局见下。
* 两者的 scissor 都由 damage 收口；`FrameOp` 顺序即绘制序。

geometry 顶点布局（P02 §9.4 冻结）：

| pipeline             | stride | 布局                                                    |
|----------------------|--------|---------------------------------------------------------|
| `Ui`                 | 12 B   | `position: float2`（逻辑像素）+ `coverage: float`（到形状边界的**有符号距离**，逻辑像素，负 = 内部） |
| `Textured { atlas }` | 16 B   | `position: float2` + `uv: float2`（归一化到对应图集页） |

`Ui` 的解析 AA（R05-6）：网格沿形状边界向外扩约 1 物理像素并携带覆盖率距离，片元
`aa_coverage(coverage × DPI)` 得到覆盖率 —— **不依赖 MSAA**（三后端一致，无 multisample 目标 /
resolve 附件）。`Textured` 没有覆盖率属性，其顶点写 `KYTRIX_AA_INSIDE` 让覆盖率为 1。

**覆盖率网格契约（AR2 P2-③，应用侧必须满足）**：顶点携带的是**单个标量**有符号距离，
因此每顶点只能表达一个梯度方向。要让 AA 带宽恒为 1 px、且不出现「尖角削角 / 共享边双重混合」，
应用生成的覆盖率网格必须满足：①距离场沿法向 **|∇d| = 1**（线性近似，禁止在 AA 带内插值非线性
距离）；②AA 带三角形**互不重叠**（相邻三角形共享边时覆盖率只由一侧写入，否则 0.5 ⊕ 0.5 = 0.75
出现暗线）；③尖角处按 Skia `GrAATriangulator` 的做法为每条边生成独立外扩三角形（顶点携带不同
覆盖率），而不是复用同一个顶点标量。引擎**不做** tessellation；不满足上述条件的网格不会报错，
只会出现上面列出的走样。这与「与 Skia 的 AA 三角化一致」的旧表述不同：Skia 在 CPU 侧完成了
三角化 + 覆盖率顶点生成，本引擎把它交给应用。

### `GeometryDraw.range`（E4：CPU 侧索引子区间）

`GeometryDraw.range` 是**索引元素**的半开区间（与 `IndexType` 无关；`TriangleStrip` 同样按索引计），
**只在 CPU 侧裁剪，不进 `GpuPrimitive`、不改本文件任何字节布局或 shader 源**：

| 项         | 契约                                                                                                                                                                                                                   |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 整份       | `0..u32::MAX`（`GeometryDraw::full`）。用哨兵而不是 `0..index_count`：几何可被 `update_*` / `truncate_geometry` 改变长度，真正的上界在绘制期裁剪                                                                        |
| 空绘制     | 空区间 / `start >= end` / `start >= index_count` ⇒ 无 op、无 damage、无 draw call                                                                                                                                     |
| 越界       | `end > index_count` 的一端 **clamp**（几何可能被 `truncate_geometry` 变短，不报错）；越界不产生越界读                                                                                                                 |
| 三角形对齐 | `TriangleList` 的 range 两端**向下**对齐到 3 的倍数（起点向下取整、长度向下取整；起点不在三角形边界时最多多画起点之前的 2 个索引，不会漏画、不会错位）；块边界也是 3 的倍数，因此逐块绘制与单次 `DrawIndexed` 画整段等价。`TriangleStrip` 是单块拓扑，不截断                                                                    |
| 块求交     | 引擎把 range 与 4096 B 页级上传的**块表**求交，逐块产出 op（块间顺序 = 索引顺序）；`first_index` / `index_count` 是**索引缓冲里的绝对索引区间**                                                                         |
| 三端一致   | 三端 `DrawIndexed` 只消费裁剪后的区间（`GeometryDrawGpu::draw_indexed` 是唯一入口，参数由纯函数 `draw_indexed_params` 推导并单测）：D3D12 / Vulkan 直接给 `first_index`，Metal 的 `indexBufferOffset` = `first_index × index_type.size()`；vertex offset / base vertex 恒 0 |
| 错误       | 区间越过**已上传**的索引数是引擎内部不变量被破坏 ⇒ `EngineError::GeometryRegister`（不静默越界读）；应用侧越界按上表 clamp                                                                                            |

块大小（`GEOMETRY_CHUNK_INDICES = 12288` 索引）是引擎内部策略（主设计 §5.5），应用不可见、本文件
也不暴露。视觉回归（I8）必须覆盖「同几何、不同 range」的 case：可见三角形集合变化 ⇒ 像素快照变化。

### 呈现色彩空间（R06-10）

引擎只产出一件事：**sRGB 编码的 8-bit RGBA**（`apply_gamma` 恒等，编码由 sRGB 渲染目标完成一次）。
呈现面必须**显式声明**它是 sRGB / Rec.709，决定点在共享层 `render/colorspace.rs`：

| 平台 | 显式声明 | API |
|------|----------|-----|
| Windows | `DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709` | `IDXGISwapChain3::SetColorSpace1`（+ `CheckColorSpaceSupport`） |
| macOS | `kCGColorSpaceSRGB` | `CAMetalLayer.setColorspace` |
| Linux | `VK_COLOR_SPACE_SRGB_NONLINEAR_KHR` | surface format 筛选 + `VkSwapchainCreateInfoKHR::imageColorSpace` |

三端不得各自决定；HDR（`G2084_NONE_P2020` / `extendedRange`）**显式不支持**：接入时需要新增
`PresentedColorSpace` 变体并让三端的 `match` 逐端处理（渲染目标格式、混合空间、背景色绝对值、
图集纹理格式都要重估）。

## 8. 图集采样

| 项         | 约定                                                                                                                                           |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------|
| UV         | `AtlasTile::uv()` 的**边**约定；**不加**半纹素偏移；三平台纹理原点都是左上、v 向下，**不做 v 翻转**；UV 的归一化基准是**该 tile 所在页**的尺寸 |
| 采样       | 图集是 texture array：glyph 恒 `layer = 0`；image 的 `layer = params1.y`（= `AtlasTile.page`）。应用注册的静态纹理几何（`geometry.textured_*`）没有页概念，固定 `layer = 0` |
| glyph 图集 | `R8_UNORM`（线性覆盖度，**非** sRGB）；`out = color0 × texel.r`（4 分量同乘）                                                                  |
| image 图集 | `BGRA8` **sRGB**；纹素是 **linear-premultiplied 后 sRGB 编码**；`out = texel × color0`                                                         |
| 纹理视图   | image 图集的 SRV / 纹理视图**必须**声明为 sRGB 格式（`*_UNORM_SRGB` / `.bgra8Unorm_srgb` / `*_SRGB`）；glyph 图集必须保持非 sRGB 的 `R8_UNORM` |
| 上传方职责 | P15 必须「linear 空间预乘 → sRGB 编码」后写图集；改成直通 alpha 时 shader 与本文件必须同时改（`texel.rgb *= texel.a`）                         |

**CPU 侧 `AtlasTile`（不进 GPU 字节布局）**：`paint/atlas.rs` 的 `AtlasTile` 是 I2 的**CPU 数据契约**
（`{ atlas, page, u0, v0, u1, v1, generation }`，v2.1 E1d 起再带分配身份 `slot: u32` / `slot_epoch: u32`）。
GPU 只消费 `uv()`（上表 UV 行）、`page`（编码进 `params1.y`）与 `atlas`（`params1.z` 的 `kind` 由来）；
`slot` / `slot_epoch` 只是 CPU 侧**淘汰检测身份**（`render/atlas.rs` 的 `AtlasAllocator::is_alive`
用它做 O(1) 判活，供编码期过滤失效字形 run），既不在 `GpuPrimitive` 的 128 B 里，也不出现在任何
shader / uniform / 顶点布局中。因此 `AtlasTile` 的 CPU 侧尺寸变化（24 → 32 B）**不是**渲染契约变更：
三端 shader、绑定表与 `GpuPrimitive` 偏移都不受影响，无需改动本文件 §1–§7。

**CPU 侧：文本装饰与 InlineBox 绘制（v2.1 E2，不进 GPU 字节布局）**：`paint/pass.rs` 把
`ShapedTextLine.decorations`（下划线/删除线）与 `InlineBoxPaint::{Quad, Image}` 编码成**既有的**
`Primitive::Quad` / `Primitive::Image`（I2：不新增图元变体、不改 `GpuPrimitive`）。装饰矩形就是普通
实色 quad（`color0` + 全 0 `corner_radius`/`params0`），InlineBox 图片就是普通图集 image（`uv` 来自
`ResourceImage.tile`，`color0` = 调制色）。因此三端 shader / 绑定 / §1–§7 **零变更**，也不触发
参数 golden（I8）；像素 golden 只受「单 run 无装饰」路径影响（与改动前逐位一致）。

## 9. 禁止项

1. 位重解释：`asuint` / `asfloat` / `as_type<>` / `floatBitsToUint` / `floatBitsToInt` / `bitcast`。
2. 矩阵类型与 `mul()`：只用两个 `float4` + 显式乘加（规避三平台 row/column-major 差异）。
3. shader 内 gamma/色彩空间转换（§3）。
4. 逐片元发散分支里的 `fwidth`（§4）。
5. 读取保留位/保留字段、读哨兵表项 0。
6. shader 内做 OKLCH/OKLab 转换、渐变 LUT 生成（主设计 §5.8.2）。
7. 浮点 `%`、`discard`（用 `return 0`；premultiplied over 下 `src = 0` 等价于不写）。

## 10. 变更流程

字段/常量/绑定/坐标/颜色空间的任何变更：改 Rust → 改 14 个源文件 → 改本文件 → `cargo build -p ui_core`
（`build.rs` 的静态校验会拦住三套源码与 Rust 的漂移）→ 跑像素快照回归（P18）。
