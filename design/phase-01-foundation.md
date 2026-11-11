# Phase 01 · 基础骨架与依赖冻结 — 详细设计（v2，审查响应版）

> **文档类型**：实施阶段详细设计（Phase 01 的唯一权威输入）
> **上游**：`custom-gui-engine-design.md`（v2.0，D1–D25 / I1–I17）、`AGENTS.md`、`design/DEVIATIONS.md`
> **版本**：v2（2026-11-12 对抗审查修订）。v1 的模块布局、taffy 版本、像素换算三分法与 128B AA 契约已按审查结论重定；
> 全部修订见 **§11 审查响应表**，偏差条目见 `DEVIATIONS.md` §六。
> **本阶段代码状态**：只落地 `crates/kytrix_gui/{Cargo.toml, src/lib.rs, src/error.rs, src/math.rs}`；
> 其余文件只做规划与阶段归属。**本阶段不写任何 paint / render / platform / node 代码。**
> **事实基线**：Rust `1.97.1`（stable）+ `nightly 1.99.0 (b07e5a086 2026-08-07)`，clippy `0.1.99`；
> 依赖与 API 全部以 `E:\.cargo\registry\src\rsproxy.cn-e3de039b2554c837\` 下的**真实 crate 源码**为准，
> 解析由 `%TEMP%\kg-final-probe`（不在仓库内）验证。

---

## 0. 结论摘要

1. **文件树**：`crates/kytrix_gui` 使用**自命名模块根**（`text.rs` + `text/`，不使用任何 `mod.rs`），
   公共契约类型定义在模块根文件；每个文件标注落地 Phase（§3）。
2. **三平台 cfg**：不使用 `#[path]`、不使用 `pub use`、不定义运行时分派 trait/枚举。做法为
   `render.rs` 内 `#[cfg] pub(crate) mod windows;` + `#[cfg] pub(crate) use windows as backend;`，
   平台目录用 `render/windows.rs` + `render/windows/…`，平台出口用 `pub(crate) use device::Device;`，
   共享代码 `use super::backend::Device;`。该形态已用 clippy 实测通过（§4.3，含原始诊断）。
3. **依赖冻结**：workspace 根新增条目见 §5.1；隔离 probe 一次解析通过（`cargo fetch` 退出 0，三平台并集
   **238 个包**）。关键结论：accesskit 三 adapter 合并到 **0.25.2**；parley 0.12 生态在隔离 probe 中为单份
   **fontique 0.12.0 / skrifa 0.44.0 / harfrust 0.12.0 / read-fonts 0.41.0**，**不需要 peniko**；
   objc2 **不能统一**（0.6.5 与 accesskit_macos 的 0.5.3 共存，但桥接为裸指针，零成本）；
   **taffy 冻结 `=0.15.0`**（`gpui-fast 0.1.14` 精确钉 `=0.15.0`，`^0.15.1` 与它无解）；
   windows 0.62.2 需补 **`Win32_Graphics_Dxgi_Common`** 与 **`Win32_UI_Controls`**。
4. **Phase 01 API**：`EngineError`（本阶段 6 个变体 + 完整冻结变体表）、`math` 数值类型与
   **三种换算语义**（未取整浮点 / 逐边取整 / 向外覆盖）+ **128B AA 契约**（§7）。
5. **无阻塞性问题**；风险与待复核项见 §10。

---

## 1. 范围与非目标

### 1.1 本阶段交付

| 交付 | 说明 |
|------|------|
| `crates/kytrix_gui/Cargo.toml` | 包元数据 + 全量依赖（`workspace = true`），见 §5.2 |
| `crates/kytrix_gui/src/lib.rs` | **只有** `mod` / `pub mod` 声明 |
| `crates/kytrix_gui/src/error.rs` | `EngineError` + `Display` / `Error` + 内联单测 |
| `crates/kytrix_gui/src/math.rs` | 逻辑像素数值类型 + 物理像素换算 + 内联单测 |
| 根 `Cargo.toml` | `members` 增加 `crates/kytrix_gui`；`[workspace.dependencies]` 增加 §5.1 条目 |
| `design/` | 本文与 `DEVIATIONS.md` |

### 1.2 明确的非目标

- 不建 `src/render*`、`src/platform*`、`src/paint*`、`src/node*` 任何文件——空壳模块无法被 `lib.rs` 声明后通过 `-D warnings`。
- 不写 `build.rs`（P10）、不写 `tests/`（P05 起）、不写 `benches/`（P18）。
- 不新增 dev-dependency `criterion`（P18）、不新增 build-dependency `usvg`（P15，且归 `base_ui`）。
- 不做任何 `#[allow]` / `#[expect]` 兜底。

---

## 2. 命名保持策略

设计文档的「文件名 / 类型名 / 语义」尽量保持；必须改动处已在 `DEVIATIONS.md` 逐条记录，三类原因：

1. **机制性**：`#[path]`（AGENTS §1.3）、`pub use`（lint `pub_use = "forbid"`）、`mod.rs` 内定义 item
   （lint `definition_in_module_root = "forbid"`）。
2. **事实性**：taffy 0.15.0 低级 trait、parley 0.12 `TextStyle`、accesskit adapter 签名、fontique 传递 feature。
3. **强类型化**：短字符串 `String → SmolStr`、`Swapchain(String) → Swapchain { detail }`。

---

## 3. 文件树与阶段归属

**模块形态规则（关键）**：

- 本 crate **不使用 `mod.rs`**。含子模块的模块用**自命名模块根**：`text.rs`（根，可定义公共类型与 `pub(crate) use`）
  + `text/`（子模块目录）。`definition_in_module_root` 只针对 `mod.rs` / `lib.rs` / `main.rs`（§4.3 实证）。
- **公共契约类型定义在模块根文件**（`app.rs`、`window.rs`、`event.rs`、`error.rs`、`math.rs`、`text.rs`、
  `layout.rs`、`resources.rs`、`a11y.rs`、`interaction.rs`、`test_support.rs`）；设计文档 §1.2 明确拆分的模块
  （`node/`、`paint/`、`render/`、`platform/`）把公共类型放在文档指定的叶子文件。
- 行数不设上限（`DEVIATIONS.md` A-01），拆分以职责为准。

约定：`P0x` 为该文件**首次创建**的 Phase。

```text
crates/kytrix_gui/
├── Cargo.toml                                  P01
├── build.rs                                    P10   # DXC / xcrun metal / glslang → OUT_DIR
├── benches/
│   └── perf.rs                                 P18   # [[bench]] harness = false
├── src/
│   ├── lib.rs                                  P01   # 仅 mod/pub mod；逐 Phase 追加行
│   ├── error.rs                                P01   # EngineError
│   ├── math.rs                                 P01   # Point/Size/Rect/Edges/Scale/Transform/PhysicalRect
│   ├── app.rs                                  P08   # Application / App / AppHandle
│   ├── window.rs                               P08   # WindowId / WindowOptions / Window / FrameScheduler
│   ├── event.rs                                P08   # Event / InputEvent / Action / KeyBinding / HitResult
│   ├── node.rs                                 P04   # NodeBuilder / NodeHandle / Slot / Children
│   │   └── node/{id,tree,signal,dependency,dirty}.rs        P04
│   ├── layout.rs                               P05   # MeasureFn / LayoutOverride
│   │   └── layout/{taffy_tree,measure,hit_test}.rs          P05
│   ├── text.rs                                 P06   # TextStyle / ShapedLine / FontStack / InlineBoxSpec
│   │   └── text/{context,shape,inline_flow,raster,atlas,cache}.rs   P06
│   ├── paint.rs                                P03   # 公共模块根：pub mod color/style/…；无 re-export
│   │   ├── color.rs                            P02   # Color / ColorResolver / ColorBrush
│   │   ├── style.rs                            P03   # Style / StylePatch / Border / Cursor
│   │   ├── primitive.rs                        P03   # Primitive（8 类）+ 绘制序
│   │   ├── scene.rs                            P03   # Scene / clip 栈 / lane
│   │   ├── instance.rs                         P03   # GpuPrimitive（pub(crate)）+ offset_of! 断言
│   │   ├── geometry.rs                         P03   # Geometry / GeometryId / GeometryDesc
│   │   └── canvas.rs                           P03   # Canvas / CustomDraw
│   ├── render.rs                               P09   # 仅 cfg mod + backend 别名 + pub(crate) 出口
│   │   ├── device.rs  frame.rs  retire.rs  damage.rs  atlas.rs  timer.rs  compositor.rs   P09
│   │   ├── windows.rs                          P11   # 平台出口（pub(crate) use device::Device 等）
│   │   │   └── render/windows/{device,queue,swapchain,pipeline,descriptor,present,dred}.rs  P11
│   │   ├── macos.rs                            P16
│   │   │   └── render/macos/{device,layer,pipeline,present}.rs            P16
│   │   ├── linux.rs                            P17
│   │   │   └── render/linux/{instance,device,swapchain,pipeline,present}.rs P17
│   │   └── shaders/                            P10   # contract.md + quad/geometry × 3 + common/
│   ├── platform.rs                             P08   # 仅 cfg mod + native 别名 + pub(crate) 出口
│   │   ├── dispatcher.rs  event.rs             P08
│   │   ├── windows.rs                          P12
│   │   │   └── platform/windows/{event_loop,window,ime,clipboard,a11y,dpi}.rs  P12
│   │   ├── macos.rs                            P16
│   │   │   └── platform/macos/{event_loop,view,ime,clipboard,a11y}.rs      P16
│   │   └── linux.rs                            P17   # 仅 Wayland（I16）
│   │       └── platform/linux/{event_loop,surface,ime,clipboard,a11y}.rs   P17
│   ├── interaction.rs                          P13   # FocusScope/ScrollHandle/VirtualList/Overlay/DragState/AnimationId
│   │   └── interaction/{focus,scroll,virtual_list,overlay,drag,animation}.rs  P13
│   ├── a11y.rs                                 P14   # A11yRole/A11yNode/A11yLabel/A11yAction/A11yOverride/A11yPatch
│   │   └── a11y/{tree,mapping,adapter,action}.rs           P14
│   ├── resources.rs                            P15   # ResourceId / ResourceCache / AtlasTile / AtlasKind
│   │   └── resources/{cache,image}.rs          P15
│   └── test_support.rs                         P08   # TestApp / TestWindow / Probe / FrameRecord / SnapshotCase
│       └── test_support/{probe,frame_record,snapshot,oracle}.rs  P08/P18
└── tests/                                      # 集成测试（只走公开 API，AGENTS §5.2）
    ├── layout.rs                               P05
    ├── frame_record.rs                         P07
    ├── snapshot.rs                             P18
    └── golden/                                 P18   # 数据目录（warp / lavapipe / metal-real）
```

补充规则：

- `platform/*/event_loop.rs` 取代设计文档的 `loop.rs`：`loop` 是 Rust 关键字，模块名需写 `r#loop`（见 DEVIATIONS F-16）。
- `resources/svg_build.rs` **删除**：构建期 SVG 由 `base_ui` 的 `build.rs` 承担（DEVIATIONS F-11/F-16）。
- `test_support` 由 `#[cfg(any(test, feature = "test-support"))] pub mod test_support;` 门控；`Probe`/`FrameRecord`
  在 P08、`SnapshotCase` 在 P18 落地（同一模块根文件后续追加）。
- 设计文档的 `node/tests.rs`、`a11y/tests.rs` 取消（DEVIATIONS M-04）。

---

## 4. 三平台 cfg 落地（无 `#[path]`、无 trait/枚举分派）

### 4.1 目录形态与根文件

```rust
// src/render.rs —— 模块根：cfg 选择 + 私有/ crate 出口，不定义 struct（此处无契约类型）
#[cfg(target_os = "windows")]
pub(crate) mod windows;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg(target_os = "linux")]
pub(crate) mod linux;

#[cfg(target_os = "windows")]
pub(crate) use windows as backend;
#[cfg(target_os = "macos")]
pub(crate) use macos as backend;
#[cfg(target_os = "linux")]
pub(crate) use linux as backend;

mod atlas;
mod compositor;
mod damage;
mod device;
mod frame;
mod retire;
mod timer;

pub(crate) use atlas::AtlasTexture;
pub(crate) use device::{DeviceInitError, GpuCaps};
pub(crate) use frame::FrameIndex;
pub(crate) use retire::RetireQueue;
pub(crate) use timer::{GpuTimerSample, Section};
// `damage` 不导出类型：damage 的整数矩形就是 `math::PhysicalRect`（DEVIATIONS F-09），
// 该模块只提供 `expand / merge / degenerate_to_full_frame` 等语义函数。
```

> 注意：`render.rs` 是 **crate 私有**模块，根文件里的 `pub(crate) use` 只做 crate 内出口；
> 应用层不可见（I9）。公共模块（`paint`、`text` 等）的根文件则定义**公共契约类型**（§4.5）。

```rust
// src/render/windows.rs —— 平台模块根：子模块 + 本平台具体类型的唯一出口
pub(crate) mod descriptor;
pub(crate) mod device;
pub(crate) mod dred;
pub(crate) mod pipeline;
pub(crate) mod present;
pub(crate) mod queue;
pub(crate) mod swapchain;

pub(crate) use device::Device;
pub(crate) use present::Presenter;
pub(crate) use queue::Queue;
pub(crate) use swapchain::Swapchain;
```

`platform.rs` 完全同构（别名 `native`）：

```rust
// src/platform.rs
#[cfg(target_os = "windows")]
pub(crate) mod windows;
#[cfg(target_os = "windows")]
pub(crate) use windows as native;

mod dispatcher;
mod event;

pub(crate) use dispatcher::PlatformDispatcher;
```

`render/macos.rs`、`render/linux.rs`、`platform/{macos,linux}.rs` 各自导出**同名具体类型**
（`Device` / `Swapchain` / `Presenter` / `WindowHandle` / `A11yAdapter`），共享层只写一份调用代码。

**两类模块根模板**（其余文件按此二选一，不得混用）：

| 类别 | 适用 | 根文件写法 | 应用层可见性 |
|------|------|-----------|--------------|
| **公共模块根** | `paint.rs`、`node.rs`、`text.rs`、`layout.rs`、`app.rs`、`event.rs`、`window.rs`、`resources.rs`、`a11y.rs`、`interaction.rs`、`test_support.rs` | `pub mod <child>;`（子模块是公共路径的一部分）；契约类型直接定义在根文件；**无 re-export** | 公共类型按 §4.5 路径访问，如 `paint::style::Style` |
| **crate 私有模块根** | `render.rs`、`platform.rs` 及其 `render/{os}.rs`、`platform/{os}.rs` | `pub(crate) mod <child>;` + `pub(crate) use <child>::<Type>;`（crate 内出口）；不定义公共契约类型 | 不可见（I9）；仅 crate 内 `use super::backend::…` |

公共模块根**不使用** `pub(crate) use` 做「扁平出口」——那等于绕过 `pub_use` 禁令；它的子模块必须 `pub mod`，
公共路径如实反映文件归属。私有模块根则相反：子模块 `pub(crate)`，出口用 `pub(crate) use` 收敛为一个名字。

### 4.2 共享代码如何引用平台具体类型

```rust
// src/render/compositor.rs —— 共享层，不出现任何 #[cfg]
use super::backend::{Device, Presenter, Swapchain};
```

```rust
// src/platform/dispatcher.rs
use super::native::WindowHandle;
```

- 别名是 `pub(crate) use`（lint 允许，§4.3），只在本 crate 内可见；应用层看不到（I9）。
- 这是**编译期替身**：`cfg` 阶段只保留一个平台 `mod`，`backend` 指向唯一真实类型；不存在 trait、枚举或 `dyn`（I1/I14）。
- 兄弟模块（如 `paint/`）不得直接 `use crate::render::windows::Device`；需要跨模块时用 `crate::render::backend::…`。

### 4.3 lint 实证（保留，防止回退到 `#[path]` / `pub use` / `mod.rs`）

probe 位于 `%TEMP%\kg-lint-probe3`、`kg-lint-probe4`、`kg-lint-probe5`、`kg-modroot-probe`（均不在仓库内），
`[lints.clippy]` 逐字复制 workspace 根，命令 `cargo +nightly clippy --all-targets -- -D warnings`（clippy `0.1.99`）。

**(a) `definition_in_module_root` 只针对 `mod.rs` / `lib.rs` / `main.rs`，不针对自命名根（`math.rs`、`render/windows.rs`）。**
`kg-lint-probe4` 的 `src/render/mod.rs` 同时含 `struct Renderer` 与 `impl Renderer`，报两条：

```
error: definition in module root file
 --> src\render\mod.rs:7:1
  |
7 | / pub(crate) struct Renderer {
  = help: move struct `Renderer` to a dedicated file
  = note: requested on the command line with `-F clippy::definition-in-module-root`
```

`kg-modroot-probe`（本次新增）验证自命名根形态：`math.rs` 内定义 `pub struct Point` + `impl Point`、
`render/windows.rs` 内 `pub(crate) mod device; pub(crate) use device::Device;`、`render.rs` 内
`pub(crate) use windows as backend;`、`render/shared.rs` 内 `use super::backend::Device;` ——
**退出码 0，零诊断**。→ 结论：`mod.rs`/`lib.rs` 只放 `mod` 声明（`lib.rs` 见 §6.1）；
自命名模块根可定义类型与 `pub(crate) use`。

**(b) `pub_use` 禁止 `pub use`，不禁止 `pub(crate) use`。** `kg-lint-probe3`：

```
error: using `pub use`
 --> src\a\mod.rs:3:1
3 | pub use inner::Thing;
  = help: move the exported item to a public module instead
  = note: requested on the command line with `-F clippy::pub-use`
```

同文件第 4 行 `pub(crate) use inner::Hidden;` 未被 `pub_use` 命中（只报未使用）。
→ 结论：**公共 API 禁止 `pub use`；内部出口允许 `pub(crate) use`**（平台出口正是靠它）。

**(c) `inline_modules` 不禁止 `#[cfg(test)] mod tests`。** `kg-lint-probe5` 通过 `-D warnings`，
与仓库现有 **277 处** `#[cfg(test)] mod tests` 一致（其中 **272 处内联在 `.rs` 文件内**、**5 处为文件式**
`mod tests;`；统计方法见附录 C）。

### 4.4 被排除的替代方案

| 方案 | 排除原因 |
|------|----------|
| `#[path = "windows/mod.rs"] mod backend;`（设计文档 §1.2 原文） | AGENTS §1.3 禁止 `#[path]`；且模块名与目录脱钩，破坏导航与 `cargo doc` |
| `pub use backend::{Device, …};`（设计文档 §1.2 原文） | lint `pub_use = "forbid"`，实测 `-D warnings` 失败（§4.3b） |
| `mod.rs` 承载共享契约（v1 方案） | lint `definition_in_module_root` 禁止在 `mod.rs` 定义任何 item；改用自命名根即可在根文件放公共类型 |
| `trait Renderer` + 三实现 / `enum Backend` | 违反 I1/D2：运行时多态强迫三套后端同构，正是设计要避免的抽象 |
| 共享层写 `#[cfg(target_os)]` 分支 | §2.1 明令禁止；平台差异必须收在 `render/{os}.rs`、`platform/{os}.rs` 内 |

### 4.5 公共 API 路径表（`pub use` 被禁的后果）

路径 = **类型定义所在文件对应的模块路径**。单文件模块的类型在模块根（`text::TextStyle`、`test_support::TestApp`）；
设计文档 §1.2 明确拆分的模块按文档文件归属（`paint::style::Style`、`node::id::NodeId`）。

| 类型 | 公开路径 | 定义文件 |
|------|----------|----------|
| `EngineError` | `kytrix_gui::error::EngineError` | `src/error.rs` |
| `Point` `Size` `Rect` `Edges` `Scale` `Transform` `PhysicalRect` | `kytrix_gui::math::*` | `src/math.rs` |
| `Application` `App` `AppHandle` | `kytrix_gui::app::*` | `src/app.rs` |
| `WindowId` `WindowOptions` `Window` | `kytrix_gui::window::*` | `src/window.rs` |
| `Event` `InputEvent` `Action` `KeyBinding` `HitResult` | `kytrix_gui::event::*` | `src/event.rs` |
| `NodeId` | `kytrix_gui::node::id::NodeId` | `src/node/id.rs` |
| `NodeBuilder` `NodeHandle` `Slot` `Children` | `kytrix_gui::node::*` | `src/node.rs` |
| `Signal<T>` `SignalId` `Derived<T>` `Transient<T>` `TaskHandle` | `kytrix_gui::node::signal::*` | `src/node/signal.rs` |
| `DerivedId` | `kytrix_gui::node::signal::DerivedId` | `src/node/signal.rs` |
| `MeasureFn` `LayoutOverride` | `kytrix_gui::layout::*` | `src/layout.rs` |
| `Axis` | `kytrix_gui::layout::Axis` | `src/layout.rs` |
| `Style` `StylePatch` `Border` `Cursor` | `kytrix_gui::paint::style::*` | `src/paint/style.rs` |
| `Primitive` | `kytrix_gui::paint::primitive::Primitive` | `src/paint/primitive.rs` |
| `Scene` | `kytrix_gui::paint::scene::Scene` | `src/paint/scene.rs` |
| `Canvas` | `kytrix_gui::paint::canvas::Canvas` | `src/paint/canvas.rs` |
| `Geometry` `GeometryId` `GeometryDesc` | `kytrix_gui::paint::geometry::*` | `src/paint/geometry.rs` |
| `Color` `ColorResolver` `ColorBrush` | `kytrix_gui::paint::color::*` | `src/paint/color.rs` |
| `TextStyle` `ShapedLine` `FontStack` `InlineBoxSpec` | `kytrix_gui::text::*` | `src/text.rs` |
| `ResourceId` `ResourceCache` `AtlasTile` | `kytrix_gui::resources::*` | `src/resources.rs` |
| `AtlasKind` | `kytrix_gui::resources::AtlasKind` | `src/resources.rs` |
| `FocusScope` `ScrollHandle` `VirtualList` `Overlay` `DragState` `AnimationId` | `kytrix_gui::interaction::*` | `src/interaction.rs` |
| `A11yRole` `A11yNode` `A11yLabel` `A11yAction` `A11yOverride` `A11yPatch` | `kytrix_gui::a11y::*` | `src/a11y.rs` |
| `TestApp` `TestWindow` `Probe` `FrameRecord` `SnapshotCase` | `kytrix_gui::test_support::*` | `src/test_support.rs` |

`render`、`platform` 整体 `pub(crate)`（应用层不得接触 `Device`/`HWND`/`NSView`/`wl_surface`，I9）；
`paint::instance::GpuPrimitive` 保持 `pub(crate)`。此表为应用层迁移的唯一路径契约。

**规则（路径的唯一判据）**：公开路径 = **类型实际定义所在的文件路径**。模块根路径
（`text::TextStyle`、`test_support::TestApp`）只在「该类型本就定义在模块根」时使用；`paint`/`node` 的类型定义在
设计文档 §1.2 指定的叶子文件里，因此路径保留子模块名。**不得为了缩短路径而移动类型定义**——那会与 §1.2 的文件职责
（`paint/style.rs` = 样式契约、`node/id.rs` = 节点句柄）冲突，也会让「模块根 = 契约 / 子模块 = 实现」的划分失真。
新增公共类型时，先按 §1.2 决定它的归属文件，路径随之确定，再登记进本表。

---

## 5. 依赖冻结

### 5.1 workspace 根 `[workspace.dependencies]` 新增/变更

已存在的保持不变（`color 0.3.3`、`image 0.25.10`、`windows 0.62.2`、`ahash 0.8.12`、`bytes 1.12.1`、`log 0.4.35`、
`smallvec 1.16.5`、`smol_str 0.3.7`、`thiserror 2.0.21`、`tokio 1.54.1`）。新增条目按依赖域分组：

```toml
# ── 数值 / 容器（跨阶段） ──
bitflags = "2.13.2"
bytemuck = { version = "1.25.2", features = ["derive"] }

# ── 布局（P05）：精确钉 0.15.0，理由见 §5.3④ ──
taffy = { version = "=0.15.0", default-features = false, features = [
    "std",
    "flexbox",
    "grid",
    "block_layout",
    "calc",
    "content_size",
] }

# ── 文本（P06） ──
parley = "0.12.0"
fontique = "0.12.0"
skrifa = "0.44.0"
swash = "0.2.10"

# ── 图集 / 路径 / 构建期 SVG（P02/P09/P15） ──
etagere = "0.3.0"
lyon = "1.0.19"
usvg = { version = "0.49.0", default-features = false }

# ── 无障碍（P14） ──
accesskit = "0.25.2"
accesskit_windows = "0.35.2"
accesskit_macos = "0.27.2"
accesskit_unix = { version = "0.24.1", default-features = false, features = ["tokio"] }

# ── macOS 平台（P16） ──
objc2 = { version = "0.6.5", features = ["std"] }
objc2-foundation = { version = "0.3.2", features = [
    "std", "alloc", "NSArray", "NSBundle", "NSData", "NSDate", "NSDictionary", "NSError",
    "NSNotification", "NSObject", "NSRange", "NSRunLoop", "NSString", "NSThread", "NSURL", "NSValue",
] }
objc2-app-kit = { version = "0.3.2", features = [
    "std", "alloc", "NSAccessibility", "NSAccessibilityConstants", "NSAccessibilityElement",
    "NSAccessibilityProtocols", "NSApplication", "NSColor", "NSEvent", "NSFont", "NSGraphics",
    "NSImage", "NSMenu", "NSMenuItem", "NSPasteboard", "NSResponder", "NSRunningApplication",
    "NSScreen", "NSText", "NSTextInputClient", "NSTextInputContext", "NSView", "NSWindow", "NSWorkspace",
] }
objc2-quartz-core = { version = "0.3.2", features = [
    "std", "alloc", "CAMetalLayer", "CATransaction", "CALayer",
] }
objc2-metal = { version = "0.3.2", features = [
    "std", "alloc", "MTLBlitCommandEncoder", "MTLBuffer", "MTLCommandBuffer", "MTLCommandEncoder",
    "MTLCommandQueue", "MTLDepthStencil", "MTLDevice", "MTLDrawable", "MTLEvent", "MTLLibrary",
    "MTLPixelFormat", "MTLRenderCommandEncoder", "MTLRenderPass", "MTLRenderPipeline", "MTLResource",
    "MTLSampler", "MTLTexture", "MTLTypes",
] }
objc2-core-foundation = { version = "0.3.2", features = ["std", "alloc", "CFString"] }
objc2-core-graphics = { version = "0.3.2", features = [
    "std", "alloc", "CGColor", "CGColorSpace", "CGContext", "CGGeometry",
] }

# ── Linux / Wayland（P17，I16：无任何 X11 依赖） ──
ash = { version = "0.38.0", default-features = false, features = ["std", "loaded", "debug"] }
wayland-client = "0.31.15"
wayland-protocols = { version = "0.32.13", features = ["client", "staging", "unstable"] }
calloop = "0.14.5"
calloop-wayland-source = "0.4.1"
xkbcommon = { version = "0.9.0", default-features = false, features = ["wayland"] }

# ── 基准（P18） ──
criterion = "0.8.2"

# ── workspace 成员 ──
kytrix_gui = { path = "crates/kytrix_gui" }
```

`members` 增加 `"crates/kytrix_gui"`。

**不声明**：`harfrust` / `icu_properties` / `icu_segmenter`——它们是 `parley_engine 0.12` 的传递依赖，
`[workspace.dependencies]` 中未被成员引用的条目不参与解析，写了也不能「钉住」；P06 若需直接 `use` 再追加，
届时按仓库实际解析版本冻结（仓库受 `boa_parser 0.22` 的 `~2.3` 约束，icu 会停在 **2.3.x**，见 §5.3②）。
同理不声明：`peniko`、`raw-window-handle`、`winit` / `accesskit_winit`、任何 X11 库。

### 5.2 `crates/kytrix_gui/Cargo.toml`

```toml
[package]
name = "kytrix_gui"
version.workspace = true
publish.workspace = true
edition.workspace = true

[dependencies]
# P01 实际使用
smol_str = { workspace = true }
thiserror = { workspace = true }

# P02 颜色 / P03 绘制契约
bitflags = { workspace = true }
bytemuck = { workspace = true }
color = { workspace = true }
lyon = { workspace = true }

# P05 布局 / P06 文本
fontique = { workspace = true }
parley = { workspace = true }
skrifa = { workspace = true }
swash = { workspace = true }
taffy = { workspace = true }

# P09 渲染共享层
etagere = { workspace = true }

[target.'cfg(windows)'.dependencies]
windows = { workspace = true, features = [
    # 窗口 / 事件 / DPI / IME
    "Win32_Foundation",
    "Win32_Globalization",
    "Win32_Graphics_Gdi",
    "Win32_System_Com",
    "Win32_System_DataExchange",
    "Win32_System_LibraryLoader",
    "Win32_System_Ole",
    "Win32_System_Performance",
    "Win32_System_Registry",
    "Win32_System_SystemServices",
    "Win32_System_Threading",
    "Win32_System_Variant",
    "Win32_System_WindowsProgramming",
    "Win32_UI_Accessibility",
    "Win32_UI_Controls",          # DwmExtendFrameIntoClientArea 的 MARGINS
    "Win32_UI_HiDpi",
    "Win32_UI_Input_Ime",
    "Win32_UI_Input_KeyboardAndMouse",
    "Win32_UI_Shell",
    "Win32_UI_WindowsAndMessaging",
    # D3D12 / DXGI
    "Win32_Graphics_Direct3D",      # D3D_FEATURE_LEVEL / ID3DBlob
    "Win32_Graphics_Direct3D12",
    "Win32_Graphics_Direct3D_Fxc",  # D3D12SerializeRootSignature 返回 ID3DBlob
    "Win32_Graphics_Dwm",
    "Win32_Graphics_Dxgi",
    "Win32_Graphics_Dxgi_Common",   # DXGI_FORMAT / D3D12 管线方法门控
] }
accesskit = { workspace = true }
accesskit_windows = { workspace = true }

[target.'cfg(target_os = "macos")'.dependencies]
objc2 = { workspace = true }
objc2-foundation = { workspace = true }
objc2-app-kit = { workspace = true }
objc2-quartz-core = { workspace = true }
objc2-metal = { workspace = true }
objc2-core-foundation = { workspace = true }
objc2-core-graphics = { workspace = true }
accesskit = { workspace = true }
accesskit_macos = { workspace = true }

[target.'cfg(target_os = "linux")'.dependencies]
accesskit = { workspace = true }
accesskit_unix = { workspace = true }
ash = { workspace = true }
calloop = { workspace = true }
calloop-wayland-source = { workspace = true }
wayland-client = { workspace = true }
wayland-protocols = { workspace = true }
xkbcommon = { workspace = true }

[lints]
workspace = true
```

说明：

1. **P01 实际只 `use` 到 `thiserror` 与 `smol_str`**；其余条目一次写全，目的是在仓库内验证三平台解析（§9），
   并避免后续 Phase 反复改 manifest。未使用依赖不触发 `unused_crate_dependencies`（workspace 未开启），
   代价是首次构建时间（§10 R3）。
2. `windows` features 写在成员 crate（沿用 `crates/network` 既有写法），根只留版本。
3. **不需要** `Win32_Graphics_DirectWrite`：`fontique` 的 `system` feature 已通过
   `[target.'cfg(windows)'.dependencies.windows] features = ["Win32_Graphics_DirectWrite"]` 传递引入（§5.3⑥）。
4. `accesskit_unix` 关闭默认 `async-io`、走 `tokio` feature（D21 单一 runtime）。
5. `[dev-dependencies] criterion`（P18）与 `[build-dependencies]`（P10）后续追加。

### 5.3 解析结论（六个必须验证点）

probe `%TEMP%\kg-final-probe`（清单 = §5.2 的并集，含三平台 target 分组）：
`cargo fetch` **退出码 0**；`cargo metadata` **238 个包**；`taffy` 解析为 **0.15.0**。

**① accesskit 三 adapter 合并到同一 0.25.x —— 成立。**
`accesskit_windows 0.35.2 → accesskit ^0.25.2`、`accesskit_macos 0.27.2 → ^0.25.2`、
`accesskit_unix 0.24.1 → ^0.25.2` → 解析出唯一 **accesskit 0.25.2**（adapter 版本号不同步是事实：0.35/0.27/0.24）。
`accesskit_windows` 依赖 `windows ^0.62.0` + `windows-core ^0.62.0`，与引擎的 `windows 0.62.2` 同版本，无第二份 `windows`。

**② parley 0.12 生态 —— 隔离 probe 中单份；仓库内会有并行版本。**
`parley 0.12.0` 的 normal 依赖：`fontique ^0.12.0`、`hashbrown ^0.17.1`、`linebender_resource_handle ^0.1.1`、
`parlance ^0.1.1`、**`parley_engine ^0.12.0`**、`smallvec ^1.16.2`；`peniko` 只是 dev-dependency。
`parley_engine 0.12.0` → `fontique 0.12`、**`harfrust ^0.12.0`**、**`skrifa ^0.44.0`**、`icu_* ^2.3.0`、`parley_data`、`parley_emoji`。
隔离 probe 解析结果：**单份 fontique 0.12.0 / skrifa 0.44.0 / read-fonts 0.41.0 / harfrust 0.12.0 / font-types 0.12.7**。
→ `harfrust` 必须 `>=0.12,<0.13`、`skrifa` 必须 `0.44.x`（更高版本会把 `read-fonts` 拆成两份）；
→ `swash 0.2.10` 也依赖 `skrifa ^0.44`，故三者共用同一份 skrifa；
→ **`peniko` 不需要**：`parley::style::Brush` 是**标记 trait**（`Clone + PartialEq + Default + Debug`，blanket impl），
引擎自定义 `paint::color::ColorBrush` 即可；
→ **icu 不直接声明**：`parley_engine` 已用 `icu_segmenter`/`icu_properties`。隔离 probe 取到 **2.4.0**，
但本仓库的 `boa_parser 0.22` 约束 `~2.3`，因此在仓库内两者会并到 **2.3.x**——这正是不能由引擎锁死 icu 版本的原因。

**③ objc2 不能统一 —— 但桥接成本为 0。**
`objc2-app-kit 0.3.2` 等要求 `objc2 >=0.6.2,<0.8.0`；`accesskit_macos 0.27.2` 固定 `objc2 ^0.5.1` +
`objc2-app-kit ^0.2.0` + `objc2-foundation ^0.2.0`（最新版无 0.6 线适配）。解析结果：
`objc2 0.5.3 + 0.6.5`、`objc2-app-kit 0.2.2 + 0.3.2`、`objc2-foundation 0.2.2 + 0.3.2`、`objc2-metal 0.2.2 + 0.3.2`。
**允许共存**：两个 `objc2` 版本均无 `links` 键（`objc2-0.6.5/Cargo.toml` 无 `links =`），Cargo 不拒绝。
关键事实：`accesskit_macos::Adapter::new` 是 `pub unsafe fn new(view: *mut c_void, is_view_focused: bool,
action_handler: impl 'static + ActionHandler)`——**收裸指针**，P16 直接 `ns_view_ptr.cast()`，无需跨版本类型转换。
`accesskit_macos` 仅 re-export `objc2_foundation::{NSArray, NSObject, NSPoint}` 三个 0.2 线类型；
进入调用面的只有 `NSPoint`，而它可直接用 `accesskit_macos::NSPoint` 构造。

**④ taffy 冻结 `=0.15.0`（不能用 `0.15.1`）。**
本仓库 `gpui-fast 0.1.14` 声明 **`taffy =0.15.0`**（sparse index 原文），仓库 `Cargo.lock` 亦为 0.15.0。
若引擎写 `taffy = "0.15.1"`（caret `^0.15.1`），Cargo **无解**（实测）：

```
error: failed to select a version for `taffy`.
    ... required by package `b v0.0.0`
versions that meet the requirements `^0.15.1` are: 0.15.1
all possible versions conflict with previously selected packages
  previously selected package `taffy v0.15.0`
```

（0.15.0 与 0.15.1 semver 兼容，Cargo 不会保留两份，故必须与 `gpui-fast` 取同一版本。）
**0.15.0 与 0.15.1 的公共 API 文件逐字节相同**（实测 SHA256：`lib.rs`、`tree/traits.rs`、`tree/layout.rs`、
`tree/cache.rs`、`style/mod.rs`、`compute/mod.rs`、`prelude.rs`、`tree/mod.rs` 全部 IDENTICAL；52 个源文件中
14 个内部实现文件有差异，属算法修复），因此附录 A.1 对 0.15.0 完全成立。
**升级路径**：M8 移除 `gpui-*` 后，再把 `=0.15.0` 放开为 `0.15.x` 并复核（V-07）。
feature 名（`features2` 表）：`std / flexbox / grid / block_layout / calc / content_size / taffy_tree / parse / serde / detailed_layout_info`；
采用 `default-features = false` + 前 6 个（不拉 `slotmap`/`cssparser`；`detailed_layout_info` 在源码中无门控，不启用）。

**⑤ windows 0.62.2 需补两条 feature。**
已确认存在：`Win32_Graphics_Direct3D12`、`Win32_Graphics_Dxgi`、`Win32_Graphics_Dwm`、`Win32_UI_Input_Ime`、
`Win32_Graphics_Direct3D`、`Win32_Graphics_Direct3D_Fxc`、`Win32_UI_HiDpi`、`Win32_UI_Accessibility`、`Win32_System_Com` 等。
**必须补**：`Win32_Graphics_Dxgi_Common`（`CreateGraphicsPipelineState` 等方法被该 feature 门控；`DXGI_FORMAT` 在此）、
`Win32_UI_Controls`（`DwmExtendFrameIntoClientArea` 的参数 `MARGINS` 定义在 `Win32::UI::Controls`）。共 **26** 项 feature。

**⑥ fontique 传递引入 DirectWrite，Windows 上会出现并行版本（预期）。**
`fontique 0.12.0` 的 `system` feature（`default` 即含）在 Windows 上依赖
`windows 0.62.2` + `features = ["Win32_Graphics_DirectWrite"]`（optional，target windows），Apple 上依赖
`objc2` / `objc2-core-foundation` / `objc2-core-text`。因此在 Windows 上 DirectWrite 能力**自动可用**，
引擎不需要也不应再声明它；同时 `windows` 的 feature 集合由 Cargo 统一（fontique 的 feature 会合并进来）。
同理需知：并入仓库后会有**预期内的并行版本**——`accesskit 0.24.1`（gpui-fast）+ `0.25.2`（引擎）、
`skrifa 0.40.0`（现有生态）+ `0.44.0`（引擎）、`harfrust 0.5.2`（`cosmic-text 0.19`）+ `0.12.0`（引擎）、
`objc2 0.5.3 + 0.6.5`。它们不影响引擎自身的依赖边，也不违反单一版本纪律（不同 semver 线）。

### 5.4 版本选择说明

| crate | 选定 | 理由 |
|-------|------|------|
| `taffy` | `=0.15.0` | 见 §5.3④；`gpui-fast` 精确钉死，`^0.15.1` 无解；公共 API 与 0.15.1 一致 |
| `etagere` | `0.3.0` | 最新稳定；`AtlasAllocator` 4 个入口与 0.2 同形；0.2.15 是 gpui 遗留 |
| `lyon` | `1.0.19` | 仓库已用；`lyon_tessellation` 是普通依赖，无需 feature |
| `usvg` | `0.49.0` + `default-features=false` | 只做构建期 path 解析；关闭默认的 `text/system-fonts/memmap-fonts/writer/svgz`。**P15 验证**默认关闭下 path-only 可解析 |
| `ash` | `0.38.0+1.3.281` | `loaded`（`libloading` 载入 `vulkan-1`）+ `std` + `debug` |
| `xkbcommon` | `0.9.0` + `wayland`，无 `x11` | 无 build.rs，`#[link(name="xkbcommon")]`（`src/xkb/ffi.rs:114`）→ **链接期**需要系统库；`cargo check` 不链接、不需要（§9） |
| `wayland-client` | `0.31.15` 默认 | 默认不启用 `system`，纯 Rust 协议实现，不链 `libwayland` |
| `accesskit_unix` | `0.24.1` + `tokio` | D21 单一 runtime；关闭默认 `async-io` |

---

## 6. Phase 01 精确 API

### 6.1 `src/lib.rs`

```rust
//! Kytrix 自研跨平台 GUI 引擎。
//!
//! 本文件只做模块声明（AGENTS §5.5）；公共 API 通过模块路径暴露，不做 re-export
//! （workspace lint `pub_use = "forbid"`，见 design/phase-01-foundation.md §4.3）。

pub mod error;
pub mod math;
```

约束：无 `use`、无定义、无测试；后续 Phase 只追加行；`mod render;` / `mod platform;` / `mod paint;`
为私有模块（`render`/`platform` 整体 `pub(crate)`，`paint` 在 P03 才成为 `pub mod`）。

### 6.2 `src/error.rs`

#### 6.2.1 结论：变体分类法与 payload **完整冻结**，实现按 Phase 分批

理由：`EngineError` 是公开错误面（§9.1）；变体名与 payload 必须一次定死，否则各 Phase 各造同义变体。
但 payload 依赖的类型尚未定义（`NodeId`、`DerivedId` P04；`Axis` P05；`AtlasKind` P09；`ResourceId` P15），
**P01 只实现不依赖未定义类型的 6 个**。
`EngineError` **不加 `#[non_exhaustive]`**：`publish = false`、应用层与引擎同仓演进，穷尽 match 更有价值。

#### 6.2.2 完整冻结变体表

| # | 变体 | payload | 实现 Phase | 语义 / 生产者 |
|---|------|---------|-----------|----------------|
| 1 | `UnsupportedGpu` | `{ os: &'static str, required: &'static str, actual: SmolStr }` | **P01** | D8 基线不满足（P09 检测） |
| 2 | `DeviceLost` | `{ reason: SmolStr, log_path: PathBuf }` | **P01** | 设备丢失（P09/P11/P16/P17） |
| 3 | `WaylandCompositor` | `{ detail: SmolStr }` | **P01** | Wayland 合成器不满足基线（P17） |
| 4 | `ShaderLoad` | `{ target: &'static str, path: PathBuf }` | **P01** | shader 产物加载失败（P10/P11/P16/P17） |
| 5 | `Swapchain` | `{ detail: SmolStr }` | **P01** | swapchain / offscreen 重建失败（P09） |
| 6 | `InvalidDpiScale` | `{ factor: f32 }` | **P01** | 平台 DPI 事件给出非有限或 `<=0` 的比例（P12/P16/P17） |
| 7 | `StaleNode` | `{ node: NodeId }` | P04 | 代际不匹配（设计 §3.2） |
| 8 | `CyclicDerived` | `{ derived: DerivedId }` | P04 | `Derived` 注册时检测到环（设计 §3.9） |
| 9 | `InvalidMeasure` | `{ node: NodeId, axis: Axis, value: f32 }` | P05 | measure 闭包返回 NaN/负数（设计 §4.8） |
| 10 | `LayoutInvalid` | `{ node: NodeId, detail: SmolStr }` | P05 | **替代** `Layout(String)`：taffy 低级 API 无 `Result`，只由引擎后置校验产出 |
| 11 | `GeometryRegister` | `{ detail: SmolStr }` | P03 | 注册静态几何时 stride/index/topology 非法 |
| 12 | `AtlasExhausted` | `{ kind: AtlasKind }` | P09 | 图集无法容纳新图元（重排后仍失败） |
| 13 | `TextShape` | `{ node: NodeId, detail: SmolStr }` | P06 | 字体回退全部失败 / `InlineBox` 超宽降级需上报 |
| 14 | `AppInit` | `{ detail: SmolStr }` | P08 | `Application::run` 前置条件失败（tokio runtime 构建 / 平台初始化） |
| 15 | `WindowCreate` | `{ detail: SmolStr }` | P12 | 原生窗口创建失败（`CreateWindowExW` / `NSWindow` / `xdg_toplevel`） |
| 16 | `ResourceDecode` | `{ resource: ResourceId, detail: SmolStr }` | P15 | 图片/SVG 字节解码失败 |
| 17 | `A11yInit` | `{ detail: SmolStr }` | P14 | 平台 a11y adapter 初始化失败（窗口显示前） |

`Display` 语义：所有变体英文诊断文本；`source()` 为 `None`（不包装外部错误，外部错误码降为字段/文本）；
`EngineError` 必须 `Send + Sync + 'static`（跨线程经 `PlatformDispatcher` 回投），由测试断言。

#### 6.2.3 P01 实现签名

```rust
use std::path::PathBuf;

use smol_str::SmolStr;

/// 引擎公开错误面。
///
/// `Display` 为英文诊断文本，只用于日志 / CI / 开发者；面向用户的本地化文案由应用层按变体生成
/// （§1.3：i18n 留应用层）。D8 的信息完整性要求（实际值 + 最低要求 + 指向发布说明 + 不降级）
/// 与语言无关，由单元测试守护（§8.2）。
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// D8 基线不满足。`actual` 按 `required` 的项顺序逐项拼接。
    #[error(
        "GPU baseline not met: {os} requires {required}; detected {actual}. \
         This build has no software or no-GPU fallback; see the release notes for the supported \
         hardware baseline."
    )]
    UnsupportedGpu {
        os: &'static str,
        required: &'static str,
        actual: SmolStr,
    },

    /// 图形设备丢失；`log_path` 为已落盘的 DRED / validation layer 诊断。
    #[error("graphics device lost: {reason}; diagnostics written to {log_path}")]
    DeviceLost { reason: SmolStr, log_path: PathBuf },

    /// Wayland 合成器不满足基线（缺 `xdg_wm_base` / 版本不足 / 无 `wl_output`）。
    #[error("Wayland compositor does not meet requirements: {detail}")]
    WaylandCompositor { detail: SmolStr },

    /// shader 产物加载失败。`target` 为 `"dxil"` / `"metallib"` / `"spirv"`。
    #[error("failed to load shader artifact for {target} at {path}")]
    ShaderLoad { target: &'static str, path: PathBuf },

    /// swapchain / offscreen 重建失败。
    #[error("swapchain reconfiguration failed: {detail}")]
    Swapchain { detail: SmolStr },

    /// 平台报告的 DPI 缩放因子非法（非有限值或 <= 0）。
    #[error("invalid DPI scale factor {factor}: must be finite and greater than zero")]
    InvalidDpiScale { factor: f32 },
}
```

#### 6.2.4 D8 文案要求（测试守护）

1. `Display` 必须同时包含 `os`、`required`、`actual`、`"release notes"`；
2. 不得出现降级承诺：禁止子串 `"software rendering"`、`"falling back"`、`"try again"`；
3. 应用层对 `UnsupportedGpu` 做穷尽 match 生成 zh-CN 文案；安装器与运行时基线清单要求**单一来源**
   （P09 导出 `*_BASELINE` 常量）。

### 6.3 `src/math.rs`

#### 6.3.1 原则

- 只存**逻辑像素 `f32`**；物理像素只出现在 `to_physical_f32` / `to_physical` / `to_physical_covering`（I11）。
- 不做 GPUI 兼容形状（`Pixels`/`Bounds`/`px()`），不做通用变换栈。
- 只放后续 Phase 已确定需要的能力；派生 `Debug, Clone, Copy, Default, PartialEq`（`PhysicalRect` 另加 `Eq`）；
  **不实现** `Hash`/`Ord`（浮点无全序）。

#### 6.3.2 完整签名

```rust
//! 逻辑像素几何与「逻辑 → 物理像素」的唯一换算点（I11）。

/// 逻辑像素坐标点（左上原点，y 向下）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };
    pub const fn new(x: f32, y: f32) -> Self;
}

impl std::ops::Add for Point { type Output = Self; fn add(self, rhs: Self) -> Self; }
impl std::ops::Sub for Point { type Output = Self; fn sub(self, rhs: Self) -> Self; }
impl std::ops::Mul<f32> for Point { type Output = Self; fn mul(self, rhs: f32) -> Self; }

/// 逻辑像素尺寸。允许 0 与负数（负数一律视为空，不做静默归一化）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub const ZERO: Self = Self { width: 0.0, height: 0.0 };
    pub const fn new(width: f32, height: f32) -> Self;
    /// 宽度或高度非有限或 `<= 0` 即空（NaN / ±∞ 也算空，安全方向）。
    pub fn is_empty(self) -> bool;
    pub fn area(self) -> f32;
}

impl std::ops::Mul<f32> for Size { type Output = Self; fn mul(self, rhs: f32) -> Self; }

/// 逻辑像素轴对齐矩形，`origin + size` 表示，半开区间 `[min, max)`。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self;
    pub const fn from_origin_size(origin: Point, size: Size) -> Self;

    pub fn min(self) -> Point;
    pub fn max(self) -> Point;
    /// 同 `Size::is_empty`；另外原点非有限（无法绘制）也判空。空矩形不参与绘制、不参与 damage。
    pub fn is_empty(self) -> bool;
    /// 半开区间判定：`min <= p < max`；空矩形恒 `false`。
    pub fn contains(self, point: Point) -> bool;

    pub fn translate(self, by: Point) -> Self;
    /// 四边内缩；负值即外扩。结果可能为空（不 panic、不 clamp 到 0）。
    pub fn inset(self, edges: Edges) -> Self;
    /// 四边等量外扩（AA / 阴影 damage 用）。
    pub fn outset(self, amount: f32) -> Self;

    /// 并集：任一为空时返回另一个（`∅ ∪ R = R`）；两个不同的空矩形 union 仍为空，
    /// 但不保证返回哪一个的值。
    pub fn union(self, other: Self) -> Self;
    /// 交集；不相交时返回零面积矩形（damage 合并需要无分支路径）。
    pub fn intersect(self, other: Self) -> Self;
    /// 半开区间重叠判定；任一不可换算（非有限或负尺寸）返回 `false`，空矩形恒 `false`。
    pub fn intersects(self, other: Self) -> bool;

    /// 未取整的物理浮点矩形：`x0 = x·s`、`x1 = (x + w)·s`、`w = x1 − x0`（y 同理），
    /// 与整型路径共享「同一逻辑边只有一种表达式」的写法。SDF 图元（Quad/Border/Gradient/Shadow）的
    /// `GpuPrimitive.bounds` 与 `clip_rect` 都由它产出（§7.1）。
    /// 非有限 → `[0.0; 4]`；负尺寸 → `[0.0; 4]`。
    pub fn to_physical_f32(self, scale: Scale) -> [f32; 4];

    /// 逐边取整的物理像素矩形：图集图元（GlyphRun/Image）四边形，1:1 采样无重采样模糊（§7.2）。
    pub fn to_physical(self, scale: Scale) -> PhysicalRect;

    /// 向外 floor/ceil 的物理像素矩形：damage 与 scissor 的安全侧（§7.3）。
    pub fn to_physical_covering(self, scale: Scale) -> PhysicalRect;
}

/// 逻辑像素四边距离（CSS 顺序：上 / 右 / 下 / 左）。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Edges {
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub left: f32,
}

impl Edges {
    pub const ZERO: Self;
    pub const fn new(top: f32, right: f32, bottom: f32, left: f32) -> Self;
    pub const fn uniform(value: f32) -> Self;
}

/// DPI 缩放因子：逻辑像素 × scale = 物理像素。构造即校验（有限且 > 0）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale(f32);

impl Scale {
    pub const ONE: Self = Self(1.0);
    /// `None` 仅当 `factor` 非有限或 `<= 0`；调用方（平台层）必须转成 `EngineError::InvalidDpiScale`，
    /// **不得**静默回退到 1.0（I10）。
    pub fn new(factor: f32) -> Option<Self>;
    pub const fn factor(self) -> f32;
    /// 物理 → 逻辑（输入事件换算）。这是**唯一**的反向换算点（正向只有 `Rect::to_physical*`）。
    pub fn to_logical(self, physical: f32) -> f32;
}

/// 2D 仿射变换（仅 2D）：`x' = m11·x + m21·y + tx`，`y' = m12·x + m22·y + ty`（列向量约定，与 lyon/kurbo 一致）。
/// GPU uniform 契约：打包为**两个 vec4** —— `(m11, m12, m21, m22)` 与 `(tx, ty, 0, 0)`（6 个有效 float + 2 padding），
/// P10 的 `contract.md` 必须镜像该布局。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    m11: f32,
    m12: f32,
    m21: f32,
    m22: f32,
    tx: f32,
    ty: f32,
}

impl Transform {
    pub const IDENTITY: Self;
    pub const fn translate(tx: f32, ty: f32) -> Self;
    pub const fn scale(sx: f32, sy: f32) -> Self;
    /// 复合：先应用 `self`，再应用 `next`（即 `next × self`）。
    pub fn then(self, next: Self) -> Self;
    pub fn apply(self, point: Point) -> Point;
    /// `[m11, m12, m21, m22, tx, ty]`（与 uniform 契约前 6 项同序）。
    pub fn to_array(self) -> [f32; 6];
}

/// 物理像素矩形，半开区间 `[x0, x1) × [y0, y1)`；不变量 `x0 <= x1 && y0 <= y1`。
/// 公开字段意味着不变量由**构造方**保证：库内一律经 [`PhysicalRect::new`] 或 `from_edges` 构造，
/// 外部直接构造时必须自行保证有序（`width`/`height` 在 debug 下断言该不变量）。
/// 同时是 P09 `DamageRect` 的存储类型（DEVIATIONS F-09）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysicalRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl PhysicalRect {
    /// 要求调用方已保证有序；debug 下 `debug_assert!`，release 由调用方不变量保证。
    pub fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Self;
    /// 宽 = `x1.saturating_sub(x0)`（任何输入都不下溢；极端饱和矩形返回 `i32::MAX`）。
    pub fn width(self) -> i32;
    /// 高 = `y1.saturating_sub(y0)`。
    pub fn height(self) -> i32;
    /// 宽或高为 0 即空；饱和矩形（宽/高溢出到 `i32::MAX`）不为空。
    pub fn is_empty(self) -> bool;
    pub fn union(self, other: Self) -> Self;
    /// 不相交时退化为零面积矩形。
    pub fn intersect(self, other: Self) -> Self;
}
```

#### 6.3.3 语义细节

- **非有限值**：`to_physical` / `to_physical_covering` / `to_physical_f32` 在任一输入分量非有限
  （NaN / ±inf，或 `scale` 非法）时返回**空矩形**（整数版 `(0,0,0,0)`，浮点版 `[0.0; 4]`），不 panic。
- **转换顺序**：先 `saturate` 到 i32 安全域（`i32::MIN as f32 ..= i32::MAX as f32`）再 `round`/`floor`/`ceil` 转整型，
  **转换后**再 clamp 不变量（`x1 = x1.max(x0)`），避免 `as` 饱和产生逆序。
- **负尺寸**：`size.width < 0` 或 `size.height < 0` 先归一化为空矩形，再做任何换算（N4）。
- **非有限原点的矩形视为空**：`Rect::is_empty` 在原点非有限时返回 `true`；`Rect::intersect` / `intersects` 对
  不可换算输入（非有限或负尺寸）直接返回空矩形，避免 NaN 参与 `min/max` 产生「看似相交」的结果。
- `Rect::intersect` 不相交时返回零面积矩形，`is_empty()` 为 `true`；判断重叠用 `intersects()`。
- `Rect::union`：任一为空时返回另一个（`∅ ∪ R = R`）；两个不同的空矩形 union 仍为空，但不保证返回哪一个的值
  （空矩形的位置没有几何意义）。
- `Rect::intersects` 对任一不可换算输入（非有限或负尺寸）返回 `false`；`Rect::is_empty` 对非有限原点返回 `true`。
- `Rect::inset` 不 clamp：过度内缩产生负 size，`is_empty()` 为真，由 paint 跳过。
- `PhysicalRect::new` 的 debug 断言是契约检查，不是输入清洗。

---

## 7. 像素换算与 AA 契约（I11）

### 7.1 三种换算语义（职责分离，取代 v1 的二分法）

| 函数 | 输出 | 语义 | 唯一用途 |
|------|------|------|----------|
| `Rect::to_physical_f32` | `[f32; 4]` | `x0 = x·s`、`x1 = (x + w)·s`、`w = x1 − x0`（y 同理），**不取整** | SDF 图元（`Quad` 含圆角/边框/渐变、`Shadow`）的 `GpuPrimitive.bounds`，以及 `clip_rect` |
| `Rect::to_physical` | `PhysicalRect` | **逐边** `round(edge·s)`（.5 远离零） | 图集图元（`GlyphRun`/`Image`）的四边形，保证纹素 1:1 无重采样 |
| `Rect::to_physical_covering` | `PhysicalRect` | `floor(min·s)` / `ceil(max·s)` | **damage** 与 **scissor**（安全侧，宁多勿少） |

### 7.2 AA 契约（128 B `GpuPrimitive` 不变）

v1 曾想用「向外 floor/ceil 的整数 bounds」解决 AA 外扩，那会把 SDF 图元整体平移最多半个像素、并让 `bounds` 与真实
几何不一致。**定案：`bounds` 保存未取整的物理浮点矩形，AA 外扩交给顶点着色器。**

```text
GpuPrimitive.bounds = (x·s, y·s, w·s, h·s)          // 物理像素，浮点，未取整
params0.w           = aa_padding                     // 物理像素 padding
  普通 SDF 图元（Quad/Border/Gradient/Clip mask）：1.0
  Shadow：blur + 1.0
  图集图元（GlyphRun/Image）：0.0（其 bounds 为 to_physical 的整数矩形）

顶点着色器：4 个 unit-quad 顶点按 bounds 定位后，向外扩 aa_padding
片元着色器：用 bounds 的真实矩形求 sd_rounded_rect / 边框 / 渐变；fwidth 解析 AA
            （扩出去的 1px 边缘覆盖率自然为 0）
```

- **不新增字段、不改 128 B 布局**（附录 B 的 `size_of == 128` / `align_of == 16` / `offset_of!` 断言不变）。
- `clip_rect`（`GpuPrimitive` 第 112 B 起）保存**浮点**物理像素 clip 矩形：clip mask 在片元里按距离求值。
- **scissor 用 `to_physical_covering`**：矩形裁剪只在像素粒度成立，宁可多画 1px 也不能漏。
- **damage = `covering(bounds)` ∪ 按物理像素外扩 `params0.w`**：`covering` 先把逻辑包围盒映射到像素网格，
  再按**物理像素**外扩 `aa_padding`（Shadow 的可见范围 = `blur + 1`）。外扩在整型像素域完成，归
  `render/damage`（P09）实现；**不能**用逻辑量 `Rect::outset` 直接套——逻辑外扩在分数缩放下无法保证覆盖整像素。
- `contract.md`（P10）必须写入本节三条 + `params0.w` 取值表；任何变更触发像素快照回归（I8）。

### 7.3 逐边取整（`to_physical`）

```text
s  = scale.factor()
x0 = round_sat(x * s)          y0 = round_sat(y * s)
x1 = round_sat((x + w) * s)    y1 = round_sat((y + h) * s)
若 x1 < x0 则 x1 = x0；y 同理（非有限/负尺寸先返回空矩形）
```

要点：

1. **先加后乘**：唯一写法 `(x + w) * s`；`Rect::max()` 同样按 `origin + size` 计算，保证同一逻辑边只有一种表达式。
2. **共享边重合**：相邻矩形若共享边**逐位相同**，`round` 输入相同 → 输出相同 → 无缝、无重叠。
   这是「相邻图元不发生 1px 缝」的完整保证（**不依赖** taffy 的 `round_layout`，见 §7.5）。
   **该保证只适用于整型路径**（`to_physical` / `to_physical_covering`）；SDF 浮点路径 `to_physical_f32`
   完全不做舍入，其视觉连续性靠 f32 精度 + 片元 AA 保证，不靠整数网格对齐。
3. **f32 累积误差**：若 `A.x + A.w` 与 `B.x` 本身差 ε，那是布局在逻辑空间的缝，换算只会如实映射；修法在 P05 布局，
   不在换算层造像素。
4. **负坐标**：`round` 为「.5 远离零」，`round(-2.5) = -3`。
5. **极窄元素**：`w·s < 0.5` 时两边界取整相同 → 零宽（物理上不可表示）；必须可见的 1px 分隔线由布局给出 ≥1 物理像素。

### 7.4 向外覆盖（`to_physical_covering`）

```text
x0 = floor_sat(x * s)          y0 = floor_sat(y * s)
x1 = ceil_sat((x + w) * s)     y1 = ceil_sat((y + h) * s)
若 x1 < x0 则 x1 = x0
```

**性质（限定条件，N4）**：当 `Rect` 有限且 `width >= 0 && height >= 0` 时，`covering ⊇ to_physical`
（区间包含），且每条边最多外扩 1（`covering.x0 ∈ {round.x0 - 1, round.x0}`，`covering.x1 ∈ {round.x1, round.x1 + 1}`）。
负尺寸/非有限输入按 §6.3.3 先归一化为空矩形，此时三者都为空，性质平凡成立（测试中有反例说明）。

### 7.5 与 taffy 的关系（更正 v1 的错误表述）

本设计（引擎 v1 产品线）曾写「P05 用 `taffy::round_layout` 把布局圆整到物理网格」。**这是错的**：taffy 的 `round_layout` 圆整到
**逻辑整数**（1.0 布局单位），在 1.25/1.5 等分数缩放下会损失亚像素、并让 1px 边框错位。
**本设计不调用 `round_layout`**（引擎 v1 产品同样不调用）；无缝由 §7.3 的逐边取整保证。若 P05 实测出现布局级 0.5px 抖动，再评估
「按物理网格自定义圆整」方案并补记偏差。

### 7.6 舍入表（单测期望值；已用 rustc 以 f32 实算校验）

`round(edge·s)`：

| 逻辑边 `e` | s=1 | s=1.25 | s=1.5 | s=2 | s=3 |
|-----------|-----|--------|-------|-----|-----|
| `0.0` | 0 | 0 | 0 | 0 | 0 |
| `0.5` | 1 | 1 (`0.625`) | 1 (`0.75`) | 1 | 2 (`1.5`) |
| `1.0` | 1 | 1 (`1.25`) | 2 (`1.5`) | 2 | 3 |
| `2.0` | 2 | 3 (`2.5`) | 3 | 4 | 6 |
| `3.0` | 3 | 4 (`3.75`) | 5 (`4.5`) | 6 | 9 |
| `4.0` | 4 | 5 | 6 | 8 | 12 |
| `6.0` | 6 | 8 (`7.5`) | 9 | 12 | 18 |
| `8.0` | 8 | 10 | 12 | 16 | 24 |
| `10.0` | 10 | 13 (`12.5`) | 15 | 20 | 30 |
| `10.5` | 11 | 13 (`13.125`) | 16 (`15.75`) | 21 | 32 (`31.5`) |
| `-2.0` | -2 | -3 (`-2.5`) | -3 | -4 | -6 |
| `-10.5` | -11 | -13 (`-13.125`) | -16 | -21 | -32 |

`Rect(10.5, 20.25, 0.25, 0.25)`（物理亚像素元素）：

| s | `to_physical` | `to_physical_covering` | `to_physical_f32` |
|---|----------------|--------------------------|---------------------|
| 1 | `(11, 20, 11, 21)` 零宽 | `(10, 20, 11, 21)` | `[10.5, 20.25, 0.25, 0.25]` |
| 1.25 | `(13, 25, 13, 26)` 零宽 | `(13, 25, 14, 26)` | `[13.125, 25.3125, 0.3125, 0.3125]` |
| 1.5 | `(16, 30, 16, 31)` 零宽 | `(15, 30, 17, 31)` | `[15.75, 30.375, 0.375, 0.375]` |
| 2 | `(21, 41, 22, 41)` 零高 | `(21, 40, 22, 41)` | `[21.0, 40.5, 0.5, 0.5]` |
| 3 | `(32, 61, 32, 62)` 零宽 | `(31, 60, 33, 62)` | `[31.5, 60.75, 0.75, 0.75]` |

> s=2 那行说明为什么 damage 必须用 `covering`：`round(20.25·2)=41` 而 `floor(20.25·2)=40`，
> 逐边取整得到零高矩形（元素在物理像素上「消失」），`covering` 仍保留 1 像素。

无缝验证（s=1.25，逻辑边界 `[0, 10, 20, 33.5, 100]`）：`×1.25 = [0, 12.5, 25, 41.875, 125]` →
`round = [0, 13, 25, 42, 125]`，相邻区间严格首尾相接。

---

## 8. 单元测试清单

组织：全部内联在被测模块（`#[cfg(test)] mod tests`，AGENTS §5.1；`inline_modules` 不拦，§4.3c）。

### 8.1 `math.rs`

| # | 测试 | 断言要点 |
|---|------|----------|
| 1 | `point_arithmetic` | `Add/Sub/Mul<f32>` 分量语义；`ZERO` 是加法单位元 |
| 2 | `size_area_and_empty` | `area`；`is_empty` 对 0 / 负数 / `NaN` 均为 `true` |
| 3 | `rect_min_max` | `max == origin + size` |
| 4 | `rect_is_empty` | 零宽、零高、负宽、`NaN` → `true` |
| 5 | `rect_contains_half_open` | 左闭右开；空矩形恒 `false` |
| 6 | `rect_translate_inset_outset` | 四边内缩/外扩；负值外扩；`outset(2)` 等价 `inset(uniform(-2))` |
| 7 | `rect_union_identity` | 任一为空返回另一个（`∅ ∪ R = R`）；R ∪ R = R；两个不同空矩形 union 仍空但不保证值相等；含负坐标 |
| 8 | `rect_intersect_disjoint_is_empty` | 不相交 → 空；相交四边正确；`intersects` 与 `!intersect().is_empty()` 一致 |
| 9 | `scale_rejects_non_positive_and_non_finite` | `0.0`/`-1.0`/`NaN`/`INFINITY` → `None`；`1.0/1.25/1.5/2.0/3.0` → `Some` |
| 10 | `scale_to_logical_is_inverse` | `s.to_logical(s.factor()*v) == v`（5 个 scale × 若干 v，浮点容差） |
| 11 | `physical_rounding_table` | §7.6 整表逐项（12 个边界 × 5 个 scale） |
| 12 | `physical_edges_are_contiguous` | §7.6 无缝用例：相邻区间 `prev.x1 == next.x0` |
| 13 | `physical_negative_rounding` | `-2.5 → -3`、`-13.125 → -13` |
| 14 | `physical_covering_superset` | 有限且非负尺寸的若干 Rect × 5 scale：`covering` 包含 `to_physical`，每边外扩 ≤ 1 |
| 15 | `physical_covering_keeps_subpixel_visible` | `(10.5,20.25,0.25,0.25)` s=1：`to_physical` 空、`covering` 非空 |
| 16 | `non_finite_inputs_are_empty` | `NaN`/`INFINITY` 原点或尺寸：三个换算函数都返回空（`[0.0;4]` / `(0,0,0,0)`），不 panic |
| 17 | `negative_size_normalizes_to_empty` | 负宽/负高：`to_physical`、`covering`、`to_physical_f32` 全为空（N4 反例） |
| 18 | `huge_coordinates_saturate` | `1e30` 坐标/尺寸：不 panic、结果有序（`x0 <= x1`）、不出现 `i32::MIN` 反转 |
| 19 | `physical_rect_union_intersect` | 归一化不变量、空集单位元、不相交退化 |
| 20 | `physical_rect_width_height_never_negative` | 零面积矩形 `width()==0` |
| 21 | `transform_identity_is_noop` | `IDENTITY.apply(p) == p` |
| 22 | `transform_translate_scale_apply` | `translate(3,4).apply((1,1)) == (4,5)`；`scale(2,3).apply((1,2)) == (2,6)` |
| 23 | `transform_then_is_apply_then_apply` | `a.then(b).apply(p) == b.apply(a.apply(p))` |
| 24 | `transform_to_array_order` | `[m11,m12,m21,m22,tx,ty]` 与 `apply` 一致（P10 uniform 契约前 6 项） |
| 25 | `physical_rect_saturated_width_height` | `PhysicalRect::new(i32::MIN, i32::MIN, i32::MAX, i32::MAX)`：`width()/height() == i32::MAX`（`saturating_sub` 不下溢），`is_empty()` 为 `false` |
| 26 | `to_physical_f32_positive_values` | `Rect(10.5,20.25,0.25,0.25)` 在 s=1/1.25/1.5 的 `[x0,y0,w,h]` 字面量；另加 `from_origin_size(3.5,7.25,20,11.5)` @1.5 = `[5.25,10.875,30.0,17.25]`（w≠h、原点非零） |
| 27 | `to_physical_f32_overflow_is_empty` | `Rect(-9.191715e37, 0, f32::MAX, 10)`：`x1−x0` 溢出为 `inf` → `[0.0;4]`；整型路径按饱和得到 `(i32::MIN,0,i32::MAX,10)` |
| 28 | `to_physical_covering_subpixel_table` | §7.6 子像素表 5 行精确断言（含 s=1.25 `(13,25,14,26)`、s=1.5 `(15,30,17,31)`、s=3 `(31,60,33,62)`） |
| 29 | `covering_edges_differ_from_round` | `Rect(1.3,0,0.4,1)` @2：min 边 `floor 2 / round 3`、max 边 `ceil 4 / round 3`；`to_physical == (3,0,3,2)`、`covering == (2,0,4,2)`（把 floor/ceil 改成 round 必失败） |
| 30 | `rect_inset_asymmetric_edges` | CSS 顺序 `(top,right,bottom,left)`：`inset(1,2,3,4) == Rect(4,1,4,6)`；等价几何 `inset(2,3,4,1) == Rect(1,2,6,4)` |
| 31 | `rect_outset_exact_values` | `Rect::from_origin_size(0,0,10,10).outset(2) == Rect(-2,-2,14,14)` |
| 32 | `size_is_empty_non_finite` | `Size::new(∞,4)`、`(4,-∞)`、`(NaN,4)` 均空；正常尺寸非空 |

### 8.2 `error.rs`

| # | 测试 | 断言要点 |
|---|------|----------|
| 1 | `engine_error_is_send_sync_static` | `fn assert<T: Send + Sync + 'static>()` |
| 2 | `unsupported_gpu_display_lists_required_and_actual` | 含 `os`/`required`/`actual`/`release notes` |
| 3 | `unsupported_gpu_display_has_no_fallback_promise` | 小写后不含 `"software rendering"`/`"falling back"`/`"try again"` |
| 4 | `device_lost_display_includes_path` | 含 `reason` 与路径 |
| 5 | `wayland_compositor_display_includes_detail` | 含 `detail` |
| 6 | `shader_load_display_includes_target_and_path` | 含 `dxil` 与路径 |
| 7 | `swapchain_display_includes_detail` | 含 `detail` |
| 8 | `invalid_dpi_scale_display_includes_factor` | 含数值文本 |
| 9 | `engine_error_source_is_none` | 全 6 个变体 `source().is_none()` |
| 10 | `engine_error_debug_contains_variant_name` | `format!("{e:?}")` 含 `UnsupportedGpu` |
| 11 | `engine_error_usable_as_boxed_error` | `let _: Box<dyn std::error::Error> = Box::new(e);` |

### 8.3 交由后续 Phase 守护

D8 各平台检测值（P09/P11/P16/P17）、`to_physical_f32` 与实机 AA 的一致性（P18 像素快照）、
布局无缝（P18 L1 断言）。

---

## 9. 验收命令与门禁

```powershell
# 1) 单元测试
cargo test -p kytrix_gui

# 2) clippy（workspace lint 由 [lints] workspace = true 继承）
cargo +nightly clippy -p kytrix_gui --all-targets -- -D warnings

# 3) 格式
cargo +nightly fmt -p kytrix_gui -- --check

# 4) 三平台依赖解析
cargo fetch
cargo metadata --format-version 1 --filter-platform x86_64-pc-windows-msvc   | Out-Null
cargo metadata --format-version 1 --filter-platform aarch64-apple-darwin     | Out-Null
cargo metadata --format-version 1 --filter-platform x86_64-unknown-linux-gnu | Out-Null

# 5) taffy 单版本门禁（B1）：只能有 0.15.0，且全仓没有第二个 ^0.15 选择
cargo tree -p kytrix_gui -e normal | Select-String 'taffy v'
#  期望输出恰好一行 "taffy v0.15.0"；若出现 0.15.1 或多行 → 失败

# 6) 关键依赖版本复核
cargo tree -p kytrix_gui -e normal | Select-String 'accesskit v|parley v|skrifa v|objc2 v|windows v'
```

说明：

- `cargo check -p kytrix_gui --target x86_64-unknown-linux-gnu`（Windows 宿主交叉检查）**不需要**链接期系统库
  （只检查、不链接），但需要两类前提：①目标 sysroot（`rust-std-x86_64-unknown-linux-gnu`）；
  ②`fontique` 传递的 `yeslogic-fontconfig-sys` 的 **build script** 会调用 `pkg-config` 找 `fontconfig`，
  交叉时必须配置 sysroot 的 pkg-config（`PKG_CONFIG` + `PKG_CONFIG_SYSROOT_DIR` / `PKG_CONFIG_PATH`），
  或用 stub 经 `PKG_CONFIG` 指定。Linux CI 原生构建则需要 `libfontconfig-dev` + `libxkbcommon-dev`。
  **缺库/缺 pkg-config 的失败点在第三方 build script，不是引擎代码错误。**
  复验记录（2026-11-12，Windows 宿主）：直接跑报
  `pkg-config has not been configured to support cross-compilation`（`yeslogic-fontconfig-sys-6.0.1/build.rs:8`）；
  以返回 `-lfontconfig` 的 stub `pkg-config` + `PKG_CONFIG_ALLOW_CROSS=1` 复跑，`cargo check` **exit 0**
  （`Checking kytrix_gui` 成功）。
- `cargo build` / 出二进制时才需要 `libxkbcommon`（`#[link(name="xkbcommon")]`，见 §5.4）。
- `cargo machete` 不进仓库工具链，不设门禁。

---

## 10. 风险与未决问题

| # | 风险 | 等级 | 现状与对策 |
|---|------|------|------------|
| R1 | objc2 双版本共存（0.5.3 + 0.6.5），体积增加 | 低 | 已解析验证；无 `links` 冲突；桥接点只有 `platform/macos/a11y.rs` 一处裸指针（§5.3③） |
| R2 | Linux 系统库依赖（`xkbcommon` 链接期、`fontconfig` build script） | 中 | Linux CI 需 `libfontconfig-dev` + `libxkbcommon-dev`；Windows 宿主交叉 `cargo check` 需 sysroot pkg-config 或 stub（§9）；P17 固化镜像（V-04） |
| R3 | P01 一次性声明全量依赖，首次构建多 1–2 分钟 | 低 | 换取仓库内可验证三平台解析；如需缩短 CI 可改为逐 Phase 追加 |
| R4 | taffy 钉 `=0.15.0`，M8 前不能升 0.15.1 | 中 | 公共 API 文件与 0.15.1 逐字节相同，功能无差；M8 移除 gpui 后放开（V-07） |
| R5 | `usvg 0.49` `default-features=false` 下 path-only 解析未验证 | 低 | P15 验证，失败则回退默认 features（V-02） |
| R6 | 错误变体表冻结后若 payload 与实现不符会返工 | 低 | 表为契约；改动必须同步 `DEVIATIONS.md` |
| R7 | 无 `#[non_exhaustive]`，未来外发会有破坏性变更 | 低 | `publish = false`；外发时再按 semver 处理 |
| R8 | `to_physical_f32` 依赖 P10 shader 正确实现 VS 外扩 | 中 | 契约写入 `contract.md`；P10/P18 用像素快照验证 AA 边缘 |
| 未决 1 | `Win32_Graphics_DirectWrite` 是否需要直接声明 | — | **不需要**：fontique 的 `system` feature 已传递引入（§5.3⑥） |
| 未决 2 | accesskit_unix `tokio` feature 与 atspi/zbus 组合 | — | P14 首次构建验证；失败回退默认 `async-io`（V-03） |

---

## 11. 审查响应（Review Response）

| 编号 | 结论 | 处理 | 修改位置 |
|------|------|------|----------|
| **B1** | 采纳 | taffy 冻结 `=0.15.0`；全文档统一；新增 `cargo tree` 单版本门禁；注明 M8 后评估升级。**补充证据**：0.15.0/0.15.1 公共 API 文件 SHA256 逐字节相同（8 个文件 IDENTICAL，14 个内部实现文件有差异），`^0.15.1` + `=0.15.0` 实测无解 | §5.1、§5.3④、§5.4、§9-5、附录 A.1、附录 B；DEVIATIONS S-06 |
| **B2** | 采纳 | 全 crate 不使用 `mod.rs`，改自命名模块根（`xxx.rs` + `xxx/`）；允许 `pub(crate) use`；平台出口 `pub(crate) use windows as backend;` + `pub(crate) use device::Device;`；公开路径表按模块根重写（`text::TextStyle`、`test_support::TestApp` 成立）。**新增自测**：`%TEMP%\kg-modroot-probe` 以同样 lints 跑 `-D warnings` 退出码 0 | §3、§4.1–§4.5、附录 C；DEVIATIONS M-06/M-07 |
| **B3** | 采纳 | 128B 不变；`bounds` = 未取整浮点物理矩形；VS 按 `params0.w` 外扩（普通 1.0 / Shadow `blur+1` / 图集 0）；片元用真实矩形求 SDF；新增 `Rect::to_physical_f32`；`to_physical` 仅图集四边形；`covering` 用于 damage/scissor；clip mask 用浮点 `clip_rect` | §6.3.2、§7.1/§7.2、§7.6 表、附录 A/B；DEVIATIONS F-13 |
| N1 | 采纳 | 删除 `harfrust`/`icu_properties`/`icu_segmenter` 的 workspace 声明；说明「未被引用不参与解析，不能钉住」；P06 需要时按仓库实际版本（icu 受 boa_parser `~2.3` 约束为 2.3.x）追加 | §5.1、§5.3②、附录 B、DEVIATIONS F-19 |
| N2 | 采纳 | 新增 `Scale::to_logical(f32) -> f32`，声明为唯一反向换算点 | §6.3.2、§8.1-10 |
| N3 | 采纳 | 非有限 → 空矩形；先 saturate 到 i32 安全域再转整型；转换后 clamp 不变量；新增 3 组测试 | §6.3.3、§8.1-16/17/18 |
| N4 | 采纳 | `covering ⊇ to_physical` 限定「有限且非负尺寸」；负尺寸先归一化为空；补反例测试 | §7.4、§8.1-17 |
| N5 | 采纳 | 写明 clip mask 用浮点 `clip_rect`、scissor 用 `covering`，不再说「禁止混用 round」 | §7.1、§7.2 |
| N6 | 采纳 | 删除「taffy `round_layout` 对齐物理网格」；明确本设计 / 引擎 v1（产品）不调用，无缝由逐边取整保证 | §7.3、§7.5、DEVIATIONS F-17 |
| N7 | 采纳 | 改为「`Win32_Graphics_DirectWrite` 由 fontique 的 `system` feature 传递引入」（源码证据：fontique Cargo.toml target windows 依赖 `windows` + 该 feature） | §5.2 说明 3、§5.3⑥、§10 未决 1、DEVIATIONS F-18 |
| N8 | 复核后修正 | 隔离 probe 包数按最终清单实测 **238**（`cargo metadata`，清单与命令见附录 C）；审查报告 233 为不同 probe 范围，差 5 个包，不改变解析结论。文档采用可复现的 238 | §0、§5.3、附录 C |
| N9 | 复核后修正 | 内联测试严格匹配 `#[cfg(test)]` + `mod tests` 实测 **277** 处（`mod tests` 字样 282 处）；审查报告 276 为另一口径。**已确认采用 277**（保留严格口径注释） | §4.3c、附录 C |
| N10 | 采纳 | 「单份」限定为「隔离 probe 成立」；补充仓库并行版本清单（accesskit 0.24.1+0.25.2、skrifa 0.40+0.44、harfrust 0.5.2+0.12、objc2 0.5.3+0.6.5） | §5.3②⑥ |
| N11 | 采纳 | 补完整冻结变体表（17 个变体，含几何注册、图集耗尽、资源解码、窗口创建、app/tokio 初始化、a11y 初始化），标注实现 Phase；P01 实现 6 个 | §6.2.2、DEVIATIONS F-15 |
| N12 | 采纳 | 从引擎文件树删除 `resources/svg_build.rs`，注明由 `base_ui` build.rs 承担 | §3、DEVIATIONS F-11 |
| N13 | 采纳 | 文件树补 `tests/snapshot.rs`；bench 改为 `benches/perf.rs`（`[[bench]] harness=false`），不再复用 test target | §3、DEVIATIONS M-05/M-09 |
| N14 | 采纳 | 改为「`cargo check --target linux` 不需要 libxkbcommon；`build`/出二进制需要」 | §5.4、§9、DEVIATIONS V-04 |
| N15 | 采纳 | ①`accesskit_macos 0.27.2` 无 `register_hit_test`，只有 `view_children`/`focus`/`hit_test`；②新增 `platform/windows/dpi.rs`；③`HitTest` → `HitResult`；④像素换算从 `layout/mod.rs` 移入 `math` | §3、§4.5、附录 A.3、DEVIATIONS F-16 |
| N16 | 采纳 | 记录 `clippy.toml` 的 `disallowed-methods` 属应用层 M8 范围；引擎 P08+ 禁用这两个 API，确需阻塞任务时在 M8 同批改写 lint | DEVIATIONS M-08 |
| N17 | 采纳 | `Transform` 仅 2D；uniform 契约为两个 vec4（6 有效 + 2 padding）；删除 `Scale::is_integral`、`Edges::horizontal/vertical`、`Transform::is_identity`（无落点） | §6.3.2、§8.1-24、DEVIATIONS F-14 |

---

## 附录 A：后续阶段 API 事实备忘录

> 全部来自 `E:\.cargo\registry\src\rsproxy.cn-e3de039b2554c837\<crate>-<ver>\` 真实源码；「路径:行号」相对该 crate 的 `src/`。
> **taffy 以 0.15.0 为准**：0.15.0 与 0.15.1 的公共 API 文件逐字节相同（§5.3④），下列签名对两者都成立。

### A.1 taffy 0.15.0（P05 布局）

```rust
// tree/traits.rs:147
pub trait TraversePartialTree {
    type ChildIter<'a>: Iterator<Item = NodeId> where Self: 'a;
    fn child_ids(&self, parent_node_id: NodeId) -> Self::ChildIter<'_>;
    fn child_count(&self, parent_node_id: NodeId) -> usize;
    fn get_child_id(&self, parent_node_id: NodeId, child_index: usize) -> NodeId;
}

// tree/traits.rs:173 —— 无 get_style / get_cache_mut，改为 associated style 类型 + 独立 CacheTree
pub trait LayoutPartialTree: TraversePartialTree {
    type CoreContainerStyle<'a>: CoreStyle<CustomIdent = Self::CustomIdent> where Self: 'a;
    type CustomIdent: CheapCloneStr;
    fn get_core_container_style(&self, node_id: NodeId) -> Self::CoreContainerStyle<'_>;
    fn resolve_calc_value(&self, val: *const (), basis: f32) -> f32 { 0.0 }   // 有默认实现
    fn set_unrounded_layout(&mut self, node_id: NodeId, layout: &Layout);
    fn compute_child_layout(&mut self, node_id: NodeId, inputs: LayoutInput) -> LayoutOutput;
}

// tree/traits.rs:212 —— compute_root_layout 要求
pub trait LayoutContainingBlock: LayoutPartialTree {
    type OofItemStyle<'a>: OofItemStyle<CustomIdent = Self::CustomIdent> where Self: 'a;
    fn get_oof_item_style(&self, node_id: NodeId) -> Self::OofItemStyle<'_>;
    fn clear_hoisted_children(&mut self, node_id: NodeId);
    fn add_hoisted_children(&mut self, node_id: NodeId, hoisted: &[NodeId]);
    fn get_detailed_layout_info(&self, node_id: NodeId) -> &DetailedLayoutInfo<Self::CustomIdent> { … }
}

// tree/traits.rs:260
pub trait CacheTree {
    fn cache_get(&mut self, node_id: NodeId, input: &LayoutInput) -> Option<LayoutOutput>;
    fn cache_store(&mut self, node_id: NodeId, input: &LayoutInput, layout_output: LayoutOutput);
    fn cache_clear(&mut self, node_id: NodeId);
}

// tree/traits.rs:275 / :307 / :328 / :358
pub trait RoundTree: TraverseTree { /* get_unrounded_layout / set_final_layout / is_out_of_flow / hoisted_* */ }
pub trait LayoutFlexboxContainer: LayoutPartialTree { /* FlexboxContainerStyle/FlexboxItemStyle + 2 getter */ }
pub trait LayoutGridContainer: LayoutPartialTree { /* GridContainerStyle/GridItemStyle + set_detailed_grid_info */ }
pub trait LayoutBlockContainer: LayoutPartialTree {
    /* BlockContainerStyle/BlockItemStyle + getter */
    fn compute_block_child_layout(&mut self, node_id: NodeId, inputs: LayoutInput,
                                  block_ctx: Option<&mut BlockContext<'_>>) -> LayoutOutput { … }
}

// compute/mod.rs:77
pub fn compute_root_layout(tree: &mut (impl crate::tree::LayoutContainingBlock + CacheTree),
                           root: NodeId, available_space: Size<AvailableSpace>);
// compute/mod.rs:330
pub fn compute_cached_layout<Tree: CacheTree + ?Sized, ComputeFunction>(
    tree: &mut Tree, node: NodeId, inputs: LayoutInput, compute_uncached: ComputeFunction,
) -> LayoutOutput where ComputeFunction: FnOnce(&mut Tree, NodeId, LayoutInput) -> LayoutOutput;
// compute/flexbox.rs:255 / grid/mod.rs:219 / block.rs:374
pub fn compute_flexbox_layout(tree: &mut impl LayoutFlexboxContainer, node: NodeId, inputs: LayoutInput) -> LayoutOutput;
pub fn compute_grid_layout<Tree: LayoutGridContainer>(tree: &mut Tree, node: NodeId, inputs: LayoutInput) -> LayoutOutput;
pub fn compute_block_layout(tree: &mut impl LayoutBlockContainer, node: NodeId, inputs: LayoutInput,
                            block_ctx: Option<&mut BlockContext<'_>>) -> LayoutOutput;
// compute/leaf.rs:20 —— 4 个参数
pub fn compute_leaf_layout<MeasureFunction>(
    inputs: LayoutInput, style: &impl CoreStyle,
    resolve_calc_value: impl Fn(*const (), f32) -> f32, measure_function: MeasureFunction,
) -> LayoutOutput where MeasureFunction: FnOnce(Size<Option<f32>>, Size<AvailableSpace>) -> Size<f32>;
```

- 数据结构：`LayoutInput`（`tree/layout.rs:125`，含 `known_dimensions` / `known_dimensions_are_definite` / `parent_size` /
  `available_space` / `sizing_mode` / `axis: RequestedAxis` / `run_mode: RunMode` / `vertical_margins_are_collapsible`）、
  `LayoutOutput`（`:472`）、`Layout`（`:579`）、`Cache`（`tree/cache.rs:214`：`get`/`store`/`clear`/`is_empty`）。
  **`Cache` 未实现 `CacheTree`**（只有 `TaffyTree`/`TaffyView` 实现，`tree/taffy_tree.rs:215`），`NodeTree` 需自己转发。
- 样式：`Style<S: CheapCloneStr = DefaultCheapStr>`（`style/mod.rs:709`）；`CheapCloneStr` blanket impl（`:67`）要求
  `AsRef<str> + From<&str> + From<String> + Eq + Clone + Default + Debug + 'static` → **`SmolStr` 可直接作为 `CustomIdent`**。
  `CoreStyle`/`Flexbox*`/`Grid*`/`Block*`/`OofItemStyle` 对 `Style<S>` 与 `&T` 均有实现（`:959`…`:1603`），
  故 `type CoreContainerStyle<'a> = &'a Style;` 成立。
- feature：`default` 含 `taffy_tree`（`slotmap`）与 `float_layout` 等，本引擎全部不启用（§5.3④）。

### A.2 parley 0.12.0（P06 文本）

```rust
// lib.rs:109/136/137/140/143-145
pub use fontique;
pub use context::LayoutContext;  pub use font::FontContext;  pub use layout::Layout;
pub use editing::*;  pub use layout::*;  pub use style::*;

// style/brush.rs:5 —— 标记 trait
pub trait Brush: Clone + PartialEq + Default + core::fmt::Debug {}
impl<T: Clone + PartialEq + Default + core::fmt::Debug> Brush for T {}

// style/mod.rs:227 —— 26 个变体
pub enum StyleProperty<'a, B: Brush> { FontFamily(..), FontSize(f32), FontWidth(..), FontStyle(..),
    FontWeight(..), FontVariations(..), FontFeatures(..), Locale(Option<Language>), Brush(B),
    Underline(bool), UnderlineOffset(Option<f32>), UnderlineSize(Option<f32>), UnderlineBrush(Option<B>),
    Strikethrough(bool), StrikethroughOffset(Option<f32>), StrikethroughSize(Option<f32>),
    StrikethroughBrush(Option<B>), LineHeight(..), VerticalAlign(..), WordSpacing(f32), LetterSpacing(f32),
    WordBreak(..), LineBreak(..), OverflowWrap(..), TextWrapMode(..), WhiteSpaceCollapse(..) }

// style/mod.rs:284 —— 0.12 有 TextStyle（26 个字段）
pub struct TextStyle<'family, 'settings, B: Brush> { pub font_family: FontFamily<'family>, pub font_size: f32,
    /* … */ pub brush: B, pub line_height: LineHeight, pub white_space_collapse: WhiteSpaceCollapse }
impl<B: Brush> Default for TextStyle<'static, 'static, B>;

// context.rs:24 / :96 —— LayoutContext 泛型；第 5 参数是 quantize，不是 is_rtl
pub struct LayoutContext<B: Brush = [u8; 4]>;
impl<B: Brush> LayoutContext<B> {
    pub fn new() -> Self;
    pub fn ranged_builder<'a>(&'a mut self, fcx: &'a mut FontContext, text: &'a str,
                              scale: f32, quantize: bool) -> RangedBuilder<'a, B>;
}

// builder.rs:51/59/71/82/103
impl<'b, B: Brush> RangedBuilder<'b, B> {
    pub fn push_default<'a>(&mut self, property: impl Into<StyleProperty<'a, B>>);
    pub fn push<'a>(&mut self, property: impl Into<StyleProperty<'a, B>>, range: impl RangeBounds<usize>);
    pub fn push_inline_box(&mut self, inline_box: InlineBox);
    pub fn set_base_direction(&mut self, base_direction: BaseDirection);
    pub fn build(self, text: impl AsRef<str>) -> Layout<B>;
}

// inline_box.rs:8
pub struct InlineBox { pub id: u64, pub kind: InlineBoxKind, pub index: usize, pub width: f32,
                       pub height: f32, pub baseline: Option<f32>, pub vertical_align: VerticalAlign }

// layout/layout.rs:177/210/226
impl<B: Brush> Layout<B> {
    pub fn lines(&self) -> impl ExactSizeIterator<Item = Line<'_, B>> + DoubleEndedIterator + Clone + '_;
    pub fn break_all_lines(&mut self, max_advance: Option<f32>);
    pub fn align(&mut self, alignment: Alignment, options: AlignmentOptions);
    pub fn width(&self) -> f32;  pub fn height(&self) -> f32;  pub fn first_baseline(&self) -> Option<f32>;
}

// layout/line.rs —— runs:112、items:142、metrics:26、text_range:35、PositionedLayoutItem:261、
//                  PositionedInlineBox:268、GlyphRun:284、positioned_glyphs:337
impl<'a, B: Brush> Line<'a, B> {
    pub fn items(&self) -> impl Iterator<Item = PositionedLayoutItem<'a, B>> + Clone + '_;
    pub fn metrics(&self) -> &LineMetrics;  pub fn text_range(&self) -> Range<usize>;
}
pub enum PositionedLayoutItem<'a, B: Brush> { GlyphRun(GlyphRun<'a, B>), InlineBox(PositionedInlineBox) }
pub struct PositionedInlineBox { pub x: f32, pub y: f32, pub width: f32, pub height: f32,
                                 pub baseline: Option<f32>, pub id: u64, pub kind: InlineBoxKind }
impl<'a, B: Brush> GlyphRun<'a, B> {
    pub fn baseline(&self) -> f32;  pub fn offset(&self) -> f32;  pub fn advance(&self) -> f32;
    pub fn positioned_glyphs(&'a self) -> impl Iterator<Item = Glyph> + Clone;  // x/y 含 run 偏移与基线
    pub fn style(&self) -> &Style<B>;
}

// layout/alignment.rs:11/38
pub enum Alignment { Start, End, Left, Center, Right, Justify }
pub struct AlignmentOptions { pub align_when_overflowing: bool, pub last_line_alignment: Option<Alignment> }
```

修订：草图第 5 参数应为 `quantize: bool`；方向用 `set_base_direction(BaseDirection::Auto/Ltr/Rtl)`；
`B` 用引擎 `ColorBrush`（标记 trait 自动满足）。

### A.3 accesskit 0.25.2 + adapters（P14）

```rust
// accesskit-0.25.2/src/lib.rs
pub struct NodeId(pub NodeIdContent);                    // :715
pub struct TreeId(pub Uuid);  impl TreeId { pub const ROOT: Self = Self(Uuid::nil()); }  // :745/:747
pub struct TreeInfo { pub root: NodeId, pub toolkit_name: Option<String>, pub toolkit_version: Option<String> } // :3173
pub struct TreeUpdate { pub nodes: Vec<(NodeId, Node)>, pub tree: Option<TreeInfo>,
                        pub tree_id: TreeId, pub focus: NodeId }                          // :3212
pub enum Action { Click, Focus, Blur, Collapse, Expand, CustomAction, Decrement, Increment, … } // :289
pub enum ActionData { CustomAction(i32), Value(Box<str>), NumericValue(f64), ScrollUnit(..),
                      ScrollHint(..), ScrollToPoint(Point), SetScrollOffset(Point), SetTextSelection(..) } // :3309
pub struct ActionRequest { pub action: Action, pub target_tree: TreeId,
                           pub target_node: NodeId, pub data: Option<ActionData> }         // :3332
pub struct Node { /* 私有字段；Node::new(role) + set_children/set_bounds/set_label/set_value/add_action/set_hidden */ } // :1107
pub trait ActivationHandler { fn request_initial_tree(&mut self) -> Option<TreeUpdate>; /* … */ }
```

```rust
// accesskit_windows 0.35.2
impl Adapter {
    pub fn new(hwnd: HWND, is_window_focused: bool,
               action_handler: impl 'static + ActionHandler + Send) -> Self;            // adapter.rs:423
    pub fn update_if_active(&mut self, update_factory: impl FnOnce() -> TreeUpdate)
        -> Option<QueuedEvents>;                                                        // :463
    pub fn handle_wm_getobject<H: ActivationHandler + ?Sized>(
        &mut self, wparam: WPARAM, lparam: LPARAM, activation_handler: &mut H,
    ) -> Option<impl Into<LRESULT> + use<H>>;                                           // :534
}
// subclass.rs:162 —— SubclassingAdapter::new(hwnd, activation_handler, action_handler)，
// 窗口已可见时 panic（"must be created before the window is shown"）

// accesskit_macos 0.27.2
impl Adapter {
    pub unsafe fn new(view: *mut c_void, is_view_focused: bool,
                      action_handler: impl 'static + ActionHandler) -> Self;           // adapter.rs:89
    pub fn update_if_active(&mut self, update_factory: impl FnOnce() -> TreeUpdate)
        -> Option<QueuedEvents>;                                                        // :114
    pub fn update_view_focus_state(&mut self, is_focused: bool) -> Option<QueuedEvents>;// :151
    // N15：**没有 register_hit_test**；命中测试走下列方法（需 ActivationHandler）
    pub fn view_children<H: ActivationHandler + ?Sized>(&mut self, …) -> …;             // :223
    pub fn focus<H: ActivationHandler + ?Sized>(&mut self, …) -> …;                     // :248
    pub fn hit_test<H: ActivationHandler + ?Sized>(&mut self, point: NSPoint, h: &mut H) -> …; // :275
}
pub use objc2_foundation::{NSArray, NSObject, NSPoint};                                  // lib.rs:25

// accesskit_unix 0.24.1
impl Adapter {
    pub fn new(activation_handler: impl 'static + ActivationHandler + Send,
               action_handler: impl 'static + ActionHandler + Send,
               deactivation_handler: impl 'static + DeactivationHandler + Send) -> Self; // adapter.rs:133
    pub fn set_root_window_bounds(&mut self, outer: Rect, inner: Rect);                  // :173
    pub fn update_if_active(&mut self, update_factory: impl FnOnce() -> TreeUpdate);      // :196 —— 无返回值
}
```

与设计文档 §7.6 的差异（P14 必读）：Windows `handle_wm_getobject` 三参数、存在 `SubclassingAdapter`（窗口显示前构造）；
macOS `unsafe` + `*mut c_void`、**无 `register_hit_test`**（N15①）；unix `update_if_active` 不返回 `QueuedEvents`，
事件提升路径与 Win/mac 不同（`QueuedEvents::raise` 只在 Win/mac）。

### A.4 color 0.3.3（P02 颜色）

```rust
// lib.rs:112-123
pub use color::{AlphaColor, HueDirection, OpaqueColor, PremulColor};
pub use dynamic::{DynamicColor, Interpolator, UnpremultipliedInterpolator};
pub use tag::ColorSpaceTag;
pub use rgba8::{PremulRgba8, Rgba8};
pub use parse::{parse_color, parse_color_prefix, ParseError};

// color.rs:78 —— 字段 pub，可直接进 GpuPrimitive.color0/1
#[repr(transparent)]
pub struct PremulColor<CS> { pub components: [f32; 4], pub cs: PhantomData<CS> }
impl<CS: ColorSpace> PremulColor<CS> {
    pub const fn new(components: [f32; 4]) -> Self;                        // :628
    pub fn convert<TargetCS: ColorSpace>(self) -> PremulColor<TargetCS>;   // :647
    pub const fn un_premultiply(self) -> AlphaColor<CS>;                   // :661
}
// AlphaColor<CS>（:410 起）/ OpaqueColor<CS>（:213 起）：new / convert / premultiply / with_alpha / split / to_rgba8

// dynamic.rs
impl DynamicColor {
    pub fn convert(self, cs: ColorSpaceTag) -> Self;                         // :161
    pub fn convert_absolute(self, cs: ColorSpaceTag) -> Self;                // :171
    pub fn clip(self) -> Self;                                               // :283（只 clamp，不做 CSS gamut mapping）
    pub fn scale_chroma(self, scale: f32) -> Self;                           // :264
    pub fn interpolate(self, other: Self, cs: ColorSpaceTag, direction: HueDirection) -> Interpolator; // :365
    pub fn interpolate_unpremultiplied(…) -> UnpremultipliedInterpolator;    // :442
    pub fn map_lightness(self, f: impl FnOnce(f32) -> f32) -> Self;          // :530
    pub fn map_hue(self, f: impl FnOnce(f32) -> f32) -> Self;                // :550
    pub fn to_alpha_color<CS: ColorSpace>(self) -> AlphaColor<CS>;           // :87
}
```

`ColorSpaceTag` 变体：`Srgb, LinearSrgb, DisplayP3, A98Rgb, ProphotoRgb, Rec2020, Aces2065_1, AcesCg, XyzD50, XyzD65,
Lab, Oklab, Lch, Oklch, Hsl, Hwb`。`ColorResolver::resolve` 目标类型 `PremulColor<LinearSrgb>`，
`components` 直接进 `GpuPrimitive.color0`；sRGB gamut mapping 需自建（`clip` 只 clamp L）。

### A.5 windows 0.62.2（P11/P12）

```rust
// Win32/Graphics/Direct3D12/mod.rs
pub unsafe fn D3D12CreateDevice<P0, T>(padapter: P0, minimumfeaturelevel: Direct3D::D3D_FEATURE_LEVEL,
    result__: *mut Option<T>) -> windows_core::Result<()>
where P0: Param<IUnknown>, T: Interface;                                                  // :3
impl ID3D12Device {
    pub unsafe fn CreateCommandQueue<T>(&self, pdesc: *const D3D12_COMMAND_QUEUE_DESC) -> Result<T>;      // :9326
    pub unsafe fn CreateCommandAllocator<T>(&self, r#type: D3D12_COMMAND_LIST_TYPE) -> Result<T>;         // :9333
    pub unsafe fn CreateGraphicsPipelineState<T>(&self, pdesc: *const D3D12_GRAPHICS_PIPELINE_STATE_DESC)
        -> Result<T>;                                                                     // :9341，需 Win32_Graphics_Dxgi_Common
    pub unsafe fn CreateDescriptorHeap<T>(&self, pdescriptorheapdesc: *const D3D12_DESCRIPTOR_HEAP_DESC)
        -> Result<T>;                                                                     // :9367
    pub unsafe fn CreateRootSignature<T>(&self, nodemask: u32, pblobwithrootsignature: &[u8]) -> Result<T>; // :9377
    pub unsafe fn CreateCommittedResource<T>(&self, pheapproperties: *const D3D12_HEAP_PROPERTIES,
        heapflags: D3D12_HEAP_FLAGS, pdesc: *const D3D12_RESOURCE_DESC,
        initialresourcestate: D3D12_RESOURCE_STATES, poptimizedclearvalue: Option<*const D3D12_CLEAR_VALUE>,
        result__: *mut Option<T>) -> Result<()>;                                          // :9441
}
pub unsafe fn D3D12SerializeRootSignature(prootsignature: *const D3D12_ROOT_SIGNATURE_DESC,
    version: D3D_ROOT_SIGNATURE_VERSION, ppblob: *mut Option<Direct3D::ID3DBlob>,
    pperrorblob: Option<*mut Option<Direct3D::ID3DBlob>>) -> Result<()>;                  // :44

// Win32/Graphics/Dxgi/mod.rs
pub unsafe fn CreateDXGIFactory<T>() -> Result<T>;                                        // :4
pub unsafe fn CreateDXGIFactory1<T>() -> Result<T>;                                       // :13
pub unsafe fn CreateDXGIFactory2<T>(flags: DXGI_CREATE_FACTORY_FLAGS) -> Result<T>;       // :22
impl IDXGISwapChain1 {
    pub unsafe fn Present1(&self, syncinterval: u32, presentflags: DXGI_PRESENT,
        ppresentparameters: *const DXGI_PRESENT_PARAMETERS) -> HRESULT;                   // :5460（返回 HRESULT）
}

// Win32/Graphics/Dwm/mod.rs —— MARGINS 在 Win32::UI::Controls（:5760），故需 Win32_UI_Controls
pub unsafe fn DwmExtendFrameIntoClientArea(hwnd: HWND, pmarinset: *const MARGINS) -> Result<()>;   // :34
pub unsafe fn DwmSetWindowAttribute(hwnd: HWND, dwattribute: DWMWINDOWATTRIBUTE,
    pvattribute: *const c_void, cbattribute: u32) -> Result<()>;                                  // :146

// Win32/Graphics/Direct3D/mod.rs
pub struct ID3DBlob;                                                                      // :756
```

### A.6 etagere 0.3.0（P09 图集）

```rust
// src/allocator.rs
pub struct AtlasAllocator;
impl AtlasAllocator {
    pub fn new(size: Size) -> Self;                                  // :114
    pub fn allocate(&mut self, size: Size) -> Option<Allocation>;    // :188
    pub fn deallocate(&mut self, id: AllocId);                       // :394
    pub fn allocated_space(&self) -> i32;                            // :530
}
// src/lib.rs:137
pub struct Allocation { pub id: AllocId, pub rectangle: Rectangle }
```

### A.7 其它（P09 / P17）

- `ash 0.38.0+1.3.281`：features `std` / `loaded`（`Entry::load()`）/ `debug`；`linked` 与 `loaded` 互斥。
  `VK_KHR_wayland_surface` 属 `ash::khr::wayland_surface`；`VK_KHR_incremental_present` 属 `ash::khr::incremental_present`。
- `wayland-client 0.31.15`：features `dlopen` / `libwayland_1_23` / `system`，**默认全关**（纯 Rust 协议实现）。
- `wayland-protocols 0.32.13`：`client` + `staging`（`wp_fractional_scale_v1`、`wp_cursor_shape_v1`）+ `unstable`
  （`zwp_text_input_v3`）。
- `calloop 0.14.5` + `calloop-wayland-source 0.4.1`（`calloop ^0.14`、`wayland-client ^0.31.1`）；
  `calloop::ping` / `LoopSignal::wakeup` 用于跨线程唤醒。
- `xkbcommon 0.9.0`：`#[link(name = "xkbcommon")]`（`src/xkb/ffi.rs:114`），`wayland` feature 启 `memmap2`。
  **`cargo check` 不链接**，`cargo build`/出二进制才需要系统库（N14）。

---

## 附录 B：冻结清单逐条（供 `Cargo.toml` 复核）

| crate | 版本 | features | target | 首个使用 Phase |
|-------|------|----------|--------|----------------|
| `accesskit` | 0.25.2 | — | win/mac/linux | P14 |
| `accesskit_windows` | 0.35.2 | — | windows | P14 |
| `accesskit_macos` | 0.27.2 | — | macos | P14 |
| `accesskit_unix` | 0.24.1 | `tokio`（`default-features=false`） | linux | P14 |
| `ahash` | 0.8.12 | `serde`（既有） | all | P04 |
| `ash` | 0.38.0 | `std`,`loaded`,`debug` | linux | P17 |
| `bitflags` | 2.13.2 | — | all | P03 |
| `bytemuck` | 1.25.2 | `derive` | all | P02/P03 |
| `bytes` | 1.12.1 | —（既有） | all | P03 |
| `calloop` | 0.14.5 | — | linux | P17 |
| `calloop-wayland-source` | 0.4.1 | — | linux | P17 |
| `color` | 0.3.3 | —（既有） | all | P02 |
| `criterion` | 0.8.2 | —（dev，P18 追加） | all | P18 |
| `etagere` | 0.3.0 | — | all | P09 |
| `fontique` | 0.12.0 | 默认（`system`） | all | P06 |
| `image` | 0.25.10 | 既有 formats | all | P15 |
| `log` | 0.4.35 | —（既有） | all | P09 |
| `lyon` | 1.0.19 | — | all | P03 |
| `objc2` | 0.6.5 | `std` | macos | P16 |
| `objc2-app-kit` | 0.3.2 | 见 §5.1 | macos | P16 |
| `objc2-core-foundation` | 0.3.2 | `std`,`alloc`,`CFString` | macos | P16 |
| `objc2-core-graphics` | 0.3.2 | `std`,`alloc`,`CGColor`,`CGColorSpace`,`CGContext`,`CGGeometry` | macos | P16 |
| `objc2-foundation` | 0.3.2 | 见 §5.1 | macos | P16 |
| `objc2-metal` | 0.3.2 | 见 §5.1 | macos | P16 |
| `objc2-quartz-core` | 0.3.2 | `std`,`alloc`,`CAMetalLayer`,`CATransaction`,`CALayer` | macos | P16 |
| `parley` | 0.12.0 | 默认（`system`） | all | P06 |
| `skrifa` | 0.44.0 | — | all | P06 |
| `smallvec` | 1.16.5 | —（既有） | all | P03 |
| `smol_str` | 0.3.7 | —（既有） | all | P01 |
| `swash` | 0.2.10 | — | all | P06 |
| `taffy` | **0.15.0（`=0.15.0`）** | `std`,`flexbox`,`grid`,`block_layout`,`calc`,`content_size`（`default-features=false`） | all | P05 |
| `thiserror` | 2.0.21 | —（既有） | all | P01 |
| `tokio` | 1.54.1 | `full`（既有） | all | P08 |
| `usvg` | 0.49.0 | `default-features=false`（build，`base_ui`） | all | P15 |
| `wayland-client` | 0.31.15 | 默认 | linux | P17 |
| `wayland-protocols` | 0.32.13 | `client`,`staging`,`unstable` | linux | P17 |
| `windows` | 0.62.2 | 26 项（见 §5.2） | windows | P11/P12 |
| `xkbcommon` | 0.9.0 | `wayland`（`default-features=false`） | linux | P17 |

**明确不进依赖图**：`wgpu*` / `naga` / `winit` / `accesskit_winit` / `glyphon` / Vello 系 / `peniko` / `cosmic-text` /
`raw-window-handle` / 任何 `x11*` / `xkbcommon` 的 `x11` feature。`harfrust`、`icu_*` 为 parley 传递依赖，
引擎不直接声明（§5.1 说明）。

---

## 附录 C：probe 记录（复现方式）

| probe | 用途 | 结果 |
|-------|------|------|
| `%TEMP%\kg-final-probe` | 依赖解析（清单 = §5.2 并集，三平台 target 分组） | `cargo fetch` 退出 0；`cargo metadata` **238 个包**；taffy **0.15.0**；accesskit 单一 **0.25.2**；objc2 **0.5.3 + 0.6.5** |
| `%TEMP%\kg-modroot-probe` | 自命名模块根 + `pub(crate) use` 出口 + `super::backend` 引用 | `cargo +nightly clippy --all-targets -- -D warnings` **退出 0** |
| `%TEMP%\kg-lint-probe3` | `pub use` vs `pub(crate) use` | `pub use` 报 `clippy::pub_use`；`pub(crate) use` 不报 |
| `%TEMP%\kg-lint-probe4` | `mod.rs` 内定义 struct/impl | 两条 `definition_in_module_root` |
| `%TEMP%\kg-lint-probe5` | `#[cfg(test)] mod tests` | 通过（`inline_modules` 不拦） |
| `%TEMP%\kg-taffy-dup` | `=0.15.0` 与 `^0.15.1` 共存 | **无解**（`all possible versions conflict`），见 §5.3④ |

统计口径：

- **包数约 233–238**（随 sparse index 漂移；采集日 **2026-11-12** 按最终清单实测 **238**）：
  `cargo metadata --format-version 1` 的 `packages` 长度（工作区成员 1 + 三平台依赖并集），
  清单即 §5.2，未含 dev/build 依赖（`criterion` 未声明、`usvg` 归 `base_ui`）。
- **内联测试 277**：`crates/**/*.rs` 中正则 `#\[cfg\(test\)\]\s*\n\s*mod tests` 的匹配数；同一范围 `mod tests` 字样为 282。
- `cargo info <crate>@<ver>` 本机不可用：全局 `E:\.cargo\config.toml` 用 `rsproxy-sparse` 替换 `crates-io`（sparse 源替换不是
  registry 条目），`--registry rsproxy` 又指向不存在的 git 镜像（404）。故版本信息用 **sparse index 原文 + 实际解析 +
  registry 源码** 三者交叉验证，强于 `cargo info`（后者不验证共同解析）。
