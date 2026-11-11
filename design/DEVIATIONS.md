# 设计与实施偏差记录（DEVIATIONS）

> 记录「设计文档 / AGENTS 规则」与「实际可落地实现」之间的全部偏差。
> 每条含：**设计原文 → 偏差 → 原因 → 修订后设计**。新增偏差必须追加，不得改写历史条目。
> 关联文档：`custom-gui-engine-design.md`（v2.0）、`AGENTS.md`、`design/phase-01-foundation.md`。

---

## 一、项目级授权

### A-01 单文件规模上限放宽

| 项 | 内容 |
|----|------|
| 原文 | `AGENTS.md` §1.3「避免出现包含数千甚至上万行代码的巨型文件。当文件过大时，按功能拆分模块……」 |
| 授权 | 用户于 **2026-11-12** 明确放宽：**实现阶段允许单个 `.rs` 文件超过 AGENTS §1.3 建议的规模（数千行）**，不要求为了行数强行拆分。 |
| 适用范围 | 新引擎 `crates/kytrix_gui` 的全部 Phase。 |
| 仍然有效 | **仅放宽行数**。模块职责边界、关注点分离、`lib.rs` 只做模块声明（§5.5）、禁止 `#[path]`、禁止 `pub use`、
测试与被测代码同模块、强类型错误、依赖只在 workspace 根声明等规则**全部不变**。 |
| 实施口径 | 文件拆分以「模块职责边界」为准：平台后端（`render/{windows,macos,linux}/`）、渲染器、文本等天然模块仍需分目录；
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

| 项 | 内容 |
|----|------|
| 设计原文 | 设计文档 §1.2 与附录 A：`#[cfg(target_os = "windows")] #[path = "windows/mod.rs"] mod backend;`，三份 `#[path]` 之后 `pub use backend::{Device, Swapchain, …}` |
| 偏差 | 不使用 `#[path]`，改为**标准模块 + 私有别名** |
| 原因 | `AGENTS.md` §1.3 明确「不使用 `#[path = "..."]` 引入模块，使用 Rust 标准模块布局」；`#[path]` 还会让 `mod backend` 与真实目录脱钩，破坏 `cargo doc` 路径与 IDE 导航 |
| 修订后设计 | 每个平台目录是普通子模块（`render/windows/mod.rs`、`render/macos/mod.rs`、`render/linux/mod.rs`），各自导出**同名具体类型**；`render/mod.rs` 中<br>`#[cfg(target_os = "windows")] mod windows;` …<br>`#[cfg(target_os = "windows")] use windows as backend;` …<br>共享层代码 `use super::backend::{Device, …};`。`platform/` 同构（别名 `native`）。**仍然满足 I1/I14**：编译期单一实现、无 trait、无枚举、无 `dyn` |

### M-02 `pub use` 被 workspace lint 禁止，公共 API 只能按模块路径暴露

| 项 | 内容 |
|----|------|
| 设计原文 | 设计文档 §1.2 / 附录 A 的 `pub use backend::{Device, Swapchain, Encoder, Presenter, AtlasTexture, GpuTimerImpl};`；§1.3 的公开 API 白名单按**类型名**列举（隐含可扁平导出） |
| 偏差 | 全仓**不允许**任何 `pub use`；公共 API 的路径 = 定义它的叶子模块路径（如 `kytrix_gui::paint::style::Style`），不提供短路径别名 |
| 原因 | workspace 根 `[workspace.lints.clippy] pub_use = "forbid"`；实测 `pub use` 在 `-D warnings` 下报 `error: using pub use`（`help: move the exported item to a public module instead`）。该规则的意图是「类型只在定义它的模块里被看见」，与 AGENTS §1.3 的模块化一致 |
| 修订后设计 | ①`lib.rs` 只有 `pub mod` / `mod`；②每个公开类型只在其叶子文件定义，应用层按模块路径 `use`；③完整路径表见 `phase-01-foundation.md` §4.5（实现与迁移时以此为准）；④`render/` 与 `platform/` 整体为**私有模块**（I9：应用层不得见 `Device`/`HWND`/`NSView`/`wl_surface`）；⑤`pub(crate) use` 虽未被 lint 拦截，但本项目**不使用**（`super::backend` 已足够，避免第二套别名） |

### M-03 `definition_in_module_root` 禁止在 `mod.rs` 定义任何 item，共享层代码必须放叶子文件

| 项 | 内容 |
|----|------|
| 设计原文 | 设计文档 §1.2：`render/mod.rs` = 「共享契约 + cfg 重导出具体后端类型」；即共享契约的 `struct`/`const` 会写在 `mod.rs` |
| 偏差 | `mod.rs` / `lib.rs` **只允许** `mod` 声明与 `#[cfg] use … as …;`；任何 `struct`/`enum`/`trait`/`impl`/`fn`/`const`/`static`/`type`（含 §2.3 的 `const _: () = assert!(…)` 静态断言）必须放叶子文件 |
| 原因 | workspace lint `definition_in_module_root = "forbid"`；实测 `mod.rs` 里的 `struct` 与 `impl` 各报一条 `error: definition in module root file`，而 `mod` 与 `use` 不报（`phase-01-foundation.md` §4.3a 有完整错误文本） |
| 修订后设计 | `render/mod.rs` 只做 cfg 选择 + 分配子模块；共享数据契约放 `render/{device,frame,damage,atlas,…}.rs`；静态断言放 `paint/instance.rs`（叶子文件）。同理 `node/mod.rs`、`text/mod.rs`、`platform/mod.rs`、`render/{os}/mod.rs` 都只是目录清单 |

### M-04 取消 `src/node/tests.rs` 与 `src/a11y/tests.rs`

| 项 | 内容 |
|----|------|
| 设计原文 | 设计文档 §1.2 文件树：`node/tests.rs  # 单元测与生产代码同模块`、`a11y/…/tests.rs` |
| 偏差 | 不创建独立 `tests.rs` 存放单元测试；单测直接内联在**被测算法的那个文件**末尾（`tree.rs` 的 `#[cfg(test)] mod tests`），跨模块共享装置用同目录 `test_support.rs` |
| 原因 | AGENTS §5.1 明确「不创建 crate 级别和 module 级别的独立 `tests.rs` 集中存放 unit test」；`node/tests.rs` 会让测试与具体实现文件脱钩，且无法访问其它文件的私有字段 |
| 修订后设计 | `node/id.rs` / `tree.rs` / `signal.rs` / `dependency.rs` / `dirty.rs` 各自内联 `#[cfg(test)] mod tests`；确实需要共享的建树辅助放 `node/test_support.rs`（AGENTS §5.1 允许）。`a11y` 同理 |

### M-05 `tests/` 目录形态：每个 `.rs` 是独立 target，`golden/` 只能是数据目录

| 项 | 内容 |
|----|------|
| 设计原文 | 设计文档 §1.2：`tests/layout/`、`tests/frame_record/`、`tests/golden/{warp,lavapipe,metal-real}/`、`tests/perf/` 均按目录列出 |
| 偏差 | 顶层 `.rs` 作为 test target（`tests/layout.rs`、`tests/frame_record.rs`、`tests/snapshot.rs`、`tests/perf.rs`），`tests/golden/` 与 `tests/perf/` 只作为**数据/基准输入目录**；`tests/perf` 通过 `[[bench]]` 声明为 bench target |
| 原因 | Cargo 只把 `tests/*.rs` 当作测试目标，子目录里的 `.rs` 不会被自动发现；AGENTS §5.2 也要求「`tests/` 下的每个顶层 `.rs` 文件都是独立的 integration-test target」 |
| 修订后设计 | 布局断言 → `tests/layout.rs`；CPU 帧记录 → `tests/frame_record.rs`；像素快照 → `tests/snapshot.rs`（读 `tests/golden/<device>/`）；基准 → `tests/perf.rs` + `[[bench]] harness = false`。全部只走公开 API（AGENTS §5.2） |

---

## 三、与设计文档正文的事实性偏差

> **状态说明（2026-11-12 复审追加）**：F-10（像素换算二分法）已被 §六 的 **F-13** 取代；F-10 中
> 「P05 用 `round_layout` 对齐物理网格」的表述由 **F-17** 更正；F-01 / F-02 / F-03 中出现的 taffy **0.15.1**
> 事实同样适用于 **0.15.0**（公共 API 文件逐字节相同，见 S-06）。

### F-01 taffy 0.15.1 低级 trait 与 §4.1 / §4.2 草图不符

| 项 | 内容 |
|----|------|
| 设计原文 | §4.1：`impl LayoutPartialTree for NodeTree` 只有 `get_style` / `set_unrounded_layout` / `get_cache_mut` / `compute_child_layout`；§4.2 表格称 measure 是「泛型闭包 `FnMut(LayoutInput, NodeId, Option<&mut NodeContext>, &Style) -> LayoutOutput`」 |
| 偏差 | 0.15.1 的 `LayoutPartialTree` **没有** `get_style` / `get_cache_mut`：改为 associated style 类型（`CoreContainerStyle<'a>`、`CustomIdent`）+ `get_core_container_style`，缓存走独立 `CacheTree`；flex/grid/block 的样式读取拆到 `LayoutFlexboxContainer` / `LayoutGridContainer` / `LayoutBlockContainer`；绝对定位需要 `LayoutContainingBlock`；`compute_leaf_layout(inputs, style, resolve_calc_value, measure_fn)` 是 **4 个参数**，闭包只收 `(Size<Option<f32>>, Size<AvailableSpace>) -> Size<f32>`；`compute_block_layout` 多一个 `Option<&mut BlockContext>` |
| 原因 | 设计文档的草图基于 taffy 0.13/0.14 的 API 形态；0.15 为「让宿主树的 `Style` 可以是借用的子集结构」重构了 trait |
| 修订后设计 | 精确签名全部记入 `phase-01-foundation.md` 附录 A.1；P05 直接实现真实 trait（`NodeTree` 实现 6 个 trait：`TraversePartialTree`、`LayoutPartialTree`、`LayoutContainingBlock`、`CacheTree`、`RoundTree` 及三个 `Layout*Container`）。`Style` 存储沿用 `taffy::Style<SmolStr>`，`type CoreContainerStyle<'a> = &'a Style;`（taffy 对 `&T` 有 blanket impl） |

### F-02 taffy 低级 API 不返回 `Result`，`EngineError::Layout(String)` 无生产者

| 项 | 内容 |
|----|------|
| 设计原文 | §4.8 表格：「taffy 计算错误 → `EngineError::Layout(String)`，保留子树快照供复现」 |
| 偏差 | 0.15.1 的 `compute_*` 全部返回 `LayoutOutput`（无 `Result`，只有 debug 断言）；`TaffyError` 只属于 `TaffyTree` 高阶层（本设计不用）。因此 `Layout(String)` 永远不会被构造 |
| 原因 | 低级 trait 路径把错误处理上移给宿主；引擎不维护 `TaffyTree`（I5） |
| 修订后设计 | 删除 `Layout(String)`，改为 `LayoutInvalid { node: NodeId, detail: SmolStr }`（P05 落地），只在引擎自身的**后置校验**（NaN / 负尺寸 / 逆序边界）产出；`InvalidMeasure { node, axis, value }` 保留（§4.8 原意）。两者都不再是字符串泛化错误 |

### F-03 parley 0.12 有 `TextStyle`，且 builder 参数与草图不同

| 项 | 内容 |
|----|------|
| 设计原文 | §4.5：「**Parley 0.12 无 `TextStyle`**，样式单元是 `StyleProperty<'a, B: Brush>`（26 变体）」；草图 `ranged_builder(&mut font_cx, text, scale, style.is_rtl)`；`b.push(StyleProperty::…, span)`；`push_inline_box(spec)` |
| 偏差 | ①0.12 **有** `parley::style::TextStyle<'family, 'settings, B>`（未解析样式结构体，26 个字段）；`StyleProperty` 仍存在且确实是 **26** 个变体；②`ranged_builder` 的第 5 个参数是 **`quantize: bool`**，不是 `is_rtl`；RTL 用 `builder.set_base_direction(BaseDirection::Auto/Ltr/Rtl)`；③`LayoutContext` 是泛型 `LayoutContext<B: Brush = [u8; 4]>`；④`push_inline_box` 只收 `InlineBox`（内部带 `index: usize` 字节偏移），没有 range 参数 |
| 原因 | 0.12 的 API 重构（`parley_engine` 拆分、`quantize` 显式化、方向改为 builder 状态） |
| 修订后设计 | `text::shape` 采用 `LayoutContext<ColorBrush>` + `ranged_builder(fcx, text, scale, /*quantize=*/false)`；方向用 `set_base_direction`；宽度变化只重跑 `break_all_lines` + `align`。若整段样式足够，可改用 `TextStyle` + `TreeBuilder`（P06 定，二者取一，不并存两套代码路径） |

### F-04 `Brush` 是标记 trait，`peniko` 不进入依赖图

| 项 | 内容 |
|----|------|
| 设计原文 | §4.5 与附录 A 用 `Brush` / `ColorBrush` 并列，且 §9.4 未排除 `peniko`（parley 生态常带 peniko） |
| 偏差 | `parley::style::Brush` 是**标记 trait**（`Clone + PartialEq + Default + Debug`，并对一切满足者 blanket impl）；`peniko` 只是 parley 的 dev-dependency。引擎定义 `paint::color::ColorBrush`（包装 `Color`）即可，无需 peniko |
| 原因 | 0.12 把颜色表示彻底下放给调用方 |
| 修订后设计 | 依赖清单**不含** `peniko`；公共 API 的 `Brush` 指 `paint::color::ColorBrush`；`Layout<ColorBrush>` 贯穿 shape/光栅/图集 |

### F-05 accesskit adapter 版本不同步、签名与 §7.6 草图不同

| 项 | 内容 |
|----|------|
| 设计原文 | §7.2：「`accesskit 0.25.0`，三 adapter 版本不同步，不得按 0.25.x 猜测」；§7.6 草图：`Adapter::new(hwnd, is_focused, action_handler)`、`adapter.handle_wm_getobject(wparam)`、`Adapter::new(ns_view, …)`、`accesskit_unix::Adapter::new(activation_handler, action_handler, ..)`、`update_if_active(|| update) → QueuedEvents::raise()` |
| 偏差 | 实测最新批次：`accesskit 0.25.2`、`accesskit_windows **0.35.2**`、`accesskit_macos **0.27.2**`、`accesskit_unix **0.24.1**`，三者都依赖 `accesskit ^0.25.2`（可并到同一 0.25.2）。签名差异：①Windows `handle_wm_getobject(wparam, lparam, &mut activation_handler)` 三参数；②Windows 另有 `SubclassingAdapter`（窗口首次显示前构造，否则 panic）；③macOS `Adapter::new` 是 **`unsafe fn`，收 `*mut c_void`**；④unix `update_if_active` **不返回** `QueuedEvents`（只有 Win/mac 返回），unix 的事件提升路径不同；⑤macOS adapter 内部固定 objc2 **0.5** 线，与引擎的 0.6 线共存（无 `links` 冲突） |
| 原因 | accesskit 各 adapter 独立发版；macOS adapter 仍锁 objc2 0.5；unix 侧走 AT-SPI 广播模型，没有 per-node queued events |
| 修订后设计 | 版本按上述四条冻结；`platform/*/a11y.rs` 按真实签名接线（`handle_wm_getobject` 三参数、macOS 传 `ns_view.as_ptr().cast()`、unix 单独一条 update 路径）；`A11yBridge` 的平台无关部分只依赖 `accesskit::{TreeUpdate, TreeInfo, TreeId, NodeId, ActionRequest}`，`raise` 抽象为 `platform::native::A11yEvents`（P14 内部类型，不是共享层 trait 分派） |

### F-06 `windows 0.62.2` 需要两个设计文档未列的 feature

| 项 | 内容 |
|----|------|
| 设计原文 | §9.4 窗口/事件：「`windows 0.62`（Win32_UI_WindowsAndMessaging / Ime / Dwm）」；§2.2 只提到 Direct3D12/DXGI 模块名 |
| 偏差 | 还需 **`Win32_Graphics_Dxgi_Common`**（`CreateGraphicsPipelineState` 等方法被 `#[cfg(feature = "Win32_Graphics_Dxgi_Common")]` 门控；`DXGI_FORMAT` 也在其中）与 **`Win32_UI_Controls`**（`DwmExtendFrameIntoClientArea` 的参数 `MARGINS` 定义在 `Win32::UI::Controls`） |
| 原因 | windows-rs 按 Windows SDK 头文件分 feature，`MARGINS` 与 `DXGI_FORMAT` 分属不同头 |
| 修订后设计 | `crates/kytrix_gui/Cargo.toml` 的 windows features 采用 `phase-01-foundation.md` §5.2 的 26 项清单（含新增两项）；不需要 `Win32_Graphics_DirectWrite`（D17 统一走 Fontique） |

### F-07 错误文案语言：引擎英文 + 结构化字段，应用层本地化

| 项 | 内容 |
|----|------|
| 设计原文 | §2.9 的 `thiserror` 消息为中文（「GPU 基线不满足：{os} 需要 {required}，实际 {actual}……」），并要求「列出实际检测值与最低要求并指向发布说明；不出现『正在尝试软件渲染』这类降级措辞」 |
| 偏差 | 引擎 `Display` 改为**英文诊断文本**，中文用户文案由应用层按变体生成 |
| 原因 | ①§1.3 明确 i18n 留应用层、引擎不持有 locale；②`Display` 的消费方是日志 / CI / 崩溃报告 / 开发者，不是终端用户；③D8 的硬要求是**信息完整性与不降级**，与语言无关；④同一份错误在三种语言环境下的日志应当可直接比对 |
| 修订后设计 | `EngineError` 的 `Display` 为英文完整句（含 `os`/`required`/`actual`/`release notes`，且**不出现** `software rendering` / `falling back` / `try again`，由单测守护）；应用层（`base_ui`/安装器）对 `UnsupportedGpu` 等变体做穷尽 match，用同一组字段生成 zh-CN 文案；安装器与运行时的基线清单要求**单一来源**（P09 导出 `*_BASELINE` 常量）。`DEVIATIONS` 记录该决策，设计文档 §2.9 的中文示例视为「语义示例」，不视为文案要求 |

### F-08 短字符串 `String → SmolStr`，元组变体 `Swapchain(String) → Swapchain { detail: SmolStr }`

| 项 | 内容 |
|----|------|
| 设计原文 | §2.9：`DeviceLost { reason: String, … }`、`WaylandCompositor { detail: String }`、`Swapchain(String)`、`UnsupportedGpu { …, actual: String }` |
| 偏差 | 五个短文本字段全部改 `SmolStr`；`Swapchain` 由元组变体改为具名字段变体 |
| 原因 | AGENTS §2.2「短字符串优先 `SmolStr`」；具名字段让错误在 match 时可读（`EngineError::Swapchain { detail }` 优于 `.0`），也便于未来增字段 |
| 修订后设计 | 见 `phase-01-foundation.md` §6.2.2 的完整签名；`PathBuf` 保持（路径不是短字符串） |

### F-09 `DamageRect` 不另定义结构，复用 `math::PhysicalRect`

| 项 | 内容 |
|----|------|
| 设计原文 | §2.3 共享数据契约：`DamageRect = { x, y, w, h }` 物理像素整数矩形，定义在 `render/damage.rs` |
| 偏差 | 整数物理矩形只有一份定义：`kytrix_gui::math::PhysicalRect { x0, y0, x1, y1 }`（半开区间）；`render/damage.rs` 只提供 **damage 语义**（外扩、合并、退化阈值、`Vec<PhysicalRect>` 的构建），不重复定义矩形类型 |
| 原因 | I2「一套数据契约、只有一份定义」；两份 `{x,y,w,h}` 只会在换算处产生语义漂移（开/闭区间、负坐标） |
| 修订后设计 | `render/damage` 的公开操作签名使用 `PhysicalRect`（P09 落地）；`contract.md` 仍以「物理像素整数矩形」描述，不绑定字段名。若 P09 发现需要额外不变量（如「damage 非空」），用 `pub struct DamageRect(PhysicalRect)` newtype 在 P09 决定并补记偏差 |

### F-10 像素换算从「统一乘 scale 并 round」细化为「逐边取整」+「向外覆盖」两个函数

| 项 | 内容 |
|----|------|
| 设计原文 | §4.4：「布局与 `LayoutBox` 全用逻辑像素 `f32`；在 paint 生成 `GpuPrimitive.bounds` 时统一乘 `scale` 并 `round`，AA 外扩按物理像素整数计算」 |
| 偏差 | 提供两个语义不同的换算：`Rect::to_physical`（**逐边** `round`，不透明填充/clip 用）与 `Rect::to_physical_covering`（`floor(min)`/`ceil(max)`，AA / 字形 / damage 用）；并把「先加后乘」写成语义的一部分 |
| 原因 | 若按「origin/size 各自 round」，相邻图元的共享边会各自取整，产生 1px 缝或 1px 重叠（半透明时双次混合）；逐边取整保证同一逻辑坐标映射到同一整数。damage 必须走「向外覆盖」才对安全侧（少覆盖 = 残留脏像素） |
| 修订后设计 | 见 `phase-01-foundation.md` §7（含 s=1/1.25/1.5/2/3 的精确语义表与无缝证明）；P05 追加 `taffy::round_layout` 把最终布局盒圆整到物理网格，P09 的 damage 只用 `covering` |

### F-11 构建期 SVG（`usvg`）归属应用层，不进引擎运行时依赖

| 项 | 内容 |
|----|------|
| 设计原文 | §1.2 文件树：`resources/svg_build.rs  # SVG 仅构建期` 位于引擎 crate 内 |
| 偏差 | `usvg` 只作为 **build-dependency** 出现在资源所在的 `base_ui`（P15/P19），引擎 crate 不声明 `usvg`；引擎只提供「已解码字节 → 图集」的机制 |
| 原因 | §1.3 明确「资源文件在应用层」「引擎只提供资源字节 → GPU 图集的机制」；54 个 Phosphor SVG 属于 `base_ui` 资产，转换脚本应与其资产同处一地 |
| 修订后设计 | workspace 根声明 `usvg` 版本（冻结），由 `base_ui` 的 `build.rs` 使用；`crates/kytrix_gui` 不声明。若 P15 确需引擎侧预处理，再改为引擎 build-dependency 并补记 |

### F-12 `accesskit_unix` 使用 `tokio` feature

| 项 | 内容 |
|----|------|
| 设计原文 | §7.2：「（`accesskit_unix`）内部 spawn 异步任务，tokio 需开 `tokio` feature」——方向正确但未定 feature 组合 |
| 偏差 | 声明为 `accesskit_unix = { version = "0.24.1", default-features = false, features = ["tokio"] }`，关闭默认 `async-io` |
| 原因 | D21 全仓单一 tokio runtime；`accesskit_unix` 的默认 `async-io` 会再拉 `async-executor`/`async-task`（设计文档 §9.2 允许库内 executor，但既然官方提供 tokio 集成就优先选它） |
| 修订后设计 | 如 P14 实测 `tokio` feature 与 `atspi 0.29`/`zbus 5.19` 组合有问题，回退默认 `async-io` 并补记（库内 executor 不违反 D21） |

---

## 四、设计补充决策（不与设计文档冲突，但需固化）

### S-01 `EngineError` 变体分批落地，且不加 `#[non_exhaustive]`

- **决策**：P01 落地 6 个变体（D8 五类 + `InvalidDpiScale`）；`StaleNode` / `CyclicDerived` / `InvalidMeasure` /
  `LayoutInvalid` / `TextShape` 在各自 Phase 追加。整份「变体名 + payload」表在 P01 冻结为契约。
- **理由**：错误**分类法**必须一次定死（否则各 Phase 各造同义变体）；但 payload 不能猜（依赖 `NodeId`/`SignalId`/`Axis`）。
  不加 `#[non_exhaustive]`：`publish = false` 且应用层同仓演进，穷尽 match 比向后兼容更有价值。

### S-02 Phase 01 一次性在 `kytrix_gui` 声明全部 target 依赖

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

| # | 项 | 复核 Phase | 触发条件 |
|---|----|-----------|----------|
| V-01 | objc2 双版本（0.6.5 + 0.5.3）在真机的运行时行为与二进制体积 | P16 | macOS 实机 UIA/NSAccessibility 联调；若 accesskit_macos 升级到 objc2 0.6 线则删除 0.2 线 |
| V-02 | `usvg 0.49` 在 `default-features = false` 下解析 path-only SVG | P15 | 若 `Tree::from_str` 依赖被裁掉的 feature，则回退默认 features |
| V-03 | `accesskit_unix` 的 `tokio` feature 与 `atspi`/`zbus` 组合 | P14 | Linux 首次构建 AT-SPI adapter |
| V-04 | Linux 系统库：`xkbcommon 0.9` 链接期需 `libxkbcommon`（无 build.rs，`#[link]` 直链）；`fontique` 传递的 `yeslogic-fontconfig-sys` 在 build script 中调 `pkg-config` 找 `fontconfig` | P17 | Linux CI 固化 `libxkbcommon-dev` + `libfontconfig-dev`；Windows 宿主交叉 `cargo check` 需 sysroot pkg-config，或用 stub `pkg-config`（返回 `-lfontconfig`）+ `PKG_CONFIG_ALLOW_CROSS=1`：2026-11-12 复验 **exit 0**（`Checking kytrix_gui` 成功）；缺库失败点在第三方 build script，不是引擎代码 |
| V-05 | 是否需要 `Win32_Graphics_DirectWrite` 做系统度量对齐 | P06 | Fontique 度量与平台原生出现可见偏差时（§2.2「可选」项） |
| V-06 | `windows` 26 项 feature 是否全部必要（可再收窄） | P11/P12 | 平台层落地后按实际 `use` 反查，删除未用 feature |

---

## 六、审查响应追加条目（2026-11-12 复审）

2026-11-12 对抗审查对 v1 设计给出 **FAIL**（3 条阻塞 B1–B3 + N1–N17）。本节为复审后的**新条目**，
其中 M-06 / M-07 / M-09 / F-13 / F-18 / F-19 / S-06 明确取代历史条目中的过期结论（历史条目文本不改写）。
逐条响应表见 `phase-01-foundation.md` §11。

### M-06 全 crate 不使用 `mod.rs`，改自命名模块根（取代 M-01 / M-02 / M-03 中关于 `mod.rs` 的写法）

| 项 | 内容 |
|----|------|
| 前版设计 | `render/mod.rs` 做 cfg 选择、共享契约放 `mod.rs`；平台目录用 `render/windows/mod.rs` |
| 偏差 | 删除全部 `mod.rs`。含子模块的模块改用**自命名模块根**：`render.rs` + `render/`、`text.rs` + `text/`、`platform/windows.rs` + `platform/windows/`。模块根文件可定义公共类型与 `pub(crate) use` |
| 原因 | clippy `definition_in_module_root` 只针对 `mod.rs` / `lib.rs` / `main.rs`：`mod.rs` 内定义 struct/impl 会失败，而自命名根不会（实测 `%TEMP%\kg-modroot-probe`：`math.rs` 定义 `pub struct Point` + `impl`、`render/windows.rs` 写 `pub(crate) mod device; pub(crate) use device::Device;`、`render.rs` 写 `pub(crate) use windows as backend;`、`render/shared.rs` 写 `use super::backend::Device;`，`cargo +nightly clippy --all-targets -- -D warnings` **退出 0**）。该布局还让公共类型能留在设计文档指定的模块根，避免 v1 的「所有公开类型都在深层叶子文件」问题 |
| 修订后设计 | ①`lib.rs` 仍只有 `mod`/`pub mod`；②平台出口：`render.rs` 内 `#[cfg] pub(crate) mod windows;` + `#[cfg] pub(crate) use windows as backend;`，`render/windows.rs` 内 `pub(crate) mod device;` + `pub(crate) use device::Device;`，共享层 `use super::backend::Device;`；③`platform/` 同构（别名 `native`）；④`loop.rs` 改名 `event_loop.rs`（关键字）；⑤文件树见 `phase-01-foundation.md` §3 |

### M-07 内部出口允许 `pub(crate) use`，公共 API 仍禁止 `pub use`（取代 M-02 第 ④ 点）

| 项 | 内容 |
|----|------|
| 前版设计 | 「`pub(crate) use` 虽未被 lint 拦截，但本项目**不使用**」 |
| 偏差 | 内部出口**使用** `pub(crate) use`（平台类型出口、模块内类型出口）；公共 API 依然只能靠模块路径，不允许任何 `pub use` |
| 原因 | 平台出口必须有一个 crate 内可见的名字，否则共享层无法在不带平台名的前提下引用具体类型；`clippy::pub_use` 只拦截 `pub use`（实测 `pub(crate) use` 不报），这是 lint 允许的唯一出口手段 |
| 修订后设计 | 规则改为：**公共 API 禁止 `pub use`；crate 内部出口允许 `pub(crate) use`**。公共路径表见 `phase-01-foundation.md` §4.5（`text::TextStyle`、`test_support::TestApp` 等模块根路径成立） |

### M-08 `clippy.toml` 的 `disallowed-methods` 属应用层 M8 范围（N16）

| 项 | 内容 |
|----|------|
| 原文 | 设计文档 §9.2：`clippy.toml` 的 `disallowed-methods`（禁 `spawn_blocking` / `Handle::current`）改为一条新约定 |
| 偏差 | 本次引擎实现**不修改** `clippy.toml`（它当前指向即将被删除的 `widget_tokio_runtime`，属应用层 M8 清理范围） |
| 原因 | 引擎 crate 在 P08 之前不引入 tokio 使用点；提前改根 `clippy.toml` 会影响现有 30+ crate 的构建（它们仍依赖 `widget_tokio_runtime`），违反「不做兼容层 / 不提前改无关代码」 |
| 修订后设计 | 引擎 P08+ **禁止**使用 `tokio::task::spawn_blocking` 与 `tokio::runtime::Handle::current`（主线程不在 runtime 上下文，直接 panic）；确需阻塞任务时，在 **M8** 与删除 `widget_tokio_runtime` 同批改写 `clippy.toml` 的 `disallowed-methods` 并补记 |

### M-09 `tests/` 与 `benches/` 的 target 形态（取代 M-05 中 perf 部分，N13）

| 项 | 内容 |
|----|------|
| 前版设计 | `tests/perf/` 目录 + 「`tests/perf.rs` + `[[bench]]`」二义表述 |
| 偏差 | 集成测试：`tests/layout.rs`（P05）、`tests/frame_record.rs`（P07）、**`tests/snapshot.rs`（P18）**；基准：**`benches/perf.rs`**（`[[bench]] harness = false`）；`tests/golden/` 仅为数据目录 |
| 原因 | Cargo 不会把 `tests/` 子目录里的 `.rs` 当 target；同一文件不能既当 test target 又当 bench harness（`harness=false` 时 `#[test]` 不生效） |
| 修订后设计 | 见 `phase-01-foundation.md` §3；`cargo bench -p kytrix_gui --bench perf` 为 P18 的 L4 入口 |

### F-13 AA 契约：128 B 不变 + 顶点着色器外扩 + 三种换算语义（取代 F-10 的二分法，B3）

| 项 | 内容 |
|----|------|
| 前版设计 | F-10：只有 `to_physical`（逐边 round）与 `to_physical_covering`（floor/ceil）两种；SDF 图元用哪种未定，曾隐含「AA 用 covering 的整数 bounds」 |
| 偏差 | 三种换算：`to_physical_f32`（未取整浮点，SDF 图元 `bounds`）、`to_physical`（逐边 round，图集图元四边形）、`to_physical_covering`（floor/ceil，damage 与 scissor）。AA 外扩**不进 bounds**，由顶点着色器按 `params0.w = aa_padding` 外扩（普通 1.0 / Shadow `blur+1.0` / 图集 0）；片元用 `bounds` 的真实矩形求 SDF；`GpuPrimitive` 保持 128 B、零新增字段 |
| 原因 | 用整数 `covering` bounds 会把 SDF 整体平移最多半像素、使 bounds 与真实几何不一致（动画闪烁、圆角/边框偏移）；而未取整 bounds + VS 外扩既保留亚像素精度，又保证 AA 边缘不被裁掉，且不破坏 §2.3 的 128 B 字节契约 |
| 修订后设计 | `phase-01-foundation.md` §7.1/§7.2；P10 的 `render/shaders/contract.md` 必须写入「bounds 浮点 + `params0.w` 外扩表 + clip mask 用浮点 `clip_rect` + scissor 用 covering」；P18 用像素快照验证 AA 边缘。**字段语义变更**：主设计文档 §2.3/附录 B 把 `params0.w` 定义为 `aa_scale`（AA 缩放系数），本设计改称 **`aa_padding`（物理像素外扩量）**；主设计文档未改，P10 以 `contract.md` 为准，禁止两处混用 |

### F-14 `Transform` 的 GPU uniform 契约（N17）

| 项 | 内容 |
|----|------|
| 原文 | 设计文档 §附录 A 只给 `Transform` 类型名，无 uniform 布局 |
| 偏差 | 冻结为「2D 仿射 + 两个 `vec4`」：`(m11, m12, m21, m22)` 与 `(tx, ty, 0, 0)`，6 个有效 float + 2 padding；`to_array()` 前 6 项与之一致 |
| 原因 | `Geometry` 每帧只更新 uniform（§5.5），布局必须在 P01 定死，否则 P09/P10 会各写一套；两个 `vec4` 满足 16 B 对齐且无需矩阵库 |
| 修订后设计 | `phase-01-foundation.md` §6.3.2；P10 `contract.md` 镜像该布局；`Transform` 仍只支持 2D（无 3D / 无投影） |

### F-15 `EngineError` 完整冻结变体表（N11）

| 项 | 内容 |
|----|------|
| 原文 | 设计文档 §2.9 只给 D8 五类；§4.8 给 `InvalidMeasure` / `Layout(String)` |
| 偏差 | 冻结 **17 个变体**（名称 + payload + 实现 Phase）：D8 五类 + `InvalidDpiScale` 在 P01；`StaleNode`/`CyclicDerived`（P04）、`InvalidMeasure`/`LayoutInvalid`（P05）、`GeometryRegister`（P03）、`AtlasExhausted`（P09）、`TextShape`（P06）、`AppInit`（P08）、`WindowCreate`（P12）、`ResourceDecode`（P15）、`A11yInit`（P14）后续追加 |
| 原因 | 变体名与 payload 是公开契约，必须一次定死（否则各 Phase 造同义变体）；但 payload 依赖尚未定义的类型（`NodeId`/`AtlasKind`/`ResourceId`），P01 只能实现不依赖它们的 6 个 |
| 修订后设计 | 表见 `phase-01-foundation.md` §6.2.2；`EngineError` 不加 `#[non_exhaustive]`（`publish=false`，穷尽 match 优先） |

### F-16 N15 四项（accesskit 方法集、`dpi.rs`、`HitTest`→`HitResult`、像素换算归属）

| # | 设计原文 | 偏差 | 原因 | 修订后设计 |
|---|----------|------|------|------------|
| ① | §7.6 `adapter.register_hit_test(|| engine.hit_test(...))` | `accesskit_macos 0.27.2` **没有** `register_hit_test`；只有 `view_children` / `focus` / `hit_test(point: NSPoint, handler)` | 0.27 把命中测试改成引擎在适配器回调里主动调用，而非注册闭包 | P16 在 `platform/macos/a11y.rs` 里实现 `hit_test` 回调路径，并把引擎 `HitResult` 转成 `accesskit` 节点；不存在的 API 不得写进实现清单 |
| ② | §8.1 平台目录 `windows/{loop,window,ime,clipboard,a11y}.rs` | 增加 `platform/windows/dpi.rs` | DPI 事件（`WM_DPICHANGED` / `WM_GETDPISCALEDSIZE`）与窗口创建强耦合但职责独立；`Scale::new` 失败要落 `InvalidDpiScale` | 目录见 `phase-01-foundation.md` §3；`dpi.rs` 是 `EngineError::InvalidDpiScale` 的唯一生产者 |
| ③ | §1.2/§1.3 用 `HitTest` 指代命中结果类型 | 统一为 **`HitResult`**（`event::HitResult`）；`hit_test` 保留为函数名 | 设计文档两处命名不一致（§1.2 `HitTest` 与 §1.3 `HitResult`），类型名必须是名词 | `event::HitResult { node, local_point, clip_chain }`；`hit_test(Point) -> Option<HitResult>` |
| ④ | §1.2 `layout/mod.rs # 逻辑/物理像素换算` | 像素换算全部移入 `math`（`to_physical_f32` / `to_physical` / `to_physical_covering` / `Scale::to_logical`）；`layout` 只管布局与命中 | 换算被 paint/render/platform 三处消费，放 `layout` 会造成反向依赖（`paint` → `layout` 仅为换算） | `math` 是唯一换算点；`layout` 不再导出换算 API |

### F-17 v1 **不调用** taffy `round_layout`（更正 F-10 中的错误表述，N6）

| 项 | 内容 |
|----|------|
| 前版设计 | F-10 末尾写「P05 追加 `taffy::round_layout` 把最终布局盒圆整到物理网格」 |
| 偏差 | **删除该表述**。本设计不调用 `round_layout`（引擎 v1 产品同样不调用）；布局盒保持逻辑 `f32`，无缝由 `Rect::to_physical` 的逐边取整保证 |
| 原因 | taffy 的 `round_layout`（`compute/mod.rs` `round_layout(tree: &mut impl RoundTree, node_id)`）圆整到 **1.0 逻辑单位**，在 1.25/1.5 等分数缩放下会损失亚像素、使 1px 边框错位；它解决的是「整数缩放下的布局盒对齐」，不是「物理网格对齐」 |
| 修订后设计 | `phase-01-foundation.md` §7.5；若 P05 实测出现布局级 0.5 px 抖动，再评估「按物理网格自定义圆整」并补记偏差（新 V 项） |

### F-18 `Win32_Graphics_DirectWrite` 由 fontique 的 `system` feature 传递引入（N7，取代 V-05 的疑问）

| 项 | 内容 |
|----|------|
| 原文 | §2.2 把 DirectWrite 列为「可选」；V-05 待定是否声明 `Win32_Graphics_DirectWrite` |
| 偏差 | 引擎**不声明**该 feature：`fontique 0.12.0` 的 `system` feature（`default` 即含）在 Windows 上依赖 `windows 0.62.2` + `features = ["Win32_Graphics_DirectWrite"]`（optional、target windows；源码 `fontique-0.12.0/Cargo.toml` 的 `[target.'cfg(target_os = "windows")'.dependencies.windows]`），Cargo feature 统一后自动可用 |
| 原因 | 声明会与传递 feature 重复，且让引擎 manifest 误以为直接使用 DirectWrite；D17 的字体枚举由 Fontique 负责 |
| 修订后设计 | `phase-01-foundation.md` §5.2 说明 3 / §5.3⑥；V-05 关闭（不再需要决策） |

### F-19 `harfrust` / `icu_*` 不作 workspace 声明（N1）

| 项 | 内容 |
|----|------|
| 前版设计 | v1 把 `harfrust` / `icu_properties` / `icu_segmenter` 写进 `[workspace.dependencies]` 并称「传递钉住」 |
| 偏差 | **删除这三条**。`[workspace.dependencies]` 中未被任何成员引用的条目不参与解析，写了既不能钉版本也不能验证兼容 |
| 原因 | 三者都是 `parley_engine 0.12` 的传递依赖；引擎没有直接 `use`，声明属「无实际需求的配置」（AGENTS §1.2）。且仓库 `boa_parser 0.22` 约束 icu `~2.3`，引擎若写 `2.4.0` 会制造版本冲突 |
| 修订后设计 | P06 若确需直接 `use`，再按**仓库实际解析版本**追加（icu 预计 2.3.x），并同步附录 B 与本文档；在此之前不在清单中 |

### F-20 `Rect::is_empty` / `Rect::intersect` 对非有限输入的补充契约（P01 实现阶段）

| 项 | 内容 |
|----|------|
| 原文 / 前版 | §6.3.2 只写「`is_empty` 同 `Size::is_empty`」，未定义原点非有限的情形；`intersect` 未定义非有限输入 |
| 偏差 | `Rect::is_empty` 增加「原点非有限 → `true`」；`Rect::intersect`（以及依赖它的 `intersects`）对任一不可换算输入（非有限或负尺寸）直接返回空矩形 |
| 原因 | NaN / ±∞ 只可能来自平台 bug 或上游坏数据，但必须**安全失败**：`f32::min/max` 会忽略 NaN 操作数，若不拦截，`intersect` 可能返回「看似相交且非空」的矩形，让命中测试与 damage 误判。换算路径本已由 `is_convertible` 保护，此条目把同一判定提升到几何谓词层 |
| 修订后设计 | `phase-01-foundation.md` §6.3.2 / §6.3.3；单测 `rect_is_empty`（NaN 原点）与 `rect_intersect_disjoint_is_empty` 覆盖 |

### F-21 数值安全的两处补充：`Size::is_empty` 措辞与 `to_physical_f32` 差值溢出（P01 复审修复）

| 项 | 内容 |
|----|------|
| 原文 / 前版 | §6.3.2 写 `Size::is_empty` 的公式为 `!(width > 0.0 && height > 0.0)`；`to_physical_f32` 只校验输入端点有限，未校验 `x1 − x0` / `y1 − y0` |
| 偏差 | ①判空语义统一表述为「**非有限或 `<= 0` 即空**」（NaN / ±∞ 都算空），与 §6.3.3 / F-20 一致；②`to_physical_f32` 在端点有限之外，再校验差值有限，溢出的 `±inf` 一律返回 `[0.0; 4]` |
| 原因 | ①原公式对 `±∞` 的表述不直观（尽管 `∞ > 0` 为真时靠乘积仍可能得空），文档与实现必须逐字一致；②两个有限端点相减仍可溢出（如 `-9.191715e37` 到 `f32::MAX` → `+inf`），若直接返回该 `w` 会污染 damage / SDF 外扩 |
| 修订后设计 | `phase-01-foundation.md` §6.3.2 / §8.1 测试 26–28、32；实现见 `math.rs::to_physical_f32` 与 `Size::is_empty` |

### F-22 `Edges::new` 参数顺序为 CSS 顺序（与复审用例字面值的差异说明）

| 项 | 内容 |
|----|------|
| 原文 | 设计 §6.3.2 明确 `Edges { top, right, bottom, left }`（CSS `padding: T R B L`），`Edges::new(top, right, bottom, left)` |
| 偏差 | 复审给出的用例 `inset(Edges::new(1,2,3,4)) == Rect::new(1,2,6,4)` 若按 CSS 顺序实际得到 `Rect::new(4,1,4,6)`；`(1,2,6,4)` 对应 `left=1, top=2, right=3, bottom=4` |
| 原因 | 复用 CSS 顺序可避免与 `base_ui` 既有 padding 语义错位；顺序本身是契约，不能为了单个用例反转 |
| 修订后设计 | 保留 CSS 顺序；单测 `rect_inset_asymmetric_edges` 同时断言两种几何（`Edges::new(1,2,3,4) → (4,1,4,6)` 与 `Edges::new(2,3,4,1) → (1,2,6,4)`），并在注释中写明顺序映射 |

### S-07 `Transform` 的 v1 能力边界：只支持 translate / scale 复合（P03 前瞻决策）

- **决策**：P01 的 `Transform` 只提供 `IDENTITY` / `translate` / `scale` / `then` / `apply` / `to_array`；
  **不提供** rotation / skew 的构造器，也不暴露字段。
- **依据**：v1 的 GPU 侧变换只有 pan / zoom（`Geometry` 静态 VBO 每帧只更新 uniform，§5.5），
  旋转会让 `bounds` 与 damage 的 AABB 语义复杂化；且 `f32` 2D 仿射已能表达 pan/zoom。
- **失败模式与退路**：字段私有 + 无构造器 = 调用方不可能传入旋转矩阵；若 P03 确认相机/地图需要旋转，
  再新增 `Transform::from_array([f32; 6])` 与旋转构造器（**additive**，不改变既有签名，也不返工 P01）；
  届时 uniform 契约（两个 vec4）不变。

### S-06 taffy 冻结 `=0.15.0`（B1）

- **决策**：workspace 声明 `taffy = { version = "=0.15.0", … }`（精确钉），全文档（§5.1/§5.3④/§5.4/§9/附录 A/B）统一为 0.15.0。
- **证据**：`gpui-fast 0.1.14` 声明 `taffy =0.15.0`（sparse index 原文），仓库 `Cargo.lock` 为 0.15.0；probe `%TEMP%\kg-taffy-dup`
  验证 `=0.15.0` 与 `^0.15.1` **无解**（`all possible versions conflict with previously selected packages`）。
  `taffy 0.15.0` 与 `0.15.1` 的公共 API 文件 SHA256 **逐字节相同**（`lib.rs`、`tree/traits.rs`、`tree/layout.rs`、
  `tree/cache.rs`、`style/mod.rs`、`compute/mod.rs`、`prelude.rs`、`tree/mod.rs`），52 个源文件中 14 个内部实现文件有差异
  （算法修复），附录 A.1 的签名对 0.15.0 成立。
- **升级路径**：M8 移除 `gpui-*` 后把 `=0.15.0` 放开为 `0.15.x` 并复核（V-07）。

### 复审新增/修订的待复核项

| # | 项 | 复核 Phase | 触发条件 |
|---|----|-----------|----------|
| V-07 | taffy 从 `=0.15.0` 放开到 `0.15.x` | M8 | `gpui-*` 全族移除后；需重跑 §9-5 门禁与布局测试 |
| V-08 | `to_physical_f32` + VS 外扩的 AA 视觉效果 | P10 / P18 | 像素快照出现边缘裁切或圆角偏移时，调整 `aa_padding` 取值表 |
| V-09 | 「按物理网格自定义布局圆整」是否需要 | P05 | 实测出现布局级 0.5 px 抖动/边框错位（否则不引入，见 F-17） |
| V-10 | `accesskit_macos` 的 `hit_test` / `view_children` 回调接线 | P16 | macOS 实机 VoiceOver 命中测试；确认 `NSPoint` 由 `accesskit_macos::NSPoint` 构造 |

### 复审数字复核记录（N8 / N9）

| 项 | 审查报告 | 本次实测 | 说明 |
|----|----------|----------|------|
| 隔离 probe 包数 | 233 | **238** | `%TEMP%\kg-final-probe`（清单 = `phase-01-foundation.md` §5.2 并集，含 tokio/atspi/zbus/objc2 双版本），`cargo metadata` 的 `packages` 长度；差 5 个包源于 probe 范围不同，不改变「三平台一次解析通过」的结论。**已确认采用可复现的 238** |
| 仓库内联测试模块 | 276 | **277** | `crates/**/*.rs` 正则 `#\[cfg\(test\)\]\s*\n\s*mod tests` 的严格匹配数；同范围 `mod tests` 字样为 282。**已确认采用 277**（保留严格口径注释） |
| 仓库并行版本（N10） | — | 已核实 | `accesskit 0.24.1`（gpui-fast `^0.24`）+ `0.25.2`（引擎）；`skrifa 0.40.0`（现有生态）+ `0.44.0`；`harfrust 0.5.2`（`cosmic-text 0.19`）+ `0.12.0`；`objc2 0.5.3 + 0.6.5`。均为不同 semver 线的预期并存，不影响引擎自身依赖边 |

---

## 七、Phase 02 绘制基础层（2026-11-12）

> 关联文档：`design/phase-02-paint-foundation.md`（本节 = 该文档 §20 的落地条目；裁决记录见该文档 §17）。
> **项目级授权 A-01（单文件行数放宽）继续适用**；M-01…M-09、F-09/F-13/F-14/F-17、S-07 继续有效，本节不重复。
> 本节只**追加**，不改写任何历史条目（一～六 未改动）。
>
> **评审修订说明（2026-11-12，实现开始前）**：§七 经设计裁决 ①—⑤ 校准，修订点逐条标注「(裁决 ①)」等：
> - **F-25** 撤销「damage 前移到 `paint/`」的草案：damage 位置仍按 P01 §4.1 留在 `render/damage.rs`，只把落地 Phase 改为 **P06**；
> - **S-08** 的落地清单改为 `render/atlas.rs`→**P05**、`render/damage.rs`→**P06**、`paint/instance.rs`→**P07**；
> - **S-10** 改为「`Scene` 只读 + `Canvas` 是唯一公开写入口」；
> - 新增 **F-33**（`Canvas` = lane 片段模型）、**S-14**（边框+渐变拆两个 `Quad`）、**S-15**（`Scene`/`Canvas`/`Lane` 所有权契约）。
>
> **第二轮审查（FAIL）追加（2026-11-12）**：B1/B2/B3/N11 把 `Scene` 整体推迟 P03、`MAX_CLIP_DEPTH` 改为 `u8`；
> N6 让 `Canvas` 的 clip API 返回 `bool`、N7 记公开白名单闭包、N2/N3/N4/N5/N13 记数值与 damage 契约、
> B4 记文档完整性复核方法与事故。**新增 S-16…S-19**，并在 S-08/S-09/S-10/S-15、F-24 内追加「第二轮」说明。
>
> 事实基线：`color 0.3.3` / `etagere 0.3.0` / `bytemuck 1.25.2` 的真实源码 + `%TEMP%\kg-{pod-probe,pod-probe2,dead-probe,dead-probe2,size-probe}` probe，详见 phase-02 文档 §2。

### F-23 `GpuPrimitive` 必须 `#[repr(C, align(16))]`（主设计的 `#[repr(C)]` + `align_of == 16` 不成立）

| 项 | 内容 |
|----|------|
| 设计原文 | §2.3 / 附录 B：`#[repr(C)]` + `#[derive(bytemuck::Pod, …)]` 的 `GpuPrimitive`，静态断言 "size_of == 128、align_of = 16" |
| 偏差 | 结构体必须是 **`#[repr(C, align(16))]`**。仅 `#[repr(C)]` 时 `size_of == 128` 成立但 **`align_of == 4`**，附录 B 声明的 `align_of = 16` 与 P01 §7.2 要求的静态断言**无法同时成立**（断言会直接编译失败） |
| 原因 | 8 个 `[f32; 4]` 字段的自然对齐是 4；Rust 不会因为「总大小是 16 的倍数」就提升对齐。实测（`%TEMP%\kg-pod-probe`）：`error[E0080]: evaluation panicked: assertion failed: align_of::<GpuPrimitive>() == 16`。补 `align(16)` 后（`%TEMP%\kg-pod-probe2`）`size_of == 128`、`align_of == 16`、8 个 `offset_of!` 全成立、`bytemuck::Pod` derive 通过、`cargo +nightly clippy --all-targets -- -D warnings` **退出 0** |
| 修订后设计 | `paint/instance.rs`（**P07 落地**，裁决 ④）用 `#[repr(C, align(16))]`；静态断言 = size 128 + align 16 + 8 个 offset。**P10 的 `render/shaders/contract.md` 必须写「结构体 16 B 对齐」**；**三平台实例缓冲的基址也必须 16 B 对齐**（分配器/偏移计算显式保证；D3D12 常量缓冲的 256 B 要求由分段上传向上取整满足）→ phase-02 §16 R7 |

### F-24 `AtlasTile` / `AtlasKind` 归属 `paint/atlas.rs`（不在 `resources.rs`）

| 项 | 内容 |
|----|------|
| 原文 | P01 §4.5 公开路径表：`AtlasTile` → `kytrix_gui::resources::*`、`AtlasKind` → `kytrix_gui::resources::AtlasKind`，定义文件 `src/resources.rs`（P15） |
| 偏差 | 两者定义在 **`src/paint/atlas.rs`**，公开路径 **`kytrix_gui::paint::atlas::{AtlasKind, AtlasTile}`**；`resources.rs`（P15）只保留 `ResourceId` / `ResourceCache`。**P01 §4.5 正文不改**，以本条为路径表的修订记录 |
| 原因 | ①`AtlasTile` 是 `Primitive::GlyphRun`/`Image` 的**字段类型**，属 I2 的共享数据契约，必须在 `paint`；②放 `resources` 会让 `paint → resources`（P15 高层模块）反向依赖，且 P15 之前（P02/P06/P07）就要用到；③图集**分配器**仍按 brief 留在 crate 私有的 `render/atlas.rs`（P05），公开的只有只读数据 |
| 修订后设计 | phase-02 文档 §4.1/§4.2 的路径表；P15 的 `resources.rs` 不再定义这两个类型，`ResourceCache` 直接复用 `paint::atlas::AtlasTile`。**P01 `design/phase-01-foundation.md` §4.5 的 `AtlasTile` 行已按 N8 加注记**（只加注记，不改历史正文） |

### F-25 damage 仍在 `render/damage.rs`，落地 Phase 改为 **P06**（裁决 ①）

| 项 | 内容 |
|----|------|
| 原文 | §2.3 共享数据契约把 `DamageRect` 与图集/几何并列；P01 §4.1 把 `damage` 归 `src/render/damage.rs`（P09） |
| 偏差 | **位置不变**（`src/render/damage.rs`，crate 私有）；**落地 Phase 由 P09 改为 P06**（首个消费者 = paint 之后的 damage 步骤）。此前草案「前移到 `paint/damage.rs`」**撤销** |
| 原因 | ①`render` 整体私有（I9），`pub(crate)` item 需要**非 test 消费者**（`-D warnings` 与 `dead_code`，phase-02 §2.4c）；P06 的帧组装正是该消费者，因此不必移动位置；②damage 是纯 CPU 算法，与 GPU 无关，位置在 `render/` 不影响可测性（同模块单测）；③矩形类型复用 `math::PhysicalRect`（F-09 不变） |
| 修订后设计 | phase-02 文档 §12（算法逐字、阈值常量、14 个用例）；P09 的渲染后端只做「`DamageOutput` → 三平台 API 参数」的**平台翻译**（`Present1` dirty rects / `VK_KHR_incremental_present` / scroll hint），不重算区域 |

### F-26 `Primitive::Path` 删除 `triangles` / `coverage_pass`（v1 路径无 AA，裁决 ③）

| 项 | 内容 |
|----|------|
| 原文 | 附录 C：`Path{ triangles, coverage_pass, fill, clip_chain }`；§5.1「CPU 三角化后的三角形 / 两阶段 coverage 标记」 |
| 偏差 | 改为 `Path{ bounds: Rect, geometry: GeometryId, fill: Color }`：三角形走**已注册的静态几何**（§5.5 的收益项），`coverage_pass` **删除**，`clip_chain` 删除（clip 改为栈指令）。**v1 的路径是硬边三角形，不做 AA** |
| 原因 | ①三角形进每帧 `Primitive` 会让每次 lane 重建都拷一份顶点，直接抵消 §5.5「顶点零上传」；②两阶段 coverage 是渲染器的 pass 结构（同一份三角形成对出现），放进每帧数据会让 1 个逻辑绘制变成 2 个 `Primitive`、破坏 `REUSABLE` 整桶复用、污染 §3.8 oracle 的 `PartialEq` 比较；③小尺寸矢量（54 个图标）在**构建期**转成图集（§5.6），不受影响 |
| 修订后设计 | phase-02 文档 §6.3.4；`Path` 的几何必须是 `GeometryPipeline::Ui` + 恒等变换；AA 由后续**两阶段 coverage**（`kind = 4` 已在 contract 预留）或 MSAA 承担，**不进 v1**；**像素 golden 覆盖矩阵必须显式标注「路径硬边」**（phase-02 §14.10） |

### F-27 `Primitive::Geometry` 删除 `pipeline` / `damage` 字段

| 项 | 内容 |
|----|------|
| 原文 | 附录 C：`Geometry{ geometry_id, transform, pipeline, damage }` |
| 偏差 | 改为 `GeometryDraw{ id: GeometryId, transform: Transform }`（独立类型名，避免与 `paint::geometry::Geometry` 混淆） |
| 原因 | ①`pipeline` 属 `GeometryDesc`（注册期不可变），每帧图元再存一份 = 两个真值来源（I2）；②`damage` 是**计算结果**（变换前后 AABB ∪ 物理像素外扩），不是存储字段；存进去会让「同一几何被两个节点引用」时无法归属，也会让复用的 lane 持有过期 damage |
| 修订后设计 | phase-02 文档 §6.3.7 / §9.6；damage 由 P06/P07 在累加时按 `desc.bounds` + `transform` 计算 |

### F-28 `color 0.3.3` 没有 `map_chroma`（§5.8.1 事实更正）

| 项 | 内容 |
|----|------|
| 原文 | §5.8.1：「它提供 `ColorSpaceTag::{Oklch, …}`、`DynamicColor::convert/interpolate`、`map_lightness/map_hue/map_chroma`、CSS Color 4 的 `none` 缺失分量与 premultiplied 插值」 |
| 偏差 | **不存在 `map_chroma`**。`DynamicColor` 只有 `scale_chroma(scale)`（`dynamic.rs:264`）、`map_lightness`（:530）、`map_hue`（:550）；`map_chroma` 在 `color-0.3.3` 全 crate 无此符号。另：`DynamicColor { cs, flags, components }` 三个字段均 `pub` 且类型**无 `#[non_exhaustive]`**（`dynamic.rs:34-50`），可直接构造，不需要 parse 路径 |
| 原因 | 0.3.x 的 chroma 操作统一为「按比例缩放」（`ColorSpace::scale_chroma`，`tag.rs:346`），没有绝对映射 |
| 修订后设计 | 自建 gamut mapping 的 chroma 二分**直接构造** `DynamicColor { cs: ColorSpaceTag::Oklch, flags: Flags::default(), components: [L, mid, h, alpha] }`，再 `.convert(ColorSpaceTag::LinearSrgb)`。等价写法 `scale_chroma(mid / C)` 需 `C == 0` 特判且有除法误差，不采用。phase-02 §5.4 的伪代码与 §14.1 的测试按直接构造实现 |

### F-29 `Canvas` v1 **不**注册应用自有 pipeline（§5.9 的延后）

| 项 | 内容 |
|----|------|
| 原文 | §5.9：「应用注册自有 pipeline（顶点布局 / uniform / shader 句柄），在 paint 阶段向 `Scene` 追加 `Primitive::Canvas(CustomDraw { pipeline, buffers, uniform, damage })`」 |
| 偏差 | v1 的 `Canvas` **只记录引擎图元**（`Quad`/`GlyphRun`/`Image`/`Path`/`Shadow`/`Geometry`/`Clip`），不注册 pipeline、不接受 GPU 句柄、不允许任意命令插入 |
| 原因 | ①D2 明确不内化外部 shader 工具链（P10 的 build.rs 只编译引擎三套 shader）；②§5.9 自身禁止「任意 GPU 命令插入」（破坏 I4 与 damage 正确性），而「不能插入命令的自定义 pipeline」语义上等价于用引擎图元拼装；③自定义 pipeline 需要三平台 PSO 创建/资源绑定/延迟销毁，是独立工程量，且必须以 P11/P16/P17 全完成为前提 |
| 修订后设计 | phase-02 文档 §11（含 `Canvas` 的 lane 片段模型，见 F-33）；扩展路径（additive）：`Canvas::push_custom(CustomDraw)` + `GeometryPipeline::Custom { pso }` + `App::register_pipeline` + `kind` 保留值 6/7 |

### F-30 `GeometryId` 是 `Copy` 句柄，释放必须显式

| 项 | 内容 |
|----|------|
| 原文 | §5.5 表格：「`drop(GeometryId)` → retire 队列延迟释放」 |
| 偏差 | `GeometryId` 是 `Copy`（无 `Drop`），释放改为显式 `App::release_geometry(id)`；节点侧由 P08 的 `NodeKind::Geometry` 卸载自动 release |
| 原因 | `Primitive::Geometry`/`Path` 每帧都要携带 id；`Drop` 句柄要么每帧 clone（引用计数）、要么让 `Primitive` 不能 `Copy`，两者都抬高 lane 复用成本 |
| 修订后设计 | phase-02 文档 §9.1；失败模式（忘记释放 = VBO 泄漏到进程结束）写进 §16 R9，P18 用内存快照/`GpuTimer` 发现 |

### F-31 `GeometryPipeline` 不含 `Path` 变体

| 项 | 内容 |
|----|------|
| 原文 | §5.5「`GeometryPipeline`：应用注册或引擎内置」；§5.1 把 `Path` 的目标管线写成 "geometry pipeline"，隐含一个独立的 Path 管线 |
| 偏差 | v1 只有 `Ui`（stride 8）与 `Textured { atlas }`（stride 16）两个变体；`Primitive::Path` 用 `Ui` + 恒等变换 |
| 原因 | `Path` 与 `Ui` 的顶点布局、着色方式、管线状态完全相同，唯一差别是「谁拥有几何、变换是否恒等」——那是 `Primitive` 的语义，不是 GPU 管线的语义。再造一个变体会让 P09 维护两条一模一样的管线（三平台 × 2 = 6 份） |
| 修订后设计 | phase-02 文档 §9.4/§9.7；`Ui` 的颜色走 per-draw 常量（`GeometryColor`，16 B），顶点不带颜色 |

### F-32 渐变 v1 只支持双色 + linear premultiplied 插值（OKLab 渐变与 LUT 延后）

| 项 | 内容 |
|----|------|
| 原文 | §5.8.2：「渐变必须在 OKLab 空间插值……在 CPU 生成梯度 LUT，仍不进片元」；§5.4 `gradient_eval(kind, t, color0, color1)` |
| 偏差 | v1 的渐变在 **shader 内、linear premultiplied 空间**插值（`color0`→`color1`），只用 `params0.z` 区分 horizontal/vertical/radial；**不生成 LUT**，不支持 ≥3 色标，不支持带角度的线性渐变 |
| 原因 | ①`GpuPrimitive` 冻结在 128 B，只有两个颜色槽；LUT 需要「渐变纹理」= 新 `AtlasKind` + 新 `kind` 值 + `texture_index` 语义扩展，属**契约变更**（I2/I8 要求三平台 shader 同步）；②shader 内做 OKLCH→linear 的每片元转换（cbrt + sin_cos）代价明显；③`ColorResolver::interpolate` 保留为 **CPU 侧**求值（P13 动画、P03 hover 过渡、主题混合），两套语义在 `contract.md` 分别标注，禁止互相替代 |
| 修订后设计 | phase-02 文档 §5.5/§5.6/§16 R3；触发条件：设计稿出现 ≥3 色标，或深色主题下双色线性渐变肉眼可见发灰 → 引入 `AtlasKind::Lut` + `kind` 变体（additive） |

### F-33 `Canvas` 改为「lane 片段」模型，删除 `CanvasOp`（裁决 ②）

| 项 | 内容 |
|----|------|
| 原文 | §5.9 的 `Primitive::Canvas(CustomDraw{…})`；phase-02 v1 草案的 `Canvas{ bounds, ops: Vec<CanvasOp> }` + `CanvasOp` 枚举 |
| 偏差 | `Canvas{ bounds: Rect, lane: scene::Lane }` + **公开 `push_*`**（`push_quad`/`push_text`/`push_image`/`push_path`/`push_shadow`/`push_geometry`/`push_clip`/`pop_clip`）+ 只读 `bounds()/primitives()/is_empty()`；**`CanvasOp` 删除** |
| 原因 | ①裁决 ②：`Canvas` 是应用层**唯一公开写入口**，`Scene` 只读；②`CanvasOp` 是 `Primitive` 的**第二份图元表示**（I2 违反），删掉后全链路只有 `Primitive` 一份；③`Canvas` 持有 `Lane` 后，`Lane::new/push/clip_depth` 在 P02 就有**非 test 消费者**（解决 `dead_code`，见 S-08） |
| 修订后设计 | phase-02 文档 §11（API/所有权/不变量/扩展路径）、§7.1（`Lane` 的 `pub(crate)` 写入面）、§7.2（可见性契约）；递归类型（`Primitive::Canvas` → `Canvas` → `Lane` → `Vec<Primitive>`）由 `Vec` 提供间接，编码器 `assert!(canvas_depth == 0)` 禁止嵌套 |

### S-08 P02 的落地策略：只落「可达且 §1.3 白名单内」的数据契约（裁决 ①）

- **决策**：P02 落地 **7 个文件** —— `paint.rs`、`paint/{color,primitive,scene,geometry,canvas,atlas}.rs`；`lib.rs` 追加
  `pub mod paint;`；`crates/kytrix_gui/Cargo.toml` 追加 `bytes`。**推迟**：`paint/instance.rs`→**P07**、
  `render/damage.rs`→**P06**、`render/atlas.rs`+`render.rs`→**P05**。三处的**规格**在 phase-02 文档里一次冻结
  （字段/签名/算法/常量），实现 Phase 只做「把规格变代码 + 接上消费者」，不得改语义。
- **依据**：`cargo +nightly clippy --all-targets -- -D warnings` 下，任何无同相非测试消费者的 `pub(crate)` item（甚至私有模块
  里的 `pub`）都会被 `dead_code` 判死（`%TEMP%\kg-dead-probe` / `kg-dead-probe2` 实测原文见 phase-02 §2.4c）；而 §1.3 的
  公开面是一张封闭表，`GpuPrimitive`/`InstanceBuffer`/`DamageAccumulator`/`AtlasAllocator` 都不在其中，不能靠「提升为 `pub`」
  规避；AGENTS §4 又禁止 `#[allow(dead_code)]`。三者夹击下唯一可行解是**按消费者排序落地**。
- **硬规则**：P02 **不得**创建 `src/render.rs`、`src/render/`、`src/paint/instance.rs`、`src/paint/damage.rs`；
  每个 `pub(crate)` item 必须有非 test 消费者（P02 是 **5 个方法 + 1 个常量**：`Lane::{new, push, clip_depth, primitives, is_empty}` 与 `MAX_CLIP_DEPTH`，消费者 = `Canvas`）。
- **代价**：P05/P06/P07 需要多一步「实现已冻结的规格」。
- **第二轮修订（B1/B2/B3/N11）**：`Scene` / `LaneId` / lane 分配 / 绘制序**整体推迟 P03**；P02 的 `paint/scene.rs` 只剩
  **`pub(crate)` 的 `Lane` 与 `MAX_CLIP_DEPTH`**（`pub(crate) mod scene;`）。`Lane::push` 内部用
  `usize::from(self.clip_depth()) < usize::from(MAX_CLIP_DEPTH)` 断言，保证 `clip_depth()`/`MAX_CLIP_DEPTH` 有非 test 消费者。

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

- **决策**：`Scene` 的公开面**全部只读**（`new`/`Default`/`lane`/`lane_count`/`order`/`iter`/`primitive_count`/`is_empty`）；
  写入面（`new_lane`/`release_lane`/`lane_mut`/`clear_frame`/`set_order`）为 `pub(crate)` 且**P02 不落地**（无消费者 → P03）。
  应用层的写入口是 **`Canvas`**（8 个 `push_*`/`pop_clip`），它内部持有 `scene::Lane`。
- **理由**：§1.3 明确「`Scene`（只读句柄）」；`Scene` 的公开可变方法会给出「应用可构造 Scene 但无法提交」的死 API；
  而把 `Scene` 改成 `pub` 可变只是为了 P02 测试方便，属于用公开面换测试便利，不可接受（裁决 ② 明确否决）。
- **备选（已否决）**：`Scene` 的写入面 `pub`。
- **第二轮修订（B1/B2/N11）**：`Scene` 在 P02 **完全不存在**（连只读面也没有），整体推迟 P03；P02 的写入口只有 `Canvas`。

### S-11 图集 v1：单页 + `R8`/`BGRA8 sRGB` + 1 纹素 padding + 无 CPU 像素副本

- **决策**：每个 `AtlasKind` **单页**（Glyph `2048²` R8 = 4 MiB，Image `2048²` BGRA8 = 16 MiB）；内容区四周 1 纹素清 0 padding
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
- **理由**：无 CSS 字符串解析 → 无缺失分量/powerless hue 的状态机；`from_hex` 无 P02 消费方（应用层可直接用 `color::parse_color`）。
- **延后**：`from_hex`、宽色域变体、HDR/PQ/ICC（§5.8.3 已定 HDR 不进 v1 默认路径）。

### S-13 damage 阈值（16 / 0.5）是常量而非配置层

- **决策**：`MAX_DAMAGE_RECTS = 16`、`FULL_FRAME_AREA_RATIO = 0.5` 都是模块内 `pub(crate) const`；不引入 `DamageConfig` / 环境变量 / 特性开关。
- **理由**：AGENTS §1.2 禁止「当前没有实际需求的配置层」；两个数字的依据分别是三平台 dirty rect 列表长度上限（16 是安全值）
  与「超过半屏时全帧更便宜」的经验值。P18 基准若给出更好的数值，改常量 + 更新文档即可（V-13）。

### S-14 边框 + 渐变 = 两个 `Quad`（顺序 / 混合 / 责任，裁决 ⑤）

- **决策**：`QuadPaint` 保持三选一（`Solid{fill,border}` / `Gradient`），**不新增**「渐变 + 边框」的复合变体。需要两者时由
  P03 的 style → paint 产出**两个 `Quad`**：①`Gradient` 填充（无 border）；②同 bounds 的 `Solid{ fill: TRANSPARENT, border: Some(b) }`。
- **绘制顺序**：先填充/渐变、后边框（顺序即绘制序，引擎不重排）。
- **混合语义**：边框像素 = `border over fill`，填充内缘只被覆盖一次；边框不透明时直接覆盖（CSS 内联边框的视觉）。
  两者都半透明时边框区域经历**两次 premultiplied over**——这是与 CSS「半透明 border 叠在 background 上」一致的**已接受行为**。
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

| # | 项 | 复核 Phase | 触发条件 |
|---|----|-----------|----------|
| V-11 | `GpuPrimitive` 的 16 B 对齐与三平台常量/结构化缓冲要求的组合（D3D12 需 256 B 基址对齐、Metal `setVertexBytes` 上限、Vulkan `minStorageBufferOffsetAlignment`） | P09/P10 | 首次上传实例缓冲；若某平台的基址对齐与「只写 dirty 区间」冲突，改为 256 B 对齐的分段上传（不改 128 B 契约） |
| V-12 | `Path` 无 AA 在大面积矢量填充上的视觉可接受性（裁决 ③） | P18 | 像素快照出现明显锯齿且该图形未走构建期图集 → 启用两阶段 coverage（`kind = 4`）；契约不变 |
| V-13 | damage 阈值 16 / 0.5 与三平台实际收益 | P18 | L4 基准显示局部重绘收益低于全帧 |
| V-14 | 图集单页容量（Glyph 2048² / Image 2048²）与真实资源 | P15/P18 | 出现 >2048 的预览图或字形集溢出 → 加 `page` 字段或扩容（契约变更，需三平台同步） |
| V-15 | 每帧 `resolve` 颜色的 CPU 成本 | P18 | >0.5 ms/帧 → lane 内缓存 `LinearPremulRgba`（additive） |
| V-16 | `render/damage.rs`（P06）与 `render/atlas.rs`（P05）的落地顺序是否与文本/图集的实际先后一致 | P05/P06 | 若总计划调整相位：文件跟**消费者**走（phase-02 §3.2 末段），并同步更新 phase-02 §19 |

### S-16 §1.3 公开白名单按**传递闭包**扩充（N7）

- **决策**：§1.3 的白名单按类型名列出；`paint` 的契约会通过字段/参数**传递**带出下表类型，因此把它们显式登记为
  §1.3 的传递扩充（不新增能力）。`Lane` 与 `MAX_CLIP_DEPTH` 保持 `pub(crate)`，**不进**公开面。
- **登记清单**（详见 phase-02 §4.4）：

  | 传递新增的公开类型 | 由谁带出 | 消费 Phase |
  |---|---|---|
  | `Quad` `QuadPaint` `Border` `Gradient` `GradientKind` | `Primitive::Quad` | P03/P07/P06 |
  | `GlyphRun` `PositionedGlyph` | `Primitive::GlyphRun` | P06/P07 |
  | `Image` `Path` `Shadow` `ClipOp` `ClipRect` `GeometryDraw` | `Primitive` 其它变体 | P03/P07/P08 |
  | `AtlasKind` `AtlasTile` | `GlyphRun`/`Image` 的字段 | P06/P15/P07 |
  | `LinearPremulRgba` `Interpolation` `HueDirection` | `ColorResolver` 的签名 | P02/P03/P06/P08 |
  | `IndexType` `Topology` `GeometryPipeline` | `GeometryDesc` 的字段 | P08/P09 |

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
  `debug_assert!(usize::from(self.clip_depth()) > 0)`（Pop）与 `debug_assert!(usize::from(self.clip_depth()) < usize::from(MAX_CLIP_DEPTH))`（Push），
  并用 `saturating_add/sub` 防止 release 下回绕。
- **`MAX_CLIP_DEPTH`**：`pub(crate) const MAX_CLIP_DEPTH: u8 = 8`（B3：类型就是契约的一部分）。
- **理由**：应用层经 `Canvas` 表达 clip，越界是**可恢复的调用错误**，不应 panic（N6）；而引擎内部若绕过 `Canvas`
  直接 `Lane::push` 属编程错误，用 debug 断言抓。C1（lane 末尾平衡）由 P06 的展平阶段断言。

### S-19 数值与 damage 语义契约（N2 / N3 / N4 / N5 / N13）

- **N2 整数取回**：`params1` 的四个分量是「整数编码进 f32」；三套 shader 一律用**数值转换**（HLSL/MSL `(int)x`、GLSL `int(x)`），
  **禁止**位重解释（`as_type`/`asfloat`/`floatBitsToInt`）。`contract.md` 必须逐字写明。
- **N4 数值健壮性**：`resolve`/`interpolate` 的**每个**分量先 `NaN → 0` 再 clamp 到定义域
  （`clamp_01` / `clamp_nonneg`，`h` 先 `rem_euclid(360)`），输出严格在 `[0,1]`（alpha 单独处理）。
- **N5 可复现性**：同平台**逐位**可复现；跨平台允许 ULP 级差异（`sin_cos`/`cbrt` 的 libm 实现不同），像素 golden 按 **8 位**比较。
- **N3 scroll 放弃规则**：仅当**所有** dirty rect 都落在 `scroll.rect` 内才保留 scroll hint；否则放弃 scroll、保留全部 rects
  （宁多画不漏画），不丢弃任何脏区。
- **N13 Clip 与 damage**：`Clip` **本身不产生像素**，不 push damage；mask 边界只作用于被裁剪图元（其 damage 由被裁剪图元
  自己的 bounds + `clip` 求交得到）。

### S-20 `GeometryId` 的可见性：私有字段 + 公开 getter + 仅测试构造器

- **文档原文（phase-02 §9.1/§18.2）**：`GeometryId { index: u32, generation: u32 }` 字段私有，「只能由 `GeometryStore` 铸出」。
- **代码审查要求**：字段改 `pub(crate)` + 公开 `index()`/`generation()`。
- **不可行**：workspace lint `field_scoped_visibility_modifiers = "forbid"` 明确拒绝 `pub struct` 里的 `pub(crate)` 字段
  （实测 `%TEMP%\kg-field-vis`：`error: scoped visibility modifier on a field`）。
- **实际落地**：字段**保持私有**；新增公开 `index()` / `generation()`；测试用 `#[cfg(test)] pub(crate) const fn for_test(index, generation)`
  构造。`for_test` 只存在于 test 构建，因此**不触发 `dead_code`**，也不扩大公开面。
- **为什么不选「`pub` 字段 + 文档说明」**：那会让应用层用结构体字面量伪造句柄（P09 无法区分伪造与真实，只能靠代际偶然拦截）；
  `for_test` 把这条口子限制在本 crate 的测试构建内。
- **P09 交接**：`GeometryStore` 落地时把 `for_test` 换成 `pub(crate) fn mint(index, generation)`（同签名、有非 test 消费者），
  测试改用 `mint`；公开面不变。

### S-21 第二轮代码审查（A/B 均 PASS-WITH-FIXES）：B1 与 N1–N15 的处置

- **阻塞 B1 已修复**：`interpolate` 的 `mid` 此前在 `Interpolation::LinearSrgb` 下复用了 `resolve` 的 (A) 路径（逐通道 clamp），
  破坏端点不变量。现在：①`resolve` 与 `interpolate` 共用 `normalized_dynamic`（alpha clamp + Oklch 的 L/C/h 归一）；
  ②`mid` **无条件**走 `gamut_mapped_linear`（转 Oklch → 24 步 chroma 二分 → premultiply），不再看插值空间。
  回归用例：`interpolate_endpoints_match_resolve`（4 空间 × 4 端点 × 2 端）、`linear_srgb_space_matches_resolve_for_p3_endpoint`
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
  `quad_paint_encodings_are_mutually_exclusive`、`gradient_kind_has_three_distinct_variants`、`clip_op_push_pop_roundtrip`。
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
- **触发条件**：P10 的 `contract.md` 同步要求 rustdoc 可读，或任何 Phase 决定开启 `#![warn(missing_docs)]` → 先补 `paint::*`。