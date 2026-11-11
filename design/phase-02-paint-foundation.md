# Phase 02 · 绘制基础层（Scene / Primitive / Color / Geometry / Atlas / Damage）— 详细设计（v1）

> **文档类型**：实施阶段详细设计（Phase 02 的唯一权威输入）
> **上游**：`custom-gui-engine-design.md`（v2.0，§0 I2/I8/I11/I15、§2.3、§3、§4.4、§5.1–§5.9、§9.3、附录 B/C）、
> `design/phase-01-foundation.md`（v2，§3 文件树、§4 模块根模板、§4.5 路径表、§6.3 math API、§7 像素/AA 契约）、
> `design/DEVIATIONS.md`（A-01、M-01…M-09、F-09、F-13、F-14、F-17、S-07）、`AGENTS.md` 全文。
> **版本**：v1（2026-11-12）。
> **本步代码状态**：**本步只交付设计**，不创建 `crates/kytrix_gui/src/paint*`、不改 `Cargo.toml` / `Cargo.lock`、不 commit。
> 文中所有 Rust 片段是**签名级契约**（字段名、类型、可见性、语义），不是可编译实现。
> **事实基线**：Rust `1.97.1 (8bab26f4f 2026-07-14)` / nightly `1.99.0 (771916f90 2026-08-08)`，clippy `0.1.99`。
> 依赖 API 全部以 `E:\.cargo\registry\src\rsproxy.cn-e3de039b2554c837\{color-0.3.3, etagere-0.3.0, bytemuck-1.25.2, euclid-0.22.14}\` 的
> **真实源码**为准，并用 `cargo info --registry crates-io` + `%TEMP%\kg-{dead-probe,pod-probe,pod-probe2}` probe 交叉验证。

---

## 0. 结论摘要（v2，已按 2026-11-12 两轮审查修订）

> 第一轮：5 条裁决（§17.1）。第二轮审查结论 **FAIL**，4 条阻塞 + N1–N14 的修复见 **§17.2**；
> 其中 **B1/B2/B3/N11** 把 `Scene` 整体推迟到 P03，P02 的 `paint/scene.rs` 只剩 `Lane` 与 `MAX_CLIP_DEPTH`（均 `pub(crate)`）。

1. **落地范围（裁决 ① + B1/B2/B3/N11）**：P02 **实际落地 7 个文件** —— `paint.rs`、
   `paint/{color,primitive,canvas,geometry,atlas}.rs`（公开）+ `paint/scene.rs`（**仅** `pub(crate)` 的 `Lane` /
   `MAX_CLIP_DEPTH`），外加 `lib.rs` 一行与 `Cargo.toml` 一行（`bytes`）。**推迟**：`Scene` / `LaneId` / lane 分配与绘制序 →
   **P03**；`paint/instance.rs` → **P07**；`render/damage.rs` → **P06**；`render/atlas.rs` + `render.rs` → **P05**。
   原因不是取舍偏好，而是 `-D warnings` 的硬事实：没有同相非测试消费方的 `pub(crate)`（乃至私有模块内的 `pub`）item
   都会被 `dead_code` 判死（probe 见 §2.4），而 §1.3 又禁止把这些能力提升为应用层公开面。
2. **P02 不得创建任何会触发 `dead_code` 的文件/模块**：每个 `pub(crate)` item 必须在**非 test 构建**里至少有一个消费者；
   P02 只允许三个：`Lane::new`（`Canvas::new`）、`Lane::push`（`Canvas::push_*`）、`Lane::clip_depth`（`Lane::push` 内部 +
   `Canvas::push_clip/pop_clip`）。`MAX_CLIP_DEPTH` 由 `Lane::push` 与 `Canvas::push_clip` 消费。
3. **`GpuPrimitive` 必须 `#[repr(C, align(16))]`**：主设计 §2.3/附录 B 的 `#[repr(C)]` + `align_of == 16` **不成立**
   （全 `[f32;4]` 的 `repr(C)` 对齐是 4；probe §2.4a）。`contract.md` 必须写「16 B 对齐」，且**三平台实例缓冲基址也须
   16 B 对齐**（§16 R7）。**代码随 P07 落地**（裁决 ④），P02 冻结完整 offset/kind/flags 表（§8.2/§8.3）。
4. **8 类 Primitive 只有 4 类产生 `GpuPrimitive`**：`Quad`(1)、`GlyphRun`(**N 字形 → N 实例**)、`Image`(1)、`Shadow`(1)；
   `Path`/`Geometry` 走 geometry 通道（0 实例 + 1 次几何绘制）；`Canvas` **展平为子图元**；`Clip` 是**栈指令**（0 实例）。
5. **clip 只有一处表示**：`Primitive::Clip(ClipOp::{Push,Pop})`；链在**编码期**解析成「父链 id + rect/radii + 交集 AABB」表，
   `params1.x` 是表索引 → lane 复用不会拿到过期 id。上限 `MAX_CLIP_DEPTH: u8 = 8`；C2/C3 在 P02 由 `Lane::push` 的
   **debug 断言**与 `Canvas` 的 **`bool` 返回**（N6）双重保证；C1 由 P06 展平阶段断言。
6. **Color（D23）**：5 变体 + **独立 alpha**（永不预乘存储）；`resolve` = 转 linear sRGB → 自建 CSS Color 4 **chroma 二分**
   （24 次定长）→ premultiply；`interpolate` 在 premultiplied 空间。**所有分量统一 `NaN→0` + 定义域 clamp**（`h` 先
   `rem_euclid(360)`，N4），输出严格在 `[0,1]`。**`color 0.3.3` 没有 `map_chroma`**（§5.8.1 原文有误）→ chroma 归约靠
   **直接构造** `DynamicColor` 改 `components[1]`（F-28）。「逐位可复现」限定为**同平台**，跨平台允许 ULP 差异，像素 golden
   按 8 位比较（N5）。
7. **渐变 v1 只支持双色**（`color0`/`color1` + `params0.z`），在 **linear premultiplied 空间**由 shader 插值；OKLab 渐变 +
   CPU 梯度 LUT **延后**（128 B 只有两个颜色槽）→ F-32。
8. **Path v1 不做 AA，也不承载两阶段 coverage 标记**（裁决 ③）：硬边三角形；AA 由后续两阶段 coverage（`kind = 4` 预留）
   或 MSAA 承担。像素 golden 覆盖矩阵必须标注该限制（§14.10 / F-26）；小尺寸矢量仍走构建期图集。
9. **`Canvas` 是唯一公开写入口**（裁决 ② + N6）：`push_*` 8 个方法，其中 `push_clip`/`pop_clip` **返回 `bool`**（超深/空栈
   返回 `false`，不 panic，全部 `# Panics: none`）；内部持有 **`pub(crate)` 的 `scene::Lane`**；`Canvas` 尺寸 48 B。
   `Scene` 在 P02 不存在（P03 落地，届时也只读）。
10. **边框 + 渐变拆两个 `Quad`**（裁决 ⑤）：先填充/渐变、后边框；两者都半透明时边框内缘**双次 premultiplied over**
    （与 CSS 一致）→ 应用层责任：边框不透明，或 `fill` 用与背景预乘合成后的不透明色（§8.5 / S-14）。
11. **Geometry**：`GeometryPipeline::{Ui, Textured{atlas}}`（`Path` 并入 `Ui`，F-31）；`Ui`/`Path` = 8 B 位置、
    `Textured` = 16 B 位置+UV；per-draw 常量 = `GeometryUniform`(32 B) + `GeometryColor`(16 B)（变换部分仍是两个 vec4）；
    `GeometryId` 是 `Copy` → 必须显式 `release_geometry(id)`（F-30）。
12. **图集**：每 kind **单页**（Glyph 2048² **R8** 4 MiB；Image 2048² **BGRA8 sRGB** 16 MiB）；UV 用**边**、按**请求内容尺寸**算
    （etagere 返回的 rect 可能被放大）；1 纹素清空 padding 不计入 UV；`generation` 只在 `clear()` 时 +1；分配器规格冻结、
    随 **P05** 落地（§10.4–§10.6 / S-11）。
13. **damage**（随 **P06** 落地于 `render/damage.rs`）：`covering` → 物理像素外扩 → ∩clip → 「并集面积 ≤ 两者之和」合并 →
    `>16` 或 `>50%` 面积 → `Full`。**N3**：只有全部 dirty rect 都落在 scroll 区内才保留 scroll hint，否则放弃 scroll、保留
    全部 rects。**N13**：`Clip` 本身不产生像素，不 push damage。
14. **零 unsafe、零 `#[allow]`/`#[expect]`、零 `serde_json`、零预防性抽象**；每个公开类型标注消费 Phase（§4.3）。
15. **依赖增量**：`crates/kytrix_gui/Cargo.toml` 追加 **`bytes = { workspace = true }`**（`GeometryDesc` 用 `Bytes`；
    P01 §5.2 漏列，workspace 根已声明）。
16. **文档完整性**：§15-11/12 给出去重后的复核命令（代码围栏内 `||`/`&&` 计数 + 双空格运算符扫描），DEVIATIONS **S-17** 记录方法。

---
## 1. 范围与非目标

### 1.1 本阶段交付

| 类别 | 内容 |
|---|---|
| **P02 实际落地的代码（7 个文件）** | `paint.rs`、`paint/color.rs`、`paint/primitive.rs`、`paint/scene.rs`、`paint/geometry.rs`、`paint/canvas.rs`、`paint/atlas.rs`；`lib.rs` 追加 `pub mod paint;`；`Cargo.toml` 追加 `bytes` |
| **规格已冻结、实现推迟** | `paint/instance.rs`（P07）、`render/damage.rs`（P06）、`render/atlas.rs` + `render.rs`（P05）——签名/算法/常量在本文件逐字定死，接手 Phase 不得改语义 |
| 算法规格（交给接手 Phase） | Color 转换/gamut mapping/插值（P02 落地）；instance 编码 1→N（P07）；damage 外扩/合并/阈值（P06）；图集装箱/脏行/generation（P05） |
| 文档 | 本文 + `design/DEVIATIONS.md` §七（F-23…F-33、S-08…S-15、V-11…V-16） |

### 1.2 明确的非目标（本阶段一行代码都不写）

- **不碰 GPU**：没有 `wgpu`/`D3D12`/`Metal`/`Vulkan`/bindless/upload/fence/retire 的任何代码。
- **不碰平台层**：没有 `platform/`、没有 `HWND`/`NSView`/`wl_surface`（I9）。
- **不碰节点树**：`Node`/`NodeId` 的归属只在 §7 描述接口形状，`node/` 由 P04 落地。
- **不落地 `Scene` 的 lane 分配/绘制序方法**（`new_lane` / `release_lane` / `lane_mut` / `set_order` / `clear_frame`）：
  P02 没有任何非测试消费者（裁决 ① + B1/B2/B3/N11）；P02 只落地 `Lane`（`new`/`push`/`clip_depth`，由 `Canvas` 消费）与
  应用层的写入口是 **`Canvas`**（裁决 ②，§11）。
- **不做布局、文本、a11y、交互**：`GlyphRun` 只定义 P06 的**输入交接形状**，不实现 shaping/raster（P06）。
- **不写路径 AA**（裁决 ③）：v1 `Path` 是硬边三角形（§6.3.4）。
- 不引入任何 workspace 之外的新依赖（`bytes` 复用既有声明）。

### 1.3 不变量落地映射

| 不变量 | P02 的落地点 |
|---|---|
| **I2** 一套数据契约 | `paint::primitive::Primitive` 是唯一绘制指令定义；`paint::instance::GpuPrimitive`（P07 落地）是唯一 CPU→GPU 布局；`paint::atlas::AtlasTile` 是唯一图集引用；`paint::geometry::GeometryDesc` 是唯一静态几何描述 |
| **I8** 三套 shader 共享字节布局 | §8 的 offset/kind/flags/params 表就是 `render/shaders/contract.md`（P10）的镜像来源；任何改动必须同时改 Rust + 三份 shader |
| **I11** 逻辑存储 / 物理绘制 | `Primitive` 全逻辑 `f32`；只有 `instance` 编码处调用 `math` 的三种换算（P01 §7.1） |
| **I12** 指令与像素分别验证 | §13 列出 `test_support` 需要的**全部只读面**；`Primitive`/`Scene`/`AtlasTile` 可 `PartialEq` 比较（§3.8 full-refresh oracle 直接比较 `(Primitive, order, bounds)`） |
| **I15** 颜色只有引擎一份语义 | `Color`/`ColorResolver` 是唯一定义；`color 0.3.3` 只出现在 `paint/color.rs` 一个文件的 `use` 里（§5.7） |
| **D2** 不内化 GPUI / 不引重依赖 | 只用 workspace 已冻结的 `color`/`bytemuck`/`bytes`/`etagere`/`lyon` |

---

## 2. 事实基线与核验记录

> 全部结论都可用本节命令复现；「路径:行号」相对 crate 的 `src/`。

### 2.1 `color 0.3.3`

`cargo info color@0.3.3 --registry crates-io`：`license = Apache-2.0 OR MIT`、`rust-version = 1.82`、
`features: default = ["std"], bytemuck, libm, serde`（引擎只用默认）。

```text
lib.rs:113   pub use color::{AlphaColor, HueDirection, OpaqueColor, PremulColor};
lib.rs:118   pub use dynamic::{DynamicColor, Interpolator, UnpremultipliedInterpolator};
lib.rs:119   pub use flags::{Flags, Missing};
lib.rs:120   pub use gradient::{gradient, gradient_unpremultiplied, GradientIter, UnpremultipliedGradientIter};
lib.rs:123   pub use tag::ColorSpaceTag;

dynamic.rs:34  pub struct DynamicColor { pub cs: ColorSpaceTag, pub flags: Flags, pub components: [f32; 4] }
               // 字段全 pub、无 #[non_exhaustive] → 可直接构造；components[3] 是**独立 alpha**（非预乘）
               // 同文件 :656 自断言 size_of::<DynamicColor>() == 20（`const _: () = if … { panic!(…) }`）
dynamic.rs:56  pub struct Interpolator { /* 私有字段 */ }
dynamic.rs:87  pub fn to_alpha_color<CS: ColorSpace>(self) -> AlphaColor<CS>
dynamic.rs:97  pub fn from_alpha_color<CS: ColorSpace>(color: AlphaColor<CS>) -> Self
dynamic.rs:161 pub fn convert(self, cs: ColorSpaceTag) -> Self            // 含色度适应
dynamic.rs:171 pub fn convert_absolute(self, cs: ColorSpaceTag) -> Self   // 无色度适应
dynamic.rs:264 pub fn scale_chroma(self, scale: f32) -> Self              // ← 唯一 chroma 归约入口
dynamic.rs:283 pub fn clip(self) -> Self                                  // 只 clamp 到自然色域，**不做 CSS gamut mapping**
dynamic.rs:365 pub fn interpolate(self, other, cs: ColorSpaceTag, direction: HueDirection) -> Interpolator
dynamic.rs:442 pub fn interpolate_unpremultiplied(...) -> UnpremultipliedInterpolator
dynamic.rs:530 pub fn map_lightness(self, f: impl FnOnce(f32) -> f32) -> Self
dynamic.rs:550 pub fn map_hue(self, f: impl FnOnce(f32) -> f32) -> Self
dynamic.rs:609 pub fn eval(&self, t: f32) -> DynamicColor                  // Interpolator::eval，返回插值空间、非预乘

color.rs:52   pub struct AlphaColor<CS> { pub components: [f32; 4], /* PhantomData */ }
color.rs:78   pub struct PremulColor<CS> { pub components: [f32; 4], /* PhantomData */ }   // #[repr(transparent)]
color.rs:105  pub enum HueDirection { Shorter = 0, Longer = 1, Increasing = 2, Decreasing = 3 }  // Default = Shorter
color.rs:410  pub const fn AlphaColor::new(components: [f32; 4]) -> Self
color.rs:450  pub fn AlphaColor::convert<TargetCs: ColorSpace>(self) -> AlphaColor<TargetCs>
color.rs:458  pub const fn AlphaColor::premultiply(self) -> PremulColor<CS>
color.rs:628  pub const fn PremulColor::new(components: [f32; 4]) -> Self
color.rs:647  pub fn PremulColor::convert<TargetCS: ColorSpace>(self) -> PremulColor<TargetCS>
color.rs:661  pub const fn PremulColor::un_premultiply(self) -> AlphaColor<CS>

tag.rs:26     pub enum ColorSpaceTag { Srgb = 0, LinearSrgb = 1, Lab, Lch, Hsl, Hwb, Oklab = 6, Oklch = 7,
                                      DisplayP3 = 8, A98Rgb, ProphotoRgb, Rec2020, Aces2065_1, AcesCg,
                                      XyzD50, XyzD65 }
tag.rs:215    pub fn convert(self, target: Self, src: [f32; 3]) -> [f32; 3]
tag.rs:346    pub fn scale_chroma(self, src: [f32; 3], scale: f32) -> [f32; 3]
tag.rs:362    pub fn clip(self, src: [f32; 3]) -> [f32; 3]

colorspace.rs:76    pub trait ColorSpace { const IS_LINEAR / LAYOUT / TAG / WHITE_POINT / WHITE_COMPONENTS;
                                           fn to_linear_srgb / from_linear_srgb / clip ... }
colorspace.rs:322   pub struct LinearSrgb;   :372 pub struct Srgb;   :443 pub struct DisplayP3;
colorspace.rs:1288  pub struct Oklch;        // 分量 [L, C, h]：L 同 Oklab（0..1）、C 自然下界 0、通常不超过 0.5、h 单位**度**
flags.rs:23   pub struct Flags { /* 私有 */ }  impl Flags { pub const fn from_missing(Missing) -> Self;
                                               pub const fn missing(self) -> Missing }
flags.rs:48   pub struct Missing(u8);        impl Missing { EMPTY, single(ix), contains(ix), insert(ix), is_empty() }
```

**核验结论（与主设计/ P01 的四条差异）**

| # | 事实 | 影响 |
|---|---|---|
| 1 | **没有 `map_chroma`**（`grep` 全 crate 无此符号；只有 `scale_chroma` / `map_lightness` / `map_hue`） | §5.8.1 原文「`map_lightness/map_hue/map_chroma`」有误；gamut mapping 的 chroma 归约用 `DynamicColor::scale_chroma(scale)`（对 Oklch 就是 `C * scale`）；我们的 bisection **直接构造** `DynamicColor` 改 C（无除法、无 `C == 0` 特判，与 `scale_chroma` 等价）→ F-28 |
| 2 | `clip()` 只把分量 clamp 到**该空间自身的自然色域**，不处理「OKLCH → sRGB 越界」 | P01 A.4 的判断正确，必须自建 gamut mapping（§5.4） |
| 3 | `DynamicColor::interpolate` 内部**预乘**颜色通道（`InterpolationAlphaSpace::Premultiplied`），`eval` 再解预乘 | 与 CSS Color 4 §12.3 一致；我们的 `interpolate` 直接复用，不需要自己处理预乘 |
| 4 | 存在 `gradient` / `gradient_unpremultiplied` / `GradientIter`（lib.rs:120、gradient.rs:114） | v1 **不用**（它产出迭代器，仍需每帧 CPU 求值；LUT 方案见 §16 R3） |

### 2.2 `etagere 0.3.0`

`cargo info etagere@0.3.0 --registry crates-io`：`license = MIT OR Apache-2.0`；features `checks` / `ffi` / `serde` /
`serialization`（默认全关，引擎不需要）。

```text
lib.rs:95    pub use euclid::{point2, size2};
lib.rs:97    pub type Point = euclid::default::Point2D<i32>;
lib.rs:98    pub type Size = euclid::default::Size2D<i32>;
lib.rs:99    pub type Rectangle = euclid::default::Box2D<i32>;      // euclid box2d.rs: pub min / pub max
lib.rs:104   pub struct AllocatorOptions { pub alignment: Size, pub vertical_shelves: bool, pub num_columns: i32 }
lib.rs:118   pub const DEFAULT_OPTIONS: AllocatorOptions = { alignment: size2(1,1), vertical_shelves: false, num_columns: 1 }
lib.rs:137   pub struct Allocation { pub id: AllocId, pub rectangle: Rectangle }
lib.rs:146   pub struct AllocId(/* u32 */);   // index: u16   gen: u16，public serialize()/deserialize(u32)

allocator.rs:71   pub struct AtlasAllocator { /* 私有 */ }
allocator.rs:86   pub fn with_options(size: Size, options: &AllocatorOptions) -> Self
allocator.rs:114  pub fn new(size: Size) -> Self
allocator.rs:118  pub fn clear(&mut self)
allocator.rs:179  pub fn size(&self) -> Size
allocator.rs:188  pub fn allocate(&mut self, mut size: Size) -> Option<Allocation>
allocator.rs:394  pub fn deallocate(&mut self, id: AllocId)         // assert!(allocated) + assert_eq!(generation) → **越界/重复释放会 panic**
allocator.rs:514  pub fn is_empty(&self) -> bool
allocator.rs:530  pub fn allocated_space(&self) -> i32
allocator.rs:535  pub fn free_space(&self) -> i32
allocator.rs:539  pub fn iter(&self) -> Iter
allocator.rs:676  pub fn get_index(&self, id: AllocId) -> u32
allocator.rs:686  pub fn get(&self, id: AllocId) -> Rectangle
```

**必须写进设计的三条实现事实**（`allocator.rs` 源码）：

1. `allocate` 在 `size.is_empty() || width > u16::MAX || height > u16::MAX` 时返回 `None`（:189-193），且宽度还必须
   `<= shelf_width`（`with_options` 里 `shelf_width = size.width / num_columns` 并对齐下取整，:99-100）→ **默认单列时，任何宽度
   超过图集宽度的分配必然失败**，不是「先失败后扩容」而是结构上不可能。
2. `adjust_size`（:1006）把请求尺寸**向上对齐**到 `options.alignment` 的倍数；`shelf_height`（:1016）把高度向上归到
   `8/16/32/64` 分档 → **返回的 `rectangle` 可能大于请求尺寸**，UV 必须按「请求的内容尺寸」算，不能按 `rect.max` 算。
3. `deallocate` 对非法 / 重复 / 跨代的 `AllocId` 直接 `panic`（:398、:399）→ 我们自己的图集必须持有
   `(AllocId → AtlasTile)` 记录并只在**自己记录过的 id** 上调用 `deallocate`（§10.4）。

`etagere` 没有「扩容」API：扩容 = 新建一个更大的 `AtlasAllocator` + 按记录的 tile 顺序重新 `allocate` + `generation += 1`（§10.6）。

### 2.3 `bytemuck 1.25.2`

`cargo info bytemuck@1.25.2 --registry crates-io`：`license = Zlib OR Apache-2.0 OR MIT`；
`features: derive = ["bytemuck_derive"]`（`Cargo.toml` 的 `[features]`，与 workspace 根声明的 `features = ["derive"]` 一致）。

```text
pod.rs:37   pub unsafe trait Pod: Zeroable + Copy + 'static {}
lib.rs:412  pub fn cast_slice<A: NoUninit, B: AnyBitPattern>(a: &[A]) -> &[B]
lib.rs:235  pub use bytemuck_derive::{AnyBitPattern, ByteEq, ByteHash, CheckedBitPattern, Contiguous,
                                     NoUninit, Pod, TransparentWrapper, Zeroable};
```

`Pod` 隐含 `NoUninit`，因此 `bytemuck::cast_slice::<GpuPrimitive, u8>(&instances)` 是上传路径的**唯一**字节视图，
不需要 `unsafe`（本阶段与后续 phase 的 instance 上传都不需要 unsafe）。

### 2.4 本地 probe（决定本设计形态的三个实测结论）

**(a) `#[repr(C)]` 的 128 B 结构体，`align_of` 是 4 而不是 16** — `%TEMP%\kg-pod-probe`：

```text
error[E0080]: evaluation panicked: assertion failed: align_of::<GpuPrimitive>() == 16
  --> src\lib.rs:18:5
```

**(b) 补 `align(16)` 后全部成立** — `%TEMP%\kg-pod-probe2`：`#[repr(C, align(16))]` + `bytemuck::{Pod, Zeroable}` derive，
`size_of == 128`、`align_of == 16`、8 个 `offset_of!` 逐个成立、`cargo +nightly clippy --all-targets -- -D warnings` **退出 0**。
附带收益：`Vec<GpuPrimitive>` 的元素天然 16 B 对齐，D3D12 常量/结构化缓冲、Vulkan `minStorageBufferOffsetAlignment`
的最小对齐要求不需要额外拷贝。

**(c) 无消费者的 `pub(crate)`/私有模块 `pub` 一律 `dead_code`** — `%TEMP%\kg-dead-probe`：

```text
error: struct `InternalThing` is never constructed          （pub(crate) struct，仅在 #[cfg(test)] 里构造）
error: function `encode` is never used
error: method `secret` is never used                        （pub(crate) 方法，仅测试调用）
```

`%TEMP%\kg-dead-probe2`（`mod hidden { pub struct X; }`，模块私有、item `pub`）：

```text
error: struct `UnreachableButPub` is never constructed
error: associated items `new` and `crate_visible` are never used
```

→ **`pub` 只在「从 crate 外部可达」时才算 reachable**；`#[cfg(test)]` 里的使用**不能**消除非 test 构建的 `dead_code`。
这条事实直接决定了 §3.2 的落地顺序。

### 2.5 尺寸 probe（`%TEMP%\kg-size-probe`，供 §6.4/§14 的 `size_budget` 测试引用）

用与本文逐字段相同的 mock 类型（`repr` 未标注、仅 `derive(Clone, Copy, Debug, PartialEq)`）在 `rustc 1.97.1` 上实测：

| 类型 | `size_of` | 说明 |
|---|---|---|
| `Point` / `Size` / `Rect` | 8 / 8 / 16 | 与 `math.rs` 一致 |
| `Color` | **20** | 5 变体 × 4×f32 + tag，对齐 4 |
| `Border` / `Option<Border>` | 24 / **24** | `Option` 使用了 `Color` 枚举 tag 的 niche，**零开销** |
| `Gradient` / `QuadPaint` / `Quad` | 44 / 48 / **80** | `Quad` 是最大变体 |
| `AtlasTile` | **24** | `AtlasKind`(1) + 4×f32 + `generation:u32`，对齐 4 |
| `PositionedGlyph` / `GlyphRun` | **40** / 48 | 40 = `AtlasTile`24 + `Point`8 + `Size`8 |
| `Image` / `Path` / `Shadow` | 60 / 44 / 48 | — |
| `ClipOp` / `GeometryDraw` | 36 / 32 | — |
| `Lane`（P02，`pub(crate)`） | **32** | `Vec<Primitive>`(24) + `clip_depth: u8` + 对齐 |
| `Canvas` | **48** | `Rect`(16) + `Lane`(32) |
| **`Primitive`** | **80** | `= Quad` 变体；`large_enum_variant` 无需触发 |

结论：`Primitive` 80 B（本文 §6.4 与 §14.2 的 `size_budget` 期望值）、`AtlasTile` 24 B（§14.6）、`Color` 20 B（§14.1 第 16 项）。
`Option<Border>` 的零开销是**实测事实**（依赖 `Color` 的 tag niche）；若将来 `Color` 改成 `#[repr(u8)]`/`#[repr(C)]` 显式标注，
这个 niche 可能消失（`Border` 变 28 B），§14 的 `size_budget` 会立刻发现 —— 因此 §14 的测试是**契约守护**，不是形式主义。

---

## 3. 落地可行性与文件树

### 3.1 文件树与阶段归属（裁决 ① 定稿）

```text
crates/kytrix_gui/src/
├── lib.rs                          P01   # P02 只追加 `pub mod paint;`（**不**加 `mod render;`，见 §3.2）
├── error.rs                        P01
├── math.rs                         P01
├── paint.rs                        P02 ✅ 公共模块根：只有 `pub mod` 行（无 item、无 re-export）
│   ├── color.rs                    P02 ✅ Color / ColorResolver / LinearPremulRgba / ColorBrush / Interpolation / HueDirection
│   ├── primitive.rs                P02 ✅ Primitive（8 类）+ Quad/GlyphRun/Image/Path/Shadow/ClipOp/GeometryDraw
│   ├── scene.rs                    P02 ✅ **仅** `Lane` + `MAX_CLIP_DEPTH`（均 `pub(crate)`；`Scene`/`LaneId` → P03）
│   ├── canvas.rs                   P02 ✅ Canvas（应用层唯一公开写入口，内部持有 scene::Lane）
│   ├── geometry.rs                 P02 ✅ GeometryId / GeometryDesc / IndexType / Topology / GeometryPipeline
│   ├── atlas.rs                    P02 ✅ AtlasKind / AtlasTile（公开数据 + UV/代际约定）
│   ├── instance.rs                 P07 ⏳ GpuPrimitive（pub(crate)）+ 静态断言 + kind/flags + 编码 + InstanceBuffer
│   └── (无 damage.rs)              ——  # damage 归 render/，见下（F-25）
└── render.rs                       P05 ⏳ crate 私有模块根（P01 §4.1 模板：`pub(crate) mod atlas;` …）
    ├── atlas.rs                    P05 ⏳ etagere 分配器封装 + 记录表 + 脏行 + generation + 单页/clear 逃逸
    └── damage.rs                   P06 ⏳ DamageAccumulator / DamageOutput / ScrollHint（纯 CPU）
```

`✅` = P02 落地；`⏳` = 规格已冻结、代码随该 Phase 落地（接手清单见 §19）。

`lib.rs` 的 P02 追加：**只有一行**

```rust
pub mod paint;
```

`paint.rs` 的 P02 内容（**只有模块清单**，无 item、无 `pub use`、无 `pub(crate) use`）：

```rust
pub mod atlas;
pub mod canvas;
pub mod color;
pub mod geometry;
pub mod primitive;

// P02 只含 Lane + MAX_CLIP_DEPTH，均为 pub(crate) → 模块对应用层不可见
pub(crate) mod scene;
```

`render.rs` 在 P05 首次出现时的形态（P01 §4.1 模板的最小实例；**P02 不创建**）：

```rust
pub(crate) mod atlas;   // P05
pub(crate) mod damage;  // P06 追加
```

### 3.2 落地顺序（裁决 ①：按「非 test 消费者」排序）

三条约束互相冲突，必须先解：

| 约束 | 内容 |
|---|---|
| C1（AGENTS §9） | 验收要求 `cargo +nightly clippy -p kytrix_gui --all-targets -- -D warnings` 通过 |
| C2（§2.4c 事实） | 没有同相非测试消费方的 `pub(crate)` item（甚至私有模块里的 `pub`）**必然**报 `dead_code`；`#[cfg(test)]` 的使用**不能**豁免 |
| C3（§1.3） | 应用层公开 API 面是一张**封闭表**；`GpuPrimitive` / `InstanceBuffer` / `DamageAccumulator` / `AtlasAllocator` 都不在表里，「超出此表即越界」 |

写 `pub(crate)` → C1 失败；提升为 `pub` → 违反 C3；加 `#[allow(dead_code)]` → AGENTS §4 明令禁止。
因此**唯一可行解是按消费者排序落地**。裁决 ① 定稿如下（P02 的完整清单见 §18.1；下表是**推迟**部分的归属与接手判据）：

| 文件 | 落地 Phase | 首个非测试消费者 | 落地时必须一并交付 |
|---|---|---|---|
| `paint/instance.rs` | **P07** | paint 阶段的 `Scene → InstanceBuffer` 编码 | `InstanceEncoder::encode()` 的调用点（帧组装）+ §14.7 单测 + §8.1 静态断言 |
| `render/damage.rs` | **P06** | paint 之后的 damage 计算（§3.5 `Paint → Damage`） | `DamageAccumulator` 的调用点（帧组装）+ §14.8 单测 |
| `render/atlas.rs` | **P05** | 字形/图片装箱（文本图集是首个消费者） | `AtlasAllocator` 的调用点（字形缓存）+ §14.9 单测 |
| `render.rs` | **P05** | 同上（第一个 render 子模块） | `pub(crate) mod atlas;`（P06 追加 `pub(crate) mod damage;`） |
| `Scene` / `LaneId` / lane 分配与绘制序 | **P03** | P03 的 paint 循环（首个持有 `Scene` 的代码） | `Scene` 只读面 + `pub(crate)` 写入面（形状见 §7.2） |

**硬规则（实现代理必须遵守）**：

1. P02 **不得**创建 `src/render.rs`、`src/render/`、`src/paint/instance.rs`、`src/paint/damage.rs`。
2. P02 的每个 `pub(crate)` item 必须在**非 test 构建**里被调用；§18 的实现清单逐文件列出消费者，没有消费者的能力一律推迟。
3. 规格冻结：§8（instance）、§10.4（图集分配器）、§12（damage）的**字段、签名、算法、常量**由本文档定死；接手 Phase 只实现，不改语义（改则必须新增偏差条目）。

**阶段编号与消费者的关系**：上表是本裁决的定稿。若后续总计划调整相位（例如文本整体后移），**以「消费者所在相」为准**——
文件跟着消费者走，而不是跟着编号走；调整时必须同步更新本表与 §19。
### 3.3 模块形态与可见性规则（硬约束）

| 规则 | 依据 |
|---|---|
| `paint.rs` 用 `pub mod <child>;`，**不写** `pub use` / `pub(crate) use` / `mod.rs` / `#[path]` | P01 §4.1、M-06、M-07、workspace lint `pub_use = "forbid"` |
| `paint.rs` 不定义任何 item（含 `const _: () = assert!(…)`） | M-03（静态断言放叶子文件）；`definition_in_module_root` 只管 `mod.rs`/`lib.rs`，但「模块根 = 清单」是本项目自定规则 |
| 每个类型只允许**一个** inherent `impl` 块 | workspace lint `multiple_inherent_impl = "forbid"` |
| 字段可见性只有「全 pub」或「全私有」两种 | workspace lint `field_scoped_visibility_modifiers = "forbid"` |
| 单测内联在被测文件末尾（`#[cfg(test)] mod tests`） | AGENTS §5.1、P01 §4.3c |
| 短字符串 `SmolStr`、字节 `Bytes`、哈希 `ahash` | AGENTS §2.2/§2.4/§2.3（本阶段只有 `Bytes` 用得上） |
| 文件行数不设上限 | A-01 |

---

## 4. 公共 / 私有边界与公开路径表增量

### 4.1 最终归属（brief 要求裁决的第一条）

| 类型 | 归属文件 | 公开路径 | 理由 |
|---|---|---|---|
| `AtlasKind` / `AtlasTile` | **`src/paint/atlas.rs`** | `kytrix_gui::paint::atlas::{AtlasKind, AtlasTile}` | 它们是 `Primitive::GlyphRun`/`Image` 的**字段类型**（I2 数据契约），必须在 `paint` 里；放 `resources.rs`（P15）会让 `paint → resources` 反向依赖，且 P15 之前就要用 |
| `ResourceId` / `ResourceCache` | `src/resources.rs`（P15） | `kytrix_gui::resources::*` | 与图集无关（P01 §4.5 原样保留） |
| `Canvas` | **`src/paint/canvas.rs`**（P02） | `kytrix_gui::paint::canvas::Canvas` | 应用层**唯一公开写入口**（裁决 ②）；内部持有 `scene::Lane`，因此也是 `Lane::new/push` 的消费者 |
| `Lane` / `MAX_CLIP_DEPTH` | `src/paint/scene.rs`（P02，**均 `pub(crate)`**） | **无公开路径**（`pub(crate) mod scene;`） | 应用层不需要 `Lane`（经 `Canvas` 间接使用）；§1.3 未列 `Lane`/`MAX_CLIP_DEPTH` → 保持 crate 私有（N7） |
| `Scene` / `LaneId` | `src/paint/scene.rs`（**P03**） | `kytrix_gui::paint::scene::{Scene, LaneId}`（P03 起） | §1.3「`Scene`（只读句柄）」，P03 才有实现与消费者（§7.2） |
| `AtlasAllocator`（装箱）/ 脏行 / generation | **`src/render/atlas.rs`**（**P05**） | crate 私有 | GPU 侧资源管理（I9）；`pub(crate)` 在 P05 才有消费者（字形装箱） |
| `GpuPrimitive` / 编码 / `InstanceBuffer` | **`src/paint/instance.rs`**（**P07**） | crate 私有（`pub(crate) mod instance;`） | 128 B 布局是**引擎内部** CPU→GPU 契约；§1.3 未列，不进应用层公开面 |
| `DamageAccumulator` / `DamageOutput` / `ScrollHint` | **`src/render/damage.rs`**（**P06**） | crate 私有 | 纯 CPU 算法 + 供 P09 做平台翻译；对应用层的可观测性经 `test_support`（P06 的 `FrameRecord.damage`） |

> **对 P01 §4.5 的修订**：原表把 `AtlasTile` 登记为 `kytrix_gui::resources::*`（`src/resources.rs`，P15）。
> 现改为 `kytrix_gui::paint::atlas::*`（`src/paint/atlas.rs`，P02）。这是一处以「数据契约归 paint」为准的路径迁移，
> `resources.rs` 在 P15 不再定义 `AtlasTile`/`AtlasKind`。→ **F-24**。

### 4.2 公开路径表增量（应用层迁移的唯一契约）

| 类型 | 公开路径 | 定义文件 | 消费 Phase |
|---|---|---|---|
| `Color` `ColorResolver` `ColorBrush` `LinearPremulRgba` `Interpolation` `HueDirection` | `kytrix_gui::paint::color::*` | `src/paint/color.rs` | P02（主题解析）/ P03（style）/ P06（text brush）/ P08（app 主题） |
| `Primitive` `Quad` `QuadPaint` `Border` `Gradient` `GradientKind` `GlyphRun` `PositionedGlyph` `Image` `Path` `Shadow` `ClipOp` `ClipRect` `GeometryDraw` | `kytrix_gui::paint::primitive::*` | `src/paint/primitive.rs` | P03（生产）/ P07（编码）/ P06（帧记录）/ P08（`Canvas` 自绘） |
| `Canvas` | `kytrix_gui::paint::canvas::*` | `src/paint/canvas.rs` | P08（自绘节点写入口）/ P07（展平编码） |
| `GeometryId` `GeometryDesc` `IndexType` `Topology` `GeometryPipeline` | `kytrix_gui::paint::geometry::*` | `src/paint/geometry.rs` | P08（`register_geometry`）/ P07 / P09 |
| `AtlasKind` `AtlasTile` | `kytrix_gui::paint::atlas::*` | `src/paint/atlas.rs` | P15（资源）/ P06（字形）/ P07 |
| `Scene` `LaneId` | `kytrix_gui::paint::scene::*`（**P03 起**） | `src/paint/scene.rs` | P06（`RenderedFrame.reused_lanes`）/ P07 / P08（`Scene` 只读句柄） |

**不进公开面**（crate 私有）：`paint::scene::{Lane, MAX_CLIP_DEPTH}`（P02；模块声明为 `pub(crate) mod scene;`）、
`paint::instance::*`（P07）、`render::*`（P05/P06，含 `render::damage`）。
`Canvas` 是**唯一**的应用层写入口（裁决 ②）：P02 里没有任何公开的可变容器；P03 落地的 `Scene` 也保持只读。

### 4.3 每个公开类型必须有真实消费方（AGENTS §1.2）

| 类型 | 消费方（Phase） |
|---|---|
| `Color` / `ColorResolver` | P03 `Style`/`StylePatch`（`background`/`foreground`/`border`）、P08 主题加载、P06 `ColorBrush` |
| `LinearPremulRgba` | P03/P07 编码（→ `GpuPrimitive.color0/1`）、P06 帧记录断言（§6.3 `assert_eq!(…resolve())`） |
| `ColorBrush` | P06 `parley::Layout<ColorBrush>`（`Brush` 是标记 trait，F-04） |
| `Primitive` + 8 类 | P03 生产者、P07 编码、P06 `RenderedFrame.primitives`、P18 像素快照 |
| `Quad`/`Shadow`/`Image` | P03（背景/边框/阴影/图片） |
| `GlyphRun`/`PositionedGlyph` | P06（shaping 输出 → paint 输入）、P07 |
| `Path` | P15（图标/图表形状的三角化）→ P07；P08（图表 widget） |
| `Scene`/`LaneId` | P03（`Node.lanes`）、P04、P07（重建/复用）、P06（`reused_lanes`）、P08（`Scene` 只读句柄） |
| `Lane` / `MAX_CLIP_DEPTH` | **P02 crate 内**：`Canvas`（`Lane::new/push/clip_depth`、`MAX_CLIP_DEPTH`）；P03 的 `Scene` 也是 `Lane` 的容器 |
| `GeometryId`/`GeometryDesc`/`IndexType`/`Topology`/`GeometryPipeline` | P08 `App::register_geometry`、P09（VBO 装载 + 校验）、P07（uniform 变换） |
| `MAX_CLIP_DEPTH`（`pub(crate) u8`） | P02 的 `Lane::push` 与 `Canvas::push_clip`（crate 内）；P07 编码器；P10 三套 shader 的数组长度镜像 |

### 4.4 §1.3 公开白名单的传递闭包扩充（N7）

§1.3 的白名单是**按类型名**列出的；`paint` 的公开契约会通过字段/参数**传递**带出下面的类型。本节把它们显式登记，
作为 §1.3 的补充（只扩充「已被现有白名单条目传递引用」的类型，不新增能力）：

| 传递新增的公开类型 | 由谁带出 | 消费 Phase |
|---|---|---|
| `Quad` `QuadPaint` `Border` `Gradient` `GradientKind` | `Primitive::Quad` 的载荷 | P03 生产 / P07 编码 / P06 帧记录 |
| `GlyphRun` `PositionedGlyph` | `Primitive::GlyphRun` 的载荷 | P06 / P07 |
| `Image` `Path` `Shadow` `ClipOp` `ClipRect` `GeometryDraw` | `Primitive` 的载荷 | P03/P07/P08 |
| `AtlasKind` `AtlasTile` | `GlyphRun`/`Image` 的字段 | P06/P15/P07 |
| `LinearPremulRgba` `Interpolation` `HueDirection` | `ColorResolver::resolve/interpolate` 的返回/入参 | P02/P03/P06/P08 |
| `IndexType` `Topology` `GeometryPipeline` | `GeometryDesc` 的字段 | P08/P09 |
| `ColorBrush` | 已列于 §1.3（`Brush`） | P06 |

**明确不扩充**：`Lane`、`MAX_CLIP_DEPTH`（`pub(crate)`，应用层不可见）、`GpuPrimitive`/`InstanceBuffer`（P07 crate 私有）、
`DamageAccumulator`/`DamageOutput`/`ScrollHint`（P06 crate 私有）、`AtlasAllocator`/`Allocation`/`DirtyRows`（P05 crate 私有）。
`Scene`/`LaneId` 在 P03 落地时按 §1.3 原有条目（`Scene`）与 §4.2（`LaneId`）登记。

---

## 5. `paint/color.rs`（D23 引擎一等颜色）

### 5.1 `Color`：5 变体 + **独立 alpha**

```rust
/// 引擎一等颜色值（D23 / I15）。所有变体都存「**分量空间 + 非预乘、独立 alpha**」。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Color {
    /// sRGB 编码分量，通常 0..=1（越界按 §5.4 的边界规则 clamp）。
    Srgb { r: f32, g: f32, b: f32, alpha: f32 },
    /// 线性 sRGB 分量，通常 0..=1。
    LinearSrgb { r: f32, g: f32, b: f32, alpha: f32 },
    /// Display P3（sRGB 传递函数、D65），通常 0..=1。
    DisplayP3 { r: f32, g: f32, b: f32, alpha: f32 },
    /// Oklab：`l` 0..=1，`a`/`b` 约 -0.4..=0.4。
    Oklab { l: f32, a: f32, b: f32, alpha: f32 },
    /// Oklch：`l` 0..=1，`c` >= 0（自然上限约 0.5），`h` 单位**度**。
    Oklch { l: f32, c: f32, h: f32, alpha: f32 },
}
```

| 决策 | 结论 | 理由 |
|---|---|---|
| 为什么不用 `color::DynamicColor` 包一层 | 不用；`Color` 是**引擎自有**枚举，`DynamicColor` 只在 `paint/color.rs` 内部出现 | I15/D23：颜色语义只在引擎实现一份；把 `ColorSpaceTag` 暴露到公开 API 等于把 `color` 的版本细节写进应用层契约（F-04 的同类判断） |
| 为什么不用 `DynamicColor` 直接当公开类型 | 同上，且 `DynamicColor` 有 `Flags`（missing/named）这套 CSS 解析状态，v1 不做 CSS 字符串解析 | 少一个必须解释的状态机 |
| alpha 为什么独立 | `Oklch`/`Oklab` 的 alpha 在 `color` 里也是独立分量；预乘会破坏「同一颜色换空间后仍是同一颜色」 | §5.8.3 明确要求「alpha 必须单独处理」 |
| `PartialEq` | 需要（§3.8 full-refresh oracle 比较 `(Primitive, order, bounds)`；P06 帧记录直接 `assert_eq!` 颜色） | 浮点相等是**契约内**的比较（同一条代码路径产出），不是容差比较 |
| 不加 `Eq`/`Hash`/`Ord` | 浮点无全序 | 同 `math.rs` |
| `Default` | `= Color::TRANSPARENT` | ①透明是绘制层的单位元（空填充/跳过判定）；②P03 的 `Style`/`Border`/`Shadow` 要能 `#[derive(Default)]`，`Color: Default` 是前置条件。`ColorBrush` **不依赖**它（默认是不透明黑，见 §5.3） |

构造器与常量（全部 `const fn` 或廉价）：

```rust
impl Color {
    pub const TRANSPARENT: Self;   // Srgb { 0, 0, 0, 0 }
    pub const BLACK: Self;         // Srgb { 0, 0, 0, 1 }
    pub const WHITE: Self;         // Srgb { 1, 1, 1, 1 }

    pub const fn srgb(r: f32, g: f32, b: f32, alpha: f32) -> Self;
    pub const fn linear_srgb(r: f32, g: f32, b: f32, alpha: f32) -> Self;
    pub const fn display_p3(r: f32, g: f32, b: f32, alpha: f32) -> Self;
    pub const fn oklab(l: f32, a: f32, b: f32, alpha: f32) -> Self;
    pub const fn oklch(l: f32, c: f32, h: f32, alpha: f32) -> Self;

    /// 主题 / 资源 / 设计稿最常见的入口：8 位 sRGB（`alpha` 也是 8 位）。
    pub fn from_rgba8(rgba: [u8; 4]) -> Self;
}

impl Default for Color { fn default() -> Self { Self::TRANSPARENT } }
```

**不做**（明确记为 v1 范围，S-13）：

| 不做 | 理由 |
|---|---|
| `from_hex` / `parse` | P02 无消费方：应用层主题若用十六进制字符串，直接调 `color::parse_color`（`color` 已是引擎依赖，应用层同仓可用），不经过引擎 `Color`；引擎侧不引入 CSS 字符串解析（§5.8.3 的 `oklch(from …)` 属 CSS Color 5，明确不进 v1） |
| `none` 缺失分量 / powerless hue | 同上（无 CSS 解析 → 无缺失分量）；`Oklch{ c: 0.0 }` 的 hue 是 powerless，但我们的 `Color` 允许它存在且不额外标记，`resolve` 对它无歧义 |
| `Hwb` / `Hsl` / `Lab` / `Lch` / `Rec2020` / `AcesCg` 变体 | 无消费方（主题是 OKLCH/OKLab/sRGB/P3；HDR 不进 v1）。`color` 支持不代表引擎要暴露；后续需要时**additive** 加变体 |
| `with_alpha` | 无消费方。`opacity`（`StylePatch.opacity`）在**预乘后**整体缩放（`LinearPremulRgba * f32`），比改 alpha 更正确（不透明色 ×0.5 = 半透明同色） |

### 5.2 `LinearPremulRgba`：唯一的「到 GPU」表示

```rust
/// 线性 sRGB + **预乘 alpha**。`components == [r*a, g*a, b*a, a]`，直接写进 `GpuPrimitive.color0/color1`。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LinearPremulRgba { pub components: [f32; 4] }

impl LinearPremulRgba {
    pub const TRANSPARENT: Self;                                  // [0.0; 4]
    pub const fn from_components(components: [f32; 4]) -> Self;
    pub const fn to_array(self) -> [f32; 4];
    /// `a >= 1.0`。`resolve` 已把 alpha clamp 到 [0,1]，故不设 epsilon。
    pub fn is_opaque(self) -> bool;
    /// `a <= 0.0`；编码器据此**跳过**不产生像素的图元（§8.6）。
    pub fn is_transparent(self) -> bool;
}

impl std::ops::Mul<f32> for LinearPremulRgba { type Output = Self; }   // 预乘空间的 opacity 缩放
```

`Mul<f32>` 的消费方：P03/P07 应用 `StylePatch.opacity`（`opacity` 对预乘色是**整体相乘**，与 CSS `opacity` 的
「先合成再整体透明」在单图元上等价）。**不实现** `Mul<Self>` / `Add`（无消费方）。

### 5.3 `ColorResolver` 与插值策略

```rust
/// CSS Color 4 的 4 种色相方向，与 `color::HueDirection` 同名同义（不把外部类型放进公开 API）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HueDirection { #[default] Shorter, Longer, Increasing, Decreasing }

/// 插值空间。**必须显式记录**（§5.8.3）：OKLab 渐变与 linear sRGB 渐变视觉不同。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Interpolation {
    /// OKLab（线性 Lab，无 hue 环绕问题）。v1 默认。
    #[default]
    Oklab,
    /// OKLCH（柱坐标，hue 按 `HueDirection` 环绕）。
    Oklch(HueDirection),
    /// 线性 sRGB（与 shader 内双色渐变同空间）。
    LinearSrgb,
}

/// 主题策略对象：持有插值空间；只做「值 → 值」，不持有缓存、不持有主题语义（D23）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ColorResolver { interpolation: Interpolation }

impl ColorResolver {
    pub const fn new(interpolation: Interpolation) -> Self;
    /// 解析成 GPU 消费的线性预乘 RGBA（§5.4 的算法）。
    pub fn resolve(&self, color: Color) -> LinearPremulRgba;
    /// 在 `self.interpolation` 空间、**预乘 alpha** 下插值，返回线性预乘结果。
    pub fn interpolate(&self, from: Color, to: Color, t: f32) -> LinearPremulRgba;
}
```

`ColorBrush`（P01 §4.5 已登记，P06 消费）：

```rust
/// `parley::style::Brush` 是**标记 trait**（`Clone + PartialEq + Default + Debug`，blanket impl，F-04）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorBrush { pub color: Color }

/// 默认画刷 = 不透明黑（文本的经典默认）；与 `Color::default()`（透明，绘制单位元）刻意不同。
impl Default for ColorBrush { fn default() -> Self { Self { color: Color::BLACK } } }

impl From<Color> for ColorBrush { fn from(color: Color) -> Self; }
```

### 5.4 `resolve` 算法（自建 sRGB gamut mapping，**逐字实现**）

`color 0.3.3` 的 `clip()` 只 clamp 到**源空间**的自然色域，不能把 `Oklch{ c: 0.35 }` 映射进 sRGB。
CSS Color 4 §13.2 的做法（chroma 归约到色域边界）必须自建。v1 定案如下（**定长、确定性、可单测**）：

```text
resolve(color) -> LinearPremulRgba:
  1. alpha = clamp_01(color.alpha)                    // N4：NaN → 0，然后 clamp 到 [0, 1]（见下）
  2. 按变体分两族：
     (A) Srgb / LinearSrgb —— **已在 sRGB 色域内**：
         转 LinearSrgb（Srgb 走 `ColorSpaceTag::Srgb → LinearSrgb`，即 sRGB EOTF 的精确逆）
         逐分量 clamp 到 [0, 1]                          // 越界分量是「调用方给了域外值」，不是宽色域源
     (B) DisplayP3 / Oklab / Oklch —— **可能是宽色域**：
         a. d = DynamicColor { cs: <源空间 tag>, flags: Flags::default(), components: [c0, c1, c2, alpha] }
         b. o = d.convert(ColorSpaceTag::Oklch)          // 统一到柱坐标做 chroma 归约
         c. L = clamp_01(o.components[0])；C = clamp_nonneg(o.components[1])；h = hue_deg(o.components[2])
         d. if in_srgb_gamut(L, C, h) → lin = oklch_to_linear_srgb(L, C, h)
            else 二分（**固定 24 次，不提前退出**）：
              lo = 0.0；hi = C；best = 0.0
              for _ in 0..24 {
                  mid = 0.5 * (lo + hi)
                  if in_srgb_gamut(L, mid, h) { lo = mid；best = mid } else { hi = mid }
              }
              lin = oklch_to_linear_srgb(L, best, h)
         e. lin 逐分量 clamp 到 [0, 1]                    // 收掉二分停在边界时的 ±1e-6 溢出
  3. 返回 LinearPremulRgba { [lin.r * alpha, lin.g * alpha, lin.b * alpha, alpha] }
```

辅助定义（同样是契约的一部分）：

```text
in_srgb_gamut(L, C, h) -> bool:
    let lin = oklch_to_linear_srgb(L, C, h);
    lin 的每个分量有限且 ∈ [-1e-6, 1.0 + 1e-6]

oklch_to_linear_srgb(L, C, h) -> [f32; 3]:
    DynamicColor { cs: Oklch, flags: Flags::default(), components: [L, C, h, 1.0] }
        .convert(ColorSpaceTag::LinearSrgb)
        .components[0..3]

clamp_01(v)     = if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) }      // N4：每个分量都先 NaN→0
clamp_nonneg(v) = if v.is_nan() { 0.0 } else { v.max(0.0) }             // C 的定义域是 [0, +inf)
hue_deg(v)      = if v.is_nan() { 0.0 } else { v.rem_euclid(360.0) }    // h 先 rem_euclid(360) 再进二分/转换
```

| 决策 | 结论与理由 |
|---|---|
| 为什么 24 次定长二分 | **同一平台逐位可复现**；**跨平台允许 ULP 级差异**（`sin_cos`/`cbrt` 的 libm 实现不同），像素 golden 按 **8 位**比较（N5）。`C` 最大约 0.5，24 次后分辨率 ≈ 3e-8，远小于 8 位色阶（1/255 ≈ 4e-3）；定长循环保证同平台同输入走出同一条路径 |
| 为什么先 clamp L | `L < 0` / `L > 1` 不在 Oklch 自然色域内，对它们做 chroma 归约没有意义（CSS 也是先修 L 再归约 C） |
| 为什么不用 `DynamicColor::clip()` | 它只 clamp 分量（对 Oklch 就是 clamp L 和 C 到自然范围），不改变「色相 + 亮度」下的可表示性，越界 C 依旧在转 sRGB 后溢出 |
| 为什么不用 `MapLightness` 等 | `map_*` 是**语义变换**工具（改主题明度），不是 gamut mapping |
| `DisplayP3` 源也走 chroma 二分 | CSS 对任何超出 sRGB 的源色都走同一算法；P3 屏幕预留（§5.8.4「不进 v1 默认路径」）只影响**输出**编码，不影响解析 |
| `Srgb`/`LinearSrgb` 源只 clamp | 它们**声明**在 sRGB 空间，域外分量是调用方错误（例如 `1.2`），chroma 归约在此无意义；clamp 是 CSS 序列化的同一语义 |
| alpha 的 NaN | `f32::clamp` 对 NaN 返回 NaN，会污染整个图元（`is_transparent`/`is_opaque` 都判错）→ 显式把 NaN 归 0 |

### 5.5 `interpolate` 算法（预乘插值、空间显式）

```text
interpolate(from, to, t):
  1. t' = clamp_01(t)                                   // N4/N5：NaN → 0，clamp 到 [0, 1]；不外插（v1 无消费方）
  2. a = normalized_dynamic(from)                       // 与 resolve 共用：alpha clamp、Oklch 的 L/C/h 归一到定义域
     b = normalized_dynamic(to)
  3. it = a.interpolate(b, tag(self.interpolation), hue_direction(self.interpolation))
       // color.rs:365：内部按 CSS §12.3 **预乘**通道，并对 missing 分量做配对处理（v1 无 missing 分量）
  4. mid = it.eval(t')                                  // dynamic.rs:609：返回插值空间、非预乘
  5. 把 mid **无条件**走「(B) 宽色域源」路径：转 Oklch → gamut map → premultiply
       // 关键：即使 self.interpolation == LinearSrgb 也**不**复用 (A) 的逐通道 clamp——
       // 否则 P3 端点会与 resolve(from) 不一致（B1 回归，见本节末行）
```

| 决策 | 结论与理由 |
|---|---|
| 插值空间 vs alpha 存储分离 | `Color` 存**非预乘 + 独立 alpha**；插值在**预乘**空间（CSS §12.3，避免透明色把颜色信息漏进结果）；输出**预乘**。三者是不同的三件事，文档与类型名必须分开（§5.8.3 的「alpha 必须单独处理」） |
| 为什么 `interpolate` 也做 gamut mapping | 插值结果可能落在色域外（P3 端点 → sRGB；OKLCH 高 C）；不映射会产生负分量，进 shader 后经预乘混合变成黑边 |
| **端点不变量（B1）** | `interpolate(a, b, 0.0) == resolve(a)` 且 `interpolate(a, b, 1.0) == resolve(b)`，对**所有**插值空间与**所有**端点色域成立。实现要件：①端点归一化共用 `normalized_dynamic`；②`mid` 一律走 (B) 路径（`gamut_mapped_linear`），**包括** `Interpolation::LinearSrgb`。代码审查发现的初版实现在 `LinearSrgb` 空间对 `mid` 用了 (A) 的逐通道 clamp，导致 P3 端点 t=0 得 `[0, 0.8201, 0]`（应 `[0, 0.73798, 0.08567]`）——已修，并由 `interpolate_endpoints_match_resolve` / `linear_srgb_space_matches_resolve_for_p3_endpoint` 两个用例守住 |
| `interpolate` 与**渐变**的关系 | v1 **不**用它生成渐变：`GpuPrimitive` 只有两个颜色槽，shader 在 **linear premultiplied** 空间插值（§8.4）。`interpolate` 只服务 CPU 侧求值场景（P13 动画淡入淡出、P03 hover 过渡、主题色混合）。两者语义不同**且都已写进 contract**（§8.4 备注），禁止互相替代 → S-12 |
| 为什么不做 `gradient_lut` | v1 shader 只需双色渐变；LUT 需要「渐变纹理」= 新 `AtlasKind` + 新 `kind` 值 + `texture_index` 语义扩展 = **契约变更**（I2/I8 要求三 platform shader 同步改）。触发条件与扩展路径见 §16 R3 / F-32 |

### 5.6 与 shader 的接口（contract 条款，P10 镜像）

1. `color0` / `color1` 一律是 **linear sRGB + premultiplied alpha**；shader **不做**任何 sRGB→linear 或预乘变换
   （唯一例外：图集采样，见 §10.7）。
2. 纯色：`color1 = [0,0,0,0]`；渐变：两端都是预乘色，`t` 在 linear 空间线性插值（**不是** OKLab）。
3. alpha 混合固定为 `src + dst * (1 - src.a)`（premultiplied over），三平台 blend state 写死同一公式（§5.8.4）。
4. `ColorResolver` 的所有输出都在 `[0, 1]`（含 alpha），shader 可对颜色做无边界检查的运算。

### 5.7 依赖隔离（I15 的可验证形式）

- `use color::…` 在整个 crate 里**只允许出现在 `src/paint/color.rs`**。
- 验收手段：`Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs | Select-String -CaseSensitive -Pattern 'use color::'`
  只应命中 `paint/color.rs`（外加 `Cargo.toml`）。任何第二个文件 `use color::` 都是 I15 违反。
  **必须大小写敏感**：`Color::WHITE`（我们的类型）与 `paint::color::Color`（我们自己的模块路径）都会在不敏感匹配下误命中。

---

## 6. `paint/primitive.rs`（8 类 Primitive）

### 6.1 坐标空间与单位（I11 的前半段）

| 项 | 契约 |
|---|---|
| 坐标系 | **绝对窗口逻辑坐标**，左上原点、y 向下（与 `math.rs` 一致） |
| 单位 | 全部 `f32` **逻辑像素**；没有任何图元携带自己的物理像素字段 |
| 矩形 | `math::Rect`，半开区间 `[min, max)`；空矩形不绘制、不参与 damage（P01 §6.3.2） |
| 变换 | `Quad`/`GlyphRun`/`Image`/`Shadow`/`Path`/`Clip` **无变换栈**，坐标已由 paint 阶段展开为绝对值。唯一例外是 `GeometryDraw.transform`（uniform 变换，§9.6） |
| 物理像素 | 只在 `paint/instance.rs` 编码时出现（P01 §7.1 的三种换算），`Primitive` 本身零物理像素 |

> 为什么不做 per-primitive transform：设计 §5.3/§5.4 的 uber-quad 只有 `bounds`，没有矩阵槽；128 B 也没有空间。
> 变换在 paint 阶段（P07）随 clip 一起展开，这让 damage（§12）能在逻辑空间直接取 AABB。

### 6.2 8 类总表（对齐主设计附录 C）

```rust
/// 一帧绘制指令的最小单位（I2）。顺序即绘制序（后画的覆盖先画的）。
#[derive(Clone, Debug, PartialEq)]
pub enum Primitive {
    Quad(Quad),             // 1
    GlyphRun(GlyphRun),     // 2
    Image(Image),           // 3
    Path(Path),             // 4
    Shadow(Shadow),         // 5
    Clip(ClipOp),           // 6
    Geometry(GeometryDraw), // 7
    Canvas(Canvas),         // 8
}
```

| # | 变体 | 目标管线 | 产生 `GpuPrimitive` | `kind` | 消费 Phase |
|---|---|---|---|---|---|
| 1 | `Quad` | uber-quad | 1 | `0` | P03 / P07 |
| 2 | `GlyphRun` | uber-quad（图集采样） | **N（每字形 1 个）** | `1` | P06 → P07 |
| 3 | `Image` | uber-quad（图集采样） | 1 | `2` | P15 → P07 |
| 4 | `Path` | geometry（静态 VBO） | 0（1 次几何绘制） | — | P15/P08 → P07 |
| 5 | `Shadow` | uber-quad（解析阴影） | 1 | `3` | P03 / P07 |
| 6 | `Clip` | 无（栈指令，影响后续所有实例） | 0 | — | P07 |
| 7 | `Geometry` | geometry（静态 VBO） | 0（1 次几何绘制） | — | P08 → P07/P09 |
| 8 | `Canvas` | 展平为子图元 | Σ子图元 | — | P08（写入口）→ P07 |

**1→N 的判定依据**：`GpuPrimitive` 只有一个 `uv`，一个字形一张图集矩形 ⇒ **每个字形必须一个实例**。
`GlyphRun` 不做「一次 draw + 逐顶点 UV」：那要求把字形 quad 顶点上传（每帧 N×4 顶点），正好是 §5.5 要消除的
「按顶点收框架费」。代价是实例数 = 字形数（一屏 5000 字形 = 5000 × 128 B = 640 KB；`InstanceBuffer` 按上限一次分配，
满足 §9.3 的「不出现 2^n 跨档翻倍」）。

**边框/阴影/路径的拆法**：边框**不拆**（同实例的 `params0.x` + `color1`，§8.5）；阴影**拆成独立 `Shadow`**（P03 在同一
lane 里**先 push `Shadow` 再 push `Quad`**，后者覆盖前者 —— 这就是「Shadow 与 Quad 的重叠语义」，顺序是契约的一部分）；
路径**拆到 geometry 通道**（0 实例）。

### 6.3 逐类字段表

#### 6.3.1 `Quad`（背景 / 边框 / 渐变）

```rust
#[derive(Clone, Debug, PartialEq)]
pub struct Quad {
    /// 逻辑像素矩形；圆角与边框都在此矩形内。
    pub bounds: Rect,
    /// 四角半径，CSS 顺序 `[tl, tr, br, bl]`，逻辑像素。
    pub corner_radius: [f32; 4],
    pub paint: QuadPaint,
}

/// 着色模式。三选一由**类型**保证（不是两个 `Option` 的运行时不变量）。
#[derive(Clone, Debug, PartialEq)]
pub enum QuadPaint {
    /// 实色填充，可带内侧边框。
    Solid { fill: Color, border: Option<Border> },
    /// 双色渐变。v1 **不允许**同时带边框（`color1` 只有一个槽，§8.5）。
    Gradient(Gradient),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Border {
    /// 边框宽度（逻辑像素），沿 `bounds` **内侧**展开（CSS `border-box` 行为，不做 outset）。
    pub width: f32,
    pub color: Color,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gradient {
    pub from: Color,
    pub to: Color,
    pub kind: GradientKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GradientKind {
    /// 沿 `bounds` 的 +x（左 → 右）。
    Horizontal,
    /// 沿 `bounds` 的 +y（上 → 下）。
    Vertical,
    /// 圆心 = `bounds` 中心，半径 = `min(w, h) / 2`。
    Radial,
}
```

| 语义 | 契约 |
|---|---|
| 空/透明 | `Solid { fill: TRANSPARENT, border: None }` 不产生实例（编码器跳过，§8.6）；`Solid { fill: TRANSPARENT, border: Some(..) }` 正常产生（只画边框） |
| 圆角半径 | 逐角独立；编码器把每角 **clamp 到 `min(w, h) / 2`**（物理像素域、`round` 前 clamp），避免 SDF 出现负半径 |
| 边框宽度 | 编码器 clamp 到 `min(bounds.w, bounds.h) / 2`；`width <= 0` 视为无边框 |
| 渐变径向 | 用 `bounds` 推导圆心/半径，**不需要额外字段**；椭圆/带角度的渐变超出 128 B 表达能力 → 延后（F-32） |
| 渐变端点 | `from`/`to` 各自 `resolve`（§5.4）后进 `color0`/`color1`；shader 在 **linear premultiplied** 空间插值（§5.6） |

#### 6.3.2 `GlyphRun` / `PositionedGlyph`（P06 的输出交接形状）

```rust
#[derive(Clone, Debug, PartialEq)]
pub struct GlyphRun {
    /// 整段文本的前景色（未预乘 `Color`）。
    pub color: Color,
    /// 已定位的字形，顺序 = 绘制顺序。
    pub glyphs: Vec<PositionedGlyph>,
}

/// 一个字形在图集里的矩形 + 它在逻辑空间的落点。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionedGlyph {
    pub tile: AtlasTile,
    /// 字形 quad 左上角（逻辑像素、**绝对**坐标，已含 run 偏移与基线）。
    pub position: Point,
    /// 字形 quad 的逻辑尺寸。契约：`scale * size` 必须是**整数**（P06 按设备 scale 光栅化，§7.1 `to_physical` 1:1 采样）。
    pub size: Size,
}
```

| 决策 | 结论与理由 |
|---|---|
| 为什么 `Vec<PositionedGlyph>` 而不是 `Bytes` | I12：CPU 帧记录必须能**直接**断言字形；`Bytes` 需要二次解码且类型不安全。lane 复用意味着只在文本/主题变化时重建，内存不是瓶颈（`PositionedGlyph` 实测 40 B，5000 字形 ≈ 200 KB/lane） |
| 为什么不是并行数组（`tiles` + `positions`） | 违反「一个结构一个语义」，长度一致性只能靠运行时不变量 |
| 为什么每字形带 `AtlasTile`（含 `generation`） | 字形可能落在不同图集/不同 generation；`generation` 是 P06 缓存校验与 §3.8「图集重排 → PAINT」的输入 |
| 为什么不带 `GlyphId` | 光栅化之后 glyph id 已无用（图集矩形是唯一身份）；保留就是未使用字段（AGENTS §2.1） |
| 对齐/字距 | 已烘进 `position`；`GlyphRun` 不保存 advance（P06 的 `ShapedLine` 负责） |

#### 6.3.3 `Image`（图集图片 + 调制色）

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Image {
    pub bounds: Rect,
    pub tile: AtlasTile,
    /// 调制色：`texel * modulation`（预乘空间）。`Color::WHITE` = 不调制；`alpha = 0` 的图元被跳过。
    pub modulation: Color,
}
```

`modulation` 的三条语义：①只做**乘法**（不做替换/混合模式）；②在**预乘空间**相乘（因此 `WHITE` 是单位元，
`Srgb{1,1,1,0.5}` 是 50% 淡出）；③图标着色（tint）只能整体乘，不能「保留形状换色」—— 换色方案（`R8` 形状图集 vs
预生成彩色图集）属 P15，不入 P02。

#### 6.3.4 `Path`（引擎三角化的矢量填充）

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Path {
    /// 逻辑像素 AABB（damage / 命中 / 图元级裁剪判断）。
    pub bounds: Rect,
    /// 已注册的三角化网格（`GeometryPipeline::Ui` 布局 + 恒等变换）。
    pub geometry: GeometryId,
    /// 实色填充（v1 只有实色；渐变/描边用 `Quad`/多次填充表达）。
    pub fill: Color,
}
```

| 决策 | 结论与理由 |
|---|---|
| 三角形放哪 | **不进 `Primitive`**。三角化结果在装载期注册成静态几何（`GeometryDesc`，§9），`Path` 只持 `GeometryId`。理由：§5.5 的「最大收益项」就是把顶点从每帧上传里彻底移除；`Path` 里塞 `Bytes` 会让每次 lane 重建都拷一份三角形 |
| 两阶段 coverage | **不进数据契约**（附录 C 的 `coverage_pass` 字段删除，F-26）。pass 结构是渲染器状态：同一份三角形在一个 `Path` 上永远成对出现（mask → composite），放进每帧数据会 ①让一个逻辑绘制变成两个 `Primitive`，②破坏 `REUSABLE` lane 的整桶复用（两半永远不能独立失效），③污染 `PartialEq` 比较（§3.8 oracle） |
| v1 的 AA | **不做**。①小尺寸矢量（54 个图标）在**构建期**转成预生成图集走 `Image`（§5.6 明确），本来就有 AA；②`Path` 的剩余用途是大面积图表填充（K 线实体、面积图），边缘与小字号可读性无关；③真正的两阶段 coverage（超采样 mask + 合成）是 P09/P10 的渲染器工程量，且要占 `kind = 4`（§8.3），不能靠猜数据字段提前锁定 → 风险 R2 |
| 与 `Geometry` 的区别 | `Path` = 恒等变换 + 引擎解析的 `Color` 填充 + 资产缓存生命周期；`Geometry` = 每帧 uniform 变换 + 顶点自带语义。两者 GPU 管线**相同**（`GeometryPipeline::Ui`），差异在生命周期与着色来源，因此保留两个变体（§5.1 的 8 类要求） |
| 描边 | 不做 `stroke` 字段：描边在 CPU 侧三角化成填充（lyon `StrokeTessellator`），结果同样是 `Path` 或 `Geometry`（P15 决定） |

#### 6.3.5 `Shadow`（解析阴影）

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shadow {
    /// 投影盒：**未偏移、未模糊**的矩形（逻辑像素）。
    pub rect: Rect,
    /// 偏移（逻辑像素）。
    pub offset: Point,
    /// 高斯模糊半径（逻辑像素，`>= 0`；erf 近似的 σ 由 shader 按 `params0.y` 推导）。
    pub blur: f32,
    pub color: Color,
}
```

| 语义 | 契约 |
|---|---|
| 与 `Quad` 的关系 | 独立图元；P03 必须**先 push `Shadow` 再 push `Quad`**（同一 lane），这是「阴影在内容之下」的唯一表达方式（无 z 字段） |
| 与 `Quad` 的重叠 | 阴影盒可以大于、小于或完全覆盖 `Quad`；渲染器不关心（两次 SDF 求值 + premultiplied over） |
| 无 `spread` | 无消费方（`base_ui` 的 box-shadow 只有 offset/blur/color）；需要时 additive 加字段并改 shader 参数 |
| `blur = 0` | 合法：退化成硬边偏移矩形，`aa_padding = 0.0 + 1.0` |
| 偏移的归属 | `offset` **不进 GPU**：编码时把 `rect.translate(offset)` 作为 `bounds`（§8.4）。`offset` 保留在 `Primitive` 里供 damage 取「内容盒 ∪ 阴影盒」的并集（§12.3） |

#### 6.3.6 `Clip`（clip 栈指令）

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClipOp {
    /// 压栈：后续图元被 `rect`（含四角圆角）裁剪。子图元的**绝对坐标不变**，裁的是可见区域。
    Push(ClipRect),
    /// 弹栈：恢复上一层。
    Pop,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipRect {
    pub rect: Rect,
    /// 四角半径 `[tl, tr, br, bl]`，逻辑像素；全 0 = 直角 clip。
    pub corner_radius: [f32; 4],
}
```

**为什么是 push/pop 指令而不是「每图元带 clip id」**：

| 判据 | push/pop（选定） | 每图元带 id |
|---|---|---|
| I2「一套契约」 | clip 只有一处表示 | 需要额外 `ClipChain` 表 + `Option<ClipChainId>` 字段（两处表示） |
| I12 CPU 帧记录 | clip 指令按顺序出现在 `Scene` 里，可直接断言 | 需要额外读表才能还原绘制时的裁剪状态 |
| lane 复用 | 相对栈：lane 内部 push/pop 自成闭环，复用时语义不变 | 绝对 id：祖先 clip 变化后复用的 lane 会持过期 id |
| 命中测试（§4.4） | 逆序遍历时 Pop→push 即可重建裁剪栈 | 直接可读 |
| 代价 | 编码器必须维护栈；lane 必须 clip-balanced（§7.4 强制） | 每图元 +4 B，`Scene` 多一张表 |

结论：**push/pop**，并把「lane 必须 clip-balanced」写成硬不变量（§7.4）。

#### 6.3.7 `GeometryDraw`（静态几何的每帧部分）

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeometryDraw {
    pub id: GeometryId,
    /// 每帧 uniform 变换（只支持 translate/scale 复合，P01 S-07）。
    pub transform: Transform,
}
```

**与附录 C 的差异**：附录 C 列 `{ geometry_id, transform, pipeline, damage }`。本设计**删除** `pipeline` 与 `damage`（F-27）：

- `pipeline` 是 `GeometryDesc` 的字段（注册期不可变，§9.2）——在每帧图元里再存一份就是两个真值来源（I2）。
- `damage` 不是存储字段：几何的 damage = `covering(desc.bounds 经 transform 的 AABB)` 再按物理像素外扩，
  由 P07 在累加时**计算**（§12.3）。图元里存 damage 会让「同一几何被两个节点引用」时分不清归属，
  也会让 `REUSABLE` lane 复用拿到过期 damage。

#### 6.3.8 `Canvas`（逃生口 = 应用层唯一公开写入口）

见 §11。`Primitive::Canvas(Canvas)` 在编码时**按顺序展平**为子图元，本身不产生实例、不产生 draw call。
`Canvas` 内部持有 `scene::Lane`（即「Scene 的 lane 数据结构」），因此它同时是 `Lane::new` / `Lane::push` 的
**非 test 消费者**（裁决 ②，S-15）。**嵌套深度 ≤ 1** 由类型/API 保证：`Canvas` 没有任何接受 `Canvas` 的 push 方法，
编码器在展平时 `assert!(depth == 0)`（§8.6）。

### 6.4 `Primitive` 的尺寸与内存

| 项 | 结论 |
|---|---|
| `size_of::<Primitive>()` | 由最大变体决定，**实测 80 B**（最大变体 `Quad` = 16 + 16 + `QuadPaint`48；见 §2.5 的 probe 输出）。workspace `large_enum_variant = "allow"`，不装箱 |
| 内存量级 | 一屏 5000 图元 ≈ 400 KB（lane 持久，不每帧重建）；`GlyphRun` 的 `Vec` 另计 40 B/字形 |
| 不装箱的理由 | `Primitive` 在 lane 里按顺序密集遍历（编码 / damage / 帧记录三次扫描），`Box` 会变成 5000 次指针追逐 |
| 守护 | §14 的 `primitive_size_budget` 单测**记录** `size_of`（不做编译期硬断言：变体增删是正常演进；但任何让 `Primitive` 超过 128 B 的改动必须在评审里说明） |
| `Clone` 的必要性 | ①§3.8 full-refresh oracle 复制整帧；②P06 `RenderedFrame` 持有 `Vec<Primitive>` 快照。只发生在测试/帧记录路径 |

### 6.5 与主设计附录 C 的逐字段对齐

| 附录 C 字段 | 本设计 | 说明 |
|---|---|---|
| `Quad{ bounds, radii, color, border, gradient, clip_chain }` | `Quad{ bounds, corner_radius, paint: Solid{fill,border}/Gradient }` | `color/border/gradient` 合并为枚举（互斥由类型表达）；`clip_chain` 删除（clip 是栈指令） |
| `GlyphRun{ glyph_ids, positions, atlas_tile, color, clip_chain }` | `GlyphRun{ color, glyphs: Vec<PositionedGlyph> }` | `glyph_ids` 删除（光栅后无用）；`positions`+`atlas_tile` 合并为 `PositionedGlyph`；`clip_chain` 删除 |
| `Image{ atlas_tile, bounds, color_modulation, clip_chain }` | `Image{ bounds, tile, modulation }` | 同上 |
| `Path{ triangles, coverage_pass, fill, clip_chain }` | `Path{ bounds, geometry, fill }` | 三角形 → `GeometryId`；`coverage_pass` 删除（F-26）；`bounds` 新增（damage 必需）；`clip_chain` 删除 |
| `Shadow{ bounds, blur, offset, color, clip_chain }` | `Shadow{ rect, offset, blur, color }` | `bounds` → `rect`（未偏移的盒；偏移进 GPU 的 `bounds`） |
| `Clip{ op(push/pop), rect, radii }` | `ClipOp::{Push(ClipRect{rect, corner_radius}), Pop}` | 字段名统一为 `corner_radius` |
| `Geometry{ geometry_id, transform, pipeline, damage }` | `GeometryDraw{ id, transform }` | 删 `pipeline`/`damage`（F-27） |
| `Canvas{ custom_draw{pipeline, buffers, uniform, damage} }` | `Canvas{ bounds: Rect, lane: Lane }` + 公开 `push_*` | v1 只记录引擎图元；`Canvas` 是应用层写入口（F-29/F-33，§11） |

---

## 7. `paint/scene.rs`（P02：只有 `Lane` + `MAX_CLIP_DEPTH`；`Scene` 推迟 P03）

> **B1/B2/B3/N11 裁决**：`Scene` / `LaneId` / lane 分配 / 绘制序**整体推迟到 P03**（P02 没有非 test 消费者）。
> P02 的 `paint/scene.rs` 只有两个 `pub(crate)` item：`Lane` 与 `MAX_CLIP_DEPTH`；`paint.rs` 里是 `pub(crate) mod scene;`
> （模块无公开 item，因此对应用层完全不可见）。

### 7.1 `Lane`（P02 唯一的数据结构，`pub(crate)`）

```rust
/// 一个绘制输出桶（§5.3）。内容按绘制序排列；`clip_depth` 是 C2/C3 的**增量校验状态**。
/// **P02 只在 crate 内可见**：应用层通过 `Canvas` 间接构造与读取（§11）。
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Lane {
    primitives: Vec<Primitive>,
    clip_depth: u8,
}

impl Lane {
    /// 空 lane。消费者：`Canvas::new`。
    pub(crate) fn new() -> Self;
    /// 唯一的压入点：更新 `clip_depth` 并做 C2/C3 的 **debug 断言**。消费者：`Canvas::push_*`。
    pub(crate) fn push(&mut self, primitive: Primitive);
    /// 当前 clip 嵌套深度。消费者：`Lane::push`（内部）、`Canvas::push_clip/pop_clip`（N6）。
    pub(crate) fn clip_depth(&self) -> u8;
    /// 只读切片。消费者：`Canvas::primitives`。
    pub(crate) fn primitives(&self) -> &[Primitive];
    /// 消费者：`Canvas::is_empty`。
    pub(crate) fn is_empty(&self) -> bool;
}

/// clip 栈深度上限：**类型是 `u8`**，值 8。
/// 三处必须一致：`Lane::push` 的 debug 断言、`Canvas::push_clip` 的 `bool` 门限、P10 三套 shader 的固定数组长度。
pub(crate) const MAX_CLIP_DEPTH: u8 = 8;
```

`Lane::push` 的精确行为（P02 实现，逐字）：

```text
match &primitive {
  Primitive::Clip(ClipOp::Push(_)) => {
      debug_assert!(usize::from(self.clip_depth()) < usize::from(MAX_CLIP_DEPTH));   // C3
      self.clip_depth = self.clip_depth.saturating_add(1);
  }
  Primitive::Clip(ClipOp::Pop) => {
      debug_assert!(usize::from(self.clip_depth()) > 0);                              // C2
      self.clip_depth = self.clip_depth.saturating_sub(1);
  }
  _ => {}
}
self.primitives.push(primitive);
```

> `usize::from(...)` 是刻意的：它让 `u8` → `usize` 的扩大转换显式可见（clippy `cast_lossless`），
> 同时保证 `clip_depth()` 与 `MAX_CLIP_DEPTH` 在 `push` 内被真实读取（因此这两个 item 在 P02 有**非 test 消费者**）。
> `saturating_*` 让 release 构建即使在断言被编译掉时也不会回绕。

### 7.2 为什么 `Scene` / `LaneId` 不在 P02（消费者分析）

| 能力 | P02 消费者 | 结论 |
|------|-----------|------|
| `Lane`（new/push/clip_depth/primitives/is_empty） | `Canvas`（`paint/canvas.rs`） | **P02 落地**（`pub(crate)`） |
| `MAX_CLIP_DEPTH: u8` | `Lane::push`、`Canvas::push_clip` | **P02 落地**（`pub(crate)`） |
| `Scene`（只读面：`lane`/`order`/`iter`/…） | 无（P02 没有人持有 `Scene`；P06 的帧记录、P07 的编码都晚于 P03） | **推迟 P03** |
| `LaneId`（+代际） | 无（`Canvas` 只有一条内部 lane，不需要 id） | **推迟 P03** |
| `Scene::new_lane`/`release_lane`/`lane_mut`/`clear_frame`/`set_order` | 无（P03 的 paint 循环） | **推迟 P03** |

**P03 落地时的形状（已冻结，P03 不得改语义）**：

```rust
// P03 追加到 paint/scene.rs（本文件已存在，只追加、不改 Lane）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LaneId { index: u32, generation: u32 }

#[derive(Debug, Default)]
pub struct Scene { lanes: Vec<LaneSlot>, order: Vec<LaneId>, free: Vec<u32> }

impl Scene {
    pub fn new() -> Self;
    pub fn lane(&self, id: LaneId) -> Option<&Lane>;
    pub fn lane_count(&self) -> usize;
    pub fn order(&self) -> &[LaneId];
    pub fn iter(&self) -> impl Iterator<Item = &Primitive> + Clone + '_;
    pub fn primitive_count(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    // 以下 pub(crate)，消费者 = P03 的 paint 循环
    pub(crate) fn new_lane(&mut self) -> LaneId;
    pub(crate) fn release_lane(&mut self, id: LaneId);
    pub(crate) fn lane_mut(&mut self, id: LaneId) -> Option<&mut Lane>;
    pub(crate) fn clear_frame(&mut self);
    pub(crate) fn set_order(&mut self, order: &[LaneId]);
}
```

要点（P03 直接照此实现）：
1. lane 各持 `Vec<Primitive>`（`REUSABLE` 整桶复用 = 搬指针，不 clone）；
2. `Scene` **持久**（lane id 跨帧稳定，`Node.lanes` 由 P04 持有）；
3. `LaneId` 带代际（§3.2「代际防悬垂」）；
4. `order: Vec<LaneId>` 由 P03 每帧按「节点绘制序 × 该节点的 lanes」展开；
5. `Scene` **不** `Clone`（没有消费者，且会掩盖「lane 复用是搬不是拷」）；`Lane` 必须 `Clone`（`Primitive::Canvas` 内嵌它）。

### 7.3 `REUSABLE` 复用与绘制序（P03 落地，P02 只定义形状）

| 需要 | 访问器 | 用途 |
|------|--------|------|
| lane → primitive 切片 | `Scene::lane(id)` + `Lane::primitives()` | P07 对 `REUSABLE` lane **不重建**：保留同一 `Vec`，只读它算 damage / 编码 |
| 本帧绘制序 | `Scene::order() -> &[LaneId]` | P07 展开顺序；P06 帧记录断言「顺序没变」 |
| 展平遍历 | `Scene::iter()` | 编码器 / 帧记录 / full-refresh oracle 的单一入口 |

### 7.4 clip 硬不变量

| # | 不变量 | 违反后果 | 强制点 |
|---|--------|----------|--------|
| C1 | 每条 lane 必须 clip-balanced（结束时 `clip_depth` 回到进入深度） | 编码器栈跨 lane 泄漏 → 后续 lane 全被误裁 | **P06 的展平阶段**断言（`Canvas` 内部由 `Canvas` 自己平衡） |
| C2 | `Pop` 不得弹穿 lane 起点 | 同上 | **P02 已落地**：`Lane::push` 的 `debug_assert!(usize::from(self.clip_depth()) > 0)`；`Canvas::pop_clip` 返回 `false`（N6） |
| C3 | 嵌套深度 `<= MAX_CLIP_DEPTH`（8） | 编码期 clip 表 / P10 uniform 溢出 | **P02 已落地**：`Lane::push` 的 `debug_assert!(usize::from(self.clip_depth()) < usize::from(MAX_CLIP_DEPTH))`；`Canvas::push_clip` 通过后返回 `false` |
| C4 | `Push.rect` / `corner_radius` 有限、尺寸非负 | SDF 出 NaN | 构建处 debug 断言（几何合法性由 P03 的布局输入保证） |

**为什么是「debug 断言 + bool 返回」而不是 `EngineError` 或 panic**：
① paint 是纯 CPU 数据构建层，clip 嵌套由引擎自己的布局/样式生成，应用层无法直接压栈；
② 应用层通过 `Canvas` 表达 clip，越界是**可恢复的调用错误**（返回 `false`，调用方可以不改绘制），
   因此在 release 与 debug 下都**不 panic**（N6）；
③ `Lane::push` 的 debug 断言只用于抓「引擎内部绕过 `Canvas` 直接 push」的编程错误（P02 里没有这种调用路径）。
## 8. `paint/instance.rs`（`GpuPrimitive` 契约与 CPU 编码）— P07 落地

> 本节是 **`render/shaders/contract.md`（P10）的权威来源**（I2/I8）。字段顺序、offset、取值表的任何改动都必须同时
> 改 Rust、三套 shader、`contract.md`，并触发像素快照回归。

### 8.1 结构体与静态断言（含 §2.4a 的修正）

```rust
/// uber-quad 的单实例。**128 B / 16 B 对齐**，三平台共用同一份字节布局。
#[repr(C, align(16))]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GpuPrimitive {
    pub bounds: [f32; 4],
    pub corner_radius: [f32; 4],
    pub color0: [f32; 4],
    pub color1: [f32; 4],
    pub params0: [f32; 4],
    pub params1: [f32; 4],
    pub uv: [f32; 4],
    pub clip_rect: [f32; 4],
}

// 静态断言写在 paint/instance.rs（叶子文件；M-03 禁止模块根放 item）
const _: () = {
    use core::mem::{align_of, offset_of, size_of};

    assert!(size_of::<GpuPrimitive>() == 128);
    assert!(align_of::<GpuPrimitive>() == 16);          // ← 必须 repr(C, align(16))；repr(C) 只有 4
    assert!(offset_of!(GpuPrimitive, bounds) == 0);
    assert!(offset_of!(GpuPrimitive, corner_radius) == 16);
    assert!(offset_of!(GpuPrimitive, color0) == 32);
    assert!(offset_of!(GpuPrimitive, color1) == 48);
    assert!(offset_of!(GpuPrimitive, params0) == 64);
    assert!(offset_of!(GpuPrimitive, params1) == 80);
    assert!(offset_of!(GpuPrimitive, uv) == 96);
    assert!(offset_of!(GpuPrimitive, clip_rect) == 112);
};
```

| 断言 | 为什么必须断言 |
|---|---|
| `size_of == 128` | §2.3/附录 B 的字节预算；缓冲区容量与「只写 dirty 区间」的偏移都依赖它 |
| `align_of == 16` | `vec4` 对齐是三套 shader 的结构体要求（Metal `float4`、HLSL `float4` 打包、GLSL `std430`）；**`repr(C)` 实际只有 4**（probe §2.4a），不加 `align(16)` 时这条断言**编译失败** |
| 8 个 `offset_of!` | `contract.md` 的 offset 表是镜像关系；任一字段重排必须让编译失败，而不是让三平台花屏 |

`bytemuck::Pod` 在 `#[repr(C, align(16))]` + 无 padding 下自动成立（probe §2.4b）。上传侧只需
`bytemuck::cast_slice::<GpuPrimitive, u8>(&instances)`，**零 unsafe**。

### 8.2 字节布局总表（`contract.md` 镜像）

| offset | 字节 | 字段 | 分量 | 语义（物理像素 / 已编码整数） |
|---|---|---|---|---|
| **0** | 4 | `bounds.x` | — | 左边界 `x·s`（**未取整**，P01 §7.2） |
| 4 | 4 | `bounds.y` | — | 上边界 `y·s`（未取整） |
| 8 | 4 | `bounds.z` | — | 宽 `w·s`（`= x1 − x0` 同一表达式） |
| 12 | 4 | `bounds.w` | — | 高 `h·s` |
| **16** | 4 | `corner_radius.x` | tl | 左上圆角（物理像素） |
| 20 | 4 | `corner_radius.y` | tr | 右上 |
| 24 | 4 | `corner_radius.z` | br | 右下 |
| 28 | 4 | `corner_radius.w` | bl | 左下 |
| **32** | 16 | `color0` | r,g,b,a | **premultiplied linear sRGB**：实色 / 渐变起色 / 阴影色 / 字形色 / 图片调制色 |
| **48** | 16 | `color1` | r,g,b,a | 渐变终色 **或** 边框色（互斥，§8.5）；无则全 0 |
| **64** | 4 | `params0.x` | `border_width` | 边框宽度（物理像素）；0 = 无边框 |
| 68 | 4 | `params0.y` | `shadow_blur` | 高斯模糊半径（物理像素）；非 Shadow 恒 0 |
| 72 | 4 | `params0.z` | `gradient_kind` | 0 none / 1 horizontal / 2 vertical / 3 radial |
| 76 | 4 | `params0.w` | `aa_padding` | 顶点外扩量（**物理像素**）：Quad `1.0`、Shadow `blur+1.0`、图集 `0.0`（P01 §7.2） |
| **80** | 4 | `params1.x` | `clip_index` | 帧内 clip 链表索引（§8.7）；表项 0 恒为「无裁剪」 |
| 84 | 4 | `params1.y` | `texture_index` | 图集页索引：Glyph `0`、Image `1`；非图集 kind 忽略 |
| 88 | 4 | `params1.z` | `kind` | §8.3 的取值 |
| 92 | 4 | `params1.w` | `flags` | 位掩码，§8.3 |
| **96** | 4 | `uv.x` | u0 | 图集归一化左边界 |
| 100 | 4 | `uv.y` | v0 | 上边界 |
| 104 | 4 | `uv.z` | u1 | 右边界 |
| 108 | 4 | `uv.w` | v1 | 下边界 |
| **112** | 4 | `clip_rect.x` | — | clip 链 AABB 左（物理浮点） |
| 116 | 4 | `clip_rect.y` | — | 上 |
| 120 | 4 | `clip_rect.z` | — | 宽 |
| 124 | 4 | `clip_rect.w` | — | 高 |
| **128** | — | 总计 | — | `align_of = 16` |

**编码约定（必须写进 `contract.md`）**：

1. `params1` 的四个分量都是「**整数编码进 f32**」（`value as f32`），取值必须落在 `[0, 2^24)`（f32 尾数保证精确）。
   三套 shader 一律用**数值转换**取回整数：HLSL/MSL `(int)x` / `int(x)`，GLSL `int(x)`。**禁止**位重解释
   （`as_type` / `asfloat` / `floatBitsToInt` / `bitcast`）——f32 存的是数值而不是整数的位模式，位重解释会得到完全错误的 kind/flags（N2；`contract.md` 必须逐字写明）。
2. `flags` 是位掩码（同样在 f32 精确区内）：`bit 0 = OPAQUE`，bit 1..=23 保留恒 0。
3. `bounds` 对 SDF 图元是**未取整浮点**（P01 `to_physical_f32`），对图集图元是**整数矩形的浮点表示**（P01 `to_physical`）。
   shader 用 `params0.w` 外扩顶点，片元用真实 `bounds` 求距离。
4. `clip_rect` 是 clip 链的**交集 AABB**（浮点物理像素）；`clip_index` 指向完整链（可含多层圆角）。
   两者冗余是**故意的**：`clip_rect` 给片元早退（一次比较丢掉被裁像素），链给精确圆角距离。

### 8.3 `kind` 与 `flags`

| `kind` | 值 | 产生者 | 说明 |
|---|---|---|---|
| `QUAD` | **0** | `Primitive::Quad` | 实色 / 边框 / 渐变（由 `params0` 区分） |
| `GLYPH` | **1** | `Primitive::GlyphRun`（每字形一个实例） | 采样 `R8` 覆盖度图集，乘以 `color0` |
| `IMAGE` | **2** | `Primitive::Image` | 采样 `BGRA8` sRGB 图集，乘以 `color0` |
| `SHADOW` | **3** | `Primitive::Shadow` | 解析阴影（erf 近似），`params0.y` = blur |
| 保留 | 4 | — | **Path 的 coverage 合成**（v1 不用；两阶段 AA 落地时启用，§6.3.4） |
| 保留 | 5 | — | Geometry 若改为实例化表达时使用（v1 走 geometry 通道） |
| 保留 | 6–7 | — | 应用自定义（§5.9 的 `Canvas` 自有 pipeline，v1 明确不做，F-29） |

```rust
// pub(crate)：kind 只能由编码器写入
pub(crate) const KIND_QUAD: f32 = 0.0;
pub(crate) const KIND_GLYPH: f32 = 1.0;
pub(crate) const KIND_IMAGE: f32 = 2.0;
pub(crate) const KIND_SHADOW: f32 = 3.0;

pub(crate) const FLAG_OPAQUE: f32 = 1.0;   // bit 0
pub(crate) const TEXTURE_GLYPH: f32 = 0.0;
pub(crate) const TEXTURE_IMAGE: f32 = 1.0;

pub(crate) const GRADIENT_NONE: f32 = 0.0;
pub(crate) const GRADIENT_HORIZONTAL: f32 = 1.0;
pub(crate) const GRADIENT_VERTICAL: f32 = 2.0;
pub(crate) const GRADIENT_RADIAL: f32 = 3.0;
```

**`flags` 只定义 bit 0（OPAQUE）**：

| 判据 | 结论 |
|---|---|
| 谁消费 | P09 的批次/管线选择：`OPAQUE` 可进「无混合」管线变体（省 blend state 切换），也用于 P18 的 draw call 统计 |
| 置位条件 | 见 §8.4 末尾的精确判据 |
| 为什么不定义 `HAS_COLOR1`/`GRADIENT`/`ROUNDED_CLIP` | 与 `params0.x`/`params0.z`/clip 链完全冗余 → 两个真值来源；shader 读一个 f32 与读一位一样便宜 |
| 为什么不预留更多语义位 | 预留位会被三平台各自解释；只在**有消费方**时增加（bit 1..=23 在 contract 里写「保留恒 0」） |

### 8.4 8 类 → `GpuPrimitive` 逐类映射表

记 `s = scale.factor()`，`R = ColorResolver::resolve`，`f32r = Rect::to_physical_f32`，`phys = Rect::to_physical`（P01 §7.1）。

| 字段 | `Quad` | `GlyphRun`（每字形） | `Image` | `Shadow` |
|---|---|---|---|---|
| `bounds` | `f32r(q.bounds)` | `phys(glyph rect)` 的 4 分量转 f32（**整数矩形**） | `phys(img.bounds)` 转 f32 | `f32r(shadow.rect.translate(offset))` |
| `corner_radius` | `corner_radius × s`，逐角 clamp `<= min(w,h)/2` | 全 0 | 全 0 | 全 0 |
| `color0` | `R(Solid.fill)` / `R(Gradient.from)` | `R(run.color)` | `R(image.modulation)` | `R(shadow.color)` |
| `color1` | 边框 `R(border.color)`；渐变 `R(Gradient.to)`；否则 `[0;4]` | `[0;4]` | `[0;4]` | `[0;4]` |
| `params0.x` | `border.width × s`（clamp）；无边框 0 | 0 | 0 | 0 |
| `params0.y` | 0 | 0 | 0 | `blur × s` |
| `params0.z` | 渐变 kind；实色 0 | 0 | 0 | 0 |
| `params0.w` | `1.0` | `0.0` | `0.0` | `blur × s + 1.0` |
| `params1.x` | 当前链 id | 当前链 id | 当前链 id | 当前链 id |
| `params1.y` | 0（忽略） | `TEXTURE_GLYPH` | `TEXTURE_IMAGE` | 0（忽略） |
| `params1.z` | `KIND_QUAD` | `KIND_GLYPH` | `KIND_IMAGE` | `KIND_SHADOW` |
| `params1.w` | OPAQUE? | OPAQUE? | OPAQUE? | `0.0` |
| `uv` | `[0;4]` | `tile.uv()` | `tile.uv()` | `[0;4]` |
| `clip_rect` | 链 AABB | 同 | 同 | 同 |

| 变体 | 映射 |
|---|---|
| `Primitive::Path` | **0 实例**；`flush_run()` 后追加 `FrameOp::Geometry(GeometryDraw { id, transform: Transform::IDENTITY })`（§8.6） |
| `Primitive::Geometry` | **0 实例**；`flush_run()` 后追加 `FrameOp::Geometry(draw)` |
| `Primitive::Clip` | **0 实例**；只改编码器栈（§8.7） |
| `Primitive::Canvas` | 按 `Canvas::primitives()` 顺序**展平**（顶层 1 层，内部可含 `Clip`；不得再含 `Canvas`，编码器 `assert!(canvas_depth == 0)`），逐项按上表映射；`Canvas::bounds` 只用于 damage/命中 |

**`OPAQUE` 的精确判据**（编码器统一实现，写进 contract）：

```text
opaque = kind ∈ {QUAD, GLYPH, IMAGE}
         && gradient_kind == 0
         && color0.is_opaque()
         && (kind != QUAD || border_width == 0.0 || color1.is_opaque())
```

### 8.5 边框与渐变：`color1` 的单一用途 + 两个 `Quad` 的拆分（裁决 ⑤）

| `QuadPaint` | `params0.x` | `params0.z` | `color0` | `color1` |
|-------------|-------------|-------------|----------|----------|
| `Solid { fill, border: None }` | 0 | 0 | fill | 0 |
| `Solid { fill, border: Some(b) }` | `b.width·s` | 0 | fill | b.color |
| `Gradient(g)` | 0 | g.kind | g.from | g.to |

三种形状互斥 ⇒ shader 的判定**无歧义**：`border_width > 0` ⇒ color1 是边框色；`gradient_kind > 0` ⇒ color1 是渐变终色；
两者同时非零是编码器 bug（`debug_assert!`）。

**「渐变 + 边框」在 v1 用具名语义的两个 `Quad` 表达**（P03 的 style → paint 负责拆分，编码器不做自动拆分）：

| 顺序 | 图元 | 内容 | 说明 |
|------|------|------|------|
| 1 | `Quad { paint: Gradient { from, to, kind } }` | 整块**填充**（含圆角） | 渐变铺满 bounds，**不带** `border` |
| 2 | `Quad { paint: Solid { fill: TRANSPARENT, border: Some(b) } }` | 同 bounds 的**边框** | `fill` 透明 ⇒ 不产生第二层填充混合 |

| 关注点 | 契约 |
|--------|------|
| **绘制顺序** | 必须「先填充/渐变，后边框」。原因：边框是叠在填充之上的描边；反序会被填充覆盖。顺序由 P03 的 style → paint 保证，`Scene`/编码器不重排（顺序即绘制序，§6.1） |
| **半透明边框的混合** | 边框 quad 自己是不透明的（`border.color.a == 1`）时，它直接覆盖填充的内缘（CSS 内联边框的视觉）；`border.color.a < 1` 时，边框像素 = `border over fill`，填充内缘**只被覆盖一次** ⇒ 无重复混合 |
| **两者都半透明时的边界** | 若 `from/to` 半透明 **且** 边框半透明，边框区域会经历「填充（半透明）→ 边框（半透明）」**两次 premultiplied over**。这是**设计上接受的行为**（与 CSS「background + 半透明 border」一致：border 叠在 background 之上），但会产生边缘颜色偏移（尤其深色描边叠在浅色渐变上） |
| **应用层责任** | ①需要「边框抠掉填充」的观感时，`base_ui` 必须给 `fill` 用**与背景预乘合成后的不透明色**，或让边框不透明；②不得指望引擎做「边框内缘挖空」（那需要 stencil/两 pass，破坏 v1 的单实例模型）。这条写进 `base_ui` 的样式约定，不写进引擎 API |
| **damage 影响** | 两个 quad 的 `bounds` **相同** ⇒ damage 累加器的合并判据（§12.4：`area(union) == area(a) + area(b)`，共边/完全重合）会**合并成一个 rect**；不会产生双倍 damage 区域，也不会因为两次 push 让 rect 数超过阈值（同一矩形反复合并） |
| **实例数** | +1（每节点最多 2 个 `Quad` 实例）；与「128 B 只有两个颜色槽」的取舍相比，多 1 个实例远比扩字段便宜 |
| **`OPAQUE` 标志** | 边框 quad 的 `color0` 是透明填充 ⇒ `color0.is_opaque() == false` ⇒ **不带** `OPAQUE`（即使边框色不透明）。这是保守的正确侧：整个实例的合成结果依赖 dst |

**为什么不新增 `border_gradient` 字段或第二个颜色槽**：`GpuPrimitive` 冻结在 128 B（I2/I8），加字段会同时改三套 shader +
contract + damage；而两个 quad 的方案**只是**用现有字段两次，P09 的批次归并会把它们并进同一批（同 clip/同 texture=0/
同 blend），draw call 不增加。
### 8.6 编码顺序与跳过规则

```text
encode(scene, scale, resolver) -> InstanceBuffer:
  buf.clear(); buf.push_clip(None)                    // clip 表 0 号 = 无裁剪
  stack = []; depth = 0
  for lane in scene.order():
      entry_depth = depth                             // §7.4 C1
      for p in lane.primitives():
          match p:
            Clip(Push(c))  -> { assert(depth < MAX_CLIP_DEPTH); stack.push(c); depth += 1 }
            Clip(Pop)      -> { assert(depth > entry_depth); stack.pop(); depth -= 1 }
            Quad(q)        -> if visible(q) { emit_quad(q) }
            Shadow(sh)     -> if !transparent(sh) { emit_shadow(sh) }
            Image(im)      -> if visible(im) { emit_image(im) }
            GlyphRun(run)  -> if run.color 非透明 { for g in run.glyphs { emit_glyph(g) } }
            Path(p)        -> { flush_run(); push_op(Geometry(p.geometry, IDENTITY)) }
            Geometry(g)    -> { flush_run(); push_op(Geometry(g.id, g.transform)) }
            Canvas(c)      -> { assert!(canvas_depth == 0);                     // 嵌套深度 ≤ 1（§6.3.8）
                                for p2 in c.primitives() { 同上（可递归 Clip/Quad/…，**不得**再遇 Canvas） }
                                assert!(depth == entry_depth) }                 // Canvas 内部也必须平衡
      assert_eq!(depth, entry_depth)                  // C1
```

**跳过规则**（「不产生像素的图元不进实例缓冲」，也是 §5.7「遮挡区域不生成 Primitive」的补充）：

| 条件 | 处理 |
|---|---|
| `R(主色).is_transparent()` 且无边框/无渐变 | 跳过（也不参与 damage：它不产生像素） |
| `bounds` 为空（`Rect::is_empty`） | 跳过 |
| `bounds` 换算后为空（非有限 / 负尺寸，P01 §6.3.3） | 跳过（**不**把 NaN 夹带进缓冲） |
| `GlyphRun.glyphs` 为空 | 跳过 |
| `Path`/`Geometry` 的 id 已失效 | **不跳过**：由 P09 在装载侧保证 id 有效；编码器不查表（它没有 store） |

**`flush_run()`**：遇到 geometry 类图元时结束当前连续实例段，保证 `FrameOp` 顺序 = 绘制顺序（z 序正确）。
clip 变化时**不**强制 flush：实例自带 `clip_index`，P09 可以在一个 draw 里按 `clip_index` 分组，也可以选择在
`Instances` 段内再切批（§8.8 的 `FrameOp` 只保证**顺序**，不承诺批次边界）。

### 8.7 clip 链的编码期解析

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClipChainId(u32);          // 帧内表索引

#[derive(Clone, Copy, Debug)]
pub(crate) struct ClipChain {
    /// 父链；`None` = 根。表项 0 是无裁剪哨兵。
    pub parent: Option<ClipChainId>,
    /// 本层 clip 的物理浮点矩形 `(x, y, w, h)`。
    pub rect: [f32; 4],
    /// 本层四角半径（物理像素）。
    pub corner_radius: [f32; 4],
    /// 从根到本层的**交集 AABB**（物理浮点），写进 `GpuPrimitive.clip_rect`。
    pub aabb: [f32; 4],
}
```

| 规则 | 内容 |
|---|---|
| 表项 0 | 恒为「无裁剪」：`parent = None`，`aabb` 取 `f32::MIN/MAX` 的安全值；P10 以「表项 0 = 直接通过」特判，不做数学运算 |
| 去重 | 键 = `(parent, rect, corner_radius)`，`ahash::HashMap`；同一帧内相同链共享 id |
| 生命周期 | 表**每帧重建**（`InstanceBuffer::clear` 时清空），`clip_index` 是**帧内**索引，不进任何持久结构 |
| 与 `Scene` 的关系 | `Scene` **没有** clip 表（§7.1）；表是编码产物 → lane 复用安全（§6.3.6 的判据） |
| 与批次的关系 | §5.6 的「按 `(clip_chain, pipeline, texture_index)` 归并」由 P09 在 `Instances` 段内完成；`clip_index` 是它的 key |
| `MAX_CLIP_DEPTH` | 链深上限即栈深上限（8），`ClipChain` 最多 8 层父链 |

### 8.8 `InstanceBuffer` 与 `FrameOp`（保持绘制序的最小表达）

```rust
/// 一段连续实例（同一帧、同一 `Instances` 段）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameOp {
    /// `instances[start .. start + len]` 连续；段内 `clip_index` 可以不同（P09 可再切批）。
    Instances { start: u32, len: u32 },
    /// `geometry[index]` 的一次几何绘制（`Primitive::Path` / `Primitive::Geometry`）。
    Geometry(u32),
}

pub(crate) struct InstanceBuffer {
    instances: Vec<GpuPrimitive>,
    clips: Vec<ClipChain>,
    geometry: Vec<GeometryDraw>,
    ops: Vec<FrameOp>,
}

impl InstanceBuffer {
    pub(crate) fn clear(&mut self);
    pub(crate) fn instances(&self) -> &[GpuPrimitive];
    pub(crate) fn as_bytes(&self) -> &[u8];          // bytemuck::cast_slice
    pub(crate) fn clips(&self) -> &[ClipChain];
    pub(crate) fn geometry(&self) -> &[GeometryDraw];
    pub(crate) fn ops(&self) -> &[FrameOp];
    pub(crate) fn len(&self) -> usize;
}

pub(crate) struct InstanceEncoder {
    buffer: InstanceBuffer,
    // 栈、去重表、scale（仅编码期）
}

impl InstanceEncoder {
    pub(crate) fn new() -> Self;
    /// 编码整帧；内部缓冲复用（不清空 capacity，§9.3）。
    pub(crate) fn encode(&mut self, scene: &Scene, scale: Scale, resolver: &ColorResolver) -> &InstanceBuffer;
}
```

| 决策 | 结论与理由 |
|---|---|
| 为什么 `FrameOp` 不描述批次 | 批次是 P09 的**优化**（按 clip/texture/kind/blend 归并），不是契约；把批次写进 CPU 侧产物会让三平台的批次策略被 P07 钉死 |
| 为什么需要 `FrameOp` 而不是两个独立数组 | 几何绘制与实例绘制的**相对顺序**决定 z 序；只有合成一棵有序列表才能表达（否则地图的静态网格会盖住所有 quad） |
| 为什么 `Instances` 用 `(start, len)` 而不是 `Range<u32>` | `Copy` 友好，且避免在内部 API 里引入 `Range` 的迭代语义 |
| 为什么 `scale` 是 `encode` 参数而不是字段 | DPI 会在运行中变化（`WM_DPICHANGED`）；`Scale` 是每帧输入，不是编码器状态 |
| 为什么 `resolver` 是参数 | 主题切换会换 `Interpolation`；主题色解析策略属应用层（D23），编码器不拥有它 |
| 颜色每帧重新 `resolve` 的成本 | 每图元约 30–80 ns（一次 `DynamicColor::convert` + Oklab↔Oklch 的三角/立方根）；5000 图元 ≈ 0.3 ms。若 P18 实测成为热点，可在 lane 内缓存 `LinearPremulRgba`（additive，不改 128 B 契约）→ 风险 R7 |

---

## 9. `paint/geometry.rs`（静态几何通道）

### 9.1 `GeometryId` 的生成/销毁语义

```rust
/// 已注册静态几何的句柄。**`Copy`**（要每帧塞进 `GeometryDraw`），带代际。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GeometryId { index: u32, generation: u32 }   // 字段**私有**；读 index()/generation()；只由 GeometryStore 铸造
```

| 操作 | 语义 | 实现 Phase |
|---|---|---|
| `App::register_geometry(desc) -> Result<GeometryId, EngineError>` | 校验 desc（§9.2）→ 分配 slot（优先复用 retired 槽位，`generation += 1`）→ 装载 VBO/IBO（常驻）→ 返回 id | **P08**（API）/ **P09**（装载） |
| 失败 | `EngineError::GeometryRegister { detail }`（F-15 #11，P03 起可用） | P09 |
| `App::release_geometry(id)` | 标记 retired，按 `FrameIndex` 入 retire 队列（I4）；in-flight 帧用完后真正释放 GPU 资源 | **P08/P09** |
| 陈旧 id | `GeometryId` 的 `generation` 不匹配 → 使用该 id 的 `Primitive` 被 P09 丢弃（并记一次诊断日志）。**不做 fallback 绘制** | P09 |

**为什么 `GeometryId` 是 `Copy` 而不是 `Drop` 句柄**（§5.5 原文写 `drop(GeometryId)` → retire）：

- `Primitive::Geometry`/`Primitive::Path` 每帧都要携带 id（`Copy` 是零成本）；若 id 是 `Drop` 句柄，要么每帧 clone（引用计数）、
  要么 `Primitive` 不能 `Copy`，两种都让 lane 复用的成本变高。
- `Copy` 句柄没有 `Drop`，因此**释放必须显式**：`App::release_geometry(id)`。
- 代价：忘记释放会泄漏 VBO（到进程结束）。缓解：P08 的 `NodeKind::Geometry` 节点在卸载时自动 release；应用层直接持有
  `GeometryId` 的场景（图表）由 `widget`/`base_ui` 在 `Drop` 里调 release。→ **F-30**

### 9.2 `GeometryDesc`（装载期一次写入、之后不可变）

```rust
#[derive(Debug)]
pub struct GeometryDesc {
    /// 顶点字节。长度必须是 `vertex_stride` 的整数倍。
    pub vertex_bytes: Bytes,
    /// 索引字节。长度必须是 `index_type.size()` 的整数倍。
    pub index_bytes: Bytes,
    /// 每顶点字节数，必须等于 `pipeline.vertex_stride()`（§9.4）。
    pub vertex_stride: u32,
    pub index_type: IndexType,
    pub topology: Topology,
    /// **逻辑像素** AABB（局部空间，未经 `transform`）。damage / 命中 / 裁剪判断的唯一来源。
    pub bounds: Rect,
    pub pipeline: GeometryPipeline,
}
```

`Bytes`（`bytes 1.12.1`）而不是 `Vec<u8>`：装载后不可变、需要廉价共享与切片（AGENTS §2.4）。

**注册期校验表**（`EngineError::GeometryRegister { detail }` 的唯一产出点，P09 实现）：

| # | 规则 | 理由 |
|---|---|---|
| V1 | `vertex_stride == pipeline.vertex_stride()` | 布局契约（§9.4）只有一份定义；不一致会让 shader 读错字节 |
| V2 | `vertex_stride > 0 && vertex_bytes.len() % vertex_stride == 0` | 否则最后一个顶点截断 |
| V3 | `index_bytes.len() % index_type.size() == 0` | 否则索引截断 |
| V4 | `TriangleList`: 索引数 `% 3 == 0` 且 `>= 3`；`TriangleStrip`: 索引数 `>= 3` | 退化几何（<3）不产生像素 |
| V5 | `bounds` 有限、`size >= 0`、非空 | damage 的 AABB 契约（P01 §6.3.3） |
| V6 | `vertex_bytes` / `index_bytes` 非空 | 空几何注册没有意义（调用方应直接不注册） |
| 不做 | 扫描索引越界（`index >= vertex_count`） | O(n) 且属调用方 bug；把违规交给 P09 的 debug 校验/D3D12 的 GPU 校验层（P18 像素测试会暴露） |

### 9.3 `IndexType` / `Topology`

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexType { U16, U32 }
impl IndexType { pub const fn size(self) -> u32; }      // 2 / 4

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Topology { TriangleList, TriangleStrip }
```

| 类型 | 取值 | 消费方 |
|---|---|---|
| `IndexType` | `U16`（默认，<= 65535 顶点）/ `U32`（大网格） | P09 的 index buffer 格式；K 线/地图网格顶点数差异很大，两者都要 |
| `Topology` | `TriangleList` / `TriangleStrip` | P08 的图表（面积图/带状的骨骼）天然是 strip；三平台都直接支持 |

`IndexType::size()` 与 `GeometryPipeline::vertex_stride()` 是**布局契约的唯一来源**（P09 校验 + P10 shader 注释 + 单测同时引用），
不是「无意义的封装函数」。

### 9.4 `GeometryPipeline` 与顶点布局

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryPipeline {
    /// 实色三角：`[f32; 2]` 位置（**逻辑像素**，8 B/顶点）。`Primitive::Path` 固定用这个。
    Ui,
    /// 纹理三角：`[f32; 2]` 位置 + `[f32; 2]` 归一化 UV（16 B/顶点），采样 `atlas` 指定的图集页。
    Textured { atlas: AtlasKind },
}
impl GeometryPipeline { pub const fn vertex_stride(self) -> u32; }   // 8 / 16
```

| pipeline | stride | 布局 | 顶点坐标系 | 着色 | 首个消费方 |
|---|---|---|---|---|---|
| `Ui` | 8 | `[x, y]` | 逻辑像素（局部）；经 `GeometryDraw.transform` + 全局帧 uniform 进 NDC | **per-draw 常量色**（§9.5 的 `color0`），顶点不带颜色 | P07/P09 |
| `Textured { atlas }` | 16 | `[x, y, u, v]` | 同上；`uv` 归一化到该图集页 | 采样 `atlas` + per-draw 常量色调制 | P09 |

> **`Ui` 的颜色为什么走 per-draw 常量而不是顶点属性**：把颜色放进顶点会把 stride 从 8 变成 16（+100% 顶点内存），
> 而 K 线/地图的每张网格只有一个颜色（§5.5 的省内存目标正是「顶点不带框架费」）。代价是一张网格一个 draw —— 这本来
> 就是 `Geometry` 的语义（一个 `GeometryId` 一次绘制）。需要「同网格多色」时由应用拆成多个 `GeometryId` 或走 `Path`。

**为什么 `Path` 不单独占一个 pipeline 变体**（F-31）：`Path` 的顶点布局、着色方式、管线状态与 `Ui` 完全一致，唯一差别是
「谁拥有这个几何、变换是否恒等」。在 `GeometryPipeline` 里再造一个 `Path` 变体会让 P09 必须维护两条一模一样的管线
（三平台 × 2 = 6 份），违反「不做预防性抽象」。

### 9.5 per-draw 常量：`GeometryUniform`（32 B）+ `GeometryColor`（16 B）

```rust
/// geometry 管线的 per-draw 变换：**32 B**，两个 vec4（P01 F-14 冻结的布局）。
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GeometryUniform {
    /// `(m11, m12, m21, m22)`：2D 仿射线性部分，与 `Transform::to_array()` 前 4 项同序。
    pub basis: [f32; 4],
    /// `(tx, ty, 0.0, 0.0)`：平移 + 2 个 padding。
    pub translation: [f32; 4],
}

/// geometry 管线的 per-draw 颜色：**16 B**，premultiplied linear RGBA。
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GeometryColor { pub color0: [f32; 4] }

const _: () = {
    assert!(core::mem::size_of::<GeometryUniform>() == 32);
    assert!(core::mem::align_of::<GeometryUniform>() == 4);
    assert!(core::mem::offset_of!(GeometryUniform, basis) == 0);
    assert!(core::mem::offset_of!(GeometryUniform, translation) == 16);
    assert!(core::mem::size_of::<GeometryColor>() == 16);
};
```

| 项 | 契约 |
|---|---|
| 上传布局 | P09 把两个结构体**相邻**写进同一条常量区：`[0..32) = GeometryUniform`、`[32..48) = GeometryColor`。每 draw 48 B，与「每帧 0 B 顶点上传」相比可忽略 |
| 顶点着色器 | `p' = (m11·x + m21·y + tx, m12·x + m22·y + ty)`（与 `math::Transform::apply` 逐字一致，P01 §6.3.2） |
| 之后 | 再乘**全局帧 uniform**（DPI `scale` + NDC 映射）。分工写进 `contract.md`：per-draw uniform **只做逻辑空间 pan/zoom**，`scale` 只出现在帧 uniform |
| 颜色来源 | `Ui`：`resolve(Style.background)` 或 `Path.fill`；`Textured`：调制色（同 `Image.modulation` 语义）。`color0` 为 premultiplied linear，shader 不做空间转换 |
| 已知取舍 | `Ui` 不做逐顶点色/渐变（需 stride 变 16 或再加一组常量）；图表需要渐变时用 `Quad` 的 `Gradient` 或拆 `Path` |
| 与 128 B 的关系 | `GeometryUniform`/`GeometryColor` 与 `GpuPrimitive` **完全独立**：geometry 通道不消费 `GpuPrimitive`（§8.3 的保留 kind 5 只是未来选项） |
| 落地 Phase | **P09**（`pub(crate)` + 无 P02 消费者，理由同 §3.2） |

### 9.6 `set_transform` 与 `GEOMETRY` dirty（只给语义，dirty 机制属 P03/P06）

| 项 | 语义 |
|---|---|
| `App::set_geometry_transform(id, transform)` | 只写「该几何本帧的 uniform」。**不上传顶点、不重建 lane、不重建 VBO**（§5.5 表格） |
| 存储位置 | `GeometryStore` 的 per-id 槽位（P09）；`Scene` 不存（`Primitive::Geometry` 携带的是**本帧**的 transform） |
| dirty | 置 `DirtyFlags::GEOMETRY`（§3.4 `0b0000_0100`）。**P02 只定义接口语义**：`set_geometry_transform` 是 `GEOMETRY` 的唯一写入点；`GEOMETRY` **不冒泡**、不触发 `PAINT`/`LAYOUT` |
| damage | `covering(desc.bounds 经 transform 的 AABB)` 按物理像素外扩 `aa_padding = 1.0`（§12.3）。变换前后 AABB 的**并集**只在「上一帧也有该几何」时才有意义 → P07 用 `prev_transform` 参与并集（`REUSABLE` lane 复用时的 damage 由 P07 的节点级记录提供，§5.7） |
| 旋转 | `Transform` 只支持 translate/scale（P01 S-07）；AABB 对这两类变换**精确**（不需要保守外扩） |
| 失效 | `release_geometry` 后仍有 `Primitive::Geometry` 引用该 id → P09 丢弃该 draw（§9.1），damage 仍按节点布局算（安全侧） |

### 9.7 与 `Primitive::Path` 的关系

`Path` 的几何必须是 `GeometryPipeline::Ui`（stride 8、位置语义、三角列表），`GeometryDraw.transform = Transform::IDENTITY`。
`GeometryRegister` 不区分「这是给 `Path` 用的还是给应用注册的」——同一个 store、同一套校验、同一套 retire 策略。
这样做的好处：`Path` 不需要第二套注册 API / 第二套 buffer 管理；代价：`Path` 的 `GeometryDesc.bounds` 必须由 P15 正确给出
（`Path.bounds` 是同一个 AABB 的副本，供 damage 直接用而不用查 store）。

---

## 10. `paint/atlas.rs`（公开数据）+ `render/atlas.rs`（P05 分配器）

### 10.1 `AtlasKind` / `AtlasTile`

```rust
/// 图集分两类（§5.6：字形与图片分图集）。`#[repr(u8)]` 的值即 `texture_index`（§8.2）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AtlasKind {
    /// 字形覆盖度：`R8`，线性（非 sRGB）。
    Glyph = 0,
    /// 图片：`BGRA8`，**sRGB 编码**（硬件 sRGB 解码）+ 直通 alpha。
    Image = 1,
}

impl AtlasKind {
    /// 该图集页在三平台纹理数组/bindless 表里的索引（= `GpuPrimitive.params1.y`）。
    pub const fn index(self) -> u32;
}

/// 图集里的一块矩形 + 代际。**只读数据**：应用层（P15）持有它来画图标，引擎（P07）把它编码进 `uv`。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AtlasTile {
    pub atlas: AtlasKind,
    /// 归一化 UV（**内容区边**，不含 padding，§10.2）。
    pub u0: f32,
    pub v0: f32,
    pub u1: f32,
    pub v1: f32,
    /// 分配时的图集代际；与当前代际不等即**失效**（§10.6）。
    pub generation: u32,
}

impl AtlasTile {
    /// 直接进 `GpuPrimitive.uv`。
    pub const fn uv(self) -> [f32; 4];
    /// `self.generation != atlas_generation`：P06 缓存校验、§3.8「图集重排 → PAINT」的判据。
    pub const fn is_stale(self, atlas_generation: u32) -> bool;
}
```

| 决策 | 结论与理由 |
|---|---|
| 为什么 `u0..v1` 是扁平字段而不是 `uv: [f32; 4]` | 帧记录/断言可读（`tile.u0` vs `tile.uv[0]`）；`uv()` 是唯一给 GPU 的组装点，避免两处数组字面量 |
| 为什么不带 `page` 字段 | v1 每个 `AtlasKind` **单页**（§10.6）；多页需要 `page` 字段 → 属契约变更，延后（风险 R5） |
| 为什么不带 `rect: PhysicalRect` | 公开面只需要 UV；像素矩形是 crate 内部事（`Allocation::content`，§10.4；P05 装箱时用）。渲染侧只需要 UV |
| `generation: u32` 而不是 `u16` | 与 `AtlasAllocator::generation` 同宽（§10.6），避免窄化比较 |

### 10.2 UV 归一化约定（**用边，不用纹素中心**）

```text
设页尺寸 (W, H)，内容区纹素矩形 [x0, x1) × [y0, y1)（x1 = x0 + w，y1 = y0 + h）：
    u0 = x0 / W        u1 = x1 / W
    v0 = y0 / H        v1 = y1 / H
```

| 为什么是「边」 | 说明 |
|---|---|
| 1:1 采样时两种约定等价 | 物理矩形是整数（`to_physical`），片元中心落在纹素中心，`u*W` 恰好命中纹素 |
| 放大图片时不丢边 | 半纹素内缩会让可见图像缩 1 纹素；用边则 quad 正好覆盖整块 tile |
| 避免跨 tile 渗色 | 内容区四周有 1 纹素**清空**的 padding（§10.3）；放大时 bilinear 最多读到 padding 的透明值，**不会**读到邻块 |
| 代价 | 放大时最外 0.5 纹素会混入 padding → 最多 1 物理像素的柔和边。v1 接受（`Image` 的放大场景主要是背景图） |

**不做**任何半纹素偏移（`+ 0.5 / W`）：字形/图片的 quad 与 tile 内容区**必须**一一对应，否则 1:1 采样会退化为双线性插值
（字重变粗、图标发虚）。这条写进 `contract.md`。

### 10.3 两种图集的格式与 tile padding

| 项 | `AtlasKind::Glyph` | `AtlasKind::Image` |
|---|---|---|
| 纹素格式 | `R8_UNORM`（线性覆盖度） | `BGRA8` **sRGB**（`*_UNORM_SRGB` / `*_SRGB`），alpha 直通 |
| 默认页尺寸 | 2048 × 2048 = **4 MiB** | 2048 × 2048 = **16 MiB** |
| 写入方 | P06（swash/skrifa 光栅出 alpha） | P15（`image` 解码 → 缩放） |
| 采样 | `.r` 作为覆盖率 | 硬件 sRGB 解码到线性 |
| shader 公式 | `out = color0 * texel.r`（预乘色 × 标量，4 分量同乘） | `out = color0 * vec4(texel.rgb, texel.a)`（预乘色 × 线性直通色） |
| tile padding | **1 纹素**，四周清 0，**不计入 UV** | 同左 |
| 对齐 | `etagere` 的 `alignment = (1, 1)`（纹素级） | 同左 |

**为什么 padding 是 1 纹素而不是 2/4**：①1:1 采样读不到 padding（§10.2）；②只有放大/旋转采样才可能读到，1 纹素足以让
bilinear 的 2×2 抽样全部落在「内容 + 透明 padding」内；③padded 尺寸 `+2` 对小图元（字形 8×12）是 40% 的面积开销，
再大就不划算。

**为什么字形图集不用 sRGB**：覆盖率是线性量；套 sRGB 解码会把 0.5 的覆盖率变成 0.21，字变细。

### 10.4 `render/atlas.rs`：分配器（**P05 落地**，裁决 ①）

```rust
/// 一个 `AtlasKind` 的分配器（单页；§10.6 的「清空 + 代际」是容量逃逸路径）。
pub(crate) struct AtlasAllocator {
    kind: AtlasKind,
    page: (u32, u32),                     // 当前页尺寸（纹素）
    allocator: etagere::AtlasAllocator,
    tiles: Vec<Option<TileRecord>>,        // 下标 = etagere 的内部 index，用于 double-free 防护
    generation: u32,
    dirty: DirtyRows,
}

struct TileRecord {
    id: etagere::AllocId,
    tile: AtlasTile,
    content: PhysicalRect,                 // 内容区（不含 padding）：P05 装箱校准 + P09 上传目标
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Allocation {
    /// 释放键。**只能**用这里返回的 id 调 `deallocate`（etagere 对非法 id 会 panic，§2.2③）。
    pub id: etagere::AllocId,
    /// 可以直接塞进 `Primitive::Image` / `PositionedGlyph`。
    pub tile: AtlasTile,
    /// 内容区像素矩形（页面坐标，不含 padding）；P09 的纹理上传目标。
    pub content: PhysicalRect,
    /// 分配到的（可能被 etagere 向上取整放大的）整块矩形，含 padding。
    pub padded: PhysicalRect,
}

impl AtlasAllocator {
    pub(crate) fn new(kind: AtlasKind) -> Self;
    pub(crate) fn kind(&self) -> AtlasKind;
    pub(crate) fn page(&self) -> (u32, u32);
    pub(crate) fn generation(&self) -> u32;

    /// 分配 `content` 大小的内容区（内部加 1 纹素 padding）。
    /// 返回 `None` = 本页放不下（调用方走 §10.6 的清空/报错路径）。
    pub(crate) fn allocate(&mut self, content: (u32, u32)) -> Option<Allocation>;

    /// 释放。**幂等**：未记录的 id 直接忽略（避免 etagere 的 panic）。
    pub(crate) fn deallocate(&mut self, id: etagere::AllocId);

    /// 清空并 `generation += 1`（DPI 变化 / 字形缓存换代 / 容量逃逸）。
    pub(crate) fn clear(&mut self);

    /// 记录「这些行被写脏了」（§10.5）。
    pub(crate) fn mark_written(&mut self, rect: PhysicalRect);
    pub(crate) fn take_dirty_rows(&mut self) -> DirtyRows;
}
```

`allocate` 的实现要点（全部来自 §2.2 的源码事实）：

```text
1. 拒绝 content.0 == 0 || content.1 == 0
2. 拒绝 content.dim + 2 > 65535                              // etagere: width/height > u16::MAX → None（:189）
3. 拒绝 content.w + 2 > page.0                               // 单列时 shelf_width == page.0（:99-100），超宽结构上不可能成功
4. padded = (content.w + 2, content.h + 2)
5. let a = allocator.allocate(size2(padded.0, padded.1))?     // 注意：返回的 rectangle 可能因 alignment/shelf 分档而更大
6. content_origin = (a.rectangle.min.x + 1, a.rectangle.min.y + 1)
7. uv = (content_origin.x / W, content_origin.y / H,
         (content_origin.x + content.w) / W, (content_origin.y + content.h) / H)
8. 记录 TileRecord { id: a.id, tile: AtlasTile { atlas: kind, u0..v1, generation: self.generation },
                     content, padded: a.rectangle 转 PhysicalRect }
9. 返回 Allocation
```

### 10.5 脏行跟踪（`DirtyRows`）

```rust
/// 被写脏的**行区间**集合（y 区间，x 覆盖整页）。用于 P09 的增量上传。
#[derive(Clone, Debug, Default)]
pub(crate) struct DirtyRows { /* spans: Vec<Range<u32>>，已合并、升序 */ }

impl DirtyRows {
    /// 标记 `[y0, y1)` 脏；与已有区间合并（相邻也算合并）。
    pub(crate) fn mark(&mut self, y0: u32, y1: u32);
    pub(crate) fn spans(&self) -> &[core::ops::Range<u32>];
    /// 是否退化为「整页」（spans 数 > 8 时自动成立）。
    pub(crate) fn is_full(&self) -> bool;
    pub(crate) fn clear(&mut self);
}
```

| 规则 | 内容 |
|---|---|
| 粒度 | **整行**（`x` 全宽）而不是矩形：图集上传在三个 API 上都是「行跨度拷贝」，按行跟踪最省 CPU 且实现最简单 |
| 上限 | `spans > 8` → 折叠为单个 `[0, page_h)`（8 段已经意味着大半页都在动，不如整页） |
| 与 `mark_written` 的关系 | `mark_written(rect)` = `mark(rect.y0, rect.y1)`；`rect` 非法（空/逆序）时忽略 |
| 消费方 | P09 的 `render/atlas` 上传路径；单元测试可独立验证合并逻辑 |
| 为什么不放 `AtlasTile` 里 | 脏行是**页级**状态，与 tile 无关；放 tile 里会让「一行有 3 个 tile 更新」产生 3 条记录 |

### 10.6 `generation` 的递增时机与容量失败路径

| 时机 | 是否 `generation += 1` | 理由 |
|---|---|---|
| 单次 `allocate` / `deallocate` | **否** | 只是占位变化；已有 tile 的 uv 不变，引用方无需失效 |
| `clear()`（DPI 变化 / 字形按 scale 换代 / P15 资源重载） | **是** | 所有 uv 失效；`is_stale` 让引用方 `PAINT`（§3.8） |
| 容量逃逸（§10.6 下方） | **是**（内部调 `clear`） | 同左 |
| 页尺寸变化（若 P09 决定实现扩容） | **是** | 全部 uv 重算 |

**容量失败路径（P05，写进实现检查单）**：

```text
allocate(content) -> None
  → 第一次：atlas.clear()（generation += 1；引用该图集的节点全部 PAINT，§3.8）
            retry allocate(content)
  → 第二次仍 None（单块比整页还大）：
            返回 EngineError::AtlasExhausted { kind }     // F-15 #12；该变体随分配器在 P05 追加
```

**v1 不做**：LRU 逐出、多页、页扩容（2× 翻倍）。理由：①`clear + 重建` 是 §9.3 明示的「重排时整体 retire」路径，语义最简；
②LRU 需要 P06/P15 的 tile→源 映射（重放源数据），属真实需求出现后再加；③页扩容在多页坐标系下会引入 `page` 字段
（契约变更）。→ 风险 R5。

**为什么图集不持有 CPU 像素副本**：源像素在 P06 的字形缓存（按 `(glyph, size, scale, font)`）与 P15 的图片缓存里已有；
图集再存一份 = 双倍内存 + 两份真值（重放时以谁为准？）。因此 `AtlasAllocator` 只做**装箱 + 脏行 + 代际**，上传由 P09 从源
缓存读、写进 GPU 纹理。清空/重建时由 P09 触发「P06/P15 重新提供像素」。

### 10.7 采样约定（`contract.md` 条款）

| 项 | 约定 |
|---|---|
| 采样器 | 三平台统一 `linear` + `clamp-to-edge` **在 tile 内**由 UV 保证（UV 不出内容区）；图集整张纹理用 `clamp-to-edge` 防止 X/Y 越界采样到未定义纹素 |
| 1:1 采样 | 字形/1:1 图片必须落在**整数物理像素矩形**（`to_physical`），否则双线性把字形糊化 |
| 放大 | `Image` 可以放大（背景图）；最多 1 物理像素柔和边（§10.2） |
| mip | v1 **不用** mipmap（UI 缩放范围小；mip 会让 1:1 采样选到更高层而糊化）。P18 若发现缩小图片有摩尔纹再评估 |
| `texture_index` | `AtlasKind::index()`：Glyph `0`、Image `1`；与 §8.2 的 `params1.y` 一致 |

---

## 11. `paint/canvas.rs`（应用层唯一公开写入口，裁决 ②）

### 11.1 v1 的 `Canvas` 是什么

`Canvas` 是**应用侧的自绘单位**，也是 P02 里唯一能写入绘制指令的公开类型：内部持有一个 `scene::Lane`
（crate 私有），公开的 `push_*` 方法把引擎图元写进这个 lane。引擎在 P03/P07 把它包成 `Primitive::Canvas` 嵌进真实 lane。

```rust
/// 一组引擎图元的记录容器 + 应用层写入口。值语义（应用构造 → 交给引擎）。
/// 尺寸：`Rect`(16) + `Lane`(32) = **48 B**（见 §2.5 的实测表）。
#[derive(Clone, Debug, PartialEq)]
pub struct Canvas {
    /// 逻辑像素 AABB。契约：**必须** ⊇ 所有内部图元的 bounds 并集（damage / 命中只用它，见 §11.3）。
    bounds: Rect,
    /// 内部绘制桶：复用 `scene::Lane`（**唯一**的图元序列表示，I2；P02 里是 `pub(crate)` 类型）。
    lane: Lane,
}

impl Canvas {
    /// 应用侧的构造入口。初始为空的 lane。
    ///
    /// # Panics: none
    pub fn new(bounds: Rect) -> Self;

    // ── 公开写入口（裁决 ②）──
    /// # Panics: none
    pub fn push_quad(&mut self, quad: Quad);
    /// 已塑形的文本。**应用层无法自行构造 `GlyphRun`**（需要 `AtlasTile`），唯一合法调用者是 P06 的文本 painter；
    /// 应用层要文本请用 `NodeBuilder::text()`（§11.5）。
    ///
    /// # Panics: none
    pub fn push_text(&mut self, run: GlyphRun);
    /// # Panics: none
    pub fn push_image(&mut self, image: Image);
    /// # Panics: none
    pub fn push_path(&mut self, path: Path);
    /// # Panics: none
    pub fn push_shadow(&mut self, shadow: Shadow);
    /// # Panics: none
    pub fn push_geometry(&mut self, draw: GeometryDraw);

    /// 压栈 clip 并返回是否成功：`clip_depth < MAX_CLIP_DEPTH` 时压入并返回 `true`，
    /// 否则**不写入**并返回 `false`（N6：不 panic）。
    ///
    /// # Panics: none
    pub fn push_clip(&mut self, clip: ClipRect) -> bool;
    /// 弹栈并返回是否成功：`clip_depth > 0` 时弹出并返回 `true`，否则返回 `false`（不 panic）。
    ///
    /// # Panics: none
    pub fn pop_clip(&mut self) -> bool;

    // ── 公开只读面 ──
    /// # Panics: none
    pub fn bounds(&self) -> Rect;
    /// # Panics: none
    pub fn primitives(&self) -> &[Primitive];
    /// # Panics: none
    pub fn is_empty(&self) -> bool;
}

/// `bounds` = `Rect::default()`、空 lane；与 `Canvas::new(Rect::default())` 等价（clippy::new_without_default）。
impl Default for Canvas;
```

实现要点（N6）：

```text
push_clip(clip):
    if usize::from(self.lane.clip_depth()) >= usize::from(MAX_CLIP_DEPTH) { return false }   // C3，不写入
    self.lane.push(Primitive::Clip(ClipOp::Push(clip)));
    true

pop_clip():
    if self.lane.clip_depth() == 0 { return false }                                          // C2
    self.lane.push(Primitive::Clip(ClipOp::Pop));
    true
```



### 11.2 所有权与可见性（裁决 ② 的落地）

| 问题 | 契约 |
|------|------|
| 谁可以写绘制指令 | **只有 `Canvas`**（应用层）与 P03 的 paint 循环（crate 内，P03 之后） |
| `Scene` 可写吗 | P02 **没有 `Scene`**（§7.2）；P03 落地后 `Scene` 也**只读**，写入面 `pub(crate)` |
| `Canvas` 为什么持有 `Lane` 而不是 `Scene` | 每个自绘节点拖一张 lane 表 + 绘制序是无边界浪费，也无法表达「这个 canvas 属于哪个节点」；**一个 lane 是两者的最小交集** |
| `Lane` 的可见性 | **`pub(crate)`**（N7）：应用层只能经 `Canvas` 的只读方法看到 `&[Primitive]`，看不到 `Lane` 类型本身 |
| `Canvas` 与 `Lane` 的依赖方向 | `canvas.rs` → `scene.rs`（`Lane`/`MAX_CLIP_DEPTH`）；`scene.rs` 不引用 `canvas.rs`。`primitive.rs` 的 `Primitive::Canvas(Canvas)` 与 `Canvas.lane: Lane` 构成跨模块递归（`Vec` 提供间接），Rust 允许 |
| 生命周期 | `Canvas` 是**值**：应用构造 → 交给引擎 → 引擎在 paint 阶段包成 `Primitive::Canvas`。引擎**不**长期持有 `&mut Canvas` |
| 可变性 | 应用在构造期可变；交给引擎后引擎只读（`primitives()`）。`Primitive: Clone`，帧记录/oracle 可复制 |

### 11.3 `bounds` 契约与不变量

| 规则 | 内容 |
|------|------|
| `bounds` ⊇ 内部图元并集 | **硬契约**。damage 只按 `bounds` 算（不逐图元算），bounds 过小 = 漏像素。P07 的展平期 `debug_assert!` 校验（`Canvas` 图元数通常 < 32，O(n) 可接受） |
| 空 `Canvas` | 合法（不产生实例，但 `bounds` 仍参与 damage —— 保守侧，宁可多画） |
| clip 平衡 | `push_clip`/`pop_clip` 由 `Canvas` 自己维护 `Lane::clip_depth`；未 `pop` 的剩余深度由 P06 展平阶段断言（C1） |
| 嵌套深度 | **≤ 1**：`Canvas` 没有任何接受 `Canvas` 的 push 方法，P07 编码器 `assert!(canvas_depth == 0)`（§8.6） |
| 内部文本 | `push_text` 只接受引擎产出的 `GlyphRun`（§11.5） |

### 11.4 v1 的 `Canvas` 能做什么 / 不能做什么

| v1 **包含** | v1 **不包含**（明确记录，F-29） |
|-------------|-------------------------------|
| 引擎图元的任何组合（含嵌套 clip） | 应用自有 shader / pipeline 注册（D2 不内化外部 shader 工具链） |
| `Geometry` 引用（应用注册的静态几何，P08 起可用） | 任意 GPU 命令插入（§5.9 明令禁止：破坏 I4 与 damage 正确性） |
| 与 lane/clip/damage/CPU 帧记录的完整集成 | 独立渲染目标 / 离屏 pass / compute |
| 单层展平 | 多层嵌套 `Canvas` 的**数据结构**（需要时由生产者展平，不改数据） |

**为什么去掉 §5.9 的「应用注册自有 pipeline」**：①D2 明确不内化外部 shader 工具链；②§5.9 自身禁止「任意 GPU 命令插入」，
而自定义 pipeline 若不能插命令，语义上等价于用引擎图元拼装；③三平台 PSO/资源/延迟销毁是独立工程量，须以 P11/P16/P17 为前提。

### 11.5 `push_text` 的可用性（诚实说明）

`GlyphRun` 需要 `AtlasTile`（图集矩形），只有引擎的 shaping/光栅路径（P06）能产出；应用层**没有**合法构造方式。
因此：①`push_text` 的实际调用者是 **P06 的文本 painter**；②应用层需要文本请用 `NodeBuilder::text()`（P04/P08）；
③该方法之所以是 `pub` 而不是 `pub(crate)`，是为了 `Canvas` 的语义完整性（一个自绘容器应当能放文本）与 API 自洽；
④**不**给应用层构造 `GlyphRun` 的能力，是刻意边界。

### 11.6 扩展路径

后续若要做自定义 draw call，additive 加：

```rust
pub struct CustomDraw { pipeline: CustomPipelineId, buffers: …, uniform: … }
impl Canvas { pub fn push_custom(&mut self, draw: CustomDraw); }
pub enum GeometryPipeline { …, Custom { pso: CustomPipelineId } }
```

数据契约的**现有字段不需要改**（这正是把 `Canvas` 设计成「lane 片段」的原因：v1 与未来共用同一条 `Primitive` 序列）。
`kind` 的保留值 6/7 预留给未来的自定义实例。
## 12. damage（`render/damage.rs`，**P06 落地**，裁决 ①）

### 12.1 输入与职责

damage 累加器是**纯 CPU、纯函数式**的：给定「逻辑矩形 + 外扩量 + clip + 帧尺寸」，产出平台无关的物理像素脏矩形集合。
它**不知道** GPU、不知道 `Present1`、不知道 16 个 rect 的硬件上限（§5.7 的「count > 16」是引擎自定的成本阈值）。

文件位置与 P01 §4.1 一致（`render/damage.rs`，crate 私有）；落地 Phase 由裁决 ① 定为 **P06**（首个消费者 = paint 之后的
damage 步骤）。P09 的渲染后端只做**平台翻译**（`Present1` dirty rects / `VK_KHR_incremental_present` / scroll hint），
消费**同一个** `DamageOutput`，不重算区域。

```rust
/// 平台无关的 damage 结果。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DamageOutput {
    /// 全帧重绘（阈值逃逸 / 窗口 resize / DPI 变化 / 主题切换）。
    Full,
    /// 局部重绘。`rects` 为空 = **本帧没有任何需要重绘的像素**（反向证明空闲帧，I3）。
    Partial { rects: Vec<PhysicalRect>, scroll: Option<ScrollHint> },
}

/// 滚动优化提示（§5.7）：`rect` 内的像素整体平移 `offset` 后仍然有效。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScrollHint { pub rect: PhysicalRect, pub offset: (i32, i32) }

pub(crate) struct DamageAccumulator { /* scale, rects, full, scroll */ }

impl DamageAccumulator {
    pub(crate) fn new(scale: Scale) -> Self;
    pub(crate) fn reset(&mut self, scale: Scale);

    /// 累加一个**逻辑**矩形。`aa_padding` 是**物理像素**外扩量（§12.3 的取值表）。
    pub(crate) fn push(&mut self, bounds: Rect, aa_padding: f32, clip: Option<PhysicalRect>);

    /// 强制全帧（resize / DPI / 主题 / 字体切换）。
    pub(crate) fn push_full(&mut self);

    /// 记录滚动（只对 `Partial` 有意义；多次调用取最后一次）。
    pub(crate) fn push_scroll(&mut self, rect: PhysicalRect, offset: (i32, i32));

    /// 合并 + 阈值判定。`frame` 是本帧 framebuffer 的物理矩形（算面积阈值用）。
    pub(crate) fn finish(&mut self, frame: PhysicalRect) -> DamageOutput;
}

pub(crate) const MAX_DAMAGE_RECTS: usize = 16;
pub(crate) const FULL_FRAME_AREA_RATIO: f32 = 0.5;
```

### 12.2 算法（逐字）

```text
push(bounds, aa_padding, clip):
  1. if bounds 不可换算（P01 §6.3.3：非有限 / 负尺寸）→ return          // 安全失败，不产生垃圾 rect
  2. r = bounds.to_physical_covering(scale)                              // 向外 floor/ceil（安全侧）
  3. pad = ceil(max(aa_padding, 0.0))                                    // 物理像素，整数化
  4. r = (r.x0 - pad, r.y0 - pad, r.x1 + pad, r.y1 + pad)
  5. if let Some(c) = clip { r = r ∩ c; if r.is_empty() { return } }      // 与父 clip 求交
  6. if r.is_empty() { return }
  7. if 已有 rect 与 r 重合（`union_area <= a.area + b.area`）→ 原地 union   // 增量合并，避免列表膨胀
     else rects.push(r)
  8. if rects.len() > MAX_DAMAGE_RECTS → full = true                      // 提前逃逸

finish(frame):
  1. if full → Full
  2. 循环合并直到不动点（见 §12.4），每轮按 (y0, x0) 排序保证确定性
  3. if rects.len() > MAX_DAMAGE_RECTS → Full
  4. total = Σ area(rects)；if total as f32 > frame.area() as f32 * FULL_FRAME_AREA_RATIO → Full
  5. scroll 的处理（N3）：**仅当所有 rects 都落在 scroll.rect 内**时保留 scroll hint；否则**放弃 scroll 优化**，
     保留全部 rects 退化为普通 `Partial`（不丢弃任何 dirty rect，避免漏重绘）
  6. Partial { rects, scroll }
```

### 12.3 外扩量的取值表（**唯一**来源，与 `params0.w` 一致）

| 图元/来源 | `aa_padding`（物理像素） | 依据 |
|---|---|---|
| `Quad`（含边框/渐变） | `1.0` | P01 §7.2：SDF AA 的 1 px 边缘 |
| `Clip` 的 mask 边界 | **不单独累加** | N13：clip 本身**不产生像素**，它只把被裁剪图元的可见范围收窄；damage 来自被裁剪图元自己的 bounds，因此 `Clip` 不 push damage（其 `rect` 已在图元 push 时通过 `clip` 参数求交） |
| `Path` / `Geometry` | `1.0` | 边缘像素的抗锯齿需要 1 px（v1 无 AA，但保守侧） |
| `Shadow` | `blur × s + 1.0` | 可见范围 = 模糊半径 + 1 px（§5.7） |
| `GlyphRun` / `Image` | `0.0` | 图集四边形是整数像素，不外扩（外扩会拖入 padding 的透明像素） |
| 节点布局变化（无图元） | `1.0` | 保守：布局盒本身可能有 1 px 的 SDF 边缘 |

**为什么 pad 在**物理**像素域 ceil 而不是逻辑域 `Rect::outset`**：P01 §7.2 已定——逻辑外扩在分数缩放下无法保证覆盖整像素；
`covering` 先把逻辑盒映射到像素网格，再按物理像素外扩，才与 shader 的 `params0.w` 语义一致。

### 12.4 合并规则（确定性的「面积不增」判据）

两个物理矩形 `a`、`b` 合并成 `u = a ∪ b` 当且仅当：

```text
area(u) <= area(a) + area(b)        // 严格成立：相交或**共边**时等号/小于；L 形时严格大于 → 不合并
```

| 性质 | 说明 |
|---|---|
| 覆盖「相交」 | `a ∩ b` 非空时 `area(u) < area(a) + area(b)` |
| 覆盖「相邻」 | 共边（gap = 0）时 `area(u) == area(a) + area(b)` |
| 拒绝 L 形膨胀 | 只在一个角相邻时 `area(u) > 和`，拒绝 —— 否则两个小脏块会合并成大块，抵消局部重绘收益 |
| 确定性 | 每轮先按 `(y0, x0)` 升序排序再两两扫描，直到不动点；同一输入任何平台得到同一输出（I8 的前提） |
| 复杂度 | O(n²) 但 n ≤ 17（超 16 立即 Full），可忽略 |

### 12.5 阈值与全帧退化

| 条件 | 结果 | 理由 |
|---|---|---|
| `rects.is_empty()` | `Partial { rects: [] }` | 显式表达「无脏区」；§3.5 的空闲帧判定用它（**不**返回 `Full`） |
| `rects.len() > 16` | `Full` | §5.7 的硬阈值；三平台的 dirty-rect 列表长度为 16/32 不等，16 是安全值 |
| `Σarea > frame.area × 0.5` | `Full` | 超过半屏时全帧拷贝更便宜（三平台一致的经验值）；P18 可重测后调整常量 |
| `push_full()` 被调用 | `Full` | resize / DPI / 主题 / 字体切换 |
| `scroll` 且**全部** dirty rect 都在 `scroll.rect` 内 | `Partial { rects: [...], scroll: Some(..) }` | Windows 走 `Present1` 的 scroll hint；其余平台退化为全帧拷贝（§5.7 末段） |
| `scroll` + 区外脏区同时存在 | `Partial { rects: [...], scroll: None }` | N3：**任何一个** rect 落在 scroll 区外就放弃 scroll，保留全部 rects（宁可多画，不能漏画） |

### 12.6 可测性（纯函数契约）

| 要求 | 内容 |
|---|---|
| 无全局状态 | `DamageAccumulator` 只用自身字段；`Scale`/`frame` 都是参数 |
| 无 GPU/平台依赖 | 不出现任何 `render`/平台类型；`PhysicalRect` 来自 `math`（F-09） |
| 确定性 | 合并顺序、排序、阈值判定都按本节的字面规则；同一输入 → 同一输出（单测按字面值断言） |
| 可复用 | `reset(scale)` 保留 `Vec` 容量（§9.3 的「不跨档翻倍」） |

---

## 13. CPU 帧记录可观测性（I12）——P06 落地前必须定死的只读面

### 13.1 两层验证的分工（也是 §3.2 的直接后果）

| 层 | 断言对象 | 测试形态 | 是否需要 GPU |
|---|---|---|---|
| **L2 指令层**（公开面） | P02：`Canvas` 的 `primitives()` 序列 + `Primitive` 全部字段；P03 起追加 `Scene`（lane 顺序 + `LaneId` 复用） | `paint/*` 的 `#[cfg(test)] mod tests`（P02）+ `tests/frame_record.rs`（P07） | 否 |
| **L2 字节层**（crate 内部） | `GpuPrimitive` 的 `kind`/`flags`/`uv`/`bounds`/`params*` 精确值、clip 表、`FrameOp` 顺序 | `paint/instance.rs` 的 `#[cfg(test)] mod tests` | 否 |
| **L3 像素层** | 视觉 golden | `tests/snapshot.rs`（P18，需 test-only device） | 是 |

> 关键设计后果：**应用层的帧记录不需要 `GpuPrimitive`**。指令层的断言用 `Primitive`/`LaneId`/`PhysicalRect` 就够；
> 字节层由同模块单测覆盖（AGENTS §5.1：unit test 可访问私有字段）。因此 §1.3 的公开面**不需要**为 I12 扩容 ——
> 这也是 §3.2 选择「instance 留在 crate 私有」的前提。

### 13.2 P02 必须公开的只读面（逐类型）

| 类型 | 必须公开的成员 | 消费方 |
|---|---|---|
| `Canvas` | `bounds()`、`primitives()`、`is_empty()`（**字段私有**：要维持 `bounds ⊇ 子图元` 的契约，且内部持有 `pub(crate)` 的 `Lane`） | P06 帧记录、P07 展平 |
| `Primitive` + 8 类载荷 | 全部变体/**pub 字段** + `Clone + Debug + PartialEq` | P06 `primitives: Vec<Primitive>`、§3.8 oracle |
| `Lane` / `MAX_CLIP_DEPTH` | **不公开**（`pub(crate)`）：应用层经 `Canvas` 的只读方法间接观测 | — |
| `Scene` / `LaneId` | **P03 起**：`lane`/`lane_count`/`order`/`iter`/`primitive_count`/`is_empty` + `Copy+Eq+Hash` | P06 `reused_lanes`、P07 |
| `AtlasTile` | pub 字段 + `uv()` + `is_stale()` + `Copy + Debug + PartialEq` | P06 断言字形落点、P15 持有 |
| `Color` / `LinearPremulRgba` | pub 字段/分量 + `PartialEq` + `ColorResolver::resolve`（公开） | §6.3 示例 `assert_eq!(frame.quads[0].background, theme::surface().resolve())` 的 `resolve` 侧 |
| `GeometryId` / `GeometryDesc` | id 可比较；desc 的 `bounds`/`pipeline`/`stride` 可读 | P09 校验、P06 断言「注册了什么」 |

**不公开**（P06 通过同模块/`pub(crate)` 访问，或**不需要**）：

| 项 | 原因 |
|---|---|
| `GpuPrimitive` / `InstanceBuffer` / `FrameOp` / `ClipChain` | §3.2 + §1.3；字节层断言用同模块单测 |
| `DamageAccumulator` / `DamageOutput` / `ScrollHint` | 同上；对应用层只暴露 `FrameRecord.damage: Vec<PhysicalRect>`（P06 转换） |
| `AtlasAllocator` / `Allocation` / `DirtyRows` | P09 内部 |
| `LaneId` 的字段 | 只允许引擎铸出（§7.1） |

### 13.3 需要「值可比较」的派生清单（编译器会强制）

| 类型 | `Clone` | `Copy` | `Debug` | `PartialEq` | `Eq`/`Hash` | 依据 |
|---|---|---|---|---|---|---|
| `Color` | ✅ | ✅ | ✅ | ✅ | — | oracle 比较 |
| `LinearPremulRgba` | ✅ | ✅ | ✅ | ✅ | — | 同上 |
| `Interpolation` / `HueDirection` | ✅ | ✅ | ✅ | ✅ | ✅ | 缓存 key / 断言 |
| `ColorResolver` / `ColorBrush` | ✅ | ✅ | ✅ | ✅ | — / Eq 可选 | `Brush` 要求 `Clone+PartialEq+Default+Debug` |
| `Quad` / `QuadPaint` / `Gradient` | ✅ | — / ✅ / ✅ | ✅ | ✅ | — | oracle |
| `Border` / `GradientKind` | ✅ | ✅ | ✅ | ✅ | — / ✅ | oracle |
| `GlyphRun` / `PositionedGlyph` | ✅ | — / ✅ | ✅ | ✅ | — | oracle |
| `Image` / `Path` / `Shadow` / `GeometryDraw` | ✅ | ✅（后三者 Copy；`Image` Copy） | ✅ | ✅ | — | oracle |
| `ClipOp` / `ClipRect` | ✅ | ✅ | ✅ | ✅ | — | oracle |
| `Canvas` | ✅ | — | ✅ | ✅ | — | oracle；**字段私有**（`bounds`/`lane`），只读面 = `bounds()`/`primitives()`/`is_empty()` |
| `Primitive` | ✅ | — | ✅ | ✅ | — | oracle + 帧记录 |
| `AtlasKind` / `AtlasTile` | ✅ | ✅ | ✅ | ✅ | ✅ / — | 图元内嵌 + 断言 |
| `GeometryId` / `IndexType` / `Topology` / `GeometryPipeline` | ✅ | ✅ | ✅ | ✅ | ✅ | 断言 / 容器 key |
| `LaneId` | ✅ | ✅ | ✅ | ✅ | ✅ | `reused_lanes` 断言 |
| `Lane`（`pub(crate)`） | ✅ | — | ✅ | ✅ | — | `Primitive::Canvas` 内嵌 `Canvas`→`Lane`，因此 `Lane: Clone` 是**前置条件**；`Default` 供 `Canvas::default` |

---

## 14. 测试清单（每文件）

> 组织：除 `tests/` 外全部内联在被测文件末尾（`#[cfg(test)] mod tests`，AGENTS §5.1 / P01 §4.3c）。
> P02 落地部分的测试是**可立即通过**的；标注 `P05`/`P06`/`P07` 的行属于延后落地文件的测试，随实现一起提交。

### 14.1 `paint/color.rs`（**28** 个用例）

| # | 测试 | 断言要点 |
|---|---|---|
| 1 | `resolve_srgb_is_linear_buffer` | `Srgb(0.5)` → 线性 ≈ `0.21404114`（sRGB EOTF），alpha 保持 1 |
| 2 | `resolve_alpha_is_clamped` | alpha `-1`/`2`/`NaN` → `0`/`1`/`0` |
| 3 | `resolve_premultiplies` | `Srgb(1,1,1,0.5)` → rgb `0.5`、alpha `0.5` |
| 4 | `linear_premul_opacity_flags` | `is_opaque`/`is_transparent` 的边界（`0.999` 不是不透明；`0` 是透明） |
| 5 | `opacity_mul_scales_all_components` | `Mul<f32>` 在预乘空间同乘 4 分量 |
| 6 | `resolve_linear_srgb_variant_is_passthrough` | `LinearSrgb` 只 clamp、不做 EOTF；`1.5`/`-1.0` → `1.0`/`0.0` |
| 7 | `resolve_oklab_variant` | `Oklab(0.5,0,0)` → 线性 ≈ **0.125**（L 是亮度的立方根，独立期望） |
| 8 | `oklch_in_gamut_is_passthrough` | 与**直接用 `color` crate** 的转换结果一致（不经过本模块 helper） |
| 9 | `oklch_out_of_gamut_reduces_chroma` | `Oklch(0.9,0.40,140)` → 三分量 ∈ `[0,1]` 且有效 chroma < 0.40 |
| 10 | `gamut_mapping_is_deterministic` | 同输入两次 `resolve` 逐位相等；二分步数 == 24 |
| 11 | `oklch_lightness_clamped` | `L=-0.5` → 近黑；`L=1.5` → 近白 |
| 12 | `display_p3_red_maps_in_gamut` | P3 红 → 三分量 ∈ `[0,1]`，`r >= 0.99` |
| 13 | `interpolate_premultiplied_no_color_leak` | 透明红 → 不透蓝，`t=0` 时 alpha 与 rgb 都归零 |
| 14 | `interpolate_clamps_t` | `t=-1`/`2` 与 `t=0`/`1` **逐位相同**（不外插） |
| 15 | `interpolate_oklab_lightness_monotone` | 黑→白在 Oklab 下 R 通道**严格**递增 |
| 16 | `interpolate_linear_srgb_space` | 黑→白在 `LinearSrgb` 空间 `t=0.5` → 三分量均 `0.5` |
| 17 | **`interpolate_endpoints_match_resolve`**（B1 回归） | 4 种插值空间 × 4 个端点色（P3/Srgb/Oklch，含 0.6 alpha）× `t=0/1`：结果与 `resolve` 差 `< 1e-5` |
| 18 | **`linear_srgb_space_matches_resolve_for_p3_endpoint`**（B1 回归） | 修复前 `[0, 0.8201, 0]` vs 修复后与 `resolve` 差 `< 1e-6` |
| 19 | `p3_green_midpoint_is_gamut_mapped` | 独立字面量 `[0, 0.3689881, 0.04283759]`（§5.5 的 (B) 路径）；容差 1e-4 |
| 20 | `p3_blue_midpoint_is_gamut_mapped` | 独立字面量 `[0, 0.02564238, 0.42373744]`；容差 1e-4 |
| 21 | `hue_direction_longer_midpoint_is_opposite` | `Oklch(350)→Oklch(10)`：`Longer` 中点色相 ≈ 180°，且与 `Shorter` 中点不同 |
| 22 | `hue_direction_increasing_and_decreasing` | `10→350`：`Increasing` 中点 ≈ 180°，`Decreasing` 中点 ≈ 0°（两者不可互换） |
| 23 | `hue_extremes_are_normalized` | `h = 1e9` 与 `1e9.rem_euclid(360)` **逐位相同**；`hue_deg(±inf) == 0` |
| 24 | `defaults_are_explicit` | `Color::default()==TRANSPARENT`；`ColorBrush::default()==BLACK`；`Interpolation::default()==Oklab`；`HueDirection::default()==Shorter` |
| 25 | `size_budget` | `Color` 20 B、`LinearPremulRgba` 16 B |
| 26 | `clamp_01_handles_nan` | `NaN→0`、`2→1`、`-1→0`（N4 逐字实现） |
| 27 | `clamp_nonneg_handles_infinity` | `±inf`/`NaN` → `0`，负数 → `0`，正数保留（N1） |
| 28 | `alpha_color_roundtrip_is_stable` | **依赖 sanity**（非本模块契约）：`AlphaColor` 往返保持分量 |

### 14.2 `paint/primitive.rs`（**6** 个用例）

| # | 测试 | 断言要点 |
|---|---|---|
| 1 | `all_variants_clone_eq_and_debug` | **8 个变体**（含 `Canvas`）各构造一次：`clone() == 原值`、`Debug` 含变体名；改一个载荷字段必须破坏相等（字段往返） |
| 2 | `size_budget` | `size_of::<Primitive>() == 80` 且 `<= 128`（软上限） |
| 3 | `quad_paint_encodings_are_mutually_exclusive` | 纯色/带边框/渐变三者互不相等；`Debug` 含 `Solid`/`Gradient`；`match` 穷尽 |
| 4 | `shadow_keeps_unoffset_rect` | `Shadow{rect, offset}` 的 `rect` 不因 `offset` 改变（偏移只在编码时应用） |
| 5 | `gradient_kind_has_three_distinct_variants` | 三个变体两两不等 + `Debug` 名唯一（`Radial` 字面量） |
| 6 | `clip_op_push_pop_roundtrip` | `Push(c) == Push(c)`；`Pop != Push(c)`；**圆角不同即不等**；`Debug` 含 `Push`/`Pop` |
### 14.3 `paint/scene.rs`（P02 只含 `Lane`，`pub(crate)`；**6** 个用例）

> P02 的 `scene.rs` **只有** `Lane` 与 `MAX_CLIP_DEPTH`（均 `pub(crate)`，模块声明为 `pub(crate) mod scene;`）；
> `Scene` / `LaneId` / lane 分配与绘制序**整体推迟 P03**（B1/B2/B3/N11，§19）。`Lane` 的写入面只由 `Canvas` 消费；
> C1（lane 末尾平衡）由**引擎展平阶段（P06）**的 `debug_assert` 断言；P03 的 `end_lane` 只做 lane 平衡的显式收尾，不做断言。P02 只保证 C2/C3 的 **debug 断言**。

| # | 测试 | 断言要点 | 落地 |
|---|---|---|---|
| 1 | `new_lane_is_empty` | `Lane::new()` 后 `is_empty()` 为真、`primitives().is_empty()`、`clip_depth() == 0` | P02 |
| 2 | `push_preserves_order` | push Quad/Image/Shadow → `primitives()` 顺序 = 调用顺序 | P02 |
| 3 | `push_tracks_clip_depth` | `Clip(Push)` → 1；再 `Clip(Pop)` → 0；`primitives().len() == 2` | P02 |
| 4 | `pop_underflow_is_debug_assert` | 空 lane 上 push `Clip(Pop)` → **debug 构建 panic**（`#[should_panic]` + `#[cfg(debug_assertions)]`，N12） | P02 |
| 5 | `depth_cap_is_debug_assert` | 连续 `MAX_CLIP_DEPTH` 次 `Clip(Push)` 后再 push → **debug 构建 panic**（同上标注） | P02 |
| 6 | `max_clip_depth_is_u8_8` | `MAX_CLIP_DEPTH == 8u8`（**类型与值都是契约**；改它必须同步 P10 三套 shader 的数组长度） | P02 |

### 14.4 `paint/canvas.rs`（**11** 个用例）

> N6：`push_clip`/`pop_clip` **返回 `bool`**（超深/空栈返回 `false`，不 panic）；`Canvas` 的所有公开方法
> 文档都写 `# Panics: none`。平衡性由引擎展平阶段断言（P06），不是 `Canvas` 的职责。

| # | 测试 | 断言要点 |
|---|---|---|
| 1 | `new_canvas_is_empty` | `is_empty()` 为真、`primitives().is_empty()`、`bounds()` 等于入参 |
| 2 | `default_canvas_is_empty` | `Canvas::default()` == `Canvas::new(Rect::default())` |
| 3 | `push_orders_primitives` | 依次 push quad/image/shadow → `primitives()` 顺序与调用顺序一致 |
| 4 | `clip_push_pop_balance` | `push_clip`/`pop_clip` 都返回 `true`；平衡后 `primitives().len() == 3` |
| 5 | `push_clip_beyond_limit_returns_false` | 前 8 次 `true`、第 9 次 `false` 且 `primitives().len()` **不增加**（C3） |
| 6 | `pop_clip_on_empty_returns_false` | 空栈 `pop_clip() == false`（C2，不 panic） |
| 7 | `push_text_keeps_run` | **全字段**保留：`run` 整体相等 + color/tile/position/size 逐项断言（N12） |
| 8 | `canvas_is_clone_and_eq` | `clone() == 原值`（`Primitive::Canvas` 的 `PartialEq` 依赖它） |
| 9 | `canvas_size_is_48_bytes` | `size_of::<Canvas>() == 48`（`Rect` 16 + `Lane` 32；lane 增大会抬高每个自绘节点成本） |
| 10 | `push_image_rejects_glyph_atlas` | debug 构建：`Image` 用了 Glyph 图集 → panic（N11；`#[cfg(debug_assertions)]`） |
| 11 | `push_clip_rejects_negative_size` | debug 构建：负尺寸 `ClipRect` → panic（C4；`#[cfg(debug_assertions)]`） |
### 14.5 `paint/geometry.rs`（**5** 个用例）

| # | 测试 | 断言要点 |
|---|---|---|
| 1 | `index_type_sizes` | `U16.size() == 2`、`U32.size() == 4` |
| 2 | `pipeline_strides` | `Ui.vertex_stride() == 8`、`Textured{..}.vertex_stride() == 16` |
| 3 | `textured_pipeline_carries_atlas` | `Textured { atlas: AtlasKind::Image }.vertex_stride() == 16`，且 `match` 能取回 `atlas` |
| 4 | `geometry_id_is_hashable_and_reads_back` | `index()`/`generation()` 读回构造值；进 `ahash::HashSet`（P09 的 retire 集合同款）；代际不同即不同句柄 |
| 5 | `desc_debug_contains_fields` | `format!("{:?}", desc)` 含 `stride`/`pipeline` 字样（帧记录可读性） |

### 14.6 `paint/atlas.rs`（**4** 个用例）

| # | 测试 | 断言要点 |
|---|---|---|
| 1 | `atlas_kind_index_matches_texture_slots` | `Glyph.index() == 0`、`Image.index() == 1`（与 §8.3 的 `TEXTURE_*` 常量一致） |
| 2 | `tile_uv_order` | `uv() == [u0, v0, u1, v1]`；`u0 < u1`、`v0 < v1` 的构造用例 |
| 3 | `tile_staleness` | `tile.generation == g` 时 `is_stale(g)` 为假；`g + 1` 时真 |
| 4 | `size_budget` | 记录 `size_of::<AtlasTile>()`（实测 **24**：`AtlasKind` 1 B + 4×f32 + u32 + 对齐），`size_of::<AtlasKind>() == 1` |

### 14.7 `paint/instance.rs`（P07）

| # | 测试 | 断言要点 |
|---|---|---|
| 1 | `layout_is_128_by_16` | 静态断言已在 §8.1；再加一条运行时 `assert_eq!` 让失败信息更可读 |
| 2 | `params1_is_integer_encoded` | `0.0/1.0/2.0/3.0` 与 `as u32 as f32` 往返一致；`2^24` 以上被拒绝（编码器 assert） |
| 3 | `quad_solid_mapping` | 逐字段断言 `bounds/corner_radius/color0/color1/params*/uv/clip_rect` |
| 4 | `quad_border_uses_color1` | `border.width > 0` → `params0.x > 0` 且 `color1 == resolve(border.color)` |
| 5 | `quad_gradient_kinds` | 三种 kind → `params0.z ∈ {1,2,3}`，`color1 == resolve(to)` |
| 6 | `atlas_bounds_are_integer` | `Image`/`Glyph` 的 `bounds` 四分量都是整数值（`fract() == 0`） |
| 7 | `glyph_run_is_one_to_n` | 3 个字形 → 3 个实例，`uv`/`bounds` 各不相同，`texture_index == 0` |
| 8 | `shadow_aa_padding_is_blur_plus_one` | `params0.y == blur*s`、`params0.w == blur*s + 1.0`、`bounds == f32r(rect.translate(offset))` |
| 9 | `skip_transparent_and_empty` | 透明 Quad / 空 bounds / 非有限 bounds 都不产生实例 |
| 10 | `clip_chain_dedup` | 两个 lane 的相同 clip 序列 → 共用同一 `clip_index`；嵌套 2 层 → `parent` 指向正确 |
| 11 | `frame_ops_preserve_order` | `Quad → Geometry → Quad` → `[Instances, Geometry, Instances]`，且 `start/len` 连续 |
| 12 | `opaque_flag` | 不透明纯色 Quad 带 `FLAG_OPAQUE`；带渐变/边框半透明/Shadow 不带 |
| 13 | `as_bytes_len` | `as_bytes().len() == len() * 128` |
| 14 | `encode_is_idempotent` | 同一 Scene 连续编码两次 → 字节完全一致（帧记录可复现） |

### 14.8 `render/damage.rs`（P06）

| # | 测试 | 断言要点 |
|---|---|---|
| 1 | `push_uses_covering_not_round` | 逻辑 `(10.5, 20.25, 0.25, 0.25)` @s=2：`covering = (21,40,22,41)`，round 会得到零高 → 断言前者的四边 |
| 2 | `push_expands_by_physical_padding` | `pad = 2.0` → 四边各外扩 2 |
| 3 | `push_intersects_clip` | clip 只覆盖一半 → 结果落在 clip 内 |
| 4 | `push_rejects_non_finite_and_negative` | NaN / ∞ / 负尺寸 → 不产生 rect，不 panic |
| 5 | `merge_overlapping` | 两个相交矩形 → 一个 |
| 6 | `merge_adjacent` | 共边（gap = 0）→ 一个 |
| 7 | `reject_l_shaped_merge` | 只在角相邻的两个矩形**不**合并（`area(u) > sum`） |
| 8 | `merge_is_order_independent` | 同一集合的两种插入顺序 → 相同输出（排序保证） |
| 9 | `too_many_rects_becomes_full` | 17 个互不相交的小矩形 → `Full` |
| 10 | `area_ratio_becomes_full` | 覆盖 > 50% 帧面积 → `Full` |
| 11 | `empty_stays_partial_empty` | 无输入 → `Partial { rects: [] }`（不是 `Full`） |
| 12 | `scroll_hint_kept_only_for_partial` | `push_scroll` + `Partial` → `scroll.is_some()`；`push_full` 后 → `Full`（scroll 丢弃） |
| 13 | `reset_reuses_capacity` | `reset` 后 `finish` 行为与新建一致，`Vec` 容量不回落到 0（用 `capacity()` 断言） |
| 14 | `deterministic_output_order` | 同输入两次 → `rects` 逐项相等且顺序一致 |

### 14.9 `render/atlas.rs`（P05）

| # | 测试 | 断言要点 |
|---|---|---|
| 1 | `allocate_adds_padding` | 请求 `8×8` → `padded == 10×10`、`content` 比 `padded` 各边内缩 1 |
| 2 | `allocate_rejects_zero_and_oversize` | `(0,8)`、`(page_w+1, 8)`、`(70000, 8)` → `None` |
| 3 | `uv_excludes_padding` | `content` 与 `uv` 换算一致（`u0*W == content.x0`，容差 1e-6） |
| 4 | `deallocate_is_idempotent` | 两次 `deallocate(同 id)` 不 panic（第二次被记录表拦截） |
| 5 | `clear_bumps_generation` | `clear()` 后 `generation + 1`，旧 tile `is_stale` 为真 |
| 6 | `dirty_rows_merge` | 相邻/重叠区间合并；`spans > 8` → `is_full()` |
| 7 | `allocate_after_clear_reuses_space` | 清空后可重新分配同样大小 |

### 14.10 集成测试（P07 起）

| 文件 | 内容 |
|---|---|
| `tests/frame_record.rs`（P07） | 合成场景 → `FrameRecord`：lane 复用（`reused_lanes`）、`primitives` 顺序与种类、`damage` 局部性（§6.3 的断言示例） |
| `tests/snapshot.rs`（P18） | 三平台像素 golden；`GpuPrimitive`/`Color`/`params` 任何变更必须更新 |

**像素 golden 覆盖矩阵必须显式标注以下 v1 限制**（裁决 ③ 要求，避免把「v1 的已知取舍」当成回归 bug）：

| # | 限制 | 对 golden 的影响 |
|---|------|------------------|
| 1 | `Path` **无 AA**（硬边三角形） | 路径填充的斜边与曲线边缘出现锯齿；容差按「硬边渲染」设置，**不得**用 AA 期望值 |
| 2 | 渐变在 **linear premultiplied** 空间插值（不是 OKLab） | 深色主题下的双色渐变中点色与 OKLab 参考值有可见差异 |
| 3 | `Ui` geometry 只有 **per-draw 常量色**（无逐顶点色/渐变） | 网格内颜色均匀；需要渐变时用 `Quad`/拆 `Path` |
| 4 | 图集 tile 放大时最外 1 物理像素与 padding 混合 | 放大图片边缘有 ≤1 px 的柔和边（§10.2） |
| 5 | 「边框 + 渐变」= 两个 `Quad`（§8.5） | 两者都半透明时边框区域经历两次 premultiplied over（与 CSS 一致） |

---

## 15. 验收命令

```powershell
# 1) 单元测试（P02 落地部分必须全绿）
cargo test -p kytrix_gui

# 2) clippy（workspace lint 由 [lints] workspace = true 继承，含 pub_use / definition_in_module_root /
#    multiple_inherent_impl / field_scoped_visibility_modifiers / inline_modules）
cargo +nightly clippy -p kytrix_gui --all-targets -- -D warnings

# 3) 格式
cargo +nightly fmt -p kytrix_gui -- --check

# 4) 模块根形态：paint.rs 只允许 pub mod 行（不得有 item、无 pub use）
Get-Content crates\kytrix_gui\src\paint.rs
#   期望恰好 6 行 `pub mod ...;`（顺序无关）

# 5) lib.rs 只追加一行
Get-Content crates\kytrix_gui\src\lib.rs
#   期望 `pub mod error; pub mod math;` + `pub mod paint;`（P02 不出现 `mod render;`，见 §3.2）

# 6) I15 隔离：`color` crate 只能在 paint/color.rs 出现
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs | Select-String -CaseSensitive -Pattern 'use color::' |
  Where-Object { $_.Path -notmatch 'paint\\color\.rs' }
#   期望无输出

# 7) 零 unsafe / 零 allow / 零 expect
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs | Select-String -Pattern '\bunsafe\b|#\[allow|#\[expect'
#   期望无输出

# 7b) P02 落地边界（裁决 ①）：不得出现 render/ 与延后文件
Select-String -Path crates\kytrix_gui\src\lib.rs -Pattern 'mod render'
#   期望无输出
Test-Path crates\kytrix_gui\src\render.rs, crates\kytrix_gui\src\paint\instance.rs, crates\kytrix_gui\src\paint\damage.rs
#   期望全部 False

# 7c) Canvas 写入口（裁决 ②）：Scene 无公开可变方法，Canvas 提供 push_*
Select-String -Path crates\kytrix_gui\src\paint\scene.rs -Pattern 'pub fn (new_lane|release_lane|lane_mut|set_order|clear_frame)'
#   期望无输出（这些应为 pub(crate)，且 P02 不落地）
Select-String -Path crates\kytrix_gui\src\paint\canvas.rs -Pattern 'pub fn push_(quad|text|image|path|shadow|geometry|clip)'
#   期望 7 行（Nice-to-have：确认写入口齐全）

# 8) 依赖增量（§0 第 12 条）
Select-String -Path crates\kytrix_gui\Cargo.toml -Pattern '^bytes\s*='
#   期望一行 `bytes = { workspace = true }`

# 9) 无新增依赖（P02 不引入任何 workspace 之外的 crate）
cargo tree -p kytrix_gui -e normal --depth 1

# 10) 公开面自检（类型必须都在 §4.2 的表里）
cargo doc -p kytrix_gui --no-deps

# 11) 落地文件边界（N14：用 Get-ChildItem，不用通配路径）
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs | Select-Object FullName
#   期望只有 lib.rs / error.rs / math.rs / paint.rs / paint{color,primitive,canvas,geometry,atlas,scene}.rs
Get-ChildItem crates/kytrix_gui/src -Recurse -Filter *.rs |
  Where-Object { $_.Name -in @('instance.rs','damage.rs','render.rs','mod.rs') }
#   期望无输出

# 12) 文档完整性复核（B4：防止表格/运算符在编辑中被降级）
$doc = Get-Content -Raw design/phase-02-paint-foundation.md
#   12a) 表格管道符总数（当前基线 **3147**；显著下降即表示表格被破坏）
([regex]::Matches($doc, '\|')).Count
#   12b) 三个必须成对出现的运算符字面量（期望 1 / 1 / 2；12b-3 在 §7.1 出现两次）
([regex]::Matches($doc, 'kind != QUAD \|\| border_width == 0\.0 \|\| color1\.is_opaque\(\)')).Count
([regex]::Matches($doc, 'content\.0 == 0 \|\| content\.1 == 0')).Count
([regex]::Matches($doc, 'usize::from\(self\.clip_depth\(\)\) < usize::from\(MAX_CLIP_DEPTH\)')).Count   # 期望 2
#   12c) 双空格伪算子扫描（代码围栏内不应出现「标识符 + 2 空格 + 标识符」）
Get-ChildItem design/phase-02-paint-foundation.md |
  Select-String -Pattern '\S  +\S' |
  Where-Object { $_.Line -notmatch '\|' -and $_.Line -notmatch '^\s*//' -and $_.Line -notmatch '^\s*#' }
#   期望：只剩对齐用的注释列（人工确认无运算符缺失）
```

**P02 的「完成」判据**：1–11 全绿 + `cargo doc` 无警告 + 12 的三条计数与基线一致。
> 注意：**不是**「`paint::*` 全部有文档注释」——公开项的 `missing_docs` 债务已登记为 **V-22**（数量见该条），本阶段不要求补齐。

---

## 16. 风险

| # | 风险 | 等级 | 对策 / 触发条件 |
|---|---|---|---|
| **R1** | ~~落地顺序与 `-D warnings` 的冲突~~ | **已关闭（裁决 ①）** | P02 只落地 7 个文件；`instance`→P07、`render/damage`→P06、`render/atlas`+`render.rs`→P05（§3.2、§19）。新增文件时必须先确认「非 test 消费者」存在 |
| **R2** | **v1 的 `Path` 无 AA**（裁决 ③ 接受）：大面积矢量填充边缘锯齿 | 中 | 小尺寸矢量走构建期图集（§5.6）始终有 AA；**像素 golden 覆盖矩阵必须标注该限制**（§14.10）；若 P18 显示大面积锯齿不可接受 → 启用两阶段 coverage（`kind = 4` 预留 + 超采样 mask），**只加渲染器实现**，`Primitive::Path` 字段不变 |
| **R3** | **渐变插值空间与 §5.8.2/5.8.3 不一致**：v1 在 linear premul 插值，不是 OKLab；多色标渐变不可表达 | 中 | ①单色/双色渐变是 `base_ui` 的绝大多数场景；②OKLab 渐变需要 LUT（新 `AtlasKind` + 新 `kind`），触发条件：设计稿出现「同一渐变在深色主题下明显发灰」或 ≥3 色标；③扩展是 additive（§5.5 末段） |
| **R4** | 每帧重新 `resolve` 颜色的 CPU 成本（≈0.3 ms / 5000 图元） | 中 | P18 基准；若成热点 → lane 内缓存 `LinearPremulRgba`（additive，不改 128 B） |
| **R5** | 图集单页 + 无扩容：大图（> 页尺寸）直接 `AtlasExhausted` | 中 | ①P15 在装箱前按页尺寸拒绝/缩放（图片资源通常 < 2K）；②触发条件：出现 4K 预览图或超大图标集 → 加 `page` 字段（契约变更，需三平台同步） |
| **R6** | clip 深度/空栈：`Canvas::push_clip`/`pop_clip` 返回 `false` 时**静默不绘制**（调用方若不检查就会少画） | 低 | ①`Lane::push` 的 debug 断言在 debug 构建抓内部误用；②release 下 `Canvas` 只返回 `false` 不 panic（N6），因此调用方必须检查返回值（文档写明）；③真实场景 ≤ 3 层，触发条件：出现 > 8 层 → 合并深层为 AABB + 丢弃 radii 并补偏差 |
| **R7** | `GpuPrimitive` 的 `align(16)` 与三平台缓冲对齐要求的组合（D3D12 常量缓冲需 256 B 对齐；VBV/SRV 有 `minStorageBufferOffsetAlignment`） | 低 | ①`contract.md` 必须写「结构体 16 B 对齐」；②**实例缓冲的基址必须 16 B 对齐**（三平台都满足，但分配器/偏移计算必须显式保证）；③分段上传时按平台对齐要求向上取整（P09） |
| **R8** | `Primitive` 体积（实测 80 B）在超大 lane（> 50k 图元）下的内存 | 低 | lane 持久、按节点粒度重建；`size_budget` 单测守护；必要时把 `GlyphRun` 的 `Vec` 装箱（additive） |
| **R9** | `GeometryId` 是 `Copy`，忘记 `release_geometry` 会泄漏 VBO | 低 | P08 的 `NodeKind::Geometry` 卸载自动 release；`widget` 侧在 `Drop` 显式 release；P18 的 `GpuTimer`/内存快照可发现 |
| **R10** | `Primitive::Canvas(Canvas)` 是递归类型（`Canvas` → `Lane` → `Vec<Primitive>`）：若允许嵌套会无界递归 | 低 | ①`Canvas` **没有**接受 `Canvas` 的 push 方法；②编码器 `assert!(canvas_depth == 0)`（§8.6）；③`Canvas` 只在应用侧构造，引擎展平 1 层 |
| **R11** | `Canvas::push_text` 对应用层**不可用**（`GlyphRun` 需 `AtlasTile`），只有 P06 的文本 painter 能调 | 低 | 诚实写进 §11.5；应用层需要富文本自绘时，由 P06 additively 加 `Canvas::push_text_blob`（内部走 shaping），不改现有 API |

---

## 17. 评审裁决与修订记录

### 17.1 第一轮裁决（5 条，已落实）

| # | 裁决项 | 结论 | 修订落点 |
|---|---|---|---|
| **①** | 落地顺序 | 采纳延迟，并按「消费者」归属：`paint/instance.rs`→**P07**、`render/damage.rs`→**P06**、`render/atlas.rs`+`render.rs`→**P05** | §0-1、§3.1、§3.2、§19、S-08 |
| **②** | `Scene` 写入面 | `Canvas` 是唯一公开写入口；`Scene` 只读 | §7、§11、S-10、S-15、F-33 |
| **③** | `Path` 无 AA | 接受；硬边三角形，AA 延后（`kind=4` 预留），golden 标注 | §6.3.4、§8.3、§14.10、F-26 |
| **④** | `GpuPrimitive` 私有 + P07 | 接受；P02 冻结完整 offset/kind/flags 表 | §8.1–§8.4 |
| **⑤** | 边框+渐变两个 `Quad` | 接受；顺序/混合/责任写死 | §8.5、S-14 |

### 17.2 第二轮审查（FAIL：B1–B4 + N1–N14）响应

| 编号 | 审查意见 | 处理 | 落点 |
|------|----------|------|------|
| **B1/B2/B3/N11** | `Scene` 整体推迟 P03；P02 的 `scene.rs` 只留 `Lane`（`pub(crate)`）与 `MAX_CLIP_DEPTH`（`pub(crate) u8 = 8`）；`LaneId` 也推迟 | **全部采纳**：`Scene`/`LaneId`/lane 分配/绘制序删除出 P02；`paint.rs` 用 `pub(crate) mod scene;`；`Lane::push` 内部调用 `clip_depth()` 并用 `usize::from(...)` 断言，保证三个方法都有非 test 消费者；`Canvas` 消费 `new`/`push` | §3.1、§3.2、§4.1–§4.4、§7（整章）、§13.2、§14.3、§18、§19、S-18 |
| **B3** | `MAX_CLIP_DEPTH: u8`；断言 `usize::from(...) < usize::from(...)`；`Canvas` 不重复 derive `Default` | 采纳：常量类型 `u8 = 8`；断言逐字采用；`Canvas` 只有**手写** `impl Default`（不 derive） | §7.1、§7.4、§11.1、§14.3 |
| **B4** | 文档损坏：L1232 的 `&& (kind != QUAD \|\| …)`、L1684 的 `content.0 == 0 \|\| content.1 == 0` 丢失；§14.3 重复标题 | 已恢复两处运算符、删除重复标题；新增 §15-11/12 的**完整性复核命令**（管道符计数 + 三个运算符字面量 + 双空格伪算子扫描）；DEVIATIONS **S-17** 记录方法与本轮事故 | §8.4、§10.4、§14.3、§15-11/12 |
| **N1** | `Canvas` 尺寸改 48 B（Rect 16 + Lane 32），重跑 probe | `Lane` = **32 B**（`Vec`24 + `u8` + 对齐）、`Canvas` = **48 B**；§2.5 已更新（`Primitive` 仍 80 B） | §2.5、§11.1、§14.2 |
| **N2** | 三套 shader 一律**数值转换**，禁止 `as_type`/`floatBitsToInt` | 采纳；`contract.md` 必写禁止位重解释 | §8.2 注 1 |
| **N3** | scroll：区外有脏区时**放弃 scroll**、保留全部 rects | 采纳（两步：算法 §12.2 第 5 步 + 阈值表两条） | §12.2、§12.5 |
| **N4** | `resolve`/`interpolate` 全分量 `NaN→0` + 定义域 clamp（`h` 先 `rem_euclid(360)`），输出严格 `[0,1]` | 采纳：新增 `clamp_01`/`clamp_nonneg`/`hue_deg` 三个逐字定义 | §5.4、§5.5 |
| **N5** | 「逐位可复现」限定同平台；跨平台 ULP 级、golden 按 8 位比较 | 采纳 | §5.4 表、§14.10 |
| **N6** | `Canvas::push_clip`/`pop_clip` 返回 `bool`，不 panic；文档写 `# Panics: none`；单测覆盖 | 采纳：签名 `-> bool`、8 个 canvas 用例（含 2 个 false 路径）；C1 由 P06 展平阶段断言 | §11.1、§11.3、§14.4、S-18 |
| **N7** | DEVIATIONS 增「§1.3 白名单按传递闭包扩充」；`Lane`/`MAX_CLIP_DEPTH` 保持 `pub(crate)` | 采纳：§4.4 新增闭包表；DEVIATIONS **S-16** | §4.4、S-16 |
| **N8** | P01 §4.5 的 `AtlasTile` 行加注记 | 采纳：P01 §4.5 已加「已被 DEVIATIONS F-24 修订为 `paint::atlas::AtlasTile`」（只加注记） | `design/phase-01-foundation.md` §4.5、F-24 |
| **N9** | 随 Scene 推迟而消失 | 确认：P02 只 derive `Debug` 的类型其字段全部满足 `Debug` | §13.3、§14 |
| **N10** | 文档版本统一 v2 | 采纳：标题与 §0 均为 v2 | 标题、§0 |
| **N12** | `#[should_panic]` + `debug_assert` 用例加 `#[cfg(debug_assertions)]` | 采纳：§14.3 的 4/5 两条标注 | §14.3 |
| **N13** | §12.3 明确「Clip 不产生像素，mask 边界只作用于被裁剪图元」 | 采纳：`Clip` 行改为「不单独累加」 | §12.3 |
| **N14** | 门禁改用 `Get-ChildItem -Recurse -Filter *.rs` | 采纳：§15-6/7/11 全部改为该形式 | §15 |

**结论**：4 条阻塞全部消解（`Scene` 推迟 → P02 零 `Scene`；`MAX_CLIP_DEPTH: u8`；签名与断言逐字采纳；文档损坏已修复并加门禁）。
本轮实现范围 = §18；不 commit。

---

## 18. P02 实现清单（实现代理的直接输入）

> **范围**：7 个源文件 + 2 行清单改动。**不得**创建 `src/render*`、`paint/instance.rs`、`paint/damage.rs`、
> `Scene`/`LaneId`/`LaneSlot`、任何 `mod.rs`、任何空文件。
> **硬约束**：每个 `pub(crate)` item 必须有**非 test 消费者**（第 4 列）；没有消费者的能力一律不写。

### 18.1 文件、可见性、`pub(crate)` item 与消费者

| 文件 | 公开类型（应用层可见） | `pub(crate)` item | 非 test 消费者 | Phase |
|------|------------------------|-------------------|----------------|-------|
| `src/paint.rs` | —（5 行 `pub mod` + 1 行 `pub(crate) mod scene;`） | — | — | P02 |
| `src/paint/color.rs` | `Color`、`ColorResolver`、`LinearPremulRgba`、`ColorBrush`、`Interpolation`、`HueDirection` | — | — | P02 |
| `src/paint/primitive.rs` | `Primitive`、`Quad`、`QuadPaint`、`Border`、`Gradient`、`GradientKind`、`GlyphRun`、`PositionedGlyph`、`Image`、`Path`、`Shadow`、`ClipOp`、`ClipRect`、`GeometryDraw` | — | — | P02 |
| `src/paint/canvas.rs` | `Canvas`（`new`/`bounds`/`primitives`/`is_empty` + `push_quad`/`push_text`/`push_image`/`push_path`/`push_shadow`/`push_geometry`/`push_clip`/`pop_clip`） | — | 它自己是 `Lane` 的消费者 | P02 |
| `src/paint/scene.rs` | —（`pub(crate) mod scene;`，无公开 item） | `Lane`（`new`/`push`/`clip_depth`/`primitives`/`is_empty`）、`MAX_CLIP_DEPTH: u8 = 8` | `Canvas`：`Lane::new`（`Canvas::new`）、`Lane::push`（`push_*`）、`Lane::clip_depth`（`Canvas::push_clip`/`pop_clip` **与** `Lane::push` 内部）、`Lane::primitives`/`is_empty`（`Canvas` 只读面）、`MAX_CLIP_DEPTH`（`Lane::push` + `Canvas::push_clip`） | P02 |
| `src/paint/geometry.rs` | `GeometryId`、`GeometryDesc`、`IndexType`、`Topology`、`GeometryPipeline` | — | — | P02 |
| `src/paint/atlas.rs` | `AtlasKind`、`AtlasTile` | — | — | P02 |
| `src/lib.rs` | —（追加 `pub mod paint;`） | — | — | P02 |
| `crates/kytrix_gui/Cargo.toml` | —（追加 `bytes = { workspace = true }`） | — | — | P02 |

### 18.2 必做签名（速查）

| 文件 | 必做 |
|------|------|
| `paint/color.rs` | §5.1 `Color`（5 变体 + `TRANSPARENT/BLACK/WHITE` + 6 构造器 + `Default`）；§5.2 `LinearPremulRgba`（+`Mul<f32>`）；§5.3 `HueDirection`/`Interpolation`/`ColorResolver`/`ColorBrush`；§5.4 `resolve`（24 次定长二分 + N4 的 `clamp_01`/`clamp_nonneg`/`hue_deg`）；§5.5 `interpolate` |
| `paint/primitive.rs` | §6.3 的 8 类载荷 + `Primitive`（8 变体；`Clone/Debug/PartialEq`） |
| `paint/canvas.rs` | §11.1 `Canvas`（私有 `bounds`+`lane`）；`push_clip -> bool` / `pop_clip -> bool`（N6）；手写 `impl Default`（不 derive）；`# Panics: none` |
| `paint/scene.rs` | §7.1 `Lane`（私有 `primitives`+`clip_depth`，derive `Clone/Debug/Default/PartialEq`）+ `new`/`push`/`clip_depth`/`primitives`/`is_empty` + `MAX_CLIP_DEPTH: u8 = 8`；**无 `Scene`** |
| `paint/geometry.rs` | §9.1 `GeometryId`（私有字段 + `index()`/`generation()` + `#[cfg(test)] for_test`，见 S-20）；§9.2 `GeometryDesc`（`Bytes`）；§9.3 `IndexType::size`/`Topology`；§9.4 `GeometryPipeline::vertex_stride` |
| `paint/atlas.rs` | §10.1 `AtlasKind`（`#[repr(u8)]` + `index()`）、`AtlasTile`（5 pub 字段 + `uv()`/`is_stale()`） |

**不写**：`Scene`/`LaneId`（P03）、`GpuPrimitive` 全家（P07）、`AtlasAllocator`/`DirtyRows`（P05）、`DamageAccumulator`（P06）、`GeometryUniform`/`GeometryColor`（P09）。

### 18.3 测试（P02 必须提交的用例：**60**）

| 文件 | 用例（详见 §14） |
|------|------------------|
| `color.rs` | §14.1 的 **28** 个（含 B1 的两条端点回归） |
| `primitive.rs` | §14.2 的 **6** 个 |
| `scene.rs` | §14.3 的 **6** 个（`Lane`；其中 2 个 `#[should_panic]` + `#[cfg(debug_assertions)]`） |
| `canvas.rs` | §14.4 的 **11** 个（2 个 `false` 路径 + 1 个尺寸 + 2 个 debug 断言 panic） |
| `geometry.rs` | §14.5 的 **5** 个 |
| `atlas.rs` | §14.6 的 **4** 个 |
| 合计 | **60**（28 color + 6 primitive + 6 lane + 11 canvas + 5 geometry + 4 atlas） |

### 18.4 验收

§15 的 1–12 组命令（含 11 文件边界、12 文档完整性）。通过判据见 §15 末。

### 18.5 实现顺序

1. `paint.rs` + `lib.rs` + `Cargo.toml`；
2. `color.rs`（无依赖）→ 3. `atlas.rs`（无依赖）→ 4. `geometry.rs`（依赖 `atlas::AtlasKind`、`Bytes`）；
5. `scene.rs`（`Lane`，无依赖）→ 6. `primitive.rs`（依赖 `atlas`/`geometry`/`scene`→无、`canvas`）→ 7. `canvas.rs`（依赖 `scene::Lane` + `primitive::*`）。
   注意 `primitive.rs` 的 `Primitive::Canvas(Canvas)` 与 `canvas.rs` 的 `Canvas.lane: Lane` 是**跨模块递归类型**（`Vec` 提供间接），
   Rust 允许；`mod` 声明顺序不影响编译；
8. 补齐 §14 的用例；每加一个 `pub(crate)` item 立刻确认消费者。

---

## 19. 推迟交付与接手 Phase（用于更新总计划）

| P02 原计划交付 | 现状 | 接手 Phase | 接手判据 |
|----------------|------|-----------|----------|
| `paint/scene.rs` 的 `Scene` / `LaneId` / lane 分配 / 绘制序 | **删除出 P02**（B1/B2/B3/N11）；`Lane` + `MAX_CLIP_DEPTH` 保留为 `pub(crate)` | **P03** | P03 的 paint 循环是首个消费者；`Scene` 只读面 + `pub(crate)` 写入面（形状见 §7.2）；测试：lane 代际/order/clear_frame |
| `paint/instance.rs`（`GpuPrimitive` 128 B + 断言 + 1→N 编码 + `InstanceBuffer`/`FrameOp`/`ClipChain`） | 规格冻结（§8 全表） | **P07** | 有非 test 调用点（帧组装）；§8.1 静态断言编译通过；§14.7 的 14 个用例 |
| `render/damage.rs`（`DamageAccumulator`/`DamageOutput`/`ScrollHint`） | 规格冻结（§12 + N3/N13） | **P06** | 有非 test 调用点（paint 之后的 damage 步骤）；§14.8 的 14 个用例 |
| `render/atlas.rs`（`AtlasAllocator`/`Allocation`/`DirtyRows`/generation） | 规格冻结（§10.4–§10.6 逐字伪代码） | **P05** | 有非 test 调用点（字形装箱）；`AtlasExhausted` 随该相追加；§14.9 的 7 个用例 |
| `render.rs` 模块根 | 不创建 | **P05** | 首个 render 子模块落地时创建（`pub(crate) mod atlas;`，P06 追加 `pub(crate) mod damage;`） |
| `GeometryUniform`/`GeometryColor`（48 B per-draw 常量） | 规格冻结（§9.5） | **P09** | 有非 test 调用点（上传路径）；`contract.md` 镜像 48 B |

**对总计划的影响**：P02 = 「5 个公开模块文件 + `scene.rs`（仅 `Lane`）+ 2 行清单」；P03 增加 `Scene`/`LaneId`；
P05 增加 `render.rs`+`render/atlas.rs`；P06 增加 `render/damage.rs`；P07 增加 `paint/instance.rs`。其它 Phase 内容不变。

---

## 20. 本阶段需追加到 `design/DEVIATIONS.md` 的条目

> 编号从 **F-23** / **S-08** 起（F-22、S-07 已被 P01 占用；历史条目不得改写）。完整条目见 `DEVIATIONS.md` §七。
> 🆕 = 本轮新增；✏️ = 按裁决调整过结论。

| 编号 | 标题 | 一句话 |
|---|---|---|
| **F-23** | `GpuPrimitive` 必须 `#[repr(C, align(16))]` | `repr(C)` 只有 align 4；`contract.md` 写 16 B 对齐，实例缓冲基址也须 16 B 对齐 |
| **F-24** ✏️ | `AtlasTile`/`AtlasKind` 归属 `paint/atlas.rs` | P01 §4.5 已加注记（N8）；`resources.rs` 不再定义 |
| **F-25** ✏️ | damage 留在 `render/damage.rs` | 位置按 P01 §4.1；落地 Phase 改 **P06** |
| **F-26** | `Path` 删除 `triangles`/`coverage_pass` | 改为 `GeometryId` + `Color`；v1 硬边，golden 标注 |
| **F-27** | `Geometry` 删除 `pipeline`/`damage` 字段 | 单一来源 = `GeometryDesc` + 计算 |
| **F-28** | `color 0.3.3` 无 `map_chroma` | 直接构造 `DynamicColor` 改 C |
| **F-29** | `Canvas` v1 不注册应用 pipeline | 只记录引擎图元；扩展见 §11.6 |
| **F-30** | `GeometryId` 是 `Copy` | 必须显式 `release_geometry` |
| **F-31** | `GeometryPipeline` 不含 `Path` 变体 | 并入 `Ui` |
| **F-32** | 渐变只支持双色 + linear premul | OKLab 渐变/LUT 延后 |
| **F-33** | `Canvas` = lane 片段模型（删 `CanvasOp`） | 唯一公开写入口 |
| **S-08** ✏️ | P02 落地 = 「可达且 §1.3 白名单内」 | `instance`→P07、`render/damage`→P06、`render/atlas`+`render.rs`→P05；`Scene`→P03 |
| **S-09** ✏️ | `MAX_CLIP_DEPTH: u8 = 8` | C2/C3 在 P02 由 `Lane::push` debug 断言 + `Canvas` bool 返回；C1 由 P06 断言 |
| **S-10** ✏️ | `Scene` 只读 + `Canvas` 唯一公开写入口 | `Scene` 整体推迟 P03 |
| **S-11** | 图集单页 + 格式 + padding + UV 按内容尺寸 | 分配器 P05 |
| **S-12** | `Color` v1 范围（无 `from_hex`/`none`/powerless hue） | 两套插值语义分离 |
| **S-13** | damage 阈值 16 / 0.5 为常量 | P18 校准 |
| **S-14** | 边框 + 渐变 = 两个 `Quad` | 顺序/混合/应用层责任 |
| **S-15** ✏️ | `Scene`/`Canvas`/`Lane` 所有权与 API 契约 | `Lane` 为 `pub(crate)`；`Canvas` 48 B |
| **S-16** 🆕 | §1.3 白名单按**传递闭包**扩充（N7） | 列 P02 新增公开类型与消费 Phase；`Lane`/`MAX_CLIP_DEPTH` 不进公开面 |
| **S-17** 🆕 | 文档完整性复核方法（B4）+ 本轮损坏事故记录 | 管道符计数 + 运算符字面量 + 双空格扫描；事故原因与修复 |
| **S-18** 🆕 | `Canvas` clip API 返回 `bool`、无 panic（N6）；`Lane`/`MAX_CLIP_DEPTH` 类型与断言（B3） | `usize::from(...)` 断言；`# Panics: none` |
| **S-19** 🆕 | 数值与 damage 语义契约（N2/N3/N4/N5/N13） | 禁止位重解释；scroll 放弃规则；NaN/clamp/rem_euclid；同平台逐位、跨平台 ULP；clip 不产生像素 |
| **S-20** 🆕 | 实现期：`GeometryId` 私有字段 + `#[cfg(test)] for_test`（`pub(crate)`/`pub` 字段均不可行） | `field_scoped_visibility_modifiers = "forbid"` 禁止 `pub(crate)` 字段；`pub` 字段又让应用层可伪造句柄。→ 私有字段 + 公开 getter + 仅测试构造器 |
| **S-21** 🆕 | 第二轮代码审查：B1 修复 + N1–N15 处置 | `interpolate` 的 `mid` 无条件走 (B) 路径；端点不变量由 2 条回归守住；其余非阻塞项逐条落地 |
| **S-22** 🆕 | 公开项 `missing_docs` 债务（V-22） | color 34 / primitive 24 / geometry 10 / atlas 1；本阶段不补齐，P03/P07 消费者落地时补 |
## 附录 A · 实现者检查单（与 §18 同源，按执行顺序排列）

> §18 是**权威清单**（文件 × 公开类型 × 消费者 × 测试）；本附录只给一份可勾选的执行顺序，避免实现时漏项。
> 详细签名与语义见 §5–§12；验收命令见 §15。

| # | 检查项 | 通过判据 |
|---|--------|----------|
| 1 | `src/paint.rs` = 5 行 `pub mod` + 1 行 `pub(crate) mod scene;` | 无 item、无 `pub use`、无 `#[path]`、无 `mod.rs`；`scene` 对应用层不可见 |
| 2 | `src/lib.rs` 只追加 `pub mod paint;` | 不出现 `mod render;`（§3.2） |
| 3 | `Cargo.toml` 追加 `bytes = { workspace = true }` | 一行；不引入其它依赖 |
| 4 | 7 个文件的类型签名与 §5–§11 逐字一致 | 字段名/可见性/派生 trait 对齐 §13.3 |
| 5 | **每个 `pub(crate)` item 有非 test 消费者** | 只有 `Lane`（5 个方法）与 `MAX_CLIP_DEPTH`：消费者 = `Canvas`（§18.1）；其余一律推迟 |
| 6 | **不存在** `src/render.rs`、`src/render/`、`paint/instance.rs`、`paint/damage.rs`、`Scene` | `Test-Path` 全 False + `Get-ChildItem` 清单（§15-11） |
| 7 | `paint/scene.rs` 无 `pub fn`（只有 `pub(crate)`） | §15-7c 无输出 |
| 8 | `Canvas` 的 8 个写方法齐全（含 `pop_clip`），`push_clip`/`pop_clip` 返回 `bool` | §15-7c 命中 7 行 push + `pop_clip -> bool` 自检 |
| 9 | §14 中标记 P02 的测试全部存在且通过 | `cargo test -p kytrix_gui`：**paint 侧 60**（28+6+6+11+5+4），全 crate **103**（+ error 11 + math 32）；release 下 paint 56 / 全 crate 99（4 个 debug 断言用例按 N12 排除） |
| 10 | 零 `unsafe` / 零 `#[allow]` / 零 `#[expect]` / 零 `serde_json` | §15-7 |
| 11 | `color::` 只出现在 `paint/color.rs` | §15-6 |
| 12 | 每个公开类型有文档注释 + 消费 Phase 标注 | `cargo doc` 零警告 |
| 13 | `DEVIATIONS.md` §七 已追加 F-23…F-33 / S-08…S-22 | 不改写历史条目 |

## 附录 B · 与上游文档的交叉引用索引

| 本文 | 主设计 | P01 | DEVIATIONS |
|---|---|---|---|
| §2.1 color 事实 | §5.8.1、§9.4 | A.4 | F-28 |
| §2.2 etagere 事实 | §2.8、§5.6 | A.6 | S-11 |
| §2.4a `align(16)` | §2.3、附录 B | §7.2 | **F-23** |
| §3.2 落地顺序（裁决 ①） | §1.2 | §3、§4.1 | **S-08** |
| §4.1 图集归属 + 边界（裁决 ②） | §1.3、§2.3 | §4.5 | **F-24**、S-10、S-15 |
| §5 Color | §5.8、D23、I15 | §6.3（math 风格） | F-28、S-12、**F-32** |
| §6 Primitive（含 Canvas 变体） | §5.1、附录 C | — | F-26、F-27、**F-29** |
| §7 Lane/clip（B1/B2/B3：Scene→P03） | §3.2、§3.4、§5.3 | — | S-09、S-10、**S-15**、S-18 |
| §8 `GpuPrimitive`（裁决 ④） | §2.3、§5.2、§5.4、附录 B | §7.1/§7.2 | F-23、F-13 |
| §9 Geometry | §5.5、附录 C | §6.3（`Transform`）、F-14 | F-30、F-31 |
| §10 图集 | §2.8、§5.6、§9.3 | A.6 | S-11 |
| §11 Canvas（裁决 ②） | §5.9、D16 | — | **F-29**、**F-33**、S-15 |
| §12 damage（裁决 ①） | §5.7、§4.4 | §7.1–§7.4、F-09 | **F-25** |
| §13 I12 | §6.3、I12 | — | S-08、S-10、S-16 |
| §14.10 golden 覆盖（裁决 ③/⑤） | §6.4、I8 | — | F-26、**S-14** |
| §17 裁决记录（两轮） | — | — | 全部 |
| §18/§19 实现与推迟清单 | — | §3 | S-08 |

---
*文档完 · 本文只描述 Phase 02 的数据契约与纯 CPU 算法；不含 GPU / 平台 / 节点树实现。*



