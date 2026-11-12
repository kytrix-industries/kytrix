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

| 项 | 内容 |
|----|------|
| 原文 | P01 §4.5 公开路径表：`NodeBuilder` `NodeHandle` `Slot` `Children` → `kytrix_gui::node::*`（定义文件 `src/node.rs`，模块根） |
| 偏差 | `NodeBuilder` 定义在 **`src/node/builder.rs`**，公开路径 **`kytrix_gui::node::builder::NodeBuilder`**。`NodeHandle`/`Slot`/`Children` 推迟 P08（S-29） |
| 原因 | P03 的文件树（brief）把 `NodeBuilder` 放叶子文件，且 P02 §3.3 已固化「模块根不定义 item」；两处约束冲突时按 P01 §4.5 自己的判据「公开路径 = 类型实际定义所在的文件路径」处理 |
| 修订后设计 | phase-03 §4.1 的路径表；P08 的应用层迁移按 `node::builder::NodeBuilder` 引用 |

### F-35 公开面首次包含第三方 crate 类型（`Style.inner: taffy::Style<SmolStr>`）

| 项 | 内容 |
|----|------|
| 原文 | 设计 §4.9.3 的草图 `pub struct Style { pub inner: taffy::Style, … }`；D24/I17 要求引擎只暴露 `Style`/`StylePatch`/`bind_style` |
| 偏差 | 照草图**暴露** `pub inner: taffy::Style<SmolStr>`（类型参数必须写全 `SmolStr`：`taffy::Style` 的默认参数是 `DefaultCheapStr`，两者不是同一类型）。应用层 crate（`base_ui`）必须自行声明 `taffy = { workspace = true }` 才能构造它 |
| 原因 | ①不暴露就必须为 40+ 个布局属性提供引擎自有访问器 —— 那正是 D24 明令只允许存在于应用层的「builder/DSL」；②`taffy` 已是引擎硬依赖且 workspace 精确钉 `=0.15.0`（P01 §5.3④），版本不会漂移；③`taffy` 的 `Style` 是**布局属性的唯一事实**（I5 的节点树即布局树），包一层再翻译一遍是双份真相 |
| 修订后设计 | phase-03 §10.1；P06 追加的引擎自有字段（圆角/clip/阴影/字体族）走 `Style` 的新字段（additive），不改 `inner` 的语义。V-23 跟踪「additive 是否被结构体字面量构造挡住」 |

### F-36 不新增 `paint::style::Border`，复用 `paint::primitive::Border`

| 项 | 内容 |
|----|------|
| 原文 | P01 §4.5：`Style` `StylePatch` `Border` `Cursor` → `kytrix_gui::paint::style::*`（`src/paint/style.rs`）；P03 的 brief 同样把 `Border` 列进 `paint/style.rs` |
| 偏差 | **`paint/style.rs` 不定义 `Border`**；`StylePatch.border: Option<paint::primitive::Border>`（P02 已有的 `{ width: f32, color: Color }`） |
| 原因 | P02 §4.2 的路径表已把 `Border` 归入 `paint::primitive::*`（它是 `QuadPaint::Solid` 的字段类型，属 I2 数据契约）。再定义一个 `paint::style::Border` 会出现两个同义类型 → 「一条边框两种真相」，且 P06 的 style→paint 每次都要转换 |
| 修订后设计 | phase-03 §10.2；P01 §4.5 的 `Border` 行以本条 + P02 §4.2 为准（历史正文不改） |

### F-37 `NodeBuilder::build` 返回 `Result<NodeId, EngineError>`

| 项 | 内容 |
|----|------|
| 原文 | 设计 §3.10 / §4.9.3 / 附录 A：`pub fn build(self, tree: &mut NodeTree, parent: NodeId) -> NodeId` |
| 偏差 | **`-> Result<NodeId, EngineError>`**，唯一错误是 `EngineError::StaleNode { node: parent }`（父节点已卸载） |
| 原因 | ①挂到已卸载的容器是应用层可触发的真实错误（`Window::tree_mut` 允许 `remove`）；②静默丢弃子树会变成 stale drawing 且无诊断（违反 I10 的「不静默降级」精神）；③`StaleNode` 需要真实生产者（P01 §6.2.2 #7 的 payload 是 `NodeId`），否则该变体在 P03 无构造点；④冻结表不允许新增「`ParentMissing`」之类的变体 |
| 修订后设计 | phase-03 §12.1/§12.2；应用层写法 `.build(&mut tree, parent)?`。P08 的 `base_ui` 在 `Result` 上传播（挂载失败 = 应用 bug，日志级别 error） |

### F-38 `Scene` 的公开签名按可见性重构（`lane → lane_primitives`、`lane_mut` → `push`）

| 项 | 内容 |
|----|------|
| 原文 | P02 §7.2 的「P03 落地时的形状（已冻结）」：`pub fn lane(&self, id) -> Option<&Lane>`、`pub(crate) fn lane_mut(&mut self, id) -> Option<&mut Lane>` |
| 偏差 | ①`lane` 改为 **`pub fn lane_primitives(&self, id: LaneId) -> Option<&[Primitive]>`**（**不**再返回 `&Lane`，见裁决 ③ 的方案②）；②`lane_mut` **不落地**，改为 **`pub fn push(&mut self, lane: LaneId, primitive: Primitive) -> bool`**；③写入面的可见性见 S-33，构造/派生约束见 F-39 |
| 原因 | `Lane` 按 P02 N7/S-15 保持 `pub(crate)`；公开签名里出现 `pub(crate)` 类型会触发 `private_interfaces`（warn-by-default），在 `-D warnings` 门禁下**编译失败**。**已实测**（`%TEMP%\kg-p03-priv-probe`，`rustc 1.97.1`，`--edition 2024 -D warnings`）：`error: type `Lane` is more private than the item `Scene::lane`` / `= note: -D private-interfaces implied by -D warnings`；唯一规避是 `#[allow]`，AGENTS §4 禁止。裁决 ③ 进一步要求取**公开面更小**的方案②（只给图元切片投影；`Lane` 的内部状态——`clip_depth`——不泄漏，P06/P07 在 crate 内另有 `pub(crate)` 通道） |
| 修订后设计 | phase-03 §13.2 的差异表；P02 §7.2 的其余条款（代际、`order`、持久性、`Scene` 不 `Clone`）**不变** |

### F-39 `Scene` 不实现 `Clone`/`Default`，且**无公开构造器**（`pub(crate) fn empty()`）

| 项 | 内容 |
|----|------|
| 原文 | P02 §7.2：`#[derive(Debug, Default)] pub struct Scene` + `pub fn new() -> Self` |
| 偏差 | ①`Scene` **只 `derive(Debug)`**（不 `Clone`、不 `Default`，P02 的「不 `Clone`」保留）；②构造函数为 **`pub(crate) fn empty() -> Self`**（唯一的构造点 = `Window::new`），应用层无法构造；③`Arena` 随之**不再实现 `Default`** |
| 原因 | 裁决 ③ 明确要求「`Scene` 不实现 `Clone`/`Default`」。去掉 `Default` 后 `pub fn new()` 会触发 clippy `new_without_default`（style 组、warn-by-default），而 `#[allow]` 被 AGENTS §4 禁止 → 改名 `empty` 并降为 `pub(crate)`（同时消除「应用层自造 Scene」的口子） |
| 修订后设计 | phase-03 §13.1/§13.2/§17.8-1；门禁 §18-11 新增三条 grep（`engine-internal` 标注、无 `derive(…Default/Clone)`、无 `pub fn new/default`） |

### F-40 `App::refresh_styles(window) -> Option<StylePassReport>` 与报告类型

| 项 | 内容 |
|----|------|
| 原文 | phase-03 提案期：`pub fn refresh_styles(&mut self) -> usize`（全窗口、只返回被求值的个数） |
| 偏差 | ①**按窗口**：`pub fn refresh_styles(&mut self, window: WindowId) -> Option<StylePassReport>`；②新增公开类型 **`StylePassReport { evaluated, layout_dirty, paint_dirty, wake_requested }`**（`Copy`）；③`None` = 该窗口不存在；④pass 结束后 `clear()` 求值栈（panic 兜底） |
| 原因 | 裁决 6：报告要能被 P06 的 `FrameRecord` 直接复用，且帧是**按窗口**推进的；P03 没有「未知窗口」的错误变体（P01 §6.2.2 冻结表），`Option` 是唯一诚实的表达（比「返回全零报告 + 静默吞 bug」好） |
| 修订后设计 | phase-03 §5.1（签名 + 报告字段语义）、§14.4（逐字 + 决策表）、§4.1/§4.2（路径与白名单闭包）、§17.9-4/5/7/8/11 |

### S-23 P03 的落地判据 =「§1.3 白名单内 **或** 有非 test 消费者」

| 项 | 内容 |
|----|------|
| 决策 | 与 P02 §3.2（S-08）同源：`pub(crate)` item 必须有非 test 消费者；`pub` item 必须在 §1.3 白名单内或按其传递闭包登记（phase-03 §4.2）。**唯一例外**是 `Scene` 的写入面（S-33） |
| 理由 | `dead_code` 是编译期硬门禁（`-D warnings`）；而白名单是「不越界」的产品约束。两者同时满足才落地 |
| 执行 | phase-03 §4.3 的逐条审计表（实现时必须逐项对齐）；门禁 §18-8 的 grep |

### S-24 `Application` / `AppHandle` 推迟 P08/P09

| 项 | 内容 |
|----|------|
| 决策 | P03 **不落地** `Application`（`run(build)` 需要平台事件循环）与 `AppHandle`（需要 `PlatformDispatcher`）。`App` 侧命名 **`runtime_handle(&self) -> &tokio::runtime::Handle`**（借用；字段名 `runtime`） |
| 理由 | P03 没有事件循环/派发器 → 两者无任何非 test 消费者，落地即 `dead_code`，或被迫写空壳（AGENTS §1.2/§4 禁止）。**命名由裁决 ① 定死**：不占用 `handle()`，P09 加 `App::handle() -> AppHandle` 时是纯新增、零破坏 |
| 影响 | §1.3 白名单里的这两个名字仍属「**允许**但不要求本阶段落地」；P08/P09 落地时补齐（`AppHandle` 的语义见设计 §1.4 的跨线程入口） |
| 交接 | P09 的 `PlatformDispatcher::wake(FrameWake)` 取代 `App::take_wake_request` 的**同一处调用点**（phase-03 §14.3） |

### S-25 信号/派生值不回收（与 `App` 同寿），`SignalId` 无代际

| 项 | 内容 |
|----|------|
| 原文 | 设计 §3.9：「Signal 值：显式 `StateStore::drop(signal)`，引用计数（应用层持有句柄），无引用即释放」 |
| 决策 | **不回收**：`values: Vec<SignalSlot>` 只增不减；`SignalId`/`DerivedId` 是纯 `u32`，**不带代际** |
| 原因 | `Signal<T>` 被冻结为 `Copy` 句柄（设计 §3.2 + phase-03 §7.2）→ **无法**做引用计数（Copy 无法观测「最后一个句柄」）。没有删除就没有槽位复用，代际无用。应用的状态集是**有界**的（一个交易终端数百到数千信号），内存上界可算：`SignalSlot` ≈ 32 B |
| 影响 | 设计 §3.9 的该行**作废**；`SignalId` 不需要 `StaleSignal` 类错误（也就不会破坏 P01 §6.2.2 的 17 变体冻结表） |

### S-26 派生环检测移到运行时求值栈，`read_derived` 返回 `Result`

| 项 | 内容 |
|----|------|
| 原文 | 设计 §3.9：「`Derived` 链在注册时做 DFS 检测环，命中即 `EngineError::CyclicDerived`，**拒绝注册**」 |
| 决策 | 环检测在 **`ReadCtx::eval_derived` 的求值栈**上做：`EvalStack.contains(id)` → `Err(EngineError::CyclicDerived { derived })`。`App::derived` 永不失败；`ReadCtx::read_derived` 返回 `Result<T, EngineError>`。**入栈/出栈由 `EvalGuard`（拥有 `&mut EvalStack`，`Drop` 里 `pop`）保证 panic 安全**；**环检测先于 `take_compute`** → 求值失败不污染缓存（`dirty` 与 cache 均不动），闭包 panic 后该派生 `compute == None`，后续读返回 `CyclicDerived` 而不是 panic |
| 原因 | 注册期 DFS **不可行**：①闭包读哪些信号/派生，在闭包运行前不可知；②注册时新派生尚不存在于任何闭包里（自引只能在运行期通过 `Rc<RefCell<Option<_>>>` 等间接建立），因此「注册时一定有环」这个前提不成立 |
| 代价 | 派生读派生时要显式处理 `Result`（可 `expect`/`unwrap_or_default`）。绝大多数派生只读信号（不失败），不受影响 |
| 备选（已否） | 「派生不得读派生」可从结构上消除环 —— 但会丢掉设计 §3.9 的派生链（行情派生指标需要它） |

### S-27 删除 `versions`（独立向量与每槽位字段都删）

| 项 | 内容 |
|----|------|
| 原文 | 设计 §3.2：`StateStore { values: HashMap<SignalId, Box<dyn Any>>, versions: Vec<u64> }`（「按 `SignalId` 索引，写入即 +1」） |
| 决策 | **两者都删**：`versions` 向量不存在；`SignalSlot { value }` 也不再保留 `version: u64`（裁决 ②：「若没有读者则删除该字段，不要为测试保留」） |
| 原因 | 本设计的失效是 **push**（`App::write` 直接标脏），staleness 由 `DerivedSlot.dirty` / `Node.dirty` 表达 —— 没有任何代码做「版本比较」。核对 P03 的全部非 test 读者：等值短路由值比较完成；`StylePassReport` 只统计 `evaluated`/`layout`/`paint`/`wake`；帧记录属 P06 → **零读者** ⇒ 保留即 `dead_code`（非 test 构建）+ 违反 AGENTS §2.1 |
| 影响 | 「写相等值时是否失效」由 `App::write` 的 `PartialEq` 短路回答（相等**不**失效，phase-03 §7.5）；`SignalSlot` 尺寸 32 B → **24 B** |

### S-28 `GEOMETRY` / `REUSABLE` 位在 P03 无读者（位已定义、有写者）

| 项 | 内容 |
|----|------|
| 事实 | `DirtyFlags` 的 6 位在 P03 全部定义；`LAYOUT`/`PAINT`/`A11Y`/`ORDER` 有写者**且**有读者；`GEOMETRY` 无写者（P08）；`REUSABLE` 只在 `mark_*` 里被 `remove`（常量被读），读者在 P06 |
| 决策 | 保留 6 位定义（设计 §3.4 冻结的位布局），不加占位/不删位 |
| 理由 | 位是 `pub const`，不触发 `dead_code`；`remove(REUSABLE)` 真实读取该常量。删位会让 P06/P08 重新编号 → 破坏「位定义冻结」 |

### S-29 `Children` / `Slot` / `NodeHandle` 推迟 P08

| 项 | 内容 |
|----|------|
| 原文 | §1.3 白名单的「节点」行列出 `NodeId`、`NodeBuilder`、`NodeHandle`、`Slot`、`Children` |
| 决策 | P03 只落 `NodeId` 与 `NodeBuilder`；其余三个推迟 P08 |
| 原因 | 它们是**声明式建树的语法层**（子节点集合 / 命名插槽 / 可保存句柄），消费者是 `base_ui` 的组件组合（P08）。P03 的等价能力是 `NodeBuilder::build(tree, parent)` + `NodeTree::append_child`；`NodeId` 本身已是完整句柄（`Copy` + 代际） |
| 影响 | 白名单不要求本阶段落地（同 S-24） |

### S-30 `StaleNode` / `CyclicDerived` 的实现 Phase 由 P04 改为 **P03**

| 项 | 内容 |
|----|------|
| 原文 | P01 §6.2.2 冻结表 #7/#8 的「实现 Phase」列 = P04（当时的相位编号里 P04 是节点/状态层） |
| 变更 | 实际落地 Phase = **P03**（相位重排：P03 = 节点状态核心）。**变体名、payload、`Display` 文案、`source() == None` 全部不变** |
| 理由 | 冻结表是按当时的总计划编号写的；相位重排后「跟着消费者走」（P02 §3.2 的同一原则）。payload/文案不变 ⇒ 对外契约无变化 |
| 追加 | `error.rs` 的 `all_variants()` 必须补两个新变体（`engine_error_source_is_none` 用例覆盖） |

### S-31 P03 交付 **81** 个内联单测（brief 估算 40–60）

| 项 | 内容 |
|----|------|
| 估算 | brief 的 40–60 |
| 实际 | phase-03 §17 列出 **81**（id 7 / signal 12 / panic 安全 1 / dependency 8 / dirty 11 / tree 9 / builder 6 / style 3 / scene 9 / app 11 / window 4） |
| 理由 | 每条对应一条独立不变量、终止条件或端到端断言（`Arena` A1–A5、依赖图 D1–D5、§3.4 的 8 行失效规则、跨窗口/唤醒/`A11Y`）。P02 交付 60，而 P03 的语义面更宽；平均每例 ~8 行。裁决后新增 5 条：等值短路风险（`Rc<Cell>` 反例）、`EvalGuard` 的 panic 安全、`refresh_styles` 未知窗口、`clear_frame` 的 clip 复位、叶子父的 debug 断言 |
| 兜底 | 排期紧张时按「每张表从上到下」交付到 52 个（覆盖全部机制性断言，表尾为幂等/边界补充） |

### S-32 `Owner::Node` 携带 `WindowId`（+ derive 与去重/序规则）

| 项 | 内容 |
|----|------|
| 原文 | 设计 §3.3：`dependents: HashMap<SignalId, NodeIdSet>`、`tracker.record(signal.id, self.node)` —— 依赖边只带 `NodeId` |
| 决策 | `Owner::Node { window: WindowId, node: NodeId }`；`NodeIdSet` 改为 `OwnerSet = SmallVec<[Owner; 2]>`（**不是** ahash `HashSet`，**不是**位图）；`Owner` 派生 `Clone, Copy, Debug, PartialEq, Eq, Hash`，**不**实现 `Ord`；去重 = 插入前 `contains` 线性查重 + 删除时 `retain` 清历史重复；**序 = 首次登记序**（不重排）；`replace_deps` 的结果**集合**与输入顺序无关，**迭代序**可能随 `new` 的顺序变化（对正确性无影响：失效只做标脏与 damage 并集，两者都与顺序无关） |
| 原因 | ①`NodeId` 是**窗口内** arena 索引：两个窗口可以各自有 `n0.1`，不带窗口会在 `App::write` 里误标另一个窗口的同号节点（stale 之外的第二类错误）；②位图键是稠密 index，而 `NodeId` 的 index 会被复用（装卸），代际不在位图里 ⇒ 陈旧句柄的位会命中复用后的新节点；③稳态读者数 1–3，`SmallVec` 内联零分配、迭代序确定、差分是两条小切片比较；④排序需要 `Ord`，而 P03 没有任何排序消费者（诊断要稳定序时由调用方排序） |
| 代价 | `Owner` 16 B（`u32` + 8 B + 判别）；mount 期主题类信号的集合插入是 O(k) 查重（V-26） |

### S-33 `Scene` 的写入面在 P03 是 `pub`，且 doc 首行标注 engine-internal

| 项 | 内容 |
|----|------|
| 原文 | P02 §7.2：「以下 `pub(crate)`，消费者 = P03 的 paint 循环」；S-10：「`Scene` 只读 + `Canvas` 唯一公开写入口」 |
| 事实 | P03 **没有** paint 循环（绘制指令生产属 P06）→ `new_lane`/`release_lane`/`lane_mut`/`clear_frame`/`set_order` 在 crate 内**零调用者** ⇒ `pub(crate)` 必触发 `dead_code` |
| 决策 | 改为 **`pub`**：`new_lane`/`release_lane`/`push`/`clear_frame`/`set_order`/`clear_order` 全部公开，**每个方法的 doc 首行**写 `engine-internal, 应用层不得调用`；`lane_mut` 不落地（F-38）；构造/派生约束见 F-39 |
| 为什么这不违反 S-10 | S-10 说的是「应用层的**唯一公开写入口**是 `Canvas`」—— 应用层依然拿不到 `&mut Scene`（`Window::scene()` 只读）、`Scene` 不 `Clone`/`Default` 且**无公开构造器**、`Window` 无 `set_scene`。`pub` 的是**方法**，不是可达的写路径 |
| 交接 | P06 的 paint 循环成为首个**引擎内部**调用者；届时若要把这 6 个方法降回 `pub(crate)`，属于可见性收紧（有真实调用者），需另记偏差 |

### S-34 `App::refresh_styles` 作为 P03 的样式求值 pass 落地（按窗口 + 报告）

| 项 | 内容 |
|----|------|
| 事实 | P03 不做 paint pass（绘制指令生产属 P06）。但 `ReadCtx`/`ReadTracker`/`DependencyGraph::replace_deps`/`Derived` 求值/`EvalStack`/`EvalGuard`/`StylePatch` 比较/`affects_layout`/`Node.deps`/`bind_style` 字段这 10 余项能力**没有**其它非 test 消费者 |
| 决策 | 落地 `App::refresh_styles(&mut self, window: WindowId) -> Option<StylePassReport>`（裁决 6 的细化，见 F-40）：按 `NodeTree::order()` 遍历该窗口，对 `PAINT` 且带 `bind_style` 的节点求值、替换依赖、写回 patch、按 `affects_layout` 升级 `LAYOUT`、按 `interactive`/`visible` 变化标 `A11Y`、最后清 `PAINT`；进入时 `clear()` 求值栈（panic 兜底） |
| 为什么不是预防性抽象 | 它是 §4.9.4 步骤 4 的**前半**（「Paint 阶段重新求值 `bind_style` 得到 `StylePatch`」），且必须在**布局之前**运行（`affects_layout` 会改变布局输入）。P06 的 paint 阶段在重建 lane 前调用同一实现 → 不是过渡层，是帧流水线的固定一段 |
| 边界 | **不产生任何 Scene 图元**；不重建 lane、不算 damage（`!REUSABLE` 与 `Node.damage` 交给 P06）；不读 `bind_text`/`bind_a11y`（P05/P11） |
| 测试 | phase-03 §17.9 的 11 条（只重算脏节点、幂等、升级 `LAYOUT`、替换 deps、未知窗口、跨窗口、`A11Y`、报告的三个标志） |

### S-35 `App::write` 的约束与「等值短路」的语义/风险

| 项 | 内容 |
|----|------|
| 决策 | `App::write<T: Clone + PartialEq + 'static>(&mut self, signal: &Signal<T>, value: T) -> bool`；`Signal<T>`/`Transient<T>`/`Derived<T>` 句柄与 `ReadCtx::read` **无** `PartialEq` 约束；短路用 `owned_ref`（借用比较，不克隆）。P03 **不提供** `Signal::set`；若将来提供，bound 必须与 `write` 相同 |
| 语义 | `false` = 值未变（不标脏、不置唤醒）；`true` = 值变了（已失效）。返回值**不**表示「有节点被标脏」 |
| 风险（必须写进应用层契约） | `T` 内部可变（`Rc<RefCell<_>>`/`Cell`/`Arc<Mutex<_>>`）且 `PartialEq` 只比外层 ⇒ 「内部变了但不重绘」。**应用层必须用不可变值语义**；风险由 phase-03 §17.2-6 的反例测试固化（R9） |
| 原因 | 高频行情下「同一报价重复推送」极常见，短路省掉整条失效链路（I3 的「空闲零帧」也依赖它）；`PartialEq` 只在写路径，不传染读/句柄 |

### S-36 `NodeBuilder::build` 的错误语义冻结（`StaleNode` 是唯一 `Err`）

| 项 | 内容 |
|----|------|
| 决策 | `build(self, tree, parent) -> Result<NodeId, EngineError>`，`Err` 只有 `StaleNode { node: parent }`（父不存在/代际不匹配）。其余情形：叶子父（`Canvas`/`Geometry`）= `debug_assert!` + release 放行（子树不参与布局）；结构环（仅 `append_child` 可能）= `debug_assert!` + 不改结构 + `Ok(())`；`Style` 非法 = `Ok`（交给 P04/P05 的布局校验）；OOM = abort（不引入 `try_reserve` 错误面） |
| 原因 | P01 §6.2.2 的冻结表没有「父不可容子」变体，新增会破坏契约；而叶子父是应用层建模错误，debug 构建立刻暴露、release 下行为**安全且已定义**（子树不参与布局，不会 UB/画错） |
| 落地 | phase-03 §12.1 的错误语义表 + `# Panics`、§12.2 伪代码的 `debug_assert!(!p.kind().is_leaf())`、§11.2 的 `NodeKind::is_leaf`、§17.6-6 用例 |

### S-37 `App::runtime_handle` 的命名与返回形态

| 项 | 内容 |
|----|------|
| 决策 | `pub fn runtime_handle(&self) -> &tokio::runtime::Handle`（**借用**，不 clone）；`App` 的字段名同步为 `runtime` |
| 原因 | 裁决 ①：不占用 `handle()` 这个名字 —— P09 加 `App::handle() -> AppHandle` 时是**纯新增、零破坏**；返回借用避免每次取句柄都 `clone`（`Handle: Clone` 是 O(1) 的 Arc 增加，但没必要） |
| 交接 | P09 的 `PlatformDispatcher` 落地后，`take_wake_request` 的调用点换成 `wake(FrameWake)`，`runtime_handle` 保持不变 |

### 八·待复核项（P03 新增）

| 编号 | 内容 | 复核 Phase | 触发条件 / 处置 |
|---|---|---|---|
| V-23 | `Style` 的 P06 字段能否安全 additive 加（若应用层已按结构体字面量构造 `Style`，加字段即破坏构造点） | P06 | style→paint 落地时若冲突，改为 `Style { inner, paint: PaintStyle }` 子结构 |
| V-24 | 派生闭包误读 `Transient` → 永久 stale（写 transient 不置 `dirty`） | P13 | 动画/滚动落地后若出现，加 `debug_assert` 或在求值期标记「读过 transient」 |
| V-25 | `App::write` 的相等比较成本（`T` 大时 O(n)）**与等值短路的语义风险**（`T` 内部可变而 `PartialEq` 只比外层 ⇒ 不重绘） | P18 / P08 | 成本：L4 基准显示写路径占比显著 → 拆细信号；语义风险：写进应用层契约（S-35/R9），`base_ui` code review 约定 + P08 迁移文档 |
| V-26 | `OwnerSet` 的 mount 期 O(k²) 插入（主题类信号聚合上千读者） | P18 | 5000 节点 mount >1 ms → 改 `ahash::HashSet`（接口不变） |
| V-27 | 冒泡终止过保守（`Dimension::Auto` 一律视为依赖子内容） | P04 | `set_layout` 比较新旧 `size`：相同则不再向上（§4.4）；P03 的判定表是它的上界 |
| V-28 | `order_pos` 是 O(n) 线性扫描（`Vec::insert`/`drain` 的定位） | P13 | 虚拟列表每帧上千次结构变更 → 加 `Node.order_index`（additive 字段） |
| V-29 | 节点卸载后 `dependents` 残留死 owner（靠写路径 `retain` 惰性剔除） | P08 | 大量增删节点时内存/写路径扫描变慢 → `App::remove_node(window, node)` 精确退订 |
| V-30 | `size_of::<Node>()` 实测（内联 `taffy::Style<SmolStr>`，估计 ~800 B） | P04 | 超 1024 B 或 5 万节点场景内存吃紧 → `style: Box<Style>`（不改公开面） |

### F-41 `NodeId.generation` 用 `NonZeroU32`（`Option<NodeId>` 保持 8 B）

| 项 | 内容 |
|----|------|
| 原文 | phase-03 §6.1：`pub struct NodeId { index: u32, generation: u32 }`，并把「`generation` 恒在 `1..=u32::MAX`」写成不变量 |
| 偏差 | `generation: std::num::NonZeroU32`；`generation()` 仍返回 `u32`（`.get()`），**公开面不变** |
| 原因 | 实现时 `size_of::<Option<NodeId>>()` 实测 **12 B**（两个 `u32` 没有 niche ⇒ `Option` 多一个 tag）。文档 §17.5-8 要求它 8 B（`Node.parent: Option<NodeId>` 零开销），而 `NonZeroU32` 把「0 永不是合法代际」提升为**类型即证明**：`Option<NodeId>` 回到 8 B，且 `ArenaKey::from_parts` 成为该不变量的唯一守门点 |
| 影响 | 无公开面变化；`NodeId::for_test(_, 0)` 会在测试里 panic（0 本就非法） |

### F-42 `ExpandedDimension::Calc` 是**无条件**分支（`#[cfg(feature = "calc")]` 不可表达），且变体名是 `Calc`

| 项 | 内容 |
|----|------|
| 原文 | phase-03 §9.3（N3）：`#[cfg(feature = "calc")] ExpandedDimension::FitContentCalc(_) => false` |
| 偏差 | ①变体名实际是 **`ExpandedDimension::Calc(_)`**（taffy 0.15.0 源码）；②写成**无条件** match 分支 |
| 原因 | ①按 `E:\.cargo\registry\...\taffy-0.15.0\src\style\dimension.rs` 的 `ExpandedDimension` 定义，calc 变体名是 `Calc`；②我们的 crate **不能** `#[cfg]` 依赖的 feature（`feature = "calc"` 只匹配**本 crate** 的 feature），taffy 的 `calc` 由 workspace 精确开启（`features = [..., "calc", ...]`）⇒ 该变体恒存在，无条件列出即可；若将来关掉 calc，这里会**编译失败**（显式失败优于悄悄把 `Calc` 归入保守分支） |
| 影响 | 行为与文档一致（`Calc` 不依赖子内容）；记录实现事实 |

### F-43 `NodeTree::insert_child` 收「部件」而不是半成品 `Node`

| 项 | 内容 |
|----|------|
| 原文 | phase-03 §11.2：`pub(crate) fn insert_child(&mut self, parent: NodeId, node: Node) -> Result<NodeId, EngineError>` |
| 偏差 | `insert_child(&mut self, parent, kind: NodeKind, style: Style, bind_style: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> StylePatch>>)` |
| 原因 | `Node` 的全部字段都是 `node::tree` 私有的，`NodeBuilder` 无法构造 `Node`；若要走 `node: Node` 就得给 `Node` 加一个带**占位 id** 的构造器（占位 id 是 `NodeId::from_parts(0,0)`，而 0 不是合法代际），比现状更危险。收部件后 `id` 由 arena 铸出、无中间态 |
| 影响 | 纯 crate 内部签名；调用点只有 `NodeBuilder::build` |

### F-44 `eval_derived` 的 `take_compute == None` 分支**不加** `debug_assert`

| 项 | 内容 |
|----|------|
| 原文 | phase-03 §7.4 伪代码：`else { debug_assert!(false, "派生值正在求值（环检测被绕过）"); return Err(CyclicDerived…) }` |
| 偏差 | 去掉 `debug_assert!`，直接 `return Err(EngineError::CyclicDerived { derived })` |
| 原因 | 这个分支有**两种**到达方式：①环检测被绕过（真不变量破坏）；②上一次求值 **panic** 导致 `compute` 未放回（**已定义的可恢复路径**，N8 明确要求「闭包 panic 后本派生返回 `CyclicDerived`，不得作为环的唯一判据」）。断言会把 ② 变成 debug 崩溃 ⇒ 一个闭包 panic 会让引擎在 debug 构建里**永久不可用**，与「panic 后仍可继续服务」的设计冲突。`tests::eval_guard_pops_on_panic_and_cache_survives` 固化了 ② |
| 影响 | 环真被绕过时也返回强类型错误（而不是断言）；`CyclicDerived` 的 `Display` 已覆盖两种来源 |

### S-38 测试落地实测：**87** 个 P03 用例（文档列 82），三处断言按可测性改写

| 项 | 内容 |
|----|------|
| 实测 | `cargo test -p kytrix_gui` = **190 passed**（P01+P02 = 103，P03 = **87**）；`--release` = **184**（6 个 `#[cfg(debug_assertions)]` 用例不编译：`paint/canvas.rs` 2 个 P02 用例 + `paint/scene.rs` 2 + `node/tree.rs` 1 + `node/builder.rs` 1） |
| 分布 | `id` 7、`signal` 13、`dependency` 9、`dirty` 13、`tree` 9、`builder` 6、`style` 3、`scene` 9（另加 P02 的 6 个 `Lane` 用例）、`app` 12、`window` 4、`error` 2（新增 `StaleNode`/`CyclicDerived` 的 Display 与 `all_variants` 覆盖） |
| 比文档多 2 条 | ①`signal::split_borrow_returns_disjoint_views`（`StateStore::split` 的借用契约，B1/B2 的落地证据）；②`app::eval_guard_pops_on_panic_and_cache_survives` 从 `dependency.rs` 移到 `app.rs`（需要读 `App::eval_stack`，那里才有字段访问权） |
| 断言改写 ① | phase-03 §17.4-10 原写「`Canvas`/`Geometry` 节点 `mark_layout` → 父不含 `LAYOUT`」。但 `NodeBuilder::build` **禁止**给叶子父挂子节点（§12.1），该场景不可达 ⇒ 改为谓词级断言 `leaf_kinds_do_not_depend_on_children`（叶子 `depends_on_children == false`，且它们自己的 `mark_layout` 仍标本节点） |
| 断言改写 ② | phase-03 §17.8-5 原要把「超过 `MAX_CLIP_DEPTH` 的 `#[should_panic]`」放在 `scene.rs`；P02 的 `paint/scene.rs` 已有 `depth_cap_is_debug_assert` 覆盖同一断言 ⇒ P03 的用例只断言 `Scene::push` → `Lane::push` 的转发与 clip 图元计数，不重复断言上限 |
| 断言改写 ③ | phase-03 §17.6-4 原写「`refresh_styles` 后 `style_patch()` 是闭包算出的值」——那是 `App` 级行为，`builder.rs` 拿不到 `App` ⇒ 用例改为断言「闭包确实存进节点、build 不预求值」，`refresh_styles` 的求值面由 `app.rs` 的 5 个用例覆盖 |

### S-39 代码审查期（A/B）新增与替换的用例清单

| 类别 | 用例 | 归属 |
|---|---|---|
| **新增（A-B2，D16/I3 端到端）** | `app::write_isolates_sibling_nodes`（同窗口两节点各读一信号，写 A 后 B 的 `DirtyFlags` 全量不变且 `take_wake_request()` 为真）、`app::write_only_derived_does_not_wake`（只有派生失效 ⇒ 不唤醒，且 `report.wake_requested == false`）、`app::write_marks_across_windows`（两窗口各一读者，写一次两者都脏） | `app.rs` |
| **新增（A-B3/B-N4/A-N3 回归）** | `app::bind_style_panic_clears_ghost_deps`（成功求值 → panic → 再写旧信号不唤醒）、`app::create_window_ids_are_unique_and_monotonic`（1/2/3 且 `windows()` 带出 id）、`app::close_window_unknown_returns_false`、`tree::append_child_marks_moved_subtree_paint`（重挂后整棵子树 `PAINT` 且 `REUSABLE` 被清）、`tree::damage_unions_prev_and_current`、`tree::remove_marks_parent_structure`、`dirty::mark_a11y_keeps_damage` | `app.rs` / `tree.rs` / `dirty.rs` |
| **强化（原断言过弱）** | `dirty::repeated_mark_layout_is_idempotent`（加 `damage == prev ∪ layout` 的内容断言）、`dirty::layout_escalation_marks_fixed_size_node_ancestors`（加两个谓词输入断言 + 早停分支复跑）、`dirty::leaf_kinds_do_not_depend_on_children`（加 auto/fixed/混合三例）、`app::refresh_styles_escalates_layout_when_patch_affects_layout`（`false→true` 与 `true→false` 双向）、`signal::equal_but_interior_mutable_value_is_not_repainted`（改为「不重绘 + 不唤醒」）、`signal::derived_cycle_is_reported_not_panicking`（去掉同义反复，改断言「依赖里没有未读信号」） | 各文件 |
| **语义变更（B-N4）** | `NodeTree::append_child` 重挂时新增 `mark_subtree_paint`（逐节点 `touch(PAINT)` ⇒ 清 `REUSABLE` + 并 damage） | `node/tree.rs` |
| **语义变更（A-N3）** | `NodeTree::touch` 只在 flags 含 `LAYOUT`/`PAINT` 时 `remove(REUSABLE)` + `accumulate_damage()`；`mark_a11y` 不再产生 damage | `node/tree.rs` |
| **语义变更（A-N2）** | `App::refresh_styles` 的「闭包已丢」分支显式退订幽灵边：`graph.replace_deps(owner, ghost_deps, &[])` + `set_deps(∅)` | `app.rs` |
| **API 对齐（A-B3/A-N8）** | `App::windows() -> (WindowId, &Window)`；`next_window_id` 从 1 起（0 非法）；`signal`/`derived`/`transient` 加 `Clone` bound；`Node` 补 7 个公开只读访问器（删 `damage_for_test`/`parent_for_test`）；`LaneId::{index, generation}` 降为 `#[cfg(test)]`；`Arena::len` 用 `saturating_sub` | 各文件 |
| **工程化（A-N4/A-N5）** | `#[cfg(debug_assertions)] fn debug_assert_order_matches_tree`（`order` 必须是 DFS 先序全序），在 `insert_child`/`append_child`/`remove` 末尾调用；`Arena::len` 溢出安全 | `node/tree.rs` / `node/id.rs` |

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

| 项 | 内容 |
|----|------|
| 原文 | P01 §4.5 公开路径表：`HitResult` → `kytrix_gui::event::*`（定义文件 `src/event.rs`） |
| 偏差 | `HitResult` 定义在 **`src/layout/hit_test.rs`**，公开路径 **`kytrix_gui::layout::hit_test::HitResult`**；新增公开类型 **`ClipChain`**（同文件，`HitResult::clip_chain` 的字段类型） |
| 原因 | ①P04 的文件树把命中测试放 `layout/hit_test.rs`（brief 明确），而 `event` 模块（P10）此时不存在，P04 不能为一个路径去建空模块；②`HitResult` 的内容全部是**布局坐标**的产物（`bounds`/`clip_chain`/`local_point`），归属布局更贴合职责；③按 P01 §4.5 自己的判据「公开路径 = 类型实际定义所在的文件路径」 |
| 修订后设计 | phase-04 §4.1 的路径表；P10 落 `event` 模块时**不再**定义 `HitResult`（避免同义类型），只在 `InputEvent` 分发里消费它 |

### F-46 `LayoutBox` 增加 `border: Edges`

| 项 | 内容 |
|----|------|
| 原文 | P03 §11.3：`LayoutBox { bounds: Rect }`（「P03 只有 `bounds`」，`baseline`/其他字段由后续 Phase additive 加） |
| 偏差 | 增加 **`pub border: Edges`**（来源 taffy `Layout.border`，四边宽）；`LayoutBox` 从 16 B → **32 B**（probe 实测） |
| 原因 | `Style.clip = Clip::Bounds` 的裁剪盒必须是 CSS 的 **padding box** = `bounds.inset(border)`。没有 `border` 就没有 P04 的正确 clip 链（只能退化成「裁到 border box」，差一个边框宽的系统性错误）；P06 的背景/圆角内缩同样需要它 → 不是预先设计，是 P04 的真实读者 |
| 修订后设计 | phase-04 §8.1；P03 的结构体字面量构造点（仅测试）随之改成 `LayoutBox { bounds, border }`；`content_size` / `baseline` 仍推迟（V-33 / P05） |

### F-47 **不实现** `RoundTree` / `TraverseTree` / `PrintTree`

| 项 | 内容 |
|----|------|
| 原文 | phase-04 brief 要求为 `NodeTree` 实现 8 个 taffy trait（含 `RoundTree`） |
| 偏差 | 只实现 **7 个**：`TraversePartialTree` / `LayoutPartialTree` / `LayoutContainingBlock` / `CacheTree` / `LayoutFlexboxContainer` / `LayoutGridContainer` / `LayoutBlockContainer`。`RoundTree` / `TraverseTree` / `PrintTree` **不实现** |
| 原因 | ①`round_layout` 的唯一调用点是 `TaffyTree::compute_layout_with_measure`（`tree/taffy_tree.rs:976-981`），引擎走低级 `compute_root_layout`，**永不圆整**（P01 §7.5 / F-17）；②`RoundTree` 的 `set_final_layout` / `hoisted_child_count` / `get_hoisted_child_id` 只被 `round_layout_inner` 读（`compute/mod.rs:379-435`）⇒ 落地即无消费者（AGENTS §1.2）；③落地代价是每节点 `final_layout: Layout` = **92 B**（probe 实测），把 `Node` 从 ≈796 B 推到 888 B，逼近 1024 预算（V-30/V-31） |
| 证据 | probe **能**实现（8 个 trait 全部编译通过、40 个用例全绿）→ 这是**决策**，不是能力限制；将来引入「按物理网格圆整」或 `print_tree` 调试时 additive 补 `final_layout` 字段 + 1 个 impl 即可 |
| 修订后设计 | phase-04 §5.3 的决策表 + §16-7 的门禁 grep（`round_layout|final_layout|impl RoundTree` 必须无输出） |

### F-48 `LayoutContainingBlock` 的 hoisted 列表**必须存储**（taffy 文档只提到 `round_layout` 消费它）

| 项 | 内容 |
|----|------|
| 原文 | taffy `tree/traits.rs:236-239`：「The recorded lists are consumed by `round_layout` … and are also useful for consumers implementing paint/hit-testing traversals」 |
| 偏差 | 引擎**必须**存 `hoisted: HashMap<NodeId, SmallVec<[NodeId; 2]>>`，且它的**唯一读者是 P04 的绝对坐标 post-pass**（不是 `round_layout`） |
| 原因 | 出流盒的包含块**可以不是它的树父**（CSS：最近的定位祖先；`position: fixed` 甚至恒定是 root）。taffy 给该盒写的 `Layout.location` 相对**包含块 border box**，而引擎的树 DFS 从树父推出不来这个参照系。`add_hoisted_children` 是 taffy 唯一权威地告诉我们「谁是谁的包含块」的地方。probe 用例 `out_of_flow_child_hoisted_to_ancestor_uses_containing_block_origin` 固化了这条（实测 (5,7)：容器 padding 10 但 taffy 的 `absolute_position_offset = border`） |
| 修订后设计 | phase-04 §7.3-A2 / §8.3；hoisted 的三处清理点在 §6.1 的表里 |

### F-49 根节点默认 `size: 100% × 100%`

| 项 | 内容 |
|----|------|
| 原文 | 上游文档没有任何「根尺寸」的规定（只假设「窗口尺寸 = 根可用空间」） |
| 偏差 | `NodeTree::new()` 把根的 `style.inner.size` 写成 `Dimension::percent(1.0)` × 2 |
| 原因 | `compute_root_layout` 只接受 `available_space`，**不会**把它强加给根（`compute/mod.rs:188-324` 的 `known_dimensions` 只对 block 根自动 stretch）。probe 实测：窗口 200×200 + 根 `size: auto` ⇒ 根 **0×0**、`width: 100%` 的子节点全 0。不修这个，所有依赖「根 = 视口」的用例（命中、damage、百分比布局）都是错的 |
| 修订后设计 | phase-04 §8.4；根默认撑满、应用可改但不该改（rustdoc 写明） |

### F-50 `MeasureFn` 的输入用 taffy `Size<Option<f32>>` / `Size<AvailableSpace>`，输出用引擎 `Size`

| 项 | 内容 |
|----|------|
| 原文 | phase-04 brief 建议 `Box<dyn Fn(Size<Option<f32>>) -> Size<f32>>`（只给已知尺寸） |
| 偏差 | `pub type MeasureFn = Box<dyn Fn(taffy::Size<Option<f32>>, taffy::Size<taffy::AvailableSpace>) -> Size<f32>>` |
| 原因 | ①文本换行必须区分 `Definite(w)` / `MinContent` / `MaxContent`（固有尺寸测量），只给 `known_dimensions` 表达不了（`PerformLayout` 时 `known` 恒 `None`）；②形状与 taffy `compute_leaf_layout` 的 `MeasureFunction` **逐字一致**（`compute/leaf.rs:20-27`），适配是零成本转发；③引擎 `Size` 按 P01 §6.3 冻结为纯 `f32`，无法表达 `Option`/三态；`taffy` 类型已在公开面（F-35 先例） |
| 修订后设计 | phase-04 §9.1 / §9.2；`Box<dyn Fn>`（不是 `FnMut`）是「measure 纯函数」的类型级保障 |

### F-51 `InvalidMeasure` 的实现 Phase 改为 **P04**；`LayoutInvalid` 不落地

| 项 | 内容 |
|----|------|
| 原文 | P01 §6.2.2：#9 `InvalidMeasure` 与 #10 `LayoutInvalid` 的实现 Phase 都是 **P05** |
| 偏差 | #9 在 **P04** 落地（payload 逐字不变）；#10 **不落地**（继续留空） |
| 原因 | ①measure 适配（`compute_leaf`）在 P04 落地，违规检测的唯一生产点就在这里（`compute/leaf.rs` 会把 NaN 夹掉/被 padding 放大，必须在闭包返回值上检测）；②#10 没有生产者：taffy 低级 API 全部返回 `LayoutOutput`、不返回 `Result`，唯一的后置校验就是 measure 违规，而它已有更精确的变体（AGENTS §1.2「不留占位」） |
| 修订后设计 | phase-04 §14.1 / §14.2；`error.rs` 的 `all_variants()` 增加 #9 |

### F-52 `calc()` 不解析（`resolve_calc_value` 走默认 `0.0`）

| 项 | 内容 |
|----|------|
| 原文 | P01 §5.3④ 把 `calc` 列进 taffy 的冻结 features；上游设计未提 calc 的求值 |
| 偏差 | 引擎**不实现** `resolve_calc_value`（走 trait 默认实现，恒返回 `0.0`）；`calc()` 值布局为 0 |
| 原因 | `taffy::style::Calc(*const ())` 是**不透明指针**（`style/dimension.rs:106-109`），只暴露 `calc_value() -> *const ()`，taffy 0.15 没有公开求值器；taffy 自带的 `TaffyView` 同样返回 `0.0`（`tree/taffy_tree.rs:400-403`）。要真支持 calc 就得 fork taffy 或自建 calc arena（P05 再评估） |
| 影响 | 应用层不得使用 `Dimension::Calc` / `LengthPercentage::Calc`；`Style.inner` 的 rustdoc 写明；不新增错误变体（没有运行时可判定的信号） |

### S-40 `NodeTree::insert_child` 收 `NodeSpec`（私有字段 + `new`）

| 项 | 内容 |
|----|------|
| 决策 | `insert_child(&mut self, parent: NodeId, spec: NodeSpec)`；`NodeSpec { kind, style, measure, layout_override, hit_shape }` **字段全私有** + `NodeSpec::new(kind, style, measure, layout_override, hit_shape)` + `Default`（= boxed 空部件） |
| 原因 | `clippy::field_scoped_visibility_modifiers = "forbid"`（workspace 根）**禁止字段带 scoped 可见性修饰**（`pub(crate)`/`pub(super)`/`pub(in …)`）：probe 实测 5 个 `pub(crate)` 字段全部报 `scoped visibility modifier on a field`。部件结构体因此不能有 `pub(crate)` 字段；备选是「6 个位置参数」（`too_many_arguments` 已 allow），但调用点可读性更差 |
| 影响 | 纯 crate 内部签名；调用点只有 `NodeBuilder::build`（与 P03 的 F-43 同源） |

### S-41 布局入口 = `Window::compute_layout()`；`NodeTree::compute_layout` 是 `pub(crate)`

| 项 | 内容 |
|----|------|
| 决策 | `pub fn Window::compute_layout(&mut self) -> Result<LayoutReport, EngineError>`；`pub(crate) fn NodeTree::compute_layout(&mut self, available: taffy::Size<Option<f32>>)`；**不新增** `App::layout` |
| 原因 | ①`App` 在这件事上只贡献「窗口查找」，`Option<Result<…>>` 是双层失败语义（窗口不存在 / measure 违规），调用方每次都要拆两层；②根可用空间**就是**窗口逻辑尺寸（P12 写入 `size`），不需要第二个参数；③`NodeTree::compute_layout` 需要 taffy 的 `Size<Option<f32>>`，不该进公开面 |
| 交接 | P06/P09 的帧驱动：`app.refresh_styles(w)` → `app.window_mut(w).unwrap().compute_layout()?` → paint |

### S-42 taffy `Cache` 旁表 = `Vec<Option<Cache>>`（按 arena 槽位下标）

| 项 | 内容 |
|----|------|
| 决策 | `NodeTree.layout_cache: Vec<Option<taffy::Cache>>`，与 `slots` 等长；三处清理：`insert_child` 复用槽位、`remove` 逐节点、新增槽位 push `None` |
| 原因 | 节点树是**稠密**的（taffy 给每个参与布局的节点建缓存）；`Cache` 实测 **408 B** ⇒ 10k 节点 ≈ 4.2 MB，可算；`Vec` 无哈希（布局顺序确定、帧记录可复现）；槽位复用是我们自己的不变量（一个 `= None` 够） |
| 备选（已否） | `ahash::HashMap<NodeId, Cache>`：命中率无优势（稠密），条目控制字更大，且两套生命周期要两处清理 |
| 影响 | `Node` 不内联 `Cache`（P03 N6 的预算约束），`size_of::<Node>()` 断言仍成立（V-31 跟踪） |

### S-43 `LAYOUT` 的唯一消费点是 `set_unrounded_layout`；`prev_layout` / damage 在 post-pass 更新

| 项 | 内容 |
|----|------|
| 决策 | `set_local_layout`（= `LayoutPartialTree::set_unrounded_layout` 的落点）里 `slot.dirty.remove(LAYOUT)` + 记 `laid_out_epoch`；`absolutize` 在绝对盒变化时写 `prev_layout = old`、`layout = new`、`damage ∪= old ∪ new` |
| 原因 | 「被 taffy 排过」= `LAYOUT` 已满足，这是唯一能精确判定的时刻（一个节点可能在一轮里被写多次）；damage 的新旧并集必须在**绝对坐标**上做（P03 的 `touch` 并的是上一帧的绝对盒） |
| 已知空位 | 布局叶子的子节点永不被布局 ⇒ 它们的 `LAYOUT` 永不清（V-32；仅 release 可达） |

### S-44 `Style.clip: Clip { None, Bounds }`：只裁子树、裁到 padding box、不读 taffy `overflow`

| 项 | 内容 |
|----|------|
| 决策 | 见 phase-04 §11 |
| 原因 | ①CSS `overflow: hidden` 的裁剪盒是 padding box（需要 `LayoutBox.border`，F-46）；②裁剪**不裁自己的 border/background**（probe 固化：边框带命中自己）；③taffy 的 `Overflow` 同时改布局（滚动条槽、可滚动溢出矩形），与「裁剪」是两件事，混用会让「设了 overflow 就悄悄改了布局」 |
| 推迟 | 圆角（`Clip::Rounded`，P06）；滚动偏移（P10 在 `clip_chain` 的绝对矩形上平移） |

### S-45 `hit_test` = 逆绘制序 DFS + 布局盒剪枝；`Overlay` 无特殊分支；`ClipChain` 定长 8

| 项 | 内容 |
|----|------|
| 决策 | 见 phase-04 §12 |
| 原因 | ①`order` 是 DFS 先序 ⇒ 兄弟**逆序** + 先子树后自身 = 与绘制序严格互逆（z 序天然正确）；②布局盒剪枝把 O(n) 降到 O(深 × 兄弟)；③`Overlay` 的「最高优先」= 排在 `order` 末尾，P13 不需要在 hit test 里加分支（probe 用「末尾的小浮层压过大底层」固化）；④`ClipChain` 定长 `Copy`（136 B）避免把 `smallvec` 版本写进公开 API，也与 `MAX_CLIP_DEPTH = 8` 对齐 |

### S-46 `LayoutReport` 的四个字段与计数语义

| 项 | 内容 |
|----|------|
| 决策 | `{ laid_out, moved, cache_hits, cache_misses }`；`laid_out` 只在 `PerformLayout` 且 cache 未命中时 +1；`cache_hits/misses` 在 `cache_get` 里计；`moved` 在 post-pass 里计 |
| 实测基线 | 首轮 4 节点 ⇒ `laid_out = 4`；改 1 个子节点 ⇒ `{ laid_out: 2, moved: 2, cache_hits: 4, cache_misses: 4 }`；无变化 ⇒ `{ laid_out: 0, moved: 0, cache_hits: 2, cache_misses: 0 }` |
| 原因 | 「只重算脏子树」必须**可断言**（L1 的增量判据）；`cache_hits` 会随 taffy 的 measure pass 次数浮动 ⇒ 用例只断言方向（`> 0` / `== 0`），不对精确值断言 |
| 不含 damage | damage 跨帧累积（P03 §9.4），P06 在帧初清零并物理化；放进报告会诱使调用方把它当「本帧 damage」 |

### S-47 不建独立 measure 缓存；`invalidate_measure` 是唯一失效入口

| 项 | 内容 |
|----|------|
| 决策 | 没有 `(measure_version, available_space)` 缓存、没有 `measure_version` 字段；`pub fn NodeTree::invalidate_measure(node)` = 标 `LAYOUT` + 清 `node..root` 的 taffy 缓存 |
| 原因 | taffy `Cache` 的 key 已含 `(known_dimensions, available_space, sizing_mode, run_mode, parent_size, mode)`（`tree/cache.rs:254-280`），probe 实测「第二轮 measure 零调用」；唯一缺的输入是「闭包捕获的状态变了」，那是一个**失效事件**（`invalidate_measure`），不是一个版本号字段（无读者的字段按 AGENTS §2.1 不加） |
| 交接 | P05 的字体栈/文本内容变化 → 对受影响文本节点调 `invalidate_measure` |

### S-48 每类型只能有一个 inherent `impl` 块；trait impl 全在 `layout/taffy_tree.rs`

| 项 | 内容 |
|----|------|
| 事实 | `clippy::multiple_inherent_impl = "forbid"`（workspace 根）；probe 实测：同一类型两个 inherent impl 块（**即使同文件**）→ `error: multiple implementations of this structure` |
| 决策 | `NodeTree` 的全部 inherent 方法（含 `compute_layout`/`absolutize`/`hit_test`）写在 `node/tree.rs` 的**那一个** `impl NodeTree { … }` 里；`layout/taffy_tree.rs` 只放 `impl <Trait> for NodeTree` + 自由函数（`run`/`dispatch_child_layout`/id 映射） |
| 门禁 | phase-04 §16-10：`layout/taffy_tree.rs` 的 `^impl ` 必须是 7 行 trait impl；`node/tree.rs` 的 `^impl NodeTree {` 恰好 1 行 |

### S-48b 结构性样式变更的公开入口 = `set_style`（取代提案期的 `apply_style`）

| 项 | 内容 |
|----|------|
| 决策 | `pub fn NodeTree::set_style(&mut self, node: NodeId, style: Style) -> Result<(), EngineError>`；写值 + 标 `LAYOUT|PAINT` + 清 `node..root` 的 taffy 缓存；陈旧句柄 ⇒ `Err(EngineError::StaleNode { node })` |
| 原因 | 审查 B1：提案期的 `apply_style` 是 `pub(crate)` 且**没有任何非 test 消费者**（`NodeBuilder` 只在挂载时写 style，`Window` 只标脏）⇒ 违反「`pub(crate)` 必须有非 test 消费者」。提升为 `pub` + 返回 `Result` 后，它是应用层「结构性样式变更」的正式入口（P08 的主题/密度切换），并给 `StaleNode` 增加一个真实生产者 |
| 文档 | `set_style` 的 rustdoc 必须写明「**动态属性请用 `bind_style`**」：每帧调它会清整条祖先链的缓存，把增量布局退化为全量 |
| 登记 | phase-04 §4.1 的「方法级公开面增量」表 + §4.3 的预留清单 + §8.5 的签名 |

### S-49 `LayoutOverride` 的 builder 副作用 + `set_layout_override` 写入口

| 项 | 内容 |
|----|------|
| 决策 | `NodeBuilder::layout_override(rect)` **强制** `style.inner.position = Position::Absolute`；挂载后用 `NodeTree::set_layout_override(node, Option<LayoutOverride>) -> bool` 更新（写值 + 标 `LAYOUT|PAINT` + 清 `node..root` 缓存） |
| 原因 | ①出流是「位置与尺寸都由应用定」的必要条件（父不留空间），由 builder 强制比要求调用方记得设更可靠；②覆盖矩形必须在挂载后可改（分栏拖拽），而它是布局输入 ⇒ 与 `set_style`/`invalidate_measure` 同一形状的「输入变了」入口 |
| 语义细节 | `rect` 相对**父 border box**；它是节点的 **border box**（不再过 min/max/padding）；节点是布局叶子（`build` 有 debug 断言） |

### S-50 `get_detailed_layout_info` / `set_detailed_grid_info` 走默认实现

| 项 | 内容 |
|----|------|
| 决策 | 不存 `DetailedGridInfo<SmolStr>`，不覆写这两个方法 |
| 原因 | 没有读者（P04 的绝对坐标 post-pass 只用 hoisted 列表）；代价是 **grid 容器内**的 absolute 子盒按 padding box 而非 grid area 定位（taffy 的默认行为，`tree/traits.rs:246-254`） |
| 复核 | V-35；若 P08 出现「absolute 子盒按 grid area 定位」的真实需求，additive 存 `DetailedGridInfo<SmolStr>`（每 grid 容器一份 Box） |

### S-51 测试与尺寸的落地实测（P04）

| 项 | 计划 | 实测 |
|---|---|---|
| 新增 `#[cfg(test)]` 用例 | 60（phase-04 §15） | **71**（`layout/taffy_tree.rs` 25、`layout/hit_test.rs` 19、`layout/measure.rs` 10、`node/tree.rs` 9、`node/builder.rs` 3、`window.rs` 4、`node/dirty.rs` 1、`error.rs` 1、`paint/style.rs` 1、`node/id.rs` 1） |
| `cargo test -p kytrix_gui` | — | **271 passed**（P03 结束 200 + 71） |
| `cargo test -p kytrix_gui --release` | — | **264 passed**（7 个 `#[cfg(debug_assertions)]`/debug-only 用例不编译） |
| `size_of::<Node>()` | ≤ 1152（§8.3） | **1016 B**（P03 是 888 B，+128 B：40 B 字段 + 88 B 对齐/填充） |
| `size_of::<LayoutBox>()` | 32 B | **32 B** |

> 74 个用例全部通过；`node_and_handle_sizes` 打印实测值，`set_style`/`set_layout_override`/`invalidate_measure`
> 的 `StaleNode` 错误路径、`node_debug_prints_layout_fields`（N4 的 Debug 同步）、`clip_chain` 不含目标自身
> （N3）、`visible=false` 不参与命中（N9）、`change_below_fixed_size_ancestor_still_relayouts`（N14 的
> `cache_hits > 0` + 未受影响子树不动）都有专门用例。

### S-52 代码审查（PASS-WITH-FIXES）后的三处语义修正

| 项 | 修正 |
|---|---|
| **B1 `LayoutOverride` 参照系** | taffy 把 `position: absolute` 的 override 节点挂到最近的**定位**祖先（无则 root/ICB），其 `local` 因此相对**包含块**；引擎的 override 契约是「相对父 border box」。修正：`absolutize` 的 children 循环不跳过 override 子节点（用父 `abs.origin + override.rect`），hoisted 循环跳过它们。用例：父被 spacer 推到 y=40/100 ⇒ override 绝对 (10,60)/(10,120)、命中、父移动后跟随 |
| **B2 hit test 剪枝** | 布局盒**不能**作为子树剪枝依据（出流子节点可溢出父盒）；唯一安全的剪枝是 `Clip::Bounds` 祖先的 padding box。修正：children 循环只跳过「叶子 + 盒外」，自身命中增加 `bounds.contains(point)` 前置条件。用例：无 clip 父 + 溢出子命中；有 clip 父 → 不命中（落 root）。代价记 V-38 |
| **N1/N3 幽灵盒** | 隐藏子树（自身或祖先 `display: none`）的所有后代（含 override 与出流盒）强制零化；`dispatch_child_layout` 里 `display:none` 判断**先于** override 分支。用例：隐藏父的出流后代 / 隐藏的 override 节点必须 `bounds.is_empty()` |
| **N2 `FlowRoot`** | `Display::FlowRoot` 的 `block_ctx` 透传改为 `None`（与 taffy `TaffyView` 一致） |
| **N5 `hoisted` 清理** | `remove` 除删除被卸载节点自身的列表外，还 `retain` 掉**所有**「值里包含被卸载 id」的旧列表项 |
| **N6 兄弟用例** | 改为完全重叠的绝对定位兄弟，断言后者胜出（原用例按布局盒剪枝，B2 后语义已变） |

### 九·待复核项（P04 新增）

| 编号 | 内容 | 复核 Phase | 触发条件 / 处置 |
|---|---|---|---|
| V-31 | `size_of::<Node>()`：P04 新增 40 B（`local` 16 + `local_border` 16 + `epoch` 4 + padding）。probe 的等价节点 = 888 B（含 92 B probe-only `final_layout`）⇒ 去掉后 ≈796 B | P04 实现首日 | 复跑 `node_size_within_budget`；超 1024 → `style: Box<Style>`（P03 V-30 的备选，公开面不变） |
| V-32 | 布局叶子（`Canvas`/`Geometry`/`LayoutOverride`）的子节点永不被布局 ⇒ 它们的 `LAYOUT` 位永不清（`StylePassReport.layout_dirty` 可能长期为真） | P06 | 若影响「有无脏位」判定：在 `remove`/`begin_frame` 里对不可达子树清位 |
| V-33 | damage 覆盖：祖先盒不含**出流溢出**的后代时，只按祖先 AABB 累积 damage 会漏 | P06 | 给 `LayoutBox` additive 加 `overflow: Rect`（taffy `Layout.scrollable_overflow_rect`）并并入 damage |
| V-34 | taffy 的 `absolute_position_offset = border`（不含 padding），与 CSS「padding box 定位」有差异 | P08 迁移 | 引擎照抄不重算（`compute/flexbox.rs:511`）；应用层文档写明「absolute 子节点的 inset 相对 border box」 |
| V-35 | 不实现 `get_detailed_layout_info` ⇒ grid 容器内的 absolute 子盒按 padding box 定位 | P08 / P13 | 出现真实需求 → additive 存 `DetailedGridInfo<SmolStr>` |
| V-36 | `ClipChain` 定长 8：超过 8 层的 clip 链被截断（点仍按前 8 层裁剪） | P10 | 与 `MAX_CLIP_DEPTH` 同一上限（shader 固定数组）；`Lane::push` 的 debug 断言先行暴露 |
| V-37 | 增量正确性依赖「结构变更也清缓存」：`append_child`/`remove` → `mark_structure` → 清 `node..root` | P04 实现 | 用例 `remove_drops_cache_and_hoisted_entries` 固化；P03 的 `mark_structure` 调用点已就位，只需内部接上 `invalidate_caches_upwards` |

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

| 项 | 内容 |
|----|------|
| 原文 | 主设计 §4.5（928 行）：「缓存键：`text + color + font_size + TextStyle + scale + font_stack_version + atlas_generation`」 |
| 偏差 | 拆成两级：**形状缓存**键 = `(text hash, TextStyle hash, scale, font_stack_version)`，**不含** `atlas_generation`；`atlas_generation` 只进**字形 tile 缓存**键与 `ShapedLine.atlas_generation` 字段（供 `AtlasTile::is_stale` 判定） |
| 原因 | ①字形 id、advance、baseline、行数全部与图集无关（phase-05 probe ⑤：同一文本的 5 个 run 只取决于字体与 scale）；②`generation` 只在 `clear()` 时 +1（S-11），而 `clear()` 的触发之一是**容量逃逸**——若形状键含它，一次图集满会把**所有**文本 re-shape（`RangedBuilder::build` + 断行），代价远大于重光栅；③P02 §10.6/§3.8 的目标语义是「图集重排 → 依赖节点 `PAINT`」，不是 `LAYOUT` |
| 修订后设计 | phase-05 §7.2 的两级键表 + §10 的 `GlyphKey { font, glyph, ppem }`；`ShapedLine.atlas_generation` 是唯一把代际带回形状快照的字段（P06 用它判 stale） |

### F-54 `parley::TextStyle` **存在但 P05 不用**；`TextContext` 不公开

| 项 | 内容 |
|----|------|
| 原文 | 主设计 §4.5（893 行）「**Parley 0.12 无 `TextStyle`**，样式单元是 `StyleProperty<'a, B: Brush>`（26 变体）」；同节 898–902 行又给出 `pub struct TextContext { pub font_cx: parley::FontContext, pub layout_cx: parley::LayoutContext, pub font_stack: FontStack }` |
| 偏差 | ①`parley::style::TextStyle<'family,'settings,B>` 在 0.12.0 **真实存在**（`style/mod.rs:284`，26 个 pub 字段；F-03 已判同一事实）——但它是 `StyleRunBuilder::push_style`（`builder.rs:136`）与 `TreeBuilder::push_style_span`（`builder.rs:237`）的「整段样式」输入，`RangedBuilder` **没有**接受它的方法（`push_default` 只收 `impl Into<StyleProperty>`）；P05 走 `RangedBuilder` + `StyleProperty`，**不**使用 `TextStyle`。②`TextContext` **不公开**（crate 私有，`pub(crate)`） |
| 原因 | ①P05 的样式是单段平铺（一个节点一个 `TextStyle`），`RangedBuilder` 的 26 个变体足够；`TreeBuilder` 还要自管文本缓冲与 style span 栈，属 Markdown 管线（P15）的形态，现在引入等于两套代码路径（F-03 的「二者取一，不并存」）。②`TextContext` 的字段必然是 `parley::{FontContext, LayoutContext}`，公开即把第三方 crate 拉进公开面（F-35 的同类事件，但更重：`Collection` 的每个方法都要 `&mut self`，公开字段等于把「谁能 shape」交给应用层）；且公开后应用可绕过 `App::refresh_text` 直接 shape，与「`App` 持有字体库/图集」的所有权冲突 |
| 修订后设计 | ①phase-05 §7.1 的逐字段映射表（`TextStyle → StyleProperty`），`ranged_builder(fcx, text, scale, quantize=true)` 5 参，方向用 `set_base_direction`（F-03 的第 5 参裁决在本阶段落地实现）。②公开面 = `kytrix_gui::text::{TextStyle, FontStack, InlineBoxSpec, ShapedLine, ShapedTextLine, ShapedTextItem, ShapedGlyphRun, ShapedInlineBox, TextContent, TextPassReport, TextDiagnostic}` + `App::{font_stack, font_stack_version, set_font_stack, reload_system_fonts, refresh_text, text_diagnostics}`；`TextContext` 不在公开面，phase-01 §4.5 的路径表**无需**新增行。**「无 `TextStyle`」这句按 F-03 视为事实错误，本节不改主设计正文** |

### F-55 `AtlasExhausted` 的落地 Phase 由 P09 改为 **P05**

| 项 | 内容 |
|----|------|
| 原文 | P01 §6.2.2 #12：「`AtlasExhausted { kind: AtlasKind }`，实现 Phase **P09**」；P02 §10.6 又说「该变体随分配器在 P05 追加」 |
| 偏差 | 在 **P05** 落地（payload 逐字不变），P09 只做消费者 |
| 原因 | 唯一生产者是 `AtlasAllocator::allocate` → `None` 的第二次重试（P02 §10.6 的容量失败路径），而分配器按 P02 裁决 ① 在 P05 落地（phase-05 §9.5）；P09 不产生该错误。与 F-51（`InvalidMeasure` 由 P05 提前到 P04）同一类修订 |
| 修订后设计 | `error.rs` 加变体 + `all_variants()` 一条；phase-05 §9.5 的失败路径逐字；P01 表 #12 的「实现 Phase」列以本条为准，正文不改 |

### F-56 `invalidate_measure` **不**标 `PAINT`（S-47 的语义缺口）：文本 pass 显式补

| 项 | 内容 |
|----|------|
| 原文 | 主设计 §4.6（939 行）：「系统字体安装/卸载、深浅色切换、语言切换 → `font_stack_version += 1` → 所有文本节点 **`LAYOUT + PAINT`**」；S-47：「P05 的字体栈/文本内容变化 → 对受影响文本节点调 `invalidate_measure`」 |
| 偏差 | `NodeTree::invalidate_measure`（P04 已落地）只标 `LAYOUT` + 清 `node..root` 缓存，**不**标 `PAINT`（`node/tree.rs:663` 走 `invalidate_layout`）。因此仅调它会让 §4.6 要求的 `PAINT` 丢失 |
| 原因 | 两条上游要求不等价：`invalidate_measure` 的语义是「布局输入变了」，`PAINT` 的语义是「像素变了」。文本/字体变化**两者都**成立 |
| 修订后设计 | 文本 pass 用 `NodeTree::invalidate_text_layout(node)`（= `invalidate_measure` + `mark_paint`，phase-05 §13.1/§6.3），`pub(crate)`；字体栈切换遍历所有文本节点调它。`invalidate_measure` 本身**不改**（P04 的公开语义保持） |

### F-57 `parley` 必须开 `complex-scripts` feature

| 项 | 内容 |
|----|------|
| 原文 | P01 §5.1 只钉 `parley = "0.12.0"`（默认 features）；主设计 §4.5 说断行由 ICU4X 分析 |
| 偏差 | `[workspace.dependencies] parley` 改为 `features = ["complex-scripts"]` |
| 原因 | 不开时 `parley_engine` 的 `word_segmenter()`/`line_segmenter()` 走 `new_for_non_complex_scripts`（`analysis.rs:110-116`、`:153-162`），ICU4X 没有 CJK 词典模型（`icu_segmenter-2.4.0/src/complex/mod.rs:183`），probe 每段中文都向 stderr 打 `ICU4X data error: No segmentation model for complex script: Chinese/Japanese`，并退化到 UAX #29 WB999 的「有无字典都按字符边界」路径（`analysis.rs:681-686`）；`has_word_dictionary() = cfg!(feature = "complex-scripts")`（`analysis.rs:854-855`） |
| 修订后设计 | phase-05 §17：只改 feature，**不新增依赖**（`Cargo.lock` 不变）；门禁 §16-12 用 `cargo tree -e features` 断言该 feature 生效 |

### S-53 `NodeKind::Text` 的载荷**内联**（`Node` **1088 B**），不做 `Box<TextContent>`

| 项 | 内容 |
|----|------|
| 决策 | `NodeKind::Text(TextContent)`，`TextContent { text: SmolStr, style: TextStyle }` **内联**；`Node.text_bind: Option<Box<dyn Fn(&mut ReadCtx<'_>) -> SmolStr>>`（**胖指针 16 B**）+ `Node.text_measured_size: Size`（8 B）；`Rc<ShapedText>`/`Rc<ShapedLine>` 放 `NodeTree` 的**旁表** `text_slots: Vec<Option<TextSlot>>`（16 B/槽，仿 P04 的 `layout_cache`） |
| 原因 | probe ③ 实测：`Node` 1016 B、`NodeKind` 48 B（= `Canvas`）、`TextStyle` 72 B、`TextContent` 96 B、`Option<Box<TextContent>>` 8 B。内联 ⇒ `NodeKind` **96 B**（载荷 96 吃掉了 tag/niche：`Canvas` 的 `Vec` 提供 niche，实测）、`Node` = 1016 − 48 + 96 + 16 + 8 = **1088 B** ≤ P04 的 1152 B 上界（余量 64 B）。`Box` 方案回 1040 B 但每个文本节点多一次分配 + 一层解引用；文本节点在 UI 里是常态而非例外 |
| 代价 | `Node` 从 1016 → **1088 B**（+72 B ≈ 7%）。若 P06/P07 再加字段顶破 1152，改 `NodeKind::Text(Box<TextContent>)` 即可（公开面不变） |
| 实测校正 | `Option<Box<dyn Fn>>` 是 16 B（`Box<dyn Trait>` = data + vtable），初版文档误记为 8 B；`NodeKind` 实测 **96 B**（不是 104：`TextContent` 的 96 B 载荷吃掉了判别位，`Canvas` 的 `Vec` 提供 niche）⇒ `Node` 实测 **1088 B**；P04 的 `node_and_handle_sizes` 断言**本来就是 `<= 1152`**，不存在 1024 的上界 |
| 登记 | phase-05 §14.1、§2.4④；`node_and_handle_sizes` 追加 `size_of::<TextContent>() == 96` 与 `size_of::<Node>() <= 1152` |

### S-54 光栅与图集在 **P05** 落地，`PositionedGlyph` 复用 P02 类型

| 项 | 内容 |
|----|------|
| 决策 | P05 交付「shape → raster → 装箱」的 CPU 全链：`App::refresh_text` 产出 `ShapedLine`，其中每个字形是 P02 的 `paint::primitive::PositionedGlyph { tile, position, size }`（`position` 为**文本盒局部**坐标）。P06 只做 `ShapedLine → Scene` 的实例编码（加节点原点、组 `GlyphRun`、分 lane） |
| 原因 | ①若把光栅/图集推到 P06，`render/atlas.rs` 的 11 个 `pub(crate)` item 只有 test 消费者 ⇒ 违反「`pub(crate)` 必须有非 test 消费者」（P02 §3.2 / P03 §4.3 / AGENTS §4），`-D warnings` 直接红；②`ShapedLine` 的自然形态就是「字形 + 图集矩形 + 落点」，P02 §6.3.2 已经冻结了 `PositionedGlyph` 的字段与语义（40 B 实测），再定义一套「未装箱字形」等于两份真相 |
| 契约细节 | `PositionedGlyph.size` 的契约「`scale * size` 必须是整数」由光栅侧的整数 ppem 保证（phase-05 §8.3）；`position` 在 `ShapedLine` 里是局部坐标，加节点原点后才是 P02 意义的绝对坐标（P06 的责任，phase-05 §5.2 的 rustdoc 写明） |
| 门禁 | `render/atlas.rs` 的每条 `pub(crate)` 必须在 phase-05 §4.3 表里指向 `text/atlas.rs` 或 `App::refresh_text`（非 test） |

### S-55 `NodeTree::text_selection_rects` 返回 `Vec<Rect>`（**不**用 `SmallVec`）

| 项 | 内容 |
|----|------|
| 决策 | `pub fn text_selection_rects(&self, node: NodeId, anchor: usize, focus: usize) -> Result<Vec<Rect>, EngineError>` |
| 原因 | P04 §4.2 已确立「不把 `smallvec` 版本写进公开面」（`ClipChain` 用定长结构就是这条）。选区每行一个矩形，行数一般 ≤ 4，但公开面不为此引入第三方类型 |
| 备选（已否） | 返回 `SmallVec<[Rect; 4]>`（少一次分配）；因公开面纪律否决 |
| 登记 | phase-05 §12.5、§4.1 |

### S-56 空文本 = `Size::ZERO` + 0 行（parley `""` 是**非零**布局）

| 项 | 内容 |
|----|------|
| 决策 | `measure` 对 `text.is_empty()` 直接返回 `Size::ZERO`；`ShapedLine` 对空文本产出 `lines: []`、`width/height: 0`、`first_baseline: None` |
| 原因 | probe 实测（16 px Segoe UI、scale=1）：`text=""` ⇒ `lines()==1`、`width()==4.4453`、`full_width()==4.4453`、`height()==18.0`、`first_baseline()==Some(14.0)`、`items()==0`。即 parley 0.12 对空文本报出一个「有宽度的一行」；直接采信会让空 label 占 4.4 px 宽（布局抖动 + damage 失真）。`" "` 的 4.4453 是真实空格，不受影响 |
| 代价 | 需要保留高度的应用显式给节点 `Style.size.height`（文档写明） |
| 用例 | `empty_text_is_zero_size`（phase-05 §15.1 #7）、`raster_space_is_empty`（§15.3 #22） |

### S-57 `NodeBuilder::bind_text` 返回 `SmolStr`（brief 写的是 `String`）

| 项 | 内容 |
|----|------|
| 决策 | `pub fn bind_text(self, f: impl Fn(&mut ReadCtx<'_>) -> SmolStr + 'static) -> Self`；`Node.text_bind` 同型 |
| 原因 | AGENTS §2.2：短字符串优先 `SmolStr`、禁止 `format!` → `String` → `SmolStr` 的中转；文本节点的值域绝大多数 ≤ 22 B（内联，零分配），`String` 会每次求值分配一次堆 |
| 影响 | 应用层写 `bind_text(|cx| format_smolstr!("…"))`；若仲裁要求 `String`，只在闭包出口做一次 `SmolStr::new(&s)`（多一次分配），设计其余部分不变 |
| 登记 | phase-05 §13.2、§18 裁决项 ② |

### S-58 P05 设计审查（FAIL：B1–B3 + N1–N6）的修正

| 项 | 修正 |
|---|---|
| **B1 形状快照失效纪律** | 任何 shape 快照变化（`text` / `TextStyle` 任一字段 / `scale` / `font_stack_version`）**必须**清 `node..root` 的 taffy 缓存；「测得尺寸是否变化」只决定**额外**置不置 `LAYOUT`（没变 → 只 `PAINT`）。实现：`NodeTree::set_text_shaped(node, shaped, measured, size_changed)` 内**无条件** `invalidate_caches_upwards(node)`。反例用例 `text_wrap_change_same_max_width_relayouts`（`Wrap`→`Anywhere`：`widths.max` 相同、`Definite(40)` 的行数不同）；另 `text_same_size_still_clears_cache`（`"ab"`→`"ba"`：尺寸全同、缓存仍重算） |
| **B2 `FontKey` 稳定性** | `(ptr, len, index)` 不稳定：`ranged_builder` 每次都 `source_cache.prune(128, false)`（`parley/context.rs:109`），Blob 释放后同地址可被复用。修正：`FontEntry { blob: parley::FontData }` **pin 住** `Blob` 的 `Arc`，键改为 `(ptr, len, ttc_index, font_epoch)`；`set_font_stack` / `reload_system_fonts` 时 `font_epoch += 1` + 清 `GlyphRasterizer::fonts` + 清 `GlyphAtlas` + `AtlasSet::glyph_mut().clear()`（字形 tile 不跨字体代际复用） |
| **B3 `Node` 尺寸与断言** | `Option<Box<dyn Fn>>` 是 **16 B**（初版误记 8）⇒ `Node` = **1088 B**（实测；`NodeKind` 96 B，载荷吃掉判别位）。P04 的 `node_and_handle_sizes` 断言本来就是 `<= 1152`，无需改动；phase-05 §0/§14.1/§16-13 的尺寸表已按实测回填，并追加 `size_of::<TextContent>() == 96`、`size_of::<NodeKind>() == 96` 打印 |
| **N1 tofu 断言** | 改为「`tofu == 0` **或** `diagnostics` 含 `FontStackMiss`」；宿主无 CJK/emoji 字体时 `eprintln!` 一条 skip 说明，不 panic（防 CI 假绿/假红） |
| **N2 借用** | `refresh_text` 用**索引循环**（`while index < tree.order().len()`），避免 `for node in tree.order()`（持 `&NodeTree`）与 `set_text_shaped`（要 `&mut`）冲突；`App` 侧先按字段解构再 `windows.get_mut` |
| **N3 命名** | 只有 `Window::scale_epoch()` 与 `App.scale_epochs`；判定用 `entry.or_insert(0)`，无 `text_scale_epoch` 字段 |
| **N4 哈希** | `ShapeKey` 的 `family` 用 `Option<&str>`（`SmolStr::as_str()`）参与 hash，不做 `SmolStr` 的 `Hash` 依赖 |
| **N5 槽位复用** | `text_slots` 与 `layout_cache` 同生命周期：`insert_child` 复用槽位时 `resize_with` 后置 `None`，`remove` 对整棵子树逐个置 `None`（否则新节点继承上一代的 `Rc<ShapedText>`） |
| **实现期实测（2026-11-12）** | `Node` = **1088 B**（`NodeKind` 96 B 吃掉判别位）；`cargo test` = **356**、release = **348**、clippy/fmt/doc 全绿；`take_dirty_rows`/`deallocate` 因「v1 无逐 tile 逐出」降为 `#[cfg(test)]`（P09 恢复），脏行的生产读面改为 `dirty_span_count()`；`GlyphAtlas::ensure` 返回 `GlyphTile`（tile + swash `left/top/w/h`，只给 `AtlasTile` 算不出落点）；`refresh_styles` 在文本变更后**重新补 `PAINT`**（`clear_paint` 会吞掉它） |
| **N6 版本来源** | 注明 `parley` 的 `pub use fontique`（`lib.rs:109`）与 `kytrix_gui` 直依赖的 `fontique` 都是 **0.12.0**，全仓单一实例（`Collection` 与 `parley::FontContext.collection` 同型） |

### 十·待复核项（P05 新增）

| 编号 | 内容 | 复核 Phase | 触发条件 / 处置 |
|---|---|---|---|
| V-38 | **（P04 欠账补齐，S-52 的 B2 行许诺「代价记 V-38」）** 命中测试**不能**用布局盒做子树剪枝后，最坏情况是整树遍历：每个节点都要做 `bounds.contains(point)` + clip 祖先链检查，O(n) 而非 O(命中路径) | P10（事件分发）/ P13（Overlay）/ P14（a11y hit test） | 若高频 hover 的树规模 > 5k 节点且命中耗时进入 L4 基准：additive 加「按层/按浮层的空间索引」（不改 `HitResult` 契约）；在此之前靠 P04 的「叶子 + 盒外」与 clip 剪枝（S-52 B2）兜住 |
| V-39 | 长文本 measure 的 `clone + rebreak` 成本：16000 字符 **288 µs/次**（probe ④，`--release`），×2–3 次/轮 ⇒ 近 1 ms | P05 实现首日 / P18 基准 | 先落 256 条 / 8 MiB 的形状缓存 + 文档写明「>8k 字符建议拆节点」；成为热点则给 `ShapeCache` 加「按宽度的测量记忆」（`(key, width_bits) → Size`，仍无 `RefCell`：记忆在 `App` 侧，闭包只捕获不可变句柄） |
| V-40 | v1 整数 ppem + 整数物理落点：同一行内不同字号/ppem 的字形会有 ±0.5 px 的基线/字面抖动（`quantize=true` 只保证 baseline 整数，`placement.top` 是字形自身的整数上沿） | P06（像素快照）/ P18 | 视觉回归若可见抖动 → 加 subpixel x（3 套 tile 或 3× 页，图集容量 ×3）或改用 `quantize=false` + 自算舍入 |
| V-41 | 彩色 emoji：R8 字形图集只能表达单色轮廓；CBDT-only 字体会渲染为空（probe ⑤ 的 `🌍` 用 `Source::Outline` 有 213 个非零像素，说明该字体有 COLR/outline 层） | P06 / P15 | 出现空 emoji → additive 加 `AtlasKind::Color`（BGRA8 的第三页）与新 `kind` 值；P02 §10.3 的两种格式表要扩 |
| V-42 | measure 闭包持有 `Rc<ShapedText>`（含 parley `Layout`）⇒ 大段落的 `Layout` 在节点卸载/文本变化前不释放 | P05 实现 / P06 帧记录 | `remove` 清 `text_slots`（已设计）；这是**期望**行为（正在显示的文本不该因缓存逐出而重排）。若内存吃紧：给 `ShapedText` 加 `SmallVec<[Line; 1]>` 之外的瘦身不划算，改为 `Rc` 弱引用 + 每帧重建（不推荐） |
| V-43 | 性能细项：`family_list()` 每次 shape `clone` 一次 `Vec`；`swash::FontRef::from_index` 每次光栅化重解析表目录；LRU 用 `VecDeque` 提升 O(n) | P18 基准 | 都在 µs 级；有热点再改（不改公开面）：`family_list` 改借用、`FontEntry` 缓存 `offset`、LRU 改侵入式链表 |
| V-44 | 图集单页 2048²（字形 4 MiB / 图片 16 MiB）：CJK + 多 DPI 档可能不够；容量逃逸是 `clear + retry`（全部文本重光栅） | P09 / P15 | 先按 P02 §10.6 的 `clear + retry` → `AtlasExhausted`；出现真实溢出再加 `page` 字段（契约变更，需 P02/P07/P09 同步）或页扩容（`generation` 语义要扩） |
| V-45 | 文本 pass 每帧遍历 `tree.order()`（O(n)），不走脏位；`DirtyRows::spans` 在 P05 只被 `TextPassReport.dirty_spans` 计数（真正的上传读者是 P07） | P06 帧驱动 | `DirtyFlags` 还有 2 个保留位，加 `TEXT` 位即可增量遍历；在此之前 `refresh_text` 的 `report` 里 `shaped/reused` 计数是「是否真的做了活」的判据 |
| V-46 | CI 的 Linux 容器可能无 CJK/emoji 系统字体 ⇒ `tofu == 0` 的断言会假失败（D17 明确不要求跨平台一致） | P05 实现 / CI | 断言写成「`tofu == 0` **或** `diagnostics` 非空」，并单独断言诊断的字段；测试注释写明依赖宿主字体 |
| V-47 | `complex-scripts` 会把 ICU4X 的 CJK 词典数据链进二进制（体积未实测） | P18 体积预算 | 中文断行正确性优先；若 release 体积超预算，再评估「仅 CJK 目标平台开 feature」（会引入 `#[cfg]`，与「平台差异收在 platform/render」的纪律冲突，慎用） |

---

### S-59 文本两阶段（shape → 按内容宽重排）+ 审查第二轮修正

| 项 | 内容 |
|---|---|
| **B1 新增第二阶段** | `App::layout_text(window) -> Result<TextLayoutReport, EngineError>`，帧序 **styles → refresh_text → compute_layout → layout_text → paint**。`refresh_text` 只 shape + 按 max-content 量固有尺寸（喂 measure）；`layout_text` 用 taffy 解析出的**内容宽**（`Layout.size - border - padding`，在 `set_local_layout` 时存进 `Node.text_content_width`）clone layout → `break_all_lines(Some(width*scale))` → `align` → 光栅/装箱 → `ShapedLine`。选区映射改用**最终断行布局**（`TextSlot.broken`），不再用 max-content 布局 |
| **B2 ppem** | `parley::Run::font_size()` 已是**物理值**；`rasterize` 改为 `font_size().round().clamp(1,512)`（原实现又乘了一次 `scale`）。用例：1x/1.25x/2x 的物理宽度递增、逻辑宽度不变 |
| **B3 `TextStyle.family`** | `shape::family_list(stack, custom)` 把 `style.family` **前置**进 `FontFamily::List`；用例：三个 family 的 advance 至少两种不同 |
| **B4 空闲帧** | `refresh_text` 先比 `ShapeKey` + `scale_epoch` + `Rc::ptr_eq`，全等则 `skipped += 1` 并 **continue**（不重建 measure、不清缓存）。`TextPassReport::skipped` 是它的计数器 |
| **N1 行内盒入口** | `TextContent.inline_boxes: Box<[InlineBoxSpec]>`（+16 B）+ `TextContent::with_inline_boxes` + `NodeBuilder::inline_boxes`；`ShapeKey` 手工哈希盒字段（f32 走 bits）；`InlineBoxOverflow` 现在用真实内容宽触发 |
| **N2 R8 留存** | `GlyphRecord.data: Box<[u8]>` 存覆盖度（P09 上传源），`GlyphAtlas::coverage` 读它；tile 命中**不重光栅**（`stats.hits` 只加计数） |
| **N3 代际一致** | `layout_text` 记录 pass 起始 `generation`；若中途换代（容量逃逸）**整 pass 重跑一次**（`TextLayoutReport::reruns`），第二次仍变则返回 `TextShape` |
| **N4 统一跳过口径** | `App::font_stack_status() -> FontStackStatus{Ready,EmptyStack,NoMatchingFamily}`；用例不再散落 `eprintln!` |
| **N5 FontStackMiss / 空格负缓存 / 合成** | 空栈与「全部候选都不存在」都**退 generic 兜底**并记 `FontStackMiss`（`ShapeFailure::EmptyFontStack` 已删除）；`GlyphAtlas.missing` 负缓存空白字形；`Run::synthesis().any()` ⇒ `TextDiagnostic::SyntheticStyle{text_range}`（只记录，不应用合成） |
| **N6 渲染出口** | `render.rs` 暴露 `{Allocation, AtlasAllocator, AtlasSet, DirtyRows}` 四项；`AtlasSet::image()`；`AtlasAllocator::take_dirty_rows` 恢复非 test，`App::pending_dirty` 合并；`DirtyRows::merge`；`close_window` 清 `scale_epochs`；删除无读者的 `AtlasSet::generation`/`dirty_span_count` |
| **尺寸（实测）** | `TextContent` **112 B**（+`Box<[InlineBoxSpec]>` 16）、`NodeKind` **112 B**、`Node` **1112 B** ≤ 1152（余 40 B） |
| **B4 复位（复审二）** | `set_text_shaped` 的 Some 分支写新快照时同时把 `line/broken/laid_width_bits/laid_generation` 清空 ⇒ `layout_text` 的复用判据（`line.is_some() && width_bits && generation`）必然失效、强制重排；回归用例覆盖文本变短/变长、style、family、inline_boxes 四类变更 |
| **inline_boxes 保留（复审二）** | `refresh_styles` 不再用 `TextContent::new(value, style)` 重建（会丢行内盒），改为 clone 旧载荷只改 `text`/`style`；用例断言 `bind_text` 首帧与更新后 `inline_boxes.len()` 不变 |
| **门禁（复审二）** | `cargo test` **364**、release **356**、clippy/fmt/doc 0；三平台 `metadata` rc=0；`Node` 1112 断言保留 |

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
> `cargo test -p kytrix_gui` 基线 **364 passed**。
>
> 裁决：5 条（phase-06 §15，全部采纳）；新增 9 条 F（F-58…F-66）、7 条 S（S-60…S-66，
> S-66 为审查 B1 的 clip_key 守卫）；P06 自己的待复核项从 **V-48** 起（V-48…V-52）。

### F-58 静态绘制属性 = `Node.paint: Option<Box<PaintSpec>>`（**不进** `Style`）

| 项 | 内容 |
|----|------|
| 原文 | 主设计 §4.9.3 / P03 §10.1：`Style` 是「静态结构」，注记「P06 才会追加其余引擎自有字段（圆角 / 阴影 / 字体族）」 |
| 偏差 | 新增公开类型 `paint::style::PaintSpec { background, border, corner_radius, shadow, gradient }` + `ShadowSpec`，作为 **`Node` 的独立字段** `paint: Option<Box<PaintSpec>>`；`Style` 的公开结构（`inner` / `clip`）**不变** |
| 原因 | ①probe：`Style` 560 B、`PaintSpec` 136 B —— 内联 `Style.paint` 会让 `Style` 696 B、`Node` **1248 B > 1152**；②`Style` 加私有字段会让全仓 `Style { inner, clip }` 字面量（`window.rs`/`app.rs` 的测试等 6 处）无法构造，加公开字段则要求所有调用点补 `paint: None`（纯噪声）；③绘制属性与布局属性是两条独立的失效通道（`StylePatch` 已按外观分组），`set_style` 不该承担绘制语义 |
| 代价 | `Node` 1112 → **1120**（+8），叠加 F-59 的 `content_offset` 后 **1128**（余 24 B，V-48） |
| 修订后设计 | phase-06 §1.2 / §6.1 / §11.1；`NodeBuilder::paint(PaintSpec)`、`Node::paint_spec()`（`pub(crate)`） |

### F-59 `REUSABLE` / damage / `prev_layout` 的复位点在**提交**（`NodeTree::settle_frame`）

| 项 | 内容 |
|----|------|
| 原文 | 主设计 §3.4：「**一帧开始时**所有节点默认 `REUSABLE`；被失效的节点清掉该位」；P03 §9.4：「damage 清零：P06 的帧初（`NodeTree::begin_frame`）」 |
| 偏差 | 不设 `begin_frame`；改在**提交**时调 `NodeTree::settle_frame`：`damage = ∅`、`prev_layout = layout`、`dirty.insert(REUSABLE)`、`dirty.remove(LAYOUT | PAINT)` |
| 原因 | `drain`（写信号 → `mark_*` → `touch` 清 `REUSABLE` 并并 damage）发生在 `frame()` **之前**。帧初清零会：①吞掉 drain 累积的旧像素 damage（"改了颜色但旧位置没重绘"）；②把 `REUSABLE` 重新置位，让 paint 复用过期 lane（stale drawing）。复位必须发生在**消费之后** |
| 推论 | 复位点是 `FrameIndex` 递增的同一处 ⇒ "提交了才结算"；错误/空闲路径**不**结算（damage/dirty 保留，下一次重试）。`prev_layout = layout` 同时让"只改颜色"的节点不必把历史上的位置并进 damage |
| 修订后设计 | phase-06 §9.4 / §5.5；用例 §12-E1/E2 |

### F-60 `absolutize` 的 `changed` 分支必须清 `REUSABLE`

| 项 | 内容 |
|----|------|
| 原文 | P04 §7.2 / S-43：`place_subtree` 在 `changed` 时把 `old ∪ new` 并入 damage、写 `prev_layout`、`layout` |
| 偏差 | 同一分支追加 `dirty.remove(REUSABLE)` |
| 原因 | lane 内的 `Primitive` 持有**绝对** bounds（P02 §6.1 无变换栈）。布局移动后，即使没有任何 `mark_paint`，lane 内容也已过期；只并 damage 会让"移动到新位置的节点"继续用旧坐标绘制（且新位置的像素没人写） |
| 修订后设计 | phase-06 §5.6；用例 §12-F3（paint-only 帧不重排、移动帧必须重建） |

### F-61 damage 的输出是 `DamageSummary{full, rects}`；`DamageOutput` / `ScrollHint` 延后 P07 / P12

| 项 | 内容 |
|----|------|
| 原文 | P02 §12.1：`DamageOutput { Full, Partial { rects: Vec<PhysicalRect>, scroll: Option<ScrollHint> } }` + `push_scroll` |
| 偏差 | 不落 `DamageOutput` / `ScrollHint` / `push_scroll`；`DamageAccumulator::finish(frame) -> DamageSummary { full: bool, rects: usize }`，矩形留在累加器里（`rects()` 只在该 feature 下提供） |
| 原因 | ①`Partial { rects: Vec }` 是**耗尽式**交出，与 P02 §12.6「`reset` 保留容量」自相矛盾；②P06 没有 scroll 数据源（`Transient` 滚动偏移属 P12），一个永远 `None` 的字段违反 AGENTS §2.1；③P07 的平台翻译（`Present1` dirty/scroll、`VK_KHR_incremental_present`）只需要 `(full, &[PhysicalRect])`，需要拥有式结果时 additive 加 `take_rects()` |
| 修订后设计 | phase-06 §8.1 / §8.5；用例 §12-B11/B12 |

### F-62 `Geometry` 的 transform 在 P06 恒为 `Transform::IDENTITY`

| 项 | 内容 |
|----|------|
| 原文 | 主设计 §5.5 / §3.10：`app.set_geometry_transform(geo, camera_matrix)` → 只更新 uniform；`GEOMETRY` dirty |
| 偏差 | P06 的 paint 生成 `Primitive::Geometry(GeometryDraw { id, transform: IDENTITY })`；`GEOMETRY` 位**仍无写者**；`set_geometry_transform` 的 uniform 通道留 P08/P09 |
| 原因 | ①`App` 还没有 `GeometryStore`（P02 §9 明确 store 属 P09），`GeometryId` 只能由测试构造（F-65）；②transform 的归属是 **`GeometryId`**（per-id 相机矩阵），不是节点 —— 在 `Node` 上造 `geometry_transform`（+24 B）会破 Node 预算（1144+24）并与 P08 的 per-id API 冲突；③P06 的职责是"0 实例 + 1 个 `FrameOp::Geometry`"，uniform 的重写属 GPU 侧 |
| 修订后设计 | phase-06 §6.2 / §7.5；P08 落地时：写者置 `GEOMETRY` + 重写该节点的单条 `GeometryDraw`（≤1 图元的 lane 重建，GPU VBO 不动），damage = 变换前后 bounds 并集 |

### F-63 `Selector::Role` 延后 P14

| 项 | 内容 |
|----|------|
| 原文 | 主设计 §6.2：`Selector` 支持 `Id(&'static str)` / `Role(A11yRole)` / `Text(&str)` / `Path(&[...])` |
| 偏差 | P06 落 `Id` / `Text` / `Path` 三个变体，`Role` 推迟到 P14 |
| 原因 | `A11yRole` 是 P14 的类型（`a11y.rs` 不存在）；先建一个带"占位类型"的变体等于引入无消费者分支，且会迫使 P14 改 `Selector` 的布局 |
| 修订后设计 | phase-06 §10.2；P14 additive 加 `Role(A11yRole)` 变体 + 匹配臂 |

### F-64 文本绝对坐标 = `bounds.origin + content_offset + 局部`

| 项 | 内容 |
|----|------|
| 原文 | P05 §5.2 / S-54：「`positioned` 的 y 已含基线，P06 加节点 `LayoutBox.bounds.origin` 后才是绝对坐标」 |
| 偏差 | 在 `bounds.origin` 之外再加 `Node.content_offset`（= `border.top/left + padding.top/left`，`set_local_layout` 从 taffy `Layout` 写入） |
| 原因 | `LayoutBox.bounds` 是 **border box**，而 `ShapedLine` 的坐标是**文本盒（内容盒）局部**。有 padding 的文本节点（按钮/输入框/标签）直接用 `bounds.origin` 会整体左上偏移 padding 像素。`content_offset` 是 8 B 的点（不是 16 B 的 `Edges`），因为绘制只需内容盒原点 |
| 修订后设计 | phase-06 §5.2 / §6.3；`Node` +8 B（F-58 合计 1128） |

### F-65 `GeometryId::for_test` 在 `feature = "test-support"` 下是 `pub`

| 项 | 内容 |
|----|------|
| 原文 | P02 S-20：`GeometryId` 字段私有 + 公开 getter + **仅测试构造器** `#[cfg(test)] pub(crate) fn for_test` |
| 偏差 | cfg 扩为 `#[cfg(any(test, feature = "test-support"))]`，可见性改为 `pub` |
| 原因 | 集成测试（`tests/frame_record.rs`）不是 `cfg(test)`，而 P09 之前没有 `GeometryStore` 铸造句柄的公开路径；没有它，"`Geometry` 节点产生 `GeometryDraw`"这条 L2 断言无法写 |
| 修订后设计 | phase-06 §1.2 / §4.1；**P09 必须删除该构造器**（`GeometryStore` 是唯一铸造点），用例只在 test-support 下存在 |

### F-66 `FrameScheduler` 只保留 `Idle` / `Pending` / `Draining`

| 项 | 内容 |
|----|------|
| 原文 | 主设计 §3.5 的 10 态：`Idle → Pending → Drain → Invalidate → Layout → Paint → Damage → Submit → WaitGpu` |
| 偏差 | headless 版 3 态 + `FrameIndex`（唯一递增点在 `commit`）；`WaitGpu` / in-flight ring / `PlatformDispatcher::wake` 留 P09 |
| 原因 | 中间态全部发生在**一次同步调用内部**（没有平台事件循环、没有异步边界），没有任何观察者；把它们造成状态只会产生"永远观测不到"的变体。in-flight 轮转的**契约**（`index.get() % 3` 槽、retire 释放）写在 §9.1，P09 扩展变体时 additive |
| 修订后设计 | phase-06 §9.1 / §9.2；`FrameIndex` 只在 `commit` 递增（I3 的"空闲零帧不增号"由此保证） |

### S-60 paint pass 归属 `src/paint/pass.rs`（crate 私有）

| 项 | 内容 |
|----|------|
| 决策 | 新增 `paint/pass.rs`（`pub(crate) mod pass;`），`PaintPass` / `PaintReport` 放在这里；`window.rs` 只做帧编排（begin/layout/paint/commit/abort） |
| 原因 | paint pass 是「style + patch + text + canvas + geometry → `Primitive`」的单一职责；塞进 `window.rs` 会让后者混入绘制语义（AGENTS §1.3 模块边界），并把它推到 700+ 行 |
| 代价 | 超出 brief 的文件清单一项（裁决 ⑤）；文件边界门禁（§13-9）已把它列入允许集合 |

### S-61 一节点一 lane；lane 自包含祖先 clip 前缀；空 lane 粘性；释放只随卸载

| 项 | 内容 |
|----|------|
| 决策 | 每个有绘制内容的节点一条 lane（`Node.lanes[0]`，容量仍 2）；lane 内容 = 祖先 `Clip(Push)*` + 本节点图元 + 逆序 `Clip(Pop)*`；lane 分配后不因"本帧无图元"释放（只 `clear_lane`），节点卸载时经 `NodeTree::removed_lanes` 归还 |
| 原因 | ①`REUSABLE` 的语义是"整桶复用"，一节点多 lane 会让一次失效写多条、`reused_lanes` 判定复杂化；②clip 若只在本节点 lane 内 push/pop，子 lane 不在作用域内（lane 是独立桶），因此必须把祖先链展开成每 lane 的前缀（C1「lane 内部闭环」）；③粘性 lane 让 `Scene::order` 跨帧稳定，oracle 的 `node` 键可直接比较，也避免 lane 反复分配/释放 |
| 代价 | clip 前缀在 CPU 侧重复（GPU 侧由 `ClipChain` 去重，P02 §8.7）；典型 depth ≤ 1 |

### S-62 `Scene` 的写面**不收紧**；新增 `Scene::clear_lane` 同为 `pub` + engine-internal doc

| 项 | 内容 |
|----|------|
| 决策 | `new_lane` / `release_lane` / `push` / `clear_frame` / `set_order` / `clear_order` 保持 P03 的 `pub`（doc 首行 "engine-internal, 应用层不得调用"）；`clear_lane` 照此新增 |
| 原因 | P06 之后 `new_lane`/`push`/`set_order`/`release_lane` 有了 crate 内调用者，但 `clear_frame`/`clear_order` 仍**只**有 `feature = "test-support"` 的 oracle 调用者。只收紧一半会让写面分裂成两套可见性规则（比 S-33 现状更难审计）；全部改 `pub(crate)` 则 `clear_frame`/`clear_order` 会在默认构建里 `dead_code` |
| 修订后设计 | phase-06 §4.3；P03 S-33 的「pub 仅为满足 dead_code」注记被本条扩展为「写面统一 pub + engine-internal」 |

### S-63 `test-support` 的模块划分与两种记录类型

| 项 | 内容 |
|----|------|
| 决策 | `lib.rs` 加 `#[cfg(feature = "test-support")] pub mod test_support;`；`test_support.rs` 只有 5 行 `pub mod`（`app` / `input` / `probe` / `record` / `snapshot`）；`FrameRecord`（调度记录）与 `RenderedFrame`（绘制快照）**分型**，`FrameRecord.frame: Option<RenderedFrame>` |
| 原因 | ①`pub use` 被 workspace lint 禁止（M-02），无法把 5 个模块压平成一个命名空间，模块划分必须显式；②调度事实（`rendered` / `damage` / `stats` / `paint_log`）与绘制事实（`quads` / `primitives` / `debug_bounds`）的生命周期不同：空闲帧只有前者，提交帧两者都有，混在一个类型里会让"本帧未提交"只能靠空 Vec 猜 |
| 公开路径 | `kytrix_gui::test_support::{app::TestApp, app::TestWindow, probe::{Probe, Selector}, record::…, input::…, snapshot::…}`（S-16 闭包表见 phase-06 §4.2） |

### S-64 全帧 damage 的触发点 = `set_size` / `set_scale` 的 `full_damage_pending`

| 项 | 内容 |
|----|------|
| 决策 | `Window` 新增 `full_damage_pending: bool`；`set_size` / `set_scale` 置位，`paint_frame` 在 `damage.reset` 之后消费它并 `push_full()` |
| 原因 | 尺寸/DPI 变化会重建 framebuffer（P09 的 swapchain），局部 rect 的坐标系整体失效（P01 §7.2 / 主设计 §5.7 的"窗口尺寸/DPI → 全帧"）。用显式 flag 而不是"比较 `scale_epoch`/`size` 与上一帧"，是因为后者在**多帧未提交**（错误中止）时会把"已经全帧过一次"误判成"还需要全帧" —— 语义上两者都不能漏，但 flag 只需要一次 |
| 修订后设计 | phase-06 §9.5 / §8.5；用例 §12-F4 |

### S-65 编码器自检承担 `InstanceBuffer` 内部字段的非 test 读点

| 项 | 内容 |
|----|------|
| 决策 | `InstanceEncoder::encode` 末尾做三项 `debug_assert!`：`Σ FrameOp::Instances.len == instances.len()`、每个 `FrameOp::Geometry(i)` 的 `i < geometry.len()`、每个 `ClipChain.parent` 的下标 < 自身下标 |
| 原因 | `InstanceBuffer` 是 crate 私有（P02 §13.2），P06 没有 GPU 上传路径，`instances/clips/geometry/ops` 如果只写不读会触发 `dead_code`。probe（§2.2）实测 `debug_assert!` 内的字段读取**计入**使用（`cfg!(debug_assertions)` 是运行期常量，HIR 里两个分支都在）。这三条同时也是真实不变量：`FrameOp` 必须恰好覆盖全部实例与几何绘制，clip 链必须无环 |
| 代价 | 每次 `encode` 一次 O(#ops + #clips) 的断言（debug 构建；release 下 `cfg!` 为 false，代价是一次可预测分支） |

### S-66 `clip_key` 侧表：lane 复用的 clip 上下文守卫（审查 B1）

| 项 | 内容 |
|----|------|
| 决策 | `NodeTree` 新增侧表 `clip_keys: Vec<Option<(u8, Option<PhysicalRect>)>>`；paint 对每个节点在 `REUSABLE` 短路**之前**比较 `(depth, covering)` 与当前递归携带的 clip 上下文，不等即重建并写回。`set_style` 不再需要"clip 变化 → 子树失效"的特殊逻辑 |
| 原因 | `Style.clip` 只表达"裁不裁"，lane 前缀是**具体矩形**。窗口 resize / 分栏拖拽 / P13 的布局写入会改变 `Clip::Bounds` 容器的 padding box，而子节点的绝对盒可能逐位不变（`absolutize` 的 `changed` 为假）⇒ `REUSABLE` 保持置位；只比较开关会复用旧矩形，把子内容裁到错误区域（stale drawing）。侧表与 `layout_cache`/`text_slots` 同一生命周期纪律（槽位复用/卸载置 `None`） |
| 代价 | 每节点 1 槽（`Option<(u8, Option<PhysicalRect>)>` ≈ 20 B，只对曾重建的节点非空）；每次 paint 每节点一次比较（O(1)） |
| 验证 | 用例 `clip_key_mismatch_after_resize_or_reparent`（phase-06 §12-E5）：resize 改变滚动容器尺寸、重挂子树到 clip 节点下两种情况都必须重建且前缀矩形更新 |

### 十一·待复核项（P06 新增）

| 编号 | 内容 | 复核 Phase | 触发条件 / 处置 |
|---|---|---|---|
| V-48 | `Node` **1128 B**（余 24 B）：`paint`(8) + `content_offset`(8) 之后，任何再加内联字段的 Phase 都会顶破 1152 | P07 / P08 | 触发即改 `style: Box<Style>`（公开面不变，P03 V-30 的备选）或把字段移到 `NodeTree` 侧表（`debug_ids`/`removed_lanes` 是现成范例） |
| V-49 | 每帧 O(n)：paint 递归全树 + 文本两 pass（P05 V-45）+ 每图元 `resolve`（P02 R7） | P13 / P18 | 触发（虚拟列表 / 基准热点）→ `DirtyFlags` 的 2 个保留位加 `TEXT` / `PAINT` 增量遍历；lane 内缓存 `LinearPremulRgba`（不改 128 B 契约） |
| V-50 | `Canvas` 在每次 lane 重建时 `clone`（48 B + `Vec<Primitive>` 深拷贝） | P15 | 出现 > 1k 图元的画布 → `NodeKind::Canvas(Rc<Canvas>)`（additive，公开面只多一个构造器） |
| V-51 | `opacity` 是**逐图元 alpha**（非 group opacity）；`Canvas` 内部图元不参与 | P15 | 需要整组淡出（模态遮罩/交叉淡入）→ 离屏合成或 `Canvas` 层 alpha，属渲染器能力 |
| V-52 | damage 的**节点级** push 对隐藏/无图元节点也生效（过绘，安全侧） | P18 | I4 的 damage 比不达标 → 只在 `rebuilt && lane 非空` 时 push；前提是先证明"图元减少"仍被节点级 rect 覆盖 |

**审查二轮新增**（PASS-WITH-FIXES：B1 空 clip、N1/N2/N3）：

| 编号 | 裁决 / 偏差 | 理由 |
|---|---|---|
| F-67 | `Clip::Bounds` 的 padding box 为 0×0 时**仍入栈**（depth+1、covering 交空、子 lane 带空前缀） | 与 `hit_test` 的 clip 链同构；跳过会让子内容"越界可见"（审查 B1，阻塞项） |
| S-67 | **无图元的节点不留 lane**（重建后 `release_lane` + 清 `Node.lanes`）；复用判据去掉 `lane_of().is_some()`，改靠 `REUSABLE` + `clip_key` | 空 lane 只占 `Scene::order`/内存；空窗口首帧必须能判空闲（审查 N3） |
| V-53 | `FrameOp::Geometry.clip_rect` 用 `[u32; 4]`（f32 位模式）而非 `[f32; 4]` | `FrameOp` 需要 `Eq`（测试按 `matches!` 断言 + 去重），f32 无 `Eq`；P07 侧 `from_bits` 还原（成本可忽略） |

### 实施记录（2026-11-12，P06 落地）

| 项 | 实测 |
|---|---|
| 新增文件 | `src/paint/{instance,pass}.rs`、`src/render/damage.rs`、`src/test_support.rs` + `src/test_support/{app,input,probe,record,snapshot}.rs`、`tests/frame_record.rs` |
| 修改文件 | `Cargo.toml`（`[features] test-support` + `[[test]] frame_record`）、`lib.rs`、`paint.rs`、`render.rs`、`paint/{style,color,geometry,scene}.rs`、`node/{tree,builder}.rs`、`window.rs`、`app.rs`、`math.rs` |
| 测试 | 审查二轮修复后：`cargo test -p kytrix_gui` **418**；`--features test-support` **427 + 集成 5**；`--release` **409**；`--release --features test-support` **418 + 集成 5**（较 P05 基线 364 新增：默认 54 / feature 63 / 集成 5） |
| 审查二轮 | B1（空 clip 入栈）+ N1/N2/N3 全部落地，逐条见上方「审查二轮新增」表与 phase-06 §17；回归用例 `zero_size_clip_still_pushes_and_empties_damage`、`transparent_gradient_emits_nothing`、`resize_forces_full_damage`、`text_glyphs_use_border_and_padding_offset`、`image_maps_to_atlas_quad` |
| 门禁 | clippy（默认与 feature 的 `--all-targets -D warnings`）0；`fmt --check` 0；`doc --no-deps`（两配置）0；三平台 `cargo metadata` rc=0；`Cargo.lock` **零变化**；零 `unsafe`/`#[allow]`/`#[expect]`/`serde_json`/`#[path]`/`mod.rs`/`pub use`（`render.rs` 的 `pub(crate) use` 属 M-07 白名单） |
| `Node` 尺寸 | 实测 **1128 B**（1112 + `paint` 8 + `content_offset` 8）≤ 1152（余 24 B） |
| 实现期修正（相对 phase-06 计划） | ①`FrameScheduler::{state, skipped}` 与 `Window::frame_state` **不落**（无默认构建消费者；测试用 `FrameOutcome`/`frame_index` 断言，见 phase-06 §9.5）；②**空 `Canvas` 节点也 push `Primitive::Canvas`**（`bounds` 参与 damage，P02 §11.3），编码器展开后 0 实例；③`rebuilt` 只统计**产生像素或有 damage** 的节点（空 lane 不触发提交，否则空窗口首帧无法判空闲）；④`FrameStats` 去掉 `geometry_draws`（无消费者）；⑤`paint/pass.rs` 的 `run` 末尾补 `scene.set_order`（计划文字里隐含、实现必须显式）；⑥`FrameOp::Geometry.clip_rect` 用 f32 位模式（V-53） |
| 遗留（已知、已登记） | ①集成文本用例依赖宿主字体（`tofu == 0` 断言，V-46）；②oracle 只比较 `primitives`/`quads` 序列（damage 必然不同，设计 §3.8 的范围）；③damage 的节点级 push 偏保守（V-52）；④`InstanceBuffer` 的 `as_bytes`/`instances()`/`clips()`/`geometry()` 等 P07 上传访问器**尚未提供**（无消费者）；⑤`Geometry` 的 transform 恒 `IDENTITY`（F-62） |
