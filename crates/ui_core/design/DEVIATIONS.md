# 设计与实施偏差记录（DEVIATIONS）

> 记录「设计文档 / AGENTS 规则」与「实际可落地实现」之间的全部偏差。
> 每条含：**设计原文 → 偏差 → 原因 → 修订后设计**。新增偏差必须追加，不得改写历史条目。
> 关联文档：`custom-gui-engine-design.md`（v2.0）、`AGENTS.md`、`design/phase-01-foundation.md`。

---

## 一、项目级授权

### A-01 单文件规模上限放宽

| 项                                                                                            | 内容                                                                                                                           |
|-----------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------|
| 原文                                                                                          | `AGENTS.md` §1.3「避免出现包含数千甚至上万行代码的巨型文件。当文件过大时，按功能拆分模块……」                                   |
| 授权                                                                                          | 用户于 **2026-11-12** 明确放宽：**实现阶段允许单个 `.rs` 文件超过 AGENTS §1.3 建议的规模（数千行）**，不要求为了行数强行拆分。 |
| 适用范围                                                                                      | 新引擎 `..` 的全部 Phase。                                                                                                     |
| 仍然有效                                                                                      | **仅放宽行数**。模块职责边界、关注点分离、`lib.rs` 只做模块声明（§5.5）、禁止 `#[path]`、禁止 `pub use`、                      
 测试与被测代码同模块、强类型错误、依赖只在 workspace 根声明等规则**全部不变**。               |
| 实施口径                                                                                      | 文件拆分以「模块职责边界」为准：平台后端（`render/{windows,macos,linux}/`）、渲染器、文本等天然模块仍需分目录；                
 `render/windows/device.rs` 这类单文件允许上千行；**不得**为了行数做无意义切割或增加额外层级。 |

### A-02 单测内联形态的确认（非放宽，是澄清）

> **状态说明（复审追加）**：本节写的「308 处」为当时粗口径统计，已被 §六「复审数字复核记录」取代：
> 严格口径为 **277 处** `#[cfg(test)] mod tests`，其中 **272 处内联在 `.rs` 文件内**、**5 处为文件式**。
> 结论不变（内联形态被 `inline_modules` 允许）。

`AGENTS.md` §5.1 要求「`#[cfg(test)] mod tests` 与被测代码同模块」。经实测（`clippy::inline_modules`，见
`phase-01-foundation.md` §4.3d）该形态**不被** workspace lint 拒绝，仓库现有 308 处同构写法。故无需偏差。

---

## 二、机制性偏差（与 AGENTS / workspace lint 直接冲突）

> **状态说明（2026-11-12 复审追加）**：M-01 / M-02 / M-03 中关于 `mod.rs` 的写法已被 §六 的 **M-06** 取代
> （改为自命名模块根 `xxx.rs` + `xxx/`）。历史条目保留供追溯，**不再作为实现依据**。

### M-01 `#[path]` 被禁止，`render/` 与 `platform/` 的 cfg 分派方式改变

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                                                                |
|------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | 设计文档 §1.2 与附录 A：`#[cfg(target_os = "windows")] #[path = "windows/mod.rs"] mod backend;`，三份 `#[path]` 之后 `pub use backend::{Device, Swapchain, …}`                                                                                                                                                                                                                                                      |
| 偏差       | 不使用 `#[path]`，改为**标准模块 + 私有别名**                                                                                                                                                                                                                                                                                                                                                                       |
| 原因       | `AGENTS.md` §1.3 明确「不使用 `#[path = "..."]` 引入模块，使用 Rust 标准模块布局」；`#[path]` 还会让 `mod backend` 与真实目录脱钩，破坏 `cargo doc` 路径与 IDE 导航                                                                                                                                                                                                                                                 |
| 修订后设计 | 每个平台目录是普通子模块（`render/windows/mod.rs`、`render/macos/mod.rs`、`render/linux/mod.rs`），各自导出**同名具体类型**；`render/mod.rs` 中<br>`#[cfg(target_os = "windows")] mod windows;` …<br>`#[cfg(target_os = "windows")] use windows as backend;` …<br>共享层代码 `use super::backend::{Device, …};`。`platform/` 同构（别名 `native`）。**仍然满足 I1/I14**：编译期单一实现、无 trait、无枚举、无 `dyn` |

### M-02 `pub use` 被 workspace lint 禁止，公共 API 只能按模块路径暴露

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                      |
|------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | 设计文档 §1.2 / 附录 A 的 `pub use backend::{Device, Swapchain, Encoder, Presenter, AtlasTexture, GpuTimerImpl};`；§1.3 的公开 API 白名单按**类型名**列举（隐含可扁平导出）                                                                                                                                                                                               |
| 偏差       | 全仓**不允许**任何 `pub use`；公共 API 的路径 = 定义它的叶子模块路径（如 `ui_core::paint::style::Style`），不提供短路径别名                                                                                                                                                                                                                                               |
| 原因       | workspace 根 `[workspace.lints.clippy] pub_use = "forbid"`；实测 `pub use` 在 `-D warnings` 下报 `error: using pub use`（`help: move the exported item to a public module instead`）。该规则的意图是「类型只在定义它的模块里被看见」，与 AGENTS §1.3 的模块化一致                                                                                                         |
| 修订后设计 | ①`lib.rs` 只有 `pub mod` / `mod`；②每个公开类型只在其叶子文件定义，应用层按模块路径 `use`；③完整路径表见 `phase-01-foundation.md` §4.5（实现与迁移时以此为准）；④`render/` 与 `platform/` 整体为**私有模块**（I9：应用层不得见 `Device`/`HWND`/`NSView`/`wl_surface`）；⑤`pub(crate) use` 虽未被 lint 拦截，但本项目**不使用**（`super::backend` 已足够，避免第二套别名） |

### M-03 `definition_in_module_root` 禁止在 `mod.rs` 定义任何 item，共享层代码必须放叶子文件

| 项         | 内容                                                                                                                                                                                                                                        |
|------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | 设计文档 §1.2：`render/mod.rs` = 「共享契约 + cfg 重导出具体后端类型」；即共享契约的 `struct`/`const` 会写在 `mod.rs`                                                                                                                       |
| 偏差       | `mod.rs` / `lib.rs` **只允许** `mod` 声明与 `#[cfg] use … as …;`；任何 `struct`/`enum`/`trait`/`impl`/`fn`/`const`/`static`/`type`（含 §2.3 的 `const _: () = assert!(…)` 静态断言）必须放叶子文件                                          |
| 原因       | workspace lint `definition_in_module_root = "forbid"`；实测 `mod.rs` 里的 `struct` 与 `impl` 各报一条 `error: definition in module root file`，而 `mod` 与 `use` 不报（`phase-01-foundation.md` §4.3a 有完整错误文本）                      |
| 修订后设计 | `render/mod.rs` 只做 cfg 选择 + 分配子模块；共享数据契约放 `render/{device,frame,damage,atlas,…}.rs`；静态断言放 `paint/instance.rs`（叶子文件）。同理 `node/mod.rs`、`text/mod.rs`、`platform/mod.rs`、`render/{os}/mod.rs` 都只是目录清单 |

### M-04 取消 `src/node/tests.rs` 与 `src/a11y/tests.rs`

| 项         | 内容                                                                                                                                                                                      |
|------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | 设计文档 §1.2 文件树：`node/tests.rs  # 单元测与生产代码同模块`、`a11y/…/tests.rs`                                                                                                        |
| 偏差       | 不创建独立 `tests.rs` 存放单元测试；单测直接内联在**被测算法的那个文件**末尾（`tree.rs` 的 `#[cfg(test)] mod tests`），跨模块共享装置用同目录 `test_support.rs`                           |
| 原因       | AGENTS §5.1 明确「不创建 crate 级别和 module 级别的独立 `tests.rs` 集中存放 unit test」；`node/tests.rs` 会让测试与具体实现文件脱钩，且无法访问其它文件的私有字段                         |
| 修订后设计 | `node/id.rs` / `tree.rs` / `signal.rs` / `dependency.rs` / `dirty.rs` 各自内联 `#[cfg(test)] mod tests`；确实需要共享的建树辅助放 `node/test_support.rs`（AGENTS §5.1 允许）。`a11y` 同理 |

### M-05 `tests/` 目录形态：每个 `.rs` 是独立 target，`golden/` 只能是数据目录

| 项         | 内容                                                                                                                                                                                                                             |
|------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | 设计文档 §1.2：`tests/layout/`、`tests/frame_record/`、`tests/golden/{warp,lavapipe,metal-real}/`、`tests/perf/` 均按目录列出                                                                                                    |
| 偏差       | 顶层 `.rs` 作为 test target（`tests/layout.rs`、`tests/frame_record.rs`、`tests/snapshot.rs`、`tests/perf.rs`），`tests/golden/` 与 `tests/perf/` 只作为**数据/基准输入目录**；`tests/perf` 通过 `[[bench]]` 声明为 bench target |
| 原因       | Cargo 只把 `tests/*.rs` 当作测试目标，子目录里的 `.rs` 不会被自动发现；AGENTS §5.2 也要求「`tests/` 下的每个顶层 `.rs` 文件都是独立的 integration-test target」                                                                  |
| 修订后设计 | 布局断言 → `tests/layout.rs`；CPU 帧记录 → `tests/frame_record.rs`；像素快照 → `tests/snapshot.rs`（读 `tests/golden/<device>/`）；基准 → `tests/perf.rs` + `[[bench]] harness = false`。全部只走公开 API（AGENTS §5.2）         |

---

## 三、与设计文档正文的事实性偏差

> **状态说明（2026-11-12 复审追加）**：F-10（像素换算二分法）已被 §六 的 **F-13** 取代；F-10 中
> 「P05 用 `round_layout` 对齐物理网格」的表述由 **F-17** 更正；F-01 / F-02 / F-03 中出现的 taffy **0.15.1**
> 事实同样适用于 **0.15.0**（公共 API 文件逐字节相同，见 S-06）。

### F-01 taffy 0.15.1 低级 trait 与 §4.1 / §4.2 草图不符

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
|------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §4.1：`impl LayoutPartialTree for NodeTree` 只有 `get_style` / `set_unrounded_layout` / `get_cache_mut` / `compute_child_layout`；§4.2 表格称 measure 是「泛型闭包 `FnMut(LayoutInput, NodeId, Option<&mut NodeContext>, &Style) -> LayoutOutput`」                                                                                                                                                                                                                                                                                                                 |
| 偏差       | 0.15.1 的 `LayoutPartialTree` **没有** `get_style` / `get_cache_mut`：改为 associated style 类型（`CoreContainerStyle<'a>`、`CustomIdent`）+ `get_core_container_style`，缓存走独立 `CacheTree`；flex/grid/block 的样式读取拆到 `LayoutFlexboxContainer` / `LayoutGridContainer` / `LayoutBlockContainer`；绝对定位需要 `LayoutContainingBlock`；`compute_leaf_layout(inputs, style, resolve_calc_value, measure_fn)` 是 **4 个参数**，闭包只收 `(Size<Option<f32>>, Size<AvailableSpace>) -> Size<f32>`；`compute_block_layout` 多一个 `Option<&mut BlockContext>` |
| 原因       | 设计文档的草图基于 taffy 0.13/0.14 的 API 形态；0.15 为「让宿主树的 `Style` 可以是借用的子集结构」重构了 trait                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| 修订后设计 | 精确签名全部记入 `phase-01-foundation.md` 附录 A.1；P05 直接实现真实 trait（`NodeTree` 实现 6 个 trait：`TraversePartialTree`、`LayoutPartialTree`、`LayoutContainingBlock`、`CacheTree`、`RoundTree` 及三个 `Layout*Container`）。`Style` 存储沿用 `taffy::Style<SmolStr>`，`type CoreContainerStyle<'a> = &'a Style;`（taffy 对 `&T` 有 blanket impl）                                                                                                                                                                                                            |

### F-02 taffy 低级 API 不返回 `Result`，`EngineError::Layout(String)` 无生产者

| 项         | 内容                                                                                                                                                                                                                                       |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §4.8 表格：「taffy 计算错误 → `EngineError::Layout(String)`，保留子树快照供复现」                                                                                                                                                          |
| 偏差       | 0.15.1 的 `compute_*` 全部返回 `LayoutOutput`（无 `Result`，只有 debug 断言）；`TaffyError` 只属于 `TaffyTree` 高阶层（本设计不用）。因此 `Layout(String)` 永远不会被构造                                                                  |
| 原因       | 低级 trait 路径把错误处理上移给宿主；引擎不维护 `TaffyTree`（I5）                                                                                                                                                                          |
| 修订后设计 | 删除 `Layout(String)`，改为 `LayoutInvalid { node: NodeId, detail: SmolStr }`（P05 落地），只在引擎自身的**后置校验**（NaN / 负尺寸 / 逆序边界）产出；`InvalidMeasure { node, axis, value }` 保留（§4.8 原意）。两者都不再是字符串泛化错误 |

### F-03 parley 0.12 有 `TextStyle`，且 builder 参数与草图不同

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                      |
|------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §4.5：「**Parley 0.12 无 `TextStyle`**，样式单元是 `StyleProperty<'a, B: Brush>`（26 变体）」；草图 `ranged_builder(&mut font_cx, text, scale, style.is_rtl)`；`b.push(StyleProperty::…, span)`；`push_inline_box(spec)`                                                                                                                                                                                                                  |
| 偏差       | ①0.12 **有** `parley::style::TextStyle<'family, 'settings, B>`（未解析样式结构体，26 个字段）；`StyleProperty` 仍存在且确实是 **26** 个变体；②`ranged_builder` 的第 5 个参数是 **`quantize: bool`**，不是 `is_rtl`；RTL 用 `builder.set_base_direction(BaseDirection::Auto/Ltr/Rtl)`；③`LayoutContext` 是泛型 `LayoutContext<B: Brush = [u8; 4]>`；④`push_inline_box` 只收 `InlineBox`（内部带 `index: usize` 字节偏移），没有 range 参数 |
| 原因       | 0.12 的 API 重构（`parley_engine` 拆分、`quantize` 显式化、方向改为 builder 状态）                                                                                                                                                                                                                                                                                                                                                        |
| 修订后设计 | `text::shape` 采用 `LayoutContext<ColorBrush>` + `ranged_builder(fcx, text, scale, /*quantize=*/false)`；方向用 `set_base_direction`；宽度变化只重跑 `break_all_lines` + `align`。若整段样式足够，可改用 `TextStyle` + `TreeBuilder`（P06 定，二者取一，不并存两套代码路径）                                                                                                                                                              |

### F-04 `Brush` 是标记 trait，`peniko` 不进入依赖图

| 项         | 内容                                                                                                                                                                                                                        |
|------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §4.5 与附录 A 用 `Brush` / `ColorBrush` 并列，且 §9.4 未排除 `peniko`（parley 生态常带 peniko）                                                                                                                             |
| 偏差       | `parley::style::Brush` 是**标记 trait**（`Clone + PartialEq + Default + Debug`，并对一切满足者 blanket impl）；`peniko` 只是 parley 的 dev-dependency。引擎定义 `paint::color::ColorBrush`（包装 `Color`）即可，无需 peniko |
| 原因       | 0.12 把颜色表示彻底下放给调用方                                                                                                                                                                                             |
| 修订后设计 | 依赖清单**不含** `peniko`；公共 API 的 `Brush` 指 `paint::color::ColorBrush`；`Layout<ColorBrush>` 贯穿 shape/光栅/图集                                                                                                     |

### F-05 accesskit adapter 版本不同步、签名与 §7.6 草图不同

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §7.2：「`accesskit 0.25.0`，三 adapter 版本不同步，不得按 0.25.x 猜测」；§7.6 草图：`Adapter::new(hwnd, is_focused, action_handler)`、`adapter.handle_wm_getobject(wparam)`、`Adapter::new(ns_view, …)`、`accesskit_unix::Adapter::new(activation_handler, action_handler, ..)`、`update_if_active(                                                                                                                                                                                                                                                                                              || update) → QueuedEvents::raise()` |
| 偏差       | 实测最新批次：`accesskit 0.25.2`、`accesskit_windows **0.35.2**`、`accesskit_macos **0.27.2**`、`accesskit_unix **0.24.1**`，三者都依赖 `accesskit ^0.25.2`（可并到同一 0.25.2）。签名差异：①Windows `handle_wm_getobject(wparam, lparam, &mut activation_handler)` 三参数；②Windows 另有 `SubclassingAdapter`（窗口首次显示前构造，否则 panic）；③macOS `Adapter::new` 是 **`unsafe fn`，收 `*mut c_void`**；④unix `update_if_active` **不返回** `QueuedEvents`（只有 Win/mac 返回），unix 的事件提升路径不同；⑤macOS adapter 内部固定 objc2 **0.5** 线，与引擎的 0.6 线共存（无 `links` 冲突） |
| 原因       | accesskit 各 adapter 独立发版；macOS adapter 仍锁 objc2 0.5；unix 侧走 AT-SPI 广播模型，没有 per-node queued events                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| 修订后设计 | 版本按上述四条冻结；`platform/*/a11y.rs` 按真实签名接线（`handle_wm_getobject` 三参数、macOS 传 `ns_view.as_ptr().cast()`、unix 单独一条 update 路径）；`A11yBridge` 的平台无关部分只依赖 `accesskit::{TreeUpdate, TreeInfo, TreeId, NodeId, ActionRequest}`，`raise` 抽象为 `platform::native::A11yEvents`（P14 内部类型，不是共享层 trait 分派）                                                                                                                                                                                                                                               |

### F-06 `windows 0.62.2` 需要两个设计文档未列的 feature

| 项         | 内容                                                                                                                                                                                                                                                                    |
|------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §9.4 窗口/事件：「`windows 0.62`（Win32_UI_WindowsAndMessaging / Ime / Dwm）」；§2.2 只提到 Direct3D12/DXGI 模块名                                                                                                                                                      |
| 偏差       | 还需 **`Win32_Graphics_Dxgi_Common`**（`CreateGraphicsPipelineState` 等方法被 `#[cfg(feature = "Win32_Graphics_Dxgi_Common")]` 门控；`DXGI_FORMAT` 也在其中）与 **`Win32_UI_Controls`**（`DwmExtendFrameIntoClientArea` 的参数 `MARGINS` 定义在 `Win32::UI::Controls`） |
| 原因       | windows-rs 按 Windows SDK 头文件分 feature，`MARGINS` 与 `DXGI_FORMAT` 分属不同头                                                                                                                                                                                       |
| 修订后设计 | `..` 的 windows features 采用 `phase-01-foundation.md` §5.2 的 26 项清单（含新增两项）；不需要 `Win32_Graphics_DirectWrite`（D17 统一走 Fontique）                                                                                                                      |

### F-07 错误文案语言：引擎英文 + 结构化字段，应用层本地化

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                                                                           |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §2.9 的 `thiserror` 消息为中文（「GPU 基线不满足：{os} 需要 {required}，实际 {actual}……」），并要求「列出实际检测值与最低要求并指向发布说明；不出现『正在尝试软件渲染』这类降级措辞」                                                                                                                                                                                                                                          |
| 偏差       | 引擎 `Display` 改为**英文诊断文本**，中文用户文案由应用层按变体生成                                                                                                                                                                                                                                                                                                                                                            |
| 原因       | ①§1.3 明确 i18n 留应用层、引擎不持有 locale；②`Display` 的消费方是日志 / CI / 崩溃报告 / 开发者，不是终端用户；③D8 的硬要求是**信息完整性与不降级**，与语言无关；④同一份错误在三种语言环境下的日志应当可直接比对                                                                                                                                                                                                               |
| 修订后设计 | `EngineError` 的 `Display` 为英文完整句（含 `os`/`required`/`actual`/`release notes`，且**不出现** `software rendering` / `falling back` / `try again`，由单测守护）；应用层（`base_ui`/安装器）对 `UnsupportedGpu` 等变体做穷尽 match，用同一组字段生成 zh-CN 文案；安装器与运行时的基线清单要求**单一来源**（P09 导出 `*_BASELINE` 常量）。`DEVIATIONS` 记录该决策，设计文档 §2.9 的中文示例视为「语义示例」，不视为文案要求 |

### F-08 短字符串 `String → SmolStr`，元组变体 `Swapchain(String) → Swapchain { detail: SmolStr }`

| 项         | 内容                                                                                                                                          |
|------------|-----------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §2.9：`DeviceLost { reason: String, … }`、`WaylandCompositor { detail: String }`、`Swapchain(String)`、`UnsupportedGpu { …, actual: String }` |
| 偏差       | 五个短文本字段全部改 `SmolStr`；`Swapchain` 由元组变体改为具名字段变体                                                                        |
| 原因       | AGENTS §2.2「短字符串优先 `SmolStr`」；具名字段让错误在 match 时可读（`EngineError::Swapchain { detail }` 优于 `.0`），也便于未来增字段       |
| 修订后设计 | 见 `phase-01-foundation.md` §6.2.2 的完整签名；`PathBuf` 保持（路径不是短字符串）                                                             |

### F-09 `DamageRect` 不另定义结构，复用 `math::PhysicalRect`

| 项         | 内容                                                                                                                                                                                                                                            |
|------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §2.3 共享数据契约：`DamageRect = { x, y, w, h }` 物理像素整数矩形，定义在 `render/damage.rs`                                                                                                                                                    |
| 偏差       | 整数物理矩形只有一份定义：`ui_core::math::PhysicalRect { x0, y0, x1, y1 }`（半开区间）；`render/damage.rs` 只提供 **damage 语义**（外扩、合并、退化阈值、`Vec<PhysicalRect>` 的构建），不重复定义矩形类型                                       |
| 原因       | I2「一套数据契约、只有一份定义」；两份 `{x,y,w,h}` 只会在换算处产生语义漂移（开/闭区间、负坐标）                                                                                                                                                |
| 修订后设计 | `render/damage` 的公开操作签名使用 `PhysicalRect`（P09 落地）；`contract.md` 仍以「物理像素整数矩形」描述，不绑定字段名。若 P09 发现需要额外不变量（如「damage 非空」），用 `pub struct DamageRect(PhysicalRect)` newtype 在 P09 决定并补记偏差 |

### F-10 像素换算从「统一乘 scale 并 round」细化为「逐边取整」+「向外覆盖」两个函数

| 项         | 内容                                                                                                                                                                                                       |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §4.4：「布局与 `LayoutBox` 全用逻辑像素 `f32`；在 paint 生成 `GpuPrimitive.bounds` 时统一乘 `scale` 并 `round`，AA 外扩按物理像素整数计算」                                                                |
| 偏差       | 提供两个语义不同的换算：`Rect::to_physical`（**逐边** `round`，不透明填充/clip 用）与 `Rect::to_physical_covering`（`floor(min)`/`ceil(max)`，AA / 字形 / damage 用）；并把「先加后乘」写成语义的一部分    |
| 原因       | 若按「origin/size 各自 round」，相邻图元的共享边会各自取整，产生 1px 缝或 1px 重叠（半透明时双次混合）；逐边取整保证同一逻辑坐标映射到同一整数。damage 必须走「向外覆盖」才对安全侧（少覆盖 = 残留脏像素） |
| 修订后设计 | 见 `phase-01-foundation.md` §7（含 s=1/1.25/1.5/2/3 的精确语义表与无缝证明）；P05 追加 `taffy::round_layout` 把最终布局盒圆整到物理网格，P09 的 damage 只用 `covering`                                     |

### F-11 构建期 SVG（`usvg`）归属应用层，不进引擎运行时依赖

| 项         | 内容                                                                                                                                            |
|------------|-------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §1.2 文件树：`resources/svg_build.rs  # SVG 仅构建期` 位于引擎 crate 内                                                                         |
| 偏差       | `usvg` 只作为 **build-dependency** 出现在资源所在的 `base_ui`（P15/P19），引擎 crate 不声明 `usvg`；引擎只提供「已解码字节 → 图集」的机制       |
| 原因       | §1.3 明确「资源文件在应用层」「引擎只提供资源字节 → GPU 图集的机制」；54 个 Phosphor SVG 属于 `base_ui` 资产，转换脚本应与其资产同处一地        |
| 修订后设计 | workspace 根声明 `usvg` 版本（冻结），由 `base_ui` 的 `build.rs` 使用；`..` 不声明。若 P15 确需引擎侧预处理，再改为引擎 build-dependency 并补记 |

### F-12 `accesskit_unix` 使用 `tokio` feature

| 项         | 内容                                                                                                                                                                        |
|------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §7.2：「（`accesskit_unix`）内部 spawn 异步任务，tokio 需开 `tokio` feature」——方向正确但未定 feature 组合                                                                  |
| 偏差       | 声明为 `accesskit_unix = { version = "0.24.1", default-features = false, features = ["tokio"] }`，关闭默认 `async-io`                                                       |
| 原因       | D21 全仓单一 tokio runtime；`accesskit_unix` 的默认 `async-io` 会再拉 `async-executor`/`async-task`（设计文档 §9.2 允许库内 executor，但既然官方提供 tokio 集成就优先选它） |
| 修订后设计 | 如 P14 实测 `tokio` feature 与 `atspi 0.29`/`zbus 5.19` 组合有问题，回退默认 `async-io` 并补记（库内 executor 不违反 D21）                                                  |

---

## 四、设计补充决策（不与设计文档冲突，但需固化）

### S-01 `EngineError` 变体分批落地，且不加 `#[non_exhaustive]`

- **决策**：P01 落地 6 个变体（D8 五类 + `InvalidDpiScale`）；`StaleNode` / `CyclicDerived` / `InvalidMeasure` /
  `LayoutInvalid` / `TextShape` 在各自 Phase 追加。整份「变体名 + payload」表在 P01 冻结为契约。
- **理由**：错误**分类法**必须一次定死（否则各 Phase 各造同义变体）；但 payload 不能猜（依赖 `NodeId`/`SignalId`/`Axis`）。
  不加 `#[non_exhaustive]`：`publish = false` 且应用层同仓演进，穷尽 match 比向后兼容更有价值。

### S-02 Phase 01 一次性在 `ui_core` 声明全部 target 依赖

- **决策**：P01 的 `Cargo.toml` 写入 §5.2 的全部 normal/target 依赖（虽然只 `use` 到 `thiserror`/`smol_str`）；
  `[dev-dependencies] criterion` 留到 P18，`[build-dependencies] usvg` 留到 P15 且归 `base_ui`。
- **理由**：依赖冻结阶段的验收要求「在仓库内用 `cargo metadata --filter-platform` 证明三平台可解析」；
  未使用依赖不会触发任何 lint，代价只是首次构建时间（`phase-01-foundation.md` R3 已记录缓解方案）。

### S-03 taffy 关闭 `default` features

- **决策**：`default-features = false`，只开 `std / flexbox / grid / block_layout / calc / content_size`；
  不启用 `taffy_tree`（不拉 `slotmap`，I5 要求自建树）、`parse`（不拉 `cssparser`）、`detailed_layout_info`（0.15.1 中无门控、
  无作用）、`float_layout` / `flexbox_balance`（本设计不用）。
- **理由**：AGENTS §1.2「不添加当前没有实际需求的配置」；`grid`/`default` 只出现在 sparse index 的 `features2` 中，
  不能按 `features` 字段判断（`phase-01-foundation.md` §5.3④）。

### S-04 `etagere` 选 0.3.0（而非仓库锁文件里的 0.2.15）

- **决策**：workspace 声明 `etagere = "0.3.0"`。
- **理由**：0.3.0 是最新稳定版，`AtlasAllocator` 的 4 个入口与 0.2 同形；0.2.15 是 `gpui` 生态的遗留锁定，
  随 M8 移除 `gpui-*` 后自然消失（`phase-01-foundation.md` §5.4）。

### S-05 集成测试从 P05 开始，P01 只有内联单测

- **决策**：P01 不建 `tests/`；`tests/layout.rs` 在 P05、`tests/frame_record.rs` 在 P07、`tests/snapshot.rs` /
  `tests/perf.rs` 在 P18 创建。
- **理由**：P01 的公开 API 只有 `error`/`math`，集成测试没有跨模块协作可测（AGENTS §5.2 的适用条件不成立）；
  内联单测已覆盖全部语义。

---

## 五、待复核项（不阻塞 P01）

| #    | 项                                                                                                                                                                                  | 复核 Phase | 触发条件                                                                                                                                                                                                                                                                                      |
|------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| V-01 | objc2 双版本（0.6.5 + 0.5.3）在真机的运行时行为与二进制体积                                                                                                                         | P16        | macOS 实机 UIA/NSAccessibility 联调；若 accesskit_macos 升级到 objc2 0.6 线则删除 0.2 线                                                                                                                                                                                                      |
| V-02 | `usvg 0.49` 在 `default-features = false` 下解析 path-only SVG                                                                                                                      | P15        | 若 `Tree::from_str` 依赖被裁掉的 feature，则回退默认 features                                                                                                                                                                                                                                 |
| V-03 | `accesskit_unix` 的 `tokio` feature 与 `atspi`/`zbus` 组合                                                                                                                          | P14        | Linux 首次构建 AT-SPI adapter                                                                                                                                                                                                                                                                 |
| V-04 | Linux 系统库：`xkbcommon 0.9` 链接期需 `libxkbcommon`（无 build.rs，`#[link]` 直链）；`fontique` 传递的 `yeslogic-fontconfig-sys` 在 build script 中调 `pkg-config` 找 `fontconfig` | P17        | Linux CI 固化 `libxkbcommon-dev` + `libfontconfig-dev`；Windows 宿主交叉 `cargo check` 需 sysroot pkg-config，或用 stub `pkg-config`（返回 `-lfontconfig`）+ `PKG_CONFIG_ALLOW_CROSS=1`：2026-11-12 复验 **exit 0**（`Checking ui_core` 成功）；缺库失败点在第三方 build script，不是引擎代码 |
| V-05 | 是否需要 `Win32_Graphics_DirectWrite` 做系统度量对齐                                                                                                                                | P06        | Fontique 度量与平台原生出现可见偏差时（§2.2「可选」项）                                                                                                                                                                                                                                       |
| V-06 | `windows` 26 项 feature 是否全部必要（可再收窄）                                                                                                                                    | P11/P12    | 平台层落地后按实际 `use` 反查，删除未用 feature                                                                                                                                                                                                                                               |

---

## 六、审查响应追加条目（2026-11-12 复审）

2026-11-12 对抗审查对 v1 设计给出 **FAIL**（3 条阻塞 B1–B3 + N1–N17）。本节为复审后的**新条目**，
其中 M-06 / M-07 / M-09 / F-13 / F-18 / F-19 / S-06 明确取代历史条目中的过期结论（历史条目文本不改写）。
逐条响应表见 `phase-01-foundation.md` §11。

### M-06 全 crate 不使用 `mod.rs`，改自命名模块根（取代 M-01 / M-02 / M-03 中关于 `mod.rs` 的写法）

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
|------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 前版设计   | `render/mod.rs` 做 cfg 选择、共享契约放 `mod.rs`；平台目录用 `render/windows/mod.rs`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| 偏差       | 删除全部 `mod.rs`。含子模块的模块改用**自命名模块根**：`render.rs` + `render/`、`text.rs` + `text/`、`platform/windows.rs` + `platform/windows/`。模块根文件可定义公共类型与 `pub(crate) use`                                                                                                                                                                                                                                                                                                                                                                        |
| 原因       | clippy `definition_in_module_root` 只针对 `mod.rs` / `lib.rs` / `main.rs`：`mod.rs` 内定义 struct/impl 会失败，而自命名根不会（实测 `%TEMP%\kg-modroot-probe`：`math.rs` 定义 `pub struct Point` + `impl`、`render/windows.rs` 写 `pub(crate) mod device; pub(crate) use device::Device;`、`render.rs` 写 `pub(crate) use windows as backend;`、`render/shared.rs` 写 `use super::backend::Device;`，`cargo +nightly clippy --all-targets -- -D warnings` **退出 0**）。该布局还让公共类型能留在设计文档指定的模块根，避免 v1 的「所有公开类型都在深层叶子文件」问题 |
| 修订后设计 | ①`lib.rs` 仍只有 `mod`/`pub mod`；②平台出口：`render.rs` 内 `#[cfg] pub(crate) mod windows;` + `#[cfg] pub(crate) use windows as backend;`，`render/windows.rs` 内 `pub(crate) mod device;` + `pub(crate) use device::Device;`，共享层 `use super::backend::Device;`；③`platform/` 同构（别名 `native`）；④`loop.rs` 改名 `event_loop.rs`（关键字）；⑤文件树见 `phase-01-foundation.md` §3                                                                                                                                                                           |

### M-07 内部出口允许 `pub(crate) use`，公共 API 仍禁止 `pub use`（取代 M-02 第 ④ 点）

| 项         | 内容                                                                                                                                                                                  |
|------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 前版设计   | 「`pub(crate) use` 虽未被 lint 拦截，但本项目**不使用**」                                                                                                                             |
| 偏差       | 内部出口**使用** `pub(crate) use`（平台类型出口、模块内类型出口）；公共 API 依然只能靠模块路径，不允许任何 `pub use`                                                                  |
| 原因       | 平台出口必须有一个 crate 内可见的名字，否则共享层无法在不带平台名的前提下引用具体类型；`clippy::pub_use` 只拦截 `pub use`（实测 `pub(crate) use` 不报），这是 lint 允许的唯一出口手段 |
| 修订后设计 | 规则改为：**公共 API 禁止 `pub use`；crate 内部出口允许 `pub(crate) use`**。公共路径表见 `phase-01-foundation.md` §4.5（`text::TextStyle`、`test_support::TestApp` 等模块根路径成立） |

### M-08 `clippy.toml` 的 `disallowed-methods` 属应用层 M8 范围（N16）

| 项         | 内容                                                                                                                                                                                                                                              |
|------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 设计文档 §9.2：`clippy.toml` 的 `disallowed-methods`（禁 `spawn_blocking` / `Handle::current`）改为一条新约定                                                                                                                                     |
| 偏差       | 本次引擎实现**不修改** `clippy.toml`（它当前指向即将被删除的 `widget_tokio_runtime`，属应用层 M8 清理范围）                                                                                                                                       |
| 原因       | 引擎 crate 在 P08 之前不引入 tokio 使用点；提前改根 `clippy.toml` 会影响现有 30+ crate 的构建（它们仍依赖 `widget_tokio_runtime`），违反「不做兼容层 / 不提前改无关代码」                                                                         |
| 修订后设计 | 引擎 P08+ **禁止**使用 `tokio::task::spawn_blocking` 与 `tokio::runtime::Handle::current`（主线程不在 runtime 上下文，直接 panic）；确需阻塞任务时，在 **M8** 与删除 `widget_tokio_runtime` 同批改写 `clippy.toml` 的 `disallowed-methods` 并补记 |

### M-09 `tests/` 与 `benches/` 的 target 形态（取代 M-05 中 perf 部分，N13）

| 项         | 内容                                                                                                                                                                                         |
|------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 前版设计   | `tests/perf/` 目录 + 「`tests/perf.rs` + `[[bench]]`」二义表述                                                                                                                               |
| 偏差       | 集成测试：`tests/layout.rs`（P05）、`tests/frame_record.rs`（P07）、**`tests/snapshot.rs`（P18）**；基准：**`benches/perf.rs`**（`[[bench]] harness = false`）；`tests/golden/` 仅为数据目录 |
| 原因       | Cargo 不会把 `tests/` 子目录里的 `.rs` 当 target；同一文件不能既当 test target 又当 bench harness（`harness=false` 时 `#[test]` 不生效）                                                     |
| 修订后设计 | 见 `phase-01-foundation.md` §3；`cargo bench -p ui_core --bench perf` 为 P18 的 L4 入口                                                                                                      |

### F-13 AA 契约：128 B 不变 + 顶点着色器外扩 + 三种换算语义（取代 F-10 的二分法，B3）

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                                                    |
|------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 前版设计   | F-10：只有 `to_physical`（逐边 round）与 `to_physical_covering`（floor/ceil）两种；SDF 图元用哪种未定，曾隐含「AA 用 covering 的整数 bounds」                                                                                                                                                                                                                                                           |
| 偏差       | 三种换算：`to_physical_f32`（未取整浮点，SDF 图元 `bounds`）、`to_physical`（逐边 round，图集图元四边形）、`to_physical_covering`（floor/ceil，damage 与 scissor）。AA 外扩**不进 bounds**，由顶点着色器按 `params0.w = aa_padding` 外扩（普通 1.0 / Shadow `blur+1.0` / 图集 0）；片元用 `bounds` 的真实矩形求 SDF；`GpuPrimitive` 保持 128 B、零新增字段                                              |
| 原因       | 用整数 `covering` bounds 会把 SDF 整体平移最多半像素、使 bounds 与真实几何不一致（动画闪烁、圆角/边框偏移）；而未取整 bounds + VS 外扩既保留亚像素精度，又保证 AA 边缘不被裁掉，且不破坏 §2.3 的 128 B 字节契约                                                                                                                                                                                         |
| 修订后设计 | `phase-01-foundation.md` §7.1/§7.2；P10 的 `render/shaders/contract.md` 必须写入「bounds 浮点 + `params0.w` 外扩表 + clip mask 用浮点 `clip_rect` + scissor 用 covering」；P18 用像素快照验证 AA 边缘。**字段语义变更**：主设计文档 §2.3/附录 B 把 `params0.w` 定义为 `aa_scale`（AA 缩放系数），本设计改称 **`aa_padding`（物理像素外扩量）**；主设计文档未改，P10 以 `contract.md` 为准，禁止两处混用 |

### F-14 `Transform` 的 GPU uniform 契约（N17）

| 项         | 内容                                                                                                                                  |
|------------|---------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 设计文档 §附录 A 只给 `Transform` 类型名，无 uniform 布局                                                                             |
| 偏差       | 冻结为「2D 仿射 + 两个 `vec4`」：`(m11, m12, m21, m22)` 与 `(tx, ty, 0, 0)`，6 个有效 float + 2 padding；`to_array()` 前 6 项与之一致 |
| 原因       | `Geometry` 每帧只更新 uniform（§5.5），布局必须在 P01 定死，否则 P09/P10 会各写一套；两个 `vec4` 满足 16 B 对齐且无需矩阵库           |
| 修订后设计 | `phase-01-foundation.md` §6.3.2；P10 `contract.md` 镜像该布局；`Transform` 仍只支持 2D（无 3D / 无投影）                              |

### F-15 `EngineError` 完整冻结变体表（N11）

| 项         | 内容                                                                                                                                                                                                                                                                                                                                    |
|------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 设计文档 §2.9 只给 D8 五类；§4.8 给 `InvalidMeasure` / `Layout(String)`                                                                                                                                                                                                                                                                 |
| 偏差       | 冻结 **17 个变体**（名称 + payload + 实现 Phase）：D8 五类 + `InvalidDpiScale` 在 P01；`StaleNode`/`CyclicDerived`（P04）、`InvalidMeasure`/`LayoutInvalid`（P05）、`GeometryRegister`（P03）、`AtlasExhausted`（P09）、`TextShape`（P06）、`AppInit`（P08）、`WindowCreate`（P12）、`ResourceDecode`（P15）、`A11yInit`（P14）后续追加 |
| 原因       | 变体名与 payload 是公开契约，必须一次定死（否则各 Phase 造同义变体）；但 payload 依赖尚未定义的类型（`NodeId`/`AtlasKind`/`ResourceId`），P01 只能实现不依赖它们的 6 个                                                                                                                                                                 |
| 修订后设计 | 表见 `phase-01-foundation.md` §6.2.2；`EngineError` 不加 `#[non_exhaustive]`（`publish=false`，穷尽 match 优先）                                                                                                                                                                                                                        |

### F-16 N15 四项（accesskit 方法集、`dpi.rs`、`HitTest`→`HitResult`、像素换算归属）

| # | 设计原文                                                    | 偏差                                                                                                                                 | 原因                                                                                                                   | 修订后设计                                                                                                                  |
|---|-------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------|
| ① | §7.6 `adapter.register_hit_test(                            |                                                                                                                                      | engine.hit_test(...))`                                                                                                 | `accesskit_macos 0.27.2` **没有** `register_hit_test`；只有 `view_children` / `focus` / `hit_test(point: NSPoint, handler)` | 0.27 把命中测试改成引擎在适配器回调里主动调用，而非注册闭包 | P16 在 `platform/macos/a11y.rs` 里实现 `hit_test` 回调路径，并把引擎 `HitResult` 转成 `accesskit` 节点；不存在的 API 不得写进实现清单 |
| ② | §8.1 平台目录 `windows/{loop,window,ime,clipboard,a11y}.rs` | 增加 `platform/windows/dpi.rs`                                                                                                       | DPI 事件（`WM_DPICHANGED` / `WM_GETDPISCALEDSIZE`）与窗口创建强耦合但职责独立；`Scale::new` 失败要落 `InvalidDpiScale` | 目录见 `phase-01-foundation.md` §3；`dpi.rs` 是 `EngineError::InvalidDpiScale` 的唯一生产者                                 |
| ③ | §1.2/§1.3 用 `HitTest` 指代命中结果类型                     | 统一为 **`HitResult`**（`event::HitResult`）；`hit_test` 保留为函数名                                                                | 设计文档两处命名不一致（§1.2 `HitTest` 与 §1.3 `HitResult`），类型名必须是名词                                         | `event::HitResult { node, local_point, clip_chain }`；`hit_test(Point) -> Option<HitResult>`                                |
| ④ | §1.2 `layout/mod.rs # 逻辑/物理像素换算`                    | 像素换算全部移入 `math`（`to_physical_f32` / `to_physical` / `to_physical_covering` / `Scale::to_logical`）；`layout` 只管布局与命中 | 换算被 paint/render/platform 三处消费，放 `layout` 会造成反向依赖（`paint` → `layout` 仅为换算）                       | `math` 是唯一换算点；`layout` 不再导出换算 API                                                                              |

### F-17 v1 **不调用** taffy `round_layout`（更正 F-10 中的错误表述，N6）

| 项         | 内容                                                                                                                                                                                                                                     |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 前版设计   | F-10 末尾写「P05 追加 `taffy::round_layout` 把最终布局盒圆整到物理网格」                                                                                                                                                                 |
| 偏差       | **删除该表述**。本设计不调用 `round_layout`（引擎 v1 产品同样不调用）；布局盒保持逻辑 `f32`，无缝由 `Rect::to_physical` 的逐边取整保证                                                                                                   |
| 原因       | taffy 的 `round_layout`（`compute/mod.rs` `round_layout(tree: &mut impl RoundTree, node_id)`）圆整到 **1.0 逻辑单位**，在 1.25/1.5 等分数缩放下会损失亚像素、使 1px 边框错位；它解决的是「整数缩放下的布局盒对齐」，不是「物理网格对齐」 |
| 修订后设计 | `phase-01-foundation.md` §7.5；若 P05 实测出现布局级 0.5 px 抖动，再评估「按物理网格自定义圆整」并补记偏差（新 V 项）                                                                                                                    |

### F-18 `Win32_Graphics_DirectWrite` 由 fontique 的 `system` feature 传递引入（N7，取代 V-05 的疑问）

| 项         | 内容                                                                                                                                                                                                                                                                                                                           |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | §2.2 把 DirectWrite 列为「可选」；V-05 待定是否声明 `Win32_Graphics_DirectWrite`                                                                                                                                                                                                                                               |
| 偏差       | 引擎**不声明**该 feature：`fontique 0.12.0` 的 `system` feature（`default` 即含）在 Windows 上依赖 `windows 0.62.2` + `features = ["Win32_Graphics_DirectWrite"]`（optional、target windows；源码 `fontique-0.12.0/Cargo.toml` 的 `[target.'cfg(target_os = "windows")'.dependencies.windows]`），Cargo feature 统一后自动可用 |
| 原因       | 声明会与传递 feature 重复，且让引擎 manifest 误以为直接使用 DirectWrite；D17 的字体枚举由 Fontique 负责                                                                                                                                                                                                                        |
| 修订后设计 | `phase-01-foundation.md` §5.2 说明 3 / §5.3⑥；V-05 关闭（不再需要决策）                                                                                                                                                                                                                                                        |

### F-19 `harfrust` / `icu_*` 不作 workspace 声明（N1）

| 项         | 内容                                                                                                                                                                               |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 前版设计   | v1 把 `harfrust` / `icu_properties` / `icu_segmenter` 写进 `[workspace.dependencies]` 并称「传递钉住」                                                                             |
| 偏差       | **删除这三条**。`[workspace.dependencies]` 中未被任何成员引用的条目不参与解析，写了既不能钉版本也不能验证兼容                                                                      |
| 原因       | 三者都是 `parley_engine 0.12` 的传递依赖；引擎没有直接 `use`，声明属「无实际需求的配置」（AGENTS §1.2）。且仓库 `boa_parser 0.22` 约束 icu `~2.3`，引擎若写 `2.4.0` 会制造版本冲突 |
| 修订后设计 | P06 若确需直接 `use`，再按**仓库实际解析版本**追加（icu 预计 2.3.x），并同步附录 B 与本文档；在此之前不在清单中                                                                    |

### F-20 `Rect::is_empty` / `Rect::intersect` 对非有限输入的补充契约（P01 实现阶段）

| 项          | 内容                                                                                                                                                                                                                                                     |
|-------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文 / 前版 | §6.3.2 只写「`is_empty` 同 `Size::is_empty`」，未定义原点非有限的情形；`intersect` 未定义非有限输入                                                                                                                                                      |
| 偏差        | `Rect::is_empty` 增加「原点非有限 → `true`」；`Rect::intersect`（以及依赖它的 `intersects`）对任一不可换算输入（非有限或负尺寸）直接返回空矩形                                                                                                           |
| 原因        | NaN / ±∞ 只可能来自平台 bug 或上游坏数据，但必须**安全失败**：`f32::min/max` 会忽略 NaN 操作数，若不拦截，`intersect` 可能返回「看似相交且非空」的矩形，让命中测试与 damage 误判。换算路径本已由 `is_convertible` 保护，此条目把同一判定提升到几何谓词层 |
| 修订后设计  | `phase-01-foundation.md` §6.3.2 / §6.3.3；单测 `rect_is_empty`（NaN 原点）与 `rect_intersect_disjoint_is_empty` 覆盖                                                                                                                                     |

### F-21 数值安全的两处补充：`Size::is_empty` 措辞与 `to_physical_f32` 差值溢出（P01 复审修复）

| 项          | 内容                                                                                                                                                                                                         |
|-------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文 / 前版 | §6.3.2 写 `Size::is_empty` 的公式为 `!(width > 0.0 && height > 0.0)`；`to_physical_f32` 只校验输入端点有限，未校验 `x1 − x0` / `y1 − y0`                                                                     |
| 偏差        | ①判空语义统一表述为「**非有限或 `<= 0` 即空**」（NaN / ±∞ 都算空），与 §6.3.3 / F-20 一致；②`to_physical_f32` 在端点有限之外，再校验差值有限，溢出的 `±inf` 一律返回 `[0.0; 4]`                              |
| 原因        | ①原公式对 `±∞` 的表述不直观（尽管 `∞ > 0` 为真时靠乘积仍可能得空），文档与实现必须逐字一致；②两个有限端点相减仍可溢出（如 `-9.191715e37` 到 `f32::MAX` → `+inf`），若直接返回该 `w` 会污染 damage / SDF 外扩 |
| 修订后设计  | `phase-01-foundation.md` §6.3.2 / §8.1 测试 26–28、32；实现见 `math.rs::to_physical_f32` 与 `Size::is_empty`                                                                                                 |

### F-22 `Edges::new` 参数顺序为 CSS 顺序（与复审用例字面值的差异说明）

| 项         | 内容                                                                                                                                                                 |
|------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 设计 §6.3.2 明确 `Edges { top, right, bottom, left }`（CSS `padding: T R B L`），`Edges::new(top, right, bottom, left)`                                              |
| 偏差       | 复审给出的用例 `inset(Edges::new(1,2,3,4)) == Rect::new(1,2,6,4)` 若按 CSS 顺序实际得到 `Rect::new(4,1,4,6)`；`(1,2,6,4)` 对应 `left=1, top=2, right=3, bottom=4`    |
| 原因       | 复用 CSS 顺序可避免与 `base_ui` 既有 padding 语义错位；顺序本身是契约，不能为了单个用例反转                                                                          |
| 修订后设计 | 保留 CSS 顺序；单测 `rect_inset_asymmetric_edges` 同时断言两种几何（`Edges::new(1,2,3,4) → (4,1,4,6)` 与 `Edges::new(2,3,4,1) → (1,2,6,4)`），并在注释中写明顺序映射 |

### S-07 `Transform` 的 v1 能力边界：只支持 translate / scale 复合（P03 前瞻决策）

- **决策**：P01 的 `Transform` 只提供 `IDENTITY` / `translate` / `scale` / `then` / `apply` / `to_array`； **不提供**
  rotation / skew 的构造器，也不暴露字段。
- **依据**：v1 的 GPU 侧变换只有 pan / zoom（`Geometry` 静态 VBO 每帧只更新 uniform，§5.5），
  旋转会让 `bounds` 与 damage 的 AABB 语义复杂化；且 `f32` 2D 仿射已能表达 pan/zoom。
- **失败模式与退路**：字段私有 + 无构造器 = 调用方不可能传入旋转矩阵；若 P03 确认相机/地图需要旋转，
  再新增 `Transform::from_array([f32; 6])` 与旋转构造器（**additive**，不改变既有签名，也不返工 P01）；
  届时 uniform 契约（两个 vec4）不变。

### S-06 taffy 冻结 `=0.15.0`（B1）

- **决策**：workspace 声明 `taffy = { version = "=0.15.0", … }`（精确钉），全文档（§5.1/§5.3④/§5.4/§9/附录 A/B）统一为 0.15.0。
- **证据**：`gpui-fast 0.1.14` 声明 `taffy =0.15.0`（sparse index 原文），仓库 `Cargo.lock` 为 0.15.0；probe
  `%TEMP%\kg-taffy-dup`
  验证 `=0.15.0` 与 `^0.15.1` **无解**（`all possible versions conflict with previously selected packages`）。
  `taffy 0.15.0` 与 `0.15.1` 的公共 API 文件 SHA256 **逐字节相同**（`lib.rs`、`tree/traits.rs`、`tree/layout.rs`、
  `tree/cache.rs`、`style/mod.rs`、`compute/mod.rs`、`prelude.rs`、`tree/mod.rs`），52 个源文件中 14 个内部实现文件有差异
  （算法修复），附录 A.1 的签名对 0.15.0 成立。
- **升级路径**：M8 移除 `gpui-*` 后把 `=0.15.0` 放开为 `0.15.x` 并复核（V-07）。

### 复审新增/修订的待复核项

| #    | 项                                                         | 复核 Phase | 触发条件                                                                         |
|------|------------------------------------------------------------|------------|----------------------------------------------------------------------------------|
| V-07 | taffy 从 `=0.15.0` 放开到 `0.15.x`                         | M8         | `gpui-*` 全族移除后；需重跑 §9-5 门禁与布局测试                                  |
| V-08 | `to_physical_f32` + VS 外扩的 AA 视觉效果                  | P10 / P18  | 像素快照出现边缘裁切或圆角偏移时，调整 `aa_padding` 取值表                       |
| V-09 | 「按物理网格自定义布局圆整」是否需要                       | P05        | 实测出现布局级 0.5 px 抖动/边框错位（否则不引入，见 F-17）                       |
| V-10 | `accesskit_macos` 的 `hit_test` / `view_children` 回调接线 | P16        | macOS 实机 VoiceOver 命中测试；确认 `NSPoint` 由 `accesskit_macos::NSPoint` 构造 |

### 复审数字复核记录（N8 / N9）

| 项                  | 审查报告 | 本次实测 | 说明                                                                                                                                                                                                                                      |
|---------------------|----------|----------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 隔离 probe 包数     | 233      | **238**  | `%TEMP%\kg-final-probe`（清单 = `phase-01-foundation.md` §5.2 并集，含 tokio/atspi/zbus/objc2 双版本），`cargo metadata` 的 `packages` 长度；差 5 个包源于 probe 范围不同，不改变「三平台一次解析通过」的结论。**已确认采用可复现的 238** |
| 仓库内联测试模块    | 276      | **277**  | `crates/**/*.rs` 正则 `#\[cfg\(test\)\]\s*\n\s*mod tests` 的严格匹配数；同范围 `mod tests` 字样为 282。**已确认采用 277**（保留严格口径注释）                                                                                             |
| 仓库并行版本（N10） | —        | 已核实   | `accesskit 0.24.1`（gpui-fast `^0.24`）+ `0.25.2`（引擎）；`skrifa 0.40.0`（现有生态）+ `0.44.0`；`harfrust 0.5.2`（`cosmic-text 0.19`）+ `0.12.0`；`objc2 0.5.3 + 0.6.5`。均为不同 semver 线的预期并存，不影响引擎自身依赖边             |

---

## 七、Phase 02 绘制基础层（2026-11-12）

> 关联文档：`design/phase-02-paint-foundation.md`（本节 = 该文档 §20 的落地条目；裁决记录见该文档 §17）。
> **项目级授权 A-01（单文件行数放宽）继续适用**；M-01…M-09、F-09/F-13/F-14/F-17、S-07 继续有效，本节不重复。
> 本节只**追加**，不改写任何历史条目（一～六 未改动）。
>
> **评审修订说明（2026-11-12，实现开始前）**：§七 经设计裁决 ①—⑤ 校准，修订点逐条标注「(裁决 ①)」等：
> - **F-25** 撤销「damage 前移到 `paint/`」的草案：damage 位置仍按 P01 §4.1 留在 `render/damage.rs`，只把落地 Phase 改为
>   **P06**；
> - **S-08** 的落地清单改为 `render/atlas.rs`→**P05**、`render/damage.rs`→**P06**、`paint/instance.rs`→**P07**；
> - **S-10** 改为「`Scene` 只读 + `Canvas` 是唯一公开写入口」；
> - 新增 **F-33**（`Canvas` = lane 片段模型）、**S-14**（边框+渐变拆两个 `Quad`）、**S-15**（`Scene`/`Canvas`/`Lane` 所有权契约）。
>
> **第二轮审查（FAIL）追加（2026-11-12）**：B1/B2/B3/N11 把 `Scene` 整体推迟 P03、`MAX_CLIP_DEPTH` 改为 `u8`；
> N6 让 `Canvas` 的 clip API 返回 `bool`、N7 记公开白名单闭包、N2/N3/N4/N5/N13 记数值与 damage 契约、
> B4 记文档完整性复核方法与事故。**新增 S-16…S-19**，并在 S-08/S-09/S-10/S-15、F-24 内追加「第二轮」说明。
>
> 事实基线：`color 0.3.3` / `etagere 0.3.0` / `bytemuck 1.25.2` 的真实源码 +
> `%TEMP%\kg-{pod-probe,pod-probe2,dead-probe,dead-probe2,size-probe}` probe，详见 phase-02 文档 §2。

### F-23 `GpuPrimitive` 必须 `#[repr(C, align(16))]`（主设计的 `#[repr(C)]` + `align_of == 16` 不成立）

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                                                             |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 设计原文   | §2.3 / 附录 B：`#[repr(C)]` + `#[derive(bytemuck::Pod, …)]` 的 `GpuPrimitive`，静态断言 "size_of == 128、align_of = 16"                                                                                                                                                                                                                                                                                          |
| 偏差       | 结构体必须是 **`#[repr(C, align(16))]`**。仅 `#[repr(C)]` 时 `size_of == 128` 成立但 **`align_of == 4`**，附录 B 声明的 `align_of = 16` 与 P01 §7.2 要求的静态断言**无法同时成立**（断言会直接编译失败）                                                                                                                                                                                                         |
| 原因       | 8 个 `[f32; 4]` 字段的自然对齐是 4；Rust 不会因为「总大小是 16 的倍数」就提升对齐。实测（`%TEMP%\kg-pod-probe`）：`error[E0080]: evaluation panicked: assertion failed: align_of::<GpuPrimitive>() == 16`。补 `align(16)` 后（`%TEMP%\kg-pod-probe2`）`size_of == 128`、`align_of == 16`、8 个 `offset_of!` 全成立、`bytemuck::Pod` derive 通过、`cargo +nightly clippy --all-targets -- -D warnings` **退出 0** |
| 修订后设计 | `paint/instance.rs`（**P07 落地**，裁决 ④）用 `#[repr(C, align(16))]`；静态断言 = size 128 + align 16 + 8 个 offset。**P10 的 `render/shaders/contract.md` 必须写「结构体 16 B 对齐」**；**三平台实例缓冲的基址也必须 16 B 对齐**（分配器/偏移计算显式保证；D3D12 常量缓冲的 256 B 要求由分段上传向上取整满足）→ phase-02 §16 R7                                                                                 |

### F-24 `AtlasTile` / `AtlasKind` 归属 `paint/atlas.rs`（不在 `resources.rs`）

| 项         | 内容                                                                                                                                                                                                                                                                                               |
|------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | P01 §4.5 公开路径表：`AtlasTile` → `ui_core::resources::*`、`AtlasKind` → `ui_core::resources::AtlasKind`，定义文件 `src/resources.rs`（P15）                                                                                                                                                      |
| 偏差       | 两者定义在 **`src/paint/atlas.rs`**，公开路径 **`ui_core::paint::atlas::{AtlasKind, AtlasTile}`**；`resources.rs`（P15）只保留 `ResourceId` / `ResourceCache`。**P01 §4.5 正文不改**，以本条为路径表的修订记录                                                                                     |
| 原因       | ①`AtlasTile` 是 `Primitive::GlyphRun`/`Image` 的**字段类型**，属 I2 的共享数据契约，必须在 `paint`；②放 `resources` 会让 `paint → resources`（P15 高层模块）反向依赖，且 P15 之前（P02/P06/P07）就要用到；③图集**分配器**仍按 brief 留在 crate 私有的 `render/atlas.rs`（P05），公开的只有只读数据 |
| 修订后设计 | phase-02 文档 §4.1/§4.2 的路径表；P15 的 `resources.rs` 不再定义这两个类型，`ResourceCache` 直接复用 `paint::atlas::AtlasTile`。**P01 `design/phase-01-foundation.md` §4.5 的 `AtlasTile` 行已按 N8 加注记**（只加注记，不改历史正文）                                                             |

### F-25 damage 仍在 `render/damage.rs`，落地 Phase 改为 **P06**（裁决 ①）

| 项         | 内容                                                                                                                                                                                                                                                                                             |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | §2.3 共享数据契约把 `DamageRect` 与图集/几何并列；P01 §4.1 把 `damage` 归 `src/render/damage.rs`（P09）                                                                                                                                                                                          |
| 偏差       | **位置不变**（`src/render/damage.rs`，crate 私有）；**落地 Phase 由 P09 改为 P06**（首个消费者 = paint 之后的 damage 步骤）。此前草案「前移到 `paint/damage.rs`」**撤销**                                                                                                                        |
| 原因       | ①`render` 整体私有（I9），`pub(crate)` item 需要**非 test 消费者**（`-D warnings` 与 `dead_code`，phase-02 §2.4c）；P06 的帧组装正是该消费者，因此不必移动位置；②damage 是纯 CPU 算法，与 GPU 无关，位置在 `render/` 不影响可测性（同模块单测）；③矩形类型复用 `math::PhysicalRect`（F-09 不变） |
| 修订后设计 | phase-02 文档 §12（算法逐字、阈值常量、14 个用例）；P09 的渲染后端只做「`DamageOutput` → 三平台 API 参数」的**平台翻译**（`Present1` dirty rects / `VK_KHR_incremental_present` / scroll hint），不重算区域                                                                                      |

### F-26 `Primitive::Path` 删除 `triangles` / `coverage_pass`（v1 路径无 AA，裁决 ③）

| 项         | 内容                                                                                                                                                                                                                                                                                                                                     |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 附录 C：`Path{ triangles, coverage_pass, fill, clip_chain }`；§5.1「CPU 三角化后的三角形 / 两阶段 coverage 标记」                                                                                                                                                                                                                        |
| 偏差       | 改为 `Path{ bounds: Rect, geometry: GeometryId, fill: Color }`：三角形走**已注册的静态几何**（§5.5 的收益项），`coverage_pass` **删除**，`clip_chain` 删除（clip 改为栈指令）。**v1 的路径是硬边三角形，不做 AA**                                                                                                                        |
| 原因       | ①三角形进每帧 `Primitive` 会让每次 lane 重建都拷一份顶点，直接抵消 §5.5「顶点零上传」；②两阶段 coverage 是渲染器的 pass 结构（同一份三角形成对出现），放进每帧数据会让 1 个逻辑绘制变成 2 个 `Primitive`、破坏 `REUSABLE` 整桶复用、污染 §3.8 oracle 的 `PartialEq` 比较；③小尺寸矢量（54 个图标）在**构建期**转成图集（§5.6），不受影响 |
| 修订后设计 | phase-02 文档 §6.3.4；`Path` 的几何必须是 `GeometryPipeline::Ui` + 恒等变换；AA 由后续**两阶段 coverage**（`kind = 4` 已在 contract 预留）或 MSAA 承担，**不进 v1**；**像素 golden 覆盖矩阵必须显式标注「路径硬边」**（phase-02 §14.10）                                                                                                 |

### F-27 `Primitive::Geometry` 删除 `pipeline` / `damage` 字段

| 项         | 内容                                                                                                                                                                                                                                            |
|------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 附录 C：`Geometry{ geometry_id, transform, pipeline, damage }`                                                                                                                                                                                  |
| 偏差       | 改为 `GeometryDraw{ id: GeometryId, transform: Transform }`（独立类型名，避免与 `paint::geometry::Geometry` 混淆）                                                                                                                              |
| 原因       | ①`pipeline` 属 `GeometryDesc`（注册期不可变），每帧图元再存一份 = 两个真值来源（I2）；②`damage` 是**计算结果**（变换前后 AABB ∪ 物理像素外扩），不是存储字段；存进去会让「同一几何被两个节点引用」时无法归属，也会让复用的 lane 持有过期 damage |
| 修订后设计 | phase-02 文档 §6.3.7 / §9.6；damage 由 P06/P07 在累加时按 `desc.bounds` + `transform` 计算                                                                                                                                                      |

### F-28 `color 0.3.3` 没有 `map_chroma`（§5.8.1 事实更正）

| 项         | 内容                                                                                                                                                                                                                                                                                                                                     |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | §5.8.1：「它提供 `ColorSpaceTag::{Oklch, …}`、`DynamicColor::convert/interpolate`、`map_lightness/map_hue/map_chroma`、CSS Color 4 的 `none` 缺失分量与 premultiplied 插值」                                                                                                                                                             |
| 偏差       | **不存在 `map_chroma`**。`DynamicColor` 只有 `scale_chroma(scale)`（`dynamic.rs:264`）、`map_lightness`（:530）、`map_hue`（:550）；`map_chroma` 在 `color-0.3.3` 全 crate 无此符号。另：`DynamicColor { cs, flags, components }` 三个字段均 `pub` 且类型**无 `#[non_exhaustive]`**（`dynamic.rs:34-50`），可直接构造，不需要 parse 路径 |
| 原因       | 0.3.x 的 chroma 操作统一为「按比例缩放」（`ColorSpace::scale_chroma`，`tag.rs:346`），没有绝对映射                                                                                                                                                                                                                                       |
| 修订后设计 | 自建 gamut mapping 的 chroma 二分**直接构造** `DynamicColor { cs: ColorSpaceTag::Oklch, flags: Flags::default(), components: [L, mid, h, alpha] }`，再 `.convert(ColorSpaceTag::LinearSrgb)`。等价写法 `scale_chroma(mid / C)` 需 `C == 0` 特判且有除法误差，不采用。phase-02 §5.4 的伪代码与 §14.1 的测试按直接构造实现                 |

### F-29 `Canvas` v1 **不**注册应用自有 pipeline（§5.9 的延后）

| 项         | 内容                                                                                                                                                                                                                                                                                                               |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | §5.9：「应用注册自有 pipeline（顶点布局 / uniform / shader 句柄），在 paint 阶段向 `Scene` 追加 `Primitive::Canvas(CustomDraw { pipeline, buffers, uniform, damage })`」                                                                                                                                           |
| 偏差       | v1 的 `Canvas` **只记录引擎图元**（`Quad`/`GlyphRun`/`Image`/`Path`/`Shadow`/`Geometry`/`Clip`），不注册 pipeline、不接受 GPU 句柄、不允许任意命令插入                                                                                                                                                             |
| 原因       | ①D2 明确不内化外部 shader 工具链（P10 的 build.rs 只编译引擎三套 shader）；②§5.9 自身禁止「任意 GPU 命令插入」（破坏 I4 与 damage 正确性），而「不能插入命令的自定义 pipeline」语义上等价于用引擎图元拼装；③自定义 pipeline 需要三平台 PSO 创建/资源绑定/延迟销毁，是独立工程量，且必须以 P11/P16/P17 全完成为前提 |
| 修订后设计 | phase-02 文档 §11（含 `Canvas` 的 lane 片段模型，见 F-33）；扩展路径（additive）：`Canvas::push_custom(CustomDraw)` + `GeometryPipeline::Custom { pso }` + `App::register_pipeline` + `kind` 保留值 6/7                                                                                                            |

### F-30 `GeometryId` 是 `Copy` 句柄，释放必须显式

| 项         | 内容                                                                                                                                          |
|------------|-----------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | §5.5 表格：「`drop(GeometryId)` → retire 队列延迟释放」                                                                                       |
| 偏差       | `GeometryId` 是 `Copy`（无 `Drop`），释放改为显式 `App::release_geometry(id)`；节点侧由 P08 的 `NodeKind::Geometry` 卸载自动 release          |
| 原因       | `Primitive::Geometry`/`Path` 每帧都要携带 id；`Drop` 句柄要么每帧 clone（引用计数）、要么让 `Primitive` 不能 `Copy`，两者都抬高 lane 复用成本 |
| 修订后设计 | phase-02 文档 §9.1；失败模式（忘记释放 = VBO 泄漏到进程结束）写进 §16 R9，P18 用内存快照/`GpuTimer` 发现                                      |

### F-31 `GeometryPipeline` 不含 `Path` 变体

| 项         | 内容                                                                                                                                                                                                          |
|------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | §5.5「`GeometryPipeline`：应用注册或引擎内置」；§5.1 把 `Path` 的目标管线写成 "geometry pipeline"，隐含一个独立的 Path 管线                                                                                   |
| 偏差       | v1 只有 `Ui`（stride 8）与 `Textured { atlas }`（stride 16）两个变体；`Primitive::Path` 用 `Ui` + 恒等变换                                                                                                    |
| 原因       | `Path` 与 `Ui` 的顶点布局、着色方式、管线状态完全相同，唯一差别是「谁拥有几何、变换是否恒等」——那是 `Primitive` 的语义，不是 GPU 管线的语义。再造一个变体会让 P09 维护两条一模一样的管线（三平台 × 2 = 6 份） |
| 修订后设计 | phase-02 文档 §9.4/§9.7；`Ui` 的颜色走 per-draw 常量（`GeometryColor`，16 B），顶点不带颜色                                                                                                                   |

### F-32 渐变 v1 只支持双色 + linear premultiplied 插值（OKLab 渐变与 LUT 延后）

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                                       |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | §5.8.2：「渐变必须在 OKLab 空间插值……在 CPU 生成梯度 LUT，仍不进片元」；§5.4 `gradient_eval(kind, t, color0, color1)`                                                                                                                                                                                                                                                                      |
| 偏差       | v1 的渐变在 **shader 内、linear premultiplied 空间**插值（`color0`→`color1`），只用 `params0.z` 区分 horizontal/vertical/radial；**不生成 LUT**，不支持 ≥3 色标，不支持带角度的线性渐变                                                                                                                                                                                                    |
| 原因       | ①`GpuPrimitive` 冻结在 128 B，只有两个颜色槽；LUT 需要「渐变纹理」= 新 `AtlasKind` + 新 `kind` 值 + `texture_index` 语义扩展，属**契约变更**（I2/I8 要求三平台 shader 同步）；②shader 内做 OKLCH→linear 的每片元转换（cbrt + sin_cos）代价明显；③`ColorResolver::interpolate` 保留为 **CPU 侧**求值（P13 动画、P03 hover 过渡、主题混合），两套语义在 `contract.md` 分别标注，禁止互相替代 |
| 修订后设计 | phase-02 文档 §5.5/§5.6/§16 R3；触发条件：设计稿出现 ≥3 色标，或深色主题下双色线性渐变肉眼可见发灰 → 引入 `AtlasKind::Lut` + `kind` 变体（additive）                                                                                                                                                                                                                                       |

### F-33 `Canvas` 改为「lane 片段」模型，删除 `CanvasOp`（裁决 ②）

| 项         | 内容                                                                                                                                                                                                                                                                        |
|------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | §5.9 的 `Primitive::Canvas(CustomDraw{…})`；phase-02 v1 草案的 `Canvas{ bounds, ops: Vec<CanvasOp> }` + `CanvasOp` 枚举                                                                                                                                                     |
| 偏差       | `Canvas{ bounds: Rect, lane: scene::Lane }` + **公开 `push_*`**（`push_quad`/`push_text`/`push_image`/`push_path`/`push_shadow`/`push_geometry`/`push_clip`/`pop_clip`）+ 只读 `bounds()/primitives()/is_empty()`；**`CanvasOp` 删除**                                      |
| 原因       | ①裁决 ②：`Canvas` 是应用层**唯一公开写入口**，`Scene` 只读；②`CanvasOp` 是 `Primitive` 的**第二份图元表示**（I2 违反），删掉后全链路只有 `Primitive` 一份；③`Canvas` 持有 `Lane` 后，`Lane::new/push/clip_depth` 在 P02 就有**非 test 消费者**（解决 `dead_code`，见 S-08） |
| 修订后设计 | phase-02 文档 §11（API/所有权/不变量/扩展路径）、§7.1（`Lane` 的 `pub(crate)` 写入面）、§7.2（可见性契约）；递归类型（`Primitive::Canvas` → `Canvas` → `Lane` → `Vec<Primitive>`）由 `Vec` 提供间接，编码器 `assert!(canvas_depth == 0)` 禁止嵌套                           |

### S-08 P02 的落地策略：只落「可达且 §1.3 白名单内」的数据契约（裁决 ①）

- **决策**：P02 落地 **7 个文件** —— `paint.rs`、`paint/{color,primitive,scene,geometry,canvas,atlas}.rs`；`lib.rs` 追加
  `pub mod paint;`；`..` 追加 `bytes`。**推迟**：`paint/instance.rs`→**P07**、
  `render/damage.rs`→**P06**、`render/atlas.rs`+`render.rs`→**P05**。三处的**规格**在 phase-02 文档里一次冻结
  （字段/签名/算法/常量），实现 Phase 只做「把规格变代码 + 接上消费者」，不得改语义。
- **依据**：`cargo +nightly clippy --all-targets -- -D warnings` 下，任何无同相非测试消费者的 `pub(crate)` item（甚至私有模块
  里的 `pub`）都会被 `dead_code` 判死（`%TEMP%\kg-dead-probe` / `kg-dead-probe2` 实测原文见 phase-02 §2.4c）；而 §1.3 的
  公开面是一张封闭表，`GpuPrimitive`/`InstanceBuffer`/`DamageAccumulator`/`AtlasAllocator` 都不在其中，不能靠「提升为
  `pub`」
  规避；AGENTS §4 又禁止 `#[allow(dead_code)]`。三者夹击下唯一可行解是**按消费者排序落地**。
- **硬规则**：P02 **不得**创建 `src/render.rs`、`src/render/`、`src/paint/instance.rs`、`src/paint/damage.rs`；
  每个 `pub(crate)` item 必须有非 test 消费者（P02 是 **5 个方法 + 1 个常量**：
  `Lane::{new, push, clip_depth, primitives, is_empty}` 与 `MAX_CLIP_DEPTH`，消费者 = `Canvas`）。
- **代价**：P05/P06/P07 需要多一步「实现已冻结的规格」。
- **第二轮修订（B1/B2/B3/N11）**：`Scene` / `LaneId` / lane 分配 / 绘制序**整体推迟 P03**；P02 的 `paint/scene.rs` 只剩
  **`pub(crate)` 的 `Lane` 与 `MAX_CLIP_DEPTH`**（`pub(crate) mod scene;`）。`Lane::push` 内部用
  `usize::from(self.clip_depth()) < usize::from(MAX_CLIP_DEPTH)` 断言，保证 `clip_depth()`/`MAX_CLIP_DEPTH` 有非 test
  消费者。

### S-09 `MAX_CLIP_DEPTH = 8`，超限 `assert!`（不新增 `EngineError` 变体）

- **决策**：`paint::scene::MAX_CLIP_DEPTH = 8`；**C2/C3 由 `Lane::push` 在 P02 即强制**（`Pop` 不得弹穿 0、深度不得超限），
  **C1（lane 末尾平衡）由 P03 的 `end_lane` 断言**。超限用 `assert!`，不返回 `Result`。
- **理由**：paint 是纯 CPU 数据层，clip 嵌套由引擎自己的布局/样式生成，应用层无法直接压栈；>8 层是编程错误，不是运行时状态。
  新增一个应用层永远无法处理的错误变体（还要进 F-15 的 17 变体冻结表）属预防性抽象。
- **一致性要求**：P02 `Lane::push` / P07 编码器 / P10 三套 shader 的固定数组长度必须同为 8；改这个常量必须三处同步。
- **第二轮修订（B3 + N6）**：类型固定为 **`u8`**（`pub(crate) const MAX_CLIP_DEPTH: u8 = 8`）；断言写成
  `usize::from(...) < usize::from(...)`；**应用层路径不 panic**——`Canvas::push_clip`/`pop_clip` 返回 `bool`
  （超深/空栈返回 `false` 且不写入），`Lane::push` 的 `debug_assert` 只抓「引擎内部绕过 Canvas」的误用；
  C1（lane 末尾平衡）由 P06 的展平阶段断言。

### S-10 `Scene` 只读 + `Canvas` 唯一公开写入口（裁决 ②）

- **决策**：`Scene` 的公开面**全部只读**（`new`/`Default`/`lane`/`lane_count`/`order`/`iter`/`primitive_count`/
  `is_empty`）；
  写入面（`new_lane`/`release_lane`/`lane_mut`/`clear_frame`/`set_order`）为 `pub(crate)` 且**P02 不落地**（无消费者 →
  P03）。
  应用层的写入口是 **`Canvas`**（8 个 `push_*`/`pop_clip`），它内部持有 `scene::Lane`。
- **理由**：§1.3 明确「`Scene`（只读句柄）」；`Scene` 的公开可变方法会给出「应用可构造 Scene 但无法提交」的死 API；
  而把 `Scene` 改成 `pub` 可变只是为了 P02 测试方便，属于用公开面换测试便利，不可接受（裁决 ② 明确否决）。
- **备选（已否决）**：`Scene` 的写入面 `pub`。
- **第二轮修订（B1/B2/N11）**：`Scene` 在 P02 **完全不存在**（连只读面也没有），整体推迟 P03；P02 的写入口只有 `Canvas`。

### S-11 图集 v1：单页 + `R8`/`BGRA8 sRGB` + 1 纹素 padding + 无 CPU 像素副本

- **决策**：每个 `AtlasKind` **单页**（Glyph `2048²` R8 = 4 MiB，Image `2048²` BGRA8 = 16 MiB）；内容区四周 1 纹素清 0
  padding
  且**不计入 UV**；UV 用「边」而非纹素中心，并**按请求的内容尺寸**计算（`etagere` 返回的 `rectangle` 可能被 alignment /
  shelf 分档放大，不能直接拿它算 UV）；`generation` 只在 `clear()`（DPI 变化 / 字形换代 / 容量逃逸）时 +1；
  容量失败路径 = `clear + retry` → 仍失败则 `EngineError::AtlasExhausted { kind }`（P05 随分配器追加该变体）；
  图集**不持有** CPU 像素副本（源像素在 P06/P15 的缓存里）。
- **理由**：`etagere 0.3.0` 的结构限制（`allocator.rs:189` 拒绝 `> u16::MAX`；:99-100 单列 `shelf_width = page_width`；
  `deallocate` 对非法 id `panic`，:398-399）；单页让 `AtlasTile` 不需要 `page` 字段（契约最小）；
  「整体清空 + 代际失效」是 §9.3 明示的重排路径，比 LRU/扩容更简单且语义闭合。
- **延后**：LRU 逐出、多页、页扩容（2× 翻倍）。触发条件见 phase-02 §16 R5。

### S-12 `Color` 的 v1 范围与「两套插值语义」的显式分离

- **决策**：`Color` 5 变体（`Srgb`/`LinearSrgb`/`DisplayP3`/`Oklab`/`Oklch`）+ 独立 alpha；**不提供** `from_hex`、CSS 字符串解析、
  `none` 缺失分量、powerless hue、`Hwb/Hsl/Lab/Lch/Rec2020/AcesCg` 变体；预乘只发生在 `ColorResolver::resolve` 的出口。
  `ColorResolver::interpolate`（CPU 侧，OKLab/OKLCH/linear）与 **shader 的双色渐变**（linear premultiplied，F-32）
  是两套语义，`contract.md` 必须分别写明，禁止互相替代。
- **理由**：无 CSS 字符串解析 → 无缺失分量/powerless hue 的状态机；`from_hex` 无 P02 消费方（应用层可直接用
  `color::parse_color`）。
- **延后**：`from_hex`、宽色域变体、HDR/PQ/ICC（§5.8.3 已定 HDR 不进 v1 默认路径）。

### S-13 damage 阈值（16 / 0.5）是常量而非配置层

- **决策**：`MAX_DAMAGE_RECTS = 16`、`FULL_FRAME_AREA_RATIO = 0.5` 都是模块内 `pub(crate) const`；不引入 `DamageConfig` /
  环境变量 / 特性开关。
- **理由**：AGENTS §1.2 禁止「当前没有实际需求的配置层」；两个数字的依据分别是三平台 dirty rect 列表长度上限（16 是安全值）
  与「超过半屏时全帧更便宜」的经验值。P18 基准若给出更好的数值，改常量 + 更新文档即可（V-13）。

### S-14 边框 + 渐变 = 两个 `Quad`（顺序 / 混合 / 责任，裁决 ⑤）

- **决策**：`QuadPaint` 保持三选一（`Solid{fill,border}` / `Gradient`），**不新增**「渐变 + 边框」的复合变体。需要两者时由
  P03 的 style → paint 产出**两个 `Quad`**：①`Gradient` 填充（无 border）；②同 bounds 的
  `Solid{ fill: TRANSPARENT, border: Some(b) }`。
- **绘制顺序**：先填充/渐变、后边框（顺序即绘制序，引擎不重排）。
- **混合语义**：边框像素 = `border over fill`，填充内缘只被覆盖一次；边框不透明时直接覆盖（CSS 内联边框的视觉）。
  两者都半透明时边框区域经历**两次 premultiplied over**——这是与 CSS「半透明 border 叠在 background 上」一致的
  **已接受行为**。
- **应用层责任**：需要「边框内缘挖空」观感时，`base_ui` 必须让边框不透明，或给 `fill` 用**与背景预乘合成后的不透明色**；
  引擎不提供 stencil/两 pass 的挖空（那会破坏 v1 的单实例模型）。
- **damage 影响**：两个 quad 的 `bounds` 相同 ⇒ 累加器的「并集面积 ≤ 两者之和」判据把它们合并成 1 个 rect，不放大 damage。
- **实例数**：每节点最多 +1 实例；比扩 128 B 契约（三套 shader + contract + damage 同步）便宜得多。

### S-15 `Scene` / `Canvas` / `Lane` 的所有权与 API 契约（裁决 ②）

- **决策**：
    - `Canvas` 是**应用层唯一公开写入口**；它是**值**（应用构造 → 交给引擎 → 引擎包成 `Primitive::Canvas` 塞进 lane），
      内部持有 `scene::Lane`；`bounds` 契约 = ⊇ 内部图元并集（damage/命中只用它）。
    - `Scene` 只读（S-10）；引擎侧的 lane 分配/绘制序在 P03。
    - `Lane` 的写入面 `pub(crate)`，**消费者 = `Canvas`**（P02 的 `dead_code` 约束因此满足）；`Lane` 的 `clip_depth`
      由 `Lane::push` 增量维护并强制 C2/C3。
- **理由**：让 `Canvas` 直接持有 `Scene` 会让每个自绘节点拖一张 lane 表 + 绘制序（无边界浪费），也无法表达「canvas 属于哪个节点」；
  一个 lane 是两者的最小交集。同时它把「图元序列只有一份表示」（I2）与「应用层不能写 Scene」（§1.3）同时满足。
- **第二轮修订（N6/N7/N1）**：`Lane` 是 **`pub(crate)`**（应用层只见 `&[Primitive]`）；`Canvas` 尺寸 48 B（Rect 16 + Lane 32）；
  `push_clip`/`pop_clip` 返回 `bool`、文档写 `# Panics: none`。
- **不变量**：`Canvas` 不得嵌套（无接受 `Canvas` 的 push 方法；编码器 `assert!(canvas_depth == 0)`）；`Canvas::push_text`
  只接受引擎产出的 `GlyphRun`（应用层无法构造，需文本请用 `NodeBuilder::text()`）。

### Phase 02 追加的待复核项

| #    | 项                                                                                                                                                             | 复核 Phase | 触发条件                                                                                                    |
|------|----------------------------------------------------------------------------------------------------------------------------------------------------------------|------------|-------------------------------------------------------------------------------------------------------------|
| V-11 | `GpuPrimitive` 的 16 B 对齐与三平台常量/结构化缓冲要求的组合（D3D12 需 256 B 基址对齐、Metal `setVertexBytes` 上限、Vulkan `minStorageBufferOffsetAlignment`） | P09/P10    | 首次上传实例缓冲；若某平台的基址对齐与「只写 dirty 区间」冲突，改为 256 B 对齐的分段上传（不改 128 B 契约） |
| V-12 | `Path` 无 AA 在大面积矢量填充上的视觉可接受性（裁决 ③）                                                                                                        | P18        | 像素快照出现明显锯齿且该图形未走构建期图集 → 启用两阶段 coverage（`kind = 4`）；契约不变                    |
| V-13 | damage 阈值 16 / 0.5 与三平台实际收益                                                                                                                          | P18        | L4 基准显示局部重绘收益低于全帧                                                                             |
| V-14 | 图集单页容量（Glyph 2048² / Image 2048²）与真实资源                                                                                                            | P15/P18    | 出现 >2048 的预览图或字形集溢出 → 加 `page` 字段或扩容（契约变更，需三平台同步）                            |
| V-15 | 每帧 `resolve` 颜色的 CPU 成本                                                                                                                                 | P18        | >0.5 ms/帧 → lane 内缓存 `LinearPremulRgba`（additive）                                                     |
| V-16 | `render/damage.rs`（P06）与 `render/atlas.rs`（P05）的落地顺序是否与文本/图集的实际先后一致                                                                    | P05/P06    | 若总计划调整相位：文件跟**消费者**走（phase-02 §3.2 末段），并同步更新 phase-02 §19                         |

### S-16 §1.3 公开白名单按**传递闭包**扩充（N7）

- **决策**：§1.3 的白名单按类型名列出；`paint` 的契约会通过字段/参数**传递**带出下表类型，因此把它们显式登记为
  §1.3 的传递扩充（不新增能力）。`Lane` 与 `MAX_CLIP_DEPTH` 保持 `pub(crate)`，**不进**公开面。
- **登记清单**（详见 phase-02 §4.4）：

  | 传递新增的公开类型                                         | 由谁带出                  | 消费 Phase      |
  |------------------------------------------------------------|---------------------------|-----------------|
  | `Quad` `QuadPaint` `Border` `Gradient` `GradientKind`      | `Primitive::Quad`         | P03/P07/P06     |
  | `GlyphRun` `PositionedGlyph`                               | `Primitive::GlyphRun`     | P06/P07         |
  | `Image` `Path` `Shadow` `ClipOp` `ClipRect` `GeometryDraw` | `Primitive` 其它变体      | P03/P07/P08     |
  | `AtlasKind` `AtlasTile`                                    | `GlyphRun`/`Image` 的字段 | P06/P15/P07     |
  | `LinearPremulRgba` `Interpolation` `HueDirection`          | `ColorResolver` 的签名    | P02/P03/P06/P08 |
  | `IndexType` `Topology` `GeometryPipeline`                  | `GeometryDesc` 的字段     | P08/P09         |

- **理由**：不登记会让「§1.3 封闭表」与「实际公开面」出现无法审计的偏差；登记后 `cargo doc` 的公开项可以与表逐条对齐。
- **不做**：不把 `GpuPrimitive`/`InstanceBuffer`（P07）、`DamageAccumulator` 系列（P06）、`AtlasAllocator` 系列（P05）加入公开面。

### S-17 文档完整性复核方法（B4）与本轮损坏事故记录

- **事故**：phase-02 文档在编辑过程中被一次 PowerShell 替换误伤——`@(@('a','b'))` 会被 PowerShell 展平成 2 个字符串，
  于是 `$pair[0]`/`$pair[1]` 变成「首字符/次字符」，一次 `Replace` 把全文的 `|` 换成了空格（DEVIATIONS 的 `>` 也被同类操作
  短暂换掉）。结果是表格结构、`||`/`&&` 运算符出现缺失。**已全部修复**（DEVIATIONS 从 git 恢复后重写 §七；phase-02 逐处
  恢复并用命令复核）。
- **复核方法（写进 phase-02 §15-12，每次改文档后跑）**：
    1. 表格管道符总数 `([regex]::Matches($doc, '\|')).Count`（当前基线见 §15-12a；显著下降即被破坏）；
    2. 三个必须成对出现的运算符字面量各命中 1 次：`kind != QUAD || border_width == 0.0 || color1.is_opaque()`、
       `content.0 == 0 || content.1 == 0`、`usize::from(self.clip_depth()) < usize::from(MAX_CLIP_DEPTH)`；
    3. 双空格伪算子扫描（代码围栏内不应出现「标识符 + 2 空格 + 标识符」，对齐用的注释列人工确认）。
- **工具纪律**：文档批量替换**只用**单个 `$old`/`$new` 变量或 `$c.Substring` 拼接；**禁止** `$pairs = @( @(a,b) )`
  这种单元素嵌套数组；每次写盘后立刻复跑上面的复核命令。

### S-18 `Canvas` 的 clip API 返回 `bool`、不 panic（N6）+ `Lane`/`MAX_CLIP_DEPTH` 的类型与断言（B3）

- **决策**：`Canvas::push_clip(&mut self, clip: ClipRect) -> bool`、`Canvas::pop_clip(&mut self) -> bool`；
  超深 / 空栈返回 `false` 且**不写入**任何图元；所有 `Canvas` 公开方法文档写 `# Panics: none`。
- **`Lane`**：`pub(crate) struct Lane { primitives: Vec<Primitive>, clip_depth: u8 }`；`push` 内部
  `debug_assert!(usize::from(self.clip_depth()) > 0)`（Pop）与
  `debug_assert!(usize::from(self.clip_depth()) < usize::from(MAX_CLIP_DEPTH))`（Push），
  并用 `saturating_add/sub` 防止 release 下回绕。
- **`MAX_CLIP_DEPTH`**：`pub(crate) const MAX_CLIP_DEPTH: u8 = 8`（B3：类型就是契约的一部分）。
- **理由**：应用层经 `Canvas` 表达 clip，越界是**可恢复的调用错误**，不应 panic（N6）；而引擎内部若绕过 `Canvas`
  直接 `Lane::push` 属编程错误，用 debug 断言抓。C1（lane 末尾平衡）由 P06 的展平阶段断言。

### S-19 数值与 damage 语义契约（N2 / N3 / N4 / N5 / N13）

- **N2 整数取回**：`params1` 的四个分量是「整数编码进 f32」；三套 shader 一律用**数值转换**（HLSL/MSL `(int)x`、GLSL
  `int(x)`）， **禁止**位重解释（`as_type`/`asfloat`/`floatBitsToInt`）。`contract.md` 必须逐字写明。
- **N4 数值健壮性**：`resolve`/`interpolate` 的**每个**分量先 `NaN → 0` 再 clamp 到定义域
  （`clamp_01` / `clamp_nonneg`，`h` 先 `rem_euclid(360)`），输出严格在 `[0,1]`（alpha 单独处理）。
- **N5 可复现性**：同平台**逐位**可复现；跨平台允许 ULP 级差异（`sin_cos`/`cbrt` 的 libm 实现不同），像素 golden 按 **8 位**
  比较。
- **N3 scroll 放弃规则**：仅当**所有** dirty rect 都落在 `scroll.rect` 内才保留 scroll hint；否则放弃 scroll、保留全部
  rects
  （宁多画不漏画），不丢弃任何脏区。
- **N13 Clip 与 damage**：`Clip` **本身不产生像素**，不 push damage；mask 边界只作用于被裁剪图元（其 damage 由被裁剪图元
  自己的 bounds + `clip` 求交得到）。

### S-20 `GeometryId` 的可见性：私有字段 + 公开 getter + 仅测试构造器

- **文档原文（phase-02 §9.1/§18.2）**：`GeometryId { index: u32, generation: u32 }` 字段私有，「只能由 `GeometryStore` 铸出」。
- **代码审查要求**：字段改 `pub(crate)` + 公开 `index()`/`generation()`。
- **不可行**：workspace lint `field_scoped_visibility_modifiers = "forbid"` 明确拒绝 `pub struct` 里的 `pub(crate)` 字段
  （实测 `%TEMP%\kg-field-vis`：`error: scoped visibility modifier on a field`）。
- **实际落地**：字段**保持私有**；新增公开 `index()` / `generation()`；测试用
  `#[cfg(test)] pub(crate) const fn for_test(index, generation)`
  构造。`for_test` 只存在于 test 构建，因此**不触发 `dead_code`**，也不扩大公开面。
- **为什么不选「`pub` 字段 + 文档说明」**：那会让应用层用结构体字面量伪造句柄（P09 无法区分伪造与真实，只能靠代际偶然拦截）；
  `for_test` 把这条口子限制在本 crate 的测试构建内。
- **P09 交接**：`GeometryStore` 落地时把 `for_test` 换成 `pub(crate) fn mint(index, generation)`（同签名、有非 test 消费者），
  测试改用 `mint`；公开面不变。

### S-21 第二轮代码审查（A/B 均 PASS-WITH-FIXES）：B1 与 N1–N15 的处置

- **阻塞 B1 已修复**：`interpolate` 的 `mid` 此前在 `Interpolation::LinearSrgb` 下复用了 `resolve` 的 (A) 路径（逐通道
  clamp），
  破坏端点不变量。现在：①`resolve` 与 `interpolate` 共用 `normalized_dynamic`（alpha clamp + Oklch 的 L/C/h 归一）；
  ②`mid` **无条件**走 `gamut_mapped_linear`（转 Oklch → 24 步 chroma 二分 → premultiply），不再看插值空间。
  回归用例：`interpolate_endpoints_match_resolve`（4 空间 × 4 端点 × 2 端）、
  `linear_srgb_space_matches_resolve_for_p3_endpoint`
  （审查给出的复现用例），以及两条独立字面量断言 `p3_green_midpoint_is_gamut_mapped` / `p3_blue_midpoint_is_gamut_mapped`。
- **N1** `clamp_nonneg`/`hue_deg` 对非有限值（含 `±inf`）统一收敛到 `0.0`；新增 `clamp_nonneg_handles_infinity`、
  `hue_extremes_are_normalized`。
- **N2** `Canvas::push_clip` 对 `ClipRect`（坐标/尺寸/圆角）加 `debug_assert!`（C4），文档写明 debug-only；新增
  `push_clip_rejects_negative_size`。
- **N3** `ColorBrush` 移到 `#[cfg(test)] mod tests` 之前。
- **N4/N5** `primitive.rs` 的样例表补第 8 个变体（`Primitive::Canvas`）；新增 `canvas_size_is_48_bytes`。
- **N6** 颜色测试对齐设计：P3 红断言 `>= 0.99`；`350→10` 的 `Shorter == Increasing` 前提写反，改为断言 `Longer` 中点 ≈180°，
  另加 `10→350` 的 `Increasing`/`Decreasing` 行为断言；`oklch_in_gamut_is_passthrough` 改用 `color` crate 独立期望。
- **N7** 新增 `resolve_linear_srgb_variant_is_passthrough`、`resolve_oklab_variant`、`interpolate_linear_srgb_space`。
- **N8** 见 **S-20**（`pub(crate)` 字段被 lint 禁止）。
- **N9** geometry 测试改用 `ahash::HashSet`；crate `[dev-dependencies]` 追加 `ahash = { workspace = true }`。
- **N10/N11** `push_text` 文档改为「应用层不应自行构造，`AtlasTile` 只能来自图集分配器」；`push_text`/`push_image` 分别
  `debug_assert` 图集 kind；新增 `push_image_rejects_glyph_atlas`。
- **N12** `push_text_keeps_run` 断言 `run` 整体相等 + color/tile/position/size 逐项保留。
- **N13** 四个弱断言重写：`all_variants_clone_eq_and_debug`（8 变体 + Debug 名 + 改字段破坏相等）、
  `quad_paint_encodings_are_mutually_exclusive`、`gradient_kind_has_three_distinct_variants`、
  `clip_op_push_pop_roundtrip`。
- **N14** 本文件与 phase-02 的不一致已统一：P02 的 `pub(crate)` item 是 **5 个方法 + 1 个常量**
  （`Lane::{new,push,clip_depth,primitives,is_empty}`、`MAX_CLIP_DEPTH`）；C1 的断言归属统一为
  「引擎展平阶段（P06）的 `debug_assert`；P03 的 `end_lane` 只做 lane 平衡的显式收尾」。
- **N15** `Canvas::push_clip`/`pop_clip` 加 `#[must_use]`。
- **B-N5** phase-02 §9.1/§18.2 与本节统一为「私有字段 + 公开 getter + 仅测试构造器」。
- **B-N6** 端点 hue 归一化由 `normalized_dynamic` 覆盖；`h = 1e9` 用例确认与 `rem_euclid(360)` 逐位一致。
- **B-N7** powerless hue：v1 **不承诺** `C = 0` 端点的 hue 语义 —— `Color::Oklch{l, 0, h}` 的字面量保留调用方给的 h，
  而 `Color::Oklab` 转 Oklch 时 missing hue 会被 `color` crate 取 0；两者在 `C = 0` 处**不可互换**（视觉无差异，但
  `PartialEq` 与帧记录会不同）。v1 的用法（主题色、渐变端点）不依赖该差异。
- **B-N8** phase-02 §15 的「`paint::*` 全部有文档注释」不成立，已删除该表述；公开项文档债登记为 **V-22**。
- **B-N9** phase-02 的表数勘误为 **83**（分隔行计数）。
- **门禁**：`cargo test` **103** / `--release` **99**、clippy `-D warnings`、`fmt --check`、`doc` 零警告、落地文件边界、
  `cargo check --target aarch64-apple-darwin` 全绿（Linux 交叉见提交说明）。

### S-22 公开项的 `missing_docs` 债务（V-22）

- **事实**：P02 的公开 API 只对**类型/模块/关键方法**写了文档注释，字段级/变体级注释不全。
  当前 `missing_docs` 计数（reviewer 实测）：`paint::color` 34、`paint::primitive` 24、`paint::geometry` 10、`paint::atlas` 1。
- **决策**：**本阶段不补齐**，登记为债务。理由：①这些字段的语义已由 phase-02 §5–§10 逐条定义，重复一遍会形成两处真相；
  ②`missing_docs` 未开启为 lint，不影响门禁；③字段注释应在 P03/P07 的**首个消费者**落地时按实际用法补（那时才知道哪些字段值得展开）。
- **触发条件**：P10 的 `contract.md` 同步要求 rustdoc 可读，或任何 Phase 决定开启 `#![warn(missing_docs)]` → 先补
  `paint::*`。

---

## 八、Phase 03 节点状态核心（2026-11-12）

> 关联文档：`design/phase-03-node-state.md`（本节 = 该文档 §22 的落地条目；裁决记录见该文档 §20）。
> **项目级授权 A-01 继续适用**；M-01…M-09、F-09/F-13/F-14/F-17、S-07、§七 全部条目继续有效，本节不重复。
> 本节只**追加**，不改写任何历史条目（一～七 未改动）。
>
> **事实基线**：`taffy 0.15.0`（`Style<S>`/`Dimension`/`Layout`/`Cache`）、`tokio 1.54.1`（`Handle`）、
> `ahash 0.8.12`、`smallvec 1.16.5`、`bitflags 2.13.2` 的真实源码（`E:\.cargo\registry\src\rsproxy.cn-e3de039b2554c837\`），
> 逐条行号见 phase-03 §2。**未使用 web 检索**；P03 不写生产代码，唯一的语言行为 probe 是
> `%TEMP%\kg-p03-priv-probe`（验证 `private_interfaces` 在 `-D warnings` 下为 error，决定 F-38 的形态，见 phase-03 §2.4）。
>
> **裁决**（phase-03 §20 提案 / §23 裁决记录，2026-11-12 上级裁决：**全部采纳**）：①`Application`/`AppHandle` 推迟，
> `App::runtime_handle(&self) -> &Handle`；②`versions` 全删（含每槽位 `version`）+ `write<T: Clone + PartialEq>` 等值短路；
> ③`Scene` 写面 `pub` + doc 标注 engine-internal + 不 `Clone`/`Default`/无公开构造（取「只给切片投影」的方案②）；
> ④`Owner` 携带 `WindowId`（derive 与去重/序规则见 S-32）；⑤派生环检测移到运行时（`EvalGuard` 保证 panic 安全）。
> 裁决同时给出 9 条细化（`refresh_styles` 报告、`Border` 核对、`build` 错误语义、taffy 泄漏的白名单登记等），
> 逐条落地位置见 phase-03 §23.2。

### F-34 `NodeBuilder` 的公开路径改为 `node::builder::NodeBuilder`

| 项         | 内容                                                                                                                                                                         |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | P01 §4.5 公开路径表：`NodeBuilder` `NodeHandle` `Slot` `Children` → `ui_core::node::*`（定义文件 `src/node.rs`，模块根）                                                     |
| 偏差       | `NodeBuilder` 定义在 **`src/node/builder.rs`**，公开路径 **`ui_core::node::builder::NodeBuilder`**。`NodeHandle`/`Slot`/`Children` 推迟 P08（S-29）                          |
| 原因       | P03 的文件树（brief）把 `NodeBuilder` 放叶子文件，且 P02 §3.3 已固化「模块根不定义 item」；两处约束冲突时按 P01 §4.5 自己的判据「公开路径 = 类型实际定义所在的文件路径」处理 |
| 修订后设计 | phase-03 §4.1 的路径表；P08 的应用层迁移按 `node::builder::NodeBuilder` 引用                                                                                                 |

### F-35 公开面首次包含第三方 crate 类型（`Style.inner: taffy::Style<SmolStr>`）

| 项         | 内容                                                                                                                                                                                                                                                                                     |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 设计 §4.9.3 的草图 `pub struct Style { pub inner: taffy::Style, … }`；D24/I17 要求引擎只暴露 `Style`/`StylePatch`/`bind_style`                                                                                                                                                           |
| 偏差       | 照草图**暴露** `pub inner: taffy::Style<SmolStr>`（类型参数必须写全 `SmolStr`：`taffy::Style` 的默认参数是 `DefaultCheapStr`，两者不是同一类型）。应用层 crate（`base_ui`）必须自行声明 `taffy = { workspace = true }` 才能构造它                                                        |
| 原因       | ①不暴露就必须为 40+ 个布局属性提供引擎自有访问器 —— 那正是 D24 明令只允许存在于应用层的「builder/DSL」；②`taffy` 已是引擎硬依赖且 workspace 精确钉 `=0.15.0`（P01 §5.3④），版本不会漂移；③`taffy` 的 `Style` 是**布局属性的唯一事实**（I5 的节点树即布局树），包一层再翻译一遍是双份真相 |
| 修订后设计 | phase-03 §10.1；P06 追加的引擎自有字段（圆角/clip/阴影/字体族）走 `Style` 的新字段（additive），不改 `inner` 的语义。V-23 跟踪「additive 是否被结构体字面量构造挡住」                                                                                                                    |

### F-36 不新增 `paint::style::Border`，复用 `paint::primitive::Border`

| 项         | 内容                                                                                                                                                                                                                             |
|------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | P01 §4.5：`Style` `StylePatch` `Border` `Cursor` → `ui_core::paint::style::*`（`src/paint/style.rs`）；P03 的 brief 同样把 `Border` 列进 `paint/style.rs`                                                                        |
| 偏差       | **`paint/style.rs` 不定义 `Border`**；`StylePatch.border: Option<paint::primitive::Border>`（P02 已有的 `{ width: f32, color: Color }`）                                                                                         |
| 原因       | P02 §4.2 的路径表已把 `Border` 归入 `paint::primitive::*`（它是 `QuadPaint::Solid` 的字段类型，属 I2 数据契约）。再定义一个 `paint::style::Border` 会出现两个同义类型 → 「一条边框两种真相」，且 P06 的 style→paint 每次都要转换 |
| 修订后设计 | phase-03 §10.2；P01 §4.5 的 `Border` 行以本条 + P02 §4.2 为准（历史正文不改）                                                                                                                                                    |

### F-37 `NodeBuilder::build` 返回 `Result<NodeId, EngineError>`

| 项         | 内容                                                                                                                                                                                                                                                                                                       |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 设计 §3.10 / §4.9.3 / 附录 A：`pub fn build(self, tree: &mut NodeTree, parent: NodeId) -> NodeId`                                                                                                                                                                                                          |
| 偏差       | **`-> Result<NodeId, EngineError>`**，唯一错误是 `EngineError::StaleNode { node: parent }`（父节点已卸载）                                                                                                                                                                                                 |
| 原因       | ①挂到已卸载的容器是应用层可触发的真实错误（`Window::tree_mut` 允许 `remove`）；②静默丢弃子树会变成 stale drawing 且无诊断（违反 I10 的「不静默降级」精神）；③`StaleNode` 需要真实生产者（P01 §6.2.2 #7 的 payload 是 `NodeId`），否则该变体在 P03 无构造点；④冻结表不允许新增「`ParentMissing`」之类的变体 |
| 修订后设计 | phase-03 §12.1/§12.2；应用层写法 `.build(&mut tree, parent)?`。P08 的 `base_ui` 在 `Result` 上传播（挂载失败 = 应用 bug，日志级别 error）                                                                                                                                                                  |

### F-38 `Scene` 的公开签名按可见性重构（`lane → lane_primitives`、`lane_mut` → `push`）

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | P02 §7.2 的「P03 落地时的形状（已冻结）」：`pub fn lane(&self, id) -> Option<&Lane>`、`pub(crate) fn lane_mut(&mut self, id) -> Option<&mut Lane>`                                                                                                                                                                                                                                                                                                                                                                                                                       |
| 偏差       | ①`lane` 改为 **`pub fn lane_primitives(&self, id: LaneId) -> Option<&[Primitive]>`**（**不**再返回 `&Lane`，见裁决 ③ 的方案②）；②`lane_mut` **不落地**，改为 **`pub fn push(&mut self, lane: LaneId, primitive: Primitive) -> bool`**；③写入面的可见性见 S-33，构造/派生约束见 F-39                                                                                                                                                                                                                                                                                      |
| 原因       | `Lane` 按 P02 N7/S-15 保持 `pub(crate)`；公开签名里出现 `pub(crate)` 类型会触发 `private_interfaces`（warn-by-default），在 `-D warnings` 门禁下**编译失败**。**已实测**（`%TEMP%\kg-p03-priv-probe`，`rustc 1.97.1`，`--edition 2024 -D warnings`）：`error: type `Lane` is more private than the item `Scene::lane`` / `= note: -D private-interfaces implied by -D warnings`；唯一规避是 `#[allow]`，AGENTS §4 禁止。裁决 ③ 进一步要求取**公开面更小**的方案②（只给图元切片投影；`Lane` 的内部状态——`clip_depth`——不泄漏，P06/P07 在 crate 内另有 `pub(crate)` 通道） |
| 修订后设计 | phase-03 §13.2 的差异表；P02 §7.2 的其余条款（代际、`order`、持久性、`Scene` 不 `Clone`）**不变**                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |

### F-39 `Scene` 不实现 `Clone`/`Default`，且**无公开构造器**（`pub(crate) fn empty()`）

| 项         | 内容                                                                                                                                                                                                                                                             |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | P02 §7.2：`#[derive(Debug, Default)] pub struct Scene` + `pub fn new() -> Self`                                                                                                                                                                                  |
| 偏差       | ①`Scene` **只 `derive(Debug)`**（不 `Clone`、不 `Default`，P02 的「不 `Clone`」保留）；②构造函数为 **`pub(crate) fn empty() -> Self`**（唯一的构造点 = `Window::new`），应用层无法构造；③`Arena` 随之**不再实现 `Default`**                                      |
| 原因       | 裁决 ③ 明确要求「`Scene` 不实现 `Clone`/`Default`」。去掉 `Default` 后 `pub fn new()` 会触发 clippy `new_without_default`（style 组、warn-by-default），而 `#[allow]` 被 AGENTS §4 禁止 → 改名 `empty` 并降为 `pub(crate)`（同时消除「应用层自造 Scene」的口子） |
| 修订后设计 | phase-03 §13.1/§13.2/§17.8-1；门禁 §18-11 新增三条 grep（`engine-internal` 标注、无 `derive(…Default/Clone)`、无 `pub fn new/default`）                                                                                                                          |

### F-40 `App::refresh_styles(window) -> Option<StylePassReport>` 与报告类型

| 项         | 内容                                                                                                                                                                                                                                                                      |
|------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | phase-03 提案期：`pub fn refresh_styles(&mut self) -> usize`（全窗口、只返回被求值的个数）                                                                                                                                                                                |
| 偏差       | ①**按窗口**：`pub fn refresh_styles(&mut self, window: WindowId) -> Option<StylePassReport>`；②新增公开类型 **`StylePassReport { evaluated, layout_dirty, paint_dirty, wake_requested }`**（`Copy`）；③`None` = 该窗口不存在；④pass 结束后 `clear()` 求值栈（panic 兜底） |
| 原因       | 裁决 6：报告要能被 P06 的 `FrameRecord` 直接复用，且帧是**按窗口**推进的；P03 没有「未知窗口」的错误变体（P01 §6.2.2 冻结表），`Option` 是唯一诚实的表达（比「返回全零报告 + 静默吞 bug」好）                                                                             |
| 修订后设计 | phase-03 §5.1（签名 + 报告字段语义）、§14.4（逐字 + 决策表）、§4.1/§4.2（路径与白名单闭包）、§17.9-4/5/7/8/11                                                                                                                                                             |

### S-23 P03 的落地判据 =「§1.3 白名单内 **或** 有非 test 消费者」

| 项   | 内容                                                                                                                                                                         |
|------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | 与 P02 §3.2（S-08）同源：`pub(crate)` item 必须有非 test 消费者；`pub` item 必须在 §1.3 白名单内或按其传递闭包登记（phase-03 §4.2）。**唯一例外**是 `Scene` 的写入面（S-33） |
| 理由 | `dead_code` 是编译期硬门禁（`-D warnings`）；而白名单是「不越界」的产品约束。两者同时满足才落地                                                                              |
| 执行 | phase-03 §4.3 的逐条审计表（实现时必须逐项对齐）；门禁 §18-8 的 grep                                                                                                         |

### S-24 `Application` / `AppHandle` 推迟 P08/P09

| 项   | 内容                                                                                                                                                                                                          |
|------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | P03 **不落地** `Application`（`run(build)` 需要平台事件循环）与 `AppHandle`（需要 `PlatformDispatcher`）。`App` 侧命名 **`runtime_handle(&self) -> &tokio::runtime::Handle`**（借用；字段名 `runtime`）       |
| 理由 | P03 没有事件循环/派发器 → 两者无任何非 test 消费者，落地即 `dead_code`，或被迫写空壳（AGENTS §1.2/§4 禁止）。**命名由裁决 ① 定死**：不占用 `handle()`，P09 加 `App::handle() -> AppHandle` 时是纯新增、零破坏 |
| 影响 | §1.3 白名单里的这两个名字仍属「**允许**但不要求本阶段落地」；P08/P09 落地时补齐（`AppHandle` 的语义见设计 §1.4 的跨线程入口）                                                                                 |
| 交接 | P09 的 `PlatformDispatcher::wake(FrameWake)` 取代 `App::take_wake_request` 的**同一处调用点**（phase-03 §14.3）                                                                                               |

### S-25 信号/派生值不回收（与 `App` 同寿），`SignalId` 无代际

| 项   | 内容                                                                                                                                                                                                                                            |
|------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文 | 设计 §3.9：「Signal 值：显式 `StateStore::drop(signal)`，引用计数（应用层持有句柄），无引用即释放」                                                                                                                                             |
| 决策 | **不回收**：`values: Vec<SignalSlot>` 只增不减；`SignalId`/`DerivedId` 是纯 `u32`，**不带代际**                                                                                                                                                 |
| 原因 | `Signal<T>` 被冻结为 `Copy` 句柄（设计 §3.2 + phase-03 §7.2）→ **无法**做引用计数（Copy 无法观测「最后一个句柄」）。没有删除就没有槽位复用，代际无用。应用的状态集是**有界**的（一个交易终端数百到数千信号），内存上界可算：`SignalSlot` ≈ 32 B |
| 影响 | 设计 §3.9 的该行**作废**；`SignalId` 不需要 `StaleSignal` 类错误（也就不会破坏 P01 §6.2.2 的 17 变体冻结表）                                                                                                                                    |

### S-26 派生环检测移到运行时求值栈，`read_derived` 返回 `Result`

| 项           | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
|--------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文         | 设计 §3.9：「`Derived` 链在注册时做 DFS 检测环，命中即 `EngineError::CyclicDerived`，**拒绝注册**」                                                                                                                                                                                                                                                                                                                                                              |
| 决策         | 环检测在 **`ReadCtx::eval_derived` 的求值栈**上做：`EvalStack.contains(id)` → `Err(EngineError::CyclicDerived { derived })`。`App::derived` 永不失败；`ReadCtx::read_derived` 返回 `Result<T, EngineError>`。**入栈/出栈由 `EvalGuard`（拥有 `&mut EvalStack`，`Drop` 里 `pop`）保证 panic 安全**；**环检测先于 `take_compute`** → 求值失败不污染缓存（`dirty` 与 cache 均不动），闭包 panic 后该派生 `compute == None`，后续读返回 `CyclicDerived` 而不是 panic |
| 原因         | 注册期 DFS **不可行**：①闭包读哪些信号/派生，在闭包运行前不可知；②注册时新派生尚不存在于任何闭包里（自引只能在运行期通过 `Rc<RefCell<Option<_>>>` 等间接建立），因此「注册时一定有环」这个前提不成立                                                                                                                                                                                                                                                             |
| 代价         | 派生读派生时要显式处理 `Result`（可 `expect`/`unwrap_or_default`）。绝大多数派生只读信号（不失败），不受影响                                                                                                                                                                                                                                                                                                                                                     |
| 备选（已否） | 「派生不得读派生」可从结构上消除环 —— 但会丢掉设计 §3.9 的派生链（行情派生指标需要它）                                                                                                                                                                                                                                                                                                                                                                           |

### S-27 删除 `versions`（独立向量与每槽位字段都删）

| 项   | 内容                                                                                                                                                                                                                                                                                                                                    |
|------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文 | 设计 §3.2：`StateStore { values: HashMap<SignalId, Box<dyn Any>>, versions: Vec<u64> }`（「按 `SignalId` 索引，写入即 +1」）                                                                                                                                                                                                            |
| 决策 | **两者都删**：`versions` 向量不存在；`SignalSlot { value }` 也不再保留 `version: u64`（裁决 ②：「若没有读者则删除该字段，不要为测试保留」）                                                                                                                                                                                             |
| 原因 | 本设计的失效是 **push**（`App::write` 直接标脏），staleness 由 `DerivedSlot.dirty` / `Node.dirty` 表达 —— 没有任何代码做「版本比较」。核对 P03 的全部非 test 读者：等值短路由值比较完成；`StylePassReport` 只统计 `evaluated`/`layout`/`paint`/`wake`；帧记录属 P06 → **零读者** ⇒ 保留即 `dead_code`（非 test 构建）+ 违反 AGENTS §2.1 |
| 影响 | 「写相等值时是否失效」由 `App::write` 的 `PartialEq` 短路回答（相等**不**失效，phase-03 §7.5）；`SignalSlot` 尺寸 32 B → **24 B**                                                                                                                                                                                                       |

### S-28 `GEOMETRY` / `REUSABLE` 位在 P03 无读者（位已定义、有写者）

| 项   | 内容                                                                                                                                                                              |
|------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 事实 | `DirtyFlags` 的 6 位在 P03 全部定义；`LAYOUT`/`PAINT`/`A11Y`/`ORDER` 有写者**且**有读者；`GEOMETRY` 无写者（P08）；`REUSABLE` 只在 `mark_*` 里被 `remove`（常量被读），读者在 P06 |
| 决策 | 保留 6 位定义（设计 §3.4 冻结的位布局），不加占位/不删位                                                                                                                          |
| 理由 | 位是 `pub const`，不触发 `dead_code`；`remove(REUSABLE)` 真实读取该常量。删位会让 P06/P08 重新编号 → 破坏「位定义冻结」                                                           |

### S-29 `Children` / `Slot` / `NodeHandle` 推迟 P08

| 项   | 内容                                                                                                                                                                                                                                 |
|------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文 | §1.3 白名单的「节点」行列出 `NodeId`、`NodeBuilder`、`NodeHandle`、`Slot`、`Children`                                                                                                                                                |
| 决策 | P03 只落 `NodeId` 与 `NodeBuilder`；其余三个推迟 P08                                                                                                                                                                                 |
| 原因 | 它们是**声明式建树的语法层**（子节点集合 / 命名插槽 / 可保存句柄），消费者是 `base_ui` 的组件组合（P08）。P03 的等价能力是 `NodeBuilder::build(tree, parent)` + `NodeTree::append_child`；`NodeId` 本身已是完整句柄（`Copy` + 代际） |
| 影响 | 白名单不要求本阶段落地（同 S-24）                                                                                                                                                                                                    |

### S-30 `StaleNode` / `CyclicDerived` 的实现 Phase 由 P04 改为 **P03**

| 项   | 内容                                                                                                                       |
|------|----------------------------------------------------------------------------------------------------------------------------|
| 原文 | P01 §6.2.2 冻结表 #7/#8 的「实现 Phase」列 = P04（当时的相位编号里 P04 是节点/状态层）                                     |
| 变更 | 实际落地 Phase = **P03**（相位重排：P03 = 节点状态核心）。**变体名、payload、`Display` 文案、`source() == None` 全部不变** |
| 理由 | 冻结表是按当时的总计划编号写的；相位重排后「跟着消费者走」（P02 §3.2 的同一原则）。payload/文案不变 ⇒ 对外契约无变化       |
| 追加 | `error.rs` 的 `all_variants()` 必须补两个新变体（`engine_error_source_is_none` 用例覆盖）                                  |

### S-31 P03 交付 **81** 个内联单测（brief 估算 40–60）

| 项   | 内容                                                                                                                                                                                                                                                                                                                             |
|------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 估算 | brief 的 40–60                                                                                                                                                                                                                                                                                                                   |
| 实际 | phase-03 §17 列出 **81**（id 7 / signal 12 / panic 安全 1 / dependency 8 / dirty 11 / tree 9 / builder 6 / style 3 / scene 9 / app 11 / window 4）                                                                                                                                                                               |
| 理由 | 每条对应一条独立不变量、终止条件或端到端断言（`Arena` A1–A5、依赖图 D1–D5、§3.4 的 8 行失效规则、跨窗口/唤醒/`A11Y`）。P02 交付 60，而 P03 的语义面更宽；平均每例 ~8 行。裁决后新增 5 条：等值短路风险（`Rc<Cell>` 反例）、`EvalGuard` 的 panic 安全、`refresh_styles` 未知窗口、`clear_frame` 的 clip 复位、叶子父的 debug 断言 |
| 兜底 | 排期紧张时按「每张表从上到下」交付到 52 个（覆盖全部机制性断言，表尾为幂等/边界补充）                                                                                                                                                                                                                                            |

### S-32 `Owner::Node` 携带 `WindowId`（+ derive 与去重/序规则）

| 项   | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
|------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文 | 设计 §3.3：`dependents: HashMap<SignalId, NodeIdSet>`、`tracker.record(signal.id, self.node)` —— 依赖边只带 `NodeId`                                                                                                                                                                                                                                                                                                                                                    |
| 决策 | `Owner::Node { window: WindowId, node: NodeId }`；`NodeIdSet` 改为 `OwnerSet = SmallVec<[Owner; 2]>`（**不是** ahash `HashSet`，**不是**位图）；`Owner` 派生 `Clone, Copy, Debug, PartialEq, Eq, Hash`，**不**实现 `Ord`；去重 = 插入前 `contains` 线性查重 + 删除时 `retain` 清历史重复；**序 = 首次登记序**（不重排）；`replace_deps` 的结果**集合**与输入顺序无关，**迭代序**可能随 `new` 的顺序变化（对正确性无影响：失效只做标脏与 damage 并集，两者都与顺序无关） |
| 原因 | ①`NodeId` 是**窗口内** arena 索引：两个窗口可以各自有 `n0.1`，不带窗口会在 `App::write` 里误标另一个窗口的同号节点（stale 之外的第二类错误）；②位图键是稠密 index，而 `NodeId` 的 index 会被复用（装卸），代际不在位图里 ⇒ 陈旧句柄的位会命中复用后的新节点；③稳态读者数 1–3，`SmallVec` 内联零分配、迭代序确定、差分是两条小切片比较；④排序需要 `Ord`，而 P03 没有任何排序消费者（诊断要稳定序时由调用方排序）                                                         |
| 代价 | `Owner` 16 B（`u32` + 8 B + 判别）；mount 期主题类信号的集合插入是 O(k) 查重（V-26）                                                                                                                                                                                                                                                                                                                                                                                    |

### S-33 `Scene` 的写入面在 P03 是 `pub`，且 doc 首行标注 engine-internal

| 项                  | 内容                                                                                                                                                                                                                           |
|---------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文                | P02 §7.2：「以下 `pub(crate)`，消费者 = P03 的 paint 循环」；S-10：「`Scene` 只读 + `Canvas` 唯一公开写入口」                                                                                                                  |
| 事实                | P03 **没有** paint 循环（绘制指令生产属 P06）→ `new_lane`/`release_lane`/`lane_mut`/`clear_frame`/`set_order` 在 crate 内**零调用者** ⇒ `pub(crate)` 必触发 `dead_code`                                                        |
| 决策                | 改为 **`pub`**：`new_lane`/`release_lane`/`push`/`clear_frame`/`set_order`/`clear_order` 全部公开，**每个方法的 doc 首行**写 `engine-internal, 应用层不得调用`；`lane_mut` 不落地（F-38）；构造/派生约束见 F-39                |
| 为什么这不违反 S-10 | S-10 说的是「应用层的**唯一公开写入口**是 `Canvas`」—— 应用层依然拿不到 `&mut Scene`（`Window::scene()` 只读）、`Scene` 不 `Clone`/`Default` 且**无公开构造器**、`Window` 无 `set_scene`。`pub` 的是**方法**，不是可达的写路径 |
| 交接                | P06 的 paint 循环成为首个**引擎内部**调用者；届时若要把这 6 个方法降回 `pub(crate)`，属于可见性收紧（有真实调用者），需另记偏差                                                                                                |

### S-34 `App::refresh_styles` 作为 P03 的样式求值 pass 落地（按窗口 + 报告）

| 项                   | 内容                                                                                                                                                                                                                                                                                                                                               |
|----------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 事实                 | P03 不做 paint pass（绘制指令生产属 P06）。但 `ReadCtx`/`ReadTracker`/`DependencyGraph::replace_deps`/`Derived` 求值/`EvalStack`/`EvalGuard`/`StylePatch` 比较/`affects_layout`/`Node.deps`/`bind_style` 字段这 10 余项能力**没有**其它非 test 消费者                                                                                              |
| 决策                 | 落地 `App::refresh_styles(&mut self, window: WindowId) -> Option<StylePassReport>`（裁决 6 的细化，见 F-40）：按 `NodeTree::order()` 遍历该窗口，对 `PAINT` 且带 `bind_style` 的节点求值、替换依赖、写回 patch、按 `affects_layout` 升级 `LAYOUT`、按 `interactive`/`visible` 变化标 `A11Y`、最后清 `PAINT`；进入时 `clear()` 求值栈（panic 兜底） |
| 为什么不是预防性抽象 | 它是 §4.9.4 步骤 4 的**前半**（「Paint 阶段重新求值 `bind_style` 得到 `StylePatch`」），且必须在**布局之前**运行（`affects_layout` 会改变布局输入）。P06 的 paint 阶段在重建 lane 前调用同一实现 → 不是过渡层，是帧流水线的固定一段                                                                                                                |
| 边界                 | **不产生任何 Scene 图元**；不重建 lane、不算 damage（`!REUSABLE` 与 `Node.damage` 交给 P06）；不读 `bind_text`/`bind_a11y`（P05/P11）                                                                                                                                                                                                              |
| 测试                 | phase-03 §17.9 的 11 条（只重算脏节点、幂等、升级 `LAYOUT`、替换 deps、未知窗口、跨窗口、`A11Y`、报告的三个标志）                                                                                                                                                                                                                                  |

### S-35 `App::write` 的约束与「等值短路」的语义/风险

| 项                         | 内容                                                                                                                                                                                                                                                                                               |
|----------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策                       | `App::write<T: Clone + PartialEq + 'static>(&mut self, signal: &Signal<T>, value: T) -> bool`；`Signal<T>`/`Transient<T>`/`Derived<T>` 句柄与 `ReadCtx::read` **无** `PartialEq` 约束；短路用 `owned_ref`（借用比较，不克隆）。P03 **不提供** `Signal::set`；若将来提供，bound 必须与 `write` 相同 |
| 语义                       | `false` = 值未变（不标脏、不置唤醒）；`true` = 值变了（已失效）。返回值**不**表示「有节点被标脏」                                                                                                                                                                                                  |
| 风险（必须写进应用层契约） | `T` 内部可变（`Rc<RefCell<_>>`/`Cell`/`Arc<Mutex<_>>`）且 `PartialEq` 只比外层 ⇒ 「内部变了但不重绘」。**应用层必须用不可变值语义**；风险由 phase-03 §17.2-6 的反例测试固化（R9）                                                                                                                  |
| 原因                       | 高频行情下「同一报价重复推送」极常见，短路省掉整条失效链路（I3 的「空闲零帧」也依赖它）；`PartialEq` 只在写路径，不传染读/句柄                                                                                                                                                                     |

### S-36 `NodeBuilder::build` 的错误语义冻结（`StaleNode` 是唯一 `Err`）

| 项   | 内容                                                                                                                                                                                                                                                                                                                                                                                       |
|------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | `build(self, tree, parent) -> Result<NodeId, EngineError>`，`Err` 只有 `StaleNode { node: parent }`（父不存在/代际不匹配）。其余情形：叶子父（`Canvas`/`Geometry`）= `debug_assert!` + release 放行（子树不参与布局）；结构环（仅 `append_child` 可能）= `debug_assert!` + 不改结构 + `Ok(())`；`Style` 非法 = `Ok`（交给 P04/P05 的布局校验）；OOM = abort（不引入 `try_reserve` 错误面） |
| 原因 | P01 §6.2.2 的冻结表没有「父不可容子」变体，新增会破坏契约；而叶子父是应用层建模错误，debug 构建立刻暴露、release 下行为**安全且已定义**（子树不参与布局，不会 UB/画错）                                                                                                                                                                                                                    |
| 落地 | phase-03 §12.1 的错误语义表 + `# Panics`、§12.2 伪代码的 `debug_assert!(!p.kind().is_leaf())`、§11.2 的 `NodeKind::is_leaf`、§17.6-6 用例                                                                                                                                                                                                                                                  |

### S-37 `App::runtime_handle` 的命名与返回形态

| 项   | 内容                                                                                                                                                                                |
|------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | `pub fn runtime_handle(&self) -> &tokio::runtime::Handle`（**借用**，不 clone）；`App` 的字段名同步为 `runtime`                                                                     |
| 原因 | 裁决 ①：不占用 `handle()` 这个名字 —— P09 加 `App::handle() -> AppHandle` 时是**纯新增、零破坏**；返回借用避免每次取句柄都 `clone`（`Handle: Clone` 是 O(1) 的 Arc 增加，但没必要） |
| 交接 | P09 的 `PlatformDispatcher` 落地后，`take_wake_request` 的调用点换成 `wake(FrameWake)`，`runtime_handle` 保持不变                                                                   |

### 八·待复核项（P03 新增）

| 编号 | 内容                                                                                                                 | 复核 Phase | 触发条件 / 处置                                                                                                            |
|------|----------------------------------------------------------------------------------------------------------------------|------------|----------------------------------------------------------------------------------------------------------------------------|
| V-23 | `Style` 的 P06 字段能否安全 additive 加（若应用层已按结构体字面量构造 `Style`，加字段即破坏构造点）                  | P06        | style→paint 落地时若冲突，改为 `Style { inner, paint: PaintStyle }` 子结构                                                 |
| V-24 | 派生闭包误读 `Transient` → 永久 stale（写 transient 不置 `dirty`）                                                   | P13        | 动画/滚动落地后若出现，加 `debug_assert` 或在求值期标记「读过 transient」                                                  |
| V-25 | `App::write` 的相等比较成本（`T` 大时 O(n)）**与等值短路的语义风险**（`T` 内部可变而 `PartialEq` 只比外层 ⇒ 不重绘） | P18 / P08  | 成本：L4 基准显示写路径占比显著 → 拆细信号；语义风险：写进应用层契约（S-35/R9），`base_ui` code review 约定 + P08 迁移文档 |
| V-26 | `OwnerSet` 的 mount 期 O(k²) 插入（主题类信号聚合上千读者）                                                          | P18        | 5000 节点 mount >1 ms → 改 `ahash::HashSet`（接口不变）                                                                    |
| V-27 | 冒泡终止过保守（`Dimension::Auto` 一律视为依赖子内容）                                                               | P04        | `set_layout` 比较新旧 `size`：相同则不再向上（§4.4）；P03 的判定表是它的上界                                               |
| V-28 | `order_pos` 是 O(n) 线性扫描（`Vec::insert`/`drain` 的定位）                                                         | P13        | 虚拟列表每帧上千次结构变更 → 加 `Node.order_index`（additive 字段）                                                        |
| V-29 | 节点卸载后 `dependents` 残留死 owner（靠写路径 `retain` 惰性剔除）                                                   | P08        | 大量增删节点时内存/写路径扫描变慢 → `App::remove_node(window, node)` 精确退订                                              |
| V-30 | `size_of::<Node>()` 实测（内联 `taffy::Style<SmolStr>`，估计 ~800 B）                                                | P04        | 超 1024 B 或 5 万节点场景内存吃紧 → `style: Box<Style>`（不改公开面）                                                      |

### F-41 `NodeId.generation` 用 `NonZeroU32`（`Option<NodeId>` 保持 8 B）

| 项   | 内容                                                                                                                                                                                                                                                                                                          |
|------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文 | phase-03 §6.1：`pub struct NodeId { index: u32, generation: u32 }`，并把「`generation` 恒在 `1..=u32::MAX`」写成不变量                                                                                                                                                                                        |
| 偏差 | `generation: std::num::NonZeroU32`；`generation()` 仍返回 `u32`（`.get()`），**公开面不变**                                                                                                                                                                                                                   |
| 原因 | 实现时 `size_of::<Option<NodeId>>()` 实测 **12 B**（两个 `u32` 没有 niche ⇒ `Option` 多一个 tag）。文档 §17.5-8 要求它 8 B（`Node.parent: Option<NodeId>` 零开销），而 `NonZeroU32` 把「0 永不是合法代际」提升为**类型即证明**：`Option<NodeId>` 回到 8 B，且 `ArenaKey::from_parts` 成为该不变量的唯一守门点 |
| 影响 | 无公开面变化；`NodeId::for_test(_, 0)` 会在测试里 panic（0 本就非法）                                                                                                                                                                                                                                         |

### F-42 `ExpandedDimension::Calc` 是**无条件**分支（`#[cfg(feature = "calc")]` 不可表达），且变体名是 `Calc`

| 项   | 内容                                                                                                                                                                                                                                                                                                                                                                                                          |
|------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文 | phase-03 §9.3（N3）：`#[cfg(feature = "calc")] ExpandedDimension::FitContentCalc(_) => false`                                                                                                                                                                                                                                                                                                                 |
| 偏差 | ①变体名实际是 **`ExpandedDimension::Calc(_)`**（taffy 0.15.0 源码）；②写成**无条件** match 分支                                                                                                                                                                                                                                                                                                               |
| 原因 | ①按 `E:\.cargo\registry\...\taffy-0.15.0\src\style\dimension.rs` 的 `ExpandedDimension` 定义，calc 变体名是 `Calc`；②我们的 crate **不能** `#[cfg]` 依赖的 feature（`feature = "calc"` 只匹配**本 crate** 的 feature），taffy 的 `calc` 由 workspace 精确开启（`features = [..., "calc", ...]`）⇒ 该变体恒存在，无条件列出即可；若将来关掉 calc，这里会**编译失败**（显式失败优于悄悄把 `Calc` 归入保守分支） |
| 影响 | 行为与文档一致（`Calc` 不依赖子内容）；记录实现事实                                                                                                                                                                                                                                                                                                                                                           |

### F-43 `NodeTree::insert_child` 收「部件」而不是半成品 `Node`

| 项   | 内容                                                                                                                                                                                                                                                     |
|------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文 | phase-03 §11.2：`pub(crate) fn insert_child(&mut self, parent: NodeId, node: Node) -> Result<NodeId, EngineError>`                                                                                                                                       |
| 偏差 | `insert_child(&mut self, parent, kind: NodeKind, style: Style, bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>)`                                                                                                                         |
| 原因 | `Node` 的全部字段都是 `node::tree` 私有的，`NodeBuilder` 无法构造 `Node`；若要走 `node: Node` 就得给 `Node` 加一个带**占位 id** 的构造器（占位 id 是 `NodeId::from_parts(0,0)`，而 0 不是合法代际），比现状更危险。收部件后 `id` 由 arena 铸出、无中间态 |
| 影响 | 纯 crate 内部签名；调用点只有 `NodeBuilder::build`                                                                                                                                                                                                       |

### F-44 `eval_derived` 的 `take_compute == None` 分支**不加** `debug_assert`

| 项   | 内容                                                                                                                                                                                                                                                                                                                                                                                                   |
|------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文 | phase-03 §7.4 伪代码：`else { debug_assert!(false, "派生值正在求值（环检测被绕过）"); return Err(CyclicDerived…) }`                                                                                                                                                                                                                                                                                    |
| 偏差 | 去掉 `debug_assert!`，直接 `return Err(EngineError::CyclicDerived { derived })`                                                                                                                                                                                                                                                                                                                        |
| 原因 | 这个分支有**两种**到达方式：①环检测被绕过（真不变量破坏）；②上一次求值 **panic** 导致 `compute` 未放回（**已定义的可恢复路径**，N8 明确要求「闭包 panic 后本派生返回 `CyclicDerived`，不得作为环的唯一判据」）。断言会把 ② 变成 debug 崩溃 ⇒ 一个闭包 panic 会让引擎在 debug 构建里**永久不可用**，与「panic 后仍可继续服务」的设计冲突。`tests::eval_guard_pops_on_panic_and_cache_survives` 固化了 ② |
| 影响 | 环真被绕过时也返回强类型错误（而不是断言）；`CyclicDerived` 的 `Display` 已覆盖两种来源                                                                                                                                                                                                                                                                                                                |

### S-38 测试落地实测：**87** 个 P03 用例（文档列 82），三处断言按可测性改写

| 项            | 内容                                                                                                                                                                                                                                                                                              |
|---------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 实测          | `cargo test -p ui_core` = **190 passed**（P01+P02 = 103，P03 = **87**）；`--release` = **184**（6 个 `#[cfg(debug_assertions)]` 用例不编译：`paint/canvas.rs` 2 个 P02 用例 + `paint/scene.rs` 2 + `node/tree.rs` 1 + `node/builder.rs` 1）                                                       |
| 分布          | `id` 7、`signal` 13、`dependency` 9、`dirty` 13、`tree` 9、`builder` 6、`style` 3、`scene` 9（另加 P02 的 6 个 `Lane` 用例）、`app` 12、`window` 4、`error` 2（新增 `StaleNode`/`CyclicDerived` 的 Display 与 `all_variants` 覆盖）                                                               |
| 比文档多 2 条 | ①`signal::split_borrow_returns_disjoint_views`（`StateStore::split` 的借用契约，B1/B2 的落地证据）；②`app::eval_guard_pops_on_panic_and_cache_survives` 从 `dependency.rs` 移到 `app.rs`（需要读 `App::eval_stack`，那里才有字段访问权）                                                          |
| 断言改写 ①    | phase-03 §17.4-10 原写「`Canvas`/`Geometry` 节点 `mark_layout` → 父不含 `LAYOUT`」。但 `NodeBuilder::build` **禁止**给叶子父挂子节点（§12.1），该场景不可达 ⇒ 改为谓词级断言 `leaf_kinds_do_not_depend_on_children`（叶子 `depends_on_children == false`，且它们自己的 `mark_layout` 仍标本节点） |
| 断言改写 ②    | phase-03 §17.8-5 原要把「超过 `MAX_CLIP_DEPTH` 的 `#[should_panic]`」放在 `scene.rs`；P02 的 `paint/scene.rs` 已有 `depth_cap_is_debug_assert` 覆盖同一断言 ⇒ P03 的用例只断言 `Scene::push` → `Lane::push` 的转发与 clip 图元计数，不重复断言上限                                                |
| 断言改写 ③    | phase-03 §17.6-4 原写「`refresh_styles` 后 `style_patch()` 是闭包算出的值」——那是 `App` 级行为，`builder.rs` 拿不到 `App` ⇒ 用例改为断言「闭包确实存进节点、build 不预求值」，`refresh_styles` 的求值面由 `app.rs` 的 5 个用例覆盖                                                                |

### S-39 代码审查期（A/B）新增与替换的用例清单

| 类别                            | 用例                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             | 归属                              |
|---------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-----------------------------------|
| **新增（A-B2，D16/I3 端到端）** | `app::write_isolates_sibling_nodes`（同窗口两节点各读一信号，写 A 后 B 的 `DirtyFlags` 全量不变且 `take_wake_request()` 为真）、`app::write_only_derived_does_not_wake`（只有派生失效 ⇒ 不唤醒，且 `report.wake_requested == false`）、`app::write_marks_across_windows`（两窗口各一读者，写一次两者都脏）                                                                                                                                                                                                                                                       | `app.rs`                          |
| **新增（A-B3/B-N4/A-N3 回归）** | `app::bind_style_panic_clears_ghost_deps`（成功求值 → panic → 再写旧信号不唤醒）、`app::create_window_ids_are_unique_and_monotonic`（1/2/3 且 `windows()` 带出 id）、`app::close_window_unknown_returns_false`、`tree::append_child_marks_moved_subtree_paint`（重挂后整棵子树 `PAINT` 且 `REUSABLE` 被清）、`tree::damage_unions_prev_and_current`、`tree::remove_marks_parent_structure`、`dirty::mark_a11y_keeps_damage`                                                                                                                                      | `app.rs` / `tree.rs` / `dirty.rs` |
| **强化（原断言过弱）**          | `dirty::repeated_mark_layout_is_idempotent`（加 `damage == prev ∪ layout` 的内容断言）、`dirty::layout_escalation_marks_fixed_size_node_ancestors`（加两个谓词输入断言 + 早停分支复跑）、`dirty::leaf_kinds_do_not_depend_on_children`（加 auto/fixed/混合三例）、`app::refresh_styles_escalates_layout_when_patch_affects_layout`（`false→true` 与 `true→false` 双向）、`signal::equal_but_interior_mutable_value_is_not_repainted`（改为「不重绘 + 不唤醒」）、`signal::derived_cycle_is_reported_not_panicking`（去掉同义反复，改断言「依赖里没有未读信号」） | 各文件                            |
| **语义变更（B-N4）**            | `NodeTree::append_child` 重挂时新增 `mark_subtree_paint`（逐节点 `touch(PAINT)` ⇒ 清 `REUSABLE` + 并 damage）                                                                                                                                                                                                                                                                                                                                                                                                                                                    | `node/tree.rs`                    |
| **语义变更（A-N3）**            | `NodeTree::touch` 只在 flags 含 `LAYOUT`/`PAINT` 时 `remove(REUSABLE)` + `accumulate_damage()`；`mark_a11y` 不再产生 damage                                                                                                                                                                                                                                                                                                                                                                                                                                      | `node/tree.rs`                    |
| **语义变更（A-N2）**            | `App::refresh_styles` 的「闭包已丢」分支显式退订幽灵边：`graph.replace_deps(owner, ghost_deps, &[])` + `set_deps(∅)`                                                                                                                                                                                                                                                                                                                                                                                                                                             | `app.rs`                          |
| **API 对齐（A-B3/A-N8）**       | `App::windows() -> (WindowId, &Window)`；`next_window_id` 从 1 起（0 非法）；`signal`/`derived`/`transient` 加 `Clone` bound；`Node` 补 7 个公开只读访问器（删 `damage_for_test`/`parent_for_test`）；`LaneId::{index, generation}` 降为 `#[cfg(test)]`；`Arena::len` 用 `saturating_sub`                                                                                                                                                                                                                                                                        | 各文件                            |
| **工程化（A-N4/A-N5）**         | `#[cfg(debug_assertions)] fn debug_assert_order_matches_tree`（`order` 必须是 DFS 先序全序），在 `insert_child`/`append_child`/`remove` 末尾调用；`Arena::len` 溢出安全                                                                                                                                                                                                                                                                                                                                                                                          | `node/tree.rs` / `node/id.rs`     |

> **A-N6 注记**：`error.rs` 的模块文档把 `GeometryRegister` 的落地 Phase 统一为 **P09**（与 P01 §6.2.2 冻结表一致）；
> P01 的该行无需改动，P09 落地时按其表执行即可（此条只是把 P03 文档里的旧笔误纠正）。

---

## 九、Phase 04 布局与命中测试（2026-11-12）

> 关联文档：`design/phase-04-layout.md`（本节 = 该文档 §20 的落地条目）。
> **项目级授权 A-01 继续适用**；一～八 全部条目继续有效，本节不重复，只**追加**（不改写任何历史条目）。
>
> **事实基线**：`taffy 0.15.0`（`tree/traits.rs` 的 7 个 trait、`compute/{mod,leaf,oof,flexbox,grid,block}.rs`、
> `tree/{cache,layout,node,taffy_tree}.rs`、`style/mod.rs`、`style/available_space.rs`），逐条行号见 phase-04 §2。
> **未使用 web 检索**。唯一的语言行为/API 验证是 `%TEMP%\kg-p04-probe`（13 文件 / 3110 行 / 40 个单测，
> `rustc 1.97.1` + `clippy 0.1.99`，`-D warnings` 与 `--release` 全绿），见 phase-04 §2.4。
>
> **5 项裁决**（phase-04 §18 提案；上级 2026-11-12 裁决：**待裁决**，本文档先按建议形态落地）：
> ①不实现 `RoundTree`；②入口 = `Window::compute_layout`（不加 `App::layout`）；③`HitResult` 落 `layout::hit_test`；
> ④`LayoutBox` 加 `border`；⑤`layout_override` 由 builder 强制 `position: Absolute`。

### F-45 `HitResult` / `ClipChain` 的公开路径改为 `layout::hit_test::*`

| 项         | 内容                                                                                                                                                                                                                                                                                                   |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | P01 §4.5 公开路径表：`HitResult` → `ui_core::event::*`（定义文件 `src/event.rs`）                                                                                                                                                                                                                      |
| 偏差       | `HitResult` 定义在 **`src/layout/hit_test.rs`**，公开路径 **`ui_core::layout::hit_test::HitResult`**；新增公开类型 **`ClipChain`**（同文件，`HitResult::clip_chain` 的字段类型）                                                                                                                       |
| 原因       | ①P04 的文件树把命中测试放 `layout/hit_test.rs`（brief 明确），而 `event` 模块（P10）此时不存在，P04 不能为一个路径去建空模块；②`HitResult` 的内容全部是**布局坐标**的产物（`bounds`/`clip_chain`/`local_point`），归属布局更贴合职责；③按 P01 §4.5 自己的判据「公开路径 = 类型实际定义所在的文件路径」 |
| 修订后设计 | phase-04 §4.1 的路径表；P10 落 `event` 模块时**不再**定义 `HitResult`（避免同义类型），只在 `InputEvent` 分发里消费它                                                                                                                                                                                  |

### F-46 `LayoutBox` 增加 `border: Edges`

| 项         | 内容                                                                                                                                                                                                                                                            |
|------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | P03 §11.3：`LayoutBox { bounds: Rect }`（「P03 只有 `bounds`」，`baseline`/其他字段由后续 Phase additive 加）                                                                                                                                                   |
| 偏差       | 增加 **`pub border: Edges`**（来源 taffy `Layout.border`，四边宽）；`LayoutBox` 从 16 B → **32 B**（probe 实测）                                                                                                                                                |
| 原因       | `Style.clip = Clip::Bounds` 的裁剪盒必须是 CSS 的 **padding box** = `bounds.inset(border)`。没有 `border` 就没有 P04 的正确 clip 链（只能退化成「裁到 border box」，差一个边框宽的系统性错误）；P06 的背景/圆角内缩同样需要它 → 不是预先设计，是 P04 的真实读者 |
| 修订后设计 | phase-04 §8.1；P03 的结构体字面量构造点（仅测试）随之改成 `LayoutBox { bounds, border }`；`content_size` / `baseline` 仍推迟（V-33 / P05）                                                                                                                      |

### F-47 **不实现** `RoundTree` / `TraverseTree` / `PrintTree`

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
|------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | phase-04 brief 要求为 `NodeTree` 实现 8 个 taffy trait（含 `RoundTree`）                                                                                                                                                                                                                                                                                                                                                                                                                |
| 偏差       | 只实现 **7 个**：`TraversePartialTree` / `LayoutPartialTree` / `LayoutContainingBlock` / `CacheTree` / `LayoutFlexboxContainer` / `LayoutGridContainer` / `LayoutBlockContainer`。`RoundTree` / `TraverseTree` / `PrintTree` **不实现**                                                                                                                                                                                                                                                 |
| 原因       | ①`round_layout` 的唯一调用点是 `TaffyTree::compute_layout_with_measure`（`tree/taffy_tree.rs:976-981`），引擎走低级 `compute_root_layout`，**永不圆整**（P01 §7.5 / F-17）；②`RoundTree` 的 `set_final_layout` / `hoisted_child_count` / `get_hoisted_child_id` 只被 `round_layout_inner` 读（`compute/mod.rs:379-435`）⇒ 落地即无消费者（AGENTS §1.2）；③落地代价是每节点 `final_layout: Layout` = **92 B**（probe 实测），把 `Node` 从 ≈796 B 推到 888 B，逼近 1024 预算（V-30/V-31） |
| 证据       | probe **能**实现（8 个 trait 全部编译通过、40 个用例全绿）→ 这是**决策**，不是能力限制；将来引入「按物理网格圆整」或 `print_tree` 调试时 additive 补 `final_layout` 字段 + 1 个 impl 即可                                                                                                                                                                                                                                                                                               |
| 修订后设计 | phase-04 §5.3 的决策表 + §16-7 的门禁 grep（`round_layout                                                                                                                                                                                                                                                                                                                                                                                                                               |final_layout|impl RoundTree` 必须无输出） |

### F-48 `LayoutContainingBlock` 的 hoisted 列表**必须存储**（taffy 文档只提到 `round_layout` 消费它）

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | taffy `tree/traits.rs:236-239`：「The recorded lists are consumed by `round_layout` … and are also useful for consumers implementing paint/hit-testing traversals」                                                                                                                                                                                                                                                                                        |
| 偏差       | 引擎**必须**存 `hoisted: HashMap<NodeId, SmallVec<[NodeId; 2]>>`，且它的**唯一读者是 P04 的绝对坐标 post-pass**（不是 `round_layout`）                                                                                                                                                                                                                                                                                                                     |
| 原因       | 出流盒的包含块**可以不是它的树父**（CSS：最近的定位祖先；`position: fixed` 甚至恒定是 root）。taffy 给该盒写的 `Layout.location` 相对**包含块 border box**，而引擎的树 DFS 从树父推出不来这个参照系。`add_hoisted_children` 是 taffy 唯一权威地告诉我们「谁是谁的包含块」的地方。probe 用例 `out_of_flow_child_hoisted_to_ancestor_uses_containing_block_origin` 固化了这条（实测 (5,7)：容器 padding 10 但 taffy 的 `absolute_position_offset = border`） |
| 修订后设计 | phase-04 §7.3-A2 / §8.3；hoisted 的三处清理点在 §6.1 的表里                                                                                                                                                                                                                                                                                                                                                                                                |

### F-49 根节点默认 `size: 100% × 100%`

| 项         | 内容                                                                                                                                                                                                                                                                                                             |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 上游文档没有任何「根尺寸」的规定（只假设「窗口尺寸 = 根可用空间」）                                                                                                                                                                                                                                              |
| 偏差       | `NodeTree::new()` 把根的 `style.inner.size` 写成 `Dimension::percent(1.0)` × 2                                                                                                                                                                                                                                   |
| 原因       | `compute_root_layout` 只接受 `available_space`，**不会**把它强加给根（`compute/mod.rs:188-324` 的 `known_dimensions` 只对 block 根自动 stretch）。probe 实测：窗口 200×200 + 根 `size: auto` ⇒ 根 **0×0**、`width: 100%` 的子节点全 0。不修这个，所有依赖「根 = 视口」的用例（命中、damage、百分比布局）都是错的 |
| 修订后设计 | phase-04 §8.4；根默认撑满、应用可改但不该改（rustdoc 写明）                                                                                                                                                                                                                                                      |

### F-50 `MeasureFn` 的输入用 taffy `Size<Option<f32>>` / `Size<AvailableSpace>`，输出用引擎 `Size`

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                              |
|------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | phase-04 brief 建议 `Box<dyn Fn(Size<Option<f32>>) -> Size<f32>>`（只给已知尺寸）                                                                                                                                                                                                                                                                                                 |
| 偏差       | `pub type MeasureFn = Box<dyn Fn(taffy::Size<Option<f32>>, taffy::Size<taffy::AvailableSpace>) -> Size<f32>>`                                                                                                                                                                                                                                                                     |
| 原因       | ①文本换行必须区分 `Definite(w)` / `MinContent` / `MaxContent`（固有尺寸测量），只给 `known_dimensions` 表达不了（`PerformLayout` 时 `known` 恒 `None`）；②形状与 taffy `compute_leaf_layout` 的 `MeasureFunction` **逐字一致**（`compute/leaf.rs:20-27`），适配是零成本转发；③引擎 `Size` 按 P01 §6.3 冻结为纯 `f32`，无法表达 `Option`/三态；`taffy` 类型已在公开面（F-35 先例） |
| 修订后设计 | phase-04 §9.1 / §9.2；`Box<dyn Fn>`（不是 `FnMut`）是「measure 纯函数」的类型级保障                                                                                                                                                                                                                                                                                               |

### F-51 `InvalidMeasure` 的实现 Phase 改为 **P04**；`LayoutInvalid` 不落地

| 项         | 内容                                                                                                                                                                                                                                                                                                            |
|------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | P01 §6.2.2：#9 `InvalidMeasure` 与 #10 `LayoutInvalid` 的实现 Phase 都是 **P05**                                                                                                                                                                                                                                |
| 偏差       | #9 在 **P04** 落地（payload 逐字不变）；#10 **不落地**（继续留空）                                                                                                                                                                                                                                              |
| 原因       | ①measure 适配（`compute_leaf`）在 P04 落地，违规检测的唯一生产点就在这里（`compute/leaf.rs` 会把 NaN 夹掉/被 padding 放大，必须在闭包返回值上检测）；②#10 没有生产者：taffy 低级 API 全部返回 `LayoutOutput`、不返回 `Result`，唯一的后置校验就是 measure 违规，而它已有更精确的变体（AGENTS §1.2「不留占位」） |
| 修订后设计 | phase-04 §14.1 / §14.2；`error.rs` 的 `all_variants()` 增加 #9                                                                                                                                                                                                                                                  |

### F-52 `calc()` 不解析（`resolve_calc_value` 走默认 `0.0`）

| 项   | 内容                                                                                                                                                                                                                                                                                    |
|------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文 | P01 §5.3④ 把 `calc` 列进 taffy 的冻结 features；上游设计未提 calc 的求值                                                                                                                                                                                                                |
| 偏差 | 引擎**不实现** `resolve_calc_value`（走 trait 默认实现，恒返回 `0.0`）；`calc()` 值布局为 0                                                                                                                                                                                             |
| 原因 | `taffy::style::Calc(*const ())` 是**不透明指针**（`style/dimension.rs:106-109`），只暴露 `calc_value() -> *const ()`，taffy 0.15 没有公开求值器；taffy 自带的 `TaffyView` 同样返回 `0.0`（`tree/taffy_tree.rs:400-403`）。要真支持 calc 就得 fork taffy 或自建 calc arena（P05 再评估） |
| 影响 | 应用层不得使用 `Dimension::Calc` / `LengthPercentage::Calc`；`Style.inner` 的 rustdoc 写明；不新增错误变体（没有运行时可判定的信号）                                                                                                                                                    |

### S-40 `NodeTree::insert_child` 收 `NodeSpec`（私有字段 + `new`）

| 项   | 内容                                                                                                                                                                                                                                                                                                                                                  |
|------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | `insert_child(&mut self, parent: NodeId, spec: NodeSpec)`；`NodeSpec { kind, style, measure, layout_override, hit_shape }` **字段全私有** + `NodeSpec::new(kind, style, measure, layout_override, hit_shape)` + `Default`（= boxed 空部件）                                                                                                           |
| 原因 | `clippy::field_scoped_visibility_modifiers = "forbid"`（workspace 根）**禁止字段带 scoped 可见性修饰**（`pub(crate)`/`pub(super)`/`pub(in …)`）：probe 实测 5 个 `pub(crate)` 字段全部报 `scoped visibility modifier on a field`。部件结构体因此不能有 `pub(crate)` 字段；备选是「6 个位置参数」（`too_many_arguments` 已 allow），但调用点可读性更差 |
| 影响 | 纯 crate 内部签名；调用点只有 `NodeBuilder::build`（与 P03 的 F-43 同源）                                                                                                                                                                                                                                                                             |

### S-41 布局入口 = `Window::compute_layout()`；`NodeTree::compute_layout` 是 `pub(crate)`

| 项   | 内容                                                                                                                                                                                                                                                                          |
|------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | `pub fn Window::compute_layout(&mut self) -> Result<LayoutReport, EngineError>`；`pub(crate) fn NodeTree::compute_layout(&mut self, available: taffy::Size<Option<f32>>)`；**不新增** `App::layout`                                                                           |
| 原因 | ①`App` 在这件事上只贡献「窗口查找」，`Option<Result<…>>` 是双层失败语义（窗口不存在 / measure 违规），调用方每次都要拆两层；②根可用空间**就是**窗口逻辑尺寸（P12 写入 `size`），不需要第二个参数；③`NodeTree::compute_layout` 需要 taffy 的 `Size<Option<f32>>`，不该进公开面 |
| 交接 | P06/P09 的帧驱动：`app.refresh_styles(w)` → `app.window_mut(w).unwrap().compute_layout()?` → paint                                                                                                                                                                            |

### S-42 taffy `Cache` 旁表 = `Vec<Option<Cache>>`（按 arena 槽位下标）

| 项           | 内容                                                                                                                                                                                                 |
|--------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策         | `NodeTree.layout_cache: Vec<Option<taffy::Cache>>`，与 `slots` 等长；三处清理：`insert_child` 复用槽位、`remove` 逐节点、新增槽位 push `None`                                                        |
| 原因         | 节点树是**稠密**的（taffy 给每个参与布局的节点建缓存）；`Cache` 实测 **408 B** ⇒ 10k 节点 ≈ 4.2 MB，可算；`Vec` 无哈希（布局顺序确定、帧记录可复现）；槽位复用是我们自己的不变量（一个 `= None` 够） |
| 备选（已否） | `ahash::HashMap<NodeId, Cache>`：命中率无优势（稠密），条目控制字更大，且两套生命周期要两处清理                                                                                                      |
| 影响         | `Node` 不内联 `Cache`（P03 N6 的预算约束），`size_of::<Node>()` 断言仍成立（V-31 跟踪）                                                                                                              |

### S-43 `LAYOUT` 的唯一消费点是 `set_unrounded_layout`；`prev_layout` / damage 在 post-pass 更新

| 项       | 内容                                                                                                                                                                                                                   |
|----------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策     | `set_local_layout`（= `LayoutPartialTree::set_unrounded_layout` 的落点）里 `slot.dirty.remove(LAYOUT)` + 记 `laid_out_epoch`；`absolutize` 在绝对盒变化时写 `prev_layout = old`、`layout = new`、`damage ∪= old ∪ new` |
| 原因     | 「被 taffy 排过」= `LAYOUT` 已满足，这是唯一能精确判定的时刻（一个节点可能在一轮里被写多次）；damage 的新旧并集必须在**绝对坐标**上做（P03 的 `touch` 并的是上一帧的绝对盒）                                           |
| 已知空位 | 布局叶子的子节点永不被布局 ⇒ 它们的 `LAYOUT` 永不清（V-32；仅 release 可达）                                                                                                                                           |

### S-44 `Style.clip: Clip { None, Bounds }`：只裁子树、裁到 padding box、不读 taffy `overflow`

| 项   | 内容                                                                                                                                                                                                                                                                            |
|------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | 见 phase-04 §11                                                                                                                                                                                                                                                                 |
| 原因 | ①CSS `overflow: hidden` 的裁剪盒是 padding box（需要 `LayoutBox.border`，F-46）；②裁剪**不裁自己的 border/background**（probe 固化：边框带命中自己）；③taffy 的 `Overflow` 同时改布局（滚动条槽、可滚动溢出矩形），与「裁剪」是两件事，混用会让「设了 overflow 就悄悄改了布局」 |
| 推迟 | 圆角（`Clip::Rounded`，P06）；滚动偏移（P10 在 `clip_chain` 的绝对矩形上平移）                                                                                                                                                                                                  |

### S-45 `hit_test` = 逆绘制序 DFS + 布局盒剪枝；`Overlay` 无特殊分支；`ClipChain` 定长 8

| 项   | 内容                                                                                                                                                                                                                                                                                                                                                     |
|------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | 见 phase-04 §12                                                                                                                                                                                                                                                                                                                                          |
| 原因 | ①`order` 是 DFS 先序 ⇒ 兄弟**逆序** + 先子树后自身 = 与绘制序严格互逆（z 序天然正确）；②布局盒剪枝把 O(n) 降到 O(深 × 兄弟)；③`Overlay` 的「最高优先」= 排在 `order` 末尾，P13 不需要在 hit test 里加分支（probe 用「末尾的小浮层压过大底层」固化）；④`ClipChain` 定长 `Copy`（136 B）避免把 `smallvec` 版本写进公开 API，也与 `MAX_CLIP_DEPTH = 8` 对齐 |

### S-46 `LayoutReport` 的四个字段与计数语义

| 项          | 内容                                                                                                                                                                            |
|-------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策        | `{ laid_out, moved, cache_hits, cache_misses }`；`laid_out` 只在 `PerformLayout` 且 cache 未命中时 +1；`cache_hits/misses` 在 `cache_get` 里计；`moved` 在 post-pass 里计       |
| 实测基线    | 首轮 4 节点 ⇒ `laid_out = 4`；改 1 个子节点 ⇒ `{ laid_out: 2, moved: 2, cache_hits: 4, cache_misses: 4 }`；无变化 ⇒ `{ laid_out: 0, moved: 0, cache_hits: 2, cache_misses: 0 }` |
| 原因        | 「只重算脏子树」必须**可断言**（L1 的增量判据）；`cache_hits` 会随 taffy 的 measure pass 次数浮动 ⇒ 用例只断言方向（`> 0` / `== 0`），不对精确值断言                            |
| 不含 damage | damage 跨帧累积（P03 §9.4），P06 在帧初清零并物理化；放进报告会诱使调用方把它当「本帧 damage」                                                                                  |

### S-47 不建独立 measure 缓存；`invalidate_measure` 是唯一失效入口

| 项   | 内容                                                                                                                                                                                                                                                                                                                 |
|------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | 没有 `(measure_version, available_space)` 缓存、没有 `measure_version` 字段；`pub fn NodeTree::invalidate_measure(node)` = 标 `LAYOUT` + 清 `node..root` 的 taffy 缓存                                                                                                                                               |
| 原因 | taffy `Cache` 的 key 已含 `(known_dimensions, available_space, sizing_mode, run_mode, parent_size, mode)`（`tree/cache.rs:254-280`），probe 实测「第二轮 measure 零调用」；唯一缺的输入是「闭包捕获的状态变了」，那是一个**失效事件**（`invalidate_measure`），不是一个版本号字段（无读者的字段按 AGENTS §2.1 不加） |
| 交接 | P05 的字体栈/文本内容变化 → 对受影响文本节点调 `invalidate_measure`                                                                                                                                                                                                                                                  |

### S-48 每类型只能有一个 inherent `impl` 块；trait impl 全在 `layout/taffy_tree.rs`

| 项   | 内容                                                                                                                                                                                                                                                 |
|------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 事实 | `clippy::multiple_inherent_impl = "forbid"`（workspace 根）；probe 实测：同一类型两个 inherent impl 块（**即使同文件**）→ `error: multiple implementations of this structure`                                                                        |
| 决策 | `NodeTree` 的全部 inherent 方法（含 `compute_layout`/`absolutize`/`hit_test`）写在 `node/tree.rs` 的**那一个** `impl NodeTree { … }` 里；`layout/taffy_tree.rs` 只放 `impl <Trait> for NodeTree` + 自由函数（`run`/`dispatch_child_layout`/id 映射） |
| 门禁 | phase-04 §16-10：`layout/taffy_tree.rs` 的 `^impl ` 必须是 7 行 trait impl；`node/tree.rs` 的 `^impl NodeTree {` 恰好 1 行                                                                                                                           |

### S-48b 结构性样式变更的公开入口 = `set_style`（取代提案期的 `apply_style`）

| 项   | 内容                                                                                                                                                                                                                                                                                                              |
|------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | `pub fn NodeTree::set_style(&mut self, node: NodeId, style: Style) -> Result<(), EngineError>`；写值 + 标 `LAYOUT                                                                                                                                                                                                 |PAINT` + 清 `node..root` 的 taffy 缓存；陈旧句柄 ⇒ `Err(EngineError::StaleNode { node })` |
| 原因 | 审查 B1：提案期的 `apply_style` 是 `pub(crate)` 且**没有任何非 test 消费者**（`NodeBuilder` 只在挂载时写 style，`Window` 只标脏）⇒ 违反「`pub(crate)` 必须有非 test 消费者」。提升为 `pub` + 返回 `Result` 后，它是应用层「结构性样式变更」的正式入口（P08 的主题/密度切换），并给 `StaleNode` 增加一个真实生产者 |
| 文档 | `set_style` 的 rustdoc 必须写明「**动态属性请用 `bind_style`**」：每帧调它会清整条祖先链的缓存，把增量布局退化为全量                                                                                                                                                                                              |
| 登记 | phase-04 §4.1 的「方法级公开面增量」表 + §4.3 的预留清单 + §8.5 的签名                                                                                                                                                                                                                                            |

### S-49 `LayoutOverride` 的 builder 副作用 + `set_layout_override` 写入口

| 项       | 内容                                                                                                                                                                                                                     |
|----------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策     | `NodeBuilder::layout_override(rect)` **强制** `style.inner.position = Position::Absolute`；挂载后用 `NodeTree::set_layout_override(node, Option<LayoutOverride>) -> bool` 更新（写值 + 标 `LAYOUT                        |PAINT` + 清 `node..root` 缓存） |
| 原因     | ①出流是「位置与尺寸都由应用定」的必要条件（父不留空间），由 builder 强制比要求调用方记得设更可靠；②覆盖矩形必须在挂载后可改（分栏拖拽），而它是布局输入 ⇒ 与 `set_style`/`invalidate_measure` 同一形状的「输入变了」入口 |
| 语义细节 | `rect` 相对**父 border box**；它是节点的 **border box**（不再过 min/max/padding）；节点是布局叶子（`build` 有 debug 断言）                                                                                               |

### S-50 `get_detailed_layout_info` / `set_detailed_grid_info` 走默认实现

| 项   | 内容                                                                                                                                                                           |
|------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | 不存 `DetailedGridInfo<SmolStr>`，不覆写这两个方法                                                                                                                             |
| 原因 | 没有读者（P04 的绝对坐标 post-pass 只用 hoisted 列表）；代价是 **grid 容器内**的 absolute 子盒按 padding box 而非 grid area 定位（taffy 的默认行为，`tree/traits.rs:246-254`） |
| 复核 | V-35；若 P08 出现「absolute 子盒按 grid area 定位」的真实需求，additive 存 `DetailedGridInfo<SmolStr>`（每 grid 容器一份 Box）                                                 |

### S-51 测试与尺寸的落地实测（P04）

| 项                                | 计划               | 实测                                                                                                                                                                                                            |
|-----------------------------------|--------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 新增 `#[cfg(test)]` 用例          | 60（phase-04 §15） | **71**（`layout/taffy_tree.rs` 25、`layout/hit_test.rs` 19、`layout/measure.rs` 10、`node/tree.rs` 9、`node/builder.rs` 3、`window.rs` 4、`node/dirty.rs` 1、`error.rs` 1、`paint/style.rs` 1、`node/id.rs` 1） |
| `cargo test -p ui_core`           | —                  | **271 passed**（P03 结束 200 + 71）                                                                                                                                                                             |
| `cargo test -p ui_core --release` | —                  | **264 passed**（7 个 `#[cfg(debug_assertions)]`/debug-only 用例不编译）                                                                                                                                         |
| `size_of::<Node>()`               | ≤ 1152（§8.3）     | **1016 B**（P03 是 888 B，+128 B：40 B 字段 + 88 B 对齐/填充）                                                                                                                                                  |
| `size_of::<LayoutBox>()`          | 32 B               | **32 B**                                                                                                                                                                                                        |

> 74 个用例全部通过；`node_and_handle_sizes` 打印实测值，`set_style`/`set_layout_override`/`invalidate_measure`
> 的 `StaleNode` 错误路径、`node_debug_prints_layout_fields`（N4 的 Debug 同步）、`clip_chain` 不含目标自身
> （N3）、`visible=false` 不参与命中（N9）、`change_below_fixed_size_ancestor_still_relayouts`（N14 的
> `cache_hits > 0` + 未受影响子树不动）都有专门用例。

### S-52 代码审查（PASS-WITH-FIXES）后的三处语义修正

| 项                             | 修正                                                                                                                                                                                                                                                                                                                                                                          |
|--------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **B1 `LayoutOverride` 参照系** | taffy 把 `position: absolute` 的 override 节点挂到最近的**定位**祖先（无则 root/ICB），其 `local` 因此相对**包含块**；引擎的 override 契约是「相对父 border box」。修正：`absolutize` 的 children 循环不跳过 override 子节点（用父 `abs.origin + override.rect`），hoisted 循环跳过它们。用例：父被 spacer 推到 y=40/100 ⇒ override 绝对 (10,60)/(10,120)、命中、父移动后跟随 |
| **B2 hit test 剪枝**           | 布局盒**不能**作为子树剪枝依据（出流子节点可溢出父盒）；唯一安全的剪枝是 `Clip::Bounds` 祖先的 padding box。修正：children 循环只跳过「叶子 + 盒外」，自身命中增加 `bounds.contains(point)` 前置条件。用例：无 clip 父 + 溢出子命中；有 clip 父 → 不命中（落 root）。代价记 V-38                                                                                              |
| **N1/N3 幽灵盒**               | 隐藏子树（自身或祖先 `display: none`）的所有后代（含 override 与出流盒）强制零化；`dispatch_child_layout` 里 `display:none` 判断**先于** override 分支。用例：隐藏父的出流后代 / 隐藏的 override 节点必须 `bounds.is_empty()`                                                                                                                                                 |
| **N2 `FlowRoot`**              | `Display::FlowRoot` 的 `block_ctx` 透传改为 `None`（与 taffy `TaffyView` 一致）                                                                                                                                                                                                                                                                                               |
| **N5 `hoisted` 清理**          | `remove` 除删除被卸载节点自身的列表外，还 `retain` 掉**所有**「值里包含被卸载 id」的旧列表项                                                                                                                                                                                                                                                                                  |
| **N6 兄弟用例**                | 改为完全重叠的绝对定位兄弟，断言后者胜出（原用例按布局盒剪枝，B2 后语义已变）                                                                                                                                                                                                                                                                                                 |

### 九·待复核项（P04 新增）

| 编号 | 内容                                                                                                                                                                     | 复核 Phase   | 触发条件 / 处置                                                                                                                    |
|------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------|--------------|------------------------------------------------------------------------------------------------------------------------------------|
| V-31 | `size_of::<Node>()`：P04 新增 40 B（`local` 16 + `local_border` 16 + `epoch` 4 + padding）。probe 的等价节点 = 888 B（含 92 B probe-only `final_layout`）⇒ 去掉后 ≈796 B | P04 实现首日 | 复跑 `node_size_within_budget`；超 1024 → `style: Box<Style>`（P03 V-30 的备选，公开面不变）                                       |
| V-32 | 布局叶子（`Canvas`/`Geometry`/`LayoutOverride`）的子节点永不被布局 ⇒ 它们的 `LAYOUT` 位永不清（`StylePassReport.layout_dirty` 可能长期为真）                             | P06          | 若影响「有无脏位」判定：在 `remove`/`begin_frame` 里对不可达子树清位                                                               |
| V-33 | damage 覆盖：祖先盒不含**出流溢出**的后代时，只按祖先 AABB 累积 damage 会漏                                                                                              | P06          | 给 `LayoutBox` additive 加 `overflow: Rect`（taffy `Layout.scrollable_overflow_rect`）并并入 damage                                |
| V-34 | taffy 的 `absolute_position_offset = border`（不含 padding），与 CSS「padding box 定位」有差异                                                                           | P08 迁移     | 引擎照抄不重算（`compute/flexbox.rs:511`）；应用层文档写明「absolute 子节点的 inset 相对 border box」                              |
| V-35 | 不实现 `get_detailed_layout_info` ⇒ grid 容器内的 absolute 子盒按 padding box 定位                                                                                       | P08 / P13    | 出现真实需求 → additive 存 `DetailedGridInfo<SmolStr>`                                                                             |
| V-36 | `ClipChain` 定长 8：超过 8 层的 clip 链被截断（点仍按前 8 层裁剪）                                                                                                       | P10          | 与 `MAX_CLIP_DEPTH` 同一上限（shader 固定数组）；`Lane::push` 的 debug 断言先行暴露                                                |
| V-37 | 增量正确性依赖「结构变更也清缓存」：`append_child`/`remove` → `mark_structure` → 清 `node..root`                                                                         | P04 实现     | 用例 `remove_drops_cache_and_hoisted_entries` 固化；P03 的 `mark_structure` 调用点已就位，只需内部接上 `invalidate_caches_upwards` |

## 十、Phase 05 文本子系统（2026-11-12）

> 关联文档：`custom-gui-engine-design.md` §4.5–§4.8 / §5.6 / §9.3 / §2.8；
> `design/phase-01-foundation.md` §4.1 / §4.5 / 附录 A.2 / A.6；
> `design/phase-02-paint-foundation.md` §10 / §6.3.2；`design/phase-04-layout.md` §8 / §9；
> **本阶段详细设计 = `design/phase-05-text.md`**（下文 §N 均指该文件）。
>
> **A-01 继续适用**：本节只**追加**，不改写任何历史条目（一～九 未改动）。
>
> 事实基线：parley 0.12.0 / fontique 0.12.0 / skrifa 0.44.0 / swash 0.2.10 / etagere 0.3.0 的**本地源码**
> （`E:\.cargo\registry\src\rsproxy.cn-e3de039b2554c837\`，逐字签名与行号见 phase-05 §2.1/§2.2）；
> 四个仓库外 probe（`%TEMP%\kg-p05-probe`、`kg-p05-font-probe`、`kg-p05-size-probe`，phase-05 §2.4）。
>
> 裁决：新增 5 条 F（F-53…F-57）、5 条 S（S-53…S-57）；补齐 S-52 许诺但缺失的 **V-38**（P04 欠账），
> P05 自己的待复核项从 **V-39** 起（V-39…V-47）。

### F-53 形状缓存键拆成两级：`atlas_generation` **不进**形状键

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                               |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 主设计 §4.5（928 行）：「缓存键：`text + color + font_size + TextStyle + scale + font_stack_version + atlas_generation`」                                                                                                                                                                                                                                                          |
| 偏差       | 拆成两级：**形状缓存**键 = `(text hash, TextStyle hash, scale, font_stack_version)`，**不含** `atlas_generation`；`atlas_generation` 只进**字形 tile 缓存**键与 `ShapedLine.atlas_generation` 字段（供 `AtlasTile::is_stale` 判定）                                                                                                                                                |
| 原因       | ①字形 id、advance、baseline、行数全部与图集无关（phase-05 probe ⑤：同一文本的 5 个 run 只取决于字体与 scale）；②`generation` 只在 `clear()` 时 +1（S-11），而 `clear()` 的触发之一是**容量逃逸**——若形状键含它，一次图集满会把**所有**文本 re-shape（`RangedBuilder::build` + 断行），代价远大于重光栅；③P02 §10.6/§3.8 的目标语义是「图集重排 → 依赖节点 `PAINT`」，不是 `LAYOUT` |
| 修订后设计 | phase-05 §7.2 的两级键表 + §10 的 `GlyphKey { font, glyph, ppem }`；`ShapedLine.atlas_generation` 是唯一把代际带回形状快照的字段（P06 用它判 stale）                                                                                                                                                                                                                               |

### F-54 `parley::TextStyle` **存在但 P05 不用**；`TextContext` 不公开

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
|------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 主设计 §4.5（893 行）「**Parley 0.12 无 `TextStyle`**，样式单元是 `StyleProperty<'a, B: Brush>`（26 变体）」；同节 898–902 行又给出 `pub struct TextContext { pub font_cx: parley::FontContext, pub layout_cx: parley::LayoutContext, pub font_stack: FontStack }`                                                                                                                                                                                                                                                                                                                                                                     |
| 偏差       | ①`parley::style::TextStyle<'family,'settings,B>` 在 0.12.0 **真实存在**（`style/mod.rs:284`，26 个 pub 字段；F-03 已判同一事实）——但它是 `StyleRunBuilder::push_style`（`builder.rs:136`）与 `TreeBuilder::push_style_span`（`builder.rs:237`）的「整段样式」输入，`RangedBuilder` **没有**接受它的方法（`push_default` 只收 `impl Into<StyleProperty>`）；P05 走 `RangedBuilder` + `StyleProperty`，**不**使用 `TextStyle`。②`TextContext` **不公开**（crate 私有，`pub(crate)`）                                                                                                                                                     |
| 原因       | ①P05 的样式是单段平铺（一个节点一个 `TextStyle`），`RangedBuilder` 的 26 个变体足够；`TreeBuilder` 还要自管文本缓冲与 style span 栈，属 Markdown 管线（P15）的形态，现在引入等于两套代码路径（F-03 的「二者取一，不并存」）。②`TextContext` 的字段必然是 `parley::{FontContext, LayoutContext}`，公开即把第三方 crate 拉进公开面（F-35 的同类事件，但更重：`Collection` 的每个方法都要 `&mut self`，公开字段等于把「谁能 shape」交给应用层）；且公开后应用可绕过 `App::refresh_text` 直接 shape，与「`App` 持有字体库/图集」的所有权冲突                                                                                               |
| 修订后设计 | ①phase-05 §7.1 的逐字段映射表（`TextStyle → StyleProperty`），`ranged_builder(fcx, text, scale, quantize=true)` 5 参，方向用 `set_base_direction`（F-03 的第 5 参裁决在本阶段落地实现）。②公开面 = `ui_core::text::{TextStyle, FontStack, InlineBoxSpec, ShapedLine, ShapedTextLine, ShapedTextItem, ShapedGlyphRun, ShapedInlineBox, TextContent, TextPassReport, TextDiagnostic}` + `App::{font_stack, font_stack_version, set_font_stack, reload_system_fonts, refresh_text, text_diagnostics}`；`TextContext` 不在公开面，phase-01 §4.5 的路径表**无需**新增行。**「无 `TextStyle`」这句按 F-03 视为事实错误，本节不改主设计正文** |

### F-55 `AtlasExhausted` 的落地 Phase 由 P09 改为 **P05**

| 项         | 内容                                                                                                                                                                                                                     |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | P01 §6.2.2 #12：「`AtlasExhausted { kind: AtlasKind }`，实现 Phase **P09**」；P02 §10.6 又说「该变体随分配器在 P05 追加」                                                                                                |
| 偏差       | 在 **P05** 落地（payload 逐字不变），P09 只做消费者                                                                                                                                                                      |
| 原因       | 唯一生产者是 `AtlasAllocator::allocate` → `None` 的第二次重试（P02 §10.6 的容量失败路径），而分配器按 P02 裁决 ① 在 P05 落地（phase-05 §9.5）；P09 不产生该错误。与 F-51（`InvalidMeasure` 由 P05 提前到 P04）同一类修订 |
| 修订后设计 | `error.rs` 加变体 + `all_variants()` 一条；phase-05 §9.5 的失败路径逐字；P01 表 #12 的「实现 Phase」列以本条为准，正文不改                                                                                               |

### F-56 `invalidate_measure` **不**标 `PAINT`（S-47 的语义缺口）：文本 pass 显式补

| 项         | 内容                                                                                                                                                                                                                       |
|------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 主设计 §4.6（939 行）：「系统字体安装/卸载、深浅色切换、语言切换 → `font_stack_version += 1` → 所有文本节点 **`LAYOUT + PAINT`**」；S-47：「P05 的字体栈/文本内容变化 → 对受影响文本节点调 `invalidate_measure`」          |
| 偏差       | `NodeTree::invalidate_measure`（P04 已落地）只标 `LAYOUT` + 清 `node..root` 缓存，**不**标 `PAINT`（`node/tree.rs:663` 走 `invalidate_layout`）。因此仅调它会让 §4.6 要求的 `PAINT` 丢失                                   |
| 原因       | 两条上游要求不等价：`invalidate_measure` 的语义是「布局输入变了」，`PAINT` 的语义是「像素变了」。文本/字体变化**两者都**成立                                                                                               |
| 修订后设计 | 文本 pass 用 `NodeTree::invalidate_text_layout(node)`（= `invalidate_measure` + `mark_paint`，phase-05 §13.1/§6.3），`pub(crate)`；字体栈切换遍历所有文本节点调它。`invalidate_measure` 本身**不改**（P04 的公开语义保持） |

### F-57 `parley` 必须开 `complex-scripts` feature

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
|------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | P01 §5.1 只钉 `parley = "0.12.0"`（默认 features）；主设计 §4.5 说断行由 ICU4X 分析                                                                                                                                                                                                                                                                                                                                                                                                               |
| 偏差       | `[workspace.dependencies] parley` 改为 `features = ["complex-scripts"]`                                                                                                                                                                                                                                                                                                                                                                                                                           |
| 原因       | 不开时 `parley_engine` 的 `word_segmenter()`/`line_segmenter()` 走 `new_for_non_complex_scripts`（`analysis.rs:110-116`、`:153-162`），ICU4X 没有 CJK 词典模型（`icu_segmenter-2.4.0/src/complex/mod.rs:183`），probe 每段中文都向 stderr 打 `ICU4X data error: No segmentation model for complex script: Chinese/Japanese`，并退化到 UAX #29 WB999 的「有无字典都按字符边界」路径（`analysis.rs:681-686`）；`has_word_dictionary() = cfg!(feature = "complex-scripts")`（`analysis.rs:854-855`） |
| 修订后设计 | phase-05 §17：只改 feature，**不新增依赖**（`Cargo.lock` 不变）；门禁 §16-12 用 `cargo tree -e features` 断言该 feature 生效                                                                                                                                                                                                                                                                                                                                                                      |

### S-53 `NodeKind::Text` 的载荷**内联**（`Node` **1088 B**），不做 `Box<TextContent>`

| 项       | 内容                                                                                                                                                                                                                                                                                                                                                                                                     |
|----------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策     | `NodeKind::Text(TextContent)`，`TextContent { text: SmolStr, style: TextStyle }` **内联**；`Node.text_bind: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>>`（**胖指针 16 B**）+ `Node.text_measured_size: Size`（8 B）；`Rc<ShapedText>`/`Rc<ShapedLine>` 放 `NodeTree` 的**旁表** `text_slots: Vec<Option<TextSlot>>`（16 B/槽，仿 P04 的 `layout_cache`）                                            |
| 原因     | probe ③ 实测：`Node` 1016 B、`NodeKind` 48 B（= `Canvas`）、`TextStyle` 72 B、`TextContent` 96 B、`Option<Box<TextContent>>` 8 B。内联 ⇒ `NodeKind` **96 B**（载荷 96 吃掉了 tag/niche：`Canvas` 的 `Vec` 提供 niche，实测）、`Node` = 1016 − 48 + 96 + 16 + 8 = **1088 B** ≤ P04 的 1152 B 上界（余量 64 B）。`Box` 方案回 1040 B 但每个文本节点多一次分配 + 一层解引用；文本节点在 UI 里是常态而非例外 |
| 代价     | `Node` 从 1016 → **1088 B**（+72 B ≈ 7%）。若 P06/P07 再加字段顶破 1152，改 `NodeKind::Text(Box<TextContent>)` 即可（公开面不变）                                                                                                                                                                                                                                                                        |
| 实测校正 | `Option<Box<dyn Fn>>` 是 16 B（`Box<dyn Trait>` = data + vtable），初版文档误记为 8 B；`NodeKind` 实测 **96 B**（不是 104：`TextContent` 的 96 B 载荷吃掉了判别位，`Canvas` 的 `Vec` 提供 niche）⇒ `Node` 实测 **1088 B**；P04 的 `node_and_handle_sizes` 断言**本来就是 `<= 1152`**，不存在 1024 的上界                                                                                                 |
| 登记     | phase-05 §14.1、§2.4④；`node_and_handle_sizes` 追加 `size_of::<TextContent>() == 96` 与 `size_of::<Node>() <= 1152`                                                                                                                                                                                                                                                                                      |

### S-54 光栅与图集在 **P05** 落地，`PositionedGlyph` 复用 P02 类型

| 项       | 内容                                                                                                                                                                                                                                                                                                                                                        |
|----------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策     | P05 交付「shape → raster → 装箱」的 CPU 全链：`App::refresh_text` 产出 `ShapedLine`，其中每个字形是 P02 的 `paint::primitive::PositionedGlyph { tile, position, size }`（`position` 为**文本盒局部**坐标）。P06 只做 `ShapedLine → Scene` 的实例编码（加节点原点、组 `GlyphRun`、分 lane）                                                                  |
| 原因     | ①若把光栅/图集推到 P06，`render/atlas.rs` 的 11 个 `pub(crate)` item 只有 test 消费者 ⇒ 违反「`pub(crate)` 必须有非 test 消费者」（P02 §3.2 / P03 §4.3 / AGENTS §4），`-D warnings` 直接红；②`ShapedLine` 的自然形态就是「字形 + 图集矩形 + 落点」，P02 §6.3.2 已经冻结了 `PositionedGlyph` 的字段与语义（40 B 实测），再定义一套「未装箱字形」等于两份真相 |
| 契约细节 | `PositionedGlyph.size` 的契约「`scale * size` 必须是整数」由光栅侧的整数 ppem 保证（phase-05 §8.3）；`position` 在 `ShapedLine` 里是局部坐标，加节点原点后才是 P02 意义的绝对坐标（P06 的责任，phase-05 §5.2 的 rustdoc 写明）                                                                                                                              |
| 门禁     | `render/atlas.rs` 的每条 `pub(crate)` 必须在 phase-05 §4.3 表里指向 `text/atlas.rs` 或 `App::refresh_text`（非 test）                                                                                                                                                                                                                                       |

### S-55 `NodeTree::text_selection_rects` 返回 `Vec<Rect>`（**不**用 `SmallVec`）

| 项           | 内容                                                                                                                                              |
|--------------|---------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策         | `pub fn text_selection_rects(&self, node: NodeId, anchor: usize, focus: usize) -> Result<Vec<Rect>, EngineError>`                                 |
| 原因         | P04 §4.2 已确立「不把 `smallvec` 版本写进公开面」（`ClipChain` 用定长结构就是这条）。选区每行一个矩形，行数一般 ≤ 4，但公开面不为此引入第三方类型 |
| 备选（已否） | 返回 `SmallVec<[Rect; 4]>`（少一次分配）；因公开面纪律否决                                                                                        |
| 登记         | phase-05 §12.5、§4.1                                                                                                                              |

### S-56 空文本 = `Size::ZERO` + 0 行（parley `""` 是**非零**布局）

| 项   | 内容                                                                                                                                                                                                                                                                                                                           |
|------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | `measure` 对 `text.is_empty()` 直接返回 `Size::ZERO`；`ShapedLine` 对空文本产出 `lines: []`、`width/height: 0`、`first_baseline: None`                                                                                                                                                                                         |
| 原因 | probe 实测（16 px Segoe UI、scale=1）：`text=""` ⇒ `lines()==1`、`width()==4.4453`、`full_width()==4.4453`、`height()==18.0`、`first_baseline()==Some(14.0)`、`items()==0`。即 parley 0.12 对空文本报出一个「有宽度的一行」；直接采信会让空 label 占 4.4 px 宽（布局抖动 + damage 失真）。`" "` 的 4.4453 是真实空格，不受影响 |
| 代价 | 需要保留高度的应用显式给节点 `Style.size.height`（文档写明）                                                                                                                                                                                                                                                                   |
| 用例 | `empty_text_is_zero_size`（phase-05 §15.1 #7）、`raster_space_is_empty`（§15.3 #22）                                                                                                                                                                                                                                           |

### S-57 `NodeBuilder::bind_text` 返回 `SmolStr`（brief 写的是 `String`）

| 项   | 内容                                                                                                                                                            |
|------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | `pub fn bind_text(self, f: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static) -> Self`；`Node.text_bind` 同型                                                      |
| 原因 | AGENTS §2.2：短字符串优先 `SmolStr`、禁止 `format!` → `String` → `SmolStr` 的中转；文本节点的值域绝大多数 ≤ 22 B（内联，零分配），`String` 会每次求值分配一次堆 |
| 影响 | 应用层写 `bind_text(                                                                                                                                            |cx| format_smolstr!("…"))`；若仲裁要求 `String`，只在闭包出口做一次 `SmolStr::new(&s)`（多一次分配），设计其余部分不变 |
| 登记 | phase-05 §13.2、§18 裁决项 ②                                                                                                                                    |

### S-58 P05 设计审查（FAIL：B1–B3 + N1–N6）的修正

| 项                           | 修正                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
|------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **B1 形状快照失效纪律**      | 任何 shape 快照变化（`text` / `TextStyle` 任一字段 / `scale` / `font_stack_version`）**必须**清 `node..root` 的 taffy 缓存；「测得尺寸是否变化」只决定**额外**置不置 `LAYOUT`（没变 → 只 `PAINT`）。实现：`NodeTree::set_text_shaped(node, shaped, measured, size_changed)` 内**无条件** `invalidate_caches_upwards(node)`。反例用例 `text_wrap_change_same_max_width_relayouts`（`Wrap`→`Anywhere`：`widths.max` 相同、`Definite(40)` 的行数不同）；另 `text_same_size_still_clears_cache`（`"ab"`→`"ba"`：尺寸全同、缓存仍重算） |
| **B2 `FontKey` 稳定性**      | `(ptr, len, index)` 不稳定：`ranged_builder` 每次都 `source_cache.prune(128, false)`（`parley/context.rs:109`），Blob 释放后同地址可被复用。修正：`FontEntry { blob: parley::FontData }` **pin 住** `Blob` 的 `Arc`，键改为 `(ptr, len, ttc_index, font_epoch)`；`set_font_stack` / `reload_system_fonts` 时 `font_epoch += 1` + 清 `GlyphRasterizer::fonts` + 清 `GlyphAtlas` + `AtlasSet::glyph_mut().clear()`（字形 tile 不跨字体代际复用）                                                                                     |
| **B3 `Node` 尺寸与断言**     | `Option<Box<dyn Fn>>` 是 **16 B**（初版误记 8）⇒ `Node` = **1088 B**（实测；`NodeKind` 96 B，载荷吃掉判别位）。P04 的 `node_and_handle_sizes` 断言本来就是 `<= 1152`，无需改动；phase-05 §0/§14.1/§16-13 的尺寸表已按实测回填，并追加 `size_of::<TextContent>() == 96`、`size_of::<NodeKind>() == 96` 打印                                                                                                                                                                                                                         |
| **N1 tofu 断言**             | 改为「`tofu == 0` **或** `diagnostics` 含 `FontStackMiss`」；宿主无 CJK/emoji 字体时 `eprintln!` 一条 skip 说明，不 panic（防 CI 假绿/假红）                                                                                                                                                                                                                                                                                                                                                                                       |
| **N2 借用**                  | `refresh_text` 用**索引循环**（`while index < tree.order().len()`），避免 `for node in tree.order()`（持 `&NodeTree`）与 `set_text_shaped`（要 `&mut`）冲突；`App` 侧先按字段解构再 `windows.get_mut`                                                                                                                                                                                                                                                                                                                              |
| **N3 命名**                  | 只有 `Window::scale_epoch()` 与 `App.scale_epochs`；判定用 `entry.or_insert(0)`，无 `text_scale_epoch` 字段                                                                                                                                                                                                                                                                                                                                                                                                                        |
| **N4 哈希**                  | `ShapeKey` 的 `family` 用 `Option<&str>`（`SmolStr::as_str()`）参与 hash，不做 `SmolStr` 的 `Hash` 依赖                                                                                                                                                                                                                                                                                                                                                                                                                            |
| **N5 槽位复用**              | `text_slots` 与 `layout_cache` 同生命周期：`insert_child` 复用槽位时 `resize_with` 后置 `None`，`remove` 对整棵子树逐个置 `None`（否则新节点继承上一代的 `Rc<ShapedText>`）                                                                                                                                                                                                                                                                                                                                                        |
| **实现期实测（2026-11-12）** | `Node` = **1088 B**（`NodeKind` 96 B 吃掉判别位）；`cargo test` = **356**、release = **348**、clippy/fmt/doc 全绿；`take_dirty_rows`/`deallocate` 因「v1 无逐 tile 逐出」降为 `#[cfg(test)]`（P09 恢复），脏行的生产读面改为 `dirty_span_count()`；`GlyphAtlas::ensure` 返回 `GlyphTile`（tile + swash `left/top/w/h`，只给 `AtlasTile` 算不出落点）；`refresh_styles` 在文本变更后**重新补 `PAINT`**（`clear_paint` 会吞掉它）                                                                                                    |
| **N6 版本来源**              | 注明 `parley` 的 `pub use fontique`（`lib.rs:109`）与 `ui_core` 直依赖的 `fontique` 都是 **0.12.0**，全仓单一实例（`Collection` 与 `parley::FontContext.collection` 同型）                                                                                                                                                                                                                                                                                                                                                         |

### 十·待复核项（P05 新增）

| 编号 | 内容                                                                                                                                                                                              | 复核 Phase                                            | 触发条件 / 处置                                                                                                                                                                                          |
|------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| V-38 | **（P04 欠账补齐，S-52 的 B2 行许诺「代价记 V-38」）** 命中测试**不能**用布局盒做子树剪枝后，最坏情况是整树遍历：每个节点都要做 `bounds.contains(point)` + clip 祖先链检查，O(n) 而非 O(命中路径) | P10（事件分发）/ P13（Overlay）/ P14（a11y hit test） | 若高频 hover 的树规模 > 5k 节点且命中耗时进入 L4 基准：additive 加「按层/按浮层的空间索引」（不改 `HitResult` 契约）；在此之前靠 P04 的「叶子 + 盒外」与 clip 剪枝（S-52 B2）兜住                        |
| V-39 | 长文本 measure 的 `clone + rebreak` 成本：16000 字符 **288 µs/次**（probe ④，`--release`），×2–3 次/轮 ⇒ 近 1 ms                                                                                  | P05 实现首日 / P18 基准                               | 先落 256 条 / 8 MiB 的形状缓存 + 文档写明「>8k 字符建议拆节点」；成为热点则给 `ShapeCache` 加「按宽度的测量记忆」（`(key, width_bits) → Size`，仍无 `RefCell`：记忆在 `App` 侧，闭包只捕获不可变句柄）   |
| V-40 | v1 整数 ppem + 整数物理落点：同一行内不同字号/ppem 的字形会有 ±0.5 px 的基线/字面抖动（`quantize=true` 只保证 baseline 整数，`placement.top` 是字形自身的整数上沿）                               | P06（像素快照）/ P18                                  | 视觉回归若可见抖动 → 加 subpixel x（3 套 tile 或 3× 页，图集容量 ×3）或改用 `quantize=false` + 自算舍入                                                                                                  |
| V-41 | 彩色 emoji：R8 字形图集只能表达单色轮廓；CBDT-only 字体会渲染为空（probe ⑤ 的 `🌍` 用 `Source::Outline` 有 213 个非零像素，说明该字体有 COLR/outline 层）                                         | P06 / P15                                             | 出现空 emoji → additive 加 `AtlasKind::Color`（BGRA8 的第三页）与新 `kind` 值；P02 §10.3 的两种格式表要扩                                                                                                |
| V-42 | measure 闭包持有 `Rc<ShapedText>`（含 parley `Layout`）⇒ 大段落的 `Layout` 在节点卸载/文本变化前不释放                                                                                            | P05 实现 / P06 帧记录                                 | `remove` 清 `text_slots`（已设计）；这是**期望**行为（正在显示的文本不该因缓存逐出而重排）。若内存吃紧：给 `ShapedText` 加 `SmallVec<[Line; 1]>` 之外的瘦身不划算，改为 `Rc` 弱引用 + 每帧重建（不推荐） |
| V-43 | 性能细项：`family_list()` 每次 shape `clone` 一次 `Vec`；`swash::FontRef::from_index` 每次光栅化重解析表目录；LRU 用 `VecDeque` 提升 O(n)                                                         | P18 基准                                              | 都在 µs 级；有热点再改（不改公开面）：`family_list` 改借用、`FontEntry` 缓存 `offset`、LRU 改侵入式链表                                                                                                  |
| V-44 | 图集单页 2048²（字形 4 MiB / 图片 16 MiB）：CJK + 多 DPI 档可能不够；容量逃逸是 `clear + retry`（全部文本重光栅）                                                                                 | P09 / P15                                             | 先按 P02 §10.6 的 `clear + retry` → `AtlasExhausted`；出现真实溢出再加 `page` 字段（契约变更，需 P02/P07/P09 同步）或页扩容（`generation` 语义要扩）                                                     |
| V-45 | 文本 pass 每帧遍历 `tree.order()`（O(n)），不走脏位；`DirtyRows::spans` 在 P05 只被 `TextPassReport.dirty_spans` 计数（真正的上传读者是 P07）                                                     | P06 帧驱动                                            | `DirtyFlags` 还有 2 个保留位，加 `TEXT` 位即可增量遍历；在此之前 `refresh_text` 的 `report` 里 `shaped/reused` 计数是「是否真的做了活」的判据                                                            |
| V-46 | CI 的 Linux 容器可能无 CJK/emoji 系统字体 ⇒ `tofu == 0` 的断言会假失败（D17 明确不要求跨平台一致）                                                                                                | P05 实现 / CI                                         | 断言写成「`tofu == 0` **或** `diagnostics` 非空」，并单独断言诊断的字段；测试注释写明依赖宿主字体                                                                                                        |
| V-47 | `complex-scripts` 会把 ICU4X 的 CJK 词典数据链进二进制（体积未实测）                                                                                                                              | P18 体积预算                                          | 中文断行正确性优先；若 release 体积超预算，再评估「仅 CJK 目标平台开 feature」（会引入 `#[cfg]`，与「平台差异收在 platform/render」的纪律冲突，慎用）                                                    |

---

### S-59 文本两阶段（shape → 按内容宽重排）+ 审查第二轮修正

| 项                                       | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
|------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **B1 新增第二阶段**                      | `App::layout_text(window) -> Result<TextLayoutReport, EngineError>`，帧序 **styles → refresh_text → compute_layout → layout_text → paint**。`refresh_text` 只 shape + 按 max-content 量固有尺寸（喂 measure）；`layout_text` 用 taffy 解析出的**内容宽**（`Layout.size - border - padding`，在 `set_local_layout` 时存进 `Node.text_content_width`）clone layout → `break_all_lines(Some(width*scale))` → `align` → 光栅/装箱 → `ShapedLine`。选区映射改用**最终断行布局**（`TextSlot.broken`），不再用 max-content 布局 |
| **B2 ppem**                              | `parley::Run::font_size()` 已是**物理值**；`rasterize` 改为 `font_size().round().clamp(1,512)`（原实现又乘了一次 `scale`）。用例：1x/1.25x/2x 的物理宽度递增、逻辑宽度不变                                                                                                                                                                                                                                                                                                                                               |
| **B3 `TextStyle.family`**                | `shape::family_list(stack, custom)` 把 `style.family` **前置**进 `FontFamily::List`；用例：三个 family 的 advance 至少两种不同                                                                                                                                                                                                                                                                                                                                                                                           |
| **B4 空闲帧**                            | `refresh_text` 先比 `ShapeKey` + `scale_epoch` + `Rc::ptr_eq`，全等则 `skipped += 1` 并 **continue**（不重建 measure、不清缓存）。`TextPassReport::skipped` 是它的计数器                                                                                                                                                                                                                                                                                                                                                 |
| **N1 行内盒入口**                        | `TextContent.inline_boxes: Box<[InlineBoxSpec]>`（+16 B）+ `TextContent::with_inline_boxes` + `NodeBuilder::inline_boxes`；`ShapeKey` 手工哈希盒字段（f32 走 bits）；`InlineBoxOverflow` 现在用真实内容宽触发                                                                                                                                                                                                                                                                                                            |
| **N2 R8 留存**                           | `GlyphRecord.data: Box<[u8]>` 存覆盖度（P09 上传源），`GlyphAtlas::coverage` 读它；tile 命中**不重光栅**（`stats.hits` 只加计数）                                                                                                                                                                                                                                                                                                                                                                                        |
| **N3 代际一致**                          | `layout_text` 记录 pass 起始 `generation`；若中途换代（容量逃逸）**整 pass 重跑一次**（`TextLayoutReport::reruns`），第二次仍变则返回 `TextShape`                                                                                                                                                                                                                                                                                                                                                                        |
| **N4 统一跳过口径**                      | `App::font_stack_status() -> FontStackStatus{Ready,EmptyStack,NoMatchingFamily}`；用例不再散落 `eprintln!`                                                                                                                                                                                                                                                                                                                                                                                                               |
| **N5 FontStackMiss / 空格负缓存 / 合成** | 空栈与「全部候选都不存在」都**退 generic 兜底**并记 `FontStackMiss`（`ShapeFailure::EmptyFontStack` 已删除）；`GlyphAtlas.missing` 负缓存空白字形；`Run::synthesis().any()` ⇒ `TextDiagnostic::SyntheticStyle{text_range}`（只记录，不应用合成）                                                                                                                                                                                                                                                                         |
| **N6 渲染出口**                          | `render.rs` 暴露 `{Allocation, AtlasAllocator, AtlasSet, DirtyRows}` 四项；`AtlasSet::image()`；`AtlasAllocator::take_dirty_rows` 恢复非 test，`App::pending_dirty` 合并；`DirtyRows::merge`；`close_window` 清 `scale_epochs`；删除无读者的 `AtlasSet::generation`/`dirty_span_count`                                                                                                                                                                                                                                   |
| **尺寸（实测）**                         | `TextContent` **112 B**（+`Box<[InlineBoxSpec]>` 16）、`NodeKind` **112 B**、`Node` **1112 B** ≤ 1152（余 40 B）                                                                                                                                                                                                                                                                                                                                                                                                         |
| **B4 复位（复审二）**                    | `set_text_shaped` 的 Some 分支写新快照时同时把 `line/broken/laid_width_bits/laid_generation` 清空 ⇒ `layout_text` 的复用判据（`line.is_some() && width_bits && generation`）必然失效、强制重排；回归用例覆盖文本变短/变长、style、family、inline_boxes 四类变更                                                                                                                                                                                                                                                          |
| **inline_boxes 保留（复审二）**          | `refresh_styles` 不再用 `TextContent::new(value, style)` 重建（会丢行内盒），改为 clone 旧载荷只改 `text`/`style`；用例断言 `bind_text` 首帧与更新后 `inline_boxes.len()` 不变                                                                                                                                                                                                                                                                                                                                           |
| **门禁（复审二）**                       | `cargo test` **364**、release **356**、clippy/fmt/doc 0；三平台 `metadata` rc=0；`Node` 1112 断言保留                                                                                                                                                                                                                                                                                                                                                                                                                    |

---

## 十一、Phase 06 paint pass / 实例编码 / damage / headless 运行时 / 测试基建（2026-11-12）

> 关联文档：`custom-gui-engine-design.md` §3.4–§3.8 / §5.1–§5.7 / §5.9 / §6.1–§6.6 / §6.8 / 附录 B、C；
> `design/phase-02-paint-foundation.md` §6–§8、§11–§13；`design/phase-03-node-state.md` §9 / §11 / §13 / §14；
> `design/phase-04-layout.md` §8 / §13；`design/phase-05-text.md` §5.2 / §13；
> **本阶段详细设计 = `design/phase-06-paint-runtime.md`**（下文 §N 均指该文件）。
>
> **A-01 继续适用**：本节只**追加**，不改写任何历史条目（一～十 未改动）。
>
> 事实基线：仓库外三个 probe（§2.1 的 `kg-p06-size-probe`、§2.2 的 `kg-p06-deadcode-probe` / `kg-p06-lints-probe`）；
> `cargo test -p ui_core` 基线 **364 passed**。
>
> 裁决：5 条（phase-06 §15，全部采纳）；新增 9 条 F（F-58…F-66）、7 条 S（S-60…S-66，
> S-66 为审查 B1 的 clip_key 守卫）；P06 自己的待复核项从 **V-48** 起（V-48…V-52）。

### F-58 静态绘制属性 = `Node.paint: Option<Box<PaintSpec>>`（**不进** `Style`）

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                                            |
|------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 主设计 §4.9.3 / P03 §10.1：`Style` 是「静态结构」，注记「P06 才会追加其余引擎自有字段（圆角 / 阴影 / 字体族）」                                                                                                                                                                                                                                                                 |
| 偏差       | 新增公开类型 `paint::style::PaintSpec { background, border, corner_radius, shadow, gradient }` + `ShadowSpec`，作为 **`Node` 的独立字段** `paint: Option<Box<PaintSpec>>`；`Style` 的公开结构（`inner` / `clip`）**不变**                                                                                                                                                       |
| 原因       | ①probe：`Style` 560 B、`PaintSpec` 136 B —— 内联 `Style.paint` 会让 `Style` 696 B、`Node` **1248 B > 1152**；②`Style` 加私有字段会让全仓 `Style { inner, clip }` 字面量（`window.rs`/`app.rs` 的测试等 6 处）无法构造，加公开字段则要求所有调用点补 `paint: None`（纯噪声）；③绘制属性与布局属性是两条独立的失效通道（`StylePatch` 已按外观分组），`set_style` 不该承担绘制语义 |
| 代价       | `Node` 1112 → **1120**（+8），叠加 F-59 的 `content_offset` 后 **1128**（余 24 B，V-48）                                                                                                                                                                                                                                                                                        |
| 修订后设计 | phase-06 §1.2 / §6.1 / §11.1；`NodeBuilder::paint(PaintSpec)`、`Node::paint_spec()`（`pub(crate)`）                                                                                                                                                                                                                                                                             |

### F-59 `REUSABLE` / damage / `prev_layout` 的复位点在**提交**（`NodeTree::settle_frame`）

| 项         | 内容                                                                                                                                                                                                                                                             |
|------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 主设计 §3.4：「**一帧开始时**所有节点默认 `REUSABLE`；被失效的节点清掉该位」；P03 §9.4：「damage 清零：P06 的帧初（`NodeTree::begin_frame`）」                                                                                                                   |
| 偏差       | 不设 `begin_frame`；改在**提交**时调 `NodeTree::settle_frame`：`damage = ∅`、`prev_layout = layout`、`dirty.insert(REUSABLE)`、`dirty.remove(LAYOUT                                                                                                              | PAINT)` |
| 原因       | `drain`（写信号 → `mark_*` → `touch` 清 `REUSABLE` 并并 damage）发生在 `frame()` **之前**。帧初清零会：①吞掉 drain 累积的旧像素 damage（"改了颜色但旧位置没重绘"）；②把 `REUSABLE` 重新置位，让 paint 复用过期 lane（stale drawing）。复位必须发生在**消费之后** |
| 推论       | 复位点是 `FrameIndex` 递增的同一处 ⇒ "提交了才结算"；错误/空闲路径**不**结算（damage/dirty 保留，下一次重试）。`prev_layout = layout` 同时让"只改颜色"的节点不必把历史上的位置并进 damage                                                                        |
| 修订后设计 | phase-06 §9.4 / §5.5；用例 §12-E1/E2                                                                                                                                                                                                                             |

### F-60 `absolutize` 的 `changed` 分支必须清 `REUSABLE`

| 项         | 内容                                                                                                                                                                                                   |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | P04 §7.2 / S-43：`place_subtree` 在 `changed` 时把 `old ∪ new` 并入 damage、写 `prev_layout`、`layout`                                                                                                 |
| 偏差       | 同一分支追加 `dirty.remove(REUSABLE)`                                                                                                                                                                  |
| 原因       | lane 内的 `Primitive` 持有**绝对** bounds（P02 §6.1 无变换栈）。布局移动后，即使没有任何 `mark_paint`，lane 内容也已过期；只并 damage 会让"移动到新位置的节点"继续用旧坐标绘制（且新位置的像素没人写） |
| 修订后设计 | phase-06 §5.6；用例 §12-F3（paint-only 帧不重排、移动帧必须重建）                                                                                                                                      |

### F-61 damage 的输出是 `DamageSummary{full, rects}`；`DamageOutput` / `ScrollHint` 延后 P07 / P12

| 项         | 内容                                                                                                                                                                                                                                                                                                                                       |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | P02 §12.1：`DamageOutput { Full, Partial { rects: Vec<PhysicalRect>, scroll: Option<ScrollHint> } }` + `push_scroll`                                                                                                                                                                                                                       |
| 偏差       | 不落 `DamageOutput` / `ScrollHint` / `push_scroll`；`DamageAccumulator::finish(frame) -> DamageSummary { full: bool, rects: usize }`，矩形留在累加器里（`rects()` 只在该 feature 下提供）                                                                                                                                                  |
| 原因       | ①`Partial { rects: Vec }` 是**耗尽式**交出，与 P02 §12.6「`reset` 保留容量」自相矛盾；②P06 没有 scroll 数据源（`Transient` 滚动偏移属 P12），一个永远 `None` 的字段违反 AGENTS §2.1；③P07 的平台翻译（`Present1` dirty/scroll、`VK_KHR_incremental_present`）只需要 `(full, &[PhysicalRect])`，需要拥有式结果时 additive 加 `take_rects()` |
| 修订后设计 | phase-06 §8.1 / §8.5；用例 §12-B11/B12                                                                                                                                                                                                                                                                                                     |

### F-62 `Geometry` 的 transform 在 P06 恒为 `Transform::IDENTITY`

| 项         | 内容                                                                                                                                                                                                                                                                                                                                             |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 主设计 §5.5 / §3.10：`app.set_geometry_transform(geo, camera_matrix)` → 只更新 uniform；`GEOMETRY` dirty                                                                                                                                                                                                                                         |
| 偏差       | P06 的 paint 生成 `Primitive::Geometry(GeometryDraw { id, transform: IDENTITY })`；`GEOMETRY` 位**仍无写者**；`set_geometry_transform` 的 uniform 通道留 P08/P09                                                                                                                                                                                 |
| 原因       | ①`App` 还没有 `GeometryStore`（P02 §9 明确 store 属 P09），`GeometryId` 只能由测试构造（F-65）；②transform 的归属是 **`GeometryId`**（per-id 相机矩阵），不是节点 —— 在 `Node` 上造 `geometry_transform`（+24 B）会破 Node 预算（1144+24）并与 P08 的 per-id API 冲突；③P06 的职责是"0 实例 + 1 个 `FrameOp::Geometry`"，uniform 的重写属 GPU 侧 |
| 修订后设计 | phase-06 §6.2 / §7.5；P08 落地时：写者置 `GEOMETRY` + 重写该节点的单条 `GeometryDraw`（≤1 图元的 lane 重建，GPU VBO 不动），damage = 变换前后 bounds 并集                                                                                                                                                                                        |

### F-63 `Selector::Role` 延后 P14

| 项         | 内容                                                                                                                            |
|------------|---------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 主设计 §6.2：`Selector` 支持 `Id(&'static str)` / `Role(A11yRole)` / `Text(&str)` / `Path(&[...])`                              |
| 偏差       | P06 落 `Id` / `Text` / `Path` 三个变体，`Role` 推迟到 P14                                                                       |
| 原因       | `A11yRole` 是 P14 的类型（`a11y.rs` 不存在）；先建一个带"占位类型"的变体等于引入无消费者分支，且会迫使 P14 改 `Selector` 的布局 |
| 修订后设计 | phase-06 §10.2；P14 additive 加 `Role(A11yRole)` 变体 + 匹配臂                                                                  |

### F-64 文本绝对坐标 = `bounds.origin + content_offset + 局部`

| 项         | 内容                                                                                                                                                                                                                                                              |
|------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | P05 §5.2 / S-54：「`positioned` 的 y 已含基线，P06 加节点 `LayoutBox.bounds.origin` 后才是绝对坐标」                                                                                                                                                              |
| 偏差       | 在 `bounds.origin` 之外再加 `Node.content_offset`（= `border.top/left + padding.top/left`，`set_local_layout` 从 taffy `Layout` 写入）                                                                                                                            |
| 原因       | `LayoutBox.bounds` 是 **border box**，而 `ShapedLine` 的坐标是**文本盒（内容盒）局部**。有 padding 的文本节点（按钮/输入框/标签）直接用 `bounds.origin` 会整体左上偏移 padding 像素。`content_offset` 是 8 B 的点（不是 16 B 的 `Edges`），因为绘制只需内容盒原点 |
| 修订后设计 | phase-06 §5.2 / §6.3；`Node` +8 B（F-58 合计 1128）                                                                                                                                                                                                               |

### F-65 `GeometryId::for_test` 在 `feature = "test-support"` 下是 `pub`

| 项         | 内容                                                                                                                                                                    |
|------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | P02 S-20：`GeometryId` 字段私有 + 公开 getter + **仅测试构造器** `#[cfg(test)] pub(crate) fn for_test`                                                                  |
| 偏差       | cfg 扩为 `#[cfg(any(test, feature = "test-support"))]`，可见性改为 `pub`                                                                                                |
| 原因       | 集成测试（`tests/frame_record.rs`）不是 `cfg(test)`，而 P09 之前没有 `GeometryStore` 铸造句柄的公开路径；没有它，"`Geometry` 节点产生 `GeometryDraw`"这条 L2 断言无法写 |
| 修订后设计 | phase-06 §1.2 / §4.1；**P09 必须删除该构造器**（`GeometryStore` 是唯一铸造点），用例只在 test-support 下存在                                                            |

### F-66 `FrameScheduler` 只保留 `Idle` / `Pending` / `Draining`

| 项         | 内容                                                                                                                                                                                                                                |
|------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | 主设计 §3.5 的 10 态：`Idle → Pending → Drain → Invalidate → Layout → Paint → Damage → Submit → WaitGpu`                                                                                                                            |
| 偏差       | headless 版 3 态 + `FrameIndex`（唯一递增点在 `commit`）；`WaitGpu` / in-flight ring / `PlatformDispatcher::wake` 留 P09                                                                                                            |
| 原因       | 中间态全部发生在**一次同步调用内部**（没有平台事件循环、没有异步边界），没有任何观察者；把它们造成状态只会产生"永远观测不到"的变体。in-flight 轮转的**契约**（`index.get() % 3` 槽、retire 释放）写在 §9.1，P09 扩展变体时 additive |
| 修订后设计 | phase-06 §9.1 / §9.2；`FrameIndex` 只在 `commit` 递增（I3 的"空闲零帧不增号"由此保证）                                                                                                                                              |

### S-60 paint pass 归属 `src/paint/pass.rs`（crate 私有）

| 项   | 内容                                                                                                                                                                 |
|------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | 新增 `paint/pass.rs`（`pub(crate) mod pass;`），`PaintPass` / `PaintReport` 放在这里；`window.rs` 只做帧编排（begin/layout/paint/commit/abort）                      |
| 原因 | paint pass 是「style + patch + text + canvas + geometry → `Primitive`」的单一职责；塞进 `window.rs` 会让后者混入绘制语义（AGENTS §1.3 模块边界），并把它推到 700+ 行 |
| 代价 | 超出 brief 的文件清单一项（裁决 ⑤）；文件边界门禁（§13-9）已把它列入允许集合                                                                                         |

### S-61 一节点一 lane；lane 自包含祖先 clip 前缀；空 lane 粘性；释放只随卸载

| 项   | 内容                                                                                                                                                                                                                                                                                                                                  |
|------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | 每个有绘制内容的节点一条 lane（`Node.lanes[0]`，容量仍 2）；lane 内容 = 祖先 `Clip(Push)*` + 本节点图元 + 逆序 `Clip(Pop)*`；lane 分配后不因"本帧无图元"释放（只 `clear_lane`），节点卸载时经 `NodeTree::removed_lanes` 归还                                                                                                          |
| 原因 | ①`REUSABLE` 的语义是"整桶复用"，一节点多 lane 会让一次失效写多条、`reused_lanes` 判定复杂化；②clip 若只在本节点 lane 内 push/pop，子 lane 不在作用域内（lane 是独立桶），因此必须把祖先链展开成每 lane 的前缀（C1「lane 内部闭环」）；③粘性 lane 让 `Scene::order` 跨帧稳定，oracle 的 `node` 键可直接比较，也避免 lane 反复分配/释放 |
| 代价 | clip 前缀在 CPU 侧重复（GPU 侧由 `ClipChain` 去重，P02 §8.7）；典型 depth ≤ 1                                                                                                                                                                                                                                                         |

### S-62 `Scene` 的写面**不收紧**；新增 `Scene::clear_lane` 同为 `pub` + engine-internal doc

| 项         | 内容                                                                                                                                                                                                                                                                                                            |
|------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策       | `new_lane` / `release_lane` / `push` / `clear_frame` / `set_order` / `clear_order` 保持 P03 的 `pub`（doc 首行 "engine-internal, 应用层不得调用"）；`clear_lane` 照此新增                                                                                                                                       |
| 原因       | P06 之后 `new_lane`/`push`/`set_order`/`release_lane` 有了 crate 内调用者，但 `clear_frame`/`clear_order` 仍**只**有 `feature = "test-support"` 的 oracle 调用者。只收紧一半会让写面分裂成两套可见性规则（比 S-33 现状更难审计）；全部改 `pub(crate)` 则 `clear_frame`/`clear_order` 会在默认构建里 `dead_code` |
| 修订后设计 | phase-06 §4.3；P03 S-33 的「pub 仅为满足 dead_code」注记被本条扩展为「写面统一 pub + engine-internal」                                                                                                                                                                                                          |

### S-63 `test-support` 的模块划分与两种记录类型

| 项       | 内容                                                                                                                                                                                                                                                                                                         |
|----------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策     | `lib.rs` 加 `#[cfg(feature = "test-support")] pub mod test_support;`；`test_support.rs` 只有 5 行 `pub mod`（`app` / `input` / `probe` / `record` / `snapshot`）；`FrameRecord`（调度记录）与 `RenderedFrame`（绘制快照）**分型**，`FrameRecord.frame: Option<RenderedFrame>`                                |
| 原因     | ①`pub use` 被 workspace lint 禁止（M-02），无法把 5 个模块压平成一个命名空间，模块划分必须显式；②调度事实（`rendered` / `damage` / `stats` / `paint_log`）与绘制事实（`quads` / `primitives` / `debug_bounds`）的生命周期不同：空闲帧只有前者，提交帧两者都有，混在一个类型里会让"本帧未提交"只能靠空 Vec 猜 |
| 公开路径 | `ui_core::test_support::{app::TestApp, app::TestWindow, probe::{Probe, Selector}, record::…, input::…, snapshot::…}`（S-16 闭包表见 phase-06 §4.2）                                                                                                                                                          |

### S-64 全帧 damage 的触发点 = `set_size` / `set_scale` 的 `full_damage_pending`

| 项         | 内容                                                                                                                                                                                                                                                                                                                |
|------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策       | `Window` 新增 `full_damage_pending: bool`；`set_size` / `set_scale` 置位，`paint_frame` 在 `damage.reset` 之后消费它并 `push_full()`                                                                                                                                                                                |
| 原因       | 尺寸/DPI 变化会重建 framebuffer（P09 的 swapchain），局部 rect 的坐标系整体失效（P01 §7.2 / 主设计 §5.7 的"窗口尺寸/DPI → 全帧"）。用显式 flag 而不是"比较 `scale_epoch`/`size` 与上一帧"，是因为后者在**多帧未提交**（错误中止）时会把"已经全帧过一次"误判成"还需要全帧" —— 语义上两者都不能漏，但 flag 只需要一次 |
| 修订后设计 | phase-06 §9.5 / §8.5；用例 §12-F4                                                                                                                                                                                                                                                                                   |

### S-65 编码器自检承担 `InstanceBuffer` 内部字段的非 test 读点

| 项   | 内容                                                                                                                                                                                                                                                                                                                                            |
|------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | `InstanceEncoder::encode` 末尾做三项 `debug_assert!`：`Σ FrameOp::Instances.len == instances.len()`、每个 `FrameOp::Geometry(i)` 的 `i < geometry.len()`、每个 `ClipChain.parent` 的下标 < 自身下标                                                                                                                                             |
| 原因 | `InstanceBuffer` 是 crate 私有（P02 §13.2），P06 没有 GPU 上传路径，`instances/clips/geometry/ops` 如果只写不读会触发 `dead_code`。probe（§2.2）实测 `debug_assert!` 内的字段读取**计入**使用（`cfg!(debug_assertions)` 是运行期常量，HIR 里两个分支都在）。这三条同时也是真实不变量：`FrameOp` 必须恰好覆盖全部实例与几何绘制，clip 链必须无环 |
| 代价 | 每次 `encode` 一次 O(#ops + #clips) 的断言（debug 构建；release 下 `cfg!` 为 false，代价是一次可预测分支）                                                                                                                                                                                                                                      |

### S-66 `clip_key` 侧表：lane 复用的 clip 上下文守卫（审查 B1）

| 项   | 内容                                                                                                                                                                                                                                                                                                                                                                     |
|------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | `NodeTree` 新增侧表 `clip_keys: Vec<Option<(u8, Option<PhysicalRect>)>>`；paint 对每个节点在 `REUSABLE` 短路**之前**比较 `(depth, covering)` 与当前递归携带的 clip 上下文，不等即重建并写回。`set_style` 不再需要"clip 变化 → 子树失效"的特殊逻辑                                                                                                                        |
| 原因 | `Style.clip` 只表达"裁不裁"，lane 前缀是**具体矩形**。窗口 resize / 分栏拖拽 / P13 的布局写入会改变 `Clip::Bounds` 容器的 padding box，而子节点的绝对盒可能逐位不变（`absolutize` 的 `changed` 为假）⇒ `REUSABLE` 保持置位；只比较开关会复用旧矩形，把子内容裁到错误区域（stale drawing）。侧表与 `layout_cache`/`text_slots` 同一生命周期纪律（槽位复用/卸载置 `None`） |
| 代价 | 每节点 1 槽（`Option<(u8, Option<PhysicalRect>)>` ≈ 20 B，只对曾重建的节点非空）；每次 paint 每节点一次比较（O(1)）                                                                                                                                                                                                                                                      |
| 验证 | 用例 `clip_key_mismatch_after_resize_or_reparent`（phase-06 §12-E5）：resize 改变滚动容器尺寸、重挂子树到 clip 节点下两种情况都必须重建且前缀矩形更新                                                                                                                                                                                                                    |

### 十一·待复核项（P06 新增）

| 编号 | 内容                                                                                                        | 复核 Phase | 触发条件 / 处置                                                                                                                        |
|------|-------------------------------------------------------------------------------------------------------------|------------|----------------------------------------------------------------------------------------------------------------------------------------|
| V-48 | `Node` **1128 B**（余 24 B）：`paint`(8) + `content_offset`(8) 之后，任何再加内联字段的 Phase 都会顶破 1152 | P07 / P08  | 触发即改 `style: Box<Style>`（公开面不变，P03 V-30 的备选）或把字段移到 `NodeTree` 侧表（`debug_ids`/`removed_lanes` 是现成范例）      |
| V-49 | 每帧 O(n)：paint 递归全树 + 文本两 pass（P05 V-45）+ 每图元 `resolve`（P02 R7）                             | P13 / P18  | 触发（虚拟列表 / 基准热点）→ `DirtyFlags` 的 2 个保留位加 `TEXT` / `PAINT` 增量遍历；lane 内缓存 `LinearPremulRgba`（不改 128 B 契约） |
| V-50 | `Canvas` 在每次 lane 重建时 `clone`（48 B + `Vec<Primitive>` 深拷贝）                                       | P15        | 出现 > 1k 图元的画布 → `NodeKind::Canvas(Rc<Canvas>)`（additive，公开面只多一个构造器）                                                |
| V-51 | `opacity` 是**逐图元 alpha**（非 group opacity）；`Canvas` 内部图元不参与                                   | P15        | 需要整组淡出（模态遮罩/交叉淡入）→ 离屏合成或 `Canvas` 层 alpha，属渲染器能力                                                          |
| V-52 | damage 的**节点级** push 对隐藏/无图元节点也生效（过绘，安全侧）                                            | P18        | I4 的 damage 比不达标 → 只在 `rebuilt && lane 非空` 时 push；前提是先证明"图元减少"仍被节点级 rect 覆盖                                |

**审查二轮新增**（PASS-WITH-FIXES：B1 空 clip、N1/N2/N3）：

| 编号 | 裁决 / 偏差                                                                                                                            | 理由                                                                                                     |
|------|----------------------------------------------------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------|
| F-67 | `Clip::Bounds` 的 padding box 为 0×0 时**仍入栈**（depth+1、covering 交空、子 lane 带空前缀）                                          | 与 `hit_test` 的 clip 链同构；跳过会让子内容"越界可见"（审查 B1，阻塞项）                                |
| S-67 | **无图元的节点不留 lane**（重建后 `release_lane` + 清 `Node.lanes`）；复用判据去掉 `lane_of().is_some()`，改靠 `REUSABLE` + `clip_key` | 空 lane 只占 `Scene::order`/内存；空窗口首帧必须能判空闲（审查 N3）                                      |
| V-53 | `FrameOp::Geometry.clip_rect` 用 `[u32; 4]`（f32 位模式）而非 `[f32; 4]`                                                               | `FrameOp` 需要 `Eq`（测试按 `matches!` 断言 + 去重），f32 无 `Eq`；P07 侧 `from_bits` 还原（成本可忽略） |

### 实施记录（2026-11-12，P06 落地）

| 项                               | 实测                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
|----------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 新增文件                         | `src/paint/{instance,pass}.rs`、`src/render/damage.rs`、`src/test_support.rs` + `src/test_support/{app,input,probe,record,snapshot}.rs`、`tests/frame_record.rs`                                                                                                                                                                                                                                                                                                                                                                                            |
| 修改文件                         | `Cargo.toml`（`[features] test-support` + `[[test]] frame_record`）、`lib.rs`、`paint.rs`、`render.rs`、`paint/{style,color,geometry,scene}.rs`、`node/{tree,builder}.rs`、`window.rs`、`app.rs`、`math.rs`                                                                                                                                                                                                                                                                                                                                                 |
| 测试                             | 审查二轮修复后：`cargo test -p ui_core` **418**；`--features test-support` **427 + 集成 5**；`--release` **409**；`--release --features test-support` **418 + 集成 5**（较 P05 基线 364 新增：默认 54 / feature 63 / 集成 5）                                                                                                                                                                                                                                                                                                                               |
| 审查二轮                         | B1（空 clip 入栈）+ N1/N2/N3 全部落地，逐条见上方「审查二轮新增」表与 phase-06 §17；回归用例 `zero_size_clip_still_pushes_and_empties_damage`、`transparent_gradient_emits_nothing`、`resize_forces_full_damage`、`text_glyphs_use_border_and_padding_offset`、`image_maps_to_atlas_quad`                                                                                                                                                                                                                                                                   |
| 门禁                             | clippy（默认与 feature 的 `--all-targets -D warnings`）0；`fmt --check` 0；`doc --no-deps`（两配置）0；三平台 `cargo metadata` rc=0；`Cargo.lock` **零变化**；零 `unsafe`/`#[allow]`/`#[expect]`/`serde_json`/`#[path]`/`mod.rs`/`pub use`（`render.rs` 的 `pub(crate) use` 属 M-07 白名单）                                                                                                                                                                                                                                                                |
| `Node` 尺寸                      | 实测 **1128 B**（1112 + `paint` 8 + `content_offset` 8）≤ 1152（余 24 B）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| 实现期修正（相对 phase-06 计划） | ①`FrameScheduler::{state, skipped}` 与 `Window::frame_state` **不落**（无默认构建消费者；测试用 `FrameOutcome`/`frame_index` 断言，见 phase-06 §9.5）；②**空 `Canvas` 节点也 push `Primitive::Canvas`**（`bounds` 参与 damage，P02 §11.3），编码器展开后 0 实例；③`rebuilt` 只统计**产生像素或有 damage** 的节点（空 lane 不触发提交，否则空窗口首帧无法判空闲）；④`FrameStats` 去掉 `geometry_draws`（无消费者）；⑤`paint/pass.rs` 的 `run` 末尾补 `scene.set_order`（计划文字里隐含、实现必须显式）；⑥`FrameOp::Geometry.clip_rect` 用 f32 位模式（V-53） |
| 遗留（已知、已登记）             | ①集成文本用例依赖宿主字体（`tofu == 0` 断言，V-46）；②oracle 只比较 `primitives`/`quads` 序列（damage 必然不同，设计 §3.8 的范围）；③damage 的节点级 push 偏保守（V-52）；④`InstanceBuffer` 的 `as_bytes`/`instances()`/`clips()`/`geometry()` 等 P07 上传访问器**尚未提供**（无消费者）；⑤`Geometry` 的 transform 恒 `IDENTITY`（F-62）                                                                                                                                                                                                                    |

---

## 十二、Phase 07 三套 shader / `build.rs` / 渲染契约（2026-11-12）

> P07 只做设计 + 本机 dxc 核验（交付 `design/phase-07-shaders.md`，不落生产代码）；下列条目在 P07 实现期生效。
> 事实基线：Windows SDK **10.0.26100.0** / `dxcompiler 1.9.2602.17`；7 个 entry 以 `-WX` 全部编过（合计 187 ms），
> 产物 4164–7864 B，magic `DXBC`；MSL/GLSL 本机无编译器（静态审查）。

### F-68 geometry per-draw 常量 48 B → 80 B，并与帧 uniform 合并为 112 B `RootUniform`

| 项         | 内容                                                                                                                                                                                                                                                                                                                                                   |
|------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | `phase-02-paint-foundation.md` §9.5：`GeometryUniform`（32 B：`basis` + `translation`）+ `GeometryColor`（16 B），P09 相邻写入同一条 48 B 常量区                                                                                                                                                                                                       |
| 偏差       | 改为 **`GeometryDrawUniform`（80 B）**：`basis`/`translation`/`color0`/`clip_rect`/`clip`（`uint4`，`.x = clip_index`）；并与 `FrameUniform`（32 B）合并为 **112 B `RootUniform`**（`frame @0`、`draw @32`）。三平台各一个槽位：D3D12 root constants `b0`（28 DWORD）/ Metal `buffer(2)`（112 B）/ Vulkan push constants `[0,112)`（≤ 规范下限 128 B） |
| 原因       | ①`Primitive::Geometry` / `Path` 的绘制必须带 clip（P06 已把 `clip_index` + `clip_rect` 放进 `FrameOp::Geometry`，V-53），48 B 布局**无处传**；②圆角 clip 在 geometry 通道同样要按 `ClipEntry` 链求距离，`clip_index` 不可省；③合并成一块后三平台的常量布局逐字节一致，且不必为 quad 单独维护第二套常量绑定                                             |
| 修订后设计 | `design/phase-07-shaders.md` §2.7（布局表）/§2.9（绑定表）/§4.2（变换链）；P09 上传 112 B，`GeometryUniform`/`GeometryColor` 两个类型由 `GeometryDrawUniform` 取代                                                                                                                                                                                     |

### F-69 clip 链的 GPU 侧镜像 `ClipEntry`（48 B，丢 `aabb`、`parent` 进 `uint4`）

| 项         | 内容                                                                                                                                                                                                                                                                                                                                               |
|------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 原文       | `phase-02-paint-foundation.md` §8.7 只定义 CPU 侧 `ClipChain { parent, rect, corner_radius, aabb }`；`GpuPrimitive.clip_index` 指向「完整链」，但 GPU 侧布局未定                                                                                                                                                                                   |
| 偏差       | GPU 侧为 **`ClipEntry` 48 B**：`rect: float4` / `corner_radius: float4` / `meta: uint4`（`.x = parent`，`.yzw` 保留恒 0）。**丢弃 `aabb`**（已由 `GpuPrimitive.clip_rect` 冗余），`parent` 用 `uint4` 占满 16 B                                                                                                                                    |
| 原因       | ①shader 只需要「每层的 rect + 半径」求距离，`aabb` 在 primitive 上已有一份；②16 B 成员的布局在 cbuffer/std140/std430/MSL 下逐字节一致（§2.1 的硬约束）；③表项 0 仍是哨兵：`clip_index == 0` 时 shader 直接返回 1，**不读表项 0 做数学**——Rust 哨兵 `aabb = [MIN,MIN,MAX,MAX]` 按其 `(x,y,w,h)` 语义算出的右/下边界是 `MIN+MAX = 0`，不是无穷大矩形 |
| 修订后设计 | `design/phase-07-shaders.md` §2.6（布局+哨兵陷阱）；P09 把 CPU 表转成 `ClipEntry` 数组上传，表项 0 必须存在                                                                                                                                                                                                                                        |

### S-68 GLSL 用 1 个 `common.glsl` + 7 个 stage 文件（不是单文件 `quad.glsl` / `geometry.glsl`）

| 项   | 内容                                                                                                                                                                                                                                                                            |
|------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | GLSL 侧共 **8 个文件**：`common.glsl`（无 `#version`）+ `quad.vert.glsl` / `quad.frag.glsl` + `geometry.ui.vert.glsl` / `geometry.textured.vert.glsl` / `geometry.ui.frag.glsl` / `geometry.textured_image.frag.glsl` / `geometry.textured_glyph.frag.glsl`                     |
| 原因 | GLSL 一个编译单元只能一个 stage、入口恒为 `main`、`#version` 必须在首行 ⇒ 无法像 HLSL/MSL 那样「一文件多入口」。共享函数只在 `common.glsl` 里出现一次（不复制），每个 stage 文件 `#version 450` + `#extension GL_GOOGLE_include_directive : require` + `#include "common.glsl"` |
| 代价 | 文件数从 3 增到 8；`build.rs` 的 `rerun-if-changed` 覆盖 14 个源文件；若发行版 glslang 不支持 `#include`，改 build.rs 做拼接式预处理（源码不动）                                                                                                                                |

### S-69 `apply_gamma` 在 v1 是恒等函数（gamma 归 sRGB 渲染目标）

| 项   | 内容                                                                                                                                                                                                                                              |
|------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | 三套源码都保留 `apply_gamma(linear)` 并在 FS 末尾调用，但 v1 的实现是 `return c;`。linear→sRGB 的**唯一**编码点是 **sRGB 格式的渲染目标**（D3D12 `*_UNORM_SRGB` / Metal `*.bgra8Unorm_srgb` / Vulkan `*_SRGB`），混合发生在编码前的 linear 值上   |
| 原因 | 主设计 §2.11 要求三套源码「函数同名同语义」；而 §5.8 又要求「不得在任一后端多/少做一次 sRGB 转换」。在 shader 内做编码会与 sRGB attachment 叠加成双重编码；删掉函数则偏离函数集契约。恒等实现同时满足两者，并把「为什么不能在这里转」写进源码注释 |
| 代价 | `apply_gamma` 目前是死代码（编译器会内联成空操作）。它存在的价值是**契约锚点**（主设计 §2.11 的函数集）与「编码点只有一处」的可审查性                                                                                                             |

### S-70 阴影 `σ = max(blur/3, 0.35)`（`blur` 是可见外扩半径，不是 CSS 的 2σ）

| 项   | 内容                                                                                                                                                                                                                                                                                                                                                                     |
|------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | `analytic_shadow(d, blur)` 内部取 `σ = max(blur/3, 0.35)`，`erf` 用 Abramowitz–Stegun 7.1.26（误差 ≤ 1.5e-7）；覆盖率 = `0.5·(1 − erf(d/(σ√2)))`                                                                                                                                                                                                                         |
| 原因 | F-13 / P02 §8.4 已把阴影的 damage 与顶点外扩冻结为 `params0.w = blur + 1`，即**可见外扩 = blur**。若按 CSS 惯例取 `σ = blur/2`，在 `d = blur+1`（2σ+1）处的残差是 **2.3% ≈ 6/255**，会在外扩边界留下可见的矩形硬边；`σ = blur/3`（3σ = blur）时残差降到 **2.2e-3 ≈ 0.5/255**（blur=0 时 σ 下限 0.35，残差 2.1e-3）。实测数值见 `phase-07-shaders.md` §0 第 8 条与 §8 R-9 |
| 代价 | 同一 `blur` 数值的阴影比 CSS 观感略紧（`base_ui` 的主题阴影按本契约校准）；`blur < ~1.05 px` 与 0 等价（σ 下限）                                                                                                                                                                                                                                                         |

### S-71 `ui_core_SKIP_SHADERS=1` 时写**占位产物**（不是不写产物）

| 项   | 内容                                                                                                                                                                                                                                                                                                                         |
|------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | 跳过编译时（`ui_core_SKIP_SHADERS` = `1`/`true`，其它值视为未设置并在 warning 里说明；或 `CARGO_CFG_TARGET_OS != host`），为**每个**预期产物写一个占位文件（内容 `KYTRIX-SHADER-SKIPPED\n`），同时 `cargo:warning=…`；并 `cargo:rerun-if-env-changed=ui_core_SKIP_SHADERS` / `KYTRIX_DXC`                                    |
| 原因 | P09 用 `include_bytes!(concat!(env!("OUT_DIR"), "/shaders/…"))` 嵌入产物；缺文件会让跨 target 的 `cargo check` 在**类型检查**阶段失败，而跳过模式的唯一目的就是让这种检查通过。占位文件让 `include_bytes!` 始终可解析；真跑起来会在 P09 的 magic 校验处得到 `EngineError::ShaderLoad`（显式失败，符合 I10 的「不静默降级」） |
| 代价 | 跳过构建的二进制不可运行（预期）；必须用 `rerun-if-env-changed` 防止占位文件在切回正常构建后残留                                                                                                                                                                                                                             |

### S-72 实例缓冲 = 只读 structured / storage buffer（不是 CBV）

| 项   | 内容                                                                                                                                                                                                                                                                      |
|------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 决策 | `GpuPrimitive` 数组统一为**只读、可下标寻址**的缓冲：D3D12 `StructuredBuffer<GpuPrimitive>` SRV `t0`（`StructureByteStride = 128`）/ Metal `device const GpuPrimitive* [[buffer(0)]]` / Vulkan `std430 readonly buffer`（set 0 binding 0）                                |
| 原因 | ①D3D12 的 CBV 单块上限 64 KB = **512 个实例**，而实例缓冲容量按历史峰值一次分配、只写不重建（主设计 §2.7），CBV 会直接顶破；②按实例做 CBV 偏移需要 256 B 对齐，128 B 的实例会浪费一倍；③SRV/SSBO/`device` 指针是三平台唯一都支持「按动态下标读一个 128 B 结构」的绑定形态 |
| 代价 | 实例缓冲必须 16 B 对齐（F-23 已要求）；D3D12 的 SRV 描述符走 descriptor table（§2.9），不能像 CBV 那样用 root descriptor 直接给地址                                                                                                                                       |

### 十二·待复核项（P07 新增）

| 编号 | 内容                                                                                                                                                                                                                    | 复核 Phase           | 触发条件 / 处置                                                                                                                                                                                   |
|------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|----------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| V-54 | MSL/GLSL **未在本机编译**（无 xcrun / glslang），只有 HLSL 经 dxc `-WX` 验证；另有三个外部不确定点：Vulkan 对 sRGB attachment 的混合空间、glslang 的 `GL_GOOGLE_include_directive`/`-I` 支持、dxc 升级后 `-WX` 的新警告 | P07（CI）/ P18（L3） | 新增 `shader-compile-linux` / `shader-compile-macos` 两个 CI job；L3 的「半透明叠加」「圆角 1 px」「字形 emoji」三个 case 是这三点的验收面；glslang 不支持 include 时改 build.rs 拼接（源码不动） |

### 实施记录（2026-11-12，P07 落地）

| 项           | 实测                                                                                                                                                                                                                                                                                                 |
|--------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 新增文件     | `..`（648 行）+ `..` + 14 个源文件（HLSL 3 / MSL 3 / GLSL 8）                                                                                                                                                                                                                                        |
| 修改文件     | `..` 只加 `build = "build.rs"`；无 `[build-dependencies]`；`Cargo.lock` **零变化**                                                                                                                                                                                                                   |
| dxc          | SDK 10.0.26100.0 / dxcompiler 1.9.2602.17；7 个 entry 全绿（`-WX`）；产物 4072–7864 B、全部 `DXBC` magic（build.rs 自检）                                                                                                                                                                            |
| 门禁         | `cargo test -p ui_core` **418 passed**（P06 基线不变）；`cargo +nightly clippy -p ui_core --all-targets -- -D warnings` rc=0（build script 在内）；`fmt --check` rc=0；`ui_core_SKIP_SHADERS=1 cargo check --target aarch64-apple-darwin` rc=0（占位产物）                                           |
| 负例         | 4/4 按预期失败：字段交换 / 常量改值 / 删字段 / `asuint(` 禁用 token；还原后 build 成功、7 产物重建（复现步骤见 `design/phase-07-shaders.md` §6.4）                                                                                                                                                   |
| 一致性       | 设计文档 §3.2–§3.4 的 HLSL 代码块与落盘文件逐字节相同；`contract.md` 的 6 个锚点由 build.rs 的 R4 守护                                                                                                                                                                                               |
| 已知限制     | ①`cargo check --target x86_64-unknown-linux-gnu` 本机 rc=101：`yeslogic-fontconfig-sys 6.0.1` 的 build.rs 需要交叉 pkg-config（P07 之前就存在，与 shader 无关；shader 侧已正确 skip）；②MSL/GLSL 未本机编译（无 xcrun / glslang）；③`cargo metadata --filter-platform x86_64-unknown-linux-gnu` rc=0 |
| 与设计的偏差 | 无新增 F/S。实现期细化并回写设计：MSL 用 `mix`；GLSL 整型常量用 `u` 后缀 + `common.glsl` 提供 `lerp`/`saturate` 包装；build.rs 实际 648 行                                                                                                                                                           |

---

## 十三、P08 · render 共享层 + Windows D3D12 + readback（2026-11-12）

### 13.1 文件清单

| 类别                              | 文件                                                                                                                                                                                                                                                                                                                                                  |
|-----------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 新增（共享层，`#[cfg(windows)]`） | `src/render/{device,frame,retire,compositor,timer}.rs`                                                                                                                                                                                                                                                                                                |
| 新增（Windows 后端）              | `src/render/windows.rs`（模块根）+ `src/render/windows/{device,queue,swapchain,pipeline,atlas,compositor,readback,timer}.rs`                                                                                                                                                                                                                          |
| 修改                              | `src/render.rs`（`mod` 行 + `backend` 别名）、`src/window.rs`（`Option<Renderer>` / `commit_frame -> Result` / `enable_offscreen_renderer`）、`src/app.rs`、`src/error.rs`（`GeometryRegister` / `AppInit`）、`src/paint/instance.rs`（`GpuClipEntry` + 访问器）、`src/test_support/{app,snapshot}.rs`（WARP 快照）、`Cargo.toml`（`Win32_Security`） |
| 未落地（按设计）                  | macOS/Linux 后端（P11）、Win32 事件循环 / 可见窗口（P09）；`Cargo.lock` **零变化**                                                                                                                                                                                                                                                                    |

### 13.2 设计期条目（F-70 … F-80、S-73 … S-75、V-55）

| 编号     | 内容                                                                                                                                                                                                                                                                                                                                                                            |
|----------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-70** | 主设计 §2.4 的 `CopySubresourceRegion1` 在 **D3D12 不存在**（那是 D3D11.3 `ID3D11DeviceContext1`）；`windows 0.62.2` 只暴露 `CopyTextureRegion`。P08 用 `CopyTextureRegion` + `D3D12_BOX`，区域求交在共享层 `render/compositor.rs`（不用 Win32 `IntersectRect`）                                                                                                                |
| **F-71** | 帧结构改为「**persist 是唯一绘制目标**，swapchain back buffer 只作 `COPY_DEST`（不建 RTV、不逐 scissor 画到 back）」；主设计 §2.4 的「copy 到 back 再画」被取代                                                                                                                                                                                                                 |
| **F-72** | `Present1` 三条**实现期实测**约束：①创建后/`ResizeBuffers` 后第一次 present 必须全量；②dirty rect 非空且完全在 surface 内；③`pPresentParameters` 不得为 NULL。违反 ①② → `0x887A0001`，违反 ③ → 进程终止                                                                                                                                                                         |
| **F-73** | `D3D12_FEATURE_DATA_SHADER_MODEL.HighestShaderModel` 是**入参**，必须先填期望上限（6_8）再查；填 `6_0` 会得到 `6_0`（本机实测会把 RTX 5080 误判为 SM 6.0）                                                                                                                                                                                                                      |
| **F-74** | `FrameIndex` 留在 `window.rs`；`render/frame.rs` 只有 `MAX_FRAMES_IN_FLIGHT` / `slot` / `InFlightRing`（D-1）                                                                                                                                                                                                                                                                   |
| **F-75** | `SnapshotError` 增 `Gpu { detail: SmolStr }`；`TestApp::snapshot` 在 `cfg(windows)` 用 WARP 真渲染，其余平台仍 `NoRenderer`                                                                                                                                                                                                                                                     |
| **F-76** | `Window::commit_frame` 返回 `Result<FrameOutcome, EngineError>`（GPU 错误冒泡；`App::frame` 的 `Some(Err)`）                                                                                                                                                                                                                                                                    |
| **F-77** | 新增 `EngineError::GeometryRegister { detail: SmolStr }` 与其**首个生产者**（`FrameOp::Geometry` 的 `GeometryId` 未注册；P09 的 `GeometryStore` 接上后语义不变）                                                                                                                                                                                                                |
| **F-78** | `InstanceBuffer` 追加 `ops()/geometry()/clips()` 与 `ClipChain::gpu_entry()`、`GpuClipEntry`（48 B）；P06 遗留 ④ 收口（非 test 消费者 = `render/windows/compositor.rs` 的录制循环）                                                                                                                                                                                             |
| **F-79** | `..` 的 windows features 追加 **`Win32_Security`**（`CreateEventW` 在 0.62.2 被该 feature 门控；缺了是 `E0425`）。**只改 feature 列表**，无新 crate，`Cargo.lock` 零变化                                                                                                                                                                                                        |
| **F-80** | persist 资源格式 = `B8G8R8A8_UNORM`、**RTV 视图格式 = `B8G8R8A8_UNORM_SRGB`**（编码一次；probe：`clear(0.5) → 188`）；**sRGB flip swapchain 在本机创建即失败**（`0x887A0001`）⇒ swapchain 固定 `*_UNORM`；`CopyTextureRegion` 的源/目标必须同格式，`GetCopyableFootprints` 用**资源格式**。主设计 §2.4 与 P07 §2.3 都未涉及「资源格式 vs 视图格式」与 flip swapchain 的格式限制 |
| **S-73** | `render/windows.rs` 是模块根（只 `mod` + `pub(crate) use`）；`Renderer` 定义在 `windows/compositor.rs`（lint `definition_in_module_root`）                                                                                                                                                                                                                                      |
| **S-74** | **不启用** SM 6.6 `ResourceDescriptorHeap` / Tier3 加速：WARP 报 SM 6.8/Tier3，启用会让 CI golden 与真机走不同 shader 路径（主设计 §6.4 的同一条警告）                                                                                                                                                                                                                          |
| **S-75** | descriptor 堆**静态分区**（每 in-flight 槽 4 个 SRV + 2 个备用，无动态索引回收）；`GpuResourceHandle` 的 `kind=2/3` 编码为 P09 预留，P08 只产生 `kind=1`                                                                                                                                                                                                                        |
| **V-55** | 「present 之后的 `ResizeBuffers`」在**本机非交互会话 + 隐藏窗口**下无法验证（`0x887A0001`，含 waitable/泵消息/释放全部引用三种尝试）；P08 以纯逻辑单测 + 「失败即 `EngineError::Swapchain`、不重试」处置，**P09 必须在可见窗口会话补实机验证**                                                                                                                                  |

### 13.3 实现期新增（F-81 … F-85、S-76 / S-77、V-56 / V-57）

| 编号     | 事实                                                                                                                                                                                                       | 处置                                                                                                                                    |
|----------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------|
| **F-81** | **`NumElements = 0` 的 buffer SRV 会移除设备**（`CreateShaderResourceView` 不报错，一帧后 readback 的 `CreateCommittedResource` / `Present1` 返回 `0x887A0005`）；空场景（`instances.len() == 0`）必然踩到 | `Pipeline::write_slot` 传 `count.max(1)`（缓冲容量恒 ≥ 1 元素）；`offscreen_full_frame_clears_transparent` 守住该路径                   |
| **F-82** | 全帧也用 `CopyTextureRegion`（逐矩形），不再用 `CopyResource`：persist→back 只有一条复制路径                                                                                                               | `copy_to_back` 统一走 `copy_rect`；与 F-71 一致                                                                                         |
| **F-83** | `Readback` 必须按**源格式**建缓冲：图集是 `R8_UNORM`，按 `B8G8R8A8_UNORM` 建 footprint 会让 copy 移除设备                                                                                                  | `capture_from(..., format, resident)`；`capture` = BGRA8 + `RENDER_TARGET`；R8 读回成 `[v,0,0,255]`                                     |
| **F-84** | 图集页尺寸参数化：产品 2048，测试 64。2048² glyph/image + 16 MB readback 在并行测试里拖垮 WARP                                                                                                             | `AtlasTextures::new(device, page)`；`Renderer::new_warp_offscreen` 用 `TEST_ATLAS_PAGE = 64`                                            |
| **F-85** | `GpuCaps::describe()` 保留为**全构建**诊断行（不再 `cfg`），由 `Device::from_adapter` 的 `debug_assert!` 消费；`GpuCaps` 不再存进 `Device`                                                                 | 删除 `Device::caps()/live_resources()` 与 `ResourceTable::live`（无消费者的状态不保留）                                                 |
| **S-76** | GPU 单测需进程级串行：并行创建多个 WARP 设备会互相拖垮（后续 `D3D12CreateDevice` 直接失败）                                                                                                                | `windows::device::gpu_test_lock()`（`static Mutex<()>`），每个 GPU 测试取锁                                                             |
| **S-77** | `FrameOp::Geometry` 只带 clip 信息（P06 契约），**不带填充色/变换**；P08 的 geometry 根常量 = 单位 basis + 白色 + clip rect                                                                                | 几何的着色/变换属 P09（`Scene → FrameOp` 需扩字段）；P08 只验证注册 + 绘制链路，不做像素断言                                            |
| **S-78** | P08 的共享层（`device/frame/retire/compositor/timer`）只在 `#[cfg(windows)]` 下编译：其余平台还没有后端消费者，无条件编译会让每个 `pub(crate)` item 成为 `dead_code`（P08 无 `#[allow(dead_code)]`）       | P11/P17 落地各自后端时同步解除该 `cfg`（`render.rs` 的模块注记）                                                                        |
| **V-56** | **WARP 设备不能 present 到 HWND**：`Present1` 直接 `0x887A0005`（`DXGI_ERROR_DEVICE_REMOVED`）；硬件设备创建成功                                                                                           | swapchain 往返测试改用硬件设备，无硬件时 skip（`Renderer::new_for_hwnd` 文档写明）                                                      |
| **V-57** | 隐藏窗口（本机非交互会话）+ 硬件设备的帧：录制结束后 `GetDeviceRemovedReason` 已是 `0x887A0001`，`Present1` 报 `0x887A0005`；同一序列在 probe（独立进程、同样隐藏窗口）通过 → 差异在会话/窗口状态          | `hidden_window_swapchain_accepts_a_full_present` 标 `#[ignore]`（理由指向 V-55），保留可手动跑的完整路径；实机 present 验收属 P09       |
| **V-59** | 被**移除**的设备会污染同进程后续创建：WARP 设备移除后，同一进程里再 `D3D12CreateDevice(WARP)` 会失败（此前多项 GPU 测试因此连带失败）；硬件移除相对稳定                                                    | ①设备丢失测试改用硬件设备，无硬件时 skip；②所有 WARP 渲染器测试在设备不可用时**跳过并诊断**（设计允许的策略），不把环境失败计为断言失败 |

### 13.4 实施记录（2026-11-12，P08 落地）

| 项           | 实测                                                                                                                                                                                                                                                                                                                |
|--------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 新增文件     | 14 个（共享层 5 + `render/windows.rs` + 后端 8），共约 3.4 k 行（含单测）                                                                                                                                                                                                                                           |
| 修改文件     | 8 个（`render.rs` / `window.rs` / `app.rs` / `error.rs` / `paint/instance.rs` / `test_support/{app,snapshot}.rs` / `Cargo.toml`）；`Cargo.lock` **零变化**；未 commit                                                                                                                                               |
| 测试         | `cargo test -p ui_core`：**466 passed / 1 ignored**（P07 基线 418 → +48；ignored = V-57）；`--features test-support`：475 + 5；`--release`：457 passed                                                                                                                                                              |
| WARP 实测    | `Device::new_warp()`：FL 12_1 / SM 6_8 / BindingTier 3 / `warp=true`；离屏帧 clear(0,0,0,0) → readback 全 0；图集脏行上传（R8，row 5 = `0xAB`、row 4/6 = 0）；局部 damage 帧不改区域外像素；`render_snapshot` 尺寸/字节数正确                                                                                       |
| 硬件实测     | RTX 5080：FL 12_2 / SM 6_8 / Tier 3；persist UNORM + SRGB RTV 单次编码；`CopyTextureRegion` persist→back 全绿（probe §2.6）                                                                                                                                                                                         |
| 门禁         | `cargo build -p ui_core` rc=0；`clippy --all-targets --all-features -D warnings` rc=0；`fmt --all --check` rc=0；`cargo doc --no-deps` rc=0；`ui_core_SKIP_SHADERS=1 cargo check --target {aarch64-apple-darwin, x86_64-unknown-linux-gnu}` rc=0（Linux 需 V-04 的 stub `pkg-config` + `PKG_CONFIG_ALLOW_CROSS=1`） |
| 死代码       | **`#[allow(` 0 处**（含 `dead_code` / `too_many_arguments`）、`#[expect(` 0 处、`#[path]` 0 处；`render` 全私有，公开面只多加 `Window::enable_offscreen_renderer()` 一个方法                                                                                                                                        |
| 与设计的偏差 | 见 13.3；设计文档本身未改动（`design/phase-08-d3d12.md` 与实现一致）                                                                                                                                                                                                                                                |
| 已知限制     | ①实机 present / resize 归 P09（V-55 / V-57）；②几何着色待 P09 扩展 `FrameOp`（S-77）；③`--release` 少 9 项（`cfg(debug_assertions)` 测试）                                                                                                                                                                          |

---

## 十四、P09 · Windows 平台层 + 最小 demo（2026-11-12）

> 设计见 `design/phase-09-win32-demo.md`；probe 在 `%TEMP%\kg-p09-win32`（仓库外）。
> 本节条目全部来自本机 Win32 probe（`kg-p09-win32` / `kg-p09-resize` 20 变体 / `kg-p09-resize2`）。

### 14.1 设计期条目

| 编号     | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
|----------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-86** | **`ResizeBuffers` 的 `0x887A0001` 根因是 COM 引用泄漏**：`windows 0.62.2` 把 `D3D12_RESOURCE_BARRIER` 的 union 载荷（`ManuallyDrop<D3D12_RESOURCE_TRANSITION_BARRIER>`）与其内层 `pResource`（`ManuallyDrop<Option<ID3D12Resource>>`）、以及 `D3D12_TEXTURE_COPY_LOCATION.pResource` 都做成 `ManuallyDrop`；**结构体 drop 不释放引用**。P08 的 `transition`/`copy_rect` 每帧对 back buffer 泄漏 3 个引用 ⇒ DXGI 判定"仍被引用" ⇒ `ResizeBuffers` 恒 `0x887A0001`（V-55 的真因）。**双层显式释放 + `allocator.Reset`/`list.Reset` 之后**，同尺寸与变尺寸 `ResizeBuffers` 都 `S_OK` 且可重复（`kg-p09-resize2` 实测）。泄漏点：`compositor.rs:675-689/715-722`、`atlas.rs:229-240/254-262`、`readback.rs:94-105/238-245`、`pipeline.rs:422` |
| **F-87** | **`PostMessageW` 队列上限 10,000**：第 10001 条失败 `0x80070718`（`GetLastError=1816 ERROR_NOT_ENOUGH_QUOTA`）⇒ 帧唤醒必须合并（`AtomicBool` 置位 + 只有首次 post）；`Dispatcher.wake()` 先置位再 post，post 失败也能被 drain-then-check 兜住                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| **F-88** | **user32 定时器粒度**：`SetTimer(16ms)` 实测 24.6 ms（13 次/320 ms）、`SetTimer(1ms)` 实测 16.0 ms（20 次/320 ms）⇒ P13 的动画 tick 用 1 ms + 容忍抖动；P09 不装定时器                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| **F-89** | `windows 0.62.2` 事实补充：**没有自由函数 `SetFocus`/`GetFocus`**（只能靠 `WM_SETFOCUS`/`WM_KILLFOCUS`）；`LoadKeyboardLayoutW` 不存在（只有 A）；`SendMessageW` 的 wparam/lparam 是 `Option<…>`；`CreateSwapChainForHwnd` 是 5 参返回 `Result`；`CF_UNICODETEXT` 在 `Win32::System::Ole` 且是 `CLIPBOARD_FORMAT(u16)`；`BeginPaint`/`InvalidateRect` 在 `Graphics::Gdi`                                                                                                                                                                                                                                                                                                                                                                  |
| **F-90** | **`WM_QUIT` 会被 `PeekMessageW` 取出并消费**：`DispatchMessageW(WM_QUIT)` 静默丢弃，之后 `GetMessageW` 永久阻塞（probe 首轮真事故）。泵循环必须在 `PeekMessageW` 后显式判 `msg.message == WM_QUIT` 再退出                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| **F-91** | **隐藏窗口可以 present**：独立进程里隐藏窗口首帧全量 `Present1` = `S_OK`，随后显示/dirty/越界三种调用与 P08 §2.6 完全一致 ⇒ V-57 是**进程内状态污染**（与 V-59 同源），不是"隐藏窗口不能 present"。V-56（WARP 不能 present 到 HWND）不受影响                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| **S-79** | `InputEvent` / `PointerButton` 从 `test_support::input` **迁到公开 `src/event.rs`**（`Event`/`InputEvent`/`Key`/`KeyState`/`Modifiers`/`ImeEvent` 同处），`test_support::app::inject` 改用公开类型；**删除**测试模块里的重复定义（AGENTS §1.1 不保留过时设计）                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| **S-80** | `platform/` 整体私有（I9），公开 `Application`/`AppHandle` 定义在 `app.rs`（它们在 §1.3 白名单里且必须可命名）；Windows 实现拆 `platform/windows/{event_loop,window,dpi,ime,clipboard}.rs`；`platform.rs` / `platform/windows.rs` 只做模块声明（M-06/M-03）                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| **S-81** | P09 **用系统标题栏**（`WM_NCCALCSIZE` 一律交 `DefWindowProcW`）；自绘标题栏 / `WindowControlArea` / 拖拽最大化留 P12。probe 实测两种模式的客户区（462×273 vs 802×602）都已记录，P12 只需切分支                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| **S-82** | 平台事件到应用层的唯一通道 = `App::drain_events() -> Vec<(WindowId, Event)>`（引擎已先做命中测试）；P10 的 `Action` 分发接在它上面，不新增 `App::on_*` 回调                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| **S-83** | 键盘输入走 `TranslateMessage` + `WM_CHAR`（死键/组合由 OS 完成一次），引擎**不**跑 `ToUnicodeEx`；probe 只把 `ToUnicodeEx` 的死键/组合语义记录为 P12 的备选（德语布局 `VK_OEM_5` → -1，随后 `VK_A` → `â`；`VK_OEM_6` 一次返回 2 个码元 ⇒ 缓冲区 ≥ 8 u16）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| **V-60** | resize 在本机已闭环（F-86），但**只在单一驱动/单会话**验证：多显示器混合 DPI、其它厂商驱动、`SIZE_MAXIMIZED` 路径未验证                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   | P12 的 L5 矩阵补；P09 保持"失败即 `EngineError::Swapchain`、不重试" |
| **V-61** | IME 只有消息通路可验：本机 `ImmGetContext` 返回有效 `HIMC`、`ImmSetCompositionWindow`/`ImmAssociateContextEx` 均 true，但**没有真实输入法输入**（组合串顺序、候选窗、去重未验证）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         | P12 用微软拼音/日文输入法补；P09 用 `WM_CHAR` 兜底 |
| **V-62** | demo 的 present 路径在无交互桌面的 CI 上不可验证（只能看 `Present1` 返回值，看不到像素）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  | CI 走 `--snapshot`（WARP 离屏 PNG）；present 属人工验收 |
| **V-63** | `WM_PAINT` 不验证会风暴：100 ms 内 **24,576** 次                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          | `wndproc` 的 `WM_PAINT` 分支只有 `BeginPaint`/`EndPaint`；写进 review checklist |

### 14.2 probe 矩阵（resize，每变体独立进程）

| 变体                                 | 帧内容                                                                                                                            | `ResizeBuffers`                                               |
|--------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------|
| 0                                    | 从不 present                                                                                                                      | `S_OK`                                                        |
| 1/2/3/4/5/7/8/9/10/11/12/13/14/15/16 | 只 `ClearRenderTargetView`（隐藏/可见、sync 0/1、立即/延迟 200 ms、`DwmFlush`、FLIP_DISCARD、dirty present、常驻 persist+RTV 堆） | 全部 `S_OK`，可重复                                           |
| 6                                    | `FRAME_LATENCY_WAITABLE_OBJECT` 创建 swapchain                                                                                    | 创建即 `0x887A0001`（本机不可用）                             |
| 17/18/19/21/22/23/24/25              | 任何 `CopyTextureRegion`/`CopyResource` 写入 back buffer（含 `Reset`、单层 `ManuallyDrop` 释放、FLIP_DISCARD）                    | 全部 `0x887A0001`                                             |
| 26                                   | 拷贝帧 → drop swapchain → 同 HWND 新建                                                                                            | 新建 `0x80070005 E_ACCESSDENIED`（旧 chain 仍被泄漏引用钉住） |
| `resize2`（F-86 修复）               | 拷贝帧 + 双层释放 + `Reset`                                                                                                       | **`S_OK`**；随后 present `S_OK`；变尺寸 `S_OK`                |

### 14.3 实现期新增（F-92、S-84 / S-85、V-64）

| 编号     | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
|----------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-92** | `..` 的 windows features 追加 **`Win32_System_Memory`**（`GlobalAlloc`/`GlobalLock`/`GlobalUnlock` 被该 feature 门控；剪贴板写入需要）。只改 feature 列表，无新 crate，`Cargo.lock` 除 workspace member 外零变化                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| **S-84** | 公开面形态定案：`Application::new()`（无参，tokio 句柄由 `App` 持有）+ **`Application::run(app: App)`**；事件只在 `App::on_frame(hook)` 里可见（`hook(&mut App)` 每轮调用一次，是 `drain_events` 的唯一消费者入口）；剪贴板是 `App::{set_clipboard_text, clipboard_text}`（`EngineError::Clipboard`）。设计稿写的 `run(build)` 闭包形态被否：闭包拿不到「建树 → 装钩子 → 进循环」这三步的顺序控制                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| **S-85** | 字形页上传在 P09 落地（P05/P08 遗留的「上传源有了、上传没接线」）：`GlyphAtlas` 记录每块 tile 的页面原点并 `write_page` 组装整页；`App::frame` 在 `layout_text` 之后整页上传（`Window::queue_atlas_upload` → `Renderer::queue_atlas_upload`）；`TestApp::snapshot` 用**同页尺寸**的临时 WARP 渲染器再传一次。整页而非脏行（P15 再改增量）。`new_warp_offscreen` 降为 `#[cfg(test)]`，快照改用 `new_warp_offscreen_with_page`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| **V-64** | **字形像素不可见（已修，根因在 P08 的图集分块上传）**：`AtlasTexture::upload` 把一次上传按 `STAGING_ROWS = 256` 行**分块复用同一块 staging**——CPU 写块 0 → 录制 copy0 → 写块 1 → 录制 copy1……；GPU 在全部录制之后才执行，**每条 copy 读到的都是最后一块的字节**。产品页 2048² 有 8 块，前 7 块的拷贝实际写的是最后一块（全 0），字形永远不可见；而 64²/256² 的测试页只有 1 块，P08 的测试因此全绿。控制实验完全吻合：整页填 255（各块相同）→ 可见；只填字形 bbox → 被最后一块的 0 覆盖 → 不可见；翻页/换页尺寸都无效。**修复**：`AtlasTexture.staging` 改为按**本次上传跨度**增长（`ensure_staging(device, slot, rows)`，初始 256 行，必要时一次性扩容），每段只录**一次** `CopyTextureRegion`，不再分块复用；`record_uploads` 因此多收一个 `&Device`。**回归**：`glyph_run_samples_the_uploaded_glyph_page`（页尺寸扫描 64/256/512/1024/2048，修复前 512 起必失败）+ `rendered_text_produces_visible_glyph_pixels`（黑底白字，读回断言亮像素数 > 0 且覆盖率 0.001…0.5）。修复后 demo 文本可见（截图见 `%TEMP%\kytrix-demo.png`） |

### 14.4 实施记录（2026-11-13，P09 落地）

| 项        | 实测                                                                                                                                                                                                                                                                                                                                                                                 |
|-----------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 新增      | `event.rs`、`platform.rs` + `platform/{dispatcher,event}.rs` + `platform/windows/{event_loop,window,dpi,ime,clipboard}.rs`、`crates/ui_core_demo/{Cargo.toml,main.rs,scene.rs}`；改 `app.rs`/`window.rs`/`error.rs`/`lib.rs`/`test_support/{input,app}.rs`、`render/windows/{queue,compositor,atlas,readback,pipeline,swapchain}.rs`、`Cargo.toml`（member + `Win32_System_Memory`） |
| 测试      | `cargo test -p ui_core` **491**（P08 466 → +25）/ 1 ignored；`--features test-support` **500 + 5**；demo 1                                                                                                                                                                                                                                                                           |
| demo 实机 | 窗口出现（标题 `Kytrix GUI · Phase 09`），客户区 1000×750（800×600 逻辑 @ 1.25 DPI），三个色块 sRGB 正确；`PrintWindow` 截图 `%TEMP%\kytrix-demo.png`（1018×797）                                                                                                                                                                                                                    |
| demo 离屏 | `cargo run -p ui_core_demo --features snapshot -- --snapshot out.png` → 800×600 PNG，色块像素正确                                                                                                                                                                                                                                                                                    |
| 门禁      | clippy `-D warnings` / fmt / doc / release / macOS check 全绿；Linux check 需 P08 的 stub `pkg-config`（环境既有约束，非 P09 引入）                                                                                                                                                                                                                                                  |
| 已知限制  | ①V-64 文本像素（P10）；②`WM_DPICHANGED` 只能合成验证（无第二显示器）；③IME 无真实输入法输入（V-61 不变）；④CI 无桌面时 present 只能人工验收（V-62 不变）                                                                                                                                                                                                                             |

---

## 十五、P10 · 交互机制 + Action 分发（2026-11-13）

> 设计见 `design/phase-10-interaction.md`；probe 在 `%TEMP%\kg-p10-interaction`（仓库外、独立 `CARGO_TARGET_DIR`、
> `--offline`）。
> 本阶段**只做设计 + probe**：未写仓库代码、未改 `Cargo.toml` / `Cargo.lock`、未 commit。§15.4 的实施记录待实现期追加。

### 15.1 设计期条目

| 编号     | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
|----------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-93** | **`Action` 的形态定死为引擎枚举 + `Custom(SmolStr)`**（`event::Action`：`FocusNext` / `FocusPrev` / `Dismiss` / `Custom`），**不是** `impl Action` trait + `TypeId` downcast（GPUI 形态）。`Action` 是 handler 表的键 ⇒ 必须 `Eq + Hash`；「怎么触发的」走 `ActionSource`（`Click { click_count }` / `Key` / `Drag { phase, delta }` / `Programmatic`），不进 `Action`。节点只持**声明**（`on_click` / `on_drag`），handler 集中在 `App::on_action`                                                                             |
| **F-94** | **滚动偏移不进 `StateStore` 的 `Transient`**（主设计 §3.7 / §8.4 的原文归类），改存 `Node.interaction.scroll.offset`。原因：`place_subtree` / `paint` / `hit_test` / `Probe` / a11y bounds 全都只拿得到 `&NodeTree`（`StateStore` 在 `App` 里）；存 store 则必须给这五个入口各加一个 `&StateStore` 参数，或者接受「store 一份 + tree 一份」的两个真值。`Transient` 的三条语义（不入依赖图、不写 `Signal`、不产生 `PAINT`）由 tree 侧槽等价实现（§7.1）                                                                          |
| **F-95** | **滚动偏移吸收进绝对布局盒**（`place_subtree` 的 `content_shift = -offset`，只在进出滚动容器时生效），代价 = 被移动子树的 lane 重建（清 `REUSABLE`，**不**标 `PAINT` ⇒ 不重算 `bind_style`）。主设计 §3.4 行 7「滚动偏移只改 uniform / clip，不改 lane 内容」在本引擎的 lane 模型下不可实现：lane 内图元持**绝对** bounds、lane 自带**祖先 clip 前缀**（P06 §5.4），给 lane 加「内容变换」会把视口 clip 一起平移（clip 在窗口空间、内容在内容空间）                                                                             |
| **F-96** | **probe：null 类光标 + `HTCLIENT` 会得到空光标（不可见）**。`DefWindowProcW(WM_SETCURSOR, lParam=MAKELPARAM(HTCLIENT, WM_MOUSEMOVE))` 返回 **0** 且 `GetCursor()` = `0x0`；类光标为 `IDC_ARROW` 时同一消息返回 0 但把光标设成箭头；非客户区（`HTLEFT` / `HTTOP` / `HTBOTTOMRIGHT`）由 `DefWindowProcW` 自行设尺寸光标并返回 **1**，`HTCAPTION` 返回 0 但设箭头。⇒ P10 把类光标改为 `IDC_ARROW`（`platform/windows/window.rs:289` 的 `HCURSOR::default()`）并**不加** `WM_SETCURSOR` 分支，只在泵里按「解析结果变化」`SetCursor` |
| **F-97** | **probe：双击 / 拖拽阈值的系统真值**：`GetDoubleClickTime()` = **500 ms**、`SM_CXDOUBLECLK` = `SM_CYDOUBLECLK` = **4**、`SM_CXDRAG` = `SM_CYDRAG` = **4**（`SM_MOUSEWHEELPRESENT` = 1，光标 32×32）。⇒ `InputThresholds::default()` 用这些值，平台在 `platform_init` 写入系统真值（`app.rs` 里 **0 个 Win32 类型**，I9）                                                                                                                                                                                                        |
| **F-98** | `Node` 新增**一个**字段 `interaction: Option<Box<NodeInteraction>>`（+8 B）：滚动槽 / 虚拟列表槽 / 浮层槽 / `on_click` / `on_drag` 全在同一个 `Box` 里，`Node` 从 **1128 → 1136 B**（P06 预算 1128 / 上限 1152）。`Node` 的手写 `Debug` 必须同步（N4 的口径：闭包打 `<closure>`）                                                                                                                                                                                                                                               |
| **S-86** | 交互状态的三处归属（唯一真值）：节点级槽（滚动 / 虚拟列表 / 浮层 / `on_click` / `on_drag`）→ `Node.interaction: Option<Box<NodeInteraction>>`；焦点 + 指针 → `Window`；handler 表 / `KeyBinding` 表 / 动画槽 / 时钟 / 阈值 → `App`。**禁止**第二份焦点、第二份偏移、第二份「谁在拖拽」                                                                                                                                                                                                                                          |
| **S-87** | `ScrollHandle` / `VirtualList` / `Overlay` / `DragState` / `AnimationId` 全部**携带 `WindowId`**：`NodeId` 只在一棵树内有意义（各窗口的 `Arena` 独立 ⇒ 跨窗口的 `NodeId` 可能偶然有效）。不匹配 ⇒ `EngineError::StaleNode`；窗口已关闭 ⇒ 静默（不是错误）                                                                                                                                                                                                                                                                       |
| **S-88** | Tab 序 = `tree.order()`（DFS 先序）过滤 `focusable`，**不做 `tab_index`**（第二排序会与 `ORDER` 脏位、虚拟列表动态挂载、浮层末尾插入三处冲突）；焦点环 = `Style.focus_ring: Option<FocusRing>` + paint 在**焦点节点自己的 lane 内**画（自动受祖先 clip 约束，不需要第二条绘制通道）                                                                                                                                                                                                                                             |
| **S-89** | 浮层 = **root 的末尾子节点**（层级 = 绘制序 = `order` 末尾，无 `z-index`）；`hit_test` 的逆序遍历因此天然优先命中浮层（`layout/hit_test.rs:444` 的既有用例就是按此契约写的）。点击外部只关**最上层** `OverlayDismiss::OutsideClick` 浮层并**消费**该次按下（否则会同时触发下层按钮）；`Escape` / `Dismiss` 动作可关任意层                                                                                                                                                                                                       |
| **S-90** | 虚拟列表：容器要求 `Clip::Bounds + position: relative`；行 = **绝对定位**子节点（`left:0 / right:0 / top:i*extent / height:extent`），**不用 `LayoutOverride`**（那是布局叶子，不能挂子节点，P04 §10.3）；`content = Size(viewport.width, count × extent)` **显式**写入滚动槽（taffy 无可滚动内容盒）；v1 只支持均匀行高                                                                                                                                                                                                        |
| **S-91** | 动画槽在 **`App`**（tick 需同时 `&mut StateStore` 与 `&mut NodeTree`）；被驱动的值落 `Transient`（`bind_style` 用 `read_transient`），重绘由**显式注册的节点集合**的 `mark_paint` 给出（对应主设计 §3.7 的「显式注册」，不把时间接进 `dependents`）；平台用 `SetTimer(1 ms)` + `WM_TIMER` 唤醒、按 `has_animations()` arm/disarm（probe：1 ms → 16.0 ms / 320 ms 内 20 次；`KillTimer` 后 60 ms 内 0 次）                                                                                                                       |
| **S-92** | `InputEvent` 的 `PointerMove` / `PointerDown` / `PointerUp` / `Scroll` 增加 `modifiers: Modifiers`（平台侧 `TranslateState::modifiers()` 已有真值）；`InputOutcome` 从 `test_support::input` 迁到公开 `event.rs`（`{hit, action, target, click_count}`），**删除** `test_support/input.rs`（与 S-79 同型处置）；`TestApp::inject` 与平台走**同一个** `App::dispatch_input`                                                                                                                                                      |
| **S-93** | `platform/windows/event_loop.rs` 的 P10 增量：`WM_MOUSELEAVE` → `PlatformEvent::PointerLeave`（`TrackMouseEvent(TME_LEAVE)` 在首次 move 时注册）、`WM_TIMER`（`wParam == 1`）→ `dispatcher.wake()`、每轮 `SetCapture`/`ReleaseCapture` / `SetCursor` / `SetTimer`/`KillTimer` / `app.sync_clock()`。`wndproc` 仍**不调用 `App`**（只写 `WndProcState` + `wake`），沿用 P09 §5.2 的纪律                                                                                                                                          |
| **S-94** | `interaction.rs` 是**公共模块根**（P01 §4.2），契约类型定义在根文件、实现放 `interaction/{focus,scroll,virtual_list,overlay,drag,animation}.rs`；但**每个类型只有一个 `impl` 块**（`multiple_inherent_impl = "forbid"`）——交互方法必须写进 `app.rs` / `window.rs` / `tree.rs` 里**已存在的** impl，子模块只放 `pub(crate)` 自由函数                                                                                                                                                                                             |
| **S-95** | 编号对齐：`node/tree.rs:319` / `:949`、`layout/hit_test.rs:444`、`node/tree.rs:1180` 的「P13」与 `paint/style.rs:101` / `:107` 的「P12」在本阶段一并改为 **P10**（纯注释）；`app.rs:122` 的 `P11/P17` 与 `render.rs` / `platform.rs` 的端口编号**不动**（属另一条编号线）                                                                                                                                                                                                                                                       |

### 15.1b 上级裁决修订（2026-11-13，实现前）

> 上级裁决：**A1/A3/A4/A5 采纳，A2 不采纳**。A2 原方案（滚动偏移吸收进 `layout.bounds`）被否，改为 **paint 期偏移**；
> 本节追加修订，§15.1 的 F-94/F-95 保留历史但**以本节为准**。

| 编号      | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
|-----------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-99**  | **滚动偏移改为 paint 期应用**（上级裁决）：偏移 = `Node.interaction.scroll.offset`（`Transient` 语义：不登记依赖、不写 `Signal`、不标 `LAYOUT`/`PAINT`、不跑 taffy、不跑 `refresh_styles`）；`layout.bounds` 恒为**内容坐标**，`place_subtree` **不感知**滚动。应用点只有两处：`paint_node` 的**累计偏移**（图元 bounds 与 clip 矩形同步平移，容器自身 padding box 用祖先累计、子树内部用含自身偏移的累计）与 `hit_test::pick`（屏幕点 → 内容点）。damage **只**来自滚动视口矩形（`Window.pending_damage`），`FrameIndex` 照常推进                                                                     |
| **F-99a** | **「仅重写实例、不重建 lane」未采用，根因已证实**：本引擎的 lane 头部嵌**祖先 clip 前缀**（P06 §5.4，矩形在**窗口空间**），而内容图元在**内容空间**；给 lane 加单个平移会把视口 clip 一起移走，反向把外层 clip 反算进内容空间则需要 `Scene` 增加 per-lane 平移 + 编码器对 8 类 `Primitive`（含 `GlyphRun` 的逐字形位置）逐个平移 —— 属 P06/P07 的契约级改动（含三套 golden），超出 P10 边界。**采用的替代**：paint 发现 `Node.lane_offset != 当前累计偏移` 时**重建该 lane**（不重算样式、不跑布局），damage 仍只算视口；代价上界 = 滚动容器子树内**已挂载**节点数（虚拟列表下 = 视口行数 + overscan） |
| **S-96**  | `Window` 增加 `pending_damage: Option<Rect>`（逻辑像素，并集）+ `needs_frame` 的第三项判据；`NodeTree` 增加 `lane_offset` 戳（`Node` 侧 1 个 `Point`，放 `NodeInteraction` 内）+ `set_lane_offset` / `lane_offset`。两者都只有非 test 消费者（`paint_frame` / `paint/pass.rs`）                                                                                                                                                                                                                                                                                                                        |

### 15.2 probe 记录（`%TEMP%\kg-p10-interaction`，2026-11-13）

> 环境：Windows，**非交互式 agent 会话**；`windows 0.62.2`（`Win32_Graphics_Gdi` 是 `WNDCLASSW` / `RegisterClassW` 的
> feature 门控）；
> `cargo run --offline`（crate 源在本地 registry 缓存）。

| #  | 实测项                                          | 结果                                                                                                                                                     |
|----|-------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------|
| 1  | `LoadCursorW` 8 个 `IDC_*`                      | ARROW `0x10003` / HAND `0x1001f` / IBEAM `0x10005` / SIZEWE `0x10011` / SIZENS `0x10013` / NO `0x10017` / SIZEALL `0x10015` / CROSS `0x10009`，全部 `Ok` |
| 2  | `SetCursor` 往返                                | `SetCursor(arrow)` 返回前值 `0x10007`；`GetCursor()` = `0x10003`（相等）⇒ 光标是**线程状态**，可在泵里随时覆盖                                           |
| 3  | `SetCursor(None)`                               | `GetCursor()` = `0x0` ⇒ 传 `None` = 清空光标（不可见），引擎永不这么干                                                                                   |
| 4  | 类光标 = null + `WM_SETCURSOR(HTCLIENT)`        | `DefWindowProcW` 返回 **0**，`GetCursor()` = **0x0**（空光标）                                                                                           |
| 5  | 类光标 = `IDC_ARROW` + `WM_SETCURSOR(HTCLIENT)` | 返回 **0**，但 `GetCursor()` = arrow ⇒ 默认光标交给系统即可                                                                                              |
| 6  | 类光标 = null + 非客户区                        | `HTLEFT` → 返回 **1**、光标 `0x10011`；`HTTOP` → 1、`0x10013`；`HTBOTTOMRIGHT` → 1、`0x1000d`；`HTCAPTION` → 0、`0x10003`                                |
| 7  | `WM_SETCURSOR` 解码                             | `wParam` = hwnd；`LOWORD(lParam)` = **1**（`HTCLIENT`）；`HIWORD(lParam)` = `0x0200`（`WM_MOUSEMOVE`）                                                   |
| 8  | `SetCapture` / `GetCapture` / `ReleaseCapture`  | 隐藏窗口上：`during == hwnd`；`ReleaseCapture()` = `Ok(())`；after = `0` ⇒ 拖拽捕获可落地、可单测                                                        |
| 9  | `TrackMouseEvent(TME_LEAVE)`                    | `Ok(())`（无真实鼠标 ⇒ `WM_MOUSELEAVE` 本身未验证，V-66）                                                                                                |
| 10 | 阈值真值                                        | `GetDoubleClickTime()` = **500 ms**；`SM_CXDOUBLECLK` = `SM_CYDOUBLECLK` = **4**；`SM_CXDRAG` = `SM_CYDRAG` = **4**；`SM_MOUSEWHEELPRESENT` = 1          |
| 11 | `MSG.time` / `GetMessageTime()`                 | 两者相等且非零（`1090101703`）⇒ 未来做事件合并可用；P10 用 `Instant`（测试可注入）                                                                       |
| 12 | `SetTimer(1 ms)`                                | 320 ms 内 **20** 次 ⇒ 实测周期 **16.0 ms**；`KillTimer` 后 60 ms 内 **0** 次（复验 P09 F-88）                                                            |
| 13 | `SetTimer(16 ms)`                               | 320 ms 内 **12~13** 次 ⇒ **24.6~26.7 ms** ⇒ 否掉「用 16 ms 定时器做 60 fps」                                                                             |

### 15.3 未验证项（不阻塞）

| 编号 | 未验证                                                                                        | 后续                                                                                                                           |
|------|-----------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------|
| V-65 | `DirtyFlags::ORDER` 在 P10 仍**没有清除者**（`settle_frame` 只清 `LAYOUT                      | PAINT`），会累积到 P11                                                                                                         | 位是 `pub const`（不触发 `dead_code`）；`VirtualList` / `Overlay` 的单测断言被置位即可 |
| V-66 | `WM_MOUSELEAVE` 的真实投递（需要真实鼠标移出客户区；probe 只验证 `TrackMouseEvent` 调用成功） | 后续的 L5 实机矩阵                                                                                                             |
| V-67 | 拖拽移出窗口/跨屏时 `SetCapture` 的实际投递（无交互桌面）                                     | 同上；单测只覆盖「捕获请求的真值序列」                                                                                         |
| V-68 | 真实滚轮/触控板的高频小增量（±40 等）：平台的 `/120.0` 已保留小数（P09 落地），本阶段不改     | 实机验收                                                                                                                       |
| V-69 | 系统定时器在 120/144 Hz 显示器上的抖动；P10 **不承诺 60 fps**（实测 16.0 ms）                 | 动画按 `Instant` 插值（抖动只影响平滑度）；若不可接受 → 后续 Phase 换 `DwmFlush`/`WaitForVBlank`（不改 `AnimationClock` 契约） |
| V-70 | 浮层**不做** collision avoidance（超出窗口不翻转）                                            | 应用用 `place_overlay` 自行修正；tooltip 需要时再加                                                                            |
| V-71 | 拖拽**不做**节流与速度采样 ⇒ 无「抛掷」惯性（只有滚轮的缓动到位）                             | 触控板/惯性滚动属后续 Phase；`AnimationClock` 已能承载                                                                         |
| V-72 | 焦点环颜色不在 `StylePatch` 里 ⇒ 主题切换需重挂载                                             | 主题 Phase 的批量失效会覆盖（把 `FocusRing.color` 接到 `ThemeHandle.role(...)`）                                               |

### 15.4 实施记录（2026-11-13，P10 落地）

| 项           | 实测                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
|--------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 新增         | `src/interaction.rs` + `interaction/{focus,scroll,virtual_list,overlay,drag,animation}.rs`；`test_support/input.rs` **删除**（`InputOutcome` 迁 `event.rs`）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| 修改         | `event.rs`（`Action`/`KeyBinding`/`ActionHandler`/`ActionSource`/`InputOutcome`、指针事件加 `modifiers`）、`app.rs`（分发 / 焦点 / 滚动 / 虚拟列表 / 浮层 / 拖拽 / 动画 / 时钟）、`window.rs`（`focus`/`pointer`/`pending_damage`）、`node/tree.rs`（`interaction` 槽 + `lane_offset` + 8 个访问器）、`node/builder.rs`（`on_click`/`on_action`/`on_drag`/`focus_ring`）、`node/signal.rs`（`transient_ref`）、`paint/{style,pass}.rs`（`FocusRing`、累计偏移、焦点环）、`layout/hit_test.rs`（累计偏移 + `interactive:false`）、`platform/{event.rs,windows/{event_loop,window}.rs}`（`PointerLeave`/捕获/光标/`SetTimer`/`sync_clock`/类光标 `IDC_ARROW`）、`error.rs`（`NotScrollable`/`NotVirtualList`）、`ui_core_demo/{Cargo.toml,scene.rs}`（`taffy` 依赖 + 动作/焦点环/滚动列表 + 1 项用例）、`Cargo.lock`（**+1 行**：demo → taffy 的边） |
| 测试         | `cargo test -p ui_core` **557**（P09 基线 491 → **+66**）；`--features test-support` **574**（含 `p10_tests` 端到端）+ 集成 5；demo **1**（评审后追加 11 项回归用例，见 §15.6）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| 门禁         | `cargo +nightly clippy -p ui_core -p ui_core_demo --all-targets --all-features -- -D warnings` 通过；`cargo +nightly fmt --all -- --check` 通过；`cargo doc -p ui_core --no-deps` 无 warning；`cargo build -p ui_core --release` 通过                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| 死代码       | `#[allow(` 0 处、`#[expect(` 0 处、`#[path]` 0 处；`unsafe` 只在 `platform/windows/**`（每处 `// SAFETY:`）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| 编号对齐     | S-95 的 6 处注释已改（`tree.rs` ×3、`hit_test.rs` ×1、`paint/style.rs` ×2）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| 偏差/发现    | 见 F-101 / F-102 / F-103 / F-104 / F-105、S-99…S-106（§15.5 / §15.6）；F-100 已被 F-104 取代                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| 已知环境限制 | ①`cargo clippy --workspace --all-features` 会在 `llama_sys`（llama.cpp 构建脚本）失败，与 P10 无关，门禁按 `-p ui_core -p ui_core_demo` 收窄；②`platform::dispatcher` 的两项 P09 用例在**高并行**下偶发失败（`PostMessageW` 计数与并行测试竞争），单跑/重跑稳定通过，属既有 flake（V-73）；③macOS / Linux 交叉 check 未在本轮执行（无对应 toolchain 目标）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                         |

### 15.5 实施期追加条目

| 编号      | 内容                                                                                                                                                                                                                                                                               |
|-----------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-100** | `windows 0.62.2` **没有导出 `WM_MOUSELEAVE`**（`WindowsAndMessaging` 里查无此常量，实测 probe 与源码 grep）。处理：`platform/event.rs` 自行命名 `WM_MOUSELEAVE_MESSAGE: u32 = 0x02A3` 并在 `translate` 里映射到 `PlatformEvent::PointerLeave`（配合 `TrackMouseEvent(TME_LEAVE)`） |
| **F-101** | **`FocusRing` 不进 `Style`**，改由 `NodeBuilder::focus_ring(ring)` 写进 `NodeInteraction.focus_ring`：落地时发现 `Style` 加字段会让全仓 40+ 处 `Style { inner, clip }` 字面量（含 P02/P04/P06 测试）全部失效；且「焦点环」是交互态而非布局/绘制样式（§6.4 已同步）                 |
| **F-102** | `Node` 实际体积 **1144 B**（1128 + `interaction` 8 + `lane_offset` 8），仍在 1152 上限内；`node_size_after_p06` 的严格断言同步改为 1144（并注明是 P10 预算）                                                                                                                       |
| **S-97**  | 滚动偏移的**唯一应用点**落地为两处：`paint/pass.rs::paint_node` 的 `offset` 参数（图元与 clip 矩形按 `-offset` 平移）与 `layout/hit_test.rs::pick` 的 `offset` 参数（屏幕点 → 内容点）；`Node.lane_offset: Point` 是 lane 复用判据的偏移戳（paint 重建后写回）                     |
| **S-98**  | 滚动**不产生任何脏位**：`App::scroll_to` 只写 `ScrollSlot.offset` + `Window::request_damage(viewport)` + `wake_requested`；`paint_frame` 把 `pending_damage` 以 `aa_padding = 1.0` 推进 `DamageAccumulator`（与其它 damage 同口径），因此滚动 damage ⊆ 视口 + 1 px AA 外扩         |
| **V-73**  | `platform::dispatcher` 的 `repeated_wakes_coalesce_into_one_post` / `queued_tasks_are_drained_once` 在高并行测试下偶发失败（P09 既有用例，`PostMessageW` 计数与并行度相关）；单跑与重跑稳定通过，非 P10 引入                                                                       |

### 15.6 评审修复（2026-11-13，第二轮）

| 编号      | 内容                                                                                                                                                                                                                                                                          |
|-----------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-103** | 新增 `EngineError::UnknownWindow { window: WindowId }`：`create_overlay`/`animate` 在窗口已关闭时返回它。原先用 `NodeId::for_test(0, 0)` 当哨兵，而该构造器要求 `generation != 0` ⇒ **panic**（评审 #1）。`NodeId::for_test` 重新收进 `#[cfg(test)]`（生产侧不再有调用者）    |
| **F-104** | `WM_MOUSELEAVE` **确实由 `windows 0.62.2` 导出**，但来自 `Win32::UI::Controls`（不是 `WindowsAndMessaging`）⇒ 撤销 F-100 的本地常量，改用导出常量（`Win32_UI_Controls` feature 本就在列表里）                                                                                 |
| **F-105** | `FocusRing` 保持 `NodeInteraction.focus_ring`（F-101 不变）；评审 #9 的「旧环残留」根因不是字段位置，而是 **damage 没含环的外扩**：`set_focus` 清旧焦点时按 `bounds.outset(offset + width)` 并入 `Window::pending_damage`                                                     |
| **S-99**  | **damage 一律屏幕坐标**：`scroll::to_screen(tree, node, rect)`（减**祖先**滚动偏移）用于 `scroll_to` 的视口 damage、`paint` 的 `node_damage`/`hide_subtree`、`resolve_placement` 的 `Anchor`；`scroll::ancestor_offset` 不含自身（容器自己的 offset 只作用于子树）            |
| **S-100** | 焦点有效性：`App::focused` 校验 `tree.get(node)`，`validate_focus` 在 `dispatch_input`/`focus_next`/`focus_prev` 前清理陈旧焦点（虚拟行回收后 Tab 不再卡死）                                                                                                                  |
| **S-101** | 虚拟列表：`visible_range` 在**视口未知（0）**时挂 `0..(1+overscan)`；`App::frame` 在 `layout_text` 之后调 `rewindow_virtuals`，布局/改尺寸后自动修窗                                                                                                                          |
| **S-102** | 分发显式传 `modifiers`：`dispatch_action_with_hit(.., modifiers)`；键盘路径用**本次按键**的修饰键，不再回落到 `pointer.modifiers`                                                                                                                                             |
| **S-103** | 动画重定向：`animate` 在入槽前 `retain(!same_target)`（`Scalar`/`Color` 比 `SignalId`、`Scroll` 比 handle），避免两条插值同时写同一瞬态值 ⇒ 回弹；零时长动画也置 `wake_requested`                                                                                             |
| **S-104** | 指针细节：拖拽中二次 `PointerDown` 忽略；浮层外部点击把本次 `press` 作废（`Up` 不再落到下层）；`ClickRecord` 带 `WindowId`；`InputThresholds` 由平台用 `GetDoubleClickTime` / `SM_CXDRAG` 初始化并提供 `App::set_input_thresholds`；`close_window` 清该窗口的动画槽与点击记录 |
| **S-105** | 公开面：`PressRecord`/`DragSession`/`ScrollSlot`/`OverlaySlot` 收为 `pub(crate)`（内部状态）；`DragInfo` 增加 `phase`；`AnimationId` 字段改名为 `generation`（单调代际）；`scroll_to` 对已卸载节点返回 `StaleNode`                                                            |
| **S-106** | `lerp_color` 改为 **sRGB 分量**插值（端点精确、中点按 sRGB 编码后线性插值再解码），符合设计 §11.2                                                                                                                                                                             |
| **V-74**  | `platform::dispatcher` 两项 P09 用例在**全量并行**下偶发失败（`PostMessageW` 计数与他测并发），单跑/重跑稳定；本轮全量连跑 3 次中 1 次复现，非 P10 引入                                                                                                                       |

---

## 十六、P11 · 无障碍（AccessKit）（2026-11-13）

> 设计见 `design/phase-11-a11y.md`；probe 在 `%TEMP%\kg-p11-a11y`（仓库外、独立 `CARGO_TARGET_DIR`、`--offline`）。
> 本阶段**只做设计 + API 核验 + 可编译 probe**：未写仓库代码、未改 `Cargo.toml` / `Cargo.lock`、未 commit。§16.4
> 的实施记录待实现期追加。
> **阶段号**：`custom-gui-engine-design.md` §7 与 `phase-01-foundation.md` 附录 A.3 的「P14」= 本阶段（旧路线图编号）；
> `phase-03/05/06/10` 里的「P11」也是本阶段。本文用 **P11**。

### 16.1 设计期条目

| 编号      | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
|-----------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-106** | **`Role::StaticText` 与 `Role::ScrollArea` 都不存在**（主设计 §7.4 的两处事实错误）。`accesskit 0.25.2` 的 `Role`（lib.rs:62）里与文本相关的只有 `TextRun`(65)、`Label`(67)、`Paragraph`(80)、`LineBreak`(160)；滚动容器是 `ScrollView`(182)。修订：文本 → `Label`，滚动容器 → `ScrollView`                                                                                                                                                                                                                                                                   |
| **F-107** | **`Role::TextRun` 不能当普通文本叶子**：`accesskit_consumer 0.39.2` 的 `common_filter_base` 把 `GenericContainer` 与 `TextRun` 都判为 `FilterResult::ExcludeNode`（filters.rs:31-34），`TextRun` 还要求 `character_lengths` / `character_positions` 才成立。且 `Label` 角色的**可访问名来自 `value`**（`label_comes_from_value()` ⇔ `role()==Label`，node.rs:756-763；`accesskit_windows/node.rs:406-409` 的 `name()` 按此分派，`Role::Label → UIA_TextControlTypeId`，node.rs:65）。修订：文本节点写 `Role::Label` + `set_value(text)`，**不写** `set_label` |
| **F-108** | **`GenericContainer` 的子节点在 UIA 树里被上提**（probe ① #3/#4 实测：`root.filtered_children(common_filter)` 直接是 `[text, button, input, canvas, scroll]`，容器本身不出现）。因此「`Box` 有自己的 a11y 节点」是错的预期；容器要在 AT 树里出现必须显式给 `A11yOverride.role = Group`。反过来说，`Box → GenericContainer` 的映射**不需要**引擎做 children 压缩                                                                                                                                                                                               |
| **F-109** | `Canvas` / `Geometry` → **`Role::Image`**（不是 `GraphicsObject`）：ARIA 的 `graphics-object` 必须位于 `graphics-document` 之内，单独使用会让 UIA/NSA 的映射退化；`Image` 在 UIA 是 `UIA_ImageControlTypeId`、在 NSAccessibility 是 `image`，正是「有替代文本的图形」的语义。主设计 §7.4 的「`Image` / `GraphicsObject` 二选一」按此收口                                                                                                                                                                                                                      |
| **F-110** | **焦点可达性**：`common_filter_base` **先判 `is_focused()`**（focused ⇒ `Include`，filters.rs:17-20），`hidden` 只在其后（22-24）⇒ **被聚焦的 hidden 节点反而出现在 AT 树里**。真正会 `panic` 的是「`TreeUpdate.focus` 指向不在 `nodes` 里的节点」（`validate_global`，tree.rs:86-96 / :485）。修订：`hidden` 只表达可见性；焦点回退规则针对**未发出的节点**（虚拟列表回收行），不是 hidden                                                                                                                                                                   |
| **F-111** | **Windows adapter 没有 deactivation 路径**：`accesskit_windows 0.35.2` 全仓无 `deactivate` 字样、`Adapter` 无 `Drop`（只有 `SubclassingAdapter` 的 `Drop` 用于卸子类化，subclass.rs:200-204）；`DeactivationHandler`（accesskit lib.rs:3380-3388）只被 `accesskit_unix` 使用。修订：Windows 的「关闭时 deactivate」= `DestroyWindow` **之后** drop `Adapter`；主设计 §8.1 shutdown 清单里的「a11y deactivate」在 Windows 无对应 API                                                                                                                           |
| **F-112** | **clip 不参与 `hidden` 判定（v1）**：主设计 §7.4 的「隐藏 / 被 clip 裁掉 ⇒ `set_hidden(true)`」只落前半。原因是"被完全裁掉"要算 `bounds ∩ clip_chain`（`hit_test` 级别的遍历），而 a11y pass 的增量语义会被"滚动导致整棵子树可见性翻转"污染。v1 只按 `visible:false` / `Display::None` / `A11yPatch.hidden` / 缺替代文本置 hidden；代价是滚出视口但**仍挂载**的节点仍暴露（虚拟列表未挂载的行本就不存在，不受影响）                                                                                                                                           |
| **F-113** | **`A11yTreeSnapshot` 进 `FrameRecord` 而不是 `RenderedFrame`**（主设计 §7.9 原文是「`RenderedFrame` 的一部分」）：`A11Y` 不产生 damage（§7.5 / P03 §9.3 表第 9 行）⇒ 纯 a11y 变化让 `App::frame` 返回 `FrameOutcome::Idle`，而 `RenderedFrame` 只在已提交帧存在（`window.rs:539-544` 的 `record_idle` 分支 `frame: None`）。放 `RenderedFrame` 会让「focus 每帧必带」「增量只含变更」两条断言在 Idle 帧上无从验证                                                                                                                                             |
| **F-114** | **a11y 槽走侧表，`Node` 保持 1144 B**：`Node` 只剩 8 B 余量（F-102 的 1144 / 上限 1152），内联 `Option<Box<NodeA11y>>` 会正好顶满 1152（P12 无余量）。改为 `NodeTree.a11y_slots: Vec<Option<Box<NodeA11y>>>`（按 arena 槽位），与 `layout_cache`（S-42）、`clip_keys`（S-66）、`text_slots`、`debug_ids` **同型**，不引入新机制；槽位复用与 `remove` 的清槽纪律完全照抄既有四处                                                                                                                                                                               |
| **S-107** | **`bind_a11y` 在 `refresh_styles` 内求值**（与 `bind_style` / `bind_text` 同一 `ReadCtx`、同一 `ReadTracker`、同一份依赖边集）。理由：`DependencyGraph::replace_deps(owner, old, new)` 是**整体替换** `Owner::Node{..}` 的边集（dependency.rs:55-82），独立 pass 会用「只有 bind_a11y 读过的信号」覆盖 `bind_style` 的边集 ⇒ 后者的依赖静默丢失。一条 pass / 一个 tracker / 一份边集是唯一自洽形态                                                                                                                                                            |
| **S-108** | **`A11Y` 是累积位**：`settle_frame` 只清 `LAYOUT                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |PAINT`（tree.rs:950-959），a11y pass 跑在帧之后 ⇒ 位会跨帧累积，由 pass 消费后显式 `clear_a11y(node)`。`NodeTree` 增加 O(1) 计数 `a11y_dirty: u32`（`touch` 里 A11Y 由无到有时 +1，`clear_a11y` 时 -1），平台层用它做「本轮有没有 a11y 工作」的 O(1) 判据 |
| **S-109** | **`mark_layout` 追加 `A11Y`**：bounds 变了 ⇒ a11y bounds 必须重发，而 `LAYOUT` 在帧末被清，pass 看不见。`A11Y` 不产生 damage、不清 `REUSABLE`（`touch` 只在含 `LAYOUT                                                                                                                                                                                                                                                                                                                                                                                         |PAINT` 时累积 damage，tree.rs:1604-1614），所以这条追加不违反「a11y 更新不产生 damage」。实现期需复核两条既有用例的断言形态（`refresh_styles_escalates_layout_when_patch_affects_layout`、`layout_escalation_marks_fixed_size_node_ancestors`），若它们断言了 flags **相等**则改为 `contains` |
| **S-110** | **a11y pass 在帧门禁之外**：`Window::needs_frame`（window.rs:378-386）与 `commit_frame` 的第二段门禁（`has_pixels`，window.rs:532-544）都只与像素有关。pass 由**平台泵每轮**调用一次（`pump` 的第 ④′ 步）+ 测试捕获路径（`App::frame` 开头）调用；平台层先用 O(1) 的 `app.a11y_pending(window)` 判据过滤，避免 adapter 一旦 Active 就每轮构全树判断                                                                                                                                                                                                           |
| **S-111** | **公开面定案**：`ui_core::a11y::{A11yRole, A11yNode, A11yLabel, A11yLabelSource, A11yNodeId, A11yAction, A11yOverride, A11yPatch, A11yTreeSnapshot, A11yPassReport, A11yDiagnostic, A11yActionCtx, A11yActionData}` 定义在 `src/a11y.rs`（自命名模块根，`interaction.rs` 同款）；`A11yBridge` / `A11yTree` / `NodeA11y` 是 `pub(crate)`。**`Selector::Role` 永久不做**（F-63 的「延后 P14」收口）：角色是 a11y pass 的产物、不在 `Node` 上，`Probe` 只有 `&NodeTree`；角色查询走 `A11yTreeSnapshot::node_for(engine_id)`                                      |
| **S-112** | **单一 drainer 规则**：a11y 的 pending 集合同一时刻只有一个消费者——生产构建里是 Windows adapter（`update_if_active` 的闭包），`test-support` 里是捕获路径。`A11yBridge.capture: bool` 只在 `TestApp` 里打开（`TestApp` 从不建原生窗口），生产恒 `false`；捕获路径不碰 `full_pending`。这条规则是 §10.5 用例 ① 的可断言前提                                                                                                                                                                                                                                    |
| **S-113** | **Action 回投形态**：`platform/windows/a11y.rs` 的 `ActionHandler` 只做 `Dispatcher::push(UserTask::new(move                                                                                                                                                                                                                                                                                                                                                                                                                                                  |app| a11y::action::dispatch(app, window, request)))`（dispatcher.rs:19-98）；`ActionRequest` 全字段 `Send`。`Click` 必须落到 P10 的 `Action` 表（`focus::action_target` → `App::dispatch_action(ActionSource::Programmatic)`），**不合成 `PointerDown/Up`**（P10 §6.4 已裁决）；`A11yAction::Click` 因此不进 `on_a11y_action` 的合法键 |

### 16.2 probe 记录（`%TEMP%\kg-p11-a11y`，2026-11-13）

> 环境：Windows，非交互式 agent 会话；`cargo run --offline`（registry 缓存命中）；
> `CARGO_TARGET_DIR=%TEMP%\kg-p11-a11y-target`。

| #  | 实测项                                                                                                                             | 结果                                                                                                   |
|----|------------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------|
| 1  | `cpu_tree`：真实 `accesskit 0.25.2` 建 11 节点树 → `accesskit_consumer 0.39.2` 的 `Tree::new`                                      | ✅ 全树接受，`root`/`focus`/`toolkit_name=="Kytrix"` 就位                                              |
| 2  | 未过滤 children vs `filtered_children(common_filter)`                                                                              | ✅ `[container, scroll, hidden]` vs `[text, button, input, canvas, scroll]`（容器上提、hidden 消失）   |
| 3  | `Label` 节点的 `label()` / `label_comes_from_value()` / `value()`                                                                  | ✅ `None` / `true` / `Some("价格")`                                                                    |
| 4  | `bounds` 换算（逻辑 8/32/60/24 × 1.25，逐边 round）                                                                                | ✅ `Rect::new(10, 40, 85, 70)`                                                                         |
| 5  | 只改 label 的增量                                                                                                                  | ✅ `nodes.len()==1`、`updated==[button]`、`added==[]`                                                  |
| 6  | 新增子节点**同批发父**                                                                                                             | ✅ `added` 含子、父 children 变 3                                                                      |
| 7  | **只发子不发父**                                                                                                                   | ✅ 消费端 `panic!("…neither in the current tree nor a child of another node…")`（`catch_unwind` 捕获） |
| 8  | 删除（只发父 + children 缩短，被删节点不出现）                                                                                     | ✅ `removed` 含被删 id、父 children 回到 2                                                             |
| 9  | 只改焦点（`nodes: []`）                                                                                                            | ✅ 合法，`focus_moved == [(root, button)]`                                                             |
| 10 | 焦点指向不存在的 id                                                                                                                | ✅ `panic!("Focused ID … is not in the node list")`                                                    |
| 11 | hidden 节点：未聚焦 / 被聚焦                                                                                                       | ✅ 前者不在 filtered children；后者**反而 Include**                                                    |
| 12 | 陈旧 `NodeId`（同槽位 generation+1）                                                                                               | ✅ `node_by_tree_local_id(..).is_none()`                                                               |
| 13 | `ActionRequest{SetValue, Value("42")}` 经 `impl ActionHandler`                                                                     | ✅ 收到且字段正确                                                                                      |
| 14 | 27 个 `Role` 变体逐条 `Node::new(role).role()==role`                                                                               | ✅                                                                                                     |
| 15 | `win_adapter`：隐藏窗口（`IsWindowVisible==false`）下 `Adapter::new(hwnd, false, handler)`                                         | ✅ 构造成功（首显前构造可行；`SubclassingAdapter` 在此处会 panic）                                     |
| 16 | Inactive：`update_if_active(                                                                                                       |                                                                                                        | calls+=1)`（显示前 + 显示后，无 AT） | ✅ 均返回 `None`，闭包执行 **0** 次 |
| 17 | `handle_wm_getobject(WPARAM(0), LPARAM(OBJID_CLIENT=-25), &mut activation)`                                                        | ✅ 返回 `Some(..)`（占位激活）                                                                         |
| 18 | 占位激活后 `update_if_active`                                                                                                      | ✅ 返回 `Some(QueuedEvents)`，闭包执行 **1** 次（首棵全树）                                            |
| 19 | 非 client/root 的 objid                                                                                                            | ✅ 返回 `None`（交 `DefWindowProcW`）                                                                  |
| 20 | `QueuedEvents::raise` 可作为 `fn(QueuedEvents)` 取到                                                                               | ✅（probe 刻意不调用：无 AT 时不向 UIA 广播）                                                          |
| 21 | `wm_getobject_pattern`：`&mut` 借用作用域只覆盖 `handle_wm_getobject`，`Option<impl Into<LRESULT>> → LRESULT` 在作用域**之外**转换 | ✅ 编译通过（`use<H>` 不把 activation handler 的生命周期绑到返回值；与 subclass.rs:50-60 同构）        |

### 16.3 未验证项（不阻塞）

| 编号 | 未验证                                                                                   | 后续                                                                          |
|------|------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------|
| V-75 | UIA 真实联动（角色 / 名称 / 动作 / 焦点事件在 NVDA / Narrator 下的表现）                 | L5 实机矩阵；本文只保证 `TreeUpdate` 的数据正确（probe ① 用真正的消费端校验） |
| V-76 | macOS `accesskit_macos 0.27.2` / Linux `accesskit_unix 0.24.1` 全链路                    | P12；接口与三平台差异见 `phase-11-a11y.md` §12                                |
| V-77 | `QueuedEvents::raise()` 在真实 AT 下执行（probe ② 刻意 drop 掉，避免向 UIA 广播）        | L5；线程约束已按 adapter.rs:633-642 落实                                      |
| V-78 | 大树（> 5k 节点）首次全树的成本未基准                                                    | L4；超标则分片交付（`TreeUpdate` 可多次调用），不改 `A11yTreeSnapshot` 契约   |
| V-79 | `A11yPatch.selected` / `expanded` 的引擎侧写点很少（多数由应用给）                       | 字段有非 test 消费者（映射），符合 AGENTS §2.1；P13 控件库若不需要则删除      |
| V-80 | `pos_in_set` / `set_size_of_set` 未填（`VirtualListState` 已有 `first/last/count` 数据） | 需要时用 `Node::set_pos_in_set` 补，不改快照契约                              |

### 16.4 实施记录（2026-11-13，P11 落地）与实施期条目

| 项       | 实测                                                                                                                                                                                                   |
|----------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 新增     | `src/a11y.rs` + `a11y/{tree,mapping,adapter,action}.rs`；`platform/windows/a11y.rs`                                                                                                                    |
| 修改     | `lib.rs`、`app.rs`、`window.rs`、`node/tree.rs`、`node/builder.rs`、`error.rs`、`platform/windows/{window,event_loop}.rs`、`test_support/{app,record}.rs`                                              |
| 测试     | `cargo test -p ui_core` **585**（P10 基线 557 → +28）；`--features test-support` **613** + 集成 5；demo 1                                                                                              |
| 门禁     | clippy 两包 `--all-targets --all-features -D warnings` 通过；`fmt --check` 通过；`doc --no-deps` 无 warning；`--release` 通过                                                                          |
| 死代码   | `#[allow(` 0 / `#[expect(` 0 / `#[path` 0 / `pub use` 0；`a11y*` 的 `unsafe` **0** 处；`platform/windows/a11y.rs` 生产部分只有 1 处 `unsafe`（`IsWindow`，带 `SAFETY`）                                |
| 尺寸     | `size_of::<Node>()` = **1144 B**（侧表生效，F-114 无需追加字段）                                                                                                                                       |
| 文件边界 | `platform/` 下只有 `windows/a11y.rs`（macOS / Linux 归 P12）                                                                                                                                           |
| P03 断言 | `mark_layout` 追加 `A11Y` 后，`refresh_styles_escalates_layout_when_patch_affects_layout` 与 `layout_escalation_marks_fixed_size_node_ancestors` **无需改**（断言用的是 `contains`），全量用例原样通过 |

实施期新增/修订条目：

| 编号      | 内容                                                                                                                                                                                                                                                                                                                                                                             |
|-----------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-115** | **虚拟列表容器的角色必须先于滚动容器判定**：`make_virtual_list` 同时装 `scroll` 与 `virtual_list` 槽，若先判 `scroll_slot` 则永远得到 `ScrollView`（实施期实测失败）。修订：`role_by_kind` 先判 `virtual_slot`（⇒ `List`），并把 `ScrollUp/Down/Left/Right`/`ScrollIntoView`/`SetScrollOffset` 改为 `effective_actions` 的「**有滚动槽就补**」规则（否则 `List` 拿不到滚动动作） |
| **F-116** | `accesskit 0.25.2` 的 `Selected` / `Expanded` 是 `bool_property_methods!` 生成的**三态属性**：`set_x(bool)` / `is_x() -> Option<bool>` / `clear_x()`（不是 `flag_methods!` 的裸 bool）。映射按 `if let Some(v) = record.selected { node.set_selected(v) }` 写，「不表态」= 不调用                                                                                                |
| **F-117** | `Action::Blur` 与 `Action::ReplaceSelectedText` **没有** `A11yAction` 变体（`A11yAction` 只表达「对目标做什么」）：`Blur` 在 `dispatch` 开头先行处理（焦点正好在该节点才清），`ReplaceSelectedText` 归一成 `SetValue`（无选区模型时等价）。`CustomAction` 的 id 来自 `ActionData`，同样在 `dispatch` 里单独解                                                                    |
| **S-114** | **`A11yPassReport` 不落地**：没有非 test 消费者。计数直接读 `A11yTreeSnapshot` 的 `full` / `nodes.len()` / `diagnostics`；`FrameStats` 不追加字段（`FrameRecord.a11y` 已经是断言面）                                                                                                                                                                                             |
| **S-115** | **a11y 交付点在帧之后**：`TestApp::drain` 在 `App::frame` 返回后调 `App::a11y_take` + `App::attach_a11y` 写进 `FrameRecord.a11y`，与平台泵的 `update_a11y` 同序。因此**纯 A11Y 变化的 Idle 帧同样带快照**（`RenderedFrame` 在空闲帧不存在，F-113 的落地形态）。`A11yBridge.capture` / `Window::set_a11y_capture` 只在 `test-support` 构建里存在（生产零字段、零分支）            |
| **S-116** | `mark_layout` / `mark_subtree_paint` 追加 `A11Y` 后，P03 的两条 flags 用例**无需改**（`contains` 断言），全量 585/613 用例原样通过——S-109 的语义变更没有引入回归面                                                                                                                                                                                                               |
| **V-81**  | `A11yBridge::take` 的增量路径是 O(n) 扫描 `tree.order()` 定位脏节点（已收集的 id 用 `SmallVec::contains` 去重）。5k 节点以上未基准；若进 L4 超标，加「脏位链表」而不改快照契约                                                                                                                                                                                                   |

### 16.5 审查修复轮（2026-11-13，PASS-WITH-FIXES 的 6 条阻塞 + 顺带项）

| 编号      | 修复                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
|-----------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **R-1**   | **布局后子树 bounds 不重发**：新增 `NodeTree::mark_moved_a11y()`（比较 `layout != prev_layout` 并标 `A11Y`），由 `Window::compute_layout` 在布局成功后调用；`set_size` / `set_scale` 改走 `invalidate_layout`（与 `set_style` 同一条路径）。用例：`p11_fix_tests::a11y_bounds_follow_window_resize`                                                                                                                                                                                                      |
| **R-2**   | **`invalidate_layout` 未标 A11Y**：该函数是 `set_style` / `invalidate_measure` / `set_layout_override` / `set_virtual_row` / `set_absolute_inset` / `set_size` / `set_scale` 的公共尾段，统一追加 `A11Y`。用例：`a11y_bounds_follow_layout_override`                                                                                                                                                                                                                                                     |
| **R-3**   | **普通滚动不失效 a11y 子树**：`a11y_note_scroll` 从 a11y action 路径移到 `App::scroll_to` 的成功路径（`scroll_by` / 滚轮 / 动画滚动 / a11y action 全汇到这里，是 `slot.offset` 的唯一写点），`a11y/action.rs` 不再重复调用。用例：`a11y_bounds_follow_scroll`                                                                                                                                                                                                                                            |
| **R-4**   | **`focus_node()` panic**：`A11yTreeSnapshot` 新增 `focus_node: A11yNode` 字段（独立于 `nodes`，用临时诊断缓冲构造，避免重复诊断），`focus_node()` 直接返回它、不再查 `nodes`。用例：`adapter::focus_node_is_carried_even_when_it_is_not_in_the_batch`                                                                                                                                                                                                                                                    |
| **R-5**   | **`WM_GETOBJECT` 重入 UB**：`WindowsA11y` 拆出 `begin_getobject` / `end_getobject`，`wm_getobject` 返回**未转换**的 `impl Into<LRESULT>`，`wndproc` 在短借用块里取值、**出块后**才 `.into()`；`focus_changed` / `update` 改为返回 `Option<QueuedEvents>`，`raise()` 同样在释放 `&mut WndProcState` 之后。旗标覆盖整个「取值→转换→清除」窗口                                                                                                                                                              |
| **R-6**   | **`bind_a11y` 写入仍重绘**：新增 `NodeTree::{has_visual_bind, has_a11y_bind}` 与 `A11yBridge.bind_work`；`App::invalidate` 对「只有 a11y 绑定」的节点只标 `A11Y` + 置 bind_work（不标 PAINT/LAYOUT）；`Window::needs_frame` 感知 bind_work（帧末门禁仍退回 `Idle`）；`refresh_styles` 的 `needs_eval` 追加 `A11Y && has_a11y_bind`；`paint_frame` 在「无 damage 且无节点需重建」时早退（lane 保持 `REUSABLE`、damage 保持空）。用例：`a11y_bind_a11y_write_is_idle_without_damage`（替换原先的假绿断言） |
| **F-118** | 诊断补强：`MissingAltText` 增加 `kind: A11yDrawKind`（`Canvas`/`Geometry`）；`UnhandledAction.action` 改为 `Option<A11yAction>`（`None` = 该 `accesskit::Action` 本阶段没有映射），不再伪造 `Custom(-1)`。顺带：声明了 `on_click`/`on_action` 的节点在 `effective_actions` 里补 `Click`（与角色无关，虚拟行也覆盖）                                                                                                                                                                                      |
| **S-117** | **焦点陈旧时回退到窗口根**，不是「最近的已发出祖先」：陈旧句柄读不到 `parent`（节点已出 arena），设计稿 §6.3 的措辞按实现收口；`MissingFocusTarget` 诊断照记                                                                                                                                                                                                                                                                                                                                             |
| **S-118** | `A11yTreeSnapshot::visible_nodes()` 按「自身 + **本快照内**的祖先链」排除 `hidden`（增量笔的祖先可能不在本笔里，那时按可见处理；平台侧的 `set_hidden` 继承语义才是权威）                                                                                                                                                                                                                                                                                                                                 |
| 门禁      | test **589**（+32 vs P10）/ test-support **621**（+47）+ 集成 **6**；clippy 两包 `-D warnings` / fmt --check / doc 0 warning / release 全绿                                                                                                                                                                                                                                                                                                                                                              |

---

## 十七、P12 · macOS（Metal）+ Linux（Vulkan / Wayland）（2026-11-13）

> 完整设计见 `design/phase-12-macos-linux.md`。本节只记**与上游文档不同的事实**与实现纪律。

### 17.1 交叉编译与依赖事实（本轮实测）

| 项                                                                                     | 实测                                                                                                                                                                 |
|----------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `cargo check -p ui_core --target aarch64-apple-darwin`（`ui_core_SKIP_SHADERS=1`）     | **exit 0**（60 warnings，全是非 Windows target 的 `dead_code`；P12 落地后由 `platform/macos` 重新消费）                                                              |
| `cargo check … --target x86_64-unknown-linux-gnu` **无** pkg-config                    | **exit 1**：`yeslogic-fontconfig-sys 6.0.1` 的 build script（`pkg-config has not been configured to support cross-compilation`）；与引擎代码无关（P07 起既有，V-04） |
| 同上 + 12 行 stub `pkg-config`（`PKG_CONFIG=<stub>`，可选 `PKG_CONFIG_ALLOW_CROSS=1`） | **exit 0**（同样 60 warnings）。stub 只需回答 `--libs [--cflags]`（输出 `-lfontconfig`）与 `--modversion`（输出 `2.14.0`）                                           |
| `RUST_FONTCONFIG_DLOPEN=1` 作为绕过                                                    | **不可行**：dlopen 形态下 `fontconfig_sys` 不在根导出 `fontique 0.12` 需要的 29 个 `Fc*` 符号（`E0432`）⇒ F-125                                                      |
| probe crate（`%TEMP%\kg-p12-macos-linux`，依赖逐字复制 P01）                           | `aarch64-apple-darwin` **exit 0**、`x86_64-unknown-linux-gnu` **exit 0**；本文 §5–§8 的每个原生调用都已编译验证（附录 B 是签名速查表）                               |

### 17.2 设计期条目

| 编号      | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
|-----------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-119** | **macOS 不做 argument buffer**：P07 已落地的 `contract.md` §7 绑定表是 `buffer(0/1/2)` + `texture(0/1)` + `sampler(0)` 直绑；改成 argument buffer 要改 14 个 shader 源文件 + `contract.md` + `build.rs` 锚点，且 D3D12/Vulkan 无对应物。`argumentBuffersSupport == Tier2` 仍作为 D8 硬基线（主设计 §2.2），未来 ≥3 页图集时按 `texture_index` 拆 draw（不是塞 argument buffer 数组）                                                                                   |
| **F-120** | **Metal v1 同步提交**：`commit` + `waitUntilCompleted` 即 fence；**不用** `MTLSharedEvent`——`encodeWaitForEvent:value:` 需要 `&ProtocolObject<dyn MTLEvent>` 上转，而 `objc2 0.6.5` 的 `ProtocolObject` 只有 `from_ref/from_id/from_retained`（probe `E0308`）。P13 异步化时用 `msg_send!` 或升级 objc2                                                                                                                                                                |
| **F-121** | **`FramePlan` 的 `copy_plan()` / `present_dirty()` 是 D3D12 专用**：Metal 的 drawable 与 Vulkan 的 swapchain image **present 后内容未定义** ⇒ 两个新后端每帧整面 blit / `cmd_copy_image`；`plan()`（scissor）与 `is_full()`（clear）仍是共用语义。`render/{macos,linux}/compositor.rs` 文件头必须写明                                                                                                                                                                  |
| **F-122** | Metal 加载 metallib 用 `MTLDevice::newLibraryWithData:error:`，需要 objc2-metal 的 **`dispatch2` feature**（其 default features 已启用）与 **`dispatch2` 直接依赖**（`dispatch2 0.3.1` 已在 `Cargo.lock`，由 objc2 生态引入 ⇒ **零 lock 变化**）。`newLibraryWithFile` 已 deprecated；`newLibraryWithSource` 无法处理 `#include "common.metal"`                                                                                                                        |
| **F-123** | Vulkan v1 **不使用** `runtimeDescriptorArray`：GLSL 的 `instances[]` 是 std430 结构体数组（`binding 0` 的普通 storage buffer），不是 descriptor array。`descriptorIndexing` 仍按 D-4 作为硬基线；`runtime_descriptor_array` 只记录，不支持时降级 `false` 不失败                                                                                                                                                                                                        |
| **F-124** | `accesskit_unix 0.24.1` 与 P11 §12 的措辞不同：①`request_initial_tree() → None` **合法**（`context.rs:91-109` 进 `Pending`，下一次 `update_if_active` 必须是全树）；②`update_if_active` **无返回值**（无需 `raise()`）；③`DeactivationHandler` 由**库自己**在 deactivate / `RemoveAdapter` 时调用，引擎只实现空 handler。**不需要**新增 `full_update` 辅助（`A11yBridge` 的第一次 `take` 必为全树）                                                                    |
| **F-125** | `RUST_FONTCONFIG_DLOPEN=1` **不能**作为 Linux 交叉 check 的绕过（见 17.1）；可行解法只有 stub `pkg-config` 或真 sysroot                                                                                                                                                                                                                                                                                                                                                |
| **F-126** | Vulkan 用**核心 `VkRenderPass` + `VkFramebuffer`**（persist 一个附件）而非 dynamic rendering：`VK_KHR_dynamic_rendering` 在 1.2 是扩展，不是基线。swapchain image 不建 framebuffer（只作 `cmd_copy_image` 目标）                                                                                                                                                                                                                                                       |
| **F-127** | macOS 自绘标题栏：`styleMask = Titled                                                                                                                                                                                                                                                                                                                                                                                                                                  |Closable|Miniaturizable|Resizable|FullSizeContentView`，`titlebarAppearsTransparent` + `titleVisibility = Hidden`；交通灯**只保留 Close**（重排到 x=12），Miniaturize/Zoom 隐藏 ⇒ P13 必须补引擎绘制的最小化/最大化控件 |
| **F-128** | `zwp_text_input_v3` 的 `preedit_string` / `commit_string` / `delete_surrounding_text` 必须**缓存到 `done` 再原子生效**（可能同批到达）；`commit` 后要吞掉同帧紧随的字符键（GNOME 的部分版本会重复发）                                                                                                                                                                                                                                                                  |
| **F-129** | `wl_data_device.set_selection` **必须**带最近的输入事件 serial（`wl_pointer::Enter` / `wl_keyboard::Enter` 的 serial）；无 serial 时 compositor 静默忽略                                                                                                                                                                                                                                                                                                               |
| **F-130** | Linux 剪贴板是**异步拉模型**：`App::clipboard_text()` 只能返回事件循环缓存的最近一次选区内容，从未收到时返回 `Err(Clipboard)`；与 Win32/macOS 的同步语义不同，必须在公开 API 文档写明                                                                                                                                                                                                                                                                                  |
| **F-131** | Metal 的 `Section::Gpu` 用 `MTLCommandBuffer::GPUStartTime/GPUEndTime`（command buffer 级），**不用** `MTLCounterSampleBuffer`（`counterSets` 可能为空、需拆 pass、CI 无法验证）                                                                                                                                                                                                                                                                                       |
| **F-132** | P01 的 objc2 feature 列表是**文档性冗余**：`objc2-app-kit` / `objc2-metal` 的 `default` 已包含 `NSButton` / `NSCursor` / `dispatch2` 等全部类与协议（`cargo tree -e features` 验证）⇒ P12 **不需要**新增任何 feature                                                                                                                                                                                                                                                   |
| **S-119** | `render/device.rs` 拆分：`GpuCaps` / `fl_name` / `sm_name` / `FEATURE_LEVEL_12_0` / `SHADER_MODEL_6_2` 与 5 个单测搬入 `render/windows/device.rs`；共享文件只留 `D8_REQUIRED_{WINDOWS,MACOS,LINUX}`                                                                                                                                                                                                                                                                    |
| **S-120** | `platform/event.rs`（`PlatformEvent` / `translate` / `key_from_vk`）**保持 `#[cfg(windows)]` 不重构**：mac/Linux 的平台层直接把原生事件翻成公共 `Event` 后调 `App::dispatch`（与 Win32 的 `to_app_event` 同层职责）                                                                                                                                                                                                                                                    |
| **S-121** | `ReadbackImage` 三个后端各一份（`render/{windows,macos,linux}/readback.rs`），**不**提到共享层：它只在 `test`/`test-support` 构建存在，且每个后端都要和自己的 `Renderer::readback` 同形                                                                                                                                                                                                                                                                                |
| **S-122** | `Application::run(self, app: App)` 三平台同签名：macOS **不子类化** `NSApplication`（帧泵挂 `CFRunLoopSource`）；Linux 用 `calloop` + `WaylandSource`。两者都不引 libdispatch / 第二套事件循环                                                                                                                                                                                                                                                                         |
| **S-123** | P08 的 S-78（共享层只在 Windows 编译）**收口**：`render.rs` / `platform.rs` / `window.rs` / `app.rs` 里为「只有一个后端」而加的 `#[cfg(windows)]` 全部删除；`app.rs` 的 `Application` / clipboard / `attach_a11y` / `upload_glyph_page` 一并解除                                                                                                                                                                                                                       |
| **S-124** | ~~mac/Linux 的 `TestApp::snapshot` 仍返回 `SnapshotError::NoRenderer`~~ **实现期更正（见 F-137）**：`TestApp::snapshot` 的非 Windows 分支**接真渲染器**（`new_offscreen_with_page` → `queue_atlas_upload` → `render_snapshot`）。macOS 的离屏 Metal 渲染器真实可用（WARP 的对应物是 Apple GPU 的 offscreen），Linux 的 `new_offscreen*` 因 Vulkan WSI 需要真实 `wl_surface` 而显式返回 `UnsupportedGpu` ⇒ Linux 上快照恒为 `SnapshotError::Gpu`（不再是 `NoRenderer`） |
| **V-82**  | **L5 实机矩阵**：GNOME 45+/KDE Plasma 6/Sway × 单/双 GPU（含 hybrid）× 单/多显示器 × 100%/125%/150%/175% × 中/日文 IME。P12 只保证「两 target 编译 + 结构正确」，出帧与像素归 L5。**2026-11-13 部分关闭**：GNOME（mutter）+ Vulkan/lavapipe 的实机验证已完成（见 §25 与 `linux-verification.md`）；KDE / Sway 作为产品目标、多显示器混合 DPI、真实中日文输入法仍未验证（V-110 / V-111 / V-113） |
| **V-83**  | MSL / SPIR-V **未在本机编译**：CI 必须加 macOS runner（`xcrun metal`）与 `glslang-tools`；`build.rs` 的 skip 分支只在缺工具链时用                                                                                                                                                                                                                                                                                                                                      |
| **V-84**  | macOS dead key / IME 行为未验证（`NSEvent.characters` 是否已合成、`insertText:` 的 `replacementRange` 语义）；L5 用中日德文语料回放                                                                                                                                                                                                                                                                                                                                    |
| **V-85**  | macOS 滚轮「行」换算（`scrollingDeltaY / 3.0`）未校准：L5 与 Win32 的 120 单位/行对齐                                                                                                                                                                                                                                                                                                                                                                                  |
| **V-86**  | 无 `zxdg_decoration_manager_v1` 时 compositor 可能自己加服务端装饰（与自绘标题栏重叠）；L5 在三大 compositor 上确认                                                                                                                                                                                                                                                                                                                                                    |
| **V-87**  | Linux 光标形状**未设置**（无 `wayland-cursor` 依赖）：`take_cursor_change()` 在 Linux 空转；是否需要新依赖由 P13 裁决                                                                                                                                                                                                                                                                                                                                                  |
| **V-88**  | 多 seat / 多键盘未支持（v1 只接第一个 `wl_seat`）；`zwp_text_input_v3` 的 seat 绑定同理                                                                                                                                                                                                                                                                                                                                                                                |
| **V-89**  | xkb 修饰键位图的取法二选一（`serialize_mods` 位图 vs `xkb_state_mod_name_is_active`）未在本机验证；实现期以 probe 编译通过的写法为准，行为归 V-82                                                                                                                                                                                                                                                                                                                      |
| **V-90**  | Metal 的 Xcode GPU Frame Capture（`MTLCaptureManager`）与 Vulkan 的 `VK_EXT_debug_utils` 未接：v1 只靠 `EngineError` + 文件诊断                                                                                                                                                                                                                                                                                                                                        |
| **V-91**  | 「每帧整面 copy/blit」的带宽成本未基准（4K/120Hz 下 persist→swapchain 约 1.99 GB/s）；L4 量化后再决定是否需要「多 swapchain image 各自缓存一份 persist」                                                                                                                                                                                                                                                                                                               |

### 17.3 门禁（实现期）

| 项                           | 期望                                                                                                                                                                                                                                    |
|------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 默认测试                     | `cargo test -p ui_core` **589**（逐项不变）；`--features test-support` 621 + 集成 6；demo 1                                                                                                                                             |
| lint / 格式 / 文档 / release | `clippy --all-targets [--all-features] -D warnings` / `fmt --check` / `cargo doc --no-deps` / `--release build` 全绿                                                                                                                    |
| 交叉 target                  | `ui_core_SKIP_SHADERS=1 cargo check -p ui_core --target {aarch64-apple-darwin, x86_64-unknown-linux-gnu}` **exit 0**（Linux 需 stub `pkg-config`，§17.1）；两 target 再加 `--features test-support` 各 **exit 0**（覆盖 `readback.rs`） |
| 依赖                         | `cargo tree` 无 `x11*` / `xcb*` / `winit` / `wgpu` / `naga`；**`wayland-sys` 必须存在**（F-135，`system` feature）；`Cargo.lock` 仅 +2 行（`dispatch2` 依赖边与其 `libc`）；`Cargo.toml` 仅 `dispatch2` / `system` 两处                 |
| 代码形态                     | `render.rs` / `platform.rs` / 四个后端模块根只有 `mod`/`use`；无 `#[allow]` / `#[expect]`；`unsafe` 只在 `render/{windows,macos,linux}/**` 与 `platform/{windows,macos,linux}/**`，每块带 `// SAFETY:`                                  |

### 17.4 实现期条目（2026-11-13，落地时新增）

| 编号      | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
|-----------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-133** | macOS 事件循环**自持**：`Application::run` 用 `NSApplication::nextEventMatchingMask:untilDate:`（drain + `distantFuture` 阻塞）+ `CFRunLoopSource` 唤醒源 + 60 Hz 动画 `CFRunLoopTimer`（有动画才挂、空闲移除），**不**调 `NSApplication::run()`，也**不**用 `NSWindowDelegate` / 通知中心：尺寸 / 缩放 / 焦点 / 关闭全部靠每轮 O(1) 快照对比。理由：`NSApp.run()` 拿走控制权后帧泵只能塞进 `performSelectorOnMainThread`（多一次 run-loop 跳转且退出路径不可控）。代价：`App` 以 `Box::into_raw` 固定地址穿透两个 C 回调（`pump_callback` / `timer_callback`），模块头写了线程/别名论证；退出路径由 `CFRunLoop::current().stop()` 打断阻塞 |
| **F-134** | mac/Linux **不建 geometry pipeline**（共享层 `FrameOp::Geometry` → `EngineError::GeometryRegister`）：P12 没有几何注册入口（P08 同），提前建管线只会引入一个永远不 bind 的对象；P13 接 `GeometryStore` 时与 Windows 一起补齐（MSL/SPIR-V 产物已在 `OUT_DIR/shaders`）                                                                                                                                                                                                                                                                                                                                                                       |
| **F-135** | `wayland-client` 必须 `features = ["system"]`：`VkWaylandSurfaceCreateInfoKHR` 要 `wl_display*` / `wl_surface*` 的**真实对象**，纯 Rust 后端的 `ObjectId` 只是令牌，交给驱动即崩。因此 `wayland-sys` + `libwayland-client` 进依赖树（设计期「纯 Rust、零系统依赖」的说法作废），Linux 构建机必须有 `wayland-client` 开发包；`wayland-sys` 的 build script 走 `pkg-config`，交叉编译仍需 §17.1 的 stub                                                                                                                                                                                                                                       |
| **F-136** | `Renderer::resize` 在 mac/Linux 只收 `(u32, u32)`：`contentsScale` / `drawableSize` 由平台层持有图层后自行更新（mac），Vulkan 的 extent 由 swapchain 自己记。`Window::submit_gpu` 因此三平台同签名，`renderer.surface()` 只报物理尺寸                                                                                                                                                                                                                                                                                                                                                                                                       |
| **F-137** | Linux 的两处「主循环独占」适配：① 剪贴板 `set_text` / `get_text` 走进程级 mailbox + 缓存（`set_selection` 需要最近输入 serial，只有循环持有，且 Wayland 是拉模型）；② `Renderer::new_offscreen` 明确返回 `UnsupportedGpu`（Vulkan WSI 需要真实 `wl_surface`，`App` 构造期没有连接），`TestApp::snapshot` 在 Linux 上因此恒为 `SnapshotError::Gpu`；macOS 的离屏渲染器（`new_offscreen[_with_page]`）真实可用，`Readback` 不再无消费者                                                                                                                                                                                                       |
| **S-125** | 共享 `render/device.rs` **删除**：`GpuCaps` / `D8_REQUIRED` / `FEATURE_LEVEL_12_0` 全部下沉到各后端（三个后端的基线本来就不同：`Base\|\|FeatureLevel12`、`ArgumentBuffersTier2`、`Vulkan1.2+descriptorIndexing`）。`render.rs` 不再声明 `device`，`caps_tests` 跟着搬进 `render/windows/device.rs`                                                                                                                                                                                                                                                                                                                                          |
| **S-126** | `platform/dispatcher.rs` 的 `post_frame_wake` 变成三段 `#[cfg]`（`PostMessageW` / `CFRunLoopSourceSignal`+`WakeUp` / `calloop::ping`），`attach` 的句柄在 mac/Linux 上只是占位 token；`Dispatcher::attach(1)` 由两个新循环调用（保持「先置位再唤醒」的唯一性，I3）                                                                                                                                                                                                                                                                                                                                                                          |
| **S-127** | 两个新 `Renderer` 都在**同步提交**下工作（mac `waitUntilCompleted`、Vulkan `wait_for_fences`+timeline），因此 `readback` / `render_snapshot` 之前不需要额外同步；`resize` / 退出前统一 `wait_idle`（Vulkan 用 timeline 值，mac 是 no-op）                                                                                                                                                                                                                                                                                                                                                                                                   |
| **V-92**  | macOS 的输入翻译是 v1 近似：`keyDown:` 同时发 `Key` 与 `Text`，IME 组合期间靠 `hasMarkedText()` + 上次 commit 去重；`scrollWheel:` 的 `precise` 与非 `precise` 分别 /10 /3 归一到「行」；光标形状只做系统箭头（`NSCursor::arrowCursor`）。L5 需要按 §7.8 的表逐项校准                                                                                                                                                                                                                                                                                                                                                                       |
| **V-93**  | Linux 输入同理：`wl_pointer` 的 `axis.value`（1/120 行）取反后传给 `delta_y`；没有 `wp_pointer_constraints` / `wp_relative_pointer`（拖拽出窗口依赖 compositor 的隐式抓取）；`set_cursor` 未接（Wayland 需要 `wl_cursor_shape` 或自绘，属 P13 窗口装饰的一部分）                                                                                                                                                                                                                                                                                                                                                                            |
| **V-94**  | `InFlightRing::reserve` 至今无生产消费者（只在共享层测试里）；P12 没有动它，`test-support` 交叉 target 上会报 1 项 `never used`（既有问题，不在 P12 范围）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |

## 十八、P13 · 资源 + 像素 golden + bench + 诊断（2026-11-13）

> 完整设计见 `design/phase-13-resources-snapshot.md`。本阶段**同时**收口上游排期的 **P15（资源）** 与 **P18（像素快照 /
> bench）**，
> 外加主设计 §9.1 的四个诊断导出。本节只记**与上游文档不同的事实**与实现纪律；一～十七 未改动。
>
> **编号说明**：F-127 / V-87 / V-93 里的「P13」是旧排期的**窗口装饰 / 光标形状**阶段，与本阶段无关（本阶段不实现窗口装饰）。
>
> **事实基线**：`image 0.25.10` 与 `criterion 0.8.2` 的真实源码 + 两个离线 probe
> （`%TEMP%\kg-p13-probe`、`%TEMP%\kg-p13-bench-probe`）；已落地代码 `paint/atlas.rs`、`render/atlas.rs`、
> `text/atlas.rs`、`test_support/{snapshot,app,record}.rs`、`app.rs`、`error.rs`（逐条行号见 phase-13 §2.1）。
> 基线门禁实测：`cargo test -p ui_core --lib -- --list` = **590**，`--features test-support` = **622**，集成
> `frame_record` = **6**。

### 18.1 设计期条目（F-138 … F-148、S-128 … S-131、V-95 … V-101）

| 编号      | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
|-----------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-138** | **`EngineError::ResourceDecode` 的 payload 改为强类型**：P01 §6.2.2 #16 冻结的 `{ resource: ResourceId, detail: SmolStr }` → `{ cause: DecodeCause }`，`DecodeCause = UnsupportedFormat { format } \| Corrupt { format, detail } \| TooLarge { width, height, limit } \| Capacity { width, height, page }`。理由：①`detail` 无法 match，AGENTS §3 要求调用方按类型决定「占位图 / 提示 / 卸载重试」；②解码发生在 `ResourceId` 铸出**之前**，冻结表里的 `resource` 只能填陈旧句柄。P01 历史正文不改，本条为 payload 修订记录                                                                                                                                             |
| **F-139** | **远程图片磁盘缓存 v1 不做**（主设计 §8.5 的「含远程图片的磁盘缓存」降级为应用层）：`ResourceCache` 只收 `&[u8]`。理由：v1 无消费者（远程头像属 M6 的 `base_ui`/`download_provider`）；引擎主线程禁阻塞 IO（`clippy.toml` 禁 `spawn_blocking`）；缓存键（URL/ETag/TTL/尺寸）未定义，仓促定会造出第二套真值。后续形态是应用层 `base_ui::assets::disk` 命中后仍调 `ResourceCache::insert` ⇒ **纯增量、无重构**                                                                                                                                                                                                                                                           |
| **F-140** | **图片图集的容量逃逸是强类型错误，不是 `clear + retry`**：P02 §10.6 的「`clear()` + 重试 → `AtlasExhausted`」只服务字形路径。图片 tile 只在应用**建树**时解析一次（`Canvas` 是值语义快照，`paint/canvas.rs:33`），清页会让应用手里的 UV 全部悬垂（被复用后画出别人的图）⇒ 溢出直接 `DecodeCause::Capacity`，不 clear、不换代                                                                                                                                                                                                                                                                                                                                           |
| **F-141** | **图片图集按 `contract.md` §8 实现「线性预乘 → sRGB 重编码」的 BGRA8**：`srgb_decode(rgb) * alpha` → `srgb_encode` → 8 bit（alpha 直存）。事实核对：已落地的 `render/shaders/contract.md:190-193` 冻结的就是这条（「纹素是 linear-premultiplied 后 sRGB 编码」「上传方职责：P15 必须 linear 空间预乘 → sRGB 编码」），而 phase-02 §10.3 写的「直通 alpha」与之冲突 ⇒ **以 `contract.md` 为准，P02 该句作废**。理由：①直通 alpha 在预乘混合下会亮边，且要改三套 shader；②编码域预乘 probe 实测最大差 **60/255**（中灰 128 @ a=128：64 vs 93）；③本方案 shader 零改动，8 bit 往返最坏线性误差 **0.0097**。实现用 64 KiB `OnceLock` LUT（`(color<<8)\|alpha`）            |
| **F-142** | **只接受 PNG / JPEG / WebP**，且 `image::Limits` 用 `default()` + 字段赋值：`guess_format` 与 feature 无关（未开 `gif` feature 也返回 `Ok(Gif)` ⇒ 必须**策略性**拒绝，不能靠 feature 裁剪）；`Limits` 是 `#[non_exhaustive]`（结构体字面量 = `E0639`）；16-bit PNG 经 `to_rgba8()` 静默降位（0/62/125/187 实测），资产契约写明「字节必须已是 sRGB，引擎不做 ICC/gAMA」                                                                                                                                                                                                                                                                                                 |
| **F-143** | **golden 字节 = 读回字节原样（预乘 sRGB RGBA8）**，不反预乘（反预乘要除 alpha，低 alpha 放大误差、破坏逐字节可复现）；PNG 无损往返实测逐字节一致。v1 只建 `tests/golden/warp/`，`lavapipe/` 与 `metal-real/` 在对应 job 真正产出 golden 时再建（空目录是噪音）；golden 在普通看图器里偏暗，`tests/golden/README.md` 写明                                                                                                                                                                                                                                                                                                                                               |
| **F-144** | **文字 / emoji 像素 golden 不进 v1**（主设计 §6.5 的「字形（拉丁/CJK/emoji）」维度暂缺）：引擎不随包字体（D17 / Fontique 系统字体），字形像素随宿主机字体文件/版本变化 ⇒ golden 跨机必红。替代覆盖：`rendered_text_produces_visible_glyph_pixels`（覆盖率 sanity）+ L2 的 `GlyphRun` 字段断言 + P06 的塑形语料。启用前提是打包固定字体资产（`FontStack` 指向资产字体，契约变更），登记 V-96                                                                                                                                                                                                                                                                            |
| **F-145** | **`FrameDump` 是「可 diff 的结构化记录」，不是已实现的 replay**（主设计 §9.1 写「可重放记录」）：v1 导出 `InstanceDump` / `OpDump` / `ClipDump` / damage 的强类型 DTO + 稳定文本，**没有** `--replay` 工具，也没有「dump + atlas → 重放」的入口。真正的 replay 要封装 atlas 页内容 + shader 产物 + 平台 renderer，属独立工具                                                                                                                                                                                                                                                                                                                                           |
| **F-146** | **诊断挂既有 `test-support` feature，不新增 `diagnostics` feature**：四个 dump 的输入（`FrameRecord` / `InstanceBuffer` / `A11yTreeSnapshot` / `FrameStats`）本身只在 `test-support` 存在，两个 feature 会产生非法组合。发布物自查：`findstr /M /C:"FrameDump" target\release\ui_core_demo.exe` 必须无输出                                                                                                                                                                                                                                                                                                                                                             |
| **F-147** | **`AtlasAllocator::deallocate` 生产化；`ResourceCache` 不做自动 LRU**（主设计 §9.3 的「图集 `etagere` + LRU + `generation`」在 v1 只保留前两者）：`deallocate` 的幂等槽位表从 `#[cfg(test)]` 提升为生产代码（`render/atlas.rs:29-33,154-166`）；自动 LRU 逐出的前置条件是 ①`bind_canvas` / 画布可重建契约 ②换代后全量 PAINT 与应用重建的协调 ③逐 tile 引用计数——三者都缺，硬做会得到静默错图。淘汰策略归应用层（D15），引擎只提供 `unload` 机制                                                                                                                                                                                                                        |
| **F-148** | **`AtlasSet` 删除**：字形分配器直接是 `App::atlas: AtlasAllocator`，图片分配器 + `Page` 移入 `ResourceCache`。原因：①`AtlasSet::clear_all()` 在 **DPI 换代**时把图片页也清空（`app.rs:2281-2284`）= 悬垂 UV；②图片 tile 与 scale 无关，本就不该随 DPI 换代；③只剩一个分配器时 `AtlasSet` 是空壳包装（AGENTS 1.2）                                                                                                                                                                                                                                                                                                                                                      |
| **S-128** | **`tests/` 与 `benches/` 的 target 形态（M-09 落地细则）**：`tests/snapshot.rs` 用 `required-features = ["test-support"]`；`benches/perf.rs` 用 `[[bench]] harness = false` 且**无** `required-features`（只用公开 API，`cargo bench --no-run` 在默认 features 下可编译）；`tests/golden/` 只是数据目录；不建 `tests/perf.rs`                                                                                                                                                                                                                                                                                                                                          |
| **S-129** | **`SnapshotCase` 的 `scene` / `theme` 不参与渲染**：`TestApp::snapshot` 的语义仍是 P08 §7.7 的「读最近一次提交的实例缓冲」，只消费 `size` / `scale`；`scene` / `theme` 的消费者是 `snapshot::mount` 与 `golden_name` / `golden_path`（两个真实消费者，不是死字段）                                                                                                                                                                                                                                                                                                                                                                                                     |
| **S-130** | **`criterion` 关闭默认 features（写在 workspace 根）**：`criterion = { version = "0.8.2", default-features = false, features = ["cargo_bench_support"] }` ⇒ 去 `plotters` / `rayon`（probe：`cargo tree` 无二者，bench 可编译可跑）。**实现期修正**：Cargo 不允许在继承 workspace 依赖时覆盖 `default-features`（`criterion = { workspace = true, default-features = false }` 直接报错），因此这条配置必须落在根 `Cargo.toml`，子 crate 只 `criterion = { workspace = true }`。`criterion::black_box` 在 0.8 已 `#[deprecated]` ⇒ benches 一律 `std::hint::black_box`。`Cargo.lock` 只有 `ui_core` 依赖表里新增 `"criterion"` / `"image"` 两行边（无新包、无版本变化） |
| **S-131** | **集成测试的 GPU 串行锁**：`test_support::snapshot::gpu_guard()`（`test-support` 构建）与 lib 单测的 `render/windows/device.rs::gpu_test_lock()`（`#[cfg(test)]` 保持）同构、同语义（进程级 `Mutex` + poison 恢复），但**不共享同一个静态**（跨二进制不共享地址空间）。理由：多个 WARP 设备并行会互相拖垮（本机实测 device removed）                                                                                                                                                                                                                                                                                                                                   |
| **V-95**  | **golden 跨设备 / 软件 vs 真机差异未量化**：v1 只有 WARP golden。L3 任一处开始出 golden 时，用同一 case 跑 WARP 与真实 GPU，量化 max/avg/超容差像素，回填 `Tolerance::default()`（当前 `1 / 0`，按 D19 不冻结）与 `tests/golden/README.md`                                                                                                                                                                                                                                                                                                                                                                                                                             |
| **V-96**  | **文字 / emoji 像素无 golden**（F-144）：打包固定字体资产（拉丁/CJK/emoji）并让 `FontStack` 能指向资产字体后，**新增** `SnapshotScene::Glyphs` 变体并补 §6.5 的 emoji 维度                                                                                                                                                                                                                                                                                                                                                                                                                                                                                             |
| **V-97**  | **`image` 解码 feature 的二进制体积增量未实测**：六个 codec 已在 `base_ui` 的依赖图内 ⇒ 引擎不新增 codec 代码，但 release 体积增量未测。发布体积预算复核（与 V-47 同批）时用 `cargo bloat`/安装包差分；超预算则改 workspace 的 `image` feature 列表（影响 `base_ui`，需单独立项）                                                                                                                                                                                                                                                                                                                                                                                      |
| **V-98**  | **图片页上传仍整页 `to_vec`**（P09 遗留，`render/windows/atlas.rs:101`）：脏行已按 `DirtyRows` 精确，但 `queue_upload` 每次都会复制整页（16 MiB）。多图/大图时若成热点，与 V-64 的字形增量上传一起改成「行跨度缓冲」                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| **V-99**  | **低 alpha 的预乘 8 bit 量化误差**：probe 只量到 alpha ≥ 32（最坏线性误差 0.0097）。阴影贴图/淡出等低 alpha 图若出现色阶，评估 16-bit 图集（契约变更）或改走非图片路径                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| **V-100** | **每个 golden case 建两个 2048² WARP 纹理**（字形 4 MiB + 图片 16 MiB）：13 个 case 串行、单次运行目标 < 60 s。超时/内存吃紧时给 `App` 注入页尺寸（`SnapshotCase` 加 `page` 字段）或复用 renderer                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| **V-101** | **编码域缩放的半透明 fringe 未验证**：`image::imageops::resize` 在 sRGB 编码域重采样（与浏览器一致），透明像素带色时可能出现暗边/亮边。`Image` golden case 会暴露明显问题；修法是「先预乘再缩放」或线性空间重采样（成本 ×3）                                                                                                                                                                                                                                                                                                                                                                                                                                           |

### 18.2 本阶段**不**改写的既有条目（易被误判为需要更新）

| 条目            | 说明                                                                                                                                                                                    |
|-----------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-11**        | `usvg` 归 `base_ui` 的 build-dependency，引擎不声明。本阶段新增 `image` 依赖**不**改变该结论；门禁含 `cargo tree -p ui_core \| findstr usvg` 必须无输出                                 |
| **F-24**        | `AtlasKind` / `AtlasTile` 在 `paint::atlas`，`resources` 只留 `ResourceId` / `ResourceCache`（本阶段再加 `ResourceImage` / `DecodeCause`）。P01 §3 的注释仍按「只加注记、不改历史正文」 |
| **S-85**        | 字形页在 P09 走整页上传（P15 再改增量）。本阶段**没有**改渲染器上传 API：图片页沿用「整页 `pixels` + 脏行」，首次整页（保证 padding 为 0）；增量属 V-98                                 |
| **M-09**        | `tests/` 与 `benches/` 的 target 形态；本阶段的 S-128 是其落地细则，不构成新决策                                                                                                        |
| **V-22**        | 公开项 `missing_docs` 债务不补齐、但**不许扩大**：本阶段新增的公开项必须有 rustdoc                                                                                                      |
| **V-14 / V-44** | 图集单页容量与 `clear + retry` 逃逸属字形路径；图片路径的策略差异见 F-140，不修改这两条                                                                                                 |

### 18.3 门禁（实现期）

| 项                        | 期望                                                                                                                                                                                                                                                       |
|---------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 默认测试                  | `590 → 636`（实测）；既有 590 项逐项不变                                                                                                                                                                                                                   |
| `--features test-support` | `622 → 691`（实测）；集成 `6 → 14`（`frame_record` 6 + `snapshot` 8）                                                                                                                                                                                      |
| release                   | 682 项全绿（debug_assert 门控的 9 项在 release 下不跑，与既有口径一致）                                                                                                                                                                                    |
| 像素 golden               | `cargo test -p ui_core --features test-support --test snapshot` 在 Windows WARP 上 13 个 case 全绿；mac/Linux 只出 skip 日志且 exit 0（依据 `skip_reason()` + F-137）                                                                                      |
| bench                     | `cargo bench -p ui_core --bench perf --no-run` 通过；数值**不**进门禁（D19）                                                                                                                                                                               |
| lint / 格式 / 文档        | `clippy --all-targets --all-features -D warnings` / `fmt --check` / `doc --no-deps` 零 warning                                                                                                                                                             |
| 发布物                    | `test_support::*` 零符号（`findstr` 自查）；`usvg` / `plotters` 不在 `-p ui_core` 的依赖树                                                                                                                                                                 |
| 依赖锁                    | `git diff Cargo.lock` 只允许 `ui_core` 依赖表里的 `"criterion"` / `"image"` 两行边（无新包、无版本变化）。实现备注：Cargo **不允许**在继承 workspace 依赖时覆盖 `default-features`，因此 `criterion` 的 `default-features = false` 写在 workspace 根声明里 |

### 18.4 实施记录（2026-11-13，P13 落地）

| 门禁                                                                                           | 实测                                                                                                                                              |
|------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------|
| `cargo test -p ui_core`                                                                        | **636** passed（基线 590 + 46；1 ignored）                                                                                                        |
| `cargo test -p ui_core --features test-support`                                                | **691** lib + **6** `frame_record` + **8** `snapshot`（基线 622 + 69）                                                                            |
| `cargo test -p ui_core --release --features test-support`                                      | **682** lib + 14 集成（debug_assert 门控的 9 项不跑）                                                                                             |
| `cargo test ... --test snapshot`（13 个 golden）                                               | 全绿，**0.4 s**；`UPDATE_GOLDEN=1` 生成后同轮自检 `max=0 / over=0`                                                                                |
| `cargo +nightly clippy -p ui_core -p ui_core_demo --all-targets --all-features -- -D warnings` | **exit 0**                                                                                                                                        |
| `cargo +nightly fmt --check` / `cargo doc --no-deps`                                           | exit 0 / 零 warning                                                                                                                               |
| `cargo bench -p ui_core --bench perf --no-run`                                                 | exit 0（未跑数值；`cargo tree` 无 `plotters`）                                                                                                    |
| `cargo check --target aarch64-apple-darwin`（`SKIP_SHADERS=1`）                                | exit 0                                                                                                                                            |
| `cargo check --target x86_64-unknown-linux-gnu`（`SKIP_SHADERS=1` + stub `pkg-config`）        | exit 0                                                                                                                                            |
| 交叉 + `--features test-support`                                                               | exit 0；唯一 warning 是既有 **V-94**（`InFlightRing::reserve`，不在本阶段范围）                                                                   |
| 发布物                                                                                         | `findstr` 查 `FrameDump` / `DependencyGraphDump` / `A11yTreeDump` / `DamageOverlay` / `SnapshotDiff` / `UPDATE_GOLDEN`：release demo **全无命中** |
| `git diff Cargo.lock`                                                                          | 仅 `ui_core` 依赖表的 `"criterion"` / `"image"` 两行                                                                                              |

**实现期与本文档的差异（都不改语义，逐条登记）**：

- **`ResourceImage` 增加 `id: ResourceId` 字段**：设计里 `insert` 只返回 `tile`/尺寸，但那样应用拿不到句柄 ⇒ `unload`
  没有入口。加字段后「先插后卸」闭环，且 `ResourceImage` 仍是 `Copy`（24 → 32 B）。
- **`GoldenError` 增加 `Frame { detail }`**：`TestApp::drain()` 的错误（`TextShape` / `AtlasExhausted`）需要与「GPU
  失败」区分，否则只能塞进 `Snapshot` 变成语义谎言。
- **golden 场景多一层 backdrop**：`shapes` / `shadow` / `gradient` / `clip` 也铺主题底色，让 golden 覆盖「背景填充 +
  主题色」，而不是大片透明区（`colors` / `image` 本来就有）。
- **`SnapshotScene::size()` 是公开方法**：case 尺寸由场景自报（`tests/snapshot.rs` 与 `capture` 共用），避免在测试里复制尺寸表。
- **`App::resource_page_pixels` 的可见性收窄为 `#[cfg(feature = "test-support")]`**：生产路径用
  `resources.page_pixels()`（`upload_image_page` 内部），只有 `TestApp::snapshot` 需要这个 crate 级转发。
- **`test_support::snapshot::mount(app, scene, theme)`** 是自由函数（不是 `TestApp` 方法）：`impl TestApp` 只能有一个文件（
  `multiple_inherent_impl`），而场景构建属于 golden 层。
- **demo 的 `SnapshotCase`** 用 `Shapes`/`Light` 作元数据（场景由 `build_into` 自己挂载），与 S-129 一致。
- **`AtlasSet` 删除的连带改动**：`render/atlas.rs` 的 12 项测试里删 1（`AtlasSet` 的组合用例）、改写 1（`default_page_sizes`
  改为断言 `GLYPH_PAGE`/`IMAGE_PAGE`）、加 1（`deallocate_is_available_outside_tests`）；`app.rs` 的 `atlas.clear_all()` /
  `atlas.glyph*()` 全部收成 `atlas.clear()` / `atlas`。

### 18.5 评审修复（PASS-WITH-FIXES → 修复后 PASS）

**阻塞 ①（`frame_dump` 在未 `drain()` 时 panic）**：`TestApp::frame_record()` 内部
`expect("drain() must have produced a frame record")`。`frame_dump` 改用 `Window::record()` 的 `Option`：无提交帧时帧号 0、
`damage` 空、实例/op 取当前（空）缓冲，**不 panic**；新增 `frame_dump_on_a_fresh_app_is_empty`。

**阻塞 ②（图集页只上传给当前窗口）**：`frame_all` 逐窗提交，旧的 `upload_image_page(window)` 用进程级 `take_dirty_rows()`
，第一个窗口消费后其余窗口的图片/字形纹理永远收不到整页。改为 `App::upload_atlas_pages()`（无 `window` 参数）**广播**
给每个有渲染器的窗口：

- 脏旗标/脏行由本方法统一消费一次，然后广播（增量行 `clone()` 给所有已同步窗口）；
- 每个窗口记 `glyph_page_received` / `image_page_received`；**没收到过整页的窗口拿整页**（新建窗口、换渲染器）；
- `Window::set_renderer` / `enable_offscreen_renderer` 把两个标记清零（新渲染器 = 空纹理）；
- 新增 `atlas_upload_without_renderers_keeps_windows_unsynced` / `atlas_upload_marks_only_windows_with_a_renderer` /
  `atlas_pages_are_broadcast_to_every_window`（Windows，两个离屏渲染器）。

**非阻塞项**（逐条已改）：

| 项                                                     | 处理                                                                                                                                         |
|--------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------|
| `snapshot_without_gpu_is_a_typed_result` 与 S-124 冲突 | 改名 `snapshot_result_is_typed_per_platform`：Windows=match(Ok)、macOS=Ok（Metal 真读回）、Linux=`Err(Gpu)`                                  |
| 非 Windows `test-support` 引用 `render::windows`       | `rendered_text_produces_visible_glyph_pixels` 整体 `#[cfg(windows)]`；`SnapshotError` 的 `use` 也按平台门控                                  |
| bench `sample_size`                                    | 30 → 50（50 samples / 1 s warmup / 3 s 测量；理由写在 bench 文件头）                                                                         |
| bench `tree_build` 把 setup/drop 算进测量              | 改 `iter_batched`：setup 只建空 `App`，routine 才是 512 次建树                                                                               |
| bench `frame_incremental` 只挂 1 节点                  | 512 个静态 box + 1 个绑信号节点（真正的「复用 511 + 重绘 1」）                                                                               |
| `compare` 在 release 静默少比                          | 新增 `GoldenError::Malformed`，长度不符返回**显式错误**（不再用 `debug_assert`）；新增 `compare_rejects_a_malformed_buffer_in_every_profile` |
| LUT 测试采样太少                                       | 全 256×256 组合逐一对比标量公式；另加 `premultiply_fast_paths_are_byte_exact`                                                                |
| `DependencyGraph::dump` 路径与文档不符                 | 实际是 `DependencyGraph::dependents()`（`cfg test-support`）+ `diagnostics::dependency_dump`；文档 §4.3 / §11.3 已同步                       |

---

## 十九、P14 · 文档与交付修复（2026-11-13）

| 编号      | 条目                                                     | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
|-----------|----------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **S-132** | `test_support/oracle.rs` **并入 `app.rs`**（无独立文件） | P01 §3 的文件树写的是 `test_support/{probe,frame_record,snapshot,oracle}.rs`。实际落地**没有** `oracle.rs`：oracle 的两个入口 `TestApp::oracle_diff` / `TestApp::assert_oracle` 直接写在 `test_support/app.rs`。理由：①oracle 必须独占调用 `Window::force_full_repaint` 再逐 lane 比较，天然属于 `TestApp` 的职责；②拆独立文件会把 `impl TestApp` 劈成两处，违反 `multiple_inherent_impl = forbid`；③独立文件只会多一层 `pub(crate)` 转发。P13 附录 B 已按「取消」记录，本条是**唯一权威登记**                                                                                                       |
| **S-133** | **`tests/layout.rs`（L1 跨模块集成）**                   | 新增 `..`（`required-features = ["test-support"]`），4 个用例：flex 行/列主轴位置、固定尺寸 + padding、命中测试回退到父节点、滚动偏移后的命中。与 `src/layout/**` 的内联单测互补：内联单测测 taffy 适配器与 `hit_test` 纯函数，这里测「挂树 → 布局 → `Probe`」整链。`taffy` 因此同时成为 dev-dependency（`workspace = true`，无新增包）                                                                                                                                                                                                                                                              |
| **V-102** | **L3 平台策略 + CI 三平台 job 待补**                     | ①**L3（像素 golden）v1 只认 Windows WARP**：macOS 的 `new_offscreen` 能真读回但没有可用 golden 基线（`metal-real/` 尚未产出），Linux 的 `new_offscreen` 在构造期即返回 `UnsupportedGpu`（F-137）。因此 `test_support::snapshot::skip_reason()` 在非 Windows 返回原因，`tests/snapshot.rs` 每个 case 打印一行 `skip <case>: …` 并 **exit 0**（不是失败，也不是静默）。跨设备容差量化仍按 **V-95** 推进。②`.github/workflows/gui-ci.yml` 已写出 windows / ubuntu / macos 三个 job（本地只做了 YAML 语法 + 结构校验，**未在 runner 上跑过**）；首次三平台全绿后回填本条，并把 golden 扩到 `metal-real/` |

**同批文档修复（无编号）**：`..` 的测试命令改为**分层**（`cargo test -p ui_core` 只跑 L0/L1 单测；L2/L3 必须
`--features test-support`，含 `--test frame_record` / `--test layout` / `--test snapshot`），并补齐 release、bench、
`UPDATE_GOLDEN=1`、demo 运行命令与依赖状态说明（引擎子树零 `gpui-*`，产品侧仍未迁移，M8 范围外）；
`design/phase-01-foundation.md` §11 加注「unsafe 门禁已演进为 FFI-only + SAFETY」。

---

## 二十、P14 终审 · 实例编码与 clip 深度（2026-11-13）

| 编号      | 条目                                                                                       | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
|-----------|--------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-149** | **`GpuPrimitive.clip_rect` 的语义修正为契约的 `(x, y, w, h)`**                             | P06 编码器把内部 `ClipChain.aabb`（`(x0, y0, x1, y1)`，为求交方便）直接写进了 `clip_rect`，而 `contract.md` §2/§5 与三套 shader 按 `clip_rect.xy` + `clip_rect.zw` 读 `(x, y, w, h)`。原点为 0 的 clip 恰好两种表示相同，**负原点 clip（`Canvas::push_clip` 允许）会把右/下内容裁掉**。修复：`FrameEncoder::clip_rect()` 在写入前做 `(x0, y0, x1−x0, y1−y0)`，无裁剪哨兵保持不变（`clip_index == 0` 时 shader 不读）。空交集产生负 `w/h` 是合法的，shader 用 `max(clip_rect.zw, 0)` 夹成零面积。回归：`negative_origin_clip_encodes_xywh`（scale=2、负原点，断言物理 `(-20, -10, 40, 40)`，旧实现为 `(-20, -10, 20, 30)`） |
| **F-150** | **`EngineError::ClipDepthExceeded`：clip 嵌套 > `MAX_CLIP_DEPTH`（8）在 release 也硬失败** | P06 的 `paint_node` 只是把祖先前缀 `Scene::push` 进去；`Lane::push` 的上限只有 `debug_assert`，release 下第 9 层会把最外层丢掉 ⇒ 子内容越界可见。现在 `PaintPass::run` / `Window::paint_frame` 返回 `Result`，超限返回 `{ node, depth, limit }`；`Window::frame` / `App::frame` 中止本帧（不提交、`FrameIndex` 不变）。这是 P01 §6.2.2 冻结表之外的**新增变体**（与 `NotScrollable`/`UnknownWindow` 同类，按 F-xxx 登记，表本身不回填）。回归：`clip_depth_over_limit_is_an_error`（9 层 → `node` = 最内层、`depth == 9`）、`clip_depth_at_limit_is_allowed`（8 层正常）；`error.rs::all_variants` 同步                    |
| **V-103** | clip 深度硬失败的可用性边界                                                                | `MAX_CLIP_DEPTH` 仍与三套 shader 的固定数组长度绑定为 **8**；超过 8 层的合法 UI 现在会**整帧失败**（而不是错画），应用只能自行展平嵌套（把多层 clip 合并成一层 AABB 或裁剪子树）。P14 的三平台 goldens 没有覆盖 >8 层；若真实应用触达，需在 P15 评估「paint 侧把多余 clip 折叠进上一层 AABB」的降级路径（本轮按审查要求选择硬失败，不做静默降级）                                                                                                                                                                                                                                                                          |

---

## 二十一、P15 · demo 展示扩展（2026-11-13）

本阶段的范围是 `crates/ui_core_demo`（`design/phase-15-demo-showcase.md`）。三次引擎侧修正（F-151 /
F-152 / F-153）是 demo 的 UIA/输入法实测**暴露出来的既有缺口**，不属于「为 demo 加功能」。

| 编号      | 条目                                                                                              | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
|-----------|---------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-151** | **a11y 缺少 `Toggled` 通道：`RadioButton` / `CheckBox` / `Switch` 的勾选态在 UIA 里不可见**       | P11 的 `A11yPatch` 只有 `selected` / `expanded`，而 `accesskit_windows` 的 `is_selection_item_pattern_supported` 对 `Role::RadioButton` 只看 `toggled()`（`accesskit_windows-0.35.2/src/node.rs:648-680`）；只写 `set_selected` 的 RadioButton 在 UIA 里**根本没有 SelectionItemPattern**（P15 用 UIA 客户端 dump 出 `SelectionItem` 缺失，而 `Selected` 在 demo 里是 true）。修复：`A11yPatch` / `A11yNode` 增加 `checked: Option<bool>`（与 `selected` / `expanded` 同型：布尔状态只走动态补丁，`A11yOverride` 不承载），`a11y::adapter::to_accesskit` 写 `set_toggled(Toggled::from(v))`（`Some(false)` **也要**写 `Toggled::False`，否则 pattern 仍不暴露）。回归：`adapter.rs::radio_checked_reaches_the_toggled_channel`（显式 false ⇒ `Toggled::False`、`is_selected() == None`）、`hidden_disabled_and_patch_fields_reach_accesskit` 增补 checked；demo 的 `A11yPatch` 从 `selected` 改为 `checked`。实测：UIA dump 现在给出 `SelectionItem(selected=True/False)`（见 `design/phase-15-demo-showcase.md` §6）                                                                                                                     |
| **F-152** | **状态写入只置 `App.wake_requested`，不唤醒 `Dispatcher`：回投任务里的状态不落帧、也不交付 a11y** | 平台泵的顺序是「事件 → 钩子 → 帧 → a11y → 回投任务」；`Dispatcher::push` 的唤醒位在**帧之后**才被任务消费，任务里的 `App::write` / `set_focus` 只置 App 的 `wake_requested`，于是循环回到 `GetMessageW` / `poll` 阻塞 —— 画面与 a11y 更新要等下一条无关消息。UIA 实测：`ValuePattern.SetValue` 改的只是下一笔 delivery 才可见，`SelectionItemPattern.Select()` 甚至完全不生效。修复：`App::request_frame()`（置位 + `Dispatcher::wake()`）成为常规写点（`set_focus` / `scroll_to` / 浮层 / 动画，共 11 处），`invalidate` 因为已按字段解构 `self`，在其中直接 `*wake_requested = true; dispatcher.wake()`（第 12 个写点）。回归：`app::tests::writing_a_signal_wakes_the_platform_loop`、`a11y::action::tests::a11y_focus_wakes_the_event_loop`。实测：UIA `SetValue("80")` → 滑块 value 80、`Select()` → 单选 B 选中且焦点环可见                                                                                                                                                                                                                                                                                                         |
| **F-153** | **`bind_text` 的内容变化只标 `PAINT`，不重发 a11y**                                               | `refresh_styles` 的 `text_changed_here` 分支只 `mark_paint`。文本节点的可访问名/值来自文本内容，**等宽替换**（「方向键 → 70%」→「方向键 → 80%」）不会触发 `LAYOUT`，AT 会一直读旧值。修复：同一分支追加 `tree.mark_a11y(node)`。回归：`app::tests::changed_text_marks_a11y`（消费首帧 A11Y 位后做等宽替换，断言重新标脏）、demo 的 `arrow_key_updates_the_slider_and_its_a11y_value`（第二笔同等宽度，断言状态行随增量交付重发）                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| **V-104** | Windows 11 Microsoft Pinyin（TSF 模式）不向 IMM32 客户端发送组合串更新                            | 本机实测：点击输入框后键入 `nihao`，应用收到 `WM_IME_STARTCOMPOSITION` + `WM_IME_COMPOSITION(GCS_RESULTSTR)` + `WM_IME_ENDCOMPOSITION`（demo 显示「共 3 次」、正文出现「你好」），但**没有** `GCS_COMPSTR` 的 `Preedit` 事件，`ImmGetCompositionStringW` 在组合期间也拿不到串（TSF 文档管理器缺失时 MSPY 用自己的组合 UI，只在提交时走 IMM32 兼容路径）。因此 demo 输入框内的**内联组合串**在真机 Windows 上看不到；`Start` 也不在组合期间到达（本机实测 Start/Commit/End 三条都在提交时）。所以应用侧**无法感知**真实组合：`ImeField.composing` 由 `Start`/`Preedit`/`Commit`/`End` 驱动（Linux/macOS 平台层不发 `Start`，经 `Preedit` 进入组合），`Start` 无组合串时状态行显示「组合中：（输入法处理中）」；「组合中 Esc 不关窗」只对按时发 `Start` 或 `Preedit` 的平台/IME 生效，本机 MSPY 若 Esc 被 IME 放行到应用会走关窗路径。`ImeEvent::Preedit` 的渲染由 `--snapshot` 的种子状态（`Scene::preview`）与 `scene::ime` 的单元测试覆盖，macOS `NSTextInputClient` / Linux `zwp_text_input_v3` 的 preedit 路径仍只有静态检查（P12 的边界不变）。要补齐 Windows 内联组合需要 TSF `ITfThreadMgr` / `ITextStoreACP`，属新工作项，不在 P15 |
| **V-105** | demo 的 CJK 字体依赖宿主系统                                                                      | 「无豆腐块」的断言只对**本机**（Windows 11：Microsoft YaHei UI / Malgun Gothic 经 fontique 兜底）成立；`FontStack::system_ui` 的候选表在 Linux 容器（无 Noto CJK）上会退到 generic 并记 `FontStackMiss`。`examples/demo/scene.rs` 的断言是「没有 `MissingGlyph`」，在缺 CJK 字体的宿主上会失败 —— 这是**有意的**（字体栈缺失必须可观测，D17），不是 flaky；P18 的 Linux CI 已安装 `fonts-noto-cjk`，本地容器仍可能没有                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| **V-106** | P15 当时没有 caret / 组合窗定位 API；P16 已补精确 caret hint                                      | P15 的 Win32 把 IME 组合窗放在**最后指针位置**（`platform/windows/event_loop.rs` 的 `set_composition_point(hwnd, state.cursor)` → `ImmSetCompositionWindow(CFS_POINT)`）；macOS `firstRectForCharacterRange` 固定 1×1（`platform/macos/view.rs`），Linux 固定 `set_cursor_rectangle(0,0,1,1)`（`platform/linux/surface.rs`）。**P16 已用 `App::set_ime_cursor_area` + `Window::text_caret_rect` 关闭该缺口**：应用声明文本节点/UTF-8 byte index，引擎计算 caret 矩形并由三平台转发（见 F-154）；仍需后续编辑模型才能提供完整选区 / `SetTextSelection`                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |

---

## 二十二、P16 · IME 精确光标定位（2026-11-13）

本阶段范围是 `design/phase-16-ime-caret.md`。P15 demo 的实机反馈（首次启动/无指针时 IME 组合窗落在左上角）
定位到 Win32 的「最后指针位置」回退 + 缺失 `ImmSetCandidateWindow`；P16 新增引擎级 caret hint，三平台转发。

| 编号      | 条目                                                 | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
|-----------|------------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-154** | **新增 IME caret 区域 API（解决 V-106 的定位缺口）** | `App::set_ime_cursor_area(window, text_node, byte_index)` 登记文本节点 + UTF-8 插入点；`NodeTree::text_caret_rect` 用 parley `Cursor::geometry` 生成 1 逻辑像素宽的 caret 矩形（含 content offset / 祖先滚动），`Window::text_caret_rect` 换算窗口绝对坐标；`App::ime_cursor_area` 供平台泵读取。Win32 用 `ImmSetCompositionWindow(CFS_POINT, caret 底边)` + `ImmSetCandidateWindow(CFS_EXCLUDE, caret 区域)`，`WM_IME_STARTCOMPOSITION` 重放，并删除最后指针回退；macOS `firstRectForCharacterRange` 返回 caret 屏幕矩形；Linux `set_cursor_rectangle` 使用 caret 逻辑矩形。demo 的输入框正文子节点在 `Scene::sync_focus` 中上报 `display(false).len()`，失焦清除。回归：inline_flow caret 几何、App set/get/clear 与陈旧文本错误、padding/border + 祖先滚动、Win32 `caret_forms` 与隐藏窗口 smoke、demo 输入框 caret 随「你好」右移并在失焦后 `Ok(None)` |
| **V-107** | **P16 IME caret 的平台/表示限制**                    | ① `CFS_POINT` 允许 IME 按自身策略调整组合窗位置，不能保证与 `ptCurrentPos` 逐像素相等；不采用 `CFS_FORCE_POSITION`。② Windows 11 Microsoft Pinyin 的 TSF 路径可能自行决定候选窗位置（V-104 的组合串边界仍在）；`ImmSetCandidateWindow` 是 IMM32/CUAS 兼容提示，不是 TSF 文档管理器的完整实现。③ 空文本没有 parley 行 metrics，caret 高度用 `TextStyle.size × line_height.unwrap_or(1.0)` 近似，不含字体 leading。④ macOS/Linux 只做交叉编译静态检查，未实机验证 IME UI 位置；Wayland 当前绑定 `zwp_text_input_v3` v1，`commit()` 直接生效（v2+ 还需 surface commit，见 `platform/linux/surface.rs` 的实现注释）。⑤ 引擎仍无完整选区/`SetTextSelection`/插入点移动模型；`byte_index` 由应用维护                                                                                                                                                             |

---

## 二十三、P17 · 引擎内置 Debug HUD（2026-11-13）

本阶段范围是 `design/phase-17-debug-hud.md`。HUD 通过 `ui_core` 的 `hud` feature 编译；
demo 默认 feature `hud = ["ui_core/hud"]`，因此默认显示、`--no-default-features` 可关闭。

| 编号      | 条目                                                     | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
|-----------|----------------------------------------------------------|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-155** | **新增 `App::set_debug_hud` / `App::debug_hud_enabled`** | HUD 是 root 末尾的普通子树（`debug-hud` + 10 个固定文本节点），右上角 244×184 浮层；`hit_shape(false)` 保证命中穿透，所有节点 `A11yOverride{hidden}`，不声明 Action/FocusRing。文本更新在 `App::frame` 内分两段：`prepare_debug_hud`（帧门禁前采样 + 请求 HUD damage）与 `apply_debug_hud`（`refresh_styles` 后、`refresh_text` 前直接 `set_text_content` + `mark_paint`）；`pending_text` 在成功提交后才清，帧中止时下一帧重写。HUD 可见时 `App::needs_continuous_frames` 让三平台定时器持续出帧，数字每 500 ms 汇总。`RenderStats` 由三平台 `Renderer::submit` 返回，DrawCall 按「full 一次 / 每个 damage scissor 一次」计数；`FrameTimings::sample` 提供 CPU/GPU/Upload/Present。                                                                                                                                      |
| **V-108** | **P17 HUD 的边界**                                       | ① `Frame`/`FPS` 采用相邻 `App::frame` 入口间隔（非 GPU 完成时刻）；CPU 行不含 `submit_gpu`/Present 等待。② 无 swapchain 的 offscreen 窗口 Present 段可能是 `0.00 ms` 而非 `--`。③ HUD 面板在小窗口上可占 damage 面积 >50%，此时 `DamageAccumulator` 退化为 full damage，不再保证 partial commit。④ 最小化/完全遮挡不做可见性过滤，HUD 可见期间仍按 60 Hz tick 唤醒。⑤ HUD 是普通 root 子树：`create_overlay`/`bring_overlay_to_front` 会自动把 HUD 提回最上层，但应用直接 `append_child` 的 root 子节点仍可能覆盖 HUD。⑥ 直接调用 `Window::frame` 不刷新 HUD 文本（HUD 没有 `bind_text`，不会触发 no-bindings 断言，但更新入口是 `App::frame`/`Application`）。⑦ macOS 交叉 `cargo check` 通过；Linux 交叉构建在 Windows 宿主缺 `pkg-config`/`wayland-client`/`fontconfig` sysroot 时无法完成，属环境限制，未伪装成通过。 |

---

## 二十四、P18 · demo 并入 example（2026-11-13）

本阶段范围是 `design/phase-18-demo-example.md`。用户确认删除独立 `crates/ui_core_demo` crate，
保留 `hud` 默认关闭，并用 `snapshot_demo` 与 `demo` 共用基础 scene。

| 编号      | 条目                                                                       | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                |
|-----------|----------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-156** | **demo 不再是 workspace crate，改为 `ui_core` 的两个 example**             | 删除 `crates/ui_core_demo/**`、workspace member 与 workspace dependency。新增 `examples/demo/main.rs`（`[[example]] name = "demo"`，`test = true`）与 `examples/demo/snapshot.rs`（`name = "snapshot_demo"`，`required-features = ["test-support"]`，`test = false`），两个源文件同目录 `pub mod scene;` 共用 `scene.rs` / `scene/`。`snapshot_demo` 保留 `--snapshot [path]` 参数与默认 `kytrix-demo.png`；不安装 HUD。                                                                                                                                                                                                                                                                                                                                                                                                            |
| **S-134** | **example 不能有独立默认 feature：HUD 显式开关，测试改 example unit test** | `Cargo.toml` 的 `[features]` 只属于包，example 无法保留原 demo 的 `default = ["hud"]` / `snapshot`。`hud` 仍默认关闭，运行 `demo` 需显式 `--features hud`；截图用 `--features test-support`。原 `tests/scene.rs` 13 项集成测试按职责搬进 `examples/demo/scene*` 的 `#[cfg(all(test, feature = "test-support"))]` unit test，共享搭建放 `scene/test_support.rs`；不使用 `#[path]`、不恢复 lib.rs、不另建 tests crate。显式 `[[example]]` 的 `test` 默认是 false，`demo` 必须写 `test = true`。                                                                                                                                                                                                                                                                                                                                       |
| **V-109** | **合并后的调用/验证边界**                                                  | ① 显式 `[[example]] demo` 的 `test = true` 让 plain `cargo test -p ui_core` 也运行 23 项场景单元测试；13 项场景测试需要 `test-support`，只跑默认测试不能证明它们全跑。`--features test-support` 跑 35 项，加 `hud` 跑 36 项；`--all-targets` 还会在 `snapshot_demo` 里再跑同一份共享测试（36 项），Cargo 无法只对其中一个 example 关闭。② 无 HUD 的 `demo` 是正常构建，F12 测试只在 `--features hud` 下存在。③ `snapshot_demo` 与旧 demo 的 `--features snapshot` 不逐字节等价：旧 demo 默认同时启用 `hud`，副标题含「F12 调试 HUD」；新命令加 `--features "hud,test-support"` 时截图哈希与迁移前完全一致，只用 `test-support` 时仅副标题少该短语，HUD 均不安装。④ Linux/macOS CI 的 `--features test-support` 会运行 35 项场景测试，依赖宿主 CJK 字体（V-105）；Linux job 增装 `fonts-noto-cjk`，Windows 另显式跑 36 项 HUD 测试。 |

---

## 二十五、Linux 实机验证与修复（2026-11-13）

本轮在 Ubuntu 26.04（Vulkan / lavapipe / Wayland）上实机验证 `ui_core`，修复 11 项缺陷。
完整验证记录见 [`linux-verification.md`](linux-verification.md)。本节只登记与设计文档不同的事实。

| 编号      | 条目                                                        | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                       |
|-----------|-------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-145** | **GLSL 的求导函数必须离开顶点可见范围**                      | `common.glsl` 被 7 个 stage 包含（3 个 vertex）。GLSL 禁止顶点阶段调用 `fwidth`/`dFdx`/`dFdy`，glslang 对**整份编译单元**报错（即使函数从未被顶点入口调用）。拆出 `common.frag.glsl`（`aa_coverage` + `clip_mask`），4 个 `.frag.glsl` 各 include 两份；`build.rs` 的 `SOURCES` 从 14 项变 15 项。HLSL/MSL 在两个 stage 都允许导数，故 `common.hlsl` / `common.metal` **不拆**                                                                   |
| **F-146** | **`wl_seat.get_keyboard` / `get_pointer` 必须先看 capabilities** | 协议要求只有 seat 声明过的能力才能 bind。无条件调用会被服务端判 `wl_seat error 0` 并断开连接，随后 Vulkan 报 `ERROR_SURFACE_LOST_KHR`（错误信息指向 swapchain，根因在平台层）。`create_devices` 改为按 `seat_caps` 幂等建/释放，并在 `wl_seat::Capabilities` 事件里重跑（覆盖能力后到齐与热插拔）                                                                                                                                          |
| **F-147** | **`wl_data_device` 必须实现 `event_created_child`**          | `data_offer` 会创建 `wl_data_offer` 子对象。缺 `event_created_child!` 时 wayland-client 在收到**第一个** offer 时 panic；mutter 建窗即发 offer ⇒ GNOME 上启动必 `abort`。headless sway 不发 offer，所以只在静态检查或 sway 下测不出来                                                                                                                                                                                                        |
| **F-148** | **`Swapchain::resize` 必须走 `oldSwapchain`**                 | 一个 surface 同一时刻只能被一个 swapchain 持有。先建后销毁 ⇒ `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR`（首次 resize 必失败）。改为 `SwapchainCreateInfoKHR::old_swapchain(old)`，创建成功后销毁旧的（规范路径，驱动可复用资源）                                                                                                                                                                                                                    |
| **F-149** | **Linux 事件循环必须在无窗口时返回**                          | `Application::run` 的契约是「跑到没有窗口」。Windows 用 `should_exit`、macOS 用 `windows.is_empty()`，Linux 原先只在 `xdg_toplevel.close` 时返回 ⇒ 应用自己调 `App::close_window`（demo 的 Esc）之后 71% CPU 空转。循环顶部补 `app.windows().next().is_none() → return Ok(())`                                                                                                                                                            |
| **F-150** | **Linux 新增无 surface 的离屏渲染器（取代 F-137 的「恒 `UnsupportedGpu`」）** | 产品**窗口**路径仍必须有真实 `wl_surface`（F-137 不变）。但「没有窗口也要画」的路径（`Window::enable_offscreen_renderer`、像素快照）新增 `VulkanInstance::new_headless()`（不启用 WSI 扩展、`surface = null`）+ `Renderer::with_headless()`（只渲染到 `persist`，不 acquire/present）。目标格式固定 `B8G8R8A8_SRGB`。这条路径**不是**产品运行时的软件回退（不满足 D8 时仍报错退出）                                                          |
| **S-135** | **glslang 的 `-I` 必须紧贴目录**                              | `glslangValidator` 要求 `-I<dir>`，写成 `-I` + 独立参数会报 `-I<dir> include path must immediately follow option (no spaces)` 并 exit 1。`build.rs` 用 `.arg(format!("-I{}", dir.display()))`                                                                                                                                                                                                                                              |
| **S-136** | **像素 golden 按设备分目录（D14 落地）**                       | `SnapshotCase::golden_path()` 改为 `tests/golden/<device_dir()>/`；`device_dir()` = `warp`（Windows）/ `lavapipe`（Linux）/ `metal-real`（macOS）。`skip_reason()` 只在 macOS 返回原因（无实机 GPU runner 产出基线）。新增 `tests/golden/lavapipe/` 13 份基线                                                                                                                                                                              |
| **S-137** | **剪贴板：mime 协商 + 协议顺序 + 读回上限**                    | ① 记录 `wl_data_offer.offer` 声明的 mime，按 `text/plain;charset=utf-8` > `text/plain` > `UTF8_STRING` 挑第一个双方都支持的；② 必须先 `accept(mime, ...)` 再 `receive(mime, fd)`（原实现漏 `accept`）；③ 处理 `wl_data_source.cancelled`（对端换属主时清空已发布文本）；④ 读回 `reader.take(1 MiB)`。**未做**：异步请求 API（`clipboard_text()` 语义不变）、primary selection                                                                      |
| **V-110** | **Linux 的 `wl_output` 整数倍率回退路径未实机验证**            | 只验了 `wp_fractional_scale_v1`（120/180/240 → 1.0/1.5/2.0）。没有该扩展的合成器会走整数 `preferred_buffer_scale`，这条路径未跑                                                                                                                                                                                                                                                                                                            |
| **V-111** | **真实输入法组合串未验证**                                    | VM 装了 `ibus-libpinyin` / `fcitx5`，但 GNOME 会话未启用输入法。本轮用 `wtype` 直出字符 + `zwp_text_input_v3` 协议流（`enable` + `set_cursor_rectangle` 随插入点移动）验证 caret 定位；**拼音/日文的组合串、候选窗、上屏时序未验证**（Windows 侧 V-61 / V-104 的口径不变）                                                                                                                                                                  |
| **V-112** | **lavapipe 与 WARP 的像素差异已量化，容差不冻结**              | 13 个 case 的逐通道差：`max` 64–164、`mean` 0.004–0.063、超 1 的像素 3–104 个（合计占比 < 0.3%）。集中在 AA 边缘与阴影渐变，是软件光栅舍入差异，不是指令漂移。`Tolerance::default()`（`per_channel = 1`、`max_over_tolerance_pixels = 0`）**只对同设备比对有效**；跨设备必须各用各的 golden                                                                                                                                              |
| **V-113** | **验证环境是合成会话，不是产品目标桌面**                      | mutter 不实现 `wlr-screencopy`、也无输入注入接口，因此用**嵌套 sway**（父合成器 mutter）做截屏与输入注入夹具；另用 headless sway（`WLR_LIBINPUT_NO_DEVICES=1`，seat `capabilities: 0`）复现 F-146。KDE / Sway 作为产品目标合成器、多显示器混合 DPI、Orca 真实读屏器联动均未验证                                                                                                                                                        |

---

## 二十六、macOS 实机验证与修复（2026-11-13）

本轮在 macOS 15.8.1 / Intel Iris Plus 655（Metal 3）上实机验证 `ui_core`，修复 7 项缺陷/缺口，并完成
**macOS-only 的事件循环 + 异步提交 + 窗口 render 线程**三轮架构改造。完整验证记录见
[`macos-verification.md`](macos-verification.md)。本节只登记与设计文档不同的事实。

| 编号      | 条目                                                        | 内容                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
|-----------|-------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **F-157** | **`NSTextInputClient` 的 NSAttributedString 必须取 `-string`，不能用 `description`** | `setMarkedText:` 收到的是带属性的 `NSAttributedString`；`-[NSObject description]` 会连同 `NSUnderline` 等属性一起输出，真实 Pinyin 的预编辑串被污染成 `ni'hao{ NSUnderline = 2; … }`。`insertText:` 收到 NSString，所以上屏正常。`platform/macos/view.rs::object_text` 改为先 `downcast_ref::<NSAttributedString>()` 取 `string()`，再 NSString；未知对象才回退 description。单测构造带 `NSUnderline` 属性的 `NSMutableAttributedString` 断言只返回正文 |
| **F-158** | **Metal 的 present 必须在 commit 前登记，`allowsNextDrawableTimeout` 必须为 true** | 旧顺序 `waitUntilCompleted → drawable.present()` 让 3 张 drawable 在同步等待期间一直被占用；`allowsNextDrawableTimeout(false)` 又在合成器暂停（AX 查询/遮挡）时无限阻塞。改为在 commit 前 `MTLCommandBuffer::presentDrawable:`，并启用超时；`nextDrawable` 返回 `None` 时按「本帧没拿到」重试。F-161 进一步异步提交，F-164 把 drawable 获取/编码整体移到 render 线程 |
| **F-159** | **`PointerLeave` 需要 NSTrackingArea；不能同时声明 `.MouseMoved`** | `GuiView` 之前只实现 `mouseExited:` 而没有 tracking area，事件永不触发；`NSWindow.setAcceptsMouseMovedEvents(true)` 只投递 `mouseMoved:`。在 `GuiView::new` 安装 `InVisibleRect \| ActiveInKeyWindow \| MouseEnteredAndExited` 的 area；不声明 `.MouseMoved`，否则窗口与 tracking area 各投递一次，每个移动产生两条 `PointerMove`（临时 event probe 实机复现）。objc2-app-kit 增加 `NSTrackingArea` feature |
| **S-138** | **macOS 像素 golden 启用，`skip_reason()` 删除** | 三个平台现在都跑 L3；`device_dir()` 仍为 `warp` / `lavapipe` / `metal-real`。删除 `skip_reason()` 及测试里的 skip 分支，新增 `tests/golden/metal-real/` 13 份基线；更新 `tests/snapshot.rs` / `tests/golden/README.md` / `Cargo.toml` / README。跨设备仍各比各的基线（D14） |
| **F-160** | **macOS 主循环改用 `NSApplication::run()`** | 自持 `nextEventMatchingMask` 循环无法完成 AppKit 原生 fullscreen/live-resize 转场（转场停在旧帧快照，AX 树是 AXUnknown 且无按钮）。改为 `NSApp.run()`；泵由 `CFRunLoopObserver`（BeforeWaiting/AfterWaiting）+ wake `CFRunLoopSource` + 连续帧 `CFRunLoopTimer` 驱动，三者都挂 **common modes**；退出用 `NSApp.stop(None)` + `CFRunLoopWakeUp`。取代 F-133 的自持循环理由；Windows/Linux 事件循环不变 |
| **F-161** | **Metal 命令提交异步化（macOS 窗口路径）** | `Queue::submit` 只 `commit`，不 `waitUntilCompleted`；command buffer 留在 `in_flight[3]`，下一次 `submit`/`wait_idle` 轮询 `status`，完成后 resolve GPU timer 并释放 retire 句柄；`wait_idle` 只用于 resize/readback/shutdown。GPU 时间写入下一帧的 `FrameTimings`（HUD 领先一帧）。离屏 golden/readback 仍同步等待。F-164 之后这些调用都发生在 render 线程，主线程只投递最新帧 |
| **F-162** | **原生全屏需要 opaque + FullScreenPrimary + contentView autoresizing** | `NSWindow` 默认 `collectionBehavior=0`、透明背景、contentView 无 autoresizing；实测全屏后 view/layer 已是 1680×1050 但屏幕停在旧 960×740 快照。改为 `setOpaque(true)` + 黑色背景、`FullScreenPrimary`、`ViewWidthSizable\|ViewHeightSizable`。修复后 HUD 关闭时全屏进出正常、无残留 |
| **F-164** | **窗口路径的 Metal drawable 获取与编码移到独立 render 线程** | 主线程构造 `GpuRenderer` + `CAMetalLayer` 后整体移交给 `kytrix-metal` worker；主线程只把实例字节/绘制序/裁剪表 (`FramePayload`) 与图集控制投进最新帧邮箱。worker 独占 `nextDrawable`、命令编码、`presentDrawable`、resize/DPI 和命令完成轮询；主线程不再访问 `CAMetalLayer` 绘制属性。worker 忙时新帧替换旧帧并强制全量；拿不到 drawable 时把 job 放回邮箱，100ms 后重试。离屏/快照路径仍同步，`Window`/`App`/golden 语义不变。F-164 关闭 V-114/V-119；实测 HUD 60fps 全屏完整 AX 树 0.20s，主线程 sample 不再出现 `nextDrawable` |
| **F-165** | **HUD 连续帧下退出必须撤掉定时器并投递 NSEvent** | HUD 开启时关窗，泵已设置 `exit` 并调用 `NSApp.stop`，但 60Hz `CFRunLoopTimer` 继续唤醒 run loop，`NSApplication::run` 睡在 `nextEventMatchingMask` 等不到 NSEvent，进程不退出。退出路径先 `wake::uninstall()` 撤掉 source/timer，再 `postEvent_atStart` 一个 `ApplicationDefined` 空事件，最后 `NSApp.stop` + `CFRunLoop::stop`/`wake_up`。HUD 开/关与 Esc 关窗均实测退出 |
| **V-114（已解决）** | **60fps 连续帧下 AX 查询延迟** | 原限制：`nextDrawable` 在主线程，HUD/动画 60fps 时 AX 树查询可超 20s。F-164 把 drawable 获取/编码移到 render 线程；实测 HUD 开启全屏下 74 行 AX 树 0.20s，HUD 关闭完整树仍约 0.12s。保留编号作为历史 |
| **V-115** | **直接写 `AXFullScreen` 属性的全屏路径会残留窗口** | `AXFullScreenButton` 正常路径进出全屏无残留（1680×1050 → 恢复 960×740）；直接 `AXUIElementSetAttributeValue(AXFullScreen)` 退出后留下 0,0 1680×1050 的无标题同进程窗口，数秒不消失。demo 隐藏 Zoom 按钮（F-127），正常 UI 不暴露该路径；按正常按钮路径验收，不做自建全屏 |
| **V-116** | **Sogou 候选窗后的按键焦点未定位** | 早期测试中 Space 上屏「你好」后后续按键不再进入 demo，直到点击输入框；引擎焦点仍在输入框、AXFocusedUIElement 为空。干净 Apple Pinyin 会话复测上屏后直接输入 `shi` 能继续组合，未复现；判定为第三方 IME/系统弹窗抢 key window 的夹具问题，不改引擎。真实 VoiceOver 联动未验 |
| **V-117** | **DPI / 显示器覆盖边界** | 只验单内建 Retina：1680×1050@2 ↔ 1650×1050@1 ↔ 恢复，收到 `DpiChanged 192/96`；无外接显示器、无混合 DPI、无多显示器。`NSRunningApplication.hide()` 的助手返回 false，App 隐藏路径未形成可靠实机结论 |
| **V-118** | **macOS 15 截图授权弹窗是环境问题，不是引擎 BUG** | root 身份调用 `screencapture` 会触发 ScreenCaptureKit「绕过窗口选择器」确认；以 console 用户运行不再弹窗。已把 `com.googlecode.iterm2` 的 `ScreenCaptureApprovals.plist` 审批日期设到 2126（用户要求消除反复弹窗），但推荐做法仍是用户身份截图。详见 `macos-verification.md` §4.8 |
| **F-163** | **DPI scale 必须随帧 job 传给 render 线程** | `Swapchain` 构造时缓存 scale；旧实现依赖主线程 `NativeWindow::update_layer_scale` 写图层，下一次 resize 读回。窗口渲染移到 render 线程后主线程不能再写 `CAMetalLayer` 属性。现在 scale 随 `FrameJob` 传给 worker，`Swapchain::resize(size, scale)` 同时写 `contentsScale`/`drawableSize`，`GpuRenderer::prepare_frame` 检测 scale 变化并强制全量 |
| **V-119（已解决）** | **HUD 连续帧开启时全屏转场卡在快照** | 原限制：HUD 开启（连续 60fps）时全屏停在 AXUnknown 旧快照。F-164 后实测 AXFullScreenButton 进出全屏正常（1680×1050 ↔ 960×740，AXStandardWindow/子节点齐全，无残留），HUD 显示 58–60fps；保留编号作为历史 |
