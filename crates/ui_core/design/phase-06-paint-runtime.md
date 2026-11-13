# Phase 06 · paint pass、实例编码、damage、headless 运行时与测试基建 — 详细设计（v1）

> 上游：`custom-gui-engine-design.md` §3.4–§3.8 / §5.1–§5.7 / §5.9 / §6.1–§6.6 / §6.8 / 附录 B、C；
> `design/phase-01-foundation.md` §4 / §7；`design/phase-02-paint-foundation.md`（下称 **P02**）§6–§8、§11–§13、§17–§19；
> `design/phase-03-node-state.md`（**P03**）§9 / §11 / §13 / §14；`design/phase-04-layout.md`（**P04**）§8 / §13；
> `design/phase-05-text.md`（**P05**）§5.2 / §13；已落地代码（`app.rs` / `window.rs` / `node/*` / `paint/*` / `text/*`）。
>
> 本阶段**不写** GPU、不写平台层：`Submit` 只到「提交计数 + 记录」为止，`WaitGpu` 不存在。

---

## 0. 结论摘要

| 项                | 结论                                                                                                                                                                                                    |
|-------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| lane 模型         | **一节点一 lane**；lane 自包含祖先 `Clip` 前缀（clip-balanced，C1）；`REUSABLE` 决定整桶复用；**无图元节点不留 lane**（按需分配，有 lane 的节点粘性保留）；`Scene::order` = 节点 DFS 前序 × 该节点 lane |
| `REUSABLE` 复位点 | **提交时**（`NodeTree::settle_frame`），不是帧初 —— 见 §9.4 的 F-59                                                                                                                                     |
| `GpuPrimitive`    | `#[repr(C, align(16))]`，**128 B**，8 个 `[f32;4]`；probe 已验 `size/align/offset` 全部成立（§2.1）                                                                                                     |
| 1→N               | 仅 `GlyphRun`：每字形 1 实例；`Path`/`Geometry` 0 实例 + `FrameOp::Geometry`；`Canvas` 展平                                                                                                             |
| 静态绘制属性      | 新增 `paint::style::PaintSpec`，挂在 **`Node.paint: Option<Box<PaintSpec>>`**（+8 B），`Style` 公开结构不变（裁决 ①）                                                                                   |
| `Node` 尺寸       | **1112 → 1128 B**（+`paint` 8 + `content_offset` 8），预算 1152，余 24 B                                                                                                                                |
| 稳定 debug id     | `NodeTree::debug_ids: Vec<Option<SmolStr>>` **侧表**，`Node` 不增尺寸（裁决 ③）                                                                                                                         |
| damage            | 新 `DamageAccumulator`（covering → 外扩 → clip 求交 → 合并 → 16/50% → Full）；**不含** scroll 字段（裁决 ②）；`remove` 把子树 bounds 并入父 damage，不加 `LayoutBox.overflow`                           |
| 帧调度            | `FrameScheduler` 三态 `Idle/Pending/Draining` + `FrameIndex`（**只在提交时 +1**）；两段门禁保证 I3 空闲零帧                                                                                             |
| 测试基建          | `feature = "test-support"`：`test_support::{app,probe,record,input,snapshot}`；`TestApp`/`TestWindow`/`Probe`/`FrameRecord`/`RenderedFrame`/`PaintLog`/oracle                                           |
| 新文件            | `src/paint/instance.rs`、`src/paint/pass.rs`、`src/render/damage.rs`、`src/test_support.rs` + 5 个子模块、`tests/frame_record.rs`                                                                       |
| 测试              | 计划 **60** 个（单元 56：instance 12 / damage 12 / pass 8 / scene 3 / tree 6 / window 6 / app 3 / test_support 6；集成 4）                                                                              |
| 需裁决            | **5 项**（§15）：`PaintSpec` 载体、damage 输出形状、debug id 存储、`Geometry` transform、`paint/pass.rs` 新文件                                                                                         |

---

## 1. 范围与非目标

### 1.1 本阶段交付（新增文件）

| 文件                           | 作用                                                                                                                                                                                       |
|--------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `src/paint/instance.rs`        | `GpuPrimitive`（128 B 契约 + 静态断言）、`kind`/`flags`/`GRADIENT_*` 常量、`ClipChain`/`ClipChainId`、`FrameOp`、`InstanceBuffer`、`InstanceEncoder`（`Primitive → GpuPrimitive`，含 1→N） |
| `src/paint/pass.rs`            | paint pass：递归遍历节点树 → 重建/复用 lane → 生成 `Primitive`（style/patch/text/canvas/geometry）→ 累积 damage                                                                            |
| `src/render/damage.rs`         | `DamageAccumulator`（covering、AA/blur 外扩、clip 求交、合并、16/50% 阈值、Full）                                                                                                          |
| `src/test_support.rs`          | `test_support` 模块根（只做 `pub mod` 声明）                                                                                                                                               |
| `src/test_support/app.rs`      | `TestApp`、`TestWindow`（`drain` / `inject` / `oracle` / `snapshot`）                                                                                                                      |
| `src/test_support/probe.rs`    | `Probe`、`Selector`                                                                                                                                                                        |
| `src/test_support/record.rs`   | `FrameRecord`、`RenderedFrame`、`FrameStats`、`QuadRecord`、`PrimitiveRecord`、`PrimitiveKind`、`PaintLog`、`LanePaintEntry`、`LaneOutcome`                                                |
| `src/test_support/input.rs`    | `InputEvent`、`PointerButton`、`InputOutcome`                                                                                                                                              |
| `src/test_support/snapshot.rs` | `SnapshotCase`、`SnapshotImage`、`SnapshotError`（GPU 实现留 P09）                                                                                                                         |
| `tests/frame_record.rs`        | 集成测试（**仅** `--features test-support`）                                                                                                                                               |

### 1.2 本阶段修改

| 文件                    | 修改                                                                                                                                                                                                                                                                                     |
|-------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `src/lib.rs`            | 追加 `#[cfg(feature = "test-support")] pub mod test_support;`（仍只有 `mod`/`pub mod` 行）                                                                                                                                                                                               |
| `src/paint.rs`          | 追加 `pub(crate) mod instance;`、`pub(crate) mod pass;`（仍无 item、无 `pub use`）                                                                                                                                                                                                       |
| `src/render.rs`         | 追加 `pub(crate) mod damage;` + `pub(crate) use damage::{DamageAccumulator, DamageSummary};`                                                                                                                                                                                             |
| `src/paint/scene.rs`    | 追加 `Scene::clear_lane`（`pub`，engine-internal doc）；**不改**既有写面可见性（§4.3）                                                                                                                                                                                                   |
| `src/paint/style.rs`    | 追加 `PaintSpec` / `ShadowSpec`（公开类型）                                                                                                                                                                                                                                              |
| `src/paint/color.rs`    | 追加 `Color::with_opacity` 与 `Color::is_transparent`（公开方法，均按 alpha 判定）                                                                                                                                                                                                       |
| `src/paint/geometry.rs` | `GeometryId::for_test` 的 cfg 扩为 `any(test, feature = "test-support")` 且 `pub`（F-65）                                                                                                                                                                                                |
| `src/node/tree.rs`      | `Node` +`paint`/`content_offset`；`NodeTree` +`debug_ids`/`removed_lanes`；新增 `settle_frame`/`set_reusable`/`lane_of`/`set_lane`/`drain_removed_lanes`/`debug_id`/`content_offset`；`remove` 并入子树 damage + 记录 lane；`absolutize` 清 `REUSABLE`；`set_style` 的 clip 变化失效子树 |
| `src/node/builder.rs`   | `NodeBuilder::{id, paint}`；`NodeSpec` 透传                                                                                                                                                                                                                                              |
| `src/app.rs`            | `App::frame(window)`（完整帧）                                                                                                                                                                                                                                                           |
| `src/window.rs`         | `FrameIndex`/`FrameState`/`FrameScheduler`/`FrameTicket`/`FrameOutcome`；`Window::{frame, needs_frame, paint_frame, commit_frame, abort_frame, begin_frame, layout_if_dirty, frame_index, record(build), force_full_repaint(cfg)}`；`PaintReport` 的二次汇总                             |
| `src/math.rs`           | `PhysicalRect::area()`（公开；damage 阈值与断言的唯一实现）                                                                                                                                                                                                                              |
| `..`                    | `[features] test-support = []` + `[[test]] name="frame_record" required-features=["test-support"]`（**不新增依赖**）                                                                                                                                                                     |
| `design/DEVIATIONS.md`  | 追加 §十一（F-58…、S-60…、V-48…）                                                                                                                                                                                                                                                        |

### 1.3 明确非目标（交给后续 Phase）

| 项                                                            | 接手    | 判据                                            |
|---------------------------------------------------------------|---------|-------------------------------------------------|
| `GpuPrimitive` 上传、PSO、批次归并、`Present1` 翻译           | P07/P09 | 需要 GPU 设备；P06 只产出 `InstanceBuffer`      |
| 像素快照设备（WARP / lavapipe / Metal）                       | P09     | `TestApp::snapshot` 在 P06 恒 `Err(NoRenderer)` |
| `set_geometry_transform` 的 uniform 通道、`GeometryStore`     | P08/P09 | 无 store 时 `GeometryId` 只能由测试构造（F-65） |
| 滚动（`Transient` 偏移、scroll hint、`Present1` scroll rect） | P12/P09 | 无滚动容器（P04 无 scroll 模型）                |
| 键盘/IME/焦点/Action 分发/点击态                              | P10/P11 | 无 `event.rs`、无交互机制                       |
| `Selector::Role`                                              | P14     | `A11yRole` 尚不存在                             |
| 文本选区、光标、Markdown 行内流渲染                           | P10/P15 | 仅有 `ShapedLine` 字形                          |
| 动画 tick / 连续帧 / `PlatformDispatcher::wake`               | P09/P10 | headless 无平台事件源                           |
| 脏位驱动的增量遍历（`report` 已够用）                         | P13/P18 | V-49                                            |

---

## 2. 事实基线（probe 记录）

### 2.1 尺寸 probe（`%TEMP%\kg-p06-size-probe`，path 依赖 `..`，`CARGO_TARGET_DIR` 复用仓库 target）

```text
Node                               size=1112 align= 8     ← 与 S-59 门禁一致
NodeKind                           size= 112 align= 8
LayoutBox                          size=  32 align= 4
Style                              size= 560 align= 8
StylePatch                         size=  76 align= 4
Color                              size=  20 align= 4     Option<Color> = 20
LinearPremulRgba                   size=  16 align= 4
Primitive                          size=  80 align= 8
Quad                               size=  80 align= 4     QuadPaint = 48
Border                             size=  24 align= 4     Option<Border> = 24
Gradient                           size=  44 align= 4     Option<Gradient> = 44
Shadow                             size=  48 align= 4
GlyphRun                           size=  48 align= 8     PositionedGlyph = 40
Image                              size=  60 align= 4
ClipRect / ClipOp                 size=  32 / 36
Canvas                             size=  48 align= 8     Scene = 72, LaneId = 8
AtlasTile                          size=  24 align= 4
TextContent                        size= 112 align= 8     TextStyle = 72
ShapedLine                         size=  80 align= 8
LayoutReport / StylePassReport      size=  32 / 32

GpuPrimitive                       size= 128 align=16     ← 静态断言编译通过
  offsets: bounds=0 corner_radius=16 color0=32 color1=48 params0=64
           params1=80 uv=96 clip_rect=112
PaintSpec（候选）                   size= 136 align= 4
Option<Box<PaintSpec>>             size=   8 align= 8
Style + paint 字段                  size= 568（+8）
ClipChain（候选）                   size=  56 align= 4
FrameOp（候选）                     size=  12 align= 4
```

**Node 预算推算**：`1112 + 8（paint）+ 8（content_offset）= 1128 ≤ 1152`（余 **24 B**）。
`Node` 的 `align_of == 8` 且 1112 是 8 的倍数 ⇒ 追加 8 B 对齐字段**不引入**额外 padding。
若内联 `SmolStr`(24 B) 作 debug id ⇒ 1144（余 8 B）——本阶段**不**采用（裁决 ③）。

### 2.2 lint / 门禁 probe

| 事实                                                                                                      | 实测                                                                                |
|-----------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------|
| `debug_assert!` 里的字段读取**算**作 `dead_code` 的「使用」                                               | `%TEMP%\kg-p06-deadcode-probe`：`cargo build` 零 warning                            |
| `pub(crate)` struct 的 **`pub` 字段**不触发 `field_scoped_visibility_modifiers` / `pub_without_shorthand` | `%TEMP%\kg-p06-lints-probe`：`cargo +nightly clippy --all-targets -D warnings` 通过 |
| 现有基线                                                                                                  | `cargo test -p ui_core` = **364 passed**，0.07 s                                    |
| `GeometryId::for_test` 现为 `#[cfg(test)] pub(crate)`                                                     | 集成测试（非 `cfg(test)`）**无法**构造 `GeometryId` ⇒ F-65                          |

### 2.3 代码事实（本阶段必须遵守的已落地形状）

1. `Scene` 已存在：`Arena<Lane, LaneId>` + `order: Vec<LaneId>`；`Lane` 的 `clear()` 保留容量并复位 `clip_depth`；
   `Scene::clear_frame` 遍历所有 lane 清空。
2. `Node.lanes: SmallVec<[LaneId; 2]>`（P03 预留，P06 首次写入）。
3. `DirtyFlags::REUSABLE` 只有清除者（`NodeTree::touch`）；**没有** `begin_frame` / `settle_frame`（P06 只加后者，见 F-59）。
4. `NodeTree::place_subtree`（`absolutize`）在 `changed` 时把 `old ∪ new` 并入 damage 并写 `prev_layout`；**不**清
   `REUSABLE`。
5. `NodeTree::remove` 已先取 `block = collect_subtree_ids(node)`，再逐节点移除 —— 子树 bounds / lane 的最后一处可读点。
6. `App::refresh_text` 在快照未变时 `continue`（B4）；`layout_text` 以 `(laid_width_bits, generation)` 判复用。二者都是 O(n)
   但零写入。
7. `Window` 当前字段：`id/options/tree/scene/scale/scale_epoch`（`scene_mut` 不存在）。
8. `text::ShapedGlyphRun::glyphs[].position/size` 是**文本盒局部**逻辑像素（`/scale` 后），y 已含基线（P05 §5.2、S-54）。
9. `LayoutBox.border: Edges` 由 taffy `Layout.border` 写入（P04 §8.1）；`content_width` 已在 `set_local_layout` 计算，但
   **padding 未保存**。

---

## 3. 文件树与落地顺序

### 3.1 文件树（P06 结束时）

```text
crates/ui_core/
├─ Cargo.toml                      # +[features] test-support、+[[test]] frame_record
├─ src/
│  ├─ lib.rs                       # + cfg(test-support) pub mod test_support;
│  ├─ app.rs                       # + App::frame
│  ├─ window.rs                    # + 帧调度 / paint 驱动 / commit / record(cfg)
│  ├─ math.rs                      # + PhysicalRect::area
│  ├─ node/{id,signal,dependency,dirty,tree,builder}.rs
│  ├─ paint.rs                     # + pub(crate) mod instance; + pub(crate) mod pass;
│  ├─ paint/{color,primitive,canvas,geometry,atlas,scene,style}.rs
│  ├─ paint/instance.rs            # 新
│  ├─ paint/pass.rs                # 新
│  ├─ render.rs                    # + pub(crate) mod damage;
│  ├─ render/{atlas,damage}.rs     # damage 新
│  ├─ text.rs + text/*
│  ├─ layout.rs + layout/*
│  ├─ test_support.rs              # 新（模块根：只有 pub mod 行）
│  └─ test_support/{app,probe,record,input,snapshot}.rs
└─ tests/frame_record.rs           # 新（required-features = ["test-support"]）
```

### 3.2 落地顺序（按「非 test 消费者」排序）

1. `math.rs`（`area`）→ 2. `paint/style.rs`（`PaintSpec`）→ 3. `paint/color.rs`（`with_opacity`）→
4. `node/tree.rs` + `node/builder.rs`（字段/侧表/`settle_frame`/`absolutize`/`remove` 改动）→
5. `render/damage.rs`（无依赖，纯函数）→ 6. `paint/instance.rs`（依赖 `primitive`/`scene`/`color`/`math`）→
7. `paint/scene.rs`（`clear_lane`）→ 8. `paint/pass.rs`（消费 1–7）→ 9. `window.rs`（帧调度 + 编码 + damage）→
10. `app.rs`（`App::frame`）→ 11. `lib.rs` + `Cargo.toml` + `test_support/*` → 12. `tests/frame_record.rs` → 13.
    DEVIATIONS。

---

## 4. 公共 / 私有边界

### 4.1 公开路径表增量（应用层迁移的唯一契约）

| 路径                                                                           | 类型/方法                                                                 | 消费 Phase                                |
|--------------------------------------------------------------------------------|---------------------------------------------------------------------------|-------------------------------------------|
| `paint::style::PaintSpec`                                                      | 静态绘制属性（`background`/`border`/`corner_radius`/`shadow`/`gradient`） | P06（paint pass）、P15（主题）            |
| `paint::style::ShadowSpec`                                                     | 阴影参数（`offset`/`blur`/`color`）                                       | P06、P15                                  |
| `paint::color::Color::with_opacity`                                            | alpha 缩放（`[0,1]` clamp，NaN → 0）                                      | P06（paint pass）                         |
| `paint::color::Color::is_transparent`                                          | alpha `<= 0`（NaN 视为透明）；「本图元不产生像素」的唯一判定              | P06（paint pass、编码器）                 |
| `paint::scene::Scene::clear_lane`                                              | engine-internal（doc 首行标注）                                           | P06（paint pass）                         |
| `node::tree::NodeTree::debug_id`                                               | 稳定调试 id 的只读查询                                                    | P06（`Probe`）                            |
| `node::builder::NodeBuilder::id`                                               | 挂载时声明稳定调试 id（`id(impl Into<SmolStr>)`，内部存 `SmolStr`）       | P06（`Probe`）、P08                       |
| `node::builder::NodeBuilder::paint`                                            | 挂载时声明静态绘制属性                                                    | P06、P15                                  |
| `math::PhysicalRect::area`                                                     | `i64` 面积（damage 阈值 / 测试）                                          | P06                                       |
| `window::FrameIndex`                                                           | 单调提交序号（0 = 未提交）                                                | P06（记录）、P07/P09                      |
| `window::FrameOutcome` / `window::FrameIndex` \| 帧结果 / 提交序号 \| P06、P09 |
| `window::Window::{frame, frame_index}`                                         | 窗口本地帧（无 styles/text 通道）                                         | P06（测试/P07）                           |
| `app::App::frame`                                                              | 完整帧（styles→text→layout→text layout→paint→commit）                     | P06、P09                                  |
| `paint::geometry::GeometryId::for_test`（`cfg(any(test, feature))`，`pub`）    | 仅测试构造句柄                                                            | P06 集成测试、P09 被 `GeometryStore` 取代 |

### 4.2 §1.3 白名单的传递闭包扩充（S-16 格式）

```text
P06 新增公开类型（含字段/方法）：
  paint::style::PaintSpec{background,border,corner_radius,shadow,gradient}
  paint::style::ShadowSpec{offset,blur,color}
  paint::color::Color::with_opacity
  paint::color::Color::is_transparent
  paint::scene::Scene::clear_lane            (engine-internal doc)
  node::builder::NodeBuilder::{id,paint}
  node::tree::NodeTree::debug_id
  math::PhysicalRect::area
  window::{FrameIndex, FrameOutcome}                 // FrameState 为 pub(crate)（无公开观察者）
  window::Window::{frame, frame_index}
  app::App::frame
  paint::geometry::GeometryId::for_test      (cfg(test | feature = "test-support"))
  test_support::{app::TestApp, app::TestWindow, probe::{Probe, Selector},
                 record::{FrameRecord, RenderedFrame, FrameStats, QuadRecord,
                         PrimitiveRecord, PrimitiveKind, PaintLog, LanePaintEntry,
                         LaneOutcome},
                 input::{InputEvent, PointerButton, InputOutcome},
                 snapshot::{SnapshotCase, SnapshotImage, SnapshotError}}
    —— 整块由 `#[cfg(feature = "test-support")]` 门禁，不进发布物（I7 / §6.6）
不新增公开面（保持 crate 私有）：
  paint::instance::{GpuPrimitive, InstanceBuffer, InstanceEncoder, FrameOp, ClipChain,
                    ClipChainId, KIND_*, FLAG_OPAQUE, TEXTURE_*, GRADIENT_*}
  paint::pass::{PaintPass, PaintReport}
  render::damage::{DamageAccumulator, DamageSummary, MAX_DAMAGE_RECTS, FULL_FRAME_AREA_RATIO}
  window::{FrameScheduler, FrameTicket, FrameIndex 的字段}
```

### 4.3 `Scene` 写面的可见性（结论：**不收紧**）

P03 的 S-33 把 `new_lane/release_lane/push/clear_frame/set_order/clear_order` 定为 `pub` + doc 首行
「engine-internal, 应用层不得调用」，理由是当时**没有 crate 内调用者**（`pub(crate)` 会 `dead_code`）。

P06 之后 `new_lane/push/set_order/release_lane` 都有 crate 内调用者，但 `clear_frame`/`clear_order` 仍然**只**有
`feature = "test-support"` 的 oracle 调用者。若只收紧一半，写面会分裂成两套可见性规则，比 S-33 现状更难审计。 **结论：全部保持
`pub`；新增的 `clear_lane` 按同一约定为 `pub` + engine-internal doc。**

### 4.4 每个 `pub(crate)` item 与其**非 test 消费者**（dead_code 审计表）

| item                                                                                                                  | 非 test 消费者                                                                                    |
|-----------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------|
| `paint::instance::GpuPrimitive`（及 8 个字段）                                                                        | `InstanceEncoder::emit_*`（构造）、`encode` 末尾自检（读 `params1`/`bounds`）                     |
| `InstanceBuffer::{clear, len}`（`len` 仅 `cfg(test-support)`；`instances` 由 encode 自检读）                          | `InstanceEncoder::{encode, push_op, self_check}` / `FrameStats`（feature）                        |
| `InstanceBuffer::{clips, geometry, ops}` 字段                                                                         | `encode` 的 clip 去重 / `push_op` / `self_check`（`debug_assert!` 读，probe §2.2 已验证计入使用） |
| `FrameOp` / `ClipChain` / `ClipChainId`                                                                               | `encode`（构造 + 自检 + 去重键）                                                                  |
| `KIND_*` / `FLAG_OPAQUE` / `TEXTURE_*` / `GRADIENT_*`                                                                 | `emit_*`、`opaque` 判据                                                                           |
| `InstanceEncoder::{new, encode}`                                                                                      | `Window::paint_frame`                                                                             |
| `render::damage::{DamageAccumulator::{new,reset,push,push_full,finish}, DamageSummary::{full,rects}}`                 | `Window::paint_frame`（push/finish）、`Window::commit_frame`（两字段都读的门禁）                  |
| `MAX_DAMAGE_RECTS` / `FULL_FRAME_AREA_RATIO`                                                                          | `DamageAccumulator::finish`                                                                       |
| `DamageAccumulator::rects`（`cfg(test-support)`）                                                                     | `FrameRecord` 捕获（cfg 构建）                                                                    |
| `paint::pass::{PaintPass, PaintReport::{rebuilt, damage}}`                                                            | `Window::{paint_frame, commit_frame}`                                                             |
| `PaintReport::{instances, log}`（`cfg(test-support)`）                                                                | `Window` 的 `FrameStats` / 记录捕获（cfg 构建）                                                   |
| `Scene::clear_lane` / `Lane::clear`                                                                                   | `PaintPass`                                                                                       |
| `NodeTree::{settle_frame,set_reusable,lane_of,set_lane,drain_removed_lanes,clip_key,set_clip_key,clear_layout_paint}` | `PaintPass` / `Window::{paint_frame, commit_frame}`                                               |
| `NodeTree::{content_offset, paint_spec, has_bindings}`                                                                | `PaintPass` / `Window::frame` 的 debug 断言                                                       |
| `NodeTree::debug_id`（`pub`）                                                                                         | `Probe`                                                                                           |
| `Window::{begin_frame, layout_if_dirty, paint_frame, commit_frame, abort_frame, needs_frame}`                         | `Window::frame` / `App::frame`                                                                    |
| `Window::{record, record_idle, set_frame_stats}`（`cfg(test-support)`）                                               | `TestApp` / `TestWindow`                                                                          |
| `FrameScheduler::{new, request_frame, begin, commit, abort, committed}`（`state` 仅 `cfg(test-support)`）             | `Window::{frame, begin_frame, commit_frame, abort_frame}`                                         |

> **「编码器自检」是刻意的非 test 读点**：`InstanceEncoder::encode` 末尾断言
> `Σ FrameOp::Instances.len == instances.len()` 且每个 `Geometry(i)` 的 `i < geometry.len()`，
> 这让 `InstanceBuffer` 的内部字段在**所有构建**（含 release，`cfg!` 是运行期常量）都有读者，
> 不必把 buffer 的只读面提前公开（P07 会加 `as_bytes`/`instances` 等真实上传读点）。

---

## 5. lane 模型

### 5.1 节点 → lane 的映射（一节点一 lane）

| 规则    | 内容                                                                                                                               |
|---------|------------------------------------------------------------------------------------------------------------------------------------|
| 分配    | 节点**首次**有东西要画时由 paint pass 调 `Scene::new_lane()`，写入 `Node.lanes[0]`（`Node.lanes` 容量仍为 2，P13 的浮层再扩）      |
| 粘性    | 有图元的 lane 一旦分配即**粘性**：只要本帧仍有图元就整桶复用，不因「内容未变」重建（§5.6）                                         |
| 空 lane | **不留**：本帧无图元的节点在重建后立即 `Scene::release_lane` + 清 `Node.lanes`；下一帧凭 `REUSABLE`+`clip_key` 直接跳过（审查 N3） |
| 释放    | 节点卸载（`remove` → `removed_lanes`）或「重建后无图元」时 `release_lane`（代际 +1，旧句柄立即失效）                               |
| 写入    | 唯一写入点是 `Scene::push(lane, primitive)`（内部转 `Lane::push`，C2/C3 断言继续生效）                                             |

**为什么一节点一 lane（而不是按"背景 / 内容"拆 2–4 条）**：

1. `Node.lanes` 的预算说明 P03 的预期就是「绝大多数节点 1 条」（容量 2 是余量）；
2. 同一节点的图元只有**一个**失效原因（本节点的 `!REUSABLE`），拆多 lane 会让一次失效写多条，收益为零；
3. `Scene::order` 的展开变成 `tree.order()` 的一对一映射，`reused_lanes` 的判定简单（每节点最多 1 项）；
4. 边框叠在内容之上的顺序需求靠**lane 内顺序**表达（§5.3），不需要第二条 lane。

### 5.2 lane 内桶序（绘制序，从上到下）

```text
[0..k)   祖先 clip 前缀：root → parent 的每层 Clip(Push)（仅 Style::clip == Bounds 的祖先）
         —— 见 §5.4，为什么不是"自己的 clip"
[k]      Shadow                  （若 paint.shadow 存在；P02 §6.3.5：必须先 Shadow 后 Quad）
[k+1]    背景填充 Quad           （gradient → QuadPaint::Gradient；否则 Solid{fill,None}）
[k+2]    边框 Quad               （无 gradient 时合并进背景 Quad 的 border 字段；有 gradient 时第二个 Quad）
[k+3]    内容：GlyphRun* / Canvas / Geometry   （按 NodeKind；text 的每行每 run 一个 GlyphRun）
[..n)    祖先 clip 后缀：与前缀逆序的 Clip(Pop)
```

- **硬不变量 C1**：每条 lane 的 `clip_depth` 必须回到 0（`Lane::clear` 复位、前缀/后缀成对、`Canvas` 自己平衡）。
- **边框在内容之前**：P02 §8.5 只钉死「填充 → 边框」；内容（文本）叠在边框上沿内侧，视觉上无重叠（边框宽度在
  padding 之外），与 CSS 的 `background → border → content` 一致。
- **渐变 + 边框 = 两个 Quad**：P02 §8.5 的裁决，顺序（先填充后边框）与透明填充的写法都是契约。

### 5.3 绘制序（`Scene::order`）

```text
order = [ lane(node) for node in tree.order() if node.lanes 非空 ]
```

- `tree.order()` 是 DFS **前序**（P03 §11.5 O1–O3）⇒ 父 lane 在子 lane 之前 ⇒ **父背景先于子内容**。
- `order` 每帧重建（O(n)，`Vec<LaneId>` 复用 `Window.paint_order` 的容量）；`ORDER` 位不单独消费 —— 结构/重挂
  已经通过 `mark_structure`/`mark_subtree_paint` 清掉相关节点的 `REUSABLE`（重挂时整棵子树标 `PAINT`，P03 §9.3）。
- `order` **无重复**（`set_order` 的契约由本阶段的一对一映射构造性保证）。

### 5.4 为什么 lane 自包含「祖先 clip 前缀」

P02 §6.3.6 的判据是「lane 内部 push/pop 自成闭环，复用时语义不变」，C1 是硬不变量。若父节点只在自己的 lane 里
`Push`/`Pop`，兄弟/子 lane 根本不在该 clip 作用域内（lane 是独立桶），子孙内容不会被裁剪。

因此 clip 只能按**绝对链**展开到每条受影响的 lane 里：paint 的递归遍历（§6.0）**携带**一个 clip 栈
（只在 `Style::clip == Bounds` 的节点入栈），子节点重建 lane 时把当前栈原样写成前缀/后缀（O(depth)，depth ≤ 8）。

**代价与对策**：

| 项        | 结论                                                                                                                         |
|-----------|------------------------------------------------------------------------------------------------------------------------------|
| 重复编码  | 每层 clip = 1 条 `Primitive::Clip`（32 B）；典型 depth ≤ 1。编码器把整条链去重成 `ClipChainId`（P02 §8.7），**GPU 侧零重复** |
| 失效传播  | 祖先 clip 变化必须让整棵子树的 lane 失效（§5.6），否则复用出旧裁剪                                                           |
| lane 复用 | 复用时**不需要**重算前缀：前缀已在上一帧写进该 lane 的内容里（含它是幂等的：同样的祖先链、同样的矩形）                       |

### 5.5 lane 的生命周期（分配 / 清空 / 释放）

```text
paint(node, hidden, clip_key: (u8, Option<PhysicalRect>), clip_stack):
    let reusable = tree.dirty(node).contains(REUSABLE)
                   && tree.clip_key(node) == Some(clip_key);   // §5.6：clip 上下文失配必重建（无 lane 也可复用）
    if !reusable:
        let lane = tree.lane_of(node).unwrap_or_else(|| { let l = scene.new_lane(); tree.set_lane(node, l); l });
        scene.clear_lane(lane);                       // 保容量、复位 clip_depth
        emit_clip_prefix(lane, clip_stack)            // §5.4：祖先链（外层在前；0×0 clip 也入栈，B1）
        emit_own_primitives(lane)                     // §6；同时逐图元累积 damage
        emit_clip_suffix(lane)                        // 逆序 Pop，C1 回到 0
        tree.set_reusable(node);                      // 本帧内容与树一致
        tree.set_clip_key(node, clip_key);            // 本帧的 clip 上下文
        tree.clear_paint(node);                       // PAINT 已兑现（§9.4）
    if lane 有图元: push order(lane) else: scene.release_lane(lane); tree.clear_lane(node)   // §5.1：空节点不留 lane
    #[cfg(test-support)] log.push(LanePaintEntry { lane, node, outcome, primitives: lane 图元数 })
    //                   outcome ∈ { Allocated（首次分配）/ Rebuilt / Cleared（变空）/ Reused }
```

节点卸载（`NodeTree::remove`）：

```text
remove(node):
    block = collect_subtree_ids(node)
    parent = node.parent
    for id in block:
        parent.damage ∪= node(id).damage ∪ node(id).layout.bounds       // 旧像素必须重绘
        removed_lanes.extend(node(id).lanes.iter().copied())
    …既有清理（cache/text_slot/hoisted/arena）…
    mark_structure(parent)
```

`Window::paint_frame` 在构建 order 之前 `for lane in tree.drain_removed_lanes() { scene.release_lane(lane) }`
（`removed_lanes` 是 `Vec<LaneId>`，容量保留）。**不**做全量 lane 扫描（O(n) 每帧）去发现孤儿 lane。

### 5.6 失效规则（本阶段新增的两条）

| 变化                                                                                                                   | 必须清除/比较的东西                                                         | 实现点                                                                          |
|------------------------------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------|---------------------------------------------------------------------------------|
| 绝对布局盒变化（`absolutize` 的 `changed` 分支）                                                                       | `REUSABLE`（lane 内的 bounds 已过期）                                       | `place_subtree` 里 `target.dirty.remove(REUSABLE)`（damage 逻辑已有，P04 §7.2） |
| **祖先 clip 几何变化**（resize / 重挂 / P13 的布局写入改变了 `Clip::Bounds` 的 padding box，而子节点的绝对盒**未变**） | 每节点重建时记录的 `clip_key`（见下），paint 在 `REUSABLE` 短路**之前**比较 | `NodeTree` 侧表 `clip_keys: Vec<Option<(u8, Option<PhysicalRect>)>>`            |

**为什么不能只守 `Style.clip` 开关（B1）**：`Style.clip` 只表达"裁不裁"，而 lane 里的前缀是**具体的 rectangle**。
父节点尺寸变化（窗口 resize、分栏拖拽、虚拟列表换高）后子节点的绝对盒可能逐位不变（`absolutize` 的 `changed`
为假）⇒ `REUSABLE` 保持置位；若只比较开关，就会复用**旧矩形**的前缀，子内容被裁到错误区域。

**`clip_key` 的契约**：

```text
clip_key(node) = ( depth: u8, covering: Option<PhysicalRect> )
  depth    = 祖先链上 Style::clip == Bounds 的层数（0 = 无裁剪）
  covering = 该链在**物理像素**的 covering 交集（to_physical_covering 后逐层 intersect）
```

- paint 递归时携带 `(depth, covering)`，对每个节点在判断 `reusable` **之前**取 `tree.clip_key(node)` 比较，
  不等即重建（即使 `REUSABLE` 置位）；重建后写回当前 key；
- 存放于 `NodeTree` 的**侧表**（按 arena 槽位，`Option` 表示"从未重建"）；`insert_child` 复用槽位时置 `None`，
  `remove` 置 `None`（与 `layout_cache`/`text_slots` 同一纪律）；
- `set_style` 因此**不需要**特殊的子树失效逻辑：clip 开关变化必然改变 `depth`，矩形变化必然改变 `covering`，
  两者都被 key 捕获。

用例见 §12-C6（lane 前缀）与 §12-E5（resize/重挂 后的 clip_key 失配）、§12-E2（`remove` 的 damage 并集）。

### 5.7 与 P02 `Scene` 写面的兼容

| P02 冻结                                                                        | P06 用法                                                          | 兼容性         |
|---------------------------------------------------------------------------------|-------------------------------------------------------------------|----------------|
| `Lane` 私有 + `push/clip_depth/primitives/is_empty/clear`                       | `Scene::clear_lane` 调 `Lane::clear`；`push` 走 `Scene::push`     | ✅ 不改 `Lane` |
| `Scene::{new_lane, release_lane, push, clear_frame, set_order, clear_order}`    | 全部被 paint 使用（`clear_frame` 只被 oracle 与 force-full 使用） | ✅ 不改签名    |
| `Scene` 只读面 `lane_primitives/lane_count/order/iter/primitive_count/is_empty` | 帧记录 / oracle / 编码器                                          | ✅             |
| `Scene` 不 `Clone`/不 `Default`                                                 | 不变                                                              | ✅             |
| `MAX_CLIP_DEPTH = 8`                                                            | lane 前缀深度受它约束（`Lane::push` 的 debug 断言）               | ✅             |

---

## 6. Primitive 生成规则（paint pass）

### 6.0 `PaintPass` 的形状（`paint/pass.rs`）

```rust
/// 一帧的 lane 重建/复用（§5.5）。字段 `pub` 是 crate 内可见（lint probe §2.2 已验）。
pub(crate) struct PaintPass<'a> {
    pub tree: &'a mut NodeTree,
    pub scene: &'a mut Scene,
    pub damage: &'a mut DamageAccumulator,
    pub scale: Scale,
    /// 本帧 framebuffer 的物理矩形（`DamageAccumulator::finish` 的面积阈值用）。
    pub frame: PhysicalRect,
    /// 本帧的 lane 绘制序（复用调用方的 `Vec` 容量，§9.5 的 `Window.paint_order`）。
    pub order: &'a mut Vec<LaneId>,
    /// 本帧的 lane 重建/复用记录（仅 feature 构建；随 `PaintReport` 一起返回）。
    #[cfg(feature = "test-support")]
    pub log: PaintLog,
}

/// 一次 paint 的产物（`damage` 已在 `run` 末尾 `finish` 过）。
pub(crate) struct PaintReport {
    pub rebuilt: usize,
    pub damage: DamageSummary,
    /// 本帧编码出的实例数（仅 feature 构建：`FrameStats.instances` 的来源）。
    #[cfg(feature = "test-support")]
    pub instances: usize,
    #[cfg(feature = "test-support")]
    pub log: PaintLog,
}

impl PaintPass<'_> {
    pub(crate) fn run(&mut self) -> PaintReport;
}
```

`run` 的骨架（递归 DFS，不用 `tree.order()` 的扁平数组：隐藏子树与 clip 前缀都要**带下来**）：

```text
run():
    paint_node(root, hidden = false, clip_stack = [])       // root 也是普通节点（可带背景）
    scene.set_order(order)
    PaintReport { rebuilt, damage: self.damage.finish(self.frame), instances: 0, log: self.log }
    //                                              ↑ 仅 feature；`instances` 由 `Window::paint_frame` 编码后回填（§9.6）

paint_node(node, hidden, clip_stack):
    hidden |= node.style().inner.display == Display::None
    if hidden: 清空 lane（若有）并 set_reusable + clear_paint；return（不遍历子节点）
    reusable = tree.dirty(node).contains(REUSABLE) && lane 已存在
    if !reusable: rebuild(node, lane, clip_stack)   // §5.5 + §6.1–§6.4；rebuilt += 1
    push order(lane)（若有）
    if node.style().clip == Clip::Bounds: clip_stack.push(padding_box(node))
    for child in node.children(): paint_node(child, false, clip_stack)
    if 刚 push 过: clip_stack.pop()
```

`rebuild` 内部对**每个**图元 push 前执行 `acc.push(bounds, pad, clip_aabb)`（§6.6），`clip_aabb` 由 `clip_stack`
的物理 covering 交集算出（`None` = 无裁剪）。

### 6.1 每节点的求值输入

```text
base  = node.paint_spec()                 // Option<&PaintSpec>，静态
patch = node.style_patch()                // 动态（bind_style 上次求值）
visible    = patch.visible.unwrap_or(true)
opacity    = patch.opacity.unwrap_or(1.0).clamp(0.0, 1.0)      // NaN → 0
background = patch.background.or(base.background).map(|c| c.with_opacity(opacity))
border     = patch.border.or(base.border).filter(|b| b.width > 0.0)
              .map(|b| Border { width: b.width, color: b.color.with_opacity(opacity) })
radii      = base.corner_radius          // 逻辑像素，编码期 clamp（§7.3）
shadow     = base.shadow.map(|s| Shadow { rect: bounds, offset: s.offset, blur: s.blur.max(0.0),
                                          color: s.color.with_opacity(opacity) })
gradient   = base.gradient.map(|g| Gradient { from: g.from.with_opacity(opacity),
                                              to: g.to.with_opacity(opacity), kind: g.kind })
```

`bounds = node.layout().bounds`（border box，窗口逻辑坐标）；`content_origin = bounds.origin + node.content_offset()`。

**`opacity` 的语义（v1 明确记为逐图元 alpha，非 group opacity）**：字形/背景/边框/阴影/渐变各自乘 alpha；
`Canvas` 内部的图元**不**参与（应用自绘内容自带 alpha）。→ V-51。

### 6.2 逐类生成表

| 节点/条件                          | 生成的 `Primitive`（顺序即绘制序）                                                                                               |
|------------------------------------|----------------------------------------------------------------------------------------------------------------------------------|
| `NodeKind::Box` + `shadow`         | `Shadow(..)`                                                                                                                     |
| … + `gradient`                     | `Quad { bounds, radii, paint: Gradient(..) }`                                                                                    |
| … + `background`（合并后非全透明） | `Quad { bounds, radii, paint: Solid { fill, border: 无 gradient 时的 `border` } }`                                               |
| … + `border` **且** 有 gradient    | `Quad { bounds, radii, paint: Solid { fill: TRANSPARENT, border: Some(b) } }`                                                    |
| … + `border` **且** 无 background  | `Quad { bounds, radii, paint: Solid { fill: TRANSPARENT, border: Some(b) } }`（只画边框）                                        |
| `NodeKind::Text`                   | 每个 `ShapedTextItem::GlyphRun` → 一个 `GlyphRun`（每字形 1 实例，§7.4）；`ShapedTextItem::InlineBox` **跳过**（行内图元属 P15） |
| `NodeKind::Canvas(c)`              | `Primitive::Canvas(c.clone())`（**整块**，编码期展平）；空 `Canvas` 也 push（`bounds` 参与 damage，P02 §11.3）                   |
| `NodeKind::Geometry(id)`           | `Primitive::Geometry(GeometryDraw { id, transform: Transform::IDENTITY })`（裁决 ④）                                             |
| `visible == Some(false)`           | 不生成任何图元（lane 被清空）；damage 仍要 push（旧像素）                                                                        |
| 自身或祖先 `display: none`         | 整棵子树跳过（lane 清空）                                                                                                        |
| `bounds` 为空                      | 不生成（`Rect::is_empty`）；隐藏子树在 `absolutize` 里已零化                                                                     |

**颜色解析时机**：paint 阶段只写 `Color`（未预乘）；`ColorResolver::resolve` 只在编码期与 `QuadRecord`
（test-support）里调用 —— 与 P02 §8.8「颜色每帧重新 resolve」一致，不引入 lane 内的颜色缓存（V-49）。

**「不产生像素」的判定统一用 `Color::is_transparent`**（alpha `<= 0`；NaN 视为透明）：背景、边框、渐变两端、
阴影、字形色在 paint 阶段就过滤掉，因此 lane 里不出现"永远不画"的图元（damage 也不必为它们开 rect）。
编码器仍保留 P02 §8.6 的结构性跳过（空 bounds / 空字形），两处判据同一函数，不会分叉。

### 6.3 文本：`ShapedLine` → `GlyphRun`（逐字）

```text
let Some(line) = tree.text_line(node) else { continue };      // Rc<ShapedLine>，P05
let origin = node.layout().bounds.origin + node.content_offset();
let foreground = patch.foreground;                            // 可选覆盖
for sh_line in line.lines.iter():
    for item in sh_line.items.iter():
        match item:
            InlineBox(_) => continue
            GlyphRun(run) => {
                if run.glyphs.is_empty() { continue }
                let color = foreground.unwrap_or(run.color).with_opacity(opacity);
                let mut glyphs = Vec::with_capacity(run.glyphs.len());
                for g in run.glyphs { glyphs.push(PositionedGlyph {
                    tile: g.tile,
                    position: Point::new(origin.x + g.position.x, origin.y + g.position.y),
                    size: g.size,
                }); }
                scene.push(lane, Primitive::GlyphRun(GlyphRun { color, glyphs }));
            }
```

- `content_offset = (border.left + padding.left, border.top + padding.top)`，在 `set_local_layout` 写入
  （taffy 的 `layout.border` / `layout.padding` 已解析完毕）。这是对 P05 S-54「加 `bounds.origin`」的**细化**
  （F-64）：有 padding 的文本节点否则会整体左上偏移。
- 每行每 run 一个 `GlyphRun`：行序 × run 序 = 绘制序；`GlyphRun` 的 `PartialEq` 保持 P02 语义（oracle 直接比较）。
- 空文本 / 空 `ShapedLine` → 0 个图元（lane 清空）。

### 6.4 `Canvas` 与 `Geometry`

| 项                   | 规则                                                                                                                                                            |
|----------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `Canvas`             | 每次 lane 重建时 `Canvas::clone`（48 B + `Vec<Primitive>` 深拷贝）。大画布（> 1k 图元）的成本见 V-50；P15 若成热点改 `NodeKind::Canvas(Rc<Canvas>)`（additive） |
| `Canvas` 的 clip     | `Canvas` 内部的 `Clip(Push/Pop)` 由编码器在展平时处理，且必须在 `Canvas` 内平衡（P02 §8.6 的 `assert!(depth == entry_depth)`）                                  |
| `Canvas` 的 damage   | 只按 `Canvas::bounds()` push 一次（P02 §11.3 契约：bounds ⊇ 子图元并集）                                                                                        |
| `Geometry`           | 0 实例；编码器 `flush_run()` 后追加 `FrameOp::Geometry(index)`（§7.5）。transform 恒 `IDENTITY`（裁决 ④）                                                       |
| `Geometry` 的 damage | 用节点 `layout.bounds`（P09 有 `GeometryDesc::bounds` 后再精化）                                                                                                |

### 6.5 边框宽度与 `LayoutBox.border` 的两个真值来源（裁决）

`LayoutBox.border: Edges`（taffy 解析结果，P04）与 `PaintSpec.border: Border{f32,Color}`（绘制参数）都能表达宽度。

**结论**：绘制只用 `paint_spec/patch` 的 `Border`；`LayoutBox.border` **只**用于（a）clip 的 padding box、
（b）`content_offset`。理由：①v1 的 `Border` 是单宽度（P02 冻结），若与四边宽度混用会产生"布局留了 3 px、
画了 1 px"的洞；②两个来源在生产侧不需要互相解释，应用要么用 taffy `border` 留空间，要么用 `PaintSpec.border`
画内描边（两者不一致时由应用负责）。文档写进 `PaintSpec::border` 的 rustdoc。

### 6.6 damage 累积（paint 阶段的输入侧）

**节点级**（每个 `!REUSABLE` 的节点）：

```text
acc.push(node.damage, 1.0, clip_aabb)                 // 旧像素 + 布局移动（P03 §9.4 已并 prev ∪ layout）
```

**图元级**（同一节点重建后的 lane 内，逐图元）：

| 图元                  | `bounds`                             | `aa_padding`（物理像素） | 依据                 |
|-----------------------|--------------------------------------|--------------------------|----------------------|
| `Quad`（含边框/渐变） | `quad.bounds`                        | `1.0`                    | P02 §12.3            |
| `Shadow`              | `shadow.rect.translate(offset)`      | `blur × s + 1.0`         | 同                   |
| `GlyphRun`            | 全部字形的并集                       | `0.0`                    | 图集整数矩形         |
| `Image`               | `image.bounds`                       | `0.0`                    | 同                   |
| `Path` / `Geometry`   | 节点 `layout.bounds`（P06 无 store） | `1.0`                    | 保守                 |
| `Canvas`              | `canvas.bounds()`（**不展开**）      | `1.0`                    | P02 §11.3            |
| `Clip`                | **不 push**                          | —                        | N13：clip 不产生像素 |

`clip_aabb` = 祖先链 clip 的交集（物理像素，`to_physical_covering`），无祖先 clip 时 `None`。

**为什么节点级与图元级都要**：节点级覆盖"旧内容消失"（节点变透明/尺寸缩小/图元减少），图元级覆盖"阴影
模糊超出布局盒"（`blur` 可达 32 px，节点盒只扩 1 px 会漏）。两者重叠时由合并规则吸收。

---

## 7. `paint/instance.rs`（`GpuPrimitive` 契约 + CPU 编码）

> 本节是 `render/shaders/contract.md`（P10）的**权威来源**（I2/I8）。字段顺序/offset/取值表的任何改动必须同时
> 改 Rust、三套 shader、`contract.md`，并触发像素快照回归。

### 7.1 结构体与静态断言（逐字）

```rust
/// uber-quad 的单实例。**128 B / 16 B 对齐**，三平台共用同一份字节布局。
#[repr(C, align(16))]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GpuPrimitive {
    pub bounds: [f32; 4],          // 0
    pub corner_radius: [f32; 4],   // 16
    pub color0: [f32; 4],          // 32
    pub color1: [f32; 4],          // 48
    pub params0: [f32; 4],         // 64
    pub params1: [f32; 4],         // 80
    pub uv: [f32; 4],              // 96
    pub clip_rect: [f32; 4],       // 112
}

const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(size_of::<GpuPrimitive>() == 128);
    assert!(align_of::<GpuPrimitive>() == 16);
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

probe 已实测通过（§2.1）。**`pub` 字段 + `pub(crate)` 类型**经 lint probe 验证不触发
`field_scoped_visibility_modifiers`（§2.2）。

### 7.2 `kind` / `flags` / 常量（P02 §8.3 逐字，不新增值）

```rust
pub(crate) const KIND_QUAD: f32 = 0.0;
pub(crate) const KIND_GLYPH: f32 = 1.0;
pub(crate) const KIND_IMAGE: f32 = 2.0;
pub(crate) const KIND_SHADOW: f32 = 3.0;

pub(crate) const FLAG_OPAQUE: f32 = 1.0;      // bit 0
pub(crate) const TEXTURE_GLYPH: f32 = 0.0;
pub(crate) const TEXTURE_IMAGE: f32 = 1.0;

pub(crate) const GRADIENT_NONE: f32 = 0.0;
pub(crate) const GRADIENT_HORIZONTAL: f32 = 1.0;
pub(crate) const GRADIENT_VERTICAL: f32 = 2.0;
pub(crate) const GRADIENT_RADIAL: f32 = 3.0;
```

`params1` 的四个分量是「整数编码进 f32」（`value as f32`，`[0, 2^24)` 精确）；
`flags` 的 `OPAQUE` 判据（P02 §8.4 末尾，编码器唯一实现）：

```text
opaque = kind ∈ {QUAD, GLYPH, IMAGE}
         && params0.z == 0                     // 非渐变
         && color0.is_opaque()
         && (kind != QUAD || params0.x == 0.0 || color1.is_opaque())
```

### 7.3 逐字段映射（`Primitive` → `GpuPrimitive`）

记 `s = scale.factor()`，`R = ColorResolver::resolve`，`f32r = Rect::to_physical_f32`，
`phys = Rect::to_physical`（P01 §7.1）。

| 字段            | `Quad`                                                | `GlyphRun`（每字形 1 个）              | `Shadow`                              |
|-----------------|-------------------------------------------------------|----------------------------------------|---------------------------------------|
| `bounds`        | `f32r(q.bounds)`                                      | `phys(字形矩形)` 的 4 分量；**整数值** | `f32r(shadow.rect.translate(offset))` |
| `corner_radius` | `radii × s`，逐角 clamp `<= min(w,h)/2`（物理域）     | 全 0                                   | 全 0                                  |
| `color0`        | `R(Solid.fill)` / `R(Gradient.from)`                  | `R(run.color)`                         | `R(shadow.color)`                     |
| `color1`        | 边框色 / 渐变终色 / 0                                 | 0                                      | 0                                     |
| `params0.x`     | `border.width × s`（clamp `<= min(w,h)/2`），无边框 0 | 0                                      | 0                                     |
| `params0.y`     | 0                                                     | 0                                      | `blur × s`                            |
| `params0.z`     | `GRADIENT_*`，实色 0                                  | 0                                      | 0                                     |
| `params0.w`     | `1.0`                                                 | `0.0`                                  | `blur × s + 1.0`                      |
| `params1.x`     | 链 id                                                 | 链 id                                  | 链 id                                 |
| `params1.y`     | 0                                                     | `TEXTURE_GLYPH`                        | 0                                     |
| `params1.z`     | `KIND_QUAD`                                           | `KIND_GLYPH`                           | `KIND_SHADOW`                         |
| `params1.w`     | `OPAQUE?`                                             | `OPAQUE?`                              | 0                                     |
| `uv`            | 0                                                     | `tile.uv()`                            | 0                                     |
| `clip_rect`     | 链 AABB                                               | 同                                     | 同                                    |

`Primitive::Image` 同样映射（`phys(bounds)`、`R(modulation)`、`TEXTURE_IMAGE`）；P06 不生产 `Image`
（无图片资产），但编码器必须实现它（`Canvas::push_image` 是公开写入口，oracle 与 P15 会用到）。

`Path` / `Geometry` / `Clip` / `Canvas`：**0 实例**，见 §7.5/§7.6。

### 7.4 1→N：`GlyphRun`

- 判定依据（P02 §6.2）：`GpuPrimitive` 只有一个 `uv` ⇒ 一个字形一个实例；
- 实例数 = 字形数（`PositionedGlyph` 数量，含空字形已由 P05 过滤）；
- `run.glyphs` 为空 → 0 实例（且不产生 `FrameOp`）；
- 单个 `GlyphRun` 的 N 个实例**连续**（不打断 `Instances` 段），逐个 `push`；
- `R(GlyphRun.color)` 对同 run 的 N 个实例相同（重复 resolve 可接受；V-49）。

### 7.5 编码顺序与跳过规则（逐字）

```text
encode(scene, scale, resolver) -> &InstanceBuffer:
    buffer.clear();  buffer.push_clip_sentinel()             // 表项 0 = 无裁剪
    stack: SmallVec<[ClipChainId; 8]> = [];  depth = 0
    for lane in scene.order():
        entry_depth = depth
        for p in scene.lane_primitives(lane).unwrap_or(&[]):
            match p:
                Clip(Push(c))  => { debug_assert!(depth < MAX_CLIP_DEPTH); let id = chain_of(c); stack.push(id); depth += 1 }
                Clip(Pop)      => { debug_assert!(depth > entry_depth); stack.pop(); depth -= 1 }
                Quad(q)        => if quad_visible(q) { emit_quad(q) }
                Shadow(sh)     => if !sh.color.is_transparent() { emit_shadow(sh) }
                Image(im)      => if !im.modulation.is_transparent() && !im.bounds.is_empty() { emit_image(im) }
                GlyphRun(run)  => if !run.color.is_transparent() { for g in run.glyphs { emit_glyph(run.color, g) } }
                Path(p)        => { flush_run(); push_op_geometry(p.geometry, Transform::IDENTITY) }
                Geometry(g)    => { flush_run(); push_op_geometry(g.id, g.transform) }
                Canvas(c)      => { assert!(canvas_depth == 0);
                                    for p2 in c.primitives() { 同上（可递归 Clip/Quad/…，不得再遇 Canvas） }
                                    assert_eq!(depth, entry_depth) }
        assert_eq!(depth, entry_depth)                        // C1（lane clip-balanced）
    flush_run()
    self_check()                                              // §7.8
```

`flush_run()`：把当前连续实例段写成 `FrameOp::Instances { start, len }`（`len == 0` 时不写），保证
`FrameOp` 顺序 = 绘制顺序。clip 变化**不** flush（实例自带 `clip_index`，P02 §8.6）。

**跳过规则**（P02 §8.4 逐条；「透不透明」的判定由 `Color::is_transparent` 统一给出）：

| 条件                                                  | 处理                                               |
|-------------------------------------------------------|----------------------------------------------------|
| 主色透明且无边框/无渐变（`Solid{TRANSPARENT, None}`） | 跳过                                               |
| `bounds.is_empty()`（含非有限/负尺寸换算为空）        | 跳过                                               |
| `GlyphRun.glyphs` 为空 / `run.color` 透明             | 跳过                                               |
| `Canvas` 为空                                         | 展开后自然 0 实例（但 `Canvas` 本身仍参与 damage） |
| `Path`/`Geometry` 的 id 失效                          | **不查表**（编码器没有 store，P02 §8.6）           |

```text
quad_visible(q) = !q.bounds.is_empty() && match q.paint {
    Solid { fill, border } => !fill.is_transparent()
                              || border.is_some_and(|b| !b.color.is_transparent()),
    Gradient(g)            => !(g.from.is_transparent() && g.to.is_transparent()),
}
```

### 7.6 clip 链（`ClipChain`）

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClipChainId(u32);

#[derive(Clone, Copy, Debug)]
pub(crate) struct ClipChain {
    parent: Option<ClipChainId>,
    rect: [f32; 4],            // 本层物理浮点 (x, y, w, h)
    corner_radius: [f32; 4],
    aabb: [f32; 4],            // root→本层的交集 AABB（写进 GpuPrimitive.clip_rect）
}
```

| 规则     | 内容                                                                         |
|----------|------------------------------------------------------------------------------|
| 表项 0   | 无裁剪哨兵：`parent=None`、`aabb = [f32::MIN, f32::MIN, f32::MAX, f32::MAX]` |
| 去重键   | `(parent, rect, corner_radius)`（`ahash::HashMap`，每帧清空）                |
| 生命周期 | 帧内索引（`buffer.clear()` 时清空）；不进 `Scene`（lane 复用安全）           |
| 深度     | ≤ `MAX_CLIP_DEPTH`（8）                                                      |

### 7.7 `InstanceBuffer`（容量策略）

```rust
pub(crate) struct InstanceBuffer {
    instances: Vec<GpuPrimitive>,
    clips: Vec<ClipChain>,
    geometry: Vec<GeometryDraw>,
    ops: Vec<FrameOp>,
}

impl InstanceBuffer {
    pub(crate) fn new() -> Self;                       // Vec::with_capacity(256 / 8 / 8 / 16)
    pub(crate) fn clear(&mut self);                    // 各 Vec 截断到 0，**容量保留**
    pub(crate) fn len(&self) -> usize;                 // instances.len()
}
```

**容量策略（结论）**：容量 = **历史峰值**，`clear()` 只截断、不 shrink、不按帧翻倍。P02 §5.5 的「不出现
`2^n` 跨档翻倍」针对的是**每帧上传的顶点缓冲**；CPU 侧的 `Vec` 在首帧/极端帧最多一次 realloc，稳态零分配。
不设硬上限（截断会静默丢像素，违反 I2 的「无静默降级」）；超预算的拆分属 P09（多段上传）。

### 7.8 `InstanceEncoder` 与自检

```rust
pub(crate) struct InstanceEncoder {
    buffer: InstanceBuffer,
    stack: SmallVec<[ClipChain; 8]>,
    chains: ahash::HashMap<(Option<ClipChainId>, [u32; 4], [u32; 4]), ClipChainId>,
}

impl InstanceEncoder {
    pub(crate) fn new() -> Self;
    /// 编码整帧；内部缓冲复用。
    pub(crate) fn encode(&mut self, scene: &Scene, scale: Scale, resolver: &ColorResolver) -> &InstanceBuffer;
}

// encode 末尾（唯一读点，见 §4.4）：
//   debug_assert_eq!(Σ FrameOp::Instances{len}, buffer.instances.len());
//   debug_assert!(每个 FrameOp::Geometry{index,..} 的 index < buffer.geometry.len());
//   debug_assert!(每个 ClipChain.parent 的下标 < 自身下标);
```

`clip_rect` 的四分量来自链 AABB（`to_physical_f32` 的 clip 矩形求交，物理浮点）；
`uv` 来自 `AtlasTile::uv()`（P02 §10.2 的边约定）。

---

## 8. `render/damage.rs`（`DamageAccumulator`）

### 8.1 输入 / 输出（相对 P02 §12.1 的收紧）

```rust
/// 一次帧末的 damage 汇总（P06 的**唯一**输出形状）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DamageSummary {
    /// 是否退化为全帧（resize / DPI / 主题 / 阈值逃逸）。
    pub full: bool,
    /// 合并后的脏矩形数（`full == true` 时为 0，无意义）。
    pub rects: usize,
}

pub(crate) struct DamageAccumulator {
    scale: Scale,
    rects: Vec<PhysicalRect>,
    full: bool,
}

pub(crate) const MAX_DAMAGE_RECTS: usize = 16;
pub(crate) const FULL_FRAME_AREA_RATIO: f32 = 0.5;
```

| 方法                                                              | 语义                                                  | 消费者                                         |
|-------------------------------------------------------------------|-------------------------------------------------------|------------------------------------------------|
| `new(scale)`                                                      | 空累加器                                              | `Window::new`                                  |
| `reset(scale)`                                                    | 清空（保容量）、按新 scale 重置                       | `Window::paint_frame`（帧初）                  |
| `push(bounds: Rect, aa_padding: f32, clip: Option<PhysicalRect>)` | 逻辑矩形 → covering → 物理外扩 → clip 求交 → 增量合并 | `PaintPass`                                    |
| `push_full()`                                                     | 强制全帧                                              | `Window::paint_frame`（`full_damage_pending`） |
| `finish(frame: PhysicalRect) -> DamageSummary`                    | 不动点合并 + 16/50% 判定                              | `Window::paint_frame`                          |
| `rects() -> &[PhysicalRect]`（`cfg(test-support)`）               | 帧记录                                                | `FrameRecord` 捕获                             |

> **不提供** `is_empty`/`is_full`：唯一门禁读的是 `DamageSummary` 的两个字段（`full` / `rects`），
> 让字段本身在**所有构建**都有读者（§4.4 的死代码纪律）。

**与 P02 §12.1 的差异（裁决 ② / F-61）**：不落 `DamageOutput` 枚举与 `ScrollHint`/`push_scroll`：

- `DamageOutput` 的唯一消费点（耗尽式交出 `Vec`）会把容量送走，与 §12.6「reset 保容量」冲突；
- scroll hint 的真实数据源（`Transient` 滚动偏移）在 P12 才存在，P06 落一个永远为 `None` 的字段违反
  AGENTS §2.1（不得定义未使用字段）；
- P07 的 `Present1` 翻译只需要 `(is_full, &[PhysicalRect])`，现有的 `DamageSummary` + `rects()` 已足够；
  需要「拥有式」结果的只有平台翻译层，届时 additive 加 `take_rects()`。

### 8.2 算法（逐字，P02 §12.2 的实现版）

```text
push(bounds, aa_padding, clip):
  1. r = bounds.to_physical_covering(scale)                      // 非有限/负尺寸 ⇒ 空矩形（P01 §6.3.3）
  2. if r.is_empty() { return }
  3. pad = ceil(max(aa_padding, 0.0))
  4. r = PhysicalRect::new(r.x0 - pad, r.y0 - pad, r.x1 + pad, r.y1 + pad)
  5. if let Some(c) = clip { r = r.intersect(c); if r.is_empty() { return } }
  6. if r.is_empty() { return }
  7. if let Some(existing) = rects.iter_mut().find(|a| area(a.union(r)) <= area(a) + area(r)) {
         *existing = existing.union(r)                          // 增量合并（只与第一个可合并项）
     } else { rects.push(r) }
  8. if rects.len() > MAX_DAMAGE_RECTS { full = true; rects.clear() }

finish(frame):
  1. if full → DamageSummary { full: true, rects: 0 }
  2. loop 直到不动点：按 (y0, x0) 升序排序，两两扫描，可合并则合并并重来
  3. if rects.len() > MAX_DAMAGE_RECTS → full
  4. if Σ area > frame.area() as f32 * FULL_FRAME_AREA_RATIO → full
  5. DamageSummary { full, rects: rects.len() }
```

`PhysicalRect::area() -> i64`（`width() as i64 * height() as i64`，避免 `i32` 溢出）是本阶段在 `math.rs`
新增的**唯一**方法（damage 阈值 + 测试断言共用）。

### 8.3 外扩量（唯一来源，与 `params0.w` 一致）

见 §6.6 的表。**物理域 ceil**，不在逻辑域 `Rect::outset`（P01 §7.2）。

### 8.4 合并规则（确定性）

`area(a ∪ b) <= area(a) + area(b)`（相交或共边）才合并；L 形拒绝；每轮排序 `(y0, x0)` ⇒ 同输入同输出（I8 前提）。

### 8.5 阈值与 Full

| 条件                         | 结果                                                       |
|------------------------------|------------------------------------------------------------|
| 无输入                       | `full=false, rects=0`（**不**退化为 Full；反向证明空闲帧） |
| `rects > 16`                 | `full=true`                                                |
| `Σarea > 0.5 × frame.area()` | `full=true`                                                |
| `push_full()`                | `full=true`（清空 rects）                                  |
| `frame.area() == 0`          | 任何非空 rects 都满足 50% ⇒ `full=true`（合法退化）        |

---

## 9. 帧调度（headless）

### 9.1 类型（`window.rs`）

```rust
/// 单调递增的提交序号。**0 = 从未提交**（非法值），首帧 = 1。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameIndex(u32);

impl FrameIndex {
    /// 原始值（`0` 只在 `Default` 里出现；`commit` 的 debug 断言保证提交序号 ≥ 1）。
    #[must_use]
    pub const fn get(self) -> u32;
}

/// headless 版帧状态机（设计的 10 态在一次同步调用内部，见 F-66）。
/// **`pub(crate)`**：P06 没有公开观察者（审查 N3；P09 需要时再公开）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameState { Idle, Pending, Draining }

/// 一帧的最终结果（`FrameRecord` 只存在于 test-support）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameOutcome {
    /// 本帧没有像素要画：不跑 pass、不编码、不提交、`FrameIndex` 不变（I3）。
    Idle,
    /// 已提交；序号 = 本次提交后的 `FrameIndex`。
    Committed(FrameIndex),
}

pub(crate) struct FrameTicket(());          // 只能由 `FrameScheduler::begin` 铸出

pub(crate) struct FrameScheduler {
    state: FrameState,
    committed: u32,      // 已提交帧数（= 最后一个合法 FrameIndex）
}

impl FrameScheduler {
    pub(crate) fn new() -> Self;
    /// `Idle → Pending`；返回是否真的发生了转换（幂等）。
    pub(crate) fn request_frame(&mut self) -> bool;
    /// `Pending → Draining`；非 Pending → `None`（调用方须先 `request_frame`）。
    pub(crate) fn begin(&mut self) -> Option<FrameTicket>;
    /// `Draining → Idle`，`committed += 1`，返回新序号（**唯一**递增点）。
    pub(crate) fn commit(&mut self, ticket: FrameTicket) -> FrameIndex;
    /// `Draining → Idle`，序号不变（空闲帧 / 错误中止）。
    pub(crate) fn abort(&mut self, ticket: FrameTicket);
    pub(crate) fn committed(&self) -> u32;
}
```

**为什么不是设计的 10 态**：`Drain → Invalidate → Layout → Paint → Damage → Submit` 这些步骤在一次同步的
`frame()` 调用内部顺序执行，**没有观察者**、没有可插入的异步边界（没有平台事件循环）；把它们做成状态只会
制造"永远观察不到"的变体。`WaitGpu`/in-flight 属 P09（那时状态机扩变体，additive）。

**in-flight 轮转（契约，不落代码）**：`FrameIndex` 只在 `commit` 递增；一帧的序号在其生命周期内不变；
P09 以 `index.get() % MAX_FRAMES_IN_FLIGHT(3)` 选 ring 槽，`retire` 时释放。P06 不落 ring（无 GPU 消费者）。

### 9.2 一帧的完整流水（`App::frame`）

```text
App::frame(window) -> Option<Result<FrameOutcome, EngineError>>:
  1. if !self.windows.contains_key(&window) → None              // 窗口不存在（不消费 wake）
  2. let wake = self.take_wake_request()                       // 唯一消费点（P03 §14.3）
  3. if !self.window(window)?.needs_frame(wake):
         #[cfg(feature)] self.window_mut(window)?.record_idle();
         return Some(Ok(FrameOutcome::Idle))                    // I3：零帧
  4. let ticket = self.window_mut(window)?.begin_frame()        // request_frame + begin（§9.1）
  5. let styles = self.refresh_styles(window)                   // Option<StylePassReport>
  6. let text = self.refresh_text(window)                       // Option<Result<TextPassReport, _>>
         → Err → abort + return Some(Err)
  7. let layout = self.window_mut(window)?.layout_if_dirty()     // 仅当有 LAYOUT：compute_layout
         → Err(InvalidMeasure) → abort + return Some(Err)
  8. let text_layout = self.layout_text(window)                 // Option<Result<TextLayoutReport, _>>
         → Err → abort + return Some(Err)
  9. let paint = self.window_mut(window)?.paint_frame()          // lane 重建/复用 + damage + 编码（§9.6）
  10. #[cfg(feature)] self.window_mut(window)?.set_frame_stats(FrameStats {
         styles, text: Some(text), text_layout: Some(text_layout), layout,
         painted_lanes: paint.rebuilt,
         reused_lanes: paint.log 中 Reused 的条数,
         primitives: self.window(window)?.scene().primitive_count(),
         instances: paint.instances,
     });
 11. Some(Ok(self.window_mut(window)?.commit_frame(ticket, paint)))  // settle + FrameIndex + 记录；零像素则 Idle
```

> 借用编排（P05 N2 的同一纪律）：`refresh_styles` / `refresh_text` / `layout_text` 都收 `&mut self` + `WindowId`，
> 因此**不能**把 `&mut Window` 跨过 App pass 长期保存；每一步重新 `window_mut(window)`（`ahash::HashMap` 查找，
> 代价可忽略）。上面的 `?` 在「窗口消失」时是 abort 语义 —— `&mut self` 下不可能发生，仅作防御。

`Window::frame()`（公开，**无 App 通道**）：

```text
Window::frame() -> Result<FrameOutcome, EngineError>:
  1. if !self.needs_frame(false) → Ok(Idle)                     // 记录 idle（cfg）
  2. debug_assert!(!self.tree.any_node(|n| n.has_bindings()),
       "Window::frame 不跑样式/文本 pass；含 bind_style/bind_text 的窗口必须用 App::frame");
  3. ticket = begin_frame(); layout = layout_if_dirty()?; paint = paint_frame();
  4. 同上的 commit/abort
```

`App::frame` 是**产品路径**；`Window::frame` 是 P06 的测试/P07 快速路径，二者的窗口侧步骤是同一份代码。

### 9.3 `needs_frame`（两段门禁的第一段）

```rust
pub(crate) fn needs_frame(&self, wake: bool) -> bool {
    wake
        || self.full_damage_pending
        || self.tree.any_node(|n| {
        let d = n.dirty();
        d.intersects(DirtyFlags::LAYOUT | DirtyFlags::PAINT) || !d.contains(DirtyFlags::REUSABLE)
    })
}
```

第二段门禁在 `commit_frame` 的入口：`paint.rebuilt == 0 && !paint.damage.full && paint.damage.rects == 0`
→ `abort` + 返回 `FrameOutcome::Idle`（**没有像素要画的 PAINT/A11Y 变化不会提交帧**）。`commit` 内部
`debug_assert!(index.get() >= 1)` 是 `FrameIndex::get` 的非 test 读点。

### 9.4 `REUSABLE` / damage / `prev_layout` 的复位点（**结论：提交时**）

| 时机                                       | 动作                                                                                           | 理由            |
|--------------------------------------------|------------------------------------------------------------------------------------------------|-----------------|
| drain（`App::write` / `NodeTree::mark_*`） | `touch` 清 `REUSABLE`、并 damage                                                               | P03 已有        |
| 帧内（样式/文本/布局）                     | 继续清 `REUSABLE`（`mark_layout`、`absolutize` 的 `changed`）                                  | 本阶段新增 §5.6 |
| 提交（`settle_frame`）                     | 每个节点：`damage = ∅`；`prev_layout = layout`；`dirty.insert(REUSABLE)`；`dirty.remove(LAYOUT | PAINT)`         | 见下 |

**`settle_frame` 只清「本帧已消费」的位**：`LAYOUT`（布局 pass 已完成，taffy 对缓存命中的子树不再写 `set_local_layout`，
所以由结算统一清）与 `PAINT`（paint 的 hidden 分支也清、`refresh_styles` 对静态节点也清）。**帧内不得 `mark_*`**
（`&mut self` 已把 `App::write` 挡在帧外；`bind_*` 闭包只拿 `ReadCtx`），因此结算清这两位不会丢合法的待办；
`A11Y`/`ORDER`/`GEOMETRY` 保留（它们的消费者不在本帧）。

**为什么不在帧初清零**：`drain`（写信号 → `touch` 并 damage）发生在 `frame()` **之前**。若帧初把 `damage`
清零，drain 阶段累积的旧像素 damage 会被吞掉，表现为"改了颜色但旧位置没重绘"。设计 §3.4 的"一帧开始时所有
节点默认 `REUSABLE`"是**概念模型**（失效发生在 drain），实现上必须把复位放在消费之后 ⇒ F-59。

`prev_layout = layout` 在提交时做：让"只改颜色"的节点下一次 damage 只覆盖**当前位置**，而不是把它历史上
所有位置都并进来（P04 的 `place_subtree` 只在变化时写 `prev_layout`，不写就会一直保留更早的值）。

### 9.5 `Window` 的帧侧字段增量

| 字段                  | 类型                                       | 用途                                                                                          |
|-----------------------|--------------------------------------------|-----------------------------------------------------------------------------------------------|
| `scheduler`           | `FrameScheduler`                           | §9.1                                                                                          |
| `encoder`             | `paint::instance::InstanceEncoder`         | 每帧编码（容量持久）                                                                          |
| `damage`              | `render::damage::DamageAccumulator`        | §8                                                                                            |
| `resolver`            | `paint::color::ColorResolver`              | 编码期 `resolve`（P14 主题的注入点；P06 用 `Interpolation::default()`）                       |
| `paint_order`         | `Vec<LaneId>`                              | order 复用（避免每帧分配）                                                                    |
| `full_damage_pending` | `bool`                                     | `set_size`/`set_scale` 置位；`paint_frame` 消费 → `push_full()`                               |
| `stats`               | `cfg(test-support)`，`FrameStats`          | `App::frame` 在提交前写入的暂存区（`set_frame_stats`）；`commit_frame` 把它搬进 `FrameRecord` |
| `record`              | `cfg(test-support)`，`Option<FrameRecord>` | 最近一次 `drain` 的结果（`Window::record()` 只读）                                            |

窗口侧的两个只读出口：

```rust
impl Window {
    /// 提交游标（0 = 从未提交；空闲帧不变）。
    #[must_use]
    pub fn frame_index(&self) -> FrameIndex;
    /// 强制全量重绘（oracle 的「全量」路径；仅 `test-support`）。
    #[cfg(feature = "test-support")]
    pub(crate) fn force_full_repaint(&mut self);
}
```

> 帧状态机本身不需要访问器：测试用 `FrameOutcome` 与 `frame_index()` 断言（成功提交 → 序号 +1；
> 空闲/中止 → 序号不变），避免为一次性断言引入只被 `#[cfg(test)]` 读的方法（会在
> `cargo build --features test-support` 里报 dead_code）。

### 9.6 `paint_frame` 的逐字步骤（窗口侧的唯一实现）

```text
Window::paint_frame() -> PaintReport:
  1. for lane in self.tree.drain_removed_lanes() { self.scene.release_lane(lane) }   // §5.5
  2. let scale = self.scale;
     let frame = Rect::new(0.0, 0.0, self.size().width, self.size().height)
                    .to_physical_covering(scale);                                    // §8.2 的面积阈值输入
  3. self.damage.reset(scale);
     if self.full_damage_pending { self.damage.push_full(); self.full_damage_pending = false; }   // S-64
  4. self.paint_order.clear();
  5. let mut report = PaintPass {
         tree: &mut self.tree, scene: &mut self.scene, damage: &mut self.damage,
         scale, frame, order: &mut self.paint_order,
         #[cfg(feature = "test-support")] log: PaintLog::default(),
     }.run();                                                                         // §6.0
  6. let buffer = self.encoder.encode(&self.scene, scale, &self.resolver);           // §7.5（P07 消费）
     #[cfg(feature = "test-support")] { report.instances = buffer.len(); }
  7. report
```

**错误面**：`paint_frame` 不返回 `Result`（唯一可能的失败是编码器的 `debug_assert` 与 `Lane` 的 clip 断言，都是
编程错误）。布局/文本错误在 `layout_if_dirty` / `layout_text` 阶段返回，`App::frame` 在那里 abort。

---

## 10. `test-support`（feature 门禁的公开面）

### 10.1 `TestApp` / `TestWindow`

```rust
/// 无 GPU 的测试引擎（I7 / §6.6）：默认 `renderer = None`。
pub struct TestApp {
    runtime: tokio::runtime::Runtime,   // current_thread；App::new 需要 Handle
    app: App,
    window: WindowId,
}

impl TestApp {
    pub fn new() -> Self;                                  // 800×600、Scale::ONE
    pub fn with_options(options: WindowOptions) -> Self;    // 尺寸/标题/scale
    pub fn window_id(&self) -> WindowId;
    pub fn app(&self) -> &App;
    pub fn app_mut(&mut self) -> &mut App;                  // 建树 / 写信号 / 注册字体栈
    pub fn drain(&mut self) -> Result<&FrameRecord, EngineError>;   // **唯一**帧推进原语（一帧）
    pub fn frame_record(&self) -> &FrameRecord;             // 最近一次 drain 的结果
    pub fn rendered(&self) -> Option<&RenderedFrame>;       // None = 最近一帧未提交
    pub fn probe(&self) -> Probe<'_>;
    pub fn inject(&mut self, event: InputEvent) -> InputOutcome;
    pub fn invalidate_all_lanes(&mut self);                 // oracle 的「全量」路径（清 REUSABLE + clear_frame）
    pub fn oracle_diff(&mut self) -> Option<String>;         // 增量 vs 全量，None = 无差异
    pub fn assert_oracle(&mut self);                         // 差异即 panic（CI 必跑，§3.8）
    pub fn snapshot(&self, case: SnapshotCase) -> Result<SnapshotImage, SnapshotError>;
}

/// 多窗口驱动：借用 `App` + 一个 `WindowId`（`TestApp` 单窗口场景的等价物）。
pub struct TestWindow<'a> {
    app: &'a mut App,
    id: WindowId
}
impl TestWindow<'_> {
    pub fn drain(&mut self) -> Result<FrameOutcome, EngineError>;
    pub fn frame_record(&self) -> &FrameRecord;
    pub fn probe(&self) -> Probe<'_>;
    pub fn inject(&mut self, event: InputEvent) -> InputOutcome;
}
impl TestApp { pub fn window(&mut self) -> TestWindow<'_>; }
```

`TestApp::drain` 的**逐字语义**：

```text
drain():
  1. let outcome = self.app.frame(self.window)          // 一帧（或空闲）
  2. Err(e) → Err(e)（此时已 abort，FrameIndex 不变）
  3. Ok(_) → Ok(self.app.window(self.window).unwrap().record())
```

- **不做**「等到没事干」的循环（设计 §6.2）：一次 `drain()` = 最多一帧；
- 空闲帧（`FrameOutcome::Idle`）也返回一条 `FrameRecord { rendered: false, frame: None, damage: [] }`，
  `frame_index` 保持**提交游标**（与上一帧相同），因此"`FrameIndex` 不增"与"零 damage"是同一份记录上的两个断言；
- 错误路径不提交、不 `settle`（damage/dirty 保留，下一次 `drain` 重试）。

### 10.2 `Probe` / `Selector`

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Selector {
    /// 稳定调试 id（`NodeBuilder::id`）。
    Id(&'static str),
    /// 文本节点的**精确**源文本（`TextContent.text`）。
    Text(&'static str),
    /// 从窗口根向下的 id 路径（每段匹配一层的 `debug_id`）。
    Path(&'static [&'static str]),
}

pub struct Probe<'a> {
    tree: &'a NodeTree,
    scene: &'a Scene
}

impl<'a> Probe<'a> {
    pub fn new(tree: &'a NodeTree, scene: &'a Scene) -> Self;
    pub fn by_selector(&self, selector: Selector) -> Option<NodeId>;
    pub fn exists(&self, selector: Selector) -> bool;
    pub fn count(&self, selector: Selector) -> usize;
    pub fn bounds(&self, node: NodeId) -> Option<Rect>;        // 逻辑像素布局盒（I11）
    pub fn hit(&self, point: Point) -> Option<HitResult>;      // 转调 `NodeTree::hit_test`
    pub fn lane_of(&self, node: NodeId) -> Option<LaneId>;
}
```

| 选择器           | 语义                                                       | 歧义处理                                         |
|------------------|------------------------------------------------------------|--------------------------------------------------|
| `Id(s)`          | `tree.debug_id(node) == Some(s)`                           | 多个命中 → `tree.order()` 的**第一个**（确定性） |
| `Text(s)`        | `NodeKind::Text(content) && content.text == s`             | 同上                                             |
| `Path(segments)` | 逐段下钻：根的直接子 → … → 目标；每段匹配该层的 `debug_id` | 同层重复 id → 第一个                             |

`Role(A11yRole)` 推迟 P14（F-63）：`A11yRole` 尚不存在，提前建变体会引入无载荷类型。

### 10.3 `FrameRecord` / `RenderedFrame`（I12 的只读面）

```rust
/// 一帧的**调度记录**（`drain()` 的返回值；空闲帧也有）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameRecord {
    pub frame_index: FrameIndex,           // 提交游标（0 = 从未提交；空闲帧 = 上一帧的值）
    pub rendered: bool,                    // 本帧是否提交了像素
    pub frame: Option<RenderedFrame>,      // Some = 本帧提交的绘制快照
    pub stats: FrameStats,
    pub damage: Vec<PhysicalRect>,         // Full 时 = [整帧]（与 damage_full 一致）
    pub damage_full: bool,
    pub dirty_nodes: Vec<NodeId>,          // 本帧被重建 lane 的节点（tree.order 序）
    pub paint_log: PaintLog,
}

/// 已提交帧的**绘制快照**（设计 §6.3 的 `RenderedFrame`）。
#[derive(Clone, Debug, PartialEq)]
pub struct RenderedFrame {
    pub frame_index: FrameIndex,
    pub debug_bounds: ahash::HashMap<NodeId, Rect>,
    pub quads: Vec<QuadRecord>,
    pub primitives: Vec<PrimitiveRecord>,
    pub reused_lanes: Vec<LaneId>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QuadRecord {
    pub node: NodeId,
    pub bounds: Rect,                       // 逻辑像素（`Primitive` 的值，不是编码后的物理值）
    pub corner_radius: [f32; 4],
    pub fill: LinearPremulRgba,             // = resolver.resolve(paint 后的 Color)
    pub border: Option<(f32, LinearPremulRgba)>,
    pub gradient: Option<GradientKind>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrimitiveRecord {
    pub node: NodeId,
    pub kind: PrimitiveKind,
    pub bounds: Rect,                       // GlyphRun = 字形并集；ClipPop = Rect::default()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimitiveKind { Quad, GlyphRun, Image, Path, Shadow, ClipPush, ClipPop, Geometry, Canvas }

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaintLog {
    pub lanes: Vec<LanePaintEntry>
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LanePaintEntry {
    pub lane: LaneId,
    pub node: NodeId,
    pub outcome: LaneOutcome,
    pub primitives: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneOutcome { Allocated, Rebuilt, Cleared, Reused }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStats {
    pub styles: StylePassReport,
    pub text: Option<TextPassReport>,
    pub text_layout: Option<TextLayoutReport>,
    pub layout: Option<LayoutReport>,
    pub painted_lanes: usize,
    pub reused_lanes: usize,
    pub primitives: usize,
    pub instances: usize,
}
```

- `RenderedFrame` 只在**提交帧**构建（`frame: Some`）；空闲/中止帧 `frame: None`，且 `frame_index` 保持上一帧值
  —— 这样"`FrameIndex` 不增"（`frame_index` 相等）与"没有提交"（`frame.is_none()`）是两个独立可断言的字段。
- `quads` 与 `primitives` 都**按绘制序**（`Scene::order()` 展平），且 `primitives` 包含 `ClipPush`/`ClipPop`
  —— 这正是 oracle 的比较键（§10.5）。
- `QuadRecord.fill`/`border` 用 `ColorResolver`，直接对应设计 §6.3 的
  `assert_eq!(frame.quads[0].background, theme::surface().resolve())`。
- `FrameRecord.frame_index` 与 `RenderedFrame.frame_index` 在 `frame: Some` 时必然相等（后者是设计 §6.3 的字段，
  值取自同一个提交游标）；`TestApp::rendered()` 只是 `frame_record().frame.as_ref()` 的便捷方法。

### 10.4 `inject` / `InputEvent`（最小事件集）

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerButton { Primary, Secondary, Middle }

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputEvent {
    PointerMove { position: Point },
    PointerDown { position: Point, button: PointerButton },
    PointerUp { position: Point, button: PointerButton },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputOutcome {
    pub hit: Option<HitResult>
}
```

`inject` 的**唯一**职责：把点转成 `NodeTree::hit_test` 的结果并原样返回（P06 无焦点/Action/点击态）。
键盘/IME/滚轮/拖拽在 P10 additive 加变体 —— 现在加空变体只会制造无消费者分支。

### 10.5 full-refresh oracle

```text
oracle_diff():
  1. drain()  → 增量帧 A（`rendered` 必须为 true，否则返回 Some("frame was idle")）
  2. invalidate_all_lanes(): 每个节点 dirty.remove(REUSABLE) + scene.clear_frame()
  3. drain()  → 全量帧 B
  4. if A.primitives != B.primitives → Some(diff)      // (kind, bounds, node) 序列
     if A.quads      != B.quads      → Some(diff)      // 含解析后的颜色
     else None
```

- 比较键 = `RenderedFrame::primitives`（`node`/`kind`/`bounds` 按序）+ `RenderedFrame::quads`； **不**比较 `damage`
  （全量路径必然更大，设计 §3.8 只要求 `(Primitive, order, bounds)` 序列一致）；
- `assert_oracle()` 在差异时 `panic!("stale drawing: …")`（CI 必跑，PoC P2 的判定标准）；
- 全量路径的 `Scene::clear_frame()` 让 lane 内容从零重建，覆盖「祖先 clip 前缀」「order 变化」「shadow 提前」等
  所有复用假设；lane **id** 不变（`clear_frame` 只清内容）⇒ 两条路径的 `node` 键可比。

### 10.6 `SnapshotCase`（接口落地，GPU 实现留 P09）

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SnapshotCase {
    pub size: Size,
    pub scale: Scale
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotImage {
    pub width: u32,
    pub height: u32,
    pub rgba8: Vec<u8>
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotError { NoRenderer }

impl TestApp {
    /// P06：恒 `Err(SnapshotError::NoRenderer)`（无 GPU）。P09 接入 WARP/lavapipe/Metal。
    pub fn snapshot(&self, case: SnapshotCase) -> Result<SnapshotImage, SnapshotError>;
}
```

`SnapshotCase` 在本阶段是**输入契约**（`size`/`scale` 由 `snapshot` 的 debug 校验读取：`scale` 非法 →
`debug_assert!`），`SnapshotImage` 的字段是公开 API（不进 P06 的任何构建路径）。用一个 `Err` 用例固化
"无 GPU 时是可预期的 `NoRenderer`，而不是 panic 或静默空图"。

### 10.7 `cfg(feature = "test-support")` 的公开面清单

```text
lib.rs:        #[cfg(feature = "test-support")] pub mod test_support;
test_support.rs: pub mod app; pub mod input; pub mod probe; pub mod record; pub mod snapshot;
                 （模块根只有这 5 行；无 item / 无 pub use）
Cargo.toml:    [features] test-support = []
               [[test]] name = "frame_record"; path = "tests/frame_record.rs";
                        required-features = ["test-support"]
```

**不**在 feature 里改任何生产类型的大小/布局：`#[cfg]` 只出现在 `Window` 的 `stats`/`record` 字段与
`Window`/`PaintReport`/`DamageAccumulator` 的记录访问器上（`cfg` 字段是唯一例外，见 §9.5）。

---

## 11. 公开面 / 死代码 / `Node` 尺寸

### 11.1 `Node` 字段增量

| 字段             | 类型                     | 大小 | 写者                                       | 非 test 读者                |
|------------------|--------------------------|------|--------------------------------------------|-----------------------------|
| `paint`          | `Option<Box<PaintSpec>>` | 8    | `insert_child`（builder）                  | `PaintPass`                 |
| `content_offset` | `Point`                  | 8    | `set_local_layout`（taffy border+padding） | `PaintPass`（文本绝对坐标） |

`1112 + 8 + 8 = 1128`；断言 `size_of::<Node>() <= 1152` 保留，并在实现时把实测值写进
`node_and_handle_sizes` 的 `println!`（S-59 的同一做法）。**余 24 B**：下一个字段必须走
`style: Box<Style>` 或侧表（V-48）。

### 11.2 `NodeTree` 字段增量（0 B 落到 `Node`）

| 字段            | 类型                                      | 用途                                                                               |
|-----------------|-------------------------------------------|------------------------------------------------------------------------------------|
| `debug_ids`     | `Vec<Option<SmolStr>>`（按 arena 槽位）   | 稳定调试 id；`insert_child` resize/置 `None`，`remove` 置 `None`（槽位复用不继承） |
| `removed_lanes` | `Vec<LaneId>`                             | 卸载时记录待释放 lane（`Window::paint_frame` 消费）                                |
| `clip_keys`     | `Vec<Option<(u8, Option<PhysicalRect>)>>` | 每节点最近一次重建时的 clip 上下文（§5.6 的 B1 守卫）；槽位复用/卸载置 `None`      |

### 11.3 新增/修改的 `pub(crate)` 方法与消费者

| 方法                                             | 签名                                                                                                            | 消费者                                                                                                       |
|--------------------------------------------------|-----------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------|
| `NodeTree::settle_frame`                         | `fn(&mut self)`                                                                                                 | `Window::commit_frame`                                                                                       |
| `NodeTree::set_reusable`                         | `fn(&mut self, NodeId)`                                                                                         | `PaintPass`                                                                                                  |
| `NodeTree::lane_of` / `set_lane`                 | `fn(&self, NodeId) -> Option<LaneId>` / `fn(&mut self, NodeId, LaneId)`                                         | `PaintPass`                                                                                                  |
| `NodeTree::drain_removed_lanes`                  | `fn(&mut self) -> Vec<LaneId>`                                                                                  | `Window::paint_frame`                                                                                        |
| `NodeTree::clip_key` / `set_clip_key`            | `fn(&self, NodeId) -> Option<(u8, Option<PhysicalRect>)>` / `fn(&mut self, NodeId, (u8, Option<PhysicalRect>))` | `PaintPass`（§5.6 的 B1 守卫）                                                                               |
| `NodeTree::content_offset`                       | `fn(&self, NodeId) -> Point`                                                                                    | `PaintPass`                                                                                                  |
| `Node::paint_spec`                               | `fn(&self) -> Option<&PaintSpec>`                                                                               | `PaintPass`                                                                                                  |
| `Node::has_bindings`                             | `fn(&self) -> bool`                                                                                             | `Window::frame` 的 **`debug_assert`**（合法消费形态：`debug_assert!` 的读取计入 dead_code 使用，probe §2.2） |
| `NodeTree::debug_id`                             | `pub fn(&self, NodeId) -> Option<&str>`                                                                         | `Probe`（公开只读）                                                                                          |
| `Scene::clear_lane`                              | `pub fn(&mut self, LaneId) -> bool`                                                                             | `PaintPass`                                                                                                  |
| `Window` 的帧侧方法                              | §9.2 / §9.5                                                                                                     | `Window::frame` / `App::frame`                                                                               |
| `Window::{record, record_idle, set_frame_stats}` | `cfg(test-support)`                                                                                             | `TestApp` / `TestWindow`                                                                                     |
| `DamageAccumulator::rects`                       | `cfg(test-support)`                                                                                             | `FrameRecord` 捕获                                                                                           |

### 11.4 `Node` 的手写 `Debug` 必须同步

`paint: Option<Box<PaintSpec>>`（正常打印，`PaintSpec: Debug`）、`content_offset`（正常打印）。
漏加会让 `probe`/失败信息缺失绘制事实（P04 N4 的同一纪律）。

### 11.5 明确的「不加」

| 不加                                                   | 理由                                                                                                                              |
|--------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------|
| `LayoutBox.overflow`（P04 V-33）                       | `remove` 并入子树 bounds + `absolutize` 的 `prev ∪ new` 已覆盖溢出/消失（§6.6 表）；加字段会破 `LayoutBox == 32 B` 与 `Node` 预算 |
| `Node.geometry_transform`                              | 无 `GeometryStore`、无写者（裁决 ④）                                                                                              |
| `DamageOutput` / `ScrollHint` / `push_scroll`          | 无消费者（裁决 ②，F-61）                                                                                                          |
| `Selector::Role`                                       | `A11yRole` 不存在（F-63）                                                                                                         |
| `FrameScheduler` 的 10 态 / in-flight ring             | 无观察者 / 无 GPU（F-66）                                                                                                         |
| `Primitive` 的 per-primitive transform / `Canvas` 嵌套 | P02 已裁决（F-32/F-33/R10）                                                                                                       |

---

## 12. 测试清单（计划 **60** 项；单元 56 内联 `#[cfg(test)] mod tests`，集成 4）

> **实测（2026-11-12 落地 + 审查二轮）**：新增单元测试 **54** 个（默认构建 418，较 P05 的 364
> 增加 54；`test-support` 构建 427），集成 `tests/frame_record.rs` **5** 个；四个构建组合全绿
> （418 / 427+5 / 409 / 418+5）。新增用例中 `zero_size_clip_still_pushes_and_empties_damage`、
> `transparent_gradient_emits_nothing`、`resize_forces_full_damage`、
> `text_glyphs_use_border_and_padding_offset`、`image_maps_to_atlas_quad` 属审查二轮补测。
> 其中 `--release` 的差额来自 `#[cfg(debug_assertions)]` 与依赖 debug 断言的用例（与 P05 同口径）。
> A–I 的分组与命名与实现一致；C 组合并了 `frame_ops`/幂等两项、E 组用 `clip_key` 用例替换了计划的
> `set_style_clip_change`（审查 B1）。

> 命名 = 函数名；「实测」列在实现时回填（S-59 的做法：计划 40–60，落地可略多）。

### A. `paint/instance.rs`（12）

| #  | 测试                              | 断言                                                                                         |
|----|-----------------------------------|----------------------------------------------------------------------------------------------|
| 1  | `layout_is_128_by_16`             | 运行时 `size_of == 128`、`align_of == 16`、8 个 `offset_of`                                  |
| 2  | `params1_is_integer_encoded`      | `kind/texture/clip_index/flags` 与 `as u32 as f32` 往返一致；小数不出现                      |
| 3  | `quad_solid_mapping`              | `bounds/corner_radius/color0/color1/params0/uv/clip_rect` 逐字段                             |
| 4  | `quad_border_uses_color1`         | `params0.x > 0` 且 `color1 == resolve(border.color)`；`params0.w == 1.0`                     |
| 5  | `quad_gradient_kinds`             | 三种 kind → `params0.z ∈ {1,2,3}`、`color1 == resolve(to)`、无边框                           |
| 6  | `glyph_run_is_one_to_n`           | 3 字形 → 3 实例（`texture_index == 0`、`uv` 各异、`bounds` 为整数）                          |
| 7  | `atlas_bounds_are_integer`        | `Image`/`Glyph` 的 `bounds` 四分量 `fract() == 0.0`                                          |
| 8  | `shadow_padding_is_blur_plus_one` | `params0.y == blur*s`、`params0.w == blur*s + 1.0`、`bounds == f32r(rect.translate(offset))` |
| 9  | `skip_transparent_and_empty`      | 透明 Quad / 空 bounds / 非有限 bounds → 0 实例                                               |
| 10 | `clip_chain_dedup`                | 两条 lane 相同 clip 序列共用 id；嵌套 2 层 `parent` 正确                                     |
| 11 | `frame_ops_preserve_order`        | `Quad→Geometry→Quad` → `[Instances, Geometry, Instances]`，`start/len` 连续                  |
| 12 | `opaque_flag_and_idempotence`     | 不透明纯色带 `FLAG_OPAQUE`；渐变/半透明/Shadow 不带；同 Scene 两次编码字节一致               |

### B. `render/damage.rs`（12）

| #  | 测试                                         | 断言                                                         |
|----|----------------------------------------------|--------------------------------------------------------------|
| 1  | `push_uses_covering_not_round`               | `(10.5,20.25,0.25,0.25)@2` → `(21,40,22,41)`（round 会零高） |
| 2  | `push_expands_by_physical_padding`           | `pad=2.0` → 四边各 2                                         |
| 3  | `push_intersects_clip`                       | clip 覆盖一半 → 结果落在 clip 内                             |
| 4  | `push_rejects_non_finite_and_negative`       | NaN/∞/负尺寸 → 无 rect、不 panic                             |
| 5  | `merge_overlapping` / 6 `merge_adjacent`     | 相交/共边 → 1 个                                             |
| 7  | `reject_l_shaped_merge`                      | 角相邻不合并（`area(u) > sum`）                              |
| 8  | `merge_is_order_independent`                 | 两种插入序 → 同输出（排序保证）                              |
| 9  | `too_many_rects_becomes_full`                | 17 个互不相交 → `full`                                       |
| 10 | `area_ratio_becomes_full`                    | > 50% 帧面积 → `full`                                        |
| 11 | `empty_stays_partial_empty`                  | 无输入 → `full=false, rects=0`                               |
| 12 | `reset_reuses_capacity_and_is_deterministic` | `reset` 后 `capacity` 不回零；同输入两次输出逐项相等         |

### C. `paint/pass.rs`（8）

| # | 测试                                         | 断言                                                                        |
|---|----------------------------------------------|-----------------------------------------------------------------------------|
| 1 | `box_background_and_border_one_quad`         | 无渐变时 1 个 `Quad`（fill+border）；顺序 = 背景                            |
| 2 | `gradient_plus_border_two_quads_in_order`    | 2 个 `Quad`：Gradient → Solid{TRANSPARENT, border}                          |
| 3 | `shadow_precedes_fill`                       | `Shadow` 在 `Quad` 之前（同 lane）                                          |
| 4 | `text_glyphs_are_absolute_and_colored`       | 合成 `ShapedLine`（无字体）+ `content_offset` → 绝对坐标、`foreground` 覆盖 |
| 5 | `hidden_node_paints_nothing_and_clears_lane` | `visible=false` → lane 清空、damage 仍非空                                  |
| 6 | `clip_brackets_lane_and_subtree`             | 祖先 `Clip::Bounds` → 子 lane 以 Push 开头、Pop 结尾、`clip_depth == 0`     |
| 7 | `lane_is_reused_when_reusable`               | 第二帧不改任何东西 → `LaneOutcome::Reused`、lane 内容逐项不变               |
| 8 | `canvas_and_geometry_emit_their_primitives`  | `Canvas` 整块、`Geometry` 单个 `GeometryDraw`、空 Canvas 仍 push            |

### D. `paint/scene.rs`（3，追加到 P03 的 12 个用例后）

| # | 测试                                              | 断言                                                                                              |
|---|---------------------------------------------------|---------------------------------------------------------------------------------------------------|
| 1 | `clear_lane_keeps_capacity_and_resets_clip_depth` | 清空后 `capacity` 不降、`clip_depth == 0`、可再写                                                 |
| 2 | `clear_lane_stale_is_false`                       | 陈旧句柄 → `false`，不 panic                                                                      |
| 3 | `order_keeps_empty_lanes`                         | 手工构造的空 lane 仍在 `order`（paint pass 本身不再产生空 lane，见 S-67）；`primitive_count` 不计 |

### E. `node/tree.rs`（6，追加）

| # | 测试                                              | 断言                                                                                                                                                     |
|---|---------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------|
| 1 | `settle_frame_resets_damage_bits_and_prev_layout` | 提交后 `damage == ∅`、`prev_layout == layout`、`REUSABLE` 置位、`LAYOUT\|PAINT` 清除、`A11Y` 保留                                                        |
| 2 | `remove_unions_subtree_bounds_into_parent_damage` | 出流子节点在父盒之外卸载 → 父 damage ⊇ 子盒                                                                                                              |
| 3 | `remove_reports_released_lanes`                   | `drain_removed_lanes` 返回子树全部 lane；二次取为空                                                                                                      |
| 4 | `debug_id_roundtrip_and_slot_reuse`               | `NodeBuilder::id` → `debug_id`；卸载后槽位复用的新节点 `debug_id == None`                                                                                |
| 5 | `clip_key_mismatch_after_resize_or_reparent`      | ①resize 改变被 clip 容器（`Clip::Bounds`）的尺寸而子节点绝对盒不变 → 子 lane 重建且前缀矩形更新；②把已有子树重挂到 clip 节点下（绝对盒不变）→ 同样的重建 |
| 6 | `node_size_within_budget_after_p06`               | `size_of::<Node>() <= 1152`（实测回填 1128）、`size_of::<Style>() == 560`                                                                                |

### F. `window.rs`（6，追加）

| # | 测试                                                                                      | 断言                                                                                            |
|---|-------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------|
| 1 | `frame_commits_once_and_increments_index`                                                 | 首帧 `Committed(FrameIndex(1))`；`window.frame_index().get() == 1`                              |
| 2 | `idle_frame_does_not_commit_or_increment`                                                 | 连续两次 `frame()` → 第二次 `Idle`、`frame_index` 不变                                          |
| 3 | `paint_only_frame_reuses_lanes_and_skips_layout`                                          | 改背景色 → `layout_if_dirty() == None`、lane 重建、其余 lane `Reused`                           |
| 4 | `resize_forces_full_damage`                                                               | `set_size` 后 `frame()` → damage `full=true`（不依赖帧数）                                      |
| 5 | `frame_aborts_on_layout_error`                                                            | 注入 NaN measure → `Err(InvalidMeasure)`、`frame_index` 不变、damage/dirty 保留（下一次可重试） |
| 6 | `frame_rejects_bound_nodes_without_styles`（`#[cfg(debug_assertions)]` + `should_panic`） | 含 `bind_style` 的窗口调 `Window::frame()` → debug 断言                                         |

### G. `app.rs`（3，追加）

| # | 测试                                | 断言                                                                    |
|---|-------------------------------------|-------------------------------------------------------------------------|
| 1 | `app_frame_runs_styles_then_paints` | 写信号 + `App::frame` → `Scene` 的 Quad 颜色 = 新 patch（无需 feature） |
| 2 | `app_frame_unknown_window_is_none`  | 未知 `WindowId` → `None`，不消费 wake                                   |
| 3 | `app_frame_is_idle_without_work`    | 无写、无脏 → `Idle`；`frame_index` 不增                                 |

### H. `test_support/*`（6，仅 `--features test-support`）

| # | 测试                                                                  | 断言                                                                                           |
|---|-----------------------------------------------------------------------|------------------------------------------------------------------------------------------------|
| 1 | `probe_by_id_and_bounds`                                              | `Selector::Id` → `NodeId`；`bounds` == 布局盒                                                  |
| 2 | `probe_by_text_and_path`                                              | `Text("…")` / `Path(&["panel","price"])` 均命中                                                |
| 3 | `probe_missing_and_hit`                                               | 不存在 → `None`；`hit` 返回 `HitResult`                                                        |
| 4 | `drain_records_quad_fields`                                           | `RenderedFrame.quads[0].fill == resolver.resolve(bg)`、`bounds` 逻辑值、`PrimitiveRecord` 顺序 |
| 5 | `second_drain_reuses_lanes`                                           | `reused_lanes` 含全部 lane；`dirty_nodes` 为空                                                 |
| 6 | `oracle_matches_full_refresh` + `snapshot_without_gpu_is_no_renderer` | `oracle_diff() == None`；`snapshot` → `Err(NoRenderer)`                                        |

### I. `tests/frame_record.rs`（4，仅 feature；集成 = 只用公开 API）

| # | 测试                                     | 断言                                                                                                        |
|---|------------------------------------------|-------------------------------------------------------------------------------------------------------------|
| 1 | `text_frame_records_glyph_run_instances` | 1 个文本节点 → `RenderedFrame.primitives` 含 `GlyphRun`；实例数 == 字形数                                   |
| 2 | `canvas_frame_flattens_primitives`       | 应用 `Canvas`（quad+clip+geometry）→ 帧记录的 `PrimitiveRecord` 顺序与展平一致                              |
| 3 | `geometry_frame_records_geometry_draw`   | `NodeBuilder::geometry(GeometryId::for_test(0,1))` → `PrimitiveKind::Geometry`                              |
| 4 | `damage_is_local_and_idle_is_zero_frame` | 改一个 label 颜色 → `damage` 全部 `< frame_area/50`；随后 `drain` → `rendered == false`、`frame_index` 不变 |

### 12.1 覆盖映射（brief 的 8 类）

| brief 要求                               | 覆盖                                                                         |
|------------------------------------------|------------------------------------------------------------------------------|
| L2 帧记录（quad 颜色/bounds/clip/order） | A3/A4/A5、H4、I2                                                             |
| lane 复用（`REUSABLE`）                  | C7、E5、F3、H5                                                               |
| damage 局部性                            | B1–B12、F4、I4                                                               |
| 空闲零帧                                 | F2、G3、I4                                                                   |
| FrameIndex 不增                          | F2、F5、I4                                                                   |
| oracle 无差异                            | H6                                                                           |
| 文本 GlyphRun 实例                       | A6、C4、I1                                                                   |
| Canvas / Geometry                        | C8、I2、I3                                                                   |
| Probe 命中/存在性                        | H1–H3                                                                        |
| release 一致性                           | §13 的 4 条 `--release` 命令（同样的用例，含 `debug_assertions` 关闭的路径） |

---

## 13. 验收门禁

```powershell
# 1) 单测（debug / release / 两侧 feature）
cargo test -p ui_core
cargo test -p ui_core --features test-support
cargo test -p ui_core --release
cargo test -p ui_core --release --features test-support
#   期望：4 组全绿；集成 target `frame_record` 只在 feature 构建里出现（required-features）

# 2) clippy（含 feature 与 all-targets）
cargo +nightly clippy -p ui_core --all-targets --features test-support -- -D warnings

# 3) 格式
cargo +nightly fmt -p ui_core -- --check

# 4) 文档零警告
cargo doc -p ui_core --no-deps --features test-support

# 5) 三平台依赖解析（不链接）
cargo metadata --no-deps --format-version 1 --filter-platform x86_64-pc-windows-msvc   | Out-Null
cargo metadata --no-deps --format-version 1 --filter-platform x86_64-apple-darwin      | Out-Null
cargo metadata --no-deps --format-version 1 --filter-platform x86_64-unknown-linux-gnu | Out-Null
#   期望三次 rc = 0；Cargo.lock 无变化

# 6) 模块根形态（只允许 mod / pub mod 行）
Get-Content crates\ui_core\src\paint.rs          # 7 行 pub mod + 2 行 pub(crate) mod
Get-Content crates\ui_core\src\render.rs         # pub(crate) mod atlas/damage + pub(crate) use
Get-Content crates\ui_core\src\test_support.rs   # 5 行 pub mod
Get-Content crates\ui_core\src\lib.rs            # 纯模块声明 + cfg(test-support) 一行

# 7) 零 unsafe / 零 allow / 零 expect / 零 serde_json / 零 #[path] / 零 mod.rs / 零 pub use
Get-ChildItem crates\ui_core\src -Recurse -Filter *.rs |
  Select-String -Pattern '\bunsafe\b|#\[allow|#\[expect|serde_json|#\[path|^pub use |\bpub\(crate\) use '
#   期望无输出（render.rs 的 `pub(crate) use damage::…` 属 M-07 白名单，单独放行）

# 8) dead_code 审计：每条 `pub(crate)` 必须能在 §4.4 表里找到非 test 消费者
Get-ChildItem crates\ui_core\src -Recurse -Filter *.rs |
  Select-String -Pattern 'pub\(crate\) fn '
#   逐条对照 §4.4；表外的一律删除或提升为 pub

# 9) 文件边界（P06 允许的集合）
Get-ChildItem crates\ui_core\src -Recurse -Filter *.rs | Select-Object -ExpandProperty Name
#   只允许：既有 + instance.rs / pass.rs / damage.rs / test_support.rs
#            + test_support/{app,probe,record,input,snapshot}.rs
#   不得出现：event.rs / a11y.rs / tests.rs / mod.rs / platform/ / render/backend*
Test-Path crates\ui_core\src\paint\damage.rs   # 期望 False（damage 在 render/）

# 10) Node 尺寸与关键布局
cargo test -p ui_core node_size_within_budget_after_p06 -- --nocapture
#   期望 size_of::<Node>() <= 1152（实测 1128）

# 11) 空闲零帧 / 帧序号（功能门禁，等价于 F2/I4）
cargo test -p ui_core --features test-support idle

# 12) 无新依赖
cargo tree -p ui_core -e normal --depth 1
#   期望与 P05 结束时一致（不新增 crate；只增 feature）
```

---

## 14. 风险

| #   | 风险                                                                                                                 | 级 | 缓解                                                                                                         |
|-----|----------------------------------------------------------------------------------------------------------------------|----|--------------------------------------------------------------------------------------------------------------|
| R1  | `Node` 只余 24 B：P07 若再加字段就破 1152                                                                            | 中 | 侧表优先（`debug_ids`/`removed_lanes` 已示范）；真的要内联时改 `style: Box<Style>`（公开面不变，V-48）       |
| R2  | 文本绝对坐标依赖 P05 的「局部坐标」契约（含行偏移）；若 P05 少加了 run 偏移，L2 断言会发现坐标不对但**像素**才看得出 | 中 | C4 用合成 `ShapedLine` 固定输入；I1 用真实字体跑一遍；任何坐标变更进 P05 回归                                |
| R3  | 祖先 clip 变化失效子树的规则若漏（只改了 `set_style`，忘了 `LayoutOverride`/P13 的新写入口）                         | 中 | 规则写在 §5.6；`oracle` 用例（H6）会在"祖先改 clip"场景暴露；P13 的新写入口必须复用同一 `mark_subtree_paint` |
| R4  | `cfg(feature)` 的 `Window` 字段/记录捕获写错（只在 `--features` 构建里露）                                           | 中 | 门禁 1 的四组命令都跑；`FrameRecord` 的字段全 `pub`（死代码从结构上不可能）                                  |
| R5  | 编码期每图元 `resolve`（5000 图元 ≈ 0.3 ms）与每帧 O(n) 遍历                                                         | 低 | P18 基准；lane 内颜色缓存是 additive（不改 128 B）；V-49                                                     |
| R6  | `Canvas` 重建时深拷贝（大画布）                                                                                      | 低 | V-50：`Rc<Canvas>` 是 additive 改动                                                                          |
| R7  | `damage` 的节点级 push 偏保守（隐藏节点、无图元节点也 push）                                                         | 低 | 过绘安全；局部性断言用「面积 < 帧/50」，实测会把阈值写进 I4                                                  |
| R8  | 集成测试依赖宿主字体（CJK/emoji 可能 tofu）                                                                          | 低 | V-46 的既有规则：断言 `tofu == 0` **或** `diagnostics` 非空                                                  |
| R9  | `GeometryId::for_test` 在 feature 下是 `pub`，P09 的 store 落地后可能被误用                                          | 低 | F-65 写明「P09 必须删除该构造器，改为 store 铸造」；用例只在 test-support                                    |
| R10 | `REUSABLE` 复位点移到提交（F-59）与设计 §3.4 的文字不一致，后续 Phase 可能按旧文字实现                               | 中 | DEVIATIONS F-59 + 本文件 §9.4 的双向链接；P07/P13 的帧驱动必须复用 `settle_frame`                            |

---

## 15. 需裁决项（5 项，含建议）

| # | 裁决项                                                                                                      | 建议                                                                                                                           | 备选（被否决的理由）                                                                                                                                                      |
|---|-------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| ① | 静态绘制属性放哪：`paint::style::PaintSpec` + `Node.paint: Option<Box<PaintSpec>>`（+8 B），`Style` 不变    | **采纳**：Node 1128；不动 `Style{inner,clip}` 的公开字面量；`set_style` 语义不变                                               | ①`Style.paint` 内联：Style 696 → Node **1248 > 1152**；②`Style.paint: Option<Box<..>>`：Style 568 → Node 1120，但破坏全仓 `Style{..}` 字面量并让 `set_style` 承担绘制语义 |
| ② | damage 输出形状：`DamageSummary{full,rects}`（+`rects()` 仅 feature）；**不落** `DamageOutput`/`ScrollHint` | **采纳**：唯一消费者（P07 平台翻译）需要的是 `(full, &[PhysicalRect])`；scroll 的数据源在 P12                                  | P02 冻结形状：`DamageOutput::Partial{rects: Vec}` 会把容量送走，与「reset 保容量」自相矛盾；`ScrollHint` 字段永远 `None` 违反 AGENTS §2.1                                 |
| ③ | 稳定 debug id 存储：`NodeTree::debug_ids: Vec<Option<SmolStr>>` 侧表                                        | **采纳**：Node 保持 1128；与 `layout_cache`/`text_slots` 同一模式；卸载/槽位复用已设计                                         | 内联 `SmolStr`：Node **1144**（余 8 B），P07 再加字段必破                                                                                                                 |
| ④ | `Geometry` 的 transform：P06 恒 `Transform::IDENTITY`；`GEOMETRY` 位仍无写者                                | **采纳**：无 `GeometryStore`、无 per-`GeometryId` 变换存储，`set_geometry_transform` 的签名（`GeometryId` 而非节点）属 P08/P09 | 在 P06 造 `Node.geometry_transform`：+24 B 破预算（1144+24）且与 P08 的 per-id API 冲突                                                                                   |
| ⑤ | 新增 `src/paint/pass.rs`（超出 brief 的文件清单）                                                           | **采纳**：paint pass 是「style/patch/text/canvas → Primitive」的单一职责，塞进 `window.rs` 会让后者混入绘制语义（AGENTS §1.3） | 塞进 `window.rs`（~700 行 + 两套职责）或 `scene.rs`（数据层污染）                                                                                                         |

---

## 16. 本阶段追加到 `design/DEVIATIONS.md` 的条目（§十一）

| 编号 | 标题                                                                                | 一句话                                                                                     |
|------|-------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------|
| F-58 | 静态绘制属性 = `Node.paint: Option<Box<PaintSpec>>`                                 | 不进 `Style`（内联会让 `Style` 696 B → `Node` 1248 > 1152）；`Node` 1128                   |
| F-59 | `REUSABLE`/damage/`prev_layout` 的复位点在**提交**（`settle_frame`）                | 帧初归零会吞掉 drain 阶段累积的 damage                                                     |
| F-60 | `absolutize` 的 `changed` 分支清 `REUSABLE`                                         | 布局移动后 lane 内的 bounds 已过期（P04 §7.2 只管 damage）                                 |
| F-61 | damage 输出 = `DamageSummary{full,rects}`；`DamageOutput`/`ScrollHint` 延后 P07/P12 | 无消费者 + 容量语义冲突                                                                    |
| F-62 | `Geometry` 的 transform 在 P06 恒 `IDENTITY`                                        | 无 store/无写者；uniform 通道属 P08/P09                                                    |
| F-63 | `Selector::Role` 延后 P14                                                           | `A11yRole` 不存在                                                                          |
| F-64 | 文本绝对坐标 = `bounds.origin + content_offset + 局部`                              | 细化 P05 S-54（padding 否则丢失）                                                          |
| F-65 | `GeometryId::for_test` 在 `feature = "test-support"` 下是 `pub`                     | 集成测试在 P09 前无合法构造路径                                                            |
| F-66 | `FrameScheduler` 只保留 `Idle/Pending/Draining`                                     | 设计 10 态在一次同步调用内部，无可观察边界；`WaitGpu` 属 P09                               |
| S-60 | paint pass 归属 `paint/pass.rs`（crate 私有）                                       | 裁决 ⑤                                                                                     |
| S-61 | 一节点一 lane；lane 自包含祖先 clip 前缀；空 lane 粘性；释放只随卸载                | §5                                                                                         |
| S-62 | `Scene` 写面**不收紧**（`clear_lane` 同为 `pub` + engine-internal doc）             | 一半收紧比 S-33 现状更难审计（§4.3）                                                       |
| S-63 | `test-support` = 5 个子模块；`FrameRecord`（调度）与 `RenderedFrame`（绘制）分型    | `pub use` 被禁，路径为 `test_support::app::TestApp` 等（S-16 闭包）                        |
| S-64 | 全帧 damage 的触发点 = `set_size`/`set_scale` 的 `full_damage_pending`              | 尺寸/DPI 变化必须 `Full`（§5.7 / P01 §7.2）                                                |
| S-65 | 编码器自检承担 `InstanceBuffer` 内部字段的非 test 读点                              | `debug_assert!` 的读取计入使用（probe §2.2）                                               |
| S-66 | `clip_key` 侧表（`depth` + `covering`）作为 lane 复用的**clip 上下文守卫**          | 审查 B1：只守 `Style.clip` 开关会复用旧矩形前缀（resize/重挂/P13 写入）                    |
| V-48 | `Node` 1128（余 24 B）；下一个字段走 `style: Box<Style>` 或侧表                     | 触发：任何再加内联字段的 Phase                                                             |
| V-49 | 每帧 O(n)（paint 全树 + 文本两 pass + 每图元 resolve）                              | 触发：P13 虚拟列表 / P18 基准；对策 = `DirtyFlags` 的 2 个保留位加 `TEXT`/`PAINT` 增量遍历 |
| V-50 | `Canvas` 在 lane 重建时深拷贝                                                       | 触发：> 1k 图元的画布；对策 `NodeKind::Canvas(Rc<Canvas>)`（additive）                     |
| V-51 | `opacity` 是逐图元 alpha（非 group opacity）；`Canvas` 内部不参与                   | 触发：需要整组淡出的场景（P15 再评估离屏合成）                                             |
| V-52 | `damage` 的节点级 push 对隐藏/无图元节点也生效（过绘）                              | 触发：I4 的 damage 比不达标；对策 = 只在 `rebuilt` 时 push（需先证明"图元减少"仍被覆盖）   |

---

## 附录 A · 实现者检查单（按执行顺序）

| #  | 检查项                                                                 | 通过判据                                                    |
|----|------------------------------------------------------------------------|-------------------------------------------------------------|
| 1  | `math.rs` 的 `PhysicalRect::area`                                      | 用 `i64`；damage 阈值与 §12-B10 共用                        |
| 2  | `PaintSpec`/`ShadowSpec` 逐字段与 §6.1 一致                            | `PaintSpec::default()` = 全透明/无边框/零圆角/无阴影/无渐变 |
| 3  | `Node` 的 `paint`/`content_offset` + 手写 `Debug`                      | `size_of::<Node>() <= 1152`（实测 1128）                    |
| 4  | `NodeTree` 的侧表 + `settle_frame` + lane 访问器                       | `debug_ids` 在 `remove`/槽位复用时清空                      |
| 5  | `absolutize` 清 `REUSABLE`；`set_style` 的 clip 变化标子树             | 用例 E2/E5 与 H6                                            |
| 6  | `render/damage.rs` 与 §8.2 逐字一致                                    | B1–B12 全绿                                                 |
| 7  | `paint/instance.rs` 的 128 B 与映射表                                  | A1–A12 全绿；`kind/flags` 只用既有常量                      |
| 8  | `paint/pass.rs` 的桶序（§5.2）与生成表（§6.2）                         | C1–C8 全绿；C1 断言的 `Lane::clip_depth()` 回到 0           |
| 9  | `Window` 的帧侧：`needs_frame → begin → paint → commit/abort`          | F1–F6 全绿；`FrameIndex` 只在 `commit` 增                   |
| 10 | `App::frame` 的 pass 顺序                                              | G1–G3；`layout_if_dirty` 只在有 `LAYOUT` 时跑               |
| 11 | `test_support` 5 个子模块 + `Cargo.toml` feature/`[[test]]`            | §10.7 的 4 行；`cargo test -p ui_core` 默认**不**编译它     |
| 12 | `tests/frame_record.rs` 只用公开 API                                   | 4 个用例；`--features test-support` 才有 target             |
| 13 | 零 `unsafe`/`allow`/`expect`/`serde_json`/`#[path]`/`mod.rs`/`pub use` | §13-7                                                       |
| 14 | 每条 `pub(crate)` 都在 §4.4 表里                                       | §13-8                                                       |
| 15 | DEVIATIONS §十一 追加（F-58…F-66 / S-60…S-66 / V-48…V-52）             | 不改写历史条目                                              |

## 附录 B · 与上游文档的交叉引用

| 本文              | 主设计                   | P02             | P03        | P04            | P05       | DEVIATIONS              |
|-------------------|--------------------------|-----------------|------------|----------------|-----------|-------------------------|
| §5 lane 模型      | §3.4、§5.3、§5.7         | §7、§11         | §13        | —              | —         | S-09/S-10/S-15/**F-59** |
| §6 Primitive 生成 | §5.1、附录 C             | §6、§8.5、§12.3 | §9.4、§10  | §8.1（border） | §5.2、§13 | F-64、S-14、F-58        |
| §7 instance       | §2.3、§5.2、§5.4、附录 B | §8（全章）      | —          | —              | —         | F-23、S-65              |
| §8 damage         | §5.7、§4.4               | §12             | §9.4       | —              | —         | F-25、F-61、S-13、S-19  |
| §9 帧调度         | §3.5、§3.6、§5.7、§6.3   | §19（推迟）     | §14.4、§15 | §13.1          | §13.3     | **F-59**、S-64、F-66    |
| §10 测试基建      | §6.1–§6.6、§6.8、§3.8    | §13             | §13.4      | §13.2          | §13.5     | S-63、F-63、F-65        |
| §11 尺寸/死代码   | —                        | §3.2、§4.3      | §11.1、§17 | §8.3           | §14       | F-58、S-65、V-48        |

## 17. 审查二轮（PASS-WITH-FIXES）修订记录

| 条目                           | 修订                                                                                                                                                                                                                                                                        |
|--------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **B1 空 clip 必须入栈**        | `Clip::Bounds` 的 padding box 为 0×0 时**照常** `ClipPush`（depth+1、covering 交空）：子 lane 带空前缀、damage 全被裁空，与 `hit_test` 的 clip 链一致；§5.4/§5.5 已改，回归用例 `zero_size_clip_still_pushes_and_empties_damage`                                            |
| N2① `DamageAccumulator::rects` | 去掉 `cfg` 门控（`pub(crate)`，发布构建可读）；`Window::commit_frame` 的 `debug_assert` 是默认构建的读点，P07 直接消费                                                                                                                                                      |
| N2② 上传访问器                 | `InstanceBuffer::as_bytes()` + `Window::instance_bytes()`（公开、P07 上传入口；`InstanceEncoder::buffer()` 为 crate 内读点）                                                                                                                                                |
| N2③ `FrameOp::Geometry`        | 改为 `{ index, clip_index, clip_rect: [u32;4] }`（f32 位模式，保 `Eq`）：编码时写入 clip 前缀，P07 不再丢裁剪                                                                                                                                                               |
| N2④ `FrameScheduler::commit`   | `checked_add` + `debug_assert`；溢出**饱和**到 `u32::MAX`，绝不回绕到 0（0 是「从未提交」哨兵）                                                                                                                                                                             |
| N3 `removed_lanes`             | `take_removed_lanes` → `drain_removed_lanes() -> Drain<'_, LaneId>`（借用式，容量保留）                                                                                                                                                                                     |
| N3 全透明渐变                  | 两端 alpha 都为 0 的 gradient 不入 lane、不产 damage（节点可能因此完全无图元）                                                                                                                                                                                              |
| N3 无观察者公开面              | `FrameState` 降为 `pub(crate)`；`Color::alpha` 私有；`FrameRecord::idle` 由 `Window::record_idle` 使用；删 `TestWindow::{by_id,hit}`、`Probe::scene`（`Probe::new(tree)`）                                                                                                  |
| N3 lane 按需                   | 重建后 lane 无图元 → `release_lane` + 清 `Node.lanes`；复用判据不再要求 lane 存在（靠 `REUSABLE`+`clip_key`），空节点不占 order/内存                                                                                                                                        |
| N3 `App::frame` 中止           | 所有失败分支（含窗口查找失败）都 `abort_frame`，不把调度器留在 `Draining`                                                                                                                                                                                                   |
| N1 补测                        | `resize_forces_full_damage`、`text_glyphs_use_border_and_padding_offset`、`image_maps_to_atlas_quad`、`settle_frame` 的 A11Y 保留断言、`background_and_border` 的 damage 非空断言；集成侧 `assert_oracle()` 真正调用、`FrameOutcome` 断言改为「首帧 Committed / 次帧 Idle」 |

### 17.1 P14 终审追加（2026-11-13）

| 条目                 | 修订                                                                                                                                                                                                                                                                                                               |
|----------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **`clip_rect` 语义** | `GpuPrimitive.clip_rect` 必须写契约的 `(x, y, w, h)`（`contract.md` §2、`quad.vert.glsl` 的 `.xy` + `.zw`），不再直接写内部 `aabb` 的 `(x0, y0, x1, y1)`；负原点 clip 曾因此裁掉右/下内容。见 DEVIATIONS **F-149**，回归 `negative_origin_clip_encodes_xywh`                                                       |
| **clip 深度 > 8**    | `PaintPass::run` / `Window::paint_frame` 改为 `Result`：超限返回新增的 `EngineError::ClipDepthExceeded { node, depth, limit }`（release 也失败，不再静默丢最外层）；`Window::frame` / `App::frame` 中止本帧。见 **F-150** / **V-103**，回归 `clip_depth_over_limit_is_an_error` + `clip_depth_at_limit_is_allowed` |

---

*文档完 · 本文只描述 Phase 06 的 CPU 侧实现与契约；不含 GPU 资源、平台翻译与像素快照设备。*
