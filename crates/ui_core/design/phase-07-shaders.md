# Phase 07 · 三套 shader + `build.rs` + 渲染契约 — 详细设计（v1）

> 状态：设计定稿候选（2026-11-12）。本阶段**不写生产代码**：交付物是这份设计 + `design/DEVIATIONS.md` 追加。
> 事实基线：Windows SDK **10.0.26100.0** 的 `dxc.exe`（`dxcompiler.dll 1.9(5184) / 1.9.2602.17`）、
> 已落地 `..`（P06）、`design/phase-01-foundation.md` §7、`phase-02-paint-foundation.md` §8/§9/§10、
> `phase-06-paint-runtime.md` §7、主设计 §2.3/§2.11/§5.4/§5.5/§5.8/§6.5/附录 B。
> 本机 **只能验证 HLSL**（dxc），MSL/GLSL 为静态审查（§8 R-1/R-2）。

---

## 0. 结论摘要（先给可执行的定死项）

| #  | 定死项                                                                                                                                                                                                                                                                       | 一句话理由                                                                                                                                                                                                        |
|----|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 1  | 实例缓冲 = **只读 structured / storage buffer**（D3D12 `StructuredBuffer<GpuPrimitive>` SRV `t0` / Metal `device const GpuPrimitive* [[buffer(0)]]` / GLSL `std430 readonly buffer` binding 0），**不是 CBV**                                                                | D3D12 CBV 单块上限 64 KB = 512 个实例，且 per-instance 偏移要求 256 B 对齐（+100% 浪费）；SRV/SSBO 可直接下标寻址                                                                                                 |
| 2  | uber-quad **无顶点缓冲**：`SV_VertexID` / `vertex_id` / `gl_VertexIndex` 取 4 顶点 unit quad，`SV_InstanceID` / `instance_id` / `gl_InstanceIndex` 取实例；片元用 `nointerpolation uint` / `[[flat]] uint` / `flat in uint` 传实例下标（**不依赖 PS 里的 `SV_InstanceID`**） | 跨 API 最稳；整数 I/O 必须显式非插值（主设计 §2.11）                                                                                                                                                              |
| 3  | 坐标：用户空间恒为**左上原点、Y 向下、物理像素**；`D3D12/Metal: ndc=(2x/w−1, 1−2y/h)`，`Vulkan: (2x/w−1, 2y/h−1)`，`z=0,w=1`                                                                                                                                                 | Y 方向只在 VS 的 `to_ndc` 一处出现，FS 永不接触 NDC                                                                                                                                                               |
| 4  | 颜色：`color0/color1` = **linear premultiplied**；渲染目标 = **sRGB 格式**；linear→sRGB 编码由目标格式在写回时完成**恰好一次**；shader 内**不做** gamma，`apply_gamma` 在 v1 是**恒等函数**                                                                                  | 三段式（计算线性、混合线性、编码一次）在三平台都成立；shader 内转换会双重编码                                                                                                                                     |
| 5  | blend 写死：`src=ONE, dst=ONE_MINUS_SRC_ALPHA, op=ADD`，RGB 与 A 同公式，`WriteMask=RGBA`                                                                                                                                                                                    | premultiplied over；`FLAG_OPAQUE` 的无混合变体结果**逐位相同**（`src.a==1 ⇒ dst×0`）                                                                                                                              |
| 6  | 顶点外扩：VS 以 `bounds` 四边向外扩 `params0.w`（Quad 1.0 / Shadow blur+1.0 / 图集 0），FS 用 `bounds` 真实矩形求 SDF；有 clip 时再以 `clip_rect ± 2 px` 夹紧（**纯优化**）                                                                                                  | F-13 的 AA 契约；2 px 是 1 px AA 半径的保守余量，保证夹紧不丢覆盖率                                                                                                                                               |
| 7  | 新增 `FrameUniform`（32 B）、`GeometryDrawUniform`（80 B，**从 P02 的 48 B 扩出**）与 `ClipEntry`（48 B/层，parent 链 ≤8）；前两者合并为**一个 112 B 常量块** `RootUniform`（`frame @0`、`draw @32`），三平台各用一个槽位                                                    | `FrameOp::Geometry` 自带 clip（P06 V-53），48 B 布局无处传；合并成一块后 D3D12 用 28 个 root constant DWORD、Vulkan 用 112 B push constant（≤128 B 下限）、Metal 用一个 112 B buffer，三者字节序完全一致。见 F-68 |
| 8  | 阴影：`σ = max(blur/3, 0.35)`，`erf` 用 Abramowitz–Stegun 7.1.26；`3σ = blur` 与 `params0.w = blur+1` 的 damage/顶点外扩自洽（顶点边界覆盖率残差 ≤ 2.2e-3 ≈ 0.5/255）                                                                                                        | 若按 CSS `σ=blur/2`，外扩边界残差 2.3%（≈6/255）会留下可见矩形边                                                                                                                                                  |
| 9  | `build.rs` **零依赖**（std only；不引 `sha2`，Cargo 的 fingerprint 就是缓存）；Windows 调 dxc（实测 7 个 entry 全绿、187 ms、产物 4–8 KB、magic `DXBC`）                                                                                                                     | 见 §5/§6；静态校验用**手写扫描器**（无 regex，无 naga/WGSL）                                                                                                                                                      |
| 10 | 构建期静态校验**永远运行**（不受 `ui_core_SKIP_SHADERS` 影响）；跳过编译时写**占位产物**，使 `include_bytes!` 在跨 target `cargo check` 时仍能解析                                                                                                                           | 校验是纯文本比对，零成本；占位产物让 P09 的 `include_bytes!` 不依赖构建顺序                                                                                                                                       |

---

## 1. 范围、文件与非目标

### 1.1 本阶段交付（P07 实现期落地的文件）

```text
crates/ui_core/
├── Cargo.toml                      # [package] build = "build.rs"（无 [build-dependencies]，零依赖）
├── build.rs                        # 编译 + 静态校验（唯一构建期入口）
└── src/render/shaders/
    ├── contract.md                 # 唯一权威（本文件 §2 逐字复制）
    ├── common.hlsl / common.metal / common.glsl          # 常量、共享结构、SDF/AA/阴影/渐变/clip/gamma
    ├── quad.hlsl   / quad.metal   / quad.vert.glsl + quad.frag.glsl
    └── geometry.hlsl / geometry.metal / geometry.ui.vert.glsl + geometry.textured.vert.glsl
                                     + geometry.ui.frag.glsl + geometry.textured_image.frag.glsl
                                     + geometry.textured_glyph.frag.glsl
```

源码共 **14** 个文件：HLSL 3、MSL 3、GLSL **8**（1 个 `common.glsl` + 7 个 stage 文件）。GLSL 不能像 HLSL/MSL 那样
在单文件里放多个入口：一个编译单元只能一个 stage，且 `#version` 必须在首行。→ **S-68 / D-1**。

### 1.2 明确的非目标

| 不做                                                      | 归属                                                   |
|-----------------------------------------------------------|--------------------------------------------------------|
| `src/render.rs` / 后端 / 根签名 / 描述符堆 / 管线状态对象 | P09（`src/render.rs` 由 shared 层与 D3D12 同阶段落地） |
| `ShaderSet` 加载器、`EngineError::ShaderLoad` 的生产者    | P09（P07 只固定**产物名与 magic**，见 §5.4）           |
| 像素快照 golden、软件参考光栅器                           | P18（本阶段只给 `L3` 的覆盖矩阵与容差策略，§7.4）      |
| 新依赖、`naga`/WGSL、单源翻译                             | D11 明确禁止                                           |
| `#[allow]`/`#[expect]`/`unsafe`                           | AGENTS §4；`build.rs` 同样适用                         |

### 1.3 与已落地代码的关系

`paint/instance.rs` 的 128 B 布局与 `KIND_*`/`FLAG_*`/`GRADIENT_*` 常量是**唯一事实源**：`build.rs` 解析它并与三套源码比对
（§6）。P07 **不改** `paint/`、`math/`、`window/` 任何一行。

---

## 2. `render/shaders/contract.md` 正文（实现期**逐字**复制）

> 本节是唯一权威。任何字段、取值、绑定、坐标约定的变更必须同时改 **Rust + 三套源码 + 本节**，并触发像素快照回归（I8）。
> 主设计 §2.3/附录 B 的 `params0.w = aa_scale` 已被 F-13 取代为 `aa_padding`；本文与 F-13 一致。

### 2.1 `GpuPrimitive`：128 B / 16 B 对齐

| offset  | 字节 | 字段            | 类型                          | 语义                                                                                   |
|---------|------|-----------------|-------------------------------|----------------------------------------------------------------------------------------|
| **0**   | 16   | `bounds`        | `[f32;4]` / `float4` / `vec4` | `x, y, w, h`，**物理像素，未取整浮点**（SDF 图元）或**整数矩形的浮点表示**（图集图元） |
| 16      | 16   | `corner_radius` | 同上                          | `tl, tr, br, bl`，物理像素，逐角 `<= min(w,h)/2`（编码器已 clamp）                     |
| 32      | 16   | `color0`        | 同上                          | **linear premultiplied** sRGB RGBA：实色 / 渐变起色 / 阴影色 / 字形色 / 图片调制色     |
| 48      | 16   | `color1`        | 同上                          | 渐变终色 **或** 边框色（互斥）；无则全 0                                               |
| 64      | 16   | `params0`       | 同上                          | `border_width, shadow_blur, gradient_kind, aa_padding`（物理像素）                     |
| 80      | 16   | `params1`       | 同上                          | `clip_index, texture_index, kind, flags`（**整数编码进 f32**）                         |
| 96      | 16   | `uv`            | 同上                          | 图集 tile `u0, v0, u1, v1`（归一化，**用边**，无半纹素偏移）                           |
| 112     | 16   | `clip_rect`     | 同上                          | clip 链**交集 AABB**（物理浮点，`x,y,w,h`）                                            |
| **128** | —    | **总计**        | —                             | `size_of == 128`、`align_of == 16`、8 个 `offset_of!` 断言（已落地）                   |

**硬约束**

1. 实例/常量结构体**只允许 16 B 成员**（`float4` / `uint4` / `[f32;4]`）：HLSL cbuffer 的 16 B 打包、MSL 自然对齐、
   std140/std430 在「全 vec4」时逐字节一致；出现 `float3`/标量就会三平台分叉。
2. **实例缓冲基址必须 16 B 对齐**（F-23）；buffer 内偏移按 128 B 步进。若某后端改为「一条大常量缓冲 + 动态偏移」承载
   `RootUniform`，偏移起点必须按 256 B 向上取整（D3D12 CBV 的对齐要求；只影响 P09 的上传布局，不影响本节字段偏移）。
3. `params1` 的四个分量是**整数编码进 f32**（`value as f32`，取值 `[0, 2^24)`）。三套 shader 一律用**数值转换**取回：
   `(uint)x` / `uint(x)`。**禁止位重解释**：`asuint` / `asfloat` / `as_type<>` / `floatBitsToUint` / `floatBitsToInt` /
   `bitcast` 一律不得出现（`build.rs` 扫描，见 §6.1）。f32 里存的是**数值**而不是整数的位模式，位重解释会得到完全错误的
   `kind`/`flags`（N2）。
4. `flags` bit 0 = `OPAQUE`，bit 1..=23 **保留恒 0**；`params1.y` 对非图集 kind **写 0 并忽略**；`uv` 对非图集 kind 写 0
   并忽略；
   `color1` 无用途时写 0。shader **不得**读取保留位/保留字段来决定分支（两个真值来源）。
5. `bounds` 的 `w/h` 必须 `>= 0` 且有限（编码器已保证跳过空/非有限图元）；shader 仍对 `bounds.zw` 取 `max(…, 0)` 作为
   防御性一致（不影响合法输入）。

### 2.2 `kind` / `texture_index` / `gradient_kind` / `flags`

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
`flags`：`FLAG_OPAQUE = 1`（bit 0）。判据（编码器唯一实现，shader 只把它用于管线选择）：

```text
opaque = kind ∈ {QUAD, GLYPH, IMAGE} && gradient_kind == 0 && color0.is_opaque()
         && (kind != QUAD || border_width == 0 || color1.is_opaque())
```

**shader 侧唯一使用点**：无。`FLAG_OPAQUE` 只供 P09 选「无混合管线变体」；因为 `src.a == 1` 时
`src + dst×(1−src.a) ≡ src`，两个变体的结果**逐位相同**，不需要在 shader 里分支。

### 2.3 颜色空间与混合（唯一转换点）

| 项             | 约定                                                                                                                                                                                                  |
|----------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 输入           | `color0/color1` 是 **linear sRGB、alpha 已预乘**（`LinearPremulRgba`），禁止在 shader 里做 sRGB→linear 或预乘                                                                                         |
| 计算           | 全程 linear；SDF AA、边框、渐变、阴影、clip mask 都在 linear premultiplied 空间相乘/相加                                                                                                              |
| 输出           | 片元输出 **linear premultiplied RGBA**，写进 **sRGB 格式**的渲染目标：D3D12 `*_UNORM_SRGB` / Metal `*.bgra8Unorm_srgb` / Vulkan `*_SRGB`                                                              |
| 编码           | linear→sRGB 由目标格式在写回时完成**一次**；**混合发生在编码前的 linear 值上**（D3D12/Metal/Vulkan 对 sRGB attachment 都是如此，Vulkan 规范：先按 `c_s` 混合，再把最终 RGB 转成非线性 sRGB 写入附件） |
| 禁止           | shader 内 `pow(x, 2.2)` / `pow(x, 1/2.2)` / 手写 sRGB 曲线；对 R8 字形图集声明 sRGB 格式；sRGB 纹理再手动解码                                                                                         |
| 双重转换的防线 | `apply_gamma(linear)` 在 v1 **恒等**（见 2.4）；唯一的编码点是目标格式，唯一的解码点是 sRGB 纹理采样                                                                                                  |

**blend state（三平台逐字相同）**

```text
src_color = ONE,  dst_color = ONE_MINUS_SRC_ALPHA,  color_op = ADD
src_alpha = ONE,  dst_alpha = ONE_MINUS_SRC_ALPHA,  alpha_op = ADD
write_mask = R | G | B | A
```

即 `dst' = src + dst × (1 − src.a)`。不写单独的 alpha 公式变体（与 P02 §5.6 第 3 条一致）。

### 2.4 函数集（三套源码同名同语义）

| 函数                                        | 语义（逐字实现）                                                                                                                |
|---------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------|
| `sd_rounded_rect(p, half_size, radii)`      | `r = 按象限取 radii`；`q = abs(p) - half_size + r`；`min(max(q.x,q.y),0) + length(max(q,0)) - r`                                |
| `aa_coverage(d)`                            | `w = max(fwidth(d), 1e-5); saturate(0.5 - d / w)`（1 px 解析 AA；`d` 单位 = 物理像素）                                          |
| `analytic_shadow(d, blur)`                  | `σ = max(blur/3, 0.35)`；`0.5 × (1 − erf(d / (σ√2)))`；`erf` 用 A&S 7.1.26                                                      |
| `gradient_eval(kind, p, half_size, c0, c1)` | 线性：`(p.x+hx)/(2hx)`；垂直：`(p.y+hy)/(2hy)`；径向：`length(p)/min(hx,hy)`；`saturate(t)` 后 `mix`（premultiplied 空间）      |
| `clip_mask(world_px, clip_index)`           | `clip_index == 0 ⇒ 1`；否则沿 `parent` 链 ≤8 层，`m *= aa_coverage(sd_rounded_rect(...))`                                       |
| `apply_gamma(c)`                            | **v1 恒等**：`return c;`。编码归目标格式；函数保留只为满足主设计 §2.11 的函数集契约（并在源码注释里写清「此处转换会双重编码」） |

**`fwidth` 的使用纪律**：只在**实例级一致**的分支里调用（`kind` / `border_width` / `clip_index` 都是逐实例常量）。
禁止在逐片元发散的控制流里调用（GLSL/HLSL/MSL 的导数在非一致控制流中都是未定义）。因此 `clip_mask` **不做**
逐片元 AABB 早退（早退会让同 quad 内部分片元跳过 `fwidth`）；被完全裁掉的实例由 **VS 的 `clip_rect` 夹紧**退化成零面积四边形解决。

### 2.5 顶点外扩、真实矩形与 clip 夹紧

```text
lo = bounds.xy - params0.w
hi = bounds.xy + bounds.zw + params0.w
if (clip_index != 0) {                       // 表项 0 的 aabb 不是可用矩形（见 2.6）
    lo = max(lo, clip_rect.xy - 2.0)
    hi = min(hi, clip_rect.xy + max(clip_rect.zw, 0) + 2.0)
    hi = max(hi, lo)                         // 完全裁掉 ⇒ 零面积 ⇒ 不产生像素
}
world_px = lerp(lo, hi, corner01)            // corner01 ∈ {0,1}²
```

* 夹紧是**纯优化**（避免巨大阴影/圆角容器外的整块光栅化），不改变像素：`clip_rect` 是链交集 AABB 的超集，2 px 是
  `fwidth ≤ √2` 时 1 px AA 半径的保守余量；越界像素本来就会被 `clip_mask` 判为 0，或被 scissor 裁掉。
* 片元用**真实** `bounds`（不减不扩）求 `local_px`：`local = world_px − (bounds.xy + bounds.zw/2)`；扩出去的边缘覆盖率自然为
  0。
* `params0.w` 取值表（P01 §7.2，编码器唯一来源）：Quad **1.0**、Shadow **blur+1.0**、图集（Glyph/Image）**0.0**。

### 2.6 clip 链的 GPU 侧镜像（P09 上传，P07 定布局）

CPU 侧 `ClipChain`（P06 `paint/instance.rs`）含 `parent/rect/corner_radius/aabb`；GPU 侧**去掉 `aabb`**（已在
`GpuPrimitive.clip_rect` 冗余）并把 `parent` 打进 `uint4`：

| offset | 字节 | 字段            | 类型     | 语义                                                                                                        |
|--------|------|-----------------|----------|-------------------------------------------------------------------------------------------------------------|
| **0**  | 16   | `rect`          | `float4` | 本层 clip `x, y, w, h`（物理像素）                                                                          |
| 16     | 16   | `corner_radius` | `float4` | `tl, tr, br, bl`（物理像素）                                                                                |
| 32     | 16   | `meta`          | `uint4`  | `.x = parent`（0 = 无裁剪），`.yzw` 保留恒 0                                                                |
| **48** | —    | 步进            | —        | 16 B 对齐；≤ 8 层；表项 **0 恒为哨兵**（shader 在 `clip_index == 0` 时直接返回 1，**不得**读表项 0 做数学） |

**哨兵陷阱（必须写进实现检查单）**：Rust 侧表项 0 的 `aabb = [f32::MIN, f32::MIN, f32::MAX, f32::MAX]`，按其 `(x,y,w,h)`
语义算出的右/下边界是 `MIN+MAX = 0`，**不是**无穷大矩形。任何对 `clip_rect` 的算术都必须在 `clip_index != 0` 时进行。
这一行为与 P02 §8.7「P10 以表项 0 = 直接通过特判，不做数学运算」一致，不是缺陷。

### 2.7 `RootUniform`：一个 112 B 常量块（`frame` + `draw`）

三平台各用**一个**常量槽位：D3D12 `b0`（28 个 root constant DWORD）、Metal `buffer(2)`（112 B，`setVertexBytes`/
`setFragmentBytes`）、Vulkan push constants `[0, 112)`（112 ≤ 规范下限 128 B）。布局逐字节一致：

| offset  | 字段                       | 语义                                                                                                                 |
|---------|----------------------------|----------------------------------------------------------------------------------------------------------------------|
| 0       | `frame.viewport: float4`   | `(w_px, h_px, 2/w_px, 2/h_px)`；`z/w` 预计算，避免逐顶点除法与「两个真值来源」                                       |
| 16      | `frame.params: float4`     | `(scale, 0, 0, 0)`；**只**用于 geometry 的逻辑→物理换算（DPI）。uber-quad 不读它                                     |
| 32      | `draw.basis: float4`       | `(m11, m12, m21, m22)`；`x' = m11·x + m21·y + tx`、`y' = m12·x + m22·y + ty`（与 `math::Transform::apply` 逐字一致） |
| 48      | `draw.translation: float4` | `(tx, ty, 0, 0)`                                                                                                     |
| 64      | `draw.color0: float4`      | linear premultiplied（`Ui` 的 `Path.fill`/背景；`Textured` 的调制色）                                                |
| 80      | `draw.clip_rect: float4`   | `FrameOp::Geometry.clip_rect`（P06 V-53 的 f32 位模式 `from_bits` 还原），`x,y,w,h` 物理像素                         |
| 96      | `draw.clip: uint4`         | `.x = clip_index`（0 = 无裁剪），`.yzw` 保留恒 0                                                                     |
| **112** | —                          | 16 B 对齐；`FrameUniform` = 前 32 B，`GeometryDrawUniform` = 后 80 B                                                 |

uber-quad 只读 `frame`（32 B），但同一次 draw 也会把这 112 B 一起绑定/推送；draw call 目标是每帧 ≤ 8，
112 B/draw 可忽略。**不**把 `draw` 的 `yzw`/padding 复用为 clip（见下）。

**为什么不把 clip 塞进 `translation.z/w` 的 padding**：`translation = (tx, ty, 0, 0)` 的 padding 是 P01 F-14 冻结的
「6 个有效 float + 2 padding」布局，复用 padding 会让「零值」契约失效、且与 `GeometryUniform` 的语义表冲突。
直接扩到 80 B，代价是每 draw +32 B（相对 0 B 顶点上传可忽略）。→ **F-68**

### 2.8 NDC（Y 方向只有一处）

| API           | 公式                           | NDC 范围                            |
|---------------|--------------------------------|-------------------------------------|
| D3D12 / Metal | `ndc = (x·2/w − 1, 1 − y·2/h)` | x,y ∈ [−1,1]，y 向上；z ∈ [0,1]     |
| Vulkan        | `ndc = (x·2/w − 1, y·2/h − 1)` | x,y ∈ [−1,1]，**y 向下**；z ∈ [0,1] |

`z = 0, w = 1`。三套源码里 `to_ndc` 是**唯一**出现 Y 符号差异的函数；FS 只见 `local_px`/`world_px`（物理像素、Y 向下），
完全不依赖 `SV_Position` 的像素中心约定（三平台都是 `(i+0.5, j+0.5)`，但这里不依赖它，也就不可能漂移）。

### 2.9 资源绑定表（逻辑槽位固定，机制归 P09）

| 逻辑资源                            | D3D12                                 | Metal                                      | Vulkan                                       |
|-------------------------------------|---------------------------------------|--------------------------------------------|----------------------------------------------|
| `instances`（128 B 步进）           | SRV `t0`（`StructureByteStride=128`） | `device const GpuPrimitive* [[buffer(0)]]` | `set 0 binding 0`，std430，`readonly buffer` |
| `clips`（48 B 步进）                | SRV `t1`                              | `[[buffer(1)]]`                            | `binding 1`，std430                          |
| `root`（112 B：frame 32 + draw 80） | root constants `b0`（28 DWORD）       | `[[buffer(2)]]`（112 B）                   | push constants `[0, 112)`                    |
| glyph 图集（R8）                    | SRV `t2`                              | `[[texture(0)]]`                           | `binding 4`（combined `sampler2D`）          |
| image 图集（BGRA8 sRGB）            | SRV `t3`                              | `[[texture(1)]]`                           | `binding 5`（combined `sampler2D`）          |
| sampler                             | static `s0`                           | `[[sampler(0)]]`                           | 同上                                         |

* 采样器状态：`FILTER=LINEAR`、`ADDRESS=CLAMP_TO_EDGE`、无 mip（`LOD=0`）、无比较、无各向异性。
* **uber-quad 的管线状态（P09 建 PSO 必须逐字照做）**：拓扑 **`TriangleStrip`**、**4 个顶点**、**`cull = NONE`**、
  无 index 缓冲、无深度/模板、MSAA 关闭。用 `TriangleList` 时 4 个顶点只能画出 2 个独立三角形中的一半（缺 index 缓冲补齐
  6 顶点），是本契约最容易踩错的一条 → 检查单 §7.1-1b。
* **image 图集的 SRV / 纹理视图必须声明为 sRGB 格式**（D3D12 `DXGI_FORMAT_*_UNORM_SRGB` / Metal `.bgra8Unorm_srgb` /
  Vulkan `VK_FORMAT_*_SRGB`）；glyph 图集必须保持**非** sRGB 的 `R8_UNORM`（否则覆盖率被当颜色解码，字变细）。
* **`constant` vs `device`（Metal）**：实例/链缓冲用 `device const`。`constant` 地址空间有 64 KB 上限
  （512 个实例就顶满），而 `device` 没有。
* 三套 shader 的 `common.*` 声明**全部**资源（含 quad 用不到的 `draw` 字段）。未使用的绑定/字段不产生读取；`frame` 与 `draw`
  在同一个 112 B 块里，所以 quad 的 draw 也推 112 B（P09 无裁剪分歧）。

### 2.10 图集采样约定

| 项         | 约定                                                                                                                                                                                   |
|------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| UV         | `AtlasTile::uv()` 的**边**约定（P02 §10.2）；**不加**半纹素偏移；三平台纹理原点都是左上、v 向下，**不做 v 翻转**                                                                       |
| glyph 图集 | `R8_UNORM`（线性覆盖度，**不是** sRGB）；`out = color0 × texel.r`（4 分量同乘）                                                                                                        |
| image 图集 | `BGRA8` **sRGB**（硬件解码到 linear）；纹素是 **linear-premultiplied 后 sRGB 编码**；`out = texel × color0`（两个预乘色逐分量相乘 = 预乘 modulate）                                    |
| 上传方职责 | P15 必须「linear 空间预乘 → sRGB 编码」后写图集；P09 必须把 SRV/采样器声明为 sRGB 格式。若改成直通 alpha 存储，shader 公式与本节必须同时改（一行：`texel.rgb *= texel.a`）→ **§9 D-3** |
| 过滤伪影   | 1:1 采样无重采样；放大时最外 0.5 纹素混入 1 纹素透明 padding（P02 §10.2 已接受）                                                                                                       |

### 2.11 禁止项（构建期/评审检查）

1. 位重解释：`asuint` / `asfloat` / `as_type<>` / `floatBitsToUint` / `floatBitsToInt` / `bitcast`。
2. 矩阵类型与 `mul()`：三套 API 的 row/column-major 默认值不同，本项目**只用两个 `float4` + 显式乘加**。
3. shader 内 gamma/色彩空间转换（2.3）。
4. 逐片元发散分支里的 `fwidth`（2.4）。
5. 读取保留位/保留字段、读取哨兵表项 0 的 `aabb`。
6. 在 shader 里做 OKLCH/OKLab 转换、渐变 LUT 生成（主设计 §5.8.2）。
7. 浮点 `%`、`discard`（用 `return 0`；premultiplied over 下 `src=0` 等价于不写）。

## 3. 三套着色器源码

### 3.1 入口 / 产物表

| 源文件                                                       | 入口                | 产物（D3D12）                             | 产物（Vulkan）                           | 产物（Metal）                          |
|--------------------------------------------------------------|---------------------|-------------------------------------------|------------------------------------------|----------------------------------------|
| `quad.hlsl` / `quad.metal` / `quad.vert.glsl`                | `vs_main`           | `shaders/quad.vs.dxil`                    | `shaders/quad.vs.spv`                    | `shaders/quad.metallib`（`vs_main`）   |
| 同上                                                         | `ps_main`           | `shaders/quad.ps.dxil`                    | `shaders/quad.ps.spv`                    | 同上（`ps_main`）                      |
| `geometry.hlsl` / `geometry.metal` / `geometry.ui.vert.glsl` | `vs_ui`             | `shaders/geometry.vs_ui.dxil`             | `shaders/geometry.vs_ui.spv`             | `shaders/geometry.metallib`（`vs_ui`） |
| 同上 / `geometry.textured.vert.glsl`                         | `vs_textured`       | `shaders/geometry.vs_textured.dxil`       | `shaders/geometry.vs_textured.spv`       | 同上                                   |
| 同上 / `geometry.ui.frag.glsl`                               | `ps_ui`             | `shaders/geometry.ps_ui.dxil`             | `shaders/geometry.ps_ui.spv`             | 同上                                   |
| 同上 / `geometry.textured_image.frag.glsl`                   | `ps_textured_image` | `shaders/geometry.ps_textured_image.dxil` | `shaders/geometry.ps_textured_image.spv` | 同上                                   |
| 同上 / `geometry.textured_glyph.frag.glsl`                   | `ps_textured_glyph` | `shaders/geometry.ps_textured_glyph.dxil` | `shaders/geometry.ps_textured_glyph.spv` | 同上                                   |

**7 个 entry**。`OUT_DIR/shaders/` 是唯一产物目录（P09 的 `include_bytes!(concat!(env!("OUT_DIR"), "/shaders/…"))`）。

### 3.2 `common.hlsl`（全文，**已用 dxc 编译验证**）

```hlsl
// common.hlsl -- uber-quad + geometry 共享的函数与数据布局
// 规则：uniform / storage 结构体只用 16 B 成员（float4 / uint4）。

// <kytrix-constants>
#define KYTRIX_KIND_QUAD 0
#define KYTRIX_KIND_GLYPH 1
#define KYTRIX_KIND_IMAGE 2
#define KYTRIX_KIND_SHADOW 3

#define KYTRIX_FLAG_OPAQUE 1

#define KYTRIX_TEXTURE_GLYPH 0
#define KYTRIX_TEXTURE_IMAGE 1

#define KYTRIX_GRADIENT_NONE 0
#define KYTRIX_GRADIENT_HORIZONTAL 1
#define KYTRIX_GRADIENT_VERTICAL 2
#define KYTRIX_GRADIENT_RADIAL 3

#define KYTRIX_MAX_CLIP_DEPTH 8
#define KYTRIX_CLIP_VERTEX_SLACK 2.0
#define KYTRIX_SHADOW_SIGMA_DIVISOR 3.0
#define KYTRIX_SHADOW_SIGMA_MIN 0.35
// </kytrix-constants>

struct FrameUniform {
    float4 viewport;   // (w_px, h_px, 2 / w_px, 2 / h_px)
    float4 params;     // (scale, 0, 0, 0)
};

struct ClipEntry {
    float4 rect;           // (x, y, w, h) 物理像素
    float4 corner_radius;  // (tl, tr, br, bl) 物理像素
    uint4  meta;           // .x = parent（0 = 无裁剪）；.yzw 保留恒 0
};

struct GeometryDrawUniform {
    float4 basis;        // 0  (m11, m12, m21, m22)
    float4 translation;  // 16 (tx, ty, 0, 0)
    float4 color0;       // 32 premultiplied linear RGBA
    float4 clip_rect;    // 48 (x, y, w, h) 物理像素
    uint4  clip;         // 64 .x = clip_index（0 = 无裁剪）；.yzw 保留恒 0
};

// 单个 112 B 常量块：frame @ 0，draw @ 32
cbuffer Root : register(b0) { FrameUniform frame; GeometryDrawUniform draw; };

struct GpuPrimitive {
    float4 bounds;         // 0
    float4 corner_radius;  // 16
    float4 color0;         // 32
    float4 color1;         // 48
    float4 params0;        // 64
    float4 params1;        // 80
    float4 uv;             // 96
    float4 clip_rect;      // 112
};

StructuredBuffer<GpuPrimitive> instances : register(t0);
StructuredBuffer<ClipEntry> clips : register(t1);

Texture2D<float>  glyph_atlas : register(t2);
Texture2D<float4> image_atlas : register(t3);
SamplerState      atlas_sampler : register(s0);

// ---- 坐标 ----------------------------------------------------------------

// 物理像素（左上原点、Y 向下）-> D3D12 / Metal 的 NDC（Y 向上）。
float2 to_ndc(float2 world_px) {
    return float2(world_px.x * frame.viewport.z - 1.0,
                  1.0 - world_px.y * frame.viewport.w);
}

// ---- SDF 与 AA -----------------------------------------------------------

// 1 px 解析 AA：d 为像素单位的带符号距离。只在「实例级一致」的分支内调用。
float aa_coverage(float d) {
    float w = max(fwidth(d), 1e-5);
    return saturate(0.5 - d / w);
}

float sd_rounded_rect(float2 p, float2 half_size, float4 radii) {
    float r = (p.x > 0.0) ? ((p.y > 0.0) ? radii.z : radii.y)
                          : ((p.y > 0.0) ? radii.w : radii.x);
    float2 q = abs(p) - half_size + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - r;
}

// ---- 解析阴影 ------------------------------------------------------------

// Abramowitz-Stegun 7.1.26（|误差| <= 1.5e-7）。
float erf_approx(float x) {
    float ax = abs(x);
    float t = 1.0 / (1.0 + 0.3275911 * ax);
    float poly = t * (0.254829592 + t * (-0.284496736 + t * (1.421413741 +
                 t * (-1.453152027 + t * 1.061405429))));
    float e = 1.0 - poly * exp(-ax * ax);
    return (x < 0.0) ? -e : e;
}

// sigma = blur / 3：3 sigma = blur 与 params0.w = blur + 1 的外扩一致。
float analytic_shadow(float d, float blur) {
    float sigma = max(blur / KYTRIX_SHADOW_SIGMA_DIVISOR, KYTRIX_SHADOW_SIGMA_MIN);
    return 0.5 * (1.0 - erf_approx(d / (sigma * 1.41421356)));
}

// ---- 渐变 ----------------------------------------------------------------

float4 gradient_eval(uint kind, float2 p, float2 half_size, float4 c0, float4 c1) {
    float t;
    if (kind == KYTRIX_GRADIENT_HORIZONTAL) {
        t = (p.x + half_size.x) / max(2.0 * half_size.x, 1e-6);
    } else if (kind == KYTRIX_GRADIENT_VERTICAL) {
        t = (p.y + half_size.y) / max(2.0 * half_size.y, 1e-6);
    } else if (kind == KYTRIX_GRADIENT_RADIAL) {
        t = length(p) / max(min(half_size.x, half_size.y), 1e-6);
    } else {
        return c0;
    }
    return lerp(c0, c1, saturate(t));
}

// ---- clip ----------------------------------------------------------------

// clip_index == 0 = 无裁剪（表项 0 的 aabb 不是可用矩形，禁止对它做数学运算）。
float clip_mask(float2 world_px, uint clip_index) {
    if (clip_index == 0u) {
        return 1.0;
    }
    float m = 1.0;
    uint idx = clip_index;
    for (uint i = 0u; i < KYTRIX_MAX_CLIP_DEPTH && idx != 0u; ++i) {
        ClipEntry e = clips[idx];
        float2 half_size = max(e.rect.zw, 0.0) * 0.5;
        float2 center = e.rect.xy + half_size;
        m *= aa_coverage(sd_rounded_rect(world_px - center, half_size, e.corner_radius));
        idx = e.meta.x;
    }
    return m;
}

// ---- 输出 ----------------------------------------------------------------

// v1：渲染目标是 sRGB 格式，linear -> sRGB 由目标格式在写回时完成一次。
float4 apply_gamma(float4 premultiplied_linear) {
    return premultiplied_linear;
}
```

### 3.3 `quad.hlsl`（全文，**已用 dxc 编译验证**）

```hlsl
// quad.hlsl -- uber-quad（4 顶点 unit quad、实例化；无顶点缓冲）
#include "common.hlsl"

struct VsOut {
    float4 position : SV_Position;
    float2 local_px : TEXCOORD0;
    float2 uv : TEXCOORD1;
    nointerpolation uint instance : TEXCOORD2;
};

VsOut vs_main(uint vertex_id : SV_VertexID, uint instance_id : SV_InstanceID) {
    GpuPrimitive p = instances[instance_id];

    float2 corner = float2((float)(vertex_id & 1u), (float)(vertex_id >> 1));
    float2 origin = p.bounds.xy;
    float2 size = max(p.bounds.zw, 0.0);
    float pad = p.params0.w;

    float2 lo = origin - pad;
    float2 hi = origin + size + pad;

    if ((uint)p.params1.x != 0u) {
        lo = max(lo, p.clip_rect.xy - KYTRIX_CLIP_VERTEX_SLACK);
        hi = min(hi, p.clip_rect.xy + max(p.clip_rect.zw, 0.0) + KYTRIX_CLIP_VERTEX_SLACK);
        hi = max(hi, lo);
    }

    float2 world_px = lerp(lo, hi, corner);
    float2 center = origin + size * 0.5;

    VsOut o;
    o.position = float4(to_ndc(world_px), 0.0, 1.0);
    o.local_px = world_px - center;
    o.uv = lerp(p.uv.xy, p.uv.zw, corner);
    o.instance = instance_id;
    return o;
}

float4 ps_main(VsOut input) : SV_Target {
    GpuPrimitive p = instances[input.instance];
    uint kind = (uint)p.params1.z;

    float2 half_size = max(p.bounds.zw, 0.0) * 0.5;
    float2 world_px = input.local_px + p.bounds.xy + half_size;

    float4 color;
    if (kind == KYTRIX_KIND_GLYPH) {
        float coverage = glyph_atlas.Sample(atlas_sampler, input.uv).r;
        color = p.color0 * coverage;
    } else if (kind == KYTRIX_KIND_IMAGE) {
        color = image_atlas.Sample(atlas_sampler, input.uv) * p.color0;
    } else {
        float d_outer = sd_rounded_rect(input.local_px, half_size, p.corner_radius);
        if (kind == KYTRIX_KIND_SHADOW) {
            color = p.color0 * analytic_shadow(d_outer, p.params0.y);
        } else {
            float coverage = aa_coverage(d_outer);
            uint gradient = (uint)p.params0.z;
            float4 fill = (gradient == KYTRIX_GRADIENT_NONE)
                              ? p.color0
                              : gradient_eval(gradient, input.local_px, half_size,
                                              p.color0, p.color1);
            color = fill * coverage;
            float border = p.params0.x;
            if (border > 0.0) {
                float4 inner_radii = max(p.corner_radius - border, 0.0);
                float2 inner_half = max(half_size - border, 0.0);
                float inner = aa_coverage(sd_rounded_rect(input.local_px, inner_half, inner_radii));
                float ring = max(coverage - inner, 0.0);
                color += (p.color1 - fill * p.color1.a) * ring;
            }
        }
    }

    color *= clip_mask(world_px, (uint)p.params1.x);
    return apply_gamma(color);
}
```

### 3.4 `geometry.hlsl`（全文，**已用 dxc 编译验证**）

```hlsl
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
```

### 3.5 Metal（`common.metal` / `quad.metal` / `geometry.metal`）

**MSL 的结构差异（唯一来源：MSL 没有全局资源绑定）**：共享函数**显式接收**资源参数（`device const ClipEntry*`、
`constant FrameUniform&`）；入口用属性声明绑定。函数**体**与 HLSL 逐行相同（仅 `lerp` → `mix`）。

```metal
// common.metal
#include <metal_stdlib>
using namespace metal;

// <kytrix-constants> 与 HLSL 同一份 #define（值必须一致）

struct FrameUniform { float4 viewport; float4 params; };
struct ClipEntry { float4 rect; float4 corner_radius; uint4 meta; };
struct GpuPrimitive { float4 bounds; float4 corner_radius; float4 color0; float4 color1;
                      float4 params0; float4 params1; float4 uv; float4 clip_rect; };
struct GeometryDrawUniform { float4 basis; float4 translation; float4 color0;
                             float4 clip_rect; uint4 clip; };
struct RootUniform { FrameUniform frame; GeometryDrawUniform draw; };   // 112 B

inline float2 to_ndc(float2 world_px, constant FrameUniform& frame);
inline float  aa_coverage(float d);
inline float  sd_rounded_rect(float2 p, float2 half_size, float4 radii);
inline float  erf_approx(float x);
inline float  analytic_shadow(float d, float blur);
inline float4 gradient_eval(uint kind, float2 p, float2 half_size, float4 c0, float4 c1);
inline float  clip_mask(float2 world_px, uint clip_index, device const ClipEntry* clips);
inline float4 apply_gamma(float4 premultiplied_linear);

// quad.metal
struct QuadVsOut {
    float4 position [[position]];
    float2 local_px;
    float2 uv;
    uint   instance [[flat]];
};

vertex QuadVsOut vs_main(uint vid [[vertex_id]], uint iid [[instance_id]],
                         device const GpuPrimitive* instances [[buffer(0)]],
                         constant RootUniform& root [[buffer(2)]]);

fragment float4 ps_main(QuadVsOut in [[stage_in]],
                        device const GpuPrimitive* instances [[buffer(0)]],
                        device const ClipEntry* clips [[buffer(1)]],
                        texture2d<float> glyph_atlas [[texture(0)]],
                        texture2d<float> image_atlas [[texture(1)]],
                        sampler atlas_sampler [[sampler(0)]]);

// geometry.metal
struct GeomVsOut { float4 position [[position]]; float2 world_px; float2 uv; };
struct GeomVsInUi { float2 position [[attribute(0)]]; };
struct GeomVsInTextured { float2 position [[attribute(0)]]; float2 uv [[attribute(1)]]; };

vertex GeomVsOut vs_ui(GeomVsInUi in [[stage_in]],
                       constant RootUniform& root [[buffer(2)]]);
vertex GeomVsOut vs_textured(GeomVsInTextured in [[stage_in]],
                             constant RootUniform& root [[buffer(2)]]);
fragment float4 ps_ui(GeomVsOut in [[stage_in]], device const ClipEntry* clips [[buffer(1)]],
                      constant RootUniform& root [[buffer(2)]]);
fragment float4 ps_textured_image(GeomVsOut in [[stage_in]],
                                  device const ClipEntry* clips [[buffer(1)]],
                                  constant RootUniform& root [[buffer(2)]],
                                  texture2d<float> image_atlas [[texture(1)]],
                                  sampler atlas_sampler [[sampler(0)]]);
fragment float4 ps_textured_glyph(GeomVsOut in [[stage_in]],
                                  device const ClipEntry* clips [[buffer(1)]],
                                  constant RootUniform& root [[buffer(2)]],
                                  texture2d<float> glyph_atlas [[texture(0)]],
                                  sampler atlas_sampler [[sampler(0)]]);
```

顶点描述符（P09 建 `MTLVertexDescriptor`）：`attribute(0) = float2 offset 0`，`attribute(1) = float2 offset 8`，
`buffer(0)` 的 `stride = 8`（`Ui`）/ `16`（`Textured`），`stepFunction = .perVertex`。

### 3.6 Vulkan GLSL（`common.glsl` + **7** 个 stage 文件）

```glsl
// common.glsl（无 #version；由 stage 文件 #include）
struct FrameUniform { vec4 viewport; vec4 params; };
struct ClipEntry { vec4 rect; vec4 corner_radius; uvec4 meta; };
struct GpuPrimitive { vec4 bounds; vec4 corner_radius; vec4 color0; vec4 color1;
                      vec4 params0; vec4 params1; vec4 uv; vec4 clip_rect; };
struct GeometryDrawUniform { vec4 basis; vec4 translation; vec4 color0;
                             vec4 clip_rect; uvec4 clip; };
// RootUniform（112 B）= frame @0 + draw @32；push block 直接展开这两个成员
layout(std430, set = 0, binding = 0) readonly buffer Instances { GpuPrimitive instances[]; };
layout(std430, set = 0, binding = 1) readonly buffer Clips { ClipEntry clips[]; };
layout(push_constant) uniform RootPush { FrameUniform frame; GeometryDrawUniform draw; } root;
layout(set = 0, binding = 4) uniform sampler2D glyph_atlas;
layout(set = 0, binding = 5) uniform sampler2D image_atlas;

// 函数：to_ndc（Vulkan 版为 (x·sx−1, y·sy−1)）/ aa_coverage / sd_rounded_rect / erf_approx /
//       analytic_shadow / gradient_eval / clip_mask / apply_gamma —— 体与 HLSL 相同，
//       替换：lerp→mix、saturate→clamp(x,0,1)、float2/3/4→vec2/3/4、uint4→uvec4、
//       Texture2D<T>.Sample(s,uv)→texture(sampler, uv)、frame/draw → root.frame/root.draw。
// 注意：Vulkan 的 NDC Y 向下 ⇒ to_ndc 是 (x*sx-1, y*sy-1)，与 HLSL/MSL 不同（唯一差异点）。
```

```glsl
// quad.vert.glsl
#version 450
#extension GL_GOOGLE_include_directive : require
#include "common.glsl"
layout(location = 0) out vec2 v_local_px;
layout(location = 1) out vec2 v_uv;
layout(location = 2) flat out uint v_instance;
void main() {
    GpuPrimitive p = instances[gl_InstanceIndex];
    vec2 corner = vec2(float(gl_VertexIndex & 1), float(gl_VertexIndex >> 1));
    /* 与 quad.hlsl 的 vs_main 逐行相同（含 clip 夹紧） */
    gl_Position = vec4(to_ndc(world_px), 0.0, 1.0);
    v_local_px = world_px - center;
    v_uv = mix(p.uv.xy, p.uv.zw, corner);
    v_instance = gl_InstanceIndex;
}

// quad.frag.glsl
#version 450
#extension GL_GOOGLE_include_directive : require
#include "common.glsl"
layout(location = 0) in vec2 v_local_px;
layout(location = 1) in vec2 v_uv;
layout(location = 2) flat in uint v_instance;
layout(location = 0) out vec4 out_color;
void main() { /* 与 quad.hlsl 的 ps_main 逐行相同，instances[v_instance] */ }

// geometry.ui.vert.glsl / geometry.textured.vert.glsl / geometry.ui.frag.glsl /
// geometry.textured_image.frag.glsl / geometry.textured_glyph.frag.glsl
// 各自是独立编译单元（首行 #version 450），入口都是 void main()：
//   - 顶点输入 layout(location = 0) in vec2 position;（Textured 另有 location 1 in vec2 uv）
//   - 函数体与 geometry.hlsl 的对应入口逐行相同（vs_ui → geometry.ui.vert.glsl …）
//   - `Ui` 管线的输入装配不启用 location 1（stride 8）
```

> `FrameUniform` / `GeometryDrawUniform` 在 `common.glsl` 里各只声明一次；`quad.*` 不使用 `root.draw`，只在
> push constant 块里占位（112 B 一起推送）。

### 3.7 逐项对照表（review 时的唯一 checklist）

| 项                  | HLSL                                                | MSL                                   | GLSL                                                                                  |
|---------------------|-----------------------------------------------------|---------------------------------------|---------------------------------------------------------------------------------------|
| 顶点索引 / 实例索引 | `SV_VertexID` / `SV_InstanceID`                     | `[[vertex_id]]` / `[[instance_id]]`   | `gl_VertexIndex` / `gl_InstanceIndex`                                                 |
| 整数插值            | `nointerpolation uint`                              | `uint … [[flat]]`                     | `flat out/in uint`                                                                    |
| NDC Y               | `1 − y·sy`                                          | `1 − y·sy`                            | `y·sy − 1`                                                                            |
| 常量块              | `cbuffer Root : register(b0)`                       | `constant RootUniform& [[buffer(2)]]` | `layout(push_constant) … root`                                                        |
| 实例/链             | `StructuredBuffer<T> : register(tN)`                | `device const T* [[buffer(N)]]`       | `std430 readonly buffer`                                                              |
| 图集                | `Texture2D<float>` / `<float4>` + `SamplerState s0` | `texture2d<float>` + `sampler`        | `sampler2D`                                                                           |
| 采样                | `.Sample(s, uv)`                                    | `.sample(s, uv)`                      | `texture(sampler, uv)`                                                                |
| `lerp` / `saturate` | `lerp` / `saturate`                                 | `mix` / `saturate`                    | `lerp`/`saturate` 由 `common.glsl` 提供同名包装（内部 `mix`/`clamp`），函数体逐行一致 |
| 整型常量            | `#define X 1`                                       | `#define X 1u`                        | `#define X 1u`（GLSL 无 int→uint 隐式转换）                                           |
| 入口                | `-E <name>`                                         | `[[vertex]]`/`[[fragment]]` 函数名    | `void main()`（每文件一 stage）                                                       |

### 3.8 人工审查清单（MSL/GLSL 无法本机编译，逐条打勾）

1. `common.*` 三份的 `<kytrix-constants>` 数值与 Rust 常量一致（`build.rs` 自动断言，见 §6）。
2. `GpuPrimitive` / `ClipEntry` / `FrameUniform` / `GeometryDrawUniform` 的字段顺序与 §2 一致（自动断言）。
3. 全文搜索 `lerp(`：HLSL 允许；MSL/GLSL 必须为 `mix(`。
4. 全文搜索 `saturate(`：GLSL 必须无（用 `clamp(x, 0.0, 1.0)`）。
5. 每个 stage 文件的第一行是 `#version 450`（`common.glsl` 除外），其后紧跟
   `#extension GL_GOOGLE_include_directive : require`
   与 `#include "common.glsl"`。
6. `quad.vert.glsl` 的 `to_ndc` 调用**不**经过 HLSL 版（必须用文件内/`common.glsl` 里的 Vulkan 版 Y 公式）。
7. 每个 `flat` 变量在 VS/FS 两侧的 location 相同（0 local / 1 uv / 2 instance）。
8. 采样：glyph 用 `.r` 标量乘 4 分量；image 用 `texel * color0`（**不**再乘 alpha，§2.10）。
9. 无 `discard`、无矩阵、无 `as_type`/`asuint`/`floatBitsToUint`。
10. Metal：`device const`（不是 `constant`）用于实例/链缓冲；`[[buffer(0..2)]]`、`[[texture(0/1)]]`、`[[sampler(0)]]` 与 §2.9
    一致。

---

## 4. geometry 管线（静态 VBO 通道）

### 4.1 顶点布局（P02 §9.4 冻结）

| pipeline             | stride   | 布局                                       | 坐标语义                      |
|----------------------|----------|--------------------------------------------|-------------------------------|
| `Ui`                 | **8 B**  | `position: float2`（offset 0）             | **逻辑像素**（局部空间）      |
| `Textured { atlas }` | **16 B** | `position: float2`（0）+ `uv: float2`（8） | 同上；`uv` 归一化到对应图集页 |

* 索引：`U16` / `U32`（`IndexType::size()`）；拓扑：`TriangleList` / `TriangleStrip`（`GeometryDesc` 决定，P09 建 PSO）。
* 无逐顶点颜色（P02 §9.4 的取舍：一张网格一个 draw、一个常量色）。
* PSO 的 rasterizer：`cull = NONE`（`TriangleStrip` 的绕序由调用方决定，不裁剪）、`fill = SOLID`、无深度、
  无 stencil、MSAA 关闭（v1）、scissor 开（damage）。

### 4.2 变换链（逐字）

```text
local  = (m11·x + m21·y + tx, m12·x + m22·y + ty)     // 逻辑像素，per-draw（F-14 的 basis/translation）
world  = local × frame.params.x                        // 物理像素（DPI scale）
ndc    = to_ndc(world)                                 // 唯一 Y 方向差异点
```

`Path` 的 `GeometryDraw.transform = IDENTITY`（F-62）；应用注册的 `Geometry` 用 per-draw 的 pan/zoom。 **per-draw uniform
只做逻辑空间 pan/zoom，`scale` 只出现在帧 uniform**（P02 §9.5 的分工，禁止两处都乘）。

### 4.3 着色与裁剪

* `Ui`：`out = draw.color0 × clip_mask(world_px, draw.clip.x)`；**无 AA**（F-26：v1 的路径是硬边三角形）。
* `Textured{Image}`：`out = texel × draw.color0 × clip_mask(...)`（texel 预乘，§2.10）。
* `Textured{Glyph}`：`out = draw.color0 × texel.r × clip_mask(...)`。
* clip：`FrameOp::Geometry` 自带 `clip_index` + `clip_rect`（P06 V-53），P09 每 draw 写进 `draw.clip.x`/`draw.clip_rect`，
  与 uber-quad 共用同一个 `clips` 缓冲（同一帧、同一张表）。
* `draw.clip_rect` 在 geometry 里**不**用于 VS 夹紧（不做逐图元 scissor 夹紧）；由 scissor + `clip_mask` 负责。

### 4.4 与 uber-quad 的共享项

| 共享           | 内容                                                         |
|----------------|--------------------------------------------------------------|
| 常量块         | 同一个 `RootUniform`（112 B），`frame` 部分完全相同          |
| clip 表        | 同一个 `ClipEntry` 缓冲（48 B 步进）与同一套 `clip_mask`     |
| 采样器 / 图集  | 同一个 `atlas_sampler`、同一对图集绑定                       |
| 输出           | 同一个 sRGB 目标 + 同一个 blend state + 同一个 `apply_gamma` |
| damage/scissor | 两者都由 P09 的 scissor 收口（本阶段不重算区域，F-25）       |

---

## 5. `build.rs` 设计（零依赖，std only）

### 5.1 结构（单文件，实测 648 行）

```text
fn main()
  ├ emit_rerun()                 // 14 个源文件 + contract.md + paint/{instance,scene}.rs + build.rs + 2 个 env
  ├ validate()                   // §6：**永远执行**（不受 skip 影响）
  └ build()                      // 编译或写占位（§5.4）
        ├ build_windows()        // dxc × 7 entry
        ├ build_macos()          // xcrun metal -c + metallib × 2
        ├ build_linux()          // glslangValidator × 7 stage
        └ placeholders()         // 按 target 写 7/2/7 个占位产物
解析器：read / strip_comments / struct_body / c_struct / rust_struct / defines / rust_consts /
        number / const_value / fail；工具：find_dxc / newest_dxc / require_tool / report /
        check_magic / check_non_empty
```

* 只 `use std::{env, fs, path::{Path, PathBuf}, process::Command}`；无 `[build-dependencies]`（**零依赖**）。
* 不用 `sha2`/哈希缓存：Cargo 的 fingerprint + `rerun-if-changed` 已经是缓存，重复哈希是第二套真值来源。
* `panic!` 就是「构建失败」：消息里带**文件、期望、实际、以及复现命令**。

### 5.2 dxc 探测（Windows）

按序取第一个存在的可执行文件，**全部失败才 panic**：

1. `KYTRIX_DXC`（显式文件路径；设置但不存在 → 直接 panic，不静默回退）。
2. `%WindowsSdkDir%\bin\<ver>\x64\dxc.exe`（VS 开发者命令行环境；`<ver>` 取 `bin\10.0.*` 中字典序最大者，实测唯一）。
3. `%ProgramFiles(x86)%\Windows Kits\10\bin\<ver>\x64\dxc.exe`（实测存在：`10.0.26100.0`）。
4. `dxc.exe`（PATH；实测**不在** PATH，仅作最后兜底）。

> 不在 `build.rs` 里读注册表（`HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots` 的 `KitsRoot10` 与 3 等价，
> 但需要 `winreg`/`windows` 依赖 → 违反零依赖）。arm64 host 用 `arm64` 子目录，回退 `x64`。

失败消息（逐字）：

```text
ui_core: 找不到 dxc.exe。
  已尝试：%WindowsSdkDir%\bin\10.0.*\x64\dxc.exe、%ProgramFiles(x86)%\Windows Kits\10\bin\10.0.*\x64\dxc.exe、PATH
  安装 Windows SDK 10.0.26100+，或设置 KYTRIX_DXC=<dxc.exe 的绝对路径>；
  仅为跨 target 类型检查时可设 ui_core_SKIP_SHADERS=1（产物是占位符，不可运行）。
```

### 5.3 编译命令（实测）

**Windows（dxc 1.9.2602.17，SDK 10.0.26100.0）**

```powershell
& dxc.exe -T vs_6_2 -E vs_main -HV 2018 -WX -nologo -Fo "<OUT_DIR>\shaders\quad.vs.dxil" "<src>\quad.hlsl"
& dxc.exe -T ps_6_2 -E ps_main -HV 2018 -WX -nologo -Fo "<OUT_DIR>\shaders\quad.ps.dxil" "<src>\quad.hlsl"
# geometry 5 个 entry：-T vs_6_2 -E vs_ui / vs_textured；-T ps_6_2 -E ps_ui / ps_textured_image / ps_textured_glyph
```

| 参数                   | 为什么                                                                                                                                    |
|------------------------|-------------------------------------------------------------------------------------------------------------------------------------------|
| `-T vs_6_2` / `ps_6_2` | SM 6.2 是 D8 现代基线；VS/PS 分开编（dxc 一次一个 entry）                                                                                 |
| `-E <name>`            | 一文件多入口                                                                                                                              |
| `-HV 2018`             | 钉死 HLSL 语言版本；**实测不是必需**（省略也能编过），但避免未来 dxc 改默认值导致语义漂移                                                 |
| `-WX`                  | 警告即错误。实测 `-Wconversion`（隐式截断）会被升级为错误；这是三套源码里最容易出现的错误类别                                             |
| `-nologo`              | 只保留诊断输出                                                                                                                            |
| `-Fo <path>`           | 产物名固定（§3.1）。**不传 `-I`**：`#include "common.hlsl"` 与源文件同目录，dxc 按源文件目录解析（实测：从任意 cwd + 绝对源路径也能编过） |
| 不做                   | `-Gis`（IEEE 严格）实测可编译，但不带来跨 API 一致性收益，且抑制优化 → 不用；`-Zi`/`-Qembed_debug` 不进产物                               |

* 退出码：失败时 `dxc` 返回 `-2147467259`（`0x80004005`），必须用 `status.success()` 判断，**不能**比较 `code() == 0` 之外的值。
* 失败输出：`panic!("dxc 失败（{entry}）：\n  命令: {cmdline}\n  stderr:\n{stderr}\n  stdout:\n{stdout}")`。
* 产物自检：编译成功后读前 4 字节，必须等于 `DXBC`（实测 7 个产物全部 `DXBC` + 含 `DXIL` 段）；否则 `panic!`。
  这条同时给 P09 的 `ShaderLoad` 提供了「magic 校验」契约。

**macOS**

```text
xcrun -sdk macosx metal -c <src>/quad.metal -o <OUT_DIR>/shaders/quad.air
xcrun -sdk macosx metallib <OUT_DIR>/shaders/quad.air -o <OUT_DIR>/shaders/quad.metallib
（geometry 同；产物 2 个 metallib，含 7 个函数）
```

**Linux**

```text
glslangValidator -V --target-env vulkan1.2 -I <src> -o <OUT>/x.spv <src>/<stage file>
```

7 次调用（一文件一 stage，入口恒为 `main`）：

| 源                                  | 产物                             |
|-------------------------------------|----------------------------------|
| `quad.vert.glsl`                    | `quad.vs.spv`                    |
| `quad.frag.glsl`                    | `quad.ps.spv`                    |
| `geometry.ui.vert.glsl`             | `geometry.vs_ui.spv`             |
| `geometry.textured.vert.glsl`       | `geometry.vs_textured.spv`       |
| `geometry.ui.frag.glsl`             | `geometry.ps_ui.spv`             |
| `geometry.textured_image.frag.glsl` | `geometry.ps_textured_image.spv` |
| `geometry.textured_glyph.frag.glsl` | `geometry.ps_textured_glyph.spv` |

> **为什么 GLSL 是 7 个 stage 文件**：GLSL 的入口恒为 `main`，一个编译单元只能有一个 stage，`#version` 必须在首行，
> 无法像 HLSL/MSL 那样「一文件多入口」。共享函数只出现在 `common.glsl` 里一次。→ **S-68 / D-1**。
> 每个 stage 文件都 `#include "common.glsl"`（`common.glsl` 无 `#version`）。

### 5.4 跳过策略与占位产物

| 条件                                                                                               | 行为                                                                                                 |
|----------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------|
| `ui_core_SKIP_SHADERS` = `1` / `true`（大小写不敏感；**其它值视为未设置**并 `cargo:warning` 说明） | **不**调编译器，为**每个**预期产物写占位文件（内容 `KYTRIX-SHADER-SKIPPED\n`），并 `cargo:warning=…` |
| `CARGO_CFG_TARGET_OS != std::env::consts::OS`（交叉 target 的 `cargo check`）                      | 同上（自动跳过，不必设环境变量）                                                                     |
| 其它                                                                                               | 正常编译                                                                                             |

* **占位产物必须存在**：P09 用 `include_bytes!(concat!(env!("OUT_DIR"), "/shaders/…"))`，缺文件会让**类型检查**失败，
  而 `ui_core_SKIP_SHADERS=1` 的用途正是让跨 target `cargo check` 通过。占位文件让 `include_bytes!` 始终可解析；
  真跑起来会在 P09 的 magic 校验处得到 `EngineError::ShaderLoad`（显式失败，不是静默降级）。
* **必须** `cargo:rerun-if-env-changed=ui_core_SKIP_SHADERS` 与 `=KYTRIX_DXC`，否则切回正常构建时 Cargo 不会重跑
  build.rs，占位文件会残留。
* `rerun-if-changed` 清单：**14 个源文件** + `contract.md` + `src/paint/instance.rs` + `build.rs`。 **不要**省略成目录级监听（会让每次改
  `src/` 都重跑 build.rs）。
* **macOS / Linux 缺少编译器**（`xcrun` / `glslangValidator`）时的行为与 dxc 同构：`panic!` + 可读消息（同样提示
  `ui_core_SKIP_SHADERS=1`）。**不**加 `KYTRIX_METAL` / `KYTRIX_GLSLANG` 覆盖：只有 Windows 需要绕开 SDK 的版本目录探测，
  另两个平台的工具链在 PATH 上是确定的。

## 6. 构建期静态校验（`build.rs`，std only、无 regex）

### 6.1 受管对象与来源

| 对象                               | 比对源                                                          | 规则                                                                                         |
|------------------------------------|-----------------------------------------------------------------|----------------------------------------------------------------------------------------------|
| `GpuPrimitive`（8 字段/`[f32;4]`） | `src/paint/instance.rs`（**参照**） vs 三份 `common.*`          | 字段名序列逐项相等、数量 == 8、类型 `[f32; 4]` ↔ `float4`/`vec4`                             |
| `ClipEntry`（3 字段）              | `common.hlsl` / `common.metal` / `common.glsl`                  | 三份字段名序列相等、数量 == 3、类型 `float4/float4/uint4(uvec4)`                             |
| `FrameUniform`（2 字段）           | 三份 `common.*`                                                 | 同上                                                                                         |
| `GeometryDrawUniform`（5 字段）    | 三份 `common.*`                                                 | 同上                                                                                         |
| 11 个常量                          | 三份 `common.*` 的 `#define KYTRIX_*` vs Rust `const NAME: f32` | 同名同值（f32 数值比较）                                                                     |
| `KYTRIX_MAX_CLIP_DEPTH`            | 三份 `common.*`                                                 | == 8 == Rust `MAX_CLIP_DEPTH`（`paint/scene.rs`）                                            |
| 禁用 token                         | 全部源文件                                                      | `asuint(`, `asfloat(`, `as_type<`, `floatBitsToInt`, `floatBitsToUint`, `bitcast` 均不得出现 |

> **`aa_padding` 不在这张表里**：它是**逐实例数据**（`params0.w`，由 P06 的编码器写入 `1.0` / `blur+1.0` / `0.0`），
> 不是 shader 常量，因此无法（也不应该）做成三套源码的常量比对。它由 P01 §7.2 + P06 §7.3 冻结，并由 L3 的
> 「AA 边缘 / 阴影 blur 表」像素 case 守护。三套源码唯一的义务是：**读 `params0.w`，不自己推 AA 宽度**。

### 6.2 解析器（实测可用，约 90 行）

```rust
// 1) 先剥离 // 行注释（用空格替换、保留换行）——实测必须做：
//    `float4 bounds;  // 112` 之后的分号会把注释粘到下一个字段上。
fn strip_comments(src: &str) -> String;
// 2) 定位 `struct <Name>`，从第一个 '{' 做花括号配平，取 body。
fn body<'a>(src: &'a str, keyword: &str, name: &str) -> &'a str;
// 3) C 系（HLSL/MSL/GLSL）：按 ';' 切分，每段两个 token（类型 字段名）。
fn c_struct(src: &str, name: &str) -> Vec<(String, String)>;
// 4) Rust：按 ',' 切分，剥 `pub `，按 ':' 切（注意 `[f32; 4]` 里有分号，绝不能按 ';' 切）。
fn rust_struct(src: &str, name: &str) -> Vec<(String, String)>;
// 5) 常量：C 系按行匹配 `#define `；Rust 匹配 `pub(crate) const NAME: f32 = V;`。
fn defines(src: &str) -> Vec<(String, f32)>;
fn rust_consts(src: &str) -> Vec<(String, f32)>;
```

**为什么不用 regex**：`regex` 会成为 `[build-dependencies]` 的第一个依赖；而这里需要的是「结构体字段序列」而不是
模式匹配，手写扫描器 90 行、可读、报错定位准确。**为什么不用 `syn`**：P07 只需要「字段名顺序」，引入完整
Rust 解析器是为一个断言拖进 20+ 依赖。

**实测（本机）**：`rustc --edition 2021 -O` 编译后对真实文件运行 →

```text
hlsl = [("float4","bounds"), ("float4","corner_radius"), ("float4","color0"), ("float4","color1"),
        ("float4","params0"), ("float4","params1"), ("float4","uv"), ("float4","clip_rect")]
rust = [("[f32; 4]","bounds"), … 同上 8 个 …]
clip = [("float4","rect"), ("float4","corner_radius"), ("uint4","meta")]
frame = [("float4","viewport"), ("float4","params")]
draw = [("float4","basis"), ("float4","translation"), ("float4","color0"), ("float4","clip_rect"), ("uint4","clip")]
OK: 8 fields + 3/2/5 struct fields + 11 constants match
```

### 6.3 规则与失败语义

* 任一规则不满足 → `panic!`（= `cargo build` 失败，不是 warning）。消息格式固定：

```text
ui_core: 着色器契约漂移
  文件:   src/render/shaders/common.glsl
  结构体: GpuPrimitive
  期望:   [bounds, corner_radius, color0, color1, params0, params1, uv, clip_rect]   （来自 src/paint/instance.rs）
  实际:   [bounds, corner_radius, color1, color0, …]
  修复:   同时改 Rust 与三套源码，并跑像素快照回归（contract.md §2.1）
```

* **反向保护**：任何 `struct` 解析结果为空 / 数量不符 / 找不到 `struct <Name>` 都算失败（防止「规则静默失效」——
  这是本类构建脚本最常见的失效模式）。
* 校验**不受** `ui_core_SKIP_SHADERS` 影响，也不受 host/target 不同影响：纯文本、常量级成本（14 个文件的逐行扫描）。

### 6.4 负例（实测，必须在实现期复跑）

| 变异                                                               | 期望                                                                            |
|--------------------------------------------------------------------|---------------------------------------------------------------------------------|
| `common.hlsl` 交换 `color0`/`color1` 两行（结构体住在 `common.*`） | 构建失败，消息 `着色器契约漂移 / 对象: GpuPrimitive`，期望/实际两个字段列表并列 |
| `common.hlsl` 把 `KYTRIX_KIND_IMAGE` 改成 3                        | 构建失败（实测：`KIND_IMAGE: shader=3 rust=2`）                                 |
| `common.glsl` 删掉 `corner_radius`                                 | 构建失败（字段数 2 ≠ 3）                                                        |
| 任意源文件加入 `asuint(x)`                                         | 构建失败（禁用 token）                                                          |
| 还原                                                               | 构建成功，7 个 DXIL 重新生成                                                    |

**复现步骤（实现期照做，记录结果进 PR / DEVIATIONS §十二的「实施记录」）**：

```powershell
$s = "crates/ui_core/src/render/shaders"
Copy-Item "$s\common.hlsl" "$s\common.hlsl.bak"
# 变异 1：交换两个字段行（连同注释一起交换，避免注释错位）
(Get-Content "$s\common.hlsl" -Raw).Replace("    float4 color0;         // 32`n    float4 color1;         // 48", "    float4 color1;         // 48`n    float4 color0;         // 32") |
  Set-Content "$s\common.hlsl" -Encoding utf8
cargo build -p ui_core    # 期望：失败，消息含「着色器契约漂移 / 对象: GpuPrimitive」
Move-Item "$s\common.hlsl.bak" "$s\common.hlsl" -Force
cargo build -p ui_core    # 期望：成功，7 个 DXIL 重新生成
```

变异 2/3/4（常量改值 / 删字段 / 插入 `asuint(`）同构：改 `common.hlsl` 的 `KYTRIX_KIND_IMAGE`、删 `corner_radius`、
在 `apply_gamma` 里加 `asuint(1.0)`，每次 `cargo build` 必须失败，`Move-Item` 还原后必须成功。变异文件必须是**临时**的：
`.bak` 用完即删，不得入库（检查单 §1.1 只允许 14 个源文件 + `contract.md`）。

---

## 7. 测试与门禁

### 7.1 P07 的验收命令（Windows 本机）

```powershell
# 1) 产物：7 个 DXIL + 全部以 DXBC 开头
cargo build -p ui_core
Get-ChildItem target/debug/build/ui_core-*/out/shaders | Select-Object Name, Length
#   期望 7 个文件：quad.vs/quad.ps/geometry.vs_ui/geometry.vs_textured/
#                  geometry.ps_ui/geometry.ps_textured_image/geometry.ps_textured_glyph（.dxil，4–8 KB）
Get-ChildItem target/debug/build/ui_core-*/out/shaders/*.dxil |
  ForEach-Object { ([System.IO.File]::ReadAllBytes($_.FullName)[0..3] | % { [char]$_ }) -join '' } # 全部 DXBC

# 1b) 契约锚点（build.rs 的 R4 校验；缺一行即构建失败）
Select-String -Path crates/ui_core/src/render/shaders/contract.md -Pattern "TriangleStrip|cull = NONE|ONE_MINUS_SRC_ALPHA|_SRGB|aa_padding"

# 2) 静态校验负例（§6.4）：改一行 → cargo build 必须失败 → 还原

# 3) 跳过模式：产物变成占位符，类型检查仍通过
$env:ui_core_SKIP_SHADERS = "1"; cargo check -p ui_core; Remove-Item Env:ui_core_SKIP_SHADERS

# 4) 跨 target：metadata（不编译）必须 rc=0；skip 模式的 cargo check 在 darwin 上已实测通过
cargo metadata --no-deps --filter-platform x86_64-unknown-linux-gnu --offline   # rc=0（P06 §13-5 的既有做法）
$env:ui_core_SKIP_SHADERS = "1"
cargo check -p ui_core --target aarch64-apple-darwin                        # rc=0（实测；产物=占位符）
cargo check -p ui_core --target x86_64-unknown-linux-gnu                    # 已知失败：yeslogic-fontconfig-sys 的 build.rs 需要交叉 pkg-config（与本阶段无关，见附录 C）
Remove-Item Env:ui_core_SKIP_SHADERS

# 5) 既有测试不变（基线：418 / 427 + 集成 5 / 409 / 418 + 5，P06 实施记录）
cargo test -p ui_core
cargo test -p ui_core --features test-support

# 6) lint / 格式（build.rs 也在 --all-targets 的 lint 范围内）
cargo +nightly clippy -p ui_core --all-targets -- -D warnings
cargo +nightly fmt -p ui_core -- --check
```

**`Cargo.lock` 必须零变化**（不新增任何依赖，含 build-dependencies）。

> **Linux / macOS 的 shader 编译不属本机门禁**：`glslangValidator`（Linux）与 `xcrun metal`（macOS）的编译成功是对应
> **CI job**（`shader-compile-linux` / `shader-compile-macos`）的完成判据，本机只能验证 HLSL（R-1 / V-54）。
> 本机对 GLSL/MSL 的检查止于 §3.8 的 10 条人工清单 + build.rs 的结构体/常量比对。

### 7.2 「`cargo test -p ui_core` 不变」的判定

P07 **不新增任何 Rust 测试**（`build.rs` 不能作为 test target 被 `cargo test` 执行）。测试项数量必须与 P06 实施记录
逐项相等（418 / 427+5 / 409 / 418+5）。任何变化都说明误改了生产代码。

### 7.3 为什么静态校验的测试放在「门禁」而不是 `#[test]`

`build.rs` 是 Cargo 的构建脚本 crate，`cargo test` 不会以 `--test` 编译它，`#[cfg(test)]` 永不执行。可行方案只有
「变异测试」（§6.4，脚本化）或把校验逻辑抽成库再测——后者会为测试引入生产代码，违反 AGENTS §1.2。**定案：变异测试 +
CI 脚本**：P07 落地时把 §6.4 的四条做成一个 `xtask`/CI step（本阶段只固化变异与还原动作本身）。

### 7.4 L3（像素快照）覆盖矩阵与容差（P18 执行，P07 定义）

| 维度 | case                                                   | 三平台容差                                                      |
|------|--------------------------------------------------------|-----------------------------------------------------------------|
| 形状 | 矩形 / 四角不同圆角 / 1 px 边框 / 圆角 clip            | 每通道 ≤ 1/255，**圆角 1 px 邻域 ≤ 2/255**（fwidth 的 √2 差异） |
| 阴影 | blur 0 / 4 / 16，offset 0 / (4,6)，颜色 α=1 / 0.5      | ≤ 2/255（`exp` 的 ULP 差异）                                    |
| 渐变 | 横 / 竖 / 径向；半透明端点                             | ≤ 1/255                                                         |
| 颜色 | 纯色 / 半透明叠加 / 深浅主题 / 高彩度 OKLCH gamut 边界 | ≤ 1/255（CPU 侧零容差）                                         |
| 图集 | 字形（拉丁/CJK/emoji）/ 图片 / 半透明 PNG 边           | ≤ 1/255                                                         |
| 裁剪 | 嵌套 2 层 / 滚动 clip / 完全移出视口                   | ≤ 1/255                                                         |
| 缩放 | 1 / 1.25 / 2 / 3                                       | ≤ 1/255                                                         |
| 几何 | 静态 VBO + 平移缩放、`Path` 硬边                       | ≤ 1/255                                                         |

**参数 golden（零容差、CPU 可断言）**：结构体布局、常量表、绑定表、blend state、NDC 公式（源码文本级）、
`ROOT` 112 B。全部由 §6 的静态校验覆盖 → 不需要 GPU。

---

## 8. 风险

| #   | 风险                                                          | 影响                               | 缓解                                                                                                                                                                                   |
|-----|---------------------------------------------------------------|------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| R-1 | **MSL / GLSL 本机无法编译**（无 xcrun / glslang）             | 语法错误只能在 macOS/Linux CI 暴露 | §3.8 的 10 条人工清单 + 每个 stage 文件首行 `#version`/`#extension` 固定；CI 增加 `shader-compile-linux` / `shader-compile-macos` 两个 job（P07 落地时补）；HLSL 是语义 oracle         |
| R-2 | GLSL 的 `#include` 需要 `GL_GOOGLE_include_directive` + `-I`  | Linux 构建失败                     | glslang 自 2017 支持该扩展；build.rs 传 `-I <shader_dir>`；若基线发行版的 glslang 不支持，改 build.rs 的「拼接式预处理」（把 common.glsl 拼到 stage 文件前写入 OUT_DIR），**源码不动** |
| R-3 | **dxc 版本差异**（`-WX` 会把新警告变成错误）                  | SDK 升级后构建失败                 | `-WX` 是质量闸门，保留；升级 SDK 时按报错修一行；`-HV 2018` 钉语言版本；CI 记录 `dxc --version`                                                                                        |
| R-4 | **sRGB 双重转换**（目标格式 + shader 内 gamma、或图集格式错） | 整体偏亮/偏暗，像素 golden 全红    | §2.3 的「唯一转换点」+ §3.8 第 8 条 + L3 的「半透明叠加」case；`apply_gamma` 恒等且有注释                                                                                              |
| R-5 | **UV 原点**（GL 的 v 翻转习惯）                               | 图片/字形上下颠倒                  | 三平台纹理原点都是左上；契约明写「不做 v 翻转」，且 **Vulkan 不做 `negative viewport`**（否则 NDC 的 Y 也要跟着改）；L3 的「字形（emoji）」case                                        |
| R-6 | `reserved` 字段/位被某一平台解释                              | 三平台分叉、难查                   | 契约「写 0 + 不读」；§6 的常量/结构校验；P06 用例已断言 `uv/color1` 为 0                                                                                                               |
| R-7 | 哨兵 `clip_rect` 被当矩形做数学                               | 所有图元被裁到 `y ≤ 0`（整屏消失） | §2.6 的「哨兵陷阱」+ §3.8 第 9 条 + `clip_index == 0` 的早退在 VS/FS 各一处                                                                                                            |
| R-8 | Vulkan 对 sRGB attachment 的混合空间与其他两平台不一致        | 半透明重叠处颜色不同               | 规范保证「先按线性混合、再转 sRGB 写入」；L3 的「半透明叠加」是验证点；P09 必须在 `VK_FORMAT_*_SRGB` 上**不**开 `VK_KHR_swapchain_mutable_format` 之类的转换路径                       |
| R-9 | 阴影 `σ = blur/3` 与美术预期（CSS `σ = blur/2`）不同          | 视觉上阴影略紧                     | 应用层 `base_ui` 的主题阴影值按本契约校准；`contract.md` 明写「blur 是可见外扩半径」；L3 固定 blur 表                                                                                  |

---

## 9. 需裁决项（5 项，含建议）

| #   | 议题                                                                                                                                      | 建议                                                                                                                                                                     | 备选与代价                                                                                                        |
|-----|-------------------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------|
| D-1 | GLSL 的**文件数**：单文件 + `#ifdef` 切换 stage（brief 原文 `quad.glsl`）vs 每 stage 一个文件（GLSL 共 **8** 个文件：1 common + 7 stage） | **每 stage 一个文件**。GLSL 一个编译单元只能一个 stage，`#version` 必须在首行，`#ifdef` 方案要求把 `#version` 移到 build.rs 生成的 shim 里，源码就不能被外部工具单独检查 | 单文件 ifdef：GLSL 文件数 4，但需要 build.rs 生成 shim + `-D` 宏，且 glslang 的宏/`-D` 语义不确定（本机无法验证） |
| D-2 | geometry per-draw 常量 **48 B → 80 B**                                                                                                    | **扩**。否则 geometry 通道无法表达圆角 clip（`FrameOp::Geometry` 已带 clip，P06 V-53）                                                                                   | 复用 `translation.z/w` 的 2 个 padding：破坏 P01 F-14 的「零值」契约，且与 `GeometryUniform` 语义表冲突           |
| D-3 | image 图集纹素：**预乘存储**（P02 §10.3 的公式）vs 直通 alpha + shader 预乘                                                               | **预乘存储**（跟随 P02，零偏差）。P15 负责「linear 预乘 → sRGB 编码」，P09 负责 sRGB 格式 SRV                                                                            | 直通 alpha：shader 多一次 `rgb *= a`，更贴近 PNG 解码的默认输出，但要改 P02 §10.3 的公式行（F 条目）              |
| D-4 | `apply_gamma` 在 v1 **恒等**                                                                                                              | **恒等**。gamma 归 sRGB 渲染目标，shader 不做转换；函数保留以满足主设计 §2.11 的函数集契约                                                                               | 删掉函数：偏离 §2.11 的「三份同名同语义」；shader 内转换：双重编码（R-4）                                         |
| D-5 | dxc 是否开 `-WX`                                                                                                                          | **开**。实测 7 个 entry 零警告；`-Wconversion` 是本项目最可能的错误类                                                                                                    | 不开：警告会被忽略，直到某一平台出错                                                                              |

---

## 10. 本阶段追加到 `design/DEVIATIONS.md` 的条目（§十二）

| 编号     | 内容                                                                                                                                                                                                                                                                                        |
|----------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-68** | geometry per-draw 常量 **48 B → 80 B**，并与 `FrameUniform` 合并为 **112 B `RootUniform`**（`frame @0`/`draw @32`）。P02 §9.5 的 `GeometryUniform`+`GeometryColor` 由 `GeometryDrawUniform` 取代；P09 上传 112 B（D3D12 28 DWORD root constants / Metal buffer(2) / Vulkan push constants） |
| **F-69** | clip 链的 **GPU 侧镜像 `ClipEntry`（48 B）** 与 CPU `ClipChain` 不同：丢弃 `aabb`（已在 `GpuPrimitive.clip_rect`）、`parent` 打进 `uint4.x`；P09 负责转换与上传，表项 0 恒为哨兵                                                                                                            |
| **S-68** | GLSL 采用 **1 个 `common.glsl` + 7 个 stage 文件**（`quad.vert/frag` + `geometry.ui.vert` / `geometry.textured.vert` / `geometry.ui.frag` / `geometry.textured_image.frag` / `geometry.textured_glyph.frag`），不是单文件 `quad.glsl`/`geometry.glsl`（D-1）                                |
| **S-69** | `apply_gamma` 在 v1 是**恒等函数**；linear→sRGB 的唯一编码点是 sRGB 渲染目标（D-4）                                                                                                                                                                                                         |
| **S-70** | 阴影 `σ = max(blur/3, 0.35)`、erf 用 A&S 7.1.26；`blur` 的语义固定为「可见外扩半径」，与 `params0.w = blur + 1` 的 damage/顶点外扩自洽                                                                                                                                                      |
| **S-71** | `ui_core_SKIP_SHADERS=1`（或 host≠target）时 build.rs **写占位产物**而不是不写，使 `include_bytes!` 在跨 target 检查时可解析；P09 的 magic 校验把占位符变成显式 `ShaderLoad` 错误                                                                                                           |
| **S-72** | 实例缓冲绑定为**只读 structured/storage buffer**（SRV/`device const`/std430），不是 CBV；三平台常量块统一为 112 B 单块                                                                                                                                                                      |
| **V-54** | MSL/GLSL 未本机编译（仅静态审查）；Vulkan sRGB 混合空间、dxc `-WX` 的版本漂移、glslang 的 `#include` 支持——由 P07 落地时的 Linux/macOS CI job 与 P18 的 L3 关闭                                                                                                                             |

---

## 附录 A · 实现检查单（按执行顺序）

1. `Cargo.toml`：`[package] build = "build.rs"`；**不加** `[build-dependencies]`；`Cargo.lock` 零变化。
2. `src/render/shaders/contract.md` ← §2 逐字。
3. 14 个源码：`common.*` 3、`quad.*`（HLSL 1 / MSL 1 / GLSL 2）、`geometry.*`（HLSL 1 / MSL 1 / GLSL 5）。HLSL 用 §3.2–§3.4 的全文。
4. `build.rs`：§5 的 5 个函数 + §6 的 5 个解析函数；`panic!` 消息按 §6.3 的格式。
5. 逐条跑 §7.1 的 6 组命令；§6.4 的 4 个负例必须失败、还原后必须成功。
6. §3.8 的 10 条人工审查（MSL/GLSL）；把结果写进 PR 描述。
7. DEVIATIONS §十二 追加。

## 附录 B · probe 记录（复现方式）

```powershell
# 目录：%TEMP%\kg-p07-shaders（common.hlsl / quad.hlsl / geometry.hlsl / probe.rs）
$dxc = "C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\dxc.exe"
& $dxc --version
# dxcompiler.dll: 1.9(5184-21d28f72)(1.9.2602.17) - 1.9.2602.17 (21d28f727)

foreach ($e in @(@('quad.hlsl','vs_6_2','vs_main'), @('quad.hlsl','ps_6_2','ps_main'),
                 @('geometry.hlsl','vs_6_2','vs_ui'), @('geometry.hlsl','vs_6_2','vs_textured'),
                 @('geometry.hlsl','ps_6_2','ps_ui'), @('geometry.hlsl','ps_6_2','ps_textured_image'),
                 @('geometry.hlsl','ps_6_2','ps_textured_glyph'))) {
  & $dxc -T $e[1] -E $e[2] -HV 2018 -WX -nologo -Fo "out\$($e[2]).dxil" $e[0]
  if ($LASTEXITCODE -ne 0) { throw $e[2] }
}
# 实测：7/7 成功、总耗时 187 ms；产物 4164–7864 B；前 4 字节 = DXBC，含 DXIL 段
```

| 实测项                 | 结果                                                                                                                          |
|------------------------|-------------------------------------------------------------------------------------------------------------------------------|
| dxc 路径               | `C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\dxc.exe`（`WindowsSdkDir` **未设置**，`ProgramFiles(x86)` 可用） |
| 版本                   | `dxcompiler.dll 1.9(5184-21d28f72) / 1.9.2602.17`                                                                             |
| 7 个 entry             | 全绿（`-WX`），187 ms                                                                                                         |
| `-HV 2018`             | 省略也能编过（保留用于钉版本）                                                                                                |
| `-WX`                  | 生效：`-Wconversion` 被升级为 `error: [-Werror,-Wconversion]`                                                                 |
| 失败退出码             | `-2147467259`（0x80004005）→ 必须用 `status.success()`                                                                        |
| 无 `-I`                | 同目录 `#include` 正常（任意 cwd + 绝对源路径亦通过）                                                                         |
| 坏 entry 名            | `error: missing entry point definition`                                                                                       |
| 产物 magic             | 全部 `DXBC` + 含 `DXIL`                                                                                                       |
| `rustc` 静态校验 probe | 8 字段 + 3/2/5 结构字段 + 11 常量全部一致；4 个负例全部失败                                                                   |
| 本机工具               | `glslangValidator` / `xcrun` **不存在** → MSL/GLSL 只能静态审查（R-1）                                                        |

---

## 附录 C · 实施记录（2026-11-12，P07 落地）

| 项              | 实测                                                                                                                                                                                                                                                                                                                                                 |
|-----------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 新增文件        | `..`（**648 行**）+ `crates/ui_core/src/render/shaders/{contract.md + 14 个源文件}`                                                                                                                                                                                                                                                                  |
| 修改文件        | `..`：只加 `build = "build.rs"`；**无** `[build-dependencies]`；`Cargo.lock` **零变化**                                                                                                                                                                                                                                                              |
| dxc             | SDK `10.0.26100.0` / `dxcompiler 1.9.2602.17`；7 个 entry 全绿（`-WX`）；`cargo build -p ui_core` ≈ 2.5–4.6 s（含 lib）                                                                                                                                                                                                                              |
| 产物            | 7 × `.dxil`：4072–7864 B，全部 `DXBC` magic（build.rs 的 `check_magic` 自检，失败即 panic）                                                                                                                                                                                                                                                          |
| 门禁            | `cargo test -p ui_core` → **418 passed**（与 P06 基线一致）；`cargo +nightly clippy -p ui_core --all-targets -- -D warnings` rc=0（含 build script）；`cargo +nightly fmt -p ui_core -- --check` rc=0；`ui_core_SKIP_SHADERS=1 cargo check -p ui_core --target aarch64-apple-darwin` rc=0                                                            |
| 占位产物        | skip 模式下 `quad.metallib` / `geometry.metallib` 各 22 B（`KYTRIX-SHADER-SKIPPED\n`）                                                                                                                                                                                                                                                               |
| 负例            | **4/4 全部按预期失败**并输出「着色器契约漂移」：①`common.hlsl` 交换 `color0`/`color1`；②`KYTRIX_KIND_IMAGE` 2→3；③`common.glsl` 删 `corner_radius`；④插入 `asuint(`。每次还原后 `cargo build` 成功、7 个产物重建                                                                                                                                     |
| 文档-源码一致性 | §3.2–§3.4 的 HLSL 代码块与落盘文件**逐字节相同**（脚本比对：`True/True/True`）；改代码必须同步改文档，否则下一轮 §3.2 的「全文」就失真                                                                                                                                                                                                               |
| 已知限制        | ①`cargo check --target x86_64-unknown-linux-gnu` 在本机 **rc=101**：`yeslogic-fontconfig-sys 6.0.1` 的 build.rs 要求交叉 pkg-config（P07 之前就存在，与 shader 无关；本阶段 build.rs 已正确进入 skip 分支并打印 warning）；②MSL/GLSL 未在本机编译（无 xcrun / glslang）；③`cargo metadata --no-deps --filter-platform x86_64-unknown-linux-gnu` rc=0 |
| 与设计的偏差    | **无新增 F/S 条目**。实现期细化 3 处并已回写：MSL 用 `mix`（MSL 无 `lerp`）；GLSL 整型常量用 `u` 后缀、并由 `common.glsl` 提供 `lerp`/`saturate` 同名包装（函数体与 HLSL 逐行一致）；build.rs 实际 648 行（设计估计 400）。这些都在 §3.7 的对照表与 §5.1 的结构里                                                                                    |

