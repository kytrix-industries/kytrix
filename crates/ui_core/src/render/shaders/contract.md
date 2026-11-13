# 渲染契约（`render/shaders/contract.md`）— 唯一权威

本文件是 CPU 侧 `GpuPrimitive`、三套 shader（`*.hlsl` / `*.metal` / `*.glsl`）与三套 renderer 之间的**唯一契约**。
字段顺序、offset、取值、绑定、坐标与颜色空间的任何变更，必须**同时**改：

1. `paint/instance.rs`（`GpuPrimitive` 与 `KIND_*`/`FLAG_*`/`GRADIENT_*` 常量）；
2. 本目录下 14 个源文件（`common.*` / `quad.*` / `geometry.*`）；
3. 本文件；
4. 触发像素快照回归（主设计 I8）。

`../../..` 会在**每次构建**时比对本文件的锚点与三套源码的结构体/常量，漂移即构建失败（见
`../../../design/phase-07-shaders.md` §6）。设计依据：主设计 §2.3/§2.11/§5.4/§5.8、P01 §7（AA）、P02 §8/§9/§10、P06 §7。

---

## 1. `GpuPrimitive`：128 B / 16 B 对齐

| offset  | 字节 | 字段            | 类型                          | 语义                                                                                     |
|---------|------|-----------------|-------------------------------|------------------------------------------------------------------------------------------|
| **0**   | 16   | `bounds`        | `[f32;4]` / `float4` / `vec4` | `x, y, w, h`：SDF 图元为**物理像素、未取整浮点**；图集图元为**整数矩形的浮点表示**       |
| 16      | 16   | `corner_radius` | 同上                          | `tl, tr, br, bl`（物理像素，逐角 `<= min(w,h)/2`，编码器已 clamp）                       |
| 32      | 16   | `color0`        | 同上                          | **linear premultiplied** sRGB RGBA：实色 / 渐变起色 / 阴影色 / 字形色 / 图片调制色       |
| 48      | 16   | `color1`        | 同上                          | 渐变终色 **或** 边框色（互斥）；无则全 0                                                 |
| 64      | 16   | `params0`       | 同上                          | `border_width, shadow_blur, gradient_kind, aa_padding`（物理像素）                       |
| 80      | 16   | `params1`       | 同上                          | `clip_index, texture_index, kind, flags`（**整数编码进 f32**）                           |
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
4. `flags` bit 0 = `OPAQUE`，bit 1..=23 **保留恒 0**；`params1.y` 对非图集 kind 写 0 并忽略；`uv` 对非图集 kind 写 0 并忽略；
   `color1` 无用途时写 0。shader **不得**读取保留位/保留字段来决定分支。
5. `bounds.zw` 必须 `>= 0` 且有限（编码器保证）；shader 仍取 `max(bounds.zw, 0)` 作为防御性一致。

## 2. `kind` / `texture_index` / `gradient_kind` / `flags`

| `kind`        | 值    | 产生者                                 | FS 分支                                    |
|---------------|-------|----------------------------------------|--------------------------------------------|
| `KIND_QUAD`   | **0** | `Primitive::Quad`                      | SDF + 实色/渐变/边框                       |
| `KIND_GLYPH`  | **1** | `Primitive::GlyphRun`（每字形 1 实例） | `color0 × texel.r`                         |
| `KIND_IMAGE`  | **2** | `Primitive::Image`                     | `texel × color0`                           |
| `KIND_SHADOW` | **3** | `Primitive::Shadow`                    | SDF + erf 解析阴影                         |
| 保留          | 4     | —                                      | Path 两阶段 coverage（v1 不用，F-26）      |
| 保留          | 5     | —                                      | Geometry 实例化表达（v1 走 geometry 通道） |
| 保留          | 6–7   | —                                      | 应用自定义（v1 不做，F-29）                |

`texture_index`：`TEXTURE_GLYPH = 0`（R8 覆盖度图集）、`TEXTURE_IMAGE = 1`（BGRA8 sRGB 图集）。
`gradient_kind`：`NONE = 0`、`HORIZONTAL = 1`、`VERTICAL = 2`、`RADIAL = 3`。
`flags`：`FLAG_OPAQUE = 1`（bit 0）。判据由编码器唯一实现：

```text
opaque = kind ∈ {QUAD, GLYPH, IMAGE} && gradient_kind == 0 && color0.is_opaque()
         && (kind != QUAD || border_width == 0 || color1.is_opaque())
```

shader **不使用** `FLAG_OPAQUE`：它只供 P09 选「无混合管线变体」。`src.a == 1` 时
`src + dst×(1−src.a) ≡ src`，两个变体的结果逐位相同。

## 3. 颜色空间与混合（唯一转换点）

| 项       | 约定                                                                                                                                     |
|----------|------------------------------------------------------------------------------------------------------------------------------------------|
| 输入     | `color0`/`color1` 是 **linear sRGB、alpha 已预乘**（`LinearPremulRgba`）；shader **不做** sRGB→linear、**不做**预乘                      |
| 计算     | 全程 linear；SDF AA、边框、渐变、阴影、clip mask 都在 linear premultiplied 空间相乘/相加                                                 |
| 输出     | 片元输出 **linear premultiplied RGBA**，写进 **sRGB 格式**的渲染目标：D3D12 `*_UNORM_SRGB` / Metal `*.bgra8Unorm_srgb` / Vulkan `*_SRGB` |
| 编码     | linear→sRGB 由目标格式在写回时完成**一次**；混合发生在编码前的 linear 值上                                                               |
| 禁止     | shader 内 `pow(x, 2.2)` / `pow(x, 1/2.2)` / 手写 sRGB 曲线；对 R8 字形图集声明 sRGB 格式；对 sRGB 纹理再手动解码                         |
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
| `sd_rounded_rect(p, half_size, radii)`      | 按象限取半径；`q = abs(p) - half_size + r`；`min(max(q.x,q.y),0) + length(max(q,0)) - r`                            |
| `aa_coverage(d)`                            | `w = max(fwidth(d), 1e-5); saturate(0.5 - d / w)`（1 px 解析 AA，`d` 单位 = 物理像素）                              |
| `analytic_shadow(d, blur)`                  | `σ = max(blur/3, 0.35)`；`0.5 × (1 − erf(d / (σ√2)))`；`erf` 用 Abramowitz–Stegun 7.1.26                            |
| `gradient_eval(kind, p, half_size, c0, c1)` | 线性 `(p.x+hx)/(2hx)`；垂直 `(p.y+hy)/(2hy)`；径向 `length(p)/min(hx,hy)`；`saturate(t)` 后 `lerp`（premultiplied） |
| `clip_mask(world_px, clip_index)`           | `clip_index == 0 ⇒ 1`；否则沿 `parent` 链 ≤ 8 层，`m *= aa_coverage(sd_rounded_rect(...))`                          |
| `apply_gamma(c)`                            | v1 恒等（见 §3）                                                                                                    |

`fwidth` 纪律：只在**实例级一致**的分支里调用（`kind` / `border_width` / `clip_index` 都是逐实例常量）。因此
`clip_mask` **不做**逐片元 AABB 早退；被完全裁掉的实例由顶点着色器的 `clip_rect` 夹紧退化成零面积四边形解决。

## 5. 顶点外扩、坐标与 NDC

```text
lo = bounds.xy - params0.w
hi = bounds.xy + bounds.zw + params0.w
if (clip_index != 0) {
    lo = max(lo, clip_rect.xy - 2.0)
    hi = min(hi, clip_rect.xy + max(clip_rect.zw, 0) + 2.0)
    hi = max(hi, lo)
}
world_px = lerp(lo, hi, corner01)        // corner01 ∈ {0,1}²
```

`params0.w`（aa_padding，编码器唯一来源）：Quad **1.0**、Shadow **blur + 1.0**、图集（Glyph/Image）**0.0**。
片元用**真实** `bounds` 求 `local_px = world_px − (bounds.xy + bounds.zw/2)`；扩出去的边缘覆盖率自然为 0。

坐标：用户空间恒为**左上原点、Y 向下、物理像素**；`z = 0, w = 1`。

| API           | NDC                            |
|---------------|--------------------------------|
| D3D12 / Metal | `(x·2/w_px − 1, 1 − y·2/h_px)` |
| Vulkan        | `(x·2/w_px − 1, y·2/h_px − 1)` |

FS **不依赖** `SV_Position` 的像素中心约定：片元只用插值出来的 `local_px` / `world_px`。

uber-quad 无顶点缓冲：`SV_VertexID` / `vertex_id` / `gl_VertexIndex` 取 4 顶点 unit quad（`(0,0),(1,0),(0,1),(1,1)`），
`SV_InstanceID` / `instance_id` / `gl_InstanceIndex` 取实例；实例下标以 `nointerpolation uint` / `[[flat]] uint` /
`flat in uint` 传给片元。

## 6. `RootUniform`（112 B）与 `ClipEntry`（48 B）

`RootUniform`：`frame` @ 0（32 B）+ `draw` @ 32（80 B）。三平台各一个常量槽位（D3D12 root constants `b0` /
Metal `buffer(2)` / Vulkan push constants `[0, 112)`）。

| offset | 字段               | 语义                                                                         |
|--------|--------------------|------------------------------------------------------------------------------|
| 0      | `frame.viewport`   | `(w_px, h_px, 2/w_px, 2/h_px)`                                               |
| 16     | `frame.params`     | `(scale, 0, 0, 0)`；**只**用于 geometry 的逻辑→物理换算                      |
| 32     | `draw.basis`       | `(m11, m12, m21, m22)`；`x' = m11·x + m21·y + tx`、`y' = m12·x + m22·y + ty` |
| 48     | `draw.translation` | `(tx, ty, 0, 0)`                                                             |
| 64     | `draw.color0`      | linear premultiplied（`Ui` 的填充色 / `Textured` 的调制色）                  |
| 80     | `draw.clip_rect`   | 该 draw 的 clip 交集 AABB（物理浮点，`x,y,w,h`）                             |
| 96     | `draw.clip`        | `.x = clip_index`（0 = 无裁剪），`.yzw` 保留恒 0                             |

`ClipEntry`（P09 从 CPU `ClipChain` 转换上传；**表项 0 恒为哨兵**）：

| offset | 字段            | 语义                                         |
|--------|-----------------|----------------------------------------------|
| 0      | `rect`          | 本层 clip `x, y, w, h`（物理像素）           |
| 16     | `corner_radius` | `tl, tr, br, bl`（物理像素）                 |
| 32     | `meta`          | `.x = parent`（0 = 无裁剪），`.yzw` 保留恒 0 |

**哨兵陷阱**：CPU 表项 0 的 `aabb = [f32::MIN, f32::MIN, f32::MAX, f32::MAX]` 按其 `(x,y,w,h)` 语义算出的右/下边界是
`MIN+MAX = 0`，**不是**无穷大矩形。任何对 `clip_rect` 的算术都必须在 `clip_index != 0` 时进行；表项 0 不得被读取。

## 7. 绑定表与管线状态

| 逻辑资源                  | D3D12                                 | Metal                                      | Vulkan                                    |
|---------------------------|---------------------------------------|--------------------------------------------|-------------------------------------------|
| `instances`（128 B 步进） | SRV `t0`（`StructureByteStride=128`） | `device const GpuPrimitive* [[buffer(0)]]` | set 0 binding 0，std430 `readonly buffer` |
| `clips`（48 B 步进）      | SRV `t1`                              | `[[buffer(1)]]`                            | binding 1，std430                         |
| `root`（112 B）           | root constants `b0`（28 DWORD）       | `[[buffer(2)]]`                            | push constants `[0, 112)`                 |
| glyph 图集（R8）          | SRV `t2`                              | `[[texture(0)]]`                           | binding 4（combined `sampler2D`）         |
| image 图集（BGRA8 sRGB）  | SRV `t3`                              | `[[texture(1)]]`                           | binding 5（combined `sampler2D`）         |
| sampler                   | static `s0`                           | `[[sampler(0)]]`                           | 同上                                      |

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
| `Ui`                 | 8 B    | `position: float2`（逻辑像素）                          |
| `Textured { atlas }` | 16 B   | `position: float2` + `uv: float2`（归一化到对应图集页） |

## 8. 图集采样

| 项         | 约定                                                                                                                                           |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------|
| UV         | `AtlasTile::uv()` 的**边**约定；**不加**半纹素偏移；三平台纹理原点都是左上、v 向下，**不做 v 翻转**                                            |
| glyph 图集 | `R8_UNORM`（线性覆盖度，**非** sRGB）；`out = color0 × texel.r`（4 分量同乘）                                                                  |
| image 图集 | `BGRA8` **sRGB**；纹素是 **linear-premultiplied 后 sRGB 编码**；`out = texel × color0`                                                         |
| 纹理视图   | image 图集的 SRV / 纹理视图**必须**声明为 sRGB 格式（`*_UNORM_SRGB` / `.bgra8Unorm_srgb` / `*_SRGB`）；glyph 图集必须保持非 sRGB 的 `R8_UNORM` |
| 上传方职责 | P15 必须「linear 空间预乘 → sRGB 编码」后写图集；改成直通 alpha 时 shader 与本文件必须同时改（`texel.rgb *= texel.a`）                         |

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
